//! Bounded command/file palette overlay. Matching and discovery live in the
//! shared host; this module only draws its capped immutable result snapshot.
use super::*;
use kwil_core_commands::palette::Palette;
use kwil_view_model::palette_session::{display_text, query_window};

pub(super) fn area(size: (usize, usize), cw: usize, ch: usize) -> Option<Rect> {
    let (cw, ch) = (cw.max(1), ch.max(1));
    let columns = (size.0 / cw).min(80);
    let rows = (size.1 / ch).min(14);
    if columns < 4 || rows < 3 {
        return None;
    }
    let (w, h) = (columns * cw, rows * ch);
    Some(Rect { x: (size.0 - w) / 2, y: ((size.1 - h) / 3 / ch) * ch, w, h })
}

pub(super) fn draw(palette: &Palette, pixels: &mut [u32], size: (usize, usize), font: &mut Font) -> Option<Rect> {
    if !palette.is_open() {
        return None;
    }
    let panel = area(size, font.cw, font.ch)?;
    let (cw, ch) = (font.cw, font.ch);
    let columns = panel.w / cw - 2;
    let rows = panel.h / ch;
    let visible = rows - 2;
    let selected = palette.selected().unwrap_or(0);
    let first = selected.saturating_sub(visible - 1);
    fill(pixels, size.0, panel, STATUS_BG);
    let query = query_window(palette, columns);
    draw_text(pixels, size.0, font, panel.x + cw, panel.y, query.text.as_bytes(), columns, STATUS_FG, STATUS_BG);
    fill(
        pixels,
        size.0,
        Rect { x: panel.x + cw + query.cursor * cw, y: panel.y + ch - 2.min(ch), w: cw, h: 1 },
        STATUS_FG,
    );
    for (row, item) in palette.rows().iter().enumerate().skip(first).take(visible) {
        let y = panel.y + (row - first + 1) * ch;
        let bg = if palette.selected() == Some(row) { tabs::ACTIVE } else { STATUS_BG };
        fill(pixels, size.0, Rect { x: panel.x, y, w: panel.w, h: ch }, bg);
        let title = display_text(&item.title);
        draw_text(pixels, size.0, font, panel.x + cw, y, title.as_bytes(), columns, FG, bg);
    }
    let footer = if palette.results_are_fresh() { "Enter: open   Esc: cancel" } else { "Searching...   Esc: cancel" };
    draw_text(
        pixels,
        size.0,
        font,
        panel.x + cw,
        panel.y + (rows - 1) * ch,
        footer.as_bytes(),
        columns,
        GUTTER_FG,
        STATUS_BG,
    );
    Some(panel)
}

impl App {
    pub(super) fn capture_input_destination(&self) -> ime::Destination {
        match self.palette.as_ref().filter(|palette| palette.model().is_open()) {
            Some(palette) => ime::Destination::Palette { epoch: palette.input_epoch() },
            None => ime::capture(&self.view, self.insert_mode()),
        }
    }
    pub(super) fn palette_open(&self) -> bool {
        self.palette.as_ref().is_some_and(|palette| palette.model().is_open())
    }

    pub(super) fn palette_redraw(&mut self) {
        self.palette_dirty = true;
        if let Some(gfx) = &self.gfx {
            self.redraw_requested = true;
            gfx.window.request_redraw();
        }
    }

    pub(super) fn dispatch_palette(&mut self, command: &str) -> bool {
        let query = match command {
            "view.commandPalette" => ">",
            "view.fileFinder" => "",
            "navigation.goToLine" => ":",
            _ => return false,
        };
        if !self.view.recovery_ready() {
            let damage = self.view.set_notice("Workspace recovery is not ready; try the palette again shortly".into());
            self.apply(damage);
            return true;
        }
        let tabs = self
            .view
            .session()
            .tabs()
            .iter()
            .map(|tab| kwil_core_commands::palette::Tab { id: tab.id.0, title: self.view.tab_title(tab.id) })
            .collect();
        let result =
            self.palette.as_mut().ok_or("Palette is starting; try again shortly".to_owned()).and_then(|palette| {
                palette.set_tabs(tabs).map_err(|error| format!("Palette: {error:?}"))?;
                palette.open(query).map_err(|error| format!("Palette: {error:?}"))
            });
        if let Err(error) = result {
            let damage = self.view.set_notice(error);
            self.apply(damage);
        } else {
            if self.ime_destination.is_some() || self.composing {
                self.ime_destination = Some(ime::Destination::Cancelled);
                self.composing = false;
            }
            self.drag = None;
            self.selecting = false;
            if let Some(keys) = &mut self.keys {
                keys.set_context(kwil_core_commands::Context::Palette);
            }
            self.palette_redraw();
        }
        true
    }

