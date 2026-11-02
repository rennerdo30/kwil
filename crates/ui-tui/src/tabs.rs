use super::*;
use kwil_core_commands::settings::TabLayout;
use kwil_view_model::{TabArea, TabGeometry, TabMetrics};

pub(super) fn geometry(workspace: &Workspace, width: u16, height: u16, layout: TabLayout) -> TabGeometry {
    let mut geometry = workspace.tab_geometry(
        TabMetrics { width: width as usize, height: height as usize, cell_width: 1, cell_height: 1, scrollbar: 0 },
        layout,
    );
    if layout == TabLayout::Horizontal {
        // Retain the shared page capacity and body, but use terminal cell
        // widths to keep each marker beside its label and gaps outside hits.
        let mut x = geometry.previous.width;
        for slot in &mut geometry.slots {
            let gap = 2.min(slot.area.width.saturating_sub(4));
            let available = slot.area.width - gap;
            let close_width = slot.close.width.min(available);
            let separator = usize::from(available > close_width);
            let title_width = Line::from(workspace.tab_title(slot.document))
                .width()
                .min(available.saturating_sub(close_width + separator));
            slot.area.x = x;
            slot.area.width = title_width + separator + close_width;
            slot.close.x = x + title_width + separator;
            slot.close.width = close_width;
            x += slot.area.width + gap;
        }
    }
    geometry
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
    draw_highlighted(frame, workspace, layout, &[]);
}

pub(super) fn draw_highlighted(
    frame: &mut ratatui::Frame,
    workspace: &Workspace,
    layout: TabLayout,
    highlights: &[&kwil_view_model::highlighting::Span],
) {
    draw_prepared(frame, workspace, layout, highlights, false, None);
}

/// Chrome remains independent of body ownership. A wrapped/settling body must
/// never invoke the logical text painter or its source-derived status position.
pub(super) fn draw_prepared(
    frame: &mut ratatui::Frame,
    workspace: &Workspace,
    layout: TabLayout,
    highlights: &[&kwil_view_model::highlighting::Span],
    prepared_body: bool,
    prepared_status: Option<&str>,
) {
    let area = frame.area();
    let geometry = geometry(workspace, area.width, area.height, layout);
    if !prepared_body {
        super::draw_area_highlighted(frame, workspace, rect(geometry.body, area), false, highlights);
    }
    let idle = theme_ui::color(kwil_core_commands::theme::UiRole::TabBackground, Color::Rgb(0x10, 0x1d, 0x31));
    let foreground = theme_ui::color(kwil_core_commands::theme::UiRole::Foreground, Color::Rgb(0xd6, 0xe4, 0xf0));
    frame.render_widget(Paragraph::new("").style(Style::default().bg(idle)), rect(geometry.chrome, area));
    for slot in &geometry.slots {
        let bg = if slot.active {
            theme_ui::color(kwil_core_commands::theme::UiRole::ActiveTabBackground, Color::Rgb(0x19, 0x2a, 0x43))
        } else {
            idle
        };
        let style = Style::default().fg(foreground).bg(bg);
        frame.render_widget(Paragraph::new("").style(style), rect(slot.area, area));
        let title = TabArea {
            width: (slot.close.x - slot.area.x).saturating_sub(usize::from(layout == TabLayout::Horizontal)),
            ..slot.area
        };
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
    let status = if let Some(status) = prepared_status {
        status.to_owned()
    } else if workspace.hex_active() {
        hex_ui::workspace_status(workspace)
    } else {
        workspace.status_text()
    };
    let status = Paragraph::new(status).style(
        Style::default()
            .fg(theme_ui::color(kwil_core_commands::theme::UiRole::StatusForeground, Color::Rgb(0x8f, 0xe3, 0xff)))
            .bg(theme_ui::color(kwil_core_commands::theme::UiRole::PanelBackground, Color::Rgb(0x08, 0x0d, 0x18))),
    );
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

    fn tab_workspace(named: bool) -> Workspace {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        let first = workspace.active_id();
        workspace.execute("edit.insert", &CommandArgs { text: Some("body".into()), ..Default::default() }, 1).unwrap();
        let directory = std::env::temp_dir().join(format!(
            "kwil-tab-spacing-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        if named {
            workspace.open_path(directory.join("palette-target.txt")).unwrap();
        } else {
            workspace.open(View::new(None)).unwrap();
        }
        workspace.open(View::new(None)).unwrap();
        workspace.activate(first);
        workspace
    }

    #[test]
    fn tab_bar_text_keeps_markers_with_labels_and_preserves_active_dirty_and_truncation() {
        let workspace = tab_workspace(true);
        for (width, expected) in [
            (80, "  * Untitled 1 [x]  palette-target.txt [x]  Untitled 3 [x]"),
            (44, "  * Untitled 1 [x]  palette-target [x]    >"),
        ] {
            let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 8)).unwrap();
            terminal.draw(|frame| draw(frame, &workspace, TabLayout::Horizontal)).unwrap();
            let buffer = terminal.backend().buffer();
            let row: String = (0..width).map(|x| buffer[(x, 0)].symbol()).collect();
            assert_eq!(row, format!("{expected:<width$}", width = width as usize));
            assert_ne!(buffer[(2, 0)].bg, buffer[(20, 0)].bg, "active tab keeps its background");
        }
        assert_mouse_closes_the_document_beside_each_rendered_marker();
    }

    fn assert_mouse_closes_the_document_beside_each_rendered_marker() {
        for width in [80, 44] {
            let initial = tab_workspace(false);
            for column in [18, 19] {
                assert_eq!(geometry(&initial, width, 8, TabLayout::Horizontal).hit(column, 0), None);
            }
            let documents: Vec<_> =
                geometry(&initial, width, 8, TabLayout::Horizontal).slots.iter().map(|slot| slot.document).collect();
            for document in documents {
                let mut workspace = tab_workspace(false);
                let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 8)).unwrap();
                terminal.draw(|frame| draw(frame, &workspace, TabLayout::Horizontal)).unwrap();
                let row: String = (0..width).map(|x| terminal.backend().buffer()[(x, 0)].symbol()).collect();
                let slot_index = geometry(&workspace, width, 8, TabLayout::Horizontal)
                    .slots
                    .iter()
                    .position(|slot| slot.document == document)
                    .unwrap();
                let column = row.match_indices("[x]").nth(slot_index).unwrap().0;
                let click = crossterm::event::MouseEvent {
                    kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
                    column: column as u16 + 1,
                    row: 0,
                    modifiers: KeyModifiers::NONE,
                };
                let remaining: Vec<_> = geometry(&workspace, width, 8, TabLayout::Horizontal)
                    .slots
                    .iter()
                    .filter(|slot| slot.document != document)
                    .map(|slot| slot.document)
                    .collect();
                assert!(mouse(&mut workspace, &click, width, 8, TabLayout::Horizontal));
                if workspace.tab_dirty(document) {
                    assert!(workspace.close_pending());
                    assert!(workspace.view(document).is_some());
                } else {
                    assert!(workspace.view(document).is_none(), "clicked marker closes its document");
                }
                for other in remaining {
                    assert!(workspace.view(other).is_some(), "neighbouring tabs stay open");
                }
            }
        }
    }

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
