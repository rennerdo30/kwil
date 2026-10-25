//! Tiny authored controls. ROOT owns execution and source retirement context.
use super::save::{SaveCompletionState, SavePreparationRefusal, SaveRetentionRefusal};
use super::*;
use crate::visual_rows::AllocationBudget;
use kwil_core_workspace::{SavedSnapshot, save_snapshot};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        for _ in 0..128 {
            let path = std::env::temp_dir().join(format!(
                "kwil-save-completion-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("tiny fixture: {e}"),
            }
        }
        panic!("exclusive tiny fixture namespace exhausted");
    }
    fn save(&self, editor: &mut Editor, text: &str) -> SavedSnapshot {
        editor.insert_text(text, Duration::ZERO).unwrap();
        save_snapshot(editor.save_request(Some(self.0.join("saved.txt"))).unwrap()).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn bytes(editor: &Editor) -> Vec<u8> {
    let mut result = Vec::new();
    editor.document().snapshot().write_to(&mut result).unwrap();
    result
}

#[test]
fn sealed_transformed_missing_recipe_keeps_every_cursor_and_saved_owner() {
    let fixture = Fixture::new();
    let mut editor = Editor::new(Document::new());
    let mut saved = fixture.save(&mut editor, "abcdef");
    saved.trimmed_rows = Some(0..1);
    let mut state = SaveCompletionState::new(saved);
    let cursors = editor.cursors.clone();
    let generation = editor.external_cursor_generation().unwrap();
    let history = editor.document.buffer.history_depth();
    assert!(editor.prepare_save_completion(&mut state, &AllocationBudget::default()).is_err());
    assert!(!state.is_committed());
    assert_eq!(editor.cursors, cursors);
    assert_eq!(editor.external_cursor_generation().unwrap(), generation);
    assert_eq!(editor.document.buffer.history_depth(), history);
    assert_eq!(bytes(&editor), b"abcdef");
    // The same residual can be retried: it was not consumed on refusal.
    assert!(editor.prepare_save_completion(&mut state, &AllocationBudget::default()).is_err());
}

#[test]
fn sealed_source_read_failure_does_not_publish_partial_cursor_remap() {
    let fixture = Fixture::new();
    let mut editor = Editor::new(Document::new());
    let mut saved = fixture.save(&mut editor, "abcdef");
    saved.normalized = true;
    editor.set_cursors(vec![Cursor::caret(1)], 0).unwrap();
    // Saved versions are mapped even when tiny. Windows forbids truncating a
    // live map, but its shared-delete handle permits this exact path rotation.
    let held = fixture.0.join("held-saved.txt");
    let owner = saved.file.path() as *const std::path::Path;
    assert_eq!(saved.file.check().unwrap(), kwil_core_text::FileState::Unchanged);
    fs::rename(&saved.path, &held).unwrap();
    assert_eq!(saved.file.check().unwrap(), kwil_core_text::FileState::Replaced);
    let source_error = text::byte(&saved.snapshot, 1).unwrap_err();
    assert!(
        matches!(
            source_error.get_ref().and_then(|error| error.downcast_ref::<kwil_core_text::ReadError>()),
            Some(kwil_core_text::ReadError::Changed(kwil_core_text::FileState::Replaced))
        ),
        "negative control: the owned mapped source really refuses reads"
    );
    let mut state = SaveCompletionState::new(saved);
    let cursors = editor.cursors.clone();
    let generation = editor.external_cursor_generation().unwrap();
    let revision = editor.document.revision;
    let history = editor.document.buffer.history_depth();
    let budget = AllocationBudget::default();
    let before = budget.retained_bytes();
    let result = editor.prepare_save_completion(&mut state, &budget);
    assert!(result.is_err());
    let error = result.unwrap_err();
    assert!(
        matches!(
            error.get_ref().and_then(|error| error.downcast_ref::<kwil_core_text::ReadError>()),
            Some(kwil_core_text::ReadError::Changed(kwil_core_text::FileState::Replaced))
        ),
        "preparation must reach the real saved-source read failure"
    );
    assert!(!state.is_committed());
    assert_eq!(editor.cursors, cursors);
    assert_eq!(editor.external_cursor_generation().unwrap(), generation);
    assert_eq!(editor.document.revision, revision);
    assert_eq!(bytes(&editor), b"abcdef");
    assert_eq!(editor.document.buffer.history_depth(), history);
    assert_eq!(state.saved.file.path() as *const std::path::Path, owner);
    assert_eq!(budget.retained_bytes(), before, "refused preparation releases only its temporary metadata");
    fs::rename(&held, &state.saved.path).unwrap();
    assert_eq!(state.saved.file.check().unwrap(), kwil_core_text::FileState::Unchanged);
    assert_eq!(text::byte(&state.saved.snapshot, 1).unwrap(), Some(b'b'));
}

#[test]
fn prepared_save_rejects_stale_cursor_generation_before_core_publication() {
    let fixture = Fixture::new();
    let mut editor = Editor::new(Document::new());
    let saved = fixture.save(&mut editor, "retained");
    let mut state = SaveCompletionState::new(saved);
    let budget = AllocationBudget::default();
    editor.prepare_save_completion(&mut state, &budget).unwrap();
    editor.normalize();
    let generation = editor.external_cursor_generation().unwrap();
    assert!(editor.commit_save_completion(&mut state).is_err());
    assert_eq!(editor.external_cursor_generation().unwrap(), generation);
    assert!(editor.document.original.is_none());
    assert!(!state.is_committed());
}

#[test]
fn saved_preparation_is_charged_and_old_sources_retire_after_installation() {
    let fixture = Fixture::new();
    let mut editor = Editor::new(Document::new());
    let saved = fixture.save(&mut editor, "first");
    let mut state = SaveCompletionState::new(saved);
    let budget = AllocationBudget::default();
    let empty = budget.retained_bytes();
    editor.prepare_save_completion(&mut state, &budget).unwrap();
    let preparing = budget.retained_bytes();
    assert!(preparing > empty);
    editor.commit_save_completion(&mut state).unwrap();
    assert!(state.is_committed());
    assert_eq!(budget.retained_bytes(), preparing, "publication retains retirement and live metadata charges");
    assert!(editor.commit_save_completion(&mut state).is_err(), "completed phases never replay");
    drop(state); // test represents the explicit off-UI retirement boundary
    assert!(budget.retained_bytes() < preparing);
    assert!(budget.retained_bytes() > empty, "Editor keeps installed metadata charged");
    assert!(!editor.document.is_dirty());
    drop(editor);
    assert_eq!(budget.retained_bytes(), empty);
}

#[test]
fn sealed_preparation_budget_refusal_keeps_original_owner_retryable() {
    let fixture = Fixture::new();
    let mut editor = Editor::new(Document::new());
    let saved = fixture.save(&mut editor, "exact");
    let mut state = SaveCompletionState::new(saved);
    assert!(editor.prepare_save_completion(&mut state, &AllocationBudget::new(0)).is_err());
    assert!(!state.is_committed());
    assert!(editor.document.original.is_none());
    editor.prepare_save_completion(&mut state, &AllocationBudget::default()).unwrap();
    editor.commit_save_completion(&mut state).unwrap();
    assert_eq!(bytes(&editor), b"exact");
}

#[test]
fn panic_after_core_installation_keeps_monotonic_residual_and_never_replays() {
    let fixture = Fixture::new();
    let mut editor = Editor::new(Document::new());
    let mut saved = fixture.save(&mut editor, "exact");
    saved.normalized = true;
    let mut state = SaveCompletionState::new(saved);
    let budget = AllocationBudget::default();
    editor.prepare_save_completion(&mut state, &budget).unwrap();
    let charged = budget.retained_bytes();
    let old_revision = editor.document.revision;
    let old_generation = editor.external_cursor_generation().unwrap();
    state.panic_after_core();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| editor.commit_save_completion(&mut state)));
    assert!(outcome.is_err());
    assert!(state.publication_started());
    assert!(
        editor.external_cursor_generation().unwrap() > old_generation,
        "partially installed source invalidates external navigation"
    );
    assert!(!state.is_committed());
    assert_eq!(editor.document.revision, old_revision, "remaining model fields have not been published");
    assert!(editor.document.original.is_none());
    assert_eq!(budget.retained_bytes(), charged, "partial residual keeps all charges");
    assert!(editor.commit_save_completion(&mut state).is_err(), "no automatic partial-handler replay");
    assert_eq!(bytes(&editor), b"exact");
}

