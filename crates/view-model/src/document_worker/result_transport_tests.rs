//! SOURCE ONLY: ROOT runs these tiny deterministic controls and mutations.
use super::*;
use crate::{Document, Editor};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc::{self, TrySendError},
};

const LIMIT: Duration = Duration::from_secs(2);

// Test consumers sometimes need to leave Saved unaccepted. Classify the slot
// and claim empty capacity under ONE lock; a wake is not a slot reservation.
impl DocumentResultReceiver {
    pub(crate) fn next_save_test_event(&mut self, deadline: Instant) -> Option<ResultInfo> {
        let mut state = lock(&self.shared.state);
        loop {
            assert_eq!(state.failure(), None, "save fixture transport failure");
            if !state.transfer_active && !state.retire_requested {
                if let Some(slot) = &state.slot {
                    let delivery = DocumentDeliveryId { worker: self.shared.worker_id, delivery: state.delivery };
                    return Some(slot.info(delivery));
                }
                if !state.in_flight && state.capacity_generation != self.observed_capacity {
                    self.observed_capacity = state.capacity_generation;
                    return None;
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "real save slot exceeded the existing absolute deadline");
            state = self.shared.changed.wait_timeout(state, remaining).unwrap().0;
        }
    }

    pub(crate) fn wait_save_retired_for_test(&self, delivery: DocumentDeliveryId, deadline: Instant) {
        let mut state = lock(&self.shared.state);
        assert_eq!(delivery.worker, self.shared.worker_id);
        while state.delivery == delivery.delivery && state.in_flight {
            assert_eq!(state.failure(), None, "save fixture retirement failure");
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "original producer residual retirement exceeded the existing deadline");
            state = self.shared.changed.wait_timeout(state, remaining).unwrap().0;
        }
        assert_eq!(state.failure(), None);
        assert!(state.commit_error.is_none(), "the original producer retired the first refusal");
    }
}

struct NativeTeardown {
    entered: mpsc::SyncSender<std::thread::ThreadId>,
    release: mpsc::Receiver<()>,
    deadline: Instant,
}
impl Drop for NativeTeardown {
    fn drop(&mut self) {
        let _ = self.entered.try_send(std::thread::current().id());
        let _ = self.release.recv_timeout(self.deadline.saturating_duration_since(Instant::now()));
    }
}
thread_local! {
    static NATIVE_TEARDOWN: std::cell::RefCell<Option<NativeTeardown>> = const { std::cell::RefCell::new(None) };
}

fn native_backoff(deadline: Instant) {
    std::thread::park_timeout(deadline.saturating_duration_since(Instant::now()).min(Duration::from_millis(1)));
}

// Explicit test callers reuse ONE original deadline and retry ONLY typed wrap
// native Pending. A completed attempt is not a receipt, nor native-exit proof.
fn complete_wrap_retry(
    receiver: &mut DocumentResultReceiver,
    token: DocumentWrapRetryToken,
    deadline: Instant,
) -> DocumentWrapRetryToken {
    let shared = receiver.shared.clone();
    let original = lock(&shared.worker).as_ref().unwrap().thread().id();
    crate::cleanup_test_support::cleanup_until(
        Some(token), deadline,
        |token, deadline| {
            let token = match token {
                Some(token) => token,
                None => match receiver.request_wrap_retry(deadline.saturating_duration_since(Instant::now())) {
                    Ok(token) => token,
                    Err(reason) => return ControlFlow::Break((None, reason)),
                },
            };
            let state = lock(&shared.state);
            let (state, _) = shared.changed.wait_timeout_while(
                state, deadline.saturating_duration_since(Instant::now()),
                |state| state.retry.is_some() || state.retry_running,
            ).unwrap();
            assert!(!state.retry_running && state.retry.is_none(), "original retry deadline expired");
            assert_eq!(state.retry_generation, token.0.control.generation);
            assert_eq!(state.delivery, token.0.control.delivery.delivery);
            match state.failure() {
                None => {
                    assert!(matches!(state.whole(), Some(DocumentResult::WrapSettled(Ok(_)))));
                    assert!(state.retry_acknowledgement_required.is_some_and(|control| control.same_identity(&token.0.control)));
                    ControlFlow::Continue(token)
                }
                Some(DocumentResultSettlementReason::WrapThreadExitPending) => {
                    assert!(state.in_flight);
                    assert!(state.retry_acknowledgement_required.is_none(), "Pending has no ACK authority");
                    assert!(matches!(state.whole(), Some(DocumentResult::WrapSettled(Err(failure))) if matches!(failure.error, WrapError::ThreadExitPending)));
                    drop(state);
                    assert!(!lock(&shared.requests).accepting);
                    assert_eq!(lock(&shared.worker).as_ref().unwrap().thread().id(), original);
                    // Only this completed scalar attempt retires, without ACK.
                    // The exact wrap ticket stays in the original producer slot.
                    drop(token);
                    ControlFlow::Break((None, DocumentResultSettlementReason::WrapThreadExitPending))
                }
                Some(reason) => ControlFlow::Break((Some(token), reason)),
            }
        },
        |reason| matches!(reason, DocumentResultSettlementReason::WrapThreadExitPending),
    ).unwrap()
}

fn retry_wrap_off_ui(ticket: &mut DocumentResultSettlementTicket, deadline: Instant) {
    crate::cleanup_test_support::cleanup_until(
        ticket, deadline,
        |ticket, deadline| {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "original off-UI retry deadline expired");
            match ticket.retry_pending_wrap(remaining) {
                Ok(retried) => { assert!(retried); ControlFlow::Continue(()) }
                Err(reason) => {
                    if matches!(reason, DocumentResultSettlementReason::WrapThreadExitPending) {
                        let inspection = ticket.inspect();
                        assert!(matches!(inspection.pending(), Some(DocumentResult::WrapSettled(Err(failure))) if matches!(failure.error, WrapError::ThreadExitPending)));
                        drop(inspection);
                    }
                    ControlFlow::Break((ticket, reason))
                }
            }
        },
        |reason| matches!(reason, DocumentResultSettlementReason::WrapThreadExitPending),
    ).unwrap();
}

fn successful_wrap_retry(shared: &Shared) -> bool {
    let state = lock(&shared.state);
    state.retry_acknowledgement_required.is_some() && matches!(state.whole(), Some(DocumentResult::WrapSettled(Ok(_))))
}

fn join_with_retirement_refusal(shared: &Shared, deadline: Instant, error: NativeObservationError) {
    loop {
        match shared.settle_native_until_with(deadline, NativeExitObservation::query, |_| Err(error)) {
            Err(DocumentResultSettlementReason::ThreadExitPending) => native_backoff(deadline),
            result => {
                assert_eq!(result, Err(DocumentResultSettlementReason::NativeExit(error)));
                break;
            }
        }
    }
}

// This module is cfg(test). Borrow the exact bound receiver and original
// producer; consume its real Stopped slot with finish_until's ACK sequence,
// then use the EXISTING one-query seam. No replacement handle or join policy.
impl BoundDocumentTicket {
    fn finish_held_until_with_query(
        &mut self,
        deadline: Instant,
        query: impl FnOnce(&NativeExitObservation, &JoinHandle<()>) -> Result<NativeExitState, NativeObservationError>,
    ) -> Result<(), DocumentResultSettlementReason> {
        let shared = &self.receiver.shared;
        if lock(&shared.requests).phase != ShutdownPhase::Closed {
            return Err(DocumentResultSettlementReason::SourceBarrierRequired);
        }
        let mut state = lock(&shared.state);
        loop {
            if let Some(reason) = state.failure() {
                return Err(reason);
            }
            if Instant::now() >= deadline {
                return Err(DocumentResultSettlementReason::TimedOut);
            }
            if state.slot.is_some() && !state.retire_requested && !state.transfer_active {
                if !matches!(state.whole(), Some(DocumentResult::Stopped(Ok(())))) {
                    return Err(DocumentResultSettlementReason::ConsumerRefused);
                }
                state.stopped_acknowledged |= matches!(state.whole(), Some(DocumentResult::Stopped(Ok(()))));
                let retired = state.slot.take();
                state.in_flight = false;
                drop(state);
                drop(retired);
                shared.changed.notify_all();
                state = lock(&shared.state);
                continue;
            }
            if state.worker_done {
                let acknowledged = state.stopped_acknowledged;
                drop(state);
                shared.settle_native_until_with(deadline, query, NativeExitObservation::retire_after_join)?;
                return if acknowledged { Ok(()) } else { Err(DocumentResultSettlementReason::MissingStoppedReceipt) };
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(DocumentResultSettlementReason::TimedOut);
            }
            state = shared.changed.wait_timeout(state, remaining).unwrap_or_else(std::sync::PoisonError::into_inner).0;
        }
    }
}

#[test]
fn result_transport_bound_native_tls_keeps_exact_handle_and_queries_once_per_attempt() {
    let deadline = Instant::now() + LIMIT;
    let (entered, reached) = mpsc::sync_channel(1);
    let (release, held) = mpsc::sync_channel(1);
    let (worker, receiver, shared) = harness(
        || Ok(()),
        move |shared, inbox| {
            NATIVE_TEARDOWN.with(|slot| *slot.borrow_mut() = Some(NativeTeardown { entered, release: held, deadline }));
            let DocumentRequest::SourceBarrier(marker) = inbox.recv_timeout(LIMIT).unwrap() else {
                panic!("original source barrier must precede close")
            };
            shared.publish(DocumentResult::SourceBarrier(DocumentBarrierReceipt::reached(marker)));
            assert!(matches!(inbox.recv_timeout(LIMIT), Err(mpsc::RecvTimeoutError::Disconnected)));
            shared.publish(DocumentResult::Stopped(Ok(())));
        },
    );
    let original = lock(&shared.worker).as_ref().unwrap().thread().id();
    let identity = Arc::as_ptr(&shared);
    let (mut bound, mut permit) = receiver.begin_bound(&worker).unwrap();
    permit.request_barrier().unwrap();
    wait(&shared, |state| matches!(state.whole(), Some(DocumentResult::SourceBarrier(_))));
    let Some(DocumentResult::SourceBarrier(receipt)) = bound.receiver.try_recv(true).unwrap() else {
        panic!("same-lane barrier receipt required")
    };
    permit.seal_sources(&receipt).unwrap();
    bound.close_after_source_release(&permit).unwrap();
    let pending = bound.finish_held_until_with_query(deadline, NativeExitObservation::query);
    let native_thread = reached.recv_timeout(deadline.saturating_duration_since(Instant::now()));
    let queries = AtomicUsize::new(0);
    let observed = shared.settle_native_until_with(
        deadline,
        |observation, handle| {
            queries.fetch_add(1, Ordering::Relaxed);
            observation.query(handle)
        },
        |_| panic!("Pending TLS cannot retire"),
    );
    let fresh_error = NativeObservationError::LinuxQuery { errno: 73 };
    let error_queries = AtomicUsize::new(0);
    let fatal = bound.finish_held_until_with_query(deadline, |_, _| {
        error_queries.fetch_add(1, Ordering::Relaxed);
        Err(fresh_error)
    });
    release.try_send(()).unwrap();
    assert_eq!(pending, Err(DocumentResultSettlementReason::ThreadExitPending));
    assert_eq!(native_thread.unwrap(), original);
    assert_eq!(observed, Err(DocumentResultSettlementReason::ThreadExitPending));
    assert_eq!(queries.load(Ordering::Relaxed), 1);
    assert_eq!(fatal, Err(DocumentResultSettlementReason::NativeExit(fresh_error)));
    assert_eq!(error_queries.load(Ordering::Relaxed), 1, "fresh native errors must not retry");
    assert_eq!(lock(&shared.worker).as_ref().unwrap().thread().id(), original);
    assert_eq!(Arc::as_ptr(&bound.receiver.shared), identity);
    assert!(lock(&shared.state).stopped_acknowledged);
    assert!(!lock(&shared.state).worker_joined);
    assert!(!lock(&shared.state).native_retired);
    let finished = bound.finish_until(deadline);
    let returned = Instant::now();
    assert_eq!(finished, Ok(()), "public finish must wait for the original native exit without outer retries");
    assert!(returned < deadline);
    crate::cleanup_test_support::cleanup_until(
        &mut bound,
        deadline,
        |bound, deadline| {
            bound
                .finish_until(deadline)
                .map_or_else(|reason| ControlFlow::Break((bound, reason)), ControlFlow::Continue)
        },
        |reason| matches!(reason, DocumentResultSettlementReason::ThreadExitPending),
    )
    .unwrap();
    assert!(lock(&shared.worker).is_none());
    assert!(lock(&shared.state).native_retired);
}

#[test]
fn result_transport_native_query_errors_and_expired_deadline_never_take_original_handle() {
    let (release, held) = mpsc::sync_channel(1);
    let (worker, receiver, shared) = harness(
        || Ok(()),
        move |shared, _| {
            shared.publish(DocumentResult::Stopped(Ok(())));
            held.recv_timeout(LIMIT).unwrap();
        },
    );
    let original = lock(&shared.worker).as_ref().unwrap().thread().id();
    let error = NativeObservationError::LinuxValidate { errno: 73 };
    let queries = AtomicUsize::new(0);
    let result = shared.settle_native_until_with(
        Instant::now() + LIMIT,
        |_, _| {
            queries.fetch_add(1, Ordering::Relaxed);
            Err(error)
        },
        |_| panic!("query error cannot retire"),
    );
    assert_eq!(result, Err(DocumentResultSettlementReason::NativeExit(error)));
    assert_eq!(queries.load(Ordering::Relaxed), 1);
    assert_eq!(
        shared.settle_native_until_with(
            Instant::now(),
            |_, _| panic!("expired deadline queried"),
            |_| panic!("expired deadline retired")
        ),
        Err(DocumentResultSettlementReason::TimedOut)
    );
    assert_eq!(lock(&shared.worker).as_ref().unwrap().thread().id(), original);
    release.try_send(()).unwrap();
    finish(receiver.stop(worker).unwrap(), |_| ControlFlow::Continue(()));
}

#[test]
fn result_transport_successful_join_retires_on_retry_after_exact_first_refusal() {
    let (worker, mut receiver, shared) = harness(
        || Ok(()),
        |shared, _| {
            shared.publish(DocumentResult::Stopped(Ok(())));
        },
    );
    wait(&shared, |state| state.slot.is_some());
    assert!(matches!(receiver.try_recv(true).unwrap(), Some(DocumentResult::Stopped(Ok(())))));
    wait(&shared, |state| state.worker_done);
    let error = NativeObservationError::MachRetire { code: 41 };
    join_with_retirement_refusal(&shared, Instant::now() + LIMIT, error);
    assert!(lock(&shared.worker).is_none());
    assert!(lock(&shared.state).worker_joined);
    assert!(!lock(&shared.state).native_retired);
    assert_eq!(
        shared.settle_native_until_with(
            Instant::now(),
            |_, _| panic!("joined handle queried"),
            |_| Err(NativeObservationError::MachRetire { code: 97 })
        ),
        Err(DocumentResultSettlementReason::NativeExit(error))
    );
    assert_eq!(receiver.status().failure, Some(DocumentResultSettlementReason::NativeExit(error)));
    finish(receiver.stop(worker).unwrap(), |_| panic!("original stopped ACK already applied"));
    assert!(lock(&shared.state).native_retired);
    assert_eq!(lock(&shared.state).native_retirement_error, None);
}

