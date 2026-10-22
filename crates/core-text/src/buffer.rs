//! Byte-exact editing over immutable, shared pieces. Offsets, columns and
//! line numbers are zero-based; columns count bytes, not Unicode scalars.

use crate::piece_tree::{self, AddBudget, Chunks, Tree};
use crate::undo::History;
use crate::{ReadError, TextFile};
use std::io::{self, Write};
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

const ADD_MEMORY: usize = 8 << 20;
const UNDO_MEMORY: usize = 16 << 20;
const TYPING_GAP: Duration = Duration::from_millis(500);

/// O(1) immutable version, safe to send to search/highlight/save workers.
/// Holding a snapshot pins its referenced storage until the worker drops it.
/// Original-file reads follow TextFile's change checks and may return ReadError.
#[derive(Clone, Default)]
pub struct Snapshot {
    pub(crate) root: Tree,
}

impl Snapshot {
    pub fn len(&self) -> u64 {
        self.root.as_ref().map_or(0, |node| node.metrics.bytes)
    }

    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    /// Newline count plus one, including an empty final line.
    pub fn line_count(&self) -> u64 {
        self.root.as_ref().map_or(1, |node| node.metrics.breaks + 1)
    }

    pub fn line_start(&self, line: u64) -> Result<Option<u64>, ReadError> {
        if line >= self.line_count() {
            Ok(None)
        } else if line == 0 {
            Ok(Some(0))
        } else {
            Ok(Some(piece_tree::after_break(self.root.as_ref().unwrap(), line)?))
        }
    }

    /// EOF is a valid position. A CR in CRLF occupies one byte column.
    pub fn offset_to_line_col(&self, offset: u64) -> Result<Option<(u64, u64)>, ReadError> {
        if offset > self.len() {
            return Ok(None);
        }
        let line = match &self.root {
            Some(node) => piece_tree::rank(node, offset)?,
            None => 0,
        };
        Ok(Some((line, offset - self.line_start(line)?.unwrap())))
    }

    /// Borrowed slices clipped to `range`; invalid/reversed ranges are errors.
    /// Empty ranges yield no chunks. No file-sized output allocation occurs.
    /// Each iterator item reports original-file read errors and then stops.
    pub fn chunks(&self, range: Range<u64>) -> io::Result<Chunks<'_>> {
        self.check_range(&range)?;
        Ok(Chunks::new(&self.root, range))
    }

    /// Borrow each viewport line as chunks, without LF or a trailing CR.
    /// Even a single giant line is never joined into an owned buffer.
    /// Both line and chunk iterator items can report ReadError.
    pub fn lines(&self, start_line: u64, count: usize) -> Lines<'_> {
        Lines { snapshot: self, next: start_line, remaining: count }
    }

    /// Stream this version to a writer. Atomic replacement is the save layer's job.
    pub fn write_to(&self, writer: &mut impl Write) -> io::Result<()> {
        for chunk in Chunks::new(&self.root, 0..self.len()) {
            writer.write_all(chunk.map_err(io::Error::other)?)?;
        }
        Ok(())
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        self.root.as_ref().map_or(0, |node| node.metrics.retained)
    }

    fn check_range(&self, range: &Range<u64>) -> io::Result<()> {
        if range.start > range.end || range.end > self.len() {
            Err(io::Error::new(io::ErrorKind::InvalidInput, "byte range outside buffer"))
        } else {
            Ok(())
        }
    }

    fn byte_at(&self, offset: u64) -> Result<u8, ReadError> {
        Ok(Chunks::new(&self.root, offset..offset + 1).next().unwrap()?[0])
    }
}

/// An iterator of line iterators; allocating scratch space is O(tree height).
pub struct Lines<'a> {
    snapshot: &'a Snapshot,
    next: u64,
    remaining: usize,
}

impl<'a> Lines<'a> {
    fn next_line(&mut self) -> Result<Option<Chunks<'a>>, ReadError> {
        let Some(start) = self.snapshot.line_start(self.next)? else { return Ok(None) };
        self.next += 1;
        self.remaining -= 1;
        let mut end = self.snapshot.line_start(self.next)?.map_or(self.snapshot.len(), |at| at - 1);
        if end > start && self.snapshot.byte_at(end - 1)? == b'\r' {
            end -= 1;
        }
        Ok(Some(Chunks::new(&self.snapshot.root, start..end)))
    }
}

