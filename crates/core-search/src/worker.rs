//! Wake-driven search service. All matching, filesystem work and source
//! retirement run on the coordinator (or its bounded file-search workers).
use crate::worker_spool::{SpoolDiagnostic, SpoolEdits, SpoolOwner, SpoolRecipe, decode_file_event, encode_file_event};
use crate::{
    Cancellation, Edit, FileEvent, FileSearch, FileSearchSummary, Match, Options, Pattern, Progress, SearchMode,
    find_in_files, plan_replace, search_default,
};
use kwil_core_text::Snapshot;
use std::{
    collections::VecDeque,
    io,
    ops::{ControlFlow, Range},
    sync::{
        Arc, Condvar, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub const INPUT_BYTES: usize = 64 << 10;
pub const PAGE_HITS: usize = 256;
pub const REPLACE_EDITS: usize = 4096;
pub const REPLACE_BYTES: usize = 8 << 20;
pub const SIGNAL_BINS: usize = 256;
const EVENTS: usize = 4;
const RETIRED: usize = 8;
const FILE_PAGE: usize = 32;

/// Frontends allocate monotonically increasing generations. Document IDs are
/// session IDs, never a tab index or a path-derived identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchKey {
    pub document_id: u64,
    pub revision: u64,
    pub generation: u64,
}

#[derive(Clone, Debug)]
pub struct Query {
    pub expression: String,
    pub literal: bool,
    pub options: Options,
    /// Engine completeness contract: every match must fit this declared bound.
    pub max_match_bytes: usize,
}
impl Query {
    pub fn literal(expression: String) -> Self {
        Self { expression, literal: true, options: Options::default(), max_match_bytes: 64 << 10 }
    }
    fn compile(&self) -> io::Result<Pattern> {
        if self.literal {
            Pattern::literal(self.expression.as_bytes(), self.options)
        } else {
            Pattern::regex(&self.expression, self.options, self.max_match_bytes)
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub struct Page {
    pub first: u64,
    pub len: usize,
}
impl Default for Page {
    fn default() -> Self {
        Self { first: 0, len: PAGE_HITS }
    }
}
#[derive(Clone, Copy, Debug)]
pub enum Navigation {
    Next { from: u64, include_current: bool },
    Previous { from: u64, include_current: bool },
}
pub enum ReplaceTarget {
    All,
    Matches(Vec<Match>),
}
pub struct Replacement {
    pub template: Vec<u8>,
    pub target: ReplaceTarget,
}
pub struct DocumentSearch {
    pub snapshot: Snapshot,
    pub query: Query,
    /// Empty means no selection, NOT the whole document. The host passes an
    /// explicit 0..snapshot.len() for whole-document queries.
    pub ranges: Vec<Range<u64>>,
    pub page: Page,
    pub navigation: Option<Navigation>,
    pub replacement: Option<Replacement>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeedCursor {
    pub anchor: u64,
    pub head: u64,
}
pub struct SeedRequest {
    pub snapshot: Snapshot,
    pub cursors: Vec<SeedCursor>,
    pub primary: usize,
    /// Explicit raw-view interpretation, never inferred from a save encoding.
    /// Typed decoded origins and added UTF-8 do not require this permission.
    pub allow_raw_utf8: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeedResult {
    pub text: String,
    pub range: Range<u64>,
    pub cursors: Vec<SeedCursor>,
    pub primary: usize,
    pub from_selection: bool,
}
pub enum SearchTask {
    Document(DocumentSearch),
    SeedSelectionOrWord(SeedRequest),
    Files(FileSearch),
    FilePage { results: FileResults, page: Page },
}
/// Immutable machine-private result session. Page reads are worker-only.
#[derive(Clone, Debug)]
pub struct FileResults {
    session: SearchKey,
    spool: SpoolRecipe,
}
impl FileResults {
    pub fn session(&self) -> SearchKey {
        self.session
    }
    pub fn len(&self) -> u64 {
        self.spool.len()
    }
    pub fn is_empty(&self) -> bool {
        self.spool.is_empty()
    }
    fn read_page(&self, page: Page, cancel: &Cancellation) -> io::Result<Vec<FileEvent>> {
        let mut rows = Vec::with_capacity(page.len);
        for record in self.spool.edits() {
            if cancel.load(Ordering::Acquire) {
                break;
            }
            let record = record?;
            if record.offset >= page.first {
                rows.push(decode_file_event(&record.new_bytes)?);
                if rows.len() == page.len {
                    break;
                }
            }
        }
        Ok(rows)
    }
}
pub struct SearchRequest {
    pub key: SearchKey,
    pub task: SearchTask,
}
/// Scalar-only identity: upgrading a token never acquires a Shared/source owner.
/// Epoch zero is reserved for "not released"; the first close uses epoch one.
#[derive(Debug)]
struct SourceReleaseIdentity {
    generation: AtomicU64,
    released: AtomicU64,
}
/// Bind to the original worker BEFORE transferring the session's closing inputs.
/// This token retains neither that worker nor its source/spool owners.
#[derive(Debug)]
pub struct SearchSourceReleaseKey {
    identity: Weak<SourceReleaseIdentity>,
    generation: u64,
}
/// Proof of input destruction, independent of spool retirement/thread exit.
/// Only the coordinator's explicit source-free phase can construct this receipt.
#[derive(Debug)]
pub struct SearchInputsReleased {
    identity: Weak<SourceReleaseIdentity>,
    generation: u64,
}
impl SearchInputsReleased {
    /// Metadata only. Old receipt/key pairs are invalid after another closing
    /// owner is admitted, even if the old pair still matches itself.
    pub fn valid_for(&self, key: &SearchSourceReleaseKey) -> bool {
        if self.generation != key.generation || !self.identity.ptr_eq(&key.identity) {
            return false;
        }
        let Some(identity) = self.identity.upgrade() else { return false };
        identity.released.load(Ordering::Acquire) == self.generation
            && identity.generation.load(Ordering::Acquire) == self.generation
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchInputsReleaseError {
    NotStopped,
    TimedOut,
    GenerationChanged,
    WorkerPanicked,
    WorkerStoppedWithoutRelease,
}
/// Refusal preserves BOTH exact owners; no allocation or source destruction is
/// needed to report closing pressure, invalid input or a completed worker.
pub struct SearchCloseError {
    pub internal: Option<SearchRequest>,
    pub external: Option<SearchRequest>,
    pub reason: &'static str,
}
impl std::fmt::Debug for SearchCloseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchCloseError").field("reason", &self.reason).finish_non_exhaustive()
    }
}
#[derive(Default)]
struct SourceReleaseControl {
    // Draining does not reopen these lifetime reservations. An arbitrary or
    // stale SearchKey cannot manufacture another closing slot.
    closing_used: [bool; 2],
    closing_finished: bool,
}
// Arc's counter header and data alignment, including its final allocation pad.
#[repr(C)]
struct SourceReleaseAllocation {
    strong: AtomicUsize,
    weak: AtomicUsize,
    identity: SourceReleaseIdentity,
}
const fn padded(bytes: usize, alignment: usize) -> usize {
    bytes.div_ceil(alignment) * alignment
}
/// Additional fixed metadata charge per search worker, prepaid by the frontend's
/// SAME aggregate budget before construction. Includes the aligned scalar Arc,
/// its Shared header, inline controls rounded to container alignment, key/receipt
/// transfer overlap (two headers each), and full close/wait refusal headers.
/// Request payloads and all existing search/spool accounting remain unchanged.
pub const SEARCH_SOURCE_RELEASE_METADATA_BYTES: usize = std::mem::size_of::<SourceReleaseAllocation>()
    + padded(std::mem::size_of::<Arc<SourceReleaseIdentity>>(), std::mem::align_of::<Shared>())
    + padded(std::mem::size_of::<SourceReleaseControl>(), std::mem::align_of::<State>())
    + 2 * (std::mem::size_of::<SearchSourceReleaseKey>() + std::mem::size_of::<SearchInputsReleased>())
    + std::mem::size_of::<SearchCloseError>()
    + std::mem::size_of::<SearchInputsReleaseError>();
/// Ownership is returned on rejection: never drop a last snapshot on input.
pub struct SubmitError {
    pub request: Box<SearchRequest>,
    pub reason: &'static str,
}
impl std::fmt::Debug for SubmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SubmitError").field(&self.reason).finish()
    }
}

/// Ordered original-snapshot coordinates. The dispatcher rechecks the key and
/// journal admission, applies every edit atomically, and emits one EditReceipt.
/// No source snapshots or filesystem cleanup responsibility cross this seam.
#[derive(Debug)]
pub struct ReplaceRecipe {
    pub key: SearchKey,
    pub edits: ReplacementEdits,
}
#[derive(Clone, Debug)]
pub enum ReplacementEdits {
    Inline(Arc<Vec<Edit>>),
    Spool(SpoolRecipe),
}
impl ReplacementEdits {
    pub fn len(&self) -> u64 {
        match self {
            Self::Inline(edits) => edits.len() as u64,
            Self::Spool(edits) => edits.len(),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn iter(&self) -> ReplacementIter {
        match self {
            Self::Inline(edits) => ReplacementIter::Inline { data: edits.clone(), front: 0, back: edits.len() },
            Self::Spool(edits) => ReplacementIter::Spool(edits.edits()),
        }
    }
}
#[derive(Clone)]
pub enum ReplacementIter {
    Inline { data: Arc<Vec<Edit>>, front: usize, back: usize },
    Spool(SpoolEdits),
}
impl Iterator for ReplacementIter {
    type Item = io::Result<Edit>;
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Spool(edits) => edits.next(),
            Self::Inline { data, front, back } => {
                if *front == *back {
                    return None;
                }
                let edit = &data[*front];
                *front += 1;
                Some(Ok(Edit { offset: edit.offset, len: edit.len, new_bytes: edit.new_bytes.clone() }))
            }
        }
    }
}
impl DoubleEndedIterator for ReplacementIter {
    fn next_back(&mut self) -> Option<Self::Item> {
        match self {
            Self::Spool(edits) => edits.next_back(),
            Self::Inline { data, front, back } => {
                if *front == *back {
                    return None;
                }
                *back -= 1;
                let edit = &data[*back];
                Some(Ok(Edit { offset: edit.offset, len: edit.len, new_bytes: edit.new_bytes.clone() }))
            }
        }
    }
}
#[derive(Debug)]
pub struct DocumentMatches {
    pub first: u64,
    pub hits: Vec<Match>,
    pub count: u64,
    pub scanned: u64,
    pub navigation: Option<Match>,
    pub signal: [u64; SIGNAL_BINS],
    pub replacement: Option<ReplaceRecipe>,
}
#[derive(Debug)]
pub enum SearchUpdate {
    Seed(Option<SeedResult>),
    Progress(Progress),
    Hits {
        first: u64,
        hits: Vec<Match>,
    },
    Document(Box<DocumentMatches>),
    /// Pages stream with backpressure. Hosts keep a bounded visible page, or
    /// explicitly spool on a worker; collecting the full stream is forbidden.
    Files {
        first: u64,
        rows: Vec<FileEvent>,
        results: Option<FileResults>,
    },
    FilesDone(FileSearchSummary),
    FilePageDone,
    Failed(String),
    Cancelled,
}
#[derive(Debug)]
pub struct SearchEvent {
    pub key: SearchKey,
    pub update: SearchUpdate,
}
struct Job {
    request: SearchRequest,
    cancel: Cancellation,
}
struct State {
    pending: Option<Job>,
    retired: [Option<Job>; RETIRED],
    events: VecDeque<SearchEvent>,
    active: Option<Cancellation>,
    closing: [Option<SearchRequest>; 2],
    source_release: SourceReleaseControl,
    live_spools: usize,
    retirement_errors: Vec<SpoolDiagnostic>,
    results_session: Option<SearchKey>,
}
struct Shared {
    state: Mutex<State>,
    source_identity: Arc<SourceReleaseIdentity>,
    space: PublisherWake,
    stopped: AtomicBool,
    retirement_due: AtomicBool,
    coordinator: OnceLock<thread::Thread>,
    retry_wake: AtomicBool,
    completed: AtomicBool,
    panicked: AtomicBool,
    completion: Condvar,
}
struct Completion<'a>(&'a Shared);
impl Drop for Completion<'_> {
    fn drop(&mut self) {
        let _guard = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
        self.0.panicked.store(thread::panicking(), Ordering::Release);
        self.0.completed.store(true, Ordering::Release);
        self.0.completion.notify_all();
    }
}
/// One publisher can block on result backpressure. An unpark token survives
/// notification before sleep, including a retry armed while its mailbox is held.
#[derive(Default)]
struct PublisherWake(OnceLock<thread::Thread>);
impl PublisherWake {
    fn register(&self) {
        let _ = self.0.set(thread::current());
    }
    fn notify_one(&self) {
        if let Some(publisher) = self.0.get() {
            publisher.unpark();
        }
    }
    fn notify_all(&self) {
        self.notify_one();
    }
}
impl Shared {
    fn new() -> Self {
        Self {
            state: Mutex::new(State {
                pending: None,
                retired: [const { None }; RETIRED],
                events: VecDeque::with_capacity(EVENTS),
                active: None,
                closing: [None, None],
                source_release: SourceReleaseControl::default(),
                live_spools: 0,
                retirement_errors: Vec::new(),
                results_session: None,
            }),
            source_identity: Arc::new(SourceReleaseIdentity {
                generation: AtomicU64::new(1),
                released: AtomicU64::new(0),
            }),
            space: PublisherWake::default(),
            stopped: AtomicBool::new(false),
            retirement_due: AtomicBool::new(false),
            coordinator: OnceLock::new(),
            retry_wake: AtomicBool::new(false),
            completed: AtomicBool::new(false),
            panicked: AtomicBool::new(false),
            completion: Condvar::new(),
        }
    }
    fn notify(&self) {
        self.space.notify_one();
        if let Some(thread) = self.coordinator.get() {
            thread.unpark();
        }
    }
}
pub struct SearchWorker {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}
/// A failed bounded shutdown retains the exact facade and its retirement
/// controls. Error transport is Send + Sync and never joins from Drop.
pub struct ShutdownError {
    owner: Mutex<Option<SearchWorker>>,
    panicked: bool,
}
impl ShutdownError {
    pub fn into_worker(self) -> SearchWorker {
        self.owner.into_inner().unwrap_or_else(|error| error.into_inner()).expect("shutdown owner")
    }
    pub fn retry_retirement(&self) {
        if let Some(worker) = self.owner.lock().unwrap_or_else(|error| error.into_inner()).as_ref() {
            worker.retry_retirement();
        }
    }
    pub fn retry(self, timeout: Duration) -> Result<(), Self> {
        let worker = self.into_worker();
        worker.retry_retirement();
        worker.shutdown_bounded(timeout)
    }
    pub fn retirement_status(&self) -> Option<(usize, Vec<SpoolDiagnostic>)> {
        self.owner.lock().unwrap_or_else(|error| error.into_inner()).as_ref()?.retirement_status()
    }
}
impl std::fmt::Display for ShutdownError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(if self.panicked {
            "Search worker panicked; exact shutdown owner retained"
        } else {
            "Search shutdown deadline reached; exact cleanup owner retained for explicit retry"
        })
    }
}
impl std::fmt::Debug for ShutdownError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, out)
    }
}
impl std::error::Error for ShutdownError {}
impl SearchWorker {
    /// Starts one sleeping coordinator. Does no source read or filesystem I/O.
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> io::Result<Self> {
        Self::with_spool_limit(1 << 30, wake)
    }
    /// Aggregate private spool disk limit, independent of file-size/heap size.
    pub fn with_spool_limit(spool_bytes: u64, wake: impl Fn() + Send + Sync + 'static) -> io::Result<Self> {
        if spool_bytes < 32 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "spool cap too small"));
        }
        let shared = Arc::new(Shared::new());
        let worker = shared.clone();
        let thread = thread::Builder::new().name("kwil-search".into()).spawn(move || {
            // Declared first, acknowledged only after jobs, spools and their
            // cleanup owners have left the coordinator scope, including unwind.
            let _completion = Completion(&worker);
            let _ = worker.coordinator.set(thread::current());
            let retire_wake = Arc::downgrade(&worker);
            let mut spools = SpoolOwner::with_wake(spool_bytes, move || {
                if let Some(shared) = retire_wake.upgrade() {
                    shared.retirement_due.store(true, Ordering::Release);
                    shared.notify();
                }
            })
            .unwrap();
            loop {
                if worker.retry_wake.swap(false, Ordering::AcqRel) {
                    wake();
                }
                worker.retirement_due.store(false, Ordering::Release);
                let _ = spools.retire();
                {
                    let mut state = worker.state.lock().unwrap();
                    state.live_spools = spools.live();
                    state.retirement_errors = spools.diagnostics().to_vec();
                }
                let (job, retired, closing, stopped, source_generation) = {
                    let mut state = worker.state.lock().unwrap();
                    if state.pending.is_none()
                        && state.retired.iter().all(Option::is_none)
                        && state.closing.iter().all(Option::is_none)
                        && !worker.stopped.load(Ordering::Acquire)
                    {
                        drop(state);
                        thread::park();
                        continue;
                    }
                    let stopped = worker.stopped.load(Ordering::Acquire);
                    if stopped {
                        state.events.clear();
                    }
                    let job = state.pending.take();
                    state.active = job.as_ref().map(|job| job.cancel.clone());
                    (
                        job,
                        std::mem::replace(&mut state.retired, [const { None }; RETIRED]),
                        std::mem::take(&mut state.closing),
                        stopped,
                        worker.source_identity.generation.load(Ordering::Acquire),
                    )
                };
                let retired_any = retired.iter().any(Option::is_some);
                drop(retired);
                drop(closing);
                if stopped {
                    drop(job);
                    // All local input owners and queued events have now been
                    // destroyed OFF UI. A later close may have installed a new
                    // owner while these drops ran: never acknowledge that epoch.
                    {
                        let mut state = worker.state.lock().unwrap();
                        state.active = None;
                        if worker.source_identity.generation.load(Ordering::Acquire) != source_generation
                            || state.pending.is_some()
                            || state.retired.iter().any(Option::is_some)
                            || state.closing.iter().any(Option::is_some)
                            || !state.events.is_empty()
                        {
                            continue;
                        }
                        worker.source_identity.released.store(source_generation, Ordering::Release);
                        worker.completion.notify_all();
                    }
                    // Source-independent recipe pins may keep this worker
                    // parked until Document finishes its journals. They cannot
                    // delay the already published INPUTS RELEASED barrier.
                    let _ = spools.retire();
                    let finished = {
                        let mut state = worker.state.lock().unwrap();
                        state.live_spools = spools.live();
                        state.retirement_errors = spools.diagnostics().to_vec();
                        if worker.source_identity.generation.load(Ordering::Acquire) != source_generation {
                            continue;
                        }
                        let finished = spools.live() == 0;
                        // Fence transfers under the same lock that admits them,
                        // BEFORE leaving the coordinator's last iteration.
                        state.source_release.closing_finished = finished;
                        finished
                    };
                    wake();
                    if finished {
                        break;
                    }
                    // Host wake code can itself consume this thread's park
                    // token (for example while waiting on a channel). A close
                    // admitted during wake() therefore cannot rely on unpark
                    // alone. Recheck its durable epoch/owners AFTER the callback
                    // and immediately before parking, with no callback between.
                    {
                        let state = worker.state.lock().unwrap();
                        if worker.source_identity.generation.load(Ordering::Acquire) != source_generation
                            || state.pending.is_some()
                            || state.retired.iter().any(Option::is_some)
                            || state.closing.iter().any(Option::is_some)
                            || !state.events.is_empty()
                            || worker.retirement_due.load(Ordering::Acquire)
                            || worker.retry_wake.load(Ordering::Acquire)
                        {
                            continue;
                        }
                    }
                    // Pins wake us on release. Failed cleanup stays owned here;
                    // retry_retirement explicitly retries without an idle timer.
                    thread::park();
                    continue;
                }
                if retired_any || job.is_some() {
                    wake();
                }
                let Some(job) = job else { continue };
                let key = job.request.key;
                let emit = |update| publish(&worker, &job.cancel, SearchEvent { key, update }, &wake);
                let result = if job.cancel.load(Ordering::Acquire) {
                    Ok(())
                } else {
                    match &job.request.task {
                        SearchTask::Document(request) => run_document(key, request, &job.cancel, &emit, &mut spools),
                        SearchTask::SeedSelectionOrWord(request) => run_seed(request, &job.cancel).map(|seed| {
                            emit(SearchUpdate::Seed(seed));
                        }),
                        SearchTask::Files(request) => run_files(key, request, &job.cancel, &emit, &mut spools),
                        SearchTask::FilePage { results, page } => results.read_page(*page, &job.cancel).map(|rows| {
                            emit(SearchUpdate::Files { first: page.first, rows, results: Some(results.clone()) });
                            emit(SearchUpdate::FilePageDone);
                        }),
                    }
                };
                if !job.cancel.load(Ordering::Acquire)
                    && let Err(error) = result
                {
                    emit(SearchUpdate::Failed(error.to_string()));
                }
                drop(job); // last Snapshot drops here, never in take_event/submit.
                worker.state.lock().unwrap().active = None;
                wake();
            }
        })?;
        Ok(Self { shared, thread: Some(thread) })
    }
    /// Nonblocking, one active plus one latest queued request. Superseded
    /// pending requests go to bounded worker retirement before replacement.
    pub fn submit(&self, request: SearchRequest) -> Result<(), SubmitError> {
        if let Err(reason) = validate(&request) {
            return Err(SubmitError { request: Box::new(request), reason });
        }
        let Ok(mut state) = self.shared.state.try_lock() else {
            self.shared.retry_wake.store(true, Ordering::Release);
            self.shared.notify();
            return Err(SubmitError { request: Box::new(request), reason: "search mailbox busy; retry on wake" });
        };
        if self.shared.stopped.load(Ordering::Acquire) {
            return Err(SubmitError { request: Box::new(request), reason: "search worker stopped" });
        }
        if let SearchTask::FilePage { results, .. } = &request.task
            && state.results_session != Some(results.session)
        {
            return Err(SubmitError { request: Box::new(request), reason: "stale file result session" });
        }
        if state.pending.is_some() {
            let Some(slot) = state.retired.iter().position(Option::is_none) else {
                return Err(SubmitError {
                    request: Box::new(request),
                    reason: "search retirement full; retry on wake",
                });
            };
            let old = state.pending.take().unwrap();
            old.cancel.store(true, Ordering::Release);
            state.retired[slot] = Some(old);
        }
        if let Some(cancel) = &state.active {
            cancel.store(true, Ordering::Release);
        }
        // Spool lease Drop only signals the coordinator. No source or file
        // cleanup occurs while the input thread holds this metadata lock.
        state.events.clear();
        match &request.task {
            SearchTask::Files(_) => state.results_session = Some(request.key),
            SearchTask::Document(_) | SearchTask::SeedSelectionOrWord(_) => state.results_session = None,
            SearchTask::FilePage { .. } => {}
        }
        state.pending = Some(Job { request, cancel: Arc::new(AtomicBool::new(false)) });
        drop(state);
        self.shared.space.notify_all();
        self.shared.notify();
        Ok(())
    }
    pub fn cancel(&self) {
        let mut state = self.shared.state.lock().unwrap();
        state.results_session = None;
        if let Some(cancel) = &state.active {
            cancel.store(true, Ordering::Release);
        }
        if let Some(job) = &state.pending {
            job.cancel.store(true, Ordering::Release);
        }
        state.events.clear();
        drop(state);
        self.shared.space.notify_all();
        self.shared.notify();
    }
    pub fn take_event(&self) -> Option<SearchEvent> {
        let Ok(mut state) = self.shared.state.try_lock() else {
            self.shared.retry_wake.store(true, Ordering::Release);
            self.shared.notify();
            return None;
        };
        let event = state.events.pop_front();
        drop(state);
        self.shared.space.notify_one();
        event
    }
    /// Return an unsubmitted source to worker-owned destruction. On pressure
    /// the caller keeps ownership and retries after the retirement wake.
    pub fn retire(&self, request: SearchRequest) -> Result<(), Box<SearchRequest>> {
        let Ok(mut state) = self.shared.state.try_lock() else {
            self.shared.retry_wake.store(true, Ordering::Release);
            self.shared.notify();
            return Err(Box::new(request));
        };
        if self.shared.stopped.load(Ordering::Acquire) {
            return Err(Box::new(request));
        }
        let Some(slot) = state.retired.iter().position(Option::is_none) else { return Err(Box::new(request)) };
        state.retired[slot] = Some(Job { request, cancel: Arc::new(AtomicBool::new(true)) });
        drop(state);
        self.shared.notify();
        Ok(())
    }
    /// Nonblocking close; coordinator retires its own sources. For orderly
    /// app shutdown use shutdown on an app-owned shutdown worker and join it.
    pub fn stop(&self) {
        // Empty refusal has no source owner, including after completion/poison.
        let _ = self.close_with(None);
    }
    /// Reserved shutdown slot for the host's one rejected request. Call once
    /// while the worker is live; shutdown never disposes that source on input.
    pub fn close_with(&self, rejected: Option<SearchRequest>) -> ControlFlow<SearchCloseError, ()> {
        self.close_with_requests(rejected, None)
    }
    /// The host's internal retry and ONE panel-owned request have independent
    /// reserved slots. No rejected source is destroyed on the input thread.
    /// Break returns both exact owners; Continue confirms their admission.
    pub fn close_with_requests(
        &self,
        internal: Option<SearchRequest>,
        external: Option<SearchRequest>,
    ) -> ControlFlow<SearchCloseError, ()> {
        // Preparation borrows both owners; every refusal precedes mutation.
        let invalid = internal
            .as_ref()
            .and_then(|request| validate(request).err())
            .or_else(|| external.as_ref().and_then(|request| validate(request).err()));
        if let Some(reason) = invalid {
            return ControlFlow::Break(SearchCloseError { internal, external, reason });
        }
        let Ok(mut state) = self.shared.state.lock() else {
            return ControlFlow::Break(SearchCloseError { internal, external, reason: "search mailbox poisoned" });
        };
        let has_inputs = internal.is_some() || external.is_some();
        if has_inputs {
            if state.source_release.closing_finished || self.shared.completed.load(Ordering::Acquire) {
                return ControlFlow::Break(SearchCloseError {
                    internal,
                    external,
                    reason: "search input retirement finished",
                });
            }
            if state
                .source_release
                .closing_used
                .iter()
                .zip(state.closing.iter())
                .zip([&internal, &external])
                .any(|((used, slot), input)| (*used || slot.is_some()) && input.is_some())
            {
                return ControlFlow::Break(SearchCloseError {
                    internal,
                    external,
                    reason: "duplicate search close request",
                });
            }
            if self.shared.stopped.load(Ordering::Acquire) {
                let Some(next) = self.shared.source_identity.generation.load(Ordering::Acquire).checked_add(1) else {
                    return ControlFlow::Break(SearchCloseError {
                        internal,
                        external,
                        reason: "search source generation exhausted",
                    });
                };
                // Invalidate proof BEFORE moving even one new source owner.
                self.shared.source_identity.released.store(0, Ordering::Release);
                self.shared.source_identity.generation.store(next, Ordering::Release);
                self.shared.completion.notify_all();
            }
        }
        for (index, request) in [internal, external].into_iter().enumerate() {
            if request.is_some() {
                state.source_release.closing_used[index] = true;
                state.closing[index] = request;
            }
        }
        self.shared.stopped.store(true, Ordering::Release);
        if let Some(cancel) = &state.active {
            cancel.store(true, Ordering::Release);
        }
        if let Some(job) = &state.pending {
            job.cancel.store(true, Ordering::Release);
        }
        drop(state);
        self.shared.space.notify_all();
        self.shared.notify();
        ControlFlow::Continue(())
    }
    /// Owner-free identity capture; bind this key before the first stop/close.
    pub fn source_release_key(&self) -> SearchSourceReleaseKey {
        SearchSourceReleaseKey {
            identity: Arc::downgrade(&self.shared.source_identity),
            generation: self.shared.source_identity.generation.load(Ordering::Acquire),
        }
    }
    /// OFF UI. One bounded event wait on the existing completion condition;
    /// never a polling loop, source read, snapshot, full shutdown or OS join.
    /// Pass only the time remaining on the bundle's original absolute deadline.
    pub fn wait_inputs_released(&self, timeout: Duration) -> Result<SearchInputsReleased, SearchInputsReleaseError> {
        let started = Instant::now();
        let shared = &self.shared;
        let generation = shared.source_identity.generation.load(Ordering::Acquire);
        let state = shared.state.lock().unwrap_or_else(|error| error.into_inner());
        if !shared.stopped.load(Ordering::Acquire) {
            return Err(SearchInputsReleaseError::NotStopped);
        }
        let (state, _) = shared
            .completion
            .wait_timeout_while(state, timeout.saturating_sub(started.elapsed()), |_| {
                shared.source_identity.released.load(Ordering::Acquire) != generation
                    && shared.source_identity.generation.load(Ordering::Acquire) == generation
                    && !shared.completed.load(Ordering::Acquire)
            })
            .unwrap_or_else(|error| error.into_inner());
        let result = if shared.source_identity.generation.load(Ordering::Acquire) != generation {
            Err(SearchInputsReleaseError::GenerationChanged)
        } else if shared.source_identity.released.load(Ordering::Acquire) == generation {
            Ok(SearchInputsReleased { identity: Arc::downgrade(&shared.source_identity), generation })
        } else if shared.panicked.load(Ordering::Acquire) {
            Err(SearchInputsReleaseError::WorkerPanicked)
        } else if shared.completed.load(Ordering::Acquire) {
            Err(SearchInputsReleaseError::WorkerStoppedWithoutRelease)
        } else {
            Err(SearchInputsReleaseError::TimedOut)
        };
        drop(state);
        result
    }
    pub fn retry_retirement(&self) {
        self.shared.retirement_due.store(true, Ordering::Release);
        self.shared.notify();
    }
    /// Bounded cached diagnostics; no filesystem access. Cleanup failures remain
    /// worker-owned, including after stop until a successful explicit retry.
    pub fn retirement_status(&self) -> Option<(usize, Vec<SpoolDiagnostic>)> {
        let state = self.shared.state.try_lock().ok()?;
        Some((state.live_spools, state.retirement_errors.clone()))
    }
    pub fn shutdown(mut self) -> thread::Result<()> {
        self.stop();
        self.thread.take().unwrap().join()
    }
    /// OFF UI: waits once for resource cleanup acknowledgement, never joins a
    /// coordinator parked on failed retirement. Timeout returns exact ownership.
    /// Successful acknowledgement leaves no accepted work or spool owner; an
    /// unfinished OS thread handle can only be completing its epilogue.
    pub fn shutdown_bounded(mut self, timeout: Duration) -> Result<(), ShutdownError> {
        self.stop();
        let shared = self.shared.clone();
        let state = shared.state.lock().unwrap_or_else(|error| error.into_inner());
        let (state, _) = shared
            .completion
            .wait_timeout_while(state, timeout, |_| !shared.completed.load(Ordering::Acquire))
            .unwrap_or_else(|error| error.into_inner());
        let completed = shared.completed.load(Ordering::Acquire);
        drop(state);
        if !completed {
            return Err(ShutdownError { owner: Mutex::new(Some(self)), panicked: false });
        }
        if shared.panicked.load(Ordering::Acquire) {
            return Err(ShutdownError { owner: Mutex::new(Some(self)), panicked: true });
        }
        // Acknowledged resource scope is finished. Do not introduce an
        // unbounded OS-thread join for its remaining runtime epilogue.
        drop(self.thread.take());
        Ok(())
    }
}
impl Drop for SearchWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