#[test]
fn deferred_retry_errors_and_observer_unwind_restore_exact_saved_state() {
    let fixture = Fixture::new();
    let mut editor = Editor::new(Document::new());
    editor.insert_text("pending", Duration::ZERO).unwrap();
    editor.set_edit_observer(Some(Box::new(Err))).unwrap();
    editor.insert_text(" retained", Duration::ZERO).unwrap();
    let request = editor.save_request(Some(fixture.0.join("saved.txt"))).unwrap();
    let mut saved = save_snapshot(request).unwrap();
    saved.normalized = true;
    let expected_path = saved.path.clone();
    assert!(
        editor.finish_save(Ok(saved)).continue_value().expect("ordinary backpressure retains guarded save").is_err()
    );
    let sequence = editor.pending_edit_sequence();
    assert!(editor.finish_pending_save_sealed(&AllocationBudget::default()).is_err());
    assert!(editor.has_pending_save_completion());
    assert_eq!(editor.receipts.deferred_save.as_ref().unwrap().saved.path, expected_path);
    assert_eq!(editor.pending_edit_sequence(), sequence);
    // Ordinary observer compatibility can unwind. The deferred saved owner
    // still has a guard; shutdown never invokes this callback in the first place.
    editor.replace_observer_for_test(Some(Box::new(|_| panic!("ordinary retry observer")))).unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| editor.finish_pending_save()));
    assert!(result.is_err());
    assert!(editor.has_pending_save_completion());
    assert_eq!(editor.receipts.deferred_save.as_ref().unwrap().saved.path, expected_path);
}

#[test]
fn transformed_save_refuses_unknown_observer_before_publication() {
    let fixture = Fixture::new();
    let mut editor = Editor::new(Document::new());
    let mut saved = fixture.save(&mut editor, "retained");
    saved.normalized = true;
    let mut state = SaveCompletionState::new(saved);
    let owner = state.saved.file.path() as *const std::path::Path;
    editor.set_edit_observer(Some(Box::new(|_| panic!("unknown observer must remain uncalled")))).unwrap();
    let revision = editor.document.revision;
    let budget = AllocationBudget::default();
    let before = budget.retained_bytes();
    let error = editor.prepare_save_completion(&mut state, &budget).unwrap_err();
    assert_eq!(
        error.get_ref().unwrap().downcast_ref::<receipts::ReceiptDeliveryRefusal>(),
        Some(&receipts::ReceiptDeliveryRefusal::UnsupportedObserver)
    );
    assert_eq!(state.saved.file.path() as *const std::path::Path, owner);
    assert_eq!(editor.document.revision, revision);
    assert!(!state.publication_started());
    assert_eq!(budget.retained_bytes(), before);
}

#[test]
fn panic_before_publication_retains_exact_saved_owner_and_prepared_charge() {
    let fixture = Fixture::new();
    let mut editor = Editor::new(Document::new());
    let mut saved = fixture.save(&mut editor, "retained");
    saved.normalized = true;
    let mut state = SaveCompletionState::new(saved);
    let owner = state.saved.file.path() as *const std::path::Path;
    let budget = AllocationBudget::default();
    editor.prepare_save_completion(&mut state, &budget).unwrap();
    let charged = budget.retained_bytes();
    let cursors = editor.cursors.clone();
    let history = editor.document.buffer.history_depth();
    state.panic_before_publication();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| editor.commit_save_completion(&mut state)));
    assert!(outcome.is_err());
    assert!(!state.publication_started());
    assert_eq!(state.saved.file.path() as *const std::path::Path, owner);
    assert_eq!(editor.cursors, cursors);
    assert_eq!(editor.document.buffer.history_depth(), history);
    assert_eq!(budget.retained_bytes(), charged);
}

#[test]
fn sealed_trim_installs_one_exact_receipt_while_closed_lane_keeps_durability_pending() {
    let fixture = Fixture::new();
    let mut editor = Editor::new(Document::new());
    editor.insert_text("a  \nb\t\nlast ", Duration::ZERO).unwrap();
    editor.settings.editor.trim_trailing_whitespace_on_save = true;
    assert!(editor.set_journal_sink(crate::document_worker::DocumentJournalSink::closed_for_test()).is_ok());
    let before = editor.document.revision;
    let saved = save_snapshot(editor.save_request(Some(fixture.0.join("saved.txt"))).unwrap()).unwrap();
    assert!(saved.trimmed_rows.is_some());
    let mut state = SaveCompletionState::new(saved);
    let budget = AllocationBudget::default();
    editor.prepare_save_completion(&mut state, &budget).unwrap();
    editor.commit_save_completion(&mut state).unwrap();
    assert!(state.is_committed());
    assert_eq!(bytes(&editor), b"a\nb\nlast");
    assert!(!editor.document.is_dirty(), "disk save completed even while journal admission is pending");
    let receipt = editor.receipts.pending.as_ref().unwrap();
    assert_eq!(receipt.kind, EditKind::WorkerTrim);
    assert_eq!(
        (receipt.sequence, receipt.before_revision, receipt.after_revision),
        (1, before, editor.document.revision)
    );
    let mut exact = Vec::new();
    receipt.after.write_to(&mut exact).unwrap();
    assert_eq!(exact, b"a\nb\nlast");
    assert!(matches!(editor.flush_edit_receipt_sealed(None), Err(receipts::ReceiptDeliveryRefusal::Closed)));
    assert_eq!(editor.pending_edit_sequence(), Some(1), "clean file is not a journal durability acknowledgement");
    assert_eq!(editor.undo().unwrap_err().kind(), io::ErrorKind::WouldBlock);
}

