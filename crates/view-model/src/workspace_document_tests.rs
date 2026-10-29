//! SOURCE ONLY. ROOT runs these real-worker controls in its isolated overlay.
use super::*;
use crate::{
    View,
    visual_rows::{BodyOwner, LayoutGeometry},
    workspace::workspace_wrap::WorkspaceWrap,
    wrap_host::WrapHost,
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver},
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};

const LIMIT: Duration = Duration::from_secs(2);

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "kwil-workspace-document-{}-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        Self(root)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("Document adapter fixture retained: {}", self.0.display());
            return;
        }
        // Only this test's create-new directory, never owner storage.
        assert!(self.0.is_absolute() && self.0.starts_with(std::env::temp_dir()));
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

fn workspace() -> Workspace {
    Workspace::new(View::new(None)).unwrap()
}
fn setup(
    workspace: &mut Workspace,
    budget: &AllocationBudget,
    root: Option<PathBuf>,
) -> (WorkspaceDocumentTransport, Receiver<()>) {
    let (wake, ready) = mpsc::sync_channel(8);
    let transport = workspace
        .start_bounded_document_worker_at(root, budget, move || match wake.try_send(()) {
            Ok(()) | Err(mpsc::TrySendError::Full(())) => Ok(()),
            Err(error @ mpsc::TrySendError::Disconnected(())) => Err(io::Error::other(error)),
        })
        .unwrap();
    (transport, ready)
}

// Test-only synchronous interleave: the ORIGINAL worker pauses in its real
// owner-free wake, outside transport locks. Both result and retirement-capacity
// wakes are acknowledged explicitly under ONE existing LIMIT deadline.
struct HeldLiveLane {
    transport: WorkspaceDocumentTransport,
    ready: Receiver<()>,
    resume: mpsc::SyncSender<()>,
    held: Arc<std::sync::atomic::AtomicBool>,
    deadline: Instant,
    last_result: Option<ResultInfo>,
}
impl HeldLiveLane {
    fn start(workspace: &mut Workspace, budget: &AllocationBudget, root: PathBuf) -> Self {
        let deadline = Instant::now() + LIMIT;
        let (wake, ready) = mpsc::sync_channel(8);
        let (resume, paused) = mpsc::sync_channel(1);
        let paused = std::sync::Mutex::new(paused);
        let held = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let hold = held.clone();
        let transport = workspace
            .start_bounded_document_worker_at(Some(root), budget, move || {
                match wake.try_send(()) {
                    Ok(()) | Err(mpsc::TrySendError::Full(())) => {}
                    Err(error @ mpsc::TrySendError::Disconnected(())) => return Err(io::Error::other(error)),
                }
                if hold.load(Ordering::Acquire) {
                    paused
                        .lock()
                        .unwrap()
                        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                        .map_err(io::Error::other)?;
                }
                Ok(())
            })
            .unwrap();
        Self { transport, ready, resume, held, deadline, last_result: None }
    }
    fn receive(&mut self, workspace: &mut Workspace, stage: &'static str) -> Option<ResultInfo> {
        self.ready
            .recv_timeout(self.deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|error| {
                let id = workspace.active_id();
                let session = workspace.hex_session().map(|session| {
                    (session.source(), session.input_epoch(), session.view().top(), session.view().cursor(), session.busy())
                });
                panic!(
                    "held-live stage={stage} original result/capacity wake under the shared deadline: {error:?}; last_result={:?} current_result={:?} transport={:?} active={id:?} document_busy={} editor_present={} observer_present={} seeded={} durable={:?} queue_len={} session(source,input_epoch,top,cursor,busy)={session:?}",
                    self.last_result,
                    self.transport.result_info(),
                    self.transport.status(),
                    workspace.document_busy(id),
                    workspace.editor().is_some(),
                    workspace.files.observed.contains_key(&id),
                    workspace.files.seeded.contains(&id),
                    workspace.files.durable.get(&id),
                    workspace.files.queue.len(),
                )
            });
        let info = self.transport.result_info();
        if info.is_some() {
            self.last_result = info;
        }
        let update = self.transport.receive(workspace, true).expect("actual sealed live acceptance");
        if let Some(update) = update {
            std::hint::black_box(update.damage());
        }
        info
    }
    fn resume(&self) {
        self.resume.try_send(()).expect("one acknowledgement of the held real wake");
    }
    fn until(
        &mut self,
        workspace: &mut Workspace,
        stage: &'static str,
        done: impl Fn(&Workspace, Option<ResultInfo>, &Self) -> bool,
    ) {
        for _ in 0..512 {
            let info = self.receive(workspace, stage);
            if done(workspace, info, self) {
                return; // Keep this exact wake held for the caller's assertions.
            }
            self.resume();
        }
        panic!(
            "held-live stage={stage} finite actual sealed result/capacity sequence; last_result={:?}",
            self.last_result
        );
    }
    fn flush_after_indexed(&mut self, workspace: &mut Workspace, id: DocumentId, stage: &'static str) {
        self.until(workspace, stage, |workspace, info, _| {
            info.is_some_and(|info| info.kind == ResultKind::Indexed && info.document == Some(id))
                && workspace.editor().is_some()
                && workspace.files.observed.contains_key(&id)
        });
        assert_eq!(workspace.active_id(), id);
        assert_eq!(
            workspace.editor().unwrap().document().hex_source(id.0),
            workspace.hex_session().unwrap().source().unwrap(),
            "the addressed original raw source is installed before flushing its seed"
        );
        assert!(workspace.files.seeded.contains(&id));
        // Initial seeding normally schedules its flush two seconds AFTER this
        // fixture's two-second absolute deadline has already started. Submit
        // ONE explicit flush behind the existing SeedJournal, preserving the
        // real durability receipt and the original final readiness predicate.
        workspace.files.queue.push_back(DocumentRequest::FlushJournals);
        workspace.pump_files();
        self.resume();
    }
    fn finish(self, workspace: Workspace) {
        self.held.store(false, Ordering::Release);
        self.resume();
        finish_saved_workspace_until(workspace, self.transport, self.deadline);
    }
}

#[test]
fn workspace_document_live_fence_retains_open_raw_until_real_capacity_and_empty_receive_cannot_pump() {
    let scratch = Scratch::new();
    let path = scratch.0.join("retained.bin");
    let original = b"physical\0bytes\x01\n";
    std::fs::write(&path, original).unwrap();
    let budget = AllocationBudget::new(crate::visual_rows::MAX_SESSION_BYTES);
    let mut workspace = workspace();
    let mut lane = HeldLiveLane::start(&mut workspace, &budget, scratch.0.join("recovery"));
    lane.until(&mut workspace, "retained-open/prepare", |workspace, info, _| {
        workspace.recovery_ready() && info.is_some_and(|info| info.kind == ResultKind::Prepared)
    });
    assert!(lane.transport.status().occupied, "Prepared residual is still owned by the held producer");
    let (id, _) = workspace.open_hex_path(path.clone(), 3).unwrap();
    let request_path = workspace
        .files
        .queue
        .iter()
        .find_map(|request| match request {
            DocumentRequest::OpenRaw { document, path, .. } if *document == id => Some(path),
            _ => None,
        })
        .expect("temporary Live Full must retain OpenRaw, not drop it as disconnected");
    let identity = request_path.as_os_str().as_encoded_bytes().as_ptr();
    assert_eq!(request_path, &path);
    assert!(workspace.document_busy(id));
    assert!(workspace.editor().is_none());
    assert!(!workspace.notice().contains("disconnected"));
    assert!(lane.transport.receive(&mut workspace, true).unwrap().is_none());
    assert!(workspace.files.queue.iter().any(|request| matches!(request,
        DocumentRequest::OpenRaw { document, path, .. }
            if *document == id && path.as_os_str().as_encoded_bytes().as_ptr() == identity)));
    lane.resume();
    assert!(
        lane.receive(&mut workspace, "retained-open/retirement-capacity").is_none(),
        "next real wake is successful producer retirement capacity"
    );
    assert!(
        !workspace.files.queue.iter().any(|request| matches!(request,
        DocumentRequest::OpenRaw { document, .. } if *document == id)),
        "capacity must admit the retained open"
    );

    // This new scalar marker has no capacity generation of its own. Repeated
    // empty receives must leave it in the ORIGINAL queue without pumping.
    workspace.files.queue.push_back(DocumentRequest::ReleaseReopen { document: id, generation: u64::MAX });
    let queued = workspace.files.queue.len();
    let charged = budget.retained_bytes();
    for _ in 0..2 {
        assert!(lane.transport.receive(&mut workspace, true).unwrap().is_none());
        assert_eq!(workspace.files.queue.len(), queued);
        assert!(matches!(
            workspace.files.queue.back(),
            Some(DocumentRequest::ReleaseReopen { generation: u64::MAX, .. })
        ));
        assert_eq!(budget.retained_bytes(), charged);
    }
    assert!(matches!(
        workspace.files.queue.pop_back(),
        Some(DocumentRequest::ReleaseReopen { generation: u64::MAX, .. })
    ));
    lane.resume();
    lane.flush_after_indexed(&mut workspace, id, "retained-open/addressed-index-and-seed");
    lane.until(&mut workspace, "retained-open/page-and-explicit-durability", |workspace, _, lane| {
        !lane.transport.status().occupied
            && !workspace.document_busy(id)
            && workspace.recovery_notice().is_none()
            && workspace.hex_session().is_some_and(|session| session.row(workspace.hex_source(), 0).is_some())
    });
    let mut bytes = Vec::new();
    workspace.editor().unwrap().document().snapshot().write_to(&mut bytes).unwrap();
    assert_eq!(bytes, original);
    assert_eq!(std::fs::read(path).unwrap(), original);
    assert!(budget.peak_bytes() <= crate::visual_rows::MAX_SESSION_BYTES);
    lane.finish(workspace);
}

#[test]
fn workspace_document_sealed_stale_hex_page_queues_latest_without_cursor_selection_or_source_rewind() {
    use crate::hex_session::Input;
    let scratch = Scratch::new();
    let path = scratch.0.join("latest-page.bin");
    let mut original = vec![b'x'; 1024];
    original[2] = 0;
    std::fs::write(&path, &original).unwrap();
    let budget = AllocationBudget::new(crate::visual_rows::MAX_SESSION_BYTES);
    let mut workspace = workspace();
    let mut lane = HeldLiveLane::start(&mut workspace, &budget, scratch.0.join("recovery"));
    lane.until(&mut workspace, "latest-page/prepare-retirement", |workspace, _, lane| {
        workspace.recovery_ready() && !lane.transport.status().occupied
    });
    let (id, _) = workspace.open_hex_path(path.clone(), 3).unwrap();
    lane.resume();
    lane.flush_after_indexed(&mut workspace, id, "latest-page/addressed-index-and-seed");
    lane.until(&mut workspace, "latest-page/open-page-and-explicit-durability", |workspace, _, lane| {
        !lane.transport.status().occupied
            && !workspace.document_busy(id)
            && workspace.recovery_notice().is_none()
            && workspace.hex_session().is_some_and(|session| session.row(workspace.hex_source(), 0).is_some())
    });
    let source = workspace.hex_source().unwrap();
    for input in
        [Input::Move { bytes: 33, extend: false }, Input::Scroll(1), Input::End { document: true, extend: true }]
    {
        let epoch = workspace.hex_session().unwrap().input_epoch();
        workspace.hex_input(source, epoch, input).unwrap();
    }
    assert!(workspace.document_busy(id));
    let latest = workspace.hex_session().unwrap().page_request().unwrap();
    assert_eq!(workspace.hex_session().unwrap().view().cursor(), 1024);
    assert_eq!(workspace.hex_session().unwrap().view().selection(), 33..1024);
    lane.resume();
    lane.until(&mut workspace, "latest-page/stale-completion", |_, info, _| {
        info.is_some_and(|info| info.kind == ResultKind::HexPage)
    });
    assert_eq!(workspace.hex_source(), Some(source));
    let session = workspace.hex_session().unwrap();
    assert_eq!(session.page_request(), Some(latest));
    assert_eq!(session.view().cursor(), 1024, "stale completion cannot rewind scalar navigation");
    assert_eq!(session.view().selection(), 33..1024);
    assert!(workspace.document_busy(id), "sealed FollowUp must submit the latest page after restoring the old loan");
    assert!(workspace.editor().is_none(), "only the original Editor is lent to the latest page");
    lane.resume();
    lane.until(&mut workspace, "latest-page/latest-completion-and-retirement", |workspace, _, lane| {
        !lane.transport.status().occupied
            && !workspace.document_busy(id)
            && workspace.recovery_notice().is_none()
            && workspace.hex_session().is_some_and(|session| session.rows(workspace.hex_source()).len() == latest.rows)
    });
    let session = workspace.hex_session().unwrap();
    assert_eq!(workspace.hex_source(), Some(source));
    assert_eq!(session.view().cursor(), 1024);
    assert_eq!(session.view().selection(), 33..1024);
    assert_eq!(session.row(Some(source), 0).unwrap().offset, latest.top * 16);
    let mut bytes = Vec::new();
    workspace.editor().unwrap().document().snapshot().write_to(&mut bytes).unwrap();
    assert_eq!(bytes, original);
    assert_eq!(std::fs::read(path).unwrap(), original);
    assert!(budget.peak_bytes() <= crate::visual_rows::MAX_SESSION_BYTES);
    lane.finish(workspace);
}

