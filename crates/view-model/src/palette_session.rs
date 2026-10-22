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
                worker.stop();
                return Err(error);
            }
        };
        Ok(Self { model: Palette::new(limits.result_limit), worker, discovery: Some(discovery), cancel, report })
    }

    pub fn model(&self) -> &Palette {
        &self.model
    }

    /// `tabs` is rebuilt only on tab metadata changes, never per query key.
    pub fn set_tabs(&mut self, tabs: Vec<Tab>) -> Result<(), QueryError> {
        let request = self.model.set_tabs(tabs)?;
        if self.model.is_open() {
            self.submit(request)?;
        }
        Ok(())
    }

    pub fn open(&mut self, query: &str) -> Result<(), QueryError> {
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
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
