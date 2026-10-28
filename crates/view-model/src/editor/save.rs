use super::receipts::Shape;
use super::*;

use kwil_core_workspace::{SaveRequest, SavedSnapshot};
use std::path::PathBuf;

use crate::visual_rows::{AllocationBudget, MetadataReservation};
use kwil_core_text::PreparedSavedInstall;
use std::ops::ControlFlow;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveCompletionRefusalReason {
    Occupied,
    RetentionAdmission,
    ApplyFailed,
}
/// Inline ownership-complete refusal. Retain or retire this on the authority
/// worker; formatting an ordinary I/O error never conceals a source owner.
pub struct SaveCompletionRefusal {
    pub reason: SaveCompletionRefusalReason,
    pub result: io::Result<SavedSnapshot>,
    pub error: Option<io::Error>,
    _retention_charge: Option<MetadataReservation>,
}

/// Install this inline in an already admitted guarded slot before preparing.
/// Drop only off UI: both the saved owner and replaced source roots stay here.
pub(crate) struct SaveCompletionState {
    pub(super) saved: SavedSnapshot,
    prepared: Option<PreparedSave>,
    committed: bool,
    retention_charge: Option<MetadataReservation>,
    #[cfg(test)]
    panic_after_core: bool,
    #[cfg(test)]
    panic_before_publication: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SaveRetentionRefusal {
    EmptyInput,
    Occupied,
    PublicationStarted,
    AdmissionRequired,
    NotCompleted,
}
/// Source-free refusal. The exact unpublished state remains in its guard until
/// the host transfers it to the original result slot for worker retirement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SavePreparationRefusal {
    StaleIdentity,
}
impl std::fmt::Display for SavePreparationRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for SavePreparationRefusal {}
fn stale_save_preparation() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, SavePreparationRefusal::StaleIdentity)
}
/// Temporarily borrows the same deferred slot. Unwind restores its exact
/// whole/partial state; this is not a second saved-result owner or a replay.
struct DeferredSaveGuard<'a> {
    editor: &'a mut Editor,
    state: Option<SaveCompletionState>,
}
impl Drop for DeferredSaveGuard<'_> {
    fn drop(&mut self) {
        self.editor.receipts.deferred_save = self.state.take();
    }
}
impl SaveCompletionState {
    pub(crate) fn new(saved: SavedSnapshot) -> Self {
        Self {
            saved,
            prepared: None,
            committed: false,
            retention_charge: None,
            #[cfg(test)]
            panic_after_core: false,
            #[cfg(test)]
            panic_before_publication: false,
        }
    }
    pub(crate) fn is_committed(&self) -> bool {
        self.committed
    }
    pub(crate) fn admit_retention(&mut self, budget: &AllocationBudget) -> io::Result<()> {
        if self.retention_charge.is_none() {
            // The transport's reusable slot charge cannot account for a state
            // that persists in Editor after that slot is acknowledged.
            let bytes = std::mem::size_of::<Self>()
                .checked_mul(2)
                .ok_or_else(|| invalid_save("save retention metadata overflow"))?;
            self.retention_charge = Some(budget.reserve_metadata(bytes).map_err(io::Error::other)?);
        }
        Ok(())
    }
    pub(crate) fn publication_started(&self) -> bool {
        self.committed
            || self.prepared.as_ref().is_some_and(|p| p.core.as_ref().is_some_and(PreparedSavedInstall::is_committed))
    }
    /// WORKER-ONLY: retire stale preparation before retrying this exact saved
    /// result. The original SavedSnapshot and its persistent header reservation
    /// stay guarded here. Publication is monotonic and can never be reset.
    pub(crate) fn reset_preparation_for_retry(&mut self) -> Result<(), SaveRetentionRefusal> {
        if self.publication_started() {
            return Err(SaveRetentionRefusal::PublicationStarted);
        }
        self.prepared = None;
        Ok(())
    }
    #[cfg(test)]
    pub(super) fn prepared_cursor_allocation_for_test(&self) -> Option<(*const Cursor, usize)> {
        self.prepared.as_ref().map(|prepared| (prepared.cursors.as_ptr(), prepared.cursors.capacity()))
    }
    #[cfg(test)]
    pub(super) fn panic_after_core(&mut self) {
        self.panic_after_core = true;
    }
    #[cfg(test)]
    pub(super) fn panic_before_publication(&mut self) {
        self.panic_before_publication = true;
    }
}

