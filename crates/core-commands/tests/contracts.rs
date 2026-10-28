use kwil_core_commands::{
    Binding, COMMANDS, Context, Key, KeyChord, KeySequence, Keymap, Mode, Modifiers, NamedKey, Preset, Resolution,
    Settings, command,
    keymap::KeymapEngine,
    settings::{Platform, Remote, SettingsLocation, UpdateChannel},
    vim::{Argument, Invocation, Motion, Vim, VimResult},
};
use std::{
    collections::HashSet,
    fs,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};

fn chord(s: &str) -> KeyChord {
    s.parse().unwrap()
}
fn keys(s: &str) -> Vec<KeyChord> {
    s.parse::<KeySequence>().unwrap().0
}
fn binding(keys: &str, command: &str, context: Option<Context>, mode: Option<Mode>) -> Binding {
    Binding { keys: keys.into(), command: command.into(), context, mode }
}
fn vim_input(vim: &mut Vim, input: &str) -> Vec<Invocation> {
    let mut result = Vec::new();
    for c in input.chars() {
        match vim.feed(KeyChord::new(Key::Character(c), Modifiers::default())) {
            VimResult::Command(invocation) => {
                assert!(command(invocation.id).is_some());
                result.push(invocation);
            }
            VimResult::Pending => {}
            other => panic!("{input:?}, {c:?}: {other:?}"),
        }
    }
    result
}
fn only(input: &str) -> Invocation {
    let result = vim_input(&mut Vim::default(), input);
    assert_eq!(result.len(), 1, "{input}");
    result.into_iter().next().unwrap()
}
fn ex(input: &str) -> Invocation {
    let mut vim = Vim::default();
    assert!(vim_input(&mut vim, input).is_empty());
    let VimResult::Command(command) = vim.feed(chord("enter")) else {
        panic!("{input}");
    };
    assert_eq!(vim.mode(), Mode::Normal);
    command
}

#[test]
fn registry_covers_v1_and_has_unique_palette_metadata() {
    let mut ids = HashSet::new();
    for c in COMMANDS {
        assert!(ids.insert(c.id), "duplicate {}", c.id);
        assert!(!c.title.is_empty() && !c.category.is_empty());
        assert_eq!(command(c.id), Some(c));
    }
    for id in [
        "file.new",
        "file.open",
        "file.save",
        "file.saveAs",
        "file.saveAll",
        "file.close",
        "file.reopenClosed",
        "file.recent",
        "file.reload",
        "file.keepChanges",
        "file.restoreSession",
        "file.setEncoding",
        "file.setLineEnding",
        "edit.undo",
        "edit.redo",
        "edit.addCursor",
        "edit.columnSelection",
        "edit.moveLineUp",
        "edit.duplicateLine",
        "edit.deleteLine",
        "edit.indent",
        "edit.outdent",
        "edit.toggleComment",
        "navigation.goToLine",
        "navigation.matchBracket",
        "view.toggleWordWrap",
        "view.toggleWhitespace",
        "view.zoomIn",
        "edit.trimTrailingWhitespace",
        "search.find",
        "search.replace",
        "search.replaceAll",
        "search.findInFiles",
        "search.toggleRegex",
        "search.toggleCase",
        "search.toggleWholeWord",
        "search.toggleSelection",
        "compare.files",
        "compare.saved",
        "compare.nextChange",
        "compare.previousChange",
        "compare.toggleIgnoreWhitespace",
        "file.openHex",
        "view.toggleHex",
        "view.toggleMarkdown",
        "edit.formatDocument",
        "edit.formatSelection",
        "file.setType",
        "log.toggleFollow",
        "log.filter",
        "log.toggleLevel",
        "log.highlightRules",
        "log.toggleMark",
        "log.goToTime",
        "theme.select",
        "theme.import",
        "theme.preview",
        "theme.followSystem",
        "update.check",
        "update.apply",
    ] {
        assert!(command(id).is_some(), "missing {id}");
    }
    assert!(command("not.a.command").is_none());
}

#[test]
fn chords_parse_valid_aliases_and_reject_invalid_input() {
    assert_eq!(
        chord("ctrl+shift+p"),
        KeyChord::new(Key::Character('p'), Modifiers { ctrl: true, shift: true, ..Modifiers::default() })
    );
    for (a, b) in [
        ("Control+Shift+P", "ctrl+shift+p"),
        ("cmd+p", "super+p"),
        ("esc", "Escape"),
        ("ctrl+Plus", "ctrl+plus"),
        ("F24", "f24"),
        ("G", "shift+g"),
    ] {
        assert_eq!(chord(a), chord(b));
    }
    assert_eq!(chord("space").character(), Some(' '));
    assert_eq!(chord("é").character(), Some('é'));
    assert_eq!(keys("Ctrl+K Ctrl+C"), vec![chord("ctrl+k"), chord("ctrl+c")]);
    for bad in ["", "ctrl+", "ctrl+ctrl+s", "hyper+s", "f0", "f25", "ctrl++", "two", "ctrl+s+p"] {
        assert!(bad.parse::<KeyChord>().is_err(), "accepted {bad:?}");
    }
    assert!(" ".parse::<KeySequence>().is_err());
}

