use super::{Handle, wide};
use crate::{Deadline, Refusal, Result, identity::hex, require};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::Read,
    os::windows::io::{AsRawHandle, FromRawHandle},
    ptr::{null, null_mut},
};
use windows_sys::{
    Win32::{
        Foundation::*,
        Globalization::CompareStringOrdinal,
        Security::Authorization::ConvertSidToStringSidW,
        Security::*,
        Storage::FileSystem::*,
        System::{Com::CoTaskMemFree, Threading::*},
        UI::Shell::*,
    },
    core::GUID,
};

// Keep the portable adapter seam tied to the actual Windows SDK rights, without
// adding TOKEN_DUPLICATE, TOKEN_ADJUST_* or TOKEN_ALL_ACCESS.
const _: () = assert!(crate::token_access::KNOWN_FOLDER_TOKEN_ACCESS == (TOKEN_QUERY | TOKEN_IMPERSONATE));

pub fn same(a: &str, b: &str) -> Result<bool> {
    let a = wide(a)?;
    let b = wide(b)?;
    let result = unsafe { CompareStringOrdinal(a.as_ptr(), (a.len() - 1) as i32, b.as_ptr(), (b.len() - 1) as i32, 1) };
    require(result != 0, "windows-api", "CompareStringOrdinal")?;
    Ok(result == 2)
}
pub fn local_path(path: &str) -> Result<()> {
    require(
        path.encode_utf16().count() < 32767
            && path.len() >= 3
            && path.as_bytes()[0].is_ascii_alphabetic()
            && &path.as_bytes()[1..3] == b":\\"
            && !path.chars().any(|c| c == '/' || c == '\0' || c.is_control())
            && !path[2..].contains(':')
            && !path.ends_with("\\\\"),
        "unsafe-path",
        "absolute local drive path",
    )?;
    require(path[3..].trim_end_matches('\\').split('\\').count() <= 32, "limit", "filesystem path segments")?;
    for segment in path[3..].trim_end_matches('\\').split('\\') {
        if path.len() == 3 {
            break;
        }
        require(
            !segment.is_empty() && segment != "." && segment != ".." && !segment.ends_with(['.', ' ']),
            "unsafe-path",
            "path segment",
        )?;
    }
    Ok(())
}
pub fn directory(path: &str) -> Result<String> {
    local_path(path)?;
    require(
        path.trim_end_matches('\\').encode_utf16().count() + 2 <= 32767,
        "limit",
        "directory path including separator/NUL",
    )?;
    let mut s = path.trim_end_matches('\\').to_owned();
    s.push('\\');
    Ok(s)
}
pub fn child(root: &str, relative: &str) -> Result<String> {
    require(
        root.ends_with('\\') && root.encode_utf16().count() + relative.encode_utf16().count() < 32767,
        "limit",
        "resource path including NUL",
    )?;
    Ok(format!("{root}{}", relative.replace('/', "\\")))
}

fn folder(id: &GUID, token: HANDLE, deadline: &Deadline) -> Result<String> {
    deadline.check()?;
    let mut output = null_mut();
    let status = unsafe { SHGetKnownFolderPath(id, KF_FLAG_DONT_VERIFY as u32, token, &mut output) };
    struct Memory(*mut u16);
    impl Drop for Memory {
        fn drop(&mut self) {
            unsafe {
                CoTaskMemFree(self.0.cast());
            }
        }
    }
    let output = Memory(output);
    require(status >= 0 && !output.0.is_null(), "known-folder", format!("HRESULT {status:#x}"))?;
    let mut count = 0;
    while count < 32767 {
        if unsafe { *output.0.add(count) } == 0 {
            break;
        }
        count += 1;
    }
    require(count < 32767, "limit", "known-folder path")?;
    let text = String::from_utf16(unsafe { std::slice::from_raw_parts(output.0, count) })
        .map_err(|_| Refusal::new("known-folder", "invalid UTF-16"))?;
    directory(&text)
}

