//! Command/file palette host. Matching and discovery live in the shared host;
//! `palette_view` draws its capped immutable result snapshot as a desktop card.
use super::*;
use kwil_core_commands::palette::Palette;

pub(super) fn palette_blocks_copy(open: bool, displayed: bool) -> bool {
    open || displayed
}

fn model_vim_command(id: &str) -> bool {
    !id.starts_with("file.")
        && !id.starts_with("view.")
        && !matches!(id, "edit.copy" | "edit.cut" | "edit.paste" | "navigation.goToLine")
}

// Drawing and pointer geometry live in `palette_view` (desktop card).

/// Line mode is synchronous in the existing Palette model. It needs neither
/// catalogue discovery nor PaletteRoot, so early Ctrl+G retains every key.
pub(super) struct GoToPrompt {
    pub model: Palette,
    document: kwil_view_model::SessionDocumentId,
    workspace_epoch: u64,
}

impl App {
    pub(super) fn sync_goto_identity(&mut self) {
        if self.goto.as_ref().is_some_and(|prompt| {
            prompt.document != self.view.active_id() || prompt.workspace_epoch != self.view.input_epoch()
        }) {
            self.goto = None;
            self.goto_epoch += 1;
            self.palette_dirty = true;
            self.ime_destination = Some(ime::Destination::Cancelled);
            self.composing = false;
            if let Some(keys) = &mut self.keys {
                keys.cancel_pending();
                keys.set_context(kwil_core_commands::Context::Editor);
            }
            self.view.set_notice("Go to Line cancelled: the target tab changed during startup or navigation".into());
        }
    }

    fn goto_input(&mut self, input: kwil_view_model::palette_session::PaletteInput<'_>) {
        use kwil_view_model::palette_session::PaletteInput;
        self.sync_goto_identity();
        let Some(prompt) = &mut self.goto else { return };
        self.goto_epoch = self.goto_epoch.checked_add(1).expect("goto input epoch exhausted");
        let mut action = None;
        let result = match input {
            PaletteInput::Text(text) => prompt.model.insert(text).map(|_| ()),
            PaletteInput::Backspace => {
                prompt.model.backspace();
                Ok(())
            }
            PaletteInput::Delete => {
                prompt.model.delete_forward();
                Ok(())
            }
            PaletteInput::Left => {
                prompt.model.move_left();
                Ok(())
            }
            PaletteInput::Right => {
                prompt.model.move_right();
                Ok(())
            }
            PaletteInput::Home => {
                prompt.model.move_home();
                Ok(())
            }
            PaletteInput::End => {
                prompt.model.move_end();
                Ok(())
            }
            PaletteInput::Next => {
                prompt.model.next();
                Ok(())
            }
            PaletteInput::Previous => {
                prompt.model.previous();
                Ok(())
            }
            PaletteInput::Accept => {
                action = prompt.model.accept();
                if action.is_none() {
                    self.view.set_notice("Go to Line: enter :line or :line:column (one based)".into());
                }
                Ok(())
            }
            PaletteInput::Cancel => {
                prompt.model.cancel();
                Ok(())
            }
        };
        if let Err(error) = result {
            self.view.set_notice(format!("Go to Line: {error:?}"));
        }
        if !prompt.model.is_open() {
            self.goto = None;
            if let Some(keys) = &mut self.keys {
                keys.cancel_pending();
                keys.set_context(kwil_core_commands::Context::Editor);
            }
        }
        self.palette_redraw();
        self.apply(Damage { status: true, ..Default::default() });
        if let Some(action) = action {
            self.palette_action(action);
        }
    }
    pub(super) fn capture_input_destination(&self) -> ime::Destination {
        if self.language_owns_input() {
            return ime::Destination::Cancelled;
        }
        if self.settings_choice_open() {
            return ime::Destination::Cancelled;
        }
        if self.settings_open() {
            return ime::Destination::Settings { epoch: self.settings_epoch };
        }
        if self.goto.is_some() {
            return ime::Destination::Palette { epoch: self.goto_epoch };
        }
        if self.search_panel.is_open() {
            return ime::Destination::Cancelled;
        }
        match self.palette.as_ref().filter(|palette| palette.model().is_open()) {
            Some(palette) => ime::Destination::Palette { epoch: palette.input_epoch() },
            None if self.markdown_mode() == markdown_ui::Mode::Preview => ime::Destination::Cancelled,
            None => ime::capture(&self.view, self.insert_mode()),
        }
    }
    pub(super) fn palette_open(&self) -> bool {
        self.goto.is_some() || self.palette.as_ref().is_some_and(|p| p.model().is_open())
    }

