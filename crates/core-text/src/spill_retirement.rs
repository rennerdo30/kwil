//! Explicit worker leases for immutable added spill blocks. The lease holds
//! each physical block once, independently of persistent tree/root sharing.
//! Collect it before publishing a recovered Buffer to another thread.
use super::*;
use std::{marker::PhantomData, mem::size_of, rc::Rc};

/// Caller policy for collecting leases. References count conservatively before
/// deduplication; metadata includes retained block/path capacity and vector slots.
#[derive(Clone, Copy)]
pub struct SpillRetirementLimits {
    pub max_block_references: usize,
    pub max_metadata_bytes: usize,
    pub max_path_bytes: usize,
}

#[derive(Default)]
pub struct SpillRetirementReport {
    pub retired: usize,
    pub live: usize,
    pub cleanup_pending: usize,
    pub first_error: Option<io::Error>,
}

/// Worker-only handle. Dropping UI snapshots cannot unlink a leased block.
/// Call retire after UI/history/clipboard references have gone. This handle is
/// deliberately !Send, so it cannot accidentally enter a frontend completion.
/// Dropping it leaves leased files behind; there is no implicit unlink fallback.
#[must_use = "retain spill leases on their owning worker and explicitly retire them"]
pub struct SpillRetirement {
    blocks: Vec<Arc<Storage>>,
    metadata_reserved: usize,
    _worker: PhantomData<Rc<()>>,
}

fn check(cancelled: &mut impl FnMut() -> bool) -> io::Result<()> {
    if cancelled() {
        Err(io::Error::new(io::ErrorKind::Interrupted, "spill lease collection cancelled"))
    } else {
        Ok(())
    }
}

