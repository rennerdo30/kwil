//! GUI/TUI lifecycle adapter for the asynchronous command and file palette.
//!
//! Construct once after the first frame with a captured workspace root. Input
//! methods only edit bounded query state and submit latest-wins requests. The
//! wake callback posts an event; the host calls `worker_ready` on that event.
use kwil_core_commands::palette::{
    IndexLimits, Palette, PaletteAction, PaletteMode, PaletteQuery, PaletteWorker, QueryError, Tab,
};
use kwil_core_workspace::catalogue::{self, Cancellation, WalkLimits, WalkReport};
use std::{
    io,
    ops::ControlFlow,
    path::PathBuf,
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
};

pub enum PaletteInput<'a> {
    Text(&'a str),
    Backspace,
    Delete,
    Left,
    Right,
    Home,
    End,
    Next,
    Previous,
    Accept,
    Cancel,
}

pub struct PaletteSession {
    tabs: Vec<Tab>,
    input_epoch: u64,
    model: Palette,
    worker: PaletteWorker,
    discovery: Option<JoinHandle<()>>,
    cancel: Cancellation,
    report: Arc<Mutex<Option<WalkReport>>>,
}

impl PaletteSession {
    pub fn new(root: PathBuf, wake: Arc<dyn Fn() + Send + Sync>) -> io::Result<Self> {
        if !root.is_absolute() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "palette root must be absolute"));
        }
        let limits = IndexLimits::default();
        let mut worker = PaletteWorker::new(limits, wake.clone())?;
        let mut feed = worker.feed();
        let cancel = Cancellation::default();
        let discovery_cancel = cancel.clone();
        let report = Arc::new(Mutex::new(None));
        let discovery_report = report.clone();
        let discovery = match thread::Builder::new().name("kwil-catalogue".into()).spawn(move || {
            let result = catalogue::walk(&root, WalkLimits::default(), &discovery_cancel, |path| {
                let label = path.strip_prefix(&root).unwrap_or(&path).to_string_lossy().into_owned();
                if feed.push(path, label.into()).is_ok() { ControlFlow::Continue(()) } else { ControlFlow::Break(()) }
            });
            *discovery_report.lock().unwrap() = Some(result);
            wake();
        }) {
            Ok(handle) => handle,
            Err(error) => {
                // No input/event loop exists yet. Signal the worker; do not join
                // it on the caller's input path, including construction failure.
                let _ = worker.stop();
                return Err(error);
            }
        };
        Ok(Self {
            tabs: Vec::new(),
            input_epoch: 0,
            model: Palette::new(limits.result_limit),
            worker,
            discovery: Some(discovery),
            cancel,
            report,
        })
    }

    pub fn model(&self) -> &Palette {
        &self.model
    }

    /// Replace the catalogue after Open Folder. Old discovery/query workers
    /// are cancelled immediately and joined away from the input thread.
    pub fn set_root(&mut self, root: PathBuf, wake: Arc<dyn Fn() + Send + Sync>) -> io::Result<()> {
        let mut next = Self::new(root, wake)?;
        next.set_tabs(self.tabs.clone()).map_err(|error| io::Error::other(format!("{error:?}")))?;
        next.input_epoch = self.input_epoch.checked_add(1).expect("palette input identity exhausted");
        let handles = self.stop();
        std::thread::Builder::new().name("kwil-catalogue-cleanup".into()).spawn(move || {
            for handle in handles {
                let _ = handle.join();
            }
        })?;
        *self = next;
        Ok(())
    }

    pub fn input_epoch(&self) -> u64 {
        self.input_epoch
    }

    /// `tabs` is rebuilt only on tab metadata changes, never per query key.
    pub fn set_tabs(&mut self, tabs: Vec<Tab>) -> Result<(), QueryError> {
        if self.tabs == tabs {
            return Ok(());
        }
        let request = self.model.set_tabs(tabs.clone())?;
        self.tabs = tabs;
        if let Some(request) = request {
            self.worker.request(request)?;
        }
        Ok(())
    }

    /// Call after addressed document/file events. Unchanged metadata does not
    /// restart matching or invalidate fresh rows.
    pub fn refresh_tabs(&mut self, workspace: &crate::Workspace) -> Result<bool, QueryError> {
        let tabs: Vec<_> = workspace
            .session()
            .tabs()
            .iter()
            .map(|tab| Tab { id: tab.id.0, title: workspace.tab_title(tab.id) })
            .collect();
        if self.tabs == tabs {
            return Ok(false);
        }
        self.set_tabs(tabs)?;
        Ok(true)
    }

    pub fn open(&mut self, query: &str) -> Result<(), QueryError> {
        self.input_epoch = self.input_epoch.checked_add(1).expect("palette input identity exhausted");
        for mode in [PaletteMode::Commands, PaletteMode::Tabs, PaletteMode::Files] {
            self.model.prime_results(self.worker.cached(mode));
        }
        let request = self.model.open(query)?;
        self.submit(request)
    }

    pub fn input(&mut self, input: PaletteInput<'_>) -> Result<Option<PaletteAction>, QueryError> {
        if !self.model.is_open() {
            return Ok(None);
        }
        self.input_epoch = self.input_epoch.checked_add(1).expect("palette input identity exhausted");
        let request = match input {
            PaletteInput::Text(text) => self.model.insert(text)?,
            PaletteInput::Backspace => self.model.backspace(),
            PaletteInput::Delete => self.model.delete_forward(),
            PaletteInput::Left => {
                self.model.move_left();
                return Ok(None);
            }
            PaletteInput::Right => {
                self.model.move_right();
                return Ok(None);
            }
            PaletteInput::Home => {
                self.model.move_home();
                return Ok(None);
            }
            PaletteInput::End => {
                self.model.move_end();
                return Ok(None);
            }
            PaletteInput::Next => {
                self.model.next();
                return Ok(None);
            }
            PaletteInput::Previous => {
                self.model.previous();
                return Ok(None);
            }
            PaletteInput::Accept => {
                let action = self.model.accept();
                if action.is_some() {
                    self.worker.cancel_query();
                }
                return Ok(action);
            }
            PaletteInput::Cancel => {
                self.model.cancel();
                self.worker.cancel_query();
                return Ok(None);
            }
        };
        self.submit(request)?;
        Ok(None)
    }

    fn submit(&self, request: Option<PaletteQuery>) -> Result<(), QueryError> {
        if let Some(request) = request {
            self.worker.request(request)?;
        } else {
            self.worker.cancel_query();
        }
        Ok(())
    }

    /// Returns true only when accepted results change the visible open palette.
    pub fn worker_ready(&mut self) -> bool {
        let result = self.worker.latest();
        if self.model.is_open() {
            self.model.apply_results(result)
        } else {
            for mode in [PaletteMode::Commands, PaletteMode::Tabs, PaletteMode::Files] {
                self.model.prime_results(self.worker.cached(mode));
            }
            false
        }
    }

    pub fn take_discovery_report(&self) -> Option<WalkReport> {
        self.report.lock().unwrap().take()
    }

    /// Transfer these handles to a shutdown worker. Never join on the input thread.
    pub fn stop(&mut self) -> Vec<JoinHandle<()>> {
        self.cancel.cancel();
        self.worker.cancel(); // releases a discovery feed blocked on capacity
        let mut handles = Vec::with_capacity(2);
        if let Some(handle) = self.discovery.take() {
            handles.push(handle);
        }
        if let Some(handle) = self.worker.stop() {
            handles.push(handle);
        }
        handles
    }
}

