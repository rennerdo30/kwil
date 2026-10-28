//! Shared Workspace boundary for BOTH normal-text wrap hosts (decision 0003).
//!
//! ROOT export: `pub use workspace::workspace_wrap;`. Create ONE owner with
//! `new(&mut workspace, &host)` using that host's budget, then prepare the
//! context with the actual text geometry BEFORE host.sync/input/paint/drain.
//! Lend only through this owner. A refused drive retains the COMPLETE request;
//! the existing WrapReady retirement wake authorizes retry, never an idle poll.
//!
//! Stop moves this owner and that SAME host into the existing off-UI cleanup
//! lane. Settle it before document cleanup and return the success receipt to
//! Workspace. Failure retains the remaining ticket and never opens the gate.
//! Pending frontend ChargedDamage travels beside this ticket in that lane.

use super::Workspace;
use crate::{
    hex_session::Representation,
    visual_rows::{
        AllocationBudget, AllocationCharge, BodyOwner, LayoutGeometry, LayoutKey, SnapshotSource, SourceIdentity,
        WrapError, WrapResult, WrapSource,
    },
    wrap_host::{
        WrapHost, WrapHostContext, WrapHostDrainFailure, WrapHostSettlementTicket, WrapHostSourceOwner,
        WrapHostSourceRefusal,
    },
    wrap_session::RetiredOwner,
};
#[cfg(test)]
use std::io;
use std::{
    fmt,
    mem::size_of,
    ops::ControlFlow,
    sync::{
        Arc, Weak,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

pub(super) struct WorkspaceWrapState {
    owner: Option<u64>,
    path_generation: Option<u64>,
    settings_generation: Option<u64>,
    settling: bool,
    failed: Option<WorkspaceWrapSettlementFailure>,
    exit_accepted: bool,
}
impl Default for WorkspaceWrapState {
    fn default() -> Self {
        Self {
            owner: None,
            path_generation: Some(0),
            settings_generation: Some(0),
            settling: false,
            failed: None,
            exit_accepted: false,
        }
    }
}
impl WorkspaceWrapState {
    pub(super) fn path_changed(&mut self) {
        self.path_generation = self.path_generation.and_then(|value| value.checked_add(1));
    }
    pub(super) fn settings_changed(&mut self) {
        self.settings_generation = self.settings_generation.and_then(|value| value.checked_add(1));
    }
}

/// No source storage lives here. The weak View token cannot keep a document
/// alive, and prevents address reuse from impersonating an old incarnation.
pub struct WorkspaceWrap {
    owner: u64,
    host_identity: u64,
    incarnation: u64,
    view: Weak<()>,
    input: Option<(u64, u64)>,
    input_generation: u64,
    layout_generation: u64,
    body_generation: u64,
    context: Option<WrapHostContext>,
    refused: Option<WrapHostSourceOwner>,
    failure: Option<Box<WrapHostDrainFailure>>,
    charge: AllocationCharge,
}

impl WorkspaceWrap {
    /// The host is already constructed; no second default budget is created.
    /// Fixed refusal/failure and stop-transfer storage is admitted BEFORE the
    /// obligation is registered or any immutable source can be pinned.
    pub fn new(workspace: &mut Workspace, host: &WrapHost) -> WrapResult<Self> {
        Self::register(workspace, host, AllocationBudget::reserve)
    }
    fn register(
        workspace: &mut Workspace,
        host: &WrapHost,
        admit: impl FnOnce(&AllocationBudget, usize) -> WrapResult<AllocationCharge>,
    ) -> WrapResult<Self> {
        if workspace.wrap.exit_accepted {
            return Err(WrapError::Closed);
        }
        if workspace.wrap.owner.is_some() {
            return Err(WrapError::Busy);
        }
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let owner = NEXT
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| value.checked_add(1))
            .map_err(|_| WrapError::Admission("workspace wrap owner identity exhausted"))?;
        let claim = host.claim_workspace(owner)?;
        let charge = admit(
            host.budget(),
            size_of::<Self>()
                + 2 * size_of::<WorkspaceWrapSettlementTicket>()
                + size_of::<WrapHostSourceRefusal>()
                + 2 * size_of::<WrapHostDrainFailure>()
                + size_of::<WorkspaceWrapState>()
                + size_of::<crate::document_worker::DocumentRequest>()
                + size_of::<crate::document_worker::DocumentResult>(),
        )?;
        let result = Self {
            owner,
            host_identity: host.workspace_identity(),
            incarnation: 0,
            view: Weak::new(),
            input: None,
            input_generation: 0,
            layout_generation: 0,
            body_generation: 0,
            context: None,
            refused: None,
            failure: None,
            charge,
        };
        // All fallible work is complete. Publish the obligation and commit the
        // unique host claim together, with no allocation or callback between.
        workspace.wrap.owner = Some(owner);
        claim.commit();
        Ok(result)
    }

    fn bound(&self, workspace: &Workspace) -> WrapResult<()> {
        if workspace.wrap.owner != Some(self.owner) {
            return Err(WrapError::Stale);
        }
        if workspace.wrap.exit_accepted {
            return Err(WrapError::Closed);
        }
        Ok(())
    }
    fn bound_host(&self, host: &WrapHost) -> WrapResult<()> {
        if self.host_identity != host.workspace_identity() || !host.workspace_owned_by(self.owner) {
            return Err(WrapError::Stale);
        }
        Ok(())
    }

    /// Cached scalar metadata only: no path clone/hash, source read, snapshot,
    /// cursor allocation, or logical-line lookup. Geometry is frontend supplied
    /// after split/panel/gutter subtraction; Workspace owns tab/word-wrap policy.
    pub fn prepare_context(
        &mut self,
        workspace: &Workspace,
        mut geometry: LayoutGeometry,
        mut body: BodyOwner,
    ) -> WrapResult<WrapHostContext> {
        self.bound(workspace)?;
        let id = workspace.active_id();
        let view = workspace.views.get(&id).ok_or(WrapError::Stale)?;
        let editor = view.editor();
        if !std::ptr::eq(self.view.as_ptr(), Arc::as_ptr(&view.input_incarnation)) {
            self.incarnation = advance(self.incarnation)?;
            self.view = Arc::downgrade(&view.input_incarnation);
        }
        let input = (workspace.input_epoch, workspace.files.next_prompt);
        if self.input != Some(input) {
            self.input_generation = advance(self.input_generation)?;
            self.input = Some(input);
        }
        if workspace.hex_owns_document(id) || workspace.binary.flights.contains_key(&id) {
            body = BodyOwner::Raw;
        } else if editor.is_none() || workspace.document_busy(id) {
            body = BodyOwner::EditorLoan;
        }
        let mut source = SourceIdentity { document_id: id.0, incarnation: self.incarnation, ..Default::default() };
        let mut cursor_generation = 0;
        let enabled = if let Some(editor) = editor {
            let metadata = editor.document().hex_source(id.0);
            source.source_epoch = metadata.epoch;
            source.revision = metadata.revision;
            if metadata.representation != Representation::DecodedText {
                body = BodyOwner::Raw;
            }
            cursor_generation = editor
                .external_cursor_generation()
                .map_err(|_| WrapError::Admission("external cursor generation exhausted"))?;
            geometry.tab_size = u16::from(editor.settings.editor.tab_size);
            editor.settings.editor.word_wrap
        } else {
            workspace.editor_settings.as_ref().is_some_and(|settings| settings.editor.word_wrap)
        };
        geometry.settings_generation = workspace
            .wrap
            .settings_generation
            .ok_or(WrapError::Admission("workspace wrap settings generation exhausted"))?;
        geometry.validate()?;
        let path_generation =
            workspace.wrap.path_generation.ok_or(WrapError::Admission("workspace wrap path generation exhausted"))?;
        if self.context.is_none_or(|old| old.key.geometry != geometry) {
            self.layout_generation = advance(self.layout_generation)?;
        }
        if self.context.is_none_or(|old| old.key.body != body) {
            self.body_generation = advance(self.body_generation)?;
        }
        let context = WrapHostContext {
            enabled,
            key: LayoutKey {
                source,
                input_epoch: self.input_generation,
                layout_generation: self.layout_generation,
                body_generation: self.body_generation,
                body,
                geometry,
            },
            path_generation,
            cursor_generation,
        };
        self.context = Some(context);
        Ok(context)
    }

    /// Validate a previously prepared full context without rebuilding it.
    /// Frontends still prepare their actual geometry/body before each event;
    /// syntax consumers must compare the page's COMPLETE key with this context.
    pub fn context_is_current(&self, workspace: &Workspace, context: WrapHostContext) -> bool {
        if self.bound(workspace).is_err()
            || self.context != Some(context)
            || self.input != Some((workspace.input_epoch, workspace.files.next_prompt))
            || workspace.wrap.path_generation != Some(context.path_generation)
            || workspace.wrap.settings_generation != Some(context.key.geometry.settings_generation)
        {
            return false;
        }
        let id = workspace.active_id();
        let Some(view) = workspace.views.get(&id) else { return false };
        if !std::ptr::eq(self.view.as_ptr(), Arc::as_ptr(&view.input_incarnation))
            || context.key.source.document_id != id.0
            || context.key.source.incarnation != self.incarnation
            || workspace.hex_owns_document(id)
            || workspace.document_busy(id)
            || workspace.binary.flights.contains_key(&id)
        {
            return false;
        }
        let Some(editor) = view.editor() else { return false };
        let source = editor.document().hex_source(id.0);
        source.representation == Representation::DecodedText
            && source.epoch == context.key.source.source_epoch
            && source.revision == context.key.source.revision
            && editor.external_cursor_generation().ok() == Some(context.cursor_generation)
            && editor.settings.editor.word_wrap == context.enabled
            && context.key.geometry.tab_size == u16::from(editor.settings.editor.tab_size)
    }

    /// Drive at most once. No permanent source Arc or speculative source slot.
    /// Every allocation, including the actual Snapshot header inside the source
    /// and Arc ownership counters, is charged to this SAME host before snapshot
    /// capture. Snapshot capture clones existing immutable roots in O(1).
    pub fn lend_source(
        &mut self,
        workspace: &Workspace,
        context: WrapHostContext,
        host: &mut WrapHost,
    ) -> WrapResult<()> {
        self.bound(workspace)?;
        self.bound_host(host)?;
        self.retire_errors(host)?;
        if !host.needs_source() {
            return Ok(());
        }
        let live = self.prepare_context(workspace, context.key.geometry, context.key.body)?;
        if live != context || !context.enabled || context.key.body != BodyOwner::Text {
            return Err(WrapError::Stale);
        }
        let editor =
            workspace.views.get(&workspace.active_id()).and_then(crate::View::editor).ok_or(WrapError::Stale)?;
        // The empty fixed refusal cell is reserved before constructing a source.
        // retire_errors above never frees a full cell by dropping its owner.
        let source = admitted_source(host.budget(), context.key.source, || editor.document().snapshot())?;
        match host.drive(source) {
            ControlFlow::Continue(()) => Ok(()),
            ControlFlow::Break(WrapHostSourceRefusal { owner, error }) => {
                self.refused = Some(owner);
                Err(error)
            }
        }
    }

    /// Saturation returns the original owner, including any request cursor Vec.
    /// Report the refusal's error before handing it back here if it is needed.
    pub fn retain_source_refusal(&mut self, refusal: WrapHostSourceRefusal) -> ControlFlow<WrapHostSourceRefusal, ()> {
        if self.refused.is_some() {
            return ControlFlow::Break(refusal);
        }
        self.refused = Some(refusal.owner);
        ControlFlow::Continue(())
    }

    /// Retains the successful publication/damage receipt as well as every
    /// exceptional source/prepared/permit field. Do not drain again on failure.
    pub fn retain_drain_failure(&mut self, failure: WrapHostDrainFailure) -> ControlFlow<WrapHostDrainFailure, ()> {
        if self.failure.is_some() {
            return ControlFlow::Break(failure);
        }
        // Both the retained Box and its transient stack overlap were admitted
        // when this fixed, single failure cell was constructed.
        self.failure = Some(Box::new(failure));
        ControlFlow::Continue(())
    }

    pub fn failed(&self) -> bool {
        self.failure.is_some()
    }
    /// The single failure slot is checked BEFORE intake; a frontend never needs
    /// a second refusal slot to preserve an exceptional drain owner.
    pub fn drain_host(
        &mut self,
        host: &mut WrapHost,
        editor: Option<&mut crate::Editor>,
    ) -> WrapResult<crate::wrap_host::WrapHostUpdate> {
        self.bound_host(host)?;
        if self.failure.is_some() {
            return Err(WrapError::Closed);
        }
        match host.drain(editor) {
            ControlFlow::Continue(update) => Ok(update),
            ControlFlow::Break(failure) => {
                self.failure = Some(Box::new(failure));
                Err(WrapError::Closed)
            }
        }
    }
    /// A terminal retirement failure may contain an already committed cursor
    /// publication. Its damage/receipt stay charged in this owner until stop.
    pub fn drain_failure(&self) -> Option<&WrapHostDrainFailure> {
        self.failure.as_deref()
    }
    pub fn has_refused_source(&self) -> bool {
        self.refused.is_some()
    }

    /// Uses only the existing worker's two fixed retirement slots. Busy retains
    /// the exact source/request here until that worker's capacity wake. Terminal
    /// drain failures are held for off-UI stop; they never authorize new pins.
    pub fn retire_errors(&mut self, host: &WrapHost) -> WrapResult<()> {
        self.bound_host(host)?;
        if self.failure.is_some() || host.availability() == crate::wrap_host::WrapHostAvailability::Failed {
            return Err(WrapError::Closed);
        }
        if let Some(owner) = self.refused.take() {
            match host.retire_owner(owner.into_retired()) {
                ControlFlow::Continue(()) => {}
                ControlFlow::Break(owner) => {
                    self.refused = Some(match owner {
                        RetiredOwner::Source(source) => WrapHostSourceOwner::from_source(source),
                        RetiredOwner::Request(request) => WrapHostSourceOwner::from_request(request),
                        _ => unreachable!("source retirement returned a different owner kind"),
                    });
                    return Err(WrapError::Busy);
                }
            }
        }
        Ok(())
    }

    /// Transfer to the existing off-UI shutdown lane, before document cleanup.
    /// Merely constructing or dropping this ticket does not open Workspace's gate.
    pub fn stop(self, host: WrapHost) -> ControlFlow<(Self, WrapHost), WorkspaceWrapSettlementTicket> {
        if self.bound_host(&host).is_err() {
            return ControlFlow::Break((self, host));
        }
        ControlFlow::Continue(WorkspaceWrapSettlementTicket {
            owner: self.owner,
            host: host.stop(),
            refused: self.refused,
            failure: self.failure,
            _charge: self.charge,
        })
    }
}

fn advance(value: u64) -> WrapResult<u64> {
    value.checked_add(1).ok_or(WrapError::Admission("workspace wrap generation exhausted"))
}

// Arc stores two ownership counters before its aligned data. Include that
// actual ownership/header layout, not just size_of::<Arc<dyn WrapSource>>().
#[repr(C)]
struct ArcAllocation<T> {
    strong: AtomicUsize,
    weak: AtomicUsize,
    data: T,
}
struct AdmittedSource {
    source: SnapshotSource,
    _charge: AllocationCharge,
}
fn admitted_source(
    budget: &AllocationBudget,
    identity: SourceIdentity,
    capture: impl FnOnce() -> kwil_core_text::Snapshot,
) -> WrapResult<Arc<dyn WrapSource>> {
    let charge = budget.reserve(size_of::<ArcAllocation<AdmittedSource>>())?;
    let transfer = budget.reserve(size_of::<AdmittedSource>())?;
    let source: Arc<dyn WrapSource> =
        Arc::new(AdmittedSource { source: SnapshotSource::new(identity, capture()), _charge: charge });
    drop(transfer);
    Ok(source)
}
impl WrapSource for AdmittedSource {
    fn identity(&self) -> SourceIdentity {
        self.source.identity()
    }
    fn len(&self) -> u64 {
        self.source.len()
    }
    fn line_count(&self) -> u64 {
        self.source.line_count()
    }
    fn run_layout(
        self: Arc<Self>,
        engine: crate::visual_rows::LayoutEngine,
        request: crate::visual_rows::task::TaskRequest,
        driver: &mut dyn crate::visual_rows::TaskDriver,
    ) -> crate::visual_rows::WorkerExit {
        crate::visual_rows::run_snapshot(self, engine, request, driver)
    }
    #[cfg(test)]
    fn line_start(&self, line: u64) -> io::Result<Option<u64>> {
        self.source.line_start(line)
    }
    #[cfg(test)]
    fn line_of(&self, byte: u64) -> io::Result<u64> {
        self.source.line_of(byte)
    }
    #[cfg(test)]
    fn read(&self, at: u64, output: &mut [u8]) -> io::Result<usize> {
        self.source.read(at, output)
    }
}
impl crate::snapshot_layout_source::SnapshotOwner for AdmittedSource {
    fn snapshot(&self) -> &kwil_core_text::Snapshot {
        crate::snapshot_layout_source::SnapshotOwner::snapshot(&self.source)
    }
}

#[must_use = "settle off UI, then acknowledge the bound receipt BEFORE document cleanup"]
pub struct WorkspaceWrapSettlementTicket {
    owner: u64,
    host: WrapHostSettlementTicket,
    refused: Option<WrapHostSourceOwner>,
    failure: Option<Box<WrapHostDrainFailure>>,
    _charge: AllocationCharge,
}
#[must_use = "retain the remaining exact ticket for explicit retry"]
pub struct WorkspaceWrapSettlementFailure {
    /// The fixed failure-transfer header is admitted by WorkspaceWrap::new;
    /// boxing here never detaches a source from its charge or copies a request.
    pub ticket: Box<WorkspaceWrapSettlementTicket>,
    pub error: WrapError,
}
impl fmt::Debug for WorkspaceWrapSettlementFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkspaceWrapSettlementFailure").field("error", &self.error).finish_non_exhaustive()
    }
}
/// Private identity; cannot be constructed from a timeout, detach or UI drop.
#[must_use = "return this receipt to its originating Workspace"]
pub struct WorkspaceWrapSettled {
    owner: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceWrapSettlementState {
    Ready,
    Active,
    Pending,
    Failed,
}

/// One preadmitted exceptional result cell for a frontend's EXISTING shutdown
/// bundle. A refused foreign/duplicate result cannot be dropped on the UI or
/// requeued. An occupied cell is a sticky shutdown failure: stop consuming that
/// result receiver and retain the cell, receiver and ticket together off UI.
#[must_use = "retain occupied cell and its receiver in the existing shutdown owner"]
pub struct WorkspaceWrapResultCell {
    result: Option<Result<WorkspaceWrapSettled, WorkspaceWrapSettlementFailure>>,
    _charge: AllocationCharge,
}
impl WorkspaceWrapResultCell {
    /// Reserve the actual inline cell/header AND result-transfer overlap from
    /// the same host before any result can enter this exceptional owner.
    pub fn new(host: &WrapHost) -> WrapResult<Self> {
        let charge = host
            .budget()
            .reserve(size_of::<Self>() + size_of::<Result<WorkspaceWrapSettled, WorkspaceWrapSettlementFailure>>())?;
        Ok(Self { result: None, _charge: charge })
    }
    pub fn retain(
        &mut self,
        result: Result<WorkspaceWrapSettled, WorkspaceWrapSettlementFailure>,
    ) -> ControlFlow<Result<WorkspaceWrapSettled, WorkspaceWrapSettlementFailure>, ()> {
        if self.result.is_some() {
            return ControlFlow::Break(result);
        }
        self.result = Some(result);
        ControlFlow::Continue(())
    }
    pub fn is_occupied(&self) -> bool {
        self.result.is_some()
    }
}
impl WorkspaceWrapSettlementTicket {
    /// OFF UI ONLY. Exceptional owners/active permits must settle BEFORE the
    /// worker's stop can complete. Try the prepared owner's own retire protocol;
    /// closed admission returns it whole for off-UI terminal abort. That abort
    /// is not settlement: only actual successful worker completion mints a
    /// receipt. A terminal worker failure stays failed on every retry.
    pub fn settle(mut self, timeout: Duration) -> Result<WorkspaceWrapSettled, WorkspaceWrapSettlementFailure> {
        let started = Instant::now();
        if let Some(mut failure) = self.failure.take() {
            if let Some(prepared) = failure.prepared.take() {
                match prepared.retire() {
                    ControlFlow::Continue(()) => {}
                    ControlFlow::Break(prepared) => drop(prepared),
                }
            }
            drop(failure);
        }
        drop(self.refused.take());
        let Self { owner, host, refused, failure, _charge } = self;
        match host.settle(timeout.saturating_sub(started.elapsed())) {
            Ok(()) => Ok(WorkspaceWrapSettled { owner }),
            Err(error) => Err(WorkspaceWrapSettlementFailure {
                error: error.error,
                ticket: Box::new(Self { owner, host: error.ticket, refused, failure, _charge }),
            }),
        }
    }
}

impl Workspace {
    /// Rebind an original Workspace after an unavailable frontend has retired
    /// its wrap owner. Accepted document work or an accepted user exit cannot
    /// be reopened by this source-free startup recovery operation.
    pub fn rearm_unstarted_frontend(&mut self) -> WrapResult<()> {
        self.validate_unstarted_frontend()?;
        let generation = self
            .wrap
            .path_generation
            .and_then(|value| value.checked_add(1))
            .ok_or(WrapError::Admission("frontend recovery generation exhausted"))?;
        self.wrap.path_generation = Some(generation);
        self.wrap.exit_accepted = false;
        Ok(())
    }

