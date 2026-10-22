//! Worker-created deletion offsets, never a resident per-line edit vector.
use super::*;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::sync::Arc;

#[derive(Clone)]
pub struct TrimSpool(Arc<Spool>);
struct Spool {
    writer: Option<BufWriter<File>>,
    temporary: Temporary,
    count: u64,
}
impl TrimSpool {
    pub(super) fn new() -> io::Result<Self> {
        let mut temporary = Temporary::new(&std::env::temp_dir())?;
        let writer = Some(BufWriter::with_capacity(64 << 10, temporary.file.take().unwrap()));
        Ok(Self(Arc::new(Spool { writer, temporary, count: 0 })))
    }
    pub(super) fn push(&mut self, range: Range<u64>) -> io::Result<()> {
        let spool = Arc::get_mut(&mut self.0).expect("unpublished trim spool");
        let file = spool.writer.as_mut().unwrap();
        file.write_all(&range.start.to_le_bytes())?;
        file.write_all(&range.end.to_le_bytes())?;
        spool.count += 1;
        Ok(())
    }
    pub(super) fn seal(&mut self) -> io::Result<()> {
        let spool = Arc::get_mut(&mut self.0).expect("unpublished trim spool");
        if let Some(writer) = &mut spool.writer {
            writer.flush()?;
        }
        spool.writer.take();
        Ok(())
    }
    /// Exact original-coordinate deletions in ascending order. Worker only:
    /// each item reads sixteen bytes; the spool is unlinked on last drop.
    pub fn ranges(&self) -> io::Result<TrimRanges> {
        Ok(TrimRanges {
            _owner: self.clone(),
            file: File::open(&self.0.temporary.path)?,
            front: 0,
            back: self.0.count,
            count: self.0.count,
            cache_block: None,
            cache: vec![0; 64 << 10],
        })
    }
    pub fn len(&self) -> u64 {
        self.0.count
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
pub struct TrimRanges {
    _owner: TrimSpool,
    file: File,
    front: u64,
    back: u64,
    count: u64,
    cache_block: Option<u64>,
    cache: Vec<u8>,
}
impl TrimRanges {
    fn read(&mut self, at: u64) -> io::Result<Range<u64>> {
        let block = at / 4096;
        if self.cache_block != Some(block) {
            let first = block * 4096;
            let len = (self.count - first).min(4096) as usize * 16;
            self.file.seek(SeekFrom::Start(first * 16))?;
            self.file.read_exact(&mut self.cache[..len])?;
            self.cache_block = Some(block);
        }
        let start = (at % 4096) as usize * 16;
        let bytes = &self.cache[start..start + 16];
        Ok(u64::from_le_bytes(bytes[..8].try_into().unwrap())..u64::from_le_bytes(bytes[8..].try_into().unwrap()))
    }
}
impl Iterator for TrimRanges {
    type Item = io::Result<Range<u64>>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.front == self.back {
            return None;
        }
        let at = self.front;
        self.front += 1;
        Some(self.read(at))
    }
}
impl DoubleEndedIterator for TrimRanges {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.front == self.back {
            return None;
        }
        self.back -= 1;
        Some(self.read(self.back))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn trim_spool_offsets_are_lazy_reversible_and_removed_on_last_drop() {
        let mut spool = TrimSpool::new().unwrap();
        let path = spool.0.temporary.path.clone();
        spool.push(3..9).unwrap();
        spool.push(20..21).unwrap();
        spool.seal().unwrap();
        assert_eq!(spool.len(), 2);
        assert_eq!(fs::metadata(&path).unwrap().len(), 32);
        let owner = spool.clone();
        let mut ranges = spool.ranges().unwrap();
        drop(spool);
        drop(owner);
        assert!(path.exists());
        assert_eq!(ranges.next_back().unwrap().unwrap(), 20..21);
        assert_eq!(ranges.next().unwrap().unwrap(), 3..9);
        assert!(ranges.next().is_none());
        drop(ranges);
        assert!(!path.exists(), "last iterator owns cleanup, too");
    }
}
