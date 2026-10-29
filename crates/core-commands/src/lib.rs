//! Command registry, keymaps, settings (shared by both frontends).
//!
//! No frontend or document buffer is owned by this crate. The optional settings
//! worker performs I/O only after the host explicitly starts it.
pub mod associations;
pub mod key;
pub mod keymap;
pub mod palette;
pub mod registry;
pub mod settings;
pub mod settings_ui;
pub mod theme;
pub mod theme_cache;
mod theme_parse;
pub mod theme_worker;
pub mod vim;

pub use key::{Key, KeyChord, KeySequence, Modifiers, NamedKey};
pub use keymap::{Binding, Context, Keymap, Mode, Preset, Resolution};
pub use registry::{COMMANDS, Command, NOT_HOSTED, command, in_palette};
pub use settings::{Settings, TabLayout, TabsSettings};

pub mod file_types;
