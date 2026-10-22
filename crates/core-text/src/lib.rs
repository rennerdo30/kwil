//! Buffer (piece tree over mmap/pread), line index, cursors, undo, encoding.
//!
//! Mapped originals, sparse indexing and persistent byte-exact editing.

mod buffer;
mod file;
mod line_index;
mod piece_tree;
mod undo;

pub use buffer::{Buffer, Lines, Snapshot};
pub use file::{IndexProgress, TextFile};
pub use line_index::LineIndex;
pub use piece_tree::Chunks;
