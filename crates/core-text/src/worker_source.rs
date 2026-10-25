//! Worker-owned source capture. See the focused regressions below: quota
//! refusal must preserve traversal, detached classification and decoder replay.
//! Existing SourceChunks intentionally keeps its terminal-error contract.

use super::{ATOM_INTERIOR, Kind, Node, Storage};
use crate::Snapshot;
use crate::decoded_backing::{AtomKind, BackingError, Boundary, ResumeAtom, ResumeBoundary};
use crate::encoding::Encoding;
use std::io;
use std::mem::ManuallyDrop;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const WINDOW: usize = 64 << 10;
/// Conservative credits for the fixed source check: open, metadata, identity,
/// close (pinned saves omit open/close). This counts API operations, not a
/// promise about OS-internal syscalls. No check runs before all eight credits.
const ORIGINAL_CHECK_OPERATIONS: u8 = 8;

#[derive(Debug, PartialEq, Eq)]
pub enum WorkerSourceStep<T> {
    Ready(T),
    Pending,
}

#[derive(Debug)]
pub enum WorkerSourceError {
    /// Construction only. During capture/validation use WorkerSourceStep::Pending.
    Pending,
    Cancelled,
    Io(io::Error),
    Backing(BackingError),
    InvalidRange,
    WindowTooSmall,
    Stopped,
}
impl std::fmt::Display for WorkerSourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => f.write_str("source cursor construction admission pending"),
            Self::Cancelled => f.write_str("source capture cancelled"),
            Self::Io(error) => error.fmt(f),
            Self::Backing(error) => error.fmt(f),
            Self::InvalidRange => f.write_str("range outside source snapshot"),
            Self::WindowTooSmall => f.write_str("source atom exceeds supplied windows"),
            Self::Stopped => f.write_str("source capture already stopped; return its snapshot on the worker"),
        }
    }
}
impl std::error::Error for WorkerSourceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Backing(error) => Some(error),
            _ => None,
        }
    }
}
impl From<BackingError> for WorkerSourceError {
    fn from(error: BackingError) -> Self {
        match error {
            BackingError::Cancelled => Self::Cancelled,
            BackingError::Io(error) => Self::Io(error),
            other => Self::Backing(other),
        }
    }
}

/// A refused constructor returns the exact source; never retire it on the UI.
#[must_use = "return this source to its worker retirement owner"]
pub struct WorkerSourceStartError {
    pub snapshot: Snapshot,
    pub error: WorkerSourceError,
    _worker: std::marker::PhantomData<std::rc::Rc<()>>,
}
impl std::fmt::Debug for WorkerSourceStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerSourceStartError").field("error", &self.error).finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerSourceKind {
    Text,
    Preserved,
    Opaque,
    Bytes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkerSourceChunk {
    pub logical_len: usize,
    pub raw_len: usize,
    pub kind: WorkerSourceKind,
    pub encoding: Option<Encoding>,
}

/// Constant-space descent. A pause retains its current Arc and absolute offset.
/// A new leaf costs O(height) admitted metadata visits; there is no stack, height
/// ceiling, allocation or root replay on a pause, even for adversarial fragments.
struct Walk {
    node: Option<Arc<Node>>,
    base: u64,
}
impl Walk {
    fn new() -> Self {
        Self { node: None, base: 0 }
    }
    fn poll(
        &mut self,
        snapshot: &Snapshot,
        at: u64,
        cancel: &AtomicBool,
        admit: &mut impl FnMut(usize) -> io::Result<bool>,
    ) -> Result<bool, WorkerSourceError> {
        if self.node.is_none() {
            // The initial extra root pin is an admitted operation too. A quota
            // refusal must not clone/pin a source behind the caller's back.
            if !visit(0, cancel, admit)? {
                return Ok(false);
            }
            self.node = snapshot.root.clone();
            self.base = 0;
        }
        loop {
            let node = self.node.as_ref().ok_or(WorkerSourceError::InvalidRange)?;
            // One logical tree visit, including the branch's left-length cache.
            if !visit(std::mem::size_of::<Node>() + 8, cancel, admit)? {
                return Ok(false);
            }
            match &node.kind {
                Kind::Branch(left, right) => {
                    let split = self.base + left.metrics.bytes;
                    let (next, base) = if at < split { (left.clone(), self.base) } else { (right.clone(), split) };
                    self.node = Some(next);
                    self.base = base;
                }
                Kind::Leaf(_) => return Ok(true),
            }
        }
    }
}

struct Detached {
    first: Option<u8>,
    begin: Option<u32>,
    scan: u64,
}

enum Decoding {
    Locate(ResumeBoundary),
    Atom { logical: u64, raw: u64, decoder: ResumeAtom },
}

struct Item {
    logical: Range<u64>,
    raw: Range<u64>,
    kind: WorkerSourceKind,
    encoding: Option<Encoding>,
    /// Exact end of the containing decoded atom, even for a partial text item.
    decoded_end: Option<(u64, u64)>,
    credit: usize,
    source_credit: u8,
}

