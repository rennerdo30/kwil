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
}

/// A cursor-only change must invalidate an asynchronous navigation too.
struct Origin {
    document: kwil_view_model::SessionDocumentId,
    epoch: u64,
    revision: u64,
    sequence: u64,
    cursors: Vec<kwil_view_model::Cursor>,
    primary: usize,
}
impl Origin {
    fn capture(view: &Workspace) -> Option<Self> {
        let editor = view.editor()?;
        Some(Self {
            document: view.active_id(),
            epoch: view.input_epoch(),
            revision: editor.document().revision(),
            sequence: editor.next_edit_sequence(),
            cursors: editor.cursors().to_vec(),
            primary: editor.primary_index(),
        })
    }
    fn matches(&self, view: &Workspace) -> bool {
        self.document == view.active_id()
            && self.epoch == view.input_epoch()
            && view.editor().is_some_and(|editor| {
                self.revision == editor.document().revision()
                    && self.sequence == editor.next_edit_sequence()
                    && self.cursors == editor.cursors()
                    && self.primary == editor.primary_index()
            })
    }
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
        if matches!(call.id, "search.findInFiles" | "search.replaceInFiles" | "search.setInclude" | "search.setExclude")
        {
            self.notice(view, "Folder search is not attached to this in-file terminal host".into());
            return true;
        }
        if call.id == "search.substitute" {
            self.notice(view, "Vim substitution arguments are not attached; use Replace".into());
            return true;
        }
        self.origin = Origin::capture(view);
        let result = match &mut self.session {
            Some(session) => self.panel.command(session, view, call.id, &settings.search),
            None => self
                .panel
                .open(
                    if call.id == "search.replace" { SearchMode::Replace } else { SearchMode::Find },
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
        self.origin = Origin::capture(view);
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
            self.origin = Origin::capture(view);
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
            SearchAction::OpenFile { .. } => {
                self.notice(view, "Folder result navigation is not attached".into());
                return;
            }
            SearchAction::Replace(_) => unreachable!(),
        };
        match result {
            Ok(damage) => {
                view.refresh_after_edit(damage);
                let result = self.panel.action_applied(session, view, &action);
                self.origin = Origin::capture(view);
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
            self.origin = Origin::capture(view);
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
    if event.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER) && panel.mode() == SearchMode::Replace {
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
    let height = (panel.visible_fields().len() as u16 + 4).min(area.height.saturating_sub(1));
    (width >= 12 && height >= 3).then_some(Rect { x: area.x + area.width - width, y: area.y, width, height })
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
    host: &SearchHost,
    view: &Workspace,
    layout: kwil_core_commands::settings::TabLayout,
) {
    let bounds = frame.area();
    let geometry = tabs::geometry(view, bounds.width, bounds.height, layout);
    let body = geometry.body;
    // Recolour existing cells: syntax foreground, tabs and wide-cell geometry
    // stay in the normal text renderer. No snapshot reads or search on draw.
    let hit = Color::Rgb(0x73, 0x59, 0x16);
    for row in 0..view.rows().min(body.height) {
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
    if body.width != 0 && body.height != 0 {
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
    let style = Style::default().fg(Color::Rgb(0xd6, 0xe4, 0xf0)).bg(Color::Rgb(0x08, 0x0d, 0x18));
    frame.render_widget(ratatui::widgets::Clear, area);
    frame.render_widget(Paragraph::new("").style(style), area);
    let mut caret = None;
    for (row, field) in host.panel.visible_fields().iter().enumerate() {
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
        if *field == host.panel.field() {
            caret = Some((area.x + 10 + column.min(area.width.saturating_sub(11) as usize) as u16, field_area.y));
        }
    }
    let options = host.panel.options();
    let on = |enabled| if enabled { "on" } else { "off" };
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
    let first = host.panel.visible_fields().len() as u16;
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
}

pub(super) fn mouse(
    host: &mut SearchHost,
    view: &mut Workspace,
    keys: &mut KeymapEngine,
    event: &event::MouseEvent,
    bounds: Rect,
) -> bool {
    let input = match event.kind {
        MouseEventKind::ScrollDown => SearchInput::Next,
        MouseEventKind::ScrollUp => SearchInput::Previous,
        MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
            let Some(area) = card(bounds, &host.panel) else { return true };
            if !area.contains((event.column, event.row).into()) {
                SearchInput::Cancel
            } else if let Some(field) = host.panel.visible_fields().get((event.row - area.y) as usize) {
                SearchInput::Focus(*field)
            } else {
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
            let root = std::env::temp_dir().join(format!(
                "kwil-tui-search-{}-{}",
                std::process::id(),
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
            ));
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
            harness.until(|h| h.view.recovery_ready() && h.view.editor().is_some());
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
            let deadline = Instant::now() + Duration::from_secs(10);
            while !ready(self) {
                let message = self
                    .rx
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .expect("bounded real TUI worker wake");
                self.dispatch(message);
            }
        }
        fn receipts(&mut self) {
            self.until(|h| {
                !h.view.search_replacement_pending()
                    && h.view.recovery_notice().is_none()
                    && h.view.editor().is_some_and(|editor| !editor.has_pending_edit_receipt())
            });
        }
        fn settle(&mut self) {
            self.until(|h| !h.host.panel.is_pending());
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
        fn render(&self, width: u16, height: u16) -> ratatui::Terminal<ratatui::backend::TestBackend> {
            let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    tabs::draw(frame, &self.view, self.settings.tabs.layout);
                    draw(frame, &self.host, &self.view, self.settings.tabs.layout);
                })
                .unwrap();
            terminal
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
