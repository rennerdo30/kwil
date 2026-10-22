//! Navigable settings panel; filesystem work belongs to SettingsSession.
use super::*;
use kwil_core_commands::settings_ui::SettingValue;
use kwil_view_model::palette_session::{display_text, text_window};
use kwil_view_model::settings_panels::{EditColumn, PanelInput};
use kwil_view_model::settings_session::{SettingsInput, SettingsSession};

fn value_text(value: &SettingValue) -> String {
    match value {
        SettingValue::Bool(value) => if *value { "On" } else { "Off" }.into(),
        SettingValue::Integer(value) => value.to_string(),
        SettingValue::Float(value) => (*value as f32).to_string(),
        SettingValue::Text(value) => display_text(value),
        SettingValue::Strings(values) => values.iter().map(|s| display_text(s)).collect::<Vec<_>>().join(", "),
        SettingValue::StringMap(values) => format!("{} entries", values.len()),
        SettingValue::Bindings(values) => format!("{} shortcuts", values.len()),
        SettingValue::Formatters(values) => format!("{} formatters", values.len()),
    }
}

pub(super) fn key(host: &mut SettingsSession, event: KeyEvent) -> bool {
    if host.panel().is_some() {
        if host.panel_is_capturing() {
            if let Some(chord) = super::chord(event) {
                let _ = host.panel_input(PanelInput::Capture(chord));
            }
            return true;
        }
        let text;
        let input = match event.code {
            KeyCode::Esc => Some(PanelInput::Cancel),
            KeyCode::Enter => Some(PanelInput::Accept),
            KeyCode::Down | KeyCode::Tab => Some(PanelInput::Next),
            KeyCode::Up | KeyCode::BackTab => Some(PanelInput::Previous),
            KeyCode::Backspace => Some(PanelInput::Backspace),
            KeyCode::Delete => Some(PanelInput::Delete),
            KeyCode::Left => Some(PanelInput::Left),
            KeyCode::Right => Some(PanelInput::Right),
            KeyCode::Home => Some(PanelInput::Home),
            KeyCode::End => Some(PanelInput::End),
            KeyCode::Char(c) if !event.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER) => {
                text = c.to_string();
                Some(PanelInput::Text(&text))
            }
            _ => None,
        };
        if let Some(input) = input {
            let _ = host.panel_input(input);
        }
        return true;
    }
    let editing = host.is_editing() || host.is_searching();
    let command = event.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER);
    if event.code == KeyCode::Tab && !host.is_editing() {
        host.cycle_category(event.modifiers.contains(KeyModifiers::SHIFT));
        return true;
    }
    if event.code == KeyCode::BackTab && !host.is_editing() {
        host.cycle_category(true);
        return true;
    }
    if command && matches!(event.code, KeyCode::Char('r' | 'R')) {
        let _ = host.adopt_external();
        return true;
    }
    let text;
    let input = match event.code {
        KeyCode::Esc => Some(SettingsInput::Cancel),
        KeyCode::Enter => Some(SettingsInput::Accept),
        KeyCode::Down if !host.is_editing() => Some(SettingsInput::Next),
        KeyCode::Up if !host.is_editing() => Some(SettingsInput::Previous),
        KeyCode::Backspace if editing => Some(SettingsInput::Backspace),
        KeyCode::Delete => Some(if editing { SettingsInput::Delete } else { SettingsInput::Reset }),
        KeyCode::Left if editing => Some(SettingsInput::Left),
        KeyCode::Right if editing => Some(SettingsInput::Right),
        KeyCode::Home if editing => Some(SettingsInput::Home),
        KeyCode::End if editing => Some(SettingsInput::End),
        KeyCode::Char('s' | 'S') if command => Some(SettingsInput::Save),
        KeyCode::Char('/') if !editing => Some(SettingsInput::Search),
        KeyCode::Char(c) if !command && !c.is_control() => {
            text = c.to_string();
            Some(SettingsInput::Text(&text))
        }
        _ => None,
    };
    if let Some(input) = input {
        let _ = host.input(input);
    }
    true
}