    pub(super) fn palette_redraw(&mut self) {
        if let Some(dialog) = &self.desktop_window {
            dialog.gfx.window.request_redraw();
            return;
        }
        self.palette_dirty = true;
        if let Some(gfx) = &self.gfx {
            self.redraw_requested = true;
            gfx.window.request_redraw();
        }
    }

    pub(super) fn dispatch_palette(&mut self, command: &str) -> bool {
        let query = match command {
            "view.commandPalette" => ">",
            "view.fileFinder" => "",
            "navigation.goToLine" => ":",
            _ => return false,
        };
        if command == "navigation.goToLine" {
            if self.view.file_prompt().is_some() {
                let damage = self.view.set_notice("Finish or cancel the file prompt before Go to Line".into());
                self.apply(damage);
                return true;
            }
            self.close_search();
            if let Some(palette) = &mut self.palette {
                let _ = palette.input(kwil_view_model::palette_session::PaletteInput::Cancel);
            }
            let mut model = Palette::new(1);
            // Use the existing bounded parser; Line mode never submits work.
            if let Err(error) = model.open(":") {
                let damage = self.view.set_notice(format!("Go to Line: {error:?}"));
                self.apply(damage);
                return true;
            }
            self.goto_epoch = self.goto_epoch.checked_add(1).expect("goto epoch exhausted");
            self.goto =
                Some(GoToPrompt { model, document: self.view.active_id(), workspace_epoch: self.view.input_epoch() });
            self.ime_destination = Some(ime::Destination::Cancelled);
            self.search_ime = None;
            self.composing = false;
            self.drag = None;
            self.selecting = false;
            self.menu_set(menu::State::Closed, false);
            if let Some(keys) = &mut self.keys {
                keys.cancel_pending();
                keys.set_context(kwil_core_commands::Context::Palette);
            }
            self.palette_redraw();
            return true;
        }
        if !self.view.recovery_ready() {
            let damage = self.view.set_notice("Workspace recovery is not ready; try the palette again shortly".into());
            self.apply(damage);
            return true;
        }
        self.close_search();
        self.goto = None;
        let tabs = self
            .view
            .session()
            .tabs()
            .iter()
            .map(|tab| kwil_core_commands::palette::Tab { id: tab.id.0, title: self.view.tab_title(tab.id) })
            .collect();
        let result =
            self.palette.as_mut().ok_or("Palette is starting; try again shortly".to_owned()).and_then(|palette| {
                palette.set_tabs(tabs).map_err(|error| format!("Palette: {error:?}"))?;
                palette.open(query).map_err(|error| format!("Palette: {error:?}"))
            });
        if let Err(error) = result {
            let damage = self.view.set_notice(error);
            self.apply(damage);
        } else {
            if self.ime_destination.is_some() || self.composing {
                self.ime_destination = Some(ime::Destination::Cancelled);
                self.composing = false;
            }
            self.drag = None;
            self.selecting = false;
            if let Some(keys) = &mut self.keys {
                keys.set_context(kwil_core_commands::Context::Palette);
            }
            self.palette_redraw();
        }
        true
    }

    pub(super) fn palette_input(&mut self, input: kwil_view_model::palette_session::PaletteInput<'_>) {
        if self.goto.is_some() {
            self.goto_input(input);
            return;
        }
        let Some(palette) = &mut self.palette else { return };
        let result = palette.input(input);
        let closed = !palette.model().is_open();
        if closed && let Some(keys) = &mut self.keys {
            keys.set_context(kwil_core_commands::Context::Editor);
        }
        self.palette_redraw();
        match result {
            Ok(Some(action)) => self.palette_action(action),
            Ok(None) => {}
            Err(error) => {
                let damage = self.view.set_notice(format!("Palette: {error:?}"));
                self.apply(damage);
            }
        }
    }

