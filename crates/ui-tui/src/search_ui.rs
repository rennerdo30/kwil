//! Terminal adapter for the shared search worker and interaction model.
//! Only cached viewport cells and bounded panel metadata are read on draw.
use super::*;
use kwil_view_model::search_panel::{SearchField, SearchInput, SearchMode, SearchOutcome, SearchPanel};
use kwil_view_model::search_session::{SearchAction, SearchSession, SelectionMode};
use std::time::Duration;

#[derive(Default)]
pub(super) struct SearchHost {
    session: Option<SearchSession>,
    panel: SearchPanel,
    origin: Option<Origin>,
    painted: Option<Painted>,
}

/// A cursor-only change must invalidate an asynchronous navigation too.
struct Origin {
    document: kwil_view_model::SessionDocumentId,
    epoch: u64,
    editor: Option<EditorOrigin>,
}
struct EditorOrigin {
    revision: u64,
    sequence: u64,
    cursors: Vec<kwil_view_model::Cursor>,
    primary: usize,
}
impl Origin {
    fn capture(view: &Workspace) -> Self {
        Self {
            document: view.active_id(),
            epoch: view.input_epoch(),
            editor: view.editor().map(|editor| EditorOrigin {
                revision: editor.document().revision(),
                sequence: editor.next_edit_sequence(),
                cursors: editor.cursors().to_vec(),
                primary: editor.primary_index(),
            }),
        }
    }
    fn matches(&self, view: &Workspace) -> bool {
        self.document == view.active_id()
            && self.epoch == view.input_epoch()
            && match (&self.editor, view.editor()) {
                (None, None) => true,
                (Some(origin), Some(editor)) => {
                    origin.revision == editor.document().revision()
                        && origin.sequence == editor.next_edit_sequence()
                        && origin.cursors == editor.cursors()
                        && origin.primary == editor.primary_index()
                }
                _ => false,
            }
    }
}

/// One bounded painted card. A click consumes the native action captured for
/// those exact cells, never an index into a newer worker page or layout.
struct Painted {
    bounds: Rect,
    area: Rect,
    epoch: u64,
    origin: Origin,
    fields: Vec<(Rect, SearchField)>,
    rows: Vec<(Rect, SearchAction)>,
}

impl SearchHost {
    pub(super) fn start(
        &mut self,
        view: &mut Workspace,
        keys: &mut KeymapEngine,
        wake: impl Fn() + Send + Sync + 'static,
    ) {
        match SearchSession::new(wake) {
            Ok(session) => {
                self.session = Some(session);
                self.sync(view, keys);
            }
            Err(error) => self.notice(view, format!("Find unavailable: {error}; query input is retained")),
        }
    }
    pub(super) fn is_open(&self) -> bool {
        self.panel.is_open()
    }
    pub(super) fn files_mode(&self) -> bool {
        self.panel.mode() == SearchMode::Files
    }
    /// Supplied by the existing worker's PaletteRoot event. Never resolve a
    /// directory or reconstruct a native path from its display on this thread.
    pub(super) fn root_ready(
        &mut self,
        view: &mut Workspace,
        keys: &mut KeymapEngine,
        root: std::path::PathBuf,
    ) -> bool {
        let result = match &mut self.session {
            Some(session) => self.panel.root_ready(session, view, root),
            None => self.panel.set_root(root).map(|_| SearchOutcome::default()),
        };
        match result {
            Ok(outcome) => self.outcome(view, keys, outcome),
            Err(error) => {
                self.notice(view, error);
                true
            }
        }
    }
    fn notice(&mut self, view: &mut Workspace, error: String) {
        self.panel.set_error(error.clone());
        view.set_notice(error);
    }
    fn outcome(&mut self, view: &mut Workspace, keys: &mut KeymapEngine, mut outcome: SearchOutcome) -> bool {
        let changed = outcome.damage.panel
            || outcome.damage.status
            || outcome.damage.signal
            || !outcome.damage.hit_rows.is_empty()
            || outcome.damage.restore_editor
            || outcome.action.is_some();
        if outcome.damage.status {
            view.set_notice(if outcome.damage.restore_editor { String::new() } else { self.panel.status().into() });
        }
        outcome.restore_context(keys);
        if let Some(action) = outcome.action.take() {
            self.action(view, keys, action);
        }
        changed
    }
    pub(super) fn command(
        &mut self,
        view: &mut Workspace,
        keys: &mut KeymapEngine,
        settings: &Settings,
        call: kwil_core_commands::vim::Invocation,
    ) -> bool {
        if view.file_prompt().is_some() {
            self.notice(view, "Finish or cancel the current file prompt before Find".into());
            return true;
        }
        if call.id == "search.substitute" {
            self.notice(view, "Vim substitution arguments are not attached; use Replace".into());
            return true;
        }
        self.origin = Some(Origin::capture(view));
        let result = match &mut self.session {
            Some(session) => self.panel.command(session, view, call.id, &settings.search),
            None => self
                .panel
                .open(
                    match call.id {
                        "search.replace" => SearchMode::Replace,
                        "search.findInFiles" | "search.replaceInFiles" | "search.setInclude" | "search.setExclude" => {
                            SearchMode::Files
                        }
                        _ => SearchMode::Find,
                    },
                    &settings.search,
                )
                .map(Some),
        };
        match result {
            Ok(Some(outcome)) => {
                self.panel.enter_context(keys);
                self.outcome(view, keys, outcome);
            }
            Ok(None) => self.notice(view, format!("Search command is not attached: {}", call.id)),
            Err(error) => {
                self.panel.enter_context(keys);
                self.notice(view, error);
            }
        }
        if self.session.is_none() {
            if let Some(field) = match call.id {
                "search.setInclude" => Some(SearchField::Include),
                "search.setExclude" => Some(SearchField::Exclude),
                _ => None,
            } {
                self.input(view, keys, SearchInput::Focus(field));
            }
            if call.id == "search.replaceInFiles" {
                self.notice(
                    view,
                    "Folder replacement awaits the atomic folder executor; Find in Files remains available".into(),
                );
            }
        }
        if call.id == "search.find"
            && let kwil_core_commands::vim::Argument::Search(query) = &call.argument
        {
            self.input(view, keys, SearchInput::SelectAll);
            self.input(view, keys, SearchInput::Text(query));
            self.input(view, keys, SearchInput::Next);
        }
        true
    }
    pub(super) fn input(&mut self, view: &mut Workspace, keys: &mut KeymapEngine, input: SearchInput<'_>) -> bool {
        self.origin = Some(Origin::capture(view));
        let result = match &mut self.session {
            Some(session) => self.panel.input(session, view, input),
            None => self.panel.deferred_input(input),
        };
        match result {
            Ok(outcome) => self.outcome(view, keys, outcome),
            Err(error) => {
                self.notice(view, error);
                true
            }
        }
    }
    pub(super) fn close(&mut self, view: &mut Workspace, keys: &mut KeymapEngine) {
        if self.is_open() {
            self.input(view, keys, SearchInput::Cancel);
        }
    }
    pub(super) fn ready(&mut self, view: &mut Workspace, keys: &mut KeymapEngine) -> bool {
        let Some(session) = &mut self.session else { return false };
        let outcome = self.panel.worker_ready(session, view);
        self.outcome(view, keys, outcome)
    }
    pub(super) fn sync(&mut self, view: &mut Workspace, keys: &mut KeymapEngine) -> bool {
        let Some(session) = &mut self.session else { return false };
        match self.panel.sync_document(session, view) {
            Ok(outcome) => self.outcome(view, keys, outcome),
            Err(error) => {
                self.notice(view, error);
                true
            }
        }
    }
    pub(super) fn settings(&mut self, view: &mut Workspace, keys: &mut KeymapEngine, settings: &Settings) {
        let previous_epoch = self.panel.epoch();
        let result = match &mut self.session {
            Some(session) => self.panel.settings_ready(session, view, &settings.search),
            None => self.panel.sync_settings(&settings.search).map(|_| SearchOutcome::default()),
        };
        // An unrelated reload cannot bless navigation issued before the
        // cursor moved. Changed policy invalidates that old followup in the
        // shared panel before a new origin is captured here.
        if self.panel.epoch() != previous_epoch {
            self.origin = Some(Origin::capture(view));
        }
        match result {
            Ok(outcome) => {
                self.outcome(view, keys, outcome);
            }
            Err(error) => self.notice(view, error),
        }
        self.panel.enter_context(keys);
    }
    fn action(&mut self, view: &mut Workspace, keys: &mut KeymapEngine, action: SearchAction) {
        let Some(session) = &mut self.session else { return };
        if !self.panel.validate_action(session, view, &action)
            || self.origin.as_ref().is_none_or(|origin| !origin.matches(view))
        {
            self.notice(view, "Search navigation cancelled: its document, query or cursor changed".into());
            return;
        }
        if let SearchAction::OpenFile { path, line_number, .. } = action {
            // Search ranges address ORIGINAL bytes (UTF-16 included). The
            // document worker's CLI navigation resolves decoded logical lines.
            if !path.is_absolute() || line_number == 0 {
                self.notice(view, "Folder result has an invalid native path or line".into());
                return;
            }
            let request = kwil_core_workspace::instance::OpenFiles {
                files: vec![kwil_core_workspace::instance::OpenFile { path, line: Some(line_number), column: Some(1) }],
            };
            self.origin = None;
            if let Err(error) = view.open_files(request) {
                self.notice(view, format!("Folder result could not open: {error}"));
            }
            self.panel.enter_context(keys);
            return;
        }
        if matches!(&action, SearchAction::Replace(_)) {
            let acknowledgement = match self.panel.replacement_acknowledgement(session, view, &action) {
                Ok(acknowledgement) => acknowledgement,
                Err(error) => {
                    self.notice(view, error);
                    return;
                }
            };
            let SearchAction::Replace(recipe) = action else { unreachable!() };
            match view.submit_search_replacement(recipe) {
                Ok(()) => {
                    let outcome = self.panel.replacement_admitted(session, acknowledgement);
                    self.origin = None;
                    self.outcome(view, keys, outcome);
                }
                Err(error) => self.notice(view, format!("Replacement was not admitted: {error}")),
            }
            return;
        }
        let result = match &action {
            SearchAction::Select { range, mode, .. } => {
                let editor = view.editor_mut().expect("validated editor");
                let mut cursors =
                    if *mode == SelectionMode::AddCursor { editor.cursors().to_vec() } else { Vec::new() };
                cursors.push(kwil_view_model::Cursor { anchor: range.start, head: range.end, desired_column: None });
                let primary = cursors.len() - 1;
                editor.set_cursors(cursors, primary)
            }
            SearchAction::SeedSelections { cursors, primary, from_selection, .. } => {
                if *from_selection {
                    Ok(Default::default())
                } else {
                    view.editor_mut().expect("validated editor").set_cursors(
                        cursors
                            .iter()
                            .map(|cursor| kwil_view_model::Cursor {
                                anchor: cursor.anchor,
                                head: cursor.head,
                                desired_column: None,
                            })
                            .collect(),
                        *primary,
                    )
                }
            }
            SearchAction::OpenFile { .. } => unreachable!("native file action handled above"),
            SearchAction::Replace(_) => unreachable!(),
        };
        match result {
            Ok(damage) => {
                view.refresh_after_edit(damage);
                let result = self.panel.action_applied(session, view, &action);
                self.origin = Some(Origin::capture(view));
                match result {
                    Ok(outcome) => {
                        self.outcome(view, keys, outcome);
                    }
                    Err(error) => self.notice(view, error),
                }
            }
            Err(error) => self.notice(view, error.to_string()),
        }
    }
    /// The actual message dispatcher and tests share this ordering: restore
    /// the loaned editor first, then acknowledge this exact replacement key.
    pub(super) fn document_result(
        &mut self,
        view: &mut Workspace,
        keys: &mut KeymapEngine,
        result: kwil_view_model::document_worker::DocumentResult,
    ) {
        use kwil_view_model::document_worker::DocumentResult;
        let replacement = match &result {
            DocumentResult::Replaced { key, result, .. } => {
                Some((*key, result.as_ref().map(|_| ()).map_err(ToString::to_string)))
            }
            _ => None,
        };
        view.document_result(result);
        if let Some((key, result)) = replacement
            && let Some(session) = &mut self.session
        {
            let outcome = self.panel.replacement_finished(session, view, key, result);
            self.origin = Some(Origin::capture(view));
            match outcome {
                Ok(outcome) => {
                    self.outcome(view, keys, outcome);
                }
                Err(error) => self.notice(view, error),
            }
        }
        // A receipt can unblock a retained replacement recipe even when the
        // search worker has no further results (and therefore no further wake).
        self.ready(view, keys);
    }
    /// Called on the shutdown lane, with all panel retry owners still alive.
    pub(super) fn into_shutdown_worker(mut self) -> Option<kwil_view_model::search_session::SearchWorker> {
        if let Some(mut session) = self.session.take() {
            match self.panel.take_shutdown(&mut session) {
                Ok(worker) => {
                    drop(self.panel);
                    drop(session);
                    return Some(worker);
                }
                Err(owner) => {
                    // Even this stopped-session fallback is disposed off UI.
                    drop(owner);
                }
            }
        }
        None
    }
    #[cfg(test)]
    fn shutdown(self) -> std::io::Result<()> {
        self.into_shutdown_worker()
            .map_or(Ok(()), |worker| worker.shutdown_bounded(Duration::from_secs(5)).map_err(std::io::Error::other))
    }
}

