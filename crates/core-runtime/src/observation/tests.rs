use super::*;
use std::cell::Cell;

fn mock_registration(observation: &NativeExitObservation) -> Identity {
    let identity = Identity::current();
    observation.register_with(identity, || Attempt { witness: Witness::Mock(73), error: None }).unwrap();
    identity
}

#[test]
fn inline_owner_is_send_sync_errors_are_copy_and_sizes_are_const_evaluable() {
    fn owner<T: Send + Sync>() {}
    fn scalar<T: Copy + Eq + fmt::Debug + std::error::Error + Send + Sync>() {}
    owner::<NativeExitObservation>();
    scalar::<NativeObservationError>();
    const RESIDENT: usize = NativeExitObservation::resident_bytes();
    const OPERATIONS: Option<usize> = NativeExitObservation::operation_stack_bytes();
    let _ = (RESIDENT, OPERATIONS);
}

#[test]
fn registration_is_one_shot_and_foreign_identities_cannot_query_or_retire() {
    let other = thread::spawn(|| thread::current().id()).join().unwrap();
    let observation = NativeExitObservation::new();
    let identity = mock_registration(&observation);
    let calls = Cell::new(0);
    assert_eq!(
        observation.register_with(identity, || {
            calls.set(calls.get() + 1);
            unreachable!()
        }),
        Err(NativeObservationError::AlreadyRegistered)
    );
    assert_eq!(
        observation.query_with(other, identity.process, |_| {
            calls.set(calls.get() + 1);
            unreachable!()
        }),
        Err(NativeObservationError::ForeignThread)
    );
    let foreign_pid = identity.process.wrapping_add(1);
    assert_eq!(
        observation.query_with(identity.thread, foreign_pid, |_| {
            calls.set(calls.get() + 1);
            unreachable!()
        }),
        Err(NativeObservationError::ForeignProcess)
    );
    assert_eq!(
        observation.retire_with(foreign_pid, |_| {
            calls.set(calls.get() + 1);
            unreachable!()
        }),
        Err(NativeObservationError::ForeignProcess)
    );
    assert_eq!(calls.get(), 0);
    assert_eq!(
        observation.query_with(identity.thread, identity.process, |witness| {
            assert!(matches!(witness, Witness::Mock(73)));
            Ok(NativeExitState::Pending)
        }),
        Ok(NativeExitState::Pending)
    );
}

#[test]
fn missing_registering_and_busy_claims_never_invoke_native_operations() {
    let identity = Identity::current();
    let missing = NativeExitObservation::new();
    assert_eq!(
        missing.query_with(identity.thread, identity.process, |_| unreachable!()),
        Err(NativeObservationError::Unregistered)
    );
    assert_eq!(missing.retire_with(identity.process, |_| unreachable!()), Err(NativeObservationError::Unregistered));
    missing.state.store(REGISTERING, Ordering::Release); // mock an in-flight registration, never reset it
    assert_eq!(
        missing.query_with(identity.thread, identity.process, |_| unreachable!()),
        Err(NativeObservationError::Registering)
    );
    assert_eq!(missing.register_with(identity, || unreachable!()), Err(NativeObservationError::Registering));
    for held in [QUERYING, RETIRING] {
        let observation = NativeExitObservation::new();
        let identity = mock_registration(&observation);
        observation.state.compare_exchange(REGISTERED, held, Ordering::AcqRel, Ordering::Acquire).unwrap();
        let _held = Claim { state: &observation.state, restore: REGISTERED };
        assert_eq!(
            observation.query_with(identity.thread, identity.process, |_| unreachable!()),
            Err(NativeObservationError::Busy)
        );
        assert_eq!(observation.retire_with(identity.process, |_| unreachable!()), Err(NativeObservationError::Busy));
    }
}

#[test]
fn query_claim_excludes_retirement_and_restores_state_even_on_unwind() {
    let observation = NativeExitObservation::new();
    let identity = mock_registration(&observation);
    assert_eq!(
        observation.query_with(identity.thread, identity.process, |_| {
            assert_eq!(
                observation.retire_with(identity.process, |_| unreachable!()),
                Err(NativeObservationError::Busy)
            );
            Ok(NativeExitState::Pending)
        }),
        Ok(NativeExitState::Pending)
    );
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = observation.query_with(identity.thread, identity.process, |_| panic!("mock query unwind"));
    }));
    assert!(panicked.is_err());
    assert_eq!(
        observation.query_with(identity.thread, identity.process, |_| Ok(NativeExitState::Pending)),
        Ok(NativeExitState::Pending)
    );
    assert_eq!(observation.retire_with(identity.process, |_| Ok(())), Ok(()));
    assert_eq!(observation.retire_with(identity.process, |_| unreachable!()), Ok(()));
    assert_eq!(
        observation.query_with(identity.thread, identity.process, |_| unreachable!()),
        Err(NativeObservationError::Retired)
    );
    assert_eq!(observation.register_with(identity, || unreachable!()), Err(NativeObservationError::AlreadyRegistered));
}