    pub(super) fn palette_input(&mut self, input: kwil_view_model::palette_session::PaletteInput<'_>) {
        let Some(palette) = &mut self.palette else { return };
        let result = palette.input(input);
        let closed = !palette.model().is_open();
        if closed && let Some(keys) = &mut self.keys {
            keys.set_context(kwil_core_commands::Context::Editor);
        }
        self.palette_redraw();
        match result {
            Ok(Some(action)) => self.palette_action(action),
            Ok(None) => {}
            Err(error) => {
                let damage = self.view.set_notice(format!("Palette: {error:?}"));
                self.apply(damage);
            }
        }
    }

    fn palette_action(&mut self, action: kwil_core_commands::palette::PaletteAction) {
        use kwil_core_commands::palette::PaletteAction;
        match action {
            PaletteAction::Command(command) => self.edit(input::Action::Command(command)),
            PaletteAction::OpenFile(path) => {
                if let Err(error) = self.view.open_path(path) {
                    self.view.set_notice(error.to_string());
                }
                self.sync_file_ui();
                self.apply(Damage { full: true, status: true, ..Default::default() });
            }
            PaletteAction::ActivateTab(id) => {
                if !self.view.activate(kwil_view_model::SessionDocumentId(id)) {
                    self.view.set_notice("Palette tab was closed".into());
                }
                self.sync_file_ui();
                self.apply(Damage { full: true, status: true, ..Default::default() });
            }
            PaletteAction::GoTo { line, column } => {
                let result = self.view.execute(
                    "navigation.goToLine",
                    &CommandArgs { line: Some(line), column: Some(column), ..Default::default() },
                    1,
                );
                match result {
                    Ok(result) => {
                        self.apply(result.damage);
                        self.keep_caret_visible();
                        self.command_status(result.status);
                    }
                    Err(error) => {
                        let damage = self.view.set_notice(error.to_string());
                        self.apply(damage);
                    }
                }
            }
        }
    }

    pub(super) fn palette_key(&mut self, event: &KeyEvent) -> bool {
        if !self.palette_open() {
            return false;
        }
        use kwil_view_model::palette_session::PaletteInput;
        if self.keys.is_none() {
            self.keys =
                kwil_core_commands::keymap::KeymapEngine::new(self.settings.keymap, &self.settings.bindings).ok();
        }
        if let Some(chord) = input::chord(&event.logical_key, self.mods)
            && let Some(keys) = &mut self.keys
        {
            keys.set_context(kwil_core_commands::Context::Palette);
            use kwil_core_commands::vim::VimResult;
            match keys.feed(chord) {
                VimResult::Pending => return true,
                VimResult::Command(call) => {
                    let input = match call.id {
                        "palette.accept" => Some(PaletteInput::Accept),
                        "palette.cancel" => Some(PaletteInput::Cancel),
                        "palette.next" => Some(PaletteInput::Next),
                        "palette.previous" => Some(PaletteInput::Previous),
                        "edit.backspace" => Some(PaletteInput::Backspace),
                        "edit.delete" => Some(PaletteInput::Delete),
                        "motion.left" => Some(PaletteInput::Left),
                        "motion.right" => Some(PaletteInput::Right),
                        "motion.lineStart" => Some(PaletteInput::Home),
                        "motion.lineEnd" => Some(PaletteInput::End),
                        _ => None,
                    };
                    if let Some(input) = input {
                        self.palette_input(input);
                    } else if call.id == "edit.paste" {
                        self.palette_paste();
                    } else if call.id.starts_with("edit.") || call.id.starts_with("motion.") {
                        let damage =
                            self.view.set_notice("This editor shortcut is unavailable in the palette query".into());
                        self.apply(damage);
                    } else {
                        self.palette_input(PaletteInput::Cancel);
                        self.palette_action(kwil_core_commands::palette::PaletteAction::Command(call.id));
                    }
                    return true;
                }
                VimResult::Error(error) => {
                    let damage = self.view.set_notice(error);
                    self.apply(damage);
                    return true;
                }
                VimResult::Unbound if keys.suppresses_fallback() => return true,
                VimResult::Unbound => {}
            }
        }
        let input = match event.logical_key {
            Key::Named(NamedKey::Backspace) => Some(PaletteInput::Backspace),
            Key::Named(NamedKey::Delete) => Some(PaletteInput::Delete),
            Key::Named(NamedKey::ArrowLeft) => Some(PaletteInput::Left),
            Key::Named(NamedKey::ArrowRight) => Some(PaletteInput::Right),
            Key::Named(NamedKey::Home) => Some(PaletteInput::Home),
            Key::Named(NamedKey::End) => Some(PaletteInput::End),
            _ if !self.mods.super_key() && (!self.mods.control_key() || self.mods.alt_key()) => {
                event.text.as_deref().filter(|text| !text.chars().any(char::is_control)).map(PaletteInput::Text)
            }
            _ => None,
        };
        if let Some(input) = input {
            self.palette_input(input);
        }
        true
    }