#[test]
fn retain_save_moves_prepared_owner_and_charges_into_existing_gated_slot() {
    let fixture = Fixture::new();
    let mut editor = Editor::new(Document::new());
    let saved = fixture.save(&mut editor, "retained");
    let mut incoming = Some(SaveCompletionState::new(saved));
    let budget = AllocationBudget::default();
    incoming.as_mut().unwrap().admit_retention(&budget).unwrap();
    editor.prepare_save_completion(incoming.as_mut().unwrap(), &budget).unwrap();
    let charged = budget.retained_bytes();
    let owner = incoming.as_ref().unwrap().saved.file.path() as *const std::path::Path;
    editor.retain_save_completion(&mut incoming).unwrap();
    assert!(incoming.is_none());
    assert_eq!(editor.receipts.deferred_save.as_ref().unwrap().saved.file.path() as *const std::path::Path, owner);
    assert!(editor.has_pending_save_completion());
    assert_eq!(budget.retained_bytes(), charged, "metadata charges move with the exact guarded state");
    assert_eq!(editor.insert_text("blocked", Duration::ZERO).unwrap_err().kind(), io::ErrorKind::WouldBlock);
    // Represents one explicit capacity event; the API adds no polling/retry.
    assert!(editor.finish_pending_save_sealed(&budget).unwrap().is_some());
    assert!(!editor.has_pending_save_completion());
    assert!(editor.receipts.deferred_save.as_ref().unwrap().is_committed());
    assert_eq!(budget.retained_bytes(), charged, "completed residual still awaits off-UI retirement");
    assert!(editor.has_completed_save_completion());
    assert!(matches!(
        editor.finish_save(Err(io::Error::other("ordinary source-free failure"))),
        std::ops::ControlFlow::Continue(Err(_))
    ));
    assert_eq!(
        editor.save_request(None).err().expect("completed residual gates save").kind(),
        io::ErrorKind::WouldBlock
    );
    let mut retirement = None;
    editor.take_completed_save_completion(&mut retirement).unwrap();
    assert!(retirement.as_ref().unwrap().is_committed());
    assert!(!editor.has_completed_save_completion());
    assert_eq!(budget.retained_bytes(), charged, "handoff into the original result slot releases no charge");
    drop(retirement); // represents existing worker retirement, never frontend Drop
    assert!(budget.retained_bytes() < charged);
}

#[test]
fn retain_save_empty_and_occupied_refusals_preserve_both_exact_owners() {
    let fixture = Fixture::new();
    let second_fixture = Fixture::new();
    let mut editor = Editor::new(Document::new());
    let mut empty = None;
    assert_eq!(editor.retain_save_completion(&mut empty), Err(SaveRetentionRefusal::EmptyInput));
    assert!(editor.receipts.deferred_save.is_none());
    let mut first = Some(SaveCompletionState::new(fixture.save(&mut editor, "first")));
    first.as_mut().unwrap().admit_retention(&AllocationBudget::default()).unwrap();
    let first_owner = first.as_ref().unwrap().saved.file.path() as *const std::path::Path;
    editor.retain_save_completion(&mut first).unwrap();
    let mut second_editor = Editor::new(Document::new());
    let mut second = Some(SaveCompletionState::new(second_fixture.save(&mut second_editor, "second")));
    let second_owner = second.as_ref().unwrap().saved.file.path() as *const std::path::Path;
    assert_eq!(editor.retain_save_completion(&mut second), Err(SaveRetentionRefusal::Occupied));
    assert_eq!(
        editor.receipts.deferred_save.as_ref().unwrap().saved.file.path() as *const std::path::Path,
        first_owner
    );
    assert_eq!(second.as_ref().unwrap().saved.file.path() as *const std::path::Path, second_owner);
    assert!(!second.as_ref().unwrap().publication_started());
    assert!(editor.has_pending_save_completion());
}

#[test]
fn retain_save_refuses_partial_and_completed_publication_without_replaying() {
    let fixture = Fixture::new();
    let mut editor = Editor::new(Document::new());
    let mut saved = fixture.save(&mut editor, "partial");
    saved.normalized = true;
    let mut incoming = Some(SaveCompletionState::new(saved));
    let budget = AllocationBudget::default();
    editor.prepare_save_completion(incoming.as_mut().unwrap(), &budget).unwrap();
    incoming.as_mut().unwrap().panic_after_core();
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(
            || editor.commit_save_completion(incoming.as_mut().unwrap())
        ))
        .is_err()
    );
    let owner = incoming.as_ref().unwrap().saved.file.path() as *const std::path::Path;
    let charged = budget.retained_bytes();
    assert_eq!(editor.retain_save_completion(&mut incoming), Err(SaveRetentionRefusal::PublicationStarted));
    assert!(editor.receipts.deferred_save.is_none());
    assert_eq!(incoming.as_ref().unwrap().saved.file.path() as *const std::path::Path, owner);
    assert_eq!(budget.retained_bytes(), charged);
    assert!(!incoming.as_ref().unwrap().is_committed());

    let complete_fixture = Fixture::new();
    let mut complete_editor = Editor::new(Document::new());
    let saved = complete_fixture.save(&mut complete_editor, "complete");
    let mut complete = Some(SaveCompletionState::new(saved));
    complete_editor.prepare_save_completion(complete.as_mut().unwrap(), &budget).unwrap();
    complete_editor.commit_save_completion(complete.as_mut().unwrap()).unwrap();
    assert_eq!(complete_editor.retain_save_completion(&mut complete), Err(SaveRetentionRefusal::PublicationStarted));
    assert!(complete.as_ref().unwrap().is_committed());
    assert!(complete_editor.receipts.deferred_save.is_none());
}

#[test]
fn refused_and_abandoned_preparation_never_grow_live_model_history_capacity() {
    let fixture = Fixture::new();
    let mut editor = Editor::new(Document::new());
    for text in ["a", "b", "c", "d"] {
        editor.insert_text(text, Duration::ZERO).unwrap();
        editor.end_burst();
    }
    let capacity = (editor.undo.capacity(), editor.redo.capacity());
    let mut saved = save_snapshot(editor.save_request(Some(fixture.0.join("saved.txt"))).unwrap()).unwrap();
    saved.normalized = true;
    let mut alien = Buffer::empty();
    alien.insert(0, b"abcd").unwrap();
    let valid = saved.snapshot.clone();
    saved.snapshot = alien.snapshot();
    let mut state = SaveCompletionState::new(saved);
    let budget = AllocationBudget::default();
    let empty = budget.retained_bytes();
    assert!(editor.prepare_save_completion(&mut state, &budget).is_err(), "non-original saved roots must refuse");
    assert_eq!((editor.undo.capacity(), editor.redo.capacity()), capacity);
    assert_eq!(budget.retained_bytes(), empty, "refusal releases every temporary allocation charge");
    state.saved.snapshot = valid;
    editor.prepare_save_completion(&mut state, &budget).unwrap();
    assert_eq!((editor.undo.capacity(), editor.redo.capacity()), capacity);
    drop(state);
    assert_eq!((editor.undo.capacity(), editor.redo.capacity()), capacity, "abandoned preparation owns its own deques");
    assert_eq!(budget.retained_bytes(), empty);
}

