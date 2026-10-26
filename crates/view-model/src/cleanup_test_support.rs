//! Test-only cleanup policy. Production settlement remains one explicit attempt.
use std::{
    fmt,
    ops::ControlFlow,
    time::{Duration, Instant},
};

pub(crate) enum CleanupFailure<Owner, Value, Error> {
    Expired { owner: Owner, last_error: Option<Error> },
    Failed { owner: Owner, error: Error },
    CompletedLate { value: Value, last_error: Option<Error> },
}
impl<O, V, E: fmt::Debug> fmt::Debug for CleanupFailure<O, V, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Expired { last_error, .. } => {
                f.debug_struct("Expired").field("last_error", last_error).finish_non_exhaustive()
            }
            Self::Failed { error, .. } => f.debug_struct("Failed").field("error", error).finish_non_exhaustive(),
            Self::CompletedLate { last_error, .. } => {
                f.debug_struct("CompletedLate").field("last_error", last_error).finish_non_exhaustive()
            }
        }
    }
}

// ControlFlow keeps even large fixture owners inline without a large-Err lint,
// boxing, or a test feature. Existing cleanup callers keep familiar assertions.
#[must_use]
pub(crate) struct CleanupOutcome<O, V, E>(ControlFlow<CleanupFailure<O, V, E>, V>);
impl<O, V, E: fmt::Debug> fmt::Debug for CleanupOutcome<O, V, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            ControlFlow::Continue(_) => f.write_str("Complete(..)"),
            ControlFlow::Break(failure) => failure.fmt(f),
        }
    }
}
impl<O, V, E> CleanupOutcome<O, V, E> {
    pub(crate) fn into_flow(self) -> ControlFlow<CleanupFailure<O, V, E>, V> {
        self.0
    }
}
impl<O, V, E: fmt::Debug> CleanupOutcome<O, V, E> {
    pub(crate) fn unwrap(self) -> V {
        match self.0 {
            ControlFlow::Continue(value) => value,
            ControlFlow::Break(failure) => panic!("cleanup retained its actual owner or late value: {failure:?}"),
        }
    }
    pub(crate) fn unwrap_err(self) -> CleanupFailure<O, V, E> {
        match self.0 {
            ControlFlow::Break(failure) => failure,
            ControlFlow::Continue(_) => panic!("expected cleanup failure, received on-time completion"),
        }
    }
}

pub(crate) fn cleanup_until<O, V, E>(
    owner: O,
    deadline: Instant,
    attempt: impl FnMut(O, Instant) -> ControlFlow<(O, E), V>,
    is_native_pending: impl Fn(&E) -> bool,
) -> CleanupOutcome<O, V, E> {
    cleanup_with_clock(owner, deadline, attempt, is_native_pending, Instant::now, std::thread::park_timeout)
}

