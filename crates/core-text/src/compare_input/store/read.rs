//! Sealed, independently admitted positioned-read ownership.
//!
//! ROOT registration in store.rs: `pub mod read;`. Reexport this module's
//! public types from compare_input.rs; this child alone needs store internals.
//! No InputStore Send/Sync change, shared mutable query, path, source pin, or
//! file-sized collection is involved. A production aggregate may be Send+Sync;
//! tests deliberately use non-Send accounting without changing this boundary.
//!
//! REQUIRED WORKER ORDER: retire every read lease (including partial creation
//! owners), then retire/unlink the original store, then retire its invocation.
//! Duplicated handles do not own the original disk reservation or paths. This
//! explicit order is part of the caller contract, not enforced by accidental
//! Drop or by retaining InputStore in an unsafe Send wrapper.
use super::{
    ATOM_RECORD, AtomLookup, AtomSpan, INPUT_WINDOW, InputError, InputIdentity, InputStore, LINE_RECORD, LineLookup,
    LineSpan, PayloadSpace, Rank, RankLookup, RecordRead, decode_atom, decode_line, source,
};
use crate::compare_input::{Admission, ReadWork, Step, Usage};
use std::{
    fs::File,
    io,
    mem::{ManuallyDrop, size_of},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

pub const READ_LEASE_HANDLES: usize = 4;

macro_rules! read_debit {
    ($work:expr, $bytes:expr, $ops:expr, $cancel:expr) => {
        match $work.debit($bytes, $ops, $cancel) {
            Step::Ready(()) => {}
            Step::Pending => return Step::Pending,
            Step::Cancelled => return Step::Cancelled,
            Step::Failed(error) => return Step::Failed(error),
        }
    };
}
macro_rules! read_attempt {
    ($result:expr) => {
        match $result {
            Ok(value) => value,
            Err(error) => return Step::Failed(error.into()),
        }
    };
}

/// Fixed counters, no per-offset map. Requests include short/error reads and
/// repeated visits. Charged bytes additionally include prepaid decoding/output
/// transfer. Simultaneous snapshots need not represent one atomic instant.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReadVisits {
    pub calls: [u64; READ_LEASE_HANDLES],
    pub requested_bytes: [u64; READ_LEASE_HANDLES],
    pub returned_bytes: [u64; READ_LEASE_HANDLES],
    pub decoded_bytes: u64,
    pub charged_bytes: u64,
}
struct Visits {
    calls: [AtomicU64; READ_LEASE_HANDLES],
    requested: [AtomicU64; READ_LEASE_HANDLES],
    returned: [AtomicU64; READ_LEASE_HANDLES],
    decoded: AtomicU64,
    charged: AtomicU64,
    #[cfg(test)]
    read_limit: std::sync::atomic::AtomicUsize,
}
impl Visits {
    fn new() -> Self {
        Self {
            calls: std::array::from_fn(|_| AtomicU64::new(0)),
            requested: std::array::from_fn(|_| AtomicU64::new(0)),
            returned: std::array::from_fn(|_| AtomicU64::new(0)),
            decoded: AtomicU64::new(0),
            charged: AtomicU64::new(0),
            #[cfg(test)]
            read_limit: std::sync::atomic::AtomicUsize::new(INPUT_WINDOW),
        }
    }
    fn snapshot(&self) -> ReadVisits {
        ReadVisits {
            calls: std::array::from_fn(|i| self.calls[i].load(Ordering::Relaxed)),
            requested_bytes: std::array::from_fn(|i| self.requested[i].load(Ordering::Relaxed)),
            returned_bytes: std::array::from_fn(|i| self.returned[i].load(Ordering::Relaxed)),
            decoded_bytes: self.decoded.load(Ordering::Relaxed),
            charged_bytes: self.charged.load(Ordering::Relaxed),
        }
    }
}

// Deliberate quarantine: no automatic handle close or aggregate release, even
// if the last Arc is accidentally dropped on the UI. Successful cleanup uses
// ReadLeaseRetirement::step, never ManuallyDrop as a substitute for retirement.
struct LeaseResources<A: Admission> {
    files: ManuallyDrop<[Option<File>; READ_LEASE_HANDLES]>,
    admission: ManuallyDrop<Option<A>>,
    usage: Usage,
}

