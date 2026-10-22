//! Shared palette input, typed actions, and an event-driven file matcher.
//!
//! `Palette` performs no filesystem operations. File matching runs on exactly one
//! `FileIndex` worker, with a 64-path feed queue and one replaceable query slot.
//! `latest()` clones an `Arc` to at most `MAX_RESULTS` entries, never the catalogue.
//! The parent posts a GUI/TUI event from the `Send + Sync` wake callback, then
//! applies `latest()` on that event. No frame polling or async runtime is needed.
//!
//! ```no_run
//! use kwil_core_commands::palette::*;
//! use std::sync::Arc;
//! let mut palette = Palette::new(32);
//! let mut index = FileIndex::new(IndexLimits::default(), Arc::new(|| {
//!     // Post a coalesced event to the parent's event loop; do not draw here.
//! })).unwrap();
//! palette.prime_files(index.latest()); // do this on index events while closed
//! let request = palette.open("src").unwrap().unwrap();
//! index.request(request).unwrap(); // cached rows remain immediately available
//! // On a worker-result event:
//! palette.apply_files(index.latest());
//! if let Some(action) = palette.accept() { /* parent dispatches action */ }
//! // On shutdown, join on a shutdown worker, never the UI input path:
//! index.stop().unwrap().join().unwrap();
//! ```
use crate::registry::COMMANDS;
use fuzzy_matcher::{FuzzyMatcher, skim::SkimMatcherV2};
use std::collections::VecDeque;
use std::io;
use std::path::PathBuf;
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::thread::{self, JoinHandle};
use unicode_segmentation::UnicodeSegmentation;