#[test]
fn adapters_normalize_logical_keys_without_conflating_ctrl_and_cmd() {
    let shifted = Modifiers { shift: true, ..Modifiers::default() };
    assert_eq!(KeyChord::new(Key::Character('$'), shifted), chord("shift+$"));
    assert_eq!(KeyChord::new(Key::Character('$'), shifted).modifiers, shifted);
    assert_eq!(KeyChord::new(Key::Character('G'), Modifiers::default()), chord("shift+g"));
    assert_eq!(KeyChord::new(Key::Character('g'), shifted).character(), Some('G'));
    assert_ne!(chord("ctrl+p"), chord("cmd+p"));
    assert_eq!(chord("ctrl+p").character(), None);
    assert_eq!(chord("enter").key, Key::Named(NamedKey::Enter));
}

#[test]
fn punctuation_preserves_logical_key_and_explicit_shift() {
    for (plain, symbol) in
        "/? [{ ]} 1! 2@ 3# 4$ 5% 6^ 7& 8* 9( 0) -_ =+ ;: '\" ,< .> `~ \\|".split_whitespace().map(|s| {
            let mut chars = s.chars();
            (chars.next().unwrap(), chars.next().unwrap())
        })
    {
        for c in [plain, symbol] {
            let name = if c == '+' { "Plus".into() } else { c.to_string() };
            let shifted = Modifiers { ctrl: true, shift: true, ..Modifiers::default() };
            assert_eq!(chord(&format!("ctrl+shift+{name}")), KeyChord::new(Key::Character(c), shifted));
            assert_ne!(chord(&format!("ctrl+{name}")), chord(&format!("ctrl+shift+{name}")));
        }
        assert_ne!(
            KeyChord::new(Key::Character(plain), Modifiers { shift: true, ..Modifiers::default() }),
            chord(&symbol.to_string().replace('+', "Plus"))
        );
    }
    assert_ne!(chord("ctrl+shift+/"), chord("ctrl+/"));
    assert_ne!(chord("ctrl+shift+/"), chord("ctrl+?"));
    let map = Keymap::default()
        .with_overrides(&[
            binding("ctrl+/", "edit.comment", None, None),
            binding("ctrl+shift+/", "edit.uncomment", None, None),
            binding("ctrl+shift+?", "search.find", None, None),
        ])
        .unwrap();
    for (c, shift, expected) in
        [('/', false, "edit.comment"), ('/', true, "edit.uncomment"), ('?', true, "search.find")]
    {
        let input = KeyChord::new(Key::Character(c), Modifiers { ctrl: true, shift, ..Modifiers::default() });
        assert_eq!(map.resolve(&[input], Context::Editor, Mode::Insert), Resolution::Command(expected));
    }
}

