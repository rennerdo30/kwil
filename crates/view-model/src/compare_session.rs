//! Read-only comparison. A single worker owns inputs, diff and on-demand word
//! refinement; the UI retains at most one capped page, never file text.
use crate::Workspace;
use kwil_core_diff::{self as diff, LineSource};
use kwil_core_text::{Snapshot, TextFile};
use std::{
    io,
    ops::Range,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
};

pub use diff::Kind;
const PAGE_ROWS: usize = 128;
const ROW_BYTES: usize = 4096;

#[derive(Default)]
pub struct Session {
    worker: Option<Worker>,
    pub page: Option<Page>,
    pub open: bool,
    pub(crate) prompt: bool,
    source: Option<(u64, u64, u64)>,
    busy: bool,
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
    pub notice: String,
}
pub struct Page {
    pub rows: Vec<Row>,
    pub top: usize,
    pub total: usize,
    pub changes: usize,
    pub ignore_whitespace: bool,
    pub collapsed: bool,
    pub right_title: String,
}
pub struct Row {
    pub left: Option<usize>,
    pub right: Option<usize>,
    pub kind: Kind,
    pub text: [Vec<u8>; 2],
    pub words: [Vec<Range<usize>>; 2],
    pub hidden: usize,
}
enum Request {
    Start(Snapshot, PathBuf),
    Action(Action),
    Close,
}
#[derive(Clone, Copy)]
pub enum Action {
    Scroll(i64),
    Next,
    Previous,
    Whitespace,
    Unchanged,
}
struct Worker {
    tx: mpsc::SyncSender<Request>,
    rx: mpsc::Receiver<Result<Option<Page>, String>>,
    cancel: diff::CancellationToken,
    stop: Arc<AtomicBool>,
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.stop.store(true, Ordering::Relaxed);
        // Dropping tx wakes recv. Inputs and any private files retire on worker.
    }
}
struct Input(Snapshot);
impl LineSource for Input {
    fn line_count(&self) -> usize {
        self.0.line_count().min(usize::MAX as u64) as usize
    }
    fn line<'a>(&'a self, index: usize, scratch: &mut Vec<u8>) -> Result<diff::Line<'a>, diff::DiffError> {
        let read = |e: kwil_core_text::ReadError| diff::DiffError::Read(io::Error::other(e).kind());
        let start = self
            .0
            .line_start(index as u64)
            .map_err(read)?
            .ok_or(diff::DiffError::Read(io::ErrorKind::UnexpectedEof))?;
        let end = self.0.line_start(index as u64 + 1).map_err(read)?.unwrap_or(self.0.len());
        if end - start > diff::MAX_LINE_BYTES as u64 {
            return Err(diff::DiffError::TooLarge);
        }
        scratch.clear();
        for chunk in self.0.chunks(start..end).map_err(|e| diff::DiffError::Read(e.kind()))? {
            scratch.extend_from_slice(chunk.map_err(read)?);
        }
        Ok(diff::Line::Scratch)
    }
}
struct State {
    left: Input,
    right: Input,
    backing: Option<crate::document_worker::PreparedDecodedDocument>,
    model: diff::Comparison,
    segments: Vec<diff::DisplaySegment>,
    options: diff::Options,
    collapsed: bool,
    top: usize,
    title: String,
}
impl Drop for State {
    fn drop(&mut self) {
        self.right.0 = Snapshot::default();
        if let Some(prepared) = self.backing.take() {
            drop(prepared.document);
            if let Err(error) = prepared.backing.try_retire() {
                // Retain the precise cleanup paths inside the error for diagnosis.
                eprintln!("{}", error.with_cause(io::Error::other("Compare backing retirement failed")));
            }
        }
    }
}
fn failure(error: diff::DiffError) -> String {
    match error {
        diff::DiffError::TooLarge => {
            "Comparison exceeds the diff limits (262,144 total lines, 1 MiB per line, 64 MiB total text)".into()
        }
        diff::DiffError::Cancelled => "Comparison cancelled".into(),
        diff::DiffError::Read(kind) => format!("Comparison source could not be read: {kind}"),
    }
}
impl State {
    fn open(
        left: Snapshot,
        path: PathBuf,
        cancel: &diff::CancellationToken,
        stop: &AtomicBool,
    ) -> Result<Self, String> {
        if left.len() > diff::MAX_SCANNED_BYTES as u64 || left.line_count() > diff::MAX_LINES as u64 {
            return Err(failure(diff::DiffError::TooLarge));
        }
        let file = TextFile::open_saved_version(&path).map_err(|e| format!("Compare: {e}"))?;
        if left.len().saturating_add(file.len()) > diff::MAX_SCANNED_BYTES as u64 {
            return Err(failure(diff::DiffError::TooLarge));
        }
        let prepared = crate::document_worker::prepare_decoded_document(
            path.clone(),
            file,
            None,
            &std::env::temp_dir(),
            stop,
            |_| Ok(()),
        )
        .map_err(|e| format!("Compare: {e}"))?;
        let mut state = Self {
            left: Input(left),
            right: Input(prepared.document.snapshot()),
            backing: Some(prepared),
            model: diff::Comparison::default(),
            segments: Vec::new(),
            options: diff::Options::default(),
            collapsed: false,
            top: 0,
            title: path.file_name().unwrap_or_default().to_string_lossy().chars().take(100).collect(),
        };
        state.recompute(cancel)?;
        state.top = state.model.hunks.first().map_or(0, |h| h.rows.start.saturating_sub(3));
        Ok(state)
    }
    fn recompute(&mut self, cancel: &diff::CancellationToken) -> Result<(), String> {
        self.model = diff::compare(&self.left, &self.right, &self.options, cancel).map_err(failure)?;
        self.rebuild();
        Ok(())
    }
    fn rebuild(&mut self) {
        self.segments = if self.collapsed {
            self.model.collapsed(3)
        } else {
            vec![diff::DisplaySegment::Rows(0..self.model.row_count())]
        };
        self.top = self.top.min(self.total().saturating_sub(1));
    }
    fn total(&self) -> usize {
        self.segments
            .iter()
            .map(|s| match s {
                diff::DisplaySegment::Rows(r) => r.len(),
                _ => 1,
            })
            .sum()
    }
    fn display_row(&self, mut index: usize) -> (usize, usize) {
        for segment in &self.segments {
            let (range, hidden) = match segment {
                diff::DisplaySegment::Rows(r) => (r, false),
                diff::DisplaySegment::Collapsed(r) => (r, true),
            };
            let len = if hidden { 1 } else { range.len() };
            if index < len {
                return (range.start + if hidden { 0 } else { index }, if hidden { range.len() } else { 0 });
            }
            index -= len;
        }
        (self.model.row_count(), 0)
    }
    fn display_index(&self, row: usize) -> usize {
        let mut index = 0;
        for s in &self.segments {
            match s {
                diff::DisplaySegment::Rows(r) if r.contains(&row) => return index + row - r.start,
                diff::DisplaySegment::Collapsed(r) if r.contains(&row) => return index,
                diff::DisplaySegment::Rows(r) => index += r.len(),
                _ => index += 1,
            }
        }
        0
    }
    fn action(&mut self, action: Action, cancel: &diff::CancellationToken) -> Result<(), String> {
        match action {
            Action::Scroll(delta) => {
                self.top = self.top.saturating_add_signed(delta as isize).min(self.total().saturating_sub(1))
            }
            Action::Next | Action::Previous => {
                let row = self.display_row(self.top).0;
                let hunk = if matches!(action, Action::Next) {
                    self.model.next_hunk(row)
                } else {
                    self.model.previous_hunk(row)
                };
                if let Some(hunk) = hunk {
                    self.top = self.display_index(hunk.rows.start);
                }
            }
            Action::Whitespace => {
                self.options.whitespace = if self.options.whitespace == diff::Whitespace::Exact {
                    diff::Whitespace::All
                } else {
                    diff::Whitespace::Exact
                };
                self.recompute(cancel)?;
            }
            Action::Unchanged => {
                let row = self.display_row(self.top).0;
                self.collapsed = !self.collapsed;
                self.rebuild();
                self.top = self.display_index(row);
            }
        }
        Ok(())
    }
    fn page(&self, cancel: &diff::CancellationToken) -> Result<Page, String> {
        let mut rows = Vec::new();
        for index in self.top..self.total().min(self.top + PAGE_ROWS) {
            let (at, hidden) = self.display_row(index);
            let Some(row) = self.model.row(at) else { break };
            let mut text = [Vec::new(), Vec::new()];
            if hidden == 0 {
                for (side, line) in [row.left, row.right].into_iter().enumerate() {
                    if let Some(line) = line {
                        let input = if side == 0 { &self.left } else { &self.right };
                        let start = input.0.line_start(line as u64).map_err(|e| e.to_string())?.unwrap();
                        let end =
                            input.0.line_start(line as u64 + 1).map_err(|e| e.to_string())?.unwrap_or(input.0.len());
                        for chunk in
                            input.0.chunks(start..end.min(start + ROW_BYTES as u64)).map_err(|e| e.to_string())?
                        {
                            text[side].extend_from_slice(chunk.map_err(|e| e.to_string())?);
                        }
                        while text[side].last().is_some_and(|b| matches!(b, b'\r' | b'\n')) {
                            text[side].pop();
                        }
                    }
                }
            }
            let words = if hidden == 0 {
                self.model.refine(at, &self.left, &self.right, cancel).map_err(failure)?
            } else {
                None
            };
            let words = words.map_or_else(|| [Vec::new(), Vec::new()], |w| [w.left, w.right]);
            rows.push(Row { left: row.left, right: row.right, kind: row.kind, text, words, hidden });
        }
        Ok(Page {
            rows,
            top: self.top,
            total: self.total(),
            changes: self.model.hunks.len(),
            ignore_whitespace: self.options.whitespace != diff::Whitespace::Exact,
            collapsed: self.collapsed,
            right_title: self.title.clone(),
        })
    }
}
impl Session {
    pub fn set_wake(&mut self, wake: Arc<dyn Fn() + Send + Sync>) {
        self.wake = Some(wake);
    }
    fn worker(&mut self) -> io::Result<&mut Worker> {
        if self.worker.is_none() {
            let wake = self.wake.clone().unwrap_or_else(|| Arc::new(|| {}));
            let (tx, requests) = mpsc::sync_channel(1);
            let (results, rx) = mpsc::sync_channel(1);
            let stop = Arc::new(AtomicBool::new(false));
            let stopped = stop.clone();
            let cancel = diff::CancellationToken::default();
            let cancelled = cancel.clone();
            std::thread::Builder::new().name("kwil-compare".into()).spawn(move || {
                let mut state: Option<State> = None;
                while let Ok(request) = requests.recv() {
                    if stopped.load(Ordering::Relaxed) {
                        break;
                    }
                    let result = match request {
                        Request::Start(snapshot, path) => {
                            state = None;
                            State::open(snapshot, path, &cancelled, &stopped).map(|s| state = Some(s))
                        }
                        Request::Action(action) => state.as_mut().map_or(Ok(()), |s| s.action(action, &cancelled)),
                        Request::Close => {
                            state = None;
                            Ok(())
                        }
                    }
                    .and_then(|()| state.as_ref().map(|s| s.page(&cancelled)).transpose());
                    if results.send(result).is_err() {
                        break;
                    }
                    wake();
                }
            })?;
            self.worker = Some(Worker { tx, rx, cancel, stop });
        }
        Ok(self.worker.as_mut().unwrap())
    }
    fn send(&mut self, request: Request) -> io::Result<()> {
        if self.busy {
            return Err(io::Error::other("Comparison is working; try again shortly"));
        }
        self.worker()?.tx.try_send(request).map_err(|_| io::Error::other("Comparison worker is unavailable"))?;
        self.busy = true;
        Ok(())
    }
    pub fn action(&mut self, action: Action) {
        if let Err(e) = self.send(Request::Action(action)) {
            self.notice = e.to_string();
        }
    }
    pub fn close(&mut self) {
        self.open = false;
        self.page = None;
        self.source = None;
        // A busy result is discarded in poll, then the input retires on worker.
        if !self.busy {
            let _ = self.send(Request::Close);
        }
    }
    fn poll(&mut self, source: Option<(u64, u64, u64)>) -> bool {
        let invalidated = self.open && self.source != source;
        if invalidated {
            self.close();
        }
        let Some(result) = self.worker.as_ref().and_then(|w| w.rx.try_recv().ok()) else { return invalidated };
        self.busy = false;
        if !self.open {
            if result.as_ref().is_ok_and(|p| p.is_some()) {
                let _ = self.send(Request::Close);
            }
            return true;
        }
        match result {
            Ok(page) => {
                self.page = page;
                self.notice.clear();
            }
            Err(error) => {
                self.page = None;
                self.notice = error;
            }
        }
        true
    }
    pub fn header(&self) -> String {
        if !self.notice.is_empty() {
            return self.notice.clone();
        }
        match &self.page {
            None => "Comparing…  Esc closes".into(),
            Some(p) => format!(
                "Buffer | {} | {} changes | whitespace: {} | unchanged: {} | F7 / Shift+F7  Esc closes",
                p.right_title,
                p.changes,
                if p.ignore_whitespace { "ignored" } else { "exact" },
                if p.collapsed { "collapsed" } else { "shown" }
            ),
        }
    }
}
impl Workspace {
    fn compare_identity(&self) -> Option<(u64, u64, u64)> {
        Some((self.active_id().0, self.input_epoch(), self.editor()?.document().revision()))
    }
    pub fn poll_compare(&mut self) -> bool {
        let source = self.compare_identity();
        self.compare.poll(source)
    }
    pub(crate) fn compare_path(&mut self, path: PathBuf) -> io::Result<()> {
        let source =
            self.compare_identity().ok_or_else(|| io::Error::other("Wait for the current tab to finish opening"))?;
        let snapshot = self.editor().unwrap().document().snapshot();
        self.compare.send(Request::Start(snapshot, path))?;
        self.compare.open = true;
        self.compare.source = Some(source);
        self.compare.page = None;
        self.compare.notice.clear();
        Ok(())
    }
    pub fn compare_command(&mut self, id: &str) -> bool {
        if !id.starts_with("compare.") {
            return false;
        }
        let result = match id {
            "compare.files" if self.file_prompt().is_some() => {
                Err(io::Error::other("Finish or cancel the file prompt first"))
            }
            "compare.files" => self.file_command("file.open").map(|_| self.compare.prompt = true),
            "compare.saved" => self
                .editor()
                .and_then(|e| e.document().path().map(std::path::Path::to_path_buf))
                .ok_or_else(|| io::Error::other("Save the current tab before comparing with disk"))
                .and_then(|path| self.compare_path(path)),
            "compare.close" => {
                self.compare.close();
                Ok(())
            }
            _ if !self.compare.open => Err(io::Error::other("Start Compare Files or Compare with Saved first")),
            _ => {
                let action = match id {
                    "compare.nextChange" => Action::Next,
                    "compare.previousChange" => Action::Previous,
                    "compare.toggleIgnoreWhitespace" => Action::Whitespace,
                    "compare.toggleUnchanged" => Action::Unchanged,
                    _ => return false,
                };
                self.compare.action(action);
                Ok(())
            }
        };
        let notice = result
            .err()
            .map_or_else(|| if self.compare.open { self.compare.header() } else { String::new() }, |e| e.to_string());
        self.set_notice(notice);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CommandArgs, View};
    use std::sync::atomic::AtomicU64;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new(bytes: &[u8]) -> Self {
            let path = std::env::temp_dir().join(format!(
                "kwil-compare-{}-{}.txt",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::write(&path, bytes).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_file(&self.0).unwrap();
        }
    }
    fn workspace(text: &str) -> Workspace {
        let mut view = Workspace::new(View::new(None)).unwrap();
        view.execute("edit.insert", &CommandArgs { text: Some(text.into()), ..Default::default() }, 1).unwrap();
        view
    }
    #[test]
    fn compare_page_aligns_words_and_navigates_collapsed_changes() {
        let fixture = Fixture::new(b"head\nold word\n2\n3\n4\n5\n6\n7\n8\n9\nold tail\n");
        let view = workspace("head\nnew word\n2\n3\n4\n5\n6\n7\n8\n9\nnew tail\n");
        let cancel = diff::CancellationToken::default();
        let mut state = State::open(
            view.editor().unwrap().document().snapshot(),
            fixture.0.clone(),
            &cancel,
            &AtomicBool::new(false),
        )
        .unwrap();
        let page = state.page(&cancel).unwrap();
        assert_eq!(page.changes, 2);
        assert_eq!(page.rows[1].kind, Kind::Modify);
        assert_eq!(page.rows[1].text, [b"new word".to_vec(), b"old word".to_vec()]);
        assert!(!page.rows[1].words[0].is_empty());
        state.action(Action::Next, &cancel).unwrap();
        assert_eq!(state.top, 1);
        state.action(Action::Next, &cancel).unwrap();
        assert_eq!(state.top, 10);
        state.action(Action::Previous, &cancel).unwrap();
        assert_eq!(state.top, 1);
        state.action(Action::Unchanged, &cancel).unwrap();
        assert!(state.page(&cancel).unwrap().rows.iter().any(|r| r.hidden > 0));
        state.action(Action::Next, &cancel).unwrap();
        assert_eq!(state.display_row(state.top).0, 10);
    }
    #[test]
    fn compare_whitespace_and_admission_do_not_change_sources() {
        let fixture = Fixture::new(b"same word\n");
        let view = workspace("same   word\n");
        let snapshot = view.editor().unwrap().document().snapshot();
        let cancel = diff::CancellationToken::default();
        let mut state = State::open(snapshot.clone(), fixture.0.clone(), &cancel, &AtomicBool::new(false)).unwrap();
        assert_eq!(state.model.hunks.len(), 1);
        state.action(Action::Whitespace, &cancel).unwrap();
        assert!(state.model.hunks.is_empty());
        state.action(Action::Whitespace, &cancel).unwrap();
        assert_eq!(state.model.hunks.len(), 1);
        let mut original = Vec::new();
        snapshot.write_to(&mut original).unwrap();
        assert_eq!(original, b"same   word\n");
        assert_eq!(std::fs::read(&fixture.0).unwrap(), b"same word\n");
        let oversized = workspace(&"\n".repeat(diff::MAX_LINES));
        assert!(
            State::open(
                oversized.editor().unwrap().document().snapshot(),
                fixture.0.clone(),
                &cancel,
                &AtomicBool::new(false)
            )
            .err()
            .unwrap()
            .contains("diff limits")
        );
    }
    #[test]
    fn compare_open_prompt_preserves_tab_and_discards_closed_completion() {
        let fixture = Fixture::new(b"disk\n");
        let mut view = workspace("buffer\n");
        let (tx, rx) = mpsc::channel();
        view.compare.set_wake(Arc::new(move || {
            let _ = tx.send(());
        }));
        let document = view.active_id();
        assert!(view.compare_command("compare.saved"));
        assert!(view.status_text().contains("Save the current tab"));
        view.compare_command("compare.files");
        view.file_prompt_mut().unwrap().text = fixture.0.to_string_lossy().into();
        view.submit_file_prompt().unwrap();
        assert_eq!(view.active_id(), document);
        view.compare.close();
        rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        view.poll_compare();
        rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        view.poll_compare();
        assert!(!view.compare.open);
        assert!(view.compare.page.is_none());
        assert_eq!(view.session().tabs().len(), 1);
    }
}