/// Search commands from both editor keys and palette actions meet here.
pub(super) fn resolve(
    host: &mut SearchHost,
    view: &mut Workspace,
    keys: &mut KeymapEngine,
    settings: &Settings,
    intent: KeyResult,
) -> KeyResult {
    match intent {
        KeyResult::Search(call) => KeyResult::Changed(host.command(view, keys, settings, call)),
        intent => {
            if !matches!(intent, KeyResult::Changed(_)) {
                host.close(view, keys);
            }
            intent
        }
    }
}

pub(super) fn key(
    host: &mut SearchHost,
    view: &mut Workspace,
    event: KeyEvent,
    keys: &mut KeymapEngine,
    settings: &Settings,
    elapsed: Duration,
) -> KeyResult {
    if !host.is_open() {
        return super::key(view, event, elapsed, keys, settings.keymap);
    }
    keys.set_context(kwil_core_commands::Context::Palette);
    if let Some(chord) = chord(event) {
        match keys.feed(chord) {
            VimResult::Pending => return KeyResult::Changed(false),
            VimResult::Error(error) => {
                host.notice(view, error);
                return KeyResult::Changed(true);
            }
            VimResult::Unbound if keys.suppresses_fallback() => return KeyResult::Changed(false),
            VimResult::Command(call) => {
                let input = match call.id {
                    "palette.cancel" => Some(SearchInput::Cancel),
                    "palette.accept" => Some(enter(&host.panel, event)),
                    "palette.next" if host.files_mode() => Some(SearchInput::RowNext),
                    "palette.previous" if host.files_mode() => Some(SearchInput::RowPrevious),
                    "motion.pageDown" if host.files_mode() => Some(SearchInput::PageNext),
                    "motion.pageUp" if host.files_mode() => Some(SearchInput::PagePrevious),
                    "palette.next" | "search.findNext" => Some(SearchInput::Next),
                    "palette.previous" | "search.findPrevious" => Some(SearchInput::Previous),
                    "edit.selectAll" => Some(SearchInput::SelectAll),
                    "edit.backspace" => Some(SearchInput::Backspace),
                    "edit.delete" => Some(SearchInput::Delete),
                    "motion.left" => Some(SearchInput::Left),
                    "motion.right" => Some(SearchInput::Right),
                    "motion.lineStart" | "motion.smartHome" => Some(SearchInput::Home),
                    "motion.lineEnd" => Some(SearchInput::End),
                    _ => None,
                };
                if let Some(input) = input {
                    return KeyResult::Changed(host.input(view, keys, input));
                }
                if !is_search(call.id) {
                    if call.id.starts_with("edit.") || call.id.starts_with("motion.") {
                        host.notice(view, "This editor shortcut is unavailable in the search field".into());
                        return KeyResult::Changed(true);
                    }
                    host.close(view, keys);
                }
                return invoke(view, call, false, elapsed, settings.keymap);
            }
            VimResult::Unbound => {}
        }
    }
    let ctrl = event.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER);
    let input = match event.code {
        KeyCode::Esc => SearchInput::Cancel,
        KeyCode::Tab if event.modifiers.contains(KeyModifiers::SHIFT) => SearchInput::PreviousField,
        KeyCode::BackTab => SearchInput::PreviousField,
        KeyCode::Tab => SearchInput::NextField,
        KeyCode::Enter => enter(&host.panel, event),
        KeyCode::F(3) if event.modifiers.contains(KeyModifiers::SHIFT) => SearchInput::Previous,
        KeyCode::Down if host.files_mode() => SearchInput::RowNext,
        KeyCode::Up if host.files_mode() => SearchInput::RowPrevious,
        KeyCode::PageDown if host.files_mode() => SearchInput::PageNext,
        KeyCode::PageUp if host.files_mode() => SearchInput::PagePrevious,
        KeyCode::F(3) | KeyCode::Down => SearchInput::Next,
        KeyCode::Up => SearchInput::Previous,
        KeyCode::Left => SearchInput::Left,
        KeyCode::Right => SearchInput::Right,
        KeyCode::Home => SearchInput::Home,
        KeyCode::End => SearchInput::End,
        KeyCode::Backspace => SearchInput::Backspace,
        KeyCode::Delete => SearchInput::Delete,
        KeyCode::Char(c) if !ctrl && event.modifiers.contains(KeyModifiers::ALT) => match c.to_ascii_lowercase() {
            'r' => SearchInput::ToggleRegex,
            'c' => SearchInput::ToggleCase,
            'w' => SearchInput::ToggleWord,
            's' => SearchInput::ToggleSelection,
            _ => return KeyResult::Changed(false),
        },
        KeyCode::Char('a' | 'A') if ctrl => SearchInput::SelectAll,
        KeyCode::Char('d' | 'D') if ctrl => SearchInput::AddCursor,
        KeyCode::Char(c) if !ctrl && !event.modifiers.contains(KeyModifiers::ALT) && !c.is_control() => {
            return KeyResult::Changed(host.input(view, keys, SearchInput::Text(&c.to_string())));
        }
        _ => return KeyResult::Changed(false),
    };
    KeyResult::Changed(host.input(view, keys, input))
}
fn enter(panel: &SearchPanel, event: KeyEvent) -> SearchInput<'static> {
    let shift = event.modifiers.contains(KeyModifiers::SHIFT);
    if panel.mode() == SearchMode::Files {
        SearchInput::SelectRow(panel.selected())
    } else if event.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER)
        && panel.mode() == SearchMode::Replace
    {
        if shift { SearchInput::ReplaceAll } else { SearchInput::ReplaceNext }
    } else if shift {
        SearchInput::Previous
    } else {
        SearchInput::Next
    }
}
pub(super) fn is_search(command: &str) -> bool {
    command.starts_with("search.") || command == "edit.selectNextMatch"
}