#[must_use]
pub struct ReadLease<A: Admission> {
    resources: LeaseResources<A>,
    identity: InputIdentity,
    lengths: [u64; READ_LEASE_HANDLES],
    line_count: u64,
    atom_count: u64,
    visits: Visits,
}

/// Pre-clone refusal/Pending/cancellation returns the exact unreserved token.
/// Once reservation succeeds, ALL terminal exits return explicit retirement,
/// including zero/partial clones. The original OS clone error is moved intact.
#[must_use]
pub enum ReadLeaseStep<A: Admission> {
    Pending(A),
    Cancelled(A),
    Rejected { admission: A, error: InputError },
    Ready(ReadLease<A>),
    Interrupted(ReadLeaseRetirement<A>),
    Failed { retirement: ReadLeaseRetirement<A>, error: InputError },
}

/// Fixed header, by-value creation/retirement overlap, and one caller-owned
/// lookup of each kind. No heap allocation or source window is constructed.
/// `transfer_resident` additionally prepays a caller's exact Arc/header/transfer
/// envelope; that charge lives with this lease through explicit retirement.
pub fn read_lease_resident_charge<A: Admission>(transfer_resident: usize) -> Option<usize> {
    size_of::<ReadLease<A>>()
        .checked_add(size_of::<ReadLeaseStep<A>>())?
        .checked_add(size_of::<ReadLeaseRetirement<A>>())?
        .checked_add(size_of::<ReadLeaseRetirementStep<A>>())?
        .checked_add(size_of::<LineLookup>())?
        .checked_add(size_of::<RankLookup>())?
        .checked_add(size_of::<AtomLookup>())?
        .checked_add(transfer_resident)
}

impl<A: Admission> InputStore<A> {
    /// Worker-only. `admission` MUST refer to the same invocation aggregate as
    /// this store. Admission has no identity operation, so callers preserve this
    /// relationship exactly as for the existing StoreAdmission transfer API.
    pub fn read_lease(&self, admission: A, work: &mut ReadWork, cancel: &AtomicBool) -> ReadLeaseStep<A> {
        self.read_lease_with_transfer(admission, 0, work, cancel)
    }

