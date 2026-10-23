//! Settings window: a desktop panel over the shared settings host. Persistence,
//! drafts, key capture and conflict confirmation live in `SettingsSession`;
//! this module lays the window out in DPI pixels, draws it with the shared
//! desktop widgets and routes keys and clicks to the host's real actions.
//!
//! Geometry is recorded while drawing (`View::hits`), so a click always hits
//! what is on screen. Every row target carries its schema id or a row
//! fingerprint and is re-checked before it acts; a click that arrives after the
//! model changed is dropped and the window redrawn.
use super::*;
use chrome::Metrics;
use desktop::{Colors, Field, Hits, Icon, Paint, Sizes, Style, Surface, inset};
use kwil_core_commands::settings_ui::{CATEGORIES, Category, SettingKind, SettingValue, SettingsDocument, SettingsRow};
use kwil_core_commands::theme::Theme;
use kwil_view_model::settings_panels::{ConfirmationViewport, EditColumn, PanelInput, PanelRow, PanelWindow};
use kwil_view_model::settings_session::{SettingsInput, SettingsPanel, SettingsSession};

/// Everything the panel can click. Row targets name their schema id (scalar
/// rows) or a fingerprint of the drawn row (compound rows).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Target {
    Close,
    OpenFile,
    Search,
    ClearSearch,
    Category(Option<Category>),
    CategoryStep {
        backwards: bool,
    },
    Back,
    Row {
        index: usize,
        id: &'static str,
    },
    Toggle {
        index: usize,
        id: &'static str,
    },
    Choice {
        index: usize,
        id: &'static str,
        value: &'static str,
    },
    CycleChoice {
        index: usize,
        id: &'static str,
    },
    Edit {
        index: usize,
        id: &'static str,
    },
    Reset {
        index: usize,
        id: &'static str,
    },
    EditAccept,
    EditCancel,
    PanelRow {
        index: usize,
        print: u64,
    },
    PanelChange {
        index: usize,
        print: u64,
    },
    PanelRemove {
        index: usize,
        print: u64,
    },
    EntryColumn(EditColumn),
    EntryAccept,
    EntryCancel,
    CaptureAccept,
    CaptureCancel,
    ConfirmAccept,
    ConfirmCancel,
    ConflictStep {
        forward: bool,
    },
    ReviewPage {
        index: usize,
        pages: usize,
    },
    /// Entry row of a page hosted inside the settings window.
    OpenHosted,
    /// Opaque target owned by the hosted page (constructed by `filetypes_ui`).
    #[allow(dead_code)]
    Hosted(u32),
}

impl Target {
    fn row(self) -> Option<usize> {
        match self {
            Self::Row { index, .. }
            | Self::Toggle { index, .. }
            | Self::Choice { index, .. }
            | Self::CycleChoice { index, .. }
            | Self::Edit { index, .. }
            | Self::Reset { index, .. } => Some(index),
            _ => None,
        }
    }
    fn panel_row(self) -> Option<usize> {
        match self {
            Self::PanelRow { index, .. } | Self::PanelChange { index, .. } | Self::PanelRemove { index, .. } => {
                Some(index)
            }
            _ => None,
        }
    }
}

/// A page that lives in the settings window but is owned by another host
/// (file-type registration). It only appears once a real backend is attached.
pub(super) trait HostedPage {
    fn title(&self) -> &'static str;
    /// One-line state for its entry row ("Not registered", "12 types …").
    fn summary(&self) -> String;
    /// Search words that list the entry row.
    fn keywords(&self) -> &'static str;
    #[allow(clippy::too_many_arguments)]
    fn draw(
        &mut self,
        buf: &mut [u32],
        stride: usize,
        font: &mut font::UiFont,
        content: Rect,
        c: &Colors,
        s: &Sizes,
        hover: Option<u32>,
        hits: &mut Hits<Target>,
    );
}

/// Panel view state owned by the frontend: scroll positions, hover and the
/// geometry of the last paint. Selection, drafts and pages stay in the host.
#[derive(Default)]
pub(super) struct View {
    scroll: usize,
    panel_scroll: usize,
    /// Selection the last paint revealed; a new one scrolls into view.
    shown: Option<(Option<SettingsPanel>, usize)>,
    list: (usize, usize),
    pub hover: Option<Target>,
    hits: Hits<Target>,
    pub area: Option<Rect>,
    /// Settings epoch of the last paint. Clicks against older geometry are dropped.
    epoch: u64,
    /// Columns and rows the pending confirmation was wrapped with.
    review: Option<(usize, usize)>,
    /// Caret of the focused field, for IME candidate placement.
    pub caret: Option<Rect>,
    /// The hosted page is showing instead of the setting list.
    pub hosted: bool,
    /// Window size and monitor scale of the last paint.
    surface: Option<Surface>,
    /// Card-only repaint (hover, wheel).
    pub paint: Paint,
    combo: Option<Combo>,
}

#[derive(Clone, Debug)]
struct Combo {
    index: usize,
    id: &'static str,
    choices: &'static [&'static str],
    selected: usize,
    anchor: Rect,
    epoch: u64,
}

impl View {
    #[cfg(feature = "frame-harness")]
    pub(super) fn has_internal_close(&self) -> bool {
        self.hits.iter().any(|(_, target)| *target == Target::Close)
    }
    #[cfg(feature = "frame-harness")]
    pub(super) fn choice_area(&self, id: &str, value: Option<&str>) -> Option<Rect> {
        self.hits.iter().find_map(|(rect, target)| match (*target, value) {
            (Target::CycleChoice { id: row, .. }, None) if row == id => Some(*rect),
            (Target::Choice { id: row, value: choice, .. }, Some(value)) if row == id && choice == value => Some(*rect),
            _ => None,
        })
    }
    fn choice_for(&self, source: &dyn Source, index: usize, id: &'static str, epoch: u64) -> Option<Combo> {
        let row = source.rows().get(index)?.clone();
        if row.spec.id != id {
            return None;
        }
        let SettingKind::Choice(choices) = row.spec.kind else { return None };
        let target = Target::CycleChoice { index, id };
        let anchor = self.hits.iter().find_map(|(rect, hit)| (*hit == target).then_some(*rect))?;
        let selected = choices.iter().position(|value| row.current == SettingValue::Text((*value).into())).unwrap_or(0);
        Some(Combo { index, id, choices, selected, anchor, epoch })
    }
    fn target(&self, point: (usize, usize)) -> Option<Target> {
        self.hits.at(point)
    }

    /// Pointer motion over the card; true when the hover changed.
    fn track(&mut self, pointer: (usize, usize)) -> bool {
        let hover = self.area.filter(|area| area.contains(pointer)).and_then(|_| self.target(pointer));
        if hover == self.hover {
            return false;
        }
        self.hover = hover;
        // No body restore: the card repaints idempotently in place.
        self.paint.request();
        true
    }

    /// Wheel over the list or a compound page: scrolls the painted window by
    /// three rows; true when the rows (and so the hit map) must be repainted.
    fn wheel(&mut self, up: bool, compound: bool) -> bool {
        let (visible, total) = self.list;
        let scroll = if compound { &mut self.panel_scroll } else { &mut self.scroll };
        let next = desktop::wheel(*scroll, up, visible, total);
        if next == *scroll {
            return false;
        }
        *scroll = next;
        self.hover = None;
        self.paint.request();
        true
    }
}

/// What a left click may do against the last paint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Admit {
    /// Inside the card on no control.
    Ignore,
    /// The geometry predates the latest input, a worker adoption, a resize
    /// or a DPI change: repaint instead of guessing what is under the pointer.
    Stale,
    Act(Target),
}

fn admit(view: &View, epoch: u64, surface: Surface, pointer: (usize, usize)) -> Admit {
    let Some(area) = view.area else { return Admit::Ignore };
    if view.surface != Some(surface) {
        return Admit::Stale;
    }
    let target = if area.contains(pointer) {
        match view.target(pointer) {
            Some(target) => target,
            None => return Admit::Ignore,
        }
    } else {
        Target::Close
    };
    if view.epoch != epoch {
        return Admit::Stale;
    }
    Admit::Act(target)
}

/// Identity of the document the window shows. A worker event that changes it
/// (a clean external reload adopted without a keypress) must fence painted
/// clicks exactly like Ctrl+R adoption; a save acknowledgement of the same
/// text, or a reload parked beside a draft, leaves it unchanged.
fn shown_document(document: Option<&SettingsDocument>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    document.map(SettingsDocument::source).hash(&mut hasher);
    hasher.finish()
}

/// Read side of the settings host. `SettingsSession` is the production source;
/// tests use the same core menu and panel controller without a worker.
pub(super) trait Source {
    fn loaded(&self) -> bool;
    fn error(&self) -> Option<&str>;
    fn status(&self) -> &str;
    fn category(&self) -> Option<Category>;
    fn rows(&self) -> Vec<SettingsRow>;
    fn selected(&self) -> usize;
    fn searching(&self) -> bool;
    fn query(&self) -> &str;
    /// Cursor of the active scalar input: the draft while editing, else the query.
    fn cursor(&self) -> usize;
    fn draft(&self) -> Option<&str>;
    fn panel(&self) -> Option<SettingsPanel>;
    fn window(&self, offset: usize, max: usize) -> Result<Option<PanelWindow>, String>;
    fn panel_cursor(&self) -> usize;
    fn panel_editing(&self) -> bool;
}

impl Source for SettingsSession {
    fn loaded(&self) -> bool {
        self.menu().is_some()
    }
    fn error(&self) -> Option<&str> {
        self.last_error()
    }
    fn status(&self) -> &str {
        SettingsSession::status(self)
    }
    fn category(&self) -> Option<Category> {
        SettingsSession::category(self)
    }
    fn rows(&self) -> Vec<SettingsRow> {
        SettingsSession::rows(self)
    }
    fn selected(&self) -> usize {
        SettingsSession::selected(self)
    }
    fn searching(&self) -> bool {
        self.is_searching()
    }
    fn query(&self) -> &str {
        self.search_text()
    }
    fn cursor(&self) -> usize {
        self.input_cursor()
    }
    fn draft(&self) -> Option<&str> {
        self.is_editing().then(|| self.input_text())
    }
    fn panel(&self) -> Option<SettingsPanel> {
        SettingsSession::panel(self)
    }
    fn window(&self, offset: usize, max: usize) -> Result<Option<PanelWindow>, String> {
        self.panel_window(offset, max)
    }
    fn panel_cursor(&self) -> usize {
        self.panel_query_cursor()
    }
    fn panel_editing(&self) -> bool {
        self.panel_is_editing()
    }
}

/// Host state that decides which clicks may act.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct State {
    searching: bool,
    editing: bool,
    query_empty: bool,
    panel: Option<SettingsPanel>,
    panel_editing: bool,
    capturing: bool,
    confirming: bool,
    column: Option<EditColumn>,
    panel_query_empty: bool,
    hosted: bool,
}

fn state(source: &dyn Source, hosted: bool) -> State {
    let window = source.window(0, 0).ok().flatten();
    State {
        searching: source.searching(),
        editing: source.draft().is_some(),
        query_empty: source.query().is_empty(),
        panel: source.panel(),
        panel_editing: source.panel_editing(),
        capturing: window.as_ref().is_some_and(|w| w.capture.is_some()),
        confirming: window.as_ref().is_some_and(|w| w.proposal.is_some()),
        column: window.as_ref().and_then(|w| w.edit_text.as_ref()).map(|e| e.column),
        panel_query_empty: window.as_ref().is_none_or(|w| w.query.is_empty()),
        hosted,
    }
}

