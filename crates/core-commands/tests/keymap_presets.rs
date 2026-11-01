use kwil_core_commands::settings_ui::{SCHEMA, SettingValue, SettingsDocument};
use kwil_core_commands::{Context, KeySequence, Mode, Preset, Settings, command};
use std::collections::HashMap;

#[test]
fn nested_preset_setting_layers_overrides_and_unbinds() {
    for (name, preset) in [("vscode", Preset::VsCode), ("notepad++", Preset::NotepadPlusPlus), ("vim", Preset::Vim)] {
        let source = format!(
            "keymap.preset = '{name}'\n[[bindings]]\nkeys='ctrl+s'\ncommand=''\n[[bindings]]\nkeys='f6'\ncommand='file.save'\n"
        );
        let report = Settings::parse(&source).unwrap();
        assert!(report.unknown_keys.is_empty());
        assert_eq!(report.settings.keymap, preset);
        let map = report.settings.keymap().unwrap();
        assert_eq!(
            map.resolve(&"ctrl+s".parse::<KeySequence>().unwrap().0, Context::Editor, Mode::Normal),
            kwil_core_commands::Resolution::Unbound
        );
        assert_eq!(
            map.resolve(&"f6".parse::<KeySequence>().unwrap().0, Context::Editor, Mode::Normal),
            kwil_core_commands::Resolution::Command("file.save")
        );
    }
}

#[test]
fn preset_setting_edits_legacy_and_nested_documents_losslessly() {
    assert!(SCHEMA.iter().any(|s| s.id == "keymap.preset"));
    for source in [
        "# owner\nkeymap = 'vim' # shortcut preference\n",
        "# owner\n[keymap]\npreset='vim' # shortcut preference\nfuture=42\n",
    ] {
        let mut doc = SettingsDocument::parse(source).unwrap();
        doc.edit("keymap.preset", SettingValue::Text("notepad++".into())).unwrap();
        assert_eq!(doc.settings().keymap, Preset::NotepadPlusPlus);
        assert!(doc.source().contains("# owner"));
        assert!(doc.source().contains("# shortcut preference"));
        if source.contains("future") {
            assert!(doc.source().contains("future=42"));
        }
        doc.reset("keymap.preset").unwrap();
        assert_eq!(doc.settings().keymap, Preset::VsCode);
    }
}

#[test]
fn raw_presets_have_no_conflicting_keys_in_overlapping_modes_and_only_registered_commands() {
    #[derive(serde::Deserialize)]
    struct Raw {
        bindings: Vec<kwil_core_commands::Binding>,
    }
    for preset in [Preset::VsCode, Preset::NotepadPlusPlus, Preset::Vim] {
        let raw: Raw = toml::from_str(preset.source()).unwrap();
        for context in [Context::Editor, Context::Palette, Context::Hex, Context::Diff] {
            for mode in [Mode::Normal, Mode::Insert, Mode::Visual, Mode::VisualLine, Mode::Command] {
                let mut keys = HashMap::new();
                for b in &raw.bindings {
                    assert!(command(&b.command).is_some(), "{preset:?}: {}", b.command);
                    if b.context.is_none_or(|c| c == context) && b.mode.is_none_or(|m| m == mode) {
                        let sequence: KeySequence = b.keys.parse().unwrap();
                        if let Some(previous) = keys.insert(sequence, &b.command) {
                            assert_eq!(previous, &b.command, "{preset:?} {context:?} {mode:?}: {}", b.keys);
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn command_labels_follow_overrides_modes_and_unreachable_prefixes() {
    use kwil_core_commands::{Binding, Keymap};
    let bindings = [
        Binding { keys: "ctrl+s".into(), command: "".into(), context: None, mode: None },
        Binding { keys: "f6".into(), command: "file.save".into(), context: None, mode: Some(Mode::Insert) },
        Binding { keys: "f9".into(), command: "edit.copy".into(), context: None, mode: None },
        Binding { keys: "f9 f8".into(), command: "settings.keymapVim".into(), context: None, mode: None },
    ];
    let map = Keymap::preset(Preset::Vim).unwrap().with_overrides(&bindings).unwrap();
    assert!(map.command_binding("file.save", Context::Editor, Mode::Normal).is_none());
    assert_eq!(map.command_binding("file.save", Context::Editor, Mode::Insert).unwrap().keys, "f6");
    assert!(map.command_binding("settings.keymapVim", Context::Editor, Mode::Insert).is_none());
}
