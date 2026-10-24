//! Trusted worker assembly of a lossless recovery stream. Frame validation,
//! backing preparation, journal retention and document identities are callers'
//! responsibilities. This module never opens an external source pathname.

use crate::decoded_backing::DecodedSnapshot;
use crate::encoding::Encoding;
use crate::piece_tree::{self, Tree};
use crate::{Buffer, Snapshot};
use std::io;
use std::ops::Range;

/// Caller-provided recovery admission limits, not product settings. The work
/// limits are cumulative conservative upper bounds across accepted revisions,
/// NOT exact live-memory/resource counters. They cover transient tree paths,
/// registry/index allocation and bookkeeping before growth. Exhaustion refuses
/// the revision: callers must retain the journal and choose checkpoint/rotation
/// policy themselves. Resident payload and subsequent editing undo have their
/// separate ordinary Buffer caps. No replay history is constructed.
#[derive(Clone, Copy, Debug)]
pub struct RecoveryLimits {
    pub add_memory_bytes: usize,
    pub undo_memory_bytes: usize,
    pub max_origins: usize,
    pub max_records: usize,
    pub max_pieces: usize,
    pub max_blocks: usize,
    pub max_metadata_bytes: usize,
}

/// Conservative cumulative admitted recovery work, including preallocated
/// registry slots and sparse index upper bounds. Failed revisions charge zero
/// accepted work. Shared blocks/indexes remain charged only at construction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RecoveryUsage {
    pub records: usize,
    pub pieces: usize,
    pub blocks: usize,
    pub metadata_bytes: usize,
}
impl RecoveryUsage {
    fn admit(self, limits: RecoveryLimits, charge: Self) -> io::Result<Self> {
        fn add(old: usize, next: usize, cap: usize, name: &str) -> io::Result<usize> {
            old.checked_add(next).filter(|&total| total <= cap).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, format!("recovery work limit exceeded: {name}"))
            })
        }
        Ok(Self {
            records: add(self.records, charge.records, limits.max_records, "records")?,
            pieces: add(self.pieces, charge.pieces, limits.max_pieces, "pieces")?,
            blocks: add(self.blocks, charge.blocks, limits.max_blocks, "blocks")?,
            metadata_bytes: add(
                self.metadata_bytes,
                charge.metadata_bytes,
                limits.max_metadata_bytes,
                "metadata bytes",
            )?,
        })
    }
}

struct Origin {
    id: u64,
    origin: DecodedSnapshot,
    snapshot: Snapshot,
}

