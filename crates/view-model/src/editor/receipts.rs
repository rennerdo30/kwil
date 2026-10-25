//! Exact raw-byte replacements. Publishing only queues immutable references;
//! recipe iteration and snapshot reads belong on the journal worker.
use super::*;
use crate::document_worker::{DocumentContinuationPermit, DocumentJournalSink};
use kwil_core_workspace::TrimSpool;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditKind {
    Edit,
    Undo,
    Redo,
    WorkerTrim,
    WorkerSave,
}

#[derive(Clone)]
pub struct SnapshotInsert {
    pub snapshot: Snapshot,
    pub range: Range<u64>,
}
impl SnapshotInsert {
    pub fn len(&self) -> u64 {
        self.range.end - self.range.start
    }
    pub fn is_empty(&self) -> bool {
        self.range.is_empty()
    }
    pub fn write_to(&self, writer: &mut impl io::Write) -> io::Result<()> {
        for chunk in self.snapshot.chunks(self.range.clone())? {
            writer.write_all(chunk.map_err(io::Error::other)?)?;
        }
        Ok(())
    }
}
pub struct ByteReplacement {
    /// Coordinates in the intermediate document after preceding recipes.
    pub range: Range<u64>,
    pub insert: SnapshotInsert,
}
#[derive(Clone)]
pub struct EditReceipt {
    /// Contiguous producer order. Actual revision IDs may decrease on undo.
    pub sequence: u64,
    pub before_revision: u64,
    pub after_revision: u64,
    pub before: Snapshot,
    pub after: Snapshot,
    pub kind: EditKind,
    recipe: Recipe,
    inverse: bool,
}
impl EditReceipt {
    /// Retained recipe heap only; the packet already accounts for this inline
    /// receipt. Reserve this before transferring the pending receipt. Snapshot
    /// and trim-spool storage retain their existing source admission.
    pub(crate) fn recipe_metadata_bytes(&self) -> io::Result<usize> {
        match &self.recipe {
            Recipe::Explicit(shapes) => explicit_metadata_bytes(shapes),
            Recipe::Bulk(_, _, Some(bytes)) => Ok(*bytes),
            Recipe::Bulk(_, _, None) => Err(io::Error::from(io::ErrorKind::Unsupported)),
            Recipe::Trim(_) => Ok(0),
        }
    }

    pub fn replacement_count(&self) -> u64 {
        match &self.recipe {
            Recipe::Explicit(shapes) => shapes.len() as u64,
            Recipe::Bulk(_, count, _) => *count,
            Recipe::Trim(spool) => spool.len(),
        }
    }
    /// Worker-only, exact serialized apply order. One outer receipt is one
    /// atomic journal batch, including every cursor and every bulk row.
    pub fn for_each_replacement(&self, visitor: &mut impl FnMut(ByteReplacement) -> io::Result<()>) -> io::Result<()> {
        let mut shift = self.after.len() as i128 - self.before.len() as i128;
        self.recipe.visit(!self.inverse, &mut |shape| {
            let (range, source) = if self.inverse {
                (shape.range.start..shape.range.start + shape.inserted, shape.range.clone())
            } else {
                shift -= shape.inserted as i128 - (shape.range.end - shape.range.start) as i128;
                let start = (shape.range.start as i128 + shift) as u64;
                (shape.range.clone(), start..start + shape.inserted)
            };
            visitor(ByteReplacement { range, insert: SnapshotInsert { snapshot: self.after.clone(), range: source } })
        })
    }
}

/// Returning the receipt retains it in the model's single pending slot.
/// The already completed edit succeeds; subsequent mutations return WouldBlock.
pub type EditObserver = Box<dyn FnMut(EditReceipt) -> Result<(), EditReceipt> + Send>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditObserverRefusalReason {
    PendingReceipt,
    SealedSink,
    GenerationExhausted,
}
/// An observer installation refusal retains the exact incoming callback inline.
/// Neither formatting nor the error reason hides or invokes that owner.
pub struct EditObserverRefusal {
    pub observer: Option<EditObserver>,
    pub reason: EditObserverRefusalReason,
}
impl EditObserverRefusal {
    pub fn kind(&self) -> io::ErrorKind {
        match self.reason {
            EditObserverRefusalReason::PendingReceipt => io::ErrorKind::WouldBlock,
            EditObserverRefusalReason::SealedSink | EditObserverRefusalReason::GenerationExhausted => {
                io::ErrorKind::InvalidInput
            }
        }
    }
}
impl std::fmt::Debug for EditObserverRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EditObserverRefusal")
            .field("reason", &self.reason)
            .field("has_observer", &self.observer.is_some())
            .finish()
    }
}
impl std::fmt::Display for EditObserverRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "edit observer installation refused: {:?}", self.reason)
    }
}

