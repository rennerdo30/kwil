use crate::{Config, Engine, Error, GrammarProvider, Highlights, InputEdit, Language, TextSource};
use kwil_core_runtime::{NativeExitObservation, NativeExitState, NativeObservationError};
use std::{
    ops::Range,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::Instant,
};

/// A snapshot plus a visible range. Revisions identify snapshots, not viewports.
/// Describe an edit against `previous_revision`; absent bases cause a fresh parse.
pub struct Request {
    pub source: Arc<dyn TextSource>,
    pub language: Language,
    pub revision: u64,
    pub previous_revision: Option<u64>,
    pub edit: Option<InputEdit>,
    pub visible: Range<usize>,
}
struct Job {
    generation: u64,
    request: Request,
}
const RETIRE_CAPACITY: usize = 8;

/// A rejected request remains caller-owned. Retain it for a later retry rather
/// than destroying a possibly large snapshot on the UI thread.
pub struct SubmitError {
    pub request: Request,
    pub error: Error,
}
impl std::fmt::Debug for SubmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubmitError").field("error", &self.error).finish_non_exhaustive()
    }
}
struct State {
    pending: Option<Job>,
    retired: [Option<Job>; RETIRE_CAPACITY],
    stopped: bool,
    result: Option<(u64, Result<Arc<Highlights>, Error>)>,
}
struct Shared {
    state: Mutex<State>,
    ready: Condvar,
    generation: AtomicU64,
    completed: AtomicBool,
    native_exit: NativeExitObservation,
}
struct Completion<'a>(&'a Shared);
impl Drop for Completion<'_> {
    fn drop(&mut self) {
        let _state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
        self.0.completed.store(true, Ordering::Release);
        self.0.ready.notify_all();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerSettlementReason {
    TimedOut,
    ThreadExitPending,
    WorkerPanicked,
    NativeObservation(NativeObservationError),
}

/// One worker, one pending request, eight retirement slots, one result. No idle
/// timer or unbounded channels. `wake` only schedules a UI event; it must not block.
pub struct Worker {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    previous: Option<Arc<Highlights>>,
    panic: Option<Box<dyn std::any::Any + Send + 'static>>,
    native_retired: bool,
    settled: bool,
}
/// Failure owns the original facade; Drop never joins or performs native I/O.
pub struct ShutdownError {
    owner: Mutex<Option<Worker>>,
    reason: WorkerSettlementReason,
}
impl ShutdownError {
    pub fn reason(&self) -> WorkerSettlementReason {
        self.reason
    }
    pub fn into_worker(self) -> Worker {
        self.owner.into_inner().unwrap_or_else(|error| error.into_inner()).expect("original shutdown owner")
    }
}
impl std::fmt::Display for ShutdownError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(out, "worker shutdown {:?}; exact owner retained", self.reason)
    }
}
impl std::fmt::Debug for ShutdownError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, out)
    }
}
impl std::error::Error for ShutdownError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.reason {
            WorkerSettlementReason::NativeObservation(error) => Some(error),
            _ => None,
        }
    }
}