struct PreparedSave {
    core: Option<PreparedSavedInstall>,
    original: Option<kwil_core_text::TextFile>,
    path: Option<PathBuf>,
    cursors: Vec<Cursor>,
    primary: usize,
    history: Option<HistoryEntry>,
    replacement_undo: VecDeque<HistoryEntry>,
    retired_undo: VecDeque<HistoryEntry>,
    retired_redo: VecDeque<HistoryEntry>,
    receipt: Option<receipts::EditReceipt>,
    latest: Option<receipts::LatestEdit>,
    outcome: CommandResult,
    top: u64,
    revision: u64,
    next_revision: u64,
    saved_revision: u64,
    syntax_sequence: u64,
    sequence: u64,
    expected: SaveIdentity,
    changed: bool,
    sealed: bool,
    // Retired storage remains charged independently of metadata installed in Editor.
    _residual_charge: MetadataReservation,
    live_charge: Option<MetadataReservation>,
}
#[derive(PartialEq, Eq)]
struct SaveIdentity {
    revision: u64,
    next_revision: u64,
    saved_revision: u64,
    source_epoch: u64,
    cursors: Option<u64>,
    cursor_capacity: usize,
    primary: usize,
    box_column: Option<u64>,
    destination_generation: u64,
    top: u64,
    rows: usize,
    sequence: u64,
    syntax_sequence: u64,
    undo: usize,
    redo: usize,
}
fn invalid_save(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
pub(super) fn reserve_vec<T>(count: usize) -> io::Result<Vec<T>> {
    let mut values = Vec::new();
    values.try_reserve_exact(count).map_err(io::Error::other)?;
    if values.capacity() > count.saturating_mul(2) {
        return Err(invalid_save("allocator exceeded admitted saved metadata"));
    }
    Ok(values)
}
fn copy_cursors(cursors: &[Cursor]) -> io::Result<Vec<Cursor>> {
    let mut result = reserve_vec(cursors.len())?;
    result.extend_from_slice(cursors);
    Ok(result)
}

impl Editor {
    /// Release the original result slot without releasing this save's owner.
    /// Both owners are borrowed until every refusal check is complete. A
    /// successful swap makes the existing mutation gate observe pending save
    /// work immediately; only a real capacity event should drive its retry.
    pub(crate) fn retain_save_completion(
        &mut self,
        state: &mut Option<SaveCompletionState>,
    ) -> Result<(), SaveRetentionRefusal> {
        let Some(saved) = state.as_ref() else {
            return Err(SaveRetentionRefusal::EmptyInput);
        };
        if self.receipts.deferred_save.is_some() {
            return Err(SaveRetentionRefusal::Occupied);
        }
        // Includes completed model publication and core publication that
        // preceded an unwind; neither phase may be deferred for replay.
        if saved.publication_started() {
            return Err(SaveRetentionRefusal::PublicationStarted);
        }
        if saved.retention_charge.is_none() {
            return Err(SaveRetentionRefusal::AdmissionRequired);
        }
        std::mem::swap(&mut self.receipts.deferred_save, state);
        Ok(())
    }
    pub(crate) fn has_completed_save_completion(&self) -> bool {
        self.receipts.deferred_save.as_ref().is_some_and(SaveCompletionState::is_committed)
    }
    /// Transfer an unpublished state to an admitted destination in the ORIGINAL
    /// result slot. The host retains its save flight until worker retirement or
    /// reset/re-admission; this operation neither clears saving nor drops roots.
    pub(crate) fn take_unpublished_save_completion(
        &mut self,
        destination: &mut Option<SaveCompletionState>,
    ) -> Result<(), SaveRetentionRefusal> {
        let Some(state) = self.receipts.deferred_save.as_ref() else {
            return Err(SaveRetentionRefusal::EmptyInput);
        };
        if destination.is_some() {
            return Err(SaveRetentionRefusal::Occupied);
        }
        if state.publication_started() {
            return Err(SaveRetentionRefusal::PublicationStarted);
        }
        std::mem::swap(&mut self.receipts.deferred_save, destination);
        Ok(())
    }
    /// Transfer into a preadmitted destination in the ORIGINAL result slot.
    /// Only the existing document worker may retire that completed residual.
    pub(crate) fn take_completed_save_completion(
        &mut self,
        destination: &mut Option<SaveCompletionState>,
    ) -> Result<(), SaveRetentionRefusal> {
        let Some(state) = self.receipts.deferred_save.as_ref() else {
            return Err(SaveRetentionRefusal::EmptyInput);
        };
        if destination.is_some() {
            return Err(SaveRetentionRefusal::Occupied);
        }
        if !state.is_committed() {
            return Err(SaveRetentionRefusal::NotCompleted);
        }
        std::mem::swap(&mut self.receipts.deferred_save, destination);
        Ok(())
    }
    fn save_identity(&self) -> SaveIdentity {
        SaveIdentity {
            revision: self.document.revision,
            next_revision: self.document.next_revision,
            saved_revision: self.document.saved_revision,
            source_epoch: self.document.source_epoch,
            cursors: self.external_cursor_generation,
            cursor_capacity: self.cursors.capacity(),
            primary: self.primary,
            box_column: self.box_column,
            destination_generation: self.receipts.destination_generation(),
            top: self.top,
            rows: self.rows,
            sequence: self.receipts.sequence,
            syntax_sequence: self.receipts.syntax_sequence,
            undo: self.undo.len(),
            redo: self.redo.len(),
        }
    }
    fn prepared_save_matches(&self, prepared: &PreparedSave) -> bool {
        self.external_cursor_generation.is_some()
            && self.compound.is_none()
            && self.save_identity() == prepared.expected
            && prepared.core.as_ref().is_none_or(|core| self.document.buffer.saved_install_matches(core))
    }
    pub(crate) fn prepare_save_completion(
        &mut self,
        state: &mut SaveCompletionState,
        budget: &AllocationBudget,
    ) -> io::Result<()> {
        self.prepare_save_completion_inner(state, budget, true)
    }
    fn prepare_save_completion_inner(
        &mut self,
        state: &mut SaveCompletionState,
        budget: &AllocationBudget,
        sealed: bool,
    ) -> io::Result<()> {
        if state.committed {
            return Err(invalid_save("save completion already committed"));
        }
        if let Some(prepared) = &mut state.prepared {
            if prepared.core.as_ref().is_some_and(PreparedSavedInstall::is_committed) {
                return Err(invalid_save("partial saved installation must not replay"));
            }
            if !self.prepared_save_matches(prepared) {
                return Err(stale_save_preparation());
            }
            if sealed {
                self.check_sealed_receipt_delivery().map_err(|e| io::Error::new(io::ErrorKind::WouldBlock, e))?;
                prepared.sealed = true;
            }
            return Ok(());
        }
        if self.external_cursor_generation.is_none() {
            return Err(stale_save_preparation());
        }
        if self.cursors.is_empty() || self.cursors.len() > MAX_CURSORS || self.primary >= self.cursors.len() {
            return Err(invalid_save("invalid cursor set for saved installation"));
        }
        if sealed {
            self.check_sealed_receipt_delivery().map_err(|e| io::Error::new(io::ErrorKind::WouldBlock, e))?;
        }
        if self.compound.is_some() {
            return Err(invalid_save("save completion during compound edit"));
        }
        let saved = &state.saved;
        let current = self.document.revision == saved.revision;
        let transformed = saved.normalized || saved.trimmed_rows.is_some();
        let changed = current && transformed;
        if changed && self.receipts.pending.is_some() {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "save completion awaits exact journal receipt"));
        }
        let next_revision = if transformed {
            self.document.next_revision.checked_add(1).ok_or_else(|| invalid_save("revision exhausted"))?
        } else {
            self.document.next_revision
        };
        let syntax_sequence = if changed {
            self.receipts.syntax_sequence.checked_add(1).ok_or_else(|| invalid_save("syntax sequence exhausted"))?
        } else {
            self.receipts.syntax_sequence
        };
        let emits = changed && self.has_receipt_destination();
        let sequence = if emits {
            self.receipts.sequence.checked_add(1).ok_or_else(|| invalid_save("receipt sequence exhausted"))?
        } else {
            self.receipts.sequence
        };
        let warning_bytes = saved
            .warnings
            .iter()
            .try_fold(0usize, |n, s| n.checked_add(s.len()).and_then(|n| n.checked_add(2)))
            .ok_or_else(|| invalid_save("warning size overflow"))?;
        let history_count = self
            .undo
            .capacity()
            .checked_add(self.redo.capacity())
            .and_then(|n| n.checked_add(2))
            .ok_or_else(|| invalid_save("history size overflow"))?;
        // Normalization can collapse len while retaining the full allocation.
        // Publication swaps that old Vec into this guarded residual, so its
        // ACTUAL capacity must stay independently charged until worker retirement.
        let retired_cursor_bytes = if changed {
            self.cursors
                .capacity()
                .checked_mul(std::mem::size_of::<Cursor>())
                .ok_or_else(|| invalid_save("retired cursor metadata overflow"))?
        } else {
            0
        };
        let model_bytes = self
            .cursors
            .len()
            .checked_mul(8 * std::mem::size_of::<Cursor>())
            .and_then(|n| n.checked_add(history_count.checked_mul(4 * std::mem::size_of::<HistoryEntry>())?))
            .and_then(|n| {
                n.checked_add(
                    std::mem::size_of::<PreparedSave>()
                        + 2 * std::mem::size_of::<Shape>()
                        + std::mem::size_of::<Vec<Shape>>()
                        + 2 * std::mem::size_of::<usize>(),
                )
            })
            .and_then(|n| n.checked_add(self.rows.checked_mul(2 * (std::mem::size_of::<usize>() + 1))?))
            .and_then(|n| n.checked_add(saved.path.as_os_str().len().checked_mul(4)?))
            .and_then(|n| n.checked_add(warning_bytes))
            .ok_or_else(|| invalid_save("save metadata overflow"))?;
        let core_bytes = self.document.buffer.saved_install_metadata_bytes()?;
        let residual_charge = budget
            .reserve_metadata(
                model_bytes
                    .checked_add(core_bytes)
                    .and_then(|bytes| bytes.checked_add(retired_cursor_bytes))
                    .ok_or_else(|| invalid_save("save metadata overflow"))?,
            )
            .map_err(io::Error::other)?;
        let live_charge = budget
            .reserve_metadata(
                model_bytes.checked_add(core_bytes).ok_or_else(|| invalid_save("save metadata overflow"))?,
            )
            .map_err(io::Error::other)?;
        let mut cursors = copy_cursors(&self.cursors)?;
        let mut primary = self.primary;
        let mut history = None;
        let mut receipt = None;
        let mut latest = None;
        let mut top = self.top;
        let mut damage = Damage { status: true, ..Damage::default() };
        let revision = if changed { self.document.next_revision } else { self.document.revision };
        if changed {
            let source = self.document.snapshot();
            let before = State {
                cursors: copy_cursors(&self.cursors)?,
                primary: self.primary,
                revision: self.document.revision,
                box_column: self.box_column,
            };
            if sealed && !saved.normalized && saved.trim_deletions.is_none() {
                return Err(io::Error::other("trim completion lacks deletion spool"));
            }
            if !sealed {
                self.note_external_cursor_change();
            }
            let remapped = remap_saved_cursors(&mut cursors, &source, saved);
            if !sealed {
                // Preserve the ordinary API's historical partial endpoint
                // publication on failure without moving a new allocation into
                // the live model or losing the original saved owner.
                self.cursors.clone_from_slice(&cursors);
            }
            remapped?;
            if !saved.normalized && saved.trim_deletions.is_none() {
                return Err(io::Error::other("trim completion lacks deletion spool"));
            }
            normalize_cursor_set(&mut cursors, &mut primary);
            let recipe = if saved.normalized {
                let mut shapes = reserve_vec(1)?;
                shapes.push(Shape { range: 0..source.len(), inserted: saved.snapshot.len() });
                Recipe::Explicit(Arc::new(shapes))
            } else {
                Recipe::Trim(saved.trim_deletions.as_ref().unwrap().clone())
            };
            let transition = Transition { before: source, after: saved.snapshot.clone(), recipe };
            let after = State { cursors: copy_cursors(&cursors)?, primary, revision, box_column: None };
            let rows = if saved.normalized {
                0..transition.before.line_count().max(saved.snapshot.line_count())
            } else {
                saved.trimmed_rows.clone().unwrap()
            };
            let kind = if saved.normalized { EditKind::WorkerSave } else { EditKind::WorkerTrim };
            latest = Some(receipts::LatestEdit {
                before_revision: before.revision,
                after_revision: revision,
                kind,
                explicit: match &transition.recipe {
                    Recipe::Explicit(shapes) => Some(shapes.clone()),
                    _ => None,
                },
            });
            if emits {
                receipt = Some(receipts::prepared_save_receipt(sequence, before.revision, revision, kind, &transition));
            }
            if saved.normalized {
                damage.full = true;
            } else {
                damage = evaluate_cursor_damage(
                    &saved.snapshot,
                    &cursors,
                    primary,
                    &mut top,
                    self.rows,
                    CursorDamageBefore::Source {
                        snapshot: &transition.before,
                        cursors: &before.cursors,
                        top: self.top,
                    },
                    Some(&rows),
                )?;
            }
            history = Some(HistoryEntry { before, after, edited: rows, structural: saved.normalized, transition });
        }
        let history_cost = history.as_ref().map_or(0, |entry| {
            std::mem::size_of::<HistoryEntry>()
                .saturating_add(
                    entry
                        .before
                        .cursors
                        .capacity()
                        .saturating_add(entry.after.cursors.capacity())
                        .saturating_mul(std::mem::size_of::<Cursor>()),
                )
                .saturating_add(entry.transition.recipe.cost())
        });
        let core = if current {
            Some(self.document.buffer.prepare_saved_install(&saved.snapshot, changed, history_cost)?)
        } else {
            None
        };
        let mut retired_undo = VecDeque::new();
        retired_undo
            .try_reserve_exact(self.undo.len().checked_add(1).ok_or_else(|| invalid_save("history size overflow"))?)
            .map_err(io::Error::other)?;
        if retired_undo.capacity() > self.undo.len().saturating_add(1).saturating_mul(2) {
            return Err(invalid_save("allocator exceeded admitted saved history metadata"));
        }
        let mut replacement_undo = VecDeque::new();
        if changed {
            let count = self.undo.len().checked_add(1).ok_or_else(|| invalid_save("history size overflow"))?;
            replacement_undo.try_reserve_exact(count).map_err(io::Error::other)?;
            if replacement_undo.capacity() > count.saturating_mul(2) {
                return Err(invalid_save("allocator exceeded admitted saved undo metadata"));
            }
        }
        let status = if saved.warnings.is_empty() {
            CommandStatus::Applied
        } else {
            let mut warnings = String::new();
            warnings.try_reserve_exact(warning_bytes).map_err(io::Error::other)?;
            for (index, warning) in saved.warnings.iter().enumerate() {
                if index > 0 {
                    warnings.push_str("; ");
                }
                warnings.push_str(warning);
            }
            CommandStatus::Message(warnings)
        };
        state.prepared = Some(PreparedSave {
            core,
            original: Some(saved.file.clone()),
            path: Some(saved.path.clone()),
            cursors,
            primary,
            history,
            replacement_undo,
            retired_undo,
            retired_redo: VecDeque::new(),
            receipt,
            latest,
            outcome: CommandResult { damage, status },
            top,
            revision,
            next_revision,
            saved_revision: if transformed {
                if current { revision } else { self.document.next_revision }
            } else {
                saved.revision
            },
            syntax_sequence,
            sequence,
            expected: self.save_identity(),
            changed,
            sealed,
            _residual_charge: residual_charge,
            live_charge: Some(live_charge),
        });
        Ok(())
    }
    pub(crate) fn commit_save_completion(&mut self, state: &mut SaveCompletionState) -> io::Result<CommandResult> {
        if state.committed {
            return Err(invalid_save("save completion already committed"));
        }
        let prepared = state.prepared.as_mut().ok_or_else(|| invalid_save("save completion not prepared"))?;
        if prepared.core.as_ref().is_some_and(PreparedSavedInstall::is_committed) {
            return Err(invalid_save("partial saved installation must not replay"));
        }
        if !self.prepared_save_matches(prepared) {
            return Err(stale_save_preparation());
        }
        if prepared.changed && self.receipts.pending.is_some() {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "save completion awaits exact journal receipt"));
        }
        if prepared.sealed {
            self.check_sealed_receipt_delivery().map_err(|e| io::Error::new(io::ErrorKind::WouldBlock, e))?;
        }
        // Core refuses before changing anything; after it succeeds there are
        // only admitted moves, swaps and scalar updates. Retain every old owner.
        #[cfg(test)]
        if state.panic_before_publication {
            panic!("controlled save panic before publication");
        }
        if let Some(core) = &mut prepared.core {
            self.document.buffer.commit_saved_install(core)?;
        }
        if prepared.changed {
            self.note_external_cursor_change();
        }
        #[cfg(test)]
        if state.panic_after_core {
            panic!("controlled save panic after core installation");
        }
        if prepared.changed {
            std::mem::swap(&mut self.cursors, &mut prepared.cursors);
            self.primary = prepared.primary;
            self.box_column = None;
            std::mem::swap(&mut self.redo, &mut prepared.retired_redo);
            std::mem::swap(&mut self.undo, &mut prepared.replacement_undo);
            while let Some(history) = prepared.replacement_undo.pop_front() {
                self.undo.push_back(history);
            }
            if let Some(history) = prepared.history.take() {
                self.undo.push_back(history);
            }
            let (undo, _) = self.document.buffer.history_depth();
            while self.undo.len() > undo {
                if let Some(entry) = self.undo.pop_front() {
                    prepared.retired_undo.push_back(entry);
                }
            }
            std::mem::swap(&mut self.receipts.latest, &mut prepared.latest);
            std::mem::swap(&mut self.receipts.pending, &mut prepared.receipt);
        }
        std::mem::swap(&mut self.receipts.save_charge, &mut prepared.live_charge);
        std::mem::swap(&mut self.document.original, &mut prepared.original);
        std::mem::swap(&mut self.document.path, &mut prepared.path);
        self.document.revision = prepared.revision;
        self.document.next_revision = prepared.next_revision;
        self.document.saved_revision = prepared.saved_revision;
        self.receipts.syntax_sequence = prepared.syntax_sequence;
        self.receipts.sequence = prepared.sequence;
        self.top = prepared.top;
        self.saving = false;
        state.committed = true;
        Ok(std::mem::replace(
            &mut prepared.outcome,
            CommandResult { damage: Damage::default(), status: CommandStatus::Applied },
        ))
    }
}

