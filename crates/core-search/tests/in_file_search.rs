use kwil_core_search::{Cancellation, Haystack, Match, Options, Pattern, SearchMode, search_default};
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const MIB: u64 = 1 << 20;

// Generate sparse, newline-free fixtures on demand, never a file-sized Vec.
// Large fixtures belong in the ignored fixtures directory, including in CI.
struct Fixture(PathBuf);
impl Fixture {
    fn new(len: u64, offset: u64, bytes: &[u8]) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
        std::fs::create_dir_all(&root).unwrap();
        let path =
            root.join(format!("in-file-search-{}-{}.bin", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let mut file = OpenOptions::new().write(true).create_new(true).open(&path).unwrap();
        file.set_len(len).unwrap();
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.write_all(bytes).unwrap();
        Self(path)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).unwrap();
    }
}

fn cancel() -> Cancellation {
    Arc::new(AtomicBool::new(false))
}

fn assert_hit<H: Haystack + Sync>(source: &H, pattern: &Pattern, offset: u64, len: u64) {
    let mut hits = Vec::new();
    let summary =
        search_default(source, pattern, 0..source.len(), SearchMode::Hits, &cancel(), |m| hits.push(m), |_| {})
            .unwrap();
    assert_eq!(hits, [Match { offset, len }]);
    assert_eq!(summary.count, 1);
    assert_eq!(summary.scanned, source.len());
}

#[test]
fn finite_regex_crosses_chunk_boundary_with_derived_overlap() {
    let offset = MIB - 32;
    let fixture = Fixture::new(2 * MIB, offset, &[b'x'; 256]);
    let source = kwil_core_text::TextFile::open(&fixture.0).unwrap();
    let pattern = Pattern::regex("x{256}", Options::default(), 16).unwrap();
    assert_hit(&source, &pattern, offset, 256);
}

#[test]
fn finite_regex_searches_inside_a_51_mib_line() {
    let offset = 50 * MIB - 32;
    let fixture = Fixture::new(51 * MIB, offset, &[b'x'; 256]);
    let source = kwil_core_text::TextFile::open(&fixture.0).unwrap();
    let pattern = Pattern::regex("x{256}", Options::default(), 16).unwrap();
    assert_hit(&source, &pattern, offset, 256);
}

#[test]
fn finite_regex_searches_large_file_tail_and_eof_anchor() {
    let len = 65 * MIB + 73;
    let fixture = Fixture::new(len, len - 256, &[b'x'; 256]);
    let source = kwil_core_text::TextFile::open(&fixture.0).unwrap();
    let pattern = Pattern::regex(r"x{256}\z", Options::default(), 16).unwrap();
    assert_hit(&source, &pattern, len - 256, 256);
}

#[test]
fn unbounded_regex_reports_input_cap_before_emitting_partial_results() {
    let fixture =
        Fixture::new(2 * MIB, MIB - 32, b"startxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxend");
    let source = kwil_core_text::TextFile::open(&fixture.0).unwrap();
    let pattern = Pattern::regex("start.*end|missing", Options::default(), 32).unwrap();
    let mut hits = 0;
    let error = search_default(&source, &pattern, 0..source.len(), SearchMode::Hits, &cancel(), |_| hits += 1, |_| {})
        .unwrap_err();
    assert_eq!(hits, 0);
    assert!(error.to_string().contains("unbounded regex"), "{error}");
    assert!(error.to_string().contains("32 bytes"), "{error}");
    assert!(error.to_string().contains("bounded repetition"), "{error}");
}

#[test]
fn unbounded_regex_preserves_greedy_lazy_alternative_and_selection_semantics_under_cap() {
    let bytes = b"start x end end";
    for expression in ["start.*end", "start.*?end", "start.*end|x"] {
        let pattern = Pattern::regex(expression, Options::default(), 32).unwrap();
        let expected: Vec<_> = regex::bytes::Regex::new(expression)
            .unwrap()
            .find_iter(bytes)
            .map(|m| Match { offset: m.start() as u64, len: m.len() as u64 })
            .collect();
        let mut hits = Vec::new();
        kwil_core_search::search(
            bytes.as_slice(),
            &pattern,
            0..bytes.len() as u64,
            SearchMode::Hits,
            &cancel(),
            |m| hits.push(m),
            |_| {},
        )
        .unwrap();
        assert_eq!(hits, expected);
    }
    // A larger file with a small selection still needs global anchor/greediness context.
    let pattern = Pattern::regex("start.*end", Options::default(), 8).unwrap();
    assert!(
        kwil_core_search::search(bytes.as_slice(), &pattern, 0..5, SearchMode::Count, &cancel(), |_| {}, |_| {})
            .is_err()
    );
}

#[test]
fn finite_regex_over_one_mib_is_rejected_with_actionable_limit() {
    let error = Pattern::regex(r"(?-u:.){1048577}", Options::default(), 32).unwrap_err();
    assert!(error.to_string().contains("1 MiB"), "{error}");
}
