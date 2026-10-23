//! Worker-prepared, lossless decoded originals. This is a storage primitive,
//! not an opening pipeline: publish the first viewport **before** preparing it.
//!
//! Logical bytes are UTF-8 except for one `0xff` per opaque malformed atom.
//! Consumers must use `atom_at` to display that atom's ORIGINAL raw bytes, not
//! display the sentinel as `\\xFF`. Literal U+FFFD and backslash escapes are text.
//! A legacy sequence may decode to two scalars (Big5); its interior is reported
//! explicitly, so a piece splitter can materialize the partial text rather than
//! invent a raw offset. An opaque atom must be edited as a whole.
//!
//! All filesystem work, cold map access and coordinate replay belong on a worker.
//! No threads are launched here. Scratch is fixed-size; the raw snapshot, logical
//! bytes and sparse index are private mapped files, never file-sized Vecs. Index
//! records live on disk even for adversarial malformed input. Endpoint replay
//! touches at most CHECKPOINT_RAW_BYTES raw bytes after binary-searching the index.
//!
//! Snapshots/range pins retain their own immutable origin across save generations
//! and undo. Explicit worker retirement refuses live snapshots. Dropping an owner
//! does NOT unlink files: retain it in the worker's retirement queue and retry.
//! A crash journal must durably copy/adopt all three private files and descriptor
//! before acknowledging recovery; merely serializing a pathname is insufficient.

use crate::encoding::{Encoding, EncodingError};
use crate::{FileState, ReadError, TextFile};
use encoding_rs::DecoderResult;
#[path = "decoded_preview.rs"]
mod preview;
use memmap2::Mmap;
pub use preview::{DecodedPreview, decode_preview};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Maximum raw distance between complete-atom checkpoints, including alignment.
pub const CHECKPOINT_RAW_BYTES: usize = 64 << 10;
const SCRATCH: usize = 16 << 10;
const RECORD: usize = 16;
const OPAQUE: u8 = 0xff;
static NEXT: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub enum BackingError {
    Io(io::Error),
    Read(ReadError),
    Encoding(EncodingError),
    Cancelled,
    InvalidDescriptor,
    InvalidRange,
    /// Endpoint lies inside a scalar, multi-scalar alias or opaque atom.
    SplitAtom(Boundary),
    DecoderContract,
    /// Preparation failed and cleanup could not complete. Preserve/report this
    /// directory; it contains only this preparation's private backing files.
    Cleanup {
        cause: Box<BackingError>,
        directory: PathBuf,
        error: io::Error,
    },
}
impl std::fmt::Display for BackingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => e.fmt(f),
            Self::Read(e) => e.fmt(f),
            Self::Encoding(e) => e.fmt(f),
            Self::Cancelled => f.write_str("decoded backing preparation cancelled"),
            Self::InvalidDescriptor => f.write_str("encoding/BOM descriptor does not match raw origin"),
            Self::InvalidRange => f.write_str("range outside decoded origin"),
            Self::SplitAtom(b) => write!(f, "range splits encoded atom: {b:?}"),
            Self::DecoderContract => f.write_str("stateless decoder violated bounded atom contract"),
            Self::Cleanup { cause, directory, error } => {
                write!(f, "{cause}; private cleanup failed at {}: {error}", directory.display())
            }
        }
    }
}
impl std::error::Error for BackingError {}
impl From<io::Error> for BackingError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<ReadError> for BackingError {
    fn from(e: ReadError) -> Self {
        Self::Read(e)
    }
}
impl From<EncodingError> for BackingError {
    fn from(e: EncodingError) -> Self {
        Self::Encoding(e)
    }
}

/// Interpretation belongs to this immutable origin, not to the current save
/// target. BOM bytes are excluded from logical coordinates but retained raw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Interpretation {
    pub encoding: Encoding,
    pub bom_len: u8,
}
impl Interpretation {
    pub fn new(encoding: Encoding, bom_len: usize) -> Result<Self, BackingError> {
        let expected = bom(encoding);
        if bom_len != 0 && (expected.is_empty() || bom_len != expected.len()) {
            return Err(BackingError::InvalidDescriptor);
        }
        Ok(Self { encoding, bom_len: bom_len as u8 })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AtomKind {
    Text,
    Opaque,
}

/// An exact point or the full atom containing an inexact point. This exposes
/// both coordinate intervals, including Big5 expansions and malformed spans.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Boundary {
    Exact { logical: u64, raw: u64 },
    Interior { logical: Range<u64>, raw: Range<u64>, kind: AtomKind },
    Bom { raw: Range<u64> },
}

#[derive(Debug)]
pub struct Atom<'a> {
    pub logical: Range<u64>,
    pub raw: Range<u64>,
    /// Always the original bytes, including malformed sequences/legacy aliases.
    pub raw_bytes: &'a [u8],
    pub content: AtomContent<'a>,
}
#[derive(Debug, PartialEq, Eq)]
pub enum AtomContent<'a> {
    Text(&'a str),
    Opaque,
}

