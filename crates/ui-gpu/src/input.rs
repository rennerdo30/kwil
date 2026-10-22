//! Logical input translation. IME commits bypass shortcut interpretation.
use winit::keyboard::{Key, ModifiersState, NamedKey};

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Command(&'static str),
    Text(String),
}

pub fn translate(key: &Key, text: Option<&str>, mods: ModifiersState, composing: bool) -> Option<Action> {
    if composing {
        return None;
    }
    let ctrl = mods.control_key() || (cfg!(target_os = "macos") && mods.super_key());
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

#[cfg(test)]
mod tests {
    use super::*;
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
