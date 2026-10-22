//! Files, sessions, hot exit, file watching, single-instance IPC.

mod save;
mod watcher;

pub use save::{SaveRequest, SavedSnapshot, TrimRanges, TrimSpool, open_for_editing, save_snapshot};
pub use watcher::{FileEvent, FileEventKind, FileWatcher};
