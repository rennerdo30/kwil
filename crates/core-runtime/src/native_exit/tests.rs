use super::*;

#[test]
fn public_types_are_copy_send_sync_and_query_borrows_any_payload() {
    fn scalar<T: Copy + std::fmt::Debug + Eq + Send + Sync>() {}
    fn error<T: std::error::Error + Send + Sync>() {}
    scalar::<NativeExitState>();
    scalar::<NativeExitQueryError>();
    error::<NativeExitQueryError>();
    // The query imposes no Clone, Copy, Send or payload inspection requirement.
    let _: fn(&JoinHandle<std::rc::Rc<()>>) -> QueryResult = native_exit_state::<std::rc::Rc<()>>;
}

#[test]
fn compact_error_display_preserves_full_codes_in_a_stack_writer() {
    use std::io::Write;
    let cases = [
        (
            NativeExitQueryError::WindowsWaitFailed { code: u32::MAX },
            "native thread wait failed (Win32 code 4294967295)",
        ),
        (NativeExitQueryError::UnexpectedWindowsWait { status: 128 }, "unexpected native thread wait status 128"),
        (NativeExitQueryError::UnsupportedTarget, "native thread exit query is unsupported on this target"),
    ];
    for (error, expected) in cases {
        let mut buffer = [0; 128];
        let written = {
            let mut output = std::io::Cursor::new(buffer.as_mut_slice());
            write!(output, "{error}").unwrap();
            output.position() as usize
        };
        assert_eq!(&buffer[..written], expected.as_bytes());
        assert!(std::error::Error::source(&error).is_none());
    }
}

#[cfg(not(windows))]
#[test]
fn unsupported_target_keeps_original_handle_joinable() {
    let worker = std::thread::spawn(|| 73_u32);
    let original = worker.thread().id();
    let result = native_exit_state(&worker);
    let retained = worker.thread().id();
    let joined = worker.join();
    assert_eq!(result, Err(NativeExitQueryError::UnsupportedTarget));
    assert_eq!(retained, original);
    assert_eq!(joined.unwrap(), 73);
}

