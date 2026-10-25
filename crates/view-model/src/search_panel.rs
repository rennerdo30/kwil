//! Shared search interaction model. No matching, parsing, filesystem access or
//! snapshot byte reads happen here. SearchSession owns the worker protocol.
use crate::{SessionDocumentId, Workspace, search_session::*};
use kwil_core_commands::settings::SearchSettings;
use kwil_core_search::{FileEvent, FileSearch, Match};
use std::{ops::Range, path::PathBuf};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchMode {
    Find,
    Replace,
    Files,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchField {
    Query,
    Replacement,
    Root,
    Include,
    Exclude,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchDestination {
    pub epoch: u64,
    pub field: SearchField,
}
#[derive(Clone, Copy, Debug)]
pub enum SearchInput<'a> {
    Text(&'a str),
    Backspace,
    Delete,
    Left,
    Right,
    Home,
    End,
    SelectAll,
    Focus(SearchField),
    NextField,
    PreviousField,
    Cancel,
    Next,
    Previous,
    AddCursor,
    ReplaceNext,
    ReplaceAll,
    ToggleRegex,
    ToggleCase,
    ToggleWord,
    ToggleSelection,
    SelectRow(usize),
    RowNext,
    RowPrevious,
    PageNext,
    PagePrevious,
}
#[derive(Default, Debug)]
pub struct SearchDamage {
    pub panel: bool,
    pub status: bool,
    pub hit_rows: Vec<usize>,
    pub signal: bool,
    pub restore_editor: bool,
}
#[derive(Default)]
pub struct SearchOutcome {
    pub action: Option<SearchAction>,
    pub damage: SearchDamage,
}
impl SearchOutcome {
    fn panel() -> Self {
        Self { damage: SearchDamage { panel: true, status: true, ..Default::default() }, ..Default::default() }
    }
    pub fn restore_context(&self, keys: &mut kwil_core_commands::keymap::KeymapEngine) {
        if self.damage.restore_editor {
            keys.cancel_pending();
            keys.set_context(kwil_core_commands::Context::Editor);
        }
    }
}
#[derive(Clone, Default)]
struct TextInput {
    text: String,
    cursor: usize,
    selected: bool,
}
impl TextInput {
    fn set(&mut self, text: String) -> Result<(), String> {
        if text.len() > INPUT_BYTES || text.contains('\0') {
            return Err("Search input exceeds 64 KiB or contains NUL".into());
        }
        self.cursor = text.len();
        self.text = text;
        self.selected = false;
        Ok(())
    }
    fn edit(&mut self, input: SearchInput<'_>) -> Result<bool, String> {
        let old = self.text.clone();
        match input {
            SearchInput::SelectAll => {
                self.selected = true;
                self.cursor = self.text.len();
            }
            SearchInput::Text(text) => {
                if (if self.selected { 0 } else { self.text.len() }).saturating_add(text.len()) > INPUT_BYTES
                    || text.contains('\0')
                {
                    return Err("Search input exceeds 64 KiB or contains NUL".into());
                }
                if self.selected {
                    self.text.clear();
                    self.cursor = 0;
                    self.selected = false;
                }
                self.text.insert_str(self.cursor, text);
                self.cursor += text.len();
            }
            SearchInput::Backspace => {
                if self.selected {
                    self.text.clear();
                    self.cursor = 0;
                    self.selected = false;
                    return Ok(old != self.text);
                }
                let at = self.text[..self.cursor].grapheme_indices(true).next_back().map_or(0, |(at, _)| at);
                self.text.replace_range(at..self.cursor, "");
                self.cursor = at;
            }
            SearchInput::Delete => {
                if self.selected {
                    self.text.clear();
                    self.cursor = 0;
                    self.selected = false;
                    return Ok(old != self.text);
                }
                let end = self.cursor + self.text[self.cursor..].graphemes(true).next().map_or(0, str::len);
                self.text.replace_range(self.cursor..end, "");
            }
            SearchInput::Left => {
                if self.selected {
                    self.cursor = 0;
                    self.selected = false;
                    return Ok(false);
                }
                self.cursor = self.text[..self.cursor].grapheme_indices(true).next_back().map_or(0, |(at, _)| at)
            }
            SearchInput::Right => {
                if self.selected {
                    self.cursor = self.text.len();
                    self.selected = false;
                } else {
                    self.cursor += self.text[self.cursor..].graphemes(true).next().map_or(0, str::len);
                }
            }
            SearchInput::Home => {
                self.selected = false;
                self.cursor = 0;
            }
            SearchInput::End => {
                self.selected = false;
                self.cursor = self.text.len();
            }
            _ => {}
        }
        Ok(old != self.text)
    }
}
#[derive(Clone, Debug)]
pub struct SearchResultRow {
    pub label: String,
    pub is_hit: bool,
}
#[derive(Clone, Copy)]
enum Followup {
    Seed,
    Select(SelectionMode),
    ReplaceSelected,
    ReplaceRecipe,
}

/// Metadata captured before the owned recipe leaves the coherent dispatcher.
/// It acknowledges one admission/completion without retaining recipe bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplacementAcknowledgement {
    key: SearchKey,
    epoch: u64,
}

pub struct SearchPanel {
    open: bool,
    active: bool,
    mode: SearchMode,
    field: SearchField,
    epoch: u64,
    query: TextInput,
    replacement: TextInput,
    root_input: TextInput,
    include: TextInput,
    exclude: TextInput,
    root: Option<PathBuf>,
    result_root: Option<PathBuf>,
    root_edited: bool,
    settings: SearchSettings,
    settings_baseline: Option<SearchSettings>,
    identity: Option<(SessionDocumentId, u64)>,
    identity_epoch: u64,
    scope: Vec<Range<u64>>,
    request_key: Option<SearchKey>,
    paint_key: Option<SearchKey>,
    paint_hits: Vec<Match>,
    result_rows: Vec<SearchResultRow>,
    file_first: u64,
    files_seen: u64,
    selected: usize,
    signal: [u64; SIGNAL_BINS],
    count: u64,
    exact: bool,
    pending: bool,
    ready: bool,
    status: String,
    error: Option<String>,
    followup: Option<(SearchKey, Followup)>,
    retry: Option<Box<SearchRequest>>,
    retry_epoch: u64,
    retry_followup: Option<Followup>,
    refresh_needed: bool,
    pending_seed: Option<(SearchKey, SeedResult)>,
    seed_epoch: Option<u64>,
    seed_origin: Option<(Vec<(u64, u64)>, usize)>,
    replacement_epoch: Option<u64>,
    replacement_pending: Option<ReplacementAcknowledgement>,
    replacement_notice: Option<(u64, String)>,
}
impl Default for SearchPanel {
    fn default() -> Self {
        Self {
            open: false,
            active: false,
            mode: SearchMode::Find,
            field: SearchField::Query,
            epoch: 0,
            query: TextInput::default(),
            replacement: TextInput::default(),
            root_input: TextInput::default(),
            include: TextInput::default(),
            exclude: TextInput::default(),
            root: None,
            result_root: None,
            root_edited: false,
            settings: SearchSettings::default(),
            settings_baseline: None,
            identity: None,
            identity_epoch: 0,
            scope: Vec::new(),
            request_key: None,
            paint_key: None,
            paint_hits: Vec::new(),
            result_rows: Vec::new(),
            file_first: 0,
            files_seen: 0,
            selected: 0,
            signal: [0; SIGNAL_BINS],
            count: 0,
            exact: false,
            pending: false,
            ready: false,
            status: "Find".into(),
            error: None,
            followup: None,
            retry: None,
            retry_epoch: 0,
            retry_followup: None,
            refresh_needed: false,
            pending_seed: None,
            seed_epoch: None,
            seed_origin: None,
            replacement_epoch: None,
            replacement_pending: None,
            replacement_notice: None,
        }
    }
}
impl SearchPanel {
    pub fn is_open(&self) -> bool {
        self.open
    }
    pub fn is_pending(&self) -> bool {
        self.pending || self.replacement_pending.is_some()
    }
    pub fn can_replace(&self) -> bool {
        self.mode != SearchMode::Files
            && !self.query.text.is_empty()
            && !self.is_pending()
            && self.ready
            && self.count != 0
            && self.paint_key.is_some()
            && self.paint_key == self.request_key
    }
    pub fn count(&self) -> (u64, bool) {
        (self.count, self.exact)
    }
    pub fn enter_context(&self, keys: &mut kwil_core_commands::keymap::KeymapEngine) {
        if self.open {
            keys.cancel_pending();
            keys.set_context(kwil_core_commands::Context::Palette);
        }
    }
    pub fn mode(&self) -> SearchMode {
        self.mode
    }
    pub fn field(&self) -> SearchField {
        self.field
    }
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
    pub fn set_error(&mut self, error: String) {
        self.error = Some(error);
    }
    pub fn destination(&self) -> Option<SearchDestination> {
        self.open.then_some(SearchDestination { epoch: self.epoch, field: self.field })
    }
    pub fn options(&self) -> &SearchSettings {
        &self.settings
    }
    pub fn status(&self) -> &str {
        self.error.as_deref().unwrap_or(&self.status)
    }
    pub fn results(&self) -> &[SearchResultRow] {
        &self.result_rows
    }
    pub fn selected(&self) -> usize {
        self.selected
    }
    pub fn signal(&self) -> &[u64; SIGNAL_BINS] {
        &self.signal
    }
    pub fn text(&self, field: SearchField) -> (&str, usize) {
        let input = self.input_ref(field);
        (&input.text, input.cursor)
    }
    pub fn visible_fields(&self) -> &'static [SearchField] {
        match self.mode {
            SearchMode::Find => &[SearchField::Query],
            SearchMode::Replace => &[SearchField::Query, SearchField::Replacement],
            SearchMode::Files => &[SearchField::Query, SearchField::Root, SearchField::Include, SearchField::Exclude],
        }
    }
    fn input_ref(&self, field: SearchField) -> &TextInput {
        match field {
            SearchField::Query => &self.query,
            SearchField::Replacement => &self.replacement,
            SearchField::Root => &self.root_input,
            SearchField::Include => &self.include,
            SearchField::Exclude => &self.exclude,
        }
    }
    fn input_mut(&mut self, field: SearchField) -> &mut TextInput {
        match field {
            SearchField::Query => &mut self.query,
            SearchField::Replacement => &mut self.replacement,
            SearchField::Root => &mut self.root_input,
            SearchField::Include => &mut self.include,
            SearchField::Exclude => &mut self.exclude,
        }
    }
    fn advance_epoch(&mut self) {
        self.epoch = self.epoch.checked_add(1).expect("search input epoch exhausted");
    }
    pub fn set_root(&mut self, root: PathBuf) -> Result<(), String> {
        if !root.is_absolute() {
            return Err("Search root must be absolute".into());
        }
        if !self.root_edited {
            let changed = self.root.as_ref() != Some(&root);
            self.root_input.set(root.to_string_lossy().into_owned())?;
            self.root = Some(root);
            self.refresh_needed |= changed && self.mode == SearchMode::Files;
        }
        Ok(())
    }
    /// Deliver the already resolved native root without filesystem work here.
    /// A late root starts the retained folder query on the existing worker.
    pub fn root_ready(
        &mut self,
        session: &mut SearchSession,
        view: &Workspace,
        root: PathBuf,
    ) -> Result<SearchOutcome, String> {
        self.set_root(root)?;
        if self.active && self.mode == SearchMode::Files && self.refresh_needed {
            self.refresh(session, view, None, None)?;
            return Ok(SearchOutcome::panel());
        }
        Ok(SearchOutcome::default())
    }
    fn check_root_edit(&self, field: SearchField, input: SearchInput<'_>) -> Result<(), String> {
        if field == SearchField::Root
            && !self.root_edited
            && self.root.as_ref().is_some_and(|root| root.to_str().is_none())
            && !self.root_input.selected
            && matches!(input, SearchInput::Text(_) | SearchInput::Backspace | SearchInput::Delete)
        {
            return Err(
                "This native folder name cannot be edited as displayed text; select all to enter a new path".into()
            );
        }
        Ok(())
    }
    pub fn sync_settings(&mut self, settings: &SearchSettings) -> Result<(), String> {
        if self.settings_baseline.as_ref() == Some(settings) {
            return Ok(());
        }
        self.include.set(settings.include.join("\n"))?;
        self.exclude.set(settings.exclude.join("\n"))?;
        self.settings = settings.clone();
        self.settings_baseline = Some(settings.clone());
        self.refresh_needed = true;
        self.advance_epoch();
        Ok(())
    }
    pub fn settings_ready(
        &mut self,
        session: &mut SearchSession,
        view: &Workspace,
        settings: &SearchSettings,
    ) -> Result<SearchOutcome, String> {
        self.sync_settings(settings)?;
        if self.refresh_needed && self.active {
            self.capture_scope(view);
            self.refresh(session, view, None, None)?;
        } else if !self.active {
            self.refresh_needed = false;
        }
        Ok(SearchOutcome::panel())
    }
    fn active(view: &Workspace) -> Option<(SessionDocumentId, u64)> {
        view.editor().map(|editor| (view.active_id(), editor.document().revision()))
    }
    fn capture_scope(&mut self, view: &Workspace) {
        self.scope = view
            .editor()
            .map(|editor| {
                editor.cursors().iter().map(|cursor| cursor.selection()).filter(|range| !range.is_empty()).collect()
            })
            .unwrap_or_default();
    }
    pub fn open(&mut self, mode: SearchMode, settings: &SearchSettings) -> Result<SearchOutcome, String> {
        self.sync_settings(settings)?;
        self.open = true;
        self.active = true;
        self.mode = mode;
        self.field = SearchField::Query;
        self.error = None;
        self.followup = None;
        self.pending_seed = None;
        self.seed_epoch = None;
        self.seed_origin = None;
        self.advance_epoch();
        Ok(SearchOutcome::panel())
    }
    pub fn close(&mut self, session: &mut SearchSession, view: &Workspace) -> SearchOutcome {
        session.cancel();
        self.open = false;
        self.active = false;
        self.followup = None;
        self.pending_seed = None;
        self.seed_epoch = None;
        self.seed_origin = None;
        self.request_key = None;
        self.pending = false;
        self.advance_epoch();
        let rows = self.hit_rows(view);
        self.paint_key = None;
        self.paint_hits.clear();
        self.result_rows.clear();
        self.signal = [0; SIGNAL_BINS];
        self.refresh_needed = false;
        SearchOutcome {
            damage: SearchDamage { panel: true, status: true, hit_rows: rows, signal: true, restore_editor: true },
            ..Default::default()
        }
    }
    pub fn commit(
        &mut self,
        session: &mut SearchSession,
        view: &Workspace,
        destination: SearchDestination,
        text: &str,
    ) -> Result<SearchOutcome, String> {
        if self.destination() != Some(destination) {
            return Err("Search composition cancelled: its field or input changed".into());
        }
        self.input(session, view, SearchInput::Text(text))
    }
    pub fn command(
        &mut self,
        session: &mut SearchSession,
        view: &Workspace,
        id: &str,
        settings: &SearchSettings,
    ) -> Result<Option<SearchOutcome>, String> {
        let open = match id {
            "search.find" => Some(SearchMode::Find),
            "search.replace" => Some(SearchMode::Replace),
            "search.findInFiles" => Some(SearchMode::Files),
            _ => None,
        };
        if let Some(mode) = open {
            let mut out = self.open(mode, settings)?;
            self.identity = Self::active(view);
            self.identity_epoch = view.input_epoch();
            self.capture_scope(view);
            self.refresh_needed = true;
            self.refresh(session, view, None, None)?;
            out.damage.hit_rows = self.hit_rows(view);
            return Ok(Some(out));
        }
        let input = match id {
            "search.findNext" => SearchInput::Next,
            "search.findPrevious" => SearchInput::Previous,
            "edit.selectNextMatch" => SearchInput::AddCursor,
            "search.replaceNext" => SearchInput::ReplaceNext,
            "search.replaceAll" => SearchInput::ReplaceAll,
            "search.toggleRegex" => SearchInput::ToggleRegex,
            "search.toggleCase" => SearchInput::ToggleCase,
            "search.toggleWholeWord" => SearchInput::ToggleWord,
            "search.toggleSelection" => SearchInput::ToggleSelection,
            "search.setInclude" => SearchInput::Focus(SearchField::Include),
            "search.setExclude" => SearchInput::Focus(SearchField::Exclude),
            "search.replaceInFiles" => {
                self.open(SearchMode::Files, settings)?;
                self.error = Some(
                    "Folder replacement awaits the atomic folder executor; Find in Files remains available".into(),
                );
                return Ok(Some(SearchOutcome::panel()));
            }
            _ => return Ok(None),
        };
        self.sync_settings(settings)?;
        self.input(session, view, input).map(Some)
    }
    pub fn input(
        &mut self,
        session: &mut SearchSession,
        view: &Workspace,
        input: SearchInput<'_>,
    ) -> Result<SearchOutcome, String> {
        let result = self.handle_input(session, view, input);
        if let Err(error) = &result {
            self.error = Some(error.clone());
        }
        result
    }
    fn handle_input(
        &mut self,
        session: &mut SearchSession,
        view: &Workspace,
        input: SearchInput<'_>,
    ) -> Result<SearchOutcome, String> {
        if matches!(input, SearchInput::ReplaceNext | SearchInput::ReplaceAll) {
            if self.mode == SearchMode::Files {
                return Err("Folder replacement awaits the atomic folder executor".into());
            }
            if view.document_busy(view.active_id()) || self.replacement_pending.is_some() {
                return Err("Replacement is still running; wait for the document worker".into());
            }
            if self.query.text.is_empty() {
                return Err("Enter a search query before replacing matches".into());
            }
            if self.pending || self.paint_key != self.request_key {
                return Err("Search results are updating; wait before replacing matches".into());
            }
            if !self.ready || self.count == 0 {
                return Err("No valid search matches are ready for replacement".into());
            }
        }
        self.error = None;
        self.replacement_notice = None;
        self.advance_epoch();
        let old_paint = self.paint_key;
        let old_rows = self.hit_rows(view);
        let mut out = SearchOutcome::panel();
        match input {
            SearchInput::Cancel => return Ok(self.close(session, view)),
            SearchInput::Focus(field) => {
                if matches!(field, SearchField::Include | SearchField::Exclude | SearchField::Root) {
                    self.mode = SearchMode::Files;
                }
                self.field = field;
                self.open = true;
            }
            SearchInput::NextField | SearchInput::PreviousField => {
                let fields = self.visible_fields();
                let at = fields.iter().position(|field| *field == self.field).unwrap_or(0);
                let step = if matches!(input, SearchInput::PreviousField) { fields.len() - 1 } else { 1 };
                self.field = fields[(at + step) % fields.len()];
            }
            SearchInput::ToggleRegex => {
                self.settings.regex = !self.settings.regex;
                self.refresh_needed = true;
            }
            SearchInput::ToggleCase => {
                self.settings.case_sensitive = !self.settings.case_sensitive;
                self.refresh_needed = true;
            }
            SearchInput::ToggleWord => {
                self.settings.whole_word = !self.settings.whole_word;
                self.refresh_needed = true;
            }
            SearchInput::ToggleSelection => {
                self.settings.in_selection = !self.settings.in_selection;
                self.capture_scope(view);
                self.refresh_needed = true;
            }
            SearchInput::Next | SearchInput::Previous | SearchInput::AddCursor | SearchInput::ReplaceNext => {
                if self.mode == SearchMode::Files {
                    return self.file_move(session, view, input);
                }
                if matches!(input, SearchInput::AddCursor) {
                    self.start_seed(session, view)?;
                    return Ok(out);
                }
                if self.query.text.is_empty() {
                    return Err("Enter a search query before navigating matches".into());
                }
                let previous = matches!(input, SearchInput::Previous);
                let from = view.editor().map_or(0, |editor| {
                    if previous { editor.primary_cursor().selection().start } else { editor.primary_cursor().head }
                });
                let navigation = if previous {
                    Navigation::Previous { from, include_current: false }
                } else {
                    Navigation::Next { from, include_current: matches!(input, SearchInput::ReplaceNext) }
                };
                let follow = if matches!(input, SearchInput::ReplaceNext) {
                    Followup::ReplaceSelected
                } else {
                    Followup::Select(if matches!(input, SearchInput::AddCursor) {
                        SelectionMode::AddCursor
                    } else {
                        SelectionMode::Replace
                    })
                };
                self.refresh(session, view, Some(navigation), None)?;
                self.plan_followup(follow);
                return Ok(out);
            }
            SearchInput::ReplaceAll => {
                if self.mode == SearchMode::Files {
                    return Err("Folder replacement awaits the atomic folder executor".into());
                }
                self.refresh(session, view, None, Some(ReplaceTarget::All))?;
                self.plan_followup(Followup::ReplaceRecipe);
                return Ok(out);
            }
            SearchInput::SelectRow(row) => {
                out.action = self.row_action(session, view, row);
                if self.paint_key != self.request_key {
                    self.error = Some("Results are updating; wait before opening this row".into());
                }
                self.selected = row.min(self.result_rows.len().max(self.paint_hits.len()).saturating_sub(1));
                return Ok(out);
            }
            SearchInput::RowNext | SearchInput::RowPrevious => {
                let count = self.result_rows.len().max(self.paint_hits.len());
                if count != 0 {
                    self.selected = if matches!(input, SearchInput::RowPrevious) {
                        self.selected.saturating_sub(1)
                    } else {
                        (self.selected + 1).min(count - 1)
                    };
                }
                return Ok(out);
            }
            SearchInput::PageNext | SearchInput::PagePrevious => {
                if self.mode == SearchMode::Files
                    && (self.request_key.is_none()
                        || self.request_key != session.key()
                        || self.paint_key != session.key())
                {
                    return Err("Folder results changed; wait for the current query before paging".into());
                }
                if self.mode != SearchMode::Files {
                    return Ok(out);
                }
                if self.retry.is_some() {
                    return Err("Search page awaits worker retirement".into());
                }
                let first = if matches!(input, SearchInput::PagePrevious) {
                    self.file_first.saturating_sub(32)
                } else {
                    self.file_first.saturating_add(32)
                };
                if first >= self.files_seen && first != 0 {
                    return Ok(out);
                }
                match session.request_file_page(Page { first, len: 32 }) {
                    Ok(Some(key)) => {
                        self.request_key = Some(key);
                        self.pending = true;
                        self.exact = false;
                    }
                    Ok(None) => self.error = Some("Wait for the folder scan before revisiting pages".into()),
                    Err(error) => self.retain_rejected(error),
                }
                return Ok(out);
            }
            edit => {
                let field = self.field;
                self.check_root_edit(field, edit)?;
                let changed = self.input_mut(field).edit(edit)?;
                if changed {
                    self.followup = None;
                    if field == SearchField::Root {
                        self.root_edited = true;
                    }
                    self.refresh_needed = field != SearchField::Replacement;
                }
            }
        }
        if self.refresh_needed {
            self.refresh(session, view, None, None)?;
        }
        if old_paint != self.paint_key {
            out.damage.hit_rows = old_rows;
            out.damage.signal = true;
        }
        Ok(out)
    }
    fn retain_rejected(&mut self, error: SubmitError) {
        assert!(self.retry.is_none(), "one bounded frontend search retry slot");
        self.retry = Some(error.request);
        self.retry_epoch = self.epoch;
        self.request_key = None;
        self.followup = None;
        self.refresh_needed = true;
        self.error = Some(error.reason.into());
    }
    fn plan_followup(&mut self, follow: Followup) {
        if matches!(follow, Followup::ReplaceSelected | Followup::ReplaceRecipe) {
            self.replacement_epoch = Some(self.epoch);
        }
        if let Some(key) = self.request_key {
            self.followup = Some((key, follow));
        } else if self.retry.is_some() {
            self.retry_followup = Some(follow);
        }
    }
    fn start_seed(&mut self, session: &mut SearchSession, view: &Workspace) -> Result<(), String> {
        self.request_key = None;
        self.followup = None;
        self.pending_seed = None;
        if self.retry.is_some() {
            return Err("Search selection seed awaits worker retirement".into());
        }
        let editor = view.editor().ok_or("Document is not ready for selection search")?;
        self.active = true;
        self.identity = Self::active(view);
        self.identity_epoch = view.input_epoch();
        self.seed_epoch = Some(self.epoch);
        self.seed_origin =
            Some((editor.cursors().iter().map(|c| (c.anchor, c.head)).collect(), editor.primary_index()));
        match session.submit(view.active_id(), editor.document().revision(), SearchSession::seed_task(editor)) {
            Ok(key) => {
                self.request_key = Some(key);
                self.followup = Some((key, Followup::Seed));
                self.pending = true;
                self.refresh_needed = false;
                self.status = "Reading selection on search worker…".into();
            }
            Err(error) => {
                self.retain_rejected(error);
                self.retry_followup = Some(Followup::Seed);
            }
        }
        Ok(())
    }
    fn seed_cursor_matches(&self, view: &Workspace) -> bool {
        let Some((cursors, primary)) = &self.seed_origin else {
            return false;
        };
        view.editor().is_some_and(|editor| {
            editor.primary_index() == *primary
                && editor.cursors().len() == cursors.len()
                && editor
                    .cursors()
                    .iter()
                    .zip(cursors)
                    .all(|(cursor, (anchor, head))| cursor.anchor == *anchor && cursor.head == *head)
        })
    }
    /// Call only after the coherent dispatcher accepted SeedSelections under
    /// its original key. A newer query must not invalidate that action first.
    pub fn action_applied(
        &mut self,
        session: &mut SearchSession,
        view: &Workspace,
        action: &SearchAction,
    ) -> Result<SearchOutcome, String> {
        if !matches!(action, SearchAction::SeedSelections { .. }) {
            return Ok(SearchOutcome::default());
        }
        let Some((key, seed)) = self.pending_seed.take() else {
            return Err("Search selection acknowledgement is obsolete".into());
        };
        if key != action.key()
            || self.request_key != Some(key)
            || self.seed_epoch != Some(self.epoch)
            || !Self::active(view).is_some_and(|(id, revision)| session.validate_action(action, id, revision))
        {
            return Err("Search selection changed before acknowledgement".into());
        }
        self.query.set(seed.text)?;
        self.settings.regex = false;
        self.settings.case_sensitive = true;
        self.settings.whole_word = false;
        self.settings.in_selection = false;
        self.seed_epoch = None;
        self.seed_origin = None;
        if seed.from_selection {
            let from = view.editor().map_or(0, |editor| editor.primary_cursor().head);
            self.refresh(session, view, Some(Navigation::Next { from, include_current: false }), None)?;
            self.plan_followup(Followup::Select(SelectionMode::AddCursor));
        } else {
            self.refresh(session, view, None, None)?;
        }
        Ok(SearchOutcome::panel())
    }
    pub fn replacement_acknowledgement(
        &self,
        session: &SearchSession,
        view: &Workspace,
        action: &SearchAction,
    ) -> Result<ReplacementAcknowledgement, String> {
        if !matches!(action, SearchAction::Replace(_))
            || self.replacement_pending.is_some()
            || !self.validate_action(session, view, action)
        {
            return Err("Search replacement changed before admission".into());
        }
        Ok(ReplacementAcknowledgement { key: action.key(), epoch: self.epoch })
    }
    /// Successful submission is pending work, never an edit acknowledgement.
    pub fn replacement_admitted(
        &mut self,
        session: &mut SearchSession,
        acknowledgement: ReplacementAcknowledgement,
    ) -> SearchOutcome {
        self.replacement_pending = Some(acknowledgement);
        self.replacement_notice = None;
        self.error = None;
        self.status = "Replacing…".into();
        self.pending = false;
        self.followup = None;
        self.request_key = None;
        session.cancel();
        SearchOutcome::panel()
    }
    /// Call after Workspace has installed the returned editor, even on error.
    /// A changed input epoch cannot receive an old query's success message.
    pub fn replacement_finished(
        &mut self,
        session: &mut SearchSession,
        view: &Workspace,
        key: SearchKey,
        result: Result<(), String>,
    ) -> Result<SearchOutcome, String> {
        let Some(acknowledgement) = self.replacement_pending else {
            return Ok(SearchOutcome::default());
        };
        if acknowledgement.key != key {
            return Ok(SearchOutcome::default());
        }
        if view.document_busy(SessionDocumentId(key.document_id))
            || view.view(SessionDocumentId(key.document_id)).is_none_or(|view| view.editor().is_none())
        {
            return Err("Replacement completion arrived before the editor returned".into());
        }
        self.replacement_pending = None;
        let current = acknowledgement.epoch == self.epoch && view.active_id().0 == key.document_id;
        let mut out = self.sync_document(session, view)?;
        out.damage.panel = self.open;
        if current {
            let notice = match result {
                Ok(()) => "Replacement complete".into(),
                Err(error) => format!("Replacement failed: {error}"),
            };
            self.replacement_notice = Some((self.epoch, notice.clone()));
            self.error = None;
            self.status = notice;
            out.damage.status = true;
        }
        Ok(out)
    }
    fn refresh(
        &mut self,
        session: &mut SearchSession,
        view: &Workspace,
        navigation: Option<Navigation>,
        replacement: Option<ReplaceTarget>,
    ) -> Result<(), String> {
        // Even rejected/invalid intent invalidates admission of old actions.
        self.request_key = None;
        self.ready = false;
        self.followup = None;
        self.retry_followup = None;
        if !self.active && !self.open && navigation.is_none() && replacement.is_none() {
            self.refresh_needed = false;
            return Ok(());
        }
        self.active = true;
        if self.retry.is_some() {
            self.refresh_needed = true;
            return Err("Search is waiting for worker retirement".into());
        }
        if self.query.text.is_empty() {
            session.cancel();
            self.request_key = None;
            self.pending = false;
            self.refresh_needed = false;
            self.paint_key = None;
            self.paint_hits.clear();
            self.result_rows.clear();
            self.signal = [0; SIGNAL_BINS];
            self.count = 0;
            self.status = "Enter a search query".into();
            return Ok(());
        }
        let result = if self.mode == SearchMode::Files {
            let root = if self.root_edited {
                PathBuf::from(&self.root_input.text)
            } else {
                self.root.clone().ok_or("Search workspace root is not ready")?
            };
            if !root.is_absolute() {
                return Err("Search root must be absolute".into());
            }
            self.result_root = Some(root.clone());
            let mut request = FileSearch::new(root, self.query.text.clone());
            request.literal = !self.settings.regex;
            request.options.case_insensitive = !self.settings.case_sensitive;
            request.options.whole_word = self.settings.whole_word;
            request.include = self.include.text.lines().filter(|line| !line.is_empty()).map(str::to_owned).collect();
            request.exclude = self.exclude.text.lines().filter(|line| !line.is_empty()).map(str::to_owned).collect();
            session.submit_files(request)
        } else {
            let editor = view.editor().ok_or("Document is not ready for search")?;
            let mut query = Query::literal(self.query.text.clone());
            query.literal = !self.settings.regex;
            query.options.case_insensitive = !self.settings.case_sensitive;
            query.options.whole_word = self.settings.whole_word;
            let replacement =
                replacement.map(|target| Replacement { template: self.replacement.text.as_bytes().to_vec(), target });
            let mut task = SearchSession::document_task(
                editor,
                query,
                self.settings.in_selection,
                Page::default(),
                navigation,
                replacement,
            );
            if self.settings.in_selection
                && let SearchTask::Document(document) = &mut task
            {
                document.ranges = self.scope.clone();
            }
            session.submit(view.active_id(), editor.document().revision(), task)
        };
        match result {
            Ok(key) => {
                self.request_key = Some(key);
                self.pending = true;
                self.exact = false;
                self.refresh_needed = false;
                self.status = "Searching…".into();
            }
            Err(error) => self.retain_rejected(error),
        }
        Ok(())
    }
    pub fn sync_document(&mut self, session: &mut SearchSession, view: &Workspace) -> Result<SearchOutcome, String> {
        let next = Self::active(view);
        if self.replacement_pending.is_some_and(|ack| ack.key.document_id == view.active_id().0)
            && view.document_busy(view.active_id())
        {
            return Ok(SearchOutcome::default());
        }
        if !self.active {
            self.identity = next;
            self.identity_epoch = view.input_epoch();
            return Ok(SearchOutcome::default());
        }
        if self.mode == SearchMode::Files {
            if self.refresh_needed {
                self.refresh(session, view, None, None)?;
                return Ok(SearchOutcome::panel());
            }
            return Ok(SearchOutcome::default());
        }
        if next == self.identity && self.identity_epoch == view.input_epoch() {
            return Ok(SearchOutcome::default());
        }
        let rows = self.hit_rows(view);
        self.advance_epoch();
        self.identity = next;
        self.identity_epoch = view.input_epoch();
        session.cancel();
        self.paint_hits.clear();
        self.paint_key = None;
        self.signal = [0; SIGNAL_BINS];
        self.followup = None;
        self.capture_scope(view);
        if !self.query.text.is_empty() {
            self.refresh(session, view, None, None)?;
        }
        Ok(SearchOutcome {
            damage: SearchDamage { panel: self.open, status: true, hit_rows: rows, signal: true, ..Default::default() },
            ..Default::default()
        })
    }
    pub fn worker_ready(&mut self, session: &mut SearchSession, view: &Workspace) -> SearchOutcome {
        let identity =
            if self.mode == SearchMode::Files { Some((SessionDocumentId(0), 0)) } else { Self::active(view) };
        let Some((document, revision)) = identity else {
            return SearchOutcome::default();
        };
        if let Some(request) = self.retry.take() {
            if self.active && self.retry_epoch == self.epoch {
                let follow = self.retry_followup.take();
                match session.submit(SessionDocumentId(request.key.document_id), request.key.revision, request.task) {
                    Ok(key) => {
                        self.request_key = Some(key);
                        self.followup = follow.map(|follow| (key, follow));
                        self.refresh_needed = false;
                        self.pending = true;
                    }
                    Err(error) => {
                        self.retain_rejected(error);
                        self.retry_followup = follow;
                    }
                }
            } else if let Err(request) = session.retire_request(*request) {
                self.retry = Some(request);
            }
        }
        let changed = session.worker_ready(document, revision);
        if self.refresh_needed
            && self.retry.is_none()
            && let Err(error) = self.refresh(session, view, None, None)
        {
            self.error = Some(error);
        }
        let seed_completion = matches!(self.followup,Some((key,Followup::Seed))if session.key()==Some(key));
        let mut out = if changed && !seed_completion {
            self.accept_state(session.state(), view)
        } else if changed {
            SearchOutcome::panel()
        } else {
            SearchOutcome::default()
        };
        if let Some((key, follow)) = self.followup
            && session.key() == Some(key)
            && session.state().status == SearchStatus::Complete
        {
            self.followup = None;
            if matches!(follow, Followup::ReplaceSelected | Followup::ReplaceRecipe)
                && self.replacement_epoch != Some(self.epoch)
            {
                self.error = Some("Replacement cancelled: search input changed".into());
                return out;
            }
            match follow {
                Followup::Seed => {
                    if self.seed_epoch == Some(self.epoch) && self.seed_cursor_matches(view) {
                        if let Some(seed) = session.state().seed.clone() {
                            self.pending_seed = Some((key, seed));
                            out.action = session.seed_action();
                        } else {
                            self.error = Some("No word or selection at the cursor".into());
                        }
                    } else {
                        self.error = Some("Selection search cancelled: the cursor or input changed".into());
                    }
                }
                Followup::Select(mode) => out.action = session.navigation_action(mode),
                Followup::ReplaceSelected => {
                    if let Some(hit) = session.state().navigation {
                        if let Err(error) = self.refresh(session, view, None, Some(ReplaceTarget::Matches(vec![hit]))) {
                            self.error = Some(error);
                        } else {
                            self.plan_followup(Followup::ReplaceRecipe);
                        }
                    }
                }
                Followup::ReplaceRecipe => {
                    if let Some(editor) = view.editor() {
                        match session.replacement_action(document, editor) {
                            Ok(action) => out.action = action,
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                self.followup = Some((key, follow));
                                self.error = Some(error.to_string());
                            }
                            Err(error) => self.error = Some(error.to_string()),
                        }
                    }
                }
            }
        }
        if out.action.as_ref().is_some_and(|action| !self.validate_action(session, view, action)) {
            out.action = None;
        }
        out
    }
    fn accept_state(&mut self, state: &SearchState, view: &Workspace) -> SearchOutcome {
        if state.key != self.request_key {
            return SearchOutcome::default();
        }
        let mut rows = self.hit_rows(view);
        self.pending = matches!(state.status, SearchStatus::Pending | SearchStatus::Searching);
        self.ready = state.status == SearchStatus::Complete;
        // core-search's regex contract requires every possible match to fit
        // Query.max_match_bytes. This GUI cannot prove arbitrary patterns fit;
        // completion is an engine count, not all-size regex completeness.
        self.exact = state.status == SearchStatus::Complete && (!self.settings.regex || self.mode == SearchMode::Files);
        if state.status == SearchStatus::Complete || !state.hits.is_empty() || !state.files.is_empty() {
            self.paint_key = state.key;
            self.paint_hits = state.hits.clone();
            // Navigation can lie outside the first bounded hit page. Keep its
            // exact match visible too; the cache remains at most PAGE_HITS + 1.
            if let Some(hit) = state.navigation {
                if !self.paint_hits.contains(&hit) {
                    self.paint_hits.push(hit);
                }
                self.paint_hits.sort_unstable_by_key(|hit| hit.offset);
            }
            self.signal = state.signal;
            self.count = state.count;
            self.file_first = state.file_first;
            self.files_seen = state.files_seen;
            self.result_rows = state
                .files
                .iter()
                .take(PAGE_HITS)
                .map(|row| match row {
                    FileEvent::Hit(hit) => SearchResultRow {
                        label: format!(
                            "{}:{}  {}",
                            bounded_label(&self.result_path(&hit.path).to_string_lossy(), 512),
                            hit.line_number,
                            bounded_label(&String::from_utf8_lossy(&hit.line), 512)
                        ),
                        is_hit: true,
                    },
                    FileEvent::Error { path, message } => SearchResultRow {
                        label: format!(
                            "{}: {}",
                            bounded_label(&self.result_path(path).to_string_lossy(), 512),
                            bounded_label(message, 512)
                        ),
                        is_hit: false,
                    },
                })
                .collect();
            self.selected = self.selected.min(self.result_rows.len().max(self.paint_hits.len()).saturating_sub(1));
        }
        self.error = state.error.clone();
        let noun = if self.mode == SearchMode::Files { "matching lines" } else { "matches" };
        self.status = if self.exact {
            format!("{} {noun}", state.count)
        } else if self.paint_key != state.key {
            format!("Searching… previous {} {noun}", self.count)
        } else {
            format!("≈ {} {noun}", state.count)
        };
        if self.settings.regex && self.mode != SearchMode::Files {
            self.status.push_str("; regex matches limited to 64 KiB, completeness is not guaranteed");
        }
        if let Some((epoch, notice)) = &self.replacement_notice
            && *epoch == self.epoch
        {
            self.status = format!("{notice}; {}", self.status);
        }
        rows.extend(self.hit_rows(view));
        rows.sort_unstable();
        rows.dedup();
        SearchOutcome {
            damage: SearchDamage { panel: self.open, status: true, hit_rows: rows, signal: true, ..Default::default() },
            ..Default::default()
        }
    }
    pub fn result_path<'a>(&self, path: &'a std::path::Path) -> &'a std::path::Path {
        self.result_root
            .as_ref()
            .and_then(|root| path.strip_prefix(root).ok())
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(path)
    }
    fn row_action(&self, session: &SearchSession, view: &Workspace, row: usize) -> Option<SearchAction> {
        if self.paint_key != session.key() || self.paint_key != self.request_key {
            return None;
        }
        let action = if self.mode == SearchMode::Files {
            session.file_action(row, 0)
        } else {
            session.selection_action(row, SelectionMode::Replace)
        }?;
        let (document, revision) =
            if self.mode == SearchMode::Files { (SessionDocumentId(0), 0) } else { Self::active(view)? };
        session.validate_action(&action, document, revision).then_some(action)
    }
    fn file_move(
        &mut self,
        session: &mut SearchSession,
        view: &Workspace,
        input: SearchInput<'_>,
    ) -> Result<SearchOutcome, String> {
        if matches!(input, SearchInput::ReplaceNext | SearchInput::AddCursor) {
            return Err("This action requires a document search".into());
        }
        if !self.result_rows.is_empty() {
            self.selected = if matches!(input, SearchInput::Previous) {
                self.selected.saturating_sub(1)
            } else {
                (self.selected + 1).min(self.result_rows.len() - 1)
            };
        }
        Ok(SearchOutcome { action: self.row_action(session, view, self.selected), ..SearchOutcome::panel() })
    }
    pub fn hit_rows(&self, view: &Workspace) -> Vec<usize> {
        let Some((document, revision)) = Self::active(view) else {
            return Vec::new();
        };
        if self.paint_key.is_none_or(|key| key.document_id != document.0 || key.revision != revision) {
            return Vec::new();
        }
        (0..view.rows())
            .filter(|&row| {
                view.line_source(row).is_some_and(|range| {
                    self.paint_hits.iter().any(|hit| {
                        if hit.len == 0 {
                            range.start <= hit.offset && hit.offset <= range.end
                        } else {
                            hit.offset < range.end && range.start < hit.offset.saturating_add(hit.len)
                        }
                    })
                })
            })
            .collect()
    }
    pub fn visible_hits(&self, document: SessionDocumentId, revision: u64) -> &[Match] {
        if self.paint_key.is_some_and(|key| key.document_id == document.0 && key.revision == revision) {
            &self.paint_hits
        } else {
            &[]
        }
    }
    pub fn validate_action(&self, session: &SearchSession, view: &Workspace, action: &SearchAction) -> bool {
        if self.mode == SearchMode::Files && matches!(action, SearchAction::OpenFile { .. }) {
            // Folder results belong to their query, not the tab that launched
            // it. Hosts separately capture the current input/cursor at action
            // admission; never substitute a newer ticket for an old click.
            return self.request_key == Some(action.key()) && session.validate_action(action, SessionDocumentId(0), 0);
        }
        self.request_key == Some(action.key())
            && self.identity_epoch == view.input_epoch()
            && Self::active(view).is_some_and(|(id, revision)| session.validate_action(action, id, revision))
            && (!matches!(action, SearchAction::SeedSelections { .. })
                || (self.seed_epoch == Some(self.epoch) && self.seed_cursor_matches(view)))
            && (!matches!(action, SearchAction::Replace(_)) || self.replacement_epoch == Some(self.epoch))
    }
    /// Current architecture: cached logical bytes + the existing grid only.
    /// Do not restore projected_row/source-reading fallbacks here. ROOT retains
    /// the P2 actual-read/version adapter and worker-owned frame-cache gate.
    pub fn highlight_columns(&self, view: &Workspace, row: usize, columns: usize) -> Vec<Range<u64>> {
        let Some((id, revision)) = Self::active(view) else { return Vec::new() };
        let Some(source) = view.line_source(row) else { return Vec::new() };
        let Some(bytes) = view.line(row) else { return Vec::new() };
        let hits = self.visible_hits(id, revision);
        // Input cache is bounded by View's 64 KiB logical line window.
        crate::grid::cells(bytes, columns, view.editor().map_or(4, |editor| editor.settings.editor.tab_size as usize))
            .into_iter()
            .filter_map(|cell| {
                let range = source.start + cell.bytes.start as u64..source.start + cell.bytes.end as u64;
                hits.iter()
                    .any(|hit| {
                        if hit.len == 0 {
                            range.start <= hit.offset && hit.offset < range.end
                        } else {
                            hit.offset < range.end && range.start < hit.offset.saturating_add(hit.len)
                        }
                    })
                    .then_some(cell.column as u64..(cell.column + cell.width) as u64)
            })
            .collect()
    }
    /// Worker startup/failure still has a visible focused input destination.
    /// Retain one bounded latest query/options state; replay it on attachment.
    pub fn deferred_input(&mut self, input: SearchInput<'_>) -> Result<SearchOutcome, String> {
        self.advance_epoch();
        match input {
            SearchInput::Cancel => {
                self.open = false;
                self.active = false;
                return Ok(SearchOutcome {
                    damage: SearchDamage { panel: true, restore_editor: true, status: true, ..Default::default() },
                    ..Default::default()
                });
            }
            SearchInput::ToggleRegex => self.settings.regex = !self.settings.regex,
            SearchInput::ToggleCase => self.settings.case_sensitive = !self.settings.case_sensitive,
            SearchInput::ToggleWord => self.settings.whole_word = !self.settings.whole_word,
            SearchInput::ToggleSelection => self.settings.in_selection = !self.settings.in_selection,
            SearchInput::Focus(field) => self.field = field,
            SearchInput::NextField | SearchInput::PreviousField => {
                let fields = self.visible_fields();
                let at = fields.iter().position(|field| *field == self.field).unwrap_or(0);
                let step = if matches!(input, SearchInput::PreviousField) { fields.len() - 1 } else { 1 };
                self.field = fields[(at + step) % fields.len()];
            }
            SearchInput::Next
            | SearchInput::Previous
            | SearchInput::AddCursor
            | SearchInput::ReplaceNext
            | SearchInput::ReplaceAll => {
                self.error = Some("Search worker is not ready; query retained, navigation awaits readiness".into());
                return Ok(SearchOutcome::panel());
            }
            edit => {
                let field = self.field;
                self.check_root_edit(field, edit)?;
                if self.input_mut(field).edit(edit)? && field == SearchField::Root {
                    self.root_edited = true;
                }
            }
        }
        self.refresh_needed = true;
        self.error = Some("Search worker is starting or unavailable; query input is retained".into());
        Ok(SearchOutcome::panel())
    }
    /// Until the backend's extra closing-slot seam is installed, ownership is
    /// explicit: the caller MUST move this request onto its shutdown worker.
    pub fn take_pending_request(&mut self) -> Option<Box<SearchRequest>> {
        self.retry.take()
    }
    pub fn take_shutdown(&mut self, session: &mut SearchSession) -> Result<SearchWorker, Option<Box<SearchRequest>>> {
        self.open = false;
        self.active = false;
        self.followup = None;
        self.pending_seed = None;
        session.take_shutdown_with(self.retry.take().map(|request| *request))
    }
}
fn bounded_label(text: &str, limit: usize) -> String {
    crate::palette_session::display_text(&text.chars().take(limit).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::mpsc,
        time::{Duration, Instant},
    };
    fn workspace(text: &str) -> Workspace {
        let mut view = Workspace::new(crate::View::new(None)).unwrap();
        view.execute("edit.insert", &crate::CommandArgs { text: Some(text.into()), ..Default::default() }, 1).unwrap();
        view.set_rows(4);
        view
    }
    fn session() -> (SearchSession, mpsc::Receiver<()>) {
        let (tx, rx) = mpsc::channel();
        (
            SearchSession::new(move || {
                let _ = tx.send(());
            })
            .unwrap(),
            rx,
        )
    }
    fn finish(
        panel: &mut SearchPanel,
        session: &mut SearchSession,
        view: &Workspace,
        rx: &mpsc::Receiver<()>,
    ) -> SearchOutcome {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let out = panel.worker_ready(session, view);
            if out.action.is_some() || (session.state().status == SearchStatus::Complete && panel.followup.is_none()) {
                return out;
            }
            assert_ne!(session.state().status, SearchStatus::Failed, "{:?}", session.state().error);
            rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).expect("bounded search panel wake");
        }
    }
    #[test]
    fn bounded_unicode_inputs_keep_horizontal_caret_and_transactional_paste() {
        let mut input = TextInput::default();
        input.edit(SearchInput::Text("界e\u{301}\nabc")).unwrap();
        input.edit(SearchInput::Left).unwrap();
        let before = input.text.clone();
        let cursor = input.cursor;
        assert!(input.edit(SearchInput::Text(&"x".repeat(INPUT_BYTES))).is_err());
        assert_eq!((input.text.clone(), input.cursor), (before, cursor));
        let window = crate::palette_session::text_window(&input.text, input.cursor, 4);
        assert!(window.cursor < 4);
        assert!(!window.text.contains('\n'));
        input.edit(SearchInput::Home).unwrap();
        input.edit(SearchInput::Delete).unwrap();
        assert_eq!(input.text, "e\u{301}\nabc");
        input.edit(SearchInput::SelectAll).unwrap();
        input.edit(SearchInput::Text("paste")).unwrap();
        assert_eq!(input.text, "paste");
    }
    #[test]
    fn ime_destinations_reject_field_moves_reopen_and_query_changes() {
        let view = workspace("");
        let (mut session, _) = session();
        let mut panel = SearchPanel::default();
        panel.open(SearchMode::Replace, &SearchSettings::default()).unwrap();
        let original = panel.destination().unwrap();
        panel.input(&mut session, &view, SearchInput::Focus(SearchField::Replacement)).unwrap();
        assert!(panel.commit(&mut session, &view, original, "日本語").is_err());
        let current = panel.destination().unwrap();
        panel.commit(&mut session, &view, current, "$1").unwrap();
        assert_eq!(panel.text(SearchField::Replacement).0, "$1");
        let current = panel.destination().unwrap();
        panel.close(&mut session, &view);
        panel.open(SearchMode::Replace, &SearchSettings::default()).unwrap();
        assert!(panel.commit(&mut session, &view, current, "stale").is_err());
        assert_eq!(panel.text(SearchField::Replacement).0, "$1");
    }
    #[test]
    fn cancel_retains_draft_but_document_edits_do_not_restart_search() {
        let mut view = workspace("old");
        let (mut session, _) = session();
        let mut panel = SearchPanel::default();
        panel.command(&mut session, &view, "search.find", &Default::default()).unwrap();
        panel.input(&mut session, &view, SearchInput::Text("old")).unwrap();
        panel.close(&mut session, &view);
        view.execute("edit.insert", &crate::CommandArgs { text: Some("next".into()), ..Default::default() }, 1)
            .unwrap();
        panel.sync_document(&mut session, &view).unwrap();
        assert!(session.key().is_none());
        assert_eq!(panel.text(SearchField::Query).0, "old");
    }
    #[test]
    fn escape_restores_editor_context_and_cancels_pending_prefix_without_changing_vim_mode() {
        use kwil_core_commands::{Binding, Context, Preset, keymap::KeymapEngine, vim::VimResult};
        let view = workspace("");
        let (mut session, _) = session();
        let mut panel = SearchPanel::default();
        panel.open(SearchMode::Find, &SearchSettings::default()).unwrap();
        let mut keys = KeymapEngine::new(
            Preset::Vim,
            &[Binding {
                keys: "ctrl+k ctrl+s".into(),
                command: "file.open".into(),
                context: Some(Context::Palette),
                mode: None,
            }],
        )
        .unwrap();
        let mode = keys.mode();
        panel.enter_context(&mut keys);
        assert!(matches!(keys.feed("ctrl+k".parse().unwrap()), VimResult::Pending));
        let out = panel.close(&mut session, &view);
        out.restore_context(&mut keys);
        assert!(out.damage.restore_editor);
        assert_eq!(keys.mode(), mode);
        assert!(matches!(keys.feed("ctrl+s".parse().unwrap()), VimResult::Command(call)if call.id=="file.save"));
        assert!(!panel.is_open());
    }
    #[test]
    fn fake_worker_results_keep_previous_paint_but_never_restamp_stale_actions() {
        let view = workspace("old new");
        let (id, revision) = SearchPanel::active(&view).unwrap();
        let old = SearchKey { document_id: id.0, revision, generation: 1 };
        let new = SearchKey { generation: 2, ..old };
        let mut panel = SearchPanel { request_key: Some(old), ..Default::default() };
        let mut state = SearchState {
            key: Some(old),
            status: SearchStatus::Complete,
            hits: vec![Match { offset: 0, len: 3 }],
            count: 1,
            ..Default::default()
        };
        state.signal[0] = 1;
        panel.accept_state(&state, &view);
        assert_eq!(panel.visible_hits(id, revision), &[Match { offset: 0, len: 3 }]);
        panel.request_key = Some(new);
        let pending = SearchState { key: Some(new), status: SearchStatus::Pending, ..Default::default() };
        panel.accept_state(&pending, &view);
        assert_eq!(panel.paint_key, Some(old));
        assert!(panel.status().contains("previous"));
        let changed = panel.accept_state(&state, &view);
        assert!(!changed.damage.panel && !changed.damage.signal);
        state.key = Some(new);
        state.hits = vec![Match { offset: 4, len: 3 }];
        panel.accept_state(&state, &view);
        assert_eq!(panel.paint_key, Some(new));
        assert_eq!(panel.paint_hits[0].offset, 4);
    }
    #[test]
    fn accepted_next_wrap_and_add_cursor_actions_are_exactly_fenced() {
        let mut view = workspace("alpha beta alpha");
        let (mut session, rx) = session();
        let mut panel = SearchPanel::default();
        panel.command(&mut session, &view, "search.find", &SearchSettings::default()).unwrap();
        panel.input(&mut session, &view, SearchInput::Text("alpha")).unwrap();
        finish(&mut panel, &mut session, &view, &rx);
        panel.input(&mut session, &view, SearchInput::Next).unwrap();
        let out = finish(&mut panel, &mut session, &view, &rx);
        let action = out.action.expect("accepted next action");
        assert!(panel.validate_action(&session, &view, &action));
        assert!(matches!(&action,SearchAction::Select{range,mode:SelectionMode::Replace,..}if range==&(0..5)));
        view.editor_mut()
            .unwrap()
            .set_cursors(vec![crate::Cursor { anchor: 0, head: 5, desired_column: None }], 0)
            .unwrap();
        panel.input(&mut session, &view, SearchInput::AddCursor).unwrap();
        let seed = finish(&mut panel, &mut session, &view, &rx).action.unwrap();
        assert!(matches!(&seed, SearchAction::SeedSelections { from_selection: true, .. }));
        assert!(panel.validate_action(&session, &view, &seed));
        let seed_key = seed.key();
        assert_eq!(session.key(), Some(seed_key));
        panel.action_applied(&mut session, &view, &seed).unwrap();
        assert_ne!(session.key(), Some(seed_key));
        let action = finish(&mut panel, &mut session, &view, &rx).action.unwrap();
        assert!(matches!(action, SearchAction::Select { mode: SelectionMode::AddCursor, .. }));
        panel.input(&mut session, &view, SearchInput::Text("z")).unwrap();
        assert!(!panel.validate_action(&session, &view, &action));
    }
    #[test]
    fn no_query_ctrl_d_selects_worker_word_before_query_generation_and_rejects_cursor_movement() {
        let mut view = workspace("one two one");
        let (mut session, rx) = session();
        let mut panel = SearchPanel::default();
        panel.input(&mut session, &view, SearchInput::AddCursor).unwrap();
        let action = finish(&mut panel, &mut session, &view, &rx).action.unwrap_or_else(|| {
            panic!(
                "seed action missing: error={:?} status={} seed={:?} epoch={} seed_epoch={:?} cursor_matches={}",
                panel.error,
                panel.status,
                session.state().seed,
                panel.epoch,
                panel.seed_epoch,
                panel.seed_cursor_matches(&view)
            )
        });
        assert!(panel.validate_action(&session, &view, &action));
        let key = action.key();
        assert_eq!(session.key(), Some(key));
        assert!(panel.text(SearchField::Query).0.is_empty());
        let SearchAction::SeedSelections { cursors, primary, from_selection: false, .. } = &action else {
            panic!("word selection first")
        };
        view.editor_mut()
            .unwrap()
            .set_cursors(
                cursors
                    .iter()
                    .map(|cursor| crate::Cursor { anchor: cursor.anchor, head: cursor.head, desired_column: None })
                    .collect(),
                *primary,
            )
            .unwrap();
        panel.action_applied(&mut session, &view, &action).unwrap();
        assert_eq!(panel.text(SearchField::Query).0, "one");
        assert_ne!(session.key(), Some(key));
        finish(&mut panel, &mut session, &view, &rx);
        panel.input(&mut session, &view, SearchInput::AddCursor).unwrap();
        let action = finish(&mut panel, &mut session, &view, &rx).action.unwrap();
        view.editor_mut()
            .unwrap()
            .set_cursors(vec![crate::Cursor { anchor: 0, head: 0, desired_column: None }], 0)
            .unwrap();
        assert!(!panel.validate_action(&session, &view, &action));
    }
    #[test]
    fn shutdown_transfers_one_panel_request_and_stopped_session_returns_its_owner() {
        let view = workspace("small");
        let (mut session, _) = session();
        let mut panel = SearchPanel::default();
        let request = || {
            Box::new(SearchRequest {
                key: SearchKey {
                    document_id: view.active_id().0,
                    revision: view.editor().unwrap().document().revision(),
                    generation: 99,
                },
                task: SearchSession::document_task(
                    view.editor().unwrap(),
                    Query::literal("s".into()),
                    false,
                    Page::default(),
                    None,
                    None,
                ),
            })
        };
        panel.retry = Some(request());
        let worker = panel.take_shutdown(&mut session).unwrap_or_else(|_| panic!("reserved shutdown owner transfer"));
        assert!(panel.retry.is_none());
        worker.shutdown().unwrap();
        panel.retry = Some(request());
        let returned = match panel.take_shutdown(&mut session) {
            Err(Some(owner)) => owner,
            _ => panic!("stopped session must return external owner"),
        };
        assert_eq!(returned.key.generation, 99);
        std::thread::spawn(move || drop(returned)).join().unwrap();
    }
    #[test]
    fn accepted_regex_capture_replacement_returns_recipe_without_mutating_document() {
        let view = workspace("x1 x2");
        let (mut session, rx) = session();
        let mut panel = SearchPanel::default();
        let settings = SearchSettings { regex: true, ..Default::default() };
        panel.command(&mut session, &view, "search.replace", &settings).unwrap();
        panel.input(&mut session, &view, SearchInput::Text("x([0-9])")).unwrap();
        finish(&mut panel, &mut session, &view, &rx);
        panel.input(&mut session, &view, SearchInput::Focus(SearchField::Replacement)).unwrap();
        panel.input(&mut session, &view, SearchInput::Text("y$1")).unwrap();
        panel.input(&mut session, &view, SearchInput::ReplaceAll).unwrap();
        let action = finish(&mut panel, &mut session, &view, &rx).action.unwrap();
        assert!(panel.validate_action(&session, &view, &action));
        let SearchAction::Replace(recipe) = action else { panic!("typed replacement recipe") };
        let edits = recipe.edits.iter().collect::<std::io::Result<Vec<_>>>().unwrap();
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0].new_bytes, b"y1");
        assert_eq!(edits[1].new_bytes, b"y2");
        assert_eq!(view.editor().unwrap().document().snapshot().len(), 5);
    }
    #[test]
    fn file_result_pages_revisit_native_paths_without_collecting_full_stream() {
        let root = std::env::temp_dir().join(format!(
            "kwil-search-panel-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        #[cfg(any(not(unix), target_os = "macos"))]
        let path = root.join("résumé-界.txt");
        #[cfg(all(unix, not(target_os = "macos")))]
        let path = {
            use std::os::unix::ffi::OsStringExt;
            root.join(std::ffi::OsString::from_vec(b"native-\xff.txt".to_vec()))
        };
        std::fs::write(&path, "hit\n".repeat(70)).unwrap();
        let view = workspace("");
        let (mut session, rx) = session();
        let mut panel = SearchPanel::default();
        panel.set_root(root.clone()).unwrap();
        panel.command(&mut session, &view, "search.findInFiles", &SearchSettings::default()).unwrap();
        panel.input(&mut session, &view, SearchInput::Text("hit")).unwrap();
        finish(&mut panel, &mut session, &view, &rx);
        assert_eq!(panel.count, 70);
        assert!(panel.result_rows.len() <= 32);
        let results = session.state().file_results.clone().unwrap();
        let original_session = results.session();
        // Fake a returned page request: the retry must revisit this same spool,
        // not silently restart filesystem matching when capacity becomes free.
        panel.retry = Some(Box::new(SearchRequest {
            key: session.key().unwrap(),
            task: SearchTask::FilePage { results, page: Page { first: 0, len: 32 } },
        }));
        panel.retry_epoch = panel.epoch;
        panel.refresh_needed = true;
        finish(&mut panel, &mut session, &view, &rx);
        assert_eq!(session.state().file_results.as_ref().unwrap().session(), original_session);
        for _ in 0..3 {
            if panel.file_first == 0 {
                break;
            }
            panel.input(&mut session, &view, SearchInput::PagePrevious).unwrap();
            finish(&mut panel, &mut session, &view, &rx);
        }
        assert_eq!(panel.file_first, 0);
        panel.input(&mut session, &view, SearchInput::PageNext).unwrap();
        finish(&mut panel, &mut session, &view, &rx);
        assert_eq!(panel.file_first, 32);
        panel.input(&mut session, &view, SearchInput::PagePrevious).unwrap();
        finish(&mut panel, &mut session, &view, &rx);
        let action = panel.input(&mut session, &view, SearchInput::SelectRow(0)).unwrap().action.unwrap();
        assert!(matches!(&action,SearchAction::OpenFile{path:actual,line_number:1,..}if actual==&path));
        panel.input(&mut session, &view, SearchInput::Focus(SearchField::Root)).unwrap();
        panel.input(&mut session, &view, SearchInput::SelectAll).unwrap();
        assert!(panel.input(&mut session, &view, SearchInput::Text("relative")).is_err());
        let (id, revision) = SearchPanel::active(&view).unwrap();
        assert!(session.validate_action(&action, id, revision), "backend still owns the old accepted result");
        assert!(
            !panel.validate_action(&session, &view, &action),
            "invalid new root intent must block old action immediately"
        );
        session.stop();
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn folder_late_root_starts_query_without_an_editor_and_actions_survive_tab_changes() {
        let root = std::env::temp_dir().join(format!(
            "kwil-folder-root-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("keep.txt");
        std::fs::write(&path, b"hit\n").unwrap();
        let mut view = workspace("");
        view.editor = None;
        let (mut session, rx) = session();
        let mut panel = SearchPanel::default();
        panel.open(SearchMode::Files, &SearchSettings::default()).unwrap();
        panel.deferred_input(SearchInput::Text("hit")).unwrap();
        panel.root_ready(&mut session, &view, root.clone()).unwrap();
        finish(&mut panel, &mut session, &view, &rx);
        assert_eq!(panel.count(), (1, true));
        assert!(panel.results()[0].label.starts_with("keep.txt:1"));
        let action = panel.row_action(&session, &view, 0).unwrap();
        assert!(panel.validate_action(&session, &view, &action));
        view.file_command("file.new").unwrap();
        panel.sync_document(&mut session, &view).unwrap();
        assert!(panel.validate_action(&session, &view, &action), "query identity is independent of the opening tab");
        assert!(matches!(action, SearchAction::OpenFile { path: actual, .. } if actual == path));
        panel.deferred_input(SearchInput::Focus(SearchField::Root)).unwrap();
        panel.deferred_input(SearchInput::SelectAll).unwrap();
        panel.deferred_input(SearchInput::Text("relative")).unwrap();
        assert!(panel.root_edited);
        assert!(panel.sync_document(&mut session, &view).is_err());
        assert!(
            panel.input(&mut session, &view, SearchInput::PageNext).is_err(),
            "invalid root cannot revive old pages"
        );
        drop(panel);
        let worker = session.take_shutdown().unwrap();
        drop(session);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut cleanup = worker.shutdown_until(deadline);
        loop {
            match cleanup {
                Err(error)
                    if error.reason() == kwil_core_search::worker::WorkerSettlementReason::ThreadExitPending
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(
                        Duration::from_millis(1).min(deadline.saturating_duration_since(Instant::now())),
                    );
                    cleanup = error.retry_until(deadline);
                }
                terminal => {
                    terminal.unwrap();
                    break;
                }
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn folder_native_root_display_cannot_be_partially_reconstructed_as_a_new_path() {
        #[cfg(unix)]
        let name = {
            use std::os::unix::ffi::OsStringExt;
            std::ffi::OsString::from_vec(b"native-\xff".to_vec())
        };
        #[cfg(windows)]
        let name = {
            use std::os::windows::ffi::OsStringExt;
            std::ffi::OsString::from_wide(&[0x6e, 0xd800])
        };
        let root = std::env::temp_dir().join(name);
        let mut panel = SearchPanel::default();
        panel.set_root(root.clone()).unwrap();
        panel.deferred_input(SearchInput::Focus(SearchField::Root)).unwrap();
        let display = panel.text(SearchField::Root).0.to_owned();
        assert!(panel.deferred_input(SearchInput::Text("x")).is_err());
        assert_eq!(panel.root.as_ref(), Some(&root));
        assert_eq!(panel.text(SearchField::Root).0, display);
        assert!(!panel.root_edited);
        panel.deferred_input(SearchInput::SelectAll).unwrap();
        panel.deferred_input(SearchInput::Text("new explicit path")).unwrap();
        assert!(panel.root_edited);
        assert_eq!(panel.text(SearchField::Root).0, "new explicit path");
    }
}
