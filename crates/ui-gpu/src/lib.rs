//! GUI frontend: window, renderer, damage-only presents.
//!
//! Renderer path B from `docs/decisions/0001-renderer.md`: winit for the
//! window and input, softbuffer as the CPU renderer. Drawing is
//! event-driven (no frame loop): a present happens only after input, a
//! worker result or a window event, and covers only the damaged rects.
//! Scrolling moves existing pixels and draws only the newly exposed rows.
//! The wgpu takeover path (DX12 only on Windows) is M1 work (KW-11).

mod font;
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

use font::Font;
use kwil_core_commands::settings::Settings;
use kwil_view_model::{Damage, Notify, View};
use softbuffer::{Context, Surface};
use std::num::NonZeroU32;
use std::rc::Rc;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, KeyEvent, MouseScrollDelta, WindowEvent};
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
    IndexDone,
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

struct App {
    view: View,
    opts: GuiOptions,
    proxy: EventLoopProxy<UserEvent>,
    on_first_frame: Option<Box<dyn FnOnce(Notify)>>,
    font_thread: Option<std::thread::JoinHandle<Option<Font>>>,
    font: Option<Font>,
    gfx: Option<Gfx>,
    size: (usize, usize),
    mods: ModifiersState,
    pending: Damage,
    error: Option<GuiError>,
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
    trace::mark("gui entry");
    // Font loading runs while the event loop and window are created.
    let font_thread = Some(std::thread::spawn(|| Font::load(14.0)));
    let event_loop =
        EventLoop::<UserEvent>::with_user_event().build().map_err(|e| GuiError::Unavailable(e.to_string()))?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    if let Some(file) = view.file() {
        let p = proxy.clone();
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
        file.index_in_background(threads, move |result| {
            let event = match result {
                Ok(()) => UserEvent::IndexDone,
                Err(e) => UserEvent::Notice(e.to_string()),
            };
            let _ = p.send_event(event);
        });
    }
    let mut app = App {
        view,
        opts,
        proxy,
        on_first_frame: Some(on_first_frame),
        font_thread,
        font: None,
        gfx: None,
        size: (0, 0),
        mods: ModifiersState::empty(),
        pending: Damage::default(),
        error: None,
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
        self.size.1.saturating_sub(self.ch()) / self.ch()
    }

    fn apply(&mut self, d: Damage) {
        if d == Damage::default() {
            return;
        }
        let p = &mut self.pending;
        p.full |= d.full;
        p.status |= d.status;
        if !p.full {
            // Rows already queued move with a further scroll.
            if d.scroll != 0 {
                let rows = self.view.rows() as i64;
                p.rows = p
                    .rows
                    .iter()
                    .map(|&r| r as i64 - d.scroll)
                    .filter(|r| (0..rows).contains(r))
                    .map(|r| r as usize)
                    .collect();
            }
            p.scroll += d.scroll;
            p.rows.extend(d.rows);
            if p.scroll.unsigned_abs() as usize >= self.view.rows() {
                p.full = true;
            }
        }
        if let Some(g) = &self.gfx {
            self.redraw_requested = true;
            g.window.request_redraw();
        }
    }

    fn key(&mut self, ev: &KeyEvent) {
        if ev.state != ElementState::Pressed {
            return;
        }
        let page = self.view.rows().max(1) as i64;
        let ctrl = self.mods.control_key();
        let d = match &ev.logical_key {
            Key::Named(NamedKey::ArrowDown) => self.view.scroll(1),
            Key::Named(NamedKey::ArrowUp) => self.view.scroll(-1),
            Key::Named(NamedKey::PageDown) => self.view.scroll(page),
            Key::Named(NamedKey::PageUp) => self.view.scroll(-page),
            Key::Named(NamedKey::End) if ctrl => self.view.scroll_to_end(),
            Key::Named(NamedKey::Home) if ctrl => self.view.scroll_to_start(),
            _ => return,
        };
        self.apply(d);
    }

