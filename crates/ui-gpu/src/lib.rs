//! GUI frontend: window, renderer, damage-only presents.
//!
//! Renderer path B from `docs/decisions/0001-renderer.md`: winit for the
//! window and input, softbuffer as the CPU renderer. Drawing is
//! event-driven (no frame loop): a present happens only after input, a
//! worker result or a window event, and covers only the damaged rects.
//! Scrolling moves existing pixels and draws only the newly exposed rows.
//! The wgpu takeover path (DX12 only on Windows) is M1 work (KW-11).

mod about;
mod chrome;
mod clipboard;
mod close_dialog;
mod context_menu;
mod desktop;
mod desktop_window;
mod file_dialog;
mod file_prompt;
mod window_placement;
// Until ROOT adopts the file-type registration backend; then `mod filetypes_ui;`.
mod filetypes_ui;
mod font;
#[cfg(all(windows, feature = "gpu"))]
mod gpu_surface;
mod hex_host;
mod hex_ui;
mod hit;
mod icons;
mod ime;
mod input;
mod language_ui;
mod log_ui;
mod markdown_diagram;
mod markdown_images;
mod markdown_ui;
mod menu;
mod palette_ui;
mod palette_view;
mod prompt_ui;
mod registration_host;
mod scrollbar;
mod search_ui;
mod search_view;
mod settings_ui;
mod tabs;
mod theme_ui;
mod welcome;
mod welcome_ui;
mod wheel;
mod zoom;

use font::Font;
use kwil_view_model::{CommandArgs, CommandStatus, Damage, Notify, View, Workspace};
#[cfg(all(feature = "gpu", not(windows)))]
mod gpu;
#[cfg(all(feature = "gpu", windows))]
#[path = "gpu_windows.rs"]
mod gpu;
#[cfg(feature = "frame-harness")]
mod harness;
mod platform;
pub mod remote;
mod shaping;
mod shutdown;
pub mod text;
mod trace;
pub use kwil_core_commands::settings::RenderBackend;
pub use remote::RenderPolicy;

use kwil_core_commands::settings::Settings;

use softbuffer::{Context, Surface};
use std::num::NonZeroU32;
use std::sync::Arc as Rc;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, Ime, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

fn bg() -> u32 {
    theme_ui::rgb(theme_ui::chrome(), kwil_core_commands::theme::UiRole::Background)
}
fn fg() -> u32 {
    theme_ui::rgb(theme_ui::chrome(), kwil_core_commands::theme::UiRole::Foreground)
}
fn gutter_bg() -> u32 {
    theme_ui::rgb(theme_ui::chrome(), kwil_core_commands::theme::UiRole::GutterBackground)
}
fn gutter_fg() -> u32 {
    theme_ui::rgb(theme_ui::chrome(), kwil_core_commands::theme::UiRole::GutterForeground)
}
#[cfg(test)]
fn status_bg() -> u32 {
    theme_ui::rgb(theme_ui::chrome(), kwil_core_commands::theme::UiRole::StatusBackground)
}
fn status_fg() -> u32 {
    theme_ui::rgb(theme_ui::chrome(), kwil_core_commands::theme::UiRole::StatusForeground)
}
const GUTTER_COLS: usize = 9;
/// Minimum clip reserved by the row painters; the DPI-sized scrollbar
/// (`chrome::Metrics::bar`) is always painted over the body edge afterwards.
const BAR: usize = 14;

#[derive(Debug, Default, Clone, Copy)]
pub struct GuiOptions {
    /// Agent and test runs: the window is DWM-cloaked (composed but never
    /// shown), unfocused and has no taskbar entry (Windows only).
    pub hidden: bool,
    /// An explicit CLI choice wins over the settings file for this invocation.
    pub renderer: Option<kwil_core_commands::settings::RenderBackend>,
}

#[derive(Debug)]
pub enum GuiError {
    /// No display or the windowing system refused; the caller may fall back
    /// to the terminal UI.
    Unavailable(String),
    NoFont,
    /// The window closed, but accepted settings or a worker could not drain.
    /// This is not a display failure and must not launch another frontend.
    Settings(String),
}

impl std::fmt::Display for GuiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GuiError::Unavailable(e) => write!(f, "no window available: {e}"),
            GuiError::NoFont => write!(f, "no monospace font found"),
            GuiError::Settings(error) => write!(f, "settings save failed during exit: {error}"),
        }
    }
}

enum UserEvent {
    FileDialog(file_dialog::Event),
    CloseDialog(close_dialog::Reply),
    HighlightsReady,
    SearchReady,
    WindowPlacementLoaded(Result<Option<window_placement::Placement>, String>),
    WindowPlacementProgress,
    #[cfg(windows)]
    WindowPlacementTouched,
    PaletteRoot(Result<std::path::PathBuf, String>),
    PaletteReady,
    SettingsPath(Result<std::path::PathBuf, String>),
    SettingsPanelReady,
    WelcomeReady,
    RegistrationReady,
    RegistrationStarted(Result<Box<kwil_view_model::file_type_registration::FileTypeRegistration>, String>),
    /// Boxed on the worker that completes it: keeps every queued event small.
    Document(Box<kwil_view_model::document_worker::DocumentResult>),
    OpenExternal,
    Notice(String),
    GlyphReady(shaping::Tile, u64),
    SettingsReady(Box<Settings>, kwil_view_model::language_selection::AcceptedLanguageMappings),
    DiagnosticsReady(String),
    LogReady,
    MarkdownReady(Box<markdown_ui::Completion>),
    FontReady(Option<Box<Font>>),
    UiFontReady(Option<Box<font::UiFont>>),
    /// Wide-coverage face read for the chrome face with this id.
    UiFallbackReady(u64, Option<(Vec<u8>, usize)>),
    SessionChanged(bool),
    DroppedFile(std::path::PathBuf),
    #[cfg(feature = "gpu")]
    GpuReady(Result<Box<gpu::Gpu>, String>, f64),
    #[cfg(feature = "gpu")]
    GpuAvailable(Result<(), String>),
    #[cfg(feature = "frame-harness")]
    Harness(harness::Step),
}

/// The same host can receive bounded worker wake events in dispatch tests
/// without constructing a native event loop or window.
#[derive(Clone)]
enum HostProxy {
    Native(EventLoopProxy<UserEvent>),
    #[cfg(test)]
    Test(std::sync::mpsc::Sender<UserEvent>),
}
impl HostProxy {
    fn send_event(&self, event: UserEvent) -> Result<(), ()> {
        match self {
            Self::Native(proxy) => proxy.send_event(event).map_err(|_| ()),
            #[cfg(test)]
            Self::Test(sender) => sender.send(event).map_err(|_| ()),
        }
    }
    #[cfg(feature = "frame-harness")]
    fn native(&self) -> Option<EventLoopProxy<UserEvent>> {
        match self {
            Self::Native(proxy) => Some(proxy.clone()),
            #[cfg(test)]
            Self::Test(_) => None,
        }
    }
}

/// Events for the app integration package. A dropped file is a request, never
/// an implicit replacement of an edited buffer.
#[derive(Clone, Debug)]
pub enum GuiEvent {
    DroppedFile(std::path::PathBuf),
    RenderPolicyChanged(RenderPolicy),
}

struct Gfx {
    #[cfg(all(windows, feature = "gpu"))]
    gpu_canvas: Option<gpu_surface::Canvas>,
    native: Option<platform::Native>,
    window: Rc<Window>,
    surface: Surface<Rc<Window>, Rc<Window>>,
    #[cfg_attr(not(windows), allow(dead_code))]
    hwnd: isize,
    pixels: Vec<u32>,
}

#[derive(Debug, PartialEq, Eq)]
enum UnboundKey {
    Quit,
    Edit(input::Action),
    Ignore,
}

fn command_notice(status: CommandStatus) -> Option<String> {
    match status {
        CommandStatus::Message(message) | CommandStatus::NotImplemented(message) => Some(message),
        CommandStatus::IndexNotReady => Some("Indexing: editing will be available when ready".into()),
        CommandStatus::Applied | CommandStatus::SaveRequested => None,
    }
}

#[cfg(test)]
mod input_dispatch_tests {
    use super::*;
    use kwil_core_commands::{Binding, Preset, keymap::KeymapEngine, vim::VimResult};

    #[test]
    fn cancelling_save_as_cancels_its_close_intent() {
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        workspace.file_command("file.saveAndQuit").unwrap();
        workspace.file_prompt_mut().unwrap().text = "unfinished.txt".into();
        file_prompt::cancel(&mut workspace);
        assert!(workspace.file_prompt().is_none());
        assert!(!workspace.close_pending(), "a later ordinary save must not close the window");
    }

    #[test]
    fn blank_save_submission_keeps_the_input_destination_and_cancel_intent() {
        for value in ["", "   ", "\t"] {
            let mut workspace = Workspace::new(View::new(None)).unwrap();
            workspace
                .execute("edit.insert", &CommandArgs { text: Some("keep".into()), ..Default::default() }, 1)
                .unwrap();
            workspace.request_window_close(false).unwrap();
            workspace.file_command("file.save").unwrap();
            workspace.file_prompt_mut().unwrap().text = value.into();
            file_prompt::submit(&mut workspace);
            assert_eq!(workspace.file_prompt().unwrap().text, value);
            assert!(workspace.close_pending());
            let mut prompt = Some(workspace.file_prompt().unwrap().text.clone());
            assert!(input::commit("\u{65e5}\u{672c}\u{8a9e}.txt".into(), &mut prompt, false).is_none());
            workspace.file_prompt_mut().unwrap().text = prompt.unwrap();
            assert!(workspace.file_prompt().unwrap().text.ends_with("\u{65e5}\u{672c}\u{8a9e}.txt"));
            assert_eq!(workspace.editor().unwrap().document().snapshot().len(), 4);
            file_prompt::cancel(&mut workspace);
            assert!(!workspace.close_pending());
        }
    }

    #[test]
    fn vim_and_bound_command_notices_remain_visible() {
        for status in [
            CommandStatus::Message("register is empty".into()),
            CommandStatus::NotImplemented("command not implemented: view.settings".into()),
            CommandStatus::IndexNotReady,
        ] {
            let mut view = View::new(None);
            let notice = command_notice(status).expect("non-applied command must explain its result");
            let damage = view.set_notice(notice.clone());
            assert!(damage.status);
            assert!(view.status_text().contains(&notice));
        }
        assert!(command_notice(CommandStatus::Applied).is_none());
    }

    #[test]
    fn gui_tombstones_suppress_selection_quit_and_chord_fallbacks() {
        let bindings = ["shift+left", "ctrl+q", "ctrl+k ctrl+q"].map(|keys| Binding {
            keys: keys.into(),
            command: String::new(),
            context: None,
            mode: None,
        });
        let mut keys = KeymapEngine::new(Preset::VsCode, &bindings).unwrap();
        for (key, mods) in [
            (Key::Named(NamedKey::ArrowLeft), ModifiersState::SHIFT),
            (Key::Character("q".into()), ModifiersState::CONTROL),
        ] {
            assert_eq!(keys.feed_with_selection(input::chord(&key, mods).unwrap()), VimResult::Unbound);
            assert_eq!(App::unbound_key(&keys, &key, None, mods), UnboundKey::Ignore);
        }
        assert_eq!(keys.feed_with_selection("ctrl+k".parse().unwrap()), VimResult::Pending);
        assert_eq!(keys.feed_with_selection("ctrl+q".parse().unwrap()), VimResult::Unbound);
        assert_eq!(
            App::unbound_key(&keys, &Key::Character("q".into()), None, ModifiersState::CONTROL),
            UnboundKey::Ignore
        );
        assert_eq!(keys.feed_with_selection("\u{754c}".parse().unwrap()), VimResult::Unbound);
        assert_eq!(
            App::unbound_key(&keys, &Key::Character("\u{754c}".into()), Some("\u{754c}"), ModifiersState::empty()),
            UnboundKey::Edit(input::Action::Text("\u{754c}".into()))
        );
        assert_eq!(keys.feed_with_selection("cmd+s".parse().unwrap()), VimResult::Unbound);
        assert_eq!(
            App::unbound_key(&keys, &Key::Character("s".into()), Some("s"), ModifiersState::SUPER),
            UnboundKey::Ignore
        );
        let mut keys = KeymapEngine::new(Preset::NotepadPlusPlus, &[]).unwrap();
        assert!(
            matches!(keys.feed_with_selection("ctrl+shift+up".parse().unwrap()), VimResult::Command(call) if call.id == "edit.moveLineUp")
        );
        let mut keys = KeymapEngine::new(Preset::VsCode, &[]).unwrap();
        assert_eq!(keys.feed_with_selection("ctrl+q".parse().unwrap()), VimResult::Unbound);
        assert_eq!(
            App::unbound_key(&keys, &Key::Character("q".into()), None, ModifiersState::CONTROL),
            UnboundKey::Quit
        );
    }
}

/// Logical key seam used by the real window host and its dispatch tests.
/// Settings capture stays ahead of this seam in `key`.
struct HostKey<'a> {
    logical_key: Key,
    text: Option<&'a str>,
    repeat: bool,
}

