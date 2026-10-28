use super::source::{self, CaptureKind, Captured, InputIdentity, SourceLease};
use super::{Admission, INPUT_WINDOW, InputError, ReadWork, Step, Usage};
use std::fs::{self, File, OpenOptions};
use std::mem::{ManuallyDrop, size_of};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

const LINE_RECORD: usize = 32;
const ATOM_RECORD: usize = 48;
const NAMES: [&str; 4] = ["logical", "original", "lines", "atoms"];

/// Paths are allocated/admitted by ROOT before transfer, so no hidden PathBuf
/// growth happens inside a build. `directory` must NOT already exist. Only
/// these four exact immediate children are ever created/deleted; never recurse.
pub struct StorePaths {
    pub directory: PathBuf,
    pub logical: PathBuf,
    pub original: PathBuf,
    pub lines: PathBuf,
    pub atoms: PathBuf,
}
impl StorePaths {
    fn file(&self, index: usize) -> &Path {
        match index {
            0 => &self.logical,
            1 => &self.original,
            2 => &self.lines,
            _ => &self.atoms,
        }
    }
    fn valid(&self) -> bool {
        self.directory.file_name().is_some()
            && (0..4).all(|i| {
                self.file(i).parent() == Some(self.directory.as_path())
                    && self.file(i).file_name() == Some(std::ffi::OsStr::new(NAMES[i]))
            })
    }
    fn capacity(&self) -> Option<usize> {
        [&self.directory, &self.logical, &self.original, &self.lines, &self.atoms]
            .into_iter()
            .try_fold(0_usize, |n, path| n.checked_add(path.capacity()))
    }
}

/// Transfer a caller aggregate admission handle and already allocated paths.
/// ROOT keeps the paths' original allocation charged for their FULL lifetime,
/// including this value before begin and after retirement. Begin additionally
/// reserves their actual capacities for transfer/construction overlap; it never
/// supplies an independent allowance for unadmitted caller allocations.
pub struct StoreAdmission<A: Admission> {
    pub aggregate: A,
    pub paths: StorePaths,
}

#[must_use]
pub struct RejectedSource<S, A: Admission> {
    pub source: S,
    pub admission: StoreAdmission<A>,
    pub error: InputError,
}

struct Windows {
    logical: Box<[u8; INPUT_WINDOW]>,
    original: Box<[u8; INPUT_WINDOW]>,
    lines: Box<[u8; INPUT_WINDOW]>,
    atoms: Box<[u8; INPUT_WINDOW]>,
}

/// Deliberately no cleanup Drop. Even an accidentally UI-dropped owner keeps
/// handles/reservations alive, rather than closing/deleting on the UI thread.
struct Resources<A: Admission> {
    files: ManuallyDrop<[Option<File>; 4]>,
    aggregate: ManuallyDrop<Option<A>>,
    windows: ManuallyDrop<Option<Windows>>,
    paths: StorePaths,
    usage: Usage,
    lengths: [u64; 4],
    disk: [u64; 4],
    created: [bool; 4],
    directory_created: bool,
    _worker: std::marker::PhantomData<std::rc::Rc<()>>,
}
impl<A: Admission> Resources<A> {
    fn file(&self, index: usize) -> &File {
        self.files[index].as_ref().expect("opened private file")
    }

    fn grow(&mut self, index: usize, end: u64) -> Result<(), InputError> {
        end.max(1).checked_add(INPUT_WINDOW as u64 - 1).ok_or(InputError::Overflow)?;
        let charge = crate::piece_tree::worker_disk_charge(end)?;
        let additional = charge.checked_sub(self.disk[index]).ok_or(InputError::Overflow)?;
        if additional == 0 {
            return Ok(());
        }
        let total = self.usage.disk.checked_add(additional).ok_or(InputError::Overflow)?;
        self.aggregate.as_mut().unwrap().reserve(Usage { disk: additional, ..Usage::default() })?;
        self.disk[index] = charge;
        self.usage.disk = total;
        Ok(())
    }
}

/// Exact reservation required BEFORE allocating the fixed windows. Includes
/// all four Box payloads, their construction overlap, by-value outcome overlap,
/// actual path capacities, and one of each caller lookup state. Source-owned
/// heap/cursor allocations are separately retained in the invocation budget.
pub fn resident_charge<S: SourceLease, A: Admission>(paths: &StorePaths) -> Option<usize> {
    paths
        .capacity()?
        .checked_add(INPUT_WINDOW.checked_mul(8)?)?
        .checked_add(size_of::<InputBuild<S, A>>())?
        .checked_add(size_of::<BuildStep<S, A>>())?
        .checked_add(size_of::<InputStore<A>>())?
        .checked_add(size_of::<RetirementStep<A>>())?
        .checked_add(size_of::<LineLookup>())?
        .checked_add(size_of::<RankLookup>())?
        .checked_add(size_of::<AtomLookup>())
}

