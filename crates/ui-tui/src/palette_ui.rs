//! Terminal palette overlay. Rendering consumes only capped published rows.
use super::*;
use kwil_core_commands::palette::Palette;
use kwil_view_model::palette_session::{PaletteInput, PaletteSession};
use kwil_view_model::palette_session::{display_text, query_window};
use std::time::Duration;

pub(super) fn command_query(id: &str) -> Option<&'static str> {
    match id {
        "view.commandPalette" => Some(">"),
        "view.fileFinder" => Some(""),
        "navigation.goToLine" => Some(":"),
        _ => None,
    }
}

pub(super) fn open(view: &mut Workspace, host: Option<&mut PaletteSession>, keys: &mut KeymapEngine, query: &str) {
    if !view.recovery_ready() {
        view.set_notice("Workspace recovery is not ready; try again shortly".into());
        return;
    }
    let Some(host) = host else {
        view.set_notice("Palette is starting; try again shortly".into());
        return;
    };
    let tabs = view
        .session()
        .tabs()
        .iter()
        .map(|tab| kwil_core_commands::palette::Tab { id: tab.id.0, title: view.tab_title(tab.id) })
        .collect();
    match host.set_tabs(tabs).and_then(|()| host.open(query)) {
        Ok(()) => keys.set_context(kwil_core_commands::Context::Palette),
        Err(error) => {
            view.set_notice(format!("Palette: {error:?}"));
        }
    }
}

pub(super) fn input(
    view: &mut Workspace,
    host: &mut PaletteSession,
    keys: &mut KeymapEngine,
    value: PaletteInput<'_>,
    elapsed: Duration,
    preset: Preset,
) -> KeyResult {
    let result = host.input(value);
    if !host.model().is_open() {
        keys.set_context(kwil_core_commands::Context::Editor);
    }
    use kwil_core_commands::palette::PaletteAction;
    match result {
        Ok(Some(PaletteAction::Command(id))) => match keys.invoke_named(id) {
            VimResult::Command(call) => invoke(view, call, false, elapsed, preset),
            VimResult::Error(error) => {
                view.set_notice(error);
                KeyResult::Changed(true)
            }
            _ => KeyResult::Changed(true),
        },
        Ok(Some(PaletteAction::OpenFile(path))) => {
            if let Err(error) = view.open_path(path) {
                view.set_notice(error.to_string());
            }
            KeyResult::Changed(true)
        }
        Ok(Some(PaletteAction::ActivateTab(id))) => {
            if !view.activate(kwil_view_model::SessionDocumentId(id)) {
                view.set_notice("Palette tab was closed".into());
            }
            KeyResult::Changed(true)
        }
        Ok(Some(PaletteAction::GoTo { line, column })) => {
            match view.execute(
                "navigation.goToLine",
                &CommandArgs { line: Some(line), column: Some(column), ..Default::default() },
                1,
            ) {
                Ok(result) => {
                    view.refresh_after_edit(result.damage);
                    if let CommandStatus::Message(message) | CommandStatus::NotImplemented(message) = result.status {
                        view.set_notice(message);
                    }
                }
                Err(error) => {
                    view.set_notice(error.to_string());
                }
            }
            KeyResult::Changed(true)
        }
        Ok(None) => KeyResult::Changed(true),
        Err(error) => {
            view.set_notice(format!("Palette: {error:?}"));
            KeyResult::Changed(true)
        }
    }
}