fn visit_storage(
    storage: &Arc<Storage>,
    cancelled: &mut impl FnMut() -> bool,
    visit: &mut impl FnMut(&Arc<Storage>) -> io::Result<()>,
) -> io::Result<()> {
    check(cancelled)?;
    match storage.as_ref() {
        Storage::Disk(_) => visit(storage),
        Storage::Opaque { logical, raw, offsets, .. } => {
            for child in [logical, raw, offsets] {
                visit_storage(child, cancelled, visit)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn visit_tree(
    node: &Node,
    cancelled: &mut impl FnMut() -> bool,
    visit: &mut impl FnMut(&Arc<Storage>) -> io::Result<()>,
) -> io::Result<()> {
    check(cancelled)?;
    if node.metrics.resources.spilled_blocks == 0 {
        return Ok(());
    }
    match &node.kind {
        Kind::Leaf(piece) => visit_storage(&piece.storage, cancelled, visit),
        Kind::Branch(left, right) => {
            visit_tree(left, cancelled, visit)?;
            visit_tree(right, cancelled, visit)
        }
    }
}

impl SpillRetirement {
    pub(crate) fn collect(
        tree: &Tree,
        limits: SpillRetirementLimits,
        mut cancelled: impl FnMut() -> bool,
    ) -> io::Result<Self> {
        check(&mut cancelled)?;
        let references = tree.as_ref().map_or(0, |node| node.metrics.resources.spilled_blocks);
        if references > limits.max_block_references {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "spill lease block limit"));
        }
        let mut metadata = references
            .checked_mul(size_of::<Arc<Storage>>())
            .and_then(|bytes| bytes.checked_add(size_of::<Self>()))
            .filter(|bytes| *bytes <= limits.max_metadata_bytes)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "spill lease metadata limit"))?;
        // First pass checks actual retained path allocations before growing the
        // lease vector. It touches only tree/descriptor metadata, never maps.
        let mut observed = 0usize;
        if let Some(root) = tree {
            visit_tree(root, &mut cancelled, &mut |storage| {
                let Storage::Disk(block) = storage.as_ref() else { unreachable!() };
                if block.path.as_os_str().as_encoded_bytes().len() > limits.max_path_bytes {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "spill lease path limit"));
                }
                observed = observed
                    .checked_add(1)
                    .filter(|n| *n <= references)
                    .ok_or_else(|| io::Error::other("spill lease resource count mismatch"))?;
                metadata = metadata
                    .checked_add(storage.retained_bytes())
                    .filter(|bytes| *bytes <= limits.max_metadata_bytes)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "spill lease metadata limit"))?;
                Ok(())
            })?;
        }
        if observed != references {
            return Err(io::Error::other("spill lease resource count mismatch"));
        }
        let mut blocks = Vec::new();
        blocks.try_reserve_exact(references).map_err(io::Error::other)?;
        if let Some(root) = tree {
            visit_tree(root, &mut cancelled, &mut |storage| {
                blocks.push(storage.clone());
                Ok(())
            })?;
        }
        check(&mut cancelled)?;
        blocks.sort_unstable_by_key(|storage| Arc::as_ptr(storage) as usize);
        blocks.dedup_by(|a, b| Arc::ptr_eq(a, b));
        check(&mut cancelled)?;
        // From this point onward, even an unexpected lease/worker drop must
        // leave unlinking to an explicit worker cleanup, never a UI last-drop.
        for storage in &blocks {
            let Storage::Disk(block) = storage.as_ref() else { unreachable!() };
            block.explicit_retirement.store(true, Ordering::Relaxed);
        }
        Ok(Self { blocks, metadata_reserved: metadata, _worker: PhantomData })
    }

    pub fn retained(&self) -> usize {
        self.blocks.len()
    }
    pub fn metadata_reserved(&self) -> usize {
        self.metadata_reserved
    }

    /// One explicit worker pass. Uniqueness is checked on individual physical
    /// blocks, so shared/split/concatenated roots cannot hide live UI references.
    /// Failed cleanup retains the closed block and exact path for another pass.
    pub fn retire(&mut self) -> SpillRetirementReport {
        self.retire_with(|block| {
            block.map.take();
            block.file.take();
            match std::fs::remove_file(&block.path) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
                _ => {
                    #[cfg(unix)]
                    {
                        match std::fs::remove_dir(block.path.parent().unwrap()) {
                            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
                            _ => Ok(()),
                        }
                    }
                    #[cfg(not(unix))]
                    {
                        Ok(())
                    }
                }
            }
        })
    }

    fn retire_with(&mut self, mut cleanup: impl FnMut(&mut DiskBlock) -> io::Result<()>) -> SpillRetirementReport {
        let mut report = SpillRetirementReport::default();
        let mut at = 0;
        while at < self.blocks.len() {
            let Some(storage) = Arc::get_mut(&mut self.blocks[at]) else {
                report.live += 1;
                at += 1;
                continue;
            };
            let Storage::Disk(block) = storage else { unreachable!() };
            let result = cleanup(block);
            match result {
                Ok(()) => {
                    self.blocks.swap_remove(at);
                    report.retired += 1;
                }
                Err(error) => {
                    report.cleanup_pending += 1;
                    if report.first_error.is_none() {
                        report.first_error = Some(error);
                    }
                    at += 1;
                }
            }
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Buffer;

    fn limits() -> SpillRetirementLimits {
        SpillRetirementLimits { max_block_references: 64, max_metadata_bytes: 1 << 20, max_path_bytes: 4096 }
    }

    #[test]
    fn spill_lease_keeps_frontend_last_drop_off_filesystem() {
        let mut buffer = Buffer::with_add_memory_limit(0);
        buffer.insert(0, b"retained recovered bytes").unwrap();
        let snapshot = buffer.snapshot();
        let paths = spill_paths(&snapshot.root);
        let mut lease = SpillRetirement::collect(&snapshot.root, limits(), || false).unwrap();
        assert_eq!(lease.retained(), 1);
        assert_eq!(lease.retire().live, 1);
        std::thread::spawn(move || drop((buffer, snapshot))).join().unwrap();
        assert!(paths.iter().all(|path| path.exists()), "frontend drop must leave every leased file intact");
        assert_eq!(lease.retire().retired, 1);
        assert!(paths.iter().all(|path| !path.exists()));
        assert_eq!(lease.retained(), 0);
    }

    #[test]
    fn spill_lease_drop_never_hands_unlink_back_to_frontend() {
        let mut buffer = Buffer::with_add_memory_limit(0);
        buffer.insert(0, b"surviving frontend").unwrap();
        let snapshot = buffer.snapshot();
        let paths = spill_paths(&snapshot.root);
        let lease = SpillRetirement::collect(&snapshot.root, limits(), || false).unwrap();
        drop(lease); // Simulate worker shutdown before the frontend drops.
        std::thread::spawn(move || drop((buffer, snapshot))).join().unwrap();
        assert!(paths.iter().all(|path| path.exists()), "worker exit cannot re-enable UI unlink");
        // Test-owned exact paths; cleanup remains explicit on this worker.
        for path in paths {
            std::fs::remove_file(&path).unwrap();
            #[cfg(unix)]
            std::fs::remove_dir(path.parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn spill_lease_deduplicates_physical_blocks_across_shared_roots() {
        let mut buffer = Buffer::with_add_memory_limit(0);
        buffer.insert(0, b"abcdef").unwrap();
        let snapshot = buffer.snapshot();
        let combined = snapshot.slice(0..3).unwrap().concat(&snapshot).unwrap();
        let mut lease = SpillRetirement::collect(&combined.root, limits(), || false).unwrap();
        assert_eq!(lease.retained(), 1, "one physical block needs exactly one worker pin");
        drop((buffer, snapshot));
        assert_eq!(lease.retire().live, 1);
        drop(combined);
        assert_eq!(lease.retire().retired, 1);
        assert_eq!(lease.retained(), 0);
    }

    #[test]
    fn spill_lease_limits_and_cancellation_precede_publication() {
        let mut buffer = Buffer::with_add_memory_limit(0);
        buffer.insert(0, b"abcdef").unwrap();
        let snapshot = buffer.snapshot();
        for policy in [
            SpillRetirementLimits { max_block_references: 0, ..limits() },
            SpillRetirementLimits { max_metadata_bytes: 0, ..limits() },
            SpillRetirementLimits { max_path_bytes: 0, ..limits() },
        ] {
            assert!(SpillRetirement::collect(&snapshot.root, policy, || false).is_err());
        }
        let error = SpillRetirement::collect(&snapshot.root, limits(), || true).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        let mut polls = 0;
        let error = SpillRetirement::collect(&snapshot.root, limits(), || {
            polls += 1;
            polls == 5
        })
        .err()
        .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(polls, 5);
        let mut lease = SpillRetirement::collect(&snapshot.root, limits(), || false).unwrap();
        drop((buffer, snapshot));
        assert_eq!(lease.retire().retired, 1);
    }

    #[test]
    fn spill_lease_retains_failed_cleanup_and_retries_exact_block() {
        let mut buffer = Buffer::with_add_memory_limit(0);
        buffer.insert(0, b"retry").unwrap();
        let snapshot = buffer.snapshot();
        let paths = spill_paths(&snapshot.root);
        let mut lease = SpillRetirement::collect(&snapshot.root, limits(), || false).unwrap();
        drop((buffer, snapshot));
        let report = lease.retire_with(|block| {
            assert_eq!(block.path, paths[0]);
            Err(io::ErrorKind::PermissionDenied.into())
        });
        assert_eq!(report.cleanup_pending, 1);
        assert_eq!(report.first_error.unwrap().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(lease.retained(), 1);
        assert!(paths[0].exists());
        assert_eq!(lease.retire().retired, 1);
        assert!(!paths[0].exists());
    }

    #[test]
    fn spill_lease_covers_all_three_opaque_storage_blocks() {
        let buffer = Buffer::with_add_memory_limit(0);
        let offsets: Vec<u8> = [0u32, 1, 2].into_iter().flat_map(u32::to_le_bytes).collect();
        let root = buffer.recovery_opaque(Encoding::UTF8, b"\xff\xfe", &offsets, 2).unwrap();
        let snapshot = Buffer::recovery_snapshot(root);
        let paths = spill_paths(&snapshot.root);
        let mut lease = SpillRetirement::collect(&snapshot.root, limits(), || false).unwrap();
        assert_eq!(lease.retained(), 3);
        std::thread::spawn(move || drop((buffer, snapshot))).join().unwrap();
        assert!(paths.iter().all(|path| path.exists()));
        assert_eq!(lease.retire().retired, 3);
        assert!(paths.iter().all(|path| !path.exists()));
    }
}
