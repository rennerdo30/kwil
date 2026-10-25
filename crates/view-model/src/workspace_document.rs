//! Frontend ownership of the existing bounded document lane (KW-21).
//!
//! ROOT inserts this child module in `workspace.rs`. The frontend supplies ONE
//! startup `AllocationBudget`, also passed to `WrapHost::with_budget`, even when
//! wrapping is disabled. Allocate its exception cells/settlement receipts before
//! receiving. This adapter adds no result slot, worker, channel, timer or UI wait.
//!
//! A failed Prepare submission is a STARTED failure: keep its exact transport and
//! refused request; the same worker remains in Workspace until explicit stop.
//! Stop transfers that worker without waiting. OFF-UI settlement must restore
//! loans/durable receipts to the original Workspace before document cleanup.
//! No API extracts the underlying ticket and prematurely releases this charge.
use super::Workspace;
#[cfg(test)]
use crate::document_worker::{DocumentResultReceiveError, DocumentResultSettlementTicket};
use crate::{
    document_worker::{
        DocumentDeliveryFailure, DocumentRequest, DocumentResult, DocumentResultReceiver,
        DocumentResultSettlementReason, DocumentResultTransportStatus, DocumentWorker, DocumentWrapRetryRefusal,
        DocumentWrapRetryToken, result_transport::DocumentResultInspection,
    },
    visual_rows::{AllocationBudget, AllocationCharge, WrapError},
};
use kwil_core_workspace::session::DocumentId;
use std::{
    fmt, io,
    mem::size_of,
    ops::ControlFlow,
    path::PathBuf,
    sync::mpsc::TrySendError,
    time::{Duration, Instant},
};

#[path = "workspace_document/commit.rs"]
pub(crate) mod commit;
pub(crate) use commit::DocumentCommit;

/// Fixed, owner-free information about the completion in the original slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResultKind {
    Wrap,
    Binary,
    Preview,
    Opened,
    Indexed,
    Recovered,
    Prepared,
    Saved,
    Replaced,
    RawReopened,
    HexPage,
    HexEdited,
    Journal,
    Persisted,
    Barrier,
    Capacity,
    Stopped,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResultInfo {
    pub kind: ResultKind,
    pub document: Option<DocumentId>,
    pub revision: Option<u64>,
    pub operation: Option<u64>,
    pub generation: Option<u64>,
    delivery: Option<crate::document_worker::result_transport::DocumentDeliveryId>,
}
impl ResultInfo {
    pub(crate) fn with_delivery(mut self, id: crate::document_worker::result_transport::DocumentDeliveryId) -> Self {
        self.delivery = Some(id);
        self
    }
    pub fn delivery(&self) -> Option<crate::document_worker::result_transport::DocumentDeliveryId> {
        self.delivery
    }
}

/// Search can acknowledge this identity without taking a new snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExactReplacementReceipt {
    pub key: kwil_core_search::worker::SearchKey,
    pub operation: u64,
    pub succeeded: bool,
}

#[derive(Debug)]
pub(crate) enum WorkspaceDocumentAcceptError {
    Admission(WrapError),
    Preparation(io::Error),
    ForeignWorkspace,
    Identity,
    PartialCommit,
    Receipt(crate::editor::receipts::ReceiptDeliveryRefusal),
}
impl fmt::Display for WorkspaceDocumentAcceptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Admission(error) => write!(f, "document acceptance admission: {error}"),
            Self::Preparation(error) => write!(f, "document acceptance preparation: {error}"),
            Self::ForeignWorkspace => f.write_str("document acceptance belongs to another Workspace"),
            Self::Identity => f.write_str("document acceptance identity changed"),
            Self::PartialCommit => f.write_str("document publication cannot replay a partial commit"),
            Self::Receipt(error) => write!(f, "document acceptance receipt: {error}"),
        }
    }
}
impl std::error::Error for WorkspaceDocumentAcceptError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Admission(error) => Some(error),
            Self::Preparation(error) => Some(error),
            Self::Receipt(error) => Some(error),
            _ => None,
        }
    }
}

pub(crate) fn stale_save_preparation(error: &io::Error) -> bool {
    error.get_ref().and_then(|error| error.downcast_ref::<crate::editor::save::SavePreparationRefusal>())
        == Some(&crate::editor::save::SavePreparationRefusal::StaleIdentity)
}

