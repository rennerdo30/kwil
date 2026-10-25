//! Workspace/actual-worker ownership regressions. ROOT runs these; fixtures
//! never use the owner's recovery directory, settings or files.
use super::*;
use std::{
    fs,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "kwil-workspace-hex-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn source(&self, bytes: &[u8]) -> PathBuf {
        let path = self.0.join("physical.bin");
        fs::write(&path, bytes).unwrap();
        path
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
fn bytes(workspace: &Workspace, id: DocumentId) -> Vec<u8> {
    let mut bytes = Vec::new();
    workspace.views[&id].editor().unwrap().document().snapshot().write_to(&mut bytes).unwrap();
    bytes
}
fn drive(workspace: &mut Workspace, receiver: &mpsc::Receiver<DocumentResult>, done: impl Fn(&Workspace) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done(workspace) {
        let remaining = deadline.checked_duration_since(Instant::now()).expect("bounded workspace hex worker deadline");
        workspace.test_document_result(receiver.recv_timeout(remaining).expect("workspace hex worker completion"));
    }
}
fn real_workspace(fixture: &Fixture) -> (Workspace, mpsc::Receiver<DocumentResult>) {
    let (sender, receiver) = mpsc::channel();
    let mut workspace = Workspace::new(View::new(None)).unwrap();
    workspace.set_rows(3);
    workspace
        .start_document_worker_at(Some(fixture.0.join("recovery")), move |result| {
            let _ = sender.send(result);
        })
        .unwrap();
    drive(&mut workspace, &receiver, Workspace::recovery_ready);
    (workspace, receiver)
}
fn stop(mut workspace: Workspace, receiver: &mpsc::Receiver<DocumentResult>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !workspace.prepare_hex_shutdown() {
        let remaining = deadline.checked_duration_since(Instant::now()).expect("bounded raw ownership disposal");
        let result = receiver.recv_timeout(remaining).unwrap_or_else(|error| {
            panic!("raw disposal wake: {error:?}; active={:?} queue={} loans={} opening={} saves_pending={} preparing={} metadata_durable={} persist_pending={} persist_inflight={} recovery={:?} observed={:?} durable={:?} journal_errors={:?}",
                workspace.active_id(), workspace.files.queue.len(), workspace.hex.loans.len(), workspace.hex.opening(), workspace.file_saves_pending(), workspace.files.preparing,
                workspace.files.metadata_durable, workspace.files.persist_pending, workspace.files.persist_inflight, workspace.recovery_notice(), workspace.files.observed, workspace.files.durable, workspace.files.journal_failed)
        });
        workspace.test_document_result(result);
    }
    drop(workspace);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline.checked_duration_since(Instant::now()).expect("bounded workspace hex shutdown");
        if let DocumentResult::Stopped(result) = receiver.recv_timeout(remaining).unwrap() {
            result.unwrap();
            break;
        }
    }
}
fn input(workspace: &mut Workspace, value: hex::Input<'_>) -> io::Result<Damage> {
    let session = workspace.hex_session().unwrap();
    let source = session.source().unwrap();
    let epoch = session.input_epoch();
    workspace.hex_input(source, epoch, value)
}
fn open_raw(workspace: &mut Workspace, receiver: &mpsc::Receiver<DocumentResult>, path: PathBuf) -> DocumentId {
    let (id, outcome) = workspace.open_hex_path(path, 3).unwrap();
    assert_eq!(outcome, OpenOutcome::Ready);
    drive(workspace, receiver, |w| {
        !w.document_busy(id)
            && w.files.observed.contains_key(&id)
            && w.hex.tabs[&id].session.row(w.hex.tabs[&id].session.source(), 0).is_some()
    });
    id
}
fn edit_and_drain(workspace: &mut Workspace, receiver: &mpsc::Receiver<DocumentResult>, value: hex::Input<'_>) {
    let id = workspace.active_id();
    input(workspace, value).unwrap();
    drive(workspace, receiver, |w| !w.document_busy(id));
}
fn channel_workspace(fixture: &Fixture, capacity: usize) -> (Workspace, mpsc::Receiver<DocumentRequest>) {
    let path = fixture.source(b"abcdefghijklmnop0123456789ABCDEFthird-row-bytes!fourth-row-data!");
    let file = kwil_core_text::TextFile::open(&path).unwrap();
    file.build_index(1).unwrap();
    let document = crate::Document::from_raw_file(path.clone(), file, crate::document::next_source_epoch()).unwrap();
    let mut workspace = Workspace::new(View::new(None)).unwrap();
    let id = workspace.active_id();
    let source = document.hex_source(id.0);
    let view = workspace.views.get_mut(&id).unwrap();
    view.editor = Some(Editor::new(document));
    view.path = Some(path);
    view.rows = 2;
    let mut tab = HexTab::new(2);
    tab.session.bind(source);
    let request = tab.session.page_request().unwrap();
    let page = hex::read_page(&view.editor().unwrap().document().snapshot(), request, || false).unwrap();
    tab.session.accept_page(page);
    tab.last_page = Some(request);
    workspace.hex.tabs.insert(id, tab);
    let (worker, receiver) = DocumentWorker::replacement_test_channel(capacity);
    workspace.files.worker = Some(worker);
    workspace.files.storage_ready = true;
    workspace.install_journal_observers();
    // This channel fixture tests loans, not seed delivery. The production
    // observer seeds every raw incarnation; discard only this fixture seed.
    workspace.files.queue.clear();
    workspace.sync_all();
    (workspace, receiver)
}