struct App {
    hex: hex_host::Host,
    about: about::About,
    markdown: markdown_ui::Host,
    context: context_menu::Context,
    context_area: Option<Rect>,
    logs: log_ui::Host,
    logs_ime: Option<log_ui::Composition>,
    search: Option<kwil_view_model::search_session::SearchSession>,
    search_panel: kwil_view_model::search_panel::SearchPanel,
    search_origin: Option<search_ui::Origin>,
    search_ime: Option<search_ui::Composition>,
    search_dirty: bool,
    search_area: Option<Rect>,
    search_view: search_view::View,
    goto: Option<palette_ui::GoToPrompt>,
    goto_epoch: u64,
    goto_reveal: Option<(kwil_view_model::SessionDocumentId, u64)>,
    highlighting: Option<kwil_view_model::highlight_session::HighlightSession>,
    language_picker: language_ui::LanguagePickerUi,
    document_stopped: Option<std::sync::mpsc::Receiver<Result<(), String>>>,
    palette: Option<kwil_view_model::palette_session::PaletteSession>,
    palette_dirty: bool,
    palette_area: Option<Rect>,
    settings_panel: Option<kwil_view_model::settings_session::SettingsSession>,
    settings_epoch: u64,
    settings_file_pending: bool,
    settings_view: settings_ui::View,
    palette_view: palette_view::View,
    prompt_view: prompt_ui::View,
    welcome: welcome::Model,
    welcome_view: welcome_ui::WelcomeView,
    welcome_worker: Option<welcome::Worker>,
    file_types: Option<filetypes_ui::Page>,
    registration_path: Option<std::path::PathBuf>,
    registration_starting: bool,
    registration_prep: Option<std::thread::JoinHandle<()>>,
    view: Workspace,
    frontend_document: kwil_view_model::SessionDocumentId,
    frontend_epoch: u64,
    opts: GuiOptions,
    proxy: HostProxy,
    on_first_frame: Option<Box<dyn FnOnce(Notify)>>,
    on_external_ready: Option<Box<dyn FnOnce(ExternalOpen)>>,
    external_open: Option<ExternalOpen>,
    external_inbox: kwil_view_model::document_worker::ExternalOpenInbox,
    font_thread: Option<std::thread::JoinHandle<Option<Font>>>,
    font_size: Option<std::sync::mpsc::SyncSender<f32>>,
    font: Option<Font>,
    gfx: Option<Gfx>,
    desktop_window: Option<desktop_window::Dialog>,
    desktop_dispatch: bool,
    pending_resize: Option<(usize, usize)>,
    window_placement: Option<window_placement::Service>,
    window_normal: Option<window_placement::Placement>,
    window_latest: Option<window_placement::Placement>,
    window_placement_loaded: bool,
    window_touched: bool,
    size: (usize, usize),
    mods: ModifiersState,
    pending: Damage,
    error: Option<GuiError>,
    horizontal: u64,
    wheel: wheel::Wheel,
    pointer: (usize, usize),
    drag: Option<scrollbar::Drag>,
    started: std::time::Instant,
    composing: bool,
    ime_destination: Option<ime::Destination>,
    selecting: bool,
    close_pending: bool,
    save_path: Option<String>,
    file_dialogs: file_dialog::Dialogs,
    close_dialog: Option<close_dialog::Ticket>,
    keys: Option<kwil_core_commands::keymap::KeymapEngine>,
    settings: Settings,
    zoom: zoom::Zoom,
    policy: RenderPolicy,
    on_event: Box<dyn FnMut(GuiEvent)>,
    #[cfg(feature = "gpu")]
    gpu: Option<gpu::Gpu>,
    #[cfg(feature = "gpu")]
    gpu_started: bool,
    presenter_gpu: bool,
    font_loading: bool,
    font_reload_pending: bool,
    redraw_requested: bool,
    settings_loaded: bool,
    body_pixels: Vec<u32>,
    chrome_frame: Option<tabs::Frame>,
    chrome_dirty: bool,
    /// Chrome sizes for the window's monitor; independent of editor zoom.
    ui: chrome::Metrics,
    ui_font: Option<font::UiFont>,
    ui_font_thread: Option<std::thread::JoinHandle<Option<font::UiFont>>>,
    ui_font_size: Option<std::sync::mpsc::SyncSender<f32>>,
    /// Bounded DPI reloads of the chrome face.
    ui_font_jobs: font::Coalesce,
    /// At most one fallback-face read per App, fenced to the asking face.
    ui_fallback: font::FallbackJobs,
    menu: menu::MenuBar,
    /// Last drawn menu list, restored from the body when the list changes.
    menu_area: Option<Rect>,
    menu_dirty: bool,
    hover: chrome::Hover,
    /// Clickable status fields as drawn: area, segment index, command.
    status_hits: Vec<(Rect, usize, &'static str)>,
    window_title: String,
    #[cfg(feature = "frame-harness")]
    metrics: harness::Metrics,
}

/// Runs the GUI until the window closes. `on_first_frame` runs once the
/// first frame is on screen; it receives a `Notify` that shows a message in
/// the status bar from any thread (used for update notices).
pub fn run(view: View, opts: GuiOptions, on_first_frame: Box<dyn FnOnce(Notify)>) -> Result<(), GuiError> {
    run_with_events(view, opts, on_first_frame, Box::new(|_| {}))
}

pub fn run_with_events(
    view: View,
    opts: GuiOptions,
    on_first_frame: Box<dyn FnOnce(Notify)>,
    on_event: Box<dyn FnMut(GuiEvent)>,
) -> Result<(), GuiError> {
    run_with_external_open(view, opts, on_first_frame, on_event, Box::new(|_| {}))
}

pub use kwil_view_model::document_worker::ExternalOpen;

/// The launcher can wire CLI/IPC requests without owning a frontend event loop.
pub fn run_with_external_open(
    view: View,
    opts: GuiOptions,
    on_first_frame: Box<dyn FnOnce(Notify)>,
    on_event: Box<dyn FnMut(GuiEvent)>,
    on_external_ready: Box<dyn FnOnce(ExternalOpen)>,
) -> Result<(), GuiError> {
    trace::mark("gui entry");
    // Font loading runs while the event loop and window are created.
    let (font_size, font_size_rx) = std::sync::mpsc::sync_channel(1);
    let font_thread = Some(std::thread::spawn(move || Font::startup(font_size_rx)));
    let event_loop =
        EventLoop::<UserEvent>::with_user_event().build().map_err(|e| GuiError::Unavailable(e.to_string()))?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = HostProxy::Native(event_loop.create_proxy());
    let open_proxy = proxy.clone();
    // The chrome face is read in parallel and rasterized at the monitor scale.
    let (ui_font_size, ui_font_size_rx) = std::sync::mpsc::sync_channel(1);
    let ui_font_thread = Some(std::thread::spawn(move || font::UiFont::startup(ui_font_size_rx)));
    let (external_open, external_inbox) = kwil_view_model::document_worker::external_open_channel(move || {
        open_proxy
            .send_event(UserEvent::OpenExternal)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "GUI closed"))
    });
    let view = Workspace::new(view).map_err(|e| GuiError::Unavailable(e.to_string()))?;
    let frontend_document = view.active_id();
    let frontend_epoch = view.input_epoch();
    let horizontal = view.horizontal_origin();
    let window_placement = match window_placement::Service::start(opts.hidden, proxy.clone()) {
        Ok(worker) => Some(worker),
        Err(error) => {
            let _ = proxy.send_event(UserEvent::Notice(format!("Window placement worker unavailable: {error}")));
            None
        }
    };
    let mut app = App {
        hex: Default::default(),
        search: None,
        search_panel: Default::default(),
        search_origin: None,
        search_ime: None,
        search_dirty: false,
        search_area: None,
        search_view: Default::default(),
        goto: None,
        goto_epoch: 0,
        goto_reveal: None,
        highlighting: None,
        language_picker: Default::default(),
        document_stopped: None,
        palette: None,
        palette_dirty: false,
        palette_area: None,
        settings_panel: None,
        settings_epoch: 0,
        settings_file_pending: false,
        settings_view: Default::default(),
        palette_view: Default::default(),
        prompt_view: Default::default(),
        welcome: Default::default(),
        welcome_view: Default::default(),
        welcome_worker: None,
        file_types: None,
        registration_path: None,
        registration_starting: false,
        registration_prep: None,
        view,
        frontend_document,
        frontend_epoch,
        opts,
        proxy,
        on_first_frame: Some(on_first_frame),
        on_external_ready: Some(on_external_ready),
        external_open: Some(external_open),
        about: Default::default(),
        markdown: markdown_ui::Host::default(),
        context: Default::default(),
        context_area: None,
        logs: Default::default(),
        logs_ime: None,
        external_inbox,
        font_thread,
        font_size: Some(font_size),
        font: None,
        gfx: None,
        desktop_window: None,
        desktop_dispatch: false,
        pending_resize: None,
        window_placement,
        window_normal: None,
        window_latest: None,
        window_placement_loaded: false,
        window_touched: false,
        size: (0, 0),
        mods: ModifiersState::empty(),
        pending: Damage::default(),
        error: None,
        horizontal,
        wheel: Default::default(),
        pointer: (0, 0),
        drag: None,
        started: std::time::Instant::now(),
        composing: false,
        ime_destination: None,
        selecting: false,
        close_pending: false,
        save_path: None,
        file_dialogs: file_dialog::Dialogs::default(),
        close_dialog: None,
        keys: None,
        settings: Settings::default(),
        zoom: zoom::Zoom::default(),
        policy: RenderPolicy::new(Default::default(), remote::detect()),
        on_event,
        #[cfg(feature = "gpu")]
        gpu: None,
        #[cfg(feature = "gpu")]
        gpu_started: false,
        presenter_gpu: false,
        font_loading: false,
        font_reload_pending: false,
        redraw_requested: false,
        settings_loaded: false,
        body_pixels: Vec::new(),
        chrome_frame: None,
        chrome_dirty: true,
        ui: chrome::Metrics::default(),
        ui_font: None,
        ui_font_thread,
        ui_font_size: Some(ui_font_size),
        ui_font_jobs: font::Coalesce::default(),
        ui_fallback: font::FallbackJobs::default(),
        menu: menu::MenuBar::default(),
        menu_area: None,
        menu_dirty: false,
        hover: chrome::Hover::None,
        status_hits: Vec::new(),
        window_title: String::new(),
        #[cfg(feature = "frame-harness")]
        metrics: Default::default(),
    };
    app.logs.admit_projection_disk(1 << 30); // Same aggregate disk admission as SearchWorker::new.
    let result = event_loop.run_app(&mut app);
    app.desktop_window.take();
    // Transfer every owner before awaiting any result. The GUI loop has ended;
    // these jobs await existing workers, which keep all I/O off the frontend.
    let prep_job = app.registration_prep.take().map(|handle| {
        Box::new(move || handle.join().map_err(|_| "registration preparation panicked".to_owned())) as shutdown::Job
    });
    let welcome_job = app.welcome_worker.take().map(|worker| {
        let handle = worker.stop();
        Box::new(move || {
            let report = handle.join();
            let mut errors = Vec::new();
            if report.worker_result.is_err() {
                errors.push("setup worker panicked".to_owned());
            }
            for report in report.reports {
                if let welcome::Report::Completed(completion) = report {
                    if let Err(error) = completion.result {
                        errors.push(format!("Setup completion: {error}"));
                    }
                    if let Some(path) = completion.retained_temporary {
                        errors.push(format!("Setup cleanup retained: {}", path.display()));
                    }
                }
            }
            if errors.is_empty() { Ok(()) } else { Err(errors.join("; ")) }
        }) as shutdown::Job
    });
    let registration_job = app.file_types.take().map(|page| {
        let handle = page.service.stop();
        Box::new(move || handle.shutdown()) as shutdown::Job
    });
    let settings_job = app.settings_panel.as_mut().map(|panel| {
        let handle = panel.stop();
        Box::new(move || handle.shutdown()) as shutdown::Job
    });
    let search_job = app.search.take().map(|mut session| {
        let mut panel = std::mem::take(&mut app.search_panel);
        Box::new(move || {
            std::thread::spawn(move || match panel.take_shutdown(&mut session) {
                Ok(worker) => {
                    drop(panel);
                    drop(session);
                    worker.shutdown().map_err(|_| "search worker panicked".to_owned())
                }
                Err(owner) => {
                    drop(owner);
                    Ok(())
                }
            })
            .join()
            .map_err(|_| "search shutdown lane panicked".to_owned())?
        }) as shutdown::Job
    });
    #[cfg(all(feature = "gpu", windows))]
    let gpu_job = app.gpu.take().map(|gpu| Box::new(move || gpu.shutdown()) as shutdown::Job);
    #[cfg(all(feature = "gpu", not(windows)))]
    let gpu_job = app.gpu.take().map(|gpu| Box::new(move || gpu.shutdown().wait()) as shutdown::Job);
    #[cfg(not(feature = "gpu"))]
    let gpu_job = None;
    let diagnostics_job = app.about.shutdown();
    let markdown_job = app.markdown.shutdown();
    let logs_job = app.logs.shutdown();
    let placement_latest = app.window_latest;
    let placement_job = app.window_placement.take().map(|service| {
        Box::new(move || {
            std::thread::spawn(move || service.stop(placement_latest))
                .join()
                .map_err(|_| "Window placement shutdown panicked".to_owned())?
        }) as shutdown::Job
    });
    let syntax_job = app.highlighting.take().map(|host| {
        Box::new(move || {
            std::thread::spawn(move || host.shutdown())
                .join()
                .map_err(|_| "syntax shutdown lane panicked".to_owned())?
        }) as shutdown::Job
    });
    let document_job = app.document_stopped.take().map(|stopped| {
        // All snapshot-producing workers settle first. Drop actual documents
        // on this final lane, then require the existing document-worker receipt.
        let empty = Workspace::new(View::new(None)).expect("empty shutdown workspace");
        let workspace = std::mem::replace(&mut app.view, empty);
        Box::new(move || {
            std::thread::spawn(move || {
                drop(workspace);
                stopped
                    .recv_timeout(std::time::Duration::from_secs(35))
                    .map_err(|_| "document shutdown receipt timed out; artifacts retained".to_owned())?
            })
            .join()
            .map_err(|_| "document shutdown lane panicked".to_owned())?
        }) as shutdown::Job
    });
    let shutdown_result = shutdown::drain([
        prep_job,
        welcome_job,
        registration_job,
        settings_job,
        search_job,
        placement_job,
        markdown_job,
        logs_job,
        gpu_job,
        diagnostics_job,
        syntax_job,
        document_job,
    ]);
    if let Some(palette) = &mut app.palette {
        let handles = palette.stop();
        std::thread::spawn(move || {
            for handle in handles {
                let _ = handle.join();
            }
        });
    }
    shutdown_result.map_err(GuiError::Settings)?;
    result.map_err(|e| GuiError::Unavailable(e.to_string()))?;
    match app.error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

impl App {
    fn ch(&self) -> usize {
        self.font.as_ref().map_or(16, |f| f.ch)
    }

    fn text_rows(&self) -> usize {
        self.body_area().height.saturating_sub(self.ui.bar) / self.ch()
    }

    /// Chrome comes from DPI metrics; only the text grid uses the editor font.
    fn body_area(&self) -> kwil_view_model::TabArea {
        let mut area = chrome::layout(self.size, &self.ui, self.settings.tabs.layout).body;
        if !self.view.hex_active() && self.markdown_mode() == markdown_ui::Mode::Split {
            area.width /= 2;
        }
        log_ui::layout(area, &self.ui, !self.view.hex_active() && self.logs.is_open()).body
    }

    fn columns(&self) -> u64 {
        let cw = self.font.as_ref().map_or(8, |f| f.cw);
        (self.body_area().width.saturating_sub(self.ui.bar + GUTTER_COLS * cw) / cw) as u64
    }

    fn bars(&self) -> (scrollbar::Bar, scrollbar::Bar) {
        if self.view.hex_active() {
            return self.hex_bars();
        }
        let body = self.body_area();
        let height = body.height.saturating_sub(self.ui.bar);
        if self.logs.owns_body() {
            let (top, horizontal) = self.logs.viewport_position();
            let (total, exact, max_bytes) = self.logs.viewport_extent();
            let visible = (height / self.ch().max(1)).min(4096) as u64;
            let total = if exact { total } else { total.max(top.saturating_add(visible + 1)) };
            return (
                scrollbar::Bar::sized(0, height, visible, total, top, self.ui.min_thumb),
                scrollbar::Bar::sized(
                    0,
                    body.width.saturating_sub(self.ui.bar),
                    self.columns(),
                    max_bytes,
                    horizontal,
                    self.ui.min_thumb,
                ),
            );
        }
        let (page, total) =
            horizontal_extent(&self.view, self.columns() as usize, self.settings.editor.tab_size as usize);
        (
            scrollbar::Bar::sized(
                0,
                height,
                self.view.rows() as u64,
                self.view.line_count().unwrap_or(self.view.rows() as u64),
                self.view.top(),
                self.ui.min_thumb,
            ),
            scrollbar::Bar::sized(
                0,
                body.width.saturating_sub(self.ui.bar),
                page,
                total,
                self.horizontal,
                self.ui.min_thumb,
            ),
        )
    }

    /// Current tab strip, built exactly as it is drawn.
    fn tab_frame(&mut self) -> tabs::Frame {
        let mut frame = tabs::frame(
            &self.view,
            self.size,
            &self.ui,
            self.ch(),
            self.settings.tabs.layout,
            self.ui_font.as_mut(),
            self.hover,
        );
        let body = self.body_area();
        frame.geometry.body = body;
        frame.geometry.text = kwil_view_model::TabArea {
            x: body.x,
            y: body.y,
            width: body.width.saturating_sub(self.ui.bar),
            height: body.height.saturating_sub(self.ui.bar) / self.ch() * self.ch(),
        };
        frame
    }

    /// Scrollbar thumb under the pointer, for hover feedback.
    fn thumb_hover(&self) -> Option<chrome::Hover> {
        let body = self.body_area();
        if !body.contains(self.pointer.0, self.pointer.1) {
            return None;
        }
        let (x, y) = (self.pointer.0 - body.x, self.pointer.1 - body.y);
        let track = body.height.saturating_sub(self.ui.bar);
        // Text-area moves stay cheap: the horizontal extent reads row tails,
        // so it is measured only while the pointer is over that track.
        if x >= body.width.saturating_sub(self.ui.bar) && y < track {
            let v = self.bars().0;
            return (v.maximum > 0 && v.contains_thumb(y)).then_some(chrome::Hover::VerticalThumb);
        }
        if y >= track && y < track + self.ui.bar {
            let h = self.bars().1;
            return (x < h.length && h.maximum > 0 && h.contains_thumb(x)).then_some(chrome::Hover::HorizontalThumb);
        }
        None
    }

    /// Recomputes the hover target; a change damages only chrome and status.
    fn sync_presenter(&mut self) -> bool {
        let active = self.gpu_active();
        if active == self.presenter_gpu {
            return false;
        }
        self.presenter_gpu = active;
        #[cfg(feature = "gpu")]
        if let Some(gpu) = &mut self.gpu {
            gpu.invalidate();
        }
        #[cfg(all(windows, feature = "gpu"))]
        if !active && let Some(canvas) = self.gfx.as_ref().and_then(|gfx| gfx.gpu_canvas.as_ref()) {
            canvas.hide();
        }
        self.pending.full = true;
        self.chrome_dirty = true;
        self.request_frame();
        true
    }
    fn gpu_active(&self) -> bool {
        #[cfg(feature = "gpu")]
        {
            self.gpu.is_some()
                && !self.policy.remote()
                && self.opts.renderer.unwrap_or(self.settings.render.backend)
                    != kwil_core_commands::settings::RenderBackend::Cpu
        }
        #[cfg(not(feature = "gpu"))]
        {
            false
        }
    }
    fn update_pointer_cursor(&self) {
        if let Some(gfx) = &self.gfx {
            gfx.window.set_cursor(self.pointer_cursor());
        }
    }
    fn pointer_cursor(&self) -> winit::window::CursorIcon {
        let pointer = self.pointer;
        let modal = self.file_modal()
            || self.language_owns_input()
            || self.save_path.is_some()
            || self.settings_open()
            || self.palette_open()
            || self.search_panel.is_open()
            || self.logs.is_open()
            || self.menu.state.is_open();
        let text = !modal
            && !self.context.is_open()
            && self.markdown_mode() != markdown_ui::Mode::Preview
            && self.chrome_frame.as_ref().is_some_and(|frame| frame.geometry.text.contains(pointer.0, pointer.1))
            && !markdown_ui::preview_area(self).is_some_and(|area| area.contains(pointer));
        let button = self.menu.toolbar.iter().any(|tool| tool.enabled && tool.rect.contains(pointer))
            || self.menu.titles.iter().any(|area| area.contains(pointer.0, pointer.1))
            || self.status_hits.iter().any(|(area, _, _)| area.contains(pointer))
            || self.menu.dropdown.as_ref().is_some_and(|list| list.area.contains(pointer));
        if let Some(drag) = self.drag {
            if drag.vertical { winit::window::CursorIcon::NsResize } else { winit::window::CursorIcon::EwResize }
        } else if text {
            winit::window::CursorIcon::Text
        } else if button {
            winit::window::CursorIcon::Pointer
        } else {
            winit::window::CursorIcon::Default
        }
    }
    fn update_hover(&mut self) {
        let (x, y) = self.pointer;
        let lay = chrome::layout(self.size, &self.ui, self.settings.tabs.layout);
        let hover = if let Some(index) = self.menu.titles.iter().position(|title| title.contains(x, y)) {
            chrome::Hover::Title(index)
        } else if lay.strip.contains(x, y) {
            let frame = self.tab_frame();
            tabs::hover(&frame, x, y)
        } else if lay.status.contains(x, y) {
            self.status_hits
                .iter()
                .find(|(rect, _, _)| rect.contains((x, y)))
                .map_or(chrome::Hover::None, |(_, index, _)| chrome::Hover::Status(*index))
        } else {
            self.thumb_hover().unwrap_or_default()
        };
        if hover != self.hover {
            self.hover = hover;
            self.chrome_dirty = true;
            self.apply(Damage { status: true, ..Default::default() });
        }
    }

