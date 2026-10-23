//! Worker-only compressed-file preparation and save payloads (KW-49).
//!
//! Decode into a private disk file, then force-map it; never allocate decoded
//! file-sized storage. Keep the `PreparedFile` in the document's worker-owned
//! retirement list. `try_finish` atomically refuses while TextFile clones,
//! buffers, undo history or snapshots still reference its mapping. Retry only
//! after reader-release events. Neither PreparedFile nor its backing unlinks
//! in Drop: abandoning an owner leaves an artifact for controlled recovery.
//!
//! Recompression only writes the caller's temporary writer. The save layer
//! must sync/preserve metadata/atomically replace, and keep journal identity
//! tied to the compressed original, never to the decoded scratch pathname.
//!
//! Dependency: flate2 =1.1.10, default-features=false, rust_backend. Its fetched
//! Cargo.toml and LICENSE-{MIT,APACHE} were checked; only API/source docs were
//! consulted. Gzip headers, CRC/ISIZE and all members are validated by flate2.

use flate2::{Compression, bufread::MultiGzDecoder, write::GzEncoder};
use kwil_core_text::{Snapshot, TextFile};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

pub const STREAM_CHUNK: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CompressionMode {
    #[default]
    Auto,
    Raw,
    Gzip,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompressionFormat {
    Raw,
    Gzip,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnsupportedFormat {
    Zip,
    Bzip2,
    Xz,
    Zstandard,
    Lz4,
    SkippableFrame,
    SevenZip,
    Rar,
    UnixCompress,
    Zlib,
}

impl std::fmt::Display for UnsupportedFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Zip => "ZIP",
            Self::Bzip2 => "bzip2",
            Self::Xz => "XZ",
            Self::Zstandard => "Zstandard",
            Self::Lz4 => "LZ4",
            Self::SkippableFrame => "Zstandard/LZ4 skippable frame",
            Self::SevenZip => "7-Zip",
            Self::Rar => "RAR",
            Self::UnixCompress => "Unix compress",
            Self::Zlib => "zlib",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DetectedFormat {
    Raw,
    Gzip,
    Unsupported(UnsupportedFormat),
}

/// Signature-only sniff. Eight bytes suffice; filenames are deliberately absent.
/// Raw deflate/Brotli have no reliable signature and cannot be auto-detected.
pub fn detect_magic(prefix: &[u8]) -> DetectedFormat {
    use UnsupportedFormat::*;
    let unsupported = if prefix.starts_with(&[0x1f, 0x8b]) {
        return DetectedFormat::Gzip;
    } else if prefix.starts_with(b"PK\x03\x04")
        || prefix.starts_with(b"PK\x05\x06")
        || prefix.starts_with(b"PK\x07\x08")
    {
        Zip
    } else if prefix.starts_with(b"BZh") && matches!(prefix.get(3), Some(b'1'..=b'9')) {
        Bzip2
    } else if prefix.starts_with(b"\xfd7zXZ\0") {
        Xz
    } else if prefix.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        Zstandard
    } else if prefix.starts_with(&[0x04, 0x22, 0x4d, 0x18]) {
        Lz4
    } else if matches!(prefix.first(), Some(0x50..=0x5f)) && prefix.get(1..4) == Some(&[0x2a, 0x4d, 0x18]) {
        SkippableFrame
    } else if prefix.starts_with(b"7z\xbc\xaf\x27\x1c") {
        SevenZip
    } else if prefix.starts_with(b"Rar!\x1a\x07\0") || prefix.starts_with(b"Rar!\x1a\x07\x01\0") {
        Rar
    } else if prefix.starts_with(&[0x1f, 0x9d]) || prefix.starts_with(&[0x1f, 0xa0]) {
        UnixCompress
    } else if prefix.len() >= 2
        && prefix[0] & 0x0f == 8
        && prefix[0] >> 4 <= 7
        && u16::from_be_bytes([prefix[0], prefix[1]]).is_multiple_of(31)
    {
        Zlib
    } else {
        return DetectedFormat::Raw;
    };
    DetectedFormat::Unsupported(unsupported)
}

#[derive(Debug)]
pub enum CompressionError {
    Cancelled,
    Unsupported(UnsupportedFormat),
    InvalidGzip(io::Error),
    Io(io::Error),
    /// Failed preparation could not remove its own partial file/directory.
    Cleanup {
        path: PathBuf,
        cause: Box<CompressionError>,
        error: io::Error,
    },
}

impl std::fmt::Display for CompressionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("compressed-file operation cancelled"),
            Self::Unsupported(format) => {
                write!(f, "unsupported compressed format: {format}; choose Raw to inspect bytes")
            }
            Self::InvalidGzip(error) => write!(f, "corrupt or invalid gzip stream: {error}"),
            Self::Io(error) => write!(f, "compressed-file I/O failed: {error}"),
            Self::Cleanup { path, cause, error } => {
                write!(f, "{cause}; partial backing retained at {}: {error}", path.display())
            }
        }
    }
}

impl std::error::Error for CompressionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) | Self::InvalidGzip(error) => Some(error),
            Self::Cleanup { cause, .. } => Some(cause.as_ref()),
            _ => None,
        }
    }
}

impl From<io::Error> for CompressionError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Successful preparation always has text_file()=Some. After a failed unlink
/// during retirement it is None; that owner is for cleanup retries only.
#[must_use = "retain the prepared backing and retire it on the file worker"]
pub struct PreparedFile {
    source: PathBuf,
    format: CompressionFormat,
    mode: CompressionMode,
    file: Option<TextFile>,
    backing: Option<Backing>,
    cleanup_error: Option<io::Error>,
}

/// Metadata distinguishes automatic gzip detection from a manual override.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompressionNotice {
    pub mode: CompressionMode,
    pub format: CompressionFormat,
}

impl PreparedFile {
    pub fn mode(&self) -> CompressionMode {
        self.mode
    }

