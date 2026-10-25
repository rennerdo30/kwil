//! Physical-byte transport on the existing document lane. Only bounded pages
//! cross to the UI; Editor ownership always returns, including every refusal.
use crate::hex_session::{
    self, EditCompletion, EditFence, EditRequest, OpenOutcome, Page, PageRequest, Representation,
};
use crate::{Document, Editor};
use kwil_core_text::TextFile;
use kwil_core_workspace::session::{self, DocumentId};
use std::{
    io,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
};

struct TabFence {
    document: DocumentId,
    epoch: u64,
    fence: EditFence,
}

pub(super) struct HexRuntime {
    // Closed tabs may retain the same Editor/history. Never evict a live fence
    // to make capacity: otherwise a replay could pass after revision ABA.
    fences: [Option<TabFence>; session::MAX_TABS + session::MAX_CLOSED],
}

impl HexRuntime {
    pub(super) fn new() -> Self {
        Self { fences: std::array::from_fn(|_| None) }
    }

    pub(super) fn release(&mut self, document: DocumentId, epoch: u64) {
        for slot in &mut self.fences {
            if slot.as_ref().is_some_and(|f| f.document == document && f.epoch == epoch) {
                *slot = None;
            }
        }
    }

    pub(super) fn discard(&mut self, document: DocumentId, editor: Box<Editor>) {
        let epoch = editor.document().hex_source(document.0).epoch;
        drop(editor);
        self.release(document, epoch);
    }

    pub(super) fn edit(&mut self, document: DocumentId, editor: &mut Editor, request: &EditRequest) -> EditCompletion {
        let current = editor.document().hex_source(document.0);
        let index = self
            .fences
            .iter()
            .position(|slot| slot.as_ref().is_some_and(|f| f.document == document))
            .or_else(|| self.fences.iter().position(Option::is_none));
        let Some(index) = index else {
            return EditCompletion {
                before: request.source,
                after: current,
                operation: request.operation,
                input_epoch: request.input_epoch,
                result: Err("raw tab replay-fence capacity retained".into()),
            };
        };
        let tab = self.fences[index].get_or_insert_with(|| TabFence {
            document,
            epoch: current.epoch,
            fence: EditFence::default(),
        });
        // A stale request must never reset the fence of the live incarnation.
        if current == request.source {
            tab.epoch = current.epoch;
        }
        hex_session::apply_edit(editor, current, request, &mut tab.fence)
    }
}

pub(super) fn page(
    document: DocumentId,
    editor: &Editor,
    request: PageRequest,
    cancel: &AtomicBool,
) -> io::Result<Page> {
    let current = editor.document().hex_source(document.0);
    if request.source != current || current.representation != Representation::RawBytes {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "stale raw page source"));
    }
    hex_session::read_page(&editor.document().snapshot(), request, || cancel.load(Ordering::Relaxed))
}

/// Publish physical bytes before the file-wide index. The preview owns rows,
/// never a TextFile/Snapshot, and its epoch is the final Document's epoch.
pub(super) fn open(
    document: DocumentId,
    path: PathBuf,
    rows: usize,
    generation: u64,
    cancel: &AtomicBool,
    publish: impl FnOnce(Page),
) -> io::Result<Document> {
    let file = kwil_core_workspace::open_for_editing(&path)?;
    open_file(document, path, file, rows, generation, cancel, publish)
}

pub(super) fn open_file(
    document: DocumentId,
    path: PathBuf,
    file: TextFile,
    rows: usize,
    generation: u64,
    cancel: &AtomicBool,
    publish: impl FnOnce(Page),
) -> io::Result<Document> {
    let epoch = crate::document::next_source_epoch();
    let request = PageRequest {
        source: hex_session::Source {
            document: document.0,
            revision: 0,
            epoch,
            len: file.len(),
            representation: Representation::RawBytes,
        },
        generation,
        top: 0,
        rows,
    };
    let page = hex_session::read_file_page(&file, request, || cancel.load(Ordering::Relaxed))?;
    publish(page);
    if cancel.load(Ordering::Relaxed) {
        return Err(io::ErrorKind::Interrupted.into());
    }
    // A raw viewport never needs this index; the existing shared Buffer/history
    // currently does. Keep construction on this lane, after the first page.
    file.build_index(1).map_err(io::Error::other)?;
    if cancel.load(Ordering::Relaxed) {
        return Err(io::ErrorKind::Interrupted.into());
    }
    Document::from_raw_file(path, file, epoch)
}

