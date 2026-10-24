//! Acceptance oracles for the actual fixed document-worker lane. Source only
//! until ROOT admits the focused selection; no native windows or owner profile.
use super::*;
use std::{path::Path, sync::atomic::Ordering};

const SAMPLE: &[u8] = include_bytes!("../../../../examples/smoke/example.log");
const GZIP: &[u8] = include_bytes!("../../../../examples/smoke/example.log.gz");

fn logical(document: &Document) -> Vec<u8> {
    let mut bytes = Vec::new();
    document.snapshot().write_to(&mut bytes).unwrap();
    bytes
}

fn start(root: &Path) -> (DocumentWorker, mpsc::Receiver<DocumentResult>) {
    let (tx, rx) = mpsc::channel();
    let worker = DocumentWorker::spawn(move |result| {
        let _ = tx.send(result);
    })
    .unwrap();
    super::tests::prepare_test_worker(&worker, &rx, root);
    (worker, rx)
}

fn opened(worker: &DocumentWorker, rx: &mpsc::Receiver<DocumentResult>, path: &Path) -> io::Result<Box<Document>> {
    worker
        .try_submit(DocumentRequest::Open { document: DocumentId(1), revision: 0, path: path.to_path_buf() })
        .unwrap();
    loop {
        match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            DocumentResult::Indexed { result, .. } => return result,
            DocumentResult::Opened { result: Err(error), .. } => return Err(error),
            DocumentResult::Opened { result: Ok(_), .. } => panic!("raw open must never bypass decompression"),
            DocumentResult::Preview { preview, .. } => {
                assert!(!preview.text.starts_with(b"\x1f\x8b"), "gzip bytes must never reach first screen");
            }
            _ => (),
        }
    }
}

#[test]
fn compressed_worker_sample_exact_save_reopen_and_clean_session_restore() {
    let root = super::tests::Scratch::new();
    // Signature detection must work even when the source has no gzip suffix.
    let path = root.0.join("owner-sample");
    std::fs::write(&path, GZIP).unwrap();
    let (worker, rx) = start(&root.0);
    let document = opened(&worker, &rx, &path).unwrap();
    assert_eq!(logical(&document), SAMPLE, "actual owner text oracle");
    assert_eq!(document.original.as_ref().unwrap().path(), path);
    let mut editor = crate::Editor::new(*document);
    editor
        .execute("edit.insert", &crate::CommandArgs { text: Some("prefix\n".into()), ..Default::default() }, 1)
        .unwrap();
    let mut expected = b"prefix\n".to_vec();
    expected.extend_from_slice(SAMPLE);
    let request = editor.save_request(None).unwrap();
    worker
        .try_submit(DocumentRequest::Save { document: DocumentId(1), request, plan: editor.document().save_plan() })
        .unwrap();
    loop {
        if let DocumentResult::Saved { result, .. } = rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            editor.finish_save(result).unwrap();
            break;
        }
    }
    assert!(std::fs::read(&path).unwrap().starts_with(b"\x1f\x8b"), "never plaintext on compressed save");
    assert_eq!(logical(editor.document()), expected);
    drop(editor);
    let reopened = opened(&worker, &rx, &path).unwrap();
    assert_eq!(logical(&reopened), expected);
    drop(reopened);
    // Persist only clean named metadata: Prepare must use the same open route.
    let mut session = Session::default();
    session.open(Some(path.clone())).unwrap();
    worker.try_submit(DocumentRequest::Persist(session)).unwrap();
    loop {
        if let DocumentResult::Persisted(result) = rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            result.unwrap();
            break;
        }
    }
    drop(worker);
    super::tests::wait_stopped(&rx);
    let (tx, rx) = mpsc::channel();
    let worker = DocumentWorker::spawn(move |result| {
        let _ = tx.send(result);
    })
    .unwrap();
    worker.try_submit(DocumentRequest::Prepare { root: Some(root.0.clone()) }).unwrap();
    loop {
        if let DocumentResult::Prepared(result) = rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            let prepared = result.unwrap();
            assert_eq!(prepared.documents.len(), 1);
            assert_eq!(logical(prepared.documents[0].1.as_ref().unwrap()), expected);
            drop(prepared);
            break;
        }
    }
    drop(worker);
    super::tests::wait_stopped(&rx);
    assert!(!std::fs::read_dir(&root.0).unwrap().any(|e| e.unwrap().path().is_dir()));
}

