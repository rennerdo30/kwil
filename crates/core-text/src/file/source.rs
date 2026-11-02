use super::*;
use std::{borrow::Cow, ops::Range};

/// Maximum allocation of an owned source range. Streaming callers reuse a
/// caller-owned buffer; no source range borrows externally mutable file pages.
pub const READ_CHUNK: usize = 1 << 20;
pub(super) const VIEW_CACHE_BYTES: usize = 96 << 10;

impl TextFile {
    pub(crate) fn validate_source(&self) -> Result<(), ReadError> {
        match self.check_source().map_err(ReadError::Io)? {
            FileState::Unchanged => Ok(()),
            state => Err(ReadError::Changed(state)),
        }
    }

    pub(super) fn cached_range(&self, range: Range<u64>) -> Result<Vec<u8>, ReadError> {
        self.with_cached_range(range, <[u8]>::to_vec)
    }

    /// Consume a validated copied window in place, without a second allocation.
    pub(super) fn with_cached_range<R>(
        &self,
        range: Range<u64>,
        use_bytes: impl FnOnce(&[u8]) -> R,
    ) -> Result<R, ReadError> {
        if let Bytes::Owned(bytes) = &self.shared.bytes {
            self.validate_source()?;
            return Ok(use_bytes(&bytes[range.start as usize..range.end as usize]));
        }
        let mut cache = self
            .shared
            .read_window
            .lock()
            .map_err(|_| ReadError::Io(io::Error::other("source window lock poisoned")))?;
        if range.start >= cache.0 && range.end <= cache.0 + cache.1.len() as u64 {
            self.validate_source()?;
            return Ok(use_bytes(&cache.1[(range.start - cache.0) as usize..(range.end - cache.0) as usize]));
        }
        let end = range.end;
        if end - range.start > VIEW_CACHE_BYTES as u64 {
            return self.read_range(range).map(|b| use_bytes(&b));
        }
        let size = (end - range.start) as usize;
        let extra = size.saturating_sub(cache.1.len());
        cache.1.try_reserve_exact(extra).map_err(|error| ReadError::Io(io::Error::other(error)))?;
        cache.1.resize(size, 0);
        if let Err(error) = self.read_into(range.start, &mut cache.1) {
            cache.1.clear();
            return Err(error);
        }
        cache.0 = range.start;
        Ok(use_bytes(&cache.1))
    }

    pub fn same_source(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }

