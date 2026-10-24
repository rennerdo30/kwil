//! Shared settings screen lifecycle. Construct **after the first frame**, with
//! an absolute, already resolved settings path. Only `SettingsWorker` reads,
//! watches, parses disk contents or saves files. Typed document transactions
//! retain the backend's validation and comment-preservation rules.
//!
//! ```no_run
//! use std::{path::PathBuf, sync::Arc};
//! use kwil_view_model::settings_session::{SettingsInput, SettingsSession};
//! # fn example(path: PathBuf) -> std::io::Result<()> {
//! // wake must enqueue a frontend event, never run UI code on the worker.
//! let mut screen = SettingsSession::start(path, Arc::new(|| {}))?;
//! screen.open();
//! // On the addressed wake event (also while the screen is closed):
//! screen.worker_ready();
//! if let Some(applied) = screen.take_applied() {
//!     let current_runtime_settings = applied.settings;
//! }
//! screen.input(SettingsInput::Search).unwrap();
//! screen.input(SettingsInput::Text("indent width")).unwrap();
//! screen.input(SettingsInput::Accept).unwrap(); // leave search
//! // Rows include schema descriptions, defaults and current typed values.
//! let rows = screen.rows();
//! // Compound panels use the very same typed document APIs as the backend:
//! screen.edit_document(|doc| doc.map_extension("rs", Some("rust"))).unwrap();
//! // Accepted edits/reset enqueue saves. Only a Saved event applies them.
//! let shutdown = screen.stop(); // nonblocking, safe on the UI thread
//! // After closing the interactive loop, wait before allowing process exit:
//! shutdown.shutdown().map_err(std::io::Error::other)?;
//! # Ok(()) }
//! ```
use crate::{
    language_selection::AcceptedLanguageMappings,
    settings_panels::{PanelInput, PanelWindow, SettingsPanelController},
};
use kwil_core_commands::{
    Settings,
    settings::{SettingsError, SettingsWorker, SettingsWorkerEvent},
    settings_ui::{Category, SettingKind, SettingValue, SettingsDocument, SettingsMenu, SettingsRow},
};
use std::{
    collections::{BTreeMap, VecDeque},
    io,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, mpsc},
    thread::{self, JoinHandle},
};
use unicode_segmentation::UnicodeSegmentation;

pub const SETTINGS_INPUT_LIMIT: usize = 64 * 1024;
const EVENT_LIMIT: usize = 8;