#[test]
fn persistent_save_header_admission_is_required_and_moves_with_exact_owner() {
    let fixture = Fixture::new();
    let mut editor = Editor::new(Document::new());
    let mut incoming = Some(SaveCompletionState::new(fixture.save(&mut editor, "retained")));
    let owner = incoming.as_ref().unwrap().saved.file.path() as *const std::path::Path;
    assert_eq!(editor.retain_save_completion(&mut incoming), Err(SaveRetentionRefusal::AdmissionRequired));
    assert!(incoming.as_mut().unwrap().admit_retention(&AllocationBudget::new(0)).is_err());
    assert_eq!(incoming.as_ref().unwrap().saved.file.path() as *const std::path::Path, owner);
    assert!(editor.receipts.deferred_save.is_none());
    let budget = AllocationBudget::default();
    let before = budget.retained_bytes();
    incoming.as_mut().unwrap().admit_retention(&budget).unwrap();
    let charged = budget.retained_bytes();
    assert!(charged > before);
    editor.retain_save_completion(&mut incoming).unwrap();
    assert_eq!(budget.retained_bytes(), charged);
    let mut destination = None;
    assert_eq!(editor.take_completed_save_completion(&mut destination), Err(SaveRetentionRefusal::NotCompleted));
    assert!(destination.is_none());
    assert_eq!(editor.receipts.deferred_save.as_ref().unwrap().saved.file.path() as *const std::path::Path, owner);
}

#[test]
fn completed_residual_refuses_new_public_save_with_both_exact_owners_intact() {
    let fixture = Fixture::new();
    let second_fixture = Fixture::new();
    let mut editor = Editor::new(Document::new());
    let saved = fixture.save(&mut editor, "first");
    let original = saved.file.path() as *const std::path::Path;
    assert!(matches!(editor.finish_save(Ok(saved)), std::ops::ControlFlow::Continue(Ok(_))));
    assert!(editor.has_completed_save_completion());
    assert_eq!(
        editor.save_request(None).err().expect("completed residual gates save").kind(),
        io::ErrorKind::WouldBlock
    );
    let mut second_editor = Editor::new(Document::new());
    let second = second_fixture.save(&mut second_editor, "second");
    let incoming = second.file.path() as *const std::path::Path;
    let refusal = match editor.finish_save(Ok(second)) {
        std::ops::ControlFlow::Break(refusal) => refusal,
        _ => panic!("occupied completed residual must preserve the new owner in its refusal"),
    };
    assert_eq!(refusal.reason, SaveCompletionRefusalReason::Occupied);
    assert_eq!(refusal.result.as_ref().unwrap().file.path() as *const std::path::Path, incoming);
    assert_eq!(editor.receipts.deferred_save.as_ref().unwrap().saved.file.path() as *const std::path::Path, original);
    let mut occupied = Some(SaveCompletionState::new(refusal.result.unwrap()));
    assert_eq!(editor.take_completed_save_completion(&mut occupied), Err(SaveRetentionRefusal::Occupied));
    assert!(editor.has_completed_save_completion());
    let mut retired = None;
    editor.take_completed_save_completion(&mut retired).unwrap();
    assert_eq!(retired.as_ref().unwrap().saved.file.path() as *const std::path::Path, original);
    assert!(editor.receipts.deferred_save.is_none());
}

#[test]
fn ordinary_and_sealed_trim_share_cursor_normalization_reveal_and_damage() {
    let first_fixture = Fixture::new();
    let second_fixture = Fixture::new();
    let mut ordinary = Editor::new(Document::new());
    let mut sealed = Editor::new(Document::new());
    for editor in [&mut ordinary, &mut sealed] {
        editor.insert_text("first  \nsecond\t\nlast ", Duration::ZERO).unwrap();
        let end = editor.document.buffer.len();
        editor.set_cursors(vec![Cursor { anchor: 3, head: 7, desired_column: None }, Cursor::caret(end)], 0).unwrap();
        editor.set_viewport(1, 2);
        editor.settings.editor.trim_trailing_whitespace_on_save = true;
    }
    let saved = save_snapshot(ordinary.save_request(Some(first_fixture.0.join("saved.txt"))).unwrap()).unwrap();
    let ordinary_result =
        ordinary.finish_save(Ok(saved)).continue_value().expect("ordinary completion succeeds").unwrap();
    let saved = save_snapshot(sealed.save_request(Some(second_fixture.0.join("saved.txt"))).unwrap()).unwrap();
    let old_rows = sealed.cursor_rows().unwrap();
    let old_top = sealed.top;
    let edited = saved.trimmed_rows.clone();
    let mut state = SaveCompletionState::new(saved);
    sealed.prepare_save_completion(&mut state, &AllocationBudget::default()).unwrap();
    let sealed_result = sealed.commit_save_completion(&mut state).unwrap();
    assert_eq!(ordinary.cursors, sealed.cursors);
    assert_eq!(ordinary.primary, sealed.primary);
    assert_eq!(ordinary.top, sealed.top);
    assert_eq!(ordinary_result.damage, sealed_result.damage);
    let mut legacy_top = old_top;
    let legacy_damage = evaluate_cursor_damage(
        &sealed.document.snapshot(),
        &sealed.cursors,
        sealed.primary,
        &mut legacy_top,
        sealed.rows,
        CursorDamageBefore::Rows(&old_rows),
        edited.as_ref(),
    )
    .unwrap();
    assert_eq!(
        legacy_damage, sealed_result.damage,
        "the ordinary edit caller's row input uses the same damage evaluator"
    );
    assert_eq!(legacy_top, sealed.top);
    assert_eq!(bytes(&ordinary), bytes(&sealed));
}

fn assert_stale_preparation(error: io::Error) {
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(
        error.get_ref().and_then(|error| error.downcast_ref::<SavePreparationRefusal>()),
        Some(&SavePreparationRefusal::StaleIdentity),
    );
}

