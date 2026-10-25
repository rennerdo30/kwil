//! Actual Workspace/document-lane and scalar admission oracles.
use super::*;
use crate::document_worker::binary_offer::{Choice, Offer, Ticket};
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
            "kwil-binary-model-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> PathBuf {
        let path = self.0.join("source");
        fs::write(&path, BINARY).unwrap();
        path
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
}
fn drive(workspace: &mut Workspace, rx: &mpsc::Receiver<DocumentResult>, done: impl Fn(&Workspace) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done(workspace) {
        let wait = deadline.checked_duration_since(Instant::now()).expect("bounded binary model deadline");
        workspace.test_document_result(rx.recv_timeout(wait).expect("binary model worker wake"));
    }
}
fn start(fixture: &Fixture, view: View) -> (Workspace, mpsc::Receiver<DocumentResult>) {
    let (tx, rx) = mpsc::channel();
    let mut workspace = Workspace::new(view).unwrap();
    workspace
        .start_document_worker_at(Some(fixture.0.join("recovery")), move |result| {
            let _ = tx.send(result);
        })
        .unwrap();
    drive(&mut workspace, &rx, Workspace::recovery_ready);
    (workspace, rx)
}
fn stop(mut workspace: Workspace, rx: &mpsc::Receiver<DocumentResult>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !workspace.prepare_hex_shutdown() {
        let wait = deadline.checked_duration_since(Instant::now()).expect("bounded binary shutdown admission");
        workspace.test_document_result(rx.recv_timeout(wait).unwrap());
    }
    drop(workspace);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let DocumentResult::Stopped(result) =
            rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap()
        {
            result.unwrap();
            break;
        }
    }
}
fn opened(fixture: &Fixture) -> (Workspace, mpsc::Receiver<DocumentResult>, Offer) {
    let path = fixture.path();
    let (mut workspace, rx) = start(fixture, View::new(None));
    workspace.open_path(path).unwrap();
    drive(&mut workspace, &rx, |w| w.binary_offer().is_some());
    let offer = workspace.binary_offer().unwrap();
    assert!(workspace.view(offer.ticket.document()).unwrap().editor().is_none());
    assert!(workspace.view(offer.ticket.document()).unwrap().file().is_none());
    (workspace, rx, offer)
}
fn logical(workspace: &Workspace, id: DocumentId) -> Vec<u8> {
    let mut bytes = Vec::new();
    workspace.views[&id].editor().unwrap().document().snapshot().write_to(&mut bytes).unwrap();
    bytes
}

#[test]
fn binary_offer_model_checked_open_failure_keeps_open_stage_and_releases_pending_owner() {
    let fixture = Fixture::new();
    let (mut workspace, rx) = start(&fixture, View::new(None));
    let missing = fixture.0.join("missing-parent").join("absent.txt");
    let id = workspace.open_path(missing.clone()).unwrap();
    drive(&mut workspace, &rx, |w| !w.binary_offer_pending(id));
    let failed = workspace.view(id).unwrap();
    assert!(failed.editor().is_none());
    assert_eq!(failed.path.as_deref(), Some(missing.as_path()));
    assert!(failed.notice().starts_with("Open failed:"), "{}", failed.notice());
    assert!(workspace.files.failed_opens.contains(&id));
    assert!(workspace.binary_offer_for(id).is_none());
    stop(workspace, &rx);
}

#[test]
fn binary_offer_model_actual_hex_text_cancel_and_late_callbacks() {
    for choice in [Choice::Hex, Choice::Text, Choice::Cancel] {
        let fixture = Fixture::new();
        let (mut workspace, rx, offer) = opened(&fixture);
        let id = offer.ticket.document();
        workspace.set_rows(2);
        assert!(
            (0..2).all(|row| workspace.line(row).is_none_or(<[u8]>::is_empty)),
            "no binary bytes may be displayed before choice"
        );
        assert!(workspace.editor_input_ticket(id).is_none());
        workspace.resolve_binary_offer(offer.ticket, choice, 2).unwrap();
        assert!(workspace.binary_offer().is_none());
        assert!(workspace.binary_offer_pending(id));
        assert!(workspace.document_busy(id));
        assert!(workspace.close(id, true).is_err(), "accepted source completion must retain its target");
        assert!(workspace.resolve_binary_offer(offer.ticket, Choice::Hex, 2).is_err());
        drive(&mut workspace, &rx, |w| {
            !w.binary_offer_pending(id) && (choice == Choice::Cancel || w.views[&id].editor().is_some())
        });
        if choice == Choice::Cancel {
            assert!(workspace.view(id).is_none());
            assert_eq!(workspace.session.tabs().len(), 1);
            assert!(logical(&workspace, workspace.active_id()).is_empty());
        } else {
            assert_eq!(logical(&workspace, id), BINARY);
            assert_eq!(workspace.hex_owns_document(id), choice == Choice::Hex);
        }
        workspace.test_document_result(DocumentResult::BinaryOffer(offer));
        assert!(workspace.binary_offer().is_none(), "late offer cannot overwrite chosen representation");
        stop(workspace, &rx);
    }
}

