use crate::*;
use std::io;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::sync_channel;
use std::time::Duration;

fn token() -> Cancellation {
    Arc::new(AtomicBool::new(false))
}
fn literal(s: &[u8]) -> Pattern {
    Pattern::literal(s, Options::default()).unwrap()
}
fn regex(s: &str) -> Pattern {
    Pattern::regex(s, Options::default(), 32).unwrap()
}
struct Chunks<'a> {
    bytes: &'a [u8],
    size: usize,
}
impl Haystack for Chunks<'_> {
    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }
    fn chunks(&self, r: Range<u64>) -> impl Iterator<Item = (u64, &[u8])> {
        self.bytes[r.start as usize..r.end as usize]
            .chunks(self.size)
            .enumerate()
            .map(move |(i, b)| (r.start + (i * self.size) as u64, b))
    }
}
fn collect<H: Haystack + ?Sized>(h: &H, p: &Pattern, r: Range<u64>) -> Vec<Match> {
    let mut hits = Vec::new();
    let result = search(h, p, r, SearchMode::Hits, &token(), |m| hits.push(m), |_| {}).unwrap();
    assert_eq!(result.count as usize, hits.len());
    assert!(!result.cancelled);
    hits
}

#[test]
fn tiny_chunks_and_work_windows_match_reference() {
    let mut bytes = b"prefix ".to_vec();
    for _ in 0..25 {
        bytes.extend_from_slice(b"aba ababa ab\ncd .. ");
    }
    for size in [1, 2, 3, 7, 61, 127, 256] {
        let h = Chunks { bytes: &bytes, size };
        for (p, re) in [
            (literal(b"aba"), "aba"),
            (regex(r"ab\ncd"), r"ab\ncd"),
            (regex(r"aba|\.\."), r"aba|\.\."),
            (regex(r"(?m)^ab|cd$"), r"(?m)^ab|cd$"),
            (regex(r"\b"), r"\b"),
            (regex(r"aba|$"), r"aba|$"),
        ] {
            let expected: Vec<_> = regex::bytes::Regex::new(re)
                .unwrap()
                .find_iter(&bytes)
                .map(|m| Match { offset: m.start() as u64, len: m.len() as u64 })
                .collect();
            assert_eq!(collect(&h, &p, 0..h.len()), expected, "size={size}, re={re}");
        }
    }
    assert_eq!(collect(&b""[..], &regex("^$"), 0..0), [Match { offset: 0, len: 0 }]);
}

#[test]
fn case_word_selection_and_invalid_bytes() {
    let text = "cat CAT scatter cat_ 猫cat cat猫 cat écat caté K k".as_bytes();
    let p = Pattern::literal(b"cat", Options { case_insensitive: true, whole_word: true }).unwrap();
    let hits = collect(text, &p, 0..text.len() as u64);
    assert_eq!(hits.iter().map(|m| m.offset).collect::<Vec<_>>(), [0, 4, 35]);
    // Selection boundaries inside a word must not turn its suffix into a word.
    assert!(collect(&b"scatX"[..], &p, 1..4).is_empty());
    assert!(collect(&b"scatX"[..], &regex("^cat$"), 1..4).is_empty());
    let k = Pattern::literal(b"k", Options { case_insensitive: true, whole_word: true }).unwrap();
    assert_eq!(collect("K k".as_bytes(), &k, 0..5).len(), 2);
    let raw = Chunks { bytes: b"\xffcat cat\xfe", size: 1 };
    assert_eq!(collect(&raw, &p, 0..raw.len()).len(), 2);
}

#[test]
fn count_progress_and_cancellation_stop_early() {
    let bytes = b"x ".repeat(250);
    let p = literal(b"x");
    let mut progress = Vec::new();
    let count = search(
        bytes.as_slice(),
        &p,
        0..500,
        SearchMode::Count,
        &token(),
        |_| panic!("count emitted hit"),
        |v| progress.push(v),
    )
    .unwrap();
    assert_eq!(count.count, 250);
    assert_eq!(count.scanned, 500);
    assert_eq!(progress.last().unwrap().count, 250);
    assert!(progress.windows(2).all(|w| w[0].scanned <= w[1].scanned));
    let cancel = token();
    let mut emitted = 0;
    let stopped = search(
        bytes.as_slice(),
        &p,
        0..500,
        SearchMode::Hits,
        &cancel,
        |_| {
            emitted += 1;
            if emitted == 3 {
                cancel.store(true, Ordering::Relaxed);
            }
        },
        |_| {},
    )
    .unwrap();
    assert_eq!(emitted, 3);
    assert_eq!(stopped.count, 3);
    assert!(stopped.cancelled);
    assert!(stopped.scanned < 500);
    let mut progress_count = 0;
    let no_hits = search(
        bytes.as_slice(),
        &literal(b"missing"),
        0..500,
        SearchMode::Count,
        &token(),
        |_| {},
        |_| progress_count += 1,
    )
    .unwrap();
    assert_eq!(no_hits.count, 0);
    assert!(progress_count > 1);
}

