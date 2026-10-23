//! Search card input and drawing. The frontend host owns event-loop dispatch,
//! clipboard access, IME capture and coherent SearchAction admission.
use super::*;
use kwil_core_commands::theme::{Theme, UiRole};
#[cfg(test)]
use kwil_view_model::search_panel::SearchField;
use kwil_view_model::search_panel::{SearchInput, SearchMode, SearchOutcome, SearchPanel};

pub(super) fn blocks_copy(open: bool, displayed: bool) -> bool {
    open || displayed
}
#[cfg(test)]
pub(super) fn area(size: (usize, usize), cw: usize, ch: usize, mode: SearchMode) -> Option<Rect> {
    let (cw, ch) = (cw.max(1), ch.max(1));
    let columns = (size.0 / cw).min(100);
    let rows = (size.1 / ch).min(if mode == SearchMode::Files { 22 } else { 9 });
    if columns < 16 || rows < 5 {
        return None;
    }
    let (w, h) = (columns * cw, rows * ch);
    Some(Rect { x: (size.0 - w) / 2, y: 0, w, h })
}
#[cfg(test)]
fn label(field: SearchField) -> &'static str {
    match field {
        SearchField::Query => "Find",
        SearchField::Replacement => "Replace",
        SearchField::Root => "Folder",
        SearchField::Include => "Include",
        SearchField::Exclude => "Exclude",
    }
}
#[cfg(test)]
pub(super) fn draw(
    panel: &SearchPanel,
    pixels: &mut [u32],
    size: (usize, usize),
    font: &mut Font,
    theme: &Theme,
) -> Option<Rect> {
    if !panel.is_open() {
        return None;
    }
    let rect = area(size, font.cw, font.ch, panel.mode())?;
    let (cw, ch) = (font.cw, font.ch);
    let columns = rect.w / cw;
    let rows = rect.h / ch;
    let color = |role| theme_ui::rgb(theme, role);
    let (bg, fg, muted, accent, selection) = (
        color(UiRole::PanelBackground),
        color(UiRole::PanelForeground),
        color(UiRole::GutterForeground),
        color(UiRole::Accent),
        color(UiRole::Selection),
    );
    fill(pixels, size.0, rect, bg);
    let title = match panel.mode() {
        SearchMode::Find => "Find",
        SearchMode::Replace => "Find and replace",
        SearchMode::Files => "Find in files",
    };
    draw_text(pixels, size.0, font, rect.x + cw, rect.y, title.as_bytes(), columns - 2, accent, bg);
    let fields = panel.visible_fields();
    let visible_fields = fields.len().min(rows.saturating_sub(4));
    let field_first = fields
        .iter()
        .position(|field| *field == panel.field())
        .unwrap_or(0)
        .saturating_sub(visible_fields.saturating_sub(1));
    for (index, &field) in fields.iter().skip(field_first).take(visible_fields).enumerate() {
        let y = rect.y + (index + 1) * ch;
        let active = panel.field() == field;
        let row_bg = if active { selection } else { bg };
        fill(pixels, size.0, Rect { x: rect.x, y, w: rect.w, h: ch }, row_bg);
        draw_text(
            pixels,
            size.0,
            font,
            rect.x + cw,
            y,
            label(field).as_bytes(),
            9,
            if active { accent } else { muted },
            row_bg,
        );
        let width = columns.saturating_sub(12);
        let (text, cursor) = panel.text(field);
        let window = kwil_view_model::palette_session::text_window(text, cursor, width);
        draw_text(pixels, size.0, font, rect.x + 11 * cw, y, window.text.as_bytes(), width, fg, row_bg);
        if active {
            fill(
                pixels,
                size.0,
                Rect { x: rect.x + (11 + window.cursor) * cw, y: y + ch.saturating_sub(2), w: cw, h: 1 },
                accent,
            );
        }
    }
    let flags_row = visible_fields + 1;
    let options = panel.options();
    let flags = format!(
        "Alt+R Regex:{}  Alt+C Case:{}  Alt+W Word:{}  Alt+S Selection:{}",
        on(options.regex),
        on(options.case_sensitive),
        on(options.whole_word),
        on(options.in_selection)
    );
    draw_text(pixels, size.0, font, rect.x + cw, rect.y + flags_row * ch, flags.as_bytes(), columns - 2, muted, bg);
    let first_row = flags_row + 1;
    let visible = rows.saturating_sub(first_row + 2);
    let first = panel.selected().saturating_sub(visible.saturating_sub(1));
    for (index, row) in panel.results().iter().enumerate().skip(first).take(visible) {
        let y = rect.y + (first_row + index - first) * ch;
        let row_bg = if index == panel.selected() { selection } else { bg };
        fill(pixels, size.0, Rect { x: rect.x, y, w: rect.w, h: ch }, row_bg);
        draw_text(
            pixels,
            size.0,
            font,
            rect.x + cw,
            y,
            row.label.as_bytes(),
            columns - 2,
            if row.is_hit { fg } else { color(UiRole::Error) },
            row_bg,
        );
    }
    draw_text(
        pixels,
        size.0,
        font,
        rect.x + cw,
        rect.y + (rows - 2) * ch,
        panel.status().as_bytes(),
        columns - 2,
        accent,
        bg,
    );
    let help = if panel.mode() == SearchMode::Replace {
        "Enter Next  Shift+Enter Previous  Ctrl+Enter Replace  Ctrl+Shift+Enter All  Esc Close"
    } else if panel.mode() == SearchMode::Files {
        "Enter Open  PgUp/PgDn Results  Tab Field  Esc Close"
    } else {
        "Enter Next  Shift+Enter Previous  Ctrl+D Add cursor  Tab Field  Esc Close"
    };
    draw_text(pixels, size.0, font, rect.x + cw, rect.y + (rows - 1) * ch, help.as_bytes(), columns - 2, muted, bg);
    Some(rect)
}
#[cfg(test)]
fn on(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}
fn input_for_key<'a>(
    panel: &SearchPanel,
    event: &'a HostKey<'a>,
    mods: ModifiersState,
) -> Result<Option<SearchInput<'a>>, String> {
    let ctrl = mods.control_key() || mods.super_key();
    let input = match &event.logical_key {
        Key::Named(NamedKey::Escape) => SearchInput::Cancel,
        Key::Named(NamedKey::Tab) => {
            if mods.shift_key() {
                SearchInput::PreviousField
            } else {
                SearchInput::NextField
            }
        }
        Key::Named(NamedKey::Enter) if ctrl && panel.mode() == SearchMode::Replace => return Err(REPLACE_GATE.into()),
        Key::Named(NamedKey::Enter | NamedKey::F3) => {
            if mods.shift_key() {
                SearchInput::Previous
            } else {
                SearchInput::Next
            }
        }
        Key::Named(NamedKey::ArrowUp) => SearchInput::Previous,
        Key::Named(NamedKey::ArrowDown) => SearchInput::Next,
        Key::Named(NamedKey::Backspace) => SearchInput::Backspace,
        Key::Named(NamedKey::Delete) => SearchInput::Delete,
        Key::Named(NamedKey::ArrowLeft) => SearchInput::Left,
        Key::Named(NamedKey::ArrowRight) => SearchInput::Right,
        Key::Named(NamedKey::Home) => SearchInput::Home,
        Key::Named(NamedKey::End) => SearchInput::End,
        Key::Character(c) if mods.alt_key() && !ctrl => match c.to_lowercase().as_str() {
            "r" => SearchInput::ToggleRegex,
            "c" => SearchInput::ToggleCase,
            "w" => SearchInput::ToggleWord,
            "s" => SearchInput::ToggleSelection,
            _ => return Ok(None),
        },
        Key::Character(c) if ctrl && c.eq_ignore_ascii_case("d") => SearchInput::AddCursor,
        Key::Character(c) if ctrl && c.eq_ignore_ascii_case("a") => SearchInput::SelectAll,
        _ if !ctrl || (mods.control_key() && mods.alt_key()) => {
            return Ok(event.text.filter(|text| !text.chars().any(char::is_control)).map(SearchInput::Text));
        }
        _ => return Ok(None),
    };
    Ok(Some(input))
}
pub(super) fn draw_signal(panel: &SearchPanel, pixels: &mut [u32], stride: usize, area: Rect, theme: &Theme) {
    fill(pixels, stride, area, theme_ui::rgb(theme, UiRole::GutterBackground));
    if area.h == 0 {
        return;
    }
    let color = theme_ui::rgb(theme, UiRole::SearchMatch);
    for (index, count) in panel.signal().iter().enumerate() {
        if *count != 0 {
            let y = area.y + index * area.h / panel.signal().len();
            fill(pixels, stride, Rect { x: area.x, y, w: area.w, h: 1 }, color);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn closed_search_pixels_block_copy_until_overlay_restored() {
        assert!(blocks_copy(false, true));
        assert!(blocks_copy(true, false));
        assert!(!blocks_copy(false, false));
    }
    #[test]
    fn search_card_is_bounded_at_actual_dpi_and_small_sizes() {
        for (size, cell) in [((800, 600), (8, 16)), ((3840, 2160), (24, 48)), ((128, 80), (8, 16))] {
            let a = area(size, cell.0, cell.1, SearchMode::Files).unwrap();
            assert!(a.x + a.w <= size.0 && a.y + a.h <= size.1);
        }
        assert!(area((50, 50), 8, 16, SearchMode::Find).is_none());
    }
    #[test]
    #[cfg(windows)]
    fn cpu_search_card_and_signal_use_current_theme_instead_of_dark_constants() {
        let mut panel = SearchPanel::default();
        panel.open(SearchMode::Find, &Default::default()).unwrap();
        let mut font = Font::load(14.0).expect("Windows monospace font");
        let size = (80 * font.cw, 10 * font.ch);
        let mut pixels = vec![0; size.0 * size.1];
        let theme = Theme::builtin(kwil_core_commands::theme::Scheme::Light);
        let rect = draw(&panel, &mut pixels, size, &mut font, &theme).unwrap();
        assert_eq!(pixels[rect.y * size.0 + rect.x], theme_ui::rgb(&theme, UiRole::PanelBackground));
        let strip = Rect { x: 0, y: 0, w: 2, h: size.1 };
        draw_signal(&panel, &mut pixels, size.0, strip, &theme);
        assert_eq!(pixels[0], theme_ui::rgb(&theme, UiRole::GutterBackground));
    }
}

/// Metadata fence retained across an asynchronous navigation. No source bytes.
pub(super) struct Origin {
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
                editor.document().revision() == self.revision
                    && editor.next_edit_sequence() == self.sequence
                    && editor.cursors() == self.cursors
                    && editor.primary_index() == self.primary
            })
    }
}
pub(super) struct Composition {
    destination: kwil_view_model::search_panel::SearchDestination,
    origin: Origin,
}