#[test]
fn retained_prepared_cursor_change_transfers_exact_owner_for_worker_reset_and_retry() {
    let fixture = Fixture::new();
    let mut editor = Editor::new(Document::new());
    editor.settings.editor.trim_trailing_whitespace_on_save = true;
    let saved = fixture.save(&mut editor, "a  \nb\t\nlast ");
    let owner = saved.file.path() as *const std::path::Path;
    let mut incoming = Some(SaveCompletionState::new(saved));
    let budget = AllocationBudget::default();
    incoming.as_mut().unwrap().admit_retention(&budget).unwrap();
    let header_charge = budget.retained_bytes();
    editor.prepare_save_completion(incoming.as_mut().unwrap(), &budget).unwrap();
    let prepared_charge = budget.retained_bytes();
    assert!(prepared_charge > header_charge);
    editor.retain_save_completion(&mut incoming).unwrap();
    editor.set_cursors(vec![Cursor::caret(4)], 0).unwrap();
    let generation = editor.external_cursor_generation().unwrap();
    let history = editor.document.buffer.history_depth();
    for _ in 0..2 {
        assert_stale_preparation(editor.finish_pending_save_sealed(&budget).unwrap_err());
        assert_eq!(editor.external_cursor_generation().unwrap(), generation);
        assert_eq!(editor.document.buffer.history_depth(), history);
        assert_eq!(editor.cursors, vec![Cursor::caret(4)]);
        assert_eq!(bytes(&editor), b"a  \nb\t\nlast ");
        assert_eq!(budget.retained_bytes(), prepared_charge, "stale retry keeps all guarded allocations");
        assert_eq!(editor.receipts.deferred_save.as_ref().unwrap().saved.file.path() as *const std::path::Path, owner);
    }
    assert_eq!(editor.insert_text("blocked", Duration::ZERO).unwrap_err().kind(), io::ErrorKind::WouldBlock);
    assert_eq!(editor.save_request(None).err().unwrap().kind(), io::ErrorKind::WouldBlock);
    assert_eq!(editor.take_completed_save_completion(&mut incoming), Err(SaveRetentionRefusal::NotCompleted));
    editor.take_unpublished_save_completion(&mut incoming).unwrap();
    assert!(!editor.has_pending_save_completion());
    assert_eq!(budget.retained_bytes(), prepared_charge, "transfer releases no owner or metadata charge");
    assert_eq!(incoming.as_ref().unwrap().saved.file.path() as *const std::path::Path, owner);
    // This test already runs on an authority/test worker. Production sends the
    // exact original result-slot state back to its existing worker first.
    incoming.as_mut().unwrap().reset_preparation_for_retry().unwrap();
    assert_eq!(budget.retained_bytes(), header_charge, "only preparation retires off UI");
    assert_eq!(incoming.as_ref().unwrap().saved.file.path() as *const std::path::Path, owner);
    assert!(editor.prepare_save_completion(incoming.as_mut().unwrap(), &AllocationBudget::new(0)).is_err());
    assert_eq!(budget.retained_bytes(), header_charge);
    editor.prepare_save_completion(incoming.as_mut().unwrap(), &budget).unwrap();
    editor.retain_save_completion(&mut incoming).unwrap();
    assert!(editor.finish_pending_save_sealed(&budget).unwrap().is_some());
    assert!(editor.has_completed_save_completion());
    assert_eq!(editor.cursors, vec![Cursor::caret(2)], "retry remaps the current cursor");
    assert!(editor.external_cursor_generation().unwrap() > generation);
    assert_eq!(bytes(&editor), b"a\nb\nlast");
    assert_eq!(editor.document.buffer.history_depth().0, history.0 + 1);
    let mut retirement = None;
    editor.take_completed_save_completion(&mut retirement).unwrap();
    assert_eq!(retirement.as_ref().unwrap().saved.file.path() as *const std::path::Path, owner);
    drop(retirement); // explicit retirement on this test worker
    editor.undo().unwrap();
    assert_eq!(bytes(&editor), b"a  \nb\t\nlast ");
    assert_eq!(editor.cursors, vec![Cursor::caret(4)]);
}

#[test]
fn prepared_viewport_change_fences_commit_and_finish_until_worker_reset_then_recomputes_damage() {
    for (top, rows) in [(3, 2), (0, 1), (3, 1)] {
        let fixture = Fixture::new();
        let mut editor = Editor::new(Document::new());
        editor.settings.editor.trim_trailing_whitespace_on_save = true;
        let saved = fixture.save(&mut editor, "a  \nb  \nc  \nd  \ne  \nf  \ng  \nh  ");
        editor.set_cursors(vec![Cursor::caret(12)], 0).unwrap();
        editor.set_viewport(0, 2);
        let owner = saved.file.path() as *const std::path::Path;
        let mut incoming = Some(SaveCompletionState::new(saved));
        let budget = AllocationBudget::default();
        incoming.as_mut().unwrap().admit_retention(&budget).unwrap();
        let header_charge = budget.retained_bytes();
        editor.prepare_save_completion(incoming.as_mut().unwrap(), &budget).unwrap();
        let prepared_charge = budget.retained_bytes();
        let generation = editor.external_cursor_generation().unwrap();
        let history = editor.document.buffer.history_depth();
        editor.set_viewport(top, rows);
        assert_eq!(editor.external_cursor_generation().unwrap(), generation, "viewport has its own identity fence");
        assert_stale_preparation(editor.commit_save_completion(incoming.as_mut().unwrap()).unwrap_err());
        editor.retain_save_completion(&mut incoming).unwrap();
        assert_stale_preparation(editor.finish_pending_save_sealed(&budget).unwrap_err());
        assert_eq!((editor.top, editor.rows), (top, rows), "stale staging never overwrites the current viewport");
        assert_eq!(editor.document.buffer.history_depth(), history);
        assert_eq!(bytes(&editor), b"a  \nb  \nc  \nd  \ne  \nf  \ng  \nh  ");
        assert_eq!(budget.retained_bytes(), prepared_charge);
        editor.take_unpublished_save_completion(&mut incoming).unwrap();
        assert_eq!(budget.retained_bytes(), prepared_charge);
        assert_eq!(incoming.as_ref().unwrap().saved.file.path() as *const std::path::Path, owner);
        incoming.as_mut().unwrap().reset_preparation_for_retry().unwrap(); // this test worker is off UI
        assert_eq!(budget.retained_bytes(), header_charge);
        editor.prepare_save_completion(incoming.as_mut().unwrap(), &budget).unwrap();
        editor.retain_save_completion(&mut incoming).unwrap();
        let outcome = editor.finish_pending_save_sealed(&budget).unwrap().unwrap();
        assert_eq!((editor.top, editor.rows), (3, rows));
        assert_eq!(editor.cursors, vec![Cursor::caret(6)]);
        assert_eq!(bytes(&editor), b"a\nb\nc\nd\ne\nf\ng\nh");
        assert_eq!(outcome.damage.full, top == 0);
        if top == 3 {
            assert_eq!(outcome.damage.scroll, 0);
            assert_eq!(outcome.damage.rows, (0..rows).collect::<Vec<_>>(), "damage uses the current viewport height");
        }
        let mut retirement = None;
        editor.take_completed_save_completion(&mut retirement).unwrap();
        assert_eq!(retirement.as_ref().unwrap().saved.file.path() as *const std::path::Path, owner);
        drop(retirement);
        editor.undo().unwrap();
        assert_eq!(bytes(&editor), b"a  \nb  \nc  \nd  \ne  \nf  \ng  \nh  ");
        assert_eq!(editor.cursors, vec![Cursor::caret(12)]);
    }
}

