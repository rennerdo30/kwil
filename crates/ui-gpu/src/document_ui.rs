//! The GUI owns one bounded document transport. Events carry no document owner.
use super::*;
use kwil_core_runtime::{NativeExitObservation, NativeExitState, NativeObservationError};
use kwil_view_model::{
    document_worker::{DocumentRequest, DocumentResultSettlementReason},
    visual_rows::{AllocationBudget, MetadataReservation, WrapResult},
    workspace_document::{WorkspaceDocumentStartFailure, WorkspaceDocumentTransport, WorkspaceDocumentUpdate},
};

/// Pre-worker refusal returns the original owners without an error-time box.
#[must_use = "retain or transfer the exact Workspace, budget and reason"]
pub struct GuiStartRefusal {
    pub workspace: Workspace,
    pub budget: AllocationBudget,
    pub reason: GuiStartReason,
}
pub enum GuiStartReason {
    UnstartedRequired,
    Admission(kwil_view_model::visual_rows::WrapError),
    Markdown(kwil_view_model::visual_rows::WrapError),
    EventLoop(winit::error::EventLoopError),
    Panicked(Box<dyn std::any::Any + Send>),
}
impl std::fmt::Debug for GuiStartReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnstartedRequired => f.write_str("UnstartedRequired"),
            Self::Admission(error) => f.debug_tuple("Admission").field(error).finish(),
            Self::Markdown(error) => f.debug_tuple("Markdown").field(error).finish(),
            Self::EventLoop(error) => f.debug_tuple("EventLoop").field(error).finish(),
            Self::Panicked(_) => f.write_str("Panicked(original payload retained)"),
        }
    }
}
impl std::fmt::Debug for GuiStartRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuiStartRefusal").field("reason", &self.reason).finish_non_exhaustive()
    }
}