    pub fn notice(&self) -> Option<CompressionNotice> {
        (self.mode != CompressionMode::Auto || self.format == CompressionFormat::Gzip)
            .then_some(CompressionNotice { mode: self.mode, format: self.format })
    }

    /// Extract a failed preparation's retained owner exactly once through typed
    /// save contexts. The worker registry must retain the returned owner.
    pub fn take_from(error: &io::Error) -> Option<Self> {
        let mut current: &(dyn std::error::Error + 'static) = error;
        loop {
            if let Some(pending) = current.downcast_ref::<PendingPreparedRetirement>() {
                return pending.owner.lock().unwrap_or_else(|p| p.into_inner()).take();
            }
            current =
                if let Some(io) = current.downcast_ref::<io::Error>() { io.get_ref()? } else { current.source()? };
        }
    }
    pub fn source_path(&self) -> &Path {
        &self.source
    }

    pub fn format(&self) -> CompressionFormat {
        self.format
    }

    pub fn text_file(&self) -> Option<&TextFile> {
        self.file.as_ref()
    }

    pub fn backing_path(&self) -> Option<&Path> {
        self.backing.as_ref().map(|backing| backing.path.as_path())
    }

    pub fn cleanup_error(&self) -> Option<&io::Error> {
        self.cleanup_error.as_ref()
    }

    /// Worker-only. Atomic uniqueness closes the last mapping before unlink.
    /// On live readers or cleanup errors return the owner for event-driven retry.
    /// Dropping the returned owner does not delete the backing on that thread.
    #[allow(clippy::result_large_err)] // Return ownership intact, without allocating on retirement.
    pub fn try_finish(mut self) -> Result<(), Self> {
        self.cleanup_error = None;
        if let Some(file) = self.file.take()
            && let Err(file) = file.try_close()
        {
            self.file = Some(file);
            return Err(self);
        }
        if let Some(backing) = &self.backing
            && let Err(error) = backing.cleanup()
        {
            self.cleanup_error = Some(error);
            return Err(self);
        }
        self.backing = None;
        Ok(())
    }
}

struct Backing {
    directory: PathBuf,
    path: PathBuf,
}

impl Backing {
    fn cleanup(&self) -> io::Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => (),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error),
        }
        // Never recurse: only our known create_new payload and empty directory.
        match fs::remove_dir(&self.directory) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    }
}

/// This guard exists only inside worker preparation, never in a UI result.
struct WorkerScratch {
    backing: Option<Backing>,
    file: Option<File>,
}

impl WorkerScratch {
    fn create(parent: &Path) -> Result<Self, CompressionError> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let parent = std::path::absolute(parent)?;
        for _ in 0..128 {
            let directory =
                parent.join(format!(".kwil-decoded-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            match create_private_directory(&directory) {
                Ok(()) => (),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
            let path = directory.join("payload");
            let mut scratch = Self { backing: Some(Backing { directory, path }), file: None };
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&scratch.backing.as_ref().unwrap().path) {
                Ok(file) => scratch.file = Some(file),
                Err(error) => return Err(scratch.abort(error.into())),
            }
            return Ok(scratch);
        }
        Err(io::Error::new(io::ErrorKind::AlreadyExists, "private compressed backing names exhausted").into())
    }

    fn abort(mut self, cause: CompressionError) -> CompressionError {
        self.file.take();
        let backing = self.backing.take().unwrap();
        match backing.cleanup() {
            Ok(()) => cause,
            Err(error) => CompressionError::Cleanup { path: backing.directory, cause: Box::new(cause), error },
        }
    }
}

impl Drop for WorkerScratch {
    fn drop(&mut self) {
        self.file.take();
        if let Some(backing) = &self.backing {
            let _ = backing.cleanup();
        }
    }
}

#[cfg(unix)]
fn create_private_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new().mode(0o700).create(path)
}

#[cfg(windows)]
fn create_private_directory(path: &Path) -> io::Result<()> {
    use std::{
        mem::{size_of, size_of_val},
        os::windows::ffi::OsStrExt,
        ptr::null_mut,
    };
    use windows_sys::Win32::{
        Foundation::{CloseHandle, LocalFree},
        Security::{
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION,
            },
            GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
        },
        Storage::FileSystem::CreateDirectoryW,
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };
    let mut token = null_mut();
    // SAFETY: out-pointers are valid; every successful handle/allocation is released.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut user = [0usize; 256];
    let mut needed = 0;
    let ok = unsafe {
        GetTokenInformation(token, TokenUser, user.as_mut_ptr().cast(), size_of_val(&user) as u32, &mut needed)
    };
    let token_error = if ok == 0 { Some(io::Error::last_os_error()) } else { None };
    unsafe {
        CloseHandle(token);
    }
    if let Some(error) = token_error {
        return Err(error);
    }
    let sid = unsafe { (*(user.as_ptr().cast::<TOKEN_USER>())).User.Sid };
    let mut sid_text = null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut sid_text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut len = 0;
    while len < 256 && unsafe { *sid_text.add(len) } != 0 {
        len += 1;
    }
    let sid_string =
        if len < 256 { String::from_utf16(unsafe { std::slice::from_raw_parts(sid_text, len) }).ok() } else { None };
    unsafe {
        LocalFree(sid_text.cast());
    }
    let sid_string = sid_string.ok_or_else(|| io::Error::other("invalid current-user SID"))?;
    // Protected DACL, inherited by files/subdirectories: only this user has access.
    let descriptor: Vec<u16> = format!("D:P(A;OICI;FA;;;{sid_string})").encode_utf16().chain(Some(0)).collect();
    let mut sd = null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(descriptor.as_ptr(), SDDL_REVISION, &mut sd, null_mut())
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let attrs = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd,
        bInheritHandle: 0,
    };
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let ok = unsafe { CreateDirectoryW(path.as_ptr(), &attrs) };
    let result = if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(()) };
    unsafe {
        LocalFree(sd);
    }
    result
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), CompressionError> {
    if cancel.load(Ordering::Relaxed) { Err(CompressionError::Cancelled) } else { Ok(()) }
}