/// Construct, use and retire on the capture worker. `required_bytes` covers the
/// entire fixed cursor and its replay/copy scratch before construction; there
/// is no heap scratch growth. Snapshot storage is separately admitted/pinned by
/// the caller's SourceLease. Reserve `required_temporary_handles()` alongside
/// the resident charge before construction. Each callback is one source access
/// or conservative compound-check operation credit plus its byte charge; false
/// pauses, while Err is an actual admission/I/O failure. Callback requests stay
/// unchanged across Pending, allowing byte prepayment over smaller quanta.
///
/// The cursor is !Send. Its source is ManuallyDrop as a last-resort fail-closed
/// guard: accidentally abandoning the cursor retains the source rather than
/// releasing maps or unlinking spill storage on an arbitrary final-drop thread.
/// Always consume into_snapshot on the worker, including cancel/error/EOF.
#[must_use = "consume into_snapshot on the owning worker at every terminal outcome"]
pub struct WorkerSourceCursor {
    snapshot: ManuallyDrop<Snapshot>,
    range: Range<u64>,
    next: u64,
    walk: Walk,
    located: bool,
    detached: Option<Detached>,
    decoding: Option<Decoding>,
    decoded_next: Option<(u64, u64)>,
    item: Option<Item>,
    validation: Walk,
    validation_next: u64,
    validation_located: bool,
    validation_credit: u8,
    stopped: bool,
    _worker: std::marker::PhantomData<std::rc::Rc<()>>,
}

fn visit(
    bytes: usize,
    cancel: &AtomicBool,
    admit: &mut impl FnMut(usize) -> io::Result<bool>,
) -> Result<bool, WorkerSourceError> {
    if cancel.load(Ordering::Relaxed) {
        return Err(WorkerSourceError::Cancelled);
    }
    let accepted = admit(bytes).map_err(WorkerSourceError::Io)?;
    if cancel.load(Ordering::Relaxed) {
        return Err(WorkerSourceError::Cancelled);
    }
    Ok(accepted)
}

fn admit_source_check(
    storage: &Storage,
    credit: &mut u8,
    cancel: &AtomicBool,
    admit: &mut impl FnMut(usize) -> io::Result<bool>,
) -> Result<bool, WorkerSourceError> {
    let needed = if matches!(storage, Storage::Original(_)) { ORIGINAL_CHECK_OPERATIONS } else { 1 };
    while *credit < needed {
        if !visit(0, cancel, admit)? {
            return Ok(false);
        }
        *credit += 1;
    }
    Ok(true)
}

impl WorkerSourceCursor {
    pub const fn required_bytes() -> usize {
        // By-value construction/move overlap plus explicit fixed decode and
        // classification temporaries. No heap/window staging is allocated.
        2 * std::mem::size_of::<Self>() + std::mem::size_of::<Boundary>() + std::mem::size_of::<Item>() + 16
    }

    /// Reserve in the caller's aggregate admission BEFORE try_new. Existing
    /// original storage's fixed metadata check may open one temporary handle;
    /// no handles overlap across resumable checks or remain held on Pending.
    pub const fn required_temporary_handles() -> usize {
        1
    }

    pub fn try_new(
        snapshot: Snapshot,
        range: Range<u64>,
        mut admit: impl FnMut(usize) -> io::Result<bool>,
    ) -> Result<Self, WorkerSourceStartError> {
        let error = match admit(Self::required_bytes()) {
            Ok(false) => Some(WorkerSourceError::Pending),
            Err(error) => Some(WorkerSourceError::Io(error)),
            Ok(true) if range.start > range.end || range.end > snapshot.len() => Some(WorkerSourceError::InvalidRange),
            Ok(true) => None,
        };
        if let Some(error) = error {
            return Err(WorkerSourceStartError { snapshot, error, _worker: std::marker::PhantomData });
        }
        Ok(Self {
            next: range.start,
            range,
            snapshot: ManuallyDrop::new(snapshot),
            walk: Walk::new(),
            located: false,
            detached: None,
            decoding: None,
            decoded_next: None,
            item: None,
            validation: Walk::new(),
            validation_next: 0,
            validation_located: false,
            validation_credit: 0,
            stopped: false,
            _worker: std::marker::PhantomData,
        })
    }

    pub fn into_snapshot(mut self) -> Snapshot {
        // SAFETY: consumes the sole cursor; no custom Drop reads this field.
        // The Arc slots drop on this worker while the returned root stays live.
        unsafe { ManuallyDrop::take(&mut self.snapshot) }
    }

