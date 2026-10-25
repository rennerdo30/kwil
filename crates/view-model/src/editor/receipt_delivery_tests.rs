//! Authored source controls; execution belongs to ROOT.
use super::receipts::ReceiptDeliveryRefusal;
use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[test]
fn sealed_flush_refuses_unknown_observer_without_invoking_or_losing_receipt() {
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let mut editor = Editor::new(Document::new());
    editor
        .set_edit_observer(Some(Box::new(move |receipt| {
            observed.fetch_add(1, Ordering::SeqCst);
            Err(receipt)
        })))
        .unwrap();
    editor.insert_text("retained", Duration::ZERO).unwrap();
    let sequence = editor.pending_edit_sequence();
    let owner = editor.receipts.pending.as_ref().unwrap() as *const receipts::EditReceipt;
    assert!(matches!(editor.flush_edit_receipt_sealed(None), Err(ReceiptDeliveryRefusal::UnsupportedObserver)));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(editor.pending_edit_sequence(), sequence);
    assert_eq!(editor.receipts.pending.as_ref().unwrap() as *const receipts::EditReceipt, owner);
    assert!(editor.has_pending_edit_receipt());
}

#[test]
fn sealed_flush_does_not_enter_panicking_observer() {
    let mut editor = Editor::new(Document::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    editor
        .set_edit_observer(Some(Box::new(move |receipt| {
            if observed.fetch_add(1, Ordering::SeqCst) != 0 {
                panic!("must not run during sealed settlement");
            }
            Err(receipt)
        })))
        .unwrap();
    editor.insert_text("exact", Duration::ZERO).unwrap();
    let sequence = editor.pending_edit_sequence();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| editor.flush_edit_receipt_sealed(None)));
    assert!(matches!(result, Ok(Err(ReceiptDeliveryRefusal::UnsupportedObserver))));
    assert_eq!(editor.pending_edit_sequence(), sequence);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn closed_publication_lane_keeps_exact_receipt_and_durability_target() {
    let mut editor = Editor::new(Document::new());
    let sink = crate::document_worker::DocumentJournalSink::closed_for_test();
    assert!(editor.set_journal_sink(sink).is_ok());
    editor.insert_text("exact pending owner", Duration::ZERO).unwrap();
    let pending = editor.receipts.pending.as_ref().unwrap();
    let owner = pending as *const receipts::EditReceipt;
    let identity = (pending.sequence, pending.before_revision, pending.after_revision);
    let mut before = Vec::new();
    pending.after.write_to(&mut before).unwrap();
    assert!(matches!(editor.flush_edit_receipt_sealed(None), Err(ReceiptDeliveryRefusal::Closed)));
    let retained = editor.receipts.pending.as_ref().unwrap();
    assert_eq!(retained as *const receipts::EditReceipt, owner);
    assert_eq!((retained.sequence, retained.before_revision, retained.after_revision), identity);
    let mut after = Vec::new();
    retained.after.write_to(&mut after).unwrap();
    assert_eq!(after, before);
    assert_eq!(editor.pending_edit_sequence(), Some(identity.0), "unsubmitted exact journal target remains required");
    assert_eq!(editor.insert_text("blocked", Duration::ZERO).unwrap_err().kind(), io::ErrorKind::WouldBlock);
}