    pub fn read_lease_with_transfer(
        &self,
        admission: A,
        transfer_resident: usize,
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> ReadLeaseStep<A> {
        self.create_read_lease(admission, transfer_resident, work, cancel, |file, _| file.try_clone())
    }

    #[cfg(test)]
    fn read_lease_with(
        &self,
        admission: A,
        work: &mut ReadWork,
        cancel: &AtomicBool,
        clone: impl FnMut(&File, usize) -> io::Result<File>,
    ) -> ReadLeaseStep<A> {
        self.create_read_lease(admission, 0, work, cancel, clone)
    }

    fn create_read_lease(
        &self,
        mut admission: A,
        transfer_resident: usize,
        work: &mut ReadWork,
        cancel: &AtomicBool,
        mut clone: impl FnMut(&File, usize) -> io::Result<File>,
    ) -> ReadLeaseStep<A> {
        let Some(resident) = read_lease_resident_charge::<A>(transfer_resident) else {
            return ReadLeaseStep::Rejected { admission, error: InputError::Overflow };
        };
        // Cached seal consistency only; no metadata syscall or pathname read.
        if self.line_count == 0
            || self.line_count.checked_mul(LINE_RECORD as u64) != Some(self.resources.lengths[2])
            || self.atom_count.checked_mul(ATOM_RECORD as u64) != Some(self.resources.lengths[3])
            || self.logical_len != self.resources.lengths[0]
            || self.original_len != self.resources.lengths[1]
        {
            return ReadLeaseStep::Rejected { admission, error: InputError::CorruptStore };
        }
        // Prepay every possible clone before the FIRST handle duplication.
        match work.debit(0, READ_LEASE_HANDLES as u64, cancel) {
            Step::Ready(()) => {}
            Step::Pending => return ReadLeaseStep::Pending(admission),
            Step::Cancelled => return ReadLeaseStep::Cancelled(admission),
            Step::Failed(error) => return ReadLeaseStep::Rejected { admission, error },
        }
        let usage = Usage { resident, disk: 0, handles: READ_LEASE_HANDLES };
        if let Err(error) = admission.reserve(usage) {
            return ReadLeaseStep::Rejected { admission, error: error.into() };
        }
        let mut resources = LeaseResources {
            files: ManuallyDrop::new(std::array::from_fn(|_| None)),
            admission: ManuallyDrop::new(Some(admission)),
            usage,
        };
        for index in 0..READ_LEASE_HANDLES {
            if cancel.load(Ordering::Relaxed) {
                return ReadLeaseStep::Interrupted(ReadLeaseRetirement::new(resources));
            }
            match clone(self.resources.file(index), index) {
                Ok(file) => resources.files[index] = Some(file),
                Err(error) => {
                    return ReadLeaseStep::Failed {
                        retirement: ReadLeaseRetirement::new(resources),
                        error: InputError::Io(error),
                    };
                }
            }
        }
        if cancel.load(Ordering::Relaxed) {
            return ReadLeaseStep::Interrupted(ReadLeaseRetirement::new(resources));
        }
        ReadLeaseStep::Ready(ReadLease {
            resources,
            identity: self.identity,
            lengths: self.resources.lengths,
            line_count: self.line_count,
            atom_count: self.atom_count,
            visits: Visits::new(),
        })
    }
}

impl<A: Admission> ReadLease<A> {
    pub fn identity(&self) -> InputIdentity {
        self.identity
    }
    pub fn byte_len(&self) -> u64 {
        self.lengths[0]
    }
    pub fn original_len(&self) -> u64 {
        self.lengths[1]
    }
    pub fn line_count(&self) -> u64 {
        self.line_count
    }
    pub fn atom_count(&self) -> u64 {
        self.atom_count
    }
    pub fn usage(&self) -> Usage {
        self.resources.usage
    }
    pub fn visits(&self) -> ReadVisits {
        self.visits.snapshot()
    }

    /// Maximum COMPLETE line/rank/provenance record visits for a fresh lookup.
    /// Short reads add operations, never more than one call per record byte.
    /// No retry on OS error/Interrupted is hidden.
    /// Count=0 needs no search; u64::MAX still has at most 64 comparisons.
    pub fn search_records(count: u64) -> u64 {
        u64::from(u64::BITS - count.leading_zeros())
    }
    pub fn rank_work_bound(&self) -> ReadWork {
        let records = Self::search_records(self.line_count);
        // Worst short reads return one byte for each success. Requested visits
        // then total N + (N-1) + ... + 1, each charged for read AND decode.
        ReadWork::new(records * LINE_RECORD as u64 * (LINE_RECORD as u64 + 1), records * LINE_RECORD as u64)
    }
    pub fn line_work_bound(&self) -> ReadWork {
        ReadWork::new(LINE_RECORD as u64 * (LINE_RECORD as u64 + 1), LINE_RECORD as u64)
    }
    pub fn provenance_work_bound(&self) -> ReadWork {
        let records = Self::search_records(self.atom_count);
        ReadWork::new(records * ATOM_RECORD as u64 * (ATOM_RECORD as u64 + 1), records * ATOM_RECORD as u64)
    }

    fn read_paid(&self, file: usize, at: u64, out: &mut [u8]) -> Result<usize, InputError> {
        if out.len() > INPUT_WINDOW || at.checked_add(out.len() as u64).is_none_or(|end| end > self.lengths[file]) {
            return Err(InputError::Range);
        }
        #[cfg(test)]
        let out = {
            let n = out.len().min(self.visits.read_limit.load(Ordering::Relaxed));
            &mut out[..n]
        };
        self.visits.calls[file].fetch_add(1, Ordering::Relaxed);
        self.visits.requested[file].fetch_add(out.len() as u64, Ordering::Relaxed);
        let read = source::read(self.resources.files[file].as_ref().expect("live lease handle"), at, out)?;
        if read > out.len() {
            return Err(InputError::SourceContract);
        }
        self.visits.returned[file].fetch_add(read as u64, Ordering::Relaxed);
        Ok(read)
    }

