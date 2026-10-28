use super::{
    paths::{self, PathCheck},
    wide,
};
use crate::{Deadline, Refusal, Result, model::Package, require};
use std::{
    fs::File,
    os::windows::io::FromRawHandle,
    ptr::{null, null_mut},
};
use windows_sys::Win32::{Foundation::*, Storage::FileSystem::*, System::RestartManager::*};

struct Rm {
    handle: u32,
    closed: bool,
}
impl Rm {
    fn close(&mut self) -> Result<()> {
        self.closed = true;
        let status = unsafe { RmEndSession(self.handle) };
        require(status == ERROR_SUCCESS, "in-use", format!("Restart Manager end status {status}"))
    }
}
impl Drop for Rm {
    fn drop(&mut self) {
        if !self.closed {
            unsafe {
                RmEndSession(self.handle);
            }
        }
    }
}
fn rm_check(status: u32, stage: &str) -> Result<()> {
    require(status == ERROR_SUCCESS, "in-use", format!("Restart Manager {stage}: {status}"))
}

pub fn check(
    original: &Package,
    install: &str,
    expected: &[(String, Option<paths::Stamp>)],
    deadline: &Deadline,
) -> Result<()> {
    let mut pair = Vec::with_capacity(2);
    for name in ["kwil.exe", "kwil.com"] {
        let row = original
            .resources
            .iter()
            .find(|r| r.kind() == "F" && r.root() == "I" && r.relative() == name)
            .ok_or_else(|| Refusal::new("invalid-inventory", name))?;
        let path = paths::child(install, name)?;
        let evidence = PathCheck::open(
            &path,
            GENERIC_READ | FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            deadline,
        )?;
        let previous = expected
            .iter()
            .find(|(id, _)| *id == row.identity())
            .ok_or_else(|| Refusal::new("in-use", "missing prior pair observation"))?;
        require(previous.1 == evidence.stamp, "in-use", "current-pair path/identity set changed")?;
        if evidence.file.is_some() {
            let (size, hash) = evidence.hash(deadline)?;
            require(size.to_string() == row.size() && hash == row.hash(), "changed-owned-file", row.identity())?;
        }
        pair.push((path, evidence));
    }
    let filenames = pair.iter().filter(|(_, e)| e.file.is_some()).map(|(p, _)| wide(p)).collect::<Result<Vec<_>>>()?;
    let names: Vec<_> = filenames.iter().map(|p| p.as_ptr()).collect();
    if !names.is_empty() {
        let mut handle = 0;
        let mut key = [0u16; 33];
        deadline.check()?;
        rm_check(unsafe { RmStartSession(&mut handle, 0, key.as_mut_ptr()) }, "start")?;
        let mut session = Rm { handle, closed: false };
        let result = (|| {
            deadline.check()?;
            rm_check(
                unsafe { RmRegisterResources(handle, names.len() as u32, names.as_ptr(), 0, null(), 0, null()) },
                "register current pair",
            )?;
            let mut needed = 0;
            let mut count = 0;
            let mut reboot = 0;
            deadline.check()?;
            let status = unsafe { RmGetList(handle, &mut needed, &mut count, null_mut(), &mut reboot) };
            require(reboot == 0, "in-use", "Restart Manager reboot reason")?;
            if status == ERROR_MORE_DATA {
                require(needed > 0 && needed <= 256 && count == 0, "in-use", "Restart Manager process count")?;
                let initial = needed;
                let mut processes = vec![RM_PROCESS_INFO::default(); needed as usize];
                count = needed;
                deadline.check()?;
                rm_check(
                    unsafe { RmGetList(handle, &mut needed, &mut count, processes.as_mut_ptr(), &mut reboot) },
                    "list bounded retry",
                )?;
                require(
                    needed == initial && count == initial,
                    "in-use",
                    "Restart Manager affected process set changed",
                )?;
            } else {
                rm_check(status, "list")?;
            }
            require(count == 0 && needed == 0 && reboot == 0, "in-use", "affected process/reboot reason")
        })();
        // End on both success and refusal; an end failure also refuses.
        let ended = session.close();
        result?;
        ended?;
    }
    let mut probes = Vec::with_capacity(2);
    for (path, evidence) in &pair {
        if let Some(original_stamp) = &evidence.stamp {
            let encoded = wide(path)?;
            deadline.check()?;
            let handle = unsafe {
                CreateFileW(
                    encoded.as_ptr(),
                    GENERIC_READ | FILE_READ_ATTRIBUTES,
                    FILE_SHARE_READ,
                    null(),
                    OPEN_EXISTING,
                    FILE_FLAG_OPEN_REPARSE_POINT,
                    null_mut(),
                )
            };
            require(
                handle != INVALID_HANDLE_VALUE,
                "in-use",
                format!("sharing probe {path}: {}", unsafe { GetLastError() }),
            )?;
            let file = unsafe { File::from_raw_handle(handle) };
            require(paths::stamp(&file, deadline)? == *original_stamp, "in-use", "current-pair identity changed")?;
            probes.push(file);
        }
        // Includes exact missing-member recheck when only one or neither exists.
        evidence.finish(deadline)?;
    }
    for (_, evidence) in &pair {
        evidence.finish(deadline)?;
    }
    drop(probes); // No sharing handle survives into MSI's transaction.
    Ok(())
}
