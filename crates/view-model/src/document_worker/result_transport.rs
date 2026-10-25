//! One result slot on the EXISTING document worker (decision 0003).
//!
//! Open/empty -> publish -> owner-free wake -> admitted receive -> open/empty.
//! `publish` does not return until that exact delivery is acknowledged, so the
//! worker cannot construct a second result while the slot is occupied. Wake
//! failure, consumer disconnect, or explicit close fences ALL request senders.
//! Close never drains the slot. A stopped frontend retains its receiver and
//! worker together; OFF-UI settlement restores accepted loans and durable ACKs
//! through a fallible consumer before allowing the producer to proceed.
//!
//! No payload queue, retirement thread, timer, polling, unsafe code or detached
//! join is added. The fixed slot, sender gate, wake closure, lifecycle headers
//! and transfer overlap are admitted before launch against the SAME aggregate
//! passed to WrapHost. Document payloads retain their existing document budgets.
//! Dropping an unsettled bundle quarantines the exact slot/join on its existing
//! worker; it is misuse, NOT successful cleanup or permission to replace it.
use super::{DocumentRequest, DocumentResult, DocumentWorker};
use crate::visual_rows::{AllocationBudget, AllocationCharge, WrapError};
use crate::workspace::workspace_document::{
    DocumentCommit, DocumentCommitError, DocumentCommitOutcome, DocumentUpdate, ResultInfo,
};
use kwil_core_runtime::{NativeExitObservation, NativeExitState, NativeObservationError};
use std::{
    any::Any,
    fmt, io,
    mem::size_of,
    ops::ControlFlow,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Condvar, Mutex, MutexGuard, Weak,
        atomic::{AtomicU64, Ordering},
        mpsc::{SyncSender, TrySendError},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

static NEXT_WORKER_ID: AtomicU64 = AtomicU64::new(1);

/// Scalar identity, never a source handle or permission to submit work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DocumentDeliveryId {
    worker: u64,
    delivery: u64,
}

