//! Frontend selection without process-global state or window creation.

pub trait EnvLookup {
    fn has(&self, key: &str) -> bool;
}

pub struct ProcessEnv;
impl EnvLookup for ProcessEnv {
    fn has(&self, key: &str) -> bool {
        std::env::var_os(key).is_some_and(|v| !v.is_empty())
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Flags {
    pub gui: bool,
    pub tui: bool,
}

#[derive(Clone, Copy, Debug)]
pub enum Platform {
    UnixDisplay,
    MacOs,
    Windows { session: Option<u32>, interactive_station: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Frontend {
    Gui,
    Tui,
    Error(&'static str),
}

pub fn choose_frontend(env: &impl EnvLookup, flags: Flags, platform: Platform) -> Frontend {
    if flags.gui && flags.tui {
        return Frontend::Error("--gui and --tui exclude each other");
    }
    if flags.tui {
        return Frontend::Tui;
    }
    let interactive = !matches!(
        platform,
        Platform::Windows { session: None | Some(0), .. } | Platform::Windows { interactive_station: false, .. }
    );
    if flags.gui {
        return if interactive {
            Frontend::Gui
        } else {
            Frontend::Error("this Windows session has no interactive desktop")
        };
    }
    let forwarded = env.has("DISPLAY") || env.has("WAYLAND_DISPLAY");
    let ssh = env.has("SSH_CONNECTION") || env.has("SSH_TTY") || env.has("SSH_CLIENT");
    if interactive && (forwarded || (!ssh && !matches!(platform, Platform::UnixDisplay))) {
        Frontend::Gui
    } else {
        Frontend::Tui
    }
}

/// A failed GUI can use the terminal only when both input and output are terminals.
pub fn after_gui_failure(terminal: bool) -> Frontend {
    if terminal {
        Frontend::Tui
    } else {
        Frontend::Error("GUI initialization failed and no interactive terminal is attached")
    }
}

pub fn current_platform() -> Platform {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
        use windows_sys::Win32::System::StationsAndDesktops::{
            GetProcessWindowStation, GetUserObjectInformationW, UOI_FLAGS, USEROBJECTFLAGS,
        };
        let mut session = 0;
        let mut flags: USEROBJECTFLAGS = unsafe { std::mem::zeroed() };
        let mut needed = 0;
        // Borrowed process window station; it must not be closed.
        let valid_session = unsafe { ProcessIdToSessionId(std::process::id(), &mut session) } != 0;
        let station = unsafe { GetProcessWindowStation() };
        let valid_station = !station.is_null()
            && unsafe {
                GetUserObjectInformationW(
                    station,
                    UOI_FLAGS,
                    (&mut flags as *mut USEROBJECTFLAGS).cast(),
                    std::mem::size_of::<USEROBJECTFLAGS>() as u32,
                    &mut needed,
                )
            } != 0;
        Platform::Windows {
            session: valid_session.then_some(session),
            interactive_station: valid_station && flags.dwFlags & 1 != 0,
        }
    }
    #[cfg(target_os = "macos")]
    {
        Platform::MacOs
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        Platform::UnixDisplay
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Env<'a>(&'a [(&'a str, &'a str)]);
    impl EnvLookup for Env<'_> {
        fn has(&self, key: &str) -> bool {
            self.0.iter().any(|(k, v)| *k == key && !v.is_empty())
        }
    }
    fn check(vars: &[(&str, &str)], flags: Flags, platform: Platform, want: Frontend) {
        assert_eq!(choose_frontend(&Env(vars), flags, platform), want);
    }
    const WIN: Platform = Platform::Windows { session: Some(1), interactive_station: true };
    macro_rules! case {
        ($name:ident, $vars:expr, $platform:expr, $want:expr) => {
            #[test]
            fn $name() {
                check($vars, Flags::default(), $platform, $want);
            }
        };
    }
    case!(linux_wayland, &[("WAYLAND_DISPLAY", "wayland-0")], Platform::UnixDisplay, Frontend::Gui);
    case!(linux_x11, &[("DISPLAY", ":0")], Platform::UnixDisplay, Frontend::Gui);
    case!(linux_no_display, &[], Platform::UnixDisplay, Frontend::Tui);
    case!(linux_empty_display, &[("DISPLAY", ""), ("WAYLAND_DISPLAY", "")], Platform::UnixDisplay, Frontend::Tui);
    case!(linux_ssh_no_forwarding, &[("SSH_CONNECTION", "peer")], Platform::UnixDisplay, Frontend::Tui);
    case!(
        linux_ssh_x_forwarding,
        &[("SSH_CONNECTION", "peer"), ("DISPLAY", "localhost:10")],
        Platform::UnixDisplay,
        Frontend::Gui
    );
    case!(
        linux_ssh_wayland_forwarding,
        &[("SSH_TTY", "tty"), ("WAYLAND_DISPLAY", "wayland-0")],
        Platform::UnixDisplay,
        Frontend::Gui
    );
    case!(mac_desktop, &[], Platform::MacOs, Frontend::Gui);
    case!(mac_ssh, &[("SSH_TTY", "tty")], Platform::MacOs, Frontend::Tui);
    case!(
        mac_ssh_forwarding,
        &[("SSH_CONNECTION", "peer"), ("DISPLAY", "localhost:10")],
        Platform::MacOs,
        Frontend::Gui
    );
    case!(windows_desktop, &[], WIN, Frontend::Gui);
    case!(windows_ssh, &[("SSH_CONNECTION", "peer")], WIN, Frontend::Tui);
    case!(windows_ssh_client, &[("SSH_CLIENT", "peer")], WIN, Frontend::Tui);
    case!(windows_ssh_forwarding, &[("SSH_CONNECTION", "peer"), ("DISPLAY", "localhost:10")], WIN, Frontend::Gui);
    case!(windows_session_zero, &[], Platform::Windows { session: Some(0), interactive_station: true }, Frontend::Tui);
    case!(windows_session_unknown, &[], Platform::Windows { session: None, interactive_station: true }, Frontend::Tui);
    case!(
        windows_noninteractive_station,
        &[],
        Platform::Windows { session: Some(1), interactive_station: false },
        Frontend::Tui
    );
    #[test]
    fn force_gui() {
        check(&[], Flags { gui: true, tui: false }, Platform::UnixDisplay, Frontend::Gui);
    }
    #[test]
    fn force_tui() {
        check(&[], Flags { gui: false, tui: true }, WIN, Frontend::Tui);
    }
    #[test]
    fn conflicting_flags() {
        assert!(matches!(choose_frontend(&Env(&[]), Flags { gui: true, tui: true }, WIN), Frontend::Error(_)));
    }
    #[test]
    fn force_gui_service_is_error() {
        assert!(matches!(
            choose_frontend(
                &Env(&[]),
                Flags { gui: true, tui: false },
                Platform::Windows { session: Some(0), interactive_station: true }
            ),
            Frontend::Error(_)
        ));
    }
    #[test]
    fn gui_failure_with_terminal() {
        assert_eq!(after_gui_failure(true), Frontend::Tui);
    }
    #[test]
    fn gui_failure_without_terminal() {
        assert!(matches!(after_gui_failure(false), Frontend::Error(_)));
    }
    #[test]
    #[ignore = "one-shot measurement; run explicitly with --ignored --nocapture"]
    fn measure_real_detection_once() {
        let start = std::time::Instant::now();
        let result = choose_frontend(&ProcessEnv, Flags::default(), current_platform());
        let elapsed = start.elapsed();
        eprintln!("frontend detection: {elapsed:?}, {result:?}");
        assert!(elapsed < std::time::Duration::from_millis(1));
    }
}
