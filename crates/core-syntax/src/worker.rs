use crate::{Config, Engine, Error, GrammarProvider, Highlights, InputEdit, Language, TextSource};
use std::{
    ops::Range,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
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
}

/// One worker, one pending request, eight retirement slots, one result. No idle
/// timer or unbounded channels. `wake` only schedules a UI event; it must not block.
pub struct Worker {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    previous: Option<Arc<Highlights>>,
}
impl Worker {
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
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                pending: None,
                retired: [const { None }; RETIRE_CAPACITY],
                stopped: false,
                result: None,
            }),
            ready: Condvar::new(),
            generation: AtomicU64::new(0),
        });
        let worker = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("kwil-syntax".into())
            .spawn(move || {
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
        Ok(Self { shared, thread: Some(thread), previous: None })
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
        let mut state = self.shared.state.lock().unwrap();
        self.shared.generation.fetch_add(1, Ordering::AcqRel);
        state.stopped = true;
        self.shared.ready.notify_one();
    }
    /// Off-UI orderly shutdown. Source reads and wake callbacks must be bounded.
    pub fn shutdown(mut self) -> thread::Result<()> {
        self.stop();
        self.thread.take().unwrap().join()
    }
}
impl Drop for Worker {
    // Dropping on the UI never joins a parser; cancellation ends the owned worker.
    fn drop(&mut self) {
        self.stop();
    }
}

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
