//! First-run policy and bounded, worker-owned completion marker.
//! Drawing and registration are driven by the GUI; this module never performs
//! an association operation merely because the welcome screen was displayed.
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::thread::{self, JoinHandle};

pub const BENEFITS: &[&str] = &[
    "Fast, focused text editing",
    "Open large files without loading the whole file into memory.",
    "Work in a desktop window or in your terminal.",
    "Keep your work with undo, atomic saves and recovery.",
];
pub const TECHNOLOGY: &[&str] = &[
    "Built for speed",
    "Persistent piece trees share unchanged text between edits.",
    "Sparse indexes and bounded workers process the ranges you need.",
    "Event-driven rendering redraws changed areas, with no idle frames.",
];

const MARKER: &[u8] = b"kwil-welcome-v1\n";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Waiting,
    About,
    FileTypes,
    Registering,
    Complete,
}

/// Continue offers file types before completion; Not now explicitly skips setup.
pub struct Model {
    stage: Stage,
    completion_pending: bool,
}

impl Default for Model {
    fn default() -> Self {
        Self { stage: Stage::Waiting, completion_pending: false }
    }
}

impl Model {
    pub fn stage(&self) -> Stage {
        self.stage
    }
    pub fn visible(&self) -> bool {
        matches!(self.stage, Stage::About | Stage::FileTypes | Stage::Registering)
    }
    pub fn ready(&mut self, show: bool) {
        // Late/repeated readiness cannot reopen a completed or active setup.
        if self.stage == Stage::Waiting {
            self.stage = if show { Stage::About } else { Stage::Complete };
        }
    }
    pub fn next(&mut self) {
        if self.stage == Stage::About {
            self.stage = Stage::FileTypes;
        }
    }
    pub fn back(&mut self) {
        if self.stage == Stage::FileTypes {
            self.stage = Stage::About;
        }
    }
    pub fn begin_registration(&mut self) -> bool {
        if self.stage != Stage::FileTypes {
            return false;
        }
        self.stage = Stage::Registering;
        true
    }
    pub fn registration_failed(&mut self) {
        if self.stage == Stage::Registering {
            self.stage = Stage::FileTypes;
        }
    }
    pub fn registration_finished(&mut self) {
        if self.stage == Stage::Registering {
            self.stage = Stage::Complete;
            self.completion_pending = true;
        }
    }
    pub fn skip_registration(&mut self) {
        if matches!(self.stage, Stage::About | Stage::FileTypes) {
            self.stage = Stage::Complete;
            self.completion_pending = true;
        }
    }
    pub fn persist_completion(&mut self, worker: &mut Worker) -> io::Result<()> {
        if !self.completion_pending {
            return Ok(());
        }
        worker.complete()?;
        self.completion_pending = false;
        Ok(())
    }
}

/// An invalid isolation override must not fall through to the owner's profile.
/// This function is called by the worker's path resolver, never on the UI.
pub fn marker_path(isolated: Option<&OsStr>, settings_path: &Path) -> io::Result<PathBuf> {
    let directory = if let Some(isolated) = isolated {
        let root = Path::new(isolated);
        if root.as_os_str().is_empty() || !root.is_absolute() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "welcome isolation path must be absolute"));
        }
        root.join(".kwil")
    } else {
        settings_path
            .parent()
            .filter(|p| p.is_absolute())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "settings path must have an absolute parent"))?
            .to_path_buf()
    };
    Ok(directory.join("welcome-v1.seen"))
}

fn seen(path: &Path) -> io::Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_file() || metadata.len() != MARKER.len() as u64 {
        return Ok(false);
    }
    let mut bytes = [0; MARKER.len() + 1];
    let mut file = File::open(path)?;
    let mut length = 0;
    while length < bytes.len() {
        let read = file.read(&mut bytes[length..])?;
        if read == 0 {
            break;
        }
        length += read;
    }
    Ok(&bytes[..length] == MARKER)
}

#[derive(Debug)]
pub struct Completion {
    pub result: io::Result<()>,
    /// An owned temporary file that could not be removed. Collection and any
    /// later retry belong on the shutdown/retirement lane, never the UI.
    pub retained_temporary: Option<PathBuf>,
}

