//! Terminal frontend.
//!
//! Same model as the GUI (`kwil_view_model::View`). Event-driven: one
//! thread blocks on terminal input and forwards events into a channel that
//! also carries worker results, so the UI thread sleeps until something
//! happens. ratatui writes only changed cells, so the TUI stays fast over
//! SSH.

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use kwil_core_commands::{Preset, keymap::KeymapEngine, settings::Settings, vim::VimResult};
use kwil_view_model::{CommandArgs, CommandStatus, Notify, View};
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::sync::mpsc;

enum Msg {
    Input(Event),
    IndexDone,
    Notice(String),
    InputClosed,
    Saved(std::io::Result<kwil_core_workspace::SavedSnapshot>),
    Settings(Box<Settings>),
}

pub fn run(mut view: View, on_first_frame: Box<dyn FnOnce(Notify)>) -> std::io::Result<()> {
    let (tx, rx) = mpsc::channel::<Msg>();
    if let Some(file) = view.file() {
        let t = tx.clone();
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
        file.index_in_background(threads, move |result| {
            let event = match result {
                Ok(()) => Msg::IndexDone,
                Err(e) => Msg::Notice(e.to_string()),
            };
            let _ = t.send(event);
        });
    }
    let mut terminal = ratatui::init();
    crossterm::execute!(
        std::io::stdout(),
        crossterm::event::EnableMouseCapture,
        crossterm::event::EnableBracketedPaste
    )?;
    {
        let t = tx.clone();
        std::thread::spawn(move || {
            loop {
                match event::read() {
                    Ok(ev) => {
                        if t.send(Msg::Input(ev)).is_err() {
                            break;
                        }
                    }
                    Err(_) => {
                        let _ = t.send(Msg::InputClosed);
                        break;
                    }
                }
            }
        });
    }

    let result = (|| -> std::io::Result<()> {
        let size = terminal.size()?;
        view.set_rows(size.height.saturating_sub(1) as usize);
        terminal.draw(|f| draw(f, &view))?;
        let t = tx.clone();
        on_first_frame(Box::new(move |msg| {
            let _ = t.send(Msg::Notice(msg));
        }));
        let settings_sender = tx.clone();
        std::thread::spawn(move || {
            let settings = std::env::current_exe()
                .ok()
                .and_then(|exe| Settings::path(&exe).ok())
                .and_then(|path| Settings::load(&path).ok())
                .map(|report| report.settings)
                .unwrap_or_default();
            let _ = settings_sender.send(Msg::Settings(Box::new(settings)));
        });
        let started = std::time::Instant::now();
        let mut closing = false;
        let mut save_path: Option<String> = None;
        let mut horizontal = 0u64;
        let mut settings = Settings::default();
        let mut keys = KeymapEngine::new(settings.keymap, &settings.bindings).expect("bundled keymap");
        while let Ok(msg) = rx.recv() {
            let changed = match msg {
                Msg::InputClosed => break,
                Msg::Settings(next) => {
                    if let Ok(next_keys) = KeymapEngine::new(next.keymap, &next.bindings) {
                        keys = next_keys;
                        settings = *next;
                        if let Some(editor) = view.editor_mut() {
                            editor.settings = settings.clone();
                        }
                    }
                    true
                }
                Msg::Saved(saved) => {
                    if let Some(editor) = view.editor_mut() {
                        match editor.finish_save(saved) {
                            Ok(result) => {
                                let notice = match result.status {
                                    CommandStatus::Message(message) => message,
                                    _ => "Saved".into(),
                                };
                                view.refresh_after_edit(result.damage);
                                if closing && view.editor().is_some_and(|editor| !editor.document().is_dirty()) {
                                    break;
                                }
                                view.set_notice(notice);
                            }
                            Err(error) => {
                                view.set_notice(format!("Save failed: {error}"));
                            }
                        }
                    }
                    true
                }
                Msg::IndexDone => {
                    view.index_ready();
                    if let Some(editor) = view.editor_mut() {
                        editor.settings = settings.clone();
                    }
                    true
                }
                Msg::Notice(n) => {
                    view.set_notice(n);
                    true
                }
                Msg::Input(Event::Resize(_, h)) => {
                    view.set_rows(h.saturating_sub(1) as usize);
                    true
                }
                Msg::Input(Event::Key(k)) if k.kind != KeyEventKind::Release => {
                    let intent = if save_path.is_some() {
                        path_key(&mut view, &mut save_path, k)
                    } else {
                        key(&mut view, k, started.elapsed(), &mut keys, settings.keymap)
                    };
                    match intent {
                        KeyResult::Quit => {
                            if view.editor().is_some_and(|editor| editor.document().is_dirty())
                                && !(closing && k.modifiers.contains(KeyModifiers::SHIFT))
                            {
                                closing = true;
                                view.set_notice(
                                    "Unsaved changes: Ctrl+S to save; Ctrl+Shift+Q to discard; Esc to cancel".into(),
                                );
                                true
                            } else {
                                break;
                            }
                        }
                        KeyResult::Save(path) => {
                            if path.is_none() && view.editor().is_some_and(|editor| editor.document().path().is_none())
                            {
                                save_path = Some(String::new());
                                view.set_notice("Save as:  (Enter a path, Enter to save, Esc to cancel)".into());
                            } else {
                                if let Some(editor) = view.editor_mut() {
                                    match editor.save_request(path) {
                                        Ok(request) => {
                                            let sender = tx.clone();
                                            std::thread::spawn(move || {
                                                let _ = sender
                                                    .send(Msg::Saved(kwil_core_workspace::save_snapshot(request)));
                                            });
                                            view.set_notice("Saving…".into());
                                        }
                                        Err(error) => {
                                            view.set_notice(error.to_string());
                                        }
                                    }
                                }
                            }
                            true
                        }
                        KeyResult::Cancel => {
                            closing = false;
                            view.set_notice(String::new());
                            true
                        }
                        KeyResult::Changed(c) => c,
                    }
                }
                Msg::Input(Event::Paste(text)) => {
                    if let Some(path) = &mut save_path {
                        if !text.chars().any(char::is_control) {
                            path.push_str(&text);
                        }
                        view.set_notice(format!("Save as: {path}  (Enter to save, Esc to cancel)"));
                        true
                    } else if keys.mode() == kwil_core_commands::Mode::Insert {
                        execute(&mut view, "edit.insert", Some(text), false, started.elapsed());
                        true
                    } else {
                        false
                    }
                }
                Msg::Input(Event::Mouse(m)) => match m.kind {
                    MouseEventKind::ScrollDown => view.scroll(3) != Default::default(),
                    MouseEventKind::ScrollUp => view.scroll(-3) != Default::default(),
                    _ => false,
                },
                Msg::Input(_) => false,
            };
            if changed {
                keep_visible(
                    &mut view,
                    &mut horizontal,
                    terminal.size()?.width.saturating_sub(8) as usize,
                    settings.editor.tab_size as usize,
                );
                // ratatui diffs against the previous frame and writes only
                // the changed cells.
                terminal.draw(|f| draw(f, &view))?;
            }
        }
        Ok(())
    })();

    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::DisableMouseCapture,
        crossterm::event::DisableBracketedPaste
    );
    ratatui::restore();
    result
}

