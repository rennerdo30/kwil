//! GUI frontend: window, renderer, damage-only presents.
//!
//! Renderer path B from `docs/decisions/0001-renderer.md`: winit for the
//! window and input, softbuffer as the CPU renderer. Drawing is
//! event-driven (no frame loop): a present happens only after input, a
//! worker result or a window event, and covers only the damaged rects.
//! Scrolling moves existing pixels and draws only the newly exposed rows.
//! The wgpu takeover path (DX12 only on Windows) is M1 work (KW-11).

mod clipboard;
mod file_prompt;
mod font;
mod hit;
mod input;
mod scrollbar;

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
}

impl std::fmt::Display for GuiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GuiError::Unavailable(e) => write!(f, "no window available: {e}"),
            GuiError::NoFont => write!(f, "no monospace font found"),
        }
    }
}

enum UserEvent {
    Document(kwil_view_model::document_worker::DocumentResult),
    OpenExternal,
    Notice(String),
    GlyphReady(shaping::Tile, u64),
    SettingsReady(Box<Settings>),
    FontReady(Option<Box<Font>>),
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
    view: Workspace,
    opts: GuiOptions,
    proxy: EventLoopProxy<UserEvent>,
    on_first_frame: Option<Box<dyn FnOnce(Notify)>>,
    on_external_ready: Option<Box<dyn FnOnce(ExternalOpen)>>,
    external_open: Option<ExternalOpen>,
    external_inbox: kwil_view_model::document_worker::ExternalOpenInbox,
    font_thread: Option<std::thread::JoinHandle<Option<Font>>>,
    font: Option<Font>,
    gfx: Option<Gfx>,
    size: (usize, usize),
    mods: ModifiersState,
    pending: Damage,
    error: Option<GuiError>,
    horizontal: u64,
    pointer: (usize, usize),
    drag: Option<(bool, usize)>,
    started: std::time::Instant,
    composing: bool,
    selecting: bool,
    close_pending: bool,
    save_path: Option<String>,
    keys: Option<kwil_core_commands::keymap::KeymapEngine>,
    settings: Settings,
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
    let font_thread = Some(std::thread::spawn(|| Font::load(14.0)));
    let event_loop =
        EventLoop::<UserEvent>::with_user_event().build().map_err(|e| GuiError::Unavailable(e.to_string()))?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    let open_proxy = proxy.clone();
    let (external_open, external_inbox) = kwil_view_model::document_worker::external_open_channel(move || {
        open_proxy
            .send_event(UserEvent::OpenExternal)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "GUI closed"))
    });
    let view = Workspace::new(view).map_err(|e| GuiError::Unavailable(e.to_string()))?;
    let mut app = App {
        view,
        opts,
        proxy,
        on_first_frame: Some(on_first_frame),
        on_external_ready: Some(on_external_ready),
        external_open: Some(external_open),
        external_inbox,
        font_thread,
        font: None,
        gfx: None,
        size: (0, 0),
        mods: ModifiersState::empty(),
        pending: Damage::default(),
        error: None,
        horizontal: 0,
        pointer: (0, 0),
        drag: None,
        started: std::time::Instant::now(),
        composing: false,
        selecting: false,
        close_pending: false,
        save_path: None,
        keys: None,
        settings: Settings::default(),
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
        #[cfg(feature = "frame-harness")]
        metrics: Default::default(),
    };
    event_loop.run_app(&mut app).map_err(|e| GuiError::Unavailable(e.to_string()))?;
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
        self.size.1.saturating_sub(self.ch() + BAR) / self.ch()
    }

    fn columns(&self) -> u64 {
        let cw = self.font.as_ref().map_or(8, |f| f.cw);
        (self.size.0.saturating_sub(BAR + GUTTER_COLS * cw) / cw) as u64
    }

    fn bars(&self) -> (scrollbar::Bar, scrollbar::Bar) {
        let height = self.size.1.saturating_sub(self.ch() + BAR);
        let (page, total) =
            horizontal_extent(&self.view, self.columns() as usize, self.settings.editor.tab_size as usize);
        (
            scrollbar::Bar::new(
                0,
                height,
                self.view.rows() as u64,
                self.view.line_count().unwrap_or(self.view.rows() as u64),
                self.view.top(),
            ),
            scrollbar::Bar::new(0, self.size.0.saturating_sub(BAR), page, total, self.horizontal),
        )
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
        let (x, y) = self.pointer;
        let (v, h) = self.bars();
        let vertical = x >= self.size.0.saturating_sub(BAR) && y < v.length;
        let horizontal = y >= v.length && y < v.length + BAR && x < h.length;
        if !vertical && !horizontal {
            return;
        }
        let (bar, pixel, offset, page) = if vertical {
            (v, y, self.view.top(), self.view.rows() as u64)
        } else {
            (h, x, self.horizontal, self.page_bytes())
        };
        if bar.contains_thumb(pixel) {
            self.drag = Some((vertical, pixel - bar.thumb_start));
        } else {
            self.scroll_bar(vertical, bar.page(pixel, offset, page));
        }
    }

    fn apply(&mut self, d: Damage) {
        if d == Damage::default() {
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
        if self.composing && !matches!(ev.logical_key, Key::Named(NamedKey::Escape)) {
            return;
        }
        // A scoped native-window probe can exercise the actual asynchronous
        // save callback without injecting global Ctrl state or taking focus.
        #[cfg(feature = "frame-harness")]
        if self.opts.hidden && matches!(ev.logical_key, Key::Named(NamedKey::F12)) {
            self.save();
            return;
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
        if self.dispatch_file(id) {
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
        let Some(font) = &self.font else {
            return;
        };
        let row = self.view.top() + (self.pointer.1 / font.ch).min(self.view.rows().saturating_sub(1)) as u64;
        let screen_row = row.saturating_sub(self.view.top()) as usize;
        let column = self.pointer.0.saturating_sub(GUTTER_COLS * font.cw) / font.cw;
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
        if self.pending == Damage::default() {
            return true;
        }
        let (w, h) = self.size;
        let bars = self.bars();
        let (Some(gfx), Some(font)) = (self.gfx.as_mut(), self.font.as_mut()) else { return false };
        if w == 0 || h < font.ch {
            return true;
        }
        let buf = &mut gfx.pixels;
        let mut p = std::mem::take(&mut self.pending);
        let ch = font.ch;
        let rows = h.saturating_sub(ch + BAR) / ch;
        let mut damage: Vec<Rect> = Vec::new();
        #[cfg(feature = "frame-harness")]
        let mut copied = 0u64;

        if p.full {
            font.invalidate_rows();
            fill(buf, w, Rect { x: 0, y: 0, w, h }, BG);
            for r in 0..rows {
                draw_row(buf, w, font, &self.view, r, self.horizontal);
                draw_editor_marks(buf, w, font, &self.view, r, self.view.line_offset(r));
            }
            p.status = true;
            damage.push(Rect { x: 0, y: 0, w, h });
        } else {
            if p.scroll != 0 {
                let d = p.scroll.unsigned_abs() as usize;
                let keep = (rows - d) * ch;
                if p.scroll > 0 {
                    buf.copy_within(d * ch * w..(d * ch + keep) * w, 0);
                    if !screen_copy(&gfx.window, gfx.hwnd, w.saturating_sub(BAR), keep, d * ch, 0) {
                        damage.push(Rect { x: 0, y: 0, w, h: keep });
                    }
                    #[cfg(feature = "frame-harness")]
                    {
                        copied = (w * keep) as u64;
                    }
                } else {
                    buf.copy_within(0..keep * w, d * ch * w);
                    if !screen_copy(&gfx.window, gfx.hwnd, w.saturating_sub(BAR), keep, 0, d * ch) {
                        damage.push(Rect { x: 0, y: d * ch, w, h: keep });
                    }
                    #[cfg(feature = "frame-harness")]
                    {
                        copied = (w * keep) as u64;
                    }
                }
            }
            p.rows.sort_unstable();
            p.rows.dedup();
            for &r in p.rows.iter().filter(|&&r| r < rows) {
                draw_row(buf, w, font, &self.view, r, self.horizontal);
                draw_editor_marks(buf, w, font, &self.view, r, self.view.line_offset(r));
                damage.push(Rect { x: 0, y: r * ch, w, h: ch });
            }
        }
        if p.status {
            let status_height = ch.min(h);
            let sr = Rect { x: 0, y: h - status_height, w, h: status_height };
            fill(buf, w, sr, STATUS_BG);
            let cols = w / font.cw;
            draw_text(
                buf,
                w,
                font,
                font.cw,
                sr.y,
                self.view.status_text().as_bytes(),
                cols.saturating_sub(2),
                STATUS_FG,
                STATUS_BG,
            );
            if !p.full {
                damage.push(sr);
            }
        }
        // Scrollbars do not move with the text. Repaint them after every copy.
        let (v, horizontal) = bars;
        let vr = Rect { x: w.saturating_sub(BAR), y: 0, w: BAR.min(w), h: v.length };
        let hr = Rect { x: 0, y: v.length, w, h: BAR.min(h.saturating_sub(v.length)) };
        fill(buf, w, vr, GUTTER_BG);
        fill(buf, w, hr, GUTTER_BG);
        if v.maximum > 0 {
            fill(
                buf,
                w,
                Rect { x: vr.x + 2.min(vr.w), y: v.thumb_start, w: vr.w.saturating_sub(4), h: v.thumb_length },
                GUTTER_FG,
            );
        }
        if horizontal.maximum > 0 {
            fill(
                buf,
                w,
                Rect {
                    x: horizontal.thumb_start,
                    y: hr.y + 2.min(hr.h),
                    w: horizontal.thumb_length,
                    h: hr.h.saturating_sub(4),
                },
                GUTTER_FG,
            );
        }
        if !p.full {
            damage.extend([vr, hr]);
        }
        let rects: Vec<_> = damage.iter().filter_map(|r| r.to_softbuffer()).collect();
        #[cfg(feature = "gpu")]
        if !self.policy.remote()
            && let Some(gpu) = &mut self.gpu
        {
            gpu.update(buf, (w, h), &damage, p.scroll, ch, rows);
        }
        if !rects.is_empty() {
            let mut target = match gfx.surface.buffer_mut() {
                Ok(target) => target,
                Err(error) => {
                    self.error = Some(GuiError::Unavailable(error.to_string()));
                    return false;
                }
            };
            // Retain a canonical framebuffer: Wayland/macOS may rotate or
            // replace buffers. Age never forces re-layout or glyph repaint.
            if target.len() != buf.len() {
                self.error = Some(GuiError::Unavailable("CPU surface resize failed".into()));
                return false;
            }
            target.copy_from_slice(buf);
            match target.present_with_damage(&rects) {
                Ok(()) => {
                    #[cfg(feature = "frame-harness")]
                    self.metrics.present(p.full, damage.iter().map(|r| (r.w * r.h) as u64).sum(), copied);
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

fn draw_row(buf: &mut [u32], w: usize, font: &mut Font, view: &View, r: usize, _horizontal: u64) {
    let (cw, ch) = (font.cw, font.ch);
    let y = r * ch;
    let gutter = GUTTER_COLS * cw;
    fill(buf, w, Rect { x: 0, y, w: gutter.min(w), h: ch }, GUTTER_BG);
    fill(buf, w, Rect { x: gutter, y, w: w.saturating_sub(gutter), h: ch }, BG);
    if let Some(text) = view.line(r) {
        let num = format!("{:>7} ", view.line_number(r));
        draw_text(buf, w, font, 0, y, num.as_bytes(), GUTTER_COLS, GUTTER_FG, GUTTER_BG);
        draw_text(buf, w, font, gutter, y, text, w.saturating_sub(gutter + BAR) / cw, FG, BG);
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

#[derive(Clone, Copy)]
struct Rect {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

impl Rect {
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
fn screen_copy(_window: &Window, hwnd: isize, w: usize, h: usize, from_y: usize, to_y: usize) -> bool {
    use windows_sys::Win32::Graphics::Gdi::{BitBlt, GetDC, ReleaseDC, SRCCOPY};
    unsafe {
        let hwnd = hwnd as windows_sys::Win32::Foundation::HWND;
        let dc = GetDC(hwnd);
        let ok = BitBlt(dc, 0, to_y as i32, w as i32, h as i32, dc, 0, from_y as i32, SRCCOPY) != 0;
        ReleaseDC(hwnd, dc);
        ok
    }
}

#[cfg(not(windows))]
fn screen_copy(window: &Window, _hwnd: isize, w: usize, h: usize, from_y: usize, to_y: usize) -> bool {
    #[cfg(target_os = "linux")]
    {
        platform::x11_copy(window, w, h, from_y, to_y)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (window, w, h, from_y, to_y);
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
        match self.font_thread.take().and_then(|t| t.join().ok()).flatten() {
            Some(f) => self.font = Some(f),
            None => {
                self.error = Some(GuiError::NoFont);
                el.exit();
                return;
            }
        }
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
        let document_proxy = self.proxy.clone();
        let complete = move |result| {
            let _ = document_proxy.send_event(UserEvent::Document(result));
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
        std::thread::spawn(move || {
            let result = std::env::current_exe()
                .map_err(|e| e.to_string())
                .and_then(|exe| Settings::path(&exe).map_err(|e| e.to_string()))
                .and_then(|path| Settings::load(&path).map_err(|e| e.to_string()));
            match result {
                Ok(report) => {
                    let _ = proxy.send_event(UserEvent::SettingsReady(Box::new(report.settings)));
                }
                Err(message) => {
                    let _ = proxy.send_event(UserEvent::Notice(format!("Settings: {message}")));
                    let _ = proxy.send_event(UserEvent::SettingsReady(Box::default()));
                }
            }
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
            UserEvent::Document(result) => {
                let damage = self.view.document_result(result);
                if let Some(editor) = self.view.editor_mut() {
                    editor.settings = self.settings.clone();
                }
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
                glyph_damage(font, tile, self.view.rows())
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
            UserEvent::SettingsReady(settings) => {
                self.settings_loaded = true;
                let font_changed =
                    settings.font.family != self.settings.font.family || settings.font.size != self.settings.font.size;
                let tabs_changed = settings.editor.tab_size != self.settings.editor.tab_size;
                self.settings = *settings;
                self.keys =
                    kwil_core_commands::keymap::KeymapEngine::new(self.settings.keymap, &self.settings.bindings).ok();
                if let Some(editor) = self.view.editor_mut() {
                    editor.settings = self.settings.clone();
                }
                self.policy = RenderPolicy::new(self.settings.render.clone(), self.policy.detected());
                self.apply_native_theme();
                (self.on_event)(GuiEvent::RenderPolicyChanged(self.policy.clone()));
                let scale = self.gfx.as_ref().map_or(1.0, |g| g.window.scale_factor()) as f32;
                if font_changed
                    || self.font.as_ref().is_some_and(|f| (f.px - self.settings.font.size * scale).abs() > 0.01)
                {
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
                let rows = self.text_rows();
                self.view.set_rows(rows)
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
                if let Some((vertical, grab)) = self.drag {
                    let (v, h) = self.bars();
                    let (bar, pixel) = if vertical { (v, self.pointer.1) } else { (h, self.pointer.0) };
                    self.scroll_bar(vertical, bar.drag(pixel, grab));
                } else if self.selecting {
                    self.place_caret(true);
                }
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => {
                if state == ElementState::Pressed {
                    self.pointer_down();
                    if self.drag.is_none()
                        && self.pointer.0 < self.size.0.saturating_sub(BAR)
                        && self.pointer.1 < self.bars().0.length
                    {
                        self.selecting = true;
                        self.place_caret(self.mods.shift_key());
                    }
                } else {
                    self.drag = None;
                    self.selecting = false;
                }
            }
            WindowEvent::ScaleFactorChanged { .. } => self.reload_font(),
            WindowEvent::ThemeChanged(_) => self.apply_native_theme(),
            WindowEvent::DroppedFile(path) => {
                self.user_event(el, UserEvent::DroppedFile(path));
            }
            WindowEvent::Focused(true) => self.user_event(el, UserEvent::SessionChanged(remote::detect())),
            WindowEvent::Occluded(false) => self.apply(Damage { full: true, ..Default::default() }),
            WindowEvent::KeyboardInput { event, .. } => {
                self.key(el, &event);
                self.view.checkpoint_session();
            }
            WindowEvent::Ime(Ime::Preedit(text, _)) => self.composing = !text.is_empty(),
            WindowEvent::Ime(Ime::Commit(text)) => {
                self.composing = false;
                let insert_mode = self.keys.as_ref().is_none_or(|keys| keys.mode() == kwil_core_commands::Mode::Insert);
                if let Some(action) = input::commit(text, &mut self.save_path, insert_mode) {
                    self.edit(action);
                } else if self.save_path.is_some() {
                    self.update_file_prompt();
                }
                self.view.checkpoint_session();
            }
            WindowEvent::Ime(Ime::Disabled) => self.composing = false,
            WindowEvent::Focused(false) => {
                self.drag = None;
                self.selecting = false;
                self.composing = false;
                if let Some(keys) = &mut self.keys {
                    keys.cancel_pending();
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
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
        let px = self.settings.font.size * self.gfx.as_ref().map_or(1.0, |g| g.window.scale_factor()) as f32;
        let proxy = self.proxy.clone();
        let family = self.settings.font.family.clone();
        std::thread::spawn(move || {
            let _ = proxy.send_event(UserEvent::FontReady(Font::load_named(px, &family).map(Box::new)));
        });
    }
    fn activate_unicode(&mut self, tabs_changed: bool) {
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
