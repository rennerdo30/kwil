//! SOURCE ONLY acceptance candidates; NOT RUN by the author.
use super::*;
use crate::{Buffer, TextFile};
fn snapshot(bytes: &[u8]) -> Snapshot {
    let mut buffer = Buffer::empty();
    buffer.insert(0, bytes).unwrap();
    buffer.snapshot()
}
fn one<T>(mut poll: impl FnMut(&mut ReadWork, &AtomicBool) -> Step<T>) -> T {
    for _ in 0..500_000 {
        match poll(&mut ReadWork::new(1, 1), &AtomicBool::new(false)) {
            Step::Ready(value) => return value,
            Step::Pending => {}
            Step::Cancelled => panic!("unexpected cancellation"),
            Step::Failed(error) => panic!("{error}"),
        }
    }
    panic!("query lost its continuation under one-byte quanta")
}
#[test]
fn fragmented_tree_line_rank_and_complete_terminators_are_exact() {
    let mut buffer = Buffer::empty();
    for byte in b"A\r\nB\n\nC\r".iter().rev() {
        buffer.insert(0, &[*byte]).unwrap();
    }
    let snapshot = buffer.snapshot();
    let expected = [
        LineSpan { start: 0, content_end: 1, end: 3, terminator: Terminator::CrLf },
        LineSpan { start: 3, content_end: 4, end: 5, terminator: Terminator::Lf },
        LineSpan { start: 5, content_end: 5, end: 6, terminator: Terminator::Lf },
        LineSpan { start: 6, content_end: 8, end: 8, terminator: Terminator::None },
    ];
    for (line, expected) in expected.into_iter().enumerate() {
        let mut query = LayoutLineQuery::new(line as u64);
        assert_eq!(one(|work, cancel| query.poll(&snapshot, work, cancel)), expected);
    }
    for at in 0..=snapshot.len() {
        let mut query = LayoutRankQuery::new(at);
        let actual = one(|work, cancel| query.poll(&snapshot, work, cancel));
        let (line, column) = snapshot.offset_to_line_col(at).unwrap().unwrap();
        assert_eq!(actual, Rank { line, column });
    }
}
#[test]
fn pending_cursor_read_is_atomic_and_short_prefix_advances() {
    let mut buffer = Buffer::empty();
    buffer.insert(0, b"cd").unwrap();
    buffer.insert(0, b"ab").unwrap();
    let snapshot = buffer.snapshot();
    let root = snapshot.root.as_ref().unwrap().clone();
    let mut query = LayoutReadQuery::new(0, 4);
    let mut logical = [77; 4];
    let mut raw = [55; 16];
    assert!(matches!(
        query.poll(&snapshot, &mut logical, &mut raw, &mut ReadWork::new(0, 0), &AtomicBool::new(false)),
        Step::Pending
    ));
    assert_eq!(logical, [77; 4]);
    assert_eq!(raw, [55; 16]);
    let chunk = one(|work, cancel| query.poll(&snapshot, &mut logical, &mut raw, work, cancel)).unwrap();
    assert!((1..=4).contains(&chunk.logical_len));
    assert_eq!(&logical[..chunk.logical_len], &b"abcd"[..chunk.logical_len]);
    drop(query);
    assert!(Arc::ptr_eq(snapshot.root.as_ref().unwrap(), &root));
}
#[test]
fn cancellation_query_change_and_invalid_read_keep_output_untouched() {
    let first = snapshot(b"one\ntwo");
    let second = snapshot(b"other");
    let mut query = LayoutRankQuery::new(5);
    assert!(matches!(query.poll(&first, &mut ReadWork::new(1, 1), &AtomicBool::new(false)), Step::Pending));
    assert!(matches!(
        query.poll(&second, &mut ReadWork::new(100, 100), &AtomicBool::new(false)),
        Step::Failed(InputError::QueryChanged)
    ));
    let mut read = LayoutReadQuery::new(0, 3);
    let mut output = [77; 3];
    let mut raw = [55; 8];
    assert!(matches!(
        read.poll(&first, &mut output, &mut raw, &mut ReadWork::new(100, 100), &AtomicBool::new(true)),
        Step::Cancelled
    ));
    assert_eq!(output, [77; 3]);
    assert_eq!(raw, [55; 8]);
    let mut invalid = LayoutReadQuery::new(100, 3);
    assert!(matches!(
        invalid.poll(&first, &mut output, &mut raw, &mut ReadWork::new(100, 100), &AtomicBool::new(false)),
        Step::Failed(InputError::Range)
    ));
    assert_eq!(output, [77; 3]);
    assert_eq!(raw, [55; 8]);
}

