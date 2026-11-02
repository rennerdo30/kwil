use super::*;
use std::{borrow::Cow, ops::Range};

/// Maximum allocation of an owned source range. Streaming callers reuse a
/// caller-owned buffer; no source range borrows externally mutable file pages.
pub const READ_CHUNK: usize = 1 << 20;

impl TextFile {
    pub(crate) fn validate_source(&self) -> Result<(), ReadError> {
        match self.check_source().map_err(ReadError::Io)? {
            FileState::Unchanged => Ok(()),
            state => Err(ReadError::Changed(state)),
        }
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
            return Err(ReadError::Io(io::Error::new(io::ErrorKind::InvalidInput, "source range exceeds bounded window")));
        }
        if let Bytes::Owned(bytes) = &self.shared.bytes {
            if let Some(result) = read_batch::admitted(self) { result?; } else { self.validate_source()?; }
            return Ok(Cow::Borrowed(&bytes[range.start as usize..range.end as usize]));
        }
        let mut bytes = vec![0; (range.end - range.start) as usize];
        self.read_into(range.start, &mut bytes)?;
        Ok(Cow::Owned(bytes))
    }
}
