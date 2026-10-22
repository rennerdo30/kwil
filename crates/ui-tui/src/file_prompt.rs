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