pub const MAX_QUERY_BYTES: usize = 4096;
pub const MAX_RESULTS: usize = 256;
pub const MAX_TABS: usize = 1024;
pub const MAX_LABEL_BYTES: usize = 32 * 1024;
const FEED_QUEUE: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaletteMode {
    Commands,
    Line,
    Tabs,
    Files,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaletteAction {
    Command(&'static str),
    /// Both numbers are one-based. The parent resolves/clamps to its document.
    GoTo {
        line: u64,
        column: u64,
    },
    ActivateTab(u64),
    /// Exact native identity, independent of any display/matching label.
    OpenFile(PathBuf),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaletteRow {
    pub title: Arc<str>,
    pub category: Arc<str>,
    pub action: PaletteAction,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tab {
    pub id: u64,
    pub title: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryError {
    TooLong,
    TooManyTabs,
    Stopped,
}

/// Queries and results are tagged with the model's generation and query text.
/// Each editing operation invalidates results, including reopening the same text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileQuery {
    pub generation: u64,
    pub query: Arc<str>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    pub path: PathBuf,
    /// Display/matching only. Never turn this string back into an opening path.
    pub label: Arc<str>,
}
impl FileEntry {
    pub fn new(path: PathBuf, label: Arc<str>) -> Self {
        Self { path, label }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileResults {
    pub generation: u64,
    pub query: Arc<str>,
    pub files: Vec<Arc<FileEntry>>,
    pub total: usize,
}

pub struct Palette {
    open: bool,
    query: String,
    cursor: usize,
    generation: u64,
    limit: usize,
    rows: Vec<PaletteRow>,
    selected: Option<usize>,
    tabs: Vec<Tab>,
    files: Option<Arc<FileResults>>,
    fresh: bool,
    matcher: SkimMatcherV2,
}

impl Palette {
    pub fn new(result_limit: usize) -> Self {
        Self {
            open: false,
            query: String::new(),
            cursor: 0,
            generation: 0,
            limit: result_limit.clamp(1, MAX_RESULTS),
            rows: Vec::new(),
            selected: None,
            tabs: Vec::new(),
            files: None,
            fresh: false,
            matcher: matcher(),
        }
    }
    pub fn is_open(&self) -> bool {
        self.open
    }
    pub fn query(&self) -> &str {
        &self.query
    }
    /// UTF-8 byte offset at a grapheme boundary, for the frontend's input caret.
    pub fn cursor(&self) -> usize {
        self.cursor
    }
    pub fn move_left(&mut self) {
        self.cursor = self.query[..self.cursor].grapheme_indices(true).next_back().map_or(0, |(at, _)| at);
    }
    pub fn move_right(&mut self) {
        self.cursor += self.query[self.cursor..].graphemes(true).next().map_or(0, str::len);
    }
    pub fn move_home(&mut self) {
        self.cursor = 0;
    }
    pub fn move_end(&mut self) {
        self.cursor = self.query.len();
    }
    pub fn rows(&self) -> &[PaletteRow] {
        &self.rows
    }
    pub fn selected(&self) -> Option<usize> {
        self.selected
    }
    /// False means cached file rows are visible but cannot yet be accepted.
    pub fn results_are_fresh(&self) -> bool {
        self.fresh
    }
    pub fn mode(&self) -> PaletteMode {
        match self.query.as_bytes().first() {
            Some(b'>') => PaletteMode::Commands,
            Some(b':') => PaletteMode::Line,
            Some(b'#') => PaletteMode::Tabs,
            _ => PaletteMode::Files,
        }
    }
    pub fn open(&mut self, query: &str) -> Result<Option<FileQuery>, QueryError> {
        let request = self.set_query(query)?;
        self.open = true;
        Ok(request)
    }
    /// Replaces the entire input. The returned request goes to `FileIndex::request`.
    /// For non-file modes call `FileIndex::cancel_query` to stop obsolete work.
    pub fn set_query(&mut self, query: &str) -> Result<Option<FileQuery>, QueryError> {
        if query.len() > MAX_QUERY_BYTES {
            return Err(QueryError::TooLong);
        }
        self.query.clear();
        self.query.push_str(query);
        self.cursor = self.query.len();
        Ok(self.changed())
    }
    pub fn insert(&mut self, text: &str) -> Result<Option<FileQuery>, QueryError> {
        if text.len() > MAX_QUERY_BYTES.saturating_sub(self.query.len()) {
            return Err(QueryError::TooLong);
        }
        self.query.insert_str(self.cursor, text);
        self.cursor += text.len();
        // Inserting a joiner/combining sequence can merge neighboring clusters.
        self.snap_cursor_forward();
        Ok(self.changed())
    }
    pub fn backspace(&mut self) -> Option<FileQuery> {
        if let Some((start, _)) = self.query[..self.cursor].grapheme_indices(true).next_back() {
            self.query.replace_range(start..self.cursor, "");
            self.cursor = start;
        }
        self.snap_cursor_forward();
        self.changed()
    }
    pub fn delete_forward(&mut self) -> Option<FileQuery> {
        if let Some(cluster) = self.query[self.cursor..].graphemes(true).next() {
            self.query.replace_range(self.cursor..self.cursor + cluster.len(), "");
        }
        self.snap_cursor_forward();
        self.changed()
    }
    fn snap_cursor_forward(&mut self) {
        self.cursor = self
            .query
            .grapheme_indices(true)
            .map(|(at, _)| at)
            .find(|at| *at >= self.cursor)
            .unwrap_or(self.query.len());
    }
    pub fn set_tabs(&mut self, tabs: Vec<Tab>) -> Result<(), QueryError> {
        if tabs.len() > MAX_TABS {
            return Err(QueryError::TooManyTabs);
        }
        if tabs.iter().any(|t| t.title.len() > MAX_QUERY_BYTES) {
            return Err(QueryError::TooLong);
        }
        self.tabs = tabs;
        if self.mode() == PaletteMode::Tabs {
            self.changed();
        }
        Ok(())
    }
    pub fn next(&mut self) {
        if !self.rows.is_empty() {
            self.selected = Some((self.selected.unwrap_or(0) + 1) % self.rows.len());
        }
    }
    pub fn previous(&mut self) {
        if !self.rows.is_empty() {
            self.selected = Some((self.selected.unwrap_or(0) + self.rows.len() - 1) % self.rows.len());
        }
    }
    pub fn accept(&mut self) -> Option<PaletteAction> {
        if !self.open || !self.fresh {
            return None;
        }
        let action = self.rows.get(self.selected?)?.action.clone();
        self.cancel();
        Some(action)
    }
    /// Also invalidate in-flight results. The parent calls `index.cancel_query()`.
    pub fn cancel(&mut self) {
        self.open = false;
        self.generation = self.generation.checked_add(1).expect("palette generation exhausted");
        self.fresh = false;
        self.rows.clear();
        self.selected = None;
    }
    pub fn apply_files(&mut self, result: Arc<FileResults>) -> bool {
        if !self.open
            || self.mode() != PaletteMode::Files
            || result.generation != self.generation
            || result.query.as_ref() != self.query
        {
            return false;
        }
        self.files = Some(result);
        self.file_rows();
        self.fresh = true;
        true
    }
    /// Prime first-open cached rows from the index while the palette is closed.
    /// This does not make any action selectable. Call on index events while closed
    /// and before `open`; subsequent keys retain the last displayed bounded rows.
    pub fn prime_files(&mut self, result: Arc<FileResults>) -> bool {
        if self.open {
            return false;
        }
        self.files = Some(result);
        true
    }
    fn file_rows(&mut self) {
        self.rows = self
            .files
            .as_ref()
            .into_iter()
            .flat_map(|r| r.files.iter())
            .take(self.limit)
            .map(|f| PaletteRow {
                title: f.label.clone(),
                category: "File".into(),
                action: PaletteAction::OpenFile(f.path.clone()),
            })
            .collect();
        self.selected =
            if self.rows.is_empty() { None } else { Some(self.selected.unwrap_or(0).min(self.rows.len() - 1)) };
    }
    fn changed(&mut self) -> Option<FileQuery> {
        self.generation = self.generation.checked_add(1).expect("palette generation exhausted");
        self.selected = None;
        self.fresh = true;
        let mut scored = Vec::new();
        match self.mode() {
            PaletteMode::Files => {
                self.fresh = false;
                self.file_rows();
                return Some(FileQuery { generation: self.generation, query: self.query.as_str().into() });
            }
            PaletteMode::Line => {
                self.rows.clear();
                if let Some((line, column)) = location(&self.query[1..]) {
                    self.rows.push(PaletteRow {
                        title: format!("Go to {line}:{column}").into(),
                        category: "Navigation".into(),
                        action: PaletteAction::GoTo { line, column },
                    });
                }
            }
            PaletteMode::Commands => {
                let query = &self.query[1..];
                for command in COMMANDS {
                    let label = format!("{} {} {}", command.id, command.title, command.category);
                    if let Some(score) = self.matcher.fuzzy_match(&label, query) {
                        keep_best(
                            &mut scored,
                            score,
                            PaletteRow {
                                title: command.title.into(),
                                category: command.category.into(),
                                action: PaletteAction::Command(command.id),
                            },
                            self.limit,
                        );
                    }
                }
                self.rows = scored.into_iter().map(|(_, row)| row).collect();
            }
            PaletteMode::Tabs => {
                for tab in &self.tabs {
                    if let Some(score) = self.matcher.fuzzy_match(&tab.title, &self.query[1..]) {
                        keep_best(
                            &mut scored,
                            score,
                            PaletteRow {
                                title: tab.title.as_str().into(),
                                category: "Tab".into(),
                                action: PaletteAction::ActivateTab(tab.id),
                            },
                            self.limit,
                        );
                    }
                }
                self.rows = scored.into_iter().map(|(_, row)| row).collect();
            }
        }
        self.selected = (!self.rows.is_empty()).then_some(0);
        None
    }
}

fn location(query: &str) -> Option<(u64, u64)> {
    let mut parts = query.split(':');
    fn positive(text: &str) -> Option<u64> {
        if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        text.parse().ok().filter(|n| *n > 0)
    }
    let line = positive(parts.next()?)?;
    let column = match parts.next() {
        Some(text) => positive(text)?,
        None => 1,
    };
    if parts.next().is_some() {
        return None;
    }
    Some((line, column))
}

fn matcher() -> SkimMatcherV2 {
    SkimMatcherV2::default().smart_case().element_limit(8192)
}

// Stable ties preserve registry, tab, or discovery order. Only the capped best
// list is stored, never a vector of all matches.
fn keep_best<T>(rows: &mut Vec<(i64, T)>, score: i64, row: T, limit: usize) {
    let at = rows.partition_point(|(old, _)| *old >= score);
    if at < limit {
        rows.insert(at, (score, row));
        if rows.len() > limit {
            rows.pop();
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct IndexLimits {
    pub max_files: usize,
    /// Combined native path and UTF-8 label bytes, reserved before enqueueing.
    pub max_path_bytes: usize,
    pub result_limit: usize,
}
impl Default for IndexLimits {
    fn default() -> Self {
        Self { max_files: 100_000, max_path_bytes: 32 * 1024 * 1024, result_limit: 32 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeedError {
    Cancelled,
    Capacity,
    PathTooLong,
}

struct Inbox {
    paths: VecDeque<FileEntry>,
    query: Option<FileQuery>,
    revision: u64,
    files: usize,
    bytes: usize,
    feed_cancelled: bool,
    stopped: bool,
    latest: Arc<FileResults>,
}
struct Shared {
    inbox: Mutex<Inbox>,
    ready: Condvar,
    space: Condvar,
    revision: AtomicU64,
    notified: AtomicBool,
    limits: IndexLimits,
}

/// A bounded producer for a parent-owned discovery worker. `push` may wait for
/// queue space, so never call it on the UI thread. `cancel`/`stop` wake producers.
pub struct FileFeed {
    shared: Arc<Shared>,
}
impl FileFeed {
    pub fn push(&mut self, path: PathBuf, label: Arc<str>) -> Result<(), FeedError> {
        let path_bytes = path.as_os_str().as_encoded_bytes().len();
        if label.len() > MAX_LABEL_BYTES || path_bytes > MAX_LABEL_BYTES {
            return Err(FeedError::PathTooLong);
        }
        let bytes = path_bytes + label.len();
        let mut inbox = self.shared.inbox.lock().unwrap();
        loop {
            if inbox.stopped || inbox.feed_cancelled {
                return Err(FeedError::Cancelled);
            }
            if inbox.files >= self.shared.limits.max_files
                || bytes > self.shared.limits.max_path_bytes.saturating_sub(inbox.bytes)
            {
                return Err(FeedError::Capacity);
            }
            if inbox.paths.len() < FEED_QUEUE {
                break;
            }
            inbox = self.shared.space.wait(inbox).unwrap();
        }
        inbox.files += 1;
        inbox.bytes += bytes;
        inbox.paths.push_back(FileEntry::new(path, label));
        drop(inbox);
        self.shared.ready.notify_one();
        Ok(())
    }
}

/// One matcher worker per workspace index, reused for every query. The caller
/// owns its lifecycle and must reuse it rather than spawning an index per key.
pub struct FileIndex {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}
impl FileIndex {
    /// Spawns one named worker; `notify` runs there and must only post an event.
    /// Wakes are coalesced until `latest()` acknowledges them. The callback must
    /// not block, call UI methods, panic, or recursively join this worker.
    pub fn new(mut limits: IndexLimits, notify: Arc<dyn Fn() + Send + Sync>) -> io::Result<Self> {
        limits.result_limit = limits.result_limit.clamp(1, MAX_RESULTS);
        let shared = Arc::new(Shared {
            inbox: Mutex::new(Inbox {
                paths: VecDeque::new(),
                query: None,
                revision: 0,
                files: 0,
                bytes: 0,
                feed_cancelled: false,
                stopped: false,
                latest: Arc::new(FileResults { generation: 0, query: "".into(), files: Vec::new(), total: 0 }),
            }),
            ready: Condvar::new(),
            space: Condvar::new(),
            revision: AtomicU64::new(0),
            notified: AtomicBool::new(false),
            limits,
        });
        let worker_shared = shared.clone();
        let worker =
            thread::Builder::new().name("kwil-palette".into()).spawn(move || run_index(worker_shared, notify))?;
        Ok(Self { shared, worker: Some(worker) })
    }
    pub fn feed(&self) -> FileFeed {
        FileFeed { shared: self.shared.clone() }
    }
    /// Replaces any pending query, invalidates matching immediately, and returns
    /// the previous capped cache without doing matching or I/O on the caller.
    pub fn request(&self, query: FileQuery) -> Result<Arc<FileResults>, QueryError> {
        if query.query.len() > MAX_QUERY_BYTES {
            return Err(QueryError::TooLong);
        }
        let mut inbox = self.shared.inbox.lock().unwrap();
        if inbox.stopped {
            return Err(QueryError::Stopped);
        }
        bump_revision(&self.shared, &mut inbox);
        inbox.query = Some(query);
        let cached = inbox.latest.clone();
        drop(inbox);
        self.shared.ready.notify_one();
        Ok(cached)
    }
    pub fn latest(&self) -> Arc<FileResults> {
        let inbox = self.shared.inbox.lock().unwrap();
        self.shared.notified.store(false, Ordering::Release);
        inbox.latest.clone()
    }
    /// Stops the current query while retaining the catalogue and discovery feed.
    pub fn cancel_query(&self) {
        let mut inbox = self.shared.inbox.lock().unwrap();
        bump_revision(&self.shared, &mut inbox);
        inbox.query = None;
        drop(inbox);
        self.shared.ready.notify_one();
    }
    /// Also cancels discovery feeds permanently. Cached indexed paths remain
    /// searchable; create a new index to switch workspace roots.
    pub fn cancel(&self) {
        self.cancel_query();
        self.shared.inbox.lock().unwrap().feed_cancelled = true;
        self.shared.space.notify_all();
    }
    /// Signals shutdown without waiting on the UI thread. Join the returned
    /// handle on the parent's shutdown worker. Dropping only signals shutdown.
    pub fn stop(&mut self) -> Option<JoinHandle<()>> {
        let mut inbox = self.shared.inbox.lock().unwrap();
        inbox.stopped = true;
        bump_revision(&self.shared, &mut inbox);
        drop(inbox);
        self.shared.ready.notify_all();
        self.shared.space.notify_all();
        self.worker.take()
    }
}
impl Drop for FileIndex {
    fn drop(&mut self) {
        self.stop();
    }
}

fn bump_revision(shared: &Shared, inbox: &mut Inbox) {
    inbox.revision = inbox.revision.checked_add(1).expect("index revision exhausted");
    shared.revision.store(inbox.revision, Ordering::Release);
}

fn run_index(shared: Arc<Shared>, notify: Arc<dyn Fn() + Send + Sync>) {
    let matcher = matcher();
    let mut paths: Vec<Arc<FileEntry>> = Vec::new();
    let mut seen_revision = 0;
    loop {
        let (batch, query, revision) = {
            let mut inbox = shared.inbox.lock().unwrap();
            while !inbox.stopped && inbox.paths.is_empty() && seen_revision == inbox.revision {
                inbox = shared.ready.wait(inbox).unwrap();
            }
            if inbox.stopped {
                return;
            }
            let batch = std::mem::take(&mut inbox.paths);
            (batch, inbox.query.clone(), inbox.revision)
        };
        shared.space.notify_all();
        paths.extend(batch.into_iter().map(Arc::new));
        seen_revision = revision;
        // Maintain a cheap default-file cache even before the first query. A
        // cancelled query also falls back to this cache without scanning names.
        let query = query.unwrap_or(FileQuery { generation: 0, query: "".into() });
        let Some(result) = match_files(&paths, &query, shared.limits.result_limit, &matcher, || {
            shared.revision.load(Ordering::Acquire) != revision
        }) else {
            continue;
        };
        if publish(&shared, revision, result) && !shared.notified.swap(true, Ordering::AcqRel) {
            notify();
        }
    }
}

fn publish(shared: &Shared, revision: u64, result: FileResults) -> bool {
    // Re-check while holding the same lock used by request/cancel so an old
    // worker can never overwrite a newer generation's published snapshot.
    let mut inbox = shared.inbox.lock().unwrap();
    if inbox.revision != revision || inbox.stopped {
        return false;
    }
    inbox.latest = Arc::new(result);
    true
}

fn match_files(
    paths: &[Arc<FileEntry>],
    query: &FileQuery,
    limit: usize,
    matcher: &SkimMatcherV2,
    cancelled: impl Fn() -> bool,
) -> Option<FileResults> {
    if cancelled() {
        return None;
    }
    let mut rows = Vec::new();
    let mut total = 0;
    if query.query.is_empty() {
        rows.extend(paths.iter().take(limit).map(|path| (0, path.clone())));
        total = paths.len();
    } else {
        for path in paths {
            if cancelled() {
                return None;
            }
            if let Some(score) = matcher.fuzzy_match(&path.label, &query.query) {
                total += 1;
                keep_best(&mut rows, score, path.clone(), limit);
            }
        }
    }
    Some(FileResults {
        generation: query.generation,
        query: query.query.clone(),
        files: rows.into_iter().map(|(_, path)| path).collect(),
        total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancellation_during_matching_discards_partial_snapshot() {
        let paths: Vec<_> =
            ["alpha", "alpine", "beta"].into_iter().map(|s| Arc::new(FileEntry::new(s.into(), s.into()))).collect();
        let checks = std::cell::Cell::new(0);
        let result = match_files(&paths, &FileQuery { generation: 1, query: "al".into() }, 2, &matcher(), || {
            checks.set(checks.get() + 1);
            checks.get() == 3
        });
        assert!(result.is_none());
        assert_eq!(checks.get(), 3, "cancel checked after matching began");
    }

    #[test]
    fn superseded_worker_cannot_publish_even_if_cancellation_arrives_after_scoring() {
        let mut index = FileIndex::new(IndexLimits::default(), Arc::new(|| {})).unwrap();
        index.request(FileQuery { generation: 10, query: "al".into() }).unwrap();
        let old_revision = index.shared.revision.load(Ordering::Acquire);
        let paths = vec![Arc::new(FileEntry::new("alpha".into(), "alpha".into()))];
        let result =
            match_files(&paths, &FileQuery { generation: 10, query: "al".into() }, 2, &matcher(), || false).unwrap();
        index.request(FileQuery { generation: 11, query: "beta".into() }).unwrap();
        assert!(!publish(&index.shared, old_revision, result));
        index.stop().unwrap().join().unwrap();
    }

    #[test]
    fn capped_snapshot_counts_all_matches_without_copying_catalogue() {
        let paths: Vec<_> = ["one.rs", "two.rs", "three.rs"]
            .into_iter()
            .map(|s| Arc::new(FileEntry::new(s.into(), s.into())))
            .collect();
        let result =
            match_files(&paths, &FileQuery { generation: 1, query: "rs".into() }, 2, &matcher(), || false).unwrap();
        assert_eq!(result.total, 3);
        assert_eq!(result.files.len(), 2);
        assert!(result.files.iter().all(|f| paths.iter().any(|p| Arc::ptr_eq(p, f))));
    }
}
