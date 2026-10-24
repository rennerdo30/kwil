//! Actual fixed-worker regressions; ROOT owns execution and fail-first proof.
use super::*;
use crate::document_worker::{DocumentRequest, DocumentResult, DocumentWorker};
use std::{
    fs,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

const BINARY: &[u8] = b"header\0binary\x01\n";
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "kwil-binary-offer-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
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
        if !std::thread::panicking() {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
}
fn ticket(document: u64, generation: u64) -> Ticket {
    Ticket { document: DocumentId(document), revision: 0, generation }
}
fn start(root: &Fixture) -> (DocumentWorker, mpsc::Receiver<DocumentResult>) {
    let (tx, rx) = mpsc::channel();
    let worker = DocumentWorker::spawn(move |result| {
        let _ = tx.send(result);
    })
    .unwrap();
    crate::document_worker::tests::prepare_test_worker(&worker, &rx, &root.0);
    (worker, rx)
}
fn next(rx: &mpsc::Receiver<DocumentResult>, until: impl Fn(&DocumentResult) -> bool) -> DocumentResult {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let result = rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("bounded binary worker completion");
        if until(&result) {
            return result;
        }
    }
}
fn offer(
    worker: &DocumentWorker,
    rx: &mpsc::Receiver<DocumentResult>,
    path: PathBuf,
    file: Option<TextFile>,
    ticket: Ticket,
) -> Offer {
    worker.try_submit(DocumentRequest::OpenChecked { ticket, path, file }).unwrap();
    let result = next(rx, |r| {
        matches!(
            r,
            DocumentResult::BinaryOffer(_)
                | DocumentResult::CheckedPreview { .. }
                | DocumentResult::CheckedOpened { .. }
        )
    });
    let DocumentResult::BinaryOffer(offer) = result else {
        panic!("binary must suspend before any decoded preview/completion")
    };
    assert_eq!(offer.ticket, ticket);
    offer
}
fn finish(worker: DocumentWorker, rx: &mpsc::Receiver<DocumentResult>) {
    drop(worker);
    crate::document_worker::tests::wait_stopped(rx);
}
fn logical(document: &crate::Document) -> Vec<u8> {
    let mut bytes = Vec::new();
    document.snapshot().write_to(&mut bytes).unwrap();
    bytes
}

#[test]
fn binary_offer_worker_suspends_before_decode_index_and_serves_other_work() {
    let fixture = Fixture::new();
    let (path, file) = fixture.file(BINARY);
    let (worker, rx) = start(&fixture);
    let offered = offer(&worker, &rx, path, Some(file.clone()), ticket(1, 1));
    assert_eq!(offered.len, BINARY.len() as u64);
    assert!(file.index().is_none(), "no full-file index before choice");
    assert!(
        fs::read_dir(&fixture.0).unwrap().all(|entry| !entry.unwrap().file_type().unwrap().is_dir()),
        "no full decoded private backing before choice"
    );
    // A queue barrier proves the lane is free, with the undecided source still
    // suspended; this does not use a delay or a large-file timing threshold.
    worker.try_submit(DocumentRequest::Persist(Default::default())).unwrap();
    loop {
        match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            DocumentResult::Persisted(result) => {
                result.unwrap();
                break;
            }
            DocumentResult::CheckedPreview { .. } | DocumentResult::CheckedOpened { .. } => {
                panic!("undecided source must remain suspended")
            }
            _ => (),
        }
    }
    assert!(file.index().is_none());
    drop(file);
    // Shutdown retires an unanswered offer on the same lane.
    finish(worker, &rx);
}