fn receive(transport: &mut WorkspaceDocumentTransport, ready: &Receiver<()>) -> DocumentResult {
    // An owner-free wake is emitted AFTER the sole slot has been filled.
    ready.recv_timeout(LIMIT).expect("bounded document wake");
    transport.try_recv(true).unwrap().expect("wake must address the existing result slot")
}
fn accept(workspace: &mut Workspace, result: DocumentResult) -> ControlFlow<DocumentResult, ()> {
    workspace.document_result(result).map_continue(|_| ())
}

// TEST ONLY: the frozen worker_done notification can precede actual thread
// exit. Retry ONLY that exact retained ticket under ONE shared deadline, with
// a bounded readiness backoff. Production settle remains a SINGLE call; every
// other failure fails this control. No source/payload queue or gate is changed.
fn finish(ticket: WorkspaceDocumentSettlementTicket, workspace: &mut Workspace) {
    finish_until(ticket, workspace, Instant::now() + LIMIT);
}

fn finish_until(ticket: WorkspaceDocumentSettlementTicket, workspace: &mut Workspace, deadline: Instant) {
    std::thread::scope(|scope| {
        scope
            .spawn(move || {
                crate::cleanup_test_support::cleanup_until(
                    ticket,
                    deadline,
                    |ticket, deadline| {
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        assert!(!remaining.is_zero(), "actual document worker exit exceeded the shared deadline");
                        ticket.settle(remaining, |result| accept(workspace, result)).map_or_else(
                            |failure| ControlFlow::Break((failure.ticket, failure.reason)),
                            ControlFlow::Continue,
                        )
                    },
                    |reason| matches!(reason, DocumentResultSettlementReason::ThreadExitPending),
                )
                .unwrap();
            })
            .join()
            .unwrap();
    });
}

// These two shutdown controls assert ordering and retained absolute time, not
// filesystem or hosted-runner throughput. Keep a logical epoch already behind
// wall time so accidentally using the real clock fails deterministically.
fn shutdown_contract_time() -> Instant {
    Instant::now() - LIMIT * 2
}

fn settle_shutdown_contract(
    bundle: &mut WorkspaceDocumentShutdown,
    requested_deadline: Instant,
    now: Instant,
) -> Result<DocumentShutdownStatus, DocumentShutdownReason> {
    let deadline = bundle.deadline().map_or(requested_deadline, |old| old.min(requested_deadline));
    bundle.settle_until_with(
        requested_deadline,
        || now,
        |actual| {
            assert_eq!(actual, deadline, "every transport effect receives the retained absolute deadline");
            // Bound each real fixture IO wait with the existing LIMIT. Logical
            // time advances only when the control explicitly changes `now`.
            let remaining = actual.saturating_duration_since(now);
            assert!(remaining <= LIMIT);
            Instant::now() + remaining
        },
    )
}

fn finish_shutdown_contract_fixture(ticket: WorkspaceDocumentSettlementTicket, workspace: &mut Workspace) {
    // All semantic assertions precede teardown. One real settlement attempt,
    // with the existing fixture limit; no retry or logical-deadline renewal.
    ticket.settle(LIMIT, |result| accept(workspace, result)).unwrap();
}

fn assert_shutdown_contract_expired(bundle: &mut WorkspaceDocumentShutdown, deadline: Instant) {
    let phase = bundle.phase;
    assert_eq!(
        settle_shutdown_contract(bundle, deadline + LIMIT, deadline),
        Err(DocumentShutdownReason::Transport(DocumentResultSettlementReason::TimedOut)),
        "a later requested deadline cannot extend the first deadline"
    );
    assert_eq!(bundle.deadline(), Some(deadline));
    assert_eq!(bundle.phase, phase, "expiry cannot advance source release or native exit");
    assert!(!bundle.status().closed);
}

fn finish_saved_workspace_until(workspace: Workspace, transport: WorkspaceDocumentTransport, deadline: Instant) {
    let mut bundle = match workspace.into_document_shutdown(transport) {
        ControlFlow::Continue(bundle) => bundle,
        ControlFlow::Break(_) => panic!("original saved Workspace and worker must bind for shutdown"),
    };
    bundle.bind_search_sources(DocumentSearchBinding::NoSearchWorker).unwrap();
    // The save/reset and all teardown stages share the original operation's
    // absolute deadline; binding cannot start another timeout window.
    bundle.deadline = Some(deadline);
    std::thread::scope(|scope| {
        let bundle = &mut bundle;
        scope
            .spawn(move || {
                let remaining = deadline.saturating_duration_since(Instant::now());
                assert!(!remaining.is_zero(), "saved fixture shutdown exceeded the original absolute deadline");
                assert_eq!(bundle.settle(remaining).unwrap(), DocumentShutdownStatus::SourcesReleased);
                assert_eq!(bundle.deadline(), Some(deadline));
                assert_eq!(bundle.phase, ShutdownPhase::SearchRelease);
                assert!(bundle.barrier.is_some());
                assert!(!bundle.status().closed, "source roots must release before actual sender closure");
                let workspace = bundle.workspace.as_ref().unwrap();
                assert!(workspace.views.is_empty());
                assert!(workspace.closed_views.is_empty());
                assert!(workspace.files.deferred_recovery.is_none());
                assert!(workspace.binary.retiring.is_empty());
                assert!(workspace.files.saves.is_empty());
                assert!(workspace.files.queue.is_empty());
                assert!(workspace.files.worker.is_some());
                eprintln!(
                    "saved fixture shutdown: SourcesReleased, views/history/deferred roots empty, exact durability and source barrier accepted, original deadline retained, sender still open"
                );
                // Only after sealed source release may this test-only terminal
                // adapter physically close the sender and require Stopped(Ok)
                // plus actual exit through the existing strict helper.
                let mut workspace = bundle.workspace.take().unwrap();
                let worker = workspace.files.worker.take().unwrap();
                let ticket = bundle.ticket.take().unwrap().into_legacy_for_test(worker);
                let charge = bundle.charge.0.take();
                let live_charge = workspace.files.live_document_metadata.take();
                finish_until(
                    WorkspaceDocumentSettlementTicket { ticket, charge: AdapterCharge(charge), live_charge },
                    &mut workspace,
                    deadline,
                );
                assert_eq!(bundle.deadline(), Some(deadline));
                bundle.phase = ShutdownPhase::Complete;
            })
            .join()
            .unwrap();
    });
}

#[test]
fn workspace_document_preadmission_precedes_spawn_callback_and_preserves_root() {
    let mut workspace = workspace();
    let budget = AllocationBudget::new(0);
    let before = budget.retained_bytes();
    let root = PathBuf::from("explicit-root-never-opened");
    let identity = root.as_os_str().as_encoded_bytes().as_ptr();
    let result = workspace.start_bounded_document_worker_with(
        Some(root),
        &budget,
        || panic!("no wake before admission"),
        |_, _| panic!("no allocation/thread constructor before adapter admission"),
        |_, _, _| panic!("no Prepare before admission"),
    );
    match result {
        Err(WorkspaceDocumentStartFailure::NotStarted {
            root: Some(root),
            error: WorkspaceDocumentStartError::Admission(_),
        }) => assert_eq!(root.as_os_str().as_encoded_bytes().as_ptr(), identity),
        _ => panic!("expected typed pre-start admission refusal"),
    }
    assert!(workspace.files.worker.is_none());
    assert!(!workspace.files.preparing);
    assert_eq!(budget.retained_bytes(), before);
}

#[test]
fn workspace_document_worker_transport_admission_also_uses_the_same_aggregate() {
    let mut workspace = workspace();
    let measured = AllocationBudget::default();
    let baseline = measured.retained_bytes();
    let (charge, _) = admit_start(&workspace, None, &measured).unwrap();
    let adapter_bytes = measured.retained_bytes() - baseline;
    drop(charge);
    let budget = AllocationBudget::new(baseline + adapter_bytes);
    let called = Arc::new(AtomicUsize::new(0));
    let wake = called.clone();
    assert!(matches!(
        workspace.start_bounded_document_worker_at(None, &budget, move || {
            wake.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }),
        Err(WorkspaceDocumentStartFailure::NotStarted { root: None, error: WorkspaceDocumentStartError::Spawn(_) })
    ));
    assert_eq!(called.load(Ordering::SeqCst), 0);
    assert!(workspace.files.worker.is_none());
    assert_eq!(budget.retained_bytes(), baseline);
}

#[test]
fn workspace_document_spawn_error_is_typed_and_releases_all_preallocated_metadata() {
    let mut workspace = workspace();
    let budget = AllocationBudget::default();
    let before = budget.retained_bytes();
    let root = PathBuf::from("retained-native-root");
    let identity = root.as_os_str().as_encoded_bytes().as_ptr();
    let called = std::cell::Cell::new(false);
    let result = workspace.start_bounded_document_worker_with(
        Some(root),
        &budget,
        || panic!("spawn refused"),
        |_, _| {
            called.set(true);
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "forced spawn refusal"))
        },
        |_, _, _| panic!("no Prepare after spawn refusal"),
    );
    match result {
        Err(WorkspaceDocumentStartFailure::NotStarted {
            root: Some(root),
            error: WorkspaceDocumentStartError::Spawn(error),
        }) => {
            assert_eq!(root.as_os_str().as_encoded_bytes().as_ptr(), identity);
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        }
        _ => panic!("spawn error lost its exact metadata"),
    }
    assert!(called.get());
    assert!(workspace.files.worker.is_none());
    assert_eq!(budget.retained_bytes(), before);
}

#[test]
fn workspace_document_initial_destination_copies_are_admitted_before_spawn() {
    let mut workspace = workspace();
    let id = workspace.active_id();
    let path = std::env::temp_dir().join("kwil-startup-reserved-destination");
    workspace.session.set_path(id, path.clone()).unwrap();
    let budget = AllocationBudget::default();
    let before = budget.retained_bytes();
    let (charge, destinations) = admit_start(&workspace, None, &budget).unwrap();
    assert_eq!(destinations, [(id, path)]);
    assert_eq!(destinations.capacity(), 1);
    assert!(budget.retained_bytes() >= before + size_of::<(DocumentId, PathBuf)>());
    drop(destinations);
    drop(charge);
    assert_eq!(budget.retained_bytes(), before);
}

