//! Document identity and byte-preserving storage. Construction and save I/O
//! belong on workers; the editor only mutates an already indexed buffer.

use kwil_core_text::{Buffer, Snapshot, TextFile};
use std::{io, path::PathBuf};

/// Encoding integration seam for KW-10. No implicit decoding or lossy save.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DocumentEncoding {
    #[default]
    Utf8,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DocumentLineEnding {
    #[default]
    Lf,
    CrLf,
}

impl DocumentLineEnding {
    pub fn bytes(self) -> &'static [u8] {
        match self {
            Self::Lf => b"\n",
            Self::CrLf => b"\r\n",
        }
    }
}

pub struct Document {
    pub(crate) buffer: Buffer,
    pub(crate) original: Option<TextFile>,
    pub(crate) path: Option<PathBuf>,
    pub encoding: DocumentEncoding,
    pub line_ending: DocumentLineEnding,
    pub(crate) revision: u64,
    pub(crate) saved_revision: u64,
    pub(crate) next_revision: u64,
}

impl Default for Document {
    fn default() -> Self {
        Self::new()
    }
}

impl Document {
    pub fn new() -> Self {
        Self {
            buffer: Buffer::empty(),
            original: None,
            path: None,
            encoding: DocumentEncoding::Utf8,
            line_ending: DocumentLineEnding::Lf,
            revision: 0,
            saved_revision: 0,
            next_revision: 1,
        }
    }

    /// Worker entry point. Buffer attachment builds the index if necessary.
    /// Newline sniffing examines at most 64 KiB; existing bytes, including
    /// mixed endings and invalid UTF-8, remain untouched by editing and save.
    pub fn from_file(path: PathBuf, file: TextFile) -> io::Result<Self> {
        kwil_core_text::cleanup_stale_save_backups(&path)?;
        Self::from_indexed_file(path, file)
    }

    pub(crate) fn from_indexed_file(path: PathBuf, file: TextFile) -> io::Result<Self> {
        let buffer = Buffer::from_file(file.clone()).map_err(io::Error::other)?;
        let mut ending = DocumentLineEnding::Lf;
        let snapshot = buffer.snapshot();
        let mut previous = 0;
        'sample: for chunk in snapshot.chunks(0..snapshot.len().min(64 << 10))? {
            for &byte in chunk.map_err(io::Error::other)? {
                if byte == b'\n' {
                    if previous == b'\r' {
                        ending = DocumentLineEnding::CrLf;
                    }
                    break 'sample;
                }
                previous = byte;
            }
        }
        Ok(Self { buffer, original: Some(file), path: Some(path), line_ending: ending, ..Self::new() })
    }

    pub fn buffer(&self) -> &Buffer {
        &self.buffer
    }
    pub fn snapshot(&self) -> Snapshot {
        self.buffer.snapshot()
    }
    pub fn path(&self) -> Option<&std::path::Path> {
        self.path.as_deref()
    }
    pub fn is_dirty(&self) -> bool {
        self.revision != self.saved_revision
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn saved_revision(&self) -> u64 {
        self.saved_revision
    }

    pub fn set_undo_memory_limit(&mut self, bytes: usize) {
        self.buffer.set_undo_memory_limit(bytes);
    }

    pub(crate) fn changed(&mut self) {
        self.revision = self.next_revision;
        self.next_revision += 1;
    }
}
