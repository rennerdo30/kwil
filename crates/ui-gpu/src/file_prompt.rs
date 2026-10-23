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
    // GUI Enter must never submit a native chooser's blank/text path. TUI owns
    // its separate inline implementation. Recent-number selection stays inline.
    if file_dialog::native_supported() && workspace.native_file_prompt().is_some() {
        return Damage::default();
    }
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
        if self.close_dialog.is_some() || self.file_dialogs.exiting() {
            return true;
        }
        if self.file_dialogs.busy() && command.starts_with("file.") {
            // Modal ownership: reject competing file actions without replacing
            // the captured prompt or its close/Save All continuation.
            self.view.set_notice("Finish or cancel the system file dialog first".into());
            self.apply(Damage { status: true, ..Default::default() });
            return true;
        }
        let outcome = if command == "file.quit" && file_dialog::native_supported() {
            self.view.request_window_close_with_prompt().map(|()| true)
        } else {
            self.view.file_command(command)
        };
        match outcome {
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

    pub(super) fn file_dialog_event(&mut self, event: file_dialog::Event) {
        // Receipts cannot commit text from a composition started in the old
        // inline GUI prompt. The OS chooser owns its own IME.
        if self.file_modal() {
            ime::reset(&mut self.composing, &mut self.ime_destination);
        }
        self.file_dialogs.event(&mut self.view, event);
        self.sync_file_ui();
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
        let owner = self.gfx.as_ref().map_or(0, |gfx| gfx.hwnd);
        let proxy = self.proxy.clone();
        self.file_dialogs.sync(&mut self.view, owner, self.opts.hidden, move |event| {
            let _ = proxy.send_event(UserEvent::FileDialog(event));
        });
        self.save_path = self
            .view
            .file_prompt()
            .filter(|prompt| !file_dialog::native_supported() || prompt.kind == kwil_view_model::FilePromptKind::Recent)
            .map(|prompt| prompt.text.clone());
        if self.save_path.is_some() || self.prompt_view.area.take().is_some() {
            self.palette_dirty = true;
        }
        self.close_pending = self.view.close_pending();
        self.sync_close_dialog();
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
        self.apply(damage);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(not(windows))]
    fn existing_explicit_path_submission_survives_until_native_backend_is_available() {
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        workspace.execute("edit.insert", &CommandArgs { text: Some("keep".into()), ..Default::default() }, 1).unwrap();
        workspace.file_command("file.save").unwrap();
        let target = std::env::temp_dir().join(format!(
            "kw56-explicit-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        workspace.file_prompt_mut().unwrap().text = target.to_str().unwrap().into();
        submit(&mut workspace);
        assert!(workspace.file_prompt().is_none());
        assert!(workspace.file_saves_pending());
        assert!(!target.exists(), "without a worker no write has been admitted to the filesystem");
    }

    #[test]
    fn worker_receipts_preserve_drag_and_per_row_windows_until_the_active_document_changes() {
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        workspace.set_rows(2);
        workspace
            .execute("edit.insert", &CommandArgs { text: Some("Ã§â€¢Å’a\nxyz".into()), ..Default::default() }, 1)
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