/// Only bounded paint metadata and scalar receipts cross the frontend boundary.
pub struct WorkspaceDocumentUpdate {
    damage: crate::Damage,
    pub replacement: Option<ExactReplacementReceipt>,
    pub barrier: Option<crate::document_worker::result_transport::DocumentBarrierReceipt>,
    _charge: crate::visual_rows::MetadataReservation,
}
impl WorkspaceDocumentUpdate {
    /// Borrow paint metadata while this update retains its allocation charge.
    pub fn damage(&self) -> &crate::Damage {
        &self.damage
    }
}
pub(crate) type DocumentUpdate = WorkspaceDocumentUpdate;
pub(crate) type DocumentCommitError = WorkspaceDocumentAcceptError;
pub(crate) enum DocumentCommitOutcome {
    Completed(DocumentUpdate),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecoveryDeferKind {
    HexLoans,
    Structural,
}
enum ShutdownReceiptFlush {
    Flushed,
    RetainedSavePreparation,
}

#[derive(Debug)]
pub enum DocumentShutdownBindReason {
    MissingWorker,
    ForeignWorker,
    Admission(WrapError),
    Transport(DocumentResultSettlementReason),
}
#[must_use = "binding refused without changing the original Workspace or transport"]
pub struct DocumentShutdownBindRefusal {
    pub workspace: Workspace,
    pub transport: WorkspaceDocumentTransport,
    pub reason: DocumentShutdownBindReason,
}
impl fmt::Debug for DocumentShutdownBindRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DocumentShutdownBindRefusal").field("reason", &self.reason).finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocumentShutdownStatus {
    ReplacementReceiptPending,
    WrapRetryReceiptPending,
    PrepublicationResetPending,
    /// Workspace/history/recipe pins are gone. Retire search before ACKing.
    SourcesReleased,
    Complete,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShutdownPhase {
    Drain,
    Barrier,
    SearchRelease,
    Stop,
    Complete,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocumentShutdownReason {
    Transport(DocumentResultSettlementReason),
    Receipt(DocumentReceiptRefusal),
    SavePreparation,
    DurabilityFailed,
    ContinuationRefused,
    MetadataAdmission,
    SearchReleaseOrder,
    SearchBindingRequired,
    RecoveryDeferred,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocumentReceiptRefusal {
    UnsupportedObserver,
    MissingSink,
    Full,
    Closed,
    WrongPermit,
    Admission,
}
impl From<crate::editor::receipts::ReceiptDeliveryRefusal> for DocumentReceiptRefusal {
    fn from(reason: crate::editor::receipts::ReceiptDeliveryRefusal) -> Self {
        use crate::editor::receipts::ReceiptDeliveryRefusal as R;
        match reason {
            R::UnsupportedObserver => Self::UnsupportedObserver,
            R::MissingSink => Self::MissingSink,
            R::Full => Self::Full,
            R::Closed => Self::Closed,
            R::WrongPermit => Self::WrongPermit,
            R::Admission => Self::Admission,
        }
    }
}
#[derive(Debug)]
pub enum DocumentSearchBinding {
    NoSearchWorker,
    SearchWorker(kwil_core_search::worker::SearchSourceReleaseKey),
}
#[must_use = "refusal returns the exact owner-free search release receipt"]
pub struct SearchInputsReleaseRefusal {
    pub receipt: kwil_core_search::worker::SearchInputsReleased,
    pub reason: DocumentShutdownReason,
}

/// A borrowed, retained shutdown owner. Public methods cannot extract Workspace,
/// source payloads, nested tickets, or private continuation authority.
#[must_use = "retain this exact bundle until actual document thread exit"]
pub struct WorkspaceDocumentShutdown {
    workspace: Option<Workspace>,
    ticket: Option<crate::document_worker::result_transport::BoundDocumentTicket>,
    permit: Option<crate::document_worker::DocumentContinuationPermit>,
    budget: AllocationBudget,
    charge: AdapterCharge,
    _bundle_charge: Option<crate::visual_rows::MetadataReservation>,
    phase: ShutdownPhase,
    deadline: Option<Instant>,
    barrier: Option<crate::document_worker::result_transport::DocumentBarrierId>,
    barrier_receipt: Option<crate::document_worker::result_transport::DocumentBarrierReceipt>,
    replacements: [Option<ExactReplacementReceipt>; kwil_core_workspace::session::MAX_TABS],
    search_released: bool,
    search_receipt: Option<kwil_core_search::worker::SearchInputsReleased>,
    expected_search: Option<kwil_core_search::worker::SearchSourceReleaseKey>,
    search_binding_decided: bool,
    durability_targets: [Option<(DocumentId, u64, u64)>;
        kwil_core_workspace::session::MAX_TABS + kwil_core_workspace::session::MAX_CLOSED],
    flush_requested: bool,
    preparation_error: Option<WorkspaceDocumentAcceptError>,
    #[cfg(test)]
    before_retirement_wait: Option<std::sync::mpsc::SyncSender<retry_ack_tests::RetirementObservation>>,
}

impl Workspace {
    pub fn into_document_shutdown(
        self,
        transport: WorkspaceDocumentTransport,
    ) -> ControlFlow<DocumentShutdownBindRefusal, WorkspaceDocumentShutdown> {
        let Some(worker) = &self.files.worker else {
            return ControlFlow::Break(DocumentShutdownBindRefusal {
                workspace: self,
                transport,
                reason: DocumentShutdownBindReason::MissingWorker,
            });
        };
        if !transport.receiver.matches_worker(worker) {
            return ControlFlow::Break(DocumentShutdownBindRefusal {
                workspace: self,
                transport,
                reason: DocumentShutdownBindReason::ForeignWorker,
            });
        }
        // Reserve all fixed cells/return overlap and the EXISTING bounded
        // request deque's maximum before fencing or constructing a bundle.
        let metadata = 2 * size_of::<WorkspaceDocumentShutdown>()
            + size_of::<DocumentShutdownBindRefusal>()
            + 512 * size_of::<DocumentRequest>()
            + kwil_core_workspace::session::MAX_TABS
                * (size_of::<(DocumentId, bool, crate::CommandResult)>()
                    + size_of::<DocumentId>()
                    + size_of::<crate::Editor>()
                    + 4096);
        let bundle_charge = match transport.budget.reserve_metadata(metadata) {
            Ok(charge) => charge,
            Err(error) => {
                return ControlFlow::Break(DocumentShutdownBindRefusal {
                    workspace: self,
                    transport,
                    reason: DocumentShutdownBindReason::Admission(error),
                });
            }
        };
        let mut workspace = self;
        if workspace.files.queue.len() > 256
            || workspace.files.queue.try_reserve(256 - workspace.files.queue.len()).is_err()
        {
            return ControlFlow::Break(DocumentShutdownBindRefusal {
                workspace,
                transport,
                reason: DocumentShutdownBindReason::Admission(WrapError::Admission("document continuation queue")),
            });
        }
        let WorkspaceDocumentTransport { receiver, charge, budget } = transport;
        match receiver.begin_bound(workspace.files.worker.as_ref().unwrap()) {
            Ok((ticket, permit)) => {
                workspace.files.document_shutdown = true;
                ControlFlow::Continue(WorkspaceDocumentShutdown {
                    workspace: Some(workspace),
                    ticket: Some(ticket),
                    permit: Some(permit),
                    budget,
                    charge,
                    _bundle_charge: Some(bundle_charge),
                    phase: ShutdownPhase::Drain,
                    deadline: None,
                    barrier: None,
                    barrier_receipt: None,
                    replacements: [None; kwil_core_workspace::session::MAX_TABS],
                    search_released: false,
                    search_receipt: None,
                    expected_search: None,
                    search_binding_decided: false,
                    durability_targets: [None;
                        kwil_core_workspace::session::MAX_TABS + kwil_core_workspace::session::MAX_CLOSED],
                    flush_requested: false,
                    preparation_error: None,
                    #[cfg(test)]
                    before_retirement_wait: None,
                })
            }
            Err(refusal) => ControlFlow::Break(DocumentShutdownBindRefusal {
                workspace,
                transport: WorkspaceDocumentTransport { receiver: refusal.receiver, charge, budget },
                reason: DocumentShutdownBindReason::Transport(refusal.reason),
            }),
        }
    }
}

impl WorkspaceDocumentShutdown {
    /// OFF UI: the guard borrows the first retained transport refusal in place.
    pub fn retained_commit_error(&self) -> DocumentResultInspection<'_> {
        self.ticket.as_ref().unwrap().retained_commit_error()
    }
    /// Borrow the retained original refusal, including its nested I/O source.
    /// Formatting a notice must never replace this owner with a string copy.
    pub fn preparation_error(&self) -> Option<&(dyn std::error::Error + 'static)> {
        if let Some(error) = &self.preparation_error {
            return Some(error);
        }
        self.workspace.as_ref()?.files.saves.values().find_map(|flight| {
            flight.preparation_error.as_ref().map(|error| error as &(dyn std::error::Error + 'static))
        })
    }
    /// Share this exact deadline with search-input release and later stages.
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }
    pub fn status(&self) -> DocumentResultTransportStatus {
        self.ticket.as_ref().unwrap().status()
    }
    pub fn result_info(&self) -> Option<ResultInfo> {
        self.ticket.as_ref().unwrap().info()
    }
    pub fn replacement_receipt(&self) -> Option<ExactReplacementReceipt> {
        self.replacements.iter().flatten().copied().next()
    }
    pub fn acknowledge_replacement_receipt(&mut self, receipt: ExactReplacementReceipt) -> bool {
        let Some(slot) = self.replacements.iter_mut().find(|slot| **slot == Some(receipt)) else { return false };
        *slot = None;
        true
    }
    /// Capture the original search worker's opaque key before stopping it.
    /// Refusal returns this exact scalar key without changing the binding.
    pub fn bind_search_sources(&mut self, binding: DocumentSearchBinding) -> Result<(), DocumentSearchBinding> {
        if self.deadline.is_some() || self.search_binding_decided {
            return Err(binding);
        }
        self.expected_search = match binding {
            DocumentSearchBinding::NoSearchWorker => None,
            DocumentSearchBinding::SearchWorker(key) => Some(key),
        };
        self.search_binding_decided = true;
        Ok(())
    }
    pub fn acknowledge_search_inputs_released(
        &mut self,
        receipt: kwil_core_search::worker::SearchInputsReleased,
    ) -> Result<(), SearchInputsReleaseRefusal> {
        if self.phase != ShutdownPhase::SearchRelease
            || !self.expected_search.as_ref().is_some_and(|key| receipt.valid_for(key))
        {
            return Err(SearchInputsReleaseRefusal { receipt, reason: DocumentShutdownReason::SearchReleaseOrder });
        }
        self.search_receipt = Some(receipt);
        self.search_released = true;
        Ok(())
    }
    pub fn acknowledge_delivery_failure(&mut self) -> Option<DocumentDeliveryFailure> {
        self.ticket.as_mut().unwrap().acknowledge_delivery_failure()
    }
    pub fn request_wrap_retry(
        &mut self,
        timeout: Duration,
    ) -> Result<DocumentWrapRetryToken, DocumentResultSettlementReason> {
        if !self.search_binding_decided {
            return Err(DocumentResultSettlementReason::RetryUnavailable);
        }
        let deadline =
            *self.deadline.get_or_insert_with(|| Instant::now().checked_add(timeout).unwrap_or_else(Instant::now));
        self.ticket.as_mut().unwrap().request_wrap_retry(deadline.saturating_duration_since(Instant::now()))
    }
    pub fn acknowledge_wrap_retry(&mut self, token: DocumentWrapRetryToken) -> Result<(), DocumentWrapRetryRefusal> {
        self.ticket.as_mut().unwrap().acknowledge_wrap_retry(token)
    }
    pub fn request_prepublication_reset(
        &mut self,
        delivery: crate::document_worker::result_transport::DocumentDeliveryId,
        timeout: Duration,
    ) -> Result<crate::document_worker::DocumentProducerRetryToken, DocumentResultSettlementReason> {
        if !self.search_binding_decided || !matches!(self.phase, ShutdownPhase::Drain | ShutdownPhase::Barrier) {
            return Err(DocumentResultSettlementReason::RetryUnavailable);
        }
        let deadline =
            *self.deadline.get_or_insert_with(|| Instant::now().checked_add(timeout).unwrap_or_else(Instant::now));
        self.ticket
            .as_mut()
            .unwrap()
            .request_prepublication_reset(delivery, deadline.saturating_duration_since(Instant::now()))
    }
    pub fn acknowledge_prepublication_reset(
        &mut self,
        token: crate::document_worker::DocumentProducerRetryToken,
    ) -> Result<(), crate::document_worker::DocumentProducerRetryRefusal> {
        if self.deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(crate::document_worker::DocumentProducerRetryRefusal {
                token,
                reason: DocumentResultSettlementReason::TimedOut,
            });
        }
        self.ticket.as_mut().unwrap().acknowledge_prepublication_reset(self.workspace.as_ref().unwrap(), token)
    }
    /// OFF UI. Every retry retains this bundle and the first absolute deadline.
    /// The loop waits on real slot/capacity notifications; no idle poll or timer.
    pub fn settle(&mut self, timeout: Duration) -> Result<DocumentShutdownStatus, DocumentShutdownReason> {
        let deadline = Instant::now().checked_add(timeout).unwrap_or_else(Instant::now);
        self.settle_until(deadline)
    }
    /// OFF UI. Share the caller's accepted deadline with every shutdown stage.
    /// A later call can shorten this deadline but can never extend it.
    pub fn settle_until(&mut self, deadline: Instant) -> Result<DocumentShutdownStatus, DocumentShutdownReason> {
        if !self.search_binding_decided {
            return Err(DocumentShutdownReason::SearchBindingRequired);
        }
        let deadline = self.deadline.map_or(deadline, |retained| retained.min(deadline));
        self.deadline = Some(deadline);
        if self.phase == ShutdownPhase::Complete {
            return Ok(DocumentShutdownStatus::Complete);
        }
        loop {
            if Instant::now() >= deadline {
                return Err(DocumentShutdownReason::Transport(DocumentResultSettlementReason::TimedOut));
            }
            match self.phase {
                ShutdownPhase::Drain | ShutdownPhase::Barrier => {
                    if self.status().retry_acknowledgement_required
                        && self.result_info().is_some_and(|info| info.kind == ResultKind::Saved)
                    {
                        return Ok(DocumentShutdownStatus::PrepublicationResetPending);
                    }
                    if self.workspace.as_ref().unwrap().files.deferred_recovery_kind
                        == Some(RecoveryDeferKind::Structural)
                    {
                        return Err(DocumentShutdownReason::RecoveryDeferred);
                    }
                    if self.status().retry_acknowledgement_required
                        && !self.status().occupied
                        && self.workspace.as_ref().unwrap().wrap_cleanup_ready()
                    {
                        return Ok(DocumentShutdownStatus::WrapRetryReceiptPending);
                    }
                    if self.replacement_receipt().is_some() {
                        return Ok(DocumentShutdownStatus::ReplacementReceiptPending);
                    }
                    let update = self
                        .ticket
                        .as_mut()
                        .unwrap()
                        .accept_bound(
                            self.workspace.as_mut().unwrap(),
                            &self.budget,
                            self.permit.as_ref().unwrap(),
                            deadline,
                        )
                        .map_err(DocumentShutdownReason::Transport)?;
                    if let Some(update) = update {
                        if let Some(receipt) = update.replacement {
                            let Some(slot) = self.replacements.iter_mut().find(|slot| slot.is_none()) else {
                                return Err(DocumentShutdownReason::MetadataAdmission);
                            };
                            *slot = Some(receipt);
                        }
                        if let Some(receipt) = update.barrier {
                            self.barrier_receipt = Some(receipt);
                        }
                        // Only fixed paint metadata is discarded OFF UI here.
                        drop(update);
                        if self.workspace.as_ref().unwrap().files.deferred_recovery_kind
                            == Some(RecoveryDeferKind::Structural)
                        {
                            return Err(DocumentShutdownReason::RecoveryDeferred);
                        }
                        if self.status().retry_acknowledgement_required
                            && !self.status().occupied
                            && self.workspace.as_ref().unwrap().wrap_cleanup_ready()
                        {
                            return Ok(DocumentShutdownStatus::WrapRetryReceiptPending);
                        }
                    }
                    let workspace = self.workspace.as_mut().unwrap();
                    match workspace.flush_document_shutdown_receipts(&self.budget, self.permit.as_ref().unwrap()) {
                        Ok(ShutdownReceiptFlush::Flushed) => {}
                        Ok(ShutdownReceiptFlush::RetainedSavePreparation) => {
                            return Err(DocumentShutdownReason::SavePreparation);
                        }
                        Err(error) => {
                            let reason = match &error {
                                WorkspaceDocumentAcceptError::Receipt(error) => {
                                    DocumentShutdownReason::Receipt((*error).into())
                                }
                                _ => DocumentShutdownReason::SavePreparation,
                            };
                            if self.preparation_error.is_none() {
                                self.preparation_error = Some(error);
                            }
                            return Err(reason);
                        }
                    }
                    workspace.queue_bound_hex_continuations()?;
                    workspace.prepare_binary_shutdown();
                    let mut targets =
                        [None; kwil_core_workspace::session::MAX_TABS + kwil_core_workspace::session::MAX_CLOSED];
                    let mut count = 0;
                    let mut needs_flush = false;
                    for (&id, view) in workspace.views.iter().chain(workspace.closed_views.iter()) {
                        let Some(editor) = view.editor() else { continue };
                        let next = editor.next_edit_sequence();
                        if editor.document().is_dirty()
                            || workspace.files.seeded.contains(&id)
                            || workspace.files.observed.get(&id).is_some_and(|(first, _)| next > *first)
                        {
                            let target = (id, editor.document().revision(), next.saturating_sub(1));
                            if count == targets.len() {
                                return Err(DocumentShutdownReason::MetadataAdmission);
                            }
                            targets[count] = Some(target);
                            count += 1;
                            needs_flush |= workspace.files.durable.get(&id) != Some(&(target.1, target.2));
                        }
                    }
                    targets[..count].sort_unstable_by_key(|target| target.unwrap().0.0);
                    if targets != self.durability_targets {
                        self.durability_targets = targets;
                        self.flush_requested = false;
                    }
                    if needs_flush && !self.flush_requested && workspace.files.queue.len() < 256 {
                        workspace.files.queue.push_back(DocumentRequest::FlushJournals);
                        self.flush_requested = true;
                    }
                    if workspace.files.deferred_recovery.is_some() && workspace.hex.loans.is_empty() {
                        match self.permit.as_ref().unwrap().restore_prepared(&mut workspace.files.deferred_recovery) {
                            Ok(()) => {
                                workspace.files.preparing = true;
                                workspace.files.deferred_recovery_kind = None;
                            }
                            Err(reason) if reason.is_full() => {}
                            Err(_) => return Err(DocumentShutdownReason::ContinuationRefused),
                        }
                    }
                    if workspace.files.storage_ready
                        && workspace.files.persist_pending
                        && !workspace.files.persist_inflight
                    {
                        workspace.persist_snapshot();
                    }
                    let progress = workspace.pump_document_continuations(self.permit.as_ref().unwrap())?;
                    if workspace.document_shutdown_drained()? {
                        self.phase = ShutdownPhase::Barrier;
                        if let Some(receipt) = self.barrier_receipt.take() {
                            if self.barrier == Some(receipt.id())
                                && self.permit.as_mut().unwrap().seal_sources(&receipt).is_ok()
                            {
                                // No source-returning continuation can be minted
                                // after this point. Release Workspace pins before
                                // closing the backing-owning worker's sender.
                                workspace.release_document_sources();
                                self.search_released = self.expected_search.is_none();
                                self.phase = ShutdownPhase::SearchRelease;
                                return Ok(DocumentShutdownStatus::SourcesReleased);
                            }
                            self.barrier = None;
                        }
                        if self.barrier.is_none() {
                            match self.permit.as_ref().unwrap().request_barrier() {
                                Ok(id) => self.barrier = Some(id),
                                Err(reason) if reason.is_full() => {}
                                Err(_) => return Err(DocumentShutdownReason::ContinuationRefused),
                            }
                        }
                    } else {
                        self.phase = ShutdownPhase::Drain;
                    }
                    if progress {
                        continue;
                    }
                    #[cfg(test)]
                    if self.status().retry_acknowledgement_required
                        && self.status().occupied
                        && self.workspace.as_ref().unwrap().wrap_cleanup_ready()
                        && let Some(observe) = self.before_retirement_wait.take()
                    {
                        observe.try_send(retry_ack_tests::RetirementObservation::BeforeWait).unwrap();
                    }
                    self.ticket.as_ref().unwrap().wait_until(deadline).map_err(DocumentShutdownReason::Transport)?;
                }
                ShutdownPhase::SearchRelease => {
                    if !self.search_released {
                        return Ok(DocumentShutdownStatus::SourcesReleased);
                    }
                    if let Some(key) = &self.expected_search
                        && !self.search_receipt.as_ref().is_some_and(|receipt| receipt.valid_for(key))
                    {
                        return Err(DocumentShutdownReason::SearchReleaseOrder);
                    }
                    self.ticket
                        .as_mut()
                        .unwrap()
                        .close_after_source_release(self.permit.as_ref().unwrap())
                        .map_err(DocumentShutdownReason::Transport)?;
                    self.phase = ShutdownPhase::Stop;
                }
                ShutdownPhase::Stop => {
                    self.ticket.as_mut().unwrap().finish_until(deadline).map_err(DocumentShutdownReason::Transport)?;
                    self.phase = ShutdownPhase::Complete;
                    // Worker sources and actual JoinHandle are now retired.
                    self.workspace.take();
                    self.permit.take();
                    self.ticket.take();
                    self.preparation_error.take();
                    drop(self.charge.0.take());
                    drop(self._bundle_charge.take());
                    return Ok(DocumentShutdownStatus::Complete);
                }
                ShutdownPhase::Complete => return Ok(DocumentShutdownStatus::Complete),
            }
        }
    }
}
impl Drop for WorkspaceDocumentShutdown {
    fn drop(&mut self) {
        if self.phase != ShutdownPhase::Complete {
            // Incorrect UI destruction is quarantined, never called cleanup.
            // Safe leaks retain exact owners; no source destructor or detach.
            if let Some(workspace) = self.workspace.take() {
                std::mem::forget(workspace);
            }
            if let Some(ticket) = self.ticket.take() {
                std::mem::forget(ticket);
            }
            if let Some(permit) = self.permit.take() {
                std::mem::forget(permit);
            }
            if let Some(charge) = self._bundle_charge.take() {
                std::mem::forget(charge);
            }
            if let Some(error) = self.preparation_error.take() {
                std::mem::forget(error);
            }
        }
    }
}

impl Workspace {
    /// Scalar work for an accepted save. Full retains the request in its existing
    /// SaveFlight; only an actual capacity/result event calls this pump again.
    pub(super) fn pump_live_save_resumes(&mut self) {
        if self.files.document_shutdown || self.files.preparing {
            return;
        }
        let Some(worker) = &self.files.worker else { return };
        for (&id, flight) in &mut self.files.saves {
            if flight.resume != super::SaveResumePhase::Needed {
                continue;
            }
            let request = DocumentRequest::ResumeSaveCompletion { key: flight.resume_key(id) };
            match worker.try_submit(request) {
                Ok(()) => flight.resume = super::SaveResumePhase::Queued,
                Err(_) => break, // exact source and original error remain guarded
            }
        }
    }

    pub fn save_preparation_error(&self, document: DocumentId) -> Option<&io::Error> {
        self.files.saves.get(&document)?.preparation_error.as_ref()
    }

    fn pump_document_continuations(
        &mut self,
        permit: &crate::document_worker::DocumentContinuationPermit,
    ) -> Result<bool, DocumentShutdownReason> {
        if self.files.preparing {
            return Ok(false);
        }
        let mut progress = false;
        while let Some(request) = self.files.queue.pop_front() {
            match permit.submit(request) {
                ControlFlow::Continue(()) => progress = true,
                ControlFlow::Break((request, reason)) => {
                    self.files.queue.push_front(request);
                    if reason.is_full() {
                        break;
                    }
                    return Err(DocumentShutdownReason::ContinuationRefused);
                }
            }
        }
        while let Some((document, editor)) = self.binary.retiring.pop() {
            match permit.submit(DocumentRequest::DiscardHex { document, editor }) {
                ControlFlow::Continue(()) => progress = true,
                ControlFlow::Break((DocumentRequest::DiscardHex { document, editor }, reason)) => {
                    self.binary.retiring.push((document, editor));
                    if reason.is_full() {
                        break;
                    }
                    return Err(DocumentShutdownReason::ContinuationRefused);
                }
                ControlFlow::Break(_) => unreachable!("private submit returns its exact request"),
            }
        }
        for (&id, flight) in &mut self.files.saves {
            if flight.resume != super::SaveResumePhase::Needed {
                continue;
            }
            match permit.submit(DocumentRequest::ResumeSaveCompletion { key: flight.resume_key(id) }) {
                ControlFlow::Continue(()) => {
                    flight.resume = super::SaveResumePhase::Queued;
                    progress = true;
                }
                ControlFlow::Break((_, reason)) if reason.is_full() => break,
                ControlFlow::Break(_) => return Err(DocumentShutdownReason::ContinuationRefused),
            }
        }
        Ok(progress)
    }
    fn flush_document_shutdown_receipts(
        &mut self,
        budget: &AllocationBudget,
        permit: &crate::document_worker::DocumentContinuationPermit,
    ) -> Result<ShutdownReceiptFlush, WorkspaceDocumentAcceptError> {
        // One fixed result cell per tab, admitted in the bound bundle header.
        let mut finished: [Option<(DocumentId, bool, crate::CommandResult)>; kwil_core_workspace::session::MAX_TABS] =
            std::array::from_fn(|_| None);
        // A later tab may refuse after an earlier Editor has already published
        // its save. Keep that exact scan outcome until every completed result
        // in the admitted fixed array has finalized its original SaveFlight.
        let outcome = (|| {
            let mut next = 0;
            let mut outcome = ShutdownReceiptFlush::Flushed;
            for (&id, view) in &mut self.views {
                let Some(editor) = view.editor_mut() else { continue };
                match editor.flush_edit_receipt_sealed(Some(permit)) {
                    Ok(()) => {}
                    Err(crate::editor::receipts::ReceiptDeliveryRefusal::Full) => continue,
                    Err(error) => return Err(WorkspaceDocumentAcceptError::Receipt(error)),
                }
                if editor.has_pending_save_completion() {
                    let Some(save) = self.files.saves.get_mut(&id) else {
                        return Err(WorkspaceDocumentAcceptError::Identity);
                    };
                    if save.resume == super::SaveResumePhase::Exhausted {
                        return Err(WorkspaceDocumentAcceptError::Identity);
                    }
                    if save.resume != super::SaveResumePhase::Local {
                        continue;
                    }
                    let current = save.revision == editor.document().revision();
                    match editor.finish_pending_save_sealed(budget) {
                        Ok(Some(result)) => {
                            finished[next] = Some((id, current, result));
                            next += 1;
                        }
                        Ok(None) => {}
                        Err(error) if stale_save_preparation(&error) => {
                            save.retain_stale_preparation(
                                editor.document().source_epoch,
                                &mut self.files.next_save_resume,
                                error,
                            );
                        }
                        Err(error) if editor.has_pending_save_completion() => {
                            let blocked = error.kind() == io::ErrorKind::WouldBlock;
                            if save.preparation_error.is_none() {
                                save.preparation_error = Some(error);
                            }
                            if !blocked {
                                outcome = ShutdownReceiptFlush::RetainedSavePreparation;
                            }
                        }
                        Err(error) => return Err(WorkspaceDocumentAcceptError::Preparation(error)),
                    }
                }
            }
            Ok(outcome)
        })();
        for (id, current, result) in finished.into_iter().flatten() {
            self.complete_save(id, current, Ok(result));
            self.checkpoint_session();
        }
        outcome
    }
    fn document_shutdown_drained(&self) -> Result<bool, DocumentShutdownReason> {
        if !self.files.journal_failed.is_empty() {
            return Err(DocumentShutdownReason::DurabilityFailed);
        }
        if !self.wrap_cleanup_ready()
            || self.files.preparing
            || !self.files.queue.is_empty()
            || !self.binary.flights.is_empty()
            || !self.binary.retiring.is_empty()
            || !self.hex.loans.is_empty()
            || self.hex.opening()
            || self.file_saves_pending()
            || self.files.deferred_recovery.is_some()
            || self.files.persist_pending
            || self.files.persist_inflight
        {
            return Ok(false);
        }
        if self.files.storage_ready && !self.files.exit_without_metadata && !self.files.metadata_durable {
            return Ok(false);
        }
        // These are exact revision/sequence targets, never queue emptiness.
        for (&id, view) in self.views.iter().chain(self.closed_views.iter()) {
            let Some(editor) = view.editor() else { continue };
            if editor.has_pending_edit_receipt() || editor.has_pending_save_completion() {
                return Ok(false);
            }
            let next = editor.next_edit_sequence();
            let needs = editor.document().is_dirty()
                || self.files.seeded.contains(&id)
                || self.files.observed.get(&id).is_some_and(|(first, _)| next > *first);
            if needs && self.files.durable.get(&id) != Some(&(editor.document().revision(), next.saturating_sub(1))) {
                return Ok(false);
            }
        }
        Ok(true)
    }
    fn release_document_sources(&mut self) {
        // OFF UI only, after the private source-return barrier and exact
        // durability. Keep scalar error/identity/target metadata through stop.
        self.views.clear();
        self.closed_views.clear();
        self.files.deferred_recovery = None;
        self.binary.retiring.clear();
        self.hex = super::HexFlow::default();
    }
}

#[derive(Debug)]
pub enum WorkspaceDocumentStartError {
    WorkerAlreadyPresent,
    Admission(WrapError),
    Allocation(std::collections::TryReserveError),
    Spawn(io::Error),
}

/// No worker was started, or the exact started transport/request came back.
/// `PrepareRefused` must enter the frontend's preadmitted shutdown owner cell;
/// converting it into an io::Error would lose the receiver and is incorrect.
#[must_use = "a started refusal retains the exact receiver and refused Prepare request"]
pub enum WorkspaceDocumentStartFailure {
    NotStarted { root: Option<PathBuf>, error: WorkspaceDocumentStartError },
    PrepareRefused(WorkspaceDocumentPrepareRefusal),
}
impl fmt::Debug for WorkspaceDocumentStartFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotStarted { error, .. } => f.debug_tuple("NotStarted").field(error).finish(),
            Self::PrepareRefused(refusal) => f.debug_tuple("PrepareRefused").field(refusal).finish(),
        }
    }
}

