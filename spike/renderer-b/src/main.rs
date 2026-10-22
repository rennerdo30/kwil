//! M0 renderer B: winit + softbuffer (CPU), event-driven, damage-only
//! presents, scrolling by copying pixels (spec §6).
//!
//!   renderer-b <file> [--script] [--capture <dir>]

use kwil_core_text::TextFile;
use softbuffer::{Context, Surface};
use spiketrace::{Phase, Stats, Step};
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, KeyEvent, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

const BG: u32 = 0x1e1e2e;
const FG: u32 = 0xcdd6f4;
const GUTTER_BG: u32 = 0x181825;
const GUTTER_FG: u32 = 0x6c7086;
const STATUS_BG: u32 = 0x11111b;
const STATUS_FG: u32 = 0xa6adc8;
const PAL_BG: u32 = 0x313244;
const PAL_SEL: u32 = 0x45475a;
const CURSOR: u32 = 0xf5e0dc;
const PAL_ROWS: usize = 12;

enum UserEvent {
    Script(Step),
    IndexDone,
    #[allow(dead_code)]
    GpuReady(f64),
}

/// Monospace ASCII fast path: glyphs rasterized once, then cached as
/// ready-to-copy cell tiles per (byte, fg, bg).
struct Font {
    cw: usize,
    ch: usize,
    glyphs: Vec<(fontdue::Metrics, Vec<u8>)>,
    baseline: i32,
    tiles: HashMap<(u8, u32, u32), Vec<u32>>,
}

impl Font {
    fn load(px: f32) -> Font {
        let data = std::fs::read(r"C:\Windows\Fonts\consola.ttf").expect("Consolas font");
        let font = fontdue::Font::from_bytes(data, fontdue::FontSettings::default()).unwrap();
        let lm = font.horizontal_line_metrics(px).unwrap();
        let glyphs: Vec<_> = (32u8..127).map(|c| font.rasterize(c as char, px)).collect();
        let cw = glyphs[(b'M' - 32) as usize].0.advance_width.round() as usize;
        let ch = (lm.ascent - lm.descent + lm.line_gap).ceil() as usize + 2;
        Font { cw, ch, glyphs, baseline: lm.ascent.ceil() as i32 + 1, tiles: HashMap::new() }
    }

    fn tile(&mut self, c: u8, fg: u32, bg: u32) -> &[u32] {
        let c = if (32..127).contains(&c) { c } else { b'?' };
        let (cw, ch, baseline) = (self.cw, self.ch, self.baseline);
        let glyphs = &self.glyphs;
        self.tiles.entry((c, fg, bg)).or_insert_with(|| {
            let mut t = vec![bg; cw * ch];
            let (m, bmp) = &glyphs[(c - 32) as usize];
            let top = baseline - m.height as i32 - m.ymin;
            for gy in 0..m.height {
                for gx in 0..m.width {
                    let (x, y) = (m.xmin + gx as i32, top + gy as i32);
                    if x >= 0 && y >= 0 && (x as usize) < cw && (y as usize) < ch {
                        t[y as usize * cw + x as usize] = blend(bg, fg, bmp[gy * m.width + gx]);
                    }
                }
            }
            t
        })
    }
}

fn blend(bg: u32, fg: u32, a: u8) -> u32 {
    let a = a as u32;
    let ch = |s: u32| {
        let (b, f) = ((bg >> s) & 0xff, (fg >> s) & 0xff);
        ((b * (255 - a) + f * a) / 255) << s
    };
    ch(16) | ch(8) | ch(0)
}

