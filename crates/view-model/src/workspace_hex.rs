//! The workspace owns transport, editor loans and recovery. Frontends only
//! borrow a bounded Session and return its Outcome; they never own snapshots.
use super::*;
use crate::hex_session::{
    self as hex, EditCompletion, EditRequest, OpenOutcome, Page, PageRequest, Representation, Source,
};

struct HexTab {
    session: hex::Session,
    opening: Option<u64>,
    last_page: Option<PageRequest>,
    text_preview: Option<Vec<u8>>,
}
impl HexTab {
    fn new(rows: usize) -> Self {
        Self { session: hex::Session::new(rows), opening: None, last_page: None, text_preview: None }
    }
}
pub(super) enum Loan {
    Journal { generation: u64, target: Representation },
    Reopen { generation: u64, target: Representation },
    Page(PageRequest),
    Edit { source: Source, operation: u64, input_epoch: u64 },
}
#[derive(Default)]
pub(super) struct HexFlow {
    tabs: HashMap<DocumentId, HexTab>,
    pub(super) loans: HashMap<DocumentId, Loan>,
    pub(super) seed: HashSet<DocumentId>,
    next_generation: u64,
    pub(super) prompt: bool,
    pub(super) disposing: bool,
}
impl HexFlow {
    pub(super) fn opening(&self) -> bool {
        self.tabs.values().any(|tab| tab.opening.is_some())
    }
    pub(super) fn busy(&self, id: DocumentId) -> bool {
        self.loans.contains_key(&id) || self.tabs.get(&id).is_some_and(|tab| tab.opening.is_some())
    }
    pub(super) fn editor_away(&self, id: DocumentId) -> bool {
        matches!(self.loans.get(&id), Some(Loan::Reopen { .. } | Loan::Page(_) | Loan::Edit { .. }))
    }
    fn generation(&mut self) -> io::Result<u64> {
        self.next_generation =
            self.next_generation.checked_add(1).ok_or_else(|| io::Error::other("Raw reopen identity exhausted"))?;
        Ok(self.next_generation)
    }
}

