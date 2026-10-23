//! Pin an original file object, never a full copy. Unix keeps its old inode;
//! Windows renames the share-delete handle's object before installing the new
//! file. Marker identity checks make orphan cleanup conservative.
use super::*;
use std::fs;
#[cfg(windows)]
use std::{io::Write, sync::atomic::AtomicU64};

pub(super) struct SaveBacking {
    backup: Option<PathBuf>,
    marker: Option<PathBuf>,
    delete_on_drop: bool,
    #[cfg(windows)]
    attributes: u32,
}
impl Drop for SaveBacking {
    fn drop(&mut self) {
        if self.delete_on_drop {
            let removed = self.backup.as_ref().is_none_or(|path| match fs::remove_file(path) {
                Ok(()) => true,
                Err(e) => e.kind() == io::ErrorKind::NotFound,
            });
            if removed && let Some(path) = &self.marker {
                let _ = fs::remove_file(path);
            }
        }
    }
}

/// A save pins before the final replace and commits immediately after it.
/// Dropping an uncommitted pin restores the original Windows path. A failed
/// restoration retains the backup and marker for recovery, never deletes it.
pub struct OriginalSavePin {
    file: TextFile,
    owns: bool,
    complete: bool,
}
impl OriginalSavePin {
    pub fn commit(mut self) {
        if self.owns
            && let Ok(mut guard) = self.file.shared.save_backing.lock()
            && let Some(backing) = guard.as_mut()
        {
            backing.delete_on_drop = true;
            #[cfg(windows)]
            if let Some(path) = &backing.backup {
                use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_HIDDEN, SetFileAttributesW};
                // SAFETY: the completed OS replacement owns this old backing.
                let _ = unsafe { SetFileAttributesW(wide(path).as_ptr(), backing.attributes | FILE_ATTRIBUTE_HIDDEN) };
            }
        }
        self.complete = true;
    }
    pub fn rollback(mut self) -> io::Result<()> {
        let result = self.restore();
        self.complete = true;
        result
    }
    /// The reserved name is created only by ReplaceFileW, never by renaming
    /// away the destination in application code.
    pub fn backup_path(&self) -> Option<PathBuf> {
        self.file.shared.save_backing.lock().ok()?.as_ref()?.backup.clone()
    }
    fn restore(&self) -> io::Result<()> {
        if !self.owns {
            return Ok(());
        }
        let mut guard = self.file.shared.save_backing.lock().map_err(|_| io::Error::other("save pin lock poisoned"))?;
        #[cfg(windows)]
        if let Some(backing) = guard.as_ref()
            && let Some(path) = &backing.backup
        {
            if path.exists() {
                if self.file.shared.path.exists() {
                    return Err(io::Error::other(
                        "replacement left both destination and recovery backing; preserve backing for comparison",
                    ));
                }
                // Recovery from an OS-level partial failure, never the normal
                // save path. Refuse to overwrite a concurrent destination.
                fs::rename(path, &self.file.shared.path)?;
            }
            use windows_sys::Win32::Storage::FileSystem::SetFileAttributesW;
            // SAFETY: a terminated OS path, no pointer retained by Windows.
            let _ = unsafe { SetFileAttributesW(wide(&self.file.shared.path).as_ptr(), backing.attributes) };
            if let Some(marker) = &backing.marker {
                let _ = fs::remove_file(marker);
            }
        }
        *guard = None;
        self.file.shared.read_generation.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}
impl Drop for OriginalSavePin {
    fn drop(&mut self) {
        if !self.complete {
            let _ = self.restore();
        }
    }
}

