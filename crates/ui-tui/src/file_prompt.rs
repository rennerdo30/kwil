use super::*;

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

pub(super) fn draw(frame: &mut ratatui::Frame, workspace: &Workspace) {
    super::draw(frame, workspace);
    let area = frame.area();
    let status = Paragraph::new(workspace.status_text())
        .style(Style::default().fg(Color::Rgb(0x8f, 0xe3, 0xff)).bg(Color::Rgb(0x08, 0x0d, 0x18)));
    frame.render_widget(
        status,
        Rect { x: area.x, y: area.y + area.height.saturating_sub(1), width: area.width, height: 1 },
    );
}