// Other rather than Interrupted: Read::read_exact retries Interrupted forever.
fn cancelled_io() -> io::Error {
    io::Error::other("compressed-file operation cancelled")
}

struct Cancellable<'a, R> {
    inner: R,
    cancel: &'a AtomicBool,
}

impl<R: Read> Read for Cancellable<'_, R> {
    fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(cancelled_io());
        }
        let count = into.len().min(STREAM_CHUNK);
        self.inner.read(&mut into[..count])
    }
}

impl<R: BufRead> BufRead for Cancellable<'_, R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(cancelled_io());
        }
        let bytes = self.inner.fill_buf()?;
        Ok(&bytes[..bytes.len().min(STREAM_CHUNK)])
    }

    fn consume(&mut self, count: usize) {
        self.inner.consume(count);
    }
}

fn read_chunk(reader: &mut impl Read, into: &mut [u8], cancel: &AtomicBool) -> Result<usize, CompressionError> {
    loop {
        check_cancel(cancel)?;
        match reader.read(into) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                check_cancel(cancel)?;
                return Err(error.into());
            }
            Ok(count) => {
                check_cancel(cancel)?;
                return Ok(count);
            }
        }
    }
}

/// Worker-only. The scratch parent must exist; no user data is modified.
/// Auto ignores suffixes and refuses recognized unsupported signatures. Raw is
/// an explicit byte-view override; Gzip requires a valid gzip stream.
pub fn prepare_file(
    path: &Path,
    mode: CompressionMode,
    scratch_parent: &Path,
    cancel: &AtomicBool,
) -> Result<PreparedFile, CompressionError> {
    check_cancel(cancel)?;
    let source = std::path::absolute(path)?;
    let mut input = File::open(&source)?;
    let before = input.metadata()?;
    let mut prefix = [0_u8; 8];
    let mut len = 0;
    if mode != CompressionMode::Raw {
        while len < prefix.len() {
            let count = read_chunk(&mut input, &mut prefix[len..], cancel)?;
            if count == 0 {
                break;
            }
            len += count;
        }
    }
    let format = match (mode, detect_magic(&prefix[..len])) {
        (CompressionMode::Raw, _) | (CompressionMode::Auto, DetectedFormat::Raw) => CompressionFormat::Raw,
        (CompressionMode::Auto, DetectedFormat::Unsupported(format)) => {
            return Err(CompressionError::Unsupported(format));
        }
        (CompressionMode::Gzip, detected) if detected != DetectedFormat::Gzip => {
            return Err(CompressionError::InvalidGzip(io::Error::new(
                io::ErrorKind::InvalidData,
                "gzip signature missing",
            )));
        }
        _ => CompressionFormat::Gzip,
    };
    if format == CompressionFormat::Raw {
        check_cancel(cancel)?;
        let file = TextFile::open_saved_version(&source).map_err(io::Error::from)?;
        check_cancel(cancel)?;
        return Ok(PreparedFile { source, format, mode, file: Some(file), backing: None, cleanup_error: None });
    }
    input.seek(SeekFrom::Start(0))?;
    let mut prepared = prepare_gzip(&source, &mut input, scratch_parent, cancel)?;
    prepared.mode = mode;
    // Catch ordinary concurrent edits before publishing decoded bytes. The
    // parent still owns original-path identity/watch/journal/save conflict checks.
    let unchanged = (|| -> io::Result<bool> {
        let after = input.metadata()?;
        Ok(before.len() == after.len() && before.modified()? == after.modified()?)
    })();
    if !matches!(unchanged, Ok(true)) || cancel.load(Ordering::Relaxed) {
        let cause = if cancel.load(Ordering::Relaxed) {
            CompressionError::Cancelled
        } else {
            CompressionError::Io(
                unchanged.err().unwrap_or_else(|| io::Error::other("compressed source changed while decoding")),
            )
        };
        // No clones have escaped: close the local map, then clean on this worker.
        prepared.file.take();
        let backing = prepared.backing.take().unwrap();
        return Err(match backing.cleanup() {
            Ok(()) => cause,
            Err(error) => CompressionError::Cleanup { path: backing.directory, cause: Box::new(cause), error },
        });
    }
    Ok(prepared)
}

fn prepare_gzip(
    source: &Path,
    input: &mut impl Read,
    scratch_parent: &Path,
    cancel: &AtomicBool,
) -> Result<PreparedFile, CompressionError> {
    check_cancel(cancel)?;
    let mut scratch = WorkerScratch::create(scratch_parent)?;
    let result = (|| {
        decode_gzip(input, scratch.file.as_mut().unwrap(), cancel)?;
        scratch.file.take();
        check_cancel(cancel)?;
        let file = TextFile::open_saved_version(&scratch.backing.as_ref().unwrap().path).map_err(io::Error::from)?;
        check_cancel(cancel)?;
        Ok(file)
    })();
    match result {
        Ok(file) => Ok(PreparedFile {
            source: source.to_path_buf(),
            format: CompressionFormat::Gzip,
            mode: CompressionMode::Gzip,
            file: Some(file),
            backing: scratch.backing.take(),
            cleanup_error: None,
        }),
        Err(error) => Err(scratch.abort(error)),
    }
}

fn decode_gzip(input: &mut impl Read, writer: &mut impl Write, cancel: &AtomicBool) -> Result<(), CompressionError> {
    check_cancel(cancel)?;
    // Guard BOTH refills and already-buffered header/member reads. Even a
    // long sequence of empty gzip members can be cancelled inside flate2.
    let buffered = BufReader::with_capacity(STREAM_CHUNK, Cancellable { inner: input, cancel });
    let mut decoder = MultiGzDecoder::new(Cancellable { inner: buffered, cancel });
    let mut bytes = [0_u8; STREAM_CHUNK];
    loop {
        check_cancel(cancel)?;
        let count = match decoder.read(&mut bytes) {
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                check_cancel(cancel)?;
                return Err(match error.kind() {
                    io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof => {
                        CompressionError::InvalidGzip(error)
                    }
                    _ => CompressionError::Io(error),
                });
            }
        };
        check_cancel(cancel)?;
        if count == 0 {
            return Ok(());
        }
        writer.write_all(&bytes[..count])?;
    }
}