#[derive(Debug, PartialEq, Eq)]
enum LinuxCall {
    Register,
    Sample(i32),
}
struct Linux {
    clock: Result<i32, i32>,
    sample: Result<(), i32>,
    calls: Vec<LinuxCall>,
}
impl linux::Api for Linux {
    fn current_thread_clock(&mut self) -> Result<i32, i32> {
        self.calls.push(LinuxCall::Register);
        self.clock
    }
    fn sample(&mut self, clock: i32) -> Result<(), i32> {
        self.calls.push(LinuxCall::Sample(clock));
        self.sample
    }
}

#[test]
fn linux_registration_and_validation_errors_stay_distinct_and_sticky() {
    for code in [3, 22, i32::MAX] {
        let observation = NativeExitObservation::new();
        let identity = Identity::current();
        let mut api = Linux { clock: Err(code), sample: Ok(()), calls: Vec::new() };
        assert_eq!(
            observation.register_with(identity, || linux::register_with(&mut api)),
            Err(NativeObservationError::LinuxRegister { code })
        );
        assert_eq!(api.calls, [LinuxCall::Register]);
        assert_eq!(
            observation.query_with(identity.thread, identity.process, |_| unreachable!()),
            Err(NativeObservationError::LinuxRegister { code })
        );
        assert_eq!(
            observation.register_with(identity, || unreachable!()),
            Err(NativeObservationError::AlreadyRegistered)
        );
    }
    for errno in [22, 5, i32::MIN] {
        let observation = NativeExitObservation::new();
        let identity = Identity::current();
        let mut api = Linux { clock: Ok(-731), sample: Err(errno), calls: Vec::new() };
        assert_eq!(
            observation.register_with(identity, || linux::register_with(&mut api)),
            Err(NativeObservationError::LinuxValidate { errno })
        );
        assert_eq!(api.calls, [LinuxCall::Register, LinuxCall::Sample(-731)]);
        assert_eq!(
            observation.query_with(identity.thread, identity.process, |_| unreachable!()),
            Err(NativeObservationError::LinuxValidate { errno })
        );
        assert_eq!(observation.retire_with(identity.process, |_| Ok(())), Ok(()));
    }
}

#[test]
fn linux_queries_once_using_exact_registered_clock_and_only_validated_einval_is_exit() {
    let observation = NativeExitObservation::new();
    let identity = Identity::current();
    let mut api = Linux { clock: Ok(-18743), sample: Ok(()), calls: Vec::new() };
    observation.register_with(identity, || linux::register_with(&mut api)).unwrap();
    for (sample, expected) in [
        (Ok(()), Ok(NativeExitState::Pending)),
        (Err(22), Ok(NativeExitState::Exited)),
        (Err(5), Err(NativeObservationError::LinuxQuery { errno: 5 })),
        (Err(i32::MIN), Err(NativeObservationError::LinuxQuery { errno: i32::MIN })),
        (Err(i32::MAX), Err(NativeObservationError::LinuxQuery { errno: i32::MAX })),
    ] {
        api.sample = sample;
        let before = api.calls.len();
        let result = observation.query_with(identity.thread, identity.process, |witness| match witness {
            Witness::Linux(witness) => linux::query_with(witness, &mut api),
            _ => unreachable!(),
        });
        assert_eq!(result, expected);
        assert_eq!(&api.calls[before..], &[LinuxCall::Sample(-18743)]);
    }
    observation.retire_with(identity.process, |_| Ok(())).unwrap();
}

#[derive(Debug, PartialEq, Eq)]
enum MachCall {
    Acquire,
    Type(u32),
    Close(u32),
}
struct Mach {
    name: u32,
    kind: Result<u32, i32>,
    close: Result<(), i32>,
    calls: Vec<MachCall>,
}
impl macos::Api for Mach {
    fn thread_self(&mut self) -> u32 {
        self.calls.push(MachCall::Acquire);
        self.name
    }
    fn port_type(&mut self, name: u32) -> Result<u32, i32> {
        self.calls.push(MachCall::Type(name));
        self.kind
    }
    fn deallocate(&mut self, name: u32) -> Result<(), i32> {
        self.calls.push(MachCall::Close(name));
        self.close
    }
}
fn retire_mach(
    observation: &NativeExitObservation,
    process: u32,
    api: &mut Mach,
) -> Result<(), NativeObservationError> {
    observation.retire_with(process, |witness| match witness {
        Witness::Mach(witness) => macos::retire_with(witness, api),
        Witness::Missing => Ok(()),
        _ => unreachable!(),
    })
}

