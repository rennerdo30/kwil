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
struct State {
    pending: Option<Job>,
    stopped: bool,
    result: Option<(u64, Result<Arc<Highlights>, Error>)>,
}
struct Shared {
    state: Mutex<State>,
    ready: Condvar,
    generation: AtomicU64,
}

/// One worker, one replaceable pending request, one replaceable result. No idle
/// timer, no unbounded channels. `wake` only schedules a UI event; it must not block.
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
            state: Mutex::new(State { pending: None, stopped: false, result: None }),
            ready: Condvar::new(),
            generation: AtomicU64::new(0),
        });
        let worker = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("kwil-syntax".into())
            .spawn(move || {
                loop {
                    let job = {
                        let mut state = worker.state.lock().unwrap();
                        while state.pending.is_none() && !state.stopped {
                            state = worker.ready.wait(state).unwrap();
                        }
                        if state.stopped {
                            break;
                        }
                        state.pending.take().unwrap()
                    };
                    let cancelled = || worker.generation.load(Ordering::Acquire) != job.generation;
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
                    let publish = {
                        let mut state = worker.state.lock().unwrap();
                        if !cancelled() && !state.stopped {
                            state.result = Some((job.generation, result));
                            true
                        } else {
                            false
                        }
                    };
                    if publish {
                        wake();
                    }
                }
            })
            .map_err(Error::Io)?;
        Ok(Self { shared, thread: Some(thread), previous: None })
    }

    /// Constant-size queue update; never waits on parsing or source I/O.
    pub fn submit(&self, request: Request) -> Result<u64, Error> {
        let mut state = self.shared.state.lock().unwrap();
        if state.stopped {
            return Err(Error::Closed);
        }
        let generation = self.shared.generation.fetch_add(1, Ordering::AcqRel) + 1;
        state.pending = Some(Job { generation, request });
        state.result = None;
        self.shared.ready.notify_one();
        Ok(generation)
    }
    pub fn cancel(&self) {
        let mut state = self.shared.state.lock().unwrap();
        self.shared.generation.fetch_add(1, Ordering::AcqRel);
        state.pending = None;
        state.result = None;
    }
    /// Consume the latest result after a wake event. Errors retain `previous()`.
    pub fn take_update(&mut self) -> Option<Result<Arc<Highlights>, Error>> {
        let mut state = self.shared.state.lock().unwrap();
        let (generation, result) = state.result.take()?;
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
        state.pending = None;
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
