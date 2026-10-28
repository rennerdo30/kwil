//! Exclusively owned directories and OFF-UI cleanup for TUI fixtures.

use super::{DocumentLane, ShutdownTicket};
use kwil_view_model::document_worker::DocumentResultSettlementReason;
use kwil_view_model::search_session::{SearchWorker, WorkerSettlementReason};

use std::{
    io,
    ops::ControlFlow,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

/// Only tests that intentionally use the production process slot share this
/// guard. Ordinary fixture cleanup never enters that slot or takes this lock.
pub(super) fn process_shutdown_guard() -> std::sync::MutexGuard<'static, ()> {
    static GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());
    GUARD.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Fresh evidence from the exact failing return, never a retained older reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CleanupAttemptFailure {
    Document(DocumentResultSettlementReason),
    Search(WorkerSettlementReason),
}
impl CleanupAttemptFailure {
    fn native_exit_pending(self) -> bool {
        matches!(
            self,
            Self::Document(
                DocumentResultSettlementReason::ThreadExitPending
                    | DocumentResultSettlementReason::WrapThreadExitPending
            ) | Self::Search(WorkerSettlementReason::ThreadExitPending)
        )
    }
}

/// An unboxed move of the original ticket. Diagnostics never replace its
/// original guarded errors, native rights, receipt tokens or source owners.
pub(super) struct TestCleanupFailure {
    pub ticket: ShutdownTicket,
    pub diagnostic: String,
}
impl std::fmt::Debug for TestCleanupFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestCleanupFailure")
            .field("diagnostic", &self.diagnostic)
            .field("attempt", &self.ticket.document.as_ref().and_then(|document| document.cleanup_attempt))
            .finish_non_exhaustive()
    }
}

/// Call inside the fixture's EXISTING cleanup closure, with its deadline
/// captured before dispatch. No global slot, additional thread or UI retry.
pub(super) fn finish_bounded_ticket_off_ui(
    ticket: ShutdownTicket,
    deadline: Instant,
) -> ControlFlow<TestCleanupFailure, ShutdownTicket> {
    finish_bounded_ticket_with(ticket, deadline, Instant::now, std::thread::park_timeout, |ticket, deadline, retry| {
        let result = super::guard_shutdown(ticket, |ticket| {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "bounded actual frontend cleanup deadline");
            super::finish_bounded_shutdown(ticket, remaining, retry)
        });
        if result.is_err() {
            let document = ticket.document.as_ref().unwrap();
            // Retain the original bounded-wrap fixture's ownership oracles,
            // now shared by every actual bounded frontend cleanup.
            match document.cleanup_attempt {
                Some(CleanupAttemptFailure::Document(DocumentResultSettlementReason::ThreadExitPending)) => assert!(
                    document.shutdown.is_some(),
                    "actual thread exit retains Workspace/history inside the sealed bundle"
                ),
                Some(CleanupAttemptFailure::Document(DocumentResultSettlementReason::WrapThreadExitPending)) => {
                    assert!(
                        (document.transport.is_some() && ticket.view.is_some()) || document.shutdown.is_some(),
                        "wrap exit retains its original live or sealed Workspace owner"
                    )
                }
                _ => {}
            }
        }
        result
    })
}

