//! Worker-only atomic save. Write and sync a same-directory temp before
//! touching the original name. Payloads stream directly from immutable pieces.
use kwil_core_text::{Buffer, Snapshot, TextFile};
use std::{
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

pub struct SaveRequest {
    pub snapshot: Snapshot,
    pub path: PathBuf,
    /// Current path's original, if this document has one. Older originals
    /// referenced by undo are already pinned by their previous save.
    pub original: Option<TextFile>,
    pub revision: u64,
}

pub struct SavedSnapshot {
    pub path: PathBuf,
    pub file: TextFile,
    pub snapshot: Snapshot,
    pub revision: u64,
    /// Windows security descriptor preservation is best effort; failures are
    /// returned to the frontend as warnings, never hidden.
    pub warnings: Vec<String>,
}

/// Opening worker entry point, also conservatively cleans recognized orphan
/// Windows save backings. Use after showing the initial viewport.
pub fn open_for_editing(path: &Path) -> io::Result<TextFile> {
    kwil_core_text::cleanup_stale_save_backups(path)?;
    TextFile::open(path).map_err(Into::into)
}

struct Temporary {
    path: PathBuf,
    file: Option<File>,
}
impl Temporary {
    fn new(parent: &Path) -> io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let path =
                parent.join(format!(".kwil-save-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(file) => return Ok(Self { path, file: Some(file) }),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
    }
}
impl Drop for Temporary {
    fn drop(&mut self) {
        self.file.take();
        let _ = fs::remove_file(&self.path);
    }
}

pub fn save_snapshot(request: SaveRequest) -> io::Result<SavedSnapshot> {
    save_with(request, |snapshot, file| snapshot.write_to(file))
}

fn save_with(
    request: SaveRequest,
    write: impl FnOnce(&Snapshot, &mut File) -> io::Result<()>,
) -> io::Result<SavedSnapshot> {
    let path = std::path::absolute(&request.path)?;
    let parent = path.parent().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "save path has no parent"))?;
    let metadata = match fs::symlink_metadata(&path) {
        Ok(m) => {
            if m.file_type().is_symlink() || !m.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "save requires a regular file, not a symlink or directory",
                ));
            }
            if m.permissions().readonly() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "file is read-only; choose another path or obtain write permission",
                ));
            }
            Some(m)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(save_error(&path, e)),
    };
    let mut temp = Temporary::new(parent).map_err(|e| save_error(&path, e))?;
    write(&request.snapshot, temp.file.as_mut().unwrap()).map_err(|e| save_error(&path, e))?;
    let mut warnings = Vec::new();
    if let Some(metadata) = &metadata {
        preserve_metadata(&path, &temp.path, metadata, &mut warnings)?;
    }
    temp.file.as_ref().unwrap().sync_all().map_err(|e| save_error(&path, e))?;
    temp.file.take();
    // Do not pin an unrelated source when Save As targets a different path.
    let original = request.original.filter(|f| f.path() == path);
    let pin = original.as_ref().map(TextFile::pin_original_for_save).transpose()?;
    if let Err(error) = replace(&temp.path, &path) {
        if let Some(pin) = pin
            && let Err(rollback) = pin.rollback()
        {
            return Err(io::Error::new(
                error.kind(),
                format!(
                    "save failed: {error}; original path restoration failed: {rollback}; recover the .kwil-old sibling before reopening"
                ),
            ));
        }
        return Err(save_error(&path, error));
    }
    if let Some(pin) = pin {
        pin.commit();
    }
    #[cfg(unix)]
    File::open(parent)?
        .sync_all()
        .map_err(|e| io::Error::new(e.kind(), format!("file replaced, but directory sync failed: {e}")))?;
    // Indexing/rebase preparation stays on this same worker. No UI-thread
    // scan or read from the original pathname can occur during completion.
    let file = TextFile::open(&path).map_err(io::Error::from)?;
    let buffer = Buffer::from_file(file.clone()).map_err(io::Error::other)?;
    Ok(SavedSnapshot { path, file, snapshot: buffer.snapshot(), revision: request.revision, warnings })
}

fn save_error(path: &Path, error: io::Error) -> io::Error {
    let hint = if error.kind() == io::ErrorKind::PermissionDenied {
        "; path needs write permission or elevation; choose another path"
    } else {
        ""
    };
    io::Error::new(error.kind(), format!("cannot save {}: {error}{hint}", path.display()))
}