    /// Cached startup preflight; a retired accepted-exit fence must first be
    /// rearmed by the frontend returning the original Workspace.
    pub fn unstarted_frontend_ready(&self) -> bool {
        !self.wrap.exit_accepted && self.validate_unstarted_frontend().is_ok()
    }

    fn validate_unstarted_frontend(&self) -> WrapResult<()> {
        if self.files.document_started
            || self.files.bounded_document
            || self.files.worker.is_some()
            || self.files.quit
            || self.files.exit
        {
            return Err(WrapError::Closed);
        }
        if self.wrap.owner.is_some()
            || self.wrap.settling
            || self.wrap.failed.is_some()
            || self.files.preparing
            || self.files.document_shutdown
            || !self.files.saves.is_empty()
            || !self.files.loans.is_empty()
            || !self.files.replacement_journals.is_empty()
            || !self.files.failed_opens.is_empty()
            || !self.files.close.is_empty()
            || !self.files.save_all.is_empty()
            || self.files.prompt.is_some()
            || self.files.storage_ready
            || self.files.persist_inflight
            || self.files.persist_pending
            || self.files.persist_timer
            || self.files.deferred_recovery.is_some()
            || self.files.deferred_recovery_kind.is_some()
            || self.files.history_wait.is_some()
            || self.files.reopen.is_some()
            || self.files.hot_exit
            || self.files.manual_guard
            || self.files.exit_without_metadata
            || !self.files.observed.is_empty()
            || !self.files.seeded.is_empty()
            || !self.files.durable.is_empty()
            || !self.files.journal_failed.is_empty()
            || !self.closed_views.is_empty()
            || !self.binary.retiring.is_empty()
            || self.hex.disposing
            || self.hex.prompt
            || self.hex.opening()
            || !self.hex.loans.is_empty()
            || !self.hex.seed.is_empty()
            || !self.initial_open_requests_ready()
        {
            return Err(WrapError::Busy);
        }
        if self.wrap.path_generation.is_none() || self.wrap.settings_generation.is_none() {
            return Err(WrapError::Admission("frontend recovery identity exhausted"));
        }
        Ok(())
    }