#[test]
fn binary_offer_worker_hex_uses_exact_source_and_text_resumes_normal_decode() {
    for choice in [Choice::Hex, Choice::Text] {
        let fixture = Fixture::new();
        let (path, file) = fixture.file(BINARY);
        let (worker, rx) = start(&fixture);
        let offered = offer(&worker, &rx, path, Some(file.clone()), ticket(1, 1));
        assert!(file.index().is_none());
        worker.try_submit(DocumentRequest::ResolveBinary { ticket: offered.ticket, choice, rows: 2 }).unwrap();
        let mut previews = 0;
        let deadline = Instant::now() + Duration::from_secs(5);
        let document = loop {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap() {
                DocumentResult::CheckedRawPreview { ticket, page } => {
                    assert_eq!(choice, Choice::Hex);
                    assert_eq!(ticket, offered.ticket);
                    assert_eq!(page.request.source.len, BINARY.len() as u64);
                    previews += 1;
                }
                DocumentResult::CheckedPreview { ticket, preview } => {
                    assert_eq!(choice, Choice::Text);
                    assert_eq!(ticket, offered.ticket);
                    assert_eq!(preview.text, BINARY);
                    previews += 1;
                }
                DocumentResult::CheckedOpened { ticket, result } => {
                    assert_eq!(ticket, offered.ticket);
                    break result.unwrap().unwrap();
                }
                DocumentResult::BinaryOffer(_) | DocumentResult::BinaryRefused { .. } => {
                    panic!("accepted choice cannot reoffer")
                }
                _ => (),
            }
        };
        assert_eq!(previews, 1, "chosen first-screen route only");
        assert_eq!(logical(&document), BINARY);
        assert_eq!(
            document.representation(),
            if choice == Choice::Hex {
                crate::hex_session::Representation::RawBytes
            } else {
                crate::hex_session::Representation::DecodedText
            }
        );
        file.build_index(1).unwrap();
        assert!(
            std::ptr::eq(
                document.original.as_ref().unwrap().index().expect("same retained source shares its index"),
                file.index().unwrap()
            ),
            "acceptance must use the exact retained TextFile, not reopen the pathname"
        );
        drop(document);
        drop(file);
        finish(worker, &rx);
    }
}

#[test]
fn binary_offer_worker_cancel_retires_exact_owner_and_stale_choice_cannot_consume_new_offer() {
    let fixture = Fixture::new();
    let (path, file) = fixture.file(BINARY);
    let (worker, rx) = start(&fixture);
    let old = offer(&worker, &rx, path.clone(), Some(file.clone()), ticket(1, 1));
    worker.try_submit(DocumentRequest::ResolveBinary { ticket: old.ticket, choice: Choice::Cancel, rows: 0 }).unwrap();
    assert!(matches!(
        next(&rx, |r| matches!(r, DocumentResult::CheckedOpened { .. })),
        DocumentResult::CheckedOpened { result: Ok(None), .. }
    ));
    assert!(file.index().is_none());
    assert!(file.try_close().is_ok(), "Cancel must release the pending mapping on the worker");
    let offered = offer(&worker, &rx, path, None, ticket(1, 2));
    worker.try_submit(DocumentRequest::ResolveBinary { ticket: old.ticket, choice: Choice::Hex, rows: 1 }).unwrap();
    assert!(
        matches!(next(&rx, |r| matches!(r, DocumentResult::CheckedOpened { .. })), DocumentResult::CheckedOpened { result: Err(error), .. } if error.kind() == io::ErrorKind::Interrupted)
    );
    worker
        .try_submit(DocumentRequest::ResolveBinary { ticket: offered.ticket, choice: Choice::Cancel, rows: 0 })
        .unwrap();
    assert!(matches!(
        next(&rx, |r| matches!(r, DocumentResult::CheckedOpened { .. })),
        DocumentResult::CheckedOpened { result: Ok(None), .. }
    ));
    finish(worker, &rx);
}

#[test]
fn binary_offer_worker_utf16_bom_and_no_bom_and_gzip_keep_existing_text_route() {
    const GZIP: &[u8] = include_bytes!("../../../examples/smoke/example.log.gz");
    const SAMPLE: &[u8] = include_bytes!("../../../examples/smoke/example.log");
    for (raw, expected) in [
        (b"\xff\xfea\0b\0c\0d\0\n\0".as_slice(), b"abcd\n".as_slice()),
        (b"\xfe\xff\0a\0b\0c\0d\0\n".as_slice(), b"abcd\n".as_slice()),
        (b"a\0b\0c\0d\0e\0f\0\n\0".as_slice(), b"abcdef\n".as_slice()),
        (b"\0a\0b\0c\0d\0e\0f\0\n".as_slice(), b"abcdef\n".as_slice()),
        (GZIP, SAMPLE),
    ] {
        let fixture = Fixture::new();
        let (path, file) = fixture.file(raw);
        let (worker, rx) = start(&fixture);
        drop(file);
        worker.try_submit(DocumentRequest::OpenChecked { ticket: ticket(1, 1), path, file: None }).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let document = loop {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap() {
                DocumentResult::BinaryOffer(_) => panic!("UTF16/compression exclusions must precede offer"),
                DocumentResult::CheckedPreview { preview, .. } => assert!(!preview.text.starts_with(b"\x1f\x8b")),
                DocumentResult::CheckedOpened { result, .. } => break result.unwrap().unwrap(),
                _ => (),
            }
        };
        assert_eq!(logical(&document), expected);
        drop(document);
        finish(worker, &rx);
    }
}