fn token_u32(token: HANDLE, class: TOKEN_INFORMATION_CLASS, deadline: &Deadline) -> Result<u32> {
    let mut value = 0u32;
    let mut needed = 0;
    deadline.check()?;
    require(
        unsafe { GetTokenInformation(token, class, (&mut value as *mut u32).cast(), 4, &mut needed) } != 0
            && needed == 4,
        "token-context",
        "token integer",
    )?;
    Ok(value)
}
fn sid(token: HANDLE, deadline: &Deadline) -> Result<String> {
    // TOKEN_USER + SID has a system-defined maximum (SECURITY_MAX_SID_SIZE=68).
    // usize storage supplies TOKEN_USER alignment; no arbitrary token-sized allocation.
    let mut storage = [0usize; 32];
    let mut needed = 0;
    deadline.check()?;
    require(
        unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                storage.as_mut_ptr().cast(),
                std::mem::size_of_val(&storage) as u32,
                &mut needed,
            )
        } != 0
            && needed as usize <= std::mem::size_of_val(&storage)
            && needed as usize >= std::mem::size_of::<TOKEN_USER>(),
        "token-context",
        "TOKEN_USER",
    )?;
    let user = unsafe { &*storage.as_ptr().cast::<TOKEN_USER>() };
    let begin = storage.as_ptr() as usize;
    let ptr = user.User.Sid as usize;
    require(
        ptr >= begin && ptr.checked_add(8).is_some_and(|end| end <= begin + needed as usize),
        "token-context",
        "SID pointer",
    )?;
    require(unsafe { IsValidSid(user.User.Sid) } != 0, "token-context", "valid SID")?;
    let length = unsafe { GetLengthSid(user.User.Sid) } as usize;
    require(
        length <= 68 && ptr.checked_add(length).is_some_and(|end| end <= begin + needed as usize),
        "token-context",
        "SID length",
    )?;
    let mut text = null_mut();
    deadline.check()?;
    require(
        unsafe { ConvertSidToStringSidW(user.User.Sid, &mut text) } != 0 && !text.is_null(),
        "token-context",
        "SID conversion",
    )?;
    struct Local(*mut u16);
    impl Drop for Local {
        fn drop(&mut self) {
            unsafe {
                LocalFree(self.0.cast());
            }
        }
    }
    let text = Local(text);
    let mut count = 0;
    while count < 184 && unsafe { *text.0.add(count) } != 0 {
        count += 1;
    }
    require(count < 184, "token-context", "SID string bound")?;
    String::from_utf16(unsafe { std::slice::from_raw_parts(text.0, count) })
        .map_err(|_| Refusal::new("token-context", "SID UTF-16"))
}

