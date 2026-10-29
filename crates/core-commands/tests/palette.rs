use kwil_core_commands::palette::*;
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::Duration;

#[test]
fn theme_commands_are_available_in_the_palette() {
    for id in ["theme.select", "theme.preview", "theme.import", "theme.followSystem"] {
        assert!(kwil_core_commands::in_palette(id), "theme command must be hosted: {id}");
    }
}

struct Worker {
    index: PaletteWorker,
    wake: mpsc::Receiver<()>,
}
impl Worker {
    fn new(limit: usize) -> Self {
        let (tx, wake) = mpsc::sync_channel(1);
        let index = PaletteWorker::new(
            IndexLimits { max_files: 16, max_path_bytes: 4096, result_limit: limit },
            Arc::new(move || {
                let _ = tx.try_send(());
            }),
        )
        .unwrap();
        Self { index, wake }
    }
    fn submit(&self, p: &mut Palette, request: PaletteQuery) {
        self.index.request(request.clone()).unwrap();
        let result = self.wait_for(&request, "worker did not publish the requested mode/generation");
        assert!(p.apply_results(result));
    }
    fn wait_for(&self, request: &PaletteQuery, failure: &str) -> Arc<PaletteResults> {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            let result = self.index.latest();
            if result.generation == request.generation
                && result.mode == request.candidates.mode()
                && result.query == request.query
            {
                return result;
            }
            self.wake.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())).expect(failure);
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.index.stop().unwrap().join().unwrap();
    }
}
fn file_result(request: &PaletteQuery, path: &str) -> Arc<PaletteResults> {
    Arc::new(PaletteResults {
        generation: request.generation,
        mode: PaletteMode::Files,
        query: request.query.clone(),
        rows: Arc::from([PaletteRow {
            title: path.into(),
            category: "File".into(),
            action: PaletteAction::OpenFile(path.into()),
        }]),
        total: 1,
    })
}

#[test]
fn commands_and_tabs_are_scheduled_without_ui_matching() {
    let mut p = Palette::new(8);
    let command = p.open(">file.save").unwrap().expect("commands must be scheduled on worker");
    assert!(p.rows().is_empty(), "cold command results must not be scored on UI");
    assert!(!p.results_are_fresh());
    p.set_tabs(vec![Tab { id: 91, title: "alpha".into() }, Tab { id: 12, title: "beta".into() }]).unwrap();
    let tabs = p.open("#beta").unwrap().expect("tabs must be scheduled on worker");
    assert!(p.rows().is_empty(), "cold tab results must not be scored on UI");
    let worker = Worker::new(8);
    worker.submit(&mut p, tabs);
    assert_eq!(p.accept(), Some(PaletteAction::ActivateTab(12)));
    // A completed command result from another mode/lifetime stays stale.
    worker.index.request(command.clone()).unwrap();
    let old_command = worker.wait_for(&command, "command worker did not finish");
    p.open("#alpha").unwrap();
    assert!(!p.apply_results(old_command), "stale command result crossed into tabs");
    let request = p.open(">file.save").unwrap().unwrap();
    worker.submit(&mut p, request);
    assert!(p.rows().iter().any(|r| r.action == PaletteAction::Command("file.save")));
    assert!(p.rows().iter().all(|r| !r.category.is_empty()));
}

#[test]
fn one_based_locations() {
    let mut p = Palette::new(8);
    assert!(p.open(":42:7").unwrap().is_none());
    assert_eq!(p.mode(), PaletteMode::Line);
    assert_eq!(p.accept(), Some(PaletteAction::GoTo { line: 42, column: 7 }), "one-based line/column action changed");
    assert!(!p.is_open());
    for invalid in [":0", ":1:0", ":-1", ":1:2:3", ":", ":1:", ":184467440737095516160"] {
        p.open(invalid).unwrap();
        assert!(p.accept().is_none(), "accepted {invalid}");
    }
    p.set_query(":9").unwrap();
    assert_eq!(p.accept(), Some(PaletteAction::GoTo { line: 9, column: 1 }));
}

