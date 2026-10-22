//! Viewport, layout model, decorations; frontend-independent.
//!
//! Both frontends (GUI and TUI) drive the same `View`: it owns the open
//! file, the visible line range and the status text, and reports what a
//! change needs redrawn so the frontend can present only that.

use kwil_core_text::TextFile;
use std::path::PathBuf;

mod document;
mod editor;
pub mod grid;
pub mod hex;
mod workspace;
pub use workspace::{FilePrompt, FilePromptKind, Workspace};
pub mod document_worker;
pub use kwil_core_workspace::session::DocumentId as SessionDocumentId;
#[cfg(test)]
mod tests;
mod text;
pub use document::{Document, DocumentEncoding, DocumentLineEnding};
pub use editor::{Clipboard, ClipboardData, CommandArgs, CommandResult, CommandStatus, Cursor, Editor, Motion};

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
    line_offsets: Vec<u64>,
    notice: String,
    editor: Option<Editor>,
    pending_goto: Option<(u64, u64, bool)>,
    first_byte_column: u64,
    line_byte_limit: usize,
}

impl View {
    pub fn new(file: Option<(PathBuf, TextFile)>) -> View {
        let (path, file) = match file {
            Some((p, f)) => (Some(p), Some(f)),
            None => (None, None),
        };
        let editor = if file.is_none() { Some(Editor::new(Document::new())) } else { None };
        let mut view = View {
            file,
            path,
            top: 0,
            rows: 0,
            lines: Vec::new(),
            line_offsets: Vec::new(),
            notice: String::new(),
            editor,
            pending_goto: None,
            first_byte_column: 0,
            line_byte_limit: 16 << 10,
        };
        if view.file.as_ref().is_some_and(|file| file.index().is_some()) {
            view.index_ready();
        }
        view
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
        if let Some(editor) = &self.editor {
            return Some(editor.document().buffer().line_count());
        }
        self.file.as_ref()?.index().map(|i| i.line_count())
    }

    /// Text of visible row `row` (without line ending), if there is a line.
    pub fn line(&self, row: usize) -> Option<&[u8]> {
        self.lines.get(row).map(|l| l.as_slice())
    }

    /// Source-byte column of this row's UTF-8-aligned visible window.
    pub fn line_offset(&self, row: usize) -> u64 {
        self.line_offsets.get(row).copied().unwrap_or(0)
    }

    /// 1-based line number shown in the gutter for `row`.
    pub fn line_number(&self, row: usize) -> u64 {
        self.top + row as u64 + 1
    }

    pub fn set_rows(&mut self, rows: usize) -> Damage {
        self.rows = rows;
        self.top = self.top.min(self.max_top());
        if let Some(editor) = &mut self.editor {
            editor.set_viewport(self.top, rows);
        }
        self.reload();
        Damage { full: true, ..Damage::default() }
    }

    /// Call when the background index finished: line counts and scrolling
    /// become available.
    pub fn index_ready(&mut self) -> Damage {
        let installing = self.editor.is_none();
        if self.editor.is_none()
            && let (Some(path), Some(file)) = (&self.path, &self.file)
            && file.index().is_some()
        {
            match Document::from_indexed_file(path.clone(), file.clone()) {
                Ok(document) => {
                    let mut editor = Editor::new(document);
                    editor.set_viewport(self.top, self.rows);
                    self.editor = Some(editor);
                }
                Err(error) => {
                    self.notice = error.to_string();
                }
            }
        }
        if let Some((line, column, extend)) = self.pending_goto.take()
            && let Some(editor) = &mut self.editor
        {
            match editor.go_to_line(line, column, extend) {
                Ok(damage) => {
                    self.top = editor.top();
                    self.reload();
                    return damage;
                }
                Err(error) => {
                    self.notice = error.to_string();
                }
            }
        }
        self.reload();
        Damage {
            status: true,
            rows: if installing && self.editor.is_some() && self.rows > 0 { vec![0] } else { Vec::new() },
            ..Damage::default()
        }
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
        if let Some(editor) = &mut self.editor {
            editor.set_viewport(self.top, self.rows);
        }
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
        if let Some(editor) = &self.editor {
            let snapshot = editor.document().snapshot();
            let result = (0..self.rows)
                .take_while(|row| self.top + (*row as u64) < snapshot.line_count())
                .map(|row| {
                    let range = text::line(&snapshot, self.top + row as u64)?;
                    let wanted = range.start.saturating_add(self.first_byte_column).min(range.end);
                    let context = wanted.saturating_sub(3).max(range.start);
                    let end = wanted.saturating_add(self.line_byte_limit as u64 + 4).min(range.end);
                    let bytes = text::read(&snapshot, context..end, self.line_byte_limit as u64 + 7)?;
                    let start = grid::boundary(&bytes, (wanted - context) as usize);
                    let end = grid::boundary(&bytes, (start + self.line_byte_limit).min(bytes.len()));
                    Ok((context - range.start + start as u64, bytes[start..end].to_vec()))
                })
                .collect::<std::io::Result<Vec<_>>>();
            match result {
                Ok(lines) => {
                    let (offsets, lines) = lines.into_iter().unzip();
                    self.line_offsets = offsets;
                    self.lines = lines;
                }
                Err(error) => self.notice = error.to_string(),
            }
            return;
        }
        let Some(file) = &self.file else {
            self.lines = vec![b"No file open. Usage: kwil <file>".to_vec()];
            return;
        };
        let lines = file.lines(self.top, self.rows).and_then(|lines| match lines {
            Some(lines) => Ok(lines),
            None => file.first_lines(self.rows),
        });
        match lines {
            Ok(lines) => {
                let (offsets, lines) = lines
                    .into_iter()
                    .map(|l| {
                        let start = grid::boundary(l, self.first_byte_column.min(l.len() as u64) as usize);
                        let end = grid::boundary(l, start.saturating_add(self.line_byte_limit).min(l.len()));
                        (start as u64, l[start..end].to_vec())
                    })
                    .unzip();
                self.line_offsets = offsets;
                self.lines = lines;
            }
            Err(e) => self.notice = e.to_string(),
        }
    }