#[test]
fn workspace_document_real_prepare_uses_explicit_root_and_workspace_receipt() {
    let root = Scratch::new();
    let mut workspace = workspace();
    let budget = AllocationBudget::default();
    let before = budget.retained_bytes();
    let (mut transport, ready) = setup(&mut workspace, &budget, Some(root.0.clone()));
    assert!(workspace.files.worker.is_some());
    assert!(workspace.files.preparing);
    assert!(matches!(transport.try_recv(false), Err(DocumentResultReceiveError::AdmissionRequired)));
    let result = receive(&mut transport, &ready);
    assert!(matches!(&result, DocumentResult::Prepared(Ok(_))));
    assert!(root.0.join("workspace.lock").is_file());
    assert!(matches!(accept(&mut workspace, result), ControlFlow::Continue(())));
    assert!(!workspace.files.preparing);
    assert!(workspace.recovery_ready());
    let charged = budget.retained_bytes();
    assert!(charged > before);
    let ticket = workspace.stop_bounded_document_worker(transport).unwrap();
    assert!(workspace.files.worker.is_none());
    assert_eq!(budget.retained_bytes(), charged, "extraction is not successful settlement");
    finish(ticket, &mut workspace);
    // Successful Prepare installs the existing editor observers. Their closed
    // sender handles still retain the original transport aggregate until the
    // original Workspace is retired OFF UI after successful settlement.
    drop(workspace);
    assert_eq!(budget.retained_bytes(), before);
}

#[test]
fn workspace_document_existing_worker_refusal_preserves_worker_preparing_and_root() {
    let mut workspace = workspace();
    let budget = AllocationBudget::default();
    let (transport, _ready) = setup(&mut workspace, &budget, Some(PathBuf::new()));
    let identity = workspace.files.worker.as_ref().unwrap() as *const DocumentWorker;
    let charged = budget.retained_bytes();
    let root = PathBuf::from("second-root-never-opened");
    let root_identity = root.as_os_str().as_encoded_bytes().as_ptr();
    let result = workspace.start_bounded_document_worker_with(
        Some(root),
        &budget,
        || panic!("existing worker must not wake through a new callback"),
        |_, _| panic!("existing worker must not spawn"),
        |_, _, _| panic!("existing worker must not submit"),
    );
    match result {
        Err(WorkspaceDocumentStartFailure::NotStarted {
            root: Some(root),
            error: WorkspaceDocumentStartError::WorkerAlreadyPresent,
        }) => assert_eq!(root.as_os_str().as_encoded_bytes().as_ptr(), root_identity),
        _ => panic!("expected existing-worker refusal"),
    }
    assert_eq!(workspace.files.worker.as_ref().unwrap() as *const DocumentWorker, identity);
    assert!(workspace.files.preparing);
    assert_eq!(budget.retained_bytes(), charged);
    let ticket = workspace.stop_bounded_document_worker(transport).unwrap();
    finish(ticket, &mut workspace);
}

#[test]
fn workspace_document_forced_prepare_refusal_returns_exact_request_receiver_and_worker() {
    let mut workspace = workspace();
    let budget = AllocationBudget::default();
    let before = budget.retained_bytes();
    let root = PathBuf::from("exact-unsubmitted-root");
    let root_identity = root.as_os_str().as_encoded_bytes().as_ptr();
    let worker_identity = std::cell::Cell::new(std::ptr::null::<DocumentWorker>());
    let request_identity = std::cell::Cell::new(std::ptr::null::<DocumentRequest>());
    let result = workspace.start_bounded_document_worker_with(
        Some(root),
        &budget,
        || panic!("closed before Prepare"),
        DocumentWorker::spawn_bounded,
        |worker, receiver, request| {
            worker_identity.set(worker as *const DocumentWorker);
            receiver.close();
            let error = worker.try_submit(request).unwrap_err();
            let TrySendError::Disconnected(request) = &error else { panic!("expected closed gate") };
            request_identity.set(&**request as *const DocumentRequest);
            Err(error)
        },
    );
    let Err(WorkspaceDocumentStartFailure::PrepareRefused(refusal)) = result else {
        panic!("started refusal must not become NotStarted/io::Error")
    };
    assert_eq!(workspace.files.worker.as_ref().unwrap() as *const DocumentWorker, worker_identity.get());
    assert!(!workspace.files.preparing);
    assert!(refusal.transport.status().closed);
    let TrySendError::Disconnected(request) = &refusal.error else { panic!("lost refusal class") };
    assert_eq!(&**request as *const DocumentRequest, request_identity.get());
    let DocumentRequest::Prepare { root: Some(root) } = &**request else { panic!("lost root request") };
    assert_eq!(root.as_os_str().as_encoded_bytes().as_ptr(), root_identity);
    assert_eq!(root, &PathBuf::from("exact-unsubmitted-root"));
    let WorkspaceDocumentPrepareRefusal { transport, error } = refusal;
    drop(error);
    let ticket = workspace.stop_bounded_document_worker(transport).unwrap();
    assert!(budget.retained_bytes() > before);
    finish(ticket, &mut workspace);
    assert_eq!(budget.retained_bytes(), before);
}

#[test]
fn workspace_document_foreign_and_missing_extraction_restore_both_original_owners() {
    let mut first = workspace();
    let mut second = workspace();
    let budget = AllocationBudget::default();
    let (mut transport, first_ready) = setup(&mut first, &budget, Some(PathBuf::new()));
    let (mut other, second_ready) = setup(&mut second, &budget, Some(PathBuf::new()));
    // Consume the two deterministic invalid-root receipts so wake cannot race
    // the foreign-stop state assertions. Neither touches persistent storage.
    assert!(matches!(accept(&mut first, receive(&mut transport, &first_ready)), ControlFlow::Continue(())));
    assert!(matches!(accept(&mut second, receive(&mut other, &second_ready)), ControlFlow::Continue(())));
    let identity = second.files.worker.as_ref().unwrap() as *const DocumentWorker;
    let charged = budget.retained_bytes();
    let refusal = second.stop_bounded_document_worker(transport).err().expect("foreign worker must refuse");
    assert_eq!(refusal.reason, WorkspaceDocumentStopReason::ForeignWorker);
    assert_eq!(second.files.worker.as_ref().unwrap() as *const DocumentWorker, identity);
    assert!(!refusal.transport.status().closed);
    assert_eq!(budget.retained_bytes(), charged);
    let worker = first.files.worker.take().unwrap();
    let refusal = first.stop_bounded_document_worker(refusal.transport).err().expect("missing worker must refuse");
    assert_eq!(refusal.reason, WorkspaceDocumentStopReason::MissingWorker);
    assert!(!refusal.transport.status().closed);
    first.files.worker = Some(worker);
    let ticket = first.stop_bounded_document_worker(refusal.transport).unwrap();
    let other = second.stop_bounded_document_worker(other).unwrap();
    finish(ticket, &mut first);
    finish(other, &mut second);
}

#[test]
fn workspace_document_shared_wrap_aggregate_and_charge_survive_extraction_and_failure() {
    let mut workspace = workspace();
    assert!(!workspace.editor().unwrap().settings.editor.word_wrap);
    let budget = AllocationBudget::default();
    let baseline = budget.retained_bytes();
    let (wrap_wake, wrap_ready) = mpsc::sync_channel(4);
    let mut host = WrapHost::with_budget(budget.clone(), move || {
        let _ = wrap_wake.try_send(());
    })
    .unwrap();
    let mut owner = WorkspaceWrap::new(&mut workspace, &host).unwrap();
    // Host construction starts the real worker before its engine scratch is
    // admitted. One actual empty-source Fill proves initialization: no source
    // bytes means no progress wake, so its sole wake follows READY publication.
    // Leave that exact owner UNTAKEN in the worker's existing ready cell; its
    // source/page and the host's display stay charged until final off-UI stop.
    let warm_deadline = Instant::now() + LIMIT;
    let geometry = LayoutGeometry { columns: 1, rows: 1, tab_size: 4, ..Default::default() };
    workspace.editor_mut().unwrap().settings.editor.word_wrap = true;
    let context = owner.prepare_context(&workspace, geometry, BodyOwner::Text).unwrap();
    assert!(host.sync(workspace.editor(), context).unwrap().needs_source);
    owner.lend_source(&workspace, context, &mut host).unwrap();
    wrap_ready.recv_timeout(warm_deadline.saturating_duration_since(Instant::now())).unwrap();
    // Restore only Workspace's setting, without sync/cancel/drain starting
    // asynchronous retirement. This intentionally retains OLD readiness
    // ownership; it does not claim a disabled host published an empty page.
    workspace.editor_mut().unwrap().settings.editor.word_wrap = false;
    assert!(!workspace.editor().unwrap().settings.editor.word_wrap);
    let wrap_bytes = budget.retained_bytes();
    let (transport, ready) = setup(&mut workspace, &budget, Some(PathBuf::new()));
    ready.recv_timeout(LIMIT).unwrap();
    let charged = budget.retained_bytes();
    assert!(charged > wrap_bytes);
    assert_eq!(host.budget().retained_bytes(), charged);
    let ticket = workspace.stop_bounded_document_worker(transport).unwrap();
    assert_eq!(budget.retained_bytes(), charged);
    let failure = ticket.settle(Duration::ZERO, |_| panic!("zero timeout never consumes")).unwrap_err();
    assert_eq!(failure.reason, DocumentResultSettlementReason::TimedOut);
    assert_eq!(budget.retained_bytes(), charged, "failed settle must retain adapter AND worker charges");
    assert!(matches!(failure.ticket.inspect().pending(), Some(DocumentResult::Prepared(Err(_)))));
    let failure = failure.ticket.settle(LIMIT, ControlFlow::Break).unwrap_err();
    assert_eq!(failure.reason, DocumentResultSettlementReason::ConsumerRefused);
    assert_eq!(budget.retained_bytes(), charged);
    finish(failure.ticket, &mut workspace);
    assert_eq!(budget.retained_bytes(), wrap_bytes);
    let wrap_ticket = match owner.stop(host) {
        ControlFlow::Continue(ticket) => ticket,
        ControlFlow::Break(_) => panic!("original host must be accepted"),
    };
    let receipt = crate::workspace::workspace_wrap::settle_until_for_test(wrap_ticket, Instant::now() + LIMIT).unwrap();
    assert!(matches!(workspace.acknowledge_wrap_settled(receipt), ControlFlow::Continue(())));
    assert_eq!(budget.retained_bytes(), baseline);
}