#[test]
fn changing_parent_tab_ids_invalidates_same_query_results() {
    let worker = Worker::new(2);
    let mut p = Palette::new(2);
    p.set_tabs(vec![Tab { id: 1, title: "alpha".into() }]).unwrap();
    let old_query = p.open("#alpha").unwrap().unwrap();
    worker.index.request(old_query.clone()).unwrap();
    let old_result = worker.wait_for(&old_query, "old tab query did not finish");
    assert!(p.apply_results(old_result.clone()));
    let request = p.set_tabs(vec![Tab { id: 2, title: "alpha".into() }]).unwrap().unwrap();
    assert!(p.rows().is_empty(), "changed tab IDs retained cached selection");
    assert!(!p.apply_results(old_result));
    worker.submit(&mut p, request);
    assert_eq!(p.accept(), Some(PaletteAction::ActivateTab(2)));
}

#[test]
fn grapheme_backspace_mode_transitions_wrap_and_caps() {
    let worker = Worker::new(2);
    let mut p = Palette::new(2);
    p.set_tabs(vec![
        Tab { id: 91, title: "alpha".into() },
        Tab { id: 12, title: "beta".into() },
        Tab { id: 7, title: "gamma".into() },
    ])
    .unwrap();
    let request = p.open("#").unwrap().unwrap();
    worker.submit(&mut p, request);
    assert_eq!(p.rows().len(), 2);
    p.previous();
    assert_eq!(p.selected(), Some(1));
    p.next();
    assert_eq!(p.selected(), Some(0));
    p.insert("e\u{301}👩‍👩‍👧‍👦").unwrap();
    p.backspace();
    assert_eq!(p.query(), "#e\u{301}", "backspace must remove the whole joined emoji");
    p.backspace();
    assert_eq!(p.query(), "#");
    p.backspace();
    assert_eq!(p.mode(), PaletteMode::Files);
    let request = p.set_query("#beta").unwrap().unwrap();
    worker.submit(&mut p, request);
    assert_eq!(p.accept(), Some(PaletteAction::ActivateTab(12)));
    let request = p.open(">").unwrap().unwrap();
    worker.submit(&mut p, request);
    assert_eq!(p.rows().len(), 2);
    p.cancel();
    assert!(!p.is_open());
    assert!(p.accept().is_none());
}

#[test]
fn stale_generations_cached_rows_and_cancel_cannot_accept_old_paths() {
    let mut p = Palette::new(2);
    let first = p.open("one").unwrap().unwrap();
    let old = file_result(&first, "one.txt");
    assert!(p.apply_results(old.clone()));
    let second = p.set_query("two").unwrap().unwrap();
    assert_eq!(p.rows().len(), 1, "keep immediate cached rows");
    assert!(p.accept().is_none(), "cached stale row must not be accepted");
    assert!(!p.apply_results(old));
    assert!(p.apply_results(file_result(&second, "two.txt")));
    assert_eq!(p.accept(), Some(PaletteAction::OpenFile(PathBuf::from("two.txt"))));
    p.open("two").unwrap();
    p.cancel();
    assert!(p.accept().is_none());
}

#[test]
fn reopening_identical_query_rejects_previous_lifetime_results() {
    let mut p = Palette::new(2);
    let first = p.open("same").unwrap().unwrap();
    let old = file_result(&first, "same.txt");
    p.cancel();
    p.open("same").unwrap();
    assert!(!p.apply_results(old), "previous palette lifetime must not apply");
    assert!(!p.results_are_fresh());
}

#[test]
fn query_limits_are_atomic_and_no_tabs_have_empty_selection() {
    let mut p = Palette::new(8);
    p.open("#").unwrap();
    p.next();
    p.previous();
    assert_eq!(p.selected(), None);
    p.insert(&"x".repeat(MAX_QUERY_BYTES + 1)).expect_err("overlong insert must be rejected atomically");
    assert_eq!(p.query(), "#");
}

