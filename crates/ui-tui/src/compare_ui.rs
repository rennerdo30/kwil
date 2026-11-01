use super::*;
use kwil_view_model::compare_session::{Action, Kind};

pub(super) fn key(view: &mut Workspace, keys: &mut KeymapEngine, k: KeyEvent) -> KeyResult {
    if let Some(chord) = chord(k)
        && let VimResult::Command(call) = keys.feed_with_selection(chord)
        && view.compare_command(call.id)
    {
        return KeyResult::Changed(true);
    }
    match k.code {
        KeyCode::Esc => view.compare.close(),
        KeyCode::F(7) => {
            view.compare.action(if k.modifiers.contains(KeyModifiers::SHIFT) { Action::Previous } else { Action::Next })
        }
        KeyCode::Down => view.compare.action(Action::Scroll(1)),
        KeyCode::Up => view.compare.action(Action::Scroll(-1)),
        KeyCode::PageDown => view.compare.action(Action::Scroll(view.rows().saturating_sub(2) as i64)),
        KeyCode::PageUp => view.compare.action(Action::Scroll(-(view.rows().saturating_sub(2) as i64))),
        KeyCode::Enter | KeyCode::Char(' ') => view.compare.expand_row(0),
        _ => {}
    }
    KeyResult::Changed(true)
}
pub(super) fn mouse(
    view: &mut Workspace,
    m: &event::MouseEvent,
    size: Rect,
    layout: kwil_core_commands::settings::TabLayout,
) {
    let body = tabs::geometry(view, size.width, size.height, layout).body;
    if !body.contains(m.column as usize, m.row as usize) {
        return;
    }
    match m.kind {
        MouseEventKind::ScrollDown => view.compare.action(Action::Scroll(3)),
        MouseEventKind::ScrollUp => view.compare.action(Action::Scroll(-3)),
        MouseEventKind::Down(event::MouseButton::Left) if m.row as usize > body.y => {
            view.compare.expand_row(m.row as usize - body.y - 1);
        }
        _ => {}
    }
}
pub(super) fn draw(frame: &mut ratatui::Frame, view: &Workspace, layout: kwil_core_commands::settings::TabLayout) {
    let size = frame.area();
    let body = tabs::geometry(view, size.width, size.height, layout).body;
    let area = Rect::new(body.x as u16, body.y as u16, body.width as u16, body.height as u16);
    let base = Style::default().fg(Color::Rgb(214, 228, 240)).bg(Color::Rgb(16, 29, 49));
    frame.render_widget(Paragraph::new("").style(base), area);
    frame.render_widget(Paragraph::new(view.compare.header()).style(base), Rect { height: 1.min(area.height), ..area });
    let Some(page) = &view.compare.page else { return };
    let half = area.width / 2;
    for (index, row) in page.rows.iter().take(area.height.saturating_sub(1) as usize).enumerate() {
        let y = area.y + index as u16 + 1;
        if row.hidden > 0 {
            frame.render_widget(
                Paragraph::new(format!("… {} unchanged lines (click or Enter to expand)", row.hidden)).style(base),
                Rect::new(area.x, y, area.width, 1),
            );
            continue;
        }
        for side in 0..2 {
            let color = match (row.kind, side) {
                (Kind::Delete | Kind::Modify, 0) => Color::Rgb(56, 37, 43),
                (Kind::Insert | Kind::Modify, 1) => Color::Rgb(32, 57, 46),
                _ => Color::Rgb(16, 29, 49),
            };
            let number = if side == 0 { row.left } else { row.right };
            let mut spans =
                vec![ratatui::text::Span::raw(number.map_or_else(|| "      ".into(), |n| format!("{:>5} ", n + 1)))];
            for cell in kwil_view_model::grid::cells(&row.text[side], half.saturating_sub(6) as usize, 4) {
                let words = &row.words[side];
                let changed = words
                    .get(words.partition_point(|r| r.end <= cell.bytes.start))
                    .is_some_and(|r| r.start < cell.bytes.end);
                let style = if changed {
                    base.bg(if side == 0 { Color::Rgb(120, 57, 71) } else { Color::Rgb(50, 103, 72) })
                } else {
                    base.bg(color)
                };
                let text = if cell.text == "\t" { " ".repeat(cell.width) } else { cell.text };
                spans.push(ratatui::text::Span::styled(text, style));
            }
            frame.render_widget(
                Paragraph::new(ratatui::text::Line::from(spans)).style(base.bg(color)),
                Rect::new(area.x + side as u16 * half, y, half, 1),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compare_tui_command_paints_both_sides_and_rejects_edits() {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "kwil-tui-compare-{}-{}.txt",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let disk = (0..40).map(|n| format!("disk word {n}\n")).collect::<String>();
        let buffer = disk.replace("disk word 0\n", "buffer word 0\n").replace("disk word 30\n", "buffer word 30\n");
        std::fs::write(&path, disk).unwrap();
        let mut view = Workspace::new(View::new(None)).unwrap();
        view.execute("edit.insert", &CommandArgs { text: Some(buffer), ..Default::default() }, 1).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        view.compare.set_wake(std::sync::Arc::new(move || {
            let _ = tx.send(());
        }));
        let mut keys = KeymapEngine::new(Preset::VsCode, &[]).unwrap();
        let VimResult::Command(call) = keys.invoke_named("compare.files") else { panic!("compare command registered") };
        let KeyResult::File(id) = invoke(&mut view, call, false, Duration::ZERO, Preset::VsCode) else {
            panic!("frontend routes comparison")
        };
        assert!(view.compare_command(id));
        view.file_prompt_mut().unwrap().text = path.to_string_lossy().into();
        view.submit_file_prompt().unwrap();
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        view.poll_compare();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 12)).unwrap();
        terminal.draw(|frame| draw(frame, &view, Settings::default().tabs.layout)).unwrap();
        let painted: String = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect();
        assert!(painted.contains("buffer") && painted.contains("disk"));
        let buffer = terminal.backend().buffer();
        let word = buffer.content.iter().find(|cell| cell.symbol() == "b").unwrap();
        assert_eq!(word.bg, Color::Rgb(120, 57, 71), "changed words need a stronger background than the line");
        for (modifiers, expected) in [(KeyModifiers::NONE, 30), (KeyModifiers::SHIFT, 0)] {
            key(&mut view, &mut keys, KeyEvent::new(KeyCode::F(7), modifiers));
            rx.recv_timeout(Duration::from_secs(5)).unwrap();
            view.poll_compare();
            assert_eq!(view.compare.page.as_ref().unwrap().rows[0].left, Some(expected));
        }
        for (command, expected) in [("compare.nextChange", 30), ("compare.previousChange", 0)] {
            let VimResult::Command(call) = keys.invoke_named(command) else { panic!("compare command registered") };
            let KeyResult::File(id) = invoke(&mut view, call, false, Duration::ZERO, Preset::VsCode) else {
                panic!("compare command routed")
            };
            assert!(view.compare_command(id));
            rx.recv_timeout(Duration::from_secs(5)).unwrap();
            view.poll_compare();
            assert_eq!(view.compare.page.as_ref().unwrap().rows[0].left, Some(expected));
        }
        let hidden = view.compare.page.as_ref().unwrap().rows.iter().position(|r| r.hidden > 0).unwrap();
        let body = tabs::geometry(&view, 100, 12, Settings::default().tabs.layout).body;
        mouse(
            &mut view,
            &event::MouseEvent {
                kind: MouseEventKind::Down(event::MouseButton::Left),
                column: body.x as u16,
                row: (body.y + 1 + hidden) as u16,
                modifiers: KeyModifiers::NONE,
            },
            Rect::new(0, 0, 100, 12),
            Settings::default().tabs.layout,
        );
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        view.poll_compare();
        assert_eq!(view.compare.page.as_ref().unwrap().rows[hidden].hidden, 0);
        let revision = view.editor().unwrap().document().revision();
        let VimResult::Command(call) = keys.invoke_named("edit.deleteLine") else { panic!("edit command registered") };
        invoke(&mut view, call, false, Duration::ZERO, Preset::VsCode);
        assert_eq!(view.editor().unwrap().document().revision(), revision);
        view.compare.close();
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        view.poll_compare();
        std::fs::remove_file(path).unwrap();
    }
}
