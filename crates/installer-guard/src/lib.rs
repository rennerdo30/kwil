//! Read-only schema-1 MSI preflight. No application dependency or package-specific
//! build input belongs here: Binary.KwilPreflight is itself a BuildID input.
#![cfg(any(windows, test))]
#![deny(unsafe_op_in_unsafe_fn)]

mod identity;
mod model;
mod pe;
mod policy;
#[cfg(any(windows, test))]
mod stream;
mod tables;
#[cfg(any(windows, test))]
mod token_access;
#[cfg(windows)]
mod windows;

use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, Refusal>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Refusal {
    code: &'static str,
    resource: String,
    instruction: &'static str,
}

impl Refusal {
    fn new(code: &'static str, resource: impl Into<String>) -> Self {
        let instruction = match code {
            "in-use" => "Close Kwil and retry.",
            "foreign-resource" | "portable-marker" => "Move the existing resource manually or use the portable ZIP.",
            "changed-owned-file" | "changed-owned-value" | "changed-owned-key" => {
                "Preserve your changes manually and restore the original resource before retrying."
            }
            "repair-required" => "Use the retained original MSI with REINSTALL=ALL REINSTALLMODE=amus.",
            "package-mismatch" => "Use the retained original MSI for repair or uninstall.",
            "schema-unsupported" => "Use the matching original installer.",
            "version-conflict" => "Use a strictly higher version or uninstall the original first.",
            "architecture-conflict" => "Uninstall the original architecture before switching architecture.",
            "unsafe-path" | "unsafe-registry" => {
                "Preserve the resource, remove the path/link redirection manually, or use the portable ZIP."
            }
            "registration-context" | "token-context" => {
                "Run the original per-user MSI as the registered standard user without elevation."
            }
            _ => {
                "Retry with the original unmodified MSI; if this persists, preserve the resources and report this reason."
            }
        };
        Self { code, resource: resource.into(), instruction }
    }
}

pub(crate) fn require(ok: bool, code: &'static str, resource: impl Into<String>) -> Result<()> {
    if ok { Ok(()) } else { Err(Refusal::new(code, resource)) }
}

pub(crate) struct Deadline(Instant);
impl Deadline {
    fn new() -> Self {
        Self(Instant::now())
    }
    fn check(&self) -> Result<()> {
        require(self.0.elapsed() < Duration::from_millis(30_000), "deadline", "30-second preflight limit")
    }
}

/// # Safety
/// Called only by Windows Installer with a live, borrowed MSIHANDLE. The handle
/// is u32 on both supported architectures and is never closed by this function.
#[cfg(windows)]
#[unsafe(no_mangle)]
pub unsafe extern "system" fn KwilPreflightV1(session: u32) -> u32 {
    // Expected errors are Results. catch_unwind also prevents an accidental
    // unwind in an unwind-enabled diagnostic build crossing the system ABI.
    // ROOT must still build the production DLL with abort + static CRT.
    let result = std::panic::catch_unwind(|| windows::preflight(session));
    match result {
        Ok(Ok(())) => 0,
        Ok(Err(reason)) => {
            windows::report(session, &reason);
            1603
        }
        Err(_) => {
            windows::report(session, &Refusal::new("internal-error", "unexpected panic"));
            1603
        }
    }
}

#[cfg(test)]
#[path = "../tests/policy.rs"]
mod tests;

#[cfg(test)]
#[path = "../tests/stream.rs"]
mod stream_tests;