impl<'a> Iterator for Lines<'a> {
    type Item = Result<Chunks<'a>, ReadError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        match self.next_line() {
            Ok(Some(line)) => Some(Ok(line)),
            Ok(None) => {
                self.remaining = 0;
                None
            }
            Err(error) => {
                self.remaining = 0;
                Some(Err(error))
            }
        }
    }
}

struct Typing {
    next_offset: u64,
    at: Duration,
}

/// Mutable editor handle with a persistent AVL tree and capped linear undo.
/// Each edit path-copies O(log pieces) nodes; inserted bytes cost O(input size).
/// Resident add bytes default to 8 MiB, shared with snapshots and undo; excess
/// blocks spill to private immutable mapped temp files. Disk spill may block:
/// bulk insertion belongs on a worker. Small typing edits stay in memory.
pub struct Buffer {
    snapshot: Snapshot,
    added: Arc<AddBudget>,
    history: History,
    transaction: Option<Snapshot>,
    typing: Option<Typing>,
}

impl Default for Buffer {
    fn default() -> Self {
        Self::empty()
    }
}

impl Buffer {
    pub fn empty() -> Self {
        Self::with_add_memory_limit(ADD_MEMORY)
    }

    /// Choose the resident add-byte cap before any edits (zero always spills).
    pub fn with_add_memory_limit(bytes: usize) -> Self {
        Self {
            snapshot: Snapshot::default(),
            added: AddBudget::new(bytes),
            history: History::new(UNDO_MEMORY),
            transaction: None,
            typing: None,
        }
    }

    /// Does not copy file bytes. O(1) when the file's sparse index is ready.
    /// Otherwise builds that index on the calling thread with four workers;
    /// call on a worker, or await TextFile's background index before attaching.
    /// Inherits TextFile's external-file-mutation safety contract.
    /// Refuses changed files even when an index is already cached.
    pub fn from_file(file: TextFile) -> Result<Self, ReadError> {
        Ok(Self { snapshot: Snapshot { root: piece_tree::original(file)? }, ..Self::empty() })
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.clone()
    }

    pub fn len(&self) -> u64 {
        self.snapshot.len()
    }

    pub fn is_empty(&self) -> bool {
        self.snapshot.is_empty()
    }

    pub fn line_count(&self) -> u64 {
        self.snapshot.line_count()
    }

    pub fn line_start(&self, line: u64) -> Result<Option<u64>, ReadError> {
        self.snapshot.line_start(line)
    }

    pub fn offset_to_line_col(&self, offset: u64) -> Result<Option<(u64, u64)>, ReadError> {
        self.snapshot.offset_to_line_col(offset)
    }

