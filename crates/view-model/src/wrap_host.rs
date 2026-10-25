//! Shared normal-text wrap coordinator (decision 0003).
//!
//! Frontends call `sync` before borrowing a page or routing input, then `drain`
//! on a worker wake and `drive` only when `needs_source` is true. `drive` takes
//! an already admitted immutable source from Workspace: it never creates a
//! snapshot, reads source bytes, or starts another worker. Refusal returns the
//! complete owner. Relative input is scalar FIFO; fill/reveal is latest state.
//!
//! ROOT owns module exports, View/Workspace/highlight wiring and validation.
//! Export with `pub mod wrap_host;` in view-model/lib.rs. The source-only test
//! module is nested here; do not add a second top-level wrap_host_tests module.
//! Shutdown transfers the exact worker/display owners to an off-UI ticket;
//! document cleanup follows successful settlement, never UI destruction.
//! Retained drain/refusal owners (including their reserved permits) settle on
//! that lane BEFORE waiting on the worker ticket; otherwise they block stop.
use crate::{
    Editor,
    editor::visual::{ChargedDamage, PreparedVisualCommit, VisualEditorState},
    hex_session::Representation,
    visual_rows::*,
    wrap_session::*,
};
use std::{
    fmt,
    mem::size_of,
    ops::{ControlFlow, Range},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WrapHostContext {
    /// The existing word-wrap setting; this API adds no settings or defaults.
    pub enabled: bool,
    pub key: LayoutKey,
    /// Advance on path replacement/Save As, even if bytes did not change.
    pub path_generation: u64,
    /// Advance for external cursor/selection changes, including same-byte ones.
    pub cursor_generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WrapHostAvailability {
    Ready,
    Disabled,
    BodyOwned(BodyOwner),
    EditorUnavailable,
    EmptyGeometry,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WrapHostSync {
    pub availability: WrapHostAvailability,
    pub cancelled: Option<CancelledInputs>,
    pub needs_source: bool,
}

#[derive(Debug)]
pub enum WrapHostInputError {
    Wrap(WrapError),
    Queue(QueueError),
}

/// One source owner, with optional already-constructed request metadata.
/// Splitting the request moves its fields; refusal allocates and clones nothing.
pub struct WrapHostSourceOwner {
    source: Arc<dyn WrapSource>,
    request: Option<WrapHostRequestParts>,
}
struct WrapHostRequestParts {
    fence: NavigationFence,
    anchor: TopAnchor,
    intent: LayoutIntent,
}
impl WrapHostSourceOwner {
    pub fn from_source(source: Arc<dyn WrapSource>) -> Self {
        Self { source, request: None }
    }
    pub fn from_request(request: WrapRequest) -> Self {
        let WrapRequest { fence, source, anchor, intent } = request;
        Self { source, request: Some(WrapHostRequestParts { fence, anchor, intent }) }
    }
    pub fn source(&self) -> &Arc<dyn WrapSource> {
        &self.source
    }
    /// Extract only a source-only refusal. A request returns its COMPLETE owner
    /// unchanged, so cursor storage cannot be dropped while extracting an Arc.
    pub fn into_source(self) -> ControlFlow<Self, Arc<dyn WrapSource>> {
        match self {
            Self { source, request: None } => ControlFlow::Continue(source),
            owner => ControlFlow::Break(owner),
        }
    }
    /// Reconstruct the exact request, including its existing cursor allocation.
    /// A source-only refusal is returned intact; no request fields are invented.
    pub fn into_request(self) -> ControlFlow<Self, WrapRequest> {
        match self {
            Self { source, request: Some(parts) } => ControlFlow::Continue(WrapRequest {
                fence: parts.fence,
                source,
                anchor: parts.anchor,
                intent: parts.intent,
            }),
            owner => ControlFlow::Break(owner),
        }
    }
    pub fn into_retired(self) -> RetiredOwner {
        match self.request {
            None => RetiredOwner::Source(self.source),
            Some(parts) => RetiredOwner::Request(WrapRequest {
                fence: parts.fence,
                source: self.source,
                anchor: parts.anchor,
                intent: parts.intent,
            }),
        }
    }
}
impl fmt::Debug for WrapHostSourceOwner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WrapHostSourceOwner")
            .field("request_fence", &self.request.as_ref().map(|parts| parts.fence))
            .finish_non_exhaustive()
    }
}
pub struct WrapHostSourceRefusal {
    pub owner: WrapHostSourceOwner,
    pub error: WrapError,
}
impl fmt::Debug for WrapHostSourceRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WrapHostSourceRefusal").field("error", &self.error).finish_non_exhaustive()
    }
}

