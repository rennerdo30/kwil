//! Lossless recovery capsule serialization, worker only. This is NOT recovery
//! acceptance: a decoder, document adoption, raw projection, retention/rotation
//! and frontend integration are separate gates.
//!
//! Version 2 wire format (all integers unsigned little endian; decoder accepts v1):
//! ```text
//! header: "KWCP", version:u16=2, flags:u16=0, expected_logical_len:u64
//! 1 OriginDefinition: id:u64, label_len:u16, bom_len:u8, raw_len:u64,
//!                     encoding_label:[u8;label_len], raw:[u8;raw_len]
//! 2 OriginReference:  id:u64, logical_start:u64, logical_end:u64
//! 3 LogicalBytes:     len:u64, bytes:[u8;len]
//! 4 DetachedOpaque:   label_len:u16, logical_count:u64, raw_len:u64,
//!                     table_len:u64, encoding_label, raw, LE-u32 endpoints
//! 5 PreservedText:    same fields as 4, then encoding_label, logical UTF-8,
//!                     raw, LE-u32 endpoints (bit31 marks atom interiors)
//! 0 End
//! ```
//! Definitions contribute ZERO logical bytes; references, text and opaque counts
//! must sum to the header length. Opaque endpoints are normalized to zero and
//! include the final raw endpoint. FF logical sentinels are implicit, never
//! UTF-8 replacement text. Original/Added bytes are logical bytes (including
//! invalid UTF-8), matching the old raw-original API, NOT an encoding label.
//! References use origin LOGICAL coordinates, including Big5 partial aliases;
//! a future decoder must rebuild origin metadata and materialize partial text
//! when needed, never invent raw boundaries. Definitions embed the entire raw
//! origin, including BOM and malformed atoms; no external pathname is persisted.
//! An explicit empty seed includes OriginReference(id,0,0), identifying its
//! origin even after deduplication. Ordinary empty deltas are header + End only.
//!
//! Planning reads metadata/piece descriptors, not raw origin payloads. The first
//! seed can copy the original once. Later moves/copies use IDs; normal insertion
//! capsules contain only inserted logical bytes. New saved generations may need
//! a new definition. This is not a disk-retention or performance acceptance claim.
//!
//! Call `plan`/`start`/`Read`/`finish`/`admit` ONLY on the ordered document worker. The
//! reader owns its immutable snapshot and origin handles, with O(tree height)
//! traversal scratch and at most one bounded detached block (64KiB raw + table).
//! Registry metadata is explicitly caller-capped; its pinned backing maps are
//! not resident heap copies. Keep DecodedBacking owners on the worker retirement
//! queue until registry/plans/readers release their handles.
//!
//! `finish` proves complete serialization, NOT journal admission. Admit its token
//! only AFTER the WHOLE capsule and outer journal batch are accepted in FIFO
//! order. A subsequent flush establishes durability. On ANY journal failure,
//! stop referencing queued IDs and clear/reload the registry on the worker from
//! the admitted/durable prefix; this module does not recover that prefix. A
//! dropped/failed reader or plan cannot publish IDs. Competing plans are stale
//! after ANY admission or clear, including capsules with no new definitions.
//! Checked `start` rejects stale/foreign plans before output. Start, read, whole
//! outer-batch admission and `admit` must still form ONE non-interleaved FIFO
//! worker critical section; two started readers cannot safely compete. If admit
//! fails after enqueue, stop and clear/reload just as for a journal failure.
//!
//! For multiple capsules in ONE outer batch, use `begin_batch`. Its exclusive
//! guard owns a private staged directory; scoped plans/readers cannot escape or
//! expose unchecked tokens. Reader `admit` stages IDs for later capsules only.
//! Guard commit publishes the directory AFTER CommitBatch queue admission, not
//! durability. Any observed error or unfinished plan/reader poisons the guard;
//! drop/abort preserves the exact live baseline. Keep this entire sequence on
//! the same FIFO worker and retain/retire backing owners there. Drops do no I/O.

use kwil_core_text::{Snapshot, SourceChunk, decoded_backing::DecodedSnapshot, encoding::Encoding};
pub mod decode;
pub mod journal;
pub mod payload;
pub mod replay;
use std::{
    io::{self, Read},
    mem::size_of,
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

pub const CAPSULE_VERSION: u16 = 2;
pub const MAX_READ_BYTES: usize = 64 << 10;
const HEADER_LEN: u64 = 16;
const REFERENCE_LEN: u64 = 25;
const LOGICAL_HEADER_LEN: u64 = 9;
const DEFINITION_HEADER_LEN: u64 = 20;
const OPAQUE_HEADER_LEN: u64 = 27;

#[derive(Clone)]
struct Origin {
    id: u64,
    snapshot: DecodedSnapshot,
}

struct Baseline {
    epoch: u64,
    next_id: u64,
    // Sorted by private backing directory. The path is only an in-process key;
    // it is borrowed from the handle, never cloned or written to the capsule.
    origins: Vec<Origin>,
}

/// One registry per journal lifetime, never shared independently across queues.
/// Budget includes two copies of entry slots and baseline headers, covering
/// immutable planning + admission merge. Backing maps and the reader's fixed
/// detached-block scratch are separate; no setting/default is introduced here.
pub struct OriginRegistry {
    baseline: Arc<Baseline>,
    max_origins: usize,
    max_metadata_bytes: usize,
    // Nonzero only in a batch's private shadow: retained live directory and
    // guard/wrapper/control headers, charged in addition to staged two-copy work.
    metadata_retained: usize,
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
fn add(a: u64, b: u64) -> io::Result<u64> {
    a.checked_add(b).ok_or_else(|| invalid("capsule length overflow"))
}
fn count(n: usize) -> io::Result<u64> {
    u64::try_from(n).map_err(|_| invalid("capsule length overflow"))
}
fn poll(cancel: &AtomicBool) -> io::Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(io::Error::new(io::ErrorKind::Interrupted, "capsule cancelled"))
    } else {
        Ok(())
    }
}
fn lookup<'a>(origins: &'a [Origin], origin: &DecodedSnapshot) -> Option<&'a Origin> {
    origins
        .binary_search_by(|entry| entry.snapshot.paths().directory.cmp(&origin.paths().directory))
        .ok()
        .map(|at| &origins[at])
}
fn metadata_cost(slots: usize) -> io::Result<usize> {
    slots
        .checked_mul(size_of::<Origin>())
        .and_then(|n| n.checked_add(size_of::<Baseline>()))
        .and_then(|n| n.checked_mul(2))
        .ok_or_else(|| invalid("origin metadata overflow"))
}
fn reserve(origins: &mut Vec<Origin>, additional: usize) -> io::Result<()> {
    origins.try_reserve_exact(additional).map_err(|_| io::Error::other("origin metadata allocation failed"))
}

impl OriginRegistry {
    pub fn new(max_origins: usize, max_metadata_bytes: usize) -> Self {
        Self {
            baseline: Arc::new(Baseline { epoch: 0, next_id: 1, origins: Vec::new() }),
            max_origins,
            max_metadata_bytes,
            metadata_retained: 0,
        }
    }

    /// Worker-only adoption of an ALREADY authenticated/admitted recovered
    /// directory. Preserve persisted IDs exactly; the next definition is above
    /// their exact maximum. This validates metadata, not journal authenticity.
    /// Caller retains DecodedBacking owners on the worker retirement queue.
    /// No raw read/copy/indexing: only bounded handles, with O(k^2) sorted moves
    /// and duplicate-ID comparisons for k <= max_origins. Failure drops all
    /// temporary pins and never publishes a partially adopted registry.
    pub fn from_recovered(
        max_origins: usize,
        max_metadata_bytes: usize,
        origins: impl IntoIterator<Item = (u64, DecodedSnapshot)>,
        cancel: &AtomicBool,
    ) -> io::Result<Self> {
        poll(cancel)?;
        if metadata_cost(0)? > max_metadata_bytes {
            return Err(invalid("origin metadata budget exceeded"));
        }
        let mut registry = Self::new(max_origins, max_metadata_bytes);
        let mut entries: Vec<Origin> = Vec::new();
        let mut next_id = 1;
        for (id, snapshot) in origins {
            poll(cancel)?;
            if id == 0 || entries.iter().any(|entry| entry.id == id) {
                return Err(invalid("duplicate or zero recovered origin ID"));
            }
            let at = match entries
                .binary_search_by(|entry| entry.snapshot.paths().directory.cmp(&snapshot.paths().directory))
            {
                Ok(_) => return Err(invalid("duplicate recovered origin identity")),
                Err(at) => at,
            };
            let needed = entries.len().checked_add(1).ok_or_else(|| invalid("origin count overflow"))?;
            if needed > max_origins {
                return Err(invalid("origin count cap exceeded"));
            }
            next_id = next_id.max(add(id, 1)?);
            registry.check_capacity(entries.capacity().max(needed))?;
            reserve(&mut entries, 1)?;
            registry.check_capacity(entries.capacity())?;
            entries.insert(at, Origin { id, snapshot });
            poll(cancel)?;
        }
        poll(cancel)?;
        registry.baseline = Arc::new(Baseline { epoch: 0, next_id, origins: entries });
        Ok(registry)
    }

    pub fn origin_count(&self) -> usize {
        self.baseline.origins.len()
    }
    pub fn epoch(&self) -> u64 {
        self.baseline.epoch
    }