struct Directory(std::path::PathBuf);
impl Directory {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "kwil-worker-layout-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.0);
    }
}
#[test]
fn existing_byte_rank_checkpoints_bound_giant_line_seeks() {
    let directory = Directory::new();
    let path = directory.0.join("input");
    let mut bytes = vec![b'x'; 2 * super::super::ADD_BLOCK + 7];
    bytes.extend_from_slice(b"\r\nZ\r");
    std::fs::write(&path, &bytes).unwrap();
    let file = TextFile::open(&path).unwrap();
    file.build_index(1).unwrap();
    let snapshot = Buffer::from_file(file).unwrap().snapshot();
    let mut query = LayoutLineQuery::new(1);
    let actual = one(|work, cancel| query.poll(&snapshot, work, cancel));
    assert_eq!(
        actual,
        LineSpan {
            start: bytes.len() as u64 - 2,
            content_end: bytes.len() as u64,
            end: bytes.len() as u64,
            terminator: Terminator::None
        }
    );
    let mut rank = LayoutRankQuery::new(bytes.len() as u64 - 4);
    assert_eq!(
        one(|work, cancel| rank.poll(&snapshot, work, cancel)),
        Rank { line: 0, column: bytes.len() as u64 - 4 }
    );
    drop(snapshot);
    std::fs::remove_file(path).unwrap();
}
#[test]
fn changed_original_is_reported_without_publishing_bytes() {
    let directory = Directory::new();
    let path = directory.0.join("input");
    std::fs::write(&path, b"before\n").unwrap();
    let file = TextFile::open(&path).unwrap();
    file.build_index(1).unwrap();
    let snapshot = Buffer::from_file(file).unwrap().snapshot();
    std::fs::write(&path, b"after\na lot more\n").unwrap();
    let mut read = LayoutReadQuery::new(0, 1);
    let mut output = [77; 1];
    let mut raw = [55; 8];
    let mut changed = false;
    for _ in 0..1000 {
        match read.poll(&snapshot, &mut output, &mut raw, &mut ReadWork::new(1, 1), &AtomicBool::new(false)) {
            Step::Pending => {}
            Step::Failed(InputError::SourceChanged) => {
                changed = true;
                break;
            }
            _ => panic!("source mutation must remain typed"),
        }
        assert_eq!(output, [77; 1]);
        assert_eq!(raw, [55; 8]);
    }
    assert!(changed);
    assert_eq!(output, [77; 1]);
    assert_eq!(raw, [55; 8]);
    drop(read);
    drop(snapshot);
    std::fs::remove_file(path).unwrap();
}
#[test]
fn detached_and_decoded_opaque_provenance_resume_without_style_fabrication() {
    let encoding = crate::encoding::Encoding::for_label("big5").unwrap();
    let budget = super::super::AddBudget::new(1024, Arc::new(()));
    let mut builder = super::super::ImportBuilder::new(encoding, &budget);
    builder.text(b"a").unwrap();
    builder.preserved("Ê\u{304}".as_bytes(), b"\x88\x62").unwrap();
    builder.opaque(b"\xff").unwrap();
    builder.text(b"Z").unwrap();
    let snapshot = Buffer::recovery_snapshot(builder.finish().unwrap());
    for (at, range, opaque) in [(1, 1..5, false), (3, 1..5, false), (5, 5..6, true), (6, 6..7, false)] {
        let mut query = LayoutProvenanceQuery::new(at);
        assert_eq!(one(|work, cancel| query.poll(&snapshot, work, cancel)), LayoutProvenance { source: range, opaque });
    }
    let directory = Directory::new();
    let mut raw = &b"a\x88\x62\xffZ"[..];
    let backing = crate::decoded_backing::DecodedBacking::prepare_from_reader(
        &mut raw,
        5,
        crate::decoded_backing::Interpretation::new(encoding, 0).unwrap(),
        &directory.0,
        &AtomicBool::new(false),
    )
    .unwrap();
    let buffer = Buffer::from_decoded(backing.snapshot());
    let snapshot = buffer.snapshot();
    for (at, range, opaque) in [(1, 1..5, false), (3, 1..5, false), (5, 5..6, true)] {
        let mut query = LayoutProvenanceQuery::new(at);
        assert_eq!(one(|work, cancel| query.poll(&snapshot, work, cancel)), LayoutProvenance { source: range, opaque });
    }
    drop(snapshot);
    drop(buffer);
    assert!(backing.try_retire().is_ok());
}
