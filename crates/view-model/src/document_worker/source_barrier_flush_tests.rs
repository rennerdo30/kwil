//! SOURCE ONLY: ROOT executes these fixed-target barrier/flush controls.
use super::*;
use crate::visual_rows::{AllocationBudget, MAX_SESSION_BYTES};
use result_transport::{BoundDocumentTicket, RequestSender, Shared};
use std::{cell::RefCell, ops::ControlFlow, sync::Arc, time::Instant};

const LIMIT: Duration = Duration::from_secs(2);
const ID: DocumentId = DocumentId(1);
const REVISION: u64 = 1;
const GENERATION: u64 = 7;

// The real private marker/permit protocol, with an unstarted bounded inbox.
// No synthetic barrier identity or new production constructor is needed.
struct MarkerLane {
    worker: Option<DocumentWorker>,
    bound: BoundDocumentTicket,
    permit: DocumentContinuationPermit,
    shared: Arc<Shared>,
    inbox: mpsc::Receiver<DocumentRequest>,
    budget: AllocationBudget,
}
impl MarkerLane {
    fn new() -> Self {
        let budget = AllocationBudget::new(MAX_SESSION_BYTES);
        let (shared, receiver) = Shared::new(&budget, || Ok(())).unwrap();
        let (sender, inbox) = mpsc::sync_channel(16);
        let worker = DocumentWorker {
            tx: RequestSender::new(sender, Some(&shared)),
            journal_capacity: Arc::new(std::sync::atomic::AtomicUsize::new(16)),
            journal_controls: Arc::new(JournalControlCredits::new()),
            save_destinations: Default::default(),
        };
        let (bound, permit) = receiver.begin_bound(&worker).unwrap();
        Self { worker: Some(worker), bound, permit, shared, inbox, budget }
    }
    fn marker(&self) -> DocumentBarrierRequest {
        let id = self.permit.request_barrier().unwrap();
        let DocumentRequest::SourceBarrier(marker) = self.inbox.try_recv().unwrap() else {
            panic!("private marker stays in its original bounded inbox")
        };
        assert_eq!(marker.id, id);
        marker
    }
    fn finish(self) {
        assert!(self.budget.peak_bytes() <= MAX_SESSION_BYTES);
        self.worker.as_ref().unwrap().tx.close_bounded();
        drop(self);
    }
}

fn seeded_storage(root: &std::path::Path, deadline: Instant) -> (PreparedWorkspace, Storage) {
    let (prepared, mut storage) = prepare_root(root.to_owned()).unwrap();
    // A real, already seeded empty journal at the same revision/sequence as the
    // observed GUI tail. Its flush receiver is the only controlled test effect.
    let handle = storage
        .service
        .try_create(kwil_core_workspace::journal::JournalSeed {
            path: root.join("1.kwj"),
            base: JournalBase {
                document: ID,
                revision: REVISION,
                original_path: None,
                original_identity: [0; 4],
                codec: b"raw-v1".to_vec(),
                byte_len: 0,
            },
            source: Box::new(io::empty()),
        })
        .unwrap();
    assert_eq!(
        handle.ready.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap().unwrap(),
        REVISION
    );
    let mut journal = JournalState::new(handle, REVISION, 0, document_policy(&Document::new()));
    journal.ready = true;
    journal.needs_flush = false;
    journal.generation = GENERATION;
    journal.barrier_durable_sequence = Some(0);
    storage.journals.insert(ID, journal);
    (prepared, storage)
}

fn finish_storage(mut storage: Storage, deadline: Instant) {
    storage.service.finish();
    storage.service.finished.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap().unwrap();
    storage.journals.clear();
    storage.finished_journals.clear();
    let retired = storage.encoding.retire();
    assert_eq!(retired.live, 0);
    assert_eq!(retired.cleanup_pending, 0);
    assert!(retired.first_error.is_none());
}