/// Worker-only reconstruction under one stable document lineage. The registry
/// pins PRIVATE immutable origins; the caller retains their DecodedBacking
/// owners and retires them on a worker after all snapshots/registry pins drop.
/// Empty/BOM-only origins retain identity in this registry despite empty trees.
/// Output encoding/BOM and revision/saved identities remain caller metadata.
pub struct RecoveryAssembler {
    buffer: Buffer,
    origins: Vec<Origin>,
    limits: RecoveryLimits,
    usage: RecoveryUsage,
}
impl RecoveryAssembler {
    pub fn new(limits: RecoveryLimits) -> io::Result<Self> {
        let bytes = limits
            .max_origins
            .checked_mul(std::mem::size_of::<Origin>())
            .and_then(|n| n.checked_add(std::mem::size_of::<Self>() + std::mem::size_of::<RecoveryRevision<'_>>()))
            .ok_or_else(overflow)?;
        let usage = RecoveryUsage::default()
            .admit(limits, RecoveryUsage { metadata_bytes: bytes, ..RecoveryUsage::default() })?;
        // Preallocate the entire explicitly bounded registry before any ID is
        // accepted. Revisions append pending entries; no growth/reallocation.
        let mut origins = Vec::new();
        origins.try_reserve_exact(limits.max_origins).map_err(io::Error::other)?;
        let mut buffer = Buffer::with_add_memory_limit(limits.add_memory_bytes);
        buffer.set_undo_memory_limit(limits.undo_memory_bytes);
        Ok(Self { buffer, origins, limits, usage })
    }

    pub fn snapshot(&self) -> Snapshot {
        self.buffer.snapshot()
    }
    pub fn origin_count(&self) -> usize {
        self.origins.len()
    }
    pub fn usage(&self) -> RecoveryUsage {
        self.usage
    }

    /// All origin IDs, fragments and sequential-coordinate replacements are
    /// staged until commit. A failed operation poisons and rolls back this
    /// entire revision immediately. Dropping it also rolls back pending IDs.
    pub fn begin_revision(&mut self) -> RecoveryRevision<'_> {
        let root = self.buffer.snapshot().root;
        let origin_start = self.origins.len();
        let usage = self.usage;
        RecoveryRevision { assembler: self, root, fragment: None, origin_start, usage, failed: false, committed: false }
    }

    /// Consume the registry on the worker and return a normal editable Buffer.
    /// Add/undo caps chosen at construction remain in effect; replay revisions
    /// contribute no undo entries. Cancellation returns no restored buffer.
    pub fn finish(mut self, mut cancelled: impl FnMut() -> bool) -> io::Result<Buffer> {
        poll(&mut cancelled)?;
        self.origins.clear();
        poll(&mut cancelled)?;
        Ok(self.buffer)
    }
}

/// One atomic journal/model revision. No per-record/piece descriptor vector is
/// retained: complete fragments are joined incrementally into a staged tree.
/// Only commit publishes the tree and new numeric origin IDs. Reuse existing
/// IDs across revisions/capsules without rescanning or importing their text.
pub struct RecoveryRevision<'a> {
    assembler: &'a mut RecoveryAssembler,
    root: Tree,
    fragment: Option<Tree>,
    origin_start: usize,
    usage: RecoveryUsage,
    failed: bool,
    committed: bool,
}
impl RecoveryRevision<'_> {
    /// Cumulative admitted work staged by this revision, before publication.
    pub fn usage(&self) -> RecoveryUsage {
        self.usage
    }

    /// Interpret the complete historical raw-v1 prefix, after all raw edits.
    /// Valid UTF-8 (including a leading BOM) stays ordinary text; each invalid
    /// source BYTE becomes one UTF8-labelled opaque atom. Logical byte length
    /// and all historical coordinates stay unchanged. No root is published
    /// until commit. Typed/decoded input refuses rather than losing provenance.
    /// Invoke before registering the first typed capsule's origins.
    pub fn promote_raw_v1(&mut self, mut cancelled: impl FnMut() -> bool) -> io::Result<()> {
        let result = (|| {
            self.ready()?;
            poll(&mut cancelled)?;
            if self.fragment.is_some() {
                return Err(invalid("raw-v1 promotion requires a complete prefix"));
            }
            if !self.assembler.origins.is_empty() {
                return Err(invalid("raw-v1 promotion refuses decoded origins"));
            }
            // Three fixed packing Vecs, the bounded opaque-only flush scratch,
            // decoder/iterator state and source traversal coexist with source
            // and target. Source construction remains charged in self.usage.
            let scratch = 4 * piece_tree::ADD_BLOCK + 1024;
            let mut usage = self.reserve(0, 0, sum(&[scratch, tree_work(&self.root)?])?)?;
            let limits = self.assembler.limits;
            let root = self.assembler.buffer.recovery_promote_raw_v1(
                &self.root,
                &mut |blocks, target| {
                    usage = usage.admit(
                        limits,
                        RecoveryUsage {
                            records: 1,
                            pieces: 1,
                            blocks,
                            metadata_bytes: sum(&[block_work(blocks)?, tree_work(target)?])?,
                        },
                    )?;
                    Ok(())
                },
                &mut || poll(&mut cancelled),
            )?;
            poll(&mut cancelled)?;
            self.root = root;
            self.usage = usage;
            Ok(())
        })();
        self.checked(result)
    }

    /// Check a fresh origin's conservative index/path charge BEFORE its raw
    /// input is read or private files are created. This allocates nothing and
    /// does not publish an ID or charge twice. The worker must register the
    /// prepared origin immediately, with actual lengths within these bounds.
    pub fn preflight_origin(
        &mut self,
        id: u64,
        logical_bytes_bound: u64,
        path_bytes_bound: usize,
        mut cancelled: impl FnMut() -> bool,
    ) -> io::Result<()> {
        let result = (|| {
            self.ready()?;
            poll(&mut cancelled)?;
            if self.assembler.origins.iter().any(|entry| entry.id == id) {
                return Err(invalid("duplicate recovery origin ID"));
            }
            if self.assembler.origins.len() >= self.assembler.limits.max_origins {
                return Err(invalid("recovery work limit exceeded: origins"));
            }
            let len = usize::try_from(logical_bytes_bound).map_err(|_| overflow())?;
            let metadata = origin_work_parts(len, path_bytes_bound)?;
            self.reserve(usize::from(len != 0), 0, metadata)?;
            poll(&mut cancelled)
        })();
        self.checked(result)
    }
    fn ready(&self) -> io::Result<()> {
        if self.failed { Err(invalid("recovery revision is poisoned")) } else { Ok(()) }
    }
    fn checked<T>(&mut self, result: io::Result<T>) -> io::Result<T> {
        if result.is_err() {
            self.failed = true;
            self.fragment = None;
            self.root = self.assembler.buffer.snapshot().root;
            self.assembler.origins.truncate(self.origin_start);
        }
        result
    }
    fn reserve(&self, pieces: usize, blocks: usize, metadata_bytes: usize) -> io::Result<RecoveryUsage> {
        self.usage.admit(self.assembler.limits, RecoveryUsage { records: 1, pieces, blocks, metadata_bytes })
    }

    /// Stage one immutable private origin. Duplicate IDs refuse the revision.
    /// Registering another ID for the very same snapshot reuses its indexed
    /// cached root. Otherwise one sparse logical index is built with polling.
    pub fn register_origin(
        &mut self,
        id: u64,
        origin: DecodedSnapshot,
        mut cancelled: impl FnMut() -> bool,
    ) -> io::Result<()> {
        let result = (|| {
            self.ready()?;
            poll(&mut cancelled)?;
            if self.assembler.origins.iter().any(|entry| entry.id == id) {
                return Err(invalid("duplicate recovery origin ID"));
            }
            if self.assembler.origins.len() >= self.assembler.limits.max_origins {
                return Err(invalid("recovery work limit exceeded: origins"));
            }
            let cached = self.assembler.origins.iter().find(|entry| entry.origin.same_origin(&origin));
            let metadata = if cached.is_some() { 0 } else { origin_work(&origin)? };
            let usage =
                self.reserve(usize::from(cached.is_none() && !origin.logical_bytes().is_empty()), 0, metadata)?;
            let snapshot = if let Some(entry) = cached {
                entry.snapshot.clone()
            } else {
                self.assembler.buffer.recovery_origin(origin.clone(), &mut || poll(&mut cancelled))?
            };
            poll(&mut cancelled)?;
            self.assembler.origins.push(Origin { id, origin, snapshot });
            self.usage = usage;
            Ok(())
        })();
        self.checked(result)
    }

    pub fn begin_fragment(&mut self) -> io::Result<()> {
        let result = (|| {
            self.ready()?;
            if self.fragment.is_some() {
                return Err(invalid("unconsumed recovery fragment"));
            }
            let usage = self.reserve(0, 0, 0)?;
            self.fragment = Some(None);
            self.usage = usage;
            Ok(())
        })();
        self.checked(result)
    }

    fn fragment(&self) -> io::Result<&Tree> {
        self.fragment.as_ref().ok_or_else(|| invalid("no active recovery fragment"))
    }
    fn appended(&mut self, tree: Tree, usage: RecoveryUsage) {
        // Callers have validated the active fragment and all fallible work.
        let before = self.fragment.take().flatten();
        self.fragment = Some(piece_tree::join(before, tree));
        self.usage = usage;
    }

    pub fn append_origin(&mut self, id: u64, range: Range<u64>, mut cancelled: impl FnMut() -> bool) -> io::Result<()> {
        let result = (|| {
            self.ready()?;
            poll(&mut cancelled)?;
            let fragment = self.fragment()?;
            let entry = self
                .assembler
                .origins
                .iter()
                .find(|entry| entry.id == id)
                .ok_or_else(|| invalid("unknown recovery origin ID"))?;
            piece_tree::recovery_validate_range(&entry.origin, range.clone(), &mut || poll(&mut cancelled))?;
            length(fragment).checked_add(range.end - range.start).ok_or_else(overflow)?;
            let work = sum(&[tree_work(&entry.snapshot.root)?, tree_work(fragment)?])?;
            let usage = self.reserve(if range.is_empty() { 0 } else { 4 }, 0, work)?;
            let (_, tail) = piece_tree::split(entry.snapshot.root.clone(), range.start).map_err(io::Error::other)?;
            let (selected, _) = piece_tree::split(tail, range.end - range.start).map_err(io::Error::other)?;
            poll(&mut cancelled)?;
            self.appended(selected, usage);
            Ok(())
        })();
        self.checked(result)
    }

    /// One bounded block of arbitrary ordinary logical bytes, matching Added
    /// semantics. UTF8 sequences may straddle blocks; FF here is ordinary data,
    /// never assigned a detached opaque provenance tag by guessing.
    pub fn append_bytes(&mut self, bytes: &[u8], mut cancelled: impl FnMut() -> bool) -> io::Result<()> {
        let result = (|| {
            self.ready()?;
            poll(&mut cancelled)?;
            let fragment = self.fragment()?;
            if bytes.len() > piece_tree::ADD_BLOCK {
                return Err(invalid("recovery logical block exceeds 64KiB"));
            }
            length(fragment).checked_add(bytes.len() as u64).ok_or_else(overflow)?;
            let blocks = usize::from(!bytes.is_empty());
            let usage = self.reserve(blocks, blocks, sum(&[tree_work(fragment)?, block_work(blocks)?])?)?;
            let tree = self.assembler.buffer.recovery_bytes(bytes)?;
            poll(&mut cancelled)?;
            self.appended(tree, usage);
            Ok(())
        })();
        self.checked(result)
    }

    /// Strict opaque-only block, <=64KiB raw/table and <=16383 logical FF atoms.
    /// LE-u32 endpoints must start at zero, strictly increase, end at raw.len(),
    /// and contain exactly logical_count+1 entries. Validation precedes writes.
    pub fn append_opaque(
        &mut self,
        encoding: Encoding,
        raw: &[u8],
        endpoints_le: &[u8],
        logical_count: u64,
        mut cancelled: impl FnMut() -> bool,
    ) -> io::Result<()> {
        let result = (|| {
            self.ready()?;
            poll(&mut cancelled)?;
            let fragment = self.fragment()?;
            piece_tree::validate_recovery_opaque(raw, endpoints_le, logical_count)?;
            length(fragment).checked_add(logical_count).ok_or_else(overflow)?;
            let usage = self.reserve(1, 3, sum(&[tree_work(fragment)?, block_work(3)?])?)?;
            let tree = self.assembler.buffer.recovery_opaque(encoding, raw, endpoints_le, logical_count)?;
            poll(&mut cancelled)?;
            self.appended(tree, usage);
            Ok(())
        })();
        self.checked(result)
    }

    /// Detached valid text with exact noncanonical source bytes. Admission
    /// includes the temporary rebased endpoint table as well as all payloads.
    pub fn append_preserved(
        &mut self,
        encoding: Encoding,
        bytes: &[u8],
        raw: &[u8],
        endpoints_le: &[u8],
        mut cancelled: impl FnMut() -> bool,
    ) -> io::Result<()> {
        let result = (|| {
            self.ready()?;
            poll(&mut cancelled)?;
            let fragment = self.fragment()?;
            piece_tree::validate_recovery_preserved(encoding, bytes, raw, endpoints_le, &mut || poll(&mut cancelled))?;
            length(fragment).checked_add(bytes.len() as u64).ok_or_else(overflow)?;
            let usage = self.reserve(1, 3, sum(&[tree_work(fragment)?, block_work(3)?, endpoints_le.len()])?)?;
            let tree = self
                .assembler
                .buffer
                .recovery_preserved(encoding, bytes, raw, endpoints_le, &mut || poll(&mut cancelled))?;
            poll(&mut cancelled)?;
            self.appended(tree, usage);
            Ok(())
        })();
        self.checked(result)
    }

    /// Apply the complete fragment in the CURRENT staged coordinates. Repeated
    /// replacements form one atomic revision, with no replay undo history.
    pub fn replace(&mut self, range: Range<u64>, mut cancelled: impl FnMut() -> bool) -> io::Result<()> {
        let result = (|| {
            self.ready()?;
            poll(&mut cancelled)?;
            let fragment = self.fragment()?;
            if range.start > range.end || range.end > length(&self.root) {
                return Err(invalid("recovery replacement range outside staged buffer"));
            }
            (length(&self.root) - (range.end - range.start)).checked_add(length(fragment)).ok_or_else(overflow)?;
            piece_tree::recovery_cut(&self.root, range.start, &mut || poll(&mut cancelled))?;
            poll(&mut cancelled)?;
            piece_tree::recovery_cut(&self.root, range.end, &mut || poll(&mut cancelled))?;
            poll(&mut cancelled)?;
            let usage = self.reserve(4, 0, sum(&[tree_work(&self.root)?, tree_work(fragment)?])?)?;
            let (left, tail) = piece_tree::split(self.root.clone(), range.start).map_err(io::Error::other)?;
            let (_, right) = piece_tree::split(tail, range.end - range.start).map_err(io::Error::other)?;
            poll(&mut cancelled)?;
            let fragment = self.fragment.take().flatten();
            self.root = piece_tree::join(piece_tree::join(left, fragment), right);
            self.usage = usage;
            Ok(())
        })();
        self.checked(result)
    }

    pub fn commit(mut self, mut cancelled: impl FnMut() -> bool) -> io::Result<()> {
        let result = (|| {
            self.ready()?;
            poll(&mut cancelled)?;
            if self.fragment.is_some() {
                return Err(invalid("unconsumed recovery fragment at commit"));
            }
            // original_uses allocates only sparse per-source bookkeeping and a
            // tree-height traversal stack. Charge its worst case before growth.
            let bookkeeping = self.assembler.origins.len().checked_mul(1024).ok_or_else(overflow)?;
            let usage = self.reserve(0, 0, sum(&[bookkeeping, tree_work(&self.root)?])?)?;
            poll(&mut cancelled)?;
            let snapshot = Buffer::recovery_snapshot(self.root.take());
            poll(&mut cancelled)?;
            self.assembler.buffer.recovery_install(snapshot);
            self.assembler.usage = usage;
            self.committed = true;
            Ok(())
        })();
        self.checked(result)
    }
}
impl Drop for RecoveryRevision<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.fragment = None;
            self.root = None;
            self.assembler.origins.truncate(self.origin_start);
        }
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn overflow() -> io::Error {
    invalid("recovery work/length overflow")
}
fn poll(cancelled: &mut impl FnMut() -> bool) -> io::Result<()> {
    if cancelled() { Err(io::Error::from(io::ErrorKind::Interrupted)) } else { Ok(()) }
}
fn length(tree: &Tree) -> u64 {
    tree.as_ref().map_or(0, |node| node.metrics.bytes)
}
fn tree_work(tree: &Tree) -> io::Result<usize> {
    piece_tree::recovery_tree_work(tree).ok_or_else(overflow)
}
fn block_work(blocks: usize) -> io::Result<usize> {
    sum(&[piece_tree::recovery_block_work(blocks).ok_or_else(overflow)?, tree_work(&None)?])
}
fn sum(parts: &[usize]) -> io::Result<usize> {
    parts.iter().try_fold(0_usize, |sum, &next| sum.checked_add(next).ok_or_else(overflow))
}