#[test]
fn workspace_document_close_drains_an_accepted_editor_loan_through_the_original_workspace() {
    use kwil_core_search::worker::{ReplaceRecipe, ReplacementEdits, SearchKey};
    let mut workspace = workspace();
    workspace.editor_mut().unwrap().insert_text("exact loan", Duration::ZERO).unwrap();
    let budget = AllocationBudget::default();
    let baseline = budget.retained_bytes();
    let (mut transport, ready) = setup(&mut workspace, &budget, Some(PathBuf::new()));
    assert!(matches!(accept(&mut workspace, receive(&mut transport, &ready)), ControlFlow::Continue(())));
    let id = workspace.active_id();
    let key =
        SearchKey { document_id: id.0, revision: workspace.editor().unwrap().document().revision(), generation: 17 };
    let operation = 41;
    let editor = Box::new(workspace.views.get_mut(&id).unwrap().editor.take().unwrap());
    let identity = &*editor as *const crate::Editor as usize;
    workspace.files.loans.insert(id, super::super::EditorLoan { key, operation, formatting: false, file_action: None });
    workspace
        .files
        .worker
        .as_ref()
        .unwrap()
        .try_submit(DocumentRequest::Replace {
            document: id,
            operation,
            editor,
            recipe: ReplaceRecipe { key, edits: ReplacementEdits::Inline(Arc::new(Vec::new())) },
        })
        .unwrap();
    transport.close();
    assert!(matches!(transport.try_recv(true), Err(DocumentResultReceiveError::Closed)));
    let ticket = workspace.stop_bounded_document_worker(transport).unwrap();
    let charged = budget.retained_bytes();
    let failure = ticket.settle(LIMIT, ControlFlow::Break).unwrap_err();
    assert_eq!(failure.reason, DocumentResultSettlementReason::ConsumerRefused);
    {
        let inspect = failure.ticket.inspect();
        let Some(DocumentResult::Replaced { editor, key: actual, operation: actual_operation, .. }) = inspect.pending()
        else {
            panic!("close lost an accepted loan")
        };
        assert_eq!(&**editor as *const crate::Editor as usize, identity);
        assert_eq!(*actual, key);
        assert_eq!(*actual_operation, operation);
    }
    assert!(workspace.editor().is_none());
    assert!(workspace.document_busy(id));
    assert_eq!(budget.retained_bytes(), charged);
    finish(failure.ticket, &mut workspace);
    assert!(!workspace.document_busy(id));
    let mut bytes = Vec::new();
    workspace.editor().unwrap().document().snapshot().write_to(&mut bytes).unwrap();
    assert_eq!(bytes, b"exact loan");
    assert_eq!(budget.retained_bytes(), baseline);
}

#[test]
fn workspace_document_wrapper_controls_keep_charge_and_cannot_invent_retry() {
    let mut workspace = workspace();
    let budget = AllocationBudget::default();
    let (mut transport, ready) = setup(&mut workspace, &budget, Some(PathBuf::new()));
    let charged = budget.retained_bytes();
    ready.recv_timeout(LIMIT).unwrap();
    assert!(transport.status().occupied);
    assert!(matches!(transport.request_wrap_retry(LIMIT), Err(DocumentResultSettlementReason::RetryUnavailable)));
    assert_eq!(budget.retained_bytes(), charged);
    let result = transport.try_recv(true).unwrap().unwrap();
    assert!(matches!(accept(&mut workspace, result), ControlFlow::Continue(())));
    let ticket = workspace.stop_bounded_document_worker(transport).unwrap();
    assert_eq!(budget.retained_bytes(), charged);
    finish(ticket, &mut workspace);
}

#[test]
fn workspace_document_wake_failure_stays_sticky_with_exact_ticket_charge_and_error_until_ack() {
    #[derive(Debug)]
    struct OriginalWakeError(u64);
    impl fmt::Display for OriginalWakeError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "original wake {}", self.0)
        }
    }
    impl std::error::Error for OriginalWakeError {}
    let mut workspace = workspace();
    let budget = AllocationBudget::default();
    let baseline = budget.retained_bytes();
    let (entered, wake) = mpsc::sync_channel(1);
    let transport = workspace
        .start_bounded_document_worker_at(Some(PathBuf::new()), &budget, move || {
            entered.try_send(()).unwrap();
            Err(io::Error::other(OriginalWakeError(71)))
        })
        .unwrap();
    // Prove the actual worker entered its fallible callback before close; the
    // original settle wait observes the eventual failure without status polling.
    wake.recv_timeout(LIMIT).unwrap();
    let charged = budget.retained_bytes();
    let ticket = workspace.stop_bounded_document_worker(transport).unwrap();
    let failure = ticket.settle(LIMIT, |result| accept(&mut workspace, result)).unwrap_err();
    assert_eq!(failure.reason, DocumentResultSettlementReason::WakeFailed);
    assert_eq!(budget.retained_bytes(), charged);
    {
        let inspect = failure.ticket.inspect();
        let Some(DocumentDeliveryFailure::Wake(error)) = inspect.delivery_failure() else {
            panic!("adapter lost original typed wake error")
        };
        assert_eq!(error.get_ref().unwrap().downcast_ref::<OriginalWakeError>().unwrap().0, 71);
    }
    let again = failure.ticket.settle(LIMIT, |_| panic!("sticky failure cannot consume")).unwrap_err();
    assert_eq!(again.reason, DocumentResultSettlementReason::WakeFailed);
    assert_eq!(budget.retained_bytes(), charged);
    let mut ticket = again.ticket;
    let Some(DocumentDeliveryFailure::Wake(error)) = ticket.acknowledge_delivery_failure() else {
        panic!("ACK must transfer original failure")
    };
    assert_eq!(error.get_ref().unwrap().downcast_ref::<OriginalWakeError>().unwrap().0, 71);
    assert!(ticket.acknowledge_delivery_failure().is_none());
    assert_eq!(budget.retained_bytes(), charged, "failure ACK alone cannot release adapter charge");
    finish(ticket, &mut workspace);
    assert_eq!(budget.retained_bytes(), baseline);
}

#[test]
fn workspace_document_bound_foreign_worker_refusal_returns_both_original_workspaces_and_transports() {
    let mut first = workspace();
    let mut second = workspace();
    let budget = AllocationBudget::default();
    let (first_transport, first_ready) = setup(&mut first, &budget, Some(PathBuf::new()));
    let (second_transport, second_ready) = setup(&mut second, &budget, Some(PathBuf::new()));
    first_ready.recv_timeout(LIMIT).unwrap();
    second_ready.recv_timeout(LIMIT).unwrap();
    let original_id = second.active_id();
    let incarnation = second.views[&original_id].input_incarnation.clone();
    let refusal = match second.into_document_shutdown(first_transport) {
        ControlFlow::Break(refusal) => refusal,
        ControlFlow::Continue(_) => panic!("foreign bind accepted"),
    };
    assert!(matches!(refusal.reason, DocumentShutdownBindReason::ForeignWorker));
    assert!(Arc::ptr_eq(&incarnation, &refusal.workspace.views[&original_id].input_incarnation));
    assert!(!refusal.transport.status().closed);
    assert!(refusal.transport.status().occupied);
    let mut second = refusal.workspace;
    let mut first_transport = refusal.transport;
    let mut second_transport = second_transport;
    let first_result = first_transport.try_recv(true).unwrap().unwrap();
    let second_result = second_transport.try_recv(true).unwrap().unwrap();
    assert!(matches!(accept(&mut first, first_result), ControlFlow::Continue(())));
    assert!(matches!(accept(&mut second, second_result), ControlFlow::Continue(())));
    let first_ticket = first.stop_bounded_document_worker(first_transport).unwrap();
    let second_ticket = second.stop_bounded_document_worker(second_transport).unwrap();
    finish(first_ticket, &mut first);
    finish(second_ticket, &mut second);
}

#[test]
fn workspace_document_real_worker_index_partial_panics_keep_exact_source_and_never_replay() {
    use super::commit::{CommitFault, DocumentCommit, DocumentCommitPhase};
    use std::panic::{AssertUnwindSafe, catch_unwind};
    for fault in [CommitFault::BeforePublication, CommitFault::AfterInstallation] {
        let scratch = Scratch::new();
        let path = scratch.0.join("tiny.txt");
        std::fs::write(&path, b"tiny\n").unwrap();
        let mut workspace = workspace();
        let budget = AllocationBudget::default();
        let (mut transport, ready) = setup(&mut workspace, &budget, Some(scratch.0.join("recovery")));
        let prepared = receive(&mut transport, &ready);
        assert!(matches!(accept(&mut workspace, prepared), ControlFlow::Continue(())));
        let id = workspace.active_id();
        workspace.views.get_mut(&id).unwrap().editor = None;
        let file = kwil_core_text::TextFile::open(&path).unwrap();
        workspace
            .files
            .worker
            .as_ref()
            .unwrap()
            .try_submit(DocumentRequest::Index { document: id, revision: 0, path, file })
            .unwrap();
        let result = (0..16)
            .find_map(|_| {
                let result = receive(&mut transport, &ready);
                if matches!(result, DocumentResult::Indexed { .. }) {
                    Some(result)
                } else {
                    assert!(matches!(accept(&mut workspace, result), ControlFlow::Continue(())));
                    None
                }
            })
            .expect("bounded actual worker completion sequence must return Indexed");
        let (pointer, epoch) = match &result {
            DocumentResult::Indexed { result: Ok(document), .. } => {
                (document.as_ref() as *const crate::Document as usize, document.source_epoch)
            }
            _ => panic!("expected tiny real-worker Indexed"),
        };
        let mut commit = DocumentCommit::new(result);
        commit.set_fault(fault);
        assert!(catch_unwind(AssertUnwindSafe(|| workspace.commit_document(&mut commit, &budget, None))).is_err());
        if fault == CommitFault::BeforePublication {
            assert_eq!(commit.phase(), DocumentCommitPhase::Prepared);
            assert!(workspace.views[&id].editor.is_none());
            assert!(
                matches!(commit.pending(),Some(DocumentResult::Indexed {result:Ok(document),..}) if document.as_ref() as *const crate::Document as usize==pointer&&document.source_epoch==epoch)
            );
        } else {
            assert_eq!(commit.phase(), DocumentCommitPhase::Installed);
            assert_eq!(workspace.views[&id].editor().unwrap().document().source_epoch, epoch);
        }
        assert!(matches!(
            workspace.commit_document(&mut commit, &budget, None),
            Err(WorkspaceDocumentAcceptError::PartialCommit)
        ));
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    workspace.views.clear();
                    drop(commit);
                })
                .join()
                .unwrap()
        });
        let ticket = workspace.stop_bounded_document_worker(transport).unwrap();
        finish(ticket, &mut workspace);
    }
}

#[test]
fn workspace_document_real_worker_barrier_releases_backing_pins_before_sender_close() {
    let scratch = Scratch::new();
    let path = scratch.0.join("tiny-backed.txt");
    std::fs::write(&path, b"pin\n").unwrap();
    let file = kwil_core_text::TextFile::open(&path).unwrap();
    let document = crate::Document::from_file(path.clone(), file).unwrap();
    let mut view = View::new(None);
    view.path = Some(path);
    view.file = document.original.clone();
    view.editor = Some(crate::Editor::new(document));
    let mut workspace = Workspace::new(view).unwrap();
    let budget = AllocationBudget::default();
    let (transport, _ready) = setup(&mut workspace, &budget, Some(scratch.0.join("recovery")));
    let mut bundle = match workspace.into_document_shutdown(transport) {
        ControlFlow::Continue(bundle) => bundle,
        ControlFlow::Break(_) => panic!("original worker bind refused"),
    };
    assert_eq!(bundle.settle(LIMIT), Err(DocumentShutdownReason::SearchBindingRequired));
    assert!(bundle.deadline.is_none(), "missing binding cannot start a different timeout window");
    bundle.bind_search_sources(DocumentSearchBinding::NoSearchWorker).unwrap();
    // Scope the entire settlement/release on an explicit OFF-UI executor.
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                assert_eq!(bundle.settle(Duration::from_secs(5)).unwrap(), DocumentShutdownStatus::SourcesReleased);
                let deadline = bundle.deadline;
                let workspace = bundle.workspace.as_ref().unwrap();
                assert!(workspace.views.is_empty());
                assert!(workspace.closed_views.is_empty());
                assert!(workspace.files.deferred_recovery.is_none());
                assert!(workspace.files.queue.is_empty());
                assert!(workspace.files.worker.is_some(), "original worker must outlive Workspace source roots");
                assert!(!bundle.status().closed, "barrier/source release cannot physically close sender");
                assert!(bundle.permit.is_some());
                assert!(bundle.barrier.is_some());
                // Test-only adapter to the already existing terminal/actual-exit
                // controls; no production API extracts Workspace or a nested ticket.
                let mut workspace = bundle.workspace.take().unwrap();
                let worker = workspace.files.worker.take().unwrap();
                let ticket = bundle.ticket.take().unwrap().into_legacy_for_test(worker);
                let charge = bundle.charge.0.take();
                bundle.phase = ShutdownPhase::Complete;
                let wrapper = WorkspaceDocumentSettlementTicket {
                    ticket,
                    charge: AdapterCharge(charge),
                    live_charge: workspace.files.live_document_metadata.take(),
                };
                finish(wrapper, &mut workspace);
                assert_eq!(bundle.deadline, deadline, "all public stages retain one absolute deadline");
            })
            .join()
            .unwrap()
    });
}