#[cfg(unix)]
fn preserve_metadata(_: &Path, temp: &Path, original: &fs::Metadata, _: &mut Vec<String>) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, chown};
    let current = fs::metadata(temp)?;
    if (current.uid(), current.gid()) != (original.uid(), original.gid()) {
        chown(temp, Some(original.uid()), Some(original.gid()))?;
    }
    // chown may clear setuid/setgid; set permissions afterwards.
    fs::set_permissions(temp, original.permissions())
}
#[cfg(unix)]
fn replace(temp: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(temp, destination)
}

#[cfg(windows)]
fn wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

#[cfg(windows)]
fn preserve_metadata(path: &Path, temp: &Path, _: &fs::Metadata, warnings: &mut Vec<String>) -> io::Result<()> {
    use windows_sys::Win32::{
        Security::{
            DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, GetFileSecurityW, OWNER_SECURITY_INFORMATION,
            SetFileSecurityW,
        },
        Storage::FileSystem::{GetFileAttributesW, INVALID_FILE_ATTRIBUTES, SetFileAttributesW},
    };
    let source = wide(path);
    let target = wide(temp);
    // SAFETY: terminated paths and a bounded aligned writable descriptor buffer.
    let attributes = unsafe { GetFileAttributesW(source.as_ptr()) };
    if attributes == INVALID_FILE_ATTRIBUTES {
        return Err(io::Error::last_os_error());
    }
    if unsafe { SetFileAttributesW(target.as_ptr(), attributes) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let flags = OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
    let mut descriptor = vec![0_u32; (64 << 10) / 4];
    let mut needed = 0;
    // SAFETY: descriptor has 64 KiB writable, aligned storage; required count is writable.
    if unsafe { GetFileSecurityW(source.as_ptr(), flags, descriptor.as_mut_ptr().cast(), 64 << 10, &mut needed) } == 0 {
        warnings.push(format!("could not read original ACL/owner: {}", io::Error::last_os_error()));
        return Ok(());
    }
    // SAFETY: successful GetFileSecurityW produced this self-relative descriptor.
    if unsafe { SetFileSecurityW(target.as_ptr(), flags, descriptor.as_mut_ptr().cast()) } == 0 {
        // Owner/group may require extra privilege even when DACL is writable.
        warnings.push(format!("could not preserve full ACL/owner: {}", io::Error::last_os_error()));
        if unsafe { SetFileSecurityW(target.as_ptr(), DACL_SECURITY_INFORMATION, descriptor.as_mut_ptr().cast()) } == 0
        {
            warnings.push(format!("could not preserve DACL: {}", io::Error::last_os_error()));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn replace(temp: &Path, destination: &Path) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW};
    // SAFETY: terminated paths, synchronous operation; no pointers retained.
    if unsafe {
        MoveFileExW(wide(temp).as_ptr(), wide(destination).as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    struct CountingAllocator;
    static COUNT_ALLOCATIONS: AtomicBool = AtomicBool::new(false);
    static LARGEST_ALLOCATION: AtomicUsize = AtomicUsize::new(0);
    // One explicitly selected measurement test enables this counter. Production
    // builds use the normal system allocator with no instrumentation.
    unsafe impl std::alloc::GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
            if COUNT_ALLOCATIONS.load(Ordering::Relaxed) {
                LARGEST_ALLOCATION.fetch_max(layout.size(), Ordering::Relaxed);
            }
            unsafe { std::alloc::System.alloc(layout) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
            unsafe { std::alloc::System.dealloc(ptr, layout) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, size: usize) -> *mut u8 {
            if COUNT_ALLOCATIONS.load(Ordering::Relaxed) {
                LARGEST_ALLOCATION.fetch_max(size, Ordering::Relaxed);
            }
            unsafe { std::alloc::System.realloc(ptr, layout, size) }
        }
    }
    #[global_allocator]
    static ALLOCATOR: CountingAllocator = CountingAllocator;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "kwil-save-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> PathBuf {
            self.0.join("text.txt")
        }
        fn large() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
            fs::create_dir_all(&root).unwrap();
            let path =
                root.join(format!("kwil-save-large-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn request(&self, original: TextFile, buffer: &Buffer) -> SaveRequest {
            SaveRequest { path: self.path(), snapshot: buffer.snapshot(), original: Some(original), revision: 1 }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn atomic_content_crlf_worker_snapshot_and_undo() {
        let f = Fixture::new();
        let path = f.path();
        fs::write(&path, b"a\r\nb\r\n").unwrap();
        let original = open_for_editing(&path).unwrap();
        let mut b = Buffer::from_file(original.clone()).unwrap();
        b.insert(1, b" edited").unwrap();
        let worker = b.snapshot();
        let saved = save_snapshot(f.request(original.clone(), &b)).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"a edited\r\nb\r\n");
        let mut out = Vec::new();
        worker.write_to(&mut out).unwrap();
        assert_eq!(out, b"a edited\r\nb\r\n");
        b.checkpoint_current(saved.snapshot).unwrap();
        assert!(b.undo());
        let saved_again = save_snapshot(f.request(saved.file, &b)).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"a\r\nb\r\n");
        assert!(b.redo());
        let mut out = Vec::new();
        b.write_to(&mut out).unwrap();
        assert_eq!(out, b"a edited\r\nb\r\n");
        drop((worker, b, original, saved_again));
        assert_eq!(fs::read_dir(&f.0).unwrap().count(), 1, "last reference must remove save backings and markers");
    }

    #[test]
    fn failed_write_leaves_original_and_no_temp() {
        let f = Fixture::new();
        let path = f.path();
        fs::write(&path, b"unchanged").unwrap();
        let original = open_for_editing(&path).unwrap();
        let mut b = Buffer::from_file(original.clone()).unwrap();
        b.insert(0, b"edited ").unwrap();
        let error = save_with(f.request(original, &b), |_, file| {
            file.write_all(b"partial")?;
            Err(io::Error::other("simulated write failure"))
        })
        .err()
        .unwrap();
        assert!(error.to_string().contains("simulated write failure"));
        assert_eq!(fs::read(&path).unwrap(), b"unchanged");
        assert_eq!(fs::read_dir(&f.0).unwrap().count(), 1);
    }

    #[test]
    fn read_only_error_is_clear() {
        let f = Fixture::new();
        let path = f.path();
        fs::write(&path, b"original").unwrap();
        let original = open_for_editing(&path).unwrap();
        let b = Buffer::from_file(original.clone()).unwrap();
        let saved_permissions = fs::metadata(&path).unwrap().permissions();
        let mut permissions = saved_permissions.clone();
        permissions.set_readonly(true);
        fs::set_permissions(&path, permissions).unwrap();
        let error = save_snapshot(f.request(original, &b)).err().unwrap();
        fs::set_permissions(&path, saved_permissions).unwrap();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("read-only"));
        assert_eq!(fs::read(&path).unwrap(), b"original");
    }

    #[cfg(unix)]
    #[test]
    fn unix_permissions_and_owner_survive_replace() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let f = Fixture::new();
        let path = f.path();
        fs::write(&path, b"old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let before = fs::metadata(&path).unwrap();
        let original = open_for_editing(&path).unwrap();
        let mut b = Buffer::from_file(original.clone()).unwrap();
        b.insert(0, b"new ").unwrap();
        save_snapshot(f.request(original, &b)).unwrap();
        let after = fs::metadata(&path).unwrap();
        assert_eq!((after.mode() & 0o7777, after.uid(), after.gid()), (0o640, before.uid(), before.gid()));
    }

    #[test]
    #[ignore = "one-shot 100 MB streaming save; explicitly requested, not an ordinary unit test"]
    fn streaming_100mb_mapped_save() {
        let f = Fixture::large();
        let path = f.path();
        let file = File::create(&path).unwrap();
        file.set_len(100 << 20).unwrap();
        drop(file);
        let original = open_for_editing(&path).unwrap();
        let mut b = Buffer::from_file(original.clone()).unwrap();
        b.insert(50 << 20, b"edit\r\n").unwrap();
        let written = (100 << 20) + 6;
        LARGEST_ALLOCATION.store(0, Ordering::Relaxed);
        COUNT_ALLOCATIONS.store(true, Ordering::Relaxed);
        let now = std::time::Instant::now();
        let saved = save_snapshot(f.request(original, &b)).unwrap();
        let elapsed_ms = now.elapsed().as_secs_f64() * 1000.0;
        COUNT_ALLOCATIONS.store(false, Ordering::Relaxed);
        let largest = LARGEST_ALLOCATION.load(Ordering::Relaxed);
        assert!(largest < 16 << 20, "file-sized allocation in saving/reopen: {largest}");
        assert_eq!(saved.file.len(), written as u64);
        assert!(b.undo());
        let snapshot = b.snapshot();
        assert_eq!(snapshot.chunks(0..16).unwrap().next().unwrap().unwrap(), &[0; 16]);
        drop(snapshot);
        drop((saved, b));
        assert_eq!(fs::read_dir(&f.0).unwrap().count(), 1);
        println!("stream_save_100mb_ms={elapsed_ms:.3} bytes={written} largest_allocation_bytes={largest}");
    }
}
