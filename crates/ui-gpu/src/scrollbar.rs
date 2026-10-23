//! Integer scrollbar geometry: document sizes never pass through f32.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bar {
    pub start: usize,
    pub length: usize,
    pub thumb_start: usize,
    pub thumb_length: usize,
    pub maximum: u64,
}

impl Bar {
    pub fn new(start: usize, length: usize, visible: u64, total: u64, offset: u64) -> Self {
        let maximum = total.saturating_sub(visible);
        let thumb_length = if total == 0 || maximum == 0 {
            length
        } else {
            ((length as u128 * visible as u128 / total as u128) as usize).max(18).min(length)
        };
        let travel = length - thumb_length;
        let thumb_start = start
            + if maximum == 0 { 0 } else { (travel as u128 * offset.min(maximum) as u128 / maximum as u128) as usize };
        Self { start, length, thumb_start, thumb_length, maximum }
    }

    pub fn contains_thumb(self, pixel: usize) -> bool {
        (self.thumb_start..self.thumb_start + self.thumb_length).contains(&pixel)
    }

    pub fn drag(self, pixel: usize, grab: usize) -> u64 {
        let travel = self.length - self.thumb_length;
        if travel == 0 {
            return 0;
        }
        let position = pixel.saturating_sub(self.start).saturating_sub(grab).min(travel);
        (position as u128 * self.maximum as u128 / travel as u128) as u64
    }

