//! Logical input translation. IME commits bypass shortcut interpretation.
use winit::keyboard::{Key, ModifiersState, NamedKey};

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Command(&'static str),
    Text(String),
}

pub fn commit(text: String, path: &mut Option<String>, insert_mode: bool) -> Option<Action> {
    if let Some(path) = path {
        path.push_str(&text);
        None
    } else if insert_mode {
        Some(Action::Text(text))
    } else {
        None
    }
}

pub fn translate(key: &Key, text: Option<&str>, mods: ModifiersState, composing: bool) -> Option<Action> {
    if composing {
        return None;
    }
    let ctrl = mods.control_key();
    let command = match key {
        Key::Character(c) if ctrl => match c.to_ascii_lowercase().as_str() {
            "z" if mods.shift_key() => "edit.redo",
            "z" => "edit.undo",
            "y" => "edit.redo",
            "a" => "edit.selectAll",
            "s" => "file.save",
            "c" => "edit.copy",
            "x" => "edit.cut",
            "v" => "edit.paste",
            _ => return None,
        },
        Key::Named(NamedKey::ArrowLeft) => {
            if ctrl {
                "motion.wordBackward"
            } else {
                "motion.left"
            }
        }
        Key::Named(NamedKey::ArrowRight) => {
            if ctrl {
                "motion.wordForward"
            } else {
                "motion.right"
            }
        }
        Key::Named(NamedKey::ArrowUp) => "motion.up",
        Key::Named(NamedKey::ArrowDown) => "motion.down",
        Key::Named(NamedKey::Home) => {
            if ctrl {
                "motion.firstLine"
            } else {
                "motion.smartHome"
            }
        }
        Key::Named(NamedKey::End) => {
            if ctrl {
                "motion.lastLine"
            } else {
                "motion.lineEnd"
            }
        }
        Key::Named(NamedKey::PageUp) => "motion.pageUp",
        Key::Named(NamedKey::PageDown) => "motion.pageDown",
        Key::Named(NamedKey::Backspace) => {
            if ctrl {
                "edit.deleteWordBackward"
            } else {
                "edit.backspace"
            }
        }
        Key::Named(NamedKey::Delete) => {
            if ctrl {
                "edit.deleteWordForward"
            } else {
                "edit.delete"
            }
        }
        Key::Named(NamedKey::Enter) if !ctrl && !mods.alt_key() => "edit.newline",
        Key::Named(NamedKey::Tab) if !ctrl && !mods.alt_key() => {
            if mods.shift_key() {
                "edit.outdent"
            } else {
                "edit.indent"
            }
        }
        _ if !ctrl && !mods.super_key() && !mods.alt_key() => {
            let text = text.filter(|text| !text.is_empty() && !text.chars().any(char::is_control))?;
            return Some(Action::Text(text.into()));
        }
        _ => return None,
    };
    Some(Action::Command(command))
}

pub fn chord(key: &Key, mods: ModifiersState) -> Option<kwil_core_commands::KeyChord> {
    use kwil_core_commands::{Key as K, KeyChord, Modifiers, NamedKey as N};
    let key = match key {
        Key::Character(text) => {
            let mut chars = text.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            K::Character(c)
        }
        Key::Named(key) if FUNCTION_KEYS.contains(key) => {
            K::Function((FUNCTION_KEYS.iter().position(|candidate| candidate == key)? + 1) as u8)
        }
        Key::Named(key) => K::Named(match key {
            NamedKey::Enter => N::Enter,
            NamedKey::Escape => N::Escape,
            NamedKey::Tab => N::Tab,
            NamedKey::Backspace => N::Backspace,
            NamedKey::Delete => N::Delete,
            NamedKey::Insert => N::Insert,
            NamedKey::Home => N::Home,
            NamedKey::End => N::End,
            NamedKey::PageUp => N::PageUp,
            NamedKey::PageDown => N::PageDown,
            NamedKey::ArrowLeft => N::Left,
            NamedKey::ArrowRight => N::Right,
            NamedKey::ArrowUp => N::Up,
            NamedKey::ArrowDown => N::Down,
            _ => return None,
        }),
        _ => return None,
    };
    Some(KeyChord::new(
        key,
        Modifiers {
            ctrl: mods.control_key(),
            alt: mods.alt_key(),
            shift: mods.shift_key(),
            super_key: mods.super_key(),
        },
    ))
}

const FUNCTION_KEYS: [NamedKey; 24] = [
    NamedKey::F1,
    NamedKey::F2,
    NamedKey::F3,
    NamedKey::F4,
    NamedKey::F5,
    NamedKey::F6,
    NamedKey::F7,
    NamedKey::F8,
    NamedKey::F9,
    NamedKey::F10,
    NamedKey::F11,
    NamedKey::F12,
    NamedKey::F13,
    NamedKey::F14,
    NamedKey::F15,
    NamedKey::F16,
    NamedKey::F17,
    NamedKey::F18,
    NamedKey::F19,
    NamedKey::F20,
    NamedKey::F21,
    NamedKey::F22,
    NamedKey::F23,
    NamedKey::F24,
];

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn function_chords_preserve_numbers_and_modifiers() {
        for (index, named) in FUNCTION_KEYS.into_iter().enumerate() {
            let chord = chord(&Key::Named(named), ModifiersState::CONTROL | ModifiersState::SHIFT).unwrap();
            assert_eq!(chord.key, kwil_core_commands::Key::Function(index as u8 + 1));
            assert!(chord.modifiers.ctrl && chord.modifiers.shift);
        }
        assert!(chord(&Key::Named(NamedKey::F25), ModifiersState::empty()).is_none());
    }
    #[test]
    fn ime_commit_targets_the_save_prompt_and_respects_normal_mode() {
        let mut path = Some("notes-".to_string());
        assert_eq!(commit("日本語.txt".into(), &mut path, true), None);
        assert_eq!(path.as_deref(), Some("notes-日本語.txt"));
        assert_eq!(commit("界".into(), &mut None, false), None);
        assert_eq!(commit("界".into(), &mut None, true), Some(Action::Text("界".into())));
    }
    #[test]
    fn printable_and_composed_text_are_never_shortcuts() {
        let key = Key::Character("é".into());
        assert_eq!(translate(&key, Some("é"), ModifiersState::empty(), false), Some(Action::Text("é".into())));
        assert_eq!(translate(&key, Some("é"), ModifiersState::empty(), true), None);
        assert_eq!(translate(&key, Some("é"), ModifiersState::CONTROL, false), None);
    }
    #[test]
    fn navigation_deletion_and_undo_are_edit_commands() {
        assert_eq!(
            translate(&Key::Named(NamedKey::Backspace), None, ModifiersState::CONTROL, false),
            Some(Action::Command("edit.deleteWordBackward"))
        );
        assert_eq!(
            translate(&Key::Character("z".into()), None, ModifiersState::CONTROL | ModifiersState::SHIFT, false),
            Some(Action::Command("edit.redo"))
        );
        assert_eq!(
            translate(&Key::Named(NamedKey::ArrowDown), None, ModifiersState::SHIFT, false),
            Some(Action::Command("motion.down"))
        );
    }
}
