//! Settings panel drawing. Persistence and input state live in the shared host.
use super::*;
use kwil_core_commands::settings_ui::{CATEGORIES, SettingValue};
use kwil_view_model::palette_session::{display_text, text_window};
use kwil_view_model::settings_session::{SettingsInput, SettingsSession};

pub(super) fn value_text(value: &SettingValue) -> String {
    match value {
        SettingValue::Bool(value) => if *value { "[x] On" } else { "[ ] Off" }.into(),
        SettingValue::Integer(value) => value.to_string(),
        SettingValue::Float(value) => value.to_string(),
        SettingValue::Text(value) => display_text(value),
        SettingValue::Strings(values) => values.iter().map(|s| display_text(s)).collect::<Vec<_>>().join(", "),
        SettingValue::StringMap(values) => format!("{} entries", values.len()),
        SettingValue::Bindings(values) => format!("{} shortcuts", values.len()),
        SettingValue::Formatters(values) => format!("{} formatters", values.len()),
    }
}

pub(super) fn panel(size: (usize, usize), cw: usize, ch: usize) -> Option<Rect> {
    let columns = (size.0 / cw.max(1)).saturating_sub(4).min(110);
    let rows = (size.1 / ch.max(1)).saturating_sub(2).min(32);
    if columns < 12 || rows < 7 {
        return None;
    }
    let (w, h) = (columns * cw, rows * ch);
    Some(Rect { x: (size.0 - w) / 2, y: ((size.1 - h) / 2 / ch) * ch, w, h })
}

pub(super) fn loading(pixels: &mut [u32], size: (usize, usize), font: &mut Font) -> Option<Rect> {
    let area = panel(size, font.cw, font.ch)?;
    fill(pixels, size.0, area, STATUS_BG);
    let (cw, ch) = (font.cw, font.ch);
    draw_text(
        pixels,
        size.0,
        font,
        area.x + cw,
        area.y + ch,
        b"Loading settings...",
        area.w / cw - 2,
        STATUS_FG,
        STATUS_BG,
    );
    Some(area)
}