pub(super) fn key(
    view: &mut Workspace,
    host: &mut PaletteSession,
    event: KeyEvent,
    keys: &mut KeymapEngine,
    elapsed: Duration,
    preset: Preset,
) -> KeyResult {
    keys.set_context(kwil_core_commands::Context::Palette);
    if let Some(chord) = chord(event) {
        match keys.feed(chord) {
            VimResult::Pending => return KeyResult::Changed(false),
            VimResult::Error(error) => {
                view.set_notice(error);
                return KeyResult::Changed(true);
            }
            VimResult::Unbound if keys.suppresses_fallback() => return KeyResult::Changed(false),
            VimResult::Command(call) => {
                let value = match call.id {
                    "palette.accept" => Some(PaletteInput::Accept),
                    "palette.cancel" => Some(PaletteInput::Cancel),
                    "palette.next" => Some(PaletteInput::Next),
                    "palette.previous" => Some(PaletteInput::Previous),
                    "edit.backspace" => Some(PaletteInput::Backspace),
                    "edit.delete" => Some(PaletteInput::Delete),
                    "motion.left" => Some(PaletteInput::Left),
                    "motion.right" => Some(PaletteInput::Right),
                    "motion.lineStart" | "motion.smartHome" => Some(PaletteInput::Home),
                    "motion.lineEnd" => Some(PaletteInput::End),
                    _ => None,
                };
                if let Some(value) = value {
                    return input(view, host, keys, value, elapsed, preset);
                }
                if call.id.starts_with("edit.") || call.id.starts_with("motion.") {
                    view.set_notice("This editor shortcut is unavailable in the palette query".into());
                    return KeyResult::Changed(true);
                }
                let _ = host.input(PaletteInput::Cancel);
                keys.set_context(kwil_core_commands::Context::Editor);
                return match keys.invoke_named(call.id) {
                    VimResult::Command(call) => invoke(view, call, false, elapsed, preset),
                    VimResult::Error(error) => {
                        view.set_notice(error);
                        KeyResult::Changed(true)
                    }
                    _ => KeyResult::Changed(true),
                };
            }
            VimResult::Unbound => {}
        }
    }
    let value = match event.code {
        KeyCode::Backspace => PaletteInput::Backspace,
        KeyCode::Delete => PaletteInput::Delete,
        KeyCode::Left => PaletteInput::Left,
        KeyCode::Right => PaletteInput::Right,
        KeyCode::Home => PaletteInput::Home,
        KeyCode::End => PaletteInput::End,
        KeyCode::Char(c)
            if !event.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
                && !c.is_control() =>
        {
            let text = c.to_string();
            return input(view, host, keys, PaletteInput::Text(&text), elapsed, preset);
        }
        _ => return KeyResult::Changed(false),
    };
    input(view, host, keys, value, elapsed, preset)
}

pub(super) fn mouse(
    view: &mut Workspace,
    host: &mut PaletteSession,
    keys: &mut KeymapEngine,
    event: &event::MouseEvent,
    bounds: Rect,
) -> bool {
    let value = match event.kind {
        MouseEventKind::ScrollDown => PaletteInput::Next,
        MouseEventKind::ScrollUp => PaletteInput::Previous,
        MouseEventKind::Down(_)
            if panel(bounds).is_none_or(|area| !area.contains((event.column, event.row).into())) =>
        {
            PaletteInput::Cancel
        }
        _ => return false,
    };
    let _ = input(view, host, keys, value, Duration::ZERO, Preset::VsCode);
    true
}

pub(super) fn sync_input_identity(
    view: &Workspace,
    keys: &mut KeymapEngine,
    previous: &mut (kwil_view_model::SessionDocumentId, u64),
) {
    let current = (view.active_id(), view.input_epoch());
    if current != *previous {
        keys.cancel_pending();
        *previous = current;
    }
}

pub(super) fn panel(area: Rect) -> Option<Rect> {
    let width = area.width.min(80);
    let height = area.height.min(14);
    if width < 4 || height < 3 {
        return None;
    }
    Some(Rect { x: area.x + (area.width - width) / 2, y: area.y + (area.height - height) / 3, width, height })
}