#[test]
fn source_barrier_adequate_outstanding_flush_precedes_marker_without_redundant_successor() {
    for durable in [None, Some(0)] {
        let deadline = Instant::now() + LIMIT;
        let scratch = tests::Scratch::new();
        let (_, mut storage) = seeded_storage(&scratch.0, deadline);
        let lane = MarkerLane::new();
        let (complete, pending) = mpsc::sync_channel(1);
        let journal = storage.journals.get_mut(&ID).unwrap();
        journal.barrier_durable_sequence = durable;
        journal.flush = Some((0, pending));
        let previous_due = journal.flush_due;
        let tracker = SourceBarrierTracker::new(lane.marker(), Some(&mut storage));
        assert!(!tracker.ready(Some(&storage)), "an outstanding completion cannot follow the source barrier");
        assert!(
            !storage.journals[&ID].needs_flush,
            "adequate in-flight target must not schedule a redundant successor"
        );
        assert_eq!(storage.journals[&ID].flush_due, previous_due);
        let completions = RefCell::new(Vec::new());
        // One explicit readiness probe retains the exact outstanding receiver.
        poll_journals(&mut storage, &|result| completions.borrow_mut().push(result));
        assert!(completions.borrow().is_empty());
        assert!(!tracker.ready(Some(&storage)));
        complete.try_send(Ok(REVISION)).unwrap();
        poll_journals(&mut storage, &|result| completions.borrow_mut().push(result));
        let mut completions = completions.into_inner();
        assert_eq!(completions.len(), 1);
        assert!(matches!(
            completions.pop(),
            Some(DocumentResult::JournalDurable { document: ID, revision: REVISION, sequence: 0, result: Ok(()) })
        ));
        assert!(tracker.ready(Some(&storage)));
        assert!(storage.journals[&ID].flush.is_none());
        assert!(!storage.journals[&ID].needs_flush);
        finish_storage(storage, deadline);
        lane.finish();
    }
}

#[test]
fn source_barrier_needed_flush_bypasses_future_debounce_under_original_deadline() {
    let deadline = Instant::now() + LIMIT;
    let scratch = tests::Scratch::new();
    let (_, mut storage) = seeded_storage(&scratch.0, deadline);
    let lane = MarkerLane::new();
    let journal = storage.journals.get_mut(&ID).unwrap();
    journal.needs_flush = true;
    journal.flush_due = deadline + LIMIT;
    let before_marker = Instant::now();
    let tracker = SourceBarrierTracker::new(lane.marker(), Some(&mut storage));
    let after_marker = Instant::now();
    assert!(!tracker.ready(Some(&storage)), "a durable target does not cancel an existing flush obligation");
    assert!(storage.journals[&ID].flush_due >= before_marker);
    assert!(storage.journals[&ID].flush_due <= after_marker);
    let completions = RefCell::new(Vec::new());
    poll_journals(&mut storage, &|result| completions.borrow_mut().push(result));
    assert!(completions.borrow().is_empty());
    let (sequence, actual) =
        storage.journals.get_mut(&ID).unwrap().flush.take().expect("marker triggered the existing real flush");
    assert_eq!(sequence, 0);
    let outcome = actual.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
    assert_eq!(*outcome.as_ref().unwrap(), REVISION);
    // Return the actual outcome once at the synchronous readiness seam. No
    // successful durability value is invented, and no polling loop is added.
    let (deliver, ready) = mpsc::sync_channel(1);
    deliver.try_send(outcome).unwrap();
    storage.journals.get_mut(&ID).unwrap().flush = Some((sequence, ready));
    assert!(!tracker.ready(Some(&storage)));
    poll_journals(&mut storage, &|result| completions.borrow_mut().push(result));
    assert_eq!(completions.borrow().len(), 1);
    assert!(tracker.ready(Some(&storage)));
    assert!(storage.journals[&ID].flush.is_none());
    assert!(!storage.journals[&ID].needs_flush);
    finish_storage(storage, deadline);
    lane.finish();
}

