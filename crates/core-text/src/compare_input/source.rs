//! Capture boundary shared with the invocation/engine loan owner.
//!
//! Uses the worker cursor's resumable tree/decoded-atom resolver. The ordinary
//! `source_chunks::next_admitted` and decoded index are deliberately untouched.

use super::{Admission, INPUT_WINDOW, InputError, ReadWork, Step, Usage};
use crate::Snapshot;
use crate::encoding::Encoding;
use crate::piece_tree::worker_source::{WorkerSourceCursor, WorkerSourceError, WorkerSourceKind, WorkerSourceStep};
use std::fs::{File, Metadata};
use std::io;
use std::mem::ManuallyDrop;
use std::ops::{ControlFlow, Range};
use std::sync::atomic::AtomicBool;
use std::time::SystemTime;

/// Invocation/revision identity assigned by the engine loan owner, not a hash
/// used as proof of content equality. All four words participate in identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputIdentity(pub [u64; 4]);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureKind {
    /// Logical text only, including materialized partial text atoms. No raw endpoint invented.
    Text,
    /// One complete source text atom, including noncanonical multi-scalar aliases.
    Preserved,
    /// Exactly one complete malformed atom. Logical width is one opaque marker;
    /// identity/equality MUST compare its encoding and original payload bytes.
    Opaque,
    /// Byte-exact native projection. No claim that these bytes were decoded.
    Bytes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Captured {
    pub logical: usize,
    pub original: usize,
    pub kind: CaptureKind,
    /// Stable codec discriminator agreed with the invocation owner. Zero for
    /// Text/Bytes. Distinct codecs must not share a key for opaque equality.
    pub encoding: u64,
}

/// Complete engine loan, including provenance pins and any cursor/decoder state.
/// Implementations are worker operations and never allocate unadmitted scratch.
/// `capture` fills at most the two supplied fixed windows. Ready(Some) commits
/// exactly one atom or plain text/byte run; Ready(None) means EOF. Pending must
/// retain exact progress internally and leave BOTH output windows untouched.
/// Every classification, endpoint, payload, decoder and replay visit must debit
/// work BEFORE access. Cancellation is separate from OS errors and admission.
/// `validate` is called before capture and after EOF; it may itself resume.
/// Return partial text as Text, never as fabricated original endpoints; opaque
/// atoms may never be split. The lease is returned on every terminal outcome.
pub trait SourceLease {
    fn identity(&self) -> InputIdentity;
    fn capture(
        &mut self,
        logical: &mut [u8],
        original: &mut [u8],
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<Option<Captured>>;
    fn validate(&mut self, work: &mut ReadWork, cancel: &AtomicBool) -> Step<()>;
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
    // SAFETY: live borrowed handle and valid writable result structure.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((info.dwVolumeSerialNumber as u64, ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64))
}

pub(super) fn read(file: &File, at: u64, out: &mut [u8]) -> io::Result<usize> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_at(out, at)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        file.seek_read(out, at)
    }
}
pub(super) fn write(file: &File, at: u64, bytes: &[u8]) -> io::Result<usize> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.write_at(bytes, at)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        file.seek_write(bytes, at)
    }
}

/// Invocation-open native handle, copied with bounded positioned reads. ROOT
/// opens/admit-charges this handle on the capture worker; never a mapped file or
/// Buffer index. A selected-encoding projection must wrap/replace this byte
/// source with an admitted decoder lease before comparing to decoded text.
/// The exact original handle returns to ROOT, which closes it on its worker.
#[must_use]
pub struct NativeFileLease {
    file: ManuallyDrop<Option<File>>,
    identity: InputIdentity,
    stamp: Option<Stamp>,
    at: u64,
    validation_credit: u8,
}
impl NativeFileLease {
    pub fn new(file: File, identity: InputIdentity) -> Self {
        Self { file: ManuallyDrop::new(Some(file)), identity, stamp: None, at: 0, validation_credit: 0 }
    }
    /// Worker only. No implicit handle close on a UI-side Drop.
    pub fn into_file(mut self) -> File {
        self.file.take().expect("owned native capture handle")
    }
}
impl SourceLease for NativeFileLease {
    fn identity(&self) -> InputIdentity {
        self.identity
    }

