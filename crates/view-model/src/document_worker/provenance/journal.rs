//! Worker-only authenticated journal -> lossless capsule replay.
//!
//! One revision spans the ENTIRE frozen reader, including its base. Nothing is
//! published until recovery, capsule validation and payload retirement succeed.
//! A truncated outer batch is omitted by JournalReader; a bad later capsule
//! aborts all staged text and IDs. Raw journals have a separate legacy consumer.
//!
//! Keep this object on the worker after extracting a Buffer. Explicitly abandon
//! and retry retirement after all Buffer/Snapshot pins have left the frontend.
//! Dropping this wrapper never unlinks a payload or retires an origin for you.

use super::{
    payload::CapsulePayload,
    replay::{ReplayLimits, ReplayRevision, ReplayState, RetirementStatus},
};
use kwil_core_text::{
    Buffer,
    encoding::{Encoding, LineEnding},
};
use kwil_core_workspace::journal::{
    JournalBase, JournalBatch, JournalReader, JournalReplacement, RecoveryEvent, RecoverySummary,
};
use std::{
    io,
    marker::PhantomData,
    path::{Path, PathBuf},
    rc::Rc,
    sync::atomic::{AtomicBool, Ordering},
};

const POLICY_MAGIC: &[u8; 4] = b"KWJR";
const POLICY_VERSION: u8 = 1;
const MAX_ENCODING_NAME: usize = 40;

/// Explicit document output policy, independent of every embedded origin.
/// Preferred ending is editing metadata, NOT a request to normalize recovery.
/// CR is representable here; existing Document Enter support is a separate gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveryPolicy {
    pub encoding: Encoding,
    pub bom: bool,
    pub preferred_ending: LineEnding,
}
impl RecoveryPolicy {
    /// Small versioned codec tag for an explicitly EncodedJournalBase. This
    /// never changes how the backend interprets an old raw opaque codec tag.
    pub fn to_codec(self) -> io::Result<Vec<u8>> {
        self.validate()?;
        let label = self.encoding.name().as_bytes();
        if label.is_empty() || label.len() > MAX_ENCODING_NAME {
            return Err(invalid("recovery encoding name length"));
        }
        let ending = match self.preferred_ending {
            LineEnding::Lf => 0,
            LineEnding::CrLf => 1,
            LineEnding::Cr => 2,
        };
        let mut tag = Vec::with_capacity(8 + label.len());
        tag.extend_from_slice(POLICY_MAGIC);
        tag.extend_from_slice(&[POLICY_VERSION, u8::from(self.bom), ending, label.len() as u8]);
        tag.extend_from_slice(label);
        Ok(tag)
    }
    pub fn from_codec(tag: &[u8]) -> io::Result<Self> {
        if tag.len() < 9
            || tag.len() > 8 + MAX_ENCODING_NAME
            || &tag[..4] != POLICY_MAGIC
            || tag[4] != POLICY_VERSION
            || tag[5] > 1
            || tag[7] as usize != tag.len() - 8
        {
            return Err(invalid("unsupported encoded recovery codec/version"));
        }
        let preferred_ending = match tag[6] {
            0 => LineEnding::Lf,
            1 => LineEnding::CrLf,
            2 => LineEnding::Cr,
            _ => return Err(invalid("recovery ending policy")),
        };
        let label = std::str::from_utf8(&tag[8..]).map_err(|_| invalid("recovery encoding name"))?;
        let encoding = Encoding::for_label(label).map_err(|e| invalid(&e.to_string()))?;
        if encoding.name() != label {
            return Err(invalid("noncanonical recovery encoding name"));
        }
        let policy = Self { encoding, bom: tag[5] != 0, preferred_ending };
        policy.validate()?;
        Ok(policy)
    }
    fn validate(self) -> io::Result<()> {
        if self.bom && ![Encoding::UTF8, Encoding::UTF16_LE, Encoding::UTF16_BE].contains(&self.encoding) {
            return Err(invalid("BOM policy is invalid for recovery encoding"));
        }
        Ok(())
    }
}

/// All limits are explicit caller admission policy. Capsule storage is capped
/// independently from decoded-origin storage and cumulative core replay work.
pub struct JournalReplayLimits {
    pub replay: ReplayLimits,
    pub max_payload_bytes: u64,
    pub max_base_metadata_bytes: usize,
    pub max_document_bytes: u64,
    pub max_batches: u64,
    pub max_replacements: u64,
}
#[derive(Debug)]
pub struct JournalRecoverySummary {
    /// Actual last authenticated revision (undo may decrease it), logical byte
    /// length, committed boundary and interrupted-tail status from the reader.
    pub journal: RecoverySummary,
    /// Original path/identity are informational; never open or attach that path.
    pub base: JournalBase,
    pub policy: RecoveryPolicy,
}
#[derive(Debug, Default, PartialEq, Eq)]
pub struct JournalRetirementStatus {
    pub origins: RetirementStatus,
    pub payload_pending: bool,
}
#[derive(PartialEq, Eq)]
enum Phase {
    Fresh,
    Failed,
    Ready,
    Taken,
    Abandoned,
}

#[must_use = "retain on the worker and explicitly retire payloads and origin owners"]
pub struct JournalRecovery {
    replay: ReplayState,
    pending: Option<CapsulePayload>,
    parent: PathBuf,
    limits: ConsumerLimits,
    phase: Phase,
    summary: Option<JournalRecoverySummary>,
    // Construct on the worker; never accidentally transfer cleanup ownership
    // in a frontend completion. Buffer alone may cross that boundary.
    _worker: PhantomData<Rc<()>>,
}
struct ConsumerLimits {
    payload: u64,
    capsule_physical: u64,
    capsule_logical: u64,
    base_metadata: usize,
    document: u64,
    batches: u64,
    replacements: u64,
}
impl JournalRecovery {
    /// Parent must already be app-private. Its explicit path cap includes the
    /// generated spool suffix, even for an origin-free text-only capsule.
    pub fn new(private_parent: PathBuf, limits: JournalReplayLimits) -> io::Result<Self> {
        let cap = limits.replay.max_private_path_bytes;
        if !private_parent.is_absolute()
            || private_parent.capacity() > cap
            || private_parent.as_os_str().as_encoded_bytes().len().checked_add(64).is_none_or(|n| n > cap)
        {
            return Err(invalid("recovery private parent/path cap"));
        }
        let consumer = ConsumerLimits {
            payload: limits.max_payload_bytes,
            capsule_physical: limits.replay.decode.max_physical_bytes,
            capsule_logical: limits.replay.decode.max_logical_bytes,
            base_metadata: limits.max_base_metadata_bytes,
            document: limits.max_document_bytes,
            batches: limits.max_batches,
            replacements: limits.max_replacements,
        };
        let replay = ReplayState::new(private_parent.clone(), limits.replay)?;
        Ok(Self {
            replay,
            pending: None,
            parent: private_parent,
            limits: consumer,
            phase: Phase::Fresh,
            summary: None,
            _worker: PhantomData,
        })
    }

