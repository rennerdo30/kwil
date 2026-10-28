//! Shared glyph-cell mapping for mouse input and caret drawing.
use crate::text::Cell;

pub fn byte_at(cells: &[Cell], column: usize) -> usize {
    cells
        .iter()
        .find(|cell| column < cell.column + cell.width)
        .map_or_else(|| cells.last().map_or(0, |cell| cell.bytes.end), |cell| cell.bytes.start)
}

pub fn column_at(cells: &[Cell], byte: usize) -> usize {
    cells
        .iter()
        .find(|cell| byte < cell.bytes.end)
        .map_or_else(|| cells.last().map_or(0, |cell| cell.column + cell.width), |cell| cell.column)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hit_testing_never_splits_clusters_tabs_or_invalid_byte_escapes() {
        let cells = crate::text::cells("a界e\u{301}\tZ".as_bytes(), 20, 4);
        assert_eq!(byte_at(&cells, 1), 1);
        assert_eq!(byte_at(&cells, 2), 1);
        assert_eq!(byte_at(&cells, 3), 4);
        assert_eq!(byte_at(&cells, 7), 7);
        assert_eq!(column_at(&cells, 4), 3);
        assert_eq!(column_at(&cells, 7), 4);
        assert_eq!(column_at(&cells, 8), 8);
        let cells = crate::text::cells(b"\xffZ", 8, 4);
        assert_eq!(byte_at(&cells, 3), 0);
        assert_eq!(byte_at(&cells, 4), 1);
        assert_eq!(column_at(&cells, 1), 4);
        assert_eq!(byte_at(&[], 100), 0);
    }
}
