//! Memory-only compound settings controller. The host retains its settings rail
//! and applies returned edits through `SettingsSession::edit_document`.
//!
//! ```
//! use kwil_core_commands::settings_ui::SettingsDocument;
//! use kwil_view_model::{settings_panels::{PanelInput, SettingsPanelController},
//!     settings_session::SettingsPanel};
//! let mut doc = SettingsDocument::parse("").unwrap();
//! let mut panel = SettingsPanelController::new(SettingsPanel::FileTypes, &doc).unwrap();
//! panel.input(&doc, PanelInput::Accept).unwrap(); // Add row, extension
//! panel.input(&doc, PanelInput::Text("rs")).unwrap();
//! panel.input(&doc, PanelInput::Accept).unwrap(); // language
//! panel.input(&doc, PanelInput::Text("rust")).unwrap();
//! let edit = panel.input(&doc, PanelInput::Accept).unwrap().unwrap();
//! edit.apply(&mut doc).unwrap(); // host: session.edit_document(|doc| edit.apply(doc))
//! assert_eq!(doc.language_for_extension("rs"), Some("rust"));
//! ```
use crate::settings_session::SettingsPanel;
use kwil_core_commands::{
    Binding, Context, Key, KeyChord, KeySequence, Mode, NamedKey,
    settings::SettingsError,
    settings_ui::{KeyCapture, KeyConflict, SettingValue, SettingsDocument},
};
use unicode_segmentation::UnicodeSegmentation;

pub const PANEL_INPUT_LIMIT: usize = 64 * 1024;
/// Refuse larger panels explicitly. A viewport is separately bounded by this cap.
pub const PANEL_ROW_LIMIT: usize = 4096;
#[cfg(test)]
thread_local! { static ROW_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
#[cfg(test)]
pub(crate) fn take_row_builds() -> usize {
    ROW_BUILDS.with(|count| count.replace(0))
}

#[derive(Clone, Copy, Debug)]
pub enum PanelInput<'a> {
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
    Capture(KeyChord),
    ReviewPage { index: usize, pages: usize },
}

