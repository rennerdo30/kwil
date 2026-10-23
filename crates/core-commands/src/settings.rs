//! One settings file. Watching is deliberately an event callback hook, ready for
//! core-workspace's FileWatcher: register the settings FILE path (it watches
//! parent directories internally), then call SettingsWatch::reload on events.
//! The original hook starts no thread. Opt-in `SettingsWorker` owns native
//! watching and atomic writes after the first frame; it has no polling loop.
use crate::{Binding, Keymap, Preset};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fmt, fs,
    io::Read,
    path::{Path, PathBuf},
};

macro_rules! group {
    ($name:ident { $($field:ident : $ty:ty = $default:expr),* $(,)? }) => {
        #[derive(Clone, Debug, PartialEq, Deserialize)]
        #[serde(default)]
        pub struct $name { $(pub $field: $ty),* }
        impl Default for $name { fn default() -> Self { Self { $($field: $default),* } } }
    };
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Remote {
    #[default]
    Auto,
    On,
    Off,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UpdateChannel {
    #[default]
    Stable,
    Prerelease,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TabLayout {
    #[default]
    Horizontal,
    Vertical,
}

group!(RenderSettings { remote: Remote = Remote::Auto, smooth_scrolling: bool = false, cursor_blink: bool = true });
group!(TabsSettings { layout: TabLayout = TabLayout::Horizontal });
group!(UpdateSettings { check: bool = true, channel: UpdateChannel = UpdateChannel::Stable });
group!(FontSettings {
    family: String = "monospace".into(),
    size: f32 = 14.0,
    line_height: f32 = 1.4,
    ligatures: bool = true
});
group!(ThemeSettings { name: String = "dark".into(), follow_os: bool = false, colors: BTreeMap<String, String> = BTreeMap::new() });
group!(EditorSettings {
    word_wrap: bool = false,
    tab_size: u8 = 4,
    insert_spaces: bool = true,
    auto_indent: bool = true,
    show_whitespace: bool = false,
    trim_trailing_whitespace_on_save: bool = false
});
group!(FilesSettings {
    small_file_threshold_bytes: u64 = 64 * 1024 * 1024,
    backup: bool = false,
    restore_session: bool = true,
    journal_interval_seconds: u32 = 3
});
group!(UndoSettings { memory_limit_bytes: u64 = 64 * 1024 * 1024, persist: bool = false });
group!(SearchSettings { regex: bool = false, case_sensitive: bool = false, whole_word: bool = false,
    in_selection: bool = false, include: Vec<String> = Vec::new(), exclude: Vec<String> = Vec::new() });
group!(CompareSettings { ignore_whitespace: bool = false, collapse_unchanged: bool = true });
group!(SyntaxSettings { full_parse_limit_bytes: u64 = 20 * 1024 * 1024 });
group!(LogSettings { follow: bool = false, detect_levels: bool = true,
    highlight_rules: BTreeMap<String, String> = BTreeMap::new() });
group!(Formatter { executable: String = String::new(), args: Vec<String> = Vec::new(), timeout_ms: u32 = 5000 });
group!(Settings {
    keymap: Preset = Preset::VsCode, bindings: Vec<Binding> = Vec::new(),
    tabs: TabsSettings = TabsSettings::default(),
    render: RenderSettings = RenderSettings::default(), update: UpdateSettings = UpdateSettings::default(),
    font: FontSettings = FontSettings::default(), theme: ThemeSettings = ThemeSettings::default(),
    editor: EditorSettings = EditorSettings::default(), files: FilesSettings = FilesSettings::default(),
    undo: UndoSettings = UndoSettings::default(), search: SearchSettings = SearchSettings::default(),
    compare: CompareSettings = CompareSettings::default(), syntax: SyntaxSettings = SyntaxSettings::default(),
    log: LogSettings = LogSettings::default(), formatters: BTreeMap<String, Formatter> = BTreeMap::new()
});

#[derive(Clone, Debug, PartialEq)]
pub struct SettingsReport {
    pub settings: Settings,
    pub unknown_keys: Vec<String>,
    /// Source positions for the corresponding nonfatal unknown-key warnings.
    pub unknown_key_diagnostics: Vec<SettingsDiagnostic>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsDiagnostic {
    pub path: String,
    /// One-based line and character column of the key.
    pub line: usize,
    pub column: usize,
}
#[derive(Debug)]
pub struct SettingsError(pub String);
impl fmt::Display for SettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for SettingsError {}

impl Settings {
    pub fn parse(source: &str) -> Result<SettingsReport, SettingsError> {
        let (table, errors) = toml::de::DeTable::parse_recoverable(source);
        let value = toml::de::DeValue::Table(table.into_inner());
        let mut locations = Vec::new();
        collect_locations(&value, "", &mut locations);
        let locate = |path: &str, message: &str| {
            let offset = locations.iter().find(|l| l.path == path).map_or(0, |l| l.key);
            let diagnostic = diagnostic(source, path, offset);
            SettingsError(format!("{path} at line {}, column {}: {message}", diagnostic.line, diagnostic.column))
        };
        let parse_error = |error: toml::de::Error| {
            let offset = error.span().map_or(0, |s| s.start);
            let path = locations
                .iter()
                .filter(|l| l.range.contains(&offset) || l.key <= offset && offset <= l.range.end)
                .min_by_key(|l| l.range.len())
                .map_or("<document>", |l| l.path.as_str());
            let diagnostic = diagnostic(source, path, offset);
            SettingsError(format!("{path} at line {}, column {}: {error}", diagnostic.line, diagnostic.column))
        };
        if let Some(error) = errors.into_iter().next() {
            return Err(parse_error(error));
        }
        let mut unknown_keys = Vec::new();
        unknown(&value, "", &mut unknown_keys);
        // Deserialize from the original source, not Value::try_into, so Serde
        // type/enum errors retain the parser's byte spans and key context.
        let settings: Self = toml::from_str(source).map_err(parse_error)?;
        settings.validate(locate)?;
        let unknown_key_diagnostics = unknown_keys
            .iter()
            .map(|path| {
                let offset = locations.iter().find(|l| l.path == *path).map_or(0, |l| l.key);
                diagnostic(source, path, offset)
            })
            .collect();
        Ok(SettingsReport { settings, unknown_keys, unknown_key_diagnostics })
    }
    pub fn keymap(&self) -> Result<Keymap, SettingsError> {
        Keymap::preset(self.keymap)
            .and_then(|map| map.with_overrides(&self.bindings))
            .map_err(|e| SettingsError(e.to_string()))
    }
    fn validate(&self, error: impl Fn(&str, &str) -> SettingsError) -> Result<(), SettingsError> {
        for (path, invalid, message) in [
            ("font.family", self.font.family.trim().is_empty(), "must be nonempty"),
            ("font.size", !self.font.size.is_finite() || self.font.size <= 0.0, "must be positive and finite"),
            (
                "font.line_height",
                !self.font.line_height.is_finite() || self.font.line_height <= 0.0,
                "must be positive and finite",
            ),
            ("editor.tab_size", self.editor.tab_size == 0, "must be positive"),
            ("files.journal_interval_seconds", self.files.journal_interval_seconds == 0, "must be positive"),
            ("undo.memory_limit_bytes", self.undo.memory_limit_bytes == 0, "must be positive"),
        ] {
            if invalid {
                return Err(error(path, message));
            }
        }
        for (kind, formatter) in &self.formatters {
            if formatter.executable.trim().is_empty() {
                return Err(error(&format!("formatters.{kind}.executable"), "needs an executable"));
            }
            if formatter.timeout_ms == 0 {
                return Err(error(&format!("formatters.{kind}.timeout_ms"), "must be positive"));
            }
        }
        for (i, binding) in self.bindings.iter().enumerate() {
            if !binding.command.is_empty() && crate::command(&binding.command).is_none() {
                return Err(error(&format!("bindings[{i}].command"), &format!("unknown command: {}", binding.command)));
            }
            binding
                .keys
                .parse::<crate::KeySequence>()
                .map_err(|e| error(&format!("bindings[{i}].keys"), &e.to_string()))?;
        }
        self.keymap()?;
        Ok(())
    }
    /// Reads at most 1 MiB of configuration, never a document-sized buffer.
    /// Missing file means defaults; unreadable/malformed files are errors.
    pub fn load(path: &Path) -> Result<SettingsReport, SettingsError> {
        Self::parse(read_settings_source(path)?.as_deref().unwrap_or(""))
    }

    /// A hook, not an active watcher. Register `handle.path().to_path_buf()`
    /// with FileWatcher and call `reload()` on Modified/Removed/Renamed events
    /// and on backend rescan/overflow errors. Keep the FileWatcher alive.
    /// The first reload delivers defaults if the file is absent. Errors leave
    /// the last valid report intact and do not invoke the callback.
    pub fn watch<F: FnMut(&SettingsReport)>(path: impl Into<PathBuf>, callback: F) -> SettingsWatch<F> {
        SettingsWatch { path: path.into(), callback, last: None }
    }

    /// Does not create directories or modify process/user environment.
    pub fn path(executable: &Path) -> Result<PathBuf, SettingsError> {
        SettingsLocation::from_environment().resolve(executable)
    }
}

pub struct SettingsWatch<F> {
    path: PathBuf,
    callback: F,
    last: Option<SettingsReport>,
}
impl<F: FnMut(&SettingsReport)> SettingsWatch<F> {
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn current(&self) -> Option<&SettingsReport> {
        self.last.as_ref()
    }
    pub fn reload(&mut self) -> Result<bool, SettingsError> {
        let report = Settings::load(&self.path)?;
        if self.last.as_ref() == Some(&report) {
            return Ok(false);
        }
        (self.callback)(&report);
        self.last = Some(report);
        Ok(true)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Windows,
    Mac,
    Unix,
}
/// Injectable environment snapshot permits cross-platform tests without
/// changing process environment (or racing other tests).
#[derive(Clone, Debug)]
pub struct SettingsLocation {
    pub platform: Platform,
    pub appdata: Option<PathBuf>,
    pub home: Option<PathBuf>,
    pub xdg_config_home: Option<PathBuf>,
}
impl SettingsLocation {
    pub fn from_environment() -> Self {
        Self {
            platform: if cfg!(target_os = "windows") {
                Platform::Windows
            } else if cfg!(target_os = "macos") {
                Platform::Mac
            } else {
                Platform::Unix
            },
            appdata: std::env::var_os("APPDATA").map(PathBuf::from),
            home: std::env::var_os("HOME").map(PathBuf::from),
            xdg_config_home: std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        }
    }
    pub fn resolve(&self, executable: &Path) -> Result<PathBuf, SettingsError> {
        let folder = executable
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or_else(|| SettingsError("executable must have a parent directory".into()))?;
        let marker = folder.join("kwil.portable");
        match fs::metadata(marker) {
            Ok(metadata) if metadata.is_file() => return Ok(folder.join("settings.toml")),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(SettingsError(e.to_string())),
        }
        fn valid(path: &Option<PathBuf>) -> Option<&PathBuf> {
            path.as_ref().filter(|p| p.is_absolute())
        }
        let folder = match self.platform {
            Platform::Windows => valid(&self.appdata).map(|p| p.join("kwil")),
            Platform::Mac => valid(&self.home).map(|p| p.join("Library/Application Support/kwil")),
            Platform::Unix => valid(&self.xdg_config_home)
                .map(|p| p.join("kwil"))
                .or_else(|| valid(&self.home).map(|p| p.join(".config/kwil"))),
        }
        .ok_or_else(|| SettingsError("no absolute per-user configuration directory available".into()))?;
        Ok(folder.join("settings.toml"))
    }
}

struct SourceLocation {
    path: String,
    key: usize,
    range: std::ops::Range<usize>,
}

fn diagnostic(source: &str, path: &str, offset: usize) -> SettingsDiagnostic {
    let prefix = &source[..offset.min(source.len())];
    SettingsDiagnostic {
        path: path.into(),
        line: prefix.bytes().filter(|&b| b == b'\n').count() + 1,
        column: prefix.rsplit('\n').next().unwrap_or_default().chars().count() + 1,
    }
}

// Parser-owned spans handle quoted/dotted keys, inline tables, multiline
// values and repeated array tables without guessing from source text.
fn collect_locations(value: &toml::de::DeValue<'_>, path: &str, result: &mut Vec<SourceLocation>) {
    match value {
        toml::de::DeValue::Table(table) => {
            for (key, value) in table {
                let full =
                    if path.is_empty() { key.get_ref().to_string() } else { format!("{path}.{}", key.get_ref()) };
                result.push(SourceLocation { path: full.clone(), key: key.span().start, range: value.span() });
                collect_locations(value.get_ref(), &full, result);
            }
        }
        toml::de::DeValue::Array(array) => {
            for (i, value) in array.iter().enumerate() {
                let full = format!("{path}[{i}]");
                result.push(SourceLocation { path: full.clone(), key: value.span().start, range: value.span() });
                collect_locations(value.get_ref(), &full, result);
            }
        }
        _ => {}
    }
}

fn known(path: &str) -> &'static [&'static str] {
    match path {
        "" => &[
            "keymap",
            "bindings",
            "render",
            "update",
            "font",
            "theme",
            "editor",
            "files",
            "undo",
            "search",
            "compare",
            "syntax",
            "log",
            "formatters",
            "file_types",
            "tabs",
        ],
        "render" => &["remote", "smooth_scrolling", "cursor_blink"],
        "tabs" => &["layout"],
        "update" => &["check", "channel"],
        "font" => &["family", "size", "line_height", "ligatures"],
        "theme" => &["name", "follow_os", "colors"],
        "editor" => &[
            "word_wrap",
            "tab_size",
            "insert_spaces",
            "auto_indent",
            "show_whitespace",
            "trim_trailing_whitespace_on_save",
        ],
        "files" => &["small_file_threshold_bytes", "backup", "restore_session", "journal_interval_seconds"],
        "undo" => &["memory_limit_bytes", "persist"],
        "search" => &["regex", "case_sensitive", "whole_word", "in_selection", "include", "exclude"],
        "compare" => &["ignore_whitespace", "collapse_unchanged"],
        "syntax" => &["full_parse_limit_bytes"],
        "log" => &["follow", "detect_levels", "highlight_rules"],
        "bindings[]" => &["keys", "command", "context", "mode"],
        "formatters.*" => &["executable", "args", "timeout_ms"],
        _ => &[],
    }
}
fn unknown(value: &toml::de::DeValue, path: &str, result: &mut Vec<String>) {
    if let Some(table) = value.as_table() {
        for (key, value) in table {
            let key = key.get_ref();
            let value = value.get_ref();
            let full = if path.is_empty() { key.to_string() } else { format!("{path}.{key}") };
            if !known(path).contains(&key.as_ref()) {
                result.push(full);
                continue;
            }
            if full == "formatters" {
                if let Some(formatters) = value.as_table() {
                    for (name, formatter) in formatters {
                        let name = name.get_ref();
                        let formatter = formatter.get_ref();
                        if let Some(fields) = formatter.as_table() {
                            for field in fields.keys() {
                                let field = field.get_ref();
                                if !known("formatters.*").contains(&field.as_ref()) {
                                    result.push(format!("formatters.{name}.{field}"));
                                }
                            }
                        }
                    }
                }
            } else if full == "bindings" {
                if let Some(bindings) = value.as_array() {
                    for (i, binding) in bindings.iter().enumerate() {
                        let mut fields = Vec::new();
                        unknown(binding.get_ref(), "bindings[]", &mut fields);
                        result.extend(fields.into_iter().map(|s| s.replace("bindings[]", &format!("bindings[{i}]"))));
                    }
                }
            } else if !matches!(full.as_str(), "theme.colors" | "log.highlight_rules" | "file_types") {
                unknown(value, &full, result);
            }
        }
    }
}

/// Worker-only bounded read, including the exact source used for optimistic
/// save conflict detection. Missing and empty files remain distinguishable.
pub fn read_settings_source(path: &Path) -> Result<Option<String>, SettingsError> {
    read_settings_source_with(path, open_settings_regular)
}
fn open_settings_regular(path: &Path) -> std::io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Nonblocking open closes the regular-file -> FIFO race. NOFOLLOW
        // also rejects symlink replacement between lstat and open.
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options.open(path)
}
fn read_settings_source_with(
    path: &Path,
    open: impl FnOnce(&Path) -> std::io::Result<fs::File>,
) -> Result<Option<String>, SettingsError> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_file() && !m.file_type().is_symlink() => {}
        Ok(_) => return Err(SettingsError("settings must be a regular file, not a symlink or special file".into())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(SettingsError(e.to_string())),
    }
    let file = match open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(SettingsError(e.to_string())),
    };
    let metadata = file.metadata().map_err(|e| SettingsError(e.to_string()))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(SettingsError("settings changed to a special file during open".into()));
    }
    let mut source = String::new();
    file.take(1024 * 1024 + 1).read_to_string(&mut source).map_err(|e| SettingsError(e.to_string()))?;
    if source.len() > 1024 * 1024 {
        return Err(SettingsError("settings exceed 1 MiB".into()));
    }
    Ok(Some(source))
}