pub(super) fn draw(frame: &mut ratatui::Frame<'_>, host: &SettingsSession) {
    if !host.is_open() {
        return;
    }
    let screen = frame.area();
    let (width, height) = (screen.width.saturating_sub(4).min(110), screen.height.saturating_sub(2).min(32));
    if width < 12 || height < 7 {
        return;
    }
    let area =
        Rect { x: screen.x + (screen.width - width) / 2, y: screen.y + (screen.height - height) / 2, width, height };
    if host.panel().is_some() {
        draw_panel(frame, host, area);
        return;
    }
    let style = Style::default().fg(Color::Rgb(214, 228, 240)).bg(Color::Rgb(8, 13, 24));
    let columns = width as usize - 2;
    let mut caret = None;
    frame.render_widget(ratatui::widgets::Clear, area);
    frame.render_widget(Paragraph::new("").style(style), area);
    let mut line = |row: u16, text: String, selected| {
        let style = if selected { style.bg(Color::Rgb(25, 42, 67)) } else { style };
        frame.render_widget(
            Paragraph::new(display_text(&text)).style(style),
            Rect { x: area.x + 1, y: area.y + row, width: width - 2, height: 1 },
        );
    };
    line(0, "Settings".into(), false);
    let search = text_window(host.search_text(), host.input_cursor(), columns);
    line(1, search.text, true);
    if host.is_searching() {
        caret = Some((area.x + 1 + search.cursor as u16, area.y + 1));
    }
    let rows = host.rows();
    let visible = (height - 6) as usize;
    let first = host.selected().saturating_sub(visible - 1);
    for (index, row) in rows.iter().enumerate().skip(first).take(visible) {
        let value = if index == host.selected() && host.is_editing() {
            let prefix = format!("{}: ", row.spec.title);
            let prefix_cells = kwil_view_model::grid::cells(prefix.as_bytes(), columns, 4)
                .iter()
                .map(|cell| cell.width)
                .sum::<usize>();
            let available = columns.saturating_sub(prefix_cells);
            let input = text_window(host.input_text(), host.input_cursor(), available);
            caret = Some((
                area.x + 1 + (prefix_cells + input.cursor).min(columns - 1) as u16,
                area.y + (index - first + 2) as u16,
            ));
            input.text
        } else {
            value_text(&row.current)
        };
        line((index - first + 2) as u16, format!("{}: {}", row.spec.title, value), index == host.selected());
    }
    if let Some(row) = rows.get(host.selected()) {
        line(height - 4, row.spec.description.into(), false);
        line(height - 3, format!("Default: {}", value_text(&row.default)), false);
    }
    line(height - 2, host.status().into(), false);
    line(height - 1, "Enter: edit  /: search  Delete: reset  Ctrl+S: save  Esc: back".into(), false);
    if let Some(cursor) = caret {
        frame.set_cursor_position(cursor);
    }
}

