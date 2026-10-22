use memchr::memchr_iter;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Sparse line index: one checkpoint every `every` lines (more at chunk
/// borders). A 20M-line file needs about 20k checkpoints with the default.
/// Separate byte-rank checkpoints every 64 KiB bound scans inside giant
/// lines, using 256 KiB per GiB of indexed input (before Vec spare capacity).
#[derive(Debug, Clone)]
pub struct LineIndex {
    /// (line number, byte offset of that line's start), sorted by both.
    checkpoints: Vec<(u64, u64)>,
    /// (newline count before offset, byte offset), including inside lines.
    rank_checkpoints: Vec<(u64, u64)>,
    line_count: u64,
    len: u64,
}

pub const DEFAULT_EVERY: u64 = 1024;
const CHUNK: usize = 16 << 20;
const RANK_BYTES: usize = 64 << 10;

struct ChunkResult {
    newlines: u64,
    /// (local newline count, byte offset after that newline)
    local: Vec<(u64, u64)>,
    ranks: Vec<(u64, u64)>,
}

impl LineIndex {
    /// Resident sparse checkpoints, used in conservative undo accounting.
    pub fn memory_bytes(&self) -> usize {
        (self.checkpoints.capacity() + self.rank_checkpoints.capacity()) * std::mem::size_of::<(u64, u64)>()
    }
    /// Continues an append-only file from its previously indexed length.
    /// `bytes` is the complete new view; only `bytes[from_len..]` is scanned.
    /// The caller must ensure the old prefix is unchanged. Truncation or
    /// replacement requires a fresh build. New checkpoints use DEFAULT_EVERY.
    pub fn extend(&mut self, bytes: &[u8], from_len: u64) {
        assert_eq!(from_len, self.len, "extension must start at the indexed length");
        assert!(from_len <= bytes.len() as u64, "cannot extend a truncated file");
        let mut start = from_len as usize;
        while start < bytes.len() {
            let last_rank = self.rank_checkpoints.last().unwrap().1;
            if start as u64 - last_rank == RANK_BYTES as u64 {
                self.rank_checkpoints.push((self.line_count - 1, start as u64));
            }
            let next_rank = self.rank_checkpoints.last().unwrap().1 + RANK_BYTES as u64;
            let end = bytes.len().min(next_rank as usize);
            for pos in memchr_iter(b'\n', &bytes[start..end]) {
                // The old final (possibly partial or empty) line already counts.
                self.line_count += 1;
                let line = self.line_count - 1;
                if line.is_multiple_of(DEFAULT_EVERY) {
                    self.checkpoints.push((line, start as u64 + pos as u64 + 1));
                }
            }
            start = end;
        }
        self.len = bytes.len() as u64;
    }

    /// Builds the index with up to `threads` worker threads. `progress`
    /// receives the number of bytes scanned so far.
    pub fn build(bytes: &[u8], every: u64, threads: usize, progress: &AtomicU64) -> Self {
        Self::build_chunked(bytes, every, CHUNK, threads, progress)
    }

    pub(crate) fn build_chunked(bytes: &[u8], every: u64, chunk: usize, threads: usize, progress: &AtomicU64) -> Self {
        let every = every.max(1);
        let chunks = bytes.len().div_ceil(chunk).max(1);
        let next = AtomicUsize::new(0);
        let mut results: Vec<Option<ChunkResult>> = (0..chunks).map(|_| None).collect();
        let threads = threads.clamp(1, chunks);

        std::thread::scope(|s| {
            let handles: Vec<_> = (0..threads)
                .map(|_| {
                    s.spawn(|| {
                        let mut out = Vec::new();
                        loop {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            if i >= chunks {
                                break;
                            }
                            let start = i * chunk;
                            let end = (start + chunk).min(bytes.len());
                            let mut newlines = 0u64;
                            let mut local = Vec::new();
                            let mut ranks = Vec::new();
                            for block_start in (start..end).step_by(RANK_BYTES) {
                                ranks.push((newlines, block_start as u64));
                                let block_end = (block_start + RANK_BYTES).min(end);
                                for pos in memchr_iter(b'\n', &bytes[block_start..block_end]) {
                                    newlines += 1;
                                    if newlines.is_multiple_of(every) {
                                        local.push((newlines, (block_start + pos + 1) as u64));
                                    }
                                }
                            }
                            progress.fetch_add((end - start) as u64, Ordering::Relaxed);
                            out.push((i, ChunkResult { newlines, local, ranks }));
                        }
                        out
                    })
                })
                .collect();
            for h in handles {
                for (i, r) in h.join().expect("line index worker panicked") {
                    results[i] = Some(r);
                }
            }
        });

        let mut checkpoints = vec![(0u64, 0u64)];
        let mut rank_checkpoints = vec![(0u64, 0u64)];
        let mut lines_before = 0u64;
        for r in results.into_iter().map(|r| r.expect("chunk not scanned")) {
            checkpoints.extend(r.local.iter().map(|&(n, off)| (lines_before + n, off)));
            rank_checkpoints
                .extend(r.ranks.iter().filter(|&&(_, off)| off != 0).map(|&(n, off)| (lines_before + n, off)));
            lines_before += r.newlines;
        }
        LineIndex { checkpoints, rank_checkpoints, line_count: lines_before + 1, len: bytes.len() as u64 }
    }

    /// Lines in the file: newline count + 1 (an empty last line counts).
    pub fn line_count(&self) -> u64 {
        self.line_count
    }

    pub fn checkpoint_count(&self) -> usize {
        self.checkpoints.len()
    }

    /// Zero-based line containing a byte offset, including EOF. Scans only
    /// at most 64 KiB from the preceding byte or line checkpoint.
    /// Returns `None` for an offset past the indexed bytes.
    pub fn line_of_offset(&self, bytes: &[u8], offset: u64) -> Option<u64> {
        self.line_of_offset_observed(bytes, offset, |_| {})
    }

    fn line_of_offset_observed(&self, bytes: &[u8], offset: u64, scanned: impl FnOnce(usize)) -> Option<u64> {
        if offset > self.len {
            return None;
        }
        let i = self.checkpoints.partition_point(|&(_, at)| at <= offset) - 1;
        let j = self.rank_checkpoints.partition_point(|&(_, at)| at <= offset) - 1;
        let (line, at) = if self.checkpoints[i].1 > self.rank_checkpoints[j].1 {
            self.checkpoints[i]
        } else {
            self.rank_checkpoints[j]
        };
        scanned((offset - at) as usize);
        Some(line + memchr_iter(b'\n', &bytes[at as usize..offset as usize]).count() as u64)
    }

    /// Byte offset where `line` starts, or `None` past the end.
    pub fn line_start(&self, bytes: &[u8], line: u64) -> Option<u64> {
        self.line_start_observed(bytes, line, |_| {})
    }

    fn line_start_observed(&self, bytes: &[u8], line: u64, scanned: impl FnOnce(usize)) -> Option<u64> {
        if line >= self.line_count {
            return None;
        }
        let i = self.checkpoints.partition_point(|&(l, _)| l <= line) - 1;
        let (mut cl, mut off) = self.checkpoints[i];
        if cl == line {
            scanned(0);
            return Some(off);
        }
        // A rank with count == line can already lie inside the requested
        // line. Use the last STRICTLY smaller rank, so its following block
        // contains the newline that starts this line, even after a giant line.
        let j = self.rank_checkpoints.partition_point(|&(count, _)| count < line) - 1;
        if self.rank_checkpoints[j].1 > off {
            (cl, off) = self.rank_checkpoints[j];
        }
        let end = self.rank_checkpoints.get(j + 1).map_or(self.len, |&(_, at)| at);
        let skip = line - cl;
        let tail = &bytes[off as usize..end as usize];
        scanned(tail.len());
        memchr_iter(b'\n', tail).nth((skip - 1) as usize).map(|p| off + p as u64 + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inverse_line_lookup_scans_one_byte_rank_block() {
        let mut bytes = vec![b'x'; 700_003];
        for at in [65_535, 65_536, 300_001, 699_999, 700_002] {
            bytes[at] = b'\n';
        }
        // Exercise rank boundaries, duplicate counts, sparse checkpoints,
        // non-grid chunk borders and append-created checkpoints.
        for every in [1, 3, DEFAULT_EVERY] {
            let mut index = LineIndex::build_chunked(&bytes[..500_000], every, 100_003, 2, &AtomicU64::new(0));
            index.extend(&bytes, 500_000);
            for (line, at) in std::iter::once(0)
                .chain(memchr_iter(b'\n', &bytes).map(|p| p as u64 + 1))
                .enumerate()
            {
                let mut scanned = 0;
                assert_eq!(index.line_start_observed(&bytes, line as u64, |n| scanned = n), Some(at));
                assert!(scanned <= RANK_BYTES, "line {line} scanned {scanned} bytes");
            }
            assert_eq!(index.line_start(&bytes, index.line_count()), None);
        }
    }

    #[test]
    fn rank_scan_is_byte_bounded_inside_giant_line() {
        // Generated fixture on disk: do not allocate a 100 MB Vec.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
        std::fs::create_dir_all(&dir).unwrap();
        struct RemoveFile(Option<std::path::PathBuf>);
        impl Drop for RemoveFile {
            fn drop(&mut self) {
                if let Some(path) = &self.0 {
                    std::fs::remove_file(path).unwrap();
                }
            }
        }
        let path = dir.join(format!("rank-long-line-{}.bin", std::process::id()));
        let mut cleanup = RemoveFile(None);
        let mut file = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&path).unwrap();
        cleanup.0 = Some(path);
        file.set_len(100_000_000).unwrap();
        use std::io::{Seek, SeekFrom, Write};
        let newlines = [0, 65_535, 65_536, 16_777_215];
        for at in newlines {
            file.seek(SeekFrom::Start(at)).unwrap();
            file.write_all(b"\n").unwrap();
        }
        // SAFETY: this test owns the file and does not resize it while mapped.
        let map = unsafe { memmap2::Mmap::map(&file).unwrap() };
        let index = LineIndex::build(&map, DEFAULT_EVERY, 2, &AtomicU64::new(0));
        for offset in [65_535, 65_536, 65_537, 16_777_217, 99_999_999, 100_000_000] {
            let mut scanned = 0;
            let expected = newlines.iter().filter(|&&at| at < offset).count() as u64;
            assert_eq!(index.line_of_offset_observed(&map, offset, |n| scanned += n), Some(expected));
            assert!(scanned <= 64 * 1024, "offset {offset} scanned {scanned} bytes");
        }
        assert_eq!(index.line_count(), newlines.len() as u64 + 1);
        for (line, at) in std::iter::once(0).chain(newlines.map(|at| at + 1)).enumerate() {
            assert_eq!(index.line_start(&map, line as u64), Some(at));
        }
        drop(map);
        drop(file);
        drop(cleanup);
    }

    #[test]
    fn extend_keeps_rank_scans_byte_bounded() {
        let mut bytes = vec![b'x'; 165_000];
        bytes[10] = b'\n';
        // A chunk boundary off the byte interval exercises continuation
        // from the last existing rank checkpoint, not a new global grid.
        let mut index = LineIndex::build_chunked(&bytes, DEFAULT_EVERY, 100_000, 2, &AtomicU64::new(0));
        for append in [vec![b'x'; 300_000], b"\n".to_vec(), vec![], vec![b'y'; 200_000]] {
            let from = bytes.len() as u64;
            bytes.extend_from_slice(&append);
            index.extend(&bytes, from);
            for offset in [from, from + append.len() as u64 / 2, bytes.len() as u64] {
                let expected = memchr_iter(b'\n', &bytes[..offset as usize]).count() as u64;
                let mut scanned = 0;
                assert_eq!(index.line_of_offset_observed(&bytes, offset, |n| scanned += n), Some(expected));
                assert!(scanned <= 64 * 1024, "appended offset {offset} scanned {scanned} bytes");
            }
        }
    }

    #[test]
    fn extend_matches_fresh_after_appends() {
        for initial in [b"".as_slice(), b"partial", b"one\n", b"one\r\npartial"] {
            let mut bytes = initial.to_vec();
            let mut index = LineIndex::build_chunked(&bytes, 3, 7, 2, &AtomicU64::new(0));
            let original = index.checkpoints.clone();
            for append in [b" suffix".as_slice(), b"\r", b"\nnext\n", b"", &b"row\n".repeat(4097), b"end"] {
                let from = bytes.len() as u64;
                bytes.extend_from_slice(append);
                index.extend(&bytes, from);
                let fresh = LineIndex::build(&bytes, DEFAULT_EVERY, 2, &AtomicU64::new(0));
                assert_eq!(index.line_count(), fresh.line_count());
                for line in 0..=fresh.line_count() {
                    assert_eq!(index.line_start(&bytes, line), fresh.line_start(&bytes, line), "line {line}");
                }
                assert!(index.checkpoints.starts_with(&original));
                assert!(index.checkpoint_count() <= original.len() + (index.line_count() / DEFAULT_EVERY) as usize);
            }
            // Existing checkpoint lookups do not depend on rescanning the old
            // prefix during extend: deliberately hide its newlines from extend.
            let old_len = bytes.len() as u64;
            bytes.fill(b'x');
            bytes.push(b'\n');
            let count = index.line_count();
            index.extend(&bytes, old_len);
            assert_eq!(index.line_count(), count + 1);
        }
    }

    #[test]
    #[should_panic(expected = "extension must start at the indexed length")]
    fn extend_rejects_wrong_offset() {
        let mut index = LineIndex::build(b"a", 1024, 1, &AtomicU64::new(0));
        index.extend(b"ab", 0);
    }

    #[test]
    #[should_panic(expected = "cannot extend a truncated file")]
    fn extend_rejects_truncation() {
        let mut index = LineIndex::build(b"abc", 1024, 1, &AtomicU64::new(0));
        index.extend(b"a", 3);
    }
}
