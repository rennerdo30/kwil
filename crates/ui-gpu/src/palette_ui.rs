//! Bounded command/file palette overlay. Matching and discovery live in the
//! shared host; this module only draws its capped immutable result snapshot.
use super::*;
use kwil_core_commands::palette::Palette;
use kwil_view_model::palette_session::{display_text, query_window};

pub(super) fn area(size: (usize, usize), cw: usize, ch: usize) -> Option<Rect> {
    let (cw, ch) = (cw.max(1), ch.max(1));
    let columns = (size.0 / cw).min(80);
    let rows = (size.1 / ch).min(14);
    if columns < 4 || rows < 3 {
        return None;
    }
    let (w, h) = (columns * cw, rows * ch);
    Some(Rect { x: (size.0 - w) / 2, y: ((size.1 - h) / 3 / ch) * ch, w, h })
}

pub(super) fn draw(palette: &Palette, pixels: &mut [u32], size: (usize, usize), font: &mut Font) -> Option<Rect> {
    if !palette.is_open() {
        return None;
    }
    let panel = area(size, font.cw, font.ch)?;
    let (cw, ch) = (font.cw, font.ch);
    let columns = panel.w / cw - 2;
    let rows = panel.h / ch;
    let visible = rows - 2;
    let selected = palette.selected().unwrap_or(0);
    let first = selected.saturating_sub(visible - 1);
    fill(pixels, size.0, panel, STATUS_BG);
    let query = query_window(palette, columns);
    draw_text(pixels, size.0, font, panel.x + cw, panel.y, query.text.as_bytes(), columns, STATUS_FG, STATUS_BG);
    fill(
        pixels,
        size.0,
        Rect { x: panel.x + cw + query.cursor * cw, y: panel.y + ch - 2.min(ch), w: cw, h: 1 },
        STATUS_FG,
    );
    for (row, item) in palette.rows().iter().enumerate().skip(first).take(visible) {
        let y = panel.y + (row - first + 1) * ch;
        let bg = if palette.selected() == Some(row) { tabs::ACTIVE } else { STATUS_BG };
        fill(pixels, size.0, Rect { x: panel.x, y, w: panel.w, h: ch }, bg);
        let title = display_text(&item.title);
        draw_text(pixels, size.0, font, panel.x + cw, y, title.as_bytes(), columns, FG, bg);
    }
    let footer = if palette.results_are_fresh() { "Enter: open   Esc: cancel" } else { "Searching...   Esc: cancel" };
    draw_text(
        pixels,
        size.0,
        font,
        panel.x + cw,
        panel.y + (rows - 1) * ch,
        footer.as_bytes(),
        columns,
        GUTTER_FG,
        STATUS_BG,
    );
    Some(panel)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn panel_is_inside_tiny_and_scaled_windows() {
        assert!(area((10, 10), 8, 16).is_none());
        for (size, cell) in [((800, 600), (8, 16)), ((3840, 2160), (24, 48)), ((33, 49), (8, 16))] {
            let rect = area(size, cell.0, cell.1).unwrap();
            assert!(rect.x + rect.w <= size.0 && rect.y + rect.h <= size.1);
            assert_eq!(rect.w % cell.0, 0);
            assert_eq!(rect.h % cell.1, 0);
        }
    }
}