#[test]
fn mach_partial_validation_failure_keeps_exact_right_and_first_close_error_until_one_success() {
    for code in [15, 5, i32::MIN, i32::MAX] {
        let observation = NativeExitObservation::new();
        let identity = Identity::current();
        let mut api = Mach { name: 0x731, kind: Err(code), close: Err(19), calls: Vec::new() };
        assert_eq!(
            observation.register_with(identity, || macos::register_with(&mut api)),
            Err(NativeObservationError::MachValidate { code })
        );
        assert_eq!(api.calls, [MachCall::Acquire, MachCall::Type(0x731)]);
        assert_eq!(
            observation.query_with(identity.thread, identity.process, |_| unreachable!()),
            Err(NativeObservationError::MachValidate { code })
        );
        assert_eq!(
            retire_mach(&observation, identity.process, &mut api),
            Err(NativeObservationError::MachRetire { code: 19 })
        );
        api.close = Err(23);
        assert_eq!(
            retire_mach(&observation, identity.process, &mut api),
            Err(NativeObservationError::MachRetire { code: 19 })
        );
        assert_eq!(observation.first_retirement_error(), Some(NativeObservationError::MachRetire { code: 19 }));
        api.close = Ok(());
        assert_eq!(retire_mach(&observation, identity.process, &mut api), Ok(()));
        assert_eq!(retire_mach(&observation, identity.process, &mut api), Ok(()));
        assert_eq!(
            api.calls,
            [
                MachCall::Acquire,
                MachCall::Type(0x731),
                MachCall::Close(0x731),
                MachCall::Close(0x731),
                MachCall::Close(0x731)
            ]
        );
        assert_eq!(observation.first_retirement_error(), Some(NativeObservationError::MachRetire { code: 19 }));
        let before_drop = api.calls.len();
        drop(observation);
        assert_eq!(api.calls.len(), before_drop, "retired Drop must not issue native I/O");
    }
}

#[test]
fn mach_invalid_acquisition_or_type_never_becomes_exited_and_partial_right_is_explicitly_retired() {
    for name in [0, u32::MAX] {
        let observation = NativeExitObservation::new();
        let identity = Identity::current();
        let mut api = Mach { name, kind: Ok(1 << 16), close: Ok(()), calls: Vec::new() };
        assert_eq!(
            observation.register_with(identity, || macos::register_with(&mut api)),
            Err(NativeObservationError::MachAcquire { name })
        );
        assert_eq!(api.calls, [MachCall::Acquire]);
        assert_eq!(
            observation.query_with(identity.thread, identity.process, |_| unreachable!()),
            Err(NativeObservationError::MachAcquire { name })
        );
        retire_mach(&observation, identity.process, &mut api).unwrap();
        assert_eq!(api.calls, [MachCall::Acquire], "invalid reserved names are not counted rights");
    }
    for bits in [0, 1 << 20, (1 << 16) | (1 << 20)] {
        let observation = NativeExitObservation::new();
        let identity = Identity::current();
        let mut api = Mach { name: 81, kind: Ok(bits), close: Ok(()), calls: Vec::new() };
        assert_eq!(
            observation.register_with(identity, || macos::register_with(&mut api)),
            Err(NativeObservationError::MachValidateType { bits })
        );
        assert_eq!(
            observation.query_with(identity.thread, identity.process, |_| unreachable!()),
            Err(NativeObservationError::MachValidateType { bits })
        );
        retire_mach(&observation, identity.process, &mut api).unwrap();
        assert_eq!(api.calls.last(), Some(&MachCall::Close(81)));
    }
}