#[test]
fn workspace_hex_worker_physical_open_edits_undo_raw_save_and_closed_history() {
    let fixture = Fixture::new();
    let raw = b"\x1f\x8b\x08\0\xff\xfeA\0 \t\r\n";
    let source_path = fixture.source(raw);
    let (mut workspace, receiver) = real_workspace(&fixture);
    let id = open_raw(&mut workspace, &receiver, source_path.clone());
    assert_eq!(bytes(&workspace, id), raw);
    assert_eq!(workspace.hex_source().unwrap().representation, Representation::RawBytes);
    assert_eq!(workspace.views[&id].file().unwrap().path(), source_path);
    assert!(workspace.files.seeded.contains(&id), "clean physical transport must seed raw recovery policy");
    let origin = workspace.hex_source().unwrap();
    edit_and_drain(&mut workspace, &receiver, hex::Input::Paste("80"));
    assert_eq!(bytes(&workspace, id)[0], 0x80);
    edit_and_drain(&mut workspace, &receiver, hex::Input::Undo);
    assert_eq!(bytes(&workspace, id), raw);
    edit_and_drain(&mut workspace, &receiver, hex::Input::Redo);
    assert_eq!(workspace.hex_source().unwrap().epoch, origin.epoch);
    assert!(workspace.editor_mut().is_none());
    assert_eq!(
        workspace
            .execute("edit.insert", &crate::CommandArgs { text: Some("bad".into()), ..Default::default() }, 1)
            .unwrap_err()
            .kind(),
        io::ErrorKind::Unsupported
    );
    let mut settings = kwil_core_commands::Settings::default();
    settings.editor.trim_trailing_whitespace_on_save = true;
    workspace.set_editor_settings(&settings);
    let saved = fixture.0.join("physical-save.bin");
    workspace.save_document(id, Some(saved.clone())).unwrap();
    drive(&mut workspace, &receiver, |w| !w.files.saves.contains_key(&id));
    let mut expected = raw.to_vec();
    expected[0] = 0x80;
    assert_eq!(fs::read(&saved).unwrap(), expected, "raw save must bypass gzip, decode, BOM and whitespace transforms");
    assert_eq!(fs::read(source_path).unwrap(), raw, "Save As retains old physical source");
    assert!(!workspace.views[&id].editor().unwrap().document().is_dirty());
    workspace.close(id, false).unwrap();
    assert!(workspace.hex.tabs.contains_key(&id), "closed cached undo keeps the raw session/fence");
    workspace.file_command("file.reopenClosed").unwrap();
    drive(&mut workspace, &receiver, |w| w.views.contains_key(&id) && w.files.reopen.is_none() && !w.document_busy(id));
    assert_eq!(bytes(&workspace, id), expected);
    edit_and_drain(&mut workspace, &receiver, hex::Input::Undo);
    assert_eq!(bytes(&workspace, id), raw, "cached reopen must retain the actual shared undo chain");
    stop(workspace, &receiver);
}