impl Worker {
    /// All declared facade/shared headers, aligned Arc counter allocation and
    /// completion/typed-return overlap. Engine/grammar/source payload accounting
    /// is unchanged and separate. Reserve in the host BEFORE construction.
    pub const fn retained_metadata_bytes() -> usize {
        match Self::checked_retained_metadata_bytes() {
            Some(bytes) => bytes,
            None => usize::MAX,
        }
    }
    pub const fn checked_retained_metadata_bytes() -> Option<usize> {
        #[repr(C)]
        struct Allocation {
            strong: std::sync::atomic::AtomicUsize,
            weak: std::sync::atomic::AtomicUsize,
            data: Shared,
        }
        let bytes = Some(0usize);
        let bytes = metadata_add(bytes, std::mem::size_of::<Self>());
        let bytes = metadata_add(bytes, std::mem::size_of::<Allocation>());
        let bytes = metadata_add(bytes, std::mem::size_of::<Completion<'static>>());
        let bytes = metadata_add(bytes, std::mem::size_of::<Result<(), WorkerSettlementReason>>());
        let bytes = metadata_add(bytes, std::mem::size_of::<ShutdownError>());
        let bytes = metadata_add(bytes, std::mem::size_of::<Result<(), ShutdownError>>());
        match NativeExitObservation::operation_stack_bytes() {
            Some(scratch) => metadata_add(metadata_add(bytes, scratch), std::mem::size_of::<&Instant>()),
            None => None,
        }
    }
    /// Exact facade + aligned existing Arc allocation growth; prepaid through
    /// the host's SAME metadata admission before construction. No allocation is
    /// added for completion or the slot retaining the original panic payload.
    pub const fn settlement_metadata_bytes() -> usize {
        #[allow(dead_code)]
        struct BeforeWorker {
            shared: Arc<Shared>,
            thread: Option<JoinHandle<()>>,
            previous: Option<Arc<Highlights>>,
        }
        #[allow(dead_code)]
        struct BeforeShared {
            state: Mutex<State>,
            ready: Condvar,
            generation: AtomicU64,
        }
        #[repr(C)]
        struct Allocation<T> {
            strong: std::sync::atomic::AtomicUsize,
            weak: std::sync::atomic::AtomicUsize,
            data: T,
        }
        std::mem::size_of::<Self>() - std::mem::size_of::<BeforeWorker>() + std::mem::size_of::<Allocation<Shared>>()
            - std::mem::size_of::<Allocation<BeforeShared>>()
    }
    pub fn new(config: Config, wake: impl Fn() + Send + Sync + 'static) -> Result<Self, Error> {
        Self::start(Engine::new(config)?, wake)
    }
    pub fn with_provider(
        config: Config,
        provider: Arc<dyn GrammarProvider>,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self, Error> {
        Self::start(Engine::with_provider(config, provider)?, wake)
    }
    fn start(mut engine: Engine, wake: impl Fn() + Send + Sync + 'static) -> Result<Self, Error> {
        Self::checked_retained_metadata_bytes().ok_or(Error::InvalidConfig)?;
        let shared = Arc::new(Shared {
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
        });
        let worker = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("kwil-syntax".into())
            .spawn(move || {
                let _completion = Completion(&worker);
                if worker.native_exit.register_current().is_err() {
                    worker.state.lock().unwrap_or_else(|error| error.into_inner()).stopped = true;
                }
                loop {
                    let (job, retired, stopped) = {
                        let mut state = worker.state.lock().unwrap();
                        while state.pending.is_none() && !state.stopped {
                            state = worker.ready.wait(state).unwrap();
                        }
                        (
                            state.pending.take(),
                            std::mem::replace(&mut state.retired, [const { None }; RETIRE_CAPACITY]),
                            state.stopped,
                        )
                    };
                    // Destruction may unmap a source or release a whole piece tree.
                    // Never perform it on the submitter or under the mailbox lock.
                    drop(retired);
                    if stopped {
                        drop(job);
                        break;
                    }
                    let Some(job) = job else { continue };
                    let cancelled = || worker.generation.load(Ordering::Acquire) != job.generation;
                    if cancelled() {
                        continue;
                    }
                    let r = job.request;
                    let result = engine
                        .highlight(
                            r.source.as_ref(),
                            r.language,
                            r.revision,
                            r.previous_revision,
                            r.edit.as_ref(),
                            r.visible,
                            &cancelled,
                        )
                        .map(Arc::new);
                    let (publish, old_result) = {
                        let mut state = worker.state.lock().unwrap();
                        if !cancelled() && !state.stopped {
                            (true, state.result.replace((job.generation, result)))
                        } else {
                            (false, None)
                        }
                    };
                    drop(old_result);
                    if publish {
                        wake();
                    }
                }
            })
            .map_err(Error::Io)?;
        Ok(Self { shared, thread: Some(thread), previous: None, panic: None, native_retired: false, settled: false })
    }

    /// Constant-size queue update; never waits on parsing or source I/O.
    /// Saturation returns ownership without waiting or invalidating accepted work.
    pub fn submit(&self, request: Request) -> Result<u64, Box<SubmitError>> {
        let mut state = self.shared.state.lock().unwrap();
        if state.stopped {
            return Err(Box::new(SubmitError { request, error: Error::Closed }));
        }
        if state.pending.is_some() {
            let Some(index) = state.retired.iter().position(Option::is_none) else {
                return Err(Box::new(SubmitError { request, error: Error::Busy }));
            };
            state.retired[index] = state.pending.take();
        }
        let generation = self.shared.generation.fetch_add(1, Ordering::AcqRel) + 1;
        state.pending = Some(Job { generation, request });
        self.shared.ready.notify_one();
        Ok(generation)
    }
    pub fn cancel(&self) {
        let _state = self.shared.state.lock().unwrap();
        self.shared.generation.fetch_add(1, Ordering::AcqRel);
        // Leave accepted snapshots/results for the worker to reclaim.
        self.shared.ready.notify_one();
    }
    /// Consume the latest result after a wake event. Errors retain `previous()`.
    pub fn take_update(&mut self) -> Option<Result<Arc<Highlights>, Error>> {
        let mut state = self.shared.state.lock().unwrap();
        let (generation, result) = state.result.take()?;
        drop(state);
        if generation != self.shared.generation.load(Ordering::Acquire) {
            return None;
        }
        if let Ok(highlights) = &result {
            self.previous = Some(Arc::clone(highlights));
        }
        Some(result)
    }
    pub fn previous(&self) -> Option<&Arc<Highlights>> {
        self.previous.as_ref()
    }
    fn stop(&self) {
        let mut state = self.shared.state.lock().unwrap_or_else(|error| error.into_inner());
        self.shared.generation.fetch_add(1, Ordering::AcqRel);
        state.stopped = true;
        self.shared.ready.notify_one();
    }
    /// Off-UI orderly shutdown. Source reads and wake callbacks must be bounded.
    /// Explicit OFF-UI UNBOUNDED compatibility join. Never call from a bounded
    /// shutdown protocol; failure retains the complete original owner.
    pub fn shutdown(mut self) -> Result<(), ShutdownError> {
        self.shutdown_in_place().map_err(|reason| ShutdownError { owner: Mutex::new(Some(self)), reason })
    }
    pub fn shutdown_until(mut self, deadline: Instant) -> Result<(), ShutdownError> {
        self.settle_until(deadline).map_err(|reason| ShutdownError { owner: Mutex::new(Some(self)), reason })
    }
    pub(crate) fn shutdown_in_place(&mut self) -> Result<(), WorkerSettlementReason> {
        self.stop();
        if let Some(handle) = self.thread.take()
            && let Err(panic) = handle.join()
        {
            self.panic = Some(panic);
        }
        self.finish_join_with(None, NativeExitObservation::retire_after_join)
    }
    /// OFF UI. Existing completion ACK, then one shared native deadline wait,
    /// using the ORIGINAL deadline. ACK/is_finished alone
    /// never authorizes join. Every refusal retains the exact original owners.
    pub fn settle_until(&mut self, deadline: Instant) -> Result<(), WorkerSettlementReason> {
        self.settle_with(
            deadline,
            |observation, handle| observation.wait_until(handle, deadline),
            NativeExitObservation::retire_after_join,
        )
    }
    fn settle_with(
        &mut self,
        deadline: Instant,
        query: impl FnOnce(&NativeExitObservation, &JoinHandle<()>) -> Result<NativeExitState, NativeObservationError>,
        retire: impl FnOnce(&NativeExitObservation) -> Result<(), NativeObservationError>,
    ) -> Result<(), WorkerSettlementReason> {
        self.stop();
        if self.thread.is_some() {
            let shared = &self.shared;
            let guard = shared.state.lock().unwrap_or_else(|error| error.into_inner());
            let (guard, _) = shared
                .ready
                .wait_timeout_while(guard, deadline.saturating_duration_since(Instant::now()), |_| {
                    !shared.completed.load(Ordering::Acquire)
                })
                .unwrap_or_else(|error| error.into_inner());
            let completed = shared.completed.load(Ordering::Acquire);
            drop(guard);
            if !completed {
                return Err(WorkerSettlementReason::TimedOut);
            }
            // Query before consuming anything, even after a cleanup ACK. It also
            // retains original registration errors and any partial owned right.
            match query(&shared.native_exit, self.thread.as_ref().expect("original worker handle")) {
                Ok(NativeExitState::Pending) => return Err(WorkerSettlementReason::ThreadExitPending),
                Err(error) => return Err(WorkerSettlementReason::NativeObservation(error)),
                Ok(NativeExitState::Exited) => {}
            }
            if Instant::now() >= deadline {
                return Err(WorkerSettlementReason::TimedOut);
            }
            if let Err(panic) = self.thread.take().expect("native-exited original handle").join() {
                self.panic = Some(panic);
            }
        }
        // thread=None means the ORIGINAL join already returned, including panic;
        // it does not imply native-right retirement completed on a prior call.
        self.finish_join_with(Some(deadline), retire)
    }
    fn finish_join_with(
        &mut self,
        deadline: Option<Instant>,
        retire: impl FnOnce(&NativeExitObservation) -> Result<(), NativeObservationError>,
    ) -> Result<(), WorkerSettlementReason> {
        self.finish_join_with_clock(deadline, retire, Instant::now)
    }
    fn finish_join_with_clock(
        &mut self,
        deadline: Option<Instant>,
        retire: impl FnOnce(&NativeExitObservation) -> Result<(), NativeObservationError>,
        mut now: impl FnMut() -> Instant,
    ) -> Result<(), WorkerSettlementReason> {
        // Once the right is retired, sticky diagnostics precede deadline-only
        // no-op retries. No consumed panic payload can turn into fake success.
        if self.native_retired {
            if self.panic.is_some() {
                return Err(WorkerSettlementReason::WorkerPanicked);
            }
            if let Some(error) = self.shared.native_exit.registration_error() {
                return Err(WorkerSettlementReason::NativeObservation(error));
            }
            if self.settled {
                return Ok(());
            }
        }
        if !self.settled && deadline.is_some_and(|deadline| now() >= deadline) {
            return Err(WorkerSettlementReason::TimedOut);
        }
        if !self.native_retired {
            if deadline.is_some_and(|deadline| now() >= deadline) {
                return Err(WorkerSettlementReason::TimedOut);
            }
            retire(&self.shared.native_exit).map_err(WorkerSettlementReason::NativeObservation)?;
            self.native_retired = true;
            if deadline.is_some_and(|deadline| now() >= deadline) {
                return Err(WorkerSettlementReason::TimedOut);
            }
        }
        if self.panic.is_some() {
            return Err(WorkerSettlementReason::WorkerPanicked);
        }
        if let Some(error) = self.shared.native_exit.registration_error() {
            return Err(WorkerSettlementReason::NativeObservation(error));
        }
        self.settled = true;
        Ok(())
    }
    /// Scalar-only original native retirement evidence; no native query or I/O.
    pub fn first_native_retirement_error(&self) -> Option<NativeObservationError> {
        self.shared.native_exit.first_retirement_error()
    }
    pub fn panic_payload(&self) -> Option<&(dyn std::any::Any + Send + 'static)> {
        self.panic.as_deref()
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.stop();
        if self.thread.is_some() || !self.settled || self.panic.is_some() {
            // Misuse quarantine, NOT successful retirement: retain the SAME
            // shared allocation/source headers/native right and exact handle.
            // No native I/O, source destruction, new allocation or cleanup thread.
            std::mem::forget(Arc::clone(&self.shared));
            std::mem::forget(self.thread.take());
            std::mem::forget(self.panic.take());
            std::mem::forget(self.previous.take());
        }
    }
}

// Checked declared Rust-value accounting, not compiler/native-stack usage.
const fn metadata_add(bytes: Option<usize>, extra: usize) -> Option<usize> {
    match bytes {
        Some(bytes) => bytes.checked_add(extra),
        None => None,
    }
}

#[cfg(test)]
#[path = "worker/bounded_settlement_tests.rs"]
mod bounded_settlement_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::{borrow::Cow, io, sync::mpsc, time::Duration};

    struct Source {
        gate: Option<Mutex<mpsc::Receiver<()>>>,
        entered: mpsc::Sender<()>,
        dropped: mpsc::Sender<(thread::ThreadId, String, bool)>,
        probe: std::sync::Weak<Shared>,
    }
    impl TextSource for Source {
        fn len(&self) -> usize {
            1
        }
        fn chunk(&self, _: usize, _: usize) -> io::Result<Cow<'_, [u8]>> {
            if let Some(gate) = &self.gate {
                self.entered.send(()).unwrap();
                gate.lock().unwrap().recv_timeout(Duration::from_secs(5)).unwrap();
            }
            Ok(Cow::Borrowed(b"x"))
        }
    }
    impl Drop for Source {
        fn drop(&mut self) {
            let unlocked = self.probe.upgrade().is_none_or(|shared| shared.state.try_lock().is_ok());
            let current = thread::current();
            let _ = self.dropped.send((current.id(), current.name().unwrap_or("").into(), unlocked));
        }
    }
    fn request(source: Source) -> Request {
        Request {
            source: Arc::new(source),
            language: Language::Rust,
            revision: 1,
            previous_revision: None,
            edit: None,
            visible: 0..1,
        }
    }

