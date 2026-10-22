//! Open views retain their own cursor, undo history and viewport. Worker
//! completions address a document ID, never whichever tab happens to be active.
use crate::document_worker::{DocumentRequest, DocumentResult, DocumentWorker};
use crate::{CommandStatus, Damage, Editor, View};
use kwil_core_workspace::session::{DocumentId, Session};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io,
    ops::{Deref, DerefMut},
    path::PathBuf,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TabArea {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}
impl TabArea {
    pub fn contains(self, x: usize, y: usize) -> bool {
        x >= self.x && y >= self.y && x < self.x.saturating_add(self.width) && y < self.y.saturating_add(self.height)
    }
}
#[derive(Clone, Copy, Debug)]
pub struct TabMetrics {
    pub width: usize,
    pub height: usize,
    pub cell_width: usize,
    pub cell_height: usize,
    pub scrollbar: usize,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabAction {
    Activate(DocumentId),
    Close(DocumentId),
    PreviousPage,
    NextPage,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabSlot {
    pub document: DocumentId,
    pub area: TabArea,
    pub close: TabArea,
    pub active: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabGeometry {
    pub body: TabArea,
    pub chrome: TabArea,
    pub text: TabArea,
    pub previous: TabArea,
    pub next: TabArea,
    pub slots: Vec<TabSlot>,
    pub first: usize,
    pub capacity: usize,
    pub total: usize,
    pub cell_height: usize,
}
impl TabGeometry {
    pub fn hit(&self, x: usize, y: usize) -> Option<TabAction> {
        if self.first > 0 && self.previous.contains(x, y) {
            return Some(TabAction::PreviousPage);
        }
        if self.capacity > 0 && self.first + self.capacity < self.total && self.next.contains(x, y) {
            return Some(TabAction::NextPage);
        }
        self.slots.iter().find(|slot| slot.area.contains(x, y)).map(|slot| {
            if slot.close.contains(x, y) { TabAction::Close(slot.document) } else { TabAction::Activate(slot.document) }
        })
    }
    /// Physical source/destination rectangles; only editor pixels participate.
    pub fn scroll_copy(&self, delta: i64) -> Option<(TabArea, TabArea)> {
        let dy = (delta.unsigned_abs() as usize).saturating_mul(self.cell_height);
        if delta == 0 || dy >= self.text.height || self.text.width == 0 {
            return None;
        }
        let source =
            TabArea { y: self.text.y + if delta > 0 { dy } else { 0 }, height: self.text.height - dy, ..self.text };
        let target = TabArea { y: self.text.y + if delta < 0 { dy } else { 0 }, ..source };
        Some((source, target))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilePromptKind {
    Open,
    SaveAs,
    Recent,
}

pub struct FilePrompt {
    /// Distinguishes a new prompt from a cancelled prompt for the same revision.
    pub generation: u64,
    pub kind: FilePromptKind,
    pub document: DocumentId,
    pub revision: u64,
    pub text: String,
}

struct SaveFlight {
    revision: u64,
    path: Option<PathBuf>,
}

#[derive(Default)]
struct FileFlow {
    worker: Option<DocumentWorker>,
    queue: VecDeque<DocumentRequest>,
    saves: HashMap<DocumentId, SaveFlight>,
    save_all: VecDeque<DocumentId>,
    close: VecDeque<DocumentId>,
    quit: bool,
    exit: bool,
    prompt: Option<FilePrompt>,
    next_prompt: u64,
    preparing: bool,
    storage_ready: bool,
    persist_inflight: bool,
    persist_pending: bool,
    persist_timer: bool,
    deferred_recovery: Option<Box<crate::document_worker::PreparedWorkspace>>,
    observed: HashMap<DocumentId, (u64, u64)>,
    next_observer: u64,
    seeded: HashSet<DocumentId>,
    durable: HashMap<DocumentId, (u64, u64)>,
    journal_failed: HashMap<DocumentId, String>,
    metadata_durable: bool,
    restore_session: Option<bool>,
    hot_exit: bool,
    manual_guard: bool,
    exit_without_metadata: bool,
}

pub struct Workspace {
    input_epoch: u64,
    session: Session,
    views: HashMap<DocumentId, View>,
    closed_views: HashMap<DocumentId, View>,
    files: FileFlow,
}

impl Workspace {
    pub fn new(view: View) -> io::Result<Self> {
        let mut workspace = Self {
            input_epoch: 0,
            session: Session::default(),
            views: HashMap::new(),
            closed_views: HashMap::new(),
            files: FileFlow::default(),
        };
        workspace.open(view)?;
        Ok(workspace)
    }

    pub fn session(&self) -> &Session {
        &self.session
    }
    pub fn recovery_ready(&self) -> bool {
        self.files.storage_ready
    }
    /// Captured input must be cancelled when startup recovery replaces identities
    /// or a tab is closed and its identity can subsequently be reopened.
    pub fn input_epoch(&self) -> u64 {
        self.input_epoch
    }
    pub fn horizontal_origin(&self) -> u64 {
        self.deref().first_byte_column
    }
    pub fn active_id(&self) -> DocumentId {
        self.session.active().expect("workspace always has a tab")
    }
    pub fn view(&self, id: DocumentId) -> Option<&View> {
        self.views.get(&id)
    }
    pub fn view_mut(&mut self, id: DocumentId) -> Option<&mut View> {
        self.views.get_mut(&id)
    }

    pub fn tab_title(&self, id: DocumentId) -> String {
        let Some(tab) = self.session.tabs().iter().find(|t| t.id == id) else { return String::new() };
        let name = tab
            .path
            .as_ref()
            .and_then(|path| path.file_name())
            .map_or_else(|| format!("Untitled {}", id.0), |name| name.to_string_lossy().into_owned());
        let dirty = self
            .views
            .get(&id)
            .and_then(View::editor)
            .map_or(tab.revision != tab.saved_revision, |editor| editor.document().is_dirty());
        if dirty { format!("* {name}") } else { name }
    }

    pub fn tab_geometry(&self, metrics: TabMetrics, layout: kwil_core_commands::settings::TabLayout) -> TabGeometry {
        use kwil_core_commands::settings::TabLayout;
        let TabMetrics { width, height, cell_width, cell_height, scrollbar } = metrics;
        let (cw, ch) = (cell_width.max(1), cell_height.max(1));
        let content_height = height.saturating_sub(ch);
        let mut result = TabGeometry {
            body: TabArea { width, height: content_height, ..Default::default() },
            chrome: TabArea::default(),
            text: TabArea::default(),
            previous: TabArea::default(),
            next: TabArea::default(),
            slots: Vec::new(),
            first: 0,
            capacity: 0,
            total: self.session.tabs().len(),
            cell_height: ch,
        };
        let (available, slot_width, slot_height, start_x, start_y) = match layout {
            TabLayout::Horizontal => {
                let header = ch.min(content_height);
                result.chrome = TabArea { width, height: header, ..Default::default() };
                result.body.y = header;
                result.body.height = content_height - header;
                let nav = cw.saturating_mul(2).min(width / 2);
                result.previous = TabArea { width: nav, height: header, ..Default::default() };
                result.next = TabArea { x: width - nav, width: nav, height: header, ..Default::default() };
                let available = width.saturating_sub(2 * nav);
                let count = (available / cw.saturating_mul(20)).max(1);
                (if available == 0 || header == 0 { 0 } else { count }, available / count, header, nav, 0)
            }
            TabLayout::Vertical => {
                let side = cw.saturating_mul(26).min(width / 3);
                result.chrome = TabArea { width: side, height: content_height, ..Default::default() };
                result.body.x = side;
                result.body.width = width - side;
                result.previous = TabArea { width: side / 2, height: ch.min(content_height), ..Default::default() };
                result.next = TabArea {
                    x: side / 2,
                    width: side - side / 2,
                    height: ch.min(content_height),
                    ..Default::default()
                };
                (if side == 0 { 0 } else { content_height.saturating_sub(ch) / ch }, side, ch, 0, ch)
            }
        };
        result.capacity = available.min(kwil_core_workspace::session::MAX_TABS);
        let active = self.session.tabs().iter().position(|t| t.id == self.active_id()).unwrap_or(0);
        if let Some(page) = active.checked_div(result.capacity) {
            result.first = page * result.capacity;
            for (index, tab) in self.session.tabs().iter().enumerate().skip(result.first).take(result.capacity) {
                let local = index - result.first;
                let area = TabArea {
                    x: start_x + if layout == TabLayout::Horizontal { local * slot_width } else { 0 },
                    y: start_y + if layout == TabLayout::Vertical { local * slot_height } else { 0 },
                    width: slot_width,
                    height: slot_height,
                };
                let close_width = cw.saturating_mul(3).min(area.width);
                let close = TabArea { x: area.x + area.width - close_width, width: close_width, ..area };
                result.slots.push(TabSlot { document: tab.id, area, close, active: tab.id == self.active_id() });
            }
        }
        result.text = TabArea {
            x: result.body.x,
            y: result.body.y,
            width: result.body.width.saturating_sub(scrollbar),
            height: result.body.height.saturating_sub(scrollbar) / ch * ch,
        };
        result
    }

    pub fn tab_action(&mut self, action: TabAction, geometry: &TabGeometry) -> io::Result<()> {
        match action {
            TabAction::Activate(id) => {
                self.cancel_file_prompt();
                self.activate(id);
            }
            TabAction::Close(id) => {
                self.cancel_file_prompt();
                self.files.close = [id].into();
                self.advance_close(false)?;
            }
            TabAction::PreviousPage | TabAction::NextPage => {
                let index = if action == TabAction::PreviousPage {
                    geometry.first.saturating_sub(geometry.capacity)
                } else {
                    (geometry.first + geometry.capacity).min(geometry.total.saturating_sub(1))
                };
                if let Some(tab) = self.session.tabs().get(index) {
                    let id = tab.id;
                    self.cancel_file_prompt();
                    self.activate(id);
                }
            }
        }
        self.checkpoint_session();
        Ok(())
    }

    pub fn open(&mut self, mut view: View) -> io::Result<DocumentId> {
        if self.session.active().is_some() {
            self.sync_active();
        }
        if let Some(path) = &view.path {
            let path = std::path::absolute(path)?;
            view.path = Some(path.clone());
            if let Some(editor) = view.editor_mut() {
                editor.document.path = Some(path);
            }
        }
        let id = self.session.open(view.path.clone())?;
        // Reopening an already-open path activates its existing unsaved view.
        self.views.entry(id).or_insert(view);
        Ok(id)
    }

    pub fn activate(&mut self, id: DocumentId) -> bool {
        self.sync_active();
        self.session.activate(id)
    }

    pub fn cycle(&mut self, backwards: bool) -> Damage {
        self.cancel_file_prompt();
        self.sync_active();
        let tabs = self.session.tabs();
        let current = tabs.iter().position(|tab| tab.id == self.active_id()).unwrap_or(0);
        let next = if backwards { (current + tabs.len() - 1) % tabs.len() } else { (current + 1) % tabs.len() };
        self.session.activate(tabs[next].id);
        self.checkpoint_session();
        Damage { full: true, status: true, ..Default::default() }
    }

    /// Refuse dirty close until the caller saves or explicitly chooses discard.
    pub fn close(&mut self, id: DocumentId, discard: bool) -> io::Result<bool> {
        if self.files.saves.contains_key(&id) {
            return Err(io::Error::other("Save in progress; wait before closing this tab"));
        }
        let Some(view) = self.views.get(&id) else { return Ok(false) };
        let dirty = view.editor().map_or_else(
            || self.session.tabs().iter().find(|t| t.id == id).is_some_and(|t| t.revision != t.saved_revision),
            |editor| editor.document().is_dirty(),
        );
        if !discard && dirty {
            return Err(io::Error::other("Unsaved changes: save or explicitly discard before closing"));
        }
        if self.session.closed().len() == kwil_core_workspace::session::MAX_CLOSED
            && self.files.storage_ready
            && !self.files.metadata_durable
        {
            // An eviction must be proven by prior durable closed metadata before
            // the worker may retire its journal. Preserve intent until then.
            self.checkpoint_session();
            self.persist_snapshot();
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Preserving closed-tab history; repeat close after recovery finishes",
            ));
        }
        // Prepare the replacement before mutating the last tab.
        self.sync_active();
        if discard && let Some(tab) = self.session.tab_mut(id) {
            tab.saved_revision = tab.revision;
        }
        if !self.session.close(id) {
            return Err(io::Error::other(
                "Closed-tab history contains unsaved work; recover it before closing another tab",
            ));
        }
        if let Some(view) = self.views.remove(&id) {
            self.input_epoch = self.input_epoch.wrapping_add(1);
            self.closed_views.insert(id, view);
        }
        self.files.queue.retain(|request| !matches!(request, DocumentRequest::Open { document, .. } | DocumentRequest::Index { document, .. } if *document == id));
        self.closed_views.retain(|id, _| self.session.closed().iter().any(|tab| tab.id == *id));
        let generation = self.files.observed.remove(&id).map_or(0, |(_, generation)| generation);
        self.files.observed.retain(|id, _| self.views.contains_key(id) || self.closed_views.contains_key(id));
        self.files.seeded.retain(|id| self.views.contains_key(id) || self.closed_views.contains_key(id));
        self.files.durable.retain(|id, _| self.views.contains_key(id) || self.closed_views.contains_key(id));
        self.files.journal_failed.retain(|id, _| self.views.contains_key(id) || self.closed_views.contains_key(id));
        self.files
            .queue
            .retain(|request| !matches!(request,DocumentRequest::FinishJournal {document,..} if *document==id));
        self.files.queue.push_back(DocumentRequest::FinishJournal { document: id, generation });
        if self.views.is_empty() {
            self.open(View::new(None))?;
        }
        Ok(true)
    }

    pub fn closed_path(&self) -> Option<PathBuf> {
        self.session.closed().iter().find_map(|tab| tab.path.clone())
    }

    pub fn sync_active(&mut self) {
        let id = self.active_id();
        let Some(view) = self.views.get(&id) else { return };
        if let Some(tab) = self.session.tab_mut(id) {
            if tab.path != view.path {
                tab.path.clone_from(&view.path);
            }
            tab.first_visible_line = view.top();
            tab.first_byte_column = view.first_byte_column;
            if let Some(editor) = view.editor() {
                tab.revision = editor.document().revision();
                tab.saved_revision = editor.document().saved_revision();
                tab.cursor = editor.primary_cursor().head;
                tab.anchor = editor.primary_cursor().anchor;
            }
        }
    }

    pub fn sync_all(&mut self) {
        for tab in self.session.tabs().iter().map(|t| t.id).collect::<Vec<_>>() {
            if let (Some(view), Some(metadata)) = (self.views.get(&tab), self.session.tab_mut(tab)) {
                if metadata.path != view.path {
                    metadata.path.clone_from(&view.path);
                }
                metadata.first_visible_line = view.top();
                metadata.first_byte_column = view.first_byte_column;
                if let Some(editor) = view.editor() {
                    metadata.revision = editor.document().revision();
                    metadata.saved_revision = editor.document().saved_revision();
                    metadata.cursor = editor.primary_cursor().head;
                    metadata.anchor = editor.primary_cursor().anchor;
                }
            }
        }
    }

    /// Start only after the first frame. Every callback is already addressed.
    pub fn start_document_worker(&mut self, complete: impl Fn(DocumentResult) + Send + 'static) -> io::Result<()> {
        self.start_document_worker_at(None, complete)
    }

    /// Hidden/native harnesses require a caller-selected absolute root. Missing
    /// and malformed overrides reach the worker as invalid explicit paths.
    pub fn start_isolated_document_worker(
        &mut self,
        complete: impl Fn(DocumentResult) + Send + 'static,
    ) -> io::Result<()> {
        let root = std::env::var_os("KWIL_WORKSPACE_ROOT").map(PathBuf::from).unwrap_or_default();
        self.start_document_worker_at(Some(root), complete)
    }

    /// Tests and embedders can select an isolated recovery location explicitly.
    pub fn start_document_worker_at(
        &mut self,
        root: Option<PathBuf>,
        complete: impl Fn(DocumentResult) + Send + 'static,
    ) -> io::Result<()> {
        if self.files.worker.is_some() {
            return Ok(());
        }
        self.files.worker = Some(DocumentWorker::spawn(complete)?);
        self.files
            .worker
            .as_ref()
            .unwrap()
            .try_submit(DocumentRequest::Prepare { root })
            .map_err(|_| io::Error::other("Recovery admission failed"))?;
        self.files.preparing = true;
        Ok(())
    }

    fn queue_initial_indexes(&mut self) {
        for tab in self.session.tabs() {
            let view = &self.views[&tab.id];
            if view.editor().is_none()
                && let (Some(path), Some(file)) = (&view.path, view.file())
            {
                self.files.queue.push_back(DocumentRequest::Index {
                    document: tab.id,
                    revision: tab.revision,
                    path: path.clone(),
                    file: file.clone(),
                });
            }
        }
        self.pump_files();
    }

    fn pump_files(&mut self) {
        if self.files.preparing {
            return;
        }
        if self.files.worker.is_none() {
            return;
        }
        while let Some(request) = self.files.queue.pop_front() {
            match self.files.worker.as_ref().unwrap().try_submit(request) {
                Ok(()) => {}
                Err(std::sync::mpsc::TrySendError::Full(request)) => {
                    self.files.queue.push_front(*request);
                    break;
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(request)) => {
                    if let DocumentRequest::Save { document, .. } = *request {
                        self.files.saves.remove(&document);
                        if let Some(editor) = self.views.get_mut(&document).and_then(View::editor_mut) {
                            let _ = editor.finish_save(Err(io::Error::other("document worker disconnected")));
                        }
                    }
                    self.cancel_close_intent();
                    self.set_notice("Document worker disconnected; changes remain unsaved".into());
                }
            }
        }
    }

    /// Cheap metadata snapshot. Coalesce writes while one worker write is active.
    pub fn checkpoint_session(&mut self) {
        self.install_journal_observers();
        self.sync_all();
        self.files.persist_pending = true;
        self.files.metadata_durable = false;
        if self.files.storage_ready && !self.files.persist_inflight {
            if self.files.hot_exit || (self.files.quit && self.files.exit) {
                self.persist_snapshot();
            } else if !self.files.persist_timer {
                self.files.persist_timer = true;
                self.files.queue.push_back(DocumentRequest::SessionTimer);
                self.pump_files();
            }
        }
    }

    fn persist_snapshot(&mut self) {
        if !self.files.storage_ready || self.files.persist_inflight {
            return;
        }
        self.files.persist_pending = false;
        self.files.persist_inflight = true;
        self.files.queue.push_back(DocumentRequest::Persist(self.session.clone()));
        self.pump_files();
    }

    pub fn set_session_restore(&mut self, enabled: bool) {
        self.files.restore_session = Some(enabled);
    }

    fn install_journal_observers(&mut self) {
        if !self.files.storage_ready {
            return;
        }
        let Some(worker) = &self.files.worker else { return };
        for (&document, view) in &mut self.views {
            if self.files.observed.contains_key(&document) {
                continue;
            }
            let Some(editor) = view.editor_mut() else { continue };
            let next = editor.next_edit_sequence();
            self.files.next_observer = self.files.next_observer.checked_add(1).expect("observer generation exhausted");
            let generation = self.files.next_observer;
            let path = editor.document().path().map(PathBuf::from);
            let observer = worker.observer(document, path.clone(), next, generation);
            if let Err(error) = editor.set_edit_observer(Some(observer)) {
                view.set_notice(format!("Recovery observer unavailable: {error}"));
                continue;
            }
            self.files.observed.insert(document, (next, generation));
            if editor.document().is_dirty() {
                self.files.seeded.insert(document);
                self.files.queue.retain(
                    |request| !matches!(request,DocumentRequest::SeedJournal {document:old,..} if *old==document),
                );
                self.files.queue.push_back(DocumentRequest::SeedJournal {
                    document,
                    revision: editor.document().revision(),
                    snapshot: editor.document().snapshot(),
                    path,
                    next_sequence: next,
                    generation,
                });
            }
        }
    }

    pub fn file_prompt(&self) -> Option<&FilePrompt> {
        self.files.prompt.as_ref()
    }
    pub fn file_prompt_mut(&mut self) -> Option<&mut FilePrompt> {
        self.files.prompt.as_mut()
    }
    pub fn wants_exit(&self) -> bool {
        self.files.exit
            && (self.files.exit_without_metadata || !self.files.storage_ready || self.files.metadata_durable)
    }
    pub fn close_pending(&self) -> bool {
        self.files.quit || !self.files.close.is_empty()
    }
    pub fn cancel_close_intent(&mut self) {
        self.files.close.clear();
        self.files.quit = false;
        self.files.exit = false;
        self.files.hot_exit = false;
        self.files.manual_guard = false;
        self.files.exit_without_metadata = false;
    }
    pub fn cancel_file_prompt(&mut self) {
        self.files.prompt = None;
        self.files.save_all.clear();
        self.cancel_close_intent();
    }
    pub fn prompt_notice(&mut self) -> Damage {
        let notice = self
            .files
            .prompt
            .as_ref()
            .map(|p| {
                let label = match p.kind {
                    FilePromptKind::Open => "Open",
                    FilePromptKind::SaveAs => "Save as",
                    FilePromptKind::Recent => "Recent (number)",
                };
                format!("{label}: {}  (Enter to submit, Esc to cancel)", p.text)
            })
            .unwrap_or_default();
        self.set_notice(notice)
    }
    pub fn submit_file_prompt(&mut self) -> io::Result<()> {
        // Blank submissions retain the prompt, destination and close intent.
        if self.files.prompt.as_ref().is_none_or(|p| p.text.trim().is_empty()) {
            return Ok(());
        }
        let prompt = self.files.prompt.take().unwrap();
        match prompt.kind {
            FilePromptKind::Open => {
                self.open_path(prompt.text.into())?;
            }
            FilePromptKind::Recent => {
                let number = prompt.text.trim().parse::<usize>().ok().and_then(|n| n.checked_sub(1));
                let path = number
                    .and_then(|n| self.session.recent().get(n))
                    .cloned()
                    .ok_or_else(|| io::Error::other("Choose a recent file number"));
                match path {
                    Ok(path) => {
                        self.open_path(path)?;
                    }
                    Err(error) => {
                        self.files.prompt = Some(prompt);
                        return Err(error);
                    }
                }
            }
            FilePromptKind::SaveAs => {
                if self
                    .views
                    .get(&prompt.document)
                    .and_then(View::editor)
                    .is_none_or(|e| e.document().revision() != prompt.revision)
                {
                    self.cancel_close_intent();
                    return Err(io::Error::other("Document changed while naming it; save again"));
                }
                self.save_document(prompt.document, Some(prompt.text.into()))?;
            }
        }
        Ok(())
    }

    pub fn open_path(&mut self, path: PathBuf) -> io::Result<DocumentId> {
        if self.files.queue.len() >= 256 {
            return Err(io::Error::other("File worker queue is full; retry after current work finishes"));
        }
        let path = std::path::absolute(path)?;
        if let Some(id) = self.files.saves.iter().find(|(_, save)| save.path.as_ref() == Some(&path)).map(|(id, _)| *id)
        {
            self.activate(id);
            return Ok(id);
        }
        if let Some(id) = self.session.tabs().iter().find(|t| t.path.as_ref() == Some(&path)).map(|t| t.id) {
            self.activate(id);
            return Ok(id);
        }
        self.cancel_file_prompt();
        self.sync_active();
        let id = self.session.open(Some(path.clone()))?;
        let mut view = View::new(None);
        view.editor = None;
        view.path = Some(path.clone());
        view.set_notice("Opening…".into());
        self.views.insert(id, view);
        self.files.queue.push_back(DocumentRequest::Open { document: id, revision: 0, path });
        self.pump_files();
        self.checkpoint_session();
        Ok(id)
    }

    /// App-owned CLI/IPC parsing feeds already validated native paths here.
    pub fn open_files(&mut self, request: kwil_core_workspace::instance::OpenFiles) -> io::Result<()> {
        if request.files.len() > kwil_core_workspace::instance::MAX_OPEN_FILES {
            return Err(io::Error::other("Too many open requests"));
        }
        for file in request.files {
            let id = self.open_path(file.path)?;
            if let Some(line) = file.line {
                let view = self.views.get_mut(&id).unwrap();
                view.execute(
                    "navigation.goToLine",
                    &crate::CommandArgs { line: Some(line), column: file.column, ..Default::default() },
                    1,
                )?;
            }
        }
        Ok(())
    }

    pub fn save_document(&mut self, id: DocumentId, path: Option<PathBuf>) -> io::Result<()> {
        if self.files.queue.len() >= 256 {
            self.cancel_close_intent();
            return Err(io::Error::other("File worker queue is full; retry save"));
        }
        if self.files.saves.contains_key(&id) {
            self.cancel_close_intent();
            return Err(io::Error::other("save already in progress"));
        }
        let path = path.map(std::path::absolute).transpose()?;
        if let Some(path) = &path
            && (self.session.tabs().iter().any(|tab| tab.id != id && tab.path.as_ref() == Some(path))
                || self.files.saves.iter().any(|(document, save)| *document != id && save.path.as_ref() == Some(path)))
        {
            self.cancel_close_intent();
            return Err(io::Error::other("Save As target is open in another tab"));
        }
        let editor = self
            .views
            .get_mut(&id)
            .and_then(View::editor_mut)
            .ok_or_else(|| io::Error::other("Indexing: editing will be available when ready"))?;
        if path.is_none() && editor.document().path().is_none() {
            self.files.next_prompt = self.files.next_prompt.wrapping_add(1);
            self.files.prompt = Some(FilePrompt {
                generation: self.files.next_prompt,
                kind: FilePromptKind::SaveAs,
                document: id,
                revision: editor.document().revision(),
                text: String::new(),
            });
            self.activate(id);
            self.prompt_notice();
            return Ok(());
        }
        let request = match editor.save_request(path) {
            Ok(request) => request,
            Err(error) => {
                self.cancel_close_intent();
                return Err(error);
            }
        };
        self.files.saves.insert(id, SaveFlight { revision: request.revision, path: Some(request.path.clone()) });
        self.files.queue.push_back(DocumentRequest::Save { document: id, request });
        if let Some(view) = self.views.get_mut(&id) {
            view.set_notice("Saving…".into());
        }
        self.pump_files();
        Ok(())
    }

    /// Commands are intercepted before Vim/editor dispatch, preserving keymap tombstones.
    pub fn file_command(&mut self, command: &str) -> io::Result<bool> {
        match command {
            "file.new" => {
                self.cancel_file_prompt();
                self.open(View::new(None))?;
            }
            "file.open" | "file.recent" | "file.saveAs" => {
                let id = self.active_id();
                let revision = self.editor().map_or(0, |e| e.document().revision());
                let kind = match command {
                    "file.open" => FilePromptKind::Open,
                    "file.recent" => FilePromptKind::Recent,
                    _ => FilePromptKind::SaveAs,
                };
                self.files.next_prompt = self.files.next_prompt.wrapping_add(1);
                self.files.prompt = Some(FilePrompt {
                    generation: self.files.next_prompt,
                    kind,
                    document: id,
                    revision,
                    text: String::new(),
                });
                if kind == FilePromptKind::Recent {
                    let recent = self
                        .session
                        .recent()
                        .iter()
                        .enumerate()
                        .map(|(i, p)| format!("{}: {}", i + 1, p.display()))
                        .collect::<Vec<_>>()
                        .join(" | ");
                    self.set_notice(format!("Recent: {recent}  (type number, Enter; Esc cancels)"));
                } else {
                    self.prompt_notice();
                }
            }
            "file.save" => {
                let id = self.files.close.front().copied().unwrap_or(self.active_id());
                self.save_document(id, None)?;
            }
            "file.saveAll" => {
                self.sync_all();
                self.files.save_all =
                    self.session.tabs().iter().filter(|t| t.revision != t.saved_revision).map(|t| t.id).collect();
                self.next_save_all()?;
            }
            "file.nextTab" => {
                self.cycle(false);
            }
            "file.previousTab" => {
                self.cycle(true);
            }
            "file.close" => {
                self.files.quit = false;
                self.files.close = [self.active_id()].into();
                self.advance_close(false)?;
            }
            "file.closeAll" => {
                self.files.quit = false;
                self.files.close = self.session.tabs().iter().map(|t| t.id).collect();
                self.advance_close(false)?;
            }
            "file.quit" => {
                self.request_window_close(false)?;
            }
            "file.saveAndQuit" => {
                self.files.quit = true;
                self.files.close = self.session.tabs().iter().map(|t| t.id).collect();
                self.save_document(self.active_id(), None)?;
            }
            "file.restoreSession" => {
                if let Some(prepared) = self.files.deferred_recovery.take() {
                    self.install_prepared(*prepared);
                    self.files.storage_ready = self.files.deferred_recovery.is_none();
                }
            }
            "file.reopenClosed" => {
                self.cancel_file_prompt();
                self.sync_active();
                if let Some(id) = self.session.reopen_closed()? {
                    if let Some(view) = self.closed_views.remove(&id) {
                        self.views.insert(id, view);
                        if self.views[&id].editor().is_none() {
                            let tab = self.session.tabs().iter().find(|tab| tab.id == id).unwrap().clone();
                            self.files.queue.retain(|request|!matches!(request,DocumentRequest::Open {document,..}|DocumentRequest::Index {document,..} if *document==id));
                            let request = if tab.revision != 0 {
                                DocumentRequest::Recover { tab }
                            } else if let (Some(path), Some(file)) = (&self.views[&id].path, self.views[&id].file()) {
                                DocumentRequest::Index {
                                    document: id,
                                    revision: 0,
                                    path: path.clone(),
                                    file: file.clone(),
                                }
                            } else if let Some(path) = &self.views[&id].path {
                                DocumentRequest::Open { document: id, revision: 0, path: path.clone() }
                            } else {
                                return Err(io::Error::other("Closed document requires recovery"));
                            };
                            self.files.queue.push_back(request);
                            self.pump_files();
                        }
                    } else {
                        let tab = self.session.tabs().iter().find(|t| t.id == id).unwrap().clone();
                        let mut view = View::new(None);
                        view.path = tab.path.clone();
                        view.editor = None;
                        view.set_notice("Recovering closed tab…".into());
                        self.views.insert(id, view);
                        self.files.queue.push_back(DocumentRequest::Recover { tab });
                        self.pump_files();
                    }
                }
            }
            _ => return Ok(false),
        }
        self.checkpoint_session();
        Ok(true)
    }

    fn next_save_all(&mut self) -> io::Result<()> {
        if let Some(id) = self.files.save_all.pop_front() {
            self.save_document(id, None)?;
        }
        Ok(())
    }

    pub fn request_window_close(&mut self, discard: bool) -> io::Result<()> {
        if discard && self.files.manual_guard {
            self.files.exit_without_metadata = true;
        }
        if !discard
            && self.files.restore_session == Some(true)
            && self.files.storage_ready
            && !self.files.journal_failed.keys().any(|id| self.views.contains_key(id))
        {
            self.begin_hot_exit();
            return Ok(());
        }
        if self.files.hot_exit || self.files.manual_guard {
            self.files.hot_exit = false;
            let active = self.active_id();
            self.files.close = std::iter::once(active)
                .chain(self.session.tabs().iter().map(|tab| tab.id).filter(|id| *id != active))
                .collect();
        }
        if !self.files.quit && !(discard && !self.files.close.is_empty()) {
            self.files.quit = true;
            self.files.close = self.session.tabs().iter().map(|t| t.id).collect();
        }
        self.advance_close(discard)?;
        if self.files.manual_guard && !discard {
            self.files.exit = false;
        }
        self.checkpoint_session();
        Ok(())
    }

    pub fn discard_close_target(&mut self) -> io::Result<()> {
        self.advance_close(true)
    }

    fn advance_close(&mut self, mut discard: bool) -> io::Result<()> {
        while let Some(id) = self.files.close.front().copied() {
            if self.files.saves.contains_key(&id) {
                self.set_notice("Save in progress; close will wait for its revision".into());
                return Ok(());
            }
            if !discard
                && self
                    .views
                    .get(&id)
                    .and_then(View::editor)
                    .is_some_and(|editor| editor.has_pending_edit_receipt() || editor.has_pending_save_completion())
            {
                self.set_notice("Recovery queue is busy; close will wait. Esc cancels".into());
                return Ok(());
            }
            let dirty = self.views.get(&id).and_then(View::editor).map_or_else(
                || self.session.tabs().iter().find(|t| t.id == id).is_some_and(|t| t.revision != t.saved_revision),
                |editor| editor.document().is_dirty(),
            );
            if dirty && !discard {
                self.activate(id);
                self.set_notice(
                    "Unsaved changes: Ctrl+S to save; Ctrl+Shift+Q to discard this tab; Esc to cancel".into(),
                );
                return Ok(());
            }
            // Window close checks every tab but does not throw away the session.
            if !self.files.quit || discard || self.files.restore_session == Some(false) {
                self.close(id, discard)?;
            }
            self.files.close.pop_front();
            discard = false;
        }
        self.files.exit = self.files.quit;
        Ok(())
    }

    fn begin_hot_exit(&mut self) {
        self.files.quit = true;
        self.files.hot_exit = true;
        self.files.manual_guard = false;
        self.files.exit = false;
        self.files.close.clear();
        self.set_notice("Preserving tabs and unsaved work… Esc cancels; Ctrl+Shift+Q discards this tab".into());
        self.checkpoint_session();
        if !self.files.queue.iter().any(|request| matches!(request, DocumentRequest::FlushJournals)) {
            self.files.queue.push_back(DocumentRequest::FlushJournals);
        }
        self.pump_files();
    }

    fn fail_hot_exit(&mut self, message: String) {
        if !self.files.quit {
            return;
        }
        self.files.hot_exit = false;
        self.files.manual_guard = true;
        self.files.exit = false;
        self.files.close = self.session.tabs().iter().map(|tab| tab.id).collect();
        let _ = self.advance_close(false);
        self.files.exit = false;
        self.set_notice(format!("{message}; save or explicitly discard each unsaved tab, Esc cancels"));
    }

    fn check_hot_exit(&mut self) {
        if !self.files.hot_exit
            || !self.files.metadata_durable
            || self.files.persist_inflight
            || self.files.persist_pending
            || !self.files.saves.is_empty()
            || self.files.prompt.is_some()
        {
            return;
        }
        let durable = self.session.tabs().iter().all(|tab| {
            let Some(editor) = self.views.get(&tab.id).and_then(View::editor) else {
                return tab.revision == tab.saved_revision;
            };
            if editor.has_pending_edit_receipt() || editor.has_pending_save_completion() {
                return false;
            }
            let next = editor.next_edit_sequence();
            let needs_journal = editor.document().is_dirty()
                || self.files.seeded.contains(&tab.id)
                || self.files.observed.get(&tab.id).is_some_and(|(first, _)| next > *first);
            !needs_journal
                || self.files.durable.get(&tab.id) == Some(&(editor.document().revision(), next.saturating_sub(1)))
        });
        if durable {
            self.files.exit = true;
        }
    }

    pub fn document_result(&mut self, result: DocumentResult) -> Damage {
        let checkpoint = matches!(
            &result,
            DocumentResult::Indexed { .. } | DocumentResult::Saved { .. } | DocumentResult::Recovered { .. }
        );
        let mut damage = Damage { status: true, ..Default::default() };
        match result {
            DocumentResult::SessionCheckpointDue => {
                damage = Damage::default();
                self.files.persist_timer = false;
                if self.files.persist_pending {
                    self.persist_snapshot();
                }
            }
            DocumentResult::CapacityAvailable => {
                damage = Damage::default();
            }
            DocumentResult::Stopped(result) => {
                damage = Damage::default();
                if let Err(error) = result {
                    damage = self.set_notice(error.to_string());
                }
            }
            DocumentResult::Prepared(result) => {
                self.files.preparing = false;
                match result {
                    Ok(prepared) => {
                        self.install_prepared(prepared);
                        self.files.storage_ready = self.files.deferred_recovery.is_none();
                        damage.full = true;
                        self.checkpoint_session();
                    }
                    Err(error) => {
                        self.set_notice(format!("Recovery unavailable: {error}; existing recovery files retained"));
                    }
                }
                self.queue_initial_indexes();
            }
            DocumentResult::Persisted(result) => {
                self.files.persist_inflight = false;
                match result {
                    Ok(()) => {
                        damage = Damage::default();
                        if self.files.persist_pending {
                            self.checkpoint_session();
                        } else {
                            self.files.metadata_durable = true;
                        }
                    }
                    Err(error) => {
                        let message = format!("Session metadata not durable: {error}");
                        self.set_notice(message.clone());
                        self.fail_hot_exit(message);
                    }
                }
            }
            DocumentResult::Recovered { document, revision, result } => {
                let metadata = self.session.tabs().iter().find(|t| t.id == document && t.revision == revision).cloned();
                if let Some(tab) = metadata
                    && let Some(view) = self.views.get_mut(&document).filter(|v| v.editor().is_none())
                {
                    match result {
                        Ok(document) => {
                            let mut editor = Editor::new(*document);
                            let len = editor.document().snapshot().len();
                            let _ = editor.set_cursors(
                                vec![crate::Cursor {
                                    head: tab.cursor.min(len),
                                    anchor: tab.anchor.min(len),
                                    desired_column: None,
                                }],
                                0,
                            );
                            editor.set_viewport(tab.first_visible_line, view.rows);
                            view.editor = Some(editor);
                            view.refresh_after_edit(Damage { full: true, ..Default::default() });
                        }
                        Err(error) => {
                            view.set_notice(format!("Recovery failed: {error}; journal retained"));
                        }
                    }
                    damage.full = self.session.active() == Some(tab.id);
                }
            }
            DocumentResult::Opened { document, revision, result } => {
                if let Some(view) = self.views.get_mut(&document).filter(|v| v.editor().is_none() && revision == 0) {
                    match result {
                        Ok((path, file)) => {
                            let rows = view.rows();
                            view.path = Some(path);
                            view.file = Some(file);
                            view.set_rows(rows);
                        }
                        Err(error) => {
                            view.set_notice(format!("Open failed: {error}"));
                        }
                    }
                    damage.full = self.active_id() == document;
                }
            }
            DocumentResult::Indexed { document, revision, result } => {
                if let Some(view) = self.views.get_mut(&document).filter(|v| v.editor().is_none() && revision == 0) {
                    match result {
                        Ok(document) => {
                            let mut editor = Editor::new(*document);
                            editor.set_viewport(view.top, view.rows);
                            view.editor = Some(editor);
                            view.index_ready();
                            view.set_notice(String::new());
                        }
                        Err(error) => {
                            view.set_notice(format!("Index failed: {error}"));
                        }
                    }
                    damage.full = self.active_id() == document;
                }
            }
            DocumentResult::Saved { document, revision, result } => {
                if result.as_ref().is_ok_and(|saved| saved.revision != revision) {
                    self.set_notice("Ignored save completion with mismatched source revision".into());
                    return damage;
                }
                if self.files.saves.get(&document).is_some_and(|save| save.revision == revision)
                    && let Some(editor) = self.views.get_mut(&document).and_then(View::editor_mut)
                {
                    let current = editor.document().revision() == revision;
                    let result = editor.finish_save(result);
                    damage = self.complete_save(document, current, result);
                }
            }
            DocumentResult::JournalQueued { document, revision, sequence: _, result } => match result {
                Ok(()) => {
                    self.files.seeded.insert(document);
                    damage = Damage::default();
                    self.checkpoint_session();
                }
                Err(error) => {
                    let message = format!("Tab {} revision {revision} is not journaled: {error}", document.0);
                    self.files.journal_failed.insert(document, message.clone());
                    self.files.durable.remove(&document);
                    self.set_notice(message.clone());
                    self.fail_hot_exit(message);
                }
            },
            DocumentResult::JournalDurable { document, revision, sequence, result } => {
                let pending_before = self.recovery_pending(self.active_id());
                match result {
                    Ok(()) => {
                        self.files.durable.insert(document, (revision, sequence));
                        damage = Damage {
                            status: pending_before != self.recovery_pending(self.active_id()),
                            ..Default::default()
                        };
                    }
                    Err(error) => {
                        let message = format!("Tab {} recovery is not durable: {error}", document.0);
                        self.files.journal_failed.insert(document, message.clone());
                        self.files.durable.remove(&document);
                        self.set_notice(message.clone());
                        self.fail_hot_exit(message);
                    }
                }
            }
            DocumentResult::JournalClosed { document, result } => {
                damage = Damage::default();
                if let Err(error) = result {
                    damage = self
                        .set_notice(format!("Tab {} journal close incomplete: {error}; recovery retained", document.0));
                }
            }
        }
        if checkpoint {
            self.checkpoint_session();
        }
        let retried = self.flush_receipts();
        damage.full |= retried.full;
        damage.status |= retried.status;
        damage.scroll += retried.scroll;
        damage.rows.extend(retried.rows);
        if self.close_pending()
            && !self.files.hot_exit
            && self.files.prompt.is_none()
            && !self.files.manual_guard
            && let Err(error) = self.advance_close(false)
        {
            self.set_notice(error.to_string());
            self.cancel_close_intent();
        }
        self.check_hot_exit();
        self.pump_files();
        damage
    }

    fn complete_save(
        &mut self,
        document: DocumentId,
        current: bool,
        result: io::Result<crate::CommandResult>,
    ) -> Damage {
        let mut damage = Damage { status: true, ..Default::default() };
        let mut may_close = false;
        let mut warning = false;
        let Some(view) = self.views.get_mut(&document) else { return damage };
        let notice = match result {
            Ok(result) => {
                self.files.saves.remove(&document);
                may_close = current && view.editor().is_some_and(|editor| !editor.document().is_dirty());
                let mut notice = match result.status {
                    CommandStatus::Message(message) | CommandStatus::NotImplemented(message) => {
                        warning = true;
                        message
                    }
                    _ if !may_close => "Saved earlier revision; current changes remain unsaved".into(),
                    _ => "Saved".into(),
                };
                let edited = view.refresh_after_edit(result.damage);
                if self.session.active() == Some(document) {
                    damage = edited;
                    damage.status = true;
                }
                if let Some(path) = &view.path
                    && let Err(error) = self.session.set_path(document, path.clone())
                {
                    // Admission reserves destinations, but a stale or inconsistent
                    // metadata result must still fail visibly and never close.
                    may_close = false;
                    warning = true;
                    view.path = None;
                    if let Some(editor) = view.editor_mut() {
                        editor.document.path = None;
                        editor.document.original = None;
                        editor.document.saved_revision = editor.document.revision.wrapping_add(1);
                    }
                    notice = format!("Save metadata conflict: {error}; recovered contents require Save As");
                }
                notice
            }
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock
                    && view.editor().is_some_and(|editor| editor.has_pending_save_completion()) =>
            {
                view.set_notice("Save completion is waiting for recovery queue admission".into());
                return damage;
            }
            Err(error) => {
                self.files.saves.remove(&document);
                format!("Save failed: {error}")
            }
        };
        view.set_notice(notice.clone());
        if self.session.active() != Some(document) {
            self.set_notice(format!("Tab {}: {notice}", document.0));
        }
        if may_close && !warning {
            if self.files.close.contains(&document) {
                if self.files.quit && self.files.restore_session == Some(true) && self.files.storage_ready {
                    self.begin_hot_exit();
                } else if let Err(error) = self.advance_close(false) {
                    self.set_notice(error.to_string());
                    self.cancel_close_intent();
                }
                damage.full = true;
            }
            if let Err(error) = self.next_save_all() {
                self.set_notice(error.to_string());
            }
        } else {
            self.cancel_close_intent();
            self.files.save_all.clear();
        }
        damage
    }

    fn flush_receipts(&mut self) -> Damage {
        let mut finished = Vec::new();
        for (&document, view) in &mut self.views {
            let Some(editor) = view.editor_mut() else { continue };
            if editor.flush_edit_receipt().is_err() {
                continue;
            }
            if editor.has_pending_save_completion()
                && let Some(save) = self.files.saves.get(&document)
            {
                let current = editor.document().revision() == save.revision;
                match editor.finish_pending_save() {
                    Ok(Some(result)) => finished.push((document, current, Ok(result))),
                    Err(error) if error.kind() != io::ErrorKind::WouldBlock => {
                        finished.push((document, current, Err(error)))
                    }
                    _ => {}
                }
            }
        }
        let mut damage = Damage::default();
        for (document, current, result) in finished {
            let changed = self.complete_save(document, current, result);
            damage.full |= changed.full;
            damage.status |= changed.status;
            damage.scroll += changed.scroll;
            damage.rows.extend(changed.rows);
            self.checkpoint_session();
        }
        damage
    }

    fn install_prepared(&mut self, prepared: crate::document_worker::PreparedWorkspace) {
        self.sync_all();
        let additional = self
            .session
            .tabs()
            .iter()
            .filter(|tab| {
                let view = &self.views[&tab.id];
                let dirty = view.editor().is_some_and(|e| e.document().is_dirty());
                let blank =
                    view.path.is_none() && view.editor().is_some_and(|e| e.document().snapshot().is_empty()) && !dirty;
                (!blank
                    || self.files.saves.contains_key(&tab.id)
                    || self.files.prompt.as_ref().is_some_and(|p| p.document == tab.id))
                    && (dirty
                        || view.path.is_none()
                        || !prepared.session.tabs().iter().any(|t| t.path.is_some() && t.path == view.path))
            })
            .count();
        if prepared.session.tabs().len() + additional > kwil_core_workspace::session::MAX_TABS
            || !self.closed_views.is_empty()
        {
            self.files.deferred_recovery = Some(Box::new(prepared));
            let reason =
                if self.closed_views.is_empty() { "tab limit reached" } else { "reopen recently closed tabs first" };
            self.set_notice(format!("Recovery retained: {reason}; then Restore Session. Active work is unchanged"));
            return;
        }
        let old_tabs = self.session.tabs().to_vec();
        let old_active = self.active_id();
        let mut old_views = std::mem::take(&mut self.views);
        self.input_epoch = self.input_epoch.wrapping_add(1);
        self.session = prepared.session;
        for (id, document) in prepared.documents {
            let tab = self.session.tabs().iter().find(|t| t.id == id).unwrap().clone();
            let mut view = View::new(None);
            view.path = tab.path.clone();
            match document {
                Ok(document) => {
                    if document.path != tab.path {
                        view.path.clone_from(&document.path);
                        self.session.tab_mut(id).unwrap().path.clone_from(&document.path);
                        view.set_notice(
                            "Recovered file changed on disk; original recovery retained. Use Save As".into(),
                        );
                    }
                    let mut editor = Editor::new(document);
                    let len = editor.document().snapshot().len();
                    let _ = editor.set_cursors(
                        vec![crate::Cursor {
                            head: tab.cursor.min(len),
                            anchor: tab.anchor.min(len),
                            desired_column: None,
                        }],
                        0,
                    );
                    editor.set_viewport(tab.first_visible_line, 0);
                    view.editor = Some(editor);
                    view.top = tab.first_visible_line;
                    view.file = view.editor().unwrap().document().original.clone();
                }
                Err(error) => {
                    view.editor = None;
                    view.set_notice(format!("Recovery failed: {error}; journal retained"));
                }
            }
            view.set_horizontal_window(tab.first_byte_column, 64 << 10);
            self.views.insert(id, view);
        }
        let mut mapping = HashMap::new();
        let mut active = None;
        for tab in old_tabs {
            let Some(view) = old_views.remove(&tab.id) else { continue };
            let dirty = view.editor().is_some_and(|e| e.document().is_dirty());
            let blank =
                view.path.is_none() && view.editor().is_some_and(|e| e.document().snapshot().is_empty()) && !dirty;
            if blank
                && !self.views.is_empty()
                && !self.files.saves.contains_key(&tab.id)
                && self.files.prompt.as_ref().is_none_or(|p| p.document != tab.id)
            {
                continue;
            }
            if let Some(existing) =
                self.session.tabs().iter().find(|t| t.path.is_some() && t.path == view.path).map(|t| t.id)
            {
                if !dirty {
                    if let Some((line, column, extend)) = view.pending_goto
                        && let Some(recovered) = self.views.get_mut(&existing)
                    {
                        let _ = recovered.execute(
                            "navigation.goToLine",
                            &crate::CommandArgs {
                                line: Some(line),
                                column: Some(column),
                                extend,
                                ..Default::default()
                            },
                            1,
                        );
                    }
                    mapping.insert(tab.id, existing);
                    if tab.id == old_active {
                        active = Some(existing);
                    }
                    continue;
                }
                // Preserve both contents without giving two tabs the same save target.
                if let Some(recovered) = self.views.get_mut(&existing) {
                    recovered.path = None;
                    if let Some(editor) = recovered.editor_mut() {
                        editor.document.path = None;
                    }
                }
                self.session.tab_mut(existing).unwrap().path = None;
            }
            let id = self.session.open(view.path.clone()).expect("validated paths and reserved recovery capacity");
            mapping.insert(tab.id, id);
            if tab.id == old_active {
                active = Some(id);
            }
            self.views.insert(id, view);
        }
        // Requests captured while recovery was preparing retain their source tab.
        for request in &mut self.files.queue {
            match request {
                DocumentRequest::Open { document, .. }
                | DocumentRequest::Index { document, .. }
                | DocumentRequest::Save { document, .. } => {
                    if let Some(id) = mapping.get(document) {
                        *document = *id;
                    }
                }
                _ => {}
            }
        }
        self.files.saves = std::mem::take(&mut self.files.saves)
            .into_iter()
            .map(|(id, save)| (*mapping.get(&id).unwrap_or(&id), save))
            .collect();
        if let Some(prompt) = &mut self.files.prompt
            && let Some(id) = mapping.get(&prompt.document)
        {
            prompt.document = *id;
        }
        for id in self.files.close.iter_mut().chain(self.files.save_all.iter_mut()) {
            if let Some(mapped) = mapping.get(id) {
                *id = *mapped;
            }
        }
        if let Some(id) = active {
            self.session.activate(id);
        }
        if self.views.is_empty() {
            let _ = self.open(View::new(None));
        }
    }

    pub fn status_text(&self) -> String {
        let tabs = self.session.tabs();
        let position = tabs.iter().position(|t| t.id == self.active_id()).unwrap_or(0) + 1;
        let recovery = if self.files.journal_failed.contains_key(&self.active_id())
            || (self.files.worker.is_some() && !self.files.preparing && !self.files.storage_ready)
        {
            "  |  Recovery unavailable"
        } else if self.recovery_pending(self.active_id()) {
            "  |  Recovery pending"
        } else {
            ""
        };
        format!("[{position}/{}] {}{recovery}", tabs.len(), self.deref().status_text())
    }

    fn recovery_pending(&self, id: DocumentId) -> bool {
        let Some(editor) = self.views.get(&id).and_then(View::editor) else { return false };
        let next = editor.next_edit_sequence();
        let needed = editor.document().is_dirty()
            || self.files.seeded.contains(&id)
            || self.files.observed.get(&id).is_some_and(|(first, _)| next > *first);
        self.files.observed.contains_key(&id)
            && needed
            && self.files.durable.get(&id) != Some(&(editor.document().revision(), next.saturating_sub(1)))
    }
}

impl Deref for Workspace {
    type Target = View;
    fn deref(&self) -> &View {
        &self.views[&self.active_id()]
    }
}
impl DerefMut for Workspace {
    fn deref_mut(&mut self) -> &mut View {
        let id = self.active_id();
        self.views.get_mut(&id).expect("active view exists")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CommandArgs;

    #[test]
    fn file_commands_create_cycle_close_reopen_and_preserve_unsaved_tabs() {
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        let first = workspace.active_id();
        workspace
            .execute("edit.insert", &CommandArgs { text: Some("日本語".into()), ..Default::default() }, 1)
            .unwrap();
        assert!(workspace.file_command("file.new").unwrap());
        let second = workspace.active_id();
        assert_ne!(first, second);
        workspace.file_command("file.previousTab").unwrap();
        assert_eq!(workspace.active_id(), first);
        workspace.file_command("file.close").unwrap();
        assert!(workspace.close_pending());
        assert_eq!(workspace.session().tabs().len(), 2);
        workspace.cancel_file_prompt();
        workspace.activate(second);
        workspace.file_command("file.close").unwrap();
        workspace.file_command("file.reopenClosed").unwrap();
        assert_eq!(workspace.active_id(), second);
        workspace.activate(first);
        assert_eq!(workspace.editor().unwrap().primary_cursor().head, "日本語".len() as u64);
    }

    #[test]
    fn window_close_visits_inactive_dirty_tabs_and_requires_each_discard() {
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        let first = workspace.active_id();
        workspace.execute("edit.insert", &CommandArgs { text: Some("first".into()), ..Default::default() }, 1).unwrap();
        workspace.file_command("file.new").unwrap();
        let second = workspace.active_id();
        workspace
            .execute("edit.insert", &CommandArgs { text: Some("second".into()), ..Default::default() }, 1)
            .unwrap();
        workspace.file_command("file.new").unwrap();
        workspace.request_window_close(false).unwrap();
        assert_eq!(workspace.active_id(), first);
        assert!(!workspace.wants_exit());
        workspace.discard_close_target().unwrap();
        assert_eq!(workspace.active_id(), second);
        assert!(!workspace.wants_exit());
        workspace.cancel_file_prompt();
        assert!(!workspace.close_pending());
        assert!(!workspace.wants_exit());
        assert!(workspace.view(second).unwrap().editor().unwrap().document().is_dirty());
    }

    #[test]
    fn save_as_named_blank_prompt_keeps_destination_and_cancels_close_intent() {
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        let first = workspace.active_id();
        workspace.file_command("file.saveAndQuit").unwrap();
        workspace.file_prompt_mut().unwrap().text = "   ".into();
        workspace.submit_file_prompt().unwrap();
        assert_eq!(workspace.file_prompt().unwrap().document, first);
        assert!(workspace.close_pending());
        workspace.cancel_file_prompt();
        assert!(!workspace.close_pending());
        workspace.file_command("file.saveAs").unwrap();
        assert_eq!(workspace.file_prompt().unwrap().kind, FilePromptKind::SaveAs);
    }

    #[test]
    fn stale_index_completion_does_not_replace_edited_or_unrelated_tab() {
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        let first = workspace.active_id();
        workspace.execute("edit.insert", &CommandArgs { text: Some("keep".into()), ..Default::default() }, 1).unwrap();
        workspace.file_command("file.new").unwrap();
        let second = workspace.active_id();
        workspace.document_result(DocumentResult::Indexed {
            document: first,
            revision: 0,
            result: Ok(Box::new(crate::Document::new())),
        });
        assert_eq!(workspace.active_id(), second);
        assert_eq!(workspace.view(first).unwrap().editor().unwrap().document().snapshot().len(), 4);
        assert!(workspace.editor().unwrap().document().snapshot().is_empty());
    }

    fn edit(workspace: &mut Workspace, text: &str) {
        workspace.execute("edit.insert", &CommandArgs { text: Some(text.into()), ..Default::default() }, 1).unwrap();
    }

    fn take_save(workspace: &mut Workspace) -> (DocumentId, kwil_core_workspace::SaveRequest) {
        let Some(DocumentRequest::Save { document, request }) = workspace.files.queue.pop_front() else {
            panic!("expected worker save")
        };
        (document, request)
    }

    #[test]
    fn pending_save_as_reserves_destination_before_any_worker_completion() {
        let path = std::env::temp_dir().join(format!("kwil-reserved-save-{}.txt", std::process::id()));
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        let first = workspace.active_id();
        edit(&mut workspace, "first");
        workspace.save_document(first, Some(path.clone())).unwrap();
        let (_, request) = take_save(&mut workspace);
        workspace.file_command("file.new").unwrap();
        let second = workspace.active_id();
        edit(&mut workspace, "second");
        assert!(
            workspace.save_document(second, Some(path.clone())).is_err(),
            "pending SaveFlight.path must reserve its destination"
        );
        assert!(!workspace.files.saves.contains_key(&second));
        assert!(workspace.files.queue.is_empty());
        assert_eq!(request.path, path);
    }

    #[test]
    fn closed_history_eviction_waits_for_its_prior_metadata_to_be_durable() {
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        for _ in 0..kwil_core_workspace::session::MAX_CLOSED {
            workspace.close(workspace.active_id(), true).unwrap();
        }
        let oldest = workspace.session.closed().back().unwrap().id;
        workspace.files.storage_ready = true;
        let current = workspace.active_id();
        assert!(workspace.close(current, true).is_err());
        assert_eq!(workspace.session.closed().back().unwrap().id, oldest);
        assert!(workspace.view(current).is_some());
        assert!(workspace.files.persist_inflight);
        workspace.document_result(DocumentResult::Persisted(Err(io::Error::other("fixture checkpoint failure"))));
        assert_eq!(workspace.session.closed().back().unwrap().id, oldest);
        assert!(workspace.view(current).is_some());
        assert!(!workspace.files.metadata_durable);
        workspace.files.metadata_durable = true;
        workspace.close(current, true).unwrap();
        assert!(!workspace.session.closed().iter().any(|tab| tab.id == oldest));
    }

    #[test]
    fn unexpected_save_metadata_collision_is_visible_and_cannot_close() {
        let path = std::env::temp_dir().join(format!(
            "kwil-save-metadata-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        let first = workspace.active_id();
        edit(&mut workspace, "first");
        workspace.save_document(first, Some(path.clone())).unwrap();
        let (_, request) = take_save(&mut workspace);
        let revision = request.revision;
        workspace.file_command("file.new").unwrap();
        let second = workspace.active_id();
        workspace.session.set_path(second, path.clone()).unwrap();
        workspace.files.close.push_back(first);
        workspace.document_result(DocumentResult::Saved {
            document: first,
            revision,
            result: kwil_core_workspace::save_snapshot(request),
        });
        let notice = workspace.status_text();
        assert!(!workspace.close_pending(), "metadata failure cannot complete a close");
        assert!(notice.contains("metadata") && notice.contains("Save As"), "collision must be visible: {notice}");
        assert!(workspace.view(first).unwrap().editor().unwrap().document().is_dirty());
        assert!(workspace.view(first).unwrap().path.is_none());
        drop(workspace);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn inactive_save_completion_keeps_active_text_and_concurrent_source_edits() {
        let path = std::env::temp_dir().join(format!(
            "kwil-inactive-save-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        let first = workspace.active_id();
        edit(&mut workspace, "before");
        workspace.save_document(first, Some(path.clone())).unwrap();
        let (document, request) = take_save(&mut workspace);
        let revision = request.revision;
        edit(&mut workspace, " after");
        workspace.file_command("file.new").unwrap();
        let second = workspace.active_id();
        edit(&mut workspace, "other");
        workspace.document_result(DocumentResult::Saved {
            document,
            revision,
            result: kwil_core_workspace::save_snapshot(request),
        });
        assert_eq!(workspace.active_id(), second);
        assert_eq!(workspace.editor().unwrap().document().snapshot().len(), 5);
        let source = workspace.view(first).unwrap().editor().unwrap();
        assert!(source.document().is_dirty());
        assert_eq!(source.document().snapshot().len(), 12);
        assert_eq!(std::fs::read(&path).unwrap(), b"before");
        drop(workspace);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn save_all_serializes_untitled_prompts_and_save_warnings_cancel_exit() {
        let root = std::env::temp_dir().join(format!(
            "kwil-saveall-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        edit(&mut workspace, "one");
        let first = workspace.active_id();
        workspace.file_command("file.new").unwrap();
        edit(&mut workspace, "two");
        let second = workspace.active_id();
        workspace.file_command("file.saveAll").unwrap();
        assert_eq!(workspace.file_prompt().unwrap().document, first);
        workspace.file_prompt_mut().unwrap().text = root.join("first.txt").to_string_lossy().into();
        workspace.submit_file_prompt().unwrap();
        let (document, request) = take_save(&mut workspace);
        let revision = request.revision;
        workspace.document_result(DocumentResult::Saved {
            document,
            revision,
            result: kwil_core_workspace::save_snapshot(request),
        });
        assert_eq!(workspace.file_prompt().unwrap().document, second);
        workspace.file_prompt_mut().unwrap().text = root.join("second.txt").to_string_lossy().into();
        workspace.submit_file_prompt().unwrap();
        let (document, request) = take_save(&mut workspace);
        let revision = request.revision;
        let mut result = kwil_core_workspace::save_snapshot(request).unwrap();
        result.warnings.push("security metadata warning".into());
        workspace.files.quit = true;
        workspace.files.close.push_back(second);
        workspace.document_result(DocumentResult::Saved { document, revision, result: Ok(result) });
        assert!(!workspace.wants_exit());
        assert!(!workspace.close_pending());
        assert!(workspace.status_text().contains("security metadata warning"));
        drop(workspace);
        std::fs::remove_file(root.join("first.txt")).unwrap();
        std::fs::remove_file(root.join("second.txt")).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn failed_and_unsolicited_save_results_never_close_any_tab() {
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        edit(&mut workspace, "unsaved");
        let document = workspace.active_id();
        workspace.files.saves.insert(document, SaveFlight { revision: 1, path: None });
        workspace.files.quit = true;
        workspace.files.close.push_back(document);
        workspace.document_result(DocumentResult::Saved {
            document,
            revision: 999,
            result: Err(io::Error::other("unrelated")),
        });
        assert!(workspace.files.saves.contains_key(&document));
        assert!(!workspace.wants_exit());
        workspace.document_result(DocumentResult::Saved {
            document,
            revision: 1,
            result: Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied")),
        });
        assert!(!workspace.wants_exit());
        assert!(!workspace.close_pending());
        assert!(workspace.editor().unwrap().document().is_dirty());
        assert!(workspace.status_text().contains("denied"));
    }

    #[test]
    fn window_close_promotes_a_tab_close_prompt_to_cover_all_tabs() {
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        edit(&mut workspace, "one");
        let first = workspace.active_id();
        workspace.file_command("file.new").unwrap();
        edit(&mut workspace, "two");
        let second = workspace.active_id();
        workspace.file_command("file.close").unwrap();
        assert_eq!(workspace.files.close.iter().copied().collect::<Vec<_>>(), vec![second]);
        workspace.request_window_close(false).unwrap();
        assert_eq!(workspace.active_id(), first);
        workspace.discard_close_target().unwrap();
        assert_eq!(workspace.active_id(), second);
        assert!(!workspace.wants_exit());
        workspace.cancel_file_prompt();
    }

    #[test]
    fn named_save_as_routes_to_new_path_without_overwriting_the_original() {
        let root = std::env::temp_dir().join(format!(
            "kwil-named-saveas-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let original = root.join("original.txt");
        let target = root.join("copy.txt");
        std::fs::write(&original, b"original").unwrap();
        let file = kwil_core_workspace::open_for_editing(&original).unwrap();
        file.build_index(1).unwrap();
        let mut workspace = Workspace::new(View::new(Some((original.clone(), file)))).unwrap();
        let document = workspace.active_id();
        workspace.execute("edit.selectAll", &CommandArgs::default(), 1).unwrap();
        edit(&mut workspace, "changed");
        workspace.file_command("file.saveAs").unwrap();
        assert_eq!(workspace.file_prompt().unwrap().kind, FilePromptKind::SaveAs);
        workspace.file_prompt_mut().unwrap().text = target.to_string_lossy().into();
        workspace.submit_file_prompt().unwrap();
        let (owner, request) = take_save(&mut workspace);
        assert_eq!(owner, document);
        assert_eq!(request.path, target);
        let revision = request.revision;
        workspace.document_result(DocumentResult::Saved {
            document,
            revision,
            result: kwil_core_workspace::save_snapshot(request),
        });
        assert_eq!(workspace.editor().unwrap().document().path(), Some(target.as_path()));
        assert_eq!(std::fs::read(&original).unwrap(), b"original");
        assert_eq!(std::fs::read(&target).unwrap(), b"changed");
        drop(workspace);
        std::fs::remove_file(original).unwrap();
        std::fs::remove_file(target).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn tabs_keep_their_text_undo_and_cursor_and_refuse_dirty_close() {
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        let first = workspace.active_id();
        workspace.execute("edit.insert", &CommandArgs { text: Some("first".into()), ..Default::default() }, 1).unwrap();
        let second = workspace.open(View::new(None)).unwrap();
        workspace
            .execute("edit.insert", &CommandArgs { text: Some("second".into()), ..Default::default() }, 1)
            .unwrap();
        workspace.activate(first);
        assert_eq!(workspace.editor().unwrap().primary_cursor().head, 5);
        let mut bytes = Vec::new();
        workspace.editor().unwrap().document().snapshot().write_to(&mut bytes).unwrap();
        assert_eq!(bytes, b"first");
        assert!(workspace.close(second, false).is_err());
        assert!(workspace.view(second).unwrap().editor().unwrap().document().is_dirty());
        workspace.execute("edit.undo", &CommandArgs::default(), 1).unwrap();
        assert!(workspace.editor().unwrap().document().snapshot().is_empty());
        assert!(workspace.close(first, false).unwrap());
        assert_eq!(workspace.active_id(), second);
    }

    #[test]
    fn cycling_and_last_close_always_leave_an_active_view() {
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        let first = workspace.active_id();
        let second = workspace.open(View::new(None)).unwrap();
        workspace.cycle(false);
        assert_eq!(workspace.active_id(), first);
        workspace.cycle(true);
        assert_eq!(workspace.active_id(), second);
        workspace.close(first, false).unwrap();
        workspace.close(second, false).unwrap();
        assert_eq!(workspace.session().tabs().len(), 1);
        assert!(workspace.editor().unwrap().document().snapshot().is_empty());
    }

    #[test]
    fn recovery_remaps_early_edits_and_their_save_prompt_without_reusing_identity() {
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        edit(&mut workspace, "early");
        workspace.file_command("file.saveAndQuit").unwrap();
        let old = workspace.active_id();
        let mut session = Session::default();
        let restored = session.open(None).unwrap();
        assert_eq!(restored, old, "the fixture must exercise a real startup identity collision");
        let mut document = crate::Document::new();
        document.buffer.insert(0, b"recovered").unwrap();
        document.revision = 4;
        document.saved_revision = 3;
        document.next_revision = 5;
        session.tab_mut(restored).unwrap().revision = 4;
        session.tab_mut(restored).unwrap().saved_revision = 3;
        workspace.install_prepared(crate::document_worker::PreparedWorkspace {
            session,
            documents: vec![(restored, Ok(document))],
        });
        let early = workspace.active_id();
        assert_ne!(early, restored);
        assert_eq!(workspace.editor().unwrap().document().snapshot().len(), 5);
        assert_eq!(workspace.view(restored).unwrap().editor().unwrap().document().snapshot().len(), 9);
        assert_eq!(workspace.file_prompt().unwrap().document, early);
        assert_eq!(workspace.files.close.front().copied(), Some(early));
        assert!(workspace.close_pending());
        assert_eq!(workspace.session().tabs().len(), 2);
        workspace.cancel_file_prompt();
    }

    #[test]
    fn tab_geometry_keeps_editor_hits_and_scroll_copy_inside_both_origins() {
        use kwil_core_commands::settings::TabLayout;
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        workspace.file_command("file.new").unwrap();
        let active = workspace.active_id();
        for layout in [TabLayout::Horizontal, TabLayout::Vertical] {
            let geometry = workspace.tab_geometry(
                TabMetrics { width: 800, height: 400, cell_width: 8, cell_height: 16, scrollbar: 14 },
                layout,
            );
            let expected = if layout == TabLayout::Horizontal { (0, 16) } else { (208, 0) };
            assert_eq!((geometry.body.x, geometry.body.y), expected);
            assert!(geometry.body.contains(geometry.text.x, geometry.text.y));
            assert!(geometry.hit(geometry.text.x, geometry.text.y).is_none(), "text origin must not activate a tab");
            let slot = geometry.slots.iter().find(|slot| slot.document == active).unwrap();
            assert_eq!(geometry.hit(slot.area.x, slot.area.y), Some(TabAction::Activate(active)));
            assert_eq!(geometry.hit(slot.close.x, slot.close.y), Some(TabAction::Close(active)));
            for delta in [-1, 1] {
                let (source, target) = geometry.scroll_copy(delta).unwrap();
                assert_eq!(source.x, geometry.text.x);
                assert_eq!(target.x, geometry.text.x);
                assert_eq!(source.width, geometry.text.width);
                assert_eq!(source.height + 16, geometry.text.height);
                assert!(geometry.text.contains(source.x, source.y));
                assert!(geometry.text.contains(target.x, target.y));
                assert!(source.y + source.height <= geometry.text.y + geometry.text.height);
                assert!(target.y + target.height <= geometry.text.y + geometry.text.height);
            }
        }
    }

    #[test]
    fn tab_overflow_virtualizes_labels_and_tiny_windows_have_no_out_of_bounds_hits() {
        use kwil_core_commands::settings::TabLayout;
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        for _ in 1..128 {
            workspace.file_command("file.new").unwrap();
        }
        for layout in [TabLayout::Horizontal, TabLayout::Vertical] {
            let geometry = workspace.tab_geometry(
                TabMetrics { width: 400, height: 200, cell_width: 8, cell_height: 16, scrollbar: 14 },
                layout,
            );
            assert!(geometry.slots.len() < 128);
            assert!(geometry.slots.iter().any(|slot| slot.document == workspace.active_id()));
            assert!(geometry.first > 0);
            let previous = geometry.previous;
            workspace.tab_action(geometry.hit(previous.x, previous.y).unwrap(), &geometry).unwrap();
            assert!(
                workspace.session.tabs().iter().position(|t| t.id == workspace.active_id()).unwrap() < geometry.first
            );
            let tiny = workspace
                .tab_geometry(TabMetrics { width: 0, height: 0, cell_width: 0, cell_height: 0, scrollbar: 14 }, layout);
            assert_eq!(tiny.body, TabArea::default());
            assert!(tiny.hit(0, 0).is_none());
            assert!(tiny.scroll_copy(1).is_none());
        }
    }
}