#[test]
fn compressed_worker_rejects_crc_truncation_and_unsupported_without_raw_fallback() {
    let root = super::tests::Scratch::new();
    let (worker, rx) = start(&root.0);
    let path = root.0.join("corrupt.gz");
    let mut crc = GZIP.to_vec();
    let at = crc.len() - 8;
    crc[at] ^= 1;
    let cases = [crc, GZIP[..GZIP.len() - 3].to_vec(), b"\xfd7zXZ\0unsupported".to_vec()];
    for bytes in cases {
        std::fs::write(&path, &bytes).unwrap();
        let error = opened(&worker, &rx, &path).err().expect("detected compression must refuse invalid input");
        assert!(!error.to_string().is_empty());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
    drop(worker);
    super::tests::wait_stopped(&rx);
    assert!(!std::fs::read_dir(&root.0).unwrap().any(|e| e.unwrap().path().is_dir()));
}

fn registry(root: &Path, slots: usize) -> retirement::WorkerBackings {
    retirement::WorkerBackings::new(
        root.to_path_buf(),
        retirement::BackingLimits { max_backings: slots, max_private_path_bytes: 1024, max_metadata_bytes: 2 << 20 },
    )
    .unwrap()
}

#[test]
fn compressed_worker_snapshot_retirement_busy_save_and_exact_cleanup_retry() {
    let root = super::tests::Scratch::new();
    let path = root.0.join("owner.gz");
    std::fs::write(&path, GZIP).unwrap();
    let cancel = AtomicBool::new(false);
    let mut backings = registry(&root.0, 1);
    let document = backings.open(path.clone(), TextFile::open(&path).unwrap(), None, &cancel, |_| Ok(())).unwrap();
    let snapshot = document.snapshot();
    let mut editor = crate::Editor::new(document);
    editor.execute("edit.insert", &crate::CommandArgs { text: Some("!".into()), ..Default::default() }, 1).unwrap();
    let request = editor.save_request(None).unwrap();
    let error = backings.save(request, editor.document().save_plan(), &cancel).err().unwrap();
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    assert_eq!(std::fs::read(&path).unwrap(), GZIP);
    editor.finish_save(Err(error)).unwrap_err();
    // Keep a snapshot pin while the editable document disappears.
    drop(editor);
    assert_eq!(backings.retire().live, 1);
    let mut bytes = Vec::new();
    snapshot.write_to(&mut bytes).unwrap();
    assert_eq!(bytes, SAMPLE);
    let directories: Vec<_> = std::fs::read_dir(&root.0)
        .unwrap()
        .map(Result::unwrap)
        .filter(|e| e.path().is_dir())
        .map(|e| e.path())
        .collect();
    assert!(!directories.is_empty());
    // Only the exact owned directory is retried; a foreign blocker is retained.
    let blocker = directories[0].join("foreign-blocker");
    std::fs::write(&blocker, b"retain").unwrap();
    drop(snapshot);
    let report = backings.retire();
    assert_eq!(report.cleanup_pending, 1);
    assert!(report.first_error.is_some());
    assert_eq!(std::fs::read(&blocker).unwrap(), b"retain");
    std::fs::remove_file(blocker).unwrap();
    assert_eq!(backings.retire().retired, 1);
    assert_eq!(backings.retained(), 0);
}

#[test]
fn compressed_worker_cancellation_and_changed_original_prevent_save() {
    let root = super::tests::Scratch::new();
    let path = root.0.join("owner.gz");
    std::fs::write(&path, GZIP).unwrap();
    let cancel = AtomicBool::new(true);
    let mut backings = registry(&root.0, 2);
    let error = backings
        .open(path.clone(), TextFile::open(&path).unwrap(), None, &cancel, |_| panic!("cancelled preview"))
        .err()
        .unwrap();
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    cancel.store(false, Ordering::Relaxed);
    let document = backings.open(path.clone(), TextFile::open(&path).unwrap(), None, &cancel, |_| Ok(())).unwrap();
    let mut editor = crate::Editor::new(document);
    editor.execute("edit.insert", &crate::CommandArgs { text: Some("!".into()), ..Default::default() }, 1).unwrap();
    std::fs::write(&path, b"foreign replacement").unwrap();
    let request = editor.save_request(None).unwrap();
    let error = backings.save(request, editor.document().save_plan(), &cancel).err().unwrap();
    editor.finish_save(Err(error)).unwrap_err();
    assert_eq!(std::fs::read(&path).unwrap(), b"foreign replacement");
    drop(editor);
    backings.retire();
    assert_eq!(backings.retained(), 0);
}

#[test]
fn compressed_worker_failed_open_retains_exact_body_cleanup_owner() {
    let root = super::tests::Scratch::new();
    let path = root.0.join("owner.gz");
    std::fs::write(&path, GZIP).unwrap();
    let cancel = AtomicBool::new(false);
    let mut backings = registry(&root.0, 1);
    let blocker = std::cell::RefCell::new(None);
    let error = backings
        .open(path.clone(), TextFile::open(&path).unwrap(), None, &cancel, |_| {
            let directory = std::fs::read_dir(&root.0)?.map(Result::unwrap).find(|e| e.path().is_dir()).unwrap().path();
            let obstruction = directory.join("foreign-blocker");
            std::fs::write(&obstruction, b"retain")?;
            *blocker.borrow_mut() = Some(obstruction);
            cancel.store(true, Ordering::Relaxed);
            Ok(())
        })
        .err()
        .unwrap();
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    assert_eq!(backings.retained(), 1);
    assert_eq!(backings.retire().cleanup_pending, 1);
    cancel.store(false, Ordering::Relaxed);
    let error = backings
        .open(path.clone(), TextFile::open(&path).unwrap(), None, &cancel, |_| panic!("busy preview"))
        .err()
        .unwrap();
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    let blocker = blocker.into_inner().unwrap();
    assert_eq!(std::fs::read(&blocker).unwrap(), b"retain");
    std::fs::remove_file(blocker).unwrap();
    assert_eq!(backings.retire().retired, 1);
    assert_eq!(backings.retained(), 0);
    assert_eq!(std::fs::read(&path).unwrap(), GZIP);
}

#[test]
fn compressed_worker_dirty_recovery_keeps_gzip_policy_for_save_as() {
    use crate::{CommandArgs, View, Workspace};
    use kwil_core_workspace::compressed::CompressionFormat;
    let root = super::tests::Scratch::new();
    let path = root.0.join("owner.gz");
    std::fs::write(&path, GZIP).unwrap();
    let output = root.0.join("saved-as.txt");
    let mut id = None;
    for round in 0..3 {
        let (tx, rx) = mpsc::channel();
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        workspace.set_session_restore(true);
        workspace
            .start_document_worker_at(Some(root.0.clone()), move |result| {
                let _ = tx.send(result);
            })
            .unwrap();
        super::tests::drive(&mut workspace, &rx, Workspace::recovery_ready);
        let document = if round == 0 {
            let opened = workspace.open_path(path.clone()).unwrap();
            super::tests::drive(&mut workspace, &rx, |w| w.view(opened).is_some_and(|v| v.editor().is_some()));
            id = Some(opened);
            opened
        } else {
            id.unwrap()
        };
        workspace.activate(document);
        let model = workspace.view(document).unwrap().editor().unwrap().document();
        assert_eq!(model.compression, CompressionFormat::Gzip);
        if round == 0 {
            assert_eq!(logical(model), SAMPLE);
            workspace.execute("edit.insert", &CommandArgs { text: Some("!".into()), ..Default::default() }, 1).unwrap();
            workspace.request_window_close(false).unwrap();
            super::tests::drive(&mut workspace, &rx, Workspace::wants_exit);
        } else if round == 1 {
            assert!(model.path().is_none(), "dirty recovery does not adopt current disk baseline");
            let mut expected = b"!".to_vec();
            expected.extend_from_slice(SAMPLE);
            assert_eq!(logical(model), expected);
            // Output extension must not select the transport format.
            workspace.save_document(document, Some(output.clone())).unwrap();
            super::tests::drive(&mut workspace, &rx, |w| {
                !w.view(document).unwrap().editor().unwrap().document().is_dirty()
            });
            assert!(std::fs::read(&output).unwrap().starts_with(b"\x1f\x8b"));
            // Separate registry validates actual saved gzip payload byte equality.
            let mut backings = registry(&root.0, 1);
            let reopened = backings
                .open(output.clone(), TextFile::open(&output).unwrap(), None, &AtomicBool::new(false), |_| Ok(()))
                .unwrap();
            assert_eq!(logical(&reopened), expected);
            drop(reopened);
            backings.retire();
            assert_eq!(backings.retained(), 0);
            workspace.request_window_close(false).unwrap();
            super::tests::drive(&mut workspace, &rx, Workspace::wants_exit);
        } else {
            assert_eq!(
                model.path(),
                Some(output.as_path()),
                "clean compressed journal regains only a verified original"
            );
            assert!(!model.is_dirty());
            let mut expected = b"!".to_vec();
            expected.extend_from_slice(SAMPLE);
            assert_eq!(logical(model), expected);
        }
        drop(workspace);
        super::tests::wait_stopped(&rx);
        assert_eq!(std::fs::read(&path).unwrap(), GZIP);
    }
}

#[test]
fn compressed_worker_encoding_is_detected_after_decompression_and_roundtrips_opaque_bytes() {
    use kwil_core_text::encoding::Encoding;
    use kwil_core_workspace::compressed::{CompressionFormat, recompress_reader};
    let root = super::tests::Scratch::new();
    let path = root.0.join("encoded.gz");
    let raw = b"\xff\xfeA\0\0\xd8\r\0\n\0";
    let mut gzip = Vec::new();
    recompress_reader(&mut raw.as_slice(), &mut gzip, &AtomicBool::new(false)).unwrap();
    std::fs::write(&path, &gzip).unwrap();
    let (worker, rx) = start(&root.0);
    let document = opened(&worker, &rx, &path).unwrap();
    assert_eq!(document.compression, CompressionFormat::Gzip);
    assert_eq!(document.save_plan().encoding, Encoding::UTF16_LE);
    assert!(document.bom);
    assert_eq!(document.line_ending, crate::DocumentLineEnding::CrLf);
    assert_eq!(logical(&document), b"A\xff\r\n");
    let mut editor = crate::Editor::new(*document);
    let request = editor.save_request(None).unwrap();
    worker
        .try_submit(DocumentRequest::Save { document: DocumentId(1), request, plan: editor.document().save_plan() })
        .unwrap();
    loop {
        if let DocumentResult::Saved { result, .. } = rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            editor.finish_save(result).unwrap();
            break;
        }
    }
    drop(editor);
    let mut backings = registry(&root.0, 1);
    assert!(
        backings
            .verify_source(
                &TextFile::open(&path).unwrap(),
                CompressionFormat::Gzip,
                &AtomicBool::new(false),
                |body| Ok(body.bytes().map_err(io::Error::other)? == raw)
            )
            .unwrap()
    );
    backings.retire();
    assert_eq!(backings.retained(), 0);
    drop(worker);
    super::tests::wait_stopped(&rx);
}

