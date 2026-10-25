//! Inline original-worker registration and native exit observation.
//!
//! The owner embeds this value in its EXISTING Shared/terminal allocation and
//! prepays resident_bytes plus operation_stack_bytes in that same admission.
//! Register exactly once INSIDE the original worker, before its source work.
//! The process must not self-exec or transition credentials while it lives.
//! PID checks reject a forked process; PID equality alone cannot detect exec.
use crate::{NativeExitQueryError, NativeExitState};
use std::{
    fmt,
    sync::{
        OnceLock,
        atomic::{AtomicU8, Ordering},
    },
    thread::{self, JoinHandle, ThreadId},
};

#[cfg(any(target_os = "linux", test))]
mod linux;
#[cfg(any(target_os = "macos", test))]
mod macos;

const UNREGISTERED: u8 = 0;
const REGISTERING: u8 = 1;
const REGISTRATION_FAILED: u8 = 2;
const REGISTERED: u8 = 3;
const QUERYING: u8 = 4;
const RETIRING: u8 = 5;
const RETIRED: u8 = 6;

/// Original scalar evidence: pthread functions return their code directly;
/// clock_gettime errno is captured immediately, Mach returns kern_return_t.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeObservationError {
    Unregistered,
    Registering,
    RegistrationFailed,
    AlreadyRegistered,
    Busy,
    ForeignThread,
    ForeignProcess,
    Retired,
    UnsupportedTarget,
    WindowsQuery(NativeExitQueryError),
    LinuxRegister { code: i32 },
    LinuxValidate { errno: i32 },
    LinuxQuery { errno: i32 },
    MachAcquire { name: u32 },
    MachValidate { code: i32 },
    MachValidateType { bits: u32 },
    MachQuery { code: i32 },
    MachQueryType { bits: u32 },
    MachRetire { code: i32 },
}
impl fmt::Display for NativeObservationError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WindowsQuery(error) => fmt::Display::fmt(error, out),
            Self::LinuxRegister { code } => write!(out, "thread clock registration failed (code {code})"),
            Self::LinuxValidate { errno } => write!(out, "thread clock validation failed (errno {errno})"),
            Self::LinuxQuery { errno } => write!(out, "thread clock query failed (errno {errno})"),
            Self::MachAcquire { name } => write!(out, "invalid acquired Mach thread right {name}"),
            Self::MachValidate { code } => write!(out, "Mach thread right validation failed (code {code})"),
            Self::MachValidateType { bits } => write!(out, "invalid registered Mach right type {bits}"),
            Self::MachQuery { code } => write!(out, "Mach thread right query failed (code {code})"),
            Self::MachQueryType { bits } => write!(out, "unexpected Mach thread right type {bits}"),
            Self::MachRetire { code } => write!(out, "Mach thread right retirement failed (code {code})"),
            other => out.write_str(match other {
                Self::Unregistered => "native exit observation is unregistered",
                Self::Registering => "native exit registration is in progress",
                Self::RegistrationFailed => "native exit registration did not complete",
                Self::AlreadyRegistered => "native exit registration is one-shot",
                Self::Busy => "native exit observation is busy",
                Self::ForeignThread => "native exit observation belongs to another thread",
                Self::ForeignProcess => "native exit observation belongs to another process",
                Self::Retired => "native exit observation is retired",
                Self::UnsupportedTarget => "native exit observation is unsupported on this target",
                _ => unreachable!(),
            }),
        }
    }
}
impl std::error::Error for NativeObservationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::WindowsQuery(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Identity {
    thread: ThreadId,
    process: u32,
}
impl Identity {
    fn current() -> Self {
        Self { thread: thread::current().id(), process: std::process::id() }
    }
}
#[derive(Debug)]
enum Witness {
    // Windows registration acquires no fallible native resource; this variant
    // still belongs to the common failed-registration/mock representation.
    #[cfg_attr(windows, allow(dead_code))]
    Missing,
    #[cfg(windows)]
    Windows,
    #[cfg(any(target_os = "linux", test))]
    Linux(linux::Witness),
    #[cfg(any(target_os = "macos", test))]
    Mach(macos::Witness),
    #[cfg(test)]
    Mock(u32),
}
struct Attempt {
    witness: Witness,
    error: Option<NativeObservationError>,
}
#[derive(Debug)]
struct Registration {
    identity: Identity,
    witness: Witness,
    error: Option<NativeObservationError>,
}

/// Non-Copy counted native ownership lives inline here, never in a new Arc.
/// There is deliberately NO native I/O from Drop. Dropping an unretired Mach
/// registration quarantines/leaks its single send/dead-name right until process
/// exit; it is misuse, not successful cleanup. The original owner must retain
/// this value on failure and explicitly retire OFF UI after successful join.
pub struct NativeExitObservation {
    state: AtomicU8,
    registration: OnceLock<Registration>,
    first_retirement_error: OnceLock<NativeObservationError>,
}
impl Default for NativeExitObservation {
    fn default() -> Self {
        Self::new()
    }
}
impl NativeExitObservation {
    pub const fn new() -> Self {
        Self {
            state: AtomicU8::new(UNREGISTERED),
            registration: OnceLock::new(),
            first_retirement_error: OnceLock::new(),
        }
    }
    pub fn register_current(&self) -> Result<(), NativeObservationError> {
        self.register_with(Identity::current(), register_native)
    }
    pub fn query<T>(&self, handle: &JoinHandle<T>) -> Result<NativeExitState, NativeObservationError> {
        self.query_with(handle.thread().id(), std::process::id(), |witness| query_native(witness, handle))
    }
    /// OFF UI, only AFTER the original owner successfully joined its original
    /// handle. This API cannot itself prove that external join; the owner must
    /// enforce that precondition. Partial failed registration is still retired.
    /// Successful repeats do no second close; failed close keeps its exact name
    /// and first native retirement code for an explicit retry.
    pub fn retire_after_join(&self) -> Result<(), NativeObservationError> {
        self.retire_with(std::process::id(), retire_native)
    }
    pub fn first_retirement_error(&self) -> Option<NativeObservationError> {
        self.first_retirement_error.get().copied()
    }
    /// Exact target size of inline Self, BYTES; no new heap/Arc header exists.
    /// The caller checks addition to its existing aggregate before allocation.
    pub const fn resident_bytes() -> usize {
        std::mem::size_of::<Self>()
    }

    /// Checked declared Rust-value scratch + return overlap, BYTES. This is
    /// intentionally conservative across registration/query/retirement, not a
    /// compiler stack-frame/native stack bound or a heap allocation estimate.
    pub const fn operation_stack_bytes() -> Option<usize> {
        let n = Some(std::mem::size_of::<Attempt>());
        let n = add(n, std::mem::size_of::<Registration>());
        let n = add(n, std::mem::size_of::<Claim<'static>>());
        let n = add(n, std::mem::size_of::<Identity>());
        let n = add(n, std::mem::size_of::<thread::Thread>());
        let n = add(n, std::mem::size_of::<&JoinHandle<()>>());
        let n = add(n, std::mem::size_of::<Result<NativeExitState, NativeObservationError>>());
        let n = add(n, std::mem::size_of::<Result<NativeExitState, NativeObservationError>>());
        let n = add(n, std::mem::size_of::<Result<(), NativeObservationError>>());
        let n = add(n, std::mem::size_of::<Result<(), NativeObservationError>>());
        #[cfg(windows)]
        let n = match crate::native_exit_query_stack_bytes() {
            Some(bytes) => add(n, bytes),
            None => None,
        };
        #[cfg(target_os = "linux")]
        let n = match linux::operation_bytes() {
            Some(bytes) => add(n, bytes),
            None => None,
        };
        #[cfg(target_os = "macos")]
        let n = match macos::operation_bytes() {
            Some(bytes) => add(n, bytes),
            None => None,
        };
        n
    }

    fn register_with(
        &self,
        identity: Identity,
        register: impl FnOnce() -> Attempt,
    ) -> Result<(), NativeObservationError> {
        self.state.compare_exchange(UNREGISTERED, REGISTERING, Ordering::AcqRel, Ordering::Acquire).map_err(
            |state| {
                if state == REGISTERING {
                    NativeObservationError::Registering
                } else {
                    NativeObservationError::AlreadyRegistered
                }
            },
        )?;
        let mut claim = Claim { state: &self.state, restore: REGISTRATION_FAILED };
        let attempt = register();
        let result = attempt.error.map_or(Ok(()), Err);
        let record = Registration { identity, witness: attempt.witness, error: attempt.error };
        // The single claim and no reset/take/get_mut API guarantee ONE set.
        // This cannot wait for another initializer: there is no other writer.
        self.registration.set(record).expect("single registration claim publishes once");
        claim.restore = if result.is_ok() { REGISTERED } else { REGISTRATION_FAILED };
        result
    }
    fn query_with(
        &self,
        thread: ThreadId,
        process: u32,
        query: impl FnOnce(&Witness) -> Result<NativeExitState, NativeObservationError>,
    ) -> Result<NativeExitState, NativeObservationError> {
        let state = self.state.load(Ordering::Acquire);
        check_available(state)?;
        let record = self.registration.get().ok_or(NativeObservationError::RegistrationFailed)?;
        if record.identity.process != process {
            return Err(NativeObservationError::ForeignProcess);
        }
        if record.identity.thread != thread {
            return Err(NativeObservationError::ForeignThread);
        }
        if state == RETIRED {
            return Err(NativeObservationError::Retired);
        }
        if let Some(error) = record.error {
            return Err(error);
        }
        self.state
            .compare_exchange(REGISTERED, QUERYING, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| NativeObservationError::Busy)?;
        let _claim = Claim { state: &self.state, restore: REGISTERED };
        query(&record.witness)
    }
    fn retire_with(
        &self,
        process: u32,
        retire: impl FnOnce(&Witness) -> Result<(), NativeObservationError>,
    ) -> Result<(), NativeObservationError> {
        let state = self.state.load(Ordering::Acquire);
        check_available(state)?;
        let record = self.registration.get().ok_or(NativeObservationError::RegistrationFailed)?;
        if record.identity.process != process {
            return Err(NativeObservationError::ForeignProcess);
        }
        if state == RETIRED {
            return Ok(());
        }
        self.state
            .compare_exchange(state, RETIRING, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| NativeObservationError::Busy)?;
        let mut claim = Claim { state: &self.state, restore: state };
        match retire(&record.witness) {
            Ok(()) => {
                claim.restore = RETIRED;
                Ok(())
            }
            Err(error) => {
                // This claim is the only writer; .get is always nonblocking.
                if self.first_retirement_error.get().is_none() {
                    let _ = self.first_retirement_error.set(error);
                }
                Err(self.first_retirement_error.get().copied().unwrap_or(error))
            }
        }
    }
}
fn check_available(state: u8) -> Result<(), NativeObservationError> {
    match state {
        UNREGISTERED => Err(NativeObservationError::Unregistered),
        REGISTERING => Err(NativeObservationError::Registering),
        QUERYING | RETIRING => Err(NativeObservationError::Busy),
        REGISTERED | REGISTRATION_FAILED | RETIRED => Ok(()),
        _ => Err(NativeObservationError::RegistrationFailed),
    }
}
struct Claim<'a> {
    state: &'a AtomicU8,
    restore: u8,
}
impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.state.store(self.restore, Ordering::Release);
    }
}
const fn add(left: Option<usize>, right: usize) -> Option<usize> {
    match left {
        Some(left) => left.checked_add(right),
        None => None,
    }
}
fn register_native() -> Attempt {
    #[cfg(windows)]
    {
        Attempt { witness: Witness::Windows, error: None }
    }
    #[cfg(target_os = "linux")]
    {
        linux::register()
    }
    #[cfg(target_os = "macos")]
    {
        macos::register()
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        Attempt { witness: Witness::Missing, error: Some(NativeObservationError::UnsupportedTarget) }
    }
}
fn query_native<T>(witness: &Witness, handle: &JoinHandle<T>) -> Result<NativeExitState, NativeObservationError> {
    let _ = handle;
    match witness {
        #[cfg(windows)]
        Witness::Windows => crate::native_exit_state(handle).map_err(NativeObservationError::WindowsQuery),
        #[cfg(target_os = "linux")]
        Witness::Linux(witness) => linux::query(witness),
        #[cfg(target_os = "macos")]
        Witness::Mach(witness) => macos::query(witness),
        _ => Err(NativeObservationError::RegistrationFailed),
    }
}
fn retire_native(witness: &Witness) -> Result<(), NativeObservationError> {
    #[cfg(target_os = "macos")]
    if let Witness::Mach(witness) = witness {
        return macos::retire(witness);
    }
    let _ = witness;
    Ok(())
}

#[cfg(test)]
mod tests;