    /// Fill exactly this bounded caller window. An error invalidates its entire
    /// contents: callers must publish nothing until this operation succeeds.
    pub fn read_into(&self, offset: u64, output: &mut [u8]) -> Result<(), ReadError> {
        let end = offset.checked_add(output.len() as u64).filter(|&end| end <= self.len());
        if end.is_none() {
            return Err(ReadError::Io(io::Error::new(io::ErrorKind::InvalidInput, "source range out of bounds")));
        }
        self.validate_source()?;
        match &self.shared.bytes {
            Bytes::Owned(bytes) => output.copy_from_slice(&bytes[offset as usize..end.unwrap() as usize]),
            _ => {
                let mut done = 0;
                while done < output.len() {
                    #[cfg(unix)]
                    let read = {
                        use std::os::unix::fs::FileExt;
                        self.shared._file.read_at(&mut output[done..], offset + done as u64)
                    };
                    #[cfg(windows)]
                    let read = {
                        use std::os::windows::fs::FileExt;
                        self.shared._file.seek_read(&mut output[done..], offset + done as u64)
                    };
                    match read {
                        Ok(0) => return Err(ReadError::Changed(FileState::Truncated)),
                        Ok(n) => done += n,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) => {
                            self.validate_source()?;
                            return Err(ReadError::Io(e));
                        }
                    }
                }
            }
        }
        // Detect rotation, growth and same-size writes during the read, as
        // well as truncation after a successful final positioned read.
        self.validate_source()
    }

    /// Read at most READ_CHUNK bytes. Only Kwil-owned immutable storage may be
    /// borrowed; external storage is copied through positioned reads.
    pub fn read_range(&self, range: Range<u64>) -> Result<Cow<'_, [u8]>, ReadError> {
        if range.start > range.end || range.end > self.len() || range.end - range.start > READ_CHUNK as u64 {
            return Err(ReadError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "source range exceeds bounded window",
            )));
        }
        if let Bytes::Owned(bytes) = &self.shared.bytes {
            if let Some(result) = read_batch::admitted(self) {
                result?;
            } else {
                self.validate_source()?;
            }
            return Ok(Cow::Borrowed(&bytes[range.start as usize..range.end as usize]));
        }
        let mut bytes = vec![0; (range.end - range.start) as usize];
        self.read_into(range.start, &mut bytes)?;
        Ok(Cow::Owned(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::{DisplaySegment, EncodedFile, Encoding};
    use std::sync::atomic::AtomicBool;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
            std::fs::create_dir_all(&root).unwrap();
            let path = root.join(format!("kw9-source-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn source(&self, bytes: &[u8]) -> TextFile {
            let path = self.0.join("original");
            std::fs::write(&path, bytes).unwrap();
            // Exercise positioned reads with a tiny fixture, independently of
            // the resident threshold. Saved originals have this same policy.
            TextFile::open_saved_version(&path).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn bounded_source_truncate_during_indexing_refuses_partial_result() {
        let fixture = Fixture::new();
        let file = fixture.source(&vec![b'x'; 8 << 20]);
        let truncated = AtomicBool::new(false);
        let progress = AtomicU64::new(0);
        let result = LineIndex::build_reader(
            file.len(),
            DEFAULT_EVERY,
            16 << 20,
            1,
            &progress,
            crate::INDEX_MEMORY_BYTES,
            |range, scratch| {
                scratch.reserve((range.end - range.start) as usize)?;
                scratch.items.resize((range.end - range.start) as usize, 0);
                file.read_into(range.start, &mut scratch.items).map_err(io::Error::other)?;
                let bytes = std::borrow::Cow::Owned(std::mem::take(&mut scratch.items));
                if !truncated.swap(true, Ordering::Relaxed) {
                    // The scanner has already admitted/read its first window.
                    // Shrink while indexing, before the next window is requested.
                    File::options().write(true).open(file.path())?.set_len(1)?;
                }
                Ok(bytes)
            },
        );
        assert!(result.unwrap_err().to_string().contains("file changed on disk"));
        assert!(progress.load(Ordering::Relaxed) < file.len());
        assert!(file.index().is_none());
    }

    #[test]
    fn bounded_source_replacement_while_viewing_refuses_old_object() {
        let fixture = Fixture::new();
        let file = fixture.source(b"old\nbody");
        file.build_index(1).unwrap();
        assert_eq!(file.first_lines(1).unwrap(), vec![b"old".to_vec()]);
        std::fs::rename(file.path(), fixture.0.join("retired")).unwrap();
        std::fs::write(file.path(), b"new\nbody").unwrap();
        assert!(matches!(file.lines(0, 1), Err(ReadError::Changed(FileState::Replaced))));
        assert!(matches!(file.read_range(0..3), Err(ReadError::Changed(FileState::Replaced))));
    }

    fn encode(text: &str, encoding: Encoding) -> Vec<u8> {
        let mut raw = encoding.bom_bytes().to_vec();
        if encoding == Encoding::UTF8 {
            raw.extend_from_slice(text.as_bytes());
        } else if encoding == Encoding::UTF16_LE || encoding == Encoding::UTF16_BE {
            for unit in text.encode_utf16() {
                raw.extend_from_slice(&if encoding == Encoding::UTF16_LE {
                    unit.to_le_bytes()
                } else {
                    unit.to_be_bytes()
                });
            }
        } else {
            for scalar in text.chars() {
                raw.extend_from_slice(&if encoding == Encoding::UTF32_LE {
                    (scalar as u32).to_le_bytes()
                } else {
                    (scalar as u32).to_be_bytes()
                });
            }
        }
        raw
    }

    #[test]
    fn bounded_source_encoding_widths_cross_index_and_payload_windows() {
        for encoding in [Encoding::UTF8, Encoding::UTF16_LE, Encoding::UTF16_BE, Encoding::UTF32_LE, Encoding::UTF32_BE]
        {
            for boundary in [64 << 10, READ_CHUNK] {
                let fixture = Fixture::new();
                let width = if encoding == Encoding::UTF8 {
                    1
                } else if encoding == Encoding::UTF16_LE || encoding == Encoding::UTF16_BE {
                    2
                } else {
                    4
                };
                let prefix = (boundary - encoding.bom_bytes().len()) / width - 1;
                let text = format!("{}🦀\r\nZ", "a".repeat(prefix));
                let raw = encode(&text, encoding);
                let file = fixture.source(&raw);
                let at = encoding.bom_bytes().len() + prefix * width;
                for window in [1, 3, 5] {
                    let mut actual = Vec::new();
                    for start in (at - 3..raw.len()).step_by(window) {
                        let end = (start + window).min(raw.len());
                        let mut bytes = vec![0; end - start];
                        file.read_into(start as u64, &mut bytes).unwrap();
                        actual.extend_from_slice(&bytes);
                    }
                    assert_eq!(actual, raw[at - 3..], "{} window {window}", encoding.name());
                }
                let encoded = EncodedFile::new_with_encoding(file.clone(), encoding).unwrap();
                assert_eq!(encoded.build_index().unwrap().line_count(), 2);
                let crab_bytes = encode("🦀", encoding).len() - encoding.bom_bytes().len();
                let view = encoded.decode_range((at + 1) as u64..(at + crab_bytes - 1) as u64).unwrap();
                assert_eq!(view.source, at as u64..(at + crab_bytes) as u64);
                assert!(!view.had_errors);
                let text: String = view
                    .segments
                    .iter()
                    .map(|segment| match segment {
                        DisplaySegment::Text(text) => text.as_ref(),
                        DisplaySegment::Invalid(_) => panic!("valid scalar became opaque"),
                    })
                    .collect();
                assert_eq!(text, "🦀");
                let buffer = crate::Buffer::from_file(file).unwrap();
                let actual: Vec<u8> =
                    buffer.chunks(0..buffer.len()).unwrap().flat_map(|chunk| chunk.unwrap().into_owned()).collect();
                assert_eq!(actual, raw, "{} payload window", encoding.name());
            }
        }
    }
}