#[test]
fn workspace_hex_dirty_decoded_refusal_and_symmetric_worker_reopen_preserve_identity() {
    let fixture = Fixture::new();
    let raw = b"\xff\xfeA\0B\0\r\0\n\0";
    let path = fixture.source(raw);
    let (mut workspace, receiver) = real_workspace(&fixture);
    let id = workspace.open_path(path).unwrap();
    drive(&mut workspace, &receiver, |w| w.views[&id].editor().is_some());
    assert_eq!(bytes(&workspace, id), b"AB\r\n");
    workspace
        .execute("edit.insert", &crate::CommandArgs { text: Some("dirty".into()), ..Default::default() }, 1)
        .unwrap();
    let before = workspace.views[&id].editor().unwrap().document().hex_source(id.0);
    assert_eq!(workspace.open_hex(3).unwrap(), OpenOutcome::NeedsRawReopen);
    assert!(!workspace.hex_active());
    assert_eq!(workspace.views[&id].editor().unwrap().document().hex_source(id.0), before);
    workspace.execute("edit.undo", &crate::CommandArgs::default(), 1).unwrap();
    drive(&mut workspace, &receiver, |w| !w.views[&id].editor().unwrap().has_pending_edit_receipt());
    assert_eq!(workspace.open_hex(3).unwrap(), OpenOutcome::Ready);
    assert_eq!(workspace.rows(), 3, "accepted hex entry must retain the requested viewport for transport handoff");
    assert!(workspace.document_busy(id));
    drive(&mut workspace, &receiver, |w| !w.document_busy(id));
    let physical = workspace.hex_source().unwrap();
    assert_eq!(physical.representation, Representation::RawBytes);
    assert_ne!(physical.epoch, before.epoch);
    assert_eq!(bytes(&workspace, id), raw);
    assert!(workspace.files.observed.contains_key(&id));
    edit_and_drain(&mut workspace, &receiver, hex::Input::Paste("EF"));
    let dirty = workspace.hex_source().unwrap();
    assert_eq!(workspace.toggle_hex(3).unwrap(), OpenOutcome::NeedsRawReopen);
    assert_eq!(workspace.hex_source(), Some(dirty));
    edit_and_drain(&mut workspace, &receiver, hex::Input::Undo);
    assert_eq!(workspace.toggle_hex(3).unwrap(), OpenOutcome::Ready);
    drive(&mut workspace, &receiver, |w| !w.document_busy(id));
    assert!(!workspace.hex_active());
    assert_eq!(bytes(&workspace, id), b"AB\r\n");
    assert_eq!(workspace.rows(), 3, "text handoff inherits the authoritative hex viewport");
    assert_eq!(
        workspace.line(0),
        Some(b"AB".as_slice()),
        "text handoff paints worker preview without a mapped UI read"
    );
    assert!(
        workspace.preview.as_ref().is_some_and(|preview| preview.starts_with(b"AB")),
        "handoff must retain the worker preview rather than enter snapshot reload"
    );
    assert_eq!(workspace.line_source(0), None, "preview cache must not claim snapshot-read source windows");
    assert_ne!(workspace.views[&id].editor().unwrap().document().hex_source(id.0).epoch, physical.epoch);
    assert!(workspace.editor_mut().is_some());
    stop(workspace, &receiver);
}

