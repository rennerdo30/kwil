use super::*;
use crate::{Cursor, Document};
use std::{
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver},
    },
    time::{Duration, Instant},
};

#[test]
fn folder_stream_count_is_hit_only_lower_bound_and_never_counts_revisited_records() {
    let key = SearchKey { document_id: 0, revision: 0, generation: 1 };
    let mut host = SearchSession {
        worker: None,
        generation: 1,
        state: SearchState { key: Some(key), ..Default::default() },
        rejected: None,
        replacement: None,
        files_counted_until: 0,
    };
    let hit = |line_number| {
        FileEvent::Hit(kwil_core_search::FileHit {
            path: PathBuf::from("hit.txt"),
            line_number,
            line_offset: 0,
            line: b"hit".to_vec(),
            line_truncated: false,
            match_ranges: std::iter::once(0..3).collect(),
            ranges_truncated: false,
        })
    };
    let error = || FileEvent::Error { path: PathBuf::from("denied"), message: "denied".into() };
    host.accept(SearchEvent {
        key,
        update: SearchUpdate::Files { first: 0, rows: vec![hit(1), error(), hit(2)], results: None },
    });
    assert_eq!(host.state.count, 2);
    assert_eq!(host.state.files_seen, 3);
    assert_eq!(host.state.status, SearchStatus::Searching);
    host.accept(SearchEvent {
        key,
        update: SearchUpdate::Files { first: 1, rows: vec![error(), hit(2), hit(3)], results: None },
    });
    assert_eq!(host.state.count, 3);
    host.accept(SearchEvent {
        key,
        update: SearchUpdate::FilesDone(FileSearchSummary {
            files: 2,
            matching_lines: 3,
            errors: 1,
            cancelled: false,
            workers: 1,
        }),
    });
    host.accept(SearchEvent {
        key,
        update: SearchUpdate::Files { first: 0, rows: vec![hit(1), error(), hit(2)], results: None },
    });
    assert_eq!(host.state.count, 3, "completed-page revisits preserve exact matching-line count");
}

fn editor(text: &str) -> Editor {
    let mut editor = Editor::new(Document::new());
    editor.insert_text(text, Duration::ZERO).unwrap();
    editor
}
fn session() -> (SearchSession, Receiver<()>) {
    let (tx, rx) = mpsc::channel();
    (
        SearchSession::new(move || {
            let _ = tx.send(());
        })
        .unwrap(),
        rx,
    )
}
fn finish(host: &mut SearchSession, wakes: &Receiver<()>, id: SessionDocumentId, revision: u64) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        host.worker_ready(id, revision);
        if matches!(host.state.status, SearchStatus::Complete | SearchStatus::Failed) {
            break;
        }
        wakes.recv_timeout(deadline.saturating_duration_since(Instant::now())).expect("bounded search wake");
    }
    assert_eq!(host.state.status, SearchStatus::Complete, "{:?}", host.state.error);
}
fn submit(
    host: &mut SearchSession,
    editor: &Editor,
    query: Query,
    selection: bool,
    replacement: Option<Replacement>,
) -> SearchKey {
    let task = SearchSession::document_task(editor, query, selection, Page::default(), None, replacement);
    host.submit(SessionDocumentId(7), editor.document().revision(), task).unwrap()
}

#[test]
fn seed_is_pending_literal_multicursor_and_generation_stamped() {
    let mut editor = editor("Ä.*\n x");
    let cursors = vec![Cursor { anchor: 6, head: 0, desired_column: None }, Cursor::caret(7)];
    editor.set_cursors(cursors.clone(), 0).unwrap();
    let (mut host, wakes) = session();
    let key =
        host.submit(SessionDocumentId(7), editor.document().revision(), SearchSession::seed_task(&editor)).unwrap();
    assert_eq!(host.state.status, SearchStatus::Pending);
    assert!(host.seed_action().is_none());
    finish(&mut host, &wakes, SessionDocumentId(7), editor.document().revision());
    let seed = host.state.seed.as_ref().unwrap();
    assert_eq!(seed.text, "Ä.*\n ");
    assert!(seed.from_selection);
    let action = host.seed_action().unwrap();
    assert_eq!(action.key(), key);
    assert!(host.validate_action(&action, SessionDocumentId(7), editor.document().revision()));
    let SearchAction::SeedSelections { cursors: selected, primary, .. } = &action else { panic!("seed endpoints") };
    assert_eq!(selected, &[SeedCursor { anchor: 6, head: 0 }, SeedCursor { anchor: 7, head: 7 }]);
    assert_eq!(*primary, 0);
    assert_eq!(editor.cursors(), cursors);
    submit(&mut host, &editor, Query::literal("x".into()), false, None);
    assert!(!host.validate_action(&action, SessionDocumentId(7), editor.document().revision()));
    assert!(host.state.seed.is_none());
    host.cancel();
    assert!(host.seed_action().is_none());
}