impl Drop for PaletteSession {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn file_commands_folder_switch_replaces_the_finder_catalogue() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "kwil-folder-picker-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        for folder in ["first", "second"] {
            std::fs::create_dir_all(root.join(folder)).unwrap();
        }
        std::fs::write(root.join("first/alpha.txt"), b"alpha").unwrap();
        std::fs::write(root.join("second/beta.txt"), b"beta").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            let _ = tx.send(());
        });
        let mut host = PaletteSession::new(root.join("first"), wake.clone()).unwrap();
        let wait = |host: &mut PaletteSession, name: &str| {
            host.open("").unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                host.worker_ready();
                if host.model().results_are_fresh()
                    && host.model().rows().iter().any(
                        |row| matches!(&row.action, PaletteAction::OpenFile(path) if path.file_name().unwrap() == name),
                    )
                {
                    break;
                }
                rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())).unwrap();
            }
        };
        wait(&mut host, "alpha.txt");
        host.set_root(root.join("second"), wake).unwrap();
        wait(&mut host, "beta.txt");
        assert!(host.model().rows().iter().all(
            |row| !matches!(&row.action, PaletteAction::OpenFile(path) if path.file_name().unwrap() == "alpha.txt")
        ));
        finish(host);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn tab_metadata_refresh_preserves_a_pending_non_tab_query() {
        use std::sync::{Condvar, Mutex, mpsc};
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        struct Release(Arc<(Mutex<bool>, Condvar)>);
        impl Drop for Release {
            fn drop(&mut self) {
                *self.0.0.lock().unwrap() = true;
                self.0.1.notify_all();
            }
        }
        let release = Release(gate.clone());
        let (sender, receiver) = mpsc::channel();
        let worker_gate = gate.clone();
        let mut session = PaletteSession::new(
            std::env::temp_dir().join("kwil-no-pending-query-catalogue"),
            Arc::new(move || {
                if std::thread::current().name() != Some("kwil-palette") {
                    return;
                }
                let _ = sender.send(());
                let (mutex, ready) = &*worker_gate;
                let guard = mutex.lock().unwrap();
                let _guard = ready.wait_while(guard, |released| !*released).unwrap();
            }),
        )
        .unwrap();
        receiver.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        session.open(">save").unwrap();
        session.set_tabs(vec![Tab { id: 99, title: "new external tab".into() }]).unwrap();
        // Release the worker only after the metadata change: the query was
        // deliberately pending throughout that update, without timing races.
        drop(release);
        session.worker_ready();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !session.model().results_are_fresh() {
            assert!(
                receiver.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())).is_ok(),
                "assertion: pending command query was cancelled by tab metadata"
            );
            session.worker_ready();
        }
        assert!(session.model().rows().iter().any(|row| row.action == PaletteAction::Command("file.save")));
        finish(session);
    }

    #[test]
    fn tab_snapshot_replacement_invalidates_acceptance_and_publishes_new_tabs() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let mut session = PaletteSession::new(
            std::env::temp_dir().join("kwil-no-catalogue-required"),
            Arc::new(move || {
                let _ = sender.send(());
            }),
        )
        .unwrap();
        session.set_tabs(vec![Tab { id: 1, title: "alpha".into() }]).unwrap();
        session.open("#").unwrap();
        let wait = |session: &mut PaletteSession| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !session.model().results_are_fresh() {
                receiver.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())).unwrap();
                session.worker_ready();
            }
        };
        wait(&mut session);
        assert_eq!(session.model().rows().len(), 1);
        session.set_tabs(vec![Tab { id: 1, title: "alpha".into() }, Tab { id: 2, title: "beta".into() }]).unwrap();
        assert!(!session.model().results_are_fresh());
        assert_eq!(session.input(PaletteInput::Accept).unwrap(), None);
        wait(&mut session);
        assert!(session.model().rows().iter().any(|row| row.action == PaletteAction::ActivateTab(2)));
        finish(session);
    }

    fn session() -> PaletteSession {
        let root = std::env::temp_dir().join(format!("kwil-palette-host-absent-{}", std::process::id()));
        PaletteSession::new(root, Arc::new(|| {})).unwrap()
    }

    fn finish(mut session: PaletteSession) {
        for handle in session.stop() {
            handle.join().unwrap();
        }
        assert!(session.stop().is_empty());
    }

    #[test]
    fn location_accept_is_typed_and_closes_without_document_input() {
        let mut session = session();
        session.open(":12:3").unwrap();
        assert!(session.model().is_open());
        assert_eq!(session.input(PaletteInput::Accept).unwrap(), Some(PaletteAction::GoTo { line: 12, column: 3 }));
        assert!(!session.model().is_open());
        let query = session.model().query().to_owned();
        assert_eq!(session.input(PaletteInput::Text("ignored")).unwrap(), None);
        assert_eq!(session.model().query(), query);
        finish(session);
    }

    #[test]
    fn cached_commands_cannot_be_accepted_before_current_worker_result() {
        let mut session = session();
        session.open(">save").unwrap();
        assert_eq!(session.input(PaletteInput::Accept).unwrap(), None);
        assert!(session.model().is_open());
        session.input(PaletteInput::Cancel).unwrap();
        assert!(!session.worker_ready());
        assert!(!session.model().is_open());
        finish(session);
    }

    #[test]
    fn query_editing_uses_grapheme_boundaries() {
        let mut session = session();
        session.open("e\u{301}界").unwrap();
        session.input(PaletteInput::Backspace).unwrap();
        assert_eq!(session.model().query(), "e\u{301}");
        session.input(PaletteInput::Left).unwrap();
        assert_eq!(session.model().cursor(), 0);
        session.input(PaletteInput::Delete).unwrap();
        assert_eq!(session.model().query(), "");
        finish(session);
    }
}

