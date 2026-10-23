//! Lossless recovery capsule serialization, worker only. This is NOT recovery
//! acceptance: a decoder, document adoption, raw projection, retention/rotation
//! and frontend integration are separate gates.
//!
//! Version 1 wire format (all integers unsigned little endian):
//! ```text
//! header: "KWCP", version:u16=1, flags:u16=0, expected_logical_len:u64
//! 1 OriginDefinition: id:u64, label_len:u16, bom_len:u8, raw_len:u64,
//!                     encoding_label:[u8;label_len], raw:[u8;raw_len]
//! 2 OriginReference:  id:u64, logical_start:u64, logical_end:u64
//! 3 LogicalBytes:     len:u64, bytes:[u8;len]
//! 4 DetachedOpaque:   label_len:u16, logical_count:u64, raw_len:u64,
//!                     table_len:u64, encoding_label, raw, LE-u32 endpoints
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

use kwil_core_text::{Snapshot, SourceChunk, decoded_backing::DecodedSnapshot, encoding::Encoding};
use std::{
    io::{self, Read},
    mem::size_of,
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

pub const CAPSULE_VERSION: u16 = 1;
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
        }
    }

    pub fn origin_count(&self) -> usize {
        self.baseline.origins.len()
    }
    pub fn epoch(&self) -> u64 {
        self.baseline.epoch
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
    /// Traversal is O(pieces * log(max_origins)), with bounded detached metadata
    /// checks. No origin raw byte scan, per-character list or per-piece Vec.
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
        self.check_capacity(total)?;
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
        if metadata_cost(slots)? > self.max_metadata_bytes {
            Err(invalid("origin metadata budget exceeded"))
        } else {
            Ok(())
        }
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
        self.check_capacity(total)?;
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
        assert_eq!(u16_(&mut bytes), 1);
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
}