enum KeyResult {
    Quit,
    Save(Option<std::path::PathBuf>),
    Cancel,
    Changed(bool),
}

fn path_key(view: &mut View, prompt: &mut Option<String>, key: KeyEvent) -> KeyResult {
    let Some(path) = prompt.as_mut() else {
        return KeyResult::Changed(false);
    };
    match key.code {
        KeyCode::Esc => {
            *prompt = None;
            view.set_notice(String::new());
            return KeyResult::Changed(true);
        }
        KeyCode::Enter if !path.trim().is_empty() => {
            return KeyResult::Save(prompt.take().map(Into::into));
        }
        KeyCode::Backspace => {
            path.pop();
        }
        KeyCode::Char(c)
            if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER) =>
        {
            path.push(c);
        }
        _ => {}
    }
    view.set_notice(format!("Save as: {path}  (Enter to save, Esc to cancel)"));
    KeyResult::Changed(true)
}

fn key(
    view: &mut View,
    k: KeyEvent,
    elapsed: std::time::Duration,
    keys: &mut KeymapEngine,
    preset: Preset,
) -> KeyResult {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    if let Some(chord) = chord(k) {
        match keys.feed_with_selection(chord) {
            VimResult::Pending => return KeyResult::Changed(false),
            VimResult::Command(call) => {
                if call.id == "file.save" {
                    return KeyResult::Save(None);
                }
                if call.id == "file.quit" {
                    return KeyResult::Quit;
                }
                if preset == Preset::Vim {
                    if let Some(editor) = view.editor_mut() {
                        match editor.execute_vim(&call) {
                            Ok(result) => {
                                view.refresh_after_edit(result.damage);
                            }
                            Err(error) => {
                                view.set_notice(error.to_string());
                            }
                        }
                    }
                    return KeyResult::Changed(true);
                }
                return KeyResult::Changed(execute(
                    view,
                    call.id,
                    None,
                    k.modifiers.contains(KeyModifiers::SHIFT),
                    elapsed,
                ));
            }
            VimResult::Error(error) => {
                view.set_notice(error);
                return KeyResult::Changed(true);
            }
            VimResult::Unbound => {
                if keys.mode() != kwil_core_commands::Mode::Insert {
                    return KeyResult::Changed(false);
                }
                if ctrl && !matches!(k.code, KeyCode::Char('q' | 'Q')) {
                    return KeyResult::Changed(false);
                }
            }
        }
    }
    let command = match k.code {
        KeyCode::Char('q' | 'Q') if ctrl => return KeyResult::Quit,
        KeyCode::Char('s' | 'S') if ctrl => return KeyResult::Save(None),
        KeyCode::Esc => return KeyResult::Cancel,
        KeyCode::Char('z' | 'Z') if ctrl && k.modifiers.contains(KeyModifiers::SHIFT) => "edit.redo",
        KeyCode::Char('z') if ctrl => "edit.undo",
        KeyCode::Char('y') if ctrl => "edit.redo",
        KeyCode::Char('a') if ctrl => "edit.selectAll",
        KeyCode::Down => "motion.down",
        KeyCode::Up => "motion.up",
        KeyCode::Left if ctrl => "motion.wordBackward",
        KeyCode::Right if ctrl => "motion.wordForward",
        KeyCode::Left => "motion.left",
        KeyCode::Right => "motion.right",
        KeyCode::PageDown => "motion.pageDown",
        KeyCode::PageUp => "motion.pageUp",
        KeyCode::End if ctrl => "motion.lastLine",
        KeyCode::Home if ctrl => "motion.firstLine",
        KeyCode::End => "motion.lineEnd",
        KeyCode::Home => "motion.smartHome",
        KeyCode::Enter => "edit.newline",
        KeyCode::Backspace if ctrl => "edit.deleteWordBackward",
        KeyCode::Delete if ctrl => "edit.deleteWordForward",
        KeyCode::Backspace => "edit.backspace",
        KeyCode::Delete => "edit.delete",
        KeyCode::Tab => "edit.indent",
        KeyCode::BackTab => "edit.outdent",
        KeyCode::Char(c) if !ctrl && !k.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::SUPER) => {
            return KeyResult::Changed(execute(view, "edit.insert", Some(c.to_string()), false, elapsed));
        }
        _ => return KeyResult::Changed(false),
    };
    KeyResult::Changed(execute(view, command, None, k.modifiers.contains(KeyModifiers::SHIFT), elapsed))
}