pub struct Context {
    pub _token: Handle,
    pub sid: String,
    pub local: String,
    pub programs: String,
    pub install_programs: String,
    pub install: String,
}
impl Context {
    pub fn resolve(deadline: &Deadline) -> Result<Self> {
        let token = crate::token_access::effective_token(
            |access| {
                deadline.check()?;
                let mut process_token = null_mut();
                require(
                    unsafe { OpenProcessToken(GetCurrentProcess(), access, &mut process_token) } != 0,
                    "token-context",
                    "process token",
                )?;
                Handle::new(process_token)
            },
            |access| {
                deadline.check()?;
                let mut thread_token = null_mut();
                if unsafe { OpenThreadToken(GetCurrentThread(), access, 1, &mut thread_token) } != 0 {
                    Ok(Some(Handle::new(thread_token)?))
                } else {
                    require(unsafe { GetLastError() } == ERROR_NO_TOKEN, "token-context", "thread token access")?;
                    Ok(None)
                }
            },
        )?;
        let current_sid = sid(token.0, deadline)?;
        // Immediate MSI actions may run on an impersonating installer thread.
        // The effective thread token (process token only when not impersonating)
        // is the current-user authority for both known folders and registration.
        require(
            !["S-1-5-18", "S-1-5-19", "S-1-5-20"].contains(&current_sid.as_str())
                && token_u32(token.0, TokenElevation, deadline)? == 0
                && token_u32(token.0, TokenElevationType, deadline)? != TokenElevationTypeFull as u32
                && token_u32(token.0, TokenSessionId, deadline)? != 0,
            "token-context",
            "non-elevated current-user token",
        )?;
        let local = folder(&FOLDERID_LocalAppData, token.0, deadline)?;
        let programs = folder(&FOLDERID_Programs, token.0, deadline)?;
        require(local.encode_utf16().count() + "Programs\\Kwil\\".len() < 32767, "limit", "derived install root")?;
        let install_programs = format!("{local}Programs\\");
        let install = format!("{install_programs}Kwil\\");
        for path in [&local, &programs, &install_programs, &install] {
            let checked = PathCheck::open(
                path,
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                deadline,
            )?;
            if let Some(stamp) = &checked.stamp {
                require(stamp.attributes & FILE_ATTRIBUTE_DIRECTORY != 0, "unsafe-path", "known-folder/root type")?;
            }
            checked.finish(deadline)?;
        }
        Ok(Self { _token: token, sid: current_sid, local, programs, install_programs, install })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stamp {
    pub volume: u32,
    pub identity: u64,
    pub length: u64,
    pub modified: u64,
    pub links: u32,
    pub attributes: u32,
}
pub(super) fn stamp(file: &File, deadline: &Deadline) -> Result<Stamp> {
    deadline.check()?;
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    require(
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } != 0,
        "filesystem-api",
        "file identity",
    )?;
    require(
        info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0
            && unsafe { GetFileType(file.as_raw_handle()) } == FILE_TYPE_DISK,
        "unsafe-path",
        "reparse/special file",
    )?;
    Ok(Stamp {
        volume: info.dwVolumeSerialNumber,
        identity: ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
        length: ((info.nFileSizeHigh as u64) << 32) | info.nFileSizeLow as u64,
        modified: ((info.ftLastWriteTime.dwHighDateTime as u64) << 32) | info.ftLastWriteTime.dwLowDateTime as u64,
        links: info.nNumberOfLinks,
        attributes: info.dwFileAttributes,
    })
}
fn final_path(file: &File, deadline: &Deadline) -> Result<String> {
    deadline.check()?;
    let needed = unsafe { GetFinalPathNameByHandleW(file.as_raw_handle(), null_mut(), 0, 0) };
    require(needed > 0 && needed <= 32767, "filesystem-api", "canonical path size")?;
    let mut buffer = vec![0u16; needed as usize];
    deadline.check()?;
    let count = unsafe { GetFinalPathNameByHandleW(file.as_raw_handle(), buffer.as_mut_ptr(), needed, 0) };
    require(
        count > 0 && count < needed && count + 1 == needed && buffer[count as usize] == 0,
        "unstable-evidence",
        "canonical path fill",
    )?;
    let path = String::from_utf16(&buffer[..count as usize])
        .map_err(|_| Refusal::new("unsafe-path", "canonical path UTF-16"))?;
    let path =
        path.strip_prefix("\\\\?\\").ok_or_else(|| Refusal::new("unsafe-path", "canonical DOS path"))?.to_owned();
    local_path(&path)?;
    Ok(path)
}
fn open(path: &str, access: u32, share: u32, deadline: &Deadline) -> Result<Option<File>> {
    let path = wide(path)?;
    deadline.check()?;
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            access,
            share,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let status = unsafe { GetLastError() };
        if matches!(status, ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) {
            return Ok(None);
        }
        return Err(Refusal::new(
            if matches!(status, ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION) { "in-use" } else { "filesystem-api" },
            format!("CreateFile status {status}"),
        ));
    }
    Ok(Some(unsafe { File::from_raw_handle(handle) }))
}

