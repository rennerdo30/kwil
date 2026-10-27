use super::*;
use std::{borrow::Cow, io, sync::mpsc, time::Duration};

fn settle(worker: &mut Worker, deadline: Instant) -> Result<(), WorkerSettlementReason> {
    loop {
        match worker.settle_until(deadline) {
            Err(WorkerSettlementReason::ThreadExitPending) if Instant::now() < deadline => thread::yield_now(),
            result => return result,
        }
    }
}
fn shared() -> Arc<Shared> {
    Arc::new(Shared {
        state: Mutex::new(State {
            pending: None,
            retired: [const { None }; RETIRE_CAPACITY],
            stopped: false,
            result: None,
        }),
        ready: Condvar::new(),
        generation: AtomicU64::new(0),
        completed: AtomicBool::new(false),
        native_exit: NativeExitObservation::new(),
    })
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
fn completion_ack_does_not_consume_original_thread_in_held_epilogue() {
    let deadline = Instant::now() + Duration::from_secs(2);
    let shared = shared();
    let original = Arc::clone(&shared);
    let (entered, observed) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let handle = thread::spawn(move || {
        let _epilogue = Epilogue { entered, release: gate, deadline };
        let _completion = Completion(&shared);
        shared.native_exit.register_current().unwrap();
    });
    let id = handle.thread().id();
    let mut worker = Worker {
        shared: Arc::clone(&original),
        thread: Some(handle),
        previous: None,
        panic: None,
        native_retired: false,
        settled: false,
    };
    observed.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
    assert!(original.completed.load(Ordering::Acquire));
    assert!(!worker.thread.as_ref().unwrap().is_finished());
    assert_eq!(
        worker.settle_with(deadline, NativeExitObservation::query, NativeExitObservation::retire_after_join),
        Err(WorkerSettlementReason::ThreadExitPending)
    );
    assert_eq!(worker.thread.as_ref().unwrap().thread().id(), id);
    assert!(Arc::ptr_eq(&worker.shared, &original));
    release.send(()).unwrap();
    assert_eq!(worker.settle_until(deadline), Ok(()));
    assert!(Instant::now() < deadline, "public settlement must complete under the original deadline");
    assert!(worker.thread.is_none());
    assert_eq!(worker.settle_until(deadline), Ok(()));
}

struct Source {
    entered: mpsc::Sender<()>,
    gate: Mutex<Option<mpsc::Receiver<()>>>,
    dropped: mpsc::Sender<thread::ThreadId>,
    deadline: Instant,
}
impl TextSource for Source {
    fn len(&self) -> usize {
        1
    }
    fn chunk(&self, _: usize, _: usize) -> io::Result<Cow<'_, [u8]>> {
        if let Some(gate) = self.gate.lock().unwrap().take() {
            self.entered.send(()).unwrap();
            gate.recv_timeout(self.deadline.saturating_duration_since(Instant::now())).unwrap();
        }
        Ok(Cow::Borrowed(b"x"))
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        self.dropped.send(thread::current().id()).unwrap();
    }
}
fn request(source: Arc<dyn TextSource>) -> Request {
    Request { source, language: Language::Rust, revision: 1, previous_revision: None, edit: None, visible: 0..1 }
}
#[test]
fn timeout_preserves_sources_and_refused_request_for_off_ui_retry() {
    let deadline = Instant::now() + Duration::from_secs(2);
    let caller = thread::current().id();
    let mut worker = Worker::new(Config { parse_limit: 0, ..Config::default() }, || {}).unwrap();
    let (entered, observed) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let (dropped, destroyed) = mpsc::channel();
    worker.submit(request(Arc::new(Source { entered, gate: Mutex::new(Some(gate)), dropped, deadline }))).unwrap();
    observed.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
    assert_eq!(worker.settle_until(Instant::now()), Err(WorkerSettlementReason::TimedOut));
    assert!(destroyed.try_recv().is_err());
    let refused_source: Arc<dyn TextSource> = Arc::new(vec![b'r']);
    let refused = worker.submit(request(Arc::clone(&refused_source))).unwrap_err();
    assert!(matches!(refused.error, Error::Closed));
    assert!(Arc::ptr_eq(&refused.request.source, &refused_source));
    release.send(()).unwrap();
    assert_eq!(settle(&mut worker, deadline), Ok(()));
    assert_ne!(destroyed.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap(), caller);
    assert_eq!(refused.request.source.len(), 1);
}

