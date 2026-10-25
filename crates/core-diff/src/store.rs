//! Private, caller-admitted disk records for the external comparison worker.
//! No filesystem work in constructors or Drop. Retirement is explicit and
//! nonrecursive; a failed retirement retains the original error and owner.
use kwil_core_text::compare_input::{Admission, INPUT_WINDOW, InputError, ReadWork, Step, Usage};
use std::{
    fs::{self, File, OpenOptions},
    mem::ManuallyDrop,
    ops::ControlFlow,
    path::PathBuf,
    sync::atomic::AtomicBool,
};

macro_rules! gate {
    ($value:expr) => {
        match $value {
            Step::Ready(v) => v,
            Step::Pending => return Step::Pending,
            Step::Cancelled => return Step::Cancelled,
            Step::Failed(e) => return Step::Failed(e),
        }
    };
}
macro_rules! attempt {
    ($value:expr) => {
        match $value {
            Ok(v) => v,
            Err(e) => return Step::Failed(e.into()),
        }
    };
}
pub(crate) use {attempt, gate};

pub(crate) const RECORD: u64 = 64;
pub(crate) const FORWARD: usize = 0;
pub(crate) const REVERSE: usize = 1;
pub(crate) const STACK: usize = 2;
pub(crate) const RUNS: usize = 3;
pub(crate) const HUNKS: usize = 4;
const NAMES: [&str; 5] = ["forward", "reverse", "stack", "runs", "hunks"];

/// Construct/admit path allocation overlap at the invocation boundary. The
/// directory must not exist. Only these exact immediate children are touched.
pub struct AlignmentPaths {
    pub directory: PathBuf,
    pub files: [PathBuf; 5],
}
impl AlignmentPaths {
    pub(crate) fn capacity(&self) -> Option<usize> {
        self.files.iter().try_fold(self.directory.capacity(), |n, p| n.checked_add(p.capacity()))
    }
    fn valid(&self) -> bool {
        self.directory.file_name().is_some()
            && self.files.iter().zip(NAMES).all(|(p, name)| {
                p.parent() == Some(self.directory.as_path()) && p.file_name() == Some(std::ffi::OsStr::new(name))
            })
    }
}
pub struct AlignmentAdmission<A: Admission> {
    pub aggregate: A,
    pub paths: AlignmentPaths,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    Read,
    Write,
}
struct Transfer {
    direction: Direction,
    file: usize,
    record: u64,
    words: [u64; 8],
    bytes: [u8; 64],
    encoded: usize,
    done: usize,
    credit: u8,
}

