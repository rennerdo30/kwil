//! Shared GUI/TUI search host. Input submits immutable snapshots; worker wakes
//! drain bounded results. Typed actions are revalidated immediately before the
//! parent dispatcher changes the editor or opens a native path.
use crate::{Editor, SessionDocumentId};
pub use kwil_core_search::FileEvent;
pub use kwil_core_search::worker::{
    DocumentMatches, DocumentSearch, FileResults, INPUT_BYTES, Navigation, PAGE_HITS, Page, Query, REPLACE_BYTES,
    REPLACE_EDITS, ReplaceRecipe, ReplaceTarget, Replacement, ReplacementEdits, ReplacementIter,
    SEARCH_SOURCE_RELEASE_METADATA_BYTES, SIGNAL_BINS, SearchCloseError, SearchEvent, SearchInputsReleaseError,
    SearchInputsReleased, SearchKey, SearchRequest, SearchSourceReleaseKey, SearchTask, SearchUpdate, SearchWorker,
    SeedCursor, SeedRequest, SeedResult, SubmitError, WorkerSettlementReason,
};
use kwil_core_search::{FileSearch, FileSearchSummary, Match};
use std::{
    io,
    ops::{ControlFlow, Range},
    path::PathBuf,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchStatus {
    Idle,
    Pending,
    Searching,
    Complete,
    Cancelled,
    Failed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionMode {
    Replace,
    AddCursor,
}
#[derive(Debug)]
pub enum SearchAction {
    /// Apply every endpoint under this exact key before submitting the seeded
    /// query (which allocates a newer generation). Never restamp old endpoints.
    SeedSelections {
        key: SearchKey,
        cursors: Vec<SeedCursor>,
        primary: usize,
        /// Existing selection already has these endpoints: acknowledge without
        /// changing its desired columns or cursor generation.
        from_selection: bool,
    },
    Select {
        key: SearchKey,
        range: Range<u64>,
        mode: SelectionMode,
    },
    /// Native path plus original-file byte coordinates. Preview bytes may be
    /// decoded UTF-16 and must never be used to calculate the navigation range.
    OpenFile {
        key: SearchKey,
        path: PathBuf,
        range: Range<u64>,
        line_number: u64,
    },
    Replace(ReplaceRecipe),
}
impl SearchAction {
    pub fn key(&self) -> SearchKey {
        match self {
            Self::Select { key, .. } | Self::OpenFile { key, .. } | Self::SeedSelections { key, .. } => *key,
            Self::Replace(plan) => plan.key,
        }
    }
}

/// One visible page and one replacement recipe. File pages replace previous
/// pages instead of accumulating the entire tree/result stream on the UI.
pub struct SearchState {
    pub status: SearchStatus,
    pub key: Option<SearchKey>,
    pub first: u64,
    pub hits: Vec<Match>,
    pub files: Vec<FileEvent>,
    pub file_first: u64,
    pub files_seen: u64,
    pub file_summary: Option<FileSearchSummary>,
    pub file_results: Option<FileResults>,
    pub count: u64,
    pub scanned: u64,
    pub total: u64,
    pub signal: [u64; SIGNAL_BINS],
    pub navigation: Option<Match>,
    pub seed: Option<SeedResult>,
    pub error: Option<String>,
}
impl Default for SearchState {
    fn default() -> Self {
        Self {
            status: SearchStatus::Idle,
            key: None,
            first: 0,
            hits: Vec::new(),
            files: Vec::new(),
            file_first: 0,
            files_seen: 0,
            file_summary: None,
            file_results: None,
            count: 0,
            scanned: 0,
            total: 0,
            signal: [0; SIGNAL_BINS],
            navigation: None,
            seed: None,
            error: None,
        }
    }
}
pub struct SearchSession {
    worker: Option<SearchWorker>,
    generation: u64,
    state: SearchState,
    rejected: Option<SearchRequest>,
    replacement: Option<ReplaceRecipe>,
    files_counted_until: u64,
}
impl SearchSession {
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> io::Result<Self> {
        Ok(Self {
            worker: Some(SearchWorker::new(wake)?),
            generation: 0,
            state: SearchState::default(),
            rejected: None,
            replacement: None,
            files_counted_until: 0,
        })
    }
    pub fn state(&self) -> &SearchState {
        &self.state
    }
    pub fn key(&self) -> Option<SearchKey> {
        self.state.key
    }
    pub fn is_pending(&self) -> bool {
        matches!(self.state.status, SearchStatus::Pending | SearchStatus::Searching)
    }
    /// Current-document convenience builder. Cursors are metadata only; source
    /// capture is O(1). Empty in-selection produces zero hits, never global search.
    pub fn document_task(
        editor: &Editor,
        query: Query,
        in_selection: bool,
        page: Page,
        navigation: Option<Navigation>,
        replacement: Option<Replacement>,
    ) -> SearchTask {
        let snapshot = editor.document().snapshot();
        let ranges = if in_selection {
            editor.cursors().iter().map(|c| c.selection()).filter(|r| !r.is_empty()).collect()
        } else {
            std::iter::once(0..snapshot.len()).collect()
        };
        SearchTask::Document(DocumentSearch { snapshot, query, ranges, page, navigation, replacement })
    }
    /// O(1) source capture and bounded cursor metadata only. Untyped original
    /// bytes require an explicit trusted raw-view interpretation; a save target
    /// encoding is not evidence about immutable source origins.
    pub fn seed_task(editor: &Editor) -> SearchTask {
        Self::seed_task_with_raw_utf8(editor, false)
    }
    pub fn seed_task_with_raw_utf8(editor: &Editor, allow_raw_utf8: bool) -> SearchTask {
        SearchTask::SeedSelectionOrWord(SeedRequest {
            snapshot: editor.document().snapshot(),
            cursors: editor
                .cursors()
                .iter()
                .map(|cursor| SeedCursor { anchor: cursor.anchor, head: cursor.head })
                .collect(),
            primary: editor.primary_index(),
            allow_raw_utf8,
        })
    }
    /// The host retains a transient rejected request and retries it on the
    /// worker wake. If that slot cannot be retired, return ownership of the NEW
    /// request to the frontend (one bounded retry slot; never drop on input).
    pub fn submit(
        &mut self,
        document_id: SessionDocumentId,
        revision: u64,
        task: SearchTask,
    ) -> Result<SearchKey, SubmitError> {
        let generation = self.generation.checked_add(1).expect("search generation exhausted");
        let key = SearchKey { document_id: document_id.0, revision, generation };
        let file_results = match &task {
            SearchTask::FilePage { results, .. } => Some(results.clone()),
            _ => None,
        };
        let request = SearchRequest { key, task };
        let Some(worker) = &self.worker else {
            return Err(SubmitError { request: Box::new(request), reason: "search session stopped" });
        };
        if let Some(rejected) = self.rejected.take()
            && let Err(rejected) = worker.retire(rejected)
        {
            self.rejected = Some(*rejected);
            return Err(SubmitError {
                request: Box::new(request),
                reason: "host retry slot awaiting worker retirement",
            });
        }
        match worker.submit(request) {
            Ok(()) => {}
            Err(error)
                if error.reason == "search mailbox busy; retry on wake"
                    || error.reason == "search retirement full; retry on wake" =>
            {
                if !matches!(&error.request.task, SearchTask::FilePage { .. }) {
                    worker.cancel();
                }
                self.rejected = Some(*error.request);
            }
            Err(error) => return Err(error),
        }
        self.generation = generation;
        if file_results.is_none() {
            self.files_counted_until = 0;
        }
        let (count, file_summary) =
            if file_results.is_some() { (self.state.count, self.state.file_summary.take()) } else { (0, None) };
        self.state = SearchState {
            key: Some(key),
            status: SearchStatus::Pending,
            file_results,
            count,
            file_summary,
            ..SearchState::default()
        };
        self.replacement = None;
        Ok(key)
    }
    pub fn submit_files(&mut self, request: FileSearch) -> Result<SearchKey, SubmitError> {
        self.submit(SessionDocumentId(0), 0, SearchTask::Files(request))
    }
    /// Revisit completed results without retaining the tree or result list on
    /// the UI. While a scan is streaming, keep consuming its current page.
    pub fn request_file_page(&mut self, page: Page) -> Result<Option<SearchKey>, SubmitError> {
        if self.state.status != SearchStatus::Complete {
            return Ok(None);
        }
        let Some(results) = self.state.file_results.clone() else { return Ok(None) };
        self.submit(SessionDocumentId(0), 0, SearchTask::FilePage { results, page }).map(Some)
    }
    /// Call on an event-loop worker wake, with the active identity. A folder
    /// query has document 0 and is independent of which tab is active.
    pub fn worker_ready(&mut self, document: SessionDocumentId, revision: u64) -> bool {
        if self
            .key()
            .is_some_and(|key| key.document_id != 0 && (key.document_id != document.0 || key.revision != revision))
        {
            self.cancel();
            return true;
        }
        let Some(worker) = &self.worker else { return false };
        if let Some(request) = self.rejected.take() {
            if self.state.key != Some(request.key) {
                self.rejected = worker.retire(request).err().map(|request| *request);
            } else if let Err(error) = worker.submit(request) {
                self.rejected = Some(*error.request);
            }
        }
        let mut changed = false;
        // Finite drain prevents a fast producer monopolizing the input turn.
        for _ in 0..4 {
            let Some(event) = self.worker.as_ref().and_then(SearchWorker::take_event) else { break };
            if !self.accepts(event.key, document, revision) {
                continue;
            }
            changed = true;
            self.accept(event);
        }
        changed
    }
    fn accepts(&self, key: SearchKey, document: SessionDocumentId, revision: u64) -> bool {
        self.state.key == Some(key)
            && (key.document_id == 0 || key.document_id == document.0 && key.revision == revision)
    }
    fn accept(&mut self, event: SearchEvent) {
        if self.state.key != Some(event.key) {
            return;
        }
        match event.update {
            SearchUpdate::Seed(seed) => {
                self.state.status = SearchStatus::Complete;
                self.state.seed = seed;
            }
            SearchUpdate::Progress(progress) => {
                self.state.status = SearchStatus::Searching;
                self.state.count = progress.count;
                self.state.scanned = progress.scanned;
                self.state.total = progress.total;
            }
            SearchUpdate::Hits { first, hits } => {
                self.state.status = SearchStatus::Searching;
                self.state.first = first;
                self.state.hits = hits;
            }
            SearchUpdate::Document(result) => {
                self.state.status = SearchStatus::Complete;
                self.state.first = result.first;
                self.state.hits = result.hits;
                self.state.count = result.count;
                self.state.scanned = result.scanned;
                self.state.total = result.scanned;
                self.state.signal = result.signal;
                self.state.navigation = result.navigation;
                self.replacement = result.replacement;
            }
            SearchUpdate::Files { first, rows, results } => {
                self.state.status = SearchStatus::Searching;
                if self.state.file_summary.is_none() {
                    // Streaming pages contain disjoint result records. Count
                    // only newly observed hits, never error rows or revisits;
                    // FilesDone later supplies the exact complete count.
                    let hits = rows
                        .iter()
                        .enumerate()
                        .filter(|(index, row)| {
                            first.saturating_add(*index as u64) >= self.files_counted_until
                                && matches!(row, FileEvent::Hit(_))
                        })
                        .count() as u64;
                    self.state.count = self.state.count.saturating_add(hits);
                    self.files_counted_until = self.files_counted_until.max(first.saturating_add(rows.len() as u64));
                }
                self.state.files_seen = results.as_ref().map_or(first + rows.len() as u64, FileResults::len);
                self.state.file_results = results;
                self.state.file_first = first;
                self.state.files = rows;
            }
            SearchUpdate::FilesDone(summary) => {
                self.state.status = if summary.cancelled { SearchStatus::Cancelled } else { SearchStatus::Complete };
                self.state.count = summary.matching_lines;
                self.state.file_summary = Some(summary);
            }
            SearchUpdate::FilePageDone => self.state.status = SearchStatus::Complete,
            SearchUpdate::Failed(error) => {
                self.state.status = SearchStatus::Failed;
                self.state.error = Some(error);
            }
            SearchUpdate::Cancelled => self.state.status = SearchStatus::Cancelled,
        }
    }
    /// Cancel invalidates the generation so a queued completion/action cannot
    /// reappear after Escape. Worker owns pending snapshots until retirement.
    pub fn cancel(&mut self) {
        if let Some(worker) = &self.worker {
            worker.cancel();
        }
        self.state.status = SearchStatus::Cancelled;
        self.state.key = None;
        self.state.hits.clear();
        self.state.files.clear();
        self.state.file_results = None;
        self.state.navigation = None;
        self.state.seed = None;
        self.replacement = None;
        // A locally rejected request still belongs to this host. Retire it;
        // when full, retain until stop or a subsequent wake, never resubmit it.
        if let Some(request) = self.rejected.take()
            && let Some(worker) = &self.worker
        {
            self.rejected = worker.retire(request).err().map(|request| *request);
        }
    }
    pub fn selection_action(&self, row: usize, mode: SelectionMode) -> Option<SearchAction> {
        let hit = self.state.hits.get(row)?;
        Some(SearchAction::Select { key: self.key()?, range: hit.offset..hit.offset + hit.len, mode })
    }
    pub fn seed_action(&self) -> Option<SearchAction> {
        let seed = self.state.seed.as_ref()?;
        Some(SearchAction::SeedSelections {
            key: self.key()?,
            cursors: seed.cursors.clone(),
            primary: seed.primary,
            from_selection: seed.from_selection,
        })
    }
    pub fn navigation_action(&self, mode: SelectionMode) -> Option<SearchAction> {
        let hit = self.state.navigation?;
        Some(SearchAction::Select { key: self.key()?, range: hit.offset..hit.offset + hit.len, mode })
    }
    pub fn file_action(&self, row: usize, match_index: usize) -> Option<SearchAction> {
        let FileEvent::Hit(hit) = self.state.files.get(row)? else { return None };
        let range = hit.match_ranges.get(match_index)?;
        Some(SearchAction::OpenFile {
            key: self.key()?,
            path: hit.path.clone(),
            range: hit.line_offset.checked_add(range.start as u64)?..hit.line_offset.checked_add(range.end as u64)?,
            line_number: hit.line_number,
        })
    }
    /// Preflight only. The parent rechecks immediately at atomic admission.
    /// WouldBlock leaves the exact recipe queued; an admitted recipe is consumed
    /// only once. The dispatcher must retain an action if its later admission blocks.
    pub fn replacement_action(
        &mut self,
        document: SessionDocumentId,
        editor: &Editor,
    ) -> io::Result<Option<SearchAction>> {
        let Some(plan) = &self.replacement else { return Ok(None) };
        if !self.accepts(plan.key, document, editor.document().revision()) {
            self.replacement = None;
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "stale search replacement"));
        }
        if editor.has_pending_edit_receipt() {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "search replace awaits journal admission"));
        }
        Ok(self.replacement.take().map(SearchAction::Replace))
    }
    pub fn validate_action(&self, action: &SearchAction, document: SessionDocumentId, revision: u64) -> bool {
        self.accepts(action.key(), document, revision)
    }
    /// Nonblocking. The worker's reserved closing slot accepts the one retained
    /// rejected snapshot and destroys it on its own thread.
    pub fn stop(&mut self) {
        let Some(worker) = self.worker.as_ref() else { return };
        if let ControlFlow::Break(error) = worker.close_with_requests(self.rejected.take(), None) {
            self.rejected = error.internal;
        }
        // Keep the original worker and all facade pins for explicit handoff.
        // A stop request is not a join or native-right retirement.
    }
    /// Return a panel-owned rejected request for worker retirement. Backpressure
    /// returns the complete boxed owner; retain it until retry or shutdown.
    pub fn retire_request(&self, request: SearchRequest) -> Result<(), Box<SearchRequest>> {
        match &self.worker {
            Some(worker) => worker.retire(request),
            None => Err(Box::new(request)),
        }
    }
    /// Retain this ticket on the app shutdown lane if cleanup diagnostics or
    /// explicit retry are needed. Join only after releasing recipe/result pins.
    /// A document shutdown bundle must bind source_release_key BEFORE taking it.
    pub fn take_shutdown(&mut self) -> Option<SearchWorker> {
        self.take_shutdown_with(None).ok()
    }
    /// Metadata only: capture the original worker identity before taking its
    /// shutdown owner. No sync, fresh snapshot or source-bearing completion.
    pub fn source_release_key(&self) -> Option<SearchSourceReleaseKey> {
        self.worker.as_ref().map(SearchWorker::source_release_key)
    }
    /// Transfer the host retry slot AND one panel-owned source before close.
    /// If already stopped, return the complete external owner intact.
    pub fn take_shutdown_with(
        &mut self,
        external: Option<SearchRequest>,
    ) -> Result<SearchWorker, Option<Box<SearchRequest>>> {
        let Some(worker) = self.worker.as_ref() else {
            return Err(external.map(Box::new));
        };
        if let ControlFlow::Break(error) = worker.close_with_requests(self.rejected.take(), external) {
            // Keep the original facade and exact internal retry owner together;
            // the existing public refusal returns the exact external owner.
            self.rejected = error.internal;
            return Err(error.external.map(Box::new));
        }
        self.state.file_results = None;
        self.replacement = None;
        Ok(self.worker.take().expect("search worker retained through close admission"))
    }
}
impl Drop for SearchSession {
    fn drop(&mut self) {
        self.stop();
        if self.worker.is_some() {
            // Misuse quarantine retains source/spool pins and the exact worker;
            // it performs no source/native I/O and is not successful shutdown.
            std::mem::forget(self.worker.take());
            std::mem::forget(self.rejected.take());
            std::mem::forget(self.state.file_results.take());
            std::mem::forget(self.replacement.take());
        }
    }
}

#[cfg(test)]
#[path = "search_session_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "focusedsearch_source_release_tests.rs"]
mod source_release_tests;
