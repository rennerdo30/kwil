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
use crate::editor::receipts::{EditKind, EditReceipt, SyntaxEdit};
use kwil_core_runtime::{NativeExitObservation, NativeExitState, NativeObservationError};
use kwil_core_syntax::WorkerSettlementReason;
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
        Arc, Condvar, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::Instant,
};

pub const SOURCE_CHUNK_BYTES: usize = 64 * 1024;
pub const MAX_WINDOW_BYTES: usize = 256 * 1024;
pub const MAX_RESULT_SPANS: usize = 64 * 1024;
const RETIRE_CAPACITY: usize = 8;

/// O(1) snapshot ownership, zero document copies. Only `chunk` accesses storage,
/// on the worker; it borrows the first piece intersecting the requested range.
pub struct SnapshotTextSource {
    snapshot: Snapshot,
    sequence: u64,
    revision: u64,
    edit: Option<SyntaxEdit>,
}
impl SnapshotTextSource {
    pub fn new(snapshot: Snapshot) -> Self {
        Self { snapshot, sequence: 0, revision: 0, edit: None }
    }
    pub(crate) fn from_editor(editor: &crate::Editor) -> Self {
        let (sequence, edit) = editor.syntax_edit();
        Self { snapshot: editor.document().snapshot(), sequence, revision: editor.document().revision(), edit }
    }
}

/// Convert serialized replacements without replaying bytes or materializing an
/// intermediate document. Sparse positions count LF rows and BYTE columns.
/// Undo visits ascending intermediate offsets: map them back to the old source,
/// then forward through the last restored endpoint (including cross-row edits).
fn input_edits(
    receipt: &EditReceipt,
    cancelled: &dyn Fn() -> bool,
    emit: &mut dyn FnMut(InputEdit),
) -> Result<(), Error> {
    use kwil_core_syntax::Point;
    fn point(snapshot: &Snapshot, offset: u64) -> io::Result<Point> {
        let (row, column) = snapshot
            .offset_to_line_col(offset)
            .map_err(io::Error::other)?
            .ok_or_else(|| io::Error::other("syntax offset outside snapshot"))?;
        Ok(Point::new(
            usize::try_from(row).map_err(io::Error::other)?,
            usize::try_from(column).map_err(io::Error::other)?,
        ))
    }
    fn tail(point: Point, old: Point, new: Point) -> Point {
        Point::new(
            new.row + point.row - old.row,
            if point.row == old.row { new.column + point.column - old.column } else { point.column },
        )
    }
    let inverse = receipt.kind == EditKind::Undo;
    let mut shift = 0_i128;
    let mut old_anchor = Point::new(0, 0);
    let mut new_anchor = Point::new(0, 0);
    let result = receipt.for_each_replacement(&mut |replacement| {
        if cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "syntax cancelled"));
        }
        let base = |offset: u64| -> io::Result<u64> {
            u64::try_from(offset as i128 - if inverse { shift } else { 0 }).map_err(io::Error::other)
        };
        let start = point(&receipt.before, base(replacement.range.start)?)?;
        let end = point(&receipt.before, base(replacement.range.end)?)?;
        let start_position = if inverse { tail(start, old_anchor, new_anchor) } else { start };
        let old_end_position = if inverse { tail(end, old_anchor, new_anchor) } else { end };
        let inserted_start = point(&receipt.after, replacement.insert.range.start)?;
        let inserted_end = point(&receipt.after, replacement.insert.range.end)?;
        let new_end_position = tail(inserted_end, inserted_start, start_position);
        emit(InputEdit {
            start_byte: usize::try_from(replacement.range.start).map_err(io::Error::other)?,
            old_end_byte: usize::try_from(replacement.range.end).map_err(io::Error::other)?,
            new_end_byte: usize::try_from(replacement.range.start + replacement.insert.len())
                .map_err(io::Error::other)?,
            start_position,
            old_end_position,
            new_end_position,
        });
        shift += replacement.insert.len() as i128 - (replacement.range.end - replacement.range.start) as i128;
        old_anchor = end;
        new_anchor = new_end_position;
        Ok(())
    });
    if cancelled() { Err(Error::Cancelled) } else { result.map_err(Error::Io) }
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
    /// Optional exact transaction metadata. Must share the same Arc as `source`,
    /// so rejected frontend handles retire with that source, never on the UI.
    pub snapshot: Option<Arc<SnapshotTextSource>>,
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
    // None preserves the configured document parser and 4096-byte default.
    // Some admits only a bounded fragment plus this explicit allowance.
    context_margin: Option<usize>,
}

