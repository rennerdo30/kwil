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
        _ => {}
    }
    KeyResult::Changed(true)
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
                Paragraph::new(format!("… {} unchanged lines …", row.hidden)).style(base),
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
            let text = format!(
                "{}{}",
                number.map_or_else(|| "      ".into(), |n| format!("{:>5} ", n + 1)),
                kwil_view_model::grid::cells(&row.text[side], half.saturating_sub(6) as usize, 4)
                    .iter()
                    .map(|c| c.text.as_str())
                    .collect::<String>()
            );
            frame.render_widget(
                Paragraph::new(text).style(base.bg(color)),
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
        std::fs::write(&path, b"disk\n").unwrap();
        let mut view = Workspace::new(View::new(None)).unwrap();
        view.execute("edit.insert", &CommandArgs { text: Some("buffer\n".into()), ..Default::default() }, 1).unwrap();
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
