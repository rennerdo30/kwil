use super::*;
use std::sync::mpsc;

// Test executor only: retry epilogue observations under ONE deadline. Production
// settlement neither polls nor refreshes the host's deadline.
pub(super) fn settle(worker: &mut SearchWorker, deadline: Instant) -> Result<(), WorkerSettlementReason> {
    loop {
        match worker.settle_until(deadline) {
            Err(WorkerSettlementReason::ThreadExitPending) if Instant::now() < deadline => thread::yield_now(),
            result => return result,
        }
    }
}

struct Epilogue {
    entered: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
    deadline: Instant,
}
impl Drop for Epilogue {
    fn drop(&mut self) {
        self.entered.send(()).unwrap();
        self.release.recv_timeout(self.deadline.saturating_duration_since(Instant::now())).unwrap();
    }
}

#[test]
fn cleanup_ack_retains_exact_unfinished_handle_and_shutdown_error_owner() {
    let deadline = Instant::now() + Duration::from_secs(2);
    let shared = Arc::new(Shared::new());
    let original = Arc::clone(&shared);
    let (entered, observed) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let handle = thread::spawn(move || {
        let _epilogue = Epilogue { entered, release: gate, deadline };
        let _completion = Completion(&shared);
        shared.native_exit.register_current().unwrap();
    });
    let id = handle.thread().id();
    let worker = SearchWorker {
        shared: Arc::clone(&original),
        thread: Some(handle),
        panic: None,
        native_retired: false,
        settled: false,
    };
    observed.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
    assert!(original.completed.load(Ordering::Acquire));
    assert!(!worker.thread.as_ref().unwrap().is_finished());
    let error = worker.shutdown_bounded(Duration::ZERO).unwrap_err();
    assert_eq!(error.reason(), WorkerSettlementReason::ThreadExitPending);
    error.retry_retirement();
    assert!(original.retirement_due.load(Ordering::Acquire));
    let mut worker = error.into_worker();
    assert!(Arc::ptr_eq(&worker.shared, &original));
    assert_eq!(worker.thread.as_ref().unwrap().thread().id(), id);
    release.send(()).unwrap();
    assert_eq!(settle(&mut worker, deadline), Ok(()));
    assert!(worker.thread.is_none(), "only successful join consumes the handle");
    assert_eq!(worker.settle_until(deadline), Ok(()));
}

#[test]
fn deadline_refusal_keeps_pending_input_and_retirement_controls() {
    let deadline = Instant::now() + Duration::from_secs(2);
    let shared = Arc::new(Shared::new());
    shared.state.lock().unwrap().pending = Some(Job {
        request: SearchRequest {
            key: SearchKey { document_id: 1, revision: 1, generation: 7 },
            task: SearchTask::Document(DocumentSearch {
                snapshot: Snapshot::default(),
                query: Query::literal("owned".into()),
                ranges: std::iter::once(0..0).collect(),
                page: Page::default(),
                navigation: None,
                replacement: None,
            }),
        },
        cancel: Arc::new(AtomicBool::new(false)),
    });
    let worker_shared = Arc::clone(&shared);
    let (release, gate) = mpsc::channel();
    let handle = thread::spawn(move || {
        let _completion = Completion(&worker_shared);
        worker_shared.native_exit.register_current().unwrap();
        gate.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
        // Same ownership rule as the coordinator: take under lock, drop outside.
        let pending = worker_shared.state.lock().unwrap().pending.take();
        drop(pending);
    });
    let id = handle.thread().id();
    let mut worker = SearchWorker {
        shared: Arc::clone(&shared),
        thread: Some(handle),
        panic: None,
        native_retired: false,
        settled: false,
    };
    assert_eq!(worker.settle_until(Instant::now()), Err(WorkerSettlementReason::TimedOut));
    assert_eq!(worker.thread.as_ref().unwrap().thread().id(), id);
    assert!(shared.state.lock().unwrap().pending.is_some());
    worker.retry_retirement();
    assert!(shared.retirement_due.load(Ordering::Acquire));
    release.send(()).unwrap();
    assert_eq!(settle(&mut worker, deadline), Ok(()));
    assert!(shared.state.lock().unwrap().pending.is_none());
}

