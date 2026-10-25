//! Source-only controls. ROOT executes these in its isolated validation tree.
use super::*;
use std::panic::{AssertUnwindSafe, catch_unwind};

fn placeholder() -> (Workspace, DocumentId) {
    let mut workspace = Workspace::new(View::new(None)).unwrap();
    let id = workspace.active_id();
    workspace.views.get_mut(&id).unwrap().editor = None;
    (workspace, id)
}
fn indexed(id: DocumentId) -> DocumentCommit {
    let mut document = Document::new();
    document.buffer.insert(0, b"abc\n").unwrap();
    DocumentCommit::new(DocumentResult::Indexed { document: id, revision: 0, result: Ok(Box::new(document)) })
}
fn bytes(editor: &Editor) -> Vec<u8> {
    let mut bytes = Vec::new();
    editor.document().snapshot().write_to(&mut bytes).unwrap();
    bytes
}

#[test]
fn guarded_index_failure_preserves_absent_editor_and_exact_stage_notice() {
    let (mut workspace, id) = placeholder();
    let mut commit = DocumentCommit::new(DocumentResult::Indexed {
        document: id,
        revision: 0,
        result: Err(io::Error::other("injected index-stage failure")),
    });
    let budget = AllocationBudget::default();
    let update = workspace.commit_document(&mut commit, &budget, None).unwrap();
    assert!(workspace.editor().is_none());
    assert_eq!(workspace.notice(), "Index failed: injected index-stage failure");
    assert!(update.damage().status);
    assert_eq!(commit.phase(), DocumentCommitPhase::Complete);
    std::thread::scope(|scope| scope.spawn(move || drop((commit, update, workspace))).join().unwrap());
}

#[test]
fn commit_before_publication_preserves_exact_original_box_and_no_installed_editor() {
    let (mut workspace, id) = placeholder();
    let mut commit = indexed(id);
    let (pointer, epoch) = match commit.pending().unwrap() {
        DocumentResult::Indexed { result: Ok(document), .. } => {
            (document.as_ref() as *const Document as usize, document.source_epoch)
        }
        _ => unreachable!(),
    };
    commit.fault = CommitFault::BeforePublication;
    let budget = AllocationBudget::default();
    assert!(catch_unwind(AssertUnwindSafe(|| workspace.commit_document(&mut commit, &budget, None))).is_err());
    assert_eq!(commit.phase(), DocumentCommitPhase::Prepared);
    assert!(workspace.views[&id].editor.is_none());
    let Some(DocumentResult::Indexed { result: Ok(document), .. }) = commit.pending() else {
        panic!("original residual lost")
    };
    assert_eq!(document.as_ref() as *const Document as usize, pointer);
    assert_eq!(document.source_epoch, epoch);
    assert_eq!(document.snapshot().len(), 4);
    assert!(matches!(
        workspace.commit_document(&mut commit, &budget, None),
        Err(WorkspaceDocumentAcceptError::PartialCommit)
    ));
    std::thread::scope(|scope| scope.spawn(move || drop(commit)).join().unwrap());
}

#[test]
fn commit_after_installation_retains_residual_and_does_not_replay_phase() {
    let (mut workspace, id) = placeholder();
    let mut commit = indexed(id);
    let epoch = match commit.pending().unwrap() {
        DocumentResult::Indexed { result: Ok(document), .. } => document.source_epoch,
        _ => unreachable!(),
    };
    commit.fault = CommitFault::AfterInstallation;
    let budget = AllocationBudget::default();
    assert!(catch_unwind(AssertUnwindSafe(|| workspace.commit_document(&mut commit, &budget, None))).is_err());
    assert_eq!(commit.phase(), DocumentCommitPhase::Installed);
    let installed = workspace.views[&id].editor().unwrap();
    assert_eq!(installed.document().source_epoch, epoch);
    assert_eq!(bytes(installed), b"abc\n");
    let Some(DocumentResult::Indexed { result: Ok(residual), .. }) = commit.pending() else {
        panic!("partial residual lost")
    };
    assert_ne!(residual.source_epoch, epoch);
    assert!(residual.snapshot().is_empty());
    let before = workspace.input_epoch;
    assert!(matches!(
        workspace.commit_document(&mut commit, &budget, None),
        Err(WorkspaceDocumentAcceptError::PartialCommit)
    ));
    assert_eq!(workspace.input_epoch, before);
    assert_eq!(bytes(workspace.views[&id].editor().unwrap()), b"abc\n");
    std::thread::scope(|scope| scope.spawn(move || drop(commit)).join().unwrap());
}