#[test]
fn binary_offer_model_startup_suspends_raw_view_before_first_layout() {
    let fixture = Fixture::new();
    let path = fixture.path();
    let file = kwil_core_text::TextFile::open(&path).unwrap();
    let mut workspace = Workspace::new(View::new(Some((path, file.clone())))).unwrap();
    workspace.set_rows(2);
    assert!(workspace.file().is_none(), "startup source transfers out of paintable View immediately");
    assert!((0..2).all(|row| workspace.line(row).is_none_or(<[u8]>::is_empty)));
    let (tx, rx) = mpsc::channel();
    workspace
        .start_document_worker_at(Some(fixture.0.join("recovery")), move |result| {
            let _ = tx.send(result);
        })
        .unwrap();
    drive(&mut workspace, &rx, |w| w.binary_offer().is_some());
    assert!(file.index().is_none(), "initial index route must suspend too");
    let ticket = workspace.binary_offer().unwrap().ticket;
    drop(file);
    workspace.resolve_binary_offer(ticket, Choice::Cancel, 0).unwrap();
    drive(&mut workspace, &rx, |w| !w.binary_offer_pending(ticket.document()));
    stop(workspace, &rx);
}

#[test]
fn binary_offer_model_close_reopen_retires_owner_and_generation_fences_stale_ticket() {
    let fixture = Fixture::new();
    let (mut workspace, rx, old) = opened(&fixture);
    let id = old.ticket.document();
    workspace.close(id, true).unwrap();
    assert!(workspace.binary_offer_for(id).is_none());
    workspace.file_command("file.reopenClosed").unwrap();
    drive(&mut workspace, &rx, |w| w.binary_offer().is_some());
    let new = workspace.binary_offer().unwrap();
    assert_eq!(new.ticket.document(), id);
    assert!(new.ticket.generation() > old.ticket.generation());
    assert_eq!(
        workspace.resolve_binary_offer(old.ticket, Choice::Text, 1).unwrap_err().kind(),
        io::ErrorKind::Interrupted
    );
    workspace.test_document_result(DocumentResult::CheckedOpened { ticket: old.ticket, result: Ok(None) });
    assert_eq!(workspace.binary_offer(), Some(new));
    workspace.resolve_binary_offer(new.ticket, Choice::Cancel, 0).unwrap();
    drive(&mut workspace, &rx, |w| !w.binary_offer_pending(id));
    stop(workspace, &rx);
}

fn offered_channel(capacity: usize) -> (Workspace, mpsc::Receiver<DocumentRequest>, Offer) {
    let mut workspace = Workspace::new(View::new(None)).unwrap();
    let id = workspace.active_id();
    let path = std::env::temp_dir().join("kwil-binary-scalar-only");
    workspace.views.get_mut(&id).unwrap().editor = None;
    workspace.views.get_mut(&id).unwrap().path = Some(path.clone());
    workspace.session.tab_mut(id).unwrap().path = Some(path.clone());
    let ticket = Ticket { document: id, revision: 0, generation: 1 };
    let offer = Offer { ticket, len: 16 };
    workspace.binary.flights.insert(
        id,
        BinaryFlight { ticket, path, phase: BinaryPhase::Offered(offer), return_to: None, preview_ready: false },
    );
    let (worker, rx) = DocumentWorker::replacement_test_channel(capacity);
    workspace.files.worker = Some(worker);
    workspace.files.storage_ready = true;
    (workspace, rx, offer)
}