    fn page_bytes(&self) -> u64 {
        horizontal_extent(&self.view, self.columns() as usize, self.settings.editor.tab_size as usize).0
    }

    fn scroll_bar(&mut self, vertical: bool, offset: u64) {
        if self.hex_scroll_bar(vertical, offset) {
            return;
        }
        if self.logs.owns_body() {
            let (top, horizontal) = self.logs.viewport_position();
            self.logs_scroll_to(if vertical { offset } else { top }, if vertical { horizontal } else { offset });
            return;
        }
        if vertical {
            let delta = (offset as i128 - self.view.top() as i128).clamp(i64::MIN as i128, i64::MAX as i128);
            let damage = self.view.scroll(delta as i64);
            self.apply(damage);
        } else if self.horizontal != offset {
            self.horizontal = offset;
            let damage = self.view.set_horizontal_window(offset, 64 << 10);
            self.apply(damage);
        }
    }

    fn pointer_down(&mut self) {
        let body = self.body_area();
        if !body.contains(self.pointer.0, self.pointer.1) {
            return;
        }
        let (x, y) = (self.pointer.0 - body.x, self.pointer.1 - body.y);
        let (v, h) = self.bars();
        let vertical = x >= body.width.saturating_sub(self.ui.bar) && y < v.length;
        let horizontal = y >= v.length && y < v.length + self.ui.bar && x < h.length;
        if !vertical && !horizontal {
            return;
        }
        let (bar, pixel, offset, page) = if vertical {
            (
                v,
                y,
                if self.view.hex_active() {
                    self.view.hex_session().map_or(0, |s| s.view().top())
                } else if self.logs.owns_body() {
                    self.logs.viewport_position().0
                } else {
                    self.view.top()
                },
                self.text_rows() as u64,
            )
        } else {
            (
                h,
                x,
                if self.view.hex_active() {
                    self.hex.horizontal
                } else if self.logs.owns_body() {
                    self.logs.viewport_position().1
                } else {
                    self.horizontal
                },
                if self.view.hex_active() {
                    (body.width.saturating_sub(self.ui.bar) / self.font.as_ref().map_or(8, |f| f.cw).max(1)) as u64
                } else if self.logs.owns_body() {
                    self.columns()
                } else {
                    self.page_bytes()
                },
            )
        };
        if bar.contains_thumb(pixel) {
            self.drag = Some(scrollbar::Drag::new(vertical, bar, pixel, offset));
        } else {
            self.scroll_bar(vertical, bar.page(pixel, offset, page));
        }
    }

    fn monitor_scale_changed(&mut self, scale_factor: f64) {
        self.close_language();
        self.drag = None;
        self.selecting = false;
        self.ui = chrome::Metrics::new(scale_factor);
        self.reload_ui_font();
        self.chrome_dirty = true;
        let rows = self.text_rows();
        let damage = self.view.set_rows(rows);
        self.apply(Damage { full: true, status: true, ..Default::default() });
        self.apply(damage);
        self.reload_font();
    }

    fn apply(&mut self, mut d: Damage) {
        let hex_damage = self.sync_hex_host();
        d.full |= hex_damage.full;
        d.status |= hex_damage.status;
        d.rows.extend(hex_damage.rows);
        if self.sync_presenter() {
            d.full = true;
        }
        if !self.view.hex_active() {
            self.sync_markdown();
            self.sync_logs();
        }
        // Addressed events/edits invalidate search by document + revision.
        // This only captures an existing typed Editor snapshot, never decodes.
        if !self.view.hex_active()
            && let Some(session) = &mut self.search
        {
            match self.search_panel.sync_document(session, &self.view) {
                Ok(outcome) => {
                    self.search_dirty |= outcome.damage.panel || outcome.damage.signal;
                    d.status |= outcome.damage.status;
                    d.rows.extend(outcome.damage.hit_rows);
                }
                Err(error) => {
                    self.search_panel.set_error(error);
                    self.search_dirty = true;
                }
            }
        }
        self.sync_goto_identity();
        self.sync_language_identity();

        if !self.view.hex_active()
            && let Some(highlighting) = &mut self.highlighting
        {
            highlighting.invalidate_painted(&self.view, &d);
            highlighting.update(&self.view);
        }
        if d == Damage::default()
            && !self.chrome_dirty
            && !self.palette_dirty
            && !self.menu_dirty
            && !self.search_dirty
            && !self.language_picker.dirty
        {
            return;
        }
        queue_damage(&mut self.pending, d, self.view.rows(), self.font.as_mut());
        if let Some(dialog) = &self.desktop_window {
            dialog.gfx.window.request_redraw();
        }
        if let Some(g) = &self.gfx {
            self.redraw_requested = true;
            g.window.request_redraw();
        }
    }

    fn key(&mut self, el: &ActiveEventLoop, ev: &KeyEvent) {
        if ev.state != ElementState::Pressed {
            return;
        }
        if matches!(ev.logical_key, Key::Named(NamedKey::Escape)) {
            self.ime_destination = Some(ime::Destination::Cancelled);
            self.composing = false;
        }
        if self.composing && !matches!(ev.logical_key, Key::Named(NamedKey::Escape)) {
            return;
        }
        // A file prompt owns input ahead of every overlay, including settings.
        if self.save_path.is_none() && self.settings_key(ev) {
            return;
        }
        self.key_after_settings(
            Some(el),
            &HostKey { logical_key: ev.logical_key.clone(), text: ev.text.as_deref(), repeat: ev.repeat },
        );
    }

    fn key_after_settings(&mut self, el: Option<&ActiveEventLoop>, ev: &HostKey<'_>) {
        if self.language_key(ev) {
            return;
        }
        if self.file_modal() {
            if self.close_dialog.is_none() && matches!(ev.logical_key, Key::Named(NamedKey::Escape)) {
                self.view.cancel_file_prompt();
                self.sync_file_ui();
                self.apply(Damage { status: true, ..Default::default() });
            }
            return;
        }
        if self.composing && !matches!(ev.logical_key, Key::Named(NamedKey::Escape)) {
            return;
        }
        if matches!(ev.logical_key, Key::Named(NamedKey::Escape)) {
            self.ime_destination = Some(ime::Destination::Cancelled);
            self.search_ime = None;
            self.composing = false;
        }
        if self.save_path.is_some() {
            if matches!(ev.logical_key, Key::Named(NamedKey::ArrowUp | NamedKey::ArrowDown)) {
                self.prompt_step(matches!(ev.logical_key, Key::Named(NamedKey::ArrowUp)));
                return;
            }
            match &ev.logical_key {
                Key::Named(NamedKey::Escape) => {
                    file_prompt::cancel(&mut self.view);
                    self.sync_file_ui();
                }
                Key::Named(NamedKey::Enter) => {
                    self.update_file_prompt();
                    let damage = file_prompt::submit(&mut self.view);
                    self.sync_file_ui();
                    self.apply(damage);
                    return;
                }
                Key::Named(NamedKey::Backspace) => {
                    self.save_path.as_mut().unwrap().pop();
                }
                _ => {
                    if let Some(text) = ev.text.filter(|text| !text.chars().any(char::is_control)) {
                        self.save_path.as_mut().unwrap().push_str(text);
                    }
                }
            }
            self.update_file_prompt();
            return;
        }
        if self.markdown_mode() == markdown_ui::Mode::Preview
            && !self.palette_open()
            && !self.search_panel.is_open()
            && !self.menu.state.is_open()
            && !self.mods.control_key()
            && !self.mods.alt_key()
            && !self.mods.super_key()
        {
            match ev.logical_key {
                Key::Named(NamedKey::Escape) => {
                    self.dispatch_markdown("view.markdownText");
                }
                Key::Named(NamedKey::ArrowDown | NamedKey::PageDown) => {
                    self.markdown_scroll_key(3);
                }
                Key::Named(NamedKey::ArrowUp | NamedKey::PageUp) => {
                    self.markdown_scroll_key(-3);
                }
                _ => {}
            }
            return;
        }
        // A scoped native-window probe can exercise the actual asynchronous
        // save callback without injecting global Ctrl state or taking focus.
        #[cfg(feature = "frame-harness")]
        if self.opts.hidden && matches!(ev.logical_key, Key::Named(NamedKey::F12)) {
            self.save();
            return;
        }
        #[cfg(all(feature = "frame-harness", debug_assertions))]
        if self.opts.hidden {
            let command = match ev.logical_key {
                Key::Named(NamedKey::F11) => Some("file.new"),
                Key::Named(NamedKey::F10) => Some("file.nextTab"),
                Key::Named(NamedKey::F9) => Some("file.previousTab"),
                Key::Named(NamedKey::F8) => Some("file.close"),
                Key::Named(NamedKey::F6) => Some("view.commandPalette"),
                Key::Named(NamedKey::F2) => Some("view.zoomIn"),
                _ => None,
            };

            if let Some(command) = command {
                if self.dispatch_search(command)
                    || self.dispatch_settings(command)
                    || self.dispatch_palette(command)
                    || self.dispatch_zoom(command)
                {
                    return;
                }
                self.dispatch_file(command);
                return;
            }
        }
        if matches!(ev.logical_key, Key::Named(NamedKey::ContextMenu))
            || (self.mods.shift_key() && matches!(ev.logical_key, Key::Named(NamedKey::F10)))
        {
            if self.view.hex_active() {
                self.hex_cancel_input();
                return;
            }
            self.open_context();
            return;
        }
        if self.context_key(ev) {
            return;
        }
        if self.logs_key(ev) {
            return;
        }
        if self.search_key(ev) {
            return;
        }
        if self.palette_key(ev) {
            return;
        }
        if self.menu_key(el, ev) {
            return;
        }
        if self.close_pending && matches!(ev.logical_key, Key::Named(NamedKey::Escape)) {
            self.view.cancel_file_prompt();
            self.hex_cancel_input();
            if let Some(keys) = &mut self.keys {
                keys.cancel_pending();
            }
            self.sync_file_ui();
            let damage = self.view.set_notice(String::new());
            self.apply(damage);
            return;
        }
        if !self.composing
            && let Some(chord) = input::chord(&ev.logical_key, self.mods)
        {
            if self.keys.is_none() {
                self.keys =
                    kwil_core_commands::keymap::KeymapEngine::new(self.settings.keymap, &self.settings.bindings).ok();
            }
            if let Some(keys) = &mut self.keys {
                // Resolve configured commands, tombstones and chord prefixes
                // in the actual body context before any hex key fallback.
                keys.set_context(if self.view.hex_active() {
                    kwil_core_commands::Context::Hex
                } else {
                    kwil_core_commands::Context::Editor
                });
                use kwil_core_commands::vim::VimResult;
                match keys.feed_with_selection(chord) {
                    VimResult::Pending => return,
                    VimResult::Command(call) => {
                        if self.dispatch_hex(call.id) {
                            return;
                        }
                        if self.logs.owns_body() && call.id.starts_with("edit.") {
                            return;
                        }
                        if self.dispatch_logs(call.id)
                            || self.dispatch_language(call.id)
                            || self.dispatch_search_call(&call)
                            || self.dispatch_settings(call.id)
                            || self.dispatch_palette(call.id)
                        {
                            return;
                        }
                        if self.dispatch_zoom(call.id) {
                            return;
                        }
                        if self.dispatch_file(call.id) {
                            if self.may_exit()
                                && let Some(el) = el
                            {
                                el.exit();
                            }
                            return;
                        }
                        if self.settings.keymap == kwil_core_commands::Preset::Vim {
                            if call.id == "file.save" {
                                self.save();
                                return;
                            }
                            if call.id == "file.quit" {
                                if let Some(el) = el {
                                    self.request_close(el, false);
                                }
                                return;
                            }
                            if call.id == "file.saveAndQuit" {
                                self.close_pending = true;
                                self.save();
                                return;
                            }
                            if self.preview_blocks_command(call.id) {
                                return;
                            }
                            if let Some(editor) = self.view.editor_mut() {
                                match editor.execute_vim(&call) {
                                    Ok(result) => {
                                        let damage = self.view.refresh_after_edit(result.damage);
                                        self.apply(damage);
                                        self.keep_caret_visible();
                                        self.command_status(result.status);
                                    }
                                    Err(error) => {
                                        let damage = self.view.set_notice(error.to_string());
                                        self.apply(damage);
                                    }
                                }
                            }
                        } else {
                            self.edit(input::Action::Command(call.id));
                        }
                        return;
                    }
                    VimResult::Error(error) => {
                        let damage = self.view.set_notice(error);
                        self.apply(damage);
                        return;
                    }
                    VimResult::Unbound => {
                        let suppressed = keys.suppresses_fallback();
                        let fallback = Self::unbound_key(keys, &ev.logical_key, ev.text, self.mods);
                        if self.view.hex_active() {
                            if suppressed {
                                return;
                            }
                            if self.hex_key(ev) {
                                return;
                            }
                        }
                        match fallback {
                            UnboundKey::Quit => {
                                if !ev.repeat
                                    && let Some(el) = el
                                {
                                    self.request_close(el, self.close_pending && self.mods.shift_key());
                                }
                            }
                            UnboundKey::Edit(action) => self.edit(action),
                            UnboundKey::Ignore => {}
                        }
                        return;
                    }
                }
            }
        }
        if self.hex_key(ev) {
            return;
        }
        if let Some(action) = input::translate(&ev.logical_key, ev.text, self.mods, self.composing) {
            self.edit(action);
        }
    }

    fn unbound_key(
        keys: &kwil_core_commands::keymap::KeymapEngine,
        key: &Key,
        text: Option<&str>,
        mods: ModifiersState,
    ) -> UnboundKey {
        if keys.suppresses_fallback() {
            return UnboundKey::Ignore;
        }
        if mods.control_key() && matches!(key, Key::Character(c) if c.eq_ignore_ascii_case("q")) {
            return UnboundKey::Quit;
        }
        if keys.mode() != kwil_core_commands::Mode::Insert || mods.control_key() || mods.super_key() || mods.alt_key() {
            return UnboundKey::Ignore;
        }
        input::translate(key, text, mods, false).map_or(UnboundKey::Ignore, UnboundKey::Edit)
    }

    fn edit(&mut self, action: input::Action) {
        if self.file_modal() || self.language_owns_input() {
            return;
        }
        if self.hex_edit(&action) {
            return;
        }
        let (id, text) = match action {
            input::Action::Command(id) => (id, None),
            input::Action::Text(text) => ("edit.insert", Some(text)),
        };
        if self.logs.owns_body() && (id.starts_with("edit.") || id.starts_with("search.replace")) {
            return;
        }
        if self.dispatch_language(id)
            || self.dispatch_logs(id)
            || self.dispatch_markdown(id)
            || self.dispatch_search(id)
            || self.dispatch_settings(id)
            || self.dispatch_palette(id)
            || self.dispatch_zoom(id)
            || self.dispatch_file(id)
        {
            return;
        }
        if self.markdown_mode() == markdown_ui::Mode::Preview && id.starts_with("edit.") && id != "edit.copy" {
            return;
        }
        let args =
            CommandArgs { text, at: self.started.elapsed(), extend: self.mods.shift_key(), ..Default::default() };
        let result = if matches!(id, "edit.copy" | "edit.cut" | "edit.paste") {
            let hwnd = self.gfx.as_ref().map_or(0, |gfx| gfx.hwnd);
            self.view
                .editor_mut()
                .map(|editor| editor.execute_with_clipboard(id, &args, 1, Some(&mut clipboard::SystemClipboard(hwnd))))
                .unwrap_or_else(|| self.view.execute(id, &args, 1))
                .map(|mut result| {
                    result.damage = self.view.refresh_after_edit(result.damage);
                    result
                })
        } else {
            self.view.execute(id, &args, 1)
        };
        match result {
            Ok(result) => {
                self.apply(result.damage);
                self.keep_caret_visible();
                self.command_status(result.status);
            }
            Err(error) => {
                let damage = self.view.set_notice(error.to_string());
                self.apply(damage);
            }
        }
    }

    fn command_status(&mut self, status: CommandStatus) {
        if status == CommandStatus::SaveRequested {
            self.save();
        } else if let Some(notice) = command_notice(status) {
            let damage = self.view.set_notice(notice);
            self.apply(damage);
        }
    }

    fn keep_caret_visible(&mut self) {
        if self.view.hex_active() {
            return;
        }
        let Some(editor) = self.view.editor() else {
            return;
        };
        let Ok((row, byte)) = editor.byte_position(editor.primary_cursor().head) else {
            return;
        };
        if row < self.view.top() || row - self.view.top() >= self.view.rows() as u64 {
            return;
        }
        let line = self.view.line((row - self.view.top()) as usize).unwrap_or_default();
        let cells = text::cells(line, self.columns() as usize, self.settings.editor.tab_size as usize);
        let last_byte = cells.last().map_or(0, |cell| cell.bytes.end as u64);
        let last_column = cells.last().map_or(0, |cell| cell.column + cell.width);
        let start = self.view.line_offset((row - self.view.top()) as usize);
        if byte < start
            || byte > start + last_byte
            || (byte == start + last_byte && last_column >= self.columns() as usize)
        {
            self.scroll_bar(false, byte);
        }
    }

