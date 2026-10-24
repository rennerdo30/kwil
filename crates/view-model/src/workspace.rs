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

#[path = "workspace_hex.rs"]
mod workspace_hex;
use crate::document_worker::binary_offer::{Choice as BinaryChoice, Offer as BinaryOffer, Ticket as BinaryTicket};
use workspace_hex::HexFlow;

enum BinaryPhase {
    Opening,
    Offered(BinaryOffer),
    Resolving(BinaryChoice),
}
struct BinaryFlight {
    ticket: BinaryTicket,
    path: PathBuf,
    phase: BinaryPhase,
    return_to: Option<DocumentId>,
    preview_ready: bool,
}
#[derive(Default)]
struct BinaryFlow {
    flights: HashMap<DocumentId, BinaryFlight>,
    next_generation: u64,
    // At most one retirement per admitted flight. Refused disposal never
    // destroys a last source on the UI, even after a stale path completion.
    retiring: Vec<(DocumentId, Box<Editor>)>,
}

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

struct EditorLoan {
    key: kwil_core_search::worker::SearchKey,
    operation: u64,
}

#[derive(Default)]
struct FileFlow {
    loans: HashMap<DocumentId, EditorLoan>,
    next_loan: u64,
    replacement_journals: HashMap<DocumentId, u64>,
    worker: Option<DocumentWorker>,
    queue: VecDeque<DocumentRequest>,
    saves: HashMap<DocumentId, SaveFlight>,
    failed_opens: HashSet<DocumentId>,
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
    history_wait: Option<DocumentId>,
    reopen: Option<(DocumentId, u64)>,
    next_reopen: u64,
    exit_without_metadata: bool,
}

/// Captured editor input survives tab activation, but never document replacement,
/// close/reopen, or a file-prompt ownership transition.
#[derive(Clone)]
pub struct EditorInputTicket {
    document: DocumentId,
    incarnation: std::sync::Arc<()>,
    prompt_generation: u64,
    source_epoch: u64,
}

pub struct Workspace {
    hex: HexFlow,
    binary: BinaryFlow,
    input_epoch: u64,
    editor_settings: Option<kwil_core_commands::Settings>,
    // Tab.path tracks named destinations; recovery_path is display-only context.
    // Only View/Document paths can supply an implicit save destination.
    session: Session,
    views: HashMap<DocumentId, View>,
    closed_views: HashMap<DocumentId, View>,
    files: FileFlow,
}