fn chord(event: KeyEvent) -> Option<kwil_core_commands::KeyChord> {
    use kwil_core_commands::{Key, KeyChord, Modifiers, NamedKey as N};
    let key = match event.code {
        KeyCode::Char(c) => Key::Character(c),
        KeyCode::F(n) => Key::Function(n),
        KeyCode::Enter => Key::Named(N::Enter),
        KeyCode::Esc => Key::Named(N::Escape),
        KeyCode::Tab | KeyCode::BackTab => Key::Named(N::Tab),
        KeyCode::Backspace => Key::Named(N::Backspace),
        KeyCode::Delete => Key::Named(N::Delete),
        KeyCode::Insert => Key::Named(N::Insert),
        KeyCode::Home => Key::Named(N::Home),
        KeyCode::End => Key::Named(N::End),
        KeyCode::PageUp => Key::Named(N::PageUp),
        KeyCode::PageDown => Key::Named(N::PageDown),
        KeyCode::Left => Key::Named(N::Left),
        KeyCode::Right => Key::Named(N::Right),
        KeyCode::Up => Key::Named(N::Up),
        KeyCode::Down => Key::Named(N::Down),
        _ => return None,
    };
    Some(KeyChord::new(
        key,
        Modifiers {
            ctrl: event.modifiers.contains(KeyModifiers::CONTROL),
            alt: event.modifiers.contains(KeyModifiers::ALT),
            shift: event.modifiers.contains(KeyModifiers::SHIFT) || event.code == KeyCode::BackTab,
            super_key: event.modifiers.contains(KeyModifiers::SUPER),
        },
    ))
}