/// One host call. A click becomes a short, fixed sequence of these.
#[derive(Clone, Copy, Debug)]
enum Step {
    Close,
    OpenFile,
    Settings(SettingsInput<'static>),
    Panel(PanelInput<'static>),
    Select { index: usize, id: &'static str },
    PanelSelect { index: usize, print: u64 },
    Category(Option<Category>),
    CycleCategory(bool),
    Choose { index: usize, id: &'static str, value: &'static str },
    Hosted(bool),
    HostedTarget(u32),
}

/// Clicks never change the destination of a scalar draft, capture, staged
/// key/value edit or pending conflict confirmation; only the controls of that
/// draft act while it is open.
fn plan(target: Target, s: &State) -> Vec<Step> {
    let drafting = s.editing || s.panel_editing;
    let scalar = s.panel.is_none() && !s.hosted;
    // Leaving search keeps the query; it must precede an activation, because
    // Accept while searching only ends the search.
    let activate = |index, id| {
        let mut steps = Vec::with_capacity(3);
        if s.searching {
            steps.push(Step::Settings(SettingsInput::Accept));
        }
        steps.extend([Step::Select { index, id }, Step::Settings(SettingsInput::Accept)]);
        steps
    };
    match target {
        Target::Close if !drafting => vec![Step::Close],
        Target::OpenFile if !drafting => vec![Step::OpenFile],
        Target::Search if scalar && !drafting && !s.searching => vec![Step::Settings(SettingsInput::Search)],
        Target::ClearSearch if !drafting && !s.hosted => match s.panel {
            Some(_) if !s.panel_query_empty => vec![Step::Panel(PanelInput::Cancel)],
            None if s.searching => vec![Step::Settings(SettingsInput::Cancel)],
            None if !s.query_empty => {
                vec![Step::Settings(SettingsInput::Search), Step::Settings(SettingsInput::Cancel)]
            }
            _ => Vec::new(),
        },
        Target::Category(category) if !drafting => {
            let mut steps = vec![Step::Category(category)];
            if s.hosted {
                steps.insert(0, Step::Hosted(false));
            }
            steps
        }
        Target::CategoryStep { backwards } if !drafting && !s.hosted => vec![Step::CycleCategory(backwards)],
        Target::Back if s.hosted => vec![Step::Hosted(false)],
        Target::Back if !drafting && s.panel.is_some() => {
            // Escape first clears the panel query, then leaves the panel.
            if s.panel_query_empty {
                vec![Step::Panel(PanelInput::Cancel)]
            } else {
                vec![Step::Panel(PanelInput::Cancel), Step::Panel(PanelInput::Cancel)]
            }
        }
        Target::Row { index, id } if scalar && !drafting => vec![Step::Select { index, id }],
        Target::Toggle { index, id } | Target::CycleChoice { index, id } | Target::Edit { index, id }
            if scalar && !drafting =>
        {
            activate(index, id)
        }
        Target::Choice { index, id, value } if scalar && !drafting => {
            vec![Step::Select { index, id }, Step::Choose { index, id, value }]
        }
        Target::Reset { index, id } if scalar && !drafting => {
            vec![Step::Select { index, id }, Step::Settings(SettingsInput::Reset)]
        }
        Target::EditAccept if scalar && s.editing => vec![Step::Settings(SettingsInput::Accept)],
        Target::EditCancel if scalar && s.editing => vec![Step::Settings(SettingsInput::Cancel)],
        Target::PanelRow { index, print } if s.panel.is_some() && !s.panel_editing => {
            vec![Step::PanelSelect { index, print }]
        }
        Target::PanelChange { index, print } if s.panel.is_some() && !s.panel_editing => {
            vec![Step::PanelSelect { index, print }, Step::Panel(PanelInput::Accept)]
        }
        Target::PanelRemove { index, print } if s.panel.is_some() && !s.panel_editing => {
            vec![Step::PanelSelect { index, print }, Step::Panel(PanelInput::Delete)]
        }
        Target::EntryColumn(column) if s.column.is_some_and(|current| current != column) => {
            vec![Step::Panel(PanelInput::Next)]
        }
        Target::EntryAccept if s.column.is_some() => vec![Step::Panel(PanelInput::Accept)],
        Target::EntryCancel if s.column.is_some() => vec![Step::Panel(PanelInput::Cancel)],
        Target::CaptureAccept if s.capturing => vec![Step::Panel(PanelInput::Accept)],
        Target::CaptureCancel if s.capturing => vec![Step::Panel(PanelInput::Cancel)],
        Target::ConfirmAccept if s.confirming => vec![Step::Panel(PanelInput::Accept)],
        Target::ConfirmCancel if s.confirming => vec![Step::Panel(PanelInput::Cancel)],
        Target::ConflictStep { forward } if s.confirming => {
            vec![Step::Panel(if forward { PanelInput::Next } else { PanelInput::Previous })]
        }
        Target::ReviewPage { index, pages } if s.confirming => {
            vec![Step::Panel(PanelInput::ReviewPage { index, pages })]
        }
        Target::OpenHosted if scalar && !drafting => vec![Step::Hosted(true)],
        Target::Hosted(code) if s.hosted => vec![Step::HostedTarget(code)],
        _ => Vec::new(),
    }
}

fn fingerprint(row: &PanelRow) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (&row.title, &row.value, row.command, row.add).hash(&mut hasher);
    hasher.finish()
}

fn finish_adoption(
    result: Result<(), String>,
    epoch: &mut u64,
    composing: &mut bool,
    destination: &mut Option<ime::Destination>,
) -> Result<(), String> {
    result?;
    *epoch = epoch.wrapping_add(1);
    *destination = Some(ime::Destination::Cancelled);
    *composing = false;
    Ok(())
}

/// A worker event that replaced the shown document is an adoption: same epoch,
/// IME and composition fencing as Ctrl+R (`finish_adoption`).
fn fence_reload(
    before: u64,
    after: u64,
    epoch: &mut u64,
    composing: &mut bool,
    destination: &mut Option<ime::Destination>,
) {
    if before != after {
        let _ = finish_adoption(Ok(()), epoch, composing, destination);
    }
}

/// Wrap width (cells of the widest Latin advance) and rows of a confirmation
/// box; keyboard paging reuses exactly the values the paint used.
fn confirmation_view(window: &PanelWindow, columns: usize, rows: usize) -> Option<ConfirmationViewport> {
    window.confirmation_viewport(columns.max(1), rows.max(1))
}

fn confirmation_page_input(
    window: &PanelWindow,
    columns: usize,
    rows: usize,
    forward: bool,
) -> Option<PanelInput<'static>> {
    let viewport = confirmation_view(window, columns, rows)?;
    let index = if forward {
        viewport.page.saturating_add(1).min(viewport.pages.saturating_sub(1))
    } else {
        viewport.page.saturating_sub(1)
    };
    Some(PanelInput::ReviewPage { index, pages: viewport.pages })
}

#[derive(Clone, Copy)]
struct KeyMode {
    compound: bool,
    capturing: bool,
    editing: bool,
    searching: bool,
    confirming: bool,
    repeat: bool,
}
enum KeyRoute<'a> {
    Ignore,
    Panel(PanelInput<'a>),
    Settings(SettingsInput<'a>),
    Paste,
    OpenFile,
    AdoptExternal,
    Category(bool),
    ReviewPage(bool),
}

fn key_route<'a>(
    key: &Key,
    text: Option<&'a str>,
    mods: winit::keyboard::ModifiersState,
    composing: bool,
    mode: KeyMode,
) -> KeyRoute<'a> {
    // A held key cannot finish capture, accept a proposal, or advance an
    // edit through multiple Enter stages. Ordinary text/deletion may repeat.
    if mode.repeat
        && (mode.capturing
            || mode.confirming
            || matches!(key, Key::Named(NamedKey::Enter | NamedKey::Escape | NamedKey::Tab)))
    {
        return KeyRoute::Ignore;
    }
    // Capture owns *all* chords before application hotkeys or text input.
    if mode.capturing {
        return input::chord(key, mods).map_or(KeyRoute::Ignore, |chord| KeyRoute::Panel(PanelInput::Capture(chord)));
    }
    if composing {
        return KeyRoute::Ignore;
    }
    let command = mods.control_key() || mods.super_key();
    if command && let Key::Character(key) = key {
        match key.to_ascii_lowercase().as_str() {
            "v" => return KeyRoute::Paste,
            "o" => return KeyRoute::OpenFile,
            "r" => return KeyRoute::AdoptExternal,
            "s" if !mode.compound => return KeyRoute::Settings(SettingsInput::Save),
            _ => {}
        }
    }
    if matches!(key, Key::Named(NamedKey::Tab)) {
        return if mode.compound && mode.editing {
            KeyRoute::Panel(if mods.shift_key() { PanelInput::Previous } else { PanelInput::Next })
        } else if !mode.editing {
            KeyRoute::Category(mods.shift_key())
        } else {
            KeyRoute::Ignore
        };
    }
    if mode.compound {
        let input = match key {
            Key::Named(NamedKey::ArrowLeft | NamedKey::PageUp) if mode.confirming => {
                return KeyRoute::ReviewPage(false);
            }
            Key::Named(NamedKey::ArrowRight | NamedKey::PageDown) if mode.confirming => {
                return KeyRoute::ReviewPage(true);
            }
            Key::Named(NamedKey::Escape) if !mode.editing => return KeyRoute::Settings(SettingsInput::Cancel),
            Key::Named(NamedKey::Escape) => Some(PanelInput::Cancel),
            Key::Named(NamedKey::Enter) if mode.editing && mods.shift_key() => Some(PanelInput::Text("\n")),
            Key::Named(NamedKey::Enter) => Some(PanelInput::Accept),
            Key::Named(NamedKey::ArrowDown) if !mode.editing => Some(PanelInput::Next),
            Key::Named(NamedKey::ArrowUp) if !mode.editing => Some(PanelInput::Previous),
            Key::Named(NamedKey::Backspace) => Some(PanelInput::Backspace),
            Key::Named(NamedKey::Delete) => Some(PanelInput::Delete),
            Key::Named(NamedKey::ArrowLeft) => Some(PanelInput::Left),
            Key::Named(NamedKey::ArrowRight) => Some(PanelInput::Right),
            Key::Named(NamedKey::Home) => Some(PanelInput::Home),
            Key::Named(NamedKey::End) => Some(PanelInput::End),
            _ if !command || mods.alt_key() => text.filter(|s| !s.chars().any(char::is_control)).map(PanelInput::Text),
            _ => None,
        };
        return input.map_or(KeyRoute::Ignore, KeyRoute::Panel);
    }
    let text_mode = mode.editing || mode.searching;
    let input = match key {
        Key::Named(NamedKey::Escape) => Some(SettingsInput::Cancel),
        Key::Named(NamedKey::Enter) => Some(SettingsInput::Accept),
        Key::Named(NamedKey::ArrowDown) if !mode.editing => Some(SettingsInput::Next),
        Key::Named(NamedKey::ArrowUp) if !mode.editing => Some(SettingsInput::Previous),
        Key::Named(NamedKey::Backspace) if text_mode => Some(SettingsInput::Backspace),
        Key::Named(NamedKey::Delete) => Some(if text_mode { SettingsInput::Delete } else { SettingsInput::Reset }),
        Key::Named(NamedKey::ArrowLeft) if text_mode => Some(SettingsInput::Left),
        Key::Named(NamedKey::ArrowRight) if text_mode => Some(SettingsInput::Right),
        Key::Named(NamedKey::Home) if text_mode => Some(SettingsInput::Home),
        Key::Named(NamedKey::End) if text_mode => Some(SettingsInput::End),
        Key::Character(key) if !text_mode && key == " " => Some(SettingsInput::Accept),
        Key::Character(key) if !text_mode && key == "/" => Some(SettingsInput::Search),
        _ if !command || mods.alt_key() => text.filter(|s| !s.chars().any(char::is_control)).map(SettingsInput::Text),
        _ => None,
    };
    input.map_or(KeyRoute::Ignore, KeyRoute::Settings)
}

/// Compact display of a value inside a field or summary.
pub(super) fn value_text(value: &SettingValue) -> String {
    match value {
        SettingValue::Bool(value) => if *value { "On" } else { "Off" }.into(),
        SettingValue::Integer(value) => value.to_string(),
        // Floating schema fields are stored as f32 font preferences.
        SettingValue::Float(value) => (*value as f32).to_string(),
        SettingValue::Text(value) => desktop::visible(value, desktop::SCALARS),
        SettingValue::Strings(values) => {
            values.iter().take(32).map(|s| desktop::visible(s, 64)).collect::<Vec<_>>().join(", ")
        }
        SettingValue::StringMap(values) => count(values.len(), "entry", "entries"),
        SettingValue::Bindings(values) => count(values.len(), "custom shortcut", "custom shortcuts"),
        SettingValue::Formatters(values) => count(values.len(), "formatter", "formatters"),
    }
}

fn count(n: usize, one: &str, many: &str) -> String {
    match n {
        0 => format!("No {many}"),
        1 => format!("1 {one}"),
        n => format!("{n} {many}"),
    }
}

/// Human labels for the schema's stored choice values.
fn choice_label(value: &str) -> &str {
    match value {
        "vscode" => "VS Code",
        "notepad++" => "Notepad++",
        "vim" => "Vim",
        "horizontal" => "Horizontal",
        "vertical" => "Vertical",
        "stable" => "Stable",
        "prerelease" => "Prerelease",
        "auto" => "Automatic",
        "on" => "On",
        "off" => "Off",
        "dark" => "Dark",
        "light" => "Light",
        "cpu" => "CPU",
        "gpu" => "GPU",
        other => other,
    }
}

fn panel_labels(panel: SettingsPanel) -> (&'static str, &'static str, &'static str) {
    match panel {
        SettingsPanel::Keymap => ("Shortcut", "Command", "shortcuts"),
        SettingsPanel::FileTypes => ("Extension", "Language", "mappings"),
        SettingsPanel::Formatters => ("Setting", "Value", "fields"),
        SettingsPanel::StringMap { id: "theme.colors" } => ("Colour role", "Colour", "overrides"),
        SettingsPanel::StringMap { id: "log.highlight_rules" } => ("Pattern", "Colour", "rules"),
        SettingsPanel::StringMap { .. } => ("Key", "Value", "entries"),
    }
}

/// Window regions in physical pixels. All sizes are monitor DIPs; editor zoom
/// and the editor font never take part.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Frame {
    card: Rect,
    header: Rect,
    nav: Option<Rect>,
    search: Rect,
    stepper: Option<Rect>,
    content: Rect,
    footer: Rect,
}

fn frame(size: (usize, usize), m: &Metrics, s: &Sizes) -> Option<Frame> {
    let (w, h) = size;
    // Header, footer, category bar, page heading and one stacked row.
    if w < m.dip(300.0) || h < m.dip(360.0) {
        return None;
    }
    let card = Rect { x: (size.0 - w) / 2, y: (size.1 - h) / 2, w, h };
    let inner = card;
    // The operating system owns the only title bar and close button.
    let header = Rect { x: inner.x, y: inner.y, w: inner.w, h: 0 };
    let footer_h = m.dip(54.0);
    let footer = Rect { x: inner.x, y: inner.y + inner.h - footer_h, w: inner.w, h: footer_h };
    let top = header.y + header.h;
    let body = footer.y.saturating_sub(top + s.hairline);
    let p = s.gap + s.gap / 2;
    let nav_w = m.dip(236.0);
    // The rail needs every category row; otherwise a compact stepper is used.
    let rail_h = p + s.control + p + (CATEGORIES.len() + 1) * s.nav + p;
    if w >= m.dip(700.0) && body >= rail_h {
        let nav = Rect { x: inner.x, y: top, w: nav_w, h: body };
        let search = Rect { x: nav.x + p, y: nav.y + p, w: nav.w - 2 * p, h: s.control };
        let content = Rect { x: nav.x + nav.w + s.hairline, y: top, w: inner.w - nav.w - s.hairline, h: body };
        return Some(Frame { card, header, nav: Some(nav), search, stepper: None, content, footer });
    }
    let bar = s.control + 2 * p;
    let stepper_w = m.dip(220.0).min(inner.w * 2 / 5);
    let search = Rect { x: inner.x + p, y: top + p, w: inner.w - 3 * p - stepper_w, h: s.control };
    let stepper = Rect { x: search.x + search.w + p, y: search.y, w: stepper_w, h: s.control };
    let content = Rect { x: inner.x, y: top + bar, w: inner.w, h: body.saturating_sub(bar) };
    Some(Frame { card, header, nav: None, search, stepper: Some(stepper), content, footer })
}

/// Rows of one list page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct List {
    heading: Rect,
    rows: Rect,
    row_h: usize,
    stacked: bool,
    visible: usize,
}

fn list(content: Rect, m: &Metrics, s: &Sizes, compound: bool) -> List {
    let stacked = !compound && content.w < m.dip(560.0);
    let small = m.dip(18.0);
    let v = m.dip(12.0);
    let row_h = if compound {
        v + s.text + m.dip(4.0) + s.control + m.dip(8.0)
    } else if stacked {
        v + s.text + 2 * small + s.gap + s.control + v
    } else {
        v + s.text + 2 * small + v
    };
    let heading =
        Rect { x: content.x + s.pad, y: content.y + s.gap, w: content.w.saturating_sub(2 * s.pad), h: s.header };
    let top = heading.y + heading.h;
    let rows = Rect {
        x: content.x + s.pad / 2,
        y: top,
        w: content.w.saturating_sub(s.pad),
        h: (content.y + content.h).saturating_sub(top + s.gap),
    };
    List { heading, rows, row_h, stacked, visible: (rows.h / row_h.max(1)).max(1) }
}

/// Paints the settings window and records its click targets. Returns the card
/// rectangle (the damage of this paint), or None when the window is too small.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw(
    source: &dyn Source,
    pixels: &mut [u32],
    size: (usize, usize),
    font: &mut font::UiFont,
    theme: &Theme,
    m: &Metrics,
    epoch: u64,
    view: &mut View,
    hosted: Option<&mut dyn HostedPage>,
) -> Option<Rect> {
    let c = Colors::new(theme);
    let s = Sizes::new(m);
    view.hits = Hits::default();
    view.review = None;
    view.caret = None;
    view.epoch = epoch;
    view.surface = Some(Surface::new(size, m));
    let Some(f) = frame(size, m, &s) else {
        view.area = too_small(pixels, size, font, m, &c, &s);
        return view.area;
    };
    view.area = Some(f.card);
    let stride = size.0;
    chrome::fill_clipped(pixels, stride, f.card, c.surface);
    let hover = view.hover;
    let mut hosted = hosted;
    let hosted_open = view.hosted && hosted.is_some();
    if !hosted_open {
        view.hosted = false;
    }
    let editing = source.draft().is_some() || source.panel_editing();
    // Search and categories.
    if let Some(nav) = f.nav {
        chrome::fill_clipped(pixels, stride, nav, c.sidebar);
        desktop::vline(pixels, stride, nav.x + nav.w, nav.y, nav.h, &c, &s);
        let p = s.gap + s.gap / 2;
        let mut y = f.search.y + f.search.h + p;
        for category in std::iter::once(None).chain(CATEGORIES.iter().copied().map(Some)) {
            let rect = Rect { x: nav.x + s.gap, y, w: nav.w - 2 * s.gap, h: s.nav };
            let target = Target::Category(category);
            let selected = source.category() == category && !hosted_open;
            desktop::nav_row(pixels, stride, rect, selected, hover == Some(target) && !editing, &c, &s);
            let glyph=match category {
                Some(Category::Editor|Category::Keymap)=>icons::Icon::Text,
                Some(Category::Appearance)=>if theme.scheme()==kwil_core_commands::theme::Scheme::Dark {icons::Icon::Moon}else{icons::Icon::Sun},
                Some(Category::Files)=>icons::Icon::File,Some(Category::Search)=>icons::Icon::Search,
                Some(Category::Updates)=>icons::Icon::Down,Some(Category::Remote)=>icons::Icon::GoTo,
                _=>icons::Icon::Settings,
            };
            icons::draw(pixels,stride,glyph,((rect.x+s.pad+m.dip(8.0)) as f32,(rect.y+rect.h/2) as f32),m.dip(16.0),if selected {c.fg}else{c.muted});
            let text = Rect { x: rect.x + s.pad+m.dip(24.0), w: rect.w.saturating_sub(s.pad+s.gap+m.dip(24.0)), ..rect };
            let title = if category==Some(Category::Appearance) {"Appearance"}else{category.map_or("All settings", Category::title)};
            desktop::label(pixels, stride, font, text, title, if selected { c.fg } else { c.muted });
            view.hits.push(rect, target);
            y += s.nav;
        }
    }
    if let Some(stepper) = f.stepper {
        let title = source.category().map_or("All settings", Category::title);
        chrome::fill_clipped(pixels, stride, stepper, c.surface);
        chrome::round_rect(pixels, stride, stepper, s.radius, c.border);
        chrome::round_rect(pixels, stride, inset(stepper, s.hairline), s.radius, c.control);
        let arrow = s.control.min(stepper.w / 3);
        let left = Rect { w: arrow, ..stepper };
        let right = Rect { x: stepper.x + stepper.w - arrow, w: arrow, ..stepper };
        for (rect, backwards, icon) in [(left, true, Icon::Back), (right, false, Icon::Forward)] {
            let target = Target::CategoryStep { backwards };
            let hot = hover == Some(target) && !editing;
            let center = (rect.x as f32 + rect.w as f32 / 2.0, rect.y as f32 + rect.h as f32 / 2.0);
            if hot {
                chrome::round_rect(pixels, stride, inset(rect, s.hairline * 2), s.radius, c.control_hover);
            }
            desktop::icon(
                pixels,
                stride,
                icon,
                center,
                s.icon,
                c.fg,
                if hot { c.control_hover } else { c.control },
                &s,
            );
            view.hits.push(rect, target);
        }
        let text = Rect { x: left.x + left.w, w: stepper.w.saturating_sub(2 * arrow), ..stepper };
        let fitted = font.fit(title, text.w);
        let width = font.measure(&fitted, text.w);
        let baseline = font.baseline(text.y, text.h);
        font.draw(pixels, stride, text, text.x + (text.w - width) / 2, baseline, &fitted, c.fg);
    }
    // Content.
    chrome::fill_clipped(pixels, stride, f.content, c.surface);
    let caret = if !source.loaded() {
        loading_page(pixels, stride, font, f.content, source, hover, &c, &s, &mut view.hits);
        None
    } else if let Some(page) = hosted.as_deref_mut().filter(|_| hosted_open) {
        let heading = Rect { x: f.content.x + s.pad, y: f.content.y + s.gap, w: f.content.w - 2 * s.pad, h: s.header };
        page_heading(pixels, stride, font, heading, page.title(), "", true, hover, &c, &s, &mut view.hits);
        let body = Rect {
            x: f.content.x,
            y: heading.y + heading.h,
            w: f.content.w,
            h: (f.content.y + f.content.h).saturating_sub(heading.y + heading.h),
        };
        let code = match hover {
            Some(Target::Hosted(code)) => Some(code),
            _ => None,
        };
        page.draw(pixels, stride, font, body, &c, &s, code, &mut view.hits);
        None
    } else if let Some(kind) = source.panel() {
        panel_page(pixels, stride, font, m, &f, source, kind, view, &c, &s)
    } else {
        scalar_page(pixels, stride, font, m, &f, source, view, hosted.as_deref(), &c, &s)
    };
    // The search field is drawn after the page so its focus follows the page.
    let compound = source.panel().is_some();
    let (query, cursor, focused, placeholder) = if hosted_open {
        ("", 0, false, "Search settings")
    } else if compound {
        let window = source.window(0, 0).ok().flatten();
        let query = window.map(|w| w.query).unwrap_or_default();
        let placeholder = match source.panel() {
            Some(SettingsPanel::Keymap) => "Search shortcuts",
            _ => "Search entries",
        };
        // `query` is owned; draw from it before it drops.
        let clear = !query.is_empty() && !source.panel_editing();
        let field = Field {
            text: &query,
            cursor: source.panel_cursor().min(query.len()),
            placeholder,
            focused: !source.panel_editing(),
            hovered: hover == Some(Target::Search),
            search: true,
            clear,
        };
        let search_caret = desktop::text_field(pixels, stride, font, f.search, field, &c, &s);
        if clear {
            view.hits.push(desktop::field_clear(f.search, &s), Target::ClearSearch);
        }
        view.caret = caret.or(search_caret);
        ("", 0, false, "")
    } else {
        let searching = source.searching() && source.draft().is_none();
        (source.query(), if searching { source.cursor() } else { source.query().len() }, searching, "Search settings")
    };
    if !compound {
        let clear = !hosted_open && !query.is_empty() && !editing;
        let field = Field {
            text: query,
            cursor,
            placeholder,
            focused,
            hovered: hover == Some(Target::Search) && !editing,
            search: true,
            clear,
        };
        let search_caret = desktop::text_field(pixels, stride, font, f.search, field, &c, &s);
        if !hosted_open {
            view.hits.push(f.search, Target::Search);
        }
        if clear {
            view.hits.push(desktop::field_clear(f.search, &s), Target::ClearSearch);
        }
        view.caret = caret.or(search_caret);
    }
    footer(pixels, stride, font, &f, source, hosted_open, view.review.is_some(), &c, &s, view);
    if view.combo.as_ref().is_some_and(|combo| combo.epoch != epoch) {
        view.combo = None;
    }
    if let Some(combo) = &view.combo {
        let row_h = s.control;
        let height = combo.choices.len() * row_h + s.hairline * 2;
        let y = (combo.anchor.y + combo.anchor.h + s.gap / 2).min(size.1.saturating_sub(height + s.gap));
        let area = Rect { x: combo.anchor.x, y, w: combo.anchor.w, h: height };
        desktop::card(pixels, stride, area, &c, &s);
        for (i, value) in combo.choices.iter().copied().enumerate() {
            let rect = Rect {
                x: area.x + s.hairline,
                y: area.y + s.hairline + i * row_h,
                w: area.w.saturating_sub(2 * s.hairline),
                h: row_h,
            };
            let target = Target::Choice { index: combo.index, id: combo.id, value };
            desktop::nav_row(pixels, stride, rect, i == combo.selected, view.hover == Some(target), &c, &s);
            desktop::label(
                pixels,
                stride,
                font,
                Rect { x: rect.x + s.inner, w: rect.w.saturating_sub(2 * s.inner), ..rect },
                choice_label(value),
                c.fg,
            );
            view.hits.push(rect, target);
        }
    }
    Some(f.card)
}