#[derive(Clone, Copy, Debug)]
pub enum SettingsInput<'a> {
    Text(&'a str),
    Backspace,
    Delete,
    Left,
    Right,
    Home,
    End,
    Next,
    Previous,
    Accept,
    Cancel,
    Search,
    Reset,
    Save,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsPanel {
    Keymap,
    FileTypes,
    Formatters,
    StringMap { id: &'static str },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsAppliedReason {
    Loaded,
    Saved { request: u64 },
}

/// Latest accepted runtime state, owned so frontends need not borrow the menu.
/// `receipt` increases for each adoption. Local edits never produce a receipt.
#[derive(Clone, Debug)]
pub struct SettingsApplied {
    pub settings: Settings,
    pub file_types: BTreeMap<String, String>,
    /// Validated on the settings worker for this exact accepted candidate.
    /// Frontends move/clone this into HighlightSession::apply_mappings; never
    /// parse language ids or compile a Detector while consuming this receipt.
    pub language_mappings: AcceptedLanguageMappings,
    pub source: Option<String>,
    pub receipt: u64,
    pub reason: SettingsAppliedReason,
}

/// Only this prepared form enters the frontend mailbox. Construction runs in
/// the existing SettingsWorker callback, before taking any queue lock. Loaded
/// already owns a parsed document; Saved's exact acknowledged source is parsed
/// here on that same worker, independently of any newer local draft.
#[derive(Debug)]
enum PreparedSettingsEvent {
    Loaded { source: Option<String>, document: Box<SettingsDocument>, language_mappings: AcceptedLanguageMappings },
    Saved { request: u64, source: String, language_mappings: Result<AcceptedLanguageMappings, String> },
    Error { request: Option<u64>, message: String },
}
impl PreparedSettingsEvent {
    fn prepare(event: SettingsWorkerEvent) -> Self {
        match event {
            SettingsWorkerEvent::Loaded { source, document } => {
                match AcceptedLanguageMappings::prepare(document.file_types()) {
                    Ok(language_mappings) => Self::Loaded { source, document, language_mappings },
                    Err(error) => Self::Error { request: None, message: error.to_string() },
                }
            }
            SettingsWorkerEvent::Saved { request, source } => {
                let prepared = SettingsDocument::parse(&source)
                    .and_then(|document| AcceptedLanguageMappings::prepare(document.file_types()));
                // Retain the source even on semantic failure so the UI still
                // checks the exact pending receipt before settling a save.
                // The backend has already acknowledged the write; a failed
                // candidate never becomes accepted runtime state.
                Self::Saved {
                    request,
                    source,
                    language_mappings: prepared.map_err(|error| format!("saved settings were not applied: {error}")),
                }
            }
            SettingsWorkerEvent::Error { request, message } => Self::Error { request, message },
        }
    }
}

#[derive(Default)]
struct Mailbox {
    events: VecDeque<PreparedSettingsEvent>,
    closed: bool,
    producer_finished: bool,
    #[cfg(test)]
    waiting: Option<mpsc::SyncSender<()>>,
}
#[derive(Default)]
struct EventQueue {
    mailbox: Mutex<Mailbox>,
    space: Condvar,
    ready: Condvar,
}
impl EventQueue {
    // Backpressure belongs exclusively to the producer. Every event survives a
    // burst; in particular Saved cannot evict Loaded, or vice versa. Each event
    // is bounded by the backend's 1 MiB document cap. No lock spans a wake.
    fn push_prepared(&self, event: PreparedSettingsEvent, wake: &dyn Fn()) {
        let mut mailbox = self.mailbox.lock().unwrap_or_else(|e| e.into_inner());
        while mailbox.events.len() == EVENT_LIMIT && !mailbox.closed {
            mailbox = self.space.wait(mailbox).unwrap_or_else(|e| e.into_inner());
        }
        if mailbox.closed {
            return;
        }
        let needs_wake = mailbox.events.is_empty();
        mailbox.events.push_back(event);
        self.ready.notify_all();
        drop(mailbox);
        if needs_wake {
            wake();
        }
    }
    // Direct event injection prepares on the test producer/setup side too.
    #[cfg(test)]
    fn push(&self, event: SettingsWorkerEvent, wake: &dyn Fn()) {
        self.push_prepared(PreparedSettingsEvent::prepare(event), wake);
    }
    fn drain(&self) -> VecDeque<PreparedSettingsEvent> {
        let mut mailbox = self.mailbox.lock().unwrap_or_else(|e| e.into_inner());
        let events = std::mem::take(&mut mailbox.events);
        self.space.notify_all();
        events
    }
    fn close(&self) {
        let mut mailbox = self.mailbox.lock().unwrap_or_else(|e| e.into_inner());
        mailbox.closed = true;
        mailbox.events.clear();
        self.space.notify_all();
        self.ready.notify_all();
    }
    // Only explicit shutdown waits here, after leaving the interactive loop.
    // Queue notification is independent of a frontend wake that can no longer
    // be delivered once its event loop has closed.
    fn wait(&self) -> Result<(), String> {
        let mut mailbox = self.mailbox.lock().unwrap_or_else(|e| e.into_inner());
        while mailbox.events.is_empty() && !mailbox.closed && !mailbox.producer_finished {
            #[cfg(test)]
            if let Some(waiting) = &mailbox.waiting {
                let _ = waiting.try_send(());
            }
            mailbox = self.ready.wait(mailbox).unwrap_or_else(|e| e.into_inner());
        }
        if !mailbox.events.is_empty() {
            Ok(())
        } else {
            Err("settings worker stopped before acknowledging accepted settings".into())
        }
    }
}

// The callback owns this guard. Worker exit (including panic) wakes a drain
// waiting for an acknowledgement that can no longer arrive.
struct EventProducer(Arc<EventQueue>);
impl EventProducer {
    fn push(&self, event: SettingsWorkerEvent, wake: &dyn Fn()) {
        self.0.push_prepared(PreparedSettingsEvent::prepare(event), wake);
    }
}
impl Drop for EventProducer {
    fn drop(&mut self) {
        let mut mailbox = self.0.mailbox.lock().unwrap_or_else(|e| e.into_inner());
        mailbox.producer_finished = true;
        self.0.ready.notify_all();
    }
}

#[derive(Default)]
struct TextInput {
    text: String,
    cursor: usize,
}
impl TextInput {
    fn new(text: String) -> Result<Self, String> {
        if text.len() > SETTINGS_INPUT_LIMIT {
            return Err("setting input exceeds 64 KiB; use Open settings file".into());
        }
        Ok(Self { cursor: text.len(), text })
    }
    fn left(&self) -> usize {
        self.text[..self.cursor].grapheme_indices(true).next_back().map_or(0, |(i, _)| i)
    }
    fn right(&self) -> usize {
        self.text[self.cursor..].graphemes(true).next().map_or(self.cursor, |g| self.cursor + g.len())
    }
    fn insert(&mut self, text: &str) -> Result<(), String> {
        if text.len() > SETTINGS_INPUT_LIMIT.saturating_sub(self.text.len()) {
            return Err("setting input exceeds 64 KiB".into());
        }
        self.text.insert_str(self.cursor, text);
        let end = self.cursor + text.len();
        // Insertion may merge with adjacent combining/ZWJ characters. Keep the
        // caret on a boundary of the resulting full string, not of the paste.
        self.cursor = self.text.grapheme_indices(true).map(|(i, _)| i).find(|i| *i >= end).unwrap_or(self.text.len());
        Ok(())
    }
    fn edit(&mut self, input: SettingsInput<'_>) -> Result<(), String> {
        match input {
            SettingsInput::Text(text) => self.insert(text)?,
            SettingsInput::Left => self.cursor = self.left(),
            SettingsInput::Right => self.cursor = self.right(),
            SettingsInput::Home => self.cursor = 0,
            SettingsInput::End => self.cursor = self.text.len(),
            SettingsInput::Backspace => {
                let start = self.left();
                self.text.replace_range(start..self.cursor, "");
                self.cursor = start;
                self.align_after_delete();
            }
            SettingsInput::Delete => {
                self.text.replace_range(self.cursor..self.right(), "");
                self.align_after_delete();
            }
            _ => {}
        }
        Ok(())
    }
    fn align_after_delete(&mut self) {
        // Deleting a separator can join two regional indicators/ZWJ clusters.
        self.cursor =
            self.text.grapheme_indices(true).map(|(i, _)| i).find(|i| *i >= self.cursor).unwrap_or(self.text.len());
    }
}

struct Editing {
    id: &'static str,
    kind: SettingKind,
    input: TextInput,
}
struct PendingSave {
    request: u64,
    document: SettingsDocument,
}
struct External {
    source: Option<String>,
    document: SettingsDocument,
    language_mappings: AcceptedLanguageMappings,
}

#[cfg(test)]
type TestRequests = Arc<Mutex<Vec<(u64, Option<String>, String)>>>;

enum Client {
    Worker(Arc<SettingsWorker>),
    #[cfg(test)]
    Fake(TestRequests),
    #[cfg(test)]
    FakeWorker(mpsc::SyncSender<(u64, Option<String>, String)>),
}
impl Client {
    fn save(&self, request: u64, expected: Option<String>, source: String) -> Result<(), SettingsError> {
        match self {
            Self::Worker(worker) => worker.save(request, expected, source),
            #[cfg(test)]
            Self::Fake(requests) => {
                requests.lock().unwrap().push((request, expected, source));
                Ok(())
            }
            #[cfg(test)]
            Self::FakeWorker(requests) => requests
                .try_send((request, expected, source))
                .map_err(|e| SettingsError(format!("fake settings queue unavailable: {e}"))),
        }
    }
    fn reload(&self) -> Result<(), SettingsError> {
        match self {
            Self::Worker(worker) => worker.reload(),
            #[cfg(test)]
            Self::Fake(_) => Ok(()),
            #[cfg(test)]
            Self::FakeWorker(_) => Ok(()),
        }
    }
}

/// Owns the stopped session until its accepted saves have been acknowledged.
/// Call `shutdown` after closing the interactive loop and wait for its result
/// before process exit. Dropping the handle only requests immediate retirement;
/// it cannot guarantee accepted saves are drained.
pub struct SettingsShutdown {
    session: Option<SettingsSession>,
}
impl SettingsShutdown {
    pub fn shutdown(mut self) -> Result<(), String> {
        let Some(mut session) = self.session.take() else { return Ok(()) };
        let result = session.drain_accepted();
        let retired = session.retire_now().map_or(Ok(()), |retirement| {
            retirement.join().map_err(|_| "settings retirement worker panicked".to_owned())
        });
        result.and(retired)
    }
}

pub struct SettingsSession {
    path: PathBuf,
    events: Arc<EventQueue>,
    client: Option<Client>,
    retire: Option<mpsc::SyncSender<()>>,
    retirement: Option<JoinHandle<()>>,
    menu: Option<SettingsMenu>,
    // The accepted runtime document is distinct from the last acknowledged
    // disk contents. A legacy/rejected Saved still advances the optimistic
    // write baseline, without accepting its settings or mappings.
    baseline: Option<External>,
    // Outer None = not loaded; inner None = acknowledged missing file.
    disk_source: Option<Option<String>>,
    external: Option<External>,
    pending: Option<PendingSave>,
    save_again: bool,
    next_request: u64,
    applied: Option<SettingsApplied>,
    receipt: u64,
    open: bool,
    query: TextInput,
    category: Option<Category>,
    searching: bool,
    editing: Option<Editing>,
    panel: Option<SettingsPanelController>,
    error: Option<String>,
    save_error: Option<String>,
    status: String,
}

impl SettingsSession {
    pub fn start(path: PathBuf, wake: Arc<dyn Fn() + Send + Sync>) -> io::Result<Self> {
        if !path.is_absolute() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "settings path must be absolute"));
        }
        let events = Arc::new(EventQueue::default());
        let worker_events = EventProducer(events.clone());
        // Create the retirement thread first. If worker creation fails, channel
        // disconnection ends it; there is no worker to drop/join on the caller.
        let (own_tx, own_rx) = mpsc::sync_channel::<Arc<SettingsWorker>>(1);
        let (retire, retire_rx) = mpsc::sync_channel(1);
        let retirement = thread::Builder::new().name("kwil-settings-retire".into()).spawn(move || {
            if let Ok(worker) = own_rx.recv() {
                let _ = retire_rx.recv();
                // The UI drops its client before sending retirement. The last
                // Arc (and therefore the backend's blocking Drop) lives here.
                drop(worker);
            }
        })?;
        let worker = Arc::new(SettingsWorker::start_with_validator(
            path.clone(),
            |document| AcceptedLanguageMappings::prepare(document.file_types()).map(|_| ()),
            move |event| {
                worker_events.push(event, wake.as_ref());
            },
        )?);
        if let Err(error) = own_tx.send(worker.clone()) {
            // The receiver has no fallible operation before recv. Even if it
            // were to fail unexpectedly, never run backend Drop on the UI.
            std::mem::forget(error.0);
            events.close();
            return Err(io::Error::other("settings retirement thread unavailable"));
        }
        let mut session = Self::state(path, events);
        session.client = Some(Client::Worker(worker));
        session.retire = Some(retire);
        session.retirement = Some(retirement);
        Ok(session)
    }

    fn state(path: PathBuf, events: Arc<EventQueue>) -> Self {
        Self {
            path,
            events,
            client: None,
            retire: None,
            retirement: None,
            menu: None,
            baseline: None,
            disk_source: None,
            external: None,
            pending: None,
            save_again: false,
            next_request: 1,
            applied: None,
            receipt: 0,
            open: false,
            query: TextInput::default(),
            category: None,
            searching: false,
            editing: None,
            panel: None,
            error: None,
            save_error: None,
            status: "Loading settings…".into(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn menu(&self) -> Option<&SettingsMenu> {
        self.menu.as_ref()
    }
    pub fn document(&self) -> Option<&SettingsDocument> {
        self.menu.as_ref().map(SettingsMenu::document)
    }
    pub fn open(&mut self) {
        self.open = true;
    }
    /// Closing hides the screen; pending saves, local drafts and errors survive.
    pub fn close(&mut self) {
        self.open = false;
    }
    pub fn is_open(&self) -> bool {
        self.open
    }
    pub fn rows(&self) -> Vec<SettingsRow> {
        self.menu.as_ref().map_or_else(Vec::new, |m| m.rows().collect())
    }
    pub fn selected(&self) -> usize {
        self.menu
            .as_ref()
            .and_then(|m| {
                let selected = m.selected()?;
                m.rows().position(|row| row.spec.id == selected.spec.id)
            })
            .unwrap_or(0)
    }
    pub fn selected_row(&self) -> Option<SettingsRow> {
        self.menu.as_ref()?.selected()
    }
    pub fn select(&mut self, index: usize) {
        if self.editing.is_some() || self.panel_has_draft() {
            return;
        }
        let current = self.selected();
        if let Some(menu) = &mut self.menu {
            menu.move_selection(index.min(isize::MAX as usize) as isize - current as isize);
        }
        self.panel = None;
    }
    pub fn category(&self) -> Option<Category> {
        self.category
    }
    pub fn cycle_category(&mut self, backwards: bool) {
        use kwil_core_commands::settings_ui::CATEGORIES;
        let count = CATEGORIES.len() + 1;
        let current = self
            .category
            .and_then(|category| CATEGORIES.iter().position(|c| *c == category))
            .map_or(0, |index| index + 1);
        let next = if backwards { (current + count - 1) % count } else { (current + 1) % count };
        self.set_category(next.checked_sub(1).map(|index| CATEGORIES[index]));
    }
    pub fn input_text(&self) -> &str {
        &self.active_input().text
    }
    /// Byte offset at a full-string extended-grapheme boundary.
    pub fn input_cursor(&self) -> usize {
        self.active_input().cursor
    }
    pub fn search_text(&self) -> &str {
        &self.query.text
    }
    pub fn is_searching(&self) -> bool {
        self.searching
    }
    pub fn is_editing(&self) -> bool {
        self.editing.is_some()
    }
    pub fn panel(&self) -> Option<SettingsPanel> {
        self.panel.as_ref().map(SettingsPanelController::panel)
    }
    /// Memory-only, bounded viewport of the active compound settings page.
    pub fn panel_window(&self, offset: usize, max: usize) -> Result<Option<PanelWindow>, String> {
        self.panel
            .as_ref()
            .map(|panel| {
                panel.window(self.document().ok_or("settings have not loaded")?, offset, max).map_err(|e| e.to_string())
            })
            .transpose()
    }
    pub fn panel_is_capturing(&self) -> bool {
        self.panel.as_ref().is_some_and(SettingsPanelController::is_capturing)
    }
    pub fn panel_is_confirming(&self) -> bool {
        self.panel.as_ref().is_some_and(SettingsPanelController::is_confirming)
    }
    /// Includes capture and conflict confirmation as well as mapping text edits.
    pub fn panel_is_editing(&self) -> bool {
        self.panel.as_ref().is_some_and(SettingsPanelController::is_editing)
    }
    pub fn panel_query_cursor(&self) -> usize {
        self.panel.as_ref().map_or(0, SettingsPanelController::query_cursor)
    }
    fn panel_has_draft(&self) -> bool {
        self.panel.as_ref().is_some_and(|panel| panel.is_editing() || !panel.query().is_empty())
    }
    /// Select within the filtered panel; never retarget a capture or row draft.
    /// Controller row limits bound the number of navigation operations.
    pub fn panel_select(&mut self, index: usize) -> Result<(), String> {
        let result = (|| {
            if !self.open {
                return Ok(());
            }
            let doc = self.menu.as_ref().ok_or("settings have not loaded")?.document();
            let panel = self.panel.as_mut().ok_or("no active settings panel")?;
            if panel.is_editing() {
                return Err("finish or cancel the panel edit before selecting another row".into());
            }
            panel.select(doc, index).map_err(|e| e.to_string())?;
            Ok(())
        })();
        self.finish_action(result)
    }
    /// Escape cancels one stage, then clears the query, then returns to the
    /// generic menu. It never closes the parent settings screen. Hidden input
    /// is ignored, and close/open retains controller drafts and selection.
    pub fn panel_input(&mut self, input: PanelInput<'_>) -> Result<(), String> {
        if !self.open {
            return Ok(());
        }
        let result = (|| {
            let doc = self.menu.as_ref().ok_or("settings have not loaded")?.document();
            let panel = self.panel.as_mut().ok_or("no active settings panel")?;
            let cancel = matches!(input, PanelInput::Cancel)
                || matches!(input, PanelInput::Capture(chord)
                    if chord.key == kwil_core_commands::Key::Named(kwil_core_commands::NamedKey::Escape)
                        && chord.modifiers == Default::default());
            if cancel && !panel.is_editing() {
                if !panel.query().is_empty() {
                    // The pure controller's Cancel only resets its stage; the
                    // session owns query dismissal and whole-panel navigation.
                    panel.search(doc, "").map_err(|e| e.to_string())?;
                } else {
                    self.panel = None;
                }
                return Ok(());
            }
            if let Some(edit) = panel.input(doc, input).map_err(|e| e.to_string())? {
                self.edit_document(|doc| edit.apply(doc))?;
            }
            Ok(())
        })();
        self.finish_action(result)
    }
    pub fn status(&self) -> &str {
        &self.status
    }
    pub fn source_exists(&self) -> bool {
        self.disk_source.as_ref().is_some_and(Option::is_some)
    }
    pub fn last_error(&self) -> Option<&str> {
        self.error.as_deref()
    }
    pub fn has_conflict(&self) -> bool {
        self.external.is_some()
    }
    pub fn is_saving(&self) -> bool {
        self.pending.is_some()
    }
    pub fn is_dirty(&self) -> bool {
        match (&self.menu, &self.baseline) {
            (Some(menu), Some(base)) => {
                menu.document().source() != base.document.source()
                    || self.disk_source.as_ref().is_some_and(|disk| disk != &base.source)
            }
            _ => false,
        }
    }
    pub fn take_applied(&mut self) -> Option<SettingsApplied> {
        self.applied.take()
    }
    pub fn clear_error(&mut self) {
        self.error = None;
        self.update_status();
    }
    pub fn set_category(&mut self, category: Option<Category>) {
        if self.editing.is_some() || self.panel_has_draft() {
            return;
        }
        self.category = category;
        if let Some(menu) = &mut self.menu {
            menu.select_category(category);
        }
        self.editing = None;
        self.panel = None;
    }
    fn active_input(&self) -> &TextInput {
        self.editing.as_ref().map_or(&self.query, |edit| &edit.input)
    }

    /// Atomic typed panel transaction. Exposes keymap_rows/conflicts/rebind,
    /// map_extension, edit_map_entry and edit_formatter without raw TOML input.
    /// A failed closure leaves the entire document unchanged. Success autosaves.
    pub fn edit_document(
        &mut self,
        edit: impl FnOnce(&mut SettingsDocument) -> Result<(), SettingsError>,
    ) -> Result<(), String> {
        let result = (|| {
            let menu = self.menu.as_mut().ok_or("settings have not loaded")?;
            let mut candidate = menu.document().clone();
            edit(&mut candidate).map_err(|e| e.to_string())?;
            menu.replace_document(candidate);
            self.save()
        })();
        self.finish_action(result)
    }

    pub fn input(&mut self, input: SettingsInput<'_>) -> Result<(), String> {
        if !self.open {
            return Ok(());
        }
        let result = self.handle_input(input);
        self.finish_action(result)
    }
    fn finish_action(&mut self, result: Result<(), String>) -> Result<(), String> {
        if let Err(error) = &result {
            self.error = Some(error.clone());
        }
        self.update_status();
        result
    }
    fn handle_input(&mut self, input: SettingsInput<'_>) -> Result<(), String> {
        if self.panel.is_some() && matches!(input, SettingsInput::Cancel) {
            return self.panel_input(PanelInput::Cancel);
        }
        if self.panel.is_some() {
            let panel_input = match input {
                SettingsInput::Text(s) => Some(PanelInput::Text(s)),
                SettingsInput::Backspace => Some(PanelInput::Backspace),
                SettingsInput::Delete => Some(PanelInput::Delete),
                SettingsInput::Left => Some(PanelInput::Left),
                SettingsInput::Right => Some(PanelInput::Right),
                SettingsInput::Home => Some(PanelInput::Home),
                SettingsInput::End => Some(PanelInput::End),
                SettingsInput::Next => Some(PanelInput::Next),
                SettingsInput::Previous => Some(PanelInput::Previous),
                SettingsInput::Accept => Some(PanelInput::Accept),
                // Generic search/reset must not discard specialized state.
                SettingsInput::Search | SettingsInput::Reset if self.panel_has_draft() => return Ok(()),
                SettingsInput::Search | SettingsInput::Reset => None,
                SettingsInput::Save => return self.save(),
                SettingsInput::Cancel => unreachable!(),
            };
            if let Some(input) = panel_input {
                return self.panel_input(input);
            }
        }
        match input {
            SettingsInput::Cancel => {
                if self.editing.take().is_some() {
                    return Ok(());
                }
                if self.searching {
                    self.query = TextInput::default();
                    if let Some(menu) = &mut self.menu {
                        menu.search("");
                    }
                    self.searching = false;
                } else {
                    self.close();
                }
            }
            SettingsInput::Search => {
                self.editing = None;
                self.panel = None;
                self.searching = true;
            }
            SettingsInput::Save => {
                if self.editing.is_some() {
                    self.accept_edit()?;
                }
                self.save()?;
            }
            SettingsInput::Reset => {
                self.editing = None;
                self.panel = None;
                self.menu.as_mut().ok_or("settings have not loaded")?.reset_selected().map_err(|e| e.to_string())?;
                self.save()?;
            }
            SettingsInput::Accept if self.editing.is_some() => {
                self.accept_edit()?;
                self.save()?;
            }
            SettingsInput::Accept if self.searching => {
                self.searching = false;
            }
            SettingsInput::Accept => self.activate_selected()?,
            SettingsInput::Next | SettingsInput::Previous if self.editing.is_none() => {
                if let Some(menu) = &mut self.menu {
                    menu.move_selection(if matches!(input, SettingsInput::Next) { 1 } else { -1 });
                }
                self.panel = None;
            }
            _ => {
                if let Some(edit) = &mut self.editing {
                    edit.input.edit(input)?;
                } else {
                    self.searching = true;
                    self.panel = None;
                    self.query.edit(input)?;
                    if let Some(menu) = &mut self.menu {
                        menu.search(&self.query.text);
                    }
                }
            }
        }
        Ok(())
    }
    fn activate_selected(&mut self) -> Result<(), String> {
        let row = self.selected_row().ok_or("no selected setting")?;
        let value = match (&row.spec.kind, &row.current) {
            (SettingKind::Bool, SettingValue::Bool(value)) => Some(SettingValue::Bool(!value)),
            (SettingKind::Choice(choices), SettingValue::Text(value)) => {
                let index = choices.iter().position(|choice| choice == value).unwrap_or(0);
                Some(SettingValue::Text(choices[(index + 1) % choices.len()].to_string()))
            }
            (SettingKind::Bindings, _) => {
                self.activate_panel(SettingsPanel::Keymap)?;
                None
            }
            (SettingKind::Formatters, _) => {
                self.activate_panel(SettingsPanel::Formatters)?;
                None
            }
            (SettingKind::StringMap, _) => {
                self.activate_panel(if row.spec.id == "file_types" {
                    SettingsPanel::FileTypes
                } else {
                    SettingsPanel::StringMap { id: row.spec.id }
                })?;
                None
            }
            _ => {
                let text = match row.current {
                    SettingValue::Integer(v) => v.to_string(),
                    SettingValue::Float(v) => v.to_string(),
                    SettingValue::Text(v) => v,
                    // Escaped line entries round-trip embedded controls and
                    // backslashes without shell or TOML parsing on keystrokes.
                    SettingValue::Strings(v) => crate::settings_panels::encode_args(&v),
                    _ => return Err("use the typed document panel for this value".into()),
                };
                self.editing = Some(Editing { id: row.spec.id, kind: row.spec.kind, input: TextInput::new(text)? });
                None
            }
        };
        if let Some(value) = value {
            self.menu.as_mut().unwrap().edit_selected(value).map_err(|e| e.to_string())?;
            self.save()?;
        }
        Ok(())
    }
    fn activate_panel(&mut self, panel: SettingsPanel) -> Result<(), String> {
        let controller = SettingsPanelController::new(panel, self.document().ok_or("settings have not loaded")?)
            .map_err(|e| e.to_string())?;
        self.panel = Some(controller);
        Ok(())
    }
    fn accept_edit(&mut self) -> Result<(), String> {
        let edit = self.editing.as_ref().ok_or("no active setting edit")?;
        let text = &edit.input.text;
        let value = match edit.kind {
            SettingKind::Integer => SettingValue::Integer(text.parse().map_err(|_| "enter a whole number")?),
            SettingKind::Float => SettingValue::Float(text.parse().map_err(|_| "enter a number")?),
            SettingKind::Text => SettingValue::Text(text.clone()),
            SettingKind::Strings => {
                SettingValue::Strings(crate::settings_panels::decode_args(text).map_err(|e| e.to_string())?)
            }
            _ => return Err("setting requires a typed panel".into()),
        };
        self.menu
            .as_mut()
            .ok_or("settings have not loaded")?
            .document_mut()
            .edit(edit.id, value)
            .map_err(|e| e.to_string())?;
        self.editing = None;
        Ok(())
    }
    fn save(&mut self) -> Result<(), String> {
        let result = self.enqueue_save();
        if let Err(error) = &result {
            self.save_error = Some(error.clone());
        }
        result
    }
    fn enqueue_save(&mut self) -> Result<(), String> {
        if self.external.is_some() {
            return Err("settings changed on disk; resolve the reload conflict before saving".into());
        }
        if self.pending.is_some() {
            self.save_again = true;
            return Ok(());
        }
        if !self.is_dirty() && self.source_exists() {
            return Ok(());
        }
        let document = self.document().ok_or("settings have not loaded")?.clone();
        let expected = self.disk_source.as_ref().ok_or("settings have not loaded")?.clone();
        let request = self.next_request;
        self.next_request = request.checked_add(1).ok_or("settings request identity exhausted")?;
        self.client
            .as_ref()
            .ok_or("settings worker stopped")?
            .save(request, expected, document.source().to_owned())
            .map_err(|e| e.to_string())?;
        self.pending = Some(PendingSave { request, document });
        self.save_again = false;
        self.save_error = None;
        Ok(())
    }

    /// Event handler, not a timer/polling hook. Drain even when the UI is hidden.
    /// Returns whether addressed worker events changed screen/runtime state.
    pub fn worker_ready(&mut self) -> bool {
        let events = self.events.drain();
        let changed = !events.is_empty();
        for event in events {
            self.receive(event);
        }
        self.update_status();
        changed
    }
    fn receive(&mut self, event: PreparedSettingsEvent) {
        match event {
            PreparedSettingsEvent::Loaded { source, document, language_mappings } => {
                // A Saved is followed by Loaded for the same source. It must
                // not conflict with edits made while that save was in flight.
                if self.baseline.as_ref().is_some_and(|base| base.source == source) {
                    self.external = None;
                    return;
                }
                if self.is_dirty() || self.editing.is_some() || self.panel_is_editing() || self.pending.is_some() {
                    self.external = Some(External { source, document: *document, language_mappings });
                } else {
                    self.adopt(source, *document, language_mappings, SettingsAppliedReason::Loaded);
                }
            }
            PreparedSettingsEvent::Saved { request, source, language_mappings } => {
                let Some(pending) = self.pending.as_ref().filter(|pending| pending.request == request) else {
                    self.error = Some("unexpected settings save acknowledgement".into());
                    self.save_error = self.error.clone();
                    return;
                };
                if source != pending.document.source() {
                    self.error = Some("settings save acknowledgement has different contents".into());
                    self.save_error = self.error.clone();
                    return;
                }
                // Receipt identity and contents match the pending write. Its
                // disk effect is acknowledged even if semantic preparation
                // subsequently rejects runtime adoption. Corrective saves
                // must compare against these bytes, not old accepted state.
                self.disk_source = Some(Some(source.clone()));
                let language_mappings = match language_mappings {
                    Ok(mappings) => mappings,
                    Err(message) => {
                        self.pending = None;
                        self.save_again = false;
                        self.save_error = Some(message.clone());
                        self.error = Some(message);
                        return;
                    }
                };
                let pending = self.pending.take().unwrap();
                self.publish(
                    &pending.document,
                    &language_mappings,
                    Some(source.clone()),
                    SettingsAppliedReason::Saved { request },
                );
                self.baseline = Some(External { source: Some(source), document: pending.document, language_mappings });
                self.save_error = None;
                if self.save_again {
                    self.save_again = false;
                    if let Err(error) = self.save() {
                        self.error = Some(error);
                    }
                }
            }
            PreparedSettingsEvent::Error { request, message } => {
                if request.is_some() && request == self.pending.as_ref().map(|pending| pending.request) {
                    self.pending = None;
                    self.save_again = false;
                    self.save_error = Some(message.clone());
                }
                self.error = Some(message);
            }
        }
    }
    fn adopt(
        &mut self,
        source: Option<String>,
        document: SettingsDocument,
        language_mappings: AcceptedLanguageMappings,
        reason: SettingsAppliedReason,
    ) {
        self.publish(&document, &language_mappings, source.clone(), reason);
        self.disk_source = Some(source.clone());
        self.baseline = Some(External { source, document: document.clone(), language_mappings });
        if let Some(menu) = &mut self.menu {
            menu.replace_document(document);
        } else {
            let mut menu = SettingsMenu::new(document);
            menu.select_category(self.category);
            menu.search(&self.query.text);
            self.menu = Some(menu);
        }
    }
    fn publish(
        &mut self,
        document: &SettingsDocument,
        language_mappings: &AcceptedLanguageMappings,
        source: Option<String>,
        reason: SettingsAppliedReason,
    ) {
        self.receipt = self.receipt.checked_add(1).expect("settings receipt identity exhausted");
        self.applied = Some(SettingsApplied {
            settings: document.settings().clone(),
            file_types: document.file_types().clone(),
            language_mappings: language_mappings.clone(),
            source,
            receipt: self.receipt,
            reason,
        });
    }
    /// Explicitly discard the local draft in favor of an already parsed reload.
    /// Saving cannot bypass a conflict, and this refuses an in-flight save.
    pub fn adopt_external(&mut self) -> Result<(), String> {
        let result = (|| {
            if self.pending.is_some() {
                return Err("wait for the in-flight settings save".into());
            }
            let external = self.external.take().ok_or("no external settings reload")?;
            self.editing = None;
            self.panel = None;
            self.save_again = false;
            self.save_error = None;
            self.adopt(external.source, external.document, external.language_mappings, SettingsAppliedReason::Loaded);
            Ok(())
        })();
        self.finish_action(result)
    }
    pub fn reload(&mut self) -> Result<(), String> {
        let result = self
            .client
            .as_ref()
            .ok_or_else(|| "settings worker stopped".to_owned())
            .and_then(|client| client.reload().map_err(|e| e.to_string()));
        self.finish_action(result)
    }
    fn update_status(&mut self) {
        self.status = if self.client.is_none() {
            "Settings worker stopped".into()
        } else if self.external.is_some() {
            "Conflict: settings changed on disk; local edits retained".into()
        } else if self.pending.is_some() {
            "Saving settings…".into()
        } else if self.menu.is_none() {
            "Loading settings…".into()
        } else if self.is_dirty() {
            "Unsaved settings".into()
        } else if self.panel.is_some() {
            "Enter to edit or confirm; Escape cancels a stage, query, then panel".into()
        } else if self.editing.is_some() {
            "Enter to save; Escape to cancel (lists: one string per line)".into()
        } else {
            "Settings ready".into()
        };
        if let Some(error) = &self.error {
            self.status = format!("{} — Error: {error}", self.status);
        }
    }

    /// Nonblocking ownership handoff. Call handle.shutdown() after leaving the
    /// interactive event loop, and wait for completion before process exit.
    /// Unaccepted scalar drafts are never committed by shutdown.
    pub fn stop(&mut self) -> SettingsShutdown {
        let mut stopped = Self::state(self.path.clone(), Arc::new(EventQueue::default()));
        stopped.update_status();
        let mut session = std::mem::replace(self, stopped);
        session.open = false;
        SettingsShutdown { session: Some(session) }
    }
    fn drain_accepted(&mut self) -> Result<(), String> {
        self.worker_ready();
        loop {
            if let Some(error) = &self.save_error {
                return Err(error.clone());
            }
            if self.pending.is_none() {
                return Ok(());
            }
            self.events.wait()?;
            self.worker_ready();
        }
    }
    // Shared by explicit shutdown after draining, and implicit Drop without a
    // drain. Never call stop here: the shutdown handle itself owns a session.
    fn retire_now(&mut self) -> Option<JoinHandle<()>> {
        self.open = false;
        self.events.close(); // releases a producer blocked by event backpressure
        self.client = None; // reaper retains the final worker Arc
        if let Some(retire) = self.retire.take() {
            let _ = retire.try_send(());
        }
        self.update_status();
        self.retirement.take()
    }
}
impl Drop for SettingsSession {
    fn drop(&mut self) {
        let _ = self.retire_now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::highlighting::Language;
    use std::time::Duration;

    type Requests = TestRequests;
    fn fixture(source: Option<&str>) -> (SettingsSession, Requests) {
        let events = Arc::new(EventQueue::default());
        let mut session = SettingsSession::state(PathBuf::from("unused-settings-test.toml"), events);
        let requests = Arc::new(Mutex::new(Vec::new()));
        session.client = Some(Client::Fake(requests.clone()));
        loaded(&mut session, source);
        session.take_applied();
        session.open();
        (session, requests)
    }
    fn loaded(session: &mut SettingsSession, source: Option<&str>) {
        session.events.push(
            SettingsWorkerEvent::Loaded {
                source: source.map(str::to_owned),
                document: Box::new(SettingsDocument::parse(source.unwrap_or("")).unwrap()),
            },
            &|| {},
        );
        assert!(session.worker_ready());
    }
    fn saved(session: &mut SettingsSession, request: &(u64, Option<String>, String)) {
        session.events.push(SettingsWorkerEvent::Saved { request: request.0, source: request.2.clone() }, &|| {});
        session.worker_ready();
    }

    #[test]
    fn settings_mapping_loaded_candidate_is_prepared_before_enqueue_and_rejected_whole() {
        let old = "# old accepted\n[editor]\ntab_size=4\n[file_types]\nrs='rust'\n";
        let (mut session, _) = fixture(Some(old));
        let valid = "# new accepted\n[editor]\ntab_size=6\n[file_types]\nrs='py'\n";
        loaded(&mut session, Some(valid));
        let accepted = session.take_applied().unwrap();
        assert_eq!(accepted.settings.editor.tab_size, 6);
        assert_eq!(accepted.file_types["rs"], "py");
        assert_eq!(accepted.language_mappings.mappings()[0].language, Language::Python);
        assert_eq!(accepted.source.as_deref(), Some(valid));
        let receipt = accepted.receipt;
        let invalid = "[editor]\ntab_size=9\n[file_types]\nrs='lua'\nz='unknown-grammar'\n";
        let producer = EventProducer(session.events.clone());
        producer.push(
            SettingsWorkerEvent::Loaded {
                source: Some(invalid.into()),
                document: Box::new(SettingsDocument::parse(invalid).unwrap()),
            },
            &|| {},
        );
        // Validation has already finished on the producer side. The UI cannot
        // see an unprepared candidate, even with otherwise valid editor fields.
        assert!(matches!(
            session.events.mailbox.lock().unwrap().events.front(),
            Some(PreparedSettingsEvent::Error { request: None, .. })
        ));
        assert!(session.worker_ready());
        assert!(session.take_applied().is_none());
        assert_eq!(session.receipt, receipt);
        let baseline = session.baseline.as_ref().unwrap();
        assert_eq!(baseline.source.as_deref(), Some(valid));
        assert_eq!(baseline.document.settings().editor.tab_size, 6);
        assert_eq!(baseline.language_mappings, accepted.language_mappings);
        assert_eq!(session.document().unwrap().source(), valid);
        assert!(!session.has_conflict(), "invalid reload must not create an adoptable partial candidate");
        assert!(session.status().contains("unsupported language"));

        // A truly removed file is a valid empty/default candidate, distinct
        // from an invalid candidate that must retain the previous runtime.
        loaded(&mut session, None);
        let defaults = session.take_applied().unwrap();
        assert_eq!(defaults.settings.editor.tab_size, 4);
        assert!(defaults.file_types.is_empty());
        assert!(defaults.language_mappings.mappings().is_empty());
        assert_eq!(defaults.source, None);
    }

    #[test]
    fn settings_mapping_invalid_initial_load_does_not_publish_default_fallback() {
        let mut session = SettingsSession::state(PathBuf::new(), Arc::new(EventQueue::default()));
        let source = "[file_types]\nrs='unavailable'\n";
        loaded(&mut session, Some(source));
        assert!(session.menu.is_none());
        assert!(session.baseline.is_none());
        assert_eq!(session.receipt, 0);
        assert!(session.take_applied().is_none());
        assert!(session.error.as_ref().unwrap().contains("unsupported language"));
    }

    #[test]
    fn settings_mapping_excessive_loaded_table_retains_accepted_runtime_limit() {
        let original = "[file_types]\nrs='rust'\n";
        let (mut session, _) = fixture(Some(original));
        let receipt = session.receipt;
        let source = format!(
            "[file_types]\n{}",
            (0..=kwil_core_syntax::detection::MAX_MAPPINGS).map(|n| format!("ext{n}='rust'\n")).collect::<String>()
        );
        loaded(&mut session, Some(&source));
        assert!(session.status().contains("at most"));
        assert_eq!(session.receipt, receipt);
        assert!(session.take_applied().is_none());
        assert_eq!(session.document().unwrap().source(), original);
        assert_eq!(session.baseline.as_ref().unwrap().language_mappings.mappings().len(), 1);
        assert_eq!(session.disk_source.as_ref().unwrap().as_deref(), Some(original));
        assert!(!session.has_conflict(), "excessive mappings cannot become an adoptable runtime candidate");
    }

    #[test]
    fn settings_mapping_saved_receipt_uses_matching_pending_source_not_latest_draft() {
        let original = "# preserve this comment\n[file_types]\nrs='rust'\n";
        let (mut session, requests) = fixture(Some(original));
        session.edit_document(|doc| doc.map_extension("rs", Some("py"))).unwrap();
        let a = requests.lock().unwrap()[0].clone();
        session.edit_document(|doc| doc.map_extension("rs", Some("js"))).unwrap();
        let b_source = session.document().unwrap().source().to_owned();
        assert_eq!(requests.lock().unwrap().len(), 1);

        // Invalid candidate preparation must not bypass the source fence and
        // accidentally settle a different pending document with this request.
        saved(&mut session, &(a.0, None, "[file_types]\nrs='unavailable'\n".into()));
        assert!(session.is_saving());
        assert_eq!(session.pending.as_ref().unwrap().document.source(), a.2);
        assert_eq!(session.disk_source.as_ref().unwrap().as_deref(), Some(original));
        assert!(session.take_applied().is_none());

        // A prepared but mismatched acknowledgement must publish neither the
        // settings nor the mappings from newer B and must keep A in flight.
        saved(&mut session, &(a.0, None, b_source.clone()));
        assert!(session.is_saving());
        assert!(session.take_applied().is_none());
        assert_eq!(session.baseline.as_ref().unwrap().language_mappings.mappings()[0].language, Language::Rust);
        saved(&mut session, &a);
        let applied_a = session.take_applied().unwrap();
        assert_eq!(applied_a.reason, SettingsAppliedReason::Saved { request: a.0 });
        assert_eq!(applied_a.source.as_deref(), Some(a.2.as_str()));
        assert_eq!(applied_a.file_types["rs"], "py");
        assert_eq!(applied_a.language_mappings.mappings()[0].language, Language::Python);
        assert_eq!(session.document().unwrap().file_types()["rs"], "js");
        let b = requests.lock().unwrap()[1].clone();
        assert_eq!(b.1.as_deref(), Some(a.2.as_str()));
        assert_eq!(b.2, b_source);
        assert!(b.2.contains("# preserve this comment"));
        loaded(&mut session, Some(&a.2));
        assert!(!session.has_conflict(), "echoed Loaded A must not conflict with pending B");
        assert!(session.take_applied().is_none());
        saved(&mut session, &b);
        let applied_b = session.take_applied().unwrap();
        assert_eq!(applied_b.file_types["rs"], "js");
        assert_eq!(applied_b.language_mappings.mappings()[0].language, Language::JavaScript);
        assert_eq!(applied_b.source.as_deref(), Some(b.2.as_str()));
        assert!(applied_b.receipt > applied_a.receipt);
    }

    #[test]
    fn settings_mapping_external_conflict_adopts_its_own_prepared_candidate_after_pending_save() {
        let (mut session, requests) = fixture(Some("[file_types]\nrs='rust'\n"));
        session.edit_document(|doc| doc.map_extension("rs", Some("python"))).unwrap();
        let a = requests.lock().unwrap()[0].clone();
        session.edit_document(|doc| doc.map_extension("rs", Some("lua"))).unwrap();
        let external = "# external candidate\n[editor]\ntab_size=9\n[file_types]\nrs='ruby'\n";
        loaded(&mut session, Some(external));
        assert!(session.has_conflict());
        assert!(session.take_applied().is_none());
        assert!(session.adopt_external().is_err(), "pending save must settle first");
        loaded(&mut session, Some("[file_types]\nrs='missing-provider-language'\n"));
        assert_eq!(session.external.as_ref().unwrap().source.as_deref(), Some(external));
        assert_eq!(session.external.as_ref().unwrap().language_mappings.mappings()[0].language, Language::Ruby);
        saved(&mut session, &a);
        assert_eq!(session.take_applied().unwrap().language_mappings.mappings()[0].language, Language::Python);
        assert_eq!(requests.lock().unwrap().len(), 1, "external conflict must block coalesced local save");
        session.adopt_external().unwrap();
        let adopted = session.take_applied().unwrap();
        assert_eq!(adopted.reason, SettingsAppliedReason::Loaded);
        assert_eq!(adopted.source.as_deref(), Some(external));
        assert_eq!(adopted.settings.editor.tab_size, 9);
        assert_eq!(adopted.file_types["rs"], "ruby");
        assert_eq!(adopted.language_mappings.mappings()[0].language, Language::Ruby);
        assert_eq!(session.document().unwrap().source(), external);
        assert!(!session.has_conflict());
    }

    #[test]
    fn settings_mapping_invalid_saved_candidate_retains_runtime_and_fails_shutdown_drain() {
        let original = "[editor]\ntab_size=4\n[file_types]\nrs='rust'\n";
        let (mut session, requests) = fixture(Some(original));
        let receipt = session.receipt;
        session
            .edit_document(|doc| {
                doc.map_extension("rs", Some("unavailable"))?;
                doc.edit("editor.tab_size", SettingValue::Integer(9))
            })
            .unwrap();
        let a = requests.lock().unwrap()[0].clone();
        saved(&mut session, &a);
        assert!(!session.is_saving());
        assert!(session.is_dirty(), "retain the rejected local draft for correction");
        assert_eq!(session.receipt, receipt);
        assert!(session.take_applied().is_none());
        let baseline = session.baseline.as_ref().unwrap();
        assert_eq!(baseline.source.as_deref(), Some(original));
        assert_eq!(baseline.document.settings().editor.tab_size, 4);
        assert_eq!(baseline.language_mappings.mappings()[0].language, Language::Rust);
        assert!(session.status().contains("saved settings were not applied"));
        // The backend write was acknowledged, so rejection must not claim an
        // on-disk rollback or a successful accepted-save shutdown.
        let error = session.stop().shutdown().unwrap_err();
        assert!(error.contains("unsupported language"));
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn settings_mapping_rejected_saved_corrections_write_against_acknowledged_disk_source() {
        struct Disk(PathBuf);
        impl Drop for Disk {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
                let _ = std::fs::remove_dir(self.0.parent().unwrap());
            }
        }
        for restore_original in [true, false] {
            let original = "# retained comment\n[editor]\ntab_size=4\n[file_types]\nrs='rust'\n";
            let folder = std::env::temp_dir().join(format!(
                "kwil-settings-correction-{}-{}-{restore_original}",
                std::process::id(),
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
            ));
            std::fs::create_dir(&folder).unwrap();
            let disk = Disk(folder.join("settings.toml"));
            kwil_core_commands::settings::write_settings_atomic(&disk.0, None, original).unwrap();
            let (mut session, requests) = fixture(Some(original));
            let accepted_receipt = session.receipt;
            session.edit_document(|doc| doc.map_extension("rs", Some("unavailable"))).unwrap();
            let a = requests.lock().unwrap()[0].clone();
            // Reproduce an already-written invalid candidate (for example a
            // legacy writer). The new worker preflight prevents new ones.
            kwil_core_commands::settings::write_settings_atomic(&disk.0, a.1.as_deref(), &a.2).unwrap();
            saved(&mut session, &a);
            assert_eq!(std::fs::read_to_string(&disk.0).unwrap(), a.2);
            assert_eq!(session.disk_source.as_ref().unwrap().as_deref(), Some(a.2.as_str()));
            assert_eq!(session.baseline.as_ref().unwrap().source.as_deref(), Some(original));
            assert_eq!(session.receipt, accepted_receipt);
            assert!(session.take_applied().is_none());
            assert!(session.status().contains("saved settings were not applied"));

            let corrected_source =
                if restore_original { original.to_owned() } else { original.replace("'rust'", "'py'") };
            // Prepare the test's replacement document outside the session.
            let corrected = SettingsDocument::parse(&corrected_source).unwrap();
            session
                .edit_document(move |doc| {
                    *doc = corrected;
                    Ok(())
                })
                .unwrap();
            assert!(session.is_saving(), "restoring exact old source must still enqueue the corrective write");
            assert!(session.is_dirty());
            let b = requests.lock().unwrap()[1].clone();
            assert_eq!(b.1.as_deref(), Some(a.2.as_str()), "compare against bytes actually acknowledged on disk");
            assert_eq!(b.2, corrected_source);
            assert_eq!(session.receipt, accepted_receipt, "correction must await its own acknowledgement");
            assert!(session.take_applied().is_none());
            kwil_core_commands::settings::write_settings_atomic(&disk.0, b.1.as_deref(), &b.2).unwrap();
            assert_eq!(std::fs::read_to_string(&disk.0).unwrap(), corrected_source);
            assert!(session.take_applied().is_none(), "disk write alone cannot advance runtime acceptance");
            saved(&mut session, &b);
            let applied = session.take_applied().unwrap();
            assert_eq!(applied.source.as_deref(), Some(corrected_source.as_str()));
            assert_eq!(
                applied.language_mappings.mappings()[0].language,
                if restore_original { Language::Rust } else { Language::Python }
            );
            assert!(applied.receipt > accepted_receipt);
            assert!(!session.is_dirty());
            assert!(!session.is_saving());
        }
    }

    fn select(session: &mut SettingsSession, query: &str) {
        session.query = TextInput::new(query.into()).unwrap();
        session.menu.as_mut().unwrap().search(query);
        session.searching = false;
        let rows = session.rows();
        let index = rows.iter().position(|row| row.spec.id == query).unwrap_or_else(|| {
            assert_eq!(rows.len(), 1, "query must identify one row");
            0
        });
        session.select(index);
    }

    fn activate_panel(session: &mut SettingsSession, id: &str) {
        select(session, id);
        session.input(SettingsInput::Accept).unwrap();
        assert!(session.panel_window(0, 8).unwrap().is_some());
    }

    #[test]
    fn settings_panel_session_capture_confirms_and_publishes_only_saved_receipt() {
        let original = "# keep\nkeymap = 'vscode'\n";
        let (mut session, requests) = fixture(Some(original));
        activate_panel(&mut session, "bindings");
        session.panel_input(PanelInput::Text("file.save")).unwrap();
        session.panel_input(PanelInput::Accept).unwrap();
        assert!(session.panel_is_capturing());
        session.panel_input(PanelInput::Capture("ctrl+c".parse().unwrap())).unwrap();
        session.panel_input(PanelInput::Accept).unwrap();
        let window = session.panel_window(0, 8).unwrap().unwrap();
        assert!(!window.confirmation.is_empty());
        assert!(session.panel_is_editing());
        assert!(requests.lock().unwrap().is_empty());
        assert!(session.take_applied().is_none());
        session.panel_input(PanelInput::Accept).unwrap();
        let request = requests.lock().unwrap()[0].clone();
        assert_eq!(request.1.as_deref(), Some(original));
        assert!(request.2.contains("# keep"));
        assert!(session.take_applied().is_none());
        assert!(
            session
                .document()
                .unwrap()
                .settings()
                .bindings
                .iter()
                .any(|b| b.command == "file.save" && b.keys == "ctrl+c")
        );
        // An acknowledgement for an earlier accepted edit cannot end a new capture.
        session.panel_input(PanelInput::Accept).unwrap();
        assert!(session.panel_is_capturing());
        saved(&mut session, &request);
        let applied = session.take_applied().unwrap();
        assert!(matches!(applied.reason, SettingsAppliedReason::Saved { request: id } if id == request.0));
        assert!(applied.settings.bindings.iter().any(|b| b.command == "file.save" && b.keys == "ctrl+c"));
        assert_eq!(
            SettingsDocument::parse(applied.source.as_deref().unwrap()).unwrap().settings().bindings,
            applied.settings.bindings
        );
        loaded(&mut session, Some(&request.2));
        assert!(!session.has_conflict());
        assert!(session.panel_is_capturing());
    }

    #[test]
    fn settings_panel_session_mapping_save_reopens_and_keeps_navigation_on_draft() {
        let (mut session, requests) = fixture(Some(""));
        activate_panel(&mut session, "file_types");
        session.panel_input(PanelInput::Accept).unwrap();
        session.panel_input(PanelInput::Text("rs")).unwrap();
        assert!(session.panel_select(usize::MAX).unwrap_err().contains("finish or cancel"));
        session.close();
        session.panel_input(PanelInput::Text("ignored")).unwrap();
        session.open();
        let selected = session.selected_row().unwrap().spec.id;
        session.select(usize::MAX);
        session.set_category(Some(Category::Appearance));
        session.cycle_category(false);
        session.input(SettingsInput::Search).unwrap();
        assert_eq!(session.selected_row().unwrap().spec.id, selected);
        assert_eq!(session.category(), None);
        assert_eq!(session.panel_window(0, 1).unwrap().unwrap().edit_text.unwrap().key, "rs");
        session.panel_input(PanelInput::Accept).unwrap();
        session.panel_input(PanelInput::Text("rust")).unwrap();
        session.panel_input(PanelInput::Accept).unwrap();
        assert_eq!(session.document().unwrap().language_for_extension("rs"), Some("rust"));
        assert!(session.take_applied().is_none());
        assert_eq!(requests.lock().unwrap().len(), 1, "a completed panel edit must enqueue an optimistic save");
        let request = requests.lock().unwrap()[0].clone();
        saved(&mut session, &request);
        assert_eq!(session.take_applied().unwrap().file_types.get("rs").map(String::as_str), Some("rust"));
        session.panel_input(PanelInput::Cancel).unwrap();
        assert_eq!(session.panel(), None);
        assert!(session.is_open());
        assert!(session.panel_window(usize::MAX, 1).unwrap().is_none());
    }

    #[test]
    fn settings_panel_session_cancel_unwinds_capture_confirmation_edit_query_then_panel() {
        for confirm in [false, true] {
            let (mut session, requests) = fixture(Some(""));
            activate_panel(&mut session, "bindings");
            session.panel_input(PanelInput::Text("file.save")).unwrap();
            session.panel_input(PanelInput::Accept).unwrap();
            if confirm {
                session.panel_input(PanelInput::Capture("ctrl+c".parse().unwrap())).unwrap();
                session.panel_input(PanelInput::Accept).unwrap();
                assert!(!session.panel_window(0, 1).unwrap().unwrap().confirmation.is_empty());
            }
            session.input(SettingsInput::Cancel).unwrap();
            assert_eq!(session.panel(), Some(SettingsPanel::Keymap));
            assert!(!session.panel_is_editing());
            assert_eq!(session.panel_window(0, 1).unwrap().unwrap().query, "file.save");
            session.panel_input(PanelInput::Cancel).unwrap();
            assert_eq!(session.panel_window(0, 1).unwrap().unwrap().query, "");
            session.panel_input(PanelInput::Cancel).unwrap();
            assert_eq!(session.panel(), None);
            assert!(session.is_open());
            assert!(requests.lock().unwrap().is_empty());
        }
        let (mut session, _) = fixture(Some(""));
        activate_panel(&mut session, "file_types");
        session.panel_input(PanelInput::Accept).unwrap();
        session.panel_input(PanelInput::Text("unfinished")).unwrap();
        session.panel_input(PanelInput::Cancel).unwrap();
        assert!(!session.panel_is_editing());
        assert_eq!(session.panel(), Some(SettingsPanel::FileTypes));
        session.panel_input(PanelInput::Cancel).unwrap();
        assert!(session.is_open());
    }

    #[test]
    fn settings_panel_session_reload_preserves_capture_and_mapping_draft_until_explicit_adoption() {
        for id in ["bindings", "file_types"] {
            let (mut session, requests) = fixture(Some(""));
            activate_panel(&mut session, id);
            session.panel_input(PanelInput::Accept).unwrap();
            if id == "file_types" {
                session.panel_input(PanelInput::Text("rs")).unwrap();
            } else {
                session.panel_input(PanelInput::Capture("ctrl+k".parse().unwrap())).unwrap();
            }
            loaded(&mut session, Some("[editor]\ntab_size=9\n"));
            assert!(session.has_conflict(), "unaccepted panel state must block silent adoption");
            assert!(session.panel_is_editing());
            assert_eq!(session.document().unwrap().settings().editor.tab_size, 4);
            assert!(session.take_applied().is_none());
            assert!(session.input(SettingsInput::Save).is_err());
            assert!(requests.lock().unwrap().is_empty());
            let window = session.panel_window(0, 1).unwrap().unwrap();
            if id == "file_types" {
                assert_eq!(window.edit_text.unwrap().key, "rs");
                session.panel_input(PanelInput::Accept).unwrap();
                session.panel_input(PanelInput::Text("rust")).unwrap();
                assert!(session.panel_input(PanelInput::Accept).unwrap_err().contains("reload conflict"));
                assert_eq!(session.document().unwrap().language_for_extension("rs"), Some("rust"));
                assert!(session.is_dirty(), "completed edits survive a rejected save");
                assert!(requests.lock().unwrap().is_empty());
            } else {
                assert_eq!(window.capture.as_deref(), Some("ctrl+k"));
            }
            session.adopt_external().unwrap();
            assert_eq!(session.panel(), None);
            assert_eq!(session.take_applied().unwrap().settings.editor.tab_size, 9);
        }
    }

    #[test]
    fn settings_panel_session_confirmation_rechecks_current_document_and_retains_new_conflicts() {
        let (mut session, requests) = fixture(Some(""));
        activate_panel(&mut session, "bindings");
        session.panel_input(PanelInput::Text("file.save")).unwrap();
        session.panel_input(PanelInput::Accept).unwrap();
        session.panel_input(PanelInput::Capture("ctrl+c".parse().unwrap())).unwrap();
        session.panel_input(PanelInput::Accept).unwrap();
        session
            .edit_document(|doc| {
                doc.rebind(
                    kwil_core_commands::Binding {
                        command: "file.open".into(),
                        keys: "ctrl+c".into(),
                        context: None,
                        mode: None,
                    },
                    true,
                )
            })
            .unwrap();
        let before = session.document().unwrap().source().to_owned();
        assert!(session.panel_input(PanelInput::Accept).unwrap_err().contains("conflicts changed"));
        assert_eq!(session.document().unwrap().source(), before);
        assert!(session.panel_window(0, 1).unwrap().unwrap().confirmation.iter().any(|c| c.command == "file.open"));
        assert_eq!(requests.lock().unwrap().len(), 1);
        session.panel_input(PanelInput::Accept).unwrap();
        assert!(
            session
                .document()
                .unwrap()
                .settings()
                .bindings
                .iter()
                .any(|b| b.command == "file.save" && b.keys == "ctrl+c")
        );
    }

    #[test]
    fn settings_panel_capacity_rejects_add_before_save_and_never_traps_cancel() {
        let source = format!("[file_types]\n{}", (0..4095).map(|n| format!("ext{n}='rust'\n")).collect::<String>());
        let (mut session, requests) = fixture(Some(""));
        // Exercise the generic panel's independent row-capacity/escape
        // contract using a constructed local draft, not an accepted Loaded
        // event. Production rejects tables above Detector::MAX_MAPPINGS;
        // the separate semantic-limit test above keeps that bound binding.
        session.menu = Some(SettingsMenu::new(SettingsDocument::parse(&source).unwrap()));
        activate_panel(&mut session, "file_types");
        session.panel_input(PanelInput::Text("no matching mapping")).unwrap();
        session.panel_input(PanelInput::Accept).unwrap();
        session.panel_input(PanelInput::Text("another")).unwrap();
        session.panel_input(PanelInput::Accept).unwrap();
        session.panel_input(PanelInput::Text("rust")).unwrap();
        let result = session.panel_input(PanelInput::Accept);
        assert!(result.is_err(), "candidate exceeding panel row capacity must be refused");
        assert_eq!(session.document().unwrap().file_types().len(), 4095);
        assert!(requests.lock().unwrap().is_empty(), "refused panel additions cannot autosave");
        // The raw settings file/typed API may independently exceed this UI's
        // capacity. Keep an unconditional escape route in that case too.
        session.edit_document(|doc| doc.map_extension("another", Some("rust"))).unwrap();
        assert!(session.panel_window(0, 1).is_err());
        assert!(session.panel_input(PanelInput::Cancel).is_ok(), "Escape must remain available for an invalid panel");
        assert!(!session.panel_is_editing(), "Escape must cancel a draft even when rows cannot be built");
        assert!(session.panel_input(PanelInput::Cancel).is_ok(), "Escape must clear an invalid panel query");
        assert_eq!(session.panel_query_cursor(), 0);
        session.panel_input(PanelInput::Cancel).unwrap();
        assert!(session.panel().is_none());
        assert!(session.is_open());
    }

    #[test]
    fn settings_panel_absolute_selection_builds_rows_once() {
        let source = format!("[file_types]\n{}", (0..32).map(|n| format!("ext{n}='rust'\n")).collect::<String>());
        let (mut session, _) = fixture(Some(&source));
        activate_panel(&mut session, "file_types");
        crate::settings_panels::take_row_builds();
        session.panel_select(usize::MAX).unwrap();
        assert_eq!(crate::settings_panels::take_row_builds(), 1, "absolute selection must build the panel once");
        assert_eq!(session.panel_window(0, 0).unwrap().unwrap().selection, 32);
    }

    #[test]
    fn settings_panel_session_typed_maps_formatters_generic_paste_and_bounded_selection() {
        for (id, key, value) in [
            ("file_types", "rs", "rust"),
            ("theme.colors", "editor.background", "#112233"),
            ("formatters", "rust.executable", "rustfmt"),
        ] {
            let (mut session, requests) = fixture(Some(""));
            activate_panel(&mut session, id);
            session.input(SettingsInput::Accept).unwrap();
            session.input(SettingsInput::Text(key)).unwrap();
            session.input(SettingsInput::Accept).unwrap();
            session.input(SettingsInput::Text(value)).unwrap();
            let window = session.panel_window(0, 1).unwrap().unwrap();
            assert_eq!(window.edit_text.unwrap().value, value);
            session.input(SettingsInput::Save).unwrap();
            assert!(requests.lock().unwrap().is_empty(), "Save must not accept panel text");
            session.input(SettingsInput::Accept).unwrap();
            assert_eq!(requests.lock().unwrap().len(), 1);
            assert!(session.search_text().is_empty() || session.search_text() == id);
            assert!(session.panel_window(0, crate::settings_panels::PANEL_ROW_LIMIT + 1).is_err());
            session.panel_select(usize::MAX).unwrap();
            let window = session.panel_window(usize::MAX, 1).unwrap().unwrap();
            assert!(window.rows.is_empty());
            assert_eq!(window.selection, window.total - 1);
            session.panel_select(0).unwrap();
            session.panel_input(PanelInput::Text("é")).unwrap();
            assert_eq!(session.panel_query_cursor(), "é".len());
            session.panel_input(PanelInput::Capture("escape".parse().unwrap())).unwrap();
            assert_eq!(session.panel_query_cursor(), 0);
            assert!(session.panel().is_some());
            session.panel_input(PanelInput::Cancel).unwrap();
            assert!(session.panel().is_none());
            session.input(SettingsInput::Cancel).unwrap(); // generic search was not active
            assert!(!session.is_open());
        }
    }

    #[test]
    fn settings_session_actual_menu_search_categories_and_typed_panels() {
        let (mut session, requests) = fixture(None);
        session.set_category(Some(Category::Editor));
        session.input(SettingsInput::Search).unwrap();
        session.input(SettingsInput::Text("indent width")).unwrap();
        assert_eq!(session.selected_row().unwrap().spec.id, "editor.tab_size");
        assert_eq!(session.selected(), 0);
        session.input(SettingsInput::Accept).unwrap();
        session.input(SettingsInput::Accept).unwrap();
        assert!(session.is_editing());
        session.input(SettingsInput::Home).unwrap();
        session.input(SettingsInput::Delete).unwrap();
        session.input(SettingsInput::Text("6")).unwrap();
        session.input(SettingsInput::Accept).unwrap();
        assert_eq!(session.document().unwrap().settings().editor.tab_size, 6);
        assert_eq!(requests.lock().unwrap().len(), 1);
        assert!(session.take_applied().is_none(), "an enqueue is not a save receipt");
        session.set_category(None);
        select(&mut session, "file_types");
        session.input(SettingsInput::Accept).unwrap();
        assert_eq!(session.panel(), Some(SettingsPanel::FileTypes));
        session.edit_document(|doc| doc.map_extension("rs", Some("rust"))).unwrap();
        assert_eq!(session.document().unwrap().language_for_extension("RS"), Some("rust"));
        select(&mut session, "formatters");
        session.input(SettingsInput::Accept).unwrap();
        assert_eq!(session.panel(), Some(SettingsPanel::Formatters));
        session
            .edit_document(|doc| doc.edit_formatter("rust", "executable", SettingValue::Text("rustfmt".into())))
            .unwrap();
        assert_eq!(session.document().unwrap().settings().formatters["rust"].executable, "rustfmt");
        select(&mut session, "bindings");
        session.input(SettingsInput::Accept).unwrap();
        assert_eq!(session.panel(), Some(SettingsPanel::Keymap));
        assert!(!session.document().unwrap().keymap_rows().unwrap().is_empty());
        let binding = kwil_core_commands::Binding {
            keys: "ctrl+c".into(),
            command: "file.save".into(),
            context: None,
            mode: None,
        };
        assert!(!session.document().unwrap().conflicts(&binding).unwrap().is_empty());
        let before = session.document().unwrap().source().to_owned();
        assert!(session.edit_document(|doc| doc.rebind(binding.clone(), false)).is_err());
        assert_eq!(session.document().unwrap().source(), before);
        session.edit_document(|doc| doc.rebind(binding, true)).unwrap();
        assert!(
            session
                .document()
                .unwrap()
                .settings()
                .bindings
                .iter()
                .any(|b| b.command == "file.save" && b.keys == "ctrl+c")
        );
    }

    #[test]
    fn settings_list_paste_normalizes_crlf_and_round_trips_explicit_controls() {
        let (mut session, _) = fixture(Some("[search]\ninclude=[]\n"));
        select(&mut session, "search.include");
        session.input(SettingsInput::Accept).unwrap();
        session.input(SettingsInput::Text("*.rs\r\n*.toml\\r")).unwrap();
        session.input(SettingsInput::Accept).unwrap();
        assert_eq!(session.document().unwrap().settings().search.include, ["*.rs", "*.toml\r"]);
        session.input(SettingsInput::Accept).unwrap();
        assert_eq!(session.input_text(), "*.rs\n*.toml\\r");
    }

    #[test]
    fn settings_session_saves_are_optimistic_coalesced_and_apply_only_acknowledged_document() {
        let original = "# keep this\n[editor]\ntab_size = 4\n";
        let (mut session, requests) = fixture(Some(original));
        session.edit_document(|doc| doc.edit("editor.tab_size", SettingValue::Integer(6))).unwrap();
        let first = requests.lock().unwrap()[0].clone();
        assert_eq!(first.1.as_deref(), Some(original));
        assert!(first.2.contains("# keep this"));
        session.edit_document(|doc| doc.edit("editor.tab_size", SettingValue::Integer(7))).unwrap();
        session.edit_document(|doc| doc.edit("editor.tab_size", SettingValue::Integer(8))).unwrap();
        assert_eq!(requests.lock().unwrap().len(), 1, "only one save may be in flight");
        assert!(session.take_applied().is_none());
        assert!(session.status().contains("Saving"));
        saved(&mut session, &first);
        let applied = session.take_applied().unwrap();
        assert_eq!(applied.settings.editor.tab_size, 6);
        assert_eq!(applied.reason, SettingsAppliedReason::Saved { request: first.0 });
        assert_eq!(session.document().unwrap().settings().editor.tab_size, 8);
        let second = requests.lock().unwrap()[1].clone();
        assert_eq!(second.1, Some(first.2.clone()));
        assert!(second.2.contains("tab_size = 8"));
        loaded(&mut session, Some(&first.2)); // worker's own post-save load
        assert!(!session.has_conflict());
        saved(&mut session, &second);
        assert_eq!(session.take_applied().unwrap().settings.editor.tab_size, 8);
        assert!(!session.is_dirty());
        assert!(!session.is_saving());
    }

    #[test]
    fn settings_session_external_conflicts_preserve_dirty_document_and_text_draft() {
        let (mut session, requests) = fixture(None);
        session.edit_document(|doc| doc.edit("editor.tab_size", SettingValue::Integer(6))).unwrap();
        let first = requests.lock().unwrap()[0].clone();
        session
            .events
            .push(SettingsWorkerEvent::Error { request: Some(first.0), message: "disk unavailable".into() }, &|| {});
        session.worker_ready();
        loaded(&mut session, Some("[editor]\ntab_size=9\n"));
        assert!(session.has_conflict());
        assert_eq!(session.document().unwrap().settings().editor.tab_size, 6);
        assert!(session.take_applied().is_none());
        assert!(session.input(SettingsInput::Save).is_err());
        assert_eq!(requests.lock().unwrap().len(), 1);
        session.adopt_external().unwrap();
        assert_eq!(session.take_applied().unwrap().settings.editor.tab_size, 9);
        assert!(!session.has_conflict());
        select(&mut session, "editor.tab_size");
        session.input(SettingsInput::Accept).unwrap();
        session.input(SettingsInput::Text("0")).unwrap();
        loaded(&mut session, Some("[editor]\ntab_size=7\n"));
        assert!(session.has_conflict(), "an unaccepted text draft also needs protection");
        assert_eq!(session.input_text(), "90");
        assert_eq!(session.document().unwrap().settings().editor.tab_size, 9);
        assert!(session.status().contains("Conflict"));
    }

    #[test]
    fn settings_session_invalid_edit_reset_and_error_status_survive_close() {
        let (mut session, requests) = fixture(Some("[editor]\ntab_size=7\n"));
        select(&mut session, "editor.tab_size");
        session.input(SettingsInput::Accept).unwrap();
        session.input(SettingsInput::Home).unwrap();
        session.input(SettingsInput::Delete).unwrap();
        session.input(SettingsInput::Text("0")).unwrap();
        assert!(session.input(SettingsInput::Accept).is_err());
        assert!(session.is_editing());
        assert_eq!(session.document().unwrap().settings().editor.tab_size, 7);
        assert!(requests.lock().unwrap().is_empty());
        session.close();
        session.open();
        assert!(session.status().contains("Error"));
        assert_eq!(session.input_text(), "0");
        session.input(SettingsInput::Cancel).unwrap();
        session.clear_error();
        session.input(SettingsInput::Reset).unwrap();
        assert_eq!(session.document().unwrap().settings().editor.tab_size, Settings::default().editor.tab_size);
        assert_eq!(requests.lock().unwrap().len(), 1);
        assert!(session.take_applied().is_none());
        // Multiple typed operations are one transaction even when backend
        // individual edits succeed before a later validation error.
        let before = session.document().unwrap().source().to_owned();
        assert!(
            session
                .edit_document(|doc| {
                    doc.map_extension("rs", Some("rust"))?;
                    doc.edit("editor.tab_size", SettingValue::Integer(0))
                })
                .is_err()
        );
        assert_eq!(session.document().unwrap().source(), before);
    }

    #[test]
    fn settings_session_grapheme_input_is_bounded_and_paste_rejection_is_atomic() {
        let mut input = TextInput::new("a\u{301}👩‍💻🇯🇵".into()).unwrap();
        input.edit(SettingsInput::Left).unwrap();
        assert_eq!(&input.text[input.cursor..], "🇯🇵");
        input.edit(SettingsInput::Backspace).unwrap();
        assert_eq!(input.text, "a\u{301}🇯🇵");
        input.edit(SettingsInput::Home).unwrap();
        input.edit(SettingsInput::Delete).unwrap();
        assert_eq!(input.text, "🇯🇵");
        input.edit(SettingsInput::Right).unwrap();
        assert_eq!(input.cursor, input.text.len());
        let before = (input.text.clone(), input.cursor);
        assert!(input.insert(&"x".repeat(SETTINGS_INPUT_LIMIT)).is_err());
        assert_eq!((input.text.clone(), input.cursor), before);
        let mut input = TextInput::new("a👩".into()).unwrap();
        input.edit(SettingsInput::Home).unwrap();
        input.edit(SettingsInput::Right).unwrap();
        input.insert("\u{301}").unwrap();
        assert_eq!(&input.text[..input.cursor], "a\u{301}");
        let mut full = TextInput::new("x".repeat(SETTINGS_INPUT_LIMIT)).unwrap();
        assert!(full.insert("é").is_err());
        assert_eq!(full.text.len(), SETTINGS_INPUT_LIMIT);
    }

    #[test]
    fn settings_session_unknown_or_failed_save_cannot_claim_success() {
        let (mut session, requests) = fixture(None);
        session.edit_document(|doc| doc.edit("editor.tab_size", SettingValue::Integer(6))).unwrap();
        let first = requests.lock().unwrap()[0].clone();
        saved(&mut session, &(first.0 + 42, None, first.2.clone()));
        assert!(session.is_saving());
        assert!(session.take_applied().is_none());
        session
            .events
            .push(SettingsWorkerEvent::Error { request: Some(first.0), message: "permission denied".into() }, &|| {});
        session.worker_ready();
        assert!(session.is_dirty());
        assert!(!session.is_saving());
        assert!(session.status().contains("permission denied"));
        assert!(session.take_applied().is_none());
        session.input(SettingsInput::Save).unwrap();
        assert_eq!(requests.lock().unwrap()[1].1, None, "retry keeps original expected missing-file source");
        session.close();
        let retry = requests.lock().unwrap()[1].clone();
        saved(&mut session, &retry);
        assert!(session.take_applied().is_some(), "hidden screen still handles addressed events");
        assert!(session.status().contains("permission denied"), "errors require explicit dismissal");
    }

    #[test]
    fn settings_session_event_bursts_are_lossless_and_close_releases_backpressure() {
        let queue = Arc::new(EventQueue::default());
        let (wake_tx, wake_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let producer_queue = queue.clone();
        let producer = thread::spawn(move || {
            for request in 0..EVENT_LIMIT as u64 + 3 {
                producer_queue.push(
                    SettingsWorkerEvent::Saved { request, source: format!("# event {request}\n") },
                    &|| {
                        wake_tx.send(()).unwrap();
                    },
                );
            }
            finished_tx.send(()).unwrap();
        });
        let mut received = Vec::new();
        while received.len() < EVENT_LIMIT + 3 {
            wake_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            received.extend(queue.drain().into_iter().map(|event| match event {
                PreparedSettingsEvent::Saved { request, .. } => request,
                _ => panic!("unexpected event"),
            }));
        }
        finished_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        producer.join().unwrap();
        assert_eq!(received, (0..EVENT_LIMIT as u64 + 3).collect::<Vec<_>>());
        for request in 0..EVENT_LIMIT as u64 {
            queue.push(SettingsWorkerEvent::Saved { request, source: String::new() }, &|| {});
        }
        let (entered_tx, entered_rx) = mpsc::channel();
        let (exited_tx, exited_rx) = mpsc::channel();
        let producer_queue = queue.clone();
        let producer = thread::spawn(move || {
            entered_tx.send(()).unwrap();
            producer_queue.push(SettingsWorkerEvent::Error { request: None, message: "full".into() }, &|| {});
            exited_tx.send(()).unwrap();
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(exited_rx.try_recv().is_err());
        queue.close();
        exited_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        producer.join().unwrap();
        assert!(queue.drain().is_empty());
    }

    #[test]
    fn settings_session_real_worker_load_save_and_explicit_shutdown() {
        // Disposable, isolated settings only. The production UI paths perform
        // no I/O; this test checks the actual worker and event/wake integration.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let folder = std::env::temp_dir().join(format!(
            "kwil-settings-session-{}-{}-{unique}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir(&folder).unwrap();
        let path = folder.join("settings.toml");
        let (tx, rx) = mpsc::channel();
        let mut session = SettingsSession::start(
            path.clone(),
            Arc::new(move || {
                let _ = tx.send(());
            }),
        )
        .unwrap();
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        session.worker_ready();
        assert_eq!(session.take_applied().unwrap().source, None);
        session.open();
        session.edit_document(|doc| doc.edit("editor.tab_size", SettingValue::Integer(6))).unwrap();
        assert!(session.take_applied().is_none());
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        session.worker_ready();
        assert_eq!(session.take_applied().unwrap().settings.editor.tab_size, 6);
        let accepted_source = std::fs::read_to_string(&path).unwrap();
        let receipt = session.receipt;
        let drain_save = |session: &mut SettingsSession| {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while session.is_saving() {
                rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())).unwrap();
                session.worker_ready();
            }
        };
        session.edit_document(|doc| doc.map_extension("rs", Some("unavailable"))).unwrap();
        drain_save(&mut session);
        assert!(session.take_applied().is_none());
        assert_eq!(session.receipt, receipt);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            accepted_source,
            "session's production worker must reject mappings BEFORE persistence"
        );
        assert_eq!(session.disk_source.as_ref().unwrap().as_deref(), Some(accepted_source.as_str()));
        assert!(session.status().contains("unsupported language"));
        session.edit_document(|doc| doc.map_extension("rs", Some("python"))).unwrap();
        drain_save(&mut session);
        let corrected = session.take_applied().unwrap();
        assert_eq!(corrected.language_mappings.mappings()[0].language, Language::Python);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), corrected.source.unwrap());
        let shutdown = session.stop();
        thread::spawn(move || shutdown.shutdown()).join().unwrap().unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("tab_size = 6"));
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn settings_session_drop_never_joins_retirement_on_the_caller() {
        let (release_tx, release_rx) = mpsc::channel();
        let retirement = thread::spawn(move || {
            let _ = release_rx.recv();
        });
        let mut session = SettingsSession::state(PathBuf::new(), Arc::new(EventQueue::default()));
        session.retirement = Some(retirement);
        let (dropped_tx, dropped_rx) = mpsc::channel();
        let dropper = thread::spawn(move || {
            drop(session);
            dropped_tx.send(()).unwrap();
        });
        let dropped = dropped_rx.recv_timeout(Duration::from_millis(200));
        let _ = release_tx.send(());
        dropper.join().unwrap();
        assert!(dropped.is_ok(), "UI teardown waited for the retirement worker");
    }

    fn shutdown_fixture() -> (SettingsSession, mpsc::Receiver<(u64, Option<String>, String)>) {
        let (mut session, _) = fixture(Some("[editor]\ntab_size = 4\n"));
        let (tx, rx) = mpsc::sync_channel(1);
        session.client = Some(Client::FakeWorker(tx));
        (session, rx)
    }

    fn observe_event_wait(events: &EventQueue) -> mpsc::Receiver<()> {
        let (tx, rx) = mpsc::sync_channel(1);
        events.mailbox.lock().unwrap().waiting = Some(tx);
        rx
    }

    #[test]
    fn settings_shutdown_waits_for_slow_a_then_drains_latest_accepted_b_without_frontend_wake() {
        let (mut session, requests) = shutdown_fixture();
        let events = session.events.clone();
        let waiting = observe_event_wait(&events);
        session.edit_document(|doc| doc.edit("editor.tab_size", SettingValue::Integer(6))).unwrap();
        let a = requests.recv_timeout(Duration::from_secs(5)).unwrap();
        session.edit_document(|doc| doc.edit("editor.tab_size", SettingValue::Integer(7))).unwrap();
        session.edit_document(|doc| doc.edit("editor.tab_size", SettingValue::Integer(8))).unwrap();
        // A raw scalar draft after accepted B must never become an autosave.
        select(&mut session, "editor.tab_size");
        session.input(SettingsInput::Accept).unwrap();
        session.input(SettingsInput::Home).unwrap();
        session.input(SettingsInput::Delete).unwrap();
        session.input(SettingsInput::Text("9")).unwrap();
        let (retire_tx, retire_rx) = mpsc::sync_channel(1);
        let (retiring_tx, retiring_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        session.retire = Some(retire_tx);
        session.retirement = Some(thread::spawn(move || {
            retire_rx.recv().unwrap();
            retiring_tx.send(()).unwrap();
            let _ = release_rx.recv();
        }));
        let shutdown = session.stop();
        assert!(!session.is_open());
        assert!(!session.worker_ready(), "stopped UI must not consume the drain's events");
        let (done_tx, done_rx) = mpsc::channel();
        let drain = thread::spawn(move || done_tx.send(shutdown.shutdown()).unwrap());
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(done_rx.try_recv().is_err());
        assert!(requests.try_recv().is_err(), "B must await A's optimistic baseline");
        assert!(retiring_rx.try_recv().is_err(), "stop must retain the worker until saves drain");
        // A frontend event proxy is gone. The independent Condvar must wake
        // the drain even though this callback deliberately does nothing.
        events.push(SettingsWorkerEvent::Saved { request: a.0, source: a.2.clone() }, &|| {});
        let b = requests.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(b.1, Some(a.2.clone()), "B must compare against acknowledged A");
        assert!(b.2.contains("tab_size = 8"), "save only latest accepted B, not raw draft 9");
        assert_ne!(a.0, b.0);
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(done_rx.try_recv().is_err(), "exit must wait for B's acknowledgement too");
        events.push(
            SettingsWorkerEvent::Loaded {
                source: Some(a.2.clone()),
                document: Box::new(SettingsDocument::parse(&a.2).unwrap()),
            },
            &|| {},
        );
        events.push(SettingsWorkerEvent::Saved { request: b.0, source: b.2 }, &|| {});
        retiring_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(done_rx.try_recv().is_err(), "explicit shutdown must also wait for worker retirement");
        release_tx.send(()).unwrap();
        assert_eq!(done_rx.recv_timeout(Duration::from_secs(5)).unwrap(), Ok(()));
        drain.join().unwrap();
        assert!(requests.try_recv().is_err());
        assert!(events.mailbox.lock().unwrap().closed);
        // Late callbacks after retirement cannot block or resurrect events.
        for request in 0..EVENT_LIMIT as u64 + 1 {
            events.push(SettingsWorkerEvent::Saved { request, source: String::new() }, &|| panic!("closed wake"));
        }
        assert!(events.drain().is_empty());
        assert_eq!(session.stop().shutdown(), Ok(()), "repeated stop is harmless");
    }

    #[test]
    fn settings_shutdown_reports_save_failure_and_never_saves_unaccepted_draft() {
        let (mut session, requests) = shutdown_fixture();
        select(&mut session, "editor.tab_size");
        session.input(SettingsInput::Accept).unwrap();
        session.input(SettingsInput::Text("0")).unwrap();
        assert_eq!(session.stop().shutdown(), Ok(()));
        assert!(requests.try_recv().is_err(), "unaccepted draft must not create a save");

        let (mut session, requests) = shutdown_fixture();
        session.edit_document(|doc| doc.edit("editor.tab_size", SettingValue::Integer(6))).unwrap();
        let a = requests.recv_timeout(Duration::from_secs(5)).unwrap();
        session.edit_document(|doc| doc.edit("editor.tab_size", SettingValue::Integer(8))).unwrap();
        let events = session.events.clone();
        let shutdown = session.stop();
        let (done_tx, done_rx) = mpsc::channel();
        let drain = thread::spawn(move || done_tx.send(shutdown.shutdown()).unwrap());
        events.push(SettingsWorkerEvent::Error { request: Some(a.0), message: "disk unavailable".into() }, &|| {});
        assert_eq!(done_rx.recv_timeout(Duration::from_secs(5)).unwrap(), Err("disk unavailable".into()));
        drain.join().unwrap();
        assert!(requests.try_recv().is_err(), "failed A must not enqueue B or claim Saved");
        assert!(events.mailbox.lock().unwrap().closed);
    }

    #[test]
    fn settings_shutdown_reports_external_conflict_and_bad_acknowledgement() {
        for bad_ack in [false, true] {
            let (mut session, requests) = shutdown_fixture();
            session.edit_document(|doc| doc.edit("editor.tab_size", SettingValue::Integer(6))).unwrap();
            let a = requests.recv_timeout(Duration::from_secs(5)).unwrap();
            session.edit_document(|doc| doc.edit("editor.tab_size", SettingValue::Integer(8))).unwrap();
            let events = session.events.clone();
            let shutdown = session.stop();
            if !bad_ack {
                events.push(
                    SettingsWorkerEvent::Loaded {
                        source: Some("[editor]\ntab_size = 9\n".into()),
                        document: Box::new(SettingsDocument::parse("[editor]\ntab_size = 9\n").unwrap()),
                    },
                    &|| {},
                );
            }
            events.push(
                SettingsWorkerEvent::Saved { request: a.0, source: if bad_ack { String::new() } else { a.2 } },
                &|| {},
            );
            let error = shutdown.shutdown().unwrap_err();
            assert!(error.contains(if bad_ack { "different contents" } else { "reload conflict" }), "{error}");
            assert!(requests.try_recv().is_err());
        }
    }

    #[test]
    fn settings_shutdown_producer_exit_or_closed_mailbox_releases_event_wait() {
        for closed in [false, true] {
            let (mut session, requests) = shutdown_fixture();
            session.edit_document(|doc| doc.edit("editor.tab_size", SettingValue::Integer(6))).unwrap();
            requests.recv_timeout(Duration::from_secs(5)).unwrap();
            let events = session.events.clone();
            let producer = EventProducer(events.clone());
            let waiting = observe_event_wait(&events);
            let shutdown = session.stop();
            let (done_tx, done_rx) = mpsc::channel();
            let drain = thread::spawn(move || done_tx.send(shutdown.shutdown()).unwrap());
            waiting.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(done_rx.try_recv().is_err());
            if closed {
                events.close();
            } else {
                drop(producer);
            }
            let error = done_rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap_err();
            assert!(error.contains("before acknowledging"));
            drain.join().unwrap();
        }
    }

    #[test]
    fn settings_shutdown_implicit_drop_with_pending_saves_does_not_drain_or_join() {
        for handoff in [false, true] {
            let (mut session, requests) = shutdown_fixture();
            session.edit_document(|doc| doc.edit("editor.tab_size", SettingValue::Integer(6))).unwrap();
            requests.recv_timeout(Duration::from_secs(5)).unwrap();
            session.edit_document(|doc| doc.edit("editor.tab_size", SettingValue::Integer(8))).unwrap();
            let events = session.events.clone();
            let (release_tx, release_rx) = mpsc::channel();
            session.retirement = Some(thread::spawn(move || {
                let _ = release_rx.recv();
            }));
            let (done_tx, done_rx) = mpsc::channel();
            let dropper = thread::spawn(move || {
                if handoff {
                    drop(session.stop());
                } else {
                    drop(session);
                }
                done_tx.send(()).unwrap();
            });
            let dropped = done_rx.recv_timeout(Duration::from_secs(5));
            let _ = release_tx.send(());
            assert!(dropped.is_ok(), "implicit Drop waited for acknowledgement or joined retirement");
            dropper.join().unwrap();
            assert!(events.mailbox.lock().unwrap().closed);
            assert!(requests.try_recv().is_err(), "implicit Drop must not enqueue a coalesced save");
        }
    }

    #[test]
    fn settings_scalar_edit_keeps_visible_row_and_category_until_explicit_accept_or_cancel() {
        let (mut session, requests) = fixture(Some("[editor]\ntab_size = 4\n"));
        let row = session.rows().iter().position(|row| row.spec.id == "editor.tab_size").unwrap();
        session.select(row);
        session.input(SettingsInput::Accept).unwrap();
        session.input(SettingsInput::Home).unwrap();
        session.input(SettingsInput::Delete).unwrap();
        session.input(SettingsInput::Text("6")).unwrap();
        session.select(usize::MAX);
        assert_eq!(session.selected_row().unwrap().spec.id, "editor.tab_size");
        assert_eq!(session.editing.as_ref().unwrap().id, "editor.tab_size");
        session.set_category(Some(Category::Appearance));
        assert_eq!(session.category(), None);
        session.cycle_category(false);
        assert_eq!(session.category(), None);
        assert_eq!(session.selected_row().unwrap().spec.id, "editor.tab_size");
        assert_eq!(session.input_text(), "6");
        assert!(session.is_editing());
        session.input(SettingsInput::Accept).unwrap();
        assert_eq!(session.document().unwrap().settings().editor.tab_size, 6);
        assert_eq!(requests.lock().unwrap().len(), 1);
        session.cycle_category(false);
        assert_eq!(session.category(), Some(Category::Editor));
        let row = session.rows().iter().position(|row| row.spec.id == "editor.tab_size").unwrap();
        session.select(row);
        session.input(SettingsInput::Accept).unwrap();
        assert!(session.is_editing());
        session.input(SettingsInput::Cancel).unwrap();
        session.set_category(None);
        session.select(usize::MAX);
        assert_ne!(session.selected_row().unwrap().spec.id, "editor.tab_size");
    }

    #[test]
    fn settings_file_creation_waits_for_ack_and_navigation_keeps_search_separate() {
        let (mut session, requests) = fixture(None);
        session.input(SettingsInput::Save).unwrap();
        assert!(!session.source_exists());
        assert!(session.take_applied().is_none());
        assert_eq!(requests.lock().unwrap().len(), 1, "missing settings file must be queued for creation");
        let request = requests.lock().unwrap()[0].clone();
        assert_eq!(request.1, None);
        saved(&mut session, &request);
        assert!(session.source_exists());
        assert!(matches!(session.take_applied().unwrap().reason, SettingsAppliedReason::Saved { .. }));
        session.cycle_category(false);
        assert_eq!(session.category(), Some(Category::Editor));
        session.cycle_category(true);
        assert_eq!(session.category(), None);
        session.input(SettingsInput::Search).unwrap();
        session.input(SettingsInput::Text("tabs layout")).unwrap();
        session.input(SettingsInput::Cancel).unwrap();
        assert_eq!(session.search_text(), "");
        assert!(!session.is_searching());
        session.select(usize::MAX);
        assert_eq!(session.selected(), session.rows().len() - 1);
    }

    #[test]
    fn settings_session_opens_before_load_and_remembers_search_category() {
        let mut session = SettingsSession::state(PathBuf::new(), Arc::new(EventQueue::default()));
        session.client = Some(Client::Fake(Arc::new(Mutex::new(Vec::new()))));
        session.open();
        assert!(session.is_open());
        assert!(session.rows().is_empty());
        assert!(session.take_applied().is_none());
        assert!(session.status().contains("Loading"));
        session.set_category(Some(Category::Editor));
        session.input(SettingsInput::Search).unwrap();
        session.input(SettingsInput::Text("indent width")).unwrap();
        loaded(&mut session, None);
        let applied = session.take_applied().unwrap();
        assert_eq!(applied.reason, SettingsAppliedReason::Loaded);
        assert_eq!(applied.source, None);
        assert!(!session.status().contains("saved"));
        assert_eq!(session.rows().len(), 1);
        assert_eq!(session.rows()[0].spec.id, "editor.tab_size");
        assert_eq!(session.search_text(), "indent width");
        session.input(SettingsInput::Cancel).unwrap();
        assert!(session.is_open());
        session.input(SettingsInput::Cancel).unwrap();
        assert!(!session.is_open());
    }
}
