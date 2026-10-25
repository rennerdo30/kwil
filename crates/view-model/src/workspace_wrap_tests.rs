//! Tiny source-only controls; ROOT owns execution and acceptance.
use super::*;
use crate::{Cursor, View, visual_rows::VisualMotion};
use std::sync::mpsc::{self, Receiver};

fn accepted<B, T>(flow: ControlFlow<B, T>) -> T {
    match flow {
        ControlFlow::Continue(value) => value,
        ControlFlow::Break(_) => panic!("unexpected owner refusal"),
    }
}
pub(crate) fn settle_until(
    mut ticket: WorkspaceWrapSettlementTicket,
    deadline: Instant,
) -> Result<WorkspaceWrapSettled, WorkspaceWrapSettlementFailure> {
    loop {
        match ticket.settle(deadline.saturating_duration_since(Instant::now())) {
            Err(failure) if matches!(failure.error, WrapError::ThreadExitPending) && Instant::now() < deadline => {
                ticket = *failure.ticket;
                std::thread::park_timeout(
                    Duration::from_millis(1).min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            result => return result,
        }
    }
}
fn geometry() -> LayoutGeometry {
    LayoutGeometry { columns: 3, rows: 2, tab_size: 4, ..Default::default() }
}
fn setup() -> (Workspace, WorkspaceWrap, WrapHost, Receiver<()>) {
    let mut workspace = Workspace::new(View::new(None)).unwrap();
    let editor = workspace.editor_mut().unwrap();
    editor.settings.editor.word_wrap = true;
    editor.insert_text("abcdef", Duration::ZERO).unwrap();
    editor.set_cursors(vec![Cursor::caret(0)], 0).unwrap();
    let (wake, ready) = mpsc::sync_channel(32);
    let host = WrapHost::new(move || {
        let _ = wake.try_send(());
    })
    .unwrap();
    let owner = WorkspaceWrap::new(&mut workspace, &host).unwrap();
    (workspace, owner, host, ready)
}
fn stop(workspace: &mut Workspace, owner: WorkspaceWrap, host: WrapHost) {
    let ticket = accepted(owner.stop(host));
    let receipt = settle_until(ticket, Instant::now() + Duration::from_secs(2)).unwrap();
    accepted(workspace.acknowledge_wrap_settled(receipt));
    assert!(workspace.wrap_cleanup_ready());
}

#[test]
fn frontend_rearm_requires_retired_wrap_and_preserves_the_original_document() {
    let (mut workspace, owner, host, _) = setup();
    let budget = host.budget().clone();
    let document = workspace.editor().unwrap().document() as *const crate::Document;
    let epoch = workspace.editor().unwrap().document().source_epoch;
    let generation = workspace.wrap.path_generation;
    assert!(!workspace.unstarted_frontend_ready());
    workspace.begin_wrap_shutdown();
    assert!(matches!(workspace.rearm_unstarted_frontend(), Err(WrapError::Busy)));
    assert_eq!(workspace.wrap.path_generation, generation);
    std::thread::scope(|scope| scope.spawn(|| stop(&mut workspace, owner, host)).join().unwrap());
    let next = WrapHost::with_budget(budget, || {}).unwrap();
    assert!(matches!(WorkspaceWrap::new(&mut workspace, &next), Err(WrapError::Closed)));
    workspace.rearm_unstarted_frontend().unwrap();
    assert!(workspace.unstarted_frontend_ready());
    assert_eq!(workspace.editor().unwrap().document() as *const crate::Document, document);
    assert_eq!(workspace.editor().unwrap().document().source_epoch, epoch);
    assert_eq!(workspace.wrap.path_generation, generation.map(|value| value + 1));
    let next_owner = WorkspaceWrap::new(&mut workspace, &next).unwrap();
    std::thread::scope(|scope| scope.spawn(|| stop(&mut workspace, next_owner, next)).join().unwrap());
}

#[test]
fn frontend_rearm_refuses_accepted_document_work_exit_and_exhausted_generation() {
    let mut workspace = Workspace::new(View::new(None)).unwrap();
    let generation = workspace.wrap.path_generation;
    assert!(workspace.unstarted_frontend_ready());
    workspace.files.document_started = true;
    assert!(!workspace.unstarted_frontend_ready());
    assert!(matches!(workspace.rearm_unstarted_frontend(), Err(WrapError::Closed)));
    workspace.files.document_started = false;
    workspace.files.bounded_document = true;
    assert!(matches!(workspace.rearm_unstarted_frontend(), Err(WrapError::Closed)));
    workspace.files.bounded_document = false;
    workspace.files.quit = true;
    assert!(matches!(workspace.rearm_unstarted_frontend(), Err(WrapError::Closed)));
    workspace.files.quit = false;
    workspace.wrap.settling = true;
    assert!(matches!(workspace.rearm_unstarted_frontend(), Err(WrapError::Busy)));
    assert_eq!(workspace.wrap.path_generation, generation);
    workspace.wrap.settling = false;
    workspace.wrap.path_generation = None;
    assert!(matches!(workspace.rearm_unstarted_frontend(), Err(WrapError::Admission(_))));
    assert!(workspace.wrap.path_generation.is_none());
}

// Old-source fail-first item: the original constructor and recovery APIs are
// sufficient; ROOT can overlay only this test before the validator repair.
#[test]
fn frontend_file_backed_rearm_transfers_original_checked_open_and_rebinds_without_reopen() {
    let root = std::env::temp_dir().join(format!(
        "kwil-file-frontend-rebind-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
    ));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("original.txt");
    std::fs::write(&path, b"original queued file\n").unwrap();
    let file = kwil_core_text::TextFile::open(&path).unwrap();
    assert!(file.index().is_none());
    let source = file.path().as_os_str().as_encoded_bytes().as_ptr() as usize;
    let mut workspace = Workspace::new(View::new(Some((path.clone(), file)))).unwrap();
    let id = workspace.active_id();
    let incarnation = Arc::downgrade(&workspace.views[&id].input_incarnation);
    let request_address =
        workspace.files.queue.front().unwrap() as *const crate::document_worker::DocumentRequest as usize;
    let crate::document_worker::DocumentRequest::OpenChecked { ticket, file: Some(queued), .. } =
        workspace.files.queue.front().unwrap()
    else {
        panic!("actual startup file must remain in the existing checked-open request");
    };
    let ticket = *ticket;
    assert_eq!(queued.path().as_os_str().as_encoded_bytes().as_ptr() as usize, source);
    assert!(!workspace.files.document_started);
    assert!(workspace.files.worker.is_none());
    if !workspace.unstarted_frontend_ready() {
        std::mem::forget(workspace); // retain the old-source negative fixture's original file on failure
        panic!("initial original-file request must permit frontend fallback");
    }
    let crate::document_worker::DocumentRequest::OpenChecked { path: queued_path, .. } =
        workspace.files.queue.front_mut().unwrap()
    else {
        unreachable!()
    };
    queued_path.push("mismatched-header");
    assert!(!workspace.unstarted_frontend_ready(), "cached source and queued path identities must agree");
    let crate::document_worker::DocumentRequest::OpenChecked { path: queued_path, .. } =
        workspace.files.queue.front_mut().unwrap()
    else {
        unreachable!()
    };
    assert!(queued_path.pop());
    assert!(workspace.unstarted_frontend_ready());
    let deadline = Instant::now() + Duration::from_secs(2);
    let host = WrapHost::new(|| {}).unwrap();
    let budget = host.budget().clone();
    let owner = WorkspaceWrap::new(&mut workspace, &host).unwrap();
    workspace.begin_wrap_shutdown();
    assert!(matches!(workspace.rearm_unstarted_frontend(), Err(WrapError::Busy)));
    std::thread::scope(|scope| {
        let workspace = &mut workspace;
        scope
            .spawn(move || {
                let ticket = accepted(owner.stop(host));
                let remaining = deadline.saturating_duration_since(Instant::now());
                assert!(!remaining.is_zero());
                let receipt = settle_until(ticket, deadline).unwrap();
                accepted(workspace.acknowledge_wrap_settled(receipt));
            })
            .join()
            .unwrap();
    });
    assert!(workspace.wrap_cleanup_ready());
    assert!(!workspace.unstarted_frontend_ready(), "the accepted wrap-exit fence must be explicitly rearmed");
    workspace.rearm_unstarted_frontend().unwrap();
    assert!(workspace.unstarted_frontend_ready());
    assert_eq!(workspace.files.queue.len(), 1);
    assert_eq!(workspace.binary.flights.len(), 1);
    assert_eq!(
        workspace.files.queue.front().unwrap() as *const crate::document_worker::DocumentRequest as usize,
        request_address
    );
    let crate::document_worker::DocumentRequest::OpenChecked {
        ticket: retained,
        path: retained_path,
        file: Some(queued),
    } = workspace.files.queue.front().unwrap()
    else {
        panic!("fallback dropped or reopened the original startup source");
    };
    assert_eq!(*retained, ticket);
    assert_eq!(retained_path, &path);
    assert_eq!(queued.path().as_os_str().as_encoded_bytes().as_ptr() as usize, source);
    assert_eq!(workspace.binary.flights[&id].ticket, ticket);
    assert!(matches!(workspace.binary.flights[&id].phase, super::super::BinaryPhase::Opening));
    assert_eq!(incarnation.strong_count(), 1);
    let next = WrapHost::with_budget(budget, || {}).unwrap();
    let owner = WorkspaceWrap::new(&mut workspace, &next).unwrap();
    std::thread::scope(|scope| {
        scope
            .spawn(move || {
                let ticket = accepted(owner.stop(next));
                let remaining = deadline.saturating_duration_since(Instant::now());
                assert!(!remaining.is_zero());
                let receipt = settle_until(ticket, deadline).unwrap();
                accepted(workspace.acknowledge_wrap_settled(receipt));
                assert_eq!(workspace.files.queue.len(), 1);
                assert!(!workspace.files.document_started);
                assert!(workspace.files.worker.is_none());
                drop(workspace); // original queued file retires only OFF UI
            })
            .join()
            .unwrap();
    });
    assert!(incarnation.upgrade().is_none());
    assert!(root.is_absolute() && root.starts_with(std::env::temp_dir()));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn frontend_initial_open_bijection_rejects_mismatch_duplicate_orphan_and_ever_started_work() {
    let mut workspace = Workspace::new(View::new(None)).unwrap();
    let id = workspace.active_id();
    let empty = workspace.views.get_mut(&id).unwrap().editor.take();
    std::thread::scope(|scope| scope.spawn(move || drop(empty)).join().unwrap());
    let path = std::env::temp_dir().join("kwil-unopened-startup-header.txt");
    workspace.views.get_mut(&id).unwrap().path = Some(path.clone());
    workspace.session.set_path(id, path.clone()).unwrap();
    let request = workspace.checked_open_request(id, 0, path.clone()).unwrap();
    workspace.files.queue.push_back(request);
    assert!(workspace.unstarted_frontend_ready(), "matching source-free initial headers remain admissible");
    workspace.files.document_started = true;
    assert!(!workspace.unstarted_frontend_ready());
    assert!(matches!(workspace.rearm_unstarted_frontend(), Err(WrapError::Closed)));
    workspace.files.document_started = false; // fixture-only restoration of the tested sticky guard
    workspace.binary.flights.get_mut(&id).unwrap().preview_ready = true;
    assert!(!workspace.unstarted_frontend_ready());
    workspace.binary.flights.get_mut(&id).unwrap().preview_ready = false;
    let crate::document_worker::DocumentRequest::OpenChecked { ticket, .. } =
        workspace.files.queue.front_mut().unwrap()
    else {
        unreachable!()
    };
    let original = *ticket;
    ticket.generation += 1;
    assert!(!workspace.unstarted_frontend_ready(), "mismatched request identity cannot authorize transfer");
    let crate::document_worker::DocumentRequest::OpenChecked { ticket, .. } =
        workspace.files.queue.front_mut().unwrap()
    else {
        unreachable!()
    };
    *ticket = original;
    workspace.files.queue.push_back(crate::document_worker::DocumentRequest::OpenChecked {
        ticket: original,
        path,
        file: None,
    });
    assert!(!workspace.unstarted_frontend_ready(), "duplicate request cannot match one flight twice");
    workspace.files.queue.pop_back();
    let request = workspace.files.queue.pop_front().unwrap();
    assert!(!workspace.unstarted_frontend_ready(), "orphan Opening flight is not initial work");
    workspace.files.queue.push_back(request);
    workspace.files.queue.push_back(crate::document_worker::DocumentRequest::FlushJournals);
    assert!(!workspace.unstarted_frontend_ready(), "all non-initial accepted request kinds stay refused");
    workspace.files.queue.pop_back();
    assert!(workspace.unstarted_frontend_ready());
    std::thread::scope(|scope| scope.spawn(move || drop(workspace)).join().unwrap());
}

fn publish(workspace: &mut Workspace, owner: &mut WorkspaceWrap, host: &mut WrapHost, ready: &Receiver<()>) {
    let context = owner.prepare_context(workspace, geometry(), BodyOwner::Text).unwrap();
    host.sync(workspace.editor(), context).unwrap();
    owner.lend_source(workspace, context, host).unwrap();
    for _ in 0..16 {
        let update = owner.drain_host(host, workspace.editor_mut()).unwrap();
        assert!(update.failure.is_none());
        if update.publication.is_some() {
            return;
        }
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
    }
    panic!("bounded worker publication count exhausted");
}

#[test]
fn workspace_wrap_context_tracks_same_byte_external_normalization_but_not_mutable_borrow() {
    let (mut workspace, mut owner, host, _) = setup();
    let before = owner.prepare_context(&workspace, geometry(), BodyOwner::Text).unwrap();
    assert!(owner.context_is_current(&workspace, before));
    let _ = workspace.editor_mut().unwrap();
    assert_eq!(before, owner.prepare_context(&workspace, geometry(), BodyOwner::Text).unwrap());
    workspace.editor_mut().unwrap().set_cursors(vec![Cursor::caret(0)], 0).unwrap();
    assert!(!owner.context_is_current(&workspace, before));
    let after = owner.prepare_context(&workspace, geometry(), BodyOwner::Text).unwrap();
    assert_eq!(before.key, after.key);
    assert!(after.cursor_generation > before.cursor_generation);
    stop(&mut workspace, owner, host);
}

#[test]
fn workspace_wrap_context_fences_path_settings_input_incarnation_and_body() {
    let (mut workspace, mut owner, host, _) = setup();
    let first = owner.prepare_context(&workspace, geometry(), BodyOwner::Text).unwrap();
    workspace.wrap.path_changed();
    let path = owner.prepare_context(&workspace, geometry(), BodyOwner::Text).unwrap();
    assert!(path.path_generation > first.path_generation);
    assert_eq!(path.key.source, first.key.source);
    let settings = workspace.editor().unwrap().settings.clone();
    workspace.set_editor_settings(&settings);
    let settings = owner.prepare_context(&workspace, geometry(), BodyOwner::Text).unwrap();
    assert!(settings.key.geometry.settings_generation > path.key.geometry.settings_generation);
    let preview = owner.prepare_context(&workspace, geometry(), BodyOwner::MarkdownPreview).unwrap();
    assert_eq!(preview.key.body, BodyOwner::MarkdownPreview);
    assert!(preview.key.body_generation > settings.key.body_generation);
    let second = workspace.open(View::new(None)).unwrap();
    let other = owner.prepare_context(&workspace, geometry(), BodyOwner::Text).unwrap();
    assert_eq!(other.key.source.document_id, second.0);
    assert_ne!(other.key.source.incarnation, first.key.source.incarnation);
    assert_ne!(other.key.input_epoch, first.key.input_epoch);
    stop(&mut workspace, owner, host);
}

#[test]
fn workspace_wrap_internal_visual_commit_preserves_external_generation_and_context() {
    let (mut workspace, mut owner, mut host, ready) = setup();
    publish(&mut workspace, &mut owner, &mut host, &ready);
    let before = owner.prepare_context(&workspace, geometry(), BodyOwner::Text).unwrap();
    host.navigate(VisualMotion::Down, 1, false).unwrap();
    publish(&mut workspace, &mut owner, &mut host, &ready);
    assert_eq!(workspace.editor().unwrap().primary_cursor().head, 3);
    assert_eq!(before, owner.prepare_context(&workspace, geometry(), BodyOwner::Text).unwrap());
    assert!(!owner.has_refused_source());
    stop(&mut workspace, owner, host);
}

#[test]
fn workspace_wrap_source_admission_uses_host_budget_and_disabled_host_pins_nothing() {
    let captured = std::cell::Cell::new(false);
    let exhausted = AllocationBudget::new(0);
    let refused = admitted_source(&exhausted, SourceIdentity::default(), || {
        captured.set(true);
        kwil_core_text::Snapshot::default()
    });
    assert!(matches!(refused, Err(WrapError::Admission(_))));
    assert!(!captured.get(), "header admission precedes even O(1) snapshot capture");
    let (mut workspace, mut owner, mut host, ready) = setup();
    publish(&mut workspace, &mut owner, &mut host, &ready);
    host.request_fill(Default::default()).unwrap();
    let peak = host.budget().peak_bytes();
    workspace.editor_mut().unwrap().settings.editor.word_wrap = false;
    let disabled = owner.prepare_context(&workspace, geometry(), BodyOwner::Text).unwrap();
    // A disabled/stale source context cannot create a new source even while
    // the old host still wants its previously registered fill.
    assert!(matches!(owner.lend_source(&workspace, disabled, &mut host), Err(WrapError::Stale)));
    assert!(!owner.has_refused_source());
    assert_eq!(host.budget().peak_bytes(), peak);
    stop(&mut workspace, owner, host);
}

#[test]
fn workspace_wrap_wrong_host_and_foreign_receipt_return_exact_owners_and_keep_cleanup_blocked() {
    let (mut workspace, owner, host, _) = setup();
    assert!(!workspace.prepare_binary_shutdown(), "gate precedes document disposal");
    assert!(matches!(WorkspaceWrap::new(&mut workspace, &host), Err(WrapError::Busy)));
    let mut other_workspace = Workspace::new(View::new(None)).unwrap();
    let other_host = WrapHost::new(|| {}).unwrap();
    let other_owner = WorkspaceWrap::new(&mut other_workspace, &other_host).unwrap();
    let (owner, other_host) = match owner.stop(other_host) {
        ControlFlow::Break(pair) => pair,
        ControlFlow::Continue(_) => panic!("wrong worker generated a ticket"),
    };
    let receipt = settle_until(accepted(owner.stop(host)), Instant::now() + Duration::from_secs(2)).unwrap();
    let receipt = match other_workspace.acknowledge_wrap_settled(receipt) {
        ControlFlow::Break(receipt) => receipt,
        ControlFlow::Continue(()) => panic!("foreign receipt opened cleanup"),
    };
    assert!(!other_workspace.wrap_cleanup_ready());
    accepted(workspace.acknowledge_wrap_settled(receipt));
    stop(&mut other_workspace, other_owner, other_host);
}

#[test]
fn workspace_wrap_same_host_cannot_register_a_second_workspace_obligation() {
    let (mut first, owner, host, _) = setup();
    let mut second = Workspace::new(View::new(None)).unwrap();
    let budget = host.budget().clone();
    assert!(matches!(WorkspaceWrap::new(&mut second, &host), Err(WrapError::Busy)));
    assert!(!first.wrap_cleanup_ready());
    assert!(second.wrap_cleanup_ready(), "refused registration cannot publish an obligation");
    assert_eq!(second.wrap_settlement_state(), WorkspaceWrapSettlementState::Ready);
    let receipt = settle_until(accepted(owner.stop(host)), Instant::now() + Duration::from_secs(2)).unwrap();
    let receipt = match second.acknowledge_wrap_settled(receipt) {
        ControlFlow::Break(receipt) => receipt,
        ControlFlow::Continue(()) => panic!("refused workspace accepted the first owner's receipt"),
    };
    assert!(!first.wrap_cleanup_ready(), "only the original acknowledgement may open cleanup");
    assert!(second.wrap_cleanup_ready());
    accepted(first.acknowledge_wrap_settled(receipt));
    assert!(first.wrap_cleanup_ready());
    assert_eq!(
        budget.retained_bytes(),
        AllocationBudget::new(0).retained_bytes(),
        "refused registration leaked no header charge"
    );
}

#[test]
fn workspace_wrap_failed_registration_releases_provisional_host_claim_and_header_admission() {
    let mut first = Workspace::new(View::new(None)).unwrap();
    let mut second = Workspace::new(View::new(None)).unwrap();
    let host = WrapHost::new(|| {}).unwrap();
    let budget = host.budget().clone();
    let refused = WorkspaceWrap::register(&mut first, &host, |budget, bytes| {
        let _header = budget.reserve(bytes)?;
        Err(WrapError::Admission("injected registration refusal after header admission"))
    });
    assert!(matches!(refused, Err(WrapError::Admission("injected registration refusal after header admission"))));
    assert!(first.wrap_cleanup_ready());
    assert!(second.wrap_cleanup_ready());
    let owner = WorkspaceWrap::new(&mut second, &host).unwrap();
    assert!(!second.wrap_cleanup_ready(), "the same host must remain usable after rollback");
    stop(&mut second, owner, host);
    assert!(first.wrap_cleanup_ready());
    assert_eq!(
        budget.retained_bytes(),
        AllocationBudget::new(0).retained_bytes(),
        "failed registration retained no header admission"
    );
}

#[test]
fn workspace_wrap_refusal_saturation_preserves_original_arc_and_constructed_request_storage() {
    let (mut workspace, mut owner, host, _) = setup();
    let source: Arc<dyn WrapSource> =
        Arc::new(SnapshotSource::new(Default::default(), workspace.editor().unwrap().document().snapshot()));
    let original = source.clone();
    accepted(owner.retain_source_refusal(WrapHostSourceRefusal {
        owner: WrapHostSourceOwner::from_source(source),
        error: WrapError::Busy,
    }));
    let request = crate::wrap_session::WrapRequest {
        fence: crate::visual_rows::NavigationFence {
            key: Default::default(),
            cursor_set_generation: 0,
            intent_generation: 0,
        },
        source: original.clone(),
        anchor: Default::default(),
        intent: crate::wrap_session::LayoutIntent::MoveCursors {
            cursors: crate::visual_rows::CursorSet::new(
                host.budget(),
                &[crate::visual_rows::VisualCursor::default()],
                0,
            )
            .unwrap(),
            motion: VisualMotion::Down,
            count: 1,
            extend: false,
        },
    };
    let pointer = match &request.intent {
        crate::wrap_session::LayoutIntent::MoveCursors { cursors, .. } => cursors.cursors().as_ptr(),
        _ => unreachable!(),
    };
    let refused = match owner.retain_source_refusal(WrapHostSourceRefusal {
        owner: WrapHostSourceOwner::from_request(request),
        error: WrapError::Busy,
    }) {
        ControlFlow::Break(refused) => refused,
        ControlFlow::Continue(()) => panic!("overwrote fixed refusal cell"),
    };
    let request = accepted(refused.owner.into_request());
    assert!(Arc::ptr_eq(&request.source, &original));
    match &request.intent {
        crate::wrap_session::LayoutIntent::MoveCursors { cursors, .. } => {
            assert_eq!(pointer, cursors.cursors().as_ptr())
        }
        _ => panic!("lost original request"),
    }
    drop(request);
    stop(&mut workspace, owner, host);
}

#[test]
fn workspace_wrap_existing_document_worker_delivers_typed_receipt_before_cleanup() {
    let (mut workspace, owner, host, _) = setup();
    let (complete, events) = mpsc::channel();
    workspace.files.worker = Some(
        crate::document_worker::DocumentWorker::spawn(move |result| {
            complete.send(result).unwrap();
        })
        .unwrap(),
    );
    let ticket = accepted(owner.stop(host));
    // No accepted-exit marker: refusal returns the same ticket intact.
    let ticket = match workspace.submit_wrap_settlement(ticket, Duration::from_secs(2)) {
        ControlFlow::Break(ticket) => ticket,
        ControlFlow::Continue(()) => panic!("settlement admitted before accepted exit"),
    };
    workspace.begin_wrap_shutdown();
    assert!(workspace.wrap_input_blocked());
    accepted(workspace.submit_wrap_settlement(ticket, Duration::from_secs(2)));
    assert_eq!(workspace.wrap_settlement_state(), WorkspaceWrapSettlementState::Pending);
    assert!(!workspace.wrap_cleanup_ready());
    let result = match workspace.document_result(events.recv_timeout(Duration::from_secs(2)).unwrap()) {
        ControlFlow::Break(crate::document_worker::DocumentResult::WrapSettled(result)) => result,
        _ => panic!("document_result must return the complete typed settlement event"),
    };
    accepted(workspace.accept_wrap_settlement(result));
    assert_eq!(workspace.wrap_settlement_state(), WorkspaceWrapSettlementState::Ready);
    assert!(workspace.wrap_cleanup_ready());
    drop(workspace);
    assert!(matches!(
        events.recv_timeout(Duration::from_secs(2)).unwrap(),
        crate::document_worker::DocumentResult::Stopped(Ok(()))
    ));
}

#[test]
fn workspace_wrap_timeout_retains_ticket_and_source_until_actual_worker_settlement() {
    use std::sync::{Mutex, atomic::AtomicBool};
    struct GatedSource {
        inner: SnapshotSource,
        first: AtomicBool,
        started: mpsc::SyncSender<()>,
        release: Mutex<Receiver<()>>,
        _charge: AllocationCharge,
    }
    impl WrapSource for GatedSource {
        fn run_layout(
            self: Arc<Self>,
            engine: crate::visual_rows::LayoutEngine,
            request: crate::visual_rows::task::TaskRequest,
            driver: &mut dyn crate::visual_rows::TaskDriver,
        ) -> crate::visual_rows::WorkerExit {
            crate::visual_rows::fixtures::run_snapshot(self, None, engine, request, driver)
        }
        fn identity(&self) -> SourceIdentity {
            self.inner.identity()
        }
        fn len(&self) -> u64 {
            self.inner.len()
        }
        fn line_count(&self) -> u64 {
            self.inner.line_count()
        }
        fn line_start(&self, line: u64) -> io::Result<Option<u64>> {
            self.inner.line_start(line)
        }
        fn line_of(&self, byte: u64) -> io::Result<u64> {
            self.inner.line_of(byte)
        }
        fn read(&self, at: u64, output: &mut [u8]) -> io::Result<usize> {
            crate::visual_rows::fixtures::SnapshotHooks::before_snapshot_read(self, output.len())?;
            self.inner.read(at, output)
        }
    }
    impl crate::snapshot_layout_source::SnapshotOwner for GatedSource {
        fn snapshot(&self) -> &kwil_core_text::Snapshot {
            crate::snapshot_layout_source::SnapshotOwner::snapshot(&self.inner)
        }
    }
    impl crate::visual_rows::fixtures::SnapshotHooks for GatedSource {
        fn before_snapshot_read(&self, _: usize) -> io::Result<()> {
            if self.first.swap(false, Ordering::AcqRel) {
                self.started.send(()).unwrap();
                self.release.lock().unwrap().recv_timeout(Duration::from_secs(2)).unwrap();
            }
            Ok(())
        }
    }
    let (mut workspace, mut owner, mut host, _) = setup();
    let context = owner.prepare_context(&workspace, geometry(), BodyOwner::Text).unwrap();
    host.sync(workspace.editor(), context).unwrap();
    let budget = host.budget().clone();
    let (started, reading) = mpsc::sync_channel(1);
    let (release, blocked) = mpsc::sync_channel(1);
    let charge = budget.reserve(size_of::<ArcAllocation<GatedSource>>()).unwrap();
    let source: Arc<dyn WrapSource> = Arc::new(GatedSource {
        inner: SnapshotSource::new(context.key.source, workspace.editor().unwrap().document().snapshot()),
        first: AtomicBool::new(true),
        started,
        release: Mutex::new(blocked),
        _charge: charge,
    });
    accepted(host.drive(source));
    reading.recv_timeout(Duration::from_secs(2)).unwrap();
    let ticket = accepted(owner.stop(host));
    let failure = ticket.settle(Duration::ZERO).err().expect("blocked worker cannot acknowledge completion");
    assert!(matches!(failure.error, WrapError::TimedOut));
    assert!(!workspace.wrap_cleanup_ready());
    release.send(()).unwrap();
    let receipt = settle_until(*failure.ticket, Instant::now() + Duration::from_secs(2)).unwrap();
    accepted(workspace.acknowledge_wrap_settled(receipt));
    assert!(workspace.wrap_cleanup_ready());
    assert_eq!(
        budget.retained_bytes(),
        AllocationBudget::new(0).retained_bytes(),
        "all real source/header/display owners retired before the receipt"
    );
}