    pub fn editor(&self) -> Option<&Editor> {
        self.editor.as_ref()
    }
    /// After direct operations call refresh_after_edit with their Damage.
    /// Editing is unavailable until the background sparse index is ready.
    pub fn editor_mut(&mut self) -> Option<&mut Editor> {
        self.editor.as_mut()
    }

    pub fn refresh_after_edit(&mut self, damage: Damage) -> Damage {
        if let Some(editor) = &self.editor {
            self.top = editor.top();
            self.path = editor.document.path.clone();
            self.file = editor.document.original.clone();
        }
        if damage.full || damage.scroll != 0 || !damage.rows.is_empty() {
            self.reload();
        }
        damage
    }

    /// Bounded byte window for long-line layout. The frontend chooses a
    /// visible column range; the default never caches an entire giant line.
    pub fn set_horizontal_window(&mut self, first_byte: u64, max_bytes: usize) -> Damage {
        self.first_byte_column = first_byte;
        self.line_byte_limit = max_bytes.clamp(1, 64 << 10);
        self.reload();
        Damage { full: true, ..Damage::default() }
    }

    pub fn execute(&mut self, id: &str, args: &CommandArgs, count: u32) -> std::io::Result<CommandResult> {
        let Some(editor) = &mut self.editor else {
            if id == "navigation.goToLine"
                && let Some(line) = args.line
            {
                self.pending_goto = Some((line, args.column.unwrap_or(1), args.extend));
            }
            return Ok(CommandResult {
                damage: Damage { status: true, ..Damage::default() },
                status: CommandStatus::IndexNotReady,
            });
        };
        let mut result = editor.execute(id, args, count)?;
        result.damage = self.refresh_after_edit(result.damage);
        Ok(result)
    }

    pub fn status_text(&self) -> String {
        let mut name = self.path.as_ref().map_or_else(|| "kwil".to_string(), |p| p.display().to_string());
        if self.editor.as_ref().is_some_and(|editor| editor.document().is_dirty()) {
            name.push_str(" *");
        }
        let lines = match (self.file.as_ref(), self.line_count()) {
            (None, _) => String::new(),
            (Some(_), Some(n)) => {
                let (row, column) = self
                    .editor
                    .as_ref()
                    .and_then(|e| e.position_at(e.primary_cursor().head).ok())
                    .unwrap_or((self.top, 0));
                format!("  |  {n} lines  |  Ln {}, Col {}", row + 1, column + 1)
            }
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
mod viewer_tests {
    use super::*;

    fn view_with_lines(name: &str, n: usize) -> View {
        // One file per test: tests run in parallel and must not share it.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("kwil-vm-test-{}-{id}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f.txt");
        let text: String = (0..n).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&path, text).unwrap();
        let file = TextFile::open(&path).unwrap();
        file.build_index(2).unwrap();
        let mut v = View::new(Some((path, file)));
        v.set_rows(10);
        v
    }

    #[test]
    fn index_completion_damages_newly_available_caret() {
        let mut view = view_with_lines("late-caret", 10);
        // The frontend has drawn the viewer while the index worker ran.
        view.editor = None;
        let damage = view.index_ready();
        assert!(view.editor().is_some());
        assert!(damage.full || damage.rows.contains(&0));
        assert!(damage.status);
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
