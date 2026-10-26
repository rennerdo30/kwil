//! One admitted GUI notification lifetime. No native proxy escapes this module.
//! Native quiescence is not a source barrier, receipt, worker join or retirement.
use super::UserEvent;
use kwil_view_model::visual_rows::{AllocationBudget, MetadataReservation, WrapError, WrapResult};
use std::{
    any::Any,
    mem::{ManuallyDrop, size_of},
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, ThreadId},
    time::{Duration, Instant},
};
use winit::event_loop::{EventLoopClosed, EventLoopProxy};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    Admission,
    NativeClosed,
    NativePanicked,
    ExistingPanicCustodian,
    Identity,
    Exhausted,
    UnresolvedOwner,
    UnresolvedControl,
    ProducerPanicked,
    GpuError,
    ControlError,
    TimedOut,
    FailedVerdict,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Policy {
    Wake,
    ExternalOpen,
    Value,
    Intent,
    Readiness,
    Control,
    Owner,
    GpuError,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SlotKind {
    Registration,
    Gpu,
    FileDialog,
    Confirmation,
    GpuCallback,
}
impl SlotKind {
    fn index(self) -> usize {
        match self {
            Self::Registration => 0,
            Self::Gpu => 1,
            Self::FileDialog => 2,
            Self::Confirmation => 3,
            Self::GpuCallback => 4,
        }
    }
}

#[derive(Default)]
struct Slot {
    generation: u64,
    outstanding: bool,
    receipts: usize,
    no_owner_created: bool,
    charge: Option<MetadataReservation>,
}

struct ClosedFailure {
    original: EventLoopClosed<UserEvent>,
    _charge: MetadataReservation,
}
struct PanicFailure {
    original: Box<dyn Any + Send>,
    _charge: MetadataReservation,
}
struct GpuFailure {
    original: String,
    _charge: Option<MetadataReservation>,
}
struct State {
    requested: bool,
    released: bool,
    failed: Option<Reason>,
    native_blocked: Option<Reason>,
    active: usize,
    // Exactly the two active-owner and three control/capability families.
    slots: [Slot; 5],
    closed: Option<ClosedFailure>,
    panic: Option<PanicFailure>,
    gpu_error: Option<GpuFailure>,
    control_error: Option<GpuFailure>,
}

enum Target {
    Native(EventLoopProxy<UserEvent>),
    #[cfg(test)]
    Channel(std::sync::mpsc::Sender<UserEvent>),
}

pub(super) struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    target: Target,
    budget: AllocationBudget,
    _metadata: MetadataReservation,
}

#[derive(Clone)]
pub(super) enum HostProxy {
    Native(Arc<Shared>),
    // Preserve existing non-native dispatch fixtures without editing their files.
    #[cfg(test)]
    Test(std::sync::mpsc::Sender<UserEvent>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SendRefusal {
    OffUiWake,
    CancelledValue,
    CancelledIntent,
    CancelledReadiness,
    RefusedControl,
    Fatal(Reason),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WakeDisposition {
    NativeEnqueued,
    OffUiWake,
}

enum RouteStatus {
    Enqueued,
    OffUiWake,
    Fatal(Reason),
}
type Routed = std::ops::ControlFlow<(UserEvent, SendRefusal), RouteStatus>;
type NativeRoute = std::ops::ControlFlow<EventLoopClosed<UserEvent>, ()>;
struct Routing {
    outcome: Routed,
    // Lives through the producer-side disposal and typed return overlap.
    _charge: Option<MetadataReservation>,
}
impl Routing {
    fn new(outcome: Routed, charge: Option<MetadataReservation>) -> Self {
        Self { outcome, _charge: charge }
    }
}

struct NativeSendClaim {
    shared: Arc<Shared>,
    charge: Option<MetadataReservation>,
    finished: bool,
}
impl Drop for NativeSendClaim {
    fn drop(&mut self) {
        let mut state = lock(&self.shared.state);
        if !self.finished {
            state.failed.get_or_insert(Reason::NativePanicked);
            state.native_blocked.get_or_insert(Reason::NativePanicked);
        }
        state.active -= 1;
        self.shared.changed.notify_all();
    }
}

/// Allocation identity is retained; a proof cannot match a reused address.
pub(super) struct NativeQuiescenceProof {
    shared: Arc<Shared>,
    deadline: Instant,
    _charge: MetadataReservation,
}

pub struct Inspection<'a> {
    state: MutexGuard<'a, State>,
}
impl Inspection<'_> {
    pub fn reason(&self) -> Option<Reason> {
        self.state.failed
    }
    pub fn panic_payload(&self) -> Option<&(dyn Any + Send)> {
        self.state.panic.as_ref().map(|failure| failure.original.as_ref())
    }
    pub fn gpu_error(&self) -> Option<&str> {
        self.state.gpu_error.as_ref().map(|failure| failure.original.as_str())
    }
    pub fn control_error(&self) -> Option<&str> {
        self.state.control_error.as_ref().map(|failure| failure.original.as_str())
    }
    pub fn native_event(&self) -> Option<&'static str> {
        self.state.closed.as_ref().map(|failure| failure.original.0.notification_name())
    }
}
impl std::fmt::Debug for Inspection<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuiNotificationFailure")
            .field("reason", &self.reason())
            .field("native_event", &self.native_event())
            .field("gpu_error", &self.gpu_error())
            .field("control_error", &self.control_error())
            .field("original_panic_retained", &self.panic_payload().is_some())
            .finish()
    }
}