#[test]
fn stale_index_result_stays_in_completed_residual_instead_of_overwriting_loan() {
    let (mut workspace, id) = placeholder();
    let mut commit = indexed(id);
    // An absent Editor because of a raw loan is never an index placeholder.
    let source = crate::hex_session::Source {
        document: id.0,
        revision: 0,
        epoch: 1,
        representation: crate::hex_session::Representation::RawBytes,
        len: 0,
    };
    let request = crate::hex_session::PageRequest { source, top: 0, rows: 0, generation: 1 };
    workspace.hex.loans.insert(id, super::super::super::workspace_hex::Loan::Page(request));
    let budget = AllocationBudget::default();
    let update = workspace.commit_document(&mut commit, &budget, None).unwrap();
    assert!(commit.is_complete());
    assert!(workspace.views[&id].editor.is_none());
    assert!(
        matches!(commit.pending(),Some(DocumentResult::Indexed {result:Ok(document),..}) if document.snapshot().len()==4)
    );
    assert!(!update.damage.full);
    std::thread::scope(|scope| scope.spawn(move || drop(commit)).join().unwrap());
}

#[test]
fn prepared_merge_keeps_old_editor_roots_in_partial_after_publication_panic() {
    let mut workspace = Workspace::new(View::new(None)).unwrap();
    let old = workspace.active_id();
    workspace.views.get_mut(&old).unwrap().editor_mut().unwrap().insert_text("unsaved", Duration::ZERO).unwrap();
    let old_epoch = workspace.views[&old].editor().unwrap().document().source_epoch;
    let mut session = Session::default();
    let recovered = session.open(None).unwrap();
    let mut document = Document::new();
    document.buffer.insert(0, b"recovered").unwrap();
    let mut commit = DocumentCommit::new(DocumentResult::Prepared(Ok(crate::document_worker::PreparedWorkspace {
        session,
        documents: vec![(recovered, Ok(document))],
    })));
    commit.fault = CommitFault::AfterInstallation;
    assert!(
        catch_unwind(AssertUnwindSafe(|| workspace.commit_document(&mut commit, &AllocationBudget::default(), None)))
            .is_err()
    );
    assert_eq!(commit.phase(), DocumentCommitPhase::Installed);
    assert!(
        workspace
            .views
            .values()
            .any(|view| view.editor().is_some_and(|editor| editor.document().source_epoch == old_epoch))
    );
    assert!(workspace.views.values().any(|view| view.editor().is_some_and(|editor| bytes(editor) == b"recovered")));
    let plan = commit.prepared.as_ref().unwrap();
    assert!(plan.views.values().all(|view| view.editor().is_none()));
    assert!(matches!(
        workspace.commit_document(&mut commit, &AllocationBudget::default(), None),
        Err(WorkspaceDocumentAcceptError::PartialCommit)
    ));
    std::thread::scope(|scope| scope.spawn(move || drop(commit)).join().unwrap());
}

#[test]
fn exact_durability_target_and_pending_receipt_are_not_empty_queue_proofs() {
    let mut workspace = Workspace::new(View::new(None)).unwrap();
    let id = workspace.active_id();
    workspace.files.storage_ready = true;
    workspace.files.metadata_durable = true;
    let editor = workspace.views.get_mut(&id).unwrap().editor_mut().unwrap();
    editor.insert_text("x", Duration::ZERO).unwrap();
    let revision = editor.document().revision();
    let sequence = editor.next_edit_sequence().saturating_sub(1);
    workspace.files.observed.insert(id, (0, 1));
    workspace.files.seeded.insert(id);
    assert!(workspace.files.queue.is_empty());
    assert!(!workspace.document_shutdown_drained().unwrap());
    workspace.files.durable.insert(id, (revision, sequence.saturating_add(1)));
    assert!(!workspace.document_shutdown_drained().unwrap(), "newer sequence is not exact target identity");
    workspace.files.durable.insert(id, (revision, sequence));
    assert!(workspace.document_shutdown_drained().unwrap());
    workspace.files.persist_inflight = true;
    assert!(!workspace.document_shutdown_drained().unwrap());
    workspace.files.persist_inflight = false;
    workspace.files.metadata_durable = false;
    assert!(!workspace.document_shutdown_drained().unwrap());
}