/// Memory/admission only. All filesystem/source activity starts in `step`.
/// Rejection returns the exact source, aggregate token and allocated paths.
pub fn begin<S: SourceLease, A: Admission>(
    source: S,
    mut admission: StoreAdmission<A>,
) -> ControlFlow<RejectedSource<S, A>, InputBuild<S, A>> {
    let charge = if !admission.paths.valid() {
        Err(InputError::Range)
    } else {
        resident_charge::<S, A>(&admission.paths).ok_or(InputError::Overflow)
    };
    let resident = match charge {
        Ok(n) => n,
        Err(error) => return ControlFlow::Break(RejectedSource { source, admission, error }),
    };
    let per_file = match crate::piece_tree::worker_disk_charge(0) {
        Ok(n) => n,
        Err(error) => return ControlFlow::Break(RejectedSource { source, admission, error: error.into() }),
    };
    let Some(disk) = per_file.checked_mul(4) else {
        return ControlFlow::Break(RejectedSource { source, admission, error: InputError::Overflow });
    };
    let usage = Usage { resident, disk, handles: 4 };
    if let Err(error) = admission.aggregate.reserve(usage) {
        return ControlFlow::Break(RejectedSource { source, admission, error: error.into() });
    }
    let windows = Windows {
        logical: Box::new([0; INPUT_WINDOW]),
        original: Box::new([0; INPUT_WINDOW]),
        lines: Box::new([0; INPUT_WINDOW]),
        atoms: Box::new([0; INPUT_WINDOW]),
    };
    ControlFlow::Continue(InputBuild {
        source: ManuallyDrop::new(source),
        resources: Resources {
            files: ManuallyDrop::new([None, None, None, None]),
            aggregate: ManuallyDrop::new(Some(admission.aggregate)),
            windows: ManuallyDrop::new(Some(windows)),
            paths: admission.paths,
            usage,
            lengths: [0; 4],
            disk: [per_file; 4],
            created: [false; 4],
            directory_created: false,
            _worker: std::marker::PhantomData,
        },
        phase: Phase::Directory,
        captured: None,
        cursor: 0,
        scan: 0,
        logical_len: 0,
        original_len: 0,
        line_start: 0,
        previous_cr: false,
        line_count: 0,
        atom_count: 0,
        record: [0; ATOM_RECORD],
        record_len: 0,
        record_file: 0,
        after_record: AfterRecord::Scan,
        pending_line: None,
        encode_credit: 0,
        scan_credit: 0,
        index_fill: [0; 2],
        index_flushed: [0; 2],
    })
}

#[derive(Clone, Copy)]
enum Phase {
    Directory,
    Open(usize),
    Before,
    Capture,
    Logical,
    Original,
    Atom,
    Scan,
    Line,
    Record,
    After,
    Sync(usize),
    Sealed,
}
#[derive(Clone, Copy)]
enum AfterRecord {
    Scan,
    After,
}

#[must_use]
pub struct InputBuild<S: SourceLease, A: Admission> {
    source: ManuallyDrop<S>,
    resources: Resources<A>,
    phase: Phase,
    captured: Option<Captured>,
    cursor: usize,
    scan: usize,
    logical_len: u64,
    original_len: u64,
    line_start: u64,
    previous_cr: bool,
    line_count: u64,
    atom_count: u64,
    record: [u8; ATOM_RECORD],
    record_len: usize,
    record_file: usize,
    after_record: AfterRecord,
    pending_line: Option<LineSpan>,
    encode_credit: usize,
    scan_credit: usize,
    index_fill: [usize; 2],
    index_flushed: [usize; 2],
}

#[must_use]
pub enum BuildStep<S: SourceLease, A: Admission> {
    Pending(InputBuild<S, A>),
    Ready { store: InputStore<A>, source: S },
    Cancelled { source: S, retirement: Retirement<A> },
    Failed { source: S, error: InputError, retirement: Retirement<A> },
}

macro_rules! debit {
    ($work:expr, $bytes:expr, $ops:expr, $cancel:expr) => {
        match $work.debit($bytes, $ops, $cancel) {
            Step::Ready(()) => {}
            Step::Pending => return Step::Pending,
            Step::Cancelled => return Step::Cancelled,
            Step::Failed(error) => return Step::Failed(error),
        }
    };
}
macro_rules! attempt {
    ($result:expr) => {
        match $result {
            Ok(value) => value,
            Err(error) => return Step::Failed(error.into()),
        }
    };
}

impl<S: SourceLease, A: Admission> InputBuild<S, A> {
    pub fn identity(&self) -> InputIdentity {
        self.source.identity()
    }

