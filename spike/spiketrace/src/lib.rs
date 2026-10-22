//! Shared M0 measurement for the renderer spikes, so A and B report the
//! same numbers in the same way.
//!
//! The scripted scenario (both spikes run it with `--script`):
//!   1. first frame shows the 1 GB fixture
//!   2. idle 1.5 s                      -> presents must be 0
//!   3. type 20 characters, 40 ms apart -> damage should be one line + status
//!   4. scroll 60 steps of 3 lines      -> one present per step
//!   5. palette: open, type "save", close
//!   6. idle 1.5 s, then exit
//!
//! Output: one JSON line on stdout (see `Stats::print`).

use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Startup,
    Idle,
    Typing,
    Scroll,
    Palette,
    IdleAfter,
}

pub const PHASES: [Phase; 6] =
    [Phase::Startup, Phase::Idle, Phase::Typing, Phase::Scroll, Phase::Palette, Phase::IdleAfter];

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Phase::Startup => "startup",
            Phase::Idle => "idle",
            Phase::Typing => "typing",
            Phase::Scroll => "scroll",
            Phase::Palette => "palette",
            Phase::IdleAfter => "idle_after",
        }
    }
}

#[derive(Default, Clone, Copy)]
struct PhaseStats {
    presents: u64,
    damage_px: u64,
    copy_px: u64,
    full_presents: u64,
    cpu_us: u64,
    max_frame_us: u64,
}

/// Script steps with their delay from the previous step, in ms.
#[derive(Clone, Copy, Debug)]
pub enum Step {
    Enter(Phase),
    TypeChar(char),
    Scroll(i32),
    OpenPalette,
    ClosePalette,
    Exit,
}

pub fn script() -> Vec<(u64, Step)> {
    let mut s = vec![(300, Step::Enter(Phase::Idle)), (1500, Step::Enter(Phase::Typing))];
    for c in "fast text, no lag!! ".chars() {
        s.push((40, Step::TypeChar(c)));
    }
    s.push((200, Step::Enter(Phase::Scroll)));
    for _ in 0..60 {
        s.push((16, Step::Scroll(3)));
    }
    s.push((200, Step::Enter(Phase::Palette)));
    s.push((50, Step::OpenPalette));
    for c in "save".chars() {
        s.push((80, Step::TypeChar(c)));
    }
    s.push((150, Step::ClosePalette));
    s.push((100, Step::Enter(Phase::IdleAfter)));
    s.push((1500, Step::Exit));
    s
}

/// Runs the script on its own thread; `send` delivers each step to the UI
/// event loop (an event proxy), so the UI stays purely event-driven.
pub fn spawn_script(send: impl Fn(Step) + Send + 'static) {
    std::thread::spawn(move || {
        for (delay, step) in script() {
            std::thread::sleep(std::time::Duration::from_millis(delay));
            send(step);
        }
    });
}

pub struct Stats {
    pub renderer: &'static str,
    phase: Phase,
    per: [PhaseStats; 6],
    first_frame_ms: Option<f64>,
    gpu_init_ms: Option<f64>,
    window_px: u64,
    notes: Vec<String>,
}

impl Stats {
    pub fn new(renderer: &'static str) -> Self {
        Stats {
            renderer,
            phase: Phase::Startup,
            per: [PhaseStats::default(); 6],
            first_frame_ms: None,
            gpu_init_ms: None,
            window_px: 0,
            notes: Vec::new(),
        }
    }

    pub fn enter(&mut self, phase: Phase) {
        self.phase = phase;
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn set_window_px(&mut self, px: u64) {
        self.window_px = px;
    }

    pub fn set_gpu_init_ms(&mut self, ms: f64) {
        self.gpu_init_ms = Some(ms);
    }

    pub fn note(&mut self, n: impl Into<String>) {
        self.notes.push(n.into());
    }

    /// Records one present: damaged pixels drawn, pixels moved by a copy,
    /// whether it covered the whole window, and the CPU time spent on it.
    pub fn present(&mut self, damage_px: u64, copy_px: u64, full: bool, started: Instant) {
        let us = started.elapsed().as_micros() as u64;
        let p = &mut self.per[PHASES.iter().position(|&x| x == self.phase).unwrap()];
        p.presents += 1;
        p.damage_px += damage_px;
        p.copy_px += copy_px;
        p.full_presents += full as u64;
        p.cpu_us += us;
        p.max_frame_us = p.max_frame_us.max(us);
        if self.first_frame_ms.is_none() {
            self.first_frame_ms = Some(process_age_ms());
        }
    }

    pub fn first_frame_done(&self) -> bool {
        self.first_frame_ms.is_some()
    }

    pub fn print(&self) {
        let mut s = format!(
            "{{\"renderer\":\"{}\",\"first_frame_ms\":{:.2},\"gpu_init_ms\":{},\"window_px\":{},\"remote_session\":{},\"peak_ws_mb\":{:.1},\"private_mb\":{:.1}",
            self.renderer,
            self.first_frame_ms.unwrap_or(-1.0),
            self.gpu_init_ms.map_or("null".into(), |v| format!("{v:.2}")),
            self.window_px,
            is_remote_session(),
            mem().0,
            mem().1,
        );
        for (i, ph) in PHASES.iter().enumerate() {
            let p = self.per[i];
            s += &format!(
                ",\"{}\":{{\"presents\":{},\"full\":{},\"damage_px\":{},\"copy_px\":{},\"cpu_ms\":{:.2},\"max_frame_ms\":{:.2}}}",
                ph.name(),
                p.presents,
                p.full_presents,
                p.damage_px,
                p.copy_px,
                p.cpu_us as f64 / 1000.0,
                p.max_frame_us as f64 / 1000.0
            );
        }
        s += &format!(",\"notes\":{:?}}}", self.notes);
        println!("{s}");
    }
}

/// Milliseconds since the OS created this process (includes loader time).
#[cfg(windows)]
pub fn process_age_ms() -> f64 {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let z = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut created, mut exit, mut kernel, mut user, mut now) = (z, z, z, z, z);
    unsafe {
        GetProcessTimes(GetCurrentProcess(), &mut created, &mut exit, &mut kernel, &mut user);
        GetSystemTimePreciseAsFileTime(&mut now);
    }
    let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
    (t(now) - t(created)) as f64 / 10_000.0
}