#[test]
fn live_metadata_charge_outlives_commit_and_overlap_refusal_keeps_old_destination() {
    let (mut workspace, id) = placeholder();
    let budget = AllocationBudget::new(12 * 1024 * 1024);
    let baseline = budget.retained_bytes();
    let mut first = indexed(id);
    let update = workspace.commit_document(&mut first, &budget, None).unwrap();
    assert!(workspace.files.live_document_metadata.is_some());
    std::thread::scope(|scope| {
        scope
            .spawn(move || {
                drop(update);
                drop(first);
            })
            .join()
            .unwrap()
    });
    let retained = budget.retained_bytes();
    assert!(retained > baseline);
    let source = workspace.views[&id].editor().unwrap().document().source_epoch;
    let mut second = DocumentCommit::new(DocumentResult::CapacityAvailable);
    assert!(matches!(
        workspace.commit_document(&mut second, &budget, None),
        Err(WorkspaceDocumentAcceptError::Admission(_))
    ));
    assert_eq!(workspace.views[&id].editor().unwrap().document().source_epoch, source);
    assert_eq!(second.phase(), DocumentCommitPhase::Received);
    assert!(matches!(second.pending(), Some(DocumentResult::CapacityAvailable)));
    std::thread::scope(|scope| scope.spawn(move || drop(second)).join().unwrap());
    assert_eq!(budget.retained_bytes(), retained, "old live reservation survives refusal");
    std::thread::scope(|scope| scope.spawn(move || drop(workspace)).join().unwrap());
    assert_eq!(budget.retained_bytes(), baseline);
}

#[test]
fn prepublication_reset_cannot_authorize_non_save_or_replay_an_installed_source() {
    let (mut workspace, id) = placeholder();
    let mut commit = indexed(id);
    let budget = AllocationBudget::default();
    let charged = budget.retained_bytes();
    assert_eq!(commit.prepublication_reset_kind(), None);
    assert_eq!(
        commit.reset_prepublication_on_producer(PrepublicationResetKind::SavePreparation),
        Err(SaveRetentionRefusal::EmptyInput)
    );
    assert_eq!(commit.phase(), DocumentCommitPhase::Received);
    assert_eq!(budget.retained_bytes(), charged);
    commit.fault = CommitFault::AfterInstallation;
    assert!(catch_unwind(AssertUnwindSafe(|| workspace.commit_document(&mut commit, &budget, None))).is_err());
    let epoch = workspace.views[&id].editor().unwrap().document().source_epoch;
    let retained = budget.retained_bytes();
    assert_eq!(commit.prepublication_reset_kind(), None);
    assert_eq!(
        commit.reset_prepublication_on_producer(PrepublicationResetKind::SavePreparation),
        Err(SaveRetentionRefusal::PublicationStarted)
    );
    assert_eq!(commit.phase(), DocumentCommitPhase::Installed);
    assert_eq!(workspace.views[&id].editor().unwrap().document().source_epoch, epoch);
    assert_eq!(bytes(workspace.views[&id].editor().unwrap()), b"abc\n");
    assert_eq!(budget.retained_bytes(), retained);
    std::thread::scope(|scope| scope.spawn(move || drop((commit, workspace))).join().unwrap());
}

#[test]
fn deferred_save_resume_generation_is_checked_global_and_deduplicated() {
    use crate::workspace::{SaveFlight, SaveResumePhase};
    fn flight() -> SaveFlight {
        SaveFlight {
            revision: 7,
            path: None,
            source_epoch: 9,
            resume_generation: 0,
            resume: SaveResumePhase::Local,
            preparation_error: None,
        }
    }
    fn stale() -> io::Error {
        io::Error::new(io::ErrorKind::InvalidInput, crate::editor::save::SavePreparationRefusal::StaleIdentity)
    }
    let mut generation = 0;
    let mut first = flight();
    first.retain_stale_preparation(9, &mut generation, stale());
    assert_eq!(first.resume, SaveResumePhase::Needed);
    let key = first.resume_key(DocumentId(1));
    first.retain_stale_preparation(9, &mut generation, io::Error::from(io::ErrorKind::PermissionDenied));
    assert_eq!(first.resume_key(DocumentId(1)), key, "Full/repeated wakes cannot enqueue another generation");
    assert!(stale_save_preparation(first.preparation_error.as_ref().unwrap()), "first actual refusal stays retained");
    let mut second = flight();
    second.retain_stale_preparation(9, &mut generation, stale());
    assert!(second.resume_generation > first.resume_generation, "recreated flights cannot reuse a resume generation");
    let mut exhausted = flight();
    generation = u64::MAX;
    exhausted.retain_stale_preparation(9, &mut generation, stale());
    assert_eq!(exhausted.resume, SaveResumePhase::Exhausted);
    assert_eq!(generation, u64::MAX);
    assert!(exhausted.preparation_error.is_some());
    let mut foreign = flight();
    generation = 0;
    foreign.retain_stale_preparation(10, &mut generation, stale());
    assert_eq!(foreign.resume, SaveResumePhase::Exhausted);
    assert_eq!(generation, 0, "wrong actual source cannot mint retry authority");
}