    fn validate(&mut self, work: &mut ReadWork, cancel: &AtomicBool) -> Step<()> {
        // Windows uses metadata plus GetFileInformationByHandle. Charge both
        // calls on every platform (no unmetered retries hidden in read_exact).
        while self.validation_credit != 2 {
            match work.debit(0, 1, cancel) {
                Step::Ready(()) => {}
                other => return other,
            }
            self.validation_credit += 1;
        }
        self.validation_credit = 0;
        let now = match Stamp::read(self.file.as_ref().unwrap()) {
            Ok(stamp) => stamp,
            Err(error) => return Step::Failed(error.into()),
        };
        match &self.stamp {
            Some(before) if before != &now => Step::Failed(InputError::SourceChanged),
            Some(_) => Step::Ready(()),
            None => {
                self.stamp = Some(now);
                Step::Ready(())
            }
        }
    }

    fn capture(
        &mut self,
        logical: &mut [u8],
        _: &mut [u8],
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<Option<Captured>> {
        match work.debit(0, 0, cancel) {
            Step::Ready(()) => {}
            Step::Pending => return Step::Pending,
            Step::Cancelled => return Step::Cancelled,
            Step::Failed(error) => return Step::Failed(error),
        }
        let Some(stamp) = &self.stamp else {
            return Step::Failed(InputError::SourceContract);
        };
        if self.at == stamp.len {
            return match work.debit(0, 0, cancel) {
                Step::Ready(()) => Step::Ready(None),
                Step::Cancelled => Step::Cancelled,
                Step::Pending => Step::Pending,
                Step::Failed(error) => Step::Failed(error),
            };
        }
        let n = (stamp.len - self.at).min(logical.len() as u64).min(work.bytes) as usize;
        if n == 0 {
            return Step::Pending;
        }
        match work.debit(n as u64, 1, cancel) {
            Step::Ready(()) => {}
            Step::Pending => return Step::Pending,
            Step::Cancelled => return Step::Cancelled,
            Step::Failed(error) => return Step::Failed(error),
        }
        match read(self.file.as_ref().unwrap(), self.at, &mut logical[..n]) {
            Ok(0) => Step::Failed(InputError::SourceChanged),
            Ok(n) => {
                self.at += n as u64;
                Step::Ready(Some(Captured { logical: n, original: 0, kind: CaptureKind::Bytes, encoding: 0 }))
            }
            Err(error) => Step::Failed(error.into()),
        }
    }
}

/// Source-access credit survives quota pauses BEFORE the source access. This
/// permits one-byte work slices even for a complete bounded atom/window visit.
/// The worker cursor must retry exactly the same access after false.
#[derive(Default)]
struct AccessCredit {
    requested: Option<usize>,
    paid: usize,
}
impl AccessCredit {
    fn admit(&mut self, bytes: usize, work: &mut ReadWork, cancel: &AtomicBool) -> io::Result<bool> {
        if self.requested.is_some_and(|n| n != bytes) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "resumed source access changed width"));
        }
        self.requested = Some(bytes);
        let n = (bytes - self.paid).min(work.bytes.min(usize::MAX as u64) as usize);
        match work.debit(n as u64, 0, cancel) {
            Step::Ready(()) => self.paid += n,
            Step::Pending | Step::Cancelled => return Ok(false),
            Step::Failed(error) => return Err(io::Error::other(error)),
        }
        if self.paid != bytes {
            return Ok(false);
        }
        match work.debit(0, 1, cancel) {
            Step::Ready(()) => {
                self.requested = None;
                self.paid = 0;
                Ok(true)
            }
            Step::Pending | Step::Cancelled => Ok(false),
            Step::Failed(error) => Err(io::Error::other(error)),
        }
    }
}
fn paused<T>(cancel: &AtomicBool) -> Step<T> {
    if cancel.load(std::sync::atomic::Ordering::Relaxed) { Step::Cancelled } else { Step::Pending }
}

