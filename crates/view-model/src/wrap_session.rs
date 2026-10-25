//! One fixed wrapping worker and a separate lossless relative-input FIFO.
//!
//! ROOT must call `stop`, retain its ticket on failure, and settle it on the
//! existing off-UI shutdown lane BEFORE the document cleanup lane. Dropping a
//! session/ticket is never an acknowledgement of successful cleanup.
use crate::visual_rows::task::{TaskRequest, TaskStep};
use crate::visual_rows::*;
use kwil_core_text::compare_input::ReadWork;
use std::{
    cell::Cell,
    fmt,
    mem::size_of,
    ops::ControlFlow,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub const RELATIVE_CAPACITY: usize = 128;
const RETIREMENT_SLOTS: usize = 2;
const WORKER_STACK_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelativeKind {
    Move { motion: VisualMotion, count: u32, extend: bool },
    Scroll(i64),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RelativeInput {
    pub kind: RelativeKind,
    pub key: LayoutKey,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RelativeRecord {
    pub id: u64,
    pub input: RelativeInput,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueError {
    Full,
    Failed,
    StaleGeometry,
    WrongHead,
    CounterExhausted,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MutationGate {
    Allowed,
    RefusedPending,
    RefusedFailed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelReason {
    Escape,
    SourceChanged,
    TabChanged,
    BodyChanged,
    GeometryChanged,
    Shutdown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CancelledInputs {
    pub reason: CancelReason,
    pub records: usize,
}

/// Scalar metadata only. No source, page, editor or display string per record.
/// ROOT pops the head ONLY after atomic editor/page publication succeeds.
pub struct RelativeQueue {
    records: [Option<RelativeRecord>; RELATIVE_CAPACITY],
    start: usize,
    length: usize,
    next_id: u64,
    active: bool,
    failed: bool,
}
impl Default for RelativeQueue {
    fn default() -> Self {
        Self { records: [None; RELATIVE_CAPACITY], start: 0, length: 0, next_id: 1, active: false, failed: false }
    }
}
impl RelativeQueue {
    pub fn len(&self) -> usize {
        self.length
    }
    pub fn is_empty(&self) -> bool {
        self.length == 0
    }
    pub fn head(&self) -> Option<RelativeRecord> {
        self.records[self.start]
    }
    #[cfg(test)]
    pub(crate) fn record_ids(&self) -> impl Iterator<Item = u64> + '_ {
        (0..self.length)
            .map(|offset| self.records[(self.start + offset) % RELATIVE_CAPACITY].expect("queued record").id)
    }
    pub fn enqueue(&mut self, input: RelativeInput) -> Result<u64, QueueError> {
        if self.failed {
            return Err(QueueError::Failed);
        }
        if self.length == RELATIVE_CAPACITY {
            return Err(QueueError::Full);
        }
        let next = self.next_id.checked_add(1).ok_or(QueueError::CounterExhausted)?;
        let id = self.next_id;
        self.next_id = next;
        self.records[(self.start + self.length) % RELATIVE_CAPACITY] = Some(RelativeRecord { id, input });
        self.length += 1;
        Ok(id)
    }
    /// Captures current geometry at dispatch, never when a later result arrives.
    pub fn begin(&mut self, key: LayoutKey) -> Result<Option<RelativeRecord>, QueueError> {
        if self.failed {
            return Err(QueueError::Failed);
        }
        let Some(head) = self.head() else { return Ok(None) };
        if head.input.key != key {
            return Err(QueueError::StaleGeometry);
        }
        self.active = true;
        Ok(Some(head))
    }
    pub fn mutation_gate(&self) -> MutationGate {
        if self.failed {
            MutationGate::RefusedFailed
        } else if self.length != 0 {
            MutationGate::RefusedPending
        } else {
            MutationGate::Allowed
        }
    }
    pub fn mark_failed(&mut self, id: u64) -> Result<(), QueueError> {
        if self.head().is_none_or(|head| head.id != id) {
            return Err(QueueError::WrongHead);
        }
        self.failed = true;
        Ok(())
    }
    pub fn retry(&mut self) {
        self.failed = false;
    }
    pub(crate) fn published(&mut self, id: u64) -> Result<(), QueueError> {
        if !self.active || self.failed || self.head().is_none_or(|head| head.id != id) {
            return Err(QueueError::WrongHead);
        }
        self.records[self.start] = None;
        self.start = (self.start + 1) % RELATIVE_CAPACITY;
        self.length -= 1;
        self.active = false;
        Ok(())
    }
    pub fn cancel(&mut self, reason: CancelReason) -> CancelledInputs {
        let records = self.length;
        self.records.fill(None);
        self.start = 0;
        self.length = 0;
        self.active = false;
        self.failed = false;
        CancelledInputs { reason, records }
    }
}

pub enum LayoutIntent {
    Fill,
    Reveal(SourcePoint),
    ScrollRows(i64),
    MoveCursors { cursors: CursorSet, motion: VisualMotion, count: u32, extend: bool },
}
impl LayoutIntent {
    fn relative(&self) -> bool {
        matches!(self, Self::ScrollRows(_) | Self::MoveCursors { .. })
    }
}
pub struct WrapRequest {
    pub fence: NavigationFence,
    pub source: Arc<dyn WrapSource>,
    pub anchor: TopAnchor,
    pub intent: LayoutIntent,
}
impl fmt::Debug for WrapRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WrapRequest").field("fence", &self.fence).field("anchor", &self.anchor).finish_non_exhaustive()
    }
}
pub struct LayoutSubmitError {
    pub request: WrapRequest,
    pub error: WrapError,
}
impl fmt::Debug for LayoutSubmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LayoutSubmitError").field("request", &self.request).field("error", &self.error).finish()
    }
}
pub enum LayoutPayload {
    Page(Box<LayoutPage>),
    Navigation(Box<ResolvedNavigation>),
}
impl fmt::Debug for LayoutPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Page(page) => page.fmt(f),
            Self::Navigation(navigation) => navigation.fmt(f),
        }
    }
}
/// The ready slot owns this source. After consuming its payload, ROOT must
/// return the owner through retire_source; an unaccepted owner is returned.
pub struct LayoutUpdate {
    pub fence: NavigationFence,
    pub relative_id: Option<u64>,
    pub result: WrapResult<LayoutPayload>,
    pub source: Arc<dyn WrapSource>,
    generation: u64,
}
impl fmt::Debug for LayoutUpdate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LayoutUpdate")
            .field("fence", &self.fence)
            .field("relative_id", &self.relative_id)
            .field("result", &self.result)
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayoutProgress {
    pub fence: NavigationFence,
    /// Bytes read for this operation (may include bounded replay/context), not
    /// a claim that the source prefix or global wrap index is complete.
    pub scanned: u64,
    pub target: u64,
}
pub enum RetiredOwner {
    Request(WrapRequest),
    Update(LayoutUpdate),
    Source(Arc<dyn WrapSource>),
    Page(LayoutPage),
    VisualState(Box<crate::editor::visual::RetiredVisualState>),
    /// The display state's existing header charge follows this Box. The host
    /// must pre-admit stack-to-Box transfer before constructing it.
    DisplayState(Box<crate::editor::visual::VisualEditorState>),
    /// Construct through PreparedVisualCommit::retire so its embedded active
    /// permit is extracted before this owner enters that permit's own slot.
    PreparedCommit(Box<crate::editor::visual::PreparedVisualCommit>),
}
impl fmt::Debug for RetiredOwner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RetiredOwner(..)")
    }
}