    /// Inspect only bounded startup headers. Original queued files, if any,
    /// stay in their exact requests through frontend recovery and rebind.
    fn initial_open_requests_ready(&self) -> bool {
        if self.files.queue.len() > kwil_core_workspace::session::MAX_TABS
            || self.files.queue.len() != self.binary.flights.len()
        {
            return false;
        }
        for (index, request) in self.files.queue.iter().enumerate() {
            let crate::document_worker::DocumentRequest::OpenChecked { ticket, path, file } = request else {
                return false;
            };
            if ticket.document().0 == 0
                || ticket.revision() != 0
                || ticket.generation() == 0
                || ticket.generation() > self.binary.next_generation
                || !path.is_absolute()
                || path.capacity() > kwil_core_workspace::session::MAX_PATH_BYTES
                || path.as_os_str().as_encoded_bytes().len() > kwil_core_workspace::session::MAX_PATH_BYTES
                || file.as_ref().is_some_and(|file| file.path() != path)
                || self.files.queue.iter().take(index).any(|previous| {
                    matches!(previous, crate::document_worker::DocumentRequest::OpenChecked { ticket: previous, .. }
                        if previous.document() == ticket.document())
                })
            {
                return false;
            }
            let Some(flight) = self.binary.flights.get(&ticket.document()) else { return false };
            if flight.ticket != *ticket
                || flight.path != *path
                || !matches!(flight.phase, super::BinaryPhase::Opening)
                || flight.preview_ready
                || flight.return_to.is_some_and(|id| id == ticket.document() || !self.views.contains_key(&id))
                || !self.session.tabs().iter().any(|tab| {
                    tab.id == ticket.document()
                        && tab.revision == 0
                        && tab.saved_revision == 0
                        && tab.path.as_ref() == Some(path)
                })
                || !self.views.get(&ticket.document()).is_some_and(|view| {
                    view.path.as_ref() == Some(path)
                        && view.editor.is_none()
                        && view.file.is_none()
                        && view.preview.as_ref().is_some_and(Vec::is_empty)
                })
            {
                return false;
            }
        }
        true
    }