    pub(super) fn palette_action(&mut self, action: kwil_core_commands::palette::PaletteAction) {
        use kwil_core_commands::palette::PaletteAction;
        match action {
            PaletteAction::Command(command) => {
                if self.dispatch_compare(command) {
                    return;
                }
                if self.view.compare.open && (command.starts_with("edit.") || command.starts_with("search.")) {
                    let damage = self.view.set_notice("Close the read-only comparison to edit or search".into());
                    self.apply(damage);
                    return;
                }
                if matches!(command, "update.check" | "update.apply") {
                    let proxy = self.proxy.clone();
                    let started = kwil_update::spawn_check(env!("CARGO_PKG_VERSION"), move |message| {
                        let _ = proxy.send_event(UserEvent::Notice(message));
                    });
                    let notice = if started { "Checking for updates…" } else { "An update check is already running" };
                    let damage = self.view.set_notice(notice.into());
                    self.apply(damage);
                    return;
                }
                if command == "view.retryWrap" && self.wrap_command(command, 1, false) {
                    return;
                }
                if self.dispatch_hex(command) {
                    return;
                }
                self.close_language();
                if self.dispatch_language(command)
                    || self.dispatch_logs(command)
                    || self.dispatch_markdown(command)
                    || self.dispatch_search(command)
                    || self.dispatch_settings(command)
                    || self.dispatch_palette(command)
                {
                    return;
                }
                use kwil_core_commands::vim::VimResult;
                let result = self.keys.as_mut().map(|keys| keys.invoke_named(command));
                match result {
                    Some(VimResult::Command(call))
                        if self.settings.keymap == kwil_core_commands::Preset::Vim && model_vim_command(call.id) =>
                    {
                        if self.logs.owns_body() && call.id.starts_with("edit.") {
                            return;
                        }
                        if self.preview_blocks_command(call.id) {
                            return;
                        }
                        if self.wrap_vim_command(&call) {
                            return;
                        }
                        if self.preview_blocks_command(call.id) {
                            return;
                        }
                        if let Some(editor) = self.view.editor_mut() {
                            match editor.execute_vim(&call) {
                                Ok(result) => {
                                    let damage = self.view.refresh_after_edit(result.damage);
                                    let damage = self.wrap_model_damage(damage);
                                    self.apply(damage);
                                    self.keep_caret_visible();
                                    self.command_status(result.status);
                                }
                                Err(error) => {
                                    let damage = self.view.set_notice(error.to_string());
                                    self.apply(damage);
                                }
                            }
                        }
                    }
                    Some(VimResult::Error(error)) => {
                        let damage = self.view.set_notice(error);
                        self.apply(damage);
                    }
                    Some(VimResult::Pending) => {
                        self.apply(Damage { status: true, ..Default::default() });
                    }
                    _ => self.edit(input::Action::Command(command)),
                }
            }
            PaletteAction::OpenFile(path) => {
                if let Err(error) = self.view.open_path(path) {
                    self.view.set_notice(error.to_string());
                }
                self.sync_file_ui();
                self.apply(Damage { full: true, status: true, ..Default::default() });
            }
            PaletteAction::ActivateTab(id) => {
                if !self.view.activate(kwil_view_model::SessionDocumentId(id)) {
                    self.view.set_notice("Palette tab was closed".into());
                }
                self.sync_file_ui();
                self.apply(Damage { full: true, status: true, ..Default::default() });
            }
            PaletteAction::GoTo { line, column } => {
                self.wrap_sync_live();
                if !self.wrap_mutation_allowed() {
                    return;
                }
                // The existing View queues a goto until indexing attaches its
                // Editor. Reveal its horizontal window on that addressed event.
                let target = (self.view.active_id(), self.view.input_epoch());
                let result = self.view.execute(
                    "navigation.goToLine",
                    &CommandArgs { line: Some(line), column: Some(column), ..Default::default() },
                    1,
                );
                match result {
                    Ok(result) => {
                        self.goto_reveal = (result.status == CommandStatus::IndexNotReady).then_some(target);
                        let damage = self.wrap_model_damage(result.damage);
                        self.apply(damage);
                        self.keep_caret_visible();
                        self.command_status(result.status);
                    }
                    Err(error) => {
                        let damage = self.view.set_notice(error.to_string());
                        self.apply(damage);
                    }
                }
            }
        }
    }