#[test]
fn unpublished_transfer_and_reset_refuse_occupied_completed_and_partial_owners() {
    let fixture = Fixture::new();
    let other_fixture = Fixture::new();
    let mut editor = Editor::new(Document::new());
    let mut destination = None;
    assert_eq!(editor.take_unpublished_save_completion(&mut destination), Err(SaveRetentionRefusal::EmptyInput));
    let mut saved = fixture.save(&mut editor, "exact");
    saved.normalized = true;
    let mut incoming = Some(SaveCompletionState::new(saved));
    let owner = incoming.as_ref().unwrap().saved.file.path() as *const std::path::Path;
    let budget = AllocationBudget::default();
    incoming.as_mut().unwrap().admit_retention(&budget).unwrap();
    editor.prepare_save_completion(incoming.as_mut().unwrap(), &budget).unwrap();
    editor.retain_save_completion(&mut incoming).unwrap();
    let mut other = Editor::new(Document::new());
    destination = Some(SaveCompletionState::new(other_fixture.save(&mut other, "other")));
    let other_owner = destination.as_ref().unwrap().saved.file.path() as *const std::path::Path;
    let charged = budget.retained_bytes();
    assert_eq!(editor.take_unpublished_save_completion(&mut destination), Err(SaveRetentionRefusal::Occupied));
    assert_eq!(destination.as_ref().unwrap().saved.file.path() as *const std::path::Path, other_owner);
    assert_eq!(editor.receipts.deferred_save.as_ref().unwrap().saved.file.path() as *const std::path::Path, owner);
    assert_eq!(budget.retained_bytes(), charged);
    drop(destination.take()); // other fixture retires on this test worker
    editor.take_unpublished_save_completion(&mut incoming).unwrap();
    incoming.as_mut().unwrap().panic_after_core();
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            editor.commit_save_completion(incoming.as_mut().unwrap())
        }))
        .is_err()
    );
    assert_eq!(incoming.as_mut().unwrap().reset_preparation_for_retry(), Err(SaveRetentionRefusal::PublicationStarted));
    editor.receipts.deferred_save = incoming.take(); // represent the unwind-restored original guarded owner
    assert_eq!(
        editor.take_unpublished_save_completion(&mut destination),
        Err(SaveRetentionRefusal::PublicationStarted)
    );
    assert!(destination.is_none());
    assert_eq!(budget.retained_bytes(), charged);
    assert_eq!(editor.receipts.deferred_save.as_ref().unwrap().saved.file.path() as *const std::path::Path, owner);
    assert!(
        editor.finish_pending_save_sealed(&budget).is_err(),
        "partial publication is never a stale-reset candidate"
    );
    assert_eq!(budget.retained_bytes(), charged);

    let complete_fixture = Fixture::new();
    let mut complete_editor = Editor::new(Document::new());
    let complete_saved = complete_fixture.save(&mut complete_editor, "complete");
    assert!(matches!(complete_editor.finish_save(Ok(complete_saved)), std::ops::ControlFlow::Continue(Ok(_))));
    assert_eq!(
        complete_editor.take_unpublished_save_completion(&mut destination),
        Err(SaveRetentionRefusal::PublicationStarted)
    );
    complete_editor.take_completed_save_completion(&mut destination).unwrap();
    assert_eq!(
        destination.as_mut().unwrap().reset_preparation_for_retry(),
        Err(SaveRetentionRefusal::PublicationStarted)
    );
}

#[test]
fn prepared_core_policy_epoch_change_is_typed_stale_even_when_history_depth_is_unchanged() {
    let fixture = Fixture::new();
    let mut editor = Editor::new(Document::new());
    editor.settings.editor.trim_trailing_whitespace_on_save = true;
    let saved = fixture.save(&mut editor, "a  \nb\t\nlast ");
    let owner = saved.file.path() as *const std::path::Path;
    let mut incoming = Some(SaveCompletionState::new(saved));
    let budget = AllocationBudget::default();
    incoming.as_mut().unwrap().admit_retention(&budget).unwrap();
    let header_charge = budget.retained_bytes();
    editor.prepare_save_completion(incoming.as_mut().unwrap(), &budget).unwrap();
    let charged = budget.retained_bytes();
    let history = editor.document.buffer.history_depth();
    let generation = editor.external_cursor_generation().unwrap();
    editor.set_undo_memory_limit(1024 * 1024);
    assert_eq!(editor.document.buffer.history_depth(), history);
    assert_eq!(editor.external_cursor_generation().unwrap(), generation);
    assert_stale_preparation(editor.prepare_save_completion(incoming.as_mut().unwrap(), &budget).unwrap_err());
    assert_stale_preparation(editor.commit_save_completion(incoming.as_mut().unwrap()).unwrap_err());
    editor.retain_save_completion(&mut incoming).unwrap();
    assert_stale_preparation(editor.finish_pending_save_sealed(&budget).unwrap_err());
    assert_eq!(bytes(&editor), b"a  \nb\t\nlast ");
    assert_eq!(editor.document.buffer.history_depth(), history);
    assert_eq!(budget.retained_bytes(), charged);
    editor.take_unpublished_save_completion(&mut incoming).unwrap();
    assert_eq!(budget.retained_bytes(), charged, "typed stale transfer retains every charge");
    assert_eq!(incoming.as_ref().unwrap().saved.file.path() as *const std::path::Path, owner);
    incoming.as_mut().unwrap().reset_preparation_for_retry().unwrap(); // authority/test worker, off UI
    assert_eq!(budget.retained_bytes(), header_charge);
    editor.prepare_save_completion(incoming.as_mut().unwrap(), &budget).unwrap();
    editor.retain_save_completion(&mut incoming).unwrap();
    assert!(editor.finish_pending_save_sealed(&budget).unwrap().is_some());
    assert_eq!(bytes(&editor), b"a\nb\nlast");
    assert_eq!(editor.document.buffer.history_depth().0, history.0 + 1);
    let mut retirement = None;
    editor.take_completed_save_completion(&mut retirement).unwrap();
    assert_eq!(retirement.as_ref().unwrap().saved.file.path() as *const std::path::Path, owner);
    drop(retirement);
    editor.undo().unwrap();
    assert_eq!(bytes(&editor), b"a  \nb\t\nlast ");
}