#[test]
fn workspace_hex_full_and_disconnected_admission_restore_editor_session_and_observer() {
    for disconnected in [false, true] {
        let fixture = Fixture::new();
        let (mut workspace, receiver) = channel_workspace(&fixture, 0);
        let mut receiver = Some(receiver);
        if disconnected {
            receiver.take();
        }
        let id = workspace.active_id();
        let before = bytes(&workspace, id);
        let source = workspace.hex_source().unwrap();
        let first = workspace.views[&id].editor().unwrap().next_edit_sequence();
        let error = input(&mut workspace, hex::Input::Paste("FF")).unwrap_err();
        assert_eq!(error.kind(), if disconnected { io::ErrorKind::BrokenPipe } else { io::ErrorKind::WouldBlock });
        assert_eq!(bytes(&workspace, id), before);
        assert_eq!(workspace.hex_source(), Some(source));
        assert!(!workspace.document_busy(id));
        assert!(!workspace.hex_session().unwrap().busy());
        assert_eq!(workspace.views[&id].editor().unwrap().next_edit_sequence(), first);
        // Recovery's original observer remains attached after rejection.
        workspace.views.get_mut(&id).unwrap().editor_mut().unwrap().replace_byte_range(0..1, b"Z").unwrap();
        assert!(workspace.views[&id].editor().unwrap().has_pending_edit_receipt());
    }
}

#[test]
fn workspace_hex_page_loan_caches_scroll_coalesces_latest_and_routes_inactive_result() {
    let fixture = Fixture::new();
    let (mut workspace, receiver) = channel_workspace(&fixture, 8);
    let id = workspace.active_id();
    let first_source = workspace.hex_source().unwrap();
    let damage = input(&mut workspace, hex::Input::Scroll(1)).unwrap();
    assert_eq!(damage.scroll, 1);
    assert!(workspace.document_busy(id));
    assert!(workspace.views[&id].editor().is_none());
    assert!(workspace.hex_session().unwrap().row(Some(first_source), 0).is_some());
    assert_eq!(workspace.save_document(id, None).unwrap_err().kind(), io::ErrorKind::WouldBlock);
    assert_eq!(workspace.close(id, true).unwrap_err().kind(), io::ErrorKind::WouldBlock);
    let DocumentRequest::HexPage { editor, request, .. } = receiver.try_recv().unwrap() else {
        panic!("bounded page loan")
    };
    input(&mut workspace, hex::Input::Scroll(1)).unwrap();
    assert!(receiver.try_recv().is_err(), "scroll while page loan is outstanding must coalesce, never enqueue editors");
    assert_eq!(input(&mut workspace, hex::Input::Paste("FF")).unwrap_err().kind(), io::ErrorKind::WouldBlock);
    let page = hex::read_page(&editor.document().snapshot(), request, || false).unwrap();
    let other = workspace.open(View::new(None)).unwrap();
    let damage =
        workspace.test_document_result(DocumentResult::HexPage { document: id, editor, request, result: Ok(page) });
    assert_eq!(damage, Damage::default(), "inactive hex completion cannot damage another tab body");
    assert_eq!(workspace.active_id(), other);
    assert_eq!(bytes(&workspace, other), b"");
    let mut latest = None;
    while let Ok(request) = receiver.try_recv() {
        if let DocumentRequest::HexPage { editor, request, .. } = request {
            latest = Some((editor, request));
        }
    }
    let (editor, request) = latest.expect("latest page retried after old loan returned");
    assert_eq!(request.top, 2);
    let page = hex::read_page(&editor.document().snapshot(), request, || false).unwrap();
    workspace.test_document_result(DocumentResult::HexPage { document: id, editor, request, result: Ok(page) });
    assert!(workspace.views[&id].editor().is_some());
    assert!(workspace.hex.tabs[&id].session.row(Some(first_source), 0).is_some());
}