    #[test]
    fn shutdown_retires_pending_on_worker() {
        // A lexical read gates the worker independently of enabled grammar features.
        let worker = Worker::new(Config { parse_limit: 0, ..Config::default() }, || {}).unwrap();
        let (entered, waiting) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let (dropped, observed) = mpsc::channel();
        worker
            .submit(request(Source {
                gate: Some(Mutex::new(gate)),
                entered: entered.clone(),
                dropped: dropped.clone(),
                probe: Arc::downgrade(&worker.shared),
            }))
            .unwrap();
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        worker.submit(request(Source { gate: None, entered, dropped, probe: Arc::downgrade(&worker.shared) })).unwrap();
        let shared = Arc::clone(&worker.shared);
        let releaser = thread::spawn(move || {
            let state = shared.state.lock().unwrap();
            let (state, timeout) =
                shared.ready.wait_timeout_while(state, Duration::from_secs(5), |state| !state.stopped).unwrap();
            assert!(!timeout.timed_out());
            drop(state);
            release.send(()).unwrap();
        });
        worker.shutdown().unwrap();
        releaser.join().unwrap();
        for _ in 0..2 {
            let (id, name, unlocked) = observed.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_ne!(id, thread::current().id());
            assert_eq!(name, "kwil-syntax");
            assert!(unlocked, "snapshot destructor holds mailbox lock");
        }
    }