#[test]
fn source_barrier_postmarker_receipt_advances_existing_need_without_expanding_fixed_target() {
    let deadline = Instant::now() + LIMIT;
    let scratch = tests::Scratch::new();
    let (_, mut storage) = seeded_storage(&scratch.0, deadline);
    let lane = MarkerLane::new();
    let tracker = SourceBarrierTracker::new(lane.marker(), Some(&mut storage));
    assert!(tracker.ready(Some(&storage)));
    let target = tracker.targets.iter().flatten().find(|target| target.document == ID).unwrap();
    let captured = (target.document, target.generation, target.sequence);
    assert_eq!(captured, (ID, GENERATION, 0));

    // The real younger receipt starts at the already seeded empty revision.
    // Its append sets needs_flush but retains the previous two-second due.
    let mut document = Document::new();
    document.revision = REVISION;
    document.saved_revision = REVISION;
    document.next_revision = REVISION + 1;
    let policy = document_policy(&document);
    let mut editor = crate::Editor::new(document);
    let (send, receive) = mpsc::sync_channel(1);
    editor
        .set_edit_observer(Some(Box::new(move |receipt| {
            send.try_send(receipt).map_err(|error| match error {
                TrySendError::Full(receipt) | TrySendError::Disconnected(receipt) => receipt,
            })
        })))
        .unwrap();
    editor.insert_text("x", Duration::ZERO).unwrap();
    let receipt = receive.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
    let revision = receipt.after_revision;
    let sequence = receipt.sequence;
    assert_eq!(receipt.before_revision, REVISION);
    assert_eq!(sequence, 1);
    let future_due = Instant::now() + LIMIT;
    storage.journals.get_mut(&ID).unwrap().flush_due = future_due;
    journal_receipt(&mut storage, ID, receipt, None, 1, GENERATION, policy).unwrap();
    assert!(storage.journals[&ID].needs_flush);
    assert_eq!(storage.journals[&ID].flush_due, future_due);
    assert!(storage.journals[&ID].flush.is_none());
    assert!(!tracker.ready(Some(&storage)), "the original marker still drains the new existing need");

    tracker.expedite_pending_flush(DocumentId(ID.0 + 1), Some(&mut storage));
    assert_eq!(storage.journals[&ID].flush_due, future_due);
    storage.journals.get_mut(&ID).unwrap().generation = GENERATION + 1;
    tracker.expedite_pending_flush(ID, Some(&mut storage));
    assert_eq!(storage.journals[&ID].flush_due, future_due, "a younger lineage cannot inherit this trigger");
    storage.journals.get_mut(&ID).unwrap().generation = GENERATION;
    storage.journals.get_mut(&ID).unwrap().failed = true;
    tracker.expedite_pending_flush(ID, Some(&mut storage));
    assert_eq!(storage.journals[&ID].flush_due, future_due, "a failed receipt is no new flush authority");
    storage.journals.get_mut(&ID).unwrap().failed = false;

    let before_trigger = Instant::now();
    tracker.expedite_pending_flush(ID, Some(&mut storage));
    let after_trigger = Instant::now();
    let journal = &storage.journals[&ID];
    assert!(journal.flush_due >= before_trigger && journal.flush_due <= after_trigger);
    assert!(journal.needs_flush && journal.flush.is_none());
    assert_eq!((journal.generation, journal.sequence, journal.revision), (GENERATION, sequence, revision));
    assert_eq!(journal.barrier_durable_sequence, Some(0));
    let target = tracker.targets.iter().flatten().find(|target| target.document == ID).unwrap();
    assert_eq!((target.document, target.generation, target.sequence), captured);

    let completions = RefCell::new(Vec::new());
    poll_journals(&mut storage, &|result| completions.borrow_mut().push(result));
    assert!(completions.borrow().is_empty());
    let journal = &storage.journals[&ID];
    assert!(!journal.needs_flush);
    let (flushing, receiver) = journal.flush.as_ref().expect("existing poll starts the actual younger flush");
    assert_eq!(*flushing, sequence);
    let receiver_identity = receiver as *const mpsc::Receiver<io::Result<u64>>;
    let flush_due = journal.flush_due;
    tracker.expedite_pending_flush(ID, Some(&mut storage));
    let journal = &storage.journals[&ID];
    assert_eq!(journal.flush_due, flush_due);
    assert!(!journal.needs_flush, "an adequate outstanding flush cannot acquire a redundant successor");
    assert_eq!(&journal.flush.as_ref().unwrap().1 as *const _, receiver_identity);
    assert!(!tracker.ready(Some(&storage)));
    let (flushing, actual) = storage.journals.get_mut(&ID).unwrap().flush.take().unwrap();
    let outcome = actual.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
    assert_eq!(*outcome.as_ref().unwrap(), revision);
    let (deliver, ready) = mpsc::sync_channel(1);
    deliver.try_send(outcome).unwrap();
    storage.journals.get_mut(&ID).unwrap().flush = Some((flushing, ready));
    poll_journals(&mut storage, &|result| completions.borrow_mut().push(result));
    let mut completions = completions.into_inner();
    assert_eq!(completions.len(), 1);
    assert!(matches!(completions.pop(), Some(DocumentResult::JournalDurable {
        document: ID, revision: actual_revision, sequence: actual_sequence, result: Ok(())
    }) if actual_revision == revision && actual_sequence == sequence));
    assert!(tracker.ready(Some(&storage)));
    assert!(!storage.journals[&ID].needs_flush && storage.journals[&ID].flush.is_none());
    let target = tracker.targets.iter().flatten().find(|target| target.document == ID).unwrap();
    assert_eq!((target.document, target.generation, target.sequence), captured);
    drop(editor);
    finish_storage(storage, deadline);
    lane.finish();
}