#[test]
fn workspace_hex_old_input_and_wrong_generation_preview_never_cross_tabs_or_reopen() {
    let fixture = Fixture::new();
    let (mut workspace, _receiver) = channel_workspace(&fixture, 8);
    let id = workspace.active_id();
    let source = workspace.hex_source().unwrap();
    let epoch = workspace.hex_session().unwrap().input_epoch();
    input(&mut workspace, hex::Input::Digit('F')).unwrap();
    let other = workspace.open(View::new(None)).unwrap();
    workspace.activate(id);
    assert_eq!(workspace.hex_session().unwrap().view().pending_high_nibble(), None);
    assert!(workspace.hex_input(source, epoch, hex::Input::Paste("FF")).is_err());
    assert_eq!(workspace.hex_source(), Some(source));
    assert!(bytes(&workspace, other).is_empty());
    let request = PageRequest { source, generation: 999, top: 0, rows: 2 };
    let page =
        hex::read_page(&workspace.views[&id].editor().unwrap().document().snapshot(), request, || false).unwrap();
    assert_eq!(workspace.hex_preview(id, 0, page), Damage::default());
    assert_eq!(workspace.hex_source(), Some(source));
}

#[test]
fn workspace_hex_reopen_waits_for_journal_and_failed_close_keeps_editor_history() {
    let fixture = Fixture::new();
    let path = fixture.source(b"clean\r\n");
    let file = kwil_core_text::TextFile::open(&path).unwrap();
    file.build_index(1).unwrap();
    let mut workspace = Workspace::new(View::new(Some((path, file)))).unwrap();
    workspace.set_rows(2);
    let id = workspace.active_id();
    let (worker, receiver) = DocumentWorker::replacement_test_channel(8);
    workspace.files.worker = Some(worker);
    workspace.files.storage_ready = true;
    workspace.install_journal_observers();
    let source = workspace.editor().unwrap().document().hex_source(id.0);
    let observed = workspace.files.observed[&id];
    let input_ticket = workspace.editor_input_ticket(id).unwrap();
    assert_eq!(workspace.open_hex(2).unwrap(), OpenOutcome::Ready);
    assert!(
        matches!(receiver.try_recv().unwrap(), DocumentRequest::FinishJournal { document, generation } if document == id && generation == observed.1)
    );
    assert!(receiver.try_recv().is_err(), "raw reopen must await successful JournalClosed");
    assert!(workspace.views[&id].editor().is_some(), "journal wait holds editor without mutation");
    workspace.test_document_result(DocumentResult::JournalClosed {
        document: id,
        result: Err(io::Error::other("fixture journal refusal")),
    });
    assert_eq!(workspace.editor().unwrap().document().hex_source(id.0), source);
    assert_eq!(workspace.files.observed[&id], observed);
    assert!(!workspace.hex_active());
    assert!(!workspace.document_busy(id));
    assert!(
        !workspace.editor_input_is_current(&input_ticket),
        "failed accepted raw transition never revives text input"
    );
    assert!(workspace.editor_input_ticket(id).is_some(), "fresh text input is available after refusal");
}

#[test]
fn workspace_hex_shutdown_backpressure_retries_exact_editor_and_keeps_cached_body() {
    let fixture = Fixture::new();
    let (mut workspace, receiver) = channel_workspace(&fixture, 1);
    let id = workspace.active_id();
    let source = workspace.hex_source().unwrap();
    let sequence = workspace.views[&id].editor().unwrap().next_edit_sequence();
    workspace.files.metadata_durable = true;
    workspace.files.durable.insert(id, (source.revision, sequence - 1));
    assert!(workspace.files.worker.as_ref().unwrap().try_submit(DocumentRequest::SessionTimer).is_ok());
    input(&mut workspace, hex::Input::Digit('F')).unwrap();
    assert!(!workspace.prepare_hex_shutdown());
    assert!(workspace.views[&id].editor().is_none(), "raw editor is retained by the rejected disposal request");
    assert!(workspace.file_saves_pending());
    assert!(workspace.hex_session_mut().is_none());
    let session = workspace.hex_session().unwrap();
    assert!(session.row(Some(source), 0).is_some(), "cached body survives disposal backpressure");
    assert_eq!(session.view().pending_high_nibble(), None);
    assert_eq!(workspace.files.queue.len(), 1);
    assert!(matches!(receiver.try_recv().unwrap(), DocumentRequest::SessionTimer));
    workspace.test_document_result(DocumentResult::CapacityAvailable);
    assert!(workspace.prepare_hex_shutdown());
    let DocumentRequest::DiscardHex { document, editor } = receiver.try_recv().unwrap() else {
        panic!("exact editor disposal request")
    };
    assert_eq!(document, id);
    assert_eq!(editor.document().hex_source(id.0), source);
    assert_eq!(editor.next_edit_sequence(), sequence);
    assert!(receiver.try_recv().is_err(), "disposal wake must never generate a new page/editor loan");
    workspace.checkpoint_session();
    assert!(!workspace.files.persist_pending, "retiring editor slots cannot overwrite durable metadata");
}