struct NativePanicOwner(Arc<AtomicUsize>);
impl Drop for NativePanicOwner {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn result_transport_retirement_pending_quarantine_retains_observation_after_handle_consumed() {
    let (worker, mut receiver, shared) =
        harness(|| Ok(()), |shared, _| shared.publish(DocumentResult::Stopped(Ok(()))));
    wait(&shared, |state| state.slot.is_some());
    assert!(matches!(receiver.try_recv(true).unwrap(), Some(DocumentResult::Stopped(Ok(())))));
    wait(&shared, |state| state.worker_done);
    let error = NativeObservationError::MachRetire { code: 59 };
    join_with_retirement_refusal(&shared, Instant::now() + LIMIT, error);
    assert!(lock(&shared.worker).is_none());
    let original = Arc::as_ptr(&shared);
    let retained = Arc::downgrade(&shared);
    drop(shared);
    drop(receiver.stop(worker).unwrap());
    let shared = retained.upgrade().expect("pending native right outlives consumed handle and bundle Drop");
    assert_eq!(Arc::as_ptr(&shared), original);
    assert!(!lock(&shared.state).native_retired);
    assert_eq!(lock(&shared.state).native_retirement_error, Some(error));
    // The recovered original observation can still retire explicitly OFF UI;
    // Drop itself performed no native I/O and never acknowledged settlement.
    shared.retire_if_joined().unwrap();
    assert!(lock(&shared.state).native_retired);
}

#[test]
fn result_transport_join_panic_keeps_original_payload_and_retirement_precedence_on_retry() {
    let drops = Arc::new(AtomicUsize::new(0));
    let payload = drops.clone();
    let (worker, receiver, shared) = harness(
        || Ok(()),
        move |_, _| {
            std::panic::resume_unwind(Box::new(NativePanicOwner(payload)));
        },
    );
    wait(&shared, |state| state.worker_done);
    let error = NativeObservationError::MachRetire { code: 53 };
    join_with_retirement_refusal(&shared, Instant::now() + LIMIT, error);
    let identity = Arc::as_ptr(&shared);
    let payload_identity = {
        let state = lock(&shared.state);
        let owner = state.worker_panic.as_ref().unwrap().downcast_ref::<NativePanicOwner>().unwrap();
        assert!(Arc::ptr_eq(&owner.0, &drops));
        owner as *const NativePanicOwner
    };
    assert!(lock(&shared.worker).is_none());
    assert_eq!(receiver.status().failure, Some(DocumentResultSettlementReason::NativeExit(error)));
    let failure = receiver
        .stop(worker)
        .unwrap()
        .settle(Duration::ZERO, |_| panic!("panic cannot acknowledge a result"))
        .unwrap_err();
    assert_eq!(failure.reason, DocumentResultSettlementReason::WorkerPanicked);
    assert_eq!(Arc::as_ptr(&failure.ticket.receiver.shared), identity);
    assert!(lock(&shared.state).native_retired);
    assert_eq!(
        lock(&shared.state).worker_panic.as_ref().unwrap().downcast_ref::<NativePanicOwner>().unwrap()
            as *const NativePanicOwner,
        payload_identity
    );
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    let retained = Arc::downgrade(&shared);
    drop(shared);
    drop(failure);
    assert!(retained.upgrade().is_some(), "failure quarantine must outlive its consumed handle");
    assert_eq!(drops.load(Ordering::Relaxed), 0);
}

fn wrap_token_for_test(shared: &Arc<Shared>, generation: u64) -> DocumentWrapRetryToken {
    let mut control = {
        let state = lock(&shared.state);
        state.retry_acknowledgement_required.or(state.retry).unwrap_or(RetryControl {
            deadline: Instant::now() + LIMIT,
            delivery: DocumentDeliveryId { worker: shared.worker_id, delivery: state.delivery },
            generation,
            action: DocumentProducerAction::RetryWrap,
            failure: None,
        })
    };
    control.generation = generation;
    control.action = DocumentProducerAction::RetryWrap;
    DocumentWrapRetryToken(shared.admit_retry_token(control).unwrap())
}

/// Test-only pause at the historical gap: the request gate was fenced but the
/// sticky wake error was not published. The repaired path holds BOTH locks.
pub(super) struct WakeFailureGate {
    entered: mpsc::SyncSender<()>,
    release: mpsc::Receiver<()>,
}
pub(super) fn hold_wake_failure(shared: &Shared) {
    if let Some(gate) = lock(&shared.wake_failure_gate).as_ref() {
        gate.entered.try_send(()).expect("bounded wake-failure observation");
        gate.release.recv_timeout(LIMIT).expect("bounded wake-failure interleave release");
    }
}

/// Pause the original producer after its wake and outside transport locks.
/// A global try_lock probe otherwise also detects unrelated producer activity.
pub(super) struct PublicationGate {
    entered: mpsc::SyncSender<()>,
    release: mpsc::Receiver<()>,
}
pub(super) fn hold_publication(shared: &Shared) {
    if let Some(gate) = lock(&shared.publication_gate).as_ref() {
        gate.entered.try_send(()).expect("bounded publication observation");
        gate.release.recv_timeout(LIMIT).expect("bounded publication probe release");
    }
}

/// Pause the SAME producer with the actual completed residual in its stack,
/// after the shared slot is emptied and transfer_active is set. No source
/// owner enters either channel; observations contain scalar identities only.
pub(super) struct RetirementDisposalGate {
    entered: mpsc::SyncSender<RetirementDisposalObservation>,
    release: mpsc::Receiver<()>,
    deadline: Instant,
}
#[derive(Debug)]
pub(super) struct RetirementDisposalObservation {
    producer: std::thread::ThreadId,
    delivery: DocumentDeliveryId,
    residual_box: usize,
}
pub(super) fn hold_retirement_disposal(shared: &Shared, residual: Option<&DocumentSlot>) {
    if let Some(gate) = lock(&shared.retirement_disposal_gate).as_ref() {
        let commit = residual.and_then(DocumentSlot::partial).expect("actual guarded completed residual");
        assert!(commit.is_complete());
        let Some(DocumentResult::Indexed { result: Ok(document), .. }) = commit.pending() else {
            panic!("this narrow disposal control retains its original indexed Box")
        };
        gate.entered
            .try_send(RetirementDisposalObservation {
                producer: std::thread::current().id(),
                delivery: commit.info().delivery().unwrap(),
                residual_box: &**document as *const Document as usize,
            })
            .expect("bounded disposal phase observation");
        gate.release
            .recv_timeout(gate.deadline.saturating_duration_since(Instant::now()))
            .expect("same absolute disposal control deadline");
    }
}

fn harness(
    wake: impl Fn() -> io::Result<()> + Send + Sync + 'static,
    run: impl FnOnce(Arc<Shared>, mpsc::Receiver<DocumentRequest>) + Send + 'static,
) -> (DocumentWorker, DocumentResultReceiver, Arc<Shared>) {
    harness_with_budget(&AllocationBudget::default(), wake, run)
}

fn harness_with_budget(
    budget: &AllocationBudget,
    wake: impl Fn() -> io::Result<()> + Send + Sync + 'static,
    run: impl FnOnce(Arc<Shared>, mpsc::Receiver<DocumentRequest>) + Send + 'static,
) -> (DocumentWorker, DocumentResultReceiver, Arc<Shared>) {
    let (shared, receiver) = Shared::new(budget, wake).unwrap();
    let (sender, inbox) = mpsc::sync_channel(16);
    let worker = DocumentWorker {
        tx: RequestSender::new(sender, Some(&shared)),
        journal_capacity: Arc::new(AtomicUsize::new(16)),
        journal_controls: Arc::new(super::super::JournalControlCredits::new()),
        save_destinations: Default::default(),
    };
    let running = shared.clone();
    let thread = std::thread::spawn(move || {
        running.register_worker();
        let _exit = WorkerExit(Some(running.clone()));
        run(running, inbox);
    });
    shared.install_worker(thread);
    (worker, receiver, shared)
}

fn wait(shared: &Shared, predicate: impl Fn(&State) -> bool) {
    let state = lock(&shared.state);
    let (state, timeout) = shared.changed.wait_timeout_while(state, LIMIT, |state| !predicate(state)).unwrap();
    assert!(!timeout.timed_out() || predicate(&state), "bounded event-driven harness wait expired");
}

// Test-only cleanup retains the original ticket under one bounded deadline.
// Only fresh native Pending repeats; no raw join bypasses the deadline.
fn finish(
    ticket: DocumentResultSettlementTicket,
    mut accept: impl FnMut(DocumentResult) -> ControlFlow<DocumentResult, ()>,
) {
    let deadline = Instant::now() + LIMIT;
    crate::cleanup_test_support::cleanup_until(
        ticket,
        deadline,
        |ticket, deadline| {
            ticket
                .settle(deadline.saturating_duration_since(Instant::now()), &mut accept)
                .map_or_else(|failure| ControlFlow::Break((failure.ticket, failure.reason)), ControlFlow::Continue)
        },
        |reason| matches!(reason, DocumentResultSettlementReason::ThreadExitPending),
    )
    .unwrap();
}

#[derive(Debug)]
struct OwnedProbe {
    dropped: mpsc::SyncSender<std::thread::ThreadId>,
    inspected: Arc<AtomicUsize>,
}
impl fmt::Display for OwnedProbe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inspected.fetch_add(1, Ordering::SeqCst);
        f.write_str("owned probe")
    }
}
impl std::error::Error for OwnedProbe {}
impl Drop for OwnedProbe {
    fn drop(&mut self) {
        self.dropped.try_send(std::thread::current().id()).unwrap();
    }
}

fn loan(probe: OwnedProbe) -> (DocumentResult, usize) {
    let editor = Box::new(Editor::new(Document::new()));
    let identity = &*editor as *const Editor as usize;
    (
        DocumentResult::Replaced {
            key: kwil_core_search::worker::SearchKey { document_id: 7, revision: 0, generation: 3 },
            operation: 19,
            editor,
            result: Err(io::Error::other(probe)),
        },
        identity,
    )
}