struct Output<'a, W> {
    writer: &'a mut W,
    cancel: &'a AtomicBool,
    aborted: &'a std::cell::Cell<bool>,
}

impl<W: Write> Write for Output<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.aborted.get() || self.cancel.load(Ordering::Relaxed) {
            return Err(cancelled_io());
        }
        self.writer.write(&bytes[..bytes.len().min(STREAM_CHUNK)])
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.aborted.get() || self.cancel.load(Ordering::Relaxed) {
            return Err(cancelled_io());
        }
        self.writer.flush()
    }
}

fn recompress_with(
    writer: &mut impl Write,
    cancel: &AtomicBool,
    feed: impl FnOnce(&mut dyn Write) -> Result<u64, CompressionError>,
) -> Result<u64, CompressionError> {
    check_cancel(cancel)?;
    let aborted = std::cell::Cell::new(false);
    let mut encoder = GzEncoder::new(Output { writer, cancel, aborted: &aborted }, Compression::default());
    let result = feed(&mut encoder).and_then(|count| {
        check_cancel(cancel)?;
        encoder.try_finish()?;
        check_cancel(cancel)?;
        Ok(count)
    });
    // flate2's writer Drop retries finishing. Disable I/O before Drop even if
    // the source/writer failed independently of cancellation; never seal a
    // truncated source as a successful gzip member or keep writing after error.
    aborted.set(true);
    drop(encoder);
    match result {
        Err(_) if cancel.load(Ordering::Relaxed) => Err(CompressionError::Cancelled),
        result => result,
    }
}

/// Worker-only save payload seam. Invalid bytes/BOM/line endings are untouched;
/// concatenated input members may become one member with identical decoded bytes.
/// Header metadata and original compressed bitstream are not preserved.
/// Returns decoded bytes written; on error the caller must discard its temp.
pub fn recompress_snapshot(
    snapshot: &Snapshot,
    writer: &mut impl Write,
    cancel: &AtomicBool,
) -> Result<u64, CompressionError> {
    recompress_with(writer, cancel, |encoder| {
        for chunk in snapshot.chunks(0..snapshot.len())? {
            check_cancel(cancel)?;
            let chunk = chunk.map_err(io::Error::other)?;
            for bytes in chunk.chunks(STREAM_CHUNK) {
                check_cancel(cancel)?;
                encoder.write_all(bytes)?;
            }
        }
        Ok(snapshot.len())
    })
}

/// Worker-only variant for a raw byte/chunk reader (e.g. encoding-save output).
/// Does not flush/sync/replace the destination; atomic-save owns those steps.
pub fn recompress_reader(
    reader: &mut impl Read,
    writer: &mut impl Write,
    cancel: &AtomicBool,
) -> Result<u64, CompressionError> {
    recompress_with(writer, cancel, |encoder| {
        let mut bytes = [0_u8; STREAM_CHUNK];
        let mut total = 0_u64;
        loop {
            let count = read_chunk(reader, &mut bytes, cancel)?;
            if count == 0 {
                return Ok(total);
            }
            encoder.write_all(&bytes[..count])?;
            total = total.checked_add(count as u64).ok_or_else(|| io::Error::other("gzip source length overflow"))?;
        }
    })
}

impl CompressionError {
    /// Keep cleanup ownership and cancellation kind intact at I/O API edges.
    pub fn into_io_error(self) -> io::Error {
        match self {
            Self::Cancelled => io::Error::new(io::ErrorKind::Interrupted, self),
            Self::Io(error) => error,
            Self::Cleanup { path, cause, error } => {
                let owner = PreparedFile {
                    source: path.clone(),
                    format: CompressionFormat::Gzip,
                    mode: CompressionMode::Gzip,
                    file: None,
                    backing: Some(Backing { path: path.join("payload"), directory: path }),
                    cleanup_error: Some(error),
                };
                let cause = cause.into_io_error();
                io::Error::new(cause.kind(), PendingPreparedRetirement { cause, owner: Mutex::new(Some(owner)) })
            }
            other => io::Error::new(io::ErrorKind::InvalidData, other),
        }
    }
}
struct PendingPreparedRetirement {
    cause: io::Error,
    owner: Mutex<Option<PreparedFile>>,
}
impl std::fmt::Debug for PendingPreparedRetirement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingGzipRetirement").field("cause", &self.cause).finish_non_exhaustive()
    }
}
impl std::fmt::Display for PendingPreparedRetirement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.cause.fmt(f)?;
        let owner = self.owner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(owner) = owner.as_ref() {
            if let Some(path) = owner.backing_path() {
                write!(f, "; gzip cleanup pending at {}", path.display())?;
            }
            if let Some(error) = owner.cleanup_error() {
                write!(f, ": {error}")?;
            }
        } else {
            f.write_str("; gzip cleanup retained by worker")?;
        }
        Ok(())
    }
}
impl std::error::Error for PendingPreparedRetirement {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}

/// Worker-only preparation of a completed immutable gzip payload. Identity is
/// the compressed destination, independent of the temporary reader. Close the
/// reader before attempting replacement on Windows.
pub fn prepare_gzip_payload(
    identity: &Path,
    input: &mut impl Read,
    scratch_parent: &Path,
    cancel: &AtomicBool,
) -> Result<PreparedFile, CompressionError> {
    let identity = std::path::absolute(identity)?;
    prepare_gzip(&identity, input, scratch_parent, cancel)
}