#[test]
fn workspace_document_structural_recovery_refusal_never_replays_or_disconnects() {
    let scratch = Scratch::new();
    let mut workspace = workspace();
    workspace.closed_views.insert(DocumentId(999), View::new(None));
    let budget = AllocationBudget::default();
    let (transport, _ready) = setup(&mut workspace, &budget, Some(scratch.0.join("recovery")));
    let mut bundle = match workspace.into_document_shutdown(transport) {
        ControlFlow::Continue(bundle) => bundle,
        ControlFlow::Break(_) => panic!("original bind refused"),
    };
    bundle.bind_search_sources(DocumentSearchBinding::NoSearchWorker).unwrap();
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                assert_eq!(bundle.settle(LIMIT), Err(DocumentShutdownReason::RecoveryDeferred));
                let workspace = bundle.workspace.as_ref().unwrap();
                assert_eq!(workspace.files.deferred_recovery_kind, Some(RecoveryDeferKind::Structural));
                let owner = workspace.files.deferred_recovery.as_deref().unwrap() as *const _ as usize;
                let metadata = (
                    workspace.files.metadata_durable,
                    workspace.files.persist_pending,
                    workspace.files.persist_inflight,
                    workspace.files.next_observer,
                );
                let deadline = bundle.deadline;
                let charged = budget.retained_bytes();
                assert_eq!(bundle.settle(LIMIT), Err(DocumentShutdownReason::RecoveryDeferred));
                let workspace = bundle.workspace.as_ref().unwrap();
                assert_eq!(workspace.files.deferred_recovery.as_deref().unwrap() as *const _ as usize, owner);
                assert_eq!(
                    (
                        workspace.files.metadata_durable,
                        workspace.files.persist_pending,
                        workspace.files.persist_inflight,
                        workspace.files.next_observer
                    ),
                    metadata
                );
                assert_eq!(bundle.deadline, deadline);
                assert!(bundle.barrier.is_none());
                assert!(!bundle.status().closed);
                assert!(workspace.files.queue.is_empty());
                assert_eq!(workspace.closed_views.len(), 1);
                // Teardown only this blank synthetic fixture OFF UI. This does NOT
                // turn RecoveryDeferred into a successful public settlement verdict.
                let mut workspace = bundle.workspace.take().unwrap();
                workspace.release_document_sources();
                let worker = workspace.files.worker.take().unwrap();
                let ticket = bundle.ticket.take().unwrap().into_legacy_for_test(worker);
                let charge = bundle.charge.0.take();
                bundle.phase = ShutdownPhase::Complete;
                let live_charge = workspace.files.live_document_metadata.take();
                finish(
                    WorkspaceDocumentSettlementTicket { ticket, charge: AdapterCharge(charge), live_charge },
                    &mut workspace,
                );
                assert!(budget.retained_bytes() < charged);
            })
            .join()
            .unwrap()
    });
}

fn real_worker_saved_control(
    workspace: &mut Workspace,
    transport: &mut WorkspaceDocumentTransport,
    ready: &Receiver<()>,
    path: PathBuf,
) -> (DocumentId, kwil_core_workspace::SavedSnapshot) {
    let prepared = receive(transport, ready);
    assert!(matches!(accept(workspace, prepared), ControlFlow::Continue(())));
    let id = workspace.active_id();
    let editor = workspace.views.get_mut(&id).unwrap().editor_mut().unwrap();
    editor.settings.editor.trim_trailing_whitespace_on_save = true;
    editor.insert_text("a  \nb\t\nlast ", Duration::ZERO).unwrap();
    workspace.save_document(id, Some(path)).unwrap();
    let deadline = Instant::now() + LIMIT;
    loop {
        if let Some(result) = transport.try_recv(true).unwrap() {
            if let DocumentResult::Saved { document, result: Ok(saved), .. } = result {
                assert_eq!(document, id);
                return (id, saved);
            }
            assert!(matches!(accept(workspace, result), ControlFlow::Continue(())));
        } else {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "actual save completion exceeded the existing deadline");
            ready.recv_timeout(remaining).expect("real save/capacity wake");
        }
    }
}

// This control uses only APIs present in old r13: ROOT can overlay it first to
// show the real failure (flush_receipts removes SaveFlight on typed stale).
#[test]
fn workspace_document_real_worker_stale_deferred_receipt_keeps_original_save_flight() {
    use crate::editor::save::SaveCompletionState;
    let scratch = Scratch::new();
    let mut workspace = workspace();
    let budget = AllocationBudget::default();
    let (mut transport, ready) = setup(&mut workspace, &budget, Some(scratch.0.join("recovery")));
    let (id, saved) = real_worker_saved_control(&mut workspace, &mut transport, &ready, scratch.0.join("saved.txt"));
    let editor = workspace.views.get_mut(&id).unwrap().editor_mut().unwrap();
    editor.flush_edit_receipt_sealed(None).unwrap();
    let mut state = Some(SaveCompletionState::new(saved));
    state.as_mut().unwrap().admit_retention(&budget).unwrap();
    editor.prepare_save_completion(state.as_mut().unwrap(), &budget).unwrap();
    editor.retain_save_completion(&mut state).unwrap();
    editor.set_cursors(vec![crate::Cursor::caret(4)], 0).unwrap();
    let charged = budget.retained_bytes();
    // Keep the exact worker while isolating the receipt-classification defect;
    // the separate end-to-end control exercises actual continuation submission.
    let worker = workspace.files.worker.take();
    let _ = workspace.flush_receipts();
    assert!(workspace.files.saves.contains_key(&id), "typed stale preparation must retain the original SaveFlight");
    assert!(workspace.views[&id].editor().unwrap().has_pending_save_completion());
    assert_eq!(budget.retained_bytes(), charged, "stale refusal cannot retire any preparation on this thread");
    assert!(workspace.close(id, false).is_err());
    assert!(workspace.save_document(id, Some(scratch.0.join("second.txt"))).is_err());
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                workspace
                    .views
                    .get_mut(&id)
                    .unwrap()
                    .editor_mut()
                    .unwrap()
                    .take_unpublished_save_completion(&mut state)
                    .unwrap();
                drop(state); // explicit fixture-only retirement OFF UI
                workspace.views.clear();
                workspace.files.saves.remove(&id);
            })
            .join()
            .unwrap();
    });
    workspace.files.worker = worker;
    let ticket = workspace.stop_bounded_document_worker(transport).unwrap();
    finish(ticket, &mut workspace);
}

// OLD R7 fail-first overlay: copy only this test item, keeping its existing
// Result-based shutdown helper and all production bytes unchanged.
#[test]
fn workspace_document_real_worker_local_admission_refusal_keeps_flight_then_completes_and_shuts_down() {
    use crate::editor::save::SaveCompletionState;
    let scratch = Scratch::new();
    let mut workspace = workspace();
    let budget = AllocationBudget::default();
    let (mut transport, ready) = setup(&mut workspace, &budget, Some(scratch.0.join("recovery")));
    let path = scratch.0.join("saved.txt");
    let (id, saved) = real_worker_saved_control(&mut workspace, &mut transport, &ready, path.clone());
    let deadline = Instant::now() + LIMIT;
    let editor = workspace.views.get_mut(&id).unwrap().editor_mut().unwrap();
    editor.flush_edit_receipt_sealed(None).unwrap();
    let history = editor.document().buffer.history_depth();
    let mut state = Some(SaveCompletionState::new(saved));
    state.as_mut().unwrap().admit_retention(&budget).unwrap();
    editor.retain_save_completion(&mut state).unwrap();
    assert!(state.is_none(), "the actual saved owner belongs only to Editor");
    let key = workspace.files.saves[&id].resume_key(id);
    let retained = budget.retained_bytes();
    let blocker = budget.reserve(crate::visual_rows::MAX_SESSION_BYTES - retained).unwrap();
    let _ = workspace.flush_receipts();
    assert!(
        workspace.files.saves.contains_key(&id),
        "Editor-owned admission refusal must retain the original SaveFlight"
    );
    let error = workspace.save_preparation_error(id).expect("the first actual preparation error is retained");
    let actual = error.get_ref().unwrap().downcast_ref::<WrapError>().unwrap();
    assert!(matches!(actual, WrapError::Admission(_)));
    assert!(!stale_save_preparation(error), "generic admission is not stale-identity reset authority");
    let first = actual as *const WrapError as usize;
    let _ = workspace.flush_receipts();
    let flight = &workspace.files.saves[&id];
    assert_eq!(flight.resume, crate::workspace::SaveResumePhase::Local);
    assert_eq!(flight.resume_key(id), key);
    assert_eq!(flight.path.as_ref(), Some(&path));
    assert_eq!(
        workspace.save_preparation_error(id).unwrap().get_ref().unwrap().downcast_ref::<WrapError>().unwrap()
            as *const WrapError as usize,
        first,
        "a repeated admission failure cannot replace the first actual nested error"
    );
    assert!(workspace.views[&id].editor().unwrap().has_pending_save_completion());
    assert_eq!(workspace.views[&id].editor().unwrap().document().source_epoch, key.source_epoch);
    assert_eq!(workspace.views[&id].editor().unwrap().document().buffer.history_depth(), history);
    assert!(workspace.close(id, true).is_err(), "explicit discard cannot remove an accepted Editor-owned save");
    assert!(workspace.save_document(id, Some(scratch.0.join("second.txt"))).is_err());
    drop(blocker);
    let _ = workspace.flush_receipts();
    assert!(!workspace.files.saves.contains_key(&id));
    let editor = workspace.views[&id].editor().unwrap();
    assert!(!editor.has_pending_save_completion());
    assert_eq!(editor.document().buffer.history_depth().0, history.0 + 1);
    let mut bytes = Vec::new();
    editor.document().snapshot().write_to(&mut bytes).unwrap();
    assert_eq!(bytes, b"a\nb\nlast");
    let _ = workspace.flush_receipts();
    assert_eq!(workspace.views[&id].editor().unwrap().document().buffer.history_depth().0, history.0 + 1);
    finish_saved_workspace_until(workspace, transport, deadline);
}