/// A compact card for windows below the minimum panel size. Escape still
/// closes the settings window; nothing else is clickable here.
fn too_small(
    pixels: &mut [u32],
    size: (usize, usize),
    font: &mut font::UiFont,
    m: &Metrics,
    c: &Colors,
    s: &Sizes,
) -> Option<Rect> {
    let w = size.0.saturating_sub(2 * s.gap).min(m.dip(320.0));
    let h = (2 * s.text + 2 * s.pad).min(size.1);
    if w < m.dip(120.0) || h < s.text {
        return None;
    }
    let card = Rect { x: (size.0 - w) / 2, y: (size.1 - h) / 2, w, h };
    desktop::card(pixels, size.0, card, c, s);
    let line = |y| Rect { x: card.x + s.pad, y, w: card.w.saturating_sub(2 * s.pad), h: s.text };
    desktop::label(pixels, size.0, font, line(card.y + s.pad), "Settings need a larger window", c.fg);
    desktop::label(pixels, size.0, font, line(card.y + s.pad + s.text), "Esc closes", c.muted);
    Some(card)
}

#[allow(clippy::too_many_arguments)]
fn footer(
    pixels: &mut [u32],
    stride: usize,
    font: &mut font::UiFont,
    f: &Frame,
    source: &dyn Source,
    _hosted: bool,
    _confirming: bool,
    c: &Colors,
    s: &Sizes,
    view: &mut View,
) {
    let r = f.footer;
    chrome::fill_clipped(pixels, stride, r, c.sidebar);
    desktop::hline(pixels, stride, r.x, r.y, r.w, c, s);
    let editing = source.draft().is_some() || source.panel_editing();
    let label = "Open settings file";
    let button =
        Rect { x: r.x + s.pad, y: r.y + (r.h - s.control) / 2, w: desktop::button_width(font, label, s), h: s.control };
    desktop::button(
        pixels,
        stride,
        font,
        button,
        label,
        Style::Subtle,
        view.hover == Some(Target::OpenFile) && !editing,
        c,
        s,
    );
    if !editing {
        view.hits.push(button, Target::OpenFile);
    }
    let message = if source.status() == "Settings ready" { "Changes are saved automatically" } else { source.status() };
    let status = Rect {
        x: button.x + button.w + s.pad,
        y: r.y,
        w: (r.x + r.w).saturating_sub(button.x + button.w + 2 * s.pad),
        h: r.h,
    };
    desktop::label_right(
        pixels,
        stride,
        font,
        status,
        message,
        if source.error().is_some() { c.error } else { c.muted },
    );
}
#[allow(clippy::too_many_arguments)]
fn page_heading(
    pixels: &mut [u32],
    stride: usize,
    font: &mut font::UiFont,
    r: Rect,
    title: &str,
    detail: &str,
    back: bool,
    hover: Option<Target>,
    c: &Colors,
    s: &Sizes,
    hits: &mut Hits<Target>,
) {
    let mut x = r.x;
    if back {
        let button = Rect { x, y: r.y + (r.h - s.control) / 2, w: s.control, h: s.control };
        desktop::icon_button(pixels, stride, button, Icon::Back, hover == Some(Target::Back), true, c, s);
        hits.push(button, Target::Back);
        x += s.control + s.gap;
    }
    let detail_w = if detail.is_empty() { 0 } else { font.measure(detail, r.w / 3) };
    let title_rect = Rect { x, y: r.y, w: (r.x + r.w).saturating_sub(x + detail_w + s.gap), h: r.h };
    desktop::label(pixels, stride, font, title_rect, title, c.fg);
    // Accent rule under the page title: hierarchy without a second font size.
    let under = Rect { x, y: r.y + r.h - s.focus - s.hairline, w: s.pad * 2, h: s.focus };
    chrome::round_rect(pixels, stride, under, s.focus / 2, c.accent);
    if detail_w > 0 {
        desktop::label_right(pixels, stride, font, Rect { x: r.x + r.w - detail_w, w: detail_w, ..r }, detail, c.muted);
    }
}

#[allow(clippy::too_many_arguments)]
fn loading_page(
    pixels: &mut [u32],
    stride: usize,
    font: &mut font::UiFont,
    content: Rect,
    source: &dyn Source,
    hover: Option<Target>,
    c: &Colors,
    s: &Sizes,
    hits: &mut Hits<Target>,
) {
    let x = content.x + s.pad * 2;
    let w = content.w.saturating_sub(s.pad * 4);
    let mut y = content.y + s.pad * 2;
    match source.error() {
        None => {
            desktop::label(pixels, stride, font, Rect { x, y, w, h: s.text }, "Loading settings…", c.muted);
        }
        Some(error) => {
            desktop::label(pixels, stride, font, Rect { x, y, w, h: s.text }, "Settings could not be loaded", c.fg);
            y += s.text + s.gap;
            for line in desktop::wrap(font, error, w, 4) {
                desktop::label(pixels, stride, font, Rect { x, y, w, h: s.text }, &line, c.error);
                y += s.text;
            }
            y += s.pad;
            let bw = desktop::button_width(font, "Open settings file", s);
            let button = Rect { x, y, w: bw.min(w), h: s.control };
            desktop::button(
                pixels,
                stride,
                font,
                button,
                "Open settings file",
                Style::Primary,
                hover == Some(Target::OpenFile),
                c,
                s,
            );
            hits.push(button, Target::OpenFile);
        }
    }
}

/// The schema list: title, description and a real control per setting.
#[allow(clippy::too_many_arguments)]
fn scalar_page(
    pixels: &mut [u32],
    stride: usize,
    font: &mut font::UiFont,
    m: &Metrics,
    f: &Frame,
    source: &dyn Source,
    view: &mut View,
    hosted: Option<&dyn HostedPage>,
    c: &Colors,
    s: &Sizes,
) -> Option<Rect> {
    let layout = list(f.content, m, s, false);
    let rows = source.rows();
    let query = source.query();
    // The hosted entry row is listed under Files, under All and in matching searches.
    let hosted_row = hosted.filter(|page| {
        source.category().is_none_or(|category| category == Category::Files)
            && query.split_whitespace().all(|word| page.keywords().contains(word.to_lowercase().as_str()))
    });
    let total = rows.len() + usize::from(hosted_row.is_some());
    let title = if !query.is_empty() {
        format!("Results for “{}”", desktop::visible(query, 64))
    } else {
        source.category().map_or("All settings", Category::title).to_owned()
    };
    page_heading(
        pixels,
        stride,
        font,
        layout.heading,
        &title,
        &count(total, "setting", "settings"),
        false,
        view.hover,
        c,
        s,
        &mut view.hits,
    );
    let selected = source.selected().min(rows.len().saturating_sub(1));
    let key = (None, selected);
    if view.shown != Some(key) || view.list.0 != layout.visible {
        view.scroll = desktop::reveal(view.scroll, selected, layout.visible, total);
        view.shown = Some(key);
    }
    view.scroll = view.scroll.min(total.saturating_sub(layout.visible));
    view.list = (layout.visible, total);
    if total == 0 {
        let r = Rect { x: layout.rows.x + s.pad, y: layout.rows.y + s.pad, w: layout.rows.w, h: s.text };
        desktop::label(pixels, stride, font, r, "No settings match this search.", c.muted);
        let r = Rect { y: r.y + s.text, ..r };
        desktop::label(pixels, stride, font, r, "Try another word, or clear the search.", c.faint);
        return None;
    }
    let editing = source.draft();
    let mut caret = None;
    let hover_row = view.hover.and_then(Target::row);
    let width = layout.rows.w.saturating_sub(s.thumb + s.gap);
    for slot in 0..layout.visible {
        let index = view.scroll + slot;
        let r =
            Rect { x: layout.rows.x, y: layout.rows.y + slot * layout.row_h, w: width, h: layout.row_h - s.hairline };
        if index == rows.len() {
            if let Some(page) = hosted_row {
                hosted_entry(pixels, stride, font, r, page, view.hover, editing.is_some(), c, s, &mut view.hits);
            }
            break;
        }
        let Some(row) = rows.get(index) else { break };
        let is_selected = index == selected;
        desktop::row(pixels, stride, r, is_selected, hover_row == Some(index) && editing.is_none(), c, s);
        view.hits.push(r, Target::Row { index, id: row.spec.id });
        let draft = editing.filter(|_| is_selected);
        caret = scalar_row(pixels, stride, font, m, r, layout.stacked, index, row, source, draft, view, c, s).or(caret);
    }
    desktop::scroll_thumb(
        pixels,
        stride,
        Rect { x: layout.rows.x + width, y: layout.rows.y, w: s.thumb + s.gap, h: layout.visible * layout.row_h },
        view.scroll,
        layout.visible,
        total,
        c,
        s,
    );
    caret
}

#[allow(clippy::too_many_arguments)]
fn hosted_entry(
    pixels: &mut [u32],
    stride: usize,
    font: &mut font::UiFont,
    r: Rect,
    page: &dyn HostedPage,
    hover: Option<Target>,
    editing: bool,
    c: &Colors,
    s: &Sizes,
    hits: &mut Hits<Target>,
) {
    desktop::row(pixels, stride, r, false, hover == Some(Target::OpenHosted) && !editing, c, s);
    let text = Rect { x: r.x + s.pad, y: r.y + s.gap + s.gap / 2, w: r.w.saturating_sub(4 * s.pad), h: s.text };
    desktop::label(pixels, stride, font, text, page.title(), c.fg);
    desktop::label(pixels, stride, font, Rect { y: text.y + s.text, ..text }, &page.summary(), c.muted);
    let chevron = (r.x as f32 + r.w as f32 - s.pad as f32 * 1.5, r.y as f32 + r.h as f32 / 2.0);
    desktop::icon(pixels, stride, Icon::Forward, chevron, s.icon, c.muted, c.surface, s);
    hits.push(r, Target::OpenHosted);
}

