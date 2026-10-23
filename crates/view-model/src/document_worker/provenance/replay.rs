//! Worker-only Serializer -> Decoder -> RecoveryAssembler adapter.
//!
//! One ReplayRevision stages ALL capsules/replacements of an authenticated outer
//! journal batch. Only explicit commit publishes IDs, root and owner admission;
//! parser/replace/commit failure or drop rolls the WHOLE revision back. Encoding,
//! BOM/output policy, journal authentication and document revision metadata belong
//! to the caller. This is not application recovery or performance acceptance.
//!
//! ReplayState must stay on the document worker after take_buffer returns a
//! normal editable Buffer. It retains every DecodedBacking owner, including on
//! cancelled finish. Explicit abandon/retry_retirement drops assembler pins and
//! retries cleanup; live UI snapshots retain owners until their final pin drops.
//! State drop performs NO filesystem cleanup. Do not drop the last state while
//! resources remain: cleanup failures retain exact retry paths, never just text.

use super::decode::{self, CapsuleSink, DecodeLimits};
use kwil_core_text::{
    Buffer, RecoveryAssembler, RecoveryLimits, RecoveryRevision, RecoveryUsage, Snapshot,
    decoded_backing::{
        BackingError, BackingPaths, CHECKPOINT_RAW_BYTES, DecodedBacking, Interpretation, RetirementError,
    },
    encoding::Encoding,
};
use std::{
    io::{self, Read},
    mem::size_of,
    ops::Range,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
};

/// Explicit admission limits, not product settings. The storage cap is a
/// conservative reservation for ALL origin raw/logical/checkpoint payloads,
/// including unretired quarantine. Core add/undo/work caps remain independent.
pub struct ReplayLimits {
    pub core: RecoveryLimits,
    pub decode: DecodeLimits,
    pub max_origin_storage_bytes: u64,
    /// Native encoded bytes/capacity, including the generated private suffix.
    /// This is an explicit caller admission cap, not a new product default.
    pub max_private_path_bytes: usize,
}

enum Resource {
    Owner(DecodedBacking),
    Paths(Box<BackingPaths>),
}
struct Slot {
    id: u64,
    resource: Option<Resource>,
    storage: u64,
    accepted: bool,
}
struct Owners {
    slots: Vec<Slot>,
    parent: PathBuf,
    max_origins: usize,
    storage_limit: u64,
    storage_reserved: u64,
    metadata_reserved: usize,
    origin_path_bytes_bound: usize,
}

/// Counts after a retirement attempt. A live owner is NOT a cleanup failure;
/// leave the state on the worker queue and retry after all snapshots are gone.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RetirementStatus {
    pub retired: usize,
    pub live_owners: usize,
    pub cleanup_paths: usize,
}

