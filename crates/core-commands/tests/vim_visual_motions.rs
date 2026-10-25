//! Source-only gj/gk contracts. ROOT runs these alongside the existing Vim
//! contracts and editor repeat/operator tests; this crate owns no document.
use kwil_core_commands::{
    Binding, Context, Key, KeyChord, Mode, Modifiers, Preset, command,
    keymap::KeymapEngine,
    vim::{Argument, Invocation, Motion, Vim, VimResult},
};

fn key(c: char) -> KeyChord {
    KeyChord::new(Key::Character(c), Modifiers::default())
}

fn input(vim: &mut Vim, text: &str) -> Vec<Invocation> {
    let mut commands = Vec::new();
    for c in text.chars() {
        match vim.feed(key(c)) {
            VimResult::Pending => {}
            VimResult::Command(call) => {
                assert!(command(call.id).is_some(), "unregistered command: {}", call.id);
                commands.push(call);
            }
            other => panic!("{text:?} at {c:?}: {other:?}"),
        }
    }
    commands
}

fn only(text: &str) -> Invocation {
    let mut commands = input(&mut Vim::default(), text);
    assert_eq!(commands.len(), 1, "{text}");
    commands.pop().unwrap()
}

fn visual(call: &Invocation, motion: Motion, count: u32, extend: bool) {
    assert_eq!(call.id, motion.id());
    assert_eq!(call.motion, Some(motion));
    assert_eq!(call.visual_motion(), Some(motion));
    assert_eq!(call.count, count);
    assert_eq!(call.argument, Argument::VisualMotion { extend });
}

#[test]
fn g_prefix_emits_only_on_completed_visual_motion() {
    for (tail, motion) in [('j', Motion::Down), ('k', Motion::Up)] {
        let mut vim = Vim::default();
        assert_eq!(vim.feed(key('g')), VimResult::Pending);
        assert_eq!(vim.mode(), Mode::Normal);
        assert!(!vim.operator_pending());
        let VimResult::Command(call) = vim.feed(key(tail)) else { panic!("g{tail}") };
        visual(&call, motion, 1, false);
        assert!(!call.explicit_count);
        assert_eq!(call.register, None);
        assert_eq!(vim.mode(), Mode::Normal);
        let next = input(&mut vim, "j").pop().unwrap();
        assert_eq!(next.argument, Argument::None);
        assert_eq!(next.count, 1);
    }
}

#[test]
fn visual_counts_and_registers_survive_the_prefix_and_reset_after_emission() {
    for (text, motion, count, explicit, register) in [
        ("gj", Motion::Down, 1, false, None),
        ("1gk", Motion::Up, 1, true, None),
        ("3gj", Motion::Down, 3, true, None),
        ("3gk", Motion::Up, 3, true, None),
        ("4294967295gj", Motion::Down, u32::MAX, true, None),
        ("\"a3gj", Motion::Down, 3, true, Some('a')),
        ("3\"Agk", Motion::Up, 3, true, Some('A')),
        ("\"_2gk", Motion::Up, 2, true, Some('_')),
    ] {
        let mut vim = Vim::default();
        let call = input(&mut vim, text).pop().unwrap();
        visual(&call, motion, count, false);
        assert_eq!(call.explicit_count, explicit, "{text}");
        assert_eq!(call.register, register, "{text}");
        let next = input(&mut vim, "p").pop().unwrap();
        assert_eq!((next.count, next.explicit_count, next.register), (1, false, None));
        assert_eq!(next.argument, Argument::None);
    }
}

#[test]
fn logical_vertical_motions_remain_distinct_with_the_same_executable_ids() {
    for (logical, wrapped) in [("3j", "3gj"), ("3k", "3gk")] {
        let logical = only(logical);
        let wrapped = only(wrapped);
        assert_eq!(logical.id, wrapped.id, "unwrapped fallback keeps an implemented command");
        assert_eq!(logical.motion, wrapped.motion);
        assert_eq!(logical.count, wrapped.count);
        assert_eq!(logical.argument, Argument::None);
        assert_eq!(logical.visual_motion(), None);
        assert_ne!(logical, wrapped, "recorded intent must distinguish logical and visual rows");
    }
}

#[test]
fn visual_modes_capture_extension_for_both_directions() {
    for (enter, mode) in [("v", Mode::Visual), ("V", Mode::VisualLine)] {
        for (motion_keys, motion) in [("3gj", Motion::Down), ("3gk", Motion::Up)] {
            let mut vim = Vim::default();
            input(&mut vim, enter);
            let call = input(&mut vim, motion_keys).pop().unwrap();
            visual(&call, motion, 3, true);
            assert_eq!(vim.mode(), mode);
            assert!(!vim.operator_pending());
            let operation = input(&mut vim, "y").pop().unwrap();
            assert_eq!(operation.id, "operator.yank");
            assert_eq!(operation.motion, Some(if mode == Mode::Visual { Motion::Selection } else { Motion::Line }));
            assert_eq!(operation.argument, Argument::None);
            assert_eq!(operation.count, 1);
            assert_eq!(vim.mode(), Mode::Normal);
        }
    }
}