#[test]
fn binary_offer_worker_changed_source_and_corrupt_gzip_fail_without_offering_or_leaking() {
    for gzip in [false, true] {
        let fixture = Fixture::new();
        let (path, file) = fixture.file(if gzip { b"\x1f\x8b\0broken" } else { BINARY });
        let (worker, rx) = start(&fixture);
        if gzip {
            drop(file);
            worker.try_submit(DocumentRequest::OpenChecked { ticket: ticket(1, 1), path, file: None }).unwrap();
        } else {
            let offered = offer(&worker, &rx, path.clone(), Some(file.clone()), ticket(1, 1));
            fs::write(path, b"changed").unwrap();
            worker
                .try_submit(DocumentRequest::ResolveBinary { ticket: offered.ticket, choice: Choice::Text, rows: 1 })
                .unwrap();
            drop(file);
        }
        loop {
            match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                DocumentResult::BinaryOffer(_) | DocumentResult::CheckedPreview { .. } => {
                    panic!("failed source/container must not paint")
                }
                DocumentResult::CheckedOpened { result, .. } => {
                    assert!(result.is_err());
                    break;
                }
                _ => (),
            }
        }
        finish(worker, &rx);
        assert!(
            fs::read_dir(&fixture.0).unwrap().all(|entry| !entry.unwrap().file_type().unwrap().is_dir()),
            "failed preparation must retire private resources"
        );
    }
}

#[test]
fn binary_offer_registry_refuses_full_duplicate_and_stale_without_evicting_owner() {
    let fixture = Fixture::new();
    let (path, file) = fixture.file(BINARY);
    let mut runtime = Runtime::new();
    for id in 1..=session::MAX_TABS as u64 {
        assert!(matches!(
            runtime.detect(ticket(id, id), path.clone(), Some(file.clone())).unwrap(),
            Detection::Offer(_)
        ));
    }
    assert_eq!(
        runtime.detect(ticket(1000, 1000), path.clone(), Some(file.clone())).err().unwrap().kind(),
        io::ErrorKind::WouldBlock
    );
    assert_eq!(
        runtime.detect(ticket(1, 2000), path, Some(file.clone())).err().unwrap().kind(),
        io::ErrorKind::WouldBlock
    );
    assert!(runtime.take(ticket(1, 2000)).is_err());
    assert_eq!(runtime.slots.iter().flatten().count(), session::MAX_TABS);
    assert!(file.index().is_none());
    file.build_index(1).unwrap();
    assert!(std::ptr::eq(runtime.pending(ticket(1, 1)).unwrap().file.index().unwrap(), file.index().unwrap()));
    drop(runtime);
    assert!(file.try_close().is_ok());
}

#[test]
fn binary_offer_worker_duplicate_open_refusal_retains_the_original_offer_owner() {
    let fixture = Fixture::new();
    let (path, file) = fixture.file(BINARY);
    let (worker, rx) = start(&fixture);
    let original = offer(&worker, &rx, path.clone(), Some(file.clone()), ticket(1, 1));
    worker.try_submit(DocumentRequest::OpenChecked { ticket: ticket(1, 2), path, file: Some(file.clone()) }).unwrap();
    assert!(
        matches!(next(&rx, |r| matches!(r, DocumentResult::CheckedOpened { .. })), DocumentResult::CheckedOpened { ticket: refused, result: Err(error) } if refused == ticket(1, 2) && error.kind() == io::ErrorKind::WouldBlock)
    );
    assert!(file.index().is_none());
    worker
        .try_submit(DocumentRequest::ResolveBinary { ticket: original.ticket, choice: Choice::Cancel, rows: 0 })
        .unwrap();
    assert!(
        matches!(next(&rx, |r| matches!(r, DocumentResult::CheckedOpened { .. })), DocumentResult::CheckedOpened { ticket: cancelled, result: Ok(None) } if cancelled == original.ticket)
    );
    assert!(file.try_close().is_ok(), "refused duplicate must not replace or leak the retained source");
    finish(worker, &rx);
}
