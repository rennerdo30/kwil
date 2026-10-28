use super::{check, wide};
use crate::{Deadline, Refusal, Result, model::Resource, policy::Observation, require};
use std::ptr::{null, null_mut};
use windows_sys::Win32::{Foundation::*, System::Registry::*};

struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            RegCloseKey(self.0);
        }
    }
}
const ACCESS: u32 = KEY_QUERY_VALUE | KEY_WOW64_64KEY;
fn open(parent: HKEY, name: &str, deadline: &Deadline) -> Result<Option<Key>> {
    let name = wide(name)?;
    let mut key = null_mut();
    deadline.check()?;
    let status = unsafe { RegOpenKeyExW(parent, name.as_ptr(), REG_OPTION_OPEN_LINK, ACCESS, &mut key) };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    check(status, "native64 read-only registry open")?;
    Ok(Some(Key(key)))
}
fn no_link(key: HKEY, deadline: &Deadline) -> Result<()> {
    let name = wide("SymbolicLinkValue")?;
    let mut ty = 0;
    let mut size = 0;
    deadline.check()?;
    let status = unsafe { RegQueryValueExW(key, name.as_ptr(), null(), &mut ty, null_mut(), &mut size) };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    check(status, "registry link inspection")?;
    require(ty != REG_LINK, "unsafe-registry", "registry symbolic link")
}
fn modified(key: HKEY, deadline: &Deadline) -> Result<u64> {
    let mut time = FILETIME::default();
    deadline.check()?;
    check(
        unsafe {
            RegQueryInfoKeyW(
                key,
                null_mut(),
                null_mut(),
                null(),
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
                &mut time,
            )
        },
        "registry key observation",
    )?;
    Ok(((time.dwHighDateTime as u64) << 32) | time.dwLowDateTime as u64)
}

struct Ancestry {
    root: Key,
    keys: Vec<(String, Key, u64)>,
    missing: Option<String>,
}
impl Ancestry {
    fn open(path: &str, deadline: &Deadline) -> Result<Self> {
        let count = path.split('\\').count();
        require(path.len() <= 240 && count <= 64, "limit", "registry ancestry")?;
        let mut root = null_mut();
        deadline.check()?;
        check(unsafe { RegOpenCurrentUser(ACCESS, &mut root) }, "current-user native64 registry")?;
        let root = Key(root);
        no_link(root.0, deadline)?;
        let mut keys = Vec::with_capacity(count);
        let mut missing = None;
        for part in path.split('\\') {
            require(!part.is_empty(), "unsafe-registry", "empty key segment")?;
            let parent = keys.last().map(|(_, k, _): &(String, Key, u64)| k.0).unwrap_or(root.0);
            let Some(key) = open(parent, part, deadline)? else {
                missing = Some(part.to_owned());
                break;
            };
            no_link(key.0, deadline)?;
            let time = modified(key.0, deadline)?;
            keys.push((part.to_owned(), key, time));
        }
        Ok(Self { root, keys, missing })
    }
    fn finish(&self, deadline: &Deadline) -> Result<()> {
        no_link(self.root.0, deadline)?;
        let mut parent = self.root.0;
        for (part, key, time) in &self.keys {
            no_link(key.0, deadline)?;
            require(modified(key.0, deadline)? == *time, "unstable-evidence", "registry ancestor changed")?;
            let current = open(parent, part, deadline)?
                .ok_or_else(|| Refusal::new("unstable-evidence", "registry ancestor disappeared"))?;
            no_link(current.0, deadline)?;
            require(modified(current.0, deadline)? == *time, "unstable-evidence", "registry ancestor replaced")?;
            parent = key.0;
        }
        if let Some(part) = &self.missing {
            require(
                open(parent, part, deadline)?.is_none(),
                "unstable-evidence",
                "missing registry ancestor appeared",
            )?;
        }
        Ok(())
    }
}

pub fn observe(resource: &Resource, deadline: &Deadline) -> Result<Observation> {
    let path = Ancestry::open(resource.relative(), deadline)?;
    if path.missing.is_some() {
        path.finish(deadline)?;
        return Ok(Observation::Absent);
    }
    let key = path.keys.last().ok_or_else(|| Refusal::new("unsafe-registry", "missing key ancestry"))?.1.0;
    if resource.kind() == "K" {
        path.finish(deadline)?;
        return Ok(Observation::Key);
    }
    require(resource.kind() == "R", "internal-error", "registry resource kind")?;
    let name = if resource.name() == "@" {
        ""
    } else {
        resource.name().strip_prefix("n:").ok_or_else(|| Refusal::new("invalid-inventory", "value name"))?
    };
    require(name.len() <= 128, "limit", "registry value name")?;
    let name = wide(name)?;
    let mut ty = 0;
    let mut size = 0;
    deadline.check()?;
    let status = unsafe { RegQueryValueExW(key, name.as_ptr(), null(), &mut ty, null_mut(), &mut size) };
    if status == ERROR_FILE_NOT_FOUND {
        // Confirm exact absence rather than treating an API failure as absent.
        deadline.check()?;
        require(
            unsafe { RegQueryValueExW(key, name.as_ptr(), null(), &mut ty, null_mut(), &mut size) }
                == ERROR_FILE_NOT_FOUND,
            "unstable-evidence",
            "registry value appeared",
        )?;
        path.finish(deadline)?;
        return Ok(Observation::Absent);
    }
    check(status, "registry value size")?;
    require(size <= 8192, "limit", "registry value bytes")?;
    let mut bytes = vec![0u8; size as usize];
    let original_type = ty;
    let original_size = size;
    deadline.check()?;
    check(
        unsafe { RegQueryValueExW(key, name.as_ptr(), null(), &mut ty, bytes.as_mut_ptr(), &mut size) },
        "registry value read",
    )?;
    require(ty == original_type && size == original_size, "unstable-evidence", "registry value type/length changed")?;
    path.finish(deadline)?;
    Ok(Observation::Value { value_type: ty, bytes })
}