#[test]
fn result_transport_preadmission_refuses_before_wake_or_worker() {
    let budget = AllocationBudget::new(0);
    let before = budget.retained_bytes();
    let wakes = Arc::new(AtomicUsize::new(0));
    let called = wakes.clone();
    assert!(
        DocumentWorker::spawn_bounded(&budget, move || {
            called.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .is_err()
    );
    assert_eq!(budget.retained_bytes(), before);
    assert_eq!(wakes.load(Ordering::SeqCst), 0);
}

#[test]
fn result_transport_resume_save_scalar_uses_original_lane_and_invalidates_barrier() {
    let budget = AllocationBudget::default();
    let (worker, mut receiver) = DocumentWorker::spawn_bounded(&budget, || Ok(())).unwrap();
    let shared = match &worker.tx {
        RequestSender::Bounded(shared) => shared.clone(),
        _ => unreachable!(),
    };
    let producer = lock(&shared.worker).as_ref().unwrap().thread().id();
    let key = super::super::SaveResumeKey {
        document: super::super::DocumentId(3),
        source_epoch: 5,
        revision: 7,
        generation: 11,
    };
    assert!(worker.try_submit(DocumentRequest::ResumeSaveCompletion { key }).is_ok());
    wait(&shared, |state| matches!(state.whole(), Some(DocumentResult::ResumeSaveCompletion { .. })));
    assert!(
        matches!(receiver.try_recv(true).unwrap(), Some(DocumentResult::ResumeSaveCompletion { key: actual }) if actual == key)
    );
    let (mut bound, mut permit) = receiver.begin_bound(&worker).unwrap();
    let marker = permit.request_barrier().unwrap();
    wait(&shared, |state| matches!(state.whole(), Some(DocumentResult::SourceBarrier(_))));
    let receipt = match lock(&shared.state).whole() {
        Some(DocumentResult::SourceBarrier(receipt)) => *receipt,
        _ => unreachable!(),
    };
    let key = super::super::SaveResumeKey { generation: 12, ..key };
    assert!(matches!(permit.submit(DocumentRequest::ResumeSaveCompletion { key }), ControlFlow::Continue(())));
    assert_eq!(permit.seal_sources(&receipt), Err(DocumentResultSettlementReason::SourceBarrierRequired));
    assert!(lock(&shared.requests).source_epoch > marker.source_epoch);
    // Retire the original scalar marker on the original producer; no payload
    // receive/second result slot is needed to admit its accepted successor.
    let transfer = {
        let mut state = lock(&shared.state);
        let value = state.slot.take();
        state.transfer_active = true;
        DocumentTransferGuard {
            shared: shared.clone(),
            value,
            delivery: DocumentDeliveryId { worker: shared.worker_id, delivery: state.delivery },
        }
    };
    transfer.restore(true);
    wait(&shared, |state| matches!(state.whole(), Some(DocumentResult::ResumeSaveCompletion { .. })));
    assert!(
        matches!(bound.receiver.try_recv(true).unwrap(), Some(DocumentResult::ResumeSaveCompletion { key: actual }) if actual == key)
    );
    assert_eq!(lock(&shared.worker).as_ref().unwrap().thread().id(), producer);
    finish(bound.into_legacy_for_test(worker), |result| {
        assert!(matches!(result, DocumentResult::Stopped(Ok(()))));
        ControlFlow::Continue(())
    });
}

#[test]
fn result_transport_producer_token_keeps_expired_weak_allocation_charged_through_refusal() {
    let budget = AllocationBudget::default();
    let before = budget.retained_bytes();
    let (shared, receiver) = Shared::new(&budget, || Ok(())).unwrap();
    let control = RetryControl {
        deadline: Instant::now() + LIMIT,
        delivery: DocumentDeliveryId { worker: shared.worker_id, delivery: 1 },
        generation: 1,
        action: DocumentProducerAction::ResetPrepublication(PrepublicationResetKind::SavePreparation),
        failure: None,
    };
    let token = shared.admit_retry_token(control).unwrap();
    let charged = token._charge.reserved_bytes();
    drop(receiver);
    drop(shared);
    assert!(token.shared.upgrade().is_none());
    assert_eq!(
        budget.retained_bytes(),
        before + charged,
        "Weak allocation outlives strong source owners only while charged"
    );
    let (foreign, receiver) = Shared::new(&AllocationBudget::default(), || Ok(())).unwrap();
    let workspace = crate::Workspace::new(crate::View::new(None)).unwrap();
    let refusal = acknowledge_prepublication_reset(&foreign, &workspace, token).unwrap_err();
    assert_eq!(refusal.reason, DocumentResultSettlementReason::RetryUnavailable);
    assert_eq!(budget.retained_bytes(), before + charged, "foreign refusal moves the exact admitted token");
    assert_eq!(refusal.token.control, control);
    drop(refusal);
    assert_eq!(budget.retained_bytes(), before, "actual Weak/control retirement releases its charge");
    drop((receiver, foreign));
}

#[test]
fn result_transport_reset_refuses_wrong_whole_and_nonstale_partial_without_taking_owner() {
    const BUDGET: usize = crate::visual_rows::MAX_SESSION_BYTES;
    let budget = AllocationBudget::new(BUDGET);
    let (worker, mut receiver, shared) = harness_with_budget(
        &budget,
        || Ok(()),
        |shared, inbox| {
            shared.publish(DocumentResult::CapacityAvailable);
            assert!(matches!(inbox.recv_timeout(LIMIT), Err(mpsc::RecvTimeoutError::Disconnected)));
            shared.publish(DocumentResult::Stopped(Ok(())));
        },
    );
    let mut workspace = crate::Workspace::new(crate::View::new(None)).unwrap();
    assert!(workspace.replace_document_worker_for_transport_test(Some(worker)).is_none());
    wait(&shared, |state| state.slot.is_some());
    let delivery = receiver.info().unwrap().delivery().unwrap();
    let wrong = DocumentDeliveryId { delivery: delivery.delivery + 1, ..delivery };
    assert_eq!(
        receiver.request_prepublication_reset(wrong, LIMIT).unwrap_err(),
        DocumentResultSettlementReason::RetryUnavailable
    );
    assert_eq!(
        receiver.request_prepublication_reset(delivery, LIMIT).unwrap_err(),
        DocumentResultSettlementReason::RetryUnavailable
    );
    assert!(matches!(lock(&shared.state).whole(), Some(DocumentResult::CapacityAvailable)));
    let blocker = budget.reserve(BUDGET - budget.retained_bytes()).unwrap();
    assert!(matches!(
        receiver.accept_workspace(&mut workspace, &budget, true),
        Err(DocumentResultSettlementReason::ConsumerRefused)
    ));
    drop(blocker);
    assert_eq!(
        receiver.request_prepublication_reset(delivery, LIMIT).unwrap_err(),
        DocumentResultSettlementReason::RetryUnavailable
    );
    assert_eq!(receiver.info().unwrap().delivery(), Some(delivery));
    assert_eq!(lock(&shared.state).retry_generation, 0);
    assert!(
        matches!(lock(&shared.state).slot.as_ref().and_then(DocumentSlot::partial), Some(commit) if commit.prepublication_reset_kind().is_none())
    );
    assert!(receiver.retained_commit_error().retained_commit_error().is_some());
    assert!(receiver.accept_workspace(&mut workspace, &budget, true).unwrap().is_some());
    wait(&shared, |state| !state.in_flight);
    assert!(receiver.retained_commit_error().retained_commit_error().is_none());
    let worker = workspace.replace_document_worker_for_transport_test(None).unwrap();
    finish(receiver.stop(worker).unwrap(), |_| ControlFlow::Continue(()));
}

#[test]
fn result_transport_live_capacity_event_survives_transfer_and_is_consumed_only_once() {
    let (worker, mut receiver, shared) = harness(
        || Ok(()),
        |shared, inbox| {
            assert!(matches!(inbox.recv_timeout(LIMIT).unwrap(), DocumentRequest::SessionTimer));
            shared.request_capacity_available();
            shared.publish(DocumentResult::CapacityAvailable);
            assert!(matches!(inbox.recv_timeout(LIMIT), Err(mpsc::RecvTimeoutError::Disconnected)));
            shared.publish(DocumentResult::Stopped(Ok(())));
        },
    );
    assert!(!receiver.take_capacity_event());
    assert!(worker.try_submit(DocumentRequest::SessionTimer).is_ok());
    wait(&shared, |state| state.slot.is_some());
    assert!(!receiver.take_capacity_event(), "occupied slot preserves its pending capacity event");
    let transfer = {
        let mut state = lock(&shared.state);
        let value = state.slot.take();
        state.transfer_active = true;
        DocumentTransferGuard {
            shared: shared.clone(),
            value,
            delivery: DocumentDeliveryId { worker: shared.worker_id, delivery: state.delivery },
        }
    };
    assert!(!receiver.take_capacity_event(), "a temporary empty transfer is not reusable capacity");
    transfer.restore(true);
    wait(&shared, |state| !state.in_flight);
    assert!(receiver.take_capacity_event());
    assert!(!receiver.take_capacity_event(), "repeated empty receive cannot drive another continuation");
    finish(receiver.stop(worker).unwrap(), |_| ControlFlow::Continue(()));
}

#[test]
fn result_transport_successful_live_retirement_fence_returns_exact_full_but_failures_stay_closed() {
    let budget = AllocationBudget::new(crate::visual_rows::MAX_SESSION_BYTES);
    let baseline = budget.retained_bytes();
    let (shared, receiver) = Shared::new(&budget, || Ok(())).unwrap();
    let (sender, inbox) = mpsc::sync_channel(16);
    let worker = DocumentWorker {
        tx: RequestSender::new(sender, Some(&shared)),
        journal_capacity: Arc::new(AtomicUsize::new(16)),
        journal_controls: Arc::new(super::super::JournalControlCredits::new()),
        save_destinations: Default::default(),
    };
    // A genuine successful transfer closes ordinary admission until the SAME
    // producer retires its residual. No thread/timing race synthesizes Full.
    {
        let mut state = lock(&shared.state);
        state.slot = Some(DocumentSlot::new(DocumentResult::CapacityAvailable));
        state.in_flight = true;
        state.transfer_active = true;
        let value = state.slot.take();
        drop(state);
        DocumentTransferGuard {
            shared: shared.clone(),
            value,
            delivery: DocumentDeliveryId { worker: shared.worker_id, delivery: 0 },
        }
        .restore(true);
    }
    let path = std::env::temp_dir().join("kwil-exact-retained-open-raw");
    let path_identity = path.as_os_str().as_encoded_bytes().as_ptr();
    let charged = budget.retained_bytes();
    let attempts = budget.allocation_attempts();
    let request =
        DocumentRequest::OpenRaw { document: super::super::DocumentId(7), revision: 0, path, rows: 3, generation: 19 };
    let Err(TrySendError::Full(request)) = worker.try_submit(request) else {
        panic!("successful Live residual retirement must retain the exact request as Full")
    };
    let DocumentRequest::OpenRaw { path, generation, .. } = &*request else { panic!("same OpenRaw request") };
    assert_eq!(path.as_os_str().as_encoded_bytes().as_ptr(), path_identity);
    assert_eq!(*generation, 19);
    assert!(matches!(inbox.try_recv(), Err(mpsc::TryRecvError::Empty)));
    assert_eq!(budget.retained_bytes(), charged);
    assert_eq!(budget.allocation_attempts(), attempts, "Full needs no new transport reservation");

    // A retained original consumer refusal is NOT successful retirement.
    lock(&shared.state).retire_requested = false;
    lock(&shared.state).commit_error =
        Some(crate::workspace::workspace_document::WorkspaceDocumentAcceptError::Identity);
    let Err(TrySendError::Disconnected(request)) = worker.try_submit(*request) else {
        panic!("original consumer refusal must retain closed admission")
    };
    assert!(matches!(
        lock(&shared.state).commit_error.as_ref(),
        Some(crate::workspace::workspace_document::WorkspaceDocumentAcceptError::Identity)
    ));
    lock(&shared.state).commit_error = None;
    lock(&shared.state).delivery_failure = Some(DocumentDeliveryFailure::Wake(io::Error::other("exact terminal wake")));
    let Err(TrySendError::Disconnected(request)) = worker.try_submit(*request) else {
        panic!("terminal failure must not become temporary Full")
    };
    assert!(matches!(lock(&shared.state).delivery_failure.as_ref(), Some(DocumentDeliveryFailure::Wake(_))));
    lock(&shared.state).delivery_failure = None;
    lock(&shared.requests).phase = ShutdownPhase::Drain;
    let Err(TrySendError::Disconnected(request)) = worker.try_submit(*request) else {
        panic!("ordinary sender cannot enter the private shutdown lane")
    };
    lock(&shared.requests).phase = ShutdownPhase::SourcesReleased;
    let Err(TrySendError::Disconnected(request)) = worker.try_submit(*request) else {
        panic!("released sources cannot regain ordinary admission")
    };
    shared.close_requests();
    let Err(TrySendError::Disconnected(request)) = worker.try_submit(*request) else {
        panic!("actual closure remains Disconnected")
    };
    let DocumentRequest::OpenRaw { path, generation, .. } = &*request else { panic!("same refused OpenRaw") };
    assert_eq!(path.as_os_str().as_encoded_bytes().as_ptr(), path_identity);
    assert_eq!(*generation, 19);
    // Test-only, OFF-UI disposal of the original unstarted carrier. No runtime
    // failure ACK/retry or shutdown permission is invented by this control.
    std::thread::scope(|scope| {
        scope
            .spawn(move || {
                let mut state = lock(&shared.state);
                state.slot = None;
                state.in_flight = false;
                state.retire_requested = false;
                drop(state);
                drop((request, inbox, worker, receiver, shared));
            })
            .join()
            .unwrap()
    });
    assert_eq!(budget.retained_bytes(), baseline);
}

#[test]
fn result_transport_complete_disposal_keeps_exact_full_through_old_first_error_retirement() {
    use crate::workspace::workspace_document::WorkspaceDocumentAcceptError;
    for earlier_refusal in [false, true] {
        let deadline = Instant::now() + LIMIT;
        let await_phase = |shared: &Shared, predicate: fn(&State) -> bool| {
            let state = lock(&shared.state);
            let (state, _) = shared
                .changed
                .wait_timeout_while(state, deadline.saturating_duration_since(Instant::now()), |state| {
                    !predicate(state)
                })
                .unwrap();
            assert!(predicate(&state), "same absolute disposal control deadline expired");
        };
        let budget = AllocationBudget::new(crate::visual_rows::MAX_SESSION_BYTES);
        let mut document = Box::new(Document::new());
        document.buffer.insert(0, b"original source").unwrap();
        let source_epoch = document.source_epoch;
        let residual_box = &*document as *const Document as usize;
        let (start, started) = mpsc::sync_channel(1);
        let (entered, disposing) = mpsc::sync_channel(1);
        let (release, held) = mpsc::sync_channel(1);
        let (observed, delivered) = mpsc::sync_channel(1);
        let progressed = Arc::new(AtomicUsize::new(0));
        let progress = progressed.clone();
        let (worker, mut receiver, shared) = harness_with_budget(
            &budget,
            || Ok(()),
            move |shared, inbox| {
                started.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
                shared.publish(DocumentResult::Indexed {
                    document: super::super::DocumentId(1),
                    revision: 0,
                    result: Ok(document),
                });
                progress.fetch_add(1, Ordering::SeqCst);
                let DocumentRequest::OpenRaw { document, revision, path, rows, generation } =
                    inbox.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap()
                else {
                    panic!("the same worker must receive the exact retained OpenRaw after retirement")
                };
                observed
                    .try_send((
                        document,
                        revision,
                        path.as_os_str().as_encoded_bytes().as_ptr() as usize,
                        rows,
                        generation,
                    ))
                    .unwrap();
                assert!(matches!(
                    inbox.recv_timeout(deadline.saturating_duration_since(Instant::now())),
                    Err(mpsc::RecvTimeoutError::Disconnected)
                ));
                shared.publish(DocumentResult::Stopped(Ok(())));
            },
        );
        let producer = lock(&shared.worker).as_ref().unwrap().thread().id();
        let worker_identity = Arc::as_ptr(&shared);
        *lock(&shared.retirement_disposal_gate) = Some(RetirementDisposalGate { entered, release: held, deadline });
        let mut view = crate::View::new(None);
        view.editor = None;
        let mut workspace = crate::Workspace::new(view).unwrap();
        assert!(workspace.replace_document_worker_for_transport_test(Some(worker)).is_none());
        start.try_send(()).unwrap();
        await_phase(&shared, |state| state.slot.is_some());
        let delivery = receiver.info().unwrap().delivery().unwrap();
        let first_error = if earlier_refusal {
            let mut foreign = crate::Workspace::new(crate::View::new(None)).unwrap();
            assert!(matches!(
                receiver.accept_workspace(&mut foreign, &budget, true),
                Err(DocumentResultSettlementReason::ConsumerRefused)
            ));
            let state = lock(&shared.state);
            assert!(!state.retire_requested, "a real unaccepted refusal has no successful retirement proof");
            assert!(matches!(state.commit_error.as_ref(), Some(WorkspaceDocumentAcceptError::ForeignWorkspace)));
            Some(state.commit_error.as_ref().unwrap() as *const DocumentCommitError as usize)
        } else {
            None
        };
        let update = receiver.accept_workspace(&mut workspace, &budget, true).unwrap().unwrap();
        drop(update);
        let observation = disposing.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
        assert_eq!(observation.producer, producer);
        assert_ne!(observation.producer, std::thread::current().id());
        assert_eq!(observation.delivery, delivery);
        assert_eq!(observation.residual_box, residual_box, "the original result Box stays in producer-owned disposal");
        assert_eq!(Arc::as_ptr(&shared), worker_identity);
        assert_eq!(workspace.editor().unwrap().document().source_epoch, source_epoch);
        let mut bytes = Vec::new();
        workspace.editor().unwrap().document().snapshot().write_to(&mut bytes).unwrap();
        assert_eq!(bytes, b"original source");
        {
            let state = lock(&shared.state);
            assert!(state.in_flight && state.retire_requested && state.transfer_active);
            assert!(state.slot.is_none(), "the owner is on the original producer, not in a second slot");
            assert_eq!(
                state.commit_error.as_ref().map(|error| error as *const DocumentCommitError as usize),
                first_error
            );
        }
        let worker = workspace.replace_document_worker_for_transport_test(None).unwrap();
        assert!(matches!(&worker.tx, RequestSender::Bounded(original) if Arc::ptr_eq(original, &shared)));
        let path = std::env::temp_dir().join("kwil-full-through-actual-disposal");
        let path_identity = path.as_os_str().as_encoded_bytes().as_ptr() as usize;
        let charged = budget.retained_bytes();
        let attempts = budget.allocation_attempts();
        let Err(TrySendError::Full(mut request)) = worker.try_submit(DocumentRequest::OpenRaw {
            document: super::super::DocumentId(9),
            revision: 0,
            path,
            rows: 3,
            generation: 41,
        }) else {
            panic!("actual successful producer disposal must return exact Full even with an earlier first error")
        };
        assert_eq!(progressed.load(Ordering::SeqCst), 0);
        assert_eq!(budget.retained_bytes(), charged);
        assert_eq!(budget.allocation_attempts(), attempts);
        let closed = |request: Box<DocumentRequest>| match worker.try_submit(*request) {
            Err(TrySendError::Disconnected(request)) => request,
            _ => panic!("failure/retry/private phase must remain closed during completed disposal"),
        };
        for phase in [ShutdownPhase::Drain, ShutdownPhase::SourcesReleased] {
            lock(&shared.requests).phase = phase;
            request = closed(request);
        }
        lock(&shared.requests).phase = ShutdownPhase::Live;
        lock(&shared.requests).failed = true;
        request = closed(request);
        lock(&shared.requests).failed = false;
        lock(&shared.state).closed = true;
        request = closed(request);
        lock(&shared.state).closed = false;
        lock(&shared.state).delivery_failure =
            Some(DocumentDeliveryFailure::Wake(io::Error::other("retained terminal wake")));
        request = closed(request);
        lock(&shared.state).delivery_failure = None;
        lock(&shared.state).retry_running = true;
        request = closed(request);
        lock(&shared.state).retry_running = false;
        let retry = RetryControl {
            deadline,
            delivery,
            generation: 1,
            action: DocumentProducerAction::RetryWrap,
            failure: None,
        };
        lock(&shared.state).retry = Some(retry);
        request = closed(request);
        lock(&shared.state).retry = None;
        lock(&shared.state).retry_acknowledgement_required = Some(retry);
        request = closed(request);
        lock(&shared.state).retry_acknowledgement_required = None;
        let Err(TrySendError::Full(request)) = worker.try_submit(*request) else {
            panic!("the original healthy completed retirement remains Full after the negative phase controls")
        };
        let DocumentRequest::OpenRaw { path, generation, .. } = &*request else { panic!("original request retained") };
        assert_eq!(path.as_os_str().as_encoded_bytes().as_ptr() as usize, path_identity);
        assert_eq!(*generation, 41);
        assert_eq!(
            lock(&shared.state).commit_error.as_ref().map(|error| error as *const DocumentCommitError as usize),
            first_error
        );
        assert!(!receiver.take_capacity_event(), "active disposal is not reusable capacity");
        release.try_send(()).unwrap();
        await_phase(&shared, |state| !state.in_flight);
        *lock(&shared.retirement_disposal_gate) = None;
        assert!(
            lock(&shared.state).commit_error.is_none(),
            "only the original producer retirement ACKs the earlier error"
        );
        assert!(receiver.take_capacity_event());
        assert!(!receiver.take_capacity_event());
        assert!(worker.try_submit(*request).is_ok());
        assert_eq!(
            delivered.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap(),
            (super::super::DocumentId(9), 0, path_identity, 3, 41)
        );
        assert_eq!(progressed.load(Ordering::SeqCst), 1);
        finish(receiver.stop(worker).unwrap(), |result| {
            assert!(matches!(result, DocumentResult::Stopped(Ok(()))));
            ControlFlow::Continue(())
        });
        assert_eq!(workspace.editor().unwrap().document().source_epoch, source_epoch);
        assert!(budget.peak_bytes() <= crate::visual_rows::MAX_SESSION_BYTES);
    }
}

fn stale_save_on_original_producer() -> (crate::Workspace, DocumentResultReceiver, Arc<Shared>, std::path::PathBuf) {
    use crate::workspace::workspace_document::commit::CommitFault;
    let root = std::env::temp_dir().join(format!(
        "kwil-producer-reset-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir(&root).unwrap();
    let budget = AllocationBudget::default();
    let (worker, mut receiver, shared) = harness_with_budget(
        &budget,
        || Ok(()),
        |shared, inbox| {
            let DocumentRequest::Save { document, request, .. } = inbox.recv_timeout(LIMIT).unwrap() else {
                panic!("fixture must save through the original bounded request lane")
            };
            let revision = request.revision;
            let saved = kwil_core_workspace::save_snapshot(request);
            shared.publish(DocumentResult::Saved { document, revision, result: saved });
            assert!(matches!(inbox.recv_timeout(LIMIT), Err(mpsc::RecvTimeoutError::Disconnected)));
            shared.publish(DocumentResult::Stopped(Ok(())));
        },
    );
    let mut workspace = crate::Workspace::new(crate::View::new(None)).unwrap();
    assert!(workspace.replace_document_worker_for_transport_test(Some(worker)).is_none());
    let editor = workspace.editor_mut().unwrap();
    editor.settings.editor.trim_trailing_whitespace_on_save = true;
    editor.insert_text("a  \nb\t\nlast ", Duration::ZERO).unwrap();
    workspace.save_document(workspace.active_id(), Some(root.join("saved.txt"))).unwrap();
    wait(&shared, |state| matches!(state.whole(), Some(DocumentResult::Saved { .. })));
    workspace.set_document_commit_fault_for_test(CommitFault::AfterSavePreparation);
    assert!(matches!(
        receiver.accept_workspace(&mut workspace, &budget, true),
        Err(DocumentResultSettlementReason::ConsumerRefused)
    ));
    {
        let state = lock(&shared.state);
        let Some(commit) = state.slot.as_ref().and_then(DocumentSlot::partial) else {
            panic!("original Prepared residual")
        };
        assert_eq!(commit.phase(), crate::workspace::workspace_document::commit::DocumentCommitPhase::Prepared);
        assert!(commit.prepublication_reset_kind().is_none(), "a pause alone is not a typed stale identity");
    }
    assert!(workspace.editor_mut().is_none(), "the Prepared transport phase must preserve the production edit gate");
    let editor = workspace.editor_mut_for_document_transport_test().unwrap();
    editor.set_cursors(vec![crate::Cursor::caret(4)], 0).unwrap();
    editor.set_viewport(1, 2);
    assert!(matches!(
        receiver.accept_workspace(&mut workspace, &budget, true),
        Err(DocumentResultSettlementReason::ConsumerRefused)
    ));
    assert!(matches!(lock(&shared.state).slot.as_ref().and_then(DocumentSlot::partial), Some(commit)
        if commit.prepublication_reset_kind() == Some(PrepublicationResetKind::SavePreparation)));
    (workspace, receiver, shared, root)
}

fn retained_save_error_identity(receiver: &DocumentResultReceiver) -> usize {
    let inspection = receiver.retained_commit_error();
    std::error::Error::source(inspection.retained_commit_error().unwrap()).unwrap().downcast_ref::<io::Error>().unwrap()
        as *const io::Error as usize
}

fn remove_reset_fixture(root: &std::path::Path) {
    assert!(root.is_absolute() && root.starts_with(std::env::temp_dir()));
    std::fs::remove_file(root.join("saved.txt")).unwrap();
    std::fs::remove_dir(root).unwrap();
}

#[test]
fn result_transport_prepublication_reset_requires_exact_ack_and_reprepares_same_saved_owner() {
    let (mut workspace, mut receiver, shared, root) = stale_save_on_original_producer();
    let budget = shared.budget.clone();
    let delivery = receiver.info().unwrap().delivery().unwrap();
    let first_error = retained_save_error_identity(&receiver);
    let producer = lock(&shared.worker).as_ref().unwrap().thread().id();
    let history = workspace.editor().unwrap().document().buffer.history_depth();
    let token = receiver.request_prepublication_reset(delivery, LIMIT).unwrap();
    wait(&shared, |state| state.retry_acknowledgement_required.is_some() && !state.retry_running);
    assert!(matches!(
        receiver.accept_workspace(&mut workspace, &budget, true),
        Err(DocumentResultSettlementReason::RetryUnavailable)
    ));
    assert_eq!(retained_save_error_identity(&receiver), first_error);
    assert_eq!(receiver.info().unwrap().delivery(), Some(delivery));
    assert!(!lock(&shared.requests).accepting);
    let control = token.control;
    let wrong = shared.admit_retry_token(RetryControl { generation: control.generation + 1, ..control }).unwrap();
    let refusal = receiver.acknowledge_prepublication_reset(&workspace, wrong).unwrap_err();
    assert_eq!(refusal.reason, DocumentResultSettlementReason::RetryUnavailable);
    assert_eq!(refusal.token.control.generation, control.generation + 1);
    drop(refusal);
    let foreign = crate::Workspace::new(crate::View::new(None)).unwrap();
    let refusal = receiver.acknowledge_prepublication_reset(&foreign, token).unwrap_err();
    assert_eq!(refusal.reason, DocumentResultSettlementReason::ConsumerRefused);
    assert_eq!(refusal.token.control, control);
    let token = refusal.token;
    // Reduced fake deadline, not a sleep: the exact ready identity is expired
    // in both control and test authority, then restored ONLY for fixture use.
    let late = RetryControl { deadline: Instant::now(), ..control };
    lock(&shared.state).retry_acknowledgement_required = Some(late);
    let late_token = shared.admit_retry_token(late).unwrap();
    let refusal = receiver.acknowledge_prepublication_reset(&workspace, late_token).unwrap_err();
    assert_eq!(refusal.reason, DocumentResultSettlementReason::TimedOut);
    assert_eq!(receiver.info().unwrap().delivery(), Some(delivery));
    drop(refusal);
    lock(&shared.state).retry_acknowledgement_required = Some(control);
    let duplicate = shared.admit_retry_token(control).unwrap();
    receiver.acknowledge_prepublication_reset(&workspace, token).unwrap();
    assert_eq!(retained_save_error_identity(&receiver), first_error, "reset ACK cannot acknowledge the first error");
    assert!(!lock(&shared.requests).accepting, "a Partial cannot reopen ordinary admission");
    let refusal = receiver.acknowledge_prepublication_reset(&workspace, duplicate).unwrap_err();
    assert_eq!(refusal.reason, DocumentResultSettlementReason::RetryUnavailable);
    drop(refusal);
    assert!(receiver.accept_workspace(&mut workspace, &budget, true).unwrap().is_some());
    wait(&shared, |state| !state.in_flight);
    assert!(receiver.retained_commit_error().retained_commit_error().is_none());
    assert_eq!(workspace.editor().unwrap().primary_cursor().head, 2);
    assert_eq!(workspace.editor().unwrap().document().buffer.history_depth().0, history.0 + 1);
    assert_eq!(lock(&shared.worker).as_ref().unwrap().thread().id(), producer);
    // Existing explicit OFF-UI fixture cleanup, with no new production lane.
    std::thread::scope(|scope| {
        scope
            .spawn(move || {
                let worker = workspace.replace_document_worker_for_transport_test(None).unwrap();
                drop(workspace);
                finish(receiver.stop(worker).unwrap(), |result| {
                    assert!(matches!(result, DocumentResult::Stopped(Ok(()))));
                    ControlFlow::Continue(())
                });
                remove_reset_fixture(&root);
            })
            .join()
            .unwrap()
    });
}

#[test]
fn result_transport_prepublication_reset_panic_restores_original_partial_and_stays_sticky() {
    use crate::workspace::workspace_document::commit::{CommitFault, DocumentCommitPhase};
    let (mut workspace, mut receiver, shared, root) = stale_save_on_original_producer();
    let delivery = receiver.info().unwrap().delivery().unwrap();
    let first_error = retained_save_error_identity(&receiver);
    {
        let mut state = lock(&shared.state);
        let Some(commit) = state.slot.as_mut().and_then(DocumentSlot::partial_mut) else { unreachable!() };
        commit.set_fault(CommitFault::ResetPrepublicationPanic);
    }
    let token = receiver.request_prepublication_reset(delivery, LIMIT).unwrap();
    wait(&shared, |state| state.commit_panic.is_some());
    assert_eq!(receiver.status().failure, Some(DocumentResultSettlementReason::CommitPanicked));
    assert_eq!(receiver.info().unwrap().delivery(), Some(delivery));
    assert_eq!(retained_save_error_identity(&receiver), first_error);
    let panic_identity = {
        let inspection = receiver.retained_commit_error();
        let commit = inspection.commit().unwrap();
        assert_eq!(commit.phase(), DocumentCommitPhase::Prepared);
        assert_eq!(commit.installed_fields(), 0);
        assert_eq!(commit.prepublication_reset_kind(), Some(PrepublicationResetKind::SavePreparation));
        inspection.commit_panic().unwrap() as *const (dyn Any + Send) as *const () as usize
    };
    let refusal = receiver.acknowledge_prepublication_reset(&workspace, token).unwrap_err();
    assert_eq!(refusal.reason, DocumentResultSettlementReason::CommitPanicked);
    let worker = workspace.replace_document_worker_for_transport_test(None).unwrap();
    let mut ticket = receiver.stop(worker).unwrap();
    assert!(ticket.acknowledge_delivery_failure().is_none());
    assert_eq!(ticket.status().failure, Some(DocumentResultSettlementReason::CommitPanicked));
    assert_eq!(
        ticket.inspect().commit_panic().unwrap() as *const (dyn Any + Send) as *const () as usize,
        panic_identity
    );
    // PRIVATE teardown only AFTER the sticky failure/owner assertions. There
    // is no shipping commit-panic ACK or replay authority. Off-UI cleanup asks
    // the SAME producer to retire its original residual, never an owned copy.
    std::thread::scope(|scope| {
        scope
            .spawn(move || {
                let panic = {
                    let mut state = lock(&shared.state);
                    state.retry_acknowledgement_required = None;
                    let panic = state.commit_panic.take();
                    lock(&shared.requests).failed = false;
                    panic
                };
                drop(panic);
                drop(refusal);
                let transfer = {
                    let mut state = lock(&shared.state);
                    let value = state.slot.take();
                    state.transfer_active = true;
                    DocumentTransferGuard { shared: shared.clone(), value, delivery }
                };
                transfer.restore(true);
                drop(workspace);
                finish(ticket, |result| {
                    assert!(matches!(result, DocumentResult::Stopped(Ok(()))));
                    ControlFlow::Continue(())
                });
                remove_reset_fixture(&root);
            })
            .join()
            .unwrap()
    });
}

#[test]
fn result_transport_prepublication_generation_exhaustion_retains_original_stale_partial() {
    let (mut workspace, mut receiver, shared, root) = stale_save_on_original_producer();
    let budget = shared.budget.clone();
    let delivery = receiver.info().unwrap().delivery().unwrap();
    let first = retained_save_error_identity(&receiver);
    lock(&shared.state).counter_limit = 0;
    let charged = budget.retained_bytes();
    assert_eq!(
        receiver.request_prepublication_reset(delivery, LIMIT).unwrap_err(),
        DocumentResultSettlementReason::CounterExhausted
    );
    assert_eq!(budget.retained_bytes(), charged, "exhaustion cannot construct an unadmitted token");
    assert_eq!(retained_save_error_identity(&receiver), first);
    assert!(receiver.info().is_none());
    assert!(matches!(lock(&shared.state).slot.as_ref().and_then(DocumentSlot::partial), Some(commit)
        if commit.prepublication_reset_kind() == Some(PrepublicationResetKind::SavePreparation)));
    {
        let gate = lock(&shared.requests);
        assert!(gate.failed && gate.sender.is_some());
    }
    // Lift only the reduced PRIVATE fixture ceiling after proving fail-stop;
    // production has no counter-reset acknowledgement or new worker fallback.
    release_exhaustion_fixture(&shared);
    let token = receiver.request_prepublication_reset(delivery, LIMIT).unwrap();
    wait(&shared, |state| state.retry_acknowledgement_required.is_some());
    receiver.acknowledge_prepublication_reset(&workspace, token).unwrap();
    assert!(receiver.accept_workspace(&mut workspace, &budget, true).unwrap().is_some());
    wait(&shared, |state| !state.in_flight);
    std::thread::scope(|scope| {
        scope
            .spawn(move || {
                let worker = workspace.replace_document_worker_for_transport_test(None).unwrap();
                drop(workspace);
                finish(receiver.stop(worker).unwrap(), |_| ControlFlow::Continue(()));
                remove_reset_fixture(&root);
            })
            .join()
            .unwrap()
    });
}

#[test]
fn result_transport_invalidated_marker_retirement_preserves_empty_capacity_before_wait() {
    let (release, held) = mpsc::sync_channel(1);
    let (worker, receiver, shared) = harness(
        || Ok(()),
        move |shared, inbox| {
            let DocumentRequest::SourceBarrier(marker) = inbox.recv_timeout(LIMIT).unwrap() else {
                panic!("first marker must use the original lane")
            };
            shared.publish(DocumentResult::SourceBarrier(DocumentBarrierReceipt::reached(marker)));
            // Hold this SAME producer after retirement and before the next
            // request, proving the next wake observes EMPTY capacity alone.
            held.recv_timeout(LIMIT).unwrap();
            assert!(matches!(inbox.recv_timeout(LIMIT).unwrap(), DocumentRequest::SessionTimer));
            let DocumentRequest::SourceBarrier(marker) = inbox.recv_timeout(LIMIT).unwrap() else {
                panic!("replacement marker must follow the accepted continuation")
            };
            shared.publish(DocumentResult::SourceBarrier(DocumentBarrierReceipt::reached(marker)));
            assert!(matches!(inbox.recv_timeout(LIMIT), Err(mpsc::RecvTimeoutError::Disconnected)));
            shared.publish(DocumentResult::Stopped(Ok(())));
        },
    );
    let (mut bound, mut permit) = receiver.begin_bound(&worker).unwrap();
    let old = permit.request_barrier().unwrap();
    wait(&shared, |state| matches!(state.whole(), Some(DocumentResult::SourceBarrier(_))));
    let deadline = Instant::now() + LIMIT;
    bound.wait_until(deadline).unwrap();
    let receipt = match lock(&shared.state).whole() {
        Some(DocumentResult::SourceBarrier(receipt)) => *receipt,
        _ => panic!("old marker stays in the original slot"),
    };
    assert_eq!(receipt.id(), old);
    assert!(matches!(permit.submit(DocumentRequest::SessionTimer), ControlFlow::Continue(())));
    assert_eq!(permit.seal_sources(&receipt), Err(DocumentResultSettlementReason::SourceBarrierRequired));
    assert_eq!(permit.request_barrier(), Err(ContinuationRefusal::Busy));
    let transfer = {
        let mut state = lock(&shared.state);
        let value = state.slot.take();
        state.transfer_active = true;
        DocumentTransferGuard {
            shared: shared.clone(),
            value,
            delivery: DocumentDeliveryId { worker: shared.worker_id, delivery: state.delivery },
        }
    };
    transfer.restore(true);
    wait(&shared, |state| !state.in_flight);
    assert!(lock(&shared.state).slot.is_none());
    assert!(lock(&shared.requests).outstanding_barrier.is_none());
    // Retirement raced BEFORE wait. An expired observation avoids timing
    // assumptions: the old predicate returns TimedOut despite the event.
    bound.wait_until(Instant::now()).unwrap();
    assert_eq!(bound.wait_until(Instant::now()), Err(DocumentResultSettlementReason::TimedOut));
    let new = permit.request_barrier().unwrap();
    assert_ne!(old, new);
    assert!(new.source_epoch > old.source_epoch);
    release.try_send(()).unwrap();
    wait(&shared, |state| matches!(state.whole(), Some(DocumentResult::SourceBarrier(receipt)) if receipt.id() == new));
    let Some(DocumentResult::SourceBarrier(receipt)) = bound.receiver.try_recv(true).unwrap() else {
        panic!("new marker must be delivered on the same slot")
    };
    permit.seal_sources(&receipt).unwrap();
    bound.close_after_source_release(&permit).unwrap();
    finish(bound.into_legacy_for_test(worker), |result| {
        assert!(matches!(result, DocumentResult::Stopped(Ok(()))));
        ControlFlow::Continue(())
    });
}

#[test]
fn result_transport_sealed_journal_admits_recipe_before_moving_pending_owner() {
    const BUDGET: usize = 16 * 1024 * 1024;
    let budget = AllocationBudget::new(BUDGET);
    let (shared, receiver) = Shared::new(&budget, || Ok(())).unwrap();
    let (sender, inbox) = mpsc::sync_channel(16);
    let worker = DocumentWorker {
        tx: RequestSender::new(sender, Some(&shared)),
        journal_capacity: Arc::new(AtomicUsize::new(16)),
        journal_controls: Arc::new(super::super::JournalControlCredits::new()),
        save_destinations: Default::default(),
    };
    let mut sink =
        worker.journal_sink(super::super::DocumentId(2), None, 1, 0, super::super::document_policy(&Document::new()));
    let (capture, captured) = mpsc::sync_channel(1);
    let mut editor = Editor::new(Document::new());
    editor
        .set_edit_observer(Some(Box::new(move |receipt| {
            assert!(capture.try_send(receipt).is_ok());
            Ok(())
        })))
        .unwrap();
    editor.insert_text("x", Duration::ZERO).unwrap();
    let mut pending = Some(captured.recv_timeout(LIMIT).unwrap());
    let recipe = pending.as_ref().unwrap().recipe_metadata_bytes().unwrap();
    assert!(recipe > 0, "fixture must retain an explicit recipe allocation");
    let sequence = pending.as_ref().unwrap().sequence;
    let base = budget.retained_bytes();
    let header = size_of::<SealedJournalRequest>() + size_of::<crate::visual_rows::MetadataReservation>();
    // Header alone fits; omitting the recipe charge makes this refusal fail.
    let blocker = budget.reserve(BUDGET - base - header).unwrap();
    assert!(matches!(sink.flush(&mut pending, None), Err(crate::editor::ReceiptDeliveryRefusal::Admission)));
    assert_eq!(pending.as_ref().unwrap().sequence, sequence);
    assert_eq!(worker.journal_capacity.load(Ordering::Acquire), 16);
    assert!(matches!(inbox.try_recv(), Err(mpsc::TryRecvError::Empty)));
    drop(blocker);
    assert_eq!(budget.retained_bytes(), base);
    assert!(sink.flush(&mut pending, None).is_ok());
    assert!(pending.is_none());
    drop(editor);
    let packet = inbox.recv_timeout(LIMIT).unwrap();
    let DocumentRequest::SealedJournal(packet) = packet else { panic!("same admitted packet") };
    assert_eq!(packet.receipt.sequence, sequence);
    assert_eq!(packet.charge.reserved_bytes(), header + recipe);
    assert_eq!(budget.retained_bytes(), base + header + recipe, "recipe stays charged after Editor releases it");
    drop(packet);
    assert_eq!(budget.retained_bytes(), base);
    drop((sink, receiver, worker, shared));
}

#[test]
fn result_transport_journal_authority_distinguishes_live_fence_from_closed_and_wrong_permit() {
    use crate::editor::ReceiptDeliveryRefusal as Refusal;
    let (shared, receiver) = Shared::new(&AllocationBudget::default(), || Ok(())).unwrap();
    let (sender, _inbox) = mpsc::sync_channel(16);
    let _sender = RequestSender::new(sender, Some(&shared));
    let (foreign, foreign_receiver) = Shared::new(&AllocationBudget::default(), || Ok(())).unwrap();
    let controls = Arc::new(super::super::JournalControlCredits::new());
    let permit =
        DocumentContinuationPermit { shared: Arc::downgrade(&shared), generation: 1, controls: controls.clone() };
    let stale =
        DocumentContinuationPermit { shared: Arc::downgrade(&shared), generation: 0, controls: controls.clone() };
    let wrong = DocumentContinuationPermit { shared: Arc::downgrade(&foreign), generation: 1, controls };
    let mut gate = lock(&shared.requests);
    gate.generation = 1;
    assert_eq!(journal_authority(&shared, &gate, None), Ok(()));
    gate.accepting = false;
    assert_eq!(journal_authority(&shared, &gate, None), Err(Refusal::Full));
    assert_eq!(journal_authority(&shared, &gate, Some(&permit)), Err(Refusal::WrongPermit));
    assert_eq!(journal_authority(&shared, &gate, Some(&wrong)), Err(Refusal::WrongPermit));
    gate.phase = ShutdownPhase::Drain;
    assert_eq!(journal_authority(&shared, &gate, None), Err(Refusal::Closed));
    assert_eq!(journal_authority(&shared, &gate, Some(&permit)), Ok(()));
    assert_eq!(journal_authority(&shared, &gate, Some(&stale)), Err(Refusal::WrongPermit));
    assert_eq!(journal_authority(&shared, &gate, Some(&wrong)), Err(Refusal::WrongPermit));
    gate.phase = ShutdownPhase::SourcesReleased;
    assert_eq!(journal_authority(&shared, &gate, None), Err(Refusal::Closed));
    assert_eq!(journal_authority(&shared, &gate, Some(&permit)), Err(Refusal::WrongPermit));
    gate.phase = ShutdownPhase::Closed;
    assert_eq!(journal_authority(&shared, &gate, None), Err(Refusal::Closed));
    assert_eq!(journal_authority(&shared, &gate, Some(&permit)), Err(Refusal::Closed));
    gate.phase = ShutdownPhase::Live;
    gate.failed = true;
    assert_eq!(journal_authority(&shared, &gate, None), Err(Refusal::Closed));
    assert_eq!(journal_authority(&shared, &gate, Some(&wrong)), Err(Refusal::Closed));
    gate.failed = false;
    let sender = gate.sender.take();
    assert_eq!(journal_authority(&shared, &gate, None), Err(Refusal::Closed));
    assert_eq!(journal_authority(&shared, &gate, Some(&permit)), Err(Refusal::Closed));
    drop(gate);
    drop(sender);
    drop((receiver, foreign_receiver));
}

#[test]
fn result_transport_live_journal_fence_preserves_receipt_then_retries_once_and_disconnect_stays_closed() {
    use crate::editor::ReceiptDeliveryRefusal as Refusal;
    let budget = AllocationBudget::default();
    let baseline = budget.retained_bytes();
    let (shared, receiver) = Shared::new(&budget, || Ok(())).unwrap();
    let (sender, inbox) = mpsc::sync_channel(16);
    let worker = DocumentWorker {
        tx: RequestSender::new(sender, Some(&shared)),
        journal_capacity: Arc::new(AtomicUsize::new(16)),
        journal_controls: Arc::new(super::super::JournalControlCredits::new()),
        save_destinations: Default::default(),
    };
    let mut sink =
        worker.journal_sink(super::super::DocumentId(2), None, 1, 0, super::super::document_policy(&Document::new()));
    let (capture, captured) = mpsc::sync_channel(1);
    let mut editor = Editor::new(Document::new());
    editor
        .set_edit_observer(Some(Box::new(move |receipt| {
            assert!(capture.try_send(receipt).is_ok());
            Ok(())
        })))
        .unwrap();
    editor.insert_text("x", Duration::ZERO).unwrap();
    let mut pending = Some(captured.recv_timeout(LIMIT).unwrap());
    let identity = pending.as_ref().unwrap() as *const crate::EditReceipt as usize;
    let sequence = pending.as_ref().unwrap().sequence;
    let revision = pending.as_ref().unwrap().after_revision;
    lock(&shared.requests).accepting = false;
    let retained = budget.retained_bytes();
    let attempts = budget.allocation_attempts();
    assert_eq!(sink.flush(&mut pending, None), Err(Refusal::Full));
    assert_eq!(pending.as_ref().unwrap() as *const crate::EditReceipt as usize, identity);
    assert_eq!(pending.as_ref().unwrap().sequence, sequence);
    assert_eq!(pending.as_ref().unwrap().after_revision, revision);
    assert_eq!(budget.retained_bytes(), retained);
    assert_eq!(budget.allocation_attempts(), attempts, "Live fence must refuse before packet admission/allocation");
    assert_eq!(worker.journal_capacity.load(Ordering::Acquire), 16);
    assert_eq!(lock(&shared.requests).source_epoch, 0);
    assert!(matches!(inbox.try_recv(), Err(mpsc::TryRecvError::Empty)));
    lock(&shared.requests).accepting = true;
    assert_eq!(sink.flush(&mut pending, None), Ok(()));
    assert!(pending.is_none());
    let DocumentRequest::SealedJournal(packet) = inbox.recv_timeout(LIMIT).unwrap() else {
        panic!("same journal lane")
    };
    assert_eq!(packet.receipt.sequence, sequence);
    assert_eq!(packet.receipt.after_revision, revision);
    assert_eq!(sink.flush(&mut pending, None), Ok(()));
    assert!(matches!(inbox.try_recv(), Err(mpsc::TryRecvError::Empty)), "retry delivered the receipt exactly once");
    assert_eq!(worker.journal_capacity.load(Ordering::Acquire), 15);
    assert_eq!(lock(&shared.requests).source_epoch, 1);
    editor.insert_text("y", Duration::ZERO).unwrap();
    pending = Some(captured.recv_timeout(LIMIT).unwrap());
    let sequence = pending.as_ref().unwrap().sequence;
    let retained = budget.retained_bytes();
    drop(inbox);
    assert_eq!(sink.flush(&mut pending, None), Err(Refusal::Closed));
    assert_eq!(pending.as_ref().unwrap().sequence, sequence);
    assert_eq!(worker.journal_capacity.load(Ordering::Acquire), 15, "real disconnect refunds its attempted credit");
    assert_eq!(lock(&shared.requests).source_epoch, 1);
    assert_eq!(budget.retained_bytes(), retained, "real disconnect restores receipt and packet reservation");
    // Recipe/snapshot owners retire OFF UI, including the exact refused
    // second receipt and first accepted packet; no source inspection occurs.
    std::thread::scope(|scope| {
        scope
            .spawn(move || {
                drop((pending, packet, editor, sink, receiver, worker, shared, captured));
            })
            .join()
            .unwrap()
    });
    assert_eq!(budget.retained_bytes(), baseline);
}

// A private fixture teardown, never a production ACK/reset API. Raise the
// reduced ceiling after proving fail-stop, preserving the original owners so
// the SAME producer can clean up without a detached or permanently held test.
fn release_exhaustion_fixture(shared: &Shared) {
    let mut state = lock(&shared.state);
    assert!(state.counter_exhausted);
    state.counter_limit = u64::MAX;
    state.counter_exhausted = false;
    let mut gate = lock(&shared.requests);
    gate.failed = false;
    gate.accepting = false;
    drop(gate);
    drop(state);
    shared.changed.notify_all();
}

#[test]
fn result_transport_delivery_exhaustion_retains_exact_owner_without_repeated_identity() {
    let (dropped, drops) = mpsc::sync_channel(1);
    let inspected = Arc::new(AtomicUsize::new(0));
    let (result, identity) = loan(OwnedProbe { dropped, inspected: inspected.clone() });
    let (release, held) = mpsc::sync_channel(1);
    let (worker, receiver, shared) = harness(
        || Ok(()),
        move |shared, inbox| {
            held.recv_timeout(LIMIT).unwrap();
            shared.publish(result);
            assert!(matches!(inbox.recv_timeout(LIMIT), Err(mpsc::RecvTimeoutError::Disconnected)));
            shared.publish(DocumentResult::Stopped(Ok(())));
        },
    );
    let producer = lock(&shared.worker).as_ref().unwrap().thread().id();
    {
        let mut state = lock(&shared.state);
        state.counter_limit = 1;
        state.delivery = 1;
    }
    release.try_send(()).unwrap();
    wait(&shared, |state| state.counter_exhausted);
    assert_eq!(receiver.status().failure, Some(DocumentResultSettlementReason::CounterExhausted));
    assert!(receiver.info().is_none(), "the failed new delivery cannot reuse the preceding identity");
    assert!(matches!(worker.try_submit(DocumentRequest::FlushJournals), Err(TrySendError::Disconnected(_))));
    let mut ticket = receiver.stop(worker).unwrap();
    assert!(ticket.acknowledge_delivery_failure().is_none());
    let token = wrap_token_for_test(&shared, 0);
    assert_eq!(
        ticket.acknowledge_wrap_retry(token).unwrap_err().reason,
        DocumentResultSettlementReason::CounterExhausted
    );
    assert!(lock(&shared.requests).failed);
    assert!(lock(&shared.requests).sender.is_some(), "even close retains the fail-stopped sender");
    let failure = ticket.settle(LIMIT, |_| panic!("exhaustion cannot consume the original owner")).unwrap_err();
    assert_eq!(failure.reason, DocumentResultSettlementReason::CounterExhausted);
    {
        let inspection = failure.ticket.inspect();
        assert!(
            matches!(inspection.pending(), Some(DocumentResult::Replaced { editor, .. }) if &**editor as *const Editor as usize == identity)
        );
    }
    assert!(matches!(drops.try_recv(), Err(mpsc::TryRecvError::Empty)));
    let mut ticket = failure.ticket;
    release_exhaustion_fixture(&shared);
    let transfer = {
        let mut state = lock(&shared.state);
        let value = state.slot.take();
        state.transfer_active = true;
        DocumentTransferGuard {
            shared: shared.clone(),
            value,
            delivery: DocumentDeliveryId { worker: shared.worker_id, delivery: state.delivery },
        }
    };
    transfer.restore(true);
    ticket.receiver.close();
    wait(&shared, |state| !state.in_flight);
    assert_eq!(drops.recv_timeout(LIMIT).unwrap(), producer, "only the original producer retires the source owner");
    assert_eq!(inspected.load(Ordering::SeqCst), 0);
    finish(ticket, |result| {
        assert!(matches!(result, DocumentResult::Stopped(Ok(()))));
        ControlFlow::Continue(())
    });
}

#[test]
fn result_transport_capacity_exhaustion_retains_completed_partial_before_retirement() {
    let budget = AllocationBudget::default();
    let (release, held) = mpsc::sync_channel(1);
    let (worker, mut receiver, shared) = harness_with_budget(
        &budget,
        || Ok(()),
        move |shared, inbox| {
            held.recv_timeout(LIMIT).unwrap();
            shared.publish(DocumentResult::CapacityAvailable);
            assert!(matches!(inbox.recv_timeout(LIMIT), Err(mpsc::RecvTimeoutError::Disconnected)));
            shared.publish(DocumentResult::Stopped(Ok(())));
        },
    );
    let mut workspace = crate::Workspace::new(crate::View::new(None)).unwrap();
    assert!(workspace.replace_document_worker_for_transport_test(Some(worker)).is_none());
    {
        let mut state = lock(&shared.state);
        state.counter_limit = 1;
        state.capacity_generation = 1;
    }
    release.try_send(()).unwrap();
    wait(&shared, |state| state.slot.is_some());
    assert!(receiver.accept_workspace(&mut workspace, &budget, true).unwrap().is_some());
    wait(&shared, |state| state.counter_exhausted);
    assert!(receiver.info().is_none());
    {
        let state = lock(&shared.state);
        assert!(matches!(state.slot.as_ref().and_then(DocumentSlot::partial), Some(commit) if commit.is_complete()));
        assert!(state.in_flight && state.retire_requested && !state.transfer_active);
        assert_eq!(state.capacity_generation, 1, "exhaustion never silently saturates or wraps");
    }
    let worker = workspace.replace_document_worker_for_transport_test(None).unwrap();
    let failure = receiver.stop(worker).unwrap().settle(LIMIT, |_| panic!("partial must remain retained")).unwrap_err();
    assert_eq!(failure.reason, DocumentResultSettlementReason::CounterExhausted);
    assert!(failure.ticket.inspect().commit().unwrap().is_complete());
    assert!(lock(&shared.requests).sender.is_some());
    release_exhaustion_fixture(&shared);
    let mut ticket = failure.ticket;
    ticket.receiver.close();
    wait(&shared, |state| !state.in_flight);
    finish(ticket, |result| {
        assert!(matches!(result, DocumentResult::Stopped(Ok(()))));
        ControlFlow::Continue(())
    });
}

#[test]
fn result_transport_capacity_exhaustion_retains_the_same_accepted_request_frame() {
    let progressed = Arc::new(AtomicUsize::new(0));
    let progress = progressed.clone();
    let (worker, receiver, shared) = harness(
        || Ok(()),
        move |shared, inbox| {
            let request = inbox.recv_timeout(LIMIT).unwrap();
            shared.request_capacity_available();
            assert!(matches!(request, DocumentRequest::ReleaseReopen { generation: 59, .. }));
            progress.fetch_add(1, Ordering::SeqCst);
            assert!(matches!(inbox.recv_timeout(LIMIT), Err(mpsc::RecvTimeoutError::Disconnected)));
            shared.publish(DocumentResult::Stopped(Ok(())));
        },
    );
    lock(&shared.state).counter_limit = 0;
    assert!(
        worker
            .try_submit(DocumentRequest::ReleaseReopen { document: super::super::DocumentId(1), generation: 59 })
            .is_ok()
    );
    wait(&shared, |state| state.counter_exhausted);
    assert_eq!(receiver.status().failure, Some(DocumentResultSettlementReason::CounterExhausted));
    assert!(receiver.info().is_none());
    assert_eq!(progressed.load(Ordering::SeqCst), 0, "failure cannot run or lose the admitted request");
    let failure = receiver.stop(worker).unwrap().settle(LIMIT, |_| panic!("no result was produced")).unwrap_err();
    assert_eq!(failure.reason, DocumentResultSettlementReason::CounterExhausted);
    assert!(lock(&shared.requests).sender.is_some());
    release_exhaustion_fixture(&shared);
    let mut ticket = failure.ticket;
    ticket.receiver.close();
    finish(ticket, |result| {
        assert!(matches!(result, DocumentResult::Stopped(Ok(()))));
        ControlFlow::Continue(())
    });
    assert_eq!(progressed.load(Ordering::SeqCst), 1);
}

#[test]
fn result_transport_inline_slot_wrong_tag_retains_exact_loan_until_original_producer_retirement() {
    use crate::workspace::workspace_document::commit::DocumentCommitPhase;
    let ui = std::thread::current().id();
    let (dropped, drops) = mpsc::sync_channel(1);
    let inspected = Arc::new(AtomicUsize::new(0));
    let (result, identity) = loan(OwnedProbe { dropped, inspected: inspected.clone() });
    let budget = AllocationBudget::new(crate::visual_rows::MAX_SESSION_BYTES);
    let (worker, mut receiver, shared) = harness_with_budget(
        &budget,
        || Ok(()),
        move |shared, inbox| {
            shared.publish(result);
            assert!(matches!(inbox.recv_timeout(LIMIT), Err(mpsc::RecvTimeoutError::Disconnected)));
            shared.publish(DocumentResult::Stopped(Ok(())));
        },
    );
    let producer = lock(&shared.worker).as_ref().unwrap().thread().id();
    wait(&shared, |state| state.slot.is_some());
    let charged = budget.retained_bytes();
    let mut transfer = {
        let mut state = lock(&shared.state);
        let value = state.slot.take();
        state.transfer_active = true;
        DocumentTransferGuard {
            shared: shared.clone(),
            value,
            delivery: DocumentDeliveryId { worker: shared.worker_id, delivery: state.delivery },
        }
    };
    let slot = transfer.slot_mut();
    assert!(slot.partial().is_none());
    let original = slot.whole().unwrap() as *const DocumentResult;
    let commit = slot.promote();
    assert_eq!(commit.phase(), DocumentCommitPhase::Received);
    assert_eq!(commit.installed_fields(), 0);
    assert_eq!(commit.pending().unwrap() as *const DocumentResult, original, "promotion moves no original result");
    let promoted = commit as *const DocumentCommit;
    assert_eq!(slot.promote() as *const DocumentCommit, promoted, "promotion never reconstructs the commit");
    assert!(slot.whole().is_none(), "Received phase does not undo the logical Partial tag");
    let (slot, refused) = match transfer.value.take().unwrap().into_whole() {
        ControlFlow::Break(slot) => (slot, true),
        ControlFlow::Continue(result) => (DocumentSlot::new(result), false),
    };
    transfer.value = Some(slot);
    assert!(refused, "wrong-tag extraction must return the exact carrier");
    transfer.restore(false);
    assert!(matches!(
        receiver.try_recv(true),
        Err(DocumentResultReceiveError::Failed(DocumentResultSettlementReason::ConsumerRefused))
    ));
    {
        let state = lock(&shared.state);
        assert!(state.in_flight && !state.transfer_active && !state.retire_requested);
        let commit = state.slot.as_ref().and_then(DocumentSlot::partial).unwrap();
        assert_eq!(commit.phase(), DocumentCommitPhase::Received);
        assert_eq!(commit.installed_fields(), 0);
        let Some(DocumentResult::Replaced { editor, operation, .. }) = commit.pending() else {
            panic!("wrong-tag refusal must retain the original loan")
        };
        assert_eq!(&**editor as *const Editor as usize, identity);
        assert_eq!(*operation, 19);
    }
    assert_eq!(budget.retained_bytes(), charged, "carrier and refusal overlap were admitted before publication");
    assert_eq!(inspected.load(Ordering::SeqCst), 0);
    assert!(matches!(drops.try_recv(), Err(mpsc::TryRecvError::Empty)));
    // Explicit test-only retirement of the proven retained carrier. The SAME
    // publishing producer destroys its original loan; no UI callback takes it.
    let transfer = {
        let mut state = lock(&shared.state);
        let value = state.slot.take();
        state.transfer_active = true;
        DocumentTransferGuard {
            shared: shared.clone(),
            value,
            delivery: DocumentDeliveryId { worker: shared.worker_id, delivery: state.delivery },
        }
    };
    transfer.restore(true);
    assert_eq!(drops.recv_timeout(LIMIT).unwrap(), producer);
    assert_ne!(producer, ui);
    wait(&shared, |state| !state.in_flight);
    finish(receiver.stop(worker).unwrap(), |result| {
        assert!(matches!(result, DocumentResult::Stopped(Ok(()))));
        ControlFlow::Continue(())
    });
    assert_eq!(inspected.load(Ordering::SeqCst), 0);
}

#[test]
fn result_transport_saturated_slot_admission_and_close_preserve_exact_loan_without_ui_work() {
    let ui = std::thread::current().id();
    let (dropped, drops) = mpsc::sync_channel(1);
    let inspected = Arc::new(AtomicUsize::new(0));
    let (result, identity) = loan(OwnedProbe { dropped, inspected: inspected.clone() });
    let progressed = Arc::new(AtomicUsize::new(0));
    let progress = progressed.clone();
    let (worker, mut receiver, shared) = harness(
        || Ok(()),
        move |shared, _| {
            shared.publish(result);
            progress.fetch_add(1, Ordering::SeqCst);
            shared.publish(DocumentResult::Stopped(Ok(())));
        },
    );
    wait(&shared, |state| state.slot.is_some());
    assert!(matches!(receiver.try_recv(false), Err(DocumentResultReceiveError::AdmissionRequired)));
    assert_eq!(progressed.load(Ordering::SeqCst), 0, "full slot must stop further production");
    assert_eq!(inspected.load(Ordering::SeqCst), 0, "status/admission must not inspect source payloads");
    assert!(matches!(drops.try_recv(), Err(mpsc::TryRecvError::Empty)));
    receiver.close();
    assert!(matches!(receiver.try_recv(true), Err(DocumentResultReceiveError::Closed)));
    let ticket = receiver.stop(worker).unwrap();
    let failure = ticket.settle(LIMIT, ControlFlow::Break).unwrap_err();
    assert_eq!(failure.reason, DocumentResultSettlementReason::ConsumerRefused);
    assert_eq!(progressed.load(Ordering::SeqCst), 0, "taking for a refused callback is NOT capacity");
    match failure.ticket.inspect().pending().unwrap() {
        DocumentResult::Replaced { editor, operation, .. } => {
            assert_eq!(&**editor as *const Editor as usize, identity);
            assert_eq!(*operation, 19);
        }
        _ => panic!("refusal lost the accepted loan"),
    }
    std::thread::spawn(move || {
        finish(failure.ticket, |result| {
            drop(result);
            ControlFlow::Continue(())
        })
    })
    .join()
    .unwrap();
    assert_ne!(drops.recv_timeout(LIMIT).unwrap(), ui, "source-bearing owner may only retire OFF UI");
    assert_eq!(inspected.load(Ordering::SeqCst), 0);
    assert_eq!(progressed.load(Ordering::SeqCst), 1);
}

#[test]
fn result_transport_wake_error_keeps_original_typed_error_and_pending_owner_until_ack() {
    #[derive(Debug)]
    struct WakeError(u64);
    impl fmt::Display for WakeError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "wake {}", self.0)
        }
    }
    impl std::error::Error for WakeError {}
    let progressed = Arc::new(AtomicUsize::new(0));
    let progress = progressed.clone();
    let (worker, mut receiver, shared) = harness(
        || Err(io::Error::other(WakeError(91))),
        move |shared, _| {
            shared.publish(DocumentResult::CapacityAvailable);
            progress.fetch_add(1, Ordering::SeqCst);
            shared.publish(DocumentResult::Stopped(Ok(())));
        },
    );
    wait(&shared, |state| state.delivery_failure.is_some());
    assert!(matches!(
        receiver.try_recv(true),
        Err(DocumentResultReceiveError::Failed(DocumentResultSettlementReason::WakeFailed))
    ));
    assert!(matches!(worker.try_submit(DocumentRequest::FlushJournals), Err(TrySendError::Disconnected(_))));
    let ticket = receiver.stop(worker).unwrap();
    let failure = ticket.settle(LIMIT, |_| panic!("sticky failure must stop the consumer")).unwrap_err();
    assert_eq!(failure.reason, DocumentResultSettlementReason::WakeFailed);
    assert_eq!(progressed.load(Ordering::SeqCst), 0);
    {
        let inspection = failure.ticket.inspect();
        assert!(matches!(inspection.pending(), Some(DocumentResult::CapacityAvailable)));
        let Some(DocumentDeliveryFailure::Wake(error)) = inspection.delivery_failure() else {
            panic!("lost wake error")
        };
        assert_eq!(error.get_ref().unwrap().downcast_ref::<WakeError>().unwrap().0, 91);
    }
    let identity = Arc::as_ptr(&failure.ticket.receiver.shared);
    let again = failure.ticket.settle(LIMIT, |_| ControlFlow::Continue(())).unwrap_err();
    assert_eq!(again.reason, DocumentResultSettlementReason::WakeFailed);
    assert_eq!(Arc::as_ptr(&again.ticket.receiver.shared), identity);
    let mut ticket = again.ticket;
    assert!(matches!(ticket.acknowledge_delivery_failure(), Some(DocumentDeliveryFailure::Wake(_))));
    finish(ticket, |_| ControlFlow::Continue(()));
    assert_eq!(progressed.load(Ordering::SeqCst), 1);
}

#[test]
fn result_transport_receiver_disconnect_is_recoverable_without_ui_drop_or_detach() {
    let (worker, receiver, shared) = harness(
        || Ok(()),
        |shared, _| {
            shared.publish(DocumentResult::CapacityAvailable);
            shared.publish(DocumentResult::Stopped(Ok(())));
        },
    );
    wait(&shared, |state| state.slot.is_some());
    drop(receiver);
    assert!(lock(&shared.worker).is_some(), "receiver destruction cannot detach the existing worker");
    let mut ticket = worker.stop_disconnected().ok().expect("recover exact disconnected lifecycle");
    assert_eq!(ticket.status().failure, Some(DocumentResultSettlementReason::ReceiverDisconnected));
    assert!(ticket.status().occupied);
    assert!(matches!(ticket.acknowledge_delivery_failure(), Some(DocumentDeliveryFailure::ReceiverDisconnected)));
    finish(ticket, |_| ControlFlow::Continue(()));
}

#[test]
fn result_transport_close_fences_observer_clones_and_returns_the_exact_receipt() {
    let (worker, mut receiver, _) = harness(
        || Ok(()),
        |shared, inbox| {
            assert!(matches!(inbox.recv_timeout(LIMIT), Err(mpsc::RecvTimeoutError::Disconnected)));
            shared.publish(DocumentResult::Stopped(Ok(())));
        },
    );
    let policy = super::super::document_policy(&Document::new());
    let mut observer = worker.observer(super::super::DocumentId(2), None, 1, 0, policy);
    // Capture one genuine journal receipt without running a disk/journal lane.
    let (tx, rx) = mpsc::sync_channel(1);
    let mut editor = Editor::new(Document::new());
    editor
        .set_edit_observer(Some(Box::new(move |receipt| {
            assert!(tx.try_send(receipt).is_ok());
            Ok(())
        })))
        .unwrap();
    editor.insert_text("x", Duration::ZERO).unwrap();
    let receipt = rx.recv_timeout(LIMIT).unwrap();
    let identity = receipt.after.clone();
    receiver.close();
    let refused = observer(receipt).expect_err("closed observer must return its exact receipt");
    assert_eq!(refused.after_revision, 1);
    assert_eq!(refused.after.len(), identity.len());
    assert_eq!(worker.journal_capacity.load(Ordering::Acquire), 16, "refused receipt returns capacity credit");
    finish(receiver.stop(worker).unwrap(), |_| ControlFlow::Continue(()));
    drop((refused, identity, observer));
}

#[test]
fn result_transport_done_notice_does_not_prove_actual_thread_exit() {
    let (release, held) = mpsc::sync_channel(1);
    let (entered, epilogue) = mpsc::sync_channel(1);
    let (worker, receiver, shared) = harness(
        || Ok(()),
        move |shared, _| {
            shared.publish(DocumentResult::Stopped(Ok(())));
            // Deterministically hold the worker AFTER its completion notification.
            lock(&shared.state).worker_done = true;
            shared.changed.notify_all();
            entered.try_send(()).unwrap();
            held.recv_timeout(LIMIT).unwrap();
        },
    );
    // This control observes the real native handle once while its epilogue is
    // held. A deadline-bounded wait would intentionally consume the release
    // deadline, racing the same gate it is meant to inspect.
    let failure = receiver
        .stop(worker)
        .unwrap()
        .settle_with_native(
            LIMIT,
            |_| ControlFlow::Continue(()),
            |shared, deadline| {
                shared.settle_native_until_with(
                    deadline,
                    NativeExitObservation::query,
                    NativeExitObservation::retire_after_join,
                )
            },
        )
        .unwrap_err();
    assert_eq!(failure.reason, DocumentResultSettlementReason::ThreadExitPending);
    epilogue.recv_timeout(LIMIT).unwrap();
    assert!(!lock(&shared.worker).as_ref().unwrap().is_finished());
    release.try_send(()).unwrap();
    lock(&shared.worker).take().unwrap().join().unwrap();
    lock(&shared.state).worker_joined = true;
    failure.ticket.settle(LIMIT, |_| ControlFlow::Continue(())).unwrap();
}

#[test]
fn result_transport_failed_stopped_receipt_remains_the_actual_sticky_failure() {
    let (worker, mut receiver, shared) = harness(
        || Ok(()),
        |shared, _| {
            shared
                .publish(DocumentResult::Stopped(Err(io::Error::new(io::ErrorKind::StorageFull, "durability failed"))));
        },
    );
    wait(&shared, |state| state.slot.is_some());
    assert!(matches!(
        receiver.try_recv(true),
        Err(DocumentResultReceiveError::Failed(DocumentResultSettlementReason::StoppedFailed))
    ));
    let failure = receiver.stop(worker).unwrap().settle(LIMIT, |_| panic!("must not hide failed stop")).unwrap_err();
    assert_eq!(failure.reason, DocumentResultSettlementReason::StoppedFailed);
    let again = failure.ticket.settle(LIMIT, |_| ControlFlow::Continue(())).unwrap_err();
    assert_eq!(again.reason, DocumentResultSettlementReason::StoppedFailed);
    match again.ticket.inspect().pending().unwrap() {
        DocumentResult::Stopped(Err(error)) => {
            assert_eq!(error.kind(), io::ErrorKind::StorageFull);
            assert_eq!(error.to_string(), "durability failed");
        }
        _ => panic!("lost real stop failure"),
    }
    // Test-only teardown of the injected terminal error; no production API
    // can erase it or mint a successful stop from it.
    lock(&shared.state).slot = Some(DocumentSlot::new(DocumentResult::Stopped(Ok(()))));
    finish(again.ticket, |_| ControlFlow::Continue(()));
}

#[test]
fn result_transport_timeout_returns_same_worker_and_pending_result() {
    let (worker, receiver, shared) = harness(
        || Ok(()),
        |shared, _| {
            shared.publish(DocumentResult::CapacityAvailable);
            shared.publish(DocumentResult::Stopped(Ok(())));
        },
    );
    wait(&shared, |state| state.slot.is_some());
    let ticket = receiver.stop(worker).unwrap();
    let identity = Arc::as_ptr(&ticket.receiver.shared);
    let failure = ticket.settle(Duration::ZERO, |_| panic!("zero deadline cannot take a result")).unwrap_err();
    assert_eq!(failure.reason, DocumentResultSettlementReason::TimedOut);
    assert_eq!(Arc::as_ptr(&failure.ticket.receiver.shared), identity);
    assert!(matches!(failure.ticket.inspect().pending(), Some(DocumentResult::CapacityAvailable)));
    finish(failure.ticket, |_| ControlFlow::Continue(()));
}

#[test]
fn result_transport_same_worker_drains_every_accepted_request_after_close() {
    let (worker, receiver, _) = harness(
        || Ok(()),
        |shared, inbox| {
            for generation in [5, 6] {
                let DocumentRequest::ReleaseReopen { generation: actual, .. } = inbox.recv_timeout(LIMIT).unwrap()
                else {
                    panic!("different accepted request")
                };
                assert_eq!(actual, generation);
                shared.publish(DocumentResult::ReopenValidated {
                    document: super::super::DocumentId(1),
                    generation,
                    result: Ok(true),
                });
            }
            assert!(matches!(inbox.recv_timeout(LIMIT), Err(mpsc::RecvTimeoutError::Disconnected)));
            shared.publish(DocumentResult::Stopped(Ok(())));
        },
    );
    for generation in [5, 6] {
        assert!(
            worker
                .try_submit(DocumentRequest::ReleaseReopen { document: super::super::DocumentId(1), generation })
                .is_ok()
        );
    }
    let ticket = receiver.stop(worker).unwrap();
    let mut delivered = Vec::new();
    finish(ticket, |result| {
        match result {
            DocumentResult::ReopenValidated { generation, .. } => delivered.push(generation),
            DocumentResult::Stopped(Ok(())) => {}
            result => return ControlFlow::Break(result),
        }
        ControlFlow::Continue(())
    });
    assert_eq!(delivered, [5, 6]);
}

fn failed_wrap() -> (
    crate::Workspace,
    crate::workspace::workspace_wrap::WorkspaceWrapSettlementFailure,
    mpsc::SyncSender<()>,
    AllocationBudget,
) {
    use crate::{
        View,
        visual_rows::{BodyOwner, LayoutGeometry, SnapshotSource, SourceIdentity, WrapSource},
        workspace::workspace_wrap::WorkspaceWrap,
        wrap_host::WrapHost,
    };
    use std::{ops::ControlFlow, sync::atomic::AtomicBool};
    struct GatedSource {
        inner: SnapshotSource,
        first: AtomicBool,
        entered: mpsc::SyncSender<()>,
        release: Mutex<mpsc::Receiver<()>>,
        ui: std::thread::ThreadId,
        _charge: AllocationCharge,
    }
    impl WrapSource for GatedSource {
        fn run_layout(
            self: Arc<Self>,
            engine: crate::visual_rows::LayoutEngine,
            request: crate::visual_rows::TaskRequest,
            driver: &mut dyn crate::visual_rows::TaskDriver,
        ) -> crate::visual_rows::WorkerExit {
            crate::visual_rows::fixtures::run_snapshot(self, None, engine, request, driver)
        }
        fn identity(&self) -> SourceIdentity {
            self.inner.identity()
        }
        fn len(&self) -> u64 {
            self.inner.len()
        }
        fn line_count(&self) -> u64 {
            self.inner.line_count()
        }
        fn line_start(&self, line: u64) -> io::Result<Option<u64>> {
            self.inner.line_start(line)
        }
        fn line_of(&self, at: u64) -> io::Result<u64> {
            self.inner.line_of(at)
        }
        fn read(&self, at: u64, output: &mut [u8]) -> io::Result<usize> {
            crate::visual_rows::fixtures::SnapshotHooks::before_snapshot_read(self, output.len())?;
            self.inner.read(at, output)
        }
    }
    impl crate::snapshot_layout_source::SnapshotOwner for GatedSource {
        fn snapshot(&self) -> &kwil_core_text::Snapshot {
            crate::snapshot_layout_source::SnapshotOwner::snapshot(&self.inner)
        }
    }
    impl crate::visual_rows::fixtures::SnapshotHooks for GatedSource {
        fn before_snapshot_read(&self, _: usize) -> io::Result<()> {
            assert_ne!(std::thread::current().id(), self.ui, "transport may not read source on UI");
            if self.first.swap(false, Ordering::SeqCst) {
                self.entered.try_send(()).unwrap();
                lock(&self.release).recv_timeout(LIMIT).unwrap();
            }
            Ok(())
        }
    }
    let mut workspace = crate::Workspace::new(View::new(None)).unwrap();
    workspace.editor_mut().unwrap().insert_text("abcdef", Duration::ZERO).unwrap();
    workspace.editor_mut().unwrap().settings.editor.word_wrap = true;
    let mut host = WrapHost::new(|| {}).unwrap();
    let budget = host.budget().clone();
    let mut owner = WorkspaceWrap::new(&mut workspace, &host).unwrap();
    let context = owner
        .prepare_context(&workspace, LayoutGeometry { columns: 3, rows: 2, ..Default::default() }, BodyOwner::Text)
        .unwrap();
    host.sync(workspace.editor(), context).unwrap();
    let (entered, reading) = mpsc::sync_channel(1);
    let (release, held) = mpsc::sync_channel(1);
    let source: Arc<dyn WrapSource> = Arc::new(GatedSource {
        inner: SnapshotSource::new(context.key.source, workspace.editor().unwrap().document().snapshot()),
        first: AtomicBool::new(true),
        entered,
        release: Mutex::new(held),
        ui: std::thread::current().id(),
        _charge: host.budget().reserve(size_of::<GatedSource>() + 2 * size_of::<usize>()).unwrap(),
    });
    assert!(matches!(host.drive(source), ControlFlow::Continue(())));
    reading.recv_timeout(LIMIT).unwrap();
    let ticket = match owner.stop(host) {
        ControlFlow::Continue(ticket) => ticket,
        ControlFlow::Break(_) => panic!("unexpected held workspace owner"),
    };
    let failure = ticket.settle(Duration::ZERO).err().expect("held actual worker cannot settle");
    (workspace, failure, release, budget)
}

#[test]
fn result_transport_wrap_failure_classifier_preserves_native_codes_priority_and_retry_eligibility() {
    for (error, expected) in [
        (WrapError::ThreadExitPending, DocumentResultSettlementReason::WrapThreadExitPending),
        (
            WrapError::NativeExit(NativeObservationError::LinuxQuery { errno: i32::MIN }),
            DocumentResultSettlementReason::WrapNativeExit(NativeObservationError::LinuxQuery { errno: i32::MIN }),
        ),
        (
            WrapError::NativeExit(NativeObservationError::MachRetire { code: 73 }),
            DocumentResultSettlementReason::WrapNativeExit(NativeObservationError::MachRetire { code: 73 }),
        ),
    ] {
        let (mut workspace, mut failed, release, budget) = failed_wrap();
        // Scalar boundary control, not a claim about actual native readiness.
        // The original failed ticket and every source/handle owner stay intact.
        failed.error = error;
        let original = &*failed.ticket as *const _ as usize;
        let (worker, mut receiver, shared) = harness_with_budget(
            &budget,
            || Ok(()),
            move |shared, inbox| {
                shared.publish(DocumentResult::WrapSettled(Err(failed)));
                assert!(matches!(inbox.recv_timeout(LIMIT), Err(mpsc::RecvTimeoutError::Disconnected)));
                shared.publish(DocumentResult::Stopped(Ok(())));
            },
        );
        wait(&shared, |state| state.slot.is_some());
        assert_eq!(receiver.status().failure, Some(expected));
        assert!(
            matches!(receiver.try_recv(true), Err(DocumentResultReceiveError::Failed(reason)) if reason == expected)
        );
        {
            let state = lock(&shared.state);
            let Some(DocumentResult::WrapSettled(Err(failure))) = state.whole() else {
                panic!("exact failed owner lost")
            };
            assert_eq!(&*failure.ticket as *const _ as usize, original);
            assert_eq!(state.wrap_failure(), Some(expected));
            assert!(state.retry_acknowledgement_required.is_none());
        }
        // A higher-priority original delivery failure must NOT become eligible
        // merely because the same whole slot also carries a retryable wrap error.
        lock(&shared.state).delivery_failure = Some(DocumentDeliveryFailure::Wake(io::Error::other("first wake")));
        assert_eq!(receiver.status().failure, Some(DocumentResultSettlementReason::WakeFailed));
        assert!(matches!(receiver.request_wrap_retry(LIMIT), Err(DocumentResultSettlementReason::WakeFailed)));
        let first = lock(&shared.state).delivery_failure.take().unwrap();
        assert!(matches!(first, DocumentDeliveryFailure::Wake(_)));
        drop(first);
        let deadline = Instant::now() + LIMIT;
        let token = receiver.request_wrap_retry(deadline.saturating_duration_since(Instant::now())).unwrap();
        assert!(matches!(receiver.request_wrap_retry(LIMIT), Err(DocumentResultSettlementReason::Busy)));
        release.try_send(()).unwrap();
        let token = complete_wrap_retry(&mut receiver, token, deadline);
        let Some(DocumentResult::WrapSettled(Ok(receipt))) = receiver.try_recv(true).unwrap() else {
            panic!("matching success receipt missing")
        };
        assert!(matches!(workspace.acknowledge_wrap_settled(receipt), ControlFlow::Continue(())));
        receiver.acknowledge_wrap_retry(token).unwrap();
        finish(receiver.stop(worker).unwrap(), |_| ControlFlow::Continue(()));
        assert!(workspace.wrap_cleanup_ready());
    }
}

#[test]
fn result_transport_completed_pending_attempt_has_no_ack_authority_and_stale_token_cannot_open_gate() {
    let (mut workspace, mut failed, release, budget) = failed_wrap();
    failed.error = WrapError::ThreadExitPending;
    let original_owner = &*failed.ticket as *const _ as usize;
    let effects = Arc::new(AtomicUsize::new(0));
    let effect_calls = effects.clone();
    let (worker, mut receiver, shared) = harness_with_budget(
        &budget,
        || Ok(()),
        move |shared, inbox| {
            shared.publish_with(DocumentResult::WrapSettled(Err(failed)), move |pending, timeout| {
            if effect_calls.fetch_add(1, Ordering::Relaxed) == 0 {
                // Deterministic Pending boundary: return the EXACT same slot
                // and owner. No receipt, native Exited, join or source work is
                // fabricated; subsequent explicit attempts use the real path.
                assert!(matches!(pending.as_ref().and_then(DocumentSlot::whole), Some(DocumentResult::WrapSettled(Err(failure))) if matches!(failure.error, WrapError::ThreadExitPending)));
                pending
            } else {
                retry_wrap_result(pending, timeout)
            }
        });
            assert!(matches!(inbox.recv_timeout(LIMIT), Err(mpsc::RecvTimeoutError::Disconnected)));
            shared.publish(DocumentResult::Stopped(Ok(())));
        },
    );
    wait(&shared, |state| state.slot.is_some());
    let original_thread = lock(&shared.worker).as_ref().unwrap().thread().id();
    let deadline = Instant::now() + LIMIT;
    let token = receiver.request_wrap_retry(deadline.saturating_duration_since(Instant::now())).unwrap();
    let generation = token.0.control.generation;
    {
        let state = lock(&shared.state);
        let (state, _) = shared
            .changed
            .wait_timeout_while(state, deadline.saturating_duration_since(Instant::now()), |state| {
                state.retry.is_some() || state.retry_running
            })
            .unwrap();
        assert!(!state.retry_running && state.retry.is_none());
        assert_eq!(state.retry_generation, generation);
        assert_eq!(state.failure(), Some(DocumentResultSettlementReason::WrapThreadExitPending));
        assert!(state.in_flight && state.retry_acknowledgement_required.is_none());
        let Some(DocumentResult::WrapSettled(Err(failure))) = state.whole() else { panic!("Pending lost its owner") };
        assert_eq!(&*failure.ticket as *const _ as usize, original_owner);
    }
    assert_eq!(effects.load(Ordering::Relaxed), 1);
    let refused = receiver.acknowledge_wrap_retry(token).unwrap_err();
    assert_eq!(refused.reason, DocumentResultSettlementReason::WrapThreadExitPending);
    assert!(!lock(&shared.requests).accepting, "completed Pending is never an ACK");
    let stale = refused.token;
    release.try_send(()).unwrap();
    let token = receiver.request_wrap_retry(deadline.saturating_duration_since(Instant::now())).unwrap();
    assert!(token.0.control.generation > generation, "new explicit attempt has a new generation");
    let token = complete_wrap_retry(&mut receiver, token, deadline);
    let successful_generation = token.0.control.generation;
    assert_eq!(lock(&shared.worker).as_ref().unwrap().thread().id(), original_thread);
    let refused = receiver.acknowledge_wrap_retry(stale).unwrap_err();
    assert_eq!(refused.reason, DocumentResultSettlementReason::RetryUnavailable);
    assert!(!lock(&shared.requests).accepting, "stale completed token cannot reopen admission");
    assert_eq!(lock(&shared.state).retry_acknowledgement_required.unwrap().generation, successful_generation);
    drop(refused.token);
    let Some(DocumentResult::WrapSettled(Ok(receipt))) = receiver.try_recv(true).unwrap() else {
        panic!("real success receipt missing")
    };
    assert!(matches!(workspace.acknowledge_wrap_settled(receipt), ControlFlow::Continue(())));
    receiver.acknowledge_wrap_retry(token).unwrap();
    assert!(lock(&shared.requests).accepting);
    assert!(effects.load(Ordering::Relaxed) >= 2);
    finish(receiver.stop(worker).unwrap(), |_| ControlFlow::Continue(()));
    assert!(workspace.wrap_cleanup_ready());
}

#[test]
fn result_transport_live_retry_is_same_worker_same_slot_and_receipt_ack_precedes_cleanup() {
    use std::ops::ControlFlow;
    let (mut workspace, failed, release, budget) = failed_wrap();
    let failed_identity = &*failed.ticket as *const _ as usize;
    let (worker_id, id) = mpsc::sync_channel(1);
    let (worker, mut receiver, shared) = harness_with_budget(
        &budget,
        || Ok(()),
        move |shared, inbox| {
            worker_id.try_send(std::thread::current().id()).unwrap();
            shared.publish(DocumentResult::WrapSettled(Err(failed)));
            // The sender must STILL exist until close, even after ordinary
            // admission was fenced by the failed slot.
            assert!(matches!(inbox.recv_timeout(LIMIT), Err(mpsc::RecvTimeoutError::Disconnected)));
            shared.publish(DocumentResult::Stopped(Ok(())));
        },
    );
    let document_thread = id.recv_timeout(LIMIT).unwrap();
    wait(&shared, |state| matches!(state.whole(), Some(DocumentResult::WrapSettled(Err(_)))));
    assert!(matches!(
        receiver.try_recv(true),
        Err(DocumentResultReceiveError::Failed(DocumentResultSettlementReason::WrapSettlementFailed))
    ));
    match lock(&shared.state).whole().unwrap() {
        DocumentResult::WrapSettled(Err(failure)) => assert_eq!(&*failure.ticket as *const _ as usize, failed_identity),
        _ => panic!("failed head replaced"),
    }
    // The actual source remains gated: whether the worker has taken the
    // scalar or not, a second control MUST be Busy until explicit release.
    let deadline = Instant::now() + LIMIT;
    let token = receiver.request_wrap_retry(deadline.saturating_duration_since(Instant::now())).unwrap();
    assert!(matches!(receiver.request_wrap_retry(LIMIT), Err(DocumentResultSettlementReason::Busy)));
    assert!(matches!(worker.try_submit(DocumentRequest::FlushJournals), Err(TrySendError::Disconnected(_))));
    release.try_send(()).unwrap();
    shared.changed.notify_all();
    let token = complete_wrap_retry(&mut receiver, token, deadline);
    assert!(lock(&shared.worker).as_ref().is_some_and(|thread| thread.thread().id() == document_thread));
    let Some(DocumentResult::WrapSettled(Ok(receipt))) = receiver.try_recv(true).unwrap() else {
        panic!("no successful matching receipt")
    };
    assert!(receiver.status().retry_acknowledgement_required);
    assert!(lock(&shared.worker).as_ref().is_some_and(|thread| !thread.is_finished()));
    let mut foreign_workspace = crate::Workspace::new(crate::View::new(None)).unwrap();
    let receipt = match foreign_workspace.acknowledge_wrap_settled(receipt) {
        ControlFlow::Break(receipt) => receipt,
        ControlFlow::Continue(_) => panic!("foreign Workspace accepted the private receipt"),
    };
    assert!(receiver.status().retry_acknowledgement_required, "Workspace refusal cannot release the worker");
    assert!(!lock(&shared.requests).accepting);
    assert!(matches!(workspace.acknowledge_wrap_settled(receipt), ControlFlow::Continue(())));
    receiver.acknowledge_wrap_retry(token).unwrap();
    assert!(!receiver.status().retry_acknowledgement_required);
    assert!(lock(&shared.requests).accepting);
    finish(receiver.stop(worker).unwrap(), |_| ControlFlow::Continue(()));
    assert!(workspace.wrap_cleanup_ready());
}

#[test]
fn result_transport_sealed_workspace_retry_retirement_waits_for_matching_ack() {
    let (mut workspace, failed, release, budget) = failed_wrap();
    let (worker, mut receiver, shared) = harness_with_budget(
        &budget,
        || Ok(()),
        move |shared, inbox| {
            shared.publish(DocumentResult::WrapSettled(Err(failed)));
            assert!(matches!(inbox.recv_timeout(LIMIT).unwrap(), DocumentRequest::SessionTimer));
            assert!(matches!(inbox.recv_timeout(LIMIT), Err(mpsc::RecvTimeoutError::Disconnected)));
            shared.publish(DocumentResult::Stopped(Ok(())));
        },
    );
    assert!(workspace.replace_document_worker_for_transport_test(Some(worker)).is_none());
    wait(&shared, |state| matches!(state.whole(), Some(DocumentResult::WrapSettled(Err(_)))));
    let deadline = Instant::now() + LIMIT;
    let token = receiver.request_wrap_retry(deadline.saturating_duration_since(Instant::now())).unwrap();
    release.try_send(()).unwrap();
    let token = complete_wrap_retry(&mut receiver, token, deadline);
    let generation = token.0.control.generation;
    assert!(receiver.accept_workspace(&mut workspace, &budget, true).unwrap().is_some());
    wait(&shared, |state| !state.in_flight);
    assert!(workspace.wrap_cleanup_ready());
    assert_eq!(lock(&shared.state).retry_acknowledgement_required.map(|control| control.generation), Some(generation));
    let worker = workspace.replace_document_worker_for_transport_test(None).unwrap();
    assert!(matches!(worker.try_submit(DocumentRequest::FlushJournals), Err(TrySendError::Disconnected(_))));
    assert!(!lock(&shared.requests).accepting, "sealed retirement cannot ACK the retry generation");
    receiver.acknowledge_wrap_retry(token).unwrap();
    assert!(lock(&shared.requests).accepting, "only the matching ACK opens ordinary admission");
    assert!(worker.try_submit(DocumentRequest::SessionTimer).is_ok());
    finish(receiver.stop(worker).unwrap(), |result| {
        assert!(matches!(result, DocumentResult::Stopped(Ok(()))));
        ControlFlow::Continue(())
    });
}

#[test]
fn result_transport_first_refusal_survives_retries_until_original_workspace_retirement_ack() {
    use crate::workspace::workspace_document::WorkspaceDocumentAcceptError as Error;
    // Successive live commits overlap old + new metadata (each includes the
    // existing 9 MiB Persist allowance). Use the established session ceiling;
    // the blockers below still consume ALL remaining capacity before refusal.
    const BUDGET: usize = crate::visual_rows::MAX_SESSION_BYTES;
    let budget = AllocationBudget::new(BUDGET);
    let (worker, mut receiver, shared) = harness_with_budget(
        &budget,
        || Ok(()),
        |shared, inbox| {
            shared.publish(DocumentResult::CapacityAvailable);
            let DocumentRequest::SourceBarrier(marker) = inbox.recv_timeout(LIMIT).unwrap() else {
                panic!("original bound worker must reach its source barrier")
            };
            shared.publish(DocumentResult::SourceBarrier(DocumentBarrierReceipt::reached(marker)));
            assert!(matches!(inbox.recv_timeout(LIMIT), Err(mpsc::RecvTimeoutError::Disconnected)));
            shared.publish(DocumentResult::Stopped(Ok(())));
        },
    );
    let mut workspace = crate::Workspace::new(crate::View::new(None)).unwrap();
    let mut foreign = crate::Workspace::new(crate::View::new(None)).unwrap();
    assert!(workspace.replace_document_worker_for_transport_test(Some(worker)).is_none());
    wait(&shared, |state| state.slot.is_some());
    assert!(matches!(
        receiver.accept_workspace(&mut foreign, &budget, true),
        Err(DocumentResultSettlementReason::ConsumerRefused)
    ));
    assert!(matches!(lock(&shared.state).commit_error.as_ref(), Some(Error::ForeignWorkspace)));
    // Force a DIFFERENT real sealed refusal after Whole -> Partial promotion.
    let blocker = budget.reserve(BUDGET - budget.retained_bytes()).unwrap();
    assert!(matches!(
        receiver.accept_workspace(&mut workspace, &budget, true),
        Err(DocumentResultSettlementReason::ConsumerRefused)
    ));
    assert!(matches!(lock(&shared.state).commit_error.as_ref(), Some(Error::ForeignWorkspace)));
    let secondary = lock(&shared.state).retain_commit_error(Error::PartialCommit);
    assert!(matches!(secondary, Some(Error::PartialCommit)));
    assert!(matches!(lock(&shared.state).commit_error.as_ref(), Some(Error::ForeignWorkspace)));
    drop(secondary);
    drop(blocker);
    let worker = workspace.replace_document_worker_for_transport_test(None).unwrap();
    let (mut bound, mut permit) = receiver.begin_bound(&worker).unwrap();
    assert!(workspace.replace_document_worker_for_transport_test(Some(worker)).is_none());
    let deadline = Instant::now() + LIMIT;
    assert_eq!(bound.wait_until(deadline), Err(DocumentResultSettlementReason::ConsumerRefused));
    assert!(bound.accept_bound(&mut workspace, &budget, &permit, deadline).unwrap().is_some());
    bound.wait_until(deadline).unwrap();
    wait(&shared, |state| !state.in_flight);
    assert!(bound.inspect().commit_error().is_none(), "same producer retirement ACKs the original refusal");
    let live_metadata = workspace.document_live_metadata_bytes(None, None).unwrap();
    assert!(
        live_metadata.checked_mul(2).unwrap() > 16 * 1024 * 1024,
        "the previous 16 MiB fixture cannot admit old + new live metadata overlap"
    );
    let marker = permit.request_barrier().unwrap();
    wait(&shared, |state| matches!(state.whole(), Some(DocumentResult::SourceBarrier(_))));
    let delivery = bound.info().unwrap().delivery().unwrap();
    // This is a NEW admission refusal, not the already acknowledged foreign
    // Workspace error. Retain the exact barrier in Received, without replay.
    let blocker = budget.reserve(BUDGET - budget.retained_bytes()).unwrap();
    assert!(matches!(
        bound.accept_bound(&mut workspace, &budget, &permit, deadline),
        Err(DocumentResultSettlementReason::ConsumerRefused)
    ));
    {
        let inspection = bound.inspect();
        assert!(matches!(inspection.commit_error(), Some(Error::Admission(_))));
        let commit = inspection.commit().unwrap();
        assert_eq!(commit.phase(), crate::workspace::workspace_document::commit::DocumentCommitPhase::Received);
        assert_eq!(commit.installed_fields(), 0);
        assert!(matches!(commit.pending(), Some(DocumentResult::SourceBarrier(receipt)) if receipt.id() == marker));
    }
    assert_eq!(bound.info().unwrap().delivery(), Some(delivery));
    drop(blocker);
    let update = bound.accept_bound(&mut workspace, &budget, &permit, deadline).unwrap().unwrap();
    assert_eq!(update.barrier.unwrap().id(), marker);
    permit.seal_sources(&update.barrier.unwrap()).unwrap();
    wait(&shared, |state| !state.in_flight);
    assert!(bound.inspect().commit_error().is_none());
    bound.close_after_source_release(&permit).unwrap();
    let worker = workspace.replace_document_worker_for_transport_test(None).unwrap();
    finish(bound.into_legacy_for_test(worker), |result| {
        assert!(matches!(result, DocumentResult::Stopped(Ok(()))));
        ControlFlow::Continue(())
    });
}

#[test]
fn result_transport_foreign_stale_and_close_racing_retry_ack_never_reopen_the_gate() {
    let (release_stop, stop_held) = mpsc::sync_channel(1);
    let (worker, mut receiver, shared) = harness(
        || Ok(()),
        move |shared, inbox| {
            assert!(matches!(inbox.recv_timeout(LIMIT), Err(mpsc::RecvTimeoutError::Disconnected)));
            // Model the real publisher's outstanding-ACK fence: Stopped may
            // not supersede the seeded exact delivery before that ACK.
            stop_held.recv_timeout(LIMIT).unwrap();
            shared.publish(DocumentResult::Stopped(Ok(())));
        },
    );
    let (foreign, foreign_receiver) = Shared::new(&AllocationBudget::default(), || Ok(())).unwrap();
    {
        let mut state = lock(&shared.state);
        state.retry_generation = 3;
        state.retry_acknowledgement_required = Some(RetryControl {
            deadline: Instant::now() + LIMIT,
            delivery: DocumentDeliveryId { worker: shared.worker_id, delivery: state.delivery },
            generation: 3,
            action: DocumentProducerAction::RetryWrap,
            failure: None,
        });
    }
    shared.fence_requests();
    let foreign = wrap_token_for_test(&foreign, 3);
    assert_eq!(
        receiver.acknowledge_wrap_retry(foreign).unwrap_err().reason,
        DocumentResultSettlementReason::RetryUnavailable
    );
    let stale = wrap_token_for_test(&shared, 2);
    assert_eq!(
        receiver.acknowledge_wrap_retry(stale).unwrap_err().reason,
        DocumentResultSettlementReason::RetryUnavailable
    );
    assert!(!lock(&shared.requests).accepting);
    let token = wrap_token_for_test(&shared, 3);
    receiver.close();
    receiver.acknowledge_wrap_retry(token).unwrap();
    assert!(!lock(&shared.requests).accepting);
    assert!(lock(&shared.requests).sender.is_none());
    let duplicate = wrap_token_for_test(&shared, 3);
    assert_eq!(
        receiver.acknowledge_wrap_retry(duplicate).unwrap_err().reason,
        DocumentResultSettlementReason::RetryUnavailable
    );
    release_stop.try_send(()).unwrap();
    finish(receiver.stop(worker).unwrap(), |_| ControlFlow::Continue(()));
    drop(foreign_receiver);
}

#[test]
fn result_transport_off_ui_retry_preserves_actual_failed_wrap_ticket_and_returns_matching_receipt() {
    use std::ops::ControlFlow;
    let (mut workspace, failure, release, budget) = failed_wrap();
    let identity = &*failure.ticket as *const _ as usize;
    let (worker, receiver, shared) = harness_with_budget(
        &budget,
        || Ok(()),
        move |shared, _| {
            shared.publish(DocumentResult::WrapSettled(Err(failure)));
            shared.publish(DocumentResult::Stopped(Ok(())));
        },
    );
    wait(&shared, |state| state.slot.is_some());
    let failed = receiver.stop(worker).unwrap().settle(LIMIT, |_| panic!("must retain failed ticket")).unwrap_err();
    assert_eq!(failed.reason, DocumentResultSettlementReason::WrapSettlementFailed);
    let mut ticket = failed.ticket;
    assert_eq!(ticket.retry_pending_wrap(Duration::ZERO).unwrap_err(), DocumentResultSettlementReason::TimedOut);
    match ticket.inspect().pending().unwrap() {
        DocumentResult::WrapSettled(Err(failure)) => assert_eq!(&*failure.ticket as *const _ as usize, identity),
        _ => panic!("zero admission moved failed owner"),
    }
    let deadline = Instant::now() + LIMIT;
    release.try_send(()).unwrap();
    retry_wrap_off_ui(&mut ticket, deadline);
    finish(ticket, |result| {
        match result {
            DocumentResult::WrapSettled(Ok(receipt)) => {
                assert!(matches!(workspace.acknowledge_wrap_settled(receipt), ControlFlow::Continue(())));
            }
            DocumentResult::Stopped(Ok(())) => {}
            result => return ControlFlow::Break(result),
        }
        ControlFlow::Continue(())
    });
    assert!(workspace.wrap_cleanup_ready());
}

#[test]
fn result_transport_real_bounded_constructor_drains_accepted_work_and_legacy_adapter_stays_compatible() {
    let scratch = super::super::tests::Scratch::new();
    let path = scratch.0.join("tiny.txt");
    std::fs::write(&path, b"tiny").unwrap();
    let budget = AllocationBudget::default();
    let before = budget.retained_bytes();
    let (worker, receiver) = DocumentWorker::spawn_bounded(&budget, || Ok(())).unwrap();
    assert!(budget.retained_bytes() > before, "actual receiver/worker/ticket metadata shares this aggregate");
    for _ in 0..2 {
        let file = kwil_core_text::TextFile::open(&path).unwrap();
        assert!(worker.try_submit(DocumentRequest::RetireSource { file }).is_ok());
    }
    let mut retired = 0;
    finish(receiver.stop(worker).unwrap(), |result| {
        match result {
            DocumentResult::CapacityAvailable => retired += 1,
            DocumentResult::Stopped(Ok(())) => {}
            result => return ControlFlow::Break(result),
        }
        ControlFlow::Continue(())
    });
    assert_eq!(retired, 2, "closing must not lose either admitted worker request");
    assert_eq!(budget.retained_bytes(), before, "all transport metadata retires after actual exit");
    let (complete, results) = mpsc::sync_channel(1);
    let legacy = DocumentWorker::spawn(move |result| assert!(complete.try_send(result).is_ok())).unwrap();
    drop(legacy);
    assert!(matches!(results.recv_timeout(LIMIT).unwrap(), DocumentResult::Stopped(Ok(()))));
}

#[test]
fn result_transport_wake_and_fallible_accept_execute_outside_all_transport_locks() {
    let published: Arc<Mutex<Option<Weak<Shared>>>> = Arc::new(Mutex::new(None));
    let reentrant = published.clone();
    let (release, held) = mpsc::sync_channel(1);
    let (woke, waking) = mpsc::sync_channel(1);
    let (entered, published_slot) = mpsc::sync_channel(1);
    let (resume, proceed) = mpsc::sync_channel(1);
    let (worker, receiver, shared) = harness(
        move || {
            let weak = lock(&reentrant).as_ref().unwrap().clone();
            let shared = weak.upgrade().unwrap();
            assert!(shared.state.try_lock().is_ok(), "wake under mailbox lock");
            assert!(shared.requests.try_lock().is_ok(), "wake under sender gate");
            woke.try_send(()).unwrap();
            Ok(())
        },
        move |shared, _| {
            held.recv_timeout(LIMIT).unwrap();
            shared.publish(DocumentResult::CapacityAvailable);
            shared.publish(DocumentResult::Stopped(Ok(())));
        },
    );
    *lock(&published) = Some(Arc::downgrade(&shared));
    *lock(&shared.publication_gate) = Some(PublicationGate { entered, release: proceed });
    release.try_send(()).unwrap();
    // Do not touch either transport lock while the SAME producer probes its
    // wake. Then hold it outside those locks through each accept probe.
    waking.recv_timeout(LIMIT).unwrap();
    published_slot.recv_timeout(LIMIT).unwrap();
    wait(&shared, |state| state.slot.is_some());
    let mut first = true;
    finish(receiver.stop(worker).unwrap(), |_| {
        if first {
            first = false;
        } else {
            published_slot.recv_timeout(LIMIT).unwrap();
        }
        assert!(shared.state.try_lock().is_ok(), "accept under mailbox lock");
        assert!(shared.requests.try_lock().is_ok(), "accept under sender gate");
        resume.try_send(()).unwrap();
        ControlFlow::Continue(())
    });
}

#[test]
fn result_transport_wake_failure_after_admitted_receive_still_halts_the_same_producer() {
    use std::ops::ControlFlow;
    let (mut workspace, failed, release_source, budget) = failed_wrap();
    let (entered, waking) = mpsc::sync_channel(1);
    let (release, held) = mpsc::sync_channel(1);
    let held = Mutex::new(held);
    let progressed = Arc::new(AtomicUsize::new(0));
    let progress = progressed.clone();
    let wake_count = AtomicUsize::new(0);
    let phase = Arc::new(Mutex::new(None::<Weak<Shared>>));
    let wake_phase = phase.clone();
    let (worker, mut receiver, shared) = harness_with_budget(
        &budget,
        move || {
            if wake_count.fetch_add(1, Ordering::SeqCst) == 0 {
                return Ok(());
            }
            // A Pending attempt also wakes. Inject the original late failure
            // ONLY for the actual successful receipt/ACK-required phase.
            let shared = lock(&wake_phase).as_ref().unwrap().upgrade().unwrap();
            if !successful_wrap_retry(&shared) {
                return Ok(());
            }
            entered.try_send(()).unwrap();
            lock(&held).recv_timeout(LIMIT).unwrap();
            Err(io::Error::other("late wake failure"))
        },
        move |shared, _| {
            shared.publish(DocumentResult::WrapSettled(Err(failed)));
            progress.fetch_add(1, Ordering::SeqCst);
            shared.publish(DocumentResult::Stopped(Ok(())));
        },
    );
    *lock(&phase) = Some(Arc::downgrade(&shared));
    wait(&shared, |state| matches!(state.whole(), Some(DocumentResult::WrapSettled(Err(_)))));
    let deadline = Instant::now() + LIMIT;
    let token = receiver.request_wrap_retry(deadline.saturating_duration_since(Instant::now())).unwrap();
    release_source.try_send(()).unwrap();
    let token = complete_wrap_retry(&mut receiver, token, deadline);
    waking.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
    let Some(DocumentResult::WrapSettled(Ok(receipt))) = receiver.try_recv(true).unwrap() else {
        panic!("actual successful retry receipt must be admitted before wake returns")
    };
    assert!(matches!(workspace.acknowledge_wrap_settled(receipt), ControlFlow::Continue(())));
    receiver.acknowledge_wrap_retry(token).unwrap();
    assert_eq!(progressed.load(Ordering::SeqCst), 0);
    release.try_send(()).unwrap();
    wait(&shared, |state| state.delivery_failure.is_some());
    assert!(!receiver.status().occupied, "the admitted recipient already owns the result");
    assert_eq!(progressed.load(Ordering::SeqCst), 0, "empty slot must not erase a late wake failure");
    let failure = receiver.stop(worker).unwrap().settle(LIMIT, |_| ControlFlow::Continue(())).unwrap_err();
    assert_eq!(failure.reason, DocumentResultSettlementReason::WakeFailed);
    let mut ticket = failure.ticket;
    assert!(matches!(ticket.acknowledge_delivery_failure(), Some(DocumentDeliveryFailure::Wake(_))));
    finish(ticket, |_| ControlFlow::Continue(()));
    assert_eq!(progressed.load(Ordering::SeqCst), 1);
    assert!(workspace.wrap_cleanup_ready(), "delivered receipt must not be recreated or lost on the late error");
}

#[test]
fn result_transport_wrong_worker_stop_returns_both_exact_original_owners() {
    let run = |shared: Arc<Shared>, inbox: mpsc::Receiver<DocumentRequest>| {
        assert!(matches!(inbox.recv_timeout(LIMIT), Err(mpsc::RecvTimeoutError::Disconnected)));
        shared.publish(DocumentResult::Stopped(Ok(())));
    };
    let (first, receiver, shared) = harness(|| Ok(()), run);
    let (second, other, other_shared) = harness(|| Ok(()), run);
    let refusal = receiver.stop(second).err().expect("wrong worker must refuse whole pair");
    assert!(Arc::ptr_eq(&refusal.receiver.shared, &shared));
    assert!(matches!(&refusal.worker.tx, RequestSender::Bounded(value) if Arc::ptr_eq(value, &other_shared)));
    finish(refusal.receiver.stop(first).unwrap(), |_| ControlFlow::Continue(()));
    finish(other.stop(refusal.worker).unwrap(), |_| ControlFlow::Continue(()));
}

#[test]
fn result_transport_wake_failure_publication_excludes_retry_ack_and_all_request_owners() {
    #[derive(Debug)]
    struct WakeRaceError(u64);
    impl fmt::Display for WakeRaceError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "wake race {}", self.0)
        }
    }
    impl std::error::Error for WakeRaceError {}

    let (mut workspace, failed, release_source, budget) = failed_wrap();
    let (wake_entered, wake_waiting) = mpsc::sync_channel(1);
    let (release_wake, held_wake) = mpsc::sync_channel(1);
    let held_wake = Mutex::new(held_wake);
    let wake_count = AtomicUsize::new(0);
    let phase = Arc::new(Mutex::new(None::<Weak<Shared>>));
    let wake_phase = phase.clone();
    let progressed = Arc::new(AtomicUsize::new(0));
    let progress = progressed.clone();
    let (worker, mut receiver, shared) = harness_with_budget(
        &budget,
        move || {
            if wake_count.fetch_add(1, Ordering::SeqCst) == 0 {
                return Ok(());
            }
            let shared = lock(&wake_phase).as_ref().unwrap().upgrade().unwrap();
            if !successful_wrap_retry(&shared) {
                return Ok(());
            }
            wake_entered.try_send(()).unwrap();
            lock(&held_wake).recv_timeout(LIMIT).unwrap();
            Err(io::Error::other(WakeRaceError(73)))
        },
        move |shared, _| {
            shared.publish(DocumentResult::WrapSettled(Err(failed)));
            progress.fetch_add(1, Ordering::SeqCst);
            shared.publish(DocumentResult::Stopped(Ok(())));
        },
    );
    let (failure_entered, publication_held) = mpsc::sync_channel(1);
    let (release_failure, held_failure) = mpsc::sync_channel(1);
    *lock(&shared.wake_failure_gate) = Some(WakeFailureGate { entered: failure_entered, release: held_failure });
    *lock(&phase) = Some(Arc::downgrade(&shared));

    // Keep an observer clone from BEFORE the failure, with one genuine receipt
    // whose immutable owners must be returned without consumption or new I/O.
    let document = super::super::DocumentId(71);
    let mut observer = worker.observer(document, None, 1, 0, super::super::document_policy(&Document::new()));
    let (receipt_tx, receipt_rx) = mpsc::sync_channel(1);
    let mut editing = Editor::new(Document::new());
    editing
        .set_edit_observer(Some(Box::new(move |receipt| {
            assert!(receipt_tx.try_send(receipt).is_ok());
            Ok(())
        })))
        .unwrap();
    editing.insert_text("loan", Duration::ZERO).unwrap();
    let edit_receipt = receipt_rx.recv_timeout(LIMIT).unwrap();
    let expected_receipt = (
        edit_receipt.sequence,
        edit_receipt.before_revision,
        edit_receipt.after_revision,
        edit_receipt.before.len(),
        edit_receipt.after.len(),
        edit_receipt.replacement_count(),
    );

    wait(&shared, |state| matches!(state.whole(), Some(DocumentResult::WrapSettled(Err(_)))));
    let deadline = Instant::now() + LIMIT;
    let token = receiver.request_wrap_retry(deadline.saturating_duration_since(Instant::now())).unwrap();
    release_source.try_send(()).unwrap();
    let token = complete_wrap_retry(&mut receiver, token, deadline);
    let generation = token.0.control.generation;
    wake_waiting.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
    let Some(DocumentResult::WrapSettled(Ok(receipt))) = receiver.try_recv(true).unwrap() else {
        panic!("matching retry receipt must be admitted before the wake error")
    };
    assert!(matches!(workspace.acknowledge_wrap_settled(receipt), ControlFlow::Continue(())));
    // Deliberately leave the matching token unacknowledged until wake Err has
    // fenced the gate, at the cfg(test)-only publication interleave point.
    release_wake.try_send(()).unwrap();
    publication_held.recv_timeout(LIMIT).unwrap();
    let excludes_ack = shared.state.try_lock().is_err();
    let excludes_requests = shared.requests.try_lock().is_err();
    let (ack_started, ack_attempt) = mpsc::sync_channel(1);
    let (ack_done, ack_result) = mpsc::sync_channel(1);
    let ack_shared = shared.clone();
    let ack_thread = std::thread::spawn(move || {
        ack_started.try_send(()).unwrap();
        assert!(ack_done.try_send(acknowledge_wrap_retry(&ack_shared, token)).is_ok());
    });
    ack_attempt.recv_timeout(LIMIT).unwrap();

    // With the original split publication, BOTH locks are free at this gate:
    // force the real ACK to finish before allowing the error to be recorded.
    // With the fix it cannot finish until failure publication releases state.
    // No scheduling sleep/timeout is used to distinguish those two paths.
    let early_ack = if excludes_ack { None } else { Some(ack_result.recv_timeout(LIMIT)) };
    release_failure.try_send(()).unwrap();
    let acknowledgement = match early_ack {
        Some(result) => result.expect("unlocked ACK completes inside the historical gap"),
        None => ack_result.recv_timeout(LIMIT).unwrap(),
    };
    ack_thread.join().unwrap();
    let refusal = acknowledgement.expect_err("wake failure must atomically prevent retry ACK from reopening admission");
    assert!(excludes_ack && excludes_requests, "failure and admission fence must share the ACK critical section");
    assert_eq!(refusal.reason, DocumentResultSettlementReason::WakeFailed);
    assert_eq!(refusal.token.0.control.generation, generation);
    assert!(Weak::ptr_eq(&refusal.token.0.shared, &Arc::downgrade(&shared)));
    assert_eq!(progressed.load(Ordering::SeqCst), 0);
    assert!(!receiver.status().occupied, "the applied receipt must not be recreated on failure");
    assert!(receiver.status().retry_acknowledgement_required);
    {
        let gate = lock(&shared.requests);
        assert!(!gate.accepting);
        assert!(gate.sender.is_some(), "logical failure must retain the sole sender");
    }

    let editor = Box::new(Editor::new(Document::new()));
    let editor_identity = &*editor as *const Editor as usize;
    let request = worker.try_submit(DocumentRequest::DiscardHex { document, editor });
    let Err(TrySendError::Disconnected(request)) = request else {
        panic!("failed transport accepted an editor loan after retry ACK")
    };
    let DocumentRequest::DiscardHex { document: returned_document, editor } = *request else {
        panic!("failed admission changed the exact request owner")
    };
    assert_eq!(returned_document, document);
    assert_eq!(&*editor as *const Editor as usize, editor_identity);
    let receipt = observer(edit_receipt).expect_err("pre-failure observer clone must return its exact receipt");
    assert_eq!(
        (
            receipt.sequence,
            receipt.before_revision,
            receipt.after_revision,
            receipt.before.len(),
            receipt.after.len(),
            receipt.replacement_count(),
        ),
        expected_receipt
    );
    assert_eq!(worker.journal_capacity.load(Ordering::Acquire), 16);
    let failure = receiver.stop(worker).unwrap().settle(LIMIT, |_| ControlFlow::Continue(())).unwrap_err();
    assert_eq!(failure.reason, DocumentResultSettlementReason::WakeFailed);
    {
        let inspected = failure.ticket.inspect();
        assert!(inspected.pending().is_none());
        let Some(DocumentDeliveryFailure::Wake(error)) = inspected.delivery_failure() else {
            panic!("lost original wake error")
        };
        assert_eq!(error.get_ref().unwrap().downcast_ref::<WakeRaceError>().unwrap().0, 73);
    }
    let mut ticket = failure.ticket;
    assert!(matches!(ticket.acknowledge_delivery_failure(), Some(DocumentDeliveryFailure::Wake(_))));
    ticket.acknowledge_wrap_retry(refusal.token).unwrap();
    assert!(!lock(&shared.requests).accepting, "closed receiver wins even after explicit error acknowledgement");
    std::thread::spawn(move || {
        finish(ticket, |_| ControlFlow::Continue(()));
        drop((editor, receipt));
    })
    .join()
    .unwrap();
    assert_eq!(progressed.load(Ordering::SeqCst), 1);
    assert!(workspace.wrap_cleanup_ready());
}

