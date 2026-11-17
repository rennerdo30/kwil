//! KW-90: offer Kwil on the user's PATH and Win+R. One state, one code path:
//! the first-run choice, the settings switch and the palette commands all call
//! `App::command_line_start`, which calls `kwil_core_commands::command_line`.
//! Nothing is written unless the user chooses it; hidden test mode, isolated
//! runs and unit tests refuse before any registry access (`command_line::gate`).
use super::*;
use kwil_core_commands::command_line::{self as cl, Report, Request, Status};

/// What a drawn control needs: the real state and one short sentence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Row {
    pub on: bool,
    pub busy: bool,
    pub detail: String,
    pub problem: bool,
}

#[derive(Default)]
pub(super) struct State {
    /// The real state once read; `None` while unknown or when reading failed.
    status: Option<Status>,
    busy: bool,
    /// The user changed it during this run (the first-run default no longer applies).
    touched: bool,
    asked: bool,
    problem: Option<String>,
}

/// Whether the first-run default (`welcome::COMMAND_LINE_PRESELECTED`) should
/// be applied now: only when it is on, the user never touched the choice, the
/// file-type step really finished and Kwil is not registered yet.
pub(super) fn default_applies(preselected: bool, touched: bool, registered: bool, registration_applied: bool) -> bool {
    preselected && !touched && !registered && registration_applied
}

impl State {
    pub(super) fn row(&self, welcome: bool) -> Option<Row> {
        if !cl::SUPPORTED {
            return None;
        }
        let on = self.status.is_some_and(|s| s.registered());
        let (detail, problem) = if self.busy {
            ("Working…".to_owned(), false)
        } else if let Some(problem) = &self.problem {
            (desktop::visible(problem, 160), true)
        } else {
            let detail = match (self.status, on, welcome) {
                (None, _, _) => "Checking…",
                (Some(_), true, true) => "Kwil is on your PATH. Open a new terminal to use `kwil`.",
                (Some(_), false, true) => "Adds Kwil to your user PATH. Nothing changes unless you tick this.",
                (Some(status), _, false) => cl::status_message(status),
            };
            (detail.to_owned(), false)
        };
        Some(Row { on, busy: self.busy, detail, problem })
    }
    #[cfg(feature = "frame-harness")]
    pub(super) fn seed_for_harness(&mut self, registered: bool) {
        use cl::AppPaths;
        self.asked = true;
        self.problem = None;
        self.status = Some(Status {
            app_paths: if registered { AppPaths::Ours } else { AppPaths::Missing },
            on_path: registered,
        });
    }
    #[cfg(feature = "frame-harness")]
    pub(super) fn problem(&self) -> Option<&str> {
        self.problem.as_deref()
    }
}

impl App {
    /// Start one request on a worker thread (the environment broadcast can wait
    /// for a hung window). Refusals are answered here, before any thread exists.
    pub(super) fn command_line_start(&mut self, request: Request) {
        if !cl::SUPPORTED {
            return;
        }
        let explicit = request != Request::Status;
        if self.command_line.busy {
            if explicit {
                let damage = self.view.set_notice("Still working on the command-line setup…".into());
                self.apply(damage);
            }
            return;
        }
        let proxy = self.proxy.clone();
        let started = cl::spawn(request, cl::gate(self.opts.hidden, cfg!(test)), move |result| {
            let _ = proxy.send_event(UserEvent::CommandLine(request, result));
        });
        match started {
            Ok(()) => self.command_line.busy = true,
            Err(why) => {
                self.command_line.problem = Some(why.clone());
                if explicit {
                    let damage = self.view.set_notice(why);
                    self.apply(damage);
                }
            }
        }
        self.command_line_redraw();
    }

    /// Read the real state once, when a page that shows it opens.
    pub(super) fn command_line_refresh(&mut self) {
        if !self.command_line.asked {
            self.command_line.asked = true;
            self.command_line_start(Request::Status);
        }
    }

