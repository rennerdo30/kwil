use kwil_core_commands::palette::*;
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::Duration;

#[test]
fn modes_commands_and_one_based_locations() {
    let mut p = Palette::new(8);
    p.open(">file.save").unwrap();
    assert_eq!(p.mode(), PaletteMode::Commands);
    assert!(p.rows().iter().any(|r| r.action == PaletteAction::Command("file.save")));
    assert!(p.rows().iter().all(|r| !r.category.is_empty()));
    p.set_query(":42:7").unwrap();
    assert_eq!(p.mode(), PaletteMode::Line);
    assert_eq!(p.accept(), Some(PaletteAction::GoTo { line: 42, column: 7 }));
    assert!(!p.is_open());
    for invalid in [":0", ":1:0", ":-1", ":1:2:3", ":", ":1:", ":184467440737095516160"] {
        p.open(invalid).unwrap();
        assert!(p.accept().is_none(), "accepted {invalid}");
    }
    p.set_query(":9").unwrap();
    assert_eq!(p.accept(), Some(PaletteAction::GoTo { line: 9, column: 1 }));
}

#[test]
fn grapheme_backspace_mode_transitions_wrap_and_caps() {
    let mut p = Palette::new(2);
    p.set_tabs(vec![
        Tab { id: 91, title: "alpha".into() },
        Tab { id: 12, title: "beta".into() },
        Tab { id: 7, title: "gamma".into() },
    ])
    .unwrap();
    p.open("#").unwrap();
    assert_eq!(p.rows().len(), 2);
    p.previous();
    assert_eq!(p.selected(), Some(1));
    p.next();
    assert_eq!(p.selected(), Some(0));
    p.insert("e\u{301}👩‍👩‍👧‍👦").unwrap();
    p.backspace();
    assert_eq!(p.query(), "#e\u{301}");
    p.backspace();
    assert_eq!(p.query(), "#");
    p.backspace();
    assert_eq!(p.mode(), PaletteMode::Files);
    p.set_query("#beta").unwrap();
    assert_eq!(p.accept(), Some(PaletteAction::ActivateTab(12)));
    p.open(">").unwrap();
    assert_eq!(p.rows().len(), 2);
    p.cancel();
    assert!(!p.is_open());
    assert!(p.accept().is_none());
}

#[test]
fn stale_generations_cached_rows_and_cancel_cannot_accept_old_paths() {
    let mut p = Palette::new(2);
    let first = p.open("one").unwrap().unwrap();
    let old = Arc::new(FileResults {
        generation: first.generation,
        query: first.query.clone(),
        files: vec![Arc::new(FileEntry::new(PathBuf::from("one.txt"), "one.txt".into()))],
        total: 1,
    });
    assert!(p.apply_files(old.clone()));
    let second = p.set_query("two").unwrap().unwrap();
    assert_eq!(p.rows().len(), 1, "keep immediate cached rows");
    assert!(p.accept().is_none(), "cached stale row is not a fresh selection");
    assert!(!p.apply_files(old));
    assert!(p.apply_files(Arc::new(FileResults {
        generation: second.generation,
        query: second.query,
        files: vec![Arc::new(FileEntry::new(PathBuf::from("two.txt"), "two.txt".into()))],
        total: 1
    })));
    assert_eq!(p.accept(), Some(PaletteAction::OpenFile(PathBuf::from("two.txt"))));
    p.open("two").unwrap();
    p.cancel();
    assert!(p.accept().is_none());
}

#[test]
fn reopening_identical_query_rejects_previous_lifetime_results() {
    let mut p = Palette::new(2);
    let first = p.open("same").unwrap().unwrap();
    let old = Arc::new(FileResults { generation: first.generation, query: first.query, files: vec![], total: 0 });
    p.cancel();
    p.open("same").unwrap();
    assert!(!p.apply_files(old));
    assert!(!p.results_are_fresh());
}

#[test]
fn query_limits_are_atomic_and_no_tabs_have_empty_selection() {
    let mut p = Palette::new(8);
    p.open("#").unwrap();
    p.next();
    p.previous();
    assert_eq!(p.selected(), None);
    assert!(p.insert(&"x".repeat(MAX_QUERY_BYTES + 1)).is_err());
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
    assert_eq!(p.query(), "#ae\u{301}z");
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
fn worker_matches_bounded_results_and_newest_query_wins() {
    let (tx, rx) = mpsc::sync_channel(1);
    let mut index = FileIndex::new(
        IndexLimits { max_files: 16, max_path_bytes: 4096, result_limit: 2 },
        Arc::new(move || {
            let _ = tx.try_send(());
        }),
    )
    .unwrap();
    let mut feed = index.feed();
    for name in ["alpha.rs", "beta.rs", "東京.rs", "alphabet.rs"] {
        feed.push(PathBuf::from(name), name.into()).unwrap();
    }
    let ready_by = std::time::Instant::now() + Duration::from_secs(3);
    let cached = loop {
        let snapshot = index.latest();
        if snapshot.total == 4 {
            break snapshot;
        }
        rx.recv_timeout(ready_by.saturating_duration_since(std::time::Instant::now())).unwrap();
    };
    let mut palette = Palette::new(2);
    assert!(palette.prime_files(cached));
    palette.open("a").unwrap();
    assert_eq!(palette.rows().len(), 2, "first key has default cached rows");
    assert!(!palette.results_are_fresh());
    assert!(palette.accept().is_none());
    index.request(FileQuery { generation: 1, query: "alpha".into() }).unwrap();
    index.request(FileQuery { generation: 2, query: "東京".into() }).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let result = index.latest();
        if result.generation == 2 {
            assert_eq!(result.files.len(), 1);
            assert_eq!(result.files[0].path, PathBuf::from("東京.rs"));
            break;
        }
        rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())).unwrap();
    }
    index.cancel();
    assert!(feed.push(PathBuf::from("late"), "late".into()).is_err());
    index.stop().unwrap().join().unwrap();
}

#[test]
fn feed_bounds_paths_and_preserves_os_identity() {
    let (tx, rx) = mpsc::sync_channel(1);
    let mut index = FileIndex::new(
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
    while !p.apply_files(index.latest()) {
        rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())).unwrap();
    }
    assert_eq!(p.accept(), Some(PaletteAction::OpenFile(path)));
    index.stop().unwrap().join().unwrap();
}
