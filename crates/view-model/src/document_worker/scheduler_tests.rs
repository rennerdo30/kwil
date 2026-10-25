//! Deterministic journal-lineage ordering controls. ROOT executes these tests.
use super::*;
use crate::{
    Editor,
    hex_session::{EditRequest, Mutation},
};
use std::{collections::VecDeque, time::Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Operation {
    Seed(u64, u64),
    Receipt(u64, u64),
    Finish(u64),
}

fn operation(request: &DocumentRequest) -> Option<Operation> {
    match request {
        DocumentRequest::SeedJournal { generation, next_sequence, .. } => {
            Some(Operation::Seed(*generation, *next_sequence))
        }
        DocumentRequest::Journal { generation, receipt, .. } => Some(Operation::Receipt(*generation, receipt.sequence)),
        DocumentRequest::FinishJournal { generation, .. } => Some(Operation::Finish(*generation)),
        _ => None,
    }
}

#[derive(Debug)]
enum Event {
    Held { actual: mpsc::Receiver<io::Result<u64>>, release: SyncSender<io::Result<u64>> },
    Arrived { operation: Operation, pending: [Option<Operation>; 8] },
    Applying(Operation),
}

pub(super) struct Control {
    document: DocumentId,
    held: bool,
    events: SyncSender<Event>,
}

impl Control {
    pub(super) fn received(
        &mut self,
        request: &DocumentRequest,
        deferred: &VecDeque<DocumentRequest>,
        _storage: Option<&Storage>,
    ) {
        if journal_document(request) != Some(self.document) {
            return;
        }
        let Some(op @ (Operation::Seed(3, _) | Operation::Receipt(3, 4))) = operation(request) else {
            return;
        };
        let mut pending = [None; 8];
        for (slot, request) in
            pending.iter_mut().zip(deferred.iter().filter(|request| journal_document(request) == Some(self.document)))
        {
            *slot = operation(request);
        }
        self.events.try_send(Event::Arrived { operation: op, pending }).expect("bounded schedule observations");
    }

    pub(super) fn applying(&mut self, request: &DocumentRequest) {
        if journal_document(request) == Some(self.document)
            && let Some(operation) = operation(request)
        {
            self.events.try_send(Event::Applying(operation)).expect("bounded schedule observations");
        }
    }

    pub(super) fn hold_ready(&mut self, document: DocumentId, generation: u64, storage: Option<&mut Storage>) {
        if self.held || document != self.document || generation != 2 {
            return;
        }
        let journal = storage.unwrap().journals.get_mut(&document).expect("initial real journal seed");
        assert!(!journal.ready);
        let (release, held) = mpsc::sync_channel(1);
        let actual = std::mem::replace(&mut journal.handle.ready, held);
        self.held = true;
        self.events.try_send(Event::Held { actual, release }).expect("bounded readiness observation");
    }
}

fn seed(document: DocumentId, generation: u64, next_sequence: u64, editor: &Editor) -> DocumentRequest {
    DocumentRequest::SeedJournal {
        document,
        revision: editor.document().revision(),
        snapshot: editor.document().snapshot(),
        path: editor.document().path().map(PathBuf::from),
        next_sequence,
        generation,
        policy: document_policy(editor.document()),
    }
}

#[test]
fn journal_scheduler_predecessors_block_new_seed_but_successors_cannot_block_finish() {
    let id = DocumentId(7);
    let mut editor = Editor::new(Document::new());
    let (tx, rx) = mpsc::sync_channel(4);
    editor
        .set_edit_observer(Some(Box::new(move |receipt| {
            tx.try_send(receipt).map_err(|error| match error {
                TrySendError::Full(receipt) | TrySendError::Disconnected(receipt) => receipt,
            })
        })))
        .unwrap();
    editor.replace_byte_range(0..0, b"x").unwrap();
    editor.undo().unwrap();
    editor.redo().unwrap();
    let policy = document_policy(editor.document());
    let mut pending = VecDeque::new();
    for _ in 0..3 {
        pending.push_back(DocumentRequest::Journal {
            document: id,
            receipt: rx.try_recv().unwrap(),
            path: None,
            first_sequence: 1,
            generation: 2,
            policy,
        });
    }
    pending.push_back(DocumentRequest::FinishJournal { document: id, generation: 2 });
    let newer = seed(id, 3, 4, &editor);
    assert!(!journal_can_run(&newer, &pending, pending.len(), None), "new seed cannot overtake retained receipts");
    assert!(
        journal_can_run(&seed(DocumentId(8), 3, 1, &editor), &pending, pending.len(), None),
        "other documents progress"
    );
    pending.push_back(newer);
    editor.undo().unwrap();
    pending.push_back(DocumentRequest::Journal {
        document: id,
        receipt: rx.try_recv().unwrap(),
        path: None,
        first_sequence: 4,
        generation: 3,
        policy,
    });
    let expected = [
        Operation::Receipt(2, 1),
        Operation::Receipt(2, 2),
        Operation::Receipt(2, 3),
        Operation::Finish(2),
        Operation::Seed(3, 4),
        Operation::Receipt(3, 4),
    ];
    for expected in expected {
        let runnable: Vec<_> = pending
            .iter()
            .enumerate()
            .filter_map(|(index, request)| journal_can_run(request, &pending, index, None).then_some(index))
            .collect();
        assert_eq!(runnable, [0], "exactly the document head is eligible, including Finish before younger receipts");
        assert_eq!(operation(&pending.pop_front().unwrap()), Some(expected));
    }
}

#[test]
fn journal_scheduler_control_capacity_retains_requests_and_refunds_failed_sends() {
    use std::sync::atomic::Ordering;
    let (worker, receiver) = DocumentWorker::replacement_test_channel(1);
    worker.journal_controls.finishes.store(1, Ordering::Release);
    assert!(worker.try_submit(DocumentRequest::FinishJournal { document: DocumentId(7), generation: 2 }).is_ok());
    assert_eq!(worker.journal_controls.finishes.load(Ordering::Acquire), 0);
    let rejected = worker.try_submit(DocumentRequest::FinishJournal { document: DocumentId(8), generation: 3 });
    assert!(
        matches!(rejected, Err(TrySendError::Full(request)) if matches!(*request, DocumentRequest::FinishJournal { document: DocumentId(8), generation: 3 }))
    );
    assert!(matches!(
        receiver.try_recv().unwrap(),
        DocumentRequest::FinishJournal { document: DocumentId(7), generation: 2 }
    ));

    let (worker, receiver) = DocumentWorker::replacement_test_channel(0);
    assert!(matches!(
        worker.try_submit(DocumentRequest::FinishJournal { document: DocumentId(7), generation: 2 }),
        Err(TrySendError::Full(_))
    ));
    assert_eq!(worker.journal_controls.finishes.load(Ordering::Acquire), JOURNAL_FINISH_CAPACITY);
    drop(receiver);
    assert!(matches!(
        worker.try_submit(DocumentRequest::FinishJournal { document: DocumentId(7), generation: 2 }),
        Err(TrySendError::Disconnected(_))
    ));
    assert_eq!(worker.journal_controls.finishes.load(Ordering::Acquire), JOURNAL_FINISH_CAPACITY);
    let (worker, receiver) = DocumentWorker::replacement_test_channel(1);
    worker.journal_controls.seeds.store(0, Ordering::Release);
    assert!(matches!(
        worker.try_submit(seed(DocumentId(7), 3, 1, &Editor::new(Document::new()))),
        Err(TrySendError::Full(_))
    ));
    assert!(
        worker.try_submit(DocumentRequest::FinishJournal { document: DocumentId(7), generation: 2 }).is_ok(),
        "seed saturation cannot consume close admission needed to free a journal slot"
    );
    drop(receiver);

    // Observer sender clones retain their separate 16 receipt credits even
    // when every control credit is held by deferred work.
    let (worker, receiver) = DocumentWorker::replacement_test_channel(1);
    worker.journal_controls.seeds.store(0, Ordering::Release);
    worker.journal_controls.finishes.store(0, Ordering::Release);
    let mut editor = Editor::new(Document::new());
    let policy = document_policy(editor.document());
    editor.set_edit_observer(Some(worker.observer(DocumentId(7), None, 1, 2, policy))).unwrap();
    editor.replace_byte_range(0..0, b"x").unwrap();
    assert!(matches!(receiver.try_recv().unwrap(), DocumentRequest::Journal { document: DocumentId(7), .. }));
    assert_eq!(worker.journal_capacity.load(Ordering::Acquire), 15);
    assert_eq!(worker.journal_controls.seeds.load(Ordering::Acquire), 0);
    assert_eq!(worker.journal_controls.finishes.load(Ordering::Acquire), 0);
}

#[test]
fn journal_scheduler_terminal_deferred_disposal_returns_each_control_credit_once() {
    use std::sync::atomic::Ordering;
    let controls = JournalControlCredits::new();
    let editor = Editor::new(Document::new());
    let mut deferred = VecDeque::from([
        seed(DocumentId(7), 2, 1, &editor),
        DocumentRequest::FinishJournal { document: DocumentId(7), generation: 2 },
        seed(DocumentId(7), 3, 1, &editor),
    ]);
    for request in &deferred {
        controls.for_request(request).unwrap().fetch_sub(1, Ordering::AcqRel);
    }
    // Same terminal release operation used by the production shutdown drain.
    for request in deferred.drain(..) {
        controls.release(&request);
    }
    assert!(deferred.is_empty());
    assert_eq!(controls.seeds.load(Ordering::Acquire), JOURNAL_SEED_CAPACITY);
    assert_eq!(controls.finishes.load(Ordering::Acquire), JOURNAL_FINISH_CAPACITY);
}

fn remaining(deadline: Instant) -> Duration {
    deadline.checked_duration_since(Instant::now()).expect("unchanged 10s worker schedule deadline")
}

fn next_result(receiver: &mpsc::Receiver<DocumentResult>, deadline: Instant) -> DocumentResult {
    let result = receiver.recv_timeout(remaining(deadline)).expect("bounded worker completion");
    match &result {
        DocumentResult::JournalQueued { result, .. }
        | DocumentResult::JournalDurable { result, .. }
        | DocumentResult::JournalClosed { result, .. } => {
            assert!(result.is_ok(), "retained receipt/lineage failure: {result:?}")
        }
        _ => {}
    }
    result
}

fn edit(
    worker: &DocumentWorker,
    receiver: &mpsc::Receiver<DocumentResult>,
    editor: Box<Editor>,
    operation: u64,
    mutation: Mutation,
    deadline: Instant,
) -> Box<Editor> {
    let request = EditRequest { source: editor.document().hex_source(7), operation, input_epoch: 1, mutation };
    assert!(worker.try_submit(DocumentRequest::HexEdit { document: DocumentId(7), editor, request }).is_ok());
    loop {
        if let DocumentResult::HexEdited { editor, completion, .. } = next_result(receiver, deadline) {
            completion.result.unwrap();
            return editor;
        }
    }
}

#[test]
fn journal_scheduler_held_ready_preserves_cached_raw_lineage_and_undo_receipts() {
    let fixture = super::tests::Scratch::new();
    let raw = b"\x1f\x8b\x08\0\xff\xfeA\0 \t\r\n";
    let path = fixture.0.join("physical.bin");
    std::fs::write(&path, raw).unwrap();
    let (events_tx, events) = mpsc::sync_channel(16);
    let (results_tx, results) = mpsc::sync_channel(32);
    let control = Control { document: DocumentId(7), held: false, events: events_tx };
    let worker = DocumentWorker::spawn_inner(move |result| results_tx.send(result).unwrap(), Some(control)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    assert!(worker.try_submit(DocumentRequest::Prepare { root: Some(fixture.0.clone()) }).is_ok());
    loop {
        if let DocumentResult::Prepared(result) = next_result(&results, deadline) {
            drop(result.unwrap());
            break;
        }
    }
    assert!(
        worker
            .try_submit(DocumentRequest::OpenRaw {
                document: DocumentId(7),
                revision: 0,
                path: path.clone(),
                rows: 1,
                generation: 1
            })
            .is_ok()
    );
    let mut editor = loop {
        if let DocumentResult::Indexed { result, .. } = next_result(&results, deadline) {
            break Box::new(Editor::new(*result.unwrap()));
        }
    };
    let epoch = editor.document().hex_source(7).epoch;
    let policy = document_policy(editor.document());
    editor.set_edit_observer(Some(worker.observer(DocumentId(7), Some(path.clone()), 1, 2, policy))).unwrap();
    assert!(worker.try_submit(seed(DocumentId(7), 2, 1, &editor)).is_ok());
    let mut applied = Vec::new();
    let (actual, release) = loop {
        match events.recv_timeout(remaining(deadline)).unwrap() {
            Event::Applying(operation) => applied.push(operation),
            Event::Held { actual, release } => break (actual, release),
            _ => panic!("initial readiness must be captured before reopen"),
        }
    };
    editor = edit(&worker, &results, editor, 1, Mutation::Replace { range: 0..1, bytes: vec![0x80] }, deadline);
    editor = edit(&worker, &results, editor, 2, Mutation::Undo, deadline);
    editor = edit(&worker, &results, editor, 3, Mutation::Redo, deadline);
    // A held journal cannot stall another document's actual physical open.
    assert!(
        worker
            .try_submit(DocumentRequest::OpenRaw {
                document: DocumentId(8),
                revision: 0,
                path: path.clone(),
                rows: 1,
                generation: 1
            })
            .is_ok()
    );
    loop {
        if let DocumentResult::Indexed { document: DocumentId(8), result, .. } = next_result(&results, deadline) {
            assert!(
                worker
                    .try_submit(DocumentRequest::DiscardHex {
                        document: DocumentId(8),
                        editor: Box::new(Editor::new(*result.unwrap()))
                    })
                    .is_ok()
            );
            break;
        }
    }
    let saved = fixture.0.join("saved.bin");
    editor.settings.editor.trim_trailing_whitespace_on_save = true;
    let request = editor.raw_save_request(Some(saved.clone())).unwrap();
    assert!(worker.try_submit(DocumentRequest::SaveRaw { document: DocumentId(7), request }).is_ok());
    loop {
        if let DocumentResult::Saved { result, .. } = next_result(&results, deadline) {
            editor.finish_save_for_test(result).unwrap();
            break;
        }
    }
    let mut expected = raw.to_vec();
    expected[0] = 0x80;
    assert_eq!(std::fs::read(&saved).unwrap(), expected);
    assert_eq!(std::fs::read(&path).unwrap(), raw);
    assert!(!editor.document().is_dirty());
    assert!(worker.try_submit(DocumentRequest::FinishJournal { document: DocumentId(7), generation: 2 }).is_ok());
    assert!(
        worker
            .try_submit(DocumentRequest::ValidateReopen {
                document: DocumentId(7),
                generation: 9,
                path: saved.clone(),
                original: editor.document().original.clone(),
                cached: true
            })
            .is_ok()
    );
    loop {
        if let DocumentResult::ReopenValidated { result, .. } = next_result(&results, deadline) {
            assert!(result.unwrap());
            break;
        }
    }
    assert_eq!(editor.next_edit_sequence(), 4);
    editor.set_edit_observer(Some(worker.observer(DocumentId(7), Some(saved.clone()), 4, 3, policy))).unwrap();
    assert!(worker.try_submit(seed(DocumentId(7), 3, 4, &editor)).is_ok());
    assert!(worker.try_submit(DocumentRequest::ReleaseReopen { document: DocumentId(7), generation: 9 }).is_ok());
    editor = edit(&worker, &results, editor, 4, Mutation::Undo, deadline);
    let mut arrivals = 0;
    while arrivals < 2 {
        match events.recv_timeout(remaining(deadline)).unwrap() {
            Event::Applying(operation) => applied.push(operation),
            Event::Arrived { operation: Operation::Seed(3, 4), pending } => {
                assert_eq!(
                    pending[..4],
                    [
                        Some(Operation::Receipt(2, 1)),
                        Some(Operation::Receipt(2, 2)),
                        Some(Operation::Receipt(2, 3)),
                        Some(Operation::Finish(2))
                    ],
                    "force old receipts/Finish overlap before newer seed application"
                );
                arrivals += 1;
            }
            Event::Arrived { operation: Operation::Receipt(3, 4), .. } => arrivals += 1,
            other => panic!("unexpected schedule event: {other:?}"),
        }
    }
    // The real seed has completed, but the document lane has not seen ready.
    // Release only after BOTH successor commands arrived, never after applying
    // Seed3: the fixed worker intentionally cannot apply it before this release.
    release.send(actual.recv_timeout(remaining(deadline)).unwrap()).unwrap();
    let mut queued = Vec::new();
    while queued.len() < 4 {
        if let DocumentResult::JournalQueued { sequence, .. } = next_result(&results, deadline) {
            queued.push(sequence);
        }
    }
    assert_eq!(queued, [1, 2, 3, 4]);
    let expected_operations = [
        Operation::Seed(2, 1),
        Operation::Receipt(2, 1),
        Operation::Receipt(2, 2),
        Operation::Receipt(2, 3),
        Operation::Finish(2),
        Operation::Seed(3, 4),
        Operation::Receipt(3, 4),
    ];
    while applied.len() < expected_operations.len() {
        match events.recv_timeout(remaining(deadline)).unwrap() {
            Event::Applying(operation) => applied.push(operation),
            other => panic!("unexpected schedule event: {other:?}"),
        }
    }
    assert_eq!(applied, expected_operations);
    assert!(worker.try_submit(DocumentRequest::FlushJournals).is_ok());
    loop {
        if let DocumentResult::JournalDurable { revision, sequence: 4, .. } = next_result(&results, deadline) {
            assert_eq!(revision, editor.document().revision());
            break;
        }
    }
    assert_eq!(
        worker.journal_controls.seeds.load(std::sync::atomic::Ordering::Acquire),
        JOURNAL_SEED_CAPACITY,
        "all applied seed ownership credits return"
    );
    assert_eq!(
        worker.journal_controls.finishes.load(std::sync::atomic::Ordering::Acquire),
        JOURNAL_FINISH_CAPACITY,
        "all applied finish ownership credits return"
    );
    assert_eq!(editor.document().hex_source(7).epoch, epoch);
    let mut undone = Vec::new();
    editor.document().snapshot().write_to(&mut undone).unwrap();
    assert_eq!(undone, raw, "cached observer replacement preserves actual undo history");
    assert!(worker.try_submit(DocumentRequest::DiscardHex { document: DocumentId(7), editor }).is_ok());
    drop(worker);
    loop {
        if let DocumentResult::Stopped(result) = next_result(&results, deadline) {
            result.unwrap();
            break;
        }
    }
}
