//! File/tab actions stay outside editor command dispatch and renderer geometry.
use super::*;

struct FileDialogNotificationCallback {
    proxy: HostProxy,
    endpoint: notification::ControlEndpoint,
}
impl FileDialogNotificationCallback {
    fn send(&self, event: file_dialog::Event) {
        let event = match event {
            file_dialog::Event::Stopped(Err(original)) => match self.proxy.retain_control_error(original) {
                Ok(diagnostic) => file_dialog::Event::Stopped(Err(diagnostic)),
                Err(reason) => {
                    self.proxy.fail(reason);
                    return;
                }
            },
            event => event,
        };
        let terminal = matches!(&event, file_dialog::Event::Stopped(_));
        match self.endpoint.receipt(event, terminal) {
            Ok(receipt) => {
                let _ = self.proxy.send_event(UserEvent::FileDialog(receipt));
            }
            Err(original) => {
                self.proxy.fail(notification::Reason::UnresolvedControl);
                std::mem::forget(original);
            }
        }
    }
}

#[cfg(test)]
pub(super) fn notification_fixture_sender(proxy: HostProxy) -> impl Fn(file_dialog::Event) {
    let endpoint = proxy
        .control_endpoint(
            notification::SlotKind::FileDialog,
            2 * std::mem::size_of::<FileDialogNotificationCallback>()
                + 2 * std::mem::size_of::<notification::Receipt<file_dialog::Event>>(),
        )
        .unwrap_or_else(|reason| panic!("windowless original callback admission: {reason:?}"));
    let callback = FileDialogNotificationCallback { proxy, endpoint };
    move |event| callback.send(event)
}

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
    pub(super) fn consume_file_dialog_notification(
        &mut self,
        event: notification::Receipt<file_dialog::Event>,
    ) -> bool {
        if !event.matches(&self.proxy, notification::SlotKind::FileDialog) {
            self.proxy.fail(notification::Reason::Identity);
            return false;
        }
        let failed = matches!(event.value(), file_dialog::Event::Stopped(Err(_)));
        match event.consume(|event| self.file_dialog_event(event)) {
            Ok(()) => {
                if failed {
                    self.proxy.fail(notification::Reason::ControlError);
                }
                true
            }
            Err(reason) => {
                self.proxy.fail(reason);
                false
            }
        }
    }
    pub(super) fn external_modal(&self) -> bool {
        self.document_input_blocked()
            || self.file_modal()
            || self.binary_pending()
            || self.view.file_prompt().is_some()
            || self.view.close_pending()
            || self.settings_open()
            || self.desktop_window.is_some()
            || self.welcome.visible()
            || self.about.open
    }
    /// Keep accepted IPC requests in their original fixed 16-slot inbox while
    /// a modal owns a captured document/prompt ticket. No secondary queue.
    pub(super) fn external_open_event(&mut self) -> Damage {
        if self.file_dialogs.exiting() || self.view.wants_exit() {
            self.external_inbox.close();
            return Damage::default();
        }
        if self.external_modal() {
            return Damage::default();
        }
        let requests = self.external_inbox.drain();
        if requests.is_empty() {
            return Damage::default();
        }
        for request in requests {
            if let Err(error) = self.view.open_files(request) {
                self.view.set_notice(format!("Open failed: {error}"));
            }
        }
        self.sync_file_ui();
        Damage { full: true, status: true, ..Default::default() }
    }
    /// Called only at the end of an existing event-loop turn. The receipt
    /// clears the inbox's pending bit; this never arms a timer or idle poll.
    pub(super) fn resume_external_open(&self) {
        if !self.file_dialogs.exiting()
            && !self.view.wants_exit()
            && !self.external_modal()
            && self.external_inbox.has_pending()
        {
            let _ = self.proxy.send_event(UserEvent::OpenExternal);
        }
    }
    pub(super) fn dispatch_file(&mut self, command: &str) -> bool {
        if self.binary_owns_input() || self.close_dialog.is_some() || self.file_dialogs.exiting() {
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
            ime::cancel(&mut self.composing, &mut self.ime_destination);
        }
        self.file_dialogs.event(&mut self.view, event);
        self.sync_file_ui();
    }

    pub(super) fn sync_file_ui(&mut self) {
        if let Some(root) = self.view.take_workspace_folder() {
            let _ = self.proxy.send_event(UserEvent::PaletteRoot(Ok(root)));
        }
        self.sync_binary_input();
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
        if !self.confirmation_worker_busy() {
            let endpoint = match proxy.control_endpoint(
                notification::SlotKind::FileDialog,
                2 * std::mem::size_of::<FileDialogNotificationCallback>()
                    + 2 * std::mem::size_of::<notification::Receipt<file_dialog::Event>>(),
            ) {
                Ok(endpoint) => endpoint,
                Err(reason) => {
                    proxy.fail(reason);
                    return;
                }
            };
            let callback = FileDialogNotificationCallback { proxy, endpoint };
            self.file_dialogs.sync(&mut self.view, owner, self.opts.hidden, move |event| callback.send(event));
        }
        self.save_path = self
            .view
            .file_prompt()
            .filter(|_| !file_dialog::native_supported() || self.view.native_file_prompt().is_none())
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
    fn file_commands_use_the_gui_prompt_and_bounded_document_completion() {
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        let root = crate::test_support::directory("kwil-gui-file-commands").unwrap();
        let lane = crate::document_ui::fixtures::Lane::start(&mut workspace, root.join("state"));
        lane.until(&mut workspace, |workspace| workspace.recovery_ready());
        workspace
            .execute("edit.insert", &CommandArgs { text: Some("one\ntwo\n".into()), ..Default::default() }, 1)
            .unwrap();
        let choose = |workspace: &mut Workspace, command: &str, choice: &str| {
            assert!(workspace.file_command(command).unwrap());
            workspace.file_prompt_mut().unwrap().text = choice.into();
            submit(workspace);
            assert!(workspace.file_prompt().is_none(), "{command} must be accepted without retries");
        };
        choose(&mut workspace, "file.setLineEnding", "2");
        let id = workspace.active_id();
        lane.until(&mut workspace, |workspace| !workspace.document_busy(id));
        let mut bytes = Vec::new();
        workspace.editor().unwrap().document().snapshot().write_to(&mut bytes).unwrap();
        assert_eq!(bytes, b"one\r\ntwo\r\n");
        choose(&mut workspace, "file.setEncoding", "2");
        lane.until(&mut workspace, |workspace| !workspace.document_busy(id));
        let path = root.join("saved.txt");
        workspace.save_document(id, Some(path.clone())).unwrap();
        lane.until(&mut workspace, |workspace| !workspace.file_saves_pending());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            "one\r\ntwo\r\n".encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<_>>()
        );
        workspace.file_command("file.reload").unwrap();
        lane.until(&mut workspace, |workspace| !workspace.document_busy(id));
        bytes.clear();
        workspace.editor().unwrap().document().snapshot().write_to(&mut bytes).unwrap();
        assert_eq!(bytes, b"one\r\ntwo\r\n");
        std::fs::rename(&path, root.join("previous.txt")).unwrap();
        std::fs::write(&path, b"outside\n").unwrap();
        workspace.file_command("file.keepChanges").unwrap();
        lane.until(&mut workspace, |workspace| !workspace.document_busy(id));
        assert!(workspace.editor().unwrap().document().is_dirty(), "keeping an older clean buffer needs a save");
        assert_eq!(std::fs::read(&path).unwrap(), b"outside\n");
        choose(&mut workspace, "file.reload", "1");
        lane.until(&mut workspace, |workspace| !workspace.document_busy(id));
        bytes.clear();
        workspace.editor().unwrap().document().snapshot().write_to(&mut bytes).unwrap();
        assert_eq!(bytes, b"outside\n");
        lane.settle(workspace);
        std::fs::remove_dir_all(root).unwrap();
    }

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
        let root = crate::test_support::directory("kwil-gui-prompt-receipt").unwrap();
        let lane = crate::document_ui::fixtures::Lane::start(&mut workspace, root.join("state"));
        lane.until(&mut workspace, |workspace| workspace.recovery_ready());
        workspace.set_rows(2);
        workspace
            .execute("edit.insert", &CommandArgs { text: Some("\u{754c}a\nxyz".into()), ..Default::default() }, 1)
            .unwrap();
        workspace.set_horizontal_window(2, 64 << 10);
        let first = workspace.active_id();
        assert_eq!(workspace.line_offset(0), 0);
        assert_eq!(workspace.line_offset(1), 2);
        let (mut previous, mut horizontal, mut selecting, mut drag) = (first, 2, true, Some((true, 3)));
        let mut epoch = workspace.input_epoch();
        workspace.checkpoint_session();
        lane.through(&mut workspace, kwil_view_model::workspace_document::ResultKind::Persisted);
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
        lane.settle(workspace);
        std::fs::remove_dir_all(root).unwrap();
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