fn finish_bounded_ticket_with(
    mut ticket: ShutdownTicket,
    deadline: Instant,
    mut now: impl FnMut() -> Instant,
    mut backoff: impl FnMut(Duration),
    mut attempt: impl FnMut(&mut ShutdownTicket, Instant, bool) -> Result<(), String>,
) -> ControlFlow<TestCleanupFailure, ShutdownTicket> {
    let document = ticket.document.as_mut().expect("fixture retains its bounded document owner");
    let deadline = document.deadline.map_or(deadline, |stored| stored.min(deadline));
    document.deadline = Some(deadline);
    let mut retry = false;
    let mut last_pending = None;
    loop {
        // Reset before even the deadline check. A later failure cannot inherit
        // authority from the previous attempt's ThreadExitPending.
        ticket.document.as_mut().unwrap().cleanup_attempt = None;
        if now() >= deadline {
            return ControlFlow::Break(TestCleanupFailure {
                ticket,
                diagnostic: last_pending.unwrap_or_else(|| {
                    "TUI fixture original absolute cleanup deadline reached; exact ticket retained".into()
                }),
            });
        }
        match attempt(&mut ticket, deadline, retry) {
            Ok(()) => return ControlFlow::Continue(ticket),
            Err(diagnostic) => {
                let pending = ticket.document.as_ref().and_then(|document| document.cleanup_attempt);
                let remaining = deadline.saturating_duration_since(now());
                if ticket.terminal_failure.is_some()
                    || ticket.terminal_panic.is_some()
                    || !pending.is_some_and(CleanupAttemptFailure::native_exit_pending)
                    || remaining.is_zero()
                {
                    return ControlFlow::Break(TestCleanupFailure { ticket, diagnostic });
                }
                retry = true;
                last_pending = Some(diagnostic);
                backoff(Duration::from_millis(1).min(remaining));
            }
        }
    }
}

pub(super) fn finish_search_worker_off_ui(
    mut worker: SearchWorker,
    deadline: Instant,
) -> ControlFlow<impl std::error::Error + Send + Sync + 'static, ()> {
    loop {
        match worker.shutdown_until(deadline) {
            Ok(()) => return ControlFlow::Continue(()),
            Err(error) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if error.reason() != WorkerSettlementReason::ThreadExitPending || remaining.is_zero() {
                    return ControlFlow::Break(error);
                }
                worker = error.into_worker();
                std::thread::park_timeout(Duration::from_millis(1).min(remaining));
            }
        }
    }
}

/// Fatal fixtures panic while their original ticket is still owned OFF UI.
pub(super) fn expect_cleanup(result: ControlFlow<TestCleanupFailure, ShutdownTicket>) -> ShutdownTicket {
    match result {
        ControlFlow::Continue(ticket) => ticket,
        ControlFlow::Break(failure) => panic!("{failure:?}"),
    }
}

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

pub(crate) fn directory(prefix: &str) -> io::Result<PathBuf> {
    directory_in(&std::env::temp_dir(), prefix, &NEXT_DIRECTORY)
}

