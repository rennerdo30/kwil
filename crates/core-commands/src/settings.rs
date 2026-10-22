//! One settings file. Watching is deliberately an event callback hook, ready for
//! core-workspace's FileWatcher: register the settings FILE path (it watches
//! parent directories internally), then call SettingsWatch::reload on events.
//! This crate starts no watcher, thread, polling loop or startup-path I/O.
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

group!(RenderSettings { remote: Remote = Remote::Auto, smooth_scrolling: bool = false, cursor_blink: bool = true });
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
        let file = match fs::File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::parse(""),
            Err(e) => return Err(SettingsError(e.to_string())),
        };
        let mut source = String::new();
        file.take(1024 * 1024 + 1).read_to_string(&mut source).map_err(|e| SettingsError(e.to_string()))?;
        if source.len() > 1024 * 1024 {
            return Err(SettingsError("settings exceed 1 MiB".into()));
        }
        Self::parse(&source)
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
        ],
        "render" => &["remote", "smooth_scrolling", "cursor_blink"],
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
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(SettingsError(e.to_string())),
    };
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
pub fn write_settings_atomic(path: &Path, expected: Option<&str>, source: &str) -> Result<(), SettingsError> {
    atomic_settings_write_with(path, expected, source, replace_settings_file)
}

fn atomic_settings_write_with(
    path: &Path,
    expected: Option<&str>,
    source: &str,
    replace: impl FnOnce(&Path, &Path, bool) -> std::io::Result<()>,
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
    struct Temp(PathBuf);
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }
    let temp = Temp(temp_path);
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
    replace(&temp.0, path, metadata.is_some()).map_err(io)?;
    #[cfg(unix)]
    fs::File::open(parent)
        .and_then(|f| f.sync_all())
        .map_err(|e| SettingsError(format!("settings replaced, but directory sync failed: {e}")))?;
    Ok(())
}
fn replace_settings_file(temp: &Path, path: &Path, exists: bool) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW, ReplaceFileW};
        let temp: Vec<_> = temp.as_os_str().encode_wide().chain(Some(0)).collect();
        let path: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // ReplaceFile preserves the destination ACL; never delete it first.
        let ok = unsafe {
            if exists {
                ReplaceFileW(path.as_ptr(), temp.as_ptr(), std::ptr::null(), 0, std::ptr::null(), std::ptr::null())
            } else {
                MoveFileExW(temp.as_ptr(), path.as_ptr(), MOVEFILE_WRITE_THROUGH)
            }
        };
        if ok == 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
    }
    #[cfg(not(windows))]
    {
        let _ = exists;
        fs::rename(temp, path)
    }
}

#[derive(Debug)]
pub enum SettingsWorkerEvent {
    Loaded { source: Option<String>, document: crate::settings_ui::SettingsDocument },
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
                .and_then(|p| fs::create_dir_all(p))
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
                            callback(SettingsWorkerEvent::Loaded { source, document });
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
    #[test]
    fn invalid_stale_and_failed_replacement_preserve_owner_file() {
        let path = std::env::temp_dir().join(format!("kwil-settings-atomic-{}", std::process::id()));
        fs::create_dir(&path).unwrap();
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
        assert_eq!(fs::read_dir(&path).unwrap().count(), 1, "failed writes clean only their own temp");
        write_settings_atomic(&file, Some(source), "[editor]\ntab_size=3").unwrap();
        assert_eq!(Settings::load(&file).unwrap().settings.editor.tab_size, 3);
        fs::remove_file(file).unwrap();
        fs::remove_dir(path).unwrap();
    }
}
