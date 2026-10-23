//! Save As reservation identities. Filesystem resolution belongs on a worker.
//!
//! Essential caller contract: immediately before **each** write, the single
//! saving worker must freshly resolve the candidate AND every open/pending
//! reservation from its original requested path, then compare them. Any error
//! refuses admission; never steal another document's reservation. In particular,
//! a missing destination can become a file (or a hard link) after reservation.
//! Refreshing only the candidate misses that transition. This also matters for
//! symlinked parents and filesystem Unicode normalization/case aliases.
//!
//! `paths_may_alias` is a bounded, filesystem-free UI prefilter. A false result
//! does not establish distinctness: only worker resolution can discover hard
//! links, symlinks, short names, or Unicode normalization aliases. Windows
//! ordinal ignore-case comparison deliberately overrejects case-sensitive
//! ambiguities. It is not a filesystem equivalence oracle.
//!
//! These are observations, not locks or permission grants. They neither write
//! nor reserve anything and cannot prevent external filesystem changes between
//! observation and write. The caller owns save serialization and atomic saving.

use std::{
    ffi::OsStr,
    fs, io,
    path::{Path, PathBuf},
};

/// Maximum input size before cloning/normalizing: native bytes (UTF-16 bytes on
/// Windows). Canonical results are also checked before retaining them.
pub const MAX_SAVE_PATH_BYTES: usize = 32 * 1024;

/// Worker observation of a regular file or a missing name in an existing
/// directory. Native paths are retained without Unicode replacement/loss.
#[derive(Clone, Debug)]
pub struct SaveDestination {
    requested_path: PathBuf,
    canonical_path: PathBuf,
    identity: DestinationIdentity,
}

#[derive(Clone, Debug)]
enum DestinationIdentity {
    Existing(NativeIdentity),
    Missing { parent: NativeIdentity },
}

#[cfg(unix)]
type NativeIdentity = (u64, u64);
#[cfg(windows)]
type NativeIdentity = (u64, [u8; 16]);

impl SaveDestination {
    /// Original spelling: use this path for every refresh, so moved symlinks
    /// and replaced parents cannot leave a stale canonical reservation.
    pub fn requested_path(&self) -> &Path {
        &self.requested_path
    }

    /// Diagnostic native path: canonical target, or canonical parent + name.
    /// Callers choose whether to display it; nothing is logged by this module.
    pub fn canonical_path(&self) -> &Path {
        &self.canonical_path
    }

    pub fn exists(&self) -> bool {
        matches!(self.identity, DestinationIdentity::Existing(_))
    }

    /// Conservative reservation conflict, using already resolved observations.
    /// No filesystem access. Both observations must obey the refresh contract.
    pub fn equivalent(&self, other: &Self) -> bool {
        match (&self.identity, &other.identity) {
            (DestinationIdentity::Existing(a), DestinationIdentity::Existing(b)) if a == b => {
                return true;
            }
            (DestinationIdentity::Missing { parent: a }, DestinationIdentity::Missing { parent: b })
                if a == b
                    && native_names_equal(
                        self.canonical_path.file_name().expect("resolved terminal name"),
                        other.canonical_path.file_name().expect("resolved terminal name"),
                    ) =>
            {
                return true;
            }
            _ => {}
        }
        paths_may_alias(&self.canonical_path, &other.canonical_path)
    }
}