/// Private paths are exposed for journal adoption, never as a writable source.
/// These three files and their directory are owned exclusively by this origin.
#[derive(Clone, Debug)]
pub struct BackingPaths {
    pub directory: PathBuf,
    pub raw: PathBuf,
    pub logical: PathBuf,
    pub checkpoints: PathBuf,
}
impl BackingPaths {
    fn create(parent: &Path) -> io::Result<Self> {
        loop {
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let directory = parent.join(format!("kwil-decoded-{}-{id}", std::process::id()));
            match fs::create_dir(&directory) {
                Ok(()) => {
                    return Ok(Self {
                        raw: directory.join("raw"),
                        logical: directory.join("logical"),
                        checkpoints: directory.join("checkpoints"),
                        directory,
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
    }
    /// Worker only, after all maps/handles have closed. Also usable to retry a
    /// reported cleanup failure. No recursive deletion or input-path removal.
    pub fn cleanup(&self) -> io::Result<()> {
        let mut failure = None;
        for path in [&self.raw, &self.logical, &self.checkpoints] {
            if let Err(e) = fs::remove_file(path)
                && e.kind() != io::ErrorKind::NotFound
                && failure.is_none()
            {
                failure = Some(e);
            }
        }
        if let Some(e) = failure {
            return Err(e);
        }
        match fs::remove_dir(&self.directory) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

struct Storage {
    raw: Option<Mmap>,
    logical: Option<Mmap>,
    checkpoints: Mmap,
    interpretation: Interpretation,
    source_path: PathBuf,
    paths: BackingPaths,
}

/// Keep this owner on a worker retirement queue. Snapshots perform no unlink
/// on drop, even when dropped on a UI thread.
#[must_use = "retain the backing owner and explicitly retire it on a worker"]
pub struct DecodedBacking {
    storage: Arc<Storage>,
}
#[derive(Clone)]
pub struct DecodedSnapshot {
    storage: Arc<Storage>,
}

pub enum RetirementError {
    LiveSnapshots(DecodedBacking),
    Cleanup { paths: Box<BackingPaths>, error: io::Error },
}

impl DecodedBacking {
    /// Worker only. `private_parent` must already exist and be app-private.
    /// Input is streamed into a private raw snapshot even when TextFile mapped
    /// it; externally mutable pages never become the immutable origin.
    /// Source checks remain enabled and run before/after the bounded copy.
    /// An explicit interpretation can recover an automatic UTF-32 sniff error,
    /// matching EncodedFile::new_with_encoding. Encoding itself rejects UTF-32
    /// and ISO-2022-JP labels; this API adds no unsupported decoder.
    pub fn prepare(
        source: &TextFile,
        interpretation: Interpretation,
        private_parent: &Path,
        cancel: &AtomicBool,
    ) -> Result<Self, BackingError> {
        cancelled(cancel)?;
        Interpretation::new(interpretation.encoding, interpretation.bom_len as usize)?;
        let paths = BackingPaths::create(private_parent)?;
        let result = Self::prepare_private(source, interpretation, &paths, cancel);
        match result {
            Ok(storage) => Ok(Self { storage: Arc::new(storage) }),
            Err(cause) => match paths.cleanup() {
                Ok(()) => Err(cause),
                Err(error) => Err(BackingError::Cleanup { cause: Box::new(cause), directory: paths.directory, error }),
            },
        }
    }

    /// Worker-only reconstruction from a framed, validated recovery payload.
    /// Reads exactly `raw_len` bytes, leaving subsequent frame bytes untouched.
    /// The caller owns frame authentication, payload limits and final EOF checks;
    /// this method never trusts an external pathname or retains the reader.
    /// Scratch and cancellation intervals match ordinary source preparation.
    pub fn prepare_from_reader(
        input: &mut impl Read,
        raw_len: u64,
        interpretation: Interpretation,
        private_parent: &Path,
        cancel: &AtomicBool,
    ) -> Result<Self, BackingError> {
        cancelled(cancel)?;
        Interpretation::new(interpretation.encoding, interpretation.bom_len as usize)?;
        let paths = BackingPaths::create(private_parent)?;
        let result = (|| {
            let mut raw_file = private_file(&paths.raw)?;
            copy_exact(input, &mut raw_file, raw_len, cancel)?;
            Self::finish_private(raw_file, interpretation, &paths, paths.raw.clone(), cancel)
        })();
        match result {
            Ok(storage) => Ok(Self { storage: Arc::new(storage) }),
            Err(cause) => match paths.cleanup() {
                Ok(()) => Err(cause),
                Err(error) => Err(BackingError::Cleanup { cause: Box::new(cause), directory: paths.directory, error }),
            },
        }
    }

    fn prepare_private(
        source: &TextFile,
        interpretation: Interpretation,
        paths: &BackingPaths,
        cancel: &AtomicBool,
    ) -> Result<Storage, BackingError> {
        check_source(source)?;
        let mut input = File::open(source.path())?;
        // Opening a fresh handle is followed by another identity check; the
        // copied length is fixed. No mmap dereference of the external source.
        check_source(source)?;
        let before = input.metadata()?;
        if before.len() != source.len() {
            return Err(BackingError::InvalidDescriptor);
        }
        let mut raw_file = private_file(&paths.raw)?;
        copy_exact(&mut input, &mut raw_file, source.len(), cancel)?;
        let after = input.metadata()?;
        if before.len() != after.len() || before.modified()? != after.modified()? {
            return Err(ReadError::Changed(FileState::Modified).into());
        }
        check_source(source)?;
        Self::finish_private(raw_file, interpretation, paths, source.path().to_owned(), cancel)
    }

    fn finish_private(
        mut raw_file: File,
        interpretation: Interpretation,
        paths: &BackingPaths,
        source_path: PathBuf,
        cancel: &AtomicBool,
    ) -> Result<Storage, BackingError> {
        cancelled(cancel)?;
        raw_file.flush()?;
        let raw = map(&raw_file)?;
        let raw_bytes = mapped(&raw);
        let skip = interpretation.bom_len as usize;
        if skip > raw_bytes.len() || (skip != 0 && !raw_bytes.starts_with(bom(interpretation.encoding))) {
            return Err(BackingError::InvalidDescriptor);
        }
        let mut logical_file = private_file(&paths.logical)?;
        let mut index_file = private_file(&paths.checkpoints)?;
        {
            let mut logical = BufWriter::with_capacity(SCRATCH, &mut logical_file);
            let mut index = BufWriter::with_capacity(SCRATCH, &mut index_file);
            build(raw_bytes, interpretation, &mut logical, &mut index, cancel)?;
            logical.flush()?;
            index.flush()?;
        }
        cancelled(cancel)?;
        Ok(Storage {
            logical: map(&logical_file)?,
            checkpoints: map(&index_file)?.ok_or(BackingError::DecoderContract)?,
            raw,
            interpretation,
            source_path,
            paths: paths.clone(),
        })
    }

    pub fn snapshot(&self) -> DecodedSnapshot {
        DecodedSnapshot { storage: self.storage.clone() }
    }

    /// No filesystem operation occurs while a snapshot/range pin is live.
    /// Retry LiveSnapshots later on a worker. Cleanup errors include retry paths.
    pub fn try_retire(self) -> Result<(), RetirementError> {
        match Arc::try_unwrap(self.storage) {
            Err(storage) => Err(RetirementError::LiveSnapshots(Self { storage })),
            Ok(storage) => {
                let paths = storage.paths.clone();
                drop(storage); // close ALL maps first (required on Windows)
                paths.cleanup().map_err(|error| RetirementError::Cleanup { paths: Box::new(paths), error })
            }
        }
    }
}

impl DecodedSnapshot {
    /// Mapped private bytes; no metadata checks, opens, or allocation. Cold page
    /// access still belongs on a worker. Do not pass this as globally valid str.
    pub fn logical_bytes(&self) -> &[u8] {
        mapped(&self.storage.logical)
    }
    pub fn raw_bytes(&self) -> &[u8] {
        mapped(&self.storage.raw)
    }
    pub fn interpretation(&self) -> Interpretation {
        self.storage.interpretation
    }
    pub fn source_path(&self) -> &Path {
        &self.storage.source_path
    }
    pub fn paths(&self) -> &BackingPaths {
        &self.storage.paths
    }
    pub fn same_origin(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.storage, &other.storage)
    }
    pub fn checkpoint_count(&self) -> usize {
        self.storage.checkpoints.len() / RECORD
    }

    fn checkpoint(&self, i: usize) -> (u64, u64) {
        let record = &self.storage.checkpoints[i * RECORD..(i + 1) * RECORD];
        (u64::from_le_bytes(record[..8].try_into().unwrap()), u64::from_le_bytes(record[8..].try_into().unwrap()))
    }
    fn floor(&self, position: u64, logical: bool) -> usize {
        let mut low = 0;
        let mut high = self.checkpoint_count();
        while low < high {
            let mid = low + (high - low) / 2;
            let (r, l) = self.checkpoint(mid);
            if (if logical { l } else { r }) <= position {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        low.saturating_sub(1)
    }

    /// Worker endpoint resolution: binary search + <= one raw interval replay.
    pub fn resolve_logical(&self, position: u64) -> Result<Boundary, BackingError> {
        self.resolve(position, true)
    }
    pub fn resolve_raw(&self, position: u64) -> Result<Boundary, BackingError> {
        self.resolve(position, false)
    }
    fn resolve(&self, position: u64, logical: bool) -> Result<Boundary, BackingError> {
        let raw_bytes = self.raw_bytes();
        if position > if logical { self.logical_bytes().len() as u64 } else { raw_bytes.len() as u64 } {
            return Err(BackingError::InvalidRange);
        }
        let skip = self.interpretation().bom_len as u64;
        if !logical && position < skip {
            return Ok(Boundary::Bom { raw: 0..skip });
        }
        let i = self.floor(position, logical);
        let (mut r, mut l) = self.checkpoint(i);
        let end = if i + 1 < self.checkpoint_count() { self.checkpoint(i + 1).0 } else { r };
        loop {
            if position == if logical { l } else { r } {
                return Ok(Boundary::Exact { logical: l, raw: r });
            }
            if r >= end {
                return Err(BackingError::DecoderContract);
            }
            let atom = next_atom(self.interpretation().encoding, &raw_bytes[r as usize..end as usize])?;
            let next_r = r + atom.raw as u64;
            let next_l = l + atom.logical as u64;
            if position < if logical { next_l } else { next_r } {
                return Ok(Boundary::Interior { logical: l..next_l, raw: r..next_r, kind: atom.kind });
            }
            r = next_r;
            l = next_l;
        }
    }

    /// Returns the containing atom, including its exact malformed raw bytes.
    /// EOF has no atom. A point at an atom boundary selects the following atom.
    pub fn atom_at(&self, position: u64) -> Result<Option<Atom<'_>>, BackingError> {
        let boundary = self.resolve_logical(position)?;
        let (logical, raw, kind) = match boundary {
            Boundary::Interior { logical, raw, kind } => (logical, raw, kind),
            Boundary::Exact { logical, raw } => {
                if logical == self.logical_bytes().len() as u64 {
                    return Ok(None);
                }
                let atom = next_atom(self.interpretation().encoding, &self.raw_bytes()[raw as usize..])?;
                (logical..logical + atom.logical as u64, raw..raw + atom.raw as u64, atom.kind)
            }
            Boundary::Bom { .. } => return Err(BackingError::DecoderContract),
        };
        Ok(Some(self.atom(logical, raw, kind)?))
    }

    fn atom(&self, logical: Range<u64>, raw: Range<u64>, kind: AtomKind) -> Result<Atom<'_>, BackingError> {
        let content = match kind {
            AtomKind::Text => AtomContent::Text(
                std::str::from_utf8(&self.logical_bytes()[logical.start as usize..logical.end as usize])
                    .map_err(|_| BackingError::DecoderContract)?,
            ),
            AtomKind::Opaque => AtomContent::Opaque,
        };
        Ok(Atom { raw_bytes: &self.raw_bytes()[raw.start as usize..raw.end as usize], logical, raw, content })
    }

    /// Stream atoms with one initial checkpoint replay, then linear work in
    /// the requested range. Retains no per-character collection.
    pub fn atoms(&self, logical: Range<u64>) -> Result<Atoms<'_>, BackingError> {
        let raw = self.raw_range(logical.clone())?;
        Ok(Atoms { origin: self, logical: logical.start, raw: raw.start, end: raw.end })
    }

    /// Validates BOTH endpoints before any output. Interior endpoints return
    /// the atom's full intervals; the caller must materialize partial text.
    pub fn raw_range(&self, logical: Range<u64>) -> Result<Range<u64>, BackingError> {
        if logical.start > logical.end {
            return Err(BackingError::InvalidRange);
        }
        let exact = |b: Boundary| match b {
            Boundary::Exact { raw, .. } => Ok(raw),
            other => Err(BackingError::SplitAtom(other)),
        };
        Ok(exact(self.resolve_logical(logical.start)?)?..exact(self.resolve_logical(logical.end)?)?)
    }

    /// Pin provenance alongside a future piece/undo root. Multiple generations
    /// have distinct Arc identities even when they share the same source path.
    pub fn pin_range(&self, logical: Range<u64>) -> Result<OriginRange, BackingError> {
        let raw = self.raw_range(logical.clone())?;
        Ok(OriginRange { origin: self.clone(), logical, raw })
    }

    /// Same-encoding raw copy: preserves aliases and malformed bytes exactly.
    /// BOM is separate: write raw_bytes()[..bom_len] once at the document start.
    pub fn write_raw_range(
        &self,
        logical: Range<u64>,
        writer: &mut impl Write,
        cancel: &AtomicBool,
    ) -> Result<(), BackingError> {
        let range = self.raw_range(logical)?;
        write_chunks(&self.raw_bytes()[range.start as usize..range.end as usize], writer, cancel)
    }
}

pub struct Atoms<'a> {
    origin: &'a DecodedSnapshot,
    logical: u64,
    raw: u64,
    end: u64,
}
impl<'a> Iterator for Atoms<'a> {
    type Item = Result<Atom<'a>, BackingError>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.raw == self.end {
            return None;
        }
        let size = match next_atom(
            self.origin.interpretation().encoding,
            &self.origin.raw_bytes()[self.raw as usize..self.end as usize],
        ) {
            Ok(size) => size,
            Err(e) => {
                self.raw = self.end;
                return Some(Err(e));
            }
        };
        let logical = self.logical..self.logical + size.logical as u64;
        let raw = self.raw..self.raw + size.raw as u64;
        self.logical = logical.end;
        self.raw = raw.end;
        Some(self.origin.atom(logical, raw, size.kind))
    }
}

#[derive(Clone)]
pub struct OriginRange {
    origin: DecodedSnapshot,
    logical: Range<u64>,
    raw: Range<u64>,
}
impl OriginRange {
    pub fn origin(&self) -> &DecodedSnapshot {
        &self.origin
    }
    pub fn logical_range(&self) -> Range<u64> {
        self.logical.clone()
    }
    pub fn raw_range(&self) -> Range<u64> {
        self.raw.clone()
    }
    pub fn raw_bytes(&self) -> &[u8] {
        &self.origin.raw_bytes()[self.raw.start as usize..self.raw.end as usize]
    }
}

fn cancelled(cancel: &AtomicBool) -> Result<(), BackingError> {
    if cancel.load(Ordering::Relaxed) { Err(BackingError::Cancelled) } else { Ok(()) }
}
fn private_file(path: &Path) -> io::Result<File> {
    OpenOptions::new().read(true).write(true).create_new(true).open(path)
}
fn map(file: &File) -> io::Result<Option<Mmap>> {
    if file.metadata()?.len() == 0 {
        return Ok(None);
    }
    // SAFETY: the file is our exclusively created private snapshot; handles
    // close after construction and this module never mutates published files.
    unsafe { Mmap::map(file).map(Some) }
}
fn mapped(map: &Option<Mmap>) -> &[u8] {
    map.as_deref().unwrap_or(&[])
}
fn check_source(source: &TextFile) -> Result<(), BackingError> {
    match source.check()? {
        FileState::Unchanged => Ok(()),
        other => Err(ReadError::Changed(other).into()),
    }
}
fn copy_exact(
    input: &mut impl Read,
    output: &mut impl Write,
    mut len: u64,
    cancel: &AtomicBool,
) -> Result<(), BackingError> {
    let mut scratch = [0; SCRATCH];
    while len != 0 {
        cancelled(cancel)?;
        let n = len.min(SCRATCH as u64) as usize;
        input.read_exact(&mut scratch[..n])?;
        output.write_all(&scratch[..n])?;
        len -= n as u64;
    }
    Ok(())
}
fn write_chunks(bytes: &[u8], writer: &mut impl Write, cancel: &AtomicBool) -> Result<(), BackingError> {
    cancelled(cancel)?;
    for chunk in bytes.chunks(SCRATCH) {
        cancelled(cancel)?;
        writer.write_all(chunk)?;
    }
    Ok(())
}
fn bom(encoding: Encoding) -> &'static [u8] {
    if encoding == Encoding::UTF8 {
        b"\xef\xbb\xbf"
    } else if encoding == Encoding::UTF16_LE {
        b"\xff\xfe"
    } else if encoding == Encoding::UTF16_BE {
        b"\xfe\xff"
    } else {
        b""
    }
}
fn codec(encoding: Encoding) -> &'static encoding_rs::Encoding {
    // Encoding admits precisely the stateless, non-replacement encodings.
    encoding_rs::Encoding::for_label_no_replacement(encoding.name().as_bytes()).unwrap()
}

