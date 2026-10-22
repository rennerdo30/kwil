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
    /// Builds the index with up to `threads` worker threads. `progress`
    /// receives the number of bytes scanned so far.
    pub fn build(bytes: &[u8], every: u64, threads: usize, progress: &AtomicU64) -> Self {
        Self::build_chunked(bytes, every, CHUNK, threads, progress)
    }

    pub(crate) fn build_chunked(
        bytes: &[u8],
        every: u64,
        chunk: usize,
        threads: usize,
        progress: &AtomicU64,
    ) -> Self {
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
                                if newlines % every == 0 {
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
        memchr_iter(b'\n', tail)
            .nth((skip - 1) as usize)
            .map(|p| off + p as u64 + 1)
    }
}
