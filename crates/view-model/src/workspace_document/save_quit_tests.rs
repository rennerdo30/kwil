//! Real save/quit lifecycle migrated from ui-tui's obsolete SaveFlow scaffold.
//! SOURCE ONLY: ROOT runs the focused selectors on the frozen candidate.
use super::*;
use crate::{
    CommandArgs, Document, Editor, View,
    document_worker::{SaveQuitTestControl, SaveQuitTestEvent, SaveQuitTestGate},
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc,
};

/// One exact Workspace/transport pair and one original deadline. The same
/// per-worker gate holds effects, publication, and the real owner-free wakes.
/// It never carries a request, completion, snapshot, or replacement owner.
struct Fixture {
    owner: Option<(Workspace, WorkspaceDocumentTransport)>,
    events: mpsc::Receiver<SaveQuitTestEvent>,
    release: mpsc::SyncSender<()>,
    holding: Arc<AtomicBool>,
    deadline: Instant,
    root: PathBuf,
    budget: AllocationBudget,
    baseline_bytes: usize,
}
impl Fixture {
    fn new(deny_next: bool) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = (0..128)
            .find_map(|_| {
                let path = std::env::temp_dir().join(format!(
                    "kwil-save-quit-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                match std::fs::create_dir(&path) {
                    Ok(()) => Some(path),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => None,
                    Err(error) => panic!("exclusive save fixture: {error}"),
                }
            })
            .expect("bounded exclusive fixture namespace");
        let deadline = Instant::now() + Duration::from_secs(10);
        let budget = AllocationBudget::default();
        let baseline_bytes = budget.retained_bytes();
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        workspace.set_rows(3);
        let (events_tx, events) = mpsc::sync_channel(8);
        let (release, released) = mpsc::sync_channel(1);
        let holding = Arc::new(AtomicBool::new(true));
        let gate = SaveQuitTestGate {
            events: events_tx,
            release: Arc::new(Mutex::new(released)),
            holding: holding.clone(),
            deadline,
        };
        let control = SaveQuitTestControl { document: workspace.active_id(), deny_next, gate: gate.clone() };
        let transport = workspace
            .start_bounded_document_worker_with(
                Some(root.join("recovery")),
                &budget,
                move || {
                    gate.pause(SaveQuitTestEvent::Ready);
                    Ok(())
                },
                move |budget, wake| {
                    crate::document_worker::DocumentWorker::spawn_bounded_save_quit_test(budget, wake, control)
                },
                |worker, _, request| worker.try_submit(request),
            )
            .unwrap();
        let mut fixture = Self {
            owner: Some((workspace, transport)),
            events,
            release,
            holding,
            deadline,
            root,
            budget,
            baseline_bytes,
        };
        fixture.until(|workspace, transport| workspace.recovery_ready() && !transport.status().occupied);
        fixture
    }
    fn workspace(&self) -> &Workspace {
        &self.owner.as_ref().unwrap().0
    }
    fn workspace_mut(&mut self) -> &mut Workspace {
        &mut self.owner.as_mut().unwrap().0
    }
    fn next(&self) -> SaveQuitTestEvent {
        self.events
            .recv_timeout(self.deadline.saturating_duration_since(Instant::now()))
            .expect("original save/quit deadline")
    }
    fn resume(&self) {
        self.release.try_send(()).expect("one explicit original-worker release");
    }
    fn accept_ready(&mut self) -> Option<ResultInfo> {
        let (workspace, transport) = self.owner.as_mut().unwrap();
        let info = transport.result_info();
        let update = transport.receive(workspace, true).expect("actual sealed Workspace acceptance");
        if info.is_some_and(|info| info.kind == ResultKind::Saved) {
            assert!(update.is_some());
            assert!(transport.status().occupied, "accepted residual still belongs to the held original producer");
        }
        info
    }
    fn until(&mut self, done: impl Fn(&Workspace, &WorkspaceDocumentTransport) -> bool) {
        for _ in 0..512 {
            let (workspace, transport) = self.owner.as_ref().unwrap();
            if done(workspace, transport) {
                return;
            }
            assert_eq!(self.next(), SaveQuitTestEvent::Ready);
            self.accept_ready();
            self.resume();
        }
        panic!("bounded real result/capacity sequence");
    }
    fn phase(&mut self) -> SaveQuitTestEvent {
        for _ in 0..512 {
            match self.next() {
                SaveQuitTestEvent::Ready => {
                    self.accept_ready();
                    self.resume();
                }
                event => return event,
            }
        }
        panic!("bounded original-worker save phase");
    }
    fn before_effect(&mut self) -> u64 {
        let SaveQuitTestEvent::BeforeEffect { revision } = self.phase() else { panic!("expected real save effect") };
        revision
    }
    fn before_publish(&mut self, revision: u64, error: Option<io::ErrorKind>) {
        assert_eq!(self.phase(), SaveQuitTestEvent::BeforePublish { revision, error });
    }
    fn accept_save(&mut self, revision: u64) {
        for _ in 0..512 {
            assert_eq!(self.next(), SaveQuitTestEvent::Ready);
            if self.accept_ready().is_some_and(|info| info.kind == ResultKind::Saved && info.revision == Some(revision))
            {
                return; // Keep the producer held through the ownership assertions.
            }
            self.resume();
        }
        panic!("bounded original saved delivery");
    }
    fn retire(&mut self) {
        self.resume();
        self.until(|_, transport| !transport.status().occupied);
    }
    fn edit(&mut self, text: &str) {
        self.workspace_mut()
            .execute("edit.insert", &CommandArgs { text: Some(text.into()), ..Default::default() }, 1)
            .unwrap();
    }
    fn quit_to(&mut self, path: &std::path::Path) {
        self.workspace_mut().file_command("file.saveAndQuit").unwrap();
        if let Some(prompt) = self.workspace_mut().file_prompt_mut() {
            prompt.text = path.to_str().unwrap().into();
            self.workspace_mut().submit_file_prompt().unwrap();
        }
        assert!(self.workspace().close_pending());
    }
    fn complete(&mut self, revision: u64) {
        self.resume();
        self.before_publish(revision, None);
        self.resume();
        self.accept_save(revision);
        self.retire();
    }
    fn unrequested(&mut self, path: PathBuf) {
        // A real save from an independent empty editor, addressed to this
        // Workspace but deliberately lacking its SaveFlight. No fake result.
        let mut editor = Editor::new(Document::new());
        let plan = editor.document().save_plan();
        let request = editor.save_request(Some(path)).unwrap();
        let document = self.workspace().active_id();
        self.workspace_mut().files.queue.push_back(DocumentRequest::Save { document, request, plan });
        self.workspace_mut().pump_files();
    }
    fn finish(mut self) {
        self.holding.store(false, Ordering::Release);
        // A final capacity wake can still be parked even when its slot is empty.
        match self.release.try_send(()) {
            Ok(()) | Err(mpsc::TrySendError::Full(())) => {}
            Err(error) => panic!("original gate disconnected before cleanup: {error}"),
        }
        let (workspace, transport) = self.owner.take().unwrap();
        let deadline = self.deadline;
        let caller = std::thread::current().id();
        std::thread::spawn(move || {
            assert_ne!(std::thread::current().id(), caller);
            let mut bundle = match workspace.into_document_shutdown(transport) {
                ControlFlow::Continue(bundle) => bundle,
                ControlFlow::Break(refusal) => panic!("exact save owner refused shutdown binding: {refusal:?}"),
            };
            bundle.bind_search_sources(DocumentSearchBinding::NoSearchWorker).unwrap();
            loop {
                match crate::cleanup_test_support::cleanup_until(
                    &mut bundle,
                    deadline,
                    |owner, deadline| {
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        assert!(!remaining.is_zero(), "original cleanup deadline; exact bundle retained");
                        owner
                            .settle_until(deadline)
                            .map_or_else(|error| ControlFlow::Break((owner, error)), ControlFlow::Continue)
                    },
                    |error| {
                        matches!(
                            error,
                            DocumentShutdownReason::Transport(DocumentResultSettlementReason::ThreadExitPending)
                        )
                    },
                )
                .into_flow()
                {
                    ControlFlow::Continue(DocumentShutdownStatus::SourcesReleased) => {
                        assert!(bundle.workspace.as_ref().unwrap().views.is_empty());
                        assert!(bundle.workspace.as_ref().unwrap().closed_views.is_empty());
                    }
                    ControlFlow::Continue(DocumentShutdownStatus::Complete) => break,
                    ControlFlow::Break(
                        crate::cleanup_test_support::CleanupFailure::Expired { .. }
                        | crate::cleanup_test_support::CleanupFailure::CompletedLate { .. },
                    ) => {
                        panic!("original cleanup deadline; exact bundle retained");
                    }
                    other => panic!("save fixture cleanup retained its exact bundle: {other:?}"),
                }
            }
            assert_eq!(bundle.deadline(), Some(deadline));
            assert!(bundle.workspace.is_none());
            assert!(bundle.ticket.is_none(), "actual producer join precedes cleanup success");
        })
        .join()
        .unwrap();
        assert_eq!(
            self.budget.retained_bytes(),
            self.baseline_bytes,
            "sealed source retirement returns the original aggregate to its pre-admission baseline"
        );
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.take() {
            // A failed assertion is not UI cleanup or permission to ACK a save.
            std::mem::forget(owner);
            eprintln!("save/quit assertion retained exact owners: {}", self.root.display());
        }
    }
}