/// Admitted capture cursor plus the invocation's exact Snapshot owner. ROOT
/// binds its document/engine loan around this value; no clone/subsnapshot here.
#[must_use]
pub struct SnapshotLease<A: Admission> {
    cursor: ManuallyDrop<Option<WorkerSourceCursor>>,
    aggregate: ManuallyDrop<Option<A>>,
    identity: InputIdentity,
    charged: usize,
    handles: usize,
    credit: AccessCredit,
}
#[must_use]
pub struct RejectedSnapshot<A: Admission> {
    pub snapshot: Snapshot,
    pub aggregate: A,
    pub error: InputError,
}
impl<A: Admission> SnapshotLease<A> {
    /// Admission covers cursor construction overlap plus this adapter's actual
    /// inline layout. Source storage remains charged by ROOT's engine loan.
    pub fn try_new(
        snapshot: Snapshot,
        range: Range<u64>,
        identity: InputIdentity,
        mut aggregate: A,
    ) -> Result<Self, RejectedSnapshot<A>> {
        let Some(charged) = WorkerSourceCursor::required_bytes().checked_add(2 * std::mem::size_of::<Self>()) else {
            return Err(RejectedSnapshot { snapshot, aggregate, error: InputError::Overflow });
        };
        let handles = WorkerSourceCursor::required_temporary_handles();
        let usage = Usage { resident: charged, handles, ..Usage::default() };
        if let Err(error) = aggregate.reserve(usage) {
            return Err(RejectedSnapshot { snapshot, aggregate, error: error.into() });
        }
        match WorkerSourceCursor::try_new(snapshot, range, |_| Ok(true)) {
            Ok(cursor) => Ok(Self {
                cursor: ManuallyDrop::new(Some(cursor)),
                aggregate: ManuallyDrop::new(Some(aggregate)),
                identity,
                charged,
                handles,
                credit: AccessCredit::default(),
            }),
            Err(error) => {
                aggregate.release(usage);
                Err(RejectedSnapshot { snapshot: error.snapshot, aggregate, error: source_error(error.error) })
            }
        }
    }

    /// Worker only. Returns the SAME Snapshot (including all provenance owners),
    /// releases cursor-only admission, and returns the aggregate handle.
    pub fn into_parts(mut self) -> (Snapshot, A) {
        let snapshot = self.cursor.take().unwrap().into_snapshot();
        let mut aggregate = self.aggregate.take().unwrap();
        aggregate.release(Usage { resident: self.charged, handles: self.handles, ..Usage::default() });
        (snapshot, aggregate)
    }
}
impl<A: Admission> SourceLease for SnapshotLease<A> {
    fn identity(&self) -> InputIdentity {
        self.identity
    }
    fn validate(&mut self, work: &mut ReadWork, cancel: &AtomicBool) -> Step<()> {
        let credit = &mut self.credit;
        let result = self.cursor.as_mut().unwrap().validate(cancel, |n| credit.admit(n, work, cancel));
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Step::Cancelled;
        }
        match result {
            Ok(WorkerSourceStep::Ready(())) => Step::Ready(()),
            Ok(WorkerSourceStep::Pending) => Step::Pending,
            Err(WorkerSourceError::Cancelled) => Step::Cancelled,
            Err(error) => Step::Failed(source_error(error)),
        }
    }
    fn capture(
        &mut self,
        logical: &mut [u8],
        original: &mut [u8],
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<Option<Captured>> {
        let credit = &mut self.credit;
        let result = self.cursor.as_mut().unwrap().fill(logical, original, cancel, |n| credit.admit(n, work, cancel));
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Step::Cancelled;
        }
        match result {
            Ok(WorkerSourceStep::Ready(None)) => Step::Ready(None),
            Ok(WorkerSourceStep::Ready(Some(chunk))) => {
                let kind = match chunk.kind {
                    WorkerSourceKind::Text => CaptureKind::Text,
                    WorkerSourceKind::Preserved => CaptureKind::Preserved,
                    WorkerSourceKind::Opaque => CaptureKind::Opaque,
                    WorkerSourceKind::Bytes => CaptureKind::Bytes,
                };
                let encoding = if matches!(kind, CaptureKind::Text | CaptureKind::Bytes) {
                    0
                } else {
                    match chunk.encoding.and_then(codec_key) {
                        Some(key) => key,
                        None => return Step::Failed(InputError::SourceContract),
                    }
                };
                Step::Ready(Some(Captured { logical: chunk.logical_len, original: chunk.raw_len, kind, encoding }))
            }
            Ok(WorkerSourceStep::Pending) => Step::Pending,
            Err(WorkerSourceError::Cancelled) => Step::Cancelled,
            Err(error) => Step::Failed(source_error(error)),
        }
    }
}

