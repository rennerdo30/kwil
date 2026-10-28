//! Access-mask and effective-token seam used by the actual Windows adapter.
//! Non-null SHGetKnownFolderPath tokens need QUERY and IMPERSONATE together.
use crate::Result;

const TOKEN_QUERY_RIGHT: u32 = 0x0008;
const TOKEN_IMPERSONATE_RIGHT: u32 = 0x0004;
pub(crate) const KNOWN_FOLDER_TOKEN_ACCESS: u32 = TOKEN_QUERY_RIGHT | TOKEN_IMPERSONATE_RIGHT;

/// Open both candidates with the same minimum rights. A thread token remains
/// authoritative; only the adapter's explicit ERROR_NO_TOKEN becomes None.
pub(crate) fn effective_token<T>(
    open_process: impl FnOnce(u32) -> Result<T>,
    open_thread: impl FnOnce(u32) -> Result<Option<T>>,
) -> Result<T> {
    let process = open_process(KNOWN_FOLDER_TOKEN_ACCESS)?;
    match open_thread(KNOWN_FOLDER_TOKEN_ACCESS)? {
        Some(thread) => Ok(thread),
        None => Ok(process),
    }
}