#[test]
fn mach_send_dead_name_and_original_query_codes_are_one_query_each() {
    let observation = NativeExitObservation::new();
    let identity = Identity::current();
    let mut api = Mach { name: 91, kind: Ok(1 << 16), close: Ok(()), calls: Vec::new() };
    observation.register_with(identity, || macos::register_with(&mut api)).unwrap();
    let cases = [
        (Ok(1 << 16), Ok(NativeExitState::Pending)),
        (Ok(1 << 20), Ok(NativeExitState::Exited)),
        (Err(15), Err(NativeObservationError::MachQuery { code: 15 })),
        (Err(i32::MIN), Err(NativeObservationError::MachQuery { code: i32::MIN })),
        (Ok(0), Err(NativeObservationError::MachQueryType { bits: 0 })),
    ];
    for (kind, expected) in cases {
        api.kind = kind;
        let before = api.calls.len();
        assert_eq!(
            observation.query_with(identity.thread, identity.process, |witness| match witness {
                Witness::Mach(witness) => macos::query_with(witness, &mut api),
                _ => unreachable!(),
            }),
            expected
        );
        assert_eq!(&api.calls[before..], &[MachCall::Type(91)]);
    }
    retire_mach(&observation, identity.process, &mut api).unwrap();
}

#[test]
fn dropping_unretired_partial_registration_does_no_native_io_or_implicit_close() {
    let observation = NativeExitObservation::new();
    let mut api = Mach { name: 121, kind: Err(15), close: Ok(()), calls: Vec::new() };
    let _ = observation.register_with(Identity::current(), || macos::register_with(&mut api));
    drop(observation); // mock-only misuse quarantine, not a successful retirement
    assert_eq!(api.calls, [MachCall::Acquire, MachCall::Type(121)]);
}

#[test]
fn nested_windows_query_error_remains_typed_without_boxing() {
    let observation = NativeExitObservation::new();
    let identity = mock_registration(&observation);
    let original = NativeExitQueryError::WindowsWaitFailed { code: u32::MAX };
    let result = observation
        .query_with(identity.thread, identity.process, |_| Err(NativeObservationError::WindowsQuery(original)));
    assert_eq!(result, Err(NativeObservationError::WindowsQuery(original)));
    assert!(std::error::Error::source(&result.unwrap_err()).is_some());
}

#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
mod actual_native {
    use super::*;
    use std::{
        sync::mpsc,
        time::{Duration, Instant},
    };

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
    fn backoff(deadline: Instant) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if !remaining.is_zero() {
            thread::park_timeout(remaining.min(Duration::from_millis(1)));
        }
    }

    #[test]
    fn original_worker_registration_observes_native_tls_then_joins_and_explicitly_retires() {
        let deadline = Instant::now() + Duration::from_secs(2);
        let (prepared, ready) = mpsc::sync_channel(1);
        let (entered, observed) = mpsc::sync_channel(1);
        let (release, gate) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let observation = NativeExitObservation::new(); // inline, no new Arc or shared allocation
            let registration = observation.register_current();
            let _ = prepared.send((observation, registration)); // move the exact record once to observer
            EPILOGUE.with(|slot| *slot.borrow_mut() = Some(Epilogue { entered, release: gate, deadline }));
            137_u32
        });
        let original = worker.thread().id();
        let mut preparation = ready.recv_timeout(deadline.saturating_duration_since(Instant::now())).ok();
        let entered = observed.recv_timeout(deadline.saturating_duration_since(Instant::now()));
        let rust_finished = worker.is_finished(); // recorded, NEVER assumed true on Unix
        let pending = preparation.as_ref().map(|(observation, _)| observation.query(&worker));
        let retained = worker.thread().id();
        let released = release.send(());
        let mut exited = None;
        while Instant::now() < deadline {
            let Some((observation, _)) = &preparation else { break };
            let state = observation.query(&worker);
            if state != Ok(NativeExitState::Pending) {
                exited = Some(state);
                break;
            }
            backoff(deadline);
        }
        // Cleanup release + ORIGINAL join precede ALL assertions. A late receive
        // still recovers any partial native right before explicit retirement.
        let joined = worker.join();
        if preparation.is_none() {
            preparation = ready.try_recv().ok();
        }
        let registration = preparation.as_ref().map(|(_, result)| *result);
        let retired = preparation.as_ref().map(|(observation, _)| observation.retire_after_join());
        let repeated = preparation.as_ref().map(|(observation, _)| observation.retire_after_join());
        eprintln!(
            "native observation TLS: original={original:?} rust_finished={rust_finished} pending={pending:?} exited={exited:?} retired={retired:?}"
        );
        assert_eq!(entered.unwrap(), original);
        assert_eq!(retained, original);
        assert_eq!(registration, Some(Ok(())));
        assert_eq!(pending, Some(Ok(NativeExitState::Pending)));
        assert!(released.is_ok());
        assert_eq!(exited, Some(Ok(NativeExitState::Exited)));
        assert_eq!(joined.unwrap(), 137);
        assert_eq!(retired, Some(Ok(())));
        assert_eq!(repeated, Some(Ok(())));
    }
}
