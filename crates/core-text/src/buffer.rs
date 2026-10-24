//! Byte-exact editing over immutable, shared pieces. Offsets, columns and
//! line numbers are zero-based; columns count bytes, not Unicode scalars.

use crate::piece_tree::{self, AddBudget, Chunks, OriginalUses, RetainedStorage, Tree};
use crate::undo::History;
use crate::{ReadError, TextFile};
use std::io::{self, Write};
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

const ADD_MEMORY: usize = 8 << 20;
const UNDO_MEMORY: usize = 16 << 20;
const TYPING_GAP: Duration = Duration::from_millis(500);

/// O(1) immutable version, safe to send to search/highlight/save workers.
/// Holding a snapshot pins its referenced storage until the worker drops it.
/// Original-file reads follow TextFile's change checks and may return ReadError.
#[derive(Clone, Default)]
pub struct Snapshot {
    pub(crate) root: Tree,
    pub(crate) originals: Arc<OriginalUses>,
    retained_storage: RetainedStorage,
}

/// Send this handle and an immutable source snapshot to an import worker.
/// Foreign raw/decoded originals and opaque provenance are copied in bounded
/// blocks under this document's resident/spill cap. Private spill creation and
/// last-drop file removal must run off the UI thread, as for all add blocks.
#[derive(Clone)]
pub struct SnapshotImporter {
    added: Arc<AddBudget>,
    owner: Arc<()>,
}

impl SnapshotImporter {
    pub fn prepare(&self, source: &Snapshot, range: Range<u64>) -> io::Result<Snapshot> {
        source.check_range(&range)?;
        piece_tree::recovery_cut(&source.root, range.start, &mut || Ok(()))?;
        piece_tree::recovery_cut(&source.root, range.end, &mut || Ok(()))?;
        let full = range.start == 0 && range.end == source.len();
        let selected = if full {
            source.root.clone()
        } else {
            let (_, tail) = piece_tree::split(source.root.clone(), range.start).map_err(io::Error::other)?;
            piece_tree::split(tail, range.end - range.start).map_err(io::Error::other)?.0
        };
        let root = piece_tree::import_added(selected, &self.added, &self.owner)?;
        let shared = match (&root, &source.root) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        let originals = if full && shared {
            source.originals.clone()
        } else {
            Arc::new(piece_tree::original_uses(&root, 0..range.end - range.start))
        };
        let retained_storage =
            if full && shared { source.retained_storage.clone() } else { RetainedStorage::from_tree(&root) };
        Ok(Snapshot { root, originals, retained_storage })
    }
}

impl Snapshot {
    /// Worker-only, before publishing this recovered snapshot to a frontend.
    /// The non-Send lease keeps physical spill cleanup on its owning worker.
    ///
    /// ```compile_fail
    /// use kwil_core_text::{Snapshot, SpillRetirementLimits};
    /// let lease = Snapshot::default().retain_spills(SpillRetirementLimits {
    ///     max_block_references: 0, max_metadata_bytes: 4096, max_path_bytes: 4096,
    /// }, || false).unwrap();
    /// std::thread::spawn(move || drop(lease));
    /// ```
    pub fn retain_spills(
        &self,
        limits: crate::SpillRetirementLimits,
        cancelled: impl FnMut() -> bool,
    ) -> io::Result<crate::SpillRetirement> {
        crate::SpillRetirement::collect(&self.root, limits, cancelled)
    }

    pub fn storage_resources(&self) -> crate::StorageResources {
        self.root.as_ref().map_or(crate::StorageResources::default(), |node| node.metrics.resources)
    }

    pub fn len(&self) -> u64 {
        self.root.as_ref().map_or(0, |node| node.metrics.bytes)
    }

    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    /// Newline count plus one, including an empty final line.
    pub fn line_count(&self) -> u64 {
        self.root.as_ref().map_or(1, |node| node.metrics.breaks + 1)
    }

    pub fn line_start(&self, line: u64) -> Result<Option<u64>, ReadError> {
        if line >= self.line_count() {
            Ok(None)
        } else if line == 0 {
            Ok(Some(0))
        } else {
            Ok(Some(piece_tree::after_break(self.root.as_ref().unwrap(), line)?))
        }
    }

    /// EOF is a valid position. A CR in CRLF occupies one byte column.
    pub fn offset_to_line_col(&self, offset: u64) -> Result<Option<(u64, u64)>, ReadError> {
        if offset > self.len() {
            return Ok(None);
        }
        let line = match &self.root {
            Some(node) => piece_tree::rank(node, offset)?,
            None => 0,
        };
        Ok(Some((line, offset - self.line_start(line)?.unwrap())))
    }

    /// Borrowed slices clipped to `range`; invalid/reversed ranges are errors.
    /// Empty ranges yield no chunks. No file-sized output allocation occurs.
    /// Each iterator item reports original-file read errors and then stops.
    pub fn chunks(&self, range: Range<u64>) -> io::Result<Chunks<'_>> {
        self.check_range(&range)?;
        Ok(Chunks::new(&self.root, range))
    }

    /// Encoding-save traversal retaining original byte ranges. Does not decode or copy.
    pub fn source_chunks(&self, range: Range<u64>) -> io::Result<crate::piece_tree::SourceChunks<'_>> {
        self.check_range(&range)?;
        Ok(crate::piece_tree::SourceChunks::new(&self.root, range))
    }

    /// Borrow each viewport line as chunks, without LF or a trailing CR.
    /// Even a single giant line is never joined into an owned buffer.
    /// Both line and chunk iterator items can report ReadError.
    pub fn lines(&self, start_line: u64, count: usize) -> Lines<'_> {
        Lines { snapshot: self, next: start_line, remaining: count }
    }

    /// Stream this version to a writer. Atomic replacement is the save layer's job.
    pub fn write_to(&self, writer: &mut impl Write) -> io::Result<()> {
        for chunk in Chunks::new(&self.root, 0..self.len()) {
            writer.write_all(chunk.map_err(io::Error::other)?)?;
        }
        Ok(())
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        self.root
            .as_ref()
            .map_or(0, |node| node.metrics.retained)
            .saturating_add(self.retained_storage.bytes())
            .saturating_add(
                self.originals.values().fold(0_usize, |cost, (source, _)| {
                    cost.saturating_add(source.retained_bytes()).saturating_add(512)
                }),
            )
    }

    /// Compose immutable ranges without importing storage. The final mutable
    /// destination enforces its own resident/spill cap when inserting them.
    pub fn slice(&self, range: Range<u64>) -> io::Result<Self> {
        self.check_range(&range)?;
        if range.start == 0 && range.end == self.len() {
            return Ok(self.clone());
        }
        let (_, tail) = piece_tree::split(self.root.clone(), range.start).map_err(io::Error::other)?;
        let (root, _) = piece_tree::split(tail, range.end - range.start).map_err(io::Error::other)?;
        let originals = Arc::new(piece_tree::original_uses(&root, 0..range.end - range.start));
        let retained_storage = RetainedStorage::from_tree(&root);
        Ok(Self { root, originals, retained_storage })
    }

    pub fn concat(&self, other: &Self) -> io::Result<Self> {
        self.len()
            .checked_add(other.len())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "buffer length overflow"))?;
        let mut originals = self.originals.as_ref().clone();
        for (&id, (source, bytes)) in other.originals.iter() {
            originals.entry(id).or_insert((source.clone(), 0)).1 += bytes;
        }
        let mut retained_storage = self.retained_storage.clone();
        retained_storage.extend(&other.retained_storage);
        Ok(Self {
            root: piece_tree::join(self.root.clone(), other.root.clone()),
            originals: Arc::new(originals),
            retained_storage,
        })
    }

    fn check_range(&self, range: &Range<u64>) -> io::Result<()> {
        if range.start > range.end || range.end > self.len() {
            Err(io::Error::new(io::ErrorKind::InvalidInput, "byte range outside buffer"))
        } else {
            Ok(())
        }
    }

    fn byte_at(&self, offset: u64) -> Result<u8, ReadError> {
        Ok(Chunks::new(&self.root, offset..offset + 1).next().unwrap()?[0])
    }
}

/// An iterator of line iterators; allocating scratch space is O(tree height).
pub struct Lines<'a> {
    snapshot: &'a Snapshot,
    next: u64,
    remaining: usize,
}

impl<'a> Lines<'a> {
    fn next_line(&mut self) -> Result<Option<Chunks<'a>>, ReadError> {
        let Some(start) = self.snapshot.line_start(self.next)? else { return Ok(None) };
        self.next += 1;
        self.remaining -= 1;
        let mut end = self.snapshot.line_start(self.next)?.map_or(self.snapshot.len(), |at| at - 1);
        if end > start && self.snapshot.byte_at(end - 1)? == b'\r' {
            end -= 1;
        }
        Ok(Some(Chunks::new(&self.snapshot.root, start..end)))
    }
}

impl<'a> Iterator for Lines<'a> {
    type Item = Result<Chunks<'a>, ReadError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        match self.next_line() {
            Ok(Some(line)) => Some(Ok(line)),
            Ok(None) => {
                self.remaining = 0;
                None
            }
            Err(error) => {
                self.remaining = 0;
                Some(Err(error))
            }
        }
    }
}

pub(crate) struct Typing {
    next_offset: u64,
    at: Duration,
}

