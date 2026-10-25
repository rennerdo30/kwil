//! Buffer (piece tree over mmap/pread), line index, cursors, undo, encoding.
//!
//! Mapped originals, sparse indexing and persistent byte-exact editing.

mod buffer;
pub mod compare_input;
pub mod decoded_backing;
mod decoded_piece;
pub mod encoding;
mod file;
mod line_index;
mod piece_tree;
mod recovery;
mod undo;
pub mod worker_edit;

pub use buffer::{Buffer, Lines, Snapshot, SnapshotImporter};
pub use file::with_checked_read_batch;
pub use file::{FileKind, FileState, IndexProgress, OpenError, ReadError, TextFile};
pub use file::{OriginalSavePin, cleanup_stale_save_backups};
pub use line_index::LineIndex;
pub use piece_tree::Chunks;
pub use piece_tree::worker_layout;
pub use piece_tree::{SourceChunk, SourceChunks, StorageResources};
pub use piece_tree::{SpillRetirement, SpillRetirementLimits, SpillRetirementReport};
pub use recovery::{RecoveryAssembler, RecoveryLimits, RecoveryRevision, RecoveryUsage};

pub use piece_tree::worker_source::{
    WorkerSourceChunk, WorkerSourceCursor, WorkerSourceError, WorkerSourceKind, WorkerSourceStartError,
    WorkerSourceStep,
};
