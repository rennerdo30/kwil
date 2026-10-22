//! GUI frontend: window, renderer, damage-only presents.
//!
//! Renderer path B from `docs/decisions/0001-renderer.md`: winit for the
//! window and input, softbuffer as the CPU renderer. Drawing is
//! event-driven (no frame loop): a present happens only after input, a
//! worker result or a window event, and covers only the damaged rects.
//! Scrolling moves existing pixels and draws only the newly exposed rows.
//! The wgpu takeover path (DX12 only on Windows) is M1 work (KW-11).

mod font;

use font::Font;
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
}

struct Gfx {
    window: Rc<Window>,
    surface: Surface<Rc<Window>, Rc<Window>>,
    #[cfg_attr(not(windows), allow(dead_code))]
    hwnd: isize,
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
}

/// Runs the GUI until the window closes. `on_first_frame` runs once the
/// first frame is on screen; it receives a `Notify` that shows a message in
/// the status bar from any thread (used for update notices).
pub fn run(view: View, opts: GuiOptions, on_first_frame: Box<dyn FnOnce(Notify)>) -> Result<(), GuiError> {
    // Font loading runs while the event loop and window are created.
    let font_thread = Some(std::thread::spawn(|| Font::load(15.0)));
    let event_loop =
        EventLoop::<UserEvent>::with_user_event().build().map_err(|e| GuiError::Unavailable(e.to_string()))?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    if let Some(file) = view.file() {
        let p = proxy.clone();
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
        file.index_in_background(threads, move || {
            let _ = p.send_event(UserEvent::IndexDone);
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

    fn render(&mut self) {
        let (w, h) = self.size;
        let (Some(gfx), Some(font)) = (self.gfx.as_mut(), self.font.as_mut()) else { return };
        if w == 0 || h == 0 {
            return;
        }
        let mut buf = match gfx.surface.buffer_mut() {
            Ok(b) => b,
            Err(_) => return,
        };
        let mut p = std::mem::take(&mut self.pending);
        if buf.age() == 0 {
            p.full = true;
        }
        let ch = font.ch;
        let rows = h.saturating_sub(ch) / ch;
        let mut damage: Vec<Rect> = Vec::new();

        if p.full {
            fill(&mut buf, w, Rect { x: 0, y: 0, w, h }, BG);
            for r in 0..rows {
                draw_row(&mut buf, w, font, &self.view, r);
            }
            p.status = true;
            damage.push(Rect { x: 0, y: 0, w, h });
        } else {
            if p.scroll != 0 {
                let d = p.scroll.unsigned_abs() as usize;
                let keep = (rows - d) * ch;
                if p.scroll > 0 {
                    buf.copy_within(d * ch * w..(d * ch + keep) * w, 0);
                    if !screen_copy(gfx.hwnd, w, keep, d * ch, 0) {
                        damage.push(Rect { x: 0, y: 0, w, h: keep });
                    }
                } else {
                    buf.copy_within(0..keep * w, d * ch * w);
                    if !screen_copy(gfx.hwnd, w, keep, 0, d * ch) {
                        damage.push(Rect { x: 0, y: d * ch, w, h: keep });
                    }
                }
            }
            p.rows.sort_unstable();
            p.rows.dedup();
            for &r in p.rows.iter().filter(|&&r| r < rows) {
                draw_row(&mut buf, w, font, &self.view, r);
                damage.push(Rect { x: 0, y: r * ch, w, h: ch });
            }
        }
        if p.status {
            let sr = Rect { x: 0, y: h - ch, w, h: ch };
            fill(&mut buf, w, sr, STATUS_BG);
            let cols = w / font.cw;
            draw_text(
                &mut buf,
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
        let _ = buf.present_with_damage(&rects);
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
    for (i, &b) in text.iter().take(max_cols).enumerate() {
        let x0 = x + i * cw;
        if x0 + cw > stride {
            break;
        }
        if b == b' ' {
            continue; // the background is already filled
        }
        let t = font.tile(b, fg, bg);
        for ty in 0..ch {
            let o = (y + ty) * stride + x0;
            buf[o..o + cw].copy_from_slice(&t[ty * cw..ty * cw + cw]);
        }
    }
}

/// Moves window pixels with a screen-to-screen copy so the OS (and RDP) sees
/// a copy instead of new pixels. Returns false where unsupported; the caller
/// then presents the moved area as damage.
#[cfg(windows)]
fn screen_copy(hwnd: isize, w: usize, h: usize, from_y: usize, to_y: usize) -> bool {
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
fn screen_copy(_hwnd: isize, _w: usize, _h: usize, _from_y: usize, _to_y: usize) -> bool {
    false // KW-11: Wayland/X11 copy paths
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
        let mut attrs = Window::default_attributes()
            .with_title("Kwil")
            .with_inner_size(PhysicalSize::new(1280, 800))
            .with_visible(!self.opts.hidden);
        #[cfg(windows)]
        {
            use winit::platform::windows::WindowAttributesExtWindows;
            attrs = attrs.with_drag_and_drop(false);
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
        self.gfx = Some(Gfx { window, surface, hwnd });
        self.resize_surface();
        let d = self.view.set_rows(self.text_rows());
        self.apply(d);
        self.render();
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
            WindowEvent::RedrawRequested => self.render(),
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
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
        }
    }
}