/// Mutable editor handle with a persistent AVL tree and capped linear undo.
/// Each edit path-copies O(log pieces) nodes; inserted bytes cost O(input size).
/// Resident add bytes default to 8 MiB, shared with snapshots and undo; excess
/// blocks spill to private immutable mapped temp files. Disk spill may block:
/// bulk insertion belongs on a worker. Small typing edits stay in memory.
pub struct Buffer {
    pub(crate) snapshot: Snapshot,
    pub(crate) added: Arc<AddBudget>,
    pub(crate) history: History,
    pub(crate) transaction: Option<Snapshot>,
    pub(crate) typing: Option<Typing>,
    pub(crate) worker_epoch: u64,
    // Stable document identity, carried by original pieces across checkpoints.
    // No file list, byte ownership or weak backing retention is stored here.
    pub(crate) owner: Arc<()>,
}

impl Default for Buffer {
    fn default() -> Self {
        Self::empty()
    }
}

impl Buffer {
    /// Existing non-Result mutators fail closed by panicking BEFORE any state
    /// change on exhaustion. Result mutators propagate InvalidInput instead.
    /// Even empty/no-op transaction boundaries invalidate outstanding seeds.
    fn advance_worker_epoch(&mut self) -> io::Result<()> {
        let next = self.worker_epoch.checked_add(1).ok_or_else(crate::worker_edit::overflow)?;
        // Every mutation inside a compound leaves one terminal transition.
        // Refusal happens before changing text, typing, history or epoch.
        if self.transaction.is_some() {
            next.checked_add(1).ok_or_else(crate::worker_edit::overflow)?;
        }
        self.worker_epoch = next;
        Ok(())
    }
    fn terminal_worker_epoch(&mut self) {
        self.worker_epoch =
            self.worker_epoch.checked_add(1).expect("core mutation epoch exhausted before terminal mutation");
    }
    pub fn empty() -> Self {
        Self::with_add_memory_limit(ADD_MEMORY)
    }

    /// Choose the resident add-byte cap before any edits (zero always spills).
    pub fn with_add_memory_limit(bytes: usize) -> Self {
        let owner = Arc::new(());
        Self {
            snapshot: Snapshot::default(),
            added: AddBudget::new(bytes, owner.clone()),
            history: History::new(UNDO_MEMORY),
            transaction: None,
            typing: None,
            worker_epoch: 0,
            owner,
        }
    }

    /// Does not copy file bytes. O(1) when the file's sparse index is ready.
    /// Otherwise builds that index on the calling thread with four workers;
    /// call on a worker, or await TextFile's background index before attaching.
    /// Inherits TextFile's external-file-mutation safety contract.
    /// Refuses changed files even when an index is already cached.
    pub fn from_file(file: TextFile) -> Result<Self, ReadError> {
        let mut buffer = Self::empty();
        let root = piece_tree::original(file.clone(), buffer.owner.clone())?;
        let originals = Arc::new(piece_tree::original_uses(&root, 0..file.len()));
        let retained_storage = RetainedStorage::from_tree(&root);
        buffer.snapshot = Snapshot { root, originals, retained_storage };
        Ok(buffer)
    }

    /// Attach an immutable worker-prepared decoded generation. Builds sparse
    /// logical UTF-8 line metrics on the calling worker, without copying text.
    /// Keep its DecodedBacking owner on the worker retirement queue.
    pub fn from_decoded(origin: crate::decoded_backing::DecodedSnapshot) -> Self {
        Self::from_decoded_checked(origin, &mut || Ok::<(), std::convert::Infallible>(()))
            .unwrap_or_else(|never| match never {})
    }

    /// Worker-only cancellable logical index construction. Cancellation is
    /// checked between <=64KiB scans, including empty origins. Interrupted
    /// returns no buffer and releases the temporary origin pin; its backing
    /// owner still belongs to the caller's explicit worker retirement queue.
    pub fn from_decoded_cancellable(
        origin: crate::decoded_backing::DecodedSnapshot,
        mut cancelled: impl FnMut() -> bool,
    ) -> io::Result<Self> {
        Self::from_decoded_checked(origin, &mut || {
            if cancelled() { Err(io::Error::from(io::ErrorKind::Interrupted)) } else { Ok(()) }
        })
    }

    fn from_decoded_checked<E>(
        origin: crate::decoded_backing::DecodedSnapshot,
        check: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<Self, E> {
        let mut buffer = Self::empty();
        let root = piece_tree::decoded_checked(origin, buffer.owner.clone(), check)?;
        let len = root.as_ref().map_or(0, |node| node.metrics.bytes);
        let originals = Arc::new(piece_tree::original_uses(&root, 0..len));
        let retained_storage = RetainedStorage::from_tree(&root);
        buffer.snapshot = Snapshot { root, originals, retained_storage };
        Ok(buffer)
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.clone()
    }

    // Explicit trusted worker construction only. These seams do not relax the
    // complete-saved-original guards on public checkpoint/worker-edit APIs.
    pub(crate) fn recovery_origin(
        &self,
        origin: crate::decoded_backing::DecodedSnapshot,
        check: &mut impl FnMut() -> io::Result<()>,
    ) -> io::Result<Snapshot> {
        let root = piece_tree::decoded_checked(origin, self.owner.clone(), check)?;
        let len = root.as_ref().map_or(0, |node| node.metrics.bytes);
        let retained_storage = RetainedStorage::from_tree(&root);
        Ok(Snapshot { originals: Arc::new(piece_tree::original_uses(&root, 0..len)), root, retained_storage })
    }

    pub(crate) fn recovery_bytes(&self, bytes: &[u8]) -> io::Result<Tree> {
        piece_tree::added(bytes, &self.added)
    }

    pub(crate) fn recovery_promote_raw_v1(
        &self,
        root: &Tree,
        admission: &mut dyn FnMut(usize, &Tree) -> io::Result<()>,
        check: &mut impl FnMut() -> io::Result<()>,
    ) -> io::Result<Tree> {
        piece_tree::recovery_promote_raw_v1(root, &self.added, admission, check)
    }

    pub(crate) fn recovery_opaque(
        &self,
        encoding: crate::encoding::Encoding,
        raw: &[u8],
        offsets: &[u8],
        count: u64,
    ) -> io::Result<Tree> {
        piece_tree::recovery_opaque(encoding, raw, offsets, count, &self.added)
    }

    pub(crate) fn recovery_preserved(
        &self,
        encoding: crate::encoding::Encoding,
        bytes: &[u8],
        raw: &[u8],
        offsets: &[u8],
        check: &mut impl FnMut() -> io::Result<()>,
    ) -> io::Result<Tree> {
        piece_tree::recovery_preserved(encoding, bytes, raw, offsets, &self.added, check)
    }

    pub(crate) fn recovery_snapshot(root: Tree) -> Snapshot {
        let len = root.as_ref().map_or(0, |node| node.metrics.bytes);
        let retained_storage = RetainedStorage::from_tree(&root);
        Snapshot { originals: Arc::new(piece_tree::original_uses(&root, 0..len)), root, retained_storage }
    }

    pub(crate) fn recovery_install(&mut self, snapshot: Snapshot) {
        self.advance_worker_epoch().expect("core mutation epoch exhausted before mutation");
        self.snapshot = snapshot;
    }

    #[cfg(test)]
    pub(crate) fn recovery_live_source_count(&self) -> usize {
        self.snapshot.originals.len()
    }

    #[cfg(test)]
    pub(crate) fn recovery_own_import_visits(&self, source: &Snapshot) -> usize {
        piece_tree::import_visit_count(source.root.clone(), &self.added, &self.owner)
    }

    /// Capture before dispatching foreign clipboard/register imports to a
    /// worker. Insert the returned snapshot through insert_snapshot_range;
    /// its already admitted blocks then share in O(1), without spill writes.
    pub fn snapshot_importer(&self) -> SnapshotImporter {
        SnapshotImporter { added: self.added.clone(), owner: self.owner.clone() }
    }

    #[cfg(test)]
    pub(crate) fn import_work(&self) -> crate::piece_tree::ImportWork {
        self.added.import_work()
    }

    pub fn len(&self) -> u64 {
        self.snapshot.len()
    }

    pub fn is_empty(&self) -> bool {
        self.snapshot.is_empty()
    }

    pub fn line_count(&self) -> u64 {
        self.snapshot.line_count()
    }

    pub fn line_start(&self, line: u64) -> Result<Option<u64>, ReadError> {
        self.snapshot.line_start(line)
    }

    pub fn offset_to_line_col(&self, offset: u64) -> Result<Option<(u64, u64)>, ReadError> {
        self.snapshot.offset_to_line_col(offset)
    }

    pub fn chunks(&self, range: Range<u64>) -> io::Result<Chunks<'_>> {
        self.snapshot.chunks(range)
    }

