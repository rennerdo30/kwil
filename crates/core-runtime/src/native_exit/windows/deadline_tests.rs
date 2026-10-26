use super::*;
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
};

#[derive(Debug, PartialEq, Eq)]
enum Event {
    Clock,
    Wait { handle: usize, milliseconds: u32 },
    LastError,
    Sleep(Duration),
}
struct Step {
    status: u32,
    elapsed: Duration,
}
struct Fake<'a> {
    clock: &'a Cell<Instant>,
    events: &'a RefCell<Vec<Event>>,
    steps: VecDeque<Step>,
    error: u32,
}
impl QueryApi for Fake<'_> {
    fn wait(&mut self, handle: RawHandle, timeout_ms: u32) -> u32 {
        self.events.borrow_mut().push(Event::Wait { handle: handle as usize, milliseconds: timeout_ms });
        assert_ne!(timeout_ms, u32::MAX, "INFINITE is never permitted");
        assert_ne!(timeout_ms, 0, "unexpired deadline must use a finite blocking wait");
        let step = self.steps.pop_front().expect("unexpected extra native wait");
        self.clock.set(self.clock.get() + step.elapsed);
        step.status
    }
    fn last_error(&mut self) -> u32 {
        self.events.borrow_mut().push(Event::LastError);
        let error = self.error;
        self.error = 0;
        error
    }
}
fn run(
    start: Instant,
    deadline: Instant,
    steps: impl IntoIterator<Item = Step>,
    error: u32,
) -> (QueryResult, Vec<Event>, Instant) {
    let clock = Cell::new(start);
    let events = RefCell::new(Vec::new());
    let mut api = Fake { clock: &clock, events: &events, steps: steps.into_iter().collect(), error };
    let result = wait_until_with(
        std::ptr::without_provenance_mut(73),
        deadline,
        &mut api,
        || {
            events.borrow_mut().push(Event::Clock);
            clock.get()
        },
        |duration| {
            assert!(!duration.is_zero());
            assert!(duration <= Duration::from_millis(1));
            assert!(duration <= deadline.saturating_duration_since(clock.get()));
            events.borrow_mut().push(Event::Sleep(duration));
            clock.set(clock.get() + duration);
        },
    );
    (result, events.into_inner(), clock.get())
}
fn waits(events: &[Event]) -> Vec<(usize, u32)> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Wait { handle, milliseconds } => Some((*handle, *milliseconds)),
            _ => None,
        })
        .collect()
}

#[test]
fn expired_windows_wait_has_no_native_or_backoff_effect() {
    let deadline = Instant::now();
    let (result, events, _) = run(deadline, deadline, [], 0);
    assert_eq!(result, Ok(NativeExitState::Pending));
    assert_eq!(events, [Event::Clock]);
}

#[test]
fn normal_remaining_time_rounds_up_and_uses_one_finite_wait() {
    let start = Instant::now();
    for (remaining, milliseconds) in [
        (Duration::from_nanos(1), 1),
        (Duration::from_millis(1), 1),
        (Duration::from_nanos(1_000_001), 2),
        (Duration::from_millis(173), 173),
    ] {
        let (result, events, returned) = run(
            start,
            start + remaining,
            [Step { status: WAIT_TIMEOUT, elapsed: Duration::from_millis(milliseconds.into()) }],
            0,
        );
        assert_eq!(result, Ok(NativeExitState::Pending));
        assert!(returned >= start + remaining);
        assert_eq!(waits(&events), [(73, milliseconds)]);
        assert!(!events.iter().any(|event| matches!(event, Event::Sleep(_) | Event::LastError)));
    }
    let (result, events, _) = run(
        start,
        start + Duration::from_millis(10),
        [Step { status: WAIT_OBJECT_0, elapsed: Duration::from_millis(3) }],
        0,
    );
    assert_eq!(result, Ok(NativeExitState::Exited));
    assert_eq!(waits(&events), [(73, 10)]);
}

#[test]
fn capped_long_wait_reuses_exact_handle_and_absolute_deadline_without_infinite() {
    let start = Instant::now();
    let cap = Duration::from_millis(u64::from(u32::MAX - 1));
    let deadline = start + cap + Duration::from_millis(10);
    let (result, events, returned) = run(
        start,
        deadline,
        [
            Step { status: WAIT_TIMEOUT, elapsed: cap },
            Step { status: WAIT_OBJECT_0, elapsed: Duration::from_millis(2) },
        ],
        0,
    );
    assert_eq!(result, Ok(NativeExitState::Exited));
    assert_eq!(waits(&events), [(73, u32::MAX - 1), (73, 9)]);
    assert_eq!(events.iter().filter(|event| matches!(event, Event::Sleep(_))).count(), 1);
    assert!(returned < deadline);
}

#[test]
fn actual_early_timeout_backs_off_then_on_time_exit_uses_remaining_time() {
    let start = Instant::now();
    let deadline = start + Duration::from_millis(10);
    let (result, events, _) = run(
        start,
        deadline,
        [
            Step { status: WAIT_TIMEOUT, elapsed: Duration::ZERO },
            Step { status: WAIT_OBJECT_0, elapsed: Duration::from_millis(2) },
        ],
        0,
    );
    assert_eq!(result, Ok(NativeExitState::Exited));
    assert_eq!(waits(&events), [(73, 10), (73, 9)]);
    assert!(events.contains(&Event::Sleep(Duration::from_millis(1))));
}

#[test]
fn early_timeout_backoff_to_expiry_prevents_another_wait_and_late_exit_is_pending() {
    let start = Instant::now();
    let deadline = start + Duration::from_micros(500);
    let (result, events, returned) = run(start, deadline, [Step { status: WAIT_TIMEOUT, elapsed: Duration::ZERO }], 0);
    assert_eq!(result, Ok(NativeExitState::Pending));
    assert_eq!(returned, deadline);
    assert_eq!(waits(&events), [(73, 1)]);
    assert!(events.contains(&Event::Sleep(Duration::from_micros(500))));
    let (result, events, _) =
        run(start, deadline, [Step { status: WAIT_OBJECT_0, elapsed: Duration::from_millis(1) }], 0);
    assert_eq!(result, Ok(NativeExitState::Pending));
    assert_eq!(waits(&events), [(73, 1)]);
}

#[test]
fn fatal_error_is_captured_before_clock_and_never_retried_even_if_late() {
    let start = Instant::now();
    let deadline = start + Duration::from_millis(1);
    for elapsed in [Duration::ZERO, Duration::from_millis(2)] {
        for code in [0, 0xDEAD_BEEF, u32::MAX] {
            let (result, events, _) = run(start, deadline, [Step { status: WAIT_FAILED, elapsed }], code);
            assert_eq!(result, Err(NativeExitQueryError::WindowsWaitFailed { code }));
            assert_eq!(
                events,
                [Event::Clock, Event::Wait { handle: 73, milliseconds: 1 }, Event::LastError, Event::Clock]
            );
        }
    }
    let (result, events, _) = run(start, deadline, [Step { status: 128, elapsed: Duration::from_millis(2) }], 73);
    assert_eq!(result, Err(NativeExitQueryError::UnexpectedWindowsWait { status: 128 }));
    assert_eq!(events, [Event::Clock, Event::Wait { handle: 73, milliseconds: 1 }, Event::Clock]);
}