#[test]
fn seed_eof_and_empty_document_emit_typed_selection_or_no_edit() {
    let mut editor = editor("one two");
    editor.set_cursors(vec![Cursor::caret(1), Cursor::caret(7)], 1).unwrap();
    let (mut host, wakes) = session();
    host.submit(SessionDocumentId(7), editor.document().revision(), SearchSession::seed_task(&editor)).unwrap();
    finish(&mut host, &wakes, SessionDocumentId(7), editor.document().revision());
    let seed = host.state.seed.as_ref().unwrap();
    assert_eq!((seed.text.as_str(), seed.range.clone()), ("two", 4..7));
    assert_eq!(seed.cursors, [SeedCursor { anchor: 0, head: 3 }, SeedCursor { anchor: 4, head: 7 }]);
    assert!(!host.validate_action(
        &host.seed_action().unwrap(),
        SessionDocumentId(7),
        editor.document().revision() + 1
    ));
    let empty = Editor::new(Document::new());
    host.submit(SessionDocumentId(8), 0, SearchSession::seed_task(&empty)).unwrap();
    finish(&mut host, &wakes, SessionDocumentId(8), 0);
    assert!(host.state.seed.is_none());
    assert!(host.seed_action().is_none());
}

#[test]
fn newest_seed_cancels_pending_origin_and_escape_clears_result() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let first = AtomicBool::new(true);
    let (wake_tx, wakes) = mpsc::channel();
    let mut host = SearchSession::new(move || {
        let _ = wake_tx.send(());
        if first.swap(false, Ordering::AcqRel) {
            entered_tx.send(()).unwrap();
            release_rx.lock().unwrap().recv_timeout(Duration::from_secs(5)).unwrap();
        }
    })
    .unwrap();
    let old = editor("old");
    let new = editor("new");
    host.submit(SessionDocumentId(7), old.document().revision(), SearchSession::seed_task(&old)).unwrap();
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let newest = host.submit(SessionDocumentId(7), new.document().revision(), SearchSession::seed_task(&new)).unwrap();
    release_tx.send(()).unwrap();
    finish(&mut host, &wakes, SessionDocumentId(7), new.document().revision());
    assert_eq!(host.key(), Some(newest));
    assert_eq!(host.state.seed.as_ref().unwrap().text, "new");
    host.cancel();
    assert!(host.state.seed.is_none());
    assert!(host.seed_action().is_none());
    host.take_shutdown().unwrap().shutdown().unwrap();
}

#[test]
fn repeated_shutdown_returns_external_owner_and_retirement_is_explicit() {
    let editor = editor("owned source");
    let (mut host, _) = session();
    let request = || SearchRequest {
        key: SearchKey { document_id: 7, revision: editor.document().revision(), generation: 1 },
        task: SearchSession::seed_task(&editor),
    };
    let worker = host.take_shutdown().unwrap();
    let retained = host.retire_request(request()).err().unwrap();
    let retained = match host.take_shutdown_with(Some(*retained)) {
        Err(Some(retained)) => retained,
        _ => panic!("stopped host must return external ownership"),
    };
    let SearchTask::SeedSelectionOrWord(seed) = &retained.task else { panic!("retained seed") };
    assert_eq!(seed.snapshot.len(), 12);
    // Caller retains the owner and can transfer it to a distinct live retirement
    // lane; no stopped-session helper silently drops it on the input thread.
    let retirement = SearchWorker::new(|| {}).unwrap();
    retirement.close_with_requests(None, Some(*retained));
    retirement.shutdown().unwrap();
    worker.shutdown().unwrap();
}