#[must_use = "Workspace still owns this transport's worker; stop with this exact transport"]
pub struct WorkspaceDocumentPrepareRefusal {
    pub transport: WorkspaceDocumentTransport,
    /// Reuses the worker's existing refusal allocation; no new box or source cell.
    pub error: TrySendError<Box<DocumentRequest>>,
}
impl fmt::Debug for WorkspaceDocumentPrepareRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkspaceDocumentPrepareRefusal").finish_non_exhaustive()
    }
}

#[must_use = "retain with the Workspace worker until actual off-UI settlement"]
pub struct WorkspaceDocumentTransport {
    receiver: DocumentResultReceiver,
    charge: AdapterCharge,
    budget: AllocationBudget,
}

// A mistaken Drop is NOT successful cleanup. The original transport already
// quarantines its worker/slot in that case; keep our reservation quarantined as
// well (including initial destinations still owned by that worker). No source
// owner, allocation, thread or separate failure slot is introduced here.
struct AdapterCharge(Option<AllocationCharge>);
impl AdapterCharge {
    #[cfg(test)]
    fn release(mut self) {
        drop(self.0.take());
    }
}
impl Drop for AdapterCharge {
    fn drop(&mut self) {
        if let Some(charge) = self.0.take() {
            std::mem::forget(charge);
        }
    }
}
impl WorkspaceDocumentTransport {
    /// OFF UI: no source owner or nested error is extracted from the slot.
    pub fn retained_commit_error(&self) -> DocumentResultInspection<'_> {
        self.receiver.retained_commit_error()
    }
    pub fn status(&self) -> DocumentResultTransportStatus {
        self.receiver.status()
    }
    /// Sealed live receive. The original slot and guard own every source until
    /// Workspace installs it; successful residual retirement runs on the worker.
    pub fn receive(
        &mut self,
        workspace: &mut Workspace,
        admitted: bool,
    ) -> Result<Option<WorkspaceDocumentUpdate>, DocumentResultSettlementReason> {
        let update = self.receiver.accept_workspace(workspace, &self.budget, admitted)?;
        if update.is_some() || !admitted || self.receiver.status().occupied {
            return Ok(update);
        }
        if workspace.files.worker.as_ref().is_none_or(|worker| !self.receiver.matches_worker(worker))
            || workspace.files.document_budget.is_none()
        {
            return Err(DocumentResultSettlementReason::ConsumerRefused);
        }
        // A real empty-slot capacity wake can unblock an Editor-retained save.
        // Admit the return metadata BEFORE touching its guarded receipt/state.
        let charge = self
            .budget
            .reserve_metadata(size_of::<WorkspaceDocumentUpdate>() + 8192 * size_of::<usize>())
            .map_err(|_| DocumentResultSettlementReason::ConsumerRefused)?;
        // The transport consumes a monotonic real-capacity generation below;
        // repeated empty receives alone are never authority to run model work.
        if !self.receiver.take_capacity_event() {
            return Ok(None);
        }
        let damage = workspace.flush_receipts();
        Ok(Some(WorkspaceDocumentUpdate { damage, replacement: None, barrier: None, _charge: charge }))
    }
    /// A scalar harness inspection never takes the result out of its slot.
    pub fn result_info(&self) -> Option<ResultInfo> {
        self.receiver.info()
    }
    /// One explicit owner-free event after a harness held the original slot
    /// for its preview frame. This does not receive, allocate, or arm a timer.
    pub fn wake_ready(&self) {
        self.receiver.wake_ready();
    }
    /// One receive per admitted event. False moves no owner. The frontend must
    /// preadmit space for every exception/receipt before saying true.
    #[cfg(test)]
    fn try_recv(&mut self, admitted: bool) -> Result<Option<DocumentResult>, DocumentResultReceiveError> {
        self.receiver.try_recv(admitted)
    }
    #[cfg(test)]
    fn close(&mut self) {
        self.receiver.close();
    }
    pub fn request_wrap_retry(
        &mut self,
        timeout: Duration,
    ) -> Result<DocumentWrapRetryToken, DocumentResultSettlementReason> {
        self.receiver.request_wrap_retry(timeout)
    }
    /// Call after the ORIGINAL Workspace accepted the matching retry receipt.
    pub fn acknowledge_wrap_retry(&mut self, token: DocumentWrapRetryToken) -> Result<(), DocumentWrapRetryRefusal> {
        self.receiver.acknowledge_wrap_retry(token)
    }
    pub fn request_prepublication_reset(
        &mut self,
        delivery: crate::document_worker::result_transport::DocumentDeliveryId,
        timeout: Duration,
    ) -> Result<crate::document_worker::DocumentProducerRetryToken, DocumentResultSettlementReason> {
        self.receiver.request_prepublication_reset(delivery, timeout)
    }
    pub fn acknowledge_prepublication_reset(
        &mut self,
        workspace: &Workspace,
        token: crate::document_worker::DocumentProducerRetryToken,
    ) -> Result<(), crate::document_worker::DocumentProducerRetryRefusal> {
        self.receiver.acknowledge_prepublication_reset(workspace, token)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceDocumentStopReason {
    MissingWorker,
    ForeignWorker,
}
#[must_use = "stop refused; Workspace and this exact transport retain their original owners"]
pub struct WorkspaceDocumentStopRefusal {
    pub transport: WorkspaceDocumentTransport,
    pub reason: WorkspaceDocumentStopReason,
}
impl fmt::Debug for WorkspaceDocumentStopRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkspaceDocumentStopRefusal").field("reason", &self.reason).finish_non_exhaustive()
    }
}

