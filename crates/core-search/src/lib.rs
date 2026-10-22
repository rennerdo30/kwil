//! Bounded, synchronous worker-side search. Frontends own worker lifetimes and
//! wake their event loop from callbacks/channels; nothing here runs on a UI timer.
//!
//! Offsets refer to the immutable source snapshot's original bytes. Encoding
//! adapters can implement [`Haystack`] separately. Search does not decode files.

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
