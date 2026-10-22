//! Bounded byte-to-grid layout. Original bytes are never changed.
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

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
    let limit = bytes.len().min(columns.saturating_mul(256).min(64 * 1024));
    let bytes = &bytes[..limit];
    let mut result = Vec::new();
    let (mut offset, mut column) = (0, 0);
    while offset < bytes.len() && column < columns {
        let (valid, bad) = match std::str::from_utf8(&bytes[offset..]) {
            Ok(s) => (s, false),
            Err(e) => (std::str::from_utf8(&bytes[offset..offset + e.valid_up_to()]).unwrap(), true),
        };
        for g in valid.graphemes(true) {
            let (text, width) = if g == "\t" {
                (g.to_owned(), tab_size.max(1) - column % tab_size.max(1))
            } else if g.chars().any(char::is_control) {
                (format!("\\x{:02X}", g.as_bytes()[0]), 4)
            } else {
                let mut end = g.len().min(256);
                while !g.is_char_boundary(end) {
                    end -= 1;
                }
                (g[..end].to_owned(), UnicodeWidthStr::width(g).max(1))
            };
            if column + width > columns {
                return result;
            }
            result.push(Cell { text, column, width, bytes: offset..offset + g.len() });
            column += width;
            offset += g.len();
            if column >= columns {
                return result;
            }
        }
        if bad && offset < bytes.len() {
            if column + 4 > columns {
                break;
            }
            result.push(Cell {
                text: format!("\\x{:02X}", bytes[offset]),
                column,
                width: 4,
                bytes: offset..offset + 1,
            });
            column += 4;
            offset += 1;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
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