#[test]
fn savequit_success_marks_only_the_requested_revision_clean_and_exits() {
    let mut fixture = Fixture::new(false);
    let path = fixture.root.join("keep.txt");
    fixture.edit("keep me");
    fixture.quit_to(&path);
    let revision = fixture.before_effect();
    assert!(fixture.workspace().editor().unwrap().document().is_dirty());
    assert!(!fixture.workspace().wants_exit());
    assert!(!path.exists(), "request did not perform storage on the UI");
    fixture.resume();
    fixture.before_publish(revision, None);
    assert_eq!(std::fs::read(&path).unwrap(), b"keep me");
    assert!(
        fixture.workspace().editor().unwrap().document().is_dirty(),
        "physical save alone cannot publish clean state"
    );
    fixture.resume();
    fixture.accept_save(revision);
    let document = fixture.workspace().editor().unwrap().document();
    assert_eq!(document.revision(), revision);
    assert_eq!(document.saved_revision(), revision);
    assert!(!document.is_dirty());
    fixture.retire();
    fixture.until(|workspace, _| workspace.wants_exit());
    fixture.finish();
}

#[test]
fn savequit_failure_and_concurrent_edits_keep_document_open() {
    let mut fixture = Fixture::new(true);
    let path = fixture.root.join("before.txt");
    fixture.edit("before");
    fixture.quit_to(&path);
    let revision = fixture.before_effect();
    fixture.resume();
    fixture.before_publish(revision, Some(io::ErrorKind::PermissionDenied));
    assert!(!path.exists(), "one-shot denial happens before the storage effect");
    fixture.resume();
    fixture.accept_save(revision);
    assert!(fixture.workspace().status_text().contains("Save failed: denied"));
    assert!(fixture.workspace().editor().unwrap().document().is_dirty());
    assert!(!fixture.workspace().close_pending());
    assert!(!fixture.workspace().wants_exit());
    fixture.retire();
    fixture.quit_to(&path);
    let requested = fixture.before_effect();
    fixture.edit(" after");
    let current = fixture.workspace().editor().unwrap().document().revision();
    assert_ne!(requested, current);
    fixture.complete(requested); // Same worker now performs the concrete save.
    assert_eq!(fixture.workspace().editor().unwrap().document().revision(), current);
    assert!(fixture.workspace().editor().unwrap().document().is_dirty());
    assert_eq!(fixture.workspace().line(0), Some(b"before after".as_slice()));
    assert_eq!(std::fs::read(&path).unwrap(), b"before");
    assert!(!fixture.workspace().close_pending());
    assert!(!fixture.workspace().wants_exit());
    fixture.finish();
}