    pub fn wrap_cleanup_ready(&self) -> bool {
        self.wrap.owner.is_none()
    }

    /// Mutation/cursor and legacy body/highlight fallback stay gated while an
    /// accepted exit owns the host, including a ticket waiting for admission.
    pub fn wrap_input_blocked(&self) -> bool {
        self.wrap.owner.is_some() && self.wrap.exit_accepted
    }
    /// Only the unique currently registered owner may open document cleanup.
    /// A receipt from another workspace or an earlier owner is returned whole.
    pub fn acknowledge_wrap_settled(&mut self, receipt: WorkspaceWrapSettled) -> ControlFlow<WorkspaceWrapSettled, ()> {
        if self.wrap.owner != Some(receipt.owner) {
            return ControlFlow::Break(receipt);
        }
        self.wrap.owner = None;
        self.wrap.settling = false;
        ControlFlow::Continue(())
    }

    pub fn wrap_settlement_state(&self) -> WorkspaceWrapSettlementState {
        if self.wrap.failed.is_some() {
            WorkspaceWrapSettlementState::Failed
        } else if self.wrap.settling {
            WorkspaceWrapSettlementState::Pending
        } else if self.wrap.owner.is_some() {
            WorkspaceWrapSettlementState::Active
        } else {
            WorkspaceWrapSettlementState::Ready
        }
    }