    /// Restartable full-source validation pass, used before capture and after
    /// EOF. It visits leaves incrementally, checking original storage via its
    /// existing source-safe bytes accessor. Private immutable storage has no
    /// external writer. Eight zero-byte operation credits admit each Original
    /// compound source check; no unvisited mapped payload pages are charged.
    /// Accepted credits survive Pending, before any source check/handle open.
    /// The existing check contains a fixed open/metadata/OS-identity sequence (or checks a pinned
    /// handle). It has no file scan or retry loop, but an individual stalled OS
    /// call cannot be interrupted by this cancellation flag.
    pub fn validate(
        &mut self,
        cancel: &AtomicBool,
        mut admit: impl FnMut(usize) -> io::Result<bool>,
    ) -> Result<WorkerSourceStep<()>, WorkerSourceError> {
        if self.stopped {
            return Err(WorkerSourceError::Stopped);
        }
        let result = self.validate_inner(cancel, &mut admit);
        if result.is_err() {
            self.stopped = true;
        }
        result
    }
    fn validate_inner(
        &mut self,
        cancel: &AtomicBool,
        admit: &mut impl FnMut(usize) -> io::Result<bool>,
    ) -> Result<WorkerSourceStep<()>, WorkerSourceError> {
        if cancel.load(Ordering::Relaxed) {
            return Err(WorkerSourceError::Cancelled);
        }
        while self.validation_next < self.snapshot.len() {
            if !self.validation_located {
                if !self.validation.poll(&self.snapshot, self.validation_next, cancel, admit)? {
                    return Ok(WorkerSourceStep::Pending);
                }
                self.validation_located = true;
            }
            let node = self.validation.node.as_ref().unwrap();
            let Kind::Leaf(piece) = &node.kind else { unreachable!() };
            if !admit_source_check(&piece.storage, &mut self.validation_credit, cancel, admit)? {
                return Ok(WorkerSourceStep::Pending);
            }
            worker_bytes(&piece.storage).map_err(read_error)?;
            self.validation_next = self.validation.base + node.metrics.bytes;
            self.validation = Walk::new();
            self.validation_located = false;
            self.validation_credit = 0;
        }
        self.validation_next = 0;
        Ok(WorkerSourceStep::Ready(()))
    }

    /// Fill caller-owned windows. Pending never writes either window. Ready
    /// commits one complete preserved/opaque atom or a plain/partial text run.
    /// Original byte-oriented storage uses Bytes; decoding requires a prepared
    /// immutable decoded generation. No UTF-8 validity is invented for Bytes.
    pub fn fill(
        &mut self,
        logical: &mut [u8],
        raw: &mut [u8],
        cancel: &AtomicBool,
        mut admit: impl FnMut(usize) -> io::Result<bool>,
    ) -> Result<WorkerSourceStep<Option<WorkerSourceChunk>>, WorkerSourceError> {
        if self.stopped {
            return Err(WorkerSourceError::Stopped);
        }
        let result = self.fill_inner(logical, raw, cancel, &mut admit);
        if result.is_err() {
            self.stopped = true;
        }
        result
    }

