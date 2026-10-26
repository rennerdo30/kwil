//! GUI-only chooser coordination. No save writer, disk access or path text conversion.
//! A native receipt is valid only for the exact Workspace prompt it captured.
use kwil_view_model::{NativeFilePrompt, Workspace};
#[cfg(not(windows))]
use std::io;
use std::path::PathBuf;

pub(crate) const fn native_supported() -> bool {
    cfg!(windows)
}

#[derive(Debug)]
// Completion variants are produced only by the Windows STA or injected tests.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) enum Outcome {
    Accepted(PathBuf),
    Cancelled,
    Failed(String),
}

#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) enum Event {
    Ready,
    Completed { ticket: NativeFilePrompt, outcome: Outcome },
    Stopped(Result<(), String>),
}

pub(crate) struct Request {
    pub ticket: Box<NativeFilePrompt>,
    /// Native owner handle only, never a COM pointer. Owner must outlive Stopped.
    #[cfg(windows)]
    pub owner: isize,
    #[cfg(windows)]
    pub suggested_name: Option<std::ffi::OsString>,
}

pub(crate) trait Port {
    /// Nonblocking admission. Err returns ownership of the complete request.
    fn submit(&mut self, request: Request) -> Result<(), (Request, String)>;
    fn cancel(&mut self);
    fn shutdown(&mut self);
}

#[derive(Default)]
pub(crate) struct Gate {
    active: Option<NativeFilePrompt>,
    ready: bool,
    stopping: bool,
    stopped: bool,
}

impl Gate {
    pub fn busy(&self) -> bool {
        self.active.is_some()
    }

    pub fn stopped(&self) -> bool {
        self.stopped
    }

    pub fn sync(&mut self, workspace: &mut Workspace, _owner: isize, port: &mut impl Port) {
        if self.stopping || !self.ready {
            return;
        }
        if let Some(ticket) = &self.active {
            // Coalesce an unchanged prompt; never clear or replace its request.
            // A new prompt waits for the old worker's terminal receipt.
            if !workspace.native_file_prompt_is_current(ticket) {
                workspace.cancel_native_file_prompt(ticket);
                port.cancel();
            }
            return;
        }
        let Some(ticket) = workspace.native_file_prompt() else {
            return;
        };
        #[cfg(windows)]
        let suggested_name = workspace
            .view(ticket.document())
            .and_then(|view| view.editor())
            .and_then(|editor| editor.document().path())
            .and_then(|path| path.file_name())
            .map(std::ffi::OsStr::to_os_string);
        let request = Request {
            ticket: Box::new(ticket.clone()),
            #[cfg(windows)]
            owner: _owner,
            #[cfg(windows)]
            suggested_name,
        };
        match port.submit(request) {
            Ok(()) => {
                self.active = Some(ticket);
                workspace.set_notice("Choose a destination in the system file dialog".into());
            }
            Err((request, error)) => {
                workspace.cancel_native_file_prompt(&request.ticket);
                workspace.set_notice(format!("File dialog: {error}"));
            }
        }
    }

    pub fn event(&mut self, workspace: &mut Workspace, event: Event) {
        match event {
            Event::Ready => self.ready = !self.stopping,
            Event::Completed { ticket, outcome } => {
                // A duplicate or foreign receipt cannot release another operation.
                if self.active.as_ref() != Some(&ticket) {
                    return;
                }
                self.active = None;
                if self.stopping || !workspace.native_file_prompt_is_current(&ticket) {
                    workspace.cancel_native_file_prompt(&ticket);
                    return;
                }
                match outcome {
                    Outcome::Accepted(path) => {
                        if let Err(error) = workspace.submit_native_file_prompt(&ticket, path) {
                            workspace.set_notice(format!("File dialog: {error}"));
                        }
                    }
                    Outcome::Cancelled => {
                        workspace.cancel_native_file_prompt(&ticket);
                        workspace.set_notice(String::new());
                    }
                    Outcome::Failed(error) => {
                        workspace.cancel_native_file_prompt(&ticket);
                        workspace.set_notice(format!("File dialog: {error}"));
                    }
                }
            }
            Event::Stopped(result) => {
                self.ready = false;
                self.stopping = true;
                self.stopped = true;
                if let Some(ticket) = self.active.take() {
                    workspace.cancel_native_file_prompt(&ticket);
                }
                if let Err(error) = result {
                    // An initialization failure also owns the current native prompt,
                    // which could have been created while the worker was starting.
                    if let Some(ticket) = workspace.native_file_prompt() {
                        workspace.cancel_native_file_prompt(&ticket);
                    }
                    workspace.set_notice(format!("File dialog unavailable: {error}"));
                }
            }
        }
    }