    pub fn lines(&self, start_line: u64, count: usize) -> Lines<'_> {
        self.snapshot.lines(start_line, count)
    }

    pub fn write_to(&self, writer: &mut impl Write) -> io::Result<()> {
        self.snapshot.write_to(writer)
    }

    pub fn insert(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        self.advance_worker_epoch()?;
        self.typing = None;
        self.insert_inner(offset, bytes, false)
    }

    /// Own-document ranges share pieces. Foreign imports can create spill files
    /// and must be prepared on a worker via snapshot_importer first. Last-drop
    /// spill cleanup also belongs on the worker retirement path.
    pub fn insert_snapshot_range(&mut self, offset: u64, source: &Snapshot, range: Range<u64>) -> io::Result<()> {
        self.advance_worker_epoch()?;
        self.snapshot.check_range(&(offset..offset))?;
        source.check_range(&range)?;
        if range.is_empty() {
            return Ok(());
        }
        self.len()
            .checked_add(range.end - range.start)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "buffer length overflow"))?;
        let prepared = self.snapshot_importer().prepare(source, range)?;
        let inserted = prepared.root;
        let uses = prepared.originals;
        let mut retained_storage = self.snapshot.retained_storage.clone();
        retained_storage.extend(&prepared.retained_storage);
        let before = self.snapshot();
        let (left, right) = piece_tree::split(before.root.clone(), offset).map_err(io::Error::other)?;
        self.snapshot.root = piece_tree::join(piece_tree::join(left, inserted), right);
        self.snapshot.retained_storage = retained_storage;
        if !uses.is_empty() {
            let mut originals = self.snapshot.originals.as_ref().clone();
            for (&id, (source, bytes)) in uses.iter() {
                originals.entry(id).or_insert((source.clone(), 0)).1 += bytes;
            }
            self.snapshot.originals = Arc::new(originals);
        }
        self.typing = None;
        self.record(before, false);
        Ok(())
    }

    fn insert_inner(&mut self, offset: u64, bytes: &[u8], coalesce: bool) -> io::Result<()> {
        self.snapshot.check_range(&(offset..offset))?;
        if bytes.is_empty() {
            return Ok(());
        }
        self.len()
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "buffer length overflow"))?;
        // Allocate all new storage before mutating the root: I/O errors are atomic.
        let added = piece_tree::added(bytes, &self.added)?;
        let mut retained_storage = self.snapshot.retained_storage.clone();
        retained_storage.change_range(&added, 0..bytes.len() as u64, false);
        let before = self.snapshot();
        let (left, right) = piece_tree::split(before.root.clone(), offset).map_err(io::Error::other)?;
        self.snapshot.root = piece_tree::join(piece_tree::join(left, added), right);
        self.snapshot.retained_storage = retained_storage;
        self.record(before, coalesce);
        Ok(())
    }

    pub fn delete(&mut self, range: Range<u64>) -> io::Result<()> {
        self.advance_worker_epoch()?;
        self.snapshot.check_range(&range)?;
        self.typing = None;
        if range.is_empty() {
            return Ok(());
        }
        let before = self.snapshot();
        let mut retained_storage = before.retained_storage.clone();
        if range.start == 0 && range.end == before.len() {
            retained_storage = RetainedStorage::default();
        } else {
            retained_storage.change_range(&before.root, range.clone(), true);
        }
        let removed = if range.start == 0 && range.end == before.len() {
            before.originals.as_ref().clone()
        } else {
            piece_tree::original_uses(&before.root, range.clone())
        };
        let (left, tail) = piece_tree::split(before.root.clone(), range.start).map_err(io::Error::other)?;
        let (_, right) = piece_tree::split(tail, range.end - range.start).map_err(io::Error::other)?;
        self.snapshot.root = piece_tree::join(left, right);
        self.snapshot.retained_storage = retained_storage;
        if !removed.is_empty() {
            let mut originals = before.originals.as_ref().clone();
            for (id, (_, bytes)) in removed {
                if let Some((_, remaining)) = originals.get_mut(&id) {
                    *remaining -= bytes;
                    if *remaining == 0 {
                        originals.remove(&id);
                    }
                }
            }
            self.snapshot.originals = Arc::new(originals);
        }
        self.record(before, false);
        Ok(())
    }

    fn record(&mut self, before: Snapshot, coalesce: bool) {
        if self.transaction.is_none() {
            self.history.record(before, self.snapshot(), coalesce);
        }
    }

    /// Begin one explicit batch. Returns false for a nested begin; nested
    /// transactions are not supported. A batch stays one undo step.
    pub fn begin(&mut self) -> bool {
        if self.transaction.is_none() {
            self.worker_epoch.checked_add(2).expect("core mutation epoch has no terminal reservation");
        }
        self.advance_worker_epoch().expect("core mutation epoch exhausted before mutation");
        self.typing = None;
        if self.transaction.is_some() {
            return false;
        }
        self.transaction = Some(self.snapshot());
        true
    }

    /// Commit an open batch. Empty batches create no history entry.
    pub fn commit(&mut self) -> bool {
        self.commit_coalesced(false)
    }

    /// Finish a batch, optionally merging it with the preceding undo step.
    /// The editing model owns burst timing and cursor-continuity checks.
    pub fn commit_coalesced(&mut self, coalesce: bool) -> bool {
        self.terminal_worker_epoch();
        self.commit_inner(coalesce)
    }

    fn commit_inner(&mut self, coalesce: bool) -> bool {
        self.typing = None;
        let Some(before) = self.transaction.take() else { return false };
        let unchanged = match (&before.root, &self.snapshot.root) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        };
        if !unchanged {
            self.history.record(before, self.snapshot(), coalesce);
        }
        true
    }

    /// Roll back an uncommitted batch after a fallible multi-edit operation.
    pub fn cancel(&mut self) -> bool {
        self.terminal_worker_epoch();
        self.typing = None;
        let Some(before) = self.transaction.take() else { return false };
        self.snapshot = before;
        true
    }

    /// Counts let model metadata follow oldest-first history eviction exactly.
    pub fn history_depth(&self) -> (usize, usize) {
        self.history.depth()
    }

    /// Charge frontend-independent cursor/recipe metadata against the same
    /// cap as roots. Replaces the latest charge, then evicts oldest entries.
    pub fn charge_latest_model_history(&mut self, bytes: usize) {
        self.advance_worker_epoch().expect("core mutation epoch exhausted before mutation");
        self.history.charge_latest_model(bytes);
    }

    /// Rebase the current version onto an equivalent, reopened saved original.
    /// Caller must establish byte equivalence and finish any pending batch.
    /// History and already issued snapshots retain their old backing objects.
    pub fn checkpoint_current(&mut self, mut saved: Snapshot) -> io::Result<()> {
        self.advance_worker_epoch()?;
        if self.transaction.is_some() || saved.len() != self.len() || saved.line_count() != self.line_count() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid saved checkpoint"));
        }
        if !piece_tree::saved_original(&saved.root) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "checkpoint requires a complete file-backed saved original",
            ));
        }
        self.typing = None;
        saved.root = piece_tree::own_saved_original(saved.root, &self.owner);
        self.snapshot = saved;
        Ok(())
    }

    /// Install a worker-prepared changed version as one undo step (for example
    /// streamed trim-on-save). No scan/copy occurs on the completion thread.
    pub fn install_worker_edit(&mut self, mut snapshot: Snapshot) -> io::Result<()> {
        self.advance_worker_epoch()?;
        if self.transaction.is_some() {
            return Err(io::Error::other("cannot install worker edit during a transaction"));
        }
        if !piece_tree::saved_original(&snapshot.root) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "worker save edit requires a complete file-backed saved original",
            ));
        }
        snapshot.root = piece_tree::own_saved_original(snapshot.root, &self.owner);
        let before = std::mem::replace(&mut self.snapshot, snapshot);
        self.typing = None;
        self.record(before, false);
        Ok(())
    }

    /// Coalesce contiguous insertions separated by at most 500 ms. The caller
    /// supplies monotonic elapsed time: no timer, polling or idle work is used.
    /// Cursor movement/commands should call `end_typing_burst`.
    pub fn insert_typing(&mut self, offset: u64, bytes: &[u8], at: Duration) -> io::Result<()> {
        self.advance_worker_epoch()?;
        let coalesce = self.transaction.is_none()
            && self.typing.as_ref().is_some_and(|previous| {
                previous.next_offset == offset && at.checked_sub(previous.at).is_some_and(|gap| gap <= TYPING_GAP)
            });
        self.insert_inner(offset, bytes, coalesce)?;
        if !bytes.is_empty() {
            self.typing = Some(Typing { next_offset: offset + bytes.len() as u64, at });
        }
        Ok(())
    }

    pub fn end_typing_burst(&mut self) {
        self.advance_worker_epoch().expect("core mutation epoch exhausted before mutation");
        self.typing = None;
    }

    /// Undo commits any pending batch first.
    pub fn undo(&mut self) -> bool {
        // Finishing a compound plus undo is ONE preflighted authority operation.
        // Do not call public commit and spend a second, unreserved epoch.
        self.terminal_worker_epoch();
        self.commit_inner(false);
        self.typing = None;
        if let Some(snapshot) = self.history.undo() {
            self.snapshot = snapshot;
            true
        } else {
            false
        }
    }

    pub fn redo(&mut self) -> bool {
        self.terminal_worker_epoch();
        self.commit_inner(false);
        self.typing = None;
        if let Some(snapshot) = self.history.redo() {
            self.snapshot = snapshot;
            true
        } else {
            false
        }
    }

    /// Conservative retained-root accounting. Each snapshot charges unique
    /// storage allocations once (including fully pinned tiny slices), while
    /// sharing between history entries may count more than once. Excludes
    /// baseline original storage and OS page cache.
    /// Drops oldest undo entries, then farthest-future redo entries, keeping
    /// a contiguous history anchored at the current state.
    /// Active transactions and externally held snapshots are not undo history.
    pub fn set_undo_memory_limit(&mut self, bytes: usize) {
        self.advance_worker_epoch().expect("core mutation epoch exhausted before mutation");
        self.history.set_limit(bytes);
    }

    pub fn undo_memory_bytes(&self) -> usize {
        self.history.bytes()
    }

    /// Resident added bytes across this buffer, history and worker snapshots.
    pub fn add_memory_bytes(&self) -> usize {
        self.added.used.load(Ordering::Relaxed)
    }

    /// Enable future resident additions exactly once after zero-cap replay.
    /// This never imports/rebases the current snapshot, changes its owner,
    /// records history, or resets shared accounting. Existing importers see the
    /// same monotonic zero-to-positive cap. Zero/repeated/nonzero reconfiguration
    /// is refused; callers retaining an explicitly configured zero skip this.
    pub fn enable_resident_additions(&mut self, limit: usize) -> io::Result<()> {
        self.advance_worker_epoch()?;
        self.added.enable_resident_additions(limit)
    }
}