    fn palette_paste(&mut self) {
        use kwil_view_model::Clipboard;
        let hwnd = self.gfx.as_ref().map_or(0, |gfx| gfx.hwnd);
        let result = (|| -> std::io::Result<Option<String>> {
            let Some(data) = clipboard::SystemClipboard(hwnd).get()? else { return Ok(None) };
            let length = data
                .ranges
                .iter()
                .try_fold(0u64, |total, range| total.checked_add(range.end.saturating_sub(range.start)));
            if length.is_none_or(|length| length > kwil_core_commands::palette::MAX_QUERY_BYTES as u64) {
                return Err(std::io::Error::other("Palette paste exceeds the 4 KiB query limit"));
            }
            let mut bytes = Vec::new();
            data.write_to(&mut bytes)?;
            String::from_utf8(bytes).map(Some).map_err(std::io::Error::other)
        })();
        match result {
            Ok(Some(text)) => self.palette_input(kwil_view_model::palette_session::PaletteInput::Text(&text)),
            Ok(None) => {}
            Err(error) => {
                let damage = self.view.set_notice(error.to_string());
                self.apply(damage);
            }
        }
    }

    pub(super) fn palette_mouse(&mut self) {
        use kwil_view_model::palette_session::PaletteInput;
        let Some(area) = self.palette_area else { return };
        let (x, y) = self.pointer;
        if x < area.x || x >= area.x + area.w || y < area.y || y >= area.y + area.h {
            self.palette_input(PaletteInput::Cancel);
            return;
        }
        let row = (y - area.y) / self.ch();
        let visible = area.h / self.ch() - 2;
        if row == 0 || row > visible {
            return;
        }
        let Some(palette) = &mut self.palette else { return };
        let selected = palette.model().selected().unwrap_or(0);
        let target = selected.saturating_sub(visible - 1) + row - 1;
        if target >= palette.model().rows().len() {
            return;
        }
        for _ in selected..target {
            let _ = palette.input(PaletteInput::Next);
        }
        for _ in target..selected {
            let _ = palette.input(PaletteInput::Previous);
        }
        self.palette_input(PaletteInput::Accept);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn panel_is_inside_tiny_and_scaled_windows() {
        assert!(area((10, 10), 8, 16).is_none());
        for (size, cell) in [((800, 600), (8, 16)), ((3840, 2160), (24, 48)), ((33, 49), (8, 16))] {
            let rect = area(size, cell.0, cell.1).unwrap();
            assert!(rect.x + rect.w <= size.0 && rect.y + rect.h <= size.1);
            assert_eq!(rect.w % cell.0, 0);
            assert_eq!(rect.h % cell.1, 0);
        }
    }
}