#[test]
fn reviewed_presets_match_windows_shortcuts() {
    let common = [
        ("ctrl+n", "file.new"),
        ("ctrl+o", "file.open"),
        ("ctrl+s", "file.save"),
        ("ctrl+w", "file.close"),
        ("ctrl+shift+t", "file.reopenClosed"),
        ("ctrl+z", "edit.undo"),
        ("ctrl+y", "edit.redo"),
        ("ctrl+x", "edit.cut"),
        ("ctrl+c", "edit.copy"),
        ("ctrl+v", "edit.paste"),
        ("ctrl+a", "edit.selectAll"),
        ("ctrl+f", "search.find"),
        ("ctrl+h", "search.replace"),
        ("ctrl+shift+f", "search.findInFiles"),
        ("f3", "search.findNext"),
        ("shift+f3", "search.findPrevious"),
        ("ctrl+g", "navigation.goToLine"),
        ("ctrl+tab", "file.nextTab"),
        ("ctrl+shift+tab", "file.previousTab"),
        ("ctrl+shift+p", "view.commandPalette"),
    ];
    let vscode = [
        ("ctrl+shift+k", "edit.deleteLine"),
        ("ctrl+shift+s", "file.saveAs"),
        ("ctrl+d", "edit.selectNextMatch"),
        ("alt+shift+down", "edit.duplicateLine"),
        ("alt+shift+up", "edit.duplicateLine"),
        ("alt+up", "edit.moveLineUp"),
        ("alt+down", "edit.moveLineDown"),
        ("ctrl+k ctrl+c", "edit.comment"),
        ("ctrl+k ctrl+u", "edit.uncomment"),
        ("ctrl+/", "edit.toggleComment"),
        ("alt+z", "view.toggleWordWrap"),
        ("alt+shift+f", "edit.formatDocument"),
        ("ctrl+]", "edit.indent"),
        ("ctrl+[", "edit.outdent"),
        ("ctrl+shift+\\", "navigation.matchBracket"),
        ("ctrl+k s", "file.saveAll"),
        ("ctrl+shift+|", "navigation.matchBracket"),
        ("ctrl+k ctrl+w", "file.closeAll"),
        ("ctrl+k ctrl+f", "edit.formatSelection"),
        ("ctrl+k ctrl+x", "edit.trimTrailingWhitespace"),
        ("ctrl+k m", "file.setType"),
        ("ctrl+shift+h", "search.replaceInFiles"),
        ("ctrl+,", "view.settings"),
        ("f1", "view.commandPalette"),
        ("ctrl+alt+up", "edit.addCursorAbove"),
        ("ctrl+alt+down", "edit.addCursorBelow"),
    ];
    let notepad = [
        ("ctrl+shift+s", "file.saveAll"),
        ("ctrl+alt+s", "file.saveAs"),
        ("ctrl+l", "edit.cutLine"),
        ("ctrl+shift+l", "edit.deleteLine"),
        ("ctrl+d", "edit.duplicateLine"),
        ("ctrl+q", "edit.toggleComment"),
        ("ctrl+k", "edit.comment"),
        ("ctrl+shift+k", "edit.uncomment"),
        ("ctrl+b", "navigation.matchBracket"),
        ("ctrl+r", "file.reload"),
        ("ctrl+shift+w", "file.closeAll"),
        ("ctrl+pageDown", "file.nextTab"),
        ("ctrl+pageUp", "file.previousTab"),
        ("ctrl+shift+z", "edit.redo"),
        ("tab", "edit.indent"),
        ("shift+tab", "edit.outdent"),
        ("ctrl+shift+up", "edit.moveLineUp"),
        ("ctrl+shift+down", "edit.moveLineDown"),
    ];
    for (preset, extra) in [(Preset::VsCode, vscode.as_slice()), (Preset::NotepadPlusPlus, notepad.as_slice())] {
        let map = Keymap::preset(preset).unwrap();
        for (input, expected) in common.iter().chain(extra) {
            assert_eq!(
                map.resolve(&keys(input), Context::Editor, Mode::Insert),
                Resolution::Command(expected),
                "{preset:?}: {input}"
            );
        }
    }
    assert_eq!(
        Keymap::preset(Preset::Vim).unwrap().resolve(&keys("ctrl+shift+p"), Context::Editor, Mode::Normal),
        Resolution::Command("view.commandPalette")
    );
}

#[test]
fn settings_validation_errors_keep_locations() {
    for (source, path, line) in [
        ("[editor]\ntab_size=0", "editor.tab_size", 2),
        ("[font]\nsize=-1", "font.size", 2),
        ("[font]\nline_height=nan", "font.line_height", 2),
        ("[font]\nfamily=''", "font.family", 2),
        ("[files]\njournal_interval_seconds=0", "files.journal_interval_seconds", 2),
        ("[undo]\nmemory_limit_bytes=0", "undo.memory_limit_bytes", 2),
        ("[formatters.'c++']\nexecutable='clang-format'\ntimeout_ms=0", "formatters.c++.timeout_ms", 3),
        ("[[bindings]]\nkeys='a'\ncommand='unknown'", "bindings[0].command", 3),
        ("bindings=[{keys='ctrl+', command='file.save'}]", "bindings[0].keys", 1),
    ] {
        let error = Settings::parse(source).unwrap_err().to_string();
        assert!(error.starts_with(&format!("{path} at line {line},")), "{source:?}: {error}");
    }
}

#[test]
fn settings_parse_errors_keep_locations() {
    for (source, path, line) in [
        ("# comment\n[font]\nsize='large'", "font.size", 3),
        ("[render]\nremote='sometimes'", "render.remote", 2),
        ("[editor]\ntab_size=", "editor.tab_size", 2),
        (
            "[[bindings]]\nkeys='a'\ncommand='edit.copy'\n[[bindings]]\nkeys=42\ncommand='edit.copy'",
            "bindings[1].keys",
            5,
        ),
        ("editor = { tab_size = 'bad' }", "editor.tab_size", 1),
    ] {
        let error = Settings::parse(source).unwrap_err().to_string();
        assert!(error.starts_with(&format!("{path} at line {line},")), "{source:?}: {error}");
    }
}

