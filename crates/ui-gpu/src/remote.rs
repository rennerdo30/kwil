use kwil_core_commands::settings::{Remote, RenderSettings};

#[derive(Default)]
pub(crate) struct SmoothScroll {
    remaining: i64,
    pub next: Option<std::time::Instant>,
    context: (u64, u64),
}
impl SmoothScroll {
    pub fn push(&mut self, context: (u64, u64), delta: i64, now: std::time::Instant) {
        if self.context != context {
            self.clear();
            self.context = context;
        }
        self.remaining = self.remaining.saturating_add(delta).clamp(-120, 120);
        self.next = (self.remaining != 0).then_some(now);
    }
    pub fn clear(&mut self) {
        self.remaining = 0;
        self.next = None;
    }
    pub fn take(&mut self, context: (u64, u64), allowed: bool, now: std::time::Instant) -> i64 {
        if !allowed || self.context != context {
            self.clear();
            return 0;
        }
        if self.next.is_none_or(|next| next > now) {
            return 0;
        }
        let step = self.remaining.signum() * ((self.remaining.abs() + 3) / 4);
        self.remaining -= step;
        self.next = (self.remaining != 0).then_some(now + std::time::Duration::from_millis(16));
        step
    }
}

#[derive(Clone, Debug)]
pub struct RenderPolicy {
    settings: RenderSettings,
    detected: bool,
}
impl RenderPolicy {
    pub fn new(settings: RenderSettings, detected: bool) -> Self {
        Self { settings, detected }
    }
    pub fn remote(&self) -> bool {
        match self.settings.remote {
            Remote::Auto => self.detected,
            Remote::On => true,
            Remote::Off => false,
        }
    }
    pub fn session_changed(&mut self, detected: bool) {
        self.detected = detected;
    }
    pub fn detected(&self) -> bool {
        self.detected
    }
    pub fn cursor_blink_allowed(&self) -> bool {
        self.settings.cursor_blink && !self.remote()
    }
    pub fn smooth_scrolling_allowed(&self) -> bool {
        self.settings.smooth_scrolling && !self.remote()
    }
    pub fn translucency_allowed(&self) -> bool {
        !self.remote()
    }
}

/// Injectable hints: no environment writes and no display/server connections.
pub fn linux_remote_hints(get: impl Fn(&str) -> Option<String>) -> bool {
    ["XRDP_SESSION", "XRDP_SOCKET_PATH", "VNCSESSION", "VNCDESKTOP", "VNC_SESSION", "X2GO_SESSION"]
        .iter()
        .any(|k| get(k).is_some_and(|v| !v.is_empty()))
        || get("XDG_SESSION_TYPE").is_some_and(|v| v.eq_ignore_ascii_case("xrdp") || v.eq_ignore_ascii_case("vnc"))
}
pub(crate) fn detect() -> bool {
    #[cfg(windows)]
    {
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                windows_sys::Win32::UI::WindowsAndMessaging::SM_REMOTESESSION,
            ) != 0
        }
    }
    #[cfg(target_os = "linux")]
    {
        linux_remote_hints(|k| std::env::var(k).ok())
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        false
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn kw46_runtime_smooth_scrolling_uses_policy_and_stops_without_idle_wakes() {
        let now = std::time::Instant::now();
        let mut scroll = SmoothScroll::default();
        let policy = RenderPolicy::new(RenderSettings { smooth_scrolling: true, ..Default::default() }, false);
        scroll.push((1, 2), 3, now);
        assert_eq!(scroll.take((1, 2), policy.smooth_scrolling_allowed(), now), 1);
        assert!(scroll.next.is_some());
        let mut remote = policy.clone();
        remote.session_changed(true);
        assert_eq!(scroll.take((1, 2), remote.smooth_scrolling_allowed(), now), 0);
        assert!(scroll.next.is_none());
        scroll.push((1, 2), 3, now);
        let sum: i64 = (0..4).map(|i| scroll.take((1, 2), true, now + std::time::Duration::from_millis(i * 16))).sum();
        assert_eq!(sum, 3);
        assert!(scroll.next.is_none());
    }
    #[test]
    fn overrides_and_live_session_change() {
        for (setting, detected, expected) in [
            (Remote::Auto, false, false),
            (Remote::Auto, true, true),
            (Remote::On, false, true),
            (Remote::Off, true, false),
        ] {
            let p = RenderPolicy::new(RenderSettings { remote: setting, ..Default::default() }, detected);
            assert_eq!(p.remote(), expected);
        }
        let mut p = RenderPolicy::new(RenderSettings::default(), false);
        p.session_changed(true);
        assert!(p.remote());
        p.session_changed(false);
        assert!(!p.remote());
    }
    #[test]
    fn remote_disables_motion_blink_and_translucency() {
        let p = RenderPolicy::new(
            RenderSettings { remote: Remote::On, smooth_scrolling: true, cursor_blink: true, ..Default::default() },
            false,
        );
        assert!(!p.cursor_blink_allowed());
        assert!(!p.smooth_scrolling_allowed());
        assert!(!p.translucency_allowed());
        let p = RenderPolicy::new(RenderSettings { cursor_blink: false, ..Default::default() }, false);
        assert!(!p.cursor_blink_allowed());
    }
    #[test]
    fn linux_hints_do_not_mistake_local_wayland_for_remote() {
        assert!(!linux_remote_hints(|_| None));
        assert!(!linux_remote_hints(|k| (k == "XDG_SESSION_TYPE").then(|| "wayland".into())));
        assert!(linux_remote_hints(|k| (k == "XRDP_SESSION").then(|| "1".into())));
        assert!(linux_remote_hints(|k| (k == "VNCDESKTOP").then(|| "desktop".into())));
    }
}
