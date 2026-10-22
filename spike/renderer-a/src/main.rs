//! M0 renderer A: gpui. Same three screens and script as renderer B.
//!
//!   renderer-a <file> [--script] [--capture <dir>]

use futures::StreamExt;
use futures::channel::mpsc;
use gpui::{
    App, Application, Bounds, Context, Font, FontWeight, Hsla, SharedString, TextRun, Window,
    WindowBounds, WindowOptions, canvas, div, font, point, prelude::*, px, rgb, size,
};
use kwil_core_text::TextFile;
use spiketrace::{Phase, Stats, Step};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;

const FONT_SIZE: f32 = 14.0;
const LINE_H: f32 = 18.0;
const GUTTER_CHARS: usize = 9;
const MAX_COLS: usize = 300;
const CURSOR_OFFSET: u64 = 5;

enum Msg {
    Step(Step),
    IndexDone,
}

struct Palette {
    query: String,
}

struct Editor {
    file: TextFile,
    top: u64,
    /// Characters typed into the cursor line (an overlay; no real editing in M0).
    typed: String,
    palette: Option<Palette>,
    commands: Vec<SharedString>,
    stats: Rc<RefCell<Stats>>,
    frame_start: Rc<Cell<Option<Instant>>>,
    window_px: Rc<Cell<u64>>,
    hwnd: isize,
    capture: Option<PathBuf>,
    cpu_at_phase: (Phase, f64),
}

fn mono() -> Font {
    let mut f = font("Consolas");
    f.weight = FontWeight::NORMAL;
    f
}

fn run(text: &str, color: Hsla) -> TextRun {
    TextRun { len: text.len(), font: mono(), color, background_color: None, underline: None, strikethrough: None }
}

impl Editor {
    fn visible_lines(&self, count: usize) -> Vec<String> {
        let rows = match self.file.lines(self.top, count) {
            Some(rows) => rows,
            None => self.file.first_lines(count),
        };
        rows.iter()
            .enumerate()
            .map(|(i, l)| {
                let mut s: String = String::from_utf8_lossy(&l[..l.len().min(MAX_COLS)]).into_owned();
                if self.top + i as u64 == self.top + CURSOR_OFFSET && !self.typed.is_empty() {
                    s.insert_str(0, &self.typed);
                }
                s
            })
            .collect()
    }

    fn status(&self) -> String {
        match self.file.index() {
            Some(ix) => format!(
                "Ln {}  |  {} lines  |  {:.1} MB  |  gpui",
                self.top + CURSOR_OFFSET + 1,
                ix.line_count(),
                self.file.len() as f64 / 1e6
            ),
            None => {
                let p = self.file.progress();
                format!("≈ indexing {}%", p.scanned * 100 / p.total.max(1))
            }
        }
    }

    fn filtered(&self, q: &str) -> Vec<SharedString> {
        let q: Vec<char> = q.to_lowercase().chars().collect();
        self.commands
            .iter()
            .filter(|c| {
                let mut it = q.iter().peekable();
                for ch in c.to_lowercase().chars() {
                    if it.peek() == Some(&&ch) {
                        it.next();
                    }
                }
                it.peek().is_none()
            })
            .take(12)
            .cloned()
            .collect()
    }

    fn scroll(&mut self, lines: i64) {
        let Some(ix) = self.file.index() else { return };
        let max = ix.line_count().saturating_sub(1);
        self.top = (self.top as i64 + lines).clamp(0, max as i64) as u64;
    }

    fn capture(&self, name: &str) {
        if let Some(dir) = &self.capture {
            let _ = std::fs::create_dir_all(dir);
            if let Err(e) = spiketrace::capture_window_bmp(self.hwnd, &dir.join(name)) {
                self.stats.borrow_mut().note(format!("capture {name} failed: {e}"));
            }
        }
    }

    fn handle(&mut self, msg: Msg, cx: &mut Context<Self>) {
        match msg {
            Msg::IndexDone => {}
            Msg::Step(step) => match step {
                Step::Enter(phase) => {
                    self.close_cpu_window();
                    match phase {
                        Phase::Typing => self.capture("a-1-file.bmp"),
                        Phase::Palette => self.capture("a-2-scrolled.bmp"),
                        _ => {}
                    }
                    if phase == Phase::Idle {
                        let r = window_rect(self.hwnd);
                        self.stats.borrow_mut().note(format!("window rect (screen px) {r:?}"));
                    }
                    self.stats.borrow_mut().enter(phase);
                    self.cpu_at_phase = (phase, process_cpu_ms());
                    wm_paint_mark(phase);
                    return; // no redraw needed
                }
                Step::TypeChar(c) => match &mut self.palette {
                    Some(p) => p.query.push(c),
                    None => self.typed.push(c),
                },
                Step::Scroll(n) => self.scroll(n as i64),
                Step::OpenPalette => self.palette = Some(Palette { query: String::new() }),
                Step::ClosePalette => {
                    self.capture("a-3-palette.bmp");
                    self.palette = None;
                }
                Step::Exit => {
                    self.close_cpu_window();
                    let mut st = self.stats.borrow_mut();
                    st.set_window_px(self.window_px.get());
                    st.note(wm_paint_report());
                    st.print();
                    drop(st);
                    cx.quit();
                    return;
                }
            },
        }
        cx.notify();
    }

