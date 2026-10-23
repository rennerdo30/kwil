//! Native dirty-close confirmation, fenced to the exact document incarnation.
use super::*;
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Ticket {
    document: kwil_view_model::SessionDocumentId,
    epoch: u64,
    revision: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Choice {
    Save,
    Discard,
    Cancel,
}
impl Choice {
    fn from_button(button: i32) -> Self {
        match button {
            100 => Self::Save,
            101 => Self::Discard,
            _ => Self::Cancel,
        }
    }
}
pub(super) struct Reply {
    pub ticket: Ticket,
    pub outcome: Result<Choice, String>,
}
impl Ticket {
    fn capture(view: &Workspace) -> Option<Self> {
        let editor = view.editor()?;
        if editor.has_pending_edit_receipt() || editor.has_pending_save_completion() {
            return None;
        }
        (view.close_pending()
            && editor.document().is_dirty()
            && !view.file_saves_pending()
            && view.file_prompt().is_none())
        .then(|| Self { document: view.active_id(), epoch: view.input_epoch(), revision: editor.document().revision() })
    }
    fn matches(&self, view: &Workspace) -> bool {
        Self::capture(view).as_ref() == Some(self)
    }
}
impl App {
    pub(super) fn sync_close_dialog(&mut self) {
        if !cfg!(windows) || self.close_dialog.is_some() || self.file_dialogs.exiting() {
            return;
        }
        let Some(ticket) = Ticket::capture(&self.view) else {
            return;
        };
        if self.opts.hidden {
            return;
        } // Never open a visible system dialog in an agent harness.
        let owner = self.gfx.as_ref().map_or(0, |gfx| gfx.hwnd);
        if owner == 0 {
            return;
        }
        let label = self
            .view
            .editor()
            .and_then(|e| e.document().path())
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled".into());
        let proxy = self.proxy.clone();
        let reply_ticket = ticket.clone();
        match std::thread::Builder::new().name("kwil-close-confirmation".into()).spawn(move || {
            let outcome = show(owner, &label);
            let _ = proxy.send_event(UserEvent::CloseDialog(Reply { ticket: reply_ticket, outcome }));
        }) {
            Ok(_handle) => {
                self.close_dialog = Some(ticket);
                ime::reset(&mut self.composing, &mut self.ime_destination);
                self.drag = None;
                self.selecting = false;
            }
            Err(error) => {
                self.view.cancel_close_intent();
                self.view.set_notice(format!("Cannot open confirmation: {error}"));
            }
        }
    }
    pub(super) fn close_dialog_event(&mut self, reply: Reply) {
        if self.close_dialog.as_ref() != Some(&reply.ticket) {
            return;
        }
        self.close_dialog = None;
        if !reply.ticket.matches(&self.view) {
            self.view.cancel_close_intent();
            self.view.set_notice("Document changed while confirming close; close cancelled".into());
            return;
        }
        match reply.outcome {
            Ok(Choice::Save) => {
                self.dispatch_file("file.save");
            }
            Ok(Choice::Discard) => {
                if let Err(error) = self.view.discard_pending_close() {
                    self.view.set_notice(error.to_string());
                }
            }
            Ok(Choice::Cancel) => {
                self.view.cancel_close_intent();
                self.view.set_notice(String::new());
            }
            Err(error) => {
                self.view.cancel_close_intent();
                self.view.set_notice(format!("Close dialog: {error}"));
            }
        }
        self.sync_file_ui();
    }
    pub(super) fn file_modal(&self) -> bool {
        self.close_dialog.is_some() || self.file_dialogs.blocks_input(&self.view)
    }
}
#[cfg(windows)]
fn show(owner: isize, label: &str) -> Result<Choice, String> {
    use windows_sys::Win32::UI::Controls::*;
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let title = wide("Kwil");
    let heading = wide(&format!("Save changes to {label}?"));
    let content = wide("Your changes will be lost if you don’t save them.");
    let save = wide("Save");
    let discard = wide("Don’t save");
    let buttons = [
        TASKDIALOG_BUTTON { nButtonID: 100, pszButtonText: save.as_ptr() },
        TASKDIALOG_BUTTON { nButtonID: 101, pszButtonText: discard.as_ptr() },
    ];
    let config = TASKDIALOGCONFIG {
        cbSize: std::mem::size_of::<TASKDIALOGCONFIG>() as u32,
        hwndParent: owner as _,
        dwFlags: TDF_ALLOW_DIALOG_CANCELLATION | TDF_POSITION_RELATIVE_TO_WINDOW,
        dwCommonButtons: TDCBF_CANCEL_BUTTON,
        pszWindowTitle: title.as_ptr(),
        pszMainInstruction: heading.as_ptr(),
        pszContent: content.as_ptr(),
        cButtons: buttons.len() as u32,
        pButtons: buttons.as_ptr(),
        nDefaultButton: 100,
        ..Default::default()
    };
    let mut button = 0;
    let result = unsafe { TaskDialogIndirect(&config, &mut button, std::ptr::null_mut(), std::ptr::null_mut()) };
    if result < 0 {
        return Err(format!("Windows task dialog failed (0x{:08X})", result as u32));
    }
    Ok(Choice::from_button(button))
}
#[cfg(not(windows))]
fn show(_: isize, _: &str) -> Result<Choice, String> {
    Ok(Choice::from_button(0))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn close_acceptance_is_bound_to_dirty_revision_and_tab() {
        let mut view = Workspace::new(View::new(None)).unwrap();
        view.execute("edit.insert", &CommandArgs { text: Some("keep me".into()), ..Default::default() }, 1).unwrap();
        view.file_command("file.close").unwrap();
        let ticket = Ticket::capture(&view).unwrap();
        assert!(ticket.matches(&view));
        view.execute("edit.insert", &CommandArgs { text: Some("new".into()), ..Default::default() }, 1).unwrap();
        assert!(!ticket.matches(&view));
        view.cancel_close_intent();
        assert!(Ticket::capture(&view).is_none());
    }
}
