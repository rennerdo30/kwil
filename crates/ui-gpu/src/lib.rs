//! GUI frontend: window, renderer, damage-only presents.
//!
//! Renderer path B from `docs/decisions/0001-renderer.md`: winit for the
//! window and input, softbuffer as the CPU renderer. Drawing is
//! event-driven (no frame loop): a present happens only after input, a
//! worker result or a window event, and covers only the damaged rects.
//! Scrolling moves existing pixels and draws only the newly exposed rows.
//! The wgpu takeover path (DX12 only on Windows) is M1 work (KW-11).

mod chrome;
mod clipboard;
mod file_prompt;
mod font;
mod hit;
mod ime;
mod input;
mod menu;
mod palette_ui;
mod scrollbar;
mod settings_ui;
mod tabs;
mod theme_ui;
mod zoom;

use font::Font;
use kwil_view_model::{CommandArgs, CommandStatus, Damage, Notify, View, Workspace};
#[cfg(feature = "gpu")]
mod gpu;
#[cfg(feature = "frame-harness")]
mod harness;
mod platform;
pub mod remote;
mod shaping;
pub mod text;
mod trace;
pub use remote::RenderPolicy;

use kwil_core_commands::settings::Settings;

use softbuffer::{Context, Surface};
use std::num::NonZeroU32;
use std::rc::Rc;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, Ime, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

const BG: u32 = 0x0d1424;
const FG: u32 = 0xd6e4f0;
const GUTTER_BG: u32 = 0x0a101d;
const GUTTER_FG: u32 = 0x5b6b85;
const STATUS_BG: u32 = 0x080d18;
const STATUS_FG: u32 = 0x8fe3ff;
const GUTTER_COLS: usize = 9;
/// Minimum clip reserved by the row painters; the DPI-sized scrollbar
/// (`chrome::Metrics::bar`) is always painted over the body edge afterwards.
const BAR: usize = 14;

#[derive(Debug, Default, Clone, Copy)]
pub struct GuiOptions {
    /// Agent and test runs: the window is DWM-cloaked (composed but never
    /// shown), unfocused and has no taskbar entry (Windows only).
    pub hidden: bool,
}

#[derive(Debug)]
pub enum GuiError {
    /// No display or the windowing system refused; the caller may fall back
    /// to the terminal UI.
    Unavailable(String),
    NoFont,
    /// The window closed, but accepted settings could not be durably drained.
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
    HighlightsReady,
    PaletteRoot(Result<std::path::PathBuf, String>),
    PaletteReady,
    SettingsPath(Result<std::path::PathBuf, String>),
    SettingsPanelReady,
    /// Boxed on the worker that completes it: keeps every queued event small.
    Document(Box<kwil_view_model::document_worker::DocumentResult>),
    OpenExternal,
    Notice(String),
    GlyphReady(shaping::Tile, u64),
    SettingsReady(Box<Settings>),
    FontReady(Option<Box<Font>>),
    UiFontReady(Option<Box<font::UiFont>>),
    /// Wide-coverage face read for the chrome face with this id.
    UiFallbackReady(u64, Option<(Vec<u8>, usize)>),
    SessionChanged(bool),
    DroppedFile(std::path::PathBuf),
    #[cfg(feature = "gpu")]
    GpuReady(Result<Box<gpu::Gpu>, String>, f64),
    #[cfg(feature = "frame-harness")]
    Harness(harness::Step),
}

/// Events for the app integration package. A dropped file is a request, never
/// an implicit replacement of an edited buffer.
#[derive(Clone, Debug)]
pub enum GuiEvent {
    DroppedFile(std::path::PathBuf),
    RenderPolicyChanged(RenderPolicy),
}