/// Both possible original Workspace owners stay OUTSIDE catch_unwind. Setup
/// borrows them; no source worker exists before this function returns.
pub(super) fn prepare_startup<T>(
    workspace: Workspace,
    budget: AllocationBudget,
    initialize: impl FnOnce() -> Result<T, GuiStartReason>,
) -> std::ops::ControlFlow<GuiStartRefusal, (Box<shutdown::Failure>, T, AllocationBudget)> {
    let mut original = Some(workspace);
    let mut admitted: Option<Box<shutdown::Failure>> = None;
    let attempted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if !original.as_ref().unwrap().unstarted_frontend_ready() {
            return Err(GuiStartReason::UnstartedRequired);
        }
        admitted = Some(
            shutdown::Failure::admit_startup(
                &budget,
                2 * std::mem::size_of::<
                    std::ops::ControlFlow<GuiStartRefusal, (Box<shutdown::Failure>, T, AllocationBudget)>,
                >(),
            )
            .map_err(GuiStartReason::Admission)?,
        );
        let failure = admitted.as_mut().unwrap();
        failure.workspace = original.take();
        failure.markdown = Some(markdown_ui::Host::with_budget(&budget).map_err(GuiStartReason::Markdown)?);
        initialize()
    }));
    let reason = match attempted {
        Ok(Ok(prepared)) => return std::ops::ControlFlow::Continue((admitted.unwrap(), prepared, budget)),
        Ok(Err(reason)) => reason,
        Err(payload) => GuiStartReason::Panicked(payload),
    };
    let workspace = original.take().or_else(|| admitted.as_mut().and_then(|failure| failure.workspace.take())).unwrap();
    // Remaining admitted metadata/Markdown fields have never acquired a source.
    std::ops::ControlFlow::Break(GuiStartRefusal { workspace, budget, reason })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FontSettlementReason {
    TimedOut,
    ThreadExitPending,
    WorkerPanicked,
    Native(NativeObservationError),
    Product(FontProductFailure),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FontProductFailure {
    Native(NativeObservationError),
    WorkerPanicked,
    WakeFailed { kind: std::io::ErrorKind, code: Option<i32> },
    WakePanicked,
    AlreadyTaken,
    TimedOut,
}
#[derive(Debug)]
pub(super) enum FontProduct<T> {
    Pending,
    Ready(Option<T>),
    Failed(FontProductFailure),
}
#[derive(Debug)]
pub(super) enum FontSpawnFailure {
    Admission(kwil_view_model::visual_rows::WrapError),
    Spawn(std::io::Error),
    AlreadyStarted,
}
struct FontState<T> {
    done: bool,
    ready: bool,
    taken: bool,
    // The ONE original retained product slot, moved here from StartupFont.
    product: Option<T>,
    failure: Option<FontProductFailure>,
    wake_error: Option<std::io::Error>,
    wake_panic: Option<Box<dyn std::any::Any + Send>>,
}
impl<T> FontState<T> {
    fn failure(&self) -> Option<FontProductFailure> {
        self.failure
            .or_else(|| {
                self.wake_error
                    .as_ref()
                    .map(|error| FontProductFailure::WakeFailed { kind: error.kind(), code: error.raw_os_error() })
            })
            .or_else(|| self.wake_panic.as_ref().map(|_| FontProductFailure::WakePanicked))
    }
}
struct FontTerminal<T> {
    state: std::sync::Mutex<FontState<T>>,
    changed: std::sync::Condvar,
    native: NativeExitObservation,
}
pub(super) struct FontWakeInspection<'a, T> {
    state: std::sync::MutexGuard<'a, FontState<T>>,
}
impl<T> FontWakeInspection<'_, T> {
    pub(super) fn panic_payload(&self) -> Option<&(dyn std::any::Any + Send)> {
        self.state.wake_panic.as_deref()
    }
}
impl<T> Default for FontTerminal<T> {
    fn default() -> Self {
        Self {
            state: std::sync::Mutex::new(FontState {
                done: false,
                ready: false,
                taken: false,
                product: None,
                failure: None,
                wake_error: None,
                wake_panic: None,
            }),
            changed: Default::default(),
            native: NativeExitObservation::new(),
        }
    }
}
struct FontExit<'a, T>(&'a FontTerminal<T>);
impl<T> Drop for FontExit<'_, T> {
    fn drop(&mut self) {
        self.0.state.lock().unwrap_or_else(|error| error.into_inner()).done = true;
        self.0.changed.notify_all();
    }
}
/// The original startup thread, with admitted terminal metadata. No extra
/// worker/channel is introduced; a refused actual join keeps this owner.
pub(super) struct StartupFont<T> {
    thread: Option<std::thread::JoinHandle<()>>,
    terminal: std::sync::Arc<FontTerminal<T>>,
    panic: Option<Box<dyn std::any::Any + Send>>,
    started: bool,
    retirement_pending: bool,
    native_error: Option<NativeObservationError>,
    timed_out: bool,
    deadline: Option<std::time::Instant>,
    budget: AllocationBudget,
    _capture_charge: Option<MetadataReservation>,
    _metadata: MetadataReservation,
}
impl<T: Send + 'static> StartupFont<T> {
    pub fn admit(budget: &AllocationBudget) -> WrapResult<Self> {
        // Terminal includes the inline native observation and its counted right;
        // no additional Arc is allocated. Admit every widened return and the
        // helper's declared operation overlap BEFORE constructing either owner.
        let bytes = std::mem::size_of::<Self>()
            .checked_mul(2)
            .and_then(|n| n.checked_add(std::mem::size_of::<FontTerminal<T>>()))
            .and_then(|n| n.checked_add(std::mem::size_of::<usize>().checked_mul(2)?))
            .and_then(|n| n.checked_add(std::mem::size_of::<FontExit<'_, T>>().checked_mul(2)?))
            .and_then(|n| n.checked_add(std::mem::size_of::<Option<T>>()))
            .and_then(|n| n.checked_add(std::mem::size_of::<Result<Option<T>, FontSettlementReason>>().checked_mul(2)?))
            .and_then(|n| n.checked_add(std::mem::size_of::<FontProduct<T>>().checked_mul(2)?))
            .and_then(|n| {
                n.checked_add(std::mem::size_of::<
                Result<Result<Option<T>, NativeObservationError>, Box<dyn std::any::Any + Send>>
            >().checked_mul(2)?)
            })
            .and_then(|n| {
                n.checked_add(
                    std::mem::size_of::<Result<Result<(), std::io::Error>, Box<dyn std::any::Any + Send>>>()
                        .checked_mul(2)?,
                )
            })
            .and_then(|n| n.checked_add(NativeExitObservation::operation_stack_bytes()?))
            .and_then(|n| n.checked_add(2 * std::mem::size_of::<FontWakeInspection<'_, T>>()))
            // take_ready's wrapper -> shared body -> synchronous effect:
            // wrapper deadline/receiver, effect deadline/borrowed arguments,
            // and the added helper return overlap. All are stack-only values.
            .and_then(|n| n.checked_add(std::mem::size_of::<std::time::Instant>().checked_mul(2)?))
            .and_then(|n| n.checked_add(std::mem::size_of::<&mut Self>()))
            .and_then(|n| n.checked_add(std::mem::size_of::<(&NativeExitObservation, &std::thread::JoinHandle<()>)>()))
            .and_then(|n| n.checked_add(std::mem::size_of::<Result<Option<T>, FontSettlementReason>>()))
            .ok_or(kwil_view_model::visual_rows::WrapError::Admission("font native-exit metadata overflow"))?;
        let metadata = budget.reserve_metadata(bytes)?;
        Ok(Self {
            thread: None,
            terminal: Default::default(),
            panic: None,
            started: false,
            retirement_pending: false,
            native_error: None,
            timed_out: false,
            deadline: None,
            budget: budget.clone(),
            _capture_charge: None,
            _metadata: metadata,
        })
    }
    #[cfg(test)]
    pub fn spawn(&mut self, start: impl FnOnce() -> Option<T> + Send + 'static) -> Result<(), FontSpawnFailure> {
        self.spawn_with_wake(start, || Ok(()))
    }
    pub fn spawn_with_wake<S, W>(&mut self, start: S, wake: W) -> Result<(), FontSpawnFailure>
    where
        S: FnOnce() -> Option<T> + Send + 'static,
        W: FnOnce() -> std::io::Result<()> + Send + 'static,
    {
        if self.started
            || self.thread.is_some()
            || self.deadline.is_some()
            || self.panic.is_some()
            || self.timed_out
            || self.retirement_pending
            || self.native_error.is_some()
        {
            return Err(FontSpawnFailure::AlreadyStarted);
        }
        let bytes = std::mem::size_of::<(S, W, std::sync::Arc<FontTerminal<T>>)>()
            .checked_mul(2)
            .and_then(|n| n.checked_add(std::mem::size_of::<Result<(), FontSpawnFailure>>().checked_mul(2)?))
            .and_then(|n| n.checked_add(std::mem::size_of::<Result<(), std::io::Error>>().checked_mul(2)?))
            .ok_or(FontSpawnFailure::Admission(kwil_view_model::visual_rows::WrapError::Admission(
                "font wake capture overflow",
            )))?;
        self._capture_charge = Some(self.budget.reserve_metadata(bytes).map_err(FontSpawnFailure::Admission)?);
        let terminal = self.terminal.clone();
        self.thread = Some(
            std::thread::Builder::new()
                .spawn(move || {
                    let exit = FontExit(&terminal);
                    let prepared = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        terminal.native.register_current()?;
                        Ok::<_, NativeObservationError>(start())
                    }));
                    let original_panic = match prepared {
                        Ok(Ok(product)) => {
                            let mut state = terminal.state.lock().unwrap_or_else(|e| e.into_inner());
                            state.product = product;
                            state.ready = true;
                            None
                        }
                        Ok(Err(error)) => {
                            terminal.state.lock().unwrap_or_else(|e| e.into_inner()).failure =
                                Some(FontProductFailure::Native(error));
                            None
                        }
                        Err(payload) => {
                            terminal.state.lock().unwrap_or_else(|e| e.into_inner()).failure =
                                Some(FontProductFailure::WorkerPanicked);
                            Some(payload)
                        }
                    };
                    // Complete all ordinary mutex writes BEFORE the sole normal wake.
                    // A nonblocking UI take cannot lose it to the done update's lock.
                    drop(exit);
                    // Readiness precedes its sole wake. A late wake failure cannot
                    // revoke an already transferred immutable UI product.
                    let delivery = std::panic::catch_unwind(std::panic::AssertUnwindSafe(wake));
                    match delivery {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            let mut state = terminal.state.lock().unwrap_or_else(|e| e.into_inner());
                            state.wake_error = Some(error);
                        }
                        Err(payload) => {
                            let mut state = terminal.state.lock().unwrap_or_else(|e| e.into_inner());
                            state.wake_panic = Some(payload);
                        }
                    }
                    // Resume the original body panic only after its owner-free wake;
                    // a secondary wake panic stays intact in the existing allocation.
                    if let Some(payload) = original_panic {
                        std::panic::resume_unwind(payload);
                    }
                })
                .map_err(FontSpawnFailure::Spawn)?,
        );
        self.started = true;
        Ok(())
    }
    pub fn try_take_product(&mut self) -> FontProduct<T> {
        if let Some(error) = self.native_error
            && (self.thread.is_some() || self.retirement_pending)
        {
            return FontProduct::Failed(FontProductFailure::Native(error));
        }
        if self.timed_out {
            return FontProduct::Failed(FontProductFailure::TimedOut);
        }
        if self.retirement_pending {
            return FontProduct::Pending;
        }
        let mut state = match self.terminal.state.try_lock() {
            Ok(state) => state,
            Err(std::sync::TryLockError::WouldBlock) => return FontProduct::Pending,
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
        };
        if let Some(reason) = state.failure() {
            return FontProduct::Failed(reason);
        }
        if state.taken {
            return FontProduct::Failed(FontProductFailure::AlreadyTaken);
        }
        if !state.ready {
            return if state.done {
                FontProduct::Failed(FontProductFailure::WorkerPanicked)
            } else {
                FontProduct::Pending
            };
        }
        state.taken = true;
        FontProduct::Ready(state.product.take())
    }
    pub fn panic_payload(&self) -> Option<&(dyn std::any::Any + Send)> {
        self.panic.as_deref()
    }
    pub(super) fn wake_inspection(&self) -> FontWakeInspection<'_, T> {
        FontWakeInspection { state: self.terminal.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner) }
    }
    /// OFF UI: successful native join is followed by explicit right retirement.
    /// ROOT's startup caller must use the existing off-UI executor as shutdown does.
    #[cfg(test)]
    pub fn take_ready(&mut self, deadline: std::time::Instant) -> Result<Option<T>, FontSettlementReason> {
        self.take_ready_with(deadline, |native, handle, _| native.query(handle))
    }
    fn take_ready_with(
        &mut self,
        deadline: std::time::Instant,
        query: impl FnOnce(
            &NativeExitObservation,
            &std::thread::JoinHandle<()>,
            std::time::Instant,
        ) -> Result<NativeExitState, NativeObservationError>,
    ) -> Result<Option<T>, FontSettlementReason> {
        let deadline = self.deadline.map_or(deadline, |stored| stored.min(deadline));
        self.deadline = Some(deadline);
        // A previous join can have left a counted right and a font/panic owner.
        // Retire first even on the thread=None and sticky-panic retry paths.
        self.retire_native(deadline)?;
        if self.panic.is_some() {
            return Err(FontSettlementReason::WorkerPanicked);
        }
        if self.timed_out {
            return Err(FontSettlementReason::TimedOut);
        }
        if self.thread.is_none() {
            return self.take_settled_product();
        }
        self.check_deadline(deadline)?;
        let state = self.terminal.state.lock().unwrap_or_else(|error| error.into_inner());
        let (state, _) = self
            .terminal
            .changed
            .wait_timeout_while(state, deadline.saturating_duration_since(std::time::Instant::now()), |state| {
                !state.done
            })
            .unwrap_or_else(|error| error.into_inner());
        if !state.done {
            self.timed_out = true;
            return Err(FontSettlementReason::TimedOut);
        }
        drop(state);
        self.check_deadline(deadline)?;
        let observed = match query(&self.terminal.native, self.thread.as_ref().unwrap(), deadline) {
            Err(error) => return Err(self.retain_native_error(error)),
            Ok(state) => state,
        };
        self.check_deadline(deadline)?;
        if observed == NativeExitState::Pending {
            return Err(FontSettlementReason::ThreadExitPending);
        }
        // The worker already captured T in the ONE terminal slot. The original
        // handle now returns only (); retirement cannot drop or publish T.
        if let Err(payload) = self.thread.take().unwrap().join() {
            self.panic = Some(payload);
        }
        self.retirement_pending = true;
        self.retire_native(deadline)?;
        if self.panic.is_some() {
            return Err(FontSettlementReason::WorkerPanicked);
        }
        self.take_settled_product()
    }
    fn take_settled_product(&mut self) -> Result<Option<T>, FontSettlementReason> {
        let mut state = self.terminal.state.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(reason) = state.failure() {
            return Err(FontSettlementReason::Product(reason));
        }
        if state.taken || !self.started {
            return Ok(None);
        }
        if !state.ready {
            return Err(FontSettlementReason::WorkerPanicked);
        }
        state.taken = true;
        Ok(state.product.take())
    }
    fn check_deadline(&mut self, deadline: std::time::Instant) -> Result<(), FontSettlementReason> {
        if std::time::Instant::now() >= deadline {
            self.timed_out = true;
            Err(FontSettlementReason::TimedOut)
        } else {
            Ok(())
        }
    }
    fn retain_native_error(&mut self, error: NativeObservationError) -> FontSettlementReason {
        FontSettlementReason::Native(*self.native_error.get_or_insert(error))
    }
    fn retire_native(&mut self, deadline: std::time::Instant) -> Result<(), FontSettlementReason> {
        self.retire_native_with(deadline, NativeExitObservation::retire_after_join)
    }
    fn retire_native_with(
        &mut self,
        deadline: std::time::Instant,
        retire: impl FnOnce(&NativeExitObservation) -> Result<(), NativeObservationError>,
    ) -> Result<(), FontSettlementReason> {
        if self.retirement_pending {
            self.check_deadline(deadline)?;
            if let Err(error) = retire(&self.terminal.native) {
                return Err(self.retain_native_error(error));
            }
            self.retirement_pending = false;
            self.check_deadline(deadline)?;
        }
        Ok(())
    }
    pub fn settle_until(&mut self, deadline: std::time::Instant) -> Result<(), FontSettlementReason> {
        drop(self.take_ready_with(deadline, NativeExitObservation::wait_until)?);
        Ok(())
    }
    #[cfg(test)]
    pub(super) fn notification_fixture_cleanup_until(
        &mut self,
        cleanup_deadline: std::time::Instant,
    ) -> Result<(), FontSettlementReason> {
        // Fixture teardown ONLY. Never pass cleanup_deadline to a production
        // settlement/deadline setter, and never clear the original verdict.
        if std::time::Instant::now() >= cleanup_deadline {
            return Err(FontSettlementReason::TimedOut);
        }
        if let Some(original) = &self.thread {
            let observed =
                self.terminal.native.wait_until(original, cleanup_deadline).map_err(FontSettlementReason::Native)?;
            if observed != NativeExitState::Exited {
                return Err(FontSettlementReason::ThreadExitPending);
            }
            if std::time::Instant::now() >= cleanup_deadline {
                return Err(FontSettlementReason::TimedOut);
            }
            if let Err(original) = self.thread.take().unwrap().join() {
                if self.panic.is_none() {
                    self.panic = Some(original);
                } else {
                    std::mem::forget(original);
                }
            }
            self.retirement_pending = true;
        }
        if self.retirement_pending {
            if std::time::Instant::now() >= cleanup_deadline {
                return Err(FontSettlementReason::TimedOut);
            }
            self.terminal.native.retire_after_join().map_err(FontSettlementReason::Native)?;
            self.retirement_pending = false;
        }
        if std::time::Instant::now() >= cleanup_deadline {
            return Err(FontSettlementReason::TimedOut);
        }
        Ok(())
    }
}