impl Workspace {
    /// True through read-only preview, loans, save and cached close/reopen.
    /// Hosts must consume ALL body input before ordinary text/search/format.
    pub fn hex_owns_document(&self, id: DocumentId) -> bool {
        self.hex.tabs.contains_key(&id)
            || self.hex.loans.contains_key(&id)
            || self
                .views
                .get(&id)
                .and_then(View::editor)
                .is_some_and(|e| e.document().representation() == Representation::RawBytes)
    }
    pub fn hex_active(&self) -> bool {
        self.hex_owns_document(self.active_id())
    }
    pub fn hex_session(&self) -> Option<&hex::Session> {
        self.hex.tabs.get(&self.active_id()).map(|tab| &tab.session)
    }
    /// Borrow only for the IO-free hex_ui adapter. Always pass its Outcome to
    /// submit_hex_outcome, even if the worker is busy. It rejects pending edits
    /// on refusal, restoring the exact Editor before returning an error.
    pub fn hex_session_mut(&mut self) -> Option<&mut hex::Session> {
        let id = self.active_id();
        if self.hex.disposing
            || matches!(self.hex.loans.get(&id), Some(Loan::Journal { .. } | Loan::Reopen { .. } | Loan::Edit { .. }))
        {
            return None;
        }
        self.hex.tabs.get_mut(&id).map(|tab| &mut tab.session)
    }
    pub fn hex_source(&self) -> Option<Source> {
        let id = self.active_id();
        self.views
            .get(&id)
            .and_then(View::editor)
            .map(|e| e.document().hex_source(id.0))
            .or_else(|| self.hex.tabs.get(&id).and_then(|tab| tab.session.source()))
    }
    pub fn cancel_hex_input(&mut self) {
        self.cancel_hex_input_for(self.active_id());
    }
    pub(super) fn cancel_hex_input_for(&mut self, id: DocumentId) {
        if let Some(tab) = self.hex.tabs.get_mut(&id) {
            tab.session.cancel_input();
        }
    }
    pub fn set_hex_rows(&mut self, rows: usize) -> Damage {
        let id = self.active_id();
        if let Some(view) = self.views.get_mut(&id) {
            view.rows = rows;
        }
        let damage = self.hex.tabs.get_mut(&id).map_or_else(Damage::default, |tab| tab.session.set_rows(rows));
        self.pump_hex();
        damage
    }
    /// Shared scroll, mouse, key, paste and undo/redo entry point. Captured
    /// source/input_epoch come from the Session at the input's origin.
    pub fn hex_input(&mut self, source: Source, input_epoch: u64, input: hex::Input<'_>) -> io::Result<Damage> {
        let id = self.active_id();
        if self.hex.disposing
            || matches!(self.hex.loans.get(&id), Some(Loan::Journal { .. } | Loan::Reopen { .. } | Loan::Edit { .. }))
        {
            return Err(busy());
        }
        let tab = self.hex.tabs.get_mut(&id).ok_or_else(|| io::Error::other("Hex view is closed"))?;
        let outcome = tab.session.input(source, input_epoch, input)?;
        self.submit_hex_outcome(outcome)
    }
    /// For frontend hex_ui adapters which already called Session::input.
    pub fn submit_hex_outcome(&mut self, outcome: hex::Outcome) -> io::Result<Damage> {
        if let Some(request) = outcome.edit {
            self.submit_hex_edit(request)?;
        }
        self.pump_hex();
        Ok(outcome.damage)
    }
    pub fn submit_hex_edit(&mut self, request: EditRequest) -> io::Result<()> {
        let id = DocumentId(request.source.document);
        let result = (|| {
            if id != self.active_id() {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "Hex input tab changed"));
            }
            self.admit_hex(id, true)?;
            if self.hex.tabs.get(&id).and_then(|t| t.session.source()) != Some(request.source) {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "Hex source changed"));
            }
            let editor = self.views[&id].editor().unwrap();
            editor.admit_worker_loan(request.source.revision)?;
            if editor.document().hex_source(id.0) != request.source {
                return Err(io::Error::other("Hex source changed"));
            }
            self.sync_all();
            let loan =
                Loan::Edit { source: request.source, operation: request.operation, input_epoch: request.input_epoch };
            let editor = Box::new(self.views.get_mut(&id).unwrap().editor.take().unwrap());
            // Keep one small request for reject_edit on queue refusal, never a
            // second editor/snapshot. The worker receives the original editor.
            let saved = request.clone();
            match self.files.worker.as_ref().unwrap().try_submit(DocumentRequest::HexEdit {
                document: id,
                editor,
                request: saved,
            }) {
                Ok(()) => {
                    self.hex.loans.insert(id, loan);
                    Ok(())
                }
                Err(error) => {
                    let (request, kind) = refused(error);
                    let DocumentRequest::HexEdit { editor, .. } = *request else { unreachable!() };
                    self.views.get_mut(&id).unwrap().editor = Some(*editor);
                    Err(io::Error::new(kind, "Hex edit was not admitted; editor retained"))
                }
            }
        })();
        if result.is_err()
            && let Some(tab) = self.hex.tabs.get_mut(&id)
        {
            tab.session.reject_edit(&request);
        }
        result
    }
    /// Submit one latest viewport page per tab on the existing bounded lane.
    /// False means already cached/pending or backpressure; CapacityAvailable
    /// and every result drain retry the latest page without accumulating work.
    pub fn submit_hex_page(&mut self) -> io::Result<bool> {
        self.submit_hex_page_for(self.active_id())
    }
    fn submit_hex_page_for(&mut self, id: DocumentId) -> io::Result<bool> {
        let Some(tab) = self.hex.tabs.get(&id) else { return Ok(false) };
        let Some(request) = tab.session.page_request() else { return Ok(false) };
        // Recovery installs inactive/unlaid-out views with no visible rows.
        // A zero-row read cannot paint anything and must not hide the restored
        // Editor in a pointless worker loan before the frontend sets its size.
        if request.rows == 0 {
            return Ok(false);
        }
        if tab.last_page == Some(request) || self.document_busy(id) {
            return Ok(false);
        }
        self.admit_hex(id, false)?;
        let editor = self.views[&id].editor().unwrap();
        if editor.document().hex_source(id.0) != request.source {
            return Err(io::Error::other("Hex page source changed"));
        }
        self.sync_all();
        let editor = Box::new(self.views.get_mut(&id).unwrap().editor.take().unwrap());
        match self.files.worker.as_ref().unwrap().try_submit(DocumentRequest::HexPage { document: id, editor, request })
        {
            Ok(()) => {
                self.hex.loans.insert(id, Loan::Page(request));
                Ok(true)
            }
            Err(error) => {
                let (request, kind) = refused(error);
                let DocumentRequest::HexPage { editor, .. } = *request else { unreachable!() };
                self.views.get_mut(&id).unwrap().editor = Some(*editor);
                Err(io::Error::new(kind, "Hex page was not admitted; editor retained"))
            }
        }
    }
    fn admit_hex(&mut self, id: DocumentId, editing: bool) -> io::Result<()> {
        if self.hex.disposing
            || self.files.preparing
            || !self.files.storage_ready
            || self.files.deferred_recovery.is_some()
            || self.files.worker.is_none()
            || self.document_busy(id)
            || self.files.saves.contains_key(&id)
            || self.close_pending()
            || self.files.prompt.is_some()
        {
            return Err(busy());
        }
        if editing && (!self.files.observed.contains_key(&id) || self.files.journal_failed.contains_key(&id)) {
            return Err(io::Error::other("Hex edits require an admitted recovery observer"));
        }
        if self.views.get(&id).and_then(View::editor).is_none() {
            return Err(busy());
        }
        self.pump_files();
        if !self.files.queue.is_empty() {
            return Err(busy());
        }
        Ok(())
    }
    /// Open the selected document's physical transport. A dirty decoded source
    /// returns NeedsRawReopen synchronously and preserves every editor field.
    /// Ready means raw mode is accepted (possibly still preparing read-only).
    pub fn open_hex(&mut self, rows: usize) -> io::Result<OpenOutcome> {
        if self.hex.disposing {
            return Err(busy());
        }
        let id = self.active_id();
        let editor = self.views[&id].editor().ok_or_else(busy)?;
        let source = editor.document().hex_source(id.0);
        if source.representation == Representation::DecodedText && editor.document().is_dirty() {
            return Ok(OpenOutcome::NeedsRawReopen);
        }
        if source.representation == Representation::RawBytes {
            let tab = self.hex.tabs.entry(id).or_insert_with(|| HexTab::new(rows));
            tab.session.bind(source);
            tab.session.set_rows(rows);
            self.views.get_mut(&id).unwrap().rows = tab.session.view().rows();
            self.pump_hex();
            return Ok(OpenOutcome::Ready);
        }
        editor.raw_reopen_ready()?;
        self.admit_hex(id, false)?;
        if self.files.queue.len() >= 256 {
            return Err(busy());
        }
        let generation = self.hex.generation()?;
        // Admission permanently retires captured text input even if transport
        // reopening later fails and restores the same editor.
        self.views.get_mut(&id).unwrap().input_incarnation = std::sync::Arc::new(());
        let tab = self.hex.tabs.entry(id).or_insert_with(|| HexTab::new(rows));
        tab.session.set_rows(rows);
        self.views.get_mut(&id).unwrap().rows = tab.session.view().rows();
        self.cancel_hex_input_for(id);
        self.input_epoch = self.input_epoch.wrapping_add(1);
        if let Some((_, observed)) = self.files.observed.get(&id) {
            self.hex.loans.insert(id, Loan::Journal { generation, target: Representation::RawBytes });
            self.files.queue.push_back(DocumentRequest::FinishJournal { document: id, generation: *observed });
            self.pump_files();
        } else {
            self.start_transport_reopen(id, generation, Representation::RawBytes)?;
        }
        Ok(OpenOutcome::Ready)
    }
    /// Toggle only by reopening physical transport on the document worker.
    /// Dirty decoded OR raw buffers return the typed refusal without changing
    /// the Session, observer, document, cursor or undo chain.
    pub fn toggle_hex(&mut self, rows: usize) -> io::Result<OpenOutcome> {
        let id = self.active_id();
        if !self.hex_active() {
            return self.open_hex(rows);
        }
        let editor = self.views[&id].editor().ok_or_else(busy)?;
        if editor.document().is_dirty() {
            return Ok(OpenOutcome::NeedsRawReopen);
        }
        editor.raw_reopen_ready()?;
        self.admit_hex(id, false)?;
        let tab = self.hex.tabs.get_mut(&id).unwrap();
        tab.session.set_rows(rows);
        self.views.get_mut(&id).unwrap().rows = tab.session.view().rows();
        let generation = self.hex.generation()?;
        self.cancel_hex_input_for(id);
        self.input_epoch = self.input_epoch.wrapping_add(1);
        if let Some((_, observed)) = self.files.observed.get(&id) {
            self.hex.loans.insert(id, Loan::Journal { generation, target: Representation::DecodedText });
            self.files.queue.push_back(DocumentRequest::FinishJournal { document: id, generation: *observed });
            self.pump_files();
        } else {
            self.start_transport_reopen(id, generation, Representation::DecodedText)?;
        }
        Ok(OpenOutcome::Ready)
    }
    /// Explicit OpenAsHex preserves native paths, aliases and save reservations.
    /// Existing tabs use the same typed clean/dirty transition as open_hex.
    pub fn open_hex_path(&mut self, path: PathBuf, rows: usize) -> io::Result<(DocumentId, OpenOutcome)> {
        if self.hex.disposing || self.files.preparing || !self.files.storage_ready || self.files.worker.is_none() {
            return Err(busy());
        }
        let path = std::path::absolute(path)?;
        let existing = self
            .session
            .tabs()
            .iter()
            .find(|tab| tab.path.as_ref().is_some_and(|p| kwil_core_workspace::paths_may_alias(p, &path)))
            .map(|tab| tab.id)
            .or_else(|| {
                self.files
                    .saves
                    .iter()
                    .find(|(_, save)| {
                        save.path.as_ref().is_some_and(|p| kwil_core_workspace::paths_may_alias(p, &path))
                    })
                    .map(|(id, _)| *id)
            });
        if let Some(id) = existing {
            self.activate(id);
            let outcome = self.open_hex(rows)?;
            return Ok((id, outcome));
        }
        if self.files.queue.len() >= 256 {
            return Err(busy());
        }
        let generation = self.hex.generation()?;
        self.cancel_file_prompt();
        self.sync_active();
        let previous = self.session.active();
        let id = self.session.open(Some(path.clone()))?;
        if previous != self.session.active() {
            self.input_epoch = self.input_epoch.wrapping_add(1);
        }
        if let Some(previous) = previous {
            self.cancel_hex_input_for(previous);
        }
        let mut view = View::new(None);
        view.editor = None;
        view.path = Some(path.clone());
        view.rows = rows.min(crate::hex::MAX_ROWS);
        view.set_notice("Opening physical bytes...".into());
        self.views.insert(id, view);
        let mut tab = HexTab::new(rows);
        tab.opening = Some(generation);
        self.hex.tabs.insert(id, tab);
        self.refresh_save_destinations();
        self.files.queue.push_back(DocumentRequest::OpenRaw {
            document: id,
            revision: 0,
            path,
            rows: rows.min(crate::hex::MAX_ROWS),
            generation,
        });
        self.pump_files();
        self.checkpoint_session();
        Ok((id, OpenOutcome::Ready))
    }
    fn start_transport_reopen(&mut self, id: DocumentId, generation: u64, target: Representation) -> io::Result<()> {
        let rows = self.hex.tabs[&id].session.view().rows();
        let editor = Box::new(self.views.get_mut(&id).unwrap().editor.take().unwrap());
        let request = if target == Representation::RawBytes {
            DocumentRequest::ReopenRaw { document: id, generation, editor, rows }
        } else {
            DocumentRequest::ReopenText { document: id, generation, editor }
        };
        match self.files.worker.as_ref().unwrap().try_submit(request) {
            Ok(()) => {
                self.hex.loans.insert(id, Loan::Reopen { generation, target });
                Ok(())
            }
            Err(error) => {
                let (request, kind) = refused(error);
                let (DocumentRequest::ReopenRaw { editor, .. } | DocumentRequest::ReopenText { editor, .. }) = *request
                else {
                    unreachable!()
                };
                self.views.get_mut(&id).unwrap().editor = Some(*editor);
                self.hex.loans.remove(&id);
                if target == Representation::RawBytes {
                    self.hex.tabs.remove(&id);
                }
                self.hex.seed.insert(id);
                self.install_journal_observers();
                Err(io::Error::new(kind, "Raw reopen was not admitted; editor retained"))
            }
        }
    }
    pub(super) fn hex_journal_closed(&mut self, id: DocumentId, result: &io::Result<()>) {
        let Some(Loan::Journal { generation, target }) = self.hex.loans.get(&id) else { return };
        let generation = *generation;
        let target = *target;
        if result.is_err() {
            self.hex.loans.remove(&id);
            if target == Representation::RawBytes {
                self.hex.tabs.remove(&id);
            }
            return;
        }
        self.files.observed.remove(&id);
        self.files.seeded.remove(&id);
        self.files.durable.remove(&id);
        self.files.journal_failed.remove(&id);
        if let Err(error) = self.start_transport_reopen(id, generation, target) {
            self.views.get_mut(&id).unwrap().set_notice(error.to_string());
        }
    }
    pub(super) fn hex_preview(&mut self, id: DocumentId, revision: u64, mut page: Page) -> Damage {
        let Some(tab) = self.hex.tabs.get_mut(&id) else { return Damage::default() };
        let generation = match self.hex.loans.get(&id) {
            Some(Loan::Reopen { generation, target: Representation::RawBytes }) => Some(*generation),
            _ => tab.opening,
        };
        if generation != Some(page.request.generation)
            || page.request.source.document != id.0
            || page.request.source.representation != Representation::RawBytes
            || self.session.tabs().iter().find(|t| t.id == id).is_none_or(|t| t.revision != revision)
        {
            return Damage::default();
        }
        tab.session.bind(page.request.source);
        // Worker transport generations and Session viewport generations have
        // different owners. The checked transport's first page seeds exactly
        // the current viewport, never an old scroll/resize generation.
        let request = tab.session.page_request().unwrap();
        if page.request.top != request.top || page.request.rows != request.rows {
            return Damage::default();
        }
        page.request = request;
        let damage = tab.session.accept_page_with_damage(page).unwrap_or_default();
        tab.last_page = Some(request);
        if self.active_id() == id { damage } else { Damage::default() }
    }
    pub(super) fn hex_indexed(&mut self, id: DocumentId) {
        let Some(tab) = self.hex.tabs.get_mut(&id) else { return };
        tab.opening = None;
        if let Some(editor) = self.views.get(&id).and_then(View::editor) {
            tab.session.bind(editor.document().hex_source(id.0));
            self.hex.seed.insert(id);
        }
    }
    pub(super) fn hex_reopened(
        &mut self,
        id: DocumentId,
        generation: u64,
        editor: Box<Editor>,
        result: io::Result<OpenOutcome>,
    ) -> Damage {
        let Some(Loan::Reopen { generation: current, target }) = self.hex.loans.get(&id) else {
            panic!("worker must return the admitted transport editor")
        };
        assert_eq!(*current, generation);
        let target = *target;
        let mut editor = editor;
        if let Some(settings) = &self.editor_settings {
            editor.settings = settings.clone();
        }
        let source = editor.document().hex_source(id.0);
        let view = self.views.get_mut(&id).expect("raw loan target cannot close");
        assert!(view.editor.is_none());
        // Restore ownership BEFORE resolving UI state, including every refusal.
        view.file = editor.document().original.clone();
        view.editor = Some(*editor);
        self.hex.loans.remove(&id);
        self.hex.seed.insert(id);
        self.input_epoch = self.input_epoch.wrapping_add(1);
        match result {
            Ok(OpenOutcome::Ready) if source.representation == target => {
                if target == Representation::RawBytes {
                    self.hex.tabs.get_mut(&id).unwrap().session.bind(source);
                } else {
                    let tab = self.hex.tabs.remove(&id).expect("transport reopen retains its viewport");
                    let rows = tab.session.view().rows();
                    let preview = tab.text_preview.unwrap_or_default();
                    // Build the first text screen from worker-owned bounded
                    // bytes while its Editor remains locally held. set_rows
                    // cannot enter View's mapped/snapshot reload branch.
                    let mut editor = view.editor.take().unwrap();
                    let mut text_view = View::new(None);
                    text_view.editor = None;
                    text_view.path = editor.document().path().map(PathBuf::from);
                    text_view.preview = Some(preview);
                    // The hex Session owns layout while it owns the body;
                    // View can still carry the zero rows of its open placeholder.
                    text_view.set_rows(rows);
                    text_view.file = editor.document().original.clone();
                    editor.set_viewport(0, rows);
                    text_view.editor = Some(editor);
                    *view = text_view;
                }
                view.set_notice(String::new());
            }
            result => {
                if target == Representation::RawBytes {
                    self.hex.tabs.remove(&id);
                } else if let Some(tab) = self.hex.tabs.get_mut(&id) {
                    tab.session.bind(source);
                    tab.text_preview = None;
                }
                view.set_notice(match result {
                    Ok(_) => "NeedsRawReopen: decoded changes preserved".into(),
                    Err(error) => format!("Raw reopen failed: {error}"),
                });
            }
        }
        self.checkpoint_session();
        if self.active_id() == id { hex::body_damage() } else { Damage::default() }
    }
    pub(super) fn hex_text_preview_pending(&self, id: DocumentId) -> bool {
        matches!(self.hex.loans.get(&id), Some(Loan::Reopen { target: Representation::DecodedText, .. }))
    }
    pub(super) fn hex_text_preview(&mut self, id: DocumentId, revision: u64, text: Vec<u8>) {
        if text.len() <= 64 << 10
            && self.hex_text_preview_pending(id)
            && self.session.tabs().iter().any(|tab| tab.id == id && tab.revision == revision)
            && let Some(tab) = self.hex.tabs.get_mut(&id)
        {
            tab.text_preview = Some(text);
        }
    }
    pub(super) fn hex_page_result(
        &mut self,
        id: DocumentId,
        editor: Box<Editor>,
        request: PageRequest,
        result: io::Result<Page>,
    ) -> Damage {
        assert!(
            matches!(self.hex.loans.get(&id), Some(Loan::Page(current)) if *current == request),
            "worker must return the admitted page editor"
        );
        let mut editor = editor;
        if let Some(settings) = &self.editor_settings {
            editor.settings = settings.clone();
        }
        let source = editor.document().hex_source(id.0);
        let view = self.views.get_mut(&id).expect("page target cannot close");
        assert!(view.editor.is_none());
        view.editor = Some(*editor);
        self.hex.loans.remove(&id);
        let tab = self.hex.tabs.get_mut(&id).unwrap();
        tab.session.bind(source);
        tab.last_page = Some(request);
        let damage = match result {
            Ok(page) => tab.session.accept_page_with_damage(page).unwrap_or_default(),
            Err(error) => {
                view.set_notice(format!("Hex page failed: {error}"));
                Damage { status: true, ..Default::default() }
            }
        };
        if self.active_id() == id { damage } else { Damage::default() }
    }
    pub(super) fn hex_edit_result(
        &mut self,
        id: DocumentId,
        editor: Box<Editor>,
        completion: EditCompletion,
    ) -> Damage {
        assert!(
            matches!(self.hex.loans.get(&id), Some(Loan::Edit { source, operation, input_epoch }) if *source == completion.before && *operation == completion.operation && *input_epoch == completion.input_epoch),
            "worker must return the admitted edit editor"
        );
        let mut editor = editor;
        if let Some(settings) = &self.editor_settings {
            editor.settings = settings.clone();
        }
        let source = editor.document().hex_source(id.0);
        let sequence = editor.next_edit_sequence().saturating_sub(1);
        let changed = source != completion.before;
        let view = self.views.get_mut(&id).expect("edit target cannot close");
        assert!(view.editor.is_none());
        view.editor = Some(*editor);
        self.hex.loans.remove(&id);
        if changed {
            self.files.replacement_journals.insert(id, sequence);
        }
        let tab = self.hex.tabs.get_mut(&id).unwrap();
        let damage = match tab.session.complete_edit_with_damage(completion) {
            Ok(Some(damage)) => damage,
            Ok(None) => Damage::default(),
            Err(error) => {
                view.set_notice(format!("Hex edit failed: {error}"));
                hex::body_damage()
            }
        };
        tab.session.bind(source);
        self.checkpoint_session();
        if self.files.hot_exit {
            self.files.queue.push_back(DocumentRequest::FlushJournals);
        }
        if self.active_id() == id { damage } else { Damage::default() }
    }
    pub(super) fn bind_raw_sessions(&mut self) {
        if self.hex.disposing {
            return;
        }
        for (&id, view) in &self.views {
            let Some(editor) = view.editor() else { continue };
            let source = editor.document().hex_source(id.0);
            if source.representation == Representation::RawBytes {
                let tab = self.hex.tabs.entry(id).or_insert_with(|| {
                    let mut tab = HexTab::new(view.rows());
                    tab.session.bind(source);
                    if let Some(metadata) = self.session.tabs().iter().find(|tab| tab.id == id) {
                        restore_navigation(
                            &mut tab.session,
                            source,
                            metadata.cursor,
                            metadata.anchor,
                            metadata.first_visible_line,
                        );
                    }
                    tab
                });
                tab.session.bind(source);
                if !self.files.observed.contains_key(&id) {
                    self.hex.seed.insert(id);
                }
            }
        }
    }
    pub(super) fn pump_hex(&mut self) {
        if self.hex.disposing {
            return;
        }
        // At most one request per live tab and one bounded mailbox. Never put
        // Editor-bearing page/edit loans in the unbounded legacy file deque.
        let ids: Vec<_> = self.session.tabs().iter().map(|tab| tab.id).collect();
        for id in ids {
            let _ = self.submit_hex_page_for(id);
        }
    }
    pub(super) fn retain_hex_history(&mut self) {
        let retained = |id: &DocumentId| self.views.contains_key(id) || self.closed_views.contains_key(id);
        self.hex.tabs.retain(|id, _| retained(id));
        self.hex.seed.retain(retained);
    }
    pub(super) fn retire_closed_hex_editors(&mut self) {
        for (&id, view) in &mut self.closed_views {
            if self.session.closed().iter().any(|tab| tab.id == id) {
                continue;
            }
            if view.editor().is_some_and(|editor| editor.document().representation() == Representation::RawBytes)
                && let Some(editor) = view.editor.take()
            {
                self.files.queue.push_back(DocumentRequest::DiscardHex { document: id, editor: Box::new(editor) });
            }
        }
    }
    pub(super) fn hex_navigation(&self, id: DocumentId) -> Option<(u64, u64, u64)> {
        let session = &self.hex.tabs.get(&id)?.session;
        session.source()?;
        let view = session.view();
        let cursor = view.cursor();
        let selection = view.selection();
        let anchor = if cursor == selection.start { selection.end } else { selection.start };
        Some((cursor, anchor, view.top()))
    }
    /// Final exit hook, AFTER the host accepted exit. False means keep the
    /// event loop alive and drain DocumentResult/CapacityAvailable, then retry.
    /// True means all raw Editors/undo roots have moved to the existing lane;
    /// final Workspace drop can no longer destroy a last raw Editor on the UI.
    /// Cached hex rows remain paintable while the bounded queue drains. This
    /// does not approve exit or discard unsaved work: metadata and admitted
    /// recovery must be durable before ownership is transferred.
    pub fn prepare_hex_shutdown(&mut self) -> bool {
        if self.hex.disposing {
            self.pump_files();
            return !self.files.queue.iter().any(|request| matches!(request, DocumentRequest::DiscardHex { .. }));
        }
        let count = self
            .views
            .values()
            .chain(self.closed_views.values())
            .filter(|view| {
                view.editor().is_some_and(|editor| editor.document().representation() == Representation::RawBytes)
            })
            .count();
        if count == 0 && !self.hex.opening() && self.hex.loans.is_empty() {
            self.pump_files();
            return !self.files.queue.iter().any(|request| matches!(request, DocumentRequest::DiscardHex { .. }));
        }
        if self.search_replacement_pending()
            || self.file_saves_pending()
            || self.files.preparing
            || self.files.worker.is_none()
            || !self.files.storage_ready
            || self.files.deferred_recovery.is_some()
        {
            return false;
        }
        if !self.files.metadata_durable || self.files.persist_pending || self.files.persist_inflight {
            if !self.files.persist_pending && !self.files.persist_inflight {
                self.checkpoint_session();
            }
            return false;
        }
        if self.session.tabs().iter().any(|tab| self.recovery_pending(tab.id)) {
            if !self.files.queue.iter().any(|request| matches!(request, DocumentRequest::FlushJournals)) {
                self.files.queue.push_back(DocumentRequest::FlushJournals);
            }
            self.pump_files();
            return false;
        }
        self.pump_files();
        if self.files.queue.len().saturating_add(count) > 256 {
            return false;
        }
        self.hex.disposing = true;
        self.input_epoch = self.input_epoch.wrapping_add(1);
        for tab in self.hex.tabs.values_mut() {
            tab.session.cancel_input();
        }
        for (&id, view) in self.views.iter_mut().chain(self.closed_views.iter_mut()) {
            if view.editor().is_some_and(|editor| editor.document().representation() == Representation::RawBytes)
                && let Some(editor) = view.editor.take()
            {
                self.files.queue.push_back(DocumentRequest::DiscardHex { document: id, editor: Box::new(editor) });
            }
        }
        self.pump_files();
        !self.files.queue.iter().any(|request| matches!(request, DocumentRequest::DiscardHex { .. }))
    }
    /// Block hidden text mutations even for callers using Workspace directly.
    /// Save/close/chrome use addressed views and the established file protocol.
    pub fn editor_mut(&mut self) -> Option<&mut Editor> {
        let id = self.active_id();
        if self.hex.disposing || self.hex_owns_document(id) || self.document_busy(id) {
            return None;
        }
        self.views.get_mut(&id).and_then(View::editor_mut)
    }
    pub fn set_rows(&mut self, rows: usize) -> Damage {
        if self.hex_active() { self.set_hex_rows(rows) } else { self.deref_mut().set_rows(rows) }
    }
    pub fn scroll(&mut self, rows: i64) -> Damage {
        if !self.hex_active() {
            return self.deref_mut().scroll(rows);
        }
        let Some(session) = self.hex_session() else { return Damage::default() };
        let Some(source) = session.source() else { return Damage::default() };
        let epoch = session.input_epoch();
        self.hex_input(source, epoch, hex::Input::Scroll(rows)).unwrap_or_default()
    }
    pub fn scroll_to_start(&mut self) -> Damage {
        self.hex_scroll_edge(false)
    }
    pub fn scroll_to_end(&mut self) -> Damage {
        self.hex_scroll_edge(true)
    }
    fn hex_scroll_edge(&mut self, end: bool) -> Damage {
        if !self.hex_active() {
            return if end { self.deref_mut().scroll_to_end() } else { self.deref_mut().scroll_to_start() };
        }
        let Some(session) = self.hex_session() else { return Damage::default() };
        let Some(source) = session.source() else { return Damage::default() };
        self.hex_input(source, session.input_epoch(), hex::Input::ScrollTo(if end { u64::MAX } else { 0 }))
            .unwrap_or_default()
    }
    pub fn execute(&mut self, id: &str, args: &crate::CommandArgs, count: u32) -> io::Result<crate::CommandResult> {
        if self.hex.disposing || self.hex_active() {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "Use hex input while physical bytes own the body"));
        }
        self.deref_mut().execute(id, args, count)
    }
}
fn busy() -> io::Error {
    io::Error::new(io::ErrorKind::WouldBlock, "Document/recovery worker is busy; retry hex action")
}
fn restore_navigation(session: &mut hex::Session, source: Source, cursor: u64, anchor: u64, top: u64) {
    let rows = session.view().rows();
    session.set_rows(rows.max(1));
    for (offset, extend) in [(anchor.min(source.len), false), (cursor.min(source.len), true)] {
        let _ = session.input(source, session.input_epoch(), hex::Input::ScrollTo(offset / 16));
        let row = (offset / 16).saturating_sub(session.view().top()) as usize;
        let column = crate::hex::hex_column((offset % 16) as usize);
        let _ = session.input(source, session.input_epoch(), hex::Input::Click { row, column, extend });
    }
    session.set_rows(rows);
    let _ = session.input(source, session.input_epoch(), hex::Input::ScrollTo(top));
    session.cancel_input();
}
fn refused(error: std::sync::mpsc::TrySendError<Box<DocumentRequest>>) -> (Box<DocumentRequest>, io::ErrorKind) {
    match error {
        std::sync::mpsc::TrySendError::Full(request) => (request, io::ErrorKind::WouldBlock),
        std::sync::mpsc::TrySendError::Disconnected(request) => (request, io::ErrorKind::BrokenPipe),
    }
}

#[cfg(test)]
#[path = "workspace_hex_tests.rs"]
mod tests;
