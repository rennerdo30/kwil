//! Real-process kill/restart assertions, driven by the existing hidden script.
use super::*;
use kwil_core_workspace::session;
use kwil_view_model::{CommandArgs, Cursor, FilePromptKind};

const NAMED: &[u8] = b"unsaved named: original on disk\n";
const SCRATCH: &[u8] = b"unsaved scratch\nsecond line\n";

fn bytes(view: &kwil_view_model::View) -> Vec<u8> {
    let mut bytes = Vec::new();
    view.editor().expect("restored editor").document().snapshot().write_to(&mut bytes).unwrap();
    bytes
}

pub(super) fn tick(app: &mut App, output: Option<&Path>) {
    assert!(app.opts.hidden);
    let output = output.expect("isolated recovery capture directory");
    let phase = app.metrics.recovery_phase;
    if phase == 4 || !app.view.recovery_ready() || app.view.document_busy(app.view.active_id()) {
        return;
    }
    let mode = std::env::var("KWIL_SESSION_PROBE").unwrap();
    let root = PathBuf::from(std::env::var_os("KWIL_WORKSPACE_ROOT").unwrap());
    let source = root.parent().unwrap().join("notes.txt");
    if mode == "kill-seed" {
        match phase {
            0 => {
                if app.view.editor().is_none() {
                    return;
                }
                assert_eq!(app.view.session().tabs().len(), 1);
                assert_eq!(bytes(&app.view), b"original on disk\n");
                app.view.set_session_restore(true);
                app.view
                    .execute(
                        "edit.insert",
                        &CommandArgs { text: Some("unsaved named: ".into()), ..Default::default() },
                        1,
                    )
                    .unwrap();
                assert_eq!(bytes(&app.view), NAMED);
                app.view
                    .editor_mut()
                    .unwrap()
                    .set_cursors(vec![Cursor { head: 7, anchor: 2, desired_column: None }], 0)
                    .unwrap();
                app.view.checkpoint_session();
                assert_eq!(
                    app.view.recovery_notice(),
                    Some("Recovery pending"),
                    "named edit must await its journal ACK"
                );
                app.metrics.recovery_phase = 1;
                app.apply(Damage { full: true, status: true, ..Default::default() });
            }
            1 if app.view.recovery_notice().is_none() => {
                assert_eq!(bytes(&app.view), NAMED);
                app.view.file_command("file.new").unwrap();
                app.view
                    .execute(
                        "edit.insert",
                        &CommandArgs { text: Some(String::from_utf8(SCRATCH.to_vec()).unwrap()), ..Default::default() },
                        2,
                    )
                    .unwrap();
                app.view
                    .editor_mut()
                    .unwrap()
                    .set_cursors(vec![Cursor { head: 9, anchor: 3, desired_column: None }], 0)
                    .unwrap();
                app.view.checkpoint_session();
                assert_eq!(
                    app.view.recovery_notice(),
                    Some("Recovery pending"),
                    "scratch edit must await its journal ACK"
                );
                app.metrics.recovery_phase = 2;
                app.apply(Damage { full: true, status: true, ..Default::default() });
            }
            2 if app.view.recovery_notice().is_none() => {
                // This is the public UI's exact revision/sequence journal ACK,
                // not a guessed delay, close/flush, or merely a nonempty file.
                app.view.sync_all();
                let Ok(saved) = session::load_session(&root.join("session.kws")) else { return };
                if &saved != app.view.session() {
                    return;
                }
                assert_eq!(saved.tabs().len(), 2);
                assert_eq!(saved.tabs()[0].path.as_ref(), Some(&source));
                assert_eq!(saved.active(), Some(saved.tabs()[1].id));
                finish(app, output, "durable", "both journals acknowledged; session metadata persisted\n");
            }
            _ => {}
        }
        return;
    }
    if app.view.session().tabs().len() != 2 || app.view.editor().is_none() {
        return;
    }
    if phase == 0 {
        let expected = session::load_session(&root.parent().unwrap().join("expected-session.kws")).unwrap();
        let restored = app.view.session();
        assert_eq!(restored.active(), expected.active(), "active tab restored");
        for (actual, expected) in restored.tabs().iter().zip(expected.tabs()) {
            assert_eq!(actual.id, expected.id, "tab identity and order restored");
            assert_eq!(
                (actual.cursor, actual.anchor, actual.first_visible_line, actual.first_byte_column),
                (expected.cursor, expected.anchor, expected.first_visible_line, expected.first_byte_column),
                "selection and viewport restored"
            );
            let view = app.view.view(actual.id).unwrap();
            assert!(
                view.editor().expect("unsaved journal must restore an editor").document().is_dirty(),
                "unsaved marker restored"
            );
        }
        assert_eq!(bytes(&app.view), SCRATCH);
        let cursor = app.view.editor().unwrap().primary_cursor();
        assert_eq!((cursor.head, cursor.anchor), (9, 3));
        let named = app.view.session().tabs()[0].id;
        assert_eq!(bytes(app.view.view(named).unwrap()), NAMED, "named unsaved edits restored exactly");
        let cursor = app.view.view(named).unwrap().editor().unwrap().primary_cursor();
        assert_eq!((cursor.head, cursor.anchor), (7, 2));
        app.view.activate(named);
        app.metrics.recovery_phase = 1;
        return;
    }
    if phase == 1 {
        // Dirty recovery intentionally has no verified pre-crash original.
        // Existing policy preserves the name as context and requires Save As,
        // rather than silently adopting either current disk or recovered text.
        assert!(app.view.editor().unwrap().document().path().is_none());
        assert_eq!(app.view.session().tabs()[0].recovery_path.as_ref(), Some(&source));
        let notice = app.view.status_text();
        assert!(notice.contains("Recovered backing cannot be verified") && notice.contains("Use Save As"), "{notice}");
        app.apply(Damage { full: true, status: true, ..Default::default() });
        assert!(app.render());
        super::capture(app, Some(output), "recovery-warning.bmp");
        app.view.file_command("file.save").unwrap();
        assert_eq!(app.view.file_prompt().map(|prompt| prompt.kind), Some(FilePromptKind::SaveAs));
        assert_eq!(bytes(&app.view), NAMED);
        finish(
            app,
            output,
            "restored",
            &format!("tabs/order/active/selection/bytes/dirty restored\nrecovery decision=SaveAs\n{notice}\n"),
        );
    }
}

fn finish(app: &mut App, output: &Path, name: &str, evidence: &str) {
    app.apply(Damage { full: true, status: true, ..Default::default() });
    assert!(app.render());
    #[cfg(windows)]
    {
        let hwnd = app.gfx.as_ref().unwrap().hwnd;
        super::native_probe(hwnd, Some(output));
        super::capture_dc(hwnd, &output.join(format!("{name}-window-dc.bmp")), false).unwrap();
    }
    super::capture(app, Some(output), &format!("{name}.bmp"));
    // Publish last: the runner must not kill before assertions/capture finish.
    std::fs::write(output.join(format!("{name}.txt")), evidence).unwrap();
    app.metrics.recovery_phase = 4;
}
