use kwil_core_commands::{Binding, Preset, keymap::KeymapEngine, vim::VimResult};

#[test]
fn zoom_shortcuts_resolve_in_every_bundled_preset() {
    for preset in [Preset::VsCode, Preset::NotepadPlusPlus, Preset::Vim] {
        for (key, expected) in [
            ("ctrl+Plus", "view.zoomIn"),
            ("ctrl+shift+Plus", "view.zoomIn"),
            ("ctrl+=", "view.zoomIn"),
            ("ctrl+shift+=", "view.zoomIn"),
            ("ctrl+-", "view.zoomOut"),
            ("ctrl+0", "view.zoomReset"),
        ] {
            let mut keys = KeymapEngine::new(preset, &[]).unwrap();
            assert!(matches!(keys.feed(key.parse().unwrap()), VimResult::Command(call) if call.id == expected), "{preset:?}: {key}");
        }
    }
}

#[test]
fn zoom_shortcuts_respect_explicit_user_unbinds() {
    for preset in [Preset::VsCode, Preset::NotepadPlusPlus, Preset::Vim] {
        let overrides = [Binding { keys: "ctrl+Plus".into(), command: String::new(), context: None, mode: None }];
        let mut keys = KeymapEngine::new(preset, &overrides).unwrap();
        assert_eq!(keys.feed("ctrl+Plus".parse().unwrap()), VimResult::Unbound);
        assert!(keys.suppresses_fallback());
    }
}
