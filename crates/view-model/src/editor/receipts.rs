//! Exact raw-byte replacements. Publishing only queues immutable references;
//! recipe iteration and snapshot reads belong on the journal worker.
use super::*;
use kwil_core_workspace::{SavedSnapshot, TrimSpool};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditKind {
    Edit,
    Undo,
    Redo,
    WorkerTrim,
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
    pub fn replacement_count(&self) -> u64 {
        match &self.recipe {
            Recipe::Explicit(shapes) => shapes.len() as u64,
            Recipe::Bulk(_, count) => *count,
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

#[derive(Clone)]
pub(super) struct Shape {
    pub range: Range<u64>,
    pub inserted: u64,
}
type ShapeIter = Box<dyn DoubleEndedIterator<Item = io::Result<Shape>> + Send>;
#[derive(Clone)]
pub(super) enum Recipe {
    Explicit(Arc<Vec<Shape>>),
    Bulk(Arc<dyn Fn() -> ShapeIter + Send + Sync>, u64),
    Trim(TrimSpool),
}
impl Recipe {
    pub(super) fn explicit(edits: &[Edit]) -> Self {
        Self::Explicit(Arc::new(
            edits.iter().map(|e| Shape { range: e.range.clone(), inserted: e.insert.len() }).collect(),
        ))
    }
    // Bulk seeds yield descending coordinates; all other sources are ascending.
    pub(super) fn bulk<I>(seed: I, count: u64) -> Self
    where
        I: DoubleEndedIterator<Item = io::Result<Option<Edit>>> + Clone + Send + Sync + 'static,
    {
        Self::Bulk(
            Arc::new(move || {
                Box::new(seed.clone().rev().filter_map(|result| match result {
                    Ok(Some(e)) => Some(Ok(Shape { range: e.range, inserted: e.insert.len() })),
                    Ok(None) => None,
                    Err(e) => Some(Err(e)),
                }))
            }),
            count,
        )
    }
    fn visit(&self, descending: bool, f: &mut dyn FnMut(Shape) -> io::Result<()>) -> io::Result<()> {
        let items: ShapeIter = match self {
            Self::Explicit(shapes) => {
                // Cloning the Arc, not a per-edit vector on each iteration.
                let shapes = shapes.clone();
                Box::new((0..shapes.len()).map(move |i| Ok(shapes[i].clone())))
            }
            Self::Bulk(factory, _) => factory(),
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
    fn cost(&self) -> usize {
        match self {
            Self::Explicit(shapes) => shapes.capacity() * std::mem::size_of::<Shape>(),
            Self::Bulk(_, _) => 4096 + MAX_CURSORS * 32,
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
#[derive(Default)]
pub(super) struct ReceiptState {
    observer: Option<EditObserver>,
    pending: Option<EditReceipt>,
    sequence: u64,
    pub deferred_save: Option<SavedSnapshot>,
}
impl Editor {
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
    pub fn set_edit_observer(&mut self, observer: Option<EditObserver>) -> io::Result<()> {
        if self.has_pending_edit_receipt() {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "pending edit receipt must be delivered first"));
        }
        self.receipts.observer = observer;
        Ok(())
    }
    pub fn next_edit_sequence(&self) -> u64 {
        self.receipts.sequence + 1
    }
    pub fn has_pending_edit_receipt(&self) -> bool {
        self.receipts.pending.is_some()
    }
    /// Retry admission after the ordered journal worker frees queue capacity.
    pub fn flush_edit_receipt(&mut self) -> io::Result<()> {
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
        let Some(observer) = &mut self.receipts.observer else {
            return;
        };
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
        self.receipts.pending = observer(receipt).err();
    }
    pub(super) fn charge_model_history(&mut self) {
        if let Some(entry) = self.undo.back() {
            let cost = std::mem::size_of::<HistoryEntry>()
                + (entry.before.cursors.capacity() + entry.after.cursors.capacity()) * std::mem::size_of::<Cursor>()
                + entry.transition.recipe.cost();
            self.document.buffer.charge_latest_model_history(cost);
        }
        self.trim_history();
    }
}
