//! Binary offers use the existing confirmation slot and desktop card primitives.
//! Only scalar tickets/geometry live here; Workspace owns every source/result.
use super::*;
use kwil_view_model::document_worker::binary_offer::{Choice, Offer, Ticket};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Prompt {
    pub ticket: Ticket,
    pub epoch: u64,
    pub native: bool,
    selected: usize,
    surface: Option<(usize, usize, u64)>,
    buttons: [Option<Rect>; 3],
}
impl Prompt {
    pub fn new(offer: Offer, epoch: u64) -> Self {
        Self { ticket: offer.ticket, epoch, native: false, selected: 0, surface: None, buttons: [None; 3] }
    }
}
const CHOICES: [Choice; 3] = [Choice::Hex, Choice::Text, Choice::Cancel];

impl App {
    pub(super) fn binary_pending(&self) -> bool {
        self.view.binary_offer_pending(self.view.active_id())
    }
    /// Worker preview authority permits painting before the full open/index
    /// completion. It never grants input or editor/source ownership to the UI.
    pub(super) fn binary_blocks_paint(&self) -> bool {
        self.binary_pending() && !self.view.binary_preview_ready(self.view.active_id())
    }
    pub(super) fn binary_owns_input(&self) -> bool {
        self.binary_pending()
            && self.view.file_prompt().is_none()
            && !self.view.close_pending()
            && !self.file_dialogs.busy()
            && !self.settings_open()
            && self.desktop_window.is_none()
            && !self.welcome.visible()
            && !self.about.open
    }
    pub(super) fn sync_binary_input(&mut self) {
        if self.binary_owns_input() {
            // Preserve a cancelled destination beyond the final worker receipt:
            // an old IME Commit must never recapture the newly installed editor.
            ime::cancel(&mut self.composing, &mut self.ime_destination);
            self.search_ime = None;
            self.logs_ime = None;
            self.hex_cancel_input();
            self.drag = None;
            self.selecting = false;
            self.wheel.clear();
            if let Some(keys) = &mut self.keys {
                keys.cancel_pending();
            }
        }
    }
    pub(super) fn binary_confirmation(&mut self, mut prompt: Prompt, outcome: Result<close_dialog::Choice, String>) {
        if prompt.epoch != self.view.input_epoch()
            || !self.view.binary_offer().is_some_and(|offer| offer.ticket == prompt.ticket)
        {
            self.sync_file_ui();
            return;
        }
        let choice = match outcome {
            Ok(close_dialog::Choice::Hex) => Some(Choice::Hex),
            Ok(close_dialog::Choice::Text) => Some(Choice::Text),
            Ok(close_dialog::Choice::Cancel) => Some(Choice::Cancel),
            _ => None,
        };
        let result = match choice {
            Some(choice) => self.view.resolve_binary_offer(prompt.ticket, choice, self.text_rows()),
            None => Err(std::io::Error::other("Binary dialog unavailable; choose Hex, Text or Cancel below")),
        };
        match result {
            Ok(damage) => self.apply(damage),
            Err(error) => {
                // Keep this exact ticket retryable. Do not automatically reopen
                // a failed OS dialog or discard a rejected worker admission.
                prompt.native = false;
                prompt.surface = None;
                prompt.buttons = [None; 3];
                self.close_dialog = Some(close_dialog::Ticket::Binary(prompt));
                self.view.set_notice(error.to_string());
            }
        }
        self.sync_file_ui();
        self.apply(Damage { full: true, status: true, ..Default::default() });
    }
    fn binary_choose(&mut self, choice: Choice) {
        let Some(close_dialog::Ticket::Binary(prompt)) = self.close_dialog.as_ref() else { return };
        if prompt.native {
            return;
        }
        let ticket = close_dialog::Ticket::Binary(prompt.clone());
        let choice = match choice {
            Choice::Hex => close_dialog::Choice::Hex,
            Choice::Text => close_dialog::Choice::Text,
            Choice::Cancel => close_dialog::Choice::Cancel,
        };
        self.close_dialog_event(close_dialog::Reply { ticket, outcome: Ok(choice) });
    }
    pub(super) fn binary_key(&mut self, ev: &HostKey<'_>) -> bool {
        if !self.binary_owns_input() {
            return false;
        }
        self.sync_binary_input();
        let Some(close_dialog::Ticket::Binary(prompt)) = self.close_dialog.as_mut() else { return true };
        if prompt.native {
            return true;
        }
        let choice = match &ev.logical_key {
            Key::Named(NamedKey::Escape) => Some(Choice::Cancel),
            Key::Named(NamedKey::Enter) => Some(CHOICES[prompt.selected]),
            Key::Named(NamedKey::Tab | NamedKey::ArrowRight | NamedKey::ArrowDown) => {
                prompt.selected = (prompt.selected + if self.mods.shift_key() { 2 } else { 1 }) % 3;
                None
            }
            Key::Named(NamedKey::ArrowLeft | NamedKey::ArrowUp) => {
                prompt.selected = (prompt.selected + 2) % 3;
                None
            }
            Key::Character(text) if !self.mods.control_key() && !self.mods.super_key() && !self.mods.alt_key() => {
                match text.to_ascii_lowercase().as_str() {
                    "h" => Some(Choice::Hex),
                    "t" => Some(Choice::Text),
                    "c" => Some(Choice::Cancel),
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some(choice) = choice {
            self.binary_choose(choice);
        }
        self.apply(Damage { status: true, ..Default::default() });
        true
    }
    /// Consume before Welcome/settings/menu/pointer/IME dispatch, even while
    /// detecting and resolving, when there is no offered card yet.
    pub(super) fn binary_window_input(&mut self, ev: &WindowEvent) -> bool {
        if !self.binary_owns_input() || !desktop_window::owns_input(ev) {
            return false;
        }
        self.sync_binary_input();
        match ev {
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                self.binary_key(&HostKey {
                    logical_key: event.logical_key.clone(),
                    text: event.text.as_deref(),
                    repeat: event.repeat,
                });
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.pointer = (position.x.max(0.0) as usize, position.y.max(0.0) as usize)
            }
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } => {
                let surface = (self.size.0, self.size.1, self.ui.scale.to_bits());
                let choice = match self.close_dialog.as_ref() {
                    Some(close_dialog::Ticket::Binary(prompt)) if !prompt.native && prompt.surface == Some(surface) => {
                        prompt
                            .buttons
                            .iter()
                            .position(|area| area.is_some_and(|area| area.contains(self.pointer)))
                            .map(|i| CHOICES[i])
                    }
                    _ => None,
                };
                if let Some(choice) = choice {
                    self.binary_choose(choice);
                }
            }
            _ => {}
        }
        true
    }
}

/// Same card, theme, font and controls as the existing file prompt. Stacked
/// choices keep all three visible at narrow widths; keys work before painting.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw(
    state: &mut Option<close_dialog::Ticket>,
    pending: bool,
    offer: Option<Offer>,
    pixels: &mut [u32],
    size: (usize, usize),
    font: &mut font::UiFont,
    theme: &kwil_core_commands::theme::Theme,
    m: &chrome::Metrics,
) -> Option<Rect> {
    if !pending {
        return None;
    }
    let Some(close_dialog::Ticket::Binary(prompt)) = state else { return None };
    prompt.surface = None;
    prompt.buttons = [None; 3];
    if prompt.native || !offer.is_some_and(|offer| offer.ticket == prompt.ticket) {
        return None;
    }
    let c = desktop::Colors::new(theme);
    let s = desktop::Sizes::new(m);
    let w = size.0.saturating_sub(2 * s.gap).min(m.dip(520.0));
    let h = s.pad * 2 + s.text * 3 + s.gap * 4 + s.control * 3;
    if w < m.dip(200.0) || h + 2 * s.gap > size.1 {
        return None;
    }
    let area = Rect { x: (size.0 - w) / 2, y: (size.1 - h) / 2, w, h };
    desktop::card(pixels, size.0, area, &c, &s);
    let inner = desktop::inset(area, s.pad);
    let mut line = Rect { h: s.text, ..inner };
    desktop::label(pixels, size.0, font, line, "This file appears to be binary", c.fg);
    line.y += s.text;
    desktop::label(pixels, size.0, font, line, "Choose how to open it.", c.muted);
    line.y += s.text + s.gap;
    for (i, label) in ["Hex (H) — original bytes", "Text (T)", "Cancel (Esc)"].into_iter().enumerate() {
        let button = Rect { y: line.y, h: s.control, ..inner };
        desktop::button(
            pixels,
            size.0,
            font,
            button,
            label,
            if prompt.selected == i { desktop::Style::Primary } else { desktop::Style::Secondary },
            false,
            &c,
            &s,
        );
        prompt.buttons[i] = Some(button);
        line.y += s.control + s.gap;
    }
    desktop::label(
        pixels,
        size.0,
        font,
        Rect { y: line.y, h: s.text, ..inner },
        "Tab / arrows choose • Enter confirms",
        c.faint,
    );
    prompt.surface = Some((size.0, size.1, m.scale.to_bits()));
    Some(area)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        path::PathBuf,
        sync::mpsc::Receiver,
        time::{Duration, Instant},
    };
    const BYTES: &[u8] = b"header\0binary\x01\n";
    fn until(app: &mut App, rx: &Receiver<UserEvent>, done: impl Fn(&App) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        for _ in 0..512 {
            if done(app) {
                return;
            }
            let remaining = deadline.checked_duration_since(Instant::now()).expect("binary GUI deadline");
            match rx.recv_timeout(remaining).expect("binary GUI worker wake") {
                UserEvent::Document(result) => {
                    let damage = app.view.document_result(*result);
                    app.sync_file_ui();
                    app.apply(damage);
                }
                _ => panic!("unexpected binary GUI test wake"),
            }
        }
        panic!("binary GUI finite receipt count");
    }
    fn host() -> (App, Receiver<UserEvent>, PathBuf, PathBuf, kwil_view_model::SessionDocumentId) {
        let (mut app, rx) = search_ui::host_dispatch_tests::host("prior editor");
        app.opts.hidden = true;
        let prior = app.view.active_id();
        let root = std::env::temp_dir().join(format!(
            "kwil-gui-binary-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("physical.bin");
        std::fs::write(&path, BYTES).unwrap();
        let proxy = app.proxy.clone();
        app.view
            .start_document_worker_at(Some(root.join("state")), move |result| {
                let _ = proxy.send_event(UserEvent::Document(Box::new(result)));
            })
            .unwrap();
        until(&mut app, &rx, |a| a.view.recovery_ready());
        (app, rx, root, path, prior)
    }
    fn open(app: &mut App, rx: &Receiver<UserEvent>, path: PathBuf) -> Offer {
        app.view.open_path(path).unwrap();
        app.sync_file_ui();
        assert!(app.binary_pending());
        assert!(app.view.editor().is_none());
        until(app, rx, |a| a.view.binary_offer().is_some());
        app.view.binary_offer().unwrap()
    }
    fn press(app: &mut App, key: Key) {
        app.key_after_settings(None, &HostKey { logical_key: key, text: None, repeat: false });
    }
    fn bytes(app: &App) -> Vec<u8> {
        let mut bytes = Vec::new();
        app.view.editor().unwrap().document().snapshot().write_to(&mut bytes).unwrap();
        bytes
    }
    fn queued_external(app: &mut App, rx: &Receiver<UserEvent>) -> usize {
        let mut wakes = 0;
        for _ in 0..512 {
            match rx.try_recv() {
                Ok(UserEvent::OpenExternal) => wakes += 1,
                Ok(UserEvent::Document(result)) => {
                    let damage = app.view.document_result(*result);
                    app.sync_file_ui();
                    app.apply(damage);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return wakes,
                _ => panic!("unexpected external-open test wake"),
            }
        }
        panic!("external-open test finite receipt count");
    }
    fn stop(mut app: App, rx: Receiver<UserEvent>, root: PathBuf) {
        until(&mut app, &rx, |a| !a.view.document_busy(a.view.active_id()) && a.view.recovery_notice().is_none());
        // The same accepted-exit ownership drain used by the production App.
        let deadline = Instant::now() + Duration::from_secs(10);
        for _ in 0..512 {
            if app.view.prepare_hex_shutdown() {
                break;
            }
            let event = rx
                .recv_timeout(deadline.checked_duration_since(Instant::now()).expect("binary GUI shutdown deadline"))
                .unwrap();
            if let UserEvent::Document(result) = event {
                app.view.document_result(*result);
            }
        }
        assert!(app.view.prepare_hex_shutdown());
        drop(app);
        for _ in 0..512 {
            let event = rx
                .recv_timeout(deadline.checked_duration_since(Instant::now()).expect("binary GUI stopped deadline"))
                .unwrap();
            if matches!(event, UserEvent::Document(result) if matches!(*result, kwil_view_model::document_worker::DocumentResult::Stopped(Ok(()))))
            {
                assert_eq!(root.parent(), Some(std::env::temp_dir().as_path()));
                assert!(root.file_name().unwrap().to_string_lossy().starts_with("kwil-gui-binary-"));
                std::fs::remove_dir_all(root).unwrap();
                return;
            }
        }
        panic!("binary GUI worker did not stop");
    }
    #[test]
    fn actual_gui_binary_offer_choices_hide_content_and_preserve_physical_bytes_and_cancel_history() {
        for choice in CHOICES {
            let (mut app, rx, root, path, prior) = host();
            app.ime_preedit("old composition");
            assert!(app.composing);
            let offer = open(&mut app, &rx, path.clone());
            assert!(app.file_modal());
            assert!(!app.confirmation_worker_busy(), "hidden must never launch an OS dialog");
            assert!(matches!(app.ime_destination, Some(ime::Destination::Cancelled)));
            assert!(app.view.line(0).unwrap_or_default().is_empty(), "binary is never a garbled text preview");
            app.mods = ModifiersState::CONTROL;
            for key in ["v", "s", "n", "f"] {
                press(&mut app, Key::Character(key.into()));
            }
            app.edit(input::Action::Text("paste must be blocked".into()));
            assert!(app.binary_window_input(&WindowEvent::Ime(Ime::Commit("late composition".into()))));
            assert_eq!(app.view.binary_offer(), Some(offer));
            assert!(app.view.file_prompt().is_none());
            assert_eq!(app.view.active_id(), offer.ticket.document());
            let closed = app.view.session().closed().len();
            app.mods = ModifiersState::empty();
            press(
                &mut app,
                match choice {
                    Choice::Hex => Key::Named(NamedKey::Enter),
                    Choice::Text => Key::Character("t".into()),
                    Choice::Cancel => Key::Named(NamedKey::Escape),
                },
            );
            assert!(app.close_dialog.is_none(), "accepted choice hides its modal immediately");
            assert!(app.view.binary_offer().is_none());
            until(&mut app, &rx, |a| {
                !a.view.binary_offer_pending(offer.ticket.document()) && !a.view.document_busy(a.view.active_id())
            });
            match choice {
                Choice::Hex | Choice::Text => {
                    assert_eq!(app.view.hex_active(), choice == Choice::Hex);
                    assert_eq!(bytes(&app), BYTES);
                    assert_eq!(std::fs::read(&path).unwrap(), BYTES);
                    let destination = app.ime_destination.take().expect("cancelled composition retained");
                    let _ = ime::commit(&mut app.view, destination, "must not recapture".into(), Duration::ZERO);
                    assert_eq!(bytes(&app), BYTES);
                }
                Choice::Cancel => {
                    assert_eq!(app.view.active_id(), prior);
                    assert_eq!(bytes(&app), b"prior editor");
                    assert_eq!(
                        app.view.session().closed().len(),
                        closed,
                        "cancelled placeholder is not closed-file history"
                    );
                }
            }
            stop(app, rx, root);
        }
    }
    #[test]
    fn actual_gui_binary_fake_port_coalesces_refuses_hidden_and_retries_failed_dialog() {
        let (mut app, rx, root, path, _) = host();
        let offer = open(&mut app, &rx, path);
        let ticket = app.close_dialog.clone().unwrap();
        let mut submits = 0;
        assert!(!app.submit_confirmation(ticket.clone(), |_, _| {
            submits += 1;
            Ok(())
        }));
        assert_eq!(submits, 0, "hidden policy is checked before the fake/native port");
        app.opts.hidden = false;
        for _ in 0..3 {
            assert!(!app.submit_confirmation(ticket.clone(), |_, _| {
                submits += 1;
                Err("fake admission full".into())
            }));
            assert_eq!(app.view.binary_offer(), Some(offer));
            assert!(!app.confirmation_worker_busy());
        }
        let mut last = None;
        for _ in 0..2 {
            let mut request = None;
            assert!(app.submit_confirmation(ticket.clone(), |ticket, _| {
                submits += 1;
                request = Some(ticket);
                Ok(())
            }));
            assert!(!app.submit_confirmation(ticket.clone(), |_, _| {
                submits += 1;
                Ok(())
            }));
            let ticket = request.unwrap();
            app.close_dialog_event(close_dialog::Reply {
                ticket: ticket.clone(),
                outcome: Err("fake OS failure".into()),
            });
            assert_eq!(app.view.binary_offer(), Some(offer));
            assert!(!app.confirmation_worker_busy());
            last = Some(ticket);
        }
        assert_eq!(submits, 5, "repeated failures remain retryable without parallel workers or a queue");
        app.opts.hidden = true;
        press(&mut app, Key::Named(NamedKey::Escape));
        until(&mut app, &rx, |a| !a.view.binary_offer_pending(offer.ticket.document()));
        app.close_dialog_event(close_dialog::Reply { ticket: last.unwrap(), outcome: Ok(close_dialog::Choice::Text) });
        assert!(!app.binary_pending(), "late duplicate cannot consume a new owner");
        stop(app, rx, root);
    }
    #[test]
    fn actual_gui_binary_stale_tab_roundtrip_and_unpainted_pointer_cannot_choose() {
        let (mut app, rx, root, path, prior) = host();
        let offer = open(&mut app, &rx, path.clone());
        let old = app.close_dialog.clone().unwrap();
        assert!(app.view.activate(prior));
        assert!(app.view.activate(offer.ticket.document()));
        app.sync_file_ui();
        let current = app.close_dialog.clone().unwrap();
        assert_ne!(old, current, "frontend epoch fences even a roundtrip to the same backend offer");
        app.close_dialog_event(close_dialog::Reply { ticket: old, outcome: Ok(close_dialog::Choice::Hex) });
        assert_eq!(app.close_dialog, Some(current.clone()));
        assert_eq!(app.view.binary_offer(), Some(offer));
        assert!(app.binary_window_input(&WindowEvent::MouseInput {
            device_id: winit::event::DeviceId::dummy(),
            state: ElementState::Pressed,
            button: MouseButton::Left
        }));
        assert_eq!(app.view.binary_offer(), Some(offer), "unpainted pointer cannot act on old geometry");
        app.binary_choose(Choice::Cancel);
        until(&mut app, &rx, |a| !a.view.binary_offer_pending(offer.ticket.document()));
        let reopened = open(&mut app, &rx, path);
        assert_ne!(reopened.ticket, offer.ticket, "reopen has an independent generation");
        app.close_dialog_event(close_dialog::Reply { ticket: current, outcome: Ok(close_dialog::Choice::Text) });
        assert_eq!(
            app.view.binary_offer(),
            Some(reopened),
            "late closed-file callback cannot choose for reopened file"
        );
        app.binary_choose(Choice::Cancel);
        until(&mut app, &rx, |a| !a.view.binary_offer_pending(reopened.ticket.document()));
        stop(app, rx, root);
    }
    #[test]
    fn actual_gui_binary_defers_to_existing_file_prompt_without_cancelling_either_owner() {
        let (mut app, rx, root, path, _) = host();
        let offer = open(&mut app, &rx, path);
        assert!(app.view.file_command("file.open").unwrap());
        let generation = app.view.file_prompt().unwrap().generation;
        app.sync_file_ui();
        assert!(!app.binary_owns_input());
        assert!(app.close_dialog.is_none(), "existing file prompt defers binary confirmation");
        assert_eq!(app.view.file_prompt().unwrap().generation, generation);
        assert!(!app.binary_key(&HostKey { logical_key: Key::Named(NamedKey::Escape), text: None, repeat: false }));
        assert_eq!(app.view.binary_offer(), Some(offer));
        press(&mut app, Key::Named(NamedKey::Escape));
        assert!(app.view.file_prompt().is_none());
        assert_eq!(app.view.binary_offer(), Some(offer), "Escape only completed the existing prompt");
        assert!(matches!(app.close_dialog, Some(close_dialog::Ticket::Binary(_))));
        app.binary_choose(Choice::Cancel);
        until(&mut app, &rx, |a| !a.view.binary_offer_pending(offer.ticket.document()));
        stop(app, rx, root);
    }
    #[test]
    fn actual_gui_external_open_waits_in_existing_inbox_for_file_prompt_then_binary_offer() {
        use kwil_core_workspace::instance::{OpenFile, OpenFiles};
        let (mut app, rx, root, path, prior) = host();
        let (open_external, inbox) = kwil_view_model::document_worker::external_open_channel(|| Ok(()));
        app.external_inbox = inbox;
        app.view.file_command("file.open").unwrap();
        app.sync_file_ui();
        let prompt = app.view.native_file_prompt().unwrap();
        open_external(OpenFiles { files: vec![OpenFile { path: path.clone(), line: None, column: None }] }).unwrap();
        assert_eq!(app.external_open_event(), Damage::default());
        assert!(app.external_inbox.has_pending());
        assert!(app.view.native_file_prompt_is_current(&prompt));
        assert_eq!(app.view.active_id(), prior);
        app.resume_external_open();
        assert_eq!(queued_external(&mut app, &rx), 0, "no repeated wake while modal stays owned");
        press(&mut app, Key::Named(NamedKey::Escape));
        assert!(app.view.file_prompt().is_none());
        app.resume_external_open();
        assert_eq!(queued_external(&mut app, &rx), 1, "release reuses the normal event queue");
        app.external_open_event();
        assert!(!app.external_inbox.has_pending());
        until(&mut app, &rx, |a| a.view.binary_offer().is_some());
        let offer = app.view.binary_offer().unwrap();
        let text_path = root.join("after.txt");
        std::fs::write(&text_path, b"external text").unwrap();
        open_external(OpenFiles { files: vec![OpenFile { path: text_path, line: None, column: None }] }).unwrap();
        assert_eq!(app.external_open_event(), Damage::default());
        assert_eq!(app.view.binary_offer(), Some(offer));
        assert!(app.external_inbox.has_pending());
        app.binary_choose(Choice::Cancel);
        until(&mut app, &rx, |a| !a.view.binary_offer_pending(offer.ticket.document()));
        app.resume_external_open();
        assert_eq!(queued_external(&mut app, &rx), 1);
        app.external_open_event();
        until(&mut app, &rx, |a| !a.view.document_busy(a.view.active_id()) && a.view.editor().is_some());
        assert_eq!(bytes(&app), b"external text");
        assert!(!app.external_inbox.has_pending());
        stop(app, rx, root);
    }
    #[test]
    fn actual_gui_vetted_preview_paints_while_real_open_completion_is_held() {
        use kwil_view_model::document_worker::DocumentResult;
        // None exercises normal classified text, whose first screen must not
        // wait for indexing either. Text/Hex exercise the accepted binary path.
        for choice in [None, Some(Choice::Text), Some(Choice::Hex)] {
            let (mut app, rx, root, path, _) = host();
            let id = if let Some(choice) = choice {
                let offer = open(&mut app, &rx, path);
                assert!(app.binary_blocks_paint(), "an unresolved binary offer stays blank");
                assert!(!app.view.binary_preview_ready(offer.ticket.document()));
                app.binary_choose(choice);
                offer.ticket.document()
            } else {
                let path = root.join("first-screen.txt");
                std::fs::write(&path, b"first screen\nsecond line\n").unwrap();
                let id = app.view.open_path(path).unwrap();
                app.sync_file_ui();
                id
            };
            assert!(app.binary_pending());
            assert!(app.binary_blocks_paint(), "no matching checked preview has been delivered yet");
            assert!(app.view.editor().is_none());
            // Hold exactly one genuine final result, retaining its document
            // ownership. No synthetic success, polling, delay or large file is
            // needed to make the preview-before-completion state deterministic.
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut completion = None;
            let mut preview = false;
            for _ in 0..512 {
                if preview && completion.is_some() {
                    break;
                }
                let UserEvent::Document(result) = rx
                    .recv_timeout(
                        deadline.checked_duration_since(Instant::now()).expect("held GUI completion deadline"),
                    )
                    .expect("held GUI completion wake")
                else {
                    panic!("unexpected held GUI completion event")
                };
                if matches!(&*result, DocumentResult::CheckedOpened { ticket, .. } if ticket.document() == id) {
                    assert!(completion.is_none(), "one finite held final result cell");
                    assert!(matches!(&*result, DocumentResult::CheckedOpened { result: Ok(Some(_)), .. }));
                    completion = Some(result);
                    continue;
                }
                let current_preview = matches!(&*result,
                    DocumentResult::CheckedPreview { ticket, .. } | DocumentResult::CheckedRawPreview { ticket, .. }
                    if ticket.document() == id);
                let damage = app.view.document_result(*result);
                app.sync_file_ui();
                app.apply(damage);
                preview |= current_preview;
            }
            assert!(preview, "the actual worker published a first-screen preview");
            let completion = completion.expect("actual final completion retained without applying it");
            assert!(app.view.binary_preview_ready(id));
            assert!(
                !app.binary_blocks_paint(),
                "actual GUI body paint permits the vetted first screen before completion"
            );
            assert!(app.binary_pending(), "preview never releases ownership/input fences");
            assert!(app.binary_owns_input());
            assert!(app.view.document_busy(id));
            assert!(app.view.editor().is_none(), "held completion still owns the full document");
            assert!(app.close_dialog.is_none(), "accepted/normal preview is not another decision prompt");
            if choice == Some(Choice::Hex) {
                assert!(app.view.hex_active());
                let row = app.view.hex_session().unwrap().row(app.view.hex_source(), 0).unwrap();
                assert_eq!(row.bytes.as_slice(), BYTES);
            } else {
                assert!(!app.view.hex_active());
                assert_eq!(
                    app.view.line(0).unwrap(),
                    if choice.is_some() { &BYTES[..BYTES.len() - 1] } else { b"first screen" }
                );
            }
            let source_epoch = app.view.input_epoch();
            let top = app.view.top();
            let raw_cursor = app.view.hex_session().map(|session| session.view().cursor());
            press(&mut app, Key::Character("a".into()));
            press(&mut app, Key::Named(NamedKey::ArrowDown));
            press(&mut app, Key::Named(NamedKey::PageDown));
            app.edit(input::Action::Text("blocked paste".into()));
            assert!(app.binary_window_input(&WindowEvent::Ime(Ime::Commit("blocked IME".into()))));
            assert_eq!(app.view.active_id(), id);
            assert_eq!(app.view.input_epoch(), source_epoch);
            assert_eq!(app.view.top(), top);
            assert_eq!(app.view.hex_session().map(|session| session.view().cursor()), raw_cursor);
            assert!(app.view.editor().is_none());
            assert!(!app.binary_blocks_paint(), "input refusal must preserve the vetted preview");
            let damage = app.view.document_result(*completion);
            app.sync_file_ui();
            app.apply(damage);
            until(&mut app, &rx, |a| !a.view.document_busy(id) && a.view.editor().is_some());
            assert!(!app.binary_pending());
            assert_eq!(bytes(&app), if choice.is_some() { BYTES } else { b"first screen\nsecond line\n" });
            stop(app, rx, root);
        }
    }
}