fn card(area: Rect, panel: &SearchPanel) -> Option<Rect> {
    let width = area.width.min(90);
    let height = if panel.mode() == SearchMode::Files {
        (panel.visible_fields().len() as u16 + 15).min(area.height)
    } else {
        (panel.visible_fields().len() as u16 + 4).min(area.height.saturating_sub(1))
    };
    (width != 0 && height != 0).then_some(Rect { x: area.x + area.width - width, y: area.y, width, height })
}

/// Field and result windows depend only on bounded metadata. Tiny terminals
/// keep the focused field and cancel hint; clicks never leak into the body.
fn visible_fields(panel: &SearchPanel, area: Rect) -> &[SearchField] {
    let fields = panel.visible_fields();
    if area.height < fields.len() as u16 + 4 || area.width < 24 {
        let index = fields.iter().position(|field| *field == panel.field()).unwrap_or(0);
        &fields[index..index + 1]
    } else {
        fields
    }
}
fn result_window(panel: &SearchPanel, area: Rect) -> (usize, u16, usize) {
    let first_y = visible_fields(panel, area).len() as u16 + 2;
    let rows = area.height.saturating_sub(first_y + 1) as usize;
    let first = panel.selected().saturating_add(1).saturating_sub(rows);
    (first, first_y, rows)
}
fn field_label(field: SearchField) -> &'static str {
    match field {
        SearchField::Query => "Find",
        SearchField::Replacement => "With",
        SearchField::Root => "Root",
        SearchField::Include => "Include",
        SearchField::Exclude => "Exclude",
    }
}
fn window(text: &str, cursor: usize, columns: usize) -> (String, usize) {
    let start =
        text[..cursor].char_indices().rev().take(columns.saturating_sub(1) / 2).last().map_or(cursor, |(at, _)| at);
    let cells = kwil_view_model::grid::cells(&text.as_bytes()[start..], columns, 4);
    let caret = cells.iter().take_while(|cell| cell.bytes.start < cursor - start).map(|cell| cell.width).sum();
    let label =
        cells.into_iter().map(|cell| if cell.text == "\t" { " ".repeat(cell.width) } else { cell.text }).collect();
    (label, caret)
}
pub(super) fn draw(
    frame: &mut ratatui::Frame,
    host: &mut SearchHost,
    view: &Workspace,
    layout: kwil_core_commands::settings::TabLayout,
) {
    host.painted = None;
    let bounds = frame.area();
    let geometry = tabs::geometry(view, bounds.width, bounds.height, layout);
    let body = geometry.body;
    // Recolour existing cells: syntax foreground, tabs and wide-cell geometry
    // stay in the normal text renderer. No snapshot reads or search on draw.
    let hit = Color::Rgb(0x73, 0x59, 0x16);
    for row in 0..if view.hex_active() { 0 } else { view.rows().min(body.height) } {
        for range in host.panel.highlight_columns(view, row, body.width.saturating_sub(8)) {
            for column in range {
                let x = bounds.x + body.x as u16 + 8 + column as u16;
                let y = bounds.y + body.y as u16 + row as u16;
                if x < bounds.right() && y < bounds.bottom() {
                    let cell = &mut frame.buffer_mut()[(x, y)];
                    // Selection remains distinguishable on top of matches.
                    if cell.bg == Color::Reset {
                        cell.set_bg(hit);
                    }
                }
            }
        }
    }
    if !view.hex_active() && body.width != 0 && body.height != 0 {
        for (index, count) in host.panel.signal().iter().enumerate().filter(|(_, count)| **count != 0) {
            let _ = count;
            let x = bounds.x + body.x as u16 + body.width as u16 - 1;
            let y = bounds.y + body.y as u16 + (index * body.height / host.panel.signal().len()) as u16;
            frame.buffer_mut()[(x, y)].set_bg(hit);
        }
    }
    if !host.is_open() {
        return;
    }
    let Some(area) = card(bounds, &host.panel) else { return };
    let mut painted = Painted {
        bounds,
        area,
        epoch: host.panel.epoch(),
        origin: Origin::capture(view),
        fields: Vec::with_capacity(4),
        rows: Vec::new(),
    };
    let style = Style::default().fg(Color::Rgb(0xd6, 0xe4, 0xf0)).bg(Color::Rgb(0x08, 0x0d, 0x18));
    frame.render_widget(ratatui::widgets::Clear, area);
    frame.render_widget(Paragraph::new("").style(style), area);
    if area.width < 12 || area.height < 3 {
        frame.render_widget(Paragraph::new("Esc cancel").style(style), area);
        host.painted = Some(painted);
        return;
    }
    let mut caret = None;
    let fields = visible_fields(&host.panel, area);
    for (row, field) in fields.iter().enumerate() {
        if row as u16 >= area.height.saturating_sub(2) {
            break;
        }
        let field_area = Rect { x: area.x, y: area.y + row as u16, width: area.width, height: 1 };
        let (text, cursor) = host.panel.text(*field);
        let (text, column) = window(text, cursor, area.width.saturating_sub(10) as usize);
        frame.render_widget(
            Paragraph::new(format!(
                "{} {:<7} {}",
                if *field == host.panel.field() { ">" } else { " " },
                field_label(*field),
                text
            ))
            .style(style),
            field_area,
        );
        painted.fields.push((field_area, *field));
        if *field == host.panel.field() {
            caret = Some((area.x + 10 + column.min(area.width.saturating_sub(11) as usize) as u16, field_area.y));
        }
    }
    let options = host.panel.options();
    let on = |enabled| if enabled { "on" } else { "off" };
    if host.files_mode() {
        let first = fields.len() as u16;
        frame.render_widget(
            Paragraph::new(format!(
                "Alt R regex:{}  C case:{}  W word:{}",
                on(options.regex),
                on(options.case_sensitive),
                on(options.whole_word)
            ))
            .style(style),
            Rect { x: area.x, y: area.y + first, width: area.width, height: 1 },
        );
        let (first_row, first_y, rows) = result_window(&host.panel, area);
        let page = host.session.as_ref().map(|session| session.state());
        let first_result = page.map_or(0, |state| state.file_first);
        let page_end = first_result.saturating_add(host.panel.results().len() as u64);
        frame.render_widget(
            Paragraph::new(format!(
                "{} | rows {}–{}",
                window(host.panel.status(), 0, area.width as usize).0,
                if page_end == 0 { 0 } else { first_result + 1 },
                page_end
            ))
            .style(style),
            Rect { x: area.x, y: area.y + first + 1, width: area.width, height: 1 },
        );
        for (visible, (index, row)) in host.panel.results().iter().enumerate().skip(first_row).take(rows).enumerate() {
            let selected = index == host.panel.selected();
            // Grid expansion escapes controls, so file names/previews cannot
            // inject terminal controls or split a row. Native path authority
            // remains solely in the SearchAction, never this display string.
            let (label, _) = window(&row.label, 0, area.width.saturating_sub(2) as usize);
            frame.render_widget(
                Paragraph::new(format!("{} {label}", if selected { ">" } else { " " })).style(if selected {
                    style.bg(Color::Rgb(0x1c, 0x35, 0x50))
                } else if !row.is_hit {
                    style.fg(Color::Rgb(0xff, 0xb0, 0x80))
                } else {
                    style
                }),
                Rect { x: area.x, y: area.y + first_y + visible as u16, width: area.width, height: 1 },
            );
            if row.is_hit
                && let Some(session) = &host.session
                && let Some(action) = session.file_action(index, 0)
                && host.panel.validate_action(session, view, &action)
            {
                painted.rows.push((
                    Rect { x: area.x, y: area.y + first_y + visible as u16, width: area.width, height: 1 },
                    action,
                ));
            }
        }
        let hint = if area.width < 24 {
            "Esc cancel  Tab field"
        } else if area.width < 40 {
            "Esc cancel  Enter open  PgUp/Dn"
        } else if area.width < 60 {
            "Enter open  PgUp/Dn page  Esc cancel"
        } else {
            "↑↓ select  Enter/click open  PgUp/Dn page  Tab field  Esc cancel"
        };
        frame.render_widget(
            Paragraph::new(hint).style(style),
            Rect { x: area.x, y: area.bottom() - 1, width: area.width, height: 1 },
        );
        if let Some(caret) = caret {
            frame.set_cursor_position(caret);
        }
        host.painted = Some(painted);
        return;
    }
    let lines = [
        format!(
            "Alt R regex:{}  C case:{}  W word:{}  S selection:{}",
            on(options.regex),
            on(options.case_sensitive),
            on(options.whole_word),
            on(options.in_selection)
        ),
        host.panel.status().to_owned(),
        if host.panel.mode() == SearchMode::Replace {
            "Enter next  Shift Enter previous  Ctrl Enter replace  Ctrl Shift Enter all  Tab field  Esc close".into()
        } else {
            "Enter/F3 next  Shift Enter/F3 previous  Tab field  Esc close".into()
        },
    ];
    let first = fields.len() as u16;
    for (row, text) in lines.iter().enumerate() {
        if first + row as u16 >= area.height {
            break;
        }
        frame.render_widget(
            Paragraph::new(text.as_str()).style(style),
            Rect { x: area.x, y: area.y + first + row as u16, width: area.width, height: 1 },
        );
    }
    if let Some(caret) = caret {
        frame.set_cursor_position(caret);
    }
    host.painted = Some(painted);
}