/// Find a complete-atom endpoint with <=64KiB raw distance. A pending decoder
/// need not ever become neutral on malformed runs: each new lead can replace
/// the previous malformed lead and itself become pending. Never extend a cut
/// looking for eventual neutrality, or finalize an arbitrary cut as EOF.
///
/// Bulk decoding records exact restart points at neutral states and the end of
/// each malformed span (excluding consumed lookahead). If the limit is pending,
/// replay whole atoms from the last such point and cut BEFORE any crossing atom.
/// Replay can inspect at most three lookahead bytes beyond the limit, but the
/// published checkpoint distance and later endpoint replay stay <=64KiB.
fn block_end(raw: &[u8], start: usize, encoding: Encoding) -> Result<usize, BackingError> {
    if raw.len() - start <= CHECKPOINT_RAW_BYTES {
        return Ok(raw.len());
    }
    let mut end = start + CHECKPOINT_RAW_BYTES;
    if encoding == Encoding::UTF16_LE || encoding == Encoding::UTF16_BE {
        end -= (end - start) % 2;
        let pair = [raw[end - 2], raw[end - 1]];
        let unit = if encoding == Encoding::UTF16_LE { u16::from_le_bytes(pair) } else { u16::from_be_bytes(pair) };
        if (0xd800..=0xdbff).contains(&unit) {
            end -= 2;
        }
        return Ok(end);
    }
    let mut decoder = codec(encoding).new_decoder_without_bom_handling();
    let mut scratch = [0; SCRATCH];
    let mut at = start;
    let mut restart = start;
    loop {
        let (result, read, _) = decoder.decode_to_utf8_without_replacement(&raw[at..end], &mut scratch, false);
        at += read;
        match result {
            DecoderResult::Malformed(_, after) => {
                // For stateless encodings this is the boundary AFTER the
                // malformed atom, even if the decoder buffered a new lead.
                restart = at - after as usize;
            }
            DecoderResult::InputEmpty | DecoderResult::OutputFull => {
                if decoder.latin1_byte_compatible_up_to(b"").is_some() {
                    restart = at;
                }
                if result == DecoderResult::InputEmpty {
                    break;
                }
            }
        }
    }
    while restart < end {
        let next = restart + next_atom(encoding, &raw[restart..])?.raw;
        if next > end {
            break;
        }
        restart = next;
    }
    Ok(restart)
}