#[derive(Debug)]
struct FlushFailure(u64);
impl std::fmt::Display for FlushFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "original held flush failure {}", self.0)
    }
}
impl std::error::Error for FlushFailure {}

#[test]
fn source_barrier_flush_failure_is_consumed_by_private_drain_before_source_release() {
    let deadline = Instant::now() + LIMIT;
    let scratch = tests::Scratch::new();
    let (prepared, mut storage) = seeded_storage(&scratch.0, deadline);
    let mut lane = MarkerLane::new();
    let (complete, pending) = mpsc::sync_channel(1);
    storage.journals.get_mut(&ID).unwrap().flush = Some((0, pending));
    let tracker = SourceBarrierTracker::new(lane.marker(), Some(&mut storage));
    assert!(!tracker.ready(Some(&storage)));
    let error = io::Error::other(FlushFailure(63));
    let identity = error.get_ref().unwrap().downcast_ref::<FlushFailure>().unwrap() as *const FlushFailure;
    complete.try_send(Err(error)).unwrap();
    let completion = RefCell::new(None);
    poll_journals(&mut storage, &|result| {
        assert!(completion.borrow().is_none(), "one original typed failure receipt");
        *completion.borrow_mut() = Some(result);
    });
    let completion = completion.into_inner().unwrap();
    let DocumentResult::JournalDurable { document, revision, sequence, result: Err(error) } = &completion else {
        panic!("the original flush error must be published before any marker receipt")
    };
    assert_eq!((*document, *revision, *sequence), (ID, REVISION, 0));
    assert_eq!(error.get_ref().unwrap().downcast_ref::<FlushFailure>().unwrap() as *const FlushFailure, identity);
    assert!(storage.journals[&ID].failed);

    let mut workspace = crate::Workspace::new(crate::View::new(None)).unwrap();
    assert!(matches!(workspace.document_result(DocumentResult::Prepared(Ok(prepared))), ControlFlow::Continue(_)));
    assert_eq!(workspace.active_id(), ID);
    assert!(workspace.recovery_notice().is_none());
    let source_epoch = workspace.editor().unwrap().document().source_epoch;
    assert!(workspace.replace_document_worker_for_transport_test(lane.worker.take()).is_none());
    // The one test publisher uses the actual guarded slot. Accept through the
    // original private Drain permit; no whole result reaches the consumer.
    let publisher = lane.shared.clone();
    std::thread::scope(|scope| {
        let producer = scope.spawn(move || publisher.publish(completion));
        lane.bound.wait_until(deadline).unwrap();
        let info = lane.bound.info().unwrap();
        assert_eq!(info.kind, crate::workspace_document::ResultKind::Journal);
        let update = lane.bound.accept_bound(&mut workspace, &lane.budget, &lane.permit, deadline).unwrap().unwrap();
        drop(update);
        producer.join().unwrap();
    });
    assert_eq!(workspace.recovery_notice(), Some("Recovery unavailable"));
    assert_eq!(
        workspace.editor().unwrap().document().source_epoch,
        source_epoch,
        "failure cannot release the original source"
    );
    assert!(tracker.ready(Some(&storage)), "the existing failed-journal branch stays unchanged");
    assert!(
        lane.bound.inspect().retained_commit_error().is_none(),
        "the typed journal failure was handled, not a commit refusal"
    );
    lane.worker = workspace.replace_document_worker_for_transport_test(None);
    assert!(matches!(
        lane.worker.as_ref().unwrap().try_submit(DocumentRequest::SessionTimer),
        Err(TrySendError::Disconnected(_))
    ));
    finish_storage(storage, deadline);
    lane.finish();
}