fn source_error(error: WorkerSourceError) -> InputError {
    match error {
        WorkerSourceError::Io(error) => original_io_error(error),
        WorkerSourceError::Backing(error) => backing_error(error),
        WorkerSourceError::InvalidRange => InputError::Range,
        WorkerSourceError::Pending
        | WorkerSourceError::Cancelled
        | WorkerSourceError::WindowTooSmall
        | WorkerSourceError::Stopped => InputError::SourceContract,
    }
}
fn original_io_error(error: io::Error) -> InputError {
    // The core source accessor transports Changed in io::Error::other. Extract
    // only that wrapper. Untouched OS/custom io::Error objects move unchanged.
    if error.get_ref().is_some_and(|inner| inner.is::<crate::ReadError>()) {
        let inner = error.into_inner().unwrap().downcast::<crate::ReadError>().unwrap();
        return match *inner {
            crate::ReadError::Changed(_) => InputError::SourceChanged,
            crate::ReadError::Io(error) => InputError::Io(error),
        };
    }
    InputError::Io(error)
}
fn backing_error(error: crate::decoded_backing::BackingError) -> InputError {
    use crate::decoded_backing::BackingError;
    match error {
        BackingError::Io(error) => original_io_error(error),
        BackingError::Read(crate::ReadError::Changed(_)) => InputError::SourceChanged,
        BackingError::Read(crate::ReadError::Io(error)) => InputError::Io(error),
        BackingError::Encoding(crate::encoding::EncodingError::Io(error)) => InputError::Io(error),
        other => InputError::Provenance(other),
    }
}

/// Collision-free disk discriminator for every canonical supported stateless
/// encoding_rs codec. No hash or process-local pointer is used as equality.
fn codec_key(encoding: Encoding) -> Option<u64> {
    Some(match encoding.name() {
        "Big5" => 1,
        "EUC-JP" => 2,
        "EUC-KR" => 3,
        "GBK" => 4,
        "IBM866" => 5,
        "ISO-8859-2" => 6,
        "ISO-8859-3" => 7,
        "ISO-8859-4" => 8,
        "ISO-8859-5" => 9,
        "ISO-8859-6" => 10,
        "ISO-8859-7" => 11,
        "ISO-8859-8" => 12,
        "ISO-8859-8-I" => 13,
        "ISO-8859-10" => 14,
        "ISO-8859-13" => 15,
        "ISO-8859-14" => 16,
        "ISO-8859-15" => 17,
        "ISO-8859-16" => 18,
        "KOI8-R" => 19,
        "KOI8-U" => 20,
        "Shift_JIS" => 21,
        "UTF-16BE" => 22,
        "UTF-16LE" => 23,
        "UTF-8" => 24,
        "gb18030" => 25,
        "macintosh" => 26,
        "windows-1250" => 27,
        "windows-1251" => 28,
        "windows-1252" => 29,
        "windows-1253" => 30,
        "windows-1254" => 31,
        "windows-1255" => 32,
        "windows-1256" => 33,
        "windows-1257" => 34,
        "windows-1258" => 35,
        "windows-874" => 36,
        "x-mac-cyrillic" => 37,
        "x-user-defined" => 38,
        _ => return None,
    })
}

struct NativeAtom {
    logical: [u8; 8],
    logical_len: usize,
    raw_len: usize,
    kind: CaptureKind,
}

