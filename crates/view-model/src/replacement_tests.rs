//! Focused ownership/worker regressions; no native UI or persistent owner data.
use super::*;
use kwil_core_search::worker::{
    DocumentSearch, PAGE_HITS, Page, Query, REPLACE_EDITS, ReplaceRecipe, ReplaceTarget, Replacement, ReplacementEdits,
    SearchKey, SearchRequest, SearchTask, SearchUpdate, SearchWorker,
};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

fn recipe(workspace: &Workspace, edits: Vec<kwil_core_search::Edit>) -> ReplaceRecipe {
    ReplaceRecipe {
        key: SearchKey {
            document_id: workspace.active_id().0,
            revision: workspace.editor().unwrap().document().revision(),
            generation: 17,
        },
        edits: ReplacementEdits::Inline(Arc::new(edits)),
    }
}

fn insert_recipe(workspace: &Workspace) -> ReplaceRecipe {
    recipe(workspace, vec![kwil_core_search::Edit { offset: 0, len: 0, new_bytes: b"replaced".to_vec() }])
}

fn stored_replacement_edits(
    edits: Vec<kwil_core_search::Edit>,
    spool: bool,
    owner: &mut kwil_core_search::worker_spool::SpoolOwner,
) -> ReplacementEdits {
    if spool {
        let slot = owner.begin().unwrap();
        for edit in edits {
            owner.append(slot, edit).unwrap();
        }
        ReplacementEdits::Spool(owner.finish(slot).unwrap())
    } else {
        ReplacementEdits::Inline(Arc::new(edits))
    }
}

fn bytes(workspace: &Workspace, id: DocumentId) -> Vec<u8> {
    let mut bytes = Vec::new();
    let view = workspace.view(id).unwrap_or_else(|| {
        panic!(
            "document {} missing after recovery; active={}, tabs={:?}",
            id.0,
            workspace.active_id().0,
            workspace.session().tabs()
        )
    });
    let editor = view.editor().unwrap_or_else(|| {
        panic!(
            "document {} has no editor; recovery_ready={}, notice={}",
            id.0,
            workspace.recovery_ready(),
            view.notice()
        )
    });
    editor.document().snapshot().write_to(&mut bytes).unwrap();
    bytes
}

fn channel_workspace(capacity: usize) -> (Workspace, mpsc::Receiver<DocumentRequest>) {
    let mut workspace = Workspace::new(View::new(None)).unwrap();
    workspace.set_rows(3);
    let (worker, receiver) = DocumentWorker::replacement_test_channel(capacity);
    workspace.files.worker = Some(worker);
    workspace.files.storage_ready = true;
    workspace.install_journal_observers();
    (workspace, receiver)
}

fn take_loan(receiver: &mpsc::Receiver<DocumentRequest>) -> (u64, Box<Editor>, ReplaceRecipe) {
    match receiver.recv_timeout(Duration::from_secs(5)).unwrap() {
        DocumentRequest::Replace { operation, editor, recipe, .. } => (operation, editor, recipe),
        _ => panic!("expected owned replacement request"),
    }
}

fn drive(workspace: &mut Workspace, receiver: &mpsc::Receiver<DocumentResult>, done: impl Fn(&Workspace) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done(workspace) {
        let remaining = deadline.checked_duration_since(Instant::now()).expect("replacement deadline");
        workspace.test_document_result(receiver.recv_timeout(remaining).expect("replacement worker completion"));
    }
}

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "kwil-replacement-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("Replacement fixture retained for diagnosis: {}", self.0.display());
            return;
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn real_workspace(root: &Scratch) -> (Workspace, mpsc::Receiver<DocumentResult>) {
    let (sender, receiver) = mpsc::channel();
    let mut workspace = Workspace::new(View::new(None)).unwrap();
    workspace
        .start_document_worker_at(Some(root.0.clone()), move |result| {
            let _ = sender.send(result);
        })
        .unwrap();
    drive(&mut workspace, &receiver, Workspace::recovery_ready);
    (workspace, receiver)
}

