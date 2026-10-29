//! About is an owned desktop dialog; diagnostics never include paths or document text.
use super::*;
#[derive(Default)]
pub(super) struct About {
    pub open: bool,
    pub platform: Option<String>,
    loading: bool,
    thread: Option<std::thread::JoinHandle<()>>,
    pub license: bool,
    hits: Vec<(Rect, Action)>,
    pub notice: String,
    scroll: usize,
}
#[derive(Clone, Copy)]
pub(super) enum Action {
    Website,
    Source,
    License,
    Copy,
}
const WEBSITE: &str = "https://github.com/rennerdo30/kwil";
const SOURCE: &str = "https://github.com/rennerdo30/kwil";
impl About {
    pub fn start(&mut self, proxy: &HostProxy) {
        self.open = true;
        if self.loading || self.platform.is_some() {
            return;
        }
        self.loading = true;
        let proxy = proxy.clone();
        self.thread = std::thread::Builder::new()
            .name("kwil-diagnostics".into())
            .spawn(move || {
                let _ = proxy.send_event(UserEvent::DiagnosticsReady(format!(
                    "{}; {} logical processors",
                    platform_version(),
                    std::thread::available_parallelism().map_or(1, usize::from)
                )));
            })
            .ok();
    }
    pub fn shutdown(&mut self) -> Option<shutdown::Job> {
        self.thread.take().map(|thread| {
            Box::new(move || thread.join().map_err(|_| "Diagnostics worker panicked".into())) as shutdown::Job
        })
    }
    pub fn hit(&self, point: (usize, usize)) -> Option<Action> {
        self.hits.iter().find(|(r, _)| r.contains(point)).map(|(_, a)| *a)
    }
}
fn platform_version() -> String {
    #[cfg(windows)]
    {
        #[repr(C)]
        struct Version {
            size: u32,
            major: u32,
            minor: u32,
            build: u32,
            platform: u32,
            service: [u16; 128],
        }
        #[link(name = "ntdll")]
        unsafe extern "system" {
            fn RtlGetVersion(version: *mut Version) -> i32;
        }
        let mut v = Version {
            size: std::mem::size_of::<Version>() as u32,
            major: 0,
            minor: 0,
            build: 0,
            platform: 0,
            service: [0; 128],
        };
        if unsafe { RtlGetVersion(&mut v) } >= 0 {
            return format!(
                "Windows {} ({}.{} build {})",
                if v.build >= 22000 { "11" } else { "10" },
                v.major,
                v.minor,
                v.build
            );
        }
    }
    #[cfg(target_os = "linux")]
    {
        use std::io::Read;
        let mut bytes = String::new();
        if std::fs::File::open("/proc/sys/kernel/osrelease")
            .and_then(|f| f.take(256).read_to_string(&mut bytes))
            .is_ok()
        {
            return format!("Linux {}", bytes.trim());
        }
    }
    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            fn sysctlbyname(
                name: *const std::ffi::c_char,
                old: *mut std::ffi::c_void,
                len: *mut usize,
                new: *const std::ffi::c_void,
                newlen: usize,
            ) -> i32;
        }
        let mut text = [0u8; 128];
        let mut len = text.len();
        if unsafe {
            sysctlbyname(c"kern.osproductversion".as_ptr(), text.as_mut_ptr().cast(), &mut len, std::ptr::null(), 0)
        } == 0
        {
            let end = text.iter().position(|c| *c == 0).unwrap_or(text.len());
            return format!("macOS {}", String::from_utf8_lossy(&text[..end]));
        }
    }
    format!("{} (version unavailable)", std::env::consts::OS)
}
impl App {
    pub(super) fn about_renderer(&self) -> String {
        #[cfg(all(windows, feature = "gpu"))]
        if !self.policy.remote()
            && self.opts.renderer.unwrap_or(self.settings.render.backend) != RenderBackend::Cpu
            && let Some(gpu) = &self.gpu
        {
            return format!("GPU — DirectX 12 / DXGI Present1 ({})", gpu.adapter());
        }
        #[cfg(all(not(windows), feature = "gpu"))]
        if !self.policy.remote()
            && self.opts.renderer.unwrap_or(self.settings.render.backend) != RenderBackend::Cpu
            && let Some(gpu) = &self.gpu
        {
            return format!(
                "GPU — {} ({})",
                if cfg!(target_os = "macos") { "Metal" } else { "Vulkan/GLES" },
                gpu.adapter()
            );
        }
        format!("CPU — softbuffer{}", if self.policy.remote() { " (remote-session policy)" } else { "" })
    }
    pub(super) fn diagnostics(&self) -> String {
        format!(
            "Kwil {}\nBuild: {}\nBuild profile: {}\nPlatform: {}\nArchitecture: {} ({}-bit)\nRenderer: {}\nRenderer requested: {:?}\nRemote mode: {}\nDisplay scale: {:.0}%\nEditor zoom: {}%\nWebsite: {}\nSource: {}\nLicense: MIT\n",
            env!("CARGO_PKG_VERSION"),
            option_env!("KWIL_BUILD_COMMIT").unwrap_or("development"),
            if cfg!(debug_assertions) { "debug" } else { "release" },
            self.about.platform.as_deref().unwrap_or("Collecting OS version…"),
            std::env::consts::ARCH,
            usize::BITS,
            self.about_renderer(),
            self.opts.renderer.unwrap_or(self.settings.render.backend),
            self.policy.remote(),
            self.ui.scale * 100.0,
            self.zoom.percent(),
            WEBSITE,
            SOURCE
        )
    }
    pub(super) fn about_action(&mut self, action: Action) {
        match action {
            Action::License => {
                self.about.license = !self.about.license;
                self.about.scroll = 0;
            }
            Action::Copy => {
                if self.opts.hidden {
                    self.about.notice = "Clipboard is disabled in hidden tests".into();
                } else {
                    use kwil_view_model::Clipboard;
                    let hwnd = self.desktop_window.as_ref().map_or(0, |d| d.gfx.hwnd);
                    self.about.notice = match kwil_view_model::ClipboardData::text(self.diagnostics().as_bytes())
                        .and_then(|data| clipboard::SystemClipboard(hwnd).set(data))
                    {
                        Ok(()) => "Diagnostics copied".into(),
                        Err(error) => error.to_string(),
                    };
                }
            }
            Action::Website | Action::Source => {
                if !self.opts.hidden {
                    open_url(if matches!(action, Action::Website) { WEBSITE } else { SOURCE });
                }
            }
        }
    }
}
fn open_url(url: &str) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};
        let wide: Vec<u16> = url.encode_utf16().chain(Some(0)).collect();
        unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                std::ptr::null(),
                wide.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            );
        }
    }
    #[cfg(not(windows))]
    let _ = url;
}
#[allow(clippy::too_many_arguments)]
pub(super) fn draw(
    state: &mut About,
    diagnostics: &str,
    pixels: &mut [u32],
    size: (usize, usize),
    font: &mut font::UiFont,
    ui: &chrome::Metrics,
    theme: &kwil_core_commands::theme::Theme,
) {
    let colors = desktop::Colors::new(theme);
    let sizes = desktop::Sizes::new(ui);
    let pad = ui.dip(24.0).min(size.0 / 8);
    let line = ui.dip(25.0);
    pixels.fill(colors.surface);
    state.hits.clear();
    let logo = include_bytes!("icon.rgba");
    let edge = ui.dip(52.0).min(size.0 / 5).max(1);
    for y in 0..edge.min(size.1.saturating_sub(pad)) {
        for x in 0..edge {
            let i = ((y * 32 / edge) * 32 + x * 32 / edge) * 4;
            let to = (pad + y) * size.0 + pad + x;
            let color = (u32::from(logo[i]) << 16) | (u32::from(logo[i + 1]) << 8) | u32::from(logo[i + 2]);
            pixels[to] = font::blend(pixels[to], color, logo[i + 3]);
        }
    }
    let title = if state.license { "Licenses" } else { "Kwil" };
    let heading = font.heading();
    heading.draw(
        pixels,
        size.0,
        Rect { x: pad + edge + pad, y: pad, w: size.0.saturating_sub(3 * pad + edge), h: edge },
        pad + edge + pad,
        heading.baseline(pad, edge),
        title,
        colors.fg,
    );
    let footer = line + 2 * pad;
    let top = pad + edge + pad;
    let bottom = size.1.saturating_sub(footer);
    let available = bottom.saturating_sub(top) / line.max(1);
    let text = if state.license {
        format!(
            "{}\n\nThird-party components\nOS fonts are not redistributed.\n\n{}\n\n{}",
            include_str!("../../../LICENSE"),
            include_str!(concat!(env!("OUT_DIR"), "/third-party-notices.txt")),
            include_str!("../../core-syntax/LICENSES.txt")
        )
    } else {
        diagnostics.to_owned()
    };
    let lines: Vec<String> =
        text.lines().flat_map(|line| desktop::wrap(font, line, size.0.saturating_sub(2 * pad), 8)).collect();
    state.scroll = state.scroll.min(lines.len().saturating_sub(available));
    for (index, line_text) in lines.iter().skip(state.scroll).take(available).enumerate() {
        desktop::label(
            pixels,
            size.0,
            font,
            Rect { x: pad, y: top + index * line, w: size.0.saturating_sub(2 * pad), h: line },
            line_text,
            colors.fg,
        );
    }
    let mut x = pad;
    let y = size.1.saturating_sub(pad + sizes.control);
    for (label, action) in [
        ("Website", Action::Website),
        ("Source", Action::Source),
        (if state.license { "Diagnostics" } else { "Licenses" }, Action::License),
        ("Copy diagnostics", Action::Copy),
    ] {
        let width = desktop::button_width(font, label, &sizes);
        if x + width > size.0.saturating_sub(pad) {
            break;
        }
        let r = Rect { x, y, w: width, h: sizes.control.min(size.1.saturating_sub(y)) };
        desktop::button(pixels, size.0, font, r, label, desktop::Style::Secondary, false, &colors, &sizes);
        state.hits.push((r, action));
        x += width + sizes.gap;
    }
}
pub(super) fn wheel(state: &mut About, up: bool) {
    state.scroll = state.scroll.saturating_add_signed(if up { -3 } else { 3 });
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn platform_diagnostics_are_bounded_and_omit_owner_paths() {
        let text = platform_version();
        assert!(!text.is_empty());
        assert!(text.len() < 256);
        assert!(!text.contains("AgentData"));
    }
}
