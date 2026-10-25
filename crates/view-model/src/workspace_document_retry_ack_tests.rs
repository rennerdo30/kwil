//! Real-worker controls. ROOT owns execution; no production transport hooks.
use super::*;
use crate::{
    visual_rows::{BodyOwner, LayoutGeometry, SnapshotSource, SourceIdentity, WrapSource},
    workspace::workspace_wrap::WorkspaceWrap,
    wrap_host::WrapHost,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, SyncSender},
};

const LIMIT: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub(super) enum RetirementObservation {
    BeforeWait,
    Returned(Result<DocumentShutdownStatus, DocumentShutdownReason>),
}

struct ReleaseWake {
    enabled: Arc<AtomicBool>,
    pulse: SyncSender<()>,
}
impl ReleaseWake {
    fn release(&self) {
        self.enabled.store(false, Ordering::Release);
        let _ = self.pulse.try_send(());
    }
}
impl Drop for ReleaseWake {
    fn drop(&mut self) {
        self.release();
    }
}
struct ReleaseSource(SyncSender<()>);
impl Drop for ReleaseSource {
    fn drop(&mut self) {
        let _ = self.0.try_send(());
    }
}
struct GatedSource {
    inner: SnapshotSource,
    first: AtomicBool,
    entered: SyncSender<()>,
    release: Mutex<Receiver<()>>,
    deadline: Instant,
    ui: std::thread::ThreadId,
    _charge: AllocationCharge,
}
impl WrapSource for GatedSource {
    fn run_layout(
        self: Arc<Self>,
        engine: crate::visual_rows::LayoutEngine,
        request: crate::visual_rows::TaskRequest,
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
    fn line_of(&self, at: u64) -> io::Result<u64> {
        self.inner.line_of(at)
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
        assert_ne!(std::thread::current().id(), self.ui, "actual source reads must stay off UI");
        if self.first.swap(false, Ordering::AcqRel) {
            self.entered.try_send(()).map_err(io::Error::other)?;
            self.release.lock().unwrap().recv_timeout(remaining(self.deadline)).map_err(io::Error::other)?;
        }
        Ok(())
    }
}
fn remaining(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}
fn advance_wake(
    pulse: &SyncSender<()>,
    returned: &Receiver<std::thread::ThreadId>,
    deadline: Instant,
) -> std::thread::ThreadId {
    pulse.try_send(()).unwrap();
    returned.recv_timeout(remaining(deadline)).unwrap()
}
fn accepted<B, T>(flow: ControlFlow<B, T>) -> T {
    match flow {
        ControlFlow::Continue(value) => value,
        ControlFlow::Break(owner) => {
            std::mem::forget(owner);
            panic!("actual owner unexpectedly refused");
        }
    }
}

fn run_control(receive_live: bool) {
    let root = std::env::temp_dir().join(format!(
        "kwil-document-retry-ack-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
    ));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("clean.txt");
    std::fs::write(&path, b"abcdef\n").unwrap();
    let file = kwil_core_text::TextFile::open(&path).unwrap();
    let document = crate::Document::from_file(path.clone(), file).unwrap();
    let mut view = crate::View::new(None);
    view.path = Some(path);
    view.editor = Some(crate::Editor::new(document));
    view.editor_mut().unwrap().settings.editor.word_wrap = true;
    let mut workspace = crate::Workspace::new(view).unwrap();
    let budget = AllocationBudget::default();
    let deadline = Instant::now() + LIMIT;
    let enabled = Arc::new(AtomicBool::new(true));
    let worker_enabled = enabled.clone();
    let (entered, entering) = mpsc::sync_channel(32);
    let (pulse, pulses) = mpsc::sync_channel(1);
    let pulses = Mutex::new(pulses);
    let (returned, returning) = mpsc::sync_channel(32);
    let release_wake = ReleaseWake { enabled, pulse };
    let mut transport = workspace
        .start_bounded_document_worker_at(Some(root.join("recovery")), &budget, move || {
            if worker_enabled.load(Ordering::Acquire) {
                let thread = std::thread::current().id();
                entered.try_send(thread).map_err(io::Error::other)?;
                pulses.lock().unwrap().recv_timeout(remaining(deadline)).map_err(io::Error::other)?;
                returned.try_send(thread).map_err(io::Error::other)?;
            }
            Ok(())
        })
        .unwrap();

    // Every callback is gated outside all transport locks. Observe an actual
    // empty capacity callback after preparation, without counting or guessing
    // wake order and without inspecting source-bearing result contents.
    let producer = loop {
        let producer = entering.recv_timeout(remaining(deadline)).unwrap();
        let empty = transport.result_info().is_none();
        let outcome = transport.receive(&mut workspace, true);
        let returned = advance_wake(&release_wake.pulse, &returning, deadline);
        assert_eq!(producer, returned);
        drop(outcome.unwrap());
        if empty && !workspace.files.preparing && !transport.status().occupied {
            break producer;
        }
    };
    let mut host = WrapHost::with_budget(budget.clone(), || {}).unwrap();
    let mut owner = WorkspaceWrap::new(&mut workspace, &host).unwrap();
    let context = owner
        .prepare_context(&workspace, LayoutGeometry { columns: 3, rows: 2, ..Default::default() }, BodyOwner::Text)
        .unwrap();
    host.sync(workspace.editor(), context).unwrap();
    let (source_entered, source_reading) = mpsc::sync_channel(1);
    let (release, held) = mpsc::sync_channel(1);
    let release_source = ReleaseSource(release);
    let source: Arc<dyn WrapSource> = Arc::new(GatedSource {
        inner: SnapshotSource::new(context.key.source, workspace.editor().unwrap().document().snapshot()),
        first: AtomicBool::new(true),
        entered: source_entered,
        release: Mutex::new(held),
        deadline,
        ui: std::thread::current().id(),
        _charge: budget.reserve(size_of::<GatedSource>() + 2 * size_of::<usize>()).unwrap(),
    });
    accepted(host.drive(source));
    source_reading.recv_timeout(remaining(deadline)).unwrap();
    let ticket = accepted(owner.stop(host));
    let failed = ticket.settle(Duration::ZERO).err().expect("the actual wrap worker remains source-gated");
    workspace.begin_wrap_shutdown();
    accepted(workspace.submit_wrap_settlement(*failed.ticket, Duration::ZERO));

    // Keep advancing actual capacity/result callbacks until this same worker
    // publishes the failed Wrap result. Its callback is released BEFORE the
    // retry is requested, so it cannot be mistaken for the successful wake.
    loop {
        let entered = entering.recv_timeout(remaining(deadline)).unwrap();
        let wrap = transport.result_info().is_some_and(|info| info.kind == ResultKind::Wrap);
        let outcome = if wrap { Ok(None) } else { transport.receive(&mut workspace, true) };
        let returned = advance_wake(&release_wake.pulse, &returning, deadline);
        assert_eq!(entered, producer);
        assert_eq!(returned, producer);
        drop(outcome.unwrap());
        if wrap {
            break;
        }
    }
    let token = transport.request_wrap_retry(remaining(deadline)).unwrap();
    release_source.0.try_send(()).unwrap();
    let entered = entering.recv_timeout(remaining(deadline)).unwrap();
    // This callback intentionally stays blocked until the test controller
    // observes BeforeWait OR the mutant's premature returned status.
    let live_outcome = if receive_live { transport.receive(&mut workspace, true) } else { Ok(None) };
    let mut bundle = accepted(workspace.into_document_shutdown(transport));
    bundle.bind_search_sources(DocumentSearchBinding::NoSearchWorker).unwrap();
    let (observe, observations) = mpsc::sync_channel(2);
    bundle.before_retirement_wait = Some(observe.clone());
    let (resume, resumed) = mpsc::sync_channel(1);
    std::thread::scope(|scope| {
        let bundle = &mut bundle;
        let executor = scope.spawn(move || {
            let outcome = bundle.settle_until(deadline);
            let occupied_on_return = bundle.status().occupied;
            observe.try_send(RetirementObservation::Returned(outcome)).unwrap();
            resumed.recv_timeout(remaining(deadline)).unwrap();
            // Cleanup works even for the old two-line mutant: wait only on the
            // original capacity condvar after the controller releases the wake.
            while bundle.status().occupied {
                bundle.ticket.as_ref().unwrap().wait_until(deadline).unwrap();
            }
            bundle.acknowledge_wrap_retry(token).unwrap();
            assert!(!bundle.status().retry_acknowledgement_required);
            assert_eq!(bundle.deadline(), Some(deadline));
            assert_eq!(bundle.settle_until(deadline).unwrap(), DocumentShutdownStatus::SourcesReleased);
            assert!(bundle.workspace.as_ref().unwrap().views.is_empty());
            assert!(bundle.workspace.as_ref().unwrap().closed_views.is_empty());
            assert!(!bundle.status().closed);
            // Same established test-only terminal policy as the held controls:
            // require Stopped(Ok)+actual exit and retain only ThreadExitPending.
            let mut workspace = bundle.workspace.take().unwrap();
            let worker = workspace.files.worker.take().unwrap();
            let mut ticket = WorkspaceDocumentSettlementTicket {
                ticket: bundle.ticket.take().unwrap().into_legacy_for_test(worker),
                charge: AdapterCharge(bundle.charge.0.take()),
                live_charge: workspace.files.live_document_metadata.take(),
            };
            loop {
                let left = remaining(deadline);
                assert!(!left.is_zero());
                match ticket.settle(left, |result| workspace.document_result(result).map_continue(|_| ())) {
                    Ok(()) => break,
                    Err(failure) => {
                        assert_eq!(failure.reason, DocumentResultSettlementReason::ThreadExitPending);
                        ticket = failure.ticket;
                        std::thread::park_timeout(Duration::from_millis(1).min(left));
                    }
                }
            }
            bundle.phase = ShutdownPhase::Complete;
            (outcome, occupied_on_return)
        });
        let observation = observations.recv_timeout(remaining(deadline));
        // Always release the publishing worker and join the model executor
        // BEFORE any regression assertion, including the old-mutant failure.
        release_wake.release();
        let _ = resume.try_send(());
        let outcome = executor.join();
        assert_eq!(entered, producer, "the exact original document producer published the retry");
        drop(live_outcome.unwrap());
        let (outcome, occupied) = outcome.unwrap();
        match observation {
            Ok(RetirementObservation::BeforeWait) => {}
            Ok(RetirementObservation::Returned(returned)) => panic!(
                "wrap retry readiness returned before the original producer retired its completed slot: {returned:?}"
            ),
            Err(error) => panic!("original retirement wait observation failed: {error}"),
        }
        assert_eq!(outcome, Ok(DocumentShutdownStatus::WrapRetryReceiptPending));
        assert!(!occupied, "a returned wrap retry status must already permit its matching token ACK");
    });
    assert_eq!(bundle.deadline(), Some(deadline));
    drop(bundle);
    assert!(root.is_absolute() && root.starts_with(std::env::temp_dir()));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn workspace_document_wrap_retry_waits_for_completed_slot_retirement_before_ack() {
    run_control(false);
}

#[test]
fn workspace_document_wrap_retry_waits_for_already_accepted_slot_retirement_before_ack() {
    run_control(true);
}