fn stop(workspace: Workspace, receiver: &mpsc::Receiver<DocumentResult>) {
    drop(workspace);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline.checked_duration_since(Instant::now()).expect("worker shutdown deadline");
        if let DocumentResult::Stopped(result) = receiver.recv_timeout(remaining).unwrap() {
            result.unwrap();
            break;
        }
    }
}

#[test]
fn replacement_stale_revision_and_missing_recovery_refuse_without_changes() {
    let (mut workspace, receiver) = channel_workspace(16);
    let id = workspace.active_id();
    let mut stale = insert_recipe(&workspace);
    stale.key.revision += 1;
    assert_eq!(workspace.submit_search_replacement(stale).unwrap_err().kind(), io::ErrorKind::InvalidInput);
    assert!(!workspace.document_busy(id));
    assert!(bytes(&workspace, id).is_empty());
    assert!(receiver.try_recv().is_err());
    workspace.files.storage_ready = false;
    assert_eq!(
        workspace.submit_search_replacement(insert_recipe(&workspace)).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    assert!(workspace.editor().is_some());
}

#[test]
fn replacement_full_queue_and_disconnect_restore_actual_editor_and_observer() {
    let (mut workspace, receiver) = channel_workspace(1);
    let id = workspace.active_id();
    assert!(workspace.files.worker.as_ref().unwrap().try_submit(DocumentRequest::SessionTimer).is_ok());
    assert_eq!(
        workspace.submit_search_replacement(insert_recipe(&workspace)).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    assert!(!workspace.document_busy(id));
    assert!(bytes(&workspace, id).is_empty());
    assert!(matches!(receiver.try_recv().unwrap(), DocumentRequest::SessionTimer));
    workspace
        .execute("edit.insert", &crate::CommandArgs { text: Some("kept".into()), ..Default::default() }, 1)
        .unwrap();
    let DocumentRequest::Journal { receipt, .. } = receiver.try_recv().unwrap() else { panic!("observer was lost") };
    assert_eq!(receipt.replacement_count(), 1);
    assert_eq!(bytes(&workspace, id), b"kept");
    drop(receiver);
    assert_eq!(
        workspace.submit_search_replacement(insert_recipe(&workspace)).unwrap_err().kind(),
        io::ErrorKind::BrokenPipe
    );
    assert_eq!(bytes(&workspace, id), b"kept");
    workspace.execute("edit.undo", &crate::CommandArgs::default(), 1).unwrap();
    assert!(
        workspace.editor().unwrap().has_pending_edit_receipt(),
        "original observer still retains refused undo receipt"
    );
    assert!(bytes(&workspace, id).is_empty());
}

#[test]
fn replacement_loan_freezes_target_and_exit_but_preserves_other_tab_and_cache() {
    let (mut workspace, receiver) = channel_workspace(16);
    let id = workspace.active_id();
    let cached = workspace.line(0).map(<[u8]>::to_vec);
    workspace.submit_search_replacement(insert_recipe(&workspace)).unwrap();
    assert!(workspace.document_busy(id));
    assert!(workspace.replacement_busy());
    assert!(workspace.file_saves_pending());
    assert!(workspace.editor_mut().is_none());
    assert_eq!(workspace.line(0).map(<[u8]>::to_vec), cached);
    assert_eq!(workspace.save_document(id, None).unwrap_err().kind(), io::ErrorKind::WouldBlock);
    assert_eq!(workspace.close(id, true).unwrap_err().kind(), io::ErrorKind::WouldBlock);
    workspace.request_window_close(true).unwrap();
    assert!(!workspace.wants_exit());
    workspace.cancel_close_intent();
    assert!(workspace.document_busy(id), "cancel close does not cancel an accepted edit");
    let other = workspace.open(View::new(None)).unwrap();
    workspace
        .execute("edit.insert", &crate::CommandArgs { text: Some("other".into()), ..Default::default() }, 1)
        .unwrap();
    assert!(!workspace.replacement_busy());
    assert_eq!(bytes(&workspace, other), b"other");
    let (operation, editor, recipe) = take_loan(&receiver);
    workspace.test_document_result(DocumentResult::Replaced {
        key: recipe.key,
        operation,
        editor,
        result: Err(io::Error::other("terminal fixture error")),
    });
    assert!(!workspace.document_busy(id));
    assert_eq!(workspace.active_id(), other);
    assert!(bytes(&workspace, id).is_empty());
    assert_eq!(bytes(&workspace, other), b"other");
}

#[test]
fn replacement_pending_receipt_and_save_are_refused_before_transfer() {
    let (mut workspace, receiver) = channel_workspace(1);
    assert!(workspace.files.worker.as_ref().unwrap().try_submit(DocumentRequest::SessionTimer).is_ok());
    workspace
        .execute("edit.insert", &crate::CommandArgs { text: Some("retained".into()), ..Default::default() }, 1)
        .unwrap();
    assert!(workspace.editor().unwrap().has_pending_edit_receipt());
    assert_eq!(
        workspace.submit_search_replacement(insert_recipe(&workspace)).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    assert!(matches!(receiver.try_recv().unwrap(), DocumentRequest::SessionTimer));
    workspace.editor_mut().unwrap().flush_edit_receipt().unwrap();
    assert!(matches!(receiver.try_recv().unwrap(), DocumentRequest::Journal { .. }));
    let path = std::env::temp_dir().join("kwil-no-io-loan-save.txt");
    workspace.editor_mut().unwrap().save_request(Some(path)).unwrap();
    assert_eq!(
        workspace.submit_search_replacement(insert_recipe(&workspace)).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    workspace.editor_mut().unwrap().finish_save_for_test(Err(io::Error::other("cancel fixture save"))).unwrap_err();
    assert_eq!(bytes(&workspace, workspace.active_id()), b"retained");
    assert!(receiver.try_recv().is_err());
}

#[test]
fn replacement_returned_pending_receipt_blocks_discard_close_and_is_pumped() {
    let (mut workspace, receiver) = channel_workspace(1);
    let id = workspace.active_id();
    workspace.submit_search_replacement(insert_recipe(&workspace)).unwrap();
    let (operation, mut editor, recipe) = take_loan(&receiver);
    assert!(workspace.files.worker.as_ref().unwrap().try_submit(DocumentRequest::SessionTimer).is_ok());
    let result = editor.apply_search_replacement(recipe);
    assert!(result.is_ok());
    assert!(editor.has_pending_edit_receipt());
    let key = workspace.files.loans[&id].key;
    workspace.test_document_result(DocumentResult::Replaced { key, operation, editor, result });
    assert!(!workspace.document_busy(id));
    assert_eq!(bytes(&workspace, id), b"replaced");
    assert!(workspace.editor().unwrap().has_pending_edit_receipt());
    assert_eq!(workspace.close(id, true).unwrap_err().kind(), io::ErrorKind::WouldBlock);
    assert!(matches!(receiver.try_recv().unwrap(), DocumentRequest::SessionTimer));
    workspace.test_document_result(DocumentResult::CapacityAvailable);
    assert!(!workspace.editor().unwrap().has_pending_edit_receipt());
    let DocumentRequest::Journal { receipt, .. } = receiver.try_recv().unwrap() else { panic!("receipt missing") };
    assert_eq!(receipt.after_revision, workspace.editor().unwrap().document().revision());
    workspace.files.exit = true;
    workspace.files.exit_without_metadata = true;
    workspace.files.queue.clear(); // fixture excludes unrelated metadata requests
    assert!(!workspace.wants_exit(), "receipt queue admission alone is not durability");
    workspace.test_document_result(DocumentResult::JournalDurable {
        document: id,
        revision: receipt.after_revision,
        sequence: receipt.sequence,
        result: Ok(()),
    });
    assert!(workspace.files.replacement_journals.is_empty());
}

#[test]
fn replacement_worker_invalid_recipe_returns_owner_and_preserves_undo() {
    let root = Scratch::new();
    let (mut workspace, receiver) = real_workspace(&root);
    let id = workspace.active_id();
    workspace
        .execute("edit.insert", &crate::CommandArgs { text: Some("before".into()), ..Default::default() }, 1)
        .unwrap();
    let bad = recipe(&workspace, vec![kwil_core_search::Edit { offset: 99, len: 1, new_bytes: b"bad".to_vec() }]);
    workspace.submit_search_replacement(bad).unwrap();
    drive(&mut workspace, &receiver, |workspace| !workspace.search_replacement_pending());
    assert_eq!(bytes(&workspace, id), b"before");
    assert!(workspace.view(id).unwrap().notice().contains("Replacement failed"));
    workspace.execute("edit.undo", &crate::CommandArgs::default(), 1).unwrap();
    assert!(bytes(&workspace, id).is_empty());
    stop(workspace, &receiver);
}

#[test]
fn replacement_real_journal_terminal_error_restores_owner_and_can_save() {
    let root = Scratch::new();
    let (mut workspace, receiver) = real_workspace(&root);
    let id = workspace.active_id();
    // Make only this isolated journal path unwritable as a file. The replace
    // itself must return its editor before the journal failure is reported.
    std::fs::create_dir(root.0.join(format!("{}.kwj", id.0))).unwrap();
    workspace.submit_search_replacement(insert_recipe(&workspace)).unwrap();
    drive(&mut workspace, &receiver, |workspace| {
        !workspace.search_replacement_pending() && workspace.files.journal_failed.contains_key(&id)
    });
    assert_eq!(bytes(&workspace, id), b"replaced");
    assert!(workspace.editor().is_some());
    assert!(workspace.file_saves_pending(), "journal failure cannot silently release the exit barrier");
    let destination = root.0.join("saved-after-journal-error.txt");
    workspace.save_document(id, Some(destination.clone())).unwrap();
    drive(&mut workspace, &receiver, |workspace| workspace.files.saves.is_empty());
    assert_eq!(std::fs::read(destination).unwrap(), b"replaced");
    assert!(workspace.files.replacement_journals.is_empty(), "current atomic save preserves the accepted bytes");
    drop(workspace);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let DocumentResult::Stopped(_) = receiver
            .recv_timeout(deadline.checked_duration_since(Instant::now()).expect("failed journal shutdown deadline"))
            .unwrap()
        {
            break;
        }
    }
}

#[test]
fn replacement_worker_replace_all_streams_beyond_initial_page_and_is_one_undo_redo() {
    let root = Scratch::new();
    let (mut workspace, receiver) = real_workspace(&root);
    let id = workspace.active_id();
    let count = REPLACE_EDITS + 11;
    let original = "a\n".repeat(count);
    workspace
        .execute("edit.insert", &crate::CommandArgs { text: Some(original.clone()), ..Default::default() }, 1)
        .unwrap();
    let snapshot = workspace.editor().unwrap().document().snapshot();
    let key =
        SearchKey { document_id: id.0, revision: workspace.editor().unwrap().document().revision(), generation: 31 };
    let (wake_tx, wake_rx) = mpsc::sync_channel(4);
    let search = SearchWorker::new(move || {
        let _ = wake_tx.try_send(());
    })
    .unwrap();
    search
        .submit(SearchRequest {
            key,
            task: SearchTask::Document(DocumentSearch {
                ranges: std::iter::once(0..snapshot.len()).collect(),
                snapshot,
                query: Query::literal("a".into()),
                page: Page::default(),
                navigation: None,
                replacement: Some(Replacement { template: b"bc".to_vec(), target: ReplaceTarget::All }),
            }),
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let replacement = 'result: loop {
        while let Some(event) = search.take_event() {
            match event.update {
                SearchUpdate::Document(mut matches) => {
                    assert_eq!(matches.count, count as u64);
                    assert!(matches.hits.len() <= PAGE_HITS);
                    let recipe = matches.replacement.take().expect("complete replacement recipe");
                    assert!(matches!(recipe.edits, ReplacementEdits::Spool(_)), "exercise bounded spilled iteration");
                    break 'result recipe;
                }
                SearchUpdate::Failed(error) => panic!("search: {error}"),
                _ => {}
            }
        }
        wake_rx.recv_timeout(deadline.checked_duration_since(Instant::now()).expect("search deadline")).unwrap();
    };
    let source_sequence = workspace.editor().unwrap().next_edit_sequence();
    workspace.submit_search_replacement(replacement).unwrap();
    workspace.cancel_close_intent(); // accepted replacement survives UI cancellation
    drive(&mut workspace, &receiver, |workspace| !workspace.search_replacement_pending());
    assert_eq!(bytes(&workspace, id), "bc\n".repeat(count).as_bytes());
    assert_eq!(workspace.editor().unwrap().next_edit_sequence(), source_sequence + 1);
    // A tiny source split into many pieces must not charge its shared added
    // block once per piece and evict the replacement at the default undo cap.
    // Keep the real spooled workload and all byte/receipt assertions intact.
    assert_eq!(
        workspace.editor().unwrap().document().buffer.history_depth(),
        (2, 0),
        "the initial insertion and replacement must both retain their undo transitions"
    );
    workspace.execute("edit.undo", &crate::CommandArgs::default(), 1).unwrap();
    assert_eq!(bytes(&workspace, id), original.as_bytes());
    assert_eq!(workspace.editor().unwrap().document().revision(), key.revision);
    assert_eq!(workspace.editor().unwrap().document().buffer.history_depth(), (1, 1));
    workspace.execute("edit.redo", &crate::CommandArgs::default(), 1).unwrap();
    assert_eq!(bytes(&workspace, id), "bc\n".repeat(count).as_bytes());
    assert_eq!(workspace.editor().unwrap().document().buffer.history_depth(), (2, 0));
    workspace.set_session_restore(true);
    workspace.request_window_close(false).unwrap();
    drive(&mut workspace, &receiver, Workspace::wants_exit);
    assert!(workspace.files.replacement_journals.is_empty());
    stop(workspace, &receiver);
    search.shutdown().unwrap();

    let (mut persisted_batches, mut persisted_replacements) = (0_u64, 0_u64);
    kwil_core_workspace::journal::JournalReader::open(&root.0.join(format!("{}.kwj", id.0)))
        .unwrap()
        .recover(&std::sync::atomic::AtomicBool::new(false), |event| {
            if let kwil_core_workspace::journal::RecoveryEvent::BeginBatch(batch) = event {
                persisted_batches += 1;
                persisted_replacements += batch.replacements;
                assert_eq!(batch.replacements, 1, "dense receipt must persist as one bounded span");
            }
            Ok(())
        })
        .unwrap();
    eprintln!("persisted replacement diagnostics: batches={persisted_batches} spans={persisted_replacements}");
    assert_eq!((persisted_batches, persisted_replacements), (4, 4));

    // The real recipe journal must recover the replacement, not just the UI cache.
    let (recovered, receiver) = real_workspace(&root);
    assert_eq!(bytes(&recovered, id), "bc\n".repeat(count).as_bytes());
    stop(recovered, &receiver);
}

#[test]
fn replacement_legacy_raw_journal_recovers_grouped_edit_undo_and_redo() {
    use kwil_core_workspace::journal::{JournalBase, JournalSeed, JournalService};
    let root = Scratch::new();
    let source = "a---a---a";
    let id = DocumentId(1);
    let mut service = JournalService::spawn(Duration::from_secs(2)).unwrap();
    let handle = service
        .try_create(JournalSeed {
            path: root.0.join("1.kwj"),
            base: JournalBase {
                document: id,
                revision: 0,
                original_path: None,
                original_identity: [0; 4],
                codec: Vec::new(),
                byte_len: source.len() as u64,
            },
            source: Box::new(io::Cursor::new(source.as_bytes().to_vec())),
        })
        .unwrap();
    handle.ready.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
    service.finish();
    service.finished.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
    drop((handle, service));
    let (mut workspace, receiver) = real_workspace(&root);
    assert_eq!(bytes(&workspace, id), source.as_bytes());
    let edits = [0, 4, 8]
        .into_iter()
        .map(|offset| kwil_core_search::Edit { offset, len: 1, new_bytes: b"bc".to_vec() })
        .collect();
    workspace.submit_search_replacement(recipe(&workspace, edits)).unwrap();
    drive(&mut workspace, &receiver, |workspace| !workspace.search_replacement_pending());
    assert_eq!(bytes(&workspace, id), b"bc---bc---bc");
    workspace.execute("edit.undo", &crate::CommandArgs::default(), 1).unwrap();
    assert_eq!(bytes(&workspace, id), source.as_bytes());
    workspace.execute("edit.redo", &crate::CommandArgs::default(), 1).unwrap();
    assert_eq!(bytes(&workspace, id), b"bc---bc---bc");
    workspace.set_session_restore(true);
    workspace.request_window_close(false).unwrap();
    drive(&mut workspace, &receiver, Workspace::wants_exit);
    stop(workspace, &receiver);
    let (recovered, receiver) = real_workspace(&root);
    assert_eq!(bytes(&recovered, id), b"bc---bc---bc");
    stop(recovered, &receiver);
}

#[test]
fn replacement_noop_inline_and_spool_following_undo_and_restart_stay_consistent() {
    for spool in [false, true] {
        let root = Scratch::new();
        let mut owner = kwil_core_search::worker_spool::SpoolOwner::new(8 << 20).unwrap();
        let (mut workspace, receiver) = real_workspace(&root);
        let id = workspace.active_id();
        workspace
            .execute("edit.insert", &crate::CommandArgs { text: Some("a\nb\n".into()), ..Default::default() }, 1)
            .unwrap();
        let revision = workspace.editor().unwrap().document().revision();
        let sequence = workspace.editor().unwrap().next_edit_sequence();
        let history = workspace.editor().unwrap().document().buffer.history_depth();
        for edits in [Vec::new(), vec![kwil_core_search::Edit { offset: 0, len: 0, new_bytes: Vec::new() }]] {
            let mut identity = recipe(&workspace, Vec::new());
            identity.edits = stored_replacement_edits(edits, spool, &mut owner);
            workspace.submit_search_replacement(identity).unwrap();
            drive(&mut workspace, &receiver, |workspace| !workspace.search_replacement_pending());
            let editor = workspace.editor().unwrap();
            assert_eq!(editor.document().revision(), revision);
            assert_eq!(editor.next_edit_sequence(), sequence);
            assert_eq!(editor.document().buffer.history_depth(), history);
            assert_eq!(bytes(&workspace, id), b"a\nb\n");
        }
        workspace.execute("edit.undo", &crate::CommandArgs::default(), 1).unwrap();
        assert!(bytes(&workspace, id).is_empty());
        assert_eq!(
            workspace.editor().unwrap().document().revision(),
            0,
            "the real insertion's undo must restore model and core together"
        );
        workspace.execute("edit.redo", &crate::CommandArgs::default(), 1).unwrap();
        assert_eq!(bytes(&workspace, id), b"a\nb\n");
        let mut mixed = recipe(&workspace, Vec::new());
        mixed.edits = stored_replacement_edits(
            vec![
                kwil_core_search::Edit { offset: 0, len: 0, new_bytes: Vec::new() },
                kwil_core_search::Edit { offset: 2, len: 1, new_bytes: b"XX".to_vec() },
                kwil_core_search::Edit { offset: 4, len: 0, new_bytes: Vec::new() },
            ],
            spool,
            &mut owner,
        );
        let sequence = workspace.editor().unwrap().next_edit_sequence();
        workspace.submit_search_replacement(mixed).unwrap();
        drive(&mut workspace, &receiver, |workspace| !workspace.search_replacement_pending());
        assert_eq!(bytes(&workspace, id), b"a\nXX\n");
        assert_eq!(workspace.editor().unwrap().next_edit_sequence(), sequence + 1);
        workspace.execute("edit.undo", &crate::CommandArgs::default(), 1).unwrap();
        assert_eq!(bytes(&workspace, id), b"a\nb\n");
        workspace.execute("edit.redo", &crate::CommandArgs::default(), 1).unwrap();
        assert_eq!(bytes(&workspace, id), b"a\nXX\n");
        workspace.set_session_restore(true);
        workspace.request_window_close(false).unwrap();
        drive(&mut workspace, &receiver, Workspace::wants_exit);
        stop(workspace, &receiver);
        owner.retire().unwrap();
        assert_eq!(owner.live(), 0);
        let (recovered, receiver) = real_workspace(&root);
        assert_eq!(bytes(&recovered, id), b"a\nXX\n");
        stop(recovered, &receiver);
    }
}
