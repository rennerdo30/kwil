//! Worker-owned immutable comparison inputs, independent of the decoded line index.
//!
//! `begin` never opens the input through `Buffer::from_file`. A source lease is
//! consumed only temporarily: every refusal/terminal build result returns that
//! exact lease. The sealed store owns only its private payload and disk indexes.
//! No file-, line-, or line-length-dependent resident collection is constructed.
//!
//! Integrators must keep builds, stores and retirement owners on the worker.
//! Dropping an owner deliberately does NOT close/unlink private files or release
//! its aggregate reservations. Explicit retirement is the only cleanup path.
//! Paths and source-owned scratch are prepared/admitted by the invocation owner.

mod source;
mod store;

pub use store::read::{
    ReadLease, ReadLeaseRetirement, ReadLeaseRetirementStep, ReadLeaseStep, ReadVisits, read_lease_resident_charge,
};

pub use source::{
    CaptureKind, Captured, DecodedNativeLease, InputIdentity, NativeFileLease, RejectedNative, RejectedSnapshot,
    SnapshotLease, SourceLease,
};
pub use store::{
    AtomLookup, AtomSpan, BuildStep, InputBuild, InputSource, InputStore, LineLookup, LineSpan, PayloadSpace, Rank,
    RankLookup, RejectedSource, Retirement, RetirementStep, StoreAdmission, StorePaths, Terminator, begin,
    resident_charge,
};

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

/// Reuse the engine's existing 64 KiB add/decode bound, not a product size cap.
pub const INPUT_WINDOW: usize = crate::piece_tree::ADD_BLOCK;

/// One caller aggregate budget. Implementations must reserve atomically, before
/// allocation/access; refusal leaves usage unchanged. No local fallback budget.
/// The token itself and source-owned storage remain the invocation's charge.
pub trait Admission {
    fn reserve(&mut self, additional: Usage) -> Result<(), AdmissionError>;
    fn release(&mut self, released: Usage);
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub resident: usize,
    pub disk: u64,
    pub handles: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdmissionError;

/// Bytes include visits/copies/replay, not just successful output bytes.
/// Operations include I/O, bounded decoder calls and source-access units.
/// Compound original checks prepay the cursor's conservative fixed I/O count
/// before access; these are never hidden behind a single operation debit.
/// Exhaustion is a cooperative pause, distinct from admission refusal and OS
/// WouldBlock.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReadWork {
    pub bytes: u64,
    pub operations: u64,
}
impl ReadWork {
    pub const fn new(bytes: u64, operations: u64) -> Self {
        Self { bytes, operations }
    }

    pub fn debit(&mut self, bytes: u64, operations: u64, cancel: &AtomicBool) -> Step<()> {
        if cancel.load(Ordering::Relaxed) {
            return Step::Cancelled;
        }
        if bytes > self.bytes || operations > self.operations {
            return Step::Pending;
        }
        self.bytes -= bytes;
        self.operations -= operations;
        if cancel.load(Ordering::Relaxed) { Step::Cancelled } else { Step::Ready(()) }
    }
}

#[derive(Debug)]
pub enum InputError {
    /// The original error is moved unchanged; raw_os_error and custom payload survive.
    Io(io::Error),
    SourceChanged,
    Range,
    Overflow,
    Admission(AdmissionError),
    SourceContract,
    CorruptStore,
    QueryChanged,
    Provenance(crate::decoded_backing::BackingError),
}
impl From<io::Error> for InputError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
impl From<AdmissionError> for InputError {
    fn from(error: AdmissionError) -> Self {
        Self::Admission(error)
    }
}
impl std::fmt::Display for InputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => error.fmt(f),
            Self::SourceChanged => f.write_str("comparison source changed during invocation capture"),
            Self::Range => f.write_str("comparison coordinate out of range"),
            Self::Overflow => f.write_str("comparison coordinate or capacity overflow"),
            Self::Admission(_) => f.write_str("comparison aggregate admission refused"),
            Self::SourceContract => f.write_str("comparison source violated its capture contract"),
            Self::CorruptStore => f.write_str("invalid comparison disk record"),
            Self::QueryChanged => f.write_str("resume the pending payload read or explicitly abandon it"),
            Self::Provenance(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for InputError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Provenance(error) => Some(error),
            _ => None,
        }
    }
}

#[must_use]
#[derive(Debug)]
pub enum Step<T> {
    Pending,
    Ready(T),
    Cancelled,
    Failed(InputError),
}

#[cfg(test)]
mod tests;
