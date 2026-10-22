//! Navigable settings panel; filesystem work belongs to SettingsSession.
use super::*;
use kwil_core_commands::settings_ui::SettingValue;
use kwil_view_model::palette_session::display_text;
use kwil_view_model::settings_session::{SettingsInput, SettingsSession};

fn value_text(value: &SettingValue) -> String {
    match value {
        SettingValue::Bool(value) => if *value { "On" } else { "Off" }.into(),
        SettingValue::Integer(value) => value.to_string(),
        SettingValue::Float(value) => value.to_string(),
        SettingValue::Text(value) => display_text(value),
        SettingValue::Strings(values) => values.iter().map(|s| display_text(s)).collect::<Vec<_>>().join(", "),
        SettingValue::StringMap(values) => format!("{} entries", values.len()),
        SettingValue::Bindings(values) => format!("{} shortcuts", values.len()),
        SettingValue::Formatters(values) => format!("{} formatters", values.len()),
    }
}

pub(super) fn key(host: &mut SettingsSession, event: KeyEvent) -> bool {
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
    let style = Style::default().fg(Color::Rgb(214, 228, 240)).bg(Color::Rgb(8, 13, 24));
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
    line(1, host.search_text().into(), true);
    let rows = host.rows();
    let visible = (height - 6) as usize;
    let first = host.selected().saturating_sub(visible - 1);
    for (index, row) in rows.iter().enumerate().skip(first).take(visible) {
        let value = if index == host.selected() && host.is_editing() {
            host.input_text().to_owned()
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, time::Duration};

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
        host.stop().shutdown();
        drop(host);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
