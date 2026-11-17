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
    License,
    Copy,
}
/// Website and source are one place: the repository.
const WEBSITE: &str = "https://github.com/rennerdo30/kwil";
impl About {
    pub fn start(&mut self, proxy: &HostProxy) {
        self.open = true;
        self.notice.clear();
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
            return format!("GPU (DirectX 12) · {}", gpu.adapter());
        }
        #[cfg(all(not(windows), feature = "gpu"))]
        if !self.policy.remote()
            && self.opts.renderer.unwrap_or(self.settings.render.backend) != RenderBackend::Cpu
            && let Some(gpu) = &self.gpu
        {
            return format!(
                "GPU ({}) · {}",
                if cfg!(target_os = "macos") { "Metal" } else { "Vulkan/GLES" },
                gpu.adapter()
            );
        }
        if self.policy.remote() { "CPU (remote session)".into() } else { "CPU (software)".into() }
    }
    /// What About shows and Copy diagnostics copies, in reading order. Values
    /// are worded for people; paths and document text never appear.
    pub(super) fn about_facts(&self) -> Vec<(&'static str, String)> {
        let on_off = |on: bool| if on { "on" } else { "off" }.to_owned();
        let requested = match self.opts.renderer.unwrap_or(self.settings.render.backend) {
            RenderBackend::Auto => "Automatic",
            RenderBackend::Gpu => "GPU",
            RenderBackend::Cpu => "CPU",
        };
        vec![
            (
                "Build",
                format!(
                    "{} ({})",
                    // A 12-digit commit prefix identifies the build and stays readable.
                    option_env!("KWIL_BUILD_COMMIT").map_or("development", |commit| &commit[..commit.len().min(12)]),
                    if cfg!(debug_assertions) { "debug" } else { "release" }
                ),
            ),
            ("Platform", self.about.platform.clone().unwrap_or_else(|| "Collecting OS version…".into())),
            ("Architecture", format!("{} ({}-bit)", std::env::consts::ARCH, usize::BITS)),
            ("Renderer", self.about_renderer()),
            ("Renderer setting", requested.into()),
            ("Remote mode", on_off(self.policy.remote())),
            ("Display scale", format!("{:.0}%", self.ui.scale * 100.0)),
            ("Editor zoom", format!("{}%", self.zoom.percent())),
            ("Website", WEBSITE.trim_start_matches("https://").into()),
            ("License", "MIT".into()),
        ]
    }
    /// Copied into bug reports, so it keeps the full commit and the full URL
    /// that the screen shortens.
    pub(super) fn diagnostics(&self) -> String {
        let mut text = format!("Kwil {}\n", env!("CARGO_PKG_VERSION"));
        for (key, value) in self.about_facts() {
            let value = match key {
                "Build" => match option_env!("KWIL_BUILD_COMMIT") {
                    Some(commit) => value.replacen(&commit[..commit.len().min(12)], commit, 1),
                    None => value,
                },
                "Website" => WEBSITE.into(),
                _ => value,
            };
            text.push_str(&format!("{key}: {value}\n"));
        }
        text
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
                        Ok(()) => "Copied".into(),
                        Err(error) => error.to_string(),
                    };
                }
            }
            Action::Website => {
                if !self.opts.hidden {
                    open_url(WEBSITE);
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
/// One visual line of the about body: a grid row (muted key, value) or a
/// continuation of a wrapped value or licence text (no key).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Line {
    pub key: Option<&'static str>,
    pub value: String,
}

/// Key/value grid: keys share one muted column sized to the widest key, values
/// start at one x and wrap (never cut) inside the rest of the width.
pub(super) fn grid(
    font: &mut font::UiFont,
    facts: &[(&'static str, String)],
    width: usize,
    gap: usize,
) -> (usize, Vec<Line>) {
    // Keys are never cut: the column fits the widest key, capped at half the
    // card only on very narrow windows (platform faces differ in width).
    let keys = facts.iter().map(|(key, _)| font.measure(key, width / 2)).max().unwrap_or(0);
    let column = keys + gap;
    let mut lines = Vec::new();
    for (key, value) in facts {
        for (index, part) in desktop::wrap(font, value, width.saturating_sub(column), 4).into_iter().enumerate() {
            lines.push(Line { key: (index == 0).then_some(*key), value: part });
        }
    }
    (column, lines)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn draw(
    state: &mut About,
    facts: &[(&'static str, String)],
    pixels: &mut [u32],
    size: (usize, usize),
    font: &mut font::UiFont,
    ui: &chrome::Metrics,
    theme: &kwil_core_commands::theme::Theme,
    pointer: (usize, usize),
) {
    let colors = desktop::Colors::new(theme);
    let sizes = desktop::Sizes::new(ui);
    let pad = sizes.xl.min(size.0 / 8);
    let line = ui.dip(chrome::tok::TEXT + chrome::tok::SM);
    pixels.fill(colors.surface);
    state.hits.clear();
    // Identity first: the logo, the name in the display face, the version under it.
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
    let text_x = pad + edge + sizes.lg;
    let head = Rect { x: text_x, y: pad, w: size.0.saturating_sub(text_x + pad), h: edge };
    if state.license {
        desktop::label(pixels, size.0, font.heading(), head, "Licenses", colors.fg);
    } else {
        let name_h = ui.dip(chrome::tok::DISPLAY + chrome::tok::XS).min(edge);
        desktop::label(pixels, size.0, font.heading(), Rect { h: name_h, ..head }, "Kwil", colors.fg);
        let version = Rect { y: head.y + name_h, h: edge - name_h, ..head };
        let text = format!("Version {}", env!("CARGO_PKG_VERSION"));
        desktop::label(pixels, size.0, font.title(), version, &text, colors.muted);
    }
    let footer = sizes.control + 2 * pad;
    let top = pad + edge + sizes.xl;
    let bottom = size.1.saturating_sub(footer);
    let available = bottom.saturating_sub(top) / line.max(1);
    let body_w = size.0.saturating_sub(2 * pad);
    let (column, lines) = if state.license {
        let text = format!(
            "{}\n\nThird-party components\nOS fonts are not redistributed.\n\n{}\n\n{}",
            include_str!("../../../LICENSE"),
            include_str!(concat!(env!("OUT_DIR"), "/third-party-notices.txt")),
            include_str!("../../core-syntax/LICENSES.txt")
        );
        let lines = text
            .lines()
            .flat_map(|line| desktop::wrap(font, line, body_w, 8))
            .map(|value| Line { key: None, value })
            .collect();
        (0, lines)
    } else {
        grid(font, facts, body_w, sizes.lg)
    };
    state.scroll = state.scroll.min(lines.len().saturating_sub(available));
    for (index, row) in lines.iter().skip(state.scroll).take(available).enumerate() {
        let y = top + index * line;
        if let Some(key) = row.key {
            let clip = Rect { x: pad, y, w: column, h: line };
            desktop::label(pixels, size.0, font, clip, key, colors.muted);
        }
        let clip = Rect { x: pad + column, y, w: body_w.saturating_sub(column), h: line };
        desktop::label(pixels, size.0, font, clip, &row.value, colors.fg);
    }
    // One action row: the main action first, then the rest; feedback after it.
    let mut x = pad;
    let y = size.1.saturating_sub(pad + sizes.control);
    for (label, action, style) in [
        ("Copy diagnostics", Action::Copy, desktop::Style::Primary),
        (if state.license { "Diagnostics" } else { "Licenses" }, Action::License, desktop::Style::Secondary),
        ("Website", Action::Website, desktop::Style::Secondary),
    ] {
        let width = desktop::button_width(font, label, &sizes);
        if x + width > size.0.saturating_sub(pad) {
            break;
        }
        let r = Rect { x, y, w: width, h: sizes.control.min(size.1.saturating_sub(y)) };
        desktop::button(pixels, size.0, font, r, label, style, r.contains(pointer), false, &colors, &sizes);
        state.hits.push((r, action));
        x += width + sizes.gap;
    }
    if !state.notice.is_empty() {
        let clip = Rect { x: x + sizes.sm, y, w: size.0.saturating_sub(x + sizes.sm + pad), h: sizes.control };
        desktop::label(pixels, size.0, font, clip, &state.notice, colors.muted);
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
    #[test]
    fn about_facts_are_human_and_name_the_website_once() {
        let (app, _events) = crate::search_ui::host_dispatch_tests::host("");
        let facts = app.about_facts();
        let keys: Vec<&str> = facts.iter().map(|(key, _)| *key).collect();
        let unique: std::collections::HashSet<&str> = keys.iter().copied().collect();
        assert_eq!(unique.len(), keys.len(), "no fact twice: {keys:?}");
        let value = |key: &str| facts.iter().find(|(k, _)| *k == key).map(|(_, v)| v.as_str());
        assert_eq!(value("Remote mode"), Some("off"));
        assert!(facts.iter().all(|(_, v)| v != "true" && v != "false"), "no raw booleans: {facts:?}");
        assert!(value("Renderer").is_some_and(|r| r.starts_with("GPU (") || r.starts_with("CPU (")));
        assert_eq!(value("Renderer setting"), Some("Automatic"));
        assert_eq!(value("Website"), Some("github.com/rennerdo30/kwil"));
        assert!(value("Source").is_none(), "website and source are one place");
        let text = app.diagnostics();
        assert!(text.starts_with(&format!("Kwil {}\n", env!("CARGO_PKG_VERSION"))));
        assert_eq!(text.matches("github.com/rennerdo30/kwil").count(), 1);
        assert!(text.contains("Build:") && text.contains("Renderer:") && text.contains("License: MIT"));
        assert!(text.contains(&format!("Website: {WEBSITE}\n")), "the copy keeps the full URL");
        if let Some(commit) = option_env!("KWIL_BUILD_COMMIT") {
            assert!(text.contains(commit), "the copy keeps the full commit, the screen shows 12 digits");
        }
    }

    #[test]
    fn about_grid_aligns_values_in_one_column_and_wraps_instead_of_cutting() {
        let Some(mut font) = font::UiFont::load(15.0) else { return };
        let long = "GPU (DirectX 12) · an adapter name long enough to need a second line here";
        let facts = vec![
            ("Build", "development (debug)".to_owned()),
            ("Renderer setting", "GPU".into()),
            ("Renderer", long.into()),
        ];
        let width = 360;
        let (column, lines) = grid(&mut font, &facts, width, 16);
        let widest = font.measure("Renderer setting", width);
        assert!(column >= widest + 16, "values start after the widest key");
        assert_eq!(lines.iter().filter_map(|l| l.key).collect::<Vec<_>>(), ["Build", "Renderer setting", "Renderer"]);
        let renderer = lines.iter().position(|l| l.key == Some("Renderer")).unwrap();
        assert!(lines.len() > renderer + 1, "a long value wraps onto a keyless line");
        let joined = lines[renderer..].iter().map(|l| l.value.as_str()).collect::<Vec<_>>().join(" ");
        assert_eq!(joined.split_whitespace().collect::<Vec<_>>(), long.split_whitespace().collect::<Vec<_>>());
        assert!(lines.iter().all(|l| !l.value.contains('…')), "nothing is cut");
        for line in &lines {
            assert!(font.measure(&line.value, usize::MAX) <= width - column, "{line:?} fits its column");
        }
    }

    #[test]
    fn about_draws_one_website_button_muted_keys_and_a_large_name() {
        let Some(mut font) = font::UiFont::load(15.0) else { return };
        let m = chrome::Metrics::new(1.0);
        let theme = theme_ui::chrome();
        let c = desktop::Colors::new(&theme);
        let size = (640, 520);
        let mut pixels = vec![0; size.0 * size.1];
        let mut state = About::default();
        let facts = vec![("Remote mode", "off".to_owned()), ("Website", "github.com/rennerdo30/kwil".into())];
        draw(&mut state, &facts, &mut pixels, size, &mut font, &m, &theme, (0, 0));
        let actions: Vec<_> = state.hits.iter().map(|(_, a)| *a).collect();
        assert_eq!(actions.iter().filter(|a| matches!(a, Action::Website)).count(), 1, "one website button");
        assert_eq!(actions.len(), 3, "copy, licences, website");
        // Ink colours per region: keys only muted, values only foreground.
        let s = desktop::Sizes::new(&m);
        let pad = s.xl;
        let edge = m.dip(52.0);
        let top = pad + edge + s.xl;
        let line = m.dip(chrome::tok::TEXT + chrome::tok::SM);
        let (column, _) = grid(&mut font, &facts, size.0 - 2 * pad, s.lg);
        let has = |x0: usize, x1: usize, y0: usize, y1: usize, color: u32| {
            (y0..y1).any(|y| pixels[y * size.0 + x0..y * size.0 + x1].contains(&color))
        };
        assert!(has(pad, pad + column, top, top + line, c.muted), "the key is muted");
        assert!(!has(pad, pad + column, top, top + line, c.fg), "the key is not foreground");
        assert!(has(pad + column, size.0 - pad, top, top + line, c.fg), "the value is foreground");
        // The name is drawn in the display face: taller ink than body text.
        let name_rows = (pad..pad + edge)
            .filter(|y| pixels[y * size.0 + pad + edge..y * size.0 + size.0 / 2].iter().any(|p| *p != c.surface));
        assert!(name_rows.count() > m.dip(chrome::tok::BODY), "name and version stand out");
    }
}