#[derive(Clone, Copy)]
struct R {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

impl R {
    fn sb(self) -> Option<softbuffer::Rect> {
        Some(softbuffer::Rect {
            x: self.x as u32,
            y: self.y as u32,
            width: NonZeroU32::new(self.w as u32)?,
            height: NonZeroU32::new(self.h as u32)?,
        })
    }
}

/// What the next present has to do.
#[derive(Default)]
struct Pending {
    full: bool,
    rows: Vec<usize>,
    status: bool,
    palette: bool,
    /// Scroll the text area up (positive) or down by this many rows first.
    scroll_rows: i64,
}

struct Gfx {
    window: Rc<Window>,
    surface: Surface<Rc<Window>, Rc<Window>>,
    hwnd: isize,
}

struct App {
    file: TextFile,
    proxy: EventLoopProxy<UserEvent>,
    stats: Stats,
    script: bool,
    capture: Option<PathBuf>,
    font: Option<Font>,
    font_thread: Option<std::thread::JoinHandle<Font>>,
    gfx: Option<Gfx>,
    size: (usize, usize),
    top: u64,
    lines: Vec<Vec<u8>>,
    cursor: (u64, usize),
    edit: Option<(u64, Vec<u8>)>,
    palette: Option<String>,
    commands: Vec<String>,
    mods: ModifiersState,
    pending: Pending,
}

impl App {
    fn font(&self) -> &Font {
        self.font.as_ref().unwrap()
    }

    fn rows(&self) -> usize {
        (self.size.1.saturating_sub(self.font().ch)) / self.font().ch
    }

    fn line_count(&self) -> u64 {
        self.file.index().map_or(0, |i| i.line_count())
    }

    fn gutter_cols(&self) -> usize {
        9
    }

    fn reload_lines(&mut self) {
        let rows = self.rows();
        let lines = match self.file.lines(self.top, rows) {
            Some(l) => l,
            None => self.file.first_lines(rows),
        };
        self.lines = lines.into_iter().map(|l| l.to_vec()).collect();
    }

    fn line_bytes(&self, row: usize) -> &[u8] {
        let n = self.top + row as u64;
        match &self.edit {
            Some((l, b)) if *l == n => b,
            _ => self.lines.get(row).map_or(&[][..], |v| v),
        }
    }

    fn request(&self) {
        if let Some(g) = &self.gfx {
            g.window.request_redraw();
        }
    }

    fn palette_rect(&self) -> R {
        let (w, _) = self.size;
        let (cw, ch) = (self.font().cw, self.font().ch);
        let pw = (70 * cw).min(w.saturating_sub(40));
        R { x: (w - pw) / 2, y: 2 * ch, w: pw, h: (1 + PAL_ROWS) * ch + 8 }
    }

    fn rows_under(&self, r: R) -> std::ops::Range<usize> {
        let ch = self.font().ch;
        (r.y / ch)..((r.y + r.h).div_ceil(ch)).min(self.rows())
    }

    // --- input -------------------------------------------------------------

    fn scroll(&mut self, delta: i64) {
        if self.file.index().is_none() {
            return;
        }
        let max_top = self.line_count().saturating_sub(self.rows() as u64);
        let new_top = (self.top as i64 + delta).clamp(0, max_top as i64) as u64;
        let d = new_top as i64 - self.top as i64;
        if d == 0 {
            return;
        }
        self.top = new_top;
        self.reload_lines();
        if self.palette.is_some() || d.unsigned_abs() as usize >= self.rows() {
            self.pending.full = true;
        } else {
            self.pending.scroll_rows += d;
        }
        self.pending.status = true;
        self.request();
    }

    fn type_char(&mut self, c: char) {
        if let Some(q) = &mut self.palette {
            q.push(c);
            self.pending.palette = true;
        } else {
            let (line, col) = self.cursor;
            let row = line.saturating_sub(self.top) as usize;
            if self.edit.as_ref().is_none_or(|(l, _)| *l != line) {
                self.edit = Some((line, self.line_bytes(row).to_vec()));
            }
            let buf = &mut self.edit.as_mut().unwrap().1;
            let col = col.min(buf.len());
            buf.insert(col, if c.is_ascii() { c as u8 } else { b'?' });
            self.cursor.1 = col + 1;
            self.pending.rows.push(row);
            self.pending.status = true;
        }
        self.request();
    }

    fn open_palette(&mut self) {
        self.palette = Some(String::new());
        self.pending.palette = true;
        self.request();
    }

    fn close_palette(&mut self) {
        if self.palette.take().is_some() {
            let r = self.palette_rect();
            self.pending.rows.extend(self.rows_under(r));
            self.request();
        }
    }

