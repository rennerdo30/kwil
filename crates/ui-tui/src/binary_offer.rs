//! Scalar terminal prompt over the Workspace-owned binary-open protocol.
//! A callback carries the displayed ticket, never the newly active document.
use super::*;
use kwil_view_model::document_worker::binary_offer::{Choice, Offer};

#[derive(Default)]
pub(super) struct Host {
    offer: Option<Offer>,
    selected: usize,
    identity: Option<(kwil_view_model::SessionDocumentId, bool, bool)>,
}

impl Host {
    pub(super) fn blocked(view: &Workspace) -> bool {
        view.binary_offer_pending(view.active_id())
    }

    pub(super) fn captures(view: &Workspace, event: &Event, settings_open: bool) -> bool {
        if !Self::blocked(view) || settings_open {
            return false;
        }
        match event {
            // Existing file/settings/close prompts finish their captured work
            // first. Body mouse events still cannot move or edit underneath.
            Event::Key(_) | Event::Paste(_) if view.file_prompt().is_some() => false,
            Event::Key(key)
                if view.close_pending()
                    && (key.code == KeyCode::Esc
                        || (key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER)
                            && matches!(key.code, KeyCode::Char('s' | 'S' | 'q' | 'Q')))) =>
            {
                false
            }
            Event::Key(_) | Event::Paste(_) | Event::Mouse(_) => true,
            _ => false,
        }
    }

    pub(super) fn sync(&mut self, view: &mut Workspace, keys: &mut KeymapEngine) -> bool {
        let blocked = Self::blocked(view);
        let deferred = view.file_prompt().is_some() || view.close_pending();
        // Worker detection may publish its own status while an older file
        // prompt still owns input. Keep that prompt visible until it finishes.
        if blocked && view.file_prompt().is_some() {
            view.prompt_notice();
        }
        let identity = Some((view.active_id(), blocked, deferred));
        let offer = view.binary_offer();
        let changed = self.identity != identity || self.offer != offer;
        if changed {
            keys.cancel_pending();
            if self.offer != offer {
                self.selected = 0;
            }
            self.identity = identity;
            self.offer = offer;
        }
        changed
    }

    fn choose(&mut self, view: &mut Workspace, rows: usize) -> bool {
        let Some(offer) = self.offer else { return false };
        let choice = [Choice::Hex, Choice::Text, Choice::Cancel][self.selected];
        match view.resolve_binary_offer(offer.ticket, choice, rows) {
            Ok(_) => self.offer = None,
            Err(error) => {
                view.set_notice(format!("Binary choice refused: {error}; try again or Esc to cancel"));
            }
        }
        true
    }

    // Called ahead of every key, paste and mouse route in the real input loop.
    // Crossterm delivers committed IME text as keys/paste, so it shares this fence.
    pub(super) fn input(&mut self, view: &mut Workspace, keys: &mut KeymapEngine, event: &Event, bounds: Rect) -> bool {
        self.sync(view, keys);
        if self.offer.is_none() || view.file_prompt().is_some() || view.close_pending() {
            return false;
        }
        let rows = view.rows();
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                let plain = !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER);
                match key.code {
                    KeyCode::Esc => {
                        self.selected = 2;
                        return self.choose(view, rows);
                    }
                    KeyCode::Enter => return self.choose(view, rows),
                    KeyCode::BackTab | KeyCode::Left | KeyCode::Up => self.selected = (self.selected + 2) % 3,
                    KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
                        self.selected = (self.selected + 2) % 3
                    }
                    KeyCode::Tab | KeyCode::Right | KeyCode::Down => self.selected = (self.selected + 1) % 3,
                    KeyCode::Char('h' | 'H') if plain => {
                        self.selected = 0;
                        return self.choose(view, rows);
                    }
                    KeyCode::Char('t' | 'T') if plain => {
                        self.selected = 1;
                        return self.choose(view, rows);
                    }
                    KeyCode::Char('c' | 'C') if plain => {
                        self.selected = 2;
                        return self.choose(view, rows);
                    }
                    _ => return false,
                }
                true
            }
            Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(event::MouseButton::Left) => {
                if let Some(index) = file_prompt::choice_hit(bounds, mouse.column, mouse.row) {
                    self.selected = index;
                    return self.choose(view, rows);
                }
                false
            }
            _ => false,
        }
    }

    pub(super) fn draw(&self, frame: &mut ratatui::Frame, view: &Workspace) {
        if !Self::blocked(view)
            || view.file_prompt().is_some()
            || view.close_pending()
            || view.binary_preview_ready(view.active_id())
        {
            return;
        }
        // Only scalar metadata is inspected, including before worker detection.
        let offer = view.binary_offer();
        let detail = offer
            .map_or_else(|| "Preparing file...".into(), |offer| format!("Binary file ({} bytes): open as", offer.len));
        let selected = offer.filter(|offer| Some(*offer) == self.offer).map(|_| self.selected);
        file_prompt::draw_choices(frame, &detail, selected, view.notice());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kwil_core_commands::settings::TabLayout;
    use std::{
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
        time::{Duration, Instant},
    };

    const BINARY: &[u8] = b"header\0binary\x01\n";
    const BOUNDS: Rect = Rect::new(0, 0, 80, 12);
    static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

    fn fixture_root_in(parent: &Path, next: &AtomicU64) -> std::io::Result<PathBuf> {
        // Clock resolution is not identity authority for parallel fixtures.
        // Exclusive creation also protects a directory left by a reused PID.
        for _ in 0..128 {
            let sequence = next.fetch_add(1, Ordering::Relaxed);
            let root = parent.join(format!("kwil-tui-binary-offer-{}-{sequence}", std::process::id()));
            match std::fs::create_dir(&root) {
                Ok(()) => return Ok(root),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "binary TUI fixture namespace exhausted"))
    }

    #[test]
    fn binary_fixture_identity_is_distinct_and_skips_existing_owner() {
        let root = fixture_root_in(&std::env::temp_dir(), &NEXT_ROOT).unwrap();
        let next = AtomicU64::new(0);
        let first = fixture_root_in(&root, &next).unwrap();
        let marker = first.join("owner.bin");
        std::fs::write(&marker, BINARY).unwrap();
        let second = fixture_root_in(&root, &next).unwrap();
        assert_ne!(first, second, "each fixture receives a distinct sequence");
        // Replaying the namespace forces collisions without clocks or threads.
        let replay = AtomicU64::new(0);
        let third = fixture_root_in(&root, &replay).unwrap();
        assert_ne!(first, third);
        assert_ne!(second, third);
        assert_eq!(replay.load(Ordering::Relaxed), 3, "both existing owners were skipped");
        assert_eq!(std::fs::read(marker).unwrap(), BINARY, "an existing owner's bytes stay untouched");
        std::fs::remove_dir_all(root).unwrap();
    }

    struct Fixture {
        view: Workspace,
        search: search_ui::SearchHost,
        keys: KeymapEngine,
        modal: Host,
        receiver: mpsc::Receiver<Msg>,
        document: DocumentLane,
        root: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let root = fixture_root_in(&std::env::temp_dir(), &NEXT_ROOT).unwrap();
            let mut view = Workspace::new(View::new(None)).unwrap();
            view.set_rows(3);
            let (tx, receiver) = mpsc::channel();
            let budget = AllocationBudget::default();
            let mut document = DocumentLane::new(&budget).unwrap();
            document
                .started(view.start_bounded_document_worker_at(Some(root.join("recovery")), &budget, move || {
                    tx.send(Msg::DocumentReady).map_err(|_| std::io::Error::other("binary fixture wake closed"))
                }))
                .unwrap();
            let mut fixture = Self {
                view,
                search: Default::default(),
                keys: KeymapEngine::new(Preset::VsCode, &[]).unwrap(),
                modal: Default::default(),
                receiver,
                document,
                root,
            };
            fixture.until("recovery ready", |f| f.view.recovery_ready());
            fixture
        }
        fn until(&mut self, stage: &str, done: impl Fn(&Self) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(10);
            self.until_before(stage, deadline, done);
        }
        fn until_before(&mut self, stage: &str, deadline: Instant, done: impl Fn(&Self) -> bool) {
            while !done(self) {
                let message = self
                    .receiver
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .unwrap_or_else(|error| {
                        panic!(
                            "binary TUI stage {stage}: {error}; status={:?}; info={:?}; notice={}",
                            self.document.transport.as_ref().map(WorkspaceDocumentTransport::status),
                            self.document.transport.as_ref().and_then(WorkspaceDocumentTransport::result_info),
                            self.view.notice()
                        )
                    });
                assert!(matches!(message, Msg::DocumentReady), "document worker wake");
                self.document.ready(&mut self.view, &mut self.search, &mut self.keys).unwrap();
                self.modal.sync(&mut self.view, &mut self.keys);
            }
        }
        fn input(&mut self, event: Event) {
            if Host::captures(&self.view, &event, false) {
                self.modal.input(&mut self.view, &mut self.keys, &event, BOUNDS);
            } else if let Event::Paste(text) = &event {
                self.view.file_prompt_mut().expect("existing file prompt owns paste").text.push_str(text);
            } else if let Event::Key(key) = event {
                let intent = if self.view.file_prompt().is_some() {
                    file_prompt::key(&mut self.view, key)
                } else if self.view.close_pending() && key.code == KeyCode::Esc {
                    KeyResult::Cancel
                } else {
                    crate::key(&mut self.view, key, Duration::ZERO, &mut self.keys, Preset::VsCode)
                };
                match intent {
                    KeyResult::File(command) => {
                        self.view.file_command(command).unwrap();
                    }
                    KeyResult::Cancel => {
                        self.view.cancel_file_prompt();
                    }
                    KeyResult::Changed(_) => {}
                    _ => panic!("unexpected terminal intent"),
                }
            }
            self.modal.sync(&mut self.view, &mut self.keys);
        }
        fn press(&mut self, code: KeyCode, modifiers: KeyModifiers) {
            self.input(Event::Key(KeyEvent::new(code, modifiers)));
        }
        fn open(&mut self, name: &str, bytes: &[u8]) -> kwil_view_model::SessionDocumentId {
            let path = self.root.join(name);
            std::fs::write(&path, bytes).unwrap();
            self.press(KeyCode::Char('o'), KeyModifiers::CONTROL);
            assert!(self.view.file_prompt().is_some(), "existing terminal Open prompt");
            self.view.file_prompt_mut().unwrap().text = path.to_str().unwrap().into();
            self.press(KeyCode::Enter, KeyModifiers::NONE);
            self.view.active_id()
        }
        fn offered(&mut self) -> Offer {
            let deadline = Instant::now() + Duration::from_secs(10);
            self.until_before("binary detected without a decoded editor", deadline, |f| {
                f.view.binary_offer().is_some()
            });
            assert!(self.view.editor().is_none());
            let offer = self.view.binary_offer().unwrap();
            // Model acceptance still leaves its completed carrier charged on
            // the original producer. Consume actual capacity wakes/receives
            // before ONE choice, without retrying the user's accepting input.
            self.until_before("same binary offer and producer capacity ready", deadline, |f| {
                assert_eq!(f.view.binary_offer(), Some(offer), "capacity cannot retarget the captured offer");
                let status = f.document.transport.as_ref().unwrap().status();
                assert!(
                    !status.closed && !status.worker_done && status.failure.is_none(),
                    "binary choice capacity failed: {status:?}; notice={}",
                    f.view.notice()
                );
                !status.occupied
                    && !status.retry_pending
                    && !status.retry_acknowledgement_required
                    && f.document.retry.is_none()
                    && f.document.replacement.is_none()
                    && f.view.recovery_ready()
                    && f.view.recovery_notice().is_none()
                    && !f.view.file_saves_pending()
                    && !f.view.document_busy(offer.ticket.document())
            });
            offer
        }
        fn settled(&mut self, id: kwil_view_model::SessionDocumentId) {
            self.until("choice and document receipts settled", |f| {
                !f.view.binary_offer_pending(id)
                    && !f.view.document_busy(f.view.active_id())
                    && f.view.editor().is_some_and(|editor| !editor.has_pending_edit_receipt())
                    && f.view.recovery_notice().is_none()
            });
        }
        fn hold_checked_opened(
            &mut self,
            id: kwil_view_model::SessionDocumentId,
        ) -> kwil_view_model::workspace_document::ResultInfo {
            use kwil_view_model::workspace_document::ResultKind;
            // The actual completion stays charged in its original worker slot.
            // Only scalar inspection crosses the frontend boundary.
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let message = self
                    .receiver
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .expect("bounded checked preview/completion delivery");
                assert!(matches!(message, Msg::DocumentReady), "document worker wake");
                let transport = self.document.transport.as_ref().unwrap();
                if let Some(info) = transport.result_info()
                    && info.kind == ResultKind::Opened
                    && info.document == Some(id)
                {
                    let before = transport.status();
                    assert!(matches!(
                        self.document.transport.as_mut().unwrap().receive(&mut self.view, false),
                        Err(DocumentResultSettlementReason::ConsumerRefused)
                    ));
                    assert_eq!(self.document.transport.as_ref().unwrap().status(), before);
                    return info;
                }
                self.document.ready(&mut self.view, &mut self.search, &mut self.keys).unwrap();
                self.modal.sync(&mut self.view, &mut self.keys);
            }
        }
        fn bytes(&self) -> Vec<u8> {
            let mut bytes = Vec::new();
            self.view.editor().unwrap().document().snapshot().write_to(&mut bytes).unwrap();
            bytes
        }
        fn paint(&mut self) -> String {
            // Match the real loop's pre-draw layout, including a newly opened
            // tab whose View starts with zero rows. The first R1 None failure
            // exercised that missing fixture geometry, not a runtime paint gate.
            let geometry = tabs::geometry(&self.view, BOUNDS.width, BOUNDS.height, TabLayout::Horizontal);
            if self.view.rows() != geometry.body.height {
                self.view.set_rows(geometry.body.height);
            }
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(BOUNDS.width, BOUNDS.height)).unwrap();
            terminal
                .draw(|frame| {
                    tabs::draw(frame, &self.view, TabLayout::Horizontal);
                    if self.view.hex_active() {
                        hex_ui::draw_workspace(frame, &self.view, TabLayout::Horizontal);
                    }
                    self.modal.draw(frame, &self.view);
                })
                .unwrap();
            terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect()
        }
        fn finish(self) {
            let root = self.root.clone();
            let deadline = Instant::now() + Duration::from_secs(35);
            std::thread::spawn(move || {
                crate::test_support::expect_cleanup(crate::shutdown_bounded_workspace(
                    self.view,
                    self.search,
                    self.receiver,
                    self.document,
                    None,
                    None,
                    deadline,
                ))
            })
            .join()
            .unwrap();
            assert_eq!(root.parent(), Some(std::env::temp_dir().as_path()));
            assert!(root.file_name().unwrap().to_string_lossy().starts_with("kwil-tui-binary-offer-"));
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn binary_offer_terminal_vetted_preview_paints_before_held_completion_without_releasing_input() {
        use kwil_view_model::workspace_document::ResultKind;
        const TEXT: &[u8] = b"vetted first screen\n";
        for choice in [None, Some(Choice::Text), Some(Choice::Hex), Some(Choice::Cancel)] {
            let mut f = Fixture::new();
            let expected = if choice.is_some() { BINARY } else { TEXT };
            let id = f.open("preview-before-completion", expected);
            assert!(Host::blocked(&f.view));
            assert!(!f.view.binary_preview_ready(id));
            if let Some(choice) = choice {
                f.offered();
                assert!(!f.view.binary_preview_ready(id));
                assert!(!f.paint().contains("header"), "unresolved binary source remains blank");
                let key = match choice {
                    Choice::Text => 't',
                    Choice::Hex => 'h',
                    Choice::Cancel => 'c',
                };
                f.press(KeyCode::Char(key), KeyModifiers::NONE);
            }
            let completion = f.hold_checked_opened(id);
            assert!(Host::blocked(&f.view), "vetted preview never releases pending ownership");
            assert!(f.view.editor().is_none(), "real source completion stays in the original transport slot");
            let painted = f.paint();
            if choice == Some(Choice::Cancel) {
                assert_eq!(completion.kind, ResultKind::Opened);
                assert!(!f.view.binary_preview_ready(id));
                assert!(painted.contains("Preparing file..."));
                assert!(!painted.contains("header"));
                assert!(!painted.contains("68 65 61 64 65 72"));
            } else {
                assert_eq!(completion.kind, ResultKind::Opened);
                assert!(f.view.binary_preview_ready(id));
                let visible = match choice {
                    None => "vetted first screen",
                    Some(Choice::Hex) => "68 65 61 64 65 72",
                    _ => "header",
                };
                assert!(
                    painted.contains(visible),
                    "vetted first screen must paint before full index completion: {choice:?}"
                );
                assert!(!painted.contains("Please wait; body input is paused"));
                assert!(!painted.contains("Detecting or opening the selected representation..."));
            }
            f.press(KeyCode::Right, KeyModifiers::NONE);
            f.press(KeyCode::Char('界'), KeyModifiers::NONE);
            f.input(Event::Paste("FF must not edit".into()));
            f.input(Event::Mouse(event::MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 10,
                row: 2,
                modifiers: KeyModifiers::NONE,
            }));
            assert_eq!(f.paint(), painted, "pending preview consumes keys/paste/IME and body navigation");
            assert!(Host::blocked(&f.view));
            assert!(f.view.editor().is_none());
            assert_eq!(f.document.transport.as_ref().unwrap().result_info(), Some(completion));
            f.document.transport.as_ref().unwrap().wake_ready();
            f.modal.sync(&mut f.view, &mut f.keys);
            f.settled(id);
            if choice == Some(Choice::Cancel) {
                assert!(f.view.view(id).is_none());
                assert!(f.bytes().is_empty());
            } else {
                assert_eq!(f.bytes(), expected);
                assert_eq!(f.view.hex_active(), choice == Some(Choice::Hex));
            }
            f.finish();
        }
    }

    #[test]
    fn binary_offer_terminal_enter_defaults_hex_and_fences_detecting_offered_resolving_input() {
        let mut f = Fixture::new();
        let id = f.open("binary", BINARY);
        assert!(Host::blocked(&f.view));
        f.press(KeyCode::Char('界'), KeyModifiers::NONE);
        f.input(Event::Paste("must not edit".into()));
        f.offered();
        let rendered = f.paint();
        for label in [
            "Binary file (15 bytes)",
            "> [H] Hex (default)",
            "[T] Text",
            "[C] Cancel",
            "Tab / arrows",
            "Enter open",
            "Esc cancel",
        ] {
            assert!(rendered.contains(label), "visible binary choice: {label}");
        }
        f.press(KeyCode::Char('界'), KeyModifiers::NONE);
        f.press(KeyCode::Char('s'), KeyModifiers::CONTROL);
        f.input(Event::Paste("must not become hex bytes".into()));
        assert!(f.view.editor().is_none());
        f.press(KeyCode::Enter, KeyModifiers::NONE);
        assert!(f.view.binary_offer().is_none());
        assert!(Host::blocked(&f.view), "accepted choice still blocks before completion");
        f.press(KeyCode::Char('A'), KeyModifiers::NONE);
        f.input(Event::Paste("FF".into()));
        f.settled(id);
        assert!(f.view.hex_active());
        assert_eq!(f.bytes(), BINARY);
        assert!(!Host::blocked(&f.view));
        assert!(!f.paint().contains("Hex (default)"));
        f.finish();
    }

    #[test]
    fn binary_offer_terminal_shortcut_release_and_modifiers_cannot_accept_a_choice() {
        let mut f = Fixture::new();
        let id = f.open("shortcut-choice", BINARY);
        let offer = f.offered();
        let mut release = KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        f.input(Event::Key(release));
        f.press(KeyCode::Char('h'), KeyModifiers::CONTROL);
        f.press(KeyCode::Char('t'), KeyModifiers::ALT);
        assert_eq!(f.view.binary_offer(), Some(offer));
        f.press(KeyCode::Char('H'), KeyModifiers::SHIFT);
        f.settled(id);
        assert!(f.view.hex_active());
        assert_eq!(f.bytes(), BINARY);
        f.finish();
    }

    #[test]
    fn binary_offer_terminal_tab_arrows_text_and_escape_cancel_use_worker_completion() {
        let mut f = Fixture::new();
        let prior = f.view.active_id();
        let id = f.open("text-choice", BINARY);
        f.offered();
        f.press(KeyCode::Tab, KeyModifiers::NONE);
        assert!(f.paint().contains("> [T] Text"));
        f.press(KeyCode::BackTab, KeyModifiers::NONE);
        assert!(f.paint().contains("> [H] Hex (default)"));
        f.press(KeyCode::Tab, KeyModifiers::SHIFT);
        assert!(f.paint().contains("> [C] Cancel"));
        f.press(KeyCode::Right, KeyModifiers::NONE);
        f.press(KeyCode::Tab, KeyModifiers::NONE);
        f.press(KeyCode::Left, KeyModifiers::NONE);
        f.press(KeyCode::Up, KeyModifiers::NONE);
        assert!(f.paint().contains("> [C] Cancel"));
        f.press(KeyCode::Down, KeyModifiers::NONE);
        f.press(KeyCode::Right, KeyModifiers::NONE);
        f.press(KeyCode::Enter, KeyModifiers::NONE);
        f.settled(id);
        assert!(!f.view.hex_active());
        assert_eq!(f.bytes(), BINARY);
        let count = f.view.session().tabs().len();
        let cancelled = f.open("cancel-choice", BINARY);
        f.offered();
        f.press(KeyCode::Esc, KeyModifiers::NONE);
        f.settled(cancelled);
        assert!(f.view.view(cancelled).is_none(), "Cancel removes the never-accepted placeholder");
        assert_eq!(f.view.session().tabs().len(), count);
        assert!(f.view.view(prior).is_some());
        assert_eq!(f.bytes(), BINARY);
        f.finish();
    }

    #[test]
    fn binary_offer_terminal_mouse_choices_and_outside_body_never_edit_or_cancel() {
        for selected in 0..3 {
            let mut f = Fixture::new();
            let id = f.open("mouse-choice", BINARY);
            let offer = f.offered();
            for kind in [MouseEventKind::ScrollDown, MouseEventKind::Down(event::MouseButton::Left)] {
                f.input(Event::Mouse(event::MouseEvent {
                    kind,
                    column: 0,
                    row: BOUNDS.bottom() - 1,
                    modifiers: KeyModifiers::NONE,
                }));
                assert_eq!(f.view.binary_offer(), Some(offer));
            }
            let area = file_prompt::choice_area(BOUNDS);
            f.input(Event::Mouse(event::MouseEvent {
                kind: MouseEventKind::Down(event::MouseButton::Left),
                column: area.x + 2,
                row: area.y + 2 + selected,
                modifiers: KeyModifiers::NONE,
            }));
            assert!(f.view.binary_offer().is_none());
            f.settled(id);
            if selected == 2 {
                assert!(f.view.view(id).is_none());
                assert!(f.bytes().is_empty());
            } else {
                assert_eq!(f.bytes(), BINARY);
                assert_eq!(f.view.hex_active(), selected == 0);
            }
            f.finish();
        }
    }

    #[test]
    fn binary_offer_terminal_refusal_and_late_callback_keep_exact_inactive_offer_retryable() {
        let mut f = Fixture::new();
        let original = f.view.active_id();
        let id = f.open("stale-choice", BINARY);
        let old = f.offered();
        f.view.file_command("file.open").unwrap();
        assert!(f.view.file_prompt().is_some());
        assert!(f.modal.choose(&mut f.view, 3));
        assert_eq!(f.modal.offer, Some(old));
        assert_eq!(f.view.binary_offer(), Some(old), "refused admission retains same retry ticket");
        f.view.cancel_file_prompt();
        assert!(f.view.activate(original));
        assert!(f.modal.choose(&mut f.view, 3), "late displayed callback uses captured ticket");
        assert_eq!(f.view.binary_offer_for(id), Some(old));
        assert!(f.bytes().is_empty(), "late choice cannot edit the new active tab");
        f.modal.sync(&mut f.view, &mut f.keys);
        assert!(!Host::blocked(&f.view));
        assert!(f.view.activate(id));
        f.modal.sync(&mut f.view, &mut f.keys);
        f.view.close(id, true).unwrap();
        f.view.file_command("file.reopenClosed").unwrap();
        let new = f.offered();
        assert_eq!(new.ticket.document(), old.ticket.document());
        assert!(new.ticket.generation() > old.ticket.generation());
        let mut stale_callback = Host { offer: Some(old), ..Default::default() };
        assert!(stale_callback.choose(&mut f.view, 3));
        assert_eq!(f.view.binary_offer(), Some(new), "old generation cannot resolve new offer");
        f.press(KeyCode::Char('t'), KeyModifiers::NONE);
        f.settled(id);
        assert_eq!(f.bytes(), BINARY);
        assert!(!f.view.hex_active());
        f.finish();
    }

    #[test]
    fn binary_offer_terminal_utf16_and_gzip_do_not_offer_hex() {
        const GZIP: &[u8] = include_bytes!("../../../examples/smoke/example.log.gz");
        const PLAIN: &[u8] = include_bytes!("../../../examples/smoke/example.log");
        for (name, source, expected) in [
            ("utf16-bom", b"\xff\xfea\0b\0c\0d\0\n\0".as_slice(), b"abcd\n".as_slice()),
            ("utf16-no-bom", b"a\0b\0c\0d\0\n\0".as_slice(), b"abcd\n".as_slice()),
            ("gzip", GZIP, PLAIN),
        ] {
            let mut f = Fixture::new();
            let id = f.open(name, source);
            f.until("nonbinary text open", |f| {
                assert!(f.view.binary_offer().is_none(), "UTF16 and gzip must retain existing decoded route");
                !f.view.binary_offer_pending(id) && f.view.editor().is_some()
            });
            f.settled(id);
            assert_eq!(f.bytes(), expected);
            assert!(!f.view.hex_active());
            assert!(!f.paint().contains("Hex (default)"));
            f.finish();
        }
    }

    #[test]
    fn binary_offer_terminal_detection_defers_to_current_file_settings_and_close_prompts() {
        let mut f = Fixture::new();
        f.view.set_session_restore(false);
        let original = f.view.active_id();
        f.view
            .execute("edit.insert", &CommandArgs { text: Some("keep dirty".into()), ..Default::default() }, 1)
            .unwrap();
        f.until("original edit receipt", |f| f.view.editor().is_some_and(|editor| !editor.has_pending_edit_receipt()));
        let id = f.open("prompt-priority", BINARY);
        // Detection is still on the worker. A newer captured prompt must keep
        // its own input even when that completion offers this binary tab.
        f.view.file_command("file.open").unwrap();
        let generation = f.view.file_prompt().unwrap().generation;
        let offered = f.offered();
        assert!(!f.paint().contains("Hex (default)"));
        assert!(f.view.notice().starts_with("Open:"));
        f.press(KeyCode::Char('界'), KeyModifiers::NONE);
        f.input(Event::Paste("prompt".into()));
        assert_eq!(f.view.file_prompt().unwrap().generation, generation);
        assert_eq!(f.view.file_prompt().unwrap().text, "界prompt");
        assert_eq!(f.view.binary_offer(), Some(offered));
        f.press(KeyCode::Esc, KeyModifiers::NONE);
        assert!(f.view.file_prompt().is_none());
        assert!(f.paint().contains("Hex (default)"));
        for event in [
            Event::Key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE)),
            Event::Paste("settings".into()),
            Event::Mouse(event::MouseEvent {
                kind: MouseEventKind::Down(event::MouseButton::Left),
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            }),
        ] {
            assert!(!Host::captures(&f.view, &event, true), "existing settings panel owns its input");
        }
        // A close waiting on detecting/resolving work retains Escape. It must
        // never be interpreted as cancelling the unrelated binary offer.
        f.view.request_window_close(false).unwrap();
        assert!(f.view.close_pending());
        assert!(f.view.view(original).unwrap().editor().unwrap().document().is_dirty());
        assert!(f.view.activate(id));
        f.modal.sync(&mut f.view, &mut f.keys);
        assert!(!f.paint().contains("Hex (default)"));
        assert!(!Host::captures(&f.view, &Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)), false));
        f.press(KeyCode::Char('t'), KeyModifiers::NONE);
        f.input(Event::Paste("blocked by close".into()));
        assert_eq!(f.view.binary_offer(), Some(offered));
        f.press(KeyCode::Esc, KeyModifiers::NONE);
        assert!(!f.view.close_pending());
        assert_eq!(f.view.binary_offer(), Some(offered));
        // Deliberately leave the real source pending for the existing bounded
        // shutdown drain; no additional host teardown protocol is introduced.
        assert_eq!(f.view.binary_offer_for(id), Some(offered));
        f.finish();
    }

    #[test]
    fn binary_offer_terminal_external_wake_keeps_bounded_inbox_until_prompt_and_offer_release() {
        use kwil_core_workspace::instance::{OpenFile, OpenFiles};
        let mut f = Fixture::new();
        let first = f.view.active_id();
        let path = f.root.join("external-binary");
        std::fs::write(&path, BINARY).unwrap();
        let (sender, receiver) = mpsc::channel();
        let wake_sender = sender.clone();
        let (open, inbox) = kwil_view_model::document_worker::external_open_channel(move || {
            wake_sender.send(Msg::OpenExternal).map_err(|_| std::io::Error::other("test TUI closed"))
        });
        let request = OpenFiles { files: vec![OpenFile { path: path.clone(), line: None, column: None }] };
        let mut gate = crate::ExternalGate::default();
        f.view.file_command("file.open").unwrap();
        f.view.file_prompt_mut().unwrap().text = "captured prompt".into();
        let generation = f.view.file_prompt().unwrap().generation;
        for _ in 0..16 {
            open(request.clone()).unwrap();
        }
        assert!(matches!(open(request.clone()), Err(mpsc::TrySendError::Full(retained)) if retained == request));
        assert!(matches!(receiver.recv_timeout(Duration::from_secs(1)).unwrap(), Msg::OpenExternal));
        assert!(!gate.wake(&mut f.view, &inbox, false));
        assert!(gate.pending);
        assert_eq!(f.view.active_id(), first);
        assert_eq!(f.view.file_prompt().unwrap().generation, generation);
        assert_eq!(f.view.file_prompt().unwrap().text, "captured prompt");
        gate.replay(&f.view, false, &sender);
        assert!(receiver.try_recv().is_err(), "no replay loop while modal owns input");
        assert!(
            matches!(open(request.clone()), Err(mpsc::TrySendError::Full(_))),
            "deferred wake must not drain the bounded inbox"
        );
        f.press(KeyCode::Esc, KeyModifiers::NONE);
        gate.replay(&f.view, true, &sender);
        assert!(receiver.try_recv().is_err(), "existing settings panel still defers opens");
        gate.replay(&f.view, false, &sender);
        assert!(matches!(receiver.recv_timeout(Duration::from_secs(1)).unwrap(), Msg::OpenExternal));
        assert!(gate.wake(&mut f.view, &inbox, false));
        assert!(!gate.pending);
        assert!(!gate.wake(&mut f.view, &inbox, false), "duplicate wake cannot dispatch a request twice");
        let id = f.view.active_id();
        assert_ne!(id, first);
        let offered = f.offered();
        assert_eq!(offered.ticket.document(), id);
        assert_eq!(f.view.session().tabs().len(), 2, "duplicate requests reuse the same exact path");
        open(request).unwrap();
        assert!(matches!(receiver.recv_timeout(Duration::from_secs(1)).unwrap(), Msg::OpenExternal));
        assert!(!gate.wake(&mut f.view, &inbox, false), "active offer retains input priority");
        gate.replay(&f.view, false, &sender);
        assert!(receiver.try_recv().is_err());
        f.press(KeyCode::Char('c'), KeyModifiers::NONE);
        f.settled(id);
        gate.replay(&f.view, false, &sender);
        assert!(matches!(receiver.recv_timeout(Duration::from_secs(1)).unwrap(), Msg::OpenExternal));
        assert!(gate.wake(&mut f.view, &inbox, false));
        f.offered();
        f.finish();

        let mut closing = Fixture::new();
        closing.view.set_session_restore(false);
        closing.view.request_window_close(false).unwrap();
        closing.until("accepted exit", |f| f.view.wants_exit());
        let exit_request = OpenFiles {
            files: vec![OpenFile { path: closing.root.join("never-open-after-exit"), line: None, column: None }],
        };
        open(exit_request).unwrap();
        assert!(matches!(receiver.recv_timeout(Duration::from_secs(1)).unwrap(), Msg::OpenExternal));
        assert!(!gate.wake(&mut closing.view, &inbox, false));
        gate.replay(&closing.view, false, &sender);
        assert!(gate.pending, "accepted exit retains deferred admission without replay");
        assert!(receiver.try_recv().is_err());
        assert!(inbox.has_pending(), "accepted exit must not drain retained requests");
        closing.finish();
    }

    #[test]
    fn binary_offer_terminal_shutdown_drains_an_unanswered_offer_through_existing_owner() {
        let mut f = Fixture::new();
        f.open("shutdown-offer", BINARY);
        f.offered();
        f.finish();
    }
}