fn cleanup_with_clock<O, V, E>(
    mut owner: O,
    deadline: Instant,
    mut attempt: impl FnMut(O, Instant) -> ControlFlow<(O, E), V>,
    is_native_pending: impl Fn(&E) -> bool,
    mut now: impl FnMut() -> Instant,
    mut backoff: impl FnMut(Duration),
) -> CleanupOutcome<O, V, E> {
    let mut last_error = None;
    loop {
        if now() >= deadline {
            return CleanupOutcome(ControlFlow::Break(CleanupFailure::Expired { owner, last_error }));
        }
        match attempt(owner, deadline) {
            ControlFlow::Continue(value) => {
                return if now() < deadline {
                    CleanupOutcome(ControlFlow::Continue(value))
                } else {
                    CleanupOutcome(ControlFlow::Break(CleanupFailure::CompletedLate { value, last_error }))
                };
            }
            ControlFlow::Break((returned, error)) => {
                if !is_native_pending(&error) {
                    return CleanupOutcome(ControlFlow::Break(CleanupFailure::Failed { owner: returned, error }));
                }
                owner = returned;
                last_error = Some(error);
                let remaining = deadline.saturating_duration_since(now());
                if !remaining.is_zero() {
                    backoff(remaining.min(Duration::from_millis(1)));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[derive(Debug, PartialEq, Eq)]
    enum Error {
        Pending(u8),
        Fatal,
    }
    fn pending(error: &Error) -> bool {
        matches!(error, Error::Pending(_))
    }

    #[test]
    fn expired_input_never_attempts_and_retains_exact_owner() {
        let deadline = Instant::now();
        let owner = Box::new(7);
        let address = &*owner as *const i32;
        let calls = Cell::new(0);
        let result = cleanup_with_clock(
            owner,
            deadline,
            |owner, _| {
                calls.set(calls.get() + 1);
                ControlFlow::<(Box<i32>, Error), _>::Continue(owner)
            },
            pending,
            || deadline,
            |_| panic!("expired input cannot back off"),
        );
        let CleanupFailure::Expired { owner, last_error } = result.unwrap_err() else { panic!("expiry required") };
        assert_eq!(&*owner as *const i32, address);
        assert_eq!(last_error, None);
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn fresh_pending_then_on_time_success() {
        let start = Instant::now();
        let deadline = start + Duration::from_secs(2);
        let clock = Cell::new(start);
        let calls = Cell::new(0);
        let result = cleanup_with_clock(
            7,
            deadline,
            |owner, _| {
                calls.set(calls.get() + 1);
                if calls.get() == 1 {
                    ControlFlow::Break((owner, Error::Pending(1)))
                } else {
                    ControlFlow::Continue(owner)
                }
            },
            pending,
            || clock.get(),
            |delay| clock.set(clock.get() + delay),
        );
        assert_eq!(result.unwrap(), 7);
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn fresh_fatal_after_pending_is_not_retried_or_reclassified() {
        let start = Instant::now();
        let deadline = start + Duration::from_secs(2);
        let clock = Cell::new(start);
        let calls = Cell::new(0);
        let result = cleanup_with_clock(
            7,
            deadline,
            |owner, _| {
                calls.set(calls.get() + 1);
                if calls.get() == 1 {
                    ControlFlow::Break((owner, Error::Pending(1)))
                } else {
                    clock.set(deadline);
                    ControlFlow::<_, ()>::Break((owner, Error::Fatal))
                }
            },
            pending,
            || clock.get(),
            |delay| clock.set(clock.get() + delay),
        );
        let CleanupFailure::Failed { owner, error } = result.unwrap_err() else { panic!("actual fatal required") };
        assert_eq!(owner, 7);
        assert_eq!(error, Error::Fatal);
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn retries_keep_exact_owner_original_deadline_and_bounded_backoff() {
        let start = Instant::now();
        let deadline = start + Duration::from_secs(2);
        let clock = Cell::new(start);
        let owner = Box::new(7);
        let address = &*owner as *const i32;
        let calls = Cell::new(0);
        let result = cleanup_with_clock(
            owner,
            deadline,
            |owner, actual_deadline| {
                assert_eq!(actual_deadline, deadline);
                assert_eq!(&*owner as *const i32, address);
                calls.set(calls.get() + 1);
                if calls.get() < 3 {
                    ControlFlow::Break((owner, Error::Pending(calls.get())))
                } else {
                    ControlFlow::Continue(owner)
                }
            },
            pending,
            || clock.get(),
            |delay| {
                assert!(delay <= Duration::from_millis(1));
                assert!(delay <= deadline.saturating_duration_since(clock.get()));
                clock.set(clock.get() + delay);
            },
        );
        assert_eq!(&*result.unwrap() as *const i32, address);
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn pending_expiry_retains_last_actual_error_without_another_attempt() {
        let start = Instant::now();
        let deadline = start + Duration::from_millis(1);
        let clock = Cell::new(start);
        let calls = Cell::new(0);
        let result = cleanup_with_clock(
            7,
            deadline,
            |owner, _| {
                calls.set(calls.get() + 1);
                ControlFlow::<_, ()>::Break((owner, Error::Pending(23)))
            },
            pending,
            || clock.get(),
            |delay| clock.set(clock.get() + delay),
        );
        let CleanupFailure::Expired { owner, last_error } = result.unwrap_err() else { panic!("expiry required") };
        assert_eq!(owner, 7);
        assert_eq!(last_error, Some(Error::Pending(23)));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn late_success_retains_actual_value_and_last_pending_without_success() {
        let start = Instant::now();
        let deadline = start + Duration::from_secs(2);
        let clock = Cell::new(start);
        let calls = Cell::new(0);
        let value = Box::new(37);
        let address = &*value as *const i32;
        let mut value = Some(value);
        let result = cleanup_with_clock(
            7,
            deadline,
            |owner, _| {
                calls.set(calls.get() + 1);
                if calls.get() == 1 {
                    ControlFlow::Break((owner, Error::Pending(1)))
                } else {
                    clock.set(deadline);
                    ControlFlow::Continue(value.take().unwrap())
                }
            },
            pending,
            || clock.get(),
            |delay| clock.set(clock.get() + delay),
        );
        let CleanupFailure::CompletedLate { value, last_error } = result.unwrap_err() else {
            panic!("late value required")
        };
        assert_eq!(&*value as *const i32, address);
        assert_eq!(*value, 37);
        assert_eq!(last_error, Some(Error::Pending(1)));
        assert_eq!(calls.get(), 2);
    }
}