#[test]
fn result_transport_wake_unwind_retains_original_panic_and_slot_on_the_existing_worker() {
    struct PanicOwner {
        identity: Box<usize>,
        dropped: mpsc::SyncSender<std::thread::ThreadId>,
    }
    impl Drop for PanicOwner {
        fn drop(&mut self) {
            self.dropped.try_send(std::thread::current().id()).unwrap();
        }
    }
    let ui = std::thread::current().id();
    let (panic_dropped, panic_drops) = mpsc::sync_channel(1);
    let panic_owner = PanicOwner { identity: Box::new(97), dropped: panic_dropped };
    let panic_identity = &*panic_owner.identity as *const usize as usize;
    let panic_owner = Mutex::new(Some(panic_owner));
    let (result_dropped, result_drops) = mpsc::sync_channel(1);
    let inspected = Arc::new(AtomicUsize::new(0));
    let (result, editor_identity) = loan(OwnedProbe { dropped: result_dropped, inspected: inspected.clone() });
    let wakes = Arc::new(AtomicUsize::new(0));
    let wake_count = wakes.clone();
    let progressed = Arc::new(AtomicUsize::new(0));
    let progress = progressed.clone();
    let (worker, mut receiver, shared) = harness(
        move || {
            wake_count.fetch_add(1, Ordering::SeqCst);
            let payload = lock(&panic_owner).take().expect("one explicit wake invocation");
            std::panic::panic_any(payload);
        },
        move |shared, _| {
            shared.publish(result);
            progress.fetch_add(1, Ordering::SeqCst);
            shared.publish(DocumentResult::Stopped(Ok(())));
        },
    );
    wait(&shared, |state| state.failure().is_some());
    assert!(matches!(
        receiver.try_recv(true),
        Err(DocumentResultReceiveError::Failed(DocumentResultSettlementReason::WakePanicked))
    ));
    assert_eq!(wakes.load(Ordering::SeqCst), 1, "a wake panic must never trigger automatic retry");
    assert_eq!(progressed.load(Ordering::SeqCst), 0);
    assert!(!receiver.status().worker_done, "callback panic must not unwind the existing document worker");
    assert!(lock(&shared.worker).as_ref().is_some_and(|worker| !worker.is_finished()));
    {
        let gate = lock(&shared.requests);
        assert!(!gate.accepting);
        assert!(gate.sender.is_some(), "logical unwind failure must retain the sole sender");
    }
    assert!(matches!(panic_drops.try_recv(), Err(mpsc::TryRecvError::Empty)));
    assert!(matches!(result_drops.try_recv(), Err(mpsc::TryRecvError::Empty)));
    assert_eq!(inspected.load(Ordering::SeqCst), 0);

    // UI receiver destruction cannot drop either original owner. Recover the
    // same quarantined worker/slot, rather than spawning a replacement lane.
    drop(receiver);
    let ticket = worker.stop_disconnected().ok().expect("recover the exact retained wake-unwind lifecycle");
    assert!(Arc::ptr_eq(&ticket.receiver.shared, &shared));
    drop(shared);
    let failure = ticket.settle(LIMIT, |_| panic!("unacknowledged panic must stop the owned consumer")).unwrap_err();
    assert_eq!(failure.reason, DocumentResultSettlementReason::WakePanicked);
    {
        let inspection = failure.ticket.inspect();
        let Some(DocumentDeliveryFailure::WakePanicked(payload)) = inspection.delivery_failure() else {
            panic!("original panic payload lost")
        };
        let owner = payload.downcast_ref::<PanicOwner>().expect("original Any+Send type retained");
        assert_eq!(&*owner.identity as *const usize as usize, panic_identity);
        match inspection.pending().unwrap() {
            DocumentResult::Replaced { editor, operation, .. } => {
                assert_eq!(&**editor as *const Editor as usize, editor_identity);
                assert_eq!(*operation, 19);
            }
            _ => panic!("panic changed or dropped the exact pending result"),
        }
    }
    let again = failure.ticket.settle(LIMIT, |_| ControlFlow::Continue(())).unwrap_err();
    assert_eq!(again.reason, DocumentResultSettlementReason::WakePanicked, "retry is not acknowledgement");
    assert!(matches!(panic_drops.try_recv(), Err(mpsc::TryRecvError::Empty)));
    assert!(matches!(result_drops.try_recv(), Err(mpsc::TryRecvError::Empty)));
    std::thread::spawn(move || {
        let mut ticket = again.ticket;
        let Some(DocumentDeliveryFailure::WakePanicked(payload)) = ticket.acknowledge_delivery_failure() else {
            panic!("explicit acknowledgement must return the original payload")
        };
        let owner = match payload.downcast::<PanicOwner>() {
            Ok(owner) => owner,
            Err(_) => panic!("original panic owner type"),
        };
        assert_eq!(&*owner.identity as *const usize as usize, panic_identity);
        drop(owner);
        finish(ticket, |result| {
            drop(result);
            ControlFlow::Continue(())
        });
    })
    .join()
    .unwrap();
    assert_ne!(panic_drops.recv_timeout(LIMIT).unwrap(), ui, "panic owner must retire only after OFF-UI ACK");
    assert_ne!(result_drops.recv_timeout(LIMIT).unwrap(), ui, "pending source owner must not Drop on UI");
    assert_eq!(wakes.load(Ordering::SeqCst), 1);
    assert_eq!(progressed.load(Ordering::SeqCst), 1);
    assert_eq!(inspected.load(Ordering::SeqCst), 0);
}
