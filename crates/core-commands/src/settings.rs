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
        let value: toml::Value = toml::from_str(source).map_err(|e| SettingsError(e.to_string()))?;
        let mut unknown_keys = Vec::new();
        unknown(&value, "", &mut unknown_keys);
        let settings: Self = value.try_into().map_err(|e| SettingsError(e.to_string()))?;
        settings.validate()?;
        Ok(SettingsReport { settings, unknown_keys })
    }
    pub fn keymap(&self) -> Result<Keymap, SettingsError> {
        Keymap::preset(self.keymap)
            .and_then(|map| map.with_overrides(&self.bindings))
            .map_err(|e| SettingsError(e.to_string()))
    }
    fn validate(&self) -> Result<(), SettingsError> {
        if self.font.family.trim().is_empty()
            || !self.font.size.is_finite()
            || self.font.size <= 0.0
            || !self.font.line_height.is_finite()
            || self.font.line_height <= 0.0
        {
            return Err(SettingsError("font family, size and line_height must be nonempty/positive and finite".into()));
        }
        if self.editor.tab_size == 0 || self.files.journal_interval_seconds == 0 || self.undo.memory_limit_bytes == 0 {
            return Err(SettingsError("tab size, journal interval and undo memory cap must be positive".into()));
        }
        for (kind, formatter) in &self.formatters {
            if formatter.executable.trim().is_empty() || formatter.timeout_ms == 0 {
                return Err(SettingsError(format!("formatter {kind} needs an executable and positive timeout")));
            }
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
fn unknown(value: &toml::Value, path: &str, result: &mut Vec<String>) {
    if let Some(table) = value.as_table() {
        for (key, value) in table {
            let full = if path.is_empty() { key.clone() } else { format!("{path}.{key}") };
            if !known(path).contains(&key.as_str()) {
                result.push(full);
                continue;
            }
            if full == "formatters" {
                if let Some(formatters) = value.as_table() {
                    for (name, formatter) in formatters {
                        if let Some(fields) = formatter.as_table() {
                            for field in fields.keys() {
                                if !known("formatters.*").contains(&field.as_str()) {
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
                        unknown(binding, "bindings[]", &mut fields);
                        result.extend(fields.into_iter().map(|s| s.replace("bindings[]", &format!("bindings[{i}]"))));
                    }
                }
            } else if !matches!(full.as_str(), "theme.colors" | "log.highlight_rules") {
                unknown(value, &full, result);
            }
        }
    }
}