#[test]
fn attaching_sink_after_preparation_requires_worker_reset_and_reprepare_with_exact_receipt() {
    let fixture = Fixture::new();
    let mut editor = Editor::new(Document::new());
    editor.settings.editor.trim_trailing_whitespace_on_save = true;
    let saved = fixture.save(&mut editor, "a  \nb\t\nlast ");
    let owner = saved.file.path() as *const std::path::Path;
    let mut incoming = Some(SaveCompletionState::new(saved));
    let budget = AllocationBudget::default();
    incoming.as_mut().unwrap().admit_retention(&budget).unwrap();
    let header_charge = budget.retained_bytes();
    let revision = editor.document.revision;
    let history = editor.document.buffer.history_depth();
    let sequence = editor.next_edit_sequence();
    editor.prepare_save_completion(incoming.as_mut().unwrap(), &budget).unwrap();
    let charged = budget.retained_bytes();
    let destination = editor.receipts.destination_generation();
    assert!(editor.set_journal_sink(crate::document_worker::DocumentJournalSink::closed_for_test()).is_ok());
    assert!(editor.receipts.destination_generation() > destination);
    assert_stale_preparation(editor.prepare_save_completion(incoming.as_mut().unwrap(), &budget).unwrap_err());
    assert_stale_preparation(editor.commit_save_completion(incoming.as_mut().unwrap()).unwrap_err());
    assert_eq!(editor.document.revision, revision);
    assert_eq!(editor.document.buffer.history_depth(), history);
    assert!(!editor.has_pending_edit_receipt());
    assert_eq!(bytes(&editor), b"a  \nb\t\nlast ");
    editor.retain_save_completion(&mut incoming).unwrap();
    assert_stale_preparation(editor.finish_pending_save_sealed(&budget).unwrap_err());
    editor.take_unpublished_save_completion(&mut incoming).unwrap();
    assert_eq!(budget.retained_bytes(), charged);
    assert_eq!(incoming.as_ref().unwrap().saved.file.path() as *const std::path::Path, owner);
    incoming.as_mut().unwrap().reset_preparation_for_retry().unwrap(); // authority/test worker, off UI
    assert_eq!(budget.retained_bytes(), header_charge);
    editor.prepare_save_completion(incoming.as_mut().unwrap(), &budget).unwrap();
    editor.retain_save_completion(&mut incoming).unwrap();
    assert!(editor.finish_pending_save_sealed(&budget).unwrap().is_some());
    assert_eq!(bytes(&editor), b"a\nb\nlast");
    let receipt = editor.receipts.pending.as_ref().expect("new destination requires the exact transformation receipt");
    let receipt_owner = receipt as *const receipts::EditReceipt;
    assert_eq!(receipt.sequence, sequence);
    assert_eq!(receipt.before_revision, revision);
    assert_eq!(receipt.after_revision, editor.document.revision);
    assert_eq!(receipt.replacement_count(), 3);
    let mut before = Vec::new();
    receipt.before.write_to(&mut before).unwrap();
    assert_eq!(before, b"a  \nb\t\nlast ");
    let mut after = Vec::new();
    receipt.after.write_to(&mut after).unwrap();
    assert_eq!(after, b"a\nb\nlast");
    let mut deletions = Vec::new();
    receipt
        .for_each_replacement(&mut |replacement| {
            assert!(replacement.insert.is_empty());
            deletions.push(replacement.range);
            Ok(())
        })
        .unwrap();
    assert_eq!(deletions, vec![11..12, 5..6, 1..3]);
    assert!(matches!(editor.flush_edit_receipt_sealed(None), Err(receipts::ReceiptDeliveryRefusal::Closed)));
    assert_eq!(editor.receipts.pending.as_ref().unwrap() as *const receipts::EditReceipt, receipt_owner);
    assert_eq!(
        editor.pending_edit_sequence(),
        Some(sequence),
        "closed lane cannot acknowledge exact durability target"
    );
    assert_eq!(editor.undo().unwrap_err().kind(), io::ErrorKind::WouldBlock);
    let mut retirement = None;
    editor.take_completed_save_completion(&mut retirement).unwrap();
    assert_eq!(retirement.as_ref().unwrap().saved.file.path() as *const std::path::Path, owner);
    drop(retirement); // only the completed residual retires; the pending receipt stays guarded
}

#[test]
fn saved_request_recipe_remains_authoritative_after_settings_mode_and_language_changes() {
    let fixture = Fixture::new();
    let mut editor = Editor::new(Document::new());
    editor.settings.editor.trim_trailing_whitespace_on_save = true;
    let mut state = SaveCompletionState::new(fixture.save(&mut editor, "a  \nb\t\nlast "));
    let budget = AllocationBudget::default();
    editor.prepare_save_completion(&mut state, &budget).unwrap();
    editor.settings.editor.trim_trailing_whitespace_on_save = false;
    editor.settings.editor.tab_size = 8;
    editor.mode = Mode::Insert;
    editor.set_language(Some("markdown".into()));
    editor.set_line_comment_token(Some("//".into()));
    editor.prepare_save_completion(&mut state, &budget).unwrap();
    editor.commit_save_completion(&mut state).unwrap();
    assert!(state.is_committed());
    assert_eq!(bytes(&editor), b"a\nb\nlast", "later policy changes cannot replace the saved request's exact recipe");
    drop(state); // authority/test worker retirement
    editor.undo().unwrap();
    assert_eq!(bytes(&editor), b"a  \nb\t\nlast ");
}

#[test]
fn exhausted_external_cursor_identity_refuses_save_without_publishing_or_losing_owner() {
    let fixture = Fixture::new();
    let mut editor = Editor::new(Document::new());
    let mut incoming = Some(SaveCompletionState::new(fixture.save(&mut editor, "retained")));
    let owner = incoming.as_ref().unwrap().saved.file.path() as *const std::path::Path;
    let budget = AllocationBudget::default();
    incoming.as_mut().unwrap().admit_retention(&budget).unwrap();
    let header_charge = budget.retained_bytes();
    editor.external_cursor_generation = None;
    assert_stale_preparation(editor.prepare_save_completion(incoming.as_mut().unwrap(), &budget).unwrap_err());
    assert_eq!(budget.retained_bytes(), header_charge);
    editor.retain_save_completion(&mut incoming).unwrap();
    assert_stale_preparation(editor.finish_pending_save_sealed(&budget).unwrap_err());
    assert_eq!(bytes(&editor), b"retained");
    assert!(editor.document.original.is_none());
    editor.take_unpublished_save_completion(&mut incoming).unwrap();
    assert_eq!(incoming.as_ref().unwrap().saved.file.path() as *const std::path::Path, owner);
    incoming.as_mut().unwrap().reset_preparation_for_retry().unwrap(); // test worker
    assert_stale_preparation(editor.prepare_save_completion(incoming.as_mut().unwrap(), &budget).unwrap_err());
    assert_eq!(budget.retained_bytes(), header_charge, "exhausted identities never restart after worker reset");
}