fn execute(view: &mut View, id: &str, text: Option<String>, extend: bool, at: std::time::Duration) -> bool {
    match view.execute(id, &CommandArgs { text, extend, at, ..Default::default() }, 1) {
        Ok(result) => match result.status {
            CommandStatus::Message(message) | CommandStatus::NotImplemented(message) => {
                view.set_notice(message);
            }
            CommandStatus::IndexNotReady => {
                view.set_notice("Indexing: editing will be available when ready".into());
            }
            _ => {}
        },
        Err(error) => {
            view.set_notice(error.to_string());
        }
    }
    true
}

fn draw(f: &mut ratatui::Frame, view: &View) {
    let area = f.area();
    let gutter_style = Style::default().fg(Color::Rgb(0x5b, 0x6b, 0x85));
    for row in 0..view.rows().min(area.height.saturating_sub(1) as usize) {
        let y = area.y + row as u16;
        let Some(text) = view.line(row) else { continue };
        let tab = view.editor().map_or(4, |editor| editor.settings.editor.tab_size as usize);
        let cells = kwil_view_model::grid::cells(text, area.width.saturating_sub(8) as usize, tab);
        let source = view
            .editor()
            .and_then(|editor| editor.line_range(view.top() + row as u64).ok())
            .map_or(0, |range| range.start)
            + view.line_offset(row);
        let selections =
            view.editor().and_then(|editor| editor.selection_ranges(view.top() + row as u64).ok()).unwrap_or_default();
        let mut spans = vec![Span::styled(format!("{:>7} ", view.line_number(row)), gutter_style)];
        for cell in cells {
            let selected = selections.iter().any(|range| {
                range.start < source + cell.bytes.end as u64 && source + (cell.bytes.start as u64) < range.end
            });
            let style = if selected { Style::default().bg(Color::Rgb(0x30, 0x58, 0x80)) } else { Style::default() };
            spans.push(Span::styled(if cell.text == "\t" { " ".repeat(cell.width) } else { cell.text }, style));
        }
        let line = Line::from(spans);
        f.render_widget(Paragraph::new(line), Rect { x: area.x, y, width: area.width, height: 1 });
    }
    let status = Paragraph::new(view.status_text())
        .style(Style::default().fg(Color::Rgb(0x8f, 0xe3, 0xff)).bg(Color::Rgb(0x08, 0x0d, 0x18)));
    f.render_widget(
        status,
        Rect { x: area.x, y: area.y + area.height.saturating_sub(1), width: area.width, height: 1 },
    );
    if let Some(editor) = view.editor()
        && let Ok((row, byte)) = editor.byte_position(editor.primary_cursor().head)
        && row >= view.top()
        && row - view.top() < view.rows() as u64
    {
        let screen_row = (row - view.top()) as usize;
        let first = view.line_offset(screen_row);
        let cells = kwil_view_model::grid::cells(
            view.line(screen_row).unwrap_or_default(),
            area.width.saturating_sub(8) as usize,
            editor.settings.editor.tab_size as usize,
        );
        if byte >= first {
            let column = cells
                .iter()
                .find(|cell| byte - first < cell.bytes.end as u64)
                .map_or_else(|| cells.last().map_or(0, |cell| cell.column + cell.width), |cell| cell.column);
            if column + 8 < (area.width as usize) {
                f.set_cursor_position((area.x + 8 + column as u16, area.y + screen_row as u16));
            }
        }
    }
}