/// Same-directory write + sync + atomic replacement. Call on a worker.
/// Refuses invalid documents, symlinks, read-only files and stale snapshots.
/// `expected == None` means the path must still be absent. This is optimistic
/// concurrency, not an OS lock against unrelated editors: the last check and
/// replacement cannot form a cross-process compare-and-swap.
/// Replacement errors retain the pending and previous-file survivors; the
/// error names their recovery paths. Inspect those files before retrying.
pub fn write_settings_atomic(path: &Path, expected: Option<&str>, source: &str) -> Result<(), SettingsError> {
    atomic_settings_write_with(path, expected, source, replace_settings_file)
}

fn atomic_settings_write_with(
    path: &Path,
    expected: Option<&str>,
    source: &str,
    replace: impl FnOnce(&Path, &Path, Option<&Path>) -> std::io::Result<()>,
) -> Result<(), SettingsError> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    crate::settings_ui::SettingsDocument::parse(source)?;
    if !path.is_absolute() {
        return Err(SettingsError("settings path must be absolute".into()));
    }
    let parent = path.parent().ok_or_else(|| SettingsError("settings path needs a parent".into()))?;
    let metadata = match fs::symlink_metadata(path) {
        Ok(m) if m.is_file() && !m.file_type().is_symlink() && !m.permissions().readonly() => Some(m),
        Ok(_) => return Err(SettingsError("settings path must be a writable regular file, not a symlink".into())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(SettingsError(e.to_string())),
    };
    if read_settings_source(path)?.as_deref() != expected {
        return Err(SettingsError("settings changed on disk; reload before saving".into()));
    }
    fs::create_dir_all(parent).map_err(|e| SettingsError(e.to_string()))?;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let (temp_path, mut file) = (0..32)
        .find_map(|_| {
            let temp =
                parent.join(format!(".kwil-settings-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&temp) {
                Ok(f) => Some(Ok((temp, f))),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => None,
                Err(e) => Some(Err(e)),
            }
        })
        .ok_or_else(|| SettingsError("could not allocate settings temporary file".into()))?
        .map_err(|e| SettingsError(e.to_string()))?;
    struct Temp {
        path: PathBuf,
        cleanup: bool,
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            if self.cleanup {
                let _ = fs::remove_file(&self.path);
            }
        }
    }
    let mut temp = Temp { path: temp_path, cleanup: true };
    let io = |e: std::io::Error| SettingsError(e.to_string());
    file.write_all(source.as_bytes()).map_err(io)?;
    if let Some(m) = &metadata {
        file.set_permissions(m.permissions()).map_err(io)?;
    }
    file.sync_all().map_err(io)?;
    drop(file);
    if read_settings_source(path)?.as_deref() != expected {
        return Err(SettingsError("settings changed during save; reload before saving".into()));
    }
    // Reserve a unique namespace through the create_new temp above. Never
    // overwrite a recovery file left by an older run with a recycled PID.
    let backup_path = temp.path.with_extension("previous");
    match fs::symlink_metadata(&backup_path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => return Err(SettingsError(format!("recovery path already exists: {}", backup_path.display()))),
        Err(e) => return Err(io(e)),
    }
    let backup = metadata.as_ref().map(|_| backup_path.as_path());
    if let Err(e) = replace(&temp.path, path, backup) {
        // ReplaceFileW 1177 may have moved the original to the backup name;
        // 1176 without a backup may delete it. Always request a backup, retain
        // both survivors on ANY error, and restore a missing canonical name by
        // a no-clobber hard link. An external replacement is never overwritten.
        temp.cleanup = false;
        let restored = backup.is_some_and(|backup| fs::hard_link(backup, path).is_ok());
        return Err(SettingsError(format!(
            "settings replacement failed: {e}; automatic restore link created: {restored}; recovery files preserved where present: pending={}, previous={}, destination={}; inspect before retrying",
            temp.path.display(),
            backup_path.display(),
            path.display()
        )));
    }
    if backup.is_some() {
        match fs::remove_file(&backup_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(SettingsError(format!(
                    "settings committed, but previous file remains at {}: {e}",
                    backup_path.display()
                )));
            }
        }
    }
    #[cfg(unix)]
    fs::File::open(parent)
        .and_then(|f| f.sync_all())
        .map_err(|e| SettingsError(format!("settings replaced, but directory sync failed: {e}")))?;
    Ok(())
}
fn replace_settings_file(temp: &Path, path: &Path, backup: Option<&Path>) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW, ReplaceFileW};
        let temp: Vec<_> = temp.as_os_str().encode_wide().chain(Some(0)).collect();
        let path: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let backup: Option<Vec<_>> = backup.map(|p| p.as_os_str().encode_wide().chain(Some(0)).collect());
        // ReplaceFile preserves the destination ACL; never delete it first.
        let ok = unsafe {
            if let Some(backup) = backup.as_ref() {
                ReplaceFileW(path.as_ptr(), temp.as_ptr(), backup.as_ptr(), 0, std::ptr::null(), std::ptr::null())
            } else {
                MoveFileExW(temp.as_ptr(), path.as_ptr(), MOVEFILE_WRITE_THROUGH)
            }
        };
        if ok == 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
    }
    #[cfg(not(windows))]
    {
        let _ = backup;
        fs::rename(temp, path)
    }
}