    /// Consumes the build so a cancelled/failed result cannot accidentally keep
    /// a second source pin. Short reads/writes retain exact cursor positions.
    pub fn step(mut self, mut work: ReadWork, cancel: &AtomicBool) -> BuildStep<S, A> {
        loop {
            match self.advance(&mut work, cancel) {
                Step::Pending => return BuildStep::Pending(self),
                Step::Cancelled => {
                    return BuildStep::Cancelled {
                        source: ManuallyDrop::into_inner(self.source),
                        retirement: Retirement::new(self.resources),
                    };
                }
                Step::Failed(error) => {
                    return BuildStep::Failed {
                        source: ManuallyDrop::into_inner(self.source),
                        error,
                        retirement: Retirement::new(self.resources),
                    };
                }
                Step::Ready(false) => {}
                Step::Ready(true) => {
                    let identity = self.source.identity();
                    return BuildStep::Ready {
                        store: InputStore {
                            resources: self.resources,
                            identity,
                            logical_len: self.logical_len,
                            original_len: self.original_len,
                            line_count: self.line_count,
                            atom_count: self.atom_count,
                            payload_read: None,
                        },
                        source: ManuallyDrop::into_inner(self.source),
                    };
                }
            }
        }
    }

    fn encode_paid(&mut self, bytes: usize, work: &mut ReadWork, cancel: &AtomicBool) -> Step<()> {
        let n = (bytes - self.encode_credit).min(work.bytes.min(ATOM_RECORD as u64) as usize);
        debit!(work, n as u64, 0, cancel);
        self.encode_credit += n;
        if self.encode_credit < bytes {
            return Step::Pending;
        }
        self.encode_credit = 0;
        Step::Ready(())
    }

    fn flush_index(&mut self, file: usize, work: &mut ReadWork, cancel: &AtomicBool) -> Step<bool> {
        let index = file - 2;
        if self.index_flushed[index] == self.index_fill[index] {
            self.index_flushed[index] = 0;
            self.index_fill[index] = 0;
            return Step::Ready(true);
        }
        let start = self.index_flushed[index];
        let n = (self.index_fill[index] - start).min(work.bytes.min(INPUT_WINDOW as u64) as usize);
        if n == 0 {
            return Step::Pending;
        }
        debit!(work, n as u64, 1, cancel);
        let at = self.resources.lengths[file];
        let end = attempt!(at.checked_add(n as u64).ok_or(InputError::Overflow));
        attempt!(self.resources.grow(file, end));
        let windows = self.resources.windows.as_ref().unwrap();
        let bytes = if file == 2 { &windows.lines[start..start + n] } else { &windows.atoms[start..start + n] };
        let written = attempt!(source::write(self.resources.file(file), at, bytes));
        if written == 0 {
            return Step::Failed(io_zero_write());
        }
        self.resources.lengths[file] += written as u64;
        self.index_flushed[index] += written;
        Step::Ready(false)
    }