impl TextFile {
    /// Worker-only seam for atomic save. All clones, history roots and issued
    /// snapshots share the pin. External mutations are still checked against
    /// the live object's identity/size/mtime before exposing mapped bytes.
    pub fn pin_original_for_save(&self) -> io::Result<OriginalSavePin> {
        let mut guard = self.shared.save_backing.lock().map_err(|_| io::Error::other("save pin lock poisoned"))?;
        if guard.is_some() {
            return Err(io::Error::other("original is already pinned; reopen the current saved path"));
        }
        let now = File::open(&self.shared.path)?;
        if self.shared.stamp.state(&Stamp::read(&now)?) != FileState::Unchanged {
            return Err(io::Error::other("file changed on disk before save; reload or compare first"));
        }
        #[cfg(windows)]
        let backing = {
            use windows_sys::Win32::Storage::FileSystem::{GetFileAttributesW, INVALID_FILE_ATTRIBUTES};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let parent = self.shared.path.parent().ok_or_else(|| io::Error::other("save path has no parent"))?;
            // SAFETY: valid terminated OS path.
            let attributes = unsafe { GetFileAttributesW(wide(&self.shared.path).as_ptr()) };
            if attributes == INVALID_FILE_ATTRIBUTES {
                return Err(io::Error::last_os_error());
            }
            let (backup, marker) = loop {
                let id = NEXT.fetch_add(1, Ordering::Relaxed);
                let name = format!(".kwil-old-{}-{id}", std::process::id());
                let backup = parent.join(&name);
                let marker = parent.join(format!("{name}.owner"));
                if backup.exists() {
                    continue;
                }
                match fs::OpenOptions::new().write(true).create_new(true).open(&marker) {
                    Ok(mut file) => {
                        let (a, b) = self.shared.stamp.identity;
                        use std::os::windows::ffi::OsStrExt;
                        let original_name: String = self
                            .shared
                            .path
                            .file_name()
                            .unwrap()
                            .encode_wide()
                            .map(|unit| format!("{unit:04x}"))
                            .collect();
                        if let Err(error) =
                            write!(file, "KWIL_SAVE_BACKING_V1\n{}\n{a}\n{b}\n{original_name}\n", std::process::id())
                                .and_then(|_| file.sync_all())
                        {
                            drop(file);
                            let _ = fs::remove_file(&marker);
                            return Err(error);
                        }
                        break (backup, marker);
                    }
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(e) => return Err(e),
                }
            };
            SaveBacking { backup: Some(backup), marker: Some(marker), delete_on_drop: false, attributes }
        };
        #[cfg(not(windows))]
        let backing = SaveBacking { backup: None, marker: None, delete_on_drop: false };
        *guard = Some(backing);
        self.shared.read_generation.fetch_add(1, Ordering::Relaxed);
        Ok(OriginalSavePin { file: self.clone(), owns: true, complete: false })
    }
}

#[cfg(windows)]
pub(super) fn wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

/// Run on an opening worker, after the first screen. Only recognized marker
/// files whose recorded file identity still matches, and whose PID is gone,
/// are removed. A missing current document path preserves backups for recovery.
/// PID reuse deliberately leaves a harmless orphan. Never age out active pins.
pub fn cleanup_stale_save_backups(document_path: &Path) -> io::Result<usize> {
    #[cfg(not(windows))]
    {
        let _ = document_path;
        Ok(0)
    }
    #[cfg(windows)]
    {
        use std::io::Read;
        if !document_path.exists() {
            return Ok(0);
        }
        let Some(parent) = document_path.parent() else { return Ok(0) };
        let mut removed = 0;
        // Bound directory cleanup work; the next open can continue it.
        for entry in fs::read_dir(parent)?.take(4096) {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !name.starts_with(".kwil-old-") || !name.ends_with(".owner") || entry.file_type()?.is_symlink() {
                continue;
            }
            let mut bytes = String::new();
            File::open(entry.path())?.take(4096).read_to_string(&mut bytes)?;
            let mut fields = bytes.lines();
            if fields.next() != Some("KWIL_SAVE_BACKING_V1") {
                continue;
            }
            let Some(pid) = fields.next().and_then(|s| s.parse::<u32>().ok()) else { continue };
            let Some(a) = fields.next().and_then(|s| s.parse::<u64>().ok()) else { continue };
            let Some(b) = fields.next().and_then(|s| s.parse::<u64>().ok()) else { continue };
            let Some(encoded) = fields.next() else { continue };
            if encoded.len() > 2048 || encoded.len() % 4 != 0 || !encoded.is_ascii() {
                continue;
            }
            let units: Option<Vec<_>> =
                (0..encoded.len()).step_by(4).map(|i| u16::from_str_radix(&encoded[i..i + 4], 16).ok()).collect();
            let Some(units) = units else { continue };
            use std::os::windows::ffi::OsStringExt;
            let original = PathBuf::from(std::ffi::OsString::from_wide(&units));
            if original.components().count() != 1 || !parent.join(original).is_file() {
                continue;
            }
            if !name.starts_with(&format!(".kwil-old-{pid}-")) || !process_gone(pid) {
                continue;
            }
            let backup = parent.join(name.trim_end_matches(".owner"));
            if fs::symlink_metadata(&backup).is_ok_and(|m| m.file_type().is_symlink()) {
                continue;
            }
            let Ok(file) = File::open(&backup) else { continue };
            if Stamp::read(&file)?.identity != (a, b) {
                continue;
            }
            drop(file);
            fs::remove_file(backup)?;
            fs::remove_file(entry.path())?;
            removed += 1;
        }
        Ok(removed)
    }
}