#[test]
fn savequit_cancelled_prompt_and_failed_request_do_not_close_later() {
    let mut fixture = Fixture::new(false);
    let path = fixture.root.join("cancel.txt");
    fixture.workspace_mut().file_command("file.saveAndQuit").unwrap();
    assert!(fixture.workspace().file_prompt().is_some());
    fixture.workspace_mut().cancel_file_prompt();
    assert!(!fixture.workspace().close_pending());
    let id = fixture.workspace().active_id();
    fixture.workspace_mut().save_document(id, Some(path.clone())).unwrap();
    let revision = fixture.before_effect();
    let error = fixture.workspace_mut().file_command("file.saveAndQuit").unwrap_err();
    assert!(error.to_string().contains("save already in progress"));
    assert!(!fixture.workspace().close_pending());
    fixture.complete(revision);
    assert!(!fixture.workspace().wants_exit());
    fixture.workspace_mut().save_document(id, None).unwrap();
    let revision = fixture.before_effect();
    fixture.complete(revision);
    assert!(!fixture.workspace().wants_exit());
    assert!(!fixture.workspace().close_pending());
    fixture.finish();
}

#[test]
fn savequit_unrequested_completion_never_exits_even_when_clean() {
    let mut fixture = Fixture::new(false);
    let path = fixture.root.join("unrequested.txt");
    fixture.workspace_mut().file_command("file.saveAndQuit").unwrap();
    assert!(fixture.workspace().close_pending());
    fixture.unrequested(path.clone());
    let revision = fixture.before_effect();
    fixture.complete(revision);
    assert_eq!(std::fs::read(path).unwrap(), b"");
    assert!(!fixture.workspace().editor().unwrap().document().is_dirty());
    assert!(!fixture.workspace().wants_exit());
    assert!(fixture.workspace().file_prompt().is_some(), "unrequested result cannot satisfy the pending prompt");
    fixture.workspace_mut().cancel_file_prompt();
    fixture.finish();
}