struct Gfx {
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
            assert!(input::commit("日本語.txt".into(), &mut prompt, false).is_none());
            workspace.file_prompt_mut().unwrap().text = prompt.unwrap();
            assert!(workspace.file_prompt().unwrap().text.ends_with("日本語.txt"));
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
        assert_eq!(keys.feed_with_selection("界".parse().unwrap()), VimResult::Unbound);
        assert_eq!(
            App::unbound_key(&keys, &Key::Character("界".into()), Some("界"), ModifiersState::empty()),
            UnboundKey::Edit(input::Action::Text("界".into()))
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

struct App {
    highlighting: Option<kwil_view_model::highlight_session::HighlightSession>,
    palette: Option<kwil_view_model::palette_session::PaletteSession>,
    palette_dirty: bool,
    palette_area: Option<Rect>,
    settings_panel: Option<kwil_view_model::settings_session::SettingsSession>,
    settings_epoch: u64,
    settings_file_pending: bool,
    view: Workspace,
    frontend_document: kwil_view_model::SessionDocumentId,
    frontend_epoch: u64,
    opts: GuiOptions,
    proxy: EventLoopProxy<UserEvent>,
    on_first_frame: Option<Box<dyn FnOnce(Notify)>>,
    on_external_ready: Option<Box<dyn FnOnce(ExternalOpen)>>,
    external_open: Option<ExternalOpen>,
    external_inbox: kwil_view_model::document_worker::ExternalOpenInbox,
    font_thread: Option<std::thread::JoinHandle<Option<Font>>>,
    font_size: Option<std::sync::mpsc::SyncSender<f32>>,
    font: Option<Font>,
    gfx: Option<Gfx>,
    size: (usize, usize),
    mods: ModifiersState,
    pending: Damage,
    error: Option<GuiError>,
    horizontal: u64,
    pointer: (usize, usize),
    drag: Option<scrollbar::Drag>,
    started: std::time::Instant,
    composing: bool,
    ime_destination: Option<ime::Destination>,
    selecting: bool,
    close_pending: bool,
    save_path: Option<String>,
    keys: Option<kwil_core_commands::keymap::KeymapEngine>,
    settings: Settings,
    zoom: zoom::Zoom,
    policy: RenderPolicy,
    on_event: Box<dyn FnMut(GuiEvent)>,
    #[cfg(feature = "gpu")]
    gpu: Option<gpu::Gpu>,
    #[cfg(feature = "gpu")]
    gpu_started: bool,
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
    let proxy = event_loop.create_proxy();
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
    let mut app = App {
        highlighting: None,
        palette: None,
        palette_dirty: false,
        palette_area: None,
        settings_panel: None,
        settings_epoch: 0,
        settings_file_pending: false,
        view,
        frontend_document,
        frontend_epoch,
        opts,
        proxy,
        on_first_frame: Some(on_first_frame),
        on_external_ready: Some(on_external_ready),
        external_open: Some(external_open),
        external_inbox,
        font_thread,
        font_size: Some(font_size),
        font: None,
        gfx: None,
        size: (0, 0),
        mods: ModifiersState::empty(),
        pending: Damage::default(),
        error: None,
        horizontal,
        pointer: (0, 0),
        drag: None,
        started: std::time::Instant::now(),
        composing: false,
        ime_destination: None,
        selecting: false,
        close_pending: false,
        save_path: None,
        keys: None,
        settings: Settings::default(),
        zoom: zoom::Zoom::default(),
        policy: RenderPolicy::new(Default::default(), remote::detect()),
        on_event,
        #[cfg(feature = "gpu")]
        gpu: None,
        #[cfg(feature = "gpu")]
        gpu_started: false,
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
    let result = event_loop.run_app(&mut app);
    if let Some(panel) = &mut app.settings_panel {
        let shutdown = panel.stop();
        // The interactive event loop has ended. Keep the process alive while
        // the settings worker drains accepted/coalesced changes off that loop.
        std::thread::spawn(move || shutdown.shutdown())
            .join()
            .map_err(|_| GuiError::Settings("settings shutdown worker panicked".into()))?
            .map_err(GuiError::Settings)?;
    }
    if let Some(palette) = &mut app.palette {
        let handles = palette.stop();
        std::thread::spawn(move || {
            for handle in handles {
                let _ = handle.join();
            }
        });
    }
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
        chrome::layout(self.size, &self.ui, self.settings.tabs.layout).body
    }

    fn columns(&self) -> u64 {
        let cw = self.font.as_ref().map_or(8, |f| f.cw);
        (self.body_area().width.saturating_sub(self.ui.bar + GUTTER_COLS * cw) / cw) as u64
    }

    fn bars(&self) -> (scrollbar::Bar, scrollbar::Bar) {
        let body = self.body_area();
        let height = body.height.saturating_sub(self.ui.bar);
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
        tabs::frame(
            &self.view,
            self.size,
            &self.ui,
            self.ch(),
            self.settings.tabs.layout,
            self.ui_font.as_mut(),
            self.hover,
        )
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
            let v = scrollbar::Bar::sized(
                0,
                track,
                self.view.rows() as u64,
                self.view.line_count().unwrap_or(self.view.rows() as u64),
                self.view.top(),
                self.ui.min_thumb,
            );
            return (v.maximum > 0 && v.contains_thumb(y)).then_some(chrome::Hover::VerticalThumb);
        }
        if y >= track && y < track + self.ui.bar {
            let h = self.bars().1;
            return (x < h.length && h.maximum > 0 && h.contains_thumb(x)).then_some(chrome::Hover::HorizontalThumb);
        }
        None
    }

    /// Recomputes the hover target; a change damages only chrome and status.
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
            (v, y, self.view.top(), self.view.rows() as u64)
        } else {
            (h, x, self.horizontal, self.page_bytes())
        };
        if bar.contains_thumb(pixel) {
            self.drag = Some(scrollbar::Drag::new(vertical, bar, pixel, offset));
        } else {
            self.scroll_bar(vertical, bar.page(pixel, offset, page));
        }
    }

    fn monitor_scale_changed(&mut self, scale_factor: f64) {
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

    fn apply(&mut self, d: Damage) {
        if let Some(highlighting) = &mut self.highlighting {
            highlighting.invalidate_painted(&self.view, &d);
            highlighting.update(&self.view);
        }
        if d == Damage::default() && !self.chrome_dirty && !self.palette_dirty && !self.menu_dirty {
            return;
        }
        queue_damage(&mut self.pending, d, self.view.rows(), self.font.as_mut());
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
        if self.settings_key(ev) {
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
            if matches!(ev.logical_key, Key::Named(NamedKey::F3)) {
                self.apply(Damage { full: true, status: true, ..Default::default() });
                return;
            }
            if let Some(command) = command {
                if self.dispatch_settings(command) || self.dispatch_palette(command) || self.dispatch_zoom(command) {
                    return;
                }
                self.dispatch_file(command);
                return;
            }
        }
        if self.save_path.is_some() {
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
                    if let Some(text) = ev.text.as_deref().filter(|text| !text.chars().any(char::is_control)) {
                        self.save_path.as_mut().unwrap().push_str(text);
                    }
                }
            }
            self.update_file_prompt();
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
                use kwil_core_commands::vim::VimResult;
                match keys.feed_with_selection(chord) {
                    VimResult::Pending => return,
                    VimResult::Command(call) => {
                        if self.dispatch_settings(call.id) || self.dispatch_palette(call.id) {
                            return;
                        }
                        if self.dispatch_zoom(call.id) {
                            return;
                        }
                        if self.dispatch_file(call.id) {
                            if self.view.wants_exit() {
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
                                self.request_close(el, false);
                                return;
                            }
                            if call.id == "file.saveAndQuit" {
                                self.close_pending = true;
                                self.save();
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
                        match Self::unbound_key(keys, &ev.logical_key, ev.text.as_deref(), self.mods) {
                            UnboundKey::Quit => {
                                if !ev.repeat {
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
        if let Some(action) = input::translate(&ev.logical_key, ev.text.as_deref(), self.mods, self.composing) {
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
        let (id, text) = match action {
            input::Action::Command(id) => (id, None),
            input::Action::Text(text) => ("edit.insert", Some(text)),
        };
        if self.dispatch_settings(id) || self.dispatch_palette(id) || self.dispatch_zoom(id) || self.dispatch_file(id) {
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

    fn request_close(&mut self, el: &ActiveEventLoop, discard: bool) {
        if let Err(error) = self.view.request_window_close(discard) {
            self.view.set_notice(error.to_string());
        }
        self.sync_file_ui();
        self.apply(Damage { full: true, status: true, ..Default::default() });
        if self.view.wants_exit() {
            el.exit();
        }
    }

    fn save(&mut self) {
        self.dispatch_file("file.save");
    }

    fn place_caret(&mut self, extend: bool) {
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
            editor.set_cursors(vec![kwil_view_model::Cursor { anchor, head: offset, desired_column: None }], 0)
        });
        match result {
            Ok(damage) => {
                let damage = self.view.refresh_after_edit(damage);
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
        if self.pending == Damage::default() && !self.chrome_dirty && !self.palette_dirty && !self.menu_dirty {
            return true;
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
        let (notice, segments) = chrome::status(&self.view, self.zoom.percent());
        let title = chrome::window_title(&self.view);
        let dragging = self.drag.map(|drag| drag.vertical);
        let hover = self.hover;
        let theme = theme_ui::chrome();
        let body = geometry.body;
        let (bw, bh) = (body.width, body.height);
        self.body_pixels.resize(bw.saturating_mul(bh), BG);
        let bars = self.bars();
        let rows = geometry.text.height / ch;
        let palette_open = self.palette_open() || self.settings_open();
        let menu_list = self.menu.dropdown.is_some() || self.menu_area.is_some();
        let spans: Vec<_> = self.highlighting.as_ref().into_iter().flat_map(|host| host.spans(&self.view)).collect();
        let (Some(gfx), Some(font)) = (self.gfx.as_mut(), self.font.as_mut()) else { return false };
        if self.window_title != title {
            gfx.window.set_title(&title);
            self.window_title = title;
        }
        let pixels = &mut gfx.pixels;
        let body_pixels = &mut self.body_pixels;
        let mut p = std::mem::take(&mut self.pending);
        let palette_dirty = std::mem::take(&mut self.palette_dirty);
        let menu_dirty = std::mem::take(&mut self.menu_dirty);
        if (palette_ui::palette_blocks_copy(palette_open, self.palette_area.is_some()) || menu_list) && p.scroll != 0 {
            // A pending document scroll predates opening the overlay. Avoid
            // copying overlay pixels through the native scroll seam.
            p.full = true;
            p.scroll = 0;
        }
        let mut damage = Vec::new();
        #[cfg(feature = "frame-harness")]
        let mut copied = 0u64;
        let body_changed = p.full || p.scroll != 0 || !p.rows.is_empty();
        if p.full {
            font.invalidate_rows();
            pixels.fill(BG);
            body_pixels.fill(BG);
            for row in 0..rows {
                draw_row(body_pixels, bw, font, &self.view, row, self.horizontal, &spans);
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
                if !screen_copy_region(&gfx.window, gfx.hwnd, source, target) {
                    damage.push(tabs::rect(target));
                }
                #[cfg(feature = "frame-harness")]
                {
                    copied = (source.width * source.height) as u64;
                }
            }
            p.rows.sort_unstable();
            p.rows.dedup();
            for &row in p.rows.iter().filter(|&&r| r < rows) {
                draw_row(body_pixels, bw, font, &self.view, row, self.horizontal, &spans);
                draw_editor_marks(body_pixels, bw, font, &self.view, row, self.view.line_offset(row));
                let area = Rect { x: 0, y: row * ch, w: bw, h: ch };
                tabs::composite(body_pixels, bw, pixels, w, body, area);
                damage.push(tabs::translated(area, body));
            }
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
        for (dirty, area) in [(palette_dirty, &mut self.palette_area), (menu_dirty, &mut self.menu_area)] {
            if dirty && let Some(area) = area.take() {
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
        if body_changed || p.status {
            let (vertical, horizontal) = bars;
            let (bar, line, inset) = (self.ui.bar, self.ui.line, self.ui.inset);
            let vr = Rect { x: bw.saturating_sub(bar), y: 0, w: bar.min(bw), h: vertical.length };
            let hr = Rect { x: 0, y: vertical.length, w: bw, h: bar.min(bh.saturating_sub(vertical.length)) };
            let track = chrome::mix(BG, FG, 0x0a);
            fill(body_pixels, bw, vr, track);
            fill(body_pixels, bw, hr, track);
            // Hairlines keep the bars distinct from text and from a signal strip.
            let edge = chrome::mix(BG, FG, 0x1c);
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
                chrome::mix(BG, FG, amount)
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
            for area in [vr, hr] {
                tabs::composite(body_pixels, bw, pixels, w, body, area);
                if !p.full {
                    damage.push(tabs::translated(area, body));
                }
            }
        }
        if let (Some(dropdown), Some(ui_font)) = (self.menu.dropdown.as_ref(), self.ui_font.as_mut()) {
            menu::draw_dropdown(&self.menu, dropdown, &available, pixels, w, ui_font, theme, &self.ui);
            self.menu_area = Some(dropdown.area);
            damage.push(dropdown.area);
        }
        // A glyph miss asks for the wide-coverage face: one read at a time per
        // App, fenced to the face that asked (see `font::FallbackJobs`).
        if let Some(ui_font) = self.ui_font.as_mut()
            && ui_font.take_fallback_request()
            && let Some(id) = self.ui_fallback.request(ui_font.id())
        {
            spawn_ui_fallback(&self.proxy, id);
        }
        if let Some(palette) = self.palette.as_ref().filter(|palette| palette.model().is_open()) {
            self.palette_area = palette_ui::draw(palette.model(), pixels, (w, h), font);
            if let Some(area) = self.palette_area {
                damage.push(area);
            }
        }
        if let Some(panel) = self.settings_panel.as_ref().filter(|panel| panel.is_open()) {
            if panel.menu().is_some() {
                self.palette_area = settings_ui::draw(panel, pixels, (w, h), font);
            } else {
                self.palette_area = settings_ui::loading(panel, pixels, (w, h), font);
            }
            if let Some(area) = self.palette_area {
                damage.push(area);
            }
        }
        let rects: Vec<_> = damage.iter().filter_map(|area| area.to_softbuffer()).collect();
        #[cfg(feature = "gpu")]
        if !self.policy.remote()
            && let Some(gpu) = &mut self.gpu
        {
            gpu.update_body(pixels, (w, h), &damage, p.scroll, ch, rows, Some(tabs::rect(geometry.text)));
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
fn spawn_ui_fallback(proxy: &EventLoopProxy<UserEvent>, id: u64) {
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
    fill(buf, w, Rect { x: 0, y, w: gutter.min(w), h: ch }, GUTTER_BG);
    fill(buf, w, Rect { x: gutter, y, w: w.saturating_sub(gutter), h: ch }, BG);
    if let Some(text) = view.line(r) {
        let num = format!("{:>7} ", view.line_number(r));
        draw_text(buf, w, font, 0, y, num.as_bytes(), GUTTER_COLS, GUTTER_FG, GUTTER_BG);
        draw_text(buf, w, font, gutter, y, text, w.saturating_sub(gutter + BAR) / cw, FG, BG);
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
                    fill(buf, w, Rect { x, y, w: (end - start) * cw, h: ch }, BG);
                    draw_text(
                        buf,
                        w,
                        font,
                        x,
                        y,
                        &text[start..end],
                        end - start,
                        kwil_view_model::highlight_session::syntax_rgb(span.class),
                        BG,
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
                        let color = kwil_view_model::highlight_session::syntax_rgb(span.class);
                        let x = gutter + cell.column * cw;
                        fill(buf, w, Rect { x, y, w: cell.width * cw, h: ch }, BG);
                        draw_text(buf, w, font, x, y, cell.text.as_bytes(), cell.width, color, BG);
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
            fill(buf, w, Rect { x: gutter + column * font.cw, y: row * font.ch, w: 2, h: font.ch }, STATUS_FG);
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
            .with_inner_size(PhysicalSize::new(1280, 800))
            .with_decorations(true)
            .with_transparent(false)
            .with_window_icon(icon.clone())
            .with_visible(!self.opts.hidden);
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
        self.gfx = Some(Gfx { native: None, window, surface, hwnd, pixels: vec![BG; self.size.0 * self.size.1] });
        self.resize_surface();
        let rows = self.text_rows();
        let d = self.view.set_rows(rows);
        self.apply(d);
        if !self.render() {
            el.exit();
            return;
        }
        trace::mark("first frame presented");
        let highlight_proxy = self.proxy.clone();
        self.highlighting = kwil_view_model::highlight_session::HighlightSession::new(move || {
            let _ = highlight_proxy.send_event(UserEvent::HighlightsReady);
        })
        .ok();
        if let Some(highlighting) = &mut self.highlighting {
            highlighting.update(&self.view);
        }
        let document_proxy = self.proxy.clone();
        let complete = move |result| {
            let _ = document_proxy.send_event(UserEvent::Document(Box::new(result)));
        };
        let worker = if self.opts.hidden {
            self.view.start_isolated_document_worker(complete)
        } else {
            self.view.start_document_worker(complete)
        };
        if let Err(error) = worker {
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
        if self.opts.hidden && std::env::var_os("KWIL_FRAME_SCRIPT").is_some() {
            harness::start(self.proxy.clone());
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
            UserEvent::HighlightsReady => {
                let rows = self.highlighting.as_mut().map_or_else(Vec::new, |host| host.worker_ready(&self.view));
                Damage { rows, ..Default::default() }
            }
            UserEvent::PaletteRoot(root) => {
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
            UserEvent::Document(result) => {
                let damage = self.view.document_result(*result);
                self.sync_file_ui();
                if self.view.wants_exit() {
                    el.exit();
                }
                damage
            }
            UserEvent::OpenExternal => {
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
                let Some(font) = self.font.as_mut().filter(|f| f.generation == generation) else { return };
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
                #[cfg(feature = "frame-harness")]
                println!(
                    "kwil-frame session_detected={detected} remote={} blink={}",
                    self.policy.remote(),
                    self.policy.cursor_blink_allowed()
                );
                return;
            }
            UserEvent::SettingsPath(path) => {
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
                        let _ = self.proxy.send_event(UserEvent::SettingsReady(Box::default()));
                        let damage = self.view.set_notice(format!("Settings: {error}"));
                        self.apply(damage);
                    }
                }
                return;
            }
            UserEvent::SettingsPanelReady => {
                let mut open_path = None;
                if let Some(panel) = &mut self.settings_panel {
                    panel.worker_ready();
                    if let Some(applied) = panel.take_applied() {
                        let _ = self.proxy.send_event(UserEvent::SettingsReady(Box::new(applied.settings)));
                    } else if !self.settings_loaded && panel.menu().is_none() && panel.last_error().is_some() {
                        let _ = self.proxy.send_event(UserEvent::SettingsReady(Box::default()));
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
            UserEvent::SettingsReady(settings) => {
                self.settings_loaded = true;
                let font_changed =
                    settings.font.family != self.settings.font.family || settings.font.size != self.settings.font.size;
                let tabs_changed = settings.editor.tab_size != self.settings.editor.tab_size;
                let layout_changed = settings.tabs.layout != self.settings.tabs.layout;
                let keymap_changed =
                    settings.keymap != self.settings.keymap || settings.bindings != self.settings.bindings;
                self.settings = *settings;
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
                let Some(font) = self.ui_font.as_mut().filter(|_| bind) else { return };
                font.install_fallback(face);
                self.chrome_dirty = true;
                self.menu_dirty = true;
                Damage { status: true, ..Default::default() }
            }
            UserEvent::DroppedFile(path) => {
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
                    eprintln!("kwil-gpu device_init_ms={ms:.3} onscreen_takeover=false reason=no-damage-present-api");
                }
                match result {
                    Ok(gpu) => self.gpu = Some(*gpu),
                    Err(error) => eprintln!("kwil-gpu unavailable: {error}"),
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
        match ev {
            WindowEvent::CloseRequested => self.request_close(el, false),
            WindowEvent::Resized(sz) => {
                // The first Resized repeats the creation size.
                if (sz.width as usize, sz.height as usize) == self.size {
                    return;
                }
                self.drag = None;
                self.selecting = false;
                self.size = (sz.width as usize, sz.height as usize);
                self.resize_surface();
                let rows = self.text_rows();
                let d = self.view.set_rows(rows);
                self.apply(d);
            }
            WindowEvent::RedrawRequested => {
                // Distinguish our already-serviced startup request from an OS
                // expose event (X11 needs the retained pixels restored).
                if !std::mem::take(&mut self.redraw_requested) && self.pending == Damage::default() {
                    self.pending.full = true;
                }
                if !self.render() {
                    el.exit();
                }
            }
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::CursorMoved { position, .. } => {
                self.pointer = (position.x.max(0.0) as usize, position.y.max(0.0) as usize);
                if self.menu.state.is_open() {
                    self.menu_track();
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
                if self.drag.is_some() || self.selecting {
                    self.view.checkpoint_session();
                }
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => {
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
                    if self.view.wants_exit() {
                        el.exit();
                    }
                    return;
                }
                if state == ElementState::Pressed {
                    self.menu.alt_tap = false;
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
                        self.run_menu_command(el, command);
                        return;
                    }
                    self.pointer_down();
                    let body = self.body_area();
                    if self.drag.is_none()
                        && body.contains(self.pointer.0, self.pointer.1)
                        && self.pointer.0 - body.x < body.width.saturating_sub(self.ui.bar)
                        && self.pointer.1 - body.y < self.bars().0.length
                    {
                        self.selecting = true;
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
                // Middle click closes a tab, as in browsers and other editors.
                if self.settings_open() || self.palette_open() || self.menu.state.is_open() {
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
                    if self.view.wants_exit() {
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
                self.user_event(el, UserEvent::DroppedFile(path));
            }
            WindowEvent::Focused(true) => self.user_event(el, UserEvent::SessionChanged(remote::detect())),
            WindowEvent::Occluded(false) => self.apply(Damage { full: true, ..Default::default() }),
            WindowEvent::KeyboardInput { event, .. } => {
                if self.menu_alt(&event) {
                    return;
                }
                self.key(el, &event);
                self.view.checkpoint_session();
                if self.view.wants_exit() {
                    el.exit();
                }
            }
            WindowEvent::Ime(Ime::Preedit(text, _)) => {
                self.ime_preedit(&text);
            }
            WindowEvent::Ime(Ime::Commit(text)) => {
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
                self.composing = false;
                self.ime_destination = Some(ime::Destination::Cancelled);
            }
            WindowEvent::Ime(Ime::Enabled) => ime::reset(&mut self.composing, &mut self.ime_destination),
            WindowEvent::Focused(false) => {
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
                self.menu.alt_tap = false;
                let up = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y > 0.0,
                    MouseScrollDelta::PixelDelta(point) => point.y > 0.0,
                };
                if self.menu_wheel(up) {
                    return;
                }
                if self.settings_open() {
                    let up = match delta {
                        MouseScrollDelta::LineDelta(_, y) => y > 0.0,
                        MouseScrollDelta::PixelDelta(point) => point.y > 0.0,
                    };
                    for _ in 0..3 {
                        self.settings_input(if up {
                            kwil_view_model::settings_session::SettingsInput::Previous
                        } else {
                            kwil_view_model::settings_session::SettingsInput::Next
                        });
                    }
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
                let (horizontal, vertical) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (-(x * 3.0) as i64, -(y * 3.0) as i64),
                    MouseScrollDelta::PixelDelta(p) => {
                        (-(p.x / self.ch() as f64) as i64, -(p.y / self.ch() as f64) as i64)
                    }
                };
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
            g.pixels.resize(self.size.0 * self.size.1, BG);
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
    fn apply_native_theme(&self) {
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
        if self.settings_loaded && !self.gpu_started && !self.policy.remote() {
            self.gpu_started = true;
            let proxy = self.proxy.clone();
            std::thread::spawn(move || {
                let start = std::time::Instant::now();
                let result = gpu::Gpu::init();
                let _ =
                    proxy.send_event(UserEvent::GpuReady(result.map(Box::new), start.elapsed().as_secs_f64() * 1000.0));
            });
        }
    }
}