    pub fn shutdown(&mut self, workspace: &mut Workspace, port: &mut impl Port) {
        if self.stopping {
            return;
        }
        self.stopping = true;
        if let Some(ticket) = &self.active {
            workspace.cancel_native_file_prompt(ticket);
        }
        port.shutdown();
    }
}

#[cfg(windows)]
pub(crate) use crate::platform::file_dialog_windows::Worker;

#[cfg(not(windows))]
pub(crate) struct Worker;

#[cfg(not(windows))]
impl Worker {
    pub fn spawn(_complete: impl Fn(Event) + Send + 'static) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            if cfg!(target_os = "macos") {
                "macOS chooser gate: ROOT must approve AppKit bindings for NSOpenPanel/NSSavePanel and main-thread sheet integration"
            } else {
                "Linux chooser gate: ROOT must approve an xdg-desktop-portal D-Bus binding (or platform-native GTK integration)"
            },
        ))
    }
}

#[cfg(not(windows))]
impl Port for Worker {
    fn submit(&mut self, request: Request) -> Result<(), (Request, String)> {
        Err((request, "native file chooser unavailable on this platform".into()))
    }
    fn cancel(&mut self) {}
    fn shutdown(&mut self) {}
}

/// App-owned lifetime, created lazily on the first chooser request. Failed
/// initialization is latched; a command cannot silently fall back to inline UI.
#[derive(Default)]
pub(crate) struct Dialogs {
    gate: Gate,
    worker: Option<Worker>,
    failure: Option<String>,
    exiting: bool,
}
impl Dialogs {
    /// An accepted close after a document delivery refusal transfers pending
    /// saves to the sealed bundle. Only the native dialog's real stop receipt
    /// must still arrive on this event loop; no document result is discarded.
    pub fn document_shutdown(&mut self, workspace: &mut Workspace) -> bool {
        self.exiting = true;
        if let Some(worker) = &mut self.worker {
            self.gate.shutdown(workspace, worker);
            self.gate.stopped()
        } else {
            true
        }
    }
    pub fn exiting(&self) -> bool {
        self.exiting
    }
    pub fn busy(&self) -> bool {
        self.gate.busy()
    }
    pub fn blocks_input(&self, workspace: &Workspace) -> bool {
        self.exiting || self.busy() || (native_supported() && workspace.native_file_prompt().is_some())
    }
    pub fn sync(
        &mut self,
        workspace: &mut Workspace,
        owner: isize,
        hidden: bool,
        complete: impl Fn(Event) + Send + 'static,
    ) {
        if self.exiting || !native_supported() || owner == 0 {
            return;
        }
        if let Some(error) = &self.failure {
            if let Some(ticket) = workspace.native_file_prompt() {
                workspace.cancel_native_file_prompt(&ticket);
                workspace.set_notice(format!("File dialog unavailable: {error}"));
            }
            return;
        }
        if workspace.native_file_prompt().is_some() && self.worker.is_none() {
            if hidden && self.failure.is_none() {
                self.failure = Some("native chooser disabled in hidden harness; ROOT must supply an STA dialog-cloaking hook for native proof".into());
            }
            if self.failure.is_none() {
                match Worker::spawn(complete) {
                    Ok(worker) => self.worker = Some(worker),
                    Err(error) => self.failure = Some(error.to_string()),
                }
            }
            if let Some(error) = &self.failure {
                if let Some(ticket) = workspace.native_file_prompt() {
                    workspace.cancel_native_file_prompt(&ticket);
                    workspace.set_notice(format!("File dialog unavailable: {error}"));
                }
                return;
            }
        }
        if let Some(worker) = &mut self.worker {
            self.gate.sync(workspace, owner, worker);
        }
    }
    pub fn event(&mut self, workspace: &mut Workspace, event: Event) {
        if let Event::Stopped(result) = &event {
            self.failure = Some(result.as_ref().err().cloned().unwrap_or_else(|| "STA worker stopped".into()));
        }
        self.gate.event(workspace, event);
    }
    /// Call on every exit route. Keep pumping events and retain Gfx/window until
    /// true. No joining, sleeping or disk access occurs on the UI thread.
    pub fn may_exit(&mut self, workspace: &mut Workspace) -> bool {
        if !self.exiting && !workspace.wants_exit() {
            return false;
        }
        self.exiting = true;
        if let Some(worker) = &mut self.worker {
            self.gate.shutdown(workspace, worker);
            self.gate.stopped() && !workspace.file_saves_pending()
        } else {
            !workspace.file_saves_pending()
        }
    }
    pub fn force_exit(&mut self, workspace: &mut Workspace) -> bool {
        if !self.exiting {
            // Cancel unscheduled prompt/Save All/close continuations, but never
            // cancel or discard a Save already admitted to DocumentWorker.
            workspace.cancel_file_prompt();
        }
        self.exiting = true;
        self.may_exit(workspace)
    }
}

