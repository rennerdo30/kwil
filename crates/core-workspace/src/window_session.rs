//! Worker-only per-window storage. The primary keeps the legacy root in place;
//! secondary windows have disjoint document IDs/journals under `windows/`.
use std::{
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
};

const MAX_WINDOWS: usize = 128;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum WindowSession {
    #[default]
    Primary,
    New,
    Restore(String),
}

pub fn valid_window_id(id: &str) -> bool {
    id.starts_with("window-")
        && id.len() <= 96
        && id[7..].bytes().all(|c| c.is_ascii_digit() || c == b'-')
        && id.len() > 7
}

pub struct WindowClaim {
    root: PathBuf,
    ownership: File,
}
impl WindowClaim {
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn into_parts(self) -> (PathBuf, File) {
        (self.root, self.ownership)
    }
}

fn private_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().recursive(true).mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    fs::create_dir_all(path)
}

fn lock(root: &Path) -> io::Result<File> {
    let ownership =
        OpenOptions::new().read(true).write(true).create(true).truncate(false).open(root.join("workspace.lock"))?;
    ownership.try_lock().map_err(|error| match error {
        std::fs::TryLockError::WouldBlock => io::Error::from(io::ErrorKind::WouldBlock),
        std::fs::TryLockError::Error(error) => error,
    })?;
    Ok(ownership)
}

/// Claims ownership before metadata or journals may be read, replaced or resumed.
/// Existing primary data is used in place; no migration moves or deletes it.
pub fn claim_window(base: &Path, window: &WindowSession) -> io::Result<WindowClaim> {
    let window = if matches!(window, WindowSession::New) { &WindowSession::Primary } else { window };
    if !base.is_absolute() {
        return Err(io::Error::other("Recovery directory must be absolute"));
    }
    private_directory(base)?;
    let root = match window {
        WindowSession::Primary => base.to_owned(),
        WindowSession::New => {
            let parent = base.join("windows");
            private_directory(&parent)?;
            // Bound enumeration; never evict an old window to make room.
            if window_ids(base)?.len() >= MAX_WINDOWS {
                return Err(io::Error::other("Saved window limit reached"));
            }
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(io::Error::other)?
                .as_nanos();
            let id = format!(
                "window-{nonce}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            );
            let root = parent.join(id);
            // create_new semantics: never adopt a pre-existing window.
            fs::create_dir(&root)?;
            root
        }
        WindowSession::Restore(id) => {
            if !valid_window_id(id) {
                return Err(io::Error::other("Invalid saved window identity"));
            }
            let root = base.join("windows").join(id);
            if !fs::symlink_metadata(&root)?.file_type().is_dir() {
                return Err(io::Error::other("Invalid saved window directory"));
            }
            root
        }
    };
    let ownership = lock(&root)?;
    Ok(WindowClaim { root, ownership })
}

fn window_ids(base: &Path) -> io::Result<Vec<String>> {
    let entries = match fs::read_dir(base.join("windows")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut ids = Vec::new();
    for entry in entries {
        let entry = entry?;
        let Some(id) = entry.file_name().to_str().map(str::to_owned) else { continue };
        if !valid_window_id(&id) || !entry.file_type()?.is_dir() {
            continue;
        }
        if ids.len() == MAX_WINDOWS {
            return Err(io::Error::other("Saved window limit reached"));
        }
        ids.push(id);
    }
    ids.sort();
    Ok(ids)
}

/// Only the primary claimant enumerates secondary windows. Live windows are
/// skipped; the child must still claim its own lock before touching recovery.
pub fn restorable_windows(primary: &WindowClaim) -> io::Result<Vec<String>> {
    let mut ids = Vec::new();
    for id in window_ids(primary.root())? {
        match lock(&primary.root.join("windows").join(&id)) {
            Ok(ownership) => {
                drop(ownership);
                ids.push(id);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
    }
    Ok(ids)
}