fn remap_saved_cursors(cursors: &mut [Cursor], source: &Snapshot, saved: &SavedSnapshot) -> io::Result<()> {
    for cursor in cursors {
        for position in [&mut cursor.anchor, &mut cursor.head] {
            if saved.normalized {
                *position = (*position).min(saved.snapshot.len());
                for _ in 0..3 {
                    if text::byte(&saved.snapshot, *position)?.is_some_and(|b| b & 0xc0 == 0x80) {
                        *position += 1;
                    } else {
                        break;
                    }
                }
                if *position > 0
                    && text::byte(&saved.snapshot, *position)? == Some(b'\n')
                    && text::byte(&saved.snapshot, *position - 1)? == Some(b'\r')
                {
                    *position -= 1;
                }
            } else {
                let (row, column) = source
                    .offset_to_line_col(*position)
                    .map_err(io::Error::other)?
                    .ok_or_else(|| io::Error::other("cursor outside source"))?;
                let bounds = text::line(&saved.snapshot, row)?;
                *position = bounds.start.saturating_add(column).min(bounds.end);
            }
        }
        cursor.desired_column = None;
    }
    Ok(())
}
impl Editor {
    /// O(1) request creation, including trim-on-save. Workers scan/transform
    /// the immutable source while writing, then return a rebased result.
    pub fn save_request(&mut self, path: Option<PathBuf>) -> io::Result<SaveRequest> {
        if self.receipts.deferred_save.is_some() {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "retire the retained save completion first"));
        }
        if self.saving {
            return Err(io::Error::other("save already in progress"));
        }
        let path = path
            .or_else(|| self.document.path.clone())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "save requires a path"))?;
        let target_markdown = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "md" | "markdown" | "mdown" | "mkd"));
        self.end_burst();
        self.saving = true;
        Ok(SaveRequest {
            compression: self.document.compression,
            snapshot: self.document.snapshot(),
            path,
            original: self.document.original.clone(),
            revision: self.document.revision,
            trim_trailing_whitespace: self.settings.editor.trim_trailing_whitespace_on_save
                && !self.is_markdown()
                && !target_markdown,
        })
    }

    /// Complete on the UI/model thread without rescanning/copying file bytes.
    /// Concurrent edits stay dirty; their original pieces were pinned by save.
    /// A byte-equivalent current version rebases in O(1), preserving history.
    pub fn finish_save(
        &mut self,
        result: io::Result<SavedSnapshot>,
    ) -> ControlFlow<SaveCompletionRefusal, io::Result<CommandResult>> {
        let saved = match result {
            Ok(saved) => saved,
            Err(error) => {
                self.saving = false;
                return ControlFlow::Continue(Err(error));
            }
        };
        if self.receipts.deferred_save.is_some() {
            return ControlFlow::Break(SaveCompletionRefusal {
                reason: SaveCompletionRefusalReason::Occupied,
                result: Ok(saved),
                error: None,
                _retention_charge: None,
            });
        }
        let mut state = SaveCompletionState::new(saved);
        let budget = AllocationBudget::default();
        if let Err(error) = state.admit_retention(&budget) {
            return ControlFlow::Break(SaveCompletionRefusal {
                reason: SaveCompletionRefusalReason::RetentionAdmission,
                result: Ok(state.saved),
                error: Some(error),
                _retention_charge: None,
            });
        }
        // The original saved owner is guarded before any observer or source
        // lookup. Unwind restores the same slot, including completed residuals.
        let mut guard = DeferredSaveGuard { editor: self, state: Some(state) };
        let saved = &guard.state.as_ref().unwrap().saved;
        let needs_flush =
            guard.editor.document.revision == saved.revision && (saved.normalized || saved.trimmed_rows.is_some());
        if needs_flush && guard.editor.flush_edit_receipt().is_err() {
            return ControlFlow::Continue(Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "save completion awaits journal admission",
            )));
        }
        guard.editor.saving = false;
        let prepared = guard.editor.prepare_save_completion_inner(guard.state.as_mut().unwrap(), &budget, false);
        let outcome = prepared.and_then(|()| guard.editor.commit_save_completion(guard.state.as_mut().unwrap()));
        match outcome {
            Ok(result) => {
                // Ordinary observer compatibility only. The completed save and
                // every replaced root remain in the guard even if it unwinds.
                if needs_flush {
                    let _ = guard.editor.flush_edit_receipt();
                }
                ControlFlow::Continue(Ok(result))
            }
            Err(error) => {
                if guard.state.as_ref().unwrap().publication_started() {
                    // A monotonic partial phase remains in the existing slot;
                    // it is never unpacked or described as rolled back.
                    return ControlFlow::Continue(Err(error));
                }
                let mut state = guard.state.take().unwrap();
                let retention = state.retention_charge.take();
                ControlFlow::Break(SaveCompletionRefusal {
                    reason: SaveCompletionRefusalReason::ApplyFailed,
                    result: Ok(state.saved),
                    error: Some(error),
                    _retention_charge: retention,
                })
            }
        }
    }
    pub fn has_pending_save_completion(&self) -> bool {
        self.receipts.deferred_save.as_ref().is_some_and(|state| !state.is_committed())
    }
    /// Legacy assertion adapter only. The test harness already runs this on a
    /// non-UI worker, which explicitly retires completed/refused fixture owners.
    /// Ownership controls exercise the real ControlFlow API instead.
    #[cfg(test)]
    pub(crate) fn finish_save_for_test(&mut self, result: io::Result<SavedSnapshot>) -> io::Result<CommandResult> {
        let outcome = match self.finish_save(result) {
            ControlFlow::Continue(outcome) => outcome,
            ControlFlow::Break(mut refusal) => {
                assert_eq!(refusal.reason, SaveCompletionRefusalReason::ApplyFailed, "unexpected save refusal");
                let error = refusal.error.take().expect("failed save retains its original error");
                drop(refusal); // explicit fixture retirement on this test worker
                Err(error)
            }
        };
        if self.has_completed_save_completion() {
            let mut retirement = None;
            self.take_completed_save_completion(&mut retirement).expect("completed fixture retirement");
            drop(retirement); // explicit fixture retirement on this test worker
        }
        outcome
    }
    /// Retry a retained transformed completion after journal queue admission.
    pub fn finish_pending_save(&mut self) -> io::Result<Option<CommandResult>> {
        // Public observer compatibility is confined to this ordinary path.
        // The saved owner is already in its RAII-restored slot when a callback
        // or a preparation read runs; no take followed by a fallible retry.
        if !self.has_pending_save_completion() {
            return Ok(None);
        }
        let state = self.receipts.deferred_save.take();
        let mut guard = DeferredSaveGuard { editor: self, state };
        guard.editor.flush_edit_receipt()?;
        let budget = AllocationBudget::default();
        let state = guard.state.as_mut().unwrap();
        guard.editor.prepare_save_completion_inner(state, &budget, false)?;
        let result = guard.editor.commit_save_completion(state)?;
        let _ = guard.editor.flush_edit_receipt();
        Ok(Some(result))
    }
    pub(crate) fn finish_pending_save_sealed(
        &mut self,
        budget: &AllocationBudget,
    ) -> io::Result<Option<CommandResult>> {
        if !self.has_pending_save_completion() {
            return Ok(None);
        }
        let state = self.receipts.deferred_save.take();
        let mut guard = DeferredSaveGuard { editor: self, state };
        let state = guard.state.as_mut().unwrap();
        guard.editor.prepare_save_completion(state, budget)?;
        let result = guard.editor.commit_save_completion(state)?;
        Ok(Some(result))
    }
}

