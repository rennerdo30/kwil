//! Real-worker controls for the source-only shutdown phase. These are authored
//! acceptance tests; execution belongs to ROOT's admitted validation run.
use super::*;
use std::sync::mpsc::{self, Receiver};

const WAIT: Duration = Duration::from_secs(5);

fn request(generation: u64, text: &[u8]) -> SearchRequest {
    let mut buffer = kwil_core_text::Buffer::empty();
    buffer.insert(0, text).unwrap();
    SearchRequest {
        key: SearchKey { document_id: 1, revision: 0, generation },
        task: SearchTask::Document(DocumentSearch {
            snapshot: buffer.snapshot(),
            query: Query::literal("x".into()),
            ranges: std::iter::once(0..text.len() as u64).collect(),
            page: Page::default(),
            navigation: None,
            replacement: None,
        }),
    }
}

fn submit_on_wake(worker: &SearchWorker, mut request: SearchRequest, wakes: &Receiver<()>) {
    let deadline = Instant::now() + WAIT;
    loop {
        match worker.submit(request) {
            Ok(()) => break,
            Err(error) => {
                assert_eq!(error.reason, "search mailbox busy; retry on wake");
                request = *error.request;
                wakes.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
            }
        }
    }
}

fn pinned_replacement(worker: &SearchWorker, wakes: &Receiver<()>) -> ReplaceRecipe {
    let mut replace = request(1, b"x");
    let SearchTask::Document(doc) = &mut replace.task else { unreachable!() };
    doc.replacement = Some(Replacement { template: b"y".to_vec(), target: ReplaceTarget::All });
    submit_on_wake(worker, replace, wakes);
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(SearchEvent { update: SearchUpdate::Document(result), .. }) = worker.take_event() {
            return result.replacement.unwrap();
        }
        wakes.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
    }
}

struct Gate {
    armed: Arc<AtomicBool>,
    entered: Receiver<()>,
    release: mpsc::Sender<()>,
}
impl Drop for Gate {
    fn drop(&mut self) {
        let _ = self.release.send(());
    }
}
fn gated_worker() -> (SearchWorker, Receiver<()>, Gate) {
    let armed = Arc::new(AtomicBool::new(false));
    let worker_armed = armed.clone();
    let (entered_tx, entered) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let (wake_tx, wakes) = mpsc::channel();
    let worker = SearchWorker::new(move || {
        let _ = wake_tx.send(());
        if worker_armed.swap(false, Ordering::AcqRel) {
            entered_tx.send(()).unwrap();
            release_rx.lock().unwrap().recv_timeout(WAIT).unwrap();
            // Model host/library code consuming the coordinator's park token.
            // The self-unpark coalesces with any earlier close notification;
            // the following park consumes that single token without waiting.
            // Late close is admitted BEFORE this gate is released, so the old
            // unconditional outer park deterministically loses that wake.
            thread::current().unpark();
            thread::park();
        }
    })
    .unwrap();
    (worker, wakes, Gate { armed, entered, release })
}