#[cfg(windows)]
fn process_gone(pid: u32) -> bool {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_INVALID_PARAMETER},
        System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };
    // SAFETY: query-only process handle; no process state is changed.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return io::Error::last_os_error().raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32);
    }
    // SAFETY: this function owns the handle it just opened.
    unsafe {
        CloseHandle(handle);
    }
    false
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    #[test]
    fn stale_cleanup_checks_pid_identity_marker_and_recovery_path() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("kwil-pin-cleanup-{}-{id}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("file.txt");
        fs::write(&path, b"old").unwrap();
        let file = TextFile::open(&path).unwrap();
        let pin = file.pin_original_for_save().unwrap();
        pin.rollback().unwrap();
        assert_eq!(file.bytes().unwrap(), b"old");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        let pin = file.pin_original_for_save().unwrap();
        let replacement = dir.join("replacement");
        fs::write(&replacement, b"new").unwrap();
        use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;
        let backup_path = pin.backup_path().unwrap();
        // SAFETY: valid unique paths owned by this test, synchronous call.
        assert_ne!(
            unsafe {
                ReplaceFileW(
                    wide(&path).as_ptr(),
                    wide(&replacement).as_ptr(),
                    wide(&backup_path).as_ptr(),
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        pin.commit();
        assert_eq!(cleanup_stale_save_backups(&path).unwrap(), 0, "a live owner is never reaped");
        let (backup, marker) = {
            let mut g = file.shared.save_backing.lock().unwrap();
            let b = g.as_mut().unwrap();
            b.delete_on_drop = false;
            (b.backup.clone().unwrap(), b.marker.clone().unwrap())
        };
        drop(file); // Simulate a crash: marker/backing survive, handles do not.
        let dead = u32::MAX - 1;
        assert!(process_gone(dead));
        let orphan = dir.join(format!(".kwil-old-{dead}-{id}"));
        let owner = dir.join(format!(".kwil-old-{dead}-{id}.owner"));
        fs::rename(&backup, &orphan).unwrap();
        fs::rename(&marker, &owner).unwrap();
        let mut contents = fs::read_to_string(&owner).unwrap().lines().map(str::to_owned).collect::<Vec<_>>();
        contents[1] = dead.to_string();
        fs::write(&owner, contents.join("\n")).unwrap();
        let unrelated = dir.join(".kwil-old-unrelated");
        fs::write(&unrelated, b"foreign").unwrap();
        let fake_marker = dir.join(".kwil-old-fake.owner");
        fs::write(&fake_marker, b"not a kwil marker").unwrap();
        fs::remove_file(&path).unwrap();
        let other = dir.join("other.txt");
        fs::write(&other, b"other").unwrap();
        assert_eq!(cleanup_stale_save_backups(&other).unwrap(), 0, "preserve a crash-gap original for recovery");
        fs::write(&path, b"new").unwrap();
        let saved_marker = fs::read_to_string(&owner).unwrap();
        let mut mismatched = contents.clone();
        mismatched[3] = "0".into();
        fs::write(&owner, mismatched.join("\n")).unwrap();
        assert_eq!(cleanup_stale_save_backups(&path).unwrap(), 0, "identity mismatch must refuse deletion");
        fs::write(&owner, saved_marker).unwrap();
        assert_eq!(cleanup_stale_save_backups(&path).unwrap(), 1);
        assert!(!orphan.exists() && !owner.exists());
        assert_eq!(fs::read(&unrelated).unwrap(), b"foreign");
        assert!(fake_marker.exists());
        fs::remove_dir_all(dir).unwrap();
    }
}
