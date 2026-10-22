//! KW-47 frontend hooks. Own ONE `HighlightWorker` per frontend, not per tab.
//!
//! On open/edit/scroll/picker change, submit the immutable snapshot, session
//! DocumentId, revision and a monotonically increasing viewport generation.
//! Increase the generation for picker/settings changes too. `submit` only moves
//! handles into a bounded mailbox; compilation, detection, I/O and parsing run
//! on the syntax worker. Use `SnapshotTextSource`, never collect a file's bytes.
//!
//! The wake callback only posts an event. On that event call `take_update` with
//! the CURRENT key; paint only matching spans (absolute document byte offsets).
//! `HighlightState` retains the last successful result while a request runs or
//! fails. It never returns a different document's highlights. The frontend owns
//! that small state per tab and may additionally clip spans to its current rows.
//! Map `Class::name()` through the active theme; this module does no drawing.
//!
//! Use `Language::CATALOGUE` and `LanguageChoice` for the status-bar/palette
//! picker. Store `LanguageChoice` per session DocumentId (persist alongside a
//! file's tab metadata if desired). `Automatic` clears the per-file override;
//! extension persistence edits settings and resubmits mappings on the next
//! request. Settings strings become typed `LanguageMapping`s on the SETTINGS
//! worker via `Language::from_id`; unsupported names are errors, never Rust.
//! Existing settings bare extension keys and full globs are both supported.
//!
//! On `Busy`, retain the returned request and retry after a worker event. Do
//! not destroy its last snapshot reference on the UI. Accepted/superseded jobs
//! retire on the worker, outside the mailbox lock. `Drop` only signals stop;
//! orderly `shutdown` joins OFF the UI. No idle polling or per-request threads.
//!
//! `with_provider` exposes the existing prebuilt grammar seam. A provider must
//! verify permissive licence/version/hash/ABI provenance and retain its library
//! handles. No downloader or unverified grammar catalogue is implied here.
use kwil_core_syntax::detection::Detector;
pub use kwil_core_syntax::detection::{Detection, DetectionOrigin, LanguageMapping};
pub use kwil_core_syntax::{Class, Language, Mode, Span};
use kwil_core_syntax::{Config, Engine, Error, GrammarProvider, Highlights, InputEdit, TextSource};
use kwil_core_text::Snapshot;
pub use kwil_core_workspace::session::DocumentId;
use std::{
    borrow::Cow,
    io,
    ops::Range,
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
};

pub const SOURCE_CHUNK_BYTES: usize = 64 * 1024;
pub const MAX_WINDOW_BYTES: usize = 256 * 1024;
pub const MAX_RESULT_SPANS: usize = 64 * 1024;
const RETIRE_CAPACITY: usize = 8;

/// O(1) snapshot ownership, zero document copies. Only `chunk` accesses storage,
/// on the worker; it borrows the first piece intersecting the requested range.
pub struct SnapshotTextSource {
    snapshot: Snapshot,
}
impl SnapshotTextSource {
    pub fn new(snapshot: Snapshot) -> Self {
        Self { snapshot }
    }
}
impl TextSource for SnapshotTextSource {
    fn len(&self) -> usize {
        usize::try_from(self.snapshot.len()).unwrap_or(usize::MAX)
    }
    fn chunk(&self, offset: usize, max_len: usize) -> io::Result<Cow<'_, [u8]>> {
        if offset > self.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "syntax offset past EOF"));
        }
        let end = self.len().min(offset.saturating_add(max_len.min(SOURCE_CHUNK_BYTES)));
        let first = self.snapshot.chunks(offset as u64..end as u64)?.next();
        match first {
            Some(bytes) => Ok(Cow::Borrowed(bytes.map_err(io::Error::other)?)),
            None => Ok(Cow::Borrowed(b"")),
        }
    }
}

