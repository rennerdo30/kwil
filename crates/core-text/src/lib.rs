//! Buffer (piece tree over mmap/pread), line index, cursors, undo, encoding.
//!
//! Mapped originals, sparse indexing and persistent byte-exact editing.

mod buffer;
pub mod decoded_backing;
pub mod encoding;
mod file;
mod line_index;
mod piece_tree;
mod undo;

pub use buffer::{Buffer, Lines, Snapshot};
pub use file::with_checked_read_batch;
pub use file::{FileKind, FileState, IndexProgress, OpenError, ReadError, TextFile};
pub use file::{OriginalSavePin, cleanup_stale_save_backups};
pub use line_index::LineIndex;
pub use piece_tree::Chunks;
pub use piece_tree::{SourceChunk, SourceChunks};