/// Streams encoded output through the guarded gzip encoder. Finishes CRC/trailer
/// on success; suppresses Drop writes after failure. Preserves typed I/O causes.
pub fn recompress_payload(
    writer: &mut impl Write,
    cancel: &AtomicBool,
    feed: impl FnOnce(&mut dyn Write) -> io::Result<()>,
) -> Result<(), CompressionError> {
    recompress_with(writer, cancel, |encoder| {
        feed(encoder)?;
        Ok(0)
    })
    .map(|_| ())
}
#[cfg(test)]
mod tests {
    use super::*;
    use kwil_core_text::Buffer;
    use std::{cell::Cell, io::Cursor};

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "kwil-compressed-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            create_private_directory(&root).unwrap();
            Self(root)
        }

        fn source(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.0.join(name);
            let mut file = OpenOptions::new().write(true).create_new(true).open(&path).unwrap();
            file.write_all(bytes).unwrap();
            path
        }

        fn backing_count(&self) -> usize {
            fs::read_dir(&self.0).unwrap().filter(|entry| entry.as_ref().unwrap().file_type().unwrap().is_dir()).count()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            // Only known fixture files, never recursive cleanup of an unknown tree.
            if let Ok(entries) = fs::read_dir(&self.0) {
                for entry in entries.flatten() {
                    if entry.file_type().is_ok_and(|kind| kind.is_file()) {
                        let _ = fs::remove_file(entry.path());
                    } else if entry.file_name().to_string_lossy().starts_with(".kwil-decoded-") {
                        let _ = fs::remove_file(entry.path().join("payload"));
                        let _ = fs::remove_dir(entry.path());
                    }
                }
            }
            let _ = fs::remove_dir(&self.0);
        }
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn decoded(bytes: &[u8]) -> Vec<u8> {
        // Tiny isolated fixtures only; production never uses read_to_end.
        let mut result = Vec::new();
        MultiGzDecoder::new(bytes).read_to_end(&mut result).unwrap();
        result
    }

    fn cancel() -> AtomicBool {
        AtomicBool::new(false)
    }

    #[test]
    fn signatures_refuse_common_unsupported_formats() {
        use UnsupportedFormat::*;
        for (prefix, format) in [
            (&b"PK\x03\x04"[..], Zip),
            (&b"PK\x05\x06"[..], Zip),
            (&b"PK\x07\x08"[..], Zip),
            (&b"BZh9"[..], Bzip2),
            (&b"\xfd7zXZ\0"[..], Xz),
            (&b"\x28\xb5\x2f\xfd"[..], Zstandard),
            (&b"\x04\x22\x4d\x18"[..], Lz4),
            (&b"\x50\x2a\x4d\x18"[..], SkippableFrame),
            (&b"7z\xbc\xaf\x27\x1c"[..], SevenZip),
            (&b"Rar!\x1a\x07\0"[..], Rar),
            (&b"Rar!\x1a\x07\x01\0"[..], Rar),
            (&b"\x1f\x9d"[..], UnixCompress),
            (&b"\x78\x9c"[..], Zlib),
        ] {
            assert_eq!(detect_magic(prefix), DetectedFormat::Unsupported(format));
        }
        assert_eq!(detect_magic(b"\x1f\x8b"), DetectedFormat::Gzip);
        for raw in [&b""[..], &b"plain"[..], &b"BZhello"[..], &b"\x78\x9d"[..]] {
            assert_eq!(detect_magic(raw), DetectedFormat::Raw);
        }
    }

    #[test]
    fn magic_wins_over_name_and_small_decoded_file_is_mapped() {
        let fixture = Fixture::new();
        let bytes = b"\xef\xbb\xbftext\r\n\xff\0\xc0\xaf\n";
        let source = fixture.source("looks-raw.txt", &gzip(bytes));
        let prepared = prepare_file(&source, CompressionMode::Auto, &fixture.0, &cancel()).unwrap();
        assert_eq!(prepared.format(), CompressionFormat::Gzip);
        assert_eq!(prepared.source_path(), source);
        assert_eq!(prepared.text_file().unwrap().bytes().unwrap(), bytes);
        assert_eq!(prepared.text_file().unwrap().resident_original_bytes(), 0);
        assert!(prepared.backing_path().unwrap().exists());
        assert!(prepared.try_finish().is_ok());
        assert_eq!(fixture.backing_count(), 0);

        let source = fixture.source("plain.gz", b"uncompressed");
        let prepared = prepare_file(&source, CompressionMode::Auto, &fixture.0, &cancel()).unwrap();
        assert_eq!(prepared.format(), CompressionFormat::Raw);
        assert!(prepared.backing_path().is_none());
        assert_eq!(prepared.text_file().unwrap().bytes().unwrap(), b"uncompressed");
        assert!(prepared.try_finish().is_ok());
    }

    #[test]
    fn manual_raw_and_gzip_overrides_are_explicit() {
        let fixture = Fixture::new();
        let encoded = gzip(b"text");
        let source = fixture.source("gzip", &encoded);
        let raw = prepare_file(&source, CompressionMode::Raw, &fixture.0, &cancel()).unwrap();
        assert_eq!(raw.text_file().unwrap().bytes().unwrap(), encoded);
        assert_eq!(raw.format(), CompressionFormat::Raw);
        assert!(raw.try_finish().is_ok());
        let gzip = prepare_file(&source, CompressionMode::Gzip, &fixture.0, &cancel()).unwrap();
        assert_eq!(gzip.text_file().unwrap().bytes().unwrap(), b"text");
        assert!(gzip.try_finish().is_ok());

        let source = fixture.source("unsupported", b"\xfd7zXZ\0stuff");
        let error = prepare_file(&source, CompressionMode::Auto, &fixture.0, &cancel()).err().unwrap();
        assert!(matches!(error, CompressionError::Unsupported(UnsupportedFormat::Xz)));
        assert!(error.to_string().contains("unsupported compressed format: XZ"));
        assert!(matches!(
            prepare_file(&source, CompressionMode::Gzip, &fixture.0, &cancel()),
            Err(CompressionError::InvalidGzip(_))
        ));
        let raw = prepare_file(&source, CompressionMode::Raw, &fixture.0, &cancel()).unwrap();
        assert_eq!(raw.text_file().unwrap().bytes().unwrap(), b"\xfd7zXZ\0stuff");
        assert!(raw.try_finish().is_ok());
        assert_eq!(fixture.backing_count(), 0);
    }

    #[test]
    fn concatenated_members_include_empty_and_invalid_bytes() {
        let fixture = Fixture::new();
        let encoded = [gzip(b"first\r\n"), gzip(b""), gzip(b"\xffsecond\0")].concat();
        let source = fixture.source("members", &encoded);
        let prepared = prepare_file(&source, CompressionMode::Auto, &fixture.0, &cancel()).unwrap();
        assert_eq!(prepared.text_file().unwrap().bytes().unwrap(), b"first\r\n\xffsecond\0");
        assert!(prepared.try_finish().is_ok());
    }

    #[test]
    fn crc_size_truncation_header_and_trailing_garbage_fail_and_clean_up() {
        let fixture = Fixture::new();
        let good = gzip(b"decoded-prefix-before-error\xff");
        let mut crc = good.clone();
        let at = crc.len() - 8;
        crc[at] ^= 1;
        let mut size = good.clone();
        let at = size.len() - 4;
        size[at] ^= 1;
        let mut header = good.clone();
        header[3] |= 0xe0;
        let mut trailing = good.clone();
        trailing.extend_from_slice(b"junk");
        let mut second = good.clone();
        second.extend_from_slice(&crc);
        for (index, bad) in
            [crc, size, good[..good.len() - 1].to_vec(), header, trailing, second, vec![0x1f, 0x8b]].iter().enumerate()
        {
            let source = fixture.source(&format!("bad-{index}"), bad);
            assert!(matches!(
                prepare_file(&source, CompressionMode::Auto, &fixture.0, &cancel()),
                Err(CompressionError::InvalidGzip(_))
            ));
            assert_eq!(fixture.backing_count(), 0, "partial backing survived corrupt stream {index}");
            assert_eq!(fs::read(&source).unwrap(), *bad);
        }
    }

    #[test]
    fn cancellation_before_open_and_after_partial_decode_removes_backing() {
        let fixture = Fixture::new();
        let cancelled = AtomicBool::new(true);
        assert!(matches!(
            prepare_file(&fixture.0.join("absent"), CompressionMode::Auto, &fixture.0, &cancelled),
            Err(CompressionError::Cancelled)
        ));
        let cancel = cancel();
        let partial_seen = Cell::new(false);
        struct CancelAtEof<'a> {
            bytes: Cursor<Vec<u8>>,
            cancel: &'a AtomicBool,
            partial_seen: &'a Cell<bool>,
            parent: &'a Path,
        }
        impl Read for CancelAtEof<'_> {
            fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
                assert!(into.len() <= 64 * 1024);
                let count = self.bytes.read(into)?;
                if count == 0 {
                    let entry = fs::read_dir(self.parent)?.next().unwrap()?;
                    self.partial_seen.set(fs::metadata(entry.path().join("payload"))?.len() > 0);
                    self.cancel.store(true, Ordering::Relaxed);
                }
                Ok(count)
            }
        }
        let mut reader = CancelAtEof {
            bytes: Cursor::new(gzip(b"already decoded")),
            cancel: &cancel,
            partial_seen: &partial_seen,
            parent: &fixture.0,
        };
        assert!(matches!(
            prepare_gzip(Path::new("synthetic"), &mut reader, &fixture.0, &cancel),
            Err(CompressionError::Cancelled)
        ));
        assert!(partial_seen.get(), "cancellation must exercise cleanup after output was written");
        assert_eq!(fixture.backing_count(), 0);
    }

    #[test]
    fn empty_payload_is_valid() {
        let fixture = Fixture::new();
        let source = fixture.source("empty", &gzip(b""));
        let prepared = prepare_file(&source, CompressionMode::Auto, &fixture.0, &cancel()).unwrap();
        assert!(prepared.text_file().unwrap().is_empty());
        assert!(prepared.try_finish().is_ok());
    }

    #[test]
    fn retirement_refuses_live_mapping_and_snapshot_then_closes_before_unlink() {
        let fixture = Fixture::new();
        let source = fixture.source("source", &gzip(b"retained snapshot\xff"));
        let prepared = prepare_file(&source, CompressionMode::Auto, &fixture.0, &cancel()).unwrap();
        let backing = prepared.backing_path().unwrap().to_path_buf();
        let clone = prepared.text_file().unwrap().clone();
        let buffer = Buffer::from_file(clone.clone()).unwrap();
        let snapshot = buffer.snapshot();
        drop(buffer);
        let prepared = prepared.try_finish().unwrap_err();
        assert!(backing.exists());
        assert!(prepared.cleanup_error().is_none());
        drop(clone);
        let prepared = prepared.try_finish().unwrap_err();
        let mut bytes = Vec::new();
        snapshot.write_to(&mut bytes).unwrap();
        assert_eq!(bytes, b"retained snapshot\xff");
        drop(snapshot);
        assert!(prepared.try_finish().is_ok());
        assert!(!backing.exists());
    }

    #[test]
    fn cleanup_failure_returns_owner_for_retry_without_recursing() {
        let fixture = Fixture::new();
        let source = fixture.source("source", &gzip(b"text"));
        let prepared = prepare_file(&source, CompressionMode::Auto, &fixture.0, &cancel()).unwrap();
        let extra = prepared.backing_path().unwrap().parent().unwrap().join("do-not-delete");
        File::create_new(&extra).unwrap();
        let prepared = prepared.try_finish().unwrap_err();
        assert!(prepared.cleanup_error().is_some());
        assert!(prepared.text_file().is_none());
        assert!(extra.exists());
        fs::remove_file(extra).unwrap();
        assert!(prepared.try_finish().is_ok());
    }

    #[test]
    fn drop_does_not_unlink_on_the_dropping_thread() {
        let fixture = Fixture::new();
        let source = fixture.source("source", &gzip(b"recoverable artifact"));
        let prepared = prepare_file(&source, CompressionMode::Auto, &fixture.0, &cancel()).unwrap();
        let backing = prepared.backing_path().unwrap().to_path_buf();
        let directory = backing.parent().unwrap().to_path_buf();
        drop(prepared);
        assert!(backing.exists());
        fs::remove_file(backing).unwrap();
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn snapshot_recompression_round_trips_exact_raw_bytes_after_edits() {
        let mut buffer = Buffer::empty();
        buffer.insert(0, b"\xff\xfe\0\r\ninvalid\xc0\xaf").unwrap();
        buffer.insert(4, b"edited\x80").unwrap();
        let snapshot = buffer.snapshot();
        let mut expected = Vec::new();
        snapshot.write_to(&mut expected).unwrap();
        let mut encoded = Vec::new();
        assert_eq!(recompress_snapshot(&snapshot, &mut encoded, &cancel()).unwrap(), expected.len() as u64);
        assert_eq!(decoded(&encoded), expected);
    }

    #[test]
    fn reader_recompression_has_bounded_io_and_exact_bytes() {
        struct BoundedReader(Cursor<Vec<u8>>);
        impl Read for BoundedReader {
            fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
                assert!(into.len() <= 64 * 1024);
                self.0.read(into)
            }
        }
        struct BoundedWriter(Vec<u8>);
        impl Write for BoundedWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                assert!(bytes.len() <= 64 * 1024);
                self.0.write(bytes)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let bytes: Vec<u8> = (0..STREAM_CHUNK + 17).map(|i| (i % 251) as u8).collect();
        let mut reader = BoundedReader(Cursor::new(bytes.clone()));
        let mut writer = BoundedWriter(Vec::new());
        assert_eq!(recompress_reader(&mut reader, &mut writer, &cancel()).unwrap(), bytes.len() as u64);
        assert_eq!(decoded(&writer.0), bytes);
        let mut output = BoundedWriter(Vec::new());
        let mut input = BoundedReader(Cursor::new(writer.0));
        decode_gzip(&mut input, &mut output, &cancel()).unwrap();
        assert_eq!(output.0, bytes);
    }

    #[test]
    fn decode_write_failure_propagates_and_partial_owner_cleans_up() {
        struct NoSpace;
        impl Write for NoSpace {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("injected storage full"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let fixture = Fixture::new();
        let mut scratch = WorkerScratch::create(&fixture.0).unwrap();
        scratch.file.as_mut().unwrap().write_all(b"partial").unwrap();
        let error = decode_gzip(&mut &gzip(b"bytes")[..], &mut NoSpace, &cancel()).unwrap_err();
        assert!(error.to_string().contains("injected storage full"));
        assert!(matches!(error, CompressionError::Io(_)));
        assert!(matches!(scratch.abort(error), CompressionError::Io(_)));
        assert_eq!(fixture.backing_count(), 0);
    }

    #[test]
    fn partial_cleanup_error_reports_retained_path_and_original_cancellation() {
        let fixture = Fixture::new();
        let mut scratch = WorkerScratch::create(&fixture.0).unwrap();
        scratch.file.as_mut().unwrap().write_all(b"partial").unwrap();
        let directory = scratch.backing.as_ref().unwrap().directory.clone();
        let blocker = directory.join("keep");
        File::create_new(&blocker).unwrap();
        let error = scratch.abort(CompressionError::Cancelled);
        match error {
            CompressionError::Cleanup { path, cause, .. } => {
                assert_eq!(path, directory);
                assert!(matches!(*cause, CompressionError::Cancelled));
            }
            other => panic!("cleanup error was hidden: {other}"),
        }
        assert!(!directory.join("payload").exists());
        assert!(blocker.exists());
        fs::remove_file(blocker).unwrap();
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn recompression_cancellation_does_not_finish_a_truncated_member() {
        let cancel = cancel();
        struct CancelAfterChunk<'a>(&'a AtomicBool, bool);
        impl Read for CancelAfterChunk<'_> {
            fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
                if !self.1 {
                    self.1 = true;
                    into[..5].copy_from_slice(b"first");
                    Ok(5)
                } else {
                    self.0.store(true, Ordering::Relaxed);
                    Ok(0)
                }
            }
        }
        let mut encoded = Vec::new();
        assert!(matches!(
            recompress_reader(&mut CancelAfterChunk(&cancel, false), &mut encoded, &cancel),
            Err(CompressionError::Cancelled)
        ));
        let mut scratch = Vec::new();
        assert!(MultiGzDecoder::new(&encoded[..]).read_to_end(&mut scratch).is_err());
    }

    #[test]
    fn source_and_writer_errors_propagate_without_drop_writes() {
        struct FailingSource(bool);
        impl Read for FailingSource {
            fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
                if self.0 {
                    return Err(io::Error::other("injected source error"));
                }
                self.0 = true;
                into[0] = b'x';
                Ok(1)
            }
        }
        let mut encoded = Vec::new();
        let error = recompress_reader(&mut FailingSource(false), &mut encoded, &cancel()).unwrap_err();
        assert!(error.to_string().contains("injected source error"));
        assert!(MultiGzDecoder::new(&encoded[..]).read_to_end(&mut Vec::new()).is_err());
        let error = decode_gzip(&mut FailingSource(true), &mut Vec::new(), &cancel()).unwrap_err();
        assert!(matches!(error, CompressionError::Io(_)));
        assert!(error.to_string().contains("injected source error"));
        struct FailingWriter {
            calls: usize,
            failed: bool,
        }
        impl Write for FailingWriter {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                assert!(!self.failed, "encoder Drop retried output after an error");
                self.calls += 1;
                self.failed = true;
                Err(io::Error::new(io::ErrorKind::WriteZero, "injected writer error"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut writer = FailingWriter { calls: 0, failed: false };
        assert!(recompress_reader(&mut &b"bytes"[..], &mut writer, &cancel()).is_err());
        assert_eq!(writer.calls, 1);
    }

    #[test]
    fn recompression_footer_error_is_not_success_or_retried_by_drop() {
        struct FooterFailure {
            remaining: usize,
            failed: bool,
        }
        impl Write for FooterFailure {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                assert!(!self.failed, "footer write was retried by Drop");
                if self.remaining == 0 {
                    self.failed = true;
                    return Err(io::Error::other("injected footer error"));
                }
                let count = self.remaining.min(bytes.len());
                self.remaining -= count;
                Ok(count)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let bytes = b"finish the trailer";
        let mut writer = FooterFailure { remaining: gzip(bytes).len() - 8, failed: false };
        let error = recompress_reader(&mut &bytes[..], &mut writer, &cancel()).unwrap_err();
        assert!(error.to_string().contains("injected footer error"));
        assert!(writer.failed);
    }

    #[cfg(unix)]
    #[test]
    fn private_backing_has_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        let source = fixture.source("source", &gzip(b"private"));
        let prepared = prepare_file(&source, CompressionMode::Auto, &fixture.0, &cancel()).unwrap();
        let path = prepared.backing_path().unwrap();
        assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        assert!(prepared.try_finish().is_ok());
    }
}

#[cfg(test)]
mod payload_tests {
    use super::*;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "kwil-gzip-payload-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            Self(root)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut output = Vec::new();
        recompress_payload(&mut output, &AtomicBool::new(false), |writer| writer.write_all(bytes)).unwrap();
        output
    }

    #[test]
    fn notices_distinguish_automatic_detection_and_explicit_overrides() {
        let f = Fixture::new();
        let path = f.0.join("input");
        fs::write(&path, gzip(b"plain")).unwrap();
        for mode in [CompressionMode::Auto, CompressionMode::Gzip, CompressionMode::Raw] {
            let owner = prepare_file(&path, mode, &f.0, &AtomicBool::new(false)).unwrap();
            assert_eq!(owner.mode(), mode);
            assert_eq!(owner.notice(), Some(CompressionNotice { mode, format: owner.format() }));
            assert!(owner.try_finish().is_ok());
        }
        fs::write(&path, b"plain").unwrap();
        let owner = prepare_file(&path, CompressionMode::Auto, &f.0, &AtomicBool::new(false)).unwrap();
        assert_eq!(owner.mode(), CompressionMode::Auto);
        assert_eq!(owner.notice(), None);
        assert!(owner.try_finish().is_ok());
    }

    #[test]
    fn payload_preparation_uses_destination_identity_and_validates_all_members() {
        let f = Fixture::new();
        let payload = [gzip(b"first"), gzip(b""), gzip(b"\xfflast")].concat();
        let identity = f.0.join("original.gz");
        let owner = prepare_gzip_payload(&identity, &mut &payload[..], &f.0, &AtomicBool::new(false)).unwrap();
        assert_eq!(owner.source_path(), identity);
        assert_eq!(owner.text_file().unwrap().bytes().unwrap(), b"first\xfflast");
        assert_eq!(owner.text_file().unwrap().resident_original_bytes(), 0);
        assert!(owner.try_finish().is_ok());
        assert!(!identity.exists(), "decompression must never create or overwrite the original");
    }

    #[test]
    fn payload_callback_failure_keeps_typed_cause_and_never_seals_truncated_input() {
        #[derive(Debug)]
        struct FeedFailure;
        impl std::fmt::Display for FeedFailure {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("typed feed failure")
            }
        }
        impl std::error::Error for FeedFailure {}
        let mut output = Vec::new();
        let error = recompress_payload(&mut output, &AtomicBool::new(false), |writer| {
            writer.write_all(b"partial")?;
            Err(io::Error::other(FeedFailure))
        })
        .unwrap_err()
        .into_io_error();
        assert!(error.get_ref().unwrap().is::<FeedFailure>());
        assert!(MultiGzDecoder::new(&output[..]).read_to_end(&mut Vec::new()).is_err());
    }

    #[test]
    fn payload_cancellation_and_footer_failure_never_retry_output_in_drop() {
        let cancel = AtomicBool::new(false);
        let mut output = Vec::new();
        let error = recompress_payload(&mut output, &cancel, |writer| {
            writer.write_all(b"partial")?;
            cancel.store(true, Ordering::Relaxed);
            Ok(())
        })
        .unwrap_err()
        .into_io_error();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(MultiGzDecoder::new(&output[..]).read_to_end(&mut Vec::new()).is_err());
        struct FooterFailure {
            left: usize,
            failed: bool,
        }
        impl Write for FooterFailure {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                assert!(!self.failed, "Drop retried failed output");
                if self.left == 0 {
                    self.failed = true;
                    return Err(io::Error::other("payload footer failure"));
                }
                let count = bytes.len().min(self.left);
                self.left -= count;
                Ok(count)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut writer = FooterFailure { left: gzip(b"trailer").len() - 8, failed: false };
        let error = recompress_payload(&mut writer, &AtomicBool::new(false), |writer| writer.write_all(b"trailer"))
            .unwrap_err();
        assert!(writer.failed);
        assert!(error.to_string().contains("payload footer failure"));
    }

    #[test]
    fn payload_cleanup_error_retains_typed_owner_and_original_cancellation() {
        let f = Fixture::new();
        let scratch = WorkerScratch::create(&f.0).unwrap();
        let directory = scratch.backing.as_ref().unwrap().directory.clone();
        let blocker = directory.join("keep");
        fs::write(&blocker, b"keep").unwrap();
        let error = scratch.abort(CompressionError::Cancelled).into_io_error();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        let owner = PreparedFile::take_from(&error).expect("cleanup owner must survive I/O context");
        assert!(PreparedFile::take_from(&error).is_none());
        assert!(owner.text_file().is_none());
        assert!(owner.cleanup_error().is_some());
        let owner = match owner.try_finish() {
            Err(owner) => owner,
            Ok(()) => panic!("blocker must retain cleanup owner"),
        };
        fs::remove_file(&blocker).unwrap();
        assert!(owner.try_finish().is_ok());
        assert!(!directory.exists());
    }
}