#[test]
fn capture_replace_plans_and_literal_dollars() {
    let text = b"start id=12 id=345 end";
    let h = Chunks { bytes: text, size: 1 };
    let p = regex(r"id=(?P<number>\d{1,3})");
    let hits = collect(&h, &p, 0..h.len());
    let mut edits = Vec::new();
    plan_replace(&h, &p, hits.clone(), b"${number}:$1:$$:$missing", &token(), |e| {
        edits.push(e);
        Ok(())
    })
    .unwrap();
    assert_eq!(
        edits,
        [
            Edit { offset: 6, len: 5, new_bytes: b"12:12:$:".to_vec() },
            Edit { offset: 12, len: 6, new_bytes: b"345:345:$:".to_vec() }
        ]
    );
    let re = regex::bytes::Regex::new(r"id=(?P<number>\d{1,3})").unwrap();
    for template in [b"$1x ${number} $0".as_slice(), b"${missing} $$ ${1} $", b"${} ${no-end"] {
        let mut got = Vec::new();
        plan_replace(&h, &p, [hits[0]], template, &token(), |e| {
            got = e.new_bytes;
            Ok(())
        })
        .unwrap();
        let mut expected = Vec::new();
        re.captures(text).unwrap().expand(template, &mut expected);
        assert_eq!(got, expected);
    }
    let ci = Pattern::literal(b"ID", Options { case_insensitive: true, whole_word: false }).unwrap();
    let mut got = Vec::new();
    plan_replace(&h, &ci, [Match { offset: 6, len: 2 }], b"$1", &token(), |e| {
        got = e.new_bytes;
        Ok(())
    })
    .unwrap();
    assert_eq!(got, b"$1");
    assert!(plan_replace(&h, &p, [hits[1], hits[0]], b"x", &token(), |_| Ok(())).is_err());
    assert!(plan_replace(&h, &literal(b"no"), [Match { offset: 0, len: 2 }], b"x", &token(), |_| Ok(())).is_err());
}

#[test]
fn filter_boolean_runs_and_incremental_partial_line() {
    let p = vec![regex("ERROR|WARN"), literal(b"user"), literal(b"ignore")];
    let expr = FilterExpr::And(vec![
        FilterExpr::Or(vec![FilterExpr::Pattern(0), FilterExpr::Pattern(1)]),
        FilterExpr::Not(Box::new(FilterExpr::Pattern(2))),
    ]);
    let filter = LineFilter::new(p, expr).unwrap();
    let bytes = b"ERROR x\r\nWARN user\nERROR ignore\nplain\nuser partial";
    let mut runs = LineRuns::new(4);
    let h = Chunks { bytes, size: 2 };
    let first = filter.scan(&h, FilterResume::default(), false, &token(), |r| runs.push(r)).unwrap();
    assert_eq!(runs.runs(), [LineRun { start: 1, end: 3 }]);
    assert_eq!(first.matching_lines, 2);
    assert_eq!(first.resume, FilterResume { offset: 38, line: 5 });
    let mut appended = bytes.to_vec();
    appended.extend_from_slice(b" done\nERROR end\n");
    let second = filter.scan(appended.as_slice(), first.resume, false, &token(), |r| runs.push(r)).unwrap();
    assert_eq!(second.matching_lines, 2);
    assert_eq!(runs.runs(), [LineRun { start: 1, end: 3 }, LineRun { start: 5, end: 7 }]);
    assert_eq!(second.resume.offset, appended.len() as u64);
    let mut static_runs = LineRuns::new(5);
    let provisional = filter.scan(bytes.as_slice(), FilterResume::default(), true, &token(), |_| Ok(())).unwrap();
    assert_eq!(provisional.resume, first.resume);
    assert_eq!(
        filter
            .scan(bytes.as_slice(), FilterResume::default(), true, &token(), |r| static_runs.push(r))
            .unwrap()
            .matching_lines,
        3
    );
    let cancel = token();
    cancel.store(true, Ordering::Relaxed);
    assert!(filter.scan(&h, FilterResume::default(), false, &cancel, |_| panic!()).unwrap().cancelled);
}