fn write_marker(path: &Path) -> Completion {
    let mut temporary = None;
    let result = (|| {
        if seen(path)? {
            return Ok(());
        }
        let parent = path.parent().ok_or_else(|| io::Error::other("welcome marker has no parent"))?;
        fs::create_dir_all(parent)?;
        // Bounded collision handling; another process's file is never replaced.
        let mut file = None;
        for nonce in 0..8 {
            let candidate = parent.join(format!(".welcome-v1-{}-{nonce}.tmp", std::process::id()));
            match OpenOptions::new().write(true).create_new(true).open(&candidate) {
                Ok(opened) => {
                    temporary = Some(candidate);
                    file = Some(opened);
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        let mut file =
            file.ok_or_else(|| io::Error::new(io::ErrorKind::AlreadyExists, "welcome temporary slots occupied"))?;
        file.write_all(MARKER)?;
        file.sync_all()?;
        drop(file);
        match fs::hard_link(temporary.as_ref().unwrap(), path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists && seen(path)? => Ok(()),
            Err(error) => Err(error),
        }
    })();
    let retained_temporary = temporary.filter(|path| fs::remove_file(path).is_err());
    Completion { result, retained_temporary }
}

pub enum Report {
    Ready { show: bool, problem: Option<String> },
    Completed(Completion),
}

pub struct Worker {
    command: SyncSender<()>,
    reports: Receiver<Report>,
    thread: JoinHandle<()>,
    completion_admitted: bool,
}

pub struct Shutdown {
    thread: JoinHandle<()>,
    reports: Receiver<Report>,
}

pub struct ShutdownReport {
    pub worker_result: thread::Result<()>,
    pub reports: Vec<Report>,
}

impl Shutdown {
    /// Call only on the off-UI shutdown lane. Every pending completion,
    /// including an owned cleanup failure path, remains available to its owner.
    pub fn join(self) -> ShutdownReport {
        let worker_result = self.thread.join();
        let reports = self.reports.try_iter().collect();
        ShutdownReport { worker_result, reports }
    }
}

impl Worker {
    /// One worker and at most two reports per launch. `debug` bypasses all
    /// profile reads/writes so a debug test cannot consume the release welcome.
    pub fn start(
        debug: bool,
        resolve_path: impl FnOnce() -> io::Result<PathBuf> + Send + 'static,
        wake: impl Fn() + Send + 'static,
    ) -> io::Result<Self> {
        let (command, commands) = mpsc::sync_channel(1);
        let (reports_tx, reports) = mpsc::sync_channel(2);
        let thread = thread::Builder::new().name("kwil-welcome".into()).spawn(move || {
            let path = if debug { None } else { Some(resolve_path()) };
            let (show, problem) = match &path {
                None => (true, None),
                Some(Ok(path)) => match seen(path) {
                    Ok(seen) => (!seen, None),
                    Err(error) => (true, Some(error.to_string())),
                },
                Some(Err(error)) => (true, Some(error.to_string())),
            };
            if reports_tx.send(Report::Ready { show, problem }).is_err() {
                return;
            }
            wake();
            if !show || commands.recv().is_err() {
                return;
            }
            let completion = match path {
                None => Completion { result: Ok(()), retained_temporary: None },
                Some(Ok(path)) => write_marker(&path),
                Some(Err(error)) => Completion { result: Err(error), retained_temporary: None },
            };
            if reports_tx.send(Report::Completed(completion)).is_ok() {
                wake();
            }
        })?;
        Ok(Self { command, reports, thread, completion_admitted: false })
    }
    pub fn complete(&mut self) -> io::Result<()> {
        if self.completion_admitted {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "welcome completion already accepted"));
        }
        self.command.try_send(()).map_err(|error| match error {
            TrySendError::Full(()) => io::Error::new(io::ErrorKind::WouldBlock, "welcome completion already queued"),
            TrySendError::Disconnected(()) => io::Error::new(io::ErrorKind::BrokenPipe, "welcome worker closed"),
        })?;
        self.completion_admitted = true;
        Ok(())
    }
    pub fn take_report(&self) -> Result<Report, TryRecvError> {
        self.reports.try_recv()
    }
    /// Transfer this handle to the existing off-UI shutdown lane. Dropping the
    /// command sender releases a worker whose welcome was closed, not completed.
    pub fn stop(self) -> Shutdown {
        drop(self.command);
        Shutdown { thread: self.thread, reports: self.reports }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn continuing_about_offers_registration_before_completion() {
        let mut model = Model::default();
        model.ready(true);
        assert_eq!(model.stage(), Stage::About);
        model.next();
        assert_eq!(model.stage(), Stage::FileTypes);
        model.back();
        assert_eq!(model.stage(), Stage::About);
        model.next();
        assert!(model.begin_registration());
        model.skip_registration();
        assert_eq!(model.stage(), Stage::Registering);
        model.registration_failed();
        assert_eq!(model.stage(), Stage::FileTypes);
        assert!(model.begin_registration());
        model.registration_finished();
        assert_eq!(model.stage(), Stage::Complete);
        model.ready(true);
        assert_eq!(model.stage(), Stage::Complete);
    }

    #[test]
    fn explicit_skip_completes_without_registration() {
        let mut model = Model::default();
        model.ready(true);
        model.next();
        model.skip_registration();
        assert!(!model.visible());
        assert_eq!(model.stage(), Stage::Complete);
    }

    #[test]
    fn debug_worker_never_resolves_or_writes_profile() {
        let (wake, events) = mpsc::sync_channel(2);
        let mut worker = Worker::start(
            true,
            || panic!("debug must not touch profile"),
            move || {
                let _ = wake.try_send(());
            },
        )
        .unwrap();
        events.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(worker.take_report(), Ok(Report::Ready { show: true, problem: None })));
        worker.complete().unwrap();
        events.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(
            worker.take_report(),
            Ok(Report::Completed(Completion { result: Ok(()), retained_temporary: None }))
        ));
        worker.stop().join().worker_result.unwrap();
    }

    #[test]
    fn invalid_isolation_never_falls_through_to_settings_profile() {
        let settings = std::env::temp_dir().join("kwil-welcome-profile/settings.toml");
        for value in ["", "relative", "../escape"] {
            assert!(marker_path(Some(OsStr::new(value)), &settings).is_err());
        }
        let isolated = std::env::temp_dir().join("kwil-welcome-isolated");
        assert_eq!(marker_path(Some(isolated.as_os_str()), &settings).unwrap(), isolated.join(".kwil/welcome-v1.seen"));
    }

    struct Case(PathBuf);
    impl Case {
        fn new() -> Self {
            let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            let path = std::env::temp_dir().join(format!("kwil-welcome-{}-{nonce}", std::process::id()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn marker(&self) -> PathBuf {
            self.0.join("welcome-v1.seen")
        }
    }
    impl Drop for Case {
        fn drop(&mut self) {
            let _ = fs::remove_file(self.marker());
            let _ = fs::remove_dir(&self.0);
        }
    }

    #[test]
    fn normal_setup_persists_only_after_explicit_registration_skip() {
        let case = Case::new();
        let path = case.marker();
        let requested = path.clone();
        let (wake, events) = mpsc::sync_channel(2);
        let mut worker = Worker::start(
            false,
            move || Ok(requested),
            move || {
                let _ = wake.try_send(());
            },
        )
        .unwrap();
        events.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(worker.take_report(), Ok(Report::Ready { show: true, problem: None })));
        assert!(!path.exists(), "displaying first-run setup must not consume it");
        let mut model = Model::default();
        model.ready(true);
        model.next();
        model.persist_completion(&mut worker).unwrap();
        assert!(!path.exists(), "offering extensions is not explicit completion");
        model.skip_registration();
        model.persist_completion(&mut worker).unwrap();
        assert_eq!(worker.complete().unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        events.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(
            worker.take_report(),
            Ok(Report::Completed(Completion { result: Ok(()), retained_temporary: None }))
        ));
        worker.stop().join().worker_result.unwrap();
        assert_eq!(fs::read(&path).unwrap(), MARKER);
        let (wake, events) = mpsc::sync_channel(2);
        let worker = Worker::start(
            false,
            move || Ok(path),
            move || {
                let _ = wake.try_send(());
            },
        )
        .unwrap();
        events.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(worker.take_report(), Ok(Report::Ready { show: false, problem: None })));
        worker.stop().join().worker_result.unwrap();
    }

    #[test]
    fn closing_setup_preserves_first_run_and_pending_shutdown_reports() {
        let case = Case::new();
        let path = case.marker();
        let requested = path.clone();
        let (wake, events) = mpsc::sync_channel(2);
        let worker = Worker::start(
            false,
            move || Ok(requested),
            move || {
                let _ = wake.try_send(());
            },
        )
        .unwrap();
        events.recv_timeout(Duration::from_secs(2)).unwrap();
        let shutdown = worker.stop().join();
        shutdown.worker_result.unwrap();
        assert!(matches!(shutdown.reports.as_slice(), [Report::Ready { show: true, .. }]));
        assert!(!path.exists());
    }

    #[test]
    fn invalid_existing_marker_is_not_overwritten() {
        let case = Case::new();
        let path = case.marker();
        fs::write(&path, b"foreign marker").unwrap();
        assert!(!seen(&path).unwrap());
        let completion = write_marker(&path);
        assert_eq!(completion.result.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert!(completion.retained_temporary.is_none());
        assert_eq!(fs::read(&path).unwrap(), b"foreign marker");
        assert_eq!(fs::read_dir(&case.0).unwrap().count(), 1);
    }
}

#[cfg(test)]
mod skip_tests {
    use super::*;
    #[test]
    fn not_now_skips_about_and_late_ready_does_not_reopen() {
        let mut model = Model::default();
        model.ready(true);
        model.skip_registration();
        assert_eq!(model.stage(), Stage::Complete);
        assert!(!model.visible());
        assert!(model.completion_pending);
        model.ready(true);
        assert!(!model.visible());
    }
    #[test]
    fn skip_cannot_interrupt_an_accepted_registration() {
        let mut model = Model::default();
        model.ready(true);
        model.next();
        assert!(model.begin_registration());
        model.skip_registration();
        assert_eq!(model.stage(), Stage::Registering);
        assert!(!model.completion_pending);
    }
}
