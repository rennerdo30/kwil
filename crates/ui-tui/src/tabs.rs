use super::*;
use kwil_core_commands::settings::TabLayout;
use kwil_view_model::{TabArea, TabGeometry, TabMetrics};

pub(super) fn geometry(workspace: &Workspace, width: u16, height: u16, layout: TabLayout) -> TabGeometry {
    workspace.tab_geometry(
        TabMetrics { width: width as usize, height: height as usize, cell_width: 1, cell_height: 1, scrollbar: 0 },
        layout,
    )
}

fn rect(area: TabArea, origin: Rect) -> Rect {
    Rect {
        x: origin.x + area.x as u16,
        y: origin.y + area.y as u16,
        width: area.width as u16,
        height: area.height as u16,
    }
}

pub(super) fn draw(frame: &mut ratatui::Frame, workspace: &Workspace, layout: TabLayout) {
    let area = frame.area();
    let geometry = geometry(workspace, area.width, area.height, layout);
    super::draw_area(frame, workspace, rect(geometry.body, area), false);
    let idle = Color::Rgb(0x10, 0x1d, 0x31);
    let foreground = Color::Rgb(0xd6, 0xe4, 0xf0);
    frame.render_widget(Paragraph::new("").style(Style::default().bg(idle)), rect(geometry.chrome, area));
    for slot in &geometry.slots {
        let bg = if slot.active { Color::Rgb(0x19, 0x2a, 0x43) } else { idle };
        let style = Style::default().fg(foreground).bg(bg);
        frame.render_widget(Paragraph::new("").style(style), rect(slot.area, area));
        let title = TabArea { width: slot.close.x - slot.area.x, ..slot.area };
        frame.render_widget(Paragraph::new(workspace.tab_title(slot.document)).style(style), rect(title, area));
        frame.render_widget(Paragraph::new("[x]").style(style), rect(slot.close, area));
    }
    for (button, enabled, label) in [
        (geometry.previous, geometry.first > 0, "<"),
        (geometry.next, geometry.first + geometry.capacity < geometry.total, ">"),
    ] {
        if enabled {
            frame.render_widget(
                Paragraph::new(label).style(Style::default().fg(foreground).bg(idle)),
                rect(button, area),
            );
        }
    }
    let status = Paragraph::new(workspace.status_text())
        .style(Style::default().fg(Color::Rgb(0x8f, 0xe3, 0xff)).bg(Color::Rgb(0x08, 0x0d, 0x18)));
    frame.render_widget(
        status,
        Rect { x: area.x, y: area.y + area.height.saturating_sub(1), width: area.width, height: 1 },
    );
}

pub(super) fn mouse(
    workspace: &mut Workspace,
    event: &crossterm::event::MouseEvent,
    width: u16,
    height: u16,
    layout: TabLayout,
) -> bool {
    let geometry = geometry(workspace, width, height, layout);
    if !geometry.chrome.contains(event.column as usize, event.row as usize) {
        return false;
    }
    match event.kind {
        MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
            if let Some(action) = geometry.hit(event.column as usize, event.row as usize)
                && let Err(error) = workspace.tab_action(action, &geometry)
            {
                workspace.set_notice(error.to_string());
            }
        }
        MouseEventKind::ScrollUp => {
            workspace.cycle(true);
        }
        MouseEventKind::ScrollDown => {
            workspace.cycle(false);
        }
        _ => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_tabs_and_mouse_actions_preserve_body_text_and_dirty_close_guards() {
        for layout in [TabLayout::Horizontal, TabLayout::Vertical] {
            let mut workspace = Workspace::new(View::new(None)).unwrap();
            workspace
                .execute("edit.insert", &CommandArgs { text: Some("body".into()), ..Default::default() }, 1)
                .unwrap();
            let first = workspace.active_id();
            workspace.file_command("file.new").unwrap();
            workspace.open_path(std::env::temp_dir().join("kwil-virtual-notes.txt")).unwrap();
            workspace.activate(first);
            let geometry = geometry(&workspace, 80, 8, layout);
            workspace.set_rows(geometry.body.height);
            let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 8)).unwrap();
            terminal.draw(|frame| draw(frame, &workspace, layout)).unwrap();
            let position = terminal.get_cursor_position().unwrap();
            assert_eq!(position.x, geometry.body.x as u16 + 12);
            assert_eq!(position.y, geometry.body.y as u16);
            let rendered: String = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect();
            assert!(rendered.contains("kwil-virtual-notes"));
            let close = geometry.slots.iter().find(|slot| slot.document == first).unwrap().close;
            let click = crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: close.x as u16,
                row: close.y as u16,
                modifiers: KeyModifiers::NONE,
            };
            assert!(mouse(&mut workspace, &click, 80, 8, layout));
            assert!(workspace.close_pending());
            assert!(!workspace.wants_exit());
            assert_eq!(workspace.view(first).unwrap().editor().unwrap().document().snapshot().len(), 4);
        }
    }
}
