//! Explicit, per-user association transactions. Planning only reads state.
//! Review and persist the serializable plan/receipt before calling apply on a
//! worker. Tests use `AssociationAdapter` fakes, never a native constructor.
//!
//! Windows registers Open With only; protected defaults require the OS chooser.
//! macOS requires an installed bundle with declared document types; changing a
//! Launch Services role is reversible only if a previous handler exists.
//! Linux edits the XDG desktop/mimeapps files used by xdg-mime, without a shell.
//! Conflict checks are optimistic: platform APIs do not offer a cross-process
//! compare-and-swap spanning inspection and replacement. Hosts must serialize
//! their own actions; an external edit racing the final check may still win.
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, fmt, path::PathBuf};

pub mod worker;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MacDocumentRole {
    Viewer,
    Editor,
}
#[cfg(target_os = "macos")]
impl MacDocumentRole {
    fn mask(self) -> u32 {
        match self {
            Self::Viewer => 2,
            Self::Editor => 4,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Location {
    WindowsValue { subkey: String, name: String },
    UserFile(PathBuf),
    MacRole { uti: String, role: MacDocumentRole },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredValue {
    pub kind: u32,
    pub bytes: Vec<u8>,
}
impl StoredValue {
    fn text(s: &str) -> Self {
        Self { kind: 0, bytes: s.as_bytes().to_vec() }
    }
    fn registry(s: &str) -> Self {
        Self { kind: 1, bytes: s.encode_utf16().chain(Some(0)).flat_map(u16::to_le_bytes).collect() }
    }
    fn as_text(&self) -> Result<&str, AssociationError> {
        std::str::from_utf8(&self.bytes).map_err(|_| err("unsupported non-UTF-8 association file"))
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssociationError(pub String);
impl fmt::Display for AssociationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for AssociationError {}
fn err(s: impl Into<String>) -> AssociationError {
    AssociationError(s.into())
}

/// Adapters must make each individual write atomic or leave it unchanged on
/// error. Only the native platform adapter may touch that platform's resources.
pub trait AssociationAdapter {
    /// Stable per-user/backend identity, included in persisted ownership receipts.
    fn owner(&self) -> String;
    fn read(&mut self, location: &Location) -> Result<Option<StoredValue>, AssociationError>;
    fn write(&mut self, location: &Location, value: Option<&StoredValue>) -> Result<(), AssociationError>;
    /// Persist restored state (including removals and already-visible old values)
    /// before its durable recovery receipt may be retired. Called on the worker
    /// after complete rollback with the ORIGINAL full change list. Visibility or
    /// platform notification alone is insufficient. Unsupported backends must
    /// keep recovery rather than promise durability they cannot establish.
    fn sync_restored(&mut self, _changes: &[Change]) -> Result<(), AssociationError> {
        Err(err("backend cannot verify durable association restoration; retain recovery receipt"))
    }
    /// Notify the platform after owned mutations; never runs during planning.
    fn notify_changes(&mut self) {}
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DefaultChoice {
    KeepCurrent,
    MakeDefault,
}
#[derive(Clone, Debug)]
pub struct FileType {
    pub extension: String,
    pub content_type: Option<String>,
}
#[derive(Clone, Debug)]
pub enum AssociationPlatform {
    Windows,
    MacOS { bundle_id: String },
    Linux { data_home: PathBuf, config_home: PathBuf },
}
#[derive(Clone, Debug)]
pub struct AssociationRequest {
    pub platform: AssociationPlatform,
    pub executable: PathBuf,
    pub types: Vec<FileType>,
    pub default_choice: DefaultChoice,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub location: Location,
    pub before: Option<StoredValue>,
    pub after: Option<StoredValue>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AssociationPlan {
    owner: String,
    changes: Vec<Change>,
    pub notes: Vec<String>,
    pub default_choice: DefaultChoice,
}
impl AssociationPlan {
    pub fn owner(&self) -> &str {
        &self.owner
    }
    pub fn changes(&self) -> &[Change] {
        &self.changes
    }
}
/// A durable rollback journal, including the exact bytes and registry types
/// replaced by this operation. Store it in the user's application data.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AssociationReceipt {
    owner: String,
    remaining: Vec<Change>,
}
impl AssociationReceipt {
    pub fn owner(&self) -> &str {
        &self.owner
    }
    pub fn pending(&self) -> &[Change] {
        &self.remaining
    }
    pub fn is_rolled_back(&self) -> bool {
        self.remaining.is_empty()
    }
}
#[derive(Debug)]
pub struct ApplyFailure {
    pub error: AssociationError,
    pub receipt: AssociationReceipt,
}
impl fmt::Display for ApplyFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for ApplyFailure {}

pub fn plan(
    adapter: &mut impl AssociationAdapter,
    request: &AssociationRequest,
) -> Result<AssociationPlan, AssociationError> {
    if !request.executable.is_absolute() || request.types.is_empty() || request.types.len() > 128 {
        return Err(err("choose an absolute executable and 1 to 128 extensions"));
    }
    let executable = request.executable.to_str().ok_or_else(|| err("executable must be Unicode"))?;
    if executable.chars().any(|c| c.is_control() || c == '"' || c == '%') {
        return Err(err("executable contains unsupported association command characters"));
    }
    let mut desired = Vec::new();
    let mut linux_snapshot = None;
    let mut notes = Vec::new();
    let mut extensions = BTreeSet::new();
    let mut content_types = BTreeSet::new();
    for file_type in &request.types {
        let ext = crate::settings_ui::normalize_extension(&file_type.extension).map_err(|e| err(e.to_string()))?;
        if !extensions.insert(ext.clone()) {
            return Err(err("duplicate extension"));
        }
        match &request.platform {
            AssociationPlatform::Windows => {
                if request.default_choice == DefaultChoice::MakeDefault {
                    return Err(err(
                        "Windows protected defaults require an explicit choice in system Default Apps; register Open With first",
                    ));
                }
                let progid = format!("Kwil.{ext}");
                for (key, name, value) in [
                    (progid.clone(), String::new(), StoredValue::registry(&format!("Kwil {ext} document"))),
                    (
                        format!("{progid}\\shell\\open\\command"),
                        String::new(),
                        StoredValue::registry(&format!("\"{executable}\" \"%1\"")),
                    ),
                    (format!(".{ext}\\OpenWithProgids"), progid, StoredValue { kind: 0, bytes: Vec::new() }),
                ] {
                    desired.push((Location::WindowsValue { subkey: key, name }, value));
                }
            }
            AssociationPlatform::MacOS { bundle_id } => {
                if !identifier(bundle_id, false) {
                    return Err(err("invalid macOS bundle identifier"));
                }
                let uti = file_type
                    .content_type
                    .as_deref()
                    .filter(|s| identifier(s, false))
                    .ok_or_else(|| err("macOS requires an existing uniform type identifier"))?;
                if request.default_choice == DefaultChoice::KeepCurrent {
                    return Err(err(
                        "macOS registration requires an installed app bundle declaring the selected types; standalone bundle registration has no reversible public unregister API",
                    ));
                }
                if content_types.insert(uti.to_owned()) {
                    for role in [MacDocumentRole::Viewer, MacDocumentRole::Editor] {
                        desired.push((Location::MacRole { uti: uti.into(), role }, StoredValue::text(bundle_id)));
                    }
                }
            }
            AssociationPlatform::Linux { .. } => {
                let mime=file_type.content_type.as_deref().filter(|s|s.split_once('/').is_some_and(|(a,b)|identifier(a,true)&&identifier(b,true))).ok_or_else(||err("Linux requires the chosen extension's existing MIME type; custom MIME database installation is unsupported"))?;
                content_types.insert(mime.to_owned());
            }
        }
    }
    if let AssociationPlatform::Linux { data_home, config_home } = &request.platform {
        if !data_home.is_absolute() || !config_home.is_absolute() {
            return Err(err("XDG user data/config paths must be absolute"));
        }
        let desktop = format!(
            "[Desktop Entry]\nType=Application\nName=Kwil\nExec=\"{}\" %f\nTerminal=false\nNoDisplay=true\nMimeType={};\n",
            desktop_escape(executable),
            content_types.iter().cloned().collect::<Vec<_>>().join(";")
        );
        desired.push((Location::UserFile(data_home.join("applications/kwil.desktop")), StoredValue::text(&desktop)));
        let location = Location::UserFile(config_home.join("mimeapps.list"));
        let old = adapter.read(&location)?;
        linux_snapshot = Some((location.clone(), old.clone()));
        let mut source = old.as_ref().map(StoredValue::as_text).transpose()?.unwrap_or("").to_owned();
        for mime in &content_types {
            source = change_ini_list(&source, "Removed Associations", mime, "kwil.desktop", None)?;
            source = edit_ini_list(&source, "Added Associations", mime, "kwil.desktop", false)?;
            if request.default_choice == DefaultChoice::MakeDefault {
                source = edit_ini_list(&source, "Default Applications", mime, "kwil.desktop", true)?;
            }
        }
        desired.push((location, StoredValue::text(&source)));
        notes.push("Uses user XDG mimeapps.list (the xdg-mime format). Desktop-specific higher-priority defaults may override this file. Existing MIME types only; no shell or cache-refresh program is launched.".into());
    }
    if matches!(request.platform, AssociationPlatform::Windows) {
        notes.push("Registers per-user ProgIDs and Open With under HKCU\\Software\\Classes. Default Apps choice stays with the user. Rollback removes owned values only; empty registry containers may remain.".into());
    }
    if matches!(request.platform, AssociationPlatform::MacOS { .. }) {
        notes.push("Changes Launch Services viewer and editor roles with separate previous-handler receipts. Bundle discovery belongs to packaging; no public reversible standalone bundle-registration API is used.".into());
    }
    let mut changes = Vec::new();
    for (location, after) in desired {
        let before = match &linux_snapshot {
            Some((snap_location, before)) if *snap_location == location => before.clone(),
            _ => adapter.read(&location)?,
        };
        if matches!(location, Location::MacRole { .. }) && before.is_none() {
            return Err(err("cannot reversibly change a Launch Services role with no prior handler"));
        }
        if matches!(location, Location::MacRole { .. })
            && before.as_ref().is_some_and(|v| v.kind != 0 || !v.as_text().is_ok_and(|s| identifier(s, false)))
        {
            return Err(err("prior Launch Services handler cannot be restored safely"));
        }
        if before.as_ref() != Some(&after) {
            changes.push(Change { location, before, after: Some(after) });
        }
    }
    Ok(AssociationPlan { owner: adapter.owner(), changes, notes, default_choice: request.default_choice })
}

pub fn apply(
    adapter: &mut impl AssociationAdapter,
    plan: &AssociationPlan,
) -> Result<AssociationReceipt, ApplyFailure> {
    let mut receipt = AssociationReceipt { owner: plan.owner.clone(), remaining: Vec::new() };
    let execute =
        |adapter: &mut dyn AssociationAdapter, receipt: &mut AssociationReceipt| -> Result<(), AssociationError> {
            if adapter.owner() != plan.owner {
                return Err(err("association plan belongs to another user/backend"));
            }
            for change in &plan.changes {
                if adapter.read(&change.location)? != change.before {
                    return Err(err("association state changed since planning; review a new plan"));
                }
            }
            for change in &plan.changes {
                if adapter.read(&change.location)? != change.before {
                    return Err(err("association state changed during apply"));
                }
                adapter.write(&change.location, change.after.as_ref())?;
                receipt.remaining.push(change.clone());
            }
            Ok(())
        };
    if let Err(error) = execute(adapter, &mut receipt) {
        let recovery = rollback(adapter, &mut receipt);
        let error = if recovery.is_empty() {
            error
        } else {
            err(format!(
                "{error}; rollback incomplete: {}",
                recovery.iter().map(ToString::to_string).collect::<Vec<_>>().join("; ")
            ))
        };
        return Err(ApplyFailure { error, receipt });
    }
    if !receipt.remaining.is_empty() {
        adapter.notify_changes();
    }
    Ok(receipt)
}
/// Idempotent rollback. Detected foreign edits are conflicts, never overwritten. Keep
/// the returned pending receipt when any restoration fails; it can be retried.
pub fn rollback(adapter: &mut impl AssociationAdapter, receipt: &mut AssociationReceipt) -> Vec<AssociationError> {
    if adapter.owner() != receipt.owner {
        return vec![err("association receipt belongs to another user/backend")];
    }
    let mut failures = Vec::new();
    let mut pending = Vec::new();
    for change in receipt.remaining.drain(..).rev() {
        let restore = (|| {
            let current = adapter.read(&change.location)?;
            if current == change.before {
                return Ok(());
            }
            if current != change.after {
                return Err(err("association was changed by another application; refusing rollback overwrite"));
            }
            adapter.write(&change.location, change.before.as_ref())
        })();
        if let Err(e) = restore {
            failures.push(e);
            pending.push(change);
        }
    }
    pending.reverse();
    receipt.remaining = pending;
    adapter.notify_changes();
    failures
}

fn identifier(s: &str, mime: bool) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && s.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_') || mime && c == b'+')
}
fn desktop_escape(s: &str) -> String {
    s.replace('\\', "\\\\\\\\").replace('`', "\\\\`").replace('$', "\\\\$")
}
// Preserve all unrelated lines/comments. Refuse duplicate groups or keys rather
// than guess which the desktop will use. Change only the selected list value.
fn edit_ini_list(source: &str, group: &str, key: &str, entry: &str, first: bool) -> Result<String, AssociationError> {
    change_ini_list(source, group, key, entry, Some(first))
}
fn change_ini_list(
    source: &str,
    group: &str,
    key: &str,
    entry: &str,
    add: Option<bool>,
) -> Result<String, AssociationError> {
    let newline = if source.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines: Vec<String> = source.lines().map(str::to_owned).collect();
    let header = format!("[{group}]");
    let groups: Vec<_> = lines.iter().enumerate().filter(|(_, l)| l.trim() == header).map(|(i, _)| i).collect();
    if groups.len() > 1 {
        return Err(err("unsupported duplicate mimeapps group"));
    }
    if add.is_none() && groups.is_empty() {
        return Ok(source.to_owned());
    }
    let start = if let Some(i) = groups.first() {
        *i
    } else {
        if lines.last().is_some_and(|l| !l.is_empty()) {
            lines.push(String::new());
        }
        lines.push(header);
        lines.len() - 1
    };
    let end = (start + 1..lines.len()).find(|&i| lines[i].trim().starts_with('[')).unwrap_or(lines.len());
    let matches: Vec<_> =
        (start + 1..end).filter(|&i| lines[i].split_once('=').is_some_and(|(k, _)| k.trim() == key)).collect();
    if matches.len() > 1 {
        return Err(err("unsupported duplicate mimeapps key"));
    }
    if add.is_none()
        && (matches.is_empty()
            || !lines[matches[0]].split_once('=').expect("matched key").1.split(';').any(|s| s.trim() == entry))
    {
        return Ok(source.to_owned());
    }
    let (index, mut values) = if let Some(i) = matches.first() {
        (
            *i,
            lines[*i]
                .split_once('=')
                .unwrap()
                .1
                .split(';')
                .filter(|s| !s.is_empty() && s.trim() != entry)
                .map(str::to_owned)
                .collect::<Vec<_>>(),
        )
    } else {
        (end, Vec::new())
    };
    if let Some(first) = add {
        if first {
            values.insert(0, entry.into());
        } else {
            values.push(entry.into());
        }
    }
    let line = format!("{key}={};", values.join(";"));
    if matches.is_empty() {
        lines.insert(index, line);
    } else {
        lines[index] = line;
    }
    Ok(lines.join(newline) + newline)
}

/// Native Linux adapter is rooted in explicitly supplied per-user XDG paths,
/// not arbitrary paths from a deserialized receipt. No global files or shells.
pub struct LinuxAssociations {
    data_home: PathBuf,
    config_home: PathBuf,
}
impl LinuxAssociations {
    pub fn new(data_home: PathBuf, config_home: PathBuf) -> Result<Self, AssociationError> {
        if !data_home.is_absolute() || !config_home.is_absolute() {
            return Err(err("absolute user XDG paths required"));
        }
        Ok(Self { data_home, config_home })
    }
    fn path<'a>(&self, location: &'a Location) -> Result<&'a std::path::Path, AssociationError> {
        if let Location::UserFile(path) = location
            && (*path == self.data_home.join("applications/kwil.desktop")
                || *path == self.config_home.join("mimeapps.list"))
        {
            return Ok(path);
        }
        Err(err("resource is outside this user's association files"))
    }
}
impl AssociationAdapter for LinuxAssociations {
    fn owner(&self) -> String {
        format!("xdg:{}:{}", self.data_home.display(), self.config_home.display())
    }
    #[cfg(target_os = "linux")]
    fn sync_restored(&mut self, changes: &[Change]) -> Result<(), AssociationError> {
        for change in changes {
            let path = self.path(&change.location)?;
            linux_sync_restored_file(path, change.before.is_some()).map_err(|e| err(e.to_string()))?;
        }
        Ok(())
    }
    fn read(&mut self, location: &Location) -> Result<Option<StoredValue>, AssociationError> {
        let path = self.path(location)?;
        match std::fs::symlink_metadata(path) {
            Ok(m) if m.is_file() && !m.file_type().is_symlink() => {}
            Ok(_) => return Err(err("association path is not a regular file")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(err(e.to_string())),
        }
        let mut bytes = Vec::new();
        use std::io::Read;
        std::fs::File::open(path)
            .and_then(|f| f.take(1024 * 1024 + 1).read_to_end(&mut bytes))
            .map_err(|e| err(e.to_string()))?;
        if bytes.len() > 1024 * 1024 {
            return Err(err("association file exceeds 1 MiB"));
        }
        Ok(Some(StoredValue { kind: 0, bytes }))
    }
    fn write(&mut self, location: &Location, value: Option<&StoredValue>) -> Result<(), AssociationError> {
        let path = self.path(location)?.to_owned();
        let current = self.read(location)?;
        if let Some(value) = value {
            if value.kind != 0 || value.bytes.len() > 1024 * 1024 {
                return Err(err("invalid association file value"));
            }
            write_user_file(&path, &value.bytes).map_err(|e| err(e.to_string()))
        } else if current.is_some() {
            std::fs::remove_file(path).map_err(|e| err(e.to_string()))
        } else {
            Ok(())
        }
    }
}
// Open one component relative to an already-validated directory. O_NOFOLLOW
// only protects the last component, so whole-path opens are insufficient.
#[cfg(target_os = "linux")]
fn linux_open_sync_at(
    parent: std::os::fd::RawFd,
    name: &std::ffi::OsStr,
    directory: bool,
) -> std::io::Result<std::fs::File> {
    use std::{
        ffi::CString,
        os::{fd::FromRawFd, unix::ffi::OsStrExt},
    };
    let name = CString::new(name.as_bytes()).map_err(|_| std::io::Error::other("invalid association component"))?;
    let flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NONBLOCK | libc::O_NOFOLLOW;
    let flags = if directory { flags | libc::O_DIRECTORY } else { flags };
    // No create flag: no mode argument is needed. A successful descriptor is
    // uniquely owned below, including every metadata/error path.
    let fd = unsafe { libc::openat(parent, name.as_ptr(), flags) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    if if directory { !metadata.is_dir() } else { !metadata.is_file() } {
        return Err(std::io::Error::other("association sync descriptor has an unsafe resource type"));
    }
    Ok(file)
}
#[cfg(target_os = "linux")]
fn linux_sync_restored_file(path: &std::path::Path, must_exist: bool) -> std::io::Result<()> {
    use std::{os::fd::AsRawFd, path::Component};
    let mut components = path.components().peekable();
    if components.next() != Some(Component::RootDir) || components.peek().is_none() {
        return Err(std::io::Error::other("association sync needs an absolute file path"));
    }
    let mut directories = vec![linux_open_sync_at(libc::AT_FDCWD, std::ffi::OsStr::new("/"), true)?];
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err(std::io::Error::other("association sync refuses non-normal path components"));
        };
        let directory = components.peek().is_some();
        match linux_open_sync_at(directories.last().expect("root directory").as_raw_fd(), name, directory) {
            Ok(file) if directory => directories.push(file),
            Ok(file) => file.sync_all()?,
            // An absent prior file may have absent parents when Apply never
            // reached it. Still sync the existing ancestor descriptors below.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !must_exist => break,
            Err(e) => return Err(e),
        }
    }
    // Retain and sync the actual opened directories: never re-open their paths
    // after validation. This persists rename/unlink and newly-created ancestry.
    for directory in directories.iter().rev() {
        directory.sync_all()?;
    }
    Ok(())
}
fn write_user_file(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    let parent = path.parent().ok_or_else(|| std::io::Error::other("file needs a parent"))?;
    std::fs::create_dir_all(parent)?;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let temp =
        parent.join(format!(".kwil-associations-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp)?;
    let result = (|| {
        file.write_all(bytes)?;
        if let Ok(m) = std::fs::metadata(path) {
            file.set_permissions(m.permissions())?;
        }
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    result
}

#[cfg(windows)]
pub struct WindowsAssociations {
    owner: String,
}
#[cfg(windows)]
impl WindowsAssociations {
    /// `user_identity` is the account SID supplied by the host. No registry I/O.
    pub fn new(user_identity: String) -> Result<Self, AssociationError> {
        if user_identity.is_empty() {
            return Err(err("user identity is required"));
        }
        Ok(Self { owner: format!("HKCU:{user_identity}") })
    }
}
#[cfg(windows)]
impl AssociationAdapter for WindowsAssociations {
    fn owner(&self) -> String {
        self.owner.clone()
    }
    fn read(&mut self, location: &Location) -> Result<Option<StoredValue>, AssociationError> {
        windows_registry(location, None, false)
    }
    fn write(&mut self, location: &Location, value: Option<&StoredValue>) -> Result<(), AssociationError> {
        windows_registry(location, value, true).map(|_| ())
    }
    fn sync_restored(&mut self, changes: &[Change]) -> Result<(), AssociationError> {
        // Validate receipt resources through the same per-user restriction as
        // reads/writes. A missing value/key still requires the hive barrier.
        for change in changes {
            self.read(&change.location)?;
        }
        if changes.is_empty() {
            return Ok(());
        }
        // Flush the Classes key's containing hive (Classes can be mounted in a
        // separate hive from HKCU itself). One barrier, not one per value.
        use windows_sys::Win32::{Foundation::ERROR_SUCCESS, System::Registry::*};
        let key: Vec<u16> = "Software\\Classes".encode_utf16().chain(Some(0)).collect();
        let mut handle = std::ptr::null_mut();
        let status = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, key.as_ptr(), 0, KEY_QUERY_VALUE, &mut handle) };
        if status != ERROR_SUCCESS {
            return Err(err(std::io::Error::from_raw_os_error(status as i32).to_string()));
        }
        let status = unsafe { RegFlushKey(handle) };
        unsafe {
            RegCloseKey(handle);
        }
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(err(std::io::Error::from_raw_os_error(status as i32).to_string()))
        }
    }
    fn notify_changes(&mut self) {
        use windows_sys::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify};
        unsafe {
            SHChangeNotify(SHCNE_ASSOCCHANGED as i32, SHCNF_IDLIST, std::ptr::null(), std::ptr::null());
        }
    }
}
#[cfg(windows)]
fn windows_registry(
    location: &Location,
    value: Option<&StoredValue>,
    write: bool,
) -> Result<Option<StoredValue>, AssociationError> {
    use windows_sys::Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS},
        System::Registry::*,
    };
    let Location::WindowsValue { subkey, name } = location else {
        return Err(err("expected a per-user registry value"));
    };
    // Receipts are untrusted input: never permit escaping Classes or writing
    // another ProgID/default/UserChoice. Only resources generated by plan.
    let valid = if let Some(rest) = subkey.strip_prefix("Kwil.") {
        let ext = rest.split('\\').next().unwrap_or("");
        crate::settings_ui::normalize_extension(ext).is_ok()
            && (rest == ext || rest == format!("{ext}\\shell\\open\\command"))
            && name.is_empty()
    } else if let Some(ext) = subkey.strip_prefix('.').and_then(|s| s.strip_suffix("\\OpenWithProgids")) {
        crate::settings_ui::normalize_extension(ext).is_ok() && *name == format!("Kwil.{ext}")
    } else {
        false
    };
    if !valid {
        return Err(err("registry resource is outside owned Kwil registration values"));
    }
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let key = wide(&format!("Software\\Classes\\{subkey}"));
    let name = wide(name);
    let mut handle = std::ptr::null_mut();
    let access = if write { KEY_QUERY_VALUE | KEY_SET_VALUE } else { KEY_QUERY_VALUE };
    let status = unsafe {
        if write && value.is_some() {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                0,
                std::ptr::null(),
                0,
                access,
                std::ptr::null(),
                &mut handle,
                std::ptr::null_mut(),
            )
        } else {
            RegOpenKeyExW(HKEY_CURRENT_USER, key.as_ptr(), 0, access, &mut handle)
        }
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if status != ERROR_SUCCESS {
        return Err(err(std::io::Error::from_raw_os_error(status as i32).to_string()));
    }
    struct Key(windows_sys::Win32::System::Registry::HKEY);
    impl Drop for Key {
        fn drop(&mut self) {
            unsafe {
                RegCloseKey(self.0);
            }
        }
    }
    let handle = Key(handle);
    if write {
        let status = unsafe {
            match value {
                Some(v) => {
                    if v.bytes.len() > 1024 * 1024 {
                        return Err(err("registry value exceeds 1 MiB"));
                    }
                    RegSetValueExW(handle.0, name.as_ptr(), 0, v.kind, v.bytes.as_ptr(), v.bytes.len() as u32)
                }
                None => RegDeleteValueW(handle.0, name.as_ptr()),
            }
        };
        return if status == ERROR_SUCCESS || status == ERROR_FILE_NOT_FOUND {
            Ok(None)
        } else {
            Err(err(std::io::Error::from_raw_os_error(status as i32).to_string()))
        };
    }
    let mut kind = 0;
    let mut length = 0;
    let status = unsafe {
        RegQueryValueExW(handle.0, name.as_ptr(), std::ptr::null(), &mut kind, std::ptr::null_mut(), &mut length)
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if status != ERROR_SUCCESS || length > 1024 * 1024 {
        return Err(err("cannot read bounded registry value"));
    }
    let mut bytes = vec![0; length as usize];
    let status = unsafe {
        RegQueryValueExW(handle.0, name.as_ptr(), std::ptr::null(), &mut kind, bytes.as_mut_ptr(), &mut length)
    };
    if status != ERROR_SUCCESS {
        return Err(err("registry changed while reading; review a new plan"));
    }
    bytes.truncate(length as usize);
    Ok(Some(StoredValue { kind, bytes }))
}

#[cfg(target_os = "macos")]
pub struct MacAssociations {
    owner: String,
}
#[cfg(target_os = "macos")]
impl MacAssociations {
    pub fn new(user_identity: String) -> Result<Self, AssociationError> {
        if user_identity.is_empty() {
            return Err(err("user identity is required"));
        }
        Ok(Self { owner: format!("LaunchServices:{user_identity}") })
    }
}
#[cfg(target_os = "macos")]
impl AssociationAdapter for MacAssociations {
    fn owner(&self) -> String {
        self.owner.clone()
    }
    fn read(&mut self, location: &Location) -> Result<Option<StoredValue>, AssociationError> {
        mac_role(location, None)
    }
    fn write(&mut self, location: &Location, value: Option<&StoredValue>) -> Result<(), AssociationError> {
        let value = value.ok_or_else(|| err("Launch Services cannot restore an absent role handler"))?;
        mac_role(location, Some(value)).map(|_| ())
    }
}
#[cfg(target_os = "macos")]
fn mac_role(location: &Location, new: Option<&StoredValue>) -> Result<Option<StoredValue>, AssociationError> {
    use std::ffi::{c_char, c_void};
    type CFString = *const c_void;
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithBytes(a: *const c_void, b: *const u8, n: isize, encoding: u32, external: u8) -> CFString;
        fn CFStringGetLength(s: CFString) -> isize;
        fn CFStringGetCString(s: CFString, b: *mut c_char, n: isize, encoding: u32) -> u8;
        fn CFRelease(s: *const c_void);
    }
    #[link(name = "CoreServices", kind = "framework")]
    unsafe extern "C" {
        fn LSCopyDefaultRoleHandlerForContentType(uti: CFString, role: u32) -> CFString;
        fn LSSetDefaultRoleHandlerForContentType(uti: CFString, role: u32, bundle: CFString) -> i32;
    }
    struct StringRef(CFString);
    impl Drop for StringRef {
        fn drop(&mut self) {
            unsafe {
                CFRelease(self.0);
            }
        }
    }
    fn string(s: &str) -> Result<StringRef, AssociationError> {
        let p = unsafe { CFStringCreateWithBytes(std::ptr::null(), s.as_ptr(), s.len() as isize, 0x08000100, 0) };
        if p.is_null() { Err(err("cannot create Core Foundation string")) } else { Ok(StringRef(p)) }
    }
    let Location::MacRole { uti, role } = location else {
        return Err(err("expected Launch Services content type"));
    };
    if !identifier(uti, false) {
        return Err(err("invalid content type"));
    }
    let uti = string(uti)?;
    // Each role has its own receipt. Never set kLSRolesAll: it would overwrite
    // other role handlers without preserving their distinct prior values.
    if let Some(new) = new {
        let bundle = new.as_text()?;
        if !identifier(bundle, false) {
            return Err(err("invalid bundle identifier"));
        }
        let bundle = string(bundle)?;
        let status = unsafe { LSSetDefaultRoleHandlerForContentType(uti.0, role.mask(), bundle.0) };
        return if status == 0 { Ok(None) } else { Err(err(format!("Launch Services failed: {status}"))) };
    }
    let p = unsafe { LSCopyDefaultRoleHandlerForContentType(uti.0, role.mask()) };
    if p.is_null() {
        return Ok(None);
    }
    let p = StringRef(p);
    let length = unsafe { CFStringGetLength(p.0) };
    if !(0..=65536).contains(&length) {
        return Err(err("Launch Services handler exceeds bound"));
    }
    let mut bytes = vec![0u8; length as usize * 4 + 1];
    if unsafe { CFStringGetCString(p.0, bytes.as_mut_ptr().cast(), bytes.len() as isize, 0x08000100) } == 0 {
        return Err(err("cannot decode Launch Services handler"));
    }
    bytes.truncate(bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len()));
    Ok(Some(StoredValue { kind: 0, bytes }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Fake {
        values: Vec<(Location, StoredValue)>,
        writes: usize,
        fail_on: Option<usize>,
    }
    impl AssociationAdapter for Fake {
        fn owner(&self) -> String {
            "fake-user".into()
        }
        fn read(&mut self, l: &Location) -> Result<Option<StoredValue>, AssociationError> {
            Ok(self.values.iter().find(|(k, _)| k == l).map(|(_, v)| v.clone()))
        }
        fn write(&mut self, l: &Location, v: Option<&StoredValue>) -> Result<(), AssociationError> {
            self.writes += 1;
            if self.fail_on == Some(self.writes) {
                return Err(err("injected write failure"));
            }
            self.values.retain(|(k, _)| k != l);
            if let Some(v) = v {
                self.values.push((l.clone(), v.clone()));
            }
            Ok(())
        }
    }
    fn request() -> AssociationRequest {
        AssociationRequest {
            platform: AssociationPlatform::Windows,
            executable: std::env::current_dir().unwrap().join("kwil.exe"),
            types: vec![FileType { extension: "txt".into(), content_type: None }],
            default_choice: DefaultChoice::KeepCurrent,
        }
    }
    #[test]
    fn association_failure_rolls_back_and_receipt_never_overwrites_foreign_edits() {
        let mut adapter = Fake::default();
        let plan = plan(&mut adapter, &request()).unwrap();
        assert_eq!(adapter.writes, 0);
        adapter.fail_on = Some(3);
        let failure = apply(&mut adapter, &plan).unwrap_err();
        assert!(failure.receipt.is_rolled_back());
        assert!(adapter.values.is_empty());
        adapter.fail_on = None;
        let mut receipt = apply(&mut adapter, &plan).unwrap();
        let location = receipt.remaining[0].location.clone();
        adapter.write(&location, Some(&StoredValue::text("foreign"))).unwrap();
        assert_eq!(rollback(&mut adapter, &mut receipt).len(), 1);
        assert_eq!(receipt.pending().len(), 1);
        assert_eq!(adapter.read(&location).unwrap(), Some(StoredValue::text("foreign")));
        adapter.write(&location, receipt.remaining[0].after.as_ref()).unwrap();
        assert!(rollback(&mut adapter, &mut receipt).is_empty());
        assert!(adapter.values.is_empty());
        assert!(rollback(&mut adapter, &mut receipt).is_empty());
    }
    #[test]
    fn associations_require_explicit_default_choice_and_restore_linux_bytes() {
        let mut adapter = Fake::default();
        let mut req = request();
        req.default_choice = DefaultChoice::MakeDefault;
        assert!(plan(&mut adapter, &req).is_err());
        assert_eq!(adapter.writes, 0);
        let root = std::env::current_dir().unwrap();
        req.platform =
            AssociationPlatform::Linux { data_home: root.join("fake-data"), config_home: root.join("fake-config") };
        req.types[0].content_type = Some("text/plain".into());
        let location = Location::UserFile(root.join("fake-config/mimeapps.list"));
        let original = StoredValue::text("# owner\n[Default Applications]\ntext/plain=other.desktop;\n");
        adapter.write(&location, Some(&original)).unwrap();
        let before = adapter.values.clone();
        let p = plan(&mut adapter, &req).unwrap();
        let mut receipt = apply(&mut adapter, &p).unwrap();
        let current = adapter.read(&location).unwrap().unwrap();
        assert!(current.as_text().unwrap().contains("text/plain=kwil.desktop;other.desktop;"));
        assert!(current.as_text().unwrap().contains("# owner"));
        assert!(rollback(&mut adapter, &mut receipt).is_empty());
        assert_eq!(adapter.values, before);
    }
    #[test]
    fn stale_plan_and_foreign_owner_are_rejected_before_writing() {
        let mut adapter = Fake::default();
        let mut p = plan(&mut adapter, &request()).unwrap();
        p.owner = "someone else".into();
        assert!(apply(&mut adapter, &p).is_err());
        assert_eq!(adapter.writes, 0);
        p.owner = adapter.owner();
        adapter.write(&p.changes[0].location, Some(&StoredValue::text("new"))).unwrap();
        let writes = adapter.writes;
        assert!(apply(&mut adapter, &p).is_err());
        assert_eq!(adapter.writes, writes);
    }
    #[test]
    fn mac_roles_restore_distinct_handlers_and_refuse_missing_prior_default() {
        let mut adapter = Fake::default();
        let mut req = request();
        req.platform = AssociationPlatform::MacOS { bundle_id: "dev.renner.kwil".into() };
        req.default_choice = DefaultChoice::MakeDefault;
        req.types[0].content_type = Some("public.plain-text".into());
        assert!(plan(&mut adapter, &req).is_err());
        assert_eq!(adapter.writes, 0);
        for (role, handler) in [(MacDocumentRole::Viewer, "other.viewer"), (MacDocumentRole::Editor, "other.editor")] {
            adapter
                .write(&Location::MacRole { uti: "public.plain-text".into(), role }, Some(&StoredValue::text(handler)))
                .unwrap();
        }
        let before = adapter.values.clone();
        let plan = plan(&mut adapter, &req).unwrap();
        assert_eq!(plan.changes().len(), 2);
        let mut receipt = apply(&mut adapter, &plan).unwrap();
        assert!(rollback(&mut adapter, &mut receipt).is_empty());
        for (location, value) in before {
            assert_eq!(adapter.read(&location).unwrap(), Some(value));
        }
    }
    #[test]
    fn linux_registration_removes_only_chosen_removed_entry_and_rolls_back_exact_bytes() {
        let mut adapter = Fake::default();
        let mut req = request();
        let root = std::env::current_dir().unwrap();
        req.platform =
            AssociationPlatform::Linux { data_home: root.join("fake-data"), config_home: root.join("fake-config") };
        req.types[0].content_type = Some("text/plain".into());
        let location = Location::UserFile(root.join("fake-config/mimeapps.list"));
        let original = StoredValue::text(
            "# owner\n[Removed Associations]\n# preserve this\ntext/plain=other.desktop;kwil.desktop;third.desktop;\napplication/json=kwil.desktop;\n[Default Applications]\ntext/plain=other.desktop;\n",
        );
        adapter.write(&location, Some(&original)).unwrap();
        let p = plan(&mut adapter, &req).unwrap();
        assert_eq!(adapter.read(&location).unwrap(), Some(original.clone()), "planning cannot mutate state");
        let mut receipt = apply(&mut adapter, &p).unwrap();
        let after = adapter.read(&location).unwrap().unwrap();
        let after = after.as_text().unwrap();
        assert!(after.contains("text/plain=other.desktop;third.desktop;"));
        assert!(after.contains("application/json=kwil.desktop;"));
        assert!(after.contains("# preserve this"));
        assert!(after.contains("[Added Associations]\ntext/plain=kwil.desktop;"));
        assert!(after.contains("[Default Applications]\ntext/plain=other.desktop;"));
        assert!(rollback(&mut adapter, &mut receipt).is_empty());
        assert_eq!(adapter.read(&location).unwrap(), Some(original));
    }
}