    fn may_exit(&mut self) -> bool {
        if self.file_dialogs.exiting() || self.view.wants_exit() {
            self.external_inbox.close();
        }
        if self.close_dialog.is_some() {
            return false;
        }
        self.file_dialogs.may_exit(&mut self.view) && self.view.prepare_hex_shutdown()
    }
    fn force_exit(&mut self) -> bool {
        if self.close_dialog.is_some() {
            return false;
        }
        self.external_inbox.close();
        self.file_dialogs.force_exit(&mut self.view) && self.view.prepare_hex_shutdown()
    }
    fn request_close(&mut self, el: &ActiveEventLoop, discard: bool) {
        if self.file_dialogs.exiting() {
            if self.may_exit() {
                el.exit();
            }
            return;
        }

        let result =
            if discard { self.view.request_window_close(true) } else { self.view.request_window_close_with_prompt() };
        if let Err(error) = result {
            self.view.set_notice(error.to_string());
        }
        self.sync_file_ui();
        self.apply(Damage { full: true, status: true, ..Default::default() });
        if self.may_exit() {
            el.exit();
        }
    }

    fn save(&mut self) {
        self.dispatch_file("file.save");
    }

    fn place_caret(&mut self, extend: bool) {
        if self.hex_pointer(extend) {
            return;
        }
        let body = self.body_area();
        let Some(font) = &self.font else {
            return;
        };
        let row = self.view.top()
            + (self.pointer.1.saturating_sub(body.y) / font.ch).min(self.view.rows().saturating_sub(1)) as u64;
        let screen_row = row.saturating_sub(self.view.top()) as usize;
        let column = self.pointer.0.saturating_sub(body.x + GUTTER_COLS * font.cw) / font.cw;
        let columns = self.columns() as usize;
        let bytes = self.view.line(screen_row).unwrap_or_default();
        let cells = text::cells(bytes, columns, self.settings.editor.tab_size as usize);
        let byte = hit::byte_at(&cells, column);
        let first = self.view.line_offset(screen_row);
        // Anchor the window to the bytes actually displayed on the clicked
        // row. A requested offset inside a scalar was aligned down by View;
        // retaining that request would hide the first inserted ASCII byte.
        let aligned_window = if first != self.horizontal {
            self.horizontal = first;
            Some(self.view.set_horizontal_window(first, 64 << 10))
        } else {
            None
        };
        let Some(editor) = self.view.editor_mut() else {
            return;
        };
        let result = editor.line_range(row).and_then(|range| {
            let offset = (range.start + first + byte as u64).min(range.end);
            let anchor = if extend { editor.primary_cursor().anchor } else { offset };
            if editor.cursors().len() == 1
                && editor.primary_cursor().head == offset
                && editor.primary_cursor().anchor == anchor
            {
                return Ok(Damage::default());
            }
            editor.set_cursors(vec![kwil_view_model::Cursor { anchor, head: offset, desired_column: None }], 0)
        });
        match result {
            Ok(damage) => {
                let damage = self.view.refresh_after_cursor(damage);
                self.apply(damage);
                if let Some(damage) = aligned_window {
                    self.apply(damage);
                }
            }
            Err(error) => {
                let damage = self.view.set_notice(error.to_string());
                self.apply(damage);
            }
        }
    }

    fn render(&mut self) -> bool {
        // A native present already in flight keeps ownership until its worker
        // acknowledges the pause. Preserve all damage and wait for that wake.
        #[cfg(all(feature = "gpu", not(windows)))]
        {
            let active = self.gpu_active();
            if let Some(gpu) = &mut self.gpu
                && !gpu.set_active(active)
            {
                self.pending.full = true;
                return true;
            }
        }
        if let Some(size) = self.pending_resize.take() {
            self.size = size;
            self.resize_surface();
            let rows = self.text_rows();
            let damage = self.view.set_rows(rows);
            queue_damage(&mut self.pending, damage, rows, self.font.as_mut());
            self.pending.full = true;
        }
        if self.pending == Damage::default()
            && !self.language_picker.dirty
            && !self.chrome_dirty
            && !self.palette_dirty
            && !self.menu_dirty
            && !self.overlay_paint_pending()
        {
            return true;
        }
        // Card-only repaints (hover, wheel) are admitted once; open cards then
        // repaint in place and damage only their own rectangles.
        let projection_body_dirty = self.logs.body_paint.pending();
        self.take_overlay_paint();
        let rows = self.text_rows();
        if rows != self.view.rows() {
            self.drag = None;
            self.selecting = false;
            let damage = self.view.set_rows(rows);
            queue_damage(&mut self.pending, damage, rows, self.font.as_mut());
            self.pending.full = true;
        }
        let (w, h) = self.size;
        let ch = self.ch();
        if w == 0 || h < ch {
            return true;
        }
        // Chrome geometry and text measurement need the UI font mutably.
        let frame = self.tab_frame();
        let lay = frame.layout;
        let geometry = frame.geometry.clone();
        let chrome_changed = self.chrome_dirty || self.chrome_frame.as_ref() != Some(&frame);
        self.chrome_frame = Some(frame);
        self.chrome_dirty = false;
        self.menu.titles = menu::title_areas(lay.menu, &self.ui, self.ui_font.as_mut());
        if let menu::State::Open { menu: index, item: Some(item), .. } = self.menu.state
            && std::mem::take(&mut self.menu.reveal)
            && let Some(title) = self.menu.titles.get(index)
        {
            let top = title.y + title.height;
            self.menu.scroll = menu::reveal(index, item, self.menu.scroll, top, h, &self.ui);
        }
        self.menu.dropdown = match (self.menu.state, self.ui_font.as_mut()) {
            (menu::State::Open { menu: index, .. }, Some(font)) => self.menu.titles.get(index).map(|title| {
                menu::dropdown(index, *title, (w, h), &self.ui, font, &self.menu.labels, self.menu.scroll)
            }),
            _ => None,
        };
        if let Some(dropdown) = &self.menu.dropdown {
            self.menu.scroll = dropdown.scroll;
        }
        let available = self.menu_available();
        self.menu.toolbar = menu::toolbar(lay.menu, &self.menu.titles, &self.ui, &available);
        if self.view.editor().is_some_and(|editor| editor.is_markdown())
            || self.markdown_mode() != markdown_ui::Mode::Text
        {
            let mode = self.markdown_mode();
            menu::markdown_tools(&mut self.menu.toolbar, lay.menu, &self.menu.titles, &self.ui, mode);
        }
        self.menu.toolbar_surface = Some((self.size.0, self.size.1, self.ui.scale.to_bits()));
        if let Some(native) = self.gfx.as_mut().and_then(|gfx| gfx.native.as_mut()) {
            native.toolbar_tooltips(&self.menu.toolbar, &self.menu.labels);
        }
        let hex_active = self.view.hex_active();
        let (notice, mut segments) = if hex_active {
            (
                self.hex_status(),
                vec![chrome::Segment { text: "Physical bytes".into(), command: Some("view.toggleHex"), warn: false }],
            )
        } else {
            chrome::status(&self.view, self.zoom.percent())
        };
        if !hex_active && let Some(host) = &self.highlighting {
            segments.push(chrome::language_segment(host.language_status(&self.view)));
        }
        let title = chrome::window_title(&self.view);
        let dragging = self.drag.map(|drag| drag.vertical);
        let hover = self.hover;
        let theme = theme_ui::chrome();
        let markdown_mode = if hex_active { markdown_ui::Mode::Text } else { self.markdown_mode() };
        let markdown_eligible = !hex_active && self.view.editor().is_some_and(|editor| editor.is_markdown());
        let markdown_area = tabs::rect(lay.body);
        let body = geometry.body;
        let (bw, bh) = (body.width, body.height);
        self.body_pixels.resize(bw.saturating_mul(bh), bg());
        let bars = self.bars();
        let rows = geometry.text.height / ch;
        let palette_open = self.welcome.visible()
            || self.language_open()
            || self.palette_open()
            || self.settings_open()
            || self.search_panel.is_open()
            || self.logs.is_open()
            || self.save_path.is_some();
        let menu_list = self.menu.dropdown.is_some() || self.menu_area.is_some();
        let gpu_active = self.gpu_active();
        let projection_owns_body = !hex_active && self.logs.owns_body();
        let projection_source = log_ui::Source::capture(&self.view);
        let spans: Vec<_> = self
            .highlighting
            .as_ref()
            .filter(|_| !hex_active)
            .into_iter()
            .flat_map(|host| host.spans(&self.view))
            .collect();
        let hex_source = self.view.hex_source();
        let hex_read_only = hex_active && self.view.document_busy(self.view.active_id());
        let hex_horizontal = self.hex.horizontal as usize;
        let (Some(gfx), Some(font)) = (self.gfx.as_mut(), self.font.as_mut()) else {
            return false;
        };
        if self.window_title != title {
            gfx.window.set_title(&title);
            self.window_title = title;
        }
        #[cfg(all(windows, feature = "gpu"))]
        if !gpu_active && let Some(canvas) = &gfx.gpu_canvas {
            canvas.hide();
        }
        let pixels = &mut gfx.pixels;
        let body_pixels = &mut self.body_pixels;
        let mut p = std::mem::take(&mut self.pending);
        let palette_dirty = std::mem::take(&mut self.palette_dirty);
        let menu_dirty = std::mem::take(&mut self.menu_dirty);
        let search_dirty = std::mem::take(&mut self.search_dirty);
        let language_dirty = std::mem::take(&mut self.language_picker.dirty);
        let language_restore = language_dirty && !self.language_picker.is_open();
        if (palette_ui::palette_blocks_copy(palette_open, self.palette_area.is_some())
            || search_ui::blocks_copy(self.search_panel.is_open(), self.search_area.is_some())
            || self.context.is_open()
            || self.context_area.is_some()
            || self.language_picker.area.is_some()
            || self.logs.blocks_copy()
            || menu_list)
            && p.scroll != 0
        {
            // A pending document scroll predates opening the overlay. Avoid
            // copying overlay pixels through the native scroll seam.
            p.full = true;
            p.scroll = 0;
        }
        let mut damage = Vec::new();
        #[cfg(feature = "frame-harness")]
        let mut copied = 0u64;
        let body_changed = p.full || p.scroll != 0 || !p.rows.is_empty() || projection_body_dirty;
        if p.full {
            font.invalidate_rows();
            pixels.fill(bg());
            body_pixels.fill(bg());
            for row in (0..rows).filter(|_| !projection_owns_body && !hex_active) {
                draw_row(body_pixels, bw, font, &self.view, row, self.horizontal, &spans);
                search_ui::draw_marks(&self.search_panel, body_pixels, bw, font, &self.view, row);
                draw_editor_marks(body_pixels, bw, font, &self.view, row, self.view.line_offset(row));
            }
            tabs::composite(body_pixels, bw, pixels, w, body, Rect { x: 0, y: 0, w: bw, h: bh });
            p.status = true;
            damage.push(Rect { x: 0, y: 0, w, h });
        } else {
            if let Some((source, target)) = geometry.scroll_copy(p.scroll) {
                let dy = p.scroll.unsigned_abs() as usize * ch;
                let keep = source.height;
                let (from, to) = if p.scroll > 0 { (dy, 0) } else { (0, dy) };
                body_pixels.copy_within(from * bw..(from + keep) * bw, to * bw);
                let moved = Rect { x: 0, y: to, w: bw, h: keep };
                tabs::composite(body_pixels, bw, pixels, w, body, moved);
                #[cfg(feature = "gpu")]
                let cpu_scroll = !gpu_active;
                #[cfg(not(feature = "gpu"))]
                let cpu_scroll = true;
                if cpu_scroll && !screen_copy_region(&gfx.window, gfx.hwnd, source, target) {
                    damage.push(tabs::rect(target));
                }
                #[cfg(feature = "frame-harness")]
                {
                    copied = (source.width * source.height) as u64;
                }
            }
            p.rows.sort_unstable();
            p.rows.dedup();
            for &row in p.rows.iter().filter(|&&r| r < rows && !projection_owns_body && !hex_active) {
                draw_row(body_pixels, bw, font, &self.view, row, self.horizontal, &spans);
                search_ui::draw_marks(&self.search_panel, body_pixels, bw, font, &self.view, row);
                search_ui::draw_marks(&self.search_panel, body_pixels, bw, font, &self.view, row);
                draw_editor_marks(body_pixels, bw, font, &self.view, row, self.view.line_offset(row));
                let area = Rect { x: 0, y: row * ch, w: bw, h: ch };
                tabs::composite(body_pixels, bw, pixels, w, body, area);
                damage.push(tabs::translated(area, body));
            }
        }
        if hex_active && body_changed {
            let mut paint_damage = p.clone();
            // The common retained-body/native path already copied this scroll.
            paint_damage.scroll = 0;
            if let Some(session) = self.view.hex_session() {
                hex_ui::draw_damage(
                    session,
                    body_pixels,
                    bw,
                    font,
                    hex_ui::Paint {
                        current: hex_source,
                        horizontal: hex_horizontal,
                        read_only: hex_read_only,
                        damage: &paint_damage,
                    },
                );
            } else {
                // Opening/reopen is still hex-owned even before its first page.
                body_pixels.fill(bg());
            }
            if p.full {
                tabs::composite(body_pixels, bw, pixels, w, body, Rect { x: 0, y: 0, w: bw, h: bh });
            } else {
                for &row in p.rows.iter().filter(|&&r| r < rows) {
                    let area = Rect { x: 0, y: row * ch, w: bw, h: ch };
                    tabs::composite(body_pixels, bw, pixels, w, body, area);
                    damage.push(tabs::translated(area, body));
                }
            }
        }
        if projection_owns_body && body_changed {
            log_ui::draw_body(&self.logs, body_pixels, bw, font, projection_source);
            tabs::composite(body_pixels, bw, pixels, w, body, Rect { x: 0, y: 0, w: bw, h: bh });
            damage.push(tabs::rect(body));
        }
        let chrome_frame = self.chrome_frame.as_ref().unwrap();
        if p.full || chrome_changed {
            draw_chrome(chrome_frame, &self.menu, pixels, w, self.ui_font.as_mut(), theme, &self.ui, hover);
            if !p.full {
                damage.push(tabs::rect(lay.menu));
                damage.push(tabs::rect(geometry.chrome));
            }
        }
        // Overlays leave by restoring body and chrome pixels underneath them.
        let mut logs_restore_area = self.logs.take_restore();
        let mut content_restored = false;
        for (dirty, area) in [
            (palette_dirty, &mut self.palette_area),
            (menu_dirty, &mut self.menu_area),
            (menu_dirty, &mut self.context_area),
            (search_dirty, &mut self.search_area),
            (language_restore, &mut self.language_picker.area),
            (logs_restore_area.is_some(), &mut logs_restore_area),
        ] {
            if dirty && let Some(area) = area.take() {
                content_restored = true;
                tabs::composite(body_pixels, bw, pixels, w, body, Rect { x: 0, y: 0, w: bw, h: bh });
                draw_chrome(chrome_frame, &self.menu, pixels, w, self.ui_font.as_mut(), theme, &self.ui, hover);
                p.status = true;
                damage.push(area);
            }
        }
        if p.status {
            self.status_hits = chrome::draw_status(
                pixels,
                w,
                lay.status,
                self.ui_font.as_mut(),
                theme,
                &self.ui,
                &notice,
                &segments,
                hover,
            );
            if !p.full {
                damage.push(tabs::rect(lay.status));
            }
        }
        if body_changed || p.status || search_dirty {
            let (vertical, horizontal) = bars;
            let (bar, line, inset) = (self.ui.bar, self.ui.line, self.ui.inset);
            let vr = Rect { x: bw.saturating_sub(bar), y: 0, w: bar.min(bw), h: vertical.length };
            let hr = Rect { x: 0, y: vertical.length, w: bw, h: bar.min(bh.saturating_sub(vertical.length)) };
            let track = chrome::mix(bg(), fg(), 0x0a);
            fill(body_pixels, bw, vr, track);
            fill(body_pixels, bw, hr, track);
            // Hairlines keep the bars distinct from text and from a signal strip.
            let edge = chrome::mix(bg(), fg(), 0x1c);
            fill(body_pixels, bw, Rect { w: line.min(vr.w), ..vr }, edge);
            fill(body_pixels, bw, Rect { h: line.min(hr.h), ..hr }, edge);
            let thumb = |active: bool, hovered: bool| {
                let amount = if active {
                    0xa0
                } else if hovered {
                    0x7c
                } else {
                    0x52
                };
                chrome::mix(bg(), fg(), amount)
            };
            if vertical.maximum > 0 && vr.w > 2 * inset {
                let r =
                    Rect { x: vr.x + inset, y: vertical.thumb_start, w: vr.w - 2 * inset, h: vertical.thumb_length };
                let color = thumb(dragging == Some(true), hover == chrome::Hover::VerticalThumb);
                chrome::round_rect(body_pixels, bw, r, r.w / 2, color);
            }
            if horizontal.maximum > 0 && hr.h > 2 * inset {
                let r = Rect {
                    x: horizontal.thumb_start,
                    y: hr.y + inset,
                    w: horizontal.thumb_length,
                    h: hr.h - 2 * inset,
                };
                let color = thumb(dragging == Some(false), hover == chrome::Hover::HorizontalThumb);
                chrome::round_rect(body_pixels, bw, r, r.h / 2, color);
            }
            let signal =
                Rect { x: vr.x.saturating_sub(self.ui.inset.max(2)), y: 0, w: self.ui.inset.max(2).min(vr.x), h: vr.h };
            if !hex_active {
                search_ui::draw_signal(&self.search_panel, body_pixels, bw, signal, theme);
            }
            for area in [signal, vr, hr] {
                tabs::composite(body_pixels, bw, pixels, w, body, area);
                if !p.full {
                    damage.push(tabs::translated(area, body));
                }
            }
        }
        if (body_changed || chrome_changed || content_restored)
            && let Some(ui_font) = self.ui_font.as_mut()
            && let Some(area) = markdown_ui::draw(
                &mut self.markdown,
                markdown_mode,
                markdown_eligible,
                self.view.top(),
                pixels,
                (w, h),
                markdown_area,
                ui_font,
                &self.ui,
                theme,
            )
        {
            damage.push(area);
        }
        if let Some(font) = self.ui_font.as_mut()
            && let Some(area) = log_ui::draw(&mut self.logs, pixels, (w, h), font, theme, &self.ui)
        {
            damage.push(area);
        }
        if let Some(caret) = self.logs.caret() {
            gfx.window.set_ime_cursor_area(
                winit::dpi::PhysicalPosition::new(caret.x as i32, caret.y as i32),
                winit::dpi::PhysicalSize::new(caret.w as u32, caret.h as u32),
            );
        }
        if let (Some(dropdown), Some(ui_font)) = (self.menu.dropdown.as_ref(), self.ui_font.as_mut()) {
            menu::draw_dropdown(&self.menu, dropdown, &available, pixels, w, ui_font, theme, &self.ui);
            self.menu_area = Some(dropdown.area);
            damage.push(dropdown.area);
        }
        // Overlays are desktop cards in the DPI chrome face; the editor font
        // and zoom never reach them. Each card repaints itself idempotently and
        // damages only its own rectangle, hover included.
        if let (Some(text), Some(ui_font)) = (self.save_path.as_deref(), self.ui_font.as_mut())
            && !self.palette.as_ref().is_some_and(|palette| palette.model().is_open())
            && !self.settings_panel.as_ref().is_some_and(|panel| panel.is_open())
        {
            self.palette_area = prompt_ui::draw(
                &self.view,
                text,
                pixels,
                (w, h),
                ui_font,
                theme,
                &self.ui,
                false,
                &mut self.prompt_view,
            );
            if let Some(area) = self.palette_area {
                damage.push(area);
            }
        }
        if let (Some(palette), Some(ui_font)) =
            (self.palette.as_ref().filter(|palette| palette.model().is_open()), self.ui_font.as_mut())
        {
            self.palette_area = palette_view::draw(
                palette.model(),
                pixels,
                (w, h),
                ui_font,
                theme,
                &self.ui,
                &self.menu.labels,
                &mut self.palette_view,
            );
            if let Some(area) = self.palette_area {
                damage.push(area);
            }
        }
        if let (Some(panel), Some(ui_font)) = (
            self.settings_panel.as_ref().filter(|panel| panel.is_open() && self.desktop_dispatch),
            self.ui_font.as_mut(),
        ) {
            // After adoption: self.file_types.as_mut().map(|page| page as &mut dyn settings_ui::HostedPage)
            if let Some(page) = &mut self.file_types {
                page.ui.setup = false;
            }
            let hosted = self.file_types.as_mut().map(|page| page as &mut dyn settings_ui::HostedPage);
            self.palette_area = settings_ui::draw(
                panel,
                pixels,
                (w, h),
                ui_font,
                theme,
                &self.ui,
                self.settings_epoch,
                &mut self.settings_view,
                hosted,
            );
            if let Some(area) = self.palette_area {
                damage.push(area);
            }
        }
        if self.desktop_dispatch
            && let Some(ui_font) = self.ui_font.as_mut()
            && let Some(area) = welcome_ui::draw(
                &self.welcome,
                &mut self.welcome_view,
                self.file_types.as_mut(),
                pixels,
                (w, h),
                ui_font,
                theme,
                &self.ui,
            )
        {
            damage.push(area);
        }
        // A glyph miss asks for the wide-coverage face: one read at a time per
        // App, fenced to the face that asked (see `font::FallbackJobs`). Checked
        // after the overlays so their misses are requested in this frame too.
        if let Some(ui_font) = self.ui_font.as_mut()
            && ui_font.take_fallback_request()
            && let Some(id) = self.ui_fallback.request(ui_font.id())
        {
            spawn_ui_fallback(&self.proxy, id);
        }
        if let (Some(prompt), Some(ui_font)) = (self.goto.as_ref(), self.ui_font.as_mut()) {
            self.palette_area = palette_view::draw(
                &prompt.model,
                pixels,
                (w, h),
                ui_font,
                theme,
                &self.ui,
                &self.menu.labels,
                &mut self.palette_view,
            );
            if let Some(area) = self.palette_area {
                damage.push(area);
            }
        }
        if self.search_panel.is_open() {
            if let Some(ui_font) = self.ui_font.as_mut() {
                self.search_area = search_view::draw(
                    &self.search_panel,
                    pixels,
                    (w, h),
                    ui_font,
                    theme,
                    &self.ui,
                    geometry.body.y,
                    &mut self.search_view,
                );
            }
            if let Some(area) = self.search_area {
                damage.push(area);
            }
        }
        if let Some(font) = self.ui_font.as_mut() {
            let anchor = self
                .status_hits
                .iter()
                .find(|(_, _, command)| *command == "file.setType")
                .map_or(tabs::rect(lay.status), |(area, _, _)| *area);
            if let Some(area) = language_ui::draw(&mut self.language_picker, pixels, (w, h), anchor, font, &self.ui) {
                damage.push(area);
            }
            self.context_area = context_menu::draw(&mut self.context, pixels, (w, h), font, &self.ui, theme);
            if let Some(area) = self.context_area {
                damage.push(area);
            }
        }
        let mut rects: Vec<_> = damage.iter().filter_map(|area| area.to_softbuffer()).collect();
        #[cfg(feature = "gpu")]
        if !rects.is_empty()
            && !self.policy.remote()
            && self.opts.renderer.unwrap_or(self.settings.render.backend) != RenderBackend::Cpu
            && let Some(gpu) = &mut self.gpu
        {
            #[cfg(windows)]
            let presented = {
                if gfx.gpu_canvas.is_none() {
                    match gpu_surface::Canvas::create(gfx.hwnd) {
                        Ok(canvas) => gfx.gpu_canvas = Some(canvas),
                        Err(error) => {
                            eprintln!("GPU canvas unavailable: {error}");
                        }
                    }
                }
                match &gfx.gpu_canvas {
                    Some(canvas) => gpu.present(
                        canvas.show((w, h)),
                        pixels,
                        (w, h),
                        &damage,
                        p.scroll,
                        ch,
                        tabs::rect(geometry.text),
                    ),
                    None => gpu::Presented::Failed("GPU child surface unavailable".into()),
                }
            };
            #[cfg(not(windows))]
            let presented =
                gpu.present(gfx.window.clone(), pixels, (w, h), &damage, p.scroll, ch, tabs::rect(geometry.text));
            match presented {
                gpu::Presented::Yes => {
                    #[cfg(feature = "frame-harness")]
                    self.metrics.present(p.full, damage.iter().map(|area| (area.w * area.h) as u64).sum(), copied);
                    return true;
                }
                gpu::Presented::Deferred => {
                    self.pending.full = true;
                    return true;
                }
                gpu::Presented::Failed(error) => {
                    eprintln!("kwil GPU presentation failed; using CPU: {error}");
                    self.gpu = None;
                    self.presenter_gpu = false;
                    #[cfg(windows)]
                    if let Some(canvas) = &gfx.gpu_canvas {
                        canvas.hide();
                    }
                    // CPU owns the untouched parent HWND. Repair the entire
                    // displayed image immediately, never wait for a later input.
                    damage.clear();
                    damage.push(Rect { x: 0, y: 0, w, h });
                    rects.clear();
                    if let Some(full) = (Rect { x: 0, y: 0, w, h }).to_softbuffer() {
                        rects.push(full);
                    }
                }
            }
        }
        if !rects.is_empty() {
            let mut target = match gfx.surface.buffer_mut() {
                Ok(target) => target,
                Err(error) => {
                    self.error = Some(GuiError::Unavailable(error.to_string()));
                    return false;
                }
            };
            if target.len() != pixels.len() {
                self.error = Some(GuiError::Unavailable("CPU surface resize failed".into()));
                return false;
            }
            target.copy_from_slice(pixels);
            match target.present_with_damage(&rects) {
                Ok(()) => {
                    #[cfg(feature = "frame-harness")]
                    self.metrics.present(p.full, damage.iter().map(|area| (area.w * area.h) as u64).sum(), copied);
                }
                Err(error) => {
                    self.error = Some(GuiError::Unavailable(error.to_string()));
                    return false;
                }
            }
        }
        true
    }
}

