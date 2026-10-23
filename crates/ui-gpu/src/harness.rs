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
    MenuOpen,
    MenuClose,
    ZoomIn,
    InputOwnership,
    UsabilityCapture(&'static str),
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
        let usual = [
            (2000, Step::Idle),
            (250, Step::Scroll),
            (200, Step::Repaint),
            (150, Step::Status),
            (150, Step::RemoteOn),
            (100, Step::RemoteOff),
            (100, Step::End),
        ];
        let usability = [
            (2000, Step::Idle),
            (100, Step::InputOwnership),
            (100, Step::MenuOpen),
            (150, Step::UsabilityCapture("menu.bmp")),
            (100, Step::MenuClose),
            (100, Step::ZoomIn),
            (200, Step::ZoomIn),
            (200, Step::ZoomIn),
            (400, Step::UsabilityCapture("zoom.bmp")),
            (100, Step::End),
        ];
        let steps = if std::env::var_os("KWIL_UI_USABILITY_CAPTURE").is_some() {
            usability.as_slice()
        } else {
            usual.as_slice()
        };
        for &(delay, step) in steps {
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
            // Prime the retained GPU image without an onscreen present, so the
            // following scroll/readback exercises GPU copies, not a fresh upload.
            #[cfg(feature = "gpu")]
            if let (Some(gpu), Some(gfx), Some(font)) = (&mut app.gpu, &app.gfx, &app.font) {
                gpu.update(
                    &gfx.pixels,
                    app.size,
                    &[super::Rect { x: 0, y: 0, w: app.size.0, h: app.size.1 }],
                    0,
                    font.ch,
                    app.view.rows(),
                );
            }
            #[cfg(windows)]
            if let Some(gfx) = &app.gfx {
                native_probe(gfx.hwnd, dir.as_deref());
            }
            println!("kwil-frame glyphs_pending={}", app.font.as_ref().map_or(0, |f| f.pending_glyphs()));
            let unicode_enabled = app.font.as_ref().is_some_and(|f| f.unicode_enabled());
            println!("kwil-frame unicode_enabled={unicode_enabled}");
            if std::env::var_os("KWIL_REQUIRE_UNICODE").is_some() {
                assert!(unicode_enabled, "Unicode recovery unavailable");
            }
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
        Step::MenuOpen => {
            let available = app.menu_available();
            app.menu_set(super::menu::State::open(0, &|id| super::menu::enabled(&available, id)), true);
        }
        Step::InputOwnership => {
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| input_ownership(app, el, dir.as_deref())));
            if let Err(payload) = result {
                if let Some(dir) = &dir {
                    let message = payload
                        .downcast_ref::<String>()
                        .map(String::as_str)
                        .or_else(|| payload.downcast_ref::<&str>().copied())
                        .unwrap_or("non-string assertion payload");
                    let _ = std::fs::write(dir.join("input-ownership-failure.txt"), message);
                }
                std::panic::resume_unwind(payload);
            }
        }
        Step::MenuClose => app.menu_set(super::menu::State::Closed, false),
        Step::ZoomIn => {
            app.dispatch_zoom("view.zoomIn");
        }
        Step::UsabilityCapture(name) => {
            let metadata = format!(
                "kwil-ui capture={name} scale={} menu={} tabs={} status={} bar={} thumb={} zoom={} editor_cell={:?}",
                app.ui.scale,
                app.ui.menu,
                app.ui.tabs,
                app.ui.status,
                app.ui.bar,
                app.ui.min_thumb,
                app.zoom.percent(),
                app.font.as_ref().map(|font| (font.cw, font.ch)),
            );
            println!("{metadata}");
            if let Some(dir) = &dir {
                std::fs::write(dir.join(format!("{name}.txt")), metadata).expect("capture metadata write");
            }
            capture(app, dir.as_deref(), name);
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
fn input_ownership(app: &mut App, el: &ActiveEventLoop, dir: Option<&Path>) {
    use winit::application::ApplicationHandler;
    use winit::event::{ElementState, Ime, MouseButton, WindowEvent};
    let window = app.gfx.as_ref().unwrap().window.id();
    let arm = |app: &mut App| {
        let body = app.body_area();
        let offset = app.view.top();
        let bar = super::scrollbar::Bar::sized(0, body.height, 40, 20_000_000, offset, 48);
        app.drag = Some(super::scrollbar::Drag::new(true, bar, bar.thumb_start + 20, offset));
    };
    // Exercise the real App routes, not only the integer geometry object.
    let scale = app.ui.scale;
    arm(app);
    app.monitor_scale_changed(scale + 0.25);
    assert!(app.drag.is_none(), "DPI change must release the old coordinate anchor");
    app.monitor_scale_changed(scale);
    let settings = app.settings.clone();
    let mut changed = settings.clone();
    changed.tabs.layout = match settings.tabs.layout {
        kwil_core_commands::settings::TabLayout::Horizontal => kwil_core_commands::settings::TabLayout::Vertical,
        kwil_core_commands::settings::TabLayout::Vertical => kwil_core_commands::settings::TabLayout::Horizontal,
    };
    arm(app);
    app.user_event(el, UserEvent::SettingsReady(Box::new(changed)));
    assert!(app.drag.is_none(), "layout change must release the old coordinate anchor");
    app.user_event(el, UserEvent::SettingsReady(Box::new(settings)));
    let size = app.size;
    arm(app);
    app.window_event(el, window, WindowEvent::Resized(winit::dpi::PhysicalSize::new(size.0 as u32 + 1, size.1 as u32)));
    assert!(app.drag.is_none(), "resize must release the old coordinate anchor");
    app.window_event(el, window, WindowEvent::Resized(winit::dpi::PhysicalSize::new(size.0 as u32, size.1 as u32)));

    let before = app.view.editor().map(|editor| (editor.document().revision(), editor.cursors().to_vec()));
    // Find is explicitly disabled until the search host is integrated. Its click must remain
    // owned by the dropdown while an IME attempts to compose behind it.
    assert!(!super::menu::enabled(&app.menu_available(), "search.find"));
    app.menu_set(super::menu::State::Open { menu: 4, item: None, keyboard: false }, true);
    assert!(app.render());
    let row = app.menu.dropdown.as_ref().unwrap().rows[0].1;
    app.window_event(el, window, WindowEvent::Ime(Ime::Preedit("日本語".into(), None)));
    assert!(!app.composing, "cancelled composition cannot suspend menu ownership");
    app.pointer = (row.x + 2, row.y + 2);
    app.window_event(
        el,
        window,
        WindowEvent::MouseInput {
            device_id: winit::event::DeviceId::dummy(),
            state: ElementState::Pressed,
            button: MouseButton::Left,
        },
    );
    assert!(app.menu.state.is_open(), "disabled dropdown row click must not fall through");
    app.window_event(el, window, WindowEvent::Ime(Ime::Commit("unexpected edit".into())));
    assert_eq!(app.view.editor().map(|editor| (editor.document().revision(), editor.cursors().to_vec())), before);
    app.menu_set(super::menu::State::Closed, false);
    if let Some(dir) = dir {
        std::fs::write(dir.join("input-ownership.txt"), "PASS: App DPI/layout/resize drag cancellation; open menu Preedit/click/Commit retains document and cursors\n").unwrap();
    }
}

fn capture(app: &App, dir: Option<&Path>, name: &str) {
    if let (Some(dir), Some(gfx)) = (dir, &app.gfx) {
        #[cfg(windows)]
        {
            if let Err(error) = capture_dc(gfx.hwnd, &dir.join(name), false) {
                eprintln!("capture failed: {error}");
            }
            if std::env::var_os("KWIL_CAPTURE_WINDOW_FRAME").is_some()
                && let Err(error) = capture_dc(gfx.hwnd, &dir.join(format!("frame-{name}")), true)
            {
                eprintln!("window frame capture failed: {error}");
            }
        }
        #[cfg(not(windows))]
        {
            let _ = (dir, gfx, name);
        }
    }
}
#[cfg(windows)]
fn native_probe(hwnd: isize, output: Option<&Path>) {
    use windows_sys::Win32::{
        Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute},
        UI::WindowsAndMessaging::*,
    };
    unsafe {
        let hwnd = hwnd as _;
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
        let required = WS_CAPTION | WS_THICKFRAME | WS_MINIMIZEBOX | WS_MAXIMIZEBOX;
        let controls = style & required == required;
        let small = SendMessageW(hwnd, WM_GETICON, ICON_SMALL as usize, 0) != 0;
        let big = SendMessageW(hwnd, WM_GETICON, ICON_BIG as usize, 0) != 0;
        let mut cloaked = 0u32;
        let hr = DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED as u32, (&mut cloaked as *mut u32).cast(), 4);
        let metadata = format!(
            "kwil-native caption_resize_min_max={controls} small_icon={small} taskbar_icon={big} cloaked={}",
            hr >= 0 && cloaked != 0
        );
        println!("{metadata}");
        if let Some(output) = output {
            std::fs::write(output.join("native-window.txt"), metadata).expect("native window metadata write");
        }
    }
}
// Based on the admitted M0 window-DC capture; the HWND comes directly from the
// owned window, never FindWindow/title guessing. No GPU/PrintWindow claim here.
#[cfg(windows)]
fn capture_dc(hwnd: isize, path: &Path, frame: bool) -> std::io::Result<()> {
    use windows_sys::Win32::{
        Foundation::RECT,
        Graphics::Gdi::*,
        UI::WindowsAndMessaging::{GetClientRect, GetWindowRect},
    };
    unsafe {
        let hwnd = hwnd as _;
        if frame {
            use windows_sys::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
            let mut cloaked = 0u32;
            let result = DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED as u32, (&mut cloaked as *mut u32).cast(), 4);
            if result < 0 || cloaked != 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "native caption pixels are unavailable from a DWM-cloaked window",
                ));
            }
        }
        let mut r: RECT = std::mem::zeroed();
        let measured = if frame { GetWindowRect(hwnd, &mut r) } else { GetClientRect(hwnd, &mut r) };
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        if measured == 0 || w <= 0 || h <= 0 {
            return Err(std::io::Error::other("window capture bounds unavailable"));
        }
        let dc = if frame { GetWindowDC(hwnd) } else { GetDC(hwnd) };
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