struct Job {
    request: WrapRequest,
    relative_id: Option<u64>,
    generation: u64,
}
struct State {
    dispatch: Option<Job>,
    desired: Option<Job>,
    ready: Option<LayoutUpdate>,
    retired: [Option<RetiredOwner>; RETIREMENT_SLOTS],
    processing: bool,
    retirement_reserved: [bool; RETIREMENT_SLOTS],
    relative_accepted: Option<AcceptedRelative>,
    progress: Option<LayoutProgress>,
    admission_closed: bool,
    terminal_failure: bool,
    terminal_notified: bool,
    intake_waiting_slots: usize,
    relative_admission_waiting: bool,
}
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    desired_generation: AtomicU64,
    relative_generation: AtomicU64,
    stop: AtomicBool,
    acknowledged: AtomicBool,
    wake: Box<dyn Fn() + Send + Sync>,
    _charge: AllocationCharge,
    #[cfg(test)]
    completion_gate: Mutex<Option<Arc<tests::CompletionGate>>>,
    #[cfg(test)]
    settlement_waiting: Mutex<Option<std::sync::mpsc::SyncSender<()>>>,
    #[cfg(test)]
    work_quantum: Mutex<ReadWork>,
}
struct AcceptedRelative {
    id: u64,
    fence: NavigationFence,
    generation: u64,
    delivered_success: bool,
}
struct WorkerControl<'a> {
    shared: &'a Shared,
    relative: bool,
    generation: u64,
    fence: NavigationFence,
    target: u64,
    last_progress: Cell<Instant>,
}
impl LayoutControl for WorkerControl<'_> {
    fn cancelled(&self) -> bool {
        self.shared.stop.load(Ordering::Acquire)
            || self.generation
                != if self.relative {
                    self.shared.relative_generation.load(Ordering::Acquire)
                } else {
                    self.shared.desired_generation.load(Ordering::Acquire)
                }
    }
    fn progress(&self, scanned: u64, target: u64) {
        if self.cancelled() || self.last_progress.get().elapsed() < Duration::from_millis(250) {
            return;
        }
        self.last_progress.set(Instant::now());
        {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.progress = Some(LayoutProgress { fence: self.fence, scanned, target });
        }
        (self.shared.wake)();
    }
}
impl TaskDriver for WorkerControl<'_> {
    fn drive(&mut self, task: &mut dyn TaskStepper) -> TaskEnd {
        let cancel = AtomicBool::new(false);
        loop {
            cancel.store(self.cancelled(), Ordering::Release);
            if cancel.load(Ordering::Acquire) {
                return TaskEnd::Cancelled;
            }
            // A finite active-computation quantum, using the existing source
            // window bound for bytes and operations. No idle readiness retry.
            #[cfg(not(test))]
            let quantum = ReadWork::new(SOURCE_WINDOW as u64, SOURCE_WINDOW as u64);
            #[cfg(test)]
            let quantum = *self.shared.work_quantum.lock().unwrap();
            let step = task.step(quantum, &cancel);
            if self.cancelled() {
                return TaskEnd::Cancelled;
            }
            self.progress(task.read_bytes(), self.target);
            match step {
                TaskStep::Pending => {}
                TaskStep::Ready => return TaskEnd::Ready,
                TaskStep::Cancelled => return TaskEnd::Cancelled,
                TaskStep::Failed(error) => return TaskEnd::Failed(error),
            }
        }
    }
}

