use super::{NativeExitQueryError, NativeExitState, QueryResult};
use std::os::windows::io::RawHandle;
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
        // it nor transfers ownership. The query passes a zero timeout.
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