// No strong source owner: viewport changes reuse the immutable source, while
// the weak allocation identity prevents address reuse from matching old data.
struct DetectionCache {
    source: Weak<dyn TextSource>,
    document: DocumentId,
    revision: u64,
    path: Arc<PathBuf>,
    mappings: Arc<[LanguageMapping]>,
    choice: LanguageChoice,
    detection: Detection,
}
impl DetectionCache {
    fn matches(&self, request: &HighlightRequest) -> bool {
        self.source.ptr_eq(&Arc::downgrade(&request.source))
            && self.document == request.key.document_id
            && self.revision == request.key.revision
            && self.path == request.path
            && self.mappings == request.mappings
            && self.choice == request.choice
    }
}
struct State {
    pending: Option<Job>,
    retired: [Option<Job>; RETIRE_CAPACITY],
    retired_sources: [Option<Arc<dyn TextSource>>; RETIRE_CAPACITY],
    result: Option<(u64, HighlightKey, Result<Arc<PublishedHighlights>, Error>)>,
    stopped: bool,
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

/// One active + one latest pending job, eight retirement slots and one result.
/// At most eighteen accepted job handles including a draining retire batch,
/// plus sixteen standalone source handles across queued/draining retirement and
/// one worker-owned prior snapshot. Transaction recipes share these lifetimes.
pub struct HighlightWorker {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    panic: Option<Box<dyn std::any::Any + Send + 'static>>,
    native_retired: bool,
    settled: bool,
}
/// Failure owns the original facade; Drop never joins or performs native I/O.
pub struct HighlightShutdownError {
    owner: Mutex<Option<HighlightWorker>>,
    reason: WorkerSettlementReason,
}
impl HighlightShutdownError {
    pub fn reason(&self) -> WorkerSettlementReason {
        self.reason
    }
    pub fn into_worker(self) -> HighlightWorker {
        self.owner.into_inner().unwrap_or_else(|error| error.into_inner()).expect("original shutdown owner")
    }
}
impl std::fmt::Display for HighlightShutdownError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(out, "worker shutdown {:?}; exact owner retained", self.reason)
    }
}
impl std::fmt::Debug for HighlightShutdownError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, out)
    }
}
impl std::error::Error for HighlightShutdownError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.reason {
            WorkerSettlementReason::NativeObservation(error) => Some(error),
            _ => None,
        }
    }
}