fn validate(request: &SearchRequest) -> Result<(), &'static str> {
    match &request.task {
        SearchTask::SeedSelectionOrWord(seed) => {
            if seed.cursors.is_empty() || seed.cursors.len() > REPLACE_EDITS || seed.primary >= seed.cursors.len() {
                return Err("seed cursor metadata exceeds cap or has no primary");
            }
            if seed
                .cursors
                .iter()
                .any(|cursor| cursor.anchor > seed.snapshot.len() || cursor.head > seed.snapshot.len())
            {
                return Err("seed cursor outside snapshot");
            }
        }
        SearchTask::Document(doc) => {
            if doc.query.expression.len() > INPUT_BYTES
                || doc.page.len == 0
                || doc.page.len > PAGE_HITS
                || doc.ranges.len() > REPLACE_EDITS
            {
                return Err("search request exceeds input/page/range cap");
            }
            if let Some(replacement) = &doc.replacement {
                if replacement.template.len() > INPUT_BYTES {
                    return Err("replacement template exceeds input cap");
                }
                if let ReplaceTarget::Matches(matches) = &replacement.target
                    && matches.len() > REPLACE_EDITS
                {
                    return Err("replacement selection exceeds edit cap");
                }
            }
        }
        SearchTask::Files(files) => {
            if files.expression.len() > INPUT_BYTES
                || files.root.as_os_str().as_encoded_bytes().len() > 32768
                || files.include.len() + files.exclude.len() > 256
                || files.include.iter().chain(&files.exclude).map(String::len).sum::<usize>() > INPUT_BYTES
                || files.max_line_bytes > 4096
                || files.max_match_ranges > PAGE_HITS
            {
                return Err("folder search exceeds bounded request/row cap");
            }
        }
        SearchTask::FilePage { page, .. } => {
            if page.len == 0 || page.len > PAGE_HITS {
                return Err("file result page exceeds cap");
            }
        }
    }
    Ok(())
}
fn publish(shared: &Shared, cancel: &Cancellation, event: SearchEvent, wake: &impl Fn()) -> bool {
    publish_with_gate(shared, cancel, event, wake, &|| {})
}
fn publish_with_gate(
    shared: &Shared,
    cancel: &Cancellation,
    event: SearchEvent,
    wake: &impl Fn(),
    before_full_wait: &impl Fn(),
) -> bool {
    loop {
        let mut state = shared.state.lock().unwrap();
        if cancel.load(Ordering::Acquire) || shared.stopped.load(Ordering::Acquire) {
            cancel.store(true, Ordering::Release);
            return false;
        }
        if state.events.len() < EVENTS {
            state.events.push_back(event);
            drop(state);
            // Never invoke the host callback under the mailbox lock.
            wake();
            return true;
        }
        shared.space.register();
        before_full_wait();
        drop(state);
        // A contended drain can consume the last host notification while the
        // fifth result is pending. Service its retry HERE, before this job can
        // return; the outer coordinator loop cannot make progress yet.
        if shared.retry_wake.swap(false, Ordering::AcqRel) {
            wake();
        }
        thread::park();
    }
}