#[test]
fn workspace_document_shutdown_local_admission_refusal_borrows_first_error_then_releases_sources() {
    use crate::editor::save::SaveCompletionState;
    let scratch = Scratch::new();
    let mut workspace = workspace();
    let budget = AllocationBudget::default();
    let (mut transport, ready) = setup(&mut workspace, &budget, Some(scratch.0.join("recovery")));
    let (id, saved) = real_worker_saved_control(&mut workspace, &mut transport, &ready, scratch.0.join("saved.txt"));
    let deadline = Instant::now() + LIMIT;
    let editor = workspace.views.get_mut(&id).unwrap().editor_mut().unwrap();
    editor.flush_edit_receipt_sealed(None).unwrap();
    let history = editor.document().buffer.history_depth();
    let mut state = Some(SaveCompletionState::new(saved));
    state.as_mut().unwrap().admit_retention(&budget).unwrap();
    editor.retain_save_completion(&mut state).unwrap();
    assert!(state.is_none());
    let key = workspace.files.saves[&id].resume_key(id);
    let mut bundle = match workspace.into_document_shutdown(transport) {
        ControlFlow::Continue(bundle) => bundle,
        ControlFlow::Break(_) => panic!("original Editor-owned save worker must bind"),
    };
    bundle.bind_search_sources(DocumentSearchBinding::NoSearchWorker).unwrap();
    bundle.deadline = Some(deadline);
    let retained = budget.retained_bytes();
    let blocker = budget.reserve(crate::visual_rows::MAX_SESSION_BYTES - retained).unwrap();
    std::thread::scope(|scope| {
        let bundle = &mut bundle;
        scope
            .spawn(move || {
                // The same private Drain permit and actual shared allowance
                // exercise the receipt stage without spending return-metadata
                // admission before reaching the intended preparation refusal.
                let mut first = None;
                for _ in 0..2 {
                    assert!(matches!(
                        bundle
                            .workspace
                            .as_mut()
                            .unwrap()
                            .flush_document_shutdown_receipts(&bundle.budget, bundle.permit.as_ref().unwrap(),),
                        Ok(ShutdownReceiptFlush::RetainedSavePreparation)
                    ));
                    let workspace = bundle.workspace.as_ref().unwrap();
                    assert_eq!(workspace.files.saves[&id].resume, crate::workspace::SaveResumePhase::Local);
                    assert_eq!(workspace.files.saves[&id].resume_key(id), key);
                    assert!(workspace.views[&id].editor().unwrap().has_pending_save_completion());
                    let error = workspace.save_preparation_error(id).unwrap();
                    assert!(matches!(
                        error.get_ref().unwrap().downcast_ref::<WrapError>(),
                        Some(WrapError::Admission(_))
                    ));
                    let actual =
                        error.get_ref().unwrap().downcast_ref::<WrapError>().unwrap() as *const WrapError as usize;
                    assert_eq!(*first.get_or_insert(actual), actual, "the first nested error survives every retry");
                    let borrowed = bundle.preparation_error().unwrap().downcast_ref::<io::Error>().unwrap();
                    assert!(std::ptr::eq(error, borrowed), "shutdown borrows the exact error from the same flight");
                    assert!(bundle.preparation_error.is_none(), "no duplicate error owner is created");
                    assert_eq!(bundle.deadline(), Some(deadline));
                    assert!(!bundle.status().closed);
                }
                drop(blocker);
                assert!(matches!(
                    bundle
                        .workspace
                        .as_mut()
                        .unwrap()
                        .flush_document_shutdown_receipts(&bundle.budget, bundle.permit.as_ref().unwrap(),),
                    Ok(ShutdownReceiptFlush::Flushed)
                ));
                let workspace = bundle.workspace.as_ref().unwrap();
                assert!(!workspace.files.saves.contains_key(&id));
                let editor = workspace.views[&id].editor().unwrap();
                assert!(!editor.has_pending_save_completion());
                assert_eq!(editor.document().buffer.history_depth().0, history.0 + 1);
                let mut bytes = Vec::new();
                editor.document().snapshot().write_to(&mut bytes).unwrap();
                assert_eq!(bytes, b"a\nb\nlast");
                let remaining = deadline.saturating_duration_since(Instant::now());
                assert!(!remaining.is_zero());
                assert_eq!(bundle.settle(remaining).unwrap(), DocumentShutdownStatus::SourcesReleased);
                assert_eq!(bundle.deadline(), Some(deadline));
                assert!(bundle.workspace.as_ref().unwrap().views.is_empty());
                assert!(bundle.workspace.as_ref().unwrap().closed_views.is_empty());
                assert!(bundle.workspace.as_ref().unwrap().files.saves.is_empty());
                assert!(!bundle.status().closed, "source release precedes physical sender closure");
                let mut workspace = bundle.workspace.take().unwrap();
                let worker = workspace.files.worker.take().unwrap();
                let ticket = bundle.ticket.take().unwrap().into_legacy_for_test(worker);
                let charge = bundle.charge.0.take();
                let live_charge = workspace.files.live_document_metadata.take();
                finish_until(
                    WorkspaceDocumentSettlementTicket { ticket, charge: AdapterCharge(charge), live_charge },
                    &mut workspace,
                    deadline,
                );
                bundle.phase = ShutdownPhase::Complete;
                assert_eq!(bundle.deadline(), Some(deadline));
            })
            .join()
            .unwrap();
    });
}

// OLD R8 fail-first overlay: copy only this test item. It uses the existing
// duration-compatible API and helpers, without any R9 production changes.
#[test]
fn workspace_document_shutdown_finalizes_earlier_save_before_later_receipt_admission_refusal() {
    use crate::editor::save::SaveCompletionState;
    let scratch = Scratch::new();
    let mut workspace = workspace();
    let budget = AllocationBudget::default();
    let (mut transport, ready) = setup(&mut workspace, &budget, Some(scratch.0.join("recovery")));
    let prepared = receive(&mut transport, &ready);
    assert!(matches!(accept(&mut workspace, prepared), ControlFlow::Continue(())));
    workspace.open(View::new(None)).unwrap();
    workspace.install_journal_observers();
    // Choose roles from the actual HashMap order. No key is inserted or
    // removed before the receipt scan, so A deterministically precedes B.
    let mut order = workspace.views.keys().copied();
    let a = order.next().unwrap();
    let b = order.next().unwrap();
    assert!(order.next().is_none());
    assert_ne!(a, b);
    assert!(workspace.activate(a));
    let editor = workspace.views.get_mut(&a).unwrap().editor_mut().unwrap();
    editor.settings.editor.trim_trailing_whitespace_on_save = true;
    editor.insert_text("A  \n", Duration::ZERO).unwrap();
    let history = editor.document().buffer.history_depth();
    let deadline = Instant::now() + LIMIT;
    workspace.save_document(a, Some(scratch.0.join("a.txt"))).unwrap();
    let saved = loop {
        if let Some(result) = transport.try_recv(true).unwrap() {
            if let DocumentResult::Saved { document, result: Ok(saved), .. } = result {
                assert_eq!(document, a);
                break saved;
            }
            assert!(matches!(accept(&mut workspace, result), ControlFlow::Continue(())));
        } else {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "two-tab actual save exceeded the original absolute deadline");
            ready.recv_timeout(remaining).expect("two-tab actual save/capacity wake");
        }
    };
    let editor = workspace.views.get_mut(&a).unwrap().editor_mut().unwrap();
    editor.flush_edit_receipt_sealed(None).unwrap();
    let mut state = Some(SaveCompletionState::new(saved));
    state.as_mut().unwrap().admit_retention(&budget).unwrap();
    editor.prepare_save_completion(state.as_mut().unwrap(), &budget).unwrap();
    editor.retain_save_completion(&mut state).unwrap();
    assert!(state.is_none(), "only A's actual Editor owns the original prepared save");
    let mut bundle = match workspace.into_document_shutdown(transport) {
        ControlFlow::Continue(bundle) => bundle,
        ControlFlow::Break(_) => panic!("original two-tab worker must bind"),
    };
    bundle.bind_search_sources(DocumentSearchBinding::NoSearchWorker).unwrap();
    let now = shutdown_contract_time();
    let deadline = now + LIMIT;
    bundle.deadline = Some(deadline);
    std::thread::scope(|scope| {
        let bundle = &mut bundle;
        scope
            .spawn(move || {
                let workspace = bundle.workspace.as_mut().unwrap();
                assert_eq!(workspace.views.keys().copied().next(), Some(a));
                let editor = workspace.views.get_mut(&b).unwrap().editor_mut().unwrap();
                // Ordinary admission is already fenced. Publishing this tiny
                // fixture edit retains its actual receipt in B; the Drain
                // permit below is the only authority allowed to submit it.
                editor.insert_text("B", Duration::ZERO).unwrap();
                let b_owner = editor as *const crate::Editor as usize;
                let b_epoch = editor.document().source_epoch;
                let b_revision = editor.document().revision();
                let b_sequence = editor.pending_edit_sequence().unwrap();
                let retained = budget.retained_bytes();
                let blocker = budget.reserve(crate::visual_rows::MAX_SESSION_BYTES - retained).unwrap();
                let outcome =
                    workspace.flush_document_shutdown_receipts(&bundle.budget, bundle.permit.as_ref().unwrap());
                assert!(
                    matches!(
                        &outcome,
                        Err(WorkspaceDocumentAcceptError::Receipt(crate::editor::ReceiptDeliveryRefusal::Admission))
                    ),
                    "B must hit the actual bounded receipt admission refusal after A commits"
                );
                let editor = workspace.views[&a].editor().unwrap();
                assert!(editor.has_completed_save_completion(), "A's Editor already committed its actual save");
                assert!(
                    !workspace.files.saves.contains_key(&a),
                    "earlier completed save must finalize its flight before a later tab's receipt refusal returns"
                );
                assert_eq!(editor.document().buffer.history_depth().0, history.0 + 1);
                let mut bytes = Vec::new();
                editor.document().snapshot().write_to(&mut bytes).unwrap();
                assert_eq!(bytes, b"A\n");
                let editor = workspace.views[&b].editor().unwrap();
                assert_eq!(editor as *const crate::Editor as usize, b_owner);
                assert_eq!(editor.document().source_epoch, b_epoch);
                assert_eq!(editor.document().revision(), b_revision);
                assert_eq!(editor.pending_edit_sequence(), Some(b_sequence));
                assert!(!bundle.status().closed, "receipt refusal retains the actual sender and both source owners");
                drop(blocker);
                let workspace = bundle.workspace.as_mut().unwrap();
                assert!(matches!(
                    workspace.flush_document_shutdown_receipts(&bundle.budget, bundle.permit.as_ref().unwrap()),
                    Ok(ShutdownReceiptFlush::Flushed)
                ));
                assert!(!workspace.files.saves.contains_key(&a));
                assert_eq!(workspace.views[&a].editor().unwrap().document().buffer.history_depth().0, history.0 + 1);
                let editor = workspace.views[&b].editor().unwrap();
                assert_eq!(editor as *const crate::Editor as usize, b_owner);
                assert_eq!(editor.document().source_epoch, b_epoch);
                assert_eq!(editor.document().revision(), b_revision);
                assert!(!editor.has_pending_edit_receipt(), "retry submitted B's exact retained receipt");
                assert!(
                    matches!(
                        outcome,
                        Err(WorkspaceDocumentAcceptError::Receipt(crate::editor::ReceiptDeliveryRefusal::Admission))
                    ),
                    "retry cannot replace the original refusal outcome"
                );
                assert_eq!(
                    settle_shutdown_contract(bundle, deadline, now).unwrap(),
                    DocumentShutdownStatus::SourcesReleased
                );
                assert_eq!(bundle.deadline(), Some(deadline));
                assert!(bundle.workspace.as_ref().unwrap().views.is_empty());
                assert!(bundle.workspace.as_ref().unwrap().closed_views.is_empty());
                assert!(bundle.workspace.as_ref().unwrap().files.saves.is_empty());
                assert!(!bundle.status().closed);
                assert_shutdown_contract_expired(bundle, deadline);
                let mut workspace = bundle.workspace.take().unwrap();
                let worker = workspace.files.worker.take().unwrap();
                let ticket = bundle.ticket.take().unwrap().into_legacy_for_test(worker);
                let charge = bundle.charge.0.take();
                let live_charge = workspace.files.live_document_metadata.take();
                finish_shutdown_contract_fixture(
                    WorkspaceDocumentSettlementTicket { ticket, charge: AdapterCharge(charge), live_charge },
                    &mut workspace,
                );
                bundle.phase = ShutdownPhase::Complete;
                assert_eq!(bundle.deadline(), Some(deadline));
            })
            .join()
            .unwrap();
    });
}