/// One schema row. Returns the caret of its draft field.
#[allow(clippy::too_many_arguments)]
fn scalar_row(
    pixels: &mut [u32],
    stride: usize,
    font: &mut font::UiFont,
    m: &Metrics,
    r: Rect,
    stacked: bool,
    index: usize,
    row: &SettingsRow,
    source: &dyn Source,
    draft: Option<&str>,
    view: &mut View,
    c: &Colors,
    s: &Sizes,
) -> Option<Rect> {
    let id = row.spec.id;
    let hover = view.hover;
    let busy = source.draft().is_some();
    let modified = row.current != row.default;
    let v = m.dip(12.0);
    let small = m.dip(18.0);
    let left = r.x + s.pad + s.gap;
    let reset_w = if modified && draft.is_none() { desktop::button_width(font, "Reset", s) } else { 0 };
    let (control, reset, text_w) = if stacked {
        let y = r.y + v + s.text + 2 * small + s.gap;
        let reset = Rect { x: r.x + r.w - s.pad - reset_w, y, w: reset_w, h: s.control };
        let w = reset.x.saturating_sub(left + s.gap).min(m.dip(360.0));
        (Rect { x: left, y, w, h: s.control }, reset, r.w.saturating_sub(left - r.x + s.pad))
    } else {
        let w = m.dip(300.0).min(r.w * 2 / 5);
        let control = Rect { x: r.x + r.w - s.pad - w, y: r.y + (r.h - s.control) / 2, w, h: s.control };
        let reset = Rect { x: control.x.saturating_sub(s.gap + reset_w), w: reset_w, ..control };
        let end = if reset_w > 0 { reset.x } else { control.x };
        (control, reset, end.saturating_sub(left + s.pad))
    };
    if modified {
        // A modified value carries an accent dot in front of its title.
        let center = ((r.x + s.pad) as f32, (r.y + v + s.text / 2) as f32);
        chrome::disc(pixels, stride, center, m.dip(3.0) as f32, c.accent);
    }
    let title = Rect { x: left, y: r.y + v, w: text_w, h: s.text };
    let pen = desktop::label(pixels, stride, font, title, row.spec.title, c.fg);
    if source.category().is_none() || !source.query().is_empty() {
        let tag = Rect { x: pen + s.gap, w: (title.x + title.w).saturating_sub(pen + s.gap), ..title };
        desktop::label(pixels, stride, font, tag, row.spec.category.title(), c.faint);
    }
    for (line, text) in desktop::wrap(font, row.spec.description, text_w, 2).iter().enumerate() {
        let y = title.y + s.text + line * small;
        desktop::label(pixels, stride, font, Rect { x: left, y, w: text_w, h: small }, text, c.muted);
    }
    if reset_w > 0 {
        let target = Target::Reset { index, id };
        desktop::button(pixels, stride, font, reset, "Reset", Style::Subtle, hover == Some(target) && !busy, c, s);
        view.hits.push(reset, target);
    }
    match (row.spec.kind, &row.current) {
        (SettingKind::Bool, SettingValue::Bool(on)) => {
            let target = Target::Toggle { index, id };
            desktop::switch(pixels, stride, font, control, *on, hover == Some(target) && !busy, c, s);
            view.hits.push(control, target);
            None
        }
        (SettingKind::Choice(choices), SettingValue::Text(value)) => {
            let _ = choices;
            let target = Target::CycleChoice { index, id };
            desktop::button(
                pixels,
                stride,
                font,
                control,
                choice_label(value),
                Style::Secondary,
                hover == Some(target),
                c,
                s,
            );
            let center = ((control.x + control.w) as f32 - s.pad as f32, control.y as f32 + control.h as f32 / 2.0);
            desktop::icon(pixels, stride, Icon::Down, center, s.icon * 3 / 4, c.muted, c.control, s);
            view.hits.push(control, target);
            None
        }
        (SettingKind::Integer | SettingKind::Float | SettingKind::Text | SettingKind::Strings, value) => {
            if let Some(text) = draft {
                let button = s.control;
                let field = Rect { w: control.w.saturating_sub(2 * (button + s.gap / 2)), ..control };
                let accept = Rect { x: field.x + field.w + s.gap / 2, w: button, ..control };
                let cancel = Rect { x: accept.x + button + s.gap / 2, w: button, ..control };
                let caret = desktop::text_field(
                    pixels,
                    stride,
                    font,
                    field,
                    Field {
                        text,
                        cursor: source.cursor(),
                        placeholder: "",
                        focused: true,
                        hovered: false,
                        search: false,
                        clear: false,
                    },
                    c,
                    s,
                );
                desktop::icon_button(
                    pixels,
                    stride,
                    accept,
                    Icon::Check,
                    hover == Some(Target::EditAccept),
                    true,
                    c,
                    s,
                );
                desktop::icon_button(
                    pixels,
                    stride,
                    cancel,
                    Icon::Close,
                    hover == Some(Target::EditCancel),
                    true,
                    c,
                    s,
                );
                view.hits.push(accept, Target::EditAccept);
                view.hits.push(cancel, Target::EditCancel);
                caret
            } else {
                let target = Target::Edit { index, id };
                let shown = value_text(value);
                let field = Field {
                    text: &shown,
                    cursor: 0,
                    placeholder: "Empty",
                    focused: false,
                    hovered: hover == Some(target) && !busy,
                    search: false,
                    clear: false,
                };
                desktop::text_field(pixels, stride, font, control, field, c, s);
                view.hits.push(control, target);
                None
            }
        }
        (_, value) => {
            // Compound values open their own page.
            let target = Target::Edit { index, id };
            let bw = desktop::button_width(font, "Edit", s).min(control.w);
            let button = Rect { x: control.x + control.w - bw, w: bw, ..control };
            let summary = Rect { w: control.w.saturating_sub(bw + s.gap), ..control };
            desktop::label_right(pixels, stride, font, summary, &value_text(value), c.muted);
            desktop::button(
                pixels,
                stride,
                font,
                button,
                "Edit",
                Style::Secondary,
                hover == Some(target) && !busy,
                c,
                s,
            );
            view.hits.push(button, target);
            None
        }
    }
}

/// Keymap, file-type mapping, formatter and string-map pages.
#[allow(clippy::too_many_arguments)]
fn panel_page(
    pixels: &mut [u32],
    stride: usize,
    font: &mut font::UiFont,
    m: &Metrics,
    f: &Frame,
    source: &dyn Source,
    kind: SettingsPanel,
    view: &mut View,
    c: &Colors,
    s: &Sizes,
) -> Option<Rect> {
    let layout = list(f.content, m, s, true);
    let meta = match source.window(0, 0) {
        Ok(Some(meta)) => meta,
        Ok(None) => return None,
        Err(error) => {
            page_heading(
                pixels,
                stride,
                font,
                layout.heading,
                "Settings page",
                "",
                true,
                view.hover,
                c,
                s,
                &mut view.hits,
            );
            let w = layout.rows.w.saturating_sub(2 * s.pad);
            let mut y = layout.rows.y + s.pad;
            for line in desktop::wrap(font, &error, w, 3) {
                desktop::label(
                    pixels,
                    stride,
                    font,
                    Rect { x: layout.rows.x + s.pad, y, w, h: s.text },
                    &line,
                    c.error,
                );
                y += s.text;
            }
            return None;
        }
    };
    let (key_label, value_label, unit) = panel_labels(kind);
    if meta.proposal.is_some() {
        page_heading(
            pixels,
            stride,
            font,
            layout.heading,
            "Shortcut conflict",
            "",
            false,
            view.hover,
            c,
            s,
            &mut view.hits,
        );
        confirm_sheet(pixels, stride, font, m, layout, &meta, view, c, s);
        return None;
    }
    if let Some(keys) = &meta.capture {
        page_heading(
            pixels,
            stride,
            font,
            layout.heading,
            "Change shortcut",
            "",
            false,
            view.hover,
            c,
            s,
            &mut view.hits,
        );
        capture_sheet(pixels, stride, font, m, layout, source, keys, view, c, s);
        return None;
    }
    if let Some(edit) = &meta.edit_text {
        let title = format!("{} · Edit", meta.title);
        page_heading(pixels, stride, font, layout.heading, &title, "", false, view.hover, c, s, &mut view.hits);
        let help = source
            .window(meta.selection, 1)
            .ok()
            .flatten()
            .and_then(|w| w.rows.into_iter().next())
            .map(|row| row.description)
            .unwrap_or_default();
        return entry_sheet(pixels, stride, font, m, layout, edit, (key_label, value_label), &help, view, c, s);
    }
    let detail = format!("{} {unit}", meta.total.saturating_sub(usize::from(kind != SettingsPanel::Keymap)));
    page_heading(pixels, stride, font, layout.heading, meta.title, &detail, true, view.hover, c, s, &mut view.hits);
    let total = meta.total;
    let key = (Some(kind), meta.selection);
    if view.shown.is_none_or(|(panel, _)| panel != Some(kind)) {
        view.panel_scroll = 0;
    }
    if view.shown != Some(key) || view.list.0 != layout.visible {
        view.panel_scroll = desktop::reveal(view.panel_scroll, meta.selection, layout.visible, total);
        view.shown = Some(key);
    }
    view.panel_scroll = view.panel_scroll.min(total.saturating_sub(layout.visible));
    view.list = (layout.visible, total);
    let window = match source.window(view.panel_scroll, layout.visible) {
        Ok(Some(window)) => window,
        _ => return None,
    };
    let width = layout.rows.w.saturating_sub(s.thumb + s.gap);
    let hover_row = view.hover.and_then(Target::panel_row);
    for (slot, row) in window.rows.iter().enumerate() {
        let index = window.offset + slot;
        let r =
            Rect { x: layout.rows.x, y: layout.rows.y + slot * layout.row_h, w: width, h: layout.row_h - s.hairline };
        let print = fingerprint(row);
        let hovered = hover_row == Some(index);
        desktop::row(pixels, stride, r, row.selected, hovered, c, s);
        view.hits.push(r, Target::PanelRow { index, print });
        panel_row(pixels, stride, font, m, r, kind, index, print, row, row.selected || hovered, view, c, s);
    }
    desktop::scroll_thumb(
        pixels,
        stride,
        Rect { x: layout.rows.x + width, y: layout.rows.y, w: s.thumb + s.gap, h: layout.visible * layout.row_h },
        view.panel_scroll,
        layout.visible,
        total,
        c,
        s,
    );
    None
}

#[allow(clippy::too_many_arguments)]
fn panel_row(
    pixels: &mut [u32],
    stride: usize,
    font: &mut font::UiFont,
    m: &Metrics,
    r: Rect,
    kind: SettingsPanel,
    index: usize,
    print: u64,
    row: &PanelRow,
    actions: bool,
    view: &mut View,
    c: &Colors,
    s: &Sizes,
) {
    let hover = view.hover;
    let v = m.dip(12.0);
    let left = r.x + s.pad + s.gap;
    let right = r.x + r.w - s.pad;
    let line2 = Rect { x: left, y: r.y + v + s.text + m.dip(4.0), w: right.saturating_sub(left), h: s.control };
    // Second line: description, with the row's actions at its right end.
    let mut text_end = right;
    if actions {
        let change = Target::PanelChange { index, print };
        let label = if row.add {
            "Add"
        } else if kind == SettingsPanel::Keymap {
            "Change"
        } else {
            "Edit"
        };
        let remove = match kind {
            _ if row.add => None,
            SettingsPanel::Keymap => Some("Reset"),
            SettingsPanel::Formatters => None,
            _ => Some("Remove"),
        };
        let mut x = right;
        if let Some(text) = remove {
            let target = Target::PanelRemove { index, print };
            let w = desktop::button_width(font, text, s);
            x = x.saturating_sub(w);
            let rect = Rect { x, w, ..line2 };
            let style = if kind == SettingsPanel::Keymap { Style::Subtle } else { Style::Danger };
            desktop::button(pixels, stride, font, rect, text, style, hover == Some(target), c, s);
            view.hits.push(rect, target);
            x = x.saturating_sub(s.gap);
        }
        let w = desktop::button_width(font, label, s);
        x = x.saturating_sub(w);
        let rect = Rect { x, w, ..line2 };
        desktop::button(pixels, stride, font, rect, label, Style::Secondary, hover == Some(change), c, s);
        view.hits.push(rect, change);
        text_end = x.saturating_sub(s.gap);
    }
    let title_y = r.y + v;
    if row.add {
        let title = format!("Add {}", panel_labels(kind).0.to_lowercase());
        desktop::label(
            pixels,
            stride,
            font,
            Rect { x: left, y: title_y, w: right - left, h: s.text },
            &title,
            c.accent,
        );
        let desc = Rect { w: text_end.saturating_sub(left), ..line2 };
        desktop::label(pixels, stride, font, desc, &row.description, c.muted);
        return;
    }
    // First line: title left, current value right.
    let value_w = (r.w * 2 / 5).min(m.dip(360.0));
    let value_rect = Rect { x: right.saturating_sub(value_w), y: title_y, w: value_w, h: s.text };
    let title = Rect { x: left, y: title_y, w: value_rect.x.saturating_sub(left + s.gap), h: s.text };
    let pen = desktop::label(pixels, stride, font, title, &row.title, c.fg);
    if !row.source.is_empty() && row.source != "Preset" {
        let tag = Rect { x: pen + s.gap, w: (title.x + title.w).saturating_sub(pen + s.gap), ..title };
        desktop::label(pixels, stride, font, tag, &row.source, c.faint);
    }
    if kind == SettingsPanel::Keymap {
        if row.value.is_empty() {
            desktop::label_right(pixels, stride, font, value_rect, "Unbound", c.faint);
        } else {
            let w = desktop::keycaps_width(font, &row.value, s, value_rect.w);
            desktop::keycaps(pixels, stride, font, value_rect, value_rect.x + value_rect.w - w, &row.value, c, s);
        }
    } else {
        desktop::label_right(pixels, stride, font, value_rect, &row.value, c.muted);
    }
    let mut desc = Rect { w: text_end.saturating_sub(left), ..line2 };
    if !row.conflicts.is_empty() {
        let text = count(row.conflicts.len(), "conflict", "conflicts");
        let end = desktop::label(pixels, stride, font, desc, &text, c.error);
        desc = Rect { x: end + s.gap, w: (desc.x + desc.w).saturating_sub(end + s.gap), ..desc };
    }
    if desc.w > m.dip(80.0) {
        desktop::label(pixels, stride, font, desc, &row.description, c.muted);
    }
}

/// Bottom-right button pair of a sheet; returns their rects (secondary, primary).
#[allow(clippy::too_many_arguments)]
fn sheet_buttons(
    pixels: &mut [u32],
    stride: usize,
    font: &mut font::UiFont,
    area: Rect,
    secondary: (&str, Target),
    primary: Option<(&str, Target)>,
    hover: Option<Target>,
    c: &Colors,
    s: &Sizes,
    hits: &mut Hits<Target>,
) {
    let y = (area.y + area.h).saturating_sub(s.control);
    let mut x = area.x + area.w;
    if let Some((text, target)) = primary {
        let w = desktop::button_width(font, text, s);
        x = x.saturating_sub(w);
        let r = Rect { x, y, w, h: s.control };
        desktop::button(pixels, stride, font, r, text, Style::Primary, hover == Some(target), c, s);
        hits.push(r, target);
        x = x.saturating_sub(s.gap);
    }
    let (text, target) = secondary;
    let w = desktop::button_width(font, text, s);
    x = x.saturating_sub(w);
    let r = Rect { x, y, w, h: s.control };
    desktop::button(pixels, stride, font, r, text, Style::Secondary, hover == Some(target), c, s);
    hits.push(r, target);
}

fn sheet_area(layout: List, s: &Sizes) -> Rect {
    inset(Rect { h: layout.rows.h, ..layout.rows }, s.pad / 2)
}

/// Bottom action row of a sheet. It is reserved before any content is placed,
/// so no field, help line or capture box can be painted under the buttons.
fn sheet_actions(area: Rect, w: usize, s: &Sizes) -> Rect {
    let h = s.control.min(area.h);
    Rect { x: area.x, y: area.y + area.h - h, w, h }
}

/// One column of the key/value entry sheet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
    /// Label above its field.
    Full {
        label: Rect,
        field: Rect,
    },
    /// The field alone: no room for its label.
    Bare(Rect),
    /// One clickable line naming the unfocused column and its text.
    Summary(Rect),
    Hidden,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EntryLayout {
    key: Slot,
    value: Slot,
    /// Top of the help text and how many of its lines fit.
    help: (usize, usize),
    /// "Shift+Enter adds a line break." when the value is focused and it fits.
    hint: Option<Rect>,
    actions: Rect,
}

/// Places the entry sheet top-down above its reserved action row. When both
/// labelled fields do not fit, the unfocused column shrinks to a one-line
/// summary (click or Tab switches to it), then disappears; help and the hint
/// take only the room that is left. The focused field is always placed.
fn entry_layout(area: Rect, w: usize, s: &Sizes, focused: EditColumn, help: usize, hint: bool) -> EntryLayout {
    let actions = sheet_actions(area, w, s);
    let bottom = actions.y.saturating_sub(s.gap).max(area.y);
    let room = bottom - area.y;
    let labelled = s.text + s.gap / 2 + s.control;
    let full = |y: usize| Slot::Full {
        label: Rect { x: area.x, y, w, h: s.text },
        field: Rect { x: area.x, y: y + s.text + s.gap / 2, w, h: s.control },
    };
    let summary = |y: usize| Slot::Summary(Rect { x: area.x, y, w, h: s.text });
    let key_focused = focused == EditColumn::Key;
    let mut y = area.y;
    let (key, value);
    if 2 * labelled + s.pad <= room {
        key = full(y);
        value = full(y + labelled + s.pad);
        y += 2 * (labelled + s.pad);
    } else if labelled + s.gap + s.text <= room {
        if key_focused {
            key = full(y);
            value = summary(y + labelled + s.gap);
        } else {
            key = summary(y);
            value = full(y + s.text + s.gap);
        }
        y += labelled + s.gap + s.text + s.gap;
    } else {
        let slot = if labelled <= room {
            y += labelled + s.gap;
            full(area.y)
        } else {
            let h = s.control.min(room);
            y += h;
            Slot::Bare(Rect { x: area.x, y: area.y, w, h })
        };
        (key, value) = if key_focused { (slot, Slot::Hidden) } else { (Slot::Hidden, slot) };
    }
    let y = y.min(bottom);
    let lines = help.min((bottom - y) / s.text.max(1));
    let after = y + lines * s.text;
    let hint = (hint && after + s.text <= bottom).then_some(Rect { x: area.x, y: after, w, h: s.text });
    EntryLayout { key, value, help: (y, lines), hint, actions }
}

