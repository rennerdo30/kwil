//! Windows host boundary. Raw documented Win32 ABI avoids dependency/feature
//! changes. No registry function is called by prepare or its factory.
use super::{NativeAdapter, Prepared, security};
use kwil_core_commands::associations::{AssociationError, AssociationPlatform, DefaultChoice, WindowsAssociations};
use kwil_view_model::file_type_registration::{RegistrationContext, RegistrationLimits};
use std::{
    collections::BTreeMap,
    ffi::c_void,
    fs::{self, File, OpenOptions},
    io,
    mem::{size_of, size_of_val},
    os::windows::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    },
    path::{Component, Path, PathBuf, Prefix},
    ptr::null_mut,
};

const JOURNAL_CHILD: &str = "kwil-associations-v1";
const MAX_PATH_UNITS: usize = 4096;
const MAX_DEPTH: usize = 128;
const REPARSE: u32 = 0x400;
const TOKEN_QUERY: u32 = 8;
type Handle = *mut c_void;

#[repr(C)]
struct TokenUser {
    sid: *mut c_void,
    attributes: u32,
}
#[repr(C)]
struct SecurityAttributes {
    length: u32,
    descriptor: *mut c_void,
    inherit: i32,
}
#[repr(C)]
#[derive(Default)]
struct FileInfo {
    attributes: u32,
    creation: [u32; 2],
    access: [u32; 2],
    write: [u32; 2],
    volume: u32,
    size_high: u32,
    size_low: u32,
    links: u32,
    index_high: u32,
    index_low: u32,
}
#[link(name = "advapi32")]
unsafe extern "system" {
    fn OpenProcessToken(process: Handle, access: u32, token: *mut Handle) -> i32;
    fn OpenThreadToken(thread: Handle, access: u32, as_self: i32, token: *mut Handle) -> i32;
    fn GetTokenInformation(token: Handle, class: i32, into: *mut c_void, length: u32, needed: *mut u32) -> i32;
    fn GetKernelObjectSecurity(
        object: Handle,
        information: u32,
        into: *mut c_void,
        length: u32,
        needed: *mut u32,
    ) -> i32;
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        text: *const u16,
        revision: u32,
        into: *mut *mut c_void,
        size: *mut u32,
    ) -> i32;
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcess() -> Handle;
    fn GetCurrentThread() -> Handle;
    fn CloseHandle(handle: Handle) -> i32;
    fn LocalFree(memory: *mut c_void) -> *mut c_void;
    fn CreateDirectoryW(path: *const u16, attributes: *const SecurityAttributes) -> i32;
    fn GetFileInformationByHandle(file: Handle, into: *mut FileInfo) -> i32;
}
struct OwnedHandle(Handle);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
struct LocalAllocation(*mut c_void);
impl Drop for LocalAllocation {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}
fn os_error(step: &str) -> String {
    format!("{step}: {}", io::Error::last_os_error())
}

/// Fixed aligned TokenUser buffer. Validate the returned length AND in-buffer
/// SID address/length before dereferencing. There is no username/env fallback.
fn token_user() -> Result<Vec<u8>, String> {
    let mut impersonation = null_mut();
    // HKCU's process identity must agree with the filesystem identity. Refuse
    // impersonation, including inability to inspect the thread token.
    if unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut impersonation) } != 0 {
        let _token = OwnedHandle(impersonation);
        return Err("Registration is unavailable while this thread impersonates another identity".into());
    }
    if io::Error::last_os_error().raw_os_error() != Some(1008) {
        return Err(os_error("Inspect thread token"));
    }
    let mut token = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(os_error("Open process token"));
    }
    let token = OwnedHandle(token);
    let mut buffer = [0usize; 256];
    let mut needed = 0u32;
    if unsafe { GetTokenInformation(token.0, 1, buffer.as_mut_ptr().cast(), size_of_val(&buffer) as u32, &mut needed) }
        == 0
    {
        return Err(os_error("Read bounded TokenUser"));
    }
    let length = needed as usize;
    if length < size_of::<TokenUser>() || length > size_of_val(&buffer) {
        return Err("Invalid TokenUser length".into());
    }
    // buffer is usize-aligned, sufficient for TOKEN_USER on x86/x64/ARM64.
    let user = unsafe { &*buffer.as_ptr().cast::<TokenUser>() };
    let base = buffer.as_ptr() as usize;
    let offset = (user.sid as usize).checked_sub(base).ok_or("SID precedes TokenUser buffer")?;
    if offset < size_of::<TokenUser>() || offset > length {
        return Err("SID is outside TokenUser buffer".into());
    }
    // Read through the original allocation, never a foreign returned pointer.
    let bytes = unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), length) };
    Ok(security::sid(&bytes[offset..])?.to_vec())
}