#[test]
fn first_line_prefix_and_logical_operator_ranges_are_unchanged() {
    for (text, id, motion, count, explicit) in [
        ("gg", "motion.firstLine", Motion::FirstLine, 1, false),
        ("3gg", "motion.firstLine", Motion::FirstLine, 3, true),
        ("dgg", "operator.delete", Motion::FirstLine, 1, false),
        ("2d3gg", "operator.delete", Motion::FirstLine, 6, true),
        ("3dj", "operator.delete", Motion::Down, 3, true),
        ("2y3k", "operator.yank", Motion::Up, 6, true),
        ("ck", "operator.change", Motion::Up, 1, false),
    ] {
        let call = only(text);
        assert_eq!((call.id, call.motion, call.count), (id, Some(motion), count), "{text}");
        assert_eq!(call.explicit_count, explicit, "{text}");
        assert_eq!(call.argument, Argument::None);
        assert_eq!(call.visual_motion(), None);
    }
}

#[test]
fn operator_visual_motions_refuse_without_emitting_any_partial_command_or_mode_change() {
    for operator in ['d', 'c', 'y'] {
        for tail in ['j', 'k'] {
            let mut vim = Vim::default();
            // Every earlier key must remain Pending: a frontend has received
            // no editor/register mutation to undo when the final key refuses.
            for c in format!("\"a2{operator}3g").chars() {
                assert_eq!(vim.feed(key(c)), VimResult::Pending);
            }
            assert!(vim.operator_pending());
            assert_eq!(
                vim.feed(key(tail)),
                VimResult::Error("visual-row motions are not supported for operators; use j or k".into())
            );
            assert!(!vim.operator_pending());
            assert_eq!(vim.mode(), Mode::Normal, "c must not enter Insert on refusal");
            let next = input(&mut vim, "gj").pop().unwrap();
            visual(&next, Motion::Down, 1, false);
            assert_eq!((next.explicit_count, next.register), (false, None));
        }
    }
}

#[test]
fn invalid_g_continuations_clear_pending_count_register_and_operator() {
    for prefix in ["\"a3g", "\"a2d3g"] {
        for tail in ['x', 'G', 'J', 'K', '3', '"', '.'] {
            let mut vim = Vim::default();
            for c in prefix.chars() {
                assert_eq!(vim.feed(key(c)), VimResult::Pending);
            }
            assert_eq!(vim.feed(key(tail)), VimResult::Error("expected gg, gj or gk".into()));
            assert_eq!(vim.mode(), Mode::Normal);
            assert!(!vim.operator_pending());
            let next = input(&mut vim, "k").pop().unwrap();
            assert_eq!((next.id, next.count, next.register), ("motion.up", 1, None));
            assert_eq!(next.argument, Argument::None);
        }
    }
}

#[test]
fn escape_focus_cancel_and_noncharacter_input_fence_the_visual_prefix() {
    for cancel in ["escape", "tab", "ctrl+j"] {
        let mut vim = Vim::default();
        for c in "\"a3g".chars() {
            assert_eq!(vim.feed(key(c)), VimResult::Pending);
        }
        let result = vim.feed(cancel.parse().unwrap());
        if cancel == "escape" {
            assert!(matches!(result, VimResult::Command(call) if call.id == "mode.normal"));
        } else {
            assert_eq!(result, VimResult::Unbound);
        }
        let next = input(&mut vim, "j").pop().unwrap();
        assert_eq!((next.count, next.register, next.visual_motion()), (1, None, None));
    }
    let mut vim = Vim::default();
    input(&mut vim, "v\"a3g");
    vim.cancel_input();
    assert_eq!(vim.mode(), Mode::Visual);
    let next = input(&mut vim, "gk").pop().unwrap();
    visual(&next, Motion::Up, 1, true);
    assert_eq!(next.register, None);
}

#[test]
fn overflow_cannot_leak_count_or_visual_intent_into_the_next_command() {
    let mut vim = Vim::default();
    for c in "\"a4294967295".chars() {
        assert_eq!(vim.feed(key(c)), VimResult::Pending);
    }
    assert_eq!(vim.feed(key('9')), VimResult::Error("count overflow".into()));
    let call = input(&mut vim, "gk").pop().unwrap();
    visual(&call, Motion::Up, 1, false);
    assert_eq!((call.explicit_count, call.register), (false, None));
}

#[test]
fn recorded_invocations_retain_visual_intent_and_dot_still_requests_the_completed_change() {
    let keys = "2dw\"a3gj2gk.3.";
    let recorded = input(&mut Vim::default(), keys);
    let replay = recorded.clone();
    assert_eq!(replay, input(&mut Vim::default(), keys));
    assert_eq!(replay.len(), 5);
    assert_eq!((replay[0].id, replay[0].count, replay[0].argument.clone()), ("operator.delete", 2, Argument::None));
    visual(&replay[1], Motion::Down, 3, false);
    assert_eq!(replay[1].register, Some('a'));
    visual(&replay[2], Motion::Up, 2, false);
    for (call, count) in replay[3..].iter().zip([1, 3]) {
        assert_eq!((call.id, call.count, call.motion, call.register), ("edit.repeat", count, None, None));
        assert_eq!(call.argument, Argument::None);
        assert_eq!(call.visual_motion(), None);
    }
}