    pub(super) fn palette_key(&mut self, event: &HostKey<'_>) -> bool {
        if event.repeat && matches!(event.logical_key, Key::Named(NamedKey::Enter | NamedKey::Escape)) {
            return self.palette_open();
        }
        if !self.palette_open() {
            return false;
        }
        use kwil_view_model::palette_session::PaletteInput;
        if self.keys.is_none() {
            self.keys =
                kwil_core_commands::keymap::KeymapEngine::new(self.settings.keymap, &self.settings.bindings).ok();
        }
        if let Some(chord) = input::chord(&event.logical_key, self.mods)
            && let Some(keys) = &mut self.keys
        {
            keys.set_context(kwil_core_commands::Context::Palette);
            use kwil_core_commands::vim::VimResult;
            match keys.feed(chord) {
                VimResult::Pending => return true,
                VimResult::Command(call) => {
                    let input = match call.id {
                        "palette.accept" => Some(PaletteInput::Accept),
                        "palette.cancel" => Some(PaletteInput::Cancel),
                        "palette.next" => Some(PaletteInput::Next),
                        "palette.previous" => Some(PaletteInput::Previous),
                        "edit.backspace" => Some(PaletteInput::Backspace),
                        "edit.delete" => Some(PaletteInput::Delete),
                        "motion.left" => Some(PaletteInput::Left),
                        "motion.right" => Some(PaletteInput::Right),
                        "motion.lineStart" => Some(PaletteInput::Home),
                        "motion.lineEnd" => Some(PaletteInput::End),
                        _ => None,
                    };
                    if let Some(input) = input {
                        self.palette_input(input);
                    } else if call.id == "edit.paste" {
                        self.palette_paste();
                    } else if call.id.starts_with("edit.") || call.id.starts_with("motion.") {
                        let damage =
                            self.view.set_notice("This editor shortcut is unavailable in the palette query".into());
                        self.apply(damage);
                    } else {
                        self.palette_input(PaletteInput::Cancel);
                        self.palette_action(kwil_core_commands::palette::PaletteAction::Command(call.id));
                    }
                    return true;
                }
                VimResult::Error(error) => {
                    let damage = self.view.set_notice(error);
                    self.apply(damage);
                    return true;
                }
                VimResult::Unbound if keys.suppresses_fallback() => return true,
                VimResult::Unbound => {}
            }
        }
        let input = match event.logical_key {
            Key::Named(NamedKey::Backspace) => Some(PaletteInput::Backspace),
            Key::Named(NamedKey::Delete) => Some(PaletteInput::Delete),
            Key::Named(NamedKey::ArrowLeft) => Some(PaletteInput::Left),
            Key::Named(NamedKey::ArrowRight) => Some(PaletteInput::Right),
            Key::Named(NamedKey::Home) => Some(PaletteInput::Home),
            Key::Named(NamedKey::End) => Some(PaletteInput::End),
            _ if !self.mods.super_key() && (!self.mods.control_key() || self.mods.alt_key()) => {
                event.text.filter(|text| !text.chars().any(char::is_control)).map(PaletteInput::Text)
            }
            _ => None,
        };
        if let Some(input) = input {
            self.palette_input(input);
        }
        true
    }