#[must_use = "retain the charged ticket until off-UI settlement actually succeeds"]
#[cfg(test)]
pub struct WorkspaceDocumentSettlementTicket {
    ticket: DocumentResultSettlementTicket,
    charge: AdapterCharge,
    live_charge: Option<crate::visual_rows::MetadataReservation>,
}
#[must_use = "failure retains the exact underlying ticket AND its adapter charge"]
#[cfg(test)]
pub struct WorkspaceDocumentSettlementFailure {
    pub ticket: WorkspaceDocumentSettlementTicket,
    pub reason: DocumentResultSettlementReason,
}
#[cfg(test)]
impl fmt::Debug for WorkspaceDocumentSettlementFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkspaceDocumentSettlementFailure").field("reason", &self.reason).finish_non_exhaustive()
    }
}
#[cfg(test)]
impl WorkspaceDocumentSettlementTicket {
    pub fn status(&self) -> DocumentResultTransportStatus {
        self.ticket.status()
    }
    /// OFF UI: borrowed inspection delegates to the single original result slot.
    pub fn inspect(&self) -> DocumentResultInspection<'_> {
        self.ticket.inspect()
    }
    pub fn acknowledge_delivery_failure(&mut self) -> Option<DocumentDeliveryFailure> {
        self.ticket.acknowledge_delivery_failure()
    }
    pub fn acknowledge_wrap_retry(&mut self, token: DocumentWrapRetryToken) -> Result<(), DocumentWrapRetryRefusal> {
        self.ticket.acknowledge_wrap_retry(token)
    }
    pub fn retry_pending_wrap(&mut self, timeout: Duration) -> Result<bool, DocumentResultSettlementReason> {
        self.ticket.retry_pending_wrap(timeout)
    }
    /// OFF UI ONLY. Restore every accepted loan/receipt in the original
    /// Workspace; Break(EXACT result) refuses without disposal. Explicit retry
    /// and failure ACK controls remain those of the original transport ticket.
    pub fn settle(
        self,
        timeout: Duration,
        accept: impl FnMut(DocumentResult) -> ControlFlow<DocumentResult, ()>,
    ) -> Result<(), WorkspaceDocumentSettlementFailure> {
        let Self { ticket, charge, live_charge } = self;
        match ticket.settle(timeout, accept) {
            Ok(()) => {
                charge.release();
                drop(live_charge);
                Ok(())
            }
            Err(failure) => Err(WorkspaceDocumentSettlementFailure {
                ticket: Self { ticket: failure.ticket, charge, live_charge },
                reason: failure.reason,
            }),
        }
    }
}