impl HighlightWorker {
    /// Declared fixed facade/shared headers, aligned Arc allocation and
    /// completion/typed-return overlap. Host session/source/engine payloads have
    /// their existing separate caps. Reserve before constructing the worker.
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
        let bytes = metadata_add(bytes, std::mem::size_of::<HighlightShutdownError>());
        let bytes = metadata_add(bytes, std::mem::size_of::<Result<(), HighlightShutdownError>>());
        match NativeExitObservation::operation_stack_bytes() {
            Some(scratch) => metadata_add(metadata_add(bytes, scratch), std::mem::size_of::<&Instant>()),
            None => None,
        }
    }
    /// Exact fixed facade + aligned Arc<Shared> growth for the existing host
    /// metadata admission BEFORE construction. Completion and panic retention
    /// add inline fields only; the thread's original panic Box is never copied.
    pub const fn settlement_metadata_bytes() -> usize {
        #[allow(dead_code)]
        struct BeforeWorker {
            shared: Arc<Shared>,
            thread: Option<JoinHandle<()>>,
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
        Self::checked_retained_metadata_bytes().ok_or(Error::InvalidConfig)?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                pending: None,
                retired: [const { None }; RETIRE_CAPACITY],
                retired_sources: [const { None }; RETIRE_CAPACITY],
                result: None,
                stopped: false,
            }),
            ready: Condvar::new(),
            generation: AtomicU64::new(0),
            completed: AtomicBool::new(false),
            native_exit: NativeExitObservation::new(),
        });
        let worker = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("kwil-highlighting".into())
            .spawn(move || {
                let _completion = Completion(&worker);
                if worker.native_exit.register_current().is_err() {
                    worker.state.lock().unwrap_or_else(|error| error.into_inner()).stopped = true;
                }
                let mut document = None;
                let mut settings: Option<Arc<[LanguageMapping]>> = None;
                let mut detector = None;
                let mut previous: Option<Arc<SnapshotTextSource>> = None;
                let mut detection_cache: Option<DetectionCache> = None;
                loop {
                    let (job, retired, sources, stopped) = {
                        let mut state = worker.state.lock().unwrap();
                        while state.pending.is_none()
                            && state.retired_sources.iter().all(Option::is_none)
                            && !state.stopped
                        {
                            state = worker.ready.wait(state).unwrap();
                        }
                        (
                            state.pending.take(),
                            std::mem::replace(&mut state.retired, [const { None }; RETIRE_CAPACITY]),
                            std::mem::replace(&mut state.retired_sources, [const { None }; RETIRE_CAPACITY]),
                            state.stopped,
                        )
                    };
                    drop(retired);
                    let drained = sources.iter().any(Option::is_some);
                    drop(sources);
                    if drained {
                        wake();
                    }
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
                        if document != Some(key.document_id)
                            || detection_cache.as_ref().is_some_and(|cached| {
                                cached.revision == key.revision
                                    && !cached.source.ptr_eq(&Arc::downgrade(&request.source))
                            })
                        {
                            engine.reset_document();
                            previous = None;
                            document = Some(key.document_id);
                        }
                        if settings.as_ref().is_none_or(|s| !Arc::ptr_eq(s, &request.mappings)) {
                            let compiled = Detector::new(&request.mappings)?;
                            detector = Some(compiled);
                            settings = Some(Arc::clone(&request.mappings));
                        }
                        let detection = if let Some(cached) = &detection_cache
                            && cached.matches(request)
                        {
                            cached.detection
                        } else {
                            let detection = detector.as_ref().unwrap().detect(
                                &request.path,
                                request.choice.language(),
                                request.source.as_ref(),
                                &cancelled,
                            )?;
                            detection_cache = Some(DetectionCache {
                                source: Arc::downgrade(&request.source),
                                document: key.document_id,
                                revision: key.revision,
                                path: request.path.clone(),
                                mappings: request.mappings.clone(),
                                choice: request.choice,
                                detection,
                            });
                            detection
                        };
                        if job.context_margin.is_none()
                            && let Some(snapshot) = &request.snapshot
                        {
                            match (&previous, &snapshot.edit) {
                                (Some(old), _) if old.sequence == snapshot.sequence && old.revision == key.revision => {
                                }
                                (Some(old), Some(edit))
                                    if old.sequence.checked_add(1) == Some(snapshot.sequence)
                                        && old.revision == edit.before_revision
                                        && key.revision == edit.after_revision =>
                                {
                                    engine.stage_edits(
                                        detection.language,
                                        edit.before_revision,
                                        edit.after_revision,
                                        |emit| {
                                            input_edits(
                                                &edit.receipt(old.snapshot.clone(), snapshot.snapshot.clone()),
                                                &cancelled,
                                                emit,
                                            )
                                        },
                                    )?;
                                }
                                _ => engine.reset_document(),
                            }
                        }
                        let highlights = if let Some(margin) = job.context_margin {
                            engine.highlight_window(
                                request.source.as_ref(),
                                detection.language,
                                key.revision,
                                request.visible.clone(),
                                margin,
                                &cancelled,
                            )?
                        } else {
                            engine.highlight(
                                request.source.as_ref(),
                                detection.language,
                                key.revision,
                                request.previous_revision,
                                request.edit.as_ref(),
                                request.visible.clone(),
                                &cancelled,
                            )?
                        };
                        if highlights.spans.len() > MAX_RESULT_SPANS {
                            return Err(Error::QueryLimit);
                        }
                        Ok(Arc::new(PublishedHighlights { key, detection, highlights }))
                    })();
                    if result.is_ok() && !cancelled() {
                        previous = request.snapshot.clone();
                    } else {
                        engine.reset_document();
                        previous = None;
                    }
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
        Ok(Self { shared, thread: Some(thread), panic: None, native_retired: false, settled: false })
    }

    /// Constant mailbox work, no source access, glob compilation or destruction
    /// of superseded requests. Saturation returns ownership for later retry.
    pub fn submit(&self, request: HighlightRequest) -> Result<u64, Box<HighlightSubmitError>> {
        self.submit_policy(request, None)
    }

    /// Admit a bounded per-request context policy on this SAME worker. This
    /// never changes defaults or permits parsing outside the fragment. On refusal the
    /// caller retains its request and retries with the same explicit policy.
    pub fn submit_with_context_margin(
        &self,
        request: HighlightRequest,
        margin_bytes: usize,
    ) -> Result<u64, Box<HighlightSubmitError>> {
        if margin_bytes > MAX_WINDOW_BYTES / 2
            || request.visible.start > request.visible.end
            || request
                .visible
                .len()
                .checked_add(margin_bytes.saturating_mul(2))
                .is_none_or(|bytes| bytes > MAX_WINDOW_BYTES)
        {
            return Err(Box::new(HighlightSubmitError { request, error: Error::InvalidRange }));
        }
        self.submit_policy(request, Some(margin_bytes))
    }

    fn submit_policy(
        &self,
        request: HighlightRequest,
        context_margin: Option<usize>,
    ) -> Result<u64, Box<HighlightSubmitError>> {
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
        state.pending = Some(Job { generation, request, context_margin });
        self.shared.ready.notify_one();
        Ok(generation)
    }

    pub fn cancel(&self) {
        let _state = self.shared.state.lock().unwrap();
        self.shared.generation.fetch_add(1, Ordering::AcqRel);
        self.shared.ready.notify_one();
    }

    /// Retire frontend-held source handles on this same fixed worker. A full
    /// queue returns ownership for retry on a worker event.
    pub fn retire_source(&self, source: Arc<dyn TextSource>) -> Result<(), Arc<dyn TextSource>> {
        let mut state = self.shared.state.lock().unwrap();
        if state.stopped {
            return Err(source);
        }
        let Some(slot) = state.retired_sources.iter_mut().find(|slot| slot.is_none()) else { return Err(source) };
        *slot = Some(source);
        self.shared.ready.notify_one();
        Ok(())
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

    pub(crate) fn stop(&self) {
        let mut state = self.shared.state.lock().unwrap_or_else(|error| error.into_inner());
        self.shared.generation.fetch_add(1, Ordering::AcqRel);
        state.stopped = true;
        self.shared.ready.notify_one();
    }
    /// Explicit OFF-UI UNBOUNDED compatibility join. Never call from a bounded
    /// shutdown protocol; failure retains the complete original owner.
    pub fn shutdown(mut self) -> Result<(), HighlightShutdownError> {
        self.shutdown_in_place().map_err(|reason| HighlightShutdownError { owner: Mutex::new(Some(self)), reason })
    }
    pub fn shutdown_until(mut self, deadline: Instant) -> Result<(), HighlightShutdownError> {
        self.settle_until(deadline).map_err(|reason| HighlightShutdownError { owner: Mutex::new(Some(self)), reason })
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
impl Drop for HighlightWorker {
    fn drop(&mut self) {
        self.stop();
        if self.thread.is_some() || !self.settled || self.panic.is_some() {
            // Misuse quarantine, NOT successful retirement: retain the SAME
            // shared allocation/source headers/native right and exact handle.
            // No native I/O, source destruction, new allocation or cleanup thread.
            std::mem::forget(Arc::clone(&self.shared));
            std::mem::forget(self.thread.take());
            std::mem::forget(self.panic.take());
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
#[path = "highlighting/highlight_settlement_tests.rs"]
mod highlight_settlement_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use kwil_core_text::Buffer;
    use std::{
        sync::{atomic::AtomicBool, mpsc},
        time::Duration,
    };

    fn assert_exact_points(receipt: &EditReceipt) {
        use kwil_core_syntax::Point;
        fn position(bytes: &[u8], offset: usize) -> Point {
            let prefix = &bytes[..offset];
            Point::new(
                prefix.iter().filter(|&&b| b == b'\n').count(),
                prefix.iter().rposition(|&b| b == b'\n').map_or(offset, |lf| offset - lf - 1),
            )
        }
        let mut edits = Vec::new();
        input_edits(receipt, &|| false, &mut |edit| edits.push(edit)).unwrap();
        let mut bytes = Vec::new();
        receipt.before.write_to(&mut bytes).unwrap();
        let mut index = 0;
        receipt
            .for_each_replacement(&mut |replacement| {
                let edit = &edits[index];
                assert_eq!(edit.start_byte, replacement.range.start as usize);
                assert_eq!(edit.old_end_byte, replacement.range.end as usize);
                assert_eq!(edit.start_position, position(&bytes, edit.start_byte), "start {index} {:?}", receipt.kind);
                assert_eq!(
                    edit.old_end_position,
                    position(&bytes, edit.old_end_byte),
                    "old end {index} {:?}",
                    receipt.kind
                );
                let mut insert = Vec::new();
                replacement.insert.write_to(&mut insert)?;
                bytes.splice(edit.start_byte..edit.old_end_byte, insert);
                assert_eq!(
                    edit.new_end_position,
                    position(&bytes, edit.new_end_byte),
                    "new end {index} {:?}",
                    receipt.kind
                );
                index += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(index, edits.len());
        let mut expected = Vec::new();
        receipt.after.write_to(&mut expected).unwrap();
        assert_eq!(bytes, expected);
    }

    #[test]
    fn exact_transaction_points_cover_utf8_cross_rows_multicursor_undo_redo_and_bulk() {
        std::thread::spawn(|| {
            use crate::{Cursor, Document, Editor};
            let mut document = Document::new();
            document.buffer.insert(0, "α one\nβ two\nγ three\nfn end() {}\n".as_bytes()).unwrap();
            let mut editor = Editor::new(document);
            let mut previous = editor.document().snapshot();
            let mut sequence = 0;
            let mut check = |editor: &Editor| {
                let current = SnapshotTextSource::from_editor(editor);
                assert_eq!(current.sequence, sequence + 1);
                let receipt = current
                    .edit
                    .as_ref()
                    .expect("exact recipe retained")
                    .receipt(previous.clone(), current.snapshot.clone());
                assert_exact_points(&receipt);
                previous = current.snapshot;
                sequence = current.sequence;
                // Highlighting must never install or advance a journal observer.
                assert_eq!(editor.next_edit_sequence(), 1);
            };
            editor
                .set_cursors(
                    vec![Cursor { anchor: 2, head: 9, desired_column: None }, Cursor::caret(16), Cursor::caret(26)],
                    0,
                )
                .unwrap();
            editor.insert_text("界\nx", Duration::ZERO).unwrap();
            check(&editor);
            editor.insert_text("é", Duration::from_millis(20)).unwrap();
            check(&editor);
            editor.undo().unwrap();
            check(&editor);
            editor.redo().unwrap();
            check(&editor);
            editor.select_all().unwrap();
            editor.indent(false).unwrap();
            check(&editor);
            editor.undo().unwrap();
            check(&editor);
            editor.redo().unwrap();
            check(&editor);
            editor.replace_byte_range(0..5, b"\n").unwrap();
            check(&editor);
            editor.undo().unwrap();
            check(&editor);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn exact_trim_spool_points_and_inverse_remain_worker_owned() {
        std::thread::spawn(|| {
            let mut document = crate::Document::new();
            document.buffer.insert(0, "α  \nβ\t\nlast ".as_bytes()).unwrap();
            let mut editor = crate::Editor::new(document);
            editor.settings.editor.trim_trailing_whitespace_on_save = true;
            let path = std::env::temp_dir().join(format!("kwil-syntax-trim-{}.txt", std::process::id()));
            let before = editor.document().snapshot();
            let request = editor.save_request(Some(path.clone())).unwrap();
            editor.finish_save_for_test(kwil_core_workspace::save_snapshot(request)).unwrap();
            let trimmed = SnapshotTextSource::from_editor(&editor);
            let receipt = trimmed.edit.as_ref().unwrap().receipt(before.clone(), trimmed.snapshot.clone());
            assert_eq!(receipt.kind, EditKind::WorkerTrim);
            assert_eq!(receipt.replacement_count(), 3);
            assert_exact_points(&receipt);
            editor.undo().unwrap();
            let undone = SnapshotTextSource::from_editor(&editor);
            assert_exact_points(
                &undone.edit.as_ref().unwrap().receipt(trimmed.snapshot.clone(), undone.snapshot.clone()),
            );
            editor.redo().unwrap();
            let redone = SnapshotTextSource::from_editor(&editor);
            assert_exact_points(
                &redone.edit.as_ref().unwrap().receipt(undone.snapshot.clone(), redone.snapshot.clone()),
            );
            drop((receipt, trimmed, undone, redone, before, editor));
            std::fs::remove_file(path).unwrap();
        })
        .join()
        .unwrap();
    }

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
            snapshot: None,
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
    fn prepared_worker_context_is_per_request_and_opaque_gap_bytes_are_never_read() {
        struct Gapped {
            ui: thread::ThreadId,
            ranges: [Range<usize>; 2],
            reads: Mutex<Vec<Range<usize>>>,
        }
        impl TextSource for Gapped {
            fn len(&self) -> usize {
                1_000_000_100
            }
            fn chunk(&self, offset: usize, max: usize) -> io::Result<Cow<'_, [u8]>> {
                assert_ne!(thread::current().id(), self.ui, "source I/O on the submitter");
                assert_eq!(thread::current().name(), Some("kwil-highlighting"));
                let range = offset..offset + max;
                assert!(
                    self.ranges.iter().any(|allowed| range.start >= allowed.start && range.end <= allowed.end),
                    "opaque/gap read {range:?}"
                );
                self.reads.lock().unwrap().push(range);
                Ok(Cow::Borrowed(&b"42"[..max]))
            }
        }
        let source = Arc::new(Gapped {
            ui: thread::current().id(),
            ranges: [10..12, 1_000_000_000..1_000_000_002],
            reads: Mutex::new(Vec::new()),
        });
        let (worker, events) = worker(Config::default());
        for (index, range) in source.ranges.iter().enumerate() {
            let current = key(1, 1, index as u64 + 1);
            let mut next = request(current, source.clone());
            next.choice = LanguageChoice::Explicit(Language::Rust);
            next.visible = range.clone();
            worker.submit_with_context_margin(next, 0).unwrap();
            wait(&events);
            let result = worker.take_update(current).unwrap().unwrap();
            assert_eq!(result.highlights.requested, *range);
            assert_eq!(result.highlights.queried, *range);
            assert_eq!(result.highlights.mode, Mode::Lexical, "large-source fallback policy is unchanged");
            assert!(result.highlights.spans.iter().any(|span| span.class == Class::Number && span.range == *range));
        }
        let source: Arc<dyn TextSource> = Arc::new(vec![b' '; 20_000]);
        let mut next = request(key(1, 1, 3), source);
        next.visible = 10_000..10_001;
        worker.submit(next).unwrap();
        wait(&events);
        assert_eq!(worker.take_update(key(1, 1, 3)).unwrap().unwrap().highlights.queried, 5_904..14_097);
        worker.shutdown().unwrap();
    }

    #[test]
    fn prepared_detection_cache_tracks_source_and_policy_but_not_viewport() {
        use std::sync::atomic::AtomicUsize;
        struct Counted {
            bytes: Vec<u8>,
            prefixes: AtomicUsize,
        }
        impl TextSource for Counted {
            fn len(&self) -> usize {
                self.bytes.len()
            }
            fn chunk(&self, offset: usize, max: usize) -> io::Result<Cow<'_, [u8]>> {
                if offset == 0 {
                    self.prefixes.fetch_add(1, Ordering::Relaxed);
                }
                assert_eq!(thread::current().name(), Some("kwil-highlighting"));
                Ok(Cow::Borrowed(&self.bytes[offset..offset + max]))
            }
        }
        let mut bytes = vec![b' '; 100_000];
        bytes[..9].copy_from_slice(b"ordinary\n");
        bytes[80_000..80_005].copy_from_slice(b"42 43");
        let source = Arc::new(Counted { bytes, prefixes: AtomicUsize::new(0) });
        let (worker, events) = worker(Config::default());
        for generation in 1..=4 {
            let current = key(1, 1, generation);
            let mut next = request(current, source.clone());
            next.choice = LanguageChoice::Automatic;
            next.path = Arc::new(if generation < 3 { "file.rs".into() } else { "renamed.py".into() });
            next.visible = if generation == 1 { 80_000..80_002 } else { 80_003..80_005 };
            if generation == 4 {
                next.mappings = Arc::from([LanguageMapping { pattern: "py".into(), language: Language::Log }]);
            }
            worker.submit_with_context_margin(next, 0).unwrap();
            wait(&events);
            let result = worker.take_update(current).unwrap().unwrap();
            let expected = match generation {
                1 | 2 => Language::Rust,
                3 => Language::Python,
                _ => Language::Log,
            };
            assert_eq!(result.detection.language, expected);
            assert_eq!(
                source.prefixes.load(Ordering::Relaxed),
                if generation < 3 { 1 } else { 2 },
                "viewport reissue must not redetect"
            );
        }
        // Remove the mapping without replacing the immutable source.
        let mut next = request(key(1, 1, 5), source.clone());
        next.choice = LanguageChoice::Automatic;
        next.path = Arc::new("renamed.py".into());
        next.visible = 80_000..80_002;
        worker.submit_with_context_margin(next, 0).unwrap();
        wait(&events);
        assert_eq!(worker.take_update(key(1, 1, 5)).unwrap().unwrap().detection.language, Language::Python);
        assert_eq!(source.prefixes.load(Ordering::Relaxed), 3);
        // Identical document/revision/path/policy, but a new immutable owner
        // with a Ruby shebang. A source-blind cache would return Python.
        let replacement: Arc<dyn TextSource> = Arc::new(b"#!/usr/bin/ruby\n42".to_vec());
        let mut next = request(key(1, 1, 6), replacement.clone());
        next.choice = LanguageChoice::Automatic;
        next.path = Arc::new("renamed.py".into());
        next.visible = replacement.len() - 2..replacement.len();
        worker.submit_with_context_margin(next, 0).unwrap();
        wait(&events);
        assert_eq!(worker.take_update(key(1, 1, 6)).unwrap().unwrap().detection.language, Language::Ruby);
        let mut next = request(key(1, 1, 7), replacement.clone());
        next.path = Arc::new("renamed.py".into());
        next.visible = replacement.len() - 2..replacement.len();
        worker.submit_with_context_margin(next, 0).unwrap();
        wait(&events);
        assert_eq!(worker.take_update(key(1, 1, 7)).unwrap().unwrap().detection.language, Language::Generic);
        worker.shutdown().unwrap();
    }

    #[test]
    fn prepared_context_refusal_returns_exact_source_without_reading_or_cancelling() {
        let (worker, events) = worker(Config::default());
        let source: Arc<dyn TextSource> = Arc::new(b"42".to_vec());
        let rejected =
            worker.submit_with_context_margin(request(key(1, 1, 1), source.clone()), MAX_WINDOW_BYTES).unwrap_err();
        assert!(matches!(rejected.error, Error::InvalidRange));
        assert!(Arc::ptr_eq(&rejected.request.source, &source));
        worker.submit(request(key(1, 1, 2), source)).unwrap();
        wait(&events);
        assert!(worker.take_update(key(1, 1, 2)).unwrap().is_ok());
        worker.shutdown().unwrap();
    }

    #[test]
    fn idle_source_retirement_runs_on_the_existing_worker_and_wakes_host() {
        struct Retired(mpsc::Sender<Option<String>>);
        impl TextSource for Retired {
            fn len(&self) -> usize {
                0
            }
            fn chunk(&self, _: usize, _: usize) -> io::Result<Cow<'_, [u8]>> {
                Ok(Cow::Borrowed(b""))
            }
        }
        impl Drop for Retired {
            fn drop(&mut self) {
                let _ = self.0.send(std::thread::current().name().map(str::to_owned));
            }
        }
        let (worker, events) = worker(Config::default());
        let (sender, receiver) = mpsc::channel();
        assert!(worker.retire_source(Arc::new(Retired(sender))).is_ok());
        assert_eq!(receiver.recv_timeout(Duration::from_secs(5)).unwrap().as_deref(), Some("kwil-highlighting"));
        wait(&events);
        worker.shutdown().unwrap();
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