impl Workspace {
    pub fn new(view: View) -> io::Result<Self> {
        let mut workspace = Self {
            hex: HexFlow::default(),
            binary: BinaryFlow::default(),
            input_epoch: 0,
            editor_settings: None,
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
    pub fn editor_input_ticket(&self, document: DocumentId) -> Option<EditorInputTicket> {
        if self.files.prompt.is_some() || self.hex_owns_document(document) {
            return None;
        }
        let view = self.views.get(&document)?;
        let editor = view.editor()?;
        Some(EditorInputTicket {
            document,
            incarnation: view.input_incarnation.clone(),
            prompt_generation: self.files.next_prompt,
            source_epoch: editor.document().source_epoch,
        })
    }

    pub fn editor_input_is_current(&self, ticket: &EditorInputTicket) -> bool {
        self.files.prompt.is_none()
            && self.files.next_prompt == ticket.prompt_generation
            && !self.hex_owns_document(ticket.document)
            && self.views.get(&ticket.document).is_some_and(|view| {
                std::sync::Arc::ptr_eq(&view.input_incarnation, &ticket.incarnation)
                    && view.editor().is_some_and(|editor| editor.document().source_epoch == ticket.source_epoch)
            })
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

    /// Frontends keep displaying the cached viewport and gate navigation,
    /// editing, layout reloads and save/close actions for this target while true.
    /// Other tabs remain available. No new event or export is required.
    pub fn document_busy(&self, id: DocumentId) -> bool {
        self.files.loans.contains_key(&id)
            || self.hex.busy(id)
            || self.binary.flights.get(&id).is_some_and(|flight| !matches!(flight.phase, BinaryPhase::Offered(_)))
    }

    pub fn replacement_busy(&self) -> bool {
        self.replacement_busy_for(self.active_id())
    }

    pub fn replacement_busy_for(&self, id: DocumentId) -> bool {
        self.document_busy(id)
    }

    pub fn search_replacement_pending(&self) -> bool {
        !self.files.loans.is_empty() || !self.hex.loans.is_empty() || self.hex.opening()
    }

    /// O(1) UI admission. Recipe validation, iteration and edits run on the
    /// existing fixed worker; queue rejection restores the exact editor.
    pub fn submit_search_replacement(&mut self, recipe: kwil_core_search::worker::ReplaceRecipe) -> io::Result<()> {
        let id = DocumentId(recipe.key.document_id);
        if self.hex_owns_document(id) {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "Text replacement is unavailable in hex view"));
        }
        if self.files.preparing
            || self.files.deferred_recovery.is_some()
            || !self.files.storage_ready
            || self.files.worker.is_none()
            || self.document_busy(id)
            || self.files.saves.contains_key(&id)
            || self.close_pending()
            || self.files.prompt.is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Document/recovery worker is busy; retry replacement",
            ));
        }
        if !self.files.observed.contains_key(&id) || self.files.journal_failed.contains_key(&id) {
            return Err(io::Error::other("Replacement requires an admitted recovery observer"));
        }
        let view =
            self.views.get(&id).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Search tab is closed"))?;
        view.editor()
            .ok_or_else(|| io::Error::new(io::ErrorKind::WouldBlock, "Document is not ready"))?
            .admit_worker_loan(recipe.key.revision)?;
        self.pump_files();
        if !self.files.queue.is_empty() {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "Recovery queue is full; retry replacement"));
        }
        self.sync_all();
        let operation =
            self.files.next_loan.checked_add(1).ok_or_else(|| io::Error::other("Replacement identity exhausted"))?;
        self.files.next_loan = operation;
        let key = recipe.key;
        let editor = Box::new(self.views.get_mut(&id).unwrap().editor.take().unwrap());
        let request = DocumentRequest::Replace { document: id, operation, editor, recipe };
        match self.files.worker.as_ref().unwrap().try_submit(request) {
            Ok(()) => {
                self.files.loans.insert(id, EditorLoan { key, operation });
                self.input_epoch = self.input_epoch.wrapping_add(1);
                self.views.get_mut(&id).unwrap().set_notice("Replacing...".into());
                Ok(())
            }
            Err(error) => {
                let (request, kind) = match error {
                    std::sync::mpsc::TrySendError::Full(request) => (request, io::ErrorKind::WouldBlock),
                    std::sync::mpsc::TrySendError::Disconnected(request) => (request, io::ErrorKind::BrokenPipe),
                };
                let DocumentRequest::Replace { editor, .. } = *request else { unreachable!() };
                self.views.get_mut(&id).unwrap().editor = Some(*editor);
                Err(io::Error::new(kind, "Replacement was not admitted; editor retained"))
            }
        }
    }

    /// Apply accepted settings in memory, including inactive and cached tabs.
    /// Keep the policy for editors installed by later open/recovery results.
    /// Frontends retain responsibility for settings receipts and visual damage.
    pub fn set_editor_settings(&mut self, settings: &kwil_core_commands::Settings) {
        self.editor_settings = Some(settings.clone());
        for view in self.views.values_mut().chain(self.closed_views.values_mut()) {
            if let Some(editor) = view.editor_mut() {
                editor.settings = settings.clone();
            }
        }
    }

    pub fn tab_title(&self, id: DocumentId) -> String {
        let Some(tab) = self.session.tabs().iter().find(|t| t.id == id) else { return String::new() };
        let name = tab
            .path
            .as_ref()
            .or(tab.recovery_path.as_ref())
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
        if let Some(path) = &view.path
            && let Some(id) = self
                .session
                .tabs()
                .iter()
                .find(|tab| {
                    tab.path.as_ref().is_some_and(|reserved| kwil_core_workspace::paths_may_alias(reserved, path))
                })
                .map(|tab| tab.id)
        {
            self.activate(id);
            return Ok(id);
        }
        if let Some(settings) = &self.editor_settings
            && let Some(editor) = view.editor_mut()
        {
            editor.settings = settings.clone();
        }
        let previous = self.session.active();
        let id = self.session.open(view.path.clone())?;
        if previous != self.session.active() {
            self.input_epoch = self.input_epoch.wrapping_add(1);
            if let Some(previous) = previous {
                self.cancel_hex_input_for(previous);
            }
        }
        // Reopening an already-open path activates its existing unsaved view.
        self.views.entry(id).or_insert(view);
        if self.views[&id].editor().is_none()
            && self.views[&id].file().is_some()
            && let Some(path) = self.views[&id].path.clone()
        {
            // Startup may hand us an unindexed TextFile before recovery is
            // ready. Suspend its raw display immediately, before set_rows or
            // the first frontend paint can read it as text.
            let request = self.checked_open_request(id, 0, path)?;
            self.binary.flights.get_mut(&id).unwrap().return_to = previous;
            self.files.queue.push_back(request);
        }
        self.refresh_save_destinations();
        Ok(id)
    }

    fn refresh_save_destinations(&self) {
        let Some(worker) = &self.files.worker else { return };
        let mut destinations: Vec<_> = self
            .session
            .tabs()
            .iter()
            .filter(|tab| {
                !self.files.failed_opens.contains(&tab.id)
                    || self.views.get(&tab.id).is_some_and(|view| view.editor().is_some() || view.file().is_some())
            })
            .filter_map(|tab| tab.path.clone().map(|path| (tab.id, path)))
            .collect();
        destinations.extend(self.files.saves.iter().filter_map(|(id, save)| save.path.clone().map(|path| (*id, path))));
        worker.set_save_destinations(destinations);
    }

    pub fn activate(&mut self, id: DocumentId) -> bool {
        self.sync_active();
        let previous = self.session.active();
        let activated = self.session.activate(id);
        if activated && previous != self.session.active() {
            self.input_epoch = self.input_epoch.wrapping_add(1);
            if let Some(previous) = previous {
                self.cancel_hex_input_for(previous);
            }
            self.cancel_hex_input_for(id);
        }
        activated
    }

    pub fn cycle(&mut self, backwards: bool) -> Damage {
        self.cancel_file_prompt();
        self.sync_active();
        let tabs = self.session.tabs();
        let current = tabs.iter().position(|tab| tab.id == self.active_id()).unwrap_or(0);
        let next = if backwards { (current + tabs.len() - 1) % tabs.len() } else { (current + 1) % tabs.len() };
        let target = tabs[next].id;
        self.activate(target);
        self.checkpoint_session();
        Damage { full: true, status: true, ..Default::default() }
    }

    /// Refuse dirty close until the caller saves or explicitly chooses discard.
    pub fn close(&mut self, id: DocumentId, discard: bool) -> io::Result<bool> {
        if self.files.queue.len() >= 254 {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "Recovery queue must drain before closing"));
        }
        if self.document_busy(id)
            || self
                .views
                .get(&id)
                .and_then(View::editor)
                .is_some_and(|editor| editor.has_pending_edit_receipt() || editor.has_pending_save_completion())
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Accepted edit/recovery work must finish before closing",
            ));
        }
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
            self.files.history_wait = Some(id);
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
        self.files.metadata_durable = false;
        if self.files.history_wait == Some(id) {
            self.files.history_wait = None;
        }
        if let Some(mut view) = self.views.remove(&id) {
            view.input_incarnation = std::sync::Arc::new(());
            self.input_epoch = self.input_epoch.wrapping_add(1);
            self.closed_views.insert(id, view);
        }
        self.cancel_hex_input_for(id);
        self.files.failed_opens.remove(&id);
        self.retire_binary_offer(id);
        self.files.queue.retain(|request| !matches!(request, DocumentRequest::Open { document, .. } | DocumentRequest::Index { document, .. } if *document == id));
        self.retire_closed_hex_editors();
        self.closed_views.retain(|id, _| self.session.closed().iter().any(|tab| tab.id == *id));
        self.retain_hex_history();
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
        self.refresh_save_destinations();
        Ok(true)
    }

    pub fn closed_path(&self) -> Option<PathBuf> {
        self.session.closed().iter().find_map(|tab| tab.path.clone())
    }

    fn check_reopen_destination(&self) -> io::Result<()> {
        let Some(tab) = self.session.closed().front() else { return Ok(()) };
        if let Some(path) = &tab.path
            && (self.session.tabs().iter().any(|open| {
                open.path.as_ref().is_some_and(|reserved| kwil_core_workspace::paths_may_alias(reserved, path))
            }) || self.files.saves.values().any(|save| {
                save.path.as_ref().is_some_and(|reserved| kwil_core_workspace::paths_may_alias(reserved, path))
            }))
        {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "Closed path is open or reserved by a save"));
        }
        Ok(())
    }

    fn reopen_closed(&mut self, keep_path: bool) -> io::Result<()> {
        self.check_reopen_destination()?;
        if let Some(tab) = self.session.closed().front() {
            let cached = self.closed_views.get(&tab.id);
            if cached.is_none_or(|view| view.editor().is_none()) && self.files.queue.len() >= 256 {
                return Err(io::Error::other("File worker queue is full; retry reopening"));
            }
            if cached.is_some_and(|view| view.editor().is_none() && view.path.is_none())
                && tab.revision == 0
                && tab.saved_revision == 0
                && tab.recovery_path.is_none()
            {
                return Err(io::Error::other("Closed document requires recovery"));
            }
        }
        if let Some(id) = self.session.reopen_closed()? {
            if !keep_path {
                let tab = self.session.tab_mut(id).unwrap();
                if let Some(path) = tab.path.take() {
                    tab.recovery_path = Some(path);
                }
            }
            if !keep_path && let Some(view) = self.closed_views.get_mut(&id) {
                view.path = None;
                if let Some(editor) = view.editor_mut() {
                    editor.document.path = None;
                    editor.document.original = None;
                    editor.document.mark_unsaved();
                }
                view.set_notice("Closed backing cannot be verified; use Save As".into());
            }
            // Publish before recovery/index requests can pump more saves. The
            // worker also retains its provisional reservation until release.
            self.refresh_save_destinations();
            if let Some(view) = self.closed_views.remove(&id) {
                self.views.insert(id, view);
                if self.views[&id].editor().is_none() {
                    let tab = self.session.tabs().iter().find(|tab| tab.id == id).unwrap().clone();
                    self.files.queue.retain(|request|!matches!(request,DocumentRequest::Open {document,..}|DocumentRequest::Index {document,..} if *document==id));
                    let request = if tab.revision != 0 || tab.saved_revision != 0 || tab.recovery_path.is_some() {
                        DocumentRequest::Recover { tab }
                    } else if let Some(path) = &self.views[&id].path {
                        let path = path.clone();
                        self.checked_open_request(id, 0, path)?
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
                view.top = tab.first_visible_line;
                view.set_horizontal_window(tab.first_byte_column, 64 << 10);
                view.set_notice("Recovering closed tabâ€¦".into());
                self.views.insert(id, view);
                self.files.queue.push_back(DocumentRequest::Recover { tab });
                self.pump_files();
            }
        }
        Ok(())
    }

    pub fn sync_active(&mut self) {
        let id = self.active_id();
        let hex_navigation = self.hex_navigation(id);
        let Some(view) = self.views.get(&id) else { return };
        if let Some(tab) = self.session.tab_mut(id) {
            sync_tab_path(tab, &view.path);
            tab.first_visible_line = view.top();
            tab.first_byte_column = view.first_byte_column;
            if let Some(editor) = view.editor() {
                tab.revision = editor.document().revision();
                tab.saved_revision = editor.document().saved_revision();
                tab.cursor = editor.primary_cursor().head;
                tab.anchor = editor.primary_cursor().anchor;
            }
            if let Some((cursor, anchor, top)) = hex_navigation {
                tab.cursor = cursor;
                tab.anchor = anchor;
                tab.first_visible_line = top;
                tab.first_byte_column = 0;
            }
        }
    }

    pub fn sync_all(&mut self) {
        for tab in self.session.tabs().iter().map(|t| t.id).collect::<Vec<_>>() {
            let hex_navigation = self.hex_navigation(tab);
            if let (Some(view), Some(metadata)) = (self.views.get(&tab), self.session.tab_mut(tab)) {
                sync_tab_path(metadata, &view.path);
                metadata.first_visible_line = view.top();
                metadata.first_byte_column = view.first_byte_column;
                if let Some(editor) = view.editor() {
                    metadata.revision = editor.document().revision();
                    metadata.saved_revision = editor.document().saved_revision();
                    metadata.cursor = editor.primary_cursor().head;
                    metadata.anchor = editor.primary_cursor().anchor;
                }
                if let Some((cursor, anchor, top)) = hex_navigation {
                    metadata.cursor = cursor;
                    metadata.anchor = anchor;
                    metadata.first_visible_line = top;
                    metadata.first_byte_column = 0;
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
        self.refresh_save_destinations();
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
        let tabs = self.session.tabs().to_vec();
        for tab in tabs {
            let view = &self.views[&tab.id];
            if view.editor().is_none()
                && !self.binary.flights.contains_key(&tab.id)
                && let Some(path) = &view.path
                && view.file().is_some()
            {
                let path = path.clone();
                match self.checked_open_request(tab.id, tab.revision, path) {
                    Ok(request) => self.files.queue.push_back(request),
                    Err(error) => {
                        self.views.get_mut(&tab.id).unwrap().set_notice(error.to_string());
                    }
                }
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
        self.pump_binary_retirement();
        while let Some(request) = self.files.queue.pop_front() {
            match self.files.worker.as_ref().unwrap().try_submit(request) {
                Ok(()) => {}
                Err(std::sync::mpsc::TrySendError::Full(request)) => {
                    self.files.queue.push_front(*request);
                    break;
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(request)) => {
                    if matches!(
                        &*request,
                        DocumentRequest::DiscardHex { .. }
                            | DocumentRequest::OpenChecked { .. }
                            | DocumentRequest::RetireSource { .. }
                            | DocumentRequest::ResolveBinary { choice: BinaryChoice::Cancel, .. }
                    ) {
                        // Never destroy the last raw Editor/undo root on the UI
                        // merely because its disposal lane disconnected.
                        self.files.queue.push_front(*request);
                        self.cancel_close_intent();
                        self.set_notice("Source ownership awaits the document worker".into());
                        break;
                    }
                    if let DocumentRequest::ValidateReopen { document, generation, .. } = &*request
                        && self.files.reopen == Some((*document, *generation))
                    {
                        self.files.reopen = None;
                    }
                    if let DocumentRequest::Save { document, .. } | DocumentRequest::SaveRaw { document, .. } = *request
                    {
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
        if self.hex.disposing {
            return;
        }
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
        let mut snapshot = self.session.clone();
        for id in self.binary.flights.keys() {
            if let Some(tab) = snapshot.tab_mut(*id) {
                // An undecided open has no accepted recovery interpretation.
                // Persist other work normally, but never restore this physical
                // binary later as implicitly accepted decoded text.
                tab.path = None;
                tab.recovery_path = None;
            }
        }
        self.files.queue.push_back(DocumentRequest::Persist(snapshot));
        self.pump_files();
    }

    pub fn set_session_restore(&mut self, enabled: bool) {
        self.files.restore_session = Some(enabled);
    }

    fn install_journal_observers(&mut self) {
        if self.hex.disposing || !self.files.storage_ready {
            return;
        }
        let Some(worker) = &self.files.worker else { return };
        for (&document, view) in &mut self.views {
            if self.files.observed.contains_key(&document) || self.hex.loans.contains_key(&document) {
                continue;
            }
            let Some(editor) = view.editor_mut() else { continue };
            let next = editor.next_edit_sequence();
            self.files.next_observer = self.files.next_observer.checked_add(1).expect("observer generation exhausted");
            let generation = self.files.next_observer;
            let path = editor.document().path().map(PathBuf::from);
            let policy = crate::document_worker::document_policy(editor.document());
            let observer = worker.observer(document, path.clone(), next, generation, policy);
            if let Err(error) = editor.set_edit_observer(Some(observer)) {
                view.set_notice(format!("Recovery observer unavailable: {error}"));
                continue;
            }
            self.files.observed.insert(document, (next, generation));
            let seed = self.hex.seed.remove(&document)
                || editor.document().representation() == crate::hex_session::Representation::RawBytes;
            if editor.document().is_dirty() || seed {
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
                    policy,
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
            && !self.search_replacement_pending()
            && self.files.replacement_journals.is_empty()
            && (self.files.worker.is_none() || self.files.queue.is_empty())
            && !self.views.values().chain(self.closed_views.values()).any(|view| {
                view.editor()
                    .is_some_and(|editor| editor.has_pending_edit_receipt() || editor.has_pending_save_completion())
            })
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
        self.files.history_wait = None;
        self.files.exit_without_metadata = false;
    }
    pub fn cancel_file_prompt(&mut self) {
        self.hex.prompt = false;
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
                if std::mem::take(&mut self.hex.prompt) {
                    let (_, outcome) = self.open_hex_path(prompt.text.into(), self.rows())?;
                    if outcome == crate::hex_session::OpenOutcome::NeedsRawReopen {
                        self.set_notice("Save decoded changes before reopening physical bytes".into());
                    }
                } else {
                    self.open_path(prompt.text.into())?;
                }
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
        if let Some(id) = self
            .files
            .saves
            .iter()
            .find(|(_, save)| {
                save.path.as_ref().is_some_and(|reserved| kwil_core_workspace::paths_may_alias(reserved, &path))
            })
            .map(|(id, _)| *id)
        {
            self.activate(id);
            return Ok(id);
        }
        if let Some(id) = self
            .session
            .tabs()
            .iter()
            .find(|t| t.path.as_ref().is_some_and(|reserved| kwil_core_workspace::paths_may_alias(reserved, &path)))
            .map(|t| t.id)
        {
            self.activate(id);
            return Ok(id);
        }
        self.cancel_file_prompt();
        self.sync_active();
        let previous = self.session.active();
        let id = self.session.open(Some(path.clone()))?;
        if previous != self.session.active() {
            self.input_epoch = self.input_epoch.wrapping_add(1);
            if let Some(previous) = previous {
                self.cancel_hex_input_for(previous);
            }
        }
        let mut view = View::new(None);
        view.editor = None;
        view.path = Some(path.clone());
        view.set_notice("Openingâ€¦".into());
        self.views.insert(id, view);
        self.refresh_save_destinations();
        let request = self.checked_open_request(id, 0, path)?;
        self.binary.flights.get_mut(&id).unwrap().return_to = previous;
        self.files.queue.push_back(request);
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
                if self.hex_owns_document(id) {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "Switch physical bytes to text before opening a text-line location",
                    ));
                }
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
        if self.document_busy(id) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Replacement in progress; retry save after completion",
            ));
        }
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
            && (self.session.tabs().iter().any(|tab| {
                tab.id != id
                    && tab.path.as_ref().is_some_and(|reserved| kwil_core_workspace::paths_may_alias(reserved, path))
            }) || self.files.saves.iter().any(|(document, save)| {
                *document != id
                    && save.path.as_ref().is_some_and(|reserved| kwil_core_workspace::paths_may_alias(reserved, path))
            }))
        {
            self.cancel_close_intent();
            return Err(io::Error::other("Save As target is open in another tab"));
        }
        let editor = self
            .views
            .get_mut(&id)
            .and_then(View::editor_mut)
            .ok_or_else(|| io::Error::other("Indexing: editing will be available when ready"))?;
        if let Some(settings) = &self.editor_settings {
            editor.settings = settings.clone();
        }
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
        let plan = editor.document().save_plan();
        let raw = editor.document().representation() == crate::hex_session::Representation::RawBytes;
        let request = match if raw { editor.raw_save_request(path) } else { editor.save_request(path) } {
            Ok(request) => request,
            Err(error) => {
                self.cancel_close_intent();
                return Err(error);
            }
        };
        self.files.saves.insert(id, SaveFlight { revision: request.revision, path: Some(request.path.clone()) });
        self.refresh_save_destinations();
        self.files.queue.push_back(if raw {
            DocumentRequest::SaveRaw { document: id, request }
        } else {
            DocumentRequest::Save { document: id, request, plan }
        });
        if let Some(view) = self.views.get_mut(&id) {
            view.set_notice("Savingâ€¦".into());
        }
        self.pump_files();
        Ok(())
    }

    /// Commands are intercepted before Vim/editor dispatch, preserving keymap tombstones.
    pub fn file_command(&mut self, command: &str) -> io::Result<bool> {
        if self.hex.disposing {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "Raw document disposal is in progress"));
        }
        match command {
            "file.openHex" => {
                self.file_command("file.open")?;
                self.hex.prompt = true;
            }
            "view.toggleHex" => {
                if self.toggle_hex(self.rows())? == crate::hex_session::OpenOutcome::NeedsRawReopen {
                    self.set_notice("Save changes before switching between text and physical bytes".into());
                }
            }
            "file.new" => {
                self.cancel_file_prompt();
                self.open(View::new(None))?;
            }
            "file.open" | "file.recent" | "file.saveAs" => {
                self.hex.prompt = false;
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
                self.files.close = [self.files.history_wait.unwrap_or(self.active_id())].into();
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
                if self.files.reopen.is_some() {
                    return Err(io::Error::other("Closed-tab admission already in progress"));
                }
                self.check_reopen_destination()?;
                self.cancel_file_prompt();
                self.sync_active();
                if let Some(tab) = self.session.closed().front()
                    && let Some(path) = &tab.path
                    && self.files.worker.is_some()
                {
                    if self.files.queue.len() >= 256 {
                        return Err(io::Error::other("File worker queue is full; retry reopening"));
                    }
                    self.files.next_reopen = self.files.next_reopen.wrapping_add(1);
                    let generation = self.files.next_reopen;
                    let document = tab.id;
                    let cached = self.closed_views.get(&document).and_then(View::editor);
                    let original = cached.and_then(|editor| editor.document().original.clone());
                    self.files.queue.push_back(DocumentRequest::ValidateReopen {
                        document,
                        generation,
                        path: path.clone(),
                        original,
                        cached: cached.is_some(),
                    });
                    self.files.reopen = Some((document, generation));
                    self.pump_files();
                } else {
                    self.reopen_closed(true)?;
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

    /// Existing hot-exit policy used by the terminal frontend and recovery callers.
    pub fn request_window_close(&mut self, discard: bool) -> io::Result<()> {
        self.request_close_policy(discard, true)
    }
    /// Desktop close asks about dirty tabs even when restoring the session is enabled.
    /// This retains the same accepted-save and metadata-durability gates.
    pub fn request_window_close_with_prompt(&mut self) -> io::Result<()> {
        self.request_close_policy(false, false)
    }
    pub fn discard_pending_close(&mut self) -> io::Result<()> {
        if self.files.quit { self.request_close_policy(true, false) } else { self.discard_close_target() }
    }
    fn request_close_policy(&mut self, discard: bool, allow_hot_exit: bool) -> io::Result<()> {
        if discard && self.files.manual_guard {
            self.files.exit_without_metadata = true;
        }
        if allow_hot_exit
            && !discard
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
            if self.document_busy(id) {
                self.set_notice("Replacement in progress; close will wait. Esc cancels close".into());
                return Ok(());
            }
            if self.files.saves.contains_key(&id) {
                self.set_notice("Save in progress; close will wait for its revision".into());
                return Ok(());
            }
            if self
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
        self.set_notice("Preserving tabs and unsaved workâ€¦ Esc cancels; Ctrl+Shift+Q discards this tab".into());
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
            || self.search_replacement_pending()
            || !self.files.metadata_durable
            || self.files.persist_inflight
            || self.files.persist_pending
            || self.files.reopen.is_some()
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
        if matches!(&result, DocumentResult::Preview { document, .. } if self.hex_text_preview_pending(*document)) {
            let DocumentResult::Preview { document, revision, preview, .. } = result else { unreachable!() };
            self.hex_text_preview(document, revision, preview.text);
            return Damage::default();
        }
        // An editor absent because of a loan is not an open/index placeholder.
        let placeholder = match &result {
            DocumentResult::Indexed { document, .. }
            | DocumentResult::Opened { document, .. }
            | DocumentResult::Preview { document, .. }
            | DocumentResult::Recovered { document, .. } => Some(*document),
            _ => None,
        };
        if placeholder.is_some_and(|id| self.files.loans.contains_key(&id) || self.hex.editor_away(id)) {
            return Damage::default();
        }
        let destinations_changed = matches!(
            &result,
            DocumentResult::Prepared(_)
                | DocumentResult::CheckedOpened { .. }
                | DocumentResult::Recovered { .. }
                | DocumentResult::Opened { .. }
                | DocumentResult::Preview { .. }
                | DocumentResult::Saved { .. }
        );
        let checkpoint = matches!(
            &result,
            DocumentResult::Indexed { .. }
                | DocumentResult::Saved { .. }
                | DocumentResult::Recovered { .. }
                | DocumentResult::Replaced { .. }
        );
        let mut damage = Damage { status: true, ..Default::default() };
        match result {
            DocumentResult::BinaryOffer(offer) => {
                damage = self.accept_binary_offer(offer, None);
            }
            DocumentResult::BinaryRefused { offer, error } => {
                damage = self.accept_binary_offer(offer, Some(error));
            }
            DocumentResult::CheckedPreview { ticket, preview } => {
                damage = self.checked_text_preview(ticket, preview.text);
            }
            DocumentResult::CheckedRawPreview { ticket, page } => {
                if self.binary_current(ticket)
                    && matches!(
                        self.binary.flights[&ticket.document()].phase,
                        BinaryPhase::Resolving(BinaryChoice::Hex)
                    )
                {
                    self.binary.flights.get_mut(&ticket.document()).unwrap().preview_ready = false;
                    let (preview_damage, accepted) =
                        self.hex_preview_accepted(ticket.document(), ticket.revision(), page);
                    if accepted {
                        self.binary.flights.get_mut(&ticket.document()).unwrap().preview_ready = true;
                    }
                    damage = preview_damage;
                }
            }
            DocumentResult::CheckedOpened { ticket, result } => {
                damage = self.checked_opened(ticket, result);
            }
            DocumentResult::RawPreview { document, revision, page } => {
                damage = self.hex_preview(document, revision, page);
            }
            DocumentResult::RawReopened { document, generation, editor, result } => {
                damage = self.hex_reopened(document, generation, editor, result);
            }
            DocumentResult::HexPage { document, editor, request, result } => {
                damage = self.hex_page_result(document, editor, request, result);
            }
            DocumentResult::HexEdited { document, editor, completion } => {
                damage = self.hex_edit_result(document, editor, completion);
            }
            DocumentResult::Replaced { key, operation, mut editor, result } => {
                let id = DocumentId(key.document_id);
                let current =
                    self.files.loans.get(&id).is_some_and(|loan| loan.key == key && loan.operation == operation);
                if !current {
                    // Such a completion cannot be emitted by the fixed lane:
                    // accepted loans cannot be closed/remapped/replaced.
                    // Never overwrite another editor with a stale completion.
                    self.set_notice("Unexpected replacement completion identity".into());
                    return damage;
                }
                if let Some(settings) = &self.editor_settings {
                    editor.settings = settings.clone();
                }
                let sequence = editor.next_edit_sequence().saturating_sub(1);
                if editor.document().revision() != key.revision {
                    self.files.replacement_journals.insert(id, sequence);
                }
                let view = self.views.get_mut(&id).expect("loan target cannot close");
                debug_assert!(view.editor.is_none());
                view.editor = Some(*editor);
                self.files.loans.remove(&id);
                self.input_epoch = self.input_epoch.wrapping_add(1);
                let notice = match result {
                    Ok(_) => "Replacement complete".to_string(),
                    Err(error) => format!("Replacement failed: {error}"),
                };
                // The worker's viewport may predate a resize. Rebuild only the
                // existing bounded visible cache once ownership is restored.
                view.set_notice(notice);
                let refreshed = view.refresh_after_edit(Damage { full: true, status: true, ..Default::default() });
                if id == self.active_id() {
                    damage = refreshed;
                }
                if self.files.hot_exit {
                    self.files.queue.push_back(DocumentRequest::FlushJournals);
                }
            }
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
            DocumentResult::ReopenValidated { document, generation, result } => {
                let current = self.files.reopen == Some((document, generation));
                if current {
                    self.files.reopen = None;
                    let result = result.and_then(|keep_path| {
                        if self.session.closed().front().is_none_or(|tab| tab.id != document) {
                            return Err(io::Error::other("Closed history changed; retry reopening"));
                        }
                        self.reopen_closed(keep_path)
                    });
                    match result {
                        Ok(()) => {
                            damage.full = true;
                            self.checkpoint_session();
                        }
                        Err(error) => {
                            self.set_notice(format!("Reopen failed: {error}"));
                        }
                    }
                }
                // Publish the committed registry before releasing worker-owned
                // admission. Stale/error receipts consume no closed history.
                self.refresh_save_destinations();
                self.files.queue.push_back(DocumentRequest::ReleaseReopen { document, generation });
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
                            if document.path != tab.path || (document.path.is_none() && tab.recovery_path.is_some()) {
                                view.path.clone_from(&document.path);
                                sync_tab_path(self.session.tab_mut(tab.id).unwrap(), &document.path);
                                view.set_notice(recovery_notice(&tab));
                            }
                            let mut editor = Editor::new(*document);
                            if let Some(settings) = &self.editor_settings {
                                editor.settings = settings.clone();
                            }
                            let raw =
                                editor.document().representation() == crate::hex_session::Representation::RawBytes;
                            let len = editor.document().hex_source(tab.id.0).len;
                            if !raw {
                                let _ = editor.set_cursors(
                                    vec![crate::Cursor {
                                        head: tab.cursor.min(len),
                                        anchor: tab.anchor.min(len),
                                        desired_column: None,
                                    }],
                                    0,
                                );
                            }
                            editor.set_viewport(tab.first_visible_line, view.rows);
                            view.editor = Some(editor);
                            view.top = tab.first_visible_line;
                            if raw {
                                view.file = view.editor().unwrap().document().original.clone();
                            } else {
                                view.set_horizontal_window(tab.first_byte_column, 64 << 10);
                                view.refresh_after_edit(Damage { full: true, ..Default::default() });
                            }
                        }
                        Err(error) => {
                            view.set_notice(format!("Recovery failed: {error}; journal retained"));
                        }
                    }
                    damage.full = self.session.active() == Some(tab.id);
                }
            }
            DocumentResult::Preview { document, revision, path, file, preview } => {
                if let Some(view) = self.views.get_mut(&document).filter(|v| v.editor().is_none() && revision == 0) {
                    self.files.failed_opens.remove(&document);
                    // Keep deferred navigation, viewport and tab identity. Only
                    // the bounded worker-prepared text cache is replaced here.
                    view.path = Some(path);
                    view.file = Some(file);
                    view.preview = Some(preview.text);
                    let rows = view.rows();
                    view.set_rows(rows);
                    view.set_notice("Preparing decoded documentâ€¦".into());
                    damage.full = self.active_id() == document;
                }
            }
            DocumentResult::Opened { document, revision, result } => {
                if let Some(view) = self.views.get_mut(&document).filter(|v| v.editor().is_none() && revision == 0) {
                    match result {
                        Ok((path, file)) => {
                            self.files.failed_opens.remove(&document);
                            let rows = view.rows();
                            view.path = Some(path);
                            view.file = Some(file);
                            view.set_rows(rows);
                        }
                        Err(error) => {
                            view.set_notice(format!("Open failed: {error}"));
                            self.files.failed_opens.insert(document);
                        }
                    }
                    damage.full = self.active_id() == document;
                }
            }
            DocumentResult::Indexed { document, revision, result } => {
                if let Some(view) = self.views.get_mut(&document).filter(|v| v.editor().is_none() && revision == 0) {
                    match result {
                        Ok(document) => {
                            let raw = document.representation() == crate::hex_session::Representation::RawBytes;
                            let mut editor = Editor::new(*document);
                            if let Some(settings) = &self.editor_settings {
                                editor.settings = settings.clone();
                            }
                            editor.set_viewport(view.top, view.rows);
                            if raw {
                                view.file = editor.document().original.clone();
                            }
                            view.editor = Some(editor);
                            if !raw {
                                view.index_ready();
                            }
                            view.set_notice(String::new());
                        }
                        Err(error) => {
                            self.files.failed_opens.insert(document);
                            view.set_notice(format!("Index failed: {error}"));
                        }
                    }
                    damage.full = self.active_id() == document;
                }
                self.hex_indexed(document);
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
                        if self.files.replacement_journals.get(&document).is_some_and(|needed| sequence >= *needed) {
                            self.files.replacement_journals.remove(&document);
                        }
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
                self.hex_journal_closed(document, &result);
                damage = Damage::default();
                if result.is_ok() {
                    self.files.replacement_journals.remove(&document);
                }
                if let Err(error) = result {
                    damage = self
                        .set_notice(format!("Tab {} journal close incomplete: {error}; recovery retained", document.0));
                }
            }
        }
        self.bind_raw_sessions();
        if checkpoint {
            self.checkpoint_session();
        }
        if destinations_changed {
            self.refresh_save_destinations();
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
            && self.files.history_wait.is_none()
            && let Err(error) = self.advance_close(false)
        {
            self.set_notice(error.to_string());
            self.cancel_close_intent();
        }
        self.check_hot_exit();
        self.pump_files();
        self.pump_hex();
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
                let edited = if view
                    .editor()
                    .is_some_and(|e| e.document().representation() == crate::hex_session::Representation::RawBytes)
                {
                    view.file = view.editor().unwrap().document().original.clone();
                    result.damage
                } else {
                    view.refresh_after_edit(result.damage)
                };
                if self.session.active() == Some(document) {
                    damage = edited;
                    damage.status = true;
                }
                if let Some(path) = &view.path
                    && let Err(error) = if self.session.tabs().iter().any(|tab| {
                        tab.id != document
                            && tab
                                .path
                                .as_ref()
                                .is_some_and(|reserved| kwil_core_workspace::paths_may_alias(reserved, path))
                    }) {
                        Err(io::Error::other("Save As target aliases another tab"))
                    } else {
                        self.session.set_path(document, path.clone())
                    }
                {
                    // Admission reserves destinations, but a stale or inconsistent
                    // metadata result must still fail visibly and never close.
                    may_close = false;
                    warning = true;
                    let recovery_path = view.path.take();
                    if let Some(tab) = self.session.tab_mut(document) {
                        tab.path = None;
                        tab.recovery_path = recovery_path;
                    }
                    if let Some(editor) = view.editor_mut() {
                        editor.document.path = None;
                        editor.document.original = None;
                        editor.document.mark_unsaved();
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
            // An atomic current save is also durable evidence for the accepted
            // replacement when its recovery journal failed independently.
            self.files.replacement_journals.remove(&document);
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
        self.refresh_save_destinations();
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
        if !self.hex.loans.is_empty() {
            self.files.deferred_recovery = Some(Box::new(prepared));
            return;
        }
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
        // Only bounded display state is discarded. Editor/backing ownership
        // remains in old_views while document identities are remapped below.
        self.hex = HexFlow::default();
        self.input_epoch = self.input_epoch.wrapping_add(1);
        self.session = prepared.session;
        for (id, document) in prepared.documents {
            let tab = self.session.tabs().iter().find(|t| t.id == id).unwrap().clone();
            let mut view = View::new(None);
            view.path = tab.path.clone();
            match document {
                Ok(document) => {
                    if document.path != tab.path || (document.path.is_none() && tab.recovery_path.is_some()) {
                        view.path.clone_from(&document.path);
                        sync_tab_path(self.session.tab_mut(id).unwrap(), &document.path);
                        view.set_notice(recovery_notice(&tab));
                    }
                    let mut editor = Editor::new(document);
                    if let Some(settings) = &self.editor_settings {
                        editor.settings = settings.clone();
                    }
                    let raw = editor.document().representation() == crate::hex_session::Representation::RawBytes;
                    let len = editor.document().hex_source(id.0).len;
                    if !raw {
                        let _ = editor.set_cursors(
                            vec![crate::Cursor {
                                head: tab.cursor.min(len),
                                anchor: tab.anchor.min(len),
                                desired_column: None,
                            }],
                            0,
                        );
                    }
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
            if view
                .editor()
                .is_none_or(|e| e.document().representation() != crate::hex_session::Representation::RawBytes)
            {
                view.set_horizontal_window(tab.first_byte_column, 64 << 10);
            }
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
                        && !self.hex_owns_document(existing)
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
                        editor.document.original = None;
                        editor.document.mark_unsaved();
                    }
                }
                let metadata = self.session.tab_mut(existing).unwrap();
                sync_tab_path(metadata, &None);
                if let Some(view) = self.views.get_mut(&existing) {
                    view.set_notice(recovery_notice(metadata));
                }
            }
            let id = self.session.open(view.path.clone()).expect("validated paths and reserved recovery capacity");
            mapping.insert(tab.id, id);
            if tab.id == old_active {
                active = Some(id);
            }
            self.views.insert(id, view);
        }
        // Requests captured while recovery was preparing retain their source tab.
        for mut request in std::mem::take(&mut self.files.queue) {
            if let DocumentRequest::OpenChecked { ticket, .. } = &request
                && mapping
                    .get(&ticket.document)
                    .and_then(|id| self.views.get(id))
                    .is_some_and(|view| view.editor().is_some())
            {
                // The restored journal already owns the accepted text/raw
                // policy. Its duplicate startup source must never re-offer or
                // decode, nor lose its last mapping on this UI thread.
                if let DocumentRequest::OpenChecked { file: Some(file), .. } = request {
                    self.files.queue.push_back(DocumentRequest::RetireSource { file });
                }
                continue;
            }
            match &mut request {
                DocumentRequest::OpenChecked { ticket, .. } => {
                    if let Some(id) = mapping.get(&ticket.document) {
                        ticket.document = *id;
                    }
                }
                DocumentRequest::Open { document, .. }
                | DocumentRequest::Index { document, .. }
                | DocumentRequest::Save { document, .. } => {
                    if let Some(id) = mapping.get(document) {
                        *document = *id;
                    }
                }
                _ => {}
            }
            self.files.queue.push_back(request);
        }
        self.binary.flights = std::mem::take(&mut self.binary.flights)
            .into_iter()
            .filter_map(|(old, mut flight)| {
                let id = mapping.get(&old).copied()?;
                if self.views.get(&id).is_none_or(|view| view.editor().is_some()) {
                    return None;
                }
                flight.ticket.document = id;
                flight.return_to = flight.return_to.and_then(|previous| mapping.get(&previous).copied());
                Some((id, flight))
            })
            .collect();
        self.files.saves = std::mem::take(&mut self.files.saves)
            .into_iter()
            .map(|(id, save)| (*mapping.get(&id).unwrap_or(&id), save))
            .collect();
        self.files.failed_opens = std::mem::take(&mut self.files.failed_opens)
            .into_iter()
            .filter_map(|id| mapping.get(&id).copied())
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
        self.bind_raw_sessions();
        self.refresh_save_destinations();
    }

    /// Recovery state of the active document, for typed status fields.
    pub fn recovery_notice(&self) -> Option<&'static str> {
        if self.files.journal_failed.contains_key(&self.active_id())
            || (self.files.worker.is_some() && !self.files.preparing && !self.files.storage_ready)
        {
            Some("Recovery unavailable")
        } else if self.recovery_pending(self.active_id()) {
            Some("Recovery pending")
        } else {
            None
        }
    }

    pub fn status_text(&self) -> String {
        let tabs = self.session.tabs();
        let position = tabs.iter().position(|t| t.id == self.active_id()).unwrap_or(0) + 1;
        let recovery = self.recovery_notice().map_or_else(String::new, |notice| format!("  |  {notice}"));
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

fn sync_tab_path(tab: &mut kwil_core_workspace::session::Tab, path: &Option<PathBuf>) {
    if let Some(path) = path {
        tab.path = Some(path.clone());
        tab.recovery_path = None;
    } else if let Some(previous) = tab.path.take() {
        tab.recovery_path = Some(previous);
    }
}

fn recovery_notice(tab: &kwil_core_workspace::session::Tab) -> String {
    format!(
        "Recovered backing cannot be verified for {}; original recovery retained. Use Save As",
        tab.path
            .as_ref()
            .or(tab.recovery_path.as_ref())
            .map_or_else(|| "this tab".into(), |path| path.display().to_string()),
    )
}

impl Workspace {
    /// Scalar-only offer for the active tab. Hosts capture its exact ticket in
    /// the modal callback; never read the source to draw this prompt.
    pub fn binary_offer(&self) -> Option<BinaryOffer> {
        self.binary_offer_for(self.active_id())
    }
    pub fn binary_offer_for(&self, id: DocumentId) -> Option<BinaryOffer> {
        let flight = self.binary.flights.get(&id)?;
        if !self.binary_current(flight.ticket) {
            return None;
        }
        match flight.phase {
            BinaryPhase::Offered(offer) => Some(offer),
            _ => None,
        }
    }
    pub fn binary_offer_pending(&self, id: DocumentId) -> bool {
        self.binary.flights.get(&id).is_some_and(|flight| self.binary_current(flight.ticket))
    }
    /// Paint authority for a worker-vetted, bounded first screen only. This
    /// never releases ownership or input: binary_offer_pending remains true
    /// until the final checked completion restores the editor. Hosts may paint
    /// these cached text/hex rows while every body mutation stays fenced.
    pub fn binary_preview_ready(&self, id: DocumentId) -> bool {
        self.binary.flights.get(&id).is_some_and(|flight| {
            flight.preview_ready
                && self.binary_current(flight.ticket)
                && matches!(
                    flight.phase,
                    BinaryPhase::Opening | BinaryPhase::Resolving(BinaryChoice::Text | BinaryChoice::Hex)
                )
        })
    }
    fn binary_current(&self, ticket: BinaryTicket) -> bool {
        self.binary.flights.get(&ticket.document()).is_some_and(|flight| {
            flight.ticket == ticket
                && self
                    .views
                    .get(&ticket.document())
                    .is_some_and(|view| view.editor.is_none() && view.path.as_ref() == Some(&flight.path))
                && self.session.tabs().iter().any(|tab| {
                    tab.id == ticket.document()
                        && tab.revision == ticket.revision()
                        && tab.path.as_ref() == Some(&flight.path)
                })
        })
    }
    fn checked_open_request(&mut self, id: DocumentId, revision: u64, path: PathBuf) -> io::Result<DocumentRequest> {
        if self.binary.flights.len() + self.binary.retiring.len() >= kwil_core_workspace::session::MAX_TABS
            || self.binary.flights.contains_key(&id)
            || path.capacity() > kwil_core_workspace::session::MAX_PATH_BYTES
            || path.as_os_str().as_encoded_bytes().len() > kwil_core_workspace::session::MAX_PATH_BYTES
        {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "Binary open admission retained"));
        }
        let generation = self
            .binary
            .next_generation
            .checked_add(1)
            .ok_or_else(|| io::Error::other("Binary open identity exhausted"))?;
        self.binary.flights.try_reserve(1).map_err(io::Error::other)?;
        // Reserve the eventual stale-owner disposal cell before any source is
        // admitted. There is no allocation proportional to source length.
        self.binary.retiring.try_reserve(self.binary.flights.len() + 1).map_err(io::Error::other)?;
        self.binary.next_generation = generation;
        let ticket = BinaryTicket { document: id, revision, generation };
        self.binary.flights.insert(
            id,
            BinaryFlight {
                ticket,
                path: path.clone(),
                phase: BinaryPhase::Opening,
                return_to: None,
                preview_ready: false,
            },
        );
        let view = self.views.get_mut(&id).expect("checked open placeholder");
        let file = view.file.take();
        view.preview = Some(Vec::new());
        view.set_rows(view.rows);
        Ok(DocumentRequest::OpenChecked { ticket, path, file })
    }
    fn accept_binary_offer(&mut self, offer: BinaryOffer, error: Option<io::Error>) -> Damage {
        if !self.binary_current(offer.ticket) {
            return Damage::default();
        }
        let id = offer.ticket.document();
        let allowed = matches!(
            (&self.binary.flights[&id].phase, error.is_some()),
            (BinaryPhase::Opening, false) | (BinaryPhase::Resolving(BinaryChoice::Hex | BinaryChoice::Text), true)
        );
        if !allowed {
            return Damage::default();
        }
        let flight = self.binary.flights.get_mut(&id).unwrap();
        flight.phase = BinaryPhase::Offered(offer);
        flight.preview_ready = false;
        // A rejected Hex admission may have installed only a bounded preview.
        // Remove that display before exposing the choice again.
        self.hex_forget_binary_open(id);
        let view = self.views.get_mut(&id).unwrap();
        view.preview = Some(Vec::new());
        view.set_rows(view.rows);
        view.set_notice(error.map_or_else(
            || "Binary file: choose Hex, Text, or Cancel".into(),
            |e| format!("Binary choice refused: {e}; source retained"),
        ));
        Damage { full: self.active_id() == id, status: true, ..Default::default() }
    }
    /// Ok means bounded mailbox admission. Full/disconnected leaves the same
    /// offer and exact worker owner available; the host may retry this ticket.
    pub fn resolve_binary_offer(
        &mut self,
        ticket: BinaryTicket,
        choice: BinaryChoice,
        rows: usize,
    ) -> io::Result<Damage> {
        if !self.binary_current(ticket)
            || ticket.document() != self.active_id()
            || self.binary_offer_for(ticket.document()).is_none()
        {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "Binary offer changed"));
        }
        if self.hex.disposing || self.files.preparing || self.files.prompt.is_some() || self.close_pending() {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "Binary choice awaits current file operation"));
        }
        self.pump_files();
        if !self.files.queue.is_empty() {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "Binary choice awaits document queue"));
        }
        let worker = self.files.worker.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "Document worker unavailable; offer retained")
        })?;
        let request = DocumentRequest::ResolveBinary { ticket, choice, rows: rows.min(crate::hex::MAX_ROWS) };
        worker.try_submit(request).map_err(|error| {
            io::Error::new(
                match error {
                    std::sync::mpsc::TrySendError::Full(_) => io::ErrorKind::WouldBlock,
                    std::sync::mpsc::TrySendError::Disconnected(_) => io::ErrorKind::NotConnected,
                },
                "Binary choice not admitted; offer retained",
            )
        })?;
        let id = ticket.document();
        let flight = self.binary.flights.get_mut(&id).unwrap();
        flight.phase = BinaryPhase::Resolving(choice);
        flight.preview_ready = false;
        self.views.get_mut(&id).unwrap().input_incarnation = std::sync::Arc::new(());
        self.input_epoch = self.input_epoch.wrapping_add(1);
        if choice == BinaryChoice::Hex {
            self.hex_begin_binary_open(id, ticket.generation(), rows);
        }
        self.views.get_mut(&id).unwrap().set_notice(
            if choice == BinaryChoice::Cancel { "Cancelling open..." } else { "Opening selected representation..." }
                .into(),
        );
        Ok(Damage { full: true, status: true, ..Default::default() })
    }
    fn checked_text_preview(&mut self, ticket: BinaryTicket, text: Vec<u8>) -> Damage {
        if !self.binary_current(ticket)
            || !matches!(
                self.binary.flights[&ticket.document()].phase,
                BinaryPhase::Opening | BinaryPhase::Resolving(BinaryChoice::Text)
            )
        {
            return Damage::default();
        }
        let view = self.views.get_mut(&ticket.document()).unwrap();
        view.preview = Some(text);
        view.set_rows(view.rows);
        view.set_notice("Preparing decoded document...".into());
        self.binary.flights.get_mut(&ticket.document()).unwrap().preview_ready = true;
        Damage { full: self.active_id() == ticket.document(), status: true, ..Default::default() }
    }
    fn checked_opened(&mut self, ticket: BinaryTicket, result: io::Result<Option<Box<crate::Document>>>) -> Damage {
        let id = ticket.document();
        if !self.binary_current(ticket) {
            if let Ok(Some(document)) = result {
                // Closed offered flights can only return a scalar Cancel. A
                // source result belongs to a still-admitted flight, even if
                // an embedder changed its path while it was on the lane.
                assert!(self.binary.flights.get(&id).is_some_and(|flight| flight.ticket == ticket));
                self.binary.flights.remove(&id);
                self.hex_forget_binary_open(id);
                self.binary.retiring.push((id, Box::new(Editor::new(*document))));
                self.pump_binary_retirement();
            } else if self.binary.flights.get(&id).is_some_and(|flight| flight.ticket == ticket) {
                self.binary.flights.remove(&id);
                self.hex_forget_binary_open(id);
            }
            return Damage::default();
        }
        if matches!(&result, Ok(None))
            && !matches!(self.binary.flights[&id].phase, BinaryPhase::Resolving(BinaryChoice::Cancel))
        {
            return Damage::default();
        }
        let flight = self.binary.flights.remove(&id).unwrap();
        match result {
            Ok(None) => {
                self.hex_forget_binary_open(id);
                // This exact flight was never accepted, and the scalar receipt
                // proves its physical source retired on the worker. Removing
                // its placeholder must not evict legitimate closed history.
                assert!(self.session.cancel_open(id));
                self.views.remove(&id);
                self.files.failed_opens.remove(&id);
                self.input_epoch = self.input_epoch.wrapping_add(1);
                if let Some(previous) = flight.return_to {
                    self.session.activate(previous);
                }
                if self.views.is_empty() {
                    self.open(View::new(None)).expect("cancelled placeholder leaves tab capacity");
                }
                self.refresh_save_destinations();
                self.checkpoint_session();
                Damage { full: true, status: true, ..Default::default() }
            }
            result => {
                let result = result.map(|document| document.expect("accepted checked source"));
                if let Ok(document) = &result {
                    self.views.get_mut(&id).unwrap().file = document.original.clone();
                } else {
                    self.hex_forget_binary_open(id);
                }
                self.document_result(DocumentResult::Indexed { document: id, revision: ticket.revision(), result })
            }
        }
    }
    fn retire_binary_offer(&mut self, id: DocumentId) {
        if let Some(flight) = self.binary.flights.remove(&id) {
            // Close is allowed only in the offered phase, never while a
            // source-bearing completion can be on its way to this view.
            debug_assert!(matches!(flight.phase, BinaryPhase::Offered(_)));
            self.files.queue.push_back(DocumentRequest::ResolveBinary {
                ticket: flight.ticket,
                choice: BinaryChoice::Cancel,
                rows: 0,
            });
        }
    }
    fn pump_binary_retirement(&mut self) {
        let Some(worker) = &self.files.worker else { return };
        while let Some((document, editor)) = self.binary.retiring.pop() {
            if let Err(error) = worker.try_submit(DocumentRequest::DiscardHex { document, editor }) {
                let (std::sync::mpsc::TrySendError::Full(request)
                | std::sync::mpsc::TrySendError::Disconnected(request)) = error;
                let DocumentRequest::DiscardHex { document, editor } = *request else { unreachable!() };
                self.binary.retiring.push((document, editor));
                break;
            }
        }
    }
    fn prepare_binary_shutdown(&mut self) -> bool {
        self.pump_files();
        let ids: Vec<_> = self
            .binary
            .flights
            .iter()
            .filter_map(|(&id, flight)| matches!(flight.phase, BinaryPhase::Offered(_)).then_some(id))
            .collect();
        for id in ids {
            if self.files.queue.len() >= 256 {
                break;
            }
            let flight = self.binary.flights.get_mut(&id).unwrap();
            flight.phase = BinaryPhase::Resolving(BinaryChoice::Cancel);
            flight.preview_ready = false;
            self.files.queue.push_back(DocumentRequest::ResolveBinary {
                ticket: flight.ticket,
                choice: BinaryChoice::Cancel,
                rows: 0,
            });
        }
        self.pump_files();
        self.binary.flights.is_empty()
            && self.binary.retiring.is_empty()
            && !self.files.queue.iter().any(|request| {
                matches!(
                    request,
                    DocumentRequest::ResolveBinary { choice: BinaryChoice::Cancel, .. }
                        | DocumentRequest::RetireSource { .. }
                        | DocumentRequest::OpenChecked { .. }
                )
            })
    }
}