/// Kept behind ManuallyDrop so an accidental UI drop does not perform file
/// close, unlink or budget release. The invocation must schedule retirement.
pub(crate) struct Records<A: Admission> {
    files: ManuallyDrop<[Option<File>; 5]>,
    aggregate: ManuallyDrop<Option<A>>,
    paths: AlignmentPaths,
    usage: Usage,
    disk: [u64; 5],
    created: [bool; 5],
    directory_created: bool,
    opening: usize,
    transfer: Option<Transfer>,
}
impl<A: Admission> Records<A> {
    pub(crate) fn begin(
        mut admission: AlignmentAdmission<A>,
        resident: usize,
    ) -> ControlFlow<(AlignmentAdmission<A>, InputError), Self> {
        if !admission.paths.valid() {
            return ControlFlow::Break((admission, InputError::Range));
        }
        let usage = Usage { resident, disk: INPUT_WINDOW as u64 * 5, handles: 5 };
        if let Err(error) = admission.aggregate.reserve(usage) {
            return ControlFlow::Break((admission, error.into()));
        }
        ControlFlow::Continue(Self {
            files: ManuallyDrop::new([None, None, None, None, None]),
            aggregate: ManuallyDrop::new(Some(admission.aggregate)),
            paths: admission.paths,
            usage,
            disk: [INPUT_WINDOW as u64; 5],
            created: [false; 5],
            directory_created: false,
            opening: 0,
            transfer: None,
        })
    }
    pub(crate) fn usage(&self) -> Usage {
        self.usage
    }
    pub(crate) fn open(&mut self, work: &mut ReadWork, cancel: &AtomicBool) -> Step<()> {
        if !self.directory_created {
            gate!(work.debit(0, 1, cancel));
            attempt!(fs::create_dir(&self.paths.directory));
            self.directory_created = true;
        }
        while self.opening < 5 {
            gate!(work.debit(0, 1, cancel));
            let i = self.opening;
            let file = attempt!(OpenOptions::new().read(true).write(true).create_new(true).open(&self.paths.files[i]));
            self.files[i] = Some(file);
            self.created[i] = true;
            self.opening += 1;
        }
        Step::Ready(())
    }
    fn grow(&mut self, file: usize, end: u64) -> Result<(), InputError> {
        let block = INPUT_WINDOW as u64;
        let charge = end.checked_add(block - 1).ok_or(InputError::Overflow)? / block;
        let charge = charge.checked_mul(block).ok_or(InputError::Overflow)?;
        if charge <= self.disk[file] {
            return Ok(());
        }
        let additional = charge - self.disk[file];
        let total = self.usage.disk.checked_add(additional).ok_or(InputError::Overflow)?;
        self.aggregate.as_mut().unwrap().reserve(Usage { disk: additional, ..Usage::default() })?;
        self.disk[file] = charge;
        self.usage.disk = total;
        Ok(())
    }
    pub(crate) fn abandon(&mut self) {
        self.transfer = None;
    }
    pub(crate) fn read(
        &mut self,
        file: usize,
        record: u64,
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<[u64; 8]> {
        self.transfer(file, record, None, work, cancel)
    }
    pub(crate) fn write(
        &mut self,
        file: usize,
        record: u64,
        words: [u64; 8],
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<()> {
        match self.transfer(file, record, Some(words), work, cancel) {
            Step::Ready(_) => Step::Ready(()),
            Step::Pending => Step::Pending,
            Step::Cancelled => Step::Cancelled,
            Step::Failed(e) => Step::Failed(e),
        }
    }
    fn transfer(
        &mut self,
        file: usize,
        record: u64,
        words: Option<[u64; 8]>,
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<[u64; 8]> {
        gate!(work.debit(0, 0, cancel));
        if file >= 5 || self.files[file].is_none() {
            return Step::Failed(InputError::Range);
        }
        let direction = if words.is_some() { Direction::Write } else { Direction::Read };
        if let Some(t) = &self.transfer {
            if t.file != file || t.record != record || t.direction != direction || words.is_some_and(|w| w != t.words) {
                return Step::Failed(InputError::QueryChanged);
            }
        } else {
            let at = attempt!(record.checked_mul(RECORD).ok_or(InputError::Overflow));
            if direction == Direction::Write {
                attempt!(self.grow(file, attempt!(at.checked_add(RECORD).ok_or(InputError::Overflow))));
            }
            self.transfer = Some(Transfer {
                direction,
                file,
                record,
                words: words.unwrap_or([0; 8]),
                bytes: [0; 64],
                encoded: 0,
                done: 0,
                credit: 0,
            });
        }
        loop {
            let t = self.transfer.as_mut().unwrap();
            if direction == Direction::Write && t.encoded < 64 {
                let n = (64 - t.encoded).min(work.bytes.min(64) as usize);
                if n == 0 {
                    gate!(work.debit(0, 0, cancel));
                    return Step::Pending;
                }
                gate!(work.debit(n as u64, 0, cancel));
                for i in t.encoded..t.encoded + n {
                    t.bytes[i] = (t.words[i / 8] >> ((i % 8) * 8)) as u8;
                }
                t.encoded += n;
                continue;
            }
            if t.done == 64 {
                gate!(work.debit(0, 0, cancel));
                let mut t = self.transfer.take().unwrap();
                if direction == Direction::Read {
                    // Decode visits were prepaid with each disk read.
                    for (out, bytes) in t.words.iter_mut().zip(t.bytes.as_chunks::<8>().0) {
                        *out = u64::from_le_bytes(*bytes);
                    }
                }
                return Step::Ready(t.words);
            }
            let factor = if direction == Direction::Read { 2 } else { 1 };
            let available = work.bytes.min(128) + u64::from(t.credit);
            let n = (64 - t.done).min((available / factor) as usize);
            if n == 0 {
                if factor == 2 && t.credit == 0 && work.bytes != 0 {
                    gate!(work.debit(1, 0, cancel));
                    t.credit = 1;
                }
                return Step::Pending;
            }
            gate!(work.debit(n as u64 * factor - u64::from(t.credit), 1, cancel));
            t.credit = 0;
            let at = attempt!(
                record.checked_mul(RECORD).and_then(|v| v.checked_add(t.done as u64)).ok_or(InputError::Overflow)
            );
            let file = self.files[file].as_ref().unwrap();
            let count = if direction == Direction::Read {
                attempt!(read(file, at, &mut t.bytes[t.done..t.done + n]))
            } else {
                attempt!(write(file, at, &t.bytes[t.done..t.done + n]))
            };
            if count == 0 {
                return Step::Failed(
                    std::io::Error::from(if direction == Direction::Read {
                        std::io::ErrorKind::UnexpectedEof
                    } else {
                        std::io::ErrorKind::WriteZero
                    })
                    .into(),
                );
            }
            t.done += count;
        }
    }
    pub(crate) fn retirement(self) -> AlignmentRetirement<A> {
        AlignmentRetirement { records: self, close: 0, unlink: 0 }
    }
}

fn read(file: &File, at: u64, out: &mut [u8]) -> std::io::Result<usize> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        file.seek_read(out, at)
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_at(out, at)
    }
}
fn write(file: &File, at: u64, bytes: &[u8]) -> std::io::Result<usize> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        file.seek_write(bytes, at)
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.write_at(bytes, at)
    }
}

