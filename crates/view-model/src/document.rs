//! Document identity and byte-preserving storage. Construction and save I/O
//! belong on workers; the editor only mutates an already indexed buffer.

use kwil_core_text::{Buffer, Snapshot, TextFile};
use kwil_core_text::{
    decoded_backing::DecodedSnapshot,
    encoding::{Encoding, SavePlan},
};
use std::{io, path::PathBuf};

/// Output encoding is independent of the immutable origins held by pieces.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DocumentEncoding {
    #[default]
    Utf8,
    Encoded(Encoding),
}

impl DocumentEncoding {
    pub fn encoding(self) -> Encoding {
        match self {
            Self::Utf8 => Encoding::UTF8,
            Self::Encoded(encoding) => encoding,
        }
    }
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
    /// Output transport is independent of encoding and immutable decoded origins.
    /// Retained even when recovery cannot reattach a verified named original.
    pub compression: kwil_core_workspace::compressed::CompressionFormat,
    pub encoding: DocumentEncoding,
    pub bom: bool,
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
            compression: kwil_core_workspace::compressed::CompressionFormat::Raw,
            encoding: DocumentEncoding::Utf8,
            bom: false,
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
        Self::attach(path, file, buffer)
    }

    /// Worker entry point for a prepared lossless logical backing. The worker
    /// retains its DecodedBacking owner until all snapshots have retired.
    /// `file` remains the external raw destination identity, never edit bytes.
    pub fn from_decoded_file(path: PathBuf, file: TextFile, origin: DecodedSnapshot) -> io::Result<Self> {
        Self::from_decoded_file_cancellable(path, file, origin, || false)
    }

    pub fn from_decoded_file_cancellable(
        path: PathBuf,
        file: TextFile,
        origin: DecodedSnapshot,
        mut cancelled: impl FnMut() -> bool,
    ) -> io::Result<Self> {
        let interpretation = origin.interpretation();
        let buffer = Buffer::from_decoded_cancellable(origin, &mut cancelled)?;
        let mut document = Self::attach(path, file, buffer)?;
        if cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "decoded document indexing cancelled"));
        }
        document.encoding = if interpretation.encoding == Encoding::UTF8 {
            DocumentEncoding::Utf8
        } else {
            DocumentEncoding::Encoded(interpretation.encoding)
        };
        document.bom = interpretation.bom_len != 0;
        Ok(document)
    }

    /// Capture with the immutable save request. None preserves mixed endings;
    /// the preferred Enter key ending is separate from a conversion request.
    pub fn save_plan(&self) -> SavePlan {
        SavePlan { encoding: self.encoding.encoding(), bom: self.bom, line_ending: None }
    }

    fn attach(path: PathBuf, file: TextFile, buffer: Buffer) -> io::Result<Self> {
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

    /// Reserve an identity absent from the undo chain and skip it for future
    /// edits. A guessed `revision + 1` can be the very next editing revision.
    pub(crate) fn mark_unsaved(&mut self) {
        let next = self.next_revision.checked_add(1).expect("document revision identity exhausted");
        self.saved_revision = self.next_revision;
        self.next_revision = next;
    }

    pub fn set_undo_memory_limit(&mut self, bytes: usize) {
        self.buffer.set_undo_memory_limit(bytes);
    }

    pub(crate) fn changed(&mut self) {
        self.revision = self.next_revision;
        self.next_revision += 1;
    }
}

#[cfg(test)]
mod encoding_tests {
    use super::*;
    use kwil_core_text::decoded_backing::{DecodedBacking, Interpretation};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    #[test]
    fn decoded_document_edits_logical_utf8_and_preserves_utf16_output_policy() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "kwil-document-codec-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("source.txt");
        let raw = b"\xff\xfeA\0\r\0\n\0";
        std::fs::write(&path, raw).unwrap();
        let file = TextFile::open(&path).unwrap();
        let backing = DecodedBacking::prepare(
            &file,
            Interpretation::new(Encoding::UTF16_LE, 2).unwrap(),
            &directory,
            &AtomicBool::new(false),
        )
        .unwrap();
        let mut document = Document::from_decoded_file(path.clone(), file, backing.snapshot()).unwrap();
        assert_eq!(document.line_ending, DocumentLineEnding::CrLf);
        assert_eq!(document.snapshot().len(), 3);
        assert_eq!(document.encoding, DocumentEncoding::Encoded(Encoding::UTF16_LE));
        assert!(document.bom);
        assert_eq!(document.original.as_ref().unwrap().path(), path);
        let plan = document.save_plan();
        document.buffer.insert(1, b"x").unwrap();
        let mut saved = Vec::new();
        kwil_core_text::encoding::write_snapshot(&document.snapshot(), plan, &mut saved).unwrap();
        assert_eq!(saved, b"\xff\xfeA\0x\0\r\0\n\0");
        document.encoding = DocumentEncoding::Utf8;
        document.bom = false;
        assert_eq!(plan.encoding, Encoding::UTF16_LE, "captured request policy must not change");
        assert_eq!(document.save_plan().line_ending, None, "preferred Enter ending must not normalize mixed text");
        drop(document);
        assert!(backing.try_retire().is_ok());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
