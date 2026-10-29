//! Frontend-neutral settings screen. Construction, search, editing and key
//! capture are memory-only. Submit `source()` to `SettingsWorker` for I/O.
//! `Settings` remains the existing public preferences type; this document also
//! owns the lossless TOML and extension mappings used by KW-47.
//!
//! ```
//! use kwil_core_commands::settings_ui::{SettingsDocument, SettingsMenu, SettingValue};
//! let mut menu = SettingsMenu::new(SettingsDocument::parse("").unwrap());
//! menu.search("indent width");
//! menu.edit_selected(SettingValue::Integer(2)).unwrap();
//! assert_eq!(menu.document().settings().editor.tab_size, 2);
//! // Enqueue document.source() with the original Loaded.source snapshot on
//! // SettingsWorker. The frontend owns its wake callback and screen drawing.
//! ```
use crate::settings::{SettingsError, SettingsReport};
use crate::{Binding, Context, Key, KeyChord, KeySequence, Mode, NamedKey, Resolution, Settings};
use std::collections::BTreeMap;
use toml_edit::{DocumentMut, Item, Table, Value};

/// Existing capture and conflict-preview limit; raw settings remain lossless.
pub const MAX_REBIND_CHORDS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    Editor,
    Appearance,
    Keymap,
    Files,
    Search,
    Updates,
    Remote,
    Advanced,
}
pub const CATEGORIES: &[Category] = &[
    Category::Editor,
    Category::Appearance,
    Category::Keymap,
    Category::Files,
    Category::Search,
    Category::Updates,
    Category::Remote,
    Category::Advanced,
];
impl Category {
    pub fn title(self) -> &'static str {
        match self {
            Self::Editor => "Editor",
            Self::Appearance => "Appearance / Themes",
            Self::Keymap => "Keymap",
            Self::Files => "Files and file types",
            Self::Search => "Search",
            Self::Updates => "Updates",
            Self::Remote => "Remote mode",
            Self::Advanced => "Advanced",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum SettingValue {
    Bool(bool),
    Integer(i64),
    Float(f64),
    Text(String),
    Strings(Vec<String>),
    StringMap(BTreeMap<String, String>),
    Bindings(Vec<Binding>),
    Formatters(BTreeMap<String, crate::settings::Formatter>),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingKind {
    Bool,
    Integer,
    Float,
    Text,
    Choice(&'static [&'static str]),
    Strings,
    StringMap,
    Bindings,
    Formatters,
}
#[derive(Clone, Copy, Debug)]
pub struct SettingSpec {
    pub id: &'static str,
    pub title: &'static str,
    pub category: Category,
    pub description: &'static str,
    pub kind: SettingKind,
    read: fn(&Settings) -> SettingValue,
}
impl SettingSpec {
    fn current(&self, settings: &Settings) -> SettingValue {
        (self.read)(settings)
    }
    pub fn default_value(&self) -> SettingValue {
        self.current(&Settings::default())
    }
}
macro_rules! spec {
    ($id:literal,$title:literal,$cat:ident,$desc:literal,$kind:expr,$read:expr) => {
        SettingSpec { id: $id, title: $title, category: Category::$cat, description: $desc, kind: $kind, read: $read }
    };
}
pub const SCHEMA: &[SettingSpec] = &[
    spec!(
        "tabs.layout",
        "Tabs layout",
        Appearance,
        "Arrange visible file tabs horizontally at the top or vertically on the left.",
        SettingKind::Choice(&["horizontal", "vertical"]),
        |s| SettingValue::Text(
            match s.tabs.layout {
                crate::settings::TabLayout::Horizontal => "horizontal",
                crate::settings::TabLayout::Vertical => "vertical",
            }
            .into()
        )
    ),
    spec!(
        "bindings",
        "Custom bindings",
        Keymap,
        "Capture keys to rebind commands. Conflicts include active exact and prefix shortcuts; former keys receive explicit unbinds.",
        SettingKind::Bindings,
        |s| SettingValue::Bindings(s.bindings.clone())
    ),
    spec!(
        "file_types",
        "Extension mappings",
        Files,
        "Map an extension to a language. Defaults use automatic detection. OS registration is a separate explicit action.",
        SettingKind::StringMap,
        |_| SettingValue::StringMap(BTreeMap::new())
    ),
    spec!(
        "formatters",
        "External formatters",
        Advanced,
        "Per-language executable, argument list and positive timeout in milliseconds. Add an executable first; edit fields individually.",
        SettingKind::Formatters,
        |s| SettingValue::Formatters(s.formatters.clone())
    ),
    spec!(
        "keymap",
        "Preset",
        Keymap,
        "Choose VS Code, Notepad++ or Vim shortcuts. Custom bindings remain in effect.",
        SettingKind::Choice(&["vscode", "notepad++", "vim"]),
        |s| SettingValue::Text(
            match s.keymap {
                crate::Preset::VsCode => "vscode",
                crate::Preset::NotepadPlusPlus => "notepad++",
                crate::Preset::Vim => "vim",
            }
            .into()
        )
    ),
    spec!("editor.word_wrap", "Word wrap", Editor, "Wrap long lines to the viewport.", SettingKind::Bool, |s| {
        SettingValue::Bool(s.editor.word_wrap)
    }),
    spec!("editor.tab_size", "Tab size", Editor, "Indent width in columns (1 to 255).", SettingKind::Integer, |s| {
        SettingValue::Integer(s.editor.tab_size.into())
    }),
    spec!("editor.insert_spaces", "Insert spaces", Editor, "Use spaces when indenting.", SettingKind::Bool, |s| {
        SettingValue::Bool(s.editor.insert_spaces)
    }),
    spec!("editor.auto_indent", "Auto indent", Editor, "Continue indentation on a new line.", SettingKind::Bool, |s| {
        SettingValue::Bool(s.editor.auto_indent)
    }),
    spec!("editor.show_whitespace", "Whitespace", Editor, "Show spaces and tabs.", SettingKind::Bool, |s| {
        SettingValue::Bool(s.editor.show_whitespace)
    }),
    spec!(
        "editor.trim_trailing_whitespace_on_save",
        "Trim on save",
        Editor,
        "Remove trailing whitespace when saving.",
        SettingKind::Bool,
        |s| SettingValue::Bool(s.editor.trim_trailing_whitespace_on_save)
    ),
    spec!("font.family", "Font family", Appearance, "Nonempty font family name.", SettingKind::Text, |s| {
        SettingValue::Text(s.font.family.clone())
    }),
    spec!("font.size", "Font size", Appearance, "Positive finite font size.", SettingKind::Float, |s| {
        SettingValue::Float(s.font.size.into())
    }),
    spec!(
        "font.line_height",
        "Line height",
        Appearance,
        "Positive finite line height multiplier.",
        SettingKind::Float,
        |s| SettingValue::Float(s.font.line_height.into())
    ),
    spec!("font.ligatures", "Ligatures", Appearance, "Enable font ligatures.", SettingKind::Bool, |s| {
        SettingValue::Bool(s.font.ligatures)
    }),
    spec!(
        "theme.name",
        "Theme",
        Appearance,
        "Choose the appearance of the editor and desktop windows.",
        SettingKind::Choice(&["dark", "light"]),
        |s| SettingValue::Text(s.theme.name.clone())
    ),
    spec!(
        "theme.follow_os",
        "Follow system theme",
        Appearance,
        "Follow operating system light and dark preference.",
        SettingKind::Bool,
        |s| SettingValue::Bool(s.theme.follow_os)
    ),
    spec!(
        "theme.colors",
        "Colour overrides",
        Appearance,
        "Named colour overrides. Edit one entry at a time.",
        SettingKind::StringMap,
        |s| SettingValue::StringMap(s.theme.colors.clone())
    ),
    spec!(
        "files.small_file_threshold_bytes",
        "Small file threshold",
        Files,
        "Files below this byte count use resident storage.",
        SettingKind::Integer,
        |s| SettingValue::Integer(s.files.small_file_threshold_bytes.min(i64::MAX as u64) as i64)
    ),
    spec!("files.backup", "Backup", Files, "Keep a backup when saving.", SettingKind::Bool, |s| SettingValue::Bool(
        s.files.backup
    )),
    spec!(
        "files.restore_session",
        "Restore session",
        Files,
        "Restore the previous editing session.",
        SettingKind::Bool,
        |s| SettingValue::Bool(s.files.restore_session)
    ),
    spec!(
        "files.journal_interval_seconds",
        "Journal interval",
        Files,
        "Positive seconds between recovery journal writes.",
        SettingKind::Integer,
        |s| SettingValue::Integer(s.files.journal_interval_seconds.into())
    ),
    spec!(
        "search.regex",
        "Regular expressions",
        Search,
        "Interpret search input as a regular expression.",
        SettingKind::Bool,
        |s| SettingValue::Bool(s.search.regex)
    ),
    spec!("search.case_sensitive", "Case sensitive", Search, "Match letter case.", SettingKind::Bool, |s| {
        SettingValue::Bool(s.search.case_sensitive)
    }),
    spec!("search.whole_word", "Whole word", Search, "Match complete words.", SettingKind::Bool, |s| {
        SettingValue::Bool(s.search.whole_word)
    }),
    spec!(
        "search.in_selection",
        "In selection",
        Search,
        "Search within the current selection.",
        SettingKind::Bool,
        |s| SettingValue::Bool(s.search.in_selection)
    ),
    spec!(
        "search.include",
        "Include globs",
        Search,
        "Include file patterns in folder search.",
        SettingKind::Strings,
        |s| SettingValue::Strings(s.search.include.clone())
    ),
    spec!(
        "search.exclude",
        "Exclude globs",
        Search,
        "Exclude file patterns from folder search.",
        SettingKind::Strings,
        |s| SettingValue::Strings(s.search.exclude.clone())
    ),
    spec!(
        "update.check",
        "Check for updates",
        Updates,
        "Check for updates on a background worker.",
        SettingKind::Bool,
        |s| SettingValue::Bool(s.update.check)
    ),
    spec!(
        "update.channel",
        "Update channel",
        Updates,
        "Beta builds default to prerelease updates; stable builds default to stable. Choose stable to receive only stable releases.",
        SettingKind::Choice(&["stable", "prerelease"]),
        |s| SettingValue::Text(
            match s.update.channel {
                crate::settings::UpdateChannel::Stable => "stable",
                crate::settings::UpdateChannel::Prerelease => "prerelease",
            }
            .into()
        )
    ),
    spec!(
        "render.remote",
        "Remote mode",
        Remote,
        "Auto detect, force on or force off remote desktop rendering.",
        SettingKind::Choice(&["auto", "on", "off"]),
        |s| SettingValue::Text(
            match s.render.remote {
                crate::settings::Remote::Auto => "auto",
                crate::settings::Remote::On => "on",
                crate::settings::Remote::Off => "off",
            }
            .into()
        )
    ),
    spec!(
        "render.backend",
        "Renderer",
        Appearance,
        "Automatic uses a hardware GPU when available. CPU uses the software renderer. Remote sessions keep CPU rendering.",
        SettingKind::Choice(&["auto", "cpu", "gpu"]),
        |s| SettingValue::Text(s.render.backend.as_str().into())
    ),
    spec!(
        "render.smooth_scrolling",
        "Smooth scrolling",
        Remote,
        "Animate scrolling while moving; disabled in remote mode.",
        SettingKind::Bool,
        |s| SettingValue::Bool(s.render.smooth_scrolling)
    ),
    spec!(
        "render.cursor_blink",
        "Cursor blink",
        Remote,
        "Blink the cursor while active; disabled in remote mode.",
        SettingKind::Bool,
        |s| SettingValue::Bool(s.render.cursor_blink)
    ),
    spec!(
        "undo.memory_limit_bytes",
        "Undo memory",
        Advanced,
        "Positive byte limit for undo memory.",
        SettingKind::Integer,
        |s| SettingValue::Integer(s.undo.memory_limit_bytes.min(i64::MAX as u64) as i64)
    ),
    spec!("undo.persist", "Persist undo", Advanced, "Keep undo history between sessions.", SettingKind::Bool, |s| {
        SettingValue::Bool(s.undo.persist)
    }),
    spec!(
        "compare.ignore_whitespace",
        "Ignore whitespace",
        Advanced,
        "Ignore whitespace during comparison.",
        SettingKind::Bool,
        |s| SettingValue::Bool(s.compare.ignore_whitespace)
    ),
    spec!(
        "compare.collapse_unchanged",
        "Collapse unchanged",
        Advanced,
        "Collapse unchanged comparison regions.",
        SettingKind::Bool,
        |s| SettingValue::Bool(s.compare.collapse_unchanged)
    ),
    spec!(
        "syntax.full_parse_limit_bytes",
        "Full parse limit",
        Advanced,
        "Above this byte count use viewport highlighting.",
        SettingKind::Integer,
        |s| SettingValue::Integer(s.syntax.full_parse_limit_bytes.min(i64::MAX as u64) as i64)
    ),
    spec!("log.follow", "Follow log", Advanced, "Follow appended log output.", SettingKind::Bool, |s| {
        SettingValue::Bool(s.log.follow)
    }),
    spec!(
        "log.detect_levels",
        "Detect log levels",
        Advanced,
        "Detect common log severity levels.",
        SettingKind::Bool,
        |s| SettingValue::Bool(s.log.detect_levels)
    ),
    spec!(
        "log.highlight_rules",
        "Log highlights",
        Advanced,
        "Named patterns for log highlighting. Edit one entry at a time.",
        SettingKind::StringMap,
        |s| SettingValue::StringMap(s.log.highlight_rules.clone())
    ),
];

fn error(message: impl Into<String>) -> SettingsError {
    SettingsError(message.into())
}
fn value(value: SettingValue) -> Result<Value, SettingsError> {
    Ok(match value {
        SettingValue::Bool(v) => v.into(),
        SettingValue::Integer(v) => v.into(),
        SettingValue::Float(v) if v.is_finite() => v.into(),
        SettingValue::Float(_) => return Err(error("number must be finite")),
        SettingValue::Text(v) => v.into(),
        SettingValue::Strings(v) => Value::Array(v.into_iter().collect()),
        SettingValue::StringMap(_) | SettingValue::Bindings(_) | SettingValue::Formatters(_) => {
            return Err(error("edit map entries individually to preserve comments; reset is unsupported"));
        }
    })
}

/// Lossless transaction: all edits validate a candidate before replacing self.
#[derive(Clone, Debug)]
pub struct SettingsDocument {
    document: DocumentMut,
    source: String,
    report: SettingsReport,
    file_types: BTreeMap<String, String>,
}
#[derive(Clone, Debug)]
pub struct CommandBindingRow {
    pub command: &'static crate::Command,
    pub current: Vec<Binding>,
    pub defaults: Vec<Binding>,
}
impl SettingsDocument {
    pub fn parse(source: &str) -> Result<Self, SettingsError> {
        if source.len() > 1024 * 1024 {
            return Err(error("settings exceed 1 MiB"));
        }
        let report = Settings::parse(source)?;
        let document = source.parse::<DocumentMut>().map_err(|e| error(e.to_string()))?;
        let mut file_types = BTreeMap::new();
        if let Some(item) = document.get("file_types") {
            let table = item.as_table_like().ok_or_else(|| error("file_types must be a string table"))?;
            for (key, item) in table.iter() {
                let ext = normalize_extension(key)?;
                let language = item
                    .as_str()
                    .filter(|s| valid_language(s))
                    .ok_or_else(|| error("file_types language must be a nonempty identifier"))?;
                if file_types.insert(ext, language.into()).is_some() {
                    return Err(error("duplicate case-insensitive extension mapping"));
                }
            }
        }
        Ok(Self { document, source: source.into(), report, file_types })
    }
    pub fn source(&self) -> &str {
        &self.source
    }
    pub fn settings(&self) -> &Settings {
        &self.report.settings
    }
    pub fn report(&self) -> &SettingsReport {
        &self.report
    }
    pub fn current_value(&self, id: &str) -> Result<SettingValue, SettingsError> {
        let spec = SCHEMA.iter().find(|s| s.id == id).ok_or_else(|| error("unknown settings field"))?;
        Ok(if id == "file_types" {
            SettingValue::StringMap(self.file_types.clone())
        } else {
            spec.current(self.settings())
        })
    }
    pub fn file_types(&self) -> &BTreeMap<String, String> {
        &self.file_types
    }
    pub fn language_for_extension(&self, extension: &str) -> Option<&str> {
        self.file_types.get(&normalize_extension(extension).ok()?).map(String::as_str)
    }
    fn transaction(
        &mut self,
        edit: impl FnOnce(&mut DocumentMut) -> Result<(), SettingsError>,
    ) -> Result<(), SettingsError> {
        let mut document = self.document.clone();
        edit(&mut document)?;
        *self = Self::parse(&document.to_string())?;
        Ok(())
    }
    pub fn edit(&mut self, id: &str, new: SettingValue) -> Result<(), SettingsError> {
        let spec = SCHEMA.iter().find(|s| s.id == id).ok_or_else(|| error("unknown settings field"))?;
        if let SettingValue::StringMap(new) = &new
            && matches!(id, "theme.colors" | "log.highlight_rules" | "file_types")
        {
            let old = if id == "file_types" {
                self.file_types.clone()
            } else {
                let SettingValue::StringMap(old) = spec.current(self.settings()) else { unreachable!() };
                old
            };
            let mut candidate = self.clone();
            for key in old.keys().filter(|key| !new.contains_key(*key)) {
                if id == "file_types" {
                    candidate.map_extension(key, None)?;
                } else {
                    candidate.edit_map_entry(id, key, None)?;
                }
            }
            for (key, value) in new {
                if old.get(key) == Some(value) {
                    continue;
                }
                if id == "file_types" {
                    candidate.map_extension(key, Some(value))?;
                } else {
                    candidate.edit_map_entry(id, key, Some(value))?;
                }
            }
            *self = candidate;
            return Ok(());
        }
        let valid = match (&spec.kind, &new) {
            (SettingKind::Bool, SettingValue::Bool(_))
            | (SettingKind::Integer, SettingValue::Integer(_))
            | (SettingKind::Float, SettingValue::Float(_))
            | (SettingKind::Text, SettingValue::Text(_))
            | (SettingKind::Strings, SettingValue::Strings(_)) => true,
            (SettingKind::Choice(options), SettingValue::Text(v)) => {
                options.contains(&v.as_str()) || (id == "theme.name" && crate::theme::ThemeId::user(v).is_ok())
            }
            _ => false,
        };
        if !valid {
            return Err(error(format!("{id}: incorrect value type or unsupported choice")));
        }
        let path: Vec<_> = id.split('.').collect();
        let new = value(new)?;
        self.transaction(|doc| set_leaf(doc.as_item_mut(), &path, Some(new)))
    }
    /// Reset writes the schema default while retaining the field's comments.
    /// Compound maps are edited entrywise; refusing a destructive reset is explicit.
    pub fn reset(&mut self, id: &str) -> Result<(), SettingsError> {
        if id == "bindings" || id == "formatters" {
            let comments = self.source.contains('#');
            let unknown = self
                .report
                .unknown_keys
                .iter()
                .any(|k| if id == "bindings" { k.starts_with("bindings[") } else { k.starts_with("formatters.") });
            return self.transaction(|doc| {
                if let Some(item) = doc.get(id) {
                    // Lossless removal cannot infer what owner annotations mean.
                    // Refuse comments and unknown nested fields explicitly.
                    let source = item.to_string();
                    if source.contains('#') || comments || unknown {
                        return Err(error(
                            "compound reset would discard comments or unknown fields; edit the settings file",
                        ));
                    }
                }
                doc.remove(id);
                Ok(())
            });
        }
        let default = SCHEMA.iter().find(|s| s.id == id).ok_or_else(|| error("unknown setting"))?.default_value();
        self.edit(id, default)
    }
    pub fn edit_map_entry(&mut self, id: &str, key: &str, new: Option<&str>) -> Result<(), SettingsError> {
        if !matches!(id, "theme.colors" | "log.highlight_rules") || key.is_empty() {
            return Err(error("unknown map or empty key"));
        }
        let mut path: Vec<_> = id.split('.').collect();
        path.push(key);
        self.transaction(|doc| set_leaf(doc.as_item_mut(), &path, new.map(Value::from)))
    }
    pub fn map_extension(&mut self, extension: &str, language: Option<&str>) -> Result<(), SettingsError> {
        let extension = normalize_extension(extension)?;
        if language.is_some_and(|s| !valid_language(s)) {
            return Err(error("language must be a nonempty identifier"));
        }
        // Keep the owner's spelling and comments if this mapping already exists.
        let key = self
            .document
            .get("file_types")
            .and_then(Item::as_table_like)
            .and_then(|t| {
                t.iter()
                    .find(|(k, _)| normalize_extension(k).ok().as_ref() == Some(&extension))
                    .map(|(k, _)| k.to_owned())
            })
            .unwrap_or(format!("*.{extension}"));
        self.transaction(|doc| set_leaf(doc.as_item_mut(), &["file_types", &key], language.map(Value::from)))
    }
    /// Dynamic formatter fields use the same schema validation as Settings.
    pub fn edit_formatter(&mut self, language: &str, field: &str, new: SettingValue) -> Result<(), SettingsError> {
        if !valid_language(language)
            || !matches!(
                (field, &new),
                ("executable", SettingValue::Text(_))
                    | ("args", SettingValue::Strings(_))
                    | ("timeout_ms", SettingValue::Integer(_))
            )
        {
            return Err(error("unsupported formatter field or value type"));
        }
        let new = value(new)?;
        self.transaction(|doc| set_leaf(doc.as_item_mut(), &["formatters", language, field], Some(new)))
    }
    pub fn conflicts(&self, candidate: &Binding) -> Result<Vec<KeyConflict>, SettingsError> {
        if !candidate.command.is_empty() && crate::command(&candidate.command).is_none() {
            return Err(error("unknown command"));
        }
        let sequence = candidate.keys.parse::<KeySequence>().map_err(|e| error(e.to_string()))?;
        let map = self.settings().keymap()?;
        let mut conflicts = Vec::new();
        for binding in map.bindings() {
            if binding.command.is_empty() || binding.command == candidate.command {
                continue;
            }
            let other = binding.keys.parse::<KeySequence>().map_err(|e| error(e.to_string()))?;
            let kind = if sequence == other {
                ConflictKind::Exact
            } else if sequence.0.starts_with(&other.0) || other.0.starts_with(&sequence.0) {
                ConflictKind::Prefix
            } else {
                continue;
            };
            let active = [Context::Editor, Context::Palette, Context::Hex, Context::Diff].into_iter().any(|context| {
                [Mode::Normal, Mode::Insert, Mode::Visual, Mode::VisualLine, Mode::Command].into_iter().any(|mode| {
                    candidate.context.is_none_or(|v| v == context)
                        && candidate.mode.is_none_or(|v| v == mode)
                        && binding.context.is_none_or(|v| v == context)
                        && binding.mode.is_none_or(|v| v == mode)
                        && map.resolve(&other.0, context, mode) == Resolution::Command(&binding.command)
                })
            });
            if active {
                conflicts.push(KeyConflict { keys: binding.keys.clone(), command: binding.command.clone(), kind });
            }
        }
        for warning in self.vim_grammar_warnings(candidate)? {
            let command = match warning.takeover {
                VimTakeover::Command(id) => id.to_owned(),
                VimTakeover::Pending => format!("Vim {} mode pending grammar", mode_name(warning.mode)),
            };
            let conflict = KeyConflict { keys: warning.keys, command, kind: warning.kind };
            if !conflicts.contains(&conflict) {
                conflicts.push(conflict);
            }
        }
        Ok(conflicts)
    }
    /// Probe a fresh Normal, Visual or VisualLine parser, at most eight chords
    /// per mode. No document, active parser, timers or I/O are touched. This
    /// previews grammar takeovers (including pending operators/counts/prefixes),
    /// not every possible state after arbitrary earlier Vim input.
    /// Explicit bindings and applicable tombstones are resolved before grammar,
    /// following KeymapEngine's continuation/restart rules. Rebinding a key to
    /// an equivalent complete native invocation (including count, motion,
    /// register, arguments and resulting mode) does not warn.
    pub fn vim_grammar_warnings(&self, candidate: &Binding) -> Result<Vec<VimGrammarWarning>, SettingsError> {
        if !candidate.command.is_empty() && crate::command(&candidate.command).is_none() {
            return Err(error("unknown command"));
        }
        if candidate.keys.split_whitespace().take(MAX_REBIND_CHORDS + 1).count() > MAX_REBIND_CHORDS {
            return Err(error("Vim conflict preview supports at most eight chords"));
        }
        let sequence = candidate.keys.parse::<KeySequence>().map_err(|e| error(e.to_string()))?;
        if self.settings().keymap != crate::Preset::Vim
            || candidate.command.is_empty()
            || candidate.context.is_some_and(|c| c != Context::Editor)
        {
            return Ok(Vec::new());
        }
        let map = self.settings().keymap()?;
        Ok([Mode::Normal, Mode::Visual, Mode::VisualLine]
            .into_iter()
            .filter(|mode| candidate.mode.is_none_or(|m| m == *mode))
            .filter_map(|mode| probe_vim_grammar(&map, &sequence, candidate, mode))
            .collect())
    }
    /// Includes every registered command, including actions with no shortcut.
    pub fn keymap_rows(&self) -> Result<Vec<CommandBindingRow>, SettingsError> {
        let current = self.settings().keymap()?;
        let preset = crate::Keymap::preset(self.settings().keymap).map_err(|e| error(e.to_string()))?;
        let effective = |map: &crate::Keymap, id: &str| {
            map.bindings().filter(|b| b.command == id && binding_active(map, b, None, None)).cloned().collect()
        };
        Ok(crate::COMMANDS
            .iter()
            .map(|command| CommandBindingRow {
                command,
                current: effective(&current, command.id),
                defaults: effective(&preset, command.id),
            })
            .collect())
    }
    /// Restore one command's preset keys and remove its active custom keys.
    /// Other commands occupying a preset key require explicit conflict choice.
    pub fn reset_command(&mut self, id: &str, replace_conflicts: bool) -> Result<(), SettingsError> {
        let row =
            self.keymap_rows()?.into_iter().find(|row| row.command.id == id).ok_or_else(|| error("unknown command"))?;
        if !replace_conflicts {
            for binding in &row.defaults {
                if !self.conflicts(binding)?.is_empty() {
                    return Err(error("preset shortcut conflicts with another command"));
                }
            }
        }
        let mut changes: Vec<_> = row
            .current
            .into_iter()
            .filter(|b| {
                !row.defaults.iter().any(|d| {
                    d.context.is_none_or(|c| b.context == Some(c))
                        && d.mode.is_none_or(|m| b.mode == Some(m))
                        && b.keys.parse::<KeySequence>().ok() == d.keys.parse::<KeySequence>().ok()
                })
            })
            .map(|mut b| {
                b.command.clear();
                b
            })
            .collect();
        // A scoped user tombstone outranks a restored global binding within
        // the same layer. Restore at that selector too, without deleting its
        // original table (which may carry unknown fields or owner comments).
        for default in &row.defaults {
            for custom in &self.settings().bindings {
                if custom.command.is_empty()
                    && custom.keys.parse::<KeySequence>().ok() == default.keys.parse::<KeySequence>().ok()
                    && custom.context.zip(default.context).is_none_or(|(a, b)| a == b)
                    && custom.mode.zip(default.mode).is_none_or(|(a, b)| a == b)
                {
                    let mut restored = default.clone();
                    restored.context = custom.context.or(default.context);
                    restored.mode = custom.mode.or(default.mode);
                    changes.push(restored);
                }
            }
        }
        let defaults = row.defaults;
        changes.extend(defaults.iter().cloned());
        let mut edited = self.clone();
        edited.append_bindings(changes)?;
        for binding in &defaults {
            edited.ensure_binding_reachable(binding)?;
        }
        *self = edited;
        Ok(())
    }
    /// Rebind moves this command within the selected context/mode and emits
    /// tombstones for its former effective keys. Conflicts need explicit choice.
    pub fn rebind(&mut self, binding: Binding, replace_conflicts: bool) -> Result<(), SettingsError> {
        let conflicts = self.conflicts(&binding)?;
        if !replace_conflicts && !conflicts.is_empty() {
            return Err(error(format!(
                "key conflict: {}",
                conflicts.iter().map(|c| c.command.as_str()).collect::<Vec<_>>().join(", ")
            )));
        }
        let map = self.settings().keymap()?;
        let mut old: Vec<_> = map
            .bindings()
            .filter(|b| {
                !binding.command.is_empty()
                    && b.command == binding.command
                    && binding_active(&map, b, binding.context, binding.mode)
                    && b.keys.parse::<KeySequence>().ok() != binding.keys.parse::<KeySequence>().ok()
            })
            .cloned()
            .collect();
        for b in &mut old {
            b.command.clear();
            b.context = binding.context.or(b.context);
            b.mode = binding.mode.or(b.mode);
        }
        old.push(binding.clone());
        let mut edited = self.clone();
        edited.append_bindings(old)?;
        edited.ensure_binding_reachable(&binding)?;
        *self = edited;
        Ok(())
    }
    /// Immediate short bindings and more-specific selectors may keep winning
    /// after a global override. Refuse the transaction unless the requested
    /// command is actually reachable throughout its requested scope.
    fn ensure_binding_reachable(&self, binding: &Binding) -> Result<(), SettingsError> {
        if binding.command.is_empty() {
            return Ok(());
        }
        let map = self.settings().keymap()?;
        let sequence = binding.keys.parse::<KeySequence>().map_err(|e| error(e.to_string()))?;
        for context in [Context::Editor, Context::Palette, Context::Hex, Context::Diff] {
            for mode in [Mode::Normal, Mode::Insert, Mode::Visual, Mode::VisualLine, Mode::Command] {
                if binding.context.is_some_and(|v| v != context) || binding.mode.is_some_and(|v| v != mode) {
                    continue;
                }
                let prefix_blocked = (1..sequence.0.len())
                    .any(|len| map.resolve(&sequence.0[..len], context, mode) != Resolution::Pending);
                if prefix_blocked || map.resolve(&sequence.0, context, mode) != Resolution::Command(&binding.command) {
                    return Err(error(format!(
                        "unreachable shortcut in {} / {}: remove the immediate prefix or more-specific binding first",
                        context_name(context),
                        mode_name(mode)
                    )));
                }
            }
        }
        Ok(())
    }
    fn append_bindings(&mut self, bindings: Vec<Binding>) -> Result<(), SettingsError> {
        self.transaction(|doc| {
            // Editing array/inline bindings in place is intentionally refused:
            // replacing those values would discard owner comments/unknown fields.
            if doc.get("bindings").is_some_and(|v| !v.is_array_of_tables()) {
                return Err(error("unsupported bindings representation: use [[bindings]] to preserve comments"));
            }
            if doc.get("bindings").is_none() {
                doc["bindings"] = Item::ArrayOfTables(Default::default());
            }
            let tables = doc["bindings"].as_array_of_tables_mut().expect("checked array of tables");
            for b in bindings {
                // Update only the command in the last matching selector, keeping
                // key spelling, comments and unknown fields. New selectors append.
                let sequence = b.keys.parse::<KeySequence>().map_err(|e| error(e.to_string()))?;
                let existing = (0..tables.len()).rev().find(|&i| {
                    let table = tables.get(i).expect("binding table");
                    table.get("keys").and_then(Item::as_str).and_then(|s| s.parse::<KeySequence>().ok()).as_ref()
                        == Some(&sequence)
                        && table.get("context").and_then(Item::as_str) == b.context.map(context_name)
                        && table.get("mode").and_then(Item::as_str) == b.mode.map(mode_name)
                });
                if let Some(i) = existing {
                    let table = tables.get_mut(i).expect("binding table");
                    let mut command = Value::from(b.command);
                    if let Some(old) = table.get("command").and_then(Item::as_value) {
                        *command.decor_mut() = old.decor().clone();
                    }
                    *table.get_mut("command").expect("validated command field") = Item::Value(command);
                    continue;
                }
                let mut table = Table::new();
                table["keys"] = toml_edit::value(b.keys);
                table["command"] = toml_edit::value(b.command);
                if let Some(c) = b.context {
                    table["context"] = toml_edit::value(context_name(c));
                }
                if let Some(m) = b.mode {
                    table["mode"] = toml_edit::value(mode_name(m));
                }
                tables.push(table);
            }
            Ok(())
        })
    }
}

fn binding_active(map: &crate::Keymap, b: &Binding, context: Option<Context>, mode: Option<Mode>) -> bool {
    let Ok(keys) = b.keys.parse::<KeySequence>() else {
        return false;
    };
    [Context::Editor, Context::Palette, Context::Hex, Context::Diff].into_iter().any(|c| {
        [Mode::Normal, Mode::Insert, Mode::Visual, Mode::VisualLine, Mode::Command].into_iter().any(|m| {
            context.is_none_or(|v| v == c)
                && mode.is_none_or(|v| v == m)
                && b.context.is_none_or(|v| v == c)
                && b.mode.is_none_or(|v| v == m)
                && map.resolve(&keys.0, c, m) == Resolution::Command(&b.command)
        })
    })
}

fn context_name(c: Context) -> &'static str {
    match c {
        Context::Editor => "editor",
        Context::Palette => "palette",
        Context::Hex => "hex",
        Context::Diff => "diff",
    }
}
fn mode_name(m: Mode) -> &'static str {
    match m {
        Mode::Normal => "normal",
        Mode::Insert => "insert",
        Mode::Visual => "visual",
        Mode::VisualLine => "visualLine",
        Mode::Command => "command",
    }
}
fn set_leaf(item: &mut Item, path: &[&str], new: Option<Value>) -> Result<(), SettingsError> {
    let table = item.as_table_like_mut().ok_or_else(|| error("unsupported TOML shape; edit settings file directly"))?;
    let key = path[0];
    if path.len() == 1 {
        if let Some(mut new) = new {
            if let Some(old) = table.get(key) {
                if let Some(old) = old.as_value() {
                    if old.is_array() && old.to_string().contains('#') {
                        return Err(error("replacing this array could discard comments; edit the settings file"));
                    }
                    *new.decor_mut() = old.decor().clone();
                } else {
                    return Err(error("refusing to replace a table with a scalar"));
                }
            }
            // TableLike::insert reformats an occupied key and clears its
            // decoration. Replace only the value to retain preceding comments
            // and the owner's quoted/dotted key spelling.
            if let Some(slot) = table.get_mut(key) {
                *slot = Item::Value(new);
            } else {
                table.insert(key, Item::Value(new));
            }
        } else if table.get(key).is_some() {
            // Removing an explicitly chosen map entry is intentional; preserve
            // comment text by refusing entries with attached comments.
            if table.key(key).is_some_and(|k| decor_has_comment(k.leaf_decor()) || decor_has_comment(k.dotted_decor()))
                || table.get(key).and_then(Item::as_value).is_some_and(|v| {
                    v.decor().prefix().and_then(|r| r.as_str()).is_some_and(|s| s.contains('#'))
                        || v.decor().suffix().and_then(|r| r.as_str()).is_some_and(|s| s.contains('#'))
                })
            {
                return Err(error("cannot remove a commented entry losslessly; edit the settings file"));
            }
            table.remove(key);
        }
    } else {
        if table.get(key).is_none() {
            if new.is_none() {
                return Ok(());
            }
            table.insert(key, Item::Table(Table::new()));
        }
        set_leaf(table.get_mut(key).expect("inserted parent"), &path[1..], new)?;
    }
    Ok(())
}
fn decor_has_comment(decor: &toml_edit::Decor) -> bool {
    [decor.prefix(), decor.suffix()].into_iter().flatten().any(|raw| raw.as_str().is_some_and(|s| s.contains('#')))
}
pub fn normalize_extension(extension: &str) -> Result<String, SettingsError> {
    let ext = extension.strip_prefix("*.").or_else(|| extension.strip_prefix('.')).unwrap_or(extension);
    if ext.is_empty()
        || ext.len() > 64
        || !ext.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'+'))
    {
        return Err(error("extension must contain 1 to 64 letters, digits, _, - or +"));
    }
    Ok(ext.to_ascii_lowercase())
}
fn valid_language(language: &str) -> bool {
    !language.is_empty()
        && language.len() <= 128
        && language.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'+' | b'.' | b'#'))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictKind {
    Exact,
    Prefix,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VimTakeover {
    Command(&'static str),
    Pending,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VimGrammarWarning {
    pub mode: Mode,
    /// Canonical consumed prefix, which may be shorter than the candidate.
    pub keys: String,
    pub consumed_chords: usize,
    pub kind: ConflictKind,
    pub takeover: VimTakeover,
}
fn probe_vim_grammar(
    map: &crate::Keymap,
    sequence: &KeySequence,
    candidate: &Binding,
    mode: Mode,
) -> Option<VimGrammarWarning> {
    use crate::vim::{Vim, VimResult};
    let mut vim = Vim::default();
    match mode {
        Mode::Visual => {
            vim.invoke("mode.visual");
        }
        Mode::VisualLine => {
            vim.invoke("mode.visualLine");
        }
        _ => {}
    }
    let mut explicit = vim.clone();
    let warning = |consumed: usize, takeover: VimTakeover| VimGrammarWarning {
        mode,
        keys: sequence.0[..consumed].iter().map(chord_name).collect::<Vec<_>>().join(" "),
        consumed_chords: consumed,
        kind: if takeover == VimTakeover::Pending || consumed < sequence.0.len() {
            ConflictKind::Prefix
        } else {
            ConflictKind::Exact
        },
        takeover,
    };
    let mut pending = Vec::new();
    let mut grammar_prefix = None;
    for (index, &key) in sequence.0.iter().enumerate() {
        pending.push(key);
        let mut resolution = map.resolve(&pending, Context::Editor, vim.mode());
        if resolution == Resolution::Unbound
            && pending.len() > 1
            && !map.explicitly_unbound(&pending, Context::Editor, vim.mode())
        {
            pending.clear();
            pending.push(key);
            resolution = map.resolve(&pending, Context::Editor, vim.mode());
        }
        match resolution {
            Resolution::Command(_) => return grammar_prefix.map(|n| warning(n, VimTakeover::Pending)),
            Resolution::Pending => continue,
            Resolution::Unbound => {
                if map.explicitly_unbound(&pending, Context::Editor, vim.mode()) {
                    return grammar_prefix.map(|n| warning(n, VimTakeover::Pending));
                }
                pending.clear();
            }
        }
        match vim.feed(key) {
            VimResult::Command(call) => {
                let consumed = index + 1;
                // Explicit dispatch starts from the fresh mode, not the
                // grammar state accumulated by these chords. Matching IDs
                // alone would hide dw's motion or 2j's count, for example.
                let equivalent = call.id == candidate.command
                    && consumed == sequence.0.len()
                    && explicit.invoke(call.id) == VimResult::Command(call.clone())
                    && explicit.mode() == vim.mode();
                return if equivalent { None } else { Some(warning(consumed, VimTakeover::Command(call.id))) };
            }
            VimResult::Pending => {
                grammar_prefix = Some(index + 1);
            }
            VimResult::Unbound | VimResult::Error(_) => {
                return grammar_prefix.map(|n| warning(n, VimTakeover::Pending));
            }
        }
    }
    grammar_prefix.map(|n| warning(n, VimTakeover::Pending))
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyConflict {
    pub keys: String,
    pub command: String,
    pub kind: ConflictKind,
}
#[derive(Clone, Debug, Default)]
pub struct KeyCapture {
    keys: Vec<KeyChord>,
}
impl KeyCapture {
    pub fn press(&mut self, key: KeyChord) -> Result<(), SettingsError> {
        if key.key == Key::Named(NamedKey::Escape) && key.modifiers == Default::default() {
            self.keys.clear();
            return Ok(());
        }
        if self.keys.len() >= 8 {
            return Err(error("key sequence is limited to eight chords"));
        }
        self.keys.push(key);
        Ok(())
    }
    pub fn cancel(&mut self) {
        self.keys.clear();
    }
    pub fn finish(&self) -> Result<String, SettingsError> {
        if self.keys.is_empty() {
            return Err(error("no captured keys"));
        }
        Ok(self.keys.iter().map(chord_name).collect::<Vec<_>>().join(" "))
    }
}
/// Canonical settings spelling of a parsed logical chord. Displaying this
/// value avoids retaining unbounded aliases such as zero-padded function keys.
pub fn chord_name(key: &KeyChord) -> String {
    let mut s = String::new();
    for (enabled, name) in [
        (key.modifiers.ctrl, "ctrl+"),
        (key.modifiers.alt, "alt+"),
        (key.modifiers.shift, "shift+"),
        (key.modifiers.super_key, "super+"),
    ] {
        if enabled {
            s.push_str(name);
        }
    }
    s.push_str(&match key.key {
        Key::Character(' ') => "space".into(),
        Key::Character('+') => "plus".into(),
        Key::Character(c) => c.to_string(),
        Key::Function(n) => format!("f{n}"),
        Key::Named(n) => match n {
            NamedKey::Enter => "enter",
            NamedKey::Escape => "escape",
            NamedKey::Tab => "tab",
            NamedKey::Backspace => "backspace",
            NamedKey::Delete => "delete",
            NamedKey::Insert => "insert",
            NamedKey::Home => "home",
            NamedKey::End => "end",
            NamedKey::PageUp => "pageup",
            NamedKey::PageDown => "pagedown",
            NamedKey::Left => "left",
            NamedKey::Right => "right",
            NamedKey::Up => "up",
            NamedKey::Down => "down",
        }
        .into(),
    });
    s
}
#[derive(Clone, Debug)]
pub struct SettingsRow {
    pub spec: &'static SettingSpec,
    pub current: SettingValue,
    pub default: SettingValue,
}
pub struct SettingsMenu {
    document: SettingsDocument,
    query: String,
    category: Option<Category>,
    selection: usize,
    visible: Vec<usize>,
}
impl SettingsMenu {
    pub fn new(document: SettingsDocument) -> Self {
        let mut s = Self { document, query: String::new(), category: None, selection: 0, visible: Vec::new() };
        s.filter();
        s
    }
    pub fn categories(&self) -> &'static [Category] {
        CATEGORIES
    }
    pub fn document(&self) -> &SettingsDocument {
        &self.document
    }
    pub fn document_mut(&mut self) -> &mut SettingsDocument {
        &mut self.document
    }
    pub fn replace_document(&mut self, document: SettingsDocument) {
        self.document = document;
        self.filter();
    }
    pub fn search(&mut self, query: &str) {
        self.query = query.to_lowercase();
        self.selection = 0;
        self.filter();
    }
    pub fn select_category(&mut self, category: Option<Category>) {
        self.category = category;
        self.selection = 0;
        self.filter();
    }
    pub fn move_selection(&mut self, delta: isize) {
        self.selection = self.selection.saturating_add_signed(delta).min(self.visible.len().saturating_sub(1));
    }
    fn row(&self, index: usize) -> SettingsRow {
        let spec = &SCHEMA[index];
        SettingsRow {
            spec,
            current: if spec.id == "file_types" {
                SettingValue::StringMap(self.document.file_types.clone())
            } else {
                spec.current(self.document.settings())
            },
            default: spec.default_value(),
        }
    }
    pub fn rows(&self) -> impl Iterator<Item = SettingsRow> + '_ {
        self.visible.iter().map(|i| self.row(*i))
    }
    pub fn selected(&self) -> Option<SettingsRow> {
        self.visible.get(self.selection).map(|i| self.row(*i))
    }
    pub fn edit_selected(&mut self, value: SettingValue) -> Result<(), SettingsError> {
        let id = self.selected().ok_or_else(|| error("no selected setting"))?.spec.id;
        self.document.edit(id, value)
    }
    pub fn reset_selected(&mut self) -> Result<(), SettingsError> {
        let id = self.selected().ok_or_else(|| error("no selected setting"))?.spec.id;
        self.document.reset(id)
    }
    fn filter(&mut self) {
        self.visible = SCHEMA
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                self.category.is_none_or(|c| c == s.category)
                    && self.query.split_whitespace().all(|q| {
                        format!("{} {} {} {}", s.id, s.title, s.description, s.category.title())
                            .to_lowercase()
                            .contains(q)
                    })
            })
            .map(|(i, _)| i)
            .collect();
        self.selection = self.selection.min(self.visible.len().saturating_sub(1));
    }
}
