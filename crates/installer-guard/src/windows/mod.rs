//! Actual Windows adapter. No writes, installers, process launch/termination,
//! registry enumeration, recursive walks, COM link resolution or network APIs.
mod db;
mod in_use;
mod paths;
mod registration;
mod registry;

use crate::{
    Deadline, Refusal, Result,
    model::{Package, Resource},
    policy::{self, Observation, Resources, Session},
    require, tables,
};
use windows_sys::Win32::{Foundation::*, Storage::FileSystem::*, System::ApplicationInstallationAndServicing::*};

pub(super) fn wide(text: &str) -> Result<Vec<u16>> {
    require(!text.contains('\0') && text.encode_utf16().count() < 32767, "limit", "Windows API text including NUL")?;
    Ok(text.encode_utf16().chain(std::iter::once(0)).collect())
}
pub(super) fn check(status: u32, operation: &str) -> Result<()> {
    require(status == ERROR_SUCCESS, "windows-api", format!("{operation}: {status}"))
}
pub(super) struct Handle(HANDLE);
impl Handle {
    fn new(value: HANDLE) -> Result<Self> {
        require(!value.is_null() && value != INVALID_HANDLE_VALUE, "windows-api", "missing handle")?;
        Ok(Self(value))
    }
}
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// Test-only backend/guard pairing seam. This opens only the supplied MSI in
/// read-only database mode and invokes the production parser. It deliberately
/// does not enter preflight, token/known-folder/registration/resource adapters,
/// or load/execute Binary.KwilPreflight.
#[cfg(test)]
pub(crate) fn validate_database_fixture(path: &std::path::Path) -> Result<Package> {
    use std::path::{Component, Prefix};
    require(
        path.is_absolute()
            && matches!(path.components().next(), Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_)))
            && !path.components().any(|part| matches!(part, Component::CurDir | Component::ParentDir)),
        "fixture-path",
        "explicit absolute local-drive MSI path",
    )?;
    let text = path.to_str().ok_or_else(|| Refusal::new("fixture-path", "non-Unicode MSI path"))?;
    require(
        path.extension().and_then(|ext| ext.to_str()).is_some_and(|ext| ext.eq_ignore_ascii_case("msi")),
        "fixture-path",
        "MSI extension",
    )?;
    let deadline = Deadline::new();
    let database = db::Db::cached(text, &deadline)?;
    tables::read(&database)
}

struct Adapter<'a> {
    context: &'a paths::Context,
    deadline: &'a Deadline,
    snapshots: Vec<(String, Observation)>,
    file_stamps: Vec<(String, Option<paths::Stamp>)>,
}
impl Adapter<'_> {
    fn file_path(&self, r: &Resource) -> Result<String> {
        let root = match r.root() {
            "I" => &self.context.install,
            "P" => &self.context.programs,
            _ => return Err(Refusal::new("invalid-inventory", "filesystem root")),
        };
        paths::child(root, r.relative())
    }
    fn observe_raw(&self, r: &Resource) -> Result<(Observation, Option<paths::Stamp>)> {
        self.deadline.check()?;
        if r.root() == "H" {
            return registry::observe(r, self.deadline).map(|seen| (seen, None)).map_err(|mut error| {
                error.resource = format!("{}: {}", r.identity(), error.resource);
                error
            });
        }
        require(r.kind() == "F", "invalid-inventory", "filesystem resource kind")?;
        let path = self.file_path(r)?;
        let evidence = paths::PathCheck::open(
            &path,
            GENERIC_READ | FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            self.deadline,
        )
        .map_err(|mut error| {
            error.resource = format!("{}: {}", r.identity(), error.resource);
            error
        })?;
        if evidence.file.is_none() {
            return Ok((Observation::Absent, None));
        }
        let (size, sha256) = evidence.hash(self.deadline).map_err(|mut error| {
            error.resource = format!("{}: {}", r.identity(), error.resource);
            error
        })?;
        Ok((Observation::File { size, sha256 }, evidence.stamp.clone()))
    }
}
impl Resources for Adapter<'_> {
    fn observe(&mut self, r: &Resource) -> Result<Observation> {
        let (observed, stamp) = self.observe_raw(r)?;
        let identity = r.identity();
        if r.kind() == "F" {
            if let Some((_, previous)) = self.file_stamps.iter().find(|(id, _)| id == &identity) {
                require(*previous == stamp, "unstable-evidence", format!("{identity} file identity changed"))?;
            } else {
                require(self.file_stamps.len() < 256, "limit", "original/incoming file identities")?;
                self.file_stamps.push((identity.clone(), stamp));
            }
        }
        if let Some((_, previous)) = self.snapshots.iter().find(|(id, _)| id == &identity) {
            require(observed == *previous, "unstable-evidence", identity)?;
        } else {
            require(self.snapshots.len() < 1024, "limit", "original/incoming observation count")?;
            self.snapshots.push((identity, observed.clone()));
        }
        Ok(observed)
    }
    fn portable_marker_absent(&mut self) -> Result<bool> {
        let path = paths::child(&self.context.install, "kwil.portable")?;
        let checked = paths::PathCheck::open(
            &path,
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            self.deadline,
        )?;
        checked.finish(self.deadline)?;
        Ok(checked.file.is_none())
    }
    fn in_use(&mut self, original: &Package) -> Result<()> {
        in_use::check(original, &self.context.install, &self.file_stamps, self.deadline)
    }
    fn install_root(&self) -> &str {
        &self.context.install
    }
}