#[test]
fn savequit_stale_completion_does_not_close_after_undo_to_clean_revision() {
    let mut fixture = Fixture::new(false);
    let path = fixture.root.join("current.txt");
    let old_path = fixture.root.join("earlier.txt");
    let id = fixture.workspace().active_id();
    fixture.workspace_mut().save_document(id, Some(path.clone())).unwrap();
    let clean_revision = fixture.before_effect();
    fixture.complete(clean_revision);
    assert!(!fixture.workspace().wants_exit(), "ordinary initial save cannot authorize exit");
    fixture.unrequested(old_path.clone());
    let old_revision = fixture.before_effect();
    fixture.resume();
    fixture.before_publish(old_revision, None); // Genuine persisted old result remains on its producer.
    fixture.edit("new revision");
    fixture.quit_to(&path);
    let pending = fixture.workspace().files.saves[&id].resume_key(id);
    assert_ne!(pending.revision, old_revision);
    fixture.workspace_mut().execute("edit.undo", &CommandArgs::default(), 2).unwrap();
    assert_eq!(fixture.workspace().editor().unwrap().document().revision(), clean_revision);
    assert!(!fixture.workspace().editor().unwrap().document().is_dirty());
    fixture.resume();
    fixture.accept_save(old_revision);
    assert_eq!(
        fixture.workspace().files.saves[&id].resume_key(id),
        pending,
        "unmatched old completion preserves the newer genuine flight"
    );
    assert!(!fixture.workspace().wants_exit());
    assert!(fixture.workspace().close_pending());
    assert!(!fixture.workspace().editor().unwrap().document().is_dirty());
    fixture.retire();
    let newer_revision = fixture.before_effect();
    assert_eq!(newer_revision, pending.revision);
    fixture.complete(newer_revision);
    assert_eq!(std::fs::read(old_path).unwrap(), b"");
    assert_eq!(std::fs::read(path).unwrap(), b"new revision");
    assert_eq!(fixture.workspace().editor().unwrap().document().revision(), clean_revision);
    assert_eq!(fixture.workspace().line(0), Some(b"".as_slice()));
    assert!(!fixture.workspace().wants_exit());
    assert!(!fixture.workspace().close_pending(), "matching save of a different current revision clears intent");
    fixture.finish();
}

#[test]
fn savequit_successful_trim_installs_a_clean_saved_revision() {
    let mut fixture = Fixture::new(false);
    let path = fixture.root.join("trim.txt");
    fixture.workspace_mut().editor_mut().unwrap().settings.editor.trim_trailing_whitespace_on_save = true;
    fixture.edit("trim me  ");
    fixture.quit_to(&path);
    let requested = fixture.before_effect();
    fixture.resume();
    fixture.before_publish(requested, None);
    assert_eq!(std::fs::read(&path).unwrap(), b"trim me");
    fixture.resume();
    fixture.accept_save(requested);
    let document = fixture.workspace().editor().unwrap().document();
    assert!(!document.is_dirty());
    assert_ne!(document.revision(), requested, "trim installs its own clean revision");
    assert_eq!(document.revision(), document.saved_revision());
    let mut bytes = Vec::new();
    document.snapshot().write_to(&mut bytes).unwrap();
    assert_eq!(bytes, b"trim me");
    fixture.retire();
    fixture.until(|workspace, _| workspace.wants_exit());
    fixture.finish();
}

#[test]
fn saved_closed_tab_shutdown_retains_exact_final_journal_durability() {
    let mut f = Fixture::new(false);
    let path = f.root.join("closed-save.txt");
    let id = f.workspace().active_id();
    f.edit("saved closed tab\n");
    f.workspace_mut().save_document(id, Some(path.clone())).unwrap();
    let revision = f.before_effect();
    f.complete(revision);
    assert_eq!(std::fs::read(&path).unwrap(), b"saved closed tab\n");
    assert!(!f.workspace().views[&id].editor().unwrap().document().is_dirty());
    assert!(f.workspace_mut().close(id, false).unwrap());
    let editor = f.workspace().closed_views[&id].editor().unwrap();
    let target = (editor.document().revision(), editor.next_edit_sequence() - 1);
    f.until(|workspace, transport| {
        workspace.files.queue.is_empty()
            && !transport.status().occupied
            && workspace.files.durable.get(&id) == Some(&target)
    });
    assert_eq!(f.workspace().files.durable.get(&id), Some(&target));
    f.finish();
}