fn draw_panel(frame: &mut ratatui::Frame<'_>, host: &SettingsSession, area: Rect) {
    let columns = area.width.saturating_sub(2) as usize;
    let visible = area.height.saturating_sub(6) as usize;
    let Ok(Some(summary)) = host.panel_window(0, 0) else { return };
    let first = summary.selection.saturating_sub(visible / 2).min(summary.total.saturating_sub(visible));
    let Ok(Some(window)) = host.panel_window(first, visible) else { return };
    let style = Style::default().fg(Color::Rgb(214, 228, 240)).bg(Color::Rgb(8, 13, 24));
    frame.render_widget(ratatui::widgets::Clear, area);
    let mut line = |row, text: String, selected| {
        frame.render_widget(
            Paragraph::new(display_text(&text)).style(if selected { style.bg(Color::Rgb(25, 42, 67)) } else { style }),
            Rect { x: area.x + 1, y: area.y + row, width: area.width - 2, height: 1 },
        );
    };
    line(0, format!("Settings > {}", window.title), false);
    let query = text_window(&window.query, host.panel_query_cursor(), columns.saturating_sub(2));
    let query_cursor = query.cursor;
    line(1, format!("/ {}", query.text), false);
    for (index, row) in window.rows.iter().enumerate() {
        line(index as u16 + 2, format!("{}: {}  {}", row.title, row.value, row.source), row.selected);
    }
    let mut caret = Some((area.x + 3 + query_cursor as u16, area.y + 1));
    if let Some(edit) = &window.edit_text {
        let (label, text) = match edit.column {
            EditColumn::Key => ("Key: ", &edit.key),
            EditColumn::Value => ("Value: ", &edit.value),
        };
        let input = text_window(text, edit.cursor, columns.saturating_sub(label.len()));
        line(area.height - 4, format!("{label}{}", input.text), true);
        line(area.height - 3, format!("{} -> {}", edit.key, edit.value), false);
        caret = Some((area.x + 1 + (label.len() + input.cursor).min(columns - 1) as u16, area.y + area.height - 4));
    } else if let Some(capture) = &window.capture {
        line(area.height - 4, format!("Keys: {}", if capture.is_empty() { "Press keys..." } else { capture }), true);
        line(area.height - 3, "Enter accepts a captured chord; Esc cancels".into(), false);
    } else if !window.confirmation.is_empty() {
        line(
            area.height - 4,
            format!("Conflicts: {}  Enter move binding  Esc cancel", window.confirmation.len()),
            true,
        );
    } else if let Some(row) = window.rows.iter().find(|row| row.selected) {
        line(area.height - 4, row.description.clone(), false);
        line(area.height - 3, format!("Default: {}", row.default), false);
    }
    line(area.height - 2, host.status().into(), false);
    line(area.height - 1, "Enter edit/apply  Delete reset/remove  Esc back".into(), false);
    if let Some(caret) = caret {
        frame.set_cursor_position(caret);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, time::Duration};

    #[test]
    fn terminal_compound_capture_keeps_modified_escape_and_cancels_in_stages() {
        let directory = std::env::temp_dir().join(format!(
            "kwil-panel-tui-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("settings.toml");
        std::fs::write(&path, "").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let mut host = SettingsSession::start(
            path,
            Arc::new(move || {
                let _ = tx.send(());
            }),
        )
        .unwrap();
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        host.worker_ready();
        host.take_applied();
        host.open();
        host.input(SettingsInput::Search).unwrap();
        host.input(SettingsInput::Text("bindings")).unwrap();
        host.input(SettingsInput::Accept).unwrap();
        host.input(SettingsInput::Accept).unwrap();
        assert!(host.panel().is_some());
        key(&mut host, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(host.panel_is_capturing());
        key(&mut host, KeyEvent::new(KeyCode::Esc, KeyModifiers::SHIFT));
        assert!(host.panel_is_capturing(), "modified Escape is data during capture");
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 25)).unwrap();
        terminal.draw(|frame| draw(frame, &host)).unwrap();
        let screen: String = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect();
        assert!(screen.contains("shift+escape"), "captured chord must be visible");
        key(&mut host, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!host.panel_is_capturing());
        assert!(host.panel().is_some(), "first Escape cancels the capture only");
        key(&mut host, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(host.panel().is_none());
        assert!(host.is_open(), "second Escape returns to settings, not the document");
        host.stop().shutdown().unwrap();
        drop(host);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn terminal_settings_keyboard_edits_persist_and_render_without_replacing_search() {
        let directory = std::env::temp_dir().join(format!(
            "kwil-settings-tui-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("settings.toml");
        std::fs::write(&path, "[tabs]\nlayout = 'horizontal'\n").unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let mut host = SettingsSession::start(
            path.clone(),
            Arc::new(move || {
                let _ = sender.send(());
            }),
        )
        .unwrap();
        receiver.recv_timeout(Duration::from_secs(5)).unwrap();
        host.worker_ready();
        host.take_applied();
        host.open();
        key(&mut host, KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        for c in "tabs.layout".chars() {
            key(&mut host, KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        key(&mut host, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        key(&mut host, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let wake = receiver.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()));
            assert!(wake.is_ok(), "assertion failed: accepted settings edit must wake after a persisted save");
            host.worker_ready();
            if let Some(applied) = host.take_applied() {
                assert_eq!(applied.settings.tabs.layout, kwil_core_commands::settings::TabLayout::Vertical);
                break;
            }
        }
        assert!(std::fs::read_to_string(&path).unwrap().contains("vertical"));
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 25)).unwrap();
        terminal.draw(|frame| draw(frame, &host)).unwrap();
        let screen: String = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect();
        assert!(screen.contains("Settings"));
        assert!(screen.contains("tabs.layout"));
        assert!(screen.contains("vertical"));
        assert!(screen.contains("Default:"));
        host.input(SettingsInput::Search).unwrap();
        host.input(SettingsInput::Text(&format!("{}TAIL", "a".repeat(200)))).unwrap();
        terminal.draw(|frame| draw(frame, &host)).unwrap();
        let screen: String = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect();
        assert!(screen.contains("TAIL"), "the end of a long settings query must remain visible");
        assert!(terminal.backend().cursor_visible());
        assert_eq!(terminal.get_cursor_position().unwrap(), ratatui::layout::Position::new(96, 2));
        host.input(SettingsInput::Cancel).unwrap();
        host.input(SettingsInput::Search).unwrap();
        host.input(SettingsInput::Text("font.family")).unwrap();
        host.input(SettingsInput::Accept).unwrap();
        host.input(SettingsInput::Accept).unwrap();
        host.input(SettingsInput::End).unwrap();
        host.input(SettingsInput::Text(&format!("{}VALUE_END", "b".repeat(200)))).unwrap();
        terminal.draw(|frame| draw(frame, &host)).unwrap();
        let screen: String = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect();
        assert!(screen.contains("VALUE_END"), "the end of a settings value must remain visible");
        assert!(terminal.backend().cursor_visible());
        assert_eq!(terminal.get_cursor_position().unwrap(), ratatui::layout::Position::new(96, 3));
        host.input(SettingsInput::Cancel).unwrap();
        host.stop().shutdown().unwrap();
        drop(host);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
