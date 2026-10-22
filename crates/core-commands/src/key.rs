//! Adapters use logical keys, not physical scan codes. For winit, map
//! `Key::Character` (one Unicode scalar only) / `NamedKey` plus ModifiersState;
//! for crossterm, map KeyCode::Char / named codes plus KeyModifiers. Pass press
//! events, optionally repeats, never releases. Text/IME commits go directly to
//! the editor, not through shortcut parsing. Ctrl and Super remain distinct;
//! macOS adapters must not silently turn Cmd into Ctrl.
use std::{fmt, str::FromStr};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub super_key: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NamedKey {
    Enter,
    Escape,
    Tab,
    Backspace,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    Character(char),
    Named(NamedKey),
    Function(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct KeyChord {
    pub key: Key,
    pub modifiers: Modifiers,
}

impl KeyChord {
    /// Canonicalizes adapter input: ASCII letters are lowercase plus Shift;
    /// all other logical characters preserve the supplied modifiers exactly.
    /// No keyboard-layout translation is inferred (Shift+/ is not '?').
    pub fn new(key: Key, mut modifiers: Modifiers) -> Self {
        let key = match key {
            Key::Character(c) if c.is_ascii_uppercase() => {
                modifiers.shift = true;
                Key::Character(c.to_ascii_lowercase())
            }
            key => key,
        };
        Self { key, modifiers }
    }

    /// Printable logical character, preserving the case of ASCII letters.
    pub fn character(self) -> Option<char> {
        if self.modifiers.ctrl || self.modifiers.alt || self.modifiers.super_key {
            return None;
        }
        match self.key {
            Key::Character(c) => Some(if self.modifiers.shift { c.to_ascii_uppercase() } else { c }),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyParseError(pub String);
impl fmt::Display for KeyParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for KeyParseError {}

impl FromStr for KeyChord {
    type Err = KeyParseError;
    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let input = input.trim();
        // Use Plus for an unambiguous literal '+' in settings.
        let parts: Vec<_> = input.split('+').collect();
        let mut modifiers = Modifiers::default();
        for part in &parts[..parts.len().saturating_sub(1)] {
            let flag = match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => &mut modifiers.ctrl,
                "alt" | "option" => &mut modifiers.alt,
                "shift" => &mut modifiers.shift,
                "super" | "cmd" | "command" | "meta" => &mut modifiers.super_key,
                _ => return Err(KeyParseError(format!("unknown modifier: {part}"))),
            };
            if *flag {
                return Err(KeyParseError(format!("duplicate modifier: {part}")));
            }
            *flag = true;
        }
        let name = parts.last().copied().unwrap_or_default();
        use NamedKey::*;
        let key = match name.to_ascii_lowercase().as_str() {
            "enter" | "return" => Key::Named(Enter),
            "esc" | "escape" => Key::Named(Escape),
            "tab" => Key::Named(Tab),
            "backspace" => Key::Named(Backspace),
            "delete" | "del" => Key::Named(Delete),
            "insert" | "ins" => Key::Named(Insert),
            "home" => Key::Named(Home),
            "end" => Key::Named(End),
            "pageup" => Key::Named(PageUp),
            "pagedown" => Key::Named(PageDown),
            "left" => Key::Named(Left),
            "right" => Key::Named(Right),
            "up" => Key::Named(Up),
            "down" => Key::Named(Down),
            "space" => Key::Character(' '),
            "plus" => Key::Character('+'),
            _ if name.chars().count() == 1 => Key::Character(name.chars().next().unwrap()),
            _ if name.starts_with(['f', 'F']) => {
                let n = name[1..]
                    .parse::<u8>()
                    .ok()
                    .filter(|n| (1..=24).contains(n))
                    .ok_or_else(|| KeyParseError(format!("invalid function key: {name}")))?;
                Key::Function(n)
            }
            _ => return Err(KeyParseError(format!("unknown key: {name}"))),
        };
        // Shortcut notation is case-insensitive once modifiers are written:
        // Ctrl+K means ctrl+k, while bare G means Shift+g for Vim.
        let key = match key {
            Key::Character(c) if parts.len() > 1 => Key::Character(c.to_ascii_lowercase()),
            key => key,
        };
        Ok(Self::new(key, modifiers))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct KeySequence(pub Vec<KeyChord>);
impl FromStr for KeySequence {
    type Err = KeyParseError;
    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let keys = input.split_whitespace().map(str::parse).collect::<Result<Vec<_>, _>>()?;
        if keys.is_empty() {
            return Err(KeyParseError("empty key sequence".into()));
        }
        Ok(Self(keys))
    }
}