#[test]
fn binary_offer_model_mailbox_full_and_disconnect_preserve_offer_and_return_no_source() {
    let (mut workspace, rx, offer) = offered_channel(0);
    assert_eq!(
        workspace.resolve_binary_offer(offer.ticket, Choice::Hex, 1).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    assert_eq!(workspace.binary_offer(), Some(offer));
    assert!(!workspace.hex_owns_document(offer.ticket.document()));
    assert!(!workspace.document_busy(offer.ticket.document()));
    drop(rx);
    assert_eq!(
        workspace.resolve_binary_offer(offer.ticket, Choice::Cancel, 0).unwrap_err().kind(),
        io::ErrorKind::NotConnected
    );
    assert_eq!(workspace.binary_offer(), Some(offer));
}

#[test]
fn binary_offer_model_ticket_document_revision_generation_and_exact_path_fences() {
    let (mut workspace, _rx, offer) = offered_channel(1);
    for ticket in [
        Ticket { document: DocumentId(999), ..offer.ticket },
        Ticket { revision: 1, ..offer.ticket },
        Ticket { generation: 2, ..offer.ticket },
    ] {
        assert_eq!(
            workspace.resolve_binary_offer(ticket, Choice::Hex, 1).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
    }
    workspace.views.get_mut(&offer.ticket.document()).unwrap().path =
        Some(std::env::temp_dir().join("different-binary-path"));
    assert_eq!(
        workspace.resolve_binary_offer(offer.ticket, Choice::Text, 1).unwrap_err().kind(),
        io::ErrorKind::Interrupted
    );
    assert!(workspace.binary_offer().is_none());
}

#[test]
fn binary_offer_model_refused_choice_restores_offer_and_shutdown_retains_cancel_on_disconnect() {
    let (mut workspace, rx, offer) = offered_channel(1);
    workspace.resolve_binary_offer(offer.ticket, Choice::Hex, 2).unwrap();
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        DocumentRequest::ResolveBinary { choice: Choice::Hex, .. }
    ));
    workspace.test_document_result(DocumentResult::BinaryRefused {
        offer,
        error: io::Error::new(io::ErrorKind::WouldBlock, "backing capacity retained"),
    });
    assert_eq!(workspace.binary_offer(), Some(offer));
    assert!(!workspace.hex_owns_document(offer.ticket.document()));
    drop(rx);
    assert!(!workspace.prepare_hex_shutdown());
    assert!(workspace.files.queue.iter().any(|r| matches!(r, DocumentRequest::ResolveBinary { ticket, choice: Choice::Cancel, .. } if *ticket == offer.ticket)));
}

#[test]
fn binary_offer_model_unaccepted_policy_is_not_persisted_as_accepted_text() {
    let (mut workspace, rx, offer) = offered_channel(2);
    workspace.persist_snapshot();
    let DocumentRequest::Persist(snapshot) = rx.recv_timeout(Duration::from_secs(1)).unwrap() else {
        panic!("session snapshot")
    };
    assert!(snapshot.tabs().iter().find(|tab| tab.id == offer.ticket.document()).unwrap().path.is_none());
    assert!(
        workspace.session.tabs().iter().find(|tab| tab.id == offer.ticket.document()).unwrap().path.is_some(),
        "live offer keeps exact path"
    );
}

#[test]
fn binary_offer_model_generation_exhaustion_and_oversized_path_refuse_without_source_read() {
    let (mut workspace, _rx, offer) = offered_channel(1);
    workspace.binary.next_generation = u64::MAX;
    let id = offer.ticket.document();
    workspace.binary.flights.clear();
    assert!(workspace.checked_open_request(id, 0, std::env::temp_dir().join("generation-exhausted")).is_err());
    let oversized = std::env::temp_dir().join("x".repeat(kwil_core_workspace::session::MAX_PATH_BYTES));
    assert!(workspace.checked_open_request(id, 0, oversized).is_err());
    assert!(workspace.binary.flights.is_empty());
}

#[test]
fn binary_offer_model_cancel_returns_to_prior_dirty_tab_without_closed_history_eviction() {
    let (mut workspace, _rx, offer) = offered_channel(1);
    for _ in 0..kwil_core_workspace::session::MAX_CLOSED {
        let id = workspace.session.open(None).unwrap();
        assert!(workspace.session.close(id));
    }
    let closed = workspace.session.closed().clone();
    let other = workspace.open(View::new(None)).unwrap();
    workspace.activate(other);
    workspace
        .execute("edit.insert", &crate::CommandArgs { text: Some("keep unsaved".into()), ..Default::default() }, 1)
        .unwrap();
    workspace.activate(offer.ticket.document());
    let flight = workspace.binary.flights.get_mut(&offer.ticket.document()).unwrap();
    flight.return_to = Some(other);
    // This is the same scalar success produced after worker-side Cancel drop;
    // the real-worker Cancel and cleanup oracle above covers its producer.
    flight.phase = BinaryPhase::Resolving(Choice::Cancel);
    workspace.test_document_result(DocumentResult::CheckedOpened { ticket: offer.ticket, result: Ok(None) });
    assert_eq!(workspace.active_id(), other);
    assert_eq!(logical(&workspace, other), b"keep unsaved");
    assert!(workspace.views[&other].editor().unwrap().document().is_dirty());
    assert_eq!(workspace.session.closed(), &closed, "Cancel must not use normal close's truncating history path");
    assert!(workspace.view(offer.ticket.document()).is_none());
}

