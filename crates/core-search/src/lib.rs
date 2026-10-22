//! Bounded, synchronous worker-side search. Frontends own worker lifetimes and
//! wake their event loop from callbacks/channels; nothing here runs on a UI timer.
//!
//! Offsets refer to the immutable source snapshot's original bytes. Encoding
//! adapters can implement [`Haystack`] separately. In-buffer search does not
//! decode files; find-in-files decodes BOM-marked UTF-16 LE/BE in bounded lines
//! and translates hits back to original-file byte offsets.
//!
//! Regex windowing requires every match to fit the caller's max-match bound.
//! A longer match may be missed silently across window borders; only detected
//! overlong matches return errors. Callers must not assume every bound violation
//! errors. Prefer the line-oriented [`find_in_files`] mode when completeness
//! matters: it searches complete lines and reports lines exceeding its heap cap.

mod files;
mod filter;
mod parallel;
mod search;

pub use files::{FileEvent, FileHit, FileSearch, FileSearchSummary, find_in_files, worker_count};
pub use filter::{FilterExpr, FilterResume, FilterSummary, LineFilter, LineRun, LineRuns};
pub use parallel::{search_default, search_with_threads};
pub use search::{Edit, Haystack, Match, Options, Pattern, Progress, SearchMode, SearchSummary, plan_replace, search};

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// Set to true to stop. Callbacks may set this token too.
pub type Cancellation = Arc<AtomicBool>;

#[cfg(test)]
mod tests;