#[test]
fn original_worker_panic_payload_is_retained_and_sticky() {
    let deadline = Instant::now() + Duration::from_secs(2);
    let marker = Arc::new(71_u8);
    let expected = Arc::clone(&marker);
    let mut worker = SearchWorker::new(move || std::panic::panic_any(Arc::clone(&marker))).unwrap();
    // Stop wakes the original coordinator, which invokes the actual host wake.
    worker.stop();
    assert_eq!(settle(&mut worker, deadline), Err(WorkerSettlementReason::WorkerPanicked));
    assert!(worker.thread.is_none());
    let payload = worker.panic_payload().unwrap().downcast_ref::<Arc<u8>>().unwrap();
    assert!(Arc::ptr_eq(payload, &expected));
    let address = payload as *const Arc<u8>;
    assert_eq!(worker.settle_until(deadline), Err(WorkerSettlementReason::WorkerPanicked));
    assert_eq!(worker.panic_payload().unwrap().downcast_ref::<Arc<u8>>().unwrap() as *const _, address);
    let error = worker.shutdown_bounded(Duration::ZERO).unwrap_err();
    assert_eq!(error.reason(), WorkerSettlementReason::WorkerPanicked);
    assert!(Arc::ptr_eq(error.into_worker().panic_payload().unwrap().downcast_ref::<Arc<u8>>().unwrap(), &expected));
}

// Platform native TLS teardown runs after the original worker body and its
// Completion guard. Rust is_finished is diagnostic only, never exit evidence.
struct NativeEpilogue {
    entered: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
    deadline: Instant,
}
impl Drop for NativeEpilogue {
    fn drop(&mut self) {
        // A broken bounded join can reach the same deadline while holding this
        // gate. Expiry releases it without a panic in a native TLS destructor.
        let _ = self.entered.send(());
        let _ = self.release.recv_timeout(self.deadline.saturating_duration_since(Instant::now()));
    }
}
thread_local! {
    static NATIVE_EPILOGUE: std::cell::RefCell<Option<NativeEpilogue>> = const { std::cell::RefCell::new(None) };
}
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
#[test]
fn native_tls_pending_preserves_original_owner_then_explicit_retry_joins_and_retires() {
    let deadline = Instant::now() + Duration::from_secs(2);
    let shared = Arc::new(Shared::new());
    let original = Arc::clone(&shared);
    let (entered, observed) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let handle = thread::spawn(move || {
        let _completion = Completion(&shared);
        shared.native_exit.register_current().unwrap();
        NATIVE_EPILOGUE.with(|slot| *slot.borrow_mut() = Some(NativeEpilogue { entered, release: gate, deadline }));
    });
    let id = handle.thread().id();
    let mut worker = SearchWorker {
        shared: Arc::clone(&original),
        thread: Some(handle),
        panic: None,
        native_retired: false,
        settled: false,
    };
    let entered = observed.recv_timeout(deadline.saturating_duration_since(Instant::now()));
    let completed = original.completed.load(Ordering::Acquire);
    #[cfg(windows)]
    while entered.is_ok() && !worker.thread.as_ref().unwrap().is_finished() && Instant::now() < deadline {
        thread::park_timeout(deadline.saturating_duration_since(Instant::now()).min(Duration::from_millis(1)));
    }
    let rust_finished = worker.thread.as_ref().unwrap().is_finished();
    // Inspect held TLS once; the public settlement wait runs only after release.
    let pending = worker.settle_with(deadline, NativeExitObservation::query, NativeExitObservation::retire_after_join);
    let retained = worker.thread.as_ref().map(|handle| handle.thread().id());
    let same_shared = Arc::ptr_eq(&worker.shared, &original);
    let released = release.send(());
    // The public wait, original join and retirement use the SAME 2s deadline.
    let result = worker.settle_until(deadline);
    let returned = Instant::now();
    // Release and original compatibility cleanup BEFORE assertions, even when
    // an observation failed. No gate or original handle is abandoned on panic.
    let cleanup = worker.shutdown_in_place();
    eprintln!("native TLS consumer: rust_finished={rust_finished} pending={pending:?}");
    assert!(entered.is_ok());
    assert!(completed);
    #[cfg(windows)]
    assert!(rust_finished, "the Windows native TLS fixture must reproduce the Rust completion gap");
    assert_eq!(pending, Err(WorkerSettlementReason::ThreadExitPending));
    assert_eq!(retained, Some(id));
    assert!(same_shared);
    assert!(released.is_ok());
    assert_eq!(result, Ok(()));
    assert!(returned < deadline, "public settlement must complete under the original deadline");
    assert_eq!(cleanup, Ok(()));
    assert!(worker.thread.is_none());
    assert!(worker.native_retired);
    assert_eq!(worker.settle_until(deadline), Ok(()));
}