#[allow(clippy::too_many_arguments)]
fn entry_sheet(
    pixels: &mut [u32],
    stride: usize,
    font: &mut font::UiFont,
    m: &Metrics,
    layout: List,
    edit: &kwil_view_model::settings_panels::PanelEditText,
    labels: (&str, &str),
    help: &str,
    view: &mut View,
    c: &Colors,
    s: &Sizes,
) -> Option<Rect> {
    let area = sheet_area(layout, s);
    let w = area.w.min(m.dip(560.0));
    let help = desktop::wrap(font, help, w, 3);
    let plan = entry_layout(area, w, s, edit.column, help.len(), edit.column == EditColumn::Value);
    let mut caret = None;
    for (column, label, text, slot) in
        [(EditColumn::Key, labels.0, &edit.key, plan.key), (EditColumn::Value, labels.1, &edit.value, plan.value)]
    {
        let focused = edit.column == column;
        let target = Target::EntryColumn(column);
        let rect = match slot {
            Slot::Full { label: line, field } => {
                desktop::label(pixels, stride, font, line, label, c.muted);
                field
            }
            Slot::Bare(field) => field,
            Slot::Summary(line) => {
                let shown = if text.is_empty() { "empty".to_owned() } else { desktop::visible(text, 64) };
                let color = if hover_is(view.hover, target) { c.fg } else { c.muted };
                desktop::label(pixels, stride, font, line, &format!("{label}: {shown}"), color);
                view.hits.push(line, target);
                continue;
            }
            Slot::Hidden => continue,
        };
        let field = Field {
            text,
            cursor: if focused { edit.cursor } else { 0 },
            placeholder: "",
            focused,
            hovered: hover_is(view.hover, target),
            search: false,
            clear: false,
        };
        caret = desktop::text_field(pixels, stride, font, rect, field, c, s).or(caret);
        view.hits.push(rect, target);
    }
    let (top, lines) = plan.help;
    for (index, line) in help.iter().take(lines).enumerate() {
        desktop::label(pixels, stride, font, Rect { x: area.x, y: top + index * s.text, w, h: s.text }, line, c.faint);
    }
    if let Some(hint) = plan.hint {
        desktop::label(pixels, stride, font, hint, "Shift+Enter adds a line break.", c.faint);
    }
    let primary = if edit.column == EditColumn::Key { "Next" } else { "Apply" };
    sheet_buttons(
        pixels,
        stride,
        font,
        plan.actions,
        ("Cancel", Target::EntryCancel),
        Some((primary, Target::EntryAccept)),
        view.hover,
        c,
        s,
        &mut view.hits,
    );
    caret
}