#[test]
fn keymap_engine_exposes_visual_intent_and_cancels_it_on_context_changes() {
    let mut keys = KeymapEngine::new(Preset::Vim, &[]).unwrap();
    for c in "3g".chars() {
        assert_eq!(keys.feed(key(c)), VimResult::Pending);
    }
    let VimResult::Command(call) = keys.feed(key('j')) else { panic!("3gj") };
    visual(&call, Motion::Down, 3, false);
    for c in "\"a2g".chars() {
        assert_eq!(keys.feed(key(c)), VimResult::Pending);
    }
    keys.set_context(Context::Palette);
    keys.set_context(Context::Editor);
    let VimResult::Command(call) = keys.feed(key('k')) else { panic!("logical k") };
    assert_eq!((call.count, call.register, call.visual_motion()), (1, None, None));
    assert_eq!(call.argument, Argument::None);
}

#[test]
fn explicit_bindings_keep_precedence_and_never_invent_visual_metadata() {
    let binding = Binding {
        keys: "j".into(),
        command: "edit.undo".into(),
        context: Some(Context::Editor),
        mode: Some(Mode::Normal),
    };
    let mut keys = KeymapEngine::new(Preset::Vim, &[binding]).unwrap();
    assert_eq!(keys.feed(key('g')), VimResult::Pending);
    let VimResult::Command(call) = keys.feed(key('j')) else { panic!("explicit override") };
    assert_eq!(call.id, "edit.undo");
    assert_eq!(call.argument, Argument::None);
    let VimResult::Command(call) = keys.feed(key('k')) else { panic!("prefix cancelled") };
    assert_eq!(call.visual_motion(), None);
    let VimResult::Command(call) = keys.invoke_named("motion.down") else { panic!("named motion") };
    assert_eq!(call.argument, Argument::None);
    assert_eq!(call.visual_motion(), None);
}

#[test]
fn insert_search_and_substitute_do_not_parse_visual_motion_text() {
    let mut vim = Vim::default();
    input(&mut vim, "i");
    for c in "gjgk".chars() {
        assert_eq!(vim.feed(key(c)), VimResult::Unbound);
    }
    vim.feed("escape".parse().unwrap());
    for (text, expected) in [
        ("/gjgk", Argument::Search("gjgk".into())),
        (
            ":s/gj/gk/g",
            Argument::Substitute { pattern: "gj".into(), replacement: "gk".into(), global: true, whole_file: false },
        ),
    ] {
        assert!(input(&mut vim, text).is_empty());
        let VimResult::Command(call) = vim.feed("enter".parse().unwrap()) else { panic!("{text}") };
        assert_eq!(call.argument, expected);
        assert_eq!(call.visual_motion(), None);
        assert_eq!(vim.mode(), Mode::Normal);
    }
}

#[test]
fn visual_accessor_never_classifies_operator_or_mismatched_payload_as_navigation() {
    let mut call = only("gj");
    call.id = "operator.delete";
    assert_eq!(call.visual_motion(), None);
    call.id = "motion.down";
    call.motion = Some(Motion::Up);
    assert_eq!(call.visual_motion(), None);
    call.motion = None;
    assert_eq!(call.visual_motion(), None);
    call.motion = Some(Motion::Down);
    call.argument = Argument::None;
    assert_eq!(call.visual_motion(), None);
}

#[test]
fn wrap_retry_is_registered_without_a_default_binding_and_respects_custom_tombstones() {
    let retry = command("view.retryWrap").expect("shared frontend recovery command");
    assert_eq!((retry.title, retry.category), ("Retry Wrapped View", "View"));
    for preset in [Preset::Vim, Preset::VsCode, Preset::NotepadPlusPlus] {
        let keymap = kwil_core_commands::Keymap::preset(preset).unwrap();
        assert!(!keymap.bindings().any(|binding| binding.command == retry.id));
        let mut keys = KeymapEngine::new(preset, &[]).unwrap();
        let VimResult::Command(call) = keys.invoke_named(retry.id) else { panic!("palette recovery") };
        assert_eq!(call.id, retry.id);
        assert_eq!(call.argument, Argument::None);
    }
    let mut binding =
        Binding { keys: "ctrl+alt+r".into(), command: retry.id.into(), context: Some(Context::Editor), mode: None };
    let chord: KeyChord = "ctrl+alt+r".parse().unwrap();
    let mut keys = KeymapEngine::new(Preset::VsCode, &[binding.clone()]).unwrap();
    assert!(matches!(keys.feed(chord), VimResult::Command(call) if call.id == retry.id));
    binding.command.clear();
    let mut keys = KeymapEngine::new(Preset::VsCode, &[binding]).unwrap();
    assert_eq!(keys.feed(chord), VimResult::Unbound);
    assert!(keys.suppresses_fallback());
}