/// WORKER ONLY: canonicalizes/stats and, on Windows, opens metadata handles.
/// Accepts only bounded absolute file destinations. An absent parent,
/// inaccessible metadata/identity, dangling symlink or nonregular target fails
/// closed. No directories/files are created, and no file content is read.
/// Successful resolution does not promise that a later write is permitted.
pub fn resolve_save_destination(path: &Path) -> io::Result<SaveDestination> {
    validate_path(path)?;
    match fs::canonicalize(path) {
        Ok(canonical) => existing(path, canonical),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            // NotFound can also mean a dangling symlink: that is not an absent
            // destination name and must not be admitted as one.
            match fs::symlink_metadata(path) {
                Ok(_) => return Err(invalid("destination cannot be resolved")),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            let parent = fs::canonicalize(path.parent().ok_or_else(|| invalid("missing parent"))?)?;
            check_bound(&parent)?;
            let parent_identity = native_identity(&parent, true)?;
            let name = path.file_name().ok_or_else(|| invalid("missing terminal filename"))?;
            // Bound the joined path before allocating it (canonicalization may
            // expand a short symlink into a much longer parent).
            if native_bytes(parent.as_os_str()) + native_bytes(name) + 2 > MAX_SAVE_PATH_BYTES {
                return Err(invalid("resolved destination exceeds 32 KiB"));
            }
            let canonical = parent.join(name);
            // If the name appeared during parent resolution, observe its real
            // identity now; never turn errors other than NotFound into Missing.
            match fs::symlink_metadata(&canonical) {
                Ok(_) => existing(path, fs::canonicalize(&canonical)?),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(SaveDestination {
                    requested_path: path.to_path_buf(),
                    canonical_path: canonical,
                    identity: DestinationIdentity::Missing { parent: parent_identity },
                }),
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

fn existing(requested: &Path, canonical: PathBuf) -> io::Result<SaveDestination> {
    check_bound(&canonical)?;
    let identity = native_identity(&canonical, false)?;
    Ok(SaveDestination {
        requested_path: requested.to_path_buf(),
        canonical_path: canonical,
        identity: DestinationIdentity::Existing(identity),
    })
}

/// Filesystem-free UI reservation prefilter, including for nonexistent paths.
/// Invalid/oversized/unsupported inputs conservatively conflict with everything.
/// Windows compares normalized DOS/UNC spellings with CompareStringOrdinal,
/// never Rust Unicode lowercase or lossy string conversion. Always perform the
/// worker check even when this returns false; see the module caller contract.
pub fn paths_may_alias(a: &Path, b: &Path) -> bool {
    if validate_path(a).is_err() || validate_path(b).is_err() {
        return true;
    }
    #[cfg(unix)]
    {
        a.components().eq(b.components())
    }
    #[cfg(windows)]
    {
        match (windows_path_key(a), windows_path_key(b)) {
            (Some(a), Some(b)) => ordinal_equal(&a, &b),
            _ => true,
        }
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn check_bound(path: &Path) -> io::Result<()> {
    if native_bytes(path.as_os_str()) > MAX_SAVE_PATH_BYTES {
        return Err(invalid("destination exceeds 32 KiB"));
    }
    Ok(())
}

#[cfg(unix)]
fn native_bytes(value: &OsStr) -> usize {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().len()
}

#[cfg(windows)]
fn native_bytes(value: &OsStr) -> usize {
    use std::os::windows::ffi::OsStrExt;
    // Bounded scan, no temporary UTF-16 allocation for rejected inputs.
    value.encode_wide().take(MAX_SAVE_PATH_BYTES / 2 + 1).count() * 2
}

fn validate_path(path: &Path) -> io::Result<()> {
    check_bound(path)?;
    if !path.is_absolute() || path.file_name().is_none() {
        return Err(invalid("destination must be absolute with a terminal filename"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let bytes = path.as_os_str().as_bytes();
        let terminal = bytes.rsplit(|&b| b == b'/').next().unwrap_or_default();
        if bytes.contains(&0) || terminal.is_empty() || terminal == b"." || terminal == b".." {
            return Err(invalid("invalid terminal filename"));
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use std::path::{Component, Prefix};
        if !matches!(path.components().next(), Some(Component::Prefix(p)) if matches!(
            p.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_) | Prefix::UNC(_, _) | Prefix::VerbatimUNC(_, _)
        )) {
            return Err(invalid("unsupported Windows destination namespace"));
        }
        // Reject device/stream/legacy-trimmed names rather than accidentally
        // observing one pathname and later writing a different one.
        for component in path.components() {
            if let Component::Normal(name) = component {
                let units: Vec<_> = name.encode_wide().collect();
                if !windows_filename(&units) {
                    return Err(invalid("unsupported Windows filename"));
                }
            }
        }
        let last = path.as_os_str().encode_wide().last();
        if matches!(last, Some(47 | 92 | 46)) {
            return Err(invalid("invalid terminal filename"));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn native_identity(path: &Path, directory: bool) -> io::Result<NativeIdentity> {
    use std::os::unix::fs::MetadataExt;
    // stat only: never open a FIFO/device and risk a blocking content open.
    let metadata = fs::metadata(path)?;
    if !(if directory { metadata.is_dir() } else { metadata.is_file() }) {
        return Err(invalid("destination must be a regular file with a directory parent"));
    }
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(windows)]
fn native_identity(path: &Path, directory: bool) -> io::Result<NativeIdentity> {
    use std::{
        fs::OpenOptions,
        os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_INFO, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FileIdInfo, GetFileInformationByHandleEx,
    };
    let metadata = fs::metadata(path)?;
    if !(if directory { metadata.is_dir() } else { metadata.is_file() }) {
        return Err(invalid("destination must be a regular file with a directory parent"));
    }
    // Open existing only; attribute access, no data read/write, no ownership
    // lock. BACKUP_SEMANTICS is needed for the existing parent directory.
    let file = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(if directory { FILE_FLAG_BACKUP_SEMANTICS } else { 0 })
        .open(path)?;
    let metadata = file.metadata()?;
    if !(if directory { metadata.is_dir() } else { metadata.is_file() }) {
        return Err(invalid("destination changed kind while resolving"));
    }
    let mut info = FILE_ID_INFO::default();
    // SAFETY: file owns the live handle; info has the matching layout and size.
    // No fallback to truncated 64-bit IDs: unsupported drivers fail closed.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&mut info as *mut FILE_ID_INFO).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok((info.VolumeSerialNumber, info.FileId.Identifier))
}

#[cfg(unix)]
fn native_names_equal(a: &OsStr, b: &OsStr) -> bool {
    a == b
}

#[cfg(windows)]
fn native_names_equal(a: &OsStr, b: &OsStr) -> bool {
    use std::os::windows::ffi::OsStrExt;
    ordinal_equal(&a.encode_wide().collect::<Vec<_>>(), &b.encode_wide().collect::<Vec<_>>())
}

#[cfg(windows)]
fn ordinal_equal(a: &[u16], b: &[u16]) -> bool {
    use windows_sys::Win32::Globalization::{CSTR_EQUAL, CompareStringOrdinal};
    // SAFETY: slices remain live, lengths fit i32 under the 32 KiB limit, and
    // explicit lengths preserve unpaired surrogates. Failure overrejects.
    let result = unsafe { CompareStringOrdinal(a.as_ptr(), a.len() as i32, b.as_ptr(), b.len() as i32, 1) };
    result == CSTR_EQUAL || result == 0
}

#[cfg(windows)]
fn windows_filename(units: &[u16]) -> bool {
    if units.is_empty()
        || matches!(units.last(), Some(32 | 46))
        || units.iter().any(|&c| c < 32 || matches!(c, 34 | 42 | 47 | 58 | 60 | 62 | 63 | 92 | 124))
    {
        return false;
    }
    let stem = units.split(|&c| c == 46).next().unwrap_or_default();
    let ascii = |expected: &[u8]| {
        stem.len() == expected.len()
            && stem.iter().zip(expected).all(|(&a, &b)| a <= 127 && (a as u8).eq_ignore_ascii_case(&b))
    };
    !(ascii(b"CON")
        || ascii(b"PRN")
        || ascii(b"AUX")
        || ascii(b"NUL")
        || ascii(b"CONIN$")
        || ascii(b"CONOUT$")
        || (stem.len() == 4
            && (matches!(stem[3], 49..=57 | 0xb9 | 0xb2 | 0xb3))
            && (units_prefix_ascii(stem, b"COM") || units_prefix_ascii(stem, b"LPT"))))
}

#[cfg(windows)]
fn units_prefix_ascii(units: &[u16], prefix: &[u8]) -> bool {
    units.len() >= prefix.len()
        && units.iter().zip(prefix).all(|(&a, &b)| a <= 127 && (a as u8).eq_ignore_ascii_case(&b))
}

#[cfg(windows)]
fn windows_path_key(path: &Path) -> Option<Vec<u16>> {
    use std::{
        os::windows::ffi::OsStrExt,
        path::{Component, Prefix},
    };
    let mut key = Vec::new();
    let mut starts = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => match prefix.kind() {
                Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => key.extend([drive as u16, 58]),
                Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
                    key.extend([92, 92]);
                    key.extend(server.encode_wide());
                    key.push(92);
                    key.extend(share.encode_wide());
                }
                _ => return None,
            },
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => {
                if let Some(start) = starts.pop() {
                    key.truncate(start);
                }
            }
            Component::Normal(name) => {
                starts.push(key.len());
                key.push(92);
                key.extend(name.encode_wide());
            }
        }
    }
    Some(key)
}

// Primary contracts (checked against windows-sys 0.61.2 source):
// https://learn.microsoft.com/en-us/windows/win32/api/winbase/ns-winbase-file_id_info
// https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-getfileinformationbyhandleex
// https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-createfilew
// https://learn.microsoft.com/en-us/windows/win32/api/stringapiset/nf-stringapiset-comparestringordinal
// FileIdInfo/FILE_ID_INFO use Win32_Storage_FileSystem (already enabled).
// CompareStringOrdinal uses Win32_Globalization (additive feature only).