#[cfg(windows)]
mod windows_controls {
    use super::*;
    use crate::native_exit::windows::{QueryApi, query_with};
    use std::{
        os::windows::io::{AsRawHandle, RawHandle},
        sync::mpsc,
        thread,
        time::{Duration, Instant},
    };
    use windows_sys::Win32::Foundation::{WAIT_ABANDONED_0, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Call {
        Wait { handle: usize, timeout_ms: u32 },
        LastError,
    }
    struct Query {
        status: u32,
        error: u32,
        calls: [Option<Call>; 3],
        count: usize,
    }
    impl Query {
        fn new(status: u32, error: u32) -> Self {
            Self { status, error, calls: [None; 3], count: 0 }
        }
        fn record(&mut self, call: Call) {
            if let Some(slot) = self.calls.get_mut(self.count) {
                *slot = Some(call);
            }
            self.count = self.count.saturating_add(1);
        }
        fn calls(&self) -> &[Option<Call>] {
            &self.calls[..self.count.min(self.calls.len())]
        }
    }
    impl QueryApi for Query {
        fn wait(&mut self, handle: RawHandle, timeout_ms: u32) -> u32 {
            self.record(Call::Wait { handle: handle as usize, timeout_ms });
            self.status
        }
        fn last_error(&mut self) -> u32 {
            self.record(Call::LastError);
            let original = self.error;
            self.error = 0; // A second read would lose the original code.
            original
        }
    }

    #[test]
    fn pending_and_exited_each_use_exactly_one_zero_timeout_wait() {
        let raw = std::ptr::without_provenance_mut(73);
        for (status, expected) in [(WAIT_TIMEOUT, NativeExitState::Pending), (WAIT_OBJECT_0, NativeExitState::Exited)] {
            let mut query = Query::new(status, 0xDEAD_BEEF);
            assert_eq!(query_with(raw, &mut query), Ok(expected));
            assert_eq!(query.calls(), &[Some(Call::Wait { handle: 73, timeout_ms: 0 })]);
            assert_eq!(query.error, 0xDEAD_BEEF, "successful query must not read stale last-error");
        }
    }

    #[test]
    fn failed_wait_captures_original_error_once_as_the_next_query_operation() {
        let raw = std::ptr::without_provenance_mut(19);
        for code in [0, 0xDEAD_BEEF, u32::MAX] {
            let mut query = Query::new(WAIT_FAILED, code);
            let result = query_with(raw, &mut query);
            query.error = 7; // Subsequent native error changes cannot alter the result.
            assert_eq!(result, Err(NativeExitQueryError::WindowsWaitFailed { code }));
            assert_eq!(query.calls(), &[Some(Call::Wait { handle: 19, timeout_ms: 0 }), Some(Call::LastError)]);
        }
    }

    #[test]
    fn unexpected_wait_retains_raw_status_without_reading_stale_last_error() {
        let raw = std::ptr::without_provenance_mut(29);
        for status in [WAIT_ABANDONED_0, 1, u32::MAX - 1] {
            let mut query = Query::new(status, 0xCAFE_BABE);
            assert_eq!(query_with(raw, &mut query), Err(NativeExitQueryError::UnexpectedWindowsWait { status }));
            assert_eq!(query.calls(), &[Some(Call::Wait { handle: 29, timeout_ms: 0 })]);
            assert_eq!(query.error, 0xCAFE_BABE);
        }
    }

    #[test]
    fn pending_and_mock_failure_leave_original_native_handle_for_its_only_join() {
        let deadline = Instant::now() + Duration::from_secs(2);
        let (entered, observed) = mpsc::sync_channel(1);
        let (release, gate) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let _ = entered.send(thread::current().id());
            let _ = gate.recv_timeout(deadline.saturating_duration_since(Instant::now()));
            97_u32
        });
        let original_id = worker.thread().id();
        let original_raw = worker.as_raw_handle() as usize;
        let entered = observed.recv_timeout(deadline.saturating_duration_since(Instant::now()));
        let pending = native_exit_state(&worker);
        let mut failed_query = Query::new(WAIT_FAILED, 5);
        let failed = query_with(worker.as_raw_handle(), &mut failed_query);
        let still_pending = native_exit_state(&worker);
        let retained = (worker.thread().id(), worker.as_raw_handle() as usize);
        // ALWAYS release and join the original thread BEFORE assertions.
        let released = release.send(());
        let joined = worker.join();
        assert_eq!(entered.unwrap(), original_id);
        assert_eq!(pending, Ok(NativeExitState::Pending));
        assert_eq!(failed, Err(NativeExitQueryError::WindowsWaitFailed { code: 5 }));
        assert_eq!(still_pending, Ok(NativeExitState::Pending));
        assert_eq!(retained, (original_id, original_raw));
        assert!(released.is_ok());
        assert_eq!(joined.unwrap(), 97, "query must not consume, duplicate or close the owned native handle");
    }

    struct Epilogue {
        entered: mpsc::SyncSender<thread::ThreadId>,
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
        static NATIVE_EPILOGUE: std::cell::RefCell<Option<Epilogue>> = const { std::cell::RefCell::new(None) };
    }
    // Only this native-exit fixture uses passive readiness backoff. Both Rust
    // completion and native-exit observations share its ONE original deadline.
    fn backoff(deadline: Instant) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if !remaining.is_zero() {
            thread::park_timeout(remaining.min(Duration::from_millis(1)));
        }
    }

    #[test]
    fn actual_native_tls_epilogue_is_pending_after_rust_function_finished() {
        let deadline = Instant::now() + Duration::from_secs(2);
        let (entered, observed) = mpsc::sync_channel(1);
        let (release, gate) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            NATIVE_EPILOGUE.with(|slot| {
                *slot.borrow_mut() = Some(Epilogue { entered, release: gate, deadline });
            });
            113_u32
        });
        let original_id = worker.thread().id();
        let original_raw = worker.as_raw_handle() as usize;
        let entered = observed.recv_timeout(deadline.saturating_duration_since(Instant::now()));
        while entered.is_ok() && !worker.is_finished() && Instant::now() < deadline {
            backoff(deadline);
        }
        let rust_finished = worker.is_finished();
        let native_pending = native_exit_state(&worker);
        let retained = (worker.thread().id(), worker.as_raw_handle() as usize);
        let released = release.send(());
        let mut native_exited = None;
        while Instant::now() < deadline {
            let state = native_exit_state(&worker);
            if state != Ok(NativeExitState::Pending) {
                native_exited = Some(state);
                break;
            }
            backoff(deadline);
        }
        // Cleanup is unconditional even if an observation failed or timed out.
        // The TLS gate itself expires at the same deadline, never a fresh one.
        let joined = worker.join();
        assert_eq!(entered.unwrap(), original_id);
        assert!(rust_finished, "the native TLS control must reproduce the std is_finished gap");
        assert_eq!(native_pending, Ok(NativeExitState::Pending));
        assert_eq!(retained, (original_id, original_raw));
        assert!(released.is_ok());
        assert_eq!(native_exited, Some(Ok(NativeExitState::Exited)));
        assert_eq!(joined.unwrap(), 113);
    }
}