#[test]
fn typed_query_failure_never_consumes_original_handle_even_after_cleanup_ack() {
    let deadline = Instant::now() + Duration::from_secs(2);
    let shared = Arc::new(Shared::new());
    let original = Arc::clone(&shared);
    let (entered, observed) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let handle = thread::spawn(move || {
        let _epilogue = Epilogue { entered, release: gate, deadline };
        let _completion = Completion(&shared);
        shared.native_exit.register_current().unwrap();
    });
    let id = handle.thread().id();
    let mut worker = SearchWorker {
        shared: Arc::clone(&original),
        thread: Some(handle),
        panic: None,
        native_retired: false,
        settled: false,
    };
    let entered = observed.recv_timeout(deadline.saturating_duration_since(Instant::now()));
    let original_error = NativeObservationError::MachValidate { code: i32::MIN + 17 };
    let query_count = std::cell::Cell::new(0);
    let queried_id = std::cell::Cell::new(None);
    let early_retire = std::cell::Cell::new(false);
    let refusal = worker.settle_with(
        deadline,
        |_, handle| {
            query_count.set(query_count.get() + 1);
            queried_id.set(Some(handle.thread().id()));
            Err(original_error)
        },
        |_| {
            early_retire.set(true);
            Err(original_error)
        },
    );
    let retained = worker.thread.as_ref().map(|handle| handle.thread().id());
    let released = release.send(());
    let cleanup = worker.shutdown_in_place();
    assert!(entered.is_ok());
    assert!(released.is_ok());
    assert_eq!(cleanup, Ok(()));
    assert_eq!(query_count.get(), 1);
    assert_eq!(queried_id.get(), Some(id));
    assert!(!early_retire.get());
    assert_eq!(refusal, Err(WorkerSettlementReason::NativeObservation(original_error)));
    assert_eq!(retained, Some(id));
    assert!(worker.native_retired);
}

#[test]
fn post_join_retirement_failure_keeps_same_owner_and_retries_without_query_or_second_join() {
    let deadline = Instant::now() + Duration::from_secs(2);
    let shared = Arc::new(Shared::new());
    let original = Arc::clone(&shared);
    let handle = thread::spawn(move || {
        let _completion = Completion(&shared);
        shared.native_exit.register_current().unwrap();
    });
    let mut worker = SearchWorker {
        shared: Arc::clone(&original),
        thread: Some(handle),
        panic: None,
        native_retired: false,
        settled: false,
    };
    let original_error = NativeObservationError::MachRetire { code: i32::MIN + 23 };
    let retirement_calls = std::cell::Cell::new(0);
    let mut result;
    loop {
        result = worker.settle_with(
            deadline,
            |observation, handle| observation.query(handle),
            |_| {
                retirement_calls.set(retirement_calls.get() + 1);
                Err(original_error)
            },
        );
        if result != Err(WorkerSettlementReason::ThreadExitPending) || Instant::now() >= deadline {
            break;
        }
        thread::yield_now();
    }
    let joined = worker.thread.is_none();
    let held = !worker.native_retired && Arc::ptr_eq(&worker.shared, &original);
    let retry_queries = std::cell::Cell::new(0);
    let retry = if joined {
        worker.settle_with(
            deadline,
            |_, _| {
                retry_queries.set(retry_queries.get() + 1);
                Err(original_error)
            },
            |observation| {
                retirement_calls.set(retirement_calls.get() + 1);
                observation.retire_after_join()
            },
        )
    } else {
        Err(WorkerSettlementReason::TimedOut)
    };
    let cleanup = worker.shutdown_in_place();
    assert_eq!(result, Err(WorkerSettlementReason::NativeObservation(original_error)));
    assert!(joined && held);
    assert_eq!(retirement_calls.get(), 2);
    assert_eq!(retry_queries.get(), 0);
    assert_eq!(retry, Ok(()));
    assert_eq!(cleanup, Ok(()));
    assert!(worker.native_retired);
}

#[test]
fn typed_owners_are_send_sync_and_declared_metadata_is_checked() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<ShutdownError>();
    assert_eq!(SearchWorker::checked_retained_metadata_bytes(), Some(SearchWorker::retained_metadata_bytes()));
    assert!(
        SearchWorker::retained_metadata_bytes() >= std::mem::size_of::<Shared>() + std::mem::size_of::<SearchWorker>()
    );
}