/// Invocation-selected stateless decode projection. No sniffing, whole-file
/// buffer, ordinary decoded index, or DecodedBacking::prepare call. This reuses
/// the core's exact bounded atom decoder (including malformed lookahead).
/// ROOT must include the interpretation in its assigned InputIdentity.
#[must_use]
pub struct DecodedNativeLease<A: Admission> {
    native: ManuallyDrop<Option<NativeFileLease>>,
    aggregate: ManuallyDrop<Option<A>>,
    window: ManuallyDrop<Option<Box<[u8; INPUT_WINDOW]>>>,
    interpretation: crate::decoded_backing::Interpretation,
    encoding_key: u64,
    charged: usize,
    filled: usize,
    next: usize,
    carry: [u8; 4],
    carry_len: usize,
    bom_checked: bool,
    atom: Option<NativeAtom>,
    credit: AccessCredit,
    decoder_bytes: usize,
    decoder_ops: usize,
}
#[must_use]
pub struct RejectedNative<A: Admission> {
    pub native: NativeFileLease,
    pub aggregate: A,
    pub error: InputError,
}
impl<A: Admission> DecodedNativeLease<A> {
    pub fn try_new(
        native: NativeFileLease,
        interpretation: crate::decoded_backing::Interpretation,
        mut aggregate: A,
    ) -> ControlFlow<RejectedNative<A>, Self> {
        if let Err(error) =
            crate::decoded_backing::Interpretation::new(interpretation.encoding, interpretation.bom_len as usize)
        {
            return ControlFlow::Break(RejectedNative { native, aggregate, error: backing_error(error) });
        }
        let Some(encoding_key) = codec_key(interpretation.encoding) else {
            return ControlFlow::Break(RejectedNative { native, aggregate, error: InputError::SourceContract });
        };
        // Exact fixed read window, its constructor overlap, by-value wrapper
        // overlap and shared primitive's fixed decoder/output temporaries.
        let charge = (INPUT_WINDOW * 2)
            .checked_add(2 * std::mem::size_of::<Self>())
            .and_then(|n| n.checked_add(2 * std::mem::size_of::<crate::decoded_backing::ResumeAtom>()))
            .and_then(|n| n.checked_add(std::mem::size_of::<crate::decoded_backing::AtomSize>() + 16));
        let Some(charged) = charge else {
            return ControlFlow::Break(RejectedNative { native, aggregate, error: InputError::Overflow });
        };
        if let Err(error) = aggregate.reserve(Usage { resident: charged, ..Usage::default() }) {
            return ControlFlow::Break(RejectedNative { native, aggregate, error: error.into() });
        }
        ControlFlow::Continue(Self {
            native: ManuallyDrop::new(Some(native)),
            aggregate: ManuallyDrop::new(Some(aggregate)),
            window: ManuallyDrop::new(Some(Box::new([0; INPUT_WINDOW]))),
            interpretation,
            encoding_key,
            charged,
            filled: 0,
            next: 0,
            carry: [0; 4],
            carry_len: 0,
            bom_checked: false,
            atom: None,
            credit: AccessCredit::default(),
            decoder_bytes: 0,
            decoder_ops: 0,
        })
    }
    /// Worker only; return the complete native invocation handle and budget.
    pub fn into_parts(mut self) -> (NativeFileLease, A) {
        drop(self.window.take());
        let native = self.native.take().unwrap();
        let mut aggregate = self.aggregate.take().unwrap();
        aggregate.release(Usage { resident: self.charged, ..Usage::default() });
        (native, aggregate)
    }