#[test]
fn settings_unknown_warnings_keep_key_spans() {
    let source = "# comment\n[font]\n\"odd.key\" = 1\n[formatters.'c++']\nexecutable='clang-format'\ntypo=2\n[[bindings]]\nkeys='a'\ncommand='edit.copy'\ntypo=3\n[[bindings]]\nkeys='b'\ncommand='edit.copy'\ntypo=4";
    let report = Settings::parse(source).unwrap();
    for (path, line) in
        [("font.odd.key", 3), ("formatters.c++.typo", 6), ("bindings[0].typo", 10), ("bindings[1].typo", 14)]
    {
        assert!(report.unknown_keys.contains(&path.to_string()));
        assert!(
            report.unknown_key_diagnostics.iter().any(|d| d.path == path && d.line == line && d.column == 1),
            "{path}"
        );
    }
}

#[test]
fn presets_load_and_reference_registered_commands() {
    for preset in [Preset::VsCode, Preset::NotepadPlusPlus, Preset::Vim] {
        let map = Keymap::preset(preset).unwrap();
        let mut bindings = HashSet::new();
        for b in map.bindings() {
            assert!(command(&b.command).is_some(), "{}", b.command);
            assert!(bindings.insert((keys(&b.keys), b.context, b.mode)));
        }
        assert_eq!(map.resolve(&keys("ctrl+s"), Context::Editor, Mode::Insert), Resolution::Command("file.save"));
        assert_eq!(map.resolve(&keys("escape"), Context::Palette, Mode::Normal), Resolution::Command("palette.cancel"));
    }
    assert_eq!(
        Keymap::preset(Preset::VsCode).unwrap().resolve(&keys("ctrl+d"), Context::Editor, Mode::Insert),
        Resolution::Command("edit.selectNextMatch")
    );
    assert_eq!(
        Keymap::preset(Preset::NotepadPlusPlus).unwrap().resolve(&keys("ctrl+d"), Context::Editor, Mode::Insert),
        Resolution::Command("edit.duplicateLine")
    );
    assert!(Keymap::parse("[[bindings]]\nkeys='a'\ncommand='missing.id'").is_err());
}

#[test]
fn overrides_add_rebind_unbind_and_validate_atomically() {
    let base = Keymap::preset(Preset::VsCode).unwrap();
    let map = base
        .with_overrides(&[
            binding("ctrl+s", "", None, None),
            binding("ctrl+alt+s", "file.save", None, None),
            binding("ctrl+d", "edit.duplicateLine", None, None),
            binding("ctrl+f", "", Some(Context::Hex), None),
        ])
        .unwrap();
    assert_eq!(map.resolve(&keys("ctrl+s"), Context::Editor, Mode::Insert), Resolution::Unbound);
    assert_eq!(map.resolve(&keys("ctrl+alt+s"), Context::Editor, Mode::Insert), Resolution::Command("file.save"));
    assert_eq!(map.resolve(&keys("ctrl+d"), Context::Editor, Mode::Insert), Resolution::Command("edit.duplicateLine"));
    assert_eq!(map.resolve(&keys("ctrl+f"), Context::Hex, Mode::Normal), Resolution::Unbound);
    assert_eq!(map.resolve(&keys("ctrl+f"), Context::Editor, Mode::Normal), Resolution::Command("search.find"));
    assert!(
        base.with_overrides(&[binding("ctrl+s", "edit.copy", None, None), binding("ctrl+b", "bad", None, None)])
            .is_err()
    );
    assert_eq!(base.resolve(&keys("ctrl+s"), Context::Editor, Mode::Normal), Resolution::Command("file.save"));
    assert!(base.with_overrides(&[binding("ctrl+", "edit.copy", None, None)]).is_err());
}

