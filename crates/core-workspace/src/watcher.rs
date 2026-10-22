use notify::event::ModifyKind;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
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
    watcher: Option<RecommendedWatcher>,
    tx: SyncSender<Message>,
    worker: Option<JoinHandle<()>>,
}

impl FileWatcher {
    pub fn new(
        paths: &[PathBuf],
        debounce: Duration,
        mut callback: impl FnMut(io::Result<FileEvent>) + Send + 'static,
    ) -> io::Result<Self> {
        // Resolve parents so backend paths and caller paths share one spelling.
        // Do not resolve the file itself: a symlink/path replacement matters.
        let paths: BTreeSet<PathBuf> = paths.iter().map(|p| watch_path(p)).collect::<io::Result<_>>()?;
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
                        for change in changes(&event, &paths) {
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
        // End native callbacks first; send then wakes an otherwise idle worker.
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
    Ok(absolute.parent().expect("absolute file parent").canonicalize()?.join(name))
}

fn changes(event: &Event, watched: &BTreeSet<PathBuf>) -> Vec<FileEvent> {
    let kind = match event.kind {
        EventKind::Modify(ModifyKind::Name(_)) => FileEventKind::Renamed,
        EventKind::Remove(_) => FileEventKind::Removed,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Any | EventKind::Other => FileEventKind::Modified,
        _ => return Vec::new(), // In particular, reads must never trigger feedback.
    };
    event.paths.iter().filter(|p| watched.contains(*p)).map(|path| FileEvent { path: path.clone(), kind }).collect()
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
            let time = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
            let path = std::env::temp_dir().join(format!("kwil-watch-{}-{time}", std::process::id()));
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
}