#[test]
fn accepted_snapshot_releases_off_ui_before_externally_pinned_spool() {
    let parent = std::env::temp_dir().join(format!(
        "kwil-search-input-release-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir(&parent).unwrap();
    let path = parent.join("source.txt");
    std::fs::write(&path, b"x").unwrap();
    let file = kwil_core_text::TextFile::open(&path).unwrap();
    file.build_index(1).unwrap();
    let buffer = kwil_core_text::Buffer::from_file(file.clone()).unwrap();
    let snapshot = buffer.snapshot();
    drop(buffer);
    let source = Arc::new(Mutex::new(Some(file)));
    let observed = source.clone();
    let gate = Arc::new(AtomicBool::new(false));
    let worker_gate = gate.clone();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let (closed_tx, closed_rx) = mpsc::channel();
    let (wake_tx, wakes) = mpsc::channel();
    let worker = SearchWorker::new(move || {
        let _ = wake_tx.send(());
        if worker_gate.swap(false, Ordering::AcqRel) {
            entered_tx.send(()).unwrap();
            release_rx.lock().unwrap().recv_timeout(WAIT).unwrap();
        }
        let mut source = observed.lock().unwrap();
        if let Some(file) = source.take() {
            match file.try_close() {
                Ok(()) => {
                    let _ = closed_tx.send(thread::current().name().unwrap_or("").to_owned());
                }
                Err(file) => *source = Some(file),
            }
        }
    })
    .unwrap();
    let spool = pinned_replacement(&worker, &wakes);
    let deadline = Instant::now() + WAIT;
    assert!(matches!(&spool.edits, ReplacementEdits::Spool(_)));
    gate.store(true, Ordering::Release);
    let mut accepted = request(2, b"x");
    let SearchTask::Document(doc) = &mut accepted.task else { unreachable!() };
    doc.snapshot = snapshot;
    submit_on_wake(&worker, accepted, &wakes);
    entered_rx.recv_timeout(WAIT).unwrap();
    let key = worker.source_release_key(); // Bind before the first stop.
    worker.stop();
    let file = source.lock().unwrap().take().unwrap();
    *source.lock().unwrap() = Some(file.try_close().expect_err("accepted snapshot still pins source"));
    assert!(matches!(worker.wait_inputs_released(Duration::ZERO), Err(SearchInputsReleaseError::TimedOut)));
    release_tx.send(()).unwrap();
    let receipt = worker.wait_inputs_released(deadline.saturating_duration_since(Instant::now())).unwrap();
    assert!(receipt.valid_for(&key));
    assert_eq!(closed_rx.recv_timeout(WAIT).unwrap(), "kwil-search");
    assert_eq!(worker.shared.state.lock().unwrap().live_spools, 1, "document recipe can still pin search spool");
    assert!(!worker.shared.completed.load(Ordering::Acquire), "source ACK is earlier than final retirement");
    drop(spool);
    worker.shutdown_bounded(WAIT).unwrap();
    assert!(source.lock().unwrap().is_none());
    std::fs::remove_file(path).unwrap();
    std::fs::remove_dir(parent).unwrap();
}

#[test]
fn later_closing_owner_invalidates_before_move_and_cannot_reopen_drained_slots() {
    let (worker, wakes, gate) = gated_worker();
    let spool = pinned_replacement(&worker, &wakes);
    let original = worker.source_release_key();
    worker.stop();
    let old_receipt = worker.wait_inputs_released(WAIT).unwrap();
    worker.stop();
    worker.close_with_requests(None, None).continue_value().unwrap();
    assert!(old_receipt.valid_for(&original), "empty repeated stop preserves proof");
    gate.armed.store(true, Ordering::Release);
    worker.retry_retirement(); // One explicit wake, never a polling timer.
    gate.entered.recv_timeout(WAIT).unwrap();
    worker.close_with_requests(None, Some(request(2, b"late"))).continue_value().unwrap();
    assert!(!old_receipt.valid_for(&original), "invalidate even the matching OLD pair before source moves");
    let current = worker.source_release_key();
    assert!(matches!(worker.wait_inputs_released(Duration::ZERO), Err(SearchInputsReleaseError::TimedOut)));
    assert_eq!(worker.shared.state.lock().unwrap().closing[1].as_ref().unwrap().key.generation, 2);
    let ordinary = worker.submit(request(3, b"refused ordinary")).unwrap_err();
    assert_eq!(ordinary.reason, "search worker stopped");
    assert_eq!(ordinary.request.key.generation, 3);
    gate.release.send(()).unwrap();
    let receipt = worker.wait_inputs_released(WAIT).unwrap();
    assert!(receipt.valid_for(&current));
    assert!(!receipt.valid_for(&original));
    // Both slots must be returned atomically, even though only external is full.
    // Reusing a stale search generation cannot create capacity after a drain.
    let duplicate = worker
        .close_with_requests(Some(request(4, b"internal")), Some(request(2, b"duplicate")))
        .break_value()
        .unwrap();
    assert_eq!(duplicate.reason, "duplicate search close request");
    assert_eq!(duplicate.internal.as_ref().unwrap().key.generation, 4);
    assert_eq!(duplicate.external.as_ref().unwrap().key.generation, 2);
    assert!(receipt.valid_for(&current), "refusal cannot invalidate an acknowledged generation");
    let retirement = SearchWorker::new(|| {}).unwrap();
    retirement.close_with_requests(duplicate.internal, duplicate.external).continue_value().unwrap();
    retirement.shutdown_bounded(WAIT).unwrap();
    let retirement = SearchWorker::new(|| {}).unwrap();
    retirement.close_with(Some(*ordinary.request)).continue_value().unwrap();
    retirement.shutdown_bounded(WAIT).unwrap();
    drop(spool);
    worker.shutdown_bounded(WAIT).unwrap();
}

#[test]
fn receipts_reject_foreign_workers_and_weak_keys_do_not_keep_a_worker_alive() {
    let worker = SearchWorker::new(|| {}).unwrap();
    let foreign = SearchWorker::new(|| {}).unwrap();
    let key = worker.source_release_key();
    let foreign_key = foreign.source_release_key();
    assert!(matches!(worker.wait_inputs_released(Duration::ZERO), Err(SearchInputsReleaseError::NotStopped)));
    worker.stop();
    foreign.stop();
    let receipt = worker.wait_inputs_released(WAIT).unwrap();
    let foreign_receipt = foreign.wait_inputs_released(WAIT).unwrap();
    assert!(receipt.valid_for(&key));
    assert!(!receipt.valid_for(&foreign_key));
    assert!(!foreign_receipt.valid_for(&key));
    // Actual join is used only in this fixture to make Weak liveness exact.
    worker.shutdown().unwrap();
    assert!(!receipt.valid_for(&key));
    foreign.shutdown().unwrap();
}

#[test]
fn closing_input_caps_refuse_both_exact_owners_before_any_admission() {
    let worker = SearchWorker::new(|| {}).unwrap();
    let mut external = request(2, b"external");
    let SearchTask::Document(doc) = &mut external.task else { unreachable!() };
    doc.ranges = vec![0..1; REPLACE_EDITS + 1];
    let refused = worker.close_with_requests(Some(request(1, b"internal")), Some(external)).break_value().unwrap();
    assert_eq!(refused.reason, "search request exceeds input/page/range cap");
    assert_eq!(refused.internal.as_ref().unwrap().key.generation, 1);
    assert_eq!(refused.external.as_ref().unwrap().key.generation, 2);
    assert!(!worker.shared.stopped.load(Ordering::Acquire));
    assert!(worker.shared.state.lock().unwrap().closing.iter().all(Option::is_none));
    let mut external = refused.external.unwrap();
    let SearchTask::Document(doc) = &mut external.task else { unreachable!() };
    assert_eq!(doc.snapshot.len(), 8);
    doc.ranges.truncate(1); // Keep the exact source owner, repair only fixture metadata.
    worker.close_with_requests(refused.internal, Some(external)).continue_value().unwrap();
    worker.shutdown_bounded(WAIT).unwrap();
}

#[test]
fn input_timeout_preserves_exact_worker_until_the_real_release_event() {
    let (worker, wakes, gate) = gated_worker();
    gate.armed.store(true, Ordering::Release);
    submit_on_wake(&worker, request(1, b"held"), &wakes);
    gate.entered.recv_timeout(WAIT).unwrap();
    let key = worker.source_release_key();
    worker.stop();
    assert!(matches!(worker.wait_inputs_released(Duration::from_millis(10)), Err(SearchInputsReleaseError::TimedOut)));
    assert!(key.identity.ptr_eq(&Arc::downgrade(&worker.shared.source_identity)));
    assert!(!worker.shared.completed.load(Ordering::Acquire));
    gate.release.send(()).unwrap();
    let receipt = worker.wait_inputs_released(WAIT).unwrap();
    assert!(receipt.valid_for(&key), "retry awaits the same worker and real source-release event");
    worker.shutdown_bounded(WAIT).unwrap();
}

#[test]
fn panicking_wake_completion_is_not_proof_while_shared_pending_still_owns_a_snapshot() {
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let (wake_tx, wakes) = mpsc::channel();
    let worker = SearchWorker::new(move || {
        let _ = wake_tx.send(());
        entered_tx.send(()).unwrap();
        release_rx.lock().unwrap().recv_timeout(WAIT).unwrap();
        panic!("source-release wake failure control");
    })
    .unwrap();
    submit_on_wake(&worker, request(1, b"active"), &wakes);
    entered_rx.recv_timeout(WAIT).unwrap();
    worker.submit(request(2, b"still pending")).unwrap();
    let key = worker.source_release_key();
    worker.stop();
    release_tx.send(()).unwrap();
    assert!(matches!(worker.wait_inputs_released(WAIT), Err(SearchInputsReleaseError::WorkerPanicked)));
    assert_eq!(worker.shared.state.lock().unwrap().pending.as_ref().unwrap().request.key.generation, 2);
    assert_eq!(worker.shared.source_identity.released.load(Ordering::Acquire), 0);
    assert_eq!(key.generation, 1);
    let retained = worker.shutdown_bounded(WAIT).unwrap_err().into_worker();
    assert!(matches!(retained.wait_inputs_released(Duration::ZERO), Err(SearchInputsReleaseError::WorkerPanicked)));
    // Explicit test cleanup is OFF the simulated input thread. Completion Drop
    // must never be mistaken for a source ACK that permits UI-side teardown.
    let cleanup = thread::Builder::new()
        .name("kwil-search-test-cleanup".into())
        .spawn(move || {
            assert!(retained.shutdown().is_err());
        })
        .unwrap();
    cleanup.join().unwrap();
}