#[test]
fn workspace_document_settle_until_keeps_exact_earliest_deadline_through_source_release_and_exit() {
    let scratch = Scratch::new();
    let mut workspace = workspace();
    let budget = AllocationBudget::default();
    let baseline = budget.retained_bytes();
    let (transport, _ready) = setup(&mut workspace, &budget, Some(scratch.0.join("recovery")));
    let deadline = Instant::now() + LIMIT;
    let mut bundle = match workspace.into_document_shutdown(transport) {
        ControlFlow::Continue(bundle) => bundle,
        ControlFlow::Break(_) => panic!("original deadline control worker must bind"),
    };
    assert_eq!(bundle.settle_until(deadline), Err(DocumentShutdownReason::SearchBindingRequired));
    assert!(bundle.deadline().is_none(), "the existing search-binding precondition remains unchanged");
    bundle.bind_search_sources(DocumentSearchBinding::NoSearchWorker).unwrap();
    std::thread::scope(|scope| {
        let bundle = &mut bundle;
        scope
            .spawn(move || {
                assert_eq!(bundle.settle_until(deadline).unwrap(), DocumentShutdownStatus::SourcesReleased);
                assert_eq!(bundle.deadline(), Some(deadline), "the first Instant is retained exactly");
                assert!(!bundle.status().closed);
                assert!(bundle.workspace.as_ref().unwrap().views.is_empty());
                assert!(bundle.workspace.as_ref().unwrap().closed_views.is_empty());
                assert!(bundle.workspace.as_ref().unwrap().files.saves.is_empty());
                assert!(bundle.workspace.as_ref().unwrap().files.queue.is_empty());
                assert!(bundle.barrier.is_some());
                // NoSearchWorker already satisfies the search release gate.
                // Later calls may finish the original worker, or retain its
                // exact ticket only for the established actual-exit race.
                let assert_terminal_progress = |bundle: &WorkspaceDocumentShutdown, outcome| match outcome {
                    Ok(DocumentShutdownStatus::Complete) => {
                        assert_eq!(bundle.phase, ShutdownPhase::Complete);
                        assert!(bundle.workspace.is_none(), "all original Workspace roots have retired");
                        assert!(bundle.ticket.is_none(), "Complete follows Stopped(Ok) and the actual thread join");
                        assert!(bundle.permit.is_none());
                        assert!(bundle.charge.0.is_none());
                        assert!(bundle._bundle_charge.is_none());
                    }
                    Err(DocumentShutdownReason::Transport(DocumentResultSettlementReason::ThreadExitPending)) => {
                        assert_eq!(bundle.phase, ShutdownPhase::Stop);
                        let workspace = bundle.workspace.as_ref().unwrap();
                        assert!(workspace.views.is_empty());
                        assert!(workspace.closed_views.is_empty());
                        assert!(workspace.files.saves.is_empty());
                        assert!(workspace.files.queue.is_empty());
                        assert!(workspace.files.worker.is_some());
                        assert!(bundle.ticket.is_some(), "the exact original ticket remains until actual exit");
                        let status = bundle.status();
                        assert!(status.closed);
                        assert!(status.worker_done);
                    }
                    other => {
                        panic!("deadline control requires actual completion or its exact exit-pending race: {other:?}")
                    }
                };
                let outcome = bundle.settle_until(deadline + LIMIT);
                assert_terminal_progress(bundle, outcome);
                assert_eq!(bundle.deadline(), Some(deadline), "a later absolute request cannot extend shutdown");
                let outcome = bundle.settle(LIMIT);
                assert_terminal_progress(bundle, outcome);
                assert_eq!(bundle.deadline(), Some(deadline), "duration compatibility cannot restart the timeout");
                let earlier = deadline - Duration::from_millis(1);
                let outcome = bundle.settle_until(earlier);
                assert_terminal_progress(bundle, outcome);
                assert_eq!(bundle.deadline(), Some(earlier), "an earlier caller deadline contracts every later stage");
                let outcome = bundle.settle_until(deadline);
                assert_terminal_progress(bundle, outcome);
                assert_eq!(bundle.deadline(), Some(earlier));
                if bundle.phase != ShutdownPhase::Complete {
                    assert_eq!(bundle.phase, ShutdownPhase::Stop);
                    let mut workspace = bundle.workspace.take().unwrap();
                    let worker = workspace.files.worker.take().unwrap();
                    let ticket = bundle.ticket.take().unwrap().into_legacy_for_test(worker);
                    let charge = bundle.charge.0.take();
                    let live_charge = workspace.files.live_document_metadata.take();
                    // The pre-existing strict terminal helper accepts only
                    // Stopped(Ok) plus actual exit under this same deadline.
                    finish_until(
                        WorkspaceDocumentSettlementTicket { ticket, charge: AdapterCharge(charge), live_charge },
                        &mut workspace,
                        earlier,
                    );
                    bundle.phase = ShutdownPhase::Complete;
                }
                assert_eq!(bundle.deadline(), Some(earlier));
                assert_eq!(bundle.settle_until(deadline).unwrap(), DocumentShutdownStatus::Complete);
                assert_eq!(bundle.deadline(), Some(earlier));
            })
            .join()
            .unwrap();
    });
    drop(bundle);
    assert_eq!(budget.retained_bytes(), baseline, "actual terminal cleanup retires every charged owner");
}

#[test]
fn workspace_document_error_display_and_source_keep_actual_nested_io_error() {
    #[derive(Debug)]
    struct Original;
    impl fmt::Display for Original {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("original refusal")
        }
    }
    impl std::error::Error for Original {}
    let error = WorkspaceDocumentAcceptError::Preparation(io::Error::new(io::ErrorKind::InvalidInput, Original));
    assert!(error.to_string().contains("original refusal"));
    let nested = std::error::Error::source(&error).unwrap().downcast_ref::<io::Error>().unwrap();
    assert_eq!(nested.kind(), io::ErrorKind::InvalidInput);
    assert!(nested.get_ref().unwrap().downcast_ref::<Original>().is_some(), "the original typed error must survive");
    let plain = io::Error::new(io::ErrorKind::InvalidInput, "StaleIdentity");
    assert!(!stale_save_preparation(&plain), "strings and InvalidInput alone cannot authorize reset");
}

fn await_real_save_slot(
    workspace: &mut Workspace,
    transport: &mut WorkspaceDocumentTransport,
    deadline: Instant,
) -> crate::document_worker::result_transport::DocumentDeliveryId {
    await_real_save_slot_with(workspace, transport, deadline, |_| {})
}

fn await_real_save_slot_with(
    workspace: &mut Workspace,
    transport: &mut WorkspaceDocumentTransport,
    deadline: Instant,
    mut after_empty_observation: impl FnMut(&mut WorkspaceDocumentTransport),
) -> crate::document_worker::result_transport::DocumentDeliveryId {
    loop {
        if transport.result_info().is_none() {
            after_empty_observation(transport);
        }
        match transport.receiver.next_save_test_event(deadline) {
            Some(info) if info.kind == ResultKind::Saved => return info.delivery().unwrap(),
            Some(_) => {
                // An unaccepted delivery cannot change until this sole consumer
                // accepts it. Completed residuals are excluded under the lock.
                drop(transport.receive(workspace, true).unwrap());
            }
            None => {
                // The helper claimed a real capacity generation, not a result.
                // Run its fixture's capacity work without a receive that could
                // accidentally accept a Saved published since the observation.
                let _ = workspace.flush_receipts();
                workspace.pump_files();
                workspace.pump_hex();
            }
        }
    }
}

fn await_real_save_retirement(
    transport: &WorkspaceDocumentTransport,
    delivery: crate::document_worker::result_transport::DocumentDeliveryId,
    deadline: Instant,
) {
    transport.receiver.wait_save_retired_for_test(delivery, deadline);
}

fn hold_save_before_publication(workspace: &mut Workspace, lane: &mut HeldLiveLane, path: PathBuf) -> DocumentId {
    lane.until(workspace, "save-race/prepare", |workspace, info, _| {
        workspace.recovery_ready() && info.is_some_and(|info| info.kind == ResultKind::Prepared)
    });
    lane.resume();
    lane.until(workspace, "save-race/empty-capacity", |_, _, lane| !lane.transport.status().occupied);
    let id = workspace.active_id();
    workspace.views.get_mut(&id).unwrap().editor_mut().unwrap().insert_text("saved text", Duration::ZERO).unwrap();
    workspace.save_document(id, Some(path)).unwrap();
    assert!(lane.transport.result_info().is_none(), "the producer is held in the previous capacity wake");
    id
}

#[test]
fn workspace_document_save_slot_arrival_after_empty_observation_stays_unaccepted() {
    let scratch = Scratch::new();
    let mut workspace = workspace();
    let budget = AllocationBudget::default();
    let mut lane = HeldLiveLane::start(&mut workspace, &budget, scratch.0.join("recovery"));
    let id = hold_save_before_publication(&mut workspace, &mut lane, scratch.0.join("saved.txt"));
    workspace.set_document_commit_fault_for_test(commit::CommitFault::AfterSavePreparation);
    // Only AFTER the helper observes the empty slot may the real producer
    // publish Saved. Receiving based on that observation must not consume it.
    let mut scheduled = false;
    let delivery = await_real_save_slot_with(&mut workspace, &mut lane.transport, lane.deadline, |transport| {
        assert!(!scheduled);
        assert!(transport.result_info().is_none());
        scheduled = true;
        lane.held.store(false, Ordering::Release);
        lane.resume.send(()).unwrap();
        lane.ready.recv_timeout(lane.deadline.saturating_duration_since(Instant::now())).unwrap();
        assert_eq!(transport.result_info().unwrap().kind, ResultKind::Saved);
    });
    assert!(scheduled);
    assert_eq!(lane.transport.result_info().unwrap().delivery(), Some(delivery));
    assert!(workspace.files.saves.contains_key(&id));
    assert!(lane.transport.retained_commit_error().commit().is_none(), "Saved has not been received at all");
    workspace.set_document_commit_fault_for_test(commit::CommitFault::None);
    assert!(lane.transport.receive(&mut workspace, true).unwrap().is_some());
    await_real_save_retirement(&lane.transport, delivery, lane.deadline);
    finish_saved_workspace_until(workspace, lane.transport, lane.deadline);
}

#[test]
fn workspace_document_save_retirement_does_not_wait_for_a_later_delivery() {
    let scratch = Scratch::new();
    let mut workspace = workspace();
    let budget = AllocationBudget::default();
    let mut lane = HeldLiveLane::start(&mut workspace, &budget, scratch.0.join("recovery"));
    hold_save_before_publication(&mut workspace, &mut lane, scratch.0.join("saved.txt"));
    workspace.files.worker.as_ref().unwrap().try_submit(DocumentRequest::Persist(workspace.session.clone())).unwrap();
    lane.held.store(false, Ordering::Release);
    lane.resume();
    let delivery = await_real_save_slot(&mut workspace, &mut lane.transport, lane.deadline);
    assert!(lane.transport.receive(&mut workspace, true).unwrap().is_some());
    let next = loop {
        if let Some(info) = lane.transport.receiver.next_save_test_event(lane.deadline) {
            break info;
        }
    };
    assert_ne!(next.delivery(), Some(delivery));
    assert!(lane.transport.status().occupied, "a later unaccepted delivery now occupies the same slot");
    // All previous wake units are consumed. The later result cannot retire
    // until accepted, so waiting on global occupancy deterministically stalls.
    while lane.ready.try_recv().is_ok() {}
    await_real_save_retirement(&lane.transport, delivery, lane.deadline);
    assert_eq!(lane.transport.result_info().unwrap().delivery(), next.delivery(), "leave the later result untouched");
    finish_saved_workspace_until(workspace, lane.transport, lane.deadline);
}

