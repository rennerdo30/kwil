use super::*;
use std::time::Duration;

fn settle(session: &mut LogSession, deadline: Instant) -> Result<(), LogSettlementReason> {
    match crate::cleanup_test_support::cleanup_until(
        session,
        deadline,
        |owner, deadline| {
            owner
                .settle_until(deadline)
                .map_or_else(|error| std::ops::ControlFlow::Break((owner, error)), std::ops::ControlFlow::Continue)
        },
        |error| matches!(error, LogSettlementReason::ThreadExitPending),
    )
    .into_flow()
    {
        std::ops::ControlFlow::Continue(value) => Ok(value),
        std::ops::ControlFlow::Break(crate::cleanup_test_support::CleanupFailure::Failed { error, .. }) => Err(error),
        std::ops::ControlFlow::Break(
            crate::cleanup_test_support::CleanupFailure::Expired { .. }
            | crate::cleanup_test_support::CleanupFailure::CompletedLate { .. },
        ) => Err(LogSettlementReason::TimedOut),
    }
}
fn shared() -> Arc<Shared> {
    let (signal, _) = mpsc::sync_channel(1);
    Arc::new(Shared {
        mailbox: Mutex::new(Mailbox::default()),
        latest: AtomicU64::new(0),
        closed: AtomicBool::new(false),
        retry_wake: AtomicBool::new(false),
        retire_index: AtomicBool::new(false),
        retirement_error: Mutex::new(None),
        signal,
        completed: AtomicBool::new(false),
        native_exit: NativeExitObservation::new(),
        completion: Condvar::new(),
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
fn snapshot() -> Snapshot {
    let mut buffer = kwil_core_text::Buffer::empty();
    buffer.insert(0, b"INFO owned").unwrap();
    buffer.snapshot()
}
#[test]
fn cleanup_ack_retains_exact_log_thread_and_status_until_successful_join() {
    let deadline = Instant::now() + Duration::from_secs(2);
    let shared = shared();
    let original = Arc::clone(&shared);
    let (entered, observed) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let handle = thread::spawn(move || {
        let _epilogue = Epilogue { entered, release: gate, deadline };
        let _completion = Completion(&shared);
        shared.native_exit.register_current().unwrap();
        let _retirement = WorkerRetirement(Arc::clone(&shared));
    });
    let id = handle.thread().id();
    let mut session = LogSession {
        shared: Arc::clone(&original),
        worker: Some(handle),
        generation: 7,
        panic: None,
        native_retired: false,
        settled: false,
    };
    let status = session.retirement_status();
    observed.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
    assert!(original.completed.load(Ordering::Acquire));
    assert!(!session.worker.as_ref().unwrap().is_finished());
    assert_eq!(
        session.settle_with(deadline, NativeExitObservation::query, NativeExitObservation::retire_after_join),
        Err(LogSettlementReason::ThreadExitPending)
    );
    assert_eq!(session.worker.as_ref().unwrap().thread().id(), id);
    assert!(Arc::ptr_eq(&session.shared, &original));
    assert!(Arc::ptr_eq(&status.0, &original));
    release.send(()).unwrap();
    assert_eq!(session.settle_until(deadline), Ok(()));
    assert!(Instant::now() < deadline, "public settlement must complete under the original deadline");
    assert!(session.worker.is_none());
    assert_eq!(session.settle_until(deadline), Ok(()));
    assert!(status.result().is_ok());
}

#[test]
fn timeout_keeps_original_log_owner_and_returns_refused_snapshot() {
    let deadline = Instant::now() + Duration::from_secs(2);
    let (entered, observed) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let gate = Mutex::new(Some(gate));
    let mut session = LogSession::start(move || {
        if let Some(gate) = gate.lock().unwrap().take() {
            entered.send(()).unwrap();
            gate.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
        }
    })
    .unwrap();
    session.submit(1, 2, 3, snapshot(), Filter::default(), 0).unwrap();
    observed.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
    let id = session.worker.as_ref().unwrap().thread().id();
    let original = Arc::clone(&session.shared);
    assert_eq!(session.settle_until(Instant::now()), Err(LogSettlementReason::TimedOut));
    assert_eq!(session.worker.as_ref().unwrap().thread().id(), id);
    assert!(Arc::ptr_eq(&session.shared, &original));
    let generation = session.generation;
    let refused = session.submit(4, 5, 6, snapshot(), Filter::default(), 0).unwrap_err();
    assert_eq!(refused.reason, "log session stopped");
    assert_eq!(refused.snapshot.len(), 10);
    assert_eq!(session.generation, generation, "refusal must not advance identity");
    let refused = session.retire_snapshot(refused.snapshot).unwrap_err();
    assert_eq!(refused.reason, "log session stopped");
    assert_eq!(refused.snapshot.len(), 10, "retry refusal keeps the exact source owner");
    release.send(()).unwrap();
    assert_eq!(settle(&mut session, deadline), Ok(()));
    assert!(original.mailbox.lock().unwrap().pending.is_none());
    drop(refused); // explicit disposal on this off-UI test executor, after join.
}

#[test]
fn original_log_panic_payload_stays_in_same_session_across_retries() {
    let deadline = Instant::now() + Duration::from_secs(2);
    let marker = Arc::new(53_u8);
    let expected = Arc::clone(&marker);
    let mut session = LogSession::start(move || std::panic::panic_any(Arc::clone(&marker))).unwrap();
    session.submit(1, 1, 1, snapshot(), Filter::default(), 0).unwrap();
    let mailbox = session.shared.mailbox.lock().unwrap();
    let (mailbox, timeout) = session
        .shared
        .completion
        .wait_timeout_while(mailbox, deadline.saturating_duration_since(Instant::now()), |_| {
            !session.shared.completed.load(Ordering::Acquire)
        })
        .unwrap_or_else(|error| error.into_inner());
    assert!(!timeout.timed_out());
    drop(mailbox);
    assert_eq!(settle(&mut session, deadline), Err(LogSettlementReason::WorkerPanicked));
    let payload = session.panic_payload().unwrap().downcast_ref::<Arc<u8>>().unwrap();
    assert!(Arc::ptr_eq(payload, &expected));
    let address = payload as *const Arc<u8>;
    assert_eq!(session.settle_until(deadline), Err(LogSettlementReason::WorkerPanicked));
    assert_eq!(session.panic_payload().unwrap().downcast_ref::<Arc<u8>>().unwrap() as *const _, address);
    assert!(session.worker.is_none());
    assert_eq!(
        session.retry_retirement_with(deadline, || panic!("panicked worker cannot reap"), Instant::now),
        Err(LogRetirementRetryReason::WorkerNotJoined)
    );
    assert!(Arc::ptr_eq(session.panic_payload().unwrap().downcast_ref::<Arc<u8>>().unwrap(), &expected));
}

#[test]
fn settlement_does_not_erase_existing_physical_retirement_failure() {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut session = LogSession::start(|| {}).unwrap();
    *session.shared.retirement_error.lock().unwrap() = Some("exact unlink failure still owned by registry".into());
    let status = session.retirement_status();
    assert_eq!(settle(&mut session, deadline), Ok(()));
    assert_eq!(status.result().unwrap_err(), "exact unlink failure still owned by registry");
    assert_eq!(session.retirement_status().result().unwrap_err(), status.result().unwrap_err());
}

fn joined(deadline: Instant) -> LogSession {
    let mut session = LogSession::start(|| {}).unwrap();
    assert_eq!(settle(&mut session, deadline), Ok(()));
    session
}
fn leases() -> [Option<IndexLease>; INDEX_OWNERS] {
    [
        Some(IndexLease { path: "registered-inactive-first".into(), bytes: 96, active: false }),
        Some(IndexLease { path: "registered-active-never-touch".into(), bytes: 32, active: true }),
        Some(IndexLease { path: "registered-inactive-last".into(), bytes: 64, active: false }),
        None,
    ]
}
fn charge(leases: &[Option<IndexLease>; INDEX_OWNERS]) -> u64 {
    leases.iter().flatten().map(|lease| lease.bytes).sum()
}
fn first_error(session: &LogSession) -> (*const u8, usize, String) {
    let diagnostic = session.shared.retirement_error.lock().unwrap();
    let first = diagnostic.as_ref().unwrap();
    (first.as_ptr(), first.capacity(), first.clone())
}

#[test]
fn retry_before_actual_successful_join_refuses_without_any_registry_operation() {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut session = LogSession::start(|| {}).unwrap();
    let original = Arc::clone(&session.shared);
    let id = session.worker.as_ref().unwrap().thread().id();
    *session.shared.retirement_error.lock().unwrap() = Some("first retirement diagnostic".into());
    let first = first_error(&session);
    assert_eq!(
        session.retry_retirement_with(deadline, || panic!("unjoined worker cannot reap"), Instant::now),
        Err(LogRetirementRetryReason::WorkerNotJoined)
    );
    assert_eq!(session.worker.as_ref().unwrap().thread().id(), id);
    assert!(Arc::ptr_eq(&session.shared, &original));
    assert_eq!(first_error(&session), first);
    assert_eq!(settle(&mut session, deadline), Ok(()));
}

#[test]
fn simulated_unlink_failure_retains_exact_registered_charge_and_first_error_until_explicit_success() {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut session = joined(deadline);
    let original = Arc::clone(&session.shared);
    let status = session.retirement_status();
    let mut leases = leases();
    let first_path = leases[0].as_ref().unwrap().path.clone();
    let active_path = leases[1].as_ref().unwrap().path.clone();
    let last_path = leases[2].as_ref().unwrap().path.clone();
    // The actual existing reaper simulates the original producer unlink fault.
    // Its String diagnostic is retained; this does not preserve io::Error itself.
    let original_error = reap_indexes_with(&mut leases, |_| {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, "original simulated unlink failure"))
    })
    .unwrap_err();
    *session.shared.retirement_error.lock().unwrap() = Some(original_error.to_string());
    let first = first_error(&session);
    let mut attempted = Vec::new();
    assert_eq!(
        session.retry_retirement_with(
            deadline,
            || {
                reap_indexes_until_with(
                    &mut leases,
                    deadline,
                    |path| {
                        attempted.push(path.to_owned());
                        Err(io::Error::other("different retry failure must not replace first"))
                    },
                    Instant::now,
                )
            },
            Instant::now
        ),
        Err(LogRetirementRetryReason::Failed)
    );
    assert_eq!(attempted.as_slice(), std::slice::from_ref(&first_path));
    assert_eq!(charge(&leases), 192);
    assert_eq!(leases[0].as_ref().unwrap().path, first_path);
    assert_eq!(leases[0].as_ref().unwrap().bytes, 96);
    assert!(!leases[0].as_ref().unwrap().active);
    assert_eq!(leases[1].as_ref().unwrap().path, active_path);
    assert!(leases[1].as_ref().unwrap().active);
    assert_eq!(leases[2].as_ref().unwrap().path, last_path);
    assert_eq!(first_error(&session), first, "failed retry must keep the SAME first String allocation");
    assert_eq!(status.result().unwrap_err(), first.2);
    attempted.clear();
    assert_eq!(
        session.retry_retirement_with(
            deadline,
            || {
                reap_indexes_until_with(
                    &mut leases,
                    deadline,
                    |path| {
                        attempted.push(path.to_owned());
                        if path == last_path.as_path() { Err(io::Error::from(io::ErrorKind::NotFound)) } else { Ok(()) }
                    },
                    Instant::now,
                )
            },
            Instant::now
        ),
        Ok(())
    );
    assert_eq!(attempted, [first_path, last_path]);
    assert!(attempted.len() <= INDEX_OWNERS);
    assert!(leases[0].is_none() && leases[2].is_none() && leases[3].is_none());
    assert_eq!(charge(&leases), 32, "only exact unlink/NotFound may release charges");
    assert_eq!(leases[1].as_ref().unwrap().path, active_path);
    assert!(leases[1].as_ref().unwrap().active);
    assert!(Arc::ptr_eq(&session.shared, &original));
    assert!(status.result().is_ok());
    assert!(session.shared.retirement_error.lock().unwrap().is_none());
}

#[test]
fn expired_retry_does_no_unlink_and_preserves_existing_diagnostic() {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut session = joined(deadline);
    *session.shared.retirement_error.lock().unwrap() = Some("first diagnostic remains on timeout".into());
    let first = first_error(&session);
    assert_eq!(session.retry_retirement_until(Instant::now()), Err(LogRetirementRetryReason::TimedOut));
    assert_eq!(first_error(&session), first);
    let mut leases = leases();
    assert_eq!(
        reap_indexes_until_with(&mut leases, deadline, |_| panic!("expired pass cannot unlink"), || deadline),
        Err(LogRetirementRetryReason::TimedOut)
    );
    assert_eq!(charge(&leases), 192);
    assert_eq!(first_error(&session), first);
}

#[test]
fn late_success_retires_only_actual_unlink_and_never_clears_first_error_or_starts_next_io() {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut session = joined(deadline);
    *session.shared.retirement_error.lock().unwrap() = Some("first diagnostic survives late success".into());
    let first = first_error(&session);
    let mut leases = leases();
    let first_path = leases[0].as_ref().unwrap().path.clone();
    let clock = std::cell::Cell::new(Instant::now());
    let mut attempted = Vec::new();
    assert_eq!(
        session.retry_retirement_with(
            deadline,
            || {
                reap_indexes_until_with(
                    &mut leases,
                    deadline,
                    |path| {
                        attempted.push(path.to_owned());
                        clock.set(deadline); // mocked slow unlink completes at the original deadline.
                        Ok(())
                    },
                    || clock.get(),
                )
            },
            || clock.get()
        ),
        Err(LogRetirementRetryReason::TimedOut)
    );
    assert_eq!(attempted, [first_path]);
    assert!(leases[0].is_none(), "a real successful unlink must release its charge even when late");
    assert_eq!(leases[2].as_ref().unwrap().bytes, 64);
    assert!(leases[1].as_ref().unwrap().active);
    assert_eq!(charge(&leases), 96);
    assert_eq!(first_error(&session), first);
}

#[test]
fn late_failed_unlink_keeps_exact_owner_charge_and_first_error() {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut session = joined(deadline);
    *session.shared.retirement_error.lock().unwrap() = Some("first diagnostic survives late failure".into());
    let first = first_error(&session);
    let mut leases = leases();
    let first_path = leases[0].as_ref().unwrap().path.clone();
    let clock = std::cell::Cell::new(Instant::now());
    let mut attempted = Vec::new();
    assert_eq!(
        session.retry_retirement_with(
            deadline,
            || {
                reap_indexes_until_with(
                    &mut leases,
                    deadline,
                    |path| {
                        attempted.push(path.to_owned());
                        clock.set(deadline);
                        Err(io::Error::from(io::ErrorKind::PermissionDenied))
                    },
                    || clock.get(),
                )
            },
            || clock.get()
        ),
        Err(LogRetirementRetryReason::TimedOut)
    );
    assert_eq!(attempted.as_slice(), std::slice::from_ref(&first_path));
    assert_eq!(leases[0].as_ref().unwrap().path, first_path);
    assert_eq!(leases[0].as_ref().unwrap().bytes, 96);
    assert!(!leases[0].as_ref().unwrap().active);
    assert!(leases[1].as_ref().unwrap().active);
    assert_eq!(charge(&leases), 192);
    assert_eq!(first_error(&session), first);
}

#[test]
fn full_reap_cannot_clear_diagnostic_when_status_lock_is_busy() {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut session = joined(deadline);
    let shared = Arc::clone(&session.shared);
    *shared.retirement_error.lock().unwrap() = Some("first diagnostic stays owned".into());
    let first = first_error(&session);
    let guard = shared.retirement_error.lock().unwrap();
    let mut empty = [const { None }; INDEX_OWNERS];
    assert_eq!(
        session.retry_retirement_with(
            deadline,
            || {
                reap_indexes_until_with(&mut empty, deadline, |_| panic!("empty registry has no unlink"), Instant::now)
            },
            Instant::now
        ),
        Err(LogRetirementRetryReason::Failed)
    );
    assert_eq!(guard.as_ref().unwrap().as_ptr(), first.0);
    assert_eq!(guard.as_ref().unwrap().as_str(), first.2);
    drop(guard);
    assert_eq!(first_error(&session), first);
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
    let mut worker = LogSession {
        shared: Arc::clone(&original),
        worker: Some(handle),
        panic: None,
        native_retired: false,
        settled: false,
        generation: 1,
    };
    let entered = observed.recv_timeout(deadline.saturating_duration_since(Instant::now()));
    let completed = original.completed.load(Ordering::Acquire);
    #[cfg(windows)]
    while entered.is_ok() && !worker.worker.as_ref().unwrap().is_finished() && Instant::now() < deadline {
        thread::park_timeout(deadline.saturating_duration_since(Instant::now()).min(Duration::from_millis(1)));
    }
    let rust_finished = worker.worker.as_ref().unwrap().is_finished();
    // Inspect held TLS once; the public settlement wait runs only after release.
    let pending = worker.settle_with(deadline, NativeExitObservation::query, NativeExitObservation::retire_after_join);
    let retained = worker.worker.as_ref().map(|handle| handle.thread().id());
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
    assert_eq!(pending, Err(LogSettlementReason::ThreadExitPending));
    assert_eq!(retained, Some(id));
    assert!(same_shared);
    assert!(released.is_ok());
    assert_eq!(result, Ok(()));
    assert!(returned < deadline, "public settlement must complete under the original deadline");
    assert_eq!(cleanup, Ok(()));
    assert!(worker.worker.is_none());
    assert!(worker.native_retired);
    assert_eq!(worker.settle_until(deadline), Ok(()));
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
    let mut worker = LogSession {
        shared: Arc::clone(&original),
        worker: Some(handle),
        panic: None,
        native_retired: false,
        settled: false,
        generation: 1,
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
    let retained = worker.worker.as_ref().map(|handle| handle.thread().id());
    let released = release.send(());
    let cleanup = worker.shutdown_in_place();
    assert!(entered.is_ok());
    assert!(released.is_ok());
    assert_eq!(cleanup, Ok(()));
    assert_eq!(query_count.get(), 1);
    assert_eq!(queried_id.get(), Some(id));
    assert!(!early_retire.get());
    assert_eq!(refusal, Err(LogSettlementReason::NativeObservation(original_error)));
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
    let mut worker = LogSession {
        shared: Arc::clone(&original),
        worker: Some(handle),
        panic: None,
        native_retired: false,
        settled: false,
        generation: 1,
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
        if result != Err(LogSettlementReason::ThreadExitPending) || Instant::now() >= deadline {
            break;
        }
        thread::yield_now();
    }
    let joined = worker.worker.is_none();
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
        Err(LogSettlementReason::TimedOut)
    };
    let cleanup = worker.shutdown_in_place();
    assert_eq!(result, Err(LogSettlementReason::NativeObservation(original_error)));
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
    send_sync::<LogShutdownError>();
    assert_eq!(LogSession::checked_retained_metadata_bytes(), Some(LogSession::retained_metadata_bytes()));
    assert!(LogSession::retained_metadata_bytes() >= std::mem::size_of::<Shared>() + std::mem::size_of::<LogSession>());
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
    let mut worker = LogSession {
        shared: Arc::clone(&original),
        worker: Some(handle),
        panic: None,
        native_retired: false,
        settled: false,
        generation: 1,
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
        if failure != Err(LogSettlementReason::ThreadExitPending) || Instant::now() >= deadline {
            break;
        }
        thread::yield_now();
    }
    let joined = worker.worker.is_none();
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
        Err(LogSettlementReason::TimedOut)
    };
    let cleanup = worker.shutdown_in_place();
    assert_eq!(failure, Err(LogSettlementReason::NativeObservation(first_error)));
    assert!(joined && right_retained && same_payload);
    assert_eq!(calls.get(), 2);
    assert_eq!(retry, Err(LogSettlementReason::WorkerPanicked));
    assert_eq!(cleanup, Err(LogSettlementReason::WorkerPanicked));
    assert!(worker.native_retired, "panic must not bypass explicit right retirement");
    assert_eq!(worker.panic_payload().unwrap().downcast_ref::<Arc<u8>>().map(|payload| payload as *const _), address);
    assert_eq!(worker.settle_until(deadline), Err(LogSettlementReason::WorkerPanicked));
}

