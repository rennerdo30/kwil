//! Error presentation is decided before recovery, without executing CLI actions.
use crate::frontend::{EnvLookup, Platform};
use std::ffi::OsString;
use std::process::ExitCode;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum StandardStreams {
    Absent,
    Terminal,
    Redirected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ErrorPresentation {
    StderrOnly,
    Dialog,
}

pub(crate) fn error_presentation(
    mut args: impl Iterator<Item = OsString>,
    console_launcher: bool,
    streams: StandardStreams,
    env: &impl EnvLookup,
    platform: Platform,
) -> ErrorPresentation {
    // Inspect intent only: recovery must still run before --help/--version can
    // exit or --update can modify the install. Paths stay OS strings.
    let cli_intent = args.any(|arg| match arg.to_str() {
        Some("--gui") => false,
        Some("-h" | "-V") => true,
        Some(arg) => arg.starts_with("--"),
        None => false,
    });
    let interactive_desktop = matches!(platform,
        Platform::Windows { session: Some(session), interactive_station: true } if session != 0);
    if console_launcher
        || cli_intent
        || env.has("KWIL_CONSOLE_LAUNCH")
        || ["SSH_CONNECTION", "SSH_TTY", "SSH_CLIENT"].into_iter().any(|key| env.has(key))
        || streams != StandardStreams::Absent
        || !interactive_desktop
    {
        ErrorPresentation::StderrOnly
    } else {
        ErrorPresentation::Dialog
    }
}

impl ErrorPresentation {
    pub(crate) fn report(self, message: &str, stderr: impl FnOnce(&str), dialog: impl FnOnce(&str)) {
        stderr(message);
        if self == Self::Dialog {
            dialog(message);
        }
    }
}

pub(crate) fn recover<E: std::fmt::Display>(
    presentation: ErrorPresentation,
    recovery: impl FnOnce() -> Result<(), E>,
    stderr: impl FnOnce(&str),
    dialog: impl FnOnce(&str),
) -> Result<(), ExitCode> {
    recovery().map_err(|e| {
        presentation.report(&format!("kwil: {e}"), stderr, dialog);
        ExitCode::FAILURE
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct Env<'a>(&'a [&'a str]);
    impl EnvLookup for Env<'_> {
        fn has(&self, key: &str) -> bool {
            self.0.contains(&key)
        }
    }
    const DESKTOP: Platform = Platform::Windows { session: Some(1), interactive_station: true };

    fn classify(
        args: &[&str],
        console: bool,
        streams: StandardStreams,
        env: &[&str],
        platform: Platform,
    ) -> ErrorPresentation {
        error_presentation(args.iter().map(|arg| OsString::from(*arg)), console, streams, &Env(env), platform)
    }

    #[test]
    fn cli_intent_flags_never_request_modal_errors() {
        for flag in ["--help", "-h", "--version", "-V", "--update", "--tui", "--unknown"] {
            for args in [vec![flag], vec!["--gui", flag], vec!["notes.txt", flag]] {
                assert_eq!(
                    classify(&args, false, StandardStreams::Absent, &[], DESKTOP),
                    ErrorPresentation::StderrOnly,
                    "{args:?}"
                );
            }
        }
    }

    #[test]
    fn console_launcher_is_cli_even_without_flags_or_streams() {
        assert_eq!(
            classify(&["notes.txt"], true, StandardStreams::Absent, &[], DESKTOP),
            ErrorPresentation::StderrOnly
        );
    }

    #[test]
    fn redirected_and_terminal_streams_preclude_modal_errors() {
        for streams in [StandardStreams::Redirected, StandardStreams::Terminal] {
            for args in [vec![], vec!["notes.txt"], vec!["--gui"]] {
                assert_eq!(classify(&args, false, streams, &[], DESKTOP), ErrorPresentation::StderrOnly, "{args:?}");
            }
        }
    }

    #[test]
    fn noninteractive_sessions_and_ssh_preclude_modal_errors() {
        for platform in [
            Platform::Windows { session: Some(0), interactive_station: true },
            Platform::Windows { session: None, interactive_station: true },
            Platform::Windows { session: Some(1), interactive_station: false },
            Platform::UnixDisplay,
            Platform::MacOs,
        ] {
            for args in [vec![], vec!["--gui"]] {
                assert_eq!(
                    classify(&args, false, StandardStreams::Absent, &[], platform),
                    ErrorPresentation::StderrOnly,
                    "{platform:?}"
                );
            }
        }
        for key in ["SSH_CONNECTION", "SSH_TTY", "SSH_CLIENT"] {
            assert_eq!(
                classify(&[], false, StandardStreams::Absent, &[key], DESKTOP),
                ErrorPresentation::StderrOnly,
                "{key}"
            );
        }
    }

    #[test]
    fn hidden_and_launcher_child_errors_remain_nonmodal() {
        assert_eq!(
            classify(&["--hidden"], false, StandardStreams::Absent, &[], DESKTOP),
            ErrorPresentation::StderrOnly
        );
        assert_eq!(
            classify(&[], false, StandardStreams::Absent, &["KWIL_CONSOLE_LAUNCH"], DESKTOP),
            ErrorPresentation::StderrOnly
        );
    }

    #[test]
    fn explorer_gui_retains_error_dialog() {
        for args in [vec![], vec!["notes.txt"], vec!["--gui", "notes.txt"], vec!["C:\\notes\\--version"]] {
            assert_eq!(
                classify(&args, false, StandardStreams::Absent, &[], DESKTOP),
                ErrorPresentation::Dialog,
                "{args:?}"
            );
        }
    }

    #[test]
    fn redirected_cli_lock_failure_recovers_then_reports_and_returns_failure_without_ui() {
        let attempted = Cell::new(false);
        let mut stderr = String::new();
        let mut dialogs = Vec::new();
        let presentation = classify(&["--version"], true, StandardStreams::Redirected, &[], DESKTOP);
        let result = recover(
            presentation,
            || {
                attempted.set(true);
                Err("update lock unavailable; retry after the update finishes")
            },
            |message| {
                assert!(attempted.get());
                stderr.push_str(message);
            },
            |message| dialogs.push(message.to_owned()),
        );
        assert!(attempted.get());
        assert_eq!(result, Err(ExitCode::FAILURE));
        assert_eq!(stderr, "kwil: update lock unavailable; retry after the update finishes");
        assert!(dialogs.is_empty(), "CLI startup invoked modal UI: {dialogs:?}");
    }

    #[test]
    fn explorer_recovery_failure_reports_to_stderr_and_dialog() {
        let mut stderr = String::new();
        let mut dialogs = Vec::new();
        let result = recover(
            classify(&[], false, StandardStreams::Absent, &[], DESKTOP),
            || Err("cannot restore interrupted update; backups retained"),
            |message| stderr.push_str(message),
            |message| dialogs.push(message.to_owned()),
        );
        assert_eq!(result, Err(ExitCode::FAILURE));
        assert_eq!(stderr, "kwil: cannot restore interrupted update; backups retained");
        assert_eq!(dialogs, [stderr]);
    }

    #[test]
    fn successful_recovery_does_not_present_an_error() {
        for presentation in [ErrorPresentation::StderrOnly, ErrorPresentation::Dialog] {
            assert_eq!(
                recover(
                    presentation,
                    || Ok::<_, String>(()),
                    |_| panic!("unexpected stderr"),
                    |_| panic!("unexpected dialog")
                ),
                Ok(())
            );
        }
    }
}
