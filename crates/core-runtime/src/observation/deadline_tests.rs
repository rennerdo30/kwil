use super::*;
use std::cell::Cell;

#[test]
fn expired_deadline_performs_no_query_or_wait() {
    let deadline = Instant::now();
    for now in [deadline, deadline + Duration::from_nanos(1)] {
        assert_eq!(
            wait_until_with(deadline, || now, || panic!("expired query"), |_| panic!("expired wait")),
            Ok(NativeExitState::Pending)
        );
    }
}

#[test]
fn pending_then_exited_uses_passive_bounded_wait_and_original_deadline() {
    let start = Instant::now();
    let deadline = start + Duration::from_millis(3);
    let clock = Cell::new(start);
    let queries = Cell::new(0);
    let waits = Cell::new(0);
    let result = wait_until_with(
        deadline,
        || clock.get(),
        || {
            queries.set(queries.get() + 1);
            Ok(if queries.get() == 1 { NativeExitState::Pending } else { NativeExitState::Exited })
        },
        |duration| {
            assert_eq!(duration, Duration::from_millis(1));
            waits.set(waits.get() + 1);
            clock.set(clock.get() + duration);
        },
    );
    assert_eq!(result, Ok(NativeExitState::Exited));
    assert_eq!((queries.get(), waits.get()), (2, 1));
    assert!(clock.get() < deadline);
}

#[test]
fn pending_to_expiry_does_not_query_again_or_retire_the_original_owner() {
    let observation = NativeExitObservation::new();
    let identity = Identity::current();
    observation.register_with(identity, || Attempt { witness: Witness::Mock(73), error: None }).unwrap();
    let start = Instant::now();
    let deadline = start + Duration::from_micros(1500);
    let clock = Cell::new(start);
    let queries = Cell::new(0);
    let waits = Cell::new(0);
    let result = observation.query_with(identity.thread, identity.process, |witness| {
        wait_until_with(
            deadline,
            || clock.get(),
            || {
                assert!(matches!(witness, Witness::Mock(73)));
                queries.set(queries.get() + 1);
                Ok(NativeExitState::Pending)
            },
            |duration| {
                assert_eq!(
                    observation.retire_with(identity.process, |_| panic!("wait must exclude retirement")),
                    Err(NativeObservationError::Busy)
                );
                assert_eq!(
                    duration,
                    if waits.get() == 0 { Duration::from_millis(1) } else { Duration::from_micros(500) }
                );
                waits.set(waits.get() + 1);
                clock.set(clock.get() + duration);
            },
        )
    });
    assert_eq!(result, Ok(NativeExitState::Pending));
    assert_eq!(clock.get(), deadline);
    assert_eq!((queries.get(), waits.get()), (2, 2));
    assert_eq!(observation.state.load(Ordering::Acquire), REGISTERED);
    assert_eq!(
        observation.query_with(identity.thread, identity.process, |_| Ok(NativeExitState::Exited)),
        Ok(NativeExitState::Exited)
    );
    observation.retire_with(identity.process, |_| Ok(())).unwrap();
}

#[test]
fn fresh_query_errors_survive_even_when_the_query_returns_after_deadline() {
    let start = Instant::now();
    let deadline = start + Duration::from_millis(2);
    for error in [
        NativeObservationError::LinuxQuery { errno: 5 },
        NativeObservationError::MachQuery { code: i32::MIN },
        NativeObservationError::WindowsQuery(NativeExitQueryError::WindowsWaitFailed { code: u32::MAX }),
    ] {
        for late in [false, true] {
            let clock = Cell::new(start);
            let clock_reads = Cell::new(0);
            let queries = Cell::new(0);
            let result = wait_until_with(
                deadline,
                || {
                    clock_reads.set(clock_reads.get() + 1);
                    clock.get()
                },
                || {
                    queries.set(queries.get() + 1);
                    if queries.get() == 1 {
                        Ok(NativeExitState::Pending)
                    } else {
                        if late {
                            clock.set(deadline);
                        }
                        Err(error)
                    }
                },
                |duration| clock.set(clock.get() + duration),
            );
            assert_eq!(result, Err(error));
            assert_eq!(queries.get(), 2);
            assert_eq!(clock_reads.get(), 4, "preflight after backoff and check after fresh error");
        }
    }
}