pub(super) fn reopen(
    editor: &mut Editor,
    prepare: impl FnOnce(PathBuf) -> io::Result<Document>,
) -> io::Result<OpenOutcome> {
    reopen_transport(editor, Representation::RawBytes, prepare)
}

pub(super) fn reopen_text(
    editor: &mut Editor,
    prepare: impl FnOnce(PathBuf) -> io::Result<Document>,
) -> io::Result<OpenOutcome> {
    reopen_transport(editor, Representation::DecodedText, prepare)
}

fn reopen_transport(
    editor: &mut Editor,
    target: Representation,
    prepare: impl FnOnce(PathBuf) -> io::Result<Document>,
) -> io::Result<OpenOutcome> {
    if editor.document().representation() == target {
        return Ok(OpenOutcome::Ready);
    }
    // This typed refusal precedes every observer flush, read or mutation.
    if editor.document().is_dirty() {
        return Ok(OpenOutcome::NeedsRawReopen);
    }
    editor.raw_reopen_ready()?;
    let original = editor.document().original.as_ref().ok_or_else(|| {
        io::Error::new(io::ErrorKind::Unsupported, "transport reopen requires a verified named original")
    })?;
    if original.check()? != kwil_core_text::FileState::Unchanged {
        return Err(io::Error::other("transport reopen source changed on disk; editor retained"));
    }
    let path = editor
        .document()
        .path()
        .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "transport reopen requires a named source"))?
        .to_path_buf();
    let prepared = prepare(path)?;
    if prepared.representation() != target {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "prepared transport representation mismatch; editor retained",
        ));
    }
    if original.check()? != kwil_core_text::FileState::Unchanged {
        return Err(io::Error::other("transport reopen source changed during preparation; editor retained"));
    }
    editor.install_raw_reopen(prepared);
    Ok(OpenOutcome::Ready)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document_worker::{DocumentRequest, DocumentResult, DocumentWorker};
    use crate::hex_session::{Mutation, Source};
    use std::{
        fs,
        sync::{atomic::AtomicU64, mpsc},
        time::Duration,
    };

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "kwil-raw-transport-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn file(&self, bytes: &[u8]) -> (PathBuf, TextFile) {
            let path = self.0.join("source");
            fs::write(&path, bytes).unwrap();
            let file = TextFile::open(&path).unwrap();
            (path, file)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn bytes(document: &Document) -> Vec<u8> {
        let mut bytes = Vec::new();
        document.snapshot().write_to(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn raw_transport_first_page_precedes_index_and_keeps_container_bom_and_invalid_bytes() {
        for raw in [b"\x1f\x8b\x08\0\xff\r\n \t".as_slice(), b"\xff\xfeA\0\0\xd8 \0\r\0\n\0"] {
            let fixture = Fixture::new();
            let (path, file) = fixture.file(raw);
            assert!(file.index().is_none());
            let mut epoch = 0;
            let document = open_file(DocumentId(3), path, file.clone(), 1, 9, &AtomicBool::new(false), |page| {
                assert!(file.index().is_none(), "physical first page must precede a full index pass");
                assert_eq!(page.rows[0].bytes, raw);
                assert_eq!(page.request.source.representation, Representation::RawBytes);
                epoch = page.request.source.epoch;
            })
            .unwrap();
            assert_eq!(document.hex_source(3).epoch, epoch);
            assert_eq!(bytes(&document), raw);
            assert_eq!(document.representation(), Representation::RawBytes);
        }
    }

    #[test]
    fn raw_transport_dirty_reopen_is_typed_and_leaves_editor_history_and_identity_untouched() {
        let mut editor = Editor::new(Document::new());
        editor.insert_text("dirty", Duration::ZERO).unwrap();
        let before = editor.document().hex_source(5);
        let sequence = editor.next_edit_sequence();
        let cursor = editor.primary_cursor().clone();
        assert_eq!(
            reopen(&mut editor, |_| panic!("no raw preparation on refusal")).unwrap(),
            OpenOutcome::NeedsRawReopen
        );
        assert_eq!(editor.document().hex_source(5), before);
        assert_eq!(editor.next_edit_sequence(), sequence);
        assert_eq!(editor.primary_cursor(), &cursor);
        assert_eq!(bytes(editor.document()), b"dirty");
        editor.undo().unwrap();
        assert_eq!(bytes(editor.document()), b"");
        editor.redo().unwrap();
        assert_eq!(bytes(editor.document()), b"dirty");
    }

    #[test]
    fn raw_transport_page_requires_full_worker_document_identity() {
        let fixture = Fixture::new();
        let (path, file) = fixture.file(b"\0\xffraw");
        let editor = Editor::new(open_file(DocumentId(4), path, file, 1, 0, &AtomicBool::new(false), |_| {}).unwrap());
        let source = editor.document().hex_source(4);
        let request = PageRequest { source, generation: 8, top: 0, rows: 1 };
        let cancel = AtomicBool::new(false);
        assert_eq!(page(DocumentId(4), &editor, request, &cancel).unwrap().rows[0].bytes, b"\0\xffraw");
        for source in [
            Source { epoch: source.epoch + 1, ..source },
            Source { revision: 1, ..source },
            Source { document: 5, ..source },
            Source { len: 3, ..source },
            Source { representation: Representation::DecodedText, ..source },
        ] {
            assert!(page(DocumentId(4), &editor, PageRequest { source, ..request }, &cancel).is_err());
        }
    }

    #[test]
    fn raw_transport_mailbox_refusal_returns_exact_editor_for_full_and_disconnected() {
        for disconnected in [false, true] {
            let (worker, receiver) = DocumentWorker::replacement_test_channel(0);
            let mut receiver = Some(receiver);
            if disconnected {
                receiver.take();
            }
            let editor = Box::new(Editor::new(Document::new()));
            let pointer = &*editor as *const Editor;
            let source = editor.document().hex_source(7);
            let request = EditRequest { source, operation: 1, input_epoch: 1, mutation: Mutation::Undo };
            let rejected =
                match worker.try_submit(DocumentRequest::HexEdit { document: DocumentId(7), editor, request }) {
                    Err(mpsc::TrySendError::Full(request)) if !disconnected => request,
                    Err(mpsc::TrySendError::Disconnected(request)) if disconnected => request,
                    _ => panic!("exact bounded refusal class"),
                };
            let DocumentRequest::HexEdit { editor, request, .. } = *rejected else { panic!("retain typed request") };
            assert_eq!(&*editor as *const Editor, pointer);
            assert_eq!(editor.document().hex_source(7), source);
            assert_eq!(request.source, source);
            let rejected = match worker.try_submit(DocumentRequest::DiscardHex { document: DocumentId(7), editor }) {
                Err(mpsc::TrySendError::Full(request)) if !disconnected => request,
                Err(mpsc::TrySendError::Disconnected(request)) if disconnected => request,
                _ => panic!("exact bounded discard refusal class"),
            };
            let DocumentRequest::DiscardHex { editor, .. } = *rejected else { panic!("retain discard Editor") };
            assert_eq!(&*editor as *const Editor, pointer);
            assert_eq!(editor.document().hex_source(7), source);
        }
    }

    #[test]
    fn raw_transport_discard_drops_editor_roots_and_releases_its_fence_on_worker() {
        use crate::document_worker::retirement::{BackingLimits, WorkerBackings};
        let fixture = Fixture::new();
        let (path, file) = fixture.file(b"\xffraw");
        drop(file);
        let mut backings = WorkerBackings::new(
            fixture.0.clone(),
            BackingLimits { max_backings: 1, max_private_path_bytes: 1024, max_metadata_bytes: 8 << 20 },
        )
        .unwrap();
        let document = backings.open_raw(DocumentId(3), path, 1, 0, &AtomicBool::new(false), |_| {}).unwrap();
        let mut editor = Box::new(Editor::new(document));
        let mut hex = HexRuntime::new();
        let request = EditRequest {
            source: editor.document().hex_source(3),
            operation: 1,
            input_epoch: 1,
            mutation: Mutation::Replace { range: 0..1, bytes: vec![0x80] },
        };
        assert!(hex.edit(DocumentId(3), &mut editor, &request).result.is_ok());
        assert!(hex.fences.iter().any(|slot| slot.as_ref().is_some_and(|f| f.document == DocumentId(3))));
        assert_eq!(backings.retire().live, 1);
        hex.discard(DocumentId(3), editor);
        assert!(hex.fences.iter().all(Option::is_none));
        assert_eq!(backings.retire().retired, 1, "Editor and undo roots dropped on the disposal lane");
    }

    #[test]
    fn raw_transport_source_owner_stays_worker_retained_and_capacity_refuses_before_save() {
        use crate::document_worker::retirement::{BackingLimits, WorkerBackings};
        let fixture = Fixture::new();
        let (path, file) = fixture.file(b"\xff\xfeA\0 \t\r\n");
        drop(file);
        let mut backings = WorkerBackings::new(
            fixture.0.clone(),
            BackingLimits { max_backings: 1, max_private_path_bytes: 1024, max_metadata_bytes: 8 << 20 },
        )
        .unwrap();
        let cancel = AtomicBool::new(false);
        let document = backings.open_raw(DocumentId(3), path.clone(), 1, 0, &cancel, |_| {}).unwrap();
        assert_eq!(backings.retire().live, 1, "do not drop an owner retained by the raw Editor");
        assert_eq!(backings.retained(), 1);
        assert!(
            backings.open_raw(DocumentId(4), path, 1, 0, &cancel, |_| panic!("refuse before a new preview")).is_err()
        );
        let mut editor = Editor::new(document);
        let output = fixture.0.join("refused-output");
        let request = editor.raw_save_request(Some(output.clone())).unwrap();
        let result = backings.save_raw(request, &cancel);
        assert!(result.is_err());
        assert!(!output.exists(), "raw checkpoint capacity must be admitted before atomic output");
        assert_eq!(backings.retained(), 1, "capacity refusal keeps the exact original source owner");
        assert!(editor.finish_save_for_test(result).is_err());
        assert_eq!(bytes(editor.document()), b"\xff\xfeA\0 \t\r\n");
        drop(editor);
        assert_eq!(backings.retire().retired, 1);
        assert_eq!(backings.retained(), 0);
    }

    #[test]
    fn raw_transport_clean_reopen_changes_epoch_and_failed_reopen_keeps_exact_editor() {
        let fixture = Fixture::new();
        let (path, file) = fixture.file(b"\xef\xbb\xbftext");
        let document = Document::from_file(path, file).unwrap();
        let mut editor = Editor::new(document);
        let before = editor.document().hex_source(3);
        assert_eq!(
            reopen(&mut editor, |path| open(DocumentId(3), path, 1, 9, &AtomicBool::new(false), |_| {})).unwrap(),
            OpenOutcome::Ready
        );
        let after = editor.document().hex_source(3);
        assert_ne!(after.epoch, before.epoch);
        assert_eq!(bytes(editor.document()), b"\xef\xbb\xbftext");
        assert_eq!(after.representation, Representation::RawBytes);
        let mut unnamed = Editor::new(Document::new());
        let before = unnamed.document().hex_source(4);
        assert!(reopen(&mut unnamed, |_| panic!("unnamed refusal precedes raw preparation")).is_err());
        assert_eq!(unnamed.document().hex_source(4), before);
    }

    #[test]
    fn raw_transport_lineage_promotion_preserves_old_recovery_and_restarts_receipts_after_each_direction() {
        use crate::document_worker::{self as worker, SeedSnapshot, TransportSeed};
        fn observe(editor: &mut Editor) -> mpsc::Receiver<crate::EditReceipt> {
            let (tx, rx) = mpsc::sync_channel(4);
            editor
                .set_edit_observer(Some(Box::new(move |receipt| {
                    tx.try_send(receipt).map_err(|error| match error {
                        mpsc::TrySendError::Full(receipt) | mpsc::TrySendError::Disconnected(receipt) => receipt,
                    })
                })))
                .unwrap();
            rx
        }
        fn ready(storage: &mut worker::Storage, id: DocumentId) {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !storage.journals[&id].ready {
                assert!(std::time::Instant::now() < deadline, "bounded lineage readiness");
                worker::poll_journals(storage, &|result| {
                    if let DocumentResult::JournalDurable { result: Err(error), .. } = result {
                        panic!("lineage: {error}");
                    }
                });
                std::thread::park_timeout(Duration::from_millis(1));
            }
        }
        let fixture = Fixture::new();
        let (path, file) = fixture.file(b"\xff\xfeA\0\r\0\n\0");
        let root = fixture.0.join("workspace");
        let (prepared, mut storage) = worker::prepare(Some(root.clone())).unwrap();
        drop(prepared);
        let document = storage.encoding.open(path.clone(), file, None, &AtomicBool::new(false), |_| Ok(())).unwrap();
        let mut editor = Editor::new(document);
        let id = DocumentId(17);
        let policy = worker::document_policy(editor.document());
        worker::seed_journal(
            &mut storage,
            id,
            SeedSnapshot { revision: 0, snapshot: editor.document().snapshot() },
            Some(path.clone()),
            0,
            1,
            policy,
        )
        .unwrap();
        ready(&mut storage, id);
        let rx = observe(&mut editor);
        editor.insert_text("x", Duration::ZERO).unwrap();
        worker::journal_receipt(
            &mut storage,
            id,
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Some(path.clone()),
            1,
            1,
            policy,
        )
        .unwrap();
        editor.undo().unwrap();
        worker::journal_receipt(
            &mut storage,
            id,
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Some(path.clone()),
            1,
            1,
            policy,
        )
        .unwrap();
        assert_eq!(storage.order[&id], 2);
        assert!(!editor.document().is_dirty());
        for (generation, target) in [(2, Representation::RawBytes), (3, Representation::DecodedText)] {
            worker::finish_journal(&mut storage, id, generation - 1).unwrap();
            assert!(
                storage.finished_journals[&id].origins.is_some(),
                "retain exact old provenance until new authority"
            );
            let canonical = root.join("17.kwj");
            let staged = root.join("17.kwj.next");
            let previous = fs::read(&canonical).unwrap();
            worker::reserve_transport(&mut storage, id).unwrap();
            let outcome = if target == Representation::RawBytes {
                reopen(&mut editor, |path| {
                    storage.encoding.open_raw(id, path, 1, generation, &AtomicBool::new(false), |_| {})
                })
            } else {
                reopen_text(&mut editor, |path| {
                    let file = kwil_core_workspace::open_for_editing(&path)?;
                    storage.encoding.open(path, file, None, &AtomicBool::new(false), |_| Ok(()))
                })
            }
            .unwrap();
            assert_eq!(outcome, OpenOutcome::Ready);
            storage.transports.insert(id, TransportSeed::document(editor.document()));
            let policy = worker::document_policy(editor.document());
            worker::seed_journal(
                &mut storage,
                id,
                SeedSnapshot { revision: editor.document().revision(), snapshot: editor.document().snapshot() },
                Some(path.clone()),
                0,
                generation,
                policy,
            )
            .unwrap();
            assert_eq!(fs::read(&canonical).unwrap(), previous, "seed admission cannot change old recovery authority");
            assert_eq!(storage.order[&id], 2, "old sequence remains until durable promotion");
            ready(&mut storage, id);
            assert!(!staged.exists());
            assert!(!storage.finished_journals.contains_key(&id));
            assert!(!storage.transports.contains_key(&id));
            assert_eq!(storage.order[&id], 0);
            let mut selected = None;
            kwil_core_workspace::journal::JournalReader::open(&canonical)
                .unwrap()
                .recover(&AtomicBool::new(false), |event| {
                    if let kwil_core_workspace::journal::RecoveryEvent::EncodedBase { base, .. } = event {
                        selected =
                            Some(crate::document_worker::provenance::journal::RecoveryPolicy::from_codec(&base.codec)?);
                    }
                    Ok(())
                })
                .unwrap();
            assert_eq!(selected.unwrap(), policy);
            let rx = observe(&mut editor);
            editor.replace_byte_range(0..0, b"!").unwrap();
            worker::journal_receipt(
                &mut storage,
                id,
                rx.recv_timeout(Duration::from_secs(1)).unwrap(),
                Some(path.clone()),
                1,
                generation,
                policy,
            )
            .unwrap();
            editor.undo().unwrap();
            worker::journal_receipt(
                &mut storage,
                id,
                rx.recv_timeout(Duration::from_secs(1)).unwrap(),
                Some(path.clone()),
                1,
                generation,
                policy,
            )
            .unwrap();
            assert_eq!(storage.order[&id], 2, "new receipts1/2 accepted instead of the old sequence lineage");
        }
        drop(editor);
        storage.service.finish();
        storage.service.finished.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
        storage.journals.clear();
        storage.finished_journals.clear();
        let retired = storage.encoding.retire();
        assert_eq!(retired.live + retired.cleanup_pending, 0);
    }

    #[test]
    fn raw_transport_unselected_staged_collision_preserves_old_recovery_and_transition_owners() {
        use crate::document_worker::{self as worker, SeedSnapshot, TransportSeed};
        let fixture = Fixture::new();
        let root = fixture.0.join("workspace");
        let (prepared, mut storage) = worker::prepare(Some(root.clone())).unwrap();
        drop(prepared);
        let document = Document::new();
        let id = DocumentId(17);
        let canonical = root.join("17.kwj");
        let staged = root.join("17.kwj.next");
        fs::write(&canonical, b"old exact recovery artifact").unwrap();
        fs::write(&staged, b"uncertain staged artifact").unwrap();
        storage.transports.insert(id, TransportSeed::document(&document));
        let error = worker::seed_journal(
            &mut storage,
            id,
            SeedSnapshot { revision: 0, snapshot: document.snapshot() },
            None,
            0,
            1,
            worker::document_policy(&document),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(canonical).unwrap(), b"old exact recovery artifact");
        assert_eq!(fs::read(staged).unwrap(), b"uncertain staged artifact");
        assert!(storage.transports.contains_key(&id));
        storage.service.finish();
        storage.service.finished.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
    }

    #[test]
    fn raw_transport_dirty_text_exit_preserves_raw_editor_undo_and_source_epoch() {
        let fixture = Fixture::new();
        let (path, file) = fixture.file(b"\xff\xfeA\0");
        let mut editor =
            Editor::new(open_file(DocumentId(3), path, file, 1, 0, &AtomicBool::new(false), |_| {}).unwrap());
        editor.replace_byte_range(2..3, b"B").unwrap();
        let before = editor.document().hex_source(3);
        let cursor = editor.primary_cursor().clone();
        let sequence = editor.next_edit_sequence();
        assert_eq!(
            reopen_text(&mut editor, |_| panic!("dirty raw exit must not prepare text")).unwrap(),
            OpenOutcome::NeedsRawReopen
        );
        assert_eq!(editor.document().hex_source(3), before);
        assert_eq!(editor.primary_cursor(), &cursor);
        assert_eq!(editor.next_edit_sequence(), sequence);
        assert_eq!(bytes(editor.document()), b"\xff\xfeB\0");
        editor.undo().unwrap();
        assert_eq!(bytes(editor.document()), b"\xff\xfeA\0");
        assert_eq!(editor.document().hex_source(3).epoch, before.epoch);
        editor.redo().unwrap();
        assert_eq!(bytes(editor.document()), b"\xff\xfeB\0");
    }

    #[test]
    fn raw_transport_worker_text_exit_uses_normal_decode_compression_and_bounded_preview() {
        let physical = b"\xff\xfeA\0\r\0\n\0";
        let mut gzip = Vec::new();
        kwil_core_workspace::compressed::recompress_reader(
            &mut physical.as_slice(),
            &mut gzip,
            &AtomicBool::new(false),
        )
        .unwrap();
        for raw in [physical.as_slice(), gzip.as_slice(), b"\x1f\x8b\x08\0invalid"] {
            let succeeds = raw != b"\x1f\x8b\x08\0invalid";
            let fixture = Fixture::new();
            let (path, file) = fixture.file(raw);
            drop(file);
            let (tx, rx) = mpsc::sync_channel(16);
            let worker = DocumentWorker::spawn(move |result| tx.send(result).unwrap()).unwrap();
            assert!(worker.try_submit(DocumentRequest::Prepare { root: Some(fixture.0.join("workspace")) }).is_ok());
            match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                DocumentResult::Prepared(result) => {
                    drop(result.unwrap());
                }
                _ => panic!("prepare result"),
            }
            assert!(
                worker
                    .try_submit(DocumentRequest::OpenRaw {
                        document: DocumentId(3),
                        revision: 0,
                        path,
                        rows: 1,
                        generation: 1
                    })
                    .is_ok()
            );
            assert!(matches!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), DocumentResult::RawPreview { .. }));
            let editor = match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                DocumentResult::Indexed { result, .. } => Box::new(Editor::new(*result.unwrap())),
                _ => panic!("raw indexed result"),
            };
            let pointer = &*editor as *const Editor;
            let before = editor.document().hex_source(3);
            assert!(
                worker
                    .try_submit(DocumentRequest::ReopenText { document: DocumentId(3), generation: 2, editor })
                    .is_ok()
            );
            let mut preview_seen = false;
            let editor = loop {
                match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                    DocumentResult::Preview { document, revision, preview, .. } => {
                        assert_eq!(document, DocumentId(3));
                        assert_eq!(revision, before.revision);
                        assert_eq!(preview.text, b"A\r\n");
                        preview_seen = true;
                    }
                    DocumentResult::RawReopened { document, generation, editor, result } => {
                        assert_eq!(document, DocumentId(3));
                        assert_eq!(generation, 2);
                        assert_eq!(&*editor as *const Editor, pointer);
                        if succeeds {
                            assert_eq!(result.unwrap(), OpenOutcome::Ready);
                            assert!(preview_seen, "normal text preview precedes completion");
                            assert_eq!(editor.document().representation(), Representation::DecodedText);
                            assert_ne!(editor.document().hex_source(3).epoch, before.epoch);
                            assert_eq!(bytes(editor.document()), b"A\r\n");
                            assert_eq!(
                                editor.document().encoding,
                                crate::DocumentEncoding::Encoded(kwil_core_text::encoding::Encoding::UTF16_LE)
                            );
                            assert!(editor.document().bom);
                            assert!(!crate::document_worker::document_policy(editor.document()).raw_bytes);
                        } else {
                            assert!(result.is_err());
                            assert_eq!(editor.document().hex_source(3), before);
                            assert_eq!(bytes(editor.document()), raw);
                        }
                        break editor;
                    }
                    _ => panic!("text transport result"),
                }
            };
            drop(editor);
            drop(worker);
            loop {
                if let DocumentResult::Stopped(result) = rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                    result.unwrap();
                    break;
                }
            }
        }
    }

    #[test]
    fn raw_transport_recovery_reattaches_physical_source_and_dirty_save_as_bypasses_text_transforms() {
        use crate::document_worker::{self, SeedSnapshot};
        let fixture = Fixture::new();
        let raw = b"\x1f\x8b\x08\0\xff\xfeA\0 \t\r\n";
        let (path, file) = fixture.file(raw);
        let document = open_file(DocumentId(17), path.clone(), file, 1, 0, &AtomicBool::new(false), |_| {}).unwrap();
        let root = fixture.0.join("workspace");
        let (prepared, mut storage) = document_worker::prepare(Some(root.clone())).unwrap();
        drop(prepared);
        document_worker::seed_journal(
            &mut storage,
            DocumentId(17),
            SeedSnapshot { revision: document.revision(), snapshot: document.snapshot() },
            Some(path.clone()),
            0,
            1,
            document_worker::document_policy(&document),
        )
        .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !storage.journals[&DocumentId(17)].ready {
            assert!(std::time::Instant::now() < deadline, "bounded raw seed readiness");
            document_worker::poll_journals(&mut storage, &|result| {
                if let DocumentResult::JournalDurable { result: Err(error), .. } = result {
                    panic!("raw seed: {error}");
                }
            });
            std::thread::park_timeout(Duration::from_millis(1));
        }
        storage.service.finish();
        storage.service.finished.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
        storage.journals.clear();
        drop(document);
        for clean in [true, false] {
            let tab = session::Tab {
                id: DocumentId(17),
                path: Some(path.clone()),
                recovery_path: None,
                revision: 0,
                saved_revision: if clean { 0 } else { 1 },
                cursor: 0,
                anchor: 0,
                first_visible_line: 0,
                first_byte_column: 0,
            };
            let (document, recovered, _, origins) =
                document_worker::recover_prepared(&root, &mut storage.encoding, root.join("17.kwj"), &tab).unwrap();
            assert_eq!(document.representation(), Representation::RawBytes);
            assert_eq!(bytes(&document), raw);
            assert_eq!(
                document.path().is_some(),
                clean,
                "clean physical gzip bytes must reattach without decompression"
            );
            assert_eq!(document.is_dirty(), !clean);
            let mut editor = Editor::new(document);
            editor.settings.editor.trim_trailing_whitespace_on_save = true;
            let output = fixture.0.join(if clean { "clean-copy" } else { "dirty-copy" });
            let request = editor.raw_save_request(Some(output.clone())).unwrap();
            let saved = storage.encoding.save_raw(request, &AtomicBool::new(false));
            editor.finish_save_for_test(saved).unwrap();
            assert_eq!(fs::read(output).unwrap(), raw);
            drop(editor);
            drop(origins);
            drop(recovered);
            let retirement = storage.encoding.retire();
            assert_eq!(retirement.live + retirement.cleanup_pending, 0);
        }
    }

    #[test]
    fn raw_transport_worker_route_shares_undo_fences_replay_and_saves_physical_bytes() {
        let fixture = Fixture::new();
        let raw = b"\x1f\x8b\x08\0\xff\r\n \t";
        let (path, file) = fixture.file(raw);
        drop(file);
        let (tx, rx) = mpsc::sync_channel(16);
        let worker = DocumentWorker::spawn(move |result| tx.send(result).unwrap()).unwrap();
        assert!(worker.try_submit(DocumentRequest::Prepare { root: Some(fixture.0.join("workspace")) }).is_ok());
        match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            DocumentResult::Prepared(result) => {
                drop(result.unwrap());
            }
            _ => panic!("prepare result"),
        }
        assert!(
            worker
                .try_submit(DocumentRequest::OpenRaw {
                    document: DocumentId(8),
                    revision: 0,
                    path,
                    rows: 1,
                    generation: 1
                })
                .is_ok()
        );
        let preview = match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            DocumentResult::RawPreview { page, .. } => page,
            _ => panic!("raw preview precedes index"),
        };
        let mut editor = match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            DocumentResult::Indexed { result, .. } => Box::new(Editor::new(*result.unwrap())),
            _ => panic!("indexed raw document"),
        };
        let original: Source = editor.document().hex_source(8);
        assert_eq!(original, preview.request.source);
        let first = EditRequest {
            source: original,
            operation: 1,
            input_epoch: 1,
            mutation: Mutation::Replace { range: 4..5, bytes: vec![0x80] },
        };
        for request in
            [first.clone(), EditRequest { operation: 2, mutation: Mutation::Undo, ..first.clone() }, first.clone()]
        {
            let request = if request.operation == 2 {
                EditRequest { source: editor.document().hex_source(8), ..request }
            } else {
                request
            };
            assert!(
                worker
                    .try_submit(DocumentRequest::HexEdit { document: DocumentId(8), editor, request: request.clone() })
                    .is_ok()
            );
            let (returned, completion) = match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                DocumentResult::HexEdited { editor, completion, .. } => (editor, completion),
                _ => panic!("hex edit completion"),
            };
            editor = returned;
            if request.operation == 1 && editor.document().revision() == 0 {
                assert!(completion.result.is_err(), "same revision after undo must not admit replay");
            } else {
                assert!(completion.result.is_ok());
            }
            assert_eq!(editor.document().hex_source(8).epoch, original.epoch);
        }
        assert_eq!(bytes(editor.document()), raw);
        editor.settings.editor.trim_trailing_whitespace_on_save = true;
        editor.document.encoding = crate::DocumentEncoding::Encoded(kwil_core_text::encoding::Encoding::UTF16_LE);
        editor.document.bom = true;
        editor.document.compression = kwil_core_workspace::compressed::CompressionFormat::Gzip;
        let destination = fixture.0.join("saved.gz");
        let mut request = editor.raw_save_request(Some(destination.clone())).unwrap();
        // Even a forged text trim/container field cannot alter explicit SaveRaw.
        request.trim_trailing_whitespace = true;
        request.compression = kwil_core_workspace::compressed::CompressionFormat::Gzip;
        assert!(worker.try_submit(DocumentRequest::SaveRaw { document: DocumentId(8), request }).is_ok());
        match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            DocumentResult::Saved { result, .. } => {
                editor.finish_save_for_test(result).unwrap();
            }
            _ => panic!("raw save completion"),
        }
        assert_eq!(fs::read(destination).unwrap(), raw);
        assert_eq!(editor.document().representation(), Representation::RawBytes);
        drop(editor);
        drop(worker);
        loop {
            if let DocumentResult::Stopped(result) = rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                result.unwrap();
                break;
            }
        }
    }
}