#[test]
fn resolution_handles_sequences_contexts_modes_and_prefix_conflicts() {
    let map = Keymap::default()
        .with_overrides(&[
            binding("ctrl+k ctrl+c", "edit.comment", Some(Context::Editor), None),
            binding("a", "edit.copy", None, None),
            binding("a", "edit.paste", Some(Context::Hex), None),
            binding("a", "edit.undo", None, Some(Mode::Normal)),
            binding("b c", "edit.copy", None, None),
            binding("b", "edit.paste", None, None),
        ])
        .unwrap();
    assert_eq!(map.resolve(&keys("ctrl+k"), Context::Editor, Mode::Insert), Resolution::Pending);
    assert_eq!(map.resolve(&keys("ctrl+k ctrl+c"), Context::Editor, Mode::Insert), Resolution::Command("edit.comment"));
    assert_eq!(map.resolve(&keys("ctrl+k"), Context::Hex, Mode::Insert), Resolution::Unbound);
    assert_eq!(map.resolve(&keys("ctrl+k ctrl+d"), Context::Editor, Mode::Insert), Resolution::Unbound);
    assert_eq!(map.resolve(&keys("a"), Context::Hex, Mode::Normal), Resolution::Command("edit.paste"));
    assert_eq!(map.resolve(&keys("a"), Context::Editor, Mode::Normal), Resolution::Command("edit.undo"));
    assert_eq!(map.resolve(&keys("a"), Context::Editor, Mode::Insert), Resolution::Command("edit.copy"));
    assert_eq!(map.resolve(&keys("b"), Context::Editor, Mode::Normal), Resolution::Command("edit.paste"));
    let map = map.with_overrides(&[binding("ctrl+k ctrl+c", "", Some(Context::Editor), None)]).unwrap();
    assert_eq!(map.resolve(&keys("ctrl+k"), Context::Editor, Mode::Normal), Resolution::Unbound);
    assert_eq!(map.resolve(&[], Context::Editor, Mode::Normal), Resolution::Unbound);
}

#[test]
fn vim_motions_and_counts_table() {
    for (input, motion, count, explicit) in [
        ("h", Motion::Left, 1, false),
        ("2j", Motion::Down, 2, true),
        ("k", Motion::Up, 1, false),
        ("l", Motion::Right, 1, false),
        ("12w", Motion::WordForward, 12, true),
        ("b", Motion::WordBackward, 1, false),
        ("e", Motion::WordEnd, 1, false),
        ("0", Motion::LineStart, 1, false),
        ("$", Motion::LineEnd, 1, false),
        ("gg", Motion::FirstLine, 1, false),
        ("10gg", Motion::FirstLine, 10, true),
        ("G", Motion::LastLine, 1, false),
        ("1G", Motion::LastLine, 1, true),
    ] {
        let command = only(input);
        assert_eq!(
            (command.id, command.motion, command.count, command.explicit_count),
            (motion.id(), Some(motion), count, explicit),
            "{input}"
        );
    }
}

#[test]
fn vim_operators_counts_and_text_objects_table() {
    for (input, id, motion, count) in [
        ("dw", "operator.delete", Motion::WordForward, 1),
        ("2d3w", "operator.delete", Motion::WordForward, 6),
        ("d0", "operator.delete", Motion::LineStart, 1),
        ("d$", "operator.delete", Motion::LineEnd, 1),
        ("dgg", "operator.delete", Motion::FirstLine, 1),
        ("dG", "operator.delete", Motion::LastLine, 1),
        ("dd", "operator.delete", Motion::Line, 1),
        ("3yy", "operator.yank", Motion::Line, 3),
        ("2cc", "operator.change", Motion::Line, 2),
        ("cb", "operator.change", Motion::WordBackward, 1),
        ("ye", "operator.yank", Motion::WordEnd, 1),
        ("diw", "operator.delete", Motion::InnerWord, 1),
        ("daw", "operator.delete", Motion::AroundWord, 1),
        ("ci\"", "operator.change", Motion::InnerQuote, 1),
        ("ya\"", "operator.yank", Motion::AroundQuote, 1),
        ("di(", "operator.delete", Motion::InnerParentheses, 1),
        ("ca(", "operator.change", Motion::AroundParentheses, 1),
        ("yi)", "operator.yank", Motion::InnerParentheses, 1),
        ("2d3aw", "operator.delete", Motion::AroundWord, 6),
    ] {
        let mut vim = Vim::default();
        let commands = vim_input(&mut vim, input);
        assert_eq!(commands.len(), 1, "{input}");
        assert_eq!((commands[0].id, commands[0].motion, commands[0].count), (id, Some(motion), count), "{input}");
        assert_eq!(vim.mode(), if id == "operator.change" { Mode::Insert } else { Mode::Normal });
        assert!(!vim.operator_pending());
    }
    assert!(only("1dw").explicit_count);
}

