//! A composition belongs to the input destination captured at first preedit.
use kwil_view_model::{CommandArgs, Cursor, Damage, SessionDocumentId, Workspace};
use std::time::Duration;

pub(super) enum Destination {
    Cancelled,
    Prompt {
        epoch: u64,
        generation: u64,
        document: SessionDocumentId,
        revision: u64,
    },
    Editor {
        epoch: u64,
        document: SessionDocumentId,
        revision: u64,
        sequence: u64,
        cursors: Vec<Cursor>,
        primary: usize,
        insert_mode: bool,
    },
}

pub(super) fn capture(workspace: &Workspace, insert_mode: bool) -> Destination {
    let epoch = workspace.input_epoch();
    if let Some(prompt) = workspace.file_prompt() {
        return Destination::Prompt {
            epoch,
            generation: prompt.generation,
            document: prompt.document,
            revision: prompt.revision,
        };
    }
    let Some(editor) = workspace.editor() else { return Destination::Cancelled };
    Destination::Editor {
        epoch,
        document: workspace.active_id(),
        revision: editor.document().revision(),
        sequence: editor.next_edit_sequence(),
        cursors: editor.cursors().to_vec(),
        primary: editor.primary_index(),
        insert_mode,
    }
}

pub(super) fn commit(workspace: &mut Workspace, destination: Destination, text: String, at: Duration) -> Damage {
    if text.is_empty() {
        return Damage::default();
    }
    let stale = "IME input cancelled: its original destination changed or closed";
    match destination {
        Destination::Cancelled => workspace.set_notice(stale.into()),
        Destination::Prompt { epoch, generation, document, revision } => {
            let valid = epoch == workspace.input_epoch()
                && workspace
                    .file_prompt()
                    .is_some_and(|p| p.generation == generation && p.document == document && p.revision == revision)
                && workspace
                    .view(document)
                    .is_some_and(|view| view.editor().map_or(0, |e| e.document().revision()) == revision);
            if !valid {
                return workspace.set_notice(stale.into());
            }
            if text.chars().any(char::is_control) {
                return workspace.set_notice("IME path input contains a control character".into());
            }
            workspace.file_prompt_mut().unwrap().text.push_str(&text);
            workspace.prompt_notice()
        }
        Destination::Editor { epoch, document, revision, sequence, cursors, primary, insert_mode } => {
            if !insert_mode {
                return workspace.set_notice("IME input ignored: composition began outside Insert mode".into());
            }
            let valid = epoch == workspace.input_epoch()
                && workspace.view(document).and_then(|v| v.editor()).is_some_and(|editor| {
                    editor.document().revision() == revision
                        && editor.next_edit_sequence() == sequence
                        && editor.cursors() == cursors
                        && editor.primary_index() == primary
                });
            if !valid {
                return workspace.set_notice(stale.into());
            }
            let Some(super::input::Action::Text(text)) = super::input::commit(text, &mut None, insert_mode) else {
                return Damage::default();
            };
            let active = workspace.active_id() == document;
            let result = workspace.view_mut(document).unwrap().execute(
                "edit.insert",
                &CommandArgs { text: Some(text), at, ..Default::default() },
                1,
            );
            match result {
                Ok(result) if active => result.damage,
                Ok(_) => {
                    workspace.view_mut(document).unwrap().set_notice("IME input committed".into());
                    workspace.set_notice(format!("IME input committed to tab {}", document.0))
                }
                Err(error) => {
                    workspace.view_mut(document).unwrap().set_notice(error.to_string());
                    workspace.set_notice(format!("IME input: {error}"))
                }
            }
        }
    }
}

impl super::App {
    pub(super) fn insert_mode(&self) -> bool {
        self.keys.as_ref().map_or(self.settings.keymap != kwil_core_commands::Preset::Vim, |keys| {
            keys.mode() == kwil_core_commands::Mode::Insert
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kwil_view_model::View;

    fn bytes(workspace: &Workspace, id: SessionDocumentId) -> u64 {
        workspace.view(id).unwrap().editor().unwrap().document().snapshot().len()
    }

    #[test]
    fn composition_stays_with_inactive_document_and_captured_mode() {
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        let original = workspace.active_id();
        let capture = capture(&workspace, true);
        workspace.file_command("file.new").unwrap();
        let other = workspace.active_id();
        let damage = commit(&mut workspace, capture, "日本語".into(), Default::default());
        assert_eq!(bytes(&workspace, original), 9);
        assert_eq!(bytes(&workspace, other), 0);
        assert!(!damage.full);
        assert!(damage.status);
        let destination = super::capture(&workspace, false);
        commit(&mut workspace, destination, "normal".into(), Default::default());
        assert_eq!(bytes(&workspace, other), 0);
        assert!(workspace.status_text().contains("outside Insert mode"));
    }

    #[test]
    fn changed_cursor_revision_and_closed_reopened_tab_reject_old_composition() {
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        workspace.execute("edit.insert", &CommandArgs { text: Some("x".into()), ..Default::default() }, 1).unwrap();
        let first = workspace.active_id();
        let destination = capture(&workspace, true);
        workspace.execute("motion.left", &Default::default(), 1).unwrap();
        commit(&mut workspace, destination, "bad".into(), Default::default());
        assert_eq!(bytes(&workspace, first), 1);
        let destination = capture(&workspace, true);
        workspace.execute("edit.insert", &CommandArgs { text: Some("y".into()), ..Default::default() }, 1).unwrap();
        commit(&mut workspace, destination, "bad".into(), Default::default());
        assert_eq!(bytes(&workspace, first), 2);
        let destination = capture(&workspace, true);
        workspace.close(first, true).unwrap();
        workspace.file_command("file.reopenClosed").unwrap();
        commit(&mut workspace, destination, "bad".into(), Default::default());
        assert_eq!(bytes(&workspace, first), 2);
        assert!(workspace.status_text().contains("cancelled"));
    }

    #[test]
    fn blank_enter_retains_prompt_capture_but_cancel_reopen_does_not() {
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        workspace.file_command("file.saveAs").unwrap();
        let destination = capture(&workspace, false);
        workspace.submit_file_prompt().unwrap();
        commit(&mut workspace, destination, "日本語.txt".into(), Default::default());
        assert_eq!(workspace.file_prompt().unwrap().text, "日本語.txt");
        assert_eq!(workspace.editor().unwrap().document().snapshot().len(), 0);
        let destination = capture(&workspace, false);
        workspace.cancel_file_prompt();
        workspace.file_command("file.saveAs").unwrap();
        commit(&mut workspace, destination, "wrong".into(), Default::default());
        assert_eq!(workspace.file_prompt().unwrap().text, "");
        assert!(workspace.status_text().contains("cancelled"));
    }

    #[test]
    fn preparation_id_collision_never_receives_early_composition() {
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        let original = workspace.active_id();
        let destination = capture(&workspace, true);
        let mut session = kwil_core_workspace::session::Session::default();
        let restored = session.open(None).unwrap();
        assert_eq!(restored, original);
        workspace.document_result(kwil_view_model::document_worker::DocumentResult::Prepared(Ok(
            kwil_view_model::document_worker::PreparedWorkspace {
                session,
                documents: vec![(restored, Ok(kwil_view_model::Document::new()))],
            },
        )));
        commit(&mut workspace, destination, "wrong".into(), Default::default());
        assert_eq!(bytes(&workspace, restored), 0);
        assert!(workspace.status_text().contains("cancelled"));
    }
}