#[test]
fn binary_offer_model_restored_accepted_text_and_raw_policy_never_reoffers_startup_duplicate() {
    for choice in [Choice::Text, Choice::Hex] {
        let fixture = Fixture::new();
        let path = fixture.path();
        let (mut workspace, rx) = start(&fixture, View::new(None));
        let id = workspace.open_path(path.clone()).unwrap();
        drive(&mut workspace, &rx, |w| w.binary_offer().is_some());
        let offer = workspace.binary_offer().unwrap();
        workspace.resolve_binary_offer(offer.ticket, choice, 0).unwrap();
        drive(&mut workspace, &rx, |w| !w.binary_offer_pending(id) && w.views[&id].editor().is_some());
        workspace.checkpoint_session();
        drive(&mut workspace, &rx, |w| w.files.metadata_durable && !w.recovery_pending(id));
        stop(workspace, &rx);
        let file = kwil_core_text::TextFile::open(&path).unwrap();
        let (workspace, rx) = start(&fixture, View::new(Some((path, file.clone()))));
        assert!(workspace.binary.flights.is_empty(), "restored policy supersedes unaccepted startup duplicate");
        assert!(workspace.binary_offer().is_none());
        assert_eq!(workspace.hex_active(), choice == Choice::Hex);
        assert_eq!(logical(&workspace, workspace.active_id()), BINARY);
        stop(workspace, &rx);
        assert!(file.try_close().is_ok(), "duplicate source must be retired by the worker, without decoding");
    }
}