#[test]
fn filter_long_lines_are_bounded_and_runs_enforce_capacity() {
    let bytes = [vec![b'.'; 512], b"needle\nno\nneedle\n".to_vec()].concat();
    let filter = LineFilter::new(vec![regex("needle$")], FilterExpr::Pattern(0)).unwrap();
    let mut runs = LineRuns::new(1);
    assert!(filter.scan(bytes.as_slice(), FilterResume::default(), true, &token(), |r| runs.push(r)).is_err());
    assert_eq!(runs.runs(), [LineRun { start: 1, end: 2 }]);
    let mut streamed = Vec::new();
    let summary = filter
        .scan(bytes.as_slice(), FilterResume::default(), true, &token(), |r| {
            streamed.push(r);
            Ok(())
        })
        .unwrap();
    assert_eq!(summary.matching_lines, 2);
    assert_eq!(streamed, [LineRun { start: 1, end: 2 }, LineRun { start: 3, end: 4 }]);
}

struct TempTree(PathBuf);
impl TempTree {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("kwil-search-{}-{stamp}-{id}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        Self(root)
    }
    fn write(&self, path: &str, bytes: &[u8]) {
        let p = self.0.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }
}
impl Drop for TempTree {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

fn files(options: FileSearch) -> (FileSearchSummary, Vec<FileEvent>) {
    let (tx, rx) = sync_channel(2);
    std::thread::scope(|scope| {
        let task = scope.spawn(|| find_in_files(&options, &token(), tx).unwrap());
        let events = rx.iter().collect();
        (task.join().unwrap(), events)
    })
}

#[test]
fn files_gitignore_globs_previews_ranges_and_mapped_adapter() {
    let tree = TempTree::new();
    tree.write(".gitignore", b"ignored.txt\nignored-dir/\n");
    tree.write("a.txt", b"none\nNeedle needle tail\n");
    tree.write("nested/b.txt", b"needle\n");
    tree.write("skip.txt", b"needle\n");
    tree.write("ignored.txt", b"needle\n");
    tree.write("ignored-dir/a.txt", b"needle\n");
    tree.write("a.log", b"needle\n");
    let mut options = FileSearch::new(tree.0.clone(), "needle".into());
    options.options.case_insensitive = true;
    options.include = vec!["**/*.txt".into()];
    options.exclude = vec!["skip.txt".into()];
    options.max_line_bytes = 8;
    let (summary, events) = files(options);
    assert_eq!(summary.matching_lines, 2);
    assert_eq!(summary.errors, 0);
    assert_eq!(summary.files, 2);
    assert!(summary.workers <= 4);
    let mut hits: Vec<_> = events
        .into_iter()
        .map(|e| match e {
            FileEvent::Hit(h) => h,
            FileEvent::Error { message, .. } => panic!("{message}"),
        })
        .collect();
    hits.sort_by(|a, b| a.path.cmp(&b.path));
    assert_eq!(hits[0].line_number, 2);
    assert_eq!(hits[0].line_offset, 5);
    assert_eq!(hits[0].line, b"Needle n");
    assert_eq!(hits[0].match_ranges, [0..6, 7..13]);
    assert!(hits[0].line_truncated);
    let mapped = kwil_core_text::TextFile::open(&tree.0.join("a.txt")).unwrap();
    assert_eq!(collect(&mapped, &literal(b"needle"), 0..mapped.len()), [Match { offset: 12, len: 6 }]);
}

#[test]
fn files_cancellation_backpressure_and_disconnect() {
    let tree = TempTree::new();
    tree.write("a.txt", &b"needle\n".repeat(500));
    let options = FileSearch::new(tree.0.clone(), "needle".into());
    let (tx, rx) = sync_channel(1);
    let cancel = token();
    std::thread::scope(|scope| {
        let task = scope.spawn(|| find_in_files(&options, &cancel, tx).unwrap());
        assert!(matches!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), FileEvent::Hit(_)));
        cancel.store(true, Ordering::Relaxed);
        let result = task.join().unwrap();
        assert!(result.cancelled);
        assert!(result.matching_lines < 500);
    });
    let (tx, rx) = sync_channel(1);
    drop(rx);
    let result = find_in_files(&options, &token(), tx).unwrap();
    assert!(result.cancelled);
    assert_eq!(result.matching_lines, 0);
}