#[test]
fn vim_modes_insert_visual_and_cancel() {
    for (input, id) in [
        ("i", "edit.insert"),
        ("a", "edit.append"),
        ("o", "edit.openBelow"),
        ("O", "edit.openAbove"),
        ("I", "edit.insertLineStart"),
        ("A", "edit.appendLineEnd"),
    ] {
        let mut vim = Vim::default();
        assert_eq!(vim_input(&mut vim, input)[0].id, id);
        assert_eq!(vim.mode(), Mode::Insert);
        assert_eq!(vim.feed(chord("z")), VimResult::Unbound);
        assert!(matches!(vim.feed(chord("escape")), VimResult::Command(c) if c.id == "mode.normal"));
        assert_eq!(vim.mode(), Mode::Normal);
    }
    for (input, mode, motion, id) in [
        ("vd", Mode::Normal, Motion::Selection, "operator.delete"),
        ("Vy", Mode::Normal, Motion::Line, "operator.yank"),
        ("vc", Mode::Insert, Motion::Selection, "operator.change"),
    ] {
        let mut vim = Vim::default();
        let commands = vim_input(&mut vim, input);
        assert_eq!(commands[1].motion, Some(motion));
        assert_eq!(commands[1].id, id);
        assert_eq!(vim.mode(), mode);
    }
    let mut vim = Vim::default();
    vim_input(&mut vim, "v");
    assert_eq!(vim.mode(), Mode::Visual);
    vim_input(&mut vim, "v");
    assert_eq!(vim.mode(), Mode::Normal);
    vim_input(&mut vim, "V");
    assert_eq!(vim.mode(), Mode::VisualLine);
    vim.feed(chord("escape"));
    vim_input(&mut vim, "2d");
    assert!(vim.operator_pending());
    vim.feed(chord("escape"));
    assert!(!vim.operator_pending());
    assert_eq!(vim_input(&mut vim, "w")[0].count, 1);
    vim_input(&mut vim, "d");
    assert!(matches!(vim.feed(chord("z")), VimResult::Error(_)));
    assert!(!vim.operator_pending());
}

#[test]
fn vim_repeat_search_and_ex_commands() {
    for (input, id, count) in [
        ("x", "edit.deleteCharacter", 1),
        ("p", "edit.pasteAfter", 1),
        ("P", "edit.pasteBefore", 1),
        ("u", "edit.undo", 1),
        ("3.", "edit.repeat", 3),
        ("n", "search.findNext", 1),
        ("N", "search.findPrevious", 1),
    ] {
        let c = only(input);
        assert_eq!((c.id, c.count), (id, count));
    }
    let mut vim = Vim::default();
    let commands = vim_input(&mut vim, "2dw.3.");
    assert_eq!(
        commands.iter().map(|c| (c.id, c.count)).collect::<Vec<_>>(),
        vec![("operator.delete", 2), ("edit.repeat", 1), ("edit.repeat", 3)]
    );
    assert!(matches!(vim.feed(chord("ctrl+r")), VimResult::Command(c) if c.id == "edit.redo"));
    for (input, id) in [(":w", "file.save"), (":q", "file.quit"), (":wq", "file.saveAndQuit")] {
        assert_eq!(ex(input).id, id);
    }
    assert_eq!(ex("/needle").argument, Argument::Search("needle".into()));
    assert_eq!(
        ex(":s/a/b/g").argument,
        Argument::Substitute { pattern: "a".into(), replacement: "b".into(), global: true, whole_file: false }
    );
    assert_eq!(
        ex(":%s#a/b#x#g").argument,
        Argument::Substitute { pattern: "a/b".into(), replacement: "x".into(), global: true, whole_file: true }
    );
    assert_eq!(
        ex(":s/a\\/b/\\1/g").argument,
        Argument::Substitute { pattern: "a/b".into(), replacement: "\\1".into(), global: true, whole_file: false }
    );
    for input in [":s/a/b", ":s/a/b/i", ":unknown"] {
        let mut vim = Vim::default();
        vim_input(&mut vim, input);
        assert!(matches!(vim.feed(chord("enter")), VimResult::Error(_)));
        assert_eq!(vim.mode(), Mode::Normal);
    }
    let mut vim = Vim::default();
    vim_input(&mut vim, ":wz");
    vim.feed(chord("backspace"));
    assert_eq!(vim.command_line(), Some("w"));
}

#[test]
fn vim_registers_and_overflow_reset_pending_state() {
    assert_eq!(only("\"add").register, Some('a'));
    assert_eq!(only("\"Ap").register, Some('A'));
    assert_eq!(only("\"_2d3w").count, 6);
    let mut vim = Vim::default();
    vim_input(&mut vim, "4294967295");
    assert!(matches!(vim.feed(chord("9")), VimResult::Error(_)));
    assert_eq!(vim_input(&mut vim, "w")[0].count, 1);
    vim_input(&mut vim, "4294967295d2");
    assert!(matches!(vim.feed(chord("w")), VimResult::Error(_)));
    assert!(!vim.operator_pending());
    vim_input(&mut vim, "4294967295c2");
    assert!(matches!(vim.feed(chord("w")), VimResult::Error(_)));
    assert_eq!(vim.mode(), Mode::Normal);
}