pub(super) fn draw(frame: &mut ratatui::Frame, palette: &Palette) {
    if !palette.is_open() {
        return;
    }
    let Some(area) = panel(frame.area()) else {
        return;
    };
    let bg = Color::Rgb(0x08, 0x0d, 0x18);
    let fg = Color::Rgb(0xd6, 0xe4, 0xf0);
    let style = Style::default().fg(fg).bg(bg);
    frame.render_widget(ratatui::widgets::Clear, area);
    frame.render_widget(Paragraph::new("").style(style), area);
    let columns = area.width as usize - 2;
    let query = query_window(palette, columns);
    frame.render_widget(
        Paragraph::new(query.text).style(style),
        Rect { x: area.x + 1, y: area.y, width: columns as u16, height: 1 },
    );
    let visible = area.height as usize - 2;
    let first = palette.selected().unwrap_or(0).saturating_sub(visible - 1);
    for (row, item) in palette.rows().iter().enumerate().skip(first).take(visible) {
        let selected = palette.selected() == Some(row);
        let bg = if selected { Color::Rgb(0x19, 0x2a, 0x43) } else { bg };
        let row_area = Rect { x: area.x, y: area.y + (row - first + 1) as u16, width: area.width, height: 1 };
        frame.render_widget(Paragraph::new("").style(style.bg(bg)), row_area);
        frame.render_widget(
            Paragraph::new(display_text(&item.title)).style(style.bg(bg)),
            Rect { x: area.x + 1, width: columns as u16, ..row_area },
        );
    }
    let footer = if palette.results_are_fresh() { "Enter: open   Esc: cancel" } else { "Searching...   Esc: cancel" };
    frame.render_widget(
        Paragraph::new(footer).style(style),
        Rect { x: area.x + 1, y: area.y + area.height - 1, width: columns as u16, height: 1 },
    );
    frame.set_cursor_position((area.x + 1 + query.cursor as u16, area.y));
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn named_vim_command_clears_old_operator_and_changes_mode() {
        let mut keys = KeymapEngine::new(Preset::Vim, &[]).unwrap();
        assert!(matches!(keys.feed("d".parse().unwrap()), VimResult::Pending));
        assert!(matches!(keys.invoke_named("mode.insert"), VimResult::Command(call) if call.id == "mode.insert"));
        assert_eq!(keys.mode(), kwil_core_commands::Mode::Insert);
        assert!(matches!(keys.feed("w".parse().unwrap()), VimResult::Unbound));
    }

    #[test]
    fn palette_text_and_paste_cannot_mutate_document_in_vim_normal_mode() {
        let root = std::env::temp_dir().join(format!("kwil-palette-tui-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut host = PaletteSession::new(root.clone(), std::sync::Arc::new(|| {})).unwrap();
        let mut view = Workspace::new(View::new(None)).unwrap();
        let mut keys = KeymapEngine::new(Preset::Vim, &[]).unwrap();
        host.open(":").unwrap();
        key(
            &mut view,
            &mut host,
            KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE),
            &mut keys,
            Duration::ZERO,
            Preset::Vim,
        );
        input(&mut view, &mut host, &mut keys, PaletteInput::Text("3"), Duration::ZERO, Preset::Vim);
        assert_eq!(host.model().query(), ":23");
        assert_eq!(keys.mode(), kwil_core_commands::Mode::Normal);
        assert_eq!(view.editor().unwrap().document().snapshot().len(), 0);
        input(&mut view, &mut host, &mut keys, PaletteInput::Cancel, Duration::ZERO, Preset::Vim);
        assert!(!host.model().is_open());
        host.open(":").unwrap();
        keys.set_context(kwil_core_commands::Context::Palette);
        let outside = event::MouseEvent {
            kind: MouseEventKind::Down(event::MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        assert!(mouse(&mut view, &mut host, &mut keys, &outside, Rect::new(0, 0, 100, 30)));
        assert!(!host.model().is_open());
        assert!(matches!(keys.feed("i".parse().unwrap()), VimResult::Command(call) if call.id == "edit.insert"));
        assert_eq!(keys.mode(), kwil_core_commands::Mode::Insert);
        for handle in host.stop() {
            handle.join().unwrap();
        }
        std::fs::remove_dir(&root).unwrap();
    }
    #[test]
    fn panel_respects_nonzero_origins_and_small_terminals() {
        assert!(panel(Rect::new(0, 0, 3, 10)).is_none());
        assert!(panel(Rect::new(0, 0, 20, 2)).is_none());
        let bounds = Rect::new(10, 4, 100, 30);
        let rect = panel(bounds).unwrap();
        assert_eq!(rect.width, 80);
        assert_eq!(rect.height, 14);
        assert!(rect.x >= bounds.x && rect.y >= bounds.y);
        assert!(rect.right() <= bounds.right() && rect.bottom() <= bounds.bottom());
    }
}