impl Workspace {
    /// Start after first paint, using the SAME frontend aggregate as WrapHost.
    /// Root selection is explicit: None keeps the existing worker's normal
    /// root resolution, and Some(path) preserves that exact native path.
    pub fn start_bounded_document_worker_at(
        &mut self,
        root: Option<PathBuf>,
        budget: &AllocationBudget,
        wake: impl Fn() -> io::Result<()> + Send + Sync + 'static,
    ) -> Result<WorkspaceDocumentTransport, WorkspaceDocumentStartFailure> {
        self.start_bounded_document_worker_with(
            root,
            budget,
            wake,
            DocumentWorker::spawn_bounded,
            |worker, _, request| worker.try_submit(request),
        )
    }

    // Injection is private to this new module. Tests exercise real bounded
    // workers while forcing the normally rare post-spawn admission refusal.
    fn start_bounded_document_worker_with<W>(
        &mut self,
        root: Option<PathBuf>,
        budget: &AllocationBudget,
        wake: W,
        spawn: impl FnOnce(&AllocationBudget, W) -> io::Result<(DocumentWorker, DocumentResultReceiver)>,
        submit: impl FnOnce(
            &DocumentWorker,
            &mut DocumentResultReceiver,
            DocumentRequest,
        ) -> Result<(), TrySendError<Box<DocumentRequest>>>,
    ) -> Result<WorkspaceDocumentTransport, WorkspaceDocumentStartFailure>
    where
        W: Fn() -> io::Result<()> + Send + Sync + 'static,
    {
        if self.files.worker.is_some() {
            return Err(WorkspaceDocumentStartFailure::NotStarted {
                root,
                error: WorkspaceDocumentStartError::WorkerAlreadyPresent,
            });
        }
        let (charge, destinations) = match admit_start(self, root.as_ref(), budget) {
            Ok(admitted) => admitted,
            Err(error) => return Err(WorkspaceDocumentStartFailure::NotStarted { root, error }),
        };
        let (worker, receiver) = match spawn(budget, wake) {
            Ok(owners) => owners,
            Err(error) => {
                return Err(WorkspaceDocumentStartFailure::NotStarted {
                    root,
                    error: WorkspaceDocumentStartError::Spawn(error),
                });
            }
        };
        worker.set_save_destinations(destinations);
        self.files.worker = Some(worker);
        self.files.document_started = true;
        self.files.live_document_metadata = Some(charge.live);
        self.files.bounded_document = true;
        self.files.document_budget = Some(budget.clone());
        let mut transport = WorkspaceDocumentTransport {
            receiver,
            charge: AdapterCharge(Some(charge.adapter)),
            budget: budget.clone(),
        };
        match submit(self.files.worker.as_ref().unwrap(), &mut transport.receiver, DocumentRequest::Prepare { root }) {
            Ok(()) => {
                self.files.preparing = true;
                Ok(transport)
            }
            Err(error) => {
                Err(WorkspaceDocumentStartFailure::PrepareRefused(WorkspaceDocumentPrepareRefusal { transport, error }))
            }
        }
    }