#[test]
fn engine_dispatches_overrides_before_vim_and_cancels_context_sequences() {
    let mut engine = KeymapEngine::new(
        Preset::Vim,
        &[
            binding("h", "edit.undo", Some(Context::Editor), Some(Mode::Normal)),
            binding("x", "", Some(Context::Editor), Some(Mode::Normal)),
        ],
    )
    .unwrap();
    assert!(matches!(engine.feed(chord("h")), VimResult::Command(c) if c.id == "edit.undo"));
    assert_eq!(engine.feed(chord("x")), VimResult::Unbound);
    let mut rebound = KeymapEngine::new(
        Preset::Vim,
        &[
            binding("z", "motion.wordForward", Some(Context::Editor), Some(Mode::Normal)),
            binding("f", "operator.delete", Some(Context::Editor), Some(Mode::Normal)),
            binding("t", "edit.insert", Some(Context::Editor), Some(Mode::Normal)),
        ],
    )
    .unwrap();
    rebound.feed(chord("2"));
    rebound.feed(chord("f"));
    rebound.feed(chord("3"));
    assert!(matches!(rebound.feed(chord("z")), VimResult::Command(c)
        if c.id == "operator.delete" && c.count == 6 && c.motion == Some(Motion::WordForward)));
    assert!(matches!(rebound.feed(chord("t")), VimResult::Command(c) if c.id == "edit.insert"));
    assert_eq!(rebound.mode(), Mode::Insert);
    rebound.feed(chord("escape"));
    rebound.feed(chord(":"));
    rebound.feed(chord("w"));
    assert_eq!(rebound.mode(), Mode::Command);
    rebound.set_context(Context::Palette);
    rebound.set_context(Context::Editor);
    assert_eq!(rebound.mode(), Mode::Normal);
    assert_eq!(engine.feed(chord("d")), VimResult::Pending);
    assert!(matches!(engine.feed(chord("w")), VimResult::Command(c) if c.id == "operator.delete"));
    engine.feed(chord("d"));
    engine.set_context(Context::Palette);
    assert!(matches!(engine.feed(chord("enter")), VimResult::Command(c) if c.id == "palette.accept"));
    engine.set_context(Context::Editor);
    assert!(matches!(engine.feed(chord("w")), VimResult::Command(c) if c.id == "motion.wordForward"));
    let mut engine = KeymapEngine::new(Preset::VsCode, &[]).unwrap();
    assert_eq!(engine.feed(chord("ctrl+k")), VimResult::Pending);
    assert!(matches!(engine.feed(chord("ctrl+c")), VimResult::Command(c) if c.id == "edit.comment"));
    engine.feed(chord("ctrl+k"));
    assert!(matches!(engine.feed(chord("ctrl+s")), VimResult::Command(c) if c.id == "file.save"));
    engine.feed(chord("ctrl+k"));
    engine.set_context(Context::Hex);
    assert_eq!(
        engine.feed(chord("ctrl+c")),
        VimResult::Command(Invocation {
            id: "edit.copy",
            count: 1,
            explicit_count: false,
            motion: None,
            register: None,
            argument: Argument::None
        })
    );
}

#[test]
fn settings_defaults_unknown_keys_and_typed_values() {
    let report = Settings::parse("").unwrap();
    let defaults = &report.settings;
    assert_eq!(*defaults, Settings::default());
    assert!(report.unknown_keys.is_empty());
    assert_eq!(defaults.keymap, Preset::VsCode);
    assert_eq!(defaults.render.remote, Remote::Auto);
    assert!(defaults.update.check);
    let expected_channel = if env!("CARGO_PKG_VERSION").split('+').next().unwrap().contains('-') {
        UpdateChannel::Prerelease
    } else {
        UpdateChannel::Stable
    };
    assert_eq!(defaults.update.channel, expected_channel);
    assert_eq!(defaults.editor.tab_size, 4);
    assert!(!defaults.editor.word_wrap);
    assert_eq!(defaults.font.family, "monospace");
    assert_eq!(defaults.theme.name, "dark");
    assert_eq!(defaults.files.small_file_threshold_bytes, 64 * 1024 * 1024);
    let report = Settings::parse("keymap='notepad++'\nunknown=true\n[font]\nsize=16\ntypo=1\n[render]\nremote='on'\n[theme.colors]\nbackground='#000'\n[formatters.rs]\nexecutable='rustfmt'\ntypo=1\n[[bindings]]\nkeys='ctrl+s'\ncommand='file.save'\ntypo=2").unwrap();
    assert_eq!(report.settings.keymap, Preset::NotepadPlusPlus);
    assert_eq!(report.settings.font.size, 16.0);
    assert_eq!(report.settings.render.remote, Remote::On);
    assert_eq!(
        report.unknown_keys.into_iter().collect::<HashSet<_>>(),
        ["unknown", "font.typo", "formatters.rs.typo", "bindings[0].typo"].map(String::from).into_iter().collect()
    );
}

