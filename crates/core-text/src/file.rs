use crate::line_index::{DEFAULT_EVERY, LineIndex};
use memchr::memchr_iter;
use std::fs::{File, Metadata};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

/// Files below this size are read into memory; larger ones are mapped
/// (spec §5, "mmap safety").
pub const MAP_THRESHOLD: u64 = 64 << 20;

/// Opening never falls back to a file-sized allocation above MAP_THRESHOLD.
#[derive(Debug)]
pub enum OpenError {
    Io(io::Error),
    /// Mapping was refused. A bounded positioned-read source is a follow-up.
    CannotMap(io::Error),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::CannotMap(e) => write!(f, "cannot map large file (positioned-read fallback not yet available): {e}"),
        }
    }
}

impl std::error::Error for OpenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(match self {
            Self::Io(e) | Self::CannotMap(e) => e,
        })
    }
}

impl From<io::Error> for OpenError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<OpenError> for io::Error {
    fn from(e: OpenError) -> Self {
        match e {
            OpenError::Io(e) => e,
            e @ OpenError::CannotMap(_) => io::Error::other(e),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileState {
    Unchanged,
    Grown {
        new_len: u64,
    },
    Truncated,
    Replaced,
    /// Same length and identity, but mtime changed; reopen before reading.
    Modified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Text,
    Binary,
}

#[derive(Debug, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: SystemTime,
    identity: (u64, u64),
}

impl Stamp {
    fn read(file: &File) -> io::Result<Self> {
        let metadata = file.metadata()?;
        Ok(Self { len: metadata.len(), modified: metadata.modified()?, identity: identity(file, &metadata)? })
    }

    fn state(&self, now: &Self) -> FileState {
        if self.identity != now.identity {
            FileState::Replaced
        } else if now.len < self.len {
            FileState::Truncated
        } else if now.len > self.len {
            FileState::Grown { new_len: now.len }
        } else if self.modified != now.modified {
            FileState::Modified
        } else {
            FileState::Unchanged
        }
    }
}

#[cfg(unix)]
fn identity(_: &File, metadata: &Metadata) -> io::Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(windows)]
fn identity(file: &File, _: &Metadata) -> io::Result<(u64, u64)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle};
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: the file owns a live handle and info is writable for the call.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((info.dwVolumeSerialNumber as u64, ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64))
}

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
    path: PathBuf,
    // Keep the original handle alive, including after rotation (prevents id reuse).
    _file: File,
    stamp: Stamp,
    bytes: Bytes,
    kind: OnceLock<FileKind>,
    index: OnceLock<LineIndex>,
    scanned: AtomicU64,
}

/// An open file. The first screen is readable at once; the line index
/// fills in on a background thread.
///
/// Mapped views are not immutable snapshots of external writers. Frontends
/// must call check() on watcher events and before region reads after waking
/// (including indexing workers). On any change stop reading the old view and
/// reopen; growth alone may use reopen_grown(). Checking cannot close the
/// TOCTOU race with an uncooperative writer truncating during a read. A future
/// positioned-read source is needed for that stronger guarantee on Unix.
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
    /// Opens `path` without scanning it. kind() samples lazily and caches.
    pub fn open(path: &Path) -> Result<Self, OpenError> {
        Self::open_with_mapper(path, |f| {
            // SAFETY: callers obey the mapped-view contract documented above.
            // This check does not synchronize with concurrent external writers.
            unsafe { memmap2::Mmap::map(f) }
        })
    }

    fn open_with_mapper(path: &Path, map: impl FnOnce(&File) -> io::Result<memmap2::Mmap>) -> Result<Self, OpenError> {
        let path = std::path::absolute(path)?;
        let mut f = File::open(&path)?;
        let stamp = Stamp::read(&f)?;
        let len = stamp.len;
        let bytes = if len < MAP_THRESHOLD {
            let mut v = Vec::with_capacity(len as usize);
            // Bound the read even if the file grows after metadata was read.
            (&mut f).take(len).read_to_end(&mut v)?;
            Bytes::Owned(v)
        } else {
            Bytes::Mapped(map(&f).map_err(OpenError::CannotMap)?)
        };
        if Stamp::read(&f)? != stamp || bytes.as_slice().len() as u64 != len {
            return Err(io::Error::other("file changed while opening; retry").into());
        }
        Ok(TextFile {
            shared: Arc::new(Shared {
                path,
                _file: f,
                stamp,
                bytes,
                kind: OnceLock::new(),
                index: OnceLock::new(),
                scanned: AtomicU64::new(0),
            }),
        })
    }

    /// Cheap metadata/identity check; never touches potentially stale pages.
    /// A missing path counts as replaced. Other I/O errors propagate, and
    /// callers must not read the old mapping on error.
    pub fn check(&self) -> io::Result<FileState> {
        match File::open(&self.shared.path) {
            Ok(file) => Ok(self.shared.stamp.state(&Stamp::read(&file)?)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(FileState::Replaced),
            Err(e) => Err(e),
        }
    }

    /// Reopens append-only growth. Existing checkpoints are cloned and only
    /// appended bytes are scanned; if no index is ready it stays lazy. Existing
    /// clones retain their old view. Truncation, rotation or same-size writes
    /// require open(). The caller must know the old prefix was not rewritten;
    /// size and mtime alone cannot establish append-only writes.
    pub fn reopen_grown(&self) -> Result<Self, OpenError> {
        let next = Self::open(&self.shared.path)?;
        if !matches!(self.shared.stamp.state(&next.shared.stamp), FileState::Grown { .. }) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "file did not only grow; open it afresh").into());
        }
        if let Some(index) = self.index() {
            let mut index = index.clone();
            index.extend(next.bytes(), self.len());
            let _ = next.shared.index.set(index);
            next.shared.scanned.store(next.len(), Ordering::Relaxed);
        }
        Ok(next)
    }

    /// Bounded binary heuristic, sampled once on first use (at most 128 KiB).
    /// Like bytes(), call check() before using this after a wake-up on a map.
    pub fn kind(&self) -> FileKind {
        *self.shared.kind.get_or_init(|| detect_kind(self.bytes()))
    }

    /// Raw view; call check() before mapped-region reads after a wake-up.
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

