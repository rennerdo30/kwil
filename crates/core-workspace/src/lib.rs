//! Files, sessions, hot exit, file watching, single-instance IPC.

mod save;
mod watcher;

pub mod instance;
pub mod journal;
pub mod session;

pub use save::{SaveRequest, SavedSnapshot, open_for_editing, save_snapshot};
pub use watcher::{FileEvent, FileEventKind, FileWatcher};
