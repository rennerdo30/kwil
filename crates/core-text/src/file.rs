use crate::line_index::{DEFAULT_EVERY, LineIndex};
use memchr::memchr_iter;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

/// Files below this size are read into memory; larger ones are mapped
/// (spec §5, "mmap safety").
pub const MAP_THRESHOLD: u64 = 64 << 20;

enum Bytes {
    Owned(Vec<u8>),
    Mapped(memmap2::Mmap),
}

impl Bytes {
    fn as_slice(&self) -> &[u8] {
        match self {
            Bytes::Owned(v) => v,
            Bytes::Mapped(m) => m,
        }
    }
}

struct Shared {
    bytes: Bytes,
    index: OnceLock<LineIndex>,
    scanned: AtomicU64,
}

/// An open file. The first screen is readable at once; the line index
/// fills in on a background thread.
#[derive(Clone)]
pub struct TextFile {
    shared: Arc<Shared>,
}

#[derive(Debug, Clone, Copy)]
pub struct IndexProgress {
    pub scanned: u64,
    pub total: u64,
}

impl TextFile {
    /// Opens `path` without scanning it.
    pub fn open(path: &Path) -> io::Result<Self> {
        let mut f = File::open(path)?;
        let len = f.metadata()?.len();
        let bytes = if len < MAP_THRESHOLD {
            let mut v = Vec::with_capacity(len as usize);
            f.read_to_end(&mut v)?;
            Bytes::Owned(v)
        } else {
            // SAFETY: a mapped file truncated by another process can fault on
            // access. M1 adds the watcher and size/mtime re-check from spec §5.
            match unsafe { memmap2::Mmap::map(&f) } {
                Ok(m) => Bytes::Mapped(m),
                Err(_) => {
                    let mut v = Vec::with_capacity(len as usize);
                    f.read_to_end(&mut v)?;
                    Bytes::Owned(v)
                }
            }
        };
        Ok(TextFile { shared: Arc::new(Shared { bytes, index: OnceLock::new(), scanned: AtomicU64::new(0) }) })
    }

    pub fn bytes(&self) -> &[u8] {
        self.shared.bytes.as_slice()
    }

    pub fn len(&self) -> u64 {
        self.bytes().len() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.bytes().is_empty()
    }

    /// Builds the line index on a new thread; `done` runs when it is ready
    /// (frontends use it to wake their event loop).
    pub fn index_in_background(
        &self,
        threads: usize,
        done: impl FnOnce() + Send + 'static,
    ) -> std::thread::JoinHandle<()> {
        let this = self.clone();
        std::thread::spawn(move || {
            this.build_index(threads);
            done();
        })
    }

    /// Builds the line index on the calling thread (no-op when built).
    pub fn build_index(&self, threads: usize) -> &LineIndex {
        let s = &self.shared;
        s.index.get_or_init(|| LineIndex::build(s.bytes.as_slice(), DEFAULT_EVERY, threads, &s.scanned))
    }

    pub fn index(&self) -> Option<&LineIndex> {
        self.shared.index.get()
    }

    pub fn progress(&self) -> IndexProgress {
        IndexProgress { scanned: self.shared.scanned.load(Ordering::Relaxed), total: self.len() }
    }

    /// Up to `count` lines from the start, without the index. Used to paint
    /// the first screen before any full pass.
    pub fn first_lines(&self, count: usize) -> Vec<&[u8]> {
        lines_from(self.bytes(), 0, count)
    }

    /// Up to `count` lines starting at `line`. `None` until the index is
    /// ready (callers keep showing what they have).
    pub fn lines(&self, line: u64, count: usize) -> Option<Vec<&[u8]>> {
        let index = self.index()?;
        let start = index.line_start(self.bytes(), line)?;
        Some(lines_from(self.bytes(), start as usize, count))
    }
}

/// Splits up to `count` lines from `start`, without line endings.
fn lines_from(bytes: &[u8], start: usize, count: usize) -> Vec<&[u8]> {
    let mut out = Vec::with_capacity(count);
    let mut at = start;
    let mut ends = memchr_iter(b'\n', &bytes[start..]);
    while out.len() < count {
        match ends.next() {
            Some(p) => {
                out.push(trim_cr(&bytes[at..start + p]));
                at = start + p + 1;
            }
            None => {
                out.push(trim_cr(&bytes[at..]));
                break;
            }
        }
    }
    out
}

fn trim_cr(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\r").unwrap_or(line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    /// Every line start via the sparse index matches a naive split, across
    /// chunk borders and with small checkpoint spacing.
    #[test]
    fn index_matches_naive_split() {
        let mut text = Vec::new();
        let mut seed = 7u64;
        // ~8k lines keep the debug-build test fast while still crossing many
        // chunk borders with the small chunk sizes below.
        for i in 0..8_000u64 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let n = (seed >> 58) as usize; // 0..63 bytes per line
            text.extend(std::iter::repeat_n(b'a' + (i % 26) as u8, n));
            text.extend_from_slice(if i % 3 == 0 { b"\r\n" } else { b"\n" });
        }
        text.extend_from_slice(b"last line without newline");

        let naive: Vec<u64> = std::iter::once(0).chain(memchr_iter(b'\n', &text).map(|p| p as u64 + 1)).collect();
        for (every, chunk) in [(1, 1 << 20), (7, 4096), (64, 1000), (1024, 1000), (1024, 16 << 20)] {
            let idx = LineIndex::build_chunked(&text, every, chunk, 4, &AtomicU64::new(0));
            assert_eq!(idx.line_count(), naive.len() as u64);
            for (line, &start) in naive.iter().enumerate() {
                assert_eq!(
                    idx.line_start(&text, line as u64),
                    Some(start),
                    "line {line}, every {every}, chunk {chunk}"
                );
            }
            assert_eq!(idx.line_start(&text, naive.len() as u64), None);
        }
    }

    #[test]
    fn lines_strip_endings() {
        let b = b"one\r\ntwo\n\nfour";
        assert_eq!(lines_from(b, 0, 10), vec![&b"one"[..], b"two", b"", b"four"]);
        assert_eq!(lines_from(b, 0, 2), vec![&b"one"[..], b"two"]);
    }
}