pub struct WrapSession {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    pub relative: RelativeQueue,
    budget: AllocationBudget,
}
impl WrapSession {
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> WrapResult<Self> {
        Self::with_budget(AllocationBudget::default(), wake)
    }
    fn fixed_allocation_bytes() -> WrapResult<usize> {
        size_of::<Shared>()
            .checked_add(size_of::<Self>())
            .and_then(|v| v.checked_add(size_of::<SettlementTicket>() + 256 + WORKER_STACK_BYTES))
            .and_then(|v| {
                v.checked_add(
                    size_of::<Job>()
                        + size_of::<LayoutUpdate>()
                        + size_of::<WrapRequest>()
                        + 2 * size_of::<RetiredOwner>()
                        + 2 * size_of::<RetirementPermit>(),
                )
            })
            // Source factory -> generic helper -> return/destructure overlap,
            // plus the borrowed driver and its finite-quantum scalar state.
            .and_then(|v| {
                v.checked_add(
                    3 * size_of::<WorkerExit>()
                        + 2 * size_of::<TaskRequest>()
                        + 2 * size_of::<TaskEnd>()
                        + size_of::<WorkerControl<'static>>()
                        + 2 * size_of::<ReadWork>()
                        + size_of::<AtomicBool>()
                        + 2 * size_of::<&mut dyn TaskStepper>(),
                )
            })
            .and_then(|v| v.checked_add(5 * (size_of::<SnapshotSource>() + 2 * size_of::<usize>())))
            .ok_or(WrapError::Admission("worker metadata overflow"))
    }
    pub fn with_budget(budget: AllocationBudget, wake: impl Fn() + Send + Sync + 'static) -> WrapResult<Self> {
        if size_of_val(&wake) > 256 {
            return Err(WrapError::Admission("wake callback capture limit"));
        }
        let charge = budget.reserve(Self::fixed_allocation_bytes()?)?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                dispatch: None,
                desired: None,
                ready: None,
                retired: [None, None],
                retirement_reserved: [false; RETIREMENT_SLOTS],
                relative_accepted: None,
                processing: false,
                progress: None,
                admission_closed: false,
                terminal_failure: false,
                terminal_notified: false,
                intake_waiting_slots: 0,
                relative_admission_waiting: false,
            }),
            changed: Condvar::new(),
            desired_generation: AtomicU64::new(0),
            relative_generation: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            acknowledged: AtomicBool::new(false),
            wake: Box::new(wake),
            _charge: charge,
            #[cfg(test)]
            completion_gate: Mutex::new(None),
            #[cfg(test)]
            settlement_waiting: Mutex::new(None),
            #[cfg(test)]
            work_quantum: Mutex::new(ReadWork::new(SOURCE_WINDOW as u64, SOURCE_WINDOW as u64)),
        });
        let worker_shared = shared.clone();
        let worker_budget = budget.clone();
        let worker = thread::Builder::new()
            .name("kwil-wrap".into())
            .stack_size(WORKER_STACK_BYTES)
            .spawn(move || {
                let outcome = catch_unwind(AssertUnwindSafe(|| worker_loop(&worker_shared, worker_budget)));
                if !matches!(outcome, Ok(Ok(()))) {
                    let mut state = worker_shared.state.lock().unwrap_or_else(|e| e.into_inner());
                    state.terminal_failure = true;
                    state.admission_closed = true;
                    state.processing = false;
                }
                notify_terminal_failure(&worker_shared);
                // Drain one owner at a time outside the lock; no extra retirement
                // batch doubles the fixed ownership-slot bound.
                loop {
                    let owner = {
                        let mut state = worker_shared.state.lock().unwrap_or_else(|e| e.into_inner());
                        if let Some(job) = state.dispatch.take().or_else(|| state.desired.take()) {
                            Some(RetiredOwner::Request(job.request))
                        } else if let Some(update) = state.ready.take() {
                            Some(RetiredOwner::Update(update))
                        } else if let Some(owner) = state.retired.iter_mut().find_map(Option::take) {
                            Some(owner)
                        } else if state.retirement_reserved.iter().any(|reserved| *reserved) {
                            // Initialization/panic bypasses the normal worker
                            // loop's stop wait. External permits still own their
                            // reservation and must settle before acknowledgement.
                            drop(worker_shared.changed.wait(state).unwrap_or_else(|e| e.into_inner()));
                            continue;
                        } else {
                            None
                        }
                    };
                    let Some(owner) = owner else { break };
                    if catch_unwind(AssertUnwindSafe(|| drop(owner))).is_err() {
                        worker_shared.state.lock().unwrap_or_else(|e| e.into_inner()).terminal_failure = true;
                        notify_terminal_failure(&worker_shared);
                    }
                }
                // Cleanup itself may have failed after a normal stop. Notify
                // once in that case too; a prior terminal wake is never repeated.
                notify_terminal_failure(&worker_shared);
                {
                    let _state = worker_shared.state.lock().unwrap_or_else(|e| e.into_inner());
                    worker_shared.acknowledged.store(true, Ordering::Release);
                }
                worker_shared.changed.notify_all();
                #[cfg(test)]
                if let Some(gate) = worker_shared.completion_gate.lock().unwrap().take() {
                    gate.wait();
                }
            })
            .map_err(WrapError::Io)?;
        Ok(Self { shared, worker: Some(worker), relative: RelativeQueue::default(), budget })
    }
    pub fn budget(&self) -> &AllocationBudget {
        &self.budget
    }
    /// Sticky terminal status, including failure before the first result. Call
    /// on the worker wake: an accepted FIFO head remains owned and explicitly
    /// failed, never popped or silently replayed. This accessor never wakes.
    /// As with settlement, WorkerPanicked is the terminal-worker failure class;
    /// it also covers initialization/admission or owner-cleanup failure.
    pub fn terminal_failure(&mut self) -> Option<WrapError> {
        if !self.shared.state.lock().unwrap_or_else(|e| e.into_inner()).terminal_failure {
            return None;
        }
        if let Some(head) = self.relative.head() {
            let _ = self.relative.mark_failed(head.id);
        }
        Some(WrapError::WorkerPanicked)
    }
    /// Only Fill/absolute Reveal replace the latest desired request. A relative
    /// request must match the already-begun FIFO head, and never supersedes it.
    pub fn submit(&self, request: WrapRequest) -> ControlFlow<LayoutSubmitError, ()> {
        let fail = |request, error| ControlFlow::Break(LayoutSubmitError { request, error });
        if let Err(error) = request.fence.key.validate() {
            return fail(request, error);
        }
        if request.source.identity() != request.fence.key.source {
            return fail(request, WrapError::Stale);
        }
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.terminal_failure {
            return fail(request, WrapError::WorkerPanicked);
        }
        if state.admission_closed {
            return fail(request, WrapError::Closed);
        }
        let relative_id = if request.intent.relative() {
            let Some(head) = self.relative.head().filter(|_| self.relative.active && !self.relative.failed) else {
                return fail(request, WrapError::Invalid("relative request has no active FIFO head"));
            };
            if head.input.key != request.fence.key {
                return fail(request, WrapError::Stale);
            }
            let matches = match (&request.intent, head.input.kind) {
                (LayoutIntent::ScrollRows(delta), RelativeKind::Scroll(expected)) => *delta == expected,
                (
                    LayoutIntent::MoveCursors { motion, count, extend, .. },
                    RelativeKind::Move { motion: m, count: c, extend: e },
                ) => *motion == m && *count == c && *extend == e,
                _ => false,
            };
            if !matches {
                return fail(request, WrapError::Invalid("relative request differs from FIFO head"));
            }
            Some(head.id)
        } else {
            None
        };
        if relative_id.is_some()
            && (state.processing
                || state.dispatch.is_some()
                || state.ready.is_some()
                || state.relative_accepted.is_some())
        {
            state.relative_admission_waiting = true;
            return fail(request, WrapError::Busy);
        }
        if relative_id.is_none()
            && state.desired.is_some()
            && (0..RETIREMENT_SLOTS).all(|i| state.retired[i].is_some() || state.retirement_reserved[i])
        {
            return fail(request, WrapError::Busy);
        }
        let counter =
            if relative_id.is_some() { &self.shared.relative_generation } else { &self.shared.desired_generation };
        let Some(generation) = counter.load(Ordering::Acquire).checked_add(1) else {
            return fail(request, WrapError::Admission("request generation exhausted"));
        };
        if relative_id.is_none()
            && let Some(old) = state.desired.take()
        {
            let index = (0..RETIREMENT_SLOTS)
                .find(|&i| state.retired[i].is_none() && !state.retirement_reserved[i])
                .expect("checked retirement slot");
            state.retired[index] = Some(RetiredOwner::Request(old.request));
        }
        counter.store(generation, Ordering::Release);
        let job = Job { request, relative_id, generation };
        if relative_id.is_some() {
            state.relative_admission_waiting = false;
            state.relative_accepted = relative_id.map(|id| AcceptedRelative {
                id,
                fence: job.request.fence,
                generation,
                delivered_success: false,
            });
            state.dispatch = Some(job);
        } else {
            state.desired = Some(job);
        }
        drop(state);
        self.shared.changed.notify_one();
        ControlFlow::Continue(())
    }
    /// Stale ready owners retire on the fixed worker. Saturation retains the
    /// ready slot intact, instead of dropping its last source on the UI.
    pub fn take_update(&mut self, current: NavigationFence) -> Option<LayoutUpdate> {
        self.consume_update(Some(current))
    }
    /// Route the one ready slot by lane, using CURRENT registered fences supplied
    /// by the host. Never use the result's own fence as authorization. In
    /// particular, a pending relative head must not make a valid desired result
    /// appear stale merely because the host guessed the wrong ready-slot lane.
    pub fn take_registered_update(
        &mut self,
        current_relative: Option<NavigationFence>,
        current_desired: Option<NavigationFence>,
    ) -> Option<LayoutUpdate> {
        let _ = self.terminal_failure();
        let expected = {
            let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.ready.as_ref()?.relative_id.is_some() { current_relative } else { current_desired }
        };
        self.consume_update(expected)
    }
    /// Atomic host intake. A stale ready owner retires FIRST; a valid owner
    /// leaves the mailbox only together with its source-retirement permit and
    /// publication permit, in that order. Busy keeps the exact ready owner and
    /// asks the fixed worker for one capacity wake when admission can proceed.
    pub fn take_registered_update_with_retirement(
        &mut self,
        current_relative: Option<NavigationFence>,
        current_desired: Option<NavigationFence>,
    ) -> WrapResult<Option<(LayoutUpdate, RetirementPermit, RetirementPermit)>> {
        let _ = self.terminal_failure();
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.terminal_failure {
            if let Some(head) = self.relative.head() {
                let _ = self.relative.mark_failed(head.id);
            }
            return Err(WrapError::WorkerPanicked);
        }
        if state.admission_closed || self.shared.stop.load(Ordering::Acquire) {
            return Err(WrapError::Closed);
        }
        let Some(ready) = state.ready.as_ref() else { return Ok(None) };
        let current = if ready.relative_id.is_some() { current_relative } else { current_desired };
        let generation = if ready.relative_id.is_some() {
            self.shared.relative_generation.load(Ordering::Acquire)
        } else {
            self.shared.desired_generation.load(Ordering::Acquire)
        };
        if Some(ready.fence) != current || ready.generation != generation {
            let Some(index) =
                (0..RETIREMENT_SLOTS).find(|&i| state.retired[i].is_none() && !state.retirement_reserved[i])
            else {
                state.intake_waiting_slots = 1;
                return Err(WrapError::Busy);
            };
            state.intake_waiting_slots = 0;
            state.retired[index] = state.ready.take().map(RetiredOwner::Update);
            drop(state);
            self.shared.changed.notify_one();
            return Ok(None);
        }
        let mut free = [0; RETIREMENT_SLOTS];
        let mut count = 0;
        for index in 0..RETIREMENT_SLOTS {
            if state.retired[index].is_none() && !state.retirement_reserved[index] {
                free[count] = index;
                count += 1;
            }
        }
        if count < RETIREMENT_SLOTS {
            state.intake_waiting_slots = RETIREMENT_SLOTS;
            return Err(WrapError::Busy);
        }
        state.intake_waiting_slots = 0;
        for &index in &free {
            state.retirement_reserved[index] = true;
        }
        let update = state.ready.take().expect("validated ready owner");
        record_delivery(&mut self.relative, &mut state, &update);
        let source = RetirementPermit { shared: self.shared.clone(), index: free[0], active: true };
        let publication = RetirementPermit { shared: self.shared.clone(), index: free[1], active: true };
        drop(state);
        self.shared.changed.notify_one();
        Ok(Some((update, source, publication)))
    }
    fn consume_update(&mut self, current: Option<NavigationFence>) -> Option<LayoutUpdate> {
        let _ = self.terminal_failure();
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.terminal_failure {
            if let Some(head) = self.relative.head() {
                let _ = self.relative.mark_failed(head.id);
            }
            return None;
        }
        let ready = state.ready.as_ref()?;
        let generation = if ready.relative_id.is_some() {
            self.shared.relative_generation.load(Ordering::Acquire)
        } else {
            self.shared.desired_generation.load(Ordering::Acquire)
        };
        if Some(ready.fence) != current || ready.generation != generation || self.shared.stop.load(Ordering::Acquire) {
            if let Some(index) =
                (0..RETIREMENT_SLOTS).find(|&i| state.retired[i].is_none() && !state.retirement_reserved[i])
            {
                state.retired[index] = state.ready.take().map(RetiredOwner::Update);
                state.intake_waiting_slots = 0;
                drop(state);
                self.shared.changed.notify_one();
            }
            return None;
        }
        let result = state.ready.take();
        state.intake_waiting_slots = 0;
        if let Some(update) = &result {
            record_delivery(&mut self.relative, &mut state, update);
        }
        drop(state);
        self.shared.changed.notify_one();
        result
    }
    pub fn take_progress(&self, current: NavigationFence) -> Option<LayoutProgress> {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.progress.take().filter(|progress| progress.fence == current)
    }
    /// Continue transfers the exact owner; Break returns it without allocation.
    pub fn retire_owner(&self, owner: RetiredOwner) -> ControlFlow<RetiredOwner, ()> {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.admission_closed || state.terminal_failure {
            return ControlFlow::Break(owner);
        }
        let Some(index) = (0..RETIREMENT_SLOTS).find(|&i| state.retired[i].is_none() && !state.retirement_reserved[i])
        else {
            return ControlFlow::Break(owner);
        };
        state.retired[index] = Some(owner);
        drop(state);
        self.shared.changed.notify_one();
        ControlFlow::Continue(())
    }
    pub fn reserve_retirement(&self) -> WrapResult<RetirementPermit> {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.terminal_failure {
            return Err(WrapError::WorkerPanicked);
        }
        if state.admission_closed {
            return Err(WrapError::Closed);
        }
        let index = (0..RETIREMENT_SLOTS)
            .find(|&i| state.retired[i].is_none() && !state.retirement_reserved[i])
            .ok_or(WrapError::Busy)?;
        state.retirement_reserved[index] = true;
        Ok(RetirementPermit { shared: self.shared.clone(), index, active: true })
    }
    pub fn retire_source(&self, source: Arc<dyn WrapSource>) -> Result<(), Arc<dyn WrapSource>> {
        match self.retire_owner(RetiredOwner::Source(source)) {
            ControlFlow::Continue(()) => Ok(()),
            ControlFlow::Break(RetiredOwner::Source(source)) => Err(source),
            ControlFlow::Break(_) => unreachable!(),
        }
    }
    pub fn cancel(&mut self, reason: CancelReason) -> CancelledInputs {
        let relative =
            self.shared.relative_generation.try_update(Ordering::AcqRel, Ordering::Acquire, |g| g.checked_add(1));
        let desired =
            self.shared.desired_generation.try_update(Ordering::AcqRel, Ordering::Acquire, |g| g.checked_add(1));
        {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.relative_accepted = None;
            state.relative_admission_waiting = false;
            if relative.is_err() || desired.is_err() {
                state.admission_closed = true;
                state.terminal_failure = true;
                self.shared.stop.store(true, Ordering::Release);
            }
        }
        self.shared.changed.notify_one();
        self.relative.cancel(reason)
    }
    /// Call only AFTER successful editor/page publication, never on mere worker
    /// completion. Desired-state updates do not acknowledge a relative head.
    pub fn published_relative(&mut self, id: u64, request_fence: NavigationFence) -> Result<(), QueueError> {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if self.shared.stop.load(Ordering::Acquire)
            || state.terminal_failure
            || state.relative_accepted.as_ref().is_none_or(|accepted| {
                accepted.id != id
                    || accepted.fence != request_fence
                    || !accepted.delivered_success
                    || accepted.generation != self.shared.relative_generation.load(Ordering::Acquire)
            })
        {
            return Err(QueueError::WrongHead);
        }
        self.relative.published(id)?;
        state.relative_accepted = None;
        Ok(())
    }
    pub fn retry_relative(&mut self, id: u64) -> WrapResult<()> {
        if let Some(error) = self.terminal_failure() {
            return Err(error);
        }
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.terminal_failure {
            if let Some(head) = self.relative.head() {
                let _ = self.relative.mark_failed(head.id);
            }
            return Err(WrapError::WorkerPanicked);
        }
        if state.processing || state.ready.is_some() || state.dispatch.is_some() {
            return Err(WrapError::Busy);
        }
        if self.relative.head().is_none_or(|head| head.id != id) {
            return Err(WrapError::Stale);
        }
        state.relative_accepted = None;
        self.relative.retry();
        Ok(())
    }
    fn signal_stop(&self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.state.lock().unwrap_or_else(|e| e.into_inner()).admission_closed = true;
        self.shared.changed.notify_one();
    }
    #[must_use = "retain and settle this owner on the existing off-UI shutdown lane"]
    pub fn stop(mut self) -> SettlementTicket {
        self.cancel(CancelReason::Shutdown);
        self.signal_stop();
        SettlementTicket {
            shared: self.shared.clone(),
            worker: self.worker.take(),
            acknowledged: false,
            sticky_failure: false,
        }
    }
}
impl Drop for WrapSession {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            self.signal_stop();
            // Preserve the unacknowledged join/source owner instead of
            // implicitly detaching a JoinHandle or destroying a failed source
            // holder on the UI. Normal callers consume stop().settle().
            drop(SettlementTicket {
                shared: self.shared.clone(),
                worker: Some(worker),
                acknowledged: false,
                sticky_failure: false,
            });
        }
        // This cancellation fallback is NOT successful settlement. The host
        // contract requires stop() and retention of its acknowledgement ticket.
    }
}