fn checkpoint(writer: &mut impl Write, raw: usize, logical: u64) -> io::Result<()> {
    writer.write_all(&(raw as u64).to_le_bytes())?;
    writer.write_all(&logical.to_le_bytes())
}
fn build(
    raw: &[u8],
    interpretation: Interpretation,
    logical: &mut impl Write,
    index: &mut impl Write,
    cancel: &AtomicBool,
) -> Result<(), BackingError> {
    let mut at = interpretation.bom_len as usize;
    let mut length = 0;
    checkpoint(index, at, length)?;
    while at < raw.len() {
        cancelled(cancel)?;
        let end = block_end(raw, at, interpretation.encoding)?;
        length += decode_block(&raw[at..end], interpretation.encoding, logical, cancel)?;
        at = end;
        checkpoint(index, at, length)?;
    }
    Ok(())
}
fn decode_block(
    raw: &[u8],
    encoding: Encoding,
    writer: &mut impl Write,
    cancel: &AtomicBool,
) -> Result<u64, BackingError> {
    let mut decoder = codec(encoding).new_decoder_without_bom_handling();
    let mut scratch = [0; SCRATCH];
    let mut at = 0;
    let mut logical = 0;
    loop {
        cancelled(cancel)?;
        let (result, read, written) = decoder.decode_to_utf8_without_replacement(&raw[at..], &mut scratch, true);
        at += read;
        writer.write_all(&scratch[..written])?;
        logical += written as u64;
        match result {
            DecoderResult::InputEmpty => return Ok(logical),
            DecoderResult::Malformed(_, _) => {
                writer.write_all(&[OPAQUE])?;
                logical += 1;
            }
            DecoderResult::OutputFull => {}
        }
    }
}