#[test]
fn compressed_worker_shutdown_reports_cleanup_quarantine_without_unlinking_foreign_data() {
    let root = super::tests::Scratch::new();
    let path = root.0.join("owner.gz");
    std::fs::write(&path, GZIP).unwrap();
    let (worker, rx) = start(&root.0);
    let document = opened(&worker, &rx, &path).unwrap();
    let directory = std::fs::read_dir(&root.0)
        .unwrap()
        .map(Result::unwrap)
        .find(|entry| entry.file_name().to_string_lossy().starts_with("kwil-decoded-"))
        .unwrap()
        .path();
    let blocker = directory.join("foreign-blocker");
    std::fs::write(&blocker, b"retain").unwrap();
    drop(document);
    drop(worker);
    let error = loop {
        if let DocumentResult::Stopped(result) = rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            break result.expect_err("retained cleanup must never report successful shutdown");
        }
    };
    let pending = error.get_ref().unwrap().downcast_ref::<PendingBackingRetirement>().unwrap();
    assert_eq!(pending.root, root.0);
    assert!(pending.cleanup_pending > 0);
    assert_eq!(std::fs::read(&blocker).unwrap(), b"retain");
    assert_eq!(std::fs::read(&path).unwrap(), GZIP);
    // The injected obstruction is fixture-owned. All mapping handles retired
    // before this cleanup error, so test cleanup removes only these exact paths.
    std::fs::remove_file(blocker).unwrap();
    std::fs::remove_dir(directory).unwrap();
}
