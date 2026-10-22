//! Buffer (piece tree over mmap/pread), line index, cursors, undo, encoding.
//!
//! M0 scope: open any file without a full pass, show the first screen from
//! raw bytes, build a sparse line index in the background, and read any
//! line range once the index is ready. Editing (the piece tree) is M2.

mod file;
mod line_index;

pub use file::{IndexProgress, TextFile};
pub use line_index::LineIndex;