fn validate_path(path: &Path) -> Result<(), String> {
    let units: Vec<u16> = path.as_os_str().encode_wide().take(MAX_PATH_UNITS + 1).collect();
    if !path.is_absolute()
        || path.as_os_str().len() > MAX_PATH_UNITS
        || units.len() > MAX_PATH_UNITS
        || units.contains(&0)
    {
        return Err("Registration requires a bounded absolute native path".into());
    }
    let mut components = path.components();
    if !matches!(components.next(), Some(Component::Prefix(p)) if matches!(p.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)))
    {
        return Err("Registration requires a local disk path, not UNC/device storage".into());
    }
    if components.clone().count() > MAX_DEPTH {
        return Err("Registration path is too deep".into());
    }
    for c in components {
        match c {
            Component::RootDir => {}
            Component::Normal(name) => {
                let units: Vec<u16> = name.encode_wide().collect();
                if units.iter().any(|c| matches!(*c, 0 | 58)) || matches!(units.last(), Some(32 | 46)) {
                    return Err("Ambiguous registration path component".into());
                }
            }
            _ => return Err("Registration path must be normalized".into()),
        }
    }
    // Path::components normalizes interior '.'; reject it in the original form.
    let text: Vec<u16> = path.as_os_str().encode_wide().collect();
    if text.split(|u| matches!(*u, 47 | 92)).any(|p| p == [46] || p == [46, 46]) {
        return Err("Registration path must not contain dot components".into());
    }
    Ok(())
}

fn open_checked(path: &Path, directory: bool) -> Result<File, String> {
    // No DELETE sharing: retain these handles through worker shutdown to pin
    // each checked ancestor and journal record against rename/replacement.
    let file = OpenOptions::new()
        .access_mode(0x0002_0080)
        .share_mode(3)
        .custom_flags(0x0020_0000 | if directory { 0x0200_0000 } else { 0 })
        .open(path)
        .map_err(|e| format!("Open registration storage: {e}"))?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if meta.file_attributes() & REPARSE != 0 || if directory { !meta.is_dir() } else { !meta.is_file() } {
        return Err("Registration storage must be a real directory/file without reparse points".into());
    }
    let mut info = FileInfo::default();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(os_error("Inspect registration file identity"));
    }
    if !directory && info.links != 1 {
        return Err("Registration records must not have hard links".into());
    }
    Ok(file)
}

fn pin_ancestors(path: &Path) -> Result<Vec<File>, String> {
    validate_path(path)?;
    let mut current = PathBuf::new();
    let mut guards = Vec::new();
    for c in path.components() {
        current.push(c);
        if !matches!(c, Component::Prefix(_)) {
            guards.push(open_checked(&current, true)?);
        }
    }
    Ok(guards)
}

fn review_acl(file: &File, sid: &[u8], private: bool, directory: bool) -> Result<(), String> {
    let mut buffer = [0usize; security::MAX_DESCRIPTOR / size_of::<usize>()];
    let mut needed = 0;
    // OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION. Fixed 64KiB cap;
    // oversize/unsupported filesystems fail closed, without a retry allocation.
    if unsafe {
        GetKernelObjectSecurity(
            file.as_raw_handle(),
            5,
            buffer.as_mut_ptr().cast(),
            size_of_val(&buffer) as u32,
            &mut needed,
        )
    } == 0
    {
        return Err(os_error("Read bounded registration ACL"));
    }
    if needed as usize > size_of_val(&buffer) {
        return Err("Registration ACL exceeds bound".into());
    }
    let bytes = unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), needed as usize) };
    security::review(bytes, sid, private, directory)
}

fn ensure_private_child(path: &Path, sid: &[u8]) -> Result<File, String> {
    validate_path(path)?;
    let text: Vec<u16> = format!("O:{}D:P(A;OICI;FA;;;{})", security::sid_text(sid)?, security::sid_text(sid)?)
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mut raw = null_mut();
    if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(text.as_ptr(), 1, &mut raw, null_mut()) } == 0 {
        return Err(os_error("Create private registration descriptor"));
    }
    let descriptor = LocalAllocation(raw);
    let attrs =
        SecurityAttributes { length: size_of::<SecurityAttributes>() as u32, descriptor: descriptor.0, inherit: 0 };
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe { CreateDirectoryW(wide.as_ptr(), &attrs) } == 0 && io::Error::last_os_error().raw_os_error() != Some(183)
    {
        return Err(os_error("Create private registration directory"));
    }
    // Already-existing directories are reviewed, NEVER chmod/repaired/adopted
    // on trust. Creation security is effective before the worker writes a byte.
    let file = open_checked(path, true)?;
    review_acl(&file, sid, true, true)?;
    Ok(file)
}