#[test]
fn workspace_hex_shutdown_disconnect_retains_raw_editor_without_ui_drop() {
    let fixture = Fixture::new();
    let (mut workspace, receiver) = channel_workspace(&fixture, 0);
    let id = workspace.active_id();
    let source = workspace.hex_source().unwrap();
    let sequence = workspace.views[&id].editor().unwrap().next_edit_sequence();
    workspace.files.metadata_durable = true;
    workspace.files.durable.insert(id, (source.revision, sequence - 1));
    drop(receiver);
    assert!(!workspace.prepare_hex_shutdown());
    assert!(!workspace.prepare_hex_shutdown());
    assert_eq!(workspace.files.queue.len(), 1);
    let DocumentRequest::DiscardHex { document, editor } = workspace.files.queue.front().unwrap() else {
        panic!("retain rejected raw owner")
    };
    assert_eq!(*document, id);
    assert_eq!(editor.document().hex_source(id.0), source);
    assert!(workspace.file_saves_pending());
    assert!(!workspace.wants_exit());
    assert!(workspace.hex_session().unwrap().row(Some(source), 0).is_some());
}

#[test]
fn workspace_hex_clean_recovery_restores_physical_cursor_selection_and_scroll() {
    let fixture = Fixture::new();
    let raw = [0xff; 256];
    let path = fixture.source(&raw);
    let (mut workspace, receiver) = real_workspace(&fixture);
    let id = open_raw(&mut workspace, &receiver, path);
    input(&mut workspace, hex::Input::Move { bytes: 130, extend: false }).unwrap();
    input(&mut workspace, hex::Input::Move { bytes: -17, extend: true }).unwrap();
    input(&mut workspace, hex::Input::ScrollTo(7)).unwrap();
    drive(&mut workspace, &receiver, |w| !w.document_busy(id));
    workspace.checkpoint_session();
    workspace.persist_snapshot();
    workspace.files.queue.push_back(DocumentRequest::FlushJournals);
    workspace.pump_files();
    drive(&mut workspace, &receiver, |w| {
        let editor = w.views[&id].editor().unwrap();
        w.files.metadata_durable
            && w.files.durable.get(&id)
                == Some(&(editor.document().revision(), editor.next_edit_sequence().saturating_sub(1)))
    });
    let previous_epoch = workspace.hex_source().unwrap().epoch;
    let expected = workspace.hex_navigation(id).unwrap();
    assert_eq!(expected, (113, 130, 7));
    stop(workspace, &receiver);
    let (mut recovered, receiver) = real_workspace(&fixture);
    assert_eq!(recovered.active_id(), id);
    assert!(recovered.hex_active());
    assert_eq!(recovered.hex_navigation(id), Some(expected));
    assert!(!recovered.document_busy(id), "an unlaid-out recovered raw view must not loan its editor for zero rows");
    assert_eq!(bytes(&recovered, id), raw);
    assert_ne!(recovered.hex_source().unwrap().epoch, previous_epoch);
    assert_eq!(recovered.hex_source().unwrap().representation, Representation::RawBytes);
    recovered.set_hex_rows(3);
    drive(&mut recovered, &receiver, |w| !w.document_busy(id));
    assert!(recovered.hex_session().unwrap().row(recovered.hex_source(), 0).is_some());
    stop(recovered, &receiver);
}