const REPLACE_GATE: &str = "Replace is unavailable: W2 must attach the journal-admitted transaction executor";

impl App {
    pub(super) fn start_search(&mut self) {
        if self.search.is_some() {
            return;
        }
        let proxy = self.proxy.clone();
        match kwil_view_model::search_session::SearchSession::new(move || {
            let _ = proxy.send_event(UserEvent::SearchReady);
        }) {
            Ok(session) => {
                self.search = Some(session);
                let outcome = self.search_panel.sync_document(self.search.as_mut().unwrap(), &self.view);
                self.search_origin = Origin::capture(&self.view);
                match outcome {
                    Ok(outcome) => self.search_outcome(outcome),
                    Err(error) => self.search_notice(error),
                }
            }
            Err(error) => self.search_notice(format!("Find unavailable: {error}; input is retained")),
        }
    }
    pub(super) fn search_notice(&mut self, error: String) {
        self.search_panel.set_error(error.clone());
        self.search_dirty = true;
        let damage = self.view.set_notice(error);
        self.apply(damage);
    }
    pub(super) fn close_search(&mut self) {
        if let Some(session) = &mut self.search {
            if !self.search_panel.is_open() {
                return;
            }
            let outcome = self.search_panel.close(session, &self.view);
            self.search_outcome(outcome);
        } else if self.search_panel.is_open()
            && let Ok(outcome) = self.search_panel.deferred_input(SearchInput::Cancel)
        {
            self.search_outcome(outcome);
        }
        self.search_ime = None;
    }
    pub(super) fn search_settings_ready(&mut self) {
        let result = match &mut self.search {
            Some(session) => self.search_panel.settings_ready(session, &self.view, &self.settings.search),
            None => self.search_panel.sync_settings(&self.settings.search).map(|_| SearchOutcome::default()),
        };
        match result {
            Ok(outcome) => self.search_outcome(outcome),
            Err(error) => self.search_notice(error),
        }
    }
    pub(super) fn dispatch_search(&mut self, command: &str) -> bool {
        if !command.starts_with("search.") && command != "edit.selectNextMatch" {
            return false;
        }
        if self.view.file_prompt().is_some() || self.settings_open() {
            self.search_notice("Finish or cancel the current modal prompt before Find".into());
            return true;
        }
        if matches!(command, "search.findInFiles" | "search.replaceInFiles" | "search.setInclude" | "search.setExclude")
        {
            self.search_notice("Folder search is not attached to this in-file GUI host".into());
            return true;
        }
        if matches!(command, "search.replaceNext" | "search.replaceAll" | "search.substitute") {
            self.search_notice(REPLACE_GATE.into());
            return true;
        }
        if self.goto.take().is_some() {
            self.goto_epoch += 1;
            self.palette_redraw();
        }
        if let Some(palette) = &mut self.palette {
            let _ = palette.input(kwil_view_model::palette_session::PaletteInput::Cancel);
        }
        self.menu_set(menu::State::Closed, false);
        self.search_origin = Origin::capture(&self.view);
        if self.search.is_none() {
            let mode = if command == "search.replace" { SearchMode::Replace } else { SearchMode::Find };
            match self.search_panel.open(mode, &self.settings.search) {
                Ok(outcome) => {
                    if let Some(keys) = &mut self.keys {
                        self.search_panel.enter_context(keys);
                    }
                    self.search_outcome(outcome);
                    self.search_notice("Find is starting or unavailable; query input is retained".into());
                }
                Err(error) => self.search_notice(error),
            }
            return true;
        }
        let result =
            self.search_panel.command(self.search.as_mut().unwrap(), &self.view, command, &self.settings.search);
        match result {
            Ok(Some(outcome)) => {
                self.drag = None;
                self.selecting = false;
                self.composing = false;
                self.search_ime = None;
                self.ime_destination = Some(ime::Destination::Cancelled);
                if let Some(keys) = &mut self.keys {
                    self.search_panel.enter_context(keys);
                }
                self.search_outcome(outcome);
                if command == "search.replace" {
                    self.search_notice(REPLACE_GATE.into());
                }
            }
            Ok(None) => self.search_notice(format!("Search command is not attached: {command}")),
            Err(error) => {
                if self.search_panel.is_open()
                    && let Some(keys) = &mut self.keys
                {
                    self.search_panel.enter_context(keys);
                }
                self.search_notice(error);
            }
        }
        true
    }
    pub(super) fn dispatch_search_call(&mut self, call: &kwil_core_commands::vim::Invocation) -> bool {
        if !self.dispatch_search(call.id) {
            return false;
        }
        // Vim /query is already parsed by the existing grammar. Preserve that
        // argument instead of silently opening an empty Find card.
        if call.id == "search.find"
            && let kwil_core_commands::vim::Argument::Search(query) = &call.argument
        {
            self.search_input(SearchInput::SelectAll);
            self.search_input(SearchInput::Text(query));
            self.search_input(SearchInput::Next);
        }
        true
    }
    pub(super) fn search_input(&mut self, input: SearchInput<'_>) {
        if matches!(input, SearchInput::ReplaceAll | SearchInput::ReplaceNext) {
            self.search_notice(REPLACE_GATE.into());
            return;
        }
        self.search_origin = Origin::capture(&self.view);
        let result = match &mut self.search {
            Some(session) => self.search_panel.input(session, &self.view, input),
            None => self.search_panel.deferred_input(input),
        };
        match result {
            Ok(outcome) => self.search_outcome(outcome),
            Err(error) => self.search_notice(error),
        }
    }
    pub(super) fn search_outcome(&mut self, mut outcome: SearchOutcome) {
        if outcome.damage.status {
            self.view.set_notice(if outcome.damage.restore_editor {
                String::new()
            } else {
                self.search_panel.status().to_owned()
            });
        }
        if let Some(keys) = &mut self.keys {
            outcome.restore_context(keys);
        }
        self.search_dirty |= outcome.damage.panel || outcome.damage.signal;
        // The normal Damage merger clips and deduplicates these visible rows.
        let damage = Damage { rows: outcome.damage.hit_rows, status: outcome.damage.status, ..Default::default() };
        if let Some(action) = outcome.action.take() {
            self.search_action(action);
        }
        self.apply(damage);
    }
    pub(super) fn search_ready(&mut self) {
        let Some(session) = &mut self.search else { return };
        // Bounded draining and retry-on-wake are owned by SearchSession/Panel.
        let outcome = self.search_panel.worker_ready(session, &self.view);
        self.search_outcome(outcome);
    }
    fn search_action(&mut self, action: kwil_view_model::search_session::SearchAction) {
        use kwil_view_model::search_session::{SearchAction, SelectionMode};
        if self.search.as_ref().is_none_or(|session| !self.search_panel.validate_action(session, &self.view, &action))
            || self.search_origin.as_ref().is_none_or(|origin| !origin.matches(&self.view))
        {
            self.search_notice("Search navigation cancelled: its document, query or cursor changed".into());
            return;
        }
        let result = match &action {
            SearchAction::Select { range, mode, .. } => {
                let editor = self.view.editor_mut().unwrap();
                let mut cursors =
                    if *mode == SelectionMode::AddCursor { editor.cursors().to_vec() } else { Vec::new() };
                cursors.push(kwil_view_model::Cursor { anchor: range.start, head: range.end, desired_column: None });
                let primary = cursors.len() - 1;
                editor.set_cursors(cursors, primary)
            }
            SearchAction::SeedSelections { cursors, primary, from_selection, .. } => {
                if *from_selection {
                    Ok(Damage::default())
                } else {
                    self.view.editor_mut().unwrap().set_cursors(
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
            // Neither branch is requested by this host. Never iterate a
            // replacement recipe or open an unadmitted file on the UI thread.
            SearchAction::OpenFile { .. } => {
                self.search_notice("Folder result navigation is not attached".into());
                return;
            }
            SearchAction::Replace(_) => {
                self.search_notice(REPLACE_GATE.into());
                return;
            }
        };
        match result {
            Ok(damage) => {
                let damage = self.view.refresh_after_edit(damage);
                let acknowledged = self.search_panel.action_applied(self.search.as_mut().unwrap(), &self.view, &action);
                self.search_origin = Origin::capture(&self.view);
                self.apply(damage);
                self.keep_caret_visible();
                match acknowledged {
                    Ok(outcome) => self.search_outcome(outcome),
                    Err(error) => self.search_notice(error),
                }
            }
            Err(error) => self.search_notice(error.to_string()),
        }
    }
    pub(super) fn search_key(&mut self, event: &HostKey<'_>) -> bool {
        if !self.search_panel.is_open() {
            return false;
        }
        if self.keys.is_none() {
            match kwil_core_commands::keymap::KeymapEngine::new(self.settings.keymap, &self.settings.bindings) {
                Ok(keys) => self.keys = Some(keys),
                Err(error) => {
                    self.search_notice(error.to_string());
                    return true;
                }
            }
        }
        use kwil_core_commands::vim::VimResult;
        let mapped = input::chord(&event.logical_key, self.mods).map(|chord| {
            let keys = self.keys.as_mut().unwrap();
            keys.set_context(kwil_core_commands::Context::Palette);
            let result = keys.feed(chord);
            (result, keys.suppresses_fallback())
        });
        match mapped {
            Some((VimResult::Pending, _)) => return true,
            Some((VimResult::Unbound, true)) => return true,
            Some((VimResult::Error(error), _)) => {
                self.search_notice(error);
                return true;
            }
            Some((VimResult::Command(call), _)) => {
                let input = match call.id {
                    "palette.cancel" => Some(SearchInput::Cancel),
                    "palette.accept" => {
                        Some(if self.mods.shift_key() { SearchInput::Previous } else { SearchInput::Next })
                    }
                    "palette.next" => Some(SearchInput::Next),
                    "palette.previous" => Some(SearchInput::Previous),
                    "edit.backspace" => Some(SearchInput::Backspace),
                    "edit.delete" => Some(SearchInput::Delete),
                    "motion.left" => Some(SearchInput::Left),
                    "motion.right" => Some(SearchInput::Right),
                    "motion.lineStart" => Some(SearchInput::Home),
                    "motion.lineEnd" => Some(SearchInput::End),
                    _ => None,
                };
                if let Some(input) = input {
                    self.search_input(input);
                    return true;
                }
                if call.id == "edit.paste" {
                    let hwnd = self.gfx.as_ref().map_or(0, |gfx| gfx.hwnd);
                    match clipboard::SystemClipboard(hwnd).get_text(kwil_view_model::search_session::INPUT_BYTES) {
                        Ok(Some(text)) => self.search_input(SearchInput::Text(&text)),
                        Ok(None) => {}
                        Err(error) => self.search_notice(error.to_string()),
                    }
                    return true;
                }
                if self.dispatch_search_call(&call) {
                    return true;
                }
                // A custom command retains its normal frontend/Vim adapter.
                self.close_search();
                self.palette_action(kwil_core_commands::palette::PaletteAction::Command(call.id));
                return true;
            }
            _ => {}
        }
        match input_for_key(&self.search_panel, event, self.mods) {
            Ok(Some(input)) => self.search_input(input),
            Ok(None) => {}
            Err(error) => self.search_notice(error),
        }
        true
    }
    pub(super) fn capture_search_composition(&self) -> Option<Composition> {
        if self.save_path.is_some() || self.settings_open() || self.menu.state.is_open() {
            return None;
        }
        Some(Composition { destination: self.search_panel.destination()?, origin: Origin::capture(&self.view)? })
    }
    pub(super) fn search_commit(&mut self, destination: Composition, text: &str) {
        if !destination.origin.matches(&self.view)
            || self.view.file_prompt().is_some()
            || self.settings_open()
            || self.menu.state.is_open()
        {
            self.search_notice("Search composition cancelled: its target changed".into());
            return;
        }
        self.search_origin = Origin::capture(&self.view);
        let result = if self.search_panel.destination() != Some(destination.destination) {
            Err("Search composition cancelled: its field or input changed".into())
        } else {
            match &mut self.search {
                Some(session) => self.search_panel.commit(session, &self.view, destination.destination, text),
                None => self.search_panel.deferred_input(SearchInput::Text(text)),
            }
        };
        match result {
            Ok(outcome) => self.search_outcome(outcome),
            Err(error) => self.search_notice(error),
        }
    }
    pub(super) fn search_mouse(&mut self) {
        let Some(rect) = self.search_area else { return };
        if host_area(&self.search_panel, self.size, &self.ui, self.body_area().y) != Some(rect) {
            self.search_dirty = true;
            self.request_frame();
            return;
        }
        match search_view::click(&self.search_view, &self.search_panel, Some(rect), self.pointer) {
            search_view::Click::Input(input) => self.search_input(input),
            search_view::Click::Clear => {
                for input in search_view::CLEAR_QUERY {
                    self.search_input(input);
                }
            }
            search_view::Click::ReplaceGate => self.search_notice("Replace awaits the W2 transaction executor".into()),
            search_view::Click::Outside => self.search_input(SearchInput::Cancel),
            search_view::Click::Inside => {}
        }
    }
}

fn host_area(panel: &SearchPanel, size: (usize, usize), metrics: &chrome::Metrics, body_top: usize) -> Option<Rect> {
    search_view::area(panel, size, metrics, body_top)
}

/// Cached visible bytes only. Match rectangles enter the same row cache as
/// syntax/selection so scroll-copy never drags stale hit overlays through it.
pub(super) fn draw_marks(
    panel: &SearchPanel,
    pixels: &mut [u32],
    stride: usize,
    font: &Font,
    view: &Workspace,
    row: usize,
) {
    let color = theme_ui::rgb(theme_ui::chrome(), UiRole::SearchMatch);
    let gutter = GUTTER_COLS * font.cw;
    let max = stride.saturating_sub(gutter + BAR) / font.cw.max(1);
    for range in panel.highlight_columns(view, row, max) {
        let a = (range.start as usize).min(max);
        let b = (range.end as usize).min(max);
        for y in row * font.ch..((row + 1) * font.ch).min(pixels.len() / stride.max(1)) {
            for x in gutter + a * font.cw..gutter + b * font.cw {
                let pixel = &mut pixels[y * stride + x];
                *pixel = font::blend(*pixel, color, 100);
            }
        }
    }
}

#[cfg(test)]
mod host_dispatch_tests {
    use super::*;
    use kwil_core_commands::{Binding, Context as KeyContext, Preset, keymap::KeymapEngine};
    use kwil_view_model::search_session::SearchStatus;
    use std::sync::mpsc::{self, Receiver};
    use std::time::{Duration, Instant};

    // No native event loop/window, font loading, catalogue, persistent settings
    // or storage startup. The production App and worker wake dispatch are used.
    fn host(text: &str) -> (App, Receiver<UserEvent>) {
        let (tx, rx) = mpsc::channel();
        let (_, inbox) = kwil_view_model::document_worker::external_open_channel(|| Ok(()));
        let mut view = Workspace::new(View::new(None)).unwrap();
        view.set_rows(3);
        view.execute("edit.insert", &CommandArgs { text: Some(text.into()), ..Default::default() }, 1).unwrap();
        let frontend_document = view.active_id();
        let frontend_epoch = view.input_epoch();
        (
            App {
                search: None,
                search_panel: Default::default(),
                search_origin: None,
                search_ime: None,
                search_dirty: false,
                search_area: None,
                search_view: Default::default(),
                goto: None,
                goto_epoch: 0,
                goto_reveal: None,
                highlighting: None,
                palette: None,
                palette_dirty: false,
                palette_area: None,
                desktop_window: None,
                desktop_dispatch: false,
                pending_resize: None,
                file_dialogs: Default::default(),
                close_dialog: None,
                settings_view: Default::default(),
                palette_view: Default::default(),
                prompt_view: Default::default(),
                welcome: Default::default(),
                welcome_view: Default::default(),
                welcome_worker: None,
                file_types: None,
                registration_path: None,
                registration_starting: false,
                registration_prep: None,
                settings_panel: None,
                settings_epoch: 0,
                settings_file_pending: false,
                view,
                frontend_document,
                frontend_epoch,
                opts: GuiOptions::default(),
                proxy: HostProxy::Test(tx),
                on_first_frame: None,
                on_external_ready: None,
                external_open: None,
                external_inbox: inbox,
                font_thread: None,
                font_size: None,
                font: None,
                gfx: None,
                size: (800, 600),
                mods: ModifiersState::empty(),
                pending: Default::default(),
                error: None,
                horizontal: 0,
                pointer: (0, 0),
                drag: None,
                started: Instant::now(),
                composing: false,
                ime_destination: None,
                selecting: false,
                close_pending: false,
                save_path: None,
                keys: None,
                settings: Settings::default(),
                zoom: Default::default(),
                policy: RenderPolicy::new(Default::default(), false),
                on_event: Box::new(|_| {}),
                #[cfg(feature = "gpu")]
                gpu: None,
                #[cfg(feature = "gpu")]
                gpu_started: false,
                font_loading: false,
                font_reload_pending: false,
                redraw_requested: false,
                settings_loaded: false,
                body_pixels: Vec::new(),
                chrome_frame: None,
                chrome_dirty: false,
                ui: Default::default(),
                ui_font: None,
                ui_font_thread: None,
                ui_font_size: None,
                ui_font_jobs: Default::default(),
                ui_fallback: Default::default(),
                menu: Default::default(),
                menu_area: None,
                menu_dirty: false,
                hover: chrome::Hover::None,
                status_hits: Vec::new(),
                window_title: String::new(),
                #[cfg(feature = "frame-harness")]
                metrics: Default::default(),
            },
            rx,
        )
    }
    fn press(app: &mut App, key: Key, text: Option<&str>, mods: ModifiersState) {
        app.mods = mods;
        app.key_after_settings(None, &HostKey { logical_key: key, text, repeat: false });
    }
    fn ctrl(app: &mut App, c: &str) {
        press(app, Key::Character(c.into()), None, ModifiersState::CONTROL);
    }
    fn named(app: &mut App, key: NamedKey, shift: bool) {
        press(app, Key::Named(key), None, if shift { ModifiersState::SHIFT } else { ModifiersState::empty() });
    }
    fn text(app: &mut App, value: &str) {
        for c in value.chars() {
            let text = c.to_string();
            press(app, Key::Character(text.clone().into()), Some(&text), ModifiersState::empty());
        }
    }
    fn finish(app: &mut App, rx: &Receiver<UserEvent>) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.search_panel.is_pending() {
            let event = rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("the real worker must wake the actual GUI host within five seconds");
            match event {
                UserEvent::SearchReady => app.search_ready(),
                _ => panic!("unexpected headless event"),
            }
        }
    }
    fn stop(mut app: App) {
        if let Some(mut session) = app.search.take() {
            let worker = app.search_panel.take_shutdown(&mut session).unwrap_or_else(|_| panic!("shutdown ownership"));
            drop(app);
            drop(session);
            worker.shutdown().unwrap();
        }
    }
    fn query(app: &mut App, value: &str) {
        ctrl(app, "a");
        text(app, value);
    }
    const DOCUMENT: &str =
        "needle beginning\nline two\nline three\nline four\nline five\nline six\nline seven\nneedle ending\nlast line";

    #[test]
    fn actual_host_goto_reveals_long_line_and_stale_scaled_hit_geometry_is_inert() {
        let (mut app, _) = host(&"x".repeat(512));
        ctrl(&mut app, "g");
        text(&mut app, "1:400");
        named(&mut app, NamedKey::Enter, false);
        assert_eq!(app.view.editor().unwrap().primary_cursor().head, 399);
        assert_eq!(app.horizontal, 399, "the same host reveal path must move the cached horizontal window");
        assert_eq!(app.view.line_offset(0), 399);
        ctrl(&mut app, "f");
        for scale in [1.0, 1.25, 1.5, 2.0] {
            app.ui = chrome::Metrics::new(scale);
            let rect = host_area(&app.search_panel, app.size, &app.ui, app.body_area().y).unwrap();
            assert!(rect.y >= app.body_area().y && rect.x + rect.w <= app.size.0 && rect.y + rect.h <= app.size.1);
        }
        app.search_area = host_area(&app.search_panel, app.size, &app.ui, app.body_area().y);
        app.pointer = (app.search_area.unwrap().x + 2, app.search_area.unwrap().y + 2);
        app.size = (300, 200);
        app.search_mouse();
        assert!(app.search_panel.is_open(), "a click against an old scaled card cannot dismiss or activate it");
        assert!(app.search_dirty);
    }

    #[test]
    fn actual_host_ctrl_f_typing_worker_enter_f3_previous_and_reveal() {
        let (mut app, rx) = host(DOCUMENT);
        app.start_search();
        let revision = app.view.editor().unwrap().document().revision();
        ctrl(&mut app, "f");
        assert!(app.search_panel.is_open(), "fails if the real keymap call still falls through to Editor");
        text(&mut app, "needle");
        finish(&mut app, &rx);
        assert_eq!(app.search_panel.count(), (2, true));
        assert_eq!(
            app.view.editor().unwrap().document().revision(),
            revision,
            "query typing must never edit the buffer"
        );
        named(&mut app, NamedKey::Enter, false);
        finish(&mut app, &rx);
        assert_eq!(app.view.editor().unwrap().primary_cursor().selection(), 0..6);
        assert_eq!(app.view.top(), 0);
        assert!(
            !app.search_panel.highlight_columns(&app.view, 0, 80).is_empty(),
            "cached hit marks reach the visible row"
        );
        named(&mut app, NamedKey::F3, false);
        finish(&mut app, &rx);
        assert_eq!(
            app.view.editor().unwrap().byte_position(app.view.editor().unwrap().primary_cursor().anchor).unwrap().0,
            7
        );
        assert!(app.view.top() > 0, "the selected match must be revealed beyond the old viewport");
        named(&mut app, NamedKey::F3, true);
        finish(&mut app, &rx);
        assert_eq!(
            app.view.editor().unwrap().primary_cursor().selection(),
            0..6,
            "previous starts before selection, not its end"
        );
        named(&mut app, NamedKey::Enter, true);
        finish(&mut app, &rx);
        assert!(app.view.editor().unwrap().primary_cursor().anchor > 6, "previous wraps to final hit");
        named(&mut app, NamedKey::Escape, false);
        assert!(!app.search_panel.is_open());
        assert!(app.search_panel.visible_hits(app.view.active_id(), revision).is_empty());
        stop(app);
    }
    #[test]
    fn early_find_retains_input_and_early_ctrl_g_does_not_wait_for_palette_root() {
        let (mut app, rx) = host(DOCUMENT);
        assert!(app.palette.is_none());
        ctrl(&mut app, "f");
        text(&mut app, "needle");
        assert!(app.search_panel.is_open());
        assert_eq!(app.search_panel.text(SearchField::Query).0, "needle");
        assert!(!app.menu_available().search, "menu enables only when host is ready");
        app.start_search();
        finish(&mut app, &rx);
        assert_eq!(app.search_panel.count(), (2, true));
        assert!(app.menu_available().search);
        ctrl(&mut app, "g");
        assert!(app.palette_open() && app.goto.is_some());
        assert!(!app.search_panel.is_open());
        assert!(app.palette.is_none(), "no catalogue worker is constructed by Ctrl+G");
        text(&mut app, "8:4");
        named(&mut app, NamedKey::Enter, false);
        assert_eq!(app.view.status_position(), Some((7, 3)));
        assert!(app.view.top() <= 7 && app.view.top() + app.view.rows() as u64 > 7);
        assert!(!app.palette_open());
        ctrl(&mut app, "g");
        text(&mut app, "2:2");
        named(&mut app, NamedKey::Escape, false);
        assert_eq!(app.view.status_position(), Some((7, 3)), "Escape must not execute the pending line intent");
        stop(app);
    }
    #[test]
    fn real_host_case_word_regex_selection_controls_and_errors() {
        let (mut app, rx) = host("Needle needle needles");
        app.start_search();
        ctrl(&mut app, "f");
        text(&mut app, "needle");
        finish(&mut app, &rx);
        assert_eq!(app.search_panel.count(), (3, true));
        press(&mut app, Key::Character("c".into()), None, ModifiersState::ALT);
        finish(&mut app, &rx);
        assert_eq!(app.search_panel.count(), (2, true));
        press(&mut app, Key::Character("w".into()), None, ModifiersState::ALT);
        finish(&mut app, &rx);
        assert_eq!(app.search_panel.count(), (1, true));
        press(&mut app, Key::Character("r".into()), None, ModifiersState::ALT);
        query(&mut app, "[");
        finish(&mut app, &rx);
        assert_eq!(app.search.as_ref().unwrap().state().status, SearchStatus::Failed);
        assert!(!app.search_panel.status().is_empty());
        query(&mut app, "needle");
        finish(&mut app, &rx);
        app.view
            .editor_mut()
            .unwrap()
            .set_cursors(vec![kwil_view_model::Cursor { anchor: 7, head: 13, desired_column: None }], 0)
            .unwrap();
        press(&mut app, Key::Character("s".into()), None, ModifiersState::ALT);
        finish(&mut app, &rx);
        assert_eq!(app.search_panel.count(), (1, false));
        stop(app);
    }
    #[test]
    fn host_respects_tombstones_custom_chords_cmd_and_vim_modes() {
        for preset in [Preset::VsCode, Preset::NotepadPlusPlus, Preset::Vim] {
            let (mut app, rx) = host("needle");
            app.settings.keymap = preset;
            app.start_search();
            press(&mut app, Key::Character("f".into()), None, ModifiersState::SUPER);
            assert!(app.search_panel.is_open());
            text(&mut app, "needle");
            finish(&mut app, &rx);
            let mode = app.keys.as_ref().unwrap().mode();
            let bindings = [
                Binding { keys: "f3".into(), command: String::new(), context: None, mode: None },
                Binding {
                    keys: "ctrl+k ctrl+s".into(),
                    command: "search.toggleCase".into(),
                    context: Some(KeyContext::Palette),
                    mode: None,
                },
            ];
            app.keys = Some(KeymapEngine::new(preset, &bindings).unwrap());
            let key = app.search.as_ref().unwrap().key();
            named(&mut app, NamedKey::F3, false);
            assert_eq!(app.search.as_ref().unwrap().key(), key, "F3 tombstone suppresses raw search fallback");
            ctrl(&mut app, "k");
            ctrl(&mut app, "s");
            finish(&mut app, &rx);
            assert!(
                app.search_panel.options().case_sensitive,
                "opening the search context must not clear each chord prefix"
            );
            named(&mut app, NamedKey::Escape, false);
            assert_eq!(app.keys.as_ref().unwrap().mode(), mode);
            app.keys = Some(
                KeymapEngine::new(
                    preset,
                    &[Binding { keys: "ctrl+f".into(), command: String::new(), context: None, mode: None }],
                )
                .unwrap(),
            );
            ctrl(&mut app, "f");
            assert!(!app.search_panel.is_open());
            press(&mut app, Key::Character("g".into()), None, ModifiersState::SUPER);
            assert!(app.goto.is_some(), "Cmd+G is an explicit binding in each preset");
            stop(app);
        }
    }
    #[test]
    fn actual_host_rejects_delayed_navigation_after_cursor_or_document_change() {
        let (mut app, rx) = host(DOCUMENT);
        app.start_search();
        ctrl(&mut app, "f");
        text(&mut app, "needle");
        finish(&mut app, &rx);
        named(&mut app, NamedKey::Enter, false);
        app.view.editor_mut().unwrap().set_cursors(vec![kwil_view_model::Cursor::caret(10)], 0).unwrap();
        finish(&mut app, &rx);
        assert_eq!(app.view.editor().unwrap().primary_cursor().head, 10);
        assert!(app.view.notice().contains("cancelled"));
        named(&mut app, NamedKey::F3, false);
        app.view.file_command("file.new").unwrap();
        app.apply(Damage::default());
        finish(&mut app, &rx);
        assert_eq!(app.view.editor().unwrap().primary_cursor().head, 0, "old tab hit must not navigate the new tab");
        query(&mut app, "absent");
        finish(&mut app, &rx);
        assert_eq!(app.search_panel.count(), (0, true));
        stop(app);
    }
    #[test]
    fn actual_host_ime_field_fence_save_prompt_priority_and_replace_gate() {
        let (mut app, rx) = host(DOCUMENT);
        app.start_search();
        ctrl(&mut app, "f");
        text(&mut app, "needle");
        finish(&mut app, &rx);
        let destination = app.capture_search_composition().unwrap();
        named(&mut app, NamedKey::ArrowLeft, false);
        app.search_commit(destination, "BAD");
        assert_eq!(app.search_panel.text(SearchField::Query).0, "needle");
        assert!(app.view.notice().contains("cancelled"));
        named(&mut app, NamedKey::Escape, false);
        app.view.file_command("file.saveAs").unwrap();
        app.sync_file_ui();
        ctrl(&mut app, "f");
        assert!(!app.search_panel.is_open());
        assert!(matches!(app.capture_input_destination(), ime::Destination::Prompt { .. }));
        named(&mut app, NamedKey::Escape, false);
        ctrl(&mut app, "h");
        assert_eq!(app.search_panel.mode(), SearchMode::Replace);
        let revision = app.view.editor().unwrap().document().revision();
        press(&mut app, Key::Named(NamedKey::Enter), None, ModifiersState::CONTROL | ModifiersState::SHIFT);
        assert_eq!(app.view.editor().unwrap().document().revision(), revision);
        assert!(app.search_panel.status().contains("W2"));
        stop(app);
    }
}
