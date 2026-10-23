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
    ops::Range,
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
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
pub enum SearchTask {
    Document(DocumentSearch),
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
    closing: Option<SearchRequest>,
    live_spools: usize,
    retirement_errors: Vec<SpoolDiagnostic>,
    results_session: Option<SearchKey>,
}
struct Shared {
    state: Mutex<State>,
    space: Condvar,
    stopped: AtomicBool,
    retirement_due: AtomicBool,
    coordinator: OnceLock<thread::Thread>,
    retry_wake: AtomicBool,
}
impl Shared {
    fn notify(&self) {
        if let Some(thread) = self.coordinator.get() {
            thread.unpark();
        }
    }
}
pub struct SearchWorker {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}
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
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                pending: None,
                retired: [const { None }; RETIRED],
                events: VecDeque::with_capacity(EVENTS),
                active: None,
                closing: None,
                live_spools: 0,
                retirement_errors: Vec::new(),
                results_session: None,
            }),
            space: Condvar::new(),
            stopped: AtomicBool::new(false),
            retirement_due: AtomicBool::new(false),
            coordinator: OnceLock::new(),
            retry_wake: AtomicBool::new(false),
        });
        let worker = shared.clone();
        let thread = thread::Builder::new().name("kwil-search".into()).spawn(move || {
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
                let (job, retired, closing, stopped) = {
                    let mut state = worker.state.lock().unwrap();
                    if state.pending.is_none()
                        && state.retired.iter().all(Option::is_none)
                        && state.closing.is_none()
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
                        state.closing.take(),
                        stopped,
                    )
                };
                let retired_any = retired.iter().any(Option::is_some);
                drop(retired);
                drop(closing);
                if stopped {
                    drop(job);
                    let _ = spools.retire();
                    {
                        let mut state = worker.state.lock().unwrap();
                        state.live_spools = spools.live();
                        state.retirement_errors = spools.diagnostics().to_vec();
                    }
                    if spools.live() == 0 {
                        break;
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
            SearchTask::Document(_) => state.results_session = None,
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
        let event = self.shared.state.try_lock().ok()?.events.pop_front();
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
        self.close_with(None);
    }
    /// Reserved shutdown slot for the host's one rejected request. Call once
    /// while the worker is live; shutdown never disposes that source on input.
    pub fn close_with(&self, rejected: Option<SearchRequest>) {
        let mut state = self.shared.state.lock().unwrap();
        assert!(state.closing.is_none() || rejected.is_none(), "duplicate search close request");
        if rejected.is_some() {
            state.closing = rejected;
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
}
impl Drop for SearchWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

fn validate(request: &SearchRequest) -> Result<(), &'static str> {
    match &request.task {
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
    let mut state = shared.state.lock().unwrap();
    while state.events.len() == EVENTS && !cancel.load(Ordering::Acquire) && !shared.stopped.load(Ordering::Acquire) {
        state = shared.space.wait(state).unwrap();
    }
    if cancel.load(Ordering::Acquire) || shared.stopped.load(Ordering::Acquire) {
        cancel.store(true, Ordering::Release);
        return false;
    }
    state.events.push_back(event);
    drop(state);
    // Always notify after unlocking: a host try_lock may have raced this push
    // while an older notification was already being handled.
    wake();
    true
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
    for range in ranges {
        let prior_count = count;
        let summary = search_default(
            &request.snapshot,
            &pattern,
            range,
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
            !request
                .ranges
                .iter()
                .any(|r| m.offset >= r.start && m.offset.checked_add(m.len).is_some_and(|end| end <= r.end))
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
mod tests {
    use super::*;
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