fn detect_kind(bytes: &[u8]) -> FileKind {
    if bytes.starts_with(b"\xef\xbb\xbf") || bytes.starts_with(b"\xff\xfe") || bytes.starts_with(b"\xfe\xff") {
        return FileKind::Text;
    }
    const SAMPLE: usize = 64 << 10;
    let first_end = bytes.len().min(SAMPLE);
    let last_start = first_end.max(bytes.len().saturating_sub(SAMPLE));
    let samples = [&bytes[..first_end], &bytes[last_start..]];
    let mut controls = 0;
    let mut total = 0;
    for sample in samples {
        if memchr::memchr(0, sample).is_some() {
            return FileKind::Binary;
        }
        controls += sample.iter().filter(|&&b| matches!(b, 1..=8 | 14..=31 | 127)).count();
        total += sample.len();
    }
    // Tabs, LF, CR, form feed and vertical tab are valid text whitespace.
    if controls * 100 > total * 10 { FileKind::Binary } else { FileKind::Text }
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
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, AtomicUsize};

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let time = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
            let path = std::env::temp_dir().join(format!(
                "kwil-text-{}-{time}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn file(&self, bytes: &[u8]) -> PathBuf {
            let path = self.0.join("file");
            std::fs::write(&path, bytes).unwrap();
            path
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn check_reports_growth() {
        let dir = TempDir::new();
        let path = dir.file(b"partial");
        let file = TextFile::open(&path).unwrap();
        assert_eq!(file.check().unwrap(), FileState::Unchanged);
        std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b" line\n").unwrap();
        assert_eq!(file.check().unwrap(), FileState::Grown { new_len: 13 });
        // Small-file owned bytes remain a stable snapshot.
        assert_eq!(file.bytes(), b"partial");
    }

    #[test]
    fn check_reports_truncation() {
        let dir = TempDir::new();
        let path = dir.file(b"one\ntwo\n");
        let file = TextFile::open(&path).unwrap();
        std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_len(2).unwrap();
        assert_eq!(file.check().unwrap(), FileState::Truncated);
        assert!(file.reopen_grown().is_err());
    }

    #[test]
    fn check_reports_replacement_and_removal() {
        let dir = TempDir::new();
        let path = dir.file(b"same");
        let file = TextFile::open(&path).unwrap();
        let rotated = dir.0.join("rotated");
        std::fs::rename(&path, &rotated).unwrap();
        assert_eq!(file.check().unwrap(), FileState::Replaced);
        std::fs::write(&path, b"same").unwrap();
        // Equal length and mtime must still be caught by identity.
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(file.shared.stamp.modified))
            .unwrap();
        assert_eq!(file.check().unwrap(), FileState::Replaced);
        assert!(file.reopen_grown().is_err());
    }

    #[test]
    fn check_reports_same_size_modification() {
        let dir = TempDir::new();
        let path = dir.file(b"text");
        let file = TextFile::open(&path).unwrap();
        let mut writer = File::options().write(true).open(&path).unwrap();
        writer.write_all(b"edit").unwrap();
        writer
            .set_times(
                std::fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(12345)),
            )
            .unwrap();
        assert_eq!(file.check().unwrap(), FileState::Modified);
        assert!(file.reopen_grown().is_err());
    }

    #[test]
    fn reopen_grown_carries_index() {
        let dir = TempDir::new();
        let path = dir.file(b"first\npartial");
        let file = TextFile::open(&path).unwrap();
        file.build_index(1);
        std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b" end\nnext").unwrap();
        let grown = file.reopen_grown().unwrap();
        assert!(grown.index().is_some(), "index must be carried forward eagerly");
        assert_eq!(grown.index().unwrap().line_count(), 3);
        assert_eq!(grown.lines(1, 5).unwrap(), vec![b"partial end".as_slice(), b"next"]);
        assert_eq!(grown.progress().scanned, grown.len());
        assert_eq!(grown.check().unwrap(), FileState::Unchanged);
        assert_eq!(file.index().unwrap().line_count(), 2);
        assert!(grown.reopen_grown().is_err());
        let lazy = TextFile::open(&path).unwrap();
        std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"\n").unwrap();
        assert!(lazy.reopen_grown().unwrap().index().is_none());
    }

    #[test]
    fn mapping_refusal_never_reads_large_file() {
        // Sparse fixture above 1 MB stays in ignored fixtures/ per repo rules.
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
        std::fs::create_dir_all(&root).unwrap();
        let dir = TempDir(root.join(format!("kw9-map-{}", std::process::id())));
        std::fs::create_dir(&dir.0).unwrap();
        let path = dir.file(b"small");
        let small = TextFile::open_with_mapper(&path, |_| panic!("small file must not map")).unwrap();
        assert!(matches!(small.shared.bytes, Bytes::Owned(_)));
        File::options().write(true).open(&path).unwrap().set_len(MAP_THRESHOLD).unwrap();
        let error = TextFile::open_with_mapper(&path, |_| {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "mapping refused"))
        });
        assert!(matches!(error, Err(OpenError::CannotMap(_))));
        let mapped = TextFile::open(&path).unwrap();
        assert!(matches!(mapped.shared.bytes, Bytes::Mapped(_)));
        assert_eq!(mapped.check().unwrap(), FileState::Unchanged);
        mapped.build_index(2);
        std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"\nappended").unwrap();
        assert_eq!(mapped.check().unwrap(), FileState::Grown { new_len: MAP_THRESHOLD + 9 });
        let grown = mapped.reopen_grown().unwrap();
        assert!(matches!(grown.shared.bytes, Bytes::Mapped(_)));
        assert_eq!(grown.index().unwrap().line_count(), 2);
        assert_eq!(grown.lines(1, 1).unwrap(), vec![b"appended".as_slice()]);
    }

    #[test]
    fn kind_detects_text_and_boms() {
        let dir = TempDir::new();
        for bytes in [
            b"".as_slice(),
            b"hello\tworld\r\n",
            "日本語 🦀".as_bytes(),
            b"\xff\xfeh\0i\0",
            b"\xfe\xff\0h\0i",
            b"\xef\xbb\xbfUTF-8",
        ] {
            let path = dir.file(bytes);
            let file = TextFile::open(&path).unwrap();
            assert_eq!(file.kind(), FileKind::Text, "{bytes:?}");
            assert_eq!(file.shared.kind.get(), Some(&FileKind::Text));
        }
    }

    #[test]
    fn kind_detects_binary_in_bounded_samples() {
        let dir = TempDir::new();
        for bytes in [b"abc\0def".as_slice(), b"\x01\x02\x03abc", &[b'a'; 99].map(|_| 1)] {
            let path = dir.file(bytes);
            assert_eq!(TextFile::open(&path).unwrap().kind(), FileKind::Binary);
        }
        let mut bytes = vec![b'a'; 3 * (64 << 10)];
        bytes[64 << 10] = 0; // Unsampled middle must not cause a full scan.
        let path = dir.file(&bytes);
        assert_eq!(TextFile::open(&path).unwrap().kind(), FileKind::Text);
        *bytes.last_mut().unwrap() = 0;
        let path = dir.file(&bytes);
        assert_eq!(TextFile::open(&path).unwrap().kind(), FileKind::Binary);
    }

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