struct AtomSize {
    raw: usize,
    logical: usize,
    kind: AtomKind,
}
/// Replay needs no per-character table. One stateless encoded atom consumes at
/// most four source bytes and emits at most eight UTF-8 bytes (Big5 expansion).
/// Discard decoder lookahead after malformed input, leaving it for the next atom.
fn next_atom(encoding: Encoding, raw: &[u8]) -> Result<AtomSize, BackingError> {
    if raw.is_empty() {
        return Err(BackingError::InvalidRange);
    }
    if encoding != Encoding::UTF16_LE && encoding != Encoding::UTF16_BE && raw[0] < 0x80 {
        return Ok(AtomSize { raw: 1, logical: 1, kind: AtomKind::Text });
    }
    let mut decoder = codec(encoding).new_decoder_without_bom_handling();
    let mut scratch = [0; 16];
    let mut consumed = 0;
    loop {
        if consumed == 4 {
            return Err(BackingError::DecoderContract);
        }
        let (result, read, written) = decoder.decode_to_utf8_without_replacement(
            &raw[consumed..consumed + 1],
            &mut scratch,
            consumed + 1 == raw.len(),
        );
        consumed += read;
        match result {
            DecoderResult::Malformed(length, after) => {
                let length = length as usize;
                if written != 0 || consumed != length + after as usize {
                    return Err(BackingError::DecoderContract);
                }
                return Ok(AtomSize { raw: length, logical: 1, kind: AtomKind::Opaque });
            }
            _ if written != 0 => return Ok(AtomSize { raw: consumed, logical: written, kind: AtomKind::Text }),
            DecoderResult::InputEmpty if consumed < raw.len() => {}
            _ => return Err(BackingError::DecoderContract),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        directory: PathBuf,
        source: PathBuf,
    }
    impl Fixture {
        fn new(bytes: &[u8]) -> Self {
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let directory = std::env::temp_dir().join(format!("kwil-decoded-test-{}-{id}", std::process::id()));
            fs::create_dir(&directory).unwrap();
            let source = directory.join("input");
            fs::write(&source, bytes).unwrap();
            Self { directory, source }
        }
        fn prepare(&self, encoding: Encoding, bom: usize) -> DecodedBacking {
            let source = TextFile::open(&self.source).unwrap();
            DecodedBacking::prepare(
                &source,
                Interpretation::new(encoding, bom).unwrap(),
                &self.directory,
                &AtomicBool::new(false),
            )
            .unwrap()
        }
        fn retire(&self, owner: DecodedBacking) {
            let paths = owner.storage.paths.clone();
            assert!(owner.try_retire().is_ok());
            assert!(!paths.directory.exists());
            assert!(self.source.exists());
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            // Also clean this fixture after an intentional mutant panics. All
            // local snapshots/owners unwind before their earlier fixture.
            if let Ok(entries) = fs::read_dir(&self.directory) {
                let prefix = format!("kwil-decoded-{}-", std::process::id());
                for entry in entries.flatten() {
                    if entry.file_name().to_string_lossy().starts_with(&prefix)
                        && entry.file_type().is_ok_and(|kind| kind.is_dir())
                    {
                        let directory = entry.path();
                        let _ = BackingPaths {
                            raw: directory.join("raw"),
                            logical: directory.join("logical"),
                            checkpoints: directory.join("checkpoints"),
                            directory,
                        }
                        .cleanup();
                    }
                }
            }
            // Exact test-owned paths only, no recursive/shared cleanup.
            let _ = fs::remove_file(&self.source);
            let _ = fs::remove_dir(&self.directory);
        }
    }

    #[test]
    fn opaque_utf8_is_distinct_from_replacement_and_literal_escape() {
        let fixture = Fixture::new(b"a\xe2\x82Z\xef\xbf\xbd\\xFF\xff");
        let owner = fixture.prepare(Encoding::UTF8, 0);
        let snapshot = owner.snapshot();
        assert_eq!(snapshot.logical_bytes(), b"a\xffZ\xef\xbf\xbd\\xFF\xff");
        let atom = snapshot.atom_at(1).unwrap().unwrap();
        assert_eq!(atom.content, AtomContent::Opaque);
        assert_eq!(atom.raw_bytes, b"\xe2\x82");
        assert_eq!(atom.raw, 1..3);
        assert_eq!(snapshot.atom_at(3).unwrap().unwrap().content, AtomContent::Text("\u{fffd}"));
        assert_eq!(
            snapshot.resolve_raw(2).unwrap(),
            Boundary::Interior { logical: 1..2, raw: 1..3, kind: AtomKind::Opaque }
        );
        let mut out = Vec::new();
        snapshot.write_raw_range(0..snapshot.logical_bytes().len() as u64, &mut out, &AtomicBool::new(false)).unwrap();
        assert_eq!(out, fs::read(&fixture.source).unwrap());
        drop(snapshot);
        fixture.retire(owner);
    }

    #[test]
    fn utf16_endianness_bom_surrogates_and_odd_tail_are_lossless() {
        for encoding in [Encoding::UTF16_LE, Encoding::UTF16_BE] {
            let mut raw = bom(encoding).to_vec();
            for unit in [0x0041u16, 0xd83d, 0xde00, 0x000d, 0x000a, 0xd800, 0x005a, 0xfffd] {
                raw.extend_from_slice(&if encoding == Encoding::UTF16_LE {
                    unit.to_le_bytes()
                } else {
                    unit.to_be_bytes()
                });
            }
            raw.push(0x71);
            let fixture = Fixture::new(&raw);
            let owner = fixture.prepare(encoding, 2);
            let snapshot = owner.snapshot();
            assert_eq!(snapshot.logical_bytes(), b"A\xf0\x9f\x98\x80\r\n\xffZ\xef\xbf\xbd\xff");
            assert_eq!(snapshot.resolve_logical(0).unwrap(), Boundary::Exact { logical: 0, raw: 2 });
            assert_eq!(snapshot.resolve_raw(1).unwrap(), Boundary::Bom { raw: 0..2 });
            assert_eq!(snapshot.atom_at(7).unwrap().unwrap().raw_bytes, &raw[12..14]);
            assert_eq!(snapshot.raw_range(1..5).unwrap(), 4..8);
            let mut out = raw[..2].to_vec();
            snapshot
                .write_raw_range(0..snapshot.logical_bytes().len() as u64, &mut out, &AtomicBool::new(false))
                .unwrap();
            assert_eq!(out, raw);
            drop(snapshot);
            fixture.retire(owner);
        }
    }

    #[test]
    fn big5_alias_and_two_scalar_atom_have_exact_provenance() {
        // WHATWG Big5: 88 66 -> U+00CA, which does not encode back to 88 66;
        // 88 62 -> U+00CA U+0304, two valid scalars sharing one raw atom.
        let fixture = Fixture::new(b"\x88\x66\x88\x62!");
        let encoding = Encoding::for_label("big5").unwrap();
        let owner = fixture.prepare(encoding, 0);
        let snapshot = owner.snapshot();
        assert_eq!(snapshot.logical_bytes(), "ÊÊ\u{304}!".as_bytes());
        let atom = snapshot.atom_at(2).unwrap().unwrap();
        assert_eq!(atom.content, AtomContent::Text("Ê\u{304}"));
        assert_eq!(atom.raw_bytes, b"\x88\x62");
        assert_eq!(
            snapshot.resolve_logical(4).unwrap(),
            Boundary::Interior { logical: 2..6, raw: 2..4, kind: AtomKind::Text }
        );
        let mut out = Vec::new();
        assert!(matches!(
            snapshot.write_raw_range(0..4, &mut out, &AtomicBool::new(false)),
            Err(BackingError::SplitAtom(_))
        ));
        assert!(out.is_empty(), "split endpoint must fail before output");
        snapshot.write_raw_range(0..7, &mut out, &AtomicBool::new(false)).unwrap();
        assert_eq!(out, b"\x88\x66\x88\x62!");
        drop(snapshot);
        fixture.retire(owner);
    }

    #[test]
    fn every_stateless_family_matches_independent_decoder_and_raw_partition() {
        let labels = [
            "utf-8",
            "utf-16le",
            "utf-16be",
            "ibm866",
            "iso-8859-2",
            "iso-8859-3",
            "iso-8859-4",
            "iso-8859-5",
            "iso-8859-6",
            "iso-8859-7",
            "iso-8859-8",
            "iso-8859-8-i",
            "iso-8859-10",
            "iso-8859-13",
            "iso-8859-14",
            "iso-8859-15",
            "iso-8859-16",
            "koi8-r",
            "koi8-u",
            "macintosh",
            "windows-874",
            "windows-1250",
            "windows-1251",
            "windows-1252",
            "windows-1253",
            "windows-1254",
            "windows-1255",
            "windows-1256",
            "windows-1257",
            "windows-1258",
            "x-mac-cyrillic",
            "gbk",
            "gb18030",
            "big5",
            "euc-jp",
            "shift_jis",
            "euc-kr",
            "x-user-defined",
        ];
        // Bounded deterministic corpus includes every single byte, invalid
        // trails/read-ahead, UTF-16 unpaired units and a GB18030 four-byte form.
        let mut raw: Vec<u8> = (0..=255).collect();
        raw.extend_from_slice(b"\x81\x30\x81\x30\x8f\xa1\x21\xe2\x82Z\x00\xd8A\x00\x88\x62\xff");
        for label in labels {
            let encoding = Encoding::for_label(label).unwrap();
            let fixture = Fixture::new(&raw);
            let owner = fixture.prepare(encoding, 0);
            let snapshot = owner.snapshot();
            let expected = codec(encoding).decode_without_bom_handling(&raw).0;
            let mut rendered = String::new();
            let mut reconstructed = Vec::new();
            let mut position = 0;
            for atom in snapshot.atoms(0..snapshot.logical_bytes().len() as u64).unwrap() {
                let atom = atom.unwrap_or_else(|e| panic!("{label} at {position}: {e}"));
                assert_eq!(atom.logical.start, position, "{label}");
                let (text, malformed) = codec(encoding).decode_without_bom_handling(atom.raw_bytes);
                match atom.content {
                    AtomContent::Text(actual) => {
                        assert!(!malformed, "{label}");
                        assert_eq!(actual, text, "{label}");
                        rendered.push_str(actual);
                    }
                    AtomContent::Opaque => {
                        assert!(malformed, "{label}");
                        assert_eq!(
                            &snapshot.logical_bytes()[atom.logical.start as usize..atom.logical.end as usize],
                            &[0xff],
                            "{label}"
                        );
                        rendered.push('\u{fffd}');
                    }
                }
                reconstructed.extend_from_slice(atom.raw_bytes);
                position = atom.logical.end;
            }
            assert_eq!(rendered, expected, "{label}");
            assert_eq!(reconstructed, raw, "{label}");
            assert_eq!(position, snapshot.logical_bytes().len() as u64, "{label}");
            drop(snapshot);
            fixture.retire(owner);
        }
    }

    #[test]
    fn checkpoints_bound_replay_even_for_malformed_runs_and_crossing_sequences() {
        for (label, crossing) in [
            ("utf-8", &b"\xf0\x9f\x98\x80"[..]),
            ("shift_jis", &b"\x82\xa0"[..]),
            ("gb18030", &b"\x81\x30\x81\x30"[..]),
            ("big5", &b"\x88\x62"[..]),
            ("euc-jp", &b"\x8f\xa2\xaf"[..]),
            ("utf-16le", &b"\x3d\xd8\x00\xde"[..]),
            ("utf-16be", &b"\xd8\x3d\xde\x00"[..]),
        ] {
            let mut raw = vec![0xff; CHECKPOINT_RAW_BYTES * 2 + 17];
            for at in [CHECKPOINT_RAW_BYTES - 5, CHECKPOINT_RAW_BYTES * 2 - 8] {
                raw[at..at + crossing.len()].copy_from_slice(crossing);
            }
            let fixture = Fixture::new(&raw);
            let owner = fixture.prepare(Encoding::for_label(label).unwrap(), 0);
            let snapshot = owner.snapshot();
            assert!(snapshot.checkpoint_count() <= 5, "{label}: index must stay sparse");
            for i in 1..snapshot.checkpoint_count() {
                let (r, l) = snapshot.checkpoint(i);
                let (previous, _) = snapshot.checkpoint(i - 1);
                assert!(r - previous <= CHECKPOINT_RAW_BYTES as u64, "{label}");
                assert_eq!(snapshot.resolve_logical(l).unwrap(), Boundary::Exact { logical: l, raw: r });
                for offset in [r.saturating_sub(1), r, (r + 1).min(raw.len() as u64)] {
                    match snapshot.resolve_raw(offset).unwrap_or_else(|e| panic!("{label} at {offset}: {e}")) {
                        Boundary::Exact { logical, raw } => {
                            assert_eq!(snapshot.resolve_logical(logical).unwrap(), Boundary::Exact { logical, raw })
                        }
                        Boundary::Interior { logical, raw, .. } => {
                            assert!(raw.contains(&offset));
                            assert_eq!(snapshot.raw_range(logical).unwrap(), raw);
                        }
                        Boundary::Bom { .. } => panic!("no BOM"),
                    }
                }
            }
            let mut reconstructed = Vec::new();
            snapshot
                .write_raw_range(0..snapshot.logical_bytes().len() as u64, &mut reconstructed, &AtomicBool::new(false))
                .unwrap();
            assert_eq!(reconstructed, raw, "{label}");
            drop(snapshot);
            fixture.retire(owner);
        }
    }

    /// Independent whole-input replacement decode is only a display oracle;
    /// raw partitioning and typed opaque bytes are checked separately. Fixtures
    /// stay below 128KiB; no large-file benchmark or production allocation.
    fn assert_checkpoint_round_trip(raw: &[u8], encoding: Encoding) {
        let fixture = Fixture::new(raw);
        let owner = fixture.prepare(encoding, 0);
        let snapshot = owner.snapshot();
        let expected = codec(encoding).decode_without_bom_handling(raw).0;
        let mut display = String::new();
        let mut raw_at = 0;
        let mut logical_at = 0;
        for atom in snapshot.atoms(0..snapshot.logical_bytes().len() as u64).unwrap() {
            let atom = atom.unwrap();
            assert_eq!(atom.raw.start, raw_at, "{}", encoding.name());
            assert_eq!(atom.logical.start, logical_at, "{}", encoding.name());
            assert_eq!(atom.raw_bytes, &raw[atom.raw.start as usize..atom.raw.end as usize]);
            match atom.content {
                AtomContent::Text(text) => display.push_str(text),
                AtomContent::Opaque => {
                    assert_eq!(
                        &snapshot.logical_bytes()[atom.logical.start as usize..atom.logical.end as usize],
                        &[0xff]
                    );
                    assert!(codec(encoding).decode_without_bom_handling(atom.raw_bytes).1);
                    display.push('\u{fffd}');
                }
            }
            raw_at = atom.raw.end;
            logical_at = atom.logical.end;
        }
        assert_eq!(display, expected, "{}: valid text must not become opaque at a checkpoint", encoding.name());
        assert_eq!(raw_at, raw.len() as u64);
        assert_eq!(logical_at, snapshot.logical_bytes().len() as u64);
        assert!(snapshot.checkpoint_count() <= 5, "{}: index must stay sparse", encoding.name());
        for i in 1..snapshot.checkpoint_count() {
            let (r, l) = snapshot.checkpoint(i);
            assert!(r > snapshot.checkpoint(i - 1).0);
            assert!(r - snapshot.checkpoint(i - 1).0 <= CHECKPOINT_RAW_BYTES as u64);
            assert_eq!(snapshot.resolve_logical(l).unwrap(), Boundary::Exact { logical: l, raw: r });
            for raw_point in [r - 1, r, (r + 1).min(raw.len() as u64)] {
                match snapshot.resolve_raw(raw_point).unwrap() {
                    Boundary::Exact { logical, raw } => {
                        assert_eq!(snapshot.resolve_logical(logical).unwrap(), Boundary::Exact { logical, raw })
                    }
                    Boundary::Interior { logical, raw, .. } => assert_eq!(snapshot.raw_range(logical).unwrap(), raw),
                    Boundary::Bom { .. } => panic!("fixture has no BOM"),
                }
            }
        }
        let mut copied = Vec::new();
        snapshot.write_raw_range(0..logical_at, &mut copied, &AtomicBool::new(false)).unwrap();
        assert_eq!(copied, raw, "{}: raw spans must remain byte exact", encoding.name());
        drop(snapshot);
        fixture.retire(owner);
    }

    #[test]
    fn checkpoint_pending_leads_round_trip_without_eventual_neutrality() {
        // The first case is the exact R1 reproduction: every following C2
        // invalidates the pending lead and itself becomes pending indefinitely.
        // Other families exercise pending leads, malformed lookahead, and
        // incomplete 2/3/4-byte sequences rather than a special case for C2.
        for (label, pattern) in [
            ("utf-8", &b"\xc2"[..]),
            ("utf-8", &b"\xe0"[..]),
            ("utf-8", &b"\xf0"[..]),
            ("big5", &b"\x81"[..]),
            ("euc-jp", &b"\x8f"[..]),
            ("shift_jis", &b"\x82\x20"[..]),
            ("gbk", &b"\x81\x30"[..]),
            ("gb18030", &b"\x81\x30\x81\xff"[..]),
            ("euc-kr", &b"\x81\x20"[..]),
            ("utf-16le", &b"\x00\xd8"[..]),
            ("utf-16be", &b"\xd8\x00"[..]),
        ] {
            let raw: Vec<u8> = pattern.iter().copied().cycle().take(CHECKPOINT_RAW_BYTES + 1).collect();
            assert_checkpoint_round_trip(&raw, Encoding::for_label(label).unwrap());
        }
    }

    #[test]
    fn checkpoint_valid_crossings_are_never_finalized_as_malformed() {
        for (label, character) in [
            ("utf-8", &b"\xc2\xa2"[..]),
            ("utf-8", &b"\xe2\x82\xac"[..]),
            ("utf-8", &b"\xf0\x9f\x98\x80"[..]),
            ("big5", &b"\x88\x62"[..]),
            ("euc-jp", &b"\x8f\xa2\xaf"[..]),
            ("shift_jis", &b"\x82\xa0"[..]),
            ("gbk", &b"\xd6\xd0"[..]),
            ("gb18030", &b"\x81\x30\x81\x30"[..]),
            ("euc-kr", &b"\xb0\xa1"[..]),
            ("utf-16le", &b"\x3d\xd8\x00\xde"[..]),
            ("utf-16be", &b"\xd8\x3d\xde\x00"[..]),
        ] {
            let encoding = Encoding::for_label(label).unwrap();
            let utf16 = encoding == Encoding::UTF16_LE || encoding == Encoding::UTF16_BE;
            for split in 1..character.len() {
                if utf16 && split % 2 != 0 {
                    continue;
                }
                let padding = if encoding == Encoding::UTF16_LE {
                    &b"A\x00"[..]
                } else if encoding == Encoding::UTF16_BE {
                    &b"\x00A"[..]
                } else {
                    &b"A"[..]
                };
                let mut raw: Vec<u8> = padding.iter().copied().cycle().take(CHECKPOINT_RAW_BYTES - split).collect();
                raw.extend_from_slice(character);
                raw.extend_from_slice(padding);
                assert!(!codec(encoding).decode_without_bom_handling(&raw).1, "valid test fixture: {label}");
                assert_checkpoint_round_trip(&raw, encoding);
            }
        }
    }

    #[test]
    fn private_origin_survives_source_replacement_and_retirement_waits_for_pins() {
        let fixture = Fixture::new(b"first\xff");
        let owner = fixture.prepare(Encoding::UTF8, 0);
        let snapshot = owner.snapshot();
        let pin = snapshot.pin_range(0..6).unwrap();
        fs::write(&fixture.source, b"second and longer").unwrap();
        let second = fixture.prepare(Encoding::UTF8, 0);
        let current = second.snapshot();
        assert!(!snapshot.same_origin(&current));
        assert_eq!(snapshot.raw_bytes(), b"first\xff");
        assert_eq!(pin.raw_bytes(), b"first\xff");
        assert_eq!(current.logical_bytes(), b"second and longer");
        drop(snapshot);
        let owner = match owner.try_retire() {
            Err(RetirementError::LiveSnapshots(owner)) => owner,
            _ => panic!("a range pin must retain the immutable origin"),
        };
        assert!(pin.origin().paths().raw.exists());
        drop(pin);
        fixture.retire(owner);
        drop(current);
        fixture.retire(second);
    }

    #[test]
    fn empty_and_bom_only_origins_and_invalid_ranges() {
        for (raw, skip) in [(&b""[..], 0), (&b"\xef\xbb\xbf"[..], 3)] {
            let fixture = Fixture::new(raw);
            let owner = fixture.prepare(Encoding::UTF8, skip);
            let snapshot = owner.snapshot();
            assert!(snapshot.logical_bytes().is_empty());
            assert_eq!(snapshot.checkpoint_count(), 1);
            assert_eq!(snapshot.raw_range(0..0).unwrap(), skip as u64..skip as u64);
            assert!(snapshot.atom_at(0).unwrap().is_none());
            assert!(matches!(snapshot.resolve_logical(1), Err(BackingError::InvalidRange)));
            assert!(matches!(snapshot.raw_range(Range { start: 1, end: 0 }), Err(BackingError::InvalidRange)));
            drop(snapshot);
            fixture.retire(owner);
        }
    }

    #[test]
    fn invalid_bom_changed_source_and_unsupported_encodings_fail_safely() {
        let fixture = Fixture::new(b"not a BOM");
        let source = TextFile::open(&fixture.source).unwrap();
        let cancel = AtomicBool::new(false);
        assert!(matches!(
            DecodedBacking::prepare(
                &source,
                Interpretation::new(Encoding::UTF8, 3).unwrap(),
                &fixture.directory,
                &cancel
            ),
            Err(BackingError::InvalidDescriptor)
        ));
        assert_eq!(fs::read_dir(&fixture.directory).unwrap().count(), 1, "failed preparation must clean private files");
        fs::write(&fixture.source, b"changed source length").unwrap();
        assert!(matches!(
            DecodedBacking::prepare(
                &source,
                Interpretation::new(Encoding::UTF8, 0).unwrap(),
                &fixture.directory,
                &cancel
            ),
            Err(BackingError::Read(_))
        ));
        assert_eq!(fs::read_dir(&fixture.directory).unwrap().count(), 1);
        assert_eq!(fs::read(&fixture.source).unwrap(), b"changed source length");
        for label in ["iso-2022-jp", "utf-32", "utf-32le", "utf-32be", "replacement"] {
            assert!(Encoding::for_label(label).is_err(), "{label}");
        }
        assert!(Interpretation::new(Encoding::for_label("windows-1252").unwrap(), 3).is_err());
    }

    struct Quota {
        remaining: usize,
        wrote: usize,
    }
    impl Write for Quota {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                return Err(io::Error::new(io::ErrorKind::StorageFull, "injected disk full"));
            }
            let n = bytes.len().min(self.remaining);
            self.remaining -= n;
            self.wrote += n;
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn reader_recovery_preserves_raw_origin_and_stops_at_frame_boundary() {
        let fixture = Fixture::new(b"not the recovered origin");
        let raw = b"\xff\xfeA\0\0\xd8B\0\xff";
        let mut framed = raw.to_vec();
        framed.extend_from_slice(b"NEXT");
        let mut input = io::Cursor::new(framed);
        let owner = DecodedBacking::prepare_from_reader(
            &mut input,
            raw.len() as u64,
            Interpretation::new(Encoding::UTF16_LE, 2).unwrap(),
            &fixture.directory,
            &AtomicBool::new(false),
        )
        .unwrap();
        let snapshot = owner.snapshot();
        assert_eq!(snapshot.raw_bytes(), raw);
        assert_eq!(snapshot.logical_bytes(), b"A\xffB\xff");
        assert_eq!(snapshot.atom_at(1).unwrap().unwrap().raw_bytes, b"\0\xd8");
        assert_eq!(snapshot.source_path(), snapshot.paths().raw);
        assert_eq!(input.position(), raw.len() as u64);
        let mut next = [0; 4];
        input.read_exact(&mut next).unwrap();
        assert_eq!(&next, b"NEXT");
        drop(snapshot);
        fixture.retire(owner);
    }

    #[test]
    fn reader_recovery_empty_bom_and_legacy_aliases_are_lossless() {
        let fixture = Fixture::new(b"unrelated");
        let big5 = Encoding::for_label("big5").unwrap();
        for (raw, encoding, bom_len) in [
            (&b""[..], Encoding::UTF8, 0),
            (&b"\xef\xbb\xbf"[..], Encoding::UTF8, 3),
            (&b"\xff\xfe"[..], Encoding::UTF16_LE, 2),
            (&b"\x88\x62"[..], big5, 0),
        ] {
            let mut input = raw;
            let owner = DecodedBacking::prepare_from_reader(
                &mut input,
                raw.len() as u64,
                Interpretation::new(encoding, bom_len).unwrap(),
                &fixture.directory,
                &AtomicBool::new(false),
            )
            .unwrap();
            let snapshot = owner.snapshot();
            assert_eq!(snapshot.raw_bytes(), raw);
            assert_eq!(snapshot.interpretation(), Interpretation::new(encoding, bom_len).unwrap());
            if encoding == big5 {
                assert_eq!(snapshot.logical_bytes(), "\u{00ca}\u{0304}".as_bytes());
                assert!(matches!(snapshot.resolve_logical(2).unwrap(), Boundary::Interior { .. }));
            } else {
                assert!(snapshot.logical_bytes().is_empty());
            }
            drop(snapshot);
            fixture.retire(owner);
        }
    }

    #[test]
    fn reader_recovery_failure_and_cancellation_leave_no_private_files() {
        let fixture = Fixture::new(b"original");
        let interpretation = Interpretation::new(Encoding::UTF8, 0).unwrap();
        let cancel = AtomicBool::new(false);
        assert!(matches!(
            DecodedBacking::prepare_from_reader(&mut &b"short"[..], 6, interpretation, &fixture.directory, &cancel),
            Err(BackingError::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof
        ));
        assert!(matches!(
            DecodedBacking::prepare_from_reader(
                &mut &b"wrong"[..],
                5,
                Interpretation::new(Encoding::UTF8, 3).unwrap(),
                &fixture.directory,
                &cancel
            ),
            Err(BackingError::InvalidDescriptor)
        ));
        struct Cancels<'a>(&'a AtomicBool);
        impl Read for Cancels<'_> {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                bytes.fill(b'A');
                self.0.store(true, Ordering::Relaxed);
                Ok(bytes.len())
            }
        }
        // Cancellation on the final read must still abort before publication.
        assert!(matches!(
            DecodedBacking::prepare_from_reader(&mut Cancels(&cancel), 7, interpretation, &fixture.directory, &cancel),
            Err(BackingError::Cancelled)
        ));
        assert_eq!(fs::read_dir(&fixture.directory).unwrap().count(), 1);
        assert_eq!(fs::read(&fixture.source).unwrap(), b"original");
    }

    #[test]
    fn reader_recovery_bounds_read_requests_and_retains_live_pins() {
        let fixture = Fixture::new(b"original");
        struct SmallReads {
            remaining: usize,
            largest: usize,
        }
        impl Read for SmallReads {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                self.largest = self.largest.max(bytes.len());
                let n = bytes.len().min(self.remaining).min(3);
                bytes[..n].fill(b'A');
                self.remaining -= n;
                Ok(n)
            }
        }
        let mut input = SmallReads { remaining: (64 << 10) + 1, largest: 0 };
        let owner = DecodedBacking::prepare_from_reader(
            &mut input,
            ((64 << 10) + 1) as u64,
            Interpretation::new(Encoding::UTF8, 0).unwrap(),
            &fixture.directory,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(input.largest <= SCRATCH);
        assert_eq!(input.remaining, 0);
        let snapshot = owner.snapshot();
        assert_eq!(snapshot.logical_bytes().len(), (64 << 10) + 1);
        let owner = match owner.try_retire() {
            Err(RetirementError::LiveSnapshots(owner)) => owner,
            _ => panic!("reader-created origins must retain live pins"),
        };
        drop(snapshot);
        fixture.retire(owner);
    }

    #[test]
    fn injected_disk_full_and_mid_copy_cancel_stop_bounded_work() {
        let raw = vec![0xff; SCRATCH * 2 + 1];
        let cancel = AtomicBool::new(false);
        let mut writer = Quota { remaining: 7, wrote: 0 };
        assert!(
            matches!(copy_exact(&mut &raw[..], &mut writer, raw.len() as u64, &cancel), Err(BackingError::Io(e)) if e.kind() == io::ErrorKind::StorageFull)
        );
        assert_eq!(writer.wrote, 7);
        let mut index = Vec::new();
        let mut writer = Quota { remaining: 3, wrote: 0 };
        assert!(
            matches!(build(&raw, Interpretation::new(Encoding::UTF8, 0).unwrap(), &mut writer, &mut index, &cancel), Err(BackingError::Io(e)) if e.kind() == io::ErrorKind::StorageFull)
        );
        struct CancellingRead<'a> {
            cancel: &'a AtomicBool,
            reads: usize,
        }
        impl Read for CancellingRead<'_> {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                bytes.fill(b'A');
                self.reads += 1;
                self.cancel.store(true, Ordering::Relaxed);
                Ok(bytes.len())
            }
        }
        let mut input = CancellingRead { cancel: &cancel, reads: 0 };
        let mut output = Vec::new();
        assert!(matches!(
            copy_exact(&mut input, &mut output, (SCRATCH * 2) as u64, &cancel),
            Err(BackingError::Cancelled)
        ));
        assert_eq!(input.reads, 1);
        assert_eq!(output.len(), SCRATCH);
        let fixture = Fixture::new(b"unchanged");
        let source = TextFile::open(&fixture.source).unwrap();
        assert!(matches!(
            DecodedBacking::prepare(
                &source,
                Interpretation::new(Encoding::UTF8, 0).unwrap(),
                &fixture.directory,
                &cancel
            ),
            Err(BackingError::Cancelled)
        ));
        assert_eq!(fs::read_dir(&fixture.directory).unwrap().count(), 1);
        assert_eq!(fs::read(&fixture.source).unwrap(), b"unchanged");
    }
}