    fn fill_inner(
        &mut self,
        logical: &mut [u8],
        raw: &mut [u8],
        cancel: &AtomicBool,
        admit: &mut impl FnMut(usize) -> io::Result<bool>,
    ) -> Result<WorkerSourceStep<Option<WorkerSourceChunk>>, WorkerSourceError> {
        if cancel.load(Ordering::Relaxed) {
            return Err(WorkerSourceError::Cancelled);
        }
        if self.next == self.range.end {
            return Ok(WorkerSourceStep::Ready(None));
        }
        if !self.located {
            if !self.walk.poll(&self.snapshot, self.next, cancel, admit)? {
                return Ok(WorkerSourceStep::Pending);
            }
            self.located = true;
        }
        let node = self.walk.node.as_ref().unwrap();
        let Kind::Leaf(piece) = &node.kind else { unreachable!() };
        let start = piece.start + self.next - self.walk.base;
        let limit = piece.start + (self.range.end - self.walk.base).min(node.metrics.bytes);
        if self.item.is_none() {
            let item = match piece.storage.as_ref() {
                Storage::Opaque { encoding, logical: text, offsets, .. } => {
                    let state = self.detached.get_or_insert(Detached { first: None, begin: None, scan: start + 1 });
                    if state.first.is_none() {
                        if !visit(1, cancel, admit)? {
                            return Ok(WorkerSourceStep::Pending);
                        }
                        state.first = Some(text.bytes().map_err(read_error)?[start as usize]);
                    }
                    if state.begin.is_none() {
                        if !visit(4, cancel, admit)? {
                            return Ok(WorkerSourceStep::Pending);
                        }
                        state.begin = Some(super::offset_entry(offsets.bytes().map_err(read_error)?, start));
                    }
                    let finish = loop {
                        if !visit(4, cancel, admit)? {
                            return Ok(WorkerSourceStep::Pending);
                        }
                        let entry = super::offset_entry(offsets.bytes().map_err(read_error)?, state.scan);
                        if entry & ATOM_INTERIOR == 0 || state.scan == limit {
                            break entry;
                        }
                        state.scan += 1;
                    };
                    let begin = state.begin.unwrap();
                    let exact = begin & ATOM_INTERIOR == 0 && finish & ATOM_INTERIOR == 0;
                    let kind = if !exact {
                        WorkerSourceKind::Text
                    } else if state.first == Some(0xff) {
                        WorkerSourceKind::Opaque
                    } else if finish > begin {
                        WorkerSourceKind::Preserved
                    } else {
                        WorkerSourceKind::Text
                    };
                    let raw = if kind == WorkerSourceKind::Text { 0..0 } else { u64::from(begin)..u64::from(finish) };
                    Item::new(start..state.scan, raw, kind, Some(*encoding), None)
                }
                Storage::Decoded(source) => {
                    if self.decoding.is_none() {
                        self.decoding = Some(match self.decoded_next {
                            Some((at, raw)) if at == start => Decoding::Atom {
                                logical: at,
                                raw,
                                decoder: ResumeAtom::new(source.origin.interpretation().encoding),
                            },
                            _ => Decoding::Locate(ResumeBoundary::new(&source.origin, start)),
                        });
                    }
                    let boundary = loop {
                        match self.decoding.as_mut().unwrap() {
                            Decoding::Locate(resolver) => {
                                let Some(boundary) = resolver.poll(&source.origin, cancel, admit)? else {
                                    return Ok(WorkerSourceStep::Pending);
                                };
                                match boundary {
                                    Boundary::Exact { logical: at, raw } => {
                                        self.decoding = Some(Decoding::Atom {
                                            logical: at,
                                            raw,
                                            decoder: ResumeAtom::new(source.origin.interpretation().encoding),
                                        })
                                    }
                                    other => break other,
                                }
                            }
                            Decoding::Atom { logical: at, raw, decoder } => {
                                let end = source.origin.raw_bytes().len().min(*raw as usize + WINDOW);
                                let Some(size) =
                                    decoder.poll(&source.origin.raw_bytes()[*raw as usize..end], cancel, admit)?
                                else {
                                    return Ok(WorkerSourceStep::Pending);
                                };
                                break Boundary::Interior {
                                    logical: *at..*at + size.logical as u64,
                                    raw: *raw..*raw + size.raw as u64,
                                    kind: size.kind,
                                };
                            }
                        }
                    };
                    let Boundary::Interior { logical: atom, raw: original, kind } = boundary else {
                        return Err(WorkerSourceError::Backing(BackingError::DecoderContract));
                    };
                    let end = limit.min(atom.end);
                    let complete = start == atom.start && end == atom.end;
                    let kind = if !complete {
                        WorkerSourceKind::Text
                    } else if kind == AtomKind::Opaque {
                        WorkerSourceKind::Opaque
                    } else {
                        WorkerSourceKind::Preserved
                    };
                    let decoded_end = Some((atom.end, original.end));
                    let raw = if complete { original } else { 0..0 };
                    Item::new(start..end, raw, kind, Some(source.origin.interpretation().encoding), decoded_end)
                }
                storage => {
                    if logical.is_empty() {
                        return Err(WorkerSourceError::WindowTooSmall);
                    }
                    let end = limit.min(start + logical.len().min(WINDOW) as u64);
                    let kind = if matches!(storage, Storage::Original(_)) {
                        WorkerSourceKind::Bytes
                    } else {
                        WorkerSourceKind::Text
                    };
                    Item::new(start..end, 0..0, kind, None, None)
                }
            };
            self.item = Some(item);
        }
        let item = self.item.as_mut().unwrap();
        let n = (item.logical.end - item.logical.start) as usize;
        let r = (item.raw.end - item.raw.start) as usize;
        if n > logical.len() || r > raw.len() || n > WINDOW || r > WINDOW {
            return Err(WorkerSourceError::WindowTooSmall);
        }
        // Stable request across Pending. Callers may prepay this exact request
        // over small byte quanta before returning true; no bytes are visited
        // or caller windows changed until the full request has been admitted.
        // Keep acceptance if the following source-check operation then pauses.
        if item.credit == 0 {
            if !visit(n + r, cancel, admit)? {
                return Ok(WorkerSourceStep::Pending);
            }
            item.credit = n + r;
        }
        // In Original this accessor performs the existing external-write check
        // immediately before page access. A failed check cannot publish bytes.
        if !admit_source_check(&piece.storage, &mut item.source_credit, cancel, admit)? {
            return Ok(WorkerSourceStep::Pending);
        }
        let bytes = worker_bytes(&piece.storage).map_err(read_error)?;
        let raw_bytes = match piece.storage.as_ref() {
            Storage::Opaque { raw, .. } => raw.bytes().map_err(read_error)?,
            Storage::Decoded(source) => source.origin.raw_bytes(),
            _ => &[],
        };
        if cancel.load(Ordering::Relaxed) {
            return Err(WorkerSourceError::Cancelled);
        }
        logical[..n].copy_from_slice(&bytes[item.logical.start as usize..item.logical.end as usize]);
        raw[..r].copy_from_slice(&raw_bytes[item.raw.start as usize..item.raw.end as usize]);
        let result = WorkerSourceChunk {
            logical_len: n,
            raw_len: r,
            kind: item.kind,
            encoding: if item.kind == WorkerSourceKind::Text { None } else { item.encoding },
        };
        self.next += n as u64;
        self.decoded_next = item.decoded_end;
        self.item = None;
        self.detached = None;
        self.decoding = None;
        if self.next == self.walk.base + node.metrics.bytes {
            self.walk = Walk::new();
            self.located = false;
            self.decoded_next = None;
        }
        Ok(WorkerSourceStep::Ready(Some(result)))
    }
}

