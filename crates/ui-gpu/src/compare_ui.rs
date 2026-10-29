//! Side-by-side read-only body; all source reads and refinement are worker-owned.
use super::*;
use kwil_view_model::compare_session::{Action, Kind, Session};
use std::sync::Arc;

impl App {
    pub(super) fn compare_ready(&mut self) {
        let previous = self.view.compare.page.as_ref().map(|p| (p.top, p.total, p.ignore_whitespace, p.collapsed));
        if !self.view.poll_compare() {
            return;
        }
        let current = self.view.compare.page.as_ref().map(|p| (p.top, p.total, p.ignore_whitespace, p.collapsed));
        let rows = self.view.rows();
        let scroll = match (previous, current) {
            (Some((a, n, w, c)), Some((b, m, x, d))) if (n, w, c) == (m, x, d) && a.abs_diff(b) < rows => {
                b as i64 - a as i64
            }
            _ => 0,
        };
        let dirty = if scroll > 0 {
            rows.saturating_sub(scroll as usize)..rows
        } else if scroll < 0 {
            0..scroll.unsigned_abs() as usize
        } else {
            0..rows
        };
        let notice = if self.view.compare.open { self.view.compare.header() } else { String::new() };
        self.view.set_notice(notice);
        self.apply(Damage { full: !self.view.compare.open, scroll, rows: dirty.collect(), status: true });
    }
    pub(super) fn dispatch_compare(&mut self, command: &str) -> bool {
        if !command.starts_with("compare.") {
            return false;
        }
        let proxy = self.proxy.clone();
        self.view.compare.set_wake(Arc::new(move || {
            let _ = proxy.send_event(UserEvent::Notice(String::new()));
        }));
        self.close_search();
        let was_open = self.view.compare.open;
        self.view.compare_command(command);
        self.drag = None;
        self.selecting = false;
        self.sync_file_ui();
        self.apply(Damage {
            full: !was_open || !self.view.compare.open || command == "compare.files",
            status: true,
            ..Default::default()
        });
        true
    }
    pub(super) fn compare_key(&mut self, key: &HostKey<'_>) -> bool {
        if !self.view.compare.open
            || self.palette_open()
            || self.search_panel.is_open()
            || self.settings_open()
            || self.menu.state.is_open()
        {
            return false;
        }
        if (self.mods.control_key() || self.mods.super_key())
            && matches!(&key.logical_key, Key::Character(c) if c.eq_ignore_ascii_case("p"))
        {
            return false;
        }
        if let Some(chord) = input::chord(&key.logical_key, self.mods)
            && let Some(keys) = &mut self.keys
            && let kwil_core_commands::vim::VimResult::Command(call) = keys.feed_with_selection(chord)
            && self.dispatch_compare(call.id)
        {
            return true;
        }
        match &key.logical_key {
            Key::Named(NamedKey::Escape) => self.view.compare.close(),
            Key::Named(NamedKey::F7) => {
                self.view.compare.action(if self.mods.shift_key() { Action::Previous } else { Action::Next })
            }
            Key::Named(NamedKey::ArrowDown) => self.view.compare.action(Action::Scroll(1)),
            Key::Named(NamedKey::ArrowUp) => self.view.compare.action(Action::Scroll(-1)),
            Key::Named(NamedKey::PageDown) => {
                self.view.compare.action(Action::Scroll(self.view.rows().saturating_sub(2) as i64))
            }
            Key::Named(NamedKey::PageUp) => {
                self.view.compare.action(Action::Scroll(-(self.view.rows().saturating_sub(2) as i64)))
            }
            _ => {}
        }
        self.apply(Damage { full: !self.view.compare.open, status: true, ..Default::default() });
        true
    }
}
pub(super) fn draw(
    session: &Session,
    pixels: &mut [u32],
    width: usize,
    font: &mut Font,
    dirty: impl Fn(usize) -> bool,
) {
    if width == 0 {
        return;
    }
    let rows = pixels.len() / width / font.ch;
    for row in (0..rows).filter(|&row| dirty(row)) {
        fill(pixels, width, Rect { x: 0, y: row * font.ch, w: width, h: font.ch }, bg());
    }
    let Some(page) = &session.page else {
        if dirty(0) {
            draw_text(pixels, width, font, 0, 0, session.header().as_bytes(), width / font.cw, gutter_fg(), bg());
        }
        return;
    };
    let half = width / 2;
    for (index, row) in page.rows.iter().take(rows).enumerate().filter(|(index, _)| dirty(*index)) {
        let y = index * font.ch;
        if row.hidden > 0 {
            draw_text(
                pixels,
                width,
                font,
                0,
                y,
                format!("… {} unchanged lines …", row.hidden).as_bytes(),
                width / font.cw,
                gutter_fg(),
                bg(),
            );
            continue;
        }
        for side in 0..2 {
            let x = side * half;
            let light = bg() > 0x808080;
            let color = match (row.kind, side) {
                (Kind::Delete | Kind::Modify, 0) if light => 0xf9dde1,
                (Kind::Insert | Kind::Modify, 1) if light => 0xdff2e2,
                (Kind::Delete | Kind::Modify, 0) => 0x38252b,
                (Kind::Insert | Kind::Modify, 1) => 0x20392e,
                _ => bg(),
            };
            fill(pixels, width, Rect { x, y, w: half, h: font.ch }, color);
            let number = if side == 0 { row.left } else { row.right };
            let label = number.map_or_else(|| "      ".into(), |n| format!("{:>5} ", n + 1));
            draw_text(pixels, width, font, x, y, label.as_bytes(), half / font.cw, gutter_fg(), color);
            let gutter = 6 * font.cw;
            let columns = half.saturating_sub(gutter + font.cw) / font.cw;
            draw_text(pixels, width, font, x + gutter, y, &row.text[side], columns, fg(), color);
            for cell in text::cells(&row.text[side], columns, font.tab_size()) {
                if row.words[side].iter().any(|r| r.start < cell.bytes.end && cell.bytes.start < r.end) {
                    let word_bg = match (light, side) {
                        (true, 0) => 0xf0a9b7,
                        (true, _) => 0xade1b7,
                        (false, 0) => 0x783947,
                        _ => 0x326748,
                    };
                    let cx = x + gutter + cell.column * font.cw;
                    fill(pixels, width, Rect { x: cx, y, w: cell.width * font.cw, h: font.ch }, word_bg);
                    draw_text(pixels, width, font, cx, y, cell.text.as_bytes(), cell.width, fg(), word_bg);
                }
            }
        }
        fill(pixels, width, Rect { x: half, y, w: 1, h: font.ch }, gutter_fg());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compare_gui_prompt_keeps_tab_and_body_is_read_only() {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "kwil-gui-compare-{}-{}.txt",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::write(&path, b"disk\n").unwrap();
        let (mut app, events) = crate::search_ui::host_dispatch_tests::host("buffer\n");
        let document = app.view.active_id();
        let revision = app.view.editor().unwrap().document().revision();
        app.palette_action(kwil_core_commands::palette::PaletteAction::Command("compare.files"));
        app.view.file_prompt_mut().unwrap().text = path.to_string_lossy().into();
        app.view.submit_file_prompt().unwrap();
        app.sync_file_ui();
        events.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        app.compare_ready();
        assert_eq!(app.view.active_id(), document);
        assert_eq!(app.view.compare.page.as_ref().unwrap().changes, 1);
        app.edit(input::Action::Text("must not insert".into()));
        app.palette_action(kwil_core_commands::palette::PaletteAction::Command("edit.deleteLine"));
        assert_eq!(app.view.editor().unwrap().document().revision(), revision);
        app.dispatch_compare("compare.close");
        events.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        app.compare_ready();
        assert!(!app.view.compare.open);
        std::fs::remove_file(path).unwrap();
    }
}