fn worker_loop(shared: &Arc<Shared>, budget: AllocationBudget) -> WrapResult<()> {
    let mut engine = LayoutEngine::new(budget.clone())?;
    loop {
        if drain_retired(shared) {
            if shared.state.lock().unwrap_or_else(|e| e.into_inner()).terminal_failure {
                return Err(WrapError::WorkerPanicked);
            }
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            let _ = intake_capacity_ready(&mut state);
            let _ = relative_admission_ready(&mut state);
            drop(state);
            (shared.wake)();
            continue;
        }
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if shared.stop.load(Ordering::Acquire) {
            if state.retirement_reserved.iter().any(|reserved| *reserved) {
                drop(shared.changed.wait(state).unwrap_or_else(|e| e.into_inner()));
                continue;
            }
            return Ok(());
        }
        if intake_capacity_ready(&mut state) || relative_admission_ready(&mut state) {
            drop(state);
            (shared.wake)();
            continue;
        }
        if state.ready.is_some() || state.dispatch.is_none() && state.desired.is_none() {
            drop(shared.changed.wait(state).unwrap_or_else(|e| e.into_inner()));
            continue;
        }
        let job = state.dispatch.take().or_else(|| state.desired.take()).expect("pending job");
        state.processing = true;
        drop(state);
        let relative = job.relative_id.is_some();
        let WrapRequest { fence, source, anchor, intent } = job.request;
        let mut control = WorkerControl {
            shared,
            relative,
            generation: job.generation,
            fence,
            target: source.len(),
            last_progress: Cell::new(Instant::now()),
        };
        let navigation = matches!(&intent, LayoutIntent::MoveCursors { .. });
        let request = match intent {
            LayoutIntent::Fill => TaskRequest::Fill { key: fence.key, anchor },
            LayoutIntent::Reveal(point) => TaskRequest::Fill {
                key: fence.key,
                anchor: TopAnchor { source_byte: point.byte, affinity: point.affinity, intra_row_offset: 0 },
            },
            LayoutIntent::ScrollRows(delta) => TaskRequest::Scroll { key: fence.key, anchor, delta },
            LayoutIntent::MoveCursors { cursors, motion, count, extend } => {
                TaskRequest::Navigate { fence, expected: cursors, command: NavigationCommand { motion, count, extend } }
            }
        };
        let WorkerExit { engine: recovered, source, request, output, end } =
            source.run_layout(engine, request, &mut control);
        engine = recovered;
        let result = match (end, output) {
            (WorkerEnd::Refused(error), output) => {
                drop(output);
                Err(error)
            }
            (WorkerEnd::Finished(TaskEnd::Cancelled), output) => {
                drop(output);
                Err(WrapError::Cancelled)
            }
            (WorkerEnd::Finished(TaskEnd::Failed(error)), output) => {
                drop(output);
                Err(error.into())
            }
            (WorkerEnd::Finished(TaskEnd::Ready), output) => match output {
                Some(output) if navigation => match output.into_navigation() {
                    ControlFlow::Continue(value) => boxed_transfer(&budget, value).map(LayoutPayload::Navigation),
                    ControlFlow::Break(owner) => {
                        drop(owner);
                        Err(WrapError::Invalid("task returned page for navigation"))
                    }
                },
                Some(output) => match output.into_page() {
                    ControlFlow::Continue(value) => boxed_transfer(&budget, value).map(LayoutPayload::Page),
                    ControlFlow::Break(owner) => {
                        drop(owner);
                        Err(WrapError::Invalid("task returned navigation for page"))
                    }
                },
                None => Err(WrapError::Invalid("ready task without output")),
            },
        };
        // Unconsumed request/cursor ownership is destroyed HERE on the worker.
        // processing remains true through this and cancelled update destruction.
        drop(request);
        let cancelled = control.cancelled();
        let update = LayoutUpdate { fence, relative_id: job.relative_id, result, source, generation: job.generation };
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if cancelled || shared.stop.load(Ordering::Acquire) {
            drop(state);
            drop(update);
            // Keep processing true until the canceled owner's destruction has
            // finished. A Busy FIFO head needs an event when that capacity
            // becomes available, even though there is no result to publish.
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.processing = false;
            let notify = !shared.stop.load(Ordering::Acquire) && relative_admission_ready(&mut state);
            drop(state);
            if notify {
                (shared.wake)();
            }
        } else {
            state.processing = false;
            state.ready = Some(update);
            drop(state);
            (shared.wake)();
        }
    }
}
fn relative_admission_ready(state: &mut State) -> bool {
    if !state.relative_admission_waiting
        || state.admission_closed
        || state.terminal_failure
        || state.processing
        || state.dispatch.is_some()
        || state.ready.is_some()
        || state.relative_accepted.is_some()
    {
        return false;
    }
    state.relative_admission_waiting = false;
    true
}
fn record_delivery(relative: &mut RelativeQueue, state: &mut State, update: &LayoutUpdate) {
    if let Some(id) = update.relative_id {
        if let Some(accepted) =
            state.relative_accepted.as_mut().filter(|accepted| accepted.id == id && accepted.fence == update.fence)
        {
            accepted.delivered_success = update.result.is_ok();
        }
        if update.result.is_err() {
            let _ = relative.mark_failed(id);
        }
    }
}
fn intake_capacity_ready(state: &mut State) -> bool {
    if state.ready.is_none() {
        state.intake_waiting_slots = 0;
        return false;
    }
    if state.intake_waiting_slots == 0 {
        return false;
    }
    let free = (0..RETIREMENT_SLOTS).filter(|&i| state.retired[i].is_none() && !state.retirement_reserved[i]).count();
    if free < state.intake_waiting_slots {
        return false;
    }
    state.intake_waiting_slots = 0;
    true
}
fn notify_terminal_failure(shared: &Shared) {
    let notify = {
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.terminal_failure || state.terminal_notified {
            false
        } else {
            state.terminal_notified = true;
            state.admission_closed = true;
            state.processing = false;
            state.intake_waiting_slots = 0;
            state.relative_admission_waiting = false;
            shared.stop.store(true, Ordering::Release);
            true
        }
    };
    if notify {
        // A faulty callback must not bypass exact-owner cleanup/acknowledgement.
        // There is one terminal notification attempt and no idle retry loop.
        let _ = catch_unwind(AssertUnwindSafe(|| (shared.wake)()));
        shared.changed.notify_all();
    }
}
fn boxed_transfer<T>(budget: &AllocationBudget, value: T) -> WrapResult<Box<T>> {
    // The value already retains its header charge. Admit the temporary second
    // header while moving it from the worker stack into its result allocation.
    let _transfer = budget.reserve(size_of::<T>())?;
    Ok(Box::new(value))
}