#[cfg(test)]
mod external_change {
    use super::*;

    /// Open notes.txt, edit it, let another program overwrite it, then save:
    /// Kwil offers the choice instead of a dead end.
    fn conflicted(
        name: &str,
    ) -> (Workspace, crate::document_ui::fixtures::Lane, std::path::PathBuf, kwil_view_model::SessionDocumentId) {
        let root = crate::test_support::directory(name).unwrap();
        let path = root.join("notes.txt");
        std::fs::write(&path, b"original\n").unwrap();
        let file = kwil_core_workspace::open_for_editing(&path).unwrap();
        file.build_index(1).unwrap();
        let mut workspace = Workspace::new(View::new(Some((path.clone(), file)))).unwrap();
        let lane = crate::document_ui::fixtures::Lane::start(&mut workspace, root.join("state"));
        lane.until(&mut workspace, |w| w.recovery_ready() && w.editor().is_some());
        let id = workspace.active_id();
        workspace.execute("edit.insert", &CommandArgs { text: Some("mine ".into()), ..Default::default() }, 1).unwrap();
        std::fs::write(&path, b"changed outside\n").unwrap();
        workspace.save_document(id, None).unwrap();
        lane.until(&mut workspace, |w| !w.file_saves_pending());
        assert_eq!(
            workspace.file_prompt().map(|p| p.kind),
            Some(kwil_view_model::FilePromptKind::SaveConflict),
            "{}",
            workspace.status_text()
        );
        assert!(workspace.status_text().contains("changed on disk"), "{}", workspace.status_text());
        assert_eq!(std::fs::read(&path).unwrap(), b"changed outside\n", "nothing is overwritten without asking");
        (workspace, lane, path, id)
    }