    /// UI-safe extraction only: no wait, drain, callback or source operation.
    /// Missing/foreign worker refusal restores both EXACT original owners.
    #[cfg(test)]
    fn stop_bounded_document_worker(
        &mut self,
        transport: WorkspaceDocumentTransport,
    ) -> Result<WorkspaceDocumentSettlementTicket, WorkspaceDocumentStopRefusal> {
        let Some(worker) = self.files.worker.take() else {
            return Err(WorkspaceDocumentStopRefusal { transport, reason: WorkspaceDocumentStopReason::MissingWorker });
        };
        let WorkspaceDocumentTransport { receiver, charge, budget } = transport;
        match receiver.stop(worker) {
            Ok(ticket) => Ok(WorkspaceDocumentSettlementTicket {
                ticket,
                charge,
                live_charge: self.files.live_document_metadata.take(),
            }),
            Err(refusal) => {
                self.files.worker = Some(refusal.worker);
                Err(WorkspaceDocumentStopRefusal {
                    transport: WorkspaceDocumentTransport { receiver: refusal.receiver, charge, budget },
                    reason: WorkspaceDocumentStopReason::ForeignWorker,
                })
            }
        }
    }
}

fn destinations(workspace: &Workspace) -> impl Iterator<Item = (DocumentId, &PathBuf)> {
    workspace
        .session
        .tabs()
        .iter()
        .filter(|tab| {
            !workspace.files.failed_opens.contains(&tab.id)
                || workspace.views.get(&tab.id).is_some_and(|view| view.editor().is_some() || view.file().is_some())
        })
        .filter_map(|tab| tab.path.as_ref().map(|path| (tab.id, path)))
        .chain(workspace.files.saves.iter().filter_map(|(id, save)| save.path.as_ref().map(|path| (*id, path))))
}