#[test]
fn files_oversize_lines_report_error_and_range_caps() {
    let tree = TempTree::new();
    tree.write("long.txt", &vec![b'x'; 70_000]);
    tree.write("hits.txt", b"x x x x\n");
    let mut options = FileSearch::new(tree.0.clone(), "x".into());
    options.heap_limit = 1 << 16;
    options.max_match_ranges = 2;
    let (summary, events) = files(options);
    assert_eq!(summary.errors, 1);
    assert_eq!(summary.matching_lines, 1);
    let h = events
        .into_iter()
        .find_map(|e| match e {
            FileEvent::Hit(h) => Some(h),
            _ => None,
        })
        .unwrap();
    assert!(h.ranges_truncated);
    assert_eq!(h.match_ranges, [0..1, 2..3]);
}

#[test]
fn invalid_ranges_chunks_and_match_bounds_are_errors() {
    let p = Pattern::regex("x{4}", Options::default(), 2).unwrap();
    assert!(search(&b"xxxx"[..], &p, 0..4, SearchMode::Count, &token(), |_| {}, |_| {}).is_err());
    let p = literal(b"x");
    assert!(search(&b"x"[..], &p, 0..2, SearchMode::Count, &token(), |_| {}, |_| {}).is_err());
    struct Broken;
    impl Haystack for Broken {
        fn len(&self) -> u64 {
            10
        }
        fn chunks(&self, _: Range<u64>) -> impl Iterator<Item = (u64, &[u8])> {
            std::iter::once((1, b"x".as_slice()))
        }
    }
    assert!(search(&Broken, &p, 0..10, SearchMode::Count, &token(), |_| {}, |_| {}).is_err());
    let result: io::Result<_> = LineFilter::new(vec![p], FilterExpr::Pattern(9));
    assert!(result.is_err());
}

#[test]
fn parallel_default_matches_serial_and_preserves_order() {
    let bytes = b"aa aaaa INFO ERROR line=123\n".repeat(100);
    let h = Chunks { bytes: &bytes, size: 13 };
    for p in [literal(b"aa"), regex("a{1,3}"), regex(r"\b"), regex(r"(?m)^|$"), regex("INFO|ERROR"), regex("aa|$")] {
        let expected = collect(&h, &p, 0..h.len());
        let mut actual = Vec::new();
        let s = search_default(&h, &p, 0..h.len(), SearchMode::Hits, &token(), |m| actual.push(m), |_| {}).unwrap();
        assert_eq!(actual, expected);
        assert_eq!(s.count as usize, expected.len());
        assert_eq!(s.workers, worker_count());
        let c = search_with_threads(&h, &p, 0..h.len(), SearchMode::Count, 4, &token(), |_| panic!(), |_| {}).unwrap();
        assert_eq!(c.count, s.count);
        assert_eq!(c.scanned, h.len());
    }
    let one_line = b"a".repeat(2000);
    // Multiline patterns and a giant line retain exact serial semantics.
    for (bytes, p) in [(bytes.as_slice(), literal(b"123\naa")), (one_line.as_slice(), literal(b"aa"))] {
        let s = search_default(bytes, &p, 0..bytes.len() as u64, SearchMode::Count, &token(), |_| {}, |_| {}).unwrap();
        assert_eq!(s.workers, 1);
        assert_eq!(s.count as usize, collect(bytes, &p, 0..bytes.len() as u64).len());
    }
    let cancel = token();
    let mut hits = 0;
    let stopped = search_default(
        &h,
        &literal(b"aa"),
        0..h.len(),
        SearchMode::Hits,
        &cancel,
        |_| {
            hits += 1;
            if hits == 3 {
                cancel.store(true, Ordering::Relaxed);
            }
        },
        |_| {},
    )
    .unwrap();
    assert!(stopped.cancelled);
    assert_eq!(hits, 3);
}