/// Opaque metadata sent through the existing request and result lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DocumentBarrierId {
    worker: u64,
    generation: u64,
    barrier: u64,
    source_epoch: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DocumentBarrierRequest {
    pub(super) id: DocumentBarrierId,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DocumentBarrierReceipt {
    id: DocumentBarrierId,
    complete: bool,
}
impl DocumentBarrierReceipt {
    pub fn id(&self) -> DocumentBarrierId {
        self.id
    }
    pub fn is_complete(&self) -> bool {
        self.complete
    }
    pub(super) fn reached(request: DocumentBarrierRequest) -> Self {
        Self { id: request.id, complete: true }
    }
    pub(super) fn failed(request: DocumentBarrierRequest) -> Self {
        Self { id: request.id, complete: false }
    }
}

/// The ONE logical result slot, including a monotonically installed residual.
pub(crate) struct DocumentSlot {
    commit: DocumentCommit,
    promoted: bool,
}
impl DocumentSlot {
    pub(crate) fn new(result: DocumentResult) -> Self {
        Self { commit: DocumentCommit::new(result), promoted: false }
    }
    pub(crate) fn whole(&self) -> Option<&DocumentResult> {
        if self.promoted { None } else { self.commit.pending() }
    }
    pub(crate) fn info(&self, delivery: DocumentDeliveryId) -> ResultInfo {
        match self.whole() {
            Some(result) => crate::workspace::workspace_document::commit::result_info(result),
            None => self.commit.info(),
        }
        .with_delivery(delivery)
    }
    pub(crate) fn partial(&self) -> Option<&DocumentCommit> {
        self.promoted.then_some(&self.commit)
    }
    pub(crate) fn partial_mut(&mut self) -> Option<&mut DocumentCommit> {
        self.promoted.then_some(&mut self.commit)
    }
    /// The original result already lives in this commit. Promotion changes no
    /// source owner, phase, destination or charge and cannot allocate or unwind.
    pub(crate) fn promote(&mut self) -> &mut DocumentCommit {
        self.promoted = true;
        &mut self.commit
    }
    fn into_whole(mut self) -> ControlFlow<Self, DocumentResult> {
        if self.promoted {
            return ControlFlow::Break(self);
        }
        match self.commit.take_received_result() {
            Some(result) => ControlFlow::Continue(result),
            None => ControlFlow::Break(self),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ShutdownPhase {
    Live,
    Drain,
    SourcesReleased,
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ContinuationRefusal {
    Full,
    Closed,
    WrongPermit,
    WrongPhase,
    Busy,
    Exhausted,
}
impl ContinuationRefusal {
    pub(crate) fn is_full(&self) -> bool {
        matches!(self, Self::Full | Self::Busy)
    }
}

/// Same-lane journal packet; its admitted path metadata follows readiness
/// deferral exactly as the original receipt does. Fields are not public API.
pub struct SealedJournalRequest {
    pub(super) document: kwil_core_workspace::session::DocumentId,
    pub(super) receipt: crate::EditReceipt,
    pub(super) path: Option<std::path::PathBuf>,
    pub(super) first_sequence: u64,
    pub(super) generation: u64,
    pub(super) policy: super::RecoveryPolicy,
    pub(super) charge: crate::visual_rows::MetadataReservation,
}
pub struct RestorePreparedRequest {
    pub(super) prepared: Box<super::PreparedWorkspace>,
}

pub(crate) struct DocumentJournalSink {
    sender: RequestSender,
    capacity: Arc<std::sync::atomic::AtomicUsize>,
    document: kwil_core_workspace::session::DocumentId,
    path: Option<std::path::PathBuf>,
    first_sequence: u64,
    generation: u64,
    policy: super::RecoveryPolicy,
}

impl DocumentWorker {
    pub(crate) fn matches_delivery(&self, id: DocumentDeliveryId) -> bool {
        matches!(&self.tx, RequestSender::Bounded(shared) if shared.worker_id == id.worker)
    }
    pub(crate) fn journal_sink(
        &self,
        document: kwil_core_workspace::session::DocumentId,
        path: Option<std::path::PathBuf>,
        first_sequence: u64,
        generation: u64,
        policy: super::RecoveryPolicy,
    ) -> DocumentJournalSink {
        DocumentJournalSink {
            sender: self.tx.clone(),
            capacity: self.journal_capacity.clone(),
            document,
            path,
            first_sequence,
            generation,
            policy,
        }
    }
}

impl DocumentWorker {
    pub(crate) fn restore_prepared_live(
        &self,
        pending: &mut Option<Box<super::PreparedWorkspace>>,
    ) -> Result<(), ContinuationRefusal> {
        if pending.is_none() {
            return Ok(());
        }
        let RequestSender::Bounded(shared) = &self.tx else { return Err(ContinuationRefusal::WrongPermit) };
        let mut gate = lock(&shared.requests);
        if !gate.accepting || gate.failed || gate.phase != ShutdownPhase::Live || gate.sender.is_none() {
            return Err(ContinuationRefusal::Closed);
        }
        let next = gate.source_epoch.checked_add(1).ok_or(ContinuationRefusal::Exhausted)?;
        let prepared = pending.take().expect("guarded live recovery owner");
        let result = gate
            .sender
            .as_ref()
            .expect("validated sender")
            .try_send(DocumentRequest::RestorePrepared(RestorePreparedRequest { prepared }));
        if result.is_ok() {
            gate.source_epoch = next;
        }
        drop(gate);
        match result {
            Ok(()) => Ok(()),
            Err(error) => {
                let (packet, error) = match error {
                    TrySendError::Full(DocumentRequest::RestorePrepared(packet)) => (packet, ContinuationRefusal::Full),
                    TrySendError::Disconnected(DocumentRequest::RestorePrepared(packet)) => {
                        (packet, ContinuationRefusal::Closed)
                    }
                    _ => unreachable!("same guarded recovery packet"),
                };
                *pending = Some(packet.prepared);
                Err(error)
            }
        }
    }
}

impl DocumentJournalSink {
    pub(crate) fn document_metadata_bytes(&self) -> io::Result<usize> {
        Ok(self.path.as_ref().map_or(0, |path| path.capacity()))
    }

    pub(crate) fn flush(
        &mut self,
        pending: &mut Option<crate::EditReceipt>,
        permit: Option<&DocumentContinuationPermit>,
    ) -> Result<(), crate::editor::ReceiptDeliveryRefusal> {
        use crate::editor::ReceiptDeliveryRefusal as Refusal;
        if pending.is_none() {
            return Ok(());
        }
        let RequestSender::Bounded(shared) = &self.sender else { return Err(Refusal::Closed) };
        {
            let gate = lock(&shared.requests);
            journal_authority(shared, &gate, permit)?;
        }
        let path_bytes = self.path.as_ref().map_or(0, |path| path.capacity());
        let recipe_bytes = pending
            .as_ref()
            .expect("the sealed sink borrows a pending receipt")
            .recipe_metadata_bytes()
            .map_err(|_| Refusal::Admission)?;
        let bytes = std::mem::size_of::<SealedJournalRequest>()
            .checked_add(path_bytes)
            .and_then(|bytes| bytes.checked_add(recipe_bytes))
            .ok_or(Refusal::Admission)?;
        let charge = shared.budget.reserve_metadata(bytes).map_err(|_| Refusal::Admission)?;
        let path = match &self.path {
            None => None,
            Some(path) => {
                let mut copy = std::path::PathBuf::new();
                copy.try_reserve_exact(path.as_os_str().len()).map_err(|_| Refusal::Admission)?;
                if copy.capacity() > path_bytes {
                    return Err(Refusal::Admission);
                }
                copy.push(path);
                Some(copy)
            }
        };
        let mut gate = lock(&shared.requests);
        journal_authority(shared, &gate, permit)?;
        if self.capacity.try_update(Ordering::AcqRel, Ordering::Acquire, |left| left.checked_sub(1)).is_err() {
            return Err(Refusal::Full);
        }
        let next_epoch = match gate.source_epoch.checked_add(1) {
            Some(epoch) => epoch,
            None => {
                self.capacity.fetch_add(1, Ordering::Release);
                return Err(Refusal::Admission);
            }
        };
        // Every fallible preparation is finished while the receipt is STILL
        // inside its guarded Editor owner. Only concrete channel moves follow.
        let receipt = pending.take().expect("the sealed sink borrows a pending receipt");
        let packet = SealedJournalRequest {
            document: self.document,
            receipt,
            path,
            first_sequence: self.first_sequence,
            generation: self.generation,
            policy: self.policy,
            charge,
        };
        let sent = gate
            .sender
            .as_ref()
            .expect("authority checked sole sender")
            .try_send(DocumentRequest::SealedJournal(packet));
        if sent.is_ok() {
            gate.source_epoch = next_epoch;
        }
        drop(gate);
        match sent {
            Ok(()) => Ok(()),
            Err(error) => {
                self.capacity.fetch_add(1, Ordering::Release);
                let (packet, refusal) = match error {
                    TrySendError::Full(DocumentRequest::SealedJournal(packet)) => (packet, Refusal::Full),
                    TrySendError::Disconnected(DocumentRequest::SealedJournal(packet)) => (packet, Refusal::Closed),
                    _ => unreachable!("concrete channel returns the same sealed packet"),
                };
                *pending = Some(packet.receipt);
                Err(refusal)
            }
        }
    }
    #[cfg(test)]
    pub(crate) fn closed_for_test() -> Self {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        drop(receiver);
        Self {
            sender: RequestSender::Legacy(sender),
            capacity: Arc::new(std::sync::atomic::AtomicUsize::new(16)),
            document: kwil_core_workspace::session::DocumentId(0),
            path: None,
            first_sequence: 1,
            generation: 0,
            policy: super::document_policy(&crate::Document::new()),
        }
    }
}

fn journal_authority(
    shared: &Arc<Shared>,
    gate: &RequestGate,
    permit: Option<&DocumentContinuationPermit>,
) -> Result<(), crate::editor::ReceiptDeliveryRefusal> {
    use crate::editor::ReceiptDeliveryRefusal as Refusal;
    if gate.sender.is_none() || gate.failed || gate.phase == ShutdownPhase::Closed {
        return Err(Refusal::Closed);
    }
    match permit {
        None if gate.accepting && gate.phase == ShutdownPhase::Live => Ok(()),
        // A sealed Partial temporarily fences ordinary Live admission. Keep
        // the exact receipt in Editor until producer retirement restores capacity.
        None if gate.phase == ShutdownPhase::Live => Err(Refusal::Full),
        Some(permit)
            if Weak::ptr_eq(&permit.shared, &Arc::downgrade(shared))
                && permit.generation == gate.generation
                && gate.phase == ShutdownPhase::Drain =>
        {
            Ok(())
        }
        Some(_) => Err(Refusal::WrongPermit),
        None => Err(Refusal::Closed),
    }
}

/// This authority is never exported to frontends or supplied by a user callback.
pub(crate) struct DocumentContinuationPermit {
    shared: Weak<Shared>,
    generation: u64,
    controls: Arc<super::JournalControlCredits>,
}

impl DocumentContinuationPermit {
    fn shared(&self) -> Result<Arc<Shared>, ContinuationRefusal> {
        self.shared.upgrade().ok_or(ContinuationRefusal::Closed)
    }
    fn valid(&self, shared: &Arc<Shared>, gate: &RequestGate) -> Result<(), ContinuationRefusal> {
        if !Weak::ptr_eq(&self.shared, &Arc::downgrade(shared)) || gate.generation != self.generation {
            return Err(ContinuationRefusal::WrongPermit);
        }
        if gate.sender.is_none() || gate.failed || gate.phase == ShutdownPhase::Closed {
            return Err(ContinuationRefusal::Closed);
        }
        Ok(())
    }
    pub(crate) fn submit(&self, request: DocumentRequest) -> ControlFlow<(DocumentRequest, ContinuationRefusal), ()> {
        let shared = match self.shared() {
            Ok(shared) => shared,
            Err(error) => return ControlFlow::Break((request, error)),
        };
        let mut gate = lock(&shared.requests);
        if let Err(error) = self.valid(&shared, &gate) {
            return ControlFlow::Break((request, error));
        }
        let disposal = matches!(
            &request,
            DocumentRequest::RetireSource { .. }
                | DocumentRequest::DiscardHex { .. }
                | DocumentRequest::ReleaseHex { .. }
                | DocumentRequest::FinishJournal { .. }
                | DocumentRequest::ReleaseReopen { .. }
        );
        let accepted = matches!(
            &request,
            DocumentRequest::ResumeSaveCompletion { .. }
                | DocumentRequest::ResolveBinary { .. }
                | DocumentRequest::RetireSource { .. }
                | DocumentRequest::OpenChecked { .. }
                | DocumentRequest::Open { .. }
                | DocumentRequest::OpenRaw { .. }
                | DocumentRequest::ReopenRaw { .. }
                | DocumentRequest::ReopenText { .. }
                | DocumentRequest::ValidateReopen { .. }
                | DocumentRequest::SessionTimer
                | DocumentRequest::ReleaseHex { .. }
                | DocumentRequest::DiscardHex { .. }
                | DocumentRequest::Recover { .. }
                | DocumentRequest::Index { .. }
                | DocumentRequest::Save { .. }
                | DocumentRequest::SaveRaw { .. }
                | DocumentRequest::Persist(_)
                | DocumentRequest::SeedJournal { .. }
                | DocumentRequest::FinishJournal { .. }
                | DocumentRequest::FlushJournals
                | DocumentRequest::ReleaseReopen { .. }
                | DocumentRequest::SettleWrap { .. }
        );
        if !accepted
            || (gate.phase != ShutdownPhase::Drain && !(gate.phase == ShutdownPhase::SourcesReleased && disposal))
        {
            return ControlFlow::Break((request, ContinuationRefusal::WrongPhase));
        }
        let next = match gate.source_epoch.checked_add(1) {
            Some(next) => next,
            None => return ControlFlow::Break((request, ContinuationRefusal::Exhausted)),
        };
        let credit = self.controls.for_request(&request);
        if credit.is_some_and(|credit| {
            credit.try_update(Ordering::AcqRel, Ordering::Acquire, |left| left.checked_sub(1)).is_err()
        }) {
            return ControlFlow::Break((request, ContinuationRefusal::Full));
        }
        let result = gate.sender.as_ref().expect("validated sender").try_send(request);
        if result.is_ok() && gate.phase == ShutdownPhase::Drain {
            gate.source_epoch = next;
        }
        drop(gate);
        match result {
            Ok(()) => ControlFlow::Continue(()),
            Err(error) => {
                let (request, reason) = match error {
                    TrySendError::Full(request) => (request, ContinuationRefusal::Full),
                    TrySendError::Disconnected(request) => (request, ContinuationRefusal::Closed),
                };
                if let Some(credit) = credit {
                    credit.fetch_add(1, Ordering::Release);
                }
                ControlFlow::Break((request, reason))
            }
        }
    }
    pub(crate) fn request_barrier(&self) -> Result<DocumentBarrierId, ContinuationRefusal> {
        let shared = self.shared()?;
        let mut gate = lock(&shared.requests);
        self.valid(&shared, &gate)?;
        if gate.phase != ShutdownPhase::Drain {
            return Err(ContinuationRefusal::WrongPhase);
        }
        if gate.outstanding_barrier.is_some() {
            return Err(ContinuationRefusal::Busy);
        }
        let barrier = gate.next_barrier.checked_add(1).ok_or(ContinuationRefusal::Exhausted)?;
        let id = DocumentBarrierId {
            worker: shared.worker_id,
            generation: self.generation,
            barrier,
            source_epoch: gate.source_epoch,
        };
        match gate
            .sender
            .as_ref()
            .expect("validated sender")
            .try_send(DocumentRequest::SourceBarrier(DocumentBarrierRequest { id }))
        {
            Ok(()) => {
                gate.next_barrier = barrier;
                gate.outstanding_barrier = Some(id);
                Ok(id)
            }
            Err(TrySendError::Full(_)) => Err(ContinuationRefusal::Full),
            Err(TrySendError::Disconnected(_)) => Err(ContinuationRefusal::Closed),
        }
    }
    pub(crate) fn seal_sources(
        &mut self,
        receipt: &DocumentBarrierReceipt,
    ) -> Result<(), DocumentResultSettlementReason> {
        let shared = self.shared().map_err(|_| DocumentResultSettlementReason::SourceBarrierFailed)?;
        let state = lock(&shared.state);
        if let Some(reason) = state.failure() {
            return Err(reason);
        }
        let mut gate = lock(&shared.requests);
        self.valid(&shared, &gate).map_err(|_| DocumentResultSettlementReason::SourceBarrierFailed)?;
        if gate.phase != ShutdownPhase::Drain
            || !receipt.complete
            || receipt.id.worker != shared.worker_id
            || receipt.id.generation != self.generation
            || receipt.id.source_epoch != gate.source_epoch
            || receipt.id.barrier != gate.next_barrier
        {
            return Err(DocumentResultSettlementReason::SourceBarrierRequired);
        }
        gate.phase = ShutdownPhase::SourcesReleased;
        Ok(())
    }
    pub(crate) fn restore_prepared(
        &self,
        pending: &mut Option<Box<super::PreparedWorkspace>>,
    ) -> Result<(), ContinuationRefusal> {
        if pending.is_none() {
            return Ok(());
        }
        let shared = self.shared()?;
        let mut gate = lock(&shared.requests);
        self.valid(&shared, &gate)?;
        if gate.phase != ShutdownPhase::Drain {
            return Err(ContinuationRefusal::WrongPhase);
        }
        let next = gate.source_epoch.checked_add(1).ok_or(ContinuationRefusal::Exhausted)?;
        let prepared = pending.take().expect("borrowed prepared owner");
        let result = gate
            .sender
            .as_ref()
            .expect("validated sender")
            .try_send(DocumentRequest::RestorePrepared(RestorePreparedRequest { prepared }));
        if result.is_ok() {
            gate.source_epoch = next;
        }
        drop(gate);
        match result {
            Ok(()) => Ok(()),
            Err(error) => {
                let (packet, error) = match error {
                    TrySendError::Full(DocumentRequest::RestorePrepared(packet)) => (packet, ContinuationRefusal::Full),
                    TrySendError::Disconnected(DocumentRequest::RestorePrepared(packet)) => {
                        (packet, ContinuationRefusal::Closed)
                    }
                    _ => unreachable!("same opaque restore packet"),
                };
                *pending = Some(packet.prepared);
                Err(error)
            }
        }
    }
}

/// Original worker remains inside Workspace for accepted-operation pumps.
pub(crate) struct BoundDocumentTicket {
    receiver: DocumentResultReceiver,
    observed_capacity: AtomicU64,
}
pub(crate) struct BeginBoundRefusal {
    pub receiver: DocumentResultReceiver,
    pub reason: DocumentResultSettlementReason,
}
impl BoundDocumentTicket {
    pub(crate) fn retained_commit_error(&self) -> DocumentResultInspection<'_> {
        self.inspect()
    }
    pub(crate) fn request_prepublication_reset(
        &mut self,
        delivery: DocumentDeliveryId,
        timeout: Duration,
    ) -> Result<DocumentProducerRetryToken, DocumentResultSettlementReason> {
        self.receiver.request_prepublication_reset(delivery, timeout)
    }
    pub(crate) fn acknowledge_prepublication_reset(
        &mut self,
        workspace: &crate::Workspace,
        token: DocumentProducerRetryToken,
    ) -> Result<(), DocumentProducerRetryRefusal> {
        self.receiver.acknowledge_prepublication_reset(workspace, token)
    }
    #[cfg(test)]
    pub(crate) fn into_legacy_for_test(self, worker: DocumentWorker) -> DocumentResultSettlementTicket {
        self.receiver.stop(worker).expect("source-free test bundle keeps its original worker")
    }
    pub(crate) fn status(&self) -> DocumentResultTransportStatus {
        self.receiver.status()
    }
    pub(crate) fn info(&self) -> Option<ResultInfo> {
        self.receiver.info()
    }
    pub(crate) fn inspect(&self) -> DocumentResultInspection<'_> {
        DocumentResultInspection { state: lock(&self.receiver.shared.state) }
    }
    pub(crate) fn acknowledge_delivery_failure(&mut self) -> Option<DocumentDeliveryFailure> {
        let mut state = lock(&self.receiver.shared.state);
        let failure = state.delivery_failure.take();
        if state.commit_panic.is_none() && !state.counter_exhausted {
            lock(&self.receiver.shared.requests).failed = false;
        }
        drop(state);
        self.receiver.shared.changed.notify_all();
        failure
    }
    pub(crate) fn acknowledge_wrap_retry(
        &mut self,
        token: DocumentWrapRetryToken,
    ) -> Result<(), DocumentWrapRetryRefusal> {
        self.receiver.acknowledge_wrap_retry(token)
    }
    pub(crate) fn request_wrap_retry(
        &mut self,
        timeout: Duration,
    ) -> Result<DocumentWrapRetryToken, DocumentResultSettlementReason> {
        self.receiver.request_wrap_retry(timeout)
    }
    pub(crate) fn accept_bound(
        &mut self,
        workspace: &mut crate::Workspace,
        _budget: &AllocationBudget,
        permit: &DocumentContinuationPermit,
        deadline: Instant,
    ) -> Result<Option<DocumentUpdate>, DocumentResultSettlementReason> {
        if Instant::now() >= deadline {
            return Err(DocumentResultSettlementReason::TimedOut);
        }
        {
            let state = lock(&self.receiver.shared.state);
            if let Some(reason) = state.failure() {
                return Err(reason);
            }
            let gate = lock(&self.receiver.shared.requests);
            if let Err(_reason) = permit.valid(&self.receiver.shared, &gate) {
                #[cfg(test)]
                let diagnostic = (
                    gate.phase,
                    gate.generation == permit.generation,
                    Weak::ptr_eq(&permit.shared, &Arc::downgrade(&self.receiver.shared)),
                    gate.failed,
                    gate.sender.is_some(),
                );
                drop(gate);
                drop(state);
                #[cfg(test)]
                {
                    let (sticky, unit_wake_full) = {
                        let state = lock(&self.receiver.shared.state);
                        let unit_wake_full = match state.delivery_failure.as_ref() {
                            Some(DocumentDeliveryFailure::Wake(error)) => error
                                .get_ref()
                                .and_then(|error| error.downcast_ref::<TrySendError<()>>())
                                .map(|error| matches!(error, TrySendError::Full(()))),
                            _ => None,
                        };
                        (state.failure(), unit_wake_full)
                    };
                    eprintln!(
                        "document bound refusal: reason={_reason:?} phase={:?} generation_equal={} worker_equal={} gate_failed={} sender_present={} sticky={sticky:?} unit_wake_full={unit_wake_full:?}",
                        diagnostic.0, diagnostic.1, diagnostic.2, diagnostic.3, diagnostic.4,
                    );
                }
                return Err(DocumentResultSettlementReason::SourceBarrierFailed);
            }
        }
        let budget = self.receiver.shared.budget.clone();
        let outcome = self.receiver.accept_with(workspace, &budget, true, Some(permit));
        #[cfg(test)]
        if matches!(&outcome, Err(DocumentResultSettlementReason::SourceBarrierFailed)) {
            let terminal_failed = matches!(
                lock(&self.receiver.shared.state).whole(),
                Some(DocumentResult::SourceBarrier(receipt)) if !receipt.is_complete()
            );
            eprintln!("document accept source-barrier failure: terminal_failed={terminal_failed}");
        }
        outcome
    }
    pub(crate) fn wait_until(&self, deadline: Instant) -> Result<(), DocumentResultSettlementReason> {
        let shared = &self.receiver.shared;
        let mut state = lock(&shared.state);
        loop {
            if let Some(reason) = state.failure() {
                return Err(reason);
            }
            if state.commit_error.is_some()
                && !state.retire_requested
                && !state.transfer_active
                && !state.reset_control_present()
            {
                return Err(DocumentResultSettlementReason::ConsumerRefused);
            }
            let capacity = state.capacity_generation;
            let reset_pending = state.retry.as_ref().is_some_and(RetryControl::is_reset)
                || (state.retry_running && state.transfer_active);
            if state.worker_done
                || (state.slot.is_some() && !state.transfer_active && !state.retire_requested && !reset_pending)
                || capacity != self.observed_capacity.load(Ordering::Relaxed)
            {
                self.observed_capacity.store(capacity, Ordering::Relaxed);
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(DocumentResultSettlementReason::TimedOut);
            }
            state = shared.changed.wait_timeout(state, remaining).unwrap_or_else(std::sync::PoisonError::into_inner).0;
        }
    }
    pub(crate) fn close_after_source_release(
        &mut self,
        permit: &DocumentContinuationPermit,
    ) -> Result<(), DocumentResultSettlementReason> {
        {
            let state = lock(&self.receiver.shared.state);
            if let Some(reason) = state.failure() {
                return Err(reason);
            }
            let gate = lock(&self.receiver.shared.requests);
            if gate.phase == ShutdownPhase::Closed
                && gate.generation == permit.generation
                && Weak::ptr_eq(&permit.shared, &Arc::downgrade(&self.receiver.shared))
            {
                return Ok(());
            }
            permit
                .valid(&self.receiver.shared, &gate)
                .map_err(|_| DocumentResultSettlementReason::SourceBarrierFailed)?;
            if gate.phase != ShutdownPhase::SourcesReleased {
                return Err(DocumentResultSettlementReason::SourceBarrierRequired);
            }
        }
        self.receiver.close();
        Ok(())
    }
    pub(crate) fn finish_until(&mut self, deadline: Instant) -> Result<(), DocumentResultSettlementReason> {
        let shared = self.receiver.shared.clone();
        shared.retire_if_joined()?;
        let panicked = {
            let state = lock(&shared.state);
            state.worker_done && state.worker_panicked
        };
        if panicked {
            shared.settle_native_until(deadline)?;
            return Err(lock(&shared.state).failure().unwrap_or(DocumentResultSettlementReason::WorkerPanicked));
        }
        if let Some(reason) = lock(&shared.state).failure() {
            return Err(reason);
        }
        if lock(&shared.requests).phase != ShutdownPhase::Closed {
            return Err(DocumentResultSettlementReason::SourceBarrierRequired);
        }
        let _settling = SettlementActive(shared.clone());
        lock(&shared.state).settlement_active = true;
        shared.changed.notify_all();
        loop {
            let mut state = lock(&shared.state);
            if state.worker_done && state.worker_panicked {
                drop(state);
                shared.settle_native_until(deadline)?;
                return Err(lock(&shared.state).failure().unwrap_or(DocumentResultSettlementReason::WorkerPanicked));
            }
            if let Some(reason) = state.failure() {
                return Err(reason);
            }
            if Instant::now() >= deadline {
                return Err(DocumentResultSettlementReason::TimedOut);
            }
            if state.slot.is_some() && !state.retire_requested && !state.transfer_active {
                let scalar = matches!(
                    state.whole(),
                    Some(
                        DocumentResult::Stopped(Ok(()))
                            | DocumentResult::CapacityAvailable
                            | DocumentResult::JournalClosed { result: Ok(()), .. }
                    )
                );
                if !scalar {
                    return Err(DocumentResultSettlementReason::ConsumerRefused);
                }
                state.stopped_acknowledged |= matches!(state.whole(), Some(DocumentResult::Stopped(Ok(()))));
                let retired = state.slot.take();
                state.in_flight = false;
                drop(state);
                drop(retired);
                shared.changed.notify_all();
                continue;
            }
            if state.worker_done {
                let acknowledged = state.stopped_acknowledged;
                drop(state);
                shared.settle_native_until(deadline)?;
                if let Some(reason) = lock(&shared.state).failure() {
                    return Err(reason);
                }
                return if acknowledged { Ok(()) } else { Err(DocumentResultSettlementReason::MissingStoppedReceipt) };
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(DocumentResultSettlementReason::TimedOut);
            }
            drop(shared.changed.wait_timeout(state, remaining).unwrap_or_else(std::sync::PoisonError::into_inner));
        }
    }
}
impl fmt::Debug for BeginBoundRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BeginBoundRefusal").field("reason", &self.reason).finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub(super) enum RequestSender {
    Legacy(SyncSender<DocumentRequest>),
    Bounded(Arc<Shared>),
}
impl RequestSender {
    pub(super) fn new(sender: SyncSender<DocumentRequest>, shared: Option<&Arc<Shared>>) -> Self {
        if let Some(shared) = shared {
            let mut gate = lock(&shared.requests);
            gate.sender = Some(sender);
            gate.accepting = true;
            drop(gate);
            Self::Bounded(shared.clone())
        } else {
            Self::Legacy(sender)
        }
    }
    pub(super) fn lock_sender(&self) -> SenderGuard<'_> {
        match self {
            Self::Legacy(sender) => SenderGuard(SenderGuardInner::Legacy(sender)),
            Self::Bounded(shared) => {
                // Follow the transport's state -> requests lock order. Only a
                // successfully accepted residual awaiting producer retirement
                // is temporary Full. Live accept sets retire_requested only
                // after verifying Partial::Complete; refusal/unwind/reset leave
                // it false. This proof survives producer-owned disposal and
                // retirement of an earlier first error, while transfer_active
                // is true and the residual no longer occupies the shared slot.
                let state = lock(&shared.state);
                let gate = lock(&shared.requests);
                let retirement_pending = state.retire_requested
                    && state.in_flight
                    && state.failure().is_none()
                    && !state.closed
                    && !state.retry_running
                    && state.retry.is_none()
                    && state.retry_acknowledgement_required.is_none();
                SenderGuard(SenderGuardInner::Bounded { gate, retirement_pending })
            }
        }
    }
    pub(super) fn close_bounded(&self) {
        if let Self::Bounded(shared) = self {
            shared.close_requests();
        }
    }
}

pub(super) struct SenderGuard<'a>(SenderGuardInner<'a>);
enum SenderGuardInner<'a> {
    Legacy(&'a SyncSender<DocumentRequest>),
    // Stack-only classification; Shared::new accounts the actual guard size.
    Bounded { gate: MutexGuard<'a, RequestGate>, retirement_pending: bool },
}
impl SenderGuard<'_> {
    pub(super) fn try_send(&self, request: DocumentRequest) -> ControlFlow<TrySendError<DocumentRequest>, ()> {
        // Keep the existing unboxed carrier/return overlap: this is a checked
        // representation change, without a new owner cell or reservation.
        const {
            assert!(
                size_of::<ControlFlow<TrySendError<DocumentRequest>, ()>>()
                    == size_of::<Result<(), TrySendError<DocumentRequest>>>()
            );
        }
        match &self.0 {
            SenderGuardInner::Legacy(sender) => match sender.try_send(request) {
                Ok(()) => ControlFlow::Continue(()),
                Err(error) => ControlFlow::Break(error),
            },
            SenderGuardInner::Bounded { gate, retirement_pending } => {
                if let Some(sender) = gate.sender.as_ref().filter(|_| gate.accepting) {
                    match sender.try_send(request) {
                        Ok(()) => ControlFlow::Continue(()),
                        Err(error) => ControlFlow::Break(error),
                    }
                } else if *retirement_pending
                    && gate.phase == ShutdownPhase::Live
                    && gate.sender.is_some()
                    && !gate.failed
                {
                    ControlFlow::Break(TrySendError::Full(request))
                } else {
                    ControlFlow::Break(TrySendError::Disconnected(request))
                }
            }
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Original wake errors AND unwind payloads stay owned here. Explicit OFF-UI
/// acknowledgement transfers them; merely retrying or UI Drop never does.
pub enum DocumentDeliveryFailure {
    Wake(io::Error),
    WakePanicked(Box<dyn Any + Send>),
    ReceiverDisconnected,
}
impl fmt::Debug for DocumentDeliveryFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Wake(error) => f.debug_tuple("Wake").field(error).finish(),
            Self::WakePanicked(_) => f.debug_struct("WakePanicked").finish_non_exhaustive(),
            Self::ReceiverDisconnected => f.write_str("ReceiverDisconnected"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocumentResultSettlementReason {
    WakeFailed,
    WakePanicked,
    ReceiverDisconnected,
    ConsumerRefused,
    WrapSettlementFailed,
    /// Wrapping's original native thread is still exiting. This is distinct
    /// from the document worker's own final-exit observation below.
    WrapThreadExitPending,
    WrapNativeExit(NativeObservationError),
    StoppedFailed,
    WorkerPanicked,
    TimedOut,
    /// The original registered native thread has not exited, including TLS.
    /// No join/poll loop starts. Retain and explicitly retry this same ticket.
    ThreadExitPending,
    NativeExit(NativeObservationError),
    MissingStoppedReceipt,
    Busy,
    RetryUnavailable,
    CommitPanicked,
    CounterExhausted,
    WrongWorker,
    SourceBarrierRequired,
    SourceBarrierFailed,
}

fn wrap_failure_reason(error: &WrapError) -> DocumentResultSettlementReason {
    match error {
        WrapError::ThreadExitPending => DocumentResultSettlementReason::WrapThreadExitPending,
        WrapError::NativeExit(error) => DocumentResultSettlementReason::WrapNativeExit(*error),
        _ => DocumentResultSettlementReason::WrapSettlementFailed,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocumentResultReceiveError {
    AdmissionRequired,
    Closed,
    Failed(DocumentResultSettlementReason),
    Disconnected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DocumentResultTransportStatus {
    pub occupied: bool,
    pub closed: bool,
    pub worker_done: bool,
    pub failure: Option<DocumentResultSettlementReason>,
    pub retry_pending: bool,
    pub retry_acknowledgement_required: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PrepublicationResetKind {
    SavePreparation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DocumentProducerAction {
    RetryWrap,
    ResetPrepublication(PrepublicationResetKind),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RetryControl {
    deadline: Instant,
    delivery: DocumentDeliveryId,
    generation: u64,
    action: DocumentProducerAction,
    failure: Option<DocumentResultSettlementReason>,
}
impl RetryControl {
    fn same_identity(&self, other: &Self) -> bool {
        self.delivery == other.delivery
            && self.generation == other.generation
            && self.action == other.action
            && self.deadline == other.deadline
    }
    fn is_reset(&self) -> bool {
        matches!(self.action, DocumentProducerAction::ResetPrepublication(_))
    }
}

#[must_use = "retain the exact admitted producer action until explicit acknowledgement"]
pub struct DocumentProducerRetryToken {
    // Weak MUST retire before its reservation: it can keep the entire Arc
    // allocation alive after its last strong source owner has settled.
    shared: Weak<Shared>,
    control: RetryControl,
    _charge: crate::visual_rows::MetadataReservation,
}
impl fmt::Debug for DocumentProducerRetryToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DocumentProducerRetryToken").finish_non_exhaustive()
    }
}
#[must_use = "a refused acknowledgement returns the original charged token"]
pub struct DocumentProducerRetryRefusal {
    pub token: DocumentProducerRetryToken,
    pub reason: DocumentResultSettlementReason,
}
impl fmt::Debug for DocumentProducerRetryRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DocumentProducerRetryRefusal").field("reason", &self.reason).finish_non_exhaustive()
    }
}

/// Owner-free authority for ONE explicit retry on ONE existing worker. The
/// weak identity prevents address reuse; private fields prevent forged ACKs.
#[must_use = "retain until the matching receipt is applied or retry fails"]
pub struct DocumentWrapRetryToken(DocumentProducerRetryToken);
#[must_use = "a refused ACK returns its exact scalar retry authority"]
pub struct DocumentWrapRetryRefusal {
    pub token: DocumentWrapRetryToken,
    pub reason: DocumentResultSettlementReason,
}
impl fmt::Debug for DocumentWrapRetryRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DocumentWrapRetryRefusal").field("reason", &self.reason).finish_non_exhaustive()
    }
}

struct RequestGate {
    sender: Option<SyncSender<DocumentRequest>>,
    accepting: bool,
    phase: ShutdownPhase,
    generation: u64,
    source_epoch: u64,
    next_barrier: u64,
    outstanding_barrier: Option<DocumentBarrierId>,
    failed: bool,
}

struct State {
    slot: Option<DocumentSlot>,
    // A capacity event survives an empty slot and a transition before wait.
    capacity_generation: u64,
    counter_exhausted: bool,
    #[cfg(test)]
    counter_limit: u64,
    // Stays set during OFF-UI take/accept/restore. An empty transfer window is
    // not capacity, and cannot release this producer into a second operation.
    in_flight: bool,
    closed: bool,
    delivery_failure: Option<DocumentDeliveryFailure>,
    worker_done: bool,
    worker_panicked: bool,
    worker_panic: Option<Box<dyn Any + Send>>,
    worker_joined: bool,
    native_retired: bool,
    native_retirement_error: Option<NativeObservationError>,
    stopped_acknowledged: bool,
    settlement_active: bool,
    receiver_present: bool,
    retry: Option<RetryControl>,
    retry_running: bool,
    retry_acknowledgement_required: Option<RetryControl>,
    retry_generation: u64,
    delivery: u64,
    transfer_active: bool,
    retire_requested: bool,
    commit_panic: Option<Box<dyn Any + Send>>,
    commit_error: Option<DocumentCommitError>,
    ready_wake_requested: bool,
    barrier_delivery: Option<DocumentBarrierId>,
    stopped_delivery: bool,
}
impl State {
    fn wrap_failure(&self) -> Option<DocumentResultSettlementReason> {
        match self.whole() {
            Some(DocumentResult::WrapSettled(Err(failure))) => Some(wrap_failure_reason(&failure.error)),
            _ => None,
        }
    }
    fn reset_control_present(&self) -> bool {
        self.retry.as_ref().is_some_and(RetryControl::is_reset)
            || self.retry_acknowledgement_required.as_ref().is_some_and(RetryControl::is_reset)
    }
    fn next_counter(&self, current: u64) -> Option<u64> {
        let next = current.checked_add(1)?;
        #[cfg(test)]
        if next > self.counter_limit {
            return None;
        }
        Some(next)
    }
    fn whole(&self) -> Option<&DocumentResult> {
        self.slot.as_ref().and_then(DocumentSlot::whole)
    }
    fn retain_commit_error(&mut self, error: DocumentCommitError) -> Option<DocumentCommitError> {
        if self.commit_error.is_none() {
            self.commit_error = Some(error);
            None
        } else {
            Some(error)
        }
    }
    fn failure(&self) -> Option<DocumentResultSettlementReason> {
        if let Some(error) = self.native_retirement_error {
            Some(DocumentResultSettlementReason::NativeExit(error))
        } else if self.commit_panic.is_some() {
            Some(DocumentResultSettlementReason::CommitPanicked)
        } else if self.worker_panicked {
            Some(DocumentResultSettlementReason::WorkerPanicked)
        } else if self.counter_exhausted {
            Some(DocumentResultSettlementReason::CounterExhausted)
        } else {
            match self.delivery_failure {
                Some(DocumentDeliveryFailure::Wake(_)) => Some(DocumentResultSettlementReason::WakeFailed),
                Some(DocumentDeliveryFailure::WakePanicked(_)) => Some(DocumentResultSettlementReason::WakePanicked),
                Some(DocumentDeliveryFailure::ReceiverDisconnected) => {
                    Some(DocumentResultSettlementReason::ReceiverDisconnected)
                }
                None => self.retry_acknowledgement_required.and_then(|control| control.failure).or_else(|| match self
                    .whole()
                {
                    Some(DocumentResult::SourceBarrier(receipt)) if !receipt.is_complete() => {
                        Some(DocumentResultSettlementReason::SourceBarrierFailed)
                    }
                    Some(DocumentResult::WrapSettled(Err(failure))) => Some(wrap_failure_reason(&failure.error)),
                    Some(DocumentResult::Stopped(Err(_))) => Some(DocumentResultSettlementReason::StoppedFailed),
                    _ => None,
                }),
            }
        }
    }
}

pub(super) struct Shared {
    native_exit: NativeExitObservation,
    state: Mutex<State>,
    changed: Condvar,
    // There is ONE actual sender, including all edit observers. Closing this
    // gate linearizes against try_send and disconnects after accepted work.
    requests: Mutex<RequestGate>,
    worker: Mutex<Option<JoinHandle<()>>>,
    wake: Box<dyn Fn() -> io::Result<()> + Send + Sync>,
    #[cfg(test)]
    wake_failure_gate: Mutex<Option<tests::WakeFailureGate>>,
    #[cfg(test)]
    publication_gate: Mutex<Option<tests::PublicationGate>>,
    #[cfg(test)]
    retirement_disposal_gate: Mutex<Option<tests::RetirementDisposalGate>>,
    _charge: AllocationCharge,
    budget: AllocationBudget,
    worker_id: u64,
}
impl Shared {
    fn admit_retry_token(
        self: &Arc<Self>,
        control: RetryControl,
    ) -> Result<DocumentProducerRetryToken, DocumentResultSettlementReason> {
        let bytes = size_of::<Self>()
            .checked_add(2 * size_of::<usize>())
            .and_then(|n| n.checked_add(2 * size_of::<DocumentProducerRetryToken>()))
            .and_then(|n| n.checked_add(size_of::<DocumentProducerRetryRefusal>() + size_of::<RetryControl>()))
            .ok_or(DocumentResultSettlementReason::RetryUnavailable)?;
        let charge =
            self.budget.reserve_metadata(bytes).map_err(|_| DocumentResultSettlementReason::RetryUnavailable)?;
        Ok(DocumentProducerRetryToken { shared: Arc::downgrade(self), control, _charge: charge })
    }
    pub(super) fn new<W>(budget: &AllocationBudget, wake: W) -> io::Result<(Arc<Self>, DocumentResultReceiver)>
    where
        W: Fn() -> io::Result<()> + Send + Sync + 'static,
    {
        // Include inline retained and transient return/take headers, Arc
        // counters, the concrete boxed callback, and the slot-to-consumer move.
        let bytes = size_of::<Self>()
            .checked_add(2 * size_of::<usize>())
            .and_then(|n| n.checked_add(2 * size_of::<W>()))
            .and_then(|n| n.checked_add(size_of::<DocumentWorker>()))
            .and_then(|n| n.checked_add(size_of::<WorkerExit>() + size_of::<SenderGuard<'static>>()))
            .and_then(|n| n.checked_add(4 * size_of::<Arc<Self>>()))
            .and_then(|n| n.checked_add(2 * size_of::<DocumentResultReceiver>()))
            .and_then(|n| n.checked_add(2 * size_of::<DocumentResultSettlementTicket>()))
            .and_then(|n| n.checked_add(size_of::<DocumentResultSettlementFailure>()))
            .and_then(|n| n.checked_add(size_of::<DocumentResultStopRefusal>()))
            .and_then(|n| n.checked_add(size_of::<DocumentWrapRetryToken>() + size_of::<DocumentWrapRetryRefusal>()))
            .and_then(|n| n.checked_add(2 * size_of::<RetryControl>()))
            .and_then(|n| {
                n.checked_add(size_of::<DocumentProducerRetryToken>() + size_of::<DocumentProducerRetryRefusal>())
            })
            .and_then(|n| n.checked_add(size_of::<DocumentResult>()))
            // publish -> synchronous effect argument/return overlap, plus the
            // shared classifier's typed scalar returns, in this SAME admission.
            .and_then(|n| n.checked_add(size_of::<DocumentResult>()))
            .and_then(|n| n.checked_add(size_of::<Option<DocumentSlot>>()))
            .and_then(|n| n.checked_add(size_of::<Option<DocumentResultSettlementReason>>()))
            .and_then(|n| n.checked_add(size_of::<Option<DocumentResultSettlementReason>>()))
            .and_then(|n| n.checked_add(size_of::<DocumentSlot>() + size_of::<DocumentTransferGuard>()))
            .and_then(|n| n.checked_add(size_of::<ControlFlow<DocumentSlot, DocumentResult>>()))
            .and_then(|n| {
                n.checked_add(
                    2 * size_of::<DocumentContinuationPermit>()
                        + size_of::<BoundDocumentTicket>()
                        + size_of::<BeginBoundRefusal>(),
                )
            })
            .and_then(|n| n.checked_add(2 * size_of::<super::SourceBarrierTracker>()))
            .and_then(|n| n.checked_add((super::DEFERRED_STORAGE_CAPACITY + 17) * size_of::<DocumentRequest>()))
            // Self includes the actual inline observation and retained panic
            // headers; native operations and typed call/return overlap also
            // belong to this SAME preconstruction 32 MiB aggregate admission.
            .and_then(|n| n.checked_add(NativeExitObservation::operation_stack_bytes()?))
            .and_then(|n| n.checked_add(2 * size_of::<Result<(), DocumentResultSettlementReason>>()))
            .and_then(|n| n.checked_add(2 * size_of::<Result<(), DocumentResultSettlementFailure>>()))
            .and_then(|n| n.checked_add(size_of::<std::thread::Result<()>>()))
            .ok_or_else(|| io::Error::other("document result transport metadata overflow"))?;
        let charge = budget.reserve(bytes).map_err(io::Error::other)?;
        let worker_id = NEXT_WORKER_ID
            .try_update(Ordering::AcqRel, Ordering::Acquire, |id| id.checked_add(1))
            .map_err(|_| io::Error::other("document worker identity exhausted"))?;
        let shared = Arc::new(Self {
            native_exit: NativeExitObservation::new(),
            state: Mutex::new(State {
                slot: None,
                capacity_generation: 0,
                counter_exhausted: false,
                #[cfg(test)]
                counter_limit: u64::MAX,
                in_flight: false,
                closed: false,
                delivery_failure: None,
                worker_done: false,
                worker_panicked: false,
                worker_panic: None,
                worker_joined: false,
                native_retired: false,
                native_retirement_error: None,
                stopped_acknowledged: false,
                settlement_active: false,
                receiver_present: true,
                retry: None,
                retry_running: false,
                retry_acknowledgement_required: None,
                retry_generation: 0,
                delivery: 0,
                transfer_active: false,
                retire_requested: false,
                commit_panic: None,
                commit_error: None,
                ready_wake_requested: false,
                barrier_delivery: None,
                stopped_delivery: false,
            }),
            changed: Condvar::new(),
            requests: Mutex::new(RequestGate {
                sender: None,
                accepting: false,
                phase: ShutdownPhase::Live,
                generation: 0,
                source_epoch: 0,
                next_barrier: 0,
                outstanding_barrier: None,
                failed: false,
            }),
            worker: Mutex::new(None),
            wake: Box::new(wake),
            #[cfg(test)]
            wake_failure_gate: Mutex::new(None),
            #[cfg(test)]
            publication_gate: Mutex::new(None),
            #[cfg(test)]
            retirement_disposal_gate: Mutex::new(None),
            _charge: charge,
            budget: budget.clone(),
            worker_id,
        });
        Ok((shared.clone(), DocumentResultReceiver { shared, explicitly_closed: false, observed_capacity: 0 }))
    }

    pub(super) fn install_worker(&self, worker: JoinHandle<()>) {
        *lock(&self.worker) = Some(worker);
    }

    pub(super) fn register_worker(&self) {
        // Exactly once INSIDE the original bounded worker, before source work.
        // A failed registration retains its partial right and original scalar
        // evidence in Shared; it never authorizes a fallback join.
        let _ = self.native_exit.register_current();
    }

    fn retire_joined_with(
        &self,
        retire: impl FnOnce(&NativeExitObservation) -> Result<(), NativeObservationError>,
    ) -> Result<(), DocumentResultSettlementReason> {
        let result = retire(&self.native_exit);
        let mut state = lock(&self.state);
        match result {
            Ok(()) => {
                state.native_retired = true;
                state.native_retirement_error = None;
                Ok(())
            }
            Err(error) => {
                let first = *state.native_retirement_error.get_or_insert(error);
                Err(DocumentResultSettlementReason::NativeExit(first))
            }
        }
    }

    // Synchronous effect seam, no production slot/channel/thread. The only
    // actual join follows ONE native query and the ORIGINAL deadline check.
    fn settle_native_until_with(
        &self,
        deadline: Instant,
        query: impl FnOnce(&NativeExitObservation, &JoinHandle<()>) -> Result<NativeExitState, NativeObservationError>,
        retire: impl FnOnce(&NativeExitObservation) -> Result<(), NativeObservationError>,
    ) -> Result<(), DocumentResultSettlementReason> {
        let mut worker = lock(&self.worker);
        if let Some(handle) = worker.as_ref() {
            if Instant::now() >= deadline {
                return Err(DocumentResultSettlementReason::TimedOut);
            }
            match query(&self.native_exit, handle) {
                Ok(NativeExitState::Pending) => return Err(DocumentResultSettlementReason::ThreadExitPending),
                Err(error) => return Err(DocumentResultSettlementReason::NativeExit(error)),
                Ok(NativeExitState::Exited) => {}
            }
            if Instant::now() >= deadline {
                return Err(DocumentResultSettlementReason::TimedOut);
            }
            let outcome = worker.take().expect("native-exited original worker").join();
            drop(worker);
            let mut state = lock(&self.state);
            state.worker_joined = true;
            if let Err(payload) = outcome {
                state.worker_panicked = true;
                state.worker_panic = Some(payload);
            }
            drop(state);
        } else {
            drop(worker);
            // None is only possible after this owner's join. Test-only exit
            // barriers also record that same fact when taking the handle.
            if !lock(&self.state).worker_joined {
                return Err(DocumentResultSettlementReason::ThreadExitPending);
            }
        }
        // Retirement precedes every sticky panic return and runs on retries
        // even when the original handle has already been consumed by join.
        self.retire_joined_with(retire)
    }

    fn settle_native_until(&self, deadline: Instant) -> Result<(), DocumentResultSettlementReason> {
        self.settle_native_until_with(deadline, NativeExitObservation::query, NativeExitObservation::retire_after_join)
    }

    fn retire_if_joined(&self) -> Result<(), DocumentResultSettlementReason> {
        if lock(&self.state).worker_joined {
            self.retire_joined_with(NativeExitObservation::retire_after_join)?;
        }
        Ok(())
    }

    pub(super) fn request_capacity_available(&self) {
        let mut state = lock(&self.state);
        if let Some(next) = state.next_counter(state.capacity_generation) {
            state.capacity_generation = next;
            drop(state);
            self.changed.notify_all();
            return;
        }
        self.exhaust_counter(&mut state);
        let wake = !state.closed;
        drop(state);
        self.changed.notify_all();
        if wake {
            self.wake();
        }
        // The accepted request stays in this original worker's active frame;
        // neither it nor the inbox/deferred owners are discarded on failure.
        let mut state = lock(&self.state);
        while state.counter_exhausted {
            state = self.changed.wait(state).unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn exhaust_counter(&self, state: &mut State) {
        state.counter_exhausted = true;
        let mut gate = lock(&self.requests);
        gate.failed = true;
        gate.accepting = false;
    }

    fn close_requests(&self) {
        // Drop the sole sender outside the gate. Accepted queue entries stay on
        // their existing worker; observer clones cannot keep admission alive.
        let sender = {
            let state = lock(&self.state);
            let mut gate = lock(&self.requests);
            gate.accepting = false;
            if state.counter_exhausted {
                // Fail-stop retains the actual sender with the exact worker;
                // a consumer close/Drop cannot turn exhaustion into cleanup.
                return;
            }
            gate.phase = ShutdownPhase::Closed;
            gate.sender.take()
        };
        drop(sender);
    }

    fn fence_requests(&self) {
        // Failure refuses new work, but does NOT disconnect the inbox and
        // trigger document cleanup before the private wrap receipt is applied.
        lock(&self.requests).accepting = false;
    }

    fn wake(&self) {
        // Only the owner-free callback is caught. Its result-slot owner never
        // leaves Shared, and this SAME worker stays alive on unwind. Abort-mode
        // panics are not recoverable; no panic hook or release policy changes.
        let failure = match catch_unwind(AssertUnwindSafe(|| (self.wake)())) {
            Ok(Ok(())) => return,
            Ok(Err(error)) => DocumentDeliveryFailure::Wake(error),
            Err(payload) => DocumentDeliveryFailure::WakePanicked(payload),
        };
        {
            let mut state = lock(&self.state);
            // ACK uses the SAME state -> requests order. Keep the logical
            // fence and sticky failure indivisible: otherwise ACK could
            // reopen admission between these two publications. Retain the
            // sole sender; only explicit close may disconnect the inbox.
            let mut gate = lock(&self.requests);
            gate.accepting = false;
            gate.failed = true;
            #[cfg(test)]
            tests::hold_wake_failure(self);
            if state.delivery_failure.is_none() {
                state.delivery_failure = Some(failure);
            }
            state.closed = true;
        }
        self.changed.notify_all();
    }

    pub(super) fn publish(self: &Arc<Self>, result: DocumentResult) {
        self.publish_with(result, retry_wrap_result);
    }

    // Private synchronous effect seam. Shipping uses the actual retry function
    // item above; no additional production state/allocation/executor exists.
    fn publish_with(
        self: &Arc<Self>,
        result: DocumentResult,
        mut retry_effect: impl FnMut(Option<DocumentSlot>, Duration) -> Option<DocumentSlot>,
    ) {
        let terminal = matches!(&result, DocumentResult::WrapSettled(Err(_)) | DocumentResult::Stopped(Err(_)))
            || matches!(&result,DocumentResult::SourceBarrier(receipt) if !receipt.is_complete());
        if terminal {
            self.fence_requests();
        }
        {
            let mut state = lock(&self.state);
            // This is a private serial callback on ONE worker. It only returns
            // after in_flight clears; there is no concurrent publisher API.
            debug_assert!(!state.in_flight && state.slot.is_none());
            state.barrier_delivery = match &result {
                DocumentResult::SourceBarrier(receipt) => Some(receipt.id),
                _ => None,
            };
            state.stopped_delivery = matches!(&result, DocumentResult::Stopped(Ok(())));
            state.slot = Some(DocumentSlot::new(result));
            state.in_flight = true;
            if let Some(next) = state.next_counter(state.delivery) {
                state.delivery = next;
            } else {
                // Keep the incoming owner, but never expose the previous ID
                // as the identity of this unnumbered failed delivery.
                self.exhaust_counter(&mut state);
            }
        }
        self.changed.notify_all();
        let open = !lock(&self.state).closed;
        if open {
            self.wake();
        }
        #[cfg(test)]
        tests::hold_publication(self);
        let mut state = lock(&self.state);
        loop {
            if state.counter_exhausted {
                state = self.changed.wait(state).unwrap_or_else(std::sync::PoisonError::into_inner);
                continue;
            }
            if state.ready_wake_requested {
                state.ready_wake_requested = false;
                drop(state);
                self.wake();
                state = lock(&self.state);
                continue;
            }
            if state.retire_requested && !state.transfer_active {
                let Some(next_capacity) = state.next_counter(state.capacity_generation) else {
                    // Exhaustion cannot take/drop even a completed residual.
                    self.exhaust_counter(&mut state);
                    let wake = !state.closed;
                    drop(state);
                    self.changed.notify_all();
                    if wake {
                        self.wake();
                    }
                    state = lock(&self.state);
                    continue;
                };
                let residual = state.slot.take();
                state.transfer_active = true;
                drop(state);
                #[cfg(test)]
                tests::hold_retirement_disposal(self, residual.as_ref());
                // The source owner never passes to a UI destructor. This is
                // the original producer retiring its own completed residual.
                drop(residual);
                state = lock(&self.state);
                // Successful sealed completion and source retirement ACK the
                // FIRST refusal. Dispose its metadata only on this producer,
                // outside locks, before exposing reusable capacity.
                let commit_error = state.commit_error.take();
                drop(state);
                drop(commit_error);
                state = lock(&self.state);
                state.transfer_active = false;
                state.retire_requested = false;
                state.in_flight = false;
                state.capacity_generation = next_capacity;
                state.stopped_acknowledged |= state.stopped_delivery;
                state.stopped_delivery = false;
                if let Some(id) = state.barrier_delivery.take() {
                    let mut gate = lock(&self.requests);
                    if gate.outstanding_barrier == Some(id) {
                        gate.outstanding_barrier = None;
                    }
                }
                if state.commit_panic.is_none()
                    && state.delivery_failure.is_none()
                    && state.retry_acknowledgement_required.is_none()
                    && !state.closed
                {
                    let mut gate = lock(&self.requests);
                    if gate.phase == ShutdownPhase::Live && gate.sender.is_some() && !gate.failed {
                        gate.accepting = true;
                    }
                }
                let wake = !state.closed;
                drop(state);
                self.changed.notify_all();
                if wake {
                    self.wake();
                }
                state = lock(&self.state);
                continue;
            }
            if !state.closed
                && let Some(control) = state.retry.take()
            {
                if let DocumentProducerAction::ResetPrepublication(kind) = control.action {
                    state.retry_running = true;
                    state.transfer_active = true;
                    let mut transfer = DocumentTransferGuard {
                        shared: self.clone(),
                        value: state.slot.take(),
                        delivery: control.delivery,
                    };
                    drop(state);
                    // Guard remains OUTSIDE catch; only a borrow reaches the
                    // concrete reset. Its saved owner/header/shell stay put.
                    let outcome = catch_unwind(AssertUnwindSafe(|| {
                        if Instant::now() >= control.deadline {
                            return Err(DocumentResultSettlementReason::TimedOut);
                        }
                        match transfer.slot_mut().partial_mut() {
                            Some(commit) => commit
                                .reset_prepublication_on_producer(kind)
                                .map_err(|_| DocumentResultSettlementReason::RetryUnavailable),
                            _ => Err(DocumentResultSettlementReason::RetryUnavailable),
                        }
                    }));
                    match outcome {
                        Ok(result) => transfer.restore_producer_action(control, result.err(), None),
                        Err(payload) => transfer.restore_producer_action(
                            control,
                            Some(DocumentResultSettlementReason::CommitPanicked),
                            Some(payload),
                        ),
                    }
                    let wake = !lock(&self.state).closed;
                    if wake {
                        self.wake();
                    }
                    state = lock(&self.state);
                    continue;
                }
                // The request API only installs this scalar while the sole
                // cell owns a failed WrapSettled. Transfer stays private to
                // this SAME document worker, never to an additional executor.
                state.retry_running = true;
                let pending = state.slot.take();
                drop(state);
                let result = retry_effect(pending, control.deadline.saturating_duration_since(Instant::now()));
                state = lock(&self.state);
                state.retry_acknowledgement_required =
                    matches!(result.as_ref().and_then(DocumentSlot::whole), Some(DocumentResult::WrapSettled(Ok(_))))
                        .then_some(control);
                state.slot = result;
                state.retry_running = false;
                let wake = !state.closed;
                drop(state);
                self.changed.notify_all();
                if wake {
                    self.wake();
                }
                state = lock(&self.state);
                continue;
            }
            if !state.in_flight
                && state.retry_acknowledgement_required.is_none()
                && state.delivery_failure.is_none()
                && (!state.closed || state.settlement_active)
            {
                break;
            }
            state = self.changed.wait(state).unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}

fn retry_wrap_result(pending: Option<DocumentSlot>, timeout: Duration) -> Option<DocumentSlot> {
    if timeout.is_zero()
        || !matches!(pending.as_ref().and_then(DocumentSlot::whole), Some(DocumentResult::WrapSettled(Err(_))))
    {
        return pending;
    }
    match pending?.into_whole() {
        ControlFlow::Continue(DocumentResult::WrapSettled(Err(failure))) => {
            Some(DocumentSlot::new(DocumentResult::WrapSettled((*failure.ticket).settle(timeout))))
        }
        ControlFlow::Continue(result) => Some(DocumentSlot::new(result)),
        ControlFlow::Break(slot) => Some(slot),
    }
}

pub(super) struct WorkerExit(pub(super) Option<Arc<Shared>>);
impl Drop for WorkerExit {
    fn drop(&mut self) {
        if let Some(shared) = &self.0 {
            let wake = {
                let mut state = lock(&shared.state);
                let panicking = std::thread::panicking();
                state.worker_done = true;
                state.worker_panicked |= panicking;
                !state.closed && !panicking
            };
            shared.changed.notify_all();
            if wake {
                shared.wake();
            }
        }
    }
}

/// A move of the original slot, never a second source-bearing capacity. This
/// guard stays OUTSIDE the unwind boundary and gives sealed code only borrows.
pub(crate) struct DocumentTransferGuard {
    shared: Arc<Shared>,
    value: Option<DocumentSlot>,
    delivery: DocumentDeliveryId,
}
impl DocumentTransferGuard {
    fn restore_producer_action(
        mut self,
        mut control: RetryControl,
        failure: Option<DocumentResultSettlementReason>,
        panic: Option<Box<dyn Any + Send>>,
    ) {
        let value = self.value.take().expect("producer retains the same guarded partial");
        let mut state = lock(&self.shared.state);
        let mut gate = lock(&self.shared.requests);
        debug_assert!(state.slot.is_none() && state.transfer_active && state.in_flight);
        state.slot = Some(value);
        state.transfer_active = false;
        state.retire_requested = false;
        state.retry_running = false;
        control.failure = failure;
        state.retry_acknowledgement_required = Some(control);
        if let Some(panic) = panic {
            state.commit_panic = Some(panic);
            gate.failed = true;
        }
        gate.accepting = false;
        drop(gate);
        drop(state);
        self.shared.changed.notify_all();
    }
    pub(crate) fn slot_mut(&mut self) -> &mut DocumentSlot {
        self.value.as_mut().expect("a live transfer retains its original slot")
    }
    fn restore(mut self, complete: bool) {
        self.restore_inner(complete);
    }
    fn restore_inner(&mut self, complete: bool) {
        if let Some(value) = self.value.take() {
            let mut state = lock(&self.shared.state);
            let mut gate = lock(&self.shared.requests);
            debug_assert!(state.slot.is_none() && state.transfer_active && state.in_flight);
            state.slot = Some(value);
            state.transfer_active = false;
            state.retire_requested = complete;
            gate.accepting = false;
            drop(gate);
            drop(state);
            self.shared.changed.notify_all();
        }
    }
}
impl Drop for DocumentTransferGuard {
    fn drop(&mut self) {
        self.restore_inner(false);
    }
}

#[must_use = "retain the receiver with its worker until off-UI settlement succeeds"]
pub struct DocumentResultReceiver {
    shared: Arc<Shared>,
    explicitly_closed: bool,
    observed_capacity: u64,
}
impl DocumentResultReceiver {
    /// Borrow the FIRST retained refusal; it may precede the current stale tag.
    pub fn retained_commit_error(&self) -> DocumentResultInspection<'_> {
        DocumentResultInspection { state: lock(&self.shared.state) }
    }
    pub(crate) fn take_capacity_event(&mut self) -> bool {
        let state = lock(&self.shared.state);
        if state.failure().is_some()
            || state.slot.is_some()
            || state.in_flight
            || state.transfer_active
            || state.retire_requested
            || state.capacity_generation == self.observed_capacity
        {
            return false;
        }
        self.observed_capacity = state.capacity_generation;
        true
    }
    pub(crate) fn begin_bound(
        self,
        worker: &DocumentWorker,
    ) -> Result<(BoundDocumentTicket, DocumentContinuationPermit), BeginBoundRefusal> {
        if !self.matches_worker(worker) {
            return Err(BeginBoundRefusal { receiver: self, reason: DocumentResultSettlementReason::WrongWorker });
        }
        let controls = worker.journal_controls.clone();
        let (generation, observed_capacity) = {
            let mut state = lock(&self.shared.state);
            let mut gate = lock(&self.shared.requests);
            if self.explicitly_closed || gate.phase != ShutdownPhase::Live || gate.sender.is_none() {
                let reason = state.failure().unwrap_or(DocumentResultSettlementReason::Busy);
                drop(gate);
                drop(state);
                return Err(BeginBoundRefusal { receiver: self, reason });
            }
            let Some(generation) = gate.generation.checked_add(1) else {
                drop(gate);
                drop(state);
                return Err(BeginBoundRefusal { receiver: self, reason: DocumentResultSettlementReason::Busy });
            };
            gate.generation = generation;
            gate.phase = ShutdownPhase::Drain;
            gate.accepting = false;
            state.settlement_active = true;
            (generation, state.capacity_generation)
        };
        let permit = DocumentContinuationPermit { shared: Arc::downgrade(&self.shared), generation, controls };
        Ok((BoundDocumentTicket { receiver: self, observed_capacity: AtomicU64::new(observed_capacity) }, permit))
    }
    pub(crate) fn matches_worker(&self, worker: &DocumentWorker) -> bool {
        matches!(&worker.tx, RequestSender::Bounded(shared) if Arc::ptr_eq(shared, &self.shared))
    }
    pub fn info(&self) -> Option<ResultInfo> {
        let state = lock(&self.shared.state);
        if state.counter_exhausted {
            return None;
        }
        let id = DocumentDeliveryId { worker: self.shared.worker_id, delivery: state.delivery };
        state.slot.as_ref().map(|slot| slot.info(id))
    }
    /// Explicit owner-free preview release. The existing producer emits the
    /// wake; the harness never obtains a source-bearing completion.
    pub fn wake_ready(&self) {
        lock(&self.shared.state).ready_wake_requested = true;
        self.shared.changed.notify_all();
    }
    pub(crate) fn accept_workspace(
        &mut self,
        workspace: &mut crate::Workspace,
        budget: &AllocationBudget,
        admitted: bool,
    ) -> Result<Option<DocumentUpdate>, DocumentResultSettlementReason> {
        let _ = budget;
        let admitted_budget = self.shared.budget.clone();
        self.accept_with(workspace, &admitted_budget, admitted, None)
    }
    fn accept_with(
        &mut self,
        workspace: &mut crate::Workspace,
        budget: &AllocationBudget,
        admitted: bool,
        permit: Option<&DocumentContinuationPermit>,
    ) -> Result<Option<DocumentUpdate>, DocumentResultSettlementReason> {
        let mut transfer = {
            let mut state = lock(&self.shared.state);
            if let Some(reason) = state.failure() {
                return Err(reason);
            }
            if state.closed && permit.is_none() {
                return Err(DocumentResultSettlementReason::RetryUnavailable);
            }
            if !admitted {
                return Err(DocumentResultSettlementReason::ConsumerRefused);
            }
            if state.reset_control_present() {
                return Err(DocumentResultSettlementReason::RetryUnavailable);
            }
            if state.transfer_active || state.retire_requested {
                return Ok(None);
            }
            let Some(slot) = state.slot.take() else { return Ok(None) };
            state.transfer_active = true;
            DocumentTransferGuard {
                shared: self.shared.clone(),
                value: Some(slot),
                delivery: DocumentDeliveryId { worker: self.shared.worker_id, delivery: state.delivery },
            }
        };
        let delivery = transfer.delivery;
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            workspace.commit_document_delivery(delivery, transfer.slot_mut(), budget, permit)
        }));
        match outcome {
            Ok(Ok(DocumentCommitOutcome::Completed(update))) => {
                let complete =
                    transfer.value.as_ref().and_then(DocumentSlot::partial).is_some_and(DocumentCommit::is_complete);
                transfer.restore(complete);
                if complete { Ok(Some(update)) } else { Err(DocumentResultSettlementReason::ConsumerRefused) }
            }
            Ok(Err(error)) => {
                let secondary = {
                    let mut state = lock(&self.shared.state);
                    state.retain_commit_error(error)
                };
                // A retry's PartialCommit must not replace the original useful
                // refusal, and no destructor runs under the mailbox lock.
                drop(secondary);
                drop(transfer);
                Err(DocumentResultSettlementReason::ConsumerRefused)
            }
            Err(payload) => {
                {
                    let mut state = lock(&self.shared.state);
                    let mut gate = lock(&self.shared.requests);
                    state.commit_panic = Some(payload);
                    gate.accepting = false;
                    gate.failed = true;
                }
                drop(transfer);
                Err(DocumentResultSettlementReason::CommitPanicked)
            }
        }
    }
    pub fn status(&self) -> DocumentResultTransportStatus {
        let state = lock(&self.shared.state);
        DocumentResultTransportStatus {
            occupied: state.in_flight,
            closed: state.closed,
            worker_done: state.worker_done,
            failure: state.failure(),
            retry_pending: state.retry.is_some() || state.retry_running,
            retry_acknowledgement_required: state.retry_acknowledgement_required.is_some(),
        }
    }

    /// ONE finite receive per event. Compute `admitted` from the consumer's
    /// available ownership/exception capacity BEFORE calling; false never
    /// moves an owner. No source methods, callbacks, disk or wait run here.
    #[cfg(test)]
    pub fn try_recv(&mut self, admitted: bool) -> Result<Option<DocumentResult>, DocumentResultReceiveError> {
        let result = {
            let mut state = lock(&self.shared.state);
            if let Some(reason) = state.failure() {
                return Err(DocumentResultReceiveError::Failed(reason));
            }
            if state.closed {
                return Err(DocumentResultReceiveError::Closed);
            }
            if !admitted {
                return Err(DocumentResultReceiveError::AdmissionRequired);
            }
            let result = match state.slot.take() {
                Some(slot) => match slot.into_whole() {
                    ControlFlow::Continue(result) => Some(result),
                    ControlFlow::Break(partial) => {
                        state.slot = Some(partial);
                        return Err(DocumentResultReceiveError::Failed(
                            DocumentResultSettlementReason::ConsumerRefused,
                        ));
                    }
                },
                None => None,
            };
            if let Some(result) = &result {
                state.stopped_acknowledged |= matches!(result, DocumentResult::Stopped(Ok(())));
                state.in_flight = false;
            } else if state.worker_done {
                return Err(DocumentResultReceiveError::Disconnected);
            }
            result
        };
        if result.is_some() {
            self.shared.changed.notify_all();
        }
        Ok(result)
    }

    /// Stop UI drain and ALL request admission. Does not destroy pending
    /// results or wait for a producer blocked on this slot. Call only after
    /// accepted wrap settlement/loans/save results have been restored, or keep
    /// those exact owners in the existing OFF-UI shutdown bundle.
    pub fn close(&mut self) {
        self.explicitly_closed = true;
        self.shared.close_requests();
        lock(&self.shared.state).closed = true;
        self.shared.changed.notify_all();
    }

    /// UI-safe scalar control for explicit `view.retryWrap` during accepted
    /// exit. The existing worker retries its own failed slot OFF UI. Duplicate
    /// requests return Busy; no owner is taken here and no timer is armed.
    /// Ordinary requests stay fenced until the matching receipt is applied and
    /// the returned private token is acknowledged. Failure never auto-retries.
    pub fn request_wrap_retry(
        &mut self,
        timeout: Duration,
    ) -> Result<DocumentWrapRetryToken, DocumentResultSettlementReason> {
        let delivery = {
            let state = lock(&self.shared.state);
            DocumentDeliveryId { worker: self.shared.worker_id, delivery: state.delivery }
        };
        self.request_producer_action(delivery, DocumentProducerAction::RetryWrap, timeout).map(DocumentWrapRetryToken)
    }

    pub fn request_prepublication_reset(
        &mut self,
        delivery: DocumentDeliveryId,
        timeout: Duration,
    ) -> Result<DocumentProducerRetryToken, DocumentResultSettlementReason> {
        self.request_producer_action(
            delivery,
            DocumentProducerAction::ResetPrepublication(PrepublicationResetKind::SavePreparation),
            timeout,
        )
    }

    fn request_producer_action(
        &mut self,
        delivery: DocumentDeliveryId,
        action: DocumentProducerAction,
        timeout: Duration,
    ) -> Result<DocumentProducerRetryToken, DocumentResultSettlementReason> {
        let deadline = Instant::now().checked_add(timeout).ok_or(DocumentResultSettlementReason::TimedOut)?;
        if timeout.is_zero() {
            return Err(DocumentResultSettlementReason::TimedOut);
        }
        let mut state = lock(&self.shared.state);
        if let Some(reason) = state.failure()
            && (action != DocumentProducerAction::RetryWrap || Some(reason) != state.wrap_failure())
        {
            return Err(reason);
        }
        if state.closed
            || state.worker_done
            || !state.in_flight
            || delivery != (DocumentDeliveryId { worker: self.shared.worker_id, delivery: state.delivery })
        {
            return Err(DocumentResultSettlementReason::RetryUnavailable);
        }
        if state.retry.is_some()
            || state.retry_running
            || state.retry_acknowledgement_required.is_some()
            || state.transfer_active
            || state.retire_requested
        {
            return Err(DocumentResultSettlementReason::Busy);
        }
        let eligible = match action {
            DocumentProducerAction::RetryWrap => matches!(state.whole(), Some(DocumentResult::WrapSettled(Err(_)))),
            DocumentProducerAction::ResetPrepublication(kind) => state
                .slot
                .as_ref()
                .and_then(DocumentSlot::partial)
                .is_some_and(|commit| commit.prepublication_reset_kind() == Some(kind)),
        };
        if !eligible {
            return Err(DocumentResultSettlementReason::RetryUnavailable);
        }
        let Some(generation) = state.next_counter(state.retry_generation) else {
            self.shared.exhaust_counter(&mut state);
            drop(state);
            self.shared.changed.notify_all();
            return Err(DocumentResultSettlementReason::CounterExhausted);
        };
        let control = RetryControl { deadline, delivery, generation, action, failure: None };
        let token = self.shared.admit_retry_token(control)?;
        let mut gate = lock(&self.shared.requests);
        if gate.failed || gate.sender.is_none() || gate.phase > ShutdownPhase::Drain {
            return Err(DocumentResultSettlementReason::RetryUnavailable);
        }
        gate.accepting = false;
        state.retry_generation = generation;
        state.retry = Some(control);
        drop(gate);
        drop(state);
        self.shared.changed.notify_all();
        Ok(token)
    }

    pub fn acknowledge_prepublication_reset(
        &mut self,
        workspace: &crate::Workspace,
        token: DocumentProducerRetryToken,
    ) -> Result<(), DocumentProducerRetryRefusal> {
        acknowledge_prepublication_reset(&self.shared, workspace, token)
    }

    /// Call ONLY after Workspace accepted the matching successful retry
    /// receipt. Taking it alone cannot start the next document operation.
    pub fn acknowledge_wrap_retry(&mut self, token: DocumentWrapRetryToken) -> Result<(), DocumentWrapRetryRefusal> {
        acknowledge_wrap_retry(&self.shared, token)
    }

    pub fn stop(mut self, worker: DocumentWorker) -> Result<DocumentResultSettlementTicket, DocumentResultStopRefusal> {
        if !matches!(&worker.tx, RequestSender::Bounded(shared) if Arc::ptr_eq(shared, &self.shared)) {
            return Err(DocumentResultStopRefusal { receiver: self, worker });
        }
        self.close();
        Ok(DocumentResultSettlementTicket { receiver: self, _worker: worker })
    }
}
impl Drop for DocumentResultReceiver {
    fn drop(&mut self) {
        let has_worker = lock(&self.shared.worker).is_some();
        let retain = {
            let state = lock(&self.shared.state);
            (has_worker || state.worker_joined) && (!state.native_retired || state.worker_panic.is_some())
        };
        if retain {
            // Extend the existing failure quarantine to a consumed join handle:
            // keep the exact observation/right and panic owner, no native I/O.
            std::mem::forget(self.shared.clone());
        }
        lock(&self.shared.state).receiver_present = false;
        if !self.explicitly_closed {
            {
                let mut state = lock(&self.shared.state);
                state.closed = true;
                if state.delivery_failure.is_none() {
                    state.delivery_failure = Some(DocumentDeliveryFailure::ReceiverDisconnected);
                }
            }
            self.shared.close_requests();
            self.shared.changed.notify_all();
        }
        // The existing worker retains Shared (and its own actual JoinHandle)
        // until every published owner is acknowledged off UI. Never take the
        // slot, forget an owner, or detach the worker here.
    }
}

impl DocumentWorker {
    /// Recover the existing slot/join after an unexpected receiver Drop.
    /// A live receiver or a legacy callback worker returns this worker intact.
    /// The disconnect stays sticky until explicitly acknowledged OFF UI.
    pub fn stop_disconnected(self) -> Result<DocumentResultSettlementTicket, Self> {
        let shared = match &self.tx {
            RequestSender::Bounded(shared) => shared.clone(),
            RequestSender::Legacy(_) => return Err(self),
        };
        {
            let mut state = lock(&shared.state);
            if state.receiver_present {
                drop(state);
                return Err(self);
            }
            state.receiver_present = true;
            state.closed = true;
        }
        shared.close_requests();
        Ok(DocumentResultSettlementTicket {
            receiver: DocumentResultReceiver { shared, explicitly_closed: true, observed_capacity: 0 },
            _worker: self,
        })
    }
}

#[must_use = "stop refused a different worker; retain BOTH original owners"]
pub struct DocumentResultStopRefusal {
    pub receiver: DocumentResultReceiver,
    pub worker: DocumentWorker,
}
impl fmt::Debug for DocumentResultStopRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DocumentResultStopRefusal").finish_non_exhaustive()
    }
}

/// Inspect actual errors/owners by reference OFF UI. Never take a source via
/// this guard, or invoke settlement/consumer callbacks while holding it.
pub struct DocumentResultInspection<'a> {
    state: MutexGuard<'a, State>,
}
impl DocumentResultInspection<'_> {
    /// The original first refusal, retained until successful producer retirement.
    pub fn retained_commit_error(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.state.commit_error.as_ref().map(|error| error as &(dyn std::error::Error + 'static))
    }
    #[cfg(test)]
    pub(crate) fn commit(&self) -> Option<&DocumentCommit> {
        self.state.slot.as_ref().and_then(DocumentSlot::partial)
    }
    #[cfg(test)]
    pub fn pending(&self) -> Option<&DocumentResult> {
        self.state.whole()
    }
    pub fn delivery_failure(&self) -> Option<&DocumentDeliveryFailure> {
        self.state.delivery_failure.as_ref()
    }
    pub fn commit_panic(&self) -> Option<&(dyn Any + Send)> {
        self.state.commit_panic.as_deref()
    }
    #[cfg(test)]
    pub(crate) fn commit_error(&self) -> Option<&DocumentCommitError> {
        self.state.commit_error.as_ref()
    }
}

#[must_use = "retain this exact worker and single pending slot until actual settlement"]
pub struct DocumentResultSettlementTicket {
    receiver: DocumentResultReceiver,
    _worker: DocumentWorker,
}
#[must_use = "failed settlement retains the exact ticket; it is not successful cleanup"]
pub struct DocumentResultSettlementFailure {
    pub ticket: DocumentResultSettlementTicket,
    pub reason: DocumentResultSettlementReason,
}
impl fmt::Debug for DocumentResultSettlementFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DocumentResultSettlementFailure").field("reason", &self.reason).finish_non_exhaustive()
    }
}
impl DocumentResultSettlementTicket {
    pub fn status(&self) -> DocumentResultTransportStatus {
        self.receiver.status()
    }
    pub fn inspect(&self) -> DocumentResultInspection<'_> {
        DocumentResultInspection { state: lock(&self.receiver.shared.state) }
    }

    /// Explicitly acknowledge the ORIGINAL wake/disconnect failure. This is
    /// never called by retry/settle. Closed admission and pending ownership stay
    /// unchanged. Stopped/worker/wrap failures cannot be cleared by this API.
    pub fn acknowledge_delivery_failure(&mut self) -> Option<DocumentDeliveryFailure> {
        let failure = {
            let mut state = lock(&self.receiver.shared.state);
            let failure = state.delivery_failure.take();
            if state.commit_panic.is_none() && !state.counter_exhausted {
                lock(&self.receiver.shared.requests).failed = false;
            }
            failure
        };
        self.receiver.shared.changed.notify_all();
        failure
    }

    /// OFF UI counterpart when the existing shutdown bundle held the receipt.
    pub fn acknowledge_wrap_retry(&mut self, token: DocumentWrapRetryToken) -> Result<(), DocumentWrapRetryRefusal> {
        acknowledge_wrap_retry(&self.receiver.shared, token)
    }

    /// OFF UI ONLY. One explicit retry of the SAME failed wrap ticket, in the
    /// SAME slot. The producer remains blocked throughout take/settle/replace.
    pub fn retry_pending_wrap(&mut self, timeout: Duration) -> Result<bool, DocumentResultSettlementReason> {
        let started = Instant::now();
        if timeout.is_zero() {
            return Err(DocumentResultSettlementReason::TimedOut);
        }
        let pending = {
            let mut state = lock(&self.receiver.shared.state);
            if state.counter_exhausted {
                return Err(DocumentResultSettlementReason::CounterExhausted);
            }
            if !matches!(state.whole(), Some(DocumentResult::WrapSettled(Err(_)))) {
                return Ok(false);
            }
            if started.elapsed() >= timeout {
                return Err(DocumentResultSettlementReason::TimedOut);
            }
            state.retry = None;
            state.slot.take()
        };
        if matches!(pending.as_ref().and_then(DocumentSlot::whole), Some(DocumentResult::WrapSettled(Err(_)))) {
            let result = retry_wrap_result(pending, timeout.saturating_sub(started.elapsed()));
            let reason = {
                let mut state = lock(&self.receiver.shared.state);
                state.slot = result;
                state.failure()
            };
            if let Some(reason) = reason {
                return Err(reason);
            }
            Ok(true)
        } else {
            // The variant was checked under the same lock. Preserve the full
            // owner even if this internal matching contract is later changed.
            lock(&self.receiver.shared.state).slot = pending;
            Ok(false)
        }
    }

    /// OFF UI ONLY. `accept` must restore loans and durable acknowledgements
    /// to the existing shutdown Workspace; return Break(EXACT result) on
    /// refusal or Continue(()) after acceptance. No allocation wraps the owner.
    /// No callback runs under any transport lock. Failure returns this same
    /// ticket; it never disposes generic document results or retries wrap work.
    #[cfg(test)]
    pub fn settle(
        self,
        timeout: Duration,
        mut accept: impl FnMut(DocumentResult) -> ControlFlow<DocumentResult, ()>,
    ) -> Result<(), DocumentResultSettlementFailure> {
        let started = Instant::now();
        let shared = self.receiver.shared.clone();
        let Some(deadline) = started.checked_add(timeout) else {
            return Err(DocumentResultSettlementFailure {
                ticket: self,
                reason: DocumentResultSettlementReason::TimedOut,
            });
        };
        if let Err(reason) = shared.retire_if_joined() {
            return Err(DocumentResultSettlementFailure { ticket: self, reason });
        }
        // This guard stops production on EVERY unsuccessful return/unwind.
        // It never owns a payload, and callbacks never borrow its lock.
        let _settling = SettlementActive(shared.clone());
        lock(&shared.state).settlement_active = true;
        shared.changed.notify_all();
        loop {
            let mut state = lock(&shared.state);
            if state.worker_done && state.worker_panicked {
                drop(state);
                let reason = match shared.settle_native_until(deadline) {
                    Err(reason) => reason,
                    Ok(()) => lock(&shared.state).failure().unwrap_or(DocumentResultSettlementReason::WorkerPanicked),
                };
                return Err(DocumentResultSettlementFailure { ticket: self, reason });
            }
            if let Some(reason) = state.failure() {
                drop(state);
                return Err(DocumentResultSettlementFailure { ticket: self, reason });
            }
            if started.elapsed() >= timeout {
                drop(state);
                return Err(DocumentResultSettlementFailure {
                    ticket: self,
                    reason: DocumentResultSettlementReason::TimedOut,
                });
            }
            if state.retire_requested || state.transfer_active {
                let remaining = timeout.saturating_sub(started.elapsed());
                drop(shared.changed.wait_timeout(state, remaining).unwrap_or_else(std::sync::PoisonError::into_inner));
                continue;
            }
            if let Some(slot) = state.slot.take() {
                let result = match slot.into_whole() {
                    ControlFlow::Continue(result) => result,
                    ControlFlow::Break(partial) => {
                        state.slot = Some(partial);
                        drop(state);
                        return Err(DocumentResultSettlementFailure {
                            ticket: self,
                            reason: DocumentResultSettlementReason::ConsumerRefused,
                        });
                    }
                };
                let stopped = matches!(&result, DocumentResult::Stopped(Ok(())));
                let wrap = matches!(&result, DocumentResult::WrapSettled(Ok(_)));
                drop(state);
                match accept(result) {
                    ControlFlow::Continue(()) => {
                        let mut state = lock(&shared.state);
                        state.stopped_acknowledged |= stopped;
                        if wrap {
                            state.retry_acknowledgement_required = None;
                        }
                        state.in_flight = false;
                        drop(state);
                        shared.changed.notify_all();
                    }
                    ControlFlow::Break(result) => {
                        lock(&shared.state).slot = Some(DocumentSlot::new(result));
                        return Err(DocumentResultSettlementFailure {
                            ticket: self,
                            reason: DocumentResultSettlementReason::ConsumerRefused,
                        });
                    }
                }
                continue;
            }
            if state.worker_done {
                let acknowledged = state.stopped_acknowledged;
                drop(state);
                if let Err(reason) = shared.settle_native_until(deadline) {
                    return Err(DocumentResultSettlementFailure { ticket: self, reason });
                }
                if let Some(reason) = lock(&shared.state).failure() {
                    return Err(DocumentResultSettlementFailure { ticket: self, reason });
                }
                if !acknowledged {
                    return Err(DocumentResultSettlementFailure {
                        ticket: self,
                        reason: DocumentResultSettlementReason::MissingStoppedReceipt,
                    });
                }
                return Ok(());
            }
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                drop(state);
                return Err(DocumentResultSettlementFailure {
                    ticket: self,
                    reason: DocumentResultSettlementReason::TimedOut,
                });
            }
            drop(shared.changed.wait_timeout(state, remaining).unwrap_or_else(std::sync::PoisonError::into_inner));
        }
    }
}