fn drain_retired(shared: &Arc<Shared>) -> bool {
    let owner = {
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let Some(index) = state.retired.iter().position(Option::is_some) else { return false };
        state.retirement_reserved[index] = true;
        (index, state.retired[index].take().expect("occupied retirement slot"))
    };
    let failed = catch_unwind(AssertUnwindSafe(|| drop(owner.1))).is_err();
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    state.retirement_reserved[owner.0] = false;
    state.terminal_failure |= failed;
    drop(state);
    shared.changed.notify_all();
    true
}

#[must_use = "settlement failure must retain this exact worker/source owner for retry"]
pub struct SettlementTicket {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    acknowledged: bool,
    sticky_failure: bool,
}
pub struct SettlementFailure {
    pub ticket: SettlementTicket,
    pub error: WrapError,
}
impl fmt::Debug for SettlementFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SettlementFailure").field("error", &self.error).finish_non_exhaustive()
    }
}
impl SettlementTicket {
    /// OFF UI only. Timeout returns every owner unchanged. A terminal worker
    /// panic remains failed on subsequent attempts; it is not timeout success.
    pub fn settle(mut self, timeout: Duration) -> Result<(), SettlementFailure> {
        let started = Instant::now();
        if self.sticky_failure {
            return Err(SettlementFailure { ticket: self, error: WrapError::WorkerPanicked });
        }
        if !self.acknowledged {
            let shared = self.shared.clone();
            let state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            let (state, _) = shared
                .changed
                .wait_timeout_while(state, timeout.saturating_sub(started.elapsed()), |_| {
                    !shared.acknowledged.load(Ordering::Acquire)
                })
                .unwrap_or_else(|e| e.into_inner());
            self.acknowledged = shared.acknowledged.load(Ordering::Acquire);
            drop(state);
            if !self.acknowledged {
                return Err(SettlementFailure { ticket: self, error: WrapError::TimedOut });
            }
        }
        while self.worker.as_ref().is_some_and(|worker| !worker.is_finished()) {
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(SettlementFailure { ticket: self, error: WrapError::TimedOut });
            }
            #[cfg(test)]
            if let Some(waiting) = self.shared.settlement_waiting.lock().unwrap().take() {
                let _ = waiting.try_send(());
            }
            // Rust has no portable timed JoinHandle::join or thread-exit
            // notification. Only an active off-UI settle passively waits here;
            // the single fixed worker and UI have no timer/polling wakeups.
            // The SAME monotonic budget covers acknowledgement and actual exit,
            // including retries which have already observed acknowledgement.
            let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                drop(state);
                return Err(SettlementFailure { ticket: self, error: WrapError::TimedOut });
            }
            drop(
                self.shared
                    .changed
                    .wait_timeout(state, remaining.min(Duration::from_millis(1)))
                    .unwrap_or_else(|e| e.into_inner()),
            );
        }
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            self.sticky_failure = true;
        }
        self.sticky_failure |= self.shared.state.lock().unwrap_or_else(|e| e.into_inner()).terminal_failure;
        if self.sticky_failure {
            Err(SettlementFailure { ticket: self, error: WrapError::WorkerPanicked })
        } else {
            Ok(())
        }
    }
    pub fn retained_bytes(&self) -> usize {
        self.shared._charge.retained_bytes()
    }
}
impl Drop for SettlementTicket {
    fn drop(&mut self) {
        if self.worker.is_some() || self.sticky_failure {
            // Deliberate last-resort retention, analogous to the document
            // depot's undrained-owner rule. This is NOT settlement success or
            // permission to start a replacement worker. A failed settle returns
            // this exact ticket so the host can retain/retry it normally.
            if let Some(worker) = self.worker.take() {
                std::mem::forget(worker);
            }
            std::mem::forget(self.shared.clone());
        }
    }
}