#[cfg(test)]
#[path = "binary_offer_model_tests.rs"]
mod binary_offer_model_tests;

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
#[path = "replacement_tests.rs"]
mod replacement_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_merge_never_replays_text_line_navigation_into_raw_bytes() {
        let path = std::env::temp_dir().join("kwil-deferred-raw-line-no-io.bin");
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        let current = workspace.active_id();
        workspace.session.tab_mut(current).unwrap().path = Some(path.clone());
        let view = workspace.views.get_mut(&current).unwrap();
        view.path = Some(path.clone());
        view.editor = None;
        view.pending_goto = Some((2, 1, false));

        let mut session = Session::default();
        let recovered = session.open(Some(path.clone())).unwrap();
        let mut document = crate::Document::new();
        document.buffer.insert(0, b"first\nsecond\n").unwrap();
        document.path = Some(path);
        document.representation = crate::hex_session::Representation::RawBytes;
        workspace.install_prepared(crate::document_worker::PreparedWorkspace {
            session,
            documents: vec![(recovered, Ok(document))],
        });
        assert!(workspace.hex_owns_document(recovered));
        let editor = workspace.views[&recovered].editor().unwrap();
        assert_eq!(editor.cursors()[editor.primary_index()].head, 0);
        assert!(workspace.views[&recovered].pending_goto.is_none());
    }
    use crate::CommandArgs;

    #[test]
    fn file_commands_create_cycle_close_reopen_and_preserve_unsaved_tabs() {
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        let first = workspace.active_id();
        workspace
            .execute("edit.insert", &CommandArgs { text: Some("æ—¥æœ¬èªž".into()), ..Default::default() }, 1)
            .unwrap();
        assert!(workspace.file_command("file.new").unwrap());
        let second = workspace.active_id();
        assert_ne!(first, second);
        let epoch = workspace.input_epoch();
        workspace.file_command("file.previousTab").unwrap();
        assert_eq!(workspace.active_id(), first);
        assert_ne!(workspace.input_epoch(), epoch, "cycling invalidates captured input tickets");
        workspace.file_command("file.close").unwrap();
        assert!(workspace.close_pending());
        assert_eq!(workspace.session().tabs().len(), 2);
        workspace.cancel_file_prompt();
        workspace.activate(second);
        workspace.file_command("file.close").unwrap();
        workspace.file_command("file.reopenClosed").unwrap();
        assert_eq!(workspace.active_id(), second);
        workspace.activate(first);
        assert_eq!(workspace.editor().unwrap().primary_cursor().head, "æ—¥æœ¬èªž".len() as u64);
    }

    #[test]
    fn confirmed_window_close_keeps_dirty_work_pending_even_with_session_restore() {
        let make = || {
            let mut workspace = Workspace::new(View::new(None)).unwrap();
            workspace
                .execute(
                    "edit.insert",
                    &crate::CommandArgs { text: Some("keep unsaved changes".into()), ..Default::default() },
                    1,
                )
                .unwrap();
            workspace.files.restore_session = Some(true);
            workspace.files.storage_ready = true;
            workspace.files.metadata_durable = true;
            workspace
        };
        let mut desktop = make();
        desktop.request_window_close_with_prompt().unwrap();
        assert!(desktop.close_pending());
        assert!(!desktop.files.hot_exit);
        assert!(!desktop.wants_exit());
        assert!(desktop.editor().unwrap().document().is_dirty());
        desktop.cancel_close_intent();
        assert!(!desktop.close_pending());
        assert!(desktop.editor().unwrap().document().is_dirty());
        let mut terminal = make();
        terminal.request_window_close(false).unwrap();
        assert!(terminal.files.hot_exit, "the terminal's existing recovery policy is preserved");
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
    fn recovered_context_survives_sync_dedup_reservation_and_cached_reopen_without_becoming_writable() {
        let path = std::env::temp_dir().join("kwil-virtual-last-named-context.txt");
        let mut session = Session::default();
        let id = session.open(Some(path.clone())).unwrap();
        let mut document = crate::Document::new();
        document.buffer.insert(0, b"retained").unwrap();
        document.mark_unsaved();
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        workspace.install_prepared(crate::document_worker::PreparedWorkspace {
            session,
            documents: vec![(id, Ok(document))],
        });
        workspace.sync_active();
        workspace.sync_all();
        assert!(workspace.session().tabs()[0].path.is_none());
        assert_eq!(
            workspace.session().tabs()[0].recovery_path.as_ref(),
            Some(&path),
            "sync must retain last named context"
        );
        assert!(workspace.path.is_none());
        assert!(workspace.editor_mut().unwrap().save_request(None).is_err());
        let candidate = workspace.open(View::new(None)).unwrap();
        edit(&mut workspace, "explicit destination");
        workspace.save_document(candidate, Some(path.clone())).expect("context alone must not reserve a destination");
        let (owner, request) = take_save(&mut workspace);
        assert_eq!(owner, candidate);
        assert_eq!(request.path, path);
        workspace.document_result(DocumentResult::Saved {
            document: candidate,
            revision: request.revision,
            result: Err(io::Error::new(io::ErrorKind::PermissionDenied, "fake save boundary")),
        });
        workspace.close(candidate, true).unwrap();
        workspace.activate(id);
        let named = workspace.open_path(path.clone()).unwrap();
        assert_ne!(named, id, "context must not masquerade as an open-path identity");
        assert_eq!(workspace.open_path(path.clone()).unwrap(), named, "actual open paths still deduplicate");
        assert!(workspace.view(id).unwrap().path.is_none(), "opening the name must not attach recovered bytes");
        assert!(workspace.views.get_mut(&id).unwrap().editor_mut().unwrap().save_request(None).is_err());
        let other = workspace.open(View::new(None)).unwrap();
        edit(&mut workspace, "other");
        assert!(
            workspace.save_document(other, Some(path.clone())).is_err(),
            "actual open destination remains reserved"
        );
        workspace.close(other, true).unwrap();
        workspace.close(named, true).unwrap();
        workspace.activate(id);
        workspace.close(id, true).unwrap();
        workspace.files.reopen = Some((id, 1));
        workspace.document_result(DocumentResult::ReopenValidated { document: id, generation: 1, result: Ok(false) });
        assert_eq!(workspace.active_id(), id);
        let restored = workspace.session().tabs().iter().find(|t| t.id == id).unwrap();
        assert!(restored.path.is_none());
        assert_eq!(restored.recovery_path.as_ref(), Some(&path));
        assert!(workspace.path.is_none());
        let editor = workspace.editor_mut().unwrap();
        assert!(editor.save_request(None).is_err());
        assert!(editor.document().is_dirty());
        let mut bytes = Vec::new();
        editor.document().snapshot().write_to(&mut bytes).unwrap();
        assert_eq!(bytes, b"retained");
        assert!(workspace.status_text().contains("Save As"));
        workspace.file_command("file.saveAll").unwrap();
        assert_eq!(workspace.file_prompt().unwrap().kind, FilePromptKind::SaveAs);
        assert_eq!(workspace.file_prompt().unwrap().document, id);
        workspace.cancel_file_prompt();
        assert!(workspace.close(id, false).is_err());
    }

    #[test]
    fn duplicate_live_named_tab_keeps_recovered_name_context_and_both_unsaved_contents() {
        let path = std::env::temp_dir().join("kwil-virtual-duplicate-recovery-name.txt");
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        workspace.path = Some(path.clone());
        workspace.editor_mut().unwrap().document.path = Some(path.clone());
        edit(&mut workspace, "live edits");
        let mut session = Session::default();
        let recovered = session.open(Some(path.clone())).unwrap();
        let mut document = crate::Document::new();
        document.path = Some(path.clone());
        document.buffer.insert(0, b"recovered bytes").unwrap();
        workspace.install_prepared(crate::document_worker::PreparedWorkspace {
            session,
            documents: vec![(recovered, Ok(document))],
        });
        workspace.sync_all();
        assert_eq!(workspace.session.tabs().len(), 2);
        let metadata = workspace.session.tabs().iter().find(|t| t.id == recovered).unwrap();
        assert!(metadata.path.is_none());
        assert_eq!(metadata.recovery_path.as_ref(), Some(&path), "duplicate live name must not erase recovery context");
        let recovered_view = workspace.view(recovered).unwrap();
        assert!(recovered_view.path.is_none());
        assert!(recovered_view.editor().unwrap().document().is_dirty());
        assert!(recovered_view.status_text().contains("Use Save As"));
        let mut bytes = Vec::new();
        recovered_view.editor().unwrap().document().snapshot().write_to(&mut bytes).unwrap();
        assert_eq!(bytes, b"recovered bytes");
        let live = workspace.session.tabs().iter().find(|t| t.path.as_ref() == Some(&path)).unwrap().id;
        assert_ne!(live, recovered);
        bytes.clear();
        workspace.view(live).unwrap().editor().unwrap().document().snapshot().write_to(&mut bytes).unwrap();
        assert_eq!(bytes, b"live edits");
        workspace.activate(recovered);
        assert!(workspace.editor_mut().unwrap().save_request(None).is_err());
        assert!(workspace.close(recovered, false).is_err());
        assert!(workspace.session.validate().is_ok());
    }

    #[test]
    fn recovered_closed_attachment_restores_per_row_windows_and_safe_conflict_path() {
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        let first = workspace.active_id();
        let path = std::env::temp_dir().join("kwil-virtual-recovered-conflict.txt");
        workspace.session.set_path(first, path.clone()).unwrap();
        workspace.views.get_mut(&first).unwrap().path = Some(path.clone());
        workspace.set_horizontal_window(2, 64 << 10);
        workspace.close(first, true).unwrap();
        workspace.closed_views.remove(&first);
        workspace.file_command("file.reopenClosed").unwrap();
        workspace.set_rows(2);
        let mut document = crate::Document::new();
        document.buffer.insert(0, "\u{754c}a\nxyz".as_bytes()).unwrap();
        document.saved_revision = 1;
        workspace.document_result(DocumentResult::Recovered {
            document: first,
            revision: 0,
            result: Ok(Box::new(document)),
        });
        assert!(workspace.view(first).unwrap().path.is_none());
        assert!(workspace.session.tab_mut(first).unwrap().path.is_none());
        assert_eq!(workspace.session.tab_mut(first).unwrap().recovery_path.as_ref(), Some(&path));
        assert_eq!(workspace.horizontal_origin(), 2);
        assert_eq!(workspace.line_offset(0), 0);
        assert_eq!(workspace.line_offset(1), 2);
        assert!(workspace.status_text().contains("Use Save As"));
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
        let Some(DocumentRequest::Save { document, request, .. }) = workspace.files.queue.pop_front() else {
            panic!("expected worker save")
        };
        (document, request)
    }

    fn settings_with_trim(trim: bool) -> kwil_core_commands::Settings {
        let mut settings = kwil_core_commands::Settings::default();
        settings.editor.trim_trailing_whitespace_on_save = trim;
        settings
    }

    fn inactive_save_all_after_settings_reload(trim: bool) {
        let root = std::env::temp_dir().join(format!(
            "kwil-save-policy-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        workspace.set_editor_settings(&settings_with_trim(!trim));
        let first = workspace.active_id();
        edit(&mut workspace, "first \t\n");
        workspace.file_command("file.new").unwrap();
        let second = workspace.active_id();
        edit(&mut workspace, "second \t\n");
        for (id, name) in [(first, "first.txt"), (second, "second.txt")] {
            let path = root.join(name);
            workspace.session.set_path(id, path.clone()).unwrap();
            let view = workspace.views.get_mut(&id).unwrap();
            view.path = Some(path.clone());
            view.editor_mut().unwrap().document.path = Some(path);
        }
        let revision = workspace.view(first).unwrap().editor().unwrap().document().revision();
        workspace.set_editor_settings(&settings_with_trim(trim));
        assert_eq!(workspace.active_id(), second);
        assert_eq!(workspace.view(first).unwrap().editor().unwrap().document().revision(), revision);
        workspace.file_command("file.saveAll").unwrap();
        for (expected, bytes) in [(first, b"first \t\n".as_slice()), (second, b"second \t\n".as_slice())] {
            let (document, request) = take_save(&mut workspace);
            assert_eq!(document, expected);
            assert_eq!(request.trim_trailing_whitespace, trim, "addressed save must use the accepted policy");
            let path = request.path.clone();
            let revision = request.revision;
            let result = kwil_core_workspace::save_snapshot(request);
            let damage = workspace.document_result(DocumentResult::Saved { document, revision, result });
            if document == first {
                assert!(!damage.full, "inactive save must not repaint the active text");
                assert_eq!(workspace.active_id(), second);
                assert_eq!(workspace.editor().unwrap().document().snapshot().len(), b"second \t\n".len() as u64);
            }
            let expected = if trim {
                if document == first { b"first\n".as_slice() } else { b"second\n".as_slice() }
            } else {
                bytes
            };
            assert_eq!(std::fs::read(path).unwrap(), expected);
            assert!(!workspace.view(document).unwrap().editor().unwrap().document().is_dirty());
        }
        assert!(workspace.files.saves.is_empty());
        assert!(workspace.files.save_all.is_empty());
        drop(workspace);
        for name in ["first.txt", "second.txt"] {
            std::fs::remove_file(root.join(name)).unwrap();
        }
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn live_settings_disable_trim_for_inactive_save_all() {
        inactive_save_all_after_settings_reload(false);
    }

    #[test]
    fn live_settings_enable_trim_for_inactive_save_all() {
        inactive_save_all_after_settings_reload(true);
    }

    #[test]
    fn live_settings_reach_new_opened_indexed_and_reopened_editors() {
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        let first = workspace.active_id();
        workspace.set_editor_settings(&settings_with_trim(true));
        workspace.close(first, true).unwrap();
        workspace.set_editor_settings(&settings_with_trim(false));
        workspace.file_command("file.reopenClosed").unwrap();
        assert_eq!(workspace.active_id(), first);
        assert!(!workspace.editor().unwrap().settings.editor.trim_trailing_whitespace_on_save);
        workspace.set_editor_settings(&settings_with_trim(true));
        workspace.file_command("file.new").unwrap();
        assert!(workspace.editor().unwrap().settings.editor.trim_trailing_whitespace_on_save);
        let opened = workspace.open(View::new(None)).unwrap();
        assert!(workspace.view(opened).unwrap().editor().unwrap().settings.editor.trim_trailing_whitespace_on_save);
        let indexed = workspace.open_path(std::env::temp_dir().join("kwil-virtual-settings-index.txt")).unwrap();
        workspace.activate(first);
        let damage = workspace.document_result(DocumentResult::Indexed {
            document: indexed,
            revision: 0,
            result: Ok(Box::new(crate::Document::new())),
        });
        assert!(workspace.view(indexed).unwrap().editor().unwrap().settings.editor.trim_trailing_whitespace_on_save);
        assert!(!damage.full);
        assert_eq!(workspace.active_id(), first);
    }

    #[test]
    fn live_settings_reach_restored_and_late_recovered_editors() {
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        workspace.set_editor_settings(&settings_with_trim(true));
        let mut session = Session::default();
        let recovered = session.open(None).unwrap();
        workspace.install_prepared(crate::document_worker::PreparedWorkspace {
            session,
            documents: vec![(recovered, Ok(crate::Document::new()))],
        });
        assert!(workspace.view(recovered).unwrap().editor().unwrap().settings.editor.trim_trailing_whitespace_on_save);
        workspace.views.get_mut(&recovered).unwrap().editor = None;
        workspace.file_command("file.new").unwrap();
        let active = workspace.active_id();
        workspace.set_editor_settings(&settings_with_trim(false));
        workspace.document_result(DocumentResult::Recovered {
            document: recovered,
            revision: 0,
            result: Ok(Box::new(crate::Document::new())),
        });
        assert!(!workspace.view(recovered).unwrap().editor().unwrap().settings.editor.trim_trailing_whitespace_on_save);
        workspace.views.get_mut(&recovered).unwrap().editor = None;
        workspace.set_editor_settings(&settings_with_trim(true));
        let damage = workspace.document_result(DocumentResult::Recovered {
            document: recovered,
            revision: 0,
            result: Ok(Box::new(crate::Document::new())),
        });
        assert!(workspace.view(recovered).unwrap().editor().unwrap().settings.editor.trim_trailing_whitespace_on_save);
        assert!(!damage.full);
        assert_eq!(workspace.active_id(), active);
    }

    #[test]
    fn live_settings_refresh_each_addressed_save_without_changing_inflight_policy() {
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        edit(&mut workspace, "keep \t\n");
        let first = workspace.active_id();
        workspace.set_editor_settings(&settings_with_trim(true));
        workspace.save_document(first, Some(std::env::temp_dir().join("kwil-virtual-policy-first.txt"))).unwrap();
        let (_, first_request) = take_save(&mut workspace);
        assert!(first_request.trim_trailing_whitespace);
        workspace.file_command("file.new").unwrap();
        let second = workspace.active_id();
        workspace.set_editor_settings(&settings_with_trim(false));
        // An accepted request remains immutable, but later addressed saves use
        // the current policy even if a caller supplied an older editor config.
        assert!(first_request.trim_trailing_whitespace);
        workspace.editor_mut().unwrap().settings = settings_with_trim(true);
        workspace.save_document(second, Some(std::env::temp_dir().join("kwil-virtual-policy-second.txt"))).unwrap();
        let (document, request) = take_save(&mut workspace);
        assert_eq!(document, second);
        assert!(!request.trim_trailing_whitespace);
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

    fn pending_save_blocks_cached_reopen(path: PathBuf, destination: PathBuf) {
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        let saving = workspace.active_id();
        let mut closed = View::new(None);
        closed.path = Some(path.clone());
        closed.editor_mut().unwrap().document.path = Some(path);
        let reopening = workspace.open(closed).unwrap();
        edit(&mut workspace, "cached bytes");
        workspace.close(reopening, true).unwrap();
        edit(&mut workspace, "pending save");
        workspace.save_document(saving, Some(destination)).unwrap();
        let history = workspace.session.closed().front().unwrap().clone();
        let queued = workspace.files.queue.len();
        assert!(workspace.file_command("file.reopenClosed").is_err(), "pending save must block cached reopen");
        assert_eq!(workspace.active_id(), saving);
        assert_eq!(workspace.session.closed().front().unwrap().id, history.id);
        assert_eq!(workspace.session.closed().front().unwrap().path, history.path);
        assert_eq!(workspace.session.closed().front().unwrap().revision, history.revision);
        assert!(!workspace.views.contains_key(&reopening));
        assert_eq!(workspace.closed_views[&reopening].editor().unwrap().document().snapshot().len(), 12);
        assert_eq!(workspace.files.queue.len(), queued);
        assert!(workspace.files.saves.contains_key(&saving));
        // A rejected request leaves the same stable closed entry available to retry.
        workspace.files.saves.clear();
        workspace.files.queue.clear();
        workspace.file_command("file.reopenClosed").unwrap();
        assert_eq!(workspace.active_id(), reopening);
        assert_eq!(workspace.editor().unwrap().document().snapshot().len(), 12);
    }

    #[test]
    fn cached_reopen_preserves_history_and_cache_when_save_destination_is_pending() {
        let path = std::env::temp_dir().join("kwil-virtual-cached-reopen.txt");
        pending_save_blocks_cached_reopen(path.clone(), path);
    }

    #[test]
    #[cfg(windows)]
    fn cached_reopen_honors_pending_windows_path_alias_without_filesystem_access() {
        let path = std::env::temp_dir().join("kwil-virtual-CachedReopen.txt");
        let alias = path.with_file_name("KWIL-VIRTUAL-cachedreopen.TXT");
        pending_save_blocks_cached_reopen(path, alias);
    }

    #[test]
    fn cached_reopen_refreshes_worker_reservations_before_pending_native_alias_save() {
        let root = std::env::temp_dir().join(format!(
            "kwil-reopen-admission-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("protected.txt");
        let alias = root.join("hard-link.txt");
        std::fs::write(&path, b"protected").unwrap();
        std::fs::hard_link(&path, &alias).unwrap();
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        let saving = workspace.active_id();
        let mut closed = View::new(None);
        closed.path = Some(path.clone());
        closed.editor = Some(Editor::new(
            crate::Document::from_file(path.clone(), kwil_core_workspace::open_for_editing(&path).unwrap()).unwrap(),
        ));
        let reopening = workspace.open(closed).unwrap();
        workspace.close(reopening, true).unwrap();
        edit(&mut workspace, "must not overwrite");

        let (sender, receiver) = std::sync::mpsc::channel();
        let (release, blocked) = std::sync::mpsc::sync_channel(1);
        workspace.files.worker = Some(
            DocumentWorker::spawn(move |result| {
                let pause = matches!(&result, DocumentResult::ReopenValidated { .. });
                let _ = sender.send(result);
                if pause {
                    blocked.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
                }
            })
            .unwrap(),
        );
        workspace.file_command("file.reopenClosed").unwrap();
        assert!(
            matches!(receiver.recv_timeout(std::time::Duration::from_secs(5)).unwrap(), DocumentResult::JournalClosed { document, .. } if document == reopening)
        );
        let admitted = receiver.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(
            matches!(&admitted, DocumentResult::ReopenValidated { document, result: Ok(true), .. } if *document == reopening)
        );
        assert!(
            workspace.closed_views.contains_key(&reopening),
            "worker receipt must precede consuming cached history"
        );
        assert!(workspace.file_command("file.reopenClosed").is_err(), "duplicate admission must stay bounded");
        // The worker admitted the reopen but the UI has not attached it yet.
        // Its provisional reservation must survive an ordinary UI refresh.
        workspace.save_document(saving, Some(alias.clone())).unwrap();
        release.send(()).unwrap();
        let result = receiver.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(
            matches!(&result, DocumentResult::Saved { document, result: Err(error), .. } if *document == saving && error.kind() == io::ErrorKind::AlreadyExists),
            "reopened native destination must be reserved before the worker writes"
        );
        workspace.document_result(result);
        workspace.document_result(admitted);
        assert_eq!(workspace.active_id(), reopening);
        assert_eq!(workspace.session.tabs().iter().find(|tab| tab.id == reopening).unwrap().path, Some(path.clone()));
        assert_eq!(std::fs::read(&path).unwrap(), b"protected");
        assert_eq!(std::fs::read(&alias).unwrap(), b"protected");
        assert_eq!(workspace.editor().unwrap().document().snapshot().len(), 9);
        assert!(workspace.view(saving).unwrap().editor().unwrap().document().is_dirty());
        assert!(!workspace.files.saves.contains_key(&saving));
        // After provisional release, the committed UI registry must continue
        // protecting the reopened tab from a later native alias save.
        workspace.save_document(saving, Some(alias.clone())).unwrap();
        let result = receiver.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(
            matches!(&result, DocumentResult::Saved { result: Err(error), .. } if error.kind() == io::ErrorKind::AlreadyExists)
        );
        workspace.document_result(result);
        assert_eq!(std::fs::read(&path).unwrap(), b"protected");
        drop(workspace);
        assert!(matches!(
            receiver.recv_timeout(std::time::Duration::from_secs(5)).unwrap(),
            DocumentResult::Stopped(Ok(()))
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cached_reopen_native_pending_reservation_preserves_history_until_rejected_receipt() {
        let root = std::env::temp_dir().join(format!(
            "kwil-reopen-pending-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("protected.txt");
        let alias = root.join("hard-link.txt");
        std::fs::write(&path, b"protected").unwrap();
        std::fs::hard_link(&path, &alias).unwrap();
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        let saving = workspace.active_id();
        let mut view = View::new(None);
        view.path = Some(path.clone());
        view.editor = Some(Editor::new(
            crate::Document::from_file(path.clone(), kwil_core_workspace::open_for_editing(&path).unwrap()).unwrap(),
        ));
        let reopening = workspace.open(view).unwrap();
        workspace.close(reopening, true).unwrap();
        edit(&mut workspace, "saved elsewhere");
        let (sender, receiver) = std::sync::mpsc::channel();
        let (release, blocked) = std::sync::mpsc::sync_channel(1);
        workspace.files.worker = Some(
            DocumentWorker::spawn(move |result| {
                let pause = matches!(&result, DocumentResult::JournalClosed { .. });
                let _ = sender.send(result);
                if pause {
                    blocked.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
                }
            })
            .unwrap(),
        );
        workspace
            .files
            .worker
            .as_ref()
            .unwrap()
            .try_submit(DocumentRequest::Prepare { root: Some(root.clone()) })
            .unwrap();
        match receiver.recv_timeout(std::time::Duration::from_secs(5)).unwrap() {
            DocumentResult::Prepared(result) => drop(result.unwrap()),
            _ => panic!("expected isolated worker preparation"),
        }
        workspace.file_command("file.reopenClosed").unwrap();
        assert!(matches!(
            receiver.recv_timeout(std::time::Duration::from_secs(5)).unwrap(),
            DocumentResult::JournalClosed { .. }
        ));
        workspace.save_document(saving, Some(alias.clone())).unwrap();
        assert_eq!(workspace.session.closed().front().unwrap().id, reopening);
        assert!(workspace.closed_views.contains_key(&reopening));
        release.send(()).unwrap();
        let rejected = receiver.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(
            matches!(&rejected, DocumentResult::ReopenValidated { result: Err(error), .. } if error.kind() == io::ErrorKind::AlreadyExists)
        );
        workspace.document_result(rejected);
        assert_eq!(workspace.active_id(), saving);
        assert_eq!(workspace.session.closed().front().unwrap().id, reopening);
        assert_eq!(workspace.closed_views[&reopening].editor().unwrap().document().snapshot().len(), 9);
        let saved = receiver.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(matches!(&saved, DocumentResult::Saved { result: Ok(_), .. }));
        workspace.document_result(saved);
        assert_eq!(std::fs::read(&path).unwrap(), b"protected");
        // The atomic alias save replaced only that hardlink name. After its
        // completion the unchanged closed backing is safe to admit and retry.
        workspace.file_command("file.reopenClosed").unwrap();
        let admitted = receiver.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(matches!(&admitted, DocumentResult::ReopenValidated { result: Ok(true), .. }));
        workspace.document_result(admitted);
        assert_eq!(workspace.active_id(), reopening);
        drop(workspace);
        assert!(matches!(
            receiver.recv_timeout(std::time::Duration::from_secs(5)).unwrap(),
            DocumentResult::Stopped(Ok(()))
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cached_reopen_without_original_keeps_bytes_and_requires_save_as() {
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        let reopening = workspace.active_id();
        let path = std::env::temp_dir().join("kwil-virtual-unverified-closed.txt");
        workspace.views.get_mut(&reopening).unwrap().path = Some(path.clone());
        workspace.editor_mut().unwrap().document.path = Some(path.clone());
        edit(&mut workspace, "recovered bytes");
        workspace.close(reopening, true).unwrap();
        workspace.files.reopen = Some((reopening, 1));
        workspace.document_result(DocumentResult::ReopenValidated {
            document: reopening,
            generation: 1,
            result: Ok(false),
        });
        assert_eq!(workspace.active_id(), reopening);
        assert!(workspace.path.is_none());
        let metadata = workspace.session.tabs().iter().find(|tab| tab.id == reopening).unwrap();
        assert!(metadata.path.is_none());
        assert_eq!(metadata.recovery_path.as_ref(), Some(&path));
        assert_eq!(workspace.editor().unwrap().document().snapshot().len(), 15);
        assert!(workspace.editor().unwrap().document().is_dirty());
        assert!(workspace.status_text().contains("Save As"));
        edit(&mut workspace, "!");
        assert!(
            workspace.editor().unwrap().document().is_dirty(),
            "assertion: further edits must not reach the unsaved marker"
        );
        workspace.execute("edit.undo", &Default::default(), 1).unwrap();
        assert!(workspace.editor().unwrap().document().is_dirty());
        workspace.execute("edit.redo", &Default::default(), 1).unwrap();
        assert!(workspace.editor().unwrap().document().is_dirty());
        workspace.file_command("file.saveAll").unwrap();
        assert!(workspace.file_prompt().is_some(), "assertion: Save All must request an unnamed recovery path");
        workspace.cancel_file_prompt();
        assert!(workspace.close(reopening, false).is_err());
    }

    #[test]
    fn stale_reopen_receipt_preserves_cache_and_releases_its_own_generation() {
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        let first = workspace.active_id();
        workspace.close(first, true).unwrap();
        let newest = workspace.active_id();
        workspace.close(newest, true).unwrap();
        workspace.files.queue.clear();
        workspace.files.reopen = Some((first, 7));
        workspace.document_result(DocumentResult::ReopenValidated { document: first, generation: 6, result: Ok(true) });
        assert_eq!(workspace.files.reopen, Some((first, 7)), "stale receipt cannot cancel the live admission");
        workspace.document_result(DocumentResult::ReopenValidated { document: first, generation: 7, result: Ok(true) });
        assert!(workspace.files.reopen.is_none());
        assert_eq!(workspace.session.closed().front().unwrap().id, newest);
        assert!(workspace.closed_views.contains_key(&first));
        assert!(workspace.closed_views.contains_key(&newest));
        assert!(!workspace.views.contains_key(&first));
        assert!(
            matches!(workspace.files.queue.back(), Some(DocumentRequest::ReleaseReopen { document, generation: 7 }) if *document == first)
        );
    }

    #[test]
    #[cfg(windows)]
    fn pending_windows_save_path_case_alias_is_reserved_without_ui_filesystem_access() {
        let path = std::env::temp_dir().join("kwil-virtual-NativeAlias-Note.txt");
        let alias = path.with_file_name("KWIL-VIRTUAL-nativealias-note.TXT");
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        let first = workspace.active_id();
        edit(&mut workspace, "protected");
        workspace.save_document(first, Some(path)).unwrap();
        take_save(&mut workspace);
        workspace.file_command("file.new").unwrap();
        let second = workspace.active_id();
        edit(&mut workspace, "other");
        assert!(workspace.save_document(second, Some(alias)).is_err());
        assert!(!workspace.files.saves.contains_key(&second));
    }

    #[test]
    fn failed_open_placeholder_does_not_block_an_unrelated_worker_save() {
        let root = std::env::temp_dir().join(format!(
            "kwil-failed-open-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        workspace.files.worker = Some(
            DocumentWorker::spawn(move |result| {
                let _ = sender.send(result);
            })
            .unwrap(),
        );
        workspace
            .files
            .worker
            .as_ref()
            .unwrap()
            .try_submit(DocumentRequest::Prepare { root: Some(root.clone()) })
            .unwrap();
        match receiver.recv_timeout(std::time::Duration::from_secs(5)).unwrap() {
            DocumentResult::Prepared(result) => drop(result.unwrap()),
            _ => panic!("expected isolated worker preparation"),
        }
        let failed = workspace.open_path(root.join("missing-parent/failed.txt")).unwrap();
        let result = receiver.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(
            matches!(&result, DocumentResult::CheckedOpened { ticket, result: Err(_), .. } if ticket.document() == failed)
        );
        workspace.document_result(result);
        workspace.file_command("file.new").unwrap();
        let saving = workspace.active_id();
        edit(&mut workspace, "saved");
        let path = root.join("saved.txt");
        workspace.save_document(saving, Some(path.clone())).unwrap();
        let result = receiver.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(matches!(&result, DocumentResult::Saved { document, result: Ok(_), .. } if *document == saving));
        workspace.document_result(result);
        assert_eq!(std::fs::read(&path).unwrap(), b"saved");
        assert!(workspace.view(failed).is_some(), "failed placeholder stays visible to its owner");
        drop(workspace);
        loop {
            if matches!(receiver.recv_timeout(std::time::Duration::from_secs(5)).unwrap(), DocumentResult::Stopped(_)) {
                break;
            }
        }
        std::fs::remove_dir_all(root).unwrap();
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
    fn close_all_invalidates_each_success_so_batch_eviction_waits_at_sixty_four() {
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        for _ in 1..kwil_core_workspace::session::MAX_TABS {
            workspace.open(View::new(None)).unwrap();
        }
        let original: Vec<_> = workspace.session.tabs().iter().map(|tab| tab.id).collect();
        workspace.files.storage_ready = true;
        workspace.files.metadata_durable = true;
        assert_eq!(workspace.file_command("file.closeAll").unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert_eq!(workspace.session.closed().len(), kwil_core_workspace::session::MAX_CLOSED);
        assert_eq!(
            workspace.session.tabs().len(),
            kwil_core_workspace::session::MAX_TABS - kwil_core_workspace::session::MAX_CLOSED
        );
        assert_eq!(workspace.files.close.front(), Some(&original[64]));
        workspace.document_result(DocumentResult::Persisted(Err(io::Error::other("fixture checkpoint failure"))));
        assert_eq!(workspace.files.close.front(), Some(&original[64]));
        assert!(original[..64].iter().all(|id| workspace.session.closed().iter().any(|tab| tab.id == *id)));
        assert!(original[64..].iter().all(|id| workspace.view(*id).is_some()));
        let active_before_retry = workspace.active_id();
        workspace.files.metadata_durable = true;
        workspace.file_command("file.close").unwrap();
        assert!(workspace.view(original[64]).is_none(), "retry closes the captured unapplied target");
        assert!(workspace.view(active_before_retry).is_some(), "retry cannot close the unrelated active tab");
        assert!(workspace.files.close.is_empty());
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

// ROOT-only proposal: insert this type in workspace.rs, re-export from lib.rs.
// Private fields prevent GUI code from manufacturing another prompt's ticket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeFilePrompt {
    generation: u64,
    kind: FilePromptKind,
    document: DocumentId,
    revision: u64,
    epoch: u64,
    active: DocumentId,
    close: Vec<DocumentId>,
    save_all: Vec<DocumentId>,
    quit: bool,
}
impl NativeFilePrompt {
    pub fn kind(&self) -> FilePromptKind {
        self.kind
    }
    pub fn document(&self) -> DocumentId {
        self.document
    }
}

impl Workspace {
    /// GUI teardown must consume accepted save receipts, including a completion
    /// deferred until existing recovery/journal admission becomes available.
    pub fn file_saves_pending(&self) -> bool {
        !self.files.saves.is_empty()
            || self.files.queue.iter().any(|request| matches!(request, DocumentRequest::DiscardHex { .. }))
            || self.search_replacement_pending()
            || !self.files.replacement_journals.is_empty()
            || self
                .views
                .values()
                .chain(self.closed_views.values())
                .any(|view| view.editor().is_some_and(|editor| editor.has_pending_edit_receipt()))
            || self.views.values().any(|view| view.editor().is_some_and(|editor| editor.has_pending_save_completion()))
    }

    /// Capture only native-chooser prompt kinds. TUI's String path stays intact.
    pub fn native_file_prompt(&self) -> Option<NativeFilePrompt> {
        let prompt = self.files.prompt.as_ref()?;
        if prompt.kind == FilePromptKind::Recent {
            return None;
        }
        Some(NativeFilePrompt {
            generation: prompt.generation,
            kind: prompt.kind,
            document: prompt.document,
            revision: prompt.revision,
            epoch: self.input_epoch,
            active: self.active_id(),
            close: self.files.close.iter().copied().collect(),
            save_all: self.files.save_all.iter().copied().collect(),
            quit: self.files.quit,
        })
    }

    pub fn native_file_prompt_is_current(&self, ticket: &NativeFilePrompt) -> bool {
        self.native_file_prompt().as_ref() == Some(ticket)
            && self
                .views
                .get(&ticket.document)
                .is_some_and(|view| view.editor().map_or(0, |editor| editor.document().revision()) == ticket.revision)
    }

    /// Cancel only the prompt this native operation owns, even if its revision
    /// or tab context has become stale. Never cancel a newer prompt's intent.
    pub fn cancel_native_file_prompt(&mut self, ticket: &NativeFilePrompt) {
        if self.files.prompt.as_ref().is_some_and(|prompt| {
            prompt.generation == ticket.generation && prompt.document == ticket.document && prompt.kind == ticket.kind
        }) {
            self.cancel_file_prompt();
        }
    }

    /// Admit an explicitly accepted native path directly into the existing
    /// prepare/save/journal engine. No lossy String round trip or cwd fallback.
    pub fn submit_native_file_prompt(&mut self, ticket: &NativeFilePrompt, path: PathBuf) -> io::Result<()> {
        if !self.native_file_prompt_is_current(ticket) {
            self.cancel_native_file_prompt(ticket);
            return Err(io::Error::other("Document or file action changed while choosing a path; retry"));
        }
        if !path.is_absolute() || path.file_name().is_none() {
            self.cancel_native_file_prompt(ticket);
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "The native chooser must accept an absolute file path",
            ));
        }
        self.files.prompt = None;
        let hex = std::mem::take(&mut self.hex.prompt);
        let result = match ticket.kind {
            FilePromptKind::Open if hex => self.open_hex_path(path, self.rows()).map(|(_, outcome)| {
                if outcome == crate::hex_session::OpenOutcome::NeedsRawReopen {
                    self.set_notice("Save decoded changes before reopening physical bytes".into());
                }
            }),
            FilePromptKind::Open => self.open_path(path).map(|_| ()),
            FilePromptKind::SaveAs => self.save_document(ticket.document, Some(path)),
            FilePromptKind::Recent => {
                unreachable!("native_file_prompt excludes recent-number prompts")
            }
        };
        if result.is_err() {
            // A failed Save As cannot leave a close or Save All continuation.
            self.files.save_all.clear();
            self.cancel_close_intent();
        }
        result
    }
}

#[cfg(test)]
mod native_path_tests {
    use super::*;
    fn pending() -> Workspace {
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        workspace
            .execute("edit.insert", &crate::CommandArgs { text: Some("dirty 日本語".into()), ..Default::default() }, 1)
            .unwrap();
        workspace.file_command("file.save").unwrap();
        workspace
    }
    fn save_count(workspace: &Workspace) -> usize {
        workspace.files.queue.iter().filter(|request| matches!(request, DocumentRequest::Save { .. })).count()
    }
    #[test]
    fn native_no_write_until_accept_and_relative_path_never_uses_cwd() {
        let mut workspace = pending();
        let ticket = workspace.native_file_prompt().unwrap();
        assert_eq!(save_count(&workspace), 0);
        workspace.submit_native_file_prompt(&ticket, PathBuf::from("relative.txt")).unwrap_err();
        assert_eq!(save_count(&workspace), 0);
        assert!(workspace.editor().unwrap().document().path().is_none());
        assert!(workspace.editor().unwrap().document().is_dirty());
        assert!(!workspace.close_pending());
    }
    #[test]
    fn native_accept_queues_existing_save_for_exact_path_and_revision() {
        let mut workspace = pending();
        let ticket = workspace.native_file_prompt().unwrap();
        let path = std::env::temp_dir().join("KW56-日本語-accepted.txt");
        workspace.submit_native_file_prompt(&ticket, path.clone()).unwrap();
        assert_eq!(save_count(&workspace), 1);
        let request = workspace
            .files
            .queue
            .iter()
            .find_map(|request| match request {
                DocumentRequest::Save { document, request, .. } => Some((*document, request)),
                _ => None,
            })
            .unwrap();
        assert_eq!(request.0, ticket.document);
        assert_eq!(request.1.revision, ticket.revision);
        assert_eq!(request.1.path, path);
        // No worker is attached: this is admission evidence, never disk-write proof.
    }
    #[test]
    fn native_cancel_of_close_save_clears_continuations() {
        let mut workspace = pending();
        workspace.file_command("file.saveAndQuit").unwrap();
        let ticket = workspace.native_file_prompt().unwrap();
        assert!(workspace.close_pending());
        workspace.cancel_native_file_prompt(&ticket);
        assert!(!workspace.close_pending());
        assert!(!workspace.wants_exit());
        assert_eq!(save_count(&workspace), 0);
        assert!(workspace.editor().unwrap().document().is_dirty());
    }
    #[test]
    fn native_edit_while_chooser_is_open_does_not_admit_newer_bytes() {
        let mut workspace = pending();
        let ticket = workspace.native_file_prompt().unwrap();
        workspace
            .execute("edit.insert", &crate::CommandArgs { text: Some(" newer".into()), ..Default::default() }, 2)
            .unwrap();
        workspace.submit_native_file_prompt(&ticket, std::env::temp_dir().join("kw56-newer.txt")).unwrap_err();
        assert_eq!(save_count(&workspace), 0);
        assert!(workspace.editor().unwrap().document().is_dirty());
    }
    #[test]
    fn native_switch_away_and_back_invalidates_original_context() {
        let mut workspace = pending();
        let first = workspace.active_id();
        workspace.file_command("file.new").unwrap();
        let second = workspace.active_id();
        workspace.activate(first);
        workspace.file_command("file.save").unwrap();
        let ticket = workspace.native_file_prompt().unwrap();
        workspace.activate(second);
        workspace.activate(first);
        assert!(!workspace.native_file_prompt_is_current(&ticket));
        workspace.submit_native_file_prompt(&ticket, std::env::temp_dir().join("kw56-stale.txt")).unwrap_err();
        assert_eq!(save_count(&workspace), 0);
    }
    #[test]
    fn native_close_reopen_same_document_id_and_revision_rejects_old_receipt() {
        let mut workspace = pending();
        let ticket = workspace.native_file_prompt().unwrap();
        let id = workspace.active_id();
        workspace.close(id, true).unwrap();
        workspace.file_command("file.reopenClosed").unwrap();
        assert!(!workspace.native_file_prompt_is_current(&ticket));
        workspace.submit_native_file_prompt(&ticket, std::env::temp_dir().join("kw56-reopen.txt")).unwrap_err();
        assert_eq!(save_count(&workspace), 0);
    }
    #[cfg(windows)]
    #[test]
    fn native_unpaired_utf16_path_reaches_save_request_unchanged() {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        let mut workspace = pending();
        let ticket = workspace.native_file_prompt().unwrap();
        let mut units: Vec<u16> = std::env::temp_dir().as_os_str().encode_wide().collect();
        if units.last() != Some(&92) {
            units.push(92);
        }
        units.extend([0x65e5, 0xd800, 46, 116, 120, 116]);
        let path = PathBuf::from(std::ffi::OsString::from_wide(&units));
        workspace.submit_native_file_prompt(&ticket, path).unwrap();
        let request = workspace
            .files
            .queue
            .iter()
            .find_map(|request| match request {
                DocumentRequest::Save { request, .. } => Some(request),
                _ => None,
            })
            .unwrap();
        assert_eq!(request.path.as_os_str().encode_wide().collect::<Vec<_>>(), units);
    }
}