#[test]
fn binary_offer_model_detection_after_file_prompt_preserves_modal_ticket_and_offer() {
    let (mut workspace, _rx, offer) = offered_channel(2);
    workspace.binary.flights.get_mut(&offer.ticket.document()).unwrap().phase = BinaryPhase::Opening;
    workspace.file_command("file.open").unwrap();
    let prompt = workspace.file_prompt().unwrap();
    let identity = (prompt.generation, prompt.document, prompt.revision);
    let epoch = workspace.input_epoch();
    workspace.test_document_result(DocumentResult::BinaryOffer(offer));
    assert_eq!(workspace.binary_offer(), Some(offer));
    assert_eq!(
        workspace.resolve_binary_offer(offer.ticket, Choice::Hex, 1).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    let prompt = workspace.file_prompt().unwrap();
    assert_eq!((prompt.generation, prompt.document, prompt.revision), identity);
    assert_eq!(workspace.input_epoch(), epoch);
    assert_eq!(workspace.binary_offer(), Some(offer));
}

#[test]
fn binary_offer_model_vetted_preview_is_ready_while_actual_worker_pauses_before_index_completion() {
    for choice in [None, Some(Choice::Text), Some(Choice::Hex)] {
        let fixture = Fixture::new();
        let path = fixture.path();
        let physical = if choice.is_none() { b"normal first screen\nnext line\n".as_slice() } else { BINARY };
        fs::write(&path, physical).unwrap();
        let file = kwil_core_text::TextFile::open(&path).unwrap();
        let mut workspace = Workspace::new(View::new(Some((path, file.clone())))).unwrap();
        workspace.set_rows(2);
        let (send, receiver) = mpsc::channel();
        let (release, paused) = mpsc::sync_channel(1);
        workspace
            .start_document_worker_at(Some(fixture.0.join("recovery")), move |result| {
                let preview =
                    matches!(&result, DocumentResult::CheckedPreview { .. } | DocumentResult::CheckedRawPreview { .. });
                send.send(result).unwrap();
                if preview {
                    // This is the actual existing worker callback, before decode
                    // backing/index preparation resumes. A finite acknowledgement
                    // proves ordering without any sleeps or large-file timing.
                    paused.recv_timeout(Duration::from_secs(5)).expect("bounded release of real preview callback");
                }
            })
            .unwrap();
        drive(&mut workspace, &receiver, Workspace::recovery_ready);
        let id = workspace.active_id();
        if let Some(choice) = choice {
            drive(&mut workspace, &receiver, |w| w.binary_offer().is_some());
            assert!(!workspace.binary_preview_ready(id), "undecided binary remains blank");
            workspace.resolve_binary_offer(workspace.binary_offer().unwrap().ticket, choice, 2).unwrap();
        }
        drive(&mut workspace, &receiver, |w| w.binary_preview_ready(id));
        assert!(workspace.binary_offer_pending(id), "paint authority cannot release ownership");
        assert!(workspace.document_busy(id));
        assert!(workspace.views[&id].editor().is_none(), "final completion cannot pass the callback barrier");
        assert!(workspace.editor_mut().is_none());
        assert!(workspace.editor_input_ticket(id).is_none());
        assert!(workspace.close(id, true).is_err());
        assert!(file.index().is_none());
        assert_eq!(file.progress().scanned, 0, "full raw index must follow the vetted first screen");
        assert!(
            fs::read_dir(fixture.0.join("recovery")).unwrap().all(|entry| !entry
                .unwrap()
                .file_type()
                .unwrap()
                .is_dir()),
            "full private decode backing must follow the preview callback"
        );
        if choice == Some(Choice::Hex) {
            let session = workspace.hex_session().unwrap();
            assert_eq!(session.row(session.source(), 0).unwrap().bytes, physical);
        } else {
            let expected = &physical[..physical.iter().position(|byte| *byte == b'\n').unwrap()];
            assert_eq!(workspace.line(0).unwrap(), expected);
        }
        release.send(()).unwrap();
        drive(&mut workspace, &receiver, |w| !w.binary_offer_pending(id) && w.views[&id].editor().is_some());
        assert!(!workspace.binary_preview_ready(id), "final editor paint no longer needs flight authority");
        assert_eq!(logical(&workspace, id), physical);
        drop(file);
        stop(workspace, &receiver);
    }
}

#[test]
fn binary_offer_model_preview_authority_rejects_unresolved_cancel_stale_and_malformed_pages() {
    use crate::hex_session::{Page, PageRequest, Representation, Source};
    use kwil_core_text::decoded_backing::DecodedPreview;
    let (mut workspace, _receiver, offer) = offered_channel(4);
    let id = offer.ticket.document();
    let text = |ticket| DocumentResult::CheckedPreview {
        ticket,
        preview: DecodedPreview { text: b"vetted text".to_vec(), source: 0..11, complete: true },
    };
    workspace.test_document_result(text(offer.ticket));
    assert!(!workspace.binary_preview_ready(id), "unresolved choice cannot accept text preview");
    workspace.binary.flights.get_mut(&id).unwrap().phase = BinaryPhase::Opening;
    workspace.test_document_result(text(Ticket { generation: 2, ..offer.ticket }));
    assert!(!workspace.binary_preview_ready(id));
    workspace.test_document_result(text(offer.ticket));
    assert!(workspace.binary_preview_ready(id));
    assert!(workspace.binary_offer_pending(id));
    // Move to a fresh unresolved flight, then exercise the raw route independently.
    workspace.binary.flights.get_mut(&id).unwrap().phase = BinaryPhase::Opening;
    workspace.test_document_result(DocumentResult::BinaryOffer(offer));
    assert!(!workspace.binary_preview_ready(id));
    workspace.resolve_binary_offer(offer.ticket, Choice::Hex, 1).unwrap();
    let page = Page {
        request: PageRequest {
            source: Source { document: id.0, revision: 0, epoch: 1, len: 16, representation: Representation::RawBytes },
            generation: offer.ticket.generation(),
            top: 0,
            rows: 1,
        },
        rows: vec![crate::hex::HexRow { offset: 0, bytes: vec![0x80; 16] }],
    };
    workspace.test_document_result(DocumentResult::CheckedRawPreview {
        ticket: Ticket { generation: 2, ..offer.ticket },
        page: page.clone(),
    });
    assert!(!workspace.binary_preview_ready(id));
    let mut malformed = page.clone();
    malformed.rows[0].offset = 16;
    workspace.test_document_result(DocumentResult::CheckedRawPreview { ticket: offer.ticket, page: malformed });
    assert!(!workspace.binary_preview_ready(id), "raw cache must accept the page before paint authority");
    workspace.test_document_result(DocumentResult::CheckedRawPreview { ticket: offer.ticket, page: page.clone() });
    assert!(workspace.binary_preview_ready(id));
    workspace.test_document_result(DocumentResult::BinaryRefused {
        offer,
        error: io::Error::new(io::ErrorKind::WouldBlock, "capacity retained"),
    });
    assert_eq!(workspace.binary_offer(), Some(offer));
    assert!(!workspace.binary_preview_ready(id), "refused admission removes the old preview authority");
    workspace.resolve_binary_offer(offer.ticket, Choice::Cancel, 0).unwrap();
    workspace.test_document_result(text(offer.ticket));
    workspace.test_document_result(DocumentResult::CheckedRawPreview { ticket: offer.ticket, page });
    assert!(!workspace.binary_preview_ready(id), "Cancel never permits preview paint");
    assert!(workspace.binary_offer_pending(id));
}