fn hover_is(hover: Option<Target>, target: Target) -> bool {
    hover == Some(target)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CaptureLayout {
    title: Option<Rect>,
    keys: Rect,
    /// Top of the instructions and how many of their lines fit.
    notes: (usize, usize),
    actions: Rect,
}

/// Capture sheet above its reserved action row: the key box keeps at least a
/// control's height and grows to 64 DIP, the command title comes next, then as
/// many instruction lines as fit (the footer always shows "Esc  Cancel").
fn capture_layout(area: Rect, w: usize, m: &Metrics, s: &Sizes, title: bool, notes: usize) -> CaptureLayout {
    let actions = sheet_actions(area, w, s);
    let bottom = actions.y.saturating_sub(s.gap).max(area.y);
    let mut y = area.y;
    let title = if title && bottom - y >= s.text + s.gap + s.control {
        y += s.text + s.gap;
        Some(Rect { x: area.x, y: area.y, w, h: s.text })
    } else {
        None
    };
    let keys = Rect { x: area.x, y, w, h: (bottom - y).min(m.dip(64.0)) };
    let y = (y + keys.h + s.pad).min(bottom);
    let lines = notes.min((bottom - y) / s.text.max(1));
    CaptureLayout { title, keys, notes: (y, lines), actions }
}

#[allow(clippy::too_many_arguments)]
fn capture_sheet(
    pixels: &mut [u32],
    stride: usize,
    font: &mut font::UiFont,
    m: &Metrics,
    layout: List,
    source: &dyn Source,
    keys: &str,
    view: &mut View,
    c: &Colors,
    s: &Sizes,
) {
    let area = sheet_area(layout, s);
    let w = area.w.min(m.dip(560.0));
    let selection = source.window(0, 0).ok().flatten().map_or(0, |w| w.selection);
    let row = source.window(selection, 1).ok().flatten().and_then(|window| window.rows.into_iter().next());
    let notes: Vec<String> = [
        "Up to two chords make a sequence, such as Ctrl+K Ctrl+S.",
        "Enter is recorded as a key first; press Enter again to use the shortcut.",
        "Esc cancels.",
    ]
    .iter()
    .flat_map(|line| desktop::wrap(font, line, w, 2))
    .collect();
    let plan = capture_layout(area, w, m, s, row.is_some(), notes.len());
    if let (Some(line), Some(row)) = (plan.title, &row) {
        let command = row.command.unwrap_or("");
        let pen = desktop::label(pixels, stride, font, line, &row.title, c.fg);
        let rest = Rect { x: pen + s.gap, w: (line.x + line.w).saturating_sub(pen + s.gap), ..line };
        desktop::label(pixels, stride, font, rest, command, c.faint);
    }
    // The capture box shows focus like a field; every key goes to it.
    let field = plan.keys;
    chrome::fill_clipped(pixels, stride, field, c.surface);
    chrome::round_rect(pixels, stride, field, s.radius, c.accent);
    chrome::round_rect(pixels, stride, inset(field, s.focus), s.radius, c.control);
    // Text and keycaps centre vertically, so a compact box keeps them visible.
    let inner = Rect { x: field.x + s.pad, y: field.y, w: field.w.saturating_sub(2 * s.pad), h: field.h };
    if keys.is_empty() {
        desktop::label(pixels, stride, font, inner, "Press the new shortcut…", c.faint);
    } else {
        desktop::keycaps(pixels, stride, font, inner, inner.x, keys, c, s);
    }
    let (top, lines) = plan.notes;
    for (index, text) in notes.iter().take(lines).enumerate() {
        desktop::label(pixels, stride, font, Rect { x: area.x, y: top + index * s.text, w, h: s.text }, text, c.muted);
    }
    let primary = (!keys.is_empty()).then_some(("Use shortcut", Target::CaptureAccept));
    sheet_buttons(
        pixels,
        stride,
        font,
        plan.actions,
        ("Cancel", Target::CaptureCancel),
        primary,
        view.hover,
        c,
        s,
        &mut view.hits,
    );
}

#[allow(clippy::too_many_arguments)]
fn confirm_sheet(
    pixels: &mut [u32],
    stride: usize,
    font: &mut font::UiFont,
    m: &Metrics,
    layout: List,
    window: &PanelWindow,
    view: &mut View,
    c: &Colors,
    s: &Sizes,
) {
    let area = sheet_area(layout, s);
    let w = area.w.min(m.dip(640.0));
    let controls = s.control + s.gap;
    let buttons = s.control + s.pad;
    let box_h = area.h.saturating_sub(controls + buttons).max(s.text + 2 * s.inner);
    let frame = Rect { x: area.x, y: area.y, w, h: box_h };
    chrome::fill_clipped(pixels, stride, frame, c.surface);
    chrome::round_rect(pixels, stride, frame, s.radius, c.border);
    chrome::round_rect(pixels, stride, inset(frame, s.hairline), s.radius, c.control);
    let inner = inset(frame, s.inner);
    let columns = (inner.w / font.width("M").max(1)).max(1);
    let rows = (inner.h / s.text.max(1)).max(1);
    view.review = Some((columns, rows));
    let Some(viewport) = confirmation_view(window, columns, rows) else { return };
    for (slot, line) in viewport.lines.iter().enumerate() {
        let r = Rect { x: inner.x, y: inner.y + slot * s.text, w: inner.w, h: s.text };
        let color = if slot == 0 && viewport.page == 0 { c.fg } else { c.muted };
        desktop::label(pixels, stride, font, r, line, color);
    }
    let y = frame.y + frame.h + s.gap;
    let conflicts = window.confirmation.len();
    let mut x = area.x;
    if conflicts > 1 {
        for (forward, icon) in [(false, Icon::Back), (true, Icon::Forward)] {
            let target = Target::ConflictStep { forward };
            let enabled =
                if forward { window.confirmation_index + 1 < conflicts } else { window.confirmation_index > 0 };
            let r = Rect { x, y, w: s.control, h: s.control };
            desktop::icon_button(pixels, stride, r, icon, hover_is(view.hover, target), enabled, c, s);
            if enabled {
                view.hits.push(r, target);
            }
            x += s.control;
        }
        let text = format!("Conflict {} of {conflicts}", window.confirmation_index + 1);
        x = desktop::label(pixels, stride, font, Rect { x: x + s.gap, y, w: w / 2, h: s.control }, &text, c.muted);
    }
    if viewport.pages > 1 {
        let text = format!("Page {} of {}", viewport.page + 1, viewport.pages);
        let tw = font.measure(&text, w / 3);
        let mut px = (area.x + w).saturating_sub(tw + 2 * s.control + s.gap);
        px = px.max(x + s.pad);
        desktop::label(pixels, stride, font, Rect { x: px, y, w: tw, h: s.control }, &text, c.muted);
        px += tw + s.gap;
        for (forward, icon) in [(false, Icon::Up), (true, Icon::Down)] {
            let index =
                if forward { (viewport.page + 1).min(viewport.pages - 1) } else { viewport.page.saturating_sub(1) };
            let target = Target::ReviewPage { index, pages: viewport.pages };
            let enabled = index != viewport.page;
            let r = Rect { x: px, y, w: s.control, h: s.control };
            desktop::icon_button(pixels, stride, r, icon, hover_is(view.hover, target), enabled, c, s);
            if enabled {
                view.hits.push(r, target);
            }
            px += s.control;
        }
    }
    let primary = if conflicts == 0 { "Apply" } else { "Replace and apply" };
    sheet_buttons(
        pixels,
        stride,
        font,
        Rect { w, ..area },
        ("Cancel", Target::ConfirmCancel),
        Some((primary, Target::ConfirmAccept)),
        view.hover,
        c,
        s,
        &mut view.hits,
    );
}

impl App {
    /// Left click while the settings window is open. Outside the card it
    /// closes the window unless a draft is open, as before.
    pub(super) fn settings_mouse(&mut self) {
        let target = match admit(&self.settings_view, self.settings_epoch, self.overlay_surface(), self.pointer) {
            Admit::Ignore => return,
            Admit::Stale => {
                // Geometry predates the latest input, an adopted reload or a
                // resize; repaint instead of guessing.
                self.palette_redraw();
                return;
            }
            Admit::Act(target) => target,
        };
        if self.settings_view.combo.is_some() {
            self.settings_view.combo = None;
            if !matches!(target, Target::Choice { .. }) {
                self.palette_redraw();
                return;
            }
        } else if let Target::CycleChoice { index, id } = target {
            let combo = self
                .settings_panel
                .as_ref()
                .and_then(|host| self.settings_view.choice_for(host, index, id, self.settings_epoch));
            if let Some(combo) = combo {
                self.settings_view.combo = Some(combo);
                self.composing = false;
                self.ime_destination = Some(ime::Destination::Cancelled);
                self.palette_redraw();
            }
            return;
        }
        let Some(host) = self.settings_panel.as_ref() else { return };
        let steps = plan(target, &state(host, self.settings_view.hosted));
        if !steps.is_empty() {
            self.settings_steps(steps);
        }
    }

    fn settings_steps(&mut self, steps: Vec<Step>) {
        for step in steps {
            let Some(host) = self.settings_panel.as_mut() else { return };
            let result = match step {
                Step::Close => {
                    host.close();
                    Ok(())
                }
                Step::OpenFile => {
                    self.dispatch_settings("view.settingsFile");
                    return;
                }
                Step::Settings(input) => host.input(input),
                Step::Panel(input) => host.panel_input(input),
                Step::Select { index, id } => {
                    if host.rows().get(index).map(|row| row.spec.id) != Some(id) {
                        break;
                    }
                    host.select(index);
                    Ok(())
                }
                Step::PanelSelect { index, print } => match host.panel_window(index, 1) {
                    Ok(Some(window)) if window.rows.first().map(fingerprint) == Some(print) => host.panel_select(index),
                    // The list changed since it was painted: drop the click.
                    Ok(_) => break,
                    Err(error) => Err(error),
                },
                Step::Category(category) => {
                    host.set_category(category);
                    Ok(())
                }
                Step::CycleCategory(backwards) => {
                    host.cycle_category(backwards);
                    Ok(())
                }
                Step::Choose { index, id, value } => {
                    let Some(row) = host.rows().into_iter().nth(index).filter(|row| row.spec.id == id) else { break };
                    if row.current == SettingValue::Text(value.into()) && id != "theme.name" {
                        Ok(())
                    } else {
                        host.edit_document(|doc| {
                            if id == "theme.name" {
                                doc.edit("theme.follow_os", SettingValue::Bool(false))?;
                            }
                            doc.edit(id, SettingValue::Text(value.into()))
                        })
                    }
                }
                Step::Hosted(open) => {
                    self.settings_view.hosted = open && self.settings_hosted_available();
                    if self.settings_view.hosted {
                        self.settings_hosted_opened();
                    }
                    Ok(())
                }
                Step::HostedTarget(code) => {
                    self.settings_hosted_click(code);
                    Ok(())
                }
            };
            if let Err(error) = result {
                self.view.set_notice(format!("Settings: {error}"));
                break;
            }
        }
        self.settings_epoch = self.settings_epoch.wrapping_add(1);
        self.palette_redraw();
    }

    /// Pointer motion over the open window: hover only repaints the card.
    pub(super) fn settings_track(&mut self) {
        if self.settings_view.track(self.pointer) {
            self.request_frame();
        }
    }

    /// Wheel scrolls the visible list; it never moves the selection, so it can
    /// never move a conflict review or change what Enter acts on. The rows and
    /// their hit map repaint as a card-only frame.
    pub(super) fn settings_wheel(&mut self, up: bool) {
        if let Some(combo) = &mut self.settings_view.combo {
            combo.selected = if up {
                combo.selected.saturating_sub(1)
            } else {
                (combo.selected + 1).min(combo.choices.len().saturating_sub(1))
            };
            self.palette_redraw();
            return;
        }
        if self.settings_view.hosted {
            self.settings_hosted_wheel(up);
            return;
        }
        let Some(host) = self.settings_panel.as_ref() else { return };
        if host.panel_is_editing() {
            return;
        }
        if self.settings_view.wheel(up, host.panel().is_some()) {
            self.request_frame();
        }
    }

    /// `UserEvent::SettingsPanelReady` (LIB-HOOKS-R1 hunk 5): drains the
    /// worker. When that replaced the shown document (a clean external reload
    /// is adopted at once), painted controls describe old values, so the paint
    /// epoch advances exactly as for Ctrl+R adoption: a click aimed at the old
    /// value is dropped and the window repainted. Reloads parked beside a draft
    /// and save acknowledgements keep the document and the epoch.
    pub(super) fn settings_worker_ready(&mut self) {
        let Some(panel) = self.settings_panel.as_mut() else { return };
        let before = shown_document(panel.document());
        panel.worker_ready();
        let after = shown_document(panel.document());
        fence_reload(before, after, &mut self.settings_epoch, &mut self.composing, &mut self.ime_destination);
    }

    pub(super) fn settings_open(&self) -> bool {
        self.settings_panel.as_ref().is_some_and(SettingsSession::is_open)
    }

    pub(super) fn settings_choice_open(&self) -> bool {
        self.settings_view.combo.is_some()
    }

    pub(super) fn dispatch_settings(&mut self, id: &str) -> bool {
        if !matches!(id, "view.settings" | "view.settingsFile") {
            return false;
        }
        if id == "view.settings" {
            self.start_registration();
        }
        if let Some(palette) = &mut self.palette {
            let _ = palette.input(kwil_view_model::palette_session::PaletteInput::Cancel);
        }
        if let Some(panel) = &mut self.settings_panel {
            panel.open();
            if id == "view.settingsFile" {
                panel.clear_error();
                if panel.menu().is_none() || panel.source_exists() || panel.has_conflict() {
                    let path = panel.path().to_path_buf();
                    panel.close();
                    self.settings_file_pending = false;
                    if let Err(error) = self.view.open_path(path) {
                        self.view.set_notice(format!("Open settings: {error}"));
                    }
                    self.sync_file_ui();
                    self.apply(Damage { full: true, status: true, ..Default::default() });
                } else {
                    // Only a successful Loaded(None) confirms absence. Invalid
                    // TOML and conflicts must remain directly repairable as text.
                    self.settings_file_pending = true;
                    if let Err(error) = panel.input(SettingsInput::Save) {
                        self.settings_file_pending = false;
                        self.view.set_notice(format!("Settings: {error}"));
                    }
                }
            }
        } else {
            let damage = self.view.set_notice("Settings are starting; try again shortly".into());
            self.apply(damage);
        }
        self.settings_epoch = self.settings_epoch.wrapping_add(1);
        self.settings_view.hover = None;
        self.ime_destination = Some(ime::Destination::Cancelled);
        self.composing = false;
        self.drag = None;
        self.selecting = false;
        if let Some(keys) = &mut self.keys {
            keys.cancel_pending();
        }
        self.palette_redraw();
        true
    }

    pub(super) fn settings_input(&mut self, input: SettingsInput<'_>) {
        // IME and wheel callbacks use this shared entry point. Text commits
        // during key capture/confirmation are consumed without touching search.
        if self.settings_panel.as_ref().is_some_and(SettingsSession::panel_is_capturing)
            && matches!(input, SettingsInput::Text(_))
        {
            return;
        }
        // The hosted page owns its own fields; nothing types into the list behind it.
        if self.settings_view.hosted && matches!(input, SettingsInput::Text(_)) {
            return;
        }
        if let Some(panel) = &mut self.settings_panel
            && let Err(error) = panel.input(input)
        {
            let damage = self.view.set_notice(format!("Settings: {error}"));
            self.apply(damage);
        }
        self.settings_epoch = self.settings_epoch.wrapping_add(1);
        self.palette_redraw();
    }

    fn settings_panel_input(&mut self, input: PanelInput<'_>) {
        if let Some(panel) = &mut self.settings_panel
            && let Err(error) = panel.panel_input(input)
        {
            let damage = self.view.set_notice(format!("Settings: {error}"));
            self.apply(damage);
        }
        self.settings_epoch = self.settings_epoch.wrapping_add(1);
        self.palette_redraw();
    }

    pub(super) fn settings_key(&mut self, event: &KeyEvent) -> bool {
        if !self.settings_open() {
            return false;
        }
        if let Some(mut combo) = self.settings_view.combo.take() {
            if event.state != ElementState::Pressed {
                self.settings_view.combo = Some(combo);
                return true;
            }
            if combo.epoch != self.settings_epoch {
                self.palette_redraw();
                return true;
            }
            match event.logical_key {
                Key::Named(NamedKey::Escape) => {}
                Key::Named(NamedKey::Enter | NamedKey::Space) => {
                    if let Some(value) = combo.choices.get(combo.selected) {
                        self.settings_steps(vec![
                            Step::Select { index: combo.index, id: combo.id },
                            Step::Choose { index: combo.index, id: combo.id, value },
                        ]);
                    }
                }
                _ => {
                    match event.logical_key {
                        Key::Named(NamedKey::ArrowUp) => combo.selected = combo.selected.saturating_sub(1),
                        Key::Named(NamedKey::ArrowDown) => {
                            combo.selected = (combo.selected + 1).min(combo.choices.len().saturating_sub(1))
                        }
                        Key::Named(NamedKey::Home) => combo.selected = 0,
                        Key::Named(NamedKey::End) => combo.selected = combo.choices.len().saturating_sub(1),
                        _ => {}
                    }
                    self.settings_view.combo = Some(combo);
                }
            }
            self.palette_redraw();
            return true;
        }
        if self.settings_view.hosted {
            // Escape returns to the list; the hosted page sees every other key.
            if matches!(event.logical_key, Key::Named(NamedKey::Escape)) && !event.repeat {
                self.settings_view.hosted = false;
                self.settings_epoch = self.settings_epoch.wrapping_add(1);
                self.palette_redraw();
            } else {
                self.settings_hosted_key(event);
            }
            return true;
        }
        let host = self.settings_panel.as_ref().unwrap();
        let mode = KeyMode {
            compound: host.panel().is_some(),
            capturing: host.panel_is_capturing(),
            editing: host.is_editing() || host.panel_is_editing(),
            searching: host.is_searching(),
            confirming: host.panel_is_confirming(),
            repeat: event.repeat,
        };
        match key_route(&event.logical_key, event.text.as_deref(), self.mods, self.composing, mode) {
            KeyRoute::Panel(input) => self.settings_panel_input(input),
            KeyRoute::Settings(input) => self.settings_input(input),
            KeyRoute::ReviewPage(forward) => {
                // Page with exactly the wrap the last paint used.
                if let Some((columns, rows)) = self.settings_view.review {
                    match host.panel_window(0, 0) {
                        Ok(Some(window)) => {
                            if let Some(input) = confirmation_page_input(&window, columns, rows, forward) {
                                self.settings_panel_input(input);
                            }
                        }
                        Err(error) => {
                            self.view.set_notice(format!("Settings: {error}"));
                            self.palette_redraw();
                        }
                        Ok(None) => {}
                    }
                }
            }
            KeyRoute::Paste => {
                let epoch = self.settings_epoch;
                let destination = host.panel();
                let hwnd = self.gfx.as_ref().map_or(0, |gfx| gfx.hwnd);
                match clipboard::SystemClipboard(hwnd).get_text(kwil_view_model::settings_session::SETTINGS_INPUT_LIMIT)
                {
                    Ok(Some(text))
                        if epoch == self.settings_epoch
                            && self.settings_open()
                            && self.settings_panel.as_ref().and_then(SettingsSession::panel) == destination =>
                    {
                        if destination.is_some() {
                            self.settings_panel_input(PanelInput::Text(&text));
                        } else {
                            self.settings_input(SettingsInput::Text(&text));
                        }
                    }
                    Ok(_) => {}
                    Err(error) => {
                        let damage = self.view.set_notice(format!("Settings paste: {error}"));
                        self.apply(damage);
                    }
                }
            }
            KeyRoute::OpenFile => {
                self.dispatch_settings("view.settingsFile");
            }
            KeyRoute::AdoptExternal => {
                if let Some(panel) = &mut self.settings_panel
                    && let Err(error) = finish_adoption(
                        panel.adopt_external(),
                        &mut self.settings_epoch,
                        &mut self.composing,
                        &mut self.ime_destination,
                    )
                {
                    self.view.set_notice(format!("Settings: {error}"));
                }
                self.palette_redraw();
            }
            KeyRoute::Category(backwards) => {
                self.settings_panel.as_mut().unwrap().cycle_category(backwards);
                self.settings_epoch = self.settings_epoch.wrapping_add(1);
                self.palette_redraw();
            }
            KeyRoute::Ignore => {}
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kwil_core_commands::settings_ui::SettingsDocument;
    #[cfg(windows)]
    use kwil_core_commands::settings_ui::SettingsMenu;
    use kwil_view_model::settings_panels::SettingsPanelController;

    fn mode(compound: bool, editing: bool, capturing: bool) -> KeyMode {
        KeyMode { compound, editing, capturing, searching: false, confirming: false, repeat: false }
    }

    fn confirming_mode(repeat: bool) -> KeyMode {
        KeyMode { confirming: true, repeat, ..mode(true, true, false) }
    }

    /// Worker-free settings source over the same core menu and panel
    /// controller the session wraps.
    #[cfg(windows)]
    struct Fake {
        menu: SettingsMenu,
        panel: Option<SettingsPanelController>,
        query: String,
        searching: bool,
        draft: Option<String>,
    }
    #[cfg(windows)]
    impl Fake {
        fn new(source: &str) -> Self {
            Self {
                menu: SettingsMenu::new(SettingsDocument::parse(source).unwrap()),
                panel: None,
                query: String::new(),
                searching: false,
                draft: None,
            }
        }
        fn with_panel(mut self, kind: SettingsPanel) -> Self {
            self.panel = Some(SettingsPanelController::new(kind, self.menu.document()).unwrap());
            self
        }
    }
    #[cfg(windows)]
    impl Source for Fake {
        fn loaded(&self) -> bool {
            true
        }
        fn error(&self) -> Option<&str> {
            None
        }
        fn status(&self) -> &str {
            "Saved"
        }
        fn category(&self) -> Option<Category> {
            None
        }
        fn rows(&self) -> Vec<SettingsRow> {
            self.menu.rows().collect()
        }
        fn selected(&self) -> usize {
            let id = self.menu.selected().map(|row| row.spec.id);
            self.menu.rows().position(|row| Some(row.spec.id) == id).unwrap_or(0)
        }
        fn searching(&self) -> bool {
            self.searching
        }
        fn query(&self) -> &str {
            &self.query
        }
        fn cursor(&self) -> usize {
            self.draft.as_ref().map_or(self.query.len(), String::len)
        }
        fn draft(&self) -> Option<&str> {
            self.draft.as_deref()
        }
        fn panel(&self) -> Option<SettingsPanel> {
            self.panel.as_ref().map(SettingsPanelController::panel)
        }
        fn window(&self, offset: usize, max: usize) -> Result<Option<PanelWindow>, String> {
            self.panel
                .as_ref()
                .map(|panel| panel.window(self.menu.document(), offset, max).map_err(|e| e.to_string()))
                .transpose()
        }
        fn panel_cursor(&self) -> usize {
            self.panel.as_ref().map_or(0, SettingsPanelController::query_cursor)
        }
        fn panel_editing(&self) -> bool {
            self.panel.as_ref().is_some_and(SettingsPanelController::is_editing)
        }
    }

    #[cfg(windows)]
    fn paint(source: &Fake, size: (usize, usize), scale: f64, view: &mut View) -> (Vec<u32>, Option<Rect>) {
        let mut font = font::UiFont::load(chrome::Metrics::new(scale).text_px()).expect("Windows chrome face");
        let mut pixels = vec![0u32; size.0 * size.1];
        let area = draw(source, &mut pixels, size, &mut font, theme_ui::chrome(), &Metrics::new(scale), 7, view, None);
        (pixels, area)
    }

    fn routed_panel<'a>(key: &Key, text: Option<&'a str>, mods: ModifiersState, mode: KeyMode) -> PanelInput<'a> {
        match key_route(key, text, mods, false, mode) {
            KeyRoute::Panel(input) => input,
            _ => panic!("compound input escaped its controller"),
        }
    }

    fn review_page(panel: &mut SettingsPanelController, doc: &SettingsDocument, key: NamedKey, cells: (usize, usize)) {
        let KeyRoute::ReviewPage(forward) =
            key_route(&Key::Named(key), None, ModifiersState::empty(), false, confirming_mode(false))
        else {
            panic!("review page key escaped confirmation");
        };
        let window = panel.window(doc, 0, 0).unwrap();
        let input = confirmation_page_input(&window, cells.0, cells.1, forward).unwrap();
        assert!(panel.input(doc, input).unwrap().is_none());
    }

    /// All text the confirmation box paints across its pages.
    fn reviewed(panel: &mut SettingsPanelController, doc: &SettingsDocument, cells: (usize, usize)) -> String {
        let pages = confirmation_view(&panel.window(doc, 0, 0).unwrap(), cells.0, cells.1).unwrap().pages;
        let mut content = String::new();
        for page in 0..pages {
            let viewport = confirmation_view(&panel.window(doc, 0, 0).unwrap(), cells.0, cells.1).unwrap();
            assert_eq!((viewport.page, viewport.pages), (page, pages));
            assert!(viewport.lines.len() <= cells.1);
            content.push_str(&viewport.lines.concat());
            if page + 1 < pages {
                review_page(panel, doc, NamedKey::ArrowRight, cells);
            }
        }
        content
    }

    #[test]
    fn settings_gui_capture_precedes_hotkeys_and_keeps_modified_escape() {
        let mode = mode(true, true, true);
        for (key, mods) in [
            (Key::Character("v".into()), ModifiersState::CONTROL),
            (Key::Character("o".into()), ModifiersState::CONTROL),
            (Key::Character("r".into()), ModifiersState::CONTROL),
            (Key::Named(NamedKey::Enter), ModifiersState::empty()),
            (Key::Named(NamedKey::Escape), ModifiersState::SHIFT),
            (Key::Named(NamedKey::Escape), ModifiersState::empty()),
        ] {
            let PanelInput::Capture(chord) = routed_panel(&key, None, mods, mode) else {
                panic!("shortcut was interpreted as text or a host action");
            };
            assert_eq!(Some(chord), input::chord(&key, mods));
        }
        assert!(matches!(
            key_route(&Key::Named(NamedKey::Shift), None, ModifiersState::SHIFT, false, mode),
            KeyRoute::Ignore
        ));
    }

    #[test]
    fn settings_gui_first_enter_and_second_chord_reach_controller() {
        let doc = SettingsDocument::parse("").unwrap();
        let mut panel = SettingsPanelController::new(SettingsPanel::Keymap, &doc).unwrap();
        panel.search(&doc, "file.save").unwrap();
        panel.input(&doc, PanelInput::Accept).unwrap();
        let capture = mode(true, true, true);
        let enter = routed_panel(&Key::Named(NamedKey::Enter), None, ModifiersState::empty(), capture);
        assert!(panel.input(&doc, enter).unwrap().is_none());
        assert_eq!(panel.window(&doc, 0, 1).unwrap().capture.as_deref(), Some("enter"));
        panel.input(&doc, PanelInput::Cancel).unwrap();
        panel.input(&doc, PanelInput::Accept).unwrap();
        let first = routed_panel(&Key::Named(NamedKey::F11), None, ModifiersState::ALT, capture);
        assert!(panel.input(&doc, first).unwrap().is_none());
        let second = routed_panel(&Key::Named(NamedKey::F10), None, ModifiersState::ALT, capture);
        let edit = panel.input(&doc, second).unwrap().expect("second chord completes the binding");
        let kwil_view_model::settings_panels::PanelEdit::Rebind { binding, .. } = edit else { panic!("wrong edit") };
        assert_eq!(binding.keys, "alt+f11 alt+f10");
    }

    #[test]
    fn settings_gui_conflicts_require_enter_and_escape_cancels() {
        let doc = SettingsDocument::parse("").unwrap();
        let mut panel = SettingsPanelController::new(SettingsPanel::Keymap, &doc).unwrap();
        panel.search(&doc, "file.save").unwrap();
        panel.input(&doc, PanelInput::Accept).unwrap();
        panel
            .input(
                &doc,
                routed_panel(&Key::Character("o".into()), None, ModifiersState::CONTROL, mode(true, true, true)),
            )
            .unwrap();
        panel
            .input(
                &doc,
                routed_panel(&Key::Named(NamedKey::Enter), None, ModifiersState::empty(), mode(true, true, true)),
            )
            .unwrap();
        assert!(panel.is_confirming());
        let original = doc.source().to_owned();
        for input in [
            routed_panel(&Key::Character("x".into()), Some("x"), ModifiersState::empty(), mode(true, true, false)),
            routed_panel(&Key::Named(NamedKey::Delete), None, ModifiersState::empty(), mode(true, true, false)),
            routed_panel(&Key::Named(NamedKey::Tab), None, ModifiersState::empty(), mode(true, true, false)),
        ] {
            assert!(panel.input(&doc, input).unwrap().is_none());
            assert!(panel.is_confirming());
        }
        let accept = routed_panel(&Key::Named(NamedKey::Enter), None, ModifiersState::empty(), mode(true, true, false));
        let edit = panel.input(&doc, accept).unwrap().expect("explicit Enter accepts conflicts");
        let mut accepted = doc.clone();
        edit.apply(&mut accepted).unwrap();
        assert_ne!(accepted.source(), original);
        panel.input(&doc, PanelInput::Accept).unwrap();
        let escape = routed_panel(&Key::Named(NamedKey::Escape), None, ModifiersState::empty(), mode(true, true, true));
        assert!(panel.input(&doc, escape).unwrap().is_none());
        assert!(!panel.is_editing());
        assert_eq!(doc.source(), original);
    }

    #[test]
    fn settings_gui_confirmation_review_scrolls_to_current_conflict() {
        let doc = SettingsDocument::parse("[[bindings]]\nkeys='ctrl+c ctrl+x'\ncommand='file.open'\n").unwrap();
        let original = doc.source().to_owned();
        let mut panel = SettingsPanelController::new(SettingsPanel::Keymap, &doc).unwrap();
        panel.search(&doc, "file.save").unwrap();
        panel.input(&doc, PanelInput::Accept).unwrap();
        panel.input(&doc, PanelInput::Capture("ctrl+c".parse().unwrap())).unwrap();
        panel.input(&doc, PanelInput::Accept).unwrap();
        let first = panel.window(&doc, 0, 0).unwrap();
        assert!(first.confirmation.len() >= 2);
        // Two paintable rows of a 77-cell box require actual paging.
        let cells = (77, 2);
        let editing = confirming_mode(false);
        let check = |panel: &mut SettingsPanelController, expected: usize| {
            let window = panel.window(&doc, 0, 0).unwrap();
            assert_eq!(window.confirmation_index, expected);
            assert_eq!(window.confirmation_page, 0, "changing conflicts resets its review page");
            assert!(confirmation_view(&window, cells.0, cells.1).unwrap().pages > 1);
            let content = reviewed(panel, &doc, cells);
            let conflict = &window.confirmation[expected];
            assert!(content.contains(&format!("Conflict {}/{}", expected + 1, window.confirmation.len())));
            assert!(content.contains(&format!("Proposed: {}", window.proposal.as_deref().unwrap())));
            assert!(content.contains("file.save <- ctrl+c"));
            assert!(content.contains(&format!("Command: {}", conflict.command)));
            assert!(content.contains(&format!("Kind: {:?}", conflict.kind)));
            let pages = confirmation_view(&window, cells.0, cells.1).unwrap().pages;
            review_page(panel, &doc, NamedKey::PageDown, cells);
            assert_eq!(panel.window(&doc, 0, 0).unwrap().confirmation_page, pages - 1);
            review_page(panel, &doc, NamedKey::PageUp, cells);
            assert_eq!(panel.window(&doc, 0, 0).unwrap().confirmation_page, pages - 2);
        };
        check(&mut panel, 0);
        for expected in 1..first.confirmation.len() {
            let next = routed_panel(&Key::Named(NamedKey::Tab), None, ModifiersState::empty(), editing);
            assert!(panel.input(&doc, next).unwrap().is_none());
            check(&mut panel, expected);
        }
        let previous = routed_panel(&Key::Named(NamedKey::Tab), None, ModifiersState::SHIFT, editing);
        assert!(panel.input(&doc, previous).unwrap().is_none());
        check(&mut panel, first.confirmation.len() - 2);
        assert_eq!(doc.source(), original, "review navigation must not accept the proposal");
        panel.input(&doc, routed_panel(&Key::Named(NamedKey::Escape), None, ModifiersState::empty(), editing)).unwrap();
        assert!(confirmation_view(&panel.window(&doc, 0, 0).unwrap(), cells.0, cells.1).is_none());
        assert_eq!(doc.source(), original);
    }

    #[test]
    fn settings_gui_confirmation_keeps_empty_conflicts_pending() {
        let doc = SettingsDocument::parse("").unwrap();
        let mut panel = SettingsPanelController::new(SettingsPanel::Keymap, &doc).unwrap();
        panel.search(&doc, "file.save").unwrap();
        panel.input(&doc, PanelInput::Accept).unwrap();
        panel.input(&doc, PanelInput::Capture("ctrl+o".parse().unwrap())).unwrap();
        panel.input(&doc, PanelInput::Accept).unwrap();
        assert!(panel.is_confirming());
        let mut changed = SettingsDocument::parse("[[bindings]]\nkeys='ctrl+o'\ncommand=''\n").unwrap();
        let before = changed.source().to_owned();
        let editing = confirming_mode(false);
        let enter = routed_panel(&Key::Named(NamedKey::Enter), None, ModifiersState::empty(), editing);
        assert!(panel.input(&changed, enter).unwrap_err().to_string().contains("conflicts changed"));
        let window = panel.window(&changed, 0, 0).unwrap();
        assert!(window.confirmation.is_empty());
        let painted = confirmation_view(&window, 77, 2).unwrap().lines.concat();
        assert!(painted.contains("No conflicts remain"));
        assert_eq!(changed.source(), before);
        assert!(panel.input(&changed, PanelInput::Next).unwrap().is_none());
        assert!(panel.is_confirming());
        let edit = panel.input(&changed, enter).unwrap().expect("second explicit Enter applies reviewed proposal");
        edit.apply(&mut changed).unwrap();
        assert_ne!(changed.source(), before);
    }

    #[test]
    fn settings_gui_narrow_confirmation_pages_expose_every_field() {
        let keys = "ctrl+alt+shift+super+f23 ctrl+alt+shift+super+f24";
        let doc = SettingsDocument::parse(&format!("[[bindings]]\nkeys='{keys}'\ncommand='file.open'\n")).unwrap();
        let original = doc.source().to_owned();
        let mut panel = SettingsPanelController::new(SettingsPanel::Keymap, &doc).unwrap();
        panel.search(&doc, "file.save").unwrap();
        panel.input(&doc, PanelInput::Accept).unwrap();
        for chord in keys.split_whitespace() {
            panel.input(&doc, PanelInput::Capture(chord.parse().unwrap())).unwrap();
        }
        assert!(panel.is_confirming());
        let window = panel.window(&doc, 0, 0).unwrap();
        let conflict = window.confirmation[window.confirmation_index].clone();
        let tiny = (10, 2);
        let all = reviewed(&mut panel, &doc, tiny);
        assert!(all.contains(window.proposal.as_deref().unwrap()));
        assert!(all.contains(&format!("Command: {}", conflict.command)));
        assert!(all.contains(&format!("Kind: {:?}", conflict.kind)));
        for _ in 0..64 {
            review_page(&mut panel, &doc, NamedKey::ArrowLeft, tiny);
        }
        assert_eq!(panel.window(&doc, 0, 0).unwrap().confirmation_page, 0);
        assert_eq!(doc.source(), original, "paging cannot accept a proposal");
    }

    #[test]
    fn settings_gui_repeated_capture_chords_do_not_advance() {
        let doc = SettingsDocument::parse("").unwrap();
        let mut panel = SettingsPanelController::new(SettingsPanel::Keymap, &doc).unwrap();
        panel.search(&doc, "file.save").unwrap();
        panel
            .input(
                &doc,
                routed_panel(&Key::Named(NamedKey::Enter), None, ModifiersState::empty(), mode(true, false, false)),
            )
            .unwrap();
        let repeated = KeyMode { repeat: true, ..mode(true, true, true) };
        assert!(matches!(
            key_route(&Key::Named(NamedKey::Enter), None, ModifiersState::empty(), false, repeated),
            KeyRoute::Ignore
        ));
        assert_eq!(panel.window(&doc, 0, 0).unwrap().capture.as_deref(), Some(""));
        let chord = Key::Character("o".into());
        panel.input(&doc, routed_panel(&chord, None, ModifiersState::CONTROL, mode(true, true, true))).unwrap();
        let route = key_route(&chord, None, ModifiersState::CONTROL, false, repeated);
        if let KeyRoute::Panel(input) = route {
            panel.input(&doc, input).unwrap();
        }
        assert!(panel.is_capturing(), "a held chord must not become the second chord");
        let fresh = routed_panel(&Key::Named(NamedKey::Enter), None, ModifiersState::empty(), mode(true, true, true));
        panel.input(&doc, fresh).unwrap();
        assert!(panel.is_confirming(), "fresh Enter still finishes capture");
    }

    #[test]
    fn settings_gui_repeated_enter_does_not_accept_conflicts_and_text_still_repeats() {
        let doc = SettingsDocument::parse("").unwrap();
        let original = doc.source().to_owned();
        let mut panel = SettingsPanelController::new(SettingsPanel::Keymap, &doc).unwrap();
        panel.search(&doc, "file.save").unwrap();
        panel.input(&doc, PanelInput::Accept).unwrap();
        panel.input(&doc, PanelInput::Capture("ctrl+o".parse().unwrap())).unwrap();
        panel.input(&doc, PanelInput::Accept).unwrap();
        let route =
            key_route(&Key::Named(NamedKey::Enter), None, ModifiersState::empty(), false, confirming_mode(true));
        assert!(matches!(route, KeyRoute::Ignore));
        assert!(panel.is_confirming());
        assert_eq!(doc.source(), original);
        let fresh = routed_panel(&Key::Named(NamedKey::Enter), None, ModifiersState::empty(), confirming_mode(false));
        assert!(panel.input(&doc, fresh).unwrap().is_some(), "a fresh physical Enter must accept");
        let repeated = KeyMode { repeat: true, ..mode(true, true, false) };
        assert!(matches!(
            key_route(&Key::Character("x".into()), Some("x"), ModifiersState::empty(), false, repeated),
            KeyRoute::Panel(PanelInput::Text("x"))
        ));
        let scalar = KeyMode { compound: false, ..repeated };
        assert!(matches!(
            key_route(&Key::Character("x".into()), Some("x"), ModifiersState::empty(), false, scalar),
            KeyRoute::Settings(SettingsInput::Text("x"))
        ));
    }

    #[test]
    fn settings_gui_formatter_and_string_map_keyboard_edits_stay_typed() {
        let mut doc = SettingsDocument::parse("").unwrap();
        for (kind, key, value) in [
            (SettingsPanel::Formatters, "rust.executable", "E:/Tools/rust fmt.exe"),
            (SettingsPanel::StringMap { id: "theme.colors" }, "background", "#123456"),
            (SettingsPanel::FileTypes, "rs", "rust"),
        ] {
            let mut panel = SettingsPanelController::new(kind, &doc).unwrap();
            panel.search(&doc, "unique new entry").unwrap();
            let browse = mode(true, false, false);
            let editing = mode(true, true, false);
            for input in [
                routed_panel(&Key::Named(NamedKey::Enter), None, ModifiersState::empty(), browse),
                routed_panel(&Key::Character(key.into()), Some(key), ModifiersState::empty(), editing),
                routed_panel(&Key::Named(NamedKey::Enter), None, ModifiersState::empty(), editing),
                routed_panel(&Key::Character(value.into()), Some(value), ModifiersState::empty(), editing),
            ] {
                assert!(panel.input(&doc, input).unwrap().is_none());
            }
            let edit = panel
                .input(&doc, routed_panel(&Key::Named(NamedKey::Enter), None, ModifiersState::empty(), editing))
                .unwrap()
                .unwrap();
            edit.apply(&mut doc).unwrap();
        }
        assert_eq!(doc.settings().formatters["rust"].executable, "E:/Tools/rust fmt.exe");
        assert_eq!(doc.settings().theme.colors["background"], "#123456");
        assert_eq!(doc.language_for_extension("rs"), Some("rust"));
    }

    #[test]
    fn settings_gui_adoption_invalidates_only_successful_destination_changes() {
        let mut epoch = 41;
        let mut composing = true;
        let mut destination = Some(ime::Destination::Settings { epoch });
        assert!(finish_adoption(Err("pending save".into()), &mut epoch, &mut composing, &mut destination).is_err());
        assert_eq!(epoch, 41);
        assert!(composing);
        assert!(matches!(destination, Some(ime::Destination::Settings { epoch: 41 })));
        finish_adoption(Ok(()), &mut epoch, &mut composing, &mut destination).unwrap();
        assert_eq!(epoch, 42, "late IME commit must fail its captured epoch check");
        assert!(!composing);
        assert!(matches!(destination, Some(ime::Destination::Cancelled)));
    }

    #[test]
    fn settings_gui_scalar_routing_and_compositions_stay_scoped() {
        let scalar = mode(false, false, false);
        assert!(matches!(
            key_route(&Key::Named(NamedKey::Delete), None, ModifiersState::empty(), false, scalar),
            KeyRoute::Settings(SettingsInput::Reset)
        ));
        assert!(matches!(
            key_route(&Key::Named(NamedKey::Delete), None, ModifiersState::empty(), false, mode(true, false, false)),
            KeyRoute::Panel(PanelInput::Delete)
        ));
        assert!(matches!(
            key_route(&Key::Named(NamedKey::Tab), None, ModifiersState::SHIFT, false, scalar),
            KeyRoute::Category(true)
        ));
        for mode in [scalar, mode(false, true, false), mode(true, false, false), mode(true, true, false)] {
            assert!(matches!(
                key_route(&Key::Character("x".into()), Some("x"), ModifiersState::empty(), true, mode),
                KeyRoute::Ignore
            ));
        }
    }

    #[test]
    fn settings_gui_clicks_never_retarget_open_drafts() {
        let row = Target::Row { index: 3, id: "editor.tab_size" };
        let all = [
            Target::Close,
            Target::OpenFile,
            Target::Search,
            Target::ClearSearch,
            Target::Category(None),
            Target::CategoryStep { backwards: false },
            Target::Back,
            row,
            Target::Toggle { index: 0, id: "editor.word_wrap" },
            Target::Choice { index: 1, id: "keymap", value: "vim" },
            Target::Reset { index: 2, id: "font.size" },
            Target::PanelRow { index: 0, print: 1 },
            Target::PanelChange { index: 0, print: 1 },
            Target::PanelRemove { index: 0, print: 1 },
            Target::OpenHosted,
        ];
        let scalar_draft = State { editing: true, ..State::default() };
        let capture =
            State { panel: Some(SettingsPanel::Keymap), panel_editing: true, capturing: true, ..State::default() };
        let confirm = State { confirming: true, capturing: false, ..capture };
        let entry = State {
            panel: Some(SettingsPanel::FileTypes),
            panel_editing: true,
            column: Some(EditColumn::Key),
            ..State::default()
        };
        for state in [scalar_draft, capture, confirm, entry] {
            for target in all {
                assert!(plan(target, &state).is_empty(), "{target:?} acted during a draft {state:?}");
            }
        }
        // Each draft keeps its own controls.
        assert!(matches!(plan(Target::EditAccept, &scalar_draft).as_slice(), [Step::Settings(SettingsInput::Accept)]));
        assert!(matches!(plan(Target::CaptureCancel, &capture).as_slice(), [Step::Panel(PanelInput::Cancel)]));
        assert!(plan(Target::CaptureAccept, &confirm).is_empty(), "capture buttons are inert once confirming");
        assert!(matches!(plan(Target::ConfirmAccept, &confirm).as_slice(), [Step::Panel(PanelInput::Accept)]));
        assert!(matches!(
            plan(Target::ReviewPage { index: 1, pages: 3 }, &confirm).as_slice(),
            [Step::Panel(PanelInput::ReviewPage { index: 1, pages: 3 })]
        ));
        assert!(matches!(
            plan(Target::EntryColumn(EditColumn::Value), &entry).as_slice(),
            [Step::Panel(PanelInput::Next)]
        ));
        assert!(plan(Target::EntryColumn(EditColumn::Key), &entry).is_empty(), "the focused field is no-op");
    }

    #[test]
    fn settings_gui_controls_map_to_real_host_actions() {
        let browse = State::default();
        let id = "editor.word_wrap";
        assert!(matches!(
            plan(Target::Toggle { index: 4, id }, &browse).as_slice(),
            [Step::Select { index: 4, .. }, Step::Settings(SettingsInput::Accept)]
        ));
        // Accept while searching only ends the search, so a click ends it first.
        let searching = State { searching: true, ..browse };
        assert!(matches!(
            plan(Target::Toggle { index: 4, id }, &searching).as_slice(),
            [
                Step::Settings(SettingsInput::Accept),
                Step::Select { index: 4, .. },
                Step::Settings(SettingsInput::Accept)
            ]
        ));
        assert!(matches!(
            plan(Target::Choice { index: 1, id: "keymap", value: "vim" }, &browse).as_slice(),
            [Step::Select { index: 1, .. }, Step::Choose { value: "vim", .. }]
        ));
        assert!(matches!(
            plan(Target::Reset { index: 2, id: "font.size" }, &browse).as_slice(),
            [Step::Select { index: 2, .. }, Step::Settings(SettingsInput::Reset)]
        ));
        // The chosen value goes through the same typed document edit as the TOML file.
        let mut doc = SettingsDocument::parse("").unwrap();
        doc.edit("keymap", SettingValue::Text("vim".into())).unwrap();
        assert_eq!(doc.settings().keymap, kwil_core_commands::Preset::Vim);
        let panel = State { panel: Some(SettingsPanel::Keymap), ..browse };
        assert!(matches!(
            plan(Target::PanelRemove { index: 5, print: 9 }, &panel).as_slice(),
            [Step::PanelSelect { index: 5, print: 9 }, Step::Panel(PanelInput::Delete)]
        ));
        let with_query = State { panel_query_empty: false, ..panel };
        assert_eq!(plan(Target::Back, &with_query).len(), 2, "Back clears the query and then leaves the page");
        let panel = State { panel_query_empty: true, ..panel };
        assert_eq!(plan(Target::Back, &panel).len(), 1);
        assert!(plan(Target::Toggle { index: 4, id }, &panel).is_empty(), "schema rows are not live on a panel page");
        // Clearing a retained query that is no longer focused re-enters search first.
        let retained = State { query_empty: false, ..browse };
        assert!(matches!(
            plan(Target::ClearSearch, &retained).as_slice(),
            [Step::Settings(SettingsInput::Search), Step::Settings(SettingsInput::Cancel)]
        ));
    }

    #[test]
    fn settings_gui_frames_fit_every_window_and_scale_with_dpi_only() {
        for scale in [1.0, 1.25, 1.5, 2.0] {
            let m = Metrics::new(scale);
            let s = Sizes::new(&m);
            for size in [(640, 480), (800, 600), (1280, 800), (1920, 1080), (3840, 2160), (420, 900), (2000, 400)] {
                let size = ((size.0 as f64 * scale) as usize, (size.1 as f64 * scale) as usize);
                let Some(f) = frame(size, &m, &s) else { continue };
                assert!(f.card.x + f.card.w <= size.0 && f.card.y + f.card.h <= size.1);
                for r in [f.header, f.search, f.content, f.footer] {
                    assert!(r.x >= f.card.x && r.x + r.w <= f.card.x + f.card.w, "{r:?} leaves the card");
                    assert!(r.y >= f.card.y && r.y + r.h <= f.card.y + f.card.h, "{r:?} leaves the card");
                }
                assert!(f.content.y + f.content.h <= f.footer.y, "content overlaps the footer");
                for compound in [false, true] {
                    let l = list(f.content, &m, &s, compound);
                    assert!(l.visible >= 1);
                    assert!(
                        l.rows.y + l.visible * l.row_h <= f.footer.y + s.gap,
                        "rows run under the footer at {size:?} x{scale}"
                    );
                }
                assert!(f.nav.is_some() || f.stepper.is_some(), "categories stay reachable");
            }
            assert!(frame((200, 900), &m, &s).is_none(), "below the minimum the compact card is used");
        }
        // Same logical window at 100 % and 200 % gives exactly doubled geometry.
        let (a, b) = (Metrics::new(1.0), Metrics::new(2.0));
        let fa = frame((1280, 800), &a, &Sizes::new(&a)).unwrap();
        let fb = frame((2560, 1600), &b, &Sizes::new(&b)).unwrap();
        assert_eq!((fa.card.w * 2, fa.card.h * 2), (fb.card.w, fb.card.h));
        assert_eq!(fa.nav.is_some(), fb.nav.is_some());
    }

    #[test]
    #[cfg(windows)]
    fn settings_gui_paints_real_controls_and_hits_match_their_rows() {
        let mut source = Fake::new("[editor]\nword_wrap = true\n");
        source.menu.move_selection(30);
        let mut view = View::default();
        let size = (1280, 800);
        let (pixels, area) = paint(&source, size, 1.0, &mut view);
        let area = area.expect("card fits");
        let mut toggles = 0;
        let mut rows = 0;
        for (rect, target) in view.hits.iter() {
            assert!(area.contains((rect.x, rect.y)) && area.contains((rect.x + rect.w - 1, rect.y + rect.h - 1)));
            match *target {
                Target::Row { index, id } => {
                    rows += 1;
                    assert_eq!(source.rows()[index].spec.id, id, "row hit names its own schema id");
                }
                Target::Toggle { index, .. } => {
                    toggles += 1;
                    assert!(matches!(source.rows()[index].spec.kind, SettingKind::Bool));
                }
                _ => {}
            }
        }
        assert!(rows > 0 && toggles > 0, "switches are drawn controls with targets");
        let selected = source.selected();
        assert!(
            view.hits.iter().any(|(_, t)| matches!(t, Target::Row { index, .. } if *index == selected)),
            "keyboard selection is revealed"
        );
        // A second paint is pixel-identical: hover-free redraws damage nothing new.
        let (again, _) = paint(&source, size, 1.0, &mut view);
        assert_eq!(pixels, again);
        assert_eq!(view.epoch, 7);
    }

    #[test]
    #[cfg(windows)]
    fn settings_gui_narrow_and_short_windows_stack_rows_and_keep_categories() {
        let source = Fake::new("");
        let mut view = View::default();
        let (_, area) = paint(&source, (520, 420), 1.0, &mut view);
        assert!(area.is_some());
        assert!(view.hits.iter().any(|(_, t)| matches!(t, Target::CategoryStep { .. })), "stepper replaces the rail");
        assert!(!view.hits.iter().any(|(_, t)| matches!(t, Target::Category(_))));
        let mut tiny = View::default();
        let (_, area) = paint(&source, (240, 160), 1.0, &mut tiny);
        assert!(tiny.hits.len() == 0, "the compact card offers no stale targets");
        assert!(area.is_some_and(|a| a.w <= 240));
    }

    #[test]
    #[cfg(windows)]
    fn settings_gui_keymap_page_shows_keycaps_and_row_actions() {
        let mut source = Fake::new("").with_panel(SettingsPanel::Keymap);
        let doc = source.menu.document().clone();
        source.panel.as_mut().unwrap().search(&doc, "file.save").unwrap();
        let mut view = View::default();
        let _ = paint(&source, (1280, 800), 1.25, &mut view);
        let change = view.hits.iter().find_map(|(_, t)| match t {
            Target::PanelChange { index, print } => Some((*index, *print)),
            _ => None,
        });
        let (index, print) = change.expect("the selected shortcut offers Change");
        let window = source.window(index, 1).unwrap().unwrap();
        assert_eq!(fingerprint(&window.rows[0]), print, "the button names the drawn row");
        assert!(view.hits.iter().any(|(_, t)| *t == Target::Back));
        // Start capture: the page becomes the capture sheet with only its controls.
        source.panel.as_mut().unwrap().input(&doc, PanelInput::Accept).unwrap();
        let _ = paint(&source, (1280, 800), 1.25, &mut view);
        assert!(view.hits.iter().any(|(_, t)| *t == Target::CaptureCancel));
        assert!(!view.hits.iter().any(|(_, t)| matches!(t, Target::PanelChange { .. } | Target::CaptureAccept)));
    }

    fn overlaps(a: Rect, b: Rect) -> bool {
        a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
    }

    fn inside(outer: Rect, r: Rect) -> bool {
        r.x >= outer.x && r.y >= outer.y && r.x + r.w <= outer.x + outer.w && r.y + r.h <= outer.y + outer.h
    }

    /// Content rectangles stay inside the sheet column, apart from each other
    /// and above the reserved action row.
    fn assert_apart(column: Rect, actions: Rect, content: &[Rect], at: &str) {
        assert!(inside(column, actions), "actions leave the sheet at {at}");
        for (i, rect) in content.iter().enumerate() {
            assert!(inside(column, *rect), "{rect:?} leaves the sheet at {at}");
            assert!(!overlaps(*rect, actions), "{rect:?} is painted under the actions {actions:?} at {at}");
            for other in &content[i + 1..] {
                assert!(!overlaps(*rect, *other), "{rect:?} overlaps {other:?} at {at}");
            }
        }
    }

    #[test]
    fn settings_gui_entry_and_capture_sheets_reserve_their_actions_at_every_accepted_size() {
        // R3: at 520x376 the fixed sheet put the value field (y 282..314) under
        // the buttons (y 278..310) and help ran into the footer.
        let mut checked = 0;
        for scale in [1.0, 1.25, 1.5, 1.75, 2.0] {
            let m = Metrics::new(scale);
            let s = Sizes::new(&m);
            for logical in [(316, 376), (520, 376), (520, 377), (520, 420), (640, 480), (800, 600), (1280, 800)] {
                let size = ((logical.0 as f64 * scale) as usize, (logical.1 as f64 * scale) as usize);
                let at = format!("{size:?} x{scale}");
                let f = frame(size, &m, &s).unwrap_or_else(|| panic!("{at} is an accepted size"));
                let area = sheet_area(list(f.content, &m, &s, true), &s);
                assert!(area.y + area.h <= f.footer.y, "sheet runs into the footer at {at}");
                let w = area.w.min(m.dip(560.0));
                let column = Rect { w, ..area };
                for focused in [EditColumn::Key, EditColumn::Value] {
                    for help in 0..=3 {
                        for hint in [false, true] {
                            let plan = entry_layout(area, w, &s, focused, help, hint);
                            let focus = if focused == EditColumn::Key { plan.key } else { plan.value };
                            let field = match focus {
                                Slot::Full { field, .. } | Slot::Bare(field) => field,
                                slot => panic!("focused column is not editable at {at}: {slot:?}"),
                            };
                            assert_eq!(field.h, s.control, "the focused field keeps a full control at {at}");
                            let mut content = Vec::new();
                            for slot in [plan.key, plan.value] {
                                match slot {
                                    Slot::Full { label, field } => content.extend([label, field]),
                                    Slot::Bare(rect) | Slot::Summary(rect) => content.push(rect),
                                    Slot::Hidden => {}
                                }
                            }
                            let (top, lines) = plan.help;
                            assert!(lines <= help);
                            if lines > 0 {
                                content.push(Rect { x: area.x, y: top, w, h: lines * s.text });
                            }
                            content.extend(plan.hint);
                            assert_apart(column, plan.actions, &content, &at);
                        }
                    }
                }
                for title in [false, true] {
                    for notes in 0..=6 {
                        let plan = capture_layout(area, w, &m, &s, title, notes);
                        assert!(plan.keys.h >= s.control, "capture box too small at {at}");
                        let mut content = vec![plan.keys];
                        content.extend(plan.title);
                        let (top, lines) = plan.notes;
                        if lines > 0 {
                            content.push(Rect { x: area.x, y: top, w, h: lines * s.text });
                        }
                        assert_apart(column, plan.actions, &content, &at);
                    }
                }
                checked += 1;
            }
        }
        assert_eq!(checked, 35);
        // The review's geometry needs the compact form: the other column becomes a summary line.
        let m = Metrics::new(1.0);
        let s = Sizes::new(&m);
        let f = frame((520, 376), &m, &s).unwrap();
        let area = sheet_area(list(f.content, &m, &s, true), &s);
        // The native dialog has no duplicate client caption, so the actual sheet is roomier.
        // Retain the original cramped regression independently of that appearance change.
        assert!(area.h >= 124);
        let area = Rect { y: 186, h: 124, ..area };
        let plan = entry_layout(area, area.w.min(m.dip(560.0)), &s, EditColumn::Key, 3, false);
        assert!(matches!(plan.value, Slot::Summary(_)), "{plan:?}");
        let roomy = sheet_area(list(frame((520, 420), &m, &s).unwrap().content, &m, &s, true), &s);
        let plan = entry_layout(roomy, roomy.w.min(m.dip(560.0)), &s, EditColumn::Value, 3, true);
        assert!(matches!((plan.key, plan.value), (Slot::Full { .. }, Slot::Full { .. })), "{plan:?}");
    }

    /// Checks a painted sheet against its card: every hit and every painted
    /// pixel inside the card, the focused field apart from the actions, and
    /// the caret inside the focused field. Returns the field.
    #[cfg(windows)]
    fn check_sheet(view: &View, card: Rect, pixels: &[u32], size: (usize, usize), focus: Target) -> Rect {
        for (rect, target) in view.hits.iter() {
            assert!(inside(card, *rect), "{target:?} {rect:?} leaves the card {card:?}");
        }
        for (index, pixel) in pixels.iter().enumerate() {
            let point = (index % size.0, index / size.0);
            assert!(*pixel == 0 || card.contains(point), "painted outside the card at {point:?}");
        }
        let field = view.hits.iter().find_map(|(rect, t)| (*t == focus).then_some(*rect)).expect("focused field");
        for (rect, target) in view.hits.iter().filter(|(_, target)| *target != focus) {
            if matches!(
                target,
                Target::EntryAccept | Target::EntryCancel | Target::CaptureAccept | Target::CaptureCancel
            ) {
                assert!(!overlaps(field, *rect), "{target:?} {rect:?} covers the focused field {field:?}");
            }
        }
        field
    }

    #[test]
    #[cfg(windows)]
    fn settings_gui_painted_sheets_keep_fields_carets_and_actions_apart() {
        for (size, scale) in [((520, 376), 1.0), ((520, 420), 1.0), ((650, 470), 1.25), ((780, 564), 1.5)] {
            // Key/value entry on the file-type page: the key first, then the value.
            let mut source = Fake::new("").with_panel(SettingsPanel::FileTypes);
            let doc = source.menu.document().clone();
            let panel = source.panel.as_mut().unwrap();
            panel.search(&doc, "zz").unwrap();
            assert!(panel.input(&doc, PanelInput::Accept).unwrap().is_none());
            for column in [EditColumn::Key, EditColumn::Value] {
                let mut view = View::default();
                let (pixels, area) = paint(&source, size, scale, &mut view);
                let card = area.expect("accepted size");
                let field = check_sheet(&view, card, &pixels, size, Target::EntryColumn(column));
                assert!(view.hits.iter().any(|(_, t)| *t == Target::EntryAccept), "Apply/Next is offered");
                let caret = view.caret.expect("the focused field shows its caret");
                assert!(
                    field.contains((caret.x, caret.y)) && field.contains((caret.x, caret.y + caret.h - 1)),
                    "caret {caret:?} outside {field:?} at {size:?} x{scale}"
                );
                if column == EditColumn::Key {
                    let panel = source.panel.as_mut().unwrap();
                    panel.input(&doc, PanelInput::Text("rs")).unwrap();
                    assert!(panel.input(&doc, PanelInput::Accept).unwrap().is_none(), "Enter moves to the value");
                }
            }
            // Shortcut capture on the keymap page.
            let mut source = Fake::new("").with_panel(SettingsPanel::Keymap);
            let doc = source.menu.document().clone();
            let panel = source.panel.as_mut().unwrap();
            panel.search(&doc, "file.save").unwrap();
            panel.input(&doc, PanelInput::Accept).unwrap();
            panel.input(&doc, PanelInput::Capture("ctrl+alt+k".parse().unwrap())).unwrap();
            let mut view = View::default();
            let (pixels, area) = paint(&source, size, scale, &mut view);
            let card = area.expect("accepted size");
            check_sheet(&view, card, &pixels, size, Target::CaptureCancel);
            assert!(view.hits.iter().any(|(_, t)| *t == Target::CaptureAccept), "a captured chord can be used");
        }
    }

    #[test]
    #[cfg(windows)]
    fn settings_gui_wheel_and_hover_repaint_rows_and_hits_once() {
        // R1: wheel-only and hover-only input must reach render admission
        // (`Paint`) and the repaint must move the hit map with the rows.
        let source = Fake::new("");
        let mut view = View::default();
        let size = (1280, 800);
        let (before, _) = paint(&source, size, 1.0, &mut view);
        let first_row = |view: &View| view.hits.iter().filter_map(|(_, t)| t.row()).min();
        assert_eq!(first_row(&view), Some(0));
        assert!(view.wheel(false, false), "the list scrolls");
        assert!(view.paint.take(), "a wheel-only event is admitted as a card repaint");
        let (after, area) = paint(&source, size, 1.0, &mut view);
        assert_eq!(first_row(&view), Some(3), "the hit map follows the scrolled rows");
        let area = area.unwrap();
        assert_ne!(before, after, "the rows really moved");
        for (index, (a, b)) in before.iter().zip(&after).enumerate() {
            if a != b {
                assert!(area.contains((index % size.0, index / size.0)), "the repaint stays inside the card");
            }
        }
        let (rect, target) =
            view.hits.iter().find(|(_, t)| matches!(t, Target::Toggle { .. })).map(|(r, t)| (*r, *t)).unwrap();
        let point = (rect.x + 2, rect.y + rect.h / 2);
        assert!(view.track(point));
        assert_eq!(view.hover, Some(target));
        assert!(view.paint.take(), "a hover-only event is admitted as a card repaint");
        assert!(!view.track(point) && !view.paint.pending(), "an unchanged hover leaves no idle repeat");
        while view.wheel(false, false) {}
        view.paint.take();
        assert!(!view.wheel(false, false) && !view.paint.pending(), "the end of the list asks for nothing");
    }

    #[test]
    #[cfg(windows)]
    fn settings_gui_external_reload_drops_clicks_painted_for_the_old_value() {
        // R6: paint a clean switch, adopt an external reload that flips it, then
        // click before the repaint: the click must not toggle it back.
        let source = Fake::new("");
        let mut view = View::default();
        let size = (1280, 800);
        let m = Metrics::new(1.0);
        let _ = paint(&source, size, 1.0, &mut view);
        let surface = Surface::new(size, &m);
        let (rect, index, id) = view
            .hits
            .iter()
            .find_map(|(r, t)| match *t {
                Target::Toggle { index, id } => Some((*r, index, id)),
                _ => None,
            })
            .expect("a switch is painted");
        let point = (rect.x + 2, rect.y + rect.h / 2);
        let mut epoch = 7;
        assert_eq!(admit(&view, epoch, surface, point), Admit::Act(Target::Toggle { index, id }));
        let painted = match &source.rows()[index].current {
            SettingValue::Bool(on) => *on,
            other => panic!("switch row holds {other:?}"),
        };
        let shown = shown_document(Some(source.menu.document()));
        let (mut composing, mut destination) = (true, Some(ime::Destination::Settings { epoch }));
        // A save acknowledgement, or a reload parked beside a draft, keeps the
        // shown document: the painted controls stay live.
        let same = source.menu.document().clone();
        fence_reload(shown, shown_document(Some(&same)), &mut epoch, &mut composing, &mut destination);
        assert_eq!(epoch, 7);
        assert_eq!(admit(&view, epoch, surface, point), Admit::Act(Target::Toggle { index, id }));
        // A clean external reload is adopted at once and flips the switch.
        let mut external = source.menu.document().clone();
        external.edit(id, SettingValue::Bool(!painted)).unwrap();
        fence_reload(shown, shown_document(Some(&external)), &mut epoch, &mut composing, &mut destination);
        assert_eq!(admit(&view, epoch, surface, point), Admit::Stale, "the click meant for the old value is dropped");
        assert!(!composing && matches!(destination, Some(ime::Destination::Cancelled)), "late IME commits are fenced");
        // Resize and DPI changes before the repaint are stale too.
        assert_eq!(admit(&view, 7, Surface::new((1280, 700), &m), point), Admit::Stale);
        assert_eq!(admit(&view, 7, Surface::new(size, &Metrics::new(1.25)), point), Admit::Stale);
        // The repaint shows the external value and makes the switch live again.
        let reloaded = Fake::new(external.source());
        let mut font = font::UiFont::load(m.text_px()).expect("Windows chrome face");
        let mut pixels = vec![0u32; size.0 * size.1];
        draw(&reloaded, &mut pixels, size, &mut font, theme_ui::chrome(), &m, epoch, &mut view, None).unwrap();
        assert_eq!(reloaded.rows()[index].current, SettingValue::Bool(!painted), "the external value is retained");
        assert_eq!(admit(&view, epoch, surface, point), Admit::Act(Target::Toggle { index, id }));
    }
}