#[test]
fn query_caret_edits_and_deletes_whole_graphemes() {
    let mut p = Palette::new(8);
    p.open("#ae\u{301}👩‍👩‍👧‍👦z").unwrap();
    p.move_left();
    p.move_left();
    assert_eq!(p.cursor(), "#ae\u{301}".len());
    p.delete_forward();
    assert_eq!(p.query(), "#ae\u{301}z", "forward delete must remove the whole joined emoji");
    p.backspace();
    assert_eq!(p.query(), "#az");
    p.insert("東京").unwrap();
    assert_eq!(p.query(), "#a東京z");
    p.move_home();
    p.delete_forward();
    assert_eq!(p.mode(), PaletteMode::Files);
    p.move_right();
    p.backspace();
    assert_eq!(p.query(), "東京z");
    p.move_end();
    assert_eq!(p.cursor(), p.query().len());
    p.set_query("🇦\n🇧").unwrap();
    p.move_home();
    p.move_right();
    p.delete_forward();
    assert_eq!(p.query(), "🇦🇧");
    assert_eq!(p.cursor(), p.query().len(), "deletion joined clusters around the caret");
    p.backspace();
    assert_eq!(p.query(), "");
}

#[test]
fn first_key_has_bounded_cached_files_and_cancel_stops_feed() {
    let worker = Worker::new(2);
    let mut feed = worker.index.feed();
    for name in ["alpha.rs", "beta.rs", "東京.rs", "alphabet.rs"] {
        feed.push(PathBuf::from(name), name.into()).unwrap();
    }
    let initial = PaletteQuery { generation: 1, query: "".into(), candidates: PaletteCandidates::Files };
    worker.index.request(initial.clone()).unwrap();
    let cached = worker.wait_for(&initial, "file cache not ready");
    assert_eq!(cached.total, 4);
    let mut p = Palette::new(2);
    assert!(p.prime_results(cached));
    p.open("a").unwrap();
    assert_eq!(p.rows().len(), 2, "first key has default cached rows");
    assert!(!p.results_are_fresh());
    assert!(p.accept().is_none());
    worker.index.cancel();
    assert!(feed.push(PathBuf::from("late"), "late".into()).is_err(), "cancelled feed must reject new paths");
}

#[test]
fn newest_query_is_published_after_both_requests_are_submitted() {
    let worker = Worker::new(2);
    let mut feed = worker.index.feed();
    feed.push(PathBuf::from("alpha.rs"), "alpha.rs".into()).unwrap();
    feed.push(PathBuf::from("東京.rs"), "東京.rs".into()).unwrap();
    worker
        .index
        .request(PaletteQuery { generation: 1, query: "alpha".into(), candidates: PaletteCandidates::Files })
        .unwrap();
    let newest = PaletteQuery { generation: 2, query: "東京".into(), candidates: PaletteCandidates::Files };
    worker.index.request(newest.clone()).unwrap();
    // Both submissions precede the first result assertion/wait. In particular,
    // there is no cache/selection assertion that can mask a broken query slot.
    let result =
        worker.wait_for(&newest, "newest query generation was not published after both requests were submitted");
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].action, PaletteAction::OpenFile(PathBuf::from("東京.rs")));
}

#[test]
fn feed_bounds_paths_and_preserves_os_identity() {
    let (tx, rx) = mpsc::sync_channel(1);
    let mut index = PaletteWorker::new(
        IndexLimits { max_files: 1, max_path_bytes: 1024, result_limit: 4 },
        Arc::new(move || {
            let _ = tx.try_send(());
        }),
    )
    .unwrap();
    let mut feed = index.feed();
    #[cfg(windows)]
    let path = {
        use std::os::windows::ffi::OsStringExt;
        PathBuf::from(std::ffi::OsString::from_wide(&[0x0061, 0xD800, 0x0062]))
    };
    #[cfg(unix)]
    let path = {
        use std::os::unix::ffi::OsStringExt;
        PathBuf::from(std::ffi::OsString::from_vec(vec![b'a', 0xff, b'b']))
    };
    feed.push(path.clone(), "display only".into()).unwrap();
    assert_eq!(feed.push(PathBuf::from("overflow"), "overflow".into()), Err(FeedError::Capacity));
    let mut p = Palette::new(2);
    let request = p.open("").unwrap().unwrap();
    index.request(request).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while !p.apply_results(index.latest()) {
        rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())).unwrap();
    }
    assert_eq!(p.accept(), Some(PaletteAction::OpenFile(path)), "native path identity changed in worker result");
    index.stop().unwrap().join().unwrap();
}