#[test]
fn stop_ticket_retains_whole_original_session_until_explicit_borrowed_settlement() {
    let deadline = Instant::now() + Duration::from_secs(2);
    let session = LogSession::start(|| {}).unwrap();
    let original = Arc::clone(&session.shared);
    let id = session.worker.as_ref().unwrap().thread().id();
    let status = session.retirement_status();
    let ticket = session.stop();
    let mut session = ticket.into_session();
    let retained = Arc::ptr_eq(&session.shared, &original)
        && session.worker.as_ref().map(|handle| handle.thread().id()) == Some(id);
    let settled = settle(&mut session, deadline);
    let completed_right = session.native_retired;
    let cleanup = session.stop().join();
    assert!(retained);
    assert_eq!(settled, Ok(()));
    assert!(completed_right);
    assert!(cleanup.is_ok());
    assert!(Arc::ptr_eq(&status.0, &original));
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
    let worker = LogSession {
        shared: Arc::clone(&original),
        worker: Some(handle),
        panic: None,
        native_retired: false,
        settled: false,
        generation: 19,
    };
    let entered = observed.recv_timeout(deadline.saturating_duration_since(Instant::now()));
    let refusal = worker.shutdown_until(deadline);
    let released = release.send(());
    let (reason, retained, cleanup) = match refusal {
        Err(error) => {
            let reason = error.reason();
            let mut worker = error.into_session();
            let retained = Arc::ptr_eq(&worker.shared, &original)
                && worker.worker.as_ref().map(|handle| handle.thread().id()) == Some(id);
            (Some(reason), retained, worker.shutdown_in_place())
        }
        Ok(()) => (None, false, Ok(())),
    };
    assert!(entered.is_ok());
    assert!(released.is_ok());
    assert_eq!(reason, Some(LogSettlementReason::ThreadExitPending));
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
    let mut worker = LogSession {
        shared: Arc::clone(&original),
        worker: None,
        panic: None,
        native_retired: false,
        settled: false,
        generation: 19,
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
    assert_eq!(late, Err(LogSettlementReason::TimedOut));
    assert!(retired);
    assert_eq!(retry, Err(LogSettlementReason::TimedOut));
    assert!(still_not_settled);
    assert_eq!(cleanup, Ok(()));
}