    fn advance(&mut self, work: &mut ReadWork, cancel: &AtomicBool) -> Step<bool> {
        debit!(work, 0, 0, cancel);
        match self.phase {
            Phase::Directory => {
                debit!(work, 0, 1, cancel);
                attempt!(fs::create_dir(&self.resources.paths.directory));
                self.resources.directory_created = true;
                self.phase = Phase::Open(0);
            }
            Phase::Open(index) => {
                if index == 4 {
                    self.phase = Phase::Before;
                    return Step::Ready(false);
                }
                debit!(work, 0, 1, cancel);
                let file = attempt!(
                    OpenOptions::new().read(true).write(true).create_new(true).open(self.resources.paths.file(index))
                );
                self.resources.files[index] = Some(file);
                self.resources.created[index] = true;
                self.phase = Phase::Open(index + 1);
            }
            Phase::Before | Phase::After => match self.source.validate(work, cancel) {
                Step::Ready(()) => {
                    self.phase = if matches!(self.phase, Phase::Before) { Phase::Capture } else { Phase::Sync(0) }
                }
                Step::Pending => return Step::Pending,
                Step::Cancelled => return Step::Cancelled,
                Step::Failed(error) => return Step::Failed(error),
            },
            Phase::Capture => {
                let windows = self.resources.windows.as_mut().unwrap();
                let captured =
                    match self.source.capture(&mut windows.logical[..], &mut windows.original[..], work, cancel) {
                        Step::Ready(value) => value,
                        Step::Pending => return Step::Pending,
                        Step::Cancelled => return Step::Cancelled,
                        Step::Failed(error) => return Step::Failed(error),
                    };
                if let Some(captured) = captured {
                    if captured.logical == 0
                        || captured.logical > INPUT_WINDOW
                        || captured.original > INPUT_WINDOW
                        || matches!(captured.kind, CaptureKind::Text | CaptureKind::Bytes)
                            && (captured.original != 0 || captured.encoding != 0)
                        || matches!(captured.kind, CaptureKind::Preserved | CaptureKind::Opaque)
                            && captured.original == 0
                        || captured.kind == CaptureKind::Opaque && captured.logical != 1
                    {
                        return Step::Failed(InputError::SourceContract);
                    }
                    attempt!(self.logical_len.checked_add(captured.logical as u64).ok_or(InputError::Overflow));
                    attempt!(self.original_len.checked_add(captured.original as u64).ok_or(InputError::Overflow));
                    self.captured = Some(captured);
                    self.cursor = 0;
                    self.scan = 0;
                    self.phase = Phase::Logical;
                } else {
                    self.pending_line = Some(LineSpan {
                        start: self.line_start,
                        content_end: self.logical_len,
                        end: self.logical_len,
                        terminator: Terminator::None,
                    });
                    self.after_record = AfterRecord::After;
                    self.phase = Phase::Line;
                }
            }
            Phase::Logical | Phase::Original | Phase::Record => {
                let (file, length) = match self.phase {
                    Phase::Logical => (0, self.captured.unwrap().logical),
                    Phase::Original => (1, self.captured.unwrap().original),
                    _ => (self.record_file, self.record_len),
                };
                if file >= 2 && self.index_fill[file - 2] == INPUT_WINDOW {
                    return match self.flush_index(file, work, cancel) {
                        Step::Ready(_) => Step::Ready(false),
                        other => other,
                    };
                }
                if self.cursor == length {
                    self.cursor = 0;
                    self.phase = match self.phase {
                        Phase::Logical => Phase::Original,
                        Phase::Original => Phase::Atom,
                        _ => match self.after_record {
                            AfterRecord::Scan => Phase::Scan,
                            AfterRecord::After => Phase::After,
                        },
                    };
                    return Step::Ready(false);
                }
                if file >= 2 {
                    let index = file - 2;
                    let start = self.index_fill[index];
                    let n = (length - self.cursor)
                        .min(INPUT_WINDOW - start)
                        .min(work.bytes.min(INPUT_WINDOW as u64) as usize);
                    if n == 0 {
                        return Step::Pending;
                    }
                    debit!(work, n as u64, 0, cancel);
                    let windows = self.resources.windows.as_mut().unwrap();
                    let output = if file == 2 {
                        &mut windows.lines[start..start + n]
                    } else {
                        &mut windows.atoms[start..start + n]
                    };
                    output.copy_from_slice(&self.record[self.cursor..self.cursor + n]);
                    self.index_fill[index] += n;
                    self.cursor += n;
                    return Step::Ready(false);
                }
                let n = (length - self.cursor).min(work.bytes.min(INPUT_WINDOW as u64) as usize);
                if n == 0 {
                    return Step::Pending;
                }
                debit!(work, n as u64, 1, cancel);
                let at = self.resources.lengths[file];
                let end = attempt!(at.checked_add(n as u64).ok_or(InputError::Overflow));
                attempt!(self.resources.grow(file, end));
                let bytes = match self.phase {
                    Phase::Logical => &self.resources.windows.as_ref().unwrap().logical[self.cursor..self.cursor + n],
                    Phase::Original => &self.resources.windows.as_ref().unwrap().original[self.cursor..self.cursor + n],
                    _ => &self.record[self.cursor..self.cursor + n],
                };
                let written = attempt!(source::write(self.resources.file(file), at, bytes));
                if written == 0 {
                    return Step::Failed(io_zero_write());
                }
                self.resources.lengths[file] += written as u64;
                self.cursor += written;
            }
            Phase::Atom => {
                match self.encode_paid(ATOM_RECORD, work, cancel) {
                    Step::Ready(()) => {}
                    Step::Pending => return Step::Pending,
                    Step::Cancelled => return Step::Cancelled,
                    Step::Failed(error) => return Step::Failed(error),
                }
                let captured = self.captured.unwrap();
                let end = self.logical_len + captured.logical as u64;
                let original_end = self.original_len + captured.original as u64;
                self.atom_count = attempt!(self.atom_count.checked_add(1).ok_or(InputError::Overflow));
                let tag = match captured.kind {
                    CaptureKind::Text => 0,
                    CaptureKind::Preserved => 1,
                    CaptureKind::Opaque => 2,
                    CaptureKind::Bytes => 3,
                };
                encode(
                    &mut self.record,
                    &[self.logical_len, end, self.original_len, original_end, tag, captured.encoding],
                );
                self.record_file = 3;
                self.record_len = ATOM_RECORD;
                self.after_record = AfterRecord::Scan;
                self.phase = Phase::Record;
            }
            Phase::Scan => {
                let captured = self.captured.unwrap();
                if self.scan == captured.logical {
                    self.logical_len += captured.logical as u64;
                    self.original_len += captured.original as u64;
                    self.captured = None;
                    self.phase = Phase::Capture;
                    return Step::Ready(false);
                }
                if self.scan_credit == 0 {
                    let n = (captured.logical - self.scan).min(work.bytes.min(INPUT_WINDOW as u64) as usize);
                    if n == 0 {
                        return Step::Pending;
                    }
                    debit!(work, n as u64, 0, cancel);
                    self.scan_credit = n;
                }
                while self.scan_credit != 0 {
                    let byte = self.resources.windows.as_ref().unwrap().logical[self.scan];
                    // An opaque marker never impersonates a newline/literal CR.
                    if captured.kind == CaptureKind::Opaque && byte != 0xff {
                        return Step::Failed(InputError::SourceContract);
                    }
                    let at = self.logical_len + self.scan as u64;
                    self.scan += 1;
                    self.scan_credit -= 1;
                    if byte == b'\n' {
                        self.pending_line = Some(LineSpan {
                            start: self.line_start,
                            content_end: at - u64::from(self.previous_cr),
                            end: at + 1,
                            terminator: if self.previous_cr { Terminator::CrLf } else { Terminator::Lf },
                        });
                        self.line_start = at + 1;
                        self.after_record = AfterRecord::Scan;
                        self.phase = Phase::Line;
                        self.previous_cr = false;
                        break;
                    }
                    self.previous_cr = byte == b'\r';
                }
            }
            Phase::Line => {
                match self.encode_paid(LINE_RECORD, work, cancel) {
                    Step::Ready(()) => {}
                    Step::Pending => return Step::Pending,
                    Step::Cancelled => return Step::Cancelled,
                    Step::Failed(error) => return Step::Failed(error),
                }
                let line = self.pending_line.take().unwrap();
                self.line_count = attempt!(self.line_count.checked_add(1).ok_or(InputError::Overflow));
                encode(&mut self.record, &[line.start, line.content_end, line.end, line.terminator as u64]);
                self.record_file = 2;
                self.record_len = LINE_RECORD;
                self.phase = Phase::Record;
            }
            Phase::Sync(index) => {
                if index == 4 {
                    self.phase = Phase::Sealed;
                    return Step::Ready(false);
                }
                if index >= 2 && self.index_fill[index - 2] != 0 {
                    return match self.flush_index(index, work, cancel) {
                        Step::Ready(_) => Step::Ready(false),
                        other => other,
                    };
                }
                debit!(work, 0, 1, cancel);
                attempt!(self.resources.file(index).sync_all());
                self.phase = Phase::Sync(index + 1);
            }
            Phase::Sealed => {
                let lines = attempt!(self.line_count.checked_mul(LINE_RECORD as u64).ok_or(InputError::Overflow));
                let atoms = attempt!(self.atom_count.checked_mul(ATOM_RECORD as u64).ok_or(InputError::Overflow));
                if self.resources.lengths != [self.logical_len, self.original_len, lines, atoms] {
                    return Step::Failed(InputError::CorruptStore);
                }
                return Step::Ready(true);
            }
        }
        Step::Ready(false)
    }
}