#[must_use]
pub struct AlignmentRetirement<A: Admission> {
    records: Records<A>,
    close: usize,
    unlink: usize,
}
pub enum RetirementStep<A: Admission> {
    Pending(AlignmentRetirement<A>),
    Cancelled(AlignmentRetirement<A>),
    Failed { retirement: AlignmentRetirement<A>, error: InputError },
    Ready(AlignmentAdmission<A>),
}
impl<A: Admission> AlignmentRetirement<A> {
    pub fn paths(&self) -> &AlignmentPaths {
        &self.records.paths
    }
    pub fn usage(&self) -> Usage {
        self.records.usage
    }
    pub fn step(mut self, mut work: ReadWork, cancel: &AtomicBool) -> RetirementStep<A> {
        loop {
            match work.debit(0, 1, cancel) {
                Step::Ready(()) => {}
                Step::Pending => return RetirementStep::Pending(self),
                Step::Cancelled => return RetirementStep::Cancelled(self),
                Step::Failed(error) => return RetirementStep::Failed { retirement: self, error },
            }
            if self.close < 5 {
                drop(self.records.files[self.close].take());
                self.close += 1;
                continue;
            }
            if self.unlink < 5 {
                let i = self.unlink;
                if self.records.created[i] {
                    if let Err(error) = fs::remove_file(&self.records.paths.files[i])
                        && error.kind() != std::io::ErrorKind::NotFound
                    {
                        return RetirementStep::Failed { retirement: self, error: error.into() };
                    }
                    self.records.created[i] = false;
                }
                self.unlink += 1;
                continue;
            }
            if self.records.directory_created {
                if let Err(error) = fs::remove_dir(&self.records.paths.directory)
                    && error.kind() != std::io::ErrorKind::NotFound
                {
                    return RetirementStep::Failed { retirement: self, error: error.into() };
                }
                self.records.directory_created = false;
                continue;
            }
            let mut aggregate = self.records.aggregate.take().unwrap();
            aggregate.release(self.records.usage);
            return RetirementStep::Ready(AlignmentAdmission { aggregate, paths: self.records.paths });
        }
    }
}
