//! Bounded, synchronous worker-side search. Frontends own worker lifetimes and
//! wake their event loop from callbacks/channels; nothing here runs on a UI timer.
//!
//! Offsets refer to the immutable source snapshot's original bytes. Encoding
//! adapters can implement [`Haystack`] separately. In-buffer search does not
//! decode files; find-in-files decodes BOM-marked UTF-16 LE/BE in bounded lines
//! and translates hits back to original-file byte offsets.
//!
//! Finite regexes derive their window overlap from their maximum match length
//! (up to 1 MiB), independent of file or line size. Unbounded regexes require
//! the complete source to fit the caller's input cap (UI default 64 KiB), or
//! return an actionable limit error before emitting hits. See [`Pattern::regex`].

mod files;
mod filter;
mod parallel;
mod search;
mod stream_regex;
pub mod worker;
pub mod worker_spool;

pub use stream_regex::StreamRegex;

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