struct NativeDirectories<'a> {
    sid: &'a [u8],
}
impl security::ProfileDirectories for NativeDirectories<'_> {
    type Guard = File;

    fn present(&mut self, path: &Path) -> Result<bool, String> {
        validate_path(path)?;
        match fs::symlink_metadata(path) {
            Ok(meta) if meta.is_dir() && meta.file_attributes() & REPARSE == 0 => Ok(true),
            Ok(_) => Err("Settings parent must be a real directory without reparse points".into()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(format!("Inspect settings parent: {error}")),
        }
    }

    fn pin_reviewed_existing(&mut self, path: &Path) -> Result<(PathBuf, Vec<File>), String> {
        // Both spelling and canonical ancestry must exist and be pinned before
        // either branch may create anything. Never walk upward on NotFound.
        let mut guards = pin_ancestors(path)?;
        review_acl(guards.last().ok_or("Settings ancestor is missing")?, self.sid, false, true)?;
        let canonical = fs::canonicalize(path).map_err(|e| format!("Canonical settings ancestor: {e}"))?;
        guards.extend(pin_ancestors(&canonical)?);
        review_acl(guards.last().ok_or("Canonical ancestor is missing")?, self.sid, false, true)?;
        Ok((canonical, guards))
    }

    fn create_private_verified(&mut self, path: &Path) -> Result<File, String> {
        // CREATE_DIRECTORY with explicit protected user owner/DACL, then reopen
        // without DELETE sharing and review actual owner/DACL. A racing existing
        // child must pass the same private check; no existing ACL is rewritten.
        ensure_private_child(path, self.sid)
    }
}

pub(super) fn prepare(settings_path: &Path) -> Result<Prepared, String> {
    validate_path(settings_path)?;
    let parent = settings_path.parent().filter(|p| p.file_name().is_some()).ok_or("Settings parent is required")?;
    let sid = token_user()?;
    let (parent, mut guards) = security::profile_parent(&mut NativeDirectories { sid: &sid }, parent)?;
    // An existing settings symlink must never quietly switch profile identity.
    match fs::symlink_metadata(settings_path) {
        Ok(m) if !m.is_file() || m.file_attributes() & REPARSE != 0 => {
            return Err("Settings path is not a regular native profile file".into());
        }
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.to_string()),
    }
    let journal = parent.join(JOURNAL_CHILD);
    validate_path(&journal)?;
    guards.push(ensure_private_child(&journal, &sid)?);
    // Existing journals retain recovery only when every entry already has the
    // same private owner ACL. No contents, registry values or receipts are read.
    let cap = kwil_core_commands::associations::worker::MAX_JOURNAL_ENTRIES;
    for (count, entry) in fs::read_dir(&journal).map_err(|e| e.to_string())?.enumerate() {
        if count >= cap {
            return Err("Registration journal exceeds its finite entry limit".into());
        }
        let file = open_checked(&entry.map_err(|e| e.to_string())?.path(), false)?;
        review_acl(&file, &sid, true, false)?;
        guards.push(file);
    }
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    let executable = kwil_core_commands::associations::windows_paths::explorer_executable(&executable)
        .map(PathBuf::from)
        .map_err(|e| e.to_string())?;
    let identity = security::sid_text(&sid)?;
    let context = RegistrationContext {
        platform: AssociationPlatform::Windows,
        executable,
        content_types: BTreeMap::new(),
        default_choice: DefaultChoice::KeepCurrent,
        limits: RegistrationLimits::RootAdopted256,
    };
    Ok(Prepared {
        journal,
        context,
        factory: Box::new(move || {
            // Factory runs on the association worker, not the preparation/UI thread.
            if token_user().map_err(AssociationError)? != sid {
                return Err(AssociationError("Registration identity changed before worker start".into()));
            }
            Ok(NativeAdapter { inner: WindowsAssociations::new(identity)?, _guards: guards })
        }),
    })
}

pub(super) fn repair_existing() -> Result<(), String> {
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    let settings = kwil_core_commands::settings::Settings::path(&executable).map_err(|e| e.to_string())?;
    // The same SID, ancestor, ACL and record checks as explicit registration.
    // Guards transfer into the factory and live through journal replay/repair.
    let prepared = prepare(&settings)?;
    kwil_core_commands::associations::worker::repair_startup(prepared.journal, prepared.factory)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_paths_reject_ambiguous_or_nonlocal_input_without_io() {
        for text in [
            r"relative\settings.toml",
            r"C:\a\..\settings.toml",
            r"C:\a\.\settings.toml",
            r"\\server\share\settings.toml",
            r"C:\a\settings.toml:stream",
            "C:\\a\\settings.toml.",
            "C:\\a\\settings.toml\0",
        ] {
            assert!(validate_path(Path::new(text)).is_err(), "{text:?}");
        }
        assert!(validate_path(Path::new(r"C:\Users\example\Kwil\settings.toml")).is_ok());
        assert!(validate_path(Path::new(r"\\?\C:\Users\example\Kwil\settings.toml")).is_ok());
    }
}