#[test]
fn worker_panic_retains_original_payload_across_repeated_settlement() {
    let deadline = Instant::now() + Duration::from_secs(2);
    let marker = Arc::new(29_u8);
    let expected = Arc::clone(&marker);
    let mut worker = Worker::new(Config { parse_limit: 0, ..Config::default() }, move || {
        std::panic::panic_any(Arc::clone(&marker));
    })
    .unwrap();
    worker.submit(request(Arc::new(vec![b'x']))).unwrap();
    // Wait for terminal ACK without signalling cancellation before the wake.
    let state = worker.shared.state.lock().unwrap();
    let (state, timeout) = worker
        .shared
        .ready
        .wait_timeout_while(state, deadline.saturating_duration_since(Instant::now()), |_| {
            !worker.shared.completed.load(Ordering::Acquire)
        })
        .unwrap_or_else(|error| error.into_inner());
    assert!(!timeout.timed_out());
    drop(state);
    assert_eq!(settle(&mut worker, deadline), Err(WorkerSettlementReason::WorkerPanicked));
    let payload = worker.panic_payload().unwrap().downcast_ref::<Arc<u8>>().unwrap();
    assert!(Arc::ptr_eq(payload, &expected));
    let address = payload as *const Arc<u8>;
    assert_eq!(worker.settle_until(deadline), Err(WorkerSettlementReason::WorkerPanicked));
    assert_eq!(worker.panic_payload().unwrap().downcast_ref::<Arc<u8>>().unwrap() as *const _, address);
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
    let shared = shared();
    let original = Arc::clone(&shared);
    let (entered, observed) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let handle = thread::spawn(move || {
        let _completion = Completion(&shared);
        shared.native_exit.register_current().unwrap();
        NATIVE_EPILOGUE.with(|slot| *slot.borrow_mut() = Some(NativeEpilogue { entered, release: gate, deadline }));
    });
    let id = handle.thread().id();
    let mut worker = Worker {
        shared: Arc::clone(&original),
        thread: Some(handle),
        panic: None,
        native_retired: false,
        settled: false,
        previous: None,
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

#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
#[test]
fn public_settlement_waits_for_held_native_tls_release_under_original_deadline() {
    let deadline = Instant::now() + Duration::from_secs(2);
    let shared = shared();
    let original = Arc::clone(&shared);
    let (entered, observed) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let handle = thread::spawn(move || {
        let _completion = Completion(&shared);
        shared.native_exit.register_current().unwrap();
        NATIVE_EPILOGUE.with(|slot| *slot.borrow_mut() = Some(NativeEpilogue { entered, release: gate, deadline }));
    });
    let id = handle.thread().id();
    let worker = Worker {
        shared: Arc::clone(&original),
        thread: Some(handle),
        panic: None,
        native_retired: false,
        settled: false,
        previous: None,
    };
    let entered = observed.recv_timeout(deadline.saturating_duration_since(Instant::now()));
    let acknowledged = original.completed.load(Ordering::Acquire);
    let (starting, started) = mpsc::sync_channel(1);
    let (finished, completion) = mpsc::sync_channel(1);
    let observer = thread::spawn(move || {
        let mut worker = worker;
        let retained_id = worker.thread.as_ref().unwrap().thread().id();
        let _ = starting.send(());
        let result = worker.settle_until(deadline); // actual public production wait
        let returned = Instant::now();
        let _ = finished.send(());
        (result, returned, retained_id, worker)
    });
    let started = started.recv_timeout(deadline.saturating_duration_since(Instant::now()));
    // A single nonblocking Pending implementation returns here while TLS is
    // held. A proper public wait stays pending until the gate is released.
    let early =
        completion.recv_timeout(deadline.saturating_duration_since(Instant::now()).min(Duration::from_millis(20)));
    let released = release.send(());
    let (result, returned, retained_id, mut worker) = observer.join().unwrap();
    let retained = Arc::ptr_eq(&worker.shared, &original);
    // Release and original-owner cleanup precede every assertion, including
    // failure of the single-query mutant. No new cleanup deadline is formed.
    let cleanup = worker.shutdown_in_place();
    assert!(entered.is_ok());
    assert!(acknowledged);
    assert!(started.is_ok());
    assert!(
        matches!(early, Err(mpsc::RecvTimeoutError::Timeout)),
        "public settlement returned while native TLS was held"
    );
    assert!(released.is_ok());
    assert_eq!(result, Ok(()));
    assert!(returned < deadline);
    assert_eq!(retained_id, id);
    assert!(retained);
    assert_eq!(cleanup, Ok(()));
    assert!(worker.thread.is_none(), "only the original native-exited handle may be joined: {id:?}");
    assert!(worker.native_retired);
}

#[test]
fn typed_query_failure_never_consumes_original_handle_even_after_cleanup_ack() {
    let deadline = Instant::now() + Duration::from_secs(2);
    let shared = shared();
    let original = Arc::clone(&shared);
    let (entered, observed) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let handle = thread::spawn(move || {
        let _epilogue = Epilogue { entered, release: gate, deadline };
        let _completion = Completion(&shared);
        shared.native_exit.register_current().unwrap();
    });
    let id = handle.thread().id();
    let mut worker = Worker {
        shared: Arc::clone(&original),
        thread: Some(handle),
        panic: None,
        native_retired: false,
        settled: false,
        previous: None,
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
    let shared = shared();
    let original = Arc::clone(&shared);
    let handle = thread::spawn(move || {
        let _completion = Completion(&shared);
        shared.native_exit.register_current().unwrap();
    });
    let mut worker = Worker {
        shared: Arc::clone(&original),
        thread: Some(handle),
        panic: None,
        native_retired: false,
        settled: false,
        previous: None,
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
    assert_eq!(Worker::checked_retained_metadata_bytes(), Some(Worker::retained_metadata_bytes()));
    assert!(Worker::retained_metadata_bytes() >= std::mem::size_of::<Shared>() + std::mem::size_of::<Worker>());
}

#[test]
fn panic_join_retires_right_before_sticky_error_and_preserves_payload_through_retirement_retry() {
    let deadline = Instant::now() + Duration::from_secs(2);
    let shared = shared();
    let original = Arc::clone(&shared);
    let marker = Arc::new(113_u8);
    let expected = Arc::clone(&marker);
    let handle = thread::spawn(move || {
        let _completion = Completion(&shared);
        shared.native_exit.register_current().unwrap();
        std::panic::panic_any(marker);
    });
    let mut worker = Worker {
        shared: Arc::clone(&original),
        thread: Some(handle),
        panic: None,
        native_retired: false,
        settled: false,
        previous: None,
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
    let shared = shared();
    let original = Arc::clone(&shared);
    let (entered, observed) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let handle = thread::spawn(move || {
        let _epilogue = Epilogue { entered, release: gate, deadline };
        let _completion = Completion(&shared);
        shared.native_exit.register_current().unwrap();
    });
    let id = handle.thread().id();
    let worker = Worker {
        shared: Arc::clone(&original),
        thread: Some(handle),
        panic: None,
        native_retired: false,
        settled: false,
        previous: None,
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
    let shared = shared();
    let original = Arc::clone(&shared);
    let handle = thread::spawn(move || {
        let _completion = Completion(&shared);
        shared.native_exit.register_current().unwrap();
    });
    // Explicit original join first; only the native retirement is clock-mocked.
    let joined = handle.join();
    let mut worker = Worker {
        shared: Arc::clone(&original),
        thread: None,
        panic: None,
        native_retired: false,
        settled: false,
        previous: None,
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