    /// Exclusive worker transaction for one outer journal batch. Conservatively
    /// admit the retained ACTUAL committed capacity plus two max-sized staged
    /// directories before allocating the shadow. No raw bytes are read/copied.
    /// IDs/epoch/pins stay live only on explicit successful guard commit.
    pub fn begin_batch(&mut self) -> io::Result<OriginBatch<'_>> {
        let epoch = add(self.baseline.epoch, 1)?;
        let metadata_retained = batch_retained_cost(self.baseline.origins.capacity())?
            .checked_add(self.metadata_retained)
            .ok_or_else(|| invalid("batch metadata overflow"))?;
        let peak = metadata_retained
            .checked_add(metadata_cost(self.max_origins)?)
            .ok_or_else(|| invalid("batch metadata overflow"))?;
        if peak > self.max_metadata_bytes {
            return Err(invalid("batch origin metadata budget exceeded"));
        }
        let mut origins = Vec::new();
        reserve(&mut origins, self.baseline.origins.len())?;
        if origins.capacity() > self.max_origins {
            return Err(invalid("batch origin capacity exceeds admitted bound"));
        }
        origins.extend(self.baseline.origins.iter().cloned());
        let staged = OriginRegistry {
            baseline: Arc::new(Baseline { epoch, next_id: self.baseline.next_id, origins }),
            max_origins: self.max_origins,
            max_metadata_bytes: self.max_metadata_bytes,
            metadata_retained,
        };
        Ok(OriginBatch { live: self, staged, failed: false, pending: false, cancel: None, metadata_peak: peak })
    }

    /// Invalidates all outstanding tokens, retaining monotonically increasing
    /// IDs. A restarted/reloaded journal must reconstruct its registry before
    /// emitting references; an empty registry redefines embedded origins.
    pub fn clear_after_journal_failure(&mut self) -> io::Result<()> {
        let epoch = add(self.baseline.epoch, 1)?;
        self.baseline = Arc::new(Baseline { epoch, next_id: self.baseline.next_id, origins: Vec::new() });
        Ok(())
    }

    /// No writer is touched and no registry state changes on planning failure.
    /// Traversal is O(pieces * log(max_origins)), plus O(k^2) metadata moves
    /// for inserting k new origins into the bounded sorted definition vector.
    /// Detached metadata checks are bounded. No origin raw byte scan,
    /// per-character list or per-piece Vec.
    pub fn plan(&self, snapshot: Snapshot, range: Range<u64>, cancel: Arc<AtomicBool>) -> io::Result<CapsulePlan> {
        self.plan_checked(snapshot, range, None, cancel.clone(), || poll(&cancel))
    }

    /// Initial full decoded base, including BOM-only/empty documents whose tree
    /// has no source pieces. Caller supplies the origin; it is never inferred.
    /// Nonempty bases must consist solely of this full origin in logical order.
    /// Edited/mixed bases use `plan` instead. Dedup/caps/admission are identical.
    pub fn plan_seed(
        &self,
        snapshot: Snapshot,
        origin: DecodedSnapshot,
        cancel: Arc<AtomicBool>,
    ) -> io::Result<CapsulePlan> {
        let len = snapshot.len();
        self.plan_checked(snapshot, 0..len, Some(origin), cancel.clone(), || poll(&cancel))
    }

    fn plan_checked(
        &self,
        snapshot: Snapshot,
        range: Range<u64>,
        seed: Option<DecodedSnapshot>,
        cancel: Arc<AtomicBool>,
        mut check: impl FnMut() -> io::Result<()>,
    ) -> io::Result<CapsulePlan> {
        check()?;
        if range.start > range.end || range.end > snapshot.len() {
            return Err(invalid("capsule range outside snapshot"));
        }
        let next_epoch = add(self.baseline.epoch, 1)?;
        let mut next_id = self.baseline.next_id;
        let mut definitions = Vec::new();
        self.check_capacity(self.baseline.origins.capacity())?;
        let mut physical_len = HEADER_LEN;
        let mut logical_len = 0;
        let mut empty_seed_id = None;
        if let Some(origin) = &seed {
            if count(origin.logical_bytes().len())? != snapshot.len() {
                return Err(invalid("seed origin logical length mismatch"));
            }
            self.define(origin, &mut definitions, &mut next_id, &mut physical_len)?;
            if snapshot.is_empty() {
                empty_seed_id = Some(
                    lookup(&self.baseline.origins, origin)
                        .or_else(|| lookup(&definitions, origin))
                        .ok_or_else(|| invalid("missing empty seed origin"))?
                        .id,
                );
                physical_len = add(physical_len, REFERENCE_LEN)?;
            }
            check()?;
        }
        for chunk in snapshot.source_chunks(range.clone())? {
            check()?;
            let chunk = chunk.map_err(io::Error::other)?;
            let len = match chunk {
                SourceChunk::Decoded { origin, range, bytes } => {
                    let len = decoded_len(origin, &range, bytes.len())?;
                    if let Some(seed) = &seed
                        && (seed.paths().directory != origin.paths().directory || range.start != logical_len)
                    {
                        return Err(invalid("seed source identity or logical order mismatch"));
                    }
                    self.define(origin, &mut definitions, &mut next_id, &mut physical_len)?;
                    physical_len = add(physical_len, REFERENCE_LEN)?;
                    len
                }
                SourceChunk::Original { bytes, .. } | SourceChunk::Added(bytes) => {
                    if seed.is_some() {
                        return Err(invalid("seed must reference only its decoded origin"));
                    }
                    let len = count(bytes.len())?;
                    physical_len = add(physical_len, add(LOGICAL_HEADER_LEN, len)?)?;
                    len
                }
                SourceChunk::Opaque { encoding, bytes, raw_bytes, raw_offsets } => {
                    if seed.is_some() {
                        return Err(invalid("seed must reference only its decoded origin"));
                    }
                    validate_opaque(bytes, raw_bytes, raw_offsets)?;
                    physical_len = add(physical_len, opaque_len(encoding, raw_bytes.len(), raw_offsets.len())?)?;
                    count(bytes.len())?
                }
                SourceChunk::Preserved { encoding, bytes, raw_bytes, raw_offsets } => {
                    if seed.is_some() {
                        return Err(invalid("seed must reference only its decoded origin"));
                    }
                    validate_preserved(bytes, raw_bytes, raw_offsets)?;
                    physical_len = add(
                        physical_len,
                        add(opaque_len(encoding, raw_bytes.len(), raw_offsets.len())?, count(bytes.len())?)?,
                    )?;
                    count(bytes.len())?
                }
            };
            if len == 0 {
                return Err(invalid("empty capsule source piece"));
            }
            logical_len = add(logical_len, len)?;
            check()?;
        }
        if logical_len != range.end - range.start {
            return Err(invalid("capsule logical length mismatch"));
        }
        physical_len = add(physical_len, 1)?; // explicit End
        check()?;
        Ok(CapsulePlan {
            snapshot,
            range,
            baseline: self.baseline.clone(),
            definitions,
            next_id,
            next_epoch,
            physical_len,
            logical_len,
            empty_seed_id,
            cancel,
        })
    }

    fn define(
        &self,
        origin: &DecodedSnapshot,
        definitions: &mut Vec<Origin>,
        next_id: &mut u64,
        physical_len: &mut u64,
    ) -> io::Result<()> {
        if lookup(&self.baseline.origins, origin).or_else(|| lookup(definitions, origin)).is_some() {
            return Ok(());
        }
        let total = self
            .baseline
            .origins
            .len()
            .checked_add(definitions.len())
            .and_then(|n| n.checked_add(1))
            .ok_or_else(|| invalid("origin count overflow"))?;
        if total > self.max_origins {
            return Err(invalid("origin count cap exceeded"));
        }
        let needed = definitions.len().checked_add(1).ok_or_else(|| invalid("origin capacity overflow"))?;
        self.check_capacity(
            self.baseline
                .origins
                .capacity()
                .checked_add(definitions.capacity().max(needed))
                .ok_or_else(|| invalid("origin capacity overflow"))?,
        )?;
        let id = *next_id;
        *next_id = add(*next_id, 1)?;
        *physical_len = add(*physical_len, definition_len(origin)?)?;
        reserve(definitions, 1)?;
        self.check_capacity(
            self.baseline
                .origins
                .capacity()
                .checked_add(definitions.capacity())
                .ok_or_else(|| invalid("origin capacity overflow"))?,
        )?;
        let at = definitions
            .binary_search_by(|entry| entry.snapshot.paths().directory.cmp(&origin.paths().directory))
            .unwrap_err();
        definitions.insert(at, Origin { id, snapshot: origin.clone() });
        Ok(())
    }

    fn check_capacity(&self, slots: usize) -> io::Result<()> {
        let cost = metadata_cost(slots)?
            .checked_add(self.metadata_retained)
            .ok_or_else(|| invalid("origin metadata overflow"))?;
        if self.metadata_retained != 0 && slots > self.max_origins {
            return Err(invalid("batch origin capacity exceeds admitted bound"));
        }
        if cost > self.max_metadata_bytes { Err(invalid("origin metadata budget exceeded")) } else { Ok(()) }
    }

    /// Reject stale/foreign planning before touching a writer. Keep the entire
    /// start/read/outer-batch-admission/admit sequence non-interleaved on ONE
    /// worker; this check does not reserve IDs or authorize parallel readers.
    pub fn start(&self, plan: CapsulePlan) -> io::Result<CapsuleReader> {
        poll(&plan.cancel)?;
        if !Arc::ptr_eq(&self.baseline, &plan.baseline) {
            return Err(invalid("stale or foreign capsule plan"));
        }
        Ok(plan.into_reader())
    }

    /// Caller asserts whole outer batch FIFO admission. This is NOT a flush or
    /// durability acknowledgement. No partial/failed reader can create a token.
    pub fn admit(&mut self, token: AdmissionToken) -> io::Result<()> {
        if !Arc::ptr_eq(&self.baseline, &token.baseline) {
            return Err(invalid("stale or foreign capsule admission"));
        }
        let total = self
            .baseline
            .origins
            .len()
            .checked_add(token.definitions.len())
            .ok_or_else(|| invalid("origin count overflow"))?;
        if total > self.max_origins {
            return Err(invalid("origin count cap exceeded"));
        }
        self.check_capacity(
            total.max(
                self.baseline
                    .origins
                    .capacity()
                    .checked_add(token.definitions.capacity())
                    .ok_or_else(|| invalid("origin capacity overflow"))?,
            ),
        )?;
        let mut origins = Vec::new();
        reserve(&mut origins, total)?;
        self.check_capacity(
            origins.capacity().max(
                self.baseline
                    .origins
                    .capacity()
                    .checked_add(token.definitions.capacity())
                    .ok_or_else(|| invalid("origin capacity overflow"))?,
            ),
        )?;
        // Merge sorted metadata in bounded steps, without re-reading any raw.
        let mut old = self.baseline.origins.iter().peekable();
        let mut new = token.definitions.into_iter().peekable();
        while old.peek().is_some() || new.peek().is_some() {
            poll(&token.cancel)?;
            let take_old = match (old.peek(), new.peek()) {
                (Some(a), Some(b)) => a.snapshot.paths().directory < b.snapshot.paths().directory,
                (Some(_), None) => true,
                _ => false,
            };
            origins.push(if take_old { old.next().unwrap().clone() } else { new.next().unwrap() });
        }
        poll(&token.cancel)?;
        self.baseline = Arc::new(Baseline { epoch: token.next_epoch, next_id: token.next_id, origins });
        Ok(())
    }
}