#[test]
fn collapsed_cursor_capacity_stays_charged_in_saved_residual_until_worker_retirement() {
    let fixture = Fixture::new();
    let mut editor = Editor::new(Document::new());
    editor.settings.editor.trim_trailing_whitespace_on_save = true;
    let saved = fixture.save(&mut editor, "a  \nb  ");
    let saved_owner = saved.file.path() as *const std::path::Path;
    editor.set_cursors(vec![Cursor::caret(0); MAX_CURSORS], 0).unwrap();
    assert_eq!(editor.cursors.len(), 1);
    let retired_capacity = editor.cursors.capacity();
    assert!(retired_capacity >= MAX_CURSORS);
    let retired_allocation = editor.cursors.as_ptr();
    let retired_bytes = retired_capacity.checked_mul(std::mem::size_of::<Cursor>()).unwrap();
    assert!(editor.visual_cursor_charge.is_none(), "first save has no earlier visual allocation charge");
    assert!(editor.receipts.save_charge.is_none(), "first save has no earlier save allocation charge");
    let old_inspector_bytes = editor.document_metadata_bytes().unwrap();
    let generation = editor.external_cursor_generation().unwrap();
    let revision = editor.document.revision;
    let history = editor.document.buffer.history_depth();
    let budget = AllocationBudget::default();
    let empty = budget.retained_bytes();
    let mut incoming = Some(SaveCompletionState::new(saved));
    incoming.as_mut().unwrap().admit_retention(&budget).unwrap();
    let header_charge = budget.retained_bytes();

    // Calibrate ordinary bounded staging using this SAME saved owner, history,
    // viewport and aggregate. Only the empty-spare cursor allocation differs.
    let excess = std::mem::replace(&mut editor.cursors, vec![Cursor::caret(0)]);
    let small_capacity = editor.cursors.capacity();
    editor.prepare_save_completion(incoming.as_mut().unwrap(), &budget).unwrap();
    let small_preparation_bytes = budget.retained_bytes() - header_charge;
    editor.cursors = excess; // metadata-only test setup: endpoints/generation are identical
    assert_eq!(editor.external_cursor_generation().unwrap(), generation);
    assert_stale_preparation(editor.prepare_save_completion(incoming.as_mut().unwrap(), &budget).unwrap_err());
    assert_stale_preparation(editor.commit_save_completion(incoming.as_mut().unwrap()).unwrap_err());
    assert_eq!(editor.cursors.as_ptr(), retired_allocation);
    incoming.as_mut().unwrap().reset_preparation_for_retry().unwrap(); // authority/test worker, off UI
    assert_eq!(budget.retained_bytes(), header_charge);

    // Leave exactly the small preparation's headroom in the EXISTING budget.
    // Length-only accounting would incorrectly admit the full-capacity swap.
    let blocked_payload = crate::visual_rows::MAX_SESSION_BYTES
        .checked_sub(budget.retained_bytes())
        .unwrap()
        .checked_sub(small_preparation_bytes)
        .unwrap()
        .checked_sub(std::mem::size_of::<crate::visual_rows::MetadataReservation>())
        .unwrap();
    let headroom = budget.reserve_metadata(blocked_payload).unwrap();
    let before_refusal = budget.retained_bytes();
    let error = editor.prepare_save_completion(incoming.as_mut().unwrap(), &budget).unwrap_err();
    assert!(
        matches!(
            error.get_ref().and_then(|error| error.downcast_ref::<crate::visual_rows::WrapError>()),
            Some(crate::visual_rows::WrapError::Admission(_))
        ),
        "actual retired capacity must refuse before publication when its charge does not fit"
    );
    assert_eq!(budget.retained_bytes(), before_refusal, "refusal releases all temporary admission");
    assert_eq!(editor.cursors.as_ptr(), retired_allocation);
    assert_eq!(editor.cursors.capacity(), retired_capacity);
    assert_eq!(editor.cursors, vec![Cursor::caret(0)]);
    assert_eq!(editor.external_cursor_generation().unwrap(), generation);
    assert_eq!(editor.document.revision, revision);
    assert_eq!(editor.document.buffer.history_depth(), history);
    assert_eq!(bytes(&editor), b"a  \nb  ");
    assert!(editor.document.original.is_none());
    assert!(!incoming.as_ref().unwrap().publication_started());
    assert!(incoming.as_ref().unwrap().prepared_cursor_allocation_for_test().is_none());
    assert_eq!(incoming.as_ref().unwrap().saved.file.path() as *const std::path::Path, saved_owner);
    assert!(editor.visual_cursor_charge.is_none());
    assert!(editor.receipts.save_charge.is_none());
    drop(headroom); // deterministic capacity event on this test worker
    assert_eq!(budget.retained_bytes(), header_charge);

    editor.prepare_save_completion(incoming.as_mut().unwrap(), &budget).unwrap();
    let large_preparation_bytes = budget.retained_bytes() - header_charge;
    let small_bytes = small_capacity.checked_mul(std::mem::size_of::<Cursor>()).unwrap();
    assert_eq!(
        large_preparation_bytes - small_preparation_bytes,
        retired_bytes - small_bytes,
        "the actual retired allocation is charged independently of length-based staging"
    );
    let staged_allocation = incoming.as_ref().unwrap().prepared_cursor_allocation_for_test().unwrap();
    assert!(staged_allocation.1 < retired_capacity);
    let preparing = budget.retained_bytes();
    editor.retain_save_completion(&mut incoming).unwrap();
    assert!(editor.finish_pending_save_sealed(&budget).unwrap().is_some());
    assert_eq!(budget.retained_bytes(), preparing, "publication cannot release the retired allocation charge");
    assert_eq!(editor.cursors.as_ptr(), staged_allocation.0);
    assert_eq!(editor.cursors.capacity(), staged_allocation.1);
    assert!(
        editor.document_metadata_bytes().unwrap() < old_inspector_bytes,
        "the live inspector no longer sees the retired large allocation"
    );
    assert_eq!(
        editor.receipts.deferred_save.as_ref().unwrap().prepared_cursor_allocation_for_test(),
        Some((retired_allocation, retired_capacity))
    );
    assert_eq!(bytes(&editor), b"a\nb");
    assert_eq!(editor.document.buffer.history_depth().0, history.0 + 1);

    let mut retirement = None;
    editor.take_completed_save_completion(&mut retirement).unwrap();
    assert_eq!(budget.retained_bytes(), preparing, "transfer keeps the residual's entire charge");
    assert_eq!(
        retirement.as_ref().unwrap().prepared_cursor_allocation_for_test(),
        Some((retired_allocation, retired_capacity))
    );
    assert_eq!(retirement.as_ref().unwrap().saved.file.path() as *const std::path::Path, saved_owner);
    drop(retirement); // explicit off-UI authority/test worker retirement
    let retired = budget.retained_bytes();
    assert!(preparing - retired >= retired_bytes, "the old allocation releases only at worker retirement");
    assert!(retired > empty, "installed metadata retains its separate live reservation");
    editor.undo().unwrap();
    assert_eq!(bytes(&editor), b"a  \nb  ");
    assert_eq!(editor.cursors, vec![Cursor::caret(0)]);
    drop(editor);
    assert_eq!(budget.retained_bytes(), empty);
}