    fn fill_carry(&mut self, work: &mut ReadWork, cancel: &AtomicBool) -> Step<()> {
        loop {
            match work.debit(0, 0, cancel) {
                Step::Ready(()) => {}
                other => return other,
            }
            if self.carry_len == 4 {
                return Step::Ready(());
            }
            if self.next != self.filled {
                let n = (4 - self.carry_len).min(self.filled - self.next);
                match self.credit.admit(n, work, cancel) {
                    Ok(true) => {}
                    Ok(false) => return paused(cancel),
                    Err(error) => return Step::Failed(error.into()),
                }
                self.carry[self.carry_len..self.carry_len + n]
                    .copy_from_slice(&self.window.as_ref().unwrap()[self.next..self.next + n]);
                self.carry_len += n;
                self.next += n;
                continue;
            }
            let native = self.native.as_mut().unwrap();
            let Some(stamp) = &native.stamp else {
                return Step::Failed(InputError::SourceContract);
            };
            if native.at == stamp.len {
                return Step::Ready(());
            }
            let n = (stamp.len - native.at).min(INPUT_WINDOW as u64).min(work.bytes) as usize;
            if n == 0 {
                return Step::Pending;
            }
            match work.debit(n as u64, 1, cancel) {
                Step::Ready(()) => {}
                other => return other,
            }
            match read(native.file.as_ref().unwrap(), native.at, &mut self.window.as_mut().unwrap()[..n]) {
                Ok(0) => return Step::Failed(InputError::SourceChanged),
                Ok(n) => {
                    native.at += n as u64;
                    self.next = 0;
                    self.filled = n;
                }
                Err(error) => return Step::Failed(error.into()),
            }
        }
    }
}
impl<A: Admission> SourceLease for DecodedNativeLease<A> {
    fn identity(&self) -> InputIdentity {
        self.native.as_ref().unwrap().identity()
    }
    fn validate(&mut self, work: &mut ReadWork, cancel: &AtomicBool) -> Step<()> {
        self.native.as_mut().unwrap().validate(work, cancel)
    }
    fn capture(
        &mut self,
        logical: &mut [u8],
        original: &mut [u8],
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<Option<Captured>> {
        loop {
            match work.debit(0, 0, cancel) {
                Step::Ready(()) => {}
                Step::Pending => return Step::Pending,
                Step::Cancelled => return Step::Cancelled,
                Step::Failed(error) => return Step::Failed(error),
            }
            if self.atom.is_none() {
                match self.fill_carry(work, cancel) {
                    Step::Ready(()) => {}
                    Step::Pending => return Step::Pending,
                    Step::Cancelled => return Step::Cancelled,
                    Step::Failed(error) => return Step::Failed(error),
                }
                if !self.bom_checked {
                    // Descriptor validates the BOM length; check actual bytes
                    // only when explicitly selected, never strip a literal FEFF.
                    let encoding = self.interpretation.encoding;
                    let bom = if encoding == Encoding::UTF8 {
                        b"\xef\xbb\xbf".as_slice()
                    } else if encoding == Encoding::UTF16_LE {
                        b"\xff\xfe".as_slice()
                    } else if encoding == Encoding::UTF16_BE {
                        b"\xfe\xff".as_slice()
                    } else {
                        b"".as_slice()
                    };
                    let n = self.interpretation.bom_len as usize;
                    if n != 0 {
                        match self.credit.admit(self.carry_len, work, cancel) {
                            Ok(true) => {}
                            Ok(false) => return paused(cancel),
                            Err(error) => return Step::Failed(error.into()),
                        }
                        if n > self.carry_len || self.carry[..n] != *bom {
                            return Step::Failed(InputError::Provenance(
                                crate::decoded_backing::BackingError::InvalidDescriptor,
                            ));
                        }
                        self.carry.copy_within(n..self.carry_len, 0);
                        self.carry_len -= n;
                    }
                    self.bom_checked = true;
                    continue;
                }
                if self.carry_len == 0 {
                    return Step::Ready(None);
                }
                // Shared helper's documented worst case: <=4 raw visits,
                // <=4 decoder calls and <=8 logical bytes. Prepay every unit;
                // exact payment state survives even a one-byte/op slice.
                let n = (12 - self.decoder_bytes).min(work.bytes.min(12) as usize);
                match work.debit(n as u64, 0, cancel) {
                    Step::Ready(()) => self.decoder_bytes += n,
                    Step::Pending => return Step::Pending,
                    Step::Cancelled => return Step::Cancelled,
                    Step::Failed(error) => return Step::Failed(error),
                }
                if self.decoder_bytes != 12 {
                    return Step::Pending;
                }
                while self.decoder_ops != 4 {
                    match work.debit(0, 1, cancel) {
                        Step::Ready(()) => self.decoder_ops += 1,
                        Step::Pending => return Step::Pending,
                        Step::Cancelled => return Step::Cancelled,
                        Step::Failed(error) => return Step::Failed(error),
                    }
                }
                let mut bytes = [0; 8];
                let size = match crate::decoded_backing::decode_first_atom(
                    self.interpretation.encoding,
                    &self.carry[..self.carry_len],
                    &mut bytes,
                ) {
                    Ok(size) => size,
                    Err(error) => return Step::Failed(backing_error(error)),
                };
                if size.raw == 0 || size.raw > self.carry_len || size.logical == 0 || size.logical > bytes.len() {
                    return Step::Failed(InputError::SourceContract);
                }
                self.decoder_bytes = 0;
                self.decoder_ops = 0;
                self.atom = Some(NativeAtom {
                    logical: bytes,
                    logical_len: size.logical,
                    raw_len: size.raw,
                    kind: match size.kind {
                        crate::decoded_backing::AtomKind::Text => CaptureKind::Preserved,
                        crate::decoded_backing::AtomKind::Opaque => CaptureKind::Opaque,
                    },
                });
            }
            let atom = self.atom.as_ref().unwrap();
            if atom.logical_len > logical.len() || atom.raw_len > original.len() {
                return Step::Failed(InputError::SourceContract);
            }
            let copy = atom.logical_len + atom.raw_len + (self.carry_len - atom.raw_len);
            match self.credit.admit(copy, work, cancel) {
                Ok(true) => {}
                Ok(false) => return paused(cancel),
                Err(error) => return Step::Failed(error.into()),
            }
            logical[..atom.logical_len].copy_from_slice(&atom.logical[..atom.logical_len]);
            original[..atom.raw_len].copy_from_slice(&self.carry[..atom.raw_len]);
            let result = Captured {
                logical: atom.logical_len,
                original: atom.raw_len,
                kind: atom.kind,
                encoding: self.encoding_key,
            };
            self.carry.copy_within(atom.raw_len..self.carry_len, 0);
            self.carry_len -= atom.raw_len;
            self.atom = None;
            return Step::Ready(Some(result));
        }
    }
}