fn acknowledge_wrap_retry(shared: &Arc<Shared>, token: DocumentWrapRetryToken) -> Result<(), DocumentWrapRetryRefusal> {
    match acknowledge_wrap_producer_action(shared, token.0) {
        Ok(()) => Ok(()),
        Err(refusal) => {
            Err(DocumentWrapRetryRefusal { token: DocumentWrapRetryToken(refusal.token), reason: refusal.reason })
        }
    }
}

fn validate_producer_ack(
    shared: &Arc<Shared>,
    state: &State,
    token: &DocumentProducerRetryToken,
) -> Result<RetryControl, DocumentResultSettlementReason> {
    if let Some(reason) = state.failure() {
        return Err(reason);
    }
    if !Weak::ptr_eq(&token.shared, &Arc::downgrade(shared))
        || token.control.delivery != (DocumentDeliveryId { worker: shared.worker_id, delivery: state.delivery })
    {
        return Err(DocumentResultSettlementReason::RetryUnavailable);
    }
    let control = state
        .retry_acknowledgement_required
        .filter(|control| control.same_identity(&token.control))
        .ok_or(DocumentResultSettlementReason::RetryUnavailable)?;
    if let Some(reason) = control.failure {
        return Err(reason);
    }
    Ok(control)
}

fn acknowledge_wrap_producer_action(
    shared: &Arc<Shared>,
    token: DocumentProducerRetryToken,
) -> Result<(), DocumentProducerRetryRefusal> {
    let mut state = lock(&shared.state);
    if let Err(reason) = validate_producer_ack(shared, &state, &token) {
        return Err(DocumentProducerRetryRefusal { token, reason });
    }
    if token.control.action != DocumentProducerAction::RetryWrap || state.slot.is_some() {
        return Err(DocumentProducerRetryRefusal { token, reason: DocumentResultSettlementReason::RetryUnavailable });
    }
    state.retry_acknowledgement_required = None;
    // Gate and slot are checked in one critical section. A racing close has
    // already removed the sender, or observes this ACK before its own close.
    // No closed sender is recreated, and no stale/foreign ACK opens admission.
    if !state.closed {
        let mut gate = lock(&shared.requests);
        if gate.sender.is_some() && gate.phase == ShutdownPhase::Live && !gate.failed {
            gate.accepting = true;
        }
    }
    drop(state);
    shared.changed.notify_all();
    Ok(())
}

