//! Viewport, layout model, decorations; frontend-independent.
//!
//! Both frontends (GUI and TUI) drive the same `View`: it owns the open
//! file, the visible line range and the status text, and reports what a
//! change needs redrawn so the frontend can present only that.

use kwil_core_text::TextFile;
use std::path::PathBuf;

/// A message from a worker thread for the status bar (update notices,
/// errors). Frontends turn it into a wake-up of their event loop.
pub type Notify = Box<dyn Fn(String) + Send + 'static>;

/// What a frontend must redraw after a model change.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Damage {
    /// Redraw everything (resize, jump further than one screen).
    pub full: bool,
    /// The text area moved by this many rows (positive = content moved up);
    /// frontends copy pixels or cells and redraw only `rows`.
    pub scroll: i64,
    /// Visible rows to redraw, relative to the top of the viewport.
    pub rows: Vec<usize>,
    pub status: bool,
}

pub struct View {
    file: Option<TextFile>,
    path: Option<PathBuf>,
    top: u64,
    rows: usize,
    lines: Vec<Vec<u8>>,
    notice: String,
}

impl View {
    pub fn new(file: Option<(PathBuf, TextFile)>) -> View {
        let (path, file) = match file {
            Some((p, f)) => (Some(p), Some(f)),
            None => (None, None),
        };
        View { file, path, top: 0, rows: 0, lines: Vec::new(), notice: String::new() }
    }

    pub fn file(&self) -> Option<&TextFile> {
        self.file.as_ref()
    }

    pub fn top(&self) -> u64 {
        self.top
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Lines of the file, once the background index is ready.
    pub fn line_count(&self) -> Option<u64> {
        self.file.as_ref()?.index().map(|i| i.line_count())
    }

    /// Text of visible row `row` (without line ending), if there is a line.
    pub fn line(&self, row: usize) -> Option<&[u8]> {
        self.lines.get(row).map(|l| l.as_slice())
    }

    /// 1-based line number shown in the gutter for `row`.
    pub fn line_number(&self, row: usize) -> u64 {
        self.top + row as u64 + 1
    }

    pub fn set_rows(&mut self, rows: usize) -> Damage {
        self.rows = rows;
        self.top = self.top.min(self.max_top());
        self.reload();
        Damage { full: true, ..Damage::default() }
    }

    /// Call when the background index finished: line counts and scrolling
    /// become available.
    pub fn index_ready(&mut self) -> Damage {
        self.reload();
        Damage { status: true, ..Damage::default() }
    }

    pub fn set_notice(&mut self, notice: String) -> Damage {
        self.notice = notice;
        Damage { status: true, ..Damage::default() }
    }

    fn max_top(&self) -> u64 {
        self.line_count().map_or(0, |n| n.saturating_sub(self.rows as u64))
    }

    /// Scrolls by `delta` lines (clamped). Before the index is ready the
    /// view stays on the first screen.
    pub fn scroll(&mut self, delta: i64) -> Damage {
        let new_top = (self.top as i64 + delta).clamp(0, self.max_top() as i64) as u64;
        let d = new_top as i64 - self.top as i64;
        if d == 0 {
            return Damage::default();
        }
        self.top = new_top;
        self.reload();
        let mut damage = Damage { status: true, ..Damage::default() };
        if d.unsigned_abs() as usize >= self.rows {
            damage.full = true;
        } else {
            damage.scroll = d;
            damage.rows =
                if d > 0 { (self.rows - d as usize..self.rows).collect() } else { (0..(-d) as usize).collect() };
        }
        damage
    }

    pub fn scroll_to_end(&mut self) -> Damage {
        self.scroll(i64::MAX / 2)
    }

    pub fn scroll_to_start(&mut self) -> Damage {
        self.scroll(-(self.top as i64))
    }

    fn reload(&mut self) {
        let Some(file) = &self.file else {
            self.lines = vec![b"No file open. Usage: kwil <file>".to_vec()];
            return;
        };
        let lines = file.lines(self.top, self.rows).unwrap_or_else(|| file.first_lines(self.rows));
        self.lines = lines.into_iter().map(|l| l.to_vec()).collect();
    }

    pub fn status_text(&self) -> String {
        let name = self.path.as_ref().map_or_else(|| "kwil".to_string(), |p| p.display().to_string());
        let lines = match (self.file.as_ref(), self.line_count()) {
            (None, _) => String::new(),
            (Some(_), Some(n)) => format!("  |  {n} lines  |  Ln {}", self.top + 1),
            (Some(f), None) => {
                let p = f.progress();
                format!("  |  ~ indexing {}%", p.scanned * 100 / p.total.max(1))
            }
        };
        let notice = if self.notice.is_empty() { String::new() } else { format!("  |  {}", self.notice) };
        format!("{name}{lines}{notice}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view_with_lines(name: &str, n: usize) -> View {
        // One file per test: tests run in parallel and must not share it.
        let dir = std::env::temp_dir().join(format!("kwil-vm-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f.txt");
        let text: String = (0..n).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&path, text).unwrap();
        let file = TextFile::open(&path).unwrap();
        file.build_index(2);
        let mut v = View::new(Some((path, file)));
        v.set_rows(10);
        v
    }

    #[test]
    fn scroll_reports_copy_and_exposed_rows() {
        let mut v = view_with_lines("scroll", 100);
        let d = v.scroll(3);
        assert_eq!(d.scroll, 3);
        assert_eq!(d.rows, vec![7, 8, 9]);
        assert!(!d.full);
        assert_eq!(v.line(0), Some(&b"line 3"[..]));
        let d = v.scroll(-2);
        assert_eq!((d.scroll, d.rows), (-2, vec![0, 1]));
        assert_eq!(v.line(0), Some(&b"line 1"[..]));
    }

    #[test]
    fn scroll_clamps_and_jumps_redraw_fully() {
        let mut v = view_with_lines("clamp", 100);
        // 100 lines + the empty line after the last newline = 101.
        let d = v.scroll_to_end();
        assert!(d.full);
        assert_eq!(v.top(), 91);
        assert_eq!(v.scroll(5), Damage::default());
        v.scroll_to_start();
        assert_eq!(v.top(), 0);
    }
}