    /// Call ONLY after this host has accepted exit and resolved its save/dialog
    /// decision. Forced GUI exit can have a host-local accepted close. This
    /// records that decision; request admission still waits for loans/saves and
    /// pending disposal/document work. It never starts cleanup by itself.
    pub fn begin_wrap_shutdown(&mut self) {
        self.wrap.exit_accepted = true;
    }

    /// Cached readiness before a frontend consumes its adapter/host. Accepted
    /// work keeps its existing event path until this becomes true.
    pub fn wrap_shutdown_can_submit(&self) -> bool {
        self.wrap.owner.is_some()
            && self.wrap.exit_accepted
            && !self.wrap.settling
            && self.wrap.failed.is_none()
            && !self.file_saves_pending()
            && !self.search_replacement_pending()
            && !self.files.preparing
            && self.files.deferred_recovery.is_none()
            && self.files.queue.is_empty()
            && self.files.worker.is_some()
    }

    /// Accepted exit, not ordinary tab close. Caller keeps the exact ticket on
    /// Busy/foreign binding/disconnection and retries ONLY on an existing event.
    /// The worker runs this finite operation on its existing request lane.
    pub fn submit_wrap_settlement(
        &mut self,
        ticket: WorkspaceWrapSettlementTicket,
        timeout: Duration,
    ) -> ControlFlow<WorkspaceWrapSettlementTicket, ()> {
        if self.wrap.owner != Some(ticket.owner) || !self.wrap_shutdown_can_submit() {
            return ControlFlow::Break(ticket);
        }
        let Some(worker) = &self.files.worker else { return ControlFlow::Break(ticket) };
        match worker.try_submit(crate::document_worker::DocumentRequest::SettleWrap { ticket, timeout }) {
            Ok(()) => {
                self.wrap.settling = true;
                ControlFlow::Continue(())
            }
            Err(error) => {
                let (std::sync::mpsc::TrySendError::Full(request)
                | std::sync::mpsc::TrySendError::Disconnected(request)) = error;
                let crate::document_worker::DocumentRequest::SettleWrap { ticket, .. } = *request else {
                    unreachable!("wrap settlement admission returned a different request")
                };
                ControlFlow::Break(ticket)
            }
        }
    }