/// Escape control characters in path labels and pasted query text. Native paths
/// remain in typed actions; this representation is display-only.
pub fn display_text(text: &str) -> String {
    let mut display = String::with_capacity(text.len());
    for character in text.chars() {
        if character.is_control() {
            display.extend(character.escape_default());
        } else {
            display.push(character);
        }
    }
    display
}

pub struct QueryWindow {
    pub text: String,
    /// Display cells from the start of `text`; always inside a nonempty viewport.
    pub cursor: usize,
}

/// Bound layout by the palette's 4 KiB query cap, with grapheme-safe horizontal
/// scrolling. Both frontends use this to keep the input caret visible.
pub fn query_window(palette: &Palette, columns: usize) -> QueryWindow {
    text_window(palette.query(), palette.cursor(), columns)
}

/// Grapheme-safe single-line viewport for caller-bounded palette/settings input.
pub fn text_window(input: &str, input_cursor: usize, columns: usize) -> QueryWindow {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    if columns == 0 {
        return QueryWindow { text: String::new(), cursor: 0 };
    }
    let mut cells = Vec::new();
    let mut caret = 0;
    for (byte, cluster) in input.grapheme_indices(true) {
        if byte < input_cursor {
            caret += 1;
        }
        let display = display_text(cluster);
        let width = display.width().max(1);
        cells.push((display, width));
    }
    let mut first = 0;
    let mut cursor: usize = cells[..caret].iter().map(|(_, width)| width).sum();
    while cursor >= columns && first < caret {
        cursor -= cells[first].1;
        first += 1;
    }
    let mut text = String::new();
    let mut used: usize = 0;
    for (display, width) in &cells[first..] {
        if *width > columns.saturating_sub(used) {
            break;
        }
        text.push_str(display);
        used += width;
    }
    QueryWindow { text, cursor }
}

#[cfg(test)]
mod display_tests {
    use super::*;

    #[test]
    fn query_window_keeps_unicode_caret_visible_and_escapes_controls() {
        let mut palette = Palette::new(10);
        palette.open("a界e\u{301}\nZ").unwrap();
        let window = query_window(&palette, 5);
        assert_eq!(window.text, "e\u{301}\\nZ");
        assert_eq!(window.cursor, 4);
        palette.move_left();
        let window = query_window(&palette, 3);
        assert_eq!(window.text, "\\nZ");
        assert_eq!(window.cursor, 2);
        assert_eq!(display_text("a\t\r\nb"), "a\\t\\r\\nb");
        assert!(query_window(&palette, 0).text.is_empty());
    }
}