/// Copy this state into each request. No path lookup or mutable global override
/// map: two tabs with the same revision remain independently selectable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LanguageChoice {
    #[default]
    Automatic,
    Explicit(Language),
}
impl LanguageChoice {
    pub fn language(self) -> Option<Language> {
        match self {
            Self::Automatic => None,
            Self::Explicit(language) => Some(language),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HighlightKey {
    pub document_id: DocumentId,
    pub revision: u64,
    pub viewport_generation: u64,
}

/// Metadata and handles only. The caller prepares paths/settings on their
/// existing workers; submitting does not inspect them or touch source bytes.
pub struct HighlightRequest {
    pub key: HighlightKey,
    pub source: Arc<dyn TextSource>,
    pub path: Arc<PathBuf>,
    pub choice: LanguageChoice,
    /// Stable, bounded settings order. Compiled lazily on the syntax worker.
    pub mappings: Arc<[LanguageMapping]>,
    pub visible: Range<usize>,
    pub previous_revision: Option<u64>,
    pub edit: Option<InputEdit>,
}

#[derive(Debug)]
pub struct PublishedHighlights {
    pub key: HighlightKey,
    pub detection: Detection,
    pub highlights: Highlights,
}

/// Retained per tab. Failed/stale updates do not erase successful highlights.
#[derive(Default)]
pub struct HighlightState {
    previous: Option<Arc<PublishedHighlights>>,
}
impl HighlightState {
    pub fn previous(&self, document_id: DocumentId) -> Option<&Arc<PublishedHighlights>> {
        self.previous.as_ref().filter(|p| p.key.document_id == document_id)
    }
    pub fn apply(&mut self, current: HighlightKey, update: Arc<PublishedHighlights>) -> bool {
        if update.key != current {
            return false;
        }
        self.previous = Some(update);
        true
    }
}

pub struct HighlightSubmitError {
    pub request: HighlightRequest,
    pub error: Error,
}
impl std::fmt::Debug for HighlightSubmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HighlightSubmitError").field("error", &self.error).finish_non_exhaustive()
    }
}
struct Job {
    generation: u64,
    request: HighlightRequest,
}
struct State {
    pending: Option<Job>,
    retired: [Option<Job>; RETIRE_CAPACITY],
    result: Option<(u64, HighlightKey, Result<Arc<PublishedHighlights>, Error>)>,
    stopped: bool,
}
struct Shared {
    state: Mutex<State>,
    ready: Condvar,
    generation: AtomicU64,
}

/// One active + one latest pending job, eight retirement slots and one result.
/// At most eighteen accepted source handles including a draining retire batch.
pub struct HighlightWorker {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}
impl HighlightWorker {
    pub fn new(config: Config, wake: impl Fn() + Send + Sync + 'static) -> Result<Self, Error> {
        Self::start(config, None, wake)
    }
    pub fn with_provider(
        config: Config,
        provider: Arc<dyn GrammarProvider>,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self, Error> {
        Self::start(config, Some(provider), wake)
    }
    fn start(
        config: Config,
        provider: Option<Arc<dyn GrammarProvider>>,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self, Error> {
        // Upper bounds remain hard even if a caller supplies a larger Config.
        // Creating the parser here performs no source I/O or grammar loading.
        if config.max_window_bytes > MAX_WINDOW_BYTES
            || config.parse_limit > 20 * 1024 * 1024
            || config.max_capture_bytes > SOURCE_CHUNK_BYTES
        {
            return Err(Error::InvalidConfig);
        }
        let mut engine = match provider {
            Some(provider) => Engine::with_provider(config, provider)?,
            None => Engine::new(config)?,
        };
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                pending: None,
                retired: [const { None }; RETIRE_CAPACITY],
                result: None,
                stopped: false,
            }),
            ready: Condvar::new(),
            generation: AtomicU64::new(0),
        });
        let worker = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("kwil-highlighting".into())
            .spawn(move || {
                let mut document = None;
                let mut settings: Option<Arc<[LanguageMapping]>> = None;
                let mut detector = None;
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
                    drop(retired);
                    if stopped {
                        drop(job);
                        break;
                    }
                    let Some(job) = job else {
                        continue;
                    };
                    let cancelled = || worker.generation.load(Ordering::Acquire) != job.generation;
                    if cancelled() {
                        continue;
                    }
                    let request = &job.request;
                    let key = request.key;
                    let result = (|| {
                        // Reset independently of revision. Tab A rev 0 and tab B rev
                        // 0 are unrelated snapshots, even with identical languages.
                        if document != Some(key.document_id) {
                            engine.reset_document();
                            document = Some(key.document_id);
                        }
                        if settings.as_ref().is_none_or(|s| !Arc::ptr_eq(s, &request.mappings)) {
                            let compiled = Detector::new(&request.mappings)?;
                            detector = Some(compiled);
                            settings = Some(Arc::clone(&request.mappings));
                        }
                        let detection = detector.as_ref().unwrap().detect(
                            &request.path,
                            request.choice.language(),
                            request.source.as_ref(),
                            &cancelled,
                        )?;
                        let highlights = engine.highlight(
                            request.source.as_ref(),
                            detection.language,
                            key.revision,
                            request.previous_revision,
                            request.edit.as_ref(),
                            request.visible.clone(),
                            &cancelled,
                        )?;
                        if highlights.spans.len() > MAX_RESULT_SPANS {
                            return Err(Error::QueryLimit);
                        }
                        Ok(Arc::new(PublishedHighlights { key, detection, highlights }))
                    })();
                    let (publish, old_result) = {
                        let mut state = worker.state.lock().unwrap();
                        if !cancelled() && !state.stopped {
                            (true, state.result.replace((job.generation, key, result)))
                        } else {
                            (false, None)
                        }
                    };
                    drop(old_result);
                    // Accepted request/snapshot destruction stays on this thread.
                    drop(job);
                    if publish {
                        wake();
                    }
                }
            })
            .map_err(Error::Io)?;
        Ok(Self { shared, thread: Some(thread) })
    }

    /// Constant mailbox work, no source access, glob compilation or destruction
    /// of superseded requests. Saturation returns ownership for later retry.
    pub fn submit(&self, request: HighlightRequest) -> Result<u64, Box<HighlightSubmitError>> {
        let mut state = self.shared.state.lock().unwrap();
        if state.stopped {
            return Err(Box::new(HighlightSubmitError { request, error: Error::Closed }));
        }
        if state.pending.is_some() {
            let Some(index) = state.retired.iter().position(Option::is_none) else {
                return Err(Box::new(HighlightSubmitError { request, error: Error::Busy }));
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
        self.shared.ready.notify_one();
    }

    /// Consume on wake; reject a different DocumentId, revision OR viewport.
    /// Matching errors leave the frontend's HighlightState unchanged.
    pub fn take_update(&self, current: HighlightKey) -> Option<Result<Arc<PublishedHighlights>, Error>> {
        let mut state = self.shared.state.lock().unwrap();
        let (generation, key, result) = state.result.take()?;
        if generation != self.shared.generation.load(Ordering::Acquire) || key != current {
            return None;
        }
        Some(result)
    }

    fn stop(&self) {
        let mut state = self.shared.state.lock().unwrap();
        self.shared.generation.fetch_add(1, Ordering::AcqRel);
        state.stopped = true;
        self.shared.ready.notify_one();
    }
    pub fn shutdown(mut self) -> thread::Result<()> {
        self.stop();
        self.thread.take().unwrap().join()
    }
}
impl Drop for HighlightWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kwil_core_text::Buffer;
    use std::{
        sync::{atomic::AtomicBool, mpsc},
        time::Duration,
    };

    fn key(id: u64, revision: u64, generation: u64) -> HighlightKey {
        HighlightKey { document_id: DocumentId(id), revision, viewport_generation: generation }
    }
    fn request(key: HighlightKey, source: Arc<dyn TextSource>) -> HighlightRequest {
        HighlightRequest {
            key,
            visible: 0..source.len(),
            source,
            path: Arc::new(PathBuf::from("unknown")),
            choice: LanguageChoice::Explicit(Language::Generic),
            mappings: Arc::from([]),
            previous_revision: None,
            edit: None,
        }
    }
    fn worker(config: Config) -> (HighlightWorker, mpsc::Receiver<()>) {
        let (wake, events) = mpsc::channel();
        (
            HighlightWorker::new(config, move || {
                let _ = wake.send(());
            })
            .unwrap(),
            events,
        )
    }
    fn wait(events: &mpsc::Receiver<()>) {
        events.recv_timeout(Duration::from_secs(5)).unwrap();
    }

    #[test]
    fn stale_document_id_result() {
        let (worker, events) = worker(Config::default());
        worker.submit(request(key(1, 0, 1), Arc::new(b"[42]".to_vec()))).unwrap();
        wait(&events);
        assert!(worker.take_update(key(2, 0, 1)).is_none(), "same revision/generation must not cross tabs");
        worker.shutdown().unwrap();
    }

    #[test]
    fn stale_viewport_result() {
        let (worker, events) = worker(Config::default());
        worker.submit(request(key(1, 0, 1), Arc::new(b"[42]".to_vec()))).unwrap();
        wait(&events);
        assert!(worker.take_update(key(1, 0, 2)).is_none(), "same document/revision must not cross viewports");
        worker.shutdown().unwrap();
    }

    #[test]
    fn same_revision_different_document_resets_parser() {
        let (worker, events) = worker(Config::default());
        let mut first = request(key(1, 0, 1), Arc::new(b"let x=42;".to_vec()));
        first.choice = LanguageChoice::Explicit(Language::Rust);
        worker.submit(first).unwrap();
        wait(&events);
        let first = worker.take_update(key(1, 0, 1)).unwrap().unwrap();
        assert_eq!(first.highlights.mode, Mode::TreeSitter);
        let mut second = request(key(2, 0, 1), Arc::new(b"/* hi */".to_vec()));
        second.choice = LanguageChoice::Explicit(Language::Rust);
        worker.submit(second).unwrap();
        wait(&events);
        let second = worker.take_update(key(2, 0, 1)).unwrap().unwrap();
        assert!(!second.highlights.incremental);
        assert!(second.highlights.spans.iter().any(|s| s.range == (0..8) && s.class == Class::Comment));
        worker.shutdown().unwrap();
    }

    #[test]
    fn errors_keep_prior_highlights_and_state_rejects_stale() {
        let (worker, events) = worker(Config::default());
        worker.submit(request(key(1, 1, 1), Arc::new(b"(42)".to_vec()))).unwrap();
        wait(&events);
        let previous = worker.take_update(key(1, 1, 1)).unwrap().unwrap();
        let mut state = HighlightState::default();
        assert!(state.apply(key(1, 1, 1), Arc::clone(&previous)));
        assert!(!state.apply(key(2, 1, 1), Arc::clone(&previous)));
        assert!(!state.apply(key(1, 1, 2), Arc::clone(&previous)));
        assert!(state.previous(DocumentId(2)).is_none());
        struct Broken;
        impl TextSource for Broken {
            fn len(&self) -> usize {
                4
            }
            fn chunk(&self, _: usize, _: usize) -> io::Result<Cow<'_, [u8]>> {
                Err(io::Error::other("changed on disk"))
            }
        }
        worker.submit(request(key(1, 2, 2), Arc::new(Broken))).unwrap();
        wait(&events);
        assert!(matches!(worker.take_update(key(1, 2, 2)), Some(Err(Error::Io(_)))));
        assert!(Arc::ptr_eq(state.previous(DocumentId(1)).unwrap(), &previous));
        worker.shutdown().unwrap();
    }

    #[test]
    fn snapshot_chunks_are_borrowed_bounded_and_immutable() {
        let mut buffer = Buffer::empty();
        buffer.insert(0, &vec![b'a'; 128 * 1024]).unwrap();
        let source = SnapshotTextSource::new(buffer.snapshot());
        buffer.insert(0, b"changed").unwrap();
        let chunk = source.chunk(0, usize::MAX).unwrap();
        assert!(matches!(chunk, Cow::Borrowed(_)));
        assert_eq!(chunk.len(), SOURCE_CHUNK_BYTES);
        assert!(chunk.iter().all(|b| *b == b'a'));
        assert_eq!(source.len(), 128 * 1024);
        assert!(source.chunk(source.len(), 1).unwrap().is_empty());
        assert!(source.chunk(source.len() + 1, 1).is_err());
        assert!(source.chunk(0, 0).unwrap().is_empty());
        let mut offset = 0;
        while offset < source.len() {
            let chunk = source.chunk(offset, 1024).unwrap();
            assert!(chunk.len() <= 1024 && !chunk.is_empty());
            offset += chunk.len();
        }
        // Add-storage pieces are already 64 KiB, so they alone cannot expose
        // a missing adapter cap. An original file is one larger shared piece.
        static NEXT_FILE: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "kwil-syntax-chunks-{}-{}.txt",
            std::process::id(),
            NEXT_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, vec![b'a'; 128 * 1024]).unwrap();
        {
            let file = kwil_core_text::TextFile::open(&path).unwrap();
            file.build_index(1).unwrap();
            let original = Buffer::from_file(file).unwrap();
            let source = SnapshotTextSource::new(original.snapshot());
            let chunk = source.chunk(0, usize::MAX).unwrap();
            assert!(matches!(chunk, Cow::Borrowed(_)));
            assert_eq!(chunk.len(), SOURCE_CHUNK_BYTES);
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn result_and_config_caps() {
        let (worker, events) = worker(Config { margin_bytes: 0, ..Config::default() });
        worker.submit(request(key(1, 1, 1), Arc::new(vec![b'('; MAX_RESULT_SPANS + 1]))).unwrap();
        wait(&events);
        assert!(matches!(worker.take_update(key(1, 1, 1)), Some(Err(Error::QueryLimit))));
        worker.shutdown().unwrap();
        assert!(matches!(
            HighlightWorker::new(Config { max_window_bytes: MAX_WINDOW_BYTES + 1, ..Config::default() }, || {}),
            Err(Error::InvalidConfig)
        ));
    }

    struct Gated {
        entered: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
        once: AtomicBool,
    }
    impl TextSource for Gated {
        fn len(&self) -> usize {
            1
        }
        fn chunk(&self, _: usize, _: usize) -> io::Result<Cow<'_, [u8]>> {
            assert_eq!(thread::current().name(), Some("kwil-highlighting"));
            if !self.once.swap(true, Ordering::Relaxed) {
                self.entered.send(()).unwrap();
                self.release.lock().unwrap().recv_timeout(Duration::from_secs(5)).unwrap();
            }
            Ok(Cow::Borrowed(b"("))
        }
    }
    struct Dropped {
        observed: mpsc::Sender<(String, bool)>,
        shared: std::sync::Weak<Shared>,
    }
    impl TextSource for Dropped {
        fn len(&self) -> usize {
            1
        }
        fn chunk(&self, _: usize, _: usize) -> io::Result<Cow<'_, [u8]>> {
            Ok(Cow::Borrowed(b"("))
        }
    }
    impl Drop for Dropped {
        fn drop(&mut self) {
            let unlocked = self.shared.upgrade().is_none_or(|s| s.state.try_lock().is_ok());
            let _ = self.observed.send((thread::current().name().unwrap_or("").into(), unlocked));
        }
    }

    #[test]
    fn latest_bounded_queue_retires_on_worker() {
        let (worker, events) = worker(Config::default());
        let (entered, waiting) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        worker
            .submit(request(
                key(1, 1, 1),
                Arc::new(Gated { entered, release: Mutex::new(gate), once: AtomicBool::new(false) }),
            ))
            .unwrap();
        wait(&waiting);
        let (observed, drops) = mpsc::channel();
        for n in 2..=10 {
            worker
                .submit(request(
                    key(1, n, n),
                    Arc::new(Dropped { observed: observed.clone(), shared: Arc::downgrade(&worker.shared) }),
                ))
                .unwrap();
        }
        let rejected = worker
            .submit(request(key(1, 11, 11), Arc::new(Dropped { observed, shared: Arc::downgrade(&worker.shared) })))
            .unwrap_err();
        assert!(matches!(rejected.error, Error::Busy));
        assert!(drops.try_recv().is_err(), "UI submit destroyed a snapshot");
        release.send(()).unwrap();
        wait(&events);
        let update = worker.take_update(key(1, 10, 10)).unwrap().unwrap();
        assert_eq!(update.key, key(1, 10, 10));
        assert!(events.try_recv().is_err(), "stale work published");
        worker.shutdown().unwrap();
        for _ in 0..9 {
            let (name, unlocked) = drops.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(name, "kwil-highlighting");
            assert!(unlocked);
        }
        assert_eq!(rejected.request.source.len(), 1, "rejection returns ownership");
    }

    #[test]
    fn cancel_and_drop_retire_pending_on_worker() {
        for stop in [false, true] {
            let (worker, events) = worker(Config::default());
            let (entered, waiting) = mpsc::channel();
            let (release, gate) = mpsc::channel();
            worker
                .submit(request(
                    key(1, 1, 1),
                    Arc::new(Gated { entered, release: Mutex::new(gate), once: AtomicBool::new(false) }),
                ))
                .unwrap();
            wait(&waiting);
            let (observed, drops) = mpsc::channel();
            worker
                .submit(request(key(1, 2, 2), Arc::new(Dropped { observed, shared: Arc::downgrade(&worker.shared) })))
                .unwrap();
            if stop {
                drop(worker);
                release.send(()).unwrap();
            } else {
                worker.cancel();
                release.send(()).unwrap();
                worker.shutdown().unwrap();
            }
            let (name, unlocked) = drops.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(name, "kwil-highlighting");
            assert!(unlocked);
            // A disconnected wake channel is also fine after detached shutdown.
            assert!(!matches!(events.try_recv(), Ok(())));
        }
    }
}
