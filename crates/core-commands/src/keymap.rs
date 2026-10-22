use crate::{KeyChord, KeySequence, command};
use serde::Deserialize;
use std::{fmt, str::FromStr};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Context {
    #[default]
    Editor,
    Palette,
    Hex,
    Diff,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Mode {
    #[default]
    Normal,
    Insert,
    Visual,
    VisualLine,
    Command,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
pub enum Preset {
    #[default]
    #[serde(rename = "vscode")]
    VsCode,
    #[serde(rename = "notepad++")]
    NotepadPlusPlus,
    #[serde(rename = "vim")]
    Vim,
}

impl Preset {
    pub fn source(self) -> &'static str {
        match self {
            Self::VsCode => include_str!("../presets/vscode.toml"),
            Self::NotepadPlusPlus => include_str!("../presets/notepad++.toml"),
            Self::Vim => include_str!("../presets/vim.toml"),
        }
    }
}

/// `command = ""` is an explicit unbind (a tombstone, including inherited
/// global bindings). To move a command, unbind its old key and add its new key.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Binding {
    pub keys: String,
    pub command: String,
    pub context: Option<Context>,
    pub mode: Option<Mode>,
}

#[derive(Clone, Debug)]
struct Entry {
    sequence: KeySequence,
    binding: Binding,
    layer: usize,
}

#[derive(Clone, Debug, Default)]
pub struct Keymap {
    entries: Vec<Entry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeymapError(pub String);
impl fmt::Display for KeymapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for KeymapError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution<'a> {
    Command(&'a str),
    Pending,
    Unbound,
}

impl Keymap {
    pub fn preset(preset: Preset) -> Result<Self, KeymapError> {
        Self::parse(preset.source())
    }

    pub fn parse(source: &str) -> Result<Self, KeymapError> {
        #[derive(Deserialize)]
        struct Document {
            #[serde(default)]
            bindings: Vec<Binding>,
        }
        let document: Document = toml::from_str(source).map_err(|e| KeymapError(e.to_string()))?;
        Self::default().with_overrides(&document.bindings)
    }

    /// Transactional: an invalid override never leaves a partially changed map.
    pub fn with_overrides(&self, bindings: &[Binding]) -> Result<Self, KeymapError> {
        let mut result = self.clone();
        let layer = self.entries.iter().map(|entry| entry.layer).max().unwrap_or(0) + 1;
        for binding in bindings {
            if !binding.command.is_empty() && command(&binding.command).is_none() {
                return Err(KeymapError(format!("unknown command: {}", binding.command)));
            }
            let sequence = KeySequence::from_str(&binding.keys).map_err(|e| KeymapError(e.to_string()))?;
            result.entries.retain(|entry| {
                !(entry.sequence == sequence
                    && entry.binding.context == binding.context
                    && entry.binding.mode == binding.mode)
            });
            result.entries.push(Entry { sequence, binding: binding.clone(), layer });
        }
        Ok(result)
    }

    pub fn bindings(&self) -> impl Iterator<Item = &Binding> {
        self.entries.iter().map(|entry| &entry.binding)
    }

    fn applicable(entry: &Entry, context: Context, mode: Mode) -> bool {
        entry.binding.context.is_none_or(|c| c == context) && entry.binding.mode.is_none_or(|m| m == mode)
    }