    /// At most ONE positioned read of at most 64 KiB. Prepay requested bytes
    /// for the disk visit and caller transfer, plus one I/O operation. Short
    /// reads return Ready(prefix); caller repeats at the returned endpoint and
    /// prepays that visit too. Pending/Cancelled before I/O leave out untouched.
    /// Unlike mutable InputSource::read_at this needs no private staging window:
    /// after I/O an OS failure may have touched out; failed bytes are not usable.
    pub fn read_at(
        &self,
        space: PayloadSpace,
        at: u64,
        out: &mut [u8],
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<usize> {
        read_debit!(work, 0, 0, cancel);
        let file = match space {
            PayloadSpace::Logical => 0,
            PayloadSpace::Original => 1,
        };
        if at > self.lengths[file] {
            return Step::Failed(InputError::Range);
        }
        let n = (self.lengths[file] - at).min(out.len().min(INPUT_WINDOW) as u64) as usize;
        if n == 0 {
            return Step::Ready(0);
        }
        read_debit!(work, n as u64 * 2, 1, cancel);
        self.visits.charged.fetch_add(n as u64 * 2, Ordering::Relaxed);
        match self.read_paid(file, at, &mut out[..n]) {
            Ok(0) => Step::Failed(io::Error::from(io::ErrorKind::UnexpectedEof).into()),
            Ok(n) => Step::Ready(n),
            Err(error) => Step::Failed(error),
        }
    }

    fn record<const N: usize>(
        &self,
        file: usize,
        at: u64,
        record: &mut RecordRead<N>,
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<()> {
        loop {
            read_debit!(work, 0, 0, cancel);
            if record.done == N {
                return Step::Ready(());
            }
            let available = work.bytes.min((N * 2) as u64) + u64::from(record.credit);
            let n = (N - record.done).min((available / 2) as usize);
            if n == 0 {
                if record.credit == 0 && work.bytes != 0 {
                    read_debit!(work, 1, 0, cancel);
                    record.credit = 1;
                    self.visits.charged.fetch_add(1, Ordering::Relaxed);
                }
                return Step::Pending;
            }
            let charge = n as u64 * 2 - u64::from(record.credit);
            read_debit!(work, charge, 1, cancel);
            record.credit = 0;
            self.visits.charged.fetch_add(charge, Ordering::Relaxed);
            let at = read_attempt!(at.checked_add(record.done as u64).ok_or(InputError::Overflow));
            let read = read_attempt!(self.read_paid(file, at, &mut record.bytes[record.done..record.done + n]));
            if read == 0 {
                return Step::Failed(io::Error::from(io::ErrorKind::UnexpectedEof).into());
            }
            record.done += read;
        }
    }

    pub fn line_span(&self, lookup: &mut LineLookup, work: &mut ReadWork, cancel: &AtomicBool) -> Step<LineSpan> {
        read_debit!(work, 0, 0, cancel);
        if lookup.line >= self.line_count {
            return Step::Failed(InputError::Range);
        }
        if lookup.identity.is_some_and(|id| id != self.identity) {
            return Step::Failed(InputError::QueryChanged);
        }
        lookup.identity = Some(self.identity);
        let at = read_attempt!(lookup.line.checked_mul(LINE_RECORD as u64).ok_or(InputError::Overflow));
        match self.record(2, at, &mut lookup.record, work, cancel) {
            Step::Ready(()) => {
                self.visits.decoded.fetch_add(LINE_RECORD as u64, Ordering::Relaxed);
                let span = read_attempt!(decode_line(&lookup.record.bytes, self.byte_len()));
                lookup.record.reset();
                Step::Ready(span)
            }
            Step::Pending => Step::Pending,
            Step::Cancelled => Step::Cancelled,
            Step::Failed(error) => Step::Failed(error),
        }
    }

    pub fn rank(&self, lookup: &mut RankLookup, work: &mut ReadWork, cancel: &AtomicBool) -> Step<Rank> {
        read_debit!(work, 0, 0, cancel);
        if lookup.at > self.byte_len() {
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
                return match lookup.best {
                    Some((line, span)) => Step::Ready(Rank { line, column: lookup.at - span.start }),
                    None => Step::Failed(InputError::CorruptStore),
                };
            }
            let mid = lookup.low + (lookup.high - lookup.low) / 2;
            let at = read_attempt!(mid.checked_mul(LINE_RECORD as u64).ok_or(InputError::Overflow));
            match self.record(2, at, &mut lookup.record, work, cancel) {
                Step::Ready(()) => {}
                Step::Pending => return Step::Pending,
                Step::Cancelled => return Step::Cancelled,
                Step::Failed(error) => return Step::Failed(error),
            }
            self.visits.decoded.fetch_add(LINE_RECORD as u64, Ordering::Relaxed);
            let span = read_attempt!(decode_line(&lookup.record.bytes, self.byte_len()));
            lookup.record.reset();
            if span.start <= lookup.at {
                lookup.best = Some((mid, span));
                lookup.low = mid + 1;
            } else {
                lookup.high = mid;
            }
        }
    }

    /// Full provenance span, preserving raw endpoints/codec, never sentinel
    /// equality. Original bytes are independently available through read_at.
    pub fn atom_span(
        &self,
        lookup: &mut AtomLookup,
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<Option<AtomSpan>> {
        read_debit!(work, 0, 0, cancel);
        if lookup.at > self.byte_len() {
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
        if lookup.at == self.byte_len() {
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
            let at = read_attempt!(mid.checked_mul(ATOM_RECORD as u64).ok_or(InputError::Overflow));
            match self.record(3, at, &mut lookup.record, work, cancel) {
                Step::Ready(()) => {}
                Step::Pending => return Step::Pending,
                Step::Cancelled => return Step::Cancelled,
                Step::Failed(error) => return Step::Failed(error),
            }
            self.visits.decoded.fetch_add(ATOM_RECORD as u64, Ordering::Relaxed);
            let span = read_attempt!(decode_atom(&lookup.record.bytes, self.byte_len(), self.original_len()));
            lookup.record.reset();
            if span.start <= lookup.at {
                lookup.best = Some(span);
                lookup.low = mid + 1;
            } else {
                lookup.high = mid;
            }
        }
    }

    pub fn into_retirement(self) -> ReadLeaseRetirement<A> {
        ReadLeaseRetirement::new(self.resources)
    }
}

#[must_use]
pub struct ReadLeaseRetirement<A: Admission> {
    resources: LeaseResources<A>,
    next: usize,
}
impl<A: Admission> ReadLeaseRetirement<A> {
    fn new(resources: LeaseResources<A>) -> Self {
        Self { resources, next: 0 }
    }
    pub fn usage(&self) -> Usage {
        self.resources.usage
    }
    pub fn open_handles(&self) -> usize {
        self.resources.files.iter().filter(|file| file.is_some()).count()
    }
    /// Worker-only, no unlink/paths and no source-store budget release. Uses
    /// the existing store's File-close convention (File destruction has no
    /// fallible Rust close operation). Each close and the final settlement is
    /// separately prepaid; Pending/cancellation retain remaining handles and
    /// the FULL reservation. The exact original aggregate returns only here.
    pub fn step(mut self, mut work: ReadWork, cancel: &AtomicBool) -> ReadLeaseRetirementStep<A> {
        loop {
            match work.debit(0, 1, cancel) {
                Step::Ready(()) => {}
                Step::Pending => return ReadLeaseRetirementStep::Pending(self),
                Step::Cancelled => return ReadLeaseRetirementStep::Cancelled(self),
                Step::Failed(error) => return ReadLeaseRetirementStep::Failed { retirement: self, error },
            }
            if self.next < READ_LEASE_HANDLES {
                drop(self.resources.files[self.next].take());
                self.next += 1;
            } else {
                let mut admission = self.resources.admission.take().expect("unsettled lease admission");
                admission.release(self.resources.usage);
                return ReadLeaseRetirementStep::Ready(admission);
            }
        }
    }
}
#[must_use]
pub enum ReadLeaseRetirementStep<A: Admission> {
    Pending(ReadLeaseRetirement<A>),
    Cancelled(ReadLeaseRetirement<A>),
    Failed { retirement: ReadLeaseRetirement<A>, error: InputError },
    Ready(A),
}

#[cfg(test)]
#[path = "read_tests.rs"]
mod tests;