    fn render(&mut self) -> bool {
        if self.pending == Damage::default() {
            return true;
        }
        let (w, h) = self.size;
        let (Some(gfx), Some(font)) = (self.gfx.as_mut(), self.font.as_mut()) else { return false };
        if w == 0 || h == 0 {
            return true;
        }
        let buf = &mut gfx.pixels;
        let mut p = std::mem::take(&mut self.pending);
        let ch = font.ch;
        let rows = h.saturating_sub(ch) / ch;
        let mut damage: Vec<Rect> = Vec::new();
        #[cfg(feature = "frame-harness")]
        let mut copied = 0u64;

        if p.full {
            font.invalidate_rows();
            fill(buf, w, Rect { x: 0, y: 0, w, h }, BG);
            for r in 0..rows {
                draw_row(buf, w, font, &self.view, r);
            }
            p.status = true;
            damage.push(Rect { x: 0, y: 0, w, h });
        } else {
            if p.scroll != 0 {
                font.scroll_rows(p.scroll, rows);
                let d = p.scroll.unsigned_abs() as usize;
                let keep = (rows - d) * ch;
                if p.scroll > 0 {
                    buf.copy_within(d * ch * w..(d * ch + keep) * w, 0);
                    if !screen_copy(&gfx.window, gfx.hwnd, w, keep, d * ch, 0) {
                        damage.push(Rect { x: 0, y: 0, w, h: keep });
                    }
                    #[cfg(feature = "frame-harness")]
                    {
                        copied = (w * keep) as u64;
                    }
                } else {
                    buf.copy_within(0..keep * w, d * ch * w);
                    if !screen_copy(&gfx.window, gfx.hwnd, w, keep, 0, d * ch) {
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
                draw_row(buf, w, font, &self.view, r);
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

fn draw_row(buf: &mut [u32], w: usize, font: &mut Font, view: &View, r: usize) {
    let (cw, ch) = (font.cw, font.ch);
    let y = r * ch;
    let gutter = GUTTER_COLS * cw;
    fill(buf, w, Rect { x: 0, y, w: gutter.min(w), h: ch }, GUTTER_BG);
    fill(buf, w, Rect { x: gutter, y, w: w.saturating_sub(gutter), h: ch }, BG);
    if let Some(text) = view.line(r) {
        let num = format!("{:>7} ", view.line_number(r));
        draw_text(buf, w, font, 0, y, num.as_bytes(), GUTTER_COLS, GUTTER_FG, GUTTER_BG);
        draw_text(buf, w, font, gutter, y, text, w.saturating_sub(gutter) / cw, FG, BG);
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
        #[allow(unused_mut)]
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
        let d = self.view.set_rows(self.text_rows());
        self.apply(d);
        if !self.render() {
            el.exit();
            return;
        }
        trace::mark("first frame presented");
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
    }

    fn user_event(&mut self, _el: &ActiveEventLoop, ev: UserEvent) {
        let d = match ev {
            UserEvent::IndexDone => self.view.index_ready(),
            UserEvent::Notice(msg) => self.view.set_notice(msg),
            UserEvent::GlyphReady(tile, generation) => {
                let Some(font) = self.font.as_mut().filter(|f| f.generation == generation) else { return };
                let rows = font.accept(tile);
                let ch = font.ch;
                Damage {
                    status: rows.iter().any(|y| *y / ch >= self.view.rows()),
                    rows: rows.into_iter().map(|y| y / ch).filter(|r| *r < self.view.rows()).collect(),
                    ..Default::default()
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
            UserEvent::SettingsReady(settings) => {
                self.settings_loaded = true;
                let font_changed =
                    settings.font.family != self.settings.font.family || settings.font.size != self.settings.font.size;
                let tabs_changed = settings.editor.tab_size != self.settings.editor.tab_size;
                self.settings = *settings;
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
                self.view.set_rows(self.text_rows())
            }
            UserEvent::FontReady(None) => {
                self.font_loading = false;
                self.view.set_notice("Configured font is unavailable".into())
            }
            UserEvent::DroppedFile(path) => {
                (self.on_event)(GuiEvent::DroppedFile(path));
                return;
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
                harness::step(self, _el, step);
                return;
            }
        };
        self.apply(d);
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, ev: WindowEvent) {
        match ev {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::Resized(sz) => {
                // The first Resized repeats the creation size.
                if (sz.width as usize, sz.height as usize) == self.size {
                    return;
                }
                self.size = (sz.width as usize, sz.height as usize);
                self.resize_surface();
                let d = self.view.set_rows(self.text_rows());
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
            WindowEvent::ScaleFactorChanged { .. } => self.reload_font(),
            WindowEvent::ThemeChanged(_) => self.apply_native_theme(),
            WindowEvent::DroppedFile(path) => {
                self.user_event(el, UserEvent::DroppedFile(path));
            }
            WindowEvent::Focused(true) => self.user_event(el, UserEvent::SessionChanged(remote::detect())),
            WindowEvent::Occluded(false) => self.apply(Damage { full: true, ..Default::default() }),
            WindowEvent::KeyboardInput { event, .. } => {
                if self.mods.control_key()
                    && matches!(&event.logical_key, Key::Character(c) if c.eq_ignore_ascii_case("q"))
                {
                    el.exit();
                } else {
                    self.key(&event);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => -(y * 3.0) as i64,
                    MouseScrollDelta::PixelDelta(p) => -(p.y / self.ch() as f64) as i64,
                };
                let d = self.view.scroll(lines);
                self.apply(d);
            }
            _ => {}
        }
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