#[cfg(test)]
mod workspace_wrap_cursor_tests {
    use super::*;

    #[test]
    fn workspace_wrap_failed_save_after_partial_cursor_remap_advances_external_generation() {
        use std::{
            fs::OpenOptions,
            io::Write,
            sync::atomic::{AtomicU64, Ordering},
        };
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let (path, mut output) = (0..128)
            .find_map(|_| {
                let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir().join(format!("kwil-wrap-save-{}-{sequence}.txt", std::process::id()));
                match OpenOptions::new().write(true).create_new(true).open(&path) {
                    Ok(file) => Some((path, file)),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => None,
                    Err(error) => panic!("tiny fixture creation failed: {error}"),
                }
            })
            .expect("bounded exclusive fixture namespace");
        output.write_all(b"a").unwrap();
        drop(output);
        let mut editor = Editor::new(Document::new());
        editor.insert_text("abcdef", Duration::ZERO).unwrap();
        let generation = editor.external_cursor_generation().unwrap();
        let mut short = Buffer::empty();
        short.insert(0, b"a").unwrap();
        let result = editor.finish_save(Ok(SavedSnapshot {
            path: path.clone(),
            file: kwil_core_text::TextFile::open(&path).unwrap(),
            snapshot: short.snapshot(),
            revision: editor.document().revision(),
            warnings: Vec::new(),
            trimmed_rows: Some(0..1),
            trim_deletions: None,
            normalized: false,
        }));
        let (result, _saved_refusal) = match result {
            ControlFlow::Continue(result) => (result, None),
            ControlFlow::Break(mut refusal) => {
                (Err(refusal.error.take().expect("ordinary failure retains its actual error")), Some(refusal))
            }
        };
        assert!(result.is_err(), "missing trim provenance must refuse completion");
        assert_eq!(editor.primary_cursor().head, 1, "failure followed an existing partial cursor remap");
        assert!(editor.external_cursor_generation().unwrap() > generation);
        assert_eq!(editor.document().snapshot().len(), 6, "original document semantics stay unchanged");
        assert_eq!(
            _saved_refusal.as_ref().unwrap().result.as_ref().unwrap().snapshot.len(),
            1,
            "refusal still owns the exact saved snapshot"
        );
        std::fs::remove_file(path).unwrap();
    }
}