    /// Failure stays explicit until the caller requests retry. No worker wake
    /// or ordinary input silently retries a terminal or timed-out settlement.
    pub fn retry_wrap_settlement(&mut self, timeout: Duration) -> WrapResult<()> {
        let failure = self.wrap.failed.take().ok_or(WrapError::Invalid("no failed wrap settlement"))?;
        match self.submit_wrap_settlement(*failure.ticket, timeout) {
            ControlFlow::Continue(()) => Ok(()),
            ControlFlow::Break(ticket) => {
                self.wrap.failed =
                    Some(WorkspaceWrapSettlementFailure { ticket: Box::new(ticket), error: failure.error });
                Err(WrapError::Busy)
            }
        }
    }

    pub fn wrap_settlement_error(&self) -> Option<&WrapError> {
        self.wrap.failed.as_ref().map(|failure| &failure.error)
    }

    pub fn accept_wrap_settlement(
        &mut self,
        result: Result<WorkspaceWrapSettled, WorkspaceWrapSettlementFailure>,
    ) -> ControlFlow<Result<WorkspaceWrapSettled, WorkspaceWrapSettlementFailure>, crate::Damage> {
        match result {
            Ok(receipt) => {
                if let ControlFlow::Break(receipt) = self.acknowledge_wrap_settled(receipt) {
                    return ControlFlow::Break(Ok(receipt));
                }
            }
            Err(failure) => {
                // There is one admitted operation for this bound owner. No
                // second result can replace its retained failed ticket.
                if self.wrap.owner == Some(failure.ticket.owner) && self.wrap.settling && self.wrap.failed.is_none() {
                    self.wrap.settling = false;
                    let notice = format!("Wrap shutdown failed: {}; exact ticket retained for retry", failure.error);
                    self.wrap.failed = Some(failure);
                    return ControlFlow::Continue(self.set_notice(notice));
                }
                return ControlFlow::Break(Err(failure));
            }
        }
        ControlFlow::Continue(crate::Damage { status: true, ..Default::default() })
    }
}

#[cfg(test)]
pub(crate) use tests::settle_until as settle_until_for_test;
#[cfg(test)]
#[path = "workspace_wrap_tests.rs"]
mod tests;
