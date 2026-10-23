//! File/tab actions stay outside editor command dispatch and renderer geometry.
use super::*;

pub(super) fn sync_active_document<T>(
    workspace: &Workspace,
    previous: &mut kwil_view_model::SessionDocumentId,
    previous_epoch: &mut u64,
    horizontal: &mut u64,
    selecting: &mut bool,
    drag: &mut Option<T>,
    keys: Option<&mut kwil_core_commands::keymap::KeymapEngine>,
) {
    if *previous != workspace.active_id() || *previous_epoch != workspace.input_epoch() {
        *previous = workspace.active_id();
        *previous_epoch = workspace.input_epoch();
        *horizontal = workspace.horizontal_origin();
        *selecting = false;
        *drag = None;
        if let Some(keys) = keys {
            keys.cancel_pending();
        }
    }
}

pub(super) fn cancel(workspace: &mut Workspace) -> Damage {
    workspace.cancel_file_prompt();
    workspace.set_notice(String::new())
}

pub(super) fn submit(workspace: &mut Workspace) -> Damage {
    let opens_tab = workspace.file_prompt().is_some_and(|p| {
        matches!(p.kind, kwil_view_model::FilePromptKind::Open | kwil_view_model::FilePromptKind::Recent)
    });
    if let Err(error) = workspace.submit_file_prompt() {
        workspace.set_notice(error.to_string());
    }
    Damage { full: opens_tab && workspace.file_prompt().is_none(), status: true, ..Default::default() }
}

impl App {
    pub(super) fn dispatch_file(&mut self, command: &str) -> bool {
        match self.view.file_command(command) {
            Ok(false) => return false,
            Ok(true) => {}
            Err(error) => {
                self.view.set_notice(error.to_string());
            }
        }
        self.drag = None;
        self.selecting = false;
        self.sync_file_ui();
        self.apply(Damage { full: true, status: true, ..Default::default() });
        true
    }

    pub(super) fn sync_file_ui(&mut self) {
        if let Some(palette) = self.palette.as_mut().filter(|palette| palette.model().is_open()) {
            match palette.refresh_tabs(&self.view) {
                Ok(changed) => self.palette_dirty |= changed,
                Err(error) => {
                    self.view.set_notice(format!("Palette: {error:?}"));
                }
            }
        }
        self.save_path = self.view.file_prompt().map(|p| p.text.clone());
        // The prompt card repaints while open and restores the body once it leaves.
        if self.save_path.is_some() || self.prompt_view.area.take().is_some() {
            self.palette_dirty = true;
        }
        self.close_pending = self.view.close_pending();
        sync_active_document(
            &self.view,
            &mut self.frontend_document,
            &mut self.frontend_epoch,
            &mut self.horizontal,
            &mut self.selecting,
            &mut self.drag,
            self.keys.as_mut(),
        );
        let rows = self.text_rows();
        if self.view.rows() != rows {
            self.view.set_rows(rows);
        }
        if let Some(editor) = self.view.editor_mut() {
            editor.settings = self.settings.clone();
        }
    }

    pub(super) fn update_file_prompt(&mut self) {
        if let (Some(text), Some(prompt)) = (&self.save_path, self.view.file_prompt_mut()) {
            prompt.text = text.clone();
        }
        let damage = self.view.prompt_notice();
        self.palette_dirty = true;
        self.apply(damage);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_receipts_preserve_drag_and_per_row_windows_until_the_active_document_changes() {
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        workspace.set_rows(2);
        workspace
            .execute("edit.insert", &CommandArgs { text: Some("界a\nxyz".into()), ..Default::default() }, 1)
            .unwrap();
        workspace.set_horizontal_window(2, 64 << 10);
        let first = workspace.active_id();
        assert_eq!(workspace.line_offset(0), 0);
        assert_eq!(workspace.line_offset(1), 2);
        let (mut previous, mut horizontal, mut selecting, mut drag) = (first, 2, true, Some((true, 3)));
        let mut epoch = workspace.input_epoch();
        workspace.document_result(kwil_view_model::document_worker::DocumentResult::Persisted(Ok(())));
        sync_active_document(&workspace, &mut previous, &mut epoch, &mut horizontal, &mut selecting, &mut drag, None);
        assert_eq!(horizontal, 2);
        assert!(selecting);
        assert_eq!(drag, Some((true, 3)));
        workspace.file_command("file.new").unwrap();
        sync_active_document(&workspace, &mut previous, &mut epoch, &mut horizontal, &mut selecting, &mut drag, None);
        assert_eq!(horizontal, 0);
        assert!(!selecting);
        assert!(drag.is_none());
        workspace.activate(first);
        sync_active_document(&workspace, &mut previous, &mut epoch, &mut horizontal, &mut selecting, &mut drag, None);
        assert_eq!(horizontal, 2);
        assert_eq!(workspace.line_offset(0), 0);
        assert_eq!(workspace.line_offset(1), 2);
    }

    #[test]
    fn switching_document_cancels_pending_vim_operator_and_chord_without_changing_mode() {
        use kwil_core_commands::{Mode, Preset, keymap::KeymapEngine, vim::VimResult};
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        let mut previous = workspace.active_id();
        let mut epoch = workspace.input_epoch();
        let (mut horizontal, mut selecting, mut drag) = (0, false, None::<(bool, usize)>);
        let mut keys = KeymapEngine::new(Preset::Vim, &[]).unwrap();
        assert_eq!(keys.feed_with_selection("d".parse().unwrap()), VimResult::Pending);
        workspace.file_command("file.new").unwrap();
        sync_active_document(
            &workspace,
            &mut previous,
            &mut epoch,
            &mut horizontal,
            &mut selecting,
            &mut drag,
            Some(&mut keys),
        );
        assert!(
            matches!(keys.feed_with_selection("w".parse().unwrap()), VimResult::Command(call) if call.id == "motion.wordForward")
        );
        keys.feed_with_selection("i".parse().unwrap());
        assert_eq!(keys.mode(), Mode::Insert);
        workspace.file_command("file.new").unwrap();
        sync_active_document(
            &workspace,
            &mut previous,
            &mut epoch,
            &mut horizontal,
            &mut selecting,
            &mut drag,
            Some(&mut keys),
        );
        assert_eq!(keys.mode(), Mode::Insert, "switching tabs must preserve the selected mode");
        let mut keys = KeymapEngine::new(Preset::VsCode, &[]).unwrap();
        assert_eq!(keys.feed_with_selection("ctrl+k".parse().unwrap()), VimResult::Pending);
        workspace.file_command("file.new").unwrap();
        sync_active_document(
            &workspace,
            &mut previous,
            &mut epoch,
            &mut horizontal,
            &mut selecting,
            &mut drag,
            Some(&mut keys),
        );
        assert!(
            matches!(keys.feed_with_selection("ctrl+s".parse().unwrap()), VimResult::Command(call) if call.id == "file.save")
        );
    }
}