/// A sealed refusal never transfers the pending receipt out of Editor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReceiptDeliveryRefusal {
    UnsupportedObserver,
    MissingSink,
    Full,
    Closed,
    WrongPermit,
    Admission,
}
impl std::fmt::Display for ReceiptDeliveryRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for ReceiptDeliveryRefusal {}

#[derive(Clone)]
pub(super) struct Shape {
    pub range: Range<u64>,
    pub inserted: u64,
}
type ShapeIter = Box<dyn DoubleEndedIterator<Item = io::Result<Shape>> + Send>;
#[derive(Clone)]
pub(super) enum Recipe {
    Explicit(Arc<Vec<Shape>>),
    // The optional bound includes the factory allocation and audited heap
    // storage owned by its seed. None is an explicit unknown-producer gap.
    Bulk(Arc<dyn Fn() -> ShapeIter + Send + Sync>, u64, Option<usize>),
    Trim(TrimSpool),
}
impl Recipe {
    pub(super) fn explicit(edits: &[Edit]) -> Self {
        Self::Explicit(Arc::new(
            edits.iter().map(|e| Shape { range: e.range.clone(), inserted: e.insert.len() }).collect(),
        ))
    }
    // Bulk seeds yield descending coordinates; all other sources are ascending.
    pub(super) fn bulk<I>(seed: I, count: u64, seed_heap_bytes: Option<usize>) -> Self
    where
        I: DoubleEndedIterator<Item = io::Result<Option<Edit>>> + Clone + Send + Sync + 'static,
    {
        let metadata = seed_heap_bytes
            .and_then(|bytes| bytes.checked_add(std::mem::size_of::<I>()))
            .and_then(|bytes| bytes.checked_add(2 * std::mem::size_of::<usize>()));
        Self::Bulk(
            Arc::new(move || {
                Box::new(seed.clone().rev().filter_map(|result| match result {
                    Ok(Some(e)) => Some(Ok(Shape { range: e.range, inserted: e.insert.len() })),
                    Ok(None) => None,
                    Err(e) => Some(Err(e)),
                }))
            }),
            count,
            metadata,
        )
    }
    fn visit(&self, descending: bool, f: &mut dyn FnMut(Shape) -> io::Result<()>) -> io::Result<()> {
        let items: ShapeIter = match self {
            Self::Explicit(shapes) => {
                // Cloning the Arc, not a per-edit vector on each iteration.
                let shapes = shapes.clone();
                Box::new((0..shapes.len()).map(move |i| Ok(shapes[i].clone())))
            }
            Self::Bulk(factory, _, _) => factory(),
            Self::Trim(spool) => Box::new(spool.ranges()?.map(|r| r.map(|range| Shape { range, inserted: 0 }))),
        };
        if descending {
            for shape in items.rev() {
                f(shape?)?;
            }
        } else {
            for shape in items {
                f(shape?)?;
            }
        }
        Ok(())
    }
    /// Keep typing-burst history proportional to cursor count. A change that
    /// escapes prior inserted spans starts a fresh undo step instead.
    pub(super) fn coalesced(&self, next: &Self) -> Option<Self> {
        let (Self::Explicit(previous), Self::Explicit(next)) = (self, next) else {
            return None;
        };
        if previous.len() != next.len() {
            return None;
        }
        let mut shift = 0_i128;
        let mut shapes = Vec::with_capacity(previous.len());
        for (old, new) in previous.iter().zip(next.iter()) {
            let start = (old.range.start as i128 + shift) as u64;
            if new.range.start < start || new.range.end > start + old.inserted {
                return None;
            }
            shapes.push(Shape {
                range: old.range.clone(),
                inserted: old.inserted + new.inserted - (new.range.end - new.range.start),
            });
            shift += old.inserted as i128 - (old.range.end - old.range.start) as i128;
        }
        Some(Self::Explicit(Arc::new(shapes)))
    }
    pub(super) fn cost(&self) -> usize {
        match self {
            Self::Explicit(shapes) => explicit_metadata_bytes(shapes).unwrap_or(usize::MAX),
            Self::Bulk(_, _, metadata) => metadata.unwrap_or(0).max(4096 + MAX_CURSORS * 32),
            Self::Trim(_) => 512,
        }
    }
}
#[derive(Clone)]
pub(super) struct Transition {
    pub before: Snapshot,
    pub after: Snapshot,
    pub recipe: Recipe,
}
pub(super) fn prepared_save_receipt(
    sequence: u64,
    before_revision: u64,
    after_revision: u64,
    kind: EditKind,
    transition: &Transition,
) -> EditReceipt {
    EditReceipt {
        sequence,
        before_revision,
        after_revision,
        before: transition.before.clone(),
        after: transition.after.clone(),
        kind,
        recipe: transition.recipe.clone(),
        inverse: false,
    }
}
#[derive(Default)]
pub(super) struct ReceiptState {
    observer: Option<EditObserver>,
    sink: Option<DocumentJournalSink>,
    destination_generation: u64,
    pub(super) pending: Option<EditReceipt>,
    pub(super) sequence: u64,
    pub(super) syntax_sequence: u64,
    pub(super) latest: Option<LatestEdit>,
    pub deferred_save: Option<super::save::SaveCompletionState>,
    pub(super) save_charge: Option<crate::visual_rows::MetadataReservation>,
}
fn explicit_metadata_bytes(shapes: &Arc<Vec<Shape>>) -> io::Result<usize> {
    metadata_capacity::<Shape>(shapes.capacity())?
        .checked_add(std::mem::size_of::<Vec<Shape>>() + 2 * std::mem::size_of::<usize>())
        .ok_or_else(metadata_overflow)
}
impl ReceiptState {
    pub(super) fn destination_generation(&self) -> u64 {
        self.destination_generation
    }
    pub(super) fn destination_change_ready(&self) -> io::Result<()> {
        self.destination_generation
            .checked_add(1)
            .map(|_| ())
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))
    }
    /// Called only by the worker's raw-reopen publication after its preflight.
    pub(super) fn reset_for_raw_reopen(&mut self) {
        let generation = self.destination_generation.checked_add(1).expect("raw reopen destination preflight");
        self.destination_generation = generation;
        *self = Self { destination_generation: generation, ..Self::default() };
    }
    pub(super) fn document_metadata_bytes(&self) -> io::Result<usize> {
        // Inline pending/latest/sink headers are already in sizeof(Editor).
        // Include at most TWO retained recipe allocations; either can outlive
        // eviction of the history entry that originally charged it.
        if self.observer.is_some() {
            // Unknown observer captures are a named unsupported accounting gap.
            return Err(io::Error::from(io::ErrorKind::Unsupported));
        }
        let mut bytes = 0;
        if let Some(sink) = &self.sink {
            metadata_add(&mut bytes, sink.document_metadata_bytes()?)?;
        }
        let pending_explicit = match self.pending.as_ref().map(|receipt| &receipt.recipe) {
            Some(Recipe::Explicit(shapes)) => {
                metadata_add(&mut bytes, explicit_metadata_bytes(shapes)?)?;
                Some(shapes)
            }
            Some(Recipe::Bulk(_, _, Some(metadata))) => {
                metadata_add(&mut bytes, *metadata)?;
                None
            }
            Some(Recipe::Bulk(_, _, None)) => {
                // Unknown bulk producer captures retain their exact receipt.
                return Err(io::Error::from(io::ErrorKind::Unsupported));
            }
            // Trim spool storage keeps its existing document/source admission.
            _ => None,
        };
        if let Some(shapes) = self.latest.as_ref().and_then(|latest| latest.explicit.as_ref())
            && !pending_explicit.is_some_and(|pending| Arc::ptr_eq(pending, shapes))
        {
            metadata_add(&mut bytes, explicit_metadata_bytes(shapes)?)?;
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod document_metadata_tests {
    use super::*;

    #[test]
    fn pending_and_latest_explicit_recipe_count_one_shared_allocation_after_history_eviction() {
        let mut editor = Editor::new(Document::new());
        editor.set_undo_memory_limit(0);
        assert!(editor.set_journal_sink(DocumentJournalSink::closed_for_test()).is_ok());
        editor.insert_text("retained", Duration::ZERO).unwrap();
        assert!(editor.undo.is_empty());
        let Recipe::Explicit(shapes) = &editor.receipts.pending.as_ref().unwrap().recipe else {
            panic!("explicit fixture");
        };
        let expected = explicit_metadata_bytes(shapes).unwrap();
        assert!(Arc::ptr_eq(shapes, editor.receipts.latest.as_ref().unwrap().explicit.as_ref().unwrap()));
        assert_eq!(editor.receipts.document_metadata_bytes().unwrap(), expected);
    }

    #[test]
    fn production_bulk_producers_have_bounds_and_unknown_private_seed_is_typed() {
        let mut editor = Editor::new(Document::new());
        editor.insert_text("x  ", Duration::ZERO).unwrap();
        assert!(editor.set_journal_sink(DocumentJournalSink::closed_for_test()).is_ok());
        editor.trim_trailing_whitespace().unwrap();
        assert!(matches!(&editor.receipts.pending.as_ref().unwrap().recipe, Recipe::Bulk(_, _, Some(_))));
        assert!(editor.document_metadata_bytes().is_ok());

        let mut unknown = Editor::new(Document::new());
        assert!(unknown.set_journal_sink(DocumentJournalSink::closed_for_test()).is_ok());
        unknown.apply_descending((0..1).map(|_| Ok(Some(Edit::bytes(0..0, b"x"))))).unwrap();
        assert_eq!(unknown.document_metadata_bytes().unwrap_err().kind(), io::ErrorKind::Unsupported);
        assert!(unknown.has_pending_edit_receipt(), "unknown accounting never drops the exact retained receipt");
    }
}
// One metadata slot, independent of journal admission. Explicit recipes contain
// at most MAX_CURSORS shapes (24 bytes each), never snapshots or added bytes.
// Bulk factories and trim spools are borrowed from capped history on demand;
// retaining them here could destroy snapshots or unlink spools on the UI.
pub(super) struct LatestEdit {
    pub(super) before_revision: u64,
    pub(super) after_revision: u64,
    pub(super) kind: EditKind,
    pub(super) explicit: Option<Arc<Vec<Shape>>>,
}
pub(crate) struct SyntaxEdit {
    pub before_revision: u64,
    pub after_revision: u64,
    kind: EditKind,
    recipe: Recipe,
}
impl SyntaxEdit {
    pub(crate) fn receipt(&self, before: Snapshot, after: Snapshot) -> EditReceipt {
        EditReceipt {
            sequence: 0, // Syntax serials are deliberately separate from journal sequences.
            before_revision: self.before_revision,
            after_revision: self.after_revision,
            before,
            after,
            kind: self.kind,
            recipe: self.recipe.clone(),
            inverse: self.kind == EditKind::Undo,
        }
    }
}
impl Editor {
    pub(super) fn has_receipt_destination(&self) -> bool {
        self.receipts.observer.is_some() || self.receipts.sink.is_some()
    }
    #[cfg(test)]
    pub(crate) fn pending_edit_sequence(&self) -> Option<u64> {
        self.receipts.pending.as_ref().map(|receipt| receipt.sequence)
    }
    pub(crate) fn check_sealed_receipt_delivery(&self) -> Result<(), ReceiptDeliveryRefusal> {
        if self.receipts.observer.is_some() {
            Err(ReceiptDeliveryRefusal::UnsupportedObserver)
        } else if self.receipts.pending.is_some() {
            Err(if self.receipts.sink.is_some() {
                ReceiptDeliveryRefusal::Full
            } else {
                ReceiptDeliveryRefusal::MissingSink
            })
        } else {
            Ok(())
        }
    }
    pub(crate) fn set_journal_sink(&mut self, sink: DocumentJournalSink) -> Result<(), DocumentJournalSink> {
        if self.receipts.pending.is_some() || self.receipts.observer.is_some() {
            return Err(sink);
        }
        let Some(next) = self.receipts.destination_generation.checked_add(1) else {
            return Err(sink);
        };
        self.receipts.destination_generation = next;
        let previous = self.receipts.sink.replace(sink);
        drop(previous);
        Ok(())
    }
    pub(crate) fn flush_edit_receipt_sealed(
        &mut self,
        permit: Option<&DocumentContinuationPermit>,
    ) -> Result<(), ReceiptDeliveryRefusal> {
        if self.receipts.observer.is_some() {
            return Err(ReceiptDeliveryRefusal::UnsupportedObserver);
        }
        if self.receipts.pending.is_none() {
            return Ok(());
        }
        let Some(sink) = &mut self.receipts.sink else {
            return Err(ReceiptDeliveryRefusal::MissingSink);
        };
        sink.flush(&mut self.receipts.pending, permit)
    }
    /// Constant metadata work only. No recipe iteration, source reads or I/O.
    /// Missing/evicted history explicitly means that syntax must reset.
    pub(crate) fn syntax_edit(&self) -> (u64, Option<SyntaxEdit>) {
        let descriptor = (|| {
            let latest = self.receipts.latest.as_ref()?;
            if latest.after_revision != self.document.revision {
                return None;
            }
            let recipe = if let Some(shapes) = &latest.explicit {
                Recipe::Explicit(shapes.clone())
            } else {
                let entry = if latest.kind == EditKind::Undo { self.redo.back()? } else { self.undo.back()? };
                let (before, after) = if latest.kind == EditKind::Undo {
                    (entry.after.revision, entry.before.revision)
                } else {
                    (entry.before.revision, entry.after.revision)
                };
                if (before, after) != (latest.before_revision, latest.after_revision) {
                    return None;
                }
                entry.transition.recipe.clone()
            };
            Some(SyntaxEdit {
                before_revision: latest.before_revision,
                after_revision: latest.after_revision,
                kind: latest.kind,
                recipe,
            })
        })();
        (self.receipts.syntax_sequence, descriptor)
    }
    pub(super) fn admit_edit(&mut self) -> io::Result<()> {
        if self.compound.is_some() {
            return Ok(());
        }
        self.flush_edit_receipt()?;
        if self.has_pending_save_completion() {
            Err(io::Error::new(io::ErrorKind::WouldBlock, "finish the retained save completion first"))
        } else {
            Ok(())
        }
    }
    pub fn set_edit_observer(&mut self, observer: Option<EditObserver>) -> Result<(), EditObserverRefusal> {
        if self.has_pending_edit_receipt() {
            return Err(EditObserverRefusal { observer, reason: EditObserverRefusalReason::PendingReceipt });
        }
        if self.receipts.sink.is_some() {
            return Err(EditObserverRefusal { observer, reason: EditObserverRefusalReason::SealedSink });
        }
        self.install_edit_observer(observer)
    }
    fn install_edit_observer(&mut self, observer: Option<EditObserver>) -> Result<(), EditObserverRefusal> {
        let Some(next) = self.receipts.destination_generation.checked_add(1) else {
            return Err(EditObserverRefusal { observer, reason: EditObserverRefusalReason::GenerationExhausted });
        };
        self.receipts.destination_generation = next;
        let previous = std::mem::replace(&mut self.receipts.observer, observer);
        drop(previous);
        Ok(())
    }
    /// Preserve the ordinary-observer unwind fixture while routing even its
    /// deliberate pending-observer replacement through the checked identity.
    #[cfg(test)]
    pub(super) fn replace_observer_for_test(
        &mut self,
        observer: Option<EditObserver>,
    ) -> Result<(), EditObserverRefusal> {
        if self.receipts.sink.is_some() {
            return Err(EditObserverRefusal { observer, reason: EditObserverRefusalReason::SealedSink });
        }
        self.install_edit_observer(observer)
    }
    pub fn next_edit_sequence(&self) -> u64 {
        self.receipts.sequence + 1
    }
    pub fn has_pending_edit_receipt(&self) -> bool {
        self.receipts.pending.is_some()
    }
    /// Retry admission after the ordered journal worker frees queue capacity.
    pub fn flush_edit_receipt(&mut self) -> io::Result<()> {
        if self.receipts.sink.is_some() {
            return self.flush_edit_receipt_sealed(None).map_err(|e| io::Error::new(io::ErrorKind::WouldBlock, e));
        }
        if let Some(receipt) = self.receipts.pending.take() {
            self.receipts.pending = self.receipts.observer.as_mut().unwrap()(receipt).err();
        }
        if self.has_pending_edit_receipt() {
            Err(io::Error::new(io::ErrorKind::WouldBlock, "journal queue is full"))
        } else {
            Ok(())
        }
    }
    pub(super) fn publish(&mut self, transition: Transition, before: u64, after: u64, kind: EditKind) {
        self.receipts.syntax_sequence =
            self.receipts.syntax_sequence.checked_add(1).expect("syntax sequence exhausted");
        self.receipts.latest = Some(LatestEdit {
            before_revision: before,
            after_revision: after,
            kind,
            explicit: match &transition.recipe {
                Recipe::Explicit(shapes) => Some(shapes.clone()),
                _ => None,
            },
        });
        if !self.has_receipt_destination() {
            return;
        }
        self.receipts.sequence += 1;
        let inverse = kind == EditKind::Undo;
        let receipt = EditReceipt {
            sequence: self.receipts.sequence,
            before_revision: before,
            after_revision: after,
            before: if inverse { transition.after.clone() } else { transition.before.clone() },
            after: if inverse { transition.before } else { transition.after },
            kind,
            recipe: transition.recipe,
            inverse,
        };
        if let Some(sink) = &mut self.receipts.sink {
            self.receipts.pending = Some(receipt);
            let _ = sink.flush(&mut self.receipts.pending, None);
        } else if let Some(observer) = &mut self.receipts.observer {
            self.receipts.pending = observer(receipt).err();
        }
    }
    pub(super) fn charge_model_history(&mut self) {
        if let Some(entry) = self.undo.back() {
            let cost = std::mem::size_of::<HistoryEntry>()
                .saturating_add(
                    entry
                        .before
                        .cursors
                        .capacity()
                        .saturating_add(entry.after.cursors.capacity())
                        .saturating_mul(std::mem::size_of::<Cursor>()),
                )
                .saturating_add(entry.transition.recipe.cost());
            self.document.buffer.charge_latest_model_history(cost);
        }
        self.trim_history();
    }
}

#[cfg(test)]
mod destination_generation_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn observer(calls: &Arc<AtomicUsize>) -> EditObserver {
        let calls = calls.clone();
        Box::new(move |receipt| {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(receipt)
        })
    }
    fn owner(observer: &EditObserver) -> *const () {
        observer.as_ref() as *const _ as *const ()
    }

    #[test]
    fn every_observer_change_advances_checked_identity_and_exhaustion_retains_both_callbacks() {
        let mut editor = Editor::new(Document::new());
        let old_calls = Arc::new(AtomicUsize::new(0));
        let incoming_calls = Arc::new(AtomicUsize::new(0));
        assert_eq!(editor.receipts.destination_generation(), 0);
        editor.set_edit_observer(None).unwrap();
        editor.set_edit_observer(Some(observer(&old_calls))).unwrap();
        editor.set_edit_observer(Some(observer(&old_calls))).unwrap();
        editor.set_edit_observer(None).unwrap();
        editor.set_edit_observer(None).unwrap();
        assert_eq!(editor.receipts.destination_generation(), 5, "removal and replacement advance even for equal tags");
        editor.raw_reopen_ready().unwrap();
        editor.install_raw_reopen(Document::new()); // the test worker is off UI
        assert_eq!(
            editor.receipts.destination_generation(),
            6,
            "worker raw-reopen reset never restarts the generation"
        );
        editor.set_edit_observer(Some(observer(&old_calls))).unwrap();
        let previous = owner(editor.receipts.observer.as_ref().unwrap());
        editor.receipts.destination_generation = u64::MAX; // deterministic exhaustion gate
        let incoming = observer(&incoming_calls);
        let incoming_owner = owner(&incoming);
        let refusal = editor.set_edit_observer(Some(incoming)).unwrap_err();
        assert_eq!(refusal.reason, EditObserverRefusalReason::GenerationExhausted);
        assert_eq!(refusal.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(owner(refusal.observer.as_ref().unwrap()), incoming_owner);
        assert_eq!(owner(editor.receipts.observer.as_ref().unwrap()), previous);
        assert_eq!(Arc::strong_count(&incoming_calls), 2, "refused callback still owns its exact capture");
        assert_eq!(incoming_calls.load(Ordering::SeqCst), 0);
        assert_eq!(editor.raw_reopen_ready().unwrap_err().kind(), io::ErrorKind::InvalidInput);
        let removal = editor.set_edit_observer(None).unwrap_err();
        assert_eq!(removal.reason, EditObserverRefusalReason::GenerationExhausted);
        assert!(removal.observer.is_none());
        assert_eq!(owner(editor.receipts.observer.as_ref().unwrap()), previous);
        assert_eq!(editor.receipts.destination_generation(), u64::MAX);
    }

    #[test]
    fn exhausted_sink_identity_returns_the_sink_and_preserves_previous_destination() {
        let mut editor = Editor::new(Document::new());
        editor.receipts.destination_generation = u64::MAX;
        let returned = match editor.set_journal_sink(DocumentJournalSink::closed_for_test()) {
            Err(sink) => sink,
            Ok(()) => panic!("exhausted destination must refuse without taking the incoming sink"),
        };
        assert!(!editor.has_receipt_destination());
        assert_eq!(editor.receipts.destination_generation(), u64::MAX);
        editor.receipts.destination_generation = u64::MAX - 1; // test-only admission gate reset
        assert!(editor.set_journal_sink(returned).is_ok(), "exact returned sink remains usable");
        assert_eq!(editor.receipts.destination_generation(), u64::MAX);
        let previous = editor.receipts.sink.as_ref().unwrap() as *const DocumentJournalSink;
        let refused = match editor.set_journal_sink(DocumentJournalSink::closed_for_test()) {
            Err(sink) => sink,
            Ok(()) => panic!("sink replacement cannot wrap destination identity"),
        };
        assert_eq!(editor.receipts.sink.as_ref().unwrap() as *const DocumentJournalSink, previous);
        assert_eq!(editor.receipts.destination_generation(), u64::MAX);
        assert!(!editor.has_pending_edit_receipt());
        drop(refused); // explicit retirement on this test worker
    }

    #[test]
    fn pending_receipt_and_sealed_sink_refusals_retain_exact_incoming_observer() {
        let mut editor = Editor::new(Document::new());
        let calls = Arc::new(AtomicUsize::new(0));
        editor.set_edit_observer(Some(observer(&calls))).unwrap();
        editor.insert_text("pending", Duration::ZERO).unwrap();
        let sequence = editor.pending_edit_sequence();
        let generation = editor.receipts.destination_generation();
        let incoming = observer(&calls);
        let incoming_owner = owner(&incoming);
        let refusal = editor.set_edit_observer(Some(incoming)).unwrap_err();
        assert_eq!(refusal.reason, EditObserverRefusalReason::PendingReceipt);
        assert_eq!(refusal.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(owner(refusal.observer.as_ref().unwrap()), incoming_owner);
        assert_eq!(editor.pending_edit_sequence(), sequence);
        assert_eq!(editor.receipts.destination_generation(), generation);

        let mut sealed = Editor::new(Document::new());
        assert!(sealed.set_journal_sink(DocumentJournalSink::closed_for_test()).is_ok());
        let generation = sealed.receipts.destination_generation();
        let incoming = observer(&calls);
        let incoming_owner = owner(&incoming);
        let refusal = sealed.set_edit_observer(Some(incoming)).unwrap_err();
        assert_eq!(refusal.reason, EditObserverRefusalReason::SealedSink);
        assert_eq!(owner(refusal.observer.as_ref().unwrap()), incoming_owner);
        assert_eq!(sealed.receipts.destination_generation(), generation);
        assert!(sealed.has_receipt_destination());
    }
}

#[cfg(test)]
mod destination_unwind_tests {
    use super::*;
    #[test]
    fn replaced_observer_drop_unwind_still_invalidates_preparation_identity() {
        struct PanicOnDrop;
        impl Drop for PanicOnDrop {
            fn drop(&mut self) {
                panic!("controlled ordinary observer capture retirement");
            }
        }
        let mut editor = Editor::new(Document::new());
        let capture = PanicOnDrop;
        editor
            .set_edit_observer(Some(Box::new(move |receipt| {
                let _ = &capture;
                Err(receipt)
            })))
            .unwrap();
        let generation = editor.receipts.destination_generation();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| editor.set_edit_observer(None)));
        assert!(result.is_err());
        assert_eq!(editor.receipts.destination_generation(), generation + 1);
        assert!(editor.receipts.observer.is_none(), "replacement owner is installed before retiring old callback");
    }
}
