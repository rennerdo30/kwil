//! Native dirty-close confirmation, fenced to the exact document incarnation.
use super::*;
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CloseTicket {
    document: kwil_view_model::SessionDocumentId,
    epoch: u64,
    revision: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Ticket {
    Close(CloseTicket),
    Binary(binary_offer::Prompt),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Choice {
    Save,
    Discard,
    Cancel,
    Hex,
    Text,
}
impl Choice {
    #[cfg(windows)]
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
impl CloseTicket {
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
impl Ticket {
    fn capture(view: &Workspace) -> Option<Self> {
        CloseTicket::capture(view).map(Self::Close)
    }
    fn matches(&self, view: &Workspace) -> bool {
        match self {
            Self::Close(ticket) => ticket.matches(view),
            Self::Binary(prompt) => {
                prompt.epoch == view.input_epoch()
                    && view.binary_offer().is_some_and(|offer| offer.ticket == prompt.ticket)
            }
        }
    }
    fn same_request(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Close(a), Self::Close(b)) => a == b,
            (Self::Binary(a), Self::Binary(b)) => a.ticket == b.ticket && a.epoch == b.epoch,
            _ => false,
        }
    }
}
impl App {
    pub(super) fn sync_close_dialog(&mut self) {
        self.sync_binary_input();
        if self.file_dialogs.exiting() || self.file_dialogs.busy() {
            return;
        }
        if let Some(Ticket::Binary(prompt)) = &self.close_dialog
            && !prompt.native
            && (prompt.epoch != self.view.input_epoch()
                || !self.binary_owns_input()
                || !self.view.binary_offer().is_some_and(|offer| offer.ticket == prompt.ticket))
        {
            self.close_dialog = None;
        }
        if self.close_dialog.is_some() {
            return;
        }
        if let Some(offer) = self.view.binary_offer().filter(|_| self.binary_owns_input()) {
            let ticket = Ticket::Binary(binary_offer::Prompt::new(offer, self.view.input_epoch()));
            // An owned card remains available on unsupported/hidden surfaces;
            // hidden mode never reaches the OS TaskDialog launch below.
            self.close_dialog = Some(ticket.clone());
            self.apply(Damage { full: true, status: true, ..Default::default() });
            if cfg!(windows) && !self.opts.hidden {
                let owner = self.gfx.as_ref().map_or(0, |gfx| gfx.hwnd);
                if owner != 0 {
                    self.launch_confirmation(ticket, owner, "this file".into());
                }
            }
            return;
        }
        if !cfg!(windows) {
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
        self.launch_confirmation(ticket, owner, label);
    }
    fn launch_confirmation(&mut self, ticket: Ticket, owner: isize, label: String) {
        self.submit_confirmation(ticket, move |reply_ticket, proxy| {
            std::thread::Builder::new()
                .name("kwil-close-confirmation".into())
                .spawn(move || {
                    let outcome = show(owner, &label, &reply_ticket);
                    let _ = proxy.send_event(UserEvent::CloseDialog(Reply { ticket: reply_ticket, outcome }));
                })
                .map(|_| ())
                .map_err(|error| error.to_string())
        });
    }
    /// One nonblocking admission point shared by the native worker and fake port.
    pub(super) fn submit_confirmation(
        &mut self,
        mut ticket: Ticket,
        submit: impl FnOnce(Ticket, HostProxy) -> Result<(), String>,
    ) -> bool {
        if self.opts.hidden
            || self.confirmation_worker_busy()
            || self.file_dialogs.busy()
            || self.file_dialogs.exiting()
        {
            return false;
        }
        if !ticket.matches(&self.view) || (matches!(ticket, Ticket::Binary(_)) && !self.binary_owns_input()) {
            return false;
        }
        if let Ticket::Binary(prompt) = &mut ticket {
            prompt.native = true;
        }
        match submit(ticket.clone(), self.proxy.clone()) {
            Ok(()) => {
                self.close_dialog = Some(ticket);
                ime::cancel(&mut self.composing, &mut self.ime_destination);
                self.drag = None;
                self.selecting = false;
                true
            }
            Err(error) => {
                if matches!(ticket, Ticket::Close(_)) {
                    self.view.cancel_close_intent();
                }
                self.view.set_notice(format!("Cannot open confirmation: {error}"));
                false
            }
        }
    }
    pub(super) fn close_dialog_event(&mut self, reply: Reply) {
        if !self.close_dialog.as_ref().is_some_and(|ticket| ticket.same_request(&reply.ticket)) {
            return;
        }
        self.close_dialog = None;
        if let Ticket::Binary(prompt) = reply.ticket {
            self.binary_confirmation(prompt, reply.outcome);
            return;
        }
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
            Ok(Choice::Hex | Choice::Text) => {}
        }
        self.sync_file_ui();
    }
    pub(super) fn file_modal(&self) -> bool {
        self.binary_owns_input() || self.close_dialog.is_some() || self.file_dialogs.blocks_input(&self.view)
    }
    pub(super) fn confirmation_worker_busy(&self) -> bool {
        matches!(&self.close_dialog, Some(Ticket::Close(_)))
            || matches!(&self.close_dialog, Some(Ticket::Binary(prompt)) if prompt.native)
    }
}
#[cfg(windows)]
fn show(owner: isize, label: &str, purpose: &Ticket) -> Result<Choice, String> {
    use windows_sys::Win32::UI::Controls::*;
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let title = wide("Kwil");
    let binary = matches!(purpose, Ticket::Binary(_));
    let heading =
        wide(&if binary { "This file appears to be binary".into() } else { format!("Save changes to {label}?") });
    let content = wide(if binary {
        "Choose how to open it. Hex shows the original physical bytes."
    } else {
        "Your changes will be lost if you don’t save them."
    });
    let save = wide(if binary { "Hex" } else { "Save" });
    let discard = wide(if binary { "Text" } else { "Don’t save" });
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
    Ok(if binary {
        match button {
            100 => Choice::Hex,
            101 => Choice::Text,
            _ => Choice::Cancel,
        }
    } else {
        Choice::from_button(button)
    })
}
#[cfg(not(windows))]
fn show(_: isize, _: &str, _: &Ticket) -> Result<Choice, String> {
    Err("native confirmation unavailable; use the owned prompt".into())
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