    /// One-shot, including failure and cancellation. Retry cleanup on this
    /// object; start a NEW wrapper to retry recovery, so the base cannot apply
    /// twice. This method never modifies, truncates or deletes the journal.
    pub fn recover(&mut self, reader: &mut JournalReader, cancel: &AtomicBool) -> io::Result<&JournalRecoverySummary> {
        self.recover_observed(reader, cancel, |_| Ok(()))
    }
    fn recover_observed(
        &mut self,
        reader: &mut JournalReader,
        cancel: &AtomicBool,
        mut observe: impl FnMut(Point) -> io::Result<()>,
    ) -> io::Result<&JournalRecoverySummary> {
        if self.phase != Phase::Fresh {
            return Err(invalid("journal recovery is one-shot; retire and create a new worker state"));
        }
        self.phase = Phase::Failed;
        let mut revision = self.replay.begin_revision(cancel)?;
        let outcome = (|| {
            let mut events = Consumer {
                revision: &mut revision,
                pending: &mut self.pending,
                parent: &self.parent,
                limits: &self.limits,
                cancel,
                observe: &mut observe,
                base: None,
                policy: None,
                base_complete: false,
                logical: 0,
                revision_id: 0,
                batch: None,
                replacement: None,
                batches: 0,
                replacements: 0,
            };
            let journal = reader.recover(cancel, |event| events.event(event))?;
            events.finish(journal)
        })();
        match outcome {
            Ok(summary) => {
                revision.commit()?;
                self.summary = Some(summary);
                self.phase = Phase::Ready;
                Ok(self.summary.as_ref().unwrap())
            }
            Err(error) => {
                // Drop all staged root/registry pins before retiring origins.
                let error = with_cleanup(error, revision.abort());
                Err(with_cleanup(error, retire_payload(&mut self.pending)))
            }
        }
    }
    pub fn summary(&self) -> Option<&JournalRecoverySummary> {
        self.summary.as_ref()
    }
    /// Buffer extraction is terminal on error (including cancelled finish).
    /// Owners remain here, never in a UI-drop cleanup guard.
    pub fn take_buffer(&mut self, cancel: &AtomicBool) -> io::Result<Buffer> {
        if self.phase != Phase::Ready {
            return Err(invalid("journal recovery has no unconsumed successful Buffer"));
        }
        self.phase = Phase::Failed;
        let buffer = self.replay.take_buffer(cancel)?;
        self.phase = Phase::Taken;
        Ok(buffer)
    }
    pub fn has_pending_payload(&self) -> bool {
        self.pending.is_some()
    }
    pub fn retained_origin_resources(&self) -> usize {
        self.replay.retained_resource_count()
    }
    pub fn accepted_origin_count(&self) -> usize {
        self.replay.accepted_origin_count()
    }