    pub fn chunks(&self, range: Range<u64>) -> io::Result<Chunks<'_>> {
        self.snapshot.chunks(range)
    }

    pub fn lines(&self, start_line: u64, count: usize) -> Lines<'_> {
        self.snapshot.lines(start_line, count)
    }

    pub fn write_to(&self, writer: &mut impl Write) -> io::Result<()> {
        self.snapshot.write_to(writer)
    }

    pub fn insert(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        self.typing = None;
        self.insert_inner(offset, bytes, false)
    }

    fn insert_inner(&mut self, offset: u64, bytes: &[u8], coalesce: bool) -> io::Result<()> {
        self.snapshot.check_range(&(offset..offset))?;
        if bytes.is_empty() {
            return Ok(());
        }
        self.len()
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "buffer length overflow"))?;
        // Allocate all new storage before mutating the root: I/O errors are atomic.
        let added = piece_tree::added(bytes, &self.added)?;
        let before = self.snapshot();
        let (left, right) = piece_tree::split(before.root.clone(), offset).map_err(io::Error::other)?;
        self.snapshot.root = piece_tree::join(piece_tree::join(left, added), right);
        self.record(before, coalesce);
        Ok(())
    }

    pub fn delete(&mut self, range: Range<u64>) -> io::Result<()> {
        self.snapshot.check_range(&range)?;
        self.typing = None;
        if range.is_empty() {
            return Ok(());
        }
        let before = self.snapshot();
        let (left, tail) = piece_tree::split(before.root.clone(), range.start).map_err(io::Error::other)?;
        let (_, right) = piece_tree::split(tail, range.end - range.start).map_err(io::Error::other)?;
        self.snapshot.root = piece_tree::join(left, right);
        self.record(before, false);
        Ok(())
    }

    fn record(&mut self, before: Snapshot, coalesce: bool) {
        if self.transaction.is_none() {
            self.history.record(before, self.snapshot(), coalesce);
        }
    }

    /// Begin one explicit batch. Returns false for a nested begin; nested
    /// transactions are not supported. A batch stays one undo step.
    pub fn begin(&mut self) -> bool {
        self.typing = None;
        if self.transaction.is_some() {
            return false;
        }
        self.transaction = Some(self.snapshot());
        true
    }

    /// Commit an open batch. Empty batches create no history entry.
    pub fn commit(&mut self) -> bool {
        self.typing = None;
        let Some(before) = self.transaction.take() else { return false };
        let unchanged = match (&before.root, &self.snapshot.root) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        };
        if !unchanged {
            self.history.record(before, self.snapshot(), false);
        }
        true
    }

    /// Coalesce contiguous insertions separated by at most 500 ms. The caller
    /// supplies monotonic elapsed time: no timer, polling or idle work is used.
    /// Cursor movement/commands should call `end_typing_burst`.
    pub fn insert_typing(&mut self, offset: u64, bytes: &[u8], at: Duration) -> io::Result<()> {
        let coalesce = self.transaction.is_none()
            && self.typing.as_ref().is_some_and(|previous| {
                previous.next_offset == offset && at.checked_sub(previous.at).is_some_and(|gap| gap <= TYPING_GAP)
            });
        self.insert_inner(offset, bytes, coalesce)?;
        if !bytes.is_empty() {
            self.typing = Some(Typing { next_offset: offset + bytes.len() as u64, at });
        }
        Ok(())
    }

    pub fn end_typing_burst(&mut self) {
        self.typing = None;
    }

    /// Undo commits any pending batch first.
    pub fn undo(&mut self) -> bool {
        self.commit();
        self.typing = None;
        if let Some(snapshot) = self.history.undo() {
            self.snapshot = snapshot;
            true
        } else {
            false
        }
    }

    pub fn redo(&mut self) -> bool {
        self.commit();
        self.typing = None;
        if let Some(snapshot) = self.history.redo() {
            self.snapshot = snapshot;
            true
        } else {
            false
        }
    }

    /// Conservative retained-root accounting; shared nodes/blocks may count
    /// more than once. Excludes baseline original storage and OS page cache.
    /// Drops oldest undo entries, then farthest-future redo entries, keeping
    /// a contiguous history anchored at the current state.
    /// Active transactions and externally held snapshots are not undo history.
    pub fn set_undo_memory_limit(&mut self, bytes: usize) {
        self.history.set_limit(bytes);
    }

    pub fn undo_memory_bytes(&self) -> usize {
        self.history.bytes()
    }

    /// Resident added bytes across this buffer, history and worker snapshots.
    pub fn add_memory_bytes(&self) -> usize {
        self.added.used.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use memchr::memchr_iter;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new(bytes: &[u8]) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "kwil-buffer-test-{}-{}.txt",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::OpenOptions::new().write(true).create_new(true).open(&path).unwrap().write_all(bytes).unwrap();
            Self(path)
        }

        fn open(&self) -> TextFile {
            TextFile::open(&self.0).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_file(&self.0).unwrap();
        }
    }

    fn bytes(snapshot: &Snapshot) -> Vec<u8> {
        let mut out = Vec::new();
        snapshot.write_to(&mut out).unwrap();
        out
    }

    fn check(buffer: &Buffer, model: &[u8], step: usize) {
        assert_eq!(bytes(&buffer.snapshot()), model, "bytes at step {step}");
        assert_eq!(buffer.len(), model.len() as u64, "length at step {step}");
        let starts: Vec<_> = std::iter::once(0).chain(memchr_iter(b'\n', model).map(|at| at as u64 + 1)).collect();
        assert_eq!(buffer.line_count(), starts.len() as u64, "line count at step {step}");
        for (line, &start) in starts.iter().enumerate() {
            assert_eq!(buffer.line_start(line as u64).unwrap(), Some(start), "line {line}, step {step}");
        }
        assert_eq!(buffer.line_start(starts.len() as u64).unwrap(), None);
        for offset in [0, model.len() / 2, model.len()] {
            let line = starts.partition_point(|&at| at <= offset as u64) - 1;
            assert_eq!(
                buffer.offset_to_line_col(offset as u64).unwrap(),
                Some((line as u64, offset as u64 - starts[line]))
            );
        }
        piece_tree::assert_valid(&buffer.snapshot.root);
    }

    fn random(seed: &mut u64) -> usize {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (*seed >> 32) as usize
    }

    /// Independent byte vectors and linear history; all lines checked after
    /// every operation. Seed is fixed so a failure is exactly reproducible.
    #[test]
    fn randomized_model_6000_operations() {
        let initial = b"original\r\n\xff\n".repeat(3100);
        let fixture = Fixture::new(&initial);
        let mut buffer = Buffer::from_file(fixture.open()).unwrap();
        buffer.set_undo_memory_limit(usize::MAX);
        let mut model = initial.clone();
        let mut history: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        let mut position = 0;
        let mut seed = 0x16_18_42;
        for step in 0..6000 {
            let operation = random(&mut seed) % 7;
            let before = model.clone();
            let mut changed = false;
            match operation {
                0..=2 => {
                    let at = random(&mut seed) % (model.len() + 1);
                    let n = 1 + random(&mut seed) % 23;
                    let text: Vec<u8> =
                        (0..n).map(|_| [b'a', b'\r', b'\n', 0, 0xff, b'z'][random(&mut seed) % 6]).collect();
                    buffer.insert(at as u64, &text).unwrap();
                    model.splice(at..at, text);
                    changed = true;
                }
                3 if !model.is_empty() => {
                    let a = random(&mut seed) % model.len();
                    let b = a + 1 + random(&mut seed) % (model.len() - a);
                    buffer.delete(a as u64..b as u64).unwrap();
                    model.drain(a..b);
                    changed = true;
                }
                4 => {
                    assert_eq!(buffer.undo(), position > 0, "undo at step {step}");
                    if position > 0 {
                        position -= 1;
                        model = history[position].0.clone();
                    }
                }
                5 => {
                    assert_eq!(buffer.redo(), position < history.len(), "redo at step {step}");
                    if position < history.len() {
                        model = history[position].1.clone();
                        position += 1;
                    }
                }
                6 => {
                    buffer.begin();
                    buffer.insert(0, b"\nreplace\r\n").unwrap();
                    model.splice(0..0, b"\nreplace\r\n".iter().copied());
                    let end = model.len().min(12);
                    buffer.delete(1..end as u64).unwrap();
                    model.drain(1..end);
                    buffer.insert(buffer.len(), b"\xfftail").unwrap();
                    model.extend_from_slice(b"\xfftail");
                    buffer.commit();
                    changed = true;
                }
                _ => {}
            }
            if changed {
                history.truncate(position);
                history.push((before, model.clone()));
                position += 1;
            }
            check(&buffer, &model, step);
        }
        while position > 0 {
            assert!(buffer.undo());
            position -= 1;
            check(&buffer, &history[position].0, 6000 + position);
        }
        assert_eq!(bytes(&buffer.snapshot()), initial);
    }

    #[test]
    fn snapshot_is_stable_send_sync_and_spill_is_bounded() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Snapshot>();
        let fixture = Fixture::new(b"original\r\nlast");
        let mut buffer = Buffer::from_file(fixture.open()).unwrap();
        let original = buffer.snapshot();
        buffer.delete(0..buffer.len()).unwrap();
        buffer.insert(0, b"changed").unwrap();
        assert_eq!(bytes(&original), b"original\r\nlast");

        let mut buffer = Buffer::with_add_memory_limit(3);
        buffer.set_undo_memory_limit(0);
        buffer.insert(0, b"abc").unwrap();
        let snapshot = buffer.snapshot();
        buffer.delete(0..3).unwrap();
        let large = b"x\r\n\xff".repeat(piece_tree::ADD_BLOCK / 2);
        buffer.insert(0, &large).unwrap();
        assert_eq!(buffer.add_memory_bytes(), 3); // pinned snapshot owns the quota
        assert_eq!(bytes(&buffer.snapshot()), large);
        assert_eq!(buffer.line_start(25_000).unwrap(), Some(99_999));
        let paths = spill_paths(&buffer.snapshot.root);
        assert!(!paths.is_empty());
        assert!(paths.iter().all(|path| path.exists()));
        buffer.delete(0..buffer.len()).unwrap();
        assert!(paths.iter().all(|path| !path.exists()));
        let old = std::thread::spawn(move || bytes(&snapshot)).join().unwrap();
        assert_eq!(old, b"abc");
        assert_eq!(buffer.add_memory_bytes(), 0);
    }

    // Keep storage internals private to the tree; tests ask only for temp paths.
    fn spill_paths(tree: &Tree) -> Vec<PathBuf> {
        piece_tree::spill_paths(tree)
    }

    #[test]
    fn transactions_typing_and_linear_redo() {
        let mut buffer = Buffer::empty();
        assert!(!buffer.undo());
        assert!(!buffer.redo());
        assert!(buffer.begin());
        assert!(!buffer.begin());
        assert!(buffer.commit());
        assert!(!buffer.undo());
        buffer.insert(0, b"one two one").unwrap();
        buffer.begin();
        buffer.delete(8..11).unwrap();
        buffer.insert(8, b"three").unwrap();
        buffer.delete(0..3).unwrap();
        buffer.insert(0, b"three").unwrap();
        buffer.commit();
        assert_eq!(bytes(&buffer.snapshot()), b"three two three");
        assert!(buffer.undo());
        assert_eq!(bytes(&buffer.snapshot()), b"one two one");
        assert!(buffer.redo());
        assert_eq!(bytes(&buffer.snapshot()), b"three two three");
        buffer.undo();
        buffer.insert_typing(11, b"a", Duration::from_millis(100)).unwrap();
        buffer.insert_typing(12, b"b", Duration::from_millis(500)).unwrap();
        buffer.insert_typing(13, b"c", Duration::from_millis(900)).unwrap();
        assert!(!buffer.redo());
        buffer.undo();
        assert_eq!(bytes(&buffer.snapshot()), b"one two one");
        buffer.redo();
        buffer.insert_typing(14, b"d", Duration::from_millis(2000)).unwrap();
        buffer.insert_typing(0, b"e", Duration::from_millis(2100)).unwrap();
        buffer.undo();
        assert_eq!(bytes(&buffer.snapshot()), b"one two oneabcd");
        buffer.undo();
        assert_eq!(bytes(&buffer.snapshot()), b"one two oneabc");
        buffer.begin();
        buffer.insert_typing(14, b"!", Duration::from_millis(2200)).unwrap();
        buffer.commit();
        buffer.insert_typing(15, b"?", Duration::from_millis(2300)).unwrap();
        buffer.undo();
        assert_eq!(bytes(&buffer.snapshot()), b"one two oneabc!");
        buffer.undo();
        assert_eq!(bytes(&buffer.snapshot()), b"one two oneabc");
    }

    #[test]
    fn cap_shrink_after_undo_all_preserves_redo_roundtrip() {
        let mut buffer = Buffer::empty();
        buffer.insert(0, b"a").unwrap();
        let first_cost = buffer.undo_memory_bytes();
        buffer.insert(1, b"b").unwrap();
        buffer.insert(2, b"c").unwrap();
        let cap = buffer.undo_memory_bytes() - first_cost;
        for _ in 0..3 {
            assert!(buffer.undo());
        }
        buffer.set_undo_memory_limit(cap);
        assert!(buffer.redo());
        assert!(buffer.undo());
        assert_eq!(bytes(&buffer.snapshot()), b"");
    }

    #[test]
    fn history_cap_evicts_oldest_and_releases_add_bytes() {
        let mut buffer = Buffer::empty();
        buffer.insert(0, b"a").unwrap();
        let first_cost = buffer.undo_memory_bytes();
        buffer.insert(1, b"b").unwrap();
        buffer.insert(2, b"c").unwrap();
        let cap = buffer.undo_memory_bytes() - first_cost;
        buffer.set_undo_memory_limit(cap);
        assert_eq!(buffer.undo_memory_bytes(), cap);
        assert!(buffer.undo());
        assert!(buffer.undo());
        assert!(!buffer.undo());
        assert_eq!(bytes(&buffer.snapshot()), b"a");
        assert!(buffer.redo());
        assert!(buffer.redo());
        buffer.delete(0..3).unwrap();
        buffer.set_undo_memory_limit(0);
        assert_eq!(buffer.undo_memory_bytes(), 0);
        assert_eq!(buffer.add_memory_bytes(), 0);
        assert!(!buffer.undo());
        assert!(!buffer.redo());
    }

    #[test]
    fn borrowed_ranges_lines_and_errors() {
        let mut buffer = Buffer::empty();
        buffer.insert(0, b"one\rtwo\r").unwrap();
        buffer.insert(buffer.len(), b"\n\nlast\r\n").unwrap();
        buffer.insert(2, b"\xff").unwrap();
        let lines: Vec<Vec<u8>> =
            buffer.lines(0, 10).map(|line| line.unwrap().flat_map(Result::unwrap).copied().collect()).collect();
        assert_eq!(lines, [b"on\xffe\rtwo".to_vec(), vec![], b"last".to_vec(), vec![]]);
        let middle: Vec<_> = buffer.chunks(3..12).unwrap().flat_map(Result::unwrap).copied().collect();
        assert_eq!(middle, b"e\rtwo\r\n\nl");
        assert_eq!(buffer.chunks(2..2).unwrap().count(), 0);
        assert_eq!(buffer.chunks(1..1).unwrap().count(), 0);
        assert_eq!(buffer.lines(100, 2).count(), 0);
        assert_eq!(buffer.lines(0, 0).count(), 0);
        let before = bytes(&buffer.snapshot());
        assert!(buffer.insert(buffer.len() + 1, b"bad").is_err());
        assert!(buffer.delete(0..buffer.len() + 1).is_err());
        let reversed = Range { start: 5, end: 1 };
        assert!(buffer.delete(reversed.clone()).is_err());
        assert!(buffer.chunks(reversed).is_err());
        assert_eq!(buffer.offset_to_line_col(buffer.len() + 1).unwrap(), None);
        assert_eq!(bytes(&buffer.snapshot()), before);
        struct FailingWriter;
        impl Write for FailingWriter {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("write failed"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        assert!(buffer.write_to(&mut FailingWriter).is_err());
    }

    #[test]
    fn changed_original_returns_errors_without_mutating_history() {
        use crate::FileState;
        // Rename a mapped file rather than truncate a live mapping (which
        // Windows disallows). No stale mapped pages need to be touched.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("buffer-changed-{}.bin", std::process::id()));
        let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&path).unwrap();
        file.set_len(crate::file::MAP_THRESHOLD).unwrap();
        file.write_all(b"a\nbody").unwrap();
        drop(file);
        let original = TextFile::open(&path).unwrap();
        let mut buffer = Buffer::from_file(original.clone()).unwrap();
        let snapshot = buffer.snapshot();
        let mut chunks = snapshot.chunks(0..2).unwrap();
        let mut lines = snapshot.lines(0, 1);
        let rotated = path.with_extension("old");
        fs::rename(&path, &rotated).unwrap();
        // Surface absence/replacement before dereferencing the old mapping.
        assert!(matches!(Buffer::from_file(original), Err(ReadError::Changed(FileState::Replaced))));
        assert!(matches!(chunks.next(), Some(Err(ReadError::Changed(FileState::Replaced)))));
        assert!(chunks.next().is_none());
        assert!(matches!(lines.next(), Some(Err(ReadError::Changed(FileState::Replaced)))));
        assert!(lines.next().is_none());
        assert!(matches!(snapshot.offset_to_line_col(1), Err(ReadError::Changed(FileState::Replaced))));
        assert!(matches!(snapshot.line_start(1), Err(ReadError::Changed(FileState::Replaced))));
        let mut output = Vec::new();
        let error = snapshot.write_to(&mut output).unwrap_err();
        assert!(matches!(
            error.get_ref().and_then(|e| e.downcast_ref::<ReadError>()),
            Some(ReadError::Changed(FileState::Replaced))
        ));
        assert!(output.is_empty());
        assert!(buffer.insert(1, b"edit").is_err());
        assert!(buffer.delete(1..2).is_err());
        assert!(Arc::ptr_eq(buffer.snapshot.root.as_ref().unwrap(), snapshot.root.as_ref().unwrap()));
        assert_eq!(buffer.undo_memory_bytes(), 0);
        assert_eq!(buffer.add_memory_bytes(), 0);
        assert!(!buffer.undo());
        drop(chunks);
        drop(snapshot);
        drop(buffer);
        fs::remove_file(rotated).unwrap();
    }

    #[test]
    fn sparse_original_splits_at_checkpoint_and_crlf_boundaries() {
        let initial = b"x\r\n".repeat(5000);
        let fixture = Fixture::new(&initial);
        let mut buffer = Buffer::from_file(fixture.open()).unwrap();
        let mut model = initial;
        // Reverse order retains many pieces of the original on both sides.
        for (step, at) in [14999, 12288, 6145, 6144, 6143, 3072, 3071, 2, 1, 0].into_iter().enumerate() {
            buffer.insert(at, b"A\n\r\n").unwrap();
            model.splice(at as usize..at as usize, b"A\n\r\n".iter().copied());
            check(&buffer, &model, step);
        }
        for at in 0..=model.len() {
            let line = memchr_iter(b'\n', &model[..at]).count() as u64;
            let col = model[..at].iter().rposition(|&byte| byte == b'\n').map_or(at, |p| at - p - 1);
            assert_eq!(buffer.offset_to_line_col(at as u64).unwrap(), Some((line, col as u64)));
        }
    }

    /// One-shot measurement, explicitly excluded from ordinary test runs.
    /// Generate fixtures/log-1g.log with the spec's textbench gen command, then
    /// run release `cargo test -p kwil-core-text measure_1g -- --ignored --nocapture`.
    #[test]
    #[ignore = "requires the generated 1 GB fixture; one-shot measurement only"]
    fn measure_1g() {
        use std::time::Instant;
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/log-1g.log");
        let start = Instant::now();
        let file = TextFile::open(&path).unwrap();
        let open_ms = start.elapsed().as_secs_f64() * 1000.0;
        assert!(file.len() >= 1 << 30);
        let start = Instant::now();
        let lines = file.build_index(4).unwrap().line_count();
        let index_ms = start.elapsed().as_secs_f64() * 1000.0;
        let len = file.len();
        let middle = len / 2;
        let original_jump = file.index().unwrap().line_start(file.bytes().unwrap(), 15_000_000).unwrap();
        let start = Instant::now();
        let mut buffer = Buffer::from_file(file).unwrap();
        let attach_ms = start.elapsed().as_secs_f64() * 1000.0;
        let before = buffer.snapshot();
        let start = Instant::now();
        buffer.insert(middle, b"inserted\r\n").unwrap();
        let insert_ms = start.elapsed().as_secs_f64() * 1000.0;
        let start = Instant::now();
        let jump = buffer.line_start(15_000_000).unwrap().unwrap();
        let jump_ms = start.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(buffer.len(), len + 10);
        assert_eq!(buffer.line_count(), lines + 1);
        // The edit inserts a newline before 15M; that line is now original
        // 14,999,999 plus inserted bytes. Check against the pinned version.
        assert!(middle < original_jump);
        assert_eq!(jump, before.line_start(14_999_999).unwrap().unwrap() + 10);
        assert_eq!(before.len(), len);
        assert_eq!(before.line_count(), lines);
        assert_eq!(buffer.add_memory_bytes(), 10);
        println!(
            "bytes={len} lines={lines} open_ms={open_ms:.6} index_ms={index_ms:.6} from_file_ready_ms={attach_ms:.6} insert_ms={insert_ms:.6} line_15m_ms={jump_ms:.6} undo_retained_bytes={} add_bytes={}",
            buffer.undo_memory_bytes(),
            buffer.add_memory_bytes()
        );
        #[cfg(windows)]
        println!("private_bytes={}", private_bytes());
    }

    #[cfg(windows)]
    fn private_bytes() -> usize {
        #[repr(C)]
        struct Counters {
            cb: u32,
            faults: u32,
            values: [usize; 9],
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentProcess() -> *mut std::ffi::c_void;
        }
        #[link(name = "psapi")]
        unsafe extern "system" {
            fn GetProcessMemoryInfo(process: *mut std::ffi::c_void, counters: *mut Counters, size: u32) -> i32;
        }
        let size = std::mem::size_of::<Counters>() as u32;
        let mut counters = Counters { cb: size, faults: 0, values: [0; 9] };
        // SAFETY: a valid pseudo-handle and correctly sized writable structure.
        assert_ne!(unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, size) }, 0);
        counters.values[8]
    }
}