    // User layers win over presets, then context wins over mode within a layer.
    // Pick once per full sequence, including unbind tombstones.
    fn winner(&self, sequence: &[KeyChord], context: Context, mode: Mode) -> Option<&Entry> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.sequence.0 == sequence && Self::applicable(e, context, mode))
            .max_by_key(|(i, e)| (e.layer, e.binding.context.is_some(), e.binding.mode.is_some(), *i))
            .map(|(_, e)| e)
    }

    /// Exact bindings win immediately. A proper prefix returns Pending only
    /// when a longer live binding exists in this context/mode. No timeout is
    /// owned here; callers clear their sequence on focus/mode changes or Escape.
    /// Prefix conflicts therefore favor the short command; unbind it to expose
    /// longer sequences. This rule avoids timers and delayed ordinary commands.
    pub fn resolve(&self, sequence: &[KeyChord], context: Context, mode: Mode) -> Resolution<'_> {
        if sequence.is_empty() {
            return Resolution::Unbound;
        }
        if let Some(entry) = self.winner(sequence, context, mode)
            && !entry.binding.command.is_empty()
        {
            return Resolution::Command(&entry.binding.command);
        }
        if self.entries.iter().any(|entry| {
            entry.sequence.0.len() > sequence.len()
                && entry.sequence.0.starts_with(sequence)
                && self.winner(&entry.sequence.0, context, mode).is_some_and(|e| !e.binding.command.is_empty())
        }) {
            Resolution::Pending
        } else {
            Resolution::Unbound
        }
    }

    /// Whether a matching explicit unbind suppresses the Vim grammar too.
    pub fn explicitly_unbound(&self, sequence: &[KeyChord], context: Context, mode: Mode) -> bool {
        self.winner(sequence, context, mode).is_some_and(|e| e.binding.command.is_empty())
    }
}

/// Stateful dispatch shared by GUI and TUI. Context changes cancel incomplete
/// sequences/operators. Vim grammar runs only in Editor, after explicit
/// bindings/overrides. Unhandled Insert keys and IME commits belong to text
/// input in the frontend. Reload by replacing the engine from valid settings.
pub struct KeymapEngine {
    keymap: Keymap,
    vim: Option<crate::vim::Vim>,
    context: Context,
    pending: Vec<KeyChord>,
}
impl KeymapEngine {
    pub fn new(preset: Preset, overrides: &[Binding]) -> Result<Self, KeymapError> {
        Ok(Self {
            keymap: Keymap::preset(preset)?.with_overrides(overrides)?,
            vim: (preset == Preset::Vim).then(crate::vim::Vim::default),
            context: Context::Editor,
            pending: Vec::new(),
        })
    }
    pub fn mode(&self) -> Mode {
        self.vim.as_ref().map_or(Mode::Insert, crate::vim::Vim::mode)
    }
    pub fn vim(&self) -> Option<&crate::vim::Vim> {
        self.vim.as_ref()
    }
    pub fn set_context(&mut self, context: Context) {
        if self.context != context {
            self.context = context;
            self.cancel_pending();
        }
    }
    pub fn cancel_pending(&mut self) {
        self.pending.clear();
        if let Some(vim) = &mut self.vim {
            vim.cancel_input();
        }
    }
    pub fn feed(&mut self, key: KeyChord) -> crate::vim::VimResult {
        use crate::vim::{Argument, Invocation, VimResult};
        if key.key == crate::Key::Named(crate::NamedKey::Escape) {
            self.pending.clear();
        }
        self.pending.push(key);
        let mode = self.mode();
        let mut resolution = self.keymap.resolve(&self.pending, self.context, mode);
        if resolution == Resolution::Unbound && self.pending.len() > 1 {
            // An invalid continuation may itself start a shortcut. Never eat
            // an ordinary key after a failed multi-key sequence.
            self.pending.clear();
            self.pending.push(key);
            resolution = self.keymap.resolve(&self.pending, self.context, mode);
        }
        match resolution {
            Resolution::Pending => VimResult::Pending,
            Resolution::Command(id) => {
                let id = command(id).expect("validated keymap").id;
                self.pending.clear();
                if self.context == Context::Editor
                    && let Some(vim) = &mut self.vim
                {
                    return vim.invoke(id);
                }
                self.cancel_pending();
                VimResult::Command(Invocation {
                    id,
                    count: 1,
                    explicit_count: false,
                    motion: None,
                    register: None,
                    argument: Argument::None,
                })
            }
            Resolution::Unbound => {
                let blocked = self.keymap.explicitly_unbound(&self.pending, self.context, mode);
                self.pending.clear();
                if blocked {
                    self.cancel_pending();
                    return VimResult::Unbound;
                }
                if self.context == Context::Editor
                    && let Some(vim) = &mut self.vim
                {
                    return vim.feed(key);
                }
                VimResult::Unbound
            }
        }
    }
}