// R12 cfg(test)-only windowless Port/Gate protocol. The fake has no OS dialog
// resources. Completion owns the exact request admitted by production Gate.sync;
// Stopped is emitted only after the original port's real shutdown and request
// disposal. Consumption still goes through production Dialogs.event/Gate.event.
#[cfg(test)]
pub(super) struct NotificationFixture {
    port: tests::Fake,
    deadline: std::time::Instant,
}
#[cfg(test)]
impl NotificationFixture {
    pub(super) fn new(deadline: std::time::Instant) -> Self {
        Self { port: tests::Fake::default(), deadline }
    }
    pub(super) fn ready(&self, emit: impl FnOnce(Event)) {
        assert!(std::time::Instant::now() < self.deadline);
        emit(Event::Ready);
    }
    pub(super) fn submit(&mut self, dialogs: &mut Dialogs, workspace: &mut Workspace) {
        assert!(std::time::Instant::now() < self.deadline);
        dialogs.gate.sync(workspace, 0, &mut self.port);
        assert!(self.port.request.is_some(), "the original production gate must actually admit its captured request");
    }
    pub(super) fn shutdown(&mut self, dialogs: &mut Dialogs, workspace: &mut Workspace, mut emit: impl FnMut(Event)) {
        assert!(std::time::Instant::now() < self.deadline);
        dialogs.gate.shutdown(workspace, &mut self.port);
        assert_eq!(self.port.shutdowns, 1, "original port receives actual shutdown exactly once");
        if let Some(request) = self.port.request.take() {
            emit(Event::Completed { ticket: *request.ticket, outcome: Outcome::Cancelled });
        }
        assert!(self.port.request.is_none());
        emit(Event::Stopped(Ok(()))); // windowless port has no remaining resources
    }
    pub(super) fn is_stopped(dialogs: &Dialogs) -> bool {
        dialogs.gate.stopped()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kwil_view_model::{CommandArgs, View};

    #[derive(Default)]
    pub(super) struct Fake {
        pub(super) request: Option<Request>,
        submits: usize,
        cancels: usize,
        pub(super) shutdowns: usize,
        reject: bool,
    }
    impl Port for Fake {
        fn submit(&mut self, request: Request) -> Result<(), (Request, String)> {
            self.submits += 1;
            if self.reject || self.request.is_some() {
                return Err((request, "mock admission failure".into()));
            }
            self.request = Some(request);
            Ok(())
        }
        fn cancel(&mut self) {
            self.cancels += 1;
        }
        fn shutdown(&mut self) {
            self.shutdowns += 1;
        }
    }
    fn setup() -> (Workspace, Gate, Fake) {
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        workspace
            .execute("edit.insert", &CommandArgs { text: Some("dirty 日本語".into()), ..Default::default() }, 1)
            .unwrap();
        workspace.file_command("file.save").unwrap();
        let mut gate = Gate::default();
        gate.event(&mut workspace, Event::Ready);
        (workspace, gate, Fake::default())
    }
    #[test]
    fn native_cancel_keeps_untitled_dirty_and_coalesces_repeated_sync() {
        let (mut workspace, mut gate, mut fake) = setup();
        for _ in 0..3 {
            gate.sync(&mut workspace, 1, &mut fake);
        }
        assert_eq!(fake.submits, 1);
        assert!(workspace.editor().unwrap().document().is_dirty());
        assert!(workspace.editor().unwrap().document().path().is_none());
        let request = fake.request.take().unwrap();
        gate.event(&mut workspace, Event::Completed { ticket: *request.ticket, outcome: Outcome::Cancelled });
        assert!(workspace.file_prompt().is_none());
        assert!(!workspace.close_pending());
        assert!(workspace.editor().unwrap().document().is_dirty());
        assert!(workspace.editor().unwrap().document().path().is_none());
    }
    #[test]
    fn edit_then_accept_cannot_queue_save_for_newer_revision() {
        let (mut workspace, mut gate, mut fake) = setup();
        gate.sync(&mut workspace, 1, &mut fake);
        let request = fake.request.take().unwrap();
        workspace.execute("edit.insert", &CommandArgs { text: Some("new".into()), ..Default::default() }, 2).unwrap();
        gate.event(
            &mut workspace,
            Event::Completed {
                ticket: *request.ticket,
                outcome: Outcome::Accepted(std::env::temp_dir().join("kw56-unused.txt")),
            },
        );
        assert!(workspace.file_prompt().is_none());
        assert!(workspace.editor().unwrap().document().path().is_none());
        assert!(!gate.busy());
    }
    #[test]
    fn new_prompt_waits_for_cancelled_native_operation_and_old_reply_cannot_consume_it() {
        let (mut workspace, mut gate, mut fake) = setup();
        gate.sync(&mut workspace, 1, &mut fake);
        let old = fake.request.take().unwrap();
        workspace.file_command("file.new").unwrap();
        workspace.file_command("file.save").unwrap();
        let current = workspace.native_file_prompt().unwrap();
        gate.sync(&mut workspace, 1, &mut fake);
        assert_eq!(fake.submits, 1);
        assert_eq!(fake.cancels, 1);
        gate.event(&mut workspace, Event::Completed { ticket: (*old.ticket).clone(), outcome: Outcome::Cancelled });
        assert!(workspace.native_file_prompt_is_current(&current));
        gate.sync(&mut workspace, 1, &mut fake);
        assert_eq!(fake.submits, 2);
        gate.event(&mut workspace, Event::Completed { ticket: *old.ticket, outcome: Outcome::Cancelled });
        assert!(gate.busy());
    }
    #[test]
    fn failure_cancels_save_and_quit_and_shutdown_never_accepts_late_path() {
        let (mut workspace, mut gate, mut fake) = setup();
        workspace.file_command("file.saveAndQuit").unwrap();
        gate.sync(&mut workspace, 1, &mut fake);
        let request = fake.request.take().unwrap();
        gate.event(
            &mut workspace,
            Event::Completed { ticket: *request.ticket, outcome: Outcome::Failed("access denied".into()) },
        );
        assert!(!workspace.close_pending());
        workspace.file_command("file.save").unwrap();
        gate.sync(&mut workspace, 1, &mut fake);
        let request = fake.request.take().unwrap();
        gate.shutdown(&mut workspace, &mut fake);
        gate.shutdown(&mut workspace, &mut fake);
        assert_eq!(fake.shutdowns, 1);
        assert!(!gate.stopped());
        gate.event(
            &mut workspace,
            Event::Completed {
                ticket: *request.ticket,
                outcome: Outcome::Accepted(std::env::temp_dir().join("kw56-never-written.txt")),
            },
        );
        assert!(workspace.editor().unwrap().document().path().is_none());
        gate.event(&mut workspace, Event::Stopped(Ok(())));
        assert!(gate.stopped());
    }

    #[test]
    fn rejected_native_request_keeps_document_dirty_and_releases_prompt_intent() {
        let (mut workspace, mut gate, mut fake) = setup();
        fake.reject = true;
        gate.sync(&mut workspace, 1, &mut fake);
        assert!(!gate.busy());
        assert!(workspace.file_prompt().is_none());
        assert!(workspace.editor().unwrap().document().is_dirty());
        assert!(workspace.editor().unwrap().document().path().is_none());
    }

    #[test]
    fn forced_exit_waits_for_accepted_save_receipt_without_starting_new_transactions() {
        let (mut workspace, rx, root) = isolated();
        workspace
            .execute("edit.insert", &CommandArgs { text: Some("dirty 日本語".into()), ..Default::default() }, 1)
            .unwrap();
        workspace.file_command("file.save").unwrap();
        let ticket = workspace.native_file_prompt().unwrap();
        workspace.submit_native_file_prompt(&ticket, root.join("missing-parent").join("pending-receipt.txt")).unwrap();
        assert!(workspace.file_saves_pending());
        let mut dialogs = Dialogs::default();
        assert!(!dialogs.force_exit(&mut workspace));
        assert!(!workspace.close_pending());
        drain_until(&mut workspace, &rx, |workspace| !workspace.file_saves_pending());
        assert!(!workspace.file_saves_pending());
        assert!(dialogs.may_exit(&mut workspace));
        assert!(workspace.editor().unwrap().document().is_dirty());
        settle_and_remove(workspace, rx, root);
    }

    // ROOT-only draft: actual Workspace + existing DocumentWorker + fake native
    // backend. Every write is under this test's newly created explicit root.
    // No environment override, profile lookup or real native window is needed.
    fn isolated() -> (Workspace, crate::document_ui::fixtures::Lane, PathBuf) {
        let root = crate::test_support::directory("kw56-dialog").unwrap();
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        let lane = crate::document_ui::fixtures::Lane::start(&mut workspace, root.join("state"));
        drain_until(&mut workspace, &lane, |workspace| workspace.recovery_ready());
        (workspace, lane, root)
    }
    fn drain_until(
        workspace: &mut Workspace,
        lane: &crate::document_ui::fixtures::Lane,
        done: impl Fn(&Workspace) -> bool,
    ) {
        lane.until(workspace, done);
    }
    fn settle_and_remove(workspace: Workspace, lane: crate::document_ui::fixtures::Lane, root: PathBuf) {
        lane.settle(workspace);
        assert_eq!(root.parent(), Some(std::env::temp_dir().as_path()));
        assert!(root.file_name().unwrap().to_string_lossy().starts_with("kw56-dialog-"));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn mocked_native_accept_is_the_only_point_that_can_start_real_save_and_named_save_reuses_path() {
        let (mut workspace, rx, root) = isolated();
        let path = root.join("日本語.txt");
        workspace.execute("edit.insert", &CommandArgs { text: Some("first".into()), ..Default::default() }, 1).unwrap();
        workspace.file_command("file.save").unwrap();
        let mut gate = Gate::default();
        let mut fake = Fake::default();
        gate.event(&mut workspace, Event::Ready);
        gate.sync(&mut workspace, 1, &mut fake);
        assert!(!path.exists(), "save must not reach disk before native accept");
        let request = fake.request.take().unwrap();
        gate.event(
            &mut workspace,
            Event::Completed { ticket: *request.ticket, outcome: Outcome::Accepted(path.clone()) },
        );
        drain_until(&mut workspace, &rx, |workspace| !workspace.editor().unwrap().document().is_dirty());
        assert_eq!(std::fs::read(&path).unwrap(), b"first");
        workspace
            .execute("edit.insert", &CommandArgs { text: Some(" second".into()), ..Default::default() }, 2)
            .unwrap();
        workspace.file_command("file.save").unwrap();
        gate.sync(&mut workspace, 1, &mut fake);
        assert_eq!(fake.submits, 1, "named Ctrl+S must not open another chooser");
        assert!(workspace.file_prompt().is_none());
        drain_until(&mut workspace, &rx, |workspace| !workspace.editor().unwrap().document().is_dirty());
        assert_eq!(std::fs::read(&path).unwrap(), b"first second");
        settle_and_remove(workspace, rx, root);
    }
    #[test]
    fn mocked_native_overwrite_cancel_keeps_existing_file_then_accept_uses_real_writer() {
        let (mut workspace, rx, root) = isolated();
        let path = root.join("existing.txt");
        std::fs::write(&path, b"existing").unwrap();
        workspace
            .execute("edit.insert", &CommandArgs { text: Some("replacement".into()), ..Default::default() }, 1)
            .unwrap();
        workspace.file_command("file.saveAndQuit").unwrap();
        let mut gate = Gate::default();
        let mut fake = Fake::default();
        gate.event(&mut workspace, Event::Ready);
        gate.sync(&mut workspace, 1, &mut fake);
        let request = fake.request.take().unwrap();
        gate.event(&mut workspace, Event::Completed { ticket: *request.ticket, outcome: Outcome::Cancelled });
        assert_eq!(std::fs::read(&path).unwrap(), b"existing");
        assert!(!workspace.close_pending());
        assert!(workspace.editor().unwrap().document().is_dirty());
        assert!(workspace.editor().unwrap().document().path().is_none());
        workspace.file_command("file.saveAs").unwrap();
        gate.sync(&mut workspace, 1, &mut fake);
        let request = fake.request.take().unwrap();
        gate.event(
            &mut workspace,
            Event::Completed { ticket: *request.ticket, outcome: Outcome::Accepted(path.clone()) },
        );
        drain_until(&mut workspace, &rx, |workspace| !workspace.editor().unwrap().document().is_dirty());
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
        // Mock acceptance models Shell confirmation; only native ROOT testing
        // establishes that the real Shell presented its overwrite prompt.
        settle_and_remove(workspace, rx, root);
    }

    #[test]
    fn mocked_native_save_error_keeps_dirty_untitled_and_cancels_close_intent() {
        let (mut workspace, rx, root) = isolated();
        let path = root.join("missing-parent").join("file.txt");
        workspace
            .execute("edit.insert", &CommandArgs { text: Some("must survive".into()), ..Default::default() }, 1)
            .unwrap();
        workspace.file_command("file.saveAndQuit").unwrap();
        let mut gate = Gate::default();
        let mut fake = Fake::default();
        gate.event(&mut workspace, Event::Ready);
        gate.sync(&mut workspace, 1, &mut fake);
        let request = fake.request.take().unwrap();
        gate.event(
            &mut workspace,
            Event::Completed { ticket: *request.ticket, outcome: Outcome::Accepted(path.clone()) },
        );
        drain_until(&mut workspace, &rx, |workspace| !workspace.close_pending());
        assert!(!path.exists());
        assert!(!workspace.wants_exit());
        assert!(workspace.editor().unwrap().document().is_dirty());
        assert!(workspace.editor().unwrap().document().path().is_none());
        settle_and_remove(workspace, rx, root);
    }
    #[test]
    fn mocked_native_open_uses_real_workspace_worker() {
        let (mut workspace, rx, root) = isolated();
        let path = root.join("open-日本語.txt");
        std::fs::write(&path, b"opened bytes").unwrap();
        workspace.file_command("file.open").unwrap();
        let mut gate = Gate::default();
        let mut fake = Fake::default();
        gate.event(&mut workspace, Event::Ready);
        gate.sync(&mut workspace, 1, &mut fake);
        let request = fake.request.take().unwrap();
        gate.event(
            &mut workspace,
            Event::Completed { ticket: *request.ticket, outcome: Outcome::Accepted(path.clone()) },
        );
        drain_until(&mut workspace, &rx, |workspace| {
            workspace.editor().is_some_and(|editor| editor.document().path() == Some(path.as_path()))
        });
        assert_eq!(workspace.editor().unwrap().document().snapshot().len(), 12);
        settle_and_remove(workspace, rx, root);
    }
    #[test]
    fn mocked_native_save_all_waits_for_each_saved_receipt_before_next_chooser() {
        let (mut workspace, rx, root) = isolated();
        workspace.execute("edit.insert", &CommandArgs { text: Some("one".into()), ..Default::default() }, 1).unwrap();
        workspace.file_command("file.new").unwrap();
        workspace.execute("edit.insert", &CommandArgs { text: Some("two".into()), ..Default::default() }, 2).unwrap();
        workspace.file_command("file.saveAll").unwrap();
        let mut gate = Gate::default();
        let mut fake = Fake::default();
        gate.event(&mut workspace, Event::Ready);
        gate.sync(&mut workspace, 1, &mut fake);
        let first = fake.request.take().unwrap();
        let first_id = first.ticket.document();
        gate.event(
            &mut workspace,
            Event::Completed { ticket: *first.ticket, outcome: Outcome::Accepted(root.join("one.txt")) },
        );
        assert!(workspace.file_prompt().is_none());
        gate.sync(&mut workspace, 1, &mut fake);
        assert_eq!(fake.submits, 1);
        drain_until(&mut workspace, &rx, |workspace| workspace.file_prompt().is_some());
        gate.sync(&mut workspace, 1, &mut fake);
        let second = fake.request.take().unwrap();
        assert_ne!(first_id, second.ticket.document());
        assert_eq!(std::fs::read(root.join("one.txt")).unwrap(), b"one");
        gate.event(&mut workspace, Event::Completed { ticket: *second.ticket, outcome: Outcome::Cancelled });
        assert!(!root.join("two.txt").exists());
        assert!(workspace.editor().unwrap().document().is_dirty());
        assert!(workspace.file_prompt().is_none());
        settle_and_remove(workspace, rx, root);
    }
}