/// A reserved slot, not a promise to try admission after mutating the editor.
/// `publish` checks closed admission while holding the same lock that protects
/// the reservation, then runs only the caller's infallible state replacement.
pub struct RetirementPermit {
    shared: Arc<Shared>,
    index: usize,
    active: bool,
}
impl fmt::Debug for RetirementPermit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RetirementPermit").field("active", &self.active).finish()
    }
}
impl RetirementPermit {
    pub fn is_current(&self) -> bool {
        let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        self.active
            && !state.admission_closed
            && !state.terminal_failure
            && state.retirement_reserved[self.index]
            && state.retired[self.index].is_none()
    }
    pub fn publish<T>(mut self, publish: impl FnOnce() -> (T, Option<RetiredOwner>)) -> Result<T, Self> {
        let shared = self.shared.clone();
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if !self.active
            || state.admission_closed
            || state.terminal_failure
            || !state.retirement_reserved[self.index]
            || state.retired[self.index].is_some()
        {
            drop(state);
            return Err(self);
        }
        let (result, owner) = publish();
        state.retired[self.index] = owner;
        state.retirement_reserved[self.index] = false;
        self.active = false;
        drop(state);
        shared.changed.notify_one();
        Ok(result)
    }
}
impl Drop for RetirementPermit {
    fn drop(&mut self) {
        if self.active {
            self.shared.state.lock().unwrap_or_else(|e| e.into_inner()).retirement_reserved[self.index] = false;
            self.shared.changed.notify_one();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kwil_core_text::Buffer;

    #[test]
    fn wrap_initialization_failure_notifies_once_and_remains_explicitly_terminal() {
        let metadata = AllocationBudget::new(0).retained_bytes();
        // Enough for fixed session ownership, deliberately not enough for the
        // real LayoutEngine scratch admission in the newly started worker.
        let budget = AllocationBudget::new(metadata + WrapSession::fixed_allocation_bytes().unwrap());
        let (wake, ready) = std::sync::mpsc::sync_channel(2);
        let mut session = WrapSession::with_budget(budget, move || {
            let _ = wake.try_send(());
        })
        .unwrap();
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(session.terminal_failure(), Some(WrapError::WorkerPanicked)));
        assert!(matches!(session.terminal_failure(), Some(WrapError::WorkerPanicked)));
        assert!(session.take_registered_update(None, None).is_none());
        assert!(matches!(ready.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)));
        let (key, source) = source();
        let request = WrapRequest {
            fence: NavigationFence { key, cursor_set_generation: 1, intent_generation: 1 },
            source: source.clone(),
            anchor: TopAnchor::default(),
            intent: LayoutIntent::Fill,
        };
        let refused = match session.submit(request) {
            ControlFlow::Break(refused) => refused,
            ControlFlow::Continue(()) => panic!("terminal worker accepted a new owner"),
        };
        assert!(matches!(refused.error, WrapError::WorkerPanicked));
        assert!(Arc::ptr_eq(&refused.request.source, &source));
        drop(refused);
        let failure = session.stop().settle(Duration::from_secs(2)).unwrap_err();
        assert!(matches!(failure.error, WrapError::WorkerPanicked));
        let identity = Arc::as_ptr(&failure.ticket.shared);
        let again = failure.ticket.settle(Duration::ZERO).unwrap_err();
        assert!(matches!(again.error, WrapError::WorkerPanicked));
        assert_eq!(Arc::as_ptr(&again.ticket.shared), identity);
        assert!(matches!(ready.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)));
        std::mem::forget(again);
    }

    pub(super) struct CompletionGate {
        entered: std::sync::mpsc::SyncSender<()>,
        open: Mutex<bool>,
        changed: Condvar,
    }
    impl CompletionGate {
        pub(super) fn wait(&self) {
            let _ = self.entered.try_send(());
            let open = self.open.lock().unwrap();
            let (open, _) = self.changed.wait_timeout_while(open, Duration::from_secs(2), |open| !*open).unwrap();
            assert!(*open, "bounded post-ack test gate timed out");
        }
        fn release(&self) {
            *self.open.lock().unwrap() = true;
            self.changed.notify_all();
        }
    }

    #[test]
    fn wrap_settlement_single_deadline_waits_for_actual_exit_and_retries_exact_ticket() {
        let session = WrapSession::new(|| {}).unwrap();
        let (entered, reached) = std::sync::mpsc::sync_channel(1);
        let gate = Arc::new(CompletionGate { entered, open: Mutex::new(false), changed: Condvar::new() });
        *session.shared.completion_gate.lock().unwrap() = Some(gate.clone());
        let ticket = session.stop();
        reached.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(ticket.shared.acknowledged.load(Ordering::Acquire));
        assert!(!ticket.worker.as_ref().unwrap().is_finished());
        let identity = Arc::as_ptr(&ticket.shared);
        let worker = ticket.worker.as_ref().unwrap().thread().id();
        let interval = Duration::from_millis(5);
        let started = Instant::now();
        let first = ticket.settle(interval).unwrap_err();
        assert!(matches!(first.error, WrapError::TimedOut));
        assert!(started.elapsed() >= interval, "ack is not actual completion and must not cause an early timeout");
        assert_eq!(Arc::as_ptr(&first.ticket.shared), identity);
        assert_eq!(first.ticket.worker.as_ref().unwrap().thread().id(), worker);
        let started = Instant::now();
        let second = first.ticket.settle(interval).unwrap_err();
        assert!(matches!(second.error, WrapError::TimedOut));
        assert!(
            started.elapsed() >= interval,
            "retry with acknowledgement already observed still gets its full deadline"
        );
        assert_eq!(Arc::as_ptr(&second.ticket.shared), identity);
        assert_eq!(second.ticket.worker.as_ref().unwrap().thread().id(), worker);
        let (waiting, passive) = std::sync::mpsc::sync_channel(1);
        *second.ticket.shared.settlement_waiting.lock().unwrap() = Some(waiting);
        thread::scope(|scope| {
            // A controlled test caller, never a production replacement worker.
            let result = scope.spawn(move || second.ticket.settle(Duration::from_secs(2)));
            passive.recv_timeout(Duration::from_secs(2)).unwrap();
            gate.release();
            result.join().unwrap().unwrap();
        });
    }

    #[test]
    fn wrap_generation_exhaustion_refuses_newest_and_preserves_exact_request_and_head() {
        let (key, source) = source();
        let mut session = WrapSession::new(|| {}).unwrap();
        session.relative.next_id = u64::MAX;
        let input = RelativeInput { key, kind: RelativeKind::Scroll(1) };
        assert_eq!(session.relative.enqueue(input), Err(QueueError::CounterExhausted));
        assert!(session.relative.is_empty());
        session.shared.desired_generation.store(u64::MAX, Ordering::Release);
        let request = WrapRequest {
            fence: NavigationFence { key, cursor_set_generation: 1, intent_generation: 1 },
            source: source.clone(),
            anchor: TopAnchor::default(),
            intent: LayoutIntent::Fill,
        };
        let refused = match session.submit(request) {
            ControlFlow::Break(refused) => refused,
            ControlFlow::Continue(()) => panic!("exhausted generation accepted"),
        };
        assert!(matches!(refused.error, WrapError::Admission("request generation exhausted")));
        assert!(Arc::ptr_eq(&refused.request.source, &source));
        drop(refused);
        session.relative.next_id = 1;
        let id = session.relative.enqueue(input).unwrap();
        session.relative.begin(key).unwrap();
        session.shared.relative_generation.store(u64::MAX, Ordering::Release);
        let request = WrapRequest {
            fence: NavigationFence { key, cursor_set_generation: 1, intent_generation: 2 },
            source: source.clone(),
            anchor: TopAnchor::default(),
            intent: LayoutIntent::ScrollRows(1),
        };
        let refused = match session.submit(request) {
            ControlFlow::Break(refused) => refused,
            ControlFlow::Continue(()) => panic!("exhausted relative generation accepted"),
        };
        assert!(matches!(refused.error, WrapError::Admission("request generation exhausted")));
        assert!(Arc::ptr_eq(&refused.request.source, &source));
        assert_eq!(session.relative.head().unwrap().id, id);
        assert_eq!(session.relative.len(), 1);
        assert_eq!(session.relative.mutation_gate(), MutationGate::RefusedPending);
        drop(refused);
        let failure = session.stop().settle(Duration::from_secs(2)).unwrap_err();
        assert!(
            matches!(failure.error, WrapError::WorkerPanicked),
            "exhausted cancellation is a sticky terminal failure"
        );
        let identity = Arc::as_ptr(&failure.ticket.shared);
        let next = failure.ticket.settle(Duration::ZERO).unwrap_err();
        assert!(matches!(next.error, WrapError::WorkerPanicked));
        assert_eq!(Arc::as_ptr(&next.ticket.shared), identity);
        std::mem::forget(next);
    }

    fn source() -> (LayoutKey, Arc<dyn WrapSource>) {
        let key = LayoutKey {
            geometry: LayoutGeometry { columns: 1, rows: 1, tab_size: 4, ..LayoutGeometry::default() },
            ..LayoutKey::default()
        };
        let mut buffer = Buffer::empty();
        buffer.insert(0, b"x").unwrap();
        (key, Arc::new(SnapshotSource::new(key.source, buffer.snapshot())))
    }
    #[test]
    fn wrap_closed_admission_returns_exact_request_and_retirement_owner() {
        let (key, source) = source();
        let session = WrapSession::new(|| {}).unwrap();
        session.signal_stop();
        let request = WrapRequest {
            fence: NavigationFence { key, cursor_set_generation: 1, intent_generation: 1 },
            source: source.clone(),
            anchor: TopAnchor::default(),
            intent: LayoutIntent::Fill,
        };
        let refused = match session.submit(request) {
            ControlFlow::Break(refused) => refused,
            ControlFlow::Continue(()) => panic!("closed submission accepted"),
        };
        assert!(matches!(refused.error, WrapError::Closed));
        assert!(Arc::ptr_eq(&refused.request.source, &source));
        let owner = match session.retire_owner(RetiredOwner::Source(source.clone())) {
            ControlFlow::Break(owner) => owner,
            ControlFlow::Continue(()) => panic!("closed retirement accepted"),
        };
        match owner {
            RetiredOwner::Source(returned) => assert!(Arc::ptr_eq(&returned, &source)),
            _ => panic!("retirement owner changed"),
        }
        let ticket = session.stop();
        ticket.settle(Duration::from_secs(2)).unwrap();
    }
    #[test]
    fn wrap_terminal_worker_failure_retains_same_ticket_and_never_becomes_success() {
        let (key, source) = source();
        let (wake_tx, wake_rx) = std::sync::mpsc::sync_channel(1);
        let session = WrapSession::new(move || {
            let _ = wake_tx.try_send(());
            panic!("controlled wake failure");
        })
        .unwrap();
        let request = WrapRequest {
            fence: NavigationFence { key, cursor_set_generation: 1, intent_generation: 1 },
            source,
            anchor: TopAnchor::default(),
            intent: LayoutIntent::Fill,
        };
        assert!(matches!(session.submit(request), ControlFlow::Continue(())));
        wake_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let first = match session.stop().settle(Duration::from_secs(2)) {
            Err(failure) => failure,
            Ok(()) => panic!("worker failure became success"),
        };
        assert!(matches!(first.error, WrapError::WorkerPanicked));
        let identity = Arc::as_ptr(&first.ticket.shared);
        let next = first.ticket.settle(Duration::ZERO).unwrap_err();
        assert!(matches!(next.error, WrapError::WorkerPanicked));
        assert_eq!(Arc::as_ptr(&next.ticket.shared), identity);
        // The deliberately failed owner remains retained, not acknowledged or
        // detached-as-success. This tiny fake host owns no filesystem artifact.
        std::mem::forget(next);
    }

    struct ProbedFixture {
        inner: SnapshotSource,
        probe: Arc<crate::visual_rows::fixtures::Probe>,
        ui: thread::ThreadId,
    }
    impl WrapSource for ProbedFixture {
        fn identity(&self) -> SourceIdentity {
            self.inner.identity()
        }
        fn len(&self) -> u64 {
            self.inner.len()
        }
        fn line_count(&self) -> u64 {
            self.inner.line_count()
        }
        fn run_layout(
            self: Arc<Self>,
            engine: LayoutEngine,
            request: TaskRequest,
            driver: &mut dyn TaskDriver,
        ) -> WorkerExit {
            let probe = self.probe.clone(); // instrumentation, never a source Arc
            crate::visual_rows::fixtures::run_snapshot(self, Some(probe), engine, request, driver)
        }
        fn line_start(&self, line: u64) -> std::io::Result<Option<u64>> {
            assert_ne!(thread::current().id(), self.ui);
            self.inner.line_start(line)
        }
        fn line_of(&self, byte: u64) -> std::io::Result<u64> {
            assert_ne!(thread::current().id(), self.ui);
            self.inner.line_of(byte)
        }
        fn read(&self, at: u64, out: &mut [u8]) -> std::io::Result<usize> {
            assert_ne!(thread::current().id(), self.ui);
            self.inner.read(at, out)
        }
    }

    impl crate::snapshot_layout_source::SnapshotOwner for ProbedFixture {
        fn snapshot(&self) -> &kwil_core_text::Snapshot {
            crate::snapshot_layout_source::SnapshotOwner::snapshot(&self.inner)
        }
    }
    impl crate::visual_rows::fixtures::SnapshotHooks for ProbedFixture {
        fn before_snapshot_query(&self) {
            assert_ne!(thread::current().id(), self.ui);
        }
        fn before_snapshot_read(&self, _: usize) -> std::io::Result<()> {
            assert_ne!(thread::current().id(), self.ui);
            Ok(())
        }
    }

    #[test]
    fn wrap_all_intents_use_concrete_queries_resume_and_recover_one_arc_off_ui() {
        // Fail-first mutations: restore synchronous dispatch; recreate a query
        // on Pending; retain an Arc clone in run_cooperative; skip provenance;
        // or transfer query destruction to the receiving thread. Each violates
        // an assertion below or the bounded event deadline. NOT RUN here.
        for lane in 0..4 {
            let ui = thread::current().id();
            let key = LayoutKey {
                geometry: LayoutGeometry { columns: 3, rows: 2, tab_size: 4, ..LayoutGeometry::default() },
                ..LayoutKey::default()
            };
            let mut buffer = Buffer::empty();
            buffer.insert(0, b"abc def\nXYZ").unwrap();
            let probe = Arc::new(crate::visual_rows::fixtures::Probe::default());
            let source: Arc<dyn WrapSource> = Arc::new(ProbedFixture {
                inner: SnapshotSource::new(key.source, buffer.snapshot()),
                probe: probe.clone(),
                ui,
            });
            let pointer = Arc::as_ptr(&source);
            assert_eq!(Arc::strong_count(&source), 1);
            let (wake, ready) = std::sync::mpsc::sync_channel(2);
            let mut session = WrapSession::new(move || {
                let _ = wake.try_send(());
            })
            .unwrap();
            *session.shared.work_quantum.lock().unwrap() = ReadWork::new(1, 1);
            let fence = NavigationFence { key, cursor_set_generation: 1, intent_generation: 1 };
            let intent = match lane {
                0 => LayoutIntent::Fill,
                1 => LayoutIntent::Reveal(SourcePoint { byte: 4, affinity: CaretAffinity::Downstream }),
                2 => {
                    session.relative.enqueue(RelativeInput { key, kind: RelativeKind::Scroll(1) }).unwrap();
                    session.relative.begin(key).unwrap();
                    LayoutIntent::ScrollRows(1)
                }
                _ => {
                    let motion = VisualMotion::Down;
                    session
                        .relative
                        .enqueue(RelativeInput { key, kind: RelativeKind::Move { motion, count: 1, extend: false } })
                        .unwrap();
                    session.relative.begin(key).unwrap();
                    LayoutIntent::MoveCursors {
                        cursors: CursorSet::new(session.budget(), &[VisualCursor::default()], 0).unwrap(),
                        motion,
                        count: 1,
                        extend: false,
                    }
                }
            };
            assert!(matches!(
                session.submit(WrapRequest { fence, source, anchor: TopAnchor::default(), intent }),
                ControlFlow::Continue(())
            ));
            let deadline = Instant::now() + Duration::from_secs(2);
            let update = loop {
                ready.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
                if let Some(update) = session.take_update(fence) {
                    break update;
                }
                assert!(Instant::now() < deadline, "no matching cooperative result");
            };
            assert!(update.result.is_ok(), "cooperative fixture failed: {:?}", update.result);
            assert!(std::ptr::addr_eq(pointer, Arc::as_ptr(&update.source)));
            assert_eq!(Arc::strong_count(&update.source), 1);
            assert!(probe.created.load(Ordering::Relaxed) > 0, "synchronous dispatch bypassed query creation");
            assert!(
                probe.polls.load(Ordering::Relaxed) > probe.created.load(Ordering::Relaxed),
                "one-byte Pending was bypassed"
            );
            assert!(probe.provenance.load(Ordering::Relaxed) > 0, "provenance query bypassed");
            assert_eq!(probe.created.load(Ordering::Relaxed), probe.dropped.load(Ordering::Relaxed));
            assert_eq!(probe.max_owners.load(Ordering::Relaxed), 1, "source Arc cloned during task");
            assert_ne!(*probe.drop_thread.lock().unwrap(), Some(ui), "query destruction reached UI");
            assert!(!probe.mixed_drop_threads.load(Ordering::Relaxed));
            if let Some(id) = update.relative_id {
                session.published_relative(id, fence).unwrap();
            }
            assert!(matches!(session.retire_owner(RetiredOwner::Update(update)), ControlFlow::Continue(())));
            session.stop().settle(Duration::from_secs(2)).unwrap();
        }
    }
}
