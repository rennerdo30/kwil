//! Files, sessions, hot exit, file watching, single-instance IPC.

pub mod catalogue;

mod save;
mod watcher;

pub use save::{SaveRequest, SavedSnapshot, open_for_editing, save_snapshot};
pub use watcher::{FileEvent, FileEventKind, FileWatcher};