    /// The switch and the checkbox: the opposite of what is registered now.
    pub(super) fn command_line_toggle(&mut self) {
        self.command_line.touched = true;
        let request = if self.command_line.status.is_some_and(|s| s.registered()) {
            Request::Unregister
        } else {
            Request::Register
        };
        self.command_line_start(request);
    }

    /// After the first-run file-type step really finished.
    pub(super) fn command_line_apply_default(&mut self, registration_applied: bool) {
        let registered = self.command_line.status.is_some_and(|s| s.registered());
        if default_applies(
            welcome::COMMAND_LINE_PRESELECTED,
            self.command_line.touched,
            registered,
            registration_applied,
        ) {
            self.command_line_start(Request::Register);
        }
    }

    pub(super) fn command_line_done(&mut self, request: Request, result: Result<Report, String>) -> Damage {
        self.command_line.busy = false;
        let message = match result {
            Ok(report) => {
                self.command_line.status = Some(report.status);
                self.command_line.problem = None;
                (request != Request::Status).then_some(report.message)
            }
            Err(error) => {
                self.command_line.problem = Some(error.clone());
                (request != Request::Status).then_some(error)
            }
        };
        self.command_line_redraw();
        match message {
            Some(message) => self.view.set_notice(message),
            None => Damage { full: true, status: true, ..Default::default() },
        }
    }

    fn command_line_redraw(&mut self) {
        self.welcome_view.hits = Default::default();
        self.welcome_view.paint.request();
        self.settings_epoch = self.settings_epoch.wrapping_add(1);
        self.settings_view.paint.request();
        self.request_frame();
    }

    /// Palette: "Add Kwil to PATH" / "Remove Kwil from PATH".
    pub(super) fn dispatch_command_line(&mut self, id: &str) -> bool {
        let request = match id {
            "commandLine.add" => Request::Register,
            "commandLine.remove" => Request::Unregister,
            _ => return false,
        };
        if !cl::SUPPORTED {
            let damage = self.view.set_notice(
                "Putting Kwil on the PATH is only needed on Windows; scripts/install.sh already links it.".into(),
            );
            self.apply(damage);
            return true;
        }
        self.command_line.touched = true;
        self.command_line_start(request);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_run_default_applies_only_after_a_finished_untouched_registration() {
        assert!(default_applies(true, false, false, true));
        for (preselected, touched, registered, applied) in [
            (false, false, false, true),
            (true, true, false, true),
            (true, false, true, true),
            (true, false, false, false),
        ] {
            assert!(!default_applies(preselected, touched, registered, applied));
        }
        assert!(!std::hint::black_box(welcome::COMMAND_LINE_PRESELECTED), "the owner has not decided to pre-check it");
    }

    #[test]
    fn rows_state_the_real_status_in_plain_words() {
        let mut state = State::default();
        if !cl::SUPPORTED {
            assert!(state.row(true).is_none() && state.row(false).is_none());
            return;
        }
        assert_eq!(state.row(false).unwrap().detail, "Checking…");
        state.status = Some(Status { app_paths: cl::AppPaths::Missing, on_path: false });
        let off = state.row(true).unwrap();
        assert!(!off.on && off.detail.contains("Nothing changes unless you tick this"));
        state.status = Some(Status { app_paths: cl::AppPaths::Ours, on_path: true });
        let on = state.row(true).unwrap();
        assert!(on.on && on.detail.contains("Open a new terminal"));
        assert_eq!(state.row(false).unwrap().detail, "Kwil is on your PATH and opens from Win+R.");
        state.status = Some(Status { app_paths: cl::AppPaths::Ours, on_path: false });
        assert!(!state.row(false).unwrap().on, "a partly applied state is not shown as on");
        state.problem = Some("Windows refused access".into());
        assert!(state.row(false).unwrap().problem);
        state.busy = true;
        assert_eq!(state.row(false).unwrap().detail, "Working…");
    }
}
