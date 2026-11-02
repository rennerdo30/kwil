use crate::index_memory::{INDEX_MEMORY_BYTES, IndexMemory, IndexVec};
use crate::newlines::ends;
#[cfg(test)]
use memchr::memchr_iter;
use std::io;
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
    local: IndexVec<(u64, u64)>,
    ranks: IndexVec<(u64, u64)>,
}

impl LineIndex {
    /// Constant-time sparse checkpoint access for admitted worker continuations.
    pub(crate) fn worker_checkpoint_count(&self, ranks: bool) -> usize {
        if ranks { self.rank_checkpoints.len() } else { self.checkpoints.len() }
    }

    pub(crate) fn worker_checkpoint(&self, ranks: bool, index: usize) -> Option<(u64, u64)> {
        if ranks { self.rank_checkpoints.get(index).copied() } else { self.checkpoints.get(index).copied() }
    }

    /// Resident sparse checkpoints, used in conservative undo accounting.
    pub fn memory_bytes(&self) -> usize {
        (self.checkpoints.capacity() + self.rank_checkpoints.capacity()) * std::mem::size_of::<(u64, u64)>()
    }
    /// Continues an append-only file from its previously indexed length.
    /// `bytes` is the complete new view; only `bytes[from_len..]` is scanned.
    /// The caller must ensure the old prefix is unchanged. Truncation or
    /// replacement requires a fresh build. New checkpoints use DEFAULT_EVERY.
    pub fn extend(&mut self, bytes: &[u8], from_len: u64) -> io::Result<()> {
        self.extend_with_memory_limit(bytes, from_len, INDEX_MEMORY_BYTES)
    }

    pub fn extend_with_memory_limit(&mut self, bytes: &[u8], from_len: u64, limit: usize) -> io::Result<()> {
        self.extend_reader(bytes.len() as u64, from_len, limit, |range| {
            Ok(std::borrow::Cow::Borrowed(&bytes[range.start as usize..range.end as usize]))
        })
    }