fn acknowledge_reset_on_real_wake(
    workspace: &Workspace,
    transport: &mut WorkspaceDocumentTransport,
    ready: &Receiver<()>,
    mut token: crate::document_worker::DocumentProducerRetryToken,
    deadline: Instant,
) {
    loop {
        match transport.acknowledge_prepublication_reset(workspace, token) {
            Ok(()) => return,
            Err(refusal) => {
                assert!(matches!(
                    refusal.reason,
                    DocumentResultSettlementReason::Busy | DocumentResultSettlementReason::RetryUnavailable
                ));
                token = refusal.token;
                let remaining = deadline.saturating_duration_since(Instant::now());
                assert!(!remaining.is_zero(), "reset ACK exceeded the same absolute deadline");
                ready.recv_timeout(remaining).expect("original producer reset-ready wake");
            }
        }
    }
}

fn receive_save_with_refusal_diagnostics(
    workspace: &mut Workspace,
    transport: &mut WorkspaceDocumentTransport,
) -> Result<Option<WorkspaceDocumentUpdate>, DocumentResultSettlementReason> {
    let outcome = transport.receive(workspace, true);
    if let Err(reason) = &outcome {
        let inspection = transport.retained_commit_error();
        let commit = inspection.commit();
        let first = inspection.retained_commit_error();
        eprintln!(
            "save acceptance diagnostics: settlement={reason:?} phase={:?} installed={:?} current_refusal={:?} retained_first={first:?} retained_first_source={:?}",
            commit.map(commit::DocumentCommit::phase),
            commit.map(commit::DocumentCommit::installed_fields),
            commit.and_then(commit::DocumentCommit::last_refusal),
            first.and_then(std::error::Error::source),
        );
    }
    outcome
}

#[test]
fn workspace_document_real_worker_resets_exact_prepublication_save_then_publishes_once() {
    let scratch = Scratch::new();
    let mut workspace = workspace();
    let budget = AllocationBudget::default();
    let (mut transport, ready) = setup(&mut workspace, &budget, Some(scratch.0.join("recovery")));
    let prepared = receive(&mut transport, &ready);
    assert!(matches!(accept(&mut workspace, prepared), ControlFlow::Continue(())));
    let id = workspace.active_id();
    let editor = workspace.views.get_mut(&id).unwrap().editor_mut().unwrap();
    editor.settings.editor.trim_trailing_whitespace_on_save = true;
    editor.insert_text("a  \nb\t\nlast ", Duration::ZERO).unwrap();
    workspace.save_document(id, Some(scratch.0.join("saved.txt"))).unwrap();
    let deadline = Instant::now() + LIMIT;
    let delivery = await_real_save_slot(&mut workspace, &mut transport, deadline);
    workspace.set_document_commit_fault_for_test(commit::CommitFault::AfterSavePreparation);
    assert!(matches!(transport.receive(&mut workspace, true), Err(DocumentResultSettlementReason::ConsumerRefused)));
    let first = {
        let inspection = transport.retained_commit_error();
        let error = inspection.retained_commit_error().unwrap();
        assert!(error.to_string().contains("controlled prepared save pause"));
        std::error::Error::source(error).unwrap().downcast_ref::<io::Error>().unwrap() as *const io::Error as usize
    };
    let charged = budget.retained_bytes();
    let editor = workspace.views.get_mut(&id).unwrap().editor_mut().unwrap();
    editor.set_cursors(vec![crate::Cursor::caret(4)], 0).unwrap();
    editor.set_viewport(1, 2);
    let history = editor.document().buffer.history_depth();
    assert!(matches!(transport.receive(&mut workspace, true), Err(DocumentResultSettlementReason::ConsumerRefused)));
    assert_eq!(budget.retained_bytes(), charged, "typed stale refusal retains all preparation charges");
    assert_eq!(transport.result_info().unwrap().delivery(), Some(delivery));
    let token =
        transport.request_prepublication_reset(delivery, deadline.saturating_duration_since(Instant::now())).unwrap();
    acknowledge_reset_on_real_wake(&workspace, &mut transport, &ready, token, deadline);
    {
        let inspection = transport.retained_commit_error();
        let nested = std::error::Error::source(inspection.retained_commit_error().unwrap()).unwrap();
        assert_eq!(
            nested.downcast_ref::<io::Error>().unwrap() as *const io::Error as usize,
            first,
            "reset and matching scalar ACK cannot replace or clear the first actual error"
        );
    }
    assert_eq!(transport.result_info().unwrap().delivery(), Some(delivery));
    assert!(workspace.files.saves.contains_key(&id));
    assert!(receive_save_with_refusal_diagnostics(&mut workspace, &mut transport).unwrap().is_some());
    assert!(!workspace.files.saves.contains_key(&id));
    let editor = workspace.views[&id].editor().unwrap();
    let mut bytes = Vec::new();
    editor.document().snapshot().write_to(&mut bytes).unwrap();
    assert_eq!(bytes, b"a\nb\nlast");
    assert_eq!(editor.primary_cursor().head, 2, "reprepare must remap the current cursor");
    assert_eq!(editor.document().buffer.history_depth().0, history.0 + 1);
    await_real_save_retirement(&transport, delivery, deadline);
    assert!(transport.retained_commit_error().retained_commit_error().is_none());
    finish_saved_workspace_until(workspace, transport, deadline);
}

#[test]
fn workspace_document_real_worker_deferred_reset_retains_flight_through_fresh_admission_refusal() {
    use crate::editor::save::SaveCompletionState;
    let scratch = Scratch::new();
    let mut workspace = workspace();
    let budget = AllocationBudget::default();
    let (mut transport, ready) = setup(&mut workspace, &budget, Some(scratch.0.join("recovery")));
    let (id, saved) = real_worker_saved_control(&mut workspace, &mut transport, &ready, scratch.0.join("saved.txt"));
    let editor = workspace.views.get_mut(&id).unwrap().editor_mut().unwrap();
    editor.flush_edit_receipt_sealed(None).unwrap();
    let mut state = Some(SaveCompletionState::new(saved));
    state.as_mut().unwrap().admit_retention(&budget).unwrap();
    editor.prepare_save_completion(state.as_mut().unwrap(), &budget).unwrap();
    editor.retain_save_completion(&mut state).unwrap();
    editor.set_cursors(vec![crate::Cursor::caret(4)], 0).unwrap();
    let _ = workspace.flush_receipts();
    assert!(workspace.files.saves.contains_key(&id));
    let deadline = Instant::now() + LIMIT;
    let delivery = await_real_save_slot(&mut workspace, &mut transport, deadline);
    assert_eq!(transport.result_info().unwrap().generation, Some(1));
    assert!(matches!(transport.receive(&mut workspace, true), Err(DocumentResultSettlementReason::ConsumerRefused)));
    assert!(
        !workspace.views[&id].editor().unwrap().has_pending_save_completion(),
        "the same state moved to the original slot"
    );
    assert!(workspace.files.saves.contains_key(&id));
    {
        let inspection = transport.retained_commit_error();
        let original = std::error::Error::source(inspection.retained_commit_error().unwrap()).unwrap();
        assert!(stale_save_preparation(original.downcast_ref::<io::Error>().unwrap()));
    }
    let token =
        transport.request_prepublication_reset(delivery, deadline.saturating_duration_since(Instant::now())).unwrap();
    acknowledge_reset_on_real_wake(&workspace, &mut transport, &ready, token, deadline);
    let retained = budget.retained_bytes();
    let blocker = budget.reserve(crate::visual_rows::MAX_SESSION_BYTES - retained).unwrap();
    assert!(matches!(transport.receive(&mut workspace, true), Err(DocumentResultSettlementReason::ConsumerRefused)));
    assert!(workspace.files.saves.contains_key(&id));
    assert_eq!(transport.result_info().unwrap().delivery(), Some(delivery));
    assert!(workspace.close(id, true).is_err(), "even explicit discard cannot remove the accepted flight");
    drop(blocker);
    assert_eq!(
        budget.retained_bytes(),
        retained,
        "failed fresh preparation retained its original header and saved owner"
    );
    assert!(receive_save_with_refusal_diagnostics(&mut workspace, &mut transport).unwrap().is_some());
    assert!(!workspace.files.saves.contains_key(&id));
    let mut bytes = Vec::new();
    workspace.views[&id].editor().unwrap().document().snapshot().write_to(&mut bytes).unwrap();
    assert_eq!(bytes, b"a\nb\nlast");
    await_real_save_retirement(&transport, delivery, deadline);
    finish_saved_workspace_until(workspace, transport, deadline);
}

#[test]
fn workspace_document_shutdown_deferred_reset_invalidates_old_barrier_and_keeps_absolute_deadline() {
    use crate::editor::save::SaveCompletionState;
    let scratch = Scratch::new();
    let mut workspace = workspace();
    let budget = AllocationBudget::default();
    let (mut transport, ready) = setup(&mut workspace, &budget, Some(scratch.0.join("recovery")));
    let (id, saved) = real_worker_saved_control(&mut workspace, &mut transport, &ready, scratch.0.join("saved.txt"));
    let editor = workspace.views.get_mut(&id).unwrap().editor_mut().unwrap();
    editor.flush_edit_receipt_sealed(None).unwrap();
    let mut state = Some(SaveCompletionState::new(saved));
    state.as_mut().unwrap().admit_retention(&budget).unwrap();
    editor.prepare_save_completion(state.as_mut().unwrap(), &budget).unwrap();
    editor.retain_save_completion(&mut state).unwrap();
    editor.set_cursors(vec![crate::Cursor::caret(4)], 0).unwrap();
    let mut bundle = match workspace.into_document_shutdown(transport) {
        ControlFlow::Continue(bundle) => bundle,
        ControlFlow::Break(_) => panic!("original save worker must bind"),
    };
    bundle.bind_search_sources(DocumentSearchBinding::NoSearchWorker).unwrap();
    std::thread::scope(|scope| {
        let bundle = &mut bundle;
        scope
            .spawn(move || {
                // Deliberately install a marker before a still-possible accepted
                // continuation; only a newer matching marker may release sources.
                let now = shutdown_contract_time();
                let deadline = now + LIMIT;
                let old = bundle.permit.as_ref().unwrap().request_barrier().unwrap();
                bundle.barrier = Some(old);
                assert_eq!(
                    settle_shutdown_contract(bundle, deadline, now),
                    Err(DocumentShutdownReason::Transport(DocumentResultSettlementReason::ConsumerRefused))
                );
                let deadline = bundle.deadline().unwrap();
                let delivery = bundle.result_info().unwrap().delivery().unwrap();
                let token = bundle.request_prepublication_reset_with_clock(delivery, LIMIT, || now).unwrap();
                bundle.ticket.as_ref().unwrap().wait_for_reset_for_test(&token);
                bundle.acknowledge_prepublication_reset_at(token, now).unwrap();
                assert_eq!(bundle.deadline(), Some(deadline));
                assert!(bundle.workspace.as_ref().unwrap().files.saves.contains_key(&id));
                assert_eq!(
                    settle_shutdown_contract(bundle, deadline, now).unwrap(),
                    DocumentShutdownStatus::SourcesReleased
                );
                assert_ne!(
                    bundle.barrier,
                    Some(old),
                    "the deferred-save continuation invalidates the old source epoch"
                );
                assert_eq!(bundle.deadline(), Some(deadline));
                assert!(bundle.workspace.as_ref().unwrap().views.is_empty());
                assert_shutdown_contract_expired(bundle, deadline);
                let mut workspace = bundle.workspace.take().unwrap();
                let worker = workspace.files.worker.take().unwrap();
                let ticket = bundle.ticket.take().unwrap().into_legacy_for_test(worker);
                let charge = bundle.charge.0.take();
                bundle.phase = ShutdownPhase::Complete;
                let live_charge = workspace.files.live_document_metadata.take();
                finish_shutdown_contract_fixture(
                    WorkspaceDocumentSettlementTicket { ticket, charge: AdapterCharge(charge), live_charge },
                    &mut workspace,
                );
                assert_eq!(bundle.deadline(), Some(deadline));
            })
            .join()
            .unwrap();
    });
}
