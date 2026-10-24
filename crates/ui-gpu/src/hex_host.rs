//! Live GUI hex orchestration. Workspace retains every Session, page, editor
//! loan and worker fence. This host retains only input/geometry scalar state.
use super::*;
use kwil_view_model::hex_session::{Input, Outcome, Source};

const COLUMNS: u64 = (kwil_view_model::hex::ASCII_COLUMN + 16) as u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct InputOrigin {
    source: Source,
    input_epoch: u64,
    workspace_epoch: u64,
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        path::PathBuf,
        sync::mpsc::Receiver,
        time::{Duration, Instant},
    };
    fn dispatch(app: &mut App, event: UserEvent) {
        match event {
            UserEvent::Document(result) => {
                let damage = app.view.document_result(*result);
                app.apply(damage);
            }
            UserEvent::Notice(notice) => {
                let damage = app.view.set_notice(notice);
                app.apply(damage);
            }
            _ => panic!("unexpected hex host test wake"),
        }
    }
    fn pending_state(app: &App) -> String {
        let workspace = &app.view;
        let id = workspace.active_id();
        let editor = workspace.editor().map(|editor| {
            (
                editor.document().revision(),
                editor.document().saved_revision(),
                editor.document().is_dirty(),
                editor.next_edit_sequence(),
                editor.has_pending_edit_receipt(),
                editor.has_pending_save_completion(),
            )
        });
        let session = workspace.hex_session().map(|session| {
            (
                session.source(),
                session.busy(),
                session.input_epoch(),
                session.view().top(),
                session.view().cursor(),
                session.view().pending_high_nibble(),
            )
        });
        format!(
            "document={id:?} workspace_epoch={} hex_active={} document_busy={} saves_pending={} replacement_pending={} recovery_ready={} recovery_notice={:?} wants_exit={} close_pending={} prompt={:?} editor(revision,saved,dirty,next_sequence,receipt_pending,save_completion_pending)={editor:?} session(source,busy,input_epoch,top,cursor,nibble)={session:?} notice={:?}",
            workspace.input_epoch(),
            workspace.hex_active(),
            workspace.document_busy(id),
            workspace.file_saves_pending(),
            workspace.search_replacement_pending(),
            workspace.recovery_ready(),
            workspace.recovery_notice(),
            workspace.wants_exit(),
            workspace.close_pending(),
            workspace.file_prompt().map(|prompt| (prompt.kind, prompt.document)),
            workspace.notice()
        )
    }
    fn wake_description(event: &UserEvent) -> String {
        use kwil_view_model::document_worker::DocumentResult as R;
        let UserEvent::Document(result) = event else {
            return match event {
                UserEvent::Notice(notice) => format!("Notice({notice:?})"),
                _ => "non-document wake".into(),
            };
        };
        match &**result {
            R::RawPreview { document, revision, page } => {
                format!("RawPreview document={document:?} revision={revision} request={:?}", page.request)
            }
            R::RawReopened { document, generation, result, .. } => format!(
                "RawReopened document={document:?} generation={generation} result={:?}",
                result.as_ref().map_err(ToString::to_string)
            ),
            R::HexPage { document, request, result, .. } => format!(
                "HexPage document={document:?} request={request:?} result={:?}",
                result.as_ref().map(|_| ()).map_err(ToString::to_string)
            ),
            R::HexEdited { document, completion, .. } => format!(
                "HexEdited document={document:?} operation={} before={:?} after={:?} result={:?}",
                completion.operation, completion.before, completion.after, completion.result
            ),
            R::Saved { document, revision, result } => format!(
                "Saved document={document:?} revision={revision} result={:?}",
                result.as_ref().map(|_| ()).map_err(ToString::to_string)
            ),
            R::Preview { document, revision, .. } => format!("TextPreview document={document:?} revision={revision}"),
            R::Indexed { document, revision, result } => format!(
                "Indexed document={document:?} revision={revision} result={:?}",
                result.as_ref().map(|_| ()).map_err(ToString::to_string)
            ),
            R::JournalQueued { document, revision, sequence, result } => format!(
                "JournalQueued document={document:?} revision={revision} sequence={sequence} result={:?}",
                result.as_ref().map_err(ToString::to_string)
            ),
            R::JournalDurable { document, revision, sequence, result } => format!(
                "JournalDurable document={document:?} revision={revision} sequence={sequence} result={:?}",
                result.as_ref().map_err(ToString::to_string)
            ),
            R::JournalClosed { document, result } => {
                format!("JournalClosed document={document:?} result={:?}", result.as_ref().map_err(ToString::to_string))
            }
            R::Prepared(result) => {
                format!("Prepared result={:?}", result.as_ref().map(|_| ()).map_err(ToString::to_string))
            }
            R::Persisted(result) => format!("Persisted result={:?}", result.as_ref().map_err(ToString::to_string)),
            R::Stopped(result) => format!("Stopped result={:?}", result.as_ref().map_err(ToString::to_string)),
            R::SessionCheckpointDue => "SessionCheckpointDue".into(),
            R::CapacityAvailable => "CapacityAvailable".into(),
            _ => "other document result".into(),
        }
    }
    fn until(app: &mut App, rx: &Receiver<UserEvent>, stage: &'static str, done: impl Fn(&App) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut recent = VecDeque::with_capacity(24);
        eprintln!("hex-host stage={stage} begin {}", pending_state(app));
        while !done(app) {
            let remaining = deadline.checked_duration_since(Instant::now()).unwrap_or_else(|| {
                panic!("hex-host stage={stage} deadline expired; {}; recent wakes={recent:#?}", pending_state(app))
            });
            let event = rx.recv_timeout(remaining).unwrap_or_else(|error| {
                panic!("hex-host stage={stage} worker wake {error:?}; {}; recent wakes={recent:#?}", pending_state(app))
            });
            let description = wake_description(&event);
            dispatch(app, event);
            if recent.len() == 24 {
                recent.pop_front();
            }
            recent.push_back(format!("{description}; after {}", pending_state(app)));
        }
        eprintln!("hex-host stage={stage} complete {}", pending_state(app));
    }
    fn host() -> (App, Receiver<UserEvent>, PathBuf, PathBuf) {
        let (mut app, rx) = search_ui::host_dispatch_tests::host("");
        app.opts.hidden = true;
        app.size = (800, 240);
        let root = std::env::temp_dir().join(format!(
            "kwil-gui-hex-host-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("physical.bin");
        let mut content = vec![b'x'; 1024];
        content[0] = 0x12;
        content[1] = 0xff;
        content[2] = 0;
        std::fs::write(&path, content).unwrap();
        let proxy = app.proxy.clone();
        app.view
            .start_document_worker_at(Some(root.join("recovery")), move |result| {
                let _ = proxy.send_event(UserEvent::Document(Box::new(result)));
            })
            .unwrap();
        until(&mut app, &rx, "fixture/prepare-recovery", |a| a.view.recovery_ready());
        app.view.open_hex_path(path.clone(), 3).unwrap();
        app.apply(kwil_view_model::hex_session::body_damage());
        idle(&mut app, &rx, "fixture/raw-open-page-and-recovery");
        assert!(app.view.hex_active());
        assert!(app.view.hex_session().unwrap().row(app.view.hex_source(), 0).is_some());
        (app, rx, root, path)
    }
    fn idle(app: &mut App, rx: &Receiver<UserEvent>, stage: &'static str) {
        until(app, rx, stage, |a| !a.view.document_busy(a.view.active_id()) && a.view.recovery_notice().is_none());
    }
    fn press(app: &mut App, digit: &str) {
        app.mods = ModifiersState::empty();
        app.key_after_settings(
            None,
            &HostKey { logical_key: Key::Character(digit.into()), text: Some(digit), repeat: false },
        );
    }
    fn data(app: &App) -> Vec<u8> {
        let mut data = Vec::new();
        app.view.editor().unwrap().document().snapshot().write_to(&mut data).unwrap();
        data
    }
    fn stop(mut app: App, rx: Receiver<UserEvent>, root: PathBuf) {
        idle(&mut app, &rx, "shutdown/pre-idle");
        app.view.request_window_close(true).unwrap();
        until(&mut app, &rx, "shutdown/exit-intent-and-save-drain", |a| {
            a.view.wants_exit() && !a.view.file_saves_pending()
        });
        let admission_deadline = Instant::now() + Duration::from_secs(10);
        eprintln!("hex-host stage=shutdown/raw-editor-retirement begin {}", pending_state(&app));
        while !app.may_exit() {
            let remaining = admission_deadline.checked_duration_since(Instant::now()).unwrap_or_else(|| {
                panic!("hex-host stage=shutdown/raw-editor-retirement deadline expired; {}", pending_state(&app))
            });
            let event = rx.recv_timeout(remaining).unwrap_or_else(|error| {
                panic!("hex-host stage=shutdown/raw-editor-retirement wake {error:?}; {}", pending_state(&app))
            });
            eprintln!("hex-host stage=shutdown/raw-editor-retirement wake {}", wake_description(&event));
            dispatch(&mut app, event);
        }
        let before_drop = pending_state(&app);
        eprintln!("hex-host stage=shutdown/worker-stopped begin {before_drop}");
        drop(app);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let remaining = deadline.checked_duration_since(Instant::now()).unwrap_or_else(|| {
                panic!("hex-host stage=shutdown/worker-stopped deadline expired; before drop {before_drop}")
            });
            let event = rx.recv_timeout(remaining).unwrap_or_else(|error| {
                panic!("hex-host stage=shutdown/worker-stopped wake {error:?}; before drop {before_drop}")
            });
            eprintln!("hex-host stage=shutdown/worker-stopped wake {}", wake_description(&event));
            if let UserEvent::Document(result) = event
                && let kwil_view_model::document_worker::DocumentResult::Stopped(result) = *result
            {
                result.unwrap();
                break;
            }
        }
        assert_eq!(root.parent(), Some(std::env::temp_dir().as_path()));
        assert!(root.file_name().unwrap().to_string_lossy().starts_with("kwil-gui-hex-host-"));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn actual_gui_byte_keys_hold_cached_raw_body_during_worker_loan_and_share_undo() {
        let (mut app, rx, root, _) = host();
        let original = data(&app);
        press(&mut app, "f");
        assert_eq!(data(&app), original);
        press(&mut app, "f");
        assert!(app.view.hex_active());
        assert!(app.hex_read_only());
        assert!(app.view.editor().is_none());
        assert_eq!(app.view.hex_session().unwrap().row(app.view.hex_source(), 0).unwrap().bytes[0], 0x12);
        press(&mut app, "0"); // Must be consumed while the editor is loaned.
        idle(&mut app, &rx, "byte/overwrite-and-recovery-drain");
        assert_eq!(data(&app)[0], 0xff);
        assert!(app.dispatch_hex("edit.undo"));
        idle(&mut app, &rx, "byte/undo-and-recovery-drain");
        assert_eq!(data(&app), original);
        stop(app, rx, root);
    }
    #[test]
    fn actual_gui_blocks_text_commands_preserves_dirty_toggle_and_fences_ime() {
        let (mut app, rx, root, path) = host();
        for command in [
            "search.find",
            "search.replace",
            "edit.formatDocument",
            "file.setEncoding",
            "file.setLineEnding",
            "file.setType",
            "view.logFilters",
        ] {
            app.edit(input::Action::Command(command));
            assert!(!app.search_panel.is_open());
        }
        let origin = app.hex_origin().unwrap();
        app.hex_preedit("F");
        assert!(app.hex_ime_pending());
        app.view.cancel_hex_input();
        app.hex_commit("F");
        assert_eq!(data(&app)[0], 0x12);
        app.hex_commit_text(origin, "00", true);
        assert_eq!(data(&app)[0], 0x12);
        press(&mut app, "a");
        press(&mut app, "b");
        idle(&mut app, &rx, "ime/byte-edit-and-recovery-drain");
        assert_eq!(data(&app)[0], 0xab);
        assert!(app.dispatch_hex("view.toggleHex"));
        assert!(app.view.hex_active());
        assert_eq!(data(&app)[0], 0xab);
        assert!(app.view.notice().contains("Save"));
        app.edit(input::Action::Command("file.save"));
        until(&mut app, &rx, "ime/raw-save-completion", |a| !a.view.file_saves_pending());
        let saved = std::fs::read(&path).unwrap();
        assert_eq!(saved, data(&app), "raw atomic save must preserve every untouched byte");
        assert!(app.dispatch_hex("view.toggleHex"));
        until(&mut app, &rx, "ime/clean-toggle-to-text", |a| {
            !a.view.hex_active() && !a.view.document_busy(a.view.active_id())
        });
        stop(app, rx, root);
    }
    #[test]
    fn actual_gui_scroll_keeps_cached_hex_and_chrome_shortcuts_during_page_loan() {
        let (mut app, rx, root, _) = host();
        app.size = (240, 240);
        app.apply(kwil_view_model::hex_session::body_damage());
        idle(&mut app, &rx, "scroll/resize-page-drain");
        assert!(app.hex_wheel(1));
        assert!(app.view.hex_active());
        assert!(app.hex_read_only());
        assert!(app.view.editor().is_none());
        assert_eq!(app.view.hex_session().unwrap().row(app.view.hex_source(), 0).unwrap().offset, 16);
        assert!(app.hex_status().contains("Read-only"));
        app.mods = ModifiersState::CONTROL;
        for key in [
            Key::Character("s".into()),
            Key::Character("o".into()),
            Key::Character("p".into()),
            Key::Named(NamedKey::Tab),
        ] {
            assert!(
                !app.hex_key(&HostKey { logical_key: key, text: None, repeat: false }),
                "chrome must not be consumed by an editor loan"
            );
        }
        assert!(app.hex_scroll_bar(false, 5));
        assert_eq!(app.hex.horizontal, 5);
        idle(&mut app, &rx, "scroll/page-and-recovery-drain");
        assert_eq!(app.view.hex_session().unwrap().row(app.view.hex_source(), 0).unwrap().offset, 16);
        stop(app, rx, root);
    }
    #[test]
    fn actual_gui_escape_cancels_close_wait_before_hex_page_loan_returns() {
        let (mut app, rx, root, _) = host();
        let raw = app.view.active_id();
        let original = data(&app);
        assert!(app.hex_wheel(1));
        assert!(app.view.document_busy(raw));
        assert!(app.dispatch_file("file.close"));
        assert!(app.view.close_pending(), "close must be waiting for the exact editor loan");
        app.mods = ModifiersState::empty();
        app.key_after_settings(None, &HostKey { logical_key: Key::Named(NamedKey::Escape), text: None, repeat: false });
        assert!(!app.view.close_pending(), "Escape cancels the host close intent before hex fallback");
        until(&mut app, &rx, "escape-close/returned-page-loan", |a| !a.view.document_busy(raw));
        assert!(
            app.view.session().tabs().iter().any(|tab| tab.id == raw),
            "returned editor must not close the cancelled tab"
        );
        assert_eq!(app.view.active_id(), raw);
        assert!(app.view.hex_active());
        assert_eq!(data(&app), original);
        stop(app, rx, root);
    }
    #[test]
    fn actual_gui_configured_chrome_binding_works_idle_and_during_hex_page_loan() {
        use kwil_core_commands::{Binding, Context, Preset, keymap::KeymapEngine};
        for context in [None, Some(Context::Hex)] {
            for loaned in [false, true] {
                let (mut app, rx, root, _) = host();
                let raw = app.view.active_id();
                let original = data(&app);
                let binding = Binding { keys: "ctrl+b".into(), command: "file.new".into(), context, mode: None };
                app.settings.bindings = vec![binding];
                app.keys = Some(KeymapEngine::new(Preset::VsCode, &app.settings.bindings).unwrap());
                if loaned {
                    assert!(app.hex_wheel(1));
                    assert!(app.view.document_busy(raw));
                }
                app.mods = ModifiersState::CONTROL;
                app.key_after_settings(
                    None,
                    &HostKey { logical_key: Key::Character("b".into()), text: None, repeat: false },
                );
                let created = app.view.active_id();
                assert_ne!(
                    created, raw,
                    "effective configured chrome command must execute in hex context={context:?} loaned={loaned}"
                );
                assert!(app.view.session().tabs().iter().any(|tab| tab.id == raw));
                assert!(!app.view.hex_active());
                assert_eq!(app.view.editor().unwrap().document().revision(), 0);
                until(&mut app, &rx, "configured-chrome/inactive-raw-loan-return", |a| !a.view.document_busy(raw));
                assert_eq!(app.view.active_id(), created, "inactive page result must not steal the new tab");
                assert!(app.view.activate(raw));
                app.apply(kwil_view_model::hex_session::body_damage());
                idle(&mut app, &rx, "configured-chrome/return-to-raw-tab");
                assert_eq!(data(&app), original);
                stop(app, rx, root);
            }
        }
    }
    #[test]
    fn actual_gui_hex_keymap_preserves_tombstones_chords_vim_and_readonly_body_fences() {
        use kwil_core_commands::{Binding, Context, Preset, keymap::KeymapEngine};
        for preset in [Preset::VsCode, Preset::Vim] {
            let (mut app, rx, root, _) = host();
            let raw = app.view.active_id();
            let original = data(&app);
            let binding = |keys: &str, command: &str, context| Binding {
                keys: keys.into(),
                command: command.into(),
                context,
                mode: None,
            };
            app.settings.keymap = preset;
            app.settings.bindings = vec![
                binding("ctrl+b", "file.new", None),
                binding("ctrl+b", "", Some(Context::Hex)),
                binding("f", "", Some(Context::Hex)),
                binding("ctrl+k ctrl+n", "file.new", Some(Context::Hex)),
                binding("ctrl+k ctrl+x", "", Some(Context::Hex)),
                binding("ctrl+r", "edit.insert", Some(Context::Hex)),
            ];
            app.keys = Some(KeymapEngine::new(preset, &app.settings.bindings).unwrap());
            let mode = app.keys.as_ref().unwrap().mode();
            let ctrl = |app: &mut App, key: &str| {
                app.mods = ModifiersState::CONTROL;
                app.key_after_settings(
                    None,
                    &HostKey { logical_key: Key::Character(key.into()), text: None, repeat: false },
                );
            };
            ctrl(&mut app, "b");
            assert_eq!(app.view.active_id(), raw, "Hex tombstone overrides global chrome binding");
            press(&mut app, "f");
            assert_eq!(
                app.view.hex_session().unwrap().view().pending_high_nibble(),
                None,
                "explicit unbind suppresses hex digit fallback"
            );
            ctrl(&mut app, "k");
            assert_eq!(app.view.active_id(), raw, "a configured live chord prefix must wait");
            ctrl(&mut app, "x");
            assert_eq!(app.view.active_id(), raw, "chord tombstone must suppress continuation restart");
            ctrl(&mut app, "r");
            assert_eq!(data(&app), original, "configured text-body command must pass through the raw command fence");
            assert_eq!(app.keys.as_ref().unwrap().mode(), mode, "hex dispatch must not feed Vim text grammar");
            assert!(app.hex_wheel(1));
            assert!(app.view.document_busy(raw));
            press(&mut app, "a");
            assert_eq!(
                app.view.hex_session().unwrap().view().pending_high_nibble(),
                None,
                "page-loan body stays read-only after keymap resolution"
            );
            idle(&mut app, &rx, "hex-keymap/page-loan-readonly-drain");
            assert_eq!(data(&app), original);
            ctrl(&mut app, "k");
            ctrl(&mut app, "n");
            assert_ne!(app.view.active_id(), raw, "live configured chord must execute its chrome command");
            assert!(app.view.activate(raw));
            app.apply(kwil_view_model::hex_session::body_damage());
            idle(&mut app, &rx, "hex-keymap/return-to-raw-tab");
            assert_eq!(data(&app), original);
            stop(app, rx, root);
        }
    }
    fn configure_motion(app: &mut App, command: &str) {
        use kwil_core_commands::{Binding, Context, keymap::KeymapEngine};
        app.settings.bindings = ["ctrl+b", "ctrl+shift+b"]
            .into_iter()
            .map(|keys| Binding { keys: keys.into(), command: command.into(), context: Some(Context::Hex), mode: None })
            .collect();
        app.keys = Some(KeymapEngine::new(app.settings.keymap, &app.settings.bindings).unwrap());
    }
    fn invoke_motion(app: &mut App, extend: bool) {
        app.mods = ModifiersState::CONTROL | if extend { ModifiersState::SHIFT } else { ModifiersState::empty() };
        app.key_after_settings(None, &HostKey { logical_key: Key::Character("b".into()), text: None, repeat: false });
    }
    fn position(app: &mut App, rx: &Receiver<UserEvent>, at: u64) {
        for input in [Input::Home { document: true, extend: false }, Input::Move { bytes: at as i64, extend: false }] {
            let session = app.view.hex_session().unwrap();
            let (source, epoch) = (session.source().unwrap(), session.input_epoch());
            let damage = app.view.hex_input(source, epoch, input).unwrap();
            app.apply(damage);
        }
        idle(app, rx, "configured-motion/position-and-page-drain");
    }
    #[test]
    fn actual_gui_configured_byte_motions_and_shift_selection_match_hex_coordinates() {
        let (mut app, rx, root, _) = host();
        let original = data(&app);
        let rows = app.view.hex_session().unwrap().view().rows() as u64;
        let page = rows.max(1) * 16;
        let len = app.view.hex_source().unwrap().len;
        for (command, start, target) in [
            ("motion.left", 33, 32),
            ("motion.right", 33, 34),
            ("motion.up", 33, 17),
            ("motion.down", 33, 49),
            ("motion.lineStart", 33, 32),
            ("motion.smartHome", 33, 32),
            ("motion.lineEnd", 33, 47),
            ("motion.pageUp", page + 33, 33),
            ("motion.pageDown", 33, page + 33),
            ("motion.firstLine", 33, 0),
            ("motion.lastLine", 33, len),
        ] {
            for extend in [false, true] {
                position(&mut app, &rx, start);
                configure_motion(&mut app, command);
                invoke_motion(&mut app, extend);
                idle(&mut app, &rx, "configured-motion/command-and-page-drain");
                let view = app.view.hex_session().unwrap().view();
                assert_eq!(view.cursor(), target, "configured {command}, extend={extend}");
                let selected = if extend { start.min(target)..start.max(target) } else { target..target };
                assert_eq!(view.selection(), selected, "configured {command} must preserve Shift anchor semantics");
                assert_eq!(data(&app), original, "byte navigation must not mutate the document");
            }
        }
        for forbidden in [
            "motion.wordForward",
            "motion.wordBackward",
            "motion.wordEnd",
            "motion.innerWord",
            "navigation.goToLine",
            "edit.newline",
        ] {
            position(&mut app, &rx, 33);
            configure_motion(&mut app, forbidden);
            invoke_motion(&mut app, false);
            assert_eq!(app.view.hex_session().unwrap().view().cursor(), 33, "{forbidden} is not a byte motion");
            assert_eq!(data(&app), original);
        }
        stop(app, rx, root);
    }
    #[test]
    fn actual_gui_configured_motion_fences_stale_page_input_and_edit_loans() {
        let (mut app, rx, root, _) = host();
        let original = data(&app);
        position(&mut app, &rx, 33);
        let stale_origin = app.hex_origin().unwrap();
        assert!(app.hex_wheel(1));
        assert!(app.view.document_busy(app.view.active_id()));
        configure_motion(&mut app, "motion.lastLine");
        invoke_motion(&mut app, true);
        let len = app.view.hex_source().unwrap().len;
        assert_eq!(app.view.hex_session().unwrap().view().cursor(), len, "Page loan permits bounded scalar navigation");
        assert_eq!(app.view.hex_session().unwrap().view().selection(), 33..len);
        app.hex_commit_text(stale_origin, "00", true);
        assert!(app.dispatch_hex("edit.delete"));
        assert!(!app.view.hex_session().unwrap().busy(), "page-loan mutation must not be admitted");
        idle(&mut app, &rx, "configured-motion/stale-page-and-latest-page-drain");
        let session = app.view.hex_session().unwrap();
        assert_eq!(session.view().cursor(), len, "old page completion cannot rewind the cursor");
        assert_eq!(session.row(app.view.hex_source(), 0).unwrap().offset, session.view().top() * 16);
        assert_eq!(data(&app), original, "stale IME/paste and loaned delete must not mutate bytes");
        position(&mut app, &rx, 33);
        let origin = app.hex_origin().unwrap();
        app.hex_commit_text(origin, "AB", true);
        assert!(app.view.hex_session().unwrap().busy());
        assert!(app.view.editor().is_none());
        configure_motion(&mut app, "motion.right");
        invoke_motion(&mut app, false);
        assert_eq!(
            app.view.hex_session().unwrap().view().cursor(),
            33,
            "Edit loan must consume navigation without ordinary Editor fallback"
        );
        idle(&mut app, &rx, "configured-motion/edit-loan-return");
        assert_eq!(app.view.hex_session().unwrap().view().cursor(), 34, "only the admitted edit advances the cursor");
        let mut expected = original.clone();
        expected[33] = 0xab;
        assert_eq!(data(&app), expected);
        assert!(app.dispatch_hex("edit.undo"));
        idle(&mut app, &rx, "configured-motion/cleanup-undo");
        assert_eq!(data(&app), original);
        stop(app, rx, root);
    }
}
#[derive(Default)]
pub(super) struct Host {
    identity: Option<(kwil_view_model::SessionDocumentId, u64, bool)>,
    pub horizontal: u64,
    read_only: bool,
    blocked: bool,
    ime: Option<InputOrigin>,
}

fn chrome_command(id: &str) -> bool {
    (id.starts_with("file.") && !matches!(id, "file.setEncoding" | "file.setLineEnding" | "file.setType"))
        || id.starts_with("settings.")
        || id.starts_with("view.zoom")
        || matches!(
            id,
            "view.commandPalette"
                | "view.fileFinder"
                | "view.quickOpen"
                | "view.switchTab"
                | "view.settings"
                | "view.toggleHex"
        )
}
fn blocked_command(id: &str) -> bool {
    !chrome_command(id)
        && !matches!(id, "edit.undo" | "edit.redo" | "edit.selectAll" | "edit.delete" | "edit.backspace" | "edit.paste")
}
fn editing_key(key: &Key, mods: ModifiersState) -> bool {
    match key {
        Key::Character(c) if mods.control_key() || mods.super_key() => {
            matches!(c.to_ascii_lowercase().as_str(), "z" | "y" | "v" | "x")
        }
        Key::Character(_) => !mods.alt_key(),
        Key::Named(NamedKey::Delete | NamedKey::Backspace) => true,
        _ => false,
    }
}
impl App {
    pub(super) fn hex_body_blocked(&self) -> bool {
        self.file_modal()
            || self.save_path.is_some()
            || self.view.file_prompt().is_some()
            || self.settings_open()
            || self.palette_open()
            || self.search_panel.is_open()
            || self.menu.state.is_open()
            || self.context.is_open()
            || self.language_owns_input()
            || self.welcome.visible()
    }
    fn hex_read_only(&self) -> bool {
        self.view.document_busy(self.view.active_id())
            || self.view.hex_session().is_none_or(|s| !s.is_open() || s.busy())
    }
    pub(super) fn sync_hex_host(&mut self) -> Damage {
        let active = self.view.hex_active();
        let identity = (self.view.active_id(), self.view.input_epoch(), active);
        let changed = self.hex.identity != Some(identity);
        let read_only = active && self.hex_read_only();
        let blocked = active && self.hex_body_blocked();
        let readonly_changed = read_only != self.hex.read_only;
        let blocked_changed = blocked != self.hex.blocked;
        // Set first: closing another overlay can request an immediate apply.
        self.hex.identity = Some(identity);
        self.hex.read_only = read_only;
        self.hex.blocked = blocked;
        if changed || blocked_changed {
            self.hex.ime = None;
            self.view.cancel_hex_input();
            self.wheel.clear();
            if changed {
                self.hex.horizontal = 0;
                self.selecting = false;
                self.drag = None;
                if !ime::editor_destination_is_current(&self.view, &self.ime_destination) {
                    self.composing = false;
                    self.ime_destination = Some(ime::Destination::Cancelled);
                }
            }
        }
        if changed && active {
            self.close_search();
            self.close_logs();
            self.close_language();
            self.close_context();
            if self.markdown_mode() != markdown_ui::Mode::Text {
                self.dispatch_markdown("view.markdownText");
            }
            if let Some(keys) = &mut self.keys {
                keys.cancel_pending();
            }
        }
        if active {
            let rows = self.text_rows();
            if self.view.hex_session().is_some_and(|s| s.view().rows() != rows.min(kwil_view_model::hex::MAX_ROWS)) {
                self.view.set_hex_rows(rows);
                return kwil_view_model::hex_session::body_damage();
            }
        }
        if changed {
            kwil_view_model::hex_session::body_damage()
        } else if active && (readonly_changed || blocked_changed) {
            let rows = self
                .view
                .hex_session()
                .and_then(|s| {
                    (s.view().cursor() / 16).checked_sub(s.view().top()).filter(|&r| r < s.view().rows() as u64)
                })
                .map_or_else(Vec::new, |r| vec![r as usize]);
            Damage { rows, status: true, ..Default::default() }
        } else {
            Damage::default()
        }
    }
    pub(super) fn hex_status(&self) -> String {
        let mut status =
            self.view.hex_session().map_or_else(|| "HEX · Preparing physical bytes".into(), |s| s.status());
        if self.hex_read_only() {
            status.push_str("  |  Read-only while worker is pending");
        }
        let notice = self.view.notice();
        if !notice.is_empty() {
            status.push_str("  |  ");
            status.push_str(notice);
        }
        if let Some(recovery) = self.view.recovery_notice() {
            status.push_str("  |  ");
            status.push_str(recovery);
        }
        status
    }
    fn hex_notice(&mut self, notice: impl Into<String>) {
        let damage = self.view.set_notice(notice.into());
        self.apply(damage);
    }
    fn hex_outcome(&mut self, result: Result<Outcome, String>) {
        match result.and_then(|outcome| self.view.submit_hex_outcome(outcome).map_err(|e| e.to_string())) {
            Ok(damage) => self.apply(damage),
            Err(error) => self.hex_notice(error),
        }
    }
    pub(super) fn dispatch_hex(&mut self, id: &str) -> bool {
        if matches!(id, "file.openHex" | "view.toggleHex") {
            self.hex_cancel_input();
            self.close_search();
            self.close_logs();
            self.close_language();
            self.close_context();
            return self.dispatch_file(id);
        }
        if !self.view.hex_active() {
            return false;
        }
        if chrome_command(id) {
            return false;
        }
        let rows = self.view.hex_session().map_or(0, |session| session.view().rows());
        let extend = self.mods.shift_key();
        if let Some(input) = kwil_view_model::hex_session::command_input(id, rows, extend) {
            // Match physical navigation: a Page loan permits scalar cursor/
            // viewport changes while its immutable old page stays visible.
            // Mutations remain read-only, and Edit/Reopen loans lend no mutable
            // Session at all. Never route any of these to an ordinary Editor.
            let navigation = matches!(input, Input::Move { .. } | Input::Home { .. } | Input::End { .. });
            if !navigation && self.hex_read_only() {
                self.hex_notice("Physical bytes are read-only while the document worker is pending");
                return true;
            }
            let result = self.view.hex_session_mut().and_then(|session| hex_ui::command(session, id, extend));
            if let Some(result) = result {
                self.hex_outcome(result);
            } else {
                self.hex_notice("Hex worker is busy; body input was not applied");
            }
            return true;
        }
        if blocked_command(id) {
            self.hex_notice("Text search, formatting, encoding and text commands are unavailable for physical bytes");
            return true;
        }
        if self.hex_read_only() {
            self.hex_notice("Physical bytes are read-only while the document worker is pending");
            return true;
        }
        if id == "edit.paste" {
            self.hex_clipboard_paste();
            return true;
        }
        true
    }
    /// The complete ordinary edit path also enters here (including text/IME
    /// fallback). No hidden text editor command can escape raw ownership.
    pub(super) fn hex_edit(&mut self, action: &input::Action) -> bool {
        match action {
            input::Action::Command(id) => self.dispatch_hex(id),
            input::Action::Text(text) if self.view.hex_active() => {
                if let Some(origin) = self.hex_origin() {
                    self.hex_commit_text(origin, text, false);
                }
                true
            }
            _ => false,
        }
    }
    pub(super) fn hex_key(&mut self, event: &HostKey<'_>) -> bool {
        // Fallback only: key_after_settings resolves the effective Hex-context
        // keymap first, including configured chrome, prefixes and tombstones.
        if !self.view.hex_active() || self.hex_body_blocked() {
            return false;
        }
        let ctrl = self.mods.control_key() || self.mods.super_key();
        if ctrl && matches!(&event.logical_key, Key::Character(c) if c.eq_ignore_ascii_case("v")) {
            self.hex_clipboard_paste();
            return true;
        }
        if ctrl
            && (matches!(&event.logical_key, Key::Character(c) if matches!(c.to_ascii_lowercase().as_str(), "s" | "o" | "n" | "w" | "p" | "q" | "t" | "+" | "-" | "=" | "0" | ","))
                || matches!(&event.logical_key, Key::Named(NamedKey::Tab)))
        {
            self.hex_cancel_input();
            return false;
        }
        if self.hex_read_only() && editing_key(&event.logical_key, self.mods) {
            self.hex_notice("Physical bytes are read-only while the document worker is pending");
            return true;
        }
        let result = self.view.hex_session_mut().map(|s| hex_ui::key(s, &event.logical_key, self.mods));
        match result {
            Some(Some(result)) => {
                self.hex_outcome(result);
                true
            }
            Some(None) => {
                self.hex_cancel_input();
                false
            }
            None => true,
        }
    }
    fn hex_origin(&self) -> Option<InputOrigin> {
        if !self.view.hex_active() || self.hex_read_only() || self.hex_body_blocked() {
            return None;
        }
        let session = self.view.hex_session()?;
        Some(InputOrigin {
            source: session.source()?,
            input_epoch: session.input_epoch(),
            workspace_epoch: self.view.input_epoch(),
        })
    }
    fn hex_commit_text(&mut self, origin: InputOrigin, text: &str, paste: bool) {
        if self.hex_origin() != Some(origin) {
            self.hex_notice("Hex input cancelled: its original destination changed");
            return;
        }
        let result = self.view.hex_session_mut().map(|s| {
            if paste {
                hex_ui::paste(s, origin.source, origin.input_epoch, text)
            } else {
                hex_ui::text(s, origin.source, origin.input_epoch, text)
            }
        });
        if let Some(result) = result {
            self.hex_outcome(result);
        }
    }
    fn hex_clipboard_paste(&mut self) {
        let Some(origin) = self.hex_origin() else {
            self.hex_notice("Physical bytes are read-only while the document worker is pending");
            return;
        };
        let hwnd = self.gfx.as_ref().map_or(0, |gfx| gfx.hwnd);
        match clipboard::SystemClipboard(hwnd).get_text(kwil_view_model::hex_session::MAX_EDIT_BYTES * 3) {
            Ok(Some(text)) => self.hex_commit_text(origin, &text, true),
            Ok(None) => {}
            Err(error) => self.hex_notice(error.to_string()),
        }
    }
    pub(super) fn hex_preedit(&mut self, text: &str) {
        if ime::starts_composition(self.composing, text) {
            self.hex.ime = self.hex_origin();
        }
        self.composing = !text.is_empty();
        self.ime_destination = Some(ime::Destination::Cancelled);
        self.search_ime = None;
        self.logs_ime = None;
    }
    pub(super) fn hex_ime_pending(&self) -> bool {
        self.hex.ime.is_some()
    }
    pub(super) fn hex_commit(&mut self, text: &str) {
        let origin =
            self.hex.ime.take().or_else(|| if self.ime_destination.is_none() { self.hex_origin() } else { None });
        self.composing = false;
        self.ime_destination = Some(ime::Destination::Cancelled);
        if let Some(origin) = origin {
            let valid = self.hex_origin() == Some(origin);
            self.hex_commit_text(origin, text, false);
            if valid {
                self.ime_destination = None;
            }
        }
    }
    pub(super) fn hex_cancel_input(&mut self) {
        self.hex.ime = None;
        self.view.cancel_hex_input();
        self.composing = false;
        self.ime_destination = Some(ime::Destination::Cancelled);
    }
    pub(super) fn hex_scroll_bar(&mut self, vertical: bool, offset: u64) -> bool {
        if !self.view.hex_active() {
            return false;
        }
        if vertical {
            let result = self.view.hex_session_mut().map(|s| {
                let Some(source) = s.source() else { return Ok(Outcome::default()) };
                s.input(source, s.input_epoch(), Input::ScrollTo(offset)).map_err(|e| e.to_string())
            });
            if let Some(result) = result {
                self.hex_outcome(result);
            }
        } else {
            let maximum = self.hex_bars().1.maximum;
            let offset = offset.min(maximum);
            if self.hex.horizontal != offset {
                self.hex.horizontal = offset;
                self.hex_cancel_input();
                self.apply(kwil_view_model::hex_session::body_damage());
            }
        }
        true
    }
    pub(super) fn hex_wheel(&mut self, rows: i64) -> bool {
        if !self.view.hex_active() {
            return false;
        }
        let result = self.view.hex_session_mut().map(|s| hex_ui::wheel(s, rows));
        if let Some(result) = result {
            self.hex_outcome(result);
        }
        true
    }
    pub(super) fn hex_bars(&self) -> (scrollbar::Bar, scrollbar::Bar) {
        let body = self.body_area();
        let height = body.height.saturating_sub(self.ui.bar);
        let width = body.width.saturating_sub(self.ui.bar);
        let visible = (height / self.ch().max(1)).min(kwil_view_model::hex::MAX_ROWS) as u64;
        let (top, total) = self.view.hex_session().map_or((0, 1), |s| {
            (s.view().top(), s.source().map_or(1, |source| kwil_view_model::hex::HexView::row_count(source.len)))
        });
        let columns = (width / self.font.as_ref().map_or(8, |f| f.cw).max(1)) as u64;
        (
            scrollbar::Bar::sized(0, height, visible, total, top, self.ui.min_thumb),
            scrollbar::Bar::sized(0, width, columns, COLUMNS, self.hex.horizontal, self.ui.min_thumb),
        )
    }
    /// True means hex owns this body point even without a mutable Session.
    pub(super) fn hex_pointer(&mut self, extend: bool) -> bool {
        if !self.view.hex_active() || self.hex_body_blocked() {
            return false;
        }
        let body = self.body_area();
        if !body.contains(self.pointer.0, self.pointer.1) {
            return false;
        }
        if self.hex_read_only() {
            return true;
        }
        let Some(font) = self.font.as_ref() else {
            return true;
        };
        let extra = self.hex.horizontal as usize * font.cw;
        let rect = Rect {
            x: body.x,
            y: body.y,
            w: body.width.saturating_sub(self.ui.bar) + extra,
            h: self.hex_bars().0.length,
        };
        let at = (self.pointer.0.saturating_add(extra), self.pointer.1);
        let result = self.view.hex_session_mut().and_then(|s| hex_ui::mouse(s, rect, font, at, extend));
        if let Some(result) = result {
            self.hex_outcome(result);
        }
        true
    }
}