fn read_error(error: crate::ReadError) -> WorkerSourceError {
    match error {
        crate::ReadError::Io(error) => WorkerSourceError::Io(error),
        other => WorkerSourceError::Io(io::Error::other(other)),
    }
}
fn worker_bytes(storage: &Storage) -> Result<&[u8], crate::ReadError> {
    match storage {
        Storage::Original(file) => file.worker_bytes(),
        _ => storage.bytes(),
    }
}
impl Item {
    fn new(
        logical: Range<u64>,
        raw: Range<u64>,
        kind: WorkerSourceKind,
        encoding: Option<Encoding>,
        decoded_end: Option<(u64, u64)>,
    ) -> Self {
        Self { logical, raw, kind, encoding, decoded_end, credit: 0, source_credit: 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Buffer, Snapshot};

    fn cursor(snapshot: Snapshot, range: Range<u64>) -> WorkerSourceCursor {
        WorkerSourceCursor::try_new(snapshot, range, |_| Ok(true)).unwrap_or_else(|_| panic!("start"))
    }

    // Acceptance source written before implementation. The old terminal
    // next_admitted path loses its detached cursor on the first denied visit;
    // the old decoded raw_range_admitted restarts and cannot finish this way.
    // SOURCE ONLY: neither that failure nor this candidate has been executed.
    fn one_visit(cursor: &mut WorkerSourceCursor) -> Vec<(WorkerSourceKind, Vec<u8>, Vec<u8>)> {
        let mut out = Vec::new();
        let cancel = AtomicBool::new(false);
        for _ in 0..100_000 {
            let mut logical = [0xa5; 64];
            let mut raw = [0x5a; 64];
            let mut left = 1;
            match cursor
                .fill(&mut logical, &mut raw, &cancel, |_| {
                    if left == 0 {
                        Ok(false)
                    } else {
                        left -= 1;
                        Ok(true)
                    }
                })
                .unwrap()
            {
                WorkerSourceStep::Pending => {
                    assert_eq!(logical, [0xa5; 64]);
                    assert_eq!(raw, [0x5a; 64]);
                }
                WorkerSourceStep::Ready(Some(chunk)) => {
                    out.push((chunk.kind, logical[..chunk.logical_len].to_vec(), raw[..chunk.raw_len].to_vec()))
                }
                WorkerSourceStep::Ready(None) => return out,
            }
        }
        panic!("a single admitted visit per call must make forward progress");
    }

    #[test]
    fn fragmented_cursor_survives_one_visit_quanta_and_returns_exact_owner() {
        let mut buffer = Buffer::empty();
        for i in 0..80 {
            buffer.insert(0, &[b'a' + i % 26]).unwrap();
        }
        let snapshot = buffer.snapshot();
        let root = snapshot.root.as_ref().unwrap().clone();
        let mut source = cursor(snapshot, 0..80);
        let chunks = one_visit(&mut source);
        let actual: Vec<u8> = chunks.into_iter().flat_map(|(_, text, _)| text).collect();
        let mut expected = Vec::new();
        buffer.write_to(&mut expected).unwrap();
        assert_eq!(actual, expected);
        let returned = source.into_snapshot();
        assert!(Arc::ptr_eq(returned.root.as_ref().unwrap(), &root));
    }

    #[test]
    fn construction_refusal_returns_owner_before_any_traversal() {
        let mut buffer = Buffer::empty();
        buffer.insert(0, b"abc").unwrap();
        let snapshot = buffer.snapshot();
        let root = snapshot.root.as_ref().unwrap().clone();
        let refused = WorkerSourceCursor::try_new(snapshot, 0..3, |n| {
            assert_eq!(n, WorkerSourceCursor::required_bytes());
            Ok(false)
        })
        .err()
        .unwrap();
        assert!(matches!(refused.error, WorkerSourceError::Pending));
        assert!(Arc::ptr_eq(refused.snapshot.root.as_ref().unwrap(), &root));
    }

    fn detached() -> Snapshot {
        let encoding = Encoding::for_label("big5").unwrap();
        let budget = super::super::AddBudget::new(1024, Arc::new(()));
        let mut builder = super::super::ImportBuilder::new(encoding, &budget);
        builder.text(b"a").unwrap();
        builder.preserved("Ê\u{304}".as_bytes(), b"\x88\x62").unwrap();
        builder.opaque(b"\xff").unwrap();
        builder.text(b"Z").unwrap();
        Buffer::recovery_snapshot(builder.finish().unwrap())
    }

    #[test]
    fn detached_alias_opaque_and_partial_scalars_resume_without_invented_raw_endpoints() {
        let snapshot = detached();
        let mut source = cursor(snapshot.clone(), 0..7);
        assert_eq!(
            one_visit(&mut source),
            vec![
                (WorkerSourceKind::Text, b"a".to_vec(), vec![]),
                (WorkerSourceKind::Preserved, "Ê\u{304}".as_bytes().to_vec(), b"\x88\x62".to_vec()),
                (WorkerSourceKind::Opaque, vec![0xff], vec![0xff]),
                (WorkerSourceKind::Text, b"Z".to_vec(), vec![]),
            ]
        );
        drop(source.into_snapshot());
        for range in [1..3, 3..5, 2..4, 1..4, 2..5] {
            let mut source = cursor(snapshot.clone(), range.clone());
            let chunks = one_visit(&mut source);
            assert!(chunks.iter().all(|(kind, _, raw)| *kind == WorkerSourceKind::Text && raw.is_empty()));
            let actual: Vec<_> = chunks.into_iter().flat_map(|(_, bytes, _)| bytes).collect();
            assert_eq!(actual, &b"a\xc3\x8a\xcc\x84\xffZ"[range.start as usize..range.end as usize]);
            drop(source.into_snapshot());
        }
    }

    struct PrivateDirectory(std::path::PathBuf);
    impl PrivateDirectory {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("kwil-worker-source-{}-{id}", std::process::id()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn decoded(&self, raw: &[u8], encoding: Encoding) -> crate::decoded_backing::DecodedBacking {
            crate::decoded_backing::DecodedBacking::prepare_from_reader(
                &mut &raw[..],
                raw.len() as u64,
                crate::decoded_backing::Interpretation::new(encoding, 0).unwrap(),
                &self.0,
                &AtomicBool::new(false),
            )
            .unwrap()
        }
    }
    impl Drop for PrivateDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir(&self.0);
        }
    }

