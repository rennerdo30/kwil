//! Settings panel drawing. Persistence and input state live in the shared host.
use super::*;
use kwil_core_commands::settings_ui::{CATEGORIES, SettingValue};
use kwil_view_model::palette_session::{display_text, text_window};
use kwil_view_model::settings_panels::{EditColumn, PanelInput, PanelWindow};
use kwil_view_model::settings_session::{SettingsInput, SettingsSession};

/// Drawing and hit testing share the same viewport, including compact cards.
#[derive(Clone, Copy)]
struct Layout {
    columns: usize,
    rows: usize,
    rail: usize,
    visible: usize,
}
impl Layout {
    fn new(columns: usize, rows: usize) -> Self {
        let wide = columns >= 80 && rows >= 20;
        Self {
            columns,
            rows,
            rail: if wide { 23 } else { 0 },
            visible: rows.saturating_sub(if wide { 10 } else { 7 }).max(1),
        }
    }
    fn first(self, selected: usize, total: usize) -> usize {
        selected.saturating_sub(self.visible / 2).min(total.saturating_sub(self.visible))
    }
    fn description_row(self) -> Option<usize> {
        self.rows.checked_sub(4).filter(|row| *row >= 3 + self.visible)
    }
    fn hit(self, column: usize, row: usize) -> MouseTarget {
        if column >= self.columns || row >= self.rows {
            return MouseTarget::None;
        }
        if row == 0 && column >= 25 {
            MouseTarget::OpenFile
        } else if row == 1 {
            MouseTarget::Search
        } else if self.rail != 0 && column < 22 && (3..4 + CATEGORIES.len()).contains(&row) {
            MouseTarget::Category(row - 3)
        } else if column >= self.rail && (3..3 + self.visible).contains(&row) {
            MouseTarget::Row(row - 3)
        } else {
            MouseTarget::None
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum MouseTarget {
    None,
    Close,
    OpenFile,
    Search,
    Category(usize),
    Row(usize),
}

fn mouse_route(layout: Layout, cell: Option<(usize, usize)>, editing: bool) -> MouseTarget {
    if editing {
        return MouseTarget::None;
    }
    match cell {
        Some((column, row)) => layout.hit(column, row),
        None => MouseTarget::Close,
    }
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

fn compound_window(host: &SettingsSession, layout: Layout) -> Result<Option<PanelWindow>, String> {
    let Some(meta) = host.panel_window(0, 0)? else { return Ok(None) };
    host.panel_window(layout.first(meta.selection, meta.total), layout.visible)
}

fn compound_row(window: &PanelWindow, slot: usize) -> Option<usize> {
    window.rows.get(slot)?;
    let index = window.offset.checked_add(slot)?;
    (index < window.total).then_some(index)
}

/// Keep both staged fields visible on the full card. Tiny cards show the active
/// field first; its caret must never be clipped behind an inactive field.
fn edit_fields(window: &PanelWindow, width: usize, slots: usize) -> (Vec<String>, Option<(usize, usize)>) {
    let Some(edit) = &window.edit_text else { return (Vec::new(), None) };
    let mut fields = vec![(EditColumn::Key, "Key: ", &edit.key), (EditColumn::Value, "Value: ", &edit.value)];
    if slots < 2 && edit.column == EditColumn::Value {
        fields.swap(0, 1);
    }
    let mut lines = Vec::new();
    let mut caret = None;
    for (column, label, text) in fields.into_iter().take(slots) {
        let active = column == edit.column;
        let input = text_window(text, if active { edit.cursor } else { 0 }, width.saturating_sub(label.len()));
        if active && width > label.len() {
            caret = Some((lines.len(), label.len() + input.cursor));
        }
        lines.push(format!("{label}{}", input.text));
    }
    (lines, caret)
}

#[derive(Clone, Copy)]
struct KeyMode {
    compound: bool,
    capturing: bool,
    editing: bool,
    searching: bool,
}
enum KeyRoute<'a> {
    Ignore,
    Panel(PanelInput<'a>),
    Settings(SettingsInput<'a>),
    Paste,
    OpenFile,
    AdoptExternal,
    Category(bool),
}

fn key_route<'a>(
    key: &Key,
    text: Option<&'a str>,
    mods: winit::keyboard::ModifiersState,
    composing: bool,
    mode: KeyMode,
) -> KeyRoute<'a> {
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

pub(super) fn value_text(value: &SettingValue) -> String {
    match value {
        SettingValue::Bool(value) => if *value { "[x] On" } else { "[ ] Off" }.into(),
        SettingValue::Integer(value) => value.to_string(),
        // Floating schema fields are stored as f32 font preferences.
        SettingValue::Float(value) => (*value as f32).to_string(),
        SettingValue::Text(value) => display_text(value),
        SettingValue::Strings(values) => values.iter().map(|s| display_text(s)).collect::<Vec<_>>().join(", "),
        SettingValue::StringMap(values) => format!("{} entries", values.len()),
        SettingValue::Bindings(values) => format!("{} shortcuts", values.len()),
        SettingValue::Formatters(values) => format!("{} formatters", values.len()),
    }
}

pub(super) fn panel(size: (usize, usize), cw: usize, ch: usize) -> Option<Rect> {
    let (cw, ch) = (cw.max(1), ch.max(1));
    let columns = (size.0 / cw.max(1)).saturating_sub(4).min(110);
    let rows = (size.1 / ch.max(1)).saturating_sub(2).min(32);
    if columns < 12 || rows < 7 {
        return None;
    }
    let (w, h) = (columns * cw, rows * ch);
    Some(Rect { x: (size.0 - w) / 2, y: ((size.1 - h) / 2 / ch) * ch, w, h })
}

pub(super) fn loading(
    host: &SettingsSession,
    pixels: &mut [u32],
    size: (usize, usize),
    font: &mut Font,
) -> Option<Rect> {
    let area = panel(size, font.cw, font.ch)?;
    fill(pixels, size.0, area, STATUS_BG);
    let (cw, ch) = (font.cw, font.ch);
    draw_text(
        pixels,
        size.0,
        font,
        area.x + cw,
        area.y + ch,
        host.last_error().unwrap_or("Loading settings...").as_bytes(),
        area.w / cw - 2,
        STATUS_FG,
        STATUS_BG,
    );
    if host.last_error().is_some() {
        draw_text(
            pixels,
            size.0,
            font,
            area.x + cw,
            area.y + 3 * ch,
            b"Ctrl+O Open settings file to repair   Esc Close",
            area.w / cw - 2,
            STATUS_FG,
            STATUS_BG,
        );
    }
    Some(area)
}

pub(super) fn draw(host: &SettingsSession, pixels: &mut [u32], size: (usize, usize), font: &mut Font) -> Option<Rect> {
    let area = panel(size, font.cw, font.ch)?;
    let (cw, ch) = (font.cw, font.ch);
    let columns = area.w / cw - 2;
    let rows = area.h / ch;
    let layout = Layout::new(columns + 2, rows);
    let rail = layout.rail;
    let compound = compound_window(host, layout);
    let window = compound.as_ref().ok().and_then(Option::as_ref);
    let in_panel = host.panel().is_some();
    let mut caret = None;
    fill(pixels, size.0, area, STATUS_BG);
    let mut line = |row: usize, column: usize, width: usize, text: &str, foreground, background| {
        let y = area.y + row * ch;
        fill(pixels, size.0, Rect { x: area.x + column * cw, y, w: width * cw, h: ch }, background);
        let text = display_text(text);
        draw_text(
            pixels,
            size.0,
            font,
            area.x + (column + 1) * cw,
            y,
            text.as_bytes(),
            width.saturating_sub(2),
            foreground,
            background,
        );
    };
    let title = window.map_or("Settings", |window| window.title);
    line(0, 0, columns + 2, &format!("{title:<25}Open settings file: Ctrl+O"), STATUS_FG, STATUS_BG);
    let query = window.map_or(host.search_text(), |window| window.query.as_str());
    let query_cursor = if in_panel { host.panel_query_cursor() } else { host.input_cursor() };
    let search_window = text_window(query, query_cursor, columns);
    let search = if !query.is_empty() {
        &search_window.text
    } else if in_panel {
        "Type to search settings"
    } else {
        "/ Type to search settings"
    };
    if (in_panel && !host.panel_is_editing()) || (!in_panel && host.is_searching()) {
        caret = Some((1, 1 + search_window.cursor));
    }
    line(1, 0, columns + 2, search, if query.is_empty() { GUTTER_FG } else { FG }, tabs::INACTIVE);
    if rail != 0 {
        for (index, category) in std::iter::once(None).chain(CATEGORIES.iter().copied().map(Some)).enumerate() {
            line(
                index + 3,
                0,
                22,
                category.map_or("All", |c| c.title()),
                if category == host.category() { STATUS_FG } else { GUTTER_FG },
                if category == host.category() { tabs::ACTIVE } else { STATUS_BG },
            );
        }
    } else {
        line(
            2,
            0,
            columns + 2,
            &format!("< {} >   Tab: category", host.category().map_or("All", |c| c.title())),
            STATUS_FG,
            STATUS_BG,
        );
    }
    if in_panel {
        match &compound {
            Err(error) => {
                line(3, rail, columns + 2 - rail, error, STATUS_FG, STATUS_BG);
            }
            Ok(None) => {
                line(
                    3,
                    rail,
                    columns + 2 - rail,
                    "Settings panel unavailable; Esc back or Ctrl+O to repair",
                    STATUS_FG,
                    STATUS_BG,
                );
            }
            Ok(Some(window)) => {
                for (slot, row) in window.rows.iter().enumerate() {
                    let source = if row.source.is_empty() { String::new() } else { format!(" [{}]", row.source) };
                    let conflicts = if row.conflicts.is_empty() {
                        String::new()
                    } else {
                        format!(" !{} conflict(s)", row.conflicts.len())
                    };
                    let text = if let Some(command) = row.command {
                        format!("{}: {}{source}{conflicts} ({command})", row.title, row.value)
                    } else if row.add {
                        format!("+ {}", row.title)
                    } else {
                        format!("{}: {}{source}", row.title, row.value)
                    };
                    line(
                        slot + 3,
                        rail,
                        columns + 2 - rail,
                        &text,
                        FG,
                        if row.selected { tabs::ACTIVE } else { STATUS_BG },
                    );
                }
                let first_detail = 3 + layout.visible;
                let slots = (rows - 2).saturating_sub(first_detail);
                let width = columns.saturating_sub(rail);
                let (mut details, edit_caret) = edit_fields(window, width, slots);
                if let Some((row, column)) = edit_caret {
                    caret = Some((first_detail + row, rail + 1 + column));
                }
                if !window.confirmation.is_empty() {
                    details
                        .push(format!("Replace {} conflict(s)? Enter accepts; Esc cancels", window.confirmation.len()));
                    for conflict in &window.confirmation {
                        details.push(format!("{}: {} ({:?})", conflict.keys, conflict.command, conflict.kind));
                    }
                } else if let Some(keys) = &window.capture {
                    details.push(if keys.is_empty() {
                        "Press first shortcut (Enter is a key); Esc cancels".into()
                    } else {
                        format!("Captured: {keys}; Enter accepts, or press second chord")
                    });
                } else if let Some(selected) = window.rows.iter().find(|row| row.selected) {
                    details.push(selected.description.clone());
                    details.push(format!("Default: {}", selected.default));
                    details.push(format!(
                        "Source: {}  Context: {:?}  Mode: {:?}",
                        selected.source, selected.context, selected.mode
                    ));
                    for conflict in &selected.conflicts {
                        details
                            .push(format!("Conflict: {} → {} ({:?})", conflict.keys, conflict.command, conflict.kind));
                    }
                }
                for (slot, text) in details.iter().take(slots).enumerate() {
                    line(first_detail + slot, rail, columns + 2 - rail, text, GUTTER_FG, STATUS_BG);
                }
            }
        }
    } else {
        let list = host.rows();
        let selected = host.selected();
        let first = layout.first(selected, list.len());
        for (index, row) in list.iter().enumerate().skip(first).take(layout.visible) {
            let background = if selected == index { tabs::ACTIVE } else { STATUS_BG };
            let value = if selected == index && host.is_editing() {
                let start = rail + row.spec.title.len() + 5;
                let input = text_window(host.input_text(), host.input_cursor(), (columns + 1).saturating_sub(start));
                caret = Some((index - first + 3, start + input.cursor));
                input.text
            } else {
                value_text(&row.current)
            };
            let marker = if row.current == row.default { " " } else { "*" };
            line(
                index - first + 3,
                rail,
                columns + 2 - rail,
                &format!("{marker} {}: {}", row.spec.title, value),
                FG,
                background,
            );
        }
        if let Some(selected) = list.get(selected) {
            if let Some(row) = layout.description_row() {
                line(row, rail, columns + 2 - rail, selected.spec.description, GUTTER_FG, STATUS_BG);
            }
            line(
                rows - 3,
                rail,
                columns + 2 - rail,
                &format!("Default: {}", value_text(&selected.default)),
                GUTTER_FG,
                STATUS_BG,
            );
        }
    }
    line(rows - 2, 0, columns + 2, host.status(), STATUS_FG, STATUS_BG);
    line(
        rows - 1,
        0,
        columns + 2,
        if window.is_some_and(|window| !window.confirmation.is_empty()) {
            "Enter: replace conflicts   Esc: cancel (keep existing shortcuts)"
        } else if host.panel_is_capturing() {
            "Press keys: first Enter binds Enter; next Enter accepts; second chord finishes; Esc cancels"
        } else if host.panel_is_editing() {
            "Enter: next field / apply   Tab: Key / Value   Shift+Enter: newline   Esc: cancel"
        } else if in_panel {
            "Enter edit  Del remove/reset  Type search  Tab category  Ctrl+O settings file  Esc back"
        } else {
            "Enter edit  Del reset  / search  Tab category  Ctrl+O settings file  Esc back"
        },
        GUTTER_FG,
        STATUS_BG,
    );
    if let Some((row, column)) = caret {
        fill(
            pixels,
            size.0,
            Rect { x: area.x + column.min(columns) * cw, y: area.y + (row + 1) * ch - 2.min(ch), w: cw, h: 1 },
            STATUS_FG,
        );
    }
    Some(area)
}

impl App {
    pub(super) fn settings_mouse(&mut self) {
        let (cw, ch) = (self.font.as_ref().map_or(8, |font| font.cw), self.ch());
        let Some(area) = panel(self.size, cw, ch) else { return };
        let (x, y) = self.pointer;
        let Some(host) = self.settings_panel.as_mut() else { return };
        let cell = (x >= area.x && x < area.x + area.w && y >= area.y && y < area.y + area.h)
            .then(|| ((x - area.x) / cw, (y - area.y) / ch));
        let layout = Layout::new(area.w / cw, area.h / ch);
        // Clicks never change the destination of a scalar draft, capture,
        // staged key/value edit or pending conflict confirmation.
        let result = match mouse_route(layout, cell, host.is_editing() || host.panel_is_editing()) {
            MouseTarget::Close => {
                host.close();
                Ok(())
            }
            MouseTarget::OpenFile => {
                self.dispatch_settings("view.settingsFile");
                return;
            }
            MouseTarget::Search if host.panel().is_none() => host.input(SettingsInput::Search),
            MouseTarget::Category(index) => {
                host.set_category(index.checked_sub(1).map(|index| CATEGORIES[index]));
                Ok(())
            }
            MouseTarget::Row(slot) if host.panel().is_some() => {
                // Never fall back to schema-row indices for compound pages.
                compound_window(host, layout).and_then(|window| {
                    let Some(window) = window else { return Ok(()) };
                    let Some(selected) = compound_row(&window, slot) else { return Ok(()) };
                    let activate = window.selection == selected && cell.unwrap().0 > layout.columns / 2;
                    host.panel_select(selected)?;
                    if activate { host.panel_input(PanelInput::Accept) } else { Ok(()) }
                })
            }
            MouseTarget::Row(slot) => {
                let length = host.rows().len();
                let selected = layout.first(host.selected(), length) + slot;
                if selected < length {
                    let activate = host.selected() == selected && cell.unwrap().0 > layout.columns / 2;
                    host.select(selected);
                    if activate { host.input(SettingsInput::Accept) } else { Ok(()) }
                } else {
                    Ok(())
                }
            }
            _ => return,
        };
        if let Err(error) = result {
            self.view.set_notice(format!("Settings: {error}"));
        }
        self.settings_epoch = self.settings_epoch.wrapping_add(1);
        self.palette_redraw();
    }

    pub(super) fn settings_open(&self) -> bool {
        self.settings_panel.as_ref().is_some_and(SettingsSession::is_open)
    }

    pub(super) fn dispatch_settings(&mut self, id: &str) -> bool {
        if !matches!(id, "view.settings" | "view.settingsFile") {
            return false;
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
        let panel = self.settings_panel.as_ref().unwrap();
        let mode = KeyMode {
            compound: panel.panel().is_some(),
            capturing: panel.panel_is_capturing(),
            editing: panel.is_editing() || panel.panel_is_editing(),
            searching: panel.is_searching(),
        };
        match key_route(&event.logical_key, event.text.as_deref(), self.mods, self.composing, mode) {
            KeyRoute::Panel(input) => self.settings_panel_input(input),
            KeyRoute::Settings(input) => self.settings_input(input),
            KeyRoute::Paste => {
                let epoch = self.settings_epoch;
                let destination = panel.panel();
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
    use kwil_view_model::settings_panels::SettingsPanelController;
    use kwil_view_model::settings_session::SettingsPanel;

    fn mode(compound: bool, editing: bool, capturing: bool) -> KeyMode {
        KeyMode { compound, editing, capturing, searching: false }
    }

    fn routed_panel<'a>(key: &Key, text: Option<&'a str>, mods: ModifiersState, mode: KeyMode) -> PanelInput<'a> {
        match key_route(key, text, mods, false, mode) {
            KeyRoute::Panel(input) => input,
            _ => panic!("compound input escaped its controller"),
        }
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
    fn settings_gui_staged_mapping_uses_fields_and_preserves_unicode_caret() {
        let doc = SettingsDocument::parse("").unwrap();
        let mut panel = SettingsPanelController::new(SettingsPanel::FileTypes, &doc).unwrap();
        panel.input(&doc, PanelInput::Accept).unwrap();
        let editing = mode(true, true, false);
        panel
            .input(&doc, routed_panel(&Key::Character("rs".into()), Some("rs"), ModifiersState::empty(), editing))
            .unwrap();
        panel.input(&doc, routed_panel(&Key::Named(NamedKey::Tab), None, ModifiersState::empty(), editing)).unwrap();
        assert!(panel.is_editing());
        panel.input(&doc, PanelInput::Text("a界e\u{301}\nZ")).unwrap();
        let window = panel.window(&doc, 0, 1).unwrap();
        let (fields, caret) = edit_fields(&window, 11, 1);
        assert_eq!(fields, ["Value: \\nZ"]);
        assert_eq!(caret, Some((0, 10)));
        assert!(matches!(
            key_route(&Key::Character("v".into()), None, ModifiersState::CONTROL, false, editing),
            KeyRoute::Paste
        ));
        assert!(matches!(
            key_route(&Key::Character("o".into()), None, ModifiersState::CONTROL, false, editing),
            KeyRoute::OpenFile
        ));
        panel.input(&doc, PanelInput::Home).unwrap();
        for _ in 0..5 {
            panel.input(&doc, PanelInput::Delete).unwrap();
        }
        panel.input(&doc, PanelInput::Text("rust")).unwrap();
        let edit = panel
            .input(&doc, routed_panel(&Key::Named(NamedKey::Enter), None, ModifiersState::empty(), editing))
            .unwrap()
            .unwrap();
        let mut changed = doc.clone();
        edit.apply(&mut changed).unwrap();
        assert_eq!(changed.language_for_extension("rs"), Some("rust"));
    }

    #[test]
    fn settings_gui_formatter_and_string_map_keyboard_edits_stay_typed() {
        let mut doc = SettingsDocument::parse("").unwrap();
        for (kind, key, value) in [
            (SettingsPanel::Formatters, "rust.executable", "E:/Tools/rust fmt.exe"),
            (SettingsPanel::Formatters, "rust.args", "--stdin-filepath"),
            (SettingsPanel::StringMap { id: "theme.colors" }, "background", "#123456"),
            (SettingsPanel::StringMap { id: "log.highlight_rules" }, "error", "red"),
        ] {
            let mut panel = SettingsPanelController::new(kind, &doc).unwrap();
            panel.search(&doc, "unique new entry").unwrap();
            let browse = mode(true, false, false);
            let editing = mode(true, true, false);
            panel
                .input(&doc, routed_panel(&Key::Named(NamedKey::Enter), None, ModifiersState::empty(), browse))
                .unwrap();
            panel
                .input(&doc, routed_panel(&Key::Character(key.into()), Some(key), ModifiersState::empty(), editing))
                .unwrap();
            panel
                .input(&doc, routed_panel(&Key::Named(NamedKey::Enter), None, ModifiersState::empty(), editing))
                .unwrap();
            panel
                .input(&doc, routed_panel(&Key::Character(value.into()), Some(value), ModifiersState::empty(), editing))
                .unwrap();
            if key == "rust.args" {
                let newline = routed_panel(&Key::Named(NamedKey::Enter), None, ModifiersState::SHIFT, editing);
                panel.input(&doc, newline).unwrap();
                panel.input(&doc, PanelInput::Text("a file.rs")).unwrap();
            }
            let edit = panel
                .input(&doc, routed_panel(&Key::Named(NamedKey::Enter), None, ModifiersState::empty(), editing))
                .unwrap()
                .unwrap();
            edit.apply(&mut doc).unwrap();
        }
        assert_eq!(doc.settings().formatters["rust"].executable, "E:/Tools/rust fmt.exe");
        assert_eq!(doc.settings().formatters["rust"].args, ["--stdin-filepath", "a file.rs"]);
        assert_eq!(doc.settings().theme.colors["background"], "#123456");
        assert_eq!(doc.settings().log.highlight_rules["error"], "red");
    }

    #[test]
    fn settings_gui_mouse_rail_and_bounded_rows_never_retarget_drafts() {
        let layout = Layout::new(110, 32);
        for cell in [None, Some((1, 3)), Some((1, 4)), Some((30, 1)), Some((70, 3)), Some((40, 0))] {
            assert_eq!(mouse_route(layout, cell, true), MouseTarget::None);
        }
        assert_eq!(mouse_route(layout, Some((1, 3)), false), MouseTarget::Category(0));
        assert_eq!(mouse_route(layout, Some((21, 4)), false), MouseTarget::Category(1));
        assert_eq!(mouse_route(layout, Some((22, 3)), false), MouseTarget::None);
        assert_eq!(mouse_route(layout, Some((23, 3)), false), MouseTarget::Row(0));
        assert_eq!(mouse_route(layout, Some((109, 3 + layout.visible)), false), MouseTarget::None);
        let doc = SettingsDocument::parse("[file_types]\na = 'rust'\nb = 'json'\n").unwrap();
        let panel = SettingsPanelController::new(SettingsPanel::FileTypes, &doc).unwrap();
        let window = panel.window(&doc, 1, 1).unwrap();
        assert_eq!(compound_row(&window, 0), Some(1));
        assert_eq!(compound_row(&window, 1), None);
        let compact = Layout::new(79, 19);
        assert_eq!(mouse_route(compact, Some((1, 3)), false), MouseTarget::Row(0));
        assert_eq!(layout.first(198, 200) + layout.visible, 200);
    }

    #[test]
    fn settings_gui_minimum_card_never_overwrites_selected_row() {
        for rows in 7..=32 {
            for columns in [12, 79, 80, 110] {
                let layout = Layout::new(columns, rows);
                if let Some(description) = layout.description_row() {
                    assert!(description >= 3 + layout.visible, "description overlaps a setting");
                }
                assert!(rows - 3 >= 3 + layout.visible, "default overlaps a setting");
            }
        }
        assert_eq!(Layout::new(12, 7).description_row(), None);
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
}
