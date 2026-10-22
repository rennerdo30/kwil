//! Files, sessions, hot exit, file watching, single-instance IPC.

mod watcher;

pub use watcher::{FileEvent, FileEventKind, FileWatcher};