fn io_zero_write() -> InputError {
    std::io::Error::from(std::io::ErrorKind::WriteZero).into()
}
fn encode(out: &mut [u8], values: &[u64]) {
    for (bytes, value) in out.as_chunks_mut::<8>().0.iter_mut().zip(values) {
        bytes.copy_from_slice(&value.to_le_bytes());
    }
}
fn word(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at * 8..at * 8 + 8].try_into().unwrap())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u64)]
pub enum Terminator {
    None = 0,
    Lf = 1,
    CrLf = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineSpan {
    pub start: u64,
    pub content_end: u64,
    /// Includes terminator bytes. Literal CR at EOF or before non-LF is content.
    pub end: u64,
    pub terminator: Terminator,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AtomSpan {
    pub start: u64,
    pub end: u64,
    pub original_start: u64,
    pub original_end: u64,
    pub kind: CaptureKind,
    pub encoding: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rank {
    pub line: u64,
    pub column: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayloadSpace {
    Logical,
    Original,
}

/// Caller lookup states contain fixed records and scalars, no source pins.
/// Keep at most the admitted number live; `resident_charge` includes one each.
struct RecordRead<const N: usize> {
    bytes: [u8; N],
    done: usize,
    credit: u8,
}
impl<const N: usize> RecordRead<N> {
    fn new() -> Self {
        Self { bytes: [0; N], done: 0, credit: 0 }
    }
    fn reset(&mut self) {
        self.done = 0;
        self.credit = 0;
    }
    fn step(&mut self, file: &File, at: u64, work: &mut ReadWork, cancel: &AtomicBool) -> Step<()> {
        loop {
            debit!(work, 0, 0, cancel);
            if self.done == N {
                return Step::Ready(());
            }
            // Prepay each byte's disk visit AND subsequent record decode.
            let available = work.bytes.min((N * 2) as u64) + u64::from(self.credit);
            let n = (N - self.done).min((available / 2) as usize);
            if n == 0 {
                if self.credit == 0 && work.bytes != 0 {
                    debit!(work, 1, 0, cancel);
                    self.credit = 1;
                }
                return Step::Pending;
            }
            debit!(work, n as u64 * 2 - u64::from(self.credit), 1, cancel);
            self.credit = 0;
            let at = attempt!(at.checked_add(self.done as u64).ok_or(InputError::Overflow));
            let read = attempt!(source::read(file, at, &mut self.bytes[self.done..self.done + n]));
            if read == 0 {
                return Step::Failed(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
            }
            self.done += read;
        }
    }
}

pub struct LineLookup {
    line: u64,
    identity: Option<InputIdentity>,
    record: RecordRead<LINE_RECORD>,
}
impl LineLookup {
    pub fn new(line: u64) -> Self {
        Self { line, identity: None, record: RecordRead::new() }
    }
}
pub struct RankLookup {
    at: u64,
    identity: Option<InputIdentity>,
    low: u64,
    high: u64,
    best: Option<(u64, LineSpan)>,
    record: RecordRead<LINE_RECORD>,
}
impl RankLookup {
    pub fn new(at: u64) -> Self {
        Self { at, identity: None, low: 0, high: 0, best: None, record: RecordRead::new() }
    }
}
pub struct AtomLookup {
    at: u64,
    identity: Option<InputIdentity>,
    low: u64,
    high: u64,
    best: Option<AtomSpan>,
    record: RecordRead<ATOM_RECORD>,
}
impl AtomLookup {
    pub fn new(at: u64) -> Self {
        Self { at, identity: None, low: 0, high: 0, best: None, record: RecordRead::new() }
    }
}

struct PayloadRead {
    space: PayloadSpace,
    at: u64,
    length: usize,
    done: usize,
    credit: u8,
}

#[must_use]
pub struct InputStore<A: Admission> {
    resources: Resources<A>,
    identity: InputIdentity,
    logical_len: u64,
    original_len: u64,
    line_count: u64,
    atom_count: u64,
    payload_read: Option<PayloadRead>,
}

/// No borrowed full lines or mapped source dependency. Reads return a window
/// prefix (at most INPUT_WINDOW), even when the caller supplies a larger output.
/// Pending/Cancelled/Failed never modify caller output; queries retain the store.
pub trait InputSource {
    fn identity(&self) -> InputIdentity;
    fn byte_len(&self) -> u64;
    fn line_count(&self) -> u64;
    fn original_len(&self) -> u64;
    fn read_at(
        &mut self,
        space: PayloadSpace,
        at: u64,
        out: &mut [u8],
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<usize>;
    fn line_span(&self, lookup: &mut LineLookup, work: &mut ReadWork, cancel: &AtomicBool) -> Step<LineSpan>;
    fn rank(&self, lookup: &mut RankLookup, work: &mut ReadWork, cancel: &AtomicBool) -> Step<Rank>;
    /// EOF has no atom and returns None; an interior offset returns the FULL
    /// preserved/opaque span. Text/Bytes runs have no invented raw mapping.
    fn atom_span(&self, lookup: &mut AtomLookup, work: &mut ReadWork, cancel: &AtomicBool) -> Step<Option<AtomSpan>>;
}

impl<A: Admission> InputStore<A> {
    pub fn usage(&self) -> Usage {
        self.resources.usage
    }
    pub fn atom_count(&self) -> u64 {
        self.atom_count
    }
    /// Cached payload-query state only; no I/O, mutation or allocation.
    pub fn has_pending_payload_read(&self) -> bool {
        self.payload_read.is_some()
    }
    pub fn abandon_payload_read(&mut self) {
        self.payload_read = None;
    }
    pub fn into_retirement(self) -> Retirement<A> {
        Retirement::new(self.resources)
    }
    #[cfg(test)]
    pub(super) fn resources_for_test_truncate_logical(&self, len: u64) {
        self.resources.file(0).set_len(len).unwrap();
    }
}

impl<A: Admission> InputSource for InputStore<A> {
    fn identity(&self) -> InputIdentity {
        self.identity
    }
    fn byte_len(&self) -> u64 {
        self.logical_len
    }
    fn line_count(&self) -> u64 {
        self.line_count
    }
    fn original_len(&self) -> u64 {
        self.original_len
    }

    fn read_at(
        &mut self,
        space: PayloadSpace,
        at: u64,
        out: &mut [u8],
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<usize> {
        // Cancellation explicitly abandons scratch. A different query cannot
        // silently overwrite a pending one, even with identical output buffers.
        match work.debit(0, 0, cancel) {
            Step::Ready(()) => {}
            Step::Cancelled => {
                self.payload_read = None;
                return Step::Cancelled;
            }
            Step::Pending => return Step::Pending,
            Step::Failed(error) => return Step::Failed(error),
        }
        let len = match space {
            PayloadSpace::Logical => self.logical_len,
            PayloadSpace::Original => self.original_len,
        };
        if at > len {
            return Step::Failed(InputError::Range);
        }
        let length = (len - at).min(out.len().min(INPUT_WINDOW) as u64) as usize;
        if let Some(pending) = &self.payload_read {
            if pending.space != space || pending.at != at || pending.length != length {
                return Step::Failed(InputError::QueryChanged);
            }
        } else {
            self.payload_read = Some(PayloadRead { space, at, length, done: 0, credit: 0 });
        }
        loop {
            let pending = self.payload_read.as_mut().unwrap();
            if pending.done == length {
                // Output copy was prepaid alongside EACH successful disk visit.
                // Poll cancellation again before exposing any result bytes.
                if let Step::Cancelled = work.debit(0, 0, cancel) {
                    self.payload_read = None;
                    return Step::Cancelled;
                }
                out[..length].copy_from_slice(&self.resources.windows.as_ref().unwrap().logical[..length]);
                self.payload_read = None;
                return Step::Ready(length);
            }
            let available = work.bytes.min((INPUT_WINDOW * 2) as u64) + u64::from(pending.credit);
            let n = (length - pending.done).min((available / 2) as usize);
            if n == 0 {
                if pending.credit == 0 && work.bytes != 0 {
                    match work.debit(1, 0, cancel) {
                        Step::Ready(()) => pending.credit = 1,
                        Step::Cancelled => {
                            self.payload_read = None;
                            return Step::Cancelled;
                        }
                        Step::Pending => {}
                        Step::Failed(error) => return Step::Failed(error),
                    }
                }
                return Step::Pending;
            }
            match work.debit(n as u64 * 2 - u64::from(pending.credit), 1, cancel) {
                Step::Ready(()) => {}
                Step::Pending => return Step::Pending,
                Step::Cancelled => {
                    self.payload_read = None;
                    return Step::Cancelled;
                }
                Step::Failed(error) => return Step::Failed(error),
            }
            pending.credit = 0;
            let index = match space {
                PayloadSpace::Logical => 0,
                PayloadSpace::Original => 1,
            };
            let file = self.resources.files[index].as_ref().unwrap();
            let read = source::read(
                file,
                at + pending.done as u64,
                &mut self.resources.windows.as_mut().unwrap().logical[pending.done..pending.done + n],
            );
            match read {
                Ok(0) => {
                    self.payload_read = None;
                    return Step::Failed(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
                }
                Ok(n) => pending.done += n,
                Err(error) => {
                    self.payload_read = None;
                    return Step::Failed(error.into());
                }
            }
        }
    }

    fn line_span(&self, lookup: &mut LineLookup, work: &mut ReadWork, cancel: &AtomicBool) -> Step<LineSpan> {
        debit!(work, 0, 0, cancel);
        if lookup.line >= self.line_count {
            return Step::Failed(InputError::Range);
        }
        if lookup.identity.is_some_and(|id| id != self.identity) {
            return Step::Failed(InputError::QueryChanged);
        }
        lookup.identity = Some(self.identity);
        let at = attempt!(lookup.line.checked_mul(LINE_RECORD as u64).ok_or(InputError::Overflow));
        match lookup.record.step(self.resources.file(2), at, work, cancel) {
            Step::Ready(()) => {
                let span = attempt!(decode_line(&lookup.record.bytes, self.logical_len));
                lookup.record.reset();
                Step::Ready(span)
            }
            Step::Pending => Step::Pending,
            Step::Cancelled => Step::Cancelled,
            Step::Failed(error) => Step::Failed(error),
        }
    }

    fn rank(&self, lookup: &mut RankLookup, work: &mut ReadWork, cancel: &AtomicBool) -> Step<Rank> {
        debit!(work, 0, 0, cancel);
        if lookup.at > self.logical_len {
            return Step::Failed(InputError::Range);
        }
        match lookup.identity {
            Some(id) if id != self.identity => return Step::Failed(InputError::QueryChanged),
            None => {
                lookup.identity = Some(self.identity);
                lookup.high = self.line_count;
            }
            _ => {}
        }
        loop {
            if lookup.low == lookup.high {
                let Some((line, span)) = lookup.best else {
                    return Step::Failed(InputError::CorruptStore);
                };
                return Step::Ready(Rank { line, column: lookup.at - span.start });
            }
            let mid = lookup.low + (lookup.high - lookup.low) / 2;
            let at = attempt!(mid.checked_mul(LINE_RECORD as u64).ok_or(InputError::Overflow));
            match lookup.record.step(self.resources.file(2), at, work, cancel) {
                Step::Ready(()) => {}
                Step::Pending => return Step::Pending,
                Step::Cancelled => return Step::Cancelled,
                Step::Failed(error) => return Step::Failed(error),
            }
            let span = attempt!(decode_line(&lookup.record.bytes, self.logical_len));
            lookup.record.reset();
            if span.start <= lookup.at {
                lookup.best = Some((mid, span));
                lookup.low = mid + 1;
            } else {
                lookup.high = mid;
            }
        }
    }

    fn atom_span(&self, lookup: &mut AtomLookup, work: &mut ReadWork, cancel: &AtomicBool) -> Step<Option<AtomSpan>> {
        debit!(work, 0, 0, cancel);
        if lookup.at > self.logical_len {
            return Step::Failed(InputError::Range);
        }
        match lookup.identity {
            Some(id) if id != self.identity => return Step::Failed(InputError::QueryChanged),
            None => {
                lookup.identity = Some(self.identity);
                lookup.high = self.atom_count;
            }
            _ => {}
        }
        if lookup.at == self.logical_len {
            return Step::Ready(None);
        }
        loop {
            if lookup.low == lookup.high {
                return match lookup.best {
                    Some(span) if lookup.at < span.end => Step::Ready(Some(span)),
                    _ => Step::Failed(InputError::CorruptStore),
                };
            }
            let mid = lookup.low + (lookup.high - lookup.low) / 2;
            let at = attempt!(mid.checked_mul(ATOM_RECORD as u64).ok_or(InputError::Overflow));
            match lookup.record.step(self.resources.file(3), at, work, cancel) {
                Step::Ready(()) => {}
                Step::Pending => return Step::Pending,
                Step::Cancelled => return Step::Cancelled,
                Step::Failed(error) => return Step::Failed(error),
            }
            let span = attempt!(decode_atom(&lookup.record.bytes, self.logical_len, self.original_len));
            lookup.record.reset();
            if span.start <= lookup.at {
                lookup.best = Some(span);
                lookup.low = mid + 1;
            } else {
                lookup.high = mid;
            }
        }
    }
}

fn decode_line(bytes: &[u8; LINE_RECORD], len: u64) -> Result<LineSpan, InputError> {
    let (start, content_end, end) = (word(bytes, 0), word(bytes, 1), word(bytes, 2));
    let terminator = match word(bytes, 3) {
        0 => Terminator::None,
        1 => Terminator::Lf,
        2 => Terminator::CrLf,
        _ => return Err(InputError::CorruptStore),
    };
    if start > content_end || content_end > end || end > len || end - content_end != terminator as u64 {
        return Err(InputError::CorruptStore);
    }
    Ok(LineSpan { start, content_end, end, terminator })
}
fn decode_atom(bytes: &[u8; ATOM_RECORD], len: u64, original: u64) -> Result<AtomSpan, InputError> {
    let span = AtomSpan {
        start: word(bytes, 0),
        end: word(bytes, 1),
        original_start: word(bytes, 2),
        original_end: word(bytes, 3),
        kind: match word(bytes, 4) {
            0 => CaptureKind::Text,
            1 => CaptureKind::Preserved,
            2 => CaptureKind::Opaque,
            3 => CaptureKind::Bytes,
            _ => return Err(InputError::CorruptStore),
        },
        encoding: word(bytes, 5),
    };
    if span.start >= span.end
        || span.end > len
        || span.original_start > span.original_end
        || span.original_end > original
        || matches!(span.kind, CaptureKind::Text | CaptureKind::Bytes)
            && (span.original_start != span.original_end || span.encoding != 0)
        || matches!(span.kind, CaptureKind::Preserved | CaptureKind::Opaque) && span.original_start == span.original_end
        || span.kind == CaptureKind::Opaque && span.end - span.start != 1
    {
        return Err(InputError::CorruptStore);
    }
    Ok(span)
}

#[must_use]
pub struct Retirement<A: Admission> {
    resources: Resources<A>,
    close: usize,
    unlink: usize,
}
impl<A: Admission> Retirement<A> {
    fn new(resources: Resources<A>) -> Self {
        Self { resources, close: 0, unlink: 0 }
    }
    pub fn paths(&self) -> &StorePaths {
        &self.resources.paths
    }
    pub fn usage(&self) -> Usage {
        self.resources.usage
    }

    /// Worker-only, nonrecursive and resumable. Failed cleanup retains every
    /// path/handle still owned; capacity remains reserved until full completion.
    pub fn step(mut self, mut work: ReadWork, cancel: &AtomicBool) -> RetirementStep<A> {
        loop {
            match work.debit(0, 1, cancel) {
                Step::Ready(()) => {}
                Step::Pending => return RetirementStep::Pending(self),
                Step::Cancelled => return RetirementStep::Cancelled(self),
                Step::Failed(error) => return RetirementStep::Failed { retirement: self, error },
            }
            if self.close < 4 {
                drop(self.resources.files[self.close].take());
                self.close += 1;
                continue;
            }
            if self.unlink < 4 {
                let index = self.unlink;
                if self.resources.created[index] {
                    match fs::remove_file(self.resources.paths.file(index)) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return RetirementStep::Failed { retirement: self, error: error.into() },
                    }
                    self.resources.created[index] = false;
                }
                self.unlink += 1;
                continue;
            }
            if self.resources.directory_created {
                match fs::remove_dir(&self.resources.paths.directory) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return RetirementStep::Failed { retirement: self, error: error.into() },
                }
                self.resources.directory_created = false;
                continue;
            }
            drop(self.resources.windows.take());
            let mut aggregate = self.resources.aggregate.take().unwrap();
            aggregate.release(self.resources.usage);
            return RetirementStep::Ready(StoreAdmission { aggregate, paths: self.resources.paths });
        }
    }
}
#[must_use]
pub enum RetirementStep<A: Admission> {
    Pending(Retirement<A>),
    Ready(StoreAdmission<A>),
    Cancelled(Retirement<A>),
    Failed { retirement: Retirement<A>, error: InputError },
}

pub mod read;
