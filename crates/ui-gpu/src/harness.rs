//! Dev-only, opt-in finite script. No timers or polling exist in the shipping UI.
use super::{App, Damage, UserEvent};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};

#[derive(Clone, Copy, Debug)]
pub(crate) enum Step {
    Idle,
    Scroll,
    Repaint,
    Status,
    RemoteOn,
    RemoteOff,
    End,
}
#[derive(Default, Clone, Copy)]
struct Counts {
    presents: u64,
    full: u64,
    damage: u64,
    copy: u64,
}
#[derive(Default)]
pub(crate) struct Metrics {
    phase: usize,
    per: [Counts; 5],
}
impl Metrics {
    pub fn present(&mut self, full: bool, damage: u64, copy: u64) {
        let c = &mut self.per[self.phase];
        c.presents += 1;
        c.full += u64::from(full);
        c.damage += damage;
        c.copy += copy;
    }
    fn print(&self) {
        for (i, name) in ["settling", "idle", "scroll", "repaint", "status"].iter().enumerate() {
            let c = self.per[i];
            println!(
                "kwil-frame phase={name} presents={} full={} damage_px={} copy_px={}",
                c.presents, c.full, c.damage, c.copy
            );
        }
    }
}
pub(crate) fn start(proxy: EventLoopProxy<UserEvent>) {
    std::thread::spawn(move || {
        for (delay, step) in [
            (2000, Step::Idle),
            (250, Step::Scroll),
            (200, Step::Repaint),
            (150, Step::Status),
            (150, Step::RemoteOn),
            (100, Step::RemoteOff),
            (100, Step::End),
        ] {
            std::thread::sleep(Duration::from_millis(delay));
            if proxy.send_event(UserEvent::Harness(step)).is_err() {
                break;
            }
        }
    });
}
pub(crate) fn step(app: &mut App, el: &ActiveEventLoop, step: Step) {
    let dir = std::env::var_os("KWIL_CAPTURE_DIR").map(PathBuf::from);
    match step {
        Step::Idle => {
            println!("kwil-frame glyphs_pending={}", app.font.as_ref().map_or(0, |f| f.pending_glyphs()));
            app.metrics.phase = 1;
            capture(app, dir.as_deref(), "first.bmp");
        }
        Step::Scroll => {
            app.metrics.phase = 2;
            let d = app.view.scroll(3);
            app.apply(d);
        }
        Step::Repaint => {
            capture(app, dir.as_deref(), "scroll.bmp");
            #[cfg(feature = "gpu")]
            if let (Some(gpu), Some(gfx)) = (&app.gpu, &app.gfx) {
                println!("kwil-gpu backing_pixels_equal_cpu={:?}", gpu.matches(&gfx.pixels));
            }
            app.metrics.phase = 3;
            app.apply(Damage { full: true, ..Default::default() });
        }
        Step::Status => {
            capture(app, dir.as_deref(), "repaint.bmp");
            if let Some(dir) = &dir {
                let a = std::fs::read(dir.join("scroll.bmp"));
                let b = std::fs::read(dir.join("repaint.bmp"));
                println!("kwil-frame scroll_copy_matches_repaint={}", a.is_ok() && a.ok() == b.ok());
            }
            app.metrics.phase = 4;
            let d = app.view.set_notice("status damage probe".into());
            app.apply(d);
        }
        Step::RemoteOn | Step::RemoteOff => {
            #[cfg(windows)]
            if let Some(gfx) = &app.gfx {
                unsafe {
                    use windows_sys::Win32::UI::WindowsAndMessaging::{
                        SendMessageW, WM_WTSSESSION_CHANGE, WTS_CONSOLE_CONNECT, WTS_REMOTE_CONNECT,
                    };
                    SendMessageW(
                        gfx.hwnd as _,
                        WM_WTSSESSION_CHANGE,
                        if matches!(step, Step::RemoteOn) { WTS_REMOTE_CONNECT } else { WTS_CONSOLE_CONNECT } as usize,
                        0,
                    );
                }
            }
            #[cfg(not(windows))]
            {
                let _ = app.proxy.send_event(UserEvent::SessionChanged(matches!(step, Step::RemoteOn)));
            }
        }
        Step::End => {
            capture(app, dir.as_deref(), "status.bmp");
            app.metrics.print();
            println!(
                "kwil-frame final_remote={} cursor_blink_allowed={}",
                app.policy.remote(),
                app.policy.cursor_blink_allowed()
            );
            el.exit();
        }
    }
}
fn capture(app: &App, dir: Option<&Path>, name: &str) {
    if let (Some(dir), Some(gfx)) = (dir, &app.gfx) {
        #[cfg(windows)]
        if let Err(error) = capture_dc(gfx.hwnd, &dir.join(name)) {
            eprintln!("capture failed: {error}");
        }
        #[cfg(not(windows))]
        {
            let _ = (dir, gfx, name);
        }
    }
}
// Based on the admitted M0 window-DC capture; the HWND comes directly from the
// owned window, never FindWindow/title guessing. No GPU/PrintWindow claim here.
#[cfg(windows)]
fn capture_dc(hwnd: isize, path: &Path) -> std::io::Result<()> {
    use windows_sys::Win32::{Foundation::RECT, Graphics::Gdi::*, UI::WindowsAndMessaging::GetClientRect};
    unsafe {
        let hwnd = hwnd as _;
        let mut r: RECT = std::mem::zeroed();
        GetClientRect(hwnd, &mut r);
        let (w, h) = (r.right, r.bottom);
        let dc = GetDC(hwnd);
        let memory = CreateCompatibleDC(dc);
        let bitmap = CreateCompatibleBitmap(dc, w, h);
        let old = SelectObject(memory, bitmap);
        let copied = BitBlt(memory, 0, 0, w, h, dc, 0, 0, SRCCOPY) != 0;
        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader.biSize = 40;
        info.bmiHeader.biWidth = w;
        info.bmiHeader.biHeight = h;
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 24;
        let stride = ((w * 3 + 3) & !3) as usize;
        let mut pixels = vec![0u8; stride * h as usize];
        SelectObject(memory, old);
        let lines = GetDIBits(memory, bitmap, 0, h as u32, pixels.as_mut_ptr().cast(), &mut info, DIB_RGB_COLORS);
        DeleteObject(bitmap);
        DeleteDC(memory);
        ReleaseDC(hwnd, dc);
        if !copied || lines != h {
            return Err(std::io::Error::other("window DC capture failed"));
        }
        let mut file = Vec::with_capacity(54 + pixels.len());
        file.extend_from_slice(b"BM");
        file.extend_from_slice(&((54 + pixels.len()) as u32).to_le_bytes());
        file.extend_from_slice(&[0; 4]);
        file.extend_from_slice(&54u32.to_le_bytes());
        file.extend_from_slice(&40u32.to_le_bytes());
        file.extend_from_slice(&w.to_le_bytes());
        file.extend_from_slice(&h.to_le_bytes());
        file.extend_from_slice(&1u16.to_le_bytes());
        file.extend_from_slice(&24u16.to_le_bytes());
        file.extend_from_slice(&[0; 24]);
        file.extend_from_slice(&pixels);
        std::fs::write(path, file)
    }
}