#[derive(Debug)]
pub enum SettingsWorkerEvent {
    Loaded { source: Option<String>, document: Box<crate::settings_ui::SettingsDocument> },
    Saved { request: u64, source: String },
    Error { request: Option<u64>, message: String },
}
enum SettingsRequest {
    Save { request: u64, expected: Option<String>, source: String },
    Reload,
    Stop,
}
/// All reads, writes, native watcher setup and parsing run off the UI thread.
/// The bounded command queue refuses overload. Callbacks should enqueue a UI
/// event and return promptly. Keep this handle alive; shutdown joins its worker.
pub struct SettingsWorker {
    tx: std::sync::mpsc::SyncSender<SettingsRequest>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl SettingsWorker {
    /// Spawning does no filesystem I/O on the caller. Errors arrive as events.
    pub fn start(
        path: PathBuf,
        mut callback: impl FnMut(SettingsWorkerEvent) + Send + 'static,
    ) -> std::io::Result<Self> {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc,
        };
        let (tx, rx) = mpsc::sync_channel(32);
        let events = tx.clone();
        let worker = std::thread::Builder::new().name("kwil-settings".into()).spawn(move || {
            if !path.is_absolute() {
                callback(SettingsWorkerEvent::Error {
                    request: None,
                    message: "settings path must be absolute".into(),
                });
                return;
            }
            let dirty = Arc::new(AtomicBool::new(false));
            let invalidated = dirty.clone();
            let watcher = path
                .parent()
                .ok_or_else(|| std::io::Error::other("settings path needs parent"))
                .and_then(fs::create_dir_all)
                .and_then(|_| {
                    kwil_core_workspace::FileWatcher::new(
                        std::slice::from_ref(&path),
                        std::time::Duration::from_millis(40),
                        move |_| {
                            invalidated.store(true, Ordering::Release);
                            let _ = events.try_send(SettingsRequest::Reload);
                        },
                    )
                });
            let _watcher = match watcher {
                Ok(w) => w,
                Err(e) => {
                    callback(SettingsWorkerEvent::Error {
                        request: None,
                        message: format!("live settings watcher unavailable: {e}"),
                    });
                    return;
                }
            };
            let mut last: Option<Option<String>> = None;
            let reload = |last: &mut Option<Option<String>>, callback: &mut dyn FnMut(SettingsWorkerEvent)| {
                match read_settings_source(&path).and_then(|source| {
                    crate::settings_ui::SettingsDocument::parse(source.as_deref().unwrap_or(""))
                        .map(|document| (source, document))
                }) {
                    Ok((source, document)) => {
                        if last.as_ref() != Some(&source) {
                            *last = Some(source.clone());
                            callback(SettingsWorkerEvent::Loaded { source, document: Box::new(document) });
                        }
                    }
                    Err(e) => callback(SettingsWorkerEvent::Error { request: None, message: e.to_string() }),
                }
            };
            reload(&mut last, &mut callback);
            while let Ok(request) = rx.recv() {
                match request {
                    SettingsRequest::Stop => break,
                    SettingsRequest::Reload => {
                        dirty.store(false, Ordering::Release);
                        reload(&mut last, &mut callback);
                    }
                    SettingsRequest::Save { request, expected, source } => {
                        match write_settings_atomic(&path, expected.as_deref(), &source) {
                            Ok(()) => {
                                callback(SettingsWorkerEvent::Saved { request, source });
                                reload(&mut last, &mut callback);
                            }
                            Err(e) => {
                                callback(SettingsWorkerEvent::Error { request: Some(request), message: e.to_string() })
                            }
                        }
                    }
                }
                if dirty.swap(false, Ordering::AcqRel) {
                    reload(&mut last, &mut callback);
                }
            }
        })?;
        Ok(Self { tx, worker: Some(worker) })
    }
    pub fn save(&self, request: u64, expected: Option<String>, source: String) -> Result<(), SettingsError> {
        if source.len() > 1024 * 1024 || expected.as_ref().is_some_and(|s| s.len() > 1024 * 1024) {
            return Err(SettingsError("settings exceed 1 MiB".into()));
        }
        self.tx
            .try_send(SettingsRequest::Save { request, expected, source })
            .map_err(|e| SettingsError(format!("settings queue unavailable: {e}")))
    }
    pub fn reload(&self) -> Result<(), SettingsError> {
        self.tx.try_send(SettingsRequest::Reload).map_err(|e| SettingsError(e.to_string()))
    }
    /// Run shutdown away from an interactive event handler: it waits for any
    /// in-flight OS write, then drops the native watch and joins both workers.
    pub fn shutdown(mut self) {
        self.stop();
    }
    fn stop(&mut self) {
        let _ = self.tx.send(SettingsRequest::Stop);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
impl Drop for SettingsWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod settings_atomic_tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            let path = std::env::temp_dir().join(format!(
                "kwil-settings-r1-{}-{time}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            // Only files in this test's create_new directory, never a shared tree.
            if let Ok(entries) = fs::read_dir(&self.0) {
                for entry in entries.flatten() {
                    let _ = fs::remove_file(entry.path());
                }
            }
            let _ = fs::remove_dir(&self.0);
        }
    }
    #[test]
    fn invalid_stale_and_failed_replacement_preserve_owner_file() {
        let fixture = Fixture::new();
        let path = &fixture.0;
        let file = path.join("settings.toml");
        let source = "# keep\n[editor]\ntab_size=8\n";
        fs::write(&file, source).unwrap();
        assert!(write_settings_atomic(&file, Some(source), "[editor]\ntab_size=0").is_err());
        assert!(write_settings_atomic(&file, Some("stale"), "").is_err());
        assert!(
            atomic_settings_write_with(&file, Some(source), "[editor]\ntab_size=3", |_, _, _| Err(
                std::io::Error::other("injected replace failure")
            ))
            .is_err()
        );
        assert_eq!(fs::read_to_string(&file).unwrap(), source);
        assert_eq!(fs::read_dir(path).unwrap().count(), 2, "replacement errors retain the new source for recovery");
        write_settings_atomic(&file, Some(source), "[editor]\ntab_size=3").unwrap();
        assert_eq!(Settings::load(&file).unwrap().settings.editor.tab_size, 3);
    }
    #[test]
    fn partial_replacement_errors_preserve_backup_and_new_source_without_clobbering_foreign_file() {
        let old = "# owner\n[editor]\ntab_size=7\n";
        let new = "[editor]\ntab_size=2\n";
        for (code, foreign) in [(1176, false), (1177, false), (1177, true)] {
            let fixture = Fixture::new();
            let path = fixture.0.join("settings.toml");
            fs::write(&path, old).unwrap();
            let error = atomic_settings_write_with(&path, Some(old), new, |temp, path, backup| {
                let backup = backup.expect("existing settings replacement must request an explicit backup");
                assert_eq!(fs::read_to_string(temp).unwrap(), new);
                if code == 1177 {
                    fs::rename(path, backup)?;
                    if foreign {
                        fs::write(path, "foreign owner edit")?;
                    }
                }
                Err(std::io::Error::from_raw_os_error(code))
            })
            .unwrap_err();
            assert!(error.0.contains("recovery files preserved"));
            assert_eq!(fs::read_to_string(&path).unwrap(), if foreign { "foreign owner edit" } else { old });
            let survivors: Vec<_> =
                fs::read_dir(&fixture.0).unwrap().map(|e| e.unwrap().path()).filter(|p| p != &path).collect();
            assert!(
                survivors.iter().any(|p| fs::read_to_string(p).unwrap() == new),
                "new-source survivor must not be deleted"
            );
            if code == 1177 {
                assert!(
                    survivors.iter().any(|p| fs::read_to_string(p).unwrap() == old),
                    "moved original must remain recoverable"
                );
            }
        }
    }
    #[test]
    fn special_settings_path_is_refused_before_open() {
        let fixture = Fixture::new();
        let mut opened = false;
        let error = read_settings_source_with(&fixture.0, |_| {
            opened = true;
            Err(std::io::Error::other("must not open special file"))
        })
        .unwrap_err();
        assert!(!opened);
        assert!(error.0.contains("regular file"));
    }
    #[cfg(unix)]
    fn fifo(path: &Path) -> std::io::Result<()> {
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        if unsafe { libc::mkfifo(name.as_ptr(), 0o600) } == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
    }
    #[cfg(unix)]
    #[test]
    fn regular_to_fifo_race_and_worker_shutdown_do_not_block() {
        use std::{sync::mpsc, time::Duration};
        let fixture = Fixture::new();
        let path = fixture.0.join("settings.toml");
        fs::write(&path, "").unwrap();
        let worker_path = path.clone();
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let result = read_settings_source_with(&worker_path, |path| {
                fs::remove_file(path)?;
                fifo(path)?;
                open_settings_regular(path)
            });
            let _ = tx.send(result.map_err(|e| e.0));
        });
        let result = rx.recv_timeout(Duration::from_secs(2)).expect("FIFO swap read must not block");
        assert!(result.unwrap_err().contains("special file"));
        thread.join().unwrap();
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let (events, received) = mpsc::channel();
            let worker = SettingsWorker::start(path, move |event| {
                let _ = events.send(event);
            })
            .unwrap();
            let event = received.recv_timeout(Duration::from_secs(1));
            drop(worker);
            let _ = tx.send(event);
        });
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(2)).expect("worker Drop must not wait for a FIFO writer").unwrap(),
            SettingsWorkerEvent::Error { .. }
        ));
        thread.join().unwrap();
    }
}