#[cfg(test)]
mod editing_contract_tests {
    use super::*;

    #[test]
    fn retained_storage_fragmented_replacement_keeps_one_undo_at_default_cap() {
        let mut buffer = Buffer::empty();
        let count = 4107;
        let original = b"a\n".repeat(count);
        buffer.insert(0, &original).unwrap();
        let before = buffer.snapshot();
        buffer.begin();
        for index in (0..count).rev() {
            let offset = (2 * index) as u64;
            buffer.delete(offset..offset + 1).unwrap();
            buffer.insert(offset, b"bc").unwrap();
        }
        buffer.commit();
        buffer.snapshot.retained_storage.assert_matches_tree(&buffer.snapshot.root);
        before.retained_storage.assert_matches_tree(&before.root);
        assert_eq!(buffer.history_depth(), (2, 0), "a small fragmented edit must not evict its own undo");
        assert!(buffer.undo_memory_bytes() < UNDO_MEMORY);
        assert!(buffer.undo());
        let mut bytes = Vec::new();
        buffer.write_to(&mut bytes).unwrap();
        assert_eq!(bytes, original);
        buffer.snapshot.retained_storage.assert_matches_tree(&buffer.snapshot.root);
        assert!(buffer.redo());
        bytes.clear();
        buffer.write_to(&mut bytes).unwrap();
        assert_eq!(bytes, b"bc\n".repeat(count));
    }

    #[test]
    fn retained_storage_tiny_slice_pins_full_allocation_and_concat_counts_it_once() {
        let mut buffer = Buffer::empty();
        let original = vec![b'x'; 64 << 10];
        buffer.insert(0, &original).unwrap();
        let source = buffer.snapshot();
        let tiny = source.slice(17..18).unwrap();
        assert_eq!(
            tiny.retained_storage.bytes(),
            source.retained_storage.bytes(),
            "one byte pins the full added allocation"
        );
        assert!(tiny.retained_bytes() >= original.len());
        let repeated = tiny.concat(&tiny).unwrap().concat(&tiny).unwrap();
        assert_eq!(repeated.retained_storage.bytes(), tiny.retained_storage.bytes());
        repeated.retained_storage.assert_matches_tree(&repeated.root);
        let mut target = Buffer::empty();
        let prepared = target.snapshot_importer().prepare(&repeated, 0..3).unwrap();
        target.insert_snapshot_range(0, &prepared, 0..3).unwrap();
        target.snapshot.retained_storage.assert_matches_tree(&target.snapshot.root);
        target.begin();
        target.delete(0..2).unwrap();
        target.snapshot.retained_storage.assert_matches_tree(&target.snapshot.root);
        target.cancel();
        target.snapshot.retained_storage.assert_matches_tree(&target.snapshot.root);
        target.set_undo_memory_limit(0);
        target.delete(0..target.len()).unwrap();
        assert_eq!(target.snapshot.retained_storage.bytes(), 0);
        // Immutable earlier versions retain their own valid directory/root.
        source.retained_storage.assert_matches_tree(&source.root);
        repeated.retained_storage.assert_matches_tree(&repeated.root);
    }