fn directory_in(parent: &Path, prefix: &str, next: &AtomicU64) -> io::Result<PathBuf> {
    // Sequence numbers avoid clock collisions; exclusive creation also rejects
    // occupied roots left by a previous process with the same PID.
    for _ in 0..128 {
        let sequence = next.fetch_add(1, Ordering::Relaxed);
        let root = parent.join(format!("{prefix}-{}-{sequence}", std::process::id()));
        match std::fs::create_dir(&root) {
            Ok(()) => return Ok(root),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "TUI fixture directory namespace exhausted"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn cleanup_ticket() -> (ShutdownTicket, *const kwil_view_model::Editor) {
        let view = super::super::Workspace::new(super::super::View::new(None)).unwrap();
        let identity = view.editor().unwrap() as *const kwil_view_model::Editor;
        let budget = super::super::AllocationBudget::default();
        let document = DocumentLane::new(&budget).unwrap();
        let (mut ticket, _) = super::super::document_ui::StartupOwner::new(view, budget).into_parts();
        ticket.document = Some(document);
        (ticket, identity)
    }

    fn assert_owner(ticket: &ShutdownTicket, identity: *const kwil_view_model::Editor, deadline: Instant) {
        assert_eq!(ticket.view.as_ref().unwrap().editor().unwrap() as *const kwil_view_model::Editor, identity);
        assert_eq!(ticket.document.as_ref().unwrap().deadline, Some(deadline));
    }

    #[test]
    fn cleanup_pending_then_success_keeps_exact_ticket_and_deadline() {
        for pending in [
            CleanupAttemptFailure::Document(DocumentResultSettlementReason::ThreadExitPending),
            CleanupAttemptFailure::Document(DocumentResultSettlementReason::WrapThreadExitPending),
            CleanupAttemptFailure::Search(WorkerSettlementReason::ThreadExitPending),
        ] {
            let (mut ticket, identity) = cleanup_ticket();
            let start = Instant::now();
            let deadline = start + Duration::from_millis(3);
            ticket.document.as_mut().unwrap().deadline = Some(deadline);
            let clock = Cell::new(start);
            let attempts = Cell::new(0);
            let result = finish_bounded_ticket_with(
                ticket,
                start + Duration::from_secs(1),
                || clock.get(),
                |duration| {
                    assert_eq!(duration, Duration::from_millis(1));
                    clock.set(clock.get() + duration);
                },
                |ticket, original, retry| {
                    assert_eq!(original, deadline, "a later caller cannot refresh the stored deadline");
                    assert_owner(ticket, identity, deadline);
                    assert!(ticket.document.as_ref().unwrap().cleanup_attempt.is_none());
                    let count = attempts.replace(attempts.get() + 1);
                    assert_eq!(retry, count != 0);
                    if count == 0 {
                        ticket.document.as_mut().unwrap().cleanup_attempt = Some(pending);
                        Err("first exact native pending".into())
                    } else {
                        Ok(())
                    }
                },
            );
            let ControlFlow::Continue(ticket) = result else { panic!("typed native pending must be retried") };
            assert_owner(&ticket, identity, deadline);
            assert_eq!(attempts.get(), 2);
            assert_eq!(clock.get(), start + Duration::from_millis(1));
        }
    }

    #[test]
    fn cleanup_pending_then_fatal_does_not_reuse_previous_reason() {
        let (ticket, identity) = cleanup_ticket();
        let start = Instant::now();
        let deadline = start + Duration::from_secs(1);
        let clock = Cell::new(start);
        let attempts = Cell::new(0);
        let fatal = String::from("exact later untyped refusal; first errors remain in the ticket");
        let diagnostic_identity = fatal.as_ptr();
        let mut fatal = Some(fatal);
        let result = finish_bounded_ticket_with(
            ticket,
            deadline,
            || clock.get(),
            |duration| clock.set(clock.get() + duration),
            |ticket, original, retry| {
                assert_owner(ticket, identity, original);
                let document = ticket.document.as_mut().unwrap();
                assert!(document.cleanup_attempt.is_none(), "every attempt needs fresh evidence");
                let count = attempts.replace(attempts.get() + 1);
                if count == 0 {
                    assert!(!retry);
                    document.settlement_reason = Some(DocumentResultSettlementReason::ThreadExitPending);
                    document.cleanup_attempt =
                        Some(CleanupAttemptFailure::Document(DocumentResultSettlementReason::ThreadExitPending));
                    Err("original pending".into())
                } else {
                    assert!(retry);
                    assert_eq!(count, 1, "fatal must not authorize a third attempt");
                    assert_eq!(document.settlement_reason, Some(DocumentResultSettlementReason::ThreadExitPending));
                    Err(fatal.take().unwrap())
                }
            },
        );
        let ControlFlow::Break(failure) = result else { panic!("later refusal must remain fatal") };
        assert_owner(&failure.ticket, identity, deadline);
        assert_eq!(attempts.get(), 2);
        assert_eq!(failure.diagnostic.as_ptr(), diagnostic_identity, "return the original diagnostic allocation");
        assert!(failure.ticket.document.as_ref().unwrap().cleanup_attempt.is_none());
        assert_eq!(
            failure.ticket.document.as_ref().unwrap().settlement_reason,
            Some(DocumentResultSettlementReason::ThreadExitPending),
            "retained original evidence is preserved but cannot authorize retry"
        );
    }

    #[test]
    fn cleanup_other_typed_failures_are_fatal_without_backoff() {
        for reason in [
            CleanupAttemptFailure::Document(DocumentResultSettlementReason::ConsumerRefused),
            CleanupAttemptFailure::Document(DocumentResultSettlementReason::TimedOut),
            CleanupAttemptFailure::Document(DocumentResultSettlementReason::WrapSettlementFailed),
            CleanupAttemptFailure::Document(DocumentResultSettlementReason::WorkerPanicked),
            CleanupAttemptFailure::Document(DocumentResultSettlementReason::Busy),
            CleanupAttemptFailure::Search(WorkerSettlementReason::TimedOut),
            CleanupAttemptFailure::Search(WorkerSettlementReason::WorkerPanicked),
        ] {
            let (ticket, identity) = cleanup_ticket();
            let start = Instant::now();
            let deadline = start + Duration::from_secs(1);
            let attempts = Cell::new(0);
            let result = finish_bounded_ticket_with(
                ticket,
                deadline,
                || start,
                |_| panic!("a non-Pending refusal cannot back off"),
                |ticket, _, retry| {
                    assert!(!retry);
                    attempts.set(attempts.get() + 1);
                    ticket.document.as_mut().unwrap().cleanup_attempt = Some(reason);
                    Err("exact typed fatal diagnostic".into())
                },
            );
            let ControlFlow::Break(failure) = result else { panic!("typed refusal cannot become success") };
            assert_owner(&failure.ticket, identity, deadline);
            assert_eq!(failure.ticket.document.as_ref().unwrap().cleanup_attempt, Some(reason));
            assert_eq!(failure.diagnostic, "exact typed fatal diagnostic");
            assert_eq!(attempts.get(), 1);
        }
    }

    #[test]
    fn cleanup_expired_deadline_never_dispatches_or_refreshes() {
        let (mut ticket, identity) = cleanup_ticket();
        let deadline = Instant::now();
        ticket.document.as_mut().unwrap().deadline = Some(deadline);
        let result = finish_bounded_ticket_with(
            ticket,
            deadline + Duration::from_secs(1),
            || deadline,
            |_| panic!("expired deadline cannot back off"),
            |_, _, _| panic!("expired original deadline cannot dispatch cleanup"),
        );
        let ControlFlow::Break(failure) = result else { panic!("expired cleanup cannot succeed") };
        assert_owner(&failure.ticket, identity, deadline);
    }

    #[test]
    fn cleanup_backoff_stops_at_original_deadline_and_keeps_pending_diagnostic() {
        let (ticket, identity) = cleanup_ticket();
        let start = Instant::now();
        let deadline = start + Duration::from_micros(500);
        let clock = Cell::new(start);
        let attempts = Cell::new(0);
        let pending = String::from("exact pending at the original deadline");
        let diagnostic_identity = pending.as_ptr();
        let mut pending = Some(pending);
        let result = finish_bounded_ticket_with(
            ticket,
            deadline,
            || clock.get(),
            |duration| {
                assert_eq!(duration, Duration::from_micros(500), "backoff is bounded by remaining time");
                clock.set(clock.get() + duration);
            },
            |ticket, _, retry| {
                assert!(!retry, "deadline must stop the next attempt");
                attempts.set(attempts.get() + 1);
                ticket.document.as_mut().unwrap().cleanup_attempt =
                    Some(CleanupAttemptFailure::Document(DocumentResultSettlementReason::ThreadExitPending));
                Err(pending.take().unwrap())
            },
        );
        let ControlFlow::Break(failure) = result else { panic!("deadline must retain the failed owner") };
        assert_owner(&failure.ticket, identity, deadline);
        assert_eq!(clock.get(), deadline);
        assert_eq!(attempts.get(), 1);
        assert_eq!(failure.diagnostic.as_ptr(), diagnostic_identity);
    }

    #[test]
    fn cleanup_independent_local_tickets_never_enter_process_slot() {
        let _process_guard = process_shutdown_guard();
        super::super::ensure_shutdown_idle().unwrap();
        let (first, first_identity) = cleanup_ticket();
        let (second, second_identity) = cleanup_ticket();
        assert_ne!(first_identity, second_identity);
        let start = Instant::now();
        let deadline = start + Duration::from_secs(1);
        let clock = Cell::new(start);
        let mut second = Some(second);
        let mut finished_second = None;
        let attempts = Cell::new(0);
        let first = finish_bounded_ticket_with(
            first,
            deadline,
            || clock.get(),
            |duration| clock.set(clock.get() + duration),
            |ticket, original, retry| {
                super::super::ensure_shutdown_idle().unwrap();
                assert_owner(ticket, first_identity, original);
                if attempts.replace(attempts.get() + 1) == 0 {
                    assert!(!retry);
                    // Deterministically interleave a second complete cleanup
                    // while the first exact ticket is still actively borrowed.
                    let inner = finish_bounded_ticket_with(
                        second.take().unwrap(),
                        deadline,
                        || start,
                        |_| panic!("independent successful owner needs no backoff"),
                        |other, original, retry| {
                            assert!(!retry);
                            assert_owner(other, second_identity, original);
                            assert_owner(ticket, first_identity, original);
                            super::super::ensure_shutdown_idle().unwrap();
                            Ok(())
                        },
                    );
                    let ControlFlow::Continue(other) = inner else { panic!("second local owner must finish") };
                    finished_second = Some(other);
                    ticket.document.as_mut().unwrap().cleanup_attempt =
                        Some(CleanupAttemptFailure::Document(DocumentResultSettlementReason::ThreadExitPending));
                    Err("first owner still pending".into())
                } else {
                    assert!(retry);
                    assert_owner(finished_second.as_ref().unwrap(), second_identity, original);
                    Ok(())
                }
            },
        );
        let ControlFlow::Continue(first) = first else { panic!("first local owner must finish") };
        assert_owner(&first, first_identity, deadline);
        assert_owner(finished_second.as_ref().unwrap(), second_identity, deadline);
        assert_eq!(attempts.get(), 2);
        super::super::ensure_shutdown_idle().unwrap();
    }

    #[test]
    fn directory_same_prefix_claims_distinct_roots() {
        let first = directory("kwil-tui-fixture-distinct").unwrap();
        let second = directory("kwil-tui-fixture-distinct").unwrap();
        assert_ne!(first, second);
        assert!(first.is_dir());
        assert!(second.is_dir());
        std::fs::remove_dir(first).unwrap();
        std::fs::remove_dir(second).unwrap();
    }

    #[test]
    fn directory_counter_replay_skips_existing_owners_and_preserves_sentinels() {
        let parent = directory("kwil-tui-fixture-replay").unwrap();
        let next = AtomicU64::new(0);
        let first = directory_in(&parent, "child", &next).unwrap();
        let second = directory_in(&parent, "child", &next).unwrap();
        std::fs::write(first.join("owner.bin"), b"first owner").unwrap();
        std::fs::write(second.join("owner.bin"), b"second owner").unwrap();

        // Replay the candidate sequence to force two collisions without timing.
        let replay = AtomicU64::new(0);
        let third = directory_in(&parent, "child", &replay).unwrap();
        assert_ne!(first, third);
        assert_ne!(second, third);
        assert!(third.is_dir());
        assert_eq!(replay.load(Ordering::Relaxed), 3, "both occupied owners were skipped");
        assert_eq!(std::fs::read(first.join("owner.bin")).unwrap(), b"first owner");
        assert_eq!(std::fs::read(second.join("owner.bin")).unwrap(), b"second owner");
        std::fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn directory_foreign_error_is_returned_without_retry() {
        let parent = directory("kwil-tui-fixture-error").unwrap();
        let next = AtomicU64::new(0);
        let error = directory_in(&parent.join("missing-parent"), "child", &next).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(next.load(Ordering::Relaxed), 1, "only occupied candidates may be retried");
        std::fs::remove_dir(parent).unwrap();
    }
}