enum PanicCustody<'a> {
    Direct(&'a (dyn Any + Send)),
    Document(kwil_view_model::workspace_document::DocumentWakeInspection<'a>),
    Editor(super::document_ui::FontWakeInspection<'a, super::Font>),
    Chrome(super::document_ui::FontWakeInspection<'a, super::font::UiFont>),
    Gate(Inspection<'a>),
}
pub struct PanicInspection<'a>(PanicCustody<'a>);
impl<'a> PanicInspection<'a> {
    pub(super) fn direct(payload: &'a (dyn Any + Send)) -> Self {
        Self(PanicCustody::Direct(payload))
    }
    pub(super) fn document(
        inspection: kwil_view_model::workspace_document::DocumentWakeInspection<'a>,
    ) -> Option<Self> {
        if inspection.panic_payload().is_some() { Some(Self(PanicCustody::Document(inspection))) } else { None }
    }
    pub(super) fn editor(inspection: super::document_ui::FontWakeInspection<'a, super::Font>) -> Option<Self> {
        inspection.panic_payload().is_some().then_some(Self(PanicCustody::Editor(inspection)))
    }
    pub(super) fn chrome(inspection: super::document_ui::FontWakeInspection<'a, super::font::UiFont>) -> Option<Self> {
        inspection.panic_payload().is_some().then_some(Self(PanicCustody::Chrome(inspection)))
    }
    pub(super) fn gate(inspection: Inspection<'a>) -> Option<Self> {
        inspection.panic_payload().is_some().then_some(Self(PanicCustody::Gate(inspection)))
    }
    pub fn panic_payload(&self) -> &(dyn Any + Send) {
        match &self.0 {
            PanicCustody::Direct(payload) => *payload,
            PanicCustody::Document(inspection) => inspection.panic_payload().unwrap(),
            PanicCustody::Editor(inspection) => inspection.panic_payload().unwrap(),
            PanicCustody::Chrome(inspection) => inspection.panic_payload().unwrap(),
            PanicCustody::Gate(inspection) => inspection.panic_payload().unwrap(),
        }
    }
}

impl HostProxy {
    pub(super) fn admit(proxy: EventLoopProxy<UserEvent>, budget: &AllocationBudget) -> WrapResult<Self> {
        Self::admit_target(Target::Native(proxy), budget)
    }
    fn admit_target(target: Target, budget: &AllocationBudget) -> WrapResult<Self> {
        let layout = std::alloc::Layout::new::<(std::sync::atomic::AtomicUsize, std::sync::atomic::AtomicUsize)>()
            .extend(std::alloc::Layout::new::<Shared>())
            .map_err(|_| WrapError::Admission("GUI notification allocation layout"))?
            .0
            .pad_to_align();
        let bytes = layout
            .size()
            .checked_add(2 * size_of::<Self>())
            .and_then(|n| n.checked_add(2 * size_of::<Result<Self, WrapError>>()))
            .and_then(|n| n.checked_add(size_of::<Target>()))
            .ok_or(WrapError::Admission("GUI notification allocation overflow"))?;
        let metadata = budget.reserve_metadata(bytes)?;
        Ok(Self::Native(Arc::new(Shared {
            state: Mutex::new(State {
                requested: false,
                released: false,
                failed: None,
                native_blocked: None,
                active: 0,
                slots: std::array::from_fn(|_| Slot::default()),
                closed: None,
                panic: None,
                gpu_error: None,
                control_error: None,
            }),
            changed: Condvar::new(),
            target,
            budget: budget.clone(),
            _metadata: metadata,
        })))
    }
    #[cfg(test)]
    pub(super) fn admitted_test(
        sender: std::sync::mpsc::Sender<UserEvent>,
        budget: &AllocationBudget,
    ) -> WrapResult<Self> {
        Self::admit_target(Target::Channel(sender), budget)
    }
    fn shared(&self) -> Option<&Arc<Shared>> {
        match self {
            Self::Native(shared) => Some(shared),
            #[cfg(test)]
            Self::Test(_) => None,
        }
    }
    fn same_gate(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Native(first), Self::Native(second)) => Arc::ptr_eq(first, second),
            #[cfg(test)]
            (Self::Test(_), Self::Test(_)) => true, // legacy non-native dispatch fixtures only
            #[cfg(test)]
            _ => false,
        }
    }
    pub(super) fn inspect(&self) -> Option<Inspection<'_>> {
        self.shared().map(|shared| Inspection { state: lock(&shared.state) })
    }
    pub(super) fn fail(&self, reason: Reason) {
        if let Some(shared) = self.shared() {
            let mut state = lock(&shared.state);
            state.failed.get_or_insert(reason);
            shared.changed.notify_all();
        }
    }
    pub(super) fn request_terminal(&self) {
        if let Some(shared) = self.shared() {
            let mut state = lock(&shared.state);
            state.requested = true;
            shared.changed.notify_all();
        }
    }
    pub(super) fn operation_bytes() -> Option<usize> {
        size_of::<NativeSendClaim>()
            .checked_add(2 * size_of::<UserEvent>())?
            .checked_add(2 * size_of::<Routed>())?
            .checked_add(2 * size_of::<Routing>())?
            .checked_add(size_of::<NativeRoute>())?
            .checked_add(size_of::<Result<NativeRoute, Box<dyn Any + Send>>>())?
            .checked_add(2 * size_of::<MutexGuard<'_, State>>())?
            .checked_add(size_of::<(MutexGuard<'_, State>, std::sync::WaitTimeoutResult)>())?
            .checked_add(2 * size_of::<NativeQuiescenceProof>())?
            .checked_add(2 * size_of::<Result<NativeQuiescenceProof, Reason>>())?
            .checked_add(2 * size_of::<Instant>() + size_of::<Duration>())?
            .checked_add(size_of::<Arc<Shared>>() + 2 * size_of::<usize>())?
            .checked_add(2 * size_of::<HostProxy>())?
            .checked_add(size_of::<Policy>() + size_of::<SendRefusal>() + size_of::<Reason>())
            .and_then(|n| n.checked_add(2 * size_of::<Result<(), SendRefusal>>()))
            .and_then(|n| n.checked_add(2 * size_of::<Result<WakeDisposition, SendRefusal>>()))
    }
    fn route(&self, event: UserEvent, existing_panic_custodian: bool) -> Routing {
        #[cfg(test)]
        if let Self::Test(sender) = self {
            return Routing::new(
                match sender.send(event) {
                    Ok(()) => Routed::Continue(RouteStatus::Enqueued),
                    Err(error) => Routed::Break((error.0, SendRefusal::Fatal(Reason::NativeClosed))),
                },
                None,
            );
        }
        let shared = match self {
            Self::Native(shared) => shared,
            #[cfg(test)]
            Self::Test(_) => unreachable!(),
        };
        Self::route_with(shared, event, existing_panic_custodian, |event| {
            let result = match &shared.target {
                Target::Native(proxy) => proxy.send_event(event),
                #[cfg(test)]
                Target::Channel(sender) => sender.send(event).map_err(|error| EventLoopClosed(error.0)),
            };
            match result {
                Ok(()) => NativeRoute::Continue(()),
                Err(original) => NativeRoute::Break(original),
            }
        })
    }
    fn route_with<F: FnOnce(UserEvent) -> NativeRoute>(
        shared: &Arc<Shared>,
        event: UserEvent,
        existing_panic_custodian: bool,
        send: F,
    ) -> Routing {
        let Some(bytes) = Self::operation_bytes().and_then(|n| n.checked_add(2 * size_of::<F>())) else {
            lock(&shared.state).failed.get_or_insert(Reason::Admission);
            return Routing::new(Routed::Break((event, SendRefusal::Fatal(Reason::Admission))), None);
        };
        let charge = match shared.budget.reserve_metadata(bytes) {
            Ok(charge) => charge,
            Err(_) => {
                lock(&shared.state).failed.get_or_insert(Reason::Admission);
                return Routing::new(Routed::Break((event, SendRefusal::Fatal(Reason::Admission))), None);
            }
        };
        {
            let mut state = lock(&shared.state);
            if let Some(reason) = state.native_blocked {
                return Routing::new(Routed::Break((event, SendRefusal::Fatal(reason))), Some(charge));
            }
            if state.requested {
                return Routing::new(
                    match event.notification_policy() {
                        Policy::Wake => Routed::Continue(RouteStatus::OffUiWake),
                        Policy::Value => Routed::Break((event, SendRefusal::CancelledValue)),
                        Policy::Intent => Routed::Break((event, SendRefusal::CancelledIntent)),
                        Policy::Readiness => Routed::Break((event, SendRefusal::CancelledReadiness)),
                        Policy::ExternalOpen | Policy::Control | Policy::Owner | Policy::GpuError => {
                            Routed::Break((event, SendRefusal::RefusedControl))
                        }
                    },
                    Some(charge),
                );
            }
            let Some(active) = state.active.checked_add(1) else {
                state.failed.get_or_insert(Reason::Exhausted);
                return Routing::new(Routed::Break((event, SendRefusal::Fatal(Reason::Exhausted))), Some(charge));
            };
            state.active = active;
        }
        let mut claim = NativeSendClaim { shared: shared.clone(), charge: Some(charge), finished: false };
        let result = catch_unwind(AssertUnwindSafe(|| send(event)));
        let outcome = match result {
            Ok(NativeRoute::Continue(())) => {
                claim.finished = true;
                Routed::Continue(RouteStatus::Enqueued)
            }
            Ok(NativeRoute::Break(original)) => {
                let mut state = lock(&shared.state);
                state.failed.get_or_insert(Reason::NativeClosed);
                state.native_blocked.get_or_insert(Reason::NativeClosed);
                if state.closed.is_none() {
                    state.closed = Some(ClosedFailure { original, _charge: claim.charge.take().unwrap() });
                } else {
                    // Preserve later original failures without an unbounded error queue.
                    std::mem::forget((original, claim.charge.take()));
                }
                claim.finished = true;
                Routed::Continue(RouteStatus::Fatal(Reason::NativeClosed))
            }
            Err(original) => {
                let mut state = lock(&shared.state);
                state.failed.get_or_insert(if existing_panic_custodian {
                    Reason::ExistingPanicCustodian
                } else {
                    Reason::NativePanicked
                });
                state.native_blocked.get_or_insert(if existing_panic_custodian {
                    Reason::ExistingPanicCustodian
                } else {
                    Reason::NativePanicked
                });
                claim.finished = true;
                if existing_panic_custodian {
                    drop(state);
                    drop(claim);
                    resume_unwind(original);
                }
                if state.panic.is_none() {
                    state.panic = Some(PanicFailure { original, _charge: claim.charge.take().unwrap() });
                } else {
                    std::mem::forget((original, claim.charge.take()));
                }
                Routed::Continue(RouteStatus::Fatal(Reason::NativePanicked))
            }
        };
        let charge = claim.charge.take();
        drop(claim);
        Routing::new(outcome, charge)
    }
    fn dispose_returned(&self, event: UserEvent, refusal: SendRefusal) {
        match event.notification_policy() {
            Policy::Value | Policy::Intent | Policy::Readiness | Policy::Wake | Policy::ExternalOpen => drop(event),
            Policy::Owner | Policy::Control | Policy::GpuError => {
                self.fail(match refusal {
                    SendRefusal::Fatal(reason) => reason,
                    _ => Reason::UnresolvedControl,
                });
                // The carrier retains actual payload and its admission on Drop.
                drop(event);
            }
        }
    }
    pub(super) fn send_event(&self, event: UserEvent) -> Result<(), SendRefusal> {
        let routing = self.route(event, false);
        match routing.outcome {
            Routed::Continue(RouteStatus::Enqueued) => Ok(()),
            Routed::Continue(RouteStatus::OffUiWake) => Err(SendRefusal::OffUiWake),
            Routed::Break((event, refusal)) => {
                self.dispose_returned(event, refusal);
                Err(refusal)
            }
            Routed::Continue(RouteStatus::Fatal(reason)) => Err(SendRefusal::Fatal(reason)),
        }
    }
    pub(super) fn send_computed_value(
        &self,
        event: UserEvent,
        retire_original: impl FnOnce(UserEvent),
    ) -> Result<(), SendRefusal> {
        debug_assert_eq!(event.notification_policy(), Policy::Value);
        let routing = self.route(event, false);
        match routing.outcome {
            Routed::Continue(RouteStatus::Enqueued) => Ok(()),
            Routed::Break((original, refusal)) => {
                retire_original(original);
                Err(refusal)
            }
            Routed::Continue(RouteStatus::Fatal(reason)) => Err(SendRefusal::Fatal(reason)),
            Routed::Continue(RouteStatus::OffUiWake) => {
                self.fail(Reason::Identity);
                Err(SendRefusal::Fatal(Reason::Identity))
            }
        }
    }
    pub(super) fn send_owned_wake(&self, event: UserEvent) -> Result<WakeDisposition, SendRefusal> {
        debug_assert_eq!(event.notification_policy(), Policy::Wake);
        let routing = self.route(event, true);
        match routing.outcome {
            Routed::Continue(RouteStatus::Enqueued) => Ok(WakeDisposition::NativeEnqueued),
            Routed::Continue(RouteStatus::OffUiWake) => Ok(WakeDisposition::OffUiWake),
            Routed::Break((event, refusal)) => {
                self.dispose_returned(event, refusal);
                Err(refusal)
            }
            Routed::Continue(RouteStatus::Fatal(reason)) => Err(SendRefusal::Fatal(reason)),
        }
    }
    pub(super) fn wait_native_until(&self, deadline: Instant) -> Result<NativeQuiescenceProof, Reason> {
        self.wait_native_with_clock(deadline, Instant::now)
    }
    fn wait_native_with_clock<C: FnMut() -> Instant>(
        &self,
        deadline: Instant,
        mut now: C,
    ) -> Result<NativeQuiescenceProof, Reason> {
        let shared = self.shared().ok_or(Reason::Identity)?;
        let bytes = Self::operation_bytes()
            .and_then(|n| n.checked_add(2 * size_of::<C>()))
            .and_then(|n| n.checked_add(2 * size_of::<&Self>() + size_of::<&mut C>()))
            .ok_or(Reason::Admission)?;
        let charge = shared.budget.reserve_metadata(bytes).map_err(|_| Reason::Admission)?;
        let mut state = lock(&shared.state);
        loop {
            let remaining = deadline.saturating_duration_since(now());
            if remaining.is_zero() {
                return Err(Reason::TimedOut);
            }
            if state.requested && state.active == 0 {
                if now() >= deadline {
                    return Err(Reason::TimedOut);
                }
                return Ok(NativeQuiescenceProof { shared: shared.clone(), deadline, _charge: charge });
            }
            state = shared.changed.wait_timeout(state, remaining).unwrap_or_else(std::sync::PoisonError::into_inner).0;
        }
    }
    pub(super) fn reserve(&self, kind: SlotKind, capture_bytes: usize) -> Result<ProducerTransfer, Reason> {
        #[cfg(test)]
        if matches!(self, Self::Test(_)) {
            return Ok(ProducerTransfer {
                proxy: self.clone(),
                kind,
                generation: 0,
                started: AtomicBool::new(false),
                resolved: false,
            });
        }
        let shared = self.shared().ok_or(Reason::Identity)?;
        let bytes = size_of::<ProducerTransfer>()
            .checked_mul(2)
            .and_then(|n| n.checked_add(capture_bytes))
            .and_then(|n| n.checked_add(Self::operation_bytes()?))
            .ok_or(Reason::Admission)?;
        let charge = shared.budget.reserve_metadata(bytes).map_err(|_| Reason::Admission)?;
        let mut state = lock(&shared.state);
        if state.requested || state.native_blocked.is_some() {
            return Err(state.native_blocked.unwrap_or(Reason::FailedVerdict));
        }
        let slot = &mut state.slots[kind.index()];
        if slot.outstanding || slot.receipts != 0 {
            return Err(Reason::UnresolvedOwner);
        }
        let generation = slot.generation.checked_add(1).ok_or(Reason::Exhausted)?;
        slot.generation = generation;
        slot.outstanding = true;
        slot.no_owner_created = false;
        slot.charge = Some(charge);
        Ok(ProducerTransfer { proxy: self.clone(), kind, generation, started: AtomicBool::new(false), resolved: false })
    }
    pub(super) fn control_endpoint(&self, kind: SlotKind, capture_bytes: usize) -> Result<ControlEndpoint, Reason> {
        if let Some(shared) = self.shared() {
            let charge = shared
                .budget
                .reserve_metadata(
                    size_of::<ControlEndpoint>()
                        .checked_mul(2)
                        .and_then(|n| n.checked_add(capture_bytes))
                        .ok_or(Reason::Admission)?,
                )
                .map_err(|_| Reason::Admission)?;
            let state = lock(&shared.state);
            if state.requested || state.native_blocked.is_some() {
                return Err(state.native_blocked.unwrap_or(Reason::FailedVerdict));
            }
            let slot = &state.slots[kind.index()];
            if slot.outstanding {
                // Dialogs.sync drops this unused replacement closure when its
                // original worker already owns the authoritative callback.
                let transfer = ProducerTransfer {
                    proxy: self.clone(),
                    kind,
                    generation: slot.generation,
                    started: AtomicBool::new(true),
                    resolved: true,
                };
                return Ok(ControlEndpoint { transfer, _charge: Some(charge) });
            }
            drop(state);
            return Ok(ControlEndpoint { transfer: self.reserve(kind, capture_bytes)?, _charge: Some(charge) });
        }
        Ok(ControlEndpoint { transfer: self.reserve(kind, capture_bytes)?, _charge: None })
    }
    pub(super) fn reserve_callback(&self, capture_bytes: usize) -> Result<CallbackCapability, Reason> {
        Ok(CallbackCapability(Some(self.reserve(SlotKind::GpuCallback, capture_bytes)?)))
    }
    pub(super) fn retain_gpu_error(&self, original: String) -> Result<String, Reason> {
        self.retain_diagnostic(original, true)
    }
    pub(super) fn retain_control_error(&self, original: String) -> Result<String, Reason> {
        self.retain_diagnostic(original, false)
    }
    fn retain_diagnostic(&self, original: String, gpu: bool) -> Result<String, Reason> {
        #[cfg(test)]
        if matches!(self, Self::Test(_)) {
            return Ok(original);
        }
        let shared = self.shared().ok_or(Reason::Identity)?;
        let bytes = original
            .capacity()
            .checked_add(original.len())
            .and_then(|n| n.checked_add(2 * size_of::<String>()))
            .and_then(|n| n.checked_add(size_of::<GpuFailure>()));
        let charge = match bytes.and_then(|bytes| shared.budget.reserve_metadata(bytes).ok()) {
            Some(charge) => charge,
            None => {
                // Moving an already-created original into the admitted fixed
                // failure header allocates/copies nothing. Admission refusal
                // forbids the diagnostic clone, never erases public custody.
                let mut state = lock(&shared.state);
                state.failed.get_or_insert(Reason::Admission);
                let target = if gpu { &mut state.gpu_error } else { &mut state.control_error };
                if target.is_none() {
                    *target = Some(GpuFailure { original, _charge: None });
                } else {
                    std::mem::forget(original);
                }
                shared.changed.notify_all();
                return Err(Reason::Admission);
            }
        };
        // One admitted diagnostic copy feeds the unchanged live fallback API.
        let diagnostic = original.clone();
        let mut state = lock(&shared.state);
        state.failed.get_or_insert(if gpu { Reason::GpuError } else { Reason::ControlError });
        let target = if gpu { &mut state.gpu_error } else { &mut state.control_error };
        if target.is_none() {
            *target = Some(GpuFailure { original, _charge: Some(charge) });
        } else {
            std::mem::forget((original, charge));
        }
        shared.changed.notify_all();
        Ok(diagnostic)
    }
    pub(super) fn finish_success(&self, proof: &NativeQuiescenceProof, deadline: Instant) -> Result<(), Reason> {
        let shared = self.shared().ok_or(Reason::Identity)?;
        if !Arc::ptr_eq(shared, &proof.shared) || deadline != proof.deadline {
            return Err(Reason::Identity);
        }
        let mut state = lock(&shared.state);
        if state.released {
            return Err(Reason::Identity);
        }
        if Instant::now() >= deadline {
            state.failed.get_or_insert(Reason::TimedOut);
        }
        if let Some(reason) = state.failed {
            return Err(reason);
        }
        if !state.requested || state.active != 0 {
            return Err(Reason::Identity);
        }
        if let Some(slot) = state.slots.iter().find(|slot| slot.outstanding || slot.receipts != 0) {
            let reason = if slot.no_owner_created { Reason::UnresolvedControl } else { Reason::UnresolvedOwner };
            state.failed.get_or_insert(reason);
            return Err(reason);
        }
        state.released = true;
        Ok(())
    }
    pub(super) fn wait_capabilities_until(&self, deadline: Instant) -> Result<(), Reason> {
        let shared = self.shared().ok_or(Reason::Identity)?;
        let mut state = lock(&shared.state);
        while state.slots[SlotKind::GpuCallback.index()].outstanding {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Reason::TimedOut);
            }
            state = shared.changed.wait_timeout(state, remaining).unwrap_or_else(std::sync::PoisonError::into_inner).0;
        }
        if Instant::now() >= deadline {
            return Err(Reason::TimedOut);
        }
        // Capability retirement is not the combined GUI verdict. In particular
        // an authoritatively unstarted Workspace may still follow its original
        // recovery API while the original failed GUI receiver stays quarantined.
        state.native_blocked.map_or(Ok(()), Err)
    }
}

/// Pre-start reservation. Scheduled-but-not-started producers remain outstanding.
pub(super) struct ProducerTransfer {
    proxy: HostProxy,
    kind: SlotKind,
    generation: u64,
    started: AtomicBool,
    resolved: bool,
}
impl ProducerTransfer {
    pub(super) fn start(&self) {
        self.started.store(true, Ordering::Release);
    }
    pub(super) fn no_owner_created(&self) {
        if let Some(shared) = self.proxy.shared() {
            let mut state = lock(&shared.state);
            let slot = &mut state.slots[self.kind.index()];
            if slot.generation == self.generation {
                slot.no_owner_created = true;
            } else {
                state.failed.get_or_insert(Reason::Identity);
            }
        }
    }
    fn matches(&self, proxy: &HostProxy, kind: SlotKind) -> bool {
        if self.kind != kind || !self.proxy.same_gate(proxy) {
            return false;
        }
        match self.proxy.shared() {
            Some(shared) => {
                let state = lock(&shared.state);
                let slot = &state.slots[kind.index()];
                slot.generation == self.generation && slot.outstanding
            }
            None => cfg!(test),
        }
    }
    fn finish(&mut self) -> Result<(), Reason> {
        #[cfg(test)]
        if matches!(self.proxy, HostProxy::Test(_)) {
            self.resolved = true;
            return Ok(());
        }
        let shared = self.proxy.shared().ok_or(Reason::Identity)?;
        let mut state = lock(&shared.state);
        let slot = &mut state.slots[self.kind.index()];
        if slot.generation != self.generation || !slot.outstanding {
            return Err(Reason::Identity);
        }
        slot.outstanding = false;
        if slot.receipts == 0 {
            drop(slot.charge.take());
        }
        self.resolved = true;
        shared.changed.notify_all();
        Ok(())
    }
    pub(super) fn publish<T>(self, value: T) -> Delivery<T> {
        Delivery { value: Some(value), transfer: Some(self) }
    }
    pub(super) fn receipt<T>(&self, value: T, terminal: bool) -> Result<Receipt<T>, T> {
        self.start();
        let Some(shared) = self.proxy.shared() else {
            #[cfg(test)]
            return Ok(Receipt {
                value: Some(value),
                proxy: self.proxy.clone(),
                kind: self.kind,
                generation: 0,
                terminal,
                charge: None,
                consumed: false,
            });
            #[cfg(not(test))]
            return Err(value);
        };
        let bytes = size_of::<Receipt<T>>()
            .checked_mul(2)
            .and_then(|n| n.checked_add(2 * size_of::<Result<Receipt<T>, T>>()))
            .and_then(|n| n.checked_add(HostProxy::operation_bytes()?));
        let Some(bytes) = bytes else {
            self.proxy.fail(Reason::Admission);
            return Err(value);
        };
        let charge = match shared.budget.reserve_metadata(bytes) {
            Ok(charge) => charge,
            Err(_) => {
                self.proxy.fail(Reason::Admission);
                return Err(value);
            }
        };
        let mut state = lock(&shared.state);
        let slot = &mut state.slots[self.kind.index()];
        if slot.generation != self.generation || !slot.outstanding {
            state.failed.get_or_insert(Reason::Identity);
            return Err(value);
        }
        let Some(count) = slot.receipts.checked_add(1) else {
            state.failed.get_or_insert(Reason::Exhausted);
            return Err(value);
        };
        slot.receipts = count;
        Ok(Receipt {
            value: Some(value),
            proxy: self.proxy.clone(),
            kind: self.kind,
            generation: self.generation,
            terminal,
            charge: Some(charge),
            consumed: false,
        })
    }
}
impl Drop for ProducerTransfer {
    fn drop(&mut self) {
        if self.resolved {
            return;
        }
        if !self.started.load(Ordering::Acquire) {
            // Only a not-escaped/dropped constructor closure can reach this
            // branch: a scheduled closure retains this exact token until start.
            let _ = self.finish();
        } else if !matches!(self.kind, SlotKind::FileDialog | SlotKind::Confirmation) {
            self.proxy.fail(Reason::UnresolvedOwner);
            std::mem::forget(self.proxy.clone());
        }
        // FileDialog's actual Stopped receipt, never endpoint Drop, resolves it.
    }
}

pub(super) struct Delivery<T> {
    value: Option<T>,
    transfer: Option<ProducerTransfer>,
}
impl<T> Delivery<T> {
    pub(super) fn value_mut(&mut self) -> &mut T {
        self.value.as_mut().unwrap()
    }
    pub(super) fn matches(&self, proxy: &HostProxy, kind: SlotKind) -> bool {
        self.transfer.as_ref().is_some_and(|transfer| transfer.matches(proxy, kind))
    }
    /// The mapper is private, source-reviewed, and only performs final owner moves.
    pub(super) fn adopt_into<U>(mut self, target: &mut Option<U>, map: impl FnOnce(T) -> U) -> Result<(), Reason> {
        let transfer = self.transfer.as_ref().unwrap();
        if target.is_some() || !transfer.matches(&transfer.proxy, transfer.kind) {
            return Err(Reason::Identity);
        }
        *target = Some(map(self.value.take().unwrap()));
        self.transfer.as_mut().unwrap().finish()?;
        self.transfer.take();
        Ok(())
    }
    pub(super) fn consume_error(mut self, consume: impl FnOnce(T)) -> Result<(), Reason> {
        consume(self.value.take().unwrap());
        self.transfer.as_mut().unwrap().finish()?;
        self.transfer.take();
        Ok(())
    }
}
impl<T> Drop for Delivery<T> {
    fn drop(&mut self) {
        if let Some(value) = self.value.take() {
            if let Some(transfer) = &self.transfer {
                transfer.proxy.fail(Reason::UnresolvedOwner);
                std::mem::forget(transfer.proxy.clone());
            }
            std::mem::forget(value);
        }
        // Slot retains the pre-start charge while the owner is unresolved.
    }
}

pub(super) struct Receipt<T> {
    value: Option<T>,
    proxy: HostProxy,
    kind: SlotKind,
    generation: u64,
    terminal: bool,
    charge: Option<MetadataReservation>,
    consumed: bool,
}
impl<T> Receipt<T> {
    pub(super) fn value(&self) -> &T {
        self.value.as_ref().unwrap()
    }
    pub(super) fn matches(&self, proxy: &HostProxy, kind: SlotKind) -> bool {
        if self.kind != kind || !self.proxy.same_gate(proxy) {
            return false;
        }
        match self.proxy.shared() {
            Some(shared) => {
                let state = lock(&shared.state);
                let slot = &state.slots[kind.index()];
                slot.generation == self.generation && slot.receipts != 0
            }
            None => cfg!(test),
        }
    }
    pub(super) fn consume(mut self, apply: impl FnOnce(T)) -> Result<(), Reason> {
        #[cfg(test)]
        if matches!(self.proxy, HostProxy::Test(_)) {
            apply(self.value.take().unwrap());
            self.consumed = true;
            return Ok(());
        }
        let shared = self.proxy.shared().ok_or(Reason::Identity)?;
        {
            let state = lock(&shared.state);
            let slot = &state.slots[self.kind.index()];
            if slot.generation != self.generation || slot.receipts == 0 {
                return Err(Reason::Identity);
            }
        }
        apply(self.value.take().unwrap());
        let mut state = lock(&shared.state);
        let slot = &mut state.slots[self.kind.index()];
        slot.receipts -= 1;
        if self.terminal {
            slot.outstanding = false;
        }
        if !slot.outstanding && slot.receipts == 0 {
            drop(slot.charge.take());
        }
        self.consumed = true;
        shared.changed.notify_all();
        Ok(())
    }
}

pub(super) struct ControlEndpoint {
    transfer: ProducerTransfer,
    _charge: Option<MetadataReservation>,
}
impl ControlEndpoint {
    pub(super) fn start(&self) {
        self.transfer.start();
    }
    pub(super) fn receipt<T>(&self, value: T, terminal: bool) -> Result<Receipt<T>, T> {
        self.transfer.receipt(value, terminal)
    }
}
impl<T> Drop for Receipt<T> {
    fn drop(&mut self) {
        if !self.consumed {
            self.proxy.fail(Reason::UnresolvedControl);
            std::mem::forget(self.proxy.clone());
            std::mem::forget(self.value.take());
            std::mem::forget(self.charge.take());
        }
    }
}

pub(super) struct CallbackCapability(Option<ProducerTransfer>);
impl CallbackCapability {
    pub(super) fn proxy(&self) -> &HostProxy {
        &self.0.as_ref().unwrap().proxy
    }
}
impl Drop for CallbackCapability {
    fn drop(&mut self) {
        if let Some(mut transfer) = self.0.take() {
            let _ = transfer.finish();
        }
    }
}

/// The actual original !Send receiver and origin-only owners, never in Failure.
/// Default Drop intentionally retains them and their charge on every failed path.
pub(super) struct OriginLoopOwner<L, O> {
    receiver: ManuallyDrop<L>,
    owners: Option<ManuallyDrop<O>>,
    proxy: HostProxy,
    origin: ThreadId,
    charge: ManuallyDrop<MetadataReservation>,
    released: bool,
}
impl<L, O> OriginLoopOwner<L, O> {
    pub(super) fn admit(receiver: L, proxy: HostProxy, budget: &AllocationBudget) -> Result<Self, (L, WrapError)> {
        let bytes = size_of::<Self>()
            .checked_mul(2)
            .and_then(|n| n.checked_add(2 * size_of::<Result<Self, (L, WrapError)>>()))
            .and_then(|n| n.checked_add(size_of::<O>()))
            .ok_or(WrapError::Admission("GUI origin owner overflow"));
        let charge = match bytes.and_then(|bytes| budget.reserve_metadata(bytes)) {
            Ok(charge) => charge,
            Err(error) => return Err((receiver, error)),
        };
        Ok(Self {
            receiver: ManuallyDrop::new(receiver),
            owners: None,
            proxy,
            origin: thread::current().id(),
            charge: ManuallyDrop::new(charge),
            released: false,
        })
    }
    pub(super) fn receiver_mut(&mut self) -> &mut L {
        &mut self.receiver
    }
    pub(super) fn retain_owners(&mut self, owners: O) {
        self.owners = Some(ManuallyDrop::new(owners));
    }
    pub(super) fn release(mut self, success: super::shutdown::FinalSuccess) -> Result<(), Reason> {
        let (proof, deadline) = success.into_parts();
        if thread::current().id() != self.origin {
            self.proxy.fail(Reason::Identity);
            return Err(Reason::Identity);
        }
        self.proxy.finish_success(&proof, deadline)?;
        // Only the combined final success caller may invoke this function.
        // Original owners are destroyed on the exact thread that created them.
        if let Some(mut owners) = self.owners.take() {
            unsafe {
                ManuallyDrop::drop(&mut owners);
            }
        }
        unsafe {
            ManuallyDrop::drop(&mut self.receiver);
            ManuallyDrop::drop(&mut self.charge);
        }
        self.released = true;
        Ok(())
    }
}
impl<L, O> Drop for OriginLoopOwner<L, O> {
    fn drop(&mut self) {
        if !self.released {
            self.proxy.request_terminal();
            self.proxy.fail(Reason::FailedVerdict);
            // Keep gate failure custody and all original resources/credit alive.
            std::mem::forget(self.proxy.clone());
        }
    }
}

#[cfg(test)]
#[path = "notification_tests.rs"]
mod tests;
