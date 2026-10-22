use notify::event::ModifyKind;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileEventKind {
    Modified,
    Removed,
    /// A native rename involved this watched path. Treat it as an invalidation;
    /// check file identity rather than assuming the backend paired both names.
    Renamed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEvent {
    pub path: PathBuf,
    pub kind: FileEventKind,
}

enum Message {
    Event(notify::Result<Event>),
    Stop,
}

/// Native, nonrecursive parent-directory watches survive rotation and atomic
/// saves. Events are invalidations, not an ordered journal of disk operations.
/// Call TextFile::check() before reading. Errors (including backend overflow)
/// require rechecking every watched path. No polling or idle timer is used.
///
/// The callback runs on a dedicated worker; it should only enqueue a result or
/// wake the frontend, and must return promptly. Dropping joins that worker.
pub struct FileWatcher {
    watcher: Option<Arc<Mutex<RecommendedWatcher>>>,
    tx: SyncSender<Message>,
    worker: Option<JoinHandle<()>>,
}

impl FileWatcher {
    pub fn new(
        paths: &[PathBuf],
        debounce: Duration,
        callback: impl FnMut(io::Result<FileEvent>) + Send + 'static,
    ) -> io::Result<Self> {
        Self::start(paths, false, debounce, callback)
    }

    /// Nonrecursive directory invalidations, including newly created files.
    /// The callback path is the stable directory path, not a journal of children.
    /// Parent watches keep a removed/recreated directory observable. Access-only
    /// events are ignored, and no idle polling is introduced.
    pub fn directory(
        path: &Path,
        debounce: Duration,
        callback: impl FnMut(io::Result<FileEvent>) + Send + 'static,
    ) -> io::Result<Self> {
        if !path.is_dir() || std::fs::symlink_metadata(path)?.file_type().is_symlink() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "watch an existing real directory"));
        }
        Self::start(&[path.canonicalize()?], true, debounce, callback)
    }

    fn start(
        paths: &[PathBuf],
        directories: bool,
        debounce: Duration,
        mut callback: impl FnMut(io::Result<FileEvent>) + Send + 'static,
    ) -> io::Result<Self> {
        // Resolve existing files as well as parents (including caller casing).
        // Keep the lexical path too so replacing a symlink still invalidates it.
        let mut watched = BTreeSet::new();
        for path in paths {
            watched.insert(watch_path(path)?);
            match path.canonicalize() {
                Ok(resolved) => {
                    watched.insert(resolved);
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        let paths = watched;
        let parents: BTreeSet<_> = paths.iter().filter_map(|p| p.parent().map(Path::to_path_buf)).collect();
        let (tx, rx) = mpsc::sync_channel(256);
        let overflow = Arc::new(AtomicBool::new(false));
        let events = tx.clone();
        let overrun = overflow.clone();
        let mut watcher = notify::recommended_watcher(move |event| {
            if let Err(mpsc::TrySendError::Full(_)) = events.try_send(Message::Event(event)) {
                overrun.store(true, Ordering::Release);
            }
        })
        .map_err(io::Error::other)?;
        for parent in parents {
            watcher.watch(&parent, RecursiveMode::NonRecursive).map_err(io::Error::other)?;
        }
        if directories {
            for path in &paths {
                watcher.watch(path, RecursiveMode::NonRecursive).map_err(io::Error::other)?;
            }
        }
        let watcher = Arc::new(Mutex::new(watcher));
        let rearm = watcher.clone();
        let worker = thread::Builder::new().name("kwil-file-watch".into()).spawn(move || {
            let mut pending = BTreeMap::<PathBuf, (Instant, FileEventKind)>::new();
            loop {
                let message = match pending.values().map(|(deadline, _)| *deadline).min() {
                    Some(deadline) => rx.recv_timeout(deadline.saturating_duration_since(Instant::now())),
                    None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
                };
                match message {
                    Ok(Message::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                    Ok(Message::Event(Ok(event))) => {
                        if event.need_rescan() {
                            callback(Err(io::Error::other("file watcher backend requires a rescan")));
                        }
                        if directories {
                            for path in &paths {
                                if event.paths.iter().any(|p| same_path(p, path)) {
                                    let mut watcher = rearm.lock().unwrap_or_else(|p| p.into_inner());
                                    let _ = watcher.unwatch(path);
                                    if std::fs::symlink_metadata(path)
                                        .is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink())
                                        && let Err(e) = watcher.watch(path, RecursiveMode::NonRecursive)
                                    {
                                        callback(Err(io::Error::other(e)));
                                    }
                                }
                            }
                        }
                        for change in changes_in(&event, &paths, directories) {
                            // Keep the first deadline: continuous appends still wake
                            // the frontend once per debounce period, never starve it.
                            pending
                                .entry(change.path)
                                .and_modify(|(_, k)| *k = change.kind)
                                .or_insert((Instant::now() + debounce, change.kind));
                        }
                    }
                    Ok(Message::Event(Err(e))) => callback(Err(io::Error::other(e))),
                    Err(RecvTimeoutError::Timeout) => {}
                }
                if overflow.swap(false, Ordering::AcqRel) {
                    callback(Err(io::Error::other("file watcher queue overflow; recheck all watched paths")));
                }
                let now = Instant::now();
                let ready: Vec<_> = pending.iter().filter(|(_, (d, _))| *d <= now).map(|(p, _)| p.clone()).collect();
                for path in ready {
                    let (_, kind) = pending.remove(&path).expect("pending path");
                    callback(Ok(FileEvent { path, kind }));
                }
            }
        })?;
        Ok(Self { watcher: Some(watcher), tx, worker: Some(worker) })
    }
}

impl Drop for FileWatcher {
    fn drop(&mut self) {
        // Stop wakes an otherwise idle worker; the final shared watcher handle
        // is dropped as the worker exits, ending native callbacks as well.
        drop(self.watcher.take());
        let _ = self.tx.send(Message::Stop);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn watch_path(path: &Path) -> io::Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    let name = absolute.file_name().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "watch a file path"))?;
    // Resolve existing file casing; preserve symlink replacement detection.
    if std::fs::symlink_metadata(&absolute).is_ok_and(|m| !m.file_type().is_symlink()) {
        return absolute.canonicalize();
    }
    Ok(absolute.parent().expect("absolute file parent").canonicalize()?.join(name))
}

#[cfg(test)]
fn changes(event: &Event, watched: &BTreeSet<PathBuf>) -> Vec<FileEvent> {
    changes_in(event, watched, false)
}

fn changes_in(event: &Event, watched: &BTreeSet<PathBuf>, directories: bool) -> Vec<FileEvent> {
    let kind = match event.kind {
        EventKind::Modify(ModifyKind::Name(_)) => FileEventKind::Renamed,
        EventKind::Remove(_) => FileEventKind::Removed,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Any | EventKind::Other => FileEventKind::Modified,
        _ => return Vec::new(), // In particular, reads must never trigger feedback.
    };
    // Return the stable watched spelling, even for a removed or renamed file
    // that can no longer be canonicalised after rotation.
    watched
        .iter()
        .filter(|path| {
            event
                .paths
                .iter()
                .any(|p| same_path(p, path) || directories && p.parent().is_some_and(|parent| same_path(parent, path)))
        })
        .map(|path| FileEvent { path: path.clone(), kind })
        .collect()
}

fn same_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    #[cfg(any(windows, target_os = "macos"))]
    {
        // Windows and default macOS volumes ignore case. Keep matching after
        // rotation, when canonicalising the event's old path is impossible.
        a.as_os_str().to_string_lossy().to_lowercase() == b.as_os_str().to_string_lossy().to_lowercase()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, CreateKind, DataChange, RemoveKind, RenameMode};
    use std::io::Write;
    use std::time::SystemTime;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            // Tests run in parallel and the clock can be coarse (microseconds
            // on macOS), so a per-process counter keeps every folder unique.
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let time = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
            let path = std::env::temp_dir().join(format!("kwil-watch-{}-{n}-{time}", std::process::id()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn watcher_reports_modification_after_rotation() {
        let dir = TempDir::new();
        let path = dir.0.join("log");
        std::fs::write(&path, b"one\n").unwrap();
        let expected = watch_path(&path).unwrap();
        let (tx, rx) = mpsc::channel();
        let watcher = FileWatcher::new(std::slice::from_ref(&path), Duration::from_millis(20), move |event| {
            let _ = tx.send(event);
        })
        .unwrap();
        let wait_modified = || {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let event = rx
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .expect("bounded wait for modification")
                    .unwrap();
                assert_eq!(event.path, expected);
                if event.kind == FileEventKind::Modified {
                    break;
                }
            }
        };
        let append = || {
            let mut writer = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
            writer.write_all(b"two\n").unwrap();
            writer.sync_all().unwrap();
        };
        append();
        wait_modified();
        std::fs::rename(&path, dir.0.join("old")).unwrap();
        std::fs::write(&path, b"new\n").unwrap();
        wait_modified();
        append();
        wait_modified();
        drop(watcher); // Joins without needing another disk event or timer.
    }

    #[test]
    fn watcher_matches_case_variants_after_removal() {
        let dir = TempDir::new();
        let path = watch_path(&dir.0.join("mixed.log")).unwrap();
        let event = Event::new(EventKind::Remove(RemoveKind::File)).add_path(path.with_file_name("MIXED.LOG"));
        let result = changes(&event, &BTreeSet::from([path.clone()]));
        #[cfg(any(windows, target_os = "macos"))]
        assert_eq!(result, vec![FileEvent { path, kind: FileEventKind::Removed }]);
        #[cfg(not(any(windows, target_os = "macos")))]
        assert!(result.is_empty(), "case-sensitive platforms must keep distinct file names");
    }

    #[test]
    fn watcher_accepts_existing_file_with_different_case() {
        let dir = TempDir::new();
        let path = dir.0.join("mixed.log");
        std::fs::write(&path, b"one\n").unwrap();
        let requested = path.with_file_name("MIXED.LOG");
        if !requested.exists() {
            eprintln!("skipped: fixture filesystem is case-sensitive");
            return;
        }
        assert_eq!(watch_path(&requested).unwrap(), path.canonicalize().unwrap());
        let (tx, rx) = mpsc::channel();
        let watcher = FileWatcher::new(&[requested], Duration::from_millis(20), move |event| {
            let _ = tx.send(event);
        })
        .unwrap();
        let append = || {
            let mut writer = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
            writer.write_all(b"two\n").unwrap();
            writer.sync_all().unwrap();
        };
        let wait_modified = || {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let event = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap().unwrap();
                if event.kind == FileEventKind::Modified {
                    break;
                }
            }
        };
        append();
        wait_modified();
        std::fs::rename(&path, dir.0.join("old")).unwrap();
        std::fs::write(&path, b"new\n").unwrap();
        wait_modified();
        append();
        wait_modified();
        drop(watcher);
    }

    #[test]
    fn watcher_classifies_invalidations_and_filters_unwatched_paths() {
        let dir = TempDir::new();
        let path = watch_path(&dir.0.join("log")).unwrap();
        let other = dir.0.join("unrelated");
        let watched = BTreeSet::from([path.clone()]);
        for (kind, expected) in [
            (EventKind::Modify(ModifyKind::Data(DataChange::Any)), Some(FileEventKind::Modified)),
            (EventKind::Create(CreateKind::File), Some(FileEventKind::Modified)),
            (EventKind::Remove(RemoveKind::File), Some(FileEventKind::Removed)),
            (EventKind::Modify(ModifyKind::Name(RenameMode::Both)), Some(FileEventKind::Renamed)),
            (EventKind::Access(AccessKind::Any), None),
        ] {
            let event = Event::new(kind).add_path(other.clone()).add_path(path.clone());
            assert_eq!(
                changes(&event, &watched),
                expected.map(|kind| FileEvent { path: path.clone(), kind }).into_iter().collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn watcher_directory_child_contract_preserves_exact_file_filter() {
        let dir = TempDir::new();
        let root = dir.0.canonicalize().unwrap();
        let child = root.join("new.toml");
        let watched = BTreeSet::from([root.clone()]);
        for (kind, expected) in [
            (EventKind::Create(CreateKind::File), FileEventKind::Modified),
            (EventKind::Modify(ModifyKind::Data(DataChange::Any)), FileEventKind::Modified),
            (EventKind::Remove(RemoveKind::File), FileEventKind::Removed),
            (EventKind::Modify(ModifyKind::Name(RenameMode::Both)), FileEventKind::Renamed),
        ] {
            let event = Event::new(kind).add_path(child.clone());
            assert_eq!(changes_in(&event, &watched, true), vec![FileEvent { path: root.clone(), kind: expected }]);
            assert!(changes_in(&event, &watched, false).is_empty(), "existing exact file semantics changed");
        }
        let nested = Event::new(EventKind::Create(CreateKind::File)).add_path(root.join("sub/child.toml"));
        assert!(changes_in(&nested, &watched, true).is_empty(), "folder watch must be nonrecursive");
        let access = Event::new(EventKind::Access(AccessKind::Any)).add_path(child);
        assert!(changes_in(&access, &watched, true).is_empty(), "reading themes must not feed back");
    }

    #[test]
    fn watcher_directory_native_create_rename_remove_contract() {
        let dir = TempDir::new();
        let root = dir.0.canonicalize().unwrap();
        let (tx, rx) = mpsc::channel();
        let watcher = FileWatcher::directory(&root, Duration::from_millis(10), move |e| {
            let _ = tx.send(e);
        })
        .unwrap();
        let child = root.join("new.toml");
        fs_write_fixture(&child);
        let event = rx.recv_timeout(Duration::from_secs(5)).expect("directory create wake").unwrap();
        assert_eq!(event.path, root);
        while rx.try_recv().is_ok() {}
        let renamed = root.join("renamed.toml");
        std::fs::rename(&child, &renamed).unwrap();
        rx.recv_timeout(Duration::from_secs(5)).expect("directory rename wake").unwrap();
        while rx.try_recv().is_ok() {}
        std::fs::remove_file(renamed).unwrap();
        rx.recv_timeout(Duration::from_secs(5)).expect("directory remove wake").unwrap();
        drop(watcher);
    }
    fn fs_write_fixture(path: &Path) {
        std::fs::write(path, b"fixture").unwrap();
    }
}