/// Reads the wide-coverage chrome face off the UI thread for face `id`.
fn spawn_ui_fallback(proxy: &HostProxy, id: u64) {
    let proxy = proxy.clone();
    std::thread::spawn(move || {
        let _ = proxy.send_event(UserEvent::UiFallbackReady(id, font::UiFont::read_fallback()));
    });
}

/// Menu bar and tab strip; both depend only on chrome metrics and UI text.
#[allow(clippy::too_many_arguments)]
fn draw_chrome(
    frame: &tabs::Frame,
    bar: &menu::MenuBar,
    pixels: &mut [u32],
    stride: usize,
    mut font: Option<&mut font::UiFont>,
    theme: &kwil_core_commands::theme::Theme,
    ui: &chrome::Metrics,
    hover: chrome::Hover,
) {
    menu::draw_bar(bar, frame.layout.menu, pixels, stride, font.as_deref_mut(), theme, ui, hover);
    tabs::draw(frame, pixels, stride, font, theme, ui);
}

fn horizontal_extent(view: &View, columns: usize, tab_size: usize) -> (u64, u64) {
    // The scroll offset is a source-byte column, not a display column. Find
    // each visible row's last screen in those same units, even if a shorter
    // byte row occupies more grid columns (tabs, escapes and wide glyphs).
    // Use the row that needs the greatest byte offset for both range and page.
    // Reading its suffix independently of the current window keeps the thumb
    // and right endpoint stable as the window approaches EOF.
    let columns = columns.clamp(1, 4096);
    let limit = columns.saturating_mul(256).min(64 << 10);
    let snapshot = view.editor().map(|editor| editor.document().snapshot());
    let (mut maximum, mut page) = (0, columns as u64);
    for row in 0..view.rows() {
        let Some(line) = view.line(row) else { continue };
        let (end, visible) = if let Some((snapshot, range)) =
            snapshot.as_ref().zip(view.editor().and_then(|editor| editor.line_range(view.top() + row as u64).ok()))
        {
            let length = range.end - range.start;
            let wanted = range.end.saturating_sub(limit as u64).max(range.start);
            let context = wanted.saturating_sub(3).max(range.start);
            // Only one bounded suffix is held at a time; neither the rest of
            // this line nor any offscreen row is scanned or materialized.
            let tail = (|| -> std::io::Result<Vec<u8>> {
                let mut tail = Vec::with_capacity((range.end - context) as usize);
                for chunk in snapshot.chunks(context..range.end)? {
                    tail.extend_from_slice(chunk.map_err(std::io::Error::other)?);
                }
                Ok(tail)
            })();
            match tail {
                Ok(tail) => {
                    let mut first = (wanted - context) as usize;
                    while kwil_view_model::grid::boundary(&tail, first) != first {
                        first += 1;
                    }
                    let end = context - range.start
                        + first as u64
                        + horizontal_suffix_start(&tail[first..], columns, tab_size) as u64;
                    (end, length.saturating_sub(end).max(1))
                }
                // A failed source read must not hide an already known range.
                Err(_) => (length.saturating_sub(1), 1),
            }
        } else {
            // Before the index is ready only the bounded loaded rows are known.
            let end = horizontal_suffix_start(line, columns, tab_size) as u64;
            (view.line_offset(row) + end, (line.len() as u64 - end).max(1))
        };
        if end > maximum || (end == maximum && visible < page) {
            maximum = end;
            page = visible;
        }
    }
    (page, maximum.saturating_add(page))
}

fn horizontal_suffix_start(bytes: &[u8], columns: usize, tab_size: usize) -> usize {
    // Binary search only within a grid-sized suffix. Every candidate starts
    // at a safe scalar boundary; the final fitting suffix reaches the end.
    let (mut left, mut right) = (0, bytes.len());
    while left < right {
        let mid = left + (right - left) / 2;
        let start = kwil_view_model::grid::boundary(bytes, mid);
        let cells = text::cells(&bytes[start..], columns, tab_size);
        if cells.last().map_or(0, |cell| cell.bytes.end) == bytes.len() - start {
            right = start;
        } else {
            left = mid + 1;
            while kwil_view_model::grid::boundary(bytes, left) != left {
                left += 1;
            }
        }
    }
    left
}

fn draw_row(
    buf: &mut [u32],
    w: usize,
    font: &mut Font,
    view: &View,
    r: usize,
    _horizontal: u64,
    spans: &[&kwil_view_model::highlighting::Span],
) {
    let (cw, ch) = (font.cw, font.ch);
    let y = r * ch;
    let gutter = GUTTER_COLS * cw;
    fill(buf, w, Rect { x: 0, y, w: gutter.min(w), h: ch }, gutter_bg());
    fill(buf, w, Rect { x: gutter, y, w: w.saturating_sub(gutter), h: ch }, bg());
    if let Some(text) = view.line(r) {
        let num = format!("{:>7} ", view.line_number(r));
        draw_text(buf, w, font, 0, y, num.as_bytes(), GUTTER_COLS, gutter_fg(), gutter_bg());
        draw_text(buf, w, font, gutter, y, text, w.saturating_sub(gutter + BAR) / cw, fg(), bg());
        if let Some(source) = view.line_source(r) {
            let columns = w.saturating_sub(gutter + BAR) / cw;
            let first = spans.partition_point(|span| span.range.end <= source.start as usize);
            let last = spans.partition_point(|span| span.range.start < source.end as usize);
            let row_spans = &spans[first..last.max(first)];
            if row_spans.is_empty() {
                return;
            }
            // Draw whole ASCII coloured runs, never one String per cell.
            if text.iter().take(columns).all(|byte| (32..127).contains(byte)) {
                for span in row_spans {
                    let start = span.range.start.saturating_sub(source.start as usize).min(text.len()).min(columns);
                    let end = span.range.end.saturating_sub(source.start as usize).min(text.len()).min(columns);
                    if start >= end {
                        continue;
                    }
                    let x = gutter + start * cw;
                    fill(buf, w, Rect { x, y, w: (end - start) * cw, h: ch }, bg());
                    draw_text(
                        buf,
                        w,
                        font,
                        x,
                        y,
                        &text[start..end],
                        end - start,
                        theme_ui::syntax_rgb(kwil_view_model::highlight_session::syntax_rgb(span.class)),
                        bg(),
                    );
                }
                return;
            }
            let cells = text::cells(text, columns, font.tab_size());
            // The existing RTL renderer owns visual placement for the whole
            // paragraph; logical cell overlays must not disturb that ordering.
            if !cells.iter().any(|cell| shaping::needs_bidi(&cell.text)) {
                for cell in cells {
                    let offset = source.start as usize + cell.bytes.start;
                    let index = spans.partition_point(|span| span.range.end <= offset);
                    if let Some(span) = spans.get(index).filter(|span| span.range.start <= offset)
                        && cell.text != "\t"
                    {
                        let color = theme_ui::syntax_rgb(kwil_view_model::highlight_session::syntax_rgb(span.class));
                        let x = gutter + cell.column * cw;
                        fill(buf, w, Rect { x, y, w: cell.width * cw, h: ch }, bg());
                        draw_text(buf, w, font, x, y, cell.text.as_bytes(), cell.width, color, bg());
                    }
                }
            }
        }
    }
}