    fn key(&mut self, ev: &KeyEvent) {
        if ev.state != ElementState::Pressed {
            return;
        }
        match &ev.logical_key {
            Key::Character(s) if self.mods.control_key() && self.mods.shift_key() && s.eq_ignore_ascii_case("p") => {
                self.open_palette()
            }
            Key::Named(NamedKey::Escape) => self.close_palette(),
            Key::Named(NamedKey::PageDown) => self.scroll(self.rows() as i64),
            Key::Named(NamedKey::PageUp) => self.scroll(-(self.rows() as i64)),
            Key::Named(NamedKey::ArrowDown) => self.scroll(1),
            Key::Named(NamedKey::ArrowUp) => self.scroll(-1),
            _ => {
                if let Some(t) = &ev.text
                    && !self.mods.control_key()
                {
                    for c in t.chars().filter(|c| !c.is_control()) {
                        self.type_char(c);
                    }
                }
            }
        }
    }

    fn step(&mut self, el: &ActiveEventLoop, s: Step) {
        match s {
            Step::Enter(p) => {
                self.capture_for(p);
                self.stats.enter(p);
            }
            Step::TypeChar(c) => self.type_char(c),
            Step::Scroll(n) => self.scroll(n as i64),
            Step::OpenPalette => self.open_palette(),
            Step::ClosePalette => {
                self.capture("3-palette");
                self.close_palette()
            }
            Step::Exit => el.exit(),
        }
    }

    fn capture_for(&mut self, entering: Phase) {
        match entering {
            Phase::Typing => self.capture("1-file"),
            Phase::Palette => {
                self.capture("2-scrolled");
                // Proof that the copy path is exact: repaint fully and compare.
                if self.capture.is_some() {
                    self.pending.full = true;
                    self.render();
                    self.capture("2-scrolled-full");
                }
            }
            _ => {}
        }
    }

    fn capture(&self, name: &str) {
        if let (Some(dir), Some(g)) = (&self.capture, &self.gfx) {
            let _ = std::fs::create_dir_all(dir);
            spiketrace::capture_window_dc_bmp(g.hwnd, &dir.join(format!("b-{name}.bmp"))).unwrap();
        }
    }

    // --- drawing -----------------------------------------------------------

    fn render(&mut self) {
        let t = Instant::now();
        let Some(mut gfx) = self.gfx.take() else { return };
        let (w, h) = self.size;
        if w == 0 || h == 0 {
            self.gfx = Some(gfx);
            return;
        }
        let mut font = self.font.take().unwrap();
        let mut buf = gfx.surface.buffer_mut().unwrap();
        let mut p = std::mem::take(&mut self.pending);
        if buf.age() == 0 {
            p.full = true;
        }
        let (cw, ch) = (font.cw, font.ch);
        let rows = (h.saturating_sub(ch)) / ch;
        let mut damage: Vec<R> = Vec::new();
        let mut copy_px = 0u64;

        if p.full {
            fill(&mut buf, w, R { x: 0, y: 0, w, h }, BG);
            for r in 0..rows {
                self.draw_row(&mut buf, &mut font, r);
            }
            p.status = true;
            p.palette = self.palette.is_some();
            damage.push(R { x: 0, y: 0, w, h });
        } else {
            if p.scroll_rows != 0 {
                let d = p.scroll_rows.unsigned_abs() as usize;
                let keep = (rows - d) * ch;
                if p.scroll_rows > 0 {
                    buf.copy_within(d * ch * w..(d * ch + keep) * w, 0);
                    screen_copy(gfx.hwnd, w, keep, d * ch, 0);
                    p.rows.extend(rows - d..rows);
                } else {
                    buf.copy_within(0..keep * w, d * ch * w);
                    screen_copy(gfx.hwnd, w, keep, 0, d * ch);
                    p.rows.extend(0..d);
                }
                copy_px = (keep * w) as u64;
            }
            p.rows.sort_unstable();
            p.rows.dedup();
            for &r in &p.rows {
                if r < rows {
                    self.draw_row(&mut buf, &mut font, r);
                    damage.push(R { x: 0, y: r * ch, w, h: ch });
                }
            }
        }
        if p.status {
            let sr = R { x: 0, y: h - ch, w, h: ch };
            fill(&mut buf, w, sr, STATUS_BG);
            let text = self.status_text();
            draw_text(&mut buf, w, &mut font, cw, sr.y, text.as_bytes(), w / cw - 2, STATUS_FG, STATUS_BG);
            if !p.full {
                damage.push(sr);
            }
        }
        if p.palette && self.palette.is_some() {
            self.font = Some(font);
            let pr = self.palette_rect();
            font = self.font.take().unwrap();
            self.draw_palette(&mut buf, &mut font, pr);
            if !p.full {
                damage.push(pr);
            }
        }

        let rects: Vec<_> = damage.iter().filter_map(|r| r.sb()).collect();
        let damage_px: u64 = damage.iter().map(|r| (r.w * r.h) as u64).sum();
        buf.present_with_damage(&rects).unwrap();
        self.font = Some(font);
        self.stats.present(damage_px, copy_px, p.full, t);
        self.gfx = Some(gfx);
    }

