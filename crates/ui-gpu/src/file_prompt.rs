//! File/tab actions stay outside editor command dispatch and renderer geometry.
use super::*;

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
        self.sync_file_ui();
        self.apply(Damage { full: true, status: true, ..Default::default() });
        true
    }

    pub(super) fn sync_file_ui(&mut self) {
        self.save_path = self.view.file_prompt().map(|p| p.text.clone());
        self.close_pending = self.view.close_pending();
        self.drag = None;
        self.selecting = false;
        // Each View retains its byte window, so a new tab never inherits the
        // old tab's horizontal scrollbar state.
        self.horizontal = self.view.line_offset(0);
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