type Destinations = Vec<(DocumentId, PathBuf)>;
struct StartAdmissionCharge {
    adapter: AllocationCharge,
    live: crate::visual_rows::MetadataReservation,
}

fn admit_start(
    workspace: &Workspace,
    root: Option<&PathBuf>,
    budget: &AllocationBudget,
) -> Result<(StartAdmissionCharge, Destinations), WorkspaceDocumentStartError> {
    let overflow =
        || WorkspaceDocumentStartError::Admission(WrapError::Admission("document adapter metadata overflow"));
    let mut count = 0usize;
    let mut paths = root.map_or(0, |path| path.capacity());
    for (_, path) in destinations(workspace) {
        count = count.checked_add(1).ok_or_else(overflow)?;
        paths = paths.checked_add(path.as_os_str().len()).ok_or_else(overflow)?;
    }
    // Reserve simultaneous adapter/return/failure/transfer headers, including
    // the worker's existing refused-request Box allocation before it can occur.
    // Source payload storage still belongs to the existing document budgets.
    let bytes = size_of::<WorkspaceDocumentTransport>()
        .checked_mul(2)
        .and_then(|n| n.checked_add(2 * size_of::<WorkspaceDocumentShutdown>()))
        .and_then(|n| n.checked_add(size_of::<DocumentShutdownBindRefusal>()))
        .and_then(|n| n.checked_add(size_of::<WorkspaceDocumentStopRefusal>()))
        .and_then(|n| n.checked_add(size_of::<WorkspaceDocumentStartFailure>()))
        .and_then(|n| n.checked_add(size_of::<WorkspaceDocumentPrepareRefusal>()))
        .and_then(|n| n.checked_add(size_of::<DocumentRequest>()))
        .and_then(|n| n.checked_add(2 * size_of::<Destinations>() + size_of::<PathBuf>()))
        .and_then(|n| count.checked_mul(size_of::<(DocumentId, PathBuf)>()).and_then(|d| n.checked_add(d)))
        .and_then(|n| n.checked_add(paths))
        .ok_or_else(overflow)?;
    let charge = budget.reserve(bytes).map_err(WorkspaceDocumentStartError::Admission)?;
    let live_bytes = workspace
        .document_live_metadata_bytes(None, None)
        .map_err(|_| WorkspaceDocumentStartError::Admission(WrapError::Admission("document live metadata overflow")))?;
    let live = budget.reserve_metadata(live_bytes).map_err(WorkspaceDocumentStartError::Admission)?;
    let mut owned = Vec::new();
    owned.try_reserve_exact(count).map_err(WorkspaceDocumentStartError::Allocation)?;
    if owned.capacity() > count {
        return Err(WorkspaceDocumentStartError::Admission(WrapError::Admission("document destination capacity")));
    }
    for (id, path) in destinations(workspace) {
        let length = path.as_os_str().len();
        let mut copy = PathBuf::new();
        copy.try_reserve_exact(length).map_err(WorkspaceDocumentStartError::Allocation)?;
        if copy.capacity() > length {
            return Err(WorkspaceDocumentStartError::Admission(WrapError::Admission("document path capacity")));
        }
        copy.push(path.as_os_str());
        owned.push((id, copy));
    }
    Ok((StartAdmissionCharge { adapter: charge, live }, owned))
}

#[cfg(test)]
#[path = "workspace_document_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "workspace_document_retry_ack_tests.rs"]
mod retry_ack_tests;
