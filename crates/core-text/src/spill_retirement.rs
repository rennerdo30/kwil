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

struct PendingClaims<'a> {
    blocks: &'a [Arc<Storage>],
    claimed: usize,
}

/// Only operation-ledger allocations are passed here; NEVER a composed root.
/// Memory children are skipped, and rollback clears only successfully acquired
/// new Disk claims. Existing recovery overlap/PendingClaims remains unchanged.
pub(super) fn claim_worker_allocations(
    blocks: &[Arc<Storage>],
    cancelled: &mut impl FnMut() -> bool,
) -> io::Result<()> {
    struct Attempt<'a> {
        blocks: &'a [Arc<Storage>],
        scanned: usize,
    }
    impl Drop for Attempt<'_> {
        fn drop(&mut self) {
            for storage in &self.blocks[..self.scanned] {
                if let Storage::Disk(block) = storage.as_ref() {
                    block.retirement_leased.store(false, Ordering::Release);
                }
            }
        }
    }
    let mut attempt = Attempt { blocks, scanned: 0 };
    for storage in blocks {
        check(cancelled)?;
        if let Storage::Disk(block) = storage.as_ref() {
            if !block.explicit_retirement.load(Ordering::Acquire) {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "untracked worker allocation"));
            }
            if block.retirement_leased.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "worker spill claim overlaps an existing lease",
                ));
            }
        }
        attempt.scanned += 1;
    }
    check(cancelled)?;
    attempt.scanned = 0;
    Ok(())
}
impl Drop for PendingClaims<'_> {
    fn drop(&mut self) {
        for storage in &self.blocks[..self.claimed] {
            let Storage::Disk(block) = storage.as_ref() else { unreachable!() };
            block.retirement_leased.store(false, Ordering::Release);
        }
    }
}