fn origin_work(origin: &DecodedSnapshot) -> io::Result<usize> {
    let paths = origin.paths();
    let path_bytes = sum(&[
        paths.directory.capacity(),
        paths.raw.capacity(),
        paths.logical.capacity(),
        paths.checkpoints.capacity(),
        origin.source_path().as_os_str().len(),
    ])?;
    origin_work_parts(origin.logical_bytes().len(), path_bytes)
}
fn origin_work_parts(len: usize, path_bytes: usize) -> io::Result<usize> {
    // Default sparse line checkpoints can occur at most once per1024 bytes;
    // byte-rank checkpoints at most once per64KiB (chunk grid is a multiple).
    // Vec growth is conservatively four times the final checkpoint count.
    let lines = len
        .checked_div(crate::line_index::DEFAULT_EVERY as usize)
        .and_then(|n| n.checked_add(1))
        .ok_or_else(overflow)?;
    let ranks = len.div_ceil(64 << 10).checked_add(1).ok_or_else(overflow)?;
    let index = lines
        .checked_add(ranks)
        .and_then(|n| n.checked_mul(4 * std::mem::size_of::<(u64, u64)>()))
        .ok_or_else(overflow)?;
    sum(&[index, std::mem::size_of::<crate::decoded_piece::IndexedDecoded>(), 1024, path_bytes, tree_work(&None)?])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoded_backing::{DecodedBacking, DecodedSnapshot, Interpretation};
    use crate::encoding::{self, Encoding, EncodingError, SavePlan};
    use crate::{Snapshot, SourceChunk};
    use std::cell::RefCell;
    use std::io::{self, Cursor, Write};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    struct Fixture {
        directory: PathBuf,
        owners: RefCell<Vec<DecodedBacking>>,
    }
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let directory = std::env::temp_dir().join(format!(
                "kwil-assembly-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&directory).unwrap();
            Self { directory, owners: RefCell::new(Vec::new()) }
        }
        fn origin(&self, bytes: &[u8], encoding: Encoding, bom: usize) -> DecodedSnapshot {
            let owner = DecodedBacking::prepare_from_reader(
                &mut Cursor::new(bytes),
                bytes.len() as u64,
                Interpretation::new(encoding, bom).unwrap(),
                &self.directory,
                &AtomicBool::new(false),
            )
            .unwrap();
            let origin = owner.snapshot();
            self.owners.borrow_mut().push(owner);
            origin
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            for owner in self.owners.get_mut().drain(..) {
                assert!(owner.try_retire().is_ok(), "recovery leaked origin pin");
            }
            std::fs::remove_dir_all(&self.directory).unwrap();
        }
    }
    fn limits() -> RecoveryLimits {
        RecoveryLimits {
            add_memory_bytes: 4096,
            undo_memory_bytes: 1 << 20,
            max_origins: 8,
            max_records: 256,
            max_pieces: 1024,
            max_blocks: 128,
            max_metadata_bytes: 16 << 20,
        }
    }
    fn bytes(snapshot: &Snapshot) -> Vec<u8> {
        let mut out = Vec::new();
        snapshot.write_to(&mut out).unwrap();
        out
    }
    #[test]
    fn recovery_origin_preflight_allocates_nothing_and_refuses_before_registration() {
        let mut assembler = RecoveryAssembler::new(limits()).unwrap();
        let before = assembler.usage();
        {
            let mut revision = assembler.begin_revision();
            revision.preflight_origin(1, 1024, 4096, || false).unwrap();
            assert_eq!(revision.usage, before, "preflight must not double-charge registration");
            assert_eq!(revision.assembler.origins.len(), 0);
        }
        assert_eq!(assembler.usage(), before);
        assert_eq!(assembler.origin_count(), 0);
        let mut cap = limits();
        cap.max_metadata_bytes = before.metadata_bytes;
        let mut assembler = RecoveryAssembler::new(cap).unwrap();
        let mut revision = assembler.begin_revision();
        assert!(revision.preflight_origin(1, 1, 1, || false).is_err());
        assert!(revision.commit(|| false).is_err());
        assert!(assembler.snapshot().is_empty());
        assert_eq!(assembler.origin_count(), 0);
    }
    fn plan(encoding: Encoding, bom: bool) -> SavePlan {
        SavePlan { encoding, bom, line_ending: None }
    }
    fn save(snapshot: &Snapshot, encoding: Encoding, bom: bool) -> Vec<u8> {
        let mut out = Vec::new();
        encoding::write_snapshot(snapshot, plan(encoding, bom), &mut out).unwrap();
        out
    }
    fn literal(assembler: &mut RecoveryAssembler, range: std::ops::Range<u64>, text: &[u8]) {
        let mut revision = assembler.begin_revision();
        revision.begin_fragment().unwrap();
        revision.append_bytes(text, || false).unwrap();
        revision.replace(range, || false).unwrap();
        revision.commit(|| false).unwrap();
    }

    #[test]
    fn recovery_origins_share_indices_aliases_and_generations_without_import() {
        let fixture = Fixture::new();
        let big5 = Encoding::for_label("big5").unwrap();
        let a = fixture.origin(b"\x88\x62!", big5, 0);
        let b = fixture.origin(b"\xff\xfeA\0\0\xd8\xff", Encoding::UTF16_LE, 2);
        let mut assembler = RecoveryAssembler::new(limits()).unwrap();
        let mut revision = assembler.begin_revision();
        revision.register_origin(1, a.clone(), || false).unwrap();
        revision.register_origin(2, b.clone(), || false).unwrap();
        revision.register_origin(3, a.clone(), || false).unwrap();
        revision.begin_fragment().unwrap();
        revision.append_origin(1, 0..5, || false).unwrap();
        revision.replace(0..0, || false).unwrap();
        revision.commit(|| false).unwrap();
        let first = assembler.snapshot();
        let key = crate::piece_tree::original_uses(&first.root, 0..first.len()).into_keys().next().unwrap();
        for id in [1, 3, 1] {
            let mut revision = assembler.begin_revision();
            revision.begin_fragment().unwrap();
            revision.append_origin(id, 0..2, || false).unwrap();
            revision.append_origin(id, 2..4, || false).unwrap();
            revision.replace(0..first.len(), || false).unwrap();
            revision.commit(|| false).unwrap();
            let current = assembler.snapshot();
            let uses = crate::piece_tree::original_uses(&current.root, 0..current.len());
            assert_eq!(uses.len(), 1);
            assert!(uses.contains_key(&key), "references must reuse the same indexed origin");
            assert_eq!(save(&current, Encoding::UTF8, false), "Ê\u{304}".as_bytes());
            // Reconstitute the complete source for the next generation.
            let mut revision = assembler.begin_revision();
            revision.begin_fragment().unwrap();
            revision.append_origin(1, 0..5, || false).unwrap();
            revision.replace(0..4, || false).unwrap();
            revision.commit(|| false).unwrap();
        }
        let mut revision = assembler.begin_revision();
        revision.begin_fragment().unwrap();
        revision.append_origin(2, 0..b.logical_bytes().len() as u64, || false).unwrap();
        revision.replace(0..5, || false).unwrap();
        revision.commit(|| false).unwrap();
        assert_eq!(save(&assembler.snapshot(), Encoding::UTF16_LE, true), b"\xff\xfeA\0\0\xd8\xff");
        assert_eq!(save(&first, big5, false), b"\x88\x62!");
        assert_eq!(assembler.buffer.import_work().streams, 0);
        drop((first, assembler, a, b));
    }

    #[derive(Default)]
    struct Untouched(usize);
    impl Write for Untouched {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 += 1;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.0 += 1;
            Ok(())
        }
    }
    #[test]
    fn recovery_big5_partial_start_retains_unmappable_and_zero_writer() {
        partial(2..4, '\u{304}');
    }
    #[test]
    fn recovery_big5_partial_end_retains_unmappable_and_zero_writer() {
        partial(0..2, 'Ê');
    }
    fn partial(range: std::ops::Range<u64>, character: char) {
        let fixture = Fixture::new();
        let big5 = Encoding::for_label("big5").unwrap();
        let mut assembler = RecoveryAssembler::new(limits()).unwrap();
        let mut revision = assembler.begin_revision();
        revision.register_origin(9, fixture.origin(b"\x88\x62", big5, 0), || false).unwrap();
        revision.begin_fragment().unwrap();
        revision.append_bytes(b"valid-prefix", || false).unwrap();
        revision.append_origin(9, range, || false).unwrap();
        revision.replace(0..0, || false).unwrap();
        revision.commit(|| false).unwrap();
        let mut writer = Untouched::default();
        let snapshot = assembler.snapshot();
        assert!(
            matches!(encoding::write_snapshot(&snapshot, plan(big5, false), &mut writer), Err(EncodingError::Unmappable { character: c, .. }) if c == character)
        );
        assert_eq!(writer.0, 0);
    }

    #[test]
    fn recovery_detached_opaque_preserves_exact_raw_table_and_spill_cap() {
        let mut config = limits();
        config.add_memory_bytes = 0;
        let mut assembler = RecoveryAssembler::new(config).unwrap();
        let mut revision = assembler.begin_revision();
        revision.begin_fragment().unwrap();
        let table = [0_u32, 2, 3].into_iter().flat_map(u32::to_le_bytes).collect::<Vec<_>>();
        revision.append_opaque(Encoding::UTF16_LE, b"\0\xd8\xff", &table, 2, || false).unwrap();
        revision.replace(0..0, || false).unwrap();
        revision.commit(|| false).unwrap();
        let snapshot = assembler.snapshot();
        assert_eq!(bytes(&snapshot), b"\xff\xff");
        assert_eq!(save(&snapshot, Encoding::UTF16_LE, true), b"\xff\xfe\0\xd8\xff");
        let mut chunks = snapshot.source_chunks(0..2).unwrap();
        let SourceChunk::Opaque { encoding, raw_bytes, raw_offsets, .. } = chunks.next().unwrap().unwrap() else {
            panic!("typed opaque provenance lost")
        };
        assert_eq!(encoding, Encoding::UTF16_LE);
        assert_eq!(raw_bytes, b"\0\xd8\xff");
        assert_eq!(raw_offsets, table);
        assert_eq!(assembler.buffer.add_memory_bytes(), 0);
        assert_eq!(snapshot.storage_resources().blocks, 3);
        assert_eq!(snapshot.storage_resources().spilled_blocks, 3);
        assert_eq!(crate::piece_tree::spill_paths(&snapshot.root).len(), 3);
    }

    #[test]
    fn recovery_revision_failure_is_atomic_for_replacements_ids_and_pins() {
        let fixture = Fixture::new();
        let mut assembler = RecoveryAssembler::new(limits()).unwrap();
        literal(&mut assembler, 0..0, b"abcdef");
        let accepted = assembler.snapshot();
        let usage = assembler.usage();
        let origin = fixture.origin(b"old", Encoding::UTF8, 0);
        let mut revision = assembler.begin_revision();
        revision.register_origin(4, origin.clone(), || false).unwrap();
        revision.begin_fragment().unwrap();
        revision.append_origin(4, 0..3, || false).unwrap();
        revision.replace(1..3, || false).unwrap();
        revision.begin_fragment().unwrap();
        revision.append_bytes(b"z", || false).unwrap();
        assert!(revision.replace(999..1000, || false).is_err());
        assert!(revision.commit(|| false).is_err());
        assert_eq!(bytes(&assembler.snapshot()), b"abcdef");
        assert_eq!(bytes(&accepted), b"abcdef");
        assert_eq!(assembler.origin_count(), 0);
        assert_eq!(assembler.usage(), usage);
        let mut revision = assembler.begin_revision();
        revision.register_origin(4, origin, || false).unwrap();
        drop(revision);
        assert_eq!(assembler.origin_count(), 0);
        let owner = fixture.owners.borrow_mut().pop().unwrap();
        assert!(owner.try_retire().is_ok(), "poison/drop must release staged registry pins");
    }

    #[test]
    fn recovery_replay_prunes_live_sources_and_finishes_with_normal_undo_and_own_sharing() {
        let fixture = Fixture::new();
        let mut assembler = RecoveryAssembler::new(limits()).unwrap();
        let mut revision = assembler.begin_revision();
        revision.register_origin(1, fixture.origin(b"one", Encoding::UTF8, 0), || false).unwrap();
        revision.register_origin(2, fixture.origin(b"two", Encoding::UTF8, 0), || false).unwrap();
        revision.begin_fragment().unwrap();
        revision.append_origin(1, 0..3, || false).unwrap();
        revision.replace(0..0, || false).unwrap();
        revision.commit(|| false).unwrap();
        let old = assembler.snapshot();
        let mut revision = assembler.begin_revision();
        revision.begin_fragment().unwrap();
        revision.append_origin(2, 0..3, || false).unwrap();
        revision.replace(0..3, || false).unwrap();
        revision.begin_fragment().unwrap();
        revision.append_bytes(b"!", || false).unwrap();
        revision.replace(3..3, || false).unwrap();
        revision.commit(|| false).unwrap();
        assert_eq!(assembler.buffer.recovery_live_source_count(), 1);
        assert_eq!(assembler.origin_count(), 2);
        let mut buffer = assembler.finish(|| false).unwrap();
        assert_eq!(buffer.history_depth(), (0, 0));
        assert!(!buffer.undo());
        buffer.insert(4, b"x").unwrap();
        assert!(buffer.add_memory_bytes() > 0);
        assert!(buffer.undo());
        assert_eq!(bytes(&buffer.snapshot()), b"two!");
        assert!(buffer.redo());
        buffer.delete(0..buffer.len()).unwrap();
        assert_eq!(buffer.recovery_live_source_count(), 0);
        assert_eq!(buffer.recovery_own_import_visits(&old), 1);
        buffer.insert_snapshot_range(0, &old, 0..3).unwrap();
        assert!(
            std::sync::Arc::ptr_eq(buffer.snapshot().root.as_ref().unwrap(), old.root.as_ref().unwrap()),
            "own old-generation paste must share the exact root"
        );
        assert_eq!(save(&buffer.snapshot(), Encoding::UTF8, false), b"one");
        assert!(buffer.undo());
        assert!(buffer.is_empty());
        assert!(buffer.redo());
    }

    #[test]
    fn recovery_invalid_records_and_all_work_limits_refuse_without_publication() {
        let fixture = Fixture::new();
        let origin = fixture.origin(b"\x88\x62", Encoding::for_label("big5").unwrap(), 0);
        for bad in [0..5, Range { start: 4, end: 3 }, 1..4, u64::MAX..u64::MAX] {
            let mut assembler = RecoveryAssembler::new(limits()).unwrap();
            let mut revision = assembler.begin_revision();
            revision.register_origin(1, origin.clone(), || false).unwrap();
            revision.begin_fragment().unwrap();
            assert!(revision.append_origin(1, bad, || false).is_err());
            assert!(revision.commit(|| false).is_err());
            assert_eq!(assembler.origin_count(), 0);
            assert!(assembler.snapshot().is_empty());
        }
        let mut assembler = RecoveryAssembler::new(limits()).unwrap();
        let mut revision = assembler.begin_revision();
        revision.begin_fragment().unwrap();
        assert!(revision.append_origin(99, 0..0, || false).is_err());
        drop(revision);
        let mut revision = assembler.begin_revision();
        revision.register_origin(1, origin.clone(), || false).unwrap();
        assert!(revision.register_origin(1, origin.clone(), || false).is_err());
        drop(revision);
        assert_eq!(assembler.origin_count(), 0);
        for table in [
            vec![],
            vec![0; 7],
            vec![0; 8],
            [1_u32, 2].into_iter().flat_map(u32::to_le_bytes).collect(),
            [0_u32, 3].into_iter().flat_map(u32::to_le_bytes).collect(),
        ] {
            let mut revision = assembler.begin_revision();
            revision.begin_fragment().unwrap();
            assert!(revision.append_opaque(Encoding::UTF8, b"\xff\xff", &table, 1, || false).is_err());
            drop(revision);
            assert!(assembler.snapshot().is_empty());
        }
        for which in 0..5 {
            let mut config = limits();
            match which {
                0 => config.max_origins = 0,
                1 => config.max_records = 0,
                2 => config.max_pieces = 0,
                3 => config.max_blocks = 0,
                _ => config.max_metadata_bytes = 0,
            }
            let constructed = RecoveryAssembler::new(config);
            if which == 4 {
                assert!(constructed.is_err());
                continue;
            }
            let mut bounded = constructed.unwrap();
            let mut revision = bounded.begin_revision();
            let result = if which == 0 {
                revision.register_origin(1, origin.clone(), || false)
            } else {
                revision.begin_fragment().and_then(|()| revision.append_bytes(b"x", || false))
            };
            assert!(result.is_err(), "limit {which}");
            assert!(revision.commit(|| false).is_err());
            assert!(bounded.snapshot().is_empty());
            assert_eq!(bounded.origin_count(), 0);
        }
        let mut overflow = limits();
        overflow.max_origins = usize::MAX;
        assert!(RecoveryAssembler::new(overflow).is_err());
        for (raw, table, count) in [
            (vec![0xff; 3], [0_u32, 2, 1, 3].into_iter().flat_map(u32::to_le_bytes).collect::<Vec<_>>(), 3),
            (vec![0xff; 65537], vec![0; 8], 1),
            (vec![0xff], vec![0; 8], u64::MAX),
            (vec![0xff], vec![0; 65540], 16384),
        ] {
            let mut revision = assembler.begin_revision();
            revision.begin_fragment().unwrap();
            assert!(revision.append_opaque(Encoding::UTF8, &raw, &table, count, || false).is_err());
            drop(revision);
            assert!(assembler.snapshot().is_empty());
        }
    }

    #[test]
    fn recovery_fragment_protocol_and_stream_blocks_are_bounded_and_lossless() {
        let mut assembler = RecoveryAssembler::new(limits()).unwrap();
        let mut revision = assembler.begin_revision();
        revision.begin_fragment().unwrap();
        revision.append_bytes(b"accepted?", || false).unwrap();
        assert!(revision.begin_fragment().is_err());
        assert!(revision.commit(|| false).is_err());
        assert!(assembler.snapshot().is_empty());
        let mut revision = assembler.begin_revision();
        revision.begin_fragment().unwrap();
        assert!(revision.commit(|| false).is_err());
        let mut revision = assembler.begin_revision();
        revision.begin_fragment().unwrap();
        assert!(revision.append_bytes(&vec![b'x'; 65537], || false).is_err());
        drop(revision);
        let mut revision = assembler.begin_revision();
        revision.begin_fragment().unwrap();
        revision.append_bytes(b"\xc3", || false).unwrap();
        revision.append_bytes(b"\xa9\xff\r\n", || false).unwrap();
        revision.replace(0..0, || false).unwrap();
        revision.commit(|| false).unwrap();
        assert_eq!(bytes(&assembler.snapshot()), b"\xc3\xa9\xff\r\n");
        let mut config = limits();
        config.max_records = 3;
        let mut tiny = RecoveryAssembler::new(config).unwrap();
        let mut revision = tiny.begin_revision();
        revision.begin_fragment().unwrap();
        revision.append_bytes(b"", || false).unwrap();
        revision.append_bytes(b"", || false).unwrap();
        assert!(revision.append_bytes(b"", || false).is_err());
        assert!(revision.commit(|| false).is_err());
        assert!(tiny.snapshot().is_empty());
    }

    #[test]
    fn recovery_index_metadata_is_admitted_before_scan_and_small_records_hit_work_cap() {
        let fixture = Fixture::new();
        let origin = fixture.origin(&vec![b'\n'; 128 << 10], Encoding::UTF8, 0);
        let baseline = RecoveryAssembler::new(limits()).unwrap().usage().metadata_bytes;
        let mut config = limits();
        config.max_metadata_bytes = baseline;
        let mut assembler = RecoveryAssembler::new(config).unwrap();
        let mut revision = assembler.begin_revision();
        let mut polls = 0;
        let error = revision
            .register_origin(1, origin, || {
                polls += 1;
                false
            })
            .unwrap_err();
        assert!(error.to_string().contains("metadata"));
        assert_eq!(polls, 1, "metadata exhaustion must refuse before constructing/scanning the index");
        drop(revision);
        let owner = fixture.owners.borrow_mut().pop().unwrap();
        assert!(owner.try_retire().is_ok());
        let mut config = limits();
        config.add_memory_bytes = 0;
        config.max_blocks = 2;
        let mut assembler = RecoveryAssembler::new(config).unwrap();
        let mut revision = assembler.begin_revision();
        revision.begin_fragment().unwrap();
        revision.append_bytes(b"a", || false).unwrap();
        revision.append_bytes(b"b", || false).unwrap();
        revision.replace(0..0, || false).unwrap();
        revision.commit(|| false).unwrap();
        assert_eq!(assembler.snapshot().storage_resources().spilled_blocks, 2);
        let usage = assembler.usage();
        let mut revision = assembler.begin_revision();
        revision.begin_fragment().unwrap();
        assert!(revision.append_bytes(b"c", || false).unwrap_err().to_string().contains("blocks"));
        assert!(revision.commit(|| false).is_err());
        assert_eq!(bytes(&assembler.snapshot()), b"ab");
        assert_eq!(assembler.usage(), usage);
        assert_eq!(assembler.snapshot().storage_resources().spilled_blocks, 2);
    }

    #[test]
    fn recovery_cancellation_empty_identity_commit_and_finish_never_publish_fake_state() {
        let fixture = Fixture::new();
        let empty = fixture.origin(b"\xff\xfe", Encoding::UTF16_LE, 2);
        let large = fixture.origin(&vec![b'x'; 128 << 10], Encoding::UTF8, 0);
        let mut assembler = RecoveryAssembler::new(limits()).unwrap();
        let mut polls = 0;
        let mut revision = assembler.begin_revision();
        let error = revision
            .register_origin(8, large, || {
                polls += 1;
                polls == 4
            })
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        drop(revision);
        assert_eq!(assembler.origin_count(), 0);
        let large_owner = fixture.owners.borrow_mut().pop().unwrap();
        assert!(large_owner.try_retire().is_ok());
        let mut revision = assembler.begin_revision();
        revision.register_origin(7, empty.clone(), || false).unwrap();
        revision.begin_fragment().unwrap();
        revision.append_origin(7, 0..0, || false).unwrap();
        revision.replace(0..0, || false).unwrap();
        revision.commit(|| false).unwrap();
        assert_eq!(assembler.origin_count(), 1);
        assert!(assembler.snapshot().is_empty());
        literal(&mut assembler, 0..0, b"safe");
        let accepted = assembler.snapshot();
        for stop in [1, 3] {
            let mut revision = assembler.begin_revision();
            revision.register_origin(9, empty.clone(), || false).unwrap();
            revision.begin_fragment().unwrap();
            revision.append_bytes(b"unsafe", || false).unwrap();
            revision.replace(0..4, || false).unwrap();
            let mut polls = 0;
            assert_eq!(
                revision
                    .commit(|| {
                        polls += 1;
                        polls == stop
                    })
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::Interrupted
            );
            assert_eq!(assembler.origin_count(), 1);
            assert_eq!(bytes(&assembler.snapshot()), b"safe");
            assert_eq!(bytes(&accepted), b"safe");
        }
        let mut polls = 0;
        assert!(
            matches!(assembler.finish(|| { polls += 1; polls == 2 }), Err(error) if error.kind() == io::ErrorKind::Interrupted)
        );
        drop(empty);
    }
    fn opaque_bytes(snapshot: &Snapshot) -> Vec<u8> {
        let mut raw = Vec::new();
        for chunk in snapshot.source_chunks(0..snapshot.len()).unwrap() {
            match chunk.unwrap() {
                SourceChunk::Opaque { encoding, bytes, raw_bytes, raw_offsets } => {
                    assert_eq!(encoding, Encoding::UTF8);
                    assert_eq!(bytes.len(), raw_bytes.len(), "one opaque atom per source byte");
                    assert_eq!(raw_offsets.len(), (bytes.len() + 1) * 4);
                    for adjacent in raw_offsets.as_chunks::<4>().0.windows(2) {
                        assert_eq!(u32::from_le_bytes(adjacent[1]) - u32::from_le_bytes(adjacent[0]), 1);
                    }
                    raw.extend_from_slice(raw_bytes);
                }
                SourceChunk::Added(_) => {}
                _ => panic!("promotion must produce detached text/opaque blocks"),
            }
        }
        raw
    }

    #[test]
    fn recovery_raw_v1_malformed_bytes_bom_and_zero_add_cap_are_exact() {
        for raw in [b"\xff".as_slice(), b"\xc2\xc2", b"\xf0\x90", b"\xef\xbb\xbftext"] {
            let mut config = limits();
            config.add_memory_bytes = 0;
            let mut assembler = RecoveryAssembler::new(config).unwrap();
            literal(&mut assembler, 0..0, raw);
            let before = assembler.snapshot();
            let mut revision = assembler.begin_revision();
            revision.promote_raw_v1(|| false).unwrap();
            assert_eq!(bytes(&revision.assembler.snapshot()), raw, "uncommitted source stays published");
            let staged = Buffer::recovery_snapshot(revision.root.clone());
            assert_eq!(staged.len(), raw.len() as u64);
            assert_eq!(save(&staged, Encoding::UTF8, false), raw);
            if raw == b"\xef\xbb\xbftext" {
                assert!(opaque_bytes(&staged).is_empty());
                assert_eq!(bytes(&staged), raw, "BOM is ordinary data");
            } else {
                assert_eq!(opaque_bytes(&staged), raw);
            }
            revision.commit(|| false).unwrap();
            assert_eq!(bytes(&before), raw);
            let mut buffer = assembler.finish(|| false).unwrap();
            assert_eq!(buffer.add_memory_bytes(), 0);
            assert_eq!(buffer.history_depth(), (0, 0));
            buffer.insert(buffer.len(), b"!").unwrap();
            assert!(buffer.undo());
            assert_eq!(save(&buffer.snapshot(), Encoding::UTF8, false), raw);
            assert!(buffer.redo());
            let mut edited = raw.to_vec();
            edited.push(b'!');
            assert_eq!(save(&buffer.snapshot(), Encoding::UTF8, false), edited);
        }
    }

    #[test]
    fn recovery_raw_v1_alternating_atoms_pack_before_block_creation() {
        let raw = [b'A', 0xff].repeat(8192);
        let mut config = limits();
        config.add_memory_bytes = 0;
        let mut assembler = RecoveryAssembler::new(config).unwrap();
        literal(&mut assembler, 0..0, &raw);
        let before = assembler.usage();
        let mut revision = assembler.begin_revision();
        revision.promote_raw_v1(|| false).unwrap();
        revision.commit(|| false).unwrap();
        let promoted = assembler.snapshot();
        assert_eq!(promoted.len(), raw.len() as u64);
        assert_eq!(save(&promoted, Encoding::UTF8, false), raw);
        assert_eq!(opaque_bytes(&promoted), vec![0xff; 8192]);
        assert!(promoted.storage_resources().blocks <= 6, "bounded mixed packing");
        assert_eq!(promoted.storage_resources().spilled_blocks, promoted.storage_resources().blocks);
        assert!(assembler.usage().blocks - before.blocks <= 6);
    }

    #[test]
    fn recovery_raw_v1_final_prefix_can_split_and_join_utf8_before_promotion() {
        let mut assembler = RecoveryAssembler::new(limits()).unwrap();
        literal(&mut assembler, 0..0, b"\xe2\xac"); // Historical incomplete scalar.
        literal(&mut assembler, 1..1, b"\x82"); // Join through another Added piece.
        literal(&mut assembler, 3..3, b"\xc2\xa2");
        literal(&mut assembler, 4..5, b""); // Split the final scalar through a raw edit.
        let mut revision = assembler.begin_revision();
        revision.promote_raw_v1(|| false).unwrap();
        revision.commit(|| false).unwrap();
        assert_eq!(bytes(&assembler.snapshot()), b"\xe2\x82\xac\xff");
        assert_eq!(opaque_bytes(&assembler.snapshot()), b"\xc2");
        assert_eq!(save(&assembler.snapshot(), Encoding::UTF8, false), b"\xe2\x82\xac\xc2");
    }

    #[test]
    fn recovery_raw_v1_utf8_survives_all_streamblock_scalar_splits() {
        for scalar in ["¢", "€", "𐀀"] {
            for tail in 1..scalar.len() {
                let mut raw = vec![b'a'; piece_tree::ADD_BLOCK - tail];
                raw.extend_from_slice(scalar.as_bytes());
                raw.extend_from_slice(b"z");
                let mut assembler = RecoveryAssembler::new(limits()).unwrap();
                let mut revision = assembler.begin_revision();
                revision.begin_fragment().unwrap();
                for chunk in raw.chunks(piece_tree::ADD_BLOCK) {
                    revision.append_bytes(chunk, || false).unwrap();
                }
                revision.replace(0..0, || false).unwrap();
                revision.promote_raw_v1(|| false).unwrap();
                revision.commit(|| false).unwrap();
                assert_eq!(bytes(&assembler.snapshot()), raw, "{scalar}, tail {tail}");
                assert_eq!(save(&assembler.snapshot(), Encoding::UTF8, false), raw);
                assert!(opaque_bytes(&assembler.snapshot()).is_empty());
            }
        }
    }

    #[test]
    fn recovery_raw_v1_caps_and_cancellation_poison_without_publication() {
        let raw = [b'A', 0xff].repeat(8192);
        for cap in ["records", "pieces", "blocks", "metadata"] {
            let mut config = limits();
            config.add_memory_bytes = 0;
            let mut assembler = RecoveryAssembler::new(config).unwrap();
            literal(&mut assembler, 0..0, &raw);
            let accepted = assembler.usage();
            match cap {
                "records" => assembler.limits.max_records = accepted.records,
                "pieces" => assembler.limits.max_pieces = accepted.pieces,
                "blocks" => assembler.limits.max_blocks = accepted.blocks + 2,
                "metadata" => assembler.limits.max_metadata_bytes = accepted.metadata_bytes,
                _ => unreachable!(),
            }
            let mut revision = assembler.begin_revision();
            let error = revision.promote_raw_v1(|| false).unwrap_err();
            assert!(error.to_string().contains(cap), "{cap}: {error}");
            assert!(revision.commit(|| false).is_err());
            assert_eq!(assembler.usage(), accepted);
            assert_eq!(bytes(&assembler.snapshot()), raw);
        }
        let mut assembler = RecoveryAssembler::new(limits()).unwrap();
        literal(&mut assembler, 0..0, &raw);
        let accepted = assembler.usage();
        let mut polls = 0;
        {
            let mut revision = assembler.begin_revision();
            revision
                .promote_raw_v1(|| {
                    polls += 1;
                    false
                })
                .unwrap();
            // Drop deliberately: the complete promoted target is still staged.
        }
        for stop in [1, polls / 2, polls] {
            let mut seen = 0;
            let mut revision = assembler.begin_revision();
            assert_eq!(
                revision
                    .promote_raw_v1(|| {
                        seen += 1;
                        seen == stop
                    })
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::Interrupted
            );
            assert!(revision.commit(|| false).is_err());
            assert_eq!(assembler.usage(), accepted);
            assert_eq!(bytes(&assembler.snapshot()), raw);
        }
        let mut revision = assembler.begin_revision();
        revision.promote_raw_v1(|| false).unwrap();
        assert_eq!(revision.commit(|| true).unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert_eq!(assembler.usage(), accepted);
        assert_eq!(bytes(&assembler.snapshot()), raw);
        let mut revision = assembler.begin_revision();
        revision.begin_fragment().unwrap();
        revision.append_bytes(b"pending", || false).unwrap();
        assert!(revision.promote_raw_v1(|| false).is_err());
        assert!(revision.commit(|| false).is_err());
        assert_eq!(assembler.usage(), accepted);
        assert_eq!(bytes(&assembler.snapshot()), raw);
    }

    #[test]
    fn recovery_raw_v1_rejects_typed_origins_and_rolls_back_pending_ids() {
        let fixture = Fixture::new();
        let empty = fixture.origin(b"\xef\xbb\xbf", Encoding::UTF8, 3);
        let mut assembler = RecoveryAssembler::new(limits()).unwrap();
        literal(&mut assembler, 0..0, b"\xff");
        let accepted = assembler.usage();
        {
            let mut revision = assembler.begin_revision();
            revision.register_origin(17, empty.clone(), || false).unwrap();
            assert!(revision.promote_raw_v1(|| false).is_err(), "even empty decoded identities reject");
            assert_eq!(revision.assembler.origin_count(), 0, "failure removes pending IDs immediately");
            assert!(revision.commit(|| false).is_err());
        }
        assert_eq!(assembler.usage(), accepted);
        assert_eq!(bytes(&assembler.snapshot()), b"\xff");
        {
            let mut revision = assembler.begin_revision();
            revision.register_origin(17, empty.clone(), || false).unwrap();
            revision.commit(|| false).unwrap(); // Rolled-back numeric ID is reusable.
        }
        drop(assembler);
        drop(empty);

        let mut assembler = RecoveryAssembler::new(limits()).unwrap();
        let mut revision = assembler.begin_revision();
        revision.begin_fragment().unwrap();
        revision.append_opaque(Encoding::UTF8, b"\xff", &[0, 0, 0, 0, 1, 0, 0, 0], 1, || false).unwrap();
        revision.replace(0..0, || false).unwrap();
        revision.commit(|| false).unwrap();
        let accepted = assembler.usage();
        let mut revision = assembler.begin_revision();
        assert!(revision.promote_raw_v1(|| false).is_err(), "typed opaque storage is never flattened");
        assert!(revision.commit(|| false).is_err());
        assert_eq!(assembler.usage(), accepted);
        assert_eq!(save(&assembler.snapshot(), Encoding::UTF8, false), b"\xff");
    }
}