fn seed_error(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
fn seed_cancel(cancel: &Cancellation) -> io::Result<()> {
    if cancel.load(Ordering::Acquire) {
        Err(io::Error::new(io::ErrorKind::Interrupted, "search seed cancelled"))
    } else {
        Ok(())
    }
}
const SEED_WORK_BYTES: usize = 4 * INPUT_BYTES;
struct SeedBudget {
    bytes_left: usize,
    steps_left: usize,
}
impl SeedBudget {
    fn new() -> Self {
        Self { bytes_left: SEED_WORK_BYTES, steps_left: 4 * INPUT_BYTES }
    }
    fn charge(&mut self, bytes: usize) -> io::Result<()> {
        self.require(bytes)?;
        self.bytes_left -= bytes;
        Ok(())
    }
    fn require(&self, bytes: usize) -> io::Result<()> {
        if bytes > self.bytes_left {
            return Err(seed_error("seed cursors exceed aggregate source/validation/copy budget"));
        }
        Ok(())
    }
    fn step(&mut self) -> io::Result<()> {
        if self.steps_left == 0 {
            return Err(seed_error("seed cursors exceed bounded scalar budget"));
        }
        self.steps_left -= 1;
        Ok(())
    }
}
// One reusable bounded window. Growth and sliding never admit a larger word;
// irrelevant prefetch is discarded instead of consuming the word's byte cap.
struct SeedReader<'a, 'b> {
    snapshot: &'a Snapshot,
    cancel: &'a Cancellation,
    budget: &'b mut SeedBudget,
    bytes: Vec<u8>,
    start: u64,
}
impl<'a, 'b> SeedReader<'a, 'b> {
    fn new(snapshot: &'a Snapshot, cancel: &'a Cancellation, budget: &'b mut SeedBudget) -> Self {
        Self { snapshot, cancel, budget, bytes: Vec::with_capacity(INPUT_BYTES + 8), start: 0 }
    }
    fn trim(&mut self, range: Range<u64>) {
        let start = range.start.max(self.start);
        let end = range.end.min(self.start + self.bytes.len() as u64).max(start);
        let first = (start - self.start) as usize;
        let len = (end - start) as usize;
        self.bytes.copy_within(first..first + len, 0);
        self.bytes.truncate(len);
        self.start = start;
    }
    fn ensure(&mut self, range: Range<u64>) -> io::Result<()> {
        seed_cancel(self.cancel)?;
        if range.start > range.end || range.end > self.snapshot.len() {
            return Err(seed_error("seed endpoint outside snapshot"));
        }
        let old_end = self.start + self.bytes.len() as u64;
        if !self.bytes.is_empty() && self.start <= range.start && range.end <= old_end {
            return Ok(());
        }
        if self.bytes.is_empty()
            || range.start > old_end.saturating_add(256)
            || range.end.saturating_add(256) < self.start
        {
            self.bytes.clear();
            self.start = range.start;
        }
        let cap = (INPUT_BYTES + 8) as u64;
        let old_end = self.start + self.bytes.len() as u64;
        // A lookup can move in either direction (lookbehind, another cursor).
        // Trim a useful block so scalar probes cannot slide/copy a full window
        // once per byte. Only the actually needed range must survive this move.
        if old_end.max(range.end) - self.start.min(range.start) > cap {
            if range.start < self.start {
                self.trim(self.start..(range.start + cap / 2).max(self.start));
            } else {
                self.trim(range.end.saturating_sub(cap / 2).max(self.start)..old_end);
            }
        }
        let old_end = self.start + self.bytes.len() as u64;
        let growth = self.bytes.len().max(256) as u64;
        let mut start = self.start.min(range.start);
        let mut end = old_end.max(range.end);
        if end - start > cap {
            return Err(seed_error("seed word exceeds bounded text window"));
        }
        if range.start < self.start {
            start = self.start.saturating_sub(growth).min(range.start).max(end.saturating_sub(cap));
        }
        if range.end > old_end {
            end = old_end.saturating_add(growth).max(range.end).min(self.snapshot.len()).min(start + cap);
        }
        let prefix = (self.start - start) as usize;
        let suffix = (end - old_end) as usize;
        let added = prefix + suffix;
        self.budget.charge(added)?;
        let old_len = self.bytes.len();
        self.bytes.resize((end - start) as usize, 0);
        self.bytes.copy_within(0..old_len, prefix);
        copy_seed_bytes(self.snapshot, start..self.start, &mut self.bytes[..prefix], self.cancel)?;
        copy_seed_bytes(self.snapshot, old_end..end, &mut self.bytes[prefix + old_len..], self.cancel)?;
        self.start = start;
        Ok(())
    }
    fn byte(&mut self, at: u64) -> io::Result<u8> {
        self.ensure(at..at + 1)?;
        Ok(self.bytes[(at - self.start) as usize])
    }
    fn character(&mut self, at: u64) -> io::Result<(char, u64)> {
        seed_cancel(self.cancel)?;
        self.budget.step()?;
        let width = match self.byte(at)? {
            0..=0x7f => 1,
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => return Err(seed_error("seed touches opaque or invalid UTF-8")),
        };
        self.ensure(at..at + width)?;
        let first = (at - self.start) as usize;
        let text = std::str::from_utf8(&self.bytes[first..first + width as usize])
            .map_err(|_| seed_error("seed touches opaque or invalid UTF-8"))?;
        Ok((text.chars().next().unwrap(), at + width))
    }
    fn previous(&mut self, at: u64) -> io::Result<u64> {
        let mut start = at.checked_sub(1).ok_or_else(|| seed_error("seed has no previous scalar"))?;
        for _ in 0..3 {
            if self.byte(start)? & 0xc0 != 0x80 {
                break;
            }
            start = start.checked_sub(1).ok_or_else(|| seed_error("seed splits UTF-8 scalar"))?;
        }
        let (c, end) = self.character(start)?;
        if end != at {
            return Err(seed_error("seed splits UTF-8 scalar"));
        }
        if c == '\n' && start > 0 && self.byte(start - 1)? == b'\r' {
            start -= 1;
        }
        Ok(start)
    }
    fn word(&mut self, at: u64, capture: bool) -> io::Result<(Range<u64>, Option<String>)> {
        let at = if at == self.snapshot.len() { self.previous(at)? } else { at };
        let initial = seed_class(self.character(at)?.0);
        let mut start = at;
        while start > 0 {
            let previous = self.previous(start)?;
            if seed_class(self.character(previous)?.0) != initial {
                break;
            }
            start = previous;
            if at - start > INPUT_BYTES as u64 {
                return Err(seed_error("seed word exceeds input cap"));
            }
        }
        // Lookbehind may have prefetched an unrelated delimiter prefix. It
        // must not reduce the admissible complete-word window.
        self.trim(start..self.start + self.bytes.len() as u64);
        let mut text = capture.then(|| String::with_capacity(INPUT_BYTES));
        let mut end = start;
        while end < self.snapshot.len() {
            let (c, next) = self.character(end)?;
            if seed_class(c) != initial {
                break;
            }
            let crlf = c == '\r' && next < self.snapshot.len() && self.byte(next)? == b'\n';
            end = if crlf { next + 1 } else { next };
            if end - start > INPUT_BYTES as u64 {
                return Err(seed_error("seed word exceeds input cap"));
            }
            if let Some(text) = &mut text {
                self.budget.charge(c.len_utf8() + usize::from(crlf))?;
                text.push(c);
                if crlf {
                    text.push('\n');
                }
            }
        }
        Ok((start..end, text))
    }
}
fn copy_seed_bytes(snapshot: &Snapshot, range: Range<u64>, into: &mut [u8], cancel: &Cancellation) -> io::Result<()> {
    let mut offset = 0;
    for chunk in snapshot.chunks(range)? {
        seed_cancel(cancel)?;
        let chunk = chunk.map_err(io::Error::other)?;
        into[offset..offset + chunk.len()].copy_from_slice(chunk);
        offset += chunk.len();
    }
    Ok(())
}
// Kept in step with Editor's word classes; this worker version is strict about
// invalid text and refuses incomplete capped words instead of truncating them.
fn seed_class(c: char) -> u8 {
    if c.is_whitespace() {
        0
    } else if c.is_alphanumeric()
        || c == '_'
        || matches!(c as u32, 0x300..=0x36f | 0x1ab0..=0x1aff | 0x1dc0..=0x1dff | 0xfe20..=0xfe2f)
    {
        1
    } else {
        2
    }
}
fn visit_seed_sources(
    request: &SeedRequest,
    range: Range<u64>,
    cancel: &Cancellation,
    budget: &mut SeedBudget,
    mut output: Option<&mut Vec<u8>>,
) -> io::Result<()> {
    use kwil_core_text::SourceChunk;
    if range.start > range.end || range.end > request.snapshot.len() || range.end - range.start > INPUT_BYTES as u64 {
        return Err(seed_error("seed selection outside snapshot or exceeds input cap"));
    }
    // Detached classification can revisit u32 endpoints several times per
    // logical byte. Admit/debit each actual read, never an estimate based on
    // the yielded length. Keep the existing logical source visit allowance;
    // payload copying and UTF-8 validation remain separate visits as before.
    const SOURCE_OVERHEAD: usize = 2 + 2 * std::mem::size_of::<u32>();
    let mut remaining = (range.end - range.start) as usize;
    let mut chunks = request.snapshot.source_chunks(range)?;
    while remaining != 0 {
        seed_cancel(cancel)?;
        let chunk = chunks
            .next_admitted(|bytes| {
                seed_cancel(cancel)?;
                budget.charge(bytes)
            })?
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "seed source range ended early"))?;
        let len = match &chunk {
            SourceChunk::Added(bytes)
            | SourceChunk::Original { bytes, .. }
            | SourceChunk::Decoded { bytes, .. }
            | SourceChunk::Preserved { bytes, .. }
            | SourceChunk::Opaque { bytes, .. } => bytes.len(),
        };
        if len == 0 || len > remaining {
            return Err(seed_error("seed source range contract violated"));
        }
        budget.charge(len + SOURCE_OVERHEAD)?;
        remaining -= len;
        let text = match chunk {
            SourceChunk::Added(bytes) | SourceChunk::Preserved { bytes, .. } => bytes,
            SourceChunk::Original { bytes, .. } if request.allow_raw_utf8 => bytes,
            SourceChunk::Original { .. } => {
                return Err(seed_error("seed raw origin has no verified UTF-8 interpretation"));
            }
            SourceChunk::Opaque { .. } => return Err(seed_error("seed contains an opaque atom")),
            SourceChunk::Decoded { origin, range, bytes } => {
                origin
                    .raw_range_admitted(range, cancel, |bytes| budget.charge(bytes))
                    .map_err(kwil_core_text::decoded_backing::BackingError::into_io_error)?;
                bytes
            }
        };
        if let Some(output) = &mut output {
            budget.charge(text.len())?;
            output.extend_from_slice(text);
        }
    }
    Ok(())
}
fn seed_text(
    request: &SeedRequest,
    range: Range<u64>,
    cancel: &Cancellation,
    budget: &mut SeedBudget,
) -> io::Result<String> {
    let len = range
        .end
        .checked_sub(range.start)
        .filter(|len| *len <= INPUT_BYTES as u64)
        .ok_or_else(|| seed_error("seed selection outside snapshot or exceeds input cap"))? as usize;
    let mut bytes = Vec::with_capacity(len);
    visit_seed_sources(request, range, cancel, budget, Some(&mut bytes))?;
    budget.charge(len)?; // full validation of explicit selections, not just words
    String::from_utf8(bytes).map_err(|_| seed_error("seed selection splits or contains invalid UTF-8"))
}
fn run_seed(request: &SeedRequest, cancel: &Cancellation) -> io::Result<Option<SeedResult>> {
    run_seed_with_budget(request, cancel, &mut SeedBudget::new())
}
fn run_seed_with_budget(
    request: &SeedRequest,
    cancel: &Cancellation,
    budget: &mut SeedBudget,
) -> io::Result<Option<SeedResult>> {
    seed_cancel(cancel)?;
    let primary = request.cursors.get(request.primary).ok_or_else(|| seed_error("seed has no primary cursor"))?;
    if request.snapshot.is_empty() {
        return Ok(None);
    }
    let from_selection = primary.anchor != primary.head;
    let mut cursors = request.cursors.clone();
    let text = if from_selection {
        seed_text(request, primary.anchor.min(primary.head)..primary.anchor.max(primary.head), cancel, budget)?
    } else {
        let mut reader = SeedReader::new(&request.snapshot, cancel, budget);
        // Capture primary first, once. Other cursors need validated endpoints,
        // not independent copies of the same seed text.
        let (range, text) = reader.word(primary.head, true)?;
        visit_seed_sources(request, range.clone(), cancel, reader.budget, None)?;
        cursors[request.primary] = SeedCursor { anchor: range.start, head: range.end };
        let mut validated = vec![range]; // <= cursor cap, no per-character index
        for (index, cursor) in cursors.iter_mut().enumerate() {
            if index == request.primary {
                continue;
            }
            let at = if cursor.head == request.snapshot.len() { reader.previous(cursor.head)? } else { cursor.head };
            let range = if let Some(range) = validated.iter().find(|range| range.contains(&at)) {
                // A cursor within a known word may still split a UTF-8 scalar.
                reader.character(at)?;
                range.clone()
            } else {
                let (range, _) = reader.word(cursor.head, false)?;
                visit_seed_sources(request, range.clone(), cancel, reader.budget, None)?;
                validated.push(range.clone());
                range
            };
            *cursor = SeedCursor { anchor: range.start, head: range.end };
        }
        text.unwrap()
    };
    let primary = cursors[request.primary];
    let range = primary.anchor.min(primary.head)..primary.anchor.max(primary.head);
    seed_cancel(cancel)?;
    Ok(Some(SeedResult { text, range, cursors, primary: request.primary, from_selection }))
}