    /// Releases replay pins first; live output Buffer/Snapshot pins retain their
    /// exact owners. Failed payload and origin cleanup is always retryable.
    pub fn abandon(&mut self) -> io::Result<JournalRetirementStatus> {
        self.phase = Phase::Abandoned;
        let origins = self.replay.abandon();
        let payload = retire_payload(&mut self.pending);
        retirement_result(origins, payload, self.pending.is_some())
    }
    pub fn retry_retirement(&mut self) -> io::Result<JournalRetirementStatus> {
        let origins = self.replay.retry_retirement();
        let payload = retire_payload(&mut self.pending);
        retirement_result(origins, payload, self.pending.is_some())
    }
}
fn retirement_result(
    origins: io::Result<RetirementStatus>,
    payload: io::Result<()>,
    pending: bool,
) -> io::Result<JournalRetirementStatus> {
    match origins {
        Ok(origins) => {
            payload?;
            Ok(JournalRetirementStatus { origins, payload_pending: pending })
        }
        Err(error) => Err(with_cleanup(error, payload)),
    }
}
fn retire_payload(pending: &mut Option<CapsulePayload>) -> io::Result<()> {
    if let Some(payload) = pending {
        payload.retire()?;
    }
    *pending = None;
    Ok(())
}
fn with_cleanup(error: io::Error, cleanup: io::Result<()>) -> io::Error {
    match cleanup {
        Ok(()) => error,
        Err(cleanup) => io::Error::new(error.kind(), format!("{error}; cleanup retained for retry: {cleanup}")),
    }
}
#[derive(Clone, Copy)]
enum Point {
    PayloadBytes,
    CapsuleApplied,
}
struct Batch {
    header: JournalBatch,
    completed: u64,
}
struct Replacement {
    header: JournalReplacement,
    physical: u64,
    end: u64,
    next_len: u64,
}
struct Consumer<'a, 'r, 'state, F> {
    revision: &'r mut ReplayRevision<'state>,
    pending: &'a mut Option<CapsulePayload>,
    parent: &'a Path,
    limits: &'a ConsumerLimits,
    cancel: &'a AtomicBool,
    observe: &'a mut F,
    base: Option<JournalBase>,
    policy: Option<RecoveryPolicy>,
    base_complete: bool,
    logical: u64,
    revision_id: u64,
    batch: Option<Batch>,
    replacement: Option<Replacement>,
    batches: u64,
    replacements: u64,
}
impl<F: FnMut(Point) -> io::Result<()>> Consumer<'_, '_, '_, F> {
    fn admit_payload(&mut self, physical: u64, logical: u64) -> io::Result<()> {
        if self.pending.is_some() || physical > self.limits.capsule_physical || logical > self.limits.capsule_logical {
            return Err(invalid("recovery capsule admission/order"));
        }
        *self.pending = Some(CapsulePayload::new(self.parent, physical, self.limits.payload)?);
        Ok(())
    }
    fn apply(&mut self, physical: u64, logical: u64, range: std::ops::Range<u64>) -> io::Result<()> {
        let payload = self.pending.as_mut().ok_or_else(|| invalid("missing recovery capsule"))?;
        self.revision.apply_capsule(&mut payload.reader(self.cancel)?, physical, logical, range)?;
        (self.observe)(Point::CapsuleApplied)?;
        retire_payload(self.pending)
    }
    fn event(&mut self, event: RecoveryEvent<'_>) -> io::Result<()> {
        poll(self.cancel)?;
        match event {
            RecoveryEvent::EncodedBase { base, logical_len } => {
                if self.base.is_some() || self.base_complete || logical_len > self.limits.document {
                    return Err(invalid("recovery base order/length"));
                }
                let metadata = std::mem::size_of::<JournalBase>()
                    .checked_add(base.codec.len())
                    .and_then(|n| {
                        n.checked_add(base.original_path.as_ref().map_or(0, |p| p.as_os_str().as_encoded_bytes().len()))
                    })
                    .ok_or_else(|| invalid("recovery base metadata overflow"))?;
                if metadata > self.limits.base_metadata {
                    return Err(invalid("recovery base metadata cap"));
                }
                let policy = RecoveryPolicy::from_codec(&base.codec)?;
                self.admit_payload(base.byte_len, logical_len)?;
                self.base = Some(base.clone());
                self.policy = Some(policy);
                self.logical = logical_len;
                self.revision_id = base.revision;
            }
            RecoveryEvent::BaseBytes(bytes) => {
                if self.base.is_none() || self.base_complete {
                    return Err(invalid("recovery base bytes order"));
                }
                self.push(bytes)?;
            }
            RecoveryEvent::BaseComplete => {
                if self.base_complete {
                    return Err(invalid("duplicate recovery base"));
                }
                let physical = self.base.as_ref().ok_or_else(|| invalid("recovery missing base"))?.byte_len;
                self.apply(physical, self.logical, 0..0)?;
                self.base_complete = true;
            }
            RecoveryEvent::BeginBatch(header) => {
                if !self.base_complete
                    || self.batch.is_some()
                    || self.pending.is_some()
                    || header.from_revision != self.revision_id
                    || header.from_revision == header.to_revision
                    || header.replacements == 0
                {
                    return Err(invalid("recovery batch sequence"));
                }
                self.batches = self
                    .batches
                    .checked_add(1)
                    .filter(|n| *n <= self.limits.batches)
                    .ok_or_else(|| invalid("recovery batch cap"))?;
                self.replacements = self
                    .replacements
                    .checked_add(header.replacements)
                    .filter(|n| *n <= self.limits.replacements)
                    .ok_or_else(|| invalid("recovery replacement cap"))?;
                self.batch = Some(Batch { header, completed: 0 });
            }
            RecoveryEvent::BeginEncodedReplacement { replacement: h, payload_len } => {
                let batch = self.batch.as_ref().ok_or_else(|| invalid("recovery replacement outside batch"))?;
                if self.replacement.is_some() || batch.completed >= batch.header.replacements {
                    return Err(invalid("recovery replacement sequence"));
                }
                let end = h
                    .offset
                    .checked_add(h.removed)
                    .filter(|n| *n <= self.logical)
                    .ok_or_else(|| invalid("recovery replacement range"))?;
                let next_len = self
                    .logical
                    .checked_sub(h.removed)
                    .and_then(|n| n.checked_add(h.inserted))
                    .filter(|n| *n <= self.limits.document)
                    .ok_or_else(|| invalid("recovery document length cap"))?;
                self.admit_payload(payload_len, h.inserted)?;
                self.replacement = Some(Replacement { header: h, physical: payload_len, end, next_len });
            }
            RecoveryEvent::EditBytes(bytes) => {
                if self.replacement.is_none() {
                    return Err(invalid("recovery edit bytes order"));
                }
                self.push(bytes)?;
            }
            RecoveryEvent::EndReplacement => {
                let replacement = self.replacement.take().ok_or_else(|| invalid("recovery replacement end order"))?;
                self.apply(
                    replacement.physical,
                    replacement.header.inserted,
                    replacement.header.offset..replacement.end,
                )?;
                self.logical = replacement.next_len;
                self.batch.as_mut().ok_or_else(|| invalid("recovery missing batch"))?.completed += 1;
            }
            RecoveryEvent::CommitBatch => {
                let batch = self.batch.take().ok_or_else(|| invalid("recovery batch end order"))?;
                if self.replacement.is_some() || self.pending.is_some() || batch.completed != batch.header.replacements
                {
                    return Err(invalid("recovery incomplete batch"));
                }
                self.revision_id = batch.header.to_revision;
            }
            RecoveryEvent::Base(_)
            | RecoveryEvent::BeginEdit(_)
            | RecoveryEvent::CommitEdit
            | RecoveryEvent::BeginReplacement(_) => {
                return Err(invalid("raw/mixed journal frames require the legacy recovery consumer"));
            }
        }
        Ok(())
    }
    fn push(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.pending.as_mut().ok_or_else(|| invalid("recovery bytes without payload"))?.push(bytes, self.cancel)?;
        (self.observe)(Point::PayloadBytes)
    }
    fn finish(self, journal: RecoverySummary) -> io::Result<JournalRecoverySummary> {
        if !self.base_complete
            || self.batch.is_some()
            || self.replacement.is_some()
            || self.pending.is_some()
            || journal.revision != self.revision_id
            || journal.byte_len != self.logical
        {
            return Err(invalid("recovery final revision/length/sequence mismatch"));
        }
        Ok(JournalRecoverySummary {
            journal,
            base: self.base.ok_or_else(|| invalid("recovery missing base metadata"))?,
            policy: self.policy.ok_or_else(|| invalid("recovery missing policy"))?,
        })
    }
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn poll(cancel: &AtomicBool) -> io::Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(io::Error::new(io::ErrorKind::Interrupted, "journal capsule recovery cancelled"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document_worker::provenance::{CapsulePlan, OriginRegistry, decode::DecodeLimits};
    use kwil_core_text::{
        RecoveryLimits, Snapshot, SourceChunk, TextFile,
        decoded_backing::{DecodedBacking, DecodedSnapshot, Interpretation},
        encoding::{self, SavePlan},
    };
    use kwil_core_workspace::{
        journal::{JOURNAL_CHUNK, JournalCommand, JournalWorker, RecoveryTail},
        session::DocumentId,
    };
    use std::{
        cell::RefCell,
        fs,
        io::{Cursor, Read, Write},
        sync::{Arc, atomic::AtomicU64, mpsc::TrySendError},
        time::{Duration, Instant},
    };

    const WAIT: Duration = Duration::from_secs(5);
    struct Fixture {
        root: PathBuf,
        source: PathBuf,
        private: PathBuf,
        owners: RefCell<Vec<DecodedBacking>>,
    }
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "kwil-journal-replay-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            let source = root.join("source");
            let private = root.join("private");
            fs::create_dir(&source).unwrap();
            fs::create_dir(&private).unwrap();
            Self { root, source, private, owners: RefCell::new(Vec::new()) }
        }
        fn state(&self) -> JournalRecovery {
            JournalRecovery::new(self.private.clone(), limits()).unwrap()
        }
        fn reader(&self) -> JournalReader {
            JournalReader::open(&self.root.join("journal")).unwrap()
        }
        fn original(&self, raw: &[u8], encoding: Encoding, bom: usize) -> DecodedSnapshot {
            let path = self.root.join("original");
            fs::write(&path, raw).unwrap();
            let file = TextFile::open(&path).unwrap();
            let owner = DecodedBacking::prepare(
                &file,
                Interpretation::new(encoding, bom).unwrap(),
                &self.source,
                &AtomicBool::new(false),
            )
            .unwrap();
            drop(file);
            fs::remove_file(&path).unwrap();
            let origin = owner.snapshot();
            self.owners.borrow_mut().push(owner);
            origin
        }
        fn count(&self) -> usize {
            fs::read_dir(&self.private).unwrap().count()
        }
        fn write(&self, base: &Capsule, policy: RecoveryPolicy, batches: &[EditBatch]) {
            self.write_codec(base, policy.to_codec().unwrap(), true, batches);
        }
        fn write_codec(&self, base: &Capsule, codec: Vec<u8>, encoded: bool, batches: &[EditBatch]) {
            let metadata = JournalBase {
                document: DocumentId(17),
                revision: 9,
                original_path: Some(self.root.join("original")),
                original_identity: [1, 2, 3, 4],
                codec,
                byte_len: base.bytes.len() as u64,
            };
            let path = self.root.join("journal");
            let mut worker = if encoded {
                JournalWorker::create_encoded_from_reader(
                    path,
                    metadata.with_logical_len(base.logical).unwrap(),
                    Box::new(Cursor::new(base.bytes.clone())),
                    Duration::from_secs(60),
                )
                .unwrap()
            } else {
                JournalWorker::create_from_reader(
                    path,
                    metadata,
                    Box::new(Cursor::new(base.bytes.clone())),
                    Duration::from_secs(60),
                )
                .unwrap()
            };
            assert_eq!(worker.ready.recv_timeout(WAIT).unwrap().unwrap(), 9);
            for batch in batches {
                submit(
                    &worker,
                    JournalCommand::BeginBatch(JournalBatch {
                        from_revision: batch.from,
                        to_revision: batch.to,
                        replacements: batch.parts.len() as u64,
                    }),
                );
                for part in &batch.parts {
                    let replacement = JournalReplacement {
                        offset: part.offset,
                        removed: part.removed,
                        inserted: part.capsule.logical,
                    };
                    submit(
                        &worker,
                        if part.raw {
                            JournalCommand::BeginReplacement(replacement)
                        } else {
                            JournalCommand::BeginEncodedReplacement {
                                replacement,
                                payload_len: part.capsule.bytes.len() as u64,
                            }
                        },
                    );
                    for bytes in part.capsule.bytes.chunks(JOURNAL_CHUNK) {
                        submit(&worker, JournalCommand::Bytes(bytes.to_vec()));
                    }
                    submit(&worker, JournalCommand::EndReplacement);
                }
                submit(&worker, JournalCommand::CommitBatch);
            }
            worker.finish();
            assert_eq!(worker.finished.recv_timeout(WAIT).unwrap().unwrap(), batches.last().map_or(9, |b| b.to));
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            for owner in self.owners.get_mut().drain(..) {
                assert!(owner.try_retire().is_ok(), "release all source pins before fixture retirement");
            }
            // Only this test's unique known fixture, never an application path.
            fs::remove_dir_all(&self.root).unwrap();
        }
    }
    fn limits() -> JournalReplayLimits {
        JournalReplayLimits {
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
                decode: DecodeLimits { max_physical_bytes: 128 << 10, max_logical_bytes: 128 << 10, max_records: 128 },
                max_origin_storage_bytes: 4 << 20,
                max_private_path_bytes: 1024,
            },
            max_payload_bytes: 128 << 10,
            max_base_metadata_bytes: 4096,
            max_document_bytes: 256 << 10,
            max_batches: 16,
            max_replacements: 64,
        }
    }
    fn policy() -> RecoveryPolicy {
        RecoveryPolicy { encoding: Encoding::UTF8, bom: false, preferred_ending: LineEnding::CrLf }
    }
    fn registry() -> OriginRegistry {
        OriginRegistry::new(8, 8192)
    }
    #[derive(Clone)]
    struct Capsule {
        bytes: Vec<u8>,
        logical: u64,
    }
    struct Part {
        offset: u64,
        removed: u64,
        capsule: Capsule,
        raw: bool,
    }
    struct EditBatch {
        from: u64,
        to: u64,
        parts: Vec<Part>,
    }
    fn part(offset: u64, removed: u64, capsule: Capsule) -> Part {
        Part { offset, removed, capsule, raw: false }
    }
    fn pack(registry: &mut OriginRegistry, plan: CapsulePlan) -> Capsule {
        let logical = plan.logical_len();
        let physical = plan.physical_len();
        let mut reader = registry.start(plan).unwrap();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes.len() as u64, physical);
        assert!(bytes.len() <= 128 << 10, "only small test fixtures");
        registry.admit(reader.finish().unwrap()).unwrap();
        Capsule { bytes, logical }
    }
    fn capsule(registry: &mut OriginRegistry, snapshot: Snapshot) -> Capsule {
        let len = snapshot.len();
        let plan = registry.plan(snapshot, 0..len, Arc::new(AtomicBool::new(false))).unwrap();
        pack(registry, plan)
    }
    fn seed(registry: &mut OriginRegistry, origin: DecodedSnapshot) -> Capsule {
        let snapshot = Buffer::from_decoded(origin.clone()).snapshot();
        let plan = registry.plan_seed(snapshot, origin, Arc::new(AtomicBool::new(false))).unwrap();
        pack(registry, plan)
    }
    fn literal(bytes: &[u8]) -> Snapshot {
        let mut buffer = Buffer::empty();
        buffer.insert(0, bytes).unwrap();
        buffer.snapshot()
    }
    fn text(registry: &mut OriginRegistry, bytes: &[u8]) -> Capsule {
        capsule(registry, literal(bytes))
    }
    fn logical(buffer: &Buffer) -> Vec<u8> {
        let mut bytes = Vec::new();
        buffer.snapshot().write_to(&mut bytes).unwrap();
        bytes
    }
    fn save(snapshot: &Snapshot, encoding: Encoding, bom: bool) -> Vec<u8> {
        let mut bytes = Vec::new();
        encoding::write_snapshot(snapshot, SavePlan { encoding, bom, line_ending: None }, &mut bytes).unwrap();
        bytes
    }
    fn submit(worker: &JournalWorker, mut command: JournalCommand) {
        let deadline = Instant::now() + WAIT;
        loop {
            match worker.try_submit(command) {
                Ok(()) => return,
                Err(TrySendError::Full(returned)) => {
                    assert!(Instant::now() < deadline, "bounded journal queue wait");
                    command = returned;
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(TrySendError::Disconnected(_)) => panic!("journal worker disconnected"),
            }
        }
    }
    fn assert_rolled_back(state: &JournalRecovery, fixture: &Fixture) {
        assert!(state.summary().is_none());
        assert_eq!(state.accepted_origin_count(), 0, "no partial ID publication");
        assert_eq!(state.replay.snapshot().unwrap().len(), 0, "base and earlier batches must roll back too");
        assert_eq!(state.retained_origin_resources(), 0);
        assert!(!state.has_pending_payload());
        assert_eq!(fixture.count(), 0, "all staged files explicitly retired");
    }

    #[test]
    fn journal_replay_real_seed_edits_multireplacement_and_decreasing_undo_revision() {
        let fixture = Fixture::new();
        let mut registry = registry();
        let base = text(&mut registry, b"abcd\r\n");
        let batches = [
            EditBatch {
                from: 9,
                to: 12,
                parts: vec![part(1, 1, text(&mut registry, b"X")), part(3, 1, text(&mut registry, b"YZ"))],
            },
            EditBatch { from: 12, to: 9, parts: vec![part(0, 7, text(&mut registry, b"abcd\r\n"))] },
        ];
        fixture.write(&base, policy(), &batches);
        let mut state = fixture.state();
        let summary = state.recover(&mut fixture.reader(), &AtomicBool::new(false)).unwrap();
        assert_eq!(
            (summary.journal.revision, summary.journal.byte_len, summary.journal.tail),
            (9, 6, RecoveryTail::Complete)
        );
        assert_eq!(summary.policy, policy());
        assert_eq!(
            (summary.base.document, summary.base.revision, summary.base.original_identity),
            (DocumentId(17), 9, [1, 2, 3, 4])
        );
        assert_eq!(summary.base.original_path.as_deref(), Some(fixture.root.join("original").as_path()));
        let mut buffer = state.take_buffer(&AtomicBool::new(false)).unwrap();
        assert_eq!(logical(&buffer), b"abcd\r\n");
        assert!(!buffer.undo(), "recovery does not manufacture undo history");
        buffer.insert(0, b"!").unwrap();
        assert!(buffer.undo());
        assert_eq!(logical(&buffer), b"abcd\r\n");
        assert!(state.take_buffer(&AtomicBool::new(false)).is_err(), "one extraction");
        drop(buffer);
        state.abandon().unwrap();
        assert_eq!(fixture.count(), 0);
    }

    #[test]
    fn journal_replay_multiple_origins_reference_reuse_malformed_utf16_and_big5_after_original_gone() {
        let fixture = Fixture::new();
        let mut registry = registry();
        let raw = b"\xff\xfeA\0\0\xd8B\0\xff";
        let base = seed(&mut registry, fixture.original(raw, Encoding::UTF16_LE, 2));
        assert_eq!(base.logical, 4);
        let big5 = Buffer::from_decoded(fixture.original(b"\x88\x62\xa4\x40", Encoding::for_label("big5").unwrap(), 0));
        let first = capsule(&mut registry, big5.snapshot());
        let reference = capsule(
            &mut registry,
            big5.snapshot().slice(2..4).unwrap().concat(&big5.snapshot().slice(0..2).unwrap()).unwrap(),
        );
        assert!(reference.bytes.len() < first.bytes.len(), "second payload reuses admitted origin ID");
        let expected_len = base.logical + first.logical + reference.logical;
        let batches = [EditBatch {
            from: 9,
            to: 4,
            parts: vec![part(base.logical, 0, first.clone()), part(base.logical + first.logical, 0, reference)],
        }];
        let explicit = RecoveryPolicy { encoding: Encoding::UTF16_LE, bom: true, preferred_ending: LineEnding::Cr };
        fixture.write(&base, explicit, &batches);
        drop(big5);
        drop(registry);
        // No original pathname or source private owner is needed by recovery.
        for owner in fixture.owners.borrow_mut().drain(..) {
            assert!(owner.try_retire().is_ok());
        }
        assert!(!fixture.root.join("original").exists());
        let mut state = fixture.state();
        assert_eq!(state.recover(&mut fixture.reader(), &AtomicBool::new(false)).unwrap().policy, explicit);
        assert_eq!(state.accepted_origin_count(), 2);
        let buffer = state.take_buffer(&AtomicBool::new(false)).unwrap();
        assert_eq!(buffer.len(), expected_len);
        let snapshot = buffer.snapshot();
        let mut expected = raw.to_vec();
        expected.extend("\u{ca}\u{304}一\u{304}\u{ca}".encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(
            save(&snapshot, Encoding::UTF16_LE, true),
            expected,
            "raw malformed atoms/BOM and selected aliases survive real journal recovery"
        );
        let chunks: Vec<_> = snapshot
            .source_chunks(0..snapshot.len())
            .unwrap()
            .map(|chunk| match chunk.unwrap() {
                SourceChunk::Decoded { origin, range, .. } => (origin.raw_bytes().to_vec(), range),
                _ => panic!("provenance was flattened"),
            })
            .collect();
        assert_eq!(
            chunks,
            [
                (raw.to_vec(), 0..4),
                (b"\x88\x62\xa4\x40".to_vec(), 0..7),
                (b"\x88\x62\xa4\x40".to_vec(), 2..4),
                (b"\x88\x62\xa4\x40".to_vec(), 0..2)
            ]
        );
        drop(snapshot);
        drop(buffer);
        assert_eq!(state.abandon().unwrap().origins.retired, 2);
        assert_eq!(fixture.count(), 0);
    }

    #[test]
    fn journal_replay_bom_only_base_retains_empty_origin_and_explicit_policy() {
        let fixture = Fixture::new();
        let mut registry = registry();
        let base = seed(&mut registry, fixture.original(b"\xfe\xff", Encoding::UTF16_BE, 2));
        let explicit = RecoveryPolicy { encoding: Encoding::UTF8, bom: true, preferred_ending: LineEnding::Lf };
        fixture.write(&base, explicit, &[]);
        let mut state = fixture.state();
        assert_eq!(
            state.recover(&mut fixture.reader(), &AtomicBool::new(false)).unwrap().policy,
            explicit,
            "output is not inferred from UTF16 origin"
        );
        assert_eq!(state.accepted_origin_count(), 1);
        let buffer = state.take_buffer(&AtomicBool::new(false)).unwrap();
        assert!(buffer.is_empty());
        assert_eq!(save(&buffer.snapshot(), explicit.encoding, explicit.bom), b"\xef\xbb\xbf");
        drop(buffer);
        assert_eq!(state.abandon().unwrap().origins.retired, 1);
        assert_eq!(fixture.count(), 0);
    }

    #[test]
    fn journal_replay_interrupted_outer_batch_excluded_and_frozen_reader_keeps_prefix() {
        let fixture = Fixture::new();
        let mut registry = registry();
        let base = text(&mut registry, b"base");
        let batches = [
            EditBatch { from: 9, to: 12, parts: vec![part(4, 0, text(&mut registry, b"!"))] },
            EditBatch { from: 12, to: 2, parts: vec![part(0, 5, text(&mut registry, b"undo"))] },
        ];
        fixture.write(&base, policy(), &batches);
        let path = fixture.root.join("journal");
        let bytes = fs::read(&path).unwrap();
        fs::write(&path, &bytes[..bytes.len() - 1]).unwrap();
        let mut frozen = fixture.reader();
        fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(&bytes[bytes.len() - 1..]).unwrap();
        let mut state = fixture.state();
        let summary = state.recover(&mut frozen, &AtomicBool::new(false)).unwrap();
        assert_eq!(
            (summary.journal.revision, summary.journal.byte_len, summary.journal.tail),
            (12, 5, RecoveryTail::Interrupted)
        );
        let buffer = state.take_buffer(&AtomicBool::new(false)).unwrap();
        assert_eq!(logical(&buffer), b"base!");
        drop(buffer);
        state.abandon().unwrap();
        assert_eq!(fs::read(path).unwrap(), bytes, "consumer never truncates or rewrites journal");
    }

    #[test]
    fn journal_replay_later_bad_capsule_aborts_base_and_previously_valid_batch_ids() {
        let fixture = Fixture::new();
        let mut registry = registry();
        let base = seed(&mut registry, fixture.original(b"\xff\xfeA\0", Encoding::UTF16_LE, 2));
        let next = seed(&mut registry, fixture.original(b"\xa4\x40", Encoding::for_label("big5").unwrap(), 0));
        let mut bad = text(&mut registry, b"bad");
        *bad.bytes.last_mut().unwrap() = 255;
        fixture.write(
            &base,
            policy(),
            &[
                EditBatch { from: 9, to: 12, parts: vec![part(1, 0, next)] },
                EditBatch { from: 12, to: 2, parts: vec![part(0, 0, bad)] },
            ],
        );
        let before = fs::read(fixture.root.join("journal")).unwrap();
        let mut state = fixture.state();
        let mut applied = 0;
        let error = state
            .recover_observed(&mut fixture.reader(), &AtomicBool::new(false), |point| {
                if matches!(point, Point::CapsuleApplied) {
                    applied += 1;
                }
                Ok(())
            })
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(applied, 2, "base and earlier valid batch were actually staged");
        assert_rolled_back(&state, &fixture);
        assert!(state.take_buffer(&AtomicBool::new(false)).is_err());
        assert!(
            state.recover(&mut fixture.reader(), &AtomicBool::new(false)).unwrap_err().to_string().contains("one-shot")
        );
        assert_eq!(fs::read(fixture.root.join("journal")).unwrap(), before);
        state.abandon().unwrap();
    }

    #[test]
    fn journal_replay_checksum_failure_rolls_back_staged_base_without_batch_application() {
        let fixture = Fixture::new();
        let mut registry = registry();
        let base = seed(&mut registry, fixture.original(b"\xff\xfeA\0", Encoding::UTF16_LE, 2));
        fixture.write(
            &base,
            policy(),
            &[EditBatch { from: 9, to: 2, parts: vec![part(1, 0, text(&mut registry, b"!"))] }],
        );
        let path = fixture.root.join("journal");
        let mut bytes = fs::read(&path).unwrap();
        let n = bytes.len();
        bytes[n - 1] ^= 1;
        fs::write(&path, &bytes).unwrap();
        let mut state = fixture.state();
        let mut applied = 0;
        let error = state
            .recover_observed(&mut fixture.reader(), &AtomicBool::new(false), |point| {
                if matches!(point, Point::CapsuleApplied) {
                    applied += 1;
                }
                Ok(())
            })
            .unwrap_err();
        assert!(error.to_string().contains("checksum"));
        assert_eq!(applied, 1, "no unauthenticated outer batch reaches the model");
        assert_rolled_back(&state, &fixture);
        assert_eq!(fs::read(path).unwrap(), bytes);
    }

    #[test]
    fn journal_replay_finite_cancellation_retires_spool_and_prepared_origins() {
        for after_capsule in [false, true] {
            let fixture = Fixture::new();
            let mut registry = registry();
            let raw = vec![b'a'; 80 << 10];
            let base = seed(&mut registry, fixture.original(&raw, Encoding::UTF8, 0));
            fixture.write(&base, policy(), &[]);
            let mut state = fixture.state();
            let cancel = AtomicBool::new(false);
            let mut bytes_events = 0;
            let mut reached = false;
            let error = state
                .recover_observed(&mut fixture.reader(), &cancel, |point| {
                    match point {
                        Point::PayloadBytes if !after_capsule => {
                            bytes_events += 1;
                            if bytes_events == 2 {
                                assert_eq!(fixture.count(), 1, "private spool exists before cancellation");
                                reached = true;
                                cancel.store(true, Ordering::Relaxed);
                            }
                        }
                        Point::CapsuleApplied if after_capsule => {
                            assert!(fixture.count() >= 2, "spool and decoded origin exist before cancellation");
                            reached = true;
                            cancel.store(true, Ordering::Relaxed);
                        }
                        _ => (),
                    }
                    Ok(())
                })
                .unwrap_err();
            assert!(reached, "finite callback witness reached after real work");
            assert_eq!(error.kind(), io::ErrorKind::Interrupted);
            assert_rolled_back(&state, &fixture);
        }
    }

    #[test]
    fn journal_replay_raw_unknown_codec_and_mixed_frames_rejected_read_only() {
        for case in 0..3 {
            let fixture = Fixture::new();
            let mut registry = registry();
            let base = text(&mut registry, b"base");
            let mut codec = policy().to_codec().unwrap();
            if case == 1 {
                codec[4] = 2;
            }
            let edits = if case == 2 {
                vec![EditBatch {
                    from: 9,
                    to: 2,
                    parts: vec![Part {
                        offset: 4,
                        removed: 0,
                        capsule: Capsule { bytes: vec![b'!'], logical: 1 },
                        raw: true,
                    }],
                }]
            } else {
                vec![]
            };
            fixture.write_codec(&base, codec, case != 0, &edits);
            let bytes = fs::read(fixture.root.join("journal")).unwrap();
            let mut state = fixture.state();
            let error = state.recover(&mut fixture.reader(), &AtomicBool::new(false)).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(error.to_string().contains(if case == 1 { "codec/version" } else { "legacy recovery" }));
            assert_rolled_back(&state, &fixture);
            assert_eq!(fs::read(fixture.root.join("journal")).unwrap(), bytes);
        }
    }

    #[test]
    fn journal_replay_caps_and_parent_path_admitted_before_private_file_work() {
        let fixture = Fixture::new();
        let mut registry = registry();
        let base = seed(&mut registry, fixture.original(b"\xff\xfeA\0", Encoding::UTF16_LE, 2));
        fixture.write(&base, policy(), &[]);
        for case in 0..5 {
            let mut limits = limits();
            match case {
                0 => limits.max_payload_bytes = 1,
                1 => limits.max_base_metadata_bytes = 0,
                2 => limits.max_document_bytes = 0,
                3 => limits.replay.decode.max_physical_bytes = 1,
                _ => limits.replay.decode.max_logical_bytes = 0,
            }
            let mut state = JournalRecovery::new(fixture.private.clone(), limits).unwrap();
            assert!(state.recover(&mut fixture.reader(), &AtomicBool::new(false)).is_err());
            assert_rolled_back(&state, &fixture);
        }
        let mut small = limits();
        small.replay.max_private_path_bytes = fixture.private.as_os_str().as_encoded_bytes().len();
        assert!(JournalRecovery::new(fixture.private.clone(), small).is_err());
        assert!(JournalRecovery::new(PathBuf::from("relative-private"), limits()).is_err());
        assert_eq!(fixture.count(), 0);
    }

    #[test]
    fn journal_replay_replacement_and_cumulative_work_caps_abort_before_payload_io() {
        let fixture = Fixture::new();
        let mut registry = registry();
        let base = text(&mut registry, b"a");
        let large = text(&mut registry, &vec![b'b'; 80 << 10]);
        fixture.write(
            &base,
            policy(),
            &[
                EditBatch { from: 9, to: 12, parts: vec![part(1, 0, text(&mut registry, b"!"))] },
                EditBatch { from: 12, to: 2, parts: vec![part(2, 0, large)] },
            ],
        );
        for case in 0..4 {
            let mut limits = limits();
            match case {
                0 => limits.max_payload_bytes = 1024,
                1 => limits.max_document_bytes = 2,
                2 => limits.max_batches = 1,
                _ => limits.max_replacements = 1,
            }
            let mut state = JournalRecovery::new(fixture.private.clone(), limits).unwrap();
            let mut applied = 0;
            let mut bytes_events = 0;
            assert!(
                state
                    .recover_observed(&mut fixture.reader(), &AtomicBool::new(false), |point| {
                        match point {
                            Point::CapsuleApplied => applied += 1,
                            Point::PayloadBytes => bytes_events += 1,
                        }
                        Ok(())
                    })
                    .is_err()
            );
            assert_eq!(applied, 2, "base and first complete batch really staged before later admission refusal");
            assert_eq!(bytes_events, 2, "refused payload never read into memory or private spool");
            assert_rolled_back(&state, &fixture);
        }
    }

    #[test]
    fn journal_replay_zero_insert_delete_still_parses_complete_capsule() {
        let fixture = Fixture::new();
        let mut registry = registry();
        let base = text(&mut registry, b"abc");
        let empty = text(&mut registry, b"");
        assert_eq!(empty.logical, 0);
        assert!(!empty.bytes.is_empty(), "zero logical insert still has physical framing");
        fixture.write(&base, policy(), &[EditBatch { from: 9, to: 2, parts: vec![part(1, 2, empty)] }]);
        let mut state = fixture.state();
        let summary = state.recover(&mut fixture.reader(), &AtomicBool::new(false)).unwrap();
        assert_eq!((summary.journal.revision, summary.journal.byte_len), (2, 1));
        let buffer = state.take_buffer(&AtomicBool::new(false)).unwrap();
        assert_eq!(logical(&buffer), b"a");
        drop(buffer);
        state.abandon().unwrap();
        assert_eq!(fixture.count(), 0);
    }

    #[test]
    fn journal_replay_detached_opaque_and_ordinary_invalid_bytes_keep_distinct_provenance() {
        let fixture = Fixture::new();
        let mut registry = registry();
        let origin = Buffer::from_decoded(fixture.original(b"\xff\xfeA\0\0\xd8B\0", Encoding::UTF16_LE, 2));
        let mut foreign = Buffer::empty();
        foreign.insert_snapshot_range(0, &origin.snapshot(), 0..origin.len()).unwrap();
        let opaque = foreign.snapshot().slice(1..2).unwrap();
        let combined = literal(b"raw\xff").concat(&opaque).unwrap().concat(&literal(b"\0tail")).unwrap();
        let base = capsule(&mut registry, combined);
        fixture.write(&base, policy(), &[]);
        drop(origin);
        drop(foreign);
        drop(opaque);
        drop(registry);
        for owner in fixture.owners.borrow_mut().drain(..) {
            assert!(owner.try_retire().is_ok());
        }
        let mut state = fixture.state();
        state.recover(&mut fixture.reader(), &AtomicBool::new(false)).unwrap();
        assert_eq!(state.accepted_origin_count(), 0, "detached opaque data does not fabricate an origin");
        let buffer = state.take_buffer(&AtomicBool::new(false)).unwrap();
        assert_eq!(logical(&buffer), b"raw\xff\xff\0tail");
        let snapshot = buffer.snapshot();
        let mut opaque_count = 0;
        for chunk in snapshot.source_chunks(0..snapshot.len()).unwrap() {
            if let SourceChunk::Opaque { encoding, raw_bytes, raw_offsets, .. } = chunk.unwrap() {
                assert_eq!(encoding, Encoding::UTF16_LE);
                assert_eq!(raw_bytes, b"\0\xd8");
                assert_eq!(raw_offsets, [0u32.to_le_bytes(), 2u32.to_le_bytes()].concat());
                opaque_count += 1;
            }
        }
        assert_eq!(opaque_count, 1, "typed raw atom was not flattened to a logical FF byte");
        drop(snapshot);
        drop(buffer);
        state.abandon().unwrap();
        assert_eq!(fixture.count(), 0);
    }

    #[test]
    fn journal_replay_payload_cleanup_failure_retains_exact_object_for_explicit_retry() {
        let fixture = Fixture::new();
        let mut registry = registry();
        let base = text(&mut registry, &vec![b'a'; 80 << 10]);
        fixture.write(&base, policy(), &[]);
        let mut state = fixture.state();
        let mut blocker = None;
        let error = state
            .recover_observed(&mut fixture.reader(), &AtomicBool::new(false), |point| {
                if matches!(point, Point::CapsuleApplied) {
                    let directory = fs::read_dir(&fixture.private)
                        .unwrap()
                        .map(|e| e.unwrap().path())
                        .find(|p| p.file_name().unwrap().to_string_lossy().starts_with("kwil-capsule-"))
                        .unwrap();
                    let path = directory.join("owned-blocker");
                    fs::write(&path, b"x").unwrap();
                    blocker = Some(path);
                }
                Ok(())
            })
            .unwrap_err();
        assert!(error.to_string().contains("cleanup retained for retry"));
        assert!(state.has_pending_payload(), "retirement failure must retain the actual CapsulePayload");
        assert_eq!(state.accepted_origin_count(), 0);
        assert_eq!(state.replay.snapshot().unwrap().len(), 0);
        assert!(state.retry_retirement().is_err(), "retry must still address the retained directory");
        let blocker = blocker.unwrap();
        assert!(blocker.exists());
        fs::remove_file(blocker).unwrap();
        assert_eq!(state.retry_retirement().unwrap(), JournalRetirementStatus::default());
        assert_eq!(fixture.count(), 0);
        assert!(!state.has_pending_payload());
    }

    #[test]
    fn journal_replay_live_output_pins_retained_until_worker_retirement_retry() {
        let fixture = Fixture::new();
        let mut registry = registry();
        let base = seed(&mut registry, fixture.original(b"\xff\xfeA\0", Encoding::UTF16_LE, 2));
        fixture.write(&base, policy(), &[]);
        let mut state = fixture.state();
        state.recover(&mut fixture.reader(), &AtomicBool::new(false)).unwrap();
        let buffer = state.take_buffer(&AtomicBool::new(false)).unwrap();
        let snapshot = buffer.snapshot();
        assert_eq!(state.abandon().unwrap().origins.live_owners, 1);
        drop(buffer);
        assert_eq!(state.retry_retirement().unwrap().origins.live_owners, 1, "snapshot pin survives Buffer drop");
        assert_eq!(save(&snapshot, Encoding::UTF16_LE, true), b"\xff\xfeA\0");
        drop(snapshot);
        assert_eq!(state.retry_retirement().unwrap().origins.retired, 1);
        assert_eq!(fixture.count(), 0);
    }

    #[test]
    fn journal_replay_origin_cleanup_failure_preserves_paths_and_cause_for_retry() {
        let fixture = Fixture::new();
        let mut registry = registry();
        let base = seed(&mut registry, fixture.original(b"\xff\xfeA\0", Encoding::UTF16_LE, 2));
        fixture.write(&base, policy(), &[]);
        let mut state = fixture.state();
        let cancel = AtomicBool::new(false);
        let mut blocker = None;
        let error = state
            .recover_observed(&mut fixture.reader(), &cancel, |point| {
                if matches!(point, Point::CapsuleApplied) {
                    let directory = fs::read_dir(&fixture.private)
                        .unwrap()
                        .map(|e| e.unwrap().path())
                        .find(|p| p.file_name().unwrap().to_string_lossy().starts_with("kwil-decoded-"))
                        .unwrap();
                    let path = directory.join("owned-blocker");
                    fs::write(&path, b"x").unwrap();
                    blocker = Some(path);
                    cancel.store(true, Ordering::Relaxed);
                }
                Ok(())
            })
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted, "cancellation cause survives cleanup error context");
        assert!(error.to_string().contains("cleanup retained for retry"));
        assert_eq!(state.accepted_origin_count(), 0);
        assert_eq!(state.replay.snapshot().unwrap().len(), 0);
        assert_eq!(state.retained_origin_resources(), 1);
        assert!(!state.has_pending_payload());
        assert!(state.retry_retirement().is_err());
        fs::remove_file(blocker.unwrap()).unwrap();
        assert_eq!(state.retry_retirement().unwrap().origins.retired, 1);
        assert_rolled_back(&state, &fixture);
    }

    #[test]
    fn journal_replay_cancelled_extraction_is_terminal_and_keeps_worker_owners() {
        let fixture = Fixture::new();
        let mut registry = registry();
        let base = seed(&mut registry, fixture.original(b"\xff\xfeA\0", Encoding::UTF16_LE, 2));
        fixture.write(&base, policy(), &[]);
        let mut state = fixture.state();
        state.recover(&mut fixture.reader(), &AtomicBool::new(false)).unwrap();
        assert_eq!(state.take_buffer(&AtomicBool::new(true)).err().unwrap().kind(), io::ErrorKind::Interrupted);
        assert_eq!(state.retained_origin_resources(), 1, "cancelled finish cannot lose explicit owner");
        assert!(
            state.take_buffer(&AtomicBool::new(false)).is_err(),
            "failed extraction is terminal even if entry cancellation left assembler retryable"
        );
        assert_eq!(state.abandon().unwrap().origins.retired, 1);
        assert_eq!(fixture.count(), 0);
    }

    #[test]
    fn journal_replay_policy_codec_is_small_strict_versioned_and_roundtrips_all_endings() {
        for encoding in [Encoding::UTF8, Encoding::UTF16_LE, Encoding::UTF16_BE, Encoding::for_label("big5").unwrap()] {
            for preferred_ending in [LineEnding::Lf, LineEnding::CrLf, LineEnding::Cr] {
                let explicit = RecoveryPolicy {
                    encoding,
                    bom: encoding != Encoding::for_label("big5").unwrap(),
                    preferred_ending,
                };
                let codec = explicit.to_codec().unwrap();
                assert!(codec.len() <= 8 + MAX_ENCODING_NAME);
                assert_eq!(RecoveryPolicy::from_codec(&codec).unwrap(), explicit);
                for at in 0..codec.len() {
                    assert!(RecoveryPolicy::from_codec(&codec[..at]).is_err());
                }
                for (at, value) in [(0, 0), (4, 2), (5, 2), (6, 3), (7, 255)] {
                    let mut bad = codec.clone();
                    bad[at] = value;
                    assert!(RecoveryPolicy::from_codec(&bad).is_err());
                }
                let mut extra = codec.clone();
                extra.push(0);
                assert!(RecoveryPolicy::from_codec(&extra).is_err());
            }
        }
        assert!(
            RecoveryPolicy {
                encoding: Encoding::for_label("big5").unwrap(),
                bom: true,
                preferred_ending: LineEnding::Lf
            }
            .to_codec()
            .is_err()
        );
        for label in ["utf8", "ISO-2022-JP", "unknown"] {
            let mut codec = b"KWJR\x01\x00\x00".to_vec();
            codec.push(label.len() as u8);
            codec.extend_from_slice(label.as_bytes());
            assert!(RecoveryPolicy::from_codec(&codec).is_err(), "strict canonical/stateless policy: {label}");
        }
    }
}
