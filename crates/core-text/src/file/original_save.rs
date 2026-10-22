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
            if let Some(path) = &self.backup {
                let _ = fs::remove_file(path);
            }
            if let Some(path) = &self.marker {
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
        }
        self.complete = true;
    }
    pub fn rollback(mut self) -> io::Result<()> {
        let result = self.restore();
        self.complete = true;
        result
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
            // rename refuses to overwrite a concurrent writer's destination.
            fs::rename(path, &self.file.shared.path)?;
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
            return Ok(OriginalSavePin { file: self.clone(), owns: false, complete: false });
        }
        let now = File::open(&self.shared.path)?;
        if self.shared.stamp.state(&Stamp::read(&now)?) != FileState::Unchanged {
            return Err(io::Error::other("file changed on disk before save; reload or compare first"));
        }
        #[cfg(windows)]
        let backing = {
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_ATTRIBUTE_HIDDEN, GetFileAttributesW, INVALID_FILE_ATTRIBUTES, SetFileAttributesW,
            };
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
            if let Err(error) = fs::rename(&self.shared.path, &backup) {
                let _ = fs::remove_file(marker);
                return Err(error);
            }
            // SAFETY: owned backup path; preserve all existing attributes.
            let _ = unsafe { SetFileAttributesW(wide(&backup).as_ptr(), attributes | FILE_ATTRIBUTE_HIDDEN) };
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