pub struct PathCheck {
    path: String,
    parents: Vec<(usize, File, Stamp)>,
    missing: Option<usize>,
    pub file: Option<File>,
    pub stamp: Option<Stamp>,
}
impl PathCheck {
    pub fn open(path: &str, access: u32, share: u32, deadline: &Deadline) -> Result<Self> {
        local_path(path)?;
        let path = if path.len() > 3 { path.trim_end_matches('\\') } else { path };
        let root = wide(&path[..3])?;
        deadline.check()?;
        require(matches!(unsafe { GetDriveTypeW(root.as_ptr()) }, 2 | 3 | 5 | 6), "unsafe-path", "local volume")?;
        let mut parents = Vec::new();
        let mut volume = None;
        // Store bounded offsets and handles, not a quadratic set of path strings.
        let ends = std::iter::once(3)
            .chain(path.char_indices().filter_map(|(i, c)| if c == '\\' && i > 2 { Some(i) } else { None }))
            .filter(|end| *end < path.len());
        let mut missing = None;
        for end in ends {
            require(parents.len() < 32, "limit", "filesystem ancestry")?;
            let prefix = &path[..end];
            let Some(file) =
                open(prefix, FILE_READ_ATTRIBUTES, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, deadline)?
            else {
                missing = Some(end);
                break;
            };
            let info = stamp(&file, deadline)?;
            require(
                info.attributes & FILE_ATTRIBUTE_DIRECTORY != 0
                    && same(&final_path(&file, deadline)?, prefix)?
                    && volume.is_none_or(|v| v == info.volume),
                "unsafe-path",
                "ancestor directory/canonical path/volume",
            )?;
            volume = Some(info.volume);
            parents.push((end, file, info));
        }
        let file = open(path, access, share, deadline)?;
        let info = file.as_ref().map(|f| stamp(f, deadline)).transpose()?;
        if let (Some(file), Some(info)) = (&file, &info) {
            require(
                missing.is_none()
                    && volume.is_none_or(|v| v == info.volume)
                    && same(&final_path(file, deadline)?, path)?,
                "unsafe-path",
                "leaf canonical identity/volume",
            )?;
        }
        let checked = Self { path: path.to_owned(), parents, missing, file, stamp: info };
        checked.finish(deadline)?;
        Ok(checked)
    }
    pub fn finish(&self, deadline: &Deadline) -> Result<()> {
        for (end, file, original) in &self.parents {
            require(
                stamp(file, deadline)? == *original && same(&final_path(file, deadline)?, &self.path[..*end])?,
                "unstable-evidence",
                "filesystem ancestor changed",
            )?;
            let current = open(
                &self.path[..*end],
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                deadline,
            )?
            .ok_or_else(|| Refusal::new("unstable-evidence", "ancestor disappeared"))?;
            require(stamp(&current, deadline)? == *original, "unstable-evidence", "ancestor replaced")?;
        }
        if let Some(end) = self.missing {
            require(
                open(
                    &self.path[..end],
                    FILE_READ_ATTRIBUTES,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    deadline,
                )?
                .is_none(),
                "unstable-evidence",
                "missing ancestor appeared",
            )?;
        }
        let current =
            open(&self.path, FILE_READ_ATTRIBUTES, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, deadline)?;
        let now = current.as_ref().map(|f| stamp(f, deadline)).transpose()?;
        require(now == self.stamp, "unstable-evidence", "exact resource changed")?;
        if let (Some(file), Some(original)) = (&self.file, &self.stamp) {
            require(
                stamp(file, deadline)? == *original && same(&final_path(file, deadline)?, &self.path)?,
                "unstable-evidence",
                "open resource changed",
            )?;
        }
        Ok(())
    }
    pub fn hash(&self, deadline: &Deadline) -> Result<(u64, String)> {
        let file = self.file.as_ref().ok_or_else(|| Refusal::new("filesystem-api", "hash absent file"))?;
        let info = self.stamp.as_ref().ok_or_else(|| Refusal::new("filesystem-api", "hash without identity"))?;
        require(
            info.attributes & FILE_ATTRIBUTE_DIRECTORY == 0 && info.links == 1 && info.length <= 536_870_912,
            "unsafe-path",
            "regular file/hardlink/length",
        )?;
        let mut reader = file;
        let mut bytes = [0u8; 65536];
        let mut count = 0u64;
        let mut hash = Sha256::new();
        loop {
            deadline.check()?;
            let n = reader.read(&mut bytes).map_err(|_| Refusal::new("filesystem-api", "stream file read"))?;
            if n == 0 {
                break;
            }
            count = count.checked_add(n as u64).ok_or_else(|| Refusal::new("limit", "file bytes"))?;
            require(count <= info.length, "unstable-evidence", "file grew")?;
            hash.update(&bytes[..n]);
        }
        require(count == info.length, "unstable-evidence", "file length")?;
        self.finish(deadline)?;
        Ok((count, hex(&hash.finalize())))
    }
}