    fn text(workspace: &Workspace) -> String {
        let mut bytes = Vec::new();
        workspace.editor().unwrap().document().snapshot().write_to(&mut bytes).unwrap();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn overwrite_with_my_version_after_an_external_change() {
        let (mut workspace, lane, path, id) = conflicted("kwil-conflict-overwrite");
        workspace.file_prompt_mut().unwrap().text = "1".into();
        submit(&mut workspace);
        assert!(workspace.file_prompt().is_none(), "{}", workspace.status_text());
        lane.until(&mut workspace, |w| !w.file_saves_pending());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "mine original\n");
        assert!(!workspace.editor().unwrap().document().is_dirty());
        // The next normal save is ordinary again.
        workspace
            .execute("edit.insert", &CommandArgs { text: Some("again ".into()), ..Default::default() }, 1)
            .unwrap();
        workspace.save_document(id, None).unwrap();
        lane.until(&mut workspace, |w| !w.file_saves_pending());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "mine again original\n");
        lane.settle(workspace);
    }

    #[test]
    fn reload_from_disk_after_an_external_change() {
        let (mut workspace, lane, path, id) = conflicted("kwil-conflict-reload");
        workspace.file_prompt_mut().unwrap().text = "2".into();
        submit(&mut workspace);
        lane.until(&mut workspace, |w| !w.document_busy(id) && w.file_prompt().is_none());
        assert_eq!(text(&workspace), "changed outside\n");
        assert!(!workspace.editor().unwrap().document().is_dirty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "changed outside\n");
        lane.settle(workspace);
    }
}