#[test]
fn late_exited_and_late_wake_never_authorize_on_time_success_or_another_query() {
    let start = Instant::now();
    let deadline = start + Duration::from_millis(1);
    let clock = Cell::new(start);
    let late_exit = wait_until_with(
        deadline,
        || clock.get(),
        || {
            clock.set(deadline);
            Ok(NativeExitState::Exited)
        },
        |_| panic!("late exit must not sleep"),
    );
    assert_eq!(late_exit, Ok(NativeExitState::Pending));
    clock.set(start);
    let queries = Cell::new(0);
    let late_wake = wait_until_with(
        deadline,
        || clock.get(),
        || {
            queries.set(queries.get() + 1);
            Ok(NativeExitState::Pending)
        },
        |_| clock.set(deadline + Duration::from_secs(1)),
    );
    assert_eq!(late_wake, Ok(NativeExitState::Pending));
    assert_eq!(queries.get(), 1);
}

#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
#[test]
fn real_native_tls_ack_cannot_authorize_join_and_wait_keeps_original_deadline() {
    use std::sync::mpsc;
    struct Epilogue {
        entered: mpsc::SyncSender<ThreadId>,
        release: mpsc::Receiver<()>,
        deadline: Instant,
    }
    impl Drop for Epilogue {
        fn drop(&mut self) {
            let _ = self.entered.send(thread::current().id());
            let _ = self.release.recv_timeout(self.deadline.saturating_duration_since(Instant::now()));
        }
    }
    thread_local! {
        static EPILOGUE: std::cell::RefCell<Option<Epilogue>> = const { std::cell::RefCell::new(None) };
    }
    let fixture_deadline = Instant::now() + Duration::from_secs(2);
    let (prepared, ready) = mpsc::sync_channel(1);
    let (ack, acknowledged) = mpsc::sync_channel(1);
    let (entered, observed) = mpsc::sync_channel(1);
    let (release, gate) = mpsc::sync_channel(1);
    let worker = thread::spawn(move || {
        let observation = NativeExitObservation::new();
        let registration = observation.register_current();
        let _ = prepared.send((observation, registration));
        EPILOGUE
            .with(|slot| *slot.borrow_mut() = Some(Epilogue { entered, release: gate, deadline: fixture_deadline }));
        let _ = ack.send(()); // body ACK, before native TLS destruction
        173_u32
    });
    let original = worker.thread().id();
    let mut preparation = ready.recv_timeout(fixture_deadline.saturating_duration_since(Instant::now())).ok();
    let ack = acknowledged.recv_timeout(fixture_deadline.saturating_duration_since(Instant::now()));
    let entered = observed.recv_timeout(fixture_deadline.saturating_duration_since(Instant::now()));
    let rust_finished = worker.is_finished();
    let deadline = (Instant::now() + Duration::from_millis(20)).min(fixture_deadline);
    let pending = preparation.as_ref().map(|(observation, _)| observation.wait_until(&worker, deadline));
    let returned = Instant::now();
    let retained = worker.thread().id();
    let expired_retry = preparation.as_ref().map(|(observation, _)| observation.wait_until(&worker, deadline));
    // Release, original join and explicit right retirement precede assertions.
    // No failed assertion can strand the TLS gate or its counted native right.
    let released = release.send(());
    let joined = worker.join();
    if preparation.is_none() {
        preparation = ready.try_recv().ok();
    }
    let registration = preparation.as_ref().map(|(_, result)| *result);
    let retired = preparation.as_ref().map(|(observation, _)| observation.retire_after_join());
    assert!(ack.is_ok());
    assert_eq!(entered.unwrap(), original);
    #[cfg(windows)]
    assert!(rust_finished, "Windows must reproduce the Rust/native TLS completion gap");
    #[cfg(not(windows))]
    let _ = rust_finished; // Unix std may mark Rust completion after TLS.
    assert_eq!(registration, Some(Ok(())));
    assert_eq!(pending, Some(Ok(NativeExitState::Pending)));
    assert!(returned >= deadline);
    assert!(returned < fixture_deadline, "wait must not renew to the fixture cleanup deadline");
    assert_eq!(expired_retry, Some(Ok(NativeExitState::Pending)));
    assert_eq!(retained, original);
    assert!(released.is_ok());
    assert_eq!(joined.unwrap(), 173);
    assert_eq!(retired, Some(Ok(())));
}