    #[test]
    fn retained_storage_tracks_mixed_range_edits_without_changing_snapshots() {
        let mut buffer = Buffer::empty();
        buffer.set_undo_memory_limit(0);
        let mut saved = Vec::new();
        let mut seed = 0x51ce_2026_u64;
        for step in 0..320 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let at = (seed >> 32) % (buffer.len() + 1);
            if step % 4 == 0 && buffer.len() > at {
                let end = (at + 1 + (seed % 9)).min(buffer.len());
                buffer.delete(at..end).unwrap();
            } else {
                buffer.insert(at, &[step as u8; 3]).unwrap();
            }
            buffer.snapshot.retained_storage.assert_matches_tree(&buffer.snapshot.root);
            if step % 41 == 0 {
                saved.push(buffer.snapshot());
            }
        }
        for snapshot in saved {
            snapshot.retained_storage.assert_matches_tree(&snapshot.root);
        }
    }

    #[test]
    fn retained_storage_opaque_slice_keeps_all_physical_payloads_once() {
        let buffer = Buffer::empty();
        let raw = vec![0xff; 128];
        let offsets: Vec<u8> = (0_u32..=128).flat_map(u32::to_le_bytes).collect();
        let root = buffer.recovery_opaque(crate::encoding::Encoding::UTF8, &raw, &offsets, 128).unwrap();
        let source = Buffer::recovery_snapshot(root);
        let tiny = source.slice(63..64).unwrap();
        assert_eq!(tiny.retained_storage.bytes(), source.retained_storage.bytes());
        assert!(
            tiny.retained_storage.bytes() >= 2 * raw.len() + offsets.len(),
            "logical/raw/offset allocations remain fully pinned"
        );
        let repeated = tiny.concat(&tiny).unwrap().concat(&tiny).unwrap();
        assert_eq!(repeated.retained_storage.bytes(), source.retained_storage.bytes());
        repeated.retained_storage.assert_matches_tree(&repeated.root);
    }

    #[test]
    fn retained_storage_preserved_alias_slices_charge_each_payload_once() {
        use crate::decoded_backing::{DecodedBacking, Interpretation};
        use crate::encoding::Encoding;
        use std::sync::atomic::{AtomicBool, AtomicU64};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "kwil-retained-alias-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        let owner = DecodedBacking::prepare_from_reader(
            &mut io::Cursor::new(b"\x88\x62Z"),
            3,
            Interpretation::new(Encoding::for_label("big5").unwrap(), 0).unwrap(),
            &directory,
            &AtomicBool::new(false),
        )
        .unwrap();
        let foreign = Buffer::from_decoded(owner.snapshot());
        let mut target = Buffer::with_add_memory_limit(31);
        target.set_undo_memory_limit(0);
        target.insert_snapshot_range(0, &foreign.snapshot(), 0..foreign.len()).unwrap();
        drop(foreign);
        assert!(owner.try_retire().is_ok());
        let source = target.snapshot();
        source.retained_storage.assert_matches_tree(&source.root);
        let scalar = source.slice(2..4).unwrap();
        let repeated = scalar.concat(&scalar).unwrap().concat(&scalar).unwrap();
        assert_eq!(source.retained_storage.bytes(), scalar.retained_storage.bytes());
        assert_eq!(source.retained_storage.bytes(), repeated.retained_storage.bytes());
        assert!(source.retained_storage.bytes() >= 31);
        repeated.retained_storage.assert_matches_tree(&repeated.root);
        target.begin();
        target.delete(0..4).unwrap();
        target.snapshot.retained_storage.assert_matches_tree(&target.snapshot.root);
        target.cancel();
        target.snapshot.retained_storage.assert_matches_tree(&target.snapshot.root);
        target.delete(0..target.len()).unwrap();
        assert_eq!(target.snapshot.retained_storage.bytes(), 0);
        assert_eq!(target.add_memory_bytes(), 31);
        drop((source, scalar, repeated));
        assert_eq!(target.add_memory_bytes(), 0);
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn resident_activation_preserves_spill_only_replay_provenance_and_undo() {
        use crate::encoding::{self, Encoding, SavePlan};
        use crate::{RecoveryAssembler, RecoveryLimits};
        let raw = b"A\xff\xc2\xc2";
        let mut replay = RecoveryAssembler::new(RecoveryLimits {
            add_memory_bytes: 0,
            undo_memory_bytes: 1 << 20,
            max_origins: 0,
            max_records: 128,
            max_pieces: 128,
            max_blocks: 128,
            max_metadata_bytes: 8 << 20,
        })
        .unwrap();
        let mut revision = replay.begin_revision();
        revision.begin_fragment().unwrap();
        revision.append_bytes(raw, || false).unwrap();
        revision.replace(0..0, || false).unwrap();
        revision.promote_raw_v1(|| false).unwrap();
        revision.commit(|| false).unwrap();
        let mut buffer = replay.finish(|| false).unwrap();
        let before = buffer.snapshot();
        let budget = buffer.added.clone();
        let owner = buffer.owner.clone();
        let importer = buffer.snapshot_importer();
        assert_eq!(buffer.add_memory_bytes(), 0);
        let spills = before.storage_resources().spilled_blocks;
        assert!(spills >= 3);
        buffer.enable_resident_additions(2).unwrap();
        assert!(Arc::ptr_eq(&budget, &buffer.added));
        assert!(Arc::ptr_eq(&owner, &buffer.owner));
        assert_eq!(buffer.history_depth(), (0, 0));
        assert_eq!(buffer.snapshot().storage_resources().spilled_blocks, spills);
        buffer.insert(buffer.len(), b"x").unwrap();
        assert_eq!(buffer.add_memory_bytes(), 1, "future typing uses the activated original budget");
        assert_eq!(buffer.snapshot().storage_resources().spilled_blocks, spills);
        let plan = SavePlan { encoding: Encoding::UTF8, bom: false, line_ending: None };
        let write = |snapshot: &Snapshot| {
            let mut bytes = Vec::new();
            encoding::write_snapshot(snapshot, plan, &mut bytes).unwrap();
            bytes
        };
        let mut edited = raw.to_vec();
        edited.push(b'x');
        assert_eq!(write(&buffer.snapshot()), edited);
        assert!(buffer.undo());
        assert_eq!(write(&buffer.snapshot()), raw);
        assert!(buffer.redo());
        assert_eq!(write(&buffer.snapshot()), edited);
        assert_eq!(write(&before), raw);
        let mut foreign = Buffer::empty();
        foreign.insert(0, b"Y").unwrap();
        let imported = importer.prepare(&foreign.snapshot(), 0..1).unwrap();
        assert_eq!(buffer.add_memory_bytes(), 2);
        buffer.insert(buffer.len(), b"Z").unwrap();
        assert_eq!(buffer.add_memory_bytes(), 2, "pre-activation importer and typing share one cap");
        assert!(buffer.snapshot().storage_resources().spilled_blocks > spills);
        drop(imported);
        assert_eq!(buffer.add_memory_bytes(), 1);
    }

    #[test]
    fn resident_activation_refuses_zero_repeat_and_preconfigured_budget_without_reset() {
        let mut disabled = Buffer::with_add_memory_limit(0);
        assert_eq!(disabled.enable_resident_additions(0).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        disabled.insert(0, b"x").unwrap();
        assert_eq!(disabled.add_memory_bytes(), 0);
        assert_eq!(disabled.snapshot().storage_resources().spilled_blocks, 1);
        disabled.enable_resident_additions(4).unwrap();
        for cap in [0, 4, 8] {
            assert_eq!(disabled.enable_resident_additions(cap).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        }
        assert_eq!(disabled.history_depth(), (1, 0));
        let mut configured = Buffer::with_add_memory_limit(4);
        assert!(configured.enable_resident_additions(8).is_err());
        configured.insert(0, b"ab").unwrap();
        let budget = configured.added.clone();
        assert!(configured.enable_resident_additions(8).is_err());
        assert!(Arc::ptr_eq(&budget, &configured.added));
        assert_eq!(configured.add_memory_bytes(), 2, "failed activation never resets live accounting");
    }

    #[test]
    fn resident_activation_precaptured_concurrent_importers_share_one_cap() {
        let mut destination = Buffer::with_add_memory_limit(0);
        let first_importer = destination.snapshot_importer();
        let second_importer = destination.snapshot_importer();
        let mut foreign = Buffer::empty();
        foreign.insert(0, b"XY").unwrap();
        let source = foreign.snapshot();
        let gate = Arc::new(std::sync::Barrier::new(3));
        let spawn = |importer: SnapshotImporter| {
            let gate = gate.clone();
            let source = source.clone();
            std::thread::spawn(move || {
                gate.wait();
                importer.prepare(&source, 0..source.len()).unwrap()
            })
        };
        let first = spawn(first_importer.clone());
        let second = spawn(second_importer.clone());
        destination.enable_resident_additions(4).unwrap();
        gate.wait();
        let first = first.join().unwrap();
        let second = second.join().unwrap();
        assert_eq!(destination.add_memory_bytes(), 4);
        assert_eq!(first.storage_resources().spilled_blocks + second.storage_resources().spilled_blocks, 0);
        let third = first_importer.prepare(&source, 0..source.len()).unwrap();
        assert_eq!(third.storage_resources().spilled_blocks, 1);
        assert_eq!(destination.add_memory_bytes(), 4);
        let shared = first.clone();
        drop(first);
        assert_eq!(destination.add_memory_bytes(), 4, "snapshot clones do not change physical-byte accounting");
        drop(shared);
        assert_eq!(destination.add_memory_bytes(), 2);
        let fourth = second_importer.prepare(&source, 0..source.len()).unwrap();
        assert_eq!(fourth.storage_resources().spilled_blocks, 0);
        assert_eq!(destination.add_memory_bytes(), 4);
        drop((second, third, fourth));
        assert_eq!(destination.add_memory_bytes(), 0);
    }

    #[test]
    fn decoded_cancellable_attach_interrupts_index_and_releases_partial_origin() {
        use crate::decoded_backing::{DecodedBacking, Interpretation};
        use crate::encoding::Encoding;
        use std::sync::atomic::AtomicBool;
        let fixture = OriginalsFixture::new();
        let path = fixture.0.join("cancelled-logical-index");
        std::fs::write(&path, vec![b'x'; 128 << 10]).unwrap();
        let file = TextFile::open(&path).unwrap();
        let owner = DecodedBacking::prepare(
            &file,
            Interpretation::new(Encoding::UTF8, 0).unwrap(),
            &fixture.0,
            &AtomicBool::new(false),
        )
        .unwrap();
        let mut polls = 0;
        let result = Buffer::from_decoded_cancellable(owner.snapshot(), || {
            polls += 1;
            polls == 3
        });
        assert!(matches!(result, Err(ref error) if error.kind() == io::ErrorKind::Interrupted));
        assert_eq!(polls, 3);
        assert!(owner.try_retire().is_ok(), "cancelled attach must not keep a partial piece/index origin alive");
    }

    #[test]
    fn decoded_cancellable_attach_handles_empty_and_matches_existing_constructor() {
        use crate::decoded_backing::{DecodedBacking, Interpretation};
        use crate::encoding::Encoding;
        use std::sync::atomic::AtomicBool;
        let fixture = OriginalsFixture::new();
        for (n, bytes) in [&b""[..], &b"A\r\nB\n\xffC"[..]].into_iter().enumerate() {
            let path = fixture.0.join(format!("attach-{n}"));
            std::fs::write(&path, bytes).unwrap();
            let file = TextFile::open(&path).unwrap();
            let owner = DecodedBacking::prepare(
                &file,
                Interpretation::new(Encoding::UTF8, 0).unwrap(),
                &fixture.0,
                &AtomicBool::new(false),
            )
            .unwrap();
            let mut polls = 0;
            let cancelled = Buffer::from_decoded_cancellable(owner.snapshot(), || {
                polls += 1;
                polls == 2
            });
            assert!(matches!(cancelled, Err(ref error) if error.kind() == io::ErrorKind::Interrupted));
            let old = Buffer::from_decoded(owner.snapshot());
            let new = Buffer::from_decoded_cancellable(owner.snapshot(), || false).unwrap();
            let mut old_bytes = Vec::new();
            let mut new_bytes = Vec::new();
            old.write_to(&mut old_bytes).unwrap();
            new.write_to(&mut new_bytes).unwrap();
            assert_eq!(new_bytes, old_bytes);
            assert_eq!(new.line_count(), old.line_count());
            for line in 0..=old.line_count() {
                assert_eq!(new.line_start(line).unwrap(), old.line_start(line).unwrap());
            }
            for offset in 0..=old.len() {
                assert_eq!(new.offset_to_line_col(offset).unwrap(), old.offset_to_line_col(offset).unwrap());
            }
            drop((old, new, cancelled));
            assert!(owner.try_retire().is_ok());
        }
    }

    #[test]
    fn decoded_source_bookkeeping_charges_index_once_and_prunes_empty_roots() {
        use crate::decoded_backing::{DecodedBacking, Interpretation};
        use crate::encoding::Encoding;
        use std::sync::atomic::AtomicBool;
        let fixture = OriginalsFixture::new();
        let path = fixture.0.join("decoded-history.txt");
        std::fs::write(&path, b"z\n".repeat(100_000)).unwrap();
        let file = TextFile::open(&path).unwrap();
        let owner = DecodedBacking::prepare(
            &file,
            Interpretation::new(Encoding::UTF8, 0).unwrap(),
            &fixture.0,
            &AtomicBool::new(false),
        )
        .unwrap();
        let mut buffer = Buffer::from_decoded(owner.snapshot());
        let initial = buffer.snapshot();
        let cost = initial.originals.values().next().unwrap().0.retained_bytes();
        assert!(cost > 1000, "logical sparse index must be accounted");
        buffer.begin();
        for line in (0..100).rev() {
            buffer.insert(line * 2, b"x").unwrap();
        }
        buffer.commit();
        assert_eq!(buffer.snapshot.originals.len(), 1);
        assert_eq!(buffer.snapshot.originals.values().next().unwrap().1, 200_000);
        assert!(buffer.snapshot.retained_bytes() < cost + 200_000, "100 pieces must not charge 100 full indexes");
        let clipped = initial.slice(1..2).unwrap();
        assert_eq!(clipped.originals.len(), 1);
        assert_eq!(clipped.originals.values().next().unwrap().1, 1);
        let doubled = clipped.concat(&clipped).unwrap();
        assert_eq!(doubled.originals.len(), 1);
        assert_eq!(doubled.originals.values().next().unwrap().1, 2);
        buffer.set_undo_memory_limit(0);
        buffer.delete(0..buffer.len()).unwrap();
        assert!(buffer.snapshot.originals.is_empty());
        assert_eq!(buffer.undo_memory_bytes(), 0);
        drop((buffer, initial, clipped, doubled));
        assert!(owner.try_retire().is_ok(), "bookkeeping must not retain the decoded origin");
    }

    #[test]
    fn own_composite_import_uses_cached_lineage_without_visiting_leaves() {
        let mut buffer = Buffer::empty();
        for _ in 0..32 {
            buffer.insert(buffer.len(), b"abc\n").unwrap();
        }
        let source = buffer.snapshot();
        assert_eq!(
            piece_tree::import_visit_count(source.root.clone(), &buffer.added, &buffer.owner),
            1,
            "own composite import must not traverse its leaves"
        );
        let prepared = buffer.snapshot_importer().prepare(&source, 0..source.len()).unwrap();
        assert!(Arc::ptr_eq(source.root.as_ref().unwrap(), prepared.root.as_ref().unwrap()));
        buffer.set_undo_memory_limit(0);
        buffer.delete(0..buffer.len()).unwrap();
        buffer.insert_snapshot_range(0, &prepared, 0..prepared.len()).unwrap();
        assert!(Arc::ptr_eq(source.root.as_ref().unwrap(), buffer.snapshot.root.as_ref().unwrap()));
        assert_eq!(buffer.add_memory_bytes(), 128);
    }

    #[test]
    fn model_metadata_charge_is_idempotent_and_obeys_history_cap() {
        let mut buffer = Buffer::empty();
        buffer.insert(0, b"abc").unwrap();
        let roots = buffer.undo_memory_bytes();
        buffer.charge_latest_model_history(4096);
        assert_eq!(buffer.undo_memory_bytes(), roots + 4096);
        buffer.charge_latest_model_history(1024);
        assert_eq!(buffer.undo_memory_bytes(), roots + 1024);
        buffer.set_undo_memory_limit(roots + 1023);
        assert_eq!(buffer.history_depth(), (0, 0));
        assert_eq!(buffer.undo_memory_bytes(), 0);
    }
    struct OriginalsFixture(std::path::PathBuf);
    impl OriginalsFixture {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "kwil-foreign-originals-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&dir).unwrap();
            Self(dir)
        }
        fn file(&self, n: usize) -> TextFile {
            let path = self.0.join(format!("{n}.txt"));
            std::fs::write(&path, [b'x'; 65536]).unwrap();
            TextFile::open(&path).unwrap()
        }
    }
    impl Drop for OriginalsFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn tiny_foreign_original_pastes_release_resident_sources_under_zero_caps() {
        let fixture = OriginalsFixture::new();
        let mut target = Buffer::with_add_memory_limit(0);
        target.set_undo_memory_limit(0);
        for n in 0..4 {
            let file = fixture.file(n);
            assert_eq!(file.resident_original_bytes(), 65536);
            let source = Buffer::from_file(file.clone()).unwrap();
            let probe = file.history_backing_ref();
            target.insert_snapshot_range(target.len(), &source.snapshot(), 0..1).unwrap();
            drop((source, file));
            assert_eq!(
                probe.retained_bytes(),
                0,
                "a one-byte paste must not retain a 64 KiB foreign resident original"
            );
            assert!(target.snapshot.originals.is_empty());
        }
        assert_eq!(target.len(), 4);
        assert_eq!(target.add_memory_bytes(), 0);
        assert_eq!(target.undo_memory_bytes(), 0);
        let mut bytes = Vec::new();
        target.write_to(&mut bytes).unwrap();
        assert_eq!(bytes, b"xxxx");
    }

    #[test]
    fn deleting_last_original_byte_prunes_all_source_bookkeeping() {
        let fixture = OriginalsFixture::new();
        let source = Buffer::from_file(fixture.file(0)).unwrap().snapshot();
        let mut target = Buffer::empty();
        target.set_undo_memory_limit(0);
        for _ in 0..8 {
            target.insert_snapshot_range(0, &source, 0..source.len()).unwrap();
            // This empty destination deliberately imports foreign data. Check
            // pruning separately on its own original below.
            target.delete(0..target.len()).unwrap();
            target = Buffer::from_file(fixture.file(1)).unwrap();
            target.set_undo_memory_limit(0);
            assert_eq!(target.snapshot.originals.len(), 1);
            assert!(target.snapshot.retained_bytes() > target.snapshot.root.as_ref().unwrap().metrics.retained);
            target.delete(0..target.len()).unwrap();
            assert!(target.snapshot.originals.is_empty(), "empty buffers must not accumulate weak source entries");
            assert_eq!(target.snapshot.retained_bytes(), 0);
            assert_eq!(target.undo_memory_bytes(), 0);
        }
    }

    #[test]
    fn immutable_slice_concat_keep_own_original_shared_and_counted_once() {
        let fixture = OriginalsFixture::new();
        let mut target = Buffer::from_file(fixture.file(0)).unwrap();
        let source = target.snapshot();
        let part = source.slice(16..65520).unwrap();
        let joined = part.concat(&part).unwrap();
        target.insert_snapshot_range(target.len(), &joined, 0..joined.len()).unwrap();
        assert_eq!(
            target.add_memory_bytes(),
            0,
            "own original composition must share pieces rather than materialize bytes"
        );
        assert_eq!(target.snapshot.originals.len(), 1);
        assert_eq!(target.len(), 65536 + 2 * (65520 - 16));
        assert_eq!(target.snapshot.originals.values().next().unwrap().1, target.len());
        assert!(target.undo());
        assert_eq!(target.len(), 65536);
        target.delete(0..target.len()).unwrap();
        target.set_undo_memory_limit(0);
        assert!(target.snapshot.originals.is_empty());
        target.insert_snapshot_range(0, &source, 0..source.len()).unwrap();
        assert_eq!(
            target.add_memory_bytes(),
            0,
            "restoring this document's own original after delete-all must still share"
        );
        let path = fixture.0.join("saved.txt");
        std::fs::write(&path, [b'x'; 65536]).unwrap();
        let checkpoint = Buffer::from_file(TextFile::open_saved_version(&path).unwrap()).unwrap().snapshot();
        target.checkpoint_current(checkpoint).unwrap();
        target.begin();
        target.delete(0..target.len()).unwrap();
        target.insert_snapshot_range(0, &source, 0..source.len()).unwrap();
        target.commit();
        assert_eq!(target.add_memory_bytes(), 0, "old clipboard pieces retain document ownership across a checkpoint");
    }

    #[test]
    fn saved_baseline_shortcuts_reject_foreign_resident_slices() {
        let fixture = OriginalsFixture::new();
        let source = Buffer::from_file(fixture.file(0)).unwrap().snapshot();
        let foreign = source.slice(0..1).unwrap();
        let mut target = Buffer::with_add_memory_limit(0);
        target.set_undo_memory_limit(0);
        target.insert(0, b"z").unwrap();
        assert!(target.checkpoint_current(foreign.clone()).is_err(), "checkpoint must not bypass foreign-source caps");
        assert!(
            target.install_worker_edit(foreign).is_err(),
            "worker edit must not promote a resident slice to a new baseline"
        );
        assert!(
            target.install_worker_edit(source).is_err(),
            "worker edit must not adopt a whole foreign resident copy either"
        );
        let mut bytes = Vec::new();
        target.write_to(&mut bytes).unwrap();
        assert_eq!(bytes, b"z");
    }
    #[test]
    fn batch_cancel_coalescing_shared_ranges_and_checkpoint_keep_history() {
        let mut b = Buffer::empty();
        b.insert(0, b"abc").unwrap();
        let source = b.snapshot();
        b.begin();
        b.delete(0..2).unwrap();
        b.cancel();
        let mut out = Vec::new();
        b.write_to(&mut out).unwrap();
        assert_eq!(out, b"abc");
        b.begin();
        b.insert_snapshot_range(3, &source, 0..2).unwrap();
        b.commit_coalesced(true);
        assert_eq!(b.history_depth(), (1, 0));
        assert!(b.undo());
        assert!(b.is_empty());
        assert!(b.redo());
        let fixture = OriginalsFixture::new();
        let path = fixture.0.join("checkpoint.txt");
        std::fs::write(&path, b"abcab").unwrap();
        let checkpoint = Buffer::from_file(TextFile::open_saved_version(&path).unwrap()).unwrap().snapshot();
        b.checkpoint_current(checkpoint).unwrap();
        assert!(b.undo());
        assert!(b.is_empty());
        assert!(b.redo());
        let mut out = Vec::new();
        b.write_to(&mut out).unwrap();
        assert_eq!(out, b"abcab");
        b.begin();
        assert!(b.checkpoint_current(Snapshot::default()).is_err());
        b.cancel();
    }

    #[test]
    fn foreign_snapshot_paste_obeys_destination_resident_budget() {
        let mut source = Buffer::empty();
        source.insert(0, &[b'x'; 1024]).unwrap();
        let mut target = Buffer::with_add_memory_limit(16);
        target.insert_snapshot_range(0, &source.snapshot(), 0..1024).unwrap();
        assert!(
            !piece_tree::spill_paths(&target.snapshot.root).is_empty(),
            "foreign resident blocks must be imported under the destination cap"
        );
        drop(source);
        assert!(target.add_memory_bytes() <= 16);
        let mut out = Vec::new();
        target.write_to(&mut out).unwrap();
        assert_eq!(out, [b'x'; 1024]);
    }

    #[test]
    fn history_charges_sparse_backings_once_per_snapshot_not_per_piece() {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "kwil-backing-budget-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, b"z\n".repeat(262144)).unwrap();
        let file = TextFile::open_saved_version(&path).unwrap();
        let mut b = Buffer::from_file(file.clone()).unwrap();
        let index = file.index().unwrap().memory_bytes();
        b.insert(1, b"x").unwrap();
        assert!(b.undo_memory_bytes() >= 2 * index, "each retained version must charge its sparse index");
        assert!(b.undo());
        b.set_undo_memory_limit(128 << 10);
        b.begin();
        for row in (0..100).rev() {
            b.insert(row * 2, b"x").unwrap();
        }
        b.commit();
        assert!(b.undo(), "100 pieces of one original must not charge 100 full indexes");
        assert_eq!(b.len(), 524288);
        drop((b, file));
        std::fs::remove_file(path).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use memchr::memchr_iter;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new(bytes: &[u8]) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "kwil-buffer-test-{}-{}.txt",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::OpenOptions::new().write(true).create_new(true).open(&path).unwrap().write_all(bytes).unwrap();
            Self(path)
        }

        fn open(&self) -> TextFile {
            TextFile::open(&self.0).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_file(&self.0).unwrap();
        }
    }

    fn bytes(snapshot: &Snapshot) -> Vec<u8> {
        let mut out = Vec::new();
        snapshot.write_to(&mut out).unwrap();
        out
    }

    fn check(buffer: &Buffer, model: &[u8], step: usize) {
        assert_eq!(bytes(&buffer.snapshot()), model, "bytes at step {step}");
        assert_eq!(buffer.len(), model.len() as u64, "length at step {step}");
        let starts: Vec<_> = std::iter::once(0).chain(memchr_iter(b'\n', model).map(|at| at as u64 + 1)).collect();
        assert_eq!(buffer.line_count(), starts.len() as u64, "line count at step {step}");
        for (line, &start) in starts.iter().enumerate() {
            assert_eq!(buffer.line_start(line as u64).unwrap(), Some(start), "line {line}, step {step}");
        }
        assert_eq!(buffer.line_start(starts.len() as u64).unwrap(), None);
        for offset in [0, model.len() / 2, model.len()] {
            let line = starts.partition_point(|&at| at <= offset as u64) - 1;
            assert_eq!(
                buffer.offset_to_line_col(offset as u64).unwrap(),
                Some((line as u64, offset as u64 - starts[line]))
            );
        }
        piece_tree::assert_valid(&buffer.snapshot.root);
    }

    fn random(seed: &mut u64) -> usize {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (*seed >> 32) as usize
    }

    /// Independent byte vectors and linear history; all lines checked after
    /// every operation. Seed is fixed so a failure is exactly reproducible.
    #[test]
    fn randomized_model_6000_operations() {
        let initial = b"original\r\n\xff\n".repeat(3100);
        let fixture = Fixture::new(&initial);
        let mut buffer = Buffer::from_file(fixture.open()).unwrap();
        buffer.set_undo_memory_limit(usize::MAX);
        let mut model = initial.clone();
        let mut history: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        let mut position = 0;
        let mut seed = 0x16_18_42;
        for step in 0..6000 {
            let operation = random(&mut seed) % 7;
            let before = model.clone();
            let mut changed = false;
            match operation {
                0..=2 => {
                    let at = random(&mut seed) % (model.len() + 1);
                    let n = 1 + random(&mut seed) % 23;
                    let text: Vec<u8> =
                        (0..n).map(|_| [b'a', b'\r', b'\n', 0, 0xff, b'z'][random(&mut seed) % 6]).collect();
                    buffer.insert(at as u64, &text).unwrap();
                    model.splice(at..at, text);
                    changed = true;
                }
                3 if !model.is_empty() => {
                    let a = random(&mut seed) % model.len();
                    let b = a + 1 + random(&mut seed) % (model.len() - a);
                    buffer.delete(a as u64..b as u64).unwrap();
                    model.drain(a..b);
                    changed = true;
                }
                4 => {
                    assert_eq!(buffer.undo(), position > 0, "undo at step {step}");
                    if position > 0 {
                        position -= 1;
                        model = history[position].0.clone();
                    }
                }
                5 => {
                    assert_eq!(buffer.redo(), position < history.len(), "redo at step {step}");
                    if position < history.len() {
                        model = history[position].1.clone();
                        position += 1;
                    }
                }
                6 => {
                    buffer.begin();
                    buffer.insert(0, b"\nreplace\r\n").unwrap();
                    model.splice(0..0, b"\nreplace\r\n".iter().copied());
                    let end = model.len().min(12);
                    buffer.delete(1..end as u64).unwrap();
                    model.drain(1..end);
                    buffer.insert(buffer.len(), b"\xfftail").unwrap();
                    model.extend_from_slice(b"\xfftail");
                    buffer.commit();
                    changed = true;
                }
                _ => {}
            }
            if changed {
                history.truncate(position);
                history.push((before, model.clone()));
                position += 1;
            }
            check(&buffer, &model, step);
        }
        while position > 0 {
            assert!(buffer.undo());
            position -= 1;
            check(&buffer, &history[position].0, 6000 + position);
        }
        assert_eq!(bytes(&buffer.snapshot()), initial);
    }

    #[test]
    fn snapshot_is_stable_send_sync_and_spill_is_bounded() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Snapshot>();
        let fixture = Fixture::new(b"original\r\nlast");
        let mut buffer = Buffer::from_file(fixture.open()).unwrap();
        let original = buffer.snapshot();
        buffer.delete(0..buffer.len()).unwrap();
        buffer.insert(0, b"changed").unwrap();
        assert_eq!(bytes(&original), b"original\r\nlast");

        let mut buffer = Buffer::with_add_memory_limit(3);
        buffer.set_undo_memory_limit(0);
        buffer.insert(0, b"abc").unwrap();
        let snapshot = buffer.snapshot();
        buffer.delete(0..3).unwrap();
        let large = b"x\r\n\xff".repeat(piece_tree::ADD_BLOCK / 2);
        buffer.insert(0, &large).unwrap();
        assert_eq!(buffer.add_memory_bytes(), 3); // pinned snapshot owns the quota
        assert_eq!(bytes(&buffer.snapshot()), large);
        assert_eq!(buffer.line_start(25_000).unwrap(), Some(99_999));
        let paths = spill_paths(&buffer.snapshot.root);
        assert!(!paths.is_empty());
        assert!(paths.iter().all(|path| path.exists()));
        buffer.delete(0..buffer.len()).unwrap();
        assert!(paths.iter().all(|path| !path.exists()));
        let old = std::thread::spawn(move || bytes(&snapshot)).join().unwrap();
        assert_eq!(old, b"abc");
        assert_eq!(buffer.add_memory_bytes(), 0);
    }

    // Keep storage internals private to the tree; tests ask only for temp paths.
    fn spill_paths(tree: &Tree) -> Vec<PathBuf> {
        piece_tree::spill_paths(tree)
    }

    #[test]
    fn transactions_typing_and_linear_redo() {
        let mut buffer = Buffer::empty();
        assert!(!buffer.undo());
        assert!(!buffer.redo());
        assert!(buffer.begin());
        assert!(!buffer.begin());
        assert!(buffer.commit());
        assert!(!buffer.undo());
        buffer.insert(0, b"one two one").unwrap();
        buffer.begin();
        buffer.delete(8..11).unwrap();
        buffer.insert(8, b"three").unwrap();
        buffer.delete(0..3).unwrap();
        buffer.insert(0, b"three").unwrap();
        buffer.commit();
        assert_eq!(bytes(&buffer.snapshot()), b"three two three");
        assert!(buffer.undo());
        assert_eq!(bytes(&buffer.snapshot()), b"one two one");
        assert!(buffer.redo());
        assert_eq!(bytes(&buffer.snapshot()), b"three two three");
        buffer.undo();
        buffer.insert_typing(11, b"a", Duration::from_millis(100)).unwrap();
        buffer.insert_typing(12, b"b", Duration::from_millis(500)).unwrap();
        buffer.insert_typing(13, b"c", Duration::from_millis(900)).unwrap();
        assert!(!buffer.redo());
        buffer.undo();
        assert_eq!(bytes(&buffer.snapshot()), b"one two one");
        buffer.redo();
        buffer.insert_typing(14, b"d", Duration::from_millis(2000)).unwrap();
        buffer.insert_typing(0, b"e", Duration::from_millis(2100)).unwrap();
        buffer.undo();
        assert_eq!(bytes(&buffer.snapshot()), b"one two oneabcd");
        buffer.undo();
        assert_eq!(bytes(&buffer.snapshot()), b"one two oneabc");
        buffer.begin();
        buffer.insert_typing(14, b"!", Duration::from_millis(2200)).unwrap();
        buffer.commit();
        buffer.insert_typing(15, b"?", Duration::from_millis(2300)).unwrap();
        buffer.undo();
        assert_eq!(bytes(&buffer.snapshot()), b"one two oneabc!");
        buffer.undo();
        assert_eq!(bytes(&buffer.snapshot()), b"one two oneabc");
    }

    #[test]
    fn cap_shrink_after_undo_all_preserves_redo_roundtrip() {
        let mut buffer = Buffer::empty();
        buffer.insert(0, b"a").unwrap();
        let first_cost = buffer.undo_memory_bytes();
        buffer.insert(1, b"b").unwrap();
        buffer.insert(2, b"c").unwrap();
        let cap = buffer.undo_memory_bytes() - first_cost;
        for _ in 0..3 {
            assert!(buffer.undo());
        }
        buffer.set_undo_memory_limit(cap);
        assert!(buffer.redo());
        assert!(buffer.undo());
        assert_eq!(bytes(&buffer.snapshot()), b"");
    }

    #[test]
    fn history_cap_evicts_oldest_and_releases_add_bytes() {
        let mut buffer = Buffer::empty();
        buffer.insert(0, b"a").unwrap();
        let first_cost = buffer.undo_memory_bytes();
        buffer.insert(1, b"b").unwrap();
        buffer.insert(2, b"c").unwrap();
        let cap = buffer.undo_memory_bytes() - first_cost;
        buffer.set_undo_memory_limit(cap);
        assert_eq!(buffer.undo_memory_bytes(), cap);
        assert!(buffer.undo());
        assert!(buffer.undo());
        assert!(!buffer.undo());
        assert_eq!(bytes(&buffer.snapshot()), b"a");
        assert!(buffer.redo());
        assert!(buffer.redo());
        buffer.delete(0..3).unwrap();
        buffer.set_undo_memory_limit(0);
        assert_eq!(buffer.undo_memory_bytes(), 0);
        assert_eq!(buffer.add_memory_bytes(), 0);
        assert!(!buffer.undo());
        assert!(!buffer.redo());
    }

    #[test]
    fn borrowed_ranges_lines_and_errors() {
        let mut buffer = Buffer::empty();
        buffer.insert(0, b"one\rtwo\r").unwrap();
        buffer.insert(buffer.len(), b"\n\nlast\r\n").unwrap();
        buffer.insert(2, b"\xff").unwrap();
        let lines: Vec<Vec<u8>> =
            buffer.lines(0, 10).map(|line| line.unwrap().flat_map(Result::unwrap).copied().collect()).collect();
        assert_eq!(lines, [b"on\xffe\rtwo".to_vec(), vec![], b"last".to_vec(), vec![]]);
        let middle: Vec<_> = buffer.chunks(3..12).unwrap().flat_map(Result::unwrap).copied().collect();
        assert_eq!(middle, b"e\rtwo\r\n\nl");
        assert_eq!(buffer.chunks(2..2).unwrap().count(), 0);
        assert_eq!(buffer.chunks(1..1).unwrap().count(), 0);
        assert_eq!(buffer.lines(100, 2).count(), 0);
        assert_eq!(buffer.lines(0, 0).count(), 0);
        let before = bytes(&buffer.snapshot());
        assert!(buffer.insert(buffer.len() + 1, b"bad").is_err());
        assert!(buffer.delete(0..buffer.len() + 1).is_err());
        let reversed = Range { start: 5, end: 1 };
        assert!(buffer.delete(reversed.clone()).is_err());
        assert!(buffer.chunks(reversed).is_err());
        assert_eq!(buffer.offset_to_line_col(buffer.len() + 1).unwrap(), None);
        assert_eq!(bytes(&buffer.snapshot()), before);
        struct FailingWriter;
        impl Write for FailingWriter {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("write failed"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        assert!(buffer.write_to(&mut FailingWriter).is_err());
    }

    #[test]
    fn changed_original_returns_errors_without_mutating_history() {
        use crate::FileState;
        // Rename a mapped file rather than truncate a live mapping (which
        // Windows disallows). No stale mapped pages need to be touched.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("buffer-changed-{}.bin", std::process::id()));
        let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&path).unwrap();
        file.set_len(crate::file::MAP_THRESHOLD).unwrap();
        file.write_all(b"a\nbody").unwrap();
        drop(file);
        let original = TextFile::open(&path).unwrap();
        let mut buffer = Buffer::from_file(original.clone()).unwrap();
        let snapshot = buffer.snapshot();
        let mut chunks = snapshot.chunks(0..2).unwrap();
        let mut lines = snapshot.lines(0, 1);
        let rotated = path.with_extension("old");
        fs::rename(&path, &rotated).unwrap();
        // Surface absence/replacement before dereferencing the old mapping.
        assert!(matches!(Buffer::from_file(original), Err(ReadError::Changed(FileState::Replaced))));
        assert!(matches!(chunks.next(), Some(Err(ReadError::Changed(FileState::Replaced)))));
        assert!(chunks.next().is_none());
        assert!(matches!(lines.next(), Some(Err(ReadError::Changed(FileState::Replaced)))));
        assert!(lines.next().is_none());
        assert!(matches!(snapshot.offset_to_line_col(1), Err(ReadError::Changed(FileState::Replaced))));
        assert!(matches!(snapshot.line_start(1), Err(ReadError::Changed(FileState::Replaced))));
        let mut output = Vec::new();
        let error = snapshot.write_to(&mut output).unwrap_err();
        assert!(matches!(
            error.get_ref().and_then(|e| e.downcast_ref::<ReadError>()),
            Some(ReadError::Changed(FileState::Replaced))
        ));
        assert!(output.is_empty());
        assert!(buffer.insert(1, b"edit").is_err());
        assert!(buffer.delete(1..2).is_err());
        assert!(Arc::ptr_eq(buffer.snapshot.root.as_ref().unwrap(), snapshot.root.as_ref().unwrap()));
        assert_eq!(buffer.undo_memory_bytes(), 0);
        assert_eq!(buffer.add_memory_bytes(), 0);
        assert!(!buffer.undo());
        drop(chunks);
        drop(snapshot);
        drop(buffer);
        fs::remove_file(rotated).unwrap();
    }

    #[test]
    fn sparse_original_splits_at_checkpoint_and_crlf_boundaries() {
        let initial = b"x\r\n".repeat(5000);
        let fixture = Fixture::new(&initial);
        let mut buffer = Buffer::from_file(fixture.open()).unwrap();
        let mut model = initial;
        // Reverse order retains many pieces of the original on both sides.
        for (step, at) in [14999, 12288, 6145, 6144, 6143, 3072, 3071, 2, 1, 0].into_iter().enumerate() {
            buffer.insert(at, b"A\n\r\n").unwrap();
            model.splice(at as usize..at as usize, b"A\n\r\n".iter().copied());
            check(&buffer, &model, step);
        }
        for at in 0..=model.len() {
            let line = memchr_iter(b'\n', &model[..at]).count() as u64;
            let col = model[..at].iter().rposition(|&byte| byte == b'\n').map_or(at, |p| at - p - 1);
            assert_eq!(buffer.offset_to_line_col(at as u64).unwrap(), Some((line, col as u64)));
        }
    }

    /// One-shot measurement, explicitly excluded from ordinary test runs.
    /// Generate fixtures/log-1g.log with the spec's textbench gen command, then
    /// run release `cargo test -p kwil-core-text measure_1g -- --ignored --nocapture`.
    #[test]
    #[ignore = "requires the generated 1 GB fixture; one-shot measurement only"]
    fn measure_1g() {
        use std::time::Instant;
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/log-1g.log");
        let start = Instant::now();
        let file = TextFile::open(&path).unwrap();
        let open_ms = start.elapsed().as_secs_f64() * 1000.0;
        assert!(file.len() >= 1 << 30);
        let start = Instant::now();
        let lines = file.build_index(4).unwrap().line_count();
        let index_ms = start.elapsed().as_secs_f64() * 1000.0;
        let len = file.len();
        let middle = len / 2;
        let original_jump = file.index().unwrap().line_start(file.bytes().unwrap(), 15_000_000).unwrap();
        let start = Instant::now();
        let mut buffer = Buffer::from_file(file).unwrap();
        let attach_ms = start.elapsed().as_secs_f64() * 1000.0;
        let before = buffer.snapshot();
        let start = Instant::now();
        buffer.insert(middle, b"inserted\r\n").unwrap();
        let insert_ms = start.elapsed().as_secs_f64() * 1000.0;
        let start = Instant::now();
        let jump = buffer.line_start(15_000_000).unwrap().unwrap();
        let jump_ms = start.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(buffer.len(), len + 10);
        assert_eq!(buffer.line_count(), lines + 1);
        // The edit inserts a newline before 15M; that line is now original
        // 14,999,999 plus inserted bytes. Check against the pinned version.
        assert!(middle < original_jump);
        assert_eq!(jump, before.line_start(14_999_999).unwrap().unwrap() + 10);
        assert_eq!(before.len(), len);
        assert_eq!(before.line_count(), lines);
        assert_eq!(buffer.add_memory_bytes(), 10);
        println!(
            "bytes={len} lines={lines} open_ms={open_ms:.6} index_ms={index_ms:.6} from_file_ready_ms={attach_ms:.6} insert_ms={insert_ms:.6} line_15m_ms={jump_ms:.6} undo_retained_bytes={} add_bytes={}",
            buffer.undo_memory_bytes(),
            buffer.add_memory_bytes()
        );
        #[cfg(windows)]
        println!("private_bytes={}", private_bytes());
    }

    #[cfg(windows)]
    fn private_bytes() -> usize {
        #[repr(C)]
        struct Counters {
            cb: u32,
            faults: u32,
            values: [usize; 9],
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentProcess() -> *mut std::ffi::c_void;
        }
        #[link(name = "psapi")]
        unsafe extern "system" {
            fn GetProcessMemoryInfo(process: *mut std::ffi::c_void, counters: *mut Counters, size: u32) -> i32;
        }
        let size = std::mem::size_of::<Counters>() as u32;
        let mut counters = Counters { cb: size, faults: 0, values: [0; 9] };
        // SAFETY: a valid pseudo-handle and correctly sized writable structure.
        assert_ne!(unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, size) }, 0);
        counters.values[8]
    }
}