fn batch_retained_cost(committed_capacity: usize) -> io::Result<usize> {
    // Existing metadata_cost accounts for TWO staged Baseline headers and
    // directory slots. Retain the ACTUAL committed directory as a third one,
    // plus all live/shadow/guard/scoped-wrapper headers and all three Arc control
    // blocks (two native atomic reference counts each). Plans/readers exclusively
    // borrow the guard, so only ONE capsule's definition Vec can be retained.
    // During merge old-staged + definitions + new-staged <=2*max_origins slots;
    // during definition reallocation old/new definitions fit the same bound.
    committed_capacity
        .checked_mul(size_of::<Origin>())
        .and_then(|n| n.checked_add(size_of::<Baseline>()))
        .and_then(|n| n.checked_add(size_of::<OriginRegistry>()))
        .and_then(|n| n.checked_add(size_of::<OriginBatch<'_>>()))
        .and_then(|n| n.checked_add(size_of::<OriginBatchPlan<'_, '_>>()))
        .and_then(|n| n.checked_add(size_of::<OriginBatchReader<'_, '_>>()))
        .and_then(|n| n.checked_add(6 * size_of::<usize>()))
        .ok_or_else(|| invalid("batch metadata overflow"))
}

/// One exclusive FIFO-worker scope for an outer journal batch. Staged reader
/// admission is NOT live publication. Call commit only after the caller's
/// JournalCommand::CommitBatch queue admission succeeds; later flush establishes
/// durability. Queue/write failure must drop/abort this guard, never commit it.
/// Every capsule binds the SAME cancellation Arc; no per-capsule flag list.
/// No filesystem work occurs on guard/wrapper drop. Backing owners and pins must
/// still be retained/retired on the worker by the caller.
#[must_use = "commit only after complete outer batch admission, otherwise abort/drop"]
pub struct OriginBatch<'registry> {
    live: &'registry mut OriginRegistry,
    staged: OriginRegistry,
    failed: bool,
    pending: bool,
    cancel: Option<Arc<AtomicBool>>,
    metadata_peak: usize,
}

impl<'registry> OriginBatch<'registry> {
    pub fn origin_count(&self) -> usize {
        self.staged.origin_count()
    }
    pub fn epoch(&self) -> u64 {
        self.staged.epoch()
    }
    pub fn metadata_reserved_bytes(&self) -> usize {
        self.metadata_peak
    }
    pub fn is_poisoned(&self) -> bool {
        self.failed
    }

    fn ready(&self) -> io::Result<()> {
        if self.failed {
            return Err(invalid("origin batch is poisoned"));
        }
        if self.pending {
            return Err(invalid("origin batch has an unfinished capsule"));
        }
        if let Some(cancel) = &self.cancel {
            poll(cancel)?;
        }
        Ok(())
    }
    fn bind(&mut self, cancel: &Arc<AtomicBool>) -> io::Result<()> {
        self.ready()?;
        poll(cancel)?;
        if let Some(bound) = &self.cancel {
            if !Arc::ptr_eq(bound, cancel) {
                return Err(invalid("origin batch cancellation flag changed"));
            }
        } else {
            self.cancel = Some(cancel.clone());
        }
        Ok(())
    }
    fn checked<T>(&mut self, result: io::Result<T>) -> io::Result<T> {
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    pub fn plan<'batch>(
        &'batch mut self,
        snapshot: Snapshot,
        range: Range<u64>,
        cancel: Arc<AtomicBool>,
    ) -> io::Result<OriginBatchPlan<'batch, 'registry>> {
        let result = self.bind(&cancel).and_then(|()| self.staged.plan(snapshot, range, cancel));
        let plan = self.checked(result)?;
        self.pending = true;
        Ok(OriginBatchPlan { batch: Some(self), plan: Some(plan) })
    }

    pub fn plan_seed<'batch>(
        &'batch mut self,
        snapshot: Snapshot,
        origin: DecodedSnapshot,
        cancel: Arc<AtomicBool>,
    ) -> io::Result<OriginBatchPlan<'batch, 'registry>> {
        let result = self.bind(&cancel).and_then(|()| self.staged.plan_seed(snapshot, origin, cancel));
        let plan = self.checked(result)?;
        self.pending = true;
        Ok(OriginBatchPlan { batch: Some(self), plan: Some(plan) })
    }

    /// Infallible publication after readiness/cancellation validation; no new
    /// directory allocation or callbacks after live publication. Even an empty
    /// successful batch changes baseline identity/epoch, staling old plans.
    pub fn commit(mut self) -> io::Result<()> {
        let result = self.ready();
        self.checked(result)?;
        self.live.baseline = self.staged.baseline.clone();
        Ok(())
    }
    pub fn abort(self) {}
}

/// The underlying CapsulePlan is deliberately private: it cannot escape into
/// the standalone start/admit API or outlive the exclusive batch borrow.
#[must_use = "start/read/admit this capsule, or the batch is poisoned"]
pub struct OriginBatchPlan<'batch, 'registry> {
    batch: Option<&'batch mut OriginBatch<'registry>>,
    plan: Option<CapsulePlan>,
}
impl<'batch, 'registry> OriginBatchPlan<'batch, 'registry> {
    pub fn physical_len(&self) -> u64 {
        self.plan.as_ref().expect("owned batch plan").physical_len()
    }
    pub fn logical_len(&self) -> u64 {
        self.plan.as_ref().expect("owned batch plan").logical_len()
    }
    pub fn new_origin_count(&self) -> usize {
        self.plan.as_ref().expect("owned batch plan").new_origin_count()
    }

    pub fn start(mut self) -> io::Result<OriginBatchReader<'batch, 'registry>> {
        let batch = self.batch.take().expect("owned batch borrow");
        let plan = self.plan.take().expect("owned batch plan");
        let result = if batch.failed { Err(invalid("origin batch is poisoned")) } else { batch.staged.start(plan) };
        let reader = batch.checked(result)?;
        Ok(OriginBatchReader { batch: Some(batch), reader: Some(reader) })
    }
}
impl Drop for OriginBatchPlan<'_, '_> {
    fn drop(&mut self) {
        if let Some(batch) = &mut self.batch {
            batch.failed = true;
        }
    }
}

/// Owns one bounded serializer reader and exclusively borrows its batch. A read
/// error, drop without admission, or failed completion poisons the whole guard.
/// No public unchecked reader/token extraction or mutable registry seam exists.
#[must_use = "complete serialization and admit only after capsule bytes are queued"]
pub struct OriginBatchReader<'batch, 'registry> {
    batch: Option<&'batch mut OriginBatch<'registry>>,
    reader: Option<CapsuleReader>,
}
impl OriginBatchReader<'_, '_> {
    /// Complete-reader validation plus STAGED admission. No IDs become live.
    /// The caller must have accepted this capsule's bytes before invoking it;
    /// outer CommitBatch admission still precedes OriginBatch::commit.
    pub fn admit(mut self) -> io::Result<()> {
        let batch = self.batch.take().expect("owned batch borrow");
        let reader = self.reader.take().expect("owned batch reader");
        if batch.failed {
            return Err(invalid("origin batch is poisoned"));
        }
        let token = batch.checked(reader.finish())?;
        let result = batch.staged.admit(token);
        batch.checked(result)?;
        batch.pending = false;
        Ok(())
    }
}
impl Read for OriginBatchReader<'_, '_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let batch = self.batch.as_mut().expect("owned batch borrow");
        if batch.failed {
            return Err(invalid("origin batch is poisoned"));
        }
        let result = self.reader.as_mut().expect("owned batch reader").read(output);
        batch.checked(result)
    }
}
impl Drop for OriginBatchReader<'_, '_> {
    fn drop(&mut self) {
        if let Some(batch) = &mut self.batch {
            batch.failed = true;
        }
    }
}

fn decoded_len(origin: &DecodedSnapshot, range: &Range<u64>, bytes_len: usize) -> io::Result<u64> {
    if range.start > range.end
        || range.end > count(origin.logical_bytes().len())?
        || range.end - range.start != count(bytes_len)?
    {
        return Err(invalid("invalid decoded logical range"));
    }
    Ok(range.end - range.start)
}
fn definition_len(origin: &DecodedSnapshot) -> io::Result<u64> {
    let interpretation = origin.interpretation();
    label_len(interpretation.encoding)?;
    if u64::from(interpretation.bom_len) > count(origin.raw_bytes().len())? {
        return Err(invalid("invalid origin BOM length"));
    }
    add(add(DEFINITION_HEADER_LEN, count(interpretation.encoding.name().len())?)?, count(origin.raw_bytes().len())?)
}
fn label_len(encoding: Encoding) -> io::Result<u16> {
    u16::try_from(encoding.name().len()).map_err(|_| invalid("encoding label too long"))
}
fn opaque_len(encoding: Encoding, raw: usize, table: usize) -> io::Result<u64> {
    label_len(encoding)?;
    add(add(add(OPAQUE_HEADER_LEN, count(encoding.name().len())?)?, count(raw)?)?, count(table)?)
}
fn endpoint(table: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(table[at..at + 4].try_into().unwrap())
}
fn validate_preserved(bytes: &[u8], raw: &[u8], table: &[u8]) -> io::Result<()> {
    if bytes.is_empty()
        || bytes.len() > MAX_READ_BYTES
        || raw.is_empty()
        || raw.len() > MAX_READ_BYTES
        || table.len() > MAX_READ_BYTES
        || table.len() != (bytes.len() + 1) * 4
        || std::str::from_utf8(bytes).is_err()
    {
        return Err(invalid("invalid bounded preserved block"));
    }
    let first = endpoint(table, 0);
    let last = endpoint(table, table.len() - 4);
    if first & (1 << 31) != 0 || last & (1 << 31) != 0 || last.checked_sub(first) != Some(raw.len() as u32) {
        return Err(invalid("invalid preserved endpoint extent"));
    }
    let mut previous = first;
    for at in (4..table.len()).step_by(4) {
        let value = endpoint(table, at) & !(1 << 31);
        if value < previous || value > last {
            return Err(invalid("invalid preserved endpoint order"));
        }
        previous = value;
    }
    Ok(())
}
fn validate_opaque(bytes: &[u8], raw: &[u8], table: &[u8]) -> io::Result<()> {
    let expected =
        bytes.len().checked_add(1).and_then(|n| n.checked_mul(4)).ok_or_else(|| invalid("opaque endpoint overflow"))?;
    if bytes.is_empty()
        || bytes.len() > MAX_READ_BYTES
        || raw.len() > MAX_READ_BYTES
        || table.len() > MAX_READ_BYTES
        || table.len() != expected
        || bytes.iter().any(|&b| b != 0xff)
    {
        return Err(invalid("invalid bounded opaque block"));
    }
    let first = endpoint(table, 0);
    let mut previous = first;
    for at in (4..table.len()).step_by(4) {
        let next = endpoint(table, at);
        if next <= previous {
            return Err(invalid("invalid opaque endpoints"));
        }
        previous = next;
    }
    if u64::from(previous - first) != count(raw.len())? {
        return Err(invalid("opaque raw length mismatch"));
    }
    Ok(())
}

#[must_use = "a plan publishes no IDs until completely read and admitted"]
pub struct CapsulePlan {
    snapshot: Snapshot,
    range: Range<u64>,
    baseline: Arc<Baseline>,
    definitions: Vec<Origin>,
    next_id: u64,
    next_epoch: u64,
    physical_len: u64,
    logical_len: u64,
    empty_seed_id: Option<u64>,
    cancel: Arc<AtomicBool>,
}
impl CapsulePlan {
    pub fn physical_len(&self) -> u64 {
        self.physical_len
    }
    pub fn logical_len(&self) -> u64 {
        self.logical_len
    }
    pub fn new_origin_count(&self) -> usize {
        self.definitions.len()
    }
    fn into_reader(self) -> CapsuleReader {
        let mut header = b"KWCP".to_vec();
        header.extend_from_slice(&CAPSULE_VERSION.to_le_bytes());
        header.extend_from_slice(&0_u16.to_le_bytes());
        header.extend_from_slice(&self.logical_len.to_le_bytes());
        let cursor = self.range.start;
        let empty_seed_id = self.empty_seed_id;
        CapsuleReader {
            plan: self,
            pending: header,
            pending_at: 0,
            payload: Payload::None,
            definition: 0,
            cursor,
            empty_seed_id,
            emitted: 0,
            end: false,
            failed: false,
        }
    }
}

/// Private fields deliberately prevent manufacturing or cloning admission.
#[must_use = "admit only after the complete outer journal batch is accepted"]
pub struct AdmissionToken {
    baseline: Arc<Baseline>,
    definitions: Vec<Origin>,
    next_id: u64,
    next_epoch: u64,
    cancel: Arc<AtomicBool>,
}

enum Payload {
    None,
    Definition { index: usize, offset: usize },
    Logical { cursor: u64, end: u64 },
    // Only the current already-capped detached block is copied. No piece list.
    Opaque { raw: Vec<u8>, table: Vec<u8>, offset: usize },
    Preserved { bytes: Vec<u8>, raw: Vec<u8>, table: Vec<u8>, offset: usize },
}