fn session(session: u32, deadline: &Deadline) -> Result<Session> {
    let get = |name| db::property(deadline, session, name);
    let mut installed_state = 0;
    let mut action_state = 0;
    let main = wide("Main")?;
    deadline.check()?;
    check(
        unsafe { MsiGetFeatureStateW(session, main.as_ptr(), &mut installed_state, &mut action_state) },
        "Main feature state",
    )?;
    Ok(Session {
        remove: get("REMOVE")?,
        reinstall: get("REINSTALL")?,
        reinstall_mode: get("REINSTALLMODE")?,
        installed: !get("Installed")?.is_empty(),
        action: get("ACTION")?,
        main_action: action_state,
        older: get("KWIL_OLDER")?,
        equal: get("KWIL_EQUAL")?,
        newer: get("KWIL_NEWER")?,
    })
}
fn effective(session: u32, package: &Package, context: &paths::Context, deadline: &Deadline) -> Result<()> {
    tables::effective_properties(package, |name| db::property(deadline, session, name))?;
    for (name, path) in [
        ("LocalAppDataFolder", &context.local),
        ("INSTALLPROGRAMS", &context.install_programs),
        ("INSTALLDIR", &context.install),
        ("ProgramMenuFolder", &context.programs),
        ("ARPINSTALLLOCATION", &context.install),
    ] {
        let value = db::property(deadline, session, name)?;
        require(paths::same(&paths::directory(&value)?, path)?, "session-override", name)?;
    }
    for mode in [MSIRUNMODE_ADMIN, MSIRUNMODE_ADVERTISE, MSIRUNMODE_SCHEDULED, MSIRUNMODE_ROLLBACK, MSIRUNMODE_COMMIT] {
        deadline.check()?;
        require(unsafe { MsiGetMode(session, mode) } == 0, "unsupported-operation", format!("MSI run mode {mode}"))?;
    }
    Ok(())
}

pub fn preflight(handle: u32) -> Result<()> {
    require(handle != 0, "msi-api", "session handle")?;
    let deadline = Deadline::new();
    let active_db = db::Db::active(handle, &deadline)?;
    let active = tables::read(&active_db)?;
    let native_arch = if cfg!(target_arch = "x86_64") {
        "x64"
    } else if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "unsupported"
    };
    require(active.architecture == native_arch, "architecture-conflict", "guard native architecture")?;
    let context = paths::Context::resolve(&deadline)?;
    effective(handle, &active, &context, &deadline)?;
    let registered = registration::discover(&active.product_code, &context.sid, &deadline)?;
    let mut cached_path = None;
    let original = if let Some(reg) = &registered {
        let evidence = paths::PathCheck::open(
            &reg.cache,
            GENERIC_READ | FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            &deadline,
        )?;
        let info = evidence
            .stamp
            .as_ref()
            .ok_or_else(|| Refusal::new("package-mismatch", "registered cached original is missing"))?;
        require(
            info.attributes & FILE_ATTRIBUTE_DIRECTORY == 0 && info.links == 1,
            "unsafe-path",
            "cached original file type/hardlink",
        )?;
        let database = db::Db::cached(&reg.cache, &deadline)?;
        let original = tables::read(&database)?;
        registration::agrees(reg, &original, &context.install)?;
        require(original.architecture == active.architecture, "architecture-conflict", "registered original")?;
        evidence.finish(&deadline)?;
        cached_path = Some(evidence);
        Some(original)
    } else {
        None
    };
    let operation = policy::operation(&active, original.as_ref(), &session(handle, &deadline)?)?;
    let mut api = Adapter {
        context: &context,
        deadline: &deadline,
        snapshots: Vec::with_capacity(58),
        file_stamps: Vec::with_capacity(16),
    };
    policy::resources(&active, original.as_ref(), &mut api)?;
    // Last observations catch session/registration/path changes during hashing.
    effective(handle, &active, &context, &deadline)?;
    let final_session = session(handle, &deadline)?;
    require(
        policy::operation(&active, original.as_ref(), &final_session)? == operation,
        "unstable-evidence",
        "operation changed",
    )?;
    policy::detection(operation, original.as_ref(), &final_session)?;
    require(
        registration::discover(&active.product_code, &context.sid, &deadline)? == registered,
        "unstable-evidence",
        "MSI registration changed",
    )?;
    if let Some(evidence) = &cached_path {
        evidence.finish(&deadline)?;
    }
    require(api.portable_marker_absent()?, "portable-marker", "I:kwil.portable")?;
    deadline.check()?;
    Ok(())
}

pub fn report(session: u32, reason: &Refusal) {
    // Field 0 stays SQL NULL. Bound the formatted message INCLUDING the fixed
    // Error-table text, punctuation and instruction. Display truncation only.
    let prefix = format!("{} ", reason.code);
    let overhead = "Kwil installer preflight: . ".encode_utf16().count()
        + prefix.encode_utf16().count()
        + reason.instruction.encode_utf16().count();
    let budget = 1024usize.saturating_sub(overhead);
    let mut resource = String::new();
    let mut used = 0;
    for c in reason.resource.chars() {
        if used + c.len_utf16() > budget {
            break;
        }
        resource.push(c);
        used += c.len_utf16();
    }
    let message = format!("{prefix}{resource}");
    let result = (|| -> Result<()> {
        let record = db::Msi::new(unsafe { MsiCreateRecord(3) })?;
        let message = wide(&message)?;
        let instruction = wide(reason.instruction)?;
        check(unsafe { MsiRecordSetInteger(record.0, 1, 25001) }, "error record id")?;
        check(unsafe { MsiRecordSetStringW(record.0, 2, message.as_ptr()) }, "error record reason")?;
        check(unsafe { MsiRecordSetStringW(record.0, 3, instruction.as_ptr()) }, "error record instruction")?;
        unsafe {
            MsiProcessMessage(session, INSTALLMESSAGE_ERROR, record.0);
        }
        Ok(())
    })();
    // Diagnostic failure never alters the caller's unconditional 1603 result.
    let _ = result;
}