/// Inline, fixed-size damage; no detached Vec can outlive its accounting.
pub struct WrapHostDamage {
    pub full: bool,
    pub scroll: i64,
    rows: [bool; MAX_ROWS],
    length: usize,
}
impl WrapHostDamage {
    pub fn changed_rows(&self) -> impl Iterator<Item = usize> + '_ {
        self.rows[..self.length].iter().enumerate().filter_map(|(row, changed)| changed.then_some(row))
    }
    fn between(page: &LayoutPage, previous: Option<&LayoutPage>) -> Self {
        let delta = page.delta_from(previous);
        let mut damage = Self {
            full: delta.full,
            scroll: delta.scroll,
            rows: [false; MAX_ROWS],
            length: usize::from(page.key.geometry.rows),
        };
        for row in delta.changed_rows() {
            damage.rows[row] = true;
        }
        damage
    }
}
pub struct WrapHostPublication {
    pub damage: WrapHostDamage,
    /// Preserve this wrapper if the frontend queues damage for later painting.
    pub cursor_damage: Option<ChargedDamage>,
    pub relative_id: Option<u64>,
    pub fence: NavigationFence,
}
#[derive(Default)]
pub struct WrapHostUpdate {
    pub publication: Option<WrapHostPublication>,
    pub progress: Option<LayoutProgress>,
    /// Worker/precommit failure is explicit; accepted relative input stays put.
    pub failure: Option<WrapError>,
}

/// Exceptional closed-permit ownership is returned, never dropped on the UI.
#[must_use = "retain exact owners and settle them off UI before document cleanup"]
pub struct WrapHostDrainFailure {
    pub error: WrapError,
    /// A commit may have succeeded before terminal source retirement failed.
    /// Preserve its damage charge and publication receipt with these owners.
    pub update: WrapHostUpdate,
    pub owner: Option<RetiredOwner>,
    pub permit: Option<RetirementPermit>,
    pub prepared: Option<PreparedVisualCommit>,
}
impl fmt::Debug for WrapHostDrainFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WrapHostDrainFailure").field("error", &self.error).finish_non_exhaustive()
    }
}

#[derive(Clone, Copy)]
enum DesiredKind {
    Fill(TopAnchor),
    Reveal(SourcePoint),
    Hit { point: SourcePoint, extend: bool },
}
#[derive(Clone, Copy)]
struct Desired {
    kind: DesiredKind,
    fence: NavigationFence,
    submitted: bool,
    failed: bool,
}
#[derive(Clone, Copy)]
struct ActiveRelative {
    id: u64,
    fence: NavigationFence,
}

pub struct WrapHost {
    workspace_identity: u64,
    workspace_owner: AtomicU64,
    session: WrapSession,
    display: Option<Box<VisualEditorState>>,
    context: Option<WrapHostContext>,
    availability: WrapHostAvailability,
    desired: Option<Desired>,
    active: Option<ActiveRelative>,
    next_generation: u64,
    terminal: bool,
    refresh: bool,
    anchor: TopAnchor,
    pending_cancelled: Option<CancelledInputs>,
    _charge: AllocationCharge,
}

/// A registration claim is provisional until Workspace publishes its matching
/// obligation. Failed admission releases only this claim, never another owner.
pub(crate) struct WorkspaceClaim<'a> {
    host: &'a WrapHost,
    owner: u64,
    committed: bool,
}
impl WorkspaceClaim<'_> {
    pub(crate) fn commit(mut self) {
        self.committed = true;
    }
}
impl Drop for WorkspaceClaim<'_> {
    fn drop(&mut self) {
        if !self.committed {
            let _ = self.host.workspace_owner.compare_exchange(self.owner, 0, Ordering::AcqRel, Ordering::Acquire);
        }
    }
}