    pub fn page(self, pixel: usize, offset: u64, visible: u64) -> u64 {
        if pixel < self.thumb_start {
            offset.saturating_sub(visible)
        } else {
            offset.saturating_add(visible).min(self.maximum)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kwil_view_model::View;

    fn view(text: &str, rows: usize) -> View {
        let mut view = View::new(None);
        let damage = view.editor_mut().unwrap().insert_text(text, std::time::Duration::ZERO).unwrap();
        view.refresh_after_edit(damage);
        view.set_rows(rows);
        view
    }

    fn horizontal(view: &View, columns: usize, offset: u64) -> Bar {
        let (page, total) = crate::horizontal_extent(view, columns, 4);
        Bar::new(0, 800, page, total, offset)
    }

    #[test]
    fn mixed_ascii_and_shorter_tab_row_can_reach_its_end() {
        let mut view = view(&format!("{}\n{}", "a".repeat(100), "\t".repeat(30)), 2);
        let bar = horizontal(&view, 100, 0);
        assert_eq!(bar.maximum, 5);
        assert_eq!(crate::horizontal_extent(&view, 100, 4).0, 25);
        let end = bar.drag(usize::MAX, 0);
        view.set_horizontal_window(end, 64 << 10);
        let cells = crate::text::cells(view.line(1).unwrap(), 100, 4);
        assert_eq!(view.line_offset(1) + cells.last().unwrap().bytes.end as u64, 30);
        assert_eq!(horizontal(&view, 100, end), Bar::new(0, 800, 25, 30, end));
    }

    #[test]
    fn mixed_cjk_tabs_and_ascii_measure_every_rows_grid_extent() {
        let text = format!("{}\n{}{}", "a".repeat(100), "界".repeat(20), "\t".repeat(20));
        let mut view = view(&text, 2);
        let bar = horizontal(&view, 100, 0);
        // Eleven wide glyphs occupy 22 columns; the first tab fills to 24.
        assert_eq!(bar.maximum, 27);
        view.set_horizontal_window(bar.maximum, 64 << 10);
        let cells = crate::text::cells(view.line(1).unwrap(), 100, 4);
        assert_eq!(cells.last().unwrap().column + cells.last().unwrap().width, 100);
        assert_eq!(view.line_offset(1) + cells.last().unwrap().bytes.end as u64, 80);
        // Mouse hit coordinates remain source bytes, even inside a wide glyph.
        let byte = view.line_offset(1) + crate::hit::byte_at(&cells, 1) as u64;
        assert_eq!(byte, 27);
        assert_eq!(view.editor().unwrap().document().snapshot().len(), text.len() as u64);
    }

    #[test]
    fn eof_window_keeps_the_endpoint_and_thumb_stable() {
        let mut view = view(&format!("{}{}", "界".repeat(50), "a".repeat(100)), 4);
        let start = horizontal(&view, 100, 0);
        assert_eq!(start.maximum, 150);
        view.set_horizontal_window(start.maximum, 64 << 10);
        let end = horizontal(&view, 100, start.maximum);
        assert_eq!(end.maximum, start.maximum);
        assert_eq!(end.thumb_length, start.thumb_length);
        assert_eq!(end.thumb_start + end.thumb_length, end.length);
        assert_eq!(view.line(0).unwrap(), "a".repeat(100).as_bytes());
        // A stale window beyond EOF must not shrink its page into a moving endpoint.
        view.set_horizontal_window(249, 64 << 10);
        assert_eq!(horizontal(&view, 100, start.maximum), end);
    }

    #[test]
    fn cjk_endpoint_uses_safe_per_row_utf8_offsets() {
        let mut view = view(&format!("{}\n{}", "界".repeat(51), "a".repeat(200)), 2);
        let bar = horizontal(&view, 100, 0);
        assert_eq!(bar.maximum, 100);
        view.set_horizontal_window(bar.maximum, 64 << 10);
        assert_eq!(view.line_offset(0), 99);
        assert_eq!(view.line_offset(1), 100);
        assert!(std::str::from_utf8(view.line(0).unwrap()).is_ok());
        assert_eq!(horizontal(&view, 100, bar.maximum).maximum, bar.maximum);
        view.set_horizontal_window(199, 64 << 10);
        assert_eq!(horizontal(&view, 100, bar.maximum).maximum, bar.maximum);
    }

    #[test]
    fn bounded_tail_reaches_long_line_end_without_scanning_offscreen_rows() {
        let mut view = view(&format!("{}{}\n{}", "a".repeat(70_000), "\t".repeat(30), "b".repeat(80_000)), 1);
        assert!(view.line(0).unwrap().len() <= 64 << 10);
        assert!(view.line(1).is_none());
        let bar = horizontal(&view, 100, 0);
        assert_eq!(bar.maximum, 70_005);
        view.set_horizontal_window(bar.maximum, 64 << 10);
        assert_eq!(view.line(0).unwrap(), "\t".repeat(25).as_bytes());
        assert_eq!(horizontal(&view, 100, bar.maximum).maximum, bar.maximum);
        // The wider, previously offscreen row contributes only when visible.
        view.scroll(1);
        assert_eq!(horizontal(&view, 100, 0).maximum, 79_900);
    }

    #[test]
    fn one_right_drag_reveals_all_mixed_row_endings_and_stays_at_track_end() {
        let text =
            (0..28).map(|row| format!("ROW{row}\t{}END{row}", "é界😀\tABC".repeat(80))).collect::<Vec<_>>().join("\n");
        let mut view = view(&text, 28);
        let initial = horizontal(&view, 96, 0);
        let middle = initial.maximum / 2;
        view.set_horizontal_window(middle, 64 << 10);
        let before = horizontal(&view, 96, middle);
        assert_eq!(before.maximum, initial.maximum);
        assert_eq!(before.thumb_length, initial.thumb_length);
        let end = before.drag(usize::MAX, before.thumb_length / 2);
        view.set_horizontal_window(end, 64 << 10);
        let after = horizontal(&view, 96, end);
        assert_eq!(after.thumb_start + after.thumb_length, after.length);
        assert_eq!(after.drag(usize::MAX, after.thumb_length / 2), end);
        assert_eq!(after.maximum, initial.maximum);
        assert_eq!(after.thumb_length, initial.thumb_length);
        for row in 0..28 {
            let line = view.line(row).unwrap();
            assert!(std::str::from_utf8(line).is_ok());
            let cells = crate::text::cells(line, 96, 4);
            assert_eq!(cells.last().unwrap().bytes.end, line.len(), "clipped END{row}");
            let range = view.editor().unwrap().line_range(row as u64).unwrap();
            assert_eq!(view.line_offset(row) + line.len() as u64, range.end - range.start);
        }
    }

    #[test]
    fn drag_reaches_both_ends_of_huge_documents() {
        let bar = Bar::new(4, 700, 40, u64::MAX, u64::MAX);
        assert_eq!(bar.thumb_start + bar.thumb_length, 704);
        assert_eq!(bar.drag(0, 5), 0);
        assert_eq!(bar.drag(usize::MAX, 5), bar.maximum);
        assert!(bar.contains_thumb(703));
    }

    #[test]
    fn page_click_and_small_viewport_are_clamped() {
        let bar = Bar::new(0, 100, 10, 100, 50);
        assert_eq!(bar.page(0, 50, 10), 40);
        assert_eq!(bar.page(99, 89, 10), 90);
        let tiny = Bar::new(0, 5, 10, 100, 0);
        assert_eq!(tiny.thumb_length, 5);
        assert_eq!(tiny.drag(100, 0), 0);
        let fits = Bar::new(0, 100, 200, 100, 50);
        assert_eq!(fits.thumb_length, 100);
        assert_eq!(fits.maximum, 0);
    }
}
