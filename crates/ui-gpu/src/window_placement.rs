//! Monitor-safe outer placement and a single debounced, off-UI persistence lane.
use super::*;
use std::{
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::mpsc::{self, SyncSender, TrySendError},
    thread::JoinHandle,
    time::Duration,
};
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Placement {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    pub maximized: bool,
}
#[derive(Clone, Copy, Debug)]
pub(super) struct Work {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
}
impl Placement {
    pub fn clamp(self, work: Work) -> Self {
        let ratio =
            if self.scale.is_finite() && (0.25..=8.0).contains(&self.scale) { work.scale / self.scale } else { 1.0 };
        let width = ((self.width as f64 * ratio).round() as u32).clamp(1, work.width.max(1));
        let height = ((self.height as f64 * ratio).round() as u32).clamp(1, work.height.max(1));
        let x = i64::from(self.x)
            .clamp(i64::from(work.x), i64::from(work.x) + i64::from(work.width.saturating_sub(width)))
            as i32;
        let y = i64::from(self.y)
            .clamp(i64::from(work.y), i64::from(work.y) + i64::from(work.height.saturating_sub(height)))
            as i32;
        Self { x, y, width, height, scale: work.scale, ..self }
    }
    pub fn centered(work: Work, owner: Option<Self>, logical: (f64, f64)) -> Self {
        let width = (logical.0 * work.scale).round().min(work.width as f64 * 0.92).max(1.0) as u32;
        let height = (logical.1 * work.scale).round().min(work.height as f64 * 0.92).max(1.0) as u32;
        let anchor = owner.unwrap_or(Self {
            x: work.x,
            y: work.y,
            width: work.width,
            height: work.height,
            scale: work.scale,
            maximized: false,
        });
        Self {
            x: (i64::from(anchor.x) + (i64::from(anchor.width) - i64::from(width)) / 2) as i32,
            y: (i64::from(anchor.y) + (i64::from(anchor.height) - i64::from(height)) / 2) as i32,
            width,
            height,
            scale: work.scale,
            maximized: false,
        }
        .clamp(work)
    }
}
pub(super) fn work(m: &winit::monitor::MonitorHandle) -> Work {
    let p = m.position();
    let s = m.size();
    let fallback = Work { x: p.x, y: p.y, width: s.width, height: s.height, scale: m.scale_factor() };
    #[cfg(windows)]
    {
        use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITORINFO};
        use winit::platform::windows::MonitorHandleExtWindows;
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if unsafe { GetMonitorInfoW(m.hmonitor() as _, &mut info) } != 0 {
            return Work {
                x: info.rcWork.left,
                y: info.rcWork.top,
                width: info.rcWork.right.saturating_sub(info.rcWork.left) as u32,
                height: info.rcWork.bottom.saturating_sub(info.rcWork.top) as u32,
                scale: fallback.scale,
            };
        }
    }
    fallback
}
pub(super) fn capture(window: &Window) -> Option<Placement> {
    if window.is_minimized() == Some(true) {
        return None;
    }
    let p = window.outer_position().ok()?;
    let s = window.outer_size();
    (s.width > 0 && s.height > 0).then_some(Placement {
        x: p.x,
        y: p.y,
        width: s.width,
        height: s.height,
        scale: window.scale_factor(),
        maximized: window.is_maximized(),
    })
}
pub(super) fn fit_window(window: &Window, placement: Placement) {
    let before = window.inner_size();
    let outer = window.outer_size();
    let width = placement.width.saturating_sub(outer.width.saturating_sub(before.width)).max(1);
    let height = placement.height.saturating_sub(outer.height.saturating_sub(before.height)).max(1);
    let _ = window.request_inner_size(PhysicalSize::new(width, height));
    window.set_outer_position(winit::dpi::PhysicalPosition::new(placement.x, placement.y));
    window.set_maximized(placement.maximized);
}
fn encode(p: Placement) -> String {
    format!("kwil-window-v1 {} {} {} {} {} {}\n", p.x, p.y, p.width, p.height, p.scale, u8::from(p.maximized))
}
fn decode(bytes: &[u8]) -> io::Result<Placement> {
    let fail = || io::Error::new(io::ErrorKind::InvalidData, "Invalid saved window placement");
    let text = std::str::from_utf8(bytes).map_err(|_| fail())?;
    let f: Vec<_> = text.split_ascii_whitespace().collect();
    if f.len() != 7 || f[0] != "kwil-window-v1" {
        return Err(fail());
    }
    let p = Placement {
        x: f[1].parse().map_err(|_| fail())?,
        y: f[2].parse().map_err(|_| fail())?,
        width: f[3].parse().map_err(|_| fail())?,
        height: f[4].parse().map_err(|_| fail())?,
        scale: f[5].parse().map_err(|_| fail())?,
        maximized: match f[6] {
            "0" => false,
            "1" => true,
            _ => return Err(fail()),
        },
    };
    if p.width == 0
        || p.height == 0
        || p.width > 100_000
        || p.height > 100_000
        || !p.scale.is_finite()
        || !(0.25..=8.0).contains(&p.scale)
    {
        return Err(fail());
    }
    Ok(p)
}
fn load(path: &Path) -> io::Result<Option<Placement>> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut bytes = Vec::with_capacity(257);
    file.take(257).read_to_end(&mut bytes)?;
    if bytes.len() > 256 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "Saved window placement exceeds 256 bytes"));
    }
    decode(&bytes).map(Some)
}
fn store(path: &Path, p: Placement) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| io::Error::other("Window state has no parent"))?;
    std::fs::create_dir_all(parent)?;
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temporary = parent.join(format!(".kwil-window-{}-{sequence}.tmp", std::process::id()));
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&temporary)?;
    let result = (|| {
        file.write_all(encode(p).as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}
enum Request {
    Save(Placement),
    Stop(Option<Placement>),
}
pub(super) struct Service {
    sender: SyncSender<Request>,
    thread: JoinHandle<Result<(), String>>,
}
impl Service {
    pub fn start(hidden: bool, proxy: HostProxy) -> io::Result<Self> {
        let (sender, rx) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new().name("kwil-window-placement".into()).spawn(move || {
            let path = if hidden {
                std::env::var_os("KWIL_WORKSPACE_ROOT")
                    .map(PathBuf::from)
                    .filter(|p| p.is_absolute())
                    .map(|p| p.join("window-state-v1"))
                    .ok_or_else(|| "Hidden window state requires an absolute isolated root".to_owned())
            } else {
                std::env::current_exe()
                    .map_err(|e| e.to_string())
                    .and_then(|exe| Settings::path(&exe).map_err(|e| e.to_string()))
                    .map(|p| p.with_file_name("window-state-v1"))
                    .map_err(|e| e.to_string())
            };
            let loaded = path.as_ref().map_err(Clone::clone).and_then(|p| load(p).map_err(|e| e.to_string()));
            let valid = loaded.is_ok();
            let _ = proxy.send_event(UserEvent::WindowPlacementLoaded(loaded));
            let mut pending = None;
            let mut error = None;
            loop {
                let next = if pending.is_some() {
                    rx.recv_timeout(Duration::from_millis(150))
                } else {
                    rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected)
                };
                match next {
                    Ok(Request::Save(value)) => {
                        pending = Some(value);
                        let _ = proxy.send_event(UserEvent::WindowPlacementProgress);
                    }
                    Ok(Request::Stop(value)) => {
                        pending = value.or(pending);
                        if let Some(value) = pending
                            && valid
                            && let Ok(path) = &path
                        {
                            store(path, value).map_err(|e| e.to_string())?;
                        }
                        break;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if let Some(value) = pending.take()
                            && valid
                            && let Ok(path) = &path
                            && let Err(e) = store(path, value)
                        {
                            error = Some(e.to_string());
                            let _ = proxy
                                .send_event(UserEvent::Notice(format!("Window placement could not be saved: {e}")));
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            error.map_or(Ok(()), Err)
        })?;
        Ok(Self { sender, thread })
    }
    pub fn submit(&self, value: Placement) -> Result<(), Placement> {
        self.sender.try_send(Request::Save(value)).map_err(|e| match e {
            TrySendError::Full(Request::Save(p)) | TrySendError::Disconnected(Request::Save(p)) => p,
            _ => unreachable!(),
        })
    }
    pub fn stop(self, latest: Option<Placement>) -> Result<(), String> {
        self.sender.send(Request::Stop(latest)).map_err(|_| "Window placement worker disconnected".to_owned())?;
        self.thread.join().map_err(|_| "Window placement worker panicked".to_owned())?
    }
}
impl App {
    pub(super) fn remember_placement(&mut self) {
        let Some(gfx) = self.gfx.as_ref() else {
            return;
        };
        let Some(mut value) = capture(&gfx.window) else {
            return;
        };
        if value.maximized {
            if let Some(normal) = self.window_normal {
                value = Placement { maximized: true, ..normal };
            } else {
                return;
            }
        } else {
            self.window_normal = Some(value);
        }
        self.window_latest = Some(value);
        if self.window_placement_loaded {
            self.flush_placement();
        }
    }
    fn flush_placement(&mut self) {
        if let (Some(service), Some(value)) = (&self.window_placement, self.window_latest)
            && service.submit(value).is_ok()
        {
            self.window_latest = None;
        }
    }
    pub(super) fn placement_loaded(&mut self, result: Result<Option<Placement>, String>) {
        self.window_placement_loaded = true;
        let untouched =
            !self.window_touched && self.gfx.as_ref().and_then(|g| g.native.as_ref()).is_none_or(|n| !n.user_placed());
        match result {
            Ok(Some(saved)) if untouched => {
                self.window_latest = None;
                if let Some(gfx) = &self.gfx {
                    let monitor = gfx.window.available_monitors().min_by_key(|m| {
                        let p = m.position();
                        let s = m.size();
                        let dx =
                            i64::from(saved.x) + i64::from(saved.width) / 2 - i64::from(p.x) - i64::from(s.width) / 2;
                        let dy =
                            i64::from(saved.y) + i64::from(saved.height) / 2 - i64::from(p.y) - i64::from(s.height) / 2;
                        i128::from(dx) * i128::from(dx) + i128::from(dy) * i128::from(dy)
                    });
                    if let Some(monitor) = monitor {
                        let saved = saved.clamp(work(&monitor));
                        self.window_normal = Some(Placement { maximized: false, ..saved });
                        fit_window(&gfx.window, saved);
                    }
                }
            }
            Err(error) => {
                self.view.set_notice(format!("Window placement unavailable: {error}"));
                self.window_latest = None;
            }
            _ => {}
        }
    }
    pub(super) fn placement_progress(&mut self) {
        self.flush_placement();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clamp_handles_removed_monitor_negative_origin_and_dpi_change() {
        let work = Work { x: -1920, y: 40, width: 1920, height: 1040, scale: 1.5 };
        let p = Placement { x: 5000, y: -4000, width: 1400, height: 900, scale: 1.0, maximized: true }.clamp(work);
        assert_eq!((p.x, p.y, p.width, p.height), (-1920, 40, 1920, 1040));
        assert!(p.maximized);
        let centered = Placement::centered(
            work,
            Some(Placement { x: 0, y: 0, width: 800, height: 600, scale: 1.5, maximized: false }),
            (1080., 780.),
        );
        assert!(
            centered.x >= work.x
                && centered.y >= work.y
                && i64::from(centered.x) + i64::from(centered.width) <= 0
                && centered.y + centered.height as i32 <= 1080
        );
    }
    #[test]
    fn corrupt_unbounded_and_nonfinite_geometry_is_rejected() {
        let p = Placement { x: -1500, y: 200, width: 1200, height: 800, scale: 1.5, maximized: true };
        assert_eq!(decode(encode(p).as_bytes()).unwrap(), p);
        for text in [
            "kwil-window-v1 0 0 0 800 1 0",
            "kwil-window-v1 0 0 800 800 NaN 0",
            "kwil-window-v1 0 0 800 800 1 3",
            "kwil-window-v1 0 0 800 800 1 0 extra",
        ] {
            assert!(decode(text.as_bytes()).is_err());
        }
    }
    #[test]
    fn stored_outer_bounds_and_maximized_flag_survive_replacement() {
        let root = std::env::temp_dir().join(format!(
            "kwil-placement-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("state");
        let a = Placement { x: -1800, y: 70, width: 1200, height: 900, scale: 1.25, maximized: false };
        store(&path, a).unwrap();
        assert_eq!(load(&path).unwrap(), Some(a));
        let b = Placement { maximized: true, width: 1300, ..a };
        store(&path, b).unwrap();
        assert_eq!(load(&path).unwrap(), Some(b));
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1, "successful replacement retains no temporary files");
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&root).unwrap();
    }
}
