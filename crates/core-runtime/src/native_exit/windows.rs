use super::{NativeExitQueryError, NativeExitState, QueryResult};
use std::os::windows::io::RawHandle;
use std::time::{Duration, Instant};
use windows_sys::Win32::{
    Foundation::{GetLastError, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::Threading::WaitForSingleObject,
};

/// Private synchronous adapter: no ownership/close/duplicate/join operations.
/// Tests substitute only query effects, never the original worker handle.
pub(super) trait QueryApi {
    fn wait(&mut self, handle: RawHandle, timeout_ms: u32) -> u32;
    fn last_error(&mut self) -> u32;
}
pub(super) struct SystemQuery;
impl QueryApi for SystemQuery {
    fn wait(&mut self, handle: RawHandle, timeout_ms: u32) -> u32 {
        // SAFETY: the public entry borrows a live JoinHandle throughout this
        // call; std owns the native thread handle. This adapter neither closes
        // it nor transfers ownership. Query uses zero; deadline wait uses a
        // finite, recomputed timeout that can NEVER equal INFINITE.
        unsafe { WaitForSingleObject(handle, timeout_ms) }
    }
    fn last_error(&mut self) -> u32 {
        // SAFETY: reads this calling thread's last-error scalar, immediately
        // after WAIT_FAILED, before any other OS call or formatting operation.
        unsafe { GetLastError() }
    }
}

/// Actual stack-only query values; the raw handle is borrowed metadata, not a
/// new native owner or source cell. repr(C) includes its target alignment pad.
#[repr(C)]
struct Scratch {
    handle: RawHandle,
    status: u32,
    error: u32,
}
pub(super) const fn scratch_bytes() -> Option<usize> {
    std::mem::size_of::<Scratch>().checked_add(std::mem::size_of::<&mut SystemQuery>())
}

pub(super) fn query_with(handle: RawHandle, api: &mut impl QueryApi) -> QueryResult {
    let mut scratch = Scratch { handle, status: 0, error: 0 };
    scratch.status = api.wait(scratch.handle, 0);
    match scratch.status {
        WAIT_OBJECT_0 => Ok(NativeExitState::Exited),
        WAIT_TIMEOUT => Ok(NativeExitState::Pending),
        WAIT_FAILED => {
            // Do not insert logging, formatting, allocation or another native
            // call between the wait failure and capture of its original code.
            scratch.error = api.last_error();
            Err(NativeExitQueryError::WindowsWaitFailed { code: scratch.error })
        }
        status => Err(NativeExitQueryError::UnexpectedWindowsWait { status }),
    }
}

/// Declared Rust-value carrier, not a resident header or new native owner.
#[repr(C)]
struct DeadlineScratch {
    native: Scratch,
    deadline: Instant,
    sampled: Instant,
    remaining: Duration,
    milliseconds: u128,
    timeout_ms: u32,
    result: QueryResult,
}
const fn add(left: Option<usize>, right: usize) -> Option<usize> {
    match left {
        Some(left) => left.checked_add(right),
        None => None,
    }
}
pub(super) const fn wait_scratch_bytes() -> Option<usize> {
    let n = std::mem::size_of::<DeadlineScratch>().checked_add(std::mem::size_of::<RawHandle>());
    let n = add(n, std::mem::size_of::<Instant>()); // deadline argument
    let n = add(n, std::mem::size_of::<Instant>()); // clock return overlap
    let n = add(n, std::mem::size_of::<&mut SystemQuery>());
    let n = add(n, std::mem::size_of::<RawHandle>()); // wait adapter argument
    let n = add(n, std::mem::size_of::<u32>()); // wait timeout argument
    let n = add(n, std::mem::size_of::<u32>()); // wait/status return overlap
    let n = add(n, std::mem::size_of::<u32>()); // error return overlap
    let n = add(n, std::mem::size_of::<u128>()); // ceil addition return overlap
    let n = add(n, std::mem::size_of::<u128>()); // cap return overlap
    let n = add(n, std::mem::size_of::<Duration>()); // backoff min result
    let n = add(n, std::mem::size_of::<Duration>()); // sleep argument
    let n = add(n, std::mem::size_of::<QueryResult>()); // match/return overlap
    add(n, std::mem::size_of::<QueryResult>()) // caller return overlap
}

pub(super) fn wait_until_with(
    handle: RawHandle,
    deadline: Instant,
    api: &mut impl QueryApi,
    mut now: impl FnMut() -> Instant,
    mut sleep: impl FnMut(Duration),
) -> QueryResult {
    let mut scratch = DeadlineScratch {
        native: Scratch { handle, status: 0, error: 0 },
        deadline,
        sampled: now(),
        remaining: Duration::ZERO,
        milliseconds: 0,
        timeout_ms: 0,
        result: Ok(NativeExitState::Pending),
    };
    loop {
        // Preflight every native operation against the SAME absolute Instant.
        if scratch.sampled >= scratch.deadline {
            return Ok(NativeExitState::Pending);
        }
        scratch.remaining = scratch.deadline.saturating_duration_since(scratch.sampled);
        // Round UP fractional milliseconds, then cap below INFINITE. A capped
        // wait may continue, but cannot renew the original absolute deadline.
        scratch.milliseconds = scratch
            .remaining
            .as_millis()
            .saturating_add(u128::from(!scratch.remaining.subsec_nanos().is_multiple_of(1_000_000)));
        scratch.timeout_ms = scratch.milliseconds.min(u128::from(u32::MAX - 1)) as u32;
        scratch.native.status = api.wait(scratch.native.handle, scratch.timeout_ms);
        scratch.result = match scratch.native.status {
            WAIT_OBJECT_0 => Ok(NativeExitState::Exited),
            WAIT_TIMEOUT => Ok(NativeExitState::Pending),
            WAIT_FAILED => {
                // Capture BEFORE the clock, backoff or any other native call.
                scratch.native.error = api.last_error();
                Err(NativeExitQueryError::WindowsWaitFailed { code: scratch.native.error })
            }
            status => Err(NativeExitQueryError::UnexpectedWindowsWait { status }),
        };
        scratch.sampled = now(); // after EVERY result, including fresh errors
        match scratch.result {
            Err(_) => return scratch.result,
            Ok(_) if scratch.sampled >= scratch.deadline => return Ok(NativeExitState::Pending),
            Ok(NativeExitState::Exited) => return scratch.result,
            Ok(NativeExitState::Pending) => {}
        }
        // Only actual WAIT_TIMEOUT can continue: representability cap or an
        // early return. Passive backoff prevents an early-timeout busy loop.
        scratch.remaining = scratch.deadline.saturating_duration_since(scratch.sampled);
        sleep(scratch.remaining.min(Duration::from_millis(1)));
        scratch.sampled = now();
    }
}

#[cfg(test)]
mod deadline_tests;