pub struct CapsuleReader {
    plan: CapsulePlan,
    pending: Vec<u8>,
    pending_at: usize,
    payload: Payload,
    definition: usize,
    cursor: u64,
    empty_seed_id: Option<u64>,
    emitted: u64,
    end: bool,
    failed: bool,
}
impl CapsuleReader {
    /// Consume after the last byte (EOF need not be polled). Failure/cancellation
    /// is sticky even if the cancellation flag is subsequently reset.
    pub fn finish(self) -> io::Result<AdmissionToken> {
        poll(&self.plan.cancel)?;
        if self.failed || !self.end || self.emitted != self.plan.physical_len || self.pending_at != self.pending.len() {
            return Err(invalid("capsule serialization incomplete or failed"));
        }
        Ok(AdmissionToken {
            baseline: self.plan.baseline,
            definitions: self.plan.definitions,
            next_id: self.plan.next_id,
            next_epoch: self.plan.next_epoch,
            cancel: self.plan.cancel,
        })
    }

    fn schedule(&mut self) -> io::Result<()> {
        self.pending.clear();
        self.pending_at = 0;
        if self.definition < self.plan.definitions.len() {
            let index = self.definition;
            let entry = &self.plan.definitions[index];
            let interpretation = entry.snapshot.interpretation();
            self.pending.push(1);
            self.pending.extend_from_slice(&entry.id.to_le_bytes());
            self.pending.extend_from_slice(&label_len(interpretation.encoding)?.to_le_bytes());
            self.pending.push(interpretation.bom_len);
            self.pending.extend_from_slice(&count(entry.snapshot.raw_bytes().len())?.to_le_bytes());
            self.pending.extend_from_slice(interpretation.encoding.name().as_bytes());
            self.payload = Payload::Definition { index, offset: 0 };
            self.definition += 1;
        } else if let Some(id) = self.empty_seed_id.take() {
            self.pending.push(2);
            self.pending.extend_from_slice(&id.to_le_bytes());
            self.pending.extend_from_slice(&0_u64.to_le_bytes());
            self.pending.extend_from_slice(&0_u64.to_le_bytes());
        } else if self.cursor < self.plan.range.end {
            let mut chunks = self.plan.snapshot.source_chunks(self.cursor..self.plan.range.end)?;
            let chunk =
                chunks.next().ok_or_else(|| invalid("missing capsule source piece"))?.map_err(io::Error::other)?;
            match chunk {
                SourceChunk::Decoded { origin, range, bytes } => {
                    let len = decoded_len(origin, &range, bytes.len())?;
                    let entry = lookup(&self.plan.baseline.origins, origin)
                        .or_else(|| lookup(&self.plan.definitions, origin))
                        .ok_or_else(|| invalid("unplanned capsule origin"))?;
                    self.pending.push(2);
                    self.pending.extend_from_slice(&entry.id.to_le_bytes());
                    self.pending.extend_from_slice(&range.start.to_le_bytes());
                    self.pending.extend_from_slice(&range.end.to_le_bytes());
                    self.cursor = add(self.cursor, len)?;
                }
                SourceChunk::Original { bytes, .. } | SourceChunk::Added(bytes) => {
                    let len = count(bytes.len())?;
                    self.pending.push(3);
                    self.pending.extend_from_slice(&len.to_le_bytes());
                    let end = add(self.cursor, len)?;
                    self.payload = Payload::Logical { cursor: self.cursor, end };
                    self.cursor = end;
                }
                SourceChunk::Opaque { encoding, bytes, raw_bytes, raw_offsets } => {
                    validate_opaque(bytes, raw_bytes, raw_offsets)?;
                    self.pending.push(4);
                    self.pending.extend_from_slice(&label_len(encoding)?.to_le_bytes());
                    self.pending.extend_from_slice(&count(bytes.len())?.to_le_bytes());
                    self.pending.extend_from_slice(&count(raw_bytes.len())?.to_le_bytes());
                    self.pending.extend_from_slice(&count(raw_offsets.len())?.to_le_bytes());
                    self.pending.extend_from_slice(encoding.name().as_bytes());
                    let first = endpoint(raw_offsets, 0);
                    let mut table = Vec::with_capacity(raw_offsets.len());
                    for at in (0..raw_offsets.len()).step_by(4) {
                        table.extend_from_slice(&(endpoint(raw_offsets, at) - first).to_le_bytes());
                    }
                    self.payload = Payload::Opaque { raw: raw_bytes.to_vec(), table, offset: 0 };
                    self.cursor = add(self.cursor, count(bytes.len())?)?;
                }
                SourceChunk::Preserved { encoding, bytes, raw_bytes, raw_offsets } => {
                    validate_preserved(bytes, raw_bytes, raw_offsets)?;
                    self.pending.push(5);
                    self.pending.extend_from_slice(&label_len(encoding)?.to_le_bytes());
                    self.pending.extend_from_slice(&count(bytes.len())?.to_le_bytes());
                    self.pending.extend_from_slice(&count(raw_bytes.len())?.to_le_bytes());
                    self.pending.extend_from_slice(&count(raw_offsets.len())?.to_le_bytes());
                    self.pending.extend_from_slice(encoding.name().as_bytes());
                    let first = endpoint(raw_offsets, 0);
                    let mut table = Vec::with_capacity(raw_offsets.len());
                    for at in (0..raw_offsets.len()).step_by(4) {
                        let value = endpoint(raw_offsets, at);
                        let relative = ((value & !(1 << 31)) - first) | (value & (1 << 31));
                        table.extend_from_slice(&relative.to_le_bytes());
                    }
                    self.payload =
                        Payload::Preserved { bytes: bytes.to_vec(), raw: raw_bytes.to_vec(), table, offset: 0 };
                    self.cursor = add(self.cursor, count(bytes.len())?)?;
                }
            }
        } else if !self.end {
            self.pending.push(0);
            self.end = true;
        }
        Ok(())
    }

