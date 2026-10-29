use super::*;

// The binary choice uses the established palette/language prompt colours and
// centered rows. Drawing and hit testing share this clipped terminal geometry.
pub(super) fn choice_area(bounds: Rect) -> Rect {
    let width = bounds.width.min(64);
    let height = bounds.height.min(9);
    Rect { x: bounds.x + (bounds.width - width) / 2, y: bounds.y + (bounds.height - height) / 3, width, height }
}

pub(super) fn choice_hit(bounds: Rect, column: u16, row: u16) -> Option<usize> {
    let area = choice_area(bounds);
    if area.width < 4 || area.height < 6 || !area.contains((column, row).into()) {
        return None;
    }
    let index = row.checked_sub(area.y + 2)? as usize;
    (index < 3).then_some(index)
}

pub(super) fn draw_choices(frame: &mut ratatui::Frame, detail: &str, choices: Option<usize>, notice: &str) {
    let area = choice_area(frame.area());
    let style = Style::default().fg(Color::Rgb(0xd6, 0xe4, 0xf0)).bg(Color::Rgb(0x08, 0x0d, 0x18));
    frame.render_widget(ratatui::widgets::Clear, area);
    frame.render_widget(Paragraph::new("").style(style), area);
    if area.height < 6 || area.width < 4 {
        frame.render_widget(
            Paragraph::new(if choices.is_some() {
                "Binary open: H Hex / T Text / Esc Cancel"
            } else {
                "Preparing file..."
            })
            .style(style),
            area,
        );
        return;
    }
    frame.render_widget(Paragraph::new(detail).style(style), Rect { height: 1, ..area });
    if let Some(selected) = choices {
        for (index, label) in ["[H] Hex (default)", "[T] Text", "[C] Cancel"].into_iter().enumerate() {
            let background =
                if selected == index { Color::Rgb(0x19, 0x2a, 0x43) } else { Color::Rgb(0x08, 0x0d, 0x18) };
            frame.render_widget(
                Paragraph::new(format!("{} {label}", if selected == index { ">" } else { " " }))
                    .style(style.bg(background)),
                Rect { y: area.y + 2 + index as u16, height: 1, ..area },
            );
        }
    } else {
        frame.render_widget(
            Paragraph::new("Please wait; body input is paused").style(style),
            Rect { y: area.y + 2, height: 1, ..area },
        );
    }
    if area.height >= 8 {
        frame.render_widget(Paragraph::new(notice).style(style), Rect { y: area.bottom() - 3, height: 1, ..area });
    }
    frame.render_widget(
        Paragraph::new(if choices.is_some() {
            "Tab / arrows choose  Enter open  Esc cancel"
        } else {
            "Detecting or opening the selected representation..."
        })
        .style(style),
        Rect { y: area.bottom() - 1, height: 1, ..area },
    );
}

pub(super) fn key(view: &mut Workspace, key: KeyEvent) -> KeyResult {
    match key.code {
        KeyCode::Esc => {
            view.cancel_file_prompt();
            view.set_notice(String::new());
            return KeyResult::Cancel;
        }
        KeyCode::Enter => {
            if let Err(error) = view.submit_file_prompt() {
                view.set_notice(error.to_string());
            }
            return KeyResult::Changed(true);
        }
        KeyCode::Backspace => {
            view.file_prompt_mut().unwrap().text.pop();
        }
        KeyCode::Up | KeyCode::Down => {
            let prompt = view.file_prompt_mut().unwrap();
            let count = prompt.kind.choices().len();
            if count > 0 {
                let current = prompt.text.parse::<usize>().ok().unwrap_or(0);
                let number =
                    if key.code == KeyCode::Up { current.saturating_sub(1).max(1) } else { (current + 1).min(count) };
                prompt.text = number.to_string();
            }
        }
        KeyCode::Char(c)
            if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER) =>
        {
            view.file_prompt_mut().unwrap().text.push(c);
        }
        _ => {}
    }
    view.prompt_notice();
    KeyResult::Changed(true)
}

pub(super) fn draw_prompt(frame: &mut ratatui::Frame, workspace: &Workspace) {
    let Some(prompt) = workspace.file_prompt() else { return };
    let bounds = frame.area();
    let width = bounds.width.min(72);
    let choices = prompt.kind.choices();
    let height = bounds.height.min((choices.len() + 5).min(16) as u16).max(1);
    let area =
        Rect { x: bounds.x + (bounds.width - width) / 2, y: bounds.y + (bounds.height - height) / 3, width, height };
    let selected = prompt.text.parse::<usize>().ok().and_then(|n| n.checked_sub(1));
    let visible = height.saturating_sub(4) as usize;
    let first = selected.unwrap_or(0).saturating_sub(visible.saturating_sub(1));
    let mut lines =
        vec![ratatui::text::Line::from(prompt.kind.title()), ratatui::text::Line::from(format!("> {}", prompt.text))];
    lines.extend(choices.iter().enumerate().skip(first).take(visible).map(|(i, choice)| {
        ratatui::text::Line::from(format!("{} {}: {choice}", if selected == Some(i) { ">" } else { " " }, i + 1))
    }));
    lines.push(ratatui::text::Line::from("↑ ↓ or number / path  Enter: apply  Esc: cancel"));
    frame.render_widget(ratatui::widgets::Clear, area);
    frame.render_widget(Paragraph::new(lines).style(Style::default().fg(Color::White).bg(Color::Rgb(8, 13, 24))), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_commands_picker_keys_and_terminal_card_preserve_document_text() {
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        workspace.execute("edit.insert", &CommandArgs { text: Some("keep".into()), ..Default::default() }, 1).unwrap();
        workspace.file_command("file.setEncoding").unwrap();
        assert_eq!(workspace.file_prompt().unwrap().kind, kwil_view_model::FilePromptKind::SaveEncoding);
        key(&mut workspace, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(workspace.file_prompt().unwrap().text, "1");
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| draw_prompt(frame, &workspace)).unwrap();
        let text: String = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect();
        assert!(text.contains("Save with encoding") && text.contains("UTF-8"));
        key(&mut workspace, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(workspace.file_prompt().is_none());
        assert_eq!(workspace.editor().unwrap().document().snapshot().len(), 4);
    }

    #[test]
    fn close_save_blank_enter_keeps_unicode_prompt_and_escape_cancels_close() {
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        workspace.execute("edit.insert", &CommandArgs { text: Some("keep".into()), ..Default::default() }, 1).unwrap();
        workspace.request_window_close(false).unwrap();
        workspace.file_command("file.save").unwrap();
        key(&mut workspace, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(workspace.file_prompt().is_some());
        assert!(workspace.close_pending());
        key(&mut workspace, KeyEvent::new(KeyCode::Char('界'), KeyModifiers::NONE));
        assert_eq!(workspace.file_prompt().unwrap().text, "界");
        assert_eq!(workspace.editor().unwrap().document().snapshot().len(), 4);
        assert!(matches!(key(&mut workspace, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)), KeyResult::Cancel));
        assert!(!workspace.close_pending());
        assert!(workspace.file_prompt().is_none());
    }
}