pub(super) struct Host {
    pub ever_started: bool,
    pub transport: Option<WorkspaceDocumentTransport>,
    pub update: Option<WorkspaceDocumentUpdate>,
    /// A post-spawn Prepare refusal keeps its exact request beside the receiver.
    pub prepare_refusal: Option<std::sync::mpsc::TrySendError<Box<DocumentRequest>>>,
    pub start_failure: Option<WorkspaceDocumentStartFailure>,
    pub failure: Option<DocumentResultSettlementReason>,
    pub wrap_retry: Option<kwil_view_model::document_worker::DocumentWrapRetryToken>,
    pub wrap_retry_refusal: Option<kwil_view_model::document_worker::DocumentWrapRetryRefusal>,
    pub reset: Option<kwil_view_model::document_worker::DocumentProducerRetryToken>,
    pub reset_refusal: Option<kwil_view_model::document_worker::DocumentProducerRetryRefusal>,
    pub delivery_failure: Option<kwil_view_model::document_worker::DocumentDeliveryFailure>,
    pub row_shift: i64,
    deferred_wake: bool,
    _metadata: MetadataReservation,
}
impl Host {
    pub fn new(budget: &AllocationBudget) -> WrapResult<Self> {
        // Charge exception/control/return overlap BEFORE any worker can start.
        let metadata = budget.reserve_metadata(2 * std::mem::size_of::<Self>())?;
        Ok(Self {
            ever_started: false,
            transport: None,
            update: None,
            prepare_refusal: None,
            start_failure: None,
            failure: None,
            wrap_retry: None,
            wrap_retry_refusal: None,
            reset: None,
            reset_refusal: None,
            delivery_failure: None,
            row_shift: 0,
            deferred_wake: false,
            _metadata: metadata,
        })
    }
}