#[must_use = "keep on the worker and explicitly abandon/retry retirement until resources are gone"]
pub struct ReplayState {
    assembler: Option<RecoveryAssembler>,
    owners: Owners,
    decode: DecodeLimits,
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn overflow() -> io::Error {
    invalid("replay admission arithmetic overflow")
}
fn cancelled(cancel: &AtomicBool) -> bool {
    cancel.load(Ordering::Relaxed)
}
fn poll(cancel: &AtomicBool) -> io::Result<()> {
    if cancelled(cancel) { Err(io::ErrorKind::Interrupted.into()) } else { Ok(()) }
}

// A stateless atom consumes >=1 raw byte and emits <=8 logical UTF8 bytes (core
// decoded_backing contract). Whole-atom cuts can shorten a64KiB block by <=4.
// Charge the bound before reading raw data or creating ANY private file.
fn storage_bound(raw: u64) -> io::Result<u64> {
    let records = raw.div_ceil((CHECKPOINT_RAW_BYTES - 4) as u64).checked_add(2).ok_or_else(overflow)?;
    raw.checked_mul(9).and_then(|n| records.checked_mul(16).and_then(|index| n.checked_add(index))).ok_or_else(overflow)
}
fn owner_metadata(parent: &std::path::Path, slots: usize, path_cap: usize) -> io::Result<usize> {
    // Private names are "kwil-decoded-<u32 PID>-<u64 counter>/{raw,logical,
    // checkpoints}". A64-byte suffix covers separators +13-byte prefix +10 PID
    // digits +1 dash +20 counter digits +12-byte checkpoints suffix. Per origin:
    // prepare holds four local paths, four Storage clones and one source path;
    // retirement can hold four retry clones with those nine still alive (13).
    // Cleanup error reconstruction holds directory +three joined paths (4).
    // Reserve32*path_cap: up to16 paths, each with2x native growth capacity.
    // This entire fixed owner/quarantine reservation is subtracted from core's
    // metadata budget BEFORE either registry is preallocated. Maps and bounded
    // parser/decoder scratch are not file-sized resident payload allocations.
    let parent_len = parent.as_os_str().as_encoded_bytes().len();
    if parent_len.checked_add(64).ok_or_else(overflow)? > path_cap {
        return Err(invalid("replay parent exceeds private path cap"));
    }
    let path = path_cap.checked_mul(32).ok_or_else(overflow)?;
    let slot = size_of::<Slot>()
        .checked_add(size_of::<BackingPaths>() * 2)
        .and_then(|n| n.checked_add(path))
        .ok_or_else(overflow)?;
    slots
        .checked_mul(slot)
        .and_then(|n| n.checked_add(size_of::<ReplayState>() + size_of::<ReplayRevision<'_>>()))
        .and_then(|n| n.checked_add(path_cap))
        .ok_or_else(overflow)
}

impl ReplayState {
    pub fn new(private_parent: PathBuf, mut limits: ReplayLimits) -> io::Result<Self> {
        if !private_parent.is_absolute() {
            return Err(invalid("replay parent must be an app-private absolute directory"));
        }
        if private_parent.capacity() > limits.max_private_path_bytes {
            return Err(invalid("replay parent capacity exceeds private path cap"));
        }
        let metadata_reserved =
            owner_metadata(&private_parent, limits.core.max_origins, limits.max_private_path_bytes)?;
        let origin_path_bytes_bound = limits.max_private_path_bytes.checked_mul(32).ok_or_else(overflow)?;
        limits.core.max_metadata_bytes = limits
            .core
            .max_metadata_bytes
            .checked_sub(metadata_reserved)
            .ok_or_else(|| invalid("replay owner registry metadata cap exceeded"))?;
        let mut slots = Vec::new();
        slots.try_reserve_exact(limits.core.max_origins).map_err(io::Error::other)?;
        if slots.capacity() > limits.core.max_origins {
            return Err(invalid("replay owner allocator exceeded charged slots"));
        }
        let assembler = RecoveryAssembler::new(limits.core)?;
        Ok(Self {
            assembler: Some(assembler),
            decode: limits.decode,
            owners: Owners {
                slots,
                parent: private_parent,
                max_origins: limits.core.max_origins,
                storage_limit: limits.max_origin_storage_bytes,
                storage_reserved: 0,
                metadata_reserved,
                origin_path_bytes_bound,
            },
        })
    }

    pub fn snapshot(&self) -> Option<Snapshot> {
        self.assembler.as_ref().map(RecoveryAssembler::snapshot)
    }
    pub fn usage(&self) -> Option<RecoveryUsage> {
        self.assembler.as_ref().map(RecoveryAssembler::usage)
    }
    pub fn owner_metadata_reserved(&self) -> usize {
        self.owners.metadata_reserved
    }
    pub fn origin_storage_reserved(&self) -> u64 {
        self.owners.storage_reserved
    }
    pub fn accepted_origin_count(&self) -> usize {
        self.owners.slots.iter().filter(|s| s.accepted).count()
    }
    pub fn retained_resource_count(&self) -> usize {
        self.owners.slots.len()
    }
    pub fn has_pending_cleanup(&self) -> bool {
        self.owners.slots.iter().any(|s| !s.accepted)
    }

    pub fn begin_revision<'a>(&'a mut self, cancel: &'a AtomicBool) -> io::Result<ReplayRevision<'a>> {
        poll(cancel)?;
        if self.has_pending_cleanup() {
            return Err(invalid("replay cleanup quarantine must be retried before another revision"));
        }
        let core = self
            .assembler
            .as_mut()
            .ok_or_else(|| invalid("replay assembler was already finished or abandoned"))?
            .begin_revision();
        let pending_start = self.owners.slots.len();
        Ok(ReplayRevision {
            core: Some(core),
            owners: &mut self.owners,
            decode: &self.decode,
            cancel,
            pending_start,
            fragment: Fragment::None,
            logical: 0,
            committed: false,
            rolled_back: false,
            cleanup_error: None,
            #[cfg(test)]
            index_cancel_at: None,
        })
    }

    /// Retains owners on success AND cancellation. Cancellation before taking
    /// the assembler leaves it retryable; cancellation inside core finish makes
    /// assembly terminal but retains every backing for explicit retirement.
    pub fn take_buffer(&mut self, cancel: &AtomicBool) -> io::Result<Buffer> {
        self.take_buffer_checked(|| cancelled(cancel))
    }
    fn take_buffer_checked(&mut self, mut cancel: impl FnMut() -> bool) -> io::Result<Buffer> {
        if cancel() {
            return Err(io::ErrorKind::Interrupted.into());
        }
        if self.has_pending_cleanup() {
            return Err(invalid("replay has unresolved cleanup quarantine"));
        }
        self.assembler.take().ok_or_else(|| invalid("replay assembler already consumed"))?.finish(cancel)
    }

    /// Drop all assembler pins first. Never deletes original files or journals.
    /// Returned live owners remain queued here, including with UI Buffer pins.
    pub fn abandon(&mut self) -> io::Result<RetirementStatus> {
        drop(self.assembler.take());
        self.owners.retire(true)
    }
    /// Before finish, retries ONLY quarantined/pending owners, preserving the
    /// accepted assembler's origins. After finish/abandon, retries every owner.
    pub fn retry_retirement(&mut self) -> io::Result<RetirementStatus> {
        self.owners.retire(self.assembler.is_none())
    }
}

impl Owners {
    fn preflight(&self, id: u64, raw_len: u64) -> io::Result<u64> {
        if id == 0 || self.slots.iter().any(|s| s.id == id) {
            return Err(invalid("duplicate or zero replay origin ID"));
        }
        if self.slots.len() >= self.max_origins {
            return Err(invalid("replay owner origin cap exceeded"));
        }
        let storage = storage_bound(raw_len)?;
        self.storage_reserved
            .checked_add(storage)
            .filter(|&n| n <= self.storage_limit)
            .ok_or_else(|| invalid("replay origin storage cap exceeded"))?;
        Ok(storage)
    }

    fn retire(&mut self, all: bool) -> io::Result<RetirementStatus> {
        let mut result = RetirementStatus::default();
        let mut failure = None;
        for slot in &mut self.slots {
            if slot.accepted && !all {
                continue;
            }
            let Some(resource) = slot.resource.take() else {
                continue;
            };
            let pending = match resource {
                Resource::Owner(owner) => match owner.try_retire() {
                    Ok(()) => None,
                    Err(RetirementError::LiveSnapshots(owner)) => {
                        result.live_owners += 1;
                        Some(Resource::Owner(owner))
                    }
                    Err(RetirementError::Cleanup { paths, error }) => {
                        result.cleanup_paths += 1;
                        if failure.is_none() {
                            failure = Some(error);
                        }
                        Some(Resource::Paths(paths))
                    }
                },
                Resource::Paths(paths) => match paths.cleanup() {
                    Ok(()) => None,
                    Err(error) => {
                        result.cleanup_paths += 1;
                        if failure.is_none() {
                            failure = Some(error);
                        }
                        Some(Resource::Paths(paths))
                    }
                },
            };
            slot.resource = pending;
            if slot.resource.is_none() {
                result.retired += 1;
            }
        }
        self.slots.retain(|slot| {
            if slot.resource.is_none() {
                self.storage_reserved -= slot.storage;
                false
            } else {
                true
            }
        });
        if let Some(error) = failure {
            Err(io::Error::new(
                error.kind(),
                format!("private replay cleanup incomplete; exact retry resources retained: {error}"),
            ))
        } else {
            Ok(result)
        }
    }
}

#[derive(PartialEq, Eq)]
enum Fragment {
    None,
    Building,
    Complete,
}

#[must_use = "explicitly commit the complete journal batch, or abort/drop it on the worker"]
pub struct ReplayRevision<'a> {
    core: Option<RecoveryRevision<'a>>,
    owners: &'a mut Owners,
    decode: &'a DecodeLimits,
    cancel: &'a AtomicBool,
    pending_start: usize,
    fragment: Fragment,
    logical: u64,
    committed: bool,
    rolled_back: bool,
    cleanup_error: Option<io::Error>,
    #[cfg(test)]
    index_cancel_at: Option<usize>,
}
impl<'a> ReplayRevision<'a> {
    fn core(&mut self) -> io::Result<&mut RecoveryRevision<'a>> {
        self.core.as_mut().ok_or_else(|| invalid("replay revision was aborted"))
    }
    fn building(&self) -> io::Result<()> {
        poll(self.cancel)?;
        if self.core.is_none() || self.fragment != Fragment::Building {
            return Err(invalid("no building replay fragment"));
        }
        Ok(())
    }
    fn rollback(&mut self) {
        if self.rolled_back {
            return;
        }
        self.rolled_back = true;
        // This ordering is essential: pending core IDs/roots must release their
        // snapshot pins BEFORE attempting to unlink their backing files.
        drop(self.core.take());
        self.fragment = Fragment::None;
        match self.owners.retire(false) {
            Err(error) => self.cleanup_error = Some(error),
            Ok(status) if status.live_owners != 0 || status.cleanup_paths != 0 => {
                self.cleanup_error =
                    Some(io::Error::other("pending replay backing retirement uncertain; retry resources retained"));
            }
            _ => {}
        }
    }
    fn checked<T>(&mut self, result: io::Result<T>) -> io::Result<T> {
        if let Err(error) = result {
            self.rollback();
            if let Some(cleanup) = &self.cleanup_error {
                return Err(io::Error::new(error.kind(), format!("{error}; {cleanup}")));
            }
            return Err(error);
        }
        result
    }
    fn next_logical(&self, len: u64) -> io::Result<u64> {
        self.logical
            .checked_add(len)
            .filter(|&n| n <= self.decode.max_logical_bytes)
            .ok_or_else(|| invalid("replay fragment logical cap exceeded"))
    }

    pub fn apply_capsule(
        &mut self,
        input: &mut impl Read,
        physical_len: u64,
        logical_len: u64,
        replace: Range<u64>,
    ) -> io::Result<()> {
        let result = (|| {
            poll(self.cancel)?;
            if self.core.is_none() || self.fragment != Fragment::None {
                return Err(invalid("unconsumed or aborted replay fragment"));
            }
            let cancel = self.cancel;
            let limits = self.decode;
            decode::decode_capsule(input, physical_len, logical_len, limits, cancel, self)?;
            if self.fragment != Fragment::Complete || self.logical != logical_len {
                return Err(invalid("incomplete replay fragment"));
            }
            self.core()?.replace(replace, || cancelled(cancel))?;
            self.fragment = Fragment::None;
            Ok(())
        })();
        self.checked(result)
    }

    pub fn commit(self) -> io::Result<()> {
        let cancel = self.cancel;
        self.commit_checked(|| cancelled(cancel))
    }
    fn commit_checked(mut self, cancel: impl FnMut() -> bool) -> io::Result<()> {
        let result = (|| {
            if self.fragment != Fragment::None {
                return Err(invalid("unconsumed fragment at replay commit"));
            }
            self.core.take().ok_or_else(|| invalid("replay revision was aborted"))?.commit(cancel)?;
            // Infallible publication after core commit; no cancellation or
            // callback between root/ID publication and owner admission.
            for slot in &mut self.owners.slots[self.pending_start..] {
                slot.accepted = true;
            }
            self.committed = true;
            Ok(())
        })();
        self.checked(result)
    }
    pub fn abort(mut self) -> io::Result<()> {
        self.rollback();
        match self.cleanup_error.take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}
impl Drop for ReplayRevision<'_> {
    fn drop(&mut self) {
        if !self.committed && self.core.is_some() {
            self.rollback();
        }
    }
}

impl CapsuleSink for ReplayRevision<'_> {
    fn begin_fragment(&mut self) -> io::Result<()> {
        let result = (|| {
            poll(self.cancel)?;
            if self.fragment != Fragment::None {
                return Err(invalid("unconsumed replay fragment"));
            }
            self.core()?.begin_fragment()?;
            self.fragment = Fragment::Building;
            self.logical = 0;
            Ok(())
        })();
        self.checked(result)
    }
    fn define_origin(
        &mut self,
        id: u64,
        interpretation: Interpretation,
        raw_len: u64,
        input: &mut impl Read,
    ) -> io::Result<()> {
        let result = (|| {
            self.building()?;
            let storage = self.owners.preflight(id, raw_len)?;
            let logical_bound = raw_len.checked_mul(8).ok_or_else(overflow)?;
            // The constructor charged all path copies in this reservation;
            // the same upper bound also covers the core's path/index charge.
            let path_bound = self.owners.origin_path_bytes_bound;
            let cancel = self.cancel;
            self.core()?.preflight_origin(id, logical_bound, path_bound, || cancelled(cancel))?;
            // Capacity and all owner metadata were charged at construction.
            let index = self.owners.slots.len();
            self.owners.slots.push(Slot { id, resource: None, storage, accepted: false });
            self.owners.storage_reserved += storage;
            let prepared =
                DecodedBacking::prepare_from_reader(input, raw_len, interpretation, &self.owners.parent, self.cancel);
            let owner = match prepared {
                Ok(owner) => owner,
                Err(BackingError::Cleanup { cause, directory, error }) => {
                    // These exact filenames are the pinned backing contract.
                    // Keep retry resources even when preparation never yielded
                    // a DecodedBacking owner. No recursive/shared deletion.
                    let paths = BackingPaths {
                        raw: directory.join("raw"),
                        logical: directory.join("logical"),
                        checkpoints: directory.join("checkpoints"),
                        directory,
                    };
                    self.owners.slots[index].resource = Some(Resource::Paths(Box::new(paths)));
                    return Err(io::Error::new(
                        backing_kind(&cause),
                        format!("{cause}; private backing cleanup failed: {error}"),
                    ));
                }
                Err(error) => return Err(io::Error::new(backing_kind(&error), error)),
            };
            let origin = owner.snapshot();
            // Retain OWNER before fallible sparse-index registration. A plain
            // owner drop does not unlink; rollback below must retire it.
            self.owners.slots[index].resource = Some(Resource::Owner(owner));
            let cancel = self.cancel;
            #[cfg(test)]
            let cancel_at = self.index_cancel_at;
            #[cfg(test)]
            let mut polls = 0;
            self.core()?.register_origin(id, origin, || {
                // Deterministic test-only scheduling at the REAL core index
                // polls; no timing race, stress thread or replacement index.
                #[cfg(test)]
                {
                    polls += 1;
                    if cancel_at == Some(polls) {
                        cancel.store(true, Ordering::Relaxed);
                    }
                }
                cancelled(cancel)
            })
        })();
        self.checked(result)
    }
    fn append_origin(&mut self, id: u64, range: Range<u64>) -> io::Result<()> {
        let result = (|| {
            self.building()?;
            let len = range.end.checked_sub(range.start).ok_or_else(|| invalid("reversed replay origin range"))?;
            let logical = self.next_logical(len)?;
            let cancel = self.cancel;
            self.core()?.append_origin(id, range, || cancelled(cancel))?;
            self.logical = logical;
            Ok(())
        })();
        self.checked(result)
    }
    fn append_bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
        let result = (|| {
            self.building()?;
            let logical = self.next_logical(bytes.len() as u64)?;
            let cancel = self.cancel;
            self.core()?.append_bytes(bytes, || cancelled(cancel))?;
            self.logical = logical;
            Ok(())
        })();
        self.checked(result)
    }
    fn append_opaque(
        &mut self,
        encoding: Encoding,
        raw: &[u8],
        endpoints_le: &[u8],
        logical_count: u64,
    ) -> io::Result<()> {
        let result = (|| {
            self.building()?;
            let logical = self.next_logical(logical_count)?;
            let cancel = self.cancel;
            self.core()?.append_opaque(encoding, raw, endpoints_le, logical_count, || cancelled(cancel))?;
            self.logical = logical;
            Ok(())
        })();
        self.checked(result)
    }
    fn end_fragment(&mut self, logical_len: u64) -> io::Result<()> {
        let result = (|| {
            self.building()?;
            if logical_len != self.logical {
                return Err(invalid("replay sink logical length mismatch"));
            }
            self.fragment = Fragment::Complete;
            Ok(())
        })();
        self.checked(result)
    }
    fn abort(&mut self) {
        self.rollback();
    }
}
fn backing_kind(error: &BackingError) -> io::ErrorKind {
    match error {
        BackingError::Io(error) => error.kind(),
        BackingError::Cancelled => io::ErrorKind::Interrupted,
        BackingError::Cleanup { cause, .. } => backing_kind(cause),
        _ => io::ErrorKind::InvalidData,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document_worker::provenance::OriginRegistry;
    use kwil_core_text::{
        SourceChunk, TextFile,
        decoded_backing::DecodedSnapshot,
        encoding::{self, SavePlan},
    };
    use std::{
        cell::RefCell,
        io::Cursor,
        path::Path,
        sync::{Arc, atomic::AtomicU64},
    };

    struct Fixture {
        root: PathBuf,
        source: PathBuf,
        recovered: PathBuf,
        owners: RefCell<Vec<DecodedBacking>>,
    }
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "kwil-replay-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let source = root.join("source");
            let recovered = root.join("recovered");
            std::fs::create_dir(&root).unwrap();
            std::fs::create_dir(&source).unwrap();
            std::fs::create_dir(&recovered).unwrap();
            Self { root, source, recovered, owners: RefCell::new(Vec::new()) }
        }
        fn origin(&self, raw: &[u8], encoding: Encoding, bom: usize) -> DecodedSnapshot {
            assert!(raw.len() <= 128 << 10);
            let owner = DecodedBacking::prepare_from_reader(
                &mut Cursor::new(raw),
                raw.len() as u64,
                Interpretation::new(encoding, bom).unwrap(),
                &self.source,
                &AtomicBool::new(false),
            )
            .unwrap();
            let snapshot = owner.snapshot();
            self.owners.borrow_mut().push(owner);
            snapshot
        }
        fn state(&self) -> ReplayState {
            ReplayState::new(self.recovered.clone(), limits()).unwrap()
        }
        fn count(&self) -> usize {
            std::fs::read_dir(&self.recovered).unwrap().count()
        }
        fn assert_empty(&self) {
            assert_eq!(self.count(), 0, "ordinary abort must retire every new private backing");
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            for owner in self.owners.get_mut().drain(..) {
                assert!(owner.try_retire().is_ok(), "source owner pins must be released before fixture retirement");
            }
            // Only this unique, scoped fixture is recursively removed. Never a
            // caller recovery parent, journal, original or shared temp root.
            std::fs::remove_dir_all(&self.root).unwrap();
        }
    }
    fn limits() -> ReplayLimits {
        ReplayLimits {
            core: RecoveryLimits {
                add_memory_bytes: 4096,
                undo_memory_bytes: 1 << 20,
                max_origins: 8,
                max_records: 1024,
                max_pieces: 4096,
                max_blocks: 512,
                max_metadata_bytes: 16 << 20,
            },
            decode: DecodeLimits { max_physical_bytes: 128 << 10, max_logical_bytes: 128 << 10, max_records: 128 },
            max_origin_storage_bytes: 4 << 20,
            max_private_path_bytes: 1024,
        }
    }
    fn registry() -> OriginRegistry {
        OriginRegistry::new(8, 8192)
    }
    fn literal(bytes: &[u8]) -> Snapshot {
        let mut buffer = Buffer::empty();
        buffer.insert(0, bytes).unwrap();
        buffer.snapshot()
    }
    fn raw(snapshot: &Snapshot) -> Vec<u8> {
        let mut bytes = Vec::new();
        snapshot.write_to(&mut bytes).unwrap();
        bytes
    }
    fn saved(snapshot: &Snapshot, encoding: Encoding, bom: bool) -> Vec<u8> {
        let mut bytes = Vec::new();
        encoding::write_snapshot(snapshot, SavePlan { encoding, bom, line_ending: None }, &mut bytes).unwrap();
        bytes
    }
    struct Capsule {
        bytes: Vec<u8>,
        logical: u64,
    }
    fn capsule(registry: &mut OriginRegistry, snapshot: Snapshot, range: Range<u64>) -> Capsule {
        let plan = registry.plan(snapshot, range, Arc::new(AtomicBool::new(false))).unwrap();
        pack(registry, plan)
    }
    fn seed(registry: &mut OriginRegistry, origin: DecodedSnapshot) -> Capsule {
        let snapshot = Buffer::from_decoded(origin.clone()).snapshot();
        let plan = registry.plan_seed(snapshot, origin, Arc::new(AtomicBool::new(false))).unwrap();
        pack(registry, plan)
    }
    fn pack(registry: &mut OriginRegistry, plan: super::super::CapsulePlan) -> Capsule {
        let logical = plan.logical_len();
        let physical = plan.physical_len();
        let mut reader = registry.start(plan).unwrap();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes.len() as u64, physical);
        assert!(bytes.len() <= 128 << 10);
        registry.admit(reader.finish().unwrap()).unwrap();
        Capsule { bytes, logical }
    }
    fn apply(revision: &mut ReplayRevision<'_>, capsule: &Capsule, range: Range<u64>) -> io::Result<()> {
        revision.apply_capsule(&mut Cursor::new(&capsule.bytes), capsule.bytes.len() as u64, capsule.logical, range)
    }
    fn accepted(state: &mut ReplayState, capsule: &Capsule, range: Range<u64>) {
        let cancel = AtomicBool::new(false);
        let mut revision = state.begin_revision(&cancel).unwrap();
        apply(&mut revision, capsule, range).unwrap();
        revision.commit().unwrap();
    }

    #[test]
    fn replay_real_capsules_apply_base_delete_typing_multicursor_undo_redo_and_normal_edits() {
        let fixture = Fixture::new();
        let mut registry = registry();
        let mut source = Buffer::empty();
        source.insert(0, b"abcd").unwrap();
        let mut state = fixture.state();
        accepted(&mut state, &capsule(&mut registry, source.snapshot(), 0..4), 0..0);
        source.delete(1..2).unwrap();
        accepted(&mut state, &capsule(&mut registry, Snapshot::default(), 0..0), 1..2);
        source.insert(1, b"X").unwrap();
        accepted(&mut state, &capsule(&mut registry, source.snapshot(), 1..2), 1..1);
        assert_eq!(raw(&state.snapshot().unwrap()), b"aXcd");
        let previous = source.snapshot();
        assert!(source.begin());
        source.delete(0..1).unwrap();
        source.insert(0, b"Q").unwrap();
        let first = capsule(&mut registry, source.snapshot(), 0..1);
        source.delete(3..4).unwrap();
        source.insert(3, b"R").unwrap();
        let second = capsule(&mut registry, source.snapshot(), 3..4);
        assert!(source.commit());
        let cancel = AtomicBool::new(false);
        let mut revision = state.begin_revision(&cancel).unwrap();
        apply(&mut revision, &first, 0..1).unwrap();
        apply(&mut revision, &second, 3..4).unwrap();
        revision.commit().unwrap();
        assert_eq!(raw(&state.snapshot().unwrap()), b"QXcR");
        assert!(source.undo());
        assert_eq!(raw(&source.snapshot()), raw(&previous));
        accepted(&mut state, &capsule(&mut registry, source.snapshot(), 0..4), 0..4);
        assert!(source.redo());
        accepted(&mut state, &capsule(&mut registry, source.snapshot(), 0..4), 0..4);
        let mut restored = state.take_buffer(&cancel).unwrap();
        assert_eq!(raw(&restored.snapshot()), b"QXcR");
        assert!(!restored.undo(), "replay must not manufacture editing undo history");
        restored.insert(4, b"!").unwrap();
        assert!(restored.undo());
        assert_eq!(raw(&restored.snapshot()), b"QXcR");
        assert!(restored.redo());
        assert_eq!(raw(&restored.snapshot()), b"QXcR!");
        drop(restored);
        assert_eq!(state.abandon().unwrap().live_owners, 0);
        fixture.assert_empty();
    }

    #[test]
    fn replay_all_source_variants_preserve_ordinary_invalid_bytes_and_typed_malformed_raw() {
        let fixture = Fixture::new();
        let path = fixture.source.join("ordinary");
        std::fs::write(&path, b"\xef\xbb\xbf\xffA").unwrap();
        let original = Buffer::from_file(TextFile::open(&path).unwrap()).unwrap();
        let mut registry = registry();
        let mixed = original.snapshot().concat(&literal(b"x\0\xff")).unwrap();
        let mut state = fixture.state();
        accepted(&mut state, &capsule(&mut registry, mixed, 0..8), 0..0);
        assert_eq!(raw(&state.snapshot().unwrap()), b"\xef\xbb\xbf\xffAx\0\xff");
        assert_eq!(state.accepted_origin_count(), 0);
        state.abandon().unwrap();
        fixture.assert_empty();
        let origin = fixture.origin(b"\xff\xfeA\0\0\xd8B\0\xff", Encoding::UTF16_LE, 2);
        let source = Buffer::from_decoded(origin.clone());
        let mut foreign = Buffer::empty();
        foreign.insert_snapshot_range(0, &source.snapshot(), 0..source.len()).unwrap();
        let opaque = foreign.snapshot().slice(1..2).unwrap().concat(&foreign.snapshot().slice(3..4).unwrap()).unwrap();
        let combined = source.snapshot().concat(&opaque).unwrap();
        let mut state = fixture.state();
        accepted(&mut state, &capsule(&mut registry, combined.clone(), 0..combined.len()), 0..0);
        assert_eq!(saved(&state.snapshot().unwrap(), Encoding::UTF16_LE, true), b"\xff\xfeA\0\0\xd8B\0\xff\0\xd8\xff");
        let snapshot = state.snapshot().unwrap();
        let mut opaque_raw = Vec::new();
        for chunk in snapshot.source_chunks(0..snapshot.len()).unwrap() {
            if let SourceChunk::Opaque { raw_bytes, raw_offsets, .. } = chunk.unwrap() {
                assert_eq!(u32::from_le_bytes(raw_offsets[..4].try_into().unwrap()), 0);
                opaque_raw.extend_from_slice(raw_bytes);
            }
        }
        assert_eq!(opaque_raw, b"\0\xd8\xff");
        drop(snapshot);
        state.abandon().unwrap();
        fixture.assert_empty();
    }

    #[test]
    fn replay_partial_big5_aliases_and_stateless_origins_keep_exact_raw_provenance() {
        let fixture = Fixture::new();
        let big5 = fixture.origin(b"\x88\x62\xa4\x40", Encoding::for_label("big5").unwrap(), 0);
        let source = Buffer::from_decoded(big5);
        let snapshot = source.snapshot();
        let selected = snapshot.slice(2..4).unwrap().concat(&snapshot.slice(0..2).unwrap()).unwrap();
        let mut registry = registry();
        let mut state = fixture.state();
        accepted(&mut state, &capsule(&mut registry, selected, 0..4), 0..0);
        assert_eq!(saved(&state.snapshot().unwrap(), Encoding::UTF8, false), "\u{304}\u{ca}".as_bytes());
        let restored = state.snapshot().unwrap();
        let ranges: Vec<_> = restored
            .source_chunks(0..4)
            .unwrap()
            .map(|chunk| match chunk.unwrap() {
                SourceChunk::Decoded { origin, range, .. } => {
                    assert_eq!(origin.raw_bytes(), b"\x88\x62\xa4\x40");
                    range
                }
                _ => panic!("partial alias must retain its logical origin range"),
            })
            .collect();
        assert_eq!(ranges, [2..4, 0..2]);
        drop(restored);
        state.abandon().unwrap();
        for (label, bytes) in [
            ("utf-8", &b"\xef\xbf\xbd\xff"[..]),
            ("utf-16be", &b"\0A\xd8\0\xff"[..]),
            ("windows-1252", &b"caf\xe9"[..]),
            ("shift_jis", &b"\x93\xfa\x96\x7b"[..]),
            ("gb18030", &b"\x94\x39\xfc\x36"[..]),
            ("koi8-r", &b"\xf0\xd2\xc9\xd7\xc5\xd4"[..]),
        ] {
            let encoding = Encoding::for_label(label).unwrap();
            let origin = fixture.origin(bytes, encoding, 0);
            let mut registry = super::tests::registry();
            let mut state = fixture.state();
            accepted(&mut state, &seed(&mut registry, origin), 0..0);
            assert_eq!(saved(&state.snapshot().unwrap(), encoding, false), bytes, "{label}");
            state.abandon().unwrap();
        }
        fixture.assert_empty();
    }

    #[test]
    fn replay_saved_generations_old_clipboard_and_repeat_refs_reuse_private_origins() {
        let fixture = Fixture::new();
        let first = fixture.origin(b"\xff\xfeA\0", Encoding::UTF16_LE, 2);
        let mut source = Buffer::from_decoded(first.clone());
        let clipboard = source.snapshot();
        let mut registry = registry();
        let mut state = fixture.state();
        accepted(&mut state, &seed(&mut registry, first), 0..0);
        source.insert(1, b"x").unwrap();
        accepted(&mut state, &capsule(&mut registry, source.snapshot(), 1..2), 1..1);
        let second = fixture.origin(b"\xff\xfeA\0x\0", Encoding::UTF16_LE, 2);
        source.checkpoint_current(Buffer::from_decoded(second).snapshot()).unwrap();
        let mixed = source.snapshot().concat(&clipboard).unwrap();
        accepted(&mut state, &capsule(&mut registry, mixed, 0..3), 0..2);
        assert_eq!(state.accepted_origin_count(), 2);
        assert_eq!(fixture.count(), 2);
        let before_storage = state.origin_storage_reserved();
        let before_metadata = state.owner_metadata_reserved();
        let repeat = clipboard.concat(&clipboard).unwrap();
        accepted(&mut state, &capsule(&mut registry, repeat, 0..2), 3..3);
        assert_eq!(fixture.count(), 2, "references must not create or copy any origin backing");
        assert_eq!(state.origin_storage_reserved(), before_storage);
        assert_eq!(state.owner_metadata_reserved(), before_metadata);
        assert_eq!(saved(&state.snapshot().unwrap(), Encoding::UTF16_LE, true), b"\xff\xfeA\0x\0A\0A\0A\0");
        state.abandon().unwrap();
        fixture.assert_empty();
    }

    #[test]
    fn replay_empty_bom_seed_and_deduplicated_zero_refs_keep_owner_identity() {
        let fixture = Fixture::new();
        for (bytes, encoding, bom) in [(&b"\xff\xfe"[..], Encoding::UTF16_LE, 2), (&b""[..], Encoding::UTF8, 0)] {
            let origin = fixture.origin(bytes, encoding, bom);
            let mut registry = registry();
            let mut state = fixture.state();
            accepted(&mut state, &seed(&mut registry, origin.clone()), 0..0);
            assert_eq!(state.accepted_origin_count(), 1);
            assert_eq!(fixture.count(), 1);
            let storage = state.origin_storage_reserved();
            accepted(&mut state, &seed(&mut registry, origin), 0..0);
            assert_eq!(state.accepted_origin_count(), 1);
            assert_eq!(state.origin_storage_reserved(), storage);
            assert_eq!(fixture.count(), 1);
            assert_eq!(saved(&state.snapshot().unwrap(), encoding, bom != 0), bytes);
            state.abandon().unwrap();
            fixture.assert_empty();
        }
    }

    #[test]
    fn replay_late_capsule_failure_rolls_back_entire_batch_ids_pins_and_work() {
        let fixture = Fixture::new();
        let mut state = fixture.state();
        let mut registry = registry();
        accepted(&mut state, &capsule(&mut registry, literal(b"abcd"), 0..4), 0..0);
        let usage = state.usage();
        let origin = fixture.origin(b"X", Encoding::UTF8, 0);
        let first = seed(&mut registry, origin);
        let second = capsule(&mut registry, literal(b"Y"), 0..1);
        let mut corrupt = capsule(&mut registry, literal(b"Z"), 0..1);
        *corrupt.bytes.last_mut().unwrap() = 99;
        let cancel = AtomicBool::new(false);
        let mut revision = state.begin_revision(&cancel).unwrap();
        apply(&mut revision, &first, 0..1).unwrap();
        apply(&mut revision, &second, 1..2).unwrap();
        assert!(apply(&mut revision, &corrupt, 2..3).is_err());
        assert!(revision.commit().is_err());
        assert_eq!(
            raw(&state.snapshot().unwrap()),
            b"abcd",
            "late capsule failure must not expose earlier cursor edits"
        );
        assert_eq!(state.usage(), usage);
        assert_eq!(state.accepted_origin_count(), 0);
        assert!(!state.has_pending_cleanup(), "pending root/ID pins must drop BEFORE owner retirement");
        fixture.assert_empty();
        // The previously staged ID can be defined again after complete abort.
        accepted(&mut state, &first, 0..1);
        assert_eq!(raw(&state.snapshot().unwrap()), b"Xbcd");
        assert_eq!(state.accepted_origin_count(), 1);
        let before = state.snapshot().unwrap();
        let mut revision = state.begin_revision(&cancel).unwrap();
        apply(&mut revision, &second, 1..2).unwrap();
        drop(revision);
        assert_eq!(raw(&state.snapshot().unwrap()), raw(&before));
        drop(before);
        let mut revision = state.begin_revision(&cancel).unwrap();
        assert!(apply(&mut revision, &second, 9..10).is_err());
        assert!(revision.commit().is_err());
        assert_eq!(raw(&state.snapshot().unwrap()), b"Xbcd");
        state.abandon().unwrap();
        fixture.assert_empty();
    }

    struct AtPayload<'a> {
        cursor: Cursor<&'a [u8]>,
        boundary: u64,
        cancel: Option<&'a AtomicBool>,
        error: io::ErrorKind,
    }
    impl Read for AtPayload<'_> {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            assert!(bytes.len() <= 64 << 10);
            if self.cursor.position() == self.boundary {
                if let Some(cancel) = self.cancel {
                    cancel.store(true, Ordering::Relaxed);
                }
                return Err(io::Error::new(self.error, "deliberate origin input failure"));
            }
            let len = bytes.len().min((self.boundary - self.cursor.position()) as usize);
            self.cursor.read(&mut bytes[..len])
        }
    }

    #[test]
    fn replay_duplicate_and_caps_preflight_before_raw_read_or_private_creation() {
        let fixture = Fixture::new();
        let origin = fixture.origin(b"ABC", Encoding::UTF8, 0);
        let mut registry = registry();
        let base = seed(&mut registry, origin);
        let raw_start = 16 + 20 + Encoding::UTF8.name().len() as u64;
        let mut state = fixture.state();
        accepted(&mut state, &base, 0..0);
        let cancel = AtomicBool::new(false);
        let mut reader = AtPayload {
            cursor: Cursor::new(&base.bytes),
            boundary: raw_start,
            cancel: None,
            error: io::ErrorKind::Other,
        };
        let mut revision = state.begin_revision(&cancel).unwrap();
        let error = revision.apply_capsule(&mut reader, base.bytes.len() as u64, base.logical, 0..3).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData, "duplicate must be rejected before asking for raw bytes");
        assert!(error.to_string().contains("duplicate"));
        assert_eq!(reader.cursor.position(), raw_start);
        assert!(revision.commit().is_err());
        assert_eq!(fixture.count(), 1);
        assert_eq!(raw(&state.snapshot().unwrap()), b"ABC");
        state.abandon().unwrap();
        fixture.assert_empty();
        // Duplicate against a staged ID also aborts the entire revision.
        let mut state = fixture.state();
        let mut revision = state.begin_revision(&cancel).unwrap();
        apply(&mut revision, &base, 0..0).unwrap();
        assert!(apply(&mut revision, &base, 0..3).is_err());
        assert!(revision.commit().is_err());
        assert_eq!(state.accepted_origin_count(), 0);
        fixture.assert_empty();
        for cap in ["origins", "storage", "physical", "logical", "records", "metadata", "path"] {
            let mut config = limits();
            match cap {
                "origins" => config.core.max_origins = 0,
                "storage" => config.max_origin_storage_bytes = storage_bound(3).unwrap() - 1,
                "physical" => config.decode.max_physical_bytes = base.bytes.len() as u64 - 1,
                "logical" => config.decode.max_logical_bytes = base.logical - 1,
                "records" => config.decode.max_records = 0,
                "metadata" => config.core.max_metadata_bytes = 1,
                "path" => config.max_private_path_bytes = fixture.recovered.as_os_str().as_encoded_bytes().len(),
                _ => unreachable!(),
            }
            let constructed = ReplayState::new(fixture.recovered.clone(), config);
            if matches!(cap, "metadata" | "path") {
                assert!(constructed.is_err(), "{cap}");
            } else {
                let mut state = constructed.unwrap();
                let mut reader = AtPayload {
                    cursor: Cursor::new(&base.bytes),
                    boundary: raw_start,
                    cancel: None,
                    error: io::ErrorKind::Other,
                };
                let mut revision = state.begin_revision(&cancel).unwrap();
                assert!(
                    revision.apply_capsule(&mut reader, base.bytes.len() as u64, base.logical, 0..0).is_err(),
                    "{cap}"
                );
                assert!(reader.cursor.position() <= raw_start);
                assert!(revision.commit().is_err());
                assert_eq!(state.accepted_origin_count(), 0);
                state.abandon().unwrap();
            }
            fixture.assert_empty();
        }
        assert!(storage_bound(u64::MAX).is_err());
        assert!(owner_metadata(Path::new("/private"), usize::MAX, 1024).is_err());
    }

    #[test]
    fn replay_core_origin_metadata_is_checked_before_raw_input_or_files() {
        let fixture = Fixture::new();
        let mut probe = fixture.state();
        let minimum = probe.owner_metadata_reserved() + probe.usage().unwrap().metadata_bytes;
        probe.abandon().unwrap();
        let mut config = limits();
        config.core.max_metadata_bytes = minimum;
        let mut state = ReplayState::new(fixture.recovered.clone(), config).unwrap();
        let origin = fixture.origin(b"ABC", Encoding::UTF8, 0);
        let mut registry = registry();
        let base = seed(&mut registry, origin);
        struct BeforeRaw<'a> {
            input: Cursor<&'a [u8]>,
            boundary: u64,
            raw_reads: usize,
        }
        impl Read for BeforeRaw<'_> {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                if self.input.position() >= self.boundary {
                    self.raw_reads += 1;
                    return Err(io::Error::new(io::ErrorKind::StorageFull, "raw input must not be requested"));
                }
                let len = bytes.len().min((self.boundary - self.input.position()) as usize);
                self.input.read(&mut bytes[..len])
            }
        }
        let mut input = BeforeRaw {
            input: Cursor::new(&base.bytes),
            boundary: 16 + 20 + Encoding::UTF8.name().len() as u64,
            raw_reads: 0,
        };
        let cancel = AtomicBool::new(false);
        let mut revision = state.begin_revision(&cancel).unwrap();
        let error = revision.apply_capsule(&mut input, base.bytes.len() as u64, base.logical, 0..0).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("metadata"), "wrong preflight error: {error}");
        assert_eq!(input.raw_reads, 0, "metadata refusal must precede raw preparation");
        assert!(revision.commit().is_err());
        assert_eq!(state.accepted_origin_count(), 0);
        assert_eq!(state.origin_storage_reserved(), 0);
        fixture.assert_empty();
        state.abandon().unwrap();
    }

    #[test]
    fn replay_definition_io_cancellation_index_commit_and_finish_failures_keep_ownership() {
        let fixture = Fixture::new();
        let origin = fixture.origin(&vec![b'A'; 96 << 10], Encoding::UTF8, 0);
        let mut registry = registry();
        let base = seed(&mut registry, origin);
        let raw_start = 16 + 20 + Encoding::UTF8.name().len() as u64;
        for error_kind in [io::ErrorKind::Other, io::ErrorKind::Interrupted] {
            let cancel = AtomicBool::new(false);
            let mut state = fixture.state();
            let mut reader = AtPayload {
                cursor: Cursor::new(&base.bytes),
                boundary: raw_start + 7,
                cancel: Some(&cancel),
                error: error_kind,
            };
            let mut revision = state.begin_revision(&cancel).unwrap();
            let error = revision.apply_capsule(&mut reader, base.bytes.len() as u64, base.logical, 0..0).unwrap_err();
            assert_eq!(error.kind(), error_kind, "original IO failure must win coincident cancellation");
            assert!(revision.commit().is_err());
            assert_eq!(state.accepted_origin_count(), 0);
            fixture.assert_empty();
            state.abandon().unwrap();
        }
        let cancel = AtomicBool::new(false);
        let mut state = fixture.state();
        let mut revision = state.begin_revision(&cancel).unwrap();
        revision.index_cancel_at = Some(3); // register preflight + actual logical-index poll
        assert_eq!(apply(&mut revision, &base, 0..0).unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert!(revision.commit().is_err());
        assert_eq!(state.retained_resource_count(), 0, "index failure must retire retained owner after pins");
        fixture.assert_empty();
        cancel.store(false, Ordering::Relaxed);
        let mut revision = state.begin_revision(&cancel).unwrap();
        apply(&mut revision, &base, 0..0).unwrap();
        let mut polls = 0;
        assert_eq!(
            revision
                .commit_checked(|| {
                    polls += 1;
                    polls == 3
                })
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
        assert!(raw(&state.snapshot().unwrap()).is_empty());
        assert_eq!(state.accepted_origin_count(), 0);
        fixture.assert_empty();
        accepted(&mut state, &base, 0..0);
        let reserved = state.origin_storage_reserved();
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(state.take_buffer(&cancel).err().unwrap().kind(), io::ErrorKind::Interrupted);
        assert!(state.snapshot().is_some(), "pre-take cancellation must preserve accepted assembler");
        cancel.store(false, Ordering::Relaxed);
        let mut polls = 0;
        assert_eq!(
            state
                .take_buffer_checked(|| {
                    polls += 1;
                    polls == 3
                })
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::Interrupted
        );
        assert!(state.snapshot().is_none());
        assert_eq!(state.retained_resource_count(), 1, "finish cancellation must never lose backing owner");
        assert_eq!(state.origin_storage_reserved(), reserved);
        state.abandon().unwrap();
        fixture.assert_empty();
    }

    #[test]
    fn replay_worker_retirement_waits_for_buffer_and_last_snapshot_then_cleans_exact_paths() {
        let fixture = Fixture::new();
        let origin = fixture.origin(b"\xff\xfeA\0", Encoding::UTF16_LE, 2);
        let mut registry = registry();
        let mut state = fixture.state();
        accepted(&mut state, &seed(&mut registry, origin), 0..0);
        let cancel = AtomicBool::new(false);
        let buffer = state.take_buffer(&cancel).unwrap();
        let pin = buffer.snapshot();
        let storage = state.origin_storage_reserved();
        assert_eq!(state.retry_retirement().unwrap().live_owners, 1);
        assert_eq!(state.origin_storage_reserved(), storage);
        drop(buffer);
        assert_eq!(state.retry_retirement().unwrap().live_owners, 1);
        assert_eq!(fixture.count(), 1);
        assert_eq!(saved(&pin, Encoding::UTF16_LE, true), b"\xff\xfeA\0");
        drop(pin);
        assert_eq!(state.retry_retirement().unwrap().retired, 1);
        assert_eq!(state.origin_storage_reserved(), 0);
        fixture.assert_empty();
    }

    struct BlocksCleanup<'a> {
        cursor: Cursor<&'a [u8]>,
        raw_end: u64,
        parent: &'a Path,
        blocker: Option<PathBuf>,
    }
    impl Read for BlocksCleanup<'_> {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            let before = self.cursor.position();
            let n = self.cursor.read(bytes)?;
            if before < self.raw_end && self.cursor.position() >= self.raw_end && self.blocker.is_none() {
                let directory = std::fs::read_dir(self.parent)?.next().unwrap()?.path();
                let blocker = directory.join("checkpoints");
                std::fs::create_dir(&blocker)?;
                self.blocker = Some(blocker);
            }
            Ok(n)
        }
    }

    #[test]
    fn replay_prepare_cleanup_failure_retains_exact_paths_storage_and_quarantine_until_retry() {
        let fixture = Fixture::new();
        let mut registry = registry();
        let base = seed(&mut registry, fixture.origin(b"ABC", Encoding::UTF8, 0));
        let raw_end = 16 + 20 + Encoding::UTF8.name().len() as u64 + 3;
        let mut reader =
            BlocksCleanup { cursor: Cursor::new(&base.bytes), raw_end, parent: &fixture.recovered, blocker: None };
        let mut state = fixture.state();
        let cancel = AtomicBool::new(false);
        let mut revision = state.begin_revision(&cancel).unwrap();
        assert!(revision.apply_capsule(&mut reader, base.bytes.len() as u64, base.logical, 0..0).is_err());
        assert!(revision.commit().is_err());
        assert!(state.has_pending_cleanup());
        assert_eq!(state.retained_resource_count(), 1);
        assert_eq!(state.origin_storage_reserved(), storage_bound(3).unwrap());
        let paths = match state.owners.slots[0].resource.as_ref().unwrap() {
            Resource::Paths(paths) => paths.clone(),
            Resource::Owner(_) => panic!("preparation cleanup failure must retain exact BackingPaths"),
        };
        assert_eq!(paths.checkpoints, reader.blocker.as_ref().unwrap().clone());
        assert_eq!(paths.raw, paths.directory.join("raw"));
        assert_eq!(paths.logical, paths.directory.join("logical"));
        assert!(state.begin_revision(&cancel).is_err(), "quarantine consumes its bounded slot and fails closed");
        assert!(state.retry_retirement().is_err());
        std::fs::remove_dir(reader.blocker.take().unwrap()).unwrap();
        assert_eq!(state.retry_retirement().unwrap().retired, 1);
        assert!(!state.has_pending_cleanup());
        assert_eq!(state.origin_storage_reserved(), 0);
        fixture.assert_empty();
        accepted(&mut state, &base, 0..0);
        state.abandon().unwrap();
        fixture.assert_empty();
    }

    #[test]
    fn replay_sink_rejects_wrong_count_unconsumed_fragment_and_poisoned_commit() {
        let fixture = Fixture::new();
        let cancel = AtomicBool::new(false);
        let mut state = fixture.state();
        let mut revision = state.begin_revision(&cancel).unwrap();
        CapsuleSink::begin_fragment(&mut revision).unwrap();
        CapsuleSink::append_bytes(&mut revision, b"ABC").unwrap();
        assert!(CapsuleSink::end_fragment(&mut revision, 2).is_err());
        assert!(revision.commit().is_err());
        assert!(raw(&state.snapshot().unwrap()).is_empty());
        let mut revision = state.begin_revision(&cancel).unwrap();
        CapsuleSink::begin_fragment(&mut revision).unwrap();
        CapsuleSink::append_bytes(&mut revision, b"ABC").unwrap();
        CapsuleSink::end_fragment(&mut revision, 3).unwrap();
        assert!(revision.commit().is_err(), "completed but unreplaced fragment cannot be silently discarded");
        assert!(raw(&state.snapshot().unwrap()).is_empty());
        state.abandon().unwrap();
        fixture.assert_empty();
    }
}