    #[test]
    fn decoded_checkpoint_and_atom_replay_progress_below_one_full_replay() {
        let directory = PrivateDirectory::new();
        let encoding = Encoding::for_label("big5").unwrap();
        let mut raw = vec![b'x'; 128];
        raw.extend_from_slice(b"\x88\x62\xffZ");
        let owner = directory.decoded(&raw, encoding);
        let buffer = Buffer::from_decoded(owner.snapshot());
        let snapshot = buffer.snapshot();
        let mut source = cursor(snapshot.clone(), 128..134);
        assert_eq!(
            one_visit(&mut source),
            vec![
                (WorkerSourceKind::Preserved, "Ê\u{304}".as_bytes().to_vec(), b"\x88\x62".to_vec()),
                (WorkerSourceKind::Opaque, vec![0xff], vec![0xff]),
                (WorkerSourceKind::Preserved, b"Z".to_vec(), b"Z".to_vec()),
            ]
        );
        drop(source.into_snapshot());
        for range in [128..130, 130..132, 129..131] {
            let mut source = cursor(snapshot.clone(), range.clone());
            let chunks = one_visit(&mut source);
            assert!(chunks.iter().all(|(kind, _, raw)| *kind == WorkerSourceKind::Text && raw.is_empty()));
            let actual: Vec<_> = chunks.into_iter().flat_map(|(_, bytes, _)| bytes).collect();
            assert_eq!(actual, &"Ê\u{304}".as_bytes()[(range.start - 128) as usize..(range.end - 128) as usize]);
            drop(source.into_snapshot());
        }
        drop(snapshot);
        drop(buffer);
        assert!(owner.try_retire().is_ok());
    }

    #[test]
    fn malformed_utf8_identity_and_retirement_survive_cancelled_capture() {
        use crate::decoded_backing::RetirementError;
        let directory = PrivateDirectory::new();
        let owner = directory.decoded(b"a\xe2\x82Z\xff", Encoding::UTF8);
        let buffer = Buffer::from_decoded(owner.snapshot());
        let mut source = cursor(buffer.snapshot(), 0..4);
        assert_eq!(
            one_visit(&mut source),
            vec![
                (WorkerSourceKind::Preserved, b"a".to_vec(), b"a".to_vec()),
                (WorkerSourceKind::Opaque, vec![0xff], b"\xe2\x82".to_vec()),
                (WorkerSourceKind::Preserved, b"Z".to_vec(), b"Z".to_vec()),
                (WorkerSourceKind::Opaque, vec![0xff], vec![0xff]),
            ]
        );
        drop(source.into_snapshot());
        let mut source = cursor(buffer.snapshot(), 0..4);
        drop(buffer);
        let owner = match owner.try_retire() {
            Err(RetirementError::LiveSnapshots(owner)) => owner,
            _ => panic!("capture pins must refuse retirement"),
        };
        let mut logical = [0xa5; 16];
        let mut raw = [0x5a; 16];
        assert!(matches!(
            source.fill(&mut logical, &mut raw, &AtomicBool::new(true), |_| {
                panic!("cancelled capture cannot request a source visit")
            }),
            Err(WorkerSourceError::Cancelled)
        ));
        assert_eq!(logical, [0xa5; 16]);
        assert_eq!(raw, [0x5a; 16]);
        assert!(matches!(
            source.validate(&AtomicBool::new(false), |_| panic!("terminal")),
            Err(WorkerSourceError::Stopped)
        ));
        drop(source.into_snapshot());
        assert!(owner.try_retire().is_ok());
    }

