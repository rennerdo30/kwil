//! One nonconsuming native-exit observation of an existing std thread handle.
//!
//! Rust main-function completion (`JoinHandle::is_finished`) can precede native
//! TLS destruction and actual thread exit. It is not used as exit evidence here.
use std::{fmt, thread::JoinHandle};

#[cfg(windows)]
mod windows;

/// A native observation only; successful settlement still requires the owner
/// to join its original handle and retain any original panic payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeExitState {
    Pending,
    Exited,
}
impl fmt::Display for NativeExitState {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(match self {
            Self::Pending => "pending",
            Self::Exited => "exited",
        })
    }
}

/// Compact original query evidence. No io::Error, String, Box or source owner
/// is constructed on failure. UnsupportedTarget is an isolated capability gap,
/// not an approved replacement for healthy Unix worker settlement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeExitQueryError {
    WindowsWaitFailed { code: u32 },
    UnexpectedWindowsWait { status: u32 },
    UnsupportedTarget,
}
impl fmt::Display for NativeExitQueryError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WindowsWaitFailed { code } => write!(out, "native thread wait failed (Win32 code {code})"),
            Self::UnexpectedWindowsWait { status } => write!(out, "unexpected native thread wait status {status}"),
            Self::UnsupportedTarget => out.write_str("native thread exit query is unsupported on this target"),
        }
    }
}
impl std::error::Error for NativeExitQueryError {}

type QueryResult = Result<NativeExitState, NativeExitQueryError>;

/// Observe once without consuming, closing, duplicating or joining `thread`.
/// Windows performs ONE zero-timeout kernel wait. No allocation, extra thread,
/// source cell, timer, retry loop or polling is introduced.
///
/// Pending and query failure leave the exact handle with its original owner.
/// Only Exited permits that owner to proceed with its existing join protocol,
/// after rechecking its original deadline. This does not establish a hard
/// real-time bound on scheduling, native calls or the owner's remaining work.
///
/// Non-Windows returns an explicit isolated gap. ROOT must NOT integrate that
/// branch into healthy Linux/macOS workers before viable backends are reviewed.
pub fn native_exit_state<T>(thread: &JoinHandle<T>) -> Result<NativeExitState, NativeExitQueryError> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        windows::query_with(thread.as_raw_handle(), &mut windows::SystemQuery)
    }
    #[cfg(not(windows))]
    {
        let _ = thread;
        Err(NativeExitQueryError::UnsupportedTarget)
    }
}

/// Internal Windows deadline adapter for the registered observation. Native
/// ownership stays in std's original JoinHandle throughout every finite wait.
#[cfg(windows)]
pub(crate) fn wait_until<T>(thread: &JoinHandle<T>, deadline: std::time::Instant) -> QueryResult {
    use std::os::windows::io::AsRawHandle;
    windows::wait_until_with(
        thread.as_raw_handle(),
        deadline,
        &mut windows::SystemQuery,
        std::time::Instant::now,
        std::thread::sleep,
    )
}

#[cfg(windows)]
pub(crate) const fn wait_stack_bytes() -> Option<usize> {
    let n = std::mem::size_of::<&JoinHandle<()>>().checked_add(std::mem::size_of::<std::time::Instant>());
    let n = match n {
        Some(n) => n.checked_add(std::mem::size_of::<QueryResult>()),
        None => None,
    };
    match (n, windows::wait_scratch_bytes()) {
        (Some(n), Some(scratch)) => n.checked_add(scratch),
        _ => None,
    }
}

/// Checked declared Rust-value admission, in BYTES, for one query: the borrowed
/// JoinHandle reference, two query-result headers which can overlap on return,
/// and, on Windows, the actual scalar scratch record + adapter reference.
///
/// This is NOT a compiler stack-frame measurement, native stack/unwind bound,
/// allocator charge or heap header: the helper allocates no heap storage. The
/// caller separately admits its own widened reason/status/return owners through
/// its existing aggregate budget. Numeric target evaluation belongs to ROOT.
pub const fn native_exit_query_stack_bytes() -> Option<usize> {
    let Some(returns) = std::mem::size_of::<QueryResult>().checked_mul(2) else { return None };
    let Some(common) = std::mem::size_of::<&JoinHandle<()>>().checked_add(returns) else { return None };
    #[cfg(windows)]
    {
        let Some(scratch) = windows::scratch_bytes() else { return None };
        common.checked_add(scratch)
    }
    #[cfg(not(windows))]
    {
        Some(common)
    }
}

#[cfg(test)]
mod tests;