/// Constructed only after a completed edit or explicit conflict confirmation.
/// Applying rechecks the approved conflicts against the current document, so a
/// delayed host callback cannot silently replace newly introduced conflicts.
#[derive(Clone, Debug)]
pub enum PanelEdit {
    Rebind { binding: Binding, confirmed: Vec<KeyConflict> },
    ResetCommand { command: String, confirmed: Vec<KeyConflict> },
    MapExtension { extension: String, language: Option<String> },
    MapEntry { id: &'static str, key: String, value: Option<String> },
    Formatter { language: String, field: String, value: SettingValue },
}
impl PanelEdit {
    pub fn apply(&self, doc: &mut SettingsDocument) -> Result<(), SettingsError> {
        match self {
            Self::Rebind { binding, confirmed } => {
                if !confirmed.is_empty() {
                    check_confirmation(&doc.conflicts(binding)?, confirmed)?;
                }
                doc.rebind(binding.clone(), !confirmed.is_empty())
            }
            Self::ResetCommand { command, confirmed } => {
                if !confirmed.is_empty() {
                    check_confirmation(&reset_conflicts(doc, command)?, confirmed)?;
                }
                doc.reset_command(command, !confirmed.is_empty())
            }
            Self::MapExtension { extension, language } => doc.map_extension(extension, language.as_deref()),
            Self::MapEntry { id, key, value } => doc.edit_map_entry(id, key, value.as_deref()),
            Self::Formatter { language, field, value } => doc.edit_formatter(language, field, value.clone()),
        }
    }
    fn conflicts(&self, doc: &SettingsDocument) -> Result<Vec<KeyConflict>, SettingsError> {
        match self {
            Self::Rebind { binding, .. } => doc.conflicts(binding),
            Self::ResetCommand { command, .. } => reset_conflicts(doc, command),
            _ => Ok(Vec::new()),
        }
    }
    fn confirm(&mut self, conflicts: Vec<KeyConflict>) {
        match self {
            Self::Rebind { confirmed, .. } | Self::ResetCommand { confirmed, .. } => *confirmed = conflicts,
            _ => {}
        }
    }
}
fn error(message: impl Into<String>) -> SettingsError {
    SettingsError(message.into())
}
fn check_confirmation(actual: &[KeyConflict], confirmed: &[KeyConflict]) -> Result<(), SettingsError> {
    check_reviewable_conflicts(actual)?;
    if actual == confirmed { Ok(()) } else { Err(error("shortcut conflicts changed; review and confirm again")) }
}
fn check_reviewable_conflicts(conflicts: &[KeyConflict]) -> Result<(), SettingsError> {
    use kwil_core_commands::settings_ui::MAX_REBIND_CHORDS;
    if conflicts
        .iter()
        .any(|conflict| conflict.keys.split_whitespace().take(MAX_REBIND_CHORDS + 1).count() > MAX_REBIND_CHORDS)
    {
        return Err(error("shortcut conflict exceeds eight-chord review limit; use Open settings file"));
    }
    Ok(())
}
fn reset_conflicts(doc: &SettingsDocument, command: &str) -> Result<Vec<KeyConflict>, SettingsError> {
    check_keymap_bound(doc)?;
    let row =
        doc.keymap_rows()?.into_iter().find(|r| r.command.id == command).ok_or_else(|| error("unknown command"))?;
    let mut result = Vec::new();
    for binding in row.defaults {
        for conflict in doc.conflicts(&binding)? {
            if !result.contains(&conflict) {
                result.push(conflict);
            }
        }
    }
    Ok(result)
}

#[derive(Clone, Debug)]
pub struct PanelRow {
    pub title: String,
    pub value: String,
    pub default: String,
    pub description: String,
    pub selected: bool,
    pub command: Option<&'static str>,
    pub source: String,
    pub context: Option<Context>,
    pub mode: Option<Mode>,
    pub conflicts: Vec<KeyConflict>,
    pub add: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditColumn {
    Key,
    Value,
}
#[derive(Clone, Debug)]
pub struct PanelEditText {
    pub key: String,
    pub value: String,
    pub column: EditColumn,
    /// Byte offset at a grapheme boundary in the active column.
    pub cursor: usize,
}
#[derive(Clone, Debug)]
pub struct PanelWindow {
    pub title: &'static str,
    pub rows: Vec<PanelRow>,
    pub total: usize,
    pub offset: usize,
    pub selection: usize,
    pub query: String,
    /// Some("") means waiting for the first chord, including Enter.
    pub capture: Option<String>,
    pub edit_text: Option<PanelEditText>,
    pub confirmation: Vec<KeyConflict>,
    pub confirmation_index: usize,
    pub proposal: Option<String>,
    pub confirmation_page: usize,
}

pub struct ConfirmationViewport {
    pub lines: Vec<String>,
    pub page: usize,
    pub pages: usize,
}
impl PanelWindow {
    /// Wrap every field before paging, so even a narrow card can expose all
    /// values rather than silently clipping the command or proposed shortcut.
    pub fn confirmation_viewport(&self, width: usize, rows: usize) -> Option<ConfirmationViewport> {
        let proposal = self.proposal.as_ref()?;
        let mut fields = vec![format!("Proposed: {proposal}")];
        if let Some(conflict) = self.confirmation.get(self.confirmation_index) {
            fields.extend([
                format!("Conflict {}/{}", self.confirmation_index + 1, self.confirmation.len()),
                // Display parsed identity, not an arbitrarily long equivalent
                // spelling from TOML (whitespace, aliases or numeric padding).
                format!(
                    "Keys: {}",
                    conflict.keys.parse::<KeySequence>().map_or_else(
                        |_| "Invalid shortcut; repair settings file".into(),
                        |sequence| sequence
                            .0
                            .iter()
                            .map(kwil_core_commands::settings_ui::chord_name)
                            .collect::<Vec<_>>()
                            .join(" ")
                    )
                ),
                format!("Command: {}", conflict.command),
                format!("Kind: {:?}", conflict.kind),
            ]);
        } else {
            fields.push("No conflicts remain; Enter applies, Esc cancels".into());
        }
        let width = width.clamp(1, 4096);
        let rows = rows.clamp(1, 4096);
        let mut lines = Vec::new();
        for field in fields {
            let text = crate::palette_session::display_text(&field);
            let mut line = String::new();
            let mut columns = 0;
            for cell in crate::grid::cells(text.as_bytes(), 4096, 4) {
                if columns + cell.width > width && !line.is_empty() {
                    lines.push(std::mem::take(&mut line));
                    columns = 0;
                }
                line.push_str(&cell.text);
                columns += cell.width;
            }
            lines.push(line);
        }
        let pages = lines.len().div_ceil(rows).max(1);
        let page = self.confirmation_page.min(pages - 1);
        Some(ConfirmationViewport { lines: lines.into_iter().skip(page * rows).take(rows).collect(), page, pages })
    }
}

#[derive(Clone)]
enum Target {
    Command(Binding),
    Entry(String),
    Formatter(String, &'static str),
    Add,
}
struct Row {
    view: PanelRow,
    target: Target,
}
#[derive(Default)]
struct Text {
    value: String,
    cursor: usize,
}
impl Text {
    fn new(value: String) -> Result<Self, SettingsError> {
        if value.len() > PANEL_INPUT_LIMIT {
            return Err(error("panel input exceeds 64 KiB; use Open settings file"));
        }
        Ok(Self { cursor: value.len(), value })
    }
    fn align(&mut self) {
        self.cursor =
            self.value.grapheme_indices(true).map(|(i, _)| i).find(|&i| i >= self.cursor).unwrap_or(self.value.len());
    }
    fn input(&mut self, input: PanelInput<'_>, available: usize) -> Result<(), SettingsError> {
        let left = || self.value[..self.cursor].grapheme_indices(true).next_back().map_or(0, |(i, _)| i);
        let right = || self.value[self.cursor..].graphemes(true).next().map_or(self.cursor, |s| self.cursor + s.len());
        match input {
            PanelInput::Text(s) => {
                if s.len() > available.saturating_sub(self.value.len()) {
                    return Err(error("panel input exceeds 64 KiB"));
                }
                self.value.insert_str(self.cursor, s);
                self.cursor += s.len();
                self.align();
            }
            PanelInput::Left => self.cursor = left(),
            PanelInput::Right => self.cursor = right(),
            PanelInput::Home => self.cursor = 0,
            PanelInput::End => self.cursor = self.value.len(),
            PanelInput::Backspace => {
                let start = left();
                self.value.replace_range(start..self.cursor, "");
                self.cursor = start;
                self.align();
            }
            PanelInput::Delete => {
                self.value.replace_range(self.cursor..right(), "");
                self.align();
            }
            _ => {}
        }
        Ok(())
    }
}
struct EntryEdit {
    target: Target,
    key: Text,
    value: Text,
    column: EditColumn,
}
enum State {
    Browse,
    Capture { binding: Binding, keys: Vec<KeyChord> },
    Edit(EntryEdit),
    Confirm(PanelEdit, Vec<KeyConflict>),
}

pub struct SettingsPanelController {
    panel: SettingsPanel,
    query: Text,
    selection: usize,
    state: State,
    confirmation_index: usize,
    confirmation_page: usize,
}
impl SettingsPanelController {
    pub fn new(panel: SettingsPanel, doc: &SettingsDocument) -> Result<Self, SettingsError> {
        let result = Self {
            panel,
            query: Text::default(),
            selection: 0,
            state: State::Browse,
            confirmation_index: 0,
            confirmation_page: 0,
        };
        result.rows(doc)?;
        Ok(result)
    }
    pub fn panel(&self) -> SettingsPanel {
        self.panel
    }
    pub fn selected(&self) -> usize {
        self.selection
    }
    /// Select an absolute row with one bounded validation/filter pass.
    pub fn select(&mut self, doc: &SettingsDocument, index: usize) -> Result<(), SettingsError> {
        if self.is_editing() {
            return Err(error("finish or cancel the panel edit before selecting another row"));
        }
        self.selection = index.min(self.rows(doc)?.len().saturating_sub(1));
        Ok(())
    }
    pub fn query(&self) -> &str {
        &self.query.value
    }
    /// Byte offset at a grapheme boundary in `query()`.
    pub fn query_cursor(&self) -> usize {
        self.query.cursor
    }
    pub fn is_capturing(&self) -> bool {
        matches!(self.state, State::Capture { .. })
    }
    pub fn is_editing(&self) -> bool {
        !matches!(self.state, State::Browse)
    }
    pub fn is_confirming(&self) -> bool {
        matches!(self.state, State::Confirm(_, _))
    }
    pub fn confirmation(&self) -> &[KeyConflict] {
        match &self.state {
            State::Confirm(_, conflicts) => conflicts,
            _ => &[],
        }
    }
    pub fn search(&mut self, doc: &SettingsDocument, query: &str) -> Result<(), SettingsError> {
        let text = Text::new(query.into())?;
        if !query.is_empty() {
            self.rows(doc)?;
        }
        self.query = text;
        self.selection = 0;
        self.state = State::Browse;
        Ok(())
    }
    pub fn window(&self, doc: &SettingsDocument, offset: usize, max_rows: usize) -> Result<PanelWindow, SettingsError> {
        if max_rows > PANEL_ROW_LIMIT {
            return Err(error("panel viewport exceeds row limit"));
        }
        let rows = self.rows(doc)?;
        let selection = self.selection.min(rows.len().saturating_sub(1));
        let offset = offset.min(rows.len());
        let capture = match &self.state {
            State::Capture { keys, .. } => Some(chord_names(keys)?),
            _ => None,
        };
        let edit_text = match &self.state {
            State::Edit(edit) => Some(PanelEditText {
                key: edit.key.value.clone(),
                value: edit.value.value.clone(),
                column: edit.column,
                cursor: if edit.column == EditColumn::Key { edit.key.cursor } else { edit.value.cursor },
            }),
            _ => None,
        };
        let confirmation = match &self.state {
            State::Confirm(_, c) => c.clone(),
            _ => Vec::new(),
        };
        Ok(PanelWindow {
            title: match self.panel {
                SettingsPanel::Keymap => "Keymap",
                SettingsPanel::FileTypes => "File types",
                SettingsPanel::Formatters => "Formatters",
                SettingsPanel::StringMap { .. } => "Mappings",
            },
            total: rows.len(),
            offset,
            selection,
            query: self.query.value.clone(),
            capture,
            edit_text,
            confirmation,
            confirmation_index: self.confirmation_index,
            confirmation_page: self.confirmation_page,
            proposal: match &self.state {
                State::Confirm(PanelEdit::Rebind { binding, .. }, _) => {
                    Some(format!("{} <- {} [{:?}/{:?}]", binding.command, binding.keys, binding.context, binding.mode))
                }
                State::Confirm(PanelEdit::ResetCommand { command, .. }, _) => {
                    Some(format!("Reset {command} to preset"))
                }
                _ => None,
            },
            rows: rows
                .into_iter()
                .enumerate()
                .skip(offset)
                .take(max_rows)
                .map(|(i, mut r)| {
                    r.view.selected = i == selection;
                    if let Target::Command(binding) = &r.target
                        && !binding.keys.is_empty()
                    {
                        r.view.conflicts = doc.conflicts(binding)?;
                    }
                    Ok(r.view)
                })
                .collect::<Result<Vec<_>, SettingsError>>()?,
        })
    }
    pub fn input(&mut self, doc: &SettingsDocument, input: PanelInput<'_>) -> Result<Option<PanelEdit>, SettingsError> {
        if matches!(input, PanelInput::Cancel)
            || matches!(input, PanelInput::Capture(k) if k.key == Key::Named(NamedKey::Escape) && k.modifiers == Default::default())
        {
            self.state = State::Browse;
            return Ok(None);
        }
        // Cancellation must remain available even after an external settings
        // change makes this bounded panel impossible to construct.
        let rows = self.rows(doc)?;
        self.selection = self.selection.min(rows.len().saturating_sub(1));
        match &mut self.state {
            State::Confirm(edit, approved) => {
                if matches!(input, PanelInput::Next | PanelInput::Previous | PanelInput::Home | PanelInput::End) {
                    self.confirmation_page = 0;
                }
                match input {
                    PanelInput::ReviewPage { index, pages } => {
                        if pages == 0 || pages > PANEL_ROW_LIMIT {
                            return Err(error("invalid confirmation page count"));
                        }
                        self.confirmation_page = index.min(pages - 1);
                    }
                    PanelInput::Next => {
                        self.confirmation_index = (self.confirmation_index + 1).min(approved.len().saturating_sub(1))
                    }
                    PanelInput::Previous => self.confirmation_index = self.confirmation_index.saturating_sub(1),
                    PanelInput::Home => self.confirmation_index = 0,
                    PanelInput::End => self.confirmation_index = approved.len().saturating_sub(1),
                    _ => {}
                }
                if matches!(input, PanelInput::Accept) {
                    let actual = edit.conflicts(doc)?;
                    check_reviewable_conflicts(&actual)?;
                    if actual != *approved {
                        *approved = actual;
                        self.confirmation_index = 0;
                        self.confirmation_page = 0;
                        return Err(error("shortcut conflicts changed; review and confirm again"));
                    }
                    let mut edit = edit.clone();
                    edit.confirm(actual);
                    let mut candidate = doc.clone();
                    edit.apply(&mut candidate)?;
                    self.rows(&candidate)?;
                    self.state = State::Browse;
                    return Ok(Some(edit));
                }
            }
            State::Capture { binding, keys } => {
                let chord = match input {
                    PanelInput::Capture(key) => Some(key),
                    PanelInput::Accept if keys.is_empty() => Some("enter".parse().expect("static chord")),
                    PanelInput::Accept => None,
                    _ => return Ok(None),
                };
                if let Some(chord) = chord {
                    // Frontends may route Enter as Capture throughout capture.
                    if chord.key == Key::Named(NamedKey::Enter)
                        && !keys.is_empty()
                        && chord.modifiers == Default::default()
                    {
                        // Accept the first chord.
                    } else {
                        if keys.len() == 2 {
                            return Err(error("shortcut sequence is limited to two chords"));
                        }
                        keys.push(chord);
                        if keys.len() < 2 {
                            return Ok(None);
                        }
                    }
                }
                let mut binding = binding.clone();
                binding.keys = chord_names(keys)?;
                return self.propose(doc, PanelEdit::Rebind { binding, confirmed: Vec::new() });
            }
            State::Edit(edit) => match input {
                PanelInput::Next | PanelInput::Previous => {
                    edit.column = match edit.column {
                        EditColumn::Key => EditColumn::Value,
                        EditColumn::Value => EditColumn::Key,
                    }
                }
                PanelInput::Accept if edit.column == EditColumn::Key => edit.column = EditColumn::Value,
                PanelInput::Accept => {
                    let action = entry_action(self.panel, edit)?;
                    return self.propose(doc, action);
                }
                _ => {
                    if edit.column == EditColumn::Key {
                        edit.key.input(input, PANEL_INPUT_LIMIT.saturating_sub(edit.value.value.len()))?;
                    } else {
                        edit.value.input(input, PANEL_INPUT_LIMIT.saturating_sub(edit.key.value.len()))?;
                    }
                }
            },
            State::Browse => match input {
                PanelInput::Next => self.selection = (self.selection + 1).min(rows.len().saturating_sub(1)),
                PanelInput::Previous => self.selection = self.selection.saturating_sub(1),
                PanelInput::Accept => {
                    if let Some(row) = rows.into_iter().nth(self.selection) {
                        self.state = match row.target {
                            Target::Command(binding) => State::Capture { binding, keys: Vec::new() },
                            target => {
                                let key = if matches!(target, Target::Add) { String::new() } else { row.view.title };
                                let value = if matches!(target, Target::Add) { String::new() } else { row.view.value };
                                if key.len().saturating_add(value.len()) > PANEL_INPUT_LIMIT {
                                    return Err(error("panel input exceeds 64 KiB"));
                                }
                                State::Edit(EntryEdit {
                                    column: if matches!(target, Target::Add) {
                                        EditColumn::Key
                                    } else {
                                        EditColumn::Value
                                    },
                                    target,
                                    key: Text::new(key)?,
                                    value: Text::new(value)?,
                                })
                            }
                        };
                    }
                }
                PanelInput::Delete => {
                    if let Some(row) = rows.into_iter().nth(self.selection) {
                        let edit = match row.target {
                            Target::Command(binding) => {
                                PanelEdit::ResetCommand { command: binding.command, confirmed: Vec::new() }
                            }
                            Target::Entry(key) => match self.panel {
                                SettingsPanel::FileTypes => PanelEdit::MapExtension { extension: key, language: None },
                                SettingsPanel::StringMap { id } => PanelEdit::MapEntry { id, key, value: None },
                                _ => return Err(error("unsupported entry removal")),
                            },
                            Target::Formatter(_, _) => {
                                return Err(error("formatter field removal is unsupported; use Open settings file"));
                            }
                            Target::Add => return Ok(None),
                        };
                        return self.propose(doc, edit);
                    }
                }
                _ => {
                    self.query.input(input, PANEL_INPUT_LIMIT)?;
                    self.selection = 0;
                }
            },
        }
        Ok(None)
    }
    fn propose(&mut self, doc: &SettingsDocument, edit: PanelEdit) -> Result<Option<PanelEdit>, SettingsError> {
        // Always attempt the backend's unconfirmed operation first.
        let mut candidate = doc.clone();
        match edit.apply(&mut candidate) {
            Ok(()) => {
                self.rows(&candidate)?;
                self.state = State::Browse;
                Ok(Some(edit))
            }
            Err(failure) => {
                let conflicts = edit.conflicts(doc)?;
                if conflicts.is_empty() {
                    return Err(failure);
                }
                check_reviewable_conflicts(&conflicts)?;
                self.confirmation_index = 0;
                self.confirmation_page = 0;
                self.state = State::Confirm(edit, conflicts);
                Ok(None)
            }
        }
    }
    fn rows(&self, doc: &SettingsDocument) -> Result<Vec<Row>, SettingsError> {
        #[cfg(test)]
        ROW_BUILDS.with(|count| count.set(count.get() + 1));
        let mut rows = Vec::new();
        match self.panel {
            SettingsPanel::Keymap => {
                check_keymap_bound(doc)?;
                for row in doc.keymap_rows()? {
                    let defaults = row
                        .defaults
                        .iter()
                        .map(|b| format!("{} [{:?}/{:?}]", b.keys, b.context, b.mode))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let current = if row.current.is_empty() {
                        vec![Binding { command: row.command.id.into(), keys: String::new(), context: None, mode: None }]
                    } else {
                        row.current
                    };
                    for binding in current {
                        let source = if binding.keys.is_empty() {
                            "Unbound"
                        } else if doc.settings().bindings.iter().rev().any(|b| b == &binding) {
                            "User"
                        } else {
                            "Preset"
                        };
                        let mut view = base_row(row.command.title.into(), binding.keys.clone());
                        view.command = Some(row.command.id);
                        view.description = format!(
                            "{} · {} · Enter captures, Delete restores defaults",
                            row.command.category, row.command.id
                        );
                        view.default = defaults.clone();
                        view.source = source.into();
                        view.context = binding.context;
                        view.mode = binding.mode;
                        // Conflict details are computed only for the requested window.
                        push(&mut rows, Row { view, target: Target::Command(binding) })?;
                    }
                }
            }
            SettingsPanel::FileTypes => {
                if doc.file_types().len() >= PANEL_ROW_LIMIT {
                    return Err(error("file-type panel exceeds row limit"));
                }
                for (key, value) in doc.file_types() {
                    push(
                        &mut rows,
                        Row { view: base_row(key.clone(), value.clone()), target: Target::Entry(key.clone()) },
                    )?;
                }
            }
            SettingsPanel::StringMap { id } => {
                if !matches!(id, "theme.colors" | "log.highlight_rules") {
                    return Err(error("unsupported string map"));
                }
                let SettingValue::StringMap(map) = doc.current_value(id)? else {
                    return Err(error("not a string map"));
                };
                if map.len() >= PANEL_ROW_LIMIT {
                    return Err(error("mapping panel exceeds row limit"));
                }
                for (key, value) in map {
                    push(&mut rows, Row { view: base_row(key.clone(), value), target: Target::Entry(key) })?;
                }
            }
            SettingsPanel::Formatters => {
                if doc.settings().formatters.len() > (PANEL_ROW_LIMIT - 1) / 4 {
                    return Err(error("formatter panel exceeds row limit"));
                }
                for (language, formatter) in &doc.settings().formatters {
                    for (field, value, default) in [
                        ("executable", formatter.executable.clone(), "".into()),
                        ("args", encode_args(&formatter.args), "".into()),
                        ("range_args", encode_args(&formatter.range_args), "".into()),
                        ("timeout_ms", formatter.timeout_ms.to_string(), "5000".into()),
                    ] {
                        let mut view = base_row(format!("{language}.{field}"), value);
                        view.default = default;
                        view.description = "Args: one per line; \\\\ = backslash, \\n = newline, \\r = CR, \\e = empty arg. Timeout: positive integer".into();
                        push(&mut rows, Row { view, target: Target::Formatter(language.clone(), field) })?;
                    }
                }
            }
        }
        let query = self.query.value.to_lowercase();
        rows.retain(|r| {
            let haystack =
                format!("{} {} {} {}", r.view.title, r.view.value, r.view.command.unwrap_or(""), r.view.description)
                    .to_lowercase();
            query.split_whitespace().all(|q| haystack.contains(q))
        });
        if self.panel != SettingsPanel::Keymap {
            let mut view = base_row("Add row".into(), String::new());
            view.add = true;
            view.description = match self.panel {
                SettingsPanel::FileTypes => "Extension → language",
                SettingsPanel::Formatters => {
                    "language.executable, language.args, language.range_args or language.timeout_ms"
                }
                _ => "Key → value",
            }
            .into();
            push(&mut rows, Row { view, target: Target::Add })?;
        }
        Ok(rows)
    }
}
fn check_keymap_bound(doc: &SettingsDocument) -> Result<(), SettingsError> {
    if kwil_core_commands::COMMANDS.len() > PANEL_ROW_LIMIT || doc.settings().bindings.len() > PANEL_ROW_LIMIT {
        Err(error("keymap panel exceeds row limit; use Open settings file"))
    } else {
        Ok(())
    }
}
fn push(rows: &mut Vec<Row>, row: Row) -> Result<(), SettingsError> {
    if rows.len() == PANEL_ROW_LIMIT {
        return Err(error("panel exceeds row limit; use Open settings file"));
    }
    rows.push(row);
    Ok(())
}
fn base_row(title: String, value: String) -> PanelRow {
    PanelRow {
        title,
        value,
        default: String::new(),
        description: "Enter edits, Escape cancels, Delete removes".into(),
        selected: false,
        command: None,
        source: "User".into(),
        context: None,
        mode: None,
        conflicts: Vec::new(),
        add: false,
    }
}
fn chord_names(keys: &[KeyChord]) -> Result<String, SettingsError> {
    if keys.is_empty() {
        return Ok(String::new());
    }
    let mut capture = KeyCapture::default();
    for key in keys {
        capture.press(*key)?;
    }
    capture.finish()
}
fn entry_action(panel: SettingsPanel, edit: &EntryEdit) -> Result<PanelEdit, SettingsError> {
    let key = edit.key.value.clone();
    let value = edit.value.value.clone();
    // Existing selectors remain stable. Renaming is an explicit Add + Delete,
    // avoiding an implicit destructive overwrite of a commented entry.
    if !matches!(edit.target, Target::Add) {
        let original = match &edit.target {
            Target::Entry(key) => key.clone(),
            Target::Formatter(language, field) => format!("{language}.{field}"),
            _ => unreachable!(),
        };
        if key != original {
            return Err(error("use Add row and Delete to rename an entry"));
        }
    }
    Ok(match panel {
        SettingsPanel::FileTypes => PanelEdit::MapExtension { extension: key, language: Some(value) },
        SettingsPanel::StringMap { id } => PanelEdit::MapEntry { id, key, value: Some(value) },
        SettingsPanel::Formatters => {
            let (language, field) = key.rsplit_once('.').unwrap_or((&key, "executable"));
            let value = match field {
                "executable" => SettingValue::Text(value),
                "args" | "range_args" => SettingValue::Strings(decode_args(&value)?),
                "timeout_ms" => {
                    SettingValue::Integer(value.parse().map_err(|_| error("formatter timeout must be an integer"))?)
                }
                _ => return Err(error("formatter field must be executable, args, range_args or timeout_ms")),
            };
            PanelEdit::Formatter { language: language.into(), field: field.into(), value }
        }
        SettingsPanel::Keymap => return Err(error("keymap requires key capture")),
    })
}
pub(crate) fn encode_args(args: &[String]) -> String {
    args.iter()
        .map(|arg| {
            if arg.is_empty() {
                return "\\e".into();
            }
            arg.replace('\\', "\\\\").replace('\r', "\\r").replace('\n', "\\n")
        })
        .collect::<Vec<_>>()
        .join("\n")
}
pub(crate) fn decode_args(text: &str) -> Result<Vec<String>, SettingsError> {
    if text.is_empty() {
        return Ok(Vec::new());
    }
    text.split('\n')
        .map(|line| {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if line == "\\e" {
                return Ok(String::new());
            }
            let mut arg = String::new();
            let mut chars = line.chars();
            while let Some(ch) = chars.next() {
                if ch != '\\' {
                    arg.push(ch);
                    continue;
                }
                arg.push(match chars.next() {
                    Some('\\') => '\\',
                    Some('n') => '\n',
                    Some('r') => '\r',
                    _ => return Err(error("argument escape must be \\\\, \\n, \\r or a whole-line \\e")),
                });
            }
            Ok(arg)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_crlf_lists_preserve_explicit_control_escapes() {
        assert_eq!(decode_args("*.rs\r\n*.toml").unwrap(), ["*.rs", "*.toml"]);
        let values = vec!["literal\r".into(), "line\nbreak".into(), "C:\\tools".into(), String::new()];
        assert_eq!(decode_args(&encode_args(&values).replace('\n', "\r\n")).unwrap(), values);
    }

    #[test]
    fn settings_capture_accepts_modified_escape() {
        let doc = SettingsDocument::parse("").unwrap();
        for chord in ["shift+escape", "ctrl+escape"] {
            let mut panel = SettingsPanelController::new(SettingsPanel::Keymap, &doc).unwrap();
            press(&mut panel, &doc, PanelInput::Accept);
            press(&mut panel, &doc, key(chord));
            assert_eq!(panel.window(&doc, 0, 1).unwrap().capture.as_deref(), Some(chord));
            press(&mut panel, &doc, key("escape"));
            assert!(!panel.is_capturing());
        }
    }

    fn press(panel: &mut SettingsPanelController, doc: &SettingsDocument, input: PanelInput<'_>) -> Option<PanelEdit> {
        panel.input(doc, input).unwrap()
    }
    fn key(name: &str) -> PanelInput<'_> {
        PanelInput::Capture(name.parse().unwrap())
    }
    fn selected(panel: &SettingsPanelController, doc: &SettingsDocument) -> PanelRow {
        let window = panel.window(doc, 0, PANEL_ROW_LIMIT).unwrap();
        window.rows.into_iter().find(|r| r.selected).unwrap()
    }
    fn stage_add(panel: &mut SettingsPanelController, doc: &SettingsDocument, name: &str, value: &str) -> PanelEdit {
        // Filter hides existing entries while leaving the explicit Add row.
        panel.search(doc, "no existing row matches this").unwrap();
        assert!(selected(panel, doc).add);
        assert!(press(panel, doc, PanelInput::Accept).is_none());
        assert!(press(panel, doc, PanelInput::Text(name)).is_none());
        assert!(press(panel, doc, PanelInput::Accept).is_none());
        assert!(press(panel, doc, PanelInput::Text(value)).is_none());
        press(panel, doc, PanelInput::Accept).unwrap()
    }

    #[test]
    fn capture_preserves_selector_first_enter_and_second_chord_rules() {
        let mut doc = SettingsDocument::parse(
            "[[bindings]]\nkeys='alt+f12'\ncommand='file.open'\ncontext='palette'\nmode='insert'\n",
        )
        .unwrap();
        let mut panel = SettingsPanelController::new(SettingsPanel::Keymap, &doc).unwrap();
        panel.search(&doc, "alt+f12").unwrap();
        let row = selected(&panel, &doc);
        assert_eq!((row.context, row.mode, row.source.as_str()), (Some(Context::Palette), Some(Mode::Insert), "User"));
        press(&mut panel, &doc, PanelInput::Accept);
        press(&mut panel, &doc, PanelInput::Accept); // first chord really is Enter
        assert_eq!(panel.window(&doc, 0, 1).unwrap().capture.as_deref(), Some("enter"));
        assert!(press(&mut panel, &doc, PanelInput::Accept).is_none());
        assert!(panel.confirmation().iter().any(|c| c.command == "palette.accept"));
        let edit = press(&mut panel, &doc, PanelInput::Accept).unwrap();
        let PanelEdit::Rebind { binding, .. } = &edit else {
            panic!("expected binding");
        };
        assert_eq!(
            (binding.context, binding.mode, binding.keys.as_str()),
            (Some(Context::Palette), Some(Mode::Insert), "enter")
        );
        edit.apply(&mut doc).unwrap();
        panel.search(&doc, "file.open").unwrap();
        // Select the scoped binding after the global preset binding if present.
        let scoped = panel
            .window(&doc, 0, PANEL_ROW_LIMIT)
            .unwrap()
            .rows
            .iter()
            .position(|r| r.context == Some(Context::Palette))
            .unwrap();
        for _ in 0..scoped {
            press(&mut panel, &doc, PanelInput::Next);
        }
        press(&mut panel, &doc, PanelInput::Accept);
        press(&mut panel, &doc, key("alt+f11"));
        let edit = press(&mut panel, &doc, key("alt+f10")).unwrap();
        let PanelEdit::Rebind { binding, .. } = edit else {
            panic!("expected binding");
        };
        assert_eq!(binding.keys, "alt+f11 alt+f10");
        assert!(!panel.is_capturing());
        press(&mut panel, &doc, PanelInput::Accept);
        press(&mut panel, &doc, key("alt+f9"));
        assert!(press(&mut panel, &doc, key("escape")).is_none());
        assert!(!panel.is_editing());
    }

    #[test]
    fn settings_confirmation_refuses_sequences_beyond_backend_edit_limit() {
        let raw_keys = format!("ctrl+shift+f11 {} ctrl+shift+f10", "alt+f9 ".repeat(600));
        let source = format!("[[bindings]]\nkeys='{raw_keys}'\ncommand='file.open'\n");
        let doc = SettingsDocument::parse(&source).unwrap();
        let mut panel = SettingsPanelController::new(SettingsPanel::Keymap, &doc).unwrap();
        panel.search(&doc, "file.save").unwrap();
        press(&mut panel, &doc, PanelInput::Accept);
        press(&mut panel, &doc, key("ctrl+shift+f11"));
        let result = panel.input(&doc, PanelInput::Accept);
        assert!(result.is_err(), "an over-limit conflict must not enter a truncated confirmation");
        assert!(!panel.is_confirming());
        assert_eq!(doc.source(), source, "refusal must preserve the original binding");
        press(&mut panel, &doc, PanelInput::Cancel);
        assert!(!panel.is_capturing());
    }

    #[test]
    fn settings_confirmation_pages_preserve_long_fields_in_compact_cards() {
        let raw_keys = format!("ctrl+shift+f{}11{}ctrl+shift+f10", "0".repeat(5000), " ".repeat(5000));
        let doc = SettingsDocument::parse(&format!("[[bindings]]\nkeys='{raw_keys}'\ncommand='file.open'\n")).unwrap();
        let mut panel = SettingsPanelController::new(SettingsPanel::Keymap, &doc).unwrap();
        panel.search(&doc, "file.save").unwrap();
        press(&mut panel, &doc, PanelInput::Accept);
        press(&mut panel, &doc, key("ctrl+shift+f11"));
        press(&mut panel, &doc, key("ctrl+shift+f10"));
        assert!(panel.is_confirming());
        for width in [10, 38, 77] {
            for rows in [2, 14] {
                let first = panel.window(&doc, 0, 0).unwrap().confirmation_viewport(width, rows).unwrap();
                let mut visible = String::new();
                for page in 0..first.pages {
                    press(&mut panel, &doc, PanelInput::ReviewPage { index: page, pages: first.pages });
                    let viewport = panel.window(&doc, 0, 0).unwrap().confirmation_viewport(width, rows).unwrap();
                    assert_eq!(viewport.page, page);
                    assert!(viewport.lines.len() <= rows);
                    for line in viewport.lines {
                        assert!(
                            crate::grid::cells(line.as_bytes(), 4096, 4).iter().map(|cell| cell.width).sum::<usize>()
                                <= width
                        );
                        visible.push_str(&line);
                    }
                }
                assert!(visible.contains("file.save <- ctrl+shift+f11 ctrl+shift+f10"));
                assert!(visible.contains("Keys: ctrl+shift+f11 ctrl+shift+f10"));
                assert!(visible.contains("Command: file.open"));
                assert!(visible.contains("Kind: Exact"));
            }
        }
        press(&mut panel, &doc, PanelInput::Home);
        assert_eq!(panel.window(&doc, 0, 0).unwrap().confirmation_page, 0);
    }

    #[test]
    fn settings_confirmation_exposes_proposal_and_navigates_each_conflict() {
        let doc = SettingsDocument::parse("[[bindings]]\nkeys='ctrl+c ctrl+x'\ncommand='file.open'\n").unwrap();
        let mut panel = SettingsPanelController::new(SettingsPanel::Keymap, &doc).unwrap();
        panel.search(&doc, "file.save").unwrap();
        press(&mut panel, &doc, PanelInput::Accept);
        press(&mut panel, &doc, key("ctrl+c"));
        press(&mut panel, &doc, PanelInput::Accept);
        let window = panel.window(&doc, 0, 1).unwrap();
        assert!(window.confirmation.len() >= 2);
        assert!(window.proposal.as_deref().unwrap().contains("file.save <- ctrl+c"));
        assert_eq!(window.confirmation_index, 0);
        press(&mut panel, &doc, PanelInput::Next);
        assert_eq!(panel.window(&doc, 0, 0).unwrap().confirmation_index, 1);
        press(&mut panel, &doc, PanelInput::End);
        assert_eq!(panel.window(&doc, 0, 0).unwrap().confirmation_index, window.confirmation.len() - 1);
        press(&mut panel, &doc, PanelInput::Home);
        assert_eq!(panel.window(&doc, 0, 0).unwrap().confirmation_index, 0);
        assert_eq!(doc.settings().bindings[0].keys, "ctrl+c ctrl+x", "review navigation cannot apply changes");
    }

    #[test]
    fn conflict_replace_and_reset_require_confirmation_and_recheck() {
        let mut doc = SettingsDocument::parse("").unwrap();
        let mut panel = SettingsPanelController::new(SettingsPanel::Keymap, &doc).unwrap();
        panel.search(&doc, "edit.copy").unwrap();
        press(&mut panel, &doc, PanelInput::Accept);
        press(&mut panel, &doc, key("ctrl+s"));
        assert!(press(&mut panel, &doc, PanelInput::Accept).is_none());
        assert!(panel.is_confirming());
        assert!(panel.confirmation().iter().any(|c| c.command == "file.save"));
        assert_eq!(doc.source(), "");
        assert!(press(&mut panel, &doc, PanelInput::Text("yes")).is_none());
        let edit = press(&mut panel, &doc, PanelInput::Accept).unwrap();
        let mut changed = doc.clone();
        changed
            .rebind(Binding { keys: "ctrl+s".into(), command: "file.open".into(), context: None, mode: None }, true)
            .unwrap();
        let before = changed.source().to_owned();
        assert!(edit.apply(&mut changed).is_err());
        assert_eq!(changed.source(), before);
        edit.apply(&mut doc).unwrap();
        panel.search(&doc, "file.save").unwrap();
        assert!(press(&mut panel, &doc, PanelInput::Delete).is_none());
        assert!(panel.confirmation().iter().any(|c| c.command == "edit.copy"));
        let reset = press(&mut panel, &doc, PanelInput::Accept).unwrap();
        assert!(matches!(reset, PanelEdit::ResetCommand { .. }));
        reset.apply(&mut doc).unwrap();
        assert_eq!(
            doc.settings().keymap().unwrap().resolve(
                &"ctrl+s".parse::<kwil_core_commands::KeySequence>().unwrap().0,
                Context::Editor,
                Mode::Normal
            ),
            kwil_core_commands::Resolution::Command("file.save")
        );
    }

    #[test]
    fn staged_mapping_validates_and_preserves_commented_entries() {
        let mut doc = SettingsDocument::parse("[file_types]\n'*.RS'='rust' # keep\n").unwrap();
        let mut panel = SettingsPanelController::new(SettingsPanel::FileTypes, &doc).unwrap();
        let before = doc.source().to_owned();
        assert!(panel.input(&doc, PanelInput::Delete).is_err());
        assert_eq!(doc.source(), before);
        let edit = stage_add(&mut panel, &doc, "*.py", "python");
        assert_eq!(doc.source(), before, "staging must not mutate the supplied document");
        edit.apply(&mut doc).unwrap();
        assert_eq!(doc.language_for_extension("PY"), Some("python"));
        assert!(doc.source().contains("# keep"));
        panel.search(&doc, "python").unwrap();
        press(&mut panel, &doc, PanelInput::Delete).unwrap().apply(&mut doc).unwrap();
        assert_eq!(doc.language_for_extension("py"), None);
        panel.search(&doc, "no match").unwrap();
        press(&mut panel, &doc, PanelInput::Accept);
        press(&mut panel, &doc, PanelInput::Text("bad/extension"));
        press(&mut panel, &doc, PanelInput::Accept);
        press(&mut panel, &doc, PanelInput::Text("rust"));
        assert!(panel.input(&doc, PanelInput::Accept).is_err());
        assert!(panel.is_editing());
    }

    #[test]
    fn formatter_argv_and_string_map_use_typed_backend_edits() {
        let mut doc = SettingsDocument::parse("").unwrap();
        let mut panel = SettingsPanelController::new(SettingsPanel::Formatters, &doc).unwrap();
        stage_add(&mut panel, &doc, "rust.executable", r"C:\tools\rustfmt.exe").apply(&mut doc).unwrap();
        let args = vec![r"C:\one two\source.rs".into(), "line\nnext\r".into(), String::new(), "\\e".into()];
        stage_add(&mut panel, &doc, "rust.args", &encode_args(&args)).apply(&mut doc).unwrap();
        assert_eq!(doc.settings().formatters["rust"].args, args);
        assert_eq!(doc.settings().formatters["rust"].executable, r"C:\tools\rustfmt.exe");
        panel.search(&doc, "rust.args").unwrap();
        press(&mut panel, &doc, PanelInput::Accept);
        press(&mut panel, &doc, PanelInput::Accept).unwrap().apply(&mut doc).unwrap();
        assert_eq!(doc.settings().formatters["rust"].args, args, "unchanged edit preserves argv exactly");
        let mut map = SettingsPanelController::new(SettingsPanel::StringMap { id: "theme.colors" }, &doc).unwrap();
        stage_add(&mut map, &doc, "editor.background", "#123456").apply(&mut doc).unwrap();
        assert_eq!(doc.settings().theme.colors["editor.background"], "#123456");
        assert!(SettingsPanelController::new(SettingsPanel::StringMap { id: "unknown" }, &doc).is_err());
    }

    #[test]
    fn grapheme_cursor_paste_limits_and_window_search_are_lossless() {
        let doc = SettingsDocument::parse("").unwrap();
        let mut panel = SettingsPanelController::new(SettingsPanel::FileTypes, &doc).unwrap();
        press(&mut panel, &doc, PanelInput::Accept);
        press(&mut panel, &doc, PanelInput::Text("a👩‍💻e\u{301}"));
        press(&mut panel, &doc, PanelInput::Backspace);
        press(&mut panel, &doc, PanelInput::Left);
        press(&mut panel, &doc, PanelInput::Delete);
        assert_eq!(panel.window(&doc, 0, 1).unwrap().edit_text.unwrap().key, "a");
        let before = panel.window(&doc, 0, 1).unwrap().edit_text.unwrap();
        assert!(panel.input(&doc, PanelInput::Text(&"x".repeat(PANEL_INPUT_LIMIT))).is_err());
        assert_eq!(panel.window(&doc, 0, 1).unwrap().edit_text.unwrap().key, before.key);
        press(&mut panel, &doc, PanelInput::Cancel);
        let mut keys = SettingsPanelController::new(SettingsPanel::Keymap, &doc).unwrap();
        for query in ["Open File", "file.open", "ctrl+o"] {
            keys.search(&doc, query).unwrap();
            assert!(keys.window(&doc, 0, 1).unwrap().rows.iter().any(|r| r.command == Some("file.open")));
        }
        let window = keys.window(&doc, 0, 0).unwrap();
        assert!(window.rows.is_empty());
        assert!(window.total > 0);
        keys.search(&doc, "a👩‍💻").unwrap();
        assert_eq!(keys.query_cursor(), keys.query().len());
        press(&mut keys, &doc, PanelInput::Home);
        assert_eq!(keys.query_cursor(), 0);
        press(&mut keys, &doc, PanelInput::Right);
        assert_eq!(keys.query_cursor(), 1);
        press(&mut keys, &doc, PanelInput::Right);
        assert_eq!(keys.query_cursor(), keys.query().len());
        press(&mut keys, &doc, PanelInput::Left);
        assert_eq!(keys.query_cursor(), 1);
        assert!(keys.window(&doc, 0, PANEL_ROW_LIMIT + 1).is_err());
        let mut too_many = String::from("[file_types]\n");
        for i in 0..PANEL_ROW_LIMIT {
            too_many.push_str(&format!("x{i}='rust'\n"));
        }
        let large = SettingsDocument::parse(&too_many).unwrap();
        assert!(
            SettingsPanelController::new(SettingsPanel::FileTypes, &large).is_err(),
            "never silently truncate entries"
        );
    }
}