pub(super) fn draw(host: &SettingsSession, pixels: &mut [u32], size: (usize, usize), font: &mut Font) -> Option<Rect> {
    let area = panel(size, font.cw, font.ch)?;
    let (cw, ch) = (font.cw, font.ch);
    let columns = area.w / cw - 2;
    let rows = area.h / ch;
    let wide = columns + 2 >= 80 && rows >= 20;
    let rail = if wide { 23 } else { 0 };
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
    line(0, 0, columns + 2, "Settings                 Open settings file: Ctrl+O", STATUS_FG, STATUS_BG);
    let search_window = text_window(host.search_text(), host.input_cursor(), columns);
    let search = if host.search_text().is_empty() { "/ Type to search settings" } else { &search_window.text };
    if host.is_searching() {
        caret = Some((1, 1 + search_window.cursor));
    }
    line(1, 0, columns + 2, search, if host.search_text().is_empty() { GUTTER_FG } else { FG }, tabs::INACTIVE);
    if wide {
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
    let list = host.rows();
    let selected = host.selected();
    let visible = rows.saturating_sub(if wide { 10 } else { 7 }).max(1);
    let first = selected.saturating_sub(visible / 2).min(list.len().saturating_sub(visible));
    for (index, row) in list.iter().enumerate().skip(first).take(visible) {
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
        line(rows - 4, rail, columns + 2 - rail, selected.spec.description, GUTTER_FG, STATUS_BG);
        line(
            rows - 3,
            rail,
            columns + 2 - rail,
            &format!("Default: {}", value_text(&selected.default)),
            GUTTER_FG,
            STATUS_BG,
        );
    }
    line(rows - 2, 0, columns + 2, host.status(), STATUS_FG, STATUS_BG);
    line(
        rows - 1,
        0,
        columns + 2,
        "Enter edit  Del reset  / search  Tab category  Ctrl+O settings file  Esc back",
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
        if x < area.x || x >= area.x + area.w || y < area.y || y >= area.y + area.h {
            if !host.is_editing() {
                host.close();
            }
        } else {
            let (column, row) = ((x - area.x) / cw, (y - area.y) / ch);
            let (columns, rows) = (area.w / cw, area.h / ch);
            let wide = columns >= 80 && rows >= 20;
            if row == 0 && column >= 25 {
                self.dispatch_settings("view.settingsFile");
                return;
            } else if row == 1 {
                let _ = host.input(SettingsInput::Search);
            } else if wide && column < 22 && (3..3 + CATEGORIES.len() + 1).contains(&row) {
                host.set_category(row.checked_sub(4).map(|index| CATEGORIES[index]));
            } else {
                let visible = rows.saturating_sub(if wide { 10 } else { 7 }).max(1);
                if (3..3 + visible).contains(&row) {
                    let length = host.rows().len();
                    let first = host.selected().saturating_sub(visible / 2).min(length.saturating_sub(visible));
                    let selected = first + row - 3;
                    if selected < length {
                        let activate = host.selected() == selected && column > columns / 2;
                        host.select(selected);
                        if activate {
                            let _ = host.input(SettingsInput::Accept);
                        }
                    }
                }
            }
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
                self.settings_file_pending = true;
                if let Err(error) = panel.input(SettingsInput::Save) {
                    self.settings_file_pending = false;
                    self.view.set_notice(format!("Settings: {error}"));
                } else if panel.source_exists() && !panel.is_saving() && !panel.is_dirty() {
                    let path = panel.path().to_path_buf();
                    panel.close();
                    self.settings_file_pending = false;
                    if let Err(error) = self.view.open_path(path) {
                        self.view.set_notice(format!("Open settings: {error}"));
                    }
                    self.sync_file_ui();
                    self.apply(Damage { full: true, status: true, ..Default::default() });
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
        if let Some(panel) = &mut self.settings_panel
            && let Err(error) = panel.input(input)
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
        let text_mode = panel.is_editing() || panel.is_searching();
        let command = self.mods.control_key() || self.mods.super_key();
        if command && matches!(&event.logical_key, Key::Character(key) if key.eq_ignore_ascii_case("o")) {
            self.dispatch_settings("view.settingsFile");
            return true;
        }
        if command && matches!(&event.logical_key, Key::Character(key) if key.eq_ignore_ascii_case("r")) {
            if let Some(panel) = &mut self.settings_panel {
                let _ = panel.adopt_external();
            }
            self.palette_redraw();
            return true;
        }
        if matches!(event.logical_key, Key::Named(NamedKey::Tab)) && !panel.is_editing() {
            self.settings_panel.as_mut().unwrap().cycle_category(self.mods.shift_key());
            self.palette_redraw();
            return true;
        }
        let input = match &event.logical_key {
            Key::Named(NamedKey::Escape) => Some(SettingsInput::Cancel),
            Key::Named(NamedKey::Enter) => Some(SettingsInput::Accept),
            Key::Named(NamedKey::ArrowDown) if !panel.is_editing() => Some(SettingsInput::Next),
            Key::Named(NamedKey::ArrowUp) if !panel.is_editing() => Some(SettingsInput::Previous),
            Key::Named(NamedKey::Backspace) if text_mode => Some(SettingsInput::Backspace),
            Key::Named(NamedKey::Delete) => Some(if text_mode { SettingsInput::Delete } else { SettingsInput::Reset }),
            Key::Named(NamedKey::ArrowLeft) if text_mode => Some(SettingsInput::Left),
            Key::Named(NamedKey::ArrowRight) if text_mode => Some(SettingsInput::Right),
            Key::Named(NamedKey::Home) if text_mode => Some(SettingsInput::Home),
            Key::Named(NamedKey::End) if text_mode => Some(SettingsInput::End),
            Key::Character(key) if command && key.eq_ignore_ascii_case("s") => Some(SettingsInput::Save),
            Key::Character(key) if !text_mode && key == " " => Some(SettingsInput::Accept),
            Key::Character(key) if !text_mode && key == "/" => Some(SettingsInput::Search),
            _ if !self.composing && (!command || self.mods.alt_key()) => {
                event.text.as_deref().filter(|text| !text.chars().any(char::is_control)).map(SettingsInput::Text)
            }
            _ => None,
        };
        if let Some(input) = input {
            self.settings_input(input);
        }
        true
    }
}