#[test]
fn pending_bounded_page_exact_count_and_signal() {
    let editor = editor(&"x ".repeat(600));
    let (mut host, wakes) = session();
    let task = SearchSession::document_task(
        &editor,
        Query::literal("x".into()),
        false,
        Page { first: 300, len: 17 },
        None,
        None,
    );
    host.submit(SessionDocumentId(7), editor.document().revision(), task).unwrap();
    assert_eq!(host.state.status, SearchStatus::Pending);
    assert!(host.state.hits.is_empty());
    finish(&mut host, &wakes, SessionDocumentId(7), editor.document().revision());
    assert_eq!(host.state.first, 300);
    assert_eq!(host.state.hits.len(), 17);
    assert_eq!(host.state.hits[0], Match { offset: 600, len: 1 });
    assert_eq!(host.state.count, 600);
    assert_eq!(host.state.signal.iter().sum::<u64>(), 600);
    assert_eq!(host.state.scanned, 1200);
}

#[test]
fn regex_capture_recipe_preserves_disjoint_selection_and_multicursors() {
    let mut editor = editor("x1 x2 x3");
    editor
        .set_cursors(
            vec![
                Cursor { anchor: 0, head: 2, desired_column: None },
                Cursor { anchor: 6, head: 8, desired_column: None },
            ],
            1,
        )
        .unwrap();
    let (mut host, wakes) = session();
    let mut query = Query::literal("x([0-9])".into());
    query.literal = false;
    submit(
        &mut host,
        &editor,
        query,
        true,
        Some(Replacement { template: b"${1}-$0".to_vec(), target: ReplaceTarget::All }),
    );
    finish(&mut host, &wakes, SessionDocumentId(7), editor.document().revision());
    assert_eq!(host.state.hits, [Match { offset: 0, len: 2 }, Match { offset: 6, len: 2 }]);
    let action = host.replacement_action(SessionDocumentId(7), &editor).unwrap().unwrap();
    assert!(host.validate_action(&action, SessionDocumentId(7), editor.document().revision()));
    let SearchAction::Replace(plan) = action else { panic!("replace recipe") };
    let edits = plan.edits.iter().collect::<io::Result<Vec<_>>>().unwrap();
    assert_eq!(
        edits.iter().map(|e| (e.offset, e.len, e.new_bytes.as_slice())).collect::<Vec<_>>(),
        [(0, 2, b"1-x1".as_slice()), (6, 2, b"3-x3".as_slice())]
    );
    assert!(host.replacement_action(SessionDocumentId(7), &editor).unwrap().is_none());
    assert_eq!(editor.cursors().len(), 2); // worker planning never mutates editor.
}

#[test]
fn replace_all_spools_beyond_inline_cap_and_reverses_without_index() {
    let editor = editor(&"a ".repeat(5000));
    let (mut host, wakes) = session();
    submit(
        &mut host,
        &editor,
        Query::literal("a".into()),
        false,
        Some(Replacement { template: b"$$1".to_vec(), target: ReplaceTarget::All }),
    );
    finish(&mut host, &wakes, SessionDocumentId(7), editor.document().revision());
    let SearchAction::Replace(plan) = host.replacement_action(SessionDocumentId(7), &editor).unwrap().unwrap() else {
        panic!("replace recipe")
    };
    assert!(matches!(plan.edits, ReplacementEdits::Spool(_)));
    assert_eq!(plan.edits.len(), 5000);
    let mut edits = plan.edits.iter();
    assert_eq!(edits.next().unwrap().unwrap().offset, 0);
    let last = edits.next_back().unwrap().unwrap();
    assert_eq!((last.offset, last.new_bytes.as_slice()), (9998, b"$$1".as_slice()));
    let mut clone = edits.clone();
    assert_eq!(clone.next_back().unwrap().unwrap().offset, 9996);
    assert_eq!(edits.next().unwrap().unwrap().offset, 2);
    drop((edits, clone, plan)); // pins notify worker retirement, no UI unlink.
}