#[test]
fn source_barrier_fixed_target_keeps_generation_and_younger_finish_progress() {
    let deadline = Instant::now() + LIMIT;
    let scratch = tests::Scratch::new();
    let (_, mut storage) = seeded_storage(&scratch.0, deadline);
    let lane = MarkerLane::new();
    let tracker = SourceBarrierTracker::new(lane.marker(), Some(&mut storage));
    let captured = tracker.targets.iter().flatten().find(|target| target.document == ID).unwrap();
    assert_eq!((captured.generation, captured.sequence), (GENERATION, 0));
    let journal = storage.journals.get_mut(&ID).unwrap();
    journal.sequence = 99;
    journal.needs_flush = false;
    assert!(tracker.ready(Some(&storage)), "younger receipts must not expand the captured sequence target");
    assert_eq!(tracker.targets.iter().flatten().find(|target| target.document == ID).unwrap().sequence, 0);
    let journal = storage.journals.get_mut(&ID).unwrap();
    journal.generation = GENERATION + 1;
    journal.ready = false;
    journal.needs_flush = true;
    assert!(tracker.ready(Some(&storage)), "a younger lineage cannot hold an older generation's marker");
    let pending = std::collections::VecDeque::from([DocumentRequest::SourceBarrier(tracker.request)]);
    assert!(
        journal_can_run(
            &DocumentRequest::FinishJournal { document: ID, generation: GENERATION + 1 },
            &pending,
            pending.len(),
            Some(&storage)
        ),
        "a younger Finish remains eligible instead of waiting on marker ACK"
    );
    storage.journals.get_mut(&ID).unwrap().generation = GENERATION;
    storage.journals.get_mut(&ID).unwrap().failed = true;
    assert!(tracker.ready(Some(&storage)));
    let retained = storage.journals.remove(&ID).unwrap();
    assert!(tracker.ready(Some(&storage)), "a retired journal keeps the existing missing-target branch");
    storage.journals.insert(ID, retained);
    finish_storage(storage, deadline);
    lane.finish();
}

#[test]
fn source_barrier_sender_control_flow_keeps_original_unboxed_carrier_size() {
    assert_eq!(
        std::mem::size_of::<ControlFlow<TrySendError<DocumentRequest>, ()>>(),
        std::mem::size_of::<Result<(), TrySendError<DocumentRequest>>>(),
    );
}

#[test]
fn journal_finish_reports_only_exact_successful_generation_boundary() {
    let root = tests::Scratch::new();
    let deadline = Instant::now() + LIMIT;
    let (_prepared, mut storage) = seeded_storage(&root.0, deadline);
    assert_eq!(finish_journal(&mut storage, ID, GENERATION - 1).unwrap(), None);
    assert!(storage.journals.contains_key(&ID));
    assert!(storage.finished_journals.is_empty());
    assert_eq!(finish_journal(&mut storage, ID, GENERATION).unwrap(), Some((REVISION, 0)));
    assert!(!storage.journals.contains_key(&ID));
    assert!(storage.finished_journals.contains_key(&ID));
    assert_eq!(finish_journal(&mut storage, ID, GENERATION).unwrap(), None);
    assert_eq!(finish_journal(&mut storage, DocumentId(99), GENERATION).unwrap(), None);
    finish_storage(storage, deadline);
}
