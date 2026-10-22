//! Command registry, keymaps, settings (shared by both frontends).
//!
//! No frontend, document buffer, timer or worker is owned by this crate.
pub mod associations;
pub mod key;
pub mod keymap;
pub mod registry;
pub mod settings;
pub mod settings_ui;
pub mod vim;

pub use key::{Key, KeyChord, KeySequence, Modifiers, NamedKey};
pub use keymap::{Binding, Context, Keymap, Mode, Preset, Resolution};
pub use registry::{COMMANDS, Command, command};
pub use settings::Settings;