impl App {
    pub(super) fn document_input_blocked(&self) -> bool {
        self.document.live.prepare_refusal.is_some()
            || self.document.live.failure.is_some()
            || self.document.live.wrap_retry_refusal.is_some()
            || self.document.live.reset_refusal.is_some()
    }
    pub(super) fn start_document_at(&mut self, root: Option<std::path::PathBuf>) {
        let proxy = self.proxy.clone();
        match self.view.start_bounded_document_worker_at(root, self.wrap.budget(), move || {
            proxy
                .send_owned_wake(UserEvent::DocumentReady)
                .map(|_| ())
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "GUI document wake closed"))
        }) {
            Ok(transport) => {
                self.document.live.ever_started = true;
                self.document.live.transport = Some(transport);
            }
            Err(WorkspaceDocumentStartFailure::PrepareRefused(refusal)) => {
                self.document.live.ever_started = true;
                self.document.live.transport = Some(refusal.transport);
                self.document.live.prepare_refusal = Some(refusal.error);
                self.view.set_notice("File worker preparation refused; original request retained".into());
            }
            Err(failure) => {
                self.view.set_notice(format!("File worker: {failure:?}"));
                self.document.live.start_failure = Some(failure);
            }
        }
    }

    /// Exactly one sealed receive per real event. False admission leaves the
    /// original completion in the original worker slot, including harness holds.
    pub(super) fn document_ready(&mut self) {
        self.document_ack_wrap_retry();
        if self.document.live.update.is_some() {
            self.document.live.deferred_wake = true;
            return;
        }
        let admitted = !self.wrap_delivery_blocked() && self.document.live.prepare_refusal.is_none();
        if !admitted {
            self.document.live.deferred_wake = true;
        }
        let Some(transport) = &mut self.document.live.transport else {
            return;
        };
        #[cfg(feature = "frame-harness")]
        let admitted = admitted
            && !harness::hold_preview_completion(&mut self.metrics, transport.result_info(), self.view.active_id());
        let update = match transport.receive(&mut self.view, admitted) {
            Ok(Some(update)) => update,
            Ok(None) => return,
            Err(DocumentResultSettlementReason::ConsumerRefused)
                if !admitted && transport.status().failure.is_none() =>
            {
                // R8 false admission is a typed refusal, not an empty slot.
                // No transfer occurred and no producer failure is suppressed.
                return;
            }
            Err(reason) => {
                self.document.live.failure.get_or_insert(reason);
                // The transport retains the original typed error/panic and
                // whole/partial completion; this string is only its notice.
                self.view.set_notice(format!("Document delivery retained: {reason:?}"));
                self.pending.full = true;
                self.pending.status = true;
                self.request_frame();
                return;
            }
        };
        let replacement = update.replacement;
        let damage = update.damage();
        let scalar = Damage { full: damage.full, status: damage.status, scroll: damage.scroll, rows: Vec::new() };
        let rows = self.text_rows();
        queue_damage(&mut self.pending, scalar, rows, self.font.as_mut());
        self.document.live.row_shift = 0;
        self.document.live.update = Some(update);
        if self.view.wrap_cleanup_ready() {
            self.wrap_document_settled();
            self.document_ack_wrap_retry();
        }
        if let Some(receipt) = replacement {
            self.search_replacement_finished(receipt);
        }
        self.sync_file_ui();
        if let Some((document, epoch)) = self.goto_reveal {
            if document != self.view.active_id() || epoch != self.view.input_epoch() {
                self.goto_reveal = None;
            } else if self.view.editor().is_some() {
                self.goto_reveal = None;
                self.keep_caret_visible();
            }
        }
        self.wrap_sync_live();
        self.wrap_lend_source();
        // No rows are copied into pending. The renderer borrows them while
        // this exact update retains its metadata reservation.
        if !self.wrap_owns_text()
            && !self.view.wrap_input_blocked()
            && let Some(host) = &mut self.highlighting
        {
            host.invalidate_painted(&self.view, self.document.live.update.as_ref().unwrap().damage());
        }
        self.apply(Damage::default());
        self.request_frame();
    }

    /// Applying Workspace's receipt precedes the original producer's residual
    /// retirement. ACK only after that producer's real empty-capacity wake.
    fn document_ack_wrap_retry(&mut self) {
        if let Some(status) = self.document.live.transport.as_ref().map(|transport| transport.status())
            && status.failure.is_some()
            && !status.retry_pending
            && !status.retry_acknowledgement_required
        {
            // A completed failed attempt has no ACK authority. Retire only
            // its scalar token; the exact failed wrap owner stays in transport.
            self.document.live.wrap_retry = None;
        }
        let ready = self.view.wrap_cleanup_ready()
            && self.document.live.wrap_retry.is_some()
            && self.document.live.transport.as_ref().is_some_and(|transport| {
                let status = transport.status();
                status.failure.is_none() && status.retry_acknowledgement_required && !status.occupied
            });
        if ready && let Some(token) = self.document.live.wrap_retry.take() {
            match self.document.live.transport.as_mut().unwrap().acknowledge_wrap_retry(token) {
                Ok(()) => self.document.live.failure = None,
                Err(refusal) => self.document.live.wrap_retry_refusal = Some(refusal),
            }
        }
    }

    /// Called after the charged update's row publication, never from idle.
    pub(super) fn document_frame_painted(&mut self) {
        self.document.live.update = None;
        self.document.live.row_shift = 0;
        if std::mem::take(&mut self.document.live.deferred_wake)
            && let Some(transport) = &self.document.live.transport
        {
            transport.wake_ready();
        }
    }
    /// A real layout capacity/result wake can clear receive admission. Rearm
    /// the existing scalar document wake once; never from an idle poll/timer.
    pub(super) fn document_after_wrap_wake(&mut self) {
        if self.document.live.update.is_none()
            && !self.wrap_delivery_blocked()
            && self.document.live.prepare_refusal.is_none()
            && std::mem::take(&mut self.document.live.deferred_wake)
            && let Some(transport) = &self.document.live.transport
        {
            transport.wake_ready();
        }
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;
    use std::{
        cell::RefCell,
        sync::mpsc,
        time::{Duration, Instant},
    };
    /// A real worker; this channel carries only its owner-free wakes.
    pub(crate) struct Lane {
        pub transport: RefCell<Option<WorkspaceDocumentTransport>>,
        rx: mpsc::Receiver<()>,
        _budget: AllocationBudget,
    }
    impl Lane {
        pub fn start(workspace: &mut Workspace, root: std::path::PathBuf) -> Self {
            let budget = AllocationBudget::default();
            let (tx, rx) = mpsc::channel();
            let transport = workspace
                .start_bounded_document_worker_at(Some(root), &budget, move || {
                    tx.send(()).map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "fixture closed"))
                })
                .unwrap();
            Self { transport: RefCell::new(Some(transport)), rx, _budget: budget }
        }
        pub fn until(&self, workspace: &mut Workspace, done: impl Fn(&Workspace) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(5);
            for _ in 0..512 {
                if done(workspace) {
                    return;
                }
                self.rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).expect("real worker wake");
                let update = self
                    .transport
                    .borrow_mut()
                    .as_mut()
                    .unwrap()
                    .receive(workspace, true)
                    .expect("sealed fixture acceptance");
                // Consume bounded paint metadata while its charge stays alive.
                if let Some(update) = update {
                    std::hint::black_box(update.damage());
                }
            }
            panic!("finite real worker receipt count")
        }
        pub fn through(&self, workspace: &mut Workspace, kind: kwil_view_model::workspace_document::ResultKind) {
            let deadline = Instant::now() + Duration::from_secs(5);
            for _ in 0..512 {
                self.rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).expect("scalar worker wake");
                let mut transport = self.transport.borrow_mut();
                let transport = transport.as_mut().unwrap();
                let info = transport.result_info();
                let update = transport.receive(workspace, true).expect("sealed acceptance");
                if let Some(update) = update {
                    std::hint::black_box(update.damage());
                    if info.is_some_and(|info| info.kind == kind) {
                        return;
                    }
                }
            }
            panic!("real worker did not publish expected scalar kind")
        }
        pub fn settle(self, workspace: Workspace) {
            let transport = self.transport.into_inner().unwrap();
            std::thread::spawn(move || {
                let mut bundle = match workspace.into_document_shutdown(transport) {
                    std::ops::ControlFlow::Continue(bundle) => bundle,
                    std::ops::ControlFlow::Break(refusal) => panic!("bind refusal: {:?}", refusal.reason),
                };
                bundle
                    .bind_search_sources(kwil_view_model::workspace_document::DocumentSearchBinding::NoSearchWorker)
                    .unwrap();
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    let settled = match bundle.settle_until(deadline) {
                        Ok(status) => status,
                        Err(kwil_view_model::workspace_document::DocumentShutdownReason::Transport(
                            DocumentResultSettlementReason::ThreadExitPending,
                        )) if Instant::now() < deadline => {
                            std::thread::sleep(
                                Duration::from_millis(1).min(deadline.saturating_duration_since(Instant::now())),
                            );
                            continue;
                        }
                        Err(reason) => panic!("exact real worker settlement: {reason:?}"),
                    };
                    match settled {
                        kwil_view_model::workspace_document::DocumentShutdownStatus::SourcesReleased => {}
                        kwil_view_model::workspace_document::DocumentShutdownStatus::Complete => break,
                        status => panic!("unexpected fixture shutdown control: {status:?}"),
                    }
                }
            })
            .join()
            .unwrap();
        }
    }
    /// The headless host consumes the actual borrowed update, then releases
    /// its charge. It never constructs or injects a document result.
    pub(crate) fn publish(app: &mut App) {
        if let Some(update) = &app.document.live.update {
            for row in &update.damage().rows {
                std::hint::black_box(row);
            }
        }
        app.document_frame_painted();
    }
    pub(crate) fn settle_app(app: App) {
        let App {
            document: mut failure,
            view,
            wrap,
            markdown,
            logs,
            highlighting,
            search_panel,
            search,
            external_inbox,
            external_open,
            ..
        } = app;
        failure.attach(shutdown::Owners {
            view,
            wrap,
            markdown,
            logs,
            highlighting,
            search_panel,
            search,
            external_inbox,
            external_open,
        });
        // A worker's terminal ACK can precede its actual OS thread exit. This
        // explicit test owner may retry ONLY that exact pending join under the
        // SAME absolute deadline; production never polls it into success.
        let mut failure = std::mem::ManuallyDrop::new(failure);
        let original = (&**failure) as *const shutdown::Failure;
        let deadline = failure.deadline().expect("original accepted deadline");
        loop {
            let result = failure.retry();
            assert_eq!((&**failure) as *const shutdown::Failure, original);
            assert_eq!(failure.deadline(), Some(deadline));
            match result {
                Ok(()) => {
                    drop(std::mem::ManuallyDrop::into_inner(failure));
                    return;
                }
                Err(
                    shutdown::Status::Document(kwil_view_model::workspace_document::DocumentShutdownReason::Transport(
                        DocumentResultSettlementReason::ThreadExitPending,
                    ))
                    | shutdown::Status::Search(
                        kwil_view_model::search_session::WorkerSettlementReason::ThreadExitPending,
                    ),
                ) if Instant::now() < deadline => {
                    std::thread::sleep(
                        Duration::from_millis(1).min(deadline.saturating_duration_since(Instant::now())),
                    );
                }
                Err(reason) => {
                    let scalars = failure.document_state_for_test();
                    let inspection = failure.retained_document_error();
                    let original = inspection.as_ref().and_then(|inspection| inspection.retained_commit_error());
                    panic!("retained original shutdown owner: {reason:?}; scalars={scalars:?}; original={original:?}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kwil_view_model::workspace_document::ResultKind;
    use std::time::{Duration, Instant};

    struct FontEpilogue {
        entered: std::sync::mpsc::SyncSender<std::thread::ThreadId>,
        release: std::sync::mpsc::Receiver<()>,
    }
    impl Drop for FontEpilogue {
        fn drop(&mut self) {
            self.entered.send(std::thread::current().id()).unwrap();
            let _ = self.release.recv_timeout(Duration::from_secs(5));
        }
    }
    thread_local! {
        static FONT_EPILOGUE: std::cell::RefCell<Option<FontEpilogue>> = const { std::cell::RefCell::new(None) };
    }
    #[test]
    fn product_is_taken_once_while_original_native_tls_still_blocks_settlement() {
        let budget = AllocationBudget::default();
        let mut font = std::mem::ManuallyDrop::new(StartupFont::<usize>::admit(&budget).unwrap());
        let (entered, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release, release_rx) = std::sync::mpsc::sync_channel(1);
        font.spawn_with_wake(
            move || {
                FONT_EPILOGUE.with(|slot| *slot.borrow_mut() = Some(FontEpilogue { entered, release: release_rx }));
                Some(73)
            },
            || Ok(()),
        )
        .unwrap();
        let original = font.thread.as_ref().unwrap().thread().id();
        let epilogue = entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let product = font.try_take_product();
        let duplicate = font.try_take_product();
        let pending = font.take_ready(Instant::now() + Duration::from_secs(5));
        let retained = font.thread.as_ref().map(|thread| thread.thread().id());
        release.send(()).unwrap();
        let mut font = std::thread::spawn(move || {
            // Explicit fixture cleanup, not native settlement acceptance.
            font.thread.take().unwrap().join().unwrap();
            font.terminal.native.retire_after_join().unwrap();
            font
        })
        .join()
        .unwrap();
        assert!(matches!(product, FontProduct::Ready(Some(73))));
        assert!(matches!(duplicate, FontProduct::Failed(FontProductFailure::AlreadyTaken)));
        assert_eq!(pending, Err(FontSettlementReason::ThreadExitPending));
        assert_eq!(retained, Some(original));
        assert_eq!(epilogue, original);
        let deadline = font.deadline.unwrap();
        assert_eq!(font.take_ready(deadline).unwrap(), None);
        drop(std::mem::ManuallyDrop::into_inner(font));
    }
    #[test]
    fn late_wake_failure_keeps_transferred_product_and_is_reported_after_retirement() {
        let budget = AllocationBudget::default();
        let mut font = std::mem::ManuallyDrop::new(StartupFont::<usize>::admit(&budget).unwrap());
        let (entered, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release, release_rx) = std::sync::mpsc::sync_channel(1);
        font.spawn_with_wake(
            || Some(73),
            move || {
                entered.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
            },
        )
        .unwrap();
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let product = font.try_take_product();
        let deadline = Instant::now() + Duration::from_secs(5);
        font.deadline = Some(deadline);
        release.send(()).unwrap();
        let mut font = std::thread::spawn(move || {
            font.thread.take().unwrap().join().unwrap();
            font.retirement_pending = true;
            font
        })
        .join()
        .unwrap();
        let reason = FontProductFailure::WakeFailed { kind: std::io::ErrorKind::BrokenPipe, code: None };
        assert_eq!(font.take_ready(deadline), Err(FontSettlementReason::Product(reason)));
        assert!(!font.retirement_pending);
        assert!(font.terminal.state.lock().unwrap().wake_error.is_some());
        assert!(matches!(product, FontProduct::Ready(Some(73))), "UI ownership is never retracted");
        assert!(matches!(font.try_take_product(), FontProduct::Failed(failure) if failure == reason));
        drop(std::mem::ManuallyDrop::into_inner(font));
    }
    #[test]
    fn joined_font_retirement_failure_retains_value_and_retries_without_a_handle() {
        let budget = AllocationBudget::default();
        let mut font = std::mem::ManuallyDrop::new(StartupFont::<usize>::admit(&budget).unwrap());
        font.spawn(|| Some(73)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        font.deadline = Some(deadline);
        // Explicit fixture setup joins its tiny source-free worker. This is
        // not the production native-exit gate or a settlement success claim.
        font.thread.take().unwrap().join().unwrap();
        font.retirement_pending = true;
        let original = NativeObservationError::MachRetire { code: 73 };
        assert_eq!(font.retire_native_with(deadline, |_| Err(original)), Err(FontSettlementReason::Native(original)));
        assert!(font.thread.is_none());
        assert_eq!(font.terminal.state.lock().unwrap().product, Some(73));
        assert!(font.retirement_pending);
        assert_eq!(font.native_error, Some(original));
        assert_eq!(font.take_ready(deadline).unwrap(), Some(73));
        assert!(!font.retirement_pending);
        assert_eq!(font.native_error, Some(original), "first original code remains inspectable");
        drop(std::mem::ManuallyDrop::into_inner(font));
    }
    #[test]
    fn joined_font_panic_retires_before_returning_the_same_sticky_payload() {
        let budget = AllocationBudget::default();
        let mut font = std::mem::ManuallyDrop::new(StartupFont::<usize>::admit(&budget).unwrap());
        font.spawn(|| panic!("original font panic")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        font.deadline = Some(deadline);
        font.panic = Some(font.thread.take().unwrap().join().unwrap_err());
        font.retirement_pending = true;
        assert_eq!(font.panic_payload().unwrap().downcast_ref::<&str>(), Some(&"original font panic"));
        let payload = std::ptr::from_ref(font.panic_payload().unwrap());
        let original = NativeObservationError::MachRetire { code: 91 };
        assert_eq!(font.retire_native_with(deadline, |_| Err(original)), Err(FontSettlementReason::Native(original)));
        assert!(font.retirement_pending);
        assert_eq!(font.take_ready(deadline), Err(FontSettlementReason::WorkerPanicked));
        assert!(!font.retirement_pending, "sticky panic cannot bypass native right retirement");
        assert!(std::ptr::eq(payload, std::ptr::from_ref(font.panic_payload().unwrap())));
        assert_eq!(font.native_error, Some(original));
        drop(std::mem::ManuallyDrop::into_inner(font));
    }
    #[test]
    fn actual_startup_thread_epilogue_retains_original_handle_until_actual_join() {
        let budget = AllocationBudget::default();
        let mut font = StartupFont::<usize>::admit(&budget).unwrap();
        let (entered, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release, release_rx) = std::sync::mpsc::sync_channel(1);
        font.spawn(move || {
            FONT_EPILOGUE.with(|slot| *slot.borrow_mut() = Some(FontEpilogue { entered, release: release_rx }));
            Some(73)
        })
        .unwrap();
        let original = font.thread.as_ref().unwrap().thread().id();
        let epilogue_thread = entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let observer_thread = std::thread::current().id();
        let terminal_done = font.terminal.state.lock().unwrap().done;
        let function_finished = font.thread.as_ref().unwrap().is_finished();
        let attempt = Instant::now();
        let refused = font.take_ready(Instant::now() + Duration::from_secs(5));
        let elapsed = attempt.elapsed();
        let retained = font.thread.as_ref().map(|thread| thread.thread().id());
        let _ = release.send(());
        // Fixture owns the explicit terminal join, outside the UI. The
        // protocol refusal above was not promoted to completed retirement.
        let completed = std::thread::spawn(move || {
            let mut font = std::mem::ManuallyDrop::new(font);
            let joined = font.thread.take().map(|thread| thread.join().unwrap());
            let value = joined.map(|()| {
                font.terminal.native.retire_after_join().unwrap();
                let mut state = font.terminal.state.lock().unwrap();
                state.taken = true;
                state.product.take()
            });
            drop(std::mem::ManuallyDrop::into_inner(font));
            value
        })
        .join()
        .unwrap();
        eprintln!(
            "font epilogue probe: original={original:?} epilogue={epilogue_thread:?} observer={observer_thread:?} terminal_done={terminal_done} function_finished={function_finished} refused={refused:?} elapsed={elapsed:?}"
        );
        assert_eq!(refused, Err(FontSettlementReason::ThreadExitPending));
        assert_eq!(epilogue_thread, original);
        assert_eq!(retained, Some(original));
        assert_eq!(completed, Some(Some(73)));
    }
    #[test]
    fn actual_startup_thread_timeout_retains_earliest_deadline_and_original_handle() {
        let budget = AllocationBudget::default();
        let mut font = StartupFont::<usize>::admit(&budget).unwrap();
        let (release, release_rx) = std::sync::mpsc::sync_channel(1);
        font.spawn(move || {
            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            Some(91)
        })
        .unwrap();
        let original = font.thread.as_ref().unwrap().thread().id();
        let deadline = Instant::now();
        let first = font.take_ready(deadline);
        let retained = font.thread.as_ref().map(|thread| thread.thread().id());
        let retried = font.take_ready(deadline + Duration::from_secs(5));
        let stored = font.deadline;
        release.send(()).unwrap();
        let completed = std::thread::spawn(move || {
            let mut font = std::mem::ManuallyDrop::new(font);
            let joined = font.thread.take().map(|thread| thread.join().unwrap());
            let value = joined.map(|()| {
                font.terminal.native.retire_after_join().unwrap();
                let mut state = font.terminal.state.lock().unwrap();
                state.taken = true;
                state.product.take()
            });
            drop(std::mem::ManuallyDrop::into_inner(font));
            value
        })
        .join()
        .unwrap();
        assert_eq!(first, Err(FontSettlementReason::TimedOut));
        assert_eq!(retained, Some(original));
        assert_eq!(retried, Err(FontSettlementReason::TimedOut));
        assert_eq!(stored, Some(deadline));
        assert_eq!(completed, Some(Some(91)));
    }

    #[test]
    fn startup_header_refusal_returns_original_owners_before_any_initializer() {
        let workspace = Workspace::new(View::new(None)).unwrap();
        let document = workspace.active_id();
        let editor = workspace.editor().unwrap() as *const kwil_view_model::Editor;
        let input = workspace.editor_input_ticket(document).unwrap();
        let budget = AllocationBudget::new(1);
        let observer = budget.clone();
        let baseline = observer.retained_bytes();
        let refusal = match prepare_startup::<()>(workspace, budget, || panic!("admission must precede initialization"))
        {
            std::ops::ControlFlow::Break(refusal) => refusal,
            std::ops::ControlFlow::Continue(owners) => {
                let _retained = std::mem::ManuallyDrop::new(owners);
                panic!("one-byte budget must refuse the real startup headers");
            }
        };
        let refusal = std::mem::ManuallyDrop::new(refusal);
        assert!(matches!(refusal.reason, GuiStartReason::Admission(_)));
        assert_eq!(refusal.workspace.active_id(), document);
        assert_eq!(refusal.workspace.editor().unwrap() as *const kwil_view_model::Editor, editor);
        assert!(refusal.workspace.editor_input_is_current(&input));
        assert_eq!(refusal.budget.retained_bytes(), observer.retained_bytes());
        assert_eq!(
            refusal.budget.retained_bytes(),
            baseline,
            "no startup reservation remains beyond the original counter header"
        );
        let refusal = std::mem::ManuallyDrop::into_inner(refusal);
        std::thread::spawn(move || drop(refusal)).join().unwrap();
    }

    #[test]
    fn startup_initializer_panic_returns_original_payload_workspace_and_budget() {
        let workspace = Workspace::new(View::new(None)).unwrap();
        let document = workspace.active_id();
        let editor = workspace.editor().unwrap() as *const kwil_view_model::Editor;
        let input = workspace.editor_input_ticket(document).unwrap();
        let budget = AllocationBudget::default();
        let observer = budget.clone();
        let marker = observer.reserve_metadata(271).unwrap();
        let baseline = observer.retained_bytes();
        let cause = std::sync::Arc::new(());
        let original = cause.clone();
        let refusal = match prepare_startup::<()>(workspace, budget, move || std::panic::panic_any(original)) {
            std::ops::ControlFlow::Break(refusal) => refusal,
            std::ops::ControlFlow::Continue(owners) => {
                let _retained = std::mem::ManuallyDrop::new(owners);
                panic!("panicking initializer must return original owners");
            }
        };
        let refusal = std::mem::ManuallyDrop::new(refusal);
        let GuiStartReason::Panicked(payload) = &refusal.reason else { panic!("retain original setup panic") };
        assert!(std::sync::Arc::ptr_eq(payload.downcast_ref::<std::sync::Arc<()>>().unwrap(), &cause));
        assert_eq!(refusal.workspace.active_id(), document);
        assert_eq!(refusal.workspace.editor().unwrap() as *const kwil_view_model::Editor, editor);
        assert!(refusal.workspace.editor_input_is_current(&input));
        assert_eq!(refusal.budget.retained_bytes(), baseline, "admitted headers returned without quarantine");
        drop(marker);
        assert_eq!(refusal.budget.retained_bytes(), observer.retained_bytes());
        let refusal = std::mem::ManuallyDrop::into_inner(refusal);
        std::thread::spawn(move || drop(refusal)).join().unwrap();
    }

    fn dispatch(app: &mut App, event: UserEvent) {
        match event {
            UserEvent::DocumentReady => {
                app.document_ready();
                fixtures::publish(app);
            }
            UserEvent::SearchReady => app.search_ready(),
            _ => panic!("unexpected tiny worker fixture wake"),
        }
    }
    #[test]
    fn genuinely_unstarted_recovery_returns_original_workspace_and_budget_after_real_search_join() {
        let (mut app, _rx) = crate::search_ui::host_dispatch_tests::host("recover original text");
        app.start_search();
        let document = app.view.active_id();
        let original = app.view.editor().unwrap().document().hex_source(document.0);
        let observer_budget = app.wrap.budget().clone();
        let probe = observer_budget.reserve_metadata(1234).unwrap();
        let App {
            document: mut failure,
            view,
            wrap,
            markdown,
            logs,
            highlighting,
            search_panel,
            search,
            external_inbox,
            external_open,
            ..
        } = app;
        failure.frontend_error = Some(GuiError::Unavailable("isolated unavailable GUI".into()));
        failure.attach(shutdown::Owners {
            view,
            wrap,
            markdown,
            logs,
            highlighting,
            search_panel,
            search,
            external_inbox,
            external_open,
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        let (workspace, budget) = loop {
            match failure.recover_unstarted(deadline) {
                std::ops::ControlFlow::Continue(owners) => break owners,
                std::ops::ControlFlow::Break(retained)
                    if retained.status()
                        == shutdown::Status::Search(
                            kwil_view_model::search_session::WorkerSettlementReason::ThreadExitPending,
                        )
                        && Instant::now() < deadline =>
                {
                    failure = retained;
                    std::thread::sleep(
                        Duration::from_millis(1).min(deadline.saturating_duration_since(Instant::now())),
                    );
                }
                std::ops::ControlFlow::Break(retained) => {
                    let failure = std::mem::ManuallyDrop::new(retained);
                    panic!("unstarted recovery retained its exact failure: {:?}", failure.status());
                }
            }
        };
        let before = budget.retained_bytes();
        drop(probe);
        assert!(budget.retained_bytes() < before, "returned budget is the original counter");
        assert_eq!(budget.retained_bytes(), observer_budget.retained_bytes());
        // Observe and destroy the returned source on a worker, never on UI.
        let bytes = std::thread::spawn(move || {
            assert_eq!(workspace.active_id(), document);
            let editor = workspace.editor().unwrap();
            assert_eq!(editor.document().hex_source(document.0), original);
            let mut bytes = Vec::new();
            editor.document().snapshot().write_to(&mut bytes).unwrap();
            drop(workspace);
            bytes
        })
        .join()
        .unwrap();
        assert_eq!(bytes, b"recover original text");
    }
    #[test]
    fn charged_gui_publication_defers_receive_without_extracting_the_original_slot() {
        let (mut app, rx) = crate::search_ui::host_dispatch_tests::host("");
        let root = crate::test_support::directory("kwil-gui-document-charge").unwrap();
        let path = root.join("tiny.txt");
        std::fs::write(&path, b"first\nsecond\n").unwrap();
        app.start_document_at(Some(root.join("state")));
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let event = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
            if let UserEvent::DocumentReady = event {
                app.document_ready();
                if app.document.live.update.is_some() && app.view.recovery_ready() {
                    break;
                }
                fixtures::publish(&mut app);
            } else {
                panic!("owner-free preparation wake")
            }
        }
        assert!(!app.settings.editor.word_wrap);
        let retained_rows = app.document.live.update.as_ref().unwrap().damage().rows.as_ptr();
        app.view.open_path(path.clone()).unwrap();
        let held = loop {
            let event = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
            assert!(matches!(event, UserEvent::DocumentReady));
            if let Some(info) = app.document.live.transport.as_ref().unwrap().result_info() {
                break info;
            }
        };
        app.document_ready();
        assert_eq!(app.document.live.transport.as_ref().unwrap().result_info(), Some(held));
        assert_eq!(app.document.live.update.as_ref().unwrap().damage().rows.as_ptr(), retained_rows);
        assert!(app.document.live.transport.as_ref().unwrap().status().occupied);
        assert!(app.view.document_busy(app.view.active_id()));
        fixtures::publish(&mut app);
        while app.view.document_busy(app.view.active_id()) || app.view.editor().is_none() {
            let event = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
            dispatch(&mut app, event);
        }
        assert_eq!(app.view.editor().unwrap().document().path(), Some(path.as_path()));
        fixtures::settle_app(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn accepted_exit_drains_late_binary_from_original_slot_and_releases_search_before_join() {
        let (mut app, rx) = crate::search_ui::host_dispatch_tests::host("");
        let root = crate::test_support::directory("kwil-gui-document-exit").unwrap();
        let path = root.join("late.bin");
        std::fs::write(&path, b"header\0binary\x01\n").unwrap();
        app.start_document_at(Some(root.join("state")));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !app.view.recovery_ready() {
            dispatch(&mut app, rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap());
        }
        app.start_search();
        app.view.open_path(path).unwrap();
        let held = loop {
            let event = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
            if let UserEvent::DocumentReady = event {
                if let Some(info) = app.document.live.transport.as_ref().unwrap().result_info()
                    && info.kind == ResultKind::Binary
                {
                    break info;
                }
                app.document_ready();
                fixtures::publish(&mut app);
            } else {
                dispatch(&mut app, event);
            }
        };
        assert!(app.view.binary_offer().is_none(), "late source result remains in the worker slot");
        assert_eq!(app.document.live.transport.as_ref().unwrap().result_info(), Some(held));
        // Uses the production retained bundle, original search key/input ACK,
        // source barrier and both actual thread joins. No synthetic result.
        fixtures::settle_app(app);
        std::fs::remove_dir_all(root).unwrap();
    }
}
