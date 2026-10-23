//! Bounded byte-to-grid layout. Original bytes are never changed.
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Move a byte window edge to the start of a valid UTF-8 scalar crossing it.
/// Invalid original bytes remain individually visible and editable.
pub fn boundary(bytes: &[u8], position: usize) -> usize {
    let position = position.min(bytes.len());
    for back in 1..=3.min(position) {
        let start = position - back;
        let length = match bytes[start] {
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => continue,
        };
        if start + length > position
            && start + length <= bytes.len()
            && std::str::from_utf8(&bytes[start..start + length]).is_ok()
        {
            return start;
        }
    }
    position
}

/// A single cell cluster, with byte-exact source coordinates for future hit testing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    pub text: String,
    pub column: usize,
    pub width: usize,
    pub bytes: std::ops::Range<usize>,
}

/// Only visits the visible prefix. Both output and grapheme inspection are capped,
/// including adversarial lines made entirely from combining marks.
pub fn cells(bytes: &[u8], columns: usize, tab_size: usize) -> Vec<Cell> {
    let columns = columns.min(4096);
    let limit = bytes.len().min(columns.saturating_mul(256).min(64 * 1024));
    let bytes = &bytes[..boundary(bytes, limit)];
    let mut result = Vec::new();
    visit(bytes, tab_size, |grapheme, range, column, width| {
        if column + width > columns {
            return false;
        }
        let text = match grapheme {
            Some("\t") => "\t".to_owned(),
            Some(g) if !g.chars().any(char::is_control) => {
                let mut end = g.len().min(256);
                while !g.is_char_boundary(end) {
                    end -= 1;
                }
                g[..end].to_owned()
            }
            _ => format!("\\x{:02X}", bytes[range.start]),
        };
        result.push(Cell { text, column, width, bytes: range });
        column + width < columns
    });
    result
}

/// Shared allocation-free traversal for drawing and model columns. Callers
/// provide a bounded byte slice; false stops before visiting another cluster.
/// None denotes one invalid original byte, displayed as a four-column escape.
pub(crate) fn visit(
    bytes: &[u8],
    tab_size: usize,
    mut visitor: impl FnMut(Option<&str>, std::ops::Range<usize>, usize, usize) -> bool,
) {
    let (mut offset, mut column) = (0, 0);
    while offset < bytes.len() {
        let (valid, bad) = match std::str::from_utf8(&bytes[offset..]) {
            Ok(s) => (s, false),
            Err(e) => (std::str::from_utf8(&bytes[offset..offset + e.valid_up_to()]).unwrap(), true),
        };
        for g in valid.graphemes(true) {
            let width = if g == "\t" {
                tab_size.max(1) - column % tab_size.max(1)
            } else if g.chars().any(char::is_control) {
                4
            } else {
                UnicodeWidthStr::width(g).max(1)
            };
            if !visitor(Some(g), offset..offset + g.len(), column, width) {
                return;
            }
            column += width;
            offset += g.len();
        }
        if bad && offset < bytes.len() {
            if !visitor(None, offset..offset + 1, column, 4) {
                return;
            }
            column += 4;
            offset += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn window_edges_preserve_valid_scalars_and_keep_invalid_bytes() {
        let bytes = "a界🦀z".as_bytes();
        for position in [2, 3] {
            assert_eq!(boundary(bytes, position), 1);
        }
        for position in [5, 6, 7] {
            assert_eq!(boundary(bytes, position), 4);
        }
        assert_eq!(boundary(bytes, 4), 4);
        assert_eq!(boundary(b"a\xff\xa9z", 2), 2);
    }
    #[test]
    fn cjk_and_combining_and_emoji_use_grid_columns() {
        let c = cells("A界e\u{301}👩‍💻Z".as_bytes(), 20, 4);
        assert_eq!(
            c.iter().map(|c| (c.text.as_str(), c.column, c.width)).collect::<Vec<_>>(),
            vec![("A", 0, 1), ("界", 1, 2), ("e\u{301}", 3, 1), ("👩‍💻", 4, 2), ("Z", 6, 1)]
        );
        assert_eq!(c[1].bytes, 1..4);
    }
    #[test]
    fn tabs_follow_configured_stops() {
        let c = cells(b"a\tb\tc", 16, 4);
        assert_eq!(
            c.iter().map(|c| (c.column, c.width)).collect::<Vec<_>>(),
            vec![(0, 1), (1, 3), (4, 1), (5, 3), (8, 1)]
        );
        assert_eq!(cells(b"a\tb", 8, 2)[2].column, 2);
    }
    #[test]
    fn invalid_bytes_are_visible_individually() {
        let c = cells(b"A\xff\xc0Z", 20, 4);
        assert_eq!(c.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(), vec!["A", "\\xFF", "\\xC0", "Z"]);
        assert_eq!(c[1].bytes, 1..2);
        assert_eq!(c[3].column, 9);
    }
    #[test]
    fn viewport_clips_wide_clusters_and_unbounded_marks() {
        assert_eq!(cells("A界Z".as_bytes(), 2, 4).len(), 1);
        let marks = format!("A{}Z", "\u{301}".repeat(100_000));
        let c = cells(marks.as_bytes(), 3, 4);
        assert!(!c.is_empty());
        assert!(c.iter().map(|c| c.text.len()).sum::<usize>() <= 1024);
    }
}
