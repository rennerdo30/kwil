//! Terminal hex body adapter; keep existing tabs/status and modal priority.
//! Workspace retains every per-tab Session and raw editor loan. Host dispatch
//! submits outcomes/pages through that owner and renders its cached session
//! before any text, search or formatting action can reach the hidden View.
//! Draw and input never touch a Snapshot or filesystem. Ratatui writes changed
//! cells normally: no Clear/full-terminal-reset, polling or background timer.
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use kwil_core_commands::{Preset, keymap::KeymapEngine, settings::TabLayout, vim::VimResult};
use kwil_view_model::Workspace;
use kwil_view_model::{
    hex::{ASCII_COLUMN, hex_column},
    hex_session::{Input, Outcome, Session, Source},
};
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    widgets::Paragraph,
};

fn dispatch(session: &mut Session, input: Input<'_>) -> Result<Outcome, String> {
    let source = session.source().ok_or("Hex view is closed")?;
    session.input(source, session.input_epoch(), input).map_err(|e| e.to_string())
}
/// Only commands handled as frontend chrome may leave raw body dispatch.
/// Everything else, including palette-selected text/search/format commands,
/// is resolved here before a Workspace can coerce into its hidden text View.
pub(super) fn chrome_command(id: &str) -> bool {
    matches!(
        id,
        "file.new"
            | "file.open"
            | "file.openHex"
            | "file.save"
            | "file.saveAs"
            | "file.saveAll"
            | "file.close"
            | "file.closeAll"
            | "file.reopenClosed"
            | "file.recent"
            | "file.nextTab"
            | "file.previousTab"
            | "file.restoreSession"
            | "file.quit"
            | "file.saveAndQuit"
            | "view.toggleHex"
            | "view.settings"
            | "view.settingsFile"
            | "view.commandPalette"
            | "view.fileFinder"
            | "view.zoomIn"
            | "view.zoomOut"
            | "view.zoomReset"
    )
}
fn submit(workspace: &mut Workspace, outcome: Result<Outcome, String>) -> bool {
    match outcome {
        Ok(outcome) => {
            if let Err(error) = workspace.submit_hex_outcome(outcome) {
                workspace.set_notice(error.to_string());
            }
        }
        Err(error) => {
            workspace.set_notice(error);
        }
    }
    true
}
pub(super) fn resolve(
    workspace: &mut Workspace,
    keys: &mut KeymapEngine,
    intent: super::KeyResult,
) -> super::KeyResult {
    match intent {
        super::KeyResult::Hex(call, extend) => {
            let outcome = if let Some(session) = workspace.hex_session_mut() {
                match mapped_input(call.id, session, extend) {
                    Some(input) if extend => Some(dispatch(session, input)),
                    Some(_) => command(session, call.id),
                    None => None,
                }
            } else {
                None
            };
            match outcome {
                Some(outcome) => {
                    submit(workspace, outcome);
                }
                None => {
                    workspace.set_notice("This text action is unavailable while physical bytes own the body".into());
                }
            }
            super::KeyResult::Changed(true)
        }
        super::KeyResult::Search(_) | super::KeyResult::Language if workspace.hex_active() => {
            keys.cancel_pending();
            workspace.cancel_hex_input();
            workspace.set_notice("Switch back to text before searching, replacing or choosing a language".into());
            super::KeyResult::Changed(true)
        }
        intent => {
            if workspace.hex_active() && !matches!(intent, super::KeyResult::Changed(_)) {
                workspace.cancel_hex_input();
                keys.cancel_pending();
            }
            intent
        }
    }
}
fn mapped_input(id: &str, session: &Session, extend: bool) -> Option<Input<'static>> {
    kwil_view_model::hex_session::command_input(id, session.view().rows(), extend)
}
pub(super) fn host_key(
    workspace: &mut Workspace,
    event: KeyEvent,
    keys: &mut KeymapEngine,
    elapsed: std::time::Duration,
    preset: Preset,
) -> super::KeyResult {
    if event.kind == KeyEventKind::Release {
        return super::KeyResult::Changed(false);
    }
    keys.set_context(kwil_core_commands::Context::Hex);
    if let Some(chord) = super::chord(event) {
        match keys.feed(chord) {
            VimResult::Pending => return super::KeyResult::Changed(false),
            VimResult::Unbound if keys.suppresses_fallback() => return super::KeyResult::Changed(false),
            VimResult::Error(error) => {
                workspace.set_notice(error);
                return super::KeyResult::Changed(true);
            }
            VimResult::Command(call) => {
                return super::invoke(workspace, call, event.modifiers.contains(KeyModifiers::SHIFT), elapsed, preset);
            }
            VimResult::Unbound => {}
        }
    }
    if let Some(session) = workspace.hex_session_mut() {
        if let Some(outcome) = key(session, event) {
            return super::KeyResult::Changed(submit(workspace, outcome));
        }
    } else if !chrome_key(event) {
        workspace.set_notice("Hex worker is busy; body input was not applied".into());
        return super::KeyResult::Changed(true);
    }
    // These are the established terminal chrome fallbacks. All other keys
    // remain consumed, including ordinary text during a page/edit/reopen loan.
    let ctrl = event.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER);
    let shift = event.modifiers.contains(KeyModifiers::SHIFT);
    match event.code {
        KeyCode::Char('q' | 'Q') if ctrl => super::KeyResult::Quit,
        KeyCode::Char('s' | 'S') if ctrl => super::KeyResult::Save(None),
        KeyCode::Char('o' | 'O') if ctrl => super::KeyResult::File("file.open"),
        KeyCode::Char('n' | 'N') if ctrl => super::KeyResult::File("file.new"),
        KeyCode::Char('w' | 'W') if ctrl => super::KeyResult::File("file.close"),
        KeyCode::Char('p' | 'P') if ctrl => super::KeyResult::Palette(if shift { ">" } else { "" }),
        KeyCode::Tab | KeyCode::BackTab if ctrl => super::KeyResult::File(if shift || event.code == KeyCode::BackTab {
            "file.previousTab"
        } else {
            "file.nextTab"
        }),
        _ => super::KeyResult::Changed(true),
    }
}
fn chrome_key(event: KeyEvent) -> bool {
    event.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER)
        && matches!(
            event.code,
            KeyCode::Char('q' | 'Q' | 's' | 'S' | 'o' | 'O' | 'n' | 'N' | 'w' | 'W' | 'p' | 'P')
                | KeyCode::Tab
                | KeyCode::BackTab
        )
}
pub(super) fn host_paste(workspace: &mut Workspace, text: &str) -> bool {
    let Some(session) = workspace.hex_session_mut() else {
        workspace.set_notice("Hex worker is busy; paste was not applied".into());
        return true;
    };
    let Some(source) = session.source() else { return true };
    let epoch = session.input_epoch();
    let outcome = paste(session, source, epoch, text);
    submit(workspace, outcome)
}
pub(super) fn host_mouse(
    workspace: &mut Workspace,
    event: MouseEvent,
    width: u16,
    height: u16,
    layout: TabLayout,
) -> bool {
    let body = body_rect(workspace, Rect::new(0, 0, width, height), layout);
    let outcome = workspace.hex_session_mut().and_then(|session| mouse(session, body, event));
    if let Some(outcome) = outcome {
        submit(workspace, outcome);
    }
    true
}
fn body_rect(workspace: &Workspace, bounds: Rect, layout: TabLayout) -> Rect {
    let body = super::tabs::geometry(workspace, bounds.width, bounds.height, layout).body;
    Rect {
        x: bounds.x + body.x as u16,
        y: bounds.y + body.y as u16,
        width: body.width as u16,
        height: body.height as u16,
    }
}
pub(super) fn workspace_status(workspace: &Workspace) -> String {
    let status = workspace.hex_session().map_or_else(|| "Opening physical bytes...".into(), Session::status);
    let busy = if workspace.document_busy(workspace.active_id()) { "  Worker pending" } else { "" };
    format!("{status}{busy}  {}", workspace.notice())
}
pub(super) fn draw_workspace(frame: &mut ratatui::Frame<'_>, workspace: &Workspace, layout: TabLayout) {
    let bounds = frame.area();
    let body = body_rect(workspace, bounds, layout);
    let normal = Style::default();
    // Paint the Session even while the actual editor is on the worker. Its
    // immutable page is bounded and its source falls back to the cached loan.
    if let Some(session) = workspace.hex_session() {
        if !draw(frame, body, session, workspace.hex_source(), normal, normal.bg(Color::Rgb(0x30, 0x58, 0x80))) {
            frame.render_widget(Paragraph::new("Opening physical bytes...").style(normal), body);
        }
    } else {
        frame.render_widget(Paragraph::new("Opening physical bytes...").style(normal), body);
    }
    if bounds.height != 0 {
        let area = Rect { x: bounds.x, y: bounds.bottom() - 1, width: bounds.width, height: 1 };
        if !workspace.document_busy(workspace.active_id())
            && workspace.notice().is_empty()
            && let Some(session) = workspace.hex_session()
        {
            status(frame, area, session);
        } else {
            frame.render_widget(
                Paragraph::new(workspace_status(workspace)).style(Style::default().fg(Color::Cyan)),
                area,
            );
        }
    }
}
/// None is reserved for explicit ROOT chrome. All unsupported body keys are
/// consumed, so hidden editor bindings cannot insert text into a binary.
pub(super) fn key(session: &mut Session, event: KeyEvent) -> Option<Result<Outcome, String>> {
    if !session.is_open() {
        return None;
    }
    if event.kind == KeyEventKind::Release {
        return Some(Ok(Outcome::default()));
    }
    let command = event.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER);
    let extend = event.modifiers.contains(KeyModifiers::SHIFT);
    let input = match event.code {
        KeyCode::Char(c) if command => match c.to_ascii_lowercase() {
            'z' if extend => Some(Input::Redo),
            'z' => Some(Input::Undo),
            'y' => Some(Input::Redo),
            'a' => Some(Input::SelectAll),
            's' | 'o' | 'n' | 'w' | 'p' | 'q' => return None,
            _ => None,
        },
        KeyCode::Tab | KeyCode::BackTab if command => return None,
        KeyCode::Left => Some(Input::Move { bytes: -1, extend }),
        KeyCode::Right => Some(Input::Move { bytes: 1, extend }),
        KeyCode::Up => Some(Input::Move { bytes: -16, extend }),
        KeyCode::Down => Some(Input::Move { bytes: 16, extend }),
        KeyCode::PageUp => Some(Input::Move { bytes: -(session.view().rows().max(1) as i64 * 16), extend }),
        KeyCode::PageDown => Some(Input::Move { bytes: session.view().rows().max(1) as i64 * 16, extend }),
        KeyCode::Home => Some(Input::Home { document: command, extend }),
        KeyCode::End => Some(Input::End { document: command, extend }),
        KeyCode::Insert => Some(Input::ToggleInsert),
        KeyCode::Delete => Some(Input::Delete { backwards: false }),
        KeyCode::Backspace => Some(Input::Delete { backwards: true }),
        KeyCode::Esc => Some(Input::Cancel),
        KeyCode::Char(c) if !command && !event.modifiers.contains(KeyModifiers::ALT) && c.is_ascii_hexdigit() => {
            Some(Input::Digit(c))
        }
        _ => None,
    };
    Some(input.map_or_else(|| Ok(Outcome::default()), |input| dispatch(session, input)))
}
pub(super) fn paste(session: &mut Session, source: Source, epoch: u64, text: &str) -> Result<Outcome, String> {
    session.input(source, epoch, Input::Paste(text)).map_err(|e| e.to_string())
}
pub(super) fn command(session: &mut Session, id: &str) -> Option<Result<Outcome, String>> {
    if !session.is_open() {
        return None;
    }
    let input = mapped_input(id, session, false);
    if let Some(input) = input {
        return Some(dispatch(session, input));
    }
    if id.starts_with("file.")
        || id.starts_with("tab.")
        || id.starts_with("palette.")
        || id.starts_with("settings.")
        || id == "view.toggleHex"
    {
        return None;
    }
    Some(Ok(Outcome::default()))
}
pub(super) fn mouse(session: &mut Session, body: Rect, event: MouseEvent) -> Option<Result<Outcome, String>> {
    if !session.is_open() || !body.contains((event.column, event.row).into()) {
        return None;
    }
    let input = match event.kind {
        MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Drag(MouseButton::Left) => Some(Input::Click {
            row: (event.row - body.y) as usize,
            column: (event.column - body.x) as usize,
            extend: matches!(event.kind, MouseEventKind::Drag(_)) || event.modifiers.contains(KeyModifiers::SHIFT),
        }),
        MouseEventKind::ScrollUp => Some(Input::Scroll(-3)),
        MouseEventKind::ScrollDown => Some(Input::Scroll(3)),
        _ => None,
    };
    Some(input.map_or_else(|| Ok(Outcome::default()), |input| dispatch(session, input)))
}
/// Supplied theme styles keep body rendering consistent with existing chrome.
pub(super) fn draw(
    frame: &mut ratatui::Frame<'_>,
    body: Rect,
    session: &Session,
    current: Option<Source>,
    normal: Style,
    selected: Style,
) -> bool {
    if !session.is_open() {
        return false;
    }
    frame.render_widget(Paragraph::new("").style(normal), body);
    let selection = session.view().selection();
    for index in 0..body.height as usize {
        let Some(row) = session.row(current, index) else {
            continue;
        };
        let text = row.text();
        let y = body.y + index as u16;
        frame.render_widget(
            Paragraph::new(text.as_str()).style(normal),
            Rect { x: body.x, y, width: body.width, height: 1 },
        );
        for byte in 0..row.bytes.len() {
            if !selection.contains(&(row.offset + byte as u64)) {
                continue;
            }
            for (column, width) in [(hex_column(byte), 2), (ASCII_COLUMN + byte, 1)] {
                if column + width > body.width as usize {
                    continue;
                }
                frame.render_widget(
                    Paragraph::new(&text[column..column + width]).style(selected),
                    Rect { x: body.x + column as u16, y, width: width as u16, height: 1 },
                );
            }
        }
    }
    if current == session.source() {
        let view = session.view();
        if let Some(row) = (view.cursor() / 16).checked_sub(view.top()).filter(|&row| row < body.height as u64) {
            let column = hex_column((view.cursor() % 16) as usize);
            if column + 2 <= body.width as usize {
                if let Some(high) = view.pending_high_nibble() {
                    frame.render_widget(
                        Paragraph::new(format!("{high:X}_")).style(normal),
                        Rect { x: body.x + column as u16, y: body.y + row as u16, width: 2, height: 1 },
                    );
                }
                frame.set_cursor_position((
                    body.x + column as u16 + u16::from(view.pending_high_nibble().is_some()),
                    body.y + row as u16,
                ));
            }
        }
    }
    true
}
pub(super) fn status(frame: &mut ratatui::Frame<'_>, area: Rect, session: &Session) {
    frame.render_widget(Paragraph::new(session.status()).style(Style::default().fg(Color::Cyan)), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{KeyResult, Msg};
    use kwil_view_model::hex_session::{Mutation, Representation, read_page};
    use std::{
        collections::VecDeque,
        path::PathBuf,
        sync::mpsc,
        time::{Duration, Instant},
    };

    struct Host {
        workspace: Workspace,
        search: crate::search_ui::SearchHost,
        keys: KeymapEngine,
        receiver: mpsc::Receiver<Msg>,
        document: crate::DocumentLane,
        root: PathBuf,
        trail: VecDeque<String>,
    }
    impl Host {
        fn new() -> Self {
            let root = crate::test_support::directory("kwil-tui-hex-host").unwrap();
            let (tx, receiver) = mpsc::channel();
            let budget = crate::AllocationBudget::default();
            let mut document = crate::DocumentLane::new(&budget).unwrap();
            let mut workspace = Workspace::new(kwil_view_model::View::new(None)).unwrap();
            workspace.set_rows(3);
            document
                .started(workspace.start_bounded_document_worker_at(Some(root.join("recovery")), &budget, move || {
                    tx.send(Msg::DocumentReady).map_err(|_| std::io::Error::other("hex fixture wake closed"))
                }))
                .unwrap();
            let bindings = [
                kwil_core_commands::Binding {
                    keys: "ctrl+b".into(),
                    command: "file.openHex".into(),
                    context: None,
                    mode: None,
                },
                kwil_core_commands::Binding {
                    keys: "ctrl+t".into(),
                    command: "view.toggleHex".into(),
                    context: None,
                    mode: None,
                },
            ];
            let mut host = Self {
                workspace,
                search: Default::default(),
                keys: KeymapEngine::new(Preset::VsCode, &bindings).unwrap(),
                receiver,
                document,
                root,
                trail: VecDeque::new(),
            };
            host.until("initial recovery ready", |h| h.workspace.recovery_ready());
            host
        }
        fn dispatch(&mut self, message: Msg) {
            self.record(format!("worker {}", message_summary(&message)));
            match message {
                Msg::DocumentReady => {
                    let info = self.document.transport.as_ref().unwrap().result_info();
                    self.record(format!("document slot {info:?}"));
                    self.document.ready(&mut self.workspace, &mut self.search, &mut self.keys).unwrap();
                }
                Msg::SearchReady => {
                    self.search.ready(&mut self.workspace, &mut self.keys);
                }
                _ => panic!("unexpected hex host message"),
            }
            self.record(format!("after worker {}", self.state()));
        }
        fn record(&mut self, entry: String) {
            if self.trail.len() == 24 {
                self.trail.pop_front();
            }
            self.trail.push_back(entry.chars().take(1024).collect());
        }
        fn state(&self) -> String {
            let view = &self.workspace;
            let editor = view.editor();
            let session = view.hex_session();
            format!(
                "tab={} raw={} busy={} loans={} saves={} editor={} rev={:?} receipt={} save_receipt={} source={:?} hex_pending={} hex_epoch={:?} top={:?} cursor={:?} recovery={:?} notice={}",
                view.active_id().0,
                view.hex_active(),
                view.document_busy(view.active_id()),
                view.search_replacement_pending(),
                view.file_saves_pending(),
                editor.is_some(),
                editor.map(|editor| editor.document().revision()),
                editor.is_some_and(|editor| editor.has_pending_edit_receipt()),
                editor.is_some_and(|editor| editor.has_pending_save_completion()),
                view.hex_source(),
                session.is_some_and(|session| session.busy()),
                session.map(Session::input_epoch),
                session.map(|session| session.view().top()),
                session.map(|session| session.view().cursor()),
                view.recovery_notice(),
                view.notice().chars().take(256).collect::<String>()
            )
        }
        fn until(&mut self, stage: &str, ready: impl Fn(&Self) -> bool) {
            self.record(format!("stage {stage}: {}", self.state()));
            let deadline = Instant::now() + Duration::from_secs(10);
            while !ready(self) {
                let message = self.receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .unwrap_or_else(|error| panic!("bounded real hex document worker wake at stage '{stage}': {error:?}\nstate: {}\ntrail:\n{}",
                        self.state(), self.trail.iter().cloned().collect::<Vec<_>>().join("\n")));
                self.dispatch(message);
            }
            self.record(format!("ready {stage}: {}", self.state()));
        }
        fn idle(&mut self, stage: &str) {
            self.until(stage, |h| {
                let status = h.document.transport.as_ref().unwrap().status();
                !status.closed
                    && !status.worker_done
                    && status.failure.is_none()
                    && !status.occupied
                    && !status.retry_pending
                    && !status.retry_acknowledgement_required
                    && !h.workspace.document_busy(h.workspace.active_id())
                    && h.workspace.editor().is_some_and(|editor| !editor.has_pending_edit_receipt())
                    && h.workspace.recovery_notice().is_none()
            });
        }
        fn intent(&mut self, intent: KeyResult) {
            let intent = resolve(&mut self.workspace, &mut self.keys, intent);
            match intent {
                KeyResult::File(command) => {
                    self.workspace.file_command(command).unwrap();
                }
                KeyResult::Save(path) => {
                    self.workspace.save_document(self.workspace.active_id(), path).unwrap();
                }
                KeyResult::Changed(_) => {}
                _ => panic!("unexpected hex host intent"),
            }
        }
        fn press(&mut self, code: KeyCode, modifiers: KeyModifiers) {
            self.record(format!("input {code:?} {modifiers:?}: {}", self.state()));
            let event = KeyEvent::new(code, modifiers);
            let intent = if self.workspace.file_prompt().is_some() {
                crate::file_prompt::key(&mut self.workspace, event)
            } else if self.workspace.hex_active() {
                host_key(&mut self.workspace, event, &mut self.keys, Duration::ZERO, Preset::VsCode)
            } else {
                self.keys.set_context(kwil_core_commands::Context::Editor);
                crate::key(&mut self.workspace, event, Duration::ZERO, &mut self.keys, Preset::VsCode)
            };
            self.intent(intent);
            self.record(format!("after input {}", self.state()));
        }
        fn named(&mut self, id: &'static str) {
            let VimResult::Command(call) = self.keys.invoke_named(id) else { panic!("registered command") };
            let intent = crate::invoke(&mut self.workspace, call, false, Duration::ZERO, Preset::VsCode);
            self.intent(intent);
        }
        fn open_raw(&mut self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.root.join(name);
            std::fs::write(&path, bytes).unwrap();
            self.press(KeyCode::Char('b'), KeyModifiers::CONTROL);
            assert!(
                self.workspace.file_prompt().is_some(),
                "file.openHex routes through the established native-path prompt"
            );
            self.workspace.file_prompt_mut().unwrap().text = path.to_str().unwrap().into();
            self.press(KeyCode::Enter, KeyModifiers::NONE);
            self.until(&format!("open raw {name}: bounded page and editor return"), |h| {
                h.workspace.hex_session().is_some_and(|session| session.row(h.workspace.hex_source(), 0).is_some())
                    && !h.workspace.document_busy(h.workspace.active_id())
            });
            self.idle(&format!("open raw {name}: recovery receipt settled"));
            path
        }
        fn bytes(&self) -> Vec<u8> {
            let snapshot = self.workspace.editor().unwrap().document().snapshot();
            snapshot.chunks(0..snapshot.len()).unwrap().flat_map(|chunk| chunk.unwrap().to_vec()).collect()
        }
        fn paint(&self) -> String {
            let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 5)).unwrap();
            terminal
                .draw(|frame| {
                    crate::tabs::draw(frame, &self.workspace, TabLayout::Horizontal);
                    draw_workspace(frame, &self.workspace, TabLayout::Horizontal);
                })
                .unwrap();
            terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect()
        }
        fn finish(self) {
            let root = self.root.clone();
            let deadline = Instant::now() + Duration::from_secs(35);
            std::thread::spawn(move || {
                crate::test_support::expect_cleanup(crate::shutdown_bounded_workspace(
                    self.workspace,
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
            assert!(root.file_name().unwrap().to_string_lossy().starts_with("kwil-tui-hex-host-"));
            std::fs::remove_dir_all(root).unwrap();
        }
    }
    fn message_summary(message: &Msg) -> String {
        // Diagnostic history stores discriminants/scalar ResultInfo only.
        format!("{:?}", std::mem::discriminant(message))
    }
    #[test]
    fn actual_tui_hex_dispatch_edit_loan_keeps_cache_blocks_text_and_shares_undo_raw_save() {
        let mut host = Host::new();
        let physical = b"\xff\xfeA\0B\0\r\0\n\0";
        let path = host.open_raw("physical.bin", physical);
        assert_eq!(host.bytes(), physical);
        assert!(host.paint().contains("0000000000000000  FF FE 41 00"));
        host.press(KeyCode::Char('8'), KeyModifiers::NONE);
        host.press(KeyCode::Char('0'), KeyModifiers::NONE);
        assert!(
            host.workspace.document_busy(host.workspace.active_id()),
            "hex edit was not loaned; state: {}; transport={:?}; info={:?}; trail={:?}",
            host.state(),
            host.document.transport.as_ref().map(|transport| transport.status()),
            host.document.transport.as_ref().and_then(|transport| transport.result_info()),
            host.trail
        );
        assert!(host.workspace.editor().is_none(), "actual editor is owned by raw edit worker");
        assert!(
            host.paint().contains("0000000000000000  FF FE 41 00"),
            "last bounded cache stays visible during edit loan"
        );
        host.press(KeyCode::Char('a'), KeyModifiers::NONE);
        host.named("search.replaceAll");
        host.named("edit.newline");
        host.idle("byte overwrite and refused hidden text commands settled");
        let mut changed = physical.to_vec();
        changed[0] = 0x80;
        assert_eq!(host.bytes(), changed, "raw ownership consumes unsupported text and replacement commands");
        host.press(KeyCode::Char('z'), KeyModifiers::CONTROL);
        host.idle("raw undo settled");
        assert_eq!(host.bytes(), physical);
        host.press(KeyCode::Char('y'), KeyModifiers::CONTROL);
        host.idle("raw redo settled");
        assert_eq!(host.bytes(), changed);
        host.press(KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert!(host.workspace.hex_active());
        assert!(host.workspace.notice().contains("Save"), "dirty typed refusal is shown without dropping bytes");
        host.press(KeyCode::Char('s'), KeyModifiers::CONTROL);
        host.until("raw save request finished", |h| !h.workspace.file_saves_pending());
        host.idle("raw save recovery receipt settled");
        assert_eq!(std::fs::read(path).unwrap(), changed, "save routes through workspace raw transport");
        host.finish();
    }

    #[test]
    fn actual_tui_hex_scroll_page_mouse_paste_focus_and_clean_toggle_use_worker() {
        let mut host = Host::new();
        let physical: Vec<u8> = (0..128).map(|value| value as u8).collect();
        host.open_raw("scroll.bin", &physical);
        let before = host.paint();
        let body = body_rect(&host.workspace, Rect::new(0, 0, 100, 5), TabLayout::Horizontal);
        host_mouse(
            &mut host.workspace,
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: body.x + 18,
                row: body.y,
                modifiers: KeyModifiers::NONE,
            },
            100,
            5,
            TabLayout::Horizontal,
        );
        assert!(host.workspace.document_busy(host.workspace.active_id()));
        assert!(host.workspace.hex_session().is_some());
        host.idle("scroll page editor loan returned");
        assert_ne!(host.paint(), before);
        assert!(host.paint().contains("0000000000000030  30 31 32 33"));
        host_mouse(
            &mut host.workspace,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: body.x + ASCII_COLUMN as u16 + 1,
                row: body.y,
                modifiers: KeyModifiers::NONE,
            },
            100,
            5,
            TabLayout::Horizontal,
        );
        assert_eq!(host.workspace.hex_session().unwrap().view().cursor(), 49);
        host.press(KeyCode::Char('f'), KeyModifiers::NONE);
        host.workspace.cancel_hex_input(); // same FocusLost/IME-cancel route as run
        assert!(host.workspace.hex_session().unwrap().view().pending_high_nibble().is_none());
        let source = host.workspace.hex_source();
        host_paste(&mut host.workspace, "F");
        assert_eq!(host.workspace.hex_source(), source, "odd paste never commits a prefix");
        assert_eq!(host.bytes(), physical);
        host_paste(&mut host.workspace, "FF 00");
        host.idle("valid hex paste settled");
        assert_eq!(&host.bytes()[49..51], &[255, 0]);
        host.press(KeyCode::Char('z'), KeyModifiers::CONTROL);
        host.idle("paste undo settled");
        assert_eq!(host.bytes(), physical);
        host.press(KeyCode::Char('t'), KeyModifiers::CONTROL);
        host.until("clean hex-to-text reopen returned editor", |h| !h.workspace.document_busy(h.workspace.active_id()));
        assert!(!host.workspace.hex_active(), "clean off transition is the actual ReopenText worker protocol");
        host.finish();
    }

    #[test]
    fn actual_tui_hex_tombstones_tab_isolation_and_shutdown_drain() {
        let mut host = Host::new();
        host.open_raw("first.bin", b"abcdefgh01234567");
        let first = host.workspace.active_id();
        host.keys = KeymapEngine::new(
            Preset::VsCode,
            &[
                kwil_core_commands::Binding {
                    keys: "ctrl+b".into(),
                    command: "file.openHex".into(),
                    context: None,
                    mode: None,
                },
                kwil_core_commands::Binding {
                    keys: "ctrl+t".into(),
                    command: "view.toggleHex".into(),
                    context: None,
                    mode: None,
                },
                kwil_core_commands::Binding {
                    keys: "a".into(),
                    command: String::new(),
                    context: Some(kwil_core_commands::Context::Hex),
                    mode: None,
                },
                kwil_core_commands::Binding {
                    keys: "ctrl+k ctrl+r".into(),
                    command: "edit.redo".into(),
                    context: Some(kwil_core_commands::Context::Hex),
                    mode: None,
                },
            ],
        )
        .unwrap();
        host.press(KeyCode::Char('a'), KeyModifiers::NONE);
        assert!(host.workspace.hex_session().unwrap().view().pending_high_nibble().is_none());
        host.open_raw("second.bin", b"SECOND physical!");
        let second = host.workspace.active_id();
        assert_ne!(first, second);
        assert!(host.paint().contains("53 45 43 4F 4E 44"));
        host.workspace.activate(first);
        host.workspace.set_hex_rows(3);
        host.idle("reactivated first tab page and recovery settled");
        assert!(host.paint().contains("61 62 63 64"), "per-tab cache never paints the other tab's bytes");
        host_paste(&mut host.workspace, "EE");
        host.idle("first-tab EE overwrite settled");
        host.press(KeyCode::Char('z'), KeyModifiers::CONTROL);
        host.idle("first-tab overwrite undo settled");
        assert_eq!(host.bytes()[0], b'a');
        host.press(KeyCode::Char('k'), KeyModifiers::CONTROL);
        host.press(KeyCode::Char('r'), KeyModifiers::CONTROL);
        host.idle("first-tab custom chord redo settled");
        assert_eq!(host.bytes()[0], 0xee, "hex context keeps custom chord prefixes through real worker redo");
        host_paste(&mut host.workspace, "FF");
        assert!(host.workspace.document_busy(first));
        host.finish(); // production shutdown drains the actual admitted raw editor loan
    }
    fn session() -> Session {
        let mut session = Session::new(2);
        session.bind(Source { document: 1, revision: 0, epoch: 1, len: 0, representation: Representation::RawBytes });
        session
    }
    #[test]
    fn terminal_keys_edit_bytes_and_ignore_release_and_text_shortcuts() {
        let mut session = session();
        key(&mut session, KeyEvent::new(KeyCode::Char('f'), KeyModifiers::empty())).unwrap().unwrap();
        let edit =
            key(&mut session, KeyEvent::new(KeyCode::Char('f'), KeyModifiers::empty())).unwrap().unwrap().edit.unwrap();
        assert_eq!(edit.mutation, Mutation::Replace { range: 0..0, bytes: vec![255] });
        assert!(command(&mut session, "edit.cut").unwrap().unwrap().edit.is_none());
        let release = KeyEvent::new_with_kind(KeyCode::Delete, KeyModifiers::empty(), KeyEventKind::Release);
        assert!(key(&mut session, release).unwrap().unwrap().edit.is_none());
    }
    #[test]
    fn terminal_body_clears_stale_bytes_and_draws_exact_offsets_and_ascii() {
        let mut session = session();
        let source = Source { len: 3, ..session.source().unwrap() };
        session.bind(source);
        let mut editor = kwil_view_model::Editor::new(kwil_view_model::Document::new());
        editor.replace_byte_range(0..0, &[0xff, 0, b'A']).unwrap();
        let page = read_page(&editor.document().snapshot(), session.page_request().unwrap(), || false).unwrap();
        assert!(session.accept_page(page));
        let backend = ratatui::backend::TestBackend::new(90, 2);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw(frame, frame.area(), &session, Some(source), Style::default(), Style::default());
            })
            .unwrap();
        let text = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect::<String>();
        assert!(text.contains("0000000000000000  FF 00 41"));
        assert!(text.contains("| ..A"));
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    frame.area(),
                    &session,
                    Some(Source { epoch: 2, ..source }),
                    Style::default(),
                    Style::default(),
                );
            })
            .unwrap();
        assert!(terminal.backend().buffer().content.iter().all(|cell| cell.symbol() == " "));
    }
}