pub(super) fn mouse(
    host: &mut SearchHost,
    view: &mut Workspace,
    keys: &mut KeymapEngine,
    event: &event::MouseEvent,
    bounds: Rect,
) -> bool {
    let input = match event.kind {
        MouseEventKind::ScrollDown if host.files_mode() => SearchInput::RowNext,
        MouseEventKind::ScrollUp if host.files_mode() => SearchInput::RowPrevious,
        MouseEventKind::ScrollDown => SearchInput::Next,
        MouseEventKind::ScrollUp => SearchInput::Previous,
        MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
            let Some(painted) = host.painted.take() else {
                host.notice(view, "Search click cancelled: wait for the panel to redraw".into());
                return true;
            };
            if !host.is_open()
                || painted.bounds != bounds
                || painted.epoch != host.panel.epoch()
                || !painted.origin.matches(view)
            {
                host.notice(view, "Search click cancelled: its painted layout, query, tab or cursor changed".into());
                return true;
            }
            let point = (event.column, event.row).into();
            if !painted.area.contains(point) {
                SearchInput::Cancel
            } else if let Some((_, field)) = painted.fields.iter().find(|(rect, _)| rect.contains(point)) {
                SearchInput::Focus(*field)
            } else {
                if let Some((_, action)) = painted.rows.into_iter().find(|(rect, _)| rect.contains(point)) {
                    host.origin = Some(painted.origin);
                    host.action(view, keys, action);
                }
                return true;
            }
        }
        _ => return true,
    };
    host.input(view, keys, input);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use kwil_core_commands::{Binding, Context};
    use std::{path::PathBuf, time::Instant};

    struct Harness {
        host: SearchHost,
        view: Workspace,
        keys: KeymapEngine,
        settings: Settings,
        rx: mpsc::Receiver<Msg>,
        stopped: mpsc::Receiver<Result<(), String>>,
        root: PathBuf,
    }
    impl Harness {
        fn new(text: &str) -> Self {
            let mut view = Workspace::new(View::new(None)).unwrap();
            view.set_rows(12);
            let root = crate::test_support::directory("kwil-tui-search").unwrap();
            let (tx, rx) = mpsc::channel();
            let (stop_tx, stopped) = mpsc::sync_channel(1);
            let document_tx = tx.clone();
            view.start_document_worker_at(Some(root.clone()), move |result| {
                if let kwil_view_model::document_worker::DocumentResult::Stopped(result) = &result {
                    let _ = stop_tx.try_send(result.as_ref().map(|_| ()).map_err(ToString::to_string));
                }
                let _ = document_tx.send(Msg::Document(result));
            })
            .unwrap();
            let settings = Settings::default();
            let keys = KeymapEngine::new(settings.keymap, &settings.bindings).unwrap();
            let mut harness = Self { host: SearchHost::default(), view, keys, settings, rx, stopped, root };
            harness.until_stage("initial workspace recovery and editor", |h| {
                h.view.recovery_ready() && h.view.editor().is_some()
            });
            harness
                .view
                .execute("edit.insert", &CommandArgs { text: Some(text.into()), ..Default::default() }, 1)
                .unwrap();
            harness.view.sync_all();
            harness.receipts();
            harness.host.start(&mut harness.view, &mut harness.keys, move || {
                let _ = tx.send(Msg::SearchReady);
            });
            harness
        }
        fn dispatch(&mut self, message: Msg) {
            match message {
                Msg::SearchReady => {
                    self.host.ready(&mut self.view, &mut self.keys);
                }
                Msg::Document(result) => self.host.document_result(&mut self.view, &mut self.keys, result),
                _ => panic!("unexpected headless message"),
            }
            self.host.sync(&mut self.view, &mut self.keys);
        }
        fn until(&mut self, ready: impl Fn(&Self) -> bool) {
            self.until_stage("worker condition", ready);
        }
        fn until_stage(&mut self, stage: &str, ready: impl Fn(&Self) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !ready(self) {
                let message = self.rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap_or_else(
                    |error| {
                        // Failure-only, bounded cached state: no source reads,
                        // extra worker wakes or retained document owners.
                        let tabs: Vec<_> = self
                            .view
                            .session()
                            .tabs()
                            .iter()
                            .take(8)
                            .map(|tab| {
                                let view = self.view.view(tab.id);
                                (
                                    tab.id,
                                    self.view.document_busy(tab.id),
                                    self.view.binary_offer_pending(tab.id),
                                    view.and_then(|view| view.editor()).map(|editor| editor.primary_cursor().selection()),
                                    view.map(|view| view.notice().chars().take(256).collect::<String>()),
                                )
                            })
                            .collect();
                        panic!(
                            "bounded real TUI worker wake ({stage}): {error:?}; active={:?}; search_pending={} count={:?}; recovery_ready={} file_prompt={} binary_offer={}; tabs(id,busy,binary_pending,cursor,notice)={tabs:?}",
                            self.view.active_id(),
                            self.host.panel.is_pending(),
                            self.host.panel.count(),
                            self.view.recovery_ready(),
                            self.view.file_prompt().is_some(),
                            self.view.binary_offer().is_some(),
                        );
                    },
                );
                self.dispatch(message);
            }
        }
        fn receipts(&mut self) {
            self.until_stage("edit and recovery receipts", |h| {
                !h.view.search_replacement_pending()
                    && h.view.recovery_notice().is_none()
                    && h.view.editor().is_some_and(|editor| !editor.has_pending_edit_receipt())
            });
        }
        fn settle(&mut self) {
            self.until_stage("search panel completion", |h| !h.host.panel.is_pending());
        }
        fn press(&mut self, code: KeyCode, modifiers: KeyModifiers) -> KeyResult {
            let intent = key(
                &mut self.host,
                &mut self.view,
                KeyEvent::new(code, modifiers),
                &mut self.keys,
                &self.settings,
                Duration::ZERO,
            );
            let intent = resolve(&mut self.host, &mut self.view, &mut self.keys, &self.settings, intent);
            self.host.sync(&mut self.view, &mut self.keys);
            intent
        }
        fn ctrl(&mut self, c: char) {
            self.press(KeyCode::Char(c), KeyModifiers::CONTROL);
        }
        fn type_text(&mut self, text: &str) {
            for c in text.chars() {
                self.press(KeyCode::Char(c), KeyModifiers::NONE);
            }
        }
        fn query(&mut self, text: &str) {
            self.ctrl('a');
            self.type_text(text);
            self.settle();
        }
        fn command(&mut self, id: &'static str) {
            let VimResult::Command(call) = self.keys.invoke_named(id) else { panic!("registered search command") };
            let intent = invoke(&mut self.view, call, false, Duration::ZERO, self.settings.keymap);
            assert!(matches!(
                resolve(&mut self.host, &mut self.view, &mut self.keys, &self.settings, intent),
                KeyResult::Changed(true)
            ));
        }
        fn bytes(&self) -> Vec<u8> {
            let snapshot = self.view.editor().unwrap().document().snapshot();
            snapshot.chunks(0..snapshot.len()).unwrap().flat_map(|chunk| chunk.unwrap().to_vec()).collect()
        }
        fn selection(&self) -> std::ops::Range<u64> {
            self.view.editor().unwrap().primary_cursor().selection()
        }
        fn render(&mut self, width: u16, height: u16) -> ratatui::Terminal<ratatui::backend::TestBackend> {
            let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    tabs::draw(frame, &self.view, self.settings.tabs.layout);
                    draw(frame, &mut self.host, &self.view, self.settings.tabs.layout);
                })
                .unwrap();
            terminal
        }
        fn files(&mut self, root: PathBuf, query: &str) {
            self.host.root_ready(&mut self.view, &mut self.keys, root);
            self.press(KeyCode::Char('f'), KeyModifiers::CONTROL | KeyModifiers::SHIFT);
            assert_eq!(self.host.panel.mode(), SearchMode::Files, "actual configured folder command dispatch");
            self.query(query);
        }
        fn field(&mut self, field: SearchField, text: &str) {
            self.host.input(&mut self.view, &mut self.keys, SearchInput::Focus(field));
            self.ctrl('a');
            // Bracketed paste uses this same bounded input dispatcher.
            self.host.input(&mut self.view, &mut self.keys, SearchInput::Text(text));
            self.settle();
        }
        fn click(&mut self, column: u16, row: u16, width: u16, height: u16) {
            assert!(mouse(
                &mut self.host,
                &mut self.view,
                &mut self.keys,
                &event::MouseEvent {
                    kind: MouseEventKind::Down(event::MouseButton::Left),
                    column,
                    row,
                    modifiers: KeyModifiers::NONE
                },
                Rect::new(0, 0, width, height)
            ));
        }
        fn finish(self) {
            let root = self.root.clone();
            let cleanup = std::thread::spawn(move || {
                shutdown_search_workspace(self.view, self.host, self.rx, self.stopped, true)
            });
            cleanup.join().unwrap().unwrap();
            assert_eq!(root.parent(), Some(std::env::temp_dir().as_path()));
            assert!(root.file_name().unwrap().to_string_lossy().starts_with("kwil-tui-search-"));
            std::fs::remove_dir_all(root).unwrap();
        }
    }
    fn binding(keys: &str, command: &str, context: Option<Context>) -> Binding {
        Binding { keys: keys.into(), command: command.into(), context, mode: None }
    }

    #[test]
    fn folder_r1_resize_between_paint_and_click_cannot_reinterpret_a_row() {
        let mut h = Harness::new("original");
        let tree = h.root.join("paint-resize");
        std::fs::create_dir_all(&tree).unwrap();
        let path = tree.join("hit.txt");
        std::fs::write(&path, "needle\n".repeat(8)).unwrap();
        h.files(tree, "needle");
        h.render(100, 20);
        let row = h.host.painted.as_ref().unwrap().rows[0].0;
        let original = h.view.active_id();
        // Narrow geometry displays one field instead of four. Recomputing
        // it at click time maps these painted row-one cells to row four.
        h.click(row.x + 2, row.y, 20, 20);
        assert_eq!(h.view.active_id(), original, "resize must reject the old painted click");
        assert!(h.host.panel.status().contains("painted layout"));
        assert!(h.host.is_open(), "a stale click cannot cancel or fall through to the editor");
        h.render(20, 20);
        let row = h.host.painted.as_ref().unwrap().rows[0].0;
        h.click(row.x + 2, row.y, 20, 20);
        h.until(|h| h.view.editor().is_some_and(|editor| editor.document().path() == Some(path.as_path())));
        h.until(|h| h.selection() == (0..0));
        h.finish();
    }

    #[test]
    fn folder_r1_unpainted_page_and_cursor_or_tab_changes_cannot_retarget_clicks() {
        let mut h = Harness::new("original");
        let tree = h.root.join("paint-page");
        std::fs::create_dir_all(&tree).unwrap();
        let path = tree.join("many.txt");
        std::fs::write(&path, "needle\n".repeat(300)).unwrap();
        h.files(tree, "needle");
        h.render(100, 20);
        let row = h.host.painted.as_ref().unwrap().rows[0].0;
        let original = h.view.active_id();
        let old_first = h.host.session.as_ref().unwrap().state().file_first;
        h.press(KeyCode::PageUp, KeyModifiers::NONE);
        h.settle();
        assert_ne!(h.host.session.as_ref().unwrap().state().file_first, old_first);
        h.click(row.x + 2, row.y, 100, 20);
        assert_eq!(h.view.active_id(), original, "a new worker page was not painted for this click");

        h.render(100, 20);
        let row = h.host.painted.as_ref().unwrap().rows[0].0;
        h.view
            .editor_mut()
            .unwrap()
            .set_cursors(vec![kwil_view_model::Cursor { anchor: 2, head: 2, desired_column: None }], 0)
            .unwrap();
        h.click(row.x + 2, row.y, 100, 20);
        assert_eq!(h.view.active_id(), original, "a click cannot bless a newer cursor");
        assert_eq!(h.selection(), 2..2);

        h.render(100, 20);
        h.view.file_command("file.new").unwrap();
        let next = h.view.active_id();
        h.host.sync(&mut h.view, &mut h.keys);
        h.click(row.x + 2, row.y, 100, 20);
        assert_eq!(h.view.active_id(), next, "a click cannot bless a newer tab");
        assert_eq!(h.host.panel.count(), (300, true));

        h.render(100, 20);
        let row = h.host.painted.as_ref().unwrap().rows[0].0;
        let first = h.host.session.as_ref().unwrap().state().file_first;
        h.click(row.x + 2, row.y, 100, 20);
        h.until(|h| h.view.editor().is_some_and(|editor| editor.document().path() == Some(path.as_path())));
        h.until(|h| h.selection() == (first * 7..first * 7));
        h.finish();
    }

    #[test]
    fn checked_open_missing_path_preserves_open_failure_and_indexed_failure_stage() {
        use kwil_view_model::document_worker::DocumentResult;

        let mut h = Harness::new("original");
        let failed = h.view.open_path(h.root.join("missing-parent/absent.txt")).unwrap();
        assert!(h.view.editor().is_none(), "actual checked-open placeholder has no editor");
        assert!(h.view.binary_offer_pending(failed), "checked open owns the pending source decision");
        h.until_stage("missing-path checked-open completion", |h| !h.view.binary_offer_pending(failed));
        assert_eq!(h.view.active_id(), failed);
        assert!(h.view.editor().is_none(), "failed checked open must not fabricate an editor");
        assert!(!h.view.document_busy(failed), "the error completion releases the checked-open flight");
        assert!(h.view.binary_offer().is_none(), "an unavailable source cannot offer a binary choice");
        assert!(
            h.view.notice().starts_with("Open failed:"),
            "missing-path failure belongs to open, not indexing: {}",
            h.view.notice()
        );

        // Keep the distinct legacy index-result branch covered through the
        // same host dispatcher. Only this control event is synthetic; the
        // missing-path completion above comes from the actual document worker.
        h.dispatch(Msg::Document(DocumentResult::Indexed {
            document: failed,
            revision: 0,
            result: Err(std::io::Error::other("injected index-stage failure")),
        }));
        assert!(h.view.editor().is_none());
        assert_eq!(h.view.notice(), "Index failed: injected index-stage failure");
        h.finish();
    }

    #[test]
    fn folder_r1_files_open_from_loading_and_failed_tabs_without_an_editor() {
        let mut h = Harness::new("original");
        let tree = h.root.join("editor-absent");
        std::fs::create_dir_all(&tree).unwrap();
        let path = tree.join("hit.txt");
        std::fs::write(&path, "prefix\nneedle\n").unwrap();
        h.files(tree, "needle");
        let failed = h.view.open_path(h.root.join("missing-parent/absent.txt")).unwrap();
        assert!(h.view.editor().is_none(), "actual opening placeholder has no editor");
        h.host.sync(&mut h.view, &mut h.keys);
        h.press(KeyCode::Enter, KeyModifiers::NONE);
        assert_ne!(h.view.active_id(), failed, "Files keyboard open is independent of the absent editor");
        h.until_stage("loading-tab result editor", |h| {
            h.view.editor().is_some_and(|editor| editor.document().path() == Some(path.as_path()))
        });
        h.until_stage("loading-tab result line cursor", |h| h.selection() == (7..7));
        h.until_stage("missing-path open failure notice", |h| {
            h.view.view(failed).is_some_and(|view| view.notice().contains("Open failed"))
        });
        assert!(h.view.activate(failed));
        assert!(h.view.editor().is_none(), "a real failed-open tab still has no editor");
        h.host.sync(&mut h.view, &mut h.keys);
        h.render(100, 20);
        let row = h.host.painted.as_ref().unwrap().rows[0].0;
        h.click(row.x + 2, row.y, 100, 20);
        assert_ne!(h.view.active_id(), failed, "painted Files mouse open also works without an editor");
        h.until_stage("failed-tab result line cursor", |h| h.selection() == (7..7));
        h.finish();
    }

    #[test]
    fn folder_r1_editor_arrival_invalidates_an_editorless_painted_origin() {
        let mut h = Harness::new("original");
        let tree = h.root.join("editor-arrival");
        std::fs::create_dir_all(&tree).unwrap();
        let path = tree.join("hit.txt");
        let loading_path = h.root.join("loading.txt");
        std::fs::write(&path, "needle\n").unwrap();
        std::fs::write(&loading_path, "unrelated\n").unwrap();
        h.files(tree, "needle");
        let loading = h.view.open_path(loading_path).unwrap();
        assert!(h.view.editor().is_none());
        h.render(100, 20);
        let row = h.host.painted.as_ref().unwrap().rows[0].0;
        h.until(|h| h.view.editor().is_some());
        h.click(row.x + 2, row.y, 100, 20);
        assert_eq!(h.view.active_id(), loading, "editor arrival cannot validate the old editorless click");
        h.render(100, 20);
        let row = h.host.painted.as_ref().unwrap().rows[0].0;
        h.click(row.x + 2, row.y, 100, 20);
        h.until(|h| h.view.editor().is_some_and(|editor| editor.document().path() == Some(path.as_path())));
        h.finish();
    }

    #[test]
    fn folder_r1_files_open_while_replacement_editor_is_loaned_then_shutdown_drains_it() {
        use kwil_view_model::document_worker::DocumentResult;
        let mut h = Harness::new("one one");
        let tree = h.root.join("loaned-editor");
        std::fs::create_dir_all(&tree).unwrap();
        let path = tree.join("hit.txt");
        std::fs::write(&path, "needle\n").unwrap();
        h.host.root_ready(&mut h.view, &mut h.keys, tree);
        replacement(&mut h, "one", "x");
        h.command("search.replaceAll");
        h.until(|h| h.view.search_replacement_pending());
        let loaned = h.view.active_id();
        assert!(h.view.editor().is_none());
        h.command("search.findInFiles");
        h.host.input(&mut h.view, &mut h.keys, SearchInput::SelectAll);
        h.host.input(&mut h.view, &mut h.keys, SearchInput::Text("needle"));
        // Hold only the one actual editor-return message, deterministically
        // exercising Files admission while the Workspace still owns a loan.
        let mut held = None;
        let deadline = Instant::now() + Duration::from_secs(10);
        while h.host.panel.count() != (1, true) {
            let message = h.rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
            match message {
                Msg::Document(result @ DocumentResult::Replaced { .. }) => {
                    assert!(held.replace(result).is_none(), "one bounded retained editor return");
                }
                message => h.dispatch(message),
            }
        }
        assert_eq!(h.view.active_id(), loaned);
        assert!(h.view.editor().is_none());
        h.press(KeyCode::Enter, KeyModifiers::NONE);
        assert_ne!(h.view.active_id(), loaned, "a loan in another tab cannot block Files open");
        if let Some(result) = held {
            h.dispatch(Msg::Document(result));
        }
        // Production shutdown retains the queued return if it was not yet
        // delivered, drains the real loan and releases history before search.
        h.finish();
    }

    #[test]
    fn folder_dispatch_ignore_include_exclude_options_and_compact_modal() {
        let mut h = Harness::new("editor stays unchanged");
        let tree = h.root.join("tree");
        std::fs::create_dir_all(tree.join(".git")).unwrap();
        std::fs::write(tree.join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(tree.join("ignored.txt"), "needle\n").unwrap();
        std::fs::write(tree.join("keep.txt"), "needle\nNEEDLE\nneedles\n").unwrap();
        std::fs::write(tree.join("skip.txt"), "needle\n").unwrap();
        std::fs::write(tree.join("keep.rs"), "needle\n").unwrap();
        h.files(tree.clone(), "needle");
        assert_eq!(h.host.panel.count(), (5, true), "gitignored hit is excluded");
        h.command("search.setInclude");
        assert_eq!(h.host.panel.field(), SearchField::Include);
        h.field(SearchField::Include, "*.txt");
        assert_eq!(h.host.panel.count(), (4, true), "include never overrides gitignore");
        h.command("search.setExclude");
        assert_eq!(h.host.panel.field(), SearchField::Exclude);
        h.field(SearchField::Exclude, "skip.txt");
        assert_eq!(h.host.panel.count(), (3, true));
        h.press(KeyCode::Char('c'), KeyModifiers::ALT);
        h.settle();
        assert_eq!(h.host.panel.count(), (2, true));
        h.press(KeyCode::Char('w'), KeyModifiers::ALT);
        h.settle();
        assert_eq!(h.host.panel.count(), (1, true));
        h.press(KeyCode::Char('r'), KeyModifiers::ALT);
        h.field(SearchField::Query, "n(eed)le");
        assert_eq!(h.host.panel.count(), (1, true));
        let frame = h.render(100, 20);
        let display: String = frame.backend().buffer().content.iter().map(|cell| cell.symbol()).collect();
        assert!(display.contains("keep.txt:1"), "real results are drawn in the terminal card");
        assert!(display.contains("PgUp/Dn"));
        for (width, height) in [(0, 0), (1, 1), (8, 2), (12, 4), (24, 8), (60, 10)] {
            h.render(width, height);
            h.press(KeyCode::Delete, KeyModifiers::CONTROL);
            assert!(h.host.is_open());
            assert_eq!(h.bytes(), b"editor stays unchanged", "modal input must never fall through to the body");
        }
        h.command("search.replaceInFiles");
        assert!(!h.host.panel.can_replace());
        assert!(h.host.panel.status().contains("atomic folder executor"));
        assert_eq!(std::fs::read(tree.join("keep.txt")).unwrap(), b"needle\nNEEDLE\nneedles\n");
        h.finish();
    }

    #[test]
    fn folder_count_over_256_revisits_disk_pages_and_keyboard_selects_without_opening() {
        let mut h = Harness::new("retain editor");
        let tree = h.root.join("pages");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join("many.txt"), "needle\n".repeat(300)).unwrap();
        h.files(tree, "needle");
        assert_eq!(h.host.panel.count(), (300, true));
        assert!(h.host.panel.results().len() <= 32);
        // Streaming also flushes partial batches on its 100 ms cadence, so
        // the completed tail can start at e.g. 274 instead of a multiple of
        // 32. Keyboard pages step by 32 and saturate at zero: their round trip
        // must start on that deterministic grid, not a timing-dependent batch.
        let streamed_last = h.host.session.as_ref().unwrap().state().file_first;
        assert!(streamed_last >= 256);
        for _ in 0..streamed_last.div_ceil(32) {
            h.press(KeyCode::PageUp, KeyModifiers::NONE);
            h.settle();
            assert!(h.host.panel.results().len() <= 32);
            assert_eq!(h.host.panel.count(), (300, true));
        }
        assert_eq!(h.host.session.as_ref().unwrap().state().file_first, 0);
        assert!(h.host.panel.results()[0].label.contains("many.txt:1 "));
        let canonical_last = (300_u64 - 1) / 32 * 32;
        for _ in 0..canonical_last / 32 {
            h.press(KeyCode::PageDown, KeyModifiers::NONE);
            h.settle();
            assert!(h.host.panel.results().len() <= 32);
            assert_eq!(h.host.panel.count(), (300, true));
        }
        let last = h.host.session.as_ref().unwrap().state().file_first;
        assert_eq!(last, 288);
        assert!(h.host.panel.results()[0].label.contains("many.txt:289 "));
        assert!(h.host.panel.results().last().unwrap().label.contains("many.txt:300 "));
        assert!(last >= 256);
        let active = h.view.active_id();
        h.press(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(h.view.active_id(), active, "arrow only selects, Enter opens");
        assert_eq!(h.host.panel.selected(), 1);
        for _ in 0..last.div_ceil(32) {
            h.press(KeyCode::PageUp, KeyModifiers::NONE);
            h.settle();
            assert!(h.host.panel.results().len() <= 32);
            assert_eq!(h.host.panel.count(), (300, true));
        }
        assert_eq!(h.host.session.as_ref().unwrap().state().file_first, 0);
        assert!(h.host.panel.results()[0].label.contains("many.txt:1 "));
        for _ in 0..last.div_ceil(32) {
            h.press(KeyCode::PageDown, KeyModifiers::NONE);
            h.settle();
        }
        assert_eq!(h.host.session.as_ref().unwrap().state().file_first, last);
        assert!(h.host.panel.results()[0].label.contains("many.txt:289 "));
        assert!(h.host.panel.results().last().unwrap().label.contains("many.txt:300 "));
        assert_eq!(h.host.panel.count(), (300, true));
        let frame = h.render(80, 12);
        let display: String = frame.backend().buffer().content.iter().map(|cell| cell.symbol()).collect();
        assert!(display.contains("300 matching lines"));
        assert_eq!(h.bytes(), b"retain editor");
        h.finish();
    }

    #[test]
    fn folder_utf16_result_opens_native_path_at_decoded_line_column_one_by_enter_and_mouse() {
        let mut h = Harness::new("original");
        let tree = h.root.join("utf16");
        std::fs::create_dir_all(&tree).unwrap();
        let path = tree.join("日本語.txt");
        let text = "α😀 first\nneedle target\nlast\n";
        let encoded: Vec<u8> = [0xff, 0xfe].into_iter().chain(text.encode_utf16().flat_map(u16::to_le_bytes)).collect();
        std::fs::write(&path, encoded).unwrap();
        h.files(tree, "needle");
        assert_eq!(h.host.panel.count(), (1, true));
        let original = h.view.active_id();
        h.press(KeyCode::Enter, KeyModifiers::NONE);
        h.until(|h| h.view.editor().is_some_and(|editor| editor.document().path() == Some(path.as_path())));
        h.until(|h| h.selection() == (13..13));
        assert_ne!(h.view.active_id(), original);
        assert_eq!(h.selection(), 13..13, "original UTF-16 match byte ranges are never UTF-8 cursor offsets");
        assert!(h.host.is_open());
        assert_eq!(h.host.panel.count(), (1, true), "folder results remain after a tab changes");
        assert!(h.view.activate(original));
        h.host.sync(&mut h.view, &mut h.keys);
        h.render(100, 20);
        let area = card(Rect::new(0, 0, 100, 20), &h.host.panel).unwrap();
        let (_, first_y, _) = result_window(&h.host.panel, area);
        h.click(area.x + 2, area.y + first_y, 100, 20);
        h.until(|h| h.view.editor().is_some_and(|editor| editor.document().path() == Some(path.as_path())));
        h.until(|h| h.selection() == (13..13));
        assert_eq!(h.host.panel.count(), (1, true));
        h.finish();
    }

    #[test]
    fn folder_stale_query_tab_cursor_actions_are_rejected_but_new_tab_actions_work() {
        let mut h = Harness::new("original");
        let tree = h.root.join("stale");
        std::fs::create_dir_all(&tree).unwrap();
        let path = tree.join("hit.txt");
        std::fs::write(&path, "needle\n").unwrap();
        h.files(tree, "needle");
        let action = h.host.session.as_ref().unwrap().file_action(0, 0).unwrap();
        h.view
            .editor_mut()
            .unwrap()
            .set_cursors(vec![kwil_view_model::Cursor { anchor: 2, head: 2, desired_column: None }], 0)
            .unwrap();
        let original = h.view.active_id();
        h.host.action(&mut h.view, &mut h.keys, action);
        assert_eq!(h.view.active_id(), original);
        assert_eq!(h.selection(), 2..2);
        h.host.input(&mut h.view, &mut h.keys, SearchInput::RowNext);
        let action = h.host.session.as_ref().unwrap().file_action(0, 0).unwrap();
        h.view.file_command("file.new").unwrap();
        let next = h.view.active_id();
        h.host.sync(&mut h.view, &mut h.keys);
        h.host.action(&mut h.view, &mut h.keys, action);
        assert_eq!(h.view.active_id(), next, "old user action cannot follow a tab change");
        h.host.input(&mut h.view, &mut h.keys, SearchInput::RowNext);
        let action = h.host.session.as_ref().unwrap().file_action(0, 0).unwrap();
        h.query("missing");
        h.host.action(&mut h.view, &mut h.keys, action);
        assert_eq!(h.view.active_id(), next, "old key cannot act after the query changes");
        h.query("needle");
        h.press(KeyCode::Enter, KeyModifiers::NONE);
        h.until(|h| h.view.editor().is_some_and(|editor| editor.document().path() == Some(path.as_path())));
        h.finish();
    }

    #[test]
    fn folder_cancel_and_bounded_shutdown_release_retained_replace_history() {
        let mut h = Harness::new("one one");
        replacement(&mut h, "one", "x");
        h.command("search.replaceAll");
        h.settle();
        h.receipts();
        assert_eq!(h.bytes(), b"x x");
        let tree = h.root.join("cancel");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join("many.txt"), "needle\n".repeat(300)).unwrap();
        h.host.root_ready(&mut h.view, &mut h.keys, tree);
        h.command("search.findInFiles");
        h.host.input(&mut h.view, &mut h.keys, SearchInput::SelectAll);
        h.host.input(&mut h.view, &mut h.keys, SearchInput::Text("needle"));
        let action_key = h.host.session.as_ref().unwrap().key();
        h.press(KeyCode::Esc, KeyModifiers::NONE);
        assert!(!h.host.is_open());
        assert_ne!(h.host.session.as_ref().unwrap().key(), action_key);
        assert!(h.host.panel.results().is_empty());
        assert_eq!(h.bytes(), b"x x", "replacement history stays alive until actual production shutdown");
        h.finish(); // unchanged bounded shutdown drains loans, drops owners off UI, then retires search
    }

    #[test]
    fn folder_delayed_worker_root_and_explicit_root_field_restart_the_query() {
        let mut h = Harness::new("original");
        let tree = h.root.join("delayed");
        let other = h.root.join("other");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(tree.join("hit.txt"), "needle\n").unwrap();
        std::fs::write(other.join("more.txt"), "needle\nneedle\n").unwrap();
        h.command("search.findInFiles");
        h.query("needle");
        assert!(h.host.panel.status().contains("root"));
        h.host.root_ready(&mut h.view, &mut h.keys, tree);
        h.settle();
        assert_eq!(h.host.panel.count(), (1, true), "a query before PaletteRoot must resume on its worker event");
        h.field(SearchField::Root, other.to_str().unwrap());
        assert_eq!(h.host.panel.count(), (2, true), "an explicit root edit is an actual query input");
        h.render(24, 8);
        h.press(KeyCode::Esc, KeyModifiers::NONE);
        assert!(!h.host.is_open());
        assert_eq!(h.bytes(), b"original");
        h.finish();
    }

    #[test]
    #[cfg(any(unix, windows))]
    fn folder_lossy_native_root_and_result_display_never_replace_path_authority() {
        #[cfg(windows)]
        let native = {
            use std::os::windows::ffi::OsStringExt;
            std::ffi::OsString::from_wide(&[b'n' as u16, 0xd800, b'x' as u16])
        };
        #[cfg(unix)]
        let native = {
            use std::os::unix::ffi::OsStringExt;
            std::ffi::OsString::from_vec(vec![b'n', 0xff, b'x'])
        };
        // Synthetic native paths exercise lossy-display guards even on APFS,
        // which cannot materialize the invalid UTF-8 filesystem component.
        let synthetic_root = std::env::temp_dir().join(&native);
        assert!(synthetic_root.to_str().is_none());
        let mut panel = SearchPanel::default();
        panel.set_root(synthetic_root).unwrap();
        panel.open(SearchMode::Files, &Settings::default().search).unwrap();
        panel.deferred_input(SearchInput::Focus(SearchField::Root)).unwrap();
        let displayed = panel.text(SearchField::Root).0.to_owned();
        for input in [SearchInput::Text("-changed"), SearchInput::Backspace, SearchInput::Delete] {
            assert!(panel.deferred_input(input).is_err(), "display text must not partially replace a native root");
            assert_eq!(panel.text(SearchField::Root).0, displayed);
        }
        panel.deferred_input(SearchInput::SelectAll).unwrap();
        #[cfg(windows)]
        let replacement = "C:/kwil-native-root-replacement";
        #[cfg(unix)]
        let replacement = "/kwil-native-root-replacement";
        panel.deferred_input(SearchInput::Text(replacement)).unwrap();
        assert_eq!(panel.text(SearchField::Root).0, replacement, "an explicit complete replacement remains available");

        #[cfg(target_os = "macos")]
        let filesystem_native = std::ffi::OsString::from("native-界-é");
        #[cfg(not(target_os = "macos"))]
        let filesystem_native = native;
        let mut h = Harness::new("original");
        let tree = h.root.join(&filesystem_native);
        std::fs::create_dir_all(&tree).unwrap();
        let path = tree.join(filesystem_native);
        std::fs::write(&path, "needle\n").unwrap();
        h.files(tree, "needle");
        assert_eq!(h.host.panel.count(), (1, true));
        let action = h.host.session.as_ref().unwrap().file_action(0, 0).unwrap();
        assert!(matches!(&action, SearchAction::OpenFile { path: actual, .. } if actual == &path));
        h.render(40, 10); // Unicode/replacement characters are display only
        h.press(KeyCode::Enter, KeyModifiers::NONE);
        h.until(|h| h.view.editor().is_some_and(|editor| editor.document().path() == Some(path.as_path())));
        assert_eq!(h.selection(), 0..0);
        h.finish();
    }

    #[test]
    fn real_dispatch_ctrl_f_incremental_count_navigation_highlights_and_close() {
        let original = "one\nfiller\nfiller\nfiller\nfiller\nfiller\none one";
        let mut h = Harness::new(original);
        h.ctrl('f');
        assert!(h.host.is_open());
        h.type_text("on");
        h.settle();
        assert_eq!(h.host.panel.count(), (3, true));
        h.type_text("e");
        h.settle();
        let frame = h.render(110, 14);
        let body = tabs::geometry(&h.view, 110, 14, h.settings.tabs.layout).body;
        assert_eq!(
            frame.backend().buffer()[(body.x as u16 + 8, body.y as u16 + 6)].bg,
            Color::Rgb(0x73, 0x59, 0x16),
            "published hits recolour editor cells below the card"
        );
        h.press(KeyCode::Enter, KeyModifiers::NONE);
        h.settle();
        assert_eq!(h.selection(), 0..3);
        h.press(KeyCode::F(3), KeyModifiers::NONE);
        h.settle();
        let second = original.rfind("one one").unwrap() as u64;
        assert_eq!(h.selection(), second..second + 3);
        h.press(KeyCode::F(3), KeyModifiers::SHIFT);
        h.settle();
        assert_eq!(h.selection(), 0..3);
        h.press(KeyCode::Esc, KeyModifiers::NONE);
        assert!(!h.host.is_open());
        assert_eq!(h.bytes(), original.as_bytes());
        let frame = h.render(110, 14);
        assert_eq!(frame.backend().buffer()[(8, body.y as u16 + 6)].bg, Color::Reset);
        h.finish();
    }

    #[test]
    fn actual_query_options_selection_regex_errors_and_bracketed_text_destination() {
        let mut h = Harness::new("one ONE stone one");
        h.ctrl('f');
        h.query("one");
        assert_eq!(h.host.panel.count(), (4, true));
        h.press(KeyCode::Char('c'), KeyModifiers::ALT);
        h.settle();
        assert_eq!(h.host.panel.count(), (3, true));
        h.press(KeyCode::Char('w'), KeyModifiers::ALT);
        h.settle();
        assert_eq!(h.host.panel.count(), (2, true));
        h.view
            .editor_mut()
            .unwrap()
            .set_cursors(vec![kwil_view_model::Cursor { anchor: 0, head: 3, desired_column: None }], 0)
            .unwrap();
        h.press(KeyCode::Char('s'), KeyModifiers::ALT);
        h.settle();
        assert_eq!(h.host.panel.count(), (1, true));
        h.press(KeyCode::Char('r'), KeyModifiers::ALT);
        h.query("(");
        assert!(!h.host.panel.can_replace());
        assert!(h.host.panel.status().to_ascii_lowercase().contains("error"));
        let document = h.bytes();
        h.host.input(&mut h.view, &mut h.keys, SearchInput::SelectAll);
        h.host.input(&mut h.view, &mut h.keys, SearchInput::Text("o(ne)"));
        h.settle();
        assert_eq!(h.bytes(), document, "paste targets the field, never the document");
        for (width, height) in [(0, 0), (8, 2), (12, 4), (80, 20)] {
            h.render(width, height);
        }
        h.finish();
    }

    fn replacement(h: &mut Harness, query: &str, replacement: &str) {
        h.ctrl('h');
        assert_eq!(h.host.panel.mode(), SearchMode::Replace);
        h.query(query);
        h.press(KeyCode::Tab, KeyModifiers::NONE);
        h.ctrl('a');
        h.type_text(replacement);
        assert!(!h.host.panel.is_pending());
    }

    #[test]
    fn actual_ctrl_h_capture_replace_all_completion_one_undo_redo_and_next_command() {
        let original = b"x1 x2 x3";
        let mut h = Harness::new(std::str::from_utf8(original).unwrap());
        replacement(&mut h, "x([0-9])", "y${1}$$");
        h.press(KeyCode::Char('r'), KeyModifiers::ALT);
        h.settle();
        assert!(h.host.panel.can_replace());
        h.press(KeyCode::Enter, KeyModifiers::CONTROL | KeyModifiers::SHIFT);
        h.until(|h| h.view.document_busy(h.view.active_id()));
        assert!(h.view.editor().is_none(), "actual Workspace worker admission loans the editor");
        assert!(!h.host.panel.can_replace());
        h.until(|h| !h.view.search_replacement_pending());
        assert!(h.view.editor().is_some(), "editor restored before panel completion acknowledgement");
        h.settle();
        h.receipts();
        assert_eq!(h.bytes(), b"y1$ y2$ y3$");
        assert!(h.host.panel.status().contains("Replacement complete"));
        h.press(KeyCode::Esc, KeyModifiers::NONE);
        h.ctrl('z');
        h.receipts();
        assert_eq!(h.bytes(), original, "replace all is exactly one undo step");
        h.ctrl('y');
        h.receipts();
        assert_eq!(h.bytes(), b"y1$ y2$ y3$");
        replacement(&mut h, "y", "z");
        h.press(KeyCode::Char('r'), KeyModifiers::ALT);
        h.settle();
        h.command("search.replaceNext");
        h.settle();
        h.receipts();
        assert_eq!(h.bytes(), b"z1$ y2$ y3$");
        h.finish();
    }

    #[test]
    fn old_navigation_cannot_move_a_new_cursor_or_new_document() {
        let mut h = Harness::new("one two one");
        h.ctrl('f');
        h.query("one");
        h.press(KeyCode::F(3), KeyModifiers::NONE);
        h.view
            .editor_mut()
            .unwrap()
            .set_cursors(vec![kwil_view_model::Cursor { anchor: 4, head: 4, desired_column: None }], 0)
            .unwrap();
        h.settle();
        assert_eq!(h.selection(), 4..4, "cursor-only movement fences the delayed action");
        h.press(KeyCode::F(3), KeyModifiers::NONE);
        h.view.file_command("file.new").unwrap();
        let active = h.view.active_id();
        h.host.sync(&mut h.view, &mut h.keys);
        h.settle();
        assert_eq!(h.view.active_id(), active);
        assert_eq!(h.bytes(), b"");
        assert_eq!(h.selection(), 0..0);
        h.finish();
    }

    #[test]
    fn unrelated_settings_reload_cannot_revalidate_old_navigation_after_cursor_move() {
        let mut h = Harness::new("one two one");
        h.ctrl('f');
        h.query("one");
        h.press(KeyCode::F(3), KeyModifiers::NONE);
        h.view
            .editor_mut()
            .unwrap()
            .set_cursors(vec![kwil_view_model::Cursor { anchor: 4, head: 4, desired_column: None }], 0)
            .unwrap();
        h.settings.editor.tab_size = 8;
        h.host.settings(&mut h.view, &mut h.keys, &h.settings);
        h.settle();
        assert_eq!(h.selection(), 4..4, "an unrelated accepted reload must not restamp an old F3 origin");
        h.finish();
    }

    #[test]
    fn effective_bindings_tombstones_chords_and_vim_modes_survive_search() {
        for preset in [Preset::VsCode, Preset::NotepadPlusPlus, Preset::Vim] {
            let mut h = Harness::new("one ONE");
            h.settings.keymap = preset;
            h.keys = KeymapEngine::new(preset, &[]).unwrap();
            h.ctrl('f');
            h.query("one");
            let mode = h.keys.mode();
            h.keys = KeymapEngine::new(
                preset,
                &[
                    binding("f3", "", None),
                    binding("ctrl+k ctrl+s", "search.toggleCase", Some(Context::Palette)),
                    binding("alt+w", "", Some(Context::Palette)),
                ],
            )
            .unwrap();
            h.host.panel.enter_context(&mut h.keys);
            let key = h.host.session.as_ref().unwrap().key();
            h.press(KeyCode::F(3), KeyModifiers::NONE);
            assert_eq!(h.host.session.as_ref().unwrap().key(), key);
            h.press(KeyCode::Char('w'), KeyModifiers::ALT);
            assert!(!h.host.panel.options().whole_word);
            h.ctrl('k');
            h.ctrl('s');
            h.settle();
            assert!(h.host.panel.options().case_sensitive);
            h.press(KeyCode::Esc, KeyModifiers::NONE);
            assert_eq!(h.keys.mode(), mode);
            h.keys = KeymapEngine::new(preset, &[binding("ctrl+f", "", None)]).unwrap();
            h.ctrl('f');
            assert!(!h.host.is_open());
            assert_eq!(h.bytes(), b"one ONE");
            h.finish();
        }
    }

    #[test]
    fn replacement_completion_does_not_acknowledge_a_new_query_and_shutdown_drains_loans() {
        let mut h = Harness::new("one one");
        replacement(&mut h, "one", "x");
        h.command("search.replaceAll");
        h.until(|h| h.view.search_replacement_pending());
        h.host.input(&mut h.view, &mut h.keys, SearchInput::Focus(SearchField::Query));
        h.host.input(&mut h.view, &mut h.keys, SearchInput::SelectAll);
        h.host.input(&mut h.view, &mut h.keys, SearchInput::Text("new query"));
        h.until(|h| !h.view.search_replacement_pending());
        h.settle();
        assert_eq!(h.bytes(), b"x x");
        assert_eq!(h.host.panel.text(SearchField::Query).0, "new query");
        assert!(!h.host.panel.status().contains("Replacement complete"));
        h.receipts();
        replacement(&mut h, "x", "again");
        h.command("search.replaceAll");
        h.until(|h| h.view.search_replacement_pending());
        h.finish(); // production shutdown drains the admitted editor loan
    }

    #[test]
    fn actual_replacement_history_pin_requires_document_release_before_search_retirement() {
        let mut h = Harness::new("one one");
        replacement(&mut h, "one", "x");
        h.command("search.replaceAll");
        h.settle();
        h.receipts();
        assert_eq!(h.bytes(), b"x x");
        assert!(!h.view.search_replacement_pending());
        let Harness { host, view, rx, stopped, root, .. } = h;
        let worker = host.into_shutdown_worker().expect("actual search worker");
        // Replace-all is spooled even for this small fixture. The real
        // Workspace undo/receipt recipe pins its iterator: zero-time close
        // cannot acknowledge retirement while the document still owns it.
        let pinned =
            worker.shutdown_bounded(Duration::ZERO).expect_err("actual replacement history must pin search retirement");
        let mut ticket = crate::ShutdownTicket {
            view: Some(view),
            search: None,
            worker: Some(pinned.into_worker()),
            rx: Some(rx),
            stopped,
            document_started: true,
            terminal_failure: None,
        };
        let ticket = std::thread::spawn(move || {
            crate::finish_shutdown(&mut ticket, Duration::from_secs(10)).unwrap();
            ticket
        })
        .join()
        .unwrap();
        assert!(ticket.view.is_none());
        assert!(ticket.rx.is_none());
        assert!(ticket.worker.is_none());
        assert!(!ticket.document_started, "successful document Stopped acknowledgement is still required");
        assert!(ticket.terminal_failure.is_none());
        assert_eq!(root.parent(), Some(std::env::temp_dir().as_path()));
        assert!(root.file_name().unwrap().to_string_lossy().starts_with("kwil-tui-search-"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn deferred_find_retains_bounded_query_until_worker_and_file_prompt_wins() {
        let mut view = Workspace::new(View::new(None)).unwrap();
        let mut host = SearchHost::default();
        let settings = Settings::default();
        let mut keys = KeymapEngine::new(settings.keymap, &[]).unwrap();
        let intent = key(
            &mut host,
            &mut view,
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
            &mut keys,
            &settings,
            Duration::ZERO,
        );
        resolve(&mut host, &mut view, &mut keys, &settings, intent);
        assert!(host.is_open());
        host.input(&mut view, &mut keys, SearchInput::Text("needle"));
        assert_eq!(host.panel.text(SearchField::Query).0, "needle");
        let query = host.panel.text(SearchField::Query).0.to_owned();
        assert!(
            host.panel
                .deferred_input(SearchInput::Text(&"x".repeat(kwil_view_model::search_session::INPUT_BYTES)))
                .is_err()
        );
        assert_eq!(host.panel.text(SearchField::Query).0, query, "oversized input is transactional");
        let (tx, rx) = mpsc::channel();
        host.start(&mut view, &mut keys, move || {
            let _ = tx.send(());
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while host.panel.is_pending() {
            rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
            host.ready(&mut view, &mut keys);
        }
        host.close(&mut view, &mut keys);
        view.file_command("file.open").unwrap();
        let VimResult::Command(call) = keys.invoke_named("search.find") else { panic!("registered find") };
        host.command(&mut view, &mut keys, &settings, call);
        assert!(view.file_prompt().is_some());
        assert!(!host.is_open(), "search cannot replace a live native-path prompt");
        std::thread::spawn(move || {
            host.shutdown().unwrap();
            drop(view);
        })
        .join()
        .unwrap();
    }
}
