//! Independent public Mach API use; no pthread layout or licensed implementation
//! is copied. The original worker acquires ONE counted send right to itself.
//! XNU 8792/macOS 13 source supplied by ROOT states thread ports do not retain
//! the thread; destruction after TH_TERMINATE2 converts retained rights to dead
//! names. Under the documented no-selfexec/credential-transition contract this
//! is native exit evidence, not a thread_info failure or an invalid-name guess.
use super::{Attempt, NativeExitState, NativeObservationError, Witness as NativeWitness};

const SEND: u32 = 1 << 16;
const DEAD_NAME: u32 = 1 << 20;
/// Non-Copy counted right. State in its containing observation serializes query
/// with explicit deallocation. No destructor closes it, including validation
/// failure; a partial acquisition stays in the original OnceLock record.
#[derive(Debug)]
pub(super) struct Witness {
    name: u32,
}
pub(super) trait Api {
    fn thread_self(&mut self) -> u32;
    fn port_type(&mut self, name: u32) -> Result<u32, i32>;
    fn deallocate(&mut self, name: u32) -> Result<(), i32>;
}
pub(super) fn register_with(api: &mut impl Api) -> Attempt {
    let name = api.thread_self();
    if name == 0 || name == u32::MAX {
        return Attempt { witness: NativeWitness::Missing, error: Some(NativeObservationError::MachAcquire { name }) };
    }
    let error = match api.port_type(name) {
        Ok(bits) if bits & SEND != 0 && bits & DEAD_NAME == 0 => None,
        Ok(bits) => Some(NativeObservationError::MachValidateType { bits }),
        Err(code) => Some(NativeObservationError::MachValidate { code }),
    };
    Attempt { witness: NativeWitness::Mach(Witness { name }), error }
}
pub(super) fn query_with(witness: &Witness, api: &mut impl Api) -> Result<NativeExitState, NativeObservationError> {
    match api.port_type(witness.name) {
        Ok(bits) if bits & SEND != 0 && bits & DEAD_NAME == 0 => Ok(NativeExitState::Pending),
        Ok(bits) if bits & DEAD_NAME != 0 && bits & SEND == 0 => Ok(NativeExitState::Exited),
        Ok(bits) => Err(NativeObservationError::MachQueryType { bits }),
        Err(code) => Err(NativeObservationError::MachQuery { code }),
    }
}
pub(super) fn retire_with(witness: &Witness, api: &mut impl Api) -> Result<(), NativeObservationError> {
    api.deallocate(witness.name).map_err(|code| NativeObservationError::MachRetire { code })
}

#[cfg(target_os = "macos")]
#[link(name = "System")]
unsafe extern "C" {
    fn mach_thread_self() -> u32;
    static mach_task_self_: u32;
    fn mach_port_type(task: u32, name: u32, kind: *mut u32) -> i32;
    fn mach_port_deallocate(task: u32, name: u32) -> i32;
}
#[cfg(target_os = "macos")]
struct System;
#[cfg(target_os = "macos")]
impl Api for System {
    fn thread_self(&mut self) -> u32 {
        // SAFETY: public ABI returns one send right owned by this registration.
        unsafe { mach_thread_self() }
    }
    fn port_type(&mut self, name: u32) -> Result<u32, i32> {
        let mut bits = 0;
        // SAFETY: name is retained in this process; the observation claim
        // excludes concurrent deallocation. The output is a live u32 slot.
        let code = unsafe { mach_port_type(mach_task_self_, name, &mut bits) };
        if code == 0 { Ok(bits) } else { Err(code) }
    }
    fn deallocate(&mut self, name: u32) -> Result<(), i32> {
        // SAFETY: explicit OFF-UI owner retirement after the original join;
        // one exclusive claim, one counted right, success marks it spent.
        let code = unsafe { mach_port_deallocate(mach_task_self_, name) };
        if code == 0 { Ok(()) } else { Err(code) }
    }
}
#[cfg(target_os = "macos")]
pub(super) fn register() -> Attempt {
    register_with(&mut System)
}
#[cfg(target_os = "macos")]
pub(super) fn query(witness: &Witness) -> Result<NativeExitState, NativeObservationError> {
    query_with(witness, &mut System)
}
#[cfg(target_os = "macos")]
pub(super) fn retire(witness: &Witness) -> Result<(), NativeObservationError> {
    retire_with(witness, &mut System)
}
#[cfg(target_os = "macos")]
pub(super) const fn operation_bytes() -> Option<usize> {
    let n = std::mem::size_of::<u32>().checked_mul(3);
    let n = super::add(n, std::mem::size_of::<i32>());
    let n = super::add(n, std::mem::size_of::<&mut System>());
    super::add(n, std::mem::size_of::<Result<u32, i32>>())
}
