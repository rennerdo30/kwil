//! Terminal palette overlay. Rendering consumes only capped published rows.
use super::*;
use kwil_core_commands::palette::Palette;
use kwil_view_model::palette_session::{display_text, query_window};

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