impl WrapHost {
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> WrapResult<Self> {
        Self::with_budget(AllocationBudget::default(), wake)
    }
    pub fn with_budget(budget: AllocationBudget, wake: impl Fn() + Send + Sync + 'static) -> WrapResult<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let workspace_identity = NEXT
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| value.checked_add(1))
            .map_err(|_| WrapError::Admission("wrap host identity exhausted"))?;
        // Fixed scratch covers stack/Box transfers, including refused results.
        let charge = budget.reserve(
            size_of::<Self>()
                + size_of::<WrapHostSettlementTicket>()
                + 2 * size_of::<ResolvedNavigation>()
                + size_of::<LayoutPage>()
                + size_of::<WorkspaceClaim<'_>>(),
        )?;
        let session = WrapSession::with_budget(budget, wake)?;
        Ok(Self {
            workspace_identity,
            workspace_owner: AtomicU64::new(0),
            session,
            display: None,
            context: None,
            availability: WrapHostAvailability::Disabled,
            desired: None,
            active: None,
            next_generation: 1,
            terminal: false,
            refresh: false,
            anchor: TopAnchor::default(),
            pending_cancelled: None,
            _charge: charge,
        })
    }
    pub fn budget(&self) -> &AllocationBudget {
        self.session.budget()
    }
    pub(crate) fn workspace_identity(&self) -> u64 {
        self.workspace_identity
    }
    pub(crate) fn claim_workspace(&self, owner: u64) -> WrapResult<WorkspaceClaim<'_>> {
        if owner == 0 {
            return Err(WrapError::Invalid("zero workspace owner identity"));
        }
        self.workspace_owner
            .compare_exchange(0, owner, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| WrapError::Busy)?;
        Ok(WorkspaceClaim { host: self, owner, committed: false })
    }
    pub(crate) fn workspace_owned_by(&self, owner: u64) -> bool {
        owner != 0 && self.workspace_owner.load(Ordering::Acquire) == owner
    }
    /// Transfer the COMPLETE refused source/request to the existing fixed
    /// worker. Saturation/closure returns the same owner without allocation.
    pub fn retire_owner(&self, owner: RetiredOwner) -> ControlFlow<RetiredOwner, ()> {
        self.session.retire_owner(owner)
    }
    pub fn availability(&self) -> WrapHostAvailability {
        if self.terminal { WrapHostAvailability::Failed } else { self.availability }
    }
    pub fn pending_inputs(&self) -> usize {
        self.session.relative.len()
    }
    pub fn needs_source(&self) -> bool {
        self.availability == WrapHostAvailability::Ready
            && !self.terminal
            && !self.refresh
            && (self.active.is_none() && self.session.relative.mutation_gate() == MutationGate::RefusedPending
                || self.desired.is_some_and(|desired| !desired.submitted && !desired.failed))
    }
    fn generation(&mut self) -> WrapResult<u64> {
        let next = self.next_generation.checked_add(1).ok_or(WrapError::Admission("host generation exhausted"))?;
        let result = self.next_generation;
        self.next_generation = next;
        Ok(result)
    }
    fn current(&self) -> WrapResult<&VisualEditorState> {
        if self.availability != WrapHostAvailability::Ready || self.terminal || self.refresh {
            return Err(WrapError::Stale);
        }
        let state = self.display.as_deref().ok_or(WrapError::Busy)?;
        if self.context.is_none_or(|context| context.key != state.fence.key) {
            return Err(WrapError::Stale);
        }
        Ok(state)
    }
    fn editor_matches(state: &VisualEditorState, editor: &Editor) -> bool {
        editor.document().representation() == Representation::DecodedText
            && state.fence.key.source.source_epoch == editor.document().hex_source(0).epoch
            && state.fence.key.source.revision == editor.document().revision()
            && state.cursors.primary == editor.primary_index()
            && state.cursors.cursors().len() == editor.cursors().len()
            && state
                .cursors
                .cursors()
                .iter()
                .zip(editor.cursors())
                .all(|(display, actual)| display.anchor == actual.anchor && display.head == actual.head)
    }
    fn availability_for(context: WrapHostContext, editor: Option<&Editor>) -> WrapHostAvailability {
        if !context.enabled {
            return WrapHostAvailability::Disabled;
        }
        if context.key.body != BodyOwner::Text {
            return WrapHostAvailability::BodyOwned(context.key.body);
        }
        let Some(editor) = editor else { return WrapHostAvailability::EditorUnavailable };
        if editor.document().representation() != Representation::DecodedText {
            return WrapHostAvailability::BodyOwned(BodyOwner::Raw);
        }
        if context.key.geometry.is_empty() {
            return WrapHostAvailability::EmptyGeometry;
        }
        WrapHostAvailability::Ready
    }
    /// Cached identity/cursor metadata only. No line lookup, snapshot, or I/O.
    /// On retirement saturation the old owner stays here, hidden behind the new
    /// context fence; retry sync on the worker's retirement wake.
    pub fn sync(&mut self, editor: Option<&Editor>, context: WrapHostContext) -> WrapResult<WrapHostSync> {
        let mut availability = Self::availability_for(context, editor);
        if self.terminal {
            availability = WrapHostAvailability::Failed;
        }
        let changed = self.context != Some(context)
            || self.availability != availability
            || self.display.as_deref().zip(editor).is_some_and(|(state, editor)| !Self::editor_matches(state, editor));
        if changed {
            if self.context.is_none_or(|old| old.key.source != context.key.source) {
                self.anchor = TopAnchor {
                    source_byte: editor.map_or(0, |editor| editor.primary_cursor().head),
                    affinity: CaretAffinity::Downstream,
                    intra_row_offset: 0,
                };
            }
            let reason = match self.context {
                Some(old) if old.key.source.document_id != context.key.source.document_id => CancelReason::TabChanged,
                Some(old)
                    if old.key.geometry != context.key.geometry
                        || old.key.layout_generation != context.key.layout_generation =>
                {
                    CancelReason::GeometryChanged
                }
                _ if availability != WrapHostAvailability::Ready => CancelReason::BodyChanged,
                _ => CancelReason::SourceChanged,
            };
            self.context = Some(context);
            self.availability = availability;
            self.refresh = true;
            let cancelled = self.cancel(reason);
            self.pending_cancelled = Some(cancelled);
        }
        context.key.geometry.validate()?;
        if self.refresh {
            if self.display.is_some() {
                let permit = self.session.reserve_retirement()?;
                if permit.publish(|| ((), self.display.take().map(RetiredOwner::DisplayState))).is_err() {
                    self.terminal = true;
                    self.availability = WrapHostAvailability::Failed;
                    return Err(WrapError::Closed);
                }
            }
            if availability == WrapHostAvailability::Ready {
                let editor = editor.ok_or(WrapError::Stale)?;
                let generation = self.generation()?;
                let fence = NavigationFence {
                    key: context.key,
                    cursor_set_generation: generation,
                    intent_generation: generation,
                };
                // Capture's header remains with its Box; charge the transient
                // stack-to-heap overlap BEFORE creating either owner.
                let _transfer = self.budget().reserve(size_of::<VisualEditorState>())?;
                let state = VisualEditorState::capture(editor, fence, self.budget())?;
                self.display = Some(Box::new(state));
                self.refresh = false;
                self.request_fill(self.anchor)?;
            } else {
                self.refresh = false;
            }
        }
        Ok(WrapHostSync {
            availability: self.availability,
            cancelled: self.pending_cancelled.take(),
            needs_source: self.needs_source(),
        })
    }
    fn desired(&mut self, kind: DesiredKind) -> WrapResult<()> {
        if self.desired.is_some_and(|desired| matches!(desired.kind, DesiredKind::Hit { .. }))
            && !matches!(kind, DesiredKind::Hit { .. })
        {
            return Err(WrapError::Busy);
        }
        let current = self.current()?.fence;
        let generation = self.generation()?;
        self.desired = Some(Desired {
            kind,
            fence: NavigationFence { intent_generation: generation, ..current },
            submitted: false,
            failed: false,
        });
        Ok(())
    }
    pub fn request_fill(&mut self, anchor: TopAnchor) -> WrapResult<()> {
        self.desired(DesiredKind::Fill(anchor))
    }
    pub fn request_reveal(&mut self, point: SourcePoint) -> WrapResult<()> {
        self.desired(DesiredKind::Reveal(point))
    }
    fn enqueue(&mut self, kind: RelativeKind) -> Result<u64, WrapHostInputError> {
        let key = self.current().map_err(WrapHostInputError::Wrap)?.fence.key;
        if self.desired.is_some_and(|desired| matches!(desired.kind, DesiredKind::Hit { .. })) {
            return Err(WrapHostInputError::Wrap(WrapError::Busy));
        }
        self.session.relative.enqueue(RelativeInput { kind, key }).map_err(WrapHostInputError::Queue)
    }
    pub fn navigate(&mut self, motion: VisualMotion, count: u32, extend: bool) -> Result<u64, WrapHostInputError> {
        self.enqueue(RelativeKind::Move { motion, count, extend })
    }
    pub fn scroll(&mut self, delta: i64) -> Result<u64, WrapHostInputError> {
        self.enqueue(RelativeKind::Scroll(delta))
    }
    pub fn hit(&mut self, row: usize, cell: usize, extend: bool) -> WrapResult<()> {
        if self.mutation_gate() != MutationGate::Allowed {
            return Err(WrapError::Busy);
        }
        let point = self.hit_test(row, cell).ok_or(WrapError::Stale)?;
        self.desired(DesiredKind::Hit { point, extend })
    }
    pub fn mutation_gate(&self) -> MutationGate {
        if self.terminal {
            return MutationGate::RefusedFailed;
        }
        if let Some(desired) = self.desired.filter(|desired| matches!(desired.kind, DesiredKind::Hit { .. })) {
            return if desired.failed { MutationGate::RefusedFailed } else { MutationGate::RefusedPending };
        }
        self.session.relative.mutation_gate()
    }
    pub fn cancel(&mut self, reason: CancelReason) -> CancelledInputs {
        self.active = None;
        self.desired = None;
        let mut cancelled = self.session.cancel(reason);
        if let Some(previous) = self.pending_cancelled.take() {
            cancelled.records += previous.records;
        }
        cancelled
    }
    pub fn retry_relative(&mut self) -> WrapResult<()> {
        if self.terminal {
            return Err(WrapError::Closed);
        }
        let id = self.session.relative.head().ok_or(WrapError::Invalid("no relative input to retry"))?.id;
        self.session.retry_relative(id)?;
        self.active = None;
        Ok(())
    }
    /// Exact refusal owns the original Arc or the entire constructed request.
    /// Busy leaves the accepted FIFO head intact. No source is stored here.
    pub fn drive(&mut self, source: Arc<dyn WrapSource>) -> ControlFlow<WrapHostSourceRefusal, ()> {
        let refuse = |source, error| {
            ControlFlow::Break(WrapHostSourceRefusal { owner: WrapHostSourceOwner::from_source(source), error })
        };
        let state = match self.current() {
            Ok(state) => state,
            Err(error) => return refuse(source, error),
        };
        if source.identity() != state.fence.key.source {
            return refuse(source, WrapError::Stale);
        }
        let relative = self.active.is_none() && self.session.relative.mutation_gate() == MutationGate::RefusedPending;
        let (fence, anchor, intent, relative_id) = if relative {
            let key = state.fence.key;
            let cursor_generation = state.fence.cursor_set_generation;
            let anchor = state.page.as_ref().map_or(self.anchor, |page| page.anchor);
            let generation = match self.generation() {
                Ok(generation) => generation,
                Err(error) => {
                    if let Some(head) = self.session.relative.head() {
                        let _ = self.session.relative.mark_failed(head.id);
                    }
                    return refuse(source, error);
                }
            };
            let record = match self.session.relative.begin(key) {
                Ok(Some(record)) => record,
                _ => return refuse(source, WrapError::Stale),
            };
            let intent = match record.input.kind {
                RelativeKind::Scroll(delta) => LayoutIntent::ScrollRows(delta),
                RelativeKind::Move { motion, count, extend } => {
                    let state = self.display.as_deref().expect("current display");
                    let cursors = match CursorSet::new(self.budget(), state.cursors.cursors(), state.cursors.primary) {
                        Ok(cursors) => cursors,
                        Err(error) => {
                            let _ = self.session.relative.mark_failed(record.id);
                            return refuse(source, error);
                        }
                    };
                    LayoutIntent::MoveCursors { cursors, motion, count, extend }
                }
            };
            (
                NavigationFence { key, cursor_set_generation: cursor_generation, intent_generation: generation },
                anchor,
                intent,
                Some(record.id),
            )
        } else if let Some(desired) = self.desired.filter(|desired| !desired.submitted && !desired.failed) {
            let (anchor, intent) = match desired.kind {
                DesiredKind::Fill(anchor) => (anchor, LayoutIntent::Fill),
                DesiredKind::Reveal(point) | DesiredKind::Hit { point, .. } => (
                    TopAnchor { source_byte: point.byte, affinity: point.affinity, intra_row_offset: 0 },
                    LayoutIntent::Reveal(point),
                ),
            };
            (desired.fence, anchor, intent, None)
        } else {
            return refuse(source, WrapError::Busy);
        };
        match self.session.submit(WrapRequest { fence, source, anchor, intent }) {
            ControlFlow::Break(failure) => {
                if !matches!(failure.error, WrapError::Busy) {
                    self.mark_failed(relative_id);
                }
                ControlFlow::Break(WrapHostSourceRefusal {
                    owner: WrapHostSourceOwner::from_request(failure.request),
                    error: failure.error,
                })
            }
            ControlFlow::Continue(()) => {
                if let Some(id) = relative_id {
                    self.active = Some(ActiveRelative { id, fence });
                    self.display.as_deref_mut().expect("current display").fence = fence;
                } else if let Some(desired) = &mut self.desired {
                    desired.submitted = true;
                }
                ControlFlow::Continue(())
            }
        }
    }
    pub fn page(&self) -> Option<&LayoutPage> {
        self.current().ok()?.page.as_ref()
    }
    /// Borrow endpoint affinities/goals as published; no snapshot or allocation.
    pub fn visual_cursors(&self) -> Option<&[VisualCursor]> {
        Some(self.current().ok()?.cursors.cursors())
    }
    pub fn primary_index(&self) -> Option<usize> {
        Some(self.current().ok()?.cursors.primary)
    }
    pub fn hit_test(&self, row: usize, cell: usize) -> Option<SourcePoint> {
        self.page()?.hit_test(row, cell)
    }
    pub fn caret(&self, byte: u64, affinity: CaretAffinity) -> Option<VisualPoint> {
        self.page()?.source_to_screen(byte, affinity)
    }
    pub fn syntax_windows(&self) -> &[Range<u64>] {
        self.page().map_or(&[], LayoutPage::syntax_windows)
    }
    pub fn check_syntax_request(&self, range: Range<u64>, margin: usize) -> WrapResult<()> {
        self.page().ok_or(WrapError::Stale)?.check_syntax_request(range, margin)
    }
    pub fn selection_cells(&self, row: usize, range: Range<u64>) -> impl Iterator<Item = Range<u16>> + '_ {
        self.page().into_iter().flat_map(move |page| page.selection_cells(row, range.clone()))
    }

    /// Consume at most the single bounded ready slot, only against registered
    /// CURRENT lane fences. Never authorize a result using its own identity.
    pub fn drain(&mut self, editor: Option<&mut Editor>) -> ControlFlow<WrapHostDrainFailure, WrapHostUpdate> {
        self.drain_with_fault(editor, crate::editor::visual::PrepareFault::None)
    }
    fn drain_with_fault(
        &mut self,
        editor: Option<&mut Editor>,
        fault: crate::editor::visual::PrepareFault,
    ) -> ControlFlow<WrapHostDrainFailure, WrapHostUpdate> {
        if let Some(error) = self.session.terminal_failure() {
            self.terminal = true;
            self.availability = WrapHostAvailability::Failed;
            return ControlFlow::Continue(WrapHostUpdate { failure: Some(error), ..WrapHostUpdate::default() });
        }
        let live = self.current().is_ok()
            && editor
                .as_deref()
                .zip(self.display.as_deref())
                .is_some_and(|(editor, state)| Self::editor_matches(state, editor));
        if !live {
            // A borrowed-out editor/body never installs a late page or motion.
            self.session.take_registered_update(None, None);
            return ControlFlow::Continue(WrapHostUpdate::default());
        }
        let relative_fence = self.active.map(|active| active.fence);
        let desired_fence = self.desired.filter(|desired| desired.submitted).map(|desired| desired.fence);
        let progress = relative_fence
            .and_then(|fence| self.session.take_progress(fence))
            .or_else(|| desired_fence.and_then(|fence| self.session.take_progress(fence)));
        // The worker validates/retires stale ready ownership BEFORE reserving
        // both publication slots, under the SAME lock. Pre-reserving here can
        // strand a stale ready slot forever without another frontend wake.
        let (mut update, source_permit, publication_permit) =
            match self.session.take_registered_update_with_retirement(relative_fence, desired_fence) {
                Ok(Some(ready)) => ready,
                Ok(None) | Err(WrapError::Busy) => {
                    return ControlFlow::Continue(WrapHostUpdate { progress, ..WrapHostUpdate::default() });
                }
                Err(error) => return self.closed_failure(error),
            };
        let relative_id = update.relative_id;
        let request_fence = update.fence;
        let result = std::mem::replace(&mut update.result, Err(WrapError::Cancelled));
        let payload = match result {
            Ok(payload) => payload,
            Err(error) => {
                self.mark_failed(relative_id);
                return self.retire_update(
                    source_permit,
                    update,
                    WrapHostUpdate { progress, failure: Some(error), publication: None },
                );
            }
        };
        let state = self.display.as_deref_mut().expect("live display");
        let hit =
            self.desired.filter(|desired| desired.fence == request_fence).and_then(|desired| match desired.kind {
                DesiredKind::Hit { point, extend } => Some((point, extend)),
                _ => None,
            });
        let navigation = match payload {
            LayoutPayload::Navigation(navigation) => Some(navigation),
            LayoutPayload::Page(page) if hit.is_some() => {
                let (point, extend) = hit.expect("hit intent");
                let budget = self.session.budget();
                let prepared = (|| {
                    let expected = CursorSet::new(budget, state.cursors.cursors(), state.cursors.primary)?;
                    let primary = state.cursors.cursors()[state.cursors.primary];
                    let target = VisualCursor {
                        anchor: if extend { primary.anchor } else { point.byte },
                        head: point.byte,
                        anchor_affinity: if extend { primary.anchor_affinity } else { point.affinity },
                        head_affinity: point.affinity,
                        desired_cell: None,
                    };
                    let targets = CursorSet::new(budget, &[target], 0)?;
                    let header = budget.reserve(size_of::<ResolvedNavigation>())?;
                    let transfer = budget.reserve(size_of::<ResolvedNavigation>())?;
                    Ok::<_, WrapError>((expected, targets, header, transfer))
                })();
                match prepared {
                    Ok((expected, targets, header, _transfer)) => Some(Box::new(ResolvedNavigation {
                        fence: request_fence,
                        expected,
                        targets,
                        reveal: *page,
                        _header: header,
                    })),
                    Err(error) => {
                        update.result = Ok(LayoutPayload::Page(page));
                        self.mark_failed(relative_id);
                        return self.retire_update(
                            source_permit,
                            update,
                            WrapHostUpdate { progress, failure: Some(error), publication: None },
                        );
                    }
                }
            }
            LayoutPayload::Page(page) => {
                let damage = WrapHostDamage::between(&page, state.page.as_ref());
                let mut page = Some(*page);
                if let Err(permit) = publication_permit.publish(|| {
                    let old = state.page.replace(page.take().expect("page owner"));
                    ((), old.map(RetiredOwner::Page))
                }) {
                    update.result = Ok(LayoutPayload::Page(Box::new(page.take().expect("unaccepted page"))));
                    self.terminal = true;
                    return ControlFlow::Break(WrapHostDrainFailure {
                        error: WrapError::Closed,
                        update: WrapHostUpdate::default(),
                        owner: Some(RetiredOwner::Update(update)),
                        permit: Some(permit),
                        prepared: None,
                    });
                }
                self.anchor = state.page.as_ref().expect("published page").anchor;
                if let Err(error) = self.finish_publication(relative_id, request_fence) {
                    return self.retire_update(
                        source_permit,
                        update,
                        WrapHostUpdate { progress, failure: Some(error), publication: None },
                    );
                }
                return self.retire_update(
                    source_permit,
                    update,
                    WrapHostUpdate {
                        progress,
                        failure: None,
                        publication: Some(WrapHostPublication {
                            damage,
                            cursor_damage: None,
                            relative_id,
                            fence: request_fence,
                        }),
                    },
                );
            }
        };
        let navigation = navigation.expect("navigation branch");
        let damage = WrapHostDamage::between(&navigation.reveal, state.page.as_ref());
        // Desired hit uses its own fence; relative dispatch already set this.
        let previous_fence = state.fence;
        state.fence = request_fence;
        let editor = editor.expect("live editor");
        let prepared = match editor.prepare_visual_cursor_commit_with_fault(
            state,
            *navigation,
            request_fence,
            self.session.budget(),
            publication_permit,
            fault,
        ) {
            ControlFlow::Continue(prepared) => prepared,
            ControlFlow::Break(failure) => {
                state.fence = previous_fence;
                update.result = Ok(LayoutPayload::Navigation(Box::new(failure.navigation)));
                drop(failure.permit);
                self.mark_failed(relative_id);
                return self.retire_update(
                    source_permit,
                    update,
                    WrapHostUpdate { progress, failure: Some(failure.error), publication: None },
                );
            }
        };
        match editor.commit_visual_cursor_update(state, prepared, request_fence) {
            ControlFlow::Continue(published) => {
                self.anchor = state.page.as_ref().expect("published navigation page").anchor;
                if let Err(error) = self.finish_publication(relative_id, request_fence) {
                    return self.retire_update(
                        source_permit,
                        update,
                        WrapHostUpdate { progress, failure: Some(error), publication: None },
                    );
                }
                // Any old desired fill was resolved against the previous
                // cursor set. It cannot install after this publication.
                self.desired = None;
                self.retire_update(
                    source_permit,
                    update,
                    WrapHostUpdate {
                        progress,
                        failure: None,
                        publication: Some(WrapHostPublication {
                            damage,
                            cursor_damage: Some(published.damage),
                            relative_id,
                            fence: published.fence,
                        }),
                    },
                )
            }
            ControlFlow::Break(failure) => {
                state.fence = previous_fence;
                self.mark_failed(relative_id);
                match failure.prepared.retire() {
                    ControlFlow::Continue(()) => self.retire_update(
                        source_permit,
                        update,
                        WrapHostUpdate { progress, failure: Some(failure.error), publication: None },
                    ),
                    ControlFlow::Break(prepared) => {
                        self.terminal = true;
                        ControlFlow::Break(WrapHostDrainFailure {
                            error: failure.error,
                            owner: Some(RetiredOwner::Update(update)),
                            update: WrapHostUpdate::default(),
                            permit: Some(source_permit),
                            prepared: Some(prepared),
                        })
                    }
                }
            }
        }
    }
    fn mark_failed(&mut self, relative_id: Option<u64>) {
        if let Some(id) = relative_id {
            let _ = self.session.relative.mark_failed(id);
        } else if let Some(desired) = &mut self.desired {
            desired.failed = true;
        }
    }
    fn finish_publication(&mut self, relative_id: Option<u64>, fence: NavigationFence) -> WrapResult<()> {
        if let Some(id) = relative_id {
            if self.active.is_none_or(|active| active.id != id || active.fence != fence) {
                self.terminal = true;
                return Err(WrapError::Stale);
            }
            if self.session.published_relative(id, fence).is_err() {
                self.terminal = true;
                return Err(WrapError::Stale);
            }
            self.active = None;
        } else {
            self.desired = None;
        }
        Ok(())
    }
    fn closed_failure(&mut self, error: WrapError) -> ControlFlow<WrapHostDrainFailure, WrapHostUpdate> {
        self.terminal = true;
        ControlFlow::Break(WrapHostDrainFailure {
            error,
            update: WrapHostUpdate::default(),
            owner: None,
            permit: None,
            prepared: None,
        })
    }
    fn retire_update(
        &mut self,
        permit: RetirementPermit,
        update: LayoutUpdate,
        value: WrapHostUpdate,
    ) -> ControlFlow<WrapHostDrainFailure, WrapHostUpdate> {
        let mut owner = Some(RetiredOwner::Update(update));
        match permit.publish(|| ((), owner.take())) {
            Ok(()) => ControlFlow::Continue(value),
            Err(permit) => {
                self.terminal = true;
                ControlFlow::Break(WrapHostDrainFailure {
                    error: WrapError::Closed,
                    update: value,
                    owner,
                    permit: Some(permit),
                    prepared: None,
                })
            }
        }
    }
    #[must_use = "settle off UI before document cleanup; retain failure ticket for retry"]
    pub fn stop(self) -> WrapHostSettlementTicket {
        WrapHostSettlementTicket { worker: self.session.stop(), display: self.display, _charge: self._charge }
    }
}

#[must_use = "dropping or detaching is not settlement"]
pub struct WrapHostSettlementTicket {
    worker: SettlementTicket,
    display: Option<Box<VisualEditorState>>,
    _charge: AllocationCharge,
}
pub struct WrapHostSettlementFailure {
    pub ticket: WrapHostSettlementTicket,
    pub error: WrapError,
}
impl fmt::Debug for WrapHostSettlementFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WrapHostSettlementFailure").field("error", &self.error).finish_non_exhaustive()
    }
}
impl WrapHostSettlementTicket {
    /// OFF UI. Failure preserves the exact worker and display owners together.
    pub fn settle(self, timeout: Duration) -> Result<(), WrapHostSettlementFailure> {
        let Self { worker, display, _charge } = self;
        match worker.settle(timeout) {
            Ok(()) => {
                drop(display);
                Ok(())
            }
            Err(failure) => Err(WrapHostSettlementFailure {
                error: failure.error,
                ticket: Self { worker: failure.ticket, display, _charge },
            }),
        }
    }
    pub fn retained_bytes(&self) -> usize {
        self.worker.retained_bytes()
    }
}

#[cfg(test)]
#[path = "wrap_host_tests.rs"]
mod tests;