#[test]
fn panic_join_retires_right_before_sticky_error_and_preserves_payload_through_retirement_retry() {
    let deadline = Instant::now() + Duration::from_secs(2);
    let shared = Arc::new(Shared::new());
    let original = Arc::clone(&shared);
    let marker = Arc::new(113_u8);
    let expected = Arc::clone(&marker);
    let handle = thread::spawn(move || {
        let _completion = Completion(&shared);
        shared.native_exit.register_current().unwrap();
        std::panic::panic_any(marker);
    });
    let mut worker = SearchWorker {
        shared: Arc::clone(&original),
        thread: Some(handle),
        panic: None,
        native_retired: false,
        settled: false,
    };
    let first_error = NativeObservationError::MachRetire { code: -701 };
    let calls = std::cell::Cell::new(0);
    let mut failure;
    loop {
        failure = worker.settle_with(
            deadline,
            |observation, handle| observation.query(handle),
            |_| {
                calls.set(calls.get() + 1);
                Err(first_error)
            },
        );
        if failure != Err(WorkerSettlementReason::ThreadExitPending) || Instant::now() >= deadline {
            break;
        }
        thread::yield_now();
    }
    let joined = worker.thread.is_none();
    let right_retained = !worker.native_retired;
    let payload = worker.panic_payload().and_then(|panic| panic.downcast_ref::<Arc<u8>>());
    let same_payload = payload.is_some_and(|payload| Arc::ptr_eq(payload, &expected));
    let address = payload.map(|payload| payload as *const Arc<u8>);
    // A post-join retry cannot query or join; it only retires the exact witness.
    let retry = if joined {
        worker.settle_with(
            deadline,
            |_, _| Err(NativeObservationError::ForeignThread),
            |observation| {
                calls.set(calls.get() + 1);
                observation.retire_after_join()
            },
        )
    } else {
        Err(WorkerSettlementReason::TimedOut)
    };
    let cleanup = worker.shutdown_in_place();
    assert_eq!(failure, Err(WorkerSettlementReason::NativeObservation(first_error)));
    assert!(joined && right_retained && same_payload);
    assert_eq!(calls.get(), 2);
    assert_eq!(retry, Err(WorkerSettlementReason::WorkerPanicked));
    assert_eq!(cleanup, Err(WorkerSettlementReason::WorkerPanicked));
    assert!(worker.native_retired, "panic must not bypass explicit right retirement");
    assert_eq!(worker.panic_payload().unwrap().downcast_ref::<Arc<u8>>().map(|payload| payload as *const _), address);
    assert_eq!(worker.settle_until(deadline), Err(WorkerSettlementReason::WorkerPanicked));
}

#[test]
fn owned_shutdown_until_refusal_returns_the_exact_original_facade_and_handle() {
    let deadline = Instant::now() + Duration::from_secs(2);
    let shared = Arc::new(Shared::new());
    let original = Arc::clone(&shared);
    let (entered, observed) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let handle = thread::spawn(move || {
        let _epilogue = Epilogue { entered, release: gate, deadline };
        let _completion = Completion(&shared);
        shared.native_exit.register_current().unwrap();
    });
    let id = handle.thread().id();
    let worker = SearchWorker {
        shared: Arc::clone(&original),
        thread: Some(handle),
        panic: None,
        native_retired: false,
        settled: false,
    };
    let entered = observed.recv_timeout(deadline.saturating_duration_since(Instant::now()));
    // This thread owns the epilogue release. Refuse before spending its
    // cleanup deadline waiting for that same release.
    let refusal = worker.shutdown_until(Instant::now());
    let released = release.send(());
    let (reason, retained, cleanup) = match refusal {
        Err(error) => {
            let reason = error.reason();
            let mut worker = error.into_worker();
            let retained = Arc::ptr_eq(&worker.shared, &original)
                && worker.thread.as_ref().map(|handle| handle.thread().id()) == Some(id);
            (Some(reason), retained, worker.shutdown_in_place())
        }
        Ok(()) => (None, false, Ok(())),
    };
    assert!(entered.is_ok());
    assert!(released.is_ok());
    assert_eq!(reason, Some(WorkerSettlementReason::ThreadExitPending));
    assert!(retained);
    assert_eq!(cleanup, Ok(()));
}

#[test]
fn late_native_retirement_is_not_on_time_success_and_same_expired_deadline_cannot_refresh_it() {
    let deadline = Instant::now() + Duration::from_secs(2);
    let shared = Arc::new(Shared::new());
    let original = Arc::clone(&shared);
    let handle = thread::spawn(move || {
        let _completion = Completion(&shared);
        shared.native_exit.register_current().unwrap();
    });
    // Explicit original join first; only the native retirement is clock-mocked.
    let joined = handle.join();
    let mut worker = SearchWorker {
        shared: Arc::clone(&original),
        thread: None,
        panic: None,
        native_retired: false,
        settled: false,
    };
    let clock = std::cell::Cell::new(Instant::now());
    let late = worker.finish_join_with_clock(
        Some(deadline),
        |observation| {
            let result = observation.retire_after_join();
            clock.set(deadline);
            result
        },
        || clock.get(),
    );
    let retired = worker.native_retired;
    let retry = worker.finish_join_with_clock(
        Some(deadline),
        |_| panic!("already retired right cannot close again"),
        || clock.get(),
    );
    let still_not_settled = !worker.settled;
    let cleanup = worker.shutdown_in_place();
    assert!(joined.is_ok());
    assert_eq!(late, Err(WorkerSettlementReason::TimedOut));
    assert!(retired);
    assert_eq!(retry, Err(WorkerSettlementReason::TimedOut));
    assert!(still_not_settled);
    assert_eq!(cleanup, Ok(()));
}
