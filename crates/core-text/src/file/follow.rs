use super::*;
use std::hash::{DefaultHasher, Hasher};

impl TextFile {
    /// Cheap append guard retained with each completed index. This samples the
    /// first and final 64 KiB, not every byte in the old prefix. In particular,
    /// truncate-and-regrow is not inferred from length and mtime alone.
    pub(super) fn follow_fingerprint(&self, prefix_len: u64) -> Result<[u64; 2], ReadError> {
        let mut result = [0; 2];
        for (slot, range) in
            result.iter_mut().zip([0..prefix_len.min(64 << 10), prefix_len.saturating_sub(64 << 10)..prefix_len])
        {
            let mut hash = DefaultHasher::new();
            hash.write(&self.read_range(range)?);
            *slot = hash.finish();
        }
        Ok(result)
    }

    /// Worker-only append reopen. Uses positioned reads even for small logs,
    /// verifies identity and retained prefix samples, then extends checkpoints.
    /// Truncation, replacement and changed samples require a fresh open/index.
    pub fn reopen_grown(&self) -> Result<Self, OpenError> {
        let next = Self::open_saved_version(&self.shared.path)?;
        if !matches!(self.shared.stamp.state(&next.shared.stamp), FileState::Grown { .. }) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "file did not only grow; open it afresh").into());
        }
        if let Some(index) = self.index() {
            let expected =
                self.shared.follow_prefix.get().ok_or_else(|| io::Error::other("append guard is not ready"))?;
            if next.follow_fingerprint(self.len()).map_err(io::Error::other)? != *expected {
                return Err(
                    io::Error::new(io::ErrorKind::InvalidInput, "indexed prefix changed; open it afresh").into()
                );
            }
            let mut index = index.clone();
            index.extend_reader(next.len(), self.len(), crate::INDEX_MEMORY_BYTES, |range| {
                let bytes = next.read_range(range).map_err(io::Error::other)?;
                // Count actual index input, including both extension passes and
                // their one-byte newline lookaround, independently of file size.
                next.shared.scanned.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                Ok(bytes)
            })?;
            let guard = next.follow_fingerprint(next.len()).map_err(io::Error::other)?;
            next.validate_source().map_err(io::Error::other)?;
            let _ = next.shared.follow_prefix.set(guard);
            let _ = next.shared.index.set(index);
        }
        Ok(next)
    }
}