    fn status_text(&self) -> String {
        let lines = match self.file.index() {
            Some(i) => format!("{} lines", i.line_count()),
            None => {
                let pr = self.file.progress();
                format!("~ indexing {}%", pr.scanned * 100 / pr.total.max(1))
            }
        };
        format!("kwil (renderer B)  |  {lines}  |  Ln {}, Col {}", self.cursor.0 + 1, self.cursor.1 + 1)
    }

    fn draw_row(&self, buf: &mut [u32], font: &mut Font, r: usize) {
        let w = self.size.0;
        let (cw, ch) = (font.cw, font.ch);
        let y = r * ch;
        let gutter = self.gutter_cols() * cw;
        fill(buf, w, R { x: 0, y, w: gutter, h: ch }, GUTTER_BG);
        fill(buf, w, R { x: gutter, y, w: w - gutter, h: ch }, BG);
        let n = self.top + r as u64;
        if r < self.lines.len() || self.edit.as_ref().is_some_and(|(l, _)| *l == n) {
            let num = format!("{:>7} ", n + 1);
            draw_text(buf, w, font, 0, y, num.as_bytes(), self.gutter_cols(), GUTTER_FG, GUTTER_BG);
            let cols = (w - gutter) / cw;
            draw_text(buf, w, font, gutter, y, self.line_bytes(r), cols, FG, BG);
        }
        if self.cursor.0 == n && self.palette.is_none() {
            fill(buf, w, R { x: gutter + self.cursor.1 * cw, y, w: 2, h: ch }, CURSOR);
        }
    }