#[test]
fn journal_backpressure_retains_exact_replace_recipe_for_retry() {
    let mut editor = editor("cat");
    let blocked = Arc::new(Mutex::new(true));
    let gate = blocked.clone();
    editor
        .set_edit_observer(Some(Box::new(move |receipt| if *gate.lock().unwrap() { Err(receipt) } else { Ok(()) })))
        .unwrap();
    editor.insert_text("!", Duration::from_secs(1)).unwrap();
    assert!(editor.has_pending_edit_receipt());
    let (mut host, wakes) = session();
    submit(
        &mut host,
        &editor,
        Query::literal("cat".into()),
        false,
        Some(Replacement { template: b"dog".to_vec(), target: ReplaceTarget::All }),
    );
    finish(&mut host, &wakes, SessionDocumentId(7), editor.document().revision());
    assert_eq!(host.replacement_action(SessionDocumentId(7), &editor).unwrap_err().kind(), io::ErrorKind::WouldBlock);
    assert!(host.replacement.is_some());
    *blocked.lock().unwrap() = false;
    editor.flush_edit_receipt().unwrap();
    let SearchAction::Replace(plan) = host.replacement_action(SessionDocumentId(7), &editor).unwrap().unwrap() else {
        panic!("replace recipe")
    };
    assert_eq!(plan.edits.len(), 1);
    assert_eq!(plan.edits.iter().next().unwrap().unwrap().new_bytes, b"dog");
}

#[test]
fn stale_generation_document_revision_and_cancel_reject_actions() {
    let mut editor = editor("cat dog");
    let (mut host, wakes) = session();
    let old = submit(&mut host, &editor, Query::literal("cat".into()), false, None);
    let new = submit(&mut host, &editor, Query::literal("dog".into()), false, None);
    assert_ne!(old, new);
    host.accept(SearchEvent { key: old, update: SearchUpdate::Failed("stale".into()) });
    assert!(host.state.error.is_none());
    finish(&mut host, &wakes, SessionDocumentId(7), editor.document().revision());
    let action = host.selection_action(0, SelectionMode::AddCursor).unwrap();
    let SearchAction::Select { range, mode, .. } = &action else { panic!("selection") };
    assert_eq!((range.clone(), *mode), (4..7, SelectionMode::AddCursor));
    assert!(!host.validate_action(&action, SessionDocumentId(8), editor.document().revision()));
    assert!(!host.validate_action(&action, SessionDocumentId(7), editor.document().revision() + 1));
    host.cancel();
    assert!(!host.validate_action(&action, SessionDocumentId(7), editor.document().revision()));
    assert!(host.state.hits.is_empty());
    submit(&mut host, &editor, Query::literal("cat".into()), false, None);
    editor.insert_text("!", Duration::from_secs(1)).unwrap();
    assert!(host.worker_ready(SessionDocumentId(7), editor.document().revision()));
    assert_eq!(host.state.status, SearchStatus::Cancelled);
}

#[test]
fn next_previous_navigation_wraps_independent_of_visible_page() {
    let editor = editor("a a a");
    let (mut host, wakes) = session();
    for (navigation, expected) in [
        (Navigation::Next { from: 4, include_current: false }, 0),
        (Navigation::Previous { from: 0, include_current: false }, 4),
        (Navigation::Next { from: 2, include_current: true }, 2),
    ] {
        let task = SearchSession::document_task(
            &editor,
            Query::literal("a".into()),
            false,
            Page { first: 90, len: 1 },
            Some(navigation),
            None,
        );
        host.submit(SessionDocumentId(7), editor.document().revision(), task).unwrap();
        finish(&mut host, &wakes, SessionDocumentId(7), editor.document().revision());
        assert!(host.state.hits.is_empty());
        let SearchAction::Select { range, .. } = host.navigation_action(SelectionMode::Replace).unwrap() else {
            panic!("navigation")
        };
        assert_eq!(range, expected..expected + 1);
    }
}

#[test]
fn empty_selection_and_unicode_overlap_keep_engine_contract() {
    let mut editor = editor("xa a a K k");
    let (mut host, wakes) = session();
    let mut query = Query::literal("a a".into());
    query.options.whole_word = true;
    submit(&mut host, &editor, query, false, None);
    finish(&mut host, &wakes, SessionDocumentId(7), editor.document().revision());
    assert_eq!(host.state.hits, [Match { offset: 3, len: 3 }]);
    let mut query = Query::literal("k".into());
    query.options.case_insensitive = true;
    submit(&mut host, &editor, query, false, None);
    finish(&mut host, &wakes, SessionDocumentId(7), editor.document().revision());
    assert_eq!(host.state.hits, [Match { offset: 7, len: 3 }, Match { offset: 11, len: 1 }]);
    editor.set_cursors(vec![Cursor::caret(0)], 0).unwrap();
    submit(&mut host, &editor, Query::literal("a".into()), true, None);
    finish(&mut host, &wakes, SessionDocumentId(7), editor.document().revision());
    assert_eq!(host.state.count, 0);
}