fn normalize_ranges(ranges: &[Range<u64>], len: u64) -> io::Result<Vec<Range<u64>>> {
    let mut ranges = ranges.to_vec();
    if ranges.iter().any(|range| range.start > range.end || range.end > len) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "search selection outside snapshot"));
    }
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut merged: Vec<Range<u64>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        // Merge overlaps and duplicates, but keep adjacent selection boundaries:
        // a match crossing two independently selected regions is not selected.
        if let Some(last) = merged.last_mut()
            && (range.start < last.end || *last == range)
        {
            last.end = last.end.max(range.end);
        } else {
            merged.push(range);
        }
    }
    Ok(merged)
}
fn run_document(
    key: SearchKey,
    request: &DocumentSearch,
    cancel: &Cancellation,
    emit: &impl Fn(SearchUpdate) -> bool,
    spools: &mut SpoolOwner,
) -> io::Result<()> {
    let pattern = request.query.compile()?;
    let ranges = normalize_ranges(&request.ranges, request.snapshot.len())?;
    let total = ranges.iter().map(|r| r.end - r.start).sum();
    let mut count = 0;
    let mut scanned = 0;
    let mut hits = Vec::with_capacity(request.page.len);
    let mut first = None;
    let mut last = None;
    let mut navigation = None;
    let mut signal = [0; SIGNAL_BINS];
    let mut last_progress = Instant::now();
    let replace_all = request.replacement.as_ref().is_some_and(|r| matches!(r.target, ReplaceTarget::All));
    let spool = if replace_all { Some(spools.begin()?) } else { None };
    let mut plan_error = None;
    let mut scan_error = None;
    let mut page_sent = false;
    for range in &ranges {
        let prior_count = count;
        let summary = search_default(
            &request.snapshot,
            &pattern,
            range.clone(),
            SearchMode::Hits,
            cancel,
            |hit| {
                if hit.len == 0 && last == Some(hit) {
                    return;
                }
                first.get_or_insert(hit);
                last = Some(hit);
                if let Some(direction) = request.navigation {
                    match direction {
                        Navigation::Next { from, include_current } => {
                            if navigation.is_none() && (hit.offset > from || include_current && hit.offset == from) {
                                navigation = Some(hit);
                            }
                        }
                        Navigation::Previous { from, include_current } => {
                            if hit.offset < from || include_current && hit.offset == from {
                                navigation = Some(hit);
                            }
                        }
                    }
                }
                let bin = ((hit.offset as u128 * SIGNAL_BINS as u128) / request.snapshot.len().max(1) as u128)
                    .min((SIGNAL_BINS - 1) as u128) as usize;
                signal[bin] += 1;
                if count >= request.page.first && hits.len() < request.page.len {
                    hits.push(hit);
                }
                count += 1;
                if hits.len() == request.page.len && !page_sent {
                    emit(SearchUpdate::Hits { first: request.page.first, hits: hits.clone() });
                    page_sent = true;
                }
                if let Some(slot) = spool
                    && plan_error.is_none()
                {
                    let replacement = request.replacement.as_ref().unwrap();
                    if let Err(error) =
                        plan_replace(&request.snapshot, &pattern, [hit], &replacement.template, cancel, |edit| {
                            spools.append(slot, edit)
                        })
                    {
                        plan_error = Some(error);
                    }
                }
            },
            |progress| {
                if last_progress.elapsed() >= Duration::from_millis(100) {
                    emit(SearchUpdate::Progress(Progress {
                        scanned: scanned + progress.scanned,
                        total,
                        count: prior_count + progress.count,
                    }));
                    last_progress = Instant::now();
                }
            },
        );
        let summary = match summary {
            Ok(summary) => summary,
            Err(error) => {
                scan_error = Some(error);
                break;
            }
        };
        scanned += summary.scanned;
        if cancel.load(Ordering::Acquire) {
            break;
        }
    }
    if scan_error.is_some() || plan_error.is_some() || cancel.load(Ordering::Acquire) {
        if let Some(slot) = spool {
            spools.abandon(slot);
        }
        return scan_error.or(plan_error).map_or(Ok(()), Err);
    }
    let navigation = navigation.or(match request.navigation {
        Some(Navigation::Next { .. }) => first,
        Some(Navigation::Previous { .. }) => last,
        None => None,
    });
    let replacement = if let Some(slot) = spool {
        match spools.finish(slot) {
            Ok(recipe) => Some(ReplaceRecipe { key, edits: ReplacementEdits::Spool(recipe) }),
            Err(error) => {
                spools.abandon(slot);
                return Err(error);
            }
        }
    } else if let Some(replacement) = &request.replacement {
        let ReplaceTarget::Matches(matches) = &replacement.target else { unreachable!() };
        if matches.iter().any(|m| {
            !ranges.iter().any(|r| m.offset >= r.start && m.offset.checked_add(m.len).is_some_and(|end| end <= r.end))
        }) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "replacement match outside search selection"));
        }
        let mut edits = Vec::with_capacity(matches.len());
        let mut bytes = 0;
        plan_replace(&request.snapshot, &pattern, matches.iter().copied(), &replacement.template, cancel, |edit| {
            if edit.new_bytes.len() > REPLACE_BYTES - bytes {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "replacement expansion exceeds atomic byte cap",
                ));
            }
            bytes += edit.new_bytes.len();
            edits.push(edit);
            Ok(())
        })?;
        Some(ReplaceRecipe { key, edits: ReplacementEdits::Inline(Arc::new(edits)) })
    } else {
        None
    };
    if !cancel.load(Ordering::Acquire) {
        emit(SearchUpdate::Document(Box::new(DocumentMatches {
            first: request.page.first,
            hits,
            count,
            scanned,
            navigation,
            signal,
            replacement,
        })));
    }
    Ok(())
}
fn run_files(
    key: SearchKey,
    request: &FileSearch,
    cancel: &Cancellation,
    emit: &impl Fn(SearchUpdate) -> bool,
    spools: &mut SpoolOwner,
) -> io::Result<()> {
    let slot = spools.begin()?;
    let result = thread::scope(|scope| {
        let (tx, rx) = std::sync::mpsc::sync_channel(16);
        let producer = scope.spawn(|| find_in_files(request, cancel, tx));
        let mut rows = Vec::with_capacity(FILE_PAGE);
        let mut first = 0;
        let mut ordinal = 0_u64;
        let mut flush_at = Instant::now();
        let scanned = (|| -> io::Result<()> {
            loop {
                match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(row) => {
                        let bytes = encode_file_event(&row)?;
                        spools.append(slot, Edit { offset: ordinal, len: 0, new_bytes: bytes })?;
                        ordinal =
                            ordinal.checked_add(1).ok_or_else(|| io::Error::other("file result count overflow"))?;
                        rows.push(row);
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
                if rows.len() == FILE_PAGE || !rows.is_empty() && flush_at.elapsed() >= Duration::from_millis(100) {
                    let len = rows.len();
                    if !emit(SearchUpdate::Files {
                        first,
                        rows: std::mem::replace(&mut rows, Vec::with_capacity(FILE_PAGE)),
                        results: Some(FileResults { session: key, spool: spools.checkpoint(slot)? }),
                    }) {
                        cancel.store(true, Ordering::Release);
                    }
                    first += len as u64;
                    flush_at = Instant::now();
                }
                if cancel.load(Ordering::Acquire) {
                    break;
                }
            }
            Ok(())
        })();
        drop(rx); // unblock bounded producer before joining.
        let summary = producer.join().map_err(|_| io::Error::other("folder search worker panicked"))??;
        scanned?;
        Ok::<_, io::Error>((summary, first, rows))
    });
    let (summary, first, rows) = match result {
        Ok(value) if !cancel.load(Ordering::Acquire) => value,
        Ok(_) => {
            spools.abandon(slot);
            return Ok(());
        }
        Err(error) => {
            spools.abandon(slot);
            return Err(error);
        }
    };
    let spool = match spools.finish(slot) {
        Ok(spool) => spool,
        Err(error) => {
            spools.abandon(slot);
            return Err(error);
        }
    };
    if !rows.is_empty() {
        emit(SearchUpdate::Files { first, rows, results: Some(FileResults { session: key, spool }) });
    }
    emit(SearchUpdate::FilesDone(summary));
    Ok(())
}

#[cfg(test)]
#[path = "source_release_tests.rs"]
mod source_release_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_shutdown_refusal_preserves_exact_worker_and_retry_controls() {
        let shared = Arc::new(Shared::new());
        let worker = SearchWorker { shared: shared.clone(), thread: None };
        let error = worker.shutdown_bounded(Duration::ZERO).unwrap_err();
        assert!(error.to_string().contains("owner retained"));
        error.retry_retirement();
        assert!(shared.retirement_due.load(Ordering::Acquire));
        let worker = error.into_worker();
        assert!(Arc::ptr_eq(&shared, &worker.shared));
        shared.completed.store(true, Ordering::Release);
        worker.shutdown_bounded(Duration::ZERO).unwrap();
    }
    #[test]
    fn bounded_shutdown_reports_panics_without_claiming_cleanup_success() {
        let shared = Arc::new(Shared::new());
        shared.completed.store(true, Ordering::Release);
        shared.panicked.store(true, Ordering::Release);
        let worker = SearchWorker { shared, thread: None };
        let error = worker.shutdown_bounded(Duration::ZERO).unwrap_err();
        assert!(error.to_string().contains("panicked"));
        assert!(error.retry(Duration::ZERO).is_err());
        fn error_transport<T: std::error::Error + Send + Sync>() {}
        error_transport::<ShutdownError>();
    }
    fn seed_request(text: &[u8], cursors: Vec<SeedCursor>, primary: usize) -> SeedRequest {
        let mut buffer = kwil_core_text::Buffer::empty();
        buffer.insert(0, text).unwrap();
        SeedRequest { snapshot: buffer.snapshot(), cursors, primary, allow_raw_utf8: false }
    }
    fn caret(at: u64) -> SeedCursor {
        SeedCursor { anchor: at, head: at }
    }
    #[test]
    fn seed_selection_is_literal_and_preserves_other_cursors() {
        let cursors = vec![SeedCursor { anchor: 6, head: 0 }, caret(7)];
        let request = seed_request("Ä.*\n x".as_bytes(), cursors.clone(), 0);
        let seed = run_seed(&request, &Arc::new(AtomicBool::new(false))).unwrap().unwrap();
        assert_eq!(seed.text, "Ä.*\n ");
        assert_eq!(seed.range, 0..6);
        assert_eq!(seed.cursors, cursors);
        assert!(seed.from_selection);
    }
    #[test]
    fn seed_words_preserve_classes_multicursors_and_previous_word_at_eof() {
        let request = seed_request("e\u{301} foo_bar !!".as_bytes(), vec![caret(0), caret(8), caret(14)], 1);
        let cancel = Arc::new(AtomicBool::new(false));
        let seed = run_seed(&request, &cancel).unwrap().unwrap();
        assert_eq!(seed.text, "foo_bar");
        assert_eq!(
            seed.cursors,
            [
                SeedCursor { anchor: 0, head: 3 },
                SeedCursor { anchor: 4, head: 11 },
                SeedCursor { anchor: 12, head: 14 }
            ]
        );
        assert_eq!(seed.primary, 1);
        assert!(!seed.from_selection);
        let eof = run_seed(&seed_request(b"word", vec![caret(4)], 0), &cancel).unwrap().unwrap();
        assert_eq!((eof.text.as_str(), eof.range), ("word", 0..4));
        assert!(run_seed(&seed_request(b"", vec![caret(0)], 0), &cancel).unwrap().is_none());
        let nul = run_seed(&seed_request(b"word\0", vec![caret(5)], 0), &cancel).unwrap().unwrap();
        assert_eq!((nul.text.as_bytes(), nul.range), (b"\0".as_slice(), 4..5));
        let whitespace = run_seed(&seed_request(b"word\r\n ", vec![caret(7)], 0), &cancel).unwrap().unwrap();
        assert_eq!((whitespace.text.as_str(), whitespace.range), ("\r\n ", 4..7));
    }
    #[test]
    fn seed_caps_invalid_endpoints_and_cancel_refuse_partial_output() {
        let cancel = Arc::new(AtomicBool::new(false));
        let exact = vec![b'x'; INPUT_BYTES];
        let seed = run_seed(&seed_request(&exact, vec![caret(INPUT_BYTES as u64)], 0), &cancel).unwrap().unwrap();
        assert_eq!(seed.text.len(), INPUT_BYTES);
        assert_eq!(seed.range, 0..INPUT_BYTES as u64);
        let oversized = vec![b'x'; INPUT_BYTES + 1];
        assert!(run_seed(&seed_request(&oversized, vec![caret(0)], 0), &cancel).is_err());
        assert!(run_seed(&seed_request(b"a\xffb", vec![SeedCursor { anchor: 0, head: 3 }], 0), &cancel).is_err());
        assert!(run_seed(&seed_request("é".as_bytes(), vec![SeedCursor { anchor: 0, head: 1 }], 0), &cancel).is_err());
        assert!(run_seed(&seed_request("é".as_bytes(), vec![caret(1)], 0), &cancel).is_err());
        cancel.store(true, Ordering::Release);
        assert_eq!(
            run_seed(&seed_request(b"word", vec![caret(0)], 0), &cancel).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
    }
    #[test]
    fn seed_exact_64k_word_slides_past_lookbehind_and_multibyte_delimiters() {
        let cancel = Arc::new(AtomicBool::new(false));
        let ascii = format!("{}{} ", " ".repeat(256), "x".repeat(INPUT_BYTES));
        for at in [256, 256 + (INPUT_BYTES / 2) as u64, 256 + INPUT_BYTES as u64 - 1] {
            let seed = run_seed(&seed_request(ascii.as_bytes(), vec![caret(at)], 0), &cancel).unwrap().unwrap();
            assert_eq!(seed.range, 256..256 + INPUT_BYTES as u64);
            assert_eq!(seed.text, "x".repeat(INPUT_BYTES));
        }
        let prefix = "\u{2003}".repeat(86); //258 bytes, multibyte lookbehind
        let word = "\u{10400}".repeat(INPUT_BYTES / 4);
        let text = format!("{prefix}{word}\u{2003}");
        let start = prefix.len() as u64;
        for at in [start, start + (INPUT_BYTES / 2) as u64] {
            let seed = run_seed(&seed_request(text.as_bytes(), vec![caret(at)], 0), &cancel).unwrap().unwrap();
            assert_eq!(seed.range, start..start + INPUT_BYTES as u64);
            assert_eq!(seed.text, word);
        }
        let eof = format!("{prefix}{word}");
        let seed = run_seed(&seed_request(eof.as_bytes(), vec![caret(eof.len() as u64)], 0), &cancel).unwrap().unwrap();
        assert_eq!((seed.range, seed.text), (start..eof.len() as u64, word));
    }
    #[test]
    fn seed_fifteen_astral_carets_share_one_word_without_primary_recopy() {
        let text = "\u{10400}".repeat(16384);
        assert_eq!(text.len(), INPUT_BYTES);
        let request = seed_request(text.as_bytes(), (0..15).map(|i| caret(i * 4)).collect(), 14);
        let mut budget = SeedBudget::new();
        let seed = run_seed_with_budget(&request, &Arc::new(AtomicBool::new(false)), &mut budget).unwrap().unwrap();
        assert_eq!(seed.text, text);
        assert_eq!(seed.primary, 14);
        assert_eq!(seed.cursors, vec![SeedCursor { anchor: 0, head: INPUT_BYTES as u64 }; 15]);
        assert!(budget.bytes_left > 0, "all visits and primary copy must fit the shared256KiB budget");
        // Reusing a word must never bless a cursor in a scalar's interior.
        let invalid = seed_request(text.as_bytes(), vec![caret(0), caret(1)], 0);
        assert!(run_seed(&invalid, &Arc::new(AtomicBool::new(false))).is_err());
    }
    #[test]
    fn seed_selection_and_distinct_words_charge_the_same_admission_budget() {
        let cancel = Arc::new(AtomicBool::new(false));
        let text = "\u{10400}".repeat(INPUT_BYTES / 4);
        let request = seed_request(text.as_bytes(), vec![SeedCursor { anchor: 0, head: INPUT_BYTES as u64 }], 0);
        let mut insufficient = SeedBudget { bytes_left: 3 * INPUT_BYTES + 9, steps_left: 4 * INPUT_BYTES };
        let error = run_seed_with_budget(&request, &cancel, &mut insufficient).unwrap_err();
        assert!(error.to_string().contains("aggregate source/validation/copy budget"));
        let mut exact = SeedBudget { bytes_left: 3 * INPUT_BYTES + 10, steps_left: 4 * INPUT_BYTES };
        assert_eq!(run_seed_with_budget(&request, &cancel, &mut exact).unwrap().unwrap().text, text);
        assert_eq!(exact.bytes_left, 0);
        let word = "x".repeat(INPUT_BYTES);
        let text = format!("{word} {word} {word}");
        let request = seed_request(
            text.as_bytes(),
            vec![caret(0), caret((INPUT_BYTES + 1) as u64), caret((2 * INPUT_BYTES + 2) as u64)],
            0,
        );
        let error = run_seed(&request, &cancel).unwrap_err();
        assert!(error.to_string().contains("aggregate source/validation/copy budget"));
    }
    #[test]
    fn seed_decoded_alias_and_opaque_provenance_are_not_reinterpreted() {
        use kwil_core_text::{
            Buffer, TextFile,
            decoded_backing::{DecodedBacking, Interpretation},
            encoding::Encoding,
        };
        let parent = std::env::temp_dir().join(format!(
            "kwil-search-seed-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir(&parent).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let owner = DecodedBacking::prepare_from_reader(
            &mut b"\x88\x62!".as_slice(),
            3,
            Interpretation::new(Encoding::for_label("big5").unwrap(), 0).unwrap(),
            &parent,
            &cancel,
        )
        .unwrap();
        let buffer = Buffer::from_decoded(owner.snapshot());
        let mut request =
            SeedRequest { snapshot: buffer.snapshot(), cursors: vec![caret(0)], primary: 0, allow_raw_utf8: false };
        let alias = run_seed(&request, &cancel).unwrap().unwrap();
        let mut imported = Buffer::empty();
        imported.insert_snapshot_range(0, &request.snapshot, 0..request.snapshot.len()).unwrap();
        let imported_request =
            SeedRequest { snapshot: imported.snapshot(), cursors: vec![caret(0)], primary: 0, allow_raw_utf8: false };
        let detached_alias = run_seed(&imported_request, &cancel).unwrap().unwrap();
        assert_eq!(detached_alias.text, alias.text, "preserved valid atoms remain searchable text");
        drop(imported_request);
        drop(imported);
        request.cursors = vec![SeedCursor { anchor: 0, head: 2 }];
        let split_refused = run_seed(&request, &cancel).is_err();
        drop(request);
        drop(buffer);
        owner.try_retire().map_err(|error| error.with_cause(io::Error::other("seed fixture retirement"))).unwrap();
        let owner = DecodedBacking::prepare_from_reader(
            &mut b"a\xffb".as_slice(),
            3,
            Interpretation::new(Encoding::UTF8, 0).unwrap(),
            &parent,
            &cancel,
        )
        .unwrap();
        let buffer = Buffer::from_decoded(owner.snapshot());
        let request = SeedRequest {
            snapshot: buffer.snapshot(),
            cursors: vec![SeedCursor { anchor: 0, head: 3 }],
            primary: 0,
            allow_raw_utf8: false,
        };
        let opaque_refused = run_seed(&request, &cancel).is_err();
        drop(request);
        drop(buffer);
        owner.try_retire().map_err(|error| error.with_cause(io::Error::other("seed fixture retirement"))).unwrap();
        let raw_path = parent.join("raw.txt");
        std::fs::write(&raw_path, b"word").unwrap();
        let file = TextFile::open(&raw_path).unwrap();
        file.build_index(1).unwrap();
        let buffer = Buffer::from_file(file.clone()).unwrap();
        let mut request =
            SeedRequest { snapshot: buffer.snapshot(), cursors: vec![caret(0)], primary: 0, allow_raw_utf8: false };
        let untyped_refused = run_seed(&request, &cancel).is_err();
        request.allow_raw_utf8 = true;
        let verified = run_seed(&request, &cancel).unwrap().unwrap();
        drop(request);
        drop(buffer);
        assert!(file.try_close().is_ok());
        std::fs::remove_file(&raw_path).unwrap();
        std::fs::remove_dir(&parent).unwrap();
        assert_eq!((alias.text.as_str(), alias.range), ("Ê\u{304}", 0..4));
        assert!(split_refused && opaque_refused && untyped_refused);
        assert_eq!(verified.text, "word");
    }

    #[test]
    fn seed_detached_alias_selection_debits_actual_classification_before_copying() {
        use kwil_core_text::{
            Buffer,
            decoded_backing::{DecodedBacking, Interpretation},
            encoding::Encoding,
        };
        let parent = std::env::temp_dir().join(format!(
            "kwil-seed-admitted-alias-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir(&parent).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let raw = b"\x88\x62".repeat(15_000);
        let owner = DecodedBacking::prepare_from_reader(
            &mut raw.as_slice(),
            raw.len() as u64,
            Interpretation::new(Encoding::for_label("big5").unwrap(), 0).unwrap(),
            &parent,
            &cancel,
        )
        .unwrap();
        let source = Buffer::from_decoded(owner.snapshot());
        let target = Buffer::with_add_memory_limit(400_000);
        let snapshot = target.snapshot_importer().prepare(&source.snapshot(), 0..source.len()).unwrap();
        drop(source);
        owner
            .try_retire()
            .map_err(|error| error.with_cause(io::Error::other("alias admission fixture retirement")))
            .unwrap();
        assert_eq!(snapshot.len(), 60_000);
        assert!(snapshot.len() < INPUT_BYTES as u64, "input cap alone cannot detect this case");
        // Independent unlimited traversal supplies the classification oracle.
        // The production seed must debit these actual reads in addition to
        // its logical source, copy and UTF-8 validation visits.
        let mut chunks = snapshot.source_chunks(0..snapshot.len()).unwrap();
        let mut classified = 0;
        let mut logical = 0;
        while let Some(chunk) = chunks
            .next_admitted(|bytes| {
                classified += bytes;
                Ok(())
            })
            .unwrap()
        {
            let kwil_core_text::SourceChunk::Preserved { bytes, .. } = chunk else {
                panic!("alias provenance lost");
            };
            logical += bytes.len();
        }
        assert_eq!(logical, 60_000);
        assert!(classified > SEED_WORK_BYTES, "classification alone exceeds the unchanged shared work cap");
        drop(chunks);
        let request = SeedRequest {
            snapshot,
            cursors: vec![SeedCursor { anchor: 0, head: 60_000 }],
            primary: 0,
            allow_raw_utf8: false,
        };
        let mut budget = SeedBudget::new();
        let error = run_seed_with_budget(&request, &cancel, &mut budget).unwrap_err();
        assert!(error.to_string().contains("aggregate source/validation/copy budget"));
        assert!(budget.bytes_left <= SEED_WORK_BYTES);
        for available in [0, 1, 4] {
            let mut budget = SeedBudget { bytes_left: available, steps_left: 4 * INPUT_BYTES };
            let mut output = Vec::new();
            let error = visit_seed_sources(&request, 0..60_000, &cancel, &mut budget, Some(&mut output)).unwrap_err();
            assert!(error.to_string().contains("aggregate source/validation/copy budget"));
            assert!(output.is_empty(), "classification refusal cannot publish or copy a source run");
        }
        drop(request);
        std::fs::remove_dir(parent).unwrap();
    }
    #[test]
    fn full_retirement_and_dual_shutdown_keep_sources_until_worker_last_drop() {
        let parent = std::env::temp_dir().join(format!(
            "kwil-search-close-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir(&parent).unwrap();
        let paths = [parent.join("internal.txt"), parent.join("external.txt")];
        let mut sources = Vec::new();
        let mut requests = Vec::new();
        for (index, path) in paths.iter().enumerate() {
            std::fs::write(path, b"source").unwrap();
            let file = kwil_core_text::TextFile::open(path).unwrap();
            file.build_index(1).unwrap();
            let buffer = kwil_core_text::Buffer::from_file(file.clone()).unwrap();
            let mut item = request(index as u64 + 2, b"source", "s");
            let SearchTask::Document(doc) = &mut item.task else { unreachable!() };
            doc.snapshot = buffer.snapshot();
            drop(buffer);
            requests.push(item);
            sources.push(Some(file));
        }
        let sources = Arc::new(Mutex::new(sources));
        fn try_close_sources(sources: &Mutex<Vec<Option<kwil_core_text::TextFile>>>) -> Vec<bool> {
            sources
                .lock()
                .unwrap()
                .iter_mut()
                .map(|slot| {
                    let Some(file) = slot.take() else { return true };
                    match file.try_close() {
                        Ok(()) => true,
                        Err(file) => {
                            *slot = Some(file);
                            false
                        }
                    }
                })
                .collect()
        }
        let observed = sources.clone();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        let (retired_tx, retired_rx) = std::sync::mpsc::channel();
        let first = AtomicBool::new(true);
        let worker = SearchWorker::new(move || {
            if first.swap(false, Ordering::AcqRel) {
                entered_tx.send(()).unwrap();
                release_rx.lock().unwrap().recv_timeout(Duration::from_secs(5)).unwrap();
            }
            if try_close_sources(&observed).iter().all(|closed| *closed) {
                let _ = retired_tx.send(thread::current().name().unwrap_or("").to_owned());
            }
        })
        .unwrap();
        worker.shared.state.lock().unwrap().pending =
            Some(Job { request: request(1, b"seed", "s"), cancel: Arc::new(AtomicBool::new(false)) });
        worker.shared.notify();
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        {
            let mut state = worker.shared.state.lock().unwrap();
            for (index, slot) in state.retired.iter_mut().enumerate() {
                *slot = Some(Job {
                    request: request(index as u64 + 10, b"queued", "q"),
                    cancel: Arc::new(AtomicBool::new(true)),
                });
            }
        }
        let external = worker.retire(requests.pop().unwrap()).unwrap_err();
        let internal = requests.pop().unwrap();
        worker.close_with_requests(Some(internal), Some(*external)).continue_value().unwrap();
        let retained_before_release = try_close_sources(&sources);
        release_tx.send(()).unwrap();
        let retired_on = retired_rx.recv_timeout(Duration::from_secs(5));
        worker.shutdown().unwrap();
        for path in &paths {
            std::fs::remove_file(path).unwrap();
        }
        std::fs::remove_dir(&parent).unwrap();
        assert_eq!(retained_before_release, [false, false], "neither accepted closing source may drop on input");
        assert_eq!(retired_on.unwrap(), "kwil-search", "last source pins and file handles retire on coordinator");
    }
    #[test]
    fn contended_result_drain_rearms_one_explicit_worker_wake() {
        let (wake_tx, wake_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        let first = AtomicBool::new(true);
        let worker = SearchWorker::new(move || {
            if first.swap(false, Ordering::AcqRel) {
                wake_tx.send(()).unwrap();
                release_rx.lock().unwrap().recv_timeout(Duration::from_secs(5)).unwrap();
            }
        })
        .unwrap();
        let key = SearchKey { document_id: 7, revision: 2, generation: 9 };
        let mut state = worker.shared.state.lock().unwrap();
        state.events.push_back(SearchEvent { key, update: SearchUpdate::Failed("queued completion".into()) });
        let contended = worker.take_event();
        drop(state);
        let woke = wake_rx.recv_timeout(Duration::from_secs(5)).is_ok();
        let event = worker.take_event(); // callback gate keeps coordinator outside mailbox.
        let _ = release_tx.send(());
        worker.shutdown().unwrap();
        assert!(contended.is_none());
        assert!(woke, "contended final completion must rearm the worker notification");
        let event = event.expect("queued result remains drainable without a UI timer");
        assert_eq!(event.key, key);
        assert!(matches!(event.update, SearchUpdate::Failed(message) if message == "queued completion"));
    }
    #[test]
    fn full_four_file_pages_fifth_publish_contention_retries_before_job_returns() {
        // The sole producer IS the coordinator: no idle outer loop is allowed
        // to service retry_wake on its behalf while the fifth publish blocks.
        let shared = Arc::new(Shared::new());
        let worker = SearchWorker { shared: shared.clone(), thread: None };
        let key = SearchKey { document_id: 0, revision: 0, generation: 21 };
        let page = |first| SearchEvent { key, update: SearchUpdate::Files { first, rows: Vec::new(), results: None } };
        {
            let mut state = shared.state.lock().unwrap();
            for first in 0..EVENTS {
                state.events.push_back(page(first as u64));
            }
        }
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        let first_wait = AtomicBool::new(true);
        let (wake_tx, wake_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let producer_cancel = cancel.clone();
        let producer_shared = shared.clone();
        let producer = thread::spawn(move || {
            let _ = producer_shared.coordinator.set(thread::current());
            let accepted = publish_with_gate(
                &producer_shared,
                &producer_cancel,
                SearchEvent { key, update: SearchUpdate::Files { first: 4, rows: Vec::new(), results: None } },
                &|| {
                    let _ = wake_tx.send(());
                },
                &|| {
                    if first_wait.swap(false, Ordering::AcqRel) {
                        entered_tx.send(()).unwrap();
                        release_rx.lock().unwrap().recv_timeout(Duration::from_secs(5)).unwrap();
                    }
                },
            );
            done_tx.send(accepted).unwrap();
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let contended = worker.take_event(); // producer still owns the mailbox
        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let retried = wake_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).is_ok();
        let mut pages = Vec::new();
        if retried {
            while pages.len() < 5 {
                if let Some(event) = worker.take_event() {
                    if let SearchUpdate::Files { first, .. } = event.update {
                        pages.push(first);
                    }
                } else if wake_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).is_err() {
                    break;
                }
            }
        }
        // Always release the producer before asserting the negative control.
        cancel.store(true, Ordering::Release);
        shared.notify();
        let completed = done_rx.recv_timeout(Duration::from_secs(5));
        producer.join().unwrap();
        assert!(contended.is_none());
        assert!(retried, "full result queue must retry the lost drain before active job returns");
        assert_eq!(pages, [0, 1, 2, 3, 4]);
        assert!(completed.unwrap(), "fifth page must complete after the retried drain");
    }
    #[test]
    fn explicit_replace_uses_the_same_overlap_union_as_matching() {
        let mut request = request(1, b"abcdef", "bcde");
        let SearchTask::Document(doc) = &mut request.task else { unreachable!() };
        doc.ranges = vec![0..4, 2..6];
        doc.replacement = Some(Replacement {
            template: b"X".to_vec(),
            target: ReplaceTarget::Matches(vec![Match { offset: 1, len: 4 }]),
        });
        let output = Mutex::new(Vec::new());
        let emit = |event| {
            output.lock().unwrap().push(event);
            true
        };
        let mut spools = SpoolOwner::new(4096).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        run_document(request.key, doc, &cancel, &emit, &mut spools).unwrap();
        let result = output
            .into_inner()
            .unwrap()
            .into_iter()
            .find_map(|event| match event {
                SearchUpdate::Document(result) => Some(result),
                _ => None,
            })
            .unwrap();
        assert_eq!((result.count, result.hits), (1, vec![Match { offset: 1, len: 4 }]));
        let recipe = result.replacement.unwrap();
        assert_eq!(recipe.edits.iter().next().unwrap().unwrap(), Edit { offset: 1, len: 4, new_bytes: b"X".to_vec() });
        // Touching selection boundaries are independent, never a combined region.
        doc.ranges = vec![0..3, 3..6];
        let error = run_document(request.key, doc, &cancel, &|_| true, &mut spools).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
    #[test]
    fn file_spool_revisits_first_native_page_after_ninety_rows() {
        #[cfg(windows)]
        let path = {
            use std::os::windows::ffi::OsStringExt;
            std::path::PathBuf::from(std::ffi::OsString::from_wide(&[0x61, 0xd800, 0x62]))
        };
        #[cfg(unix)]
        let path = {
            use std::os::unix::ffi::OsStringExt;
            std::path::PathBuf::from(std::ffi::OsString::from_vec(vec![b'a', 0xff, b'b']))
        };
        let key = SearchKey { document_id: 0, revision: 0, generation: 9 };
        let mut owner = SpoolOwner::new(1 << 20).unwrap();
        let slot = owner.begin().unwrap();
        for ordinal in 0..90 {
            let row = FileEvent::Hit(crate::FileHit {
                path: path.clone(),
                line_number: ordinal + 1,
                line_offset: ordinal * 4,
                line: b"hit".to_vec(),
                line_truncated: false,
                match_ranges: std::iter::once(0..3).collect(),
                ranges_truncated: false,
            });
            owner.append(slot, Edit { offset: ordinal, len: 0, new_bytes: encode_file_event(&row).unwrap() }).unwrap();
        }
        let results = FileResults { session: key, spool: owner.finish(slot).unwrap() };
        owner.retire().unwrap(); // live result lease must retain the completed backing.
        let cancel = Arc::new(AtomicBool::new(false));
        let later = results.read_page(Page { first: 64, len: 5 }, &cancel).unwrap();
        let earlier = results.read_page(Page { first: 0, len: 5 }, &cancel).unwrap();
        assert_eq!((results.len(), results.session()), (90, key));
        let FileEvent::Hit(later) = &later[0] else { panic!("later hit") };
        assert_eq!((later.line_number, later.line_offset), (65, 256));
        let FileEvent::Hit(earlier) = &earlier[0] else { panic!("earlier hit") };
        assert_eq!((earlier.line_number, earlier.line_offset), (1, 0));
        assert_eq!(earlier.path.as_os_str(), path.as_os_str());
        assert_eq!(earlier.match_ranges.len(), 1);
        assert_eq!(earlier.match_ranges[0], 0..3);
        cancel.store(true, Ordering::Release);
        assert!(results.read_page(Page { first: 0, len: 5 }, &cancel).unwrap().is_empty());
        drop(results);
        owner.retire().unwrap();
        assert_eq!(owner.live(), 0);
    }
    fn request(generation: u64, text: &[u8], query: &str) -> SearchRequest {
        let mut buffer = kwil_core_text::Buffer::empty();
        buffer.insert(0, text).unwrap();
        SearchRequest {
            key: SearchKey { document_id: 1, revision: 4, generation },
            task: SearchTask::Document(DocumentSearch {
                snapshot: buffer.snapshot(),
                query: Query::literal(query.into()),
                ranges: std::iter::once(0..text.len() as u64).collect(),
                page: Page::default(),
                navigation: None,
                replacement: None,
            }),
        }
    }
    #[test]
    fn newest_pending_cancels_active_and_retires_superseded_source() {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        let first = AtomicBool::new(true);
        let (wake_tx, wake_rx) = std::sync::mpsc::channel();
        let worker = SearchWorker::new(move || {
            let _ = wake_tx.send(());
            if first.swap(false, Ordering::AcqRel) {
                entered_tx.send(()).unwrap();
                release_rx.lock().unwrap().recv_timeout(Duration::from_secs(5)).unwrap();
            }
        })
        .unwrap();
        // Seed under the mailbox lock, so startup scheduling cannot turn this
        // cancellation fixture into an admission retry test.
        worker.shared.state.lock().unwrap().pending =
            Some(Job { request: request(1, b"abc", "a"), cancel: Arc::new(AtomicBool::new(false)) });
        worker.shared.notify();
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        worker.submit(request(2, b"abc", "b")).unwrap();
        worker.submit(request(3, b"abc", "c")).unwrap();
        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let result = loop {
            if let Some(event) = worker.take_event() {
                assert_eq!(event.key.generation, 3, "stale search must not publish");
                if let SearchUpdate::Document(result) = event.update {
                    break result;
                }
            }
            wake_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
        };
        assert_eq!(result.hits, [Match { offset: 2, len: 1 }]);
        worker.shutdown().unwrap();
    }
    #[test]
    fn bounded_result_backpressure_cancels_a_blocked_producer() {
        let worker = SearchWorker::new(|| {}).unwrap();
        let shared = worker.shared.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let key = SearchKey { document_id: 1, revision: 1, generation: 1 };
        for _ in 0..EVENTS {
            assert!(publish(&shared, &cancel, SearchEvent { key, update: SearchUpdate::Cancelled }, &|| {}));
        }
        let producer_cancel = cancel.clone();
        let producer_shared = shared.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let producer = thread::spawn(move || {
            let accepted = publish(
                &producer_shared,
                &producer_cancel,
                SearchEvent { key, update: SearchUpdate::Cancelled },
                &|| {},
            );
            done_tx.send(accepted).unwrap();
        });
        assert!(done_rx.recv_timeout(Duration::from_millis(20)).is_err());
        assert_eq!(shared.state.lock().unwrap().events.len(), EVENTS);
        cancel.store(true, Ordering::Release);
        shared.space.notify_all();
        assert!(!done_rx.recv_timeout(Duration::from_secs(5)).unwrap());
        producer.join().unwrap();
        worker.shutdown().unwrap();
    }
    #[test]
    fn rejected_requests_keep_snapshot_ownership_under_retirement_pressure() {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        let first = AtomicBool::new(true);
        let worker = SearchWorker::new(move || {
            if first.swap(false, Ordering::AcqRel) {
                entered_tx.send(()).unwrap();
                release_rx.lock().unwrap().recv_timeout(Duration::from_secs(5)).unwrap();
            }
        })
        .unwrap();
        worker.shared.state.lock().unwrap().pending =
            Some(Job { request: request(0, b"seed", "s"), cancel: Arc::new(AtomicBool::new(false)) });
        worker.shared.notify();
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let state = worker.shared.state.lock().unwrap();
        let error = worker.submit(request(1, b"abc", "a")).unwrap_err();
        let reason = error.reason;
        let retained_len = match &error.request.task {
            SearchTask::Document(doc) => doc.snapshot.len(),
            _ => 0,
        };
        drop(state);
        worker.retire(*error.request).ok().unwrap();
        release_tx.send(()).unwrap();
        worker.shutdown().unwrap();
        assert_eq!(reason, "search mailbox busy; retry on wake");
        assert_eq!(retained_len, 3);
    }
}
