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
fn tabs_layout_is_typed_validated_searchable_and_resets_without_losing_unknown_fields() {
    assert_eq!(Settings::default().tabs.layout, TabLayout::Horizontal);
    assert_eq!(Settings::parse("[tabs]\nlayout='vertical'\n").unwrap().settings.tabs.layout, TabLayout::Vertical);
    assert!(Settings::parse("[tabs]\nlayout='sideways'\n").is_err());
    assert!(Settings::parse("[tabs]\nlayout=true\n").is_err());
    let source = "[tabs]\nlayout='vertical' # owner position\nfuture='keep'\n";
    let mut menu = SettingsMenu::new(SettingsDocument::parse(source).unwrap());
    menu.search("tabs layout");
    let row = menu.selected().unwrap();
    assert_eq!(row.spec.id, "tabs.layout");
    assert_eq!(row.current, SettingValue::Text("vertical".into()));
    assert_eq!(row.default, SettingValue::Text("horizontal".into()));
    assert_eq!(menu.document().report().unknown_keys, vec!["tabs.future"]);
    assert!(menu.edit_selected(SettingValue::Text("sideways".into())).is_err());
    assert_eq!(menu.document().source(), source);
    menu.reset_selected().unwrap();
    assert_eq!(menu.document().settings().tabs.layout, TabLayout::Horizontal);
    assert!(menu.document().source().contains("# owner position"));
    assert!(menu.document().source().contains("future='keep'"));
    menu.edit_selected(SettingValue::Text("vertical".into())).unwrap();
    assert_eq!(menu.document().settings().tabs.layout, TabLayout::Vertical);
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
fn replacement_rebind_refuses_unreachable_prefixes_and_more_specific_selectors() {
    let mut doc = SettingsDocument::parse("").unwrap();
    let before = doc.source().to_owned();
    let long = Binding { keys: "ctrl+s ctrl+x".into(), command: "file.open".into(), context: None, mode: None };
    let error = doc.rebind(long.clone(), true).unwrap_err();
    assert!(error.0.contains("unreachable shortcut"));
    assert_eq!(doc.source(), before, "failed replacement must not emit partial unbinds");
    let source =
        "[[bindings]]\nkeys='ctrl+c'\ncommand='edit.copy'\ncontext='editor'\nmode='insert'\nfuture='owner' # keep\n";
    let mut doc = SettingsDocument::parse(source).unwrap();
    let global = Binding { keys: "ctrl+c".into(), command: "file.open".into(), context: None, mode: None };
    assert!(doc.rebind(global.clone(), true).is_err());
    assert_eq!(doc.source(), source);
    let scoped = Binding { context: Some(Context::Editor), mode: Some(Mode::Insert), ..global };
    doc.rebind(scoped, true).unwrap();
    assert!(doc.source().contains("future='owner' # keep"));
    assert_eq!(
        doc.settings().keymap().unwrap().resolve(
            &"ctrl+c".parse::<KeySequence>().unwrap().0,
            Context::Editor,
            Mode::Insert
        ),
        Resolution::Command("file.open")
    );
    let mut doc = SettingsDocument::parse("[[bindings]]\nkeys='ctrl+s'\ncommand=''\n").unwrap();
    doc.rebind(long, true).unwrap();
    let map = doc.settings().keymap().unwrap();
    assert_eq!(
        map.resolve(&"ctrl+s".parse::<KeySequence>().unwrap().0, Context::Editor, Mode::Insert),
        Resolution::Pending
    );
    assert_eq!(
        map.resolve(&"ctrl+s ctrl+x".parse::<KeySequence>().unwrap().0, Context::Editor, Mode::Insert),
        Resolution::Command("file.open")
    );
}

fn vim_binding(keys: &str, mode: Option<Mode>) -> Binding {
    Binding { keys: keys.into(), command: "file.save".into(), context: Some(Context::Editor), mode }
}

#[test]
fn vim_grammar_probe_reports_motion_operator_and_multikey_takeovers() {
    let mut doc = SettingsDocument::parse("keymap='vim'\n").unwrap();
    let source = doc.source().to_owned();
    let motion = doc.vim_grammar_warnings(&vim_binding("j", Some(Mode::Normal))).unwrap();
    assert_eq!(motion.len(), 1);
    assert_eq!(motion[0].takeover, VimTakeover::Command("motion.down"));
    assert_eq!(motion[0].kind, ConflictKind::Exact);
    assert_eq!(motion[0].consumed_chords, 1);
    assert!(doc.conflicts(&vim_binding("j", Some(Mode::Normal))).unwrap().iter().any(|c| c.command == "motion.down"));
    assert!(doc.rebind(vim_binding("j", Some(Mode::Normal)), false).is_err());
    assert_eq!(doc.source(), source);
    let operator = doc.vim_grammar_warnings(&vim_binding("d", Some(Mode::Normal))).unwrap();
    assert_eq!(operator[0].takeover, VimTakeover::Pending);
    assert_eq!(operator[0].kind, ConflictKind::Prefix);
    let gg = doc.vim_grammar_warnings(&vim_binding("g g", Some(Mode::Normal))).unwrap();
    assert_eq!(gg[0].takeover, VimTakeover::Command("motion.firstLine"));
    assert_eq!(gg[0].kind, ConflictKind::Exact);
    assert_eq!(gg[0].consumed_chords, 2);
    let prefix = doc.vim_grammar_warnings(&vim_binding("j x", Some(Mode::Normal))).unwrap();
    assert_eq!(prefix[0].takeover, VimTakeover::Command("motion.down"));
    assert_eq!(prefix[0].kind, ConflictKind::Prefix);
    let failed_sequence = doc.vim_grammar_warnings(&vim_binding("g z", Some(Mode::Normal))).unwrap();
    assert_eq!(failed_sequence[0].takeover, VimTakeover::Pending);
    assert_eq!(failed_sequence[0].keys, "g");
    let visual = doc.vim_grammar_warnings(&vim_binding("d", Some(Mode::Visual))).unwrap();
    assert_eq!(visual[0].mode, Mode::Visual);
    assert_eq!(visual[0].takeover, VimTakeover::Command("operator.delete"));
    assert!(doc.vim_grammar_warnings(&vim_binding("g g g g g g g g g", Some(Mode::Normal))).is_err());
}

#[test]
fn vim_grammar_probe_respects_tombstones_explicit_precedence_modes_and_contexts() {
    let doc =
        SettingsDocument::parse("keymap='vim'\n[[bindings]]\nkeys='j'\ncommand=''\ncontext='editor'\nmode='normal'\n")
            .unwrap();
    assert!(doc.vim_grammar_warnings(&vim_binding("j", Some(Mode::Normal))).unwrap().is_empty());
    assert!(
        doc.vim_grammar_warnings(&vim_binding("j", Some(Mode::Visual))).unwrap().iter().any(|w| w.mode == Mode::Visual)
    );
    let explicit = SettingsDocument::parse(
        "keymap='vim'\n[[bindings]]\nkeys='j'\ncommand='file.open'\ncontext='editor'\nmode='normal'\n",
    )
    .unwrap();
    assert!(explicit.vim_grammar_warnings(&vim_binding("j", Some(Mode::Normal))).unwrap().is_empty());
    assert!(
        explicit.conflicts(&vim_binding("j", Some(Mode::Normal))).unwrap().iter().any(|c| c.command == "file.open")
    );
    assert!(doc.vim_grammar_warnings(&vim_binding("j", Some(Mode::Insert))).unwrap().is_empty());
    let mut palette = vim_binding("j", Some(Mode::Normal));
    palette.context = Some(Context::Palette);
    assert!(doc.vim_grammar_warnings(&palette).unwrap().is_empty());
    let nonvim = SettingsDocument::parse("").unwrap();
    assert!(nonvim.vim_grammar_warnings(&vim_binding("j", Some(Mode::Normal))).unwrap().is_empty());
    let native = SettingsDocument::parse("keymap='vim'").unwrap();
    let mut same = vim_binding("j", Some(Mode::Normal));
    same.command = "motion.down".into();
    assert!(native.vim_grammar_warnings(&same).unwrap().is_empty());
    let mut unbind = vim_binding("j", Some(Mode::Normal));
    unbind.command.clear();
    assert!(native.vim_grammar_warnings(&unbind).unwrap().is_empty());
}

#[test]
fn vim_same_command_rebind_requires_equivalent_invocation_semantics() {
    for (keys, command) in [
        ("d w", "operator.delete"),
        ("d d", "operator.delete"),
        ("2 j", "motion.down"),
        ("1 G", "motion.lastLine"),
        ("\" a j", "motion.down"),
        ("/ a enter", "search.find"),
    ] {
        let mut doc = SettingsDocument::parse("keymap='vim'\n# owner annotation\n").unwrap();
        let source = doc.source().to_owned();
        let mut binding = vim_binding(keys, Some(Mode::Normal));
        binding.command = command.into();
        let warnings = doc.vim_grammar_warnings(&binding).unwrap();
        assert_eq!(warnings.len(), 1, "{keys} must warn despite matching command ID");
        assert_eq!(warnings[0].takeover, VimTakeover::Command(command));
        assert_eq!(warnings[0].kind, ConflictKind::Exact);
        assert!(doc.conflicts(&binding).unwrap().iter().any(|c| c.command == command));
        assert!(doc.rebind(binding.clone(), false).is_err(), "{keys} needs explicit replacement choice");
        assert_eq!(doc.source(), source);
        doc.rebind(binding, true).unwrap();
        assert!(doc.source().contains("# owner annotation"));
    }
    // A motion without a count and a Visual selection operator really do
    // produce the same invocation and resulting mode through explicit dispatch.
    let doc = SettingsDocument::parse("keymap='vim'\n").unwrap();
    for (keys, command, mode) in [
        ("j", "motion.down", Mode::Normal),
        ("g g", "motion.firstLine", Mode::Normal),
        ("d", "operator.delete", Mode::Visual),
        ("c", "operator.change", Mode::VisualLine),
    ] {
        let mut binding = vim_binding(keys, Some(mode));
        binding.command = command.into();
        assert!(doc.vim_grammar_warnings(&binding).unwrap().is_empty(), "equivalent {keys} should not warn");
    }
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