    fn read_inner(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        poll(&self.plan.cancel)?;
        if self.failed {
            return Err(io::Error::other("capsule reader previously failed"));
        }
        let output_len = output.len().min(MAX_READ_BYTES);
        loop {
            poll(&self.plan.cancel)?;
            let n = if self.pending_at < self.pending.len() {
                let n = output_len.min(self.pending.len() - self.pending_at);
                output[..n].copy_from_slice(&self.pending[self.pending_at..self.pending_at + n]);
                self.pending_at += n;
                n
            } else {
                match &mut self.payload {
                    Payload::Definition { index, offset } => {
                        let raw = self.plan.definitions[*index].snapshot.raw_bytes();
                        let n = output_len.min(raw.len() - *offset);
                        output[..n].copy_from_slice(&raw[*offset..*offset + n]);
                        *offset += n;
                        n
                    }
                    Payload::Logical { cursor, end } => {
                        if cursor == end {
                            0
                        } else {
                            let mut chunks = self.plan.snapshot.chunks(*cursor..*end)?;
                            let bytes = chunks
                                .next()
                                .ok_or_else(|| invalid("missing logical payload"))?
                                .map_err(io::Error::other)?;
                            let n = output_len.min(bytes.len());
                            if n == 0 {
                                return Err(invalid("empty logical payload"));
                            }
                            output[..n].copy_from_slice(&bytes[..n]);
                            *cursor = add(*cursor, count(n)?)?;
                            n
                        }
                    }
                    Payload::Opaque { raw, table, offset } => {
                        let bytes = if *offset < raw.len() { &raw[*offset..] } else { &table[*offset - raw.len()..] };
                        let n = output_len.min(bytes.len());
                        output[..n].copy_from_slice(&bytes[..n]);
                        *offset += n;
                        n
                    }
                    Payload::Preserved { bytes, raw, table, offset } => {
                        let payload = if *offset < bytes.len() {
                            &bytes[*offset..]
                        } else if *offset < bytes.len() + raw.len() {
                            &raw[*offset - bytes.len()..]
                        } else {
                            &table[*offset - bytes.len() - raw.len()..]
                        };
                        let n = output_len.min(payload.len());
                        output[..n].copy_from_slice(&payload[..n]);
                        *offset += n;
                        n
                    }
                    Payload::None => 0,
                }
            };
            if n != 0 {
                self.emitted = add(self.emitted, count(n)?)?;
                if self.emitted > self.plan.physical_len {
                    return Err(invalid("capsule exceeded planned length"));
                }
                return Ok(n);
            }
            self.payload = Payload::None;
            if self.end {
                if self.emitted != self.plan.physical_len {
                    return Err(invalid("capsule physical length mismatch"));
                }
                return Ok(0);
            }
            self.schedule()?;
        }
    }
}
impl Read for CapsuleReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let result = self.read_inner(output);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kwil_core_text::{
        Buffer, TextFile,
        decoded_backing::{DecodedBacking, Interpretation},
    };
    use std::{
        cell::{Cell, RefCell},
        path::PathBuf,
        sync::atomic::AtomicU64,
    };

    struct Fixture {
        directory: PathBuf,
        next: Cell<u64>,
        owners: RefCell<Vec<DecodedBacking>>,
    }
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let directory = std::env::temp_dir().join(format!(
                "kwil-capsule-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&directory).unwrap();
            Self { directory, next: Cell::new(0), owners: RefCell::new(Vec::new()) }
        }
        fn file(&self, bytes: &[u8]) -> TextFile {
            let next = self.next.get();
            self.next.set(next + 1);
            let path = self.directory.join(format!("source-{next}"));
            std::fs::write(&path, bytes).unwrap();
            TextFile::open(&path).unwrap()
        }
        fn decoded(&self, raw: &[u8], encoding: Encoding, bom: usize) -> Buffer {
            let owner = DecodedBacking::prepare(
                &self.file(raw),
                Interpretation::new(encoding, bom).unwrap(),
                &self.directory,
                &AtomicBool::new(false),
            )
            .unwrap();
            let buffer = Buffer::from_decoded(owner.snapshot());
            self.owners.borrow_mut().push(owner);
            buffer
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            for owner in self.owners.get_mut().drain(..) {
                assert!(owner.try_retire().is_ok(), "decoded owner must retire explicitly on test worker");
            }
            std::fs::remove_dir_all(&self.directory).unwrap();
        }
    }
    fn cancel() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }
    fn registry() -> OriginRegistry {
        OriginRegistry::new(8, 8192)
    }
    fn plan(registry: &OriginRegistry, snapshot: &Snapshot) -> CapsulePlan {
        registry.plan(snapshot.clone(), 0..snapshot.len(), cancel()).unwrap()
    }
    fn serialize(registry: &OriginRegistry, plan: CapsulePlan, size: usize) -> (Vec<u8>, AdmissionToken) {
        let expected = plan.physical_len();
        let mut reader = registry.start(plan).unwrap();
        let mut output = Vec::new();
        let mut buffer = vec![0; size];
        loop {
            let n = reader.read(&mut buffer).unwrap();
            assert!(n <= MAX_READ_BYTES);
            if n == 0 {
                break;
            }
            output.extend_from_slice(&buffer[..n]);
        }
        assert_eq!(output.len() as u64, expected, "physical plan must match every read segmentation");
        (output, reader.finish().unwrap())
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Record {
        Definition { id: u64, encoding: String, bom: u8, raw: Vec<u8> },
        Reference { id: u64, range: Range<u64> },
        Text(Vec<u8>),
        Opaque { encoding: String, logical: u64, raw: Vec<u8>, endpoints: Vec<u32> },
    }
    // Structural fixture inspector ONLY, not the production capsule decoder.
    fn inspect(bytes: &[u8], known: &[(u64, u64)]) -> (u64, Vec<Record>) {
        fn take<'a>(bytes: &mut &'a [u8], len: usize) -> &'a [u8] {
            let (head, rest) = bytes.split_at(len);
            *bytes = rest;
            head
        }
        fn u64_(bytes: &mut &[u8]) -> u64 {
            u64::from_le_bytes(take(bytes, 8).try_into().unwrap())
        }
        fn u16_(bytes: &mut &[u8]) -> u16 {
            u16::from_le_bytes(take(bytes, 2).try_into().unwrap())
        }
        let mut bytes = bytes;
        assert_eq!(take(&mut bytes, 4), b"KWCP");
        assert_eq!(u16_(&mut bytes), CAPSULE_VERSION);
        assert_eq!(u16_(&mut bytes), 0);
        let expected = u64_(&mut bytes);
        let mut logical = 0;
        let mut records = Vec::new();
        let mut defined: Vec<_> = known.to_vec();
        loop {
            match take(&mut bytes, 1)[0] {
                0 => {
                    assert!(bytes.is_empty());
                    break;
                }
                1 => {
                    let id = u64_(&mut bytes);
                    assert!(!defined.iter().any(|&(old, _)| old == id));
                    let label_len = u16_(&mut bytes) as usize;
                    let bom = take(&mut bytes, 1)[0];
                    let raw_len = u64_(&mut bytes) as usize;
                    let encoding = String::from_utf8(take(&mut bytes, label_len).to_vec()).unwrap();
                    let raw = take(&mut bytes, raw_len).to_vec();
                    defined.push((id, u64::MAX)); // ranges asserted against fixture origins below
                    records.push(Record::Definition { id, encoding, bom, raw });
                }
                2 => {
                    let id = u64_(&mut bytes);
                    let range = u64_(&mut bytes)..u64_(&mut bytes);
                    let len =
                        defined.iter().find(|&&(old, _)| old == id).expect("reference requires admitted definition").1;
                    assert!(range.start <= range.end && range.end <= len);
                    logical += range.end - range.start;
                    records.push(Record::Reference { id, range });
                }
                3 => {
                    let len = u64_(&mut bytes);
                    logical += len;
                    records.push(Record::Text(take(&mut bytes, len as usize).to_vec()));
                }
                4 => {
                    let label_len = u16_(&mut bytes) as usize;
                    let count = u64_(&mut bytes);
                    let raw_len = u64_(&mut bytes) as usize;
                    let table_len = u64_(&mut bytes) as usize;
                    let encoding = String::from_utf8(take(&mut bytes, label_len).to_vec()).unwrap();
                    let raw = take(&mut bytes, raw_len).to_vec();
                    let endpoints: Vec<_> =
                        take(&mut bytes, table_len).as_chunks::<4>().0.iter().map(|n| u32::from_le_bytes(*n)).collect();
                    assert_eq!(endpoints.len() as u64, count + 1);
                    assert_eq!(endpoints[0], 0);
                    assert_eq!(*endpoints.last().unwrap() as usize, raw_len);
                    assert!(endpoints.windows(2).all(|pair| pair[0] < pair[1]));
                    logical += count;
                    records.push(Record::Opaque { encoding, logical: count, raw, endpoints });
                }
                tag => panic!("unknown fixture record {tag}"),
            }
        }
        assert_eq!(logical, expected, "logical contributions must match header independently of physical bytes");
        (expected, records)
    }

    #[test]
    fn capsule_raw_payload_is_exact_and_logical_length_is_independent() {
        let fixture = Fixture::new();
        let raw = b"\xff\xfeA\0\0\xd8B\0\xff";
        let source = fixture.decoded(raw, Encoding::UTF16_LE, 2);
        let registry = registry();
        let snapshot = source.snapshot();
        let p = plan(&registry, &snapshot);
        assert_eq!(p.logical_len(), 4);
        assert!(p.physical_len() > p.logical_len());
        let (bytes, token) = serialize(&registry, p, 1);
        assert_eq!(
            inspect(&bytes, &[]),
            (
                4,
                vec![
                    Record::Definition { id: 1, encoding: "UTF-16LE".into(), bom: 2, raw: raw.to_vec() },
                    Record::Reference { id: 1, range: 0..4 },
                ]
            )
        );
        // No persisted native/private path dependency.
        assert!(!bytes.windows(7).any(|w| w == b"source-"));
        drop(token);
    }

    #[test]
    fn capsule_original_and_added_are_uninterpreted_logical_bytes() {
        let fixture = Fixture::new();
        let mut source = Buffer::from_file(fixture.file(b"\xef\xbb\xbf\xffA")).unwrap();
        source.insert(source.len(), b"x\0\xff").unwrap();
        let registry = registry();
        let snapshot = source.snapshot();
        let (bytes, token) = serialize(&registry, plan(&registry, &snapshot), 3);
        let (logical, records) = inspect(&bytes, &[]);
        assert_eq!(logical, 8);
        assert_eq!(records, [Record::Text(b"\xef\xbb\xbf\xffA".to_vec()), Record::Text(b"x\0\xff".to_vec())]);
        drop(token);
    }

    #[test]
    fn capsule_partial_big5_aliases_retain_logical_origin_ranges() {
        let fixture = Fixture::new();
        let source = fixture.decoded(b"\x88\x62\xa4\x40", Encoding::for_label("big5").unwrap(), 0);
        let snapshot = source.snapshot();
        // Big5 8862 maps to U+00CA U+0304 (two UTF-8 scalars, one raw alias).
        let partial = snapshot.slice(2..4).unwrap().concat(&snapshot.slice(0..2).unwrap()).unwrap();
        let registry = registry();
        let (bytes, token) = serialize(&registry, plan(&registry, &partial), 1);
        assert_eq!(
            inspect(&bytes, &[]),
            (
                4,
                vec![
                    Record::Definition { id: 1, encoding: "Big5".into(), bom: 0, raw: b"\x88\x62\xa4\x40".to_vec() },
                    Record::Reference { id: 1, range: 2..4 },
                    Record::Reference { id: 1, range: 0..2 },
                ]
            )
        );
        drop(token);
    }

    #[test]
    fn capsule_detached_opaque_keeps_raw_atoms_and_normalizes_sliced_endpoints() {
        let fixture = Fixture::new();
        let source = fixture.decoded(b"A\0\0\xd8B\0\xff", Encoding::UTF16_LE, 0);
        let mut target = Buffer::with_add_memory_limit(4096);
        target.insert_snapshot_range(0, &source.snapshot(), 0..source.len()).unwrap();
        let snapshot = target.snapshot();
        let opaque = snapshot.slice(1..2).unwrap().concat(&snapshot.slice(3..4).unwrap()).unwrap();
        let registry = registry();
        let (bytes, token) = serialize(&registry, plan(&registry, &opaque), MAX_READ_BYTES);
        assert_eq!(
            inspect(&bytes, &[]),
            (
                2,
                vec![
                    Record::Opaque {
                        encoding: "UTF-16LE".into(),
                        logical: 1,
                        raw: vec![0, 0xd8],
                        endpoints: vec![0, 2]
                    },
                    Record::Opaque { encoding: "UTF-16LE".into(), logical: 1, raw: vec![0xff], endpoints: vec![0, 1] },
                ]
            )
        );
        drop(token);
    }

    #[test]
    fn capsule_admission_is_required_before_later_references_and_deduplicates_raw() {
        let fixture = Fixture::new();
        let source = fixture.decoded(b"A\0", Encoding::UTF16_LE, 0);
        let snapshot = source.snapshot();
        let mut registry = registry();
        let dropped = plan(&registry, &snapshot);
        assert_eq!(dropped.new_origin_count(), 1);
        assert_eq!(registry.origin_count(), 0, "planning must not publish an unwritten ID");
        drop(dropped);
        let (_, token) = serialize(&registry, plan(&registry, &snapshot), 3);
        assert_eq!(registry.origin_count(), 0, "complete read is not outer-batch admission");
        registry.admit(token).unwrap();
        assert_eq!(registry.origin_count(), 1, "admission must publish the embedded origin ID");
        assert_eq!(registry.epoch(), 1);
        let duplicate = snapshot.concat(&snapshot).unwrap();
        let p = plan(&registry, &duplicate);
        assert_eq!(p.new_origin_count(), 0, "repeat references must never rewrite old raw payload");
        assert_eq!(p.physical_len(), HEADER_LEN + 2 * REFERENCE_LEN + 1);
        let (bytes, token) = serialize(&registry, p, 1);
        assert_eq!(
            inspect(&bytes, &[(1, 1)]),
            (2, vec![Record::Reference { id: 1, range: 0..1 }, Record::Reference { id: 1, range: 0..1 }])
        );
        registry.admit(token).unwrap();
    }

    #[test]
    fn capsule_checkpoint_generations_and_old_clipboard_keep_distinct_ids() {
        let fixture = Fixture::new();
        let mut source = fixture.decoded(b"\xff\xfeA\0", Encoding::UTF16_LE, 2);
        let clipboard = source.snapshot();
        let mut registry = registry();
        let (_, token) = serialize(&registry, plan(&registry, &clipboard), 3);
        registry.admit(token).unwrap();
        source.insert(1, b"x").unwrap();
        let checkpoint = fixture.decoded(b"\xff\xfeA\0x\0", Encoding::UTF16_LE, 2);
        source.checkpoint_current(checkpoint.snapshot()).unwrap();
        // Snapshot composition retains both origins, like worker-prepared move
        // ranges; importing foreign text through Buffer would detach it.
        let mixed = source.snapshot().concat(&clipboard).unwrap();
        let p = plan(&registry, &mixed);
        assert_eq!(p.new_origin_count(), 1);
        let (bytes, token) = serialize(&registry, p, 3);
        assert_eq!(
            inspect(&bytes, &[(1, 1)]),
            (
                3,
                vec![
                    Record::Definition { id: 2, encoding: "UTF-16LE".into(), bom: 2, raw: b"\xff\xfeA\0x\0".to_vec() },
                    Record::Reference { id: 2, range: 0..2 },
                    Record::Reference { id: 1, range: 0..1 },
                ]
            )
        );
        registry.admit(token).unwrap();
        assert_eq!(registry.origin_count(), 2);
        source.insert_snapshot_range(2, &clipboard, 0..1).unwrap();
        let second = fixture.decoded(b"\xff\xfeA\0x\0A\0", Encoding::UTF16_LE, 2);
        source.checkpoint_current(second.snapshot()).unwrap();
        let mixed = source.snapshot().concat(&clipboard).unwrap();
        let (bytes, token) = serialize(&registry, plan(&registry, &mixed), 3);
        let (_, records) = inspect(&bytes, &[(1, 1), (2, 2)]);
        assert!(matches!(&records[0], Record::Definition { id: 3, raw, .. } if raw == b"\xff\xfeA\0x\0A\0"));
        assert_eq!(records.last(), Some(&Record::Reference { id: 1, range: 0..1 }));
        registry.admit(token).unwrap();
    }

    #[test]
    fn capsule_typing_delta_contains_only_inserted_bytes() {
        let fixture = Fixture::new();
        let mut source = fixture.decoded(&[b'A'; 128], Encoding::UTF8, 0);
        let mut registry = registry();
        let (_, token) = serialize(&registry, plan(&registry, &source.snapshot()), 3);
        registry.admit(token).unwrap();
        source.insert(64, b"x").unwrap();
        let p = registry.plan(source.snapshot(), 64..65, cancel()).unwrap();
        assert_eq!(p.new_origin_count(), 0);
        assert_eq!(p.logical_len(), 1);
        assert_eq!(p.physical_len(), HEADER_LEN + LOGICAL_HEADER_LEN + 2);
        let (bytes, token) = serialize(&registry, p, 1);
        assert_eq!(inspect(&bytes, &[]), (1, vec![Record::Text(b"x".to_vec())]));
        registry.admit(token).unwrap();
    }

    #[test]
    fn capsule_read_segmentation_matches_physical_and_logical_plans_for_all_variants() {
        let fixture = Fixture::new();
        let decoded = fixture.decoded(b"\xff\xfeA\0\xff", Encoding::UTF16_LE, 2);
        let original = Buffer::from_file(fixture.file(b"raw\xff")).unwrap();
        let mut detached = Buffer::empty();
        detached.insert_snapshot_range(0, &decoded.snapshot(), 0..decoded.len()).unwrap();
        detached.insert(detached.len(), b"typed").unwrap();
        let mixed = decoded.snapshot().concat(&original.snapshot()).unwrap().concat(&detached.snapshot()).unwrap();
        let registry = registry();
        let (bytewise, token) = serialize(&registry, plan(&registry, &mixed), 1);
        drop(token);
        for size in [3, MAX_READ_BYTES, 2 * MAX_READ_BYTES] {
            let (bytes, token) = serialize(&registry, plan(&registry, &mixed), size);
            assert_eq!(bytes, bytewise, "wire segmentation must be independent of consumer buffer size");
            assert_eq!(inspect(&bytes, &[]).0, mixed.len());
            drop(token);
        }
    }

    #[test]
    fn capsule_stale_foreign_and_failure_clear_tokens_are_rejected() {
        let fixture = Fixture::new();
        let source = fixture.decoded(b"A", Encoding::UTF8, 0);
        let snapshot = source.snapshot();
        let mut registry = registry();
        let (_, first) = serialize(&registry, plan(&registry, &snapshot), 3);
        let (_, stale) = serialize(&registry, plan(&registry, &snapshot), 3);
        let (_, foreign) = serialize(&registry, plan(&registry, &snapshot), 3);
        let mut unrelated = super::OriginRegistry::new(8, 8192);
        assert!(unrelated.admit(foreign).is_err());
        registry.admit(first).unwrap();
        assert!(registry.admit(stale).is_err());
        let (_, prior_failure) = serialize(&registry, plan(&registry, &snapshot), 3);
        registry.clear_after_journal_failure().unwrap();
        assert!(registry.admit(prior_failure).is_err());
        assert_eq!(registry.origin_count(), 0);
        let (bytes, token) = serialize(&registry, plan(&registry, &snapshot), 3);
        assert!(matches!(&inspect(&bytes, &[]).1[0], Record::Definition { id: 2, .. }));
        registry.admit(token).unwrap();
        // Even admission of an empty, definitionless capsule stales competitors.
        let (_, a) = serialize(&registry, plan(&registry, &Snapshot::default()), 3);
        let (_, b) = serialize(&registry, plan(&registry, &Snapshot::default()), 3);
        registry.admit(a).unwrap();
        assert!(registry.admit(b).is_err());
    }

    #[test]
    fn capsule_checked_start_rejects_stale_and_foreign_plans_before_writer_output() {
        let fixture = Fixture::new();
        let source = fixture.decoded(b"A", Encoding::UTF8, 0);
        let snapshot = source.snapshot();
        let mut registry = registry();
        let foreign_plan = plan(&registry, &snapshot);
        let unrelated = OriginRegistry::new(8, 8192);
        let mut untouched = Vec::new();
        if let Ok(mut reader) = unrelated.start(foreign_plan) {
            io::copy(&mut reader, &mut untouched).unwrap();
            panic!("foreign plan must fail before output");
        }
        assert!(untouched.is_empty());
        let first = plan(&registry, &snapshot);
        let stale = plan(&registry, &snapshot);
        let (_, token) = serialize(&registry, first, 3);
        registry.admit(token).unwrap();
        if let Ok(mut reader) = registry.start(stale) {
            io::copy(&mut reader, &mut untouched).unwrap();
            panic!("competing plan must become stale before output");
        }
        assert!(untouched.is_empty());
        let before_clear = plan(&registry, &snapshot);
        registry.clear_after_journal_failure().unwrap();
        assert!(registry.start(before_clear).is_err());
    }

    #[test]
    fn capsule_planning_caps_ranges_and_overflows_fail_without_mutation() {
        let fixture = Fixture::new();
        let first = fixture.decoded(b"A", Encoding::UTF8, 0);
        let second = fixture.decoded(b"B", Encoding::UTF8, 0);
        let snapshot = first.snapshot().concat(&second.snapshot()).unwrap();
        let capped = OriginRegistry::new(1, 8192);
        let mut untouched = Vec::new();
        let result = capped.plan(snapshot.clone(), 0..snapshot.len(), cancel());
        if let Ok(p) = result {
            io::copy(&mut p.into_reader(), &mut untouched).unwrap();
            panic!("origin cap must refuse plan");
        }
        assert!(untouched.is_empty(), "planning error must leave writer untouched");
        assert_eq!(capped.origin_count(), 0);
        assert_eq!(capped.epoch(), 0);
        assert!(OriginRegistry::new(2, 0).plan(snapshot.clone(), 0..snapshot.len(), cancel()).is_err());
        assert!(capped.plan(snapshot.clone(), Range { start: 2, end: 1 }, cancel()).is_err());
        assert!(capped.plan(snapshot.clone(), 0..u64::MAX, cancel()).is_err());
        assert!(add(u64::MAX, 1).is_err());
        assert!(metadata_cost(usize::MAX).is_err());
        let mut exhausted = registry();
        Arc::get_mut(&mut exhausted.baseline).unwrap().next_id = u64::MAX;
        assert!(exhausted.plan(snapshot.clone(), 0..snapshot.len(), cancel()).is_err());
        assert_eq!(exhausted.origin_count(), 0);
        Arc::get_mut(&mut exhausted.baseline).unwrap().epoch = u64::MAX;
        assert!(exhausted.plan(Snapshot::default(), 0..0, cancel()).is_err());
        assert!(exhausted.clear_after_journal_failure().is_err());
    }

    #[test]
    fn capsule_cancellation_and_incomplete_or_failed_reads_never_publish_ids() {
        let fixture = Fixture::new();
        let source = fixture.decoded(b"A", Encoding::UTF8, 0);
        let snapshot = source.snapshot();
        let registry = registry();
        let flag = cancel();
        flag.store(true, Ordering::Relaxed);
        assert!(
            matches!(registry.plan(snapshot.clone(), 0..snapshot.len(), flag.clone()), Err(e) if e.kind() == io::ErrorKind::Interrupted)
        );
        flag.store(false, Ordering::Relaxed);
        let mut polls = 0;
        let result = registry.plan_checked(snapshot.clone(), 0..snapshot.len(), None, flag.clone(), || {
            polls += 1;
            if polls == 3 { Err(io::ErrorKind::Interrupted.into()) } else { Ok(()) }
        });
        assert!(matches!(result, Err(e) if e.kind() == io::ErrorKind::Interrupted));
        assert_eq!(polls, 3, "planning polls between descriptor steps");
        assert!(plan(&registry, &snapshot).into_reader().finish().is_err());
        let mut reader = registry.plan(snapshot.clone(), 0..snapshot.len(), flag.clone()).unwrap().into_reader();
        assert_eq!(reader.read(&mut [0; 3]).unwrap(), 3);
        flag.store(true, Ordering::Relaxed);
        assert_eq!(reader.read(&mut [0; 3]).unwrap_err().kind(), io::ErrorKind::Interrupted);
        flag.store(false, Ordering::Relaxed);
        assert!(reader.finish().is_err(), "reader errors must remain sticky");
        let mut reader = plan(&registry, &snapshot).into_reader();
        reader.plan.physical_len += 1;
        assert!(reader.read_to_end(&mut Vec::new()).is_err());
        assert!(reader.finish().is_err());
        assert_eq!(registry.origin_count(), 0);
        assert_eq!(registry.epoch(), 0);
    }

    #[test]
    fn capsule_reader_bounds_raw_payloads_and_detached_scratch() {
        let fixture = Fixture::new();
        let source = fixture.decoded(&vec![0xff; 70_000], Encoding::UTF8, 0);
        let registry = registry();
        let p = plan(&registry, &source.snapshot());
        assert_eq!(p.definitions.len(), 1);
        assert_eq!(p.definitions.capacity(), 1, "no descriptor per piece or character");
        let (bytes, token) = serialize(&registry, p, 2 * MAX_READ_BYTES);
        assert!(
            matches!(&inspect(&bytes, &[]).1[0], Record::Definition { raw, .. } if raw.len() == 70_000 && raw.iter().all(|&b| b == 0xff))
        );
        drop(token);
        let mut detached = Buffer::empty();
        detached.insert_snapshot_range(0, &source.snapshot(), 0..source.len()).unwrap();
        let mut reader = plan(&registry, &detached.snapshot()).into_reader();
        let mut buffer = vec![0; 2 * MAX_READ_BYTES];
        let mut largest = 0;
        while reader.read(&mut buffer).unwrap() != 0 {
            if let Payload::Opaque { raw, table, .. } = &reader.payload {
                assert!(raw.len() <= MAX_READ_BYTES && table.len() <= MAX_READ_BYTES);
                largest = largest.max(raw.len() + table.len());
            }
            assert!(reader.plan.definitions.is_empty());
            assert!(reader.pending.len() < 128);
        }
        assert!(largest > 0 && largest <= 2 * MAX_READ_BYTES);
        drop(reader.finish().unwrap());
    }

    #[test]
    fn capsule_explicit_empty_seed_embeds_bom_or_empty_origin_and_deduplicates_after_admission() {
        let fixture = Fixture::new();
        for (raw, encoding, bom) in [(&b"\xff\xfe"[..], Encoding::UTF16_LE, 2), (&b""[..], Encoding::UTF8, 0)] {
            let source = fixture.decoded(raw, encoding, bom);
            assert_eq!(source.len(), 0);
            let origin = fixture.owners.borrow().last().unwrap().snapshot();
            let mut registry = registry();
            let p = registry.plan_seed(source.snapshot(), origin.clone(), cancel()).unwrap();
            assert_eq!(p.logical_len(), 0);
            assert_eq!(p.new_origin_count(), 1);
            let mut partial = registry.plan_seed(source.snapshot(), origin.clone(), cancel()).unwrap().into_reader();
            assert_eq!(partial.read(&mut [0; 16]).unwrap(), 16);
            assert!(partial.finish().is_err());
            assert_eq!(registry.origin_count(), 0);
            let (bytes, token) = serialize(&registry, p, 1);
            assert_eq!(
                inspect(&bytes, &[]),
                (
                    0,
                    vec![
                        Record::Definition {
                            id: 1,
                            encoding: encoding.name().into(),
                            bom: bom as u8,
                            raw: raw.to_vec(),
                        },
                        Record::Reference { id: 1, range: 0..0 }
                    ]
                )
            );
            registry.admit(token).unwrap();
            let p = registry.plan_seed(source.snapshot(), origin, cancel()).unwrap();
            assert_eq!(p.new_origin_count(), 0);
            assert_eq!(p.physical_len(), HEADER_LEN + REFERENCE_LEN + 1);
            let (bytes, token) = serialize(&registry, p, 3);
            assert_eq!(inspect(&bytes, &[(1, 0)]), (0, vec![Record::Reference { id: 1, range: 0..0 }]));
            registry.admit(token).unwrap();
            let p = plan(&registry, &source.snapshot());
            assert_eq!(p.physical_len(), HEADER_LEN + 1);
            let (bytes, token) = serialize(&registry, p, 3);
            assert_eq!(inspect(&bytes, &[]), (0, vec![]));
            drop(token);
        }
    }

    #[test]
    fn capsule_explicit_seed_rejects_mismatched_origin_length_identity_and_order() {
        let fixture = Fixture::new();
        let source = fixture.decoded(b"AB", Encoding::UTF8, 0);
        let origin = fixture.owners.borrow().last().unwrap().snapshot();
        let foreign = fixture.decoded(b"XY", Encoding::UTF8, 0);
        let registry = registry();
        let snapshot = source.snapshot();
        assert!(registry.plan_seed(snapshot.slice(0..1).unwrap(), origin.clone(), cancel()).is_err());
        assert!(registry.plan_seed(foreign.snapshot(), origin.clone(), cancel()).is_err());
        let reversed = snapshot.slice(1..2).unwrap().concat(&snapshot.slice(0..1).unwrap()).unwrap();
        assert!(registry.plan_seed(reversed, origin.clone(), cancel()).is_err());
        let mut added = Buffer::empty();
        added.insert(0, b"AB").unwrap();
        assert!(registry.plan_seed(added.snapshot(), origin.clone(), cancel()).is_err());
        let (bytes, token) = serialize(&registry, registry.plan_seed(snapshot, origin, cancel()).unwrap(), 3);
        assert_eq!(inspect(&bytes, &[]).0, 2);
        drop(token);
        assert_eq!(registry.origin_count(), 0);
        assert_eq!(registry.epoch(), 0);
    }

    fn batch_bytes(plan: OriginBatchPlan<'_, '_>) -> Vec<u8> {
        let expected = plan.physical_len();
        let mut reader = plan.start().unwrap();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes.len() as u64, expected);
        reader.admit().unwrap(); // in-memory fixture admission; not durability
        bytes
    }

    #[test]
    fn capsule_batch_distinct_definitions_and_staged_refs_round_trip_real_journal() {
        use super::journal::{JournalRecovery, JournalReplayLimits, RecoveryPolicy};
        use super::replay::ReplayLimits;
        use kwil_core_text::{
            RecoveryLimits,
            encoding::{self, LineEnding, SavePlan},
        };
        use kwil_core_workspace::{
            journal::{JournalBase, JournalBatch, JournalCommand, JournalReader, JournalReplacement, JournalWorker},
            session::DocumentId,
        };
        use std::{io::Cursor, time::Duration};
        let fixture = Fixture::new();
        let old = fixture.decoded(b"\xff\xfeO\0", Encoding::UTF16_LE, 2);
        let a = fixture.decoded(b"A\0", Encoding::UTF16_LE, 0);
        let b = fixture.decoded(b"B\0", Encoding::UTF16_LE, 0);
        let mut registry = registry();
        let p = plan(&registry, &old.snapshot());
        let (base_bytes, base_token) = serialize(&registry, p, 3);
        let policy = RecoveryPolicy {
            encoding: Encoding::UTF16_LE,
            bom: true,
            preferred_ending: LineEnding::CrLf,
            compression: kwil_core_workspace::compressed::CompressionFormat::Raw,
            raw_bytes: false,
        };
        let path = fixture.directory.join("staged-journal");
        let metadata = JournalBase {
            document: DocumentId(41),
            revision: 0,
            original_path: None,
            original_identity: [0; 4],
            codec: policy.to_codec().unwrap(),
            byte_len: base_bytes.len() as u64,
        };
        let wait = Duration::from_secs(5);
        let mut worker = JournalWorker::create_encoded_from_reader(
            path.clone(),
            metadata.with_logical_len(1).unwrap(),
            Box::new(Cursor::new(base_bytes)),
            Duration::from_secs(60),
        )
        .unwrap();
        worker.ready.recv_timeout(wait).unwrap().unwrap();
        registry.admit(base_token).unwrap();
        let before = registry.baseline.clone();
        let flag = cancel();
        let mut batch = registry.begin_batch().unwrap();
        worker
            .try_submit(JournalCommand::BeginBatch(JournalBatch { from_revision: 0, to_revision: 1, replacements: 3 }))
            .unwrap();
        for (snapshot, id, offset, removed, definitions) in
            [(a.snapshot(), 2, 0, 1, 1), (b.snapshot(), 3, 1, 0, 1), (a.snapshot(), 2, 2, 0, 0)]
        {
            let p = batch.plan(snapshot, 0..1, flag.clone()).unwrap();
            assert_eq!(
                p.new_origin_count(),
                definitions,
                "later capsule must reuse earlier staged IDs without raw rewrite"
            );
            if definitions != 0 {
                assert_eq!(
                    p.plan.as_ref().unwrap().definitions[0].id,
                    id,
                    "staged origin IDs must remain collision-free"
                );
            } else {
                assert_eq!(p.physical_len(), HEADER_LEN + REFERENCE_LEN + 1);
            }
            let physical = p.physical_len();
            let mut reader = p.start().unwrap();
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes.len() as u64, physical);
            let known = [(1, 1), (2, 1), (3, 1)];
            let records = if definitions == 0 {
                inspect(&bytes, &known).1
            } else {
                inspect(&bytes, &known[..(id - 1) as usize]).1
            };
            assert_eq!(records.last(), Some(&Record::Reference { id, range: 0..1 }));
            worker
                .try_submit(JournalCommand::BeginEncodedReplacement {
                    replacement: JournalReplacement { offset, removed, inserted: 1 },
                    payload_len: physical,
                })
                .unwrap();
            worker.try_submit(JournalCommand::Bytes(bytes)).unwrap();
            worker.try_submit(JournalCommand::EndReplacement).unwrap();
            reader.admit().unwrap();
            assert!(Arc::ptr_eq(&batch.live.baseline, &before), "staged admission must not publish the live directory");
            assert_eq!(batch.live.origin_count(), 1);
        }
        assert_eq!(batch.origin_count(), 3);
        worker.try_submit(JournalCommand::CommitBatch).unwrap();
        batch.commit().unwrap(); // only AFTER actual outer queue acceptance
        assert_eq!(registry.origin_count(), 3);
        assert!(!Arc::ptr_eq(&registry.baseline, &before));
        worker.finish();
        assert_eq!(worker.finished.recv_timeout(wait).unwrap().unwrap(), 1);
        let private = fixture.directory.join("recovered");
        std::fs::create_dir(&private).unwrap();
        let limits = JournalReplayLimits {
            replay: ReplayLimits {
                core: RecoveryLimits {
                    add_memory_bytes: 4096,
                    undo_memory_bytes: 1 << 20,
                    max_origins: 8,
                    max_records: 1024,
                    max_pieces: 4096,
                    max_blocks: 512,
                    max_metadata_bytes: 16 << 20,
                },
                decode: decode::DecodeLimits {
                    max_physical_bytes: 128 << 10,
                    max_logical_bytes: 128 << 10,
                    max_records: 128,
                },
                max_origin_storage_bytes: 4 << 20,
                max_private_path_bytes: 1024,
            },
            max_payload_bytes: 128 << 10,
            max_base_metadata_bytes: 4096,
            max_document_bytes: 128 << 10,
            max_batches: 8,
            max_replacements: 16,
        };
        let mut recovered = JournalRecovery::new(private.clone(), limits).unwrap();
        let mut journal = JournalReader::open(&path).unwrap();
        let summary = recovered.recover(&mut journal, &flag).unwrap();
        assert_eq!(summary.journal.byte_len, 3);
        assert_eq!(summary.journal.revision, 1);
        let buffer = recovered.take_buffer(&flag).unwrap();
        let mut raw = Vec::new();
        encoding::write_snapshot(
            &buffer.snapshot(),
            SavePlan { encoding: Encoding::UTF16_LE, bom: true, line_ending: None },
            &mut raw,
        )
        .unwrap();
        assert_eq!(raw, b"\xff\xfeA\0B\0A\0");
        assert_eq!(recovered.accepted_origin_count(), 3);
        drop(buffer);
        recovered.abandon().unwrap();
        assert_eq!(std::fs::read_dir(&private).unwrap().count(), 0);
        std::fs::remove_dir(private).unwrap();
        drop((journal, worker));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn capsule_batch_drop_abort_errors_and_cancellation_preserve_exact_live_state() {
        let fixture = Fixture::new();
        let old = fixture.decoded(b"A", Encoding::UTF8, 0);
        let new = fixture.decoded(b"B", Encoding::UTF8, 0);
        let mut registry = registry();
        let (_, token) = serialize(&registry, plan(&registry, &old.snapshot()), 3);
        registry.admit(token).unwrap();
        for mode in [
            "drop",
            "abort",
            "plan",
            "plan_drop",
            "reader_drop",
            "incomplete",
            "start_cancel",
            "read_cancel",
            "admit_cancel",
            "commit_cancel",
            "flag_changed",
        ] {
            let before = registry.baseline.clone();
            let old_epoch = registry.epoch();
            let old_next = registry.baseline.next_id;
            let prior_plan = plan(&registry, &old.snapshot());
            let flag = cancel();
            let mut batch = registry.begin_batch().unwrap();
            batch_bytes(batch.plan(new.snapshot(), 0..1, flag.clone()).unwrap());
            assert_eq!(batch.origin_count(), 2);
            assert!(Arc::ptr_eq(&batch.live.baseline, &before), "staging must preserve live baseline before {mode}");
            match mode {
                "drop" => drop(batch),
                "abort" => batch.abort(),
                _ => {
                    match mode {
                        "plan" => assert!(batch.plan(new.snapshot(), 0..2, flag.clone()).is_err()),
                        "plan_drop" => drop(batch.plan(new.snapshot(), 0..1, flag.clone()).unwrap()),
                        "reader_drop" => drop(batch.plan(new.snapshot(), 0..1, flag.clone()).unwrap().start().unwrap()),
                        "incomplete" => {
                            let mut reader = batch.plan(new.snapshot(), 0..1, flag.clone()).unwrap().start().unwrap();
                            assert_eq!(reader.read(&mut [0; 3]).unwrap(), 3);
                            assert!(reader.admit().is_err());
                        }
                        "start_cancel" => {
                            let p = batch.plan(new.snapshot(), 0..1, flag.clone()).unwrap();
                            flag.store(true, Ordering::Relaxed);
                            assert!(p.start().is_err());
                            flag.store(false, Ordering::Relaxed);
                        }
                        "read_cancel" => {
                            let mut r = batch.plan(new.snapshot(), 0..1, flag.clone()).unwrap().start().unwrap();
                            flag.store(true, Ordering::Relaxed);
                            assert!(r.read(&mut [0; 3]).is_err());
                            flag.store(false, Ordering::Relaxed);
                            drop(r);
                        }
                        "admit_cancel" => {
                            let mut r = batch.plan(new.snapshot(), 0..1, flag.clone()).unwrap().start().unwrap();
                            r.read_to_end(&mut Vec::new()).unwrap();
                            flag.store(true, Ordering::Relaxed);
                            assert!(r.admit().is_err());
                            flag.store(false, Ordering::Relaxed);
                        }
                        "commit_cancel" => flag.store(true, Ordering::Relaxed),
                        "flag_changed" => assert!(batch.plan(new.snapshot(), 0..1, cancel()).is_err()),
                        _ => unreachable!(),
                    }
                    assert!(batch.commit().is_err(), "poisoned/cancelled batch must refuse partial commit: {mode}");
                }
            }
            assert!(Arc::ptr_eq(&registry.baseline, &before), "abort must preserve EXACT baseline: {mode}");
            assert_eq!(registry.origin_count(), 1);
            assert_eq!(registry.epoch(), old_epoch);
            assert_eq!(registry.baseline.next_id, old_next);
            let mut reader =
                registry.start(prior_plan).expect("pre-existing plan must remain usable after batch abort");
            reader.read_to_end(&mut Vec::new()).unwrap();
            drop(reader.finish().unwrap());
            assert_eq!(
                plan(&registry, &new.snapshot()).definitions[0].id,
                old_next,
                "failed batch must not consume live ID sequence"
            );
        }
    }

    #[test]
    fn capsule_batch_success_stales_preexisting_plans_including_empty_batch() {
        let fixture = Fixture::new();
        let source = fixture.decoded(b"A", Encoding::UTF8, 0);
        let mut registry = registry();
        let old = plan(&registry, &source.snapshot());
        let flag = cancel();
        let mut batch = registry.begin_batch().unwrap();
        batch_bytes(batch.plan(source.snapshot(), 0..1, flag).unwrap());
        batch.commit().unwrap();
        assert!(registry.start(old).is_err(), "successful batch must stale old standalone plans");
        let old = plan(&registry, &source.snapshot());
        let before = registry.baseline.clone();
        let epoch = registry.epoch();
        registry.begin_batch().unwrap().commit().unwrap();
        assert!(!Arc::ptr_eq(&registry.baseline, &before));
        assert!(registry.epoch() > epoch);
        assert!(registry.start(old).is_err());
        assert_eq!(registry.origin_count(), 1);
    }

    #[test]
    fn capsule_batch_exact_metadata_peak_accounts_retained_actual_capacity_before_growth() {
        let fixture = Fixture::new();
        let a = fixture.decoded(b"A", Encoding::UTF8, 0);
        let b = fixture.decoded(b"B", Encoding::UTF8, 0);
        let mut registry = OriginRegistry::new(2, 8192);
        let (_, token) = serialize(&registry, plan(&registry, &a.snapshot()), 3);
        registry.admit(token).unwrap();
        reserve(&mut Arc::get_mut(&mut registry.baseline).unwrap().origins, 4).unwrap();
        let actual = registry.baseline.origins.capacity();
        assert!(actual > registry.max_origins, "witness requires committed spare capacity");
        let required = batch_retained_cost(actual).unwrap() + metadata_cost(2).unwrap();
        let before = registry.baseline.clone();
        let pins = Arc::strong_count(&registry.baseline);
        registry.max_metadata_bytes = required - 1;
        assert!(
            registry.begin_batch().is_err(),
            "missing retained committed capacity accounting must refuse BEFORE shadow allocation"
        );
        assert_eq!(Arc::strong_count(&registry.baseline), pins);
        assert!(Arc::ptr_eq(&registry.baseline, &before));
        assert_eq!(registry.baseline.origins.capacity(), actual);
        registry.max_metadata_bytes = required;
        let flag = cancel();
        let mut batch = registry.begin_batch().unwrap();
        assert_eq!(batch.metadata_reserved_bytes(), required);
        batch_bytes(batch.plan(b.snapshot(), 0..1, flag.clone()).unwrap());
        batch_bytes(batch.plan(a.snapshot(), 0..1, flag).unwrap());
        batch.commit().unwrap();
        assert_eq!(registry.origin_count(), 2);
        assert!(batch_retained_cost(usize::MAX).is_err());
    }

    #[test]
    fn capsule_batch_empty_seed_dedup_and_definition_metadata_do_not_retain_payloads() {
        let fixture = Fixture::new();
        let empty = fixture.decoded(b"\xff\xfe", Encoding::UTF16_LE, 2);
        let empty_origin = fixture.owners.borrow().last().unwrap().snapshot();
        let large = fixture.decoded(&vec![b'A'; 96 << 10], Encoding::UTF8, 0);
        let mut registry = registry();
        let flag = cancel();
        let mut batch = registry.begin_batch().unwrap();
        let p = batch.plan_seed(empty.snapshot(), empty_origin.clone(), flag.clone()).unwrap();
        assert_eq!(p.new_origin_count(), 1);
        batch_bytes(p);
        let p = batch.plan_seed(empty.snapshot(), empty_origin, flag.clone()).unwrap();
        assert_eq!(p.new_origin_count(), 0);
        assert_eq!(p.physical_len(), HEADER_LEN + REFERENCE_LEN + 1);
        let bytes = batch_bytes(p);
        assert_eq!(inspect(&bytes, &[(1, 0)]), (0, vec![Record::Reference { id: 1, range: 0..0 }]));
        let p = batch.plan(large.snapshot(), 0..large.len(), flag.clone()).unwrap();
        assert_eq!(p.plan.as_ref().unwrap().definitions.len(), 1);
        let physical = p.physical_len();
        let mut reader = p.start().unwrap();
        let mut scratch = [0; MAX_READ_BYTES];
        let mut total = 0;
        loop {
            let n = reader.read(&mut scratch).unwrap();
            if n == 0 {
                break;
            }
            total += n as u64;
            let inner = reader.reader.as_ref().unwrap();
            assert!(inner.pending.len() < 128);
            assert!(matches!(inner.payload, Payload::None | Payload::Definition { .. }));
        }
        assert_eq!(total, physical);
        reader.admit().unwrap();
        assert!(
            batch.metadata_reserved_bytes() < large.len() as usize,
            "registry stores bounded handles, never raw payload copies"
        );
        let p = batch.plan(large.snapshot(), 0..1, flag).unwrap();
        assert_eq!(p.new_origin_count(), 0);
        assert_eq!(p.physical_len(), HEADER_LEN + REFERENCE_LEN + 1);
        batch_bytes(p);
        batch.commit().unwrap();
        assert_eq!(registry.origin_count(), 2);
    }

    #[test]
    fn capsule_batch_reader_io_failure_poison_and_dropped_batch_release_new_origin_pins() {
        let fixture = Fixture::new();
        let source = fixture.decoded(b"A", Encoding::UTF8, 0);
        let path = fixture.directory.join("mutable-original");
        std::fs::write(&path, b"original").unwrap();
        let original = Buffer::from_file(TextFile::open(&path).unwrap()).unwrap();
        let mut registry = registry();
        let before = registry.baseline.clone();
        let flag = cancel();
        let mut batch = registry.begin_batch().unwrap();
        batch_bytes(batch.plan(source.snapshot(), 0..1, flag.clone()).unwrap());
        let mut reader = batch.plan(original.snapshot(), 0..original.len(), flag).unwrap().start().unwrap();
        std::fs::write(&path, b"modified and longer").unwrap();
        assert!(reader.read_to_end(&mut Vec::new()).is_err());
        assert!(reader.admit().is_err());
        assert!(batch.is_poisoned());
        assert!(batch.commit().is_err());
        assert!(Arc::ptr_eq(&registry.baseline, &before));
        assert_eq!(registry.origin_count(), 0);
        drop(source);
        let owner = fixture.owners.borrow_mut().pop().unwrap();
        assert!(owner.try_retire().is_ok(), "aborted staged directory must release new origin pins on the worker");
    }

    #[test]
    fn capsule_batch_forgotten_scoped_plan_cannot_commit_an_unfinished_capsule() {
        let mut registry = registry();
        let before = registry.baseline.clone();
        let mut batch = registry.begin_batch().unwrap();
        // Safe code may deliberately forget an RAII wrapper. The separate
        // pending flag must still refuse publication. Empty fixture pins no
        // origin, payload or file; only bounded metadata is leaked to process exit.
        std::mem::forget(batch.plan(Snapshot::default(), 0..0, cancel()).unwrap());
        assert!(batch.commit().is_err(), "in-flight marker must survive a forgotten wrapper");
        assert!(Arc::ptr_eq(&registry.baseline, &before));
    }

    #[test]
    fn capsule_batch_recovered_sparse_ids_reuse_origins_and_allocate_above_exact_maximum() {
        let fixture = Fixture::new();
        let a = fixture.decoded(b"A\0", Encoding::UTF16_LE, 0);
        let a_origin = fixture.owners.borrow().last().unwrap().snapshot();
        let b = fixture.decoded(b"B\0", Encoding::UTF16_LE, 0);
        let b_origin = fixture.owners.borrow().last().unwrap().snapshot();
        let c = fixture.decoded(b"C\0", Encoding::UTF16_LE, 0);
        let flag = cancel();
        let mut registry = OriginRegistry::from_recovered(8, 8192, [(23, b_origin), (7, a_origin)], &flag).unwrap();
        assert_eq!(registry.origin_count(), 2);
        assert_eq!(registry.baseline.next_id, 24);
        let p = plan(&registry, &a.snapshot());
        assert_eq!(p.new_origin_count(), 0, "adoption must not recopy recovered raw origins");
        let (bytes, token) = serialize(&registry, p, 3);
        assert_eq!(inspect(&bytes, &[(7, 1), (23, 1)]), (1, vec![Record::Reference { id: 7, range: 0..1 }]));
        registry.admit(token).unwrap();
        let mut batch = registry.begin_batch().unwrap();
        let p = batch.plan(c.snapshot(), 0..1, flag.clone()).unwrap();
        assert_eq!(p.plan.as_ref().unwrap().definitions[0].id, 24);
        batch_bytes(p);
        let p = batch.plan(b.snapshot(), 0..1, flag).unwrap();
        assert_eq!(p.new_origin_count(), 0);
        let bytes = batch_bytes(p);
        assert_eq!(inspect(&bytes, &[(7, 1), (23, 1), (24, 1)]), (1, vec![Record::Reference { id: 23, range: 0..1 }]));
        batch.commit().unwrap();
        assert_eq!(registry.origin_count(), 3);
        assert_eq!(registry.baseline.next_id, 25);
    }

    #[test]
    fn capsule_batch_recovered_directory_rejects_invalid_ids_identity_caps_and_cancellation() {
        let fixture = Fixture::new();
        let a = fixture.decoded(b"A", Encoding::UTF8, 0);
        let a_origin = fixture.owners.borrow().last().unwrap().snapshot();
        let b = fixture.decoded(b"B", Encoding::UTF8, 0);
        let b_origin = fixture.owners.borrow().last().unwrap().snapshot();
        let flag = cancel();
        for entries in [
            vec![(0, a_origin.clone())],
            vec![(3, a_origin.clone()), (3, b_origin.clone())],
            vec![(3, a_origin.clone()), (4, a_origin.clone())],
            vec![(u64::MAX, a_origin.clone())],
        ] {
            assert!(OriginRegistry::from_recovered(8, 8192, entries, &flag).is_err());
        }
        assert!(
            OriginRegistry::from_recovered(1, 8192, [(3, a_origin.clone()), (4, b_origin.clone())], &flag).is_err()
        );
        assert!(OriginRegistry::from_recovered(8, 0, [(3, a_origin.clone())], &flag).is_err());
        flag.store(true, Ordering::Relaxed);
        assert!(OriginRegistry::from_recovered(8, 8192, [(3, a_origin.clone())], &flag).is_err());
        flag.store(false, Ordering::Relaxed);
        let entries = [(3, a_origin.clone()), (4, b_origin.clone())].into_iter().inspect(|(id, _)| {
            if *id == 4 {
                flag.store(true, Ordering::Relaxed);
            }
        });
        assert!(OriginRegistry::from_recovered(8, 8192, entries, &flag).is_err());
        drop((a, b, a_origin, b_origin));
        // Constructor failures retain no origin pins, even after a partially
        // built private directory. Source owners remain explicitly worker-owned.
        for owner in fixture.owners.borrow_mut().drain(..) {
            assert!(owner.try_retire().is_ok());
        }
    }
}