    #[test]
    fn io_error_identity_is_terminal_and_refused_operation_does_not_touch_source() {
        #[derive(Debug)]
        struct Marker(Arc<()>);
        impl std::fmt::Display for Marker {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("source I/O marker")
            }
        }
        impl std::error::Error for Marker {}
        let snapshot = detached();
        let mut source = cursor(snapshot, 0..7);
        let identity = Arc::new(());
        let mut logical = [0xa5; 16];
        let mut raw = [0x5a; 16];
        let error = source
            .fill(&mut logical, &mut raw, &AtomicBool::new(false), |_| {
                Err(io::Error::new(io::ErrorKind::PermissionDenied, Marker(identity.clone())))
            })
            .unwrap_err();
        let WorkerSourceError::Io(error) = error else { panic!("typed I/O") };
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(Arc::ptr_eq(&error.get_ref().unwrap().downcast_ref::<Marker>().unwrap().0, &identity));
        assert_eq!(logical, [0xa5; 16]);
        assert_eq!(raw, [0x5a; 16]);
        assert!(matches!(
            source.fill(&mut logical, &mut raw, &AtomicBool::new(false), |_| panic!("terminal")),
            Err(WorkerSourceError::Stopped)
        ));
        drop(source.into_snapshot());
    }

    #[test]
    fn validation_operation_quota_resumes_and_invalid_range_returns_owner() {
        let mut buffer = Buffer::empty();
        buffer.insert(0, b"x").unwrap();
        let mut source = cursor(buffer.snapshot(), 0..1);
        let cancel = AtomicBool::new(false);
        assert_eq!(source.validate(&cancel, |_| Ok(false)).unwrap(), WorkerSourceStep::Pending);
        let mut visits = 0;
        assert_eq!(
            source
                .validate(&cancel, |_| {
                    visits += 1;
                    Ok(visits <= 2)
                })
                .unwrap(),
            WorkerSourceStep::Pending
        );
        assert_eq!(
            source
                .validate(&cancel, |n| {
                    assert_eq!(n, 0);
                    Ok(true)
                })
                .unwrap(),
            WorkerSourceStep::Ready(())
        );
        drop(source.into_snapshot());
        let snapshot = buffer.snapshot();
        let root = snapshot.root.as_ref().unwrap().clone();
        let error = WorkerSourceCursor::try_new(snapshot, 0..2, |_| Ok(true)).err().unwrap();
        assert!(matches!(error.error, WorkerSourceError::InvalidRange));
        assert!(Arc::ptr_eq(error.snapshot.root.as_ref().unwrap(), &root));
    }

    #[test]
    fn one_byte_one_operation_prepayment_keeps_each_pending_request_stable() {
        let snapshot = detached();
        let mut source = cursor(snapshot, 1..6);
        let cancel = AtomicBool::new(false);
        let mut payment: Option<(usize, usize)> = None;
        let mut captured = Vec::new();
        let mut finished = false;
        for _ in 0..10_000 {
            let mut bytes_left = 1;
            let mut ops_left = 1;
            let mut logical = [0xa5; 8];
            let mut raw = [0x5a; 4];
            let result = source
                .fill(&mut logical, &mut raw, &cancel, |requested| {
                    let (width, paid) = payment.get_or_insert((requested, 0));
                    assert_eq!(*width, requested, "Pending must resume the SAME source visit");
                    let amount = (requested - *paid).min(bytes_left);
                    *paid += amount;
                    bytes_left -= amount;
                    if *paid != requested || ops_left == 0 {
                        return Ok(false);
                    }
                    ops_left -= 1;
                    payment = None;
                    Ok(true)
                })
                .unwrap();
            match result {
                WorkerSourceStep::Pending => {
                    assert_eq!(logical, [0xa5; 8]);
                    assert_eq!(raw, [0x5a; 4]);
                }
                WorkerSourceStep::Ready(Some(item)) => {
                    captured.push((item.kind, logical[..item.logical_len].to_vec(), raw[..item.raw_len].to_vec()))
                }
                WorkerSourceStep::Ready(None) => {
                    finished = true;
                    break;
                }
            }
        }
        assert!(finished, "tiny quanta must eventually finish without replay starvation");
        assert_eq!(
            captured,
            vec![
                (WorkerSourceKind::Preserved, "Ê\u{304}".as_bytes().to_vec(), b"\x88\x62".to_vec()),
                (WorkerSourceKind::Opaque, vec![0xff], vec![0xff]),
            ]
        );
        drop(source.into_snapshot());
    }

    #[test]
    fn refused_original_validation_precedes_the_existing_io_check() {
        let directory = PrivateDirectory::new();
        let path = directory.0.join("input");
        std::fs::write(&path, b"abc").unwrap();
        let file = crate::TextFile::open(&path).unwrap();
        file.build_index(1).unwrap();
        let buffer = Buffer::from_file(file).unwrap();
        let mut source = cursor(buffer.snapshot(), 0..3);
        std::fs::write(&path, b"abcd").unwrap();
        let cancel = AtomicBool::new(false);
        // Tree visit succeeds, but no original I/O check may run while that
        // operation is refused. The next admitted check detects the growth.
        let mut visits = 0;
        assert_eq!(
            source
                .validate(&cancel, |_| {
                    visits += 1;
                    Ok(visits <= 2)
                })
                .unwrap(),
            WorkerSourceStep::Pending
        );
        // Every accepted credit below is retained, but no check may observe
        // the grown source until the entire conservative bound is prepaid.
        for expected in 1..ORIGINAL_CHECK_OPERATIONS {
            let mut left = 1;
            assert_eq!(
                source
                    .validate(&cancel, |n| {
                        assert_eq!(n, 0);
                        if left == 0 {
                            Ok(false)
                        } else {
                            left -= 1;
                            Ok(true)
                        }
                    })
                    .unwrap(),
                WorkerSourceStep::Pending
            );
            assert_eq!(source.validation_credit, expected);
        }
        assert!(matches!(
            source.validate(&cancel, |n| {
                assert_eq!(n, 0);
                Ok(true)
            }),
            Err(WorkerSourceError::Io(_))
        ));
        drop(source.into_snapshot());
        drop(buffer);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn decoder_primitive_keeps_aliases_malformed_lookahead_and_surrogate_pairs() {
        let big5 = Encoding::for_label("big5").unwrap();
        for (encoding, raw, expected_raw, expected_kind, text) in [
            (big5, &b"\x88\x62\xffZ"[..], 2, AtomKind::Text, "Ê\u{304}".as_bytes()),
            (Encoding::UTF8, &b"\xe2\x82Zx"[..], 2, AtomKind::Opaque, &b"\xff"[..]),
            (Encoding::UTF16_LE, &b"\x3d\xd8\x00\xde"[..], 4, AtomKind::Text, "😀".as_bytes()),
        ] {
            let mut logical = [0; 8];
            let atom = crate::decoded_backing::decode_first_atom(encoding, raw, &mut logical).unwrap();
            assert_eq!(atom.raw, expected_raw);
            assert_eq!(atom.kind, expected_kind);
            assert_eq!(&logical[..atom.logical], text);
        }
        let mut logical = [0; 8];
        assert!(matches!(
            crate::decoded_backing::decode_first_atom(Encoding::UTF8, b"", &mut logical),
            Err(BackingError::InvalidRange)
        ));
        assert!(matches!(
            crate::decoded_backing::decode_first_atom(Encoding::UTF8, b"12345", &mut logical),
            Err(BackingError::InvalidRange)
        ));
    }

    #[test]
    fn io_terminal_keeps_decoded_owner_until_explicit_worker_return() {
        let directory = PrivateDirectory::new();
        let owner = directory.decoded(b"abc", Encoding::UTF8);
        let buffer = Buffer::from_decoded(owner.snapshot());
        let mut source = cursor(buffer.snapshot(), 0..3);
        drop(buffer);
        let mut logical = [0xa5; 8];
        let mut raw = [0x5a; 4];
        assert!(matches!(
            source.fill(&mut logical, &mut raw, &AtomicBool::new(false), |_| {
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            }),
            Err(WorkerSourceError::Io(_))
        ));
        let owner = match owner.try_retire() {
            Err(crate::decoded_backing::RetirementError::LiveSnapshots(owner)) => owner,
            _ => panic!("terminal capture must still own the immutable origin"),
        };
        assert_eq!(logical, [0xa5; 8]);
        assert_eq!(raw, [0x5a; 4]);
        drop(source.into_snapshot());
        assert!(owner.try_retire().is_ok());
    }

    #[test]
    fn worker_check_bypasses_command_batch_admission_across_quota_passes() {
        let directory = PrivateDirectory::new();
        let path = directory.0.join("input");
        std::fs::write(&path, b"abc").unwrap();
        let file = crate::TextFile::open(&path).unwrap();
        file.build_index(1).unwrap();
        let buffer = Buffer::from_file(file).unwrap();
        let mut source = cursor(buffer.snapshot(), 0..3);
        let cancel = AtomicBool::new(false);
        crate::with_checked_read_batch(|| {
            assert_eq!(source.validate(&cancel, |_| Ok(true)).unwrap(), WorkerSourceStep::Ready(()));
            std::fs::write(&path, b"abcd").unwrap();
            // Replacing worker_bytes with the batched public accessor both
            // inserts into its HashMap and reuses the stale first admission.
            // This assertion detects that wrong path without a new global
            // allocator/test-counter seam outside this module's ownership.
            assert!(matches!(source.validate(&cancel, |_| Ok(true)), Err(WorkerSourceError::Io(_))));
        });
        drop(source.into_snapshot());
        drop(buffer);
        std::fs::remove_file(path).unwrap();
    }
    include!("encoding/utf32_worker_source_tests.rs");
}