    fn draw_palette(&self, buf: &mut [u32], font: &mut Font, pr: R) {
        let w = self.size.0;
        let (cw, ch) = (font.cw, font.ch);
        fill(buf, w, pr, PAL_BG);
        let q = self.palette.as_deref().unwrap_or("");
        let cols = pr.w / cw - 2;
        draw_text(buf, w, font, pr.x + cw, pr.y + 4, format!("> {q}").as_bytes(), cols, FG, PAL_BG);
        let hits = self.commands.iter().filter(|c| subsequence(q, c)).take(PAL_ROWS);
        for (i, c) in hits.enumerate() {
            let y = pr.y + 4 + (i + 1) * ch;
            let bg = if i == 0 { PAL_SEL } else { PAL_BG };
            fill(buf, w, R { x: pr.x, y, w: pr.w, h: ch }, bg);
            draw_text(buf, w, font, pr.x + cw, y, c.as_bytes(), cols, FG, bg);
        }
    }
}

fn subsequence(q: &str, s: &str) -> bool {
    let mut it = s.chars().map(|c| c.to_ascii_lowercase());
    q.chars().all(|qc| it.any(|c| c == qc.to_ascii_lowercase()))
}

fn fill(buf: &mut [u32], stride: usize, r: R, color: u32) {
    for y in r.y..r.y + r.h {
        let row = &mut buf[y * stride..y * stride + stride];
        row[r.x.min(stride)..(r.x + r.w).min(stride)].fill(color);
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_text(buf: &mut [u32], stride: usize, font: &mut Font, x: usize, y: usize, text: &[u8], max_cols: usize, fg: u32, bg: u32) {
    let (cw, ch) = (font.cw, font.ch);
    for (i, &b) in text.iter().take(max_cols).enumerate() {
        let x0 = x + i * cw;
        if x0 + cw > stride {
            break;
        }
        if b == b' ' {
            continue; // background already filled
        }
        let t = font.tile(b, fg, bg);
        for ty in 0..ch {
            let o = (y + ty) * stride + x0;
            buf[o..o + cw].copy_from_slice(&t[ty * cw..ty * cw + cw]);
        }
    }
}

/// Moves window pixels with a screen-to-screen BitBlt, so the OS (and RDP)
/// sees a copy instead of new pixels.
#[cfg(windows)]
fn screen_copy(hwnd: isize, w: usize, h: usize, from_y: usize, to_y: usize) {
    use windows_sys::Win32::Graphics::Gdi::{BitBlt, GetDC, ReleaseDC, SRCCOPY};
    unsafe {
        let hwnd = hwnd as windows_sys::Win32::Foundation::HWND;
        let dc = GetDC(hwnd);
        BitBlt(dc, 0, to_y as i32, w as i32, h as i32, dc, 0, from_y as i32, SRCCOPY);
        ReleaseDC(hwnd, dc);
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
        assert!(hr >= 0, "DWM cloak failed: {hr:#x}; refusing to show a visible window");
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.gfx.is_some() {
            return;
        }
        use winit::platform::windows::WindowAttributesExtWindows;
        // Agent runs must not disturb the owner's desktop: the window is
        // created hidden, cloaked by DWM (composed but never shown), then
        // shown without activation and without a taskbar entry. On-screen
        // placement matters: GDI clips drawing to off-screen windows.
        let attrs = Window::default_attributes()
            .with_title("kwil - renderer B")
            .with_inner_size(PhysicalSize::new(1280, 800))
            .with_position(PhysicalPosition::new(0, 0))
            .with_visible(false)
            .with_active(false)
            .with_skip_taskbar(true)
            .with_drag_and_drop(false);
        mark("resumed");
        let window = Rc::new(el.create_window(attrs).unwrap());
        mark("window created");
        let context = Context::new(window.clone()).unwrap();
        let surface = Surface::new(&context, window.clone()).unwrap();
        let hwnd = match raw_window_handle::HasWindowHandle::window_handle(&*window).unwrap().as_raw() {
            raw_window_handle::RawWindowHandle::Win32(h) => h.hwnd.get(),
            _ => 0,
        };
        mark("surface created");
        cloak_and_show(hwnd);
        mark("shown");
        self.font = Some(self.font_thread.take().unwrap().join().unwrap());
        mark("font ready");
        let sz = window.inner_size();
        self.size = (sz.width as usize, sz.height as usize);
        self.stats.set_window_px((sz.width * sz.height) as u64);
        self.gfx = Some(Gfx { window, surface, hwnd });
        self.resize_surface();
        self.reload_lines();
        self.pending.full = true;
        self.render();
        mark("first frame presented");
        if self.script {
            let proxy = self.proxy.clone();
            spiketrace::spawn_script(move |s| {
                let _ = proxy.send_event(UserEvent::Script(s));
            });
        }
    }

    fn user_event(&mut self, el: &ActiveEventLoop, ev: UserEvent) {
        match ev {
            UserEvent::Script(s) => self.step(el, s),
            UserEvent::IndexDone => {
                self.pending.status = true;
                self.request();
            }
            UserEvent::GpuReady(ms) => self.stats.set_gpu_init_ms(ms),
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, ev: WindowEvent) {
        match ev {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::Resized(sz) => {
                // The first Resized repeats the creation size; repainting
                // for it would cost a second full present at startup.
                if (sz.width as usize, sz.height as usize) == self.size {
                    return;
                }
                self.size = (sz.width as usize, sz.height as usize);
                self.resize_surface();
                self.reload_lines();
                self.pending.full = true;
                self.request();
            }
            WindowEvent::RedrawRequested => self.render(),
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::KeyboardInput { event, .. } => self.key(&event),
            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => -(y * 3.0) as i64,
                    MouseScrollDelta::PixelDelta(p) => -(p.y / self.font().ch as f64) as i64,
                };
                self.scroll(lines);
            }
            _ => {}
        }
    }
}

impl App {
    fn resize_surface(&mut self) {
        let (w, h) = self.size;
        if let (Some(g), Some(w), Some(h)) = (&mut self.gfx, NonZeroU32::new(w as u32), NonZeroU32::new(h as u32)) {
            g.surface.resize(w, h).unwrap();
        }
    }
}

fn mark(what: &str) {
    if std::env::var_os("KWIL_TRACE_STARTUP").is_some() {
        eprintln!("{:>8.2} ms  {what}", spiketrace::process_age_ms());
    }
}

fn main() {
    mark("main");
    let args: Vec<String> = std::env::args().collect();
    let path = PathBuf::from(args.get(1).expect("usage: renderer-b <file> [--script] [--capture <dir>]"));
    let script = args.iter().any(|a| a == "--script");
    let capture = args.iter().position(|a| a == "--capture").map(|i| PathBuf::from(&args[i + 1]));

    // Font loading runs while the window is created.
    let font_thread = Some(std::thread::spawn(|| Font::load(15.0)));
    let file = TextFile::open(&path).expect("open file");
    mark("file opened");
    let event_loop = EventLoop::<UserEvent>::with_user_event().build().unwrap();
    mark("event loop built");
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    {
        let p = proxy.clone();
        file.index_in_background(8, move || {
            let _ = p.send_event(UserEvent::IndexDone);
        });
    }
    #[cfg(feature = "gpu-init")]
    {
        let p = proxy.clone();
        std::thread::spawn(move || {
            let t = Instant::now();
            // KWIL_WARP=1 forces the software adapter that RDP sessions without
            // GPU policy get (DX12 WARP).
            let warp = std::env::var_os("KWIL_WARP").is_some();
            let backends = if std::env::var_os("KWIL_DX12").is_some() { wgpu::Backends::DX12 } else { wgpu::Backends::all() };
            let instance = wgpu::Instance::new(wgpu::InstanceDescriptor { backends, ..wgpu::InstanceDescriptor::new_without_display_handle() });
            let opts = wgpu::RequestAdapterOptions { force_fallback_adapter: warp, ..Default::default() };
            let adapter = pollster::block_on(instance.request_adapter(&opts)).unwrap();
            eprintln!("adapter: {:?}", adapter.get_info().name);
            let _device = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).unwrap();
            let _ = p.send_event(UserEvent::GpuReady(t.elapsed().as_secs_f64() * 1000.0));
        });
    }
    let verbs = ["Open", "Save", "Close", "Find", "Replace", "Go to", "Toggle", "Reload", "Compare", "Select"];
    let nouns = ["File", "All Files", "Tab", "Line", "Word Wrap", "Encoding", "Line Endings", "Selection", "Theme", "Zoom", "Recent", "Folder", "Palette", "Whitespace", "Comment", "Bookmark", "Panel", "Diff", "Preview", "Settings"];
    let commands = verbs.iter().flat_map(|v| nouns.iter().map(move |n| format!("{v} {n}"))).collect();

    let mut app = App {
        file,
        proxy,
        stats: Stats::new("B: winit+softbuffer"),
        script,
        capture,
        font: None,
        font_thread,
        gfx: None,
        size: (0, 0),
        top: 0,
        lines: Vec::new(),
        cursor: (0, 0),
        edit: None,
        palette: None,
        commands,
        mods: ModifiersState::empty(),
        pending: Pending::default(),
    };
    event_loop.run_app(&mut app).unwrap();
    if app.script {
        app.stats.print();
    }
}