    #[test]
    fn retirement_is_bounded_and_rejection_returns_ownership() {
        let worker = Worker::new(Config { parse_limit: 0, ..Config::default() }, || {}).unwrap();
        let (entered, waiting) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let (dropped, observed) = mpsc::channel();
        worker
            .submit(request(Source {
                gate: Some(Mutex::new(gate)),
                entered: entered.clone(),
                dropped: dropped.clone(),
                probe: Arc::downgrade(&worker.shared),
            }))
            .unwrap();
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        // One pending plus eight retired requests is the documented storage cap.
        for _ in 0..9 {
            worker
                .submit(request(Source {
                    gate: None,
                    entered: entered.clone(),
                    dropped: dropped.clone(),
                    probe: Arc::downgrade(&worker.shared),
                }))
                .unwrap();
        }
        let rejected = worker
            .submit(request(Source { gate: None, entered, dropped, probe: Arc::downgrade(&worker.shared) }))
            .unwrap_err();
        assert!(matches!(rejected.error, Error::Busy));
        assert!(observed.try_recv().is_err(), "submit destroyed an accepted or rejected source");
        worker.cancel();
        release.send(()).unwrap();
        for _ in 0..10 {
            let (id, name, unlocked) = observed.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_ne!(id, thread::current().id());
            assert_eq!(name, "kwil-syntax");
            assert!(unlocked, "snapshot destructor holds mailbox lock");
        }
        worker.shutdown().unwrap();
        assert_eq!(rejected.request.source.len(), 1, "rejected snapshot remains owned by caller");
    }
}