fn draw_editor_marks(buf: &mut [u32], w: usize, font: &Font, view: &View, row: usize, horizontal: u64) {
    let Some(editor) = view.editor() else {
        return;
    };
    let absolute_row = view.top() + row as u64;
    let gutter = GUTTER_COLS * font.cw;
    let columns = w.saturating_sub(gutter + BAR) / font.cw;
    if let (Ok(range), Ok(selections), Some(line)) =
        (editor.line_range(absolute_row), editor.selection_ranges(absolute_row), view.line(row))
    {
        for cell in text::cells(line, columns, font.tab_size()) {
            let source =
                range.start + horizontal + cell.bytes.start as u64..range.start + horizontal + cell.bytes.end as u64;
            if !selections.iter().any(|selection| selection.start < source.end && source.start < selection.end) {
                continue;
            }
            let start = cell.column;
            let end = (cell.column + cell.width).min(columns);
            if end <= start {
                continue;
            }
            for y in row * font.ch..(row + 1) * font.ch {
                for x in gutter + start * font.cw..gutter + end * font.cw {
                    let pixel = &mut buf[y * w + x];
                    *pixel = ((*pixel & 0xfefefe) >> 1) + 0x183454;
                }
            }
        }
    }
    for cursor in editor.cursors() {
        if let Ok((cursor_row, byte)) = editor.byte_position(cursor.head)
            && cursor_row == absolute_row
            && byte >= horizontal
        {
            let line = view.line(row).unwrap_or_default();
            let cells = text::cells(line, columns, font.tab_size());
            let local = byte - horizontal;
            let column = hit::column_at(&cells, local.min(usize::MAX as u64) as usize);
            if column >= columns {
                continue;
            }
            fill(buf, w, Rect { x: gutter + column * font.cw, y: row * font.ch, w: 2, h: font.ch }, status_fg());
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Rect {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

impl Rect {
    fn contains(self, (x, y): (usize, usize)) -> bool {
        x >= self.x && y >= self.y && x - self.x < self.w && y - self.y < self.h
    }
    fn to_softbuffer(self) -> Option<softbuffer::Rect> {
        Some(softbuffer::Rect {
            x: self.x as u32,
            y: self.y as u32,
            width: NonZeroU32::new(self.w as u32)?,
            height: NonZeroU32::new(self.h as u32)?,
        })
    }
}

fn fill(buf: &mut [u32], stride: usize, r: Rect, color: u32) {
    for y in r.y..r.y + r.h {
        let row = &mut buf[y * stride..y * stride + stride];
        row[r.x.min(stride)..(r.x + r.w).min(stride)].fill(color);
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_text(
    buf: &mut [u32],
    stride: usize,
    font: &mut Font,
    x: usize,
    y: usize,
    text: &[u8],
    max_cols: usize,
    fg: u32,
    bg: u32,
) {
    let (cw, ch) = (font.cw, font.ch);
    // Pure ASCII avoids grapheme allocation and shaping entirely.
    if text.iter().take(max_cols).all(|b| (32..127).contains(b)) {
        for (i, &b) in text.iter().take(max_cols).enumerate() {
            if b != b' ' {
                copy_tile(buf, stride, x + i * cw, y, cw, ch, font.tile(b, fg, bg));
            }
        }
        return;
    }
    let cells = text::cells(text, max_cols.min(4096), font.tab_size());
    // Digits and neutral spaces belong to the same directional paragraph as
    // surrounding RTL text. Splitting them through the ASCII path loses UAX9
    // ordering. Keep all-ASCII and LTR-only mixed text on their existing paths.
    if cells.iter().any(|c| shaping::needs_bidi(&c.text)) {
        let mut paragraph = String::new();
        for c in &cells {
            if c.text == "\t" {
                paragraph.extend(std::iter::repeat_n(' ', c.width));
            } else {
                paragraph.push_str(&c.text);
            }
        }
        let columns = cells.iter().map(|c| c.width).sum();
        let key = shaping::TileKey { text: paragraph, columns, fg, bg };
        if let Some(tile) = font.unicode_tile(key, y) {
            copy_tile(buf, stride, x, y, columns * cw, ch, tile);
            return;
        }
        // The first CPU frame still shows ASCII while the lazy font worker is
        // unavailable; the resolved mixed paragraph replaces these placeholders.
        for c in &cells {
            if c.text == "\t" {
                continue;
            }
            if c.text.is_ascii() {
                for (j, b) in c.text.bytes().enumerate() {
                    if b != b' ' {
                        copy_tile(buf, stride, x + (c.column + j) * cw, y, cw, ch, font.tile(b, fg, bg));
                    }
                }
            } else {
                copy_tile(buf, stride, x + c.column * cw, y, cw, ch, font.tile(b'?', fg, bg));
            }
        }
        return;
    }
    let mut i = 0;
    while i < cells.len() {
        let c = &cells[i];
        let x0 = x + c.column * cw;
        if c.text == "\t" {
            i += 1;
            continue;
        }
        if c.text.is_ascii() {
            for (j, b) in c.text.bytes().enumerate() {
                if b != b' ' {
                    copy_tile(buf, stride, x0 + j * cw, y, cw, ch, font.tile(b, fg, bg));
                }
            }
            i += 1;
            continue;
        }
        let mut end = i + 1;
        while end < cells.len() && !cells[end].text.is_ascii() {
            end += 1;
        }
        let text = cells[i..end].iter().map(|c| c.text.as_str()).collect::<String>();
        let columns = cells[i..end].iter().map(|c| c.width).sum();
        let key = shaping::TileKey { text, columns, fg, bg };
        if let Some(tile) = font.unicode_tile(key, y) {
            copy_tile(buf, stride, x0, y, columns * cw, ch, tile);
        } else {
            copy_tile(buf, stride, x0, y, cw, ch, font.tile(b'?', fg, bg));
        }
        i = end;
    }
}

#[allow(clippy::too_many_arguments)]
fn copy_tile(buf: &mut [u32], stride: usize, x: usize, y: usize, w: usize, h: usize, tile: &[u32]) {
    if x + w > stride {
        return;
    }
    for ty in 0..h {
        let offset = (y + ty) * stride + x;
        if offset + w > buf.len() {
            break;
        }
        buf[offset..offset + w].copy_from_slice(&tile[ty * w..(ty + 1) * w]);
    }
}
/// Moves window pixels with a screen-to-screen copy so the OS (and RDP) sees
/// a copy instead of new pixels. Returns false where unsupported; the caller
/// then presents the moved area as damage.
#[cfg(windows)]
fn screen_copy_region(
    _window: &Window,
    hwnd: isize,
    source: kwil_view_model::TabArea,
    target: kwil_view_model::TabArea,
) -> bool {
    use windows_sys::Win32::Graphics::Gdi::{BitBlt, GetDC, ReleaseDC, SRCCOPY};
    unsafe {
        let hwnd = hwnd as windows_sys::Win32::Foundation::HWND;
        let dc = GetDC(hwnd);
        let ok = BitBlt(
            dc,
            target.x as i32,
            target.y as i32,
            source.width as i32,
            source.height as i32,
            dc,
            source.x as i32,
            source.y as i32,
            SRCCOPY,
        ) != 0;
        ReleaseDC(hwnd, dc);
        ok
    }
}

#[cfg(not(windows))]
fn screen_copy_region(
    window: &Window,
    _hwnd: isize,
    source: kwil_view_model::TabArea,
    target: kwil_view_model::TabArea,
) -> bool {
    #[cfg(target_os = "linux")]
    {
        platform::x11_copy(window, source.x, source.width, source.height, source.y, target.y)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (window, source, target);
        false
    }
}

#[cfg(windows)]
fn cloak_and_show(hwnd: isize) {
    use windows_sys::Win32::Graphics::Dwm::{DWMWA_CLOAK, DwmSetWindowAttribute};
    use windows_sys::Win32::UI::WindowsAndMessaging::{SW_SHOWNOACTIVATE, ShowWindow};
    let on: i32 = 1;
    unsafe {
        let hwnd = hwnd as windows_sys::Win32::Foundation::HWND;
        let hr = DwmSetWindowAttribute(hwnd, DWMWA_CLOAK as u32, (&on as *const i32).cast(), 4);
        assert!(hr >= 0, "DWM cloak failed: {hr:#x}; refusing to show a hidden-mode window");
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        self.sync_desktop_window(el);
    }
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.gfx.is_some() {
            return;
        }
        // Drag and drop starts OLE, which costs ~10 ms of window creation;
        // it is registered after the first frame (KW-11).
        let icon = winit::window::Icon::from_rgba(include_bytes!("icon.rgba").to_vec(), 32, 32).ok();
        #[allow(unused_mut)]
        let mut attrs = Window::default_attributes()
            .with_title("Kwil")
            .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 840.0))
            .with_decorations(true)
            .with_transparent(false)
            .with_window_icon(icon.clone())
            .with_visible(false);
        #[cfg(windows)]
        {
            use winit::platform::windows::WindowAttributesExtWindows;
            attrs = attrs.with_drag_and_drop(false).with_taskbar_icon(icon);
            if self.opts.hidden {
                attrs = attrs.with_active(false).with_skip_taskbar(true);
            }
        }
        let window = match el.create_window(attrs) {
            Ok(w) => Rc::new(w),
            Err(e) => {
                self.error = Some(GuiError::Unavailable(e.to_string()));
                el.exit();
                return;
            }
        };
        if let Some(monitor) = window.current_monitor().or_else(|| el.primary_monitor()) {
            let placement =
                window_placement::Placement::centered(window_placement::work(&monitor), None, (1280.0, 840.0));
            window_placement::fit_window(&window, placement);
            self.window_normal = Some(placement);
        }
        if !self.opts.hidden {
            window.set_visible(true);
        }
        window.set_ime_allowed(true);
        let surface = Context::new(window.clone()).and_then(|c| Surface::new(&c, window.clone()));
        let surface = match surface {
            Ok(s) => s,
            Err(e) => {
                self.error = Some(GuiError::Unavailable(e.to_string()));
                el.exit();
                return;
            }
        };
        let hwnd = match raw_window_handle::HasWindowHandle::window_handle(&*window).map(|h| h.as_raw()) {
            #[cfg(windows)]
            Ok(raw_window_handle::RawWindowHandle::Win32(h)) => h.hwnd.get(),
            _ => 0,
        };
        #[cfg(windows)]
        if self.opts.hidden {
            cloak_and_show(hwnd);
        }
        if let Some(font_size) = self.font_size.take() {
            let _ =
                zoom::start_font_rasterization(font_size, self.settings.font.size, window.scale_factor(), self.zoom);
        }
        self.ui = chrome::Metrics::new(window.scale_factor());
        if let Some(ui_font_size) = self.ui_font_size.take() {
            let _ = ui_font_size.try_send(self.ui.text_px());
        }
        match self.font_thread.take().and_then(|t| t.join().ok()).flatten() {
            Some(f) => self.font = Some(f),
            None => {
                self.error = Some(GuiError::NoFont);
                el.exit();
                return;
            }
        }
        // Read in parallel with window creation; chrome text is optional, so a
        // missing face leaves shapes without labels instead of failing startup.
        self.ui_font = self.ui_font_thread.take().and_then(|t| t.join().ok()).flatten();
        let sz = window.inner_size();
        self.size = (sz.width as usize, sz.height as usize);
        self.gfx = Some(Gfx {
            #[cfg(all(windows, feature = "gpu"))]
            gpu_canvas: None,
            native: None,
            window,
            surface,
            hwnd,
            pixels: vec![bg(); self.size.0 * self.size.1],
        });
        self.resize_surface();
        let rows = self.text_rows();
        let d = self.view.set_rows(rows);
        self.apply(d);
        if !self.render() && self.force_exit() {
            el.exit();
            return;
        }
        trace::mark("first frame presented");
        self.start_search();
        let highlight_proxy = self.proxy.clone();
        self.highlighting = kwil_view_model::highlight_session::HighlightSession::new(move || {
            let _ = highlight_proxy.send_event(UserEvent::HighlightsReady);
        })
        .ok();
        if !self.view.hex_active()
            && let Some(highlighting) = &mut self.highlighting
        {
            highlighting.update(&self.view);
        }
        let document_proxy = self.proxy.clone();
        let (stopped_tx, stopped_rx) = std::sync::mpsc::sync_channel(1);
        self.document_stopped = Some(stopped_rx);
        let complete = move |result| {
            if let kwil_view_model::document_worker::DocumentResult::Stopped(result) = &result {
                let _ = stopped_tx.try_send(result.as_ref().map(|_| ()).map_err(ToString::to_string));
            }
            let _ = document_proxy.send_event(UserEvent::Document(Box::new(result)));
        };
        let worker = if self.opts.hidden {
            self.view.start_isolated_document_worker(complete)
        } else {
            self.view.start_document_worker(complete)
        };
        if let Err(error) = worker {
            self.document_stopped = None;
            let damage = self.view.set_notice(format!("File worker: {error}"));
            self.apply(damage);
        }
        let proxy = self.proxy.clone();
        if let Some(gfx) = self.gfx.as_mut() {
            gfx.native = Some(platform::Native::register(gfx.hwnd, move |e| {
                let _ = proxy.send_event(e);
            }));
        }
        self.apply_native_theme();
        (self.on_event)(GuiEvent::RenderPolicyChanged(self.policy.clone()));
        let proxy = self.proxy.clone();
        let hidden = self.opts.hidden;
        std::thread::spawn(move || {
            let root = if hidden {
                std::env::var_os("KWIL_WORKSPACE_ROOT")
                    .map(std::path::PathBuf::from)
                    .filter(|root| root.is_absolute())
                    .ok_or_else(|| "Hidden palette requires an absolute isolated workspace root".to_string())
            } else {
                std::env::current_dir().map_err(|error| error.to_string())
            };
            let _ = proxy.send_event(UserEvent::PaletteRoot(root));
            let result = std::env::current_exe()
                .map_err(|e| e.to_string())
                .and_then(|exe| Settings::path(&exe).map_err(|e| e.to_string()));
            let _ = proxy.send_event(UserEvent::SettingsPath(result));
        });
        #[cfg(feature = "frame-harness")]
        if self.opts.hidden
            && std::env::var_os("KWIL_FRAME_SCRIPT").is_some()
            && let Some(proxy) = self.proxy.native()
        {
            harness::start(proxy);
        }
        if let Some(cb) = self.on_first_frame.take() {
            let p = self.proxy.clone();
            cb(Box::new(move |msg| {
                let _ = p.send_event(UserEvent::Notice(msg));
            }));
        }
        if let Some(callback) = self.on_external_ready.take() {
            callback(self.external_open.take().unwrap());
        }
    }

    fn user_event(&mut self, el: &ActiveEventLoop, ev: UserEvent) {
        let d = match ev {
            UserEvent::WindowPlacementLoaded(result) => {
                self.placement_loaded(result);
                Damage::default()
            }
            UserEvent::WindowPlacementProgress => {
                self.placement_progress();
                Damage::default()
            }
            #[cfg(windows)]
            UserEvent::WindowPlacementTouched => {
                self.window_touched = true;
                Damage::default()
            }
            UserEvent::SearchReady => {
                self.search_ready();
                Damage::default()
            }
            UserEvent::CloseDialog(reply) => {
                self.close_dialog_event(reply);
                if self.may_exit() {
                    el.exit();
                }
                Damage { full: true, status: true, ..Default::default() }
            }

            UserEvent::FileDialog(event) => {
                self.file_dialog_event(event);
                if self.may_exit() {
                    el.exit();
                }
                Damage { full: true, status: true, ..Default::default() }
            }
            UserEvent::HighlightsReady => self.language_highlights_ready(),
            UserEvent::PaletteRoot(root) => {
                if let Ok(root) = &root {
                    self.set_search_root(root.clone());
                }
                match root.and_then(|root| {
                    let proxy = self.proxy.clone();
                    kwil_view_model::palette_session::PaletteSession::new(
                        root,
                        std::sync::Arc::new(move || {
                            let _ = proxy.send_event(UserEvent::PaletteReady);
                        }),
                    )
                    .map_err(|error| error.to_string())
                }) {
                    Ok(palette) => self.palette = Some(palette),
                    Err(error) => {
                        self.view.set_notice(format!("Palette: {error}"));
                    }
                }
                Damage { status: true, ..Default::default() }
            }
            UserEvent::PaletteReady => {
                let mut notice = false;
                if let Some(palette) = &mut self.palette {
                    let changed = palette.worker_ready();
                    if let Some(report) = palette.take_discovery_report()
                        && (report.errors > 0 || report.end != kwil_core_workspace::catalogue::WalkEnd::Complete)
                    {
                        notice = true;
                        self.view.set_notice(format!(
                            "File catalogue: {} files, {} skipped; {:?}",
                            report.files, report.errors, report.end
                        ));
                    }
                    if changed {
                        self.palette_redraw();
                    }
                }
                Damage { status: notice, ..Default::default() }
            }
            UserEvent::LogReady => {
                self.logs_ready();
                return;
            }
            UserEvent::Document(result) => {
                let replacement = match &*result {
                    kwil_view_model::document_worker::DocumentResult::Replaced { key, result, .. } => {
                        Some((*key, result.as_ref().map(|_| ()).map_err(ToString::to_string)))
                    }
                    _ => None,
                };
                let damage = self.view.document_result(*result);
                if let Some((key, result)) = replacement {
                    self.search_replacement_finished(key, result);
                }
                self.sync_file_ui();
                if let Some((document, epoch)) = self.goto_reveal {
                    if document != self.view.active_id() || epoch != self.view.input_epoch() {
                        self.goto_reveal = None;
                    } else if self.view.editor().is_some() {
                        self.goto_reveal = None;
                        self.keep_caret_visible();
                    }
                }
                if self.may_exit() {
                    el.exit();
                }
                damage
            }
            UserEvent::OpenExternal => {
                if self.file_dialogs.exiting() {
                    self.external_inbox.close();
                    return;
                }

                if self.welcome.visible() {
                    return;
                }
                let requests = self.external_inbox.drain();
                if requests.is_empty() {
                    return;
                }
                for request in requests {
                    if let Err(error) = self.view.open_files(request) {
                        self.view.set_notice(format!("Open failed: {error}"));
                    }
                }
                self.sync_file_ui();
                Damage { full: true, status: true, ..Default::default() }
            }
            UserEvent::Notice(msg) => self.view.set_notice(msg),
            UserEvent::GlyphReady(tile, generation) => {
                let Some(font) = self.font.as_mut().filter(|f| f.generation == generation) else {
                    return;
                };
                self.palette_dirty |= self.palette.as_ref().is_some_and(|palette| palette.model().is_open());
                self.palette_dirty |= self.settings_panel.as_ref().is_some_and(|panel| panel.is_open());
                if matches!(tile.key.bg, tabs::ACTIVE | tabs::INACTIVE) {
                    font.accept(tile);
                    self.chrome_dirty = true;
                    Damage::default()
                } else {
                    glyph_damage(font, tile, self.view.rows())
                }
            }
            UserEvent::SessionChanged(detected) => {
                #[cfg(feature = "gpu")]
                let previous = self.policy.remote();
                self.policy.session_changed(detected);
                #[cfg(feature = "gpu")]
                if previous != self.policy.remote()
                    && let Some(gpu) = self.gpu.as_mut()
                {
                    gpu.invalidate();
                }
                self.apply_native_theme();
                (self.on_event)(GuiEvent::RenderPolicyChanged(self.policy.clone()));
                self.start_gpu();
                if self.sync_presenter() {
                    self.apply(Damage { full: true, ..Default::default() });
                }
                #[cfg(feature = "frame-harness")]
                println!(
                    "kwil-frame session_detected={detected} remote={} blink={}",
                    self.policy.remote(),
                    self.policy.cursor_blink_allowed()
                );
                return;
            }
            UserEvent::WelcomeReady => {
                let was_visible = self.welcome.visible();
                if let Some(worker) = &self.welcome_worker {
                    while let Ok(report) = worker.take_report() {
                        match report {
                            welcome::Report::Ready { show, problem } => {
                                self.welcome.ready(show);
                                self.welcome_view.problem = problem;
                            }
                            welcome::Report::Completed(completion) => {
                                if let Err(error) = completion.result {
                                    self.view.set_notice(format!("Setup could not be remembered: {error}"));
                                }
                                if let Some(path) = completion.retained_temporary {
                                    self.view.set_notice(format!("Setup cleanup retained: {}", path.display()));
                                }
                            }
                        }
                    }
                }
                if !was_visible && self.welcome.visible() {
                    self.ime_destination = Some(ime::Destination::Cancelled);
                    self.composing = false;
                    self.drag = None;
                    self.selecting = false;
                }
                Damage { full: true, status: true, ..Default::default() }
            }
            UserEvent::RegistrationReady => {
                self.welcome_view.hits = Default::default();
                if let Some(page) = &mut self.file_types {
                    page.service.worker_ready();
                }
                self.welcome_registration_changed();
                Damage { full: true, status: true, ..Default::default() }
            }
            UserEvent::RegistrationStarted(result) => {
                self.welcome_view.hits = Default::default();
                self.registration_starting = false;
                match result {
                    Ok(mut service) => {
                        service.worker_ready();
                        self.file_types = Some(filetypes_ui::Page { service: *service, ui: Default::default() });
                    }
                    Err(error) => {
                        self.view.set_notice(format!("File-type setup: {error}"));
                        self.welcome_view.problem = Some(error);
                    }
                }
                Damage { full: true, status: true, ..Default::default() }
            }
            UserEvent::SettingsPath(path) => {
                self.registration_path = path.as_ref().ok().cloned();
                if self.welcome_worker.is_none() {
                    let resolved = path.clone();
                    let hidden = self.opts.hidden;
                    let proxy = self.proxy.clone();
                    match welcome::Worker::start(
                        cfg!(debug_assertions),
                        move || {
                            let settings = resolved.map_err(std::io::Error::other)?;
                            let isolated = std::env::var_os("KWIL_WORKSPACE_ROOT");
                            if hidden && isolated.is_none() {
                                return Err(std::io::Error::other("Hidden setup requires an isolated workspace"));
                            }
                            welcome::marker_path(if hidden { isolated.as_deref() } else { None }, &settings)
                        },
                        move || {
                            let _ = proxy.send_event(UserEvent::WelcomeReady);
                        },
                    ) {
                        Ok(worker) => self.welcome_worker = Some(worker),
                        Err(error) => {
                            self.welcome.ready(true);
                            self.welcome_view.problem = Some(error.to_string());
                        }
                    }
                }
                let proxy = self.proxy.clone();
                match path.and_then(|path| {
                    kwil_view_model::settings_session::SettingsSession::start(
                        path,
                        std::sync::Arc::new(move || {
                            let _ = proxy.send_event(UserEvent::SettingsPanelReady);
                        }),
                    )
                    .map_err(|error| error.to_string())
                }) {
                    Ok(panel) => self.settings_panel = Some(panel),
                    Err(error) => {
                        let _ = self.proxy.send_event(UserEvent::SettingsReady(Box::default(), Default::default()));
                        let damage = self.view.set_notice(format!("Settings: {error}"));
                        self.apply(damage);
                    }
                }
                return;
            }
            UserEvent::SettingsPanelReady => {
                let mut open_path = None;
                // Drains the worker; an adopted external reload fences painted clicks.
                self.settings_worker_ready();
                if let Some(panel) = &mut self.settings_panel {
                    if let Some(applied) = panel.take_applied() {
                        let _ = self.proxy.send_event(UserEvent::SettingsReady(
                            Box::new(applied.settings),
                            applied.language_mappings,
                        ));
                    } else if !self.settings_loaded && panel.menu().is_none() && panel.last_error().is_some() {
                        let _ = self.proxy.send_event(UserEvent::SettingsReady(Box::default(), Default::default()));
                    }
                    if self.settings_file_pending && panel.source_exists() && !panel.is_saving() && !panel.is_dirty() {
                        open_path = Some(panel.path().to_path_buf());
                        panel.close();
                        self.settings_file_pending = false;
                    } else if panel.last_error().is_some() {
                        self.settings_file_pending = false;
                    }
                }
                if let Some(path) = open_path {
                    if let Err(error) = self.view.open_path(path) {
                        self.view.set_notice(format!("Open settings: {error}"));
                    }
                    self.sync_file_ui();
                    self.apply(Damage { full: true, status: true, ..Default::default() });
                }
                if self.settings_open() {
                    self.palette_redraw();
                }
                return;
            }
            UserEvent::DiagnosticsReady(platform) => {
                self.about.platform = Some(platform);
                if let Some(d) = &self.desktop_window {
                    d.gfx.window.request_redraw();
                }
                return;
            }
            UserEvent::MarkdownReady(completion) => {
                self.markdown_ready(*completion);
                return;
            }
            UserEvent::SettingsReady(settings, language_mappings) => {
                self.settings_loaded = true;
                self.apply_language_mappings(language_mappings);
                let font_changed =
                    settings.font.family != self.settings.font.family || settings.font.size != self.settings.font.size;
                let tabs_changed = settings.editor.tab_size != self.settings.editor.tab_size;
                let layout_changed = settings.tabs.layout != self.settings.tabs.layout;
                let keymap_changed =
                    settings.keymap != self.settings.keymap || settings.bindings != self.settings.bindings;
                self.settings = *settings;
                self.search_settings_ready();
                self.view.set_editor_settings(&self.settings);
                self.view.set_session_restore(self.settings.files.restore_session);
                if layout_changed {
                    self.drag = None;
                    self.selecting = false;
                    self.chrome_dirty = true;
                    let rows = self.text_rows();
                    let damage = self.view.set_rows(rows);
                    self.apply(damage);
                    self.keep_caret_visible();
                }
                if keymap_changed || self.keys.is_none() {
                    self.keys =
                        kwil_core_commands::keymap::KeymapEngine::new(self.settings.keymap, &self.settings.bindings)
                            .ok();
                }
                if keymap_changed {
                    self.menu_settings_changed();
                }
                self.policy = RenderPolicy::new(self.settings.render.clone(), self.policy.detected());
                self.apply_native_theme();
                (self.on_event)(GuiEvent::RenderPolicyChanged(self.policy.clone()));
                if font_changed || self.font.as_ref().is_some_and(|f| (f.px - self.font_physical_px()).abs() > 0.01) {
                    self.reload_font();
                } else {
                    self.activate_unicode(tabs_changed);
                }
                self.start_gpu();
                if self.sync_presenter() {
                    self.apply(Damage { full: true, ..Default::default() });
                }
                return;
            }
            UserEvent::FontReady(Some(mut font)) => {
                self.font_loading = false;
                if self.font_reload_pending {
                    self.font_reload_pending = false;
                    self.reload_font();
                    return;
                }
                font.tab_size = self.settings.editor.tab_size as usize;
                let proxy = self.proxy.clone();
                font.enable_unicode(self.settings.font.family.clone(), move |tile, generation| {
                    let _ = proxy.send_event(UserEvent::GlyphReady(tile, generation));
                });
                self.font = Some(*font);
                self.chrome_dirty = true;
                let rows = self.text_rows();
                let damage = self.view.set_rows(rows);
                self.keep_caret_visible();
                damage
            }
            UserEvent::FontReady(None) => {
                self.font_loading = false;
                if self.font_reload_pending {
                    self.font_reload_pending = false;
                    self.reload_font();
                    return;
                }
                // Keep the already usable ASCII font, including its own primary
                // family, and enable deferred Unicode even if the requested font
                // never loaded. Repaint Unicode/tab rows using that surviving font.
                self.activate_unicode(true);
                self.view.set_notice("Configured font is unavailable".into())
            }
            UserEvent::UiFontReady(font) => {
                // One job at a time; a scale change during it gets one rerun
                // with the latest scale. A failed load keeps the current face.
                if self.ui_font_jobs.finished() {
                    self.spawn_ui_font();
                }
                match font.filter(|font| (font.px - self.ui.text_px()).abs() < 0.01) {
                    Some(font) => self.ui_font = Some(*font),
                    None => return,
                }
                self.chrome_dirty = true;
                self.menu_dirty = true;
                Damage { status: true, ..Default::default() }
            }
            UserEvent::UiFallbackReady(id, face) => {
                // Bind only to the face that asked; a newer face that missed
                // glyphs meanwhile gets exactly one retry, still one read at a time.
                let (bind, retry) = self.ui_fallback.finished(id, self.ui_font.as_ref().map(font::UiFont::id));
                if let Some(retry) = retry {
                    spawn_ui_fallback(&self.proxy, retry);
                }
                let Some(font) = self.ui_font.as_mut().filter(|_| bind) else {
                    return;
                };
                font.install_fallback(face);
                if let Some(dialog) = &mut self.desktop_window {
                    if let Some(dialog_font) = &mut dialog.font {
                        dialog_font.copy_fallback_from(font);
                    }
                    dialog.gfx.window.request_redraw();
                }
                self.chrome_dirty = true;
                self.menu_dirty = true;
                Damage { status: true, ..Default::default() }
            }
            UserEvent::DroppedFile(path) => {
                if self.file_modal() {
                    return;
                }
                if self.welcome.visible() {
                    let damage = self.view.set_notice("Finish or skip setup before opening a dropped file".into());
                    self.apply(damage);
                    return;
                }
                (self.on_event)(GuiEvent::DroppedFile(path.clone()));
                if let Err(error) = self.view.open_path(path) {
                    self.view.set_notice(format!("Open failed: {error}"));
                }
                self.sync_file_ui();
                Damage { full: true, status: true, ..Default::default() }
            }
            #[cfg(feature = "gpu")]
            UserEvent::GpuReady(result, ms) => {
                if std::env::var_os("KWIL_TRACE_STARTUP").is_some() {
                    eprintln!("kwil-gpu device_init_ms={ms:.3} native_dx12_damage_present={}", cfg!(windows));
                }
                match result {
                    #[allow(unused_mut)]
                    Ok(mut gpu) => {
                        #[cfg(not(windows))]
                        if let Some(gfx) = &self.gfx
                            && let gpu::Presented::Failed(error) = gpu.attach_window(gfx.window.clone())
                        {
                            eprintln!("kwil GPU surface unavailable: {error}");
                            self.apply(Damage { full: true, ..Default::default() });
                            return;
                        }
                        self.gpu = Some(*gpu);
                    }
                    Err(error) => eprintln!("kwil-gpu unavailable: {error}"),
                }
                self.apply(Damage { full: true, ..Default::default() });
                return;
            }
            #[cfg(feature = "gpu")]
            UserEvent::GpuAvailable(result) => {
                #[cfg(windows)]
                if let Some(gpu) = &mut self.gpu {
                    gpu.available(result);
                }
                #[cfg(not(windows))]
                if let Err(error) = result {
                    eprintln!("kwil GPU worker failed; using CPU: {error}");
                    self.gpu = None;
                    self.pending.full = true;
                }
                // Successful asynchronous completion alone never produces an idle frame.
                if cfg!(windows) || self.pending != Damage::default() {
                    self.apply(Damage { full: true, ..Default::default() });
                }
                return;
            }
            #[cfg(feature = "frame-harness")]
            UserEvent::Harness(step) => {
                harness::step(self, el, step);
                return;
            }
        };
        self.apply(d);
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, ev: WindowEvent) {
        // Native file/close dialogs and teardown own input ahead of every GUI
        // overlay, including the separate settings window. Worker receipts still drain.
        if self.file_modal() && desktop_window::owns_input(&ev) {
            if !self.file_dialogs.exiting()
                && self.close_dialog.is_none()
                && matches!(&ev,WindowEvent::KeyboardInput{event,..} if event.state==ElementState::Pressed && matches!(event.logical_key,Key::Named(NamedKey::Escape)))
            {
                self.view.cancel_file_prompt();
                self.sync_file_ui();
                self.apply(Damage { status: true, ..Default::default() });
            }
            return;
        }

        if !self.desktop_dispatch {
            if self.desktop_window.as_ref().is_some_and(|dialog| dialog.gfx.window.id() == _id) {
                self.desktop_window_event(el, _id, ev);
                return;
            }
            if self.desktop_window.is_some() && desktop_window::owns_input(&ev) {
                return;
            }
            if self.gfx.as_ref().is_some_and(|gfx| gfx.window.id() != _id) {
                return;
            }
        }
        if matches!(&ev,WindowEvent::KeyboardInput{event,..} if event.state==ElementState::Pressed)
            || matches!(&ev, WindowEvent::MouseInput { state: ElementState::Pressed, .. })
        {
            self.window_touched = true;
        }
        if self.welcome_input(&ev) {
            return;
        }
        match ev {
            WindowEvent::CloseRequested => self.request_close(el, false),
            WindowEvent::Moved(_) => {
                self.remember_placement();
            }
            WindowEvent::Resized(sz) => {
                self.remember_placement();
                self.close_context();
                // The first Resized repeats the creation size.
                let latest = (sz.width as usize, sz.height as usize);
                if latest == self.pending_resize.unwrap_or(self.size) {
                    return;
                }
                self.close_language();
                self.status_hits.clear();
                self.drag = None;
                self.selecting = false;
                self.pending_resize = Some(latest);
                self.pending.full = true;
                if let Some(gfx) = &self.gfx {
                    self.redraw_requested = true;
                    gfx.window.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                // Distinguish our already-serviced startup request from an OS
                // expose event (X11 needs the retained pixels restored).
                if !std::mem::take(&mut self.redraw_requested) && self.pending == Damage::default() {
                    self.pending.full = true;
                }
                if !self.render() && self.force_exit() {
                    el.exit();
                }
            }
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::CursorMoved { position, .. } => {
                self.pointer = (position.x.max(0.0) as usize, position.y.max(0.0) as usize);
                if self.language_owns_input() {
                    self.language_track();
                    self.update_pointer_cursor();
                    return;
                }
                self.toolbar_track();
                self.context_track();
                self.logs_track();
                if self.menu.state.is_open() {
                    self.menu_track();
                }
                if self.search_panel.is_open() {
                    let hover = search_view::hover(&self.search_view, self.pointer);
                    if hover != self.search_view.hover {
                        self.search_view.hover = hover;
                        self.search_dirty = true;
                        self.request_frame();
                    }
                }
                if self.settings_open() {
                    self.settings_track();
                } else if self.palette_open() {
                    self.palette_track();
                } else if self.save_path.is_some() {
                    self.prompt_track();
                }
                if self.drag.is_none() && !self.selecting && !self.settings_open() && !self.palette_open() {
                    self.update_hover();
                }
                if let Some(drag) = self.drag {
                    let body = self.body_area();
                    let pixel = if drag.vertical {
                        self.pointer.1.saturating_sub(body.y)
                    } else {
                        self.pointer.0.saturating_sub(body.x)
                    };
                    self.scroll_bar(drag.vertical, drag.position(pixel));
                } else if self.selecting {
                    self.place_caret(true);
                }
                if self.drag.is_some() {
                    self.view.checkpoint_session();
                }
                self.update_pointer_cursor();
            }
            WindowEvent::MouseInput { state, button, .. } if self.language_owns_input() => {
                if state == ElementState::Pressed && button == MouseButton::Left {
                    self.language_mouse();
                }
                self.update_pointer_cursor();
            }
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Right, .. } => {
                if self.view.hex_active() && self.body_area().contains(self.pointer.0, self.pointer.1) {
                    self.hex_cancel_input();
                    return;
                }
                self.open_context();
                self.update_pointer_cursor();
            }
            WindowEvent::MouseInput { button, .. }
                if button != MouseButton::Left && self.logs.owns_point(self.pointer) && !self.logs_blocked() => {}
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => {
                if self.file_modal() {
                    return;
                }
                if self.context.is_open() {
                    if state == ElementState::Pressed {
                        self.context_mouse();
                    }
                    return;
                }
                if state == ElementState::Pressed && self.menu.state.is_open() && self.menu_mouse(el) {
                    return;
                }
                if self.logs.owns_body() && !self.logs_blocked() {
                    if state == ElementState::Pressed {
                        self.pointer_down();
                        if self.drag.is_some() {
                            return;
                        }
                        if self.logs_mouse() {
                            return;
                        }
                    } else if self.drag.take().is_some() || self.logs.owns_point(self.pointer) {
                        return;
                    }
                }
                if self.save_path.is_none() && self.search_panel.is_open() {
                    if state == ElementState::Pressed {
                        self.search_mouse();
                    }
                    return;
                }
                if self.settings_open() {
                    if state == ElementState::Pressed {
                        self.settings_mouse();
                    }
                    return;
                }
                if self.palette_open() {
                    if state == ElementState::Pressed {
                        self.palette_mouse();
                    }
                    if self.may_exit() {
                        el.exit();
                    }
                    return;
                }
                if state == ElementState::Pressed {
                    self.menu.alt_tap = false;
                    // A file prompt card is modal: its clicks never reach menus or text.
                    if self.save_path.is_some() && self.prompt_mouse() {
                        return;
                    }
                    if self.menu_mouse(el) {
                        return;
                    }
                    let frame = self.tab_frame();
                    if let Some(action) = frame.geometry.hit(self.pointer.0, self.pointer.1) {
                        let ids = tabs::ids(&self.view);
                        if let Some(action) = chrome::resolve_page(&frame.geometry, action, &ids)
                            && let Err(error) = self.view.tab_action(action, &frame.geometry)
                        {
                            self.view.set_notice(error.to_string());
                        }
                        self.sync_file_ui();
                        self.apply(Damage { full: true, status: true, ..Default::default() });
                        return;
                    }
                    if let Some(&(_, _, command)) =
                        self.status_hits.iter().find(|(rect, _, _)| rect.contains(self.pointer))
                    {
                        self.run_menu_command(Some(el), command);
                        return;
                    }
                    if self.markdown_pointer() {
                        return;
                    }
                    self.pointer_down();
                    let body = self.body_area();
                    if self.drag.is_none()
                        && body.contains(self.pointer.0, self.pointer.1)
                        && self.pointer.0 - body.x < body.width.saturating_sub(self.ui.bar)
                        && self.pointer.1 - body.y < self.bars().0.length
                    {
                        self.selecting = !self.view.hex_active() || !self.view.document_busy(self.view.active_id());
                        self.place_caret(self.mods.shift_key());
                    }
                    self.view.checkpoint_session();
                } else {
                    let dragged = self.drag.take().is_some();
                    self.selecting = false;
                    if dragged {
                        // Repaint the thumb in its resting colour.
                        self.apply(Damage { status: true, ..Default::default() });
                    }
                    self.update_hover();
                }
            }
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Middle, .. } => {
                if self.file_modal() {
                    return;
                }
                // Middle click closes a tab, as in browsers and other editors.
                if self.settings_open()
                    || self.palette_open()
                    || self.search_panel.is_open()
                    || self.menu.state.is_open()
                    || self.save_path.is_some()
                {
                    return;
                }
                let frame = self.tab_frame();
                if let Some(slot) =
                    frame.geometry.slots.iter().find(|slot| slot.area.contains(self.pointer.0, self.pointer.1))
                {
                    if let Err(error) =
                        self.view.tab_action(kwil_view_model::TabAction::Close(slot.document), &frame.geometry)
                    {
                        self.view.set_notice(error.to_string());
                    }
                    self.sync_file_ui();
                    self.apply(Damage { full: true, status: true, ..Default::default() });
                    if self.may_exit() {
                        el.exit();
                    }
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                // Chrome follows the monitor; the editor font keeps its zoom.
                self.monitor_scale_changed(scale_factor);
            }
            WindowEvent::ThemeChanged(_) => self.apply_native_theme(),
            WindowEvent::DroppedFile(path) => {
                if self.file_modal() {
                    return;
                }
                self.user_event(el, UserEvent::DroppedFile(path));
            }
            WindowEvent::Focused(true) => self.user_event(el, UserEvent::SessionChanged(remote::detect())),
            WindowEvent::Occluded(false) => self.apply(Damage { full: true, ..Default::default() }),
            WindowEvent::KeyboardInput { event, .. } => {
                if !self.language_owns_input() && self.save_path.is_none() && self.menu_alt(&event) {
                    return;
                }
                self.key(el, &event);
                self.view.checkpoint_session();
                if self.may_exit() {
                    el.exit();
                }
            }
            WindowEvent::Ime(_) if self.language_owns_input() => {
                self.hex_cancel_input();
                self.ime_destination = Some(ime::Destination::Cancelled);
                self.search_ime = None;
                self.logs_ime = None;
                self.composing = false;
            }
            WindowEvent::Ime(_) if self.file_modal() || self.context.is_open() => {}
            WindowEvent::Ime(Ime::Preedit(text, _)) if self.view.hex_active() && !self.hex_body_blocked() => {
                self.hex_preedit(&text);
            }
            WindowEvent::Ime(Ime::Commit(text))
                if self.hex_ime_pending() || (self.view.hex_active() && !self.hex_body_blocked()) =>
            {
                self.hex_commit(&text);
                self.view.checkpoint_session();
            }
            WindowEvent::Ime(Ime::Preedit(text, _)) => {
                if ime::starts_composition(self.composing, &text) {
                    self.search_ime = self.capture_search_composition();
                    self.logs_ime = self.capture_logs_composition();
                }
                self.ime_preedit(&text);
            }
            WindowEvent::Ime(Ime::Commit(text)) => {
                if let Some(token) = self.logs_ime.take() {
                    self.composing = false;
                    self.ime_destination = None;
                    self.logs_commit(token, &text);
                    return;
                }
                if self.logs.is_open() {
                    self.composing = false;
                    self.ime_destination = Some(ime::Destination::Cancelled);
                    return;
                }
                if let Some(destination) = self
                    .search_ime
                    .take()
                    .or_else(|| if self.ime_destination.is_none() { self.capture_search_composition() } else { None })
                {
                    self.composing = false;
                    self.ime_destination = None;
                    self.search_commit(destination, &text);
                    return;
                }

                self.composing = false;
                let destination = self.ime_destination.take().unwrap_or_else(|| self.ime_capture());
                let (damage, follow_active) = match destination {
                    ime::Destination::Settings { epoch } => {
                        if self.settings_open() && self.settings_epoch == epoch {
                            self.settings_input(kwil_view_model::settings_session::SettingsInput::Text(&text));
                        } else {
                            self.view.set_notice("IME settings input cancelled: its destination changed".into());
                        }
                        (Damage::default(), false)
                    }
                    ime::Destination::Palette { epoch } if self.goto.is_some() => {
                        if self.goto_epoch == epoch {
                            self.palette_input(kwil_view_model::palette_session::PaletteInput::Text(&text));
                        } else {
                            self.view.set_notice("Go to Line composition cancelled: input changed".into());
                        }
                        (Damage { status: true, ..Default::default() }, false)
                    }
                    ime::Destination::Palette { epoch } => {
                        if self
                            .palette
                            .as_ref()
                            .is_some_and(|palette| palette.model().is_open() && palette.input_epoch() == epoch)
                        {
                            self.palette_input(kwil_view_model::palette_session::PaletteInput::Text(&text));
                            (Damage::default(), false)
                        } else {
                            (
                                self.view.set_notice("IME palette input cancelled: the query changed or closed".into()),
                                false,
                            )
                        }
                    }
                    destination => ime::commit(&mut self.view, destination, text, self.started.elapsed()),
                };
                self.sync_file_ui();
                self.apply(damage);
                if follow_active {
                    self.keep_caret_visible();
                }
                self.view.checkpoint_session();
            }
            WindowEvent::Ime(Ime::Disabled) => {
                self.hex_cancel_input();
                self.search_ime = None;
                self.logs_ime = None;
                self.cancel_logs_composition();
                self.close_context();
                self.composing = false;
                self.ime_destination = Some(ime::Destination::Cancelled);
            }
            WindowEvent::Ime(Ime::Enabled) => {
                self.hex_cancel_input();
                self.search_ime = None;
                self.logs_ime = None;
                self.cancel_logs_composition();
                self.close_context();
                ime::reset(&mut self.composing, &mut self.ime_destination);
            }
            WindowEvent::Focused(false) => {
                self.hex_cancel_input();
                self.close_language();
                self.search_ime = None;
                self.logs_ime = None;
                self.cancel_logs_composition();
                self.close_context();
                self.drag = None;
                self.selecting = false;
                self.composing = false;
                self.ime_destination = Some(ime::Destination::Cancelled);
                if let Some(keys) = &mut self.keys {
                    keys.cancel_pending();
                }
                // Native menus close when the window loses focus.
                self.menu.alt_tap = false;
                if self.menu.state.is_open() {
                    self.menu_set(menu::State::Closed, false);
                }
            }
            WindowEvent::CursorLeft { .. } => {
                if self.hover != chrome::Hover::None {
                    self.hover = chrome::Hover::None;
                    self.chrome_dirty = true;
                    self.apply(Damage { status: true, ..Default::default() });
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if self.language_owns_input() {
                    self.wheel.clear();
                    self.language_wheel(delta);
                    return;
                }
                if self.save_path.is_some()
                    || self.context.is_open()
                    || self.menu.state.is_open()
                    || self.logs.is_open()
                    || self.settings_open()
                    || self.search_panel.is_open()
                    || self.palette_open()
                {
                    self.wheel.clear();
                }
                if self.save_path.is_some() {
                    return;
                }
                self.menu.alt_tap = false;
                let up = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y > 0.0,
                    MouseScrollDelta::PixelDelta(point) => point.y > 0.0,
                };
                if self.context.is_open() {
                    return;
                }
                if self.menu_wheel(up) {
                    return;
                }
                if self.logs_wheel(up) {
                    return;
                }
                if self.settings_open() {
                    // Scrolls the list; selection, drafts and reviews stay put.
                    self.settings_wheel(up);
                    return;
                }
                if self.search_panel.is_open() {
                    return;
                }
                if self.palette_open() {
                    let up = match delta {
                        MouseScrollDelta::LineDelta(_, y) => y > 0.0,
                        MouseScrollDelta::PixelDelta(point) => point.y > 0.0,
                    };
                    self.palette_input(if up {
                        kwil_view_model::palette_session::PaletteInput::Previous
                    } else {
                        kwil_view_model::palette_session::PaletteInput::Next
                    });
                    return;
                }
                // A file prompt card is modal: the wheel scrolls its list, not the text.
                if self.save_path.is_some() && self.prompt_wheel(up) {
                    return;
                }
                let (x, y) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (-f64::from(x) * 3.0, -f64::from(y) * 3.0),
                    MouseScrollDelta::PixelDelta(p) => (
                        -p.x / self.font.as_ref().map_or(1, |font| font.cw).max(1) as f64,
                        -p.y / self.ch().max(1) as f64,
                    ),
                };
                let context = [
                    self.view.active_id().0,
                    self.view.input_epoch(),
                    self.ch() as u64,
                    self.font.as_ref().map_or(1, |font| font.cw) as u64,
                    u64::from(self.mods.shift_key()),
                ];
                let (horizontal, vertical) = self.wheel.take(context, x, y);
                if horizontal == 0 && vertical == 0 {
                    return;
                }
                if self.markdown_wheel(vertical) {
                    return;
                }
                if self.view.hex_active() {
                    if horizontal != 0 || self.mods.shift_key() {
                        let delta = if horizontal != 0 { horizontal } else { vertical };
                        self.hex_scroll_bar(false, self.hex.horizontal.saturating_add_signed(delta));
                    } else {
                        self.hex_wheel(vertical);
                    }
                    self.view.checkpoint_session();
                    return;
                }
                if horizontal != 0 || self.mods.shift_key() {
                    let delta = if horizontal != 0 { horizontal } else { vertical };
                    let maximum = self.bars().1.maximum;
                    self.scroll_bar(false, self.horizontal.saturating_add_signed(delta).min(maximum));
                } else {
                    let d = self.view.scroll(vertical);
                    self.apply(d);
                }
                self.view.checkpoint_session();
            }
            _ => {}
        }
    }
}

fn queue_damage(p: &mut Damage, d: Damage, rows: usize, font: Option<&mut Font>) {
    // Waiters must move when the scroll is queued, before a GlyphReady event
    // can consume them. Moving them only at paint loses that interleaving.
    if let Some(font) = font {
        if d.full {
            font.invalidate_rows();
        } else if d.scroll != 0 {
            font.scroll_rows(d.scroll, rows);
        }
    }
    p.full |= d.full;
    p.status |= d.status;
    if !p.full {
        if d.scroll != 0 {
            p.rows = p
                .rows
                .iter()
                .map(|&r| r as i64 - d.scroll)
                .filter(|r| (0..rows as i64).contains(r))
                .map(|r| r as usize)
                .collect();
        }
        p.scroll += d.scroll;
        p.rows.extend(d.rows);
        if p.scroll.unsigned_abs() as usize >= rows {
            p.full = true;
        }
    }
}
fn glyph_damage(font: &mut Font, tile: shaping::Tile, rows: usize) -> Damage {
    let ys = font.accept(tile);
    let ch = font.ch;
    Damage {
        status: ys.iter().any(|y| *y / ch >= rows),
        rows: ys.into_iter().map(|y| y / ch).filter(|r| *r < rows).collect(),
        ..Default::default()
    }
}

impl App {
    fn resize_surface(&mut self) {
        let (w, h) = self.size;
        if let (Some(g), Some(w), Some(h)) = (&mut self.gfx, NonZeroU32::new(w as u32), NonZeroU32::new(h as u32)) {
            let _ = g.surface.resize(w, h);
            g.pixels.resize(self.size.0 * self.size.1, bg());
        }
    }
    fn reload_font(&mut self) {
        if self.font_loading {
            self.font_reload_pending = true;
            return;
        }
        self.font_loading = true;
        let px = self.font_physical_px();
        let proxy = self.proxy.clone();
        let family = self.settings.font.family.clone();
        std::thread::spawn(move || {
            let _ = proxy.send_event(UserEvent::FontReady(Font::load_named(px, &family).map(Box::new)));
        });
    }
    /// Chrome face for a new monitor scale, coalesced like `reload_font`.
    fn reload_ui_font(&mut self) {
        if self.ui_font_jobs.request() {
            self.spawn_ui_font();
        }
    }
    fn spawn_ui_font(&self) {
        let px = self.ui.text_px();
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let _ = proxy.send_event(UserEvent::UiFontReady(font::UiFont::load(px).map(Box::new)));
        });
    }
    fn activate_unicode(&mut self, tabs_changed: bool) {
        self.chrome_dirty = true;
        let proxy = self.proxy.clone();
        if let Some(font) = self.font.as_mut() {
            font.tab_size = self.settings.editor.tab_size as usize;
            font.enable_unicode(self.settings.font.family.clone(), move |tile, generation| {
                let _ = proxy.send_event(UserEvent::GlyphReady(tile, generation));
            });
        }
        self.apply(Damage {
            rows: (0..self.view.rows())
                .filter(|r| {
                    self.view.line(*r).is_some_and(|s| {
                        s.iter().take(64 * 1024).any(|b| !b.is_ascii() || (tabs_changed && *b == b'\t'))
                    })
                })
                .collect(),
            status: !self.view.status_text().is_ascii(),
            ..Default::default()
        });
    }
    fn apply_native_theme(&mut self) {
        let dark = if self.settings.theme.follow_os {
            self.gfx.as_ref().is_none_or(|g| g.window.theme() != Some(winit::window::Theme::Light))
        } else {
            self.settings.theme.name != "light"
        };
        if theme_ui::select(dark) {
            self.pending.full = true;
            self.chrome_dirty = true;
            if let Some(gfx) = &self.gfx {
                self.redraw_requested = true;
                gfx.window.request_redraw();
            }
            if let Some(dialog) = &self.desktop_window {
                dialog.gfx.window.request_redraw();
            }
        }
        if let Some(dialog) = &self.desktop_window {
            platform::decorate_window(dialog.gfx.hwnd, dark, self.policy.remote(), true);
        }
        if let Some(g) = &self.gfx
            && let Some(native) = &g.native
        {
            let dark = if self.settings.theme.follow_os {
                g.window.theme() != Some(winit::window::Theme::Light)
            } else {
                self.settings.theme.name != "light"
            };
            native.theme(dark, self.policy.remote() || !self.settings_loaded);
        }
    }
    fn start_gpu(&mut self) {
        #[cfg(feature = "gpu")]
        if self.settings_loaded
            && !self.gpu_started
            && !self.policy.remote()
            && self.opts.renderer.unwrap_or(self.settings.render.backend)
                != kwil_core_commands::settings::RenderBackend::Cpu
        {
            self.gpu_started = true;
            let proxy = self.proxy.clone();
            std::thread::spawn(move || {
                let start = std::time::Instant::now();
                let result = {
                    let wake = proxy.clone();
                    gpu::Gpu::init_with_wake(move |result| {
                        let _ = wake.send_event(UserEvent::GpuAvailable(result));
                    })
                };
                let _ =
                    proxy.send_event(UserEvent::GpuReady(result.map(Box::new), start.elapsed().as_secs_f64() * 1000.0));
            });
        }
    }
}