    fn palette_paste(&mut self) {
        let hwnd = self.gfx.as_ref().map_or(0, |gfx| gfx.hwnd);
        let result = clipboard::SystemClipboard(hwnd).get_text(kwil_core_commands::palette::MAX_QUERY_BYTES);
        match result {
            Ok(Some(text)) => self.palette_input(kwil_view_model::palette_session::PaletteInput::Text(&text)),
            Ok(None) => {}
            Err(error) => {
                let damage = self.view.set_notice(error.to_string());
                self.apply(damage);
            }
        }
    }

    pub(super) fn palette_mouse(&mut self) {
        if self.goto.is_some() {
            if self.palette_area.is_some_and(|area| area.contains(self.pointer)) {
                self.palette_input(kwil_view_model::palette_session::PaletteInput::Accept);
            } else {
                self.palette_input(kwil_view_model::palette_session::PaletteInput::Cancel);
            }
            return;
        }

        // Rows are resolved against the painted card, not an editor-cell grid.
        self.palette_pointer();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepted_frontend_actions_keep_clipboard_and_navigation_adapters() {
        for id in ["edit.copy", "edit.cut", "edit.paste", "navigation.goToLine", "file.save", "view.zoomIn"] {
            assert!(!model_vim_command(id), "assertion: {id} must retain its frontend adapter");
        }
        assert!(model_vim_command("mode.insert"));
        assert!(model_vim_command("motion.wordForward"));
    }
    #[test]
    fn dismissed_overlay_blocks_scroll_copy_until_restored() {
        assert!(palette_blocks_copy(false, true), "assertion: accepted actions still have displayed popup pixels");
        assert!(palette_blocks_copy(true, false));
        assert!(!palette_blocks_copy(false, false));
    }

    #[test]
    fn palette_paste_binding_respects_unbind_and_pending_chords() {
        use kwil_core_commands::{Binding, Context, Preset, keymap::KeymapEngine, vim::VimResult};
        for preset in [Preset::VsCode, Preset::NotepadPlusPlus, Preset::Vim] {
            let mut keys = KeymapEngine::new(preset, &[]).unwrap();
            keys.set_context(Context::Palette);
            for chord in ["ctrl+v", "cmd+v"] {
                assert!(
                    matches!(keys.feed(chord.parse().unwrap()), VimResult::Command(call) if call.id == "edit.paste")
                );
            }
            let mut keys = KeymapEngine::new(
                preset,
                &[
                    Binding {
                        keys: "ctrl+v".into(),
                        command: String::new(),
                        context: Some(Context::Palette),
                        mode: None,
                    },
                    Binding {
                        keys: "ctrl+v ctrl+s".into(),
                        command: "file.save".into(),
                        context: Some(Context::Palette),
                        mode: None,
                    },
                ],
            )
            .unwrap();
            keys.set_context(Context::Palette);
            assert!(matches!(keys.feed("ctrl+v".parse().unwrap()), VimResult::Pending));
            assert!(matches!(keys.feed("ctrl+s".parse().unwrap()), VimResult::Command(call) if call.id == "file.save"));
        }
    }
}

#[cfg(test)]
mod command_contract {
    use kwil_core_commands::palette::PaletteAction;

    /// Every command the palette offers runs in the GUI: none may answer
    /// "command not implemented" or panic. Hide unfinished features through
    /// `NOT_HOSTED` instead.
    #[test]
    fn every_palette_command_runs_in_the_gui() {
        let mut broken = Vec::new();
        for command in kwil_core_commands::COMMANDS.iter().filter(|c| kwil_core_commands::in_palette(c.id)) {
            if matches!(command.id, "file.quit" | "file.saveAndQuit") {
                continue;
            }
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let (mut app, _events) = crate::search_ui::host_dispatch_tests::host("alpha beta\n");
                app.palette_action(PaletteAction::Command(command.id));
                app.view.status_text()
            }));
            match outcome {
                Ok(status) if status.contains("not implemented") => broken.push(format!("{}: {status}", command.id)),
                Ok(_) => {}
                Err(_) => broken.push(format!("{}: panicked", command.id)),
            }
        }
        assert!(broken.is_empty(), "palette commands without a GUI host:\n{}", broken.join("\n"));
    }
}
