//! The bounded sequential-stream adapter used by the actual MSI Binary reader.
//! Keep one 64 KiB window and retain the source cursor across forward misses.
use crate::{Refusal, Result, pe::ReadAt, require};

pub(crate) const CHUNK: usize = 65_536;

pub(crate) trait Sequential {
    fn len(&self) -> u64;
    fn check(&self) -> Result<()>;
    fn read(&mut self, bytes: &mut [u8]) -> Result<usize>;
    /// Reopen the SAME database/table/key, at offset zero. The adapter checks
    /// size again; source implementations must preserve their path/identity.
    fn rewind(&mut self) -> Result<()>;
}

pub(crate) struct Window<S> {
    source: S,
    size: u64,
    position: u64,
    cache: Vec<u8>,
    cache_at: u64,
    cache_len: usize,
    failure: Option<Refusal>,
}

impl<S: Sequential> Window<S> {
    pub(crate) fn new(source: S) -> Result<Self> {
        source.check()?;
        let size = source.len();
        require((1..=16_777_216).contains(&size), "limit", "guard Binary size")?;
        Ok(Self { source, size, position: 0, cache: vec![0; CHUNK], cache_at: u64::MAX, cache_len: 0, failure: None })
    }

    fn advance(&mut self, count: usize) -> Result<()> {
        require((1..=CHUNK).contains(&count), "unstable-evidence", "short stream read")?;
        self.position =
            self.position.checked_add(count as u64).ok_or_else(|| Refusal::new("limit", "stream cursor overflow"))?;
        require(self.position <= self.size, "unstable-evidence", "stream grew")
    }

    fn fill(&mut self, start: u64) -> Result<()> {
        // Invalidate before any fallible read so partially overwritten scratch
        // bytes can never become a cache hit after a failure.
        self.cache_at = u64::MAX;
        self.cache_len = 0;
        if start < self.position {
            self.source.check()?;
            self.source.rewind()?;
            require(self.source.len() == self.size, "unstable-evidence", "Binary stream resized")?;
            self.position = 0;
        }
        while self.position < start {
            self.source.check()?;
            let wanted = (start - self.position).min(CHUNK as u64) as usize;
            let count = self.source.read(&mut self.cache[..wanted])?;
            require(count <= wanted, "unstable-evidence", "stream seek length")?;
            self.advance(count)?;
        }
        let wanted = (self.size - start).min(CHUNK as u64) as usize;
        let mut filled = 0;
        while filled < wanted {
            self.source.check()?;
            let count = self.source.read(&mut self.cache[filled..wanted])?;
            require(count <= wanted - filled, "unstable-evidence", "stream window length")?;
            self.advance(count)?;
            filled += count;
        }
        self.cache_at = start;
        self.cache_len = filled;
        Ok(())
    }

    fn read_window(&mut self, offset: u64, bytes: &mut [u8]) -> Result<()> {
        self.source.check()?;
        require(
            bytes.len() <= CHUNK && offset.checked_add(bytes.len() as u64).is_some_and(|end| end <= self.size),
            "invalid-pe",
            "stream window",
        )?;
        let mut copied = 0;
        while copied < bytes.len() {
            self.source.check()?;
            let at = offset + copied as u64;
            if self.cache_at == u64::MAX || at < self.cache_at || at >= self.cache_at + self.cache_len as u64 {
                self.fill((at / CHUNK as u64) * CHUNK as u64)?;
            }
            let begin = (at - self.cache_at) as usize;
            let count = (bytes.len() - copied).min(self.cache_len - begin);
            bytes[copied..copied + count].copy_from_slice(&self.cache[begin..begin + count]);
            copied += count;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn source(&self) -> &S {
        &self.source
    }
}

impl<S: Sequential> ReadAt for Window<S> {
    fn len(&self) -> u64 {
        self.size
    }

    fn read_at(&mut self, offset: u64, bytes: &mut [u8]) -> Result<()> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        let result = self.read_window(offset, bytes);
        if let Err(error) = &result {
            self.failure = Some(error.clone());
        }
        result
    }
}