    pub(crate) fn extend_reader<'a>(
        &mut self,
        len: u64,
        from_len: u64,
        limit: usize,
        read: impl Fn(std::ops::Range<u64>) -> io::Result<std::borrow::Cow<'a, [u8]>>,
    ) -> io::Result<()> {
        let len = usize::try_from(len).map_err(io::Error::other)?;
        assert_eq!(from_len, self.len, "extension must start at the indexed length");
        assert!(from_len <= len as u64, "cannot extend a truncated file");
        // Count only the append before reserving, so a refusal leaves all old
        // metrics intact. Exact reservation avoids hidden geometric growth.
        let edge_start = from_len.saturating_sub(1);
        let edge = read(edge_start..(from_len + 1).min(len as u64))?;
        let joins_crlf = crate::newlines::split_crlf(&edge, (from_len - edge_start) as usize);
        let mut added = 0;
        for start in (from_len as usize..len).step_by(RANK_BYTES) {
            let end = (start + RANK_BYTES).min(len);
            let bytes = read(start as u64..(end + 1).min(len) as u64)?;
            added += ends(&bytes, 0..end - start).count() as u64;
        }
        let old_breaks = self.line_count - 1 - u64::from(joins_crlf);
        let removes_line = joins_crlf && self.checkpoints.last().is_some_and(|&(_, at)| at == from_len);
        let lines = self.checkpoints.len() - usize::from(removes_line)
            + ((old_breaks + added) / DEFAULT_EVERY - old_breaks / DEFAULT_EVERY) as usize;
        let removes_rank = joins_crlf && self.rank_checkpoints.last().is_some_and(|&(_, at)| at == from_len);
        let rank_count = self.rank_checkpoints.len() - usize::from(removes_rank);
        let last_rank = self.rank_checkpoints[rank_count - 1].1 as usize;
        let ranks = rank_count + len.saturating_sub(last_rank + 1) / RANK_BYTES;
        let old_lines = self.checkpoints.capacity();
        let old_ranks = self.rank_checkpoints.capacity();
        let new_lines = lines.max(old_lines);
        let new_ranks = ranks.max(old_ranks);
        // Reserve one vector at a time; charge old+new during reallocation.
        let peak_lines = new_lines.checked_add(if lines > old_lines { old_lines } else { 0 });
        let peak_ranks = new_ranks.checked_add(if ranks > old_ranks { old_ranks } else { 0 });
        let needed = peak_lines
            .and_then(|n| n.checked_add(old_ranks))
            .zip(peak_ranks.and_then(|n| n.checked_add(new_lines)))
            .map(|(a, b)| a.max(b))
            .and_then(|n| n.checked_mul(std::mem::size_of::<(u64, u64)>()))
            .ok_or_else(|| IndexMemory::new(limit).error())?;
        if needed > limit {
            return Err(IndexMemory::new(limit).error());
        }
        self.checkpoints.try_reserve_exact(lines.saturating_sub(self.checkpoints.len())).map_err(io::Error::other)?;
        self.rank_checkpoints
            .try_reserve_exact(ranks.saturating_sub(self.rank_checkpoints.len()))
            .map_err(io::Error::other)?;
        let mut start = from_len as usize;
        if start < len && joins_crlf {
            self.line_count -= 1;
            // Only an EOF checkpoint can move when CR grows into CRLF.
            if self.checkpoints.last().is_some_and(|&(_, at)| at == from_len) {
                self.checkpoints.pop();
            }
            if self.rank_checkpoints.last().is_some_and(|&(_, at)| at == from_len) {
                self.rank_checkpoints.pop();
            }
        }
        while start < len {
            let last_rank = self.rank_checkpoints.last().unwrap().1;
            if start as u64 - last_rank == RANK_BYTES as u64 {
                self.rank_checkpoints.push((self.line_count - 1, start as u64));
            }
            let next_rank = self.rank_checkpoints.last().unwrap().1 + RANK_BYTES as u64;
            let end = len.min(next_rank as usize);
            let bytes = read(start as u64..(end + 1).min(len) as u64)?;
            for pos in ends(&bytes, 0..end - start) {
                // The old final (possibly partial or empty) line already counts.
                self.line_count += 1;
                let line = self.line_count - 1;
                if line.is_multiple_of(DEFAULT_EVERY) {
                    self.checkpoints.push((line, (start + pos) as u64));
                }
            }
            start = end;
        }
        self.len = len as u64;
        Ok(())
    }

    /// Builds the index with up to `threads` worker threads (at most four). `progress`
    /// receives the number of bytes scanned so far.
    pub fn build(bytes: &[u8], every: u64, threads: usize, progress: &AtomicU64) -> io::Result<Self> {
        Self::build_chunked(bytes, every, CHUNK, threads, progress)
    }

    /// Bounds the combined allocations of workers, intermediate results and
    /// final indexes. A refusal publishes no partial index.
    pub fn build_with_memory_limit(
        bytes: &[u8],
        every: u64,
        threads: usize,
        progress: &AtomicU64,
        limit: usize,
    ) -> io::Result<Self> {
        Self::build_chunked_with_limit(bytes, every, CHUNK, threads, progress, limit)
    }

    /// Sequential worker builder. Cancellation is checked before/after every
    /// <=64KiB scan, including before/after empty input. A cancelled build
    /// returns Interrupted and never publishes a partial index. `every` has
    /// the same chunk-local meaning (and zero normalization) as `build`.
    pub fn build_cancellable(
        bytes: &[u8],
        every: u64,
        progress: &AtomicU64,
        mut cancelled: impl FnMut() -> bool,
    ) -> io::Result<Self> {
        Self::build_checked(bytes, every, progress, &mut || {
            if cancelled() { Err(io::Error::from(io::ErrorKind::Interrupted)) } else { Ok(()) }
        })
    }

    pub(crate) fn build_checked<E: From<io::Error>>(
        bytes: &[u8],
        every: u64,
        progress: &AtomicU64,
        check: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<Self, E> {
        Self::build_checked_chunked(bytes, every, CHUNK, progress, check)
    }

    fn build_checked_chunked<E: From<io::Error>>(
        bytes: &[u8],
        every: u64,
        chunk: usize,
        progress: &AtomicU64,
        check: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<Self, E> {
        Self::build_checked_with_limit(bytes, every, chunk, progress, check, INDEX_MEMORY_BYTES)
    }

    fn build_checked_with_limit<E: From<io::Error>>(
        bytes: &[u8],
        every: u64,
        chunk: usize,
        progress: &AtomicU64,
        check: &mut impl FnMut() -> Result<(), E>,
        limit: usize,
    ) -> Result<Self, E> {
        check()?;
        let every = every.max(1);
        let memory = IndexMemory::new(limit);
        let mut checkpoints = IndexVec::new(&memory);
        let mut rank_checkpoints = IndexVec::new(&memory);
        checkpoints.push((0, 0))?;
        rank_checkpoints.push((0, 0))?;
        let mut lines_before = 0_u64;
        for start in (0..bytes.len()).step_by(chunk) {
            let end = start.saturating_add(chunk).min(bytes.len());
            let mut newlines = 0_u64;
            for block_start in (start..end).step_by(RANK_BYTES) {
                check()?;
                if block_start != 0 {
                    rank_checkpoints.push((lines_before + newlines, block_start as u64))?;
                }
                let block_end = block_start.saturating_add(RANK_BYTES).min(end);
                for pos in ends(bytes, block_start..block_end) {
                    newlines += 1;
                    if newlines.is_multiple_of(every) {
                        checkpoints.push((lines_before + newlines, pos as u64))?;
                    }
                }
                progress.fetch_add((block_end - block_start) as u64, Ordering::Relaxed);
                check()?;
            }
            lines_before += newlines;
        }
        check()?;
        Ok(Self {
            checkpoints: checkpoints.into_vec(),
            rank_checkpoints: rank_checkpoints.into_vec(),
            line_count: lines_before + 1,
            len: bytes.len() as u64,
        })
    }

    pub(crate) fn build_chunked(
        bytes: &[u8],
        every: u64,
        chunk: usize,
        threads: usize,
        progress: &AtomicU64,
    ) -> io::Result<Self> {
        Self::build_chunked_with_limit(bytes, every, chunk, threads, progress, INDEX_MEMORY_BYTES)
    }

    fn build_chunked_with_limit(
        bytes: &[u8],
        every: u64,
        chunk: usize,
        threads: usize,
        progress: &AtomicU64,
        limit: usize,
    ) -> io::Result<Self> {
        Self::build_reader(bytes.len() as u64, every, chunk, threads, progress, limit, |range, _scratch| {
            Ok(std::borrow::Cow::Borrowed(&bytes[range.start as usize..range.end as usize]))
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn build_reader<'a>(
        len: u64,
        every: u64,
        chunk: usize,
        threads: usize,
        progress: &AtomicU64,
        limit: usize,
        read: impl Fn(std::ops::Range<u64>, &mut IndexVec<u8>) -> io::Result<std::borrow::Cow<'a, [u8]>> + Sync,
    ) -> io::Result<Self> {
        let every = every.max(1);
        let chunks = usize::try_from(len.div_ceil(chunk as u64).max(1)).map_err(io::Error::other)?;
        let next = AtomicUsize::new(0);
        let memory = IndexMemory::new(limit);
        let mut results = IndexVec::new(&memory);
        results.reserve(chunks)?;
        results.items.resize_with(chunks, || None);
        let threads = threads.clamp(1, chunks.min(4));

        std::thread::scope(|s| -> io::Result<()> {
            let handles: Vec<_> = (0..threads)
                .map(|_| {
                    s.spawn(|| -> io::Result<_> {
                        let mut out = IndexVec::new(&memory);
                        let mut scratch = IndexVec::new(&memory);
                        loop {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            if i >= chunks {
                                break;
                            }
                            let start = i as u64 * chunk as u64;
                            let end = (start + chunk as u64).min(len);
                            let mut newlines = 0u64;
                            let mut local = IndexVec::new(&memory);
                            let mut ranks = IndexVec::new(&memory);
                            for window_start in (start..end).step_by(4 << 20) {
                                let window_end = (window_start + (4 << 20)).min(end);
                                let bytes = read(window_start..(window_end + 1).min(len), &mut scratch)?;
                                for block_start in (window_start..window_end).step_by(RANK_BYTES) {
                                    ranks.push((newlines, block_start))?;
                                    let block_end = (block_start + RANK_BYTES as u64).min(window_end);
                                    for pos in ends(
                                        &bytes,
                                        (block_start - window_start) as usize..(block_end - window_start) as usize,
                                    ) {
                                        newlines += 1;
                                        if newlines.is_multiple_of(every) {
                                            local.push((newlines, window_start + pos as u64))?;
                                        }
                                    }
                                }
                                if let std::borrow::Cow::Owned(bytes) = bytes {
                                    scratch.items = bytes;
                                }
                            }
                            progress.fetch_add(end - start, Ordering::Relaxed);
                            out.push((i, ChunkResult { newlines, local, ranks }))?;
                        }
                        Ok(out)
                    })
                })
                .collect();
            for h in handles {
                let mut out = h.join().map_err(|_| io::Error::other("line index worker panicked"))??;
                for (i, r) in out.items.drain(..) {
                    results.items[i] = Some(r);
                }
            }
            Ok(())
        })?;

        let mut checkpoints = IndexVec::new(&memory);
        let mut rank_checkpoints = IndexVec::new(&memory);
        checkpoints.push((0, 0))?;
        rank_checkpoints.push((0, 0))?;
        let mut lines_before = 0u64;
        for r in results.items.iter_mut().map(|r| r.take().expect("chunk not scanned")) {
            for &(n, off) in &r.local.items {
                checkpoints.push((lines_before + n, off))?;
            }
            for &(n, off) in r.ranks.items.iter().filter(|&&(_, off)| off != 0) {
                rank_checkpoints.push((lines_before + n, off))?;
            }
            lines_before += r.newlines;
        }
        Ok(LineIndex {
            checkpoints: checkpoints.into_vec(),
            rank_checkpoints: rank_checkpoints.into_vec(),
            line_count: lines_before + 1,
            len,
        })
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
        Some(line + ends(bytes, at as usize..offset as usize).count() as u64)
    }

    pub(crate) fn rank_reader(
        &self,
        offset: u64,
        read: impl FnOnce(std::ops::Range<u64>) -> io::Result<Vec<u8>>,
    ) -> io::Result<Option<u64>> {
        if offset > self.len {
            return Ok(None);
        }
        let i = self.checkpoints.partition_point(|&(_, at)| at <= offset) - 1;
        let j = self.rank_checkpoints.partition_point(|&(_, at)| at <= offset) - 1;
        let (line, at) = if self.checkpoints[i].1 > self.rank_checkpoints[j].1 {
            self.checkpoints[i]
        } else {
            self.rank_checkpoints[j]
        };
        let bytes = read(at..(offset + 1).min(self.len))?;
        Ok(Some(line + ends(&bytes, 0..(offset - at) as usize).count() as u64))
    }

    pub(crate) fn start_reader(
        &self,
        line: u64,
        read: impl FnOnce(std::ops::Range<u64>) -> io::Result<Vec<u8>>,
    ) -> io::Result<Option<u64>> {
        if line >= self.line_count {
            return Ok(None);
        }
        let i = self.checkpoints.partition_point(|&(l, _)| l <= line) - 1;
        let (mut cl, mut off) = self.checkpoints[i];
        if cl == line {
            return Ok(Some(off));
        }
        let j = self.rank_checkpoints.partition_point(|&(count, _)| count < line) - 1;
        if self.rank_checkpoints[j].1 > off {
            (cl, off) = self.rank_checkpoints[j];
        }
        let end = self.rank_checkpoints.get(j + 1).map_or(self.len, |&(_, at)| at);
        let bytes = read(off..(end + 1).min(self.len))?;
        Ok(ends(&bytes, 0..(end - off) as usize).nth((line - cl - 1) as usize).map(|p| off + p as u64))
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
        ends(bytes, off as usize..end as usize).nth((skip - 1) as usize).map(|p| p as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_memory_limit_covers_parallel_results_and_checkpoint_growth() {
        let bytes = vec![b'\n'; 1024];
        let progress = AtomicU64::new(0);
        let error = LineIndex::build_with_memory_limit(&bytes, 1, 4, &progress, 4096).unwrap_err();
        assert!(error.to_string().contains("4096-byte memory limit"));
        let index = LineIndex::build_with_memory_limit(&bytes, 1, 4, &progress, 128 << 10).unwrap();
        assert_eq!(index.line_count(), 1025);
        assert_eq!(index.line_start(&bytes, 1024), Some(1024));
        // The result directory itself must be admitted before scanning.
        progress.store(0, Ordering::Relaxed);
        assert!(LineIndex::build_chunked_with_limit(&bytes, 1024, 1, 4, &progress, 128).is_err());
        assert_eq!(progress.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn index_memory_limit_covers_checked_decoded_builder() {
        let bytes = [b'\n'; 256];
        let progress = AtomicU64::new(0);
        let error =
            LineIndex::build_checked_with_limit(&bytes, 1, CHUNK, &progress, &mut || Ok::<(), io::Error>(()), 128)
                .unwrap_err();
        assert!(error.to_string().contains("128-byte memory limit"));
        assert_eq!(progress.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn index_memory_limit_append_refusal_preserves_old_index() {
        for bytes in [vec![b'\n'; 9000], vec![b'\r'; 9000], vec![b'x'; 5 * RANK_BYTES]] {
            let mut index = LineIndex::build(b"x", DEFAULT_EVERY, 1, &AtomicU64::new(0)).unwrap();
            let previous = index.clone();
            let limit = index.memory_bytes();
            assert!(index.extend_with_memory_limit(&bytes, 1, limit).is_err());
            assert_eq!(index.len, previous.len);
            assert_eq!(index.line_count, previous.line_count);
            assert_eq!(index.checkpoints, previous.checkpoints);
            assert_eq!(index.rank_checkpoints, previous.rank_checkpoints);
            assert_eq!(index.memory_bytes(), limit);
            index.extend_with_memory_limit(&bytes, 1, 4096).unwrap();
            assert_eq!(index.len, bytes.len() as u64);
            assert!(index.memory_bytes() <= 4096);
        }
    }

    #[test]
    fn classic_mac_and_mixed_breaks_cross_chunks_and_appends() {
        let bytes = b"a\r\nb\rc\n\r\rd\r";
        let starts = [0, 3, 5, 7, 8, 9, 11];
        for chunk in [1, 2, 3, 7] {
            for every in [1, 3, DEFAULT_EVERY] {
                let index = LineIndex::build_chunked(bytes, every, chunk, 2, &AtomicU64::new(0)).unwrap();
                assert_eq!(index.line_count(), starts.len() as u64);
                for (line, &at) in starts.iter().enumerate() {
                    assert_eq!(index.line_start(bytes, line as u64), Some(at));
                }
                for at in 0..=bytes.len() {
                    assert_eq!(
                        index.line_of_offset(bytes, at as u64),
                        Some((starts.partition_point(|&s| s <= at as u64) - 1) as u64)
                    );
                }
            }
        }
        let mut index = LineIndex::build(b"a\r", 1, 1, &AtomicU64::new(0)).unwrap();
        index.extend(b"a\r\nb\r", 2).unwrap();
        assert_eq!(index.line_count(), 3);
        assert_eq!(index.line_start(b"a\r\nb\r", 1), Some(3));
        assert_eq!(index.line_start(b"a\r\nb\r", 2), Some(5));
    }

    #[test]
    fn cancellable_build_stops_after_first_rank_block_without_result() {
        let bytes = vec![b'x'; 128 << 10];
        let progress = AtomicU64::new(0);
        let thread = std::thread::current().id();
        let mut previous = 0;
        let mut polls = 0;
        let result = LineIndex::build_cancellable(&bytes, DEFAULT_EVERY, &progress, || {
            assert_eq!(std::thread::current().id(), thread, "worker builder must not launch a scan thread");
            let scanned = progress.load(Ordering::Relaxed);
            assert!(scanned >= previous && scanned - previous <= RANK_BYTES as u64, "scan between polls exceeds64KiB");
            previous = scanned;
            polls += 1;
            scanned >= RANK_BYTES as u64
        });
        assert!(matches!(result, Err(ref error) if error.kind() == io::ErrorKind::Interrupted));
        assert_eq!(progress.load(Ordering::Relaxed), RANK_BYTES as u64, "cancel must stop before the second block");
        assert!(polls >= 2);
    }

    #[test]
    fn cancellable_build_checks_empty_before_after_and_nonempty_before_scan() {
        let progress = AtomicU64::new(0);
        for bytes in [&b""[..], &b"a\r\nb\n"[..]] {
            let result = LineIndex::build_cancellable(bytes, 0, &progress, || true);
            assert!(matches!(result, Err(ref error) if error.kind() == io::ErrorKind::Interrupted));
            assert_eq!(progress.load(Ordering::Relaxed), 0);
        }
        let mut polls = 0;
        let result = LineIndex::build_cancellable(b"", 0, &progress, || {
            polls += 1;
            polls == 2
        });
        assert!(matches!(result, Err(ref error) if error.kind() == io::ErrorKind::Interrupted));
        assert_eq!(polls, 2, "empty input must also check at completion");
        let index = LineIndex::build_cancellable(b"", 0, &progress, || false).unwrap();
        assert_eq!(index.line_count(), 1);
        assert_eq!(index.line_start(b"", 0), Some(0));
    }

    #[test]
    fn cancellable_build_preserves_parallel_checkpoints_every_and_lookup_bounds() {
        let mut bytes = vec![b'x'; 128 << 10];
        for at in [0, 1000, 65_534, 65_536, 100_002, 131_071] {
            bytes[at] = b'\n';
        }
        for at in [65_535, 120_001] {
            bytes[at] = b'\r';
        }
        let starts: Vec<_> = std::iter::once(0).chain(ends(&bytes, 0..bytes.len()).map(|at| at as u64)).collect();
        for every in [0, 1, 3, DEFAULT_EVERY] {
            for chunk in [65_535, 100_003, CHUNK] {
                let old = LineIndex::build_chunked(&bytes, every, chunk, 2, &AtomicU64::new(0)).unwrap();
                let progress = AtomicU64::new(0);
                let new =
                    LineIndex::build_checked_chunked(&bytes, every, chunk, &progress, &mut || Ok::<(), io::Error>(()))
                        .unwrap();
                assert_eq!(new.checkpoints, old.checkpoints, "every{every} chunk{chunk}");
                assert_eq!(new.rank_checkpoints, old.rank_checkpoints, "every{every} chunk{chunk}");
                assert_eq!(new.line_count(), old.line_count());
                assert_eq!(progress.load(Ordering::Relaxed), bytes.len() as u64);
                for (line, &start) in starts.iter().enumerate() {
                    let mut scanned = 0;
                    assert_eq!(new.line_start_observed(&bytes, line as u64, |n| scanned = n), Some(start));
                    assert!(scanned <= RANK_BYTES);
                }
                for offset in [0, 65_535, 65_536, 65_537, 100_003, 131_071, 131_072] {
                    let mut scanned = 0;
                    assert_eq!(
                        new.line_of_offset_observed(&bytes, offset, |n| scanned = n),
                        old.line_of_offset(&bytes, offset)
                    );
                    assert!(scanned <= RANK_BYTES);
                }
                assert_eq!(new.line_start(&bytes, new.line_count()), None);
                assert_eq!(new.line_of_offset(&bytes, bytes.len() as u64 + 1), None);
                assert!(new.memory_bytes() <= 4096, "fixture metadata must stay sparse");
            }
        }
    }

    #[test]
    fn inverse_line_lookup_scans_one_byte_rank_block() {
        let mut bytes = vec![b'x'; 700_003];
        for at in [65_535, 65_536, 300_001, 699_999, 700_002] {
            bytes[at] = b'\n';
        }
        // Exercise rank boundaries, duplicate counts, sparse checkpoints,
        // non-grid chunk borders and append-created checkpoints.
        for every in [1, 3, DEFAULT_EVERY] {
            let mut index = LineIndex::build_chunked(&bytes[..500_000], every, 100_003, 2, &AtomicU64::new(0)).unwrap();
            index.extend(&bytes, 500_000).unwrap();
            for (line, at) in std::iter::once(0).chain(memchr_iter(b'\n', &bytes).map(|p| p as u64 + 1)).enumerate() {
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
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path =
            dir.join(format!("rank-long-line-{}-{}.bin", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
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
        let index = LineIndex::build(&map, DEFAULT_EVERY, 2, &AtomicU64::new(0)).unwrap();
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
        let mut index = LineIndex::build_chunked(&bytes, DEFAULT_EVERY, 100_000, 2, &AtomicU64::new(0)).unwrap();
        for append in [vec![b'x'; 300_000], b"\n".to_vec(), vec![], vec![b'y'; 200_000]] {
            let from = bytes.len() as u64;
            bytes.extend_from_slice(&append);
            index.extend(&bytes, from).unwrap();
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
            let mut index = LineIndex::build_chunked(&bytes, 3, 7, 2, &AtomicU64::new(0)).unwrap();
            let original = index.checkpoints.clone();
            for append in [b" suffix".as_slice(), b"\r", b"\nnext\n", b"", &b"row\n".repeat(4097), b"end"] {
                let from = bytes.len() as u64;
                bytes.extend_from_slice(append);
                index.extend(&bytes, from).unwrap();
                let fresh = LineIndex::build(&bytes, DEFAULT_EVERY, 2, &AtomicU64::new(0)).unwrap();
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
            index.extend(&bytes, old_len).unwrap();
            assert_eq!(index.line_count(), count + 1);
        }
    }

    #[test]
    #[should_panic(expected = "extension must start at the indexed length")]
    fn extend_rejects_wrong_offset() {
        let mut index = LineIndex::build(b"a", 1024, 1, &AtomicU64::new(0)).unwrap();
        index.extend(b"ab", 0).unwrap();
    }

    #[test]
    #[should_panic(expected = "cannot extend a truncated file")]
    fn extend_rejects_truncation() {
        let mut index = LineIndex::build(b"abc", 1024, 1, &AtomicU64::new(0)).unwrap();
        index.extend(b"a", 3).unwrap();
    }
}