    /// Process CPU time spent during idle phases (WM_PAINT wakeups cost CPU
    /// even without a present).
    fn close_cpu_window(&mut self) {
        let (phase, start) = self.cpu_at_phase;
        if matches!(phase, Phase::Idle | Phase::IdleAfter) {
            self.stats
                .borrow_mut()
                .note(format!("{phase:?}: process CPU {:.1} ms", process_cpu_ms() - start));
        }
    }
}

impl Render for Editor {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        self.frame_start.set(Some(Instant::now()));
        let bg = rgb(0x1e1f22);
        let fg: Hsla = rgb(0xd4d4d4).into();
        let dim: Hsla = rgb(0x6b6f76).into();

        // Lines are fetched for a generous screen; the canvas paints what fits.
        let lines = self.visible_lines(80);
        let top = self.top;
        let stats = self.stats.clone();
        let frame_start = self.frame_start.clone();
        let window_px = self.window_px.clone();

        let text_area = canvas(
            move |_, _, _| (),
            move |bounds, _, window, cx| {
                let ts = window.text_system().clone();
                let rows = ((bounds.size.height / px(LINE_H)) as usize + 1).min(lines.len());
                for (i, line) in lines.iter().take(rows).enumerate() {
                    let y = bounds.origin.y + px(i as f32 * LINE_H);
                    let num = format!("{:>w$} ", top + i as u64 + 1, w = GUTTER_CHARS - 1);
                    let shaped = ts.shape_line(num.clone().into(), px(FONT_SIZE), &[run(&num, dim)], None);
                    let _ = shaped.paint(point(bounds.origin.x + px(4.), y), px(LINE_H), window, cx);
                    if !line.is_empty() {
                        let shaped = ts.shape_line(line.clone().into(), px(FONT_SIZE), &[run(line, fg)], None);
                        let x = bounds.origin.x + px(4. + GUTTER_CHARS as f32 * 7.7);
                        let _ = shaped.paint(point(x, y), px(LINE_H), window, cx);
                    }
                }
                let vp = window.viewport_size();
                let scale = window.scale_factor();
                let wpx = (f32::from(vp.width) * scale) as u64 * (f32::from(vp.height) * scale) as u64;
                window_px.set(wpx);
                // gpui rebuilds and presents the whole scene each frame.
                let started = frame_start.take().unwrap_or_else(Instant::now);
                stats.borrow_mut().present(wpx, 0, true, started);
            },
        )
        .flex_1()
        .w_full();

        let status = div()
            .h(px(22.))
            .w_full()
            .px_2()
            .bg(rgb(0x2b2d31))
            .text_color(rgb(0xa0a4ab))
            .font_family("Consolas")
            .text_size(px(13.))
            .child(self.status());

        let mut root = div().size_full().flex().flex_col().bg(bg).child(text_area).child(status);

        if let Some(p) = &self.palette {
            let items = self.filtered(&p.query);
            let mut list = div().flex().flex_col();
            for (i, it) in items.into_iter().enumerate() {
                list = list.child(
                    div()
                        .px_2()
                        .py(px(2.))
                        .when(i == 0, |d| d.bg(rgb(0x3b4252)))
                        .child(it),
                );
            }
            root = root.child(
                div().absolute().top(px(40.)).left(px(340.)).w(px(600.)).bg(rgb(0x2b2d31)).border_1().border_color(rgb(0x4a4d55))
                    .font_family("Consolas").text_size(px(14.)).text_color(rgb(0xd4d4d4))
                    .child(div().px_2().py_1().bg(rgb(0x1e1f22)).child(format!("> {}", p.query)))
                    .child(list),
            );
        }
        root
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = PathBuf::from(args.get(1).expect("usage: renderer-a <file> [--script] [--capture <dir>]"));
    let script = args.iter().any(|a| a == "--script");
    let capture = args.iter().position(|a| a == "--capture").map(|i| PathBuf::from(&args[i + 1]));
    install_wm_paint_counter();

    Application::new().run(move |cx: &mut App| {
        let file = TextFile::open(&path).expect("open file");
        let (tx, mut rx) = mpsc::unbounded::<Msg>();

        let bounds = Bounds { origin: point(px(-4000.), px(100.)), size: size(px(1280.), px(800.)) };
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    focus: false,
                    // Shown by `park_offscreen` after it is moved off-screen:
                    // gpui clamps a negative origin onto a display.
                    show: false,
                    ..Default::default()
                },
                |window, cx| {
                    let hwnd = hwnd_of(window);
                    let commands = (0..200)
                        .map(|i| {
                            let verbs = ["Open", "Save", "Close", "Toggle", "Go to", "Find", "Replace", "Compare", "Show", "Reload"];
                            let nouns = ["File", "Tab", "Line", "Encoding", "Word Wrap", "Palette", "Theme", "Preview", "Diff", "Settings"];
                            SharedString::from(format!("{} {} {}", verbs[i % 10], nouns[(i / 10) % 10], i / 100 + 1))
                        })
                        .collect();
                    cx.new(|_| Editor {
                        file: file.clone(),
                        top: 0,
                        typed: String::new(),
                        palette: None,
                        commands,
                        stats: Rc::new(RefCell::new(Stats::new("gpui"))),
                        frame_start: Rc::new(Cell::new(None)),
                        window_px: Rc::new(Cell::new(0)),
                        hwnd,
                        capture: capture.clone(),
                        cpu_at_phase: (Phase::Startup, 0.0),
                    })
                },
            )
            .expect("open window");

