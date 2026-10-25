//! Original public-API use, not copied libc/kernel implementation. Cache the
//! exact per-thread clock ID returned while pthread_self() is known live.
//!
//! Linux 5.4/current source ordering supplied by ROOT: clock_gettime's eligible
//! per-thread PID lookup disappears after exit_mm/clear_child_tid/futex wake,
//! then exit_notify/release_task/PID detach. Thus validated EINVAL authorizes
//! join only under that ordering, same-process/no-selfexec contract and original
//! identity binding. Reused TIDs can conservatively remain Pending. No general
//! POSIX or unreviewed-kernel guarantee is asserted here.
use super::{Attempt, NativeExitState, NativeObservationError, Witness as NativeWitness};

#[cfg(target_os = "linux")]
type ClockId = libc::clockid_t;
#[cfg(not(target_os = "linux"))]
type ClockId = i32; // Private mock adapter only; no Unix SDK needed on Windows.
const INVALID_CLOCK: i32 = 22; // Linux UAPI EINVAL; not an arbitrary errno-as-exit rule.

#[derive(Debug)]
pub(super) struct Witness {
    clock: ClockId,
}
pub(super) trait Api {
    fn current_thread_clock(&mut self) -> Result<ClockId, i32>;
    fn sample(&mut self, clock: ClockId) -> Result<(), i32>;
}
pub(super) fn register_with(api: &mut impl Api) -> Attempt {
    let clock = match api.current_thread_clock() {
        Ok(clock) => clock,
        Err(code) => {
            return Attempt {
                witness: NativeWitness::Missing,
                error: Some(NativeObservationError::LinuxRegister { code }),
            };
        }
    };
    let error = api.sample(clock).err().map(|errno| NativeObservationError::LinuxValidate { errno });
    Attempt { witness: NativeWitness::Linux(Witness { clock }), error }
}
pub(super) fn query_with(witness: &Witness, api: &mut impl Api) -> Result<NativeExitState, NativeObservationError> {
    match api.sample(witness.clock) {
        Ok(()) => Ok(NativeExitState::Pending),
        Err(INVALID_CLOCK) => Ok(NativeExitState::Exited),
        Err(errno) => Err(NativeObservationError::LinuxQuery { errno }),
    }
}

#[cfg(target_os = "linux")]
struct System;
#[cfg(target_os = "linux")]
impl Api for System {
    fn current_thread_clock(&mut self) -> Result<ClockId, i32> {
        let mut clock = 0;
        // SAFETY: this executes in the original worker while pthread_self is
        // live. The output is libc's exact opaque clockid_t, never re-encoded.
        let code = unsafe { libc::pthread_getcpuclockid(libc::pthread_self(), &mut clock) };
        if code == 0 { Ok(clock) } else { Err(code) }
    }
    fn sample(&mut self, clock: ClockId) -> Result<(), i32> {
        let mut value = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: output points to a valid timespec. Native failure errno is
        // read immediately before formatting or any subsequent native call.
        if unsafe { libc::clock_gettime(clock, &mut value) } == 0 {
            Ok(())
        } else {
            Err(unsafe { *libc::__errno_location() })
        }
    }
}
#[cfg(target_os = "linux")]
pub(super) fn register() -> Attempt {
    register_with(&mut System)
}
#[cfg(target_os = "linux")]
pub(super) fn query(witness: &Witness) -> Result<NativeExitState, NativeObservationError> {
    query_with(witness, &mut System)
}
#[cfg(target_os = "linux")]
pub(super) const fn operation_bytes() -> Option<usize> {
    let n = std::mem::size_of::<libc::timespec>().checked_add(std::mem::size_of::<ClockId>());
    let n = super::add(n, std::mem::size_of::<&mut System>());
    super::add(n, std::mem::size_of::<Result<ClockId, i32>>())
}
