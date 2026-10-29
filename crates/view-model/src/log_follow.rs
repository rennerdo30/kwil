//! One native file watch, owned and retired off the input thread. Invalidations
//! coalesce in one atomic flag; no polling, copied file data or event backlog.
use std::{
    io,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

pub struct FollowWatch {
    stop: Option<mpsc::SyncSender<PathBuf>>,
    worker: Option<thread::JoinHandle<()>>,
    changed: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
}
impl FollowWatch {
    pub fn start(path: PathBuf, wake: impl Fn() + Send + Sync + 'static) -> io::Result<Self> {
        let changed = Arc::new(AtomicBool::new(false));
        let error = Arc::new(Mutex::new(None));
        let initial = changed.clone();
        let flag = changed.clone();
        let failure = error.clone();
        let (stop, rx) = mpsc::sync_channel(1);
        let worker = thread::Builder::new().name("kwil-log-follow".into()).spawn(move || {
            let wake = Arc::new(wake);
            let mut path = path;
            loop {
                let notify = wake.clone();
                let failure_event = failure.clone();
                let flag = flag.clone();
                let watched = kwil_core_workspace::FileWatcher::new(&[path], Duration::from_millis(80), move |event| {
                    if let Err(e) = event {
                        *failure_event.lock().unwrap_or_else(|e| e.into_inner()) = Some(e.to_string());
                    }
                    if !flag.swap(true, Ordering::AcqRel) {
                        notify();
                    }
                });
                if let Err(e) = &watched {
                    *failure.lock().unwrap_or_else(|e| e.into_inner()) = Some(e.to_string());
                }
                initial.store(true, Ordering::Release);
                wake();
                let next = rx.recv();
                drop(watched);
                match next {
                    Ok(next) => path = next,
                    Err(_) => break,
                }
            }
        })?;
        Ok(Self { stop: Some(stop), worker: Some(worker), changed, error })
    }
    pub fn take_changed(&self) -> bool {
        self.changed.swap(false, Ordering::AcqRel)
    }
    pub fn take_error(&self) -> Option<String> {
        self.error.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
    pub fn watch(&self, path: PathBuf) -> io::Result<()> {
        self.stop
            .as_ref()
            .ok_or_else(|| io::Error::other("Log watcher stopped"))?
            .try_send(path)
            .map_err(|_| io::Error::other("Log watcher is changing files; try again"))
    }
    /// OFF UI, as part of the host's existing shutdown owner.
    pub fn stop(&mut self) {
        self.stop.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
impl Drop for FollowWatch {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn follow_watch_survives_append_truncation_and_rotation() {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "kwil-follow-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("app.log");
        std::fs::write(&path, "INFO start\n").unwrap();
        let (tx, rx) = mpsc::channel();
        let mut watch = FollowWatch::start(path.clone(), move || {
            let _ = tx.send(());
        })
        .unwrap();
        let wait = || {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())).unwrap();
                assert!(watch.take_error().is_none());
                if watch.take_changed() {
                    break;
                }
            }
        };
        wait();
        use std::io::Write;
        std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"ERROR appended\n").unwrap();
        wait();
        std::fs::write(&path, "WARN truncated\n").unwrap();
        wait();
        std::fs::rename(&path, dir.join("old.log")).unwrap();
        std::fs::write(&path, "INFO rotated\n").unwrap();
        wait();
        watch.stop();
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_file(dir.join("old.log")).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
}