impl Drop for SpillRetirement {
    fn drop(&mut self) {
        for storage in &self.blocks {
            let Storage::Disk(block) = storage.as_ref() else { unreachable!() };
            block.retirement_leased.store(false, Ordering::Release);
        }
        // The permanent explicit-retirement flag still prevents UI unlink.
    }
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
    pub(crate) fn covers_worker_storage(&self, storage: &Arc<Storage>) -> bool {
        self.blocks.iter().any(|owned| Arc::ptr_eq(owned, storage))
    }
    pub(crate) fn worker_disk_bytes(&self) -> io::Result<u64> {
        self.blocks.iter().try_fold(0_u64, |total, storage| {
            let Storage::Disk(block) = storage.as_ref() else { unreachable!() };
            total
                .checked_add(super::worker_disk_charge(block.payload_len as u64)?)
                .ok_or_else(crate::worker_edit::overflow)
        })
    }
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
        let mut claims = PendingClaims { blocks: &blocks, claimed: 0 };
        for storage in &blocks {
            check(&mut cancelled)?;
            let Storage::Disk(block) = storage.as_ref() else { unreachable!() };
            let already_leased =
                block.retirement_leased.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err();
            if already_leased {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "spill block already has a retirement lease"));
            }
            claims.claimed += 1;
        }
        // From this point onward, even an unexpected lease/worker drop must
        // leave unlinking to an explicit worker cleanup, never a UI last-drop.
        for storage in &blocks {
            let Storage::Disk(block) = storage.as_ref() else { unreachable!() };
            block.explicit_retirement.store(true, Ordering::Relaxed);
        }
        claims.claimed = 0;
        drop(claims);
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
    use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};

    fn limits() -> SpillRetirementLimits {
        SpillRetirementLimits { max_block_references: 64, max_metadata_bytes: 1 << 20, max_path_bytes: 4096 }
    }

    struct PhysicalPair {
        _buffer: Buffer,
        full: crate::Snapshot,
        low: crate::Snapshot,
        high: crate::Snapshot,
        low_storage: Arc<Storage>,
        high_storage: Arc<Storage>,
    }
    impl PhysicalPair {
        fn new() -> Self {
            fn physical(snapshot: &crate::Snapshot) -> Arc<Storage> {
                let Kind::Leaf(piece) = &snapshot.root.as_ref().unwrap().kind else { panic!("one physical block") };
                assert!(matches!(piece.storage.as_ref(), Storage::Disk(_)));
                piece.storage.clone()
            }
            let mut buffer = Buffer::with_add_memory_limit(0);
            buffer.insert(0, b"a").unwrap();
            buffer.insert(1, b"b").unwrap();
            let full = buffer.snapshot();
            let first = full.slice(0..1).unwrap();
            let second = full.slice(1..2).unwrap();
            let mut blocks = [(first.clone(), physical(&first)), (second.clone(), physical(&second))];
            // Collection claims in physical address order, independent of the
            // logical order or allocation order of these two Added blocks.
            blocks.sort_unstable_by_key(|(_, storage)| Arc::as_ptr(storage) as usize);
            let [(low, low_storage), (high, high_storage)] = blocks;
            assert!((Arc::as_ptr(&low_storage) as usize) < Arc::as_ptr(&high_storage) as usize);
            Self { _buffer: buffer, full, low, high, low_storage, high_storage }
        }
        fn leased(storage: &Arc<Storage>) -> bool {
            let Storage::Disk(block) = storage.as_ref() else { unreachable!() };
            block.retirement_leased.load(Ordering::Acquire)
        }
        fn low_leased(&self) -> bool {
            Self::leased(&self.low_storage)
        }
        fn high_leased(&self) -> bool {
            Self::leased(&self.high_storage)
        }
        fn reacquire_prefix(&self) {
            let prefix = SpillRetirement::collect(&self.low.root, limits(), || false).unwrap();
            assert_eq!(prefix.retained(), 1, "the successfully claimed fresh prefix must be reacquirable");
            drop(prefix);
            assert!(!self.low_leased());
        }
    }

    fn with_physical_pair(assertions: impl FnOnce(&PhysicalPair) -> SpillRetirement) {
        let pair = PhysicalPair::new();
        let paths = spill_paths(&pair.full.root);
        let outcome = catch_unwind(AssertUnwindSafe(|| assertions(&pair)));
        // Release every source/history/inspection pin before worker cleanup,
        // including after a deliberately broken rollback assertion unwinds.
        drop(pair);
        let outcome = outcome.and_then(|mut complete| {
            catch_unwind(AssertUnwindSafe(|| {
                assert_eq!(complete.retained(), 2);
                assert_eq!(complete.retire().retired, 2);
                assert_eq!(complete.retained(), 0);
                assert!(paths.iter().all(|path| !path.exists()));
            }))
        });
        let cleanup = paths.iter().try_for_each(|path| -> io::Result<()> {
            match std::fs::remove_file(path) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
                _ => {}
            }
            #[cfg(unix)]
            match std::fs::remove_dir(path.parent().unwrap()) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
                _ => {}
            }
            Ok(())
        });
        if let Err(payload) = outcome {
            if let Err(error) = cleanup {
                eprintln!("test-owned spill cleanup failed: {error}");
            }
            resume_unwind(payload);
        }
        cleanup.unwrap();
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
    fn spill_lease_rejects_overlap_and_rolls_back_partial_registration() {
        with_physical_pair(|pair| {
            let high_lease = SpillRetirement::collect(&pair.high.root, limits(), || false).unwrap();
            let mut observed_fresh_claim = false;
            let overlap = SpillRetirement::collect(&pair.full.root, limits(), || {
                observed_fresh_claim |= pair.low_leased() && pair.high_leased();
                false
            })
            .err()
            .unwrap();
            assert!(observed_fresh_claim, "overlap must occur after claiming the lower-address fresh block");
            assert_eq!(overlap.kind(), io::ErrorKind::InvalidInput);
            assert!(!pair.low_leased(), "failed overlap must roll back its successful fresh claim");
            assert!(pair.high_leased(), "failed overlap must preserve the existing higher-address lease");
            pair.reacquire_prefix();
            assert!(pair.high_leased());
            drop(high_lease);
            SpillRetirement::collect(&pair.full.root, limits(), || false).unwrap()
        });
    }

    #[test]
    fn spill_lease_cancellation_after_fresh_claim_rolls_back_partial_registration() {
        with_physical_pair(|pair| {
            let mut observed_fresh_claim = false;
            let error = SpillRetirement::collect(&pair.full.root, limits(), || {
                if pair.low_leased() {
                    assert!(!pair.high_leased(), "cancel before the second physical claim");
                    observed_fresh_claim = true;
                    true
                } else {
                    false
                }
            })
            .err()
            .unwrap();
            assert!(observed_fresh_claim, "cancellation must follow a successful fresh claim");
            assert_eq!(error.kind(), io::ErrorKind::Interrupted);
            assert!(!pair.low_leased());
            assert!(!pair.high_leased());
            pair.reacquire_prefix();
            SpillRetirement::collect(&pair.full.root, limits(), || false).unwrap()
        });
    }

    #[test]
    fn spill_lease_unwind_after_fresh_claim_rolls_back_partial_registration() {
        with_physical_pair(|pair| {
            let mut observed_fresh_claim = false;
            let unwind = catch_unwind(AssertUnwindSafe(|| {
                let _result = SpillRetirement::collect(&pair.full.root, limits(), || {
                    if pair.low_leased() {
                        assert!(!pair.high_leased(), "unwind before the second physical claim");
                        observed_fresh_claim = true;
                        panic!("test unwind after a successful physical spill claim");
                    }
                    false
                });
            }));
            assert!(unwind.is_err());
            assert!(observed_fresh_claim, "unwind must follow a successful fresh claim");
            assert!(!pair.low_leased());
            assert!(!pair.high_leased());
            pair.reacquire_prefix();
            SpillRetirement::collect(&pair.full.root, limits(), || false).unwrap()
        });
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