#[test]
fn settings_validate_values_and_apply_binding_overrides() {
    let settings = Settings::parse("keymap='vim'\n[[bindings]]\nkeys='ctrl+s'\ncommand=''\n[[bindings]]\nkeys='ctrl+alt+s'\ncommand='file.save'\n[update]\ncheck=false\nchannel='prerelease'\n[editor]\nword_wrap=true\ntab_size=2").unwrap().settings;
    assert!(!settings.update.check);
    assert!(settings.editor.word_wrap);
    assert_eq!(settings.editor.tab_size, 2);
    let map = settings.keymap().unwrap();
    assert_eq!(map.resolve(&keys("ctrl+s"), Context::Editor, Mode::Insert), Resolution::Unbound);
    assert_eq!(map.resolve(&keys("ctrl+alt+s"), Context::Editor, Mode::Insert), Resolution::Command("file.save"));
    for source in [
        "keymap='emacs'",
        "[render]\nremote='sometimes'",
        "[font]\nsize=-1",
        "[font]\nsize=nan",
        "[font]\nline_height=0",
        "[editor]\ntab_size=0",
        "[update]\ncheck='yes'",
        "[formatters.rs]\nexecutable='rustfmt'\ntimeout_ms=0",
        "[[bindings]]\nkeys='a'\ncommand='unknown'",
    ] {
        assert!(Settings::parse(source).is_err(), "accepted {source}");
    }
}

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "kwil-commands-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn settings_reload_hook_keeps_last_valid_and_delivers_changes() {
    let fixture = Fixture::new();
    let path = fixture.0.join("settings.toml");
    let mut callbacks = Vec::new();
    {
        let mut watch = Settings::watch(&path, |report| callbacks.push(report.settings.editor.tab_size));
        assert_eq!(watch.path(), path);
        assert!(watch.reload().unwrap());
        assert!(!watch.reload().unwrap());
        let temporary = fixture.0.join("replacement.toml");
        fs::write(&temporary, "[editor]\ntab_size=8").unwrap();
        fs::rename(&temporary, &path).unwrap();
        assert!(watch.reload().unwrap());
        fs::write(&path, "[editor]\ntab_size=0").unwrap();
        assert!(watch.reload().is_err());
        assert_eq!(watch.current().unwrap().settings.editor.tab_size, 8);
        fs::remove_file(&path).unwrap();
        assert!(watch.reload().unwrap());
        fs::write(&path, vec![b' '; 1024 * 1024 + 1]).unwrap();
        assert!(watch.reload().is_err());
    }
    assert_eq!(callbacks, vec![4, 8, 4]);
}

#[test]
fn settings_paths_cover_portable_and_all_platforms_without_env_mutation() {
    let fixture = Fixture::new();
    let exe = fixture.0.join("kwil.exe");
    let base = SettingsLocation {
        platform: Platform::Windows,
        appdata: Some(fixture.0.join("appdata")),
        home: Some(fixture.0.join("home")),
        xdg_config_home: Some(fixture.0.join("xdg")),
    };
    assert_eq!(base.resolve(&exe).unwrap(), fixture.0.join("appdata/kwil/settings.toml"));
    let mut location = base.clone();
    location.platform = Platform::Mac;
    assert_eq!(location.resolve(&exe).unwrap(), fixture.0.join("home/Library/Application Support/kwil/settings.toml"));
    location.platform = Platform::Unix;
    assert_eq!(location.resolve(&exe).unwrap(), fixture.0.join("xdg/kwil/settings.toml"));
    location.xdg_config_home = Some(PathBuf::from("relative"));
    assert_eq!(location.resolve(&exe).unwrap(), fixture.0.join("home/.config/kwil/settings.toml"));
    location.home = None;
    assert!(location.resolve(&exe).is_err());
    fs::write(fixture.0.join("kwil.portable"), "").unwrap();
    assert_eq!(location.resolve(&exe).unwrap(), fixture.0.join("settings.toml"));
    assert!(location.resolve(std::path::Path::new("kwil")).is_err());
}
