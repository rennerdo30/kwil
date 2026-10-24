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

#[cfg(test)]
mod tests {
    use super::*;

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
