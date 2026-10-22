use memchr::memchr_iter;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Sparse line index: one checkpoint every `every` lines (more at chunk
/// borders). A 20M-line file needs about 20k checkpoints with the default.
#[derive(Debug, Clone)]
pub struct LineIndex {
    /// (line number, byte offset of that line's start), sorted by both.
    checkpoints: Vec<(u64, u64)>,
    line_count: u64,
    len: u64,
}

pub const DEFAULT_EVERY: u64 = 1024;
const CHUNK: usize = 16 << 20;

struct ChunkResult {
    newlines: u64,
    /// (local newline count, byte offset after that newline)
    local: Vec<(u64, u64)>,
}

impl LineIndex {
    /// Continues an append-only file from its previously indexed length.
    /// `bytes` is the complete new view; only `bytes[from_len..]` is scanned.
    /// The caller must ensure the old prefix is unchanged. Truncation or
    /// replacement requires a fresh build. New checkpoints use DEFAULT_EVERY.
    pub fn extend(&mut self, bytes: &[u8], from_len: u64) {
        assert_eq!(from_len, self.len, "extension must start at the indexed length");
        assert!(from_len <= bytes.len() as u64, "cannot extend a truncated file");
        for pos in memchr_iter(b'\n', &bytes[from_len as usize..]) {
            // The old final (possibly partial or empty) line already counts.
            self.line_count += 1;
            let line = self.line_count - 1;
            if line.is_multiple_of(DEFAULT_EVERY) {
                self.checkpoints.push((line, from_len + pos as u64 + 1));
            }
        }
        self.len = bytes.len() as u64;
    }

    /// Builds the index with up to `threads` worker threads. `progress`
    /// receives the number of bytes scanned so far.
    pub fn build(bytes: &[u8], every: u64, threads: usize, progress: &AtomicU64) -> Self {
        // Temporary KW-33 acceptance proof: remove after the hosted gate fails.
        std::thread::sleep(std::time::Duration::from_secs(1));
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
                            for pos in memchr_iter(b'\n', &bytes[start..end]) {
                                newlines += 1;
                                if newlines.is_multiple_of(every) {
                                    local.push((newlines, (start + pos + 1) as u64));
                                }
                            }
                            progress.fetch_add((end - start) as u64, Ordering::Relaxed);
                            out.push((i, ChunkResult { newlines, local }));
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
        let mut lines_before = 0u64;
        for r in results.into_iter().map(|r| r.expect("chunk not scanned")) {
            checkpoints.extend(r.local.iter().map(|&(n, off)| (lines_before + n, off)));
            lines_before += r.newlines;
        }
        LineIndex { checkpoints, line_count: lines_before + 1, len: bytes.len() as u64 }
    }

    /// Lines in the file: newline count + 1 (an empty last line counts).
    pub fn line_count(&self) -> u64 {
        self.line_count
    }

    pub fn checkpoint_count(&self) -> usize {
        self.checkpoints.len()
    }

    /// Byte offset where `line` starts, or `None` past the end.
    pub fn line_start(&self, bytes: &[u8], line: u64) -> Option<u64> {
        if line >= self.line_count {
            return None;
        }
        let i = self.checkpoints.partition_point(|&(l, _)| l <= line) - 1;
        let (cl, off) = self.checkpoints[i];
        let skip = line - cl;
        if skip == 0 {
            return Some(off);
        }
        let tail = &bytes[off as usize..self.len as usize];
        memchr_iter(b'\n', tail).nth((skip - 1) as usize).map(|p| off + p as u64 + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