#[cfg(not(windows))]
pub fn process_age_ms() -> f64 {
    -1.0
}

/// True inside an RDP session (spec §6 remote-mode detection).
#[cfg(windows)]
pub fn is_remote_session() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_REMOTESESSION};
    unsafe { GetSystemMetrics(SM_REMOTESESSION) != 0 }
}

#[cfg(not(windows))]
pub fn is_remote_session() -> bool {
    false
}

#[cfg(windows)]
fn mem() -> (f64, f64) {
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut c: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
    let mb = (1 << 20) as f64;
    (c.PeakWorkingSetSize as f64 / mb, c.PagefileUsage as f64 / mb)
}

#[cfg(not(windows))]
fn mem() -> (f64, f64) {
    (0.0, 0.0)
}

/// Captures a window's client area to a 24-bit BMP with `PrintWindow`,
/// which works for off-screen windows. Evidence for the decision record.
#[cfg(windows)]
pub fn capture_window_bmp(hwnd: isize, path: &std::path::Path) -> std::io::Result<()> {
    capture(hwnd, path, true)
}

/// Captures by reading the window's own DC (the DWM redirection surface).
/// Right for GDI-presented windows, where `PrintWindow` returns blank.
#[cfg(windows)]
pub fn capture_window_dc_bmp(hwnd: isize, path: &std::path::Path) -> std::io::Result<()> {
    capture(hwnd, path, false)
}

#[cfg(windows)]
fn capture(hwnd: isize, path: &std::path::Path, print_window: bool) -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::{HWND, RECT};
    use windows_sys::Win32::Graphics::Gdi::*;
    use windows_sys::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow};
    use windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect;
    const PW_CLIENTONLY: PRINT_WINDOW_FLAGS = 1;
    const PW_RENDERFULLCONTENT: PRINT_WINDOW_FLAGS = 2;
    unsafe {
        let hwnd = hwnd as HWND;
        let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        GetClientRect(hwnd, &mut r);
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        let wdc = GetDC(hwnd);
        let mdc = CreateCompatibleDC(wdc);
        let bmp = CreateCompatibleBitmap(wdc, w, h);
        let old = SelectObject(mdc, bmp);
        if print_window {
            PrintWindow(hwnd, mdc, PW_CLIENTONLY | PW_RENDERFULLCONTENT);
        } else {
            BitBlt(mdc, 0, 0, w, h, wdc, 0, 0, SRCCOPY);
        }
        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = w;
        info.bmiHeader.biHeight = h; // bottom-up, as BMP files store it
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 24;
        let stride = ((w * 3 + 3) & !3) as usize;
        let mut px = vec![0u8; stride * h as usize];
        GetDIBits(mdc, bmp, 0, h as u32, px.as_mut_ptr().cast(), &mut info, DIB_RGB_COLORS);
        SelectObject(mdc, old);
        DeleteObject(bmp);
        DeleteDC(mdc);
        ReleaseDC(hwnd, wdc);
        let mut f = Vec::with_capacity(54 + px.len());
        f.extend_from_slice(b"BM");
        f.extend_from_slice(&((54 + px.len()) as u32).to_le_bytes());
        f.extend_from_slice(&[0; 4]);
        f.extend_from_slice(&54u32.to_le_bytes());
        f.extend_from_slice(&40u32.to_le_bytes());
        f.extend_from_slice(&w.to_le_bytes());
        f.extend_from_slice(&h.to_le_bytes());
        f.extend_from_slice(&1u16.to_le_bytes());
        f.extend_from_slice(&24u16.to_le_bytes());
        f.extend_from_slice(&[0; 24]);
        f.extend_from_slice(&px);
        std::fs::write(path, f)
    }
}