fn keep_visible(view: &mut View, horizontal: &mut u64, columns: usize, tab: usize) {
    let Some(editor) = view.editor() else {
        return;
    };
    let Ok((row, byte)) = editor.byte_position(editor.primary_cursor().head) else {
        return;
    };
    if row < view.top() || row - view.top() >= view.rows() as u64 {
        return;
    }
    let screen_row = (row - view.top()) as usize;
    let first = view.line_offset(screen_row);
    let cells = kwil_view_model::grid::cells(view.line(screen_row).unwrap_or_default(), columns, tab);
    let end = cells.last().map_or(0, |cell| cell.bytes.end as u64);
    let end_column = cells.last().map_or(0, |cell| cell.column + cell.width);
    if byte < first || byte > first + end || (byte == first + end && end_column >= columns) {
        *horizontal = byte;
        view.set_horizontal_window(byte, 64 << 10);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn untitled_save_prompt_keeps_text_out_of_document_and_provides_a_path() {
        let mut view = View::new(None);
        view.set_rows(3);
        let mut prompt = Some(String::new());
        for c in "日本語.txt".chars() {
            path_key(&mut view, &mut prompt, KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert!(
            matches!(path_key(&mut view,&mut prompt,KeyEvent::new(KeyCode::Enter,KeyModifiers::NONE)),KeyResult::Save(Some(path)) if path.as_path()==std::path::Path::new("日本語.txt"))
        );
        assert!(view.editor().unwrap().document().snapshot().is_empty());
        assert!(prompt.is_none());
    }

    #[test]
    fn rendered_tabs_wide_characters_selection_and_horizontal_caret_agree() {
        let mut view = View::new(None);
        view.set_rows(3);
        execute(&mut view, "edit.insert", Some("a\t界Z".into()), false, Duration::ZERO);
        let backend = ratatui::backend::TestBackend::new(30, 4);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &view)).unwrap();
        assert_eq!(terminal.get_cursor_position().unwrap().x, 15);
        let damage = view
            .editor_mut()
            .unwrap()
            .set_cursors(vec![kwil_view_model::Cursor { anchor: 0, head: 1, desired_column: None }], 0)
            .unwrap();
        view.refresh_after_edit(damage);
        terminal.draw(|frame| draw(frame, &view)).unwrap();
        assert_eq!(terminal.backend().buffer()[(8, 0)].bg, Color::Rgb(0x30, 0x58, 0x80));
        execute(&mut view, "edit.selectAll", None, false, Duration::from_secs(1));
        execute(&mut view, "edit.insert", Some("x".repeat(100)), false, Duration::from_secs(2));
        let mut horizontal = 0;
        keep_visible(&mut view, &mut horizontal, 22, 4);
        assert!(horizontal > 0);
        terminal.draw(|frame| draw(frame, &view)).unwrap();
        assert!(terminal.get_cursor_position().unwrap().x < 30);
    }

    fn press(view: &mut View, code: KeyCode, modifiers: KeyModifiers, seconds: u64) {
        let mut keys = KeymapEngine::new(Preset::VsCode, &[]).unwrap();
        key(view, KeyEvent::new(code, modifiers), Duration::from_secs(seconds), &mut keys, Preset::VsCode);
    }

    #[test]
    fn typing_space_newline_navigation_and_undo_edit_the_document() {
        let mut view = View::new(None);
        view.set_rows(8);
        press(&mut view, KeyCode::Char('a'), KeyModifiers::NONE, 0);
        press(&mut view, KeyCode::Char(' '), KeyModifiers::NONE, 0);
        press(&mut view, KeyCode::Char('b'), KeyModifiers::NONE, 0);
        assert_eq!(view.line(0), Some(b"a b".as_slice()));
        press(&mut view, KeyCode::Enter, KeyModifiers::NONE, 1);
        press(&mut view, KeyCode::Char('X'), KeyModifiers::NONE, 2);
        assert_eq!(view.line(1), Some(b"X".as_slice()));
        press(&mut view, KeyCode::Char('z'), KeyModifiers::CONTROL, 3);
        assert_eq!(view.line(1), Some(b"".as_slice()));
        press(&mut view, KeyCode::Char('y'), KeyModifiers::CONTROL, 4);
        assert_eq!(view.line(1), Some(b"X".as_slice()));
        press(&mut view, KeyCode::Left, KeyModifiers::SHIFT, 5);
        assert!(!view.editor().unwrap().primary_cursor().is_caret());
        press(&mut view, KeyCode::Char('Y'), KeyModifiers::NONE, 6);
        assert_eq!(view.line(1), Some(b"Y".as_slice()));
    }
}