        let done_tx = tx.clone();
        file.index_in_background(8, move || {
            let _ = done_tx.unbounded_send(Msg::IndexDone);
        });
        if script {
            let stx = tx.clone();
            spiketrace::spawn_script(move |s| {
                let _ = stx.unbounded_send(Msg::Step(s));
            });
        }

        let entity = window.update(cx, |_, _, cx| cx.entity()).expect("entity");
        // Parked from a foreground task: run inside the App borrow, the
        // synchronous WM_SIZE would fail to reach gpui's resize handler.
        let hwnd = entity.read(cx).hwnd;
        cx.spawn(async move |_| park_offscreen(hwnd)).detach();
        cx.spawn(async move |cx| {
            while let Some(msg) = rx.next().await {
                if entity.update(cx, |ed, cx| ed.handle(msg, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
    });
}

fn hwnd_of(window: &Window) -> isize {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match HasWindowHandle::window_handle(window).map(|h| h.as_raw()) {
        Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
        _ => 0,
    }
}

// ---- WM_PAINT wakeup counter (Windows): counts paint messages the UI
// thread receives per phase, whether or not gpui presents.

use std::sync::atomic::{AtomicU64, Ordering};
static WM_PAINTS: AtomicU64 = AtomicU64::new(0);
thread_local! {
    static MARKS: RefCell<Vec<(Phase, u64)>> = const { RefCell::new(Vec::new()) };
}

fn wm_paint_mark(phase: Phase) {
    MARKS.with(|m| m.borrow_mut().push((phase, WM_PAINTS.load(Ordering::Relaxed))));
}

fn wm_paint_report() -> String {
    let now = WM_PAINTS.load(Ordering::Relaxed);
    MARKS.with(|m| {
        let m = m.borrow();
        let mut out = String::from("WM_PAINT per phase:");
        for (i, (ph, at)) in m.iter().enumerate() {
            let end = m.get(i + 1).map_or(now, |n| n.1);
            out += &format!(" {ph:?}={}", end - at);
        }
        out
    })
}

#[cfg(windows)]
fn install_wm_paint_counter() {
    use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{CallNextHookEx, MSG, PM_REMOVE, SetWindowsHookExW, WH_GETMESSAGE, WM_PAINT};
    unsafe extern "system" fn hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code >= 0 && wparam == PM_REMOVE as usize {
            let msg = unsafe { &*(lparam as *const MSG) };
            if msg.message == WM_PAINT {
                WM_PAINTS.fetch_add(1, Ordering::Relaxed);
            }
        }
        unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
    }
    unsafe { SetWindowsHookExW(WH_GETMESSAGE, Some(hook), std::ptr::null_mut(), GetCurrentThreadId()) };
}

#[cfg(not(windows))]
fn install_wm_paint_counter() {}

#[cfg(windows)]
fn process_cpu_ms() -> f64 {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let z = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut a, mut b, mut k, mut u) = (z, z, z, z);
    unsafe { GetProcessTimes(GetCurrentProcess(), &mut a, &mut b, &mut k, &mut u) };
    let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
    (t(k) + t(u)) as f64 / 10_000.0
}

/// Moves the window off every display, sizes it to 1280x800 physical
/// pixels (same as renderer B) and shows it without activating it.
#[cfg(windows)]
fn park_offscreen(hwnd: isize) {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetClientRect, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOZORDER, SetWindowPos, ShowWindow};
    let place = |w: i32, h: i32| unsafe {
        SetWindowPos(hwnd as _, std::ptr::null_mut(), -6000, 100, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
    };
    place(1280, 800);
    unsafe { ShowWindow(hwnd as _, SW_SHOWNOACTIVATE) };
    // Grow the frame so the client area is exactly 1280x800.
    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    unsafe { GetClientRect(hwnd as _, &mut r) };
    place(1280 + 1280 - r.right, 800 + 800 - r.bottom);
}

#[cfg(not(windows))]
fn park_offscreen(_: isize) {}

#[cfg(windows)]
fn window_rect(hwnd: isize) -> (i32, i32, i32, i32) {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect;
    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    unsafe { GetWindowRect(hwnd as _, &mut r) };
    (r.left, r.top, r.right, r.bottom)
}

#[cfg(not(windows))]
fn window_rect(_: isize) -> (i32, i32, i32, i32) {
    (0, 0, 0, 0)
}

#[cfg(not(windows))]
fn process_cpu_ms() -> f64 {
    0.0
}
