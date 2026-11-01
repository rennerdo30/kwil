//! Allocation accounting shared by the temporary and final sparse indexes.
use std::{
    io,
    mem::size_of,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

pub const INDEX_MEMORY_BYTES: usize = 64 << 20;

pub(crate) struct IndexMemory {
    limit: usize,
    used: AtomicUsize,
}
impl IndexMemory {
    pub(crate) fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self { limit, used: AtomicUsize::new(0) })
    }
    pub(crate) fn error(&self) -> io::Error {
        io::Error::other(format!(
            "Line index exceeds the {}-byte memory limit; indexing is unavailable for this file",
            self.limit
        ))
    }
    fn charge(&self, bytes: usize) -> io::Result<()> {
        self.used
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                used.checked_add(bytes).filter(|&n| n <= self.limit)
            })
            .map(|_| ())
            .map_err(|_| self.error())
    }
}

pub(crate) struct IndexVec<T> {
    pub(crate) items: Vec<T>,
    memory: Arc<IndexMemory>,
    charged: usize,
}
impl<T> IndexVec<T> {
    pub(crate) fn new(memory: &Arc<IndexMemory>) -> Self {
        Self { items: Vec::new(), memory: memory.clone(), charged: 0 }
    }
    pub(crate) fn reserve(&mut self, length: usize) -> io::Result<()> {
        if length <= self.items.capacity() {
            return Ok(());
        }
        let capacity = length.max(self.items.capacity().saturating_mul(2)).max(4);
        let bytes = capacity.checked_mul(size_of::<T>()).ok_or_else(|| self.memory.error())?;
        // Reallocation may hold the old and new buffers simultaneously.
        let charge = bytes;
        self.memory.charge(charge)?;
        if let Err(error) = self.items.try_reserve_exact(capacity - self.items.len()) {
            self.memory.used.fetch_sub(charge, Ordering::Relaxed);
            return Err(io::Error::other(error));
        }
        self.memory.used.fetch_sub(self.charged, Ordering::Relaxed);
        self.charged = bytes;
        Ok(())
    }
    pub(crate) fn push(&mut self, value: T) -> io::Result<()> {
        self.reserve(self.items.len().checked_add(1).ok_or_else(|| self.memory.error())?)?;
        self.items.push(value);
        Ok(())
    }
    pub(crate) fn into_vec(mut self) -> Vec<T> {
        std::mem::take(&mut self.items)
    }
}
impl<T> Drop for IndexVec<T> {
    fn drop(&mut self) {
        drop(std::mem::take(&mut self.items));
        self.memory.used.fetch_sub(self.charged, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn index_memory_refuses_before_growing_and_reclaims_temporary_charge() {
        let memory = IndexMemory::new(64);
        let mut a = IndexVec::<(u64, u64)>::new(&memory);
        a.push((0, 0)).unwrap();
        let capacity = a.items.capacity();
        let mut b = IndexVec::<(u64, u64)>::new(&memory);
        let error = b.push((1, 1)).unwrap_err();
        assert!(error.to_string().contains("64-byte memory limit"));
        assert_eq!(b.items.capacity(), 0, "refuse before allocating");
        assert_eq!(a.items.capacity(), capacity);
        drop(a);
        b.push((1, 1)).unwrap();
        drop(b);
        assert_eq!(memory.used.load(Ordering::Relaxed), 0);
    }
}
