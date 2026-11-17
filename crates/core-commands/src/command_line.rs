//! Command-line registration (KW-90), Windows only, per user, no elevation:
//! an App Paths entry (`HKCU\Software\Microsoft\Windows\CurrentVersion\App Paths\kwil.exe`,
//! so Win+R finds `kwil`) and the install folder appended to the user `PATH`
//! (`HKCU\Environment`, so a new terminal runs `kwil.com`: `.COM` precedes `.EXE`).
//!
//! What to write is decided by pure functions over the [`Registry`] trait; the
//! thin Win32 layer is [`native::WindowsRegistry`], whose root is injected so
//! tests use a throwaway key. [`gate`] refuses native access in hidden test mode,
//! isolated runs and unit tests, before any registry access.
use std::fmt;
use std::path::{Path, PathBuf};

/// macOS and Linux get `kwil` on the path from `scripts/install.sh`; no choice there.
pub const SUPPORTED: bool = cfg!(windows);

pub const APP_PATHS_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\App Paths\kwil.exe";
pub const ENVIRONMENT_SUBKEY: &str = "Environment";
const PATH_VALUE: &str = "Path";
const MAX_FOLDER_CHARS: usize = 240;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandLineError(pub String);
impl fmt::Display for CommandLineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for CommandLineError {}
fn err<T>(message: impl Into<String>) -> Result<T, CommandLineError> {
    Err(CommandLineError(message.into()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueKind {
    /// `REG_SZ`
    Sz,
    /// `REG_EXPAND_SZ`
    ExpandSz,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Value {
    pub kind: ValueKind,
    pub text: String,
}
impl Value {
    pub fn sz(text: impl Into<String>) -> Self {
        Self { kind: ValueKind::Sz, text: text.into() }
    }
}

/// Registry access for the two keys above; an empty `name` is the default value.
/// Missing keys and values are `Ok(None)` / `Ok(())`. A value that is neither
/// `REG_SZ` nor `REG_EXPAND_SZ` is an error from `read`, so it is never rewritten.
pub trait Registry {
    fn read(&mut self, subkey: &str, name: &str) -> Result<Option<Value>, CommandLineError>;
    fn write(&mut self, subkey: &str, name: &str, value: &Value) -> Result<(), CommandLineError>;
    fn delete_value(&mut self, subkey: &str, name: &str) -> Result<(), CommandLineError>;
    fn delete_key(&mut self, subkey: &str) -> Result<(), CommandLineError>;
    fn environment_changed(&mut self);
}

/// Comparison form of one PATH entry (quotes, trailing separators and case do not count).
fn entry_key(entry: &str) -> String {
    let trimmed = entry.trim();
    let unquoted = trimmed.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(trimmed);
    unquoted.trim_end_matches(['\\', '/']).to_lowercase()
}

pub fn path_contains(path: &str, folder: &str) -> bool {
    let wanted = entry_key(folder);
    !wanted.is_empty() && path.split(';').any(|entry| entry_key(entry) == wanted)
}

/// `path` with `folder` appended, or `None` when already listed. A trailing `;`
/// stays trailing, so removing the entry restores the original text exactly.
pub fn path_with(path: &str, folder: &str) -> Option<String> {
    if path_contains(path, folder) {
        return None;
    }
    Some(if path.is_empty() {
        folder.to_owned()
    } else if path.ends_with(';') {
        format!("{path}{folder};")
    } else {
        format!("{path};{folder}")
    })
}

/// `path` without `folder`, or `None` when not listed; other entries keep their order.
pub fn path_without(path: &str, folder: &str) -> Option<String> {
    if !path_contains(path, folder) {
        return None;
    }
    let wanted = entry_key(folder);
    let kept: Vec<&str> = path.split(';').filter(|entry| entry_key(entry) != wanted).collect();
    Some(kept.join(";"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppPaths {
    Missing,
    Ours,
    /// Points at another `kwil.exe` (another install or a portable copy).
    Foreign,
}

fn same_file_text(a: &str, b: &str) -> bool {
    let key = |s: &str| s.trim().trim_matches('"').replace('/', "\\").to_lowercase();
    key(a) == key(b)
}

pub fn classify_app_paths(default_value: Option<&Value>, exe: &str) -> AppPaths {
    match default_value {
        None => AppPaths::Missing,
        Some(value) if value.kind == ValueKind::Sz && same_file_text(&value.text, exe) => AppPaths::Ours,
        Some(_) => AppPaths::Foreign,
    }
}

/// One install: `kwil.exe` and its folder, normalised, local-disk, no trailing backslash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Install {
    pub exe: String,
    pub folder: String,
}

/// The folder as Explorer would launch it (the shared path check, minus a trailing separator).
fn explorer_text(folder: &Path) -> Result<String, CommandLineError> {
    let raw = folder.to_string_lossy();
    let trimmed = raw.trim_end_matches(['\\', '/']);
    crate::associations::windows_paths::explorer_executable(Path::new(trimmed))
        .map_err(|e| CommandLineError(e.to_string()))
}

/// Text rules, no file system access: refuses network and device paths, drive
/// roots, `temp_dirs` and anything that cannot live in a PATH entry.
pub fn validate_folder(folder: &Path, temp_dirs: &[PathBuf]) -> Result<String, CommandLineError> {
    let raw = folder.to_string_lossy();
    let raw = raw.strip_prefix(r"\\?\").unwrap_or(&raw);
    if raw.len() >= 2
        && raw.as_bytes()[0].is_ascii_alphabetic()
        && raw.as_bytes()[1] == b':'
        && raw[2..].trim_matches(['\\', '/']).is_empty()
    {
        return err("Kwil is in the root of a drive. Move it to its own folder first.");
    }
    let text = explorer_text(folder)?;
    let local = text.as_bytes().get(..3).is_some_and(|p| p[0].is_ascii_alphabetic() && &p[1..] == b":\\");
    if !local {
        return err("Kwil is in a network or device location. Move it to a folder on a local drive first.");
    }
    if text.contains(';') {
        return err("This folder name contains a semicolon, which Windows cannot keep in your PATH.");
    }
    let folder = text.trim_end_matches('\\').to_owned();
    if folder.chars().count() > MAX_FOLDER_CHARS {
        return err("This folder path is too long to add to your PATH. Move Kwil to a shorter folder.");
    }
    let lowered = folder.to_lowercase();
    for temp in temp_dirs {
        let Ok(temp) = crate::associations::windows_paths::explorer_executable(temp) else { continue };
        let temp = temp.trim_end_matches('\\').to_lowercase();
        if temp.len() > 2 && (lowered == temp || lowered.starts_with(&format!("{temp}\\"))) {
            return err(
                "Kwil is in a temporary folder, which Windows may clean up. Move it to a permanent folder first.",
            );
        }
    }
    Ok(folder)
}

/// The folders Windows and this process use for temporary files.
pub fn temp_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![std::env::temp_dir()];
    for name in ["TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(name) {
            dirs.push(PathBuf::from(value));
        }
    }
    if let Some(root) = std::env::var_os("SystemRoot") {
        dirs.push(Path::new(&root).join("Temp"));
    }
    dirs
}

/// Where to register: the running executable's folder (installed or portable),
/// checked on disk. `kwil.exe` must exist beside it; no linked folder on the way.
#[cfg(windows)]
pub fn locate(current_exe: &Path) -> Result<Install, CommandLineError> {
    use std::os::windows::fs::MetadataExt;
    let folder = current_exe.parent().ok_or_else(|| CommandLineError("Kwil's own folder is unknown.".into()))?;
    let folder = validate_folder(folder, &temp_dirs())?;
    let install = Install { exe: format!("{folder}\\kwil.exe"), folder };
    let mut current = PathBuf::new();
    for component in Path::new(&install.folder).components() {
        current.push(component);
        if matches!(component, std::path::Component::Prefix(_) | std::path::Component::RootDir) {
            continue;
        }
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_attributes() & 0x400 != 0 => {
                return err(
                    "Kwil is in a linked folder (junction or symbolic link). Move it to a normal folder first.",
                );
            }
            Ok(_) => {}
            Err(e) => return err(format!("Cannot inspect Kwil's folder: {e}")),
        }
    }
    match std::fs::symlink_metadata(&install.exe) {
        Ok(meta) if meta.is_file() && meta.file_attributes() & 0x400 == 0 => Ok(install),
        _ => err("kwil.exe was not found next to this program, so there is nothing to register."),
    }
}
#[cfg(not(windows))]
pub fn locate(_current_exe: &Path) -> Result<Install, CommandLineError> {
    err("Putting Kwil on the PATH is only needed on Windows; scripts/install.sh already links it.")
}

/// Removal and status only recognise this install's own entries, so they skip
/// the checks that refuse a new registration.
pub fn locate_existing(current_exe: &Path) -> Result<Install, CommandLineError> {
    let folder = current_exe.parent().ok_or_else(|| CommandLineError("Kwil's own folder is unknown.".into()))?;
    let text = explorer_text(folder)?;
    let folder = text.trim_end_matches('\\').to_owned();
    Ok(Install { exe: format!("{folder}\\kwil.exe"), folder })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status {
    pub app_paths: AppPaths,
    pub on_path: bool,
}
impl Status {
    pub fn registered(&self) -> bool {
        self.app_paths == AppPaths::Ours && self.on_path
    }
    /// Any part of this install is registered (a partly applied state counts).
    pub fn any(&self) -> bool {
        self.app_paths == AppPaths::Ours || self.on_path
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub app_paths_changed: bool,
    pub path_changed: bool,
}
impl Outcome {
    pub fn changed(&self) -> bool {
        self.app_paths_changed || self.path_changed
    }
}

fn read_path(registry: &mut impl Registry) -> Result<Option<Value>, CommandLineError> {
    registry.read(ENVIRONMENT_SUBKEY, PATH_VALUE)
}

pub fn status(registry: &mut impl Registry, install: &Install) -> Result<Status, CommandLineError> {
    let app_paths = classify_app_paths(registry.read(APP_PATHS_SUBKEY, "")?.as_ref(), &install.exe);
    let on_path = read_path(registry)?.is_some_and(|value| path_contains(&value.text, &install.folder));
    Ok(Status { app_paths, on_path })
}

/// Idempotent. Another install's App Paths entry is replaced: Win+R should open
/// the install being registered now.
pub fn register(registry: &mut impl Registry, install: &Install) -> Result<Outcome, CommandLineError> {
    let mut outcome = Outcome::default();
    // Read first: an unexpected PATH type stops everything before any write.
    let path = read_path(registry)?;
    if registry.read(APP_PATHS_SUBKEY, "")?.as_ref() != Some(&Value::sz(&install.exe)) {
        registry.write(APP_PATHS_SUBKEY, "", &Value::sz(&install.exe))?;
        outcome.app_paths_changed = true;
    }
    if registry.read(APP_PATHS_SUBKEY, PATH_VALUE)?.as_ref() != Some(&Value::sz(&install.folder)) {
        registry.write(APP_PATHS_SUBKEY, PATH_VALUE, &Value::sz(&install.folder))?;
        outcome.app_paths_changed = true;
    }
    let (kind, current) = path.map_or((ValueKind::ExpandSz, String::new()), |value| (value.kind, value.text));
    if let Some(text) = path_with(&current, &install.folder) {
        registry.write(ENVIRONMENT_SUBKEY, PATH_VALUE, &Value { kind, text })?;
        registry.environment_changed();
        outcome.path_changed = true;
    }
    Ok(outcome)
}

/// Removes what [`register`] added for this install, nothing of another one.
/// Safe to run twice and when nothing is registered.
pub fn unregister(registry: &mut impl Registry, install: &Install) -> Result<Outcome, CommandLineError> {
    let mut outcome = Outcome::default();
    if classify_app_paths(registry.read(APP_PATHS_SUBKEY, "")?.as_ref(), &install.exe) == AppPaths::Ours {
        registry.delete_value(APP_PATHS_SUBKEY, "")?;
        registry.delete_value(APP_PATHS_SUBKEY, PATH_VALUE)?;
        // A key with foreign subkeys stays; it no longer names Kwil.
        let _ = registry.delete_key(APP_PATHS_SUBKEY);
        outcome.app_paths_changed = true;
    }
    if let Some(value) = read_path(registry)?
        && let Some(text) = path_without(&value.text, &install.folder)
    {
        registry.write(ENVIRONMENT_SUBKEY, PATH_VALUE, &Value { kind: value.kind, text })?;
        registry.environment_changed();
        outcome.path_changed = true;
    }
    Ok(outcome)
}

pub fn registered_message(outcome: Outcome) -> &'static str {
    if outcome.changed() {
        "Kwil is on your PATH. Open a new terminal to use `kwil`, or press Win+R and type `kwil`."
    } else {
        "Kwil is already on your PATH."
    }
}

pub fn removed_message(outcome: Outcome) -> &'static str {
    if outcome.changed() { "Kwil is no longer on your PATH." } else { "Kwil was not on your PATH." }
}

pub fn status_message(status: Status) -> &'static str {
    match (status.registered(), status.any()) {
        (true, _) => "Kwil is on your PATH and opens from Win+R.",
        (false, true) => "Kwil is only partly set up for the command line. Turn it on again to finish.",
        (false, false) => "Kwil is not on your PATH.",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    Status,
    Register,
    Unregister,
}

/// What a request left behind: the real state and one short sentence for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub status: Status,
    pub message: String,
}

pub fn apply(registry: &mut impl Registry, install: &Install, request: Request) -> Result<Report, CommandLineError> {
    let message = match request {
        Request::Register => Some(registered_message(register(registry, install)?)),
        Request::Unregister => Some(removed_message(unregister(registry, install)?)),
        Request::Status => None,
    };
    let status = status(registry, install)?;
    Ok(Report { status, message: message.unwrap_or_else(|| status_message(status)).to_owned() })
}

pub const ISOLATION_ENV: &str = "KWIL_WORKSPACE_ROOT";

/// The seam that keeps native access out of tests and hidden runs. Hosts pass
/// hidden mode and their own `cfg!(test)`; isolation comes from the environment.
pub fn gate(hidden: bool, in_test: bool) -> Result<(), &'static str> {
    gate_with(hidden, std::env::var_os(ISOLATION_ENV).is_some(), in_test)
}

pub fn gate_with(hidden: bool, isolated: bool, in_test: bool) -> Result<(), &'static str> {
    if hidden {
        Err("Command-line registration is disabled in hidden test mode")
    } else if isolated {
        Err("Command-line registration is disabled in an isolated workspace")
    } else if in_test {
        Err("Command-line registration is disabled in unit tests")
    } else {
        Ok(())
    }
}

/// Run one request on the user's real registry; refused first unless `allowed`.
/// Blocking (the broadcast can wait for a hung window): call off the UI thread.
pub fn run(request: Request, allowed: Result<(), &'static str>) -> Result<Report, String> {
    allowed.map_err(str::to_owned)?;
    #[cfg(windows)]
    {
        let exe = std::env::current_exe().map_err(|e| format!("Cannot find Kwil's own location: {e}"))?;
        let install = if request == Request::Register { locate(&exe) } else { locate_existing(&exe) }
            .map_err(|e| e.to_string())?;
        apply(&mut native::WindowsRegistry::user(), &install, request).map_err(|e| e.to_string())
    }
    #[cfg(not(windows))]
    {
        let _ = request;
        Err("Putting Kwil on the PATH is only needed on Windows; scripts/install.sh already links it.".into())
    }
}

/// [`run`] on its own thread; `done` gets the result. A refusal or a thread that
/// cannot start is returned at once, with no thread and no registry access.
pub fn spawn(
    request: Request,
    allowed: Result<(), &'static str>,
    done: impl FnOnce(Result<Report, String>) + Send + 'static,
) -> Result<(), String> {
    allowed.map_err(str::to_owned)?;
    let thread = std::thread::Builder::new().name("kwil-command-line".into());
    thread
        .spawn(move || done(run(request, Ok(()))))
        .map(drop)
        .map_err(|e| format!("Could not start the command-line setup: {e}"))
}

#[cfg(windows)]
pub mod native {
    //! Win32 registry layer. No policy here: the pure functions above decide.
    use super::{CommandLineError, Registry, Value, ValueKind};
    use windows_sys::Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS},
        System::Registry::*,
        UI::WindowsAndMessaging::{HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE},
    };

    /// 128 KiB is four times the largest legal environment value.
    const MAX_VALUE_BYTES: u32 = 128 * 1024;
    const BROADCAST_TIMEOUT_MS: u32 = 1000;
    /// Every isolated root must live here, so a typo cannot reach real keys.
    pub const ISOLATED_PREFIX: &str = r"Software\Kwil-test-";

    /// A registry root under `HKEY_CURRENT_USER`. `user()` is the real thing;
    /// `isolated()` prefixes every key with a throwaway path.
    pub struct WindowsRegistry {
        base: String,
        broadcast: bool,
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }
    fn fail(what: &str, status: u32) -> CommandLineError {
        CommandLineError(format!("{what}: {}", std::io::Error::from_raw_os_error(status as i32)))
    }

    struct Key(HKEY);
    impl Drop for Key {
        fn drop(&mut self) {
            unsafe {
                RegCloseKey(self.0);
            }
        }
    }

    impl WindowsRegistry {
        /// The signed-in user's real `HKEY_CURRENT_USER`. Only host code that
        /// passed its hidden-mode and isolation checks may call this.
        pub fn user() -> Self {
            Self { base: String::new(), broadcast: true }
        }

        /// A throwaway root, `Software\Kwil-test-<suffix>`. Never broadcasts.
        pub fn isolated(base: &str) -> Result<Self, CommandLineError> {
            let suffix = base.strip_prefix(ISOLATED_PREFIX).unwrap_or("");
            if suffix.is_empty() || suffix.contains('\\') {
                return super::err("an isolated registry root must be Software\\Kwil-test-<name>");
            }
            Ok(Self { base: base.to_owned(), broadcast: false })
        }

        #[cfg(test)]
        pub(super) fn root_for_tests(&self) -> String {
            self.base.clone()
        }

        fn path(&self, subkey: &str) -> Vec<u16> {
            if self.base.is_empty() { wide(subkey) } else { wide(&format!("{}\\{subkey}", self.base)) }
        }

        fn open(&self, subkey: &str, access: REG_SAM_FLAGS) -> Result<Option<Key>, CommandLineError> {
            let mut handle = std::ptr::null_mut();
            let status =
                unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, self.path(subkey).as_ptr(), 0, access, &mut handle) };
            match status {
                ERROR_SUCCESS => Ok(Some(Key(handle))),
                ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => Ok(None),
                other => Err(fail("Cannot open the Windows registry", other)),
            }
        }

        fn create(&self, subkey: &str) -> Result<Key, CommandLineError> {
            let mut handle = std::ptr::null_mut();
            let status = unsafe {
                RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    self.path(subkey).as_ptr(),
                    0,
                    std::ptr::null(),
                    0,
                    KEY_QUERY_VALUE | KEY_SET_VALUE,
                    std::ptr::null(),
                    &mut handle,
                    std::ptr::null_mut(),
                )
            };
            if status == ERROR_SUCCESS {
                Ok(Key(handle))
            } else {
                Err(fail("Cannot write the Windows registry", status))
            }
        }

        /// Test cleanup: delete the whole throwaway tree. Refuses the real root.
        pub fn remove_isolated_tree(&mut self) -> Result<(), CommandLineError> {
            if self.base.is_empty() || self.broadcast {
                return super::err("only an isolated registry root can be deleted as a tree");
            }
            let status = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, wide(&self.base).as_ptr()) };
            match status {
                ERROR_SUCCESS | ERROR_FILE_NOT_FOUND => Ok(()),
                other => Err(fail("Cannot remove the isolated registry tree", other)),
            }
        }
    }

    impl Registry for WindowsRegistry {
        fn read(&mut self, subkey: &str, name: &str) -> Result<Option<Value>, CommandLineError> {
            let Some(key) = self.open(subkey, KEY_QUERY_VALUE)? else { return Ok(None) };
            let name = wide(name);
            let (mut kind, mut length) = (0, 0u32);
            let status = unsafe {
                RegQueryValueExW(key.0, name.as_ptr(), std::ptr::null(), &mut kind, std::ptr::null_mut(), &mut length)
            };
            if status == ERROR_FILE_NOT_FOUND {
                return Ok(None);
            }
            if status != ERROR_SUCCESS {
                return Err(fail("Cannot read the Windows registry", status));
            }
            if length > MAX_VALUE_BYTES {
                return super::err("A registry value Kwil needs is unexpectedly large; it was left unchanged.");
            }
            let mut bytes = vec![0u8; length as usize];
            let status = unsafe {
                RegQueryValueExW(key.0, name.as_ptr(), std::ptr::null(), &mut kind, bytes.as_mut_ptr(), &mut length)
            };
            if status != ERROR_SUCCESS || length as usize > bytes.len() {
                return Err(fail("Cannot read the Windows registry", status));
            }
            bytes.truncate(length as usize);
            let kind = match kind {
                REG_SZ => ValueKind::Sz,
                REG_EXPAND_SZ => ValueKind::ExpandSz,
                _ => return super::err("A registry value Kwil needs has an unexpected type; it was left unchanged."),
            };
            if !bytes.len().is_multiple_of(2) {
                return super::err("A registry value Kwil needs is damaged; it was left unchanged.");
            }
            let mut units: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|b| u16::from_le_bytes(*b)).collect();
            while units.last() == Some(&0) {
                units.pop();
            }
            match String::from_utf16(&units) {
                Ok(text) if !text.contains('\0') => Ok(Some(Value { kind, text })),
                _ => super::err("A registry value Kwil needs holds unreadable text; it was left unchanged."),
            }
        }

        fn write(&mut self, subkey: &str, name: &str, value: &Value) -> Result<(), CommandLineError> {
            let key = self.create(subkey)?;
            let data: Vec<u8> = wide(&value.text).iter().flat_map(|u| u.to_le_bytes()).collect();
            if data.len() as u32 > MAX_VALUE_BYTES {
                return super::err("The new value is too long for the Windows registry.");
            }
            let kind = match value.kind {
                ValueKind::Sz => REG_SZ,
                ValueKind::ExpandSz => REG_EXPAND_SZ,
            };
            let status =
                unsafe { RegSetValueExW(key.0, wide(name).as_ptr(), 0, kind, data.as_ptr(), data.len() as u32) };
            if status == ERROR_SUCCESS { Ok(()) } else { Err(fail("Cannot write the Windows registry", status)) }
        }

        fn delete_value(&mut self, subkey: &str, name: &str) -> Result<(), CommandLineError> {
            let Some(key) = self.open(subkey, KEY_SET_VALUE)? else { return Ok(()) };
            let status = unsafe { RegDeleteValueW(key.0, wide(name).as_ptr()) };
            if status == ERROR_SUCCESS || status == ERROR_FILE_NOT_FOUND {
                Ok(())
            } else {
                Err(fail("Cannot write the Windows registry", status))
            }
        }

        fn delete_key(&mut self, subkey: &str) -> Result<(), CommandLineError> {
            let status = unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, self.path(subkey).as_ptr()) };
            if matches!(status, ERROR_SUCCESS | ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) {
                Ok(())
            } else {
                Err(fail("Cannot write the Windows registry", status))
            }
        }

        fn environment_changed(&mut self) {
            if !self.broadcast {
                return;
            }
            let mut result = 0usize;
            // Bounded, and hung windows are skipped. The answer does not matter:
            // new terminals read the registry themselves.
            unsafe {
                SendMessageTimeoutW(
                    HWND_BROADCAST,
                    WM_SETTINGCHANGE,
                    0,
                    wide("Environment").as_ptr() as isize,
                    SMTO_ABORTIFHUNG,
                    BROADCAST_TIMEOUT_MS,
                    &mut result,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const FOLDER: &str = r"C:\Users\ada\AppData\Local\Programs\Kwil";
    const EXE: &str = r"C:\Users\ada\AppData\Local\Programs\Kwil\kwil.exe";

    fn install() -> Install {
        Install { exe: EXE.into(), folder: FOLDER.into() }
    }

    #[test]
    fn appends_to_empty_existing_and_semicolon_terminated_paths() {
        assert_eq!(path_with("", FOLDER).as_deref(), Some(FOLDER));
        assert_eq!(path_with(r"C:\Windows;C:\Tools", FOLDER), Some(format!(r"C:\Windows;C:\Tools;{FOLDER}")));
        assert_eq!(path_with(r"C:\Windows;", FOLDER), Some(format!(r"C:\Windows;{FOLDER};")));
    }

    #[test]
    fn an_entry_already_present_is_a_no_op_in_any_case_or_with_a_trailing_backslash() {
        for existing in [
            format!(r"C:\Windows;{FOLDER}"),
            format!(r"C:\Windows;{}", FOLDER.to_uppercase()),
            format!(r"C:\Windows;{FOLDER}\;C:\x"),
            format!(r#"C:\Windows;"{FOLDER}""#),
            format!(r"{FOLDER}/"),
        ] {
            assert!(path_contains(&existing, FOLDER), "{existing}");
            assert_eq!(path_with(&existing, FOLDER), None, "{existing}");
        }
        assert!(path_with(&format!(r"C:\Windows;{FOLDER}2"), FOLDER).is_some(), "a longer name is another folder");
        assert!(!path_contains("", FOLDER));
        assert!(!path_contains(";;", ""), "an empty folder never matches empty entries");
    }

    #[test]
    fn removal_takes_only_our_entry_and_keeps_the_others_in_order() {
        let cases = [
            (format!(r"C:\a;{FOLDER};C:\b"), r"C:\a;C:\b"),
            (format!(r"{FOLDER};C:\b"), r"C:\b"),
            (format!(r"C:\a;{}", FOLDER.to_lowercase()), r"C:\a"),
            (format!(r"C:\a;%SystemRoot%\x;{FOLDER}\;C:\b;"), r"C:\a;%SystemRoot%\x;C:\b;"),
        ];
        for (before, after) in cases {
            assert_eq!(path_without(&before, FOLDER).as_deref(), Some(after), "{before}");
        }
        assert_eq!(path_without(r"C:\a;C:\b", FOLDER), None);
        assert_eq!(path_without(&format!("{FOLDER}2"), FOLDER), None);
    }

    #[test]
    fn append_then_remove_restores_the_original_text_exactly() {
        for original in ["", r"C:\a", r"C:\a;", r"C:\a;%SystemRoot%\x;;C:\b", r"C:\a;;"] {
            let added = path_with(original, FOLDER).unwrap();
            assert_eq!(path_without(&added, FOLDER).as_deref(), Some(original), "{original:?} via {added:?}");
        }
    }

    #[test]
    fn app_paths_ownership_is_by_the_exact_executable() {
        assert_eq!(classify_app_paths(None, EXE), AppPaths::Missing);
        assert_eq!(classify_app_paths(Some(&Value::sz(EXE)), EXE), AppPaths::Ours);
        assert_eq!(classify_app_paths(Some(&Value::sz(EXE.to_uppercase())), EXE), AppPaths::Ours);
        assert_eq!(classify_app_paths(Some(&Value::sz(format!("\"{EXE}\""))), EXE), AppPaths::Ours);
        assert_eq!(classify_app_paths(Some(&Value::sz(r"D:\Portable\kwil.exe")), EXE), AppPaths::Foreign);
        let expand = Value { kind: ValueKind::ExpandSz, text: EXE.into() };
        assert_eq!(classify_app_paths(Some(&expand), EXE), AppPaths::Foreign);
    }

    #[derive(Default)]
    struct Fake {
        values: BTreeMap<(String, String), Value>,
        keys: Vec<String>,
        broadcasts: usize,
        refuse_path_write: bool,
    }
    impl Fake {
        fn with_path(kind: ValueKind, text: &str) -> Self {
            let mut fake = Self::default();
            fake.values.insert((ENVIRONMENT_SUBKEY.into(), PATH_VALUE.into()), Value { kind, text: text.into() });
            fake
        }
        fn path(&self) -> Option<&Value> {
            self.values.get(&(ENVIRONMENT_SUBKEY.to_owned(), PATH_VALUE.to_owned()))
        }
        fn app_paths(&self) -> Option<&Value> {
            self.values.get(&(APP_PATHS_SUBKEY.to_owned(), String::new()))
        }
    }
    impl Registry for Fake {
        fn read(&mut self, subkey: &str, name: &str) -> Result<Option<Value>, CommandLineError> {
            Ok(self.values.get(&(subkey.into(), name.into())).cloned())
        }
        fn write(&mut self, subkey: &str, name: &str, value: &Value) -> Result<(), CommandLineError> {
            if self.refuse_path_write && subkey == ENVIRONMENT_SUBKEY {
                return err("denied");
            }
            if !self.keys.iter().any(|k| k == subkey) {
                self.keys.push(subkey.into());
            }
            self.values.insert((subkey.into(), name.into()), value.clone());
            Ok(())
        }
        fn delete_value(&mut self, subkey: &str, name: &str) -> Result<(), CommandLineError> {
            self.values.remove(&(subkey.into(), name.into()));
            Ok(())
        }
        fn delete_key(&mut self, subkey: &str) -> Result<(), CommandLineError> {
            self.keys.retain(|k| k != subkey);
            Ok(())
        }
        fn environment_changed(&mut self) {
            self.broadcasts += 1;
        }
    }

    #[test]
    fn register_keeps_the_path_type_and_every_other_entry() {
        for kind in [ValueKind::ExpandSz, ValueKind::Sz] {
            let original = r"%USERPROFILE%\bin;C:\Tools;";
            let mut fake = Fake::with_path(kind, original);
            let outcome = register(&mut fake, &install()).unwrap();
            assert!(outcome.path_changed && outcome.app_paths_changed);
            let path = fake.path().unwrap();
            assert_eq!(path.kind, kind, "the existing type is kept");
            assert_eq!(path.text, format!(r"{original}{FOLDER};"));
            assert_eq!(fake.app_paths(), Some(&Value::sz(EXE)));
            assert_eq!(fake.values[&(APP_PATHS_SUBKEY.to_owned(), "Path".to_owned())], Value::sz(FOLDER));
            assert_eq!(fake.broadcasts, 1);
        }
    }

    #[test]
    fn register_creates_a_missing_path_as_expandable_and_is_idempotent() {
        let mut fake = Fake::default();
        register(&mut fake, &install()).unwrap();
        assert_eq!(fake.path(), Some(&Value { kind: ValueKind::ExpandSz, text: FOLDER.into() }));
        let again = register(&mut fake, &install()).unwrap();
        assert!(!again.changed(), "a second register writes nothing");
        assert_eq!(fake.broadcasts, 1, "and does not broadcast again");
        assert_eq!(registered_message(again), "Kwil is already on your PATH.");
        assert!(status(&mut fake, &install()).unwrap().registered());
    }

    #[test]
    fn unregister_restores_the_original_and_is_safe_twice_or_when_nothing_was_registered() {
        let original = r"C:\a;%SystemRoot%\x;C:\b";
        let mut fake = Fake::with_path(ValueKind::ExpandSz, original);
        register(&mut fake, &install()).unwrap();
        let first = unregister(&mut fake, &install()).unwrap();
        assert!(first.app_paths_changed && first.path_changed);
        assert_eq!(fake.path(), Some(&Value { kind: ValueKind::ExpandSz, text: original.into() }));
        assert!(fake.app_paths().is_none() && fake.keys.iter().all(|k| k != APP_PATHS_SUBKEY));
        let second = unregister(&mut fake, &install()).unwrap();
        assert!(!second.changed());
        assert_eq!(fake.path().unwrap().text, original);
        assert_eq!(removed_message(second), "Kwil was not on your PATH.");
        let mut empty = Fake::default();
        assert!(!unregister(&mut empty, &install()).unwrap().changed());
        assert!(empty.path().is_none(), "nothing is created while removing");
    }

    #[test]
    fn app_paths_that_belong_to_another_install_are_left_alone_by_unregister() {
        let mut fake = Fake::with_path(ValueKind::ExpandSz, &format!(r"C:\a;{FOLDER}"));
        fake.write(APP_PATHS_SUBKEY, "", &Value::sz(r"D:\Portable\Kwil\kwil.exe")).unwrap();
        fake.write(APP_PATHS_SUBKEY, "Path", &Value::sz(r"D:\Portable\Kwil")).unwrap();
        let outcome = unregister(&mut fake, &install()).unwrap();
        assert!(!outcome.app_paths_changed && outcome.path_changed);
        assert_eq!(fake.app_paths(), Some(&Value::sz(r"D:\Portable\Kwil\kwil.exe")));
        assert_eq!(fake.values[&(APP_PATHS_SUBKEY.to_owned(), "Path".to_owned())], Value::sz(r"D:\Portable\Kwil"));
        assert!(fake.keys.iter().any(|k| k == APP_PATHS_SUBKEY));
        assert_eq!(fake.path().unwrap().text, r"C:\a");
        let status = status(&mut fake, &install()).unwrap();
        assert_eq!(status.app_paths, AppPaths::Foreign);
        assert!(!status.registered() && !status.any());
    }

    #[test]
    fn registering_this_install_takes_over_a_stale_app_paths_entry() {
        let mut fake = Fake::default();
        fake.write(APP_PATHS_SUBKEY, "", &Value::sz(r"D:\Old\kwil.exe")).unwrap();
        let outcome = register(&mut fake, &install()).unwrap();
        assert!(outcome.app_paths_changed);
        assert_eq!(fake.app_paths(), Some(&Value::sz(EXE)));
    }

    #[test]
    fn a_failed_path_write_reports_the_error_and_leaves_status_partial() {
        let mut fake = Fake::with_path(ValueKind::ExpandSz, r"C:\a");
        fake.refuse_path_write = true;
        assert!(register(&mut fake, &install()).is_err());
        let partial = status(&mut fake, &install()).unwrap();
        assert!(!partial.registered() && partial.any());
        assert_eq!(
            status_message(partial),
            "Kwil is only partly set up for the command line. Turn it on again to finish."
        );
        assert_eq!(fake.broadcasts, 0, "nothing is broadcast for a write that failed");
    }

    #[test]
    fn folder_rules_refuse_network_temp_root_and_unsafe_names() {
        let temp = vec![PathBuf::from(r"C:\Users\ada\AppData\Local\Temp")];
        assert_eq!(validate_folder(Path::new(FOLDER), &temp).unwrap(), FOLDER);
        assert_eq!(validate_folder(Path::new(&format!("{FOLDER}\\")), &temp).unwrap(), FOLDER);
        assert_eq!(validate_folder(Path::new(&format!(r"\\?\{FOLDER}")), &temp).unwrap(), FOLDER);
        for (path, why) in [
            (r"\\server\share\Kwil", "network"),
            (r"C:\Users\ada\AppData\Local\Temp\kwil-test", "temporary"),
            (r"c:\users\ADA\appdata\local\temp", "temporary"),
            (r"C:\Users\ada\AppData\Local\Temp", "temporary"),
            (r"C:\", "root"),
            (r"C:\Apps;Old\Kwil", "semicolon"),
            (r"C:\Apps\%USERPROFILE%\Kwil", ""),
        ] {
            let error = validate_folder(Path::new(path), &temp).unwrap_err().0;
            assert!(error.to_lowercase().contains(why), "{path}: {error}");
        }
        assert!(
            validate_folder(Path::new(r"C:\Users\ada\AppData\Local\Temporary Files\Kwil"), &temp).is_ok(),
            "only a real parent folder counts, not a name that starts the same"
        );
    }

    #[test]
    fn lenient_lookup_recognises_an_install_that_would_no_longer_be_accepted() {
        let found = locate_existing(Path::new(r"C:\Users\ada\AppData\Local\Temp\Kwil\kwil.com")).unwrap();
        assert_eq!(found.exe, r"C:\Users\ada\AppData\Local\Temp\Kwil\kwil.exe");
    }

    #[test]
    fn native_access_is_refused_in_hidden_isolated_and_unit_test_runs() {
        for (hidden, isolated, in_test, why) in [
            (true, false, false, "hidden test mode"),
            (false, true, false, "isolated workspace"),
            (false, false, true, "unit tests"),
            (true, true, true, "hidden test mode"),
        ] {
            let refused = gate_with(hidden, isolated, in_test).unwrap_err();
            assert!(refused.contains(why), "{refused}");
            assert_eq!(
                run(Request::Register, Err(refused)).unwrap_err(),
                refused,
                "refusal precedes any registry access"
            );
        }
        assert_eq!(gate_with(false, false, false), Ok(()));
    }

    #[test]
    fn apply_reports_the_real_state_with_a_short_sentence() {
        let mut fake = Fake::default();
        let report = apply(&mut fake, &install(), Request::Status).unwrap();
        assert_eq!(report.message, "Kwil is not on your PATH.");
        let report = apply(&mut fake, &install(), Request::Register).unwrap();
        assert!(report.status.registered() && report.message.starts_with("Kwil is on your PATH."));
        let report = apply(&mut fake, &install(), Request::Status).unwrap();
        assert_eq!(report.message, "Kwil is on your PATH and opens from Win+R.");
        let report = apply(&mut fake, &install(), Request::Unregister).unwrap();
        assert!(!report.status.any() && report.message == "Kwil is no longer on your PATH.");
    }

    #[test]
    fn messages_are_plain_and_never_a_bare_code() {
        let done = Outcome { app_paths_changed: true, path_changed: true };
        assert!(registered_message(done).starts_with("Kwil is on your PATH. Open a new terminal"));
        assert_eq!(removed_message(done), "Kwil is no longer on your PATH.");
        assert_eq!(
            status_message(Status { app_paths: AppPaths::Ours, on_path: true }),
            "Kwil is on your PATH and opens from Win+R."
        );
    }
}

#[cfg(all(test, windows))]
mod native_tests {
    //! The Win32 layer against a throwaway key under `HKCU\Software\Kwil-test-*`.
    //! The real `Environment` and App Paths keys are never opened: `isolated()`
    //! prefixes every subkey, and refuses any root outside that prefix.
    use super::native::{ISOLATED_PREFIX, WindowsRegistry};
    use super::*;

    struct Throwaway(WindowsRegistry);
    impl Throwaway {
        fn new() -> Self {
            let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            let root = format!("{ISOLATED_PREFIX}{}-{nonce}", std::process::id());
            Self(WindowsRegistry::isolated(&root).unwrap())
        }
    }
    impl Drop for Throwaway {
        fn drop(&mut self) {
            self.0.remove_isolated_tree().expect("the throwaway key must be deleted");
        }
    }

    const FOLDER: &str = r"C:\Users\ada\AppData\Local\Programs\Kwil";
    fn install() -> Install {
        Install { exe: format!(r"{FOLDER}\kwil.exe"), folder: FOLDER.into() }
    }

    #[test]
    fn the_isolated_root_refuses_anything_that_could_be_a_real_key() {
        for root in [
            "",
            "Environment",
            r"Software\Microsoft",
            r"Software\Kwil-test-",
            r"Software\Kwil-test-a\b",
            ISOLATED_PREFIX,
        ] {
            assert!(WindowsRegistry::isolated(root).is_err(), "{root:?}");
        }
        assert!(
            WindowsRegistry::user().remove_isolated_tree().is_err(),
            "the real root can never be deleted as a tree"
        );
    }

    #[test]
    fn register_and_unregister_round_trip_in_a_throwaway_key() {
        let mut registry = Throwaway::new();
        let reg = &mut registry.0;
        let original = Value { kind: ValueKind::ExpandSz, text: r"%SystemRoot%\system32;C:\Tools;".into() };
        reg.write(ENVIRONMENT_SUBKEY, "Path", &original).unwrap();
        assert_eq!(status(reg, &install()).unwrap(), Status { app_paths: AppPaths::Missing, on_path: false });

        let outcome = register(reg, &install()).unwrap();
        assert!(outcome.path_changed && outcome.app_paths_changed);
        let path = reg.read(ENVIRONMENT_SUBKEY, "Path").unwrap().unwrap();
        assert_eq!(path.kind, ValueKind::ExpandSz, "REG_EXPAND_SZ survives the round trip through Win32");
        assert_eq!(path.text, format!("{}{FOLDER};", original.text));
        assert_eq!(reg.read(APP_PATHS_SUBKEY, "").unwrap(), Some(Value::sz(install().exe)));
        assert_eq!(reg.read(APP_PATHS_SUBKEY, "Path").unwrap(), Some(Value::sz(FOLDER)));
        assert!(status(reg, &install()).unwrap().registered());
        assert!(!register(reg, &install()).unwrap().changed());

        assert!(unregister(reg, &install()).unwrap().changed());
        assert_eq!(reg.read(ENVIRONMENT_SUBKEY, "Path").unwrap(), Some(original));
        assert_eq!(reg.read(APP_PATHS_SUBKEY, "").unwrap(), None);
        assert!(!unregister(reg, &install()).unwrap().changed());
    }

    #[test]
    fn a_plain_sz_path_stays_sz_and_a_missing_path_is_created_expandable() {
        let mut registry = Throwaway::new();
        let reg = &mut registry.0;
        reg.write(ENVIRONMENT_SUBKEY, "Path", &Value::sz(r"C:\a")).unwrap();
        register(reg, &install()).unwrap();
        assert_eq!(reg.read(ENVIRONMENT_SUBKEY, "Path").unwrap().unwrap().kind, ValueKind::Sz);
        unregister(reg, &install()).unwrap();

        let mut registry = Throwaway::new();
        register(&mut registry.0, &install()).unwrap();
        let created = registry.0.read(ENVIRONMENT_SUBKEY, "Path").unwrap().unwrap();
        assert_eq!(created, Value { kind: ValueKind::ExpandSz, text: FOLDER.into() });
    }

    #[test]
    fn another_installs_app_paths_survive_unregister_in_a_throwaway_key() {
        let mut registry = Throwaway::new();
        let reg = &mut registry.0;
        reg.write(APP_PATHS_SUBKEY, "", &Value::sz(r"D:\Portable\kwil.exe")).unwrap();
        reg.write(ENVIRONMENT_SUBKEY, "Path", &Value::sz(format!(r"C:\a;{FOLDER}"))).unwrap();
        let outcome = unregister(reg, &install()).unwrap();
        assert!(!outcome.app_paths_changed && outcome.path_changed);
        assert_eq!(reg.read(APP_PATHS_SUBKEY, "").unwrap(), Some(Value::sz(r"D:\Portable\kwil.exe")));
        assert_eq!(reg.read(ENVIRONMENT_SUBKEY, "Path").unwrap().unwrap().text, r"C:\a");
    }

    #[test]
    fn a_path_of_another_type_is_refused_and_left_unchanged() {
        use windows_sys::Win32::System::Registry::*;
        let mut registry = Throwaway::new();
        // A DWORD named Path, written with raw Win32 inside the throwaway key only.
        let base: Vec<u16> =
            format!(r"{}\{ENVIRONMENT_SUBKEY}", registry_root(&registry)).encode_utf16().chain(Some(0)).collect();
        let mut handle = std::ptr::null_mut();
        let name: Vec<u16> = "Path".encode_utf16().chain(Some(0)).collect();
        unsafe {
            assert_eq!(
                RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    base.as_ptr(),
                    0,
                    std::ptr::null(),
                    0,
                    KEY_SET_VALUE,
                    std::ptr::null(),
                    &mut handle,
                    std::ptr::null_mut()
                ),
                0
            );
            assert_eq!(RegSetValueExW(handle, name.as_ptr(), 0, REG_DWORD, 7u32.to_le_bytes().as_ptr(), 4), 0);
            RegCloseKey(handle);
        }
        let error = register(&mut registry.0, &install()).unwrap_err().0;
        assert!(error.contains("unexpected type"), "{error}");
        assert_eq!(registry.0.read(APP_PATHS_SUBKEY, "").unwrap(), None, "nothing was written before the refusal");
    }

    /// The root is private to `WindowsRegistry`; rebuild it from the same name pattern.
    fn registry_root(registry: &Throwaway) -> String {
        registry.0.root_for_tests()
    }
}
