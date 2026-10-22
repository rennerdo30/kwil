use kwil_core_commands::{settings_ui::*, *};

#[test]
fn schema_search_navigation_current_default_and_reset() {
    let doc = SettingsDocument::parse("[editor]\ntab_size = 8 # keep\n").unwrap();
    let mut menu = SettingsMenu::new(doc);
    assert_eq!(menu.categories().len(), 8);
    menu.search("indent width");
    let row = menu.selected().unwrap();
    assert_eq!(row.spec.id, "editor.tab_size");
    assert_eq!(row.current, SettingValue::Integer(8));
    assert_eq!(row.default, SettingValue::Integer(4));
    menu.reset_selected().unwrap();
    assert_eq!(menu.document().settings().editor.tab_size, 4);
    assert!(menu.document().source().contains("# keep"));
    menu.search("");
    menu.select_category(Some(Category::Updates));
    menu.move_selection(1);
    assert_eq!(menu.selected().unwrap().spec.id, "update.channel");
}

#[test]
fn invalid_edit_does_not_change_document_or_unknown_fields() {
    let source = "# owner\nfuture = { enabled = true }\n[editor]\ntab_size = 7 # width\nfuture = 'leave'\n";
    let mut doc = SettingsDocument::parse(source).unwrap();
    assert!(doc.edit("editor.tab_size", SettingValue::Integer(0)).is_err());
    assert!(doc.edit("font.size", SettingValue::Float(f64::NAN)).is_err());
    assert!(doc.edit("editor.tab_size", SettingValue::Text("8".into())).is_err());
    assert_eq!(doc.source(), source);
    doc.edit("editor.tab_size", SettingValue::Integer(2)).unwrap();
    assert!(doc.source().contains("future = { enabled = true }"));
    assert!(doc.source().contains("future = 'leave'"));
    assert!(doc.source().contains("# owner"));
    assert!(doc.source().contains("# width"));
}

#[test]
fn extension_mapping_is_validated_lossless_and_case_insensitive() {
    let mut doc = SettingsDocument::parse("[file_types]\n'*.foo' = 'python' # custom\n").unwrap();
    assert_eq!(doc.language_for_extension(".FOO"), Some("python"));
    doc.map_extension(".BAR", Some("rust")).unwrap();
    assert_eq!(doc.language_for_extension("bar"), Some("rust"));
    assert!(doc.map_extension("../bad", Some("rust")).is_err());
    assert!(doc.map_extension("bar", Some("")).is_err());
    doc.map_extension("bar", None).unwrap();
    assert_eq!(doc.language_for_extension("bar"), None);
    assert!(doc.source().contains("# custom"));
}

#[test]
fn rebind_detects_exact_prefix_and_context_conflicts_and_keeps_tombstones() {
    let mut doc = SettingsDocument::parse("").unwrap();
    let save = Binding { keys: "ctrl+c".into(), command: "file.save".into(), context: None, mode: None };
    assert!(doc.rebind(save.clone(), false).is_err());
    assert!(doc.conflicts(&save).unwrap().iter().any(|c| c.command == "edit.copy"));
    doc.rebind(save, true).unwrap();
    let map = doc.settings().keymap().unwrap();
    assert_eq!(
        map.resolve(&"ctrl+s".parse::<KeySequence>().unwrap().0, Context::Editor, Mode::Insert),
        Resolution::Unbound
    );
    assert_eq!(
        map.resolve(&"ctrl+c".parse::<KeySequence>().unwrap().0, Context::Editor, Mode::Insert),
        Resolution::Command("file.save")
    );
    let old = Binding { keys: "ctrl+s".into(), command: "file.new".into(), context: None, mode: None };
    assert!(doc.conflicts(&old).unwrap().is_empty(), "tombstones hide inherited bindings");
    let prefix = Binding { keys: "ctrl+c ctrl+x".into(), command: "file.new".into(), context: None, mode: None };
    assert!(doc.conflicts(&prefix).unwrap().iter().any(|c| c.kind == ConflictKind::Prefix));
}

#[test]
fn scoped_rebind_and_command_reset_restore_preset_without_destroying_custom_data() {
    let mut doc =
        SettingsDocument::parse("[[bindings]]\nkeys='ctrl+shift+f12'\ncommand='file.open'\nfuture='keep' # owner\n")
            .unwrap();
    let binding =
        Binding { keys: "ctrl+alt+s".into(), command: "file.save".into(), context: Some(Context::Editor), mode: None };
    doc.rebind(binding, false).unwrap();
    let map = doc.settings().keymap().unwrap();
    let old = "ctrl+s".parse::<KeySequence>().unwrap();
    assert_eq!(map.resolve(&old.0, Context::Editor, Mode::Insert), Resolution::Unbound);
    assert_eq!(map.resolve(&old.0, Context::Palette, Mode::Insert), Resolution::Command("file.save"));
    doc.reset_command("file.save", false).unwrap();
    let map = doc.settings().keymap().unwrap();
    assert_eq!(map.resolve(&old.0, Context::Editor, Mode::Insert), Resolution::Command("file.save"));
    assert_eq!(
        map.resolve(&"ctrl+alt+s".parse::<KeySequence>().unwrap().0, Context::Editor, Mode::Insert),
        Resolution::Unbound
    );
    assert!(doc.source().contains("future='keep' # owner"));
    assert_eq!(doc.keymap_rows().unwrap().len(), COMMANDS.len());
    let once = doc.source().to_owned();
    doc.reset_command("file.save", false).unwrap();
    assert_eq!(doc.source(), once, "repeated reset must not grow duplicate binding tables");
}