#[test]
fn native_file_action_uses_original_offsets_and_preserves_non_unicode_path() {
    let (mut host, _wakes) = session();
    let key = SearchKey { document_id: 0, revision: 0, generation: 1 };
    host.state.key = Some(key);
    #[cfg(windows)]
    let path = {
        use std::os::windows::ffi::OsStringExt;
        PathBuf::from(std::ffi::OsString::from_wide(&[0x61, 0xd800, 0x62]))
    };
    #[cfg(unix)]
    let path = {
        use std::os::unix::ffi::OsStringExt;
        PathBuf::from(std::ffi::OsString::from_vec(vec![b'a', 0xff, b'b']))
    };
    host.accept(SearchEvent {
        key,
        update: SearchUpdate::Files {
            first: 64,
            results: None,
            rows: vec![FileEvent::Hit(kwil_core_search::FileHit {
                path: path.clone(),
                line_number: 3,
                line_offset: 82,
                line: b"preview".to_vec(),
                line_truncated: false,
                match_ranges: std::iter::once(20..24).collect(),
                ranges_truncated: false,
            })],
        },
    });
    let SearchAction::OpenFile { path: actual, range, line_number, .. } = host.file_action(0, 0).unwrap() else {
        panic!("native file action")
    };
    assert_eq!(actual.as_os_str(), path.as_os_str());
    assert_eq!((range, line_number), (102..106, 3));
    assert_eq!(host.state.file_first, 64);
}

#[test]
fn file_results_revisit_earliest_page_after_stream_and_refuse_old_session() {
    let path = std::env::temp_dir().join(format!(
        "kwil-search-page-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir(&path).unwrap();
    let file = path.join("matches.txt");
    std::fs::write(&file, "hit\n".repeat(90)).unwrap();
    let (mut host, wakes) = session();
    let mut query = FileSearch::new(path.clone(), "hit".into());
    query.literal = true;
    query.workers = 1;
    host.submit_files(query).unwrap();
    finish(&mut host, &wakes, SessionDocumentId(7), 0);
    assert_eq!(host.state.files_seen, 90);
    assert_eq!(host.state.file_first, 64);
    assert!(host.state.files.len() <= 32);
    let original = host.state.file_results.as_ref().unwrap().clone();
    let original_session = original.session();
    host.request_file_page(Page { first: 0, len: 5 }).unwrap().unwrap();
    finish(&mut host, &wakes, SessionDocumentId(7), 0);
    assert_eq!(host.state.file_first, 0);
    assert_eq!(host.state.files.len(), 5);
    assert_eq!(host.state.file_results.as_ref().unwrap().session(), original_session);
    assert_eq!(host.state.files_seen, 90);
    assert_eq!(host.state.count, 90);
    assert_eq!(host.state.file_summary.as_ref().unwrap().matching_lines, 90);
    let SearchAction::OpenFile { path: first, range, line_number, .. } = host.file_action(0, 0).unwrap() else {
        panic!("first result navigation")
    };
    assert_eq!(first, file);
    assert_eq!((range, line_number), (0..3, 1));
    host.cancel();
    let stale = SearchRequest {
        key: SearchKey { document_id: 0, revision: 0, generation: 900 },
        task: SearchTask::FilePage { results: original, page: Page::default() },
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut request = stale;
    let rejected = loop {
        let rejected = host.worker.as_ref().unwrap().submit(request).unwrap_err();
        if rejected.reason != "search mailbox busy; retry on wake" {
            break rejected;
        }
        request = *rejected.request;
        wakes
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("bounded stale-page mailbox wake");
    };
    assert_eq!(rejected.reason, "stale file result session");
    drop(rejected); // a result lease Drop only notifies its explicit worker owner.
    host.stop();
    std::fs::remove_file(&file).unwrap();
    std::fs::remove_dir(&path).unwrap();
}