fn acknowledge_prepublication_reset(
    shared: &Arc<Shared>,
    workspace: &crate::Workspace,
    token: DocumentProducerRetryToken,
) -> Result<(), DocumentProducerRetryRefusal> {
    let mut transfer = {
        let mut state = lock(&shared.state);
        if let Err(reason) = validate_producer_ack(shared, &state, &token) {
            return Err(DocumentProducerRetryRefusal { token, reason });
        }
        if !token.control.is_reset()
            || state.transfer_active
            || state.retire_requested
            || state.slot.as_ref().and_then(DocumentSlot::partial).is_none()
        {
            return Err(DocumentProducerRetryRefusal {
                token,
                reason: DocumentResultSettlementReason::RetryUnavailable,
            });
        }
        if Instant::now() >= token.control.deadline {
            return Err(DocumentProducerRetryRefusal { token, reason: DocumentResultSettlementReason::TimedOut });
        }
        let value = state.slot.take();
        state.transfer_active = true;
        DocumentTransferGuard { shared: shared.clone(), value, delivery: token.control.delivery }
    };
    // Sealed scalar validation borrows the original guarded Partial; no owned
    // callback, source read, preparation, or lock reaches this Workspace call.
    let outcome = catch_unwind(AssertUnwindSafe(|| match transfer.slot_mut().partial_mut() {
        Some(commit) => workspace.validate_prepublication_reset_ack(token.control.delivery, commit),
        _ => unreachable!("reset ACK retained its original partial"),
    }));
    match outcome {
        Ok(Ok(())) => transfer.restore(false),
        Ok(Err(_)) => {
            drop(transfer);
            return Err(DocumentProducerRetryRefusal {
                token,
                reason: DocumentResultSettlementReason::ConsumerRefused,
            });
        }
        Err(payload) => {
            {
                let mut state = lock(&shared.state);
                let mut gate = lock(&shared.requests);
                state.commit_panic = Some(payload);
                gate.failed = true;
                gate.accepting = false;
            }
            drop(transfer);
            return Err(DocumentProducerRetryRefusal { token, reason: DocumentResultSettlementReason::CommitPanicked });
        }
    }
    let mut state = lock(&shared.state);
    if let Err(reason) = validate_producer_ack(shared, &state, &token) {
        return Err(DocumentProducerRetryRefusal { token, reason });
    }
    if Instant::now() >= token.control.deadline {
        return Err(DocumentProducerRetryRefusal { token, reason: DocumentResultSettlementReason::TimedOut });
    }
    let mut gate = lock(&shared.requests);
    if gate.failed || gate.sender.is_none() || gate.phase > ShutdownPhase::Drain {
        return Err(DocumentProducerRetryRefusal { token, reason: DocumentResultSettlementReason::RetryUnavailable });
    }
    // ONLY this action fence clears. Partial/source ownership and FIRST error
    // remain; fresh preparation is a separate subsequent sealed acceptance.
    state.retry_acknowledgement_required = None;
    gate.accepting = false;
    drop(gate);
    drop(state);
    shared.changed.notify_all();
    Ok(())
}

struct SettlementActive(Arc<Shared>);
impl Drop for SettlementActive {
    fn drop(&mut self) {
        lock(&self.0.state).settlement_active = false;
    }
}

#[cfg(test)]
#[path = "result_transport_tests.rs"]
mod tests;