#[test]
fn capture_preserves_modifiers_and_escape_cancels() {
    let mut capture = KeyCapture::default();
    capture.press("ctrl+k".parse().unwrap()).unwrap();
    capture.press("ctrl+s".parse().unwrap()).unwrap();
    assert_eq!(capture.finish().unwrap(), "ctrl+k ctrl+s");
    capture.press("escape".parse().unwrap()).unwrap();
    assert!(capture.finish().is_err());
}

#[test]
fn schema_covers_compound_preferences_and_refuses_destructive_reset() {
    let source = "[[bindings]]\nkeys='ctrl+s' # keep binding\ncommand=''\nfuture=42\n[formatters.rust]\nexecutable='rustfmt'\nfuture='preserve'\n[file_types]\n'*.foo'='rust' # owner mapping\n";
    let mut doc = SettingsDocument::parse(source).unwrap();
    assert!(doc.reset("bindings").is_err());
    assert!(doc.reset("formatters").is_err());
    assert!(doc.reset("file_types").is_err());
    assert_eq!(doc.source(), source);
    doc.edit_formatter("rust", "timeout_ms", SettingValue::Integer(4000)).unwrap();
    assert!(doc.source().contains("future='preserve'"));
    let mut menu = SettingsMenu::new(doc);
    menu.search("extension mappings");
    let SettingValue::StringMap(map) = menu.selected().unwrap().current else { panic!("mapping row missing") };
    assert_eq!(map.get("foo").map(String::as_str), Some("rust"));
    let mut empty = SettingsDocument::parse("[[bindings]]\nkeys='ctrl+s'\ncommand=''\n").unwrap();
    empty.reset("bindings").unwrap();
    assert!(empty.settings().bindings.is_empty());
}

#[test]
fn comments_inside_arrays_and_before_mapping_keys_are_preserved_or_refused() {
    let source =
        "[search]\ninclude = [\n # why this glob\n '*.rs',\n]\n[file_types]\n# custom language\n'*.foo' = 'rust'\n";
    let mut doc = SettingsDocument::parse(source).unwrap();
    assert!(doc.edit("search.include", SettingValue::Strings(vec!["*.txt".into()])).is_err());
    assert!(doc.map_extension("foo", None).is_err());
    assert_eq!(doc.source(), source);
    doc.map_extension("foo", Some("python")).unwrap();
    assert!(doc.source().contains("# custom language"));
    assert!(doc.source().contains("# why this glob"));
    let mut doc = SettingsDocument::parse("[editor]\n# indent note\n\"tab_size\"=7 # tail\n").unwrap();
    doc.edit("editor.tab_size", SettingValue::Integer(2)).unwrap();
    assert!(doc.source().contains("# indent note"));
    assert!(doc.source().contains("\"tab_size\"=2 # tail"));
}

#[test]
fn worker_loads_saves_and_reloads_external_edits_away_from_caller() {
    use kwil_core_commands::settings::{SettingsWorker, SettingsWorkerEvent};
    use std::{
        sync::mpsc,
        time::{Duration, SystemTime},
    };
    let root = std::env::temp_dir().join(format!(
        "kwil-settings-worker-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos()
    ));
    let file = root.join("settings.toml");
    let (tx, rx) = mpsc::channel();
    let caller = std::thread::current().id();
    let worker = SettingsWorker::start(file.clone(), move |event| {
        assert_ne!(caller, std::thread::current().id());
        let _ = tx.send(event);
    })
    .unwrap();
    let event = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(event, SettingsWorkerEvent::Loaded { source: None, .. }));
    worker.save(1, None, "[editor]\ntab_size=0".into()).unwrap();
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        SettingsWorkerEvent::Error { request: Some(1), .. }
    ));
    assert!(!file.exists());
    let source = "# owner\n[editor]\ntab_size=2\n";
    worker.save(2, None, source.into()).unwrap();
    assert!(matches!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), SettingsWorkerEvent::Saved { request: 2, .. }));
    assert!(matches!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), SettingsWorkerEvent::Loaded { .. }));
    std::fs::write(&file, "[editor]\ntab_size=6").unwrap();
    let event = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(event,SettingsWorkerEvent::Loaded{document,..} if document.settings().editor.tab_size==6));
    worker.shutdown();
    std::fs::remove_file(&file).unwrap();
    std::fs::remove_dir(root).unwrap();
}
