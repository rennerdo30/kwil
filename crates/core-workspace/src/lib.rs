//! Files, sessions, hot exit, file watching, single-instance IPC.

pub mod catalogue;
pub mod compressed;

mod save;
pub mod save_destination;
mod watcher;

pub mod instance;
pub mod journal;
pub mod session;

pub use save::{
    CompressedCheckpointOwners, CompressedSaveRetirement, CompressedSavedSnapshot, EncodedSavedSnapshot, SaveRequest,
    SavedSnapshot, TrimRanges, TrimSpool, open_for_editing, save_compressed_snapshot, save_encoded_snapshot,
    save_snapshot,
};
pub use save_destination::{SaveDestination, paths_may_alias, resolve_save_destination};
pub use watcher::{FileEvent, FileEventKind, FileWatcher};
