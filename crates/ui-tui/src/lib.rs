//! Terminal frontend.
//!
//! Same model as the GUI (`kwil_view_model::View`). Event-driven: one
//! thread blocks on terminal input and forwards events into a channel that
//! also carries worker results, so the UI thread sleeps until something
//! happens. ratatui writes only changed cells, so the TUI stays fast over
//! SSH. Cell diffing does not establish physical terminal scroll-copy; that
//! separate backend gap remains open.

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use kwil_core_commands::{Preset, keymap::KeymapEngine, settings::Settings, vim::VimResult};
use kwil_view_model::{CommandArgs, CommandStatus, Notify, View, Workspace};
mod binary_offer;
mod document_ui;
mod file_prompt;
mod hex_ui;
mod palette_ui;
mod search_ui;
mod settings_ui;
mod tabs;
#[cfg(test)]
mod test_support;
mod wrap_ui;
use kwil_view_model::{
    document_worker::{DocumentRequest, DocumentResultSettlementReason, DocumentWrapRetryToken},
    visual_rows::AllocationBudget,
    workspace_document::{WorkspaceDocumentStartFailure, WorkspaceDocumentTransport},
};
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::sync::mpsc;
use std::{ops::ControlFlow, time::Duration};

enum Msg {
    WrapReady,
    SearchReady,
    HighlightsReady,
    PaletteRoot(Result<std::path::PathBuf, String>),
    PaletteReady,
    SettingsPath(Result<std::path::PathBuf, String>),
    SettingsPanelReady,
    Input(Event),
    DocumentReady,
    OpenExternal,
    Notice(String),
    InputClosed,
    Settings(Box<Settings>, kwil_view_model::language_selection::AcceptedLanguageMappings),
}

use document_ui::DocumentLane;

/// Leave accepted IPC requests in their existing 16-slot inbox while a modal
/// owns input. One scalar schedules a replay through the ordinary event queue.
#[derive(Default)]
struct ExternalGate {
    pending: bool,
}
impl ExternalGate {
    fn blocked(view: &Workspace, settings_open: bool) -> bool {
        settings_open
            || view.file_prompt().is_some()
            || view.close_pending()
            || view.wants_exit()
            || binary_offer::Host::blocked(view)
    }
    fn wake(
        &mut self,
        view: &mut Workspace,
        inbox: &kwil_view_model::document_worker::ExternalOpenInbox,
        settings_open: bool,
    ) -> bool {
        self.pending = true;
        if Self::blocked(view, settings_open) {
            return false;
        }
        self.pending = false;
        let requests = inbox.drain();
        let changed = !requests.is_empty();
        for request in requests {
            if let Err(error) = view.open_files(request) {
                view.set_notice(format!("Open failed: {error}"));
            }
        }
        changed
    }
    fn replay(&mut self, view: &Workspace, settings_open: bool, sender: &mpsc::Sender<Msg>) {
        if self.pending && !Self::blocked(view, settings_open) && sender.send(Msg::OpenExternal).is_ok() {
            self.pending = false;
        }
    }
}

pub fn run(view: View, on_first_frame: Box<dyn FnOnce(Notify)>) -> std::io::Result<()> {
    run_with_external_open(view, on_first_frame, Box::new(|_| {}))
}

pub use kwil_view_model::document_worker::ExternalOpen;
pub fn run_with_external_open(
    view: View,
    on_first_frame: Box<dyn FnOnce(Notify)>,
    on_external_ready: Box<dyn FnOnce(ExternalOpen)>,
) -> std::io::Result<()> {
    let workspace = Workspace::new(view)?;
    match run_workspace_with_external_open(workspace, AllocationBudget::default(), on_first_frame, on_external_ready) {
        ControlFlow::Continue(result) => result,
        ControlFlow::Break(refusal) => {
            let owner = document_ui::StartupOwner::new(refusal.workspace, refusal.budget);
            let (ticket, budget) = owner.into_parts();
            Err(retain_shutdown_error(ticket, Some(budget), refusal.error))
        }
    }
}

pub use document_ui::TuiStartRefusal;

/// Continue a genuinely unstarted frontend with its original Workspace and
/// original allocation counter. Pre-worker refusal returns both exact inputs.
/// Once admitted, failures keep the existing retained shutdown ownership path.
#[must_use = "a refused startup returns the original Workspace and budget"]
pub fn run_workspace_with_external_open(
    workspace: Workspace,
    budget: AllocationBudget,
    on_first_frame: Box<dyn FnOnce(Notify)>,
    on_external_ready: Box<dyn FnOnce(ExternalOpen)>,
) -> ControlFlow<TuiStartRefusal, std::io::Result<()>> {
    run_workspace_with_terminal_init(workspace, budget, on_first_frame, on_external_ready, |touched| {
        *touched = true;
        ratatui::try_init()
    })
}

fn run_workspace_with_terminal_init(
    workspace: Workspace,
    budget: AllocationBudget,
    on_first_frame: Box<dyn FnOnce(Notify)>,
    on_external_ready: Box<dyn FnOnce(ExternalOpen)>,
    initialize: impl FnOnce(&mut bool) -> std::io::Result<ratatui::DefaultTerminal>,
) -> ControlFlow<TuiStartRefusal, std::io::Result<()>> {
    // Construct the sole original source owner BEFORE even ensure_idle or
    // header admission. Setup and the event loop only borrow its fields.
    let mut startup = document_ui::StartupOwner::new(workspace, budget);
    let mut terminal = None;
    let mut terminal_touched = false;
    let mut palette: Option<kwil_view_model::palette_session::PaletteSession> = None;
    let mut settings_panel: Option<kwil_view_model::settings_session::SettingsSession> = None;
    let mut settings_file_pending = false;
    let mut language_menu: Option<LanguageMenu> = None;
    let mut binary_offer = binary_offer::Host::default();
    let mut external_gate = ExternalGate::default();
    let attempted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> std::io::Result<()> {
        ensure_shutdown_idle()?;
        if !startup.ticket.view.as_ref().expect("original startup Workspace").unstarted_frontend_ready() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "TUI startup requires an unstarted Workspace with its previous frontend fully retired and rearmed",
            ));
        }
        startup.ticket.document = Some(DocumentLane::new(&startup.budget)?);
        startup.ticket.search = Some(search_ui::SearchHost::default());
        let (tx, rx) = mpsc::channel::<Msg>();
        startup.ticket.rx = Some(rx);
        let external_sender = tx.clone();
        let (external_open, external_inbox) = kwil_view_model::document_worker::external_open_channel(move || {
            external_sender
                .send(Msg::OpenExternal)
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "TUI closed"))
        });
        terminal = Some(initialize(&mut terminal_touched)?);
        crossterm::execute!(
            std::io::stdout(),
            crossterm::event::EnableMouseCapture,
            crossterm::event::EnableBracketedPaste
        )?;
        let terminal = terminal.as_mut().expect("initialized terminal");
        let budget = &startup.budget;
        let ShutdownTicket { view, document, search, wrap, highlighting, rx, .. } = &mut startup.ticket;
        let view = view.as_mut().expect("guarded original Workspace");
        let document = document.as_mut().expect("admitted startup headers");
        let search = search.as_mut().expect("original search facade");
        let rx = rx.as_ref().expect("original event receiver");
        let size = terminal.size()?;
        let geometry = tabs::geometry(view, size.width, size.height, Settings::default().tabs.layout);
        view.set_rows(geometry.body.height);
        terminal.draw(|f| {
            tabs::draw(f, view, Settings::default().tabs.layout);
            binary_offer.draw(f, view);
        })?;
        let document_sender = tx.clone();
        document.started(view.start_bounded_document_worker_at(None, budget, move || {
            document_sender
                .send(Msg::DocumentReady)
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "TUI document wake closed"))
        }))?;
        // Start the existing scalar input reader only after document admission.
        // A genuine pre-worker refusal therefore leaves no orphan reader.
        let input_sender = tx.clone();
        std::thread::Builder::new().name("kwil-tui-input".into()).spawn(move || {
            loop {
                match event::read() {
                    Ok(event) => {
                        if input_sender.send(Msg::Input(event)).is_err() {
                            break;
                        }
                    }
                    Err(_) => {
                        let _ = input_sender.send(Msg::InputClosed);
                        break;
                    }
                }
            }
        })?;
        let t = tx.clone();
        on_first_frame(Box::new(move |msg| {
            let _ = t.send(Msg::Notice(msg));
        }));
        on_external_ready(external_open);
        let wrap_sender = tx.clone();
        *wrap = Some(wrap_ui::Runtime::new(
            wrap_ui::Host::with_budget(budget.clone(), move || {
                let _ = wrap_sender.send(Msg::WrapReady);
            })
            .map_err(std::io::Error::other)?,
        ));
        wrap.as_mut().expect("new wrap owner").attach(view).map_err(std::io::Error::other)?;
        let highlight_sender = tx.clone();
        *highlighting = kwil_view_model::highlight_session::HighlightSession::new(move || {
            let _ = highlight_sender.send(Msg::HighlightsReady);
        })
        .ok();
        if let Some(host) = highlighting
            && !prepared_body(view)
        {
            host.update(view);
        }
        let settings_sender = tx.clone();
        std::thread::spawn(move || {
            let root = match std::env::var_os("KWIL_WORKSPACE_ROOT") {
                Some(path) => {
                    let path = std::path::PathBuf::from(path);
                    if path.is_absolute() {
                        Ok(path)
                    } else {
                        Err("Palette workspace root must be absolute".to_owned())
                    }
                }
                None => std::env::current_dir().map_err(|error| error.to_string()),
            };
            let _ = settings_sender.send(Msg::PaletteRoot(root));
            let path = std::env::current_exe()
                .map_err(|error| error.to_string())
                .and_then(|exe| Settings::path(&exe).map_err(|error| error.to_string()));
            let _ = settings_sender.send(Msg::SettingsPath(path));
        });
        let started = std::time::Instant::now();
        let mut horizontal = 0u64;
        let mut settings = Settings::default();
        let mut keys = KeymapEngine::new(settings.keymap, &settings.bindings).expect("bundled keymap");
        let search_sender = tx.clone();
        search.start(view, &mut keys, move || {
            let _ = search_sender.send(Msg::SearchReady);
        });
        let mut input_identity = (view.active_id(), view.input_epoch());
        let mut raw_body = view.hex_active();
        let mut exiting = false;
        binary_offer.sync(view, &mut keys);
        loop {
            let Ok(msg) = rx.recv() else {
                document.begin_shutdown(Duration::from_secs(35)).map_err(std::io::Error::other)?;
                break;
            };
            let user_input = matches!(&msg, Msg::Input(_));
            let size = terminal.size()?;
            let body = wrap_body_rect(view, size.width, size.height, settings.tabs.layout);
            let blocked = wrap_modal(view, search, palette.as_ref(), settings_panel.as_ref(), language_menu.is_some());
            let wrap_changed = wrap.as_mut().is_some_and(|host| host.sync(view, body, blocked));
            if view.hex_active() || view.wrap_input_blocked() {
                if let Some(host) = highlighting {
                    host.suspend_prepared();
                }
                if let Some(runtime) = wrap {
                    runtime.suspend_syntax();
                }
            }
            if let Some(host) = &wrap {
                search.body_gate(host.gate(), host.closed() || view.wrap_input_blocked());
            }
            let mut changed = wrap_changed
                | match msg {
                    Msg::DocumentReady => {
                        let update = document.receive(view, search, &mut keys, true).map_err(std::io::Error::other)?;
                        if let (Some(update), Some(highlighting)) = (&update, highlighting.as_mut()) {
                            // The update keeps the damage reservation alive for
                            // this borrow. Never clone rows into retained UI data.
                            highlighting.invalidate_painted(view, update.damage());
                        }
                        update.is_some() || document.wrap_failed()
                    }
                    Msg::WrapReady => wrap.as_mut().is_some_and(|host| host.ready(view)),
                    Msg::SearchReady => !exiting && search.ready(view, &mut keys),
                    Msg::HighlightsReady => {
                        !view.hex_active()
                            && highlighting
                                .as_mut()
                                .is_some_and(|host| highlighting_ready_wrapped(host, view, wrap.as_ref()))
                    }
                    Msg::PaletteRoot(root) => {
                        let search_changed = match &root {
                            Ok(root) => search.root_ready(view, &mut keys, root.clone()),
                            Err(_) => false,
                        };
                        match root.and_then(|root| {
                            let sender = tx.clone();
                            kwil_view_model::palette_session::PaletteSession::new(
                                root,
                                std::sync::Arc::new(move || {
                                    let _ = sender.send(Msg::PaletteReady);
                                }),
                            )
                            .map_err(|error| error.to_string())
                        }) {
                            Ok(host) => {
                                palette = Some(host);
                                search_changed
                            }
                            Err(error) => {
                                view.set_notice(format!("Palette: {error}"));
                                true
                            }
                        }
                    }
                    Msg::PaletteReady => {
                        let mut changed = false;
                        if let Some(host) = &mut palette {
                            changed |= host.worker_ready();
                            if let Some(report) = host.take_discovery_report()
                                && (report.errors > 0
                                    || report.end != kwil_core_workspace::catalogue::WalkEnd::Complete)
                            {
                                view.set_notice(format!(
                                    "File catalogue: {} files, {} skipped; {:?}",
                                    report.files, report.errors, report.end
                                ));
                                changed = true;
                            }
                        }
                        changed
                    }
                    Msg::InputClosed => {
                        document.begin_shutdown(Duration::from_secs(35)).map_err(std::io::Error::other)?;
                        view.cancel_hex_input();
                        view.begin_wrap_shutdown();
                        break;
                    }
                    Msg::SettingsPath(path) => {
                        let sender = tx.clone();
                        match path.and_then(|path| {
                            kwil_view_model::settings_session::SettingsSession::start(
                                path,
                                std::sync::Arc::new(move || {
                                    let _ = sender.send(Msg::SettingsPanelReady);
                                }),
                            )
                            .map_err(|error| error.to_string())
                        }) {
                            Ok(panel) => settings_panel = Some(panel),
                            Err(error) => {
                                view.set_notice(format!("Settings: {error}"));
                            }
                        }
                        true
                    }
                    Msg::SettingsPanelReady => {
                        let mut open_path = None;
                        if let Some(panel) = &mut settings_panel {
                            panel.worker_ready();
                            if let Some(applied) = panel.take_applied() {
                                let _ = tx.send(Msg::Settings(Box::new(applied.settings), applied.language_mappings));
                            }
                            if settings_file_pending && panel.source_exists() && !panel.is_saving() && !panel.is_dirty()
                            {
                                open_path = Some(panel.path().to_path_buf());
                                panel.close();
                                settings_file_pending = false;
                            } else if panel.last_error().is_some() {
                                settings_file_pending = false;
                            }
                        }
                        let opened = open_path.is_some();
                        if let Some(path) = open_path
                            && let Err(error) = view.open_path(path)
                        {
                            view.set_notice(format!("Open settings: {error}"));
                        }
                        opened || settings_panel.as_ref().is_some_and(|panel| panel.is_open())
                    }
                    Msg::Settings(next, mappings) => {
                        let accepted = if next.keymap == settings.keymap && next.bindings == settings.bindings {
                            settings = *next;
                            true
                        } else if let Ok(next_keys) = KeymapEngine::new(next.keymap, &next.bindings) {
                            keys = next_keys;
                            settings = *next;
                            true
                        } else {
                            false
                        };
                        if accepted && let Some(host) = highlighting {
                            host.apply_mappings(mappings);
                            if !view.hex_active() {
                                prepare_highlighting_for_draw(host, view);
                            }
                        }
                        view.set_session_restore(settings.files.restore_session);
                        if settings.editor.word_wrap && !view.hex_active() {
                            view.set_body_policy(kwil_view_model::ViewBodyPolicy::Prepared);
                        }
                        view.set_editor_settings(&settings);
                        search.settings(view, &mut keys, &settings);
                        true
                    }
                    Msg::Input(Event::Key(key))
                        if exiting
                            && (document.wrap_failed()
                                || view.wrap_settlement_state()
                                    == kwil_view_model::workspace_wrap::WorkspaceWrapSettlementState::Failed) =>
                    {
                        let intent = if let Some(host) = palette.as_mut().filter(|host| host.model().is_open()) {
                            palette_ui::key(view, host, key, &mut keys, started.elapsed(), settings.keymap)
                        } else {
                            keys.set_context(kwil_core_commands::Context::Editor);
                            // Decode only chrome/retry while failed shutdown
                            // owns input. Never run View's insertion fallback.
                            match chord(key).map(|chord| keys.feed_with_selection(chord)) {
                                Some(VimResult::Command(call)) if call.id == "view.retryWrap" => KeyResult::RetryWrap,
                                Some(VimResult::Command(call)) if call.id == "view.commandPalette" => {
                                    KeyResult::Palette(">")
                                }
                                _ => KeyResult::Changed(false),
                            }
                        };
                        let intent = resolve_bounded_wrap(
                            document,
                            view,
                            wrap.as_mut(),
                            intent,
                            started.elapsed(),
                            settings.keymap,
                        );
                        match intent {
                            KeyResult::Palette(query) => {
                                palette_ui::open(view, palette.as_mut(), &mut keys, query);
                                true
                            }
                            KeyResult::Changed(changed) => changed,
                            _ => {
                                view.set_notice("Wrap shutdown failed; use Retry Wrap in the command palette".into());
                                true
                            }
                        }
                    }
                    Msg::Input(_) if exiting || view.wrap_input_blocked() => false,
                    Msg::OpenExternal => external_gate.wake(
                        view,
                        &external_inbox,
                        settings_panel.as_ref().is_some_and(|panel| panel.is_open()),
                    ),
                    Msg::Notice(n) => {
                        view.set_notice(n);
                        true
                    }
                    Msg::Input(event)
                        if binary_offer::Host::captures(
                            view,
                            &event,
                            settings_panel.as_ref().is_some_and(|panel| panel.is_open()),
                        ) =>
                    {
                        let size = terminal.size()?;
                        binary_offer.input(view, &mut keys, &event, Rect::new(0, 0, size.width, size.height))
                    }
                    Msg::Input(Event::Resize(_, _)) => {
                        view.cancel_hex_input();
                        true
                    }
                    Msg::Input(Event::FocusLost | Event::FocusGained) => {
                        view.cancel_hex_input();
                        keys.cancel_pending();
                        true
                    }
                    Msg::Input(Event::Key(k)) if k.kind != KeyEventKind::Release => {
                        let wrap_cancelled = k.code == KeyCode::Esc
                            && !blocked
                            && wrap.as_ref().is_some_and(|host| {
                                host.gate() != kwil_view_model::wrap_session::MutationGate::Allowed
                            });
                        if wrap_cancelled {
                            if let Some(host) = wrap {
                                host.cancel();
                            }
                            keys.cancel_pending();
                            view.set_notice("Wrapped movement cancelled".into());
                        }
                        let intent = if wrap_cancelled {
                            KeyResult::Changed(true)
                        } else if let Some(panel) = settings_panel.as_mut().filter(|panel| panel.is_open()) {
                            if k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER)
                                && matches!(k.code, KeyCode::Char('o' | 'O'))
                                && !panel.panel_is_capturing()
                            {
                                KeyResult::SettingsFile
                            } else {
                                let size = terminal.size()?;
                                KeyResult::Changed(settings_ui::key(panel, k, (size.width, size.height)))
                            }
                        } else if view.file_prompt().is_some() {
                            file_prompt::key(view, k)
                        } else if language_menu.is_some() {
                            language_key(
                                &mut language_menu,
                                highlighting.as_mut(),
                                view,
                                &mut keys,
                                k,
                                started.elapsed(),
                                settings.keymap,
                            )
                        } else if let Some(host) = palette.as_mut().filter(|host| host.model().is_open()) {
                            palette_ui::key(view, host, k, &mut keys, started.elapsed(), settings.keymap)
                        } else if view.close_pending() && k.code == KeyCode::Esc {
                            KeyResult::Cancel
                        } else if search.is_open() {
                            search_ui::key(search, view, k, &mut keys, &settings, started.elapsed())
                        } else if view.hex_active() {
                            hex_ui::host_key(view, k, &mut keys, started.elapsed(), settings.keymap)
                        } else {
                            search_ui::key(search, view, k, &mut keys, &settings, started.elapsed())
                        };
                        let intent = hex_ui::resolve(view, &mut keys, intent);
                        let intent = search_ui::resolve(search, view, &mut keys, &settings, intent);
                        let blocked = wrap_modal(
                            view,
                            search,
                            palette.as_ref(),
                            settings_panel.as_ref(),
                            language_menu.is_some(),
                        );
                        if let Some(host) = wrap {
                            host.sync(view, body, blocked);
                        }
                        let intent = resolve_bounded_wrap(
                            document,
                            view,
                            wrap.as_mut(),
                            intent,
                            started.elapsed(),
                            settings.keymap,
                        );
                        match intent {
                            KeyResult::RetryWrap | KeyResult::BodyInvoke(_, _) | KeyResult::BodyExecute(_, _, _) => {
                                unreachable!("body intent resolved")
                            }
                            KeyResult::Search(_) => unreachable!("search intent resolved above"),
                            KeyResult::Hex(_, _) => unreachable!("hex intent resolved above"),
                            KeyResult::Language => {
                                if let Some(host) = &highlighting {
                                    if let Some(palette) = &mut palette {
                                        let _ = palette.input(kwil_view_model::palette_session::PaletteInput::Cancel);
                                    }
                                    keys.cancel_pending();
                                    keys.set_context(kwil_core_commands::Context::Palette);
                                    language_menu = Some(LanguageMenu::new(host, view));
                                } else {
                                    view.set_notice("Language highlighting is unavailable".into());
                                }
                                true
                            }
                            KeyResult::SettingsFile => {
                                if let Some(panel) = &mut settings_panel {
                                    panel.open();
                                    panel.clear_error();
                                    if panel.menu().is_none() || panel.source_exists() || panel.has_conflict() {
                                        let path = panel.path().to_path_buf();
                                        panel.close();
                                        settings_file_pending = false;
                                        if let Err(error) = view.open_path(path) {
                                            view.set_notice(format!("Open settings: {error}"));
                                        }
                                    } else {
                                        settings_file_pending = true;
                                        if let Err(error) =
                                            panel.input(kwil_view_model::settings_session::SettingsInput::Save)
                                        {
                                            view.set_notice(format!("Settings: {error}"));
                                            settings_file_pending = false;
                                        }
                                    }
                                }
                                true
                            }
                            KeyResult::Settings => {
                                if let Some(host) = &mut palette {
                                    let _ = host.input(kwil_view_model::palette_session::PaletteInput::Cancel);
                                }
                                keys.set_context(kwil_core_commands::Context::Editor);
                                keys.cancel_pending();
                                if let Some(panel) = &mut settings_panel {
                                    panel.open();
                                } else {
                                    view.set_notice("Settings are starting; try again shortly".into());
                                }
                                true
                            }
                            KeyResult::Palette(query) => {
                                palette_ui::open(view, palette.as_mut(), &mut keys, query);
                                true
                            }
                            KeyResult::Quit => {
                                let discard = view.close_pending() && k.modifiers.contains(KeyModifiers::SHIFT);
                                if let Err(error) = view.request_window_close(discard) {
                                    view.set_notice(error.to_string());
                                }
                                if view.wants_exit() {
                                    exiting = true;
                                }
                                true
                            }
                            KeyResult::Save(_) | KeyResult::SaveAndQuit => {
                                let (path, quit) = match intent {
                                    KeyResult::Save(path) => (path, false),
                                    KeyResult::SaveAndQuit => (None, true),
                                    _ => unreachable!(),
                                };
                                let result = if quit {
                                    view.file_command("file.saveAndQuit").map(|_| ())
                                } else {
                                    let id = view.active_id();
                                    if path.is_none() {
                                        view.file_command("file.save").map(|_| ())
                                    } else {
                                        view.save_document(id, path)
                                    }
                                };
                                if let Err(error) = result {
                                    view.set_notice(format!("Save failed: {error}"));
                                }
                                true
                            }
                            KeyResult::File(command) => {
                                if let Err(error) = view.file_command(command) {
                                    view.set_notice(error.to_string());
                                }
                                if view.wants_exit() {
                                    exiting = true;
                                }
                                true
                            }
                            KeyResult::Cancel => {
                                view.cancel_file_prompt();
                                view.set_notice(String::new());
                                true
                            }
                            KeyResult::Changed(c) => c,
                        }
                    }
                    Msg::Input(Event::Paste(text)) => {
                        if let Some(panel) = settings_panel.as_mut().filter(|panel| panel.is_open()) {
                            let _ = panel.input(kwil_view_model::settings_session::SettingsInput::Text(&text));
                            true
                        } else if let Some(prompt) = view.file_prompt_mut() {
                            if !text.chars().any(char::is_control) {
                                prompt.text.push_str(&text);
                            }
                            view.prompt_notice();
                            true
                        } else if let Some(menu) = &mut language_menu {
                            menu.text(view, &text);
                            true
                        } else if let Some(host) = palette.as_mut().filter(|host| host.model().is_open()) {
                            let intent = palette_ui::input(
                                view,
                                host,
                                &mut keys,
                                kwil_view_model::palette_session::PaletteInput::Text(&text),
                                started.elapsed(),
                                settings.keymap,
                            );
                            let intent = hex_ui::resolve(view, &mut keys, intent);
                            let _ = search_ui::resolve(search, view, &mut keys, &settings, intent);
                            true
                        } else if search.is_open() {
                            search.input(view, &mut keys, kwil_view_model::search_panel::SearchInput::Text(&text))
                        } else if view.hex_active() {
                            hex_ui::host_paste(view, &text)
                        } else if keys.mode() == kwil_core_commands::Mode::Insert {
                            let intent = body_execute(view, "edit.insert", Some(text), false, started.elapsed());
                            let _ = resolve_bounded_wrap(
                                document,
                                view,
                                wrap.as_mut(),
                                intent,
                                started.elapsed(),
                                settings.keymap,
                            );
                            true
                        } else {
                            false
                        }
                    }
                    Msg::Input(Event::Mouse(_)) if settings_panel.as_ref().is_some_and(|panel| panel.is_open()) => true,
                    Msg::Input(Event::Mouse(m)) if language_menu.is_some() && view.file_prompt().is_none() => {
                        let size = terminal.size()?;
                        language_mouse(
                            &mut language_menu,
                            highlighting.as_mut(),
                            view,
                            &mut keys,
                            &m,
                            Rect::new(0, 0, size.width, size.height),
                        );
                        true
                    }
                    Msg::Input(Event::Mouse(m)) if palette.as_ref().is_some_and(|host| host.model().is_open()) => {
                        let size = terminal.size()?;
                        palette_ui::mouse(
                            view,
                            palette.as_mut().unwrap(),
                            &mut keys,
                            &m,
                            Rect::new(0, 0, size.width, size.height),
                        )
                    }
                    Msg::Input(Event::Mouse(m)) if search.is_open() && view.file_prompt().is_none() => {
                        let size = terminal.size()?;
                        search_ui::mouse(search, view, &mut keys, &m, Rect::new(0, 0, size.width, size.height))
                    }
                    Msg::Input(Event::Mouse(m))
                        if {
                            let size = terminal.size()?;
                            tabs::mouse(view, &m, size.width, size.height, settings.tabs.layout)
                        } =>
                    {
                        true
                    }
                    Msg::Input(Event::Mouse(m)) => match m.kind {
                        _ if view.hex_active() => {
                            let size = terminal.size()?;
                            hex_ui::host_mouse(view, m, size.width, size.height, settings.tabs.layout)
                        }
                        _ if wrap.as_mut().is_some_and(|host| match host.mouse(m) {
                            wrap_ui::Input::Pass => host.owns_body(view),
                            input => {
                                if let Some(message) = input.notice() {
                                    view.set_notice(message.into());
                                }
                                true
                            }
                        }) =>
                        {
                            true
                        }
                        MouseEventKind::ScrollDown => view.scroll(3) != Default::default(),
                        MouseEventKind::ScrollUp => view.scroll(-3) != Default::default(),
                        _ => false,
                    },
                    Msg::Input(_) => false,
                };
            if view.wants_exit() {
                exiting = true;
            }
            if exiting {
                let deadline = document.begin_shutdown(Duration::from_secs(35)).map_err(std::io::Error::other)?;
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    return Err(std::io::Error::other("TUI accepted-exit deadline reached; exact owners retained"));
                }
                if let Some(host) = wrap {
                    changed |= host.shutdown_step(view, remaining);
                    if host.shutdown_ready(view) {
                        break;
                    }
                } else {
                    break;
                }
            }
            if let Some(host) = &wrap {
                search.body_gate(host.gate(), host.closed() || view.wrap_input_blocked());
            }
            changed |= binary_offer.sync(view, &mut keys);
            external_gate.replay(view, settings_panel.as_ref().is_some_and(|panel| panel.is_open()), &tx);
            if raw_body != view.hex_active() {
                raw_body = view.hex_active();
                view.cancel_hex_input();
                keys.cancel_pending();
                keys.set_context(if raw_body {
                    kwil_core_commands::Context::Hex
                } else {
                    kwil_core_commands::Context::Editor
                });
            }
            if exiting {
                // Search source owners remain retained for its explicit input
                // release phase; accepted exit must not start a fresh snapshot.
            } else if view.hex_active() {
                if !search.files_mode() {
                    search.close(view, &mut keys);
                }
                if language_menu.take().is_some() {
                    changed = true;
                }
            } else {
                changed |= search.sync(view, &mut keys);
            }
            if language_menu.as_ref().is_some_and(|menu| !menu.ticket.is_current(view)) {
                language_menu = None;
                keys.set_context(kwil_core_commands::Context::Editor);
                keys.cancel_pending();
                changed = true;
            }
            palette_ui::sync_input_identity(view, &mut keys, &mut input_identity);
            if let Some(host) = palette.as_mut().filter(|host| host.model().is_open())
                && let Err(error) = host.refresh_tabs(view)
            {
                view.set_notice(format!("Palette: {error:?}"));
            }
            if changed {
                if user_input {
                    view.checkpoint_session();
                }
                let size = terminal.size()?;
                let geometry = tabs::geometry(view, size.width, size.height, settings.tabs.layout);
                if view.rows() != geometry.body.height {
                    view.set_rows(geometry.body.height);
                }
                if view.hex_active() {
                    // Resize and scroll submit the latest bounded page through
                    // Workspace. Editor-bearing requests never live in the UI.
                    let _ = view.submit_hex_page();
                } else if !wrap.as_ref().is_some_and(|host| host.owns_body(view))
                    && !palette.as_ref().is_some_and(|host| host.model().is_open())
                {
                    keep_visible(
                        view,
                        &mut horizontal,
                        geometry.body.width.saturating_sub(8),
                        settings.editor.tab_size as usize,
                    );
                }
                // ratatui diffs against the previous frame and writes only
                // the changed cells.
                let body = wrap_body_rect(view, size.width, size.height, settings.tabs.layout);
                let blocked =
                    wrap_modal(view, search, palette.as_ref(), settings_panel.as_ref(), language_menu.is_some());
                if let Some(host) = wrap {
                    host.sync(view, body, blocked);
                }
                if !view.hex_active()
                    && let Some(host) = highlighting
                {
                    prepare_highlighting_wrapped(host, view, wrap.as_mut());
                }
                let prepared = wrap.as_ref().is_some_and(|host| host.owns_body(view));
                let spans: Vec<_> = if prepared {
                    if let Some((page, context)) = wrap
                        .as_ref()
                        .and_then(|runtime| runtime.host())
                        .and_then(|host| Some((host.page()?, host.context()?)))
                    {
                        highlighting
                            .as_ref()
                            .into_iter()
                            .flat_map(|host| host.spans_prepared(view, page, context))
                            .collect()
                    } else {
                        Vec::new()
                    }
                } else {
                    highlighting.as_ref().into_iter().flat_map(|host| host.spans(view)).collect()
                };
                let prepared_status = wrap.as_ref().filter(|_| prepared).map(|host| {
                    let mut status = host.status(view);
                    if let Some(error) = highlighting.as_ref().and_then(|syntax| syntax.prepared_error()) {
                        status.push_str(&format!(" | Syntax: {error}"));
                    }
                    status
                });
                terminal.draw(|f| {
                    tabs::draw_prepared(f, view, settings.tabs.layout, &spans, prepared, prepared_status.as_deref());
                    if !view.wrap_input_blocked()
                        && let Some(host) = wrap.as_ref().and_then(wrap_ui::Runtime::host)
                        && let Some(key) = host.key()
                    {
                        host.draw(f, &wrap_ui::Paint { syntax: &spans, ..wrap_ui::Paint::empty(key) });
                    }
                    if view.hex_active() {
                        hex_ui::draw_workspace(f, view, settings.tabs.layout);
                    } else {
                        if let Some(host) = &highlighting {
                            draw_language_status(f, host, view);
                        }
                    }
                    search_ui::draw_prepared(
                        f,
                        search,
                        view,
                        settings.tabs.layout,
                        if view.wrap_input_blocked() { None } else { wrap.as_ref().and_then(wrap_ui::Runtime::host) },
                        prepared,
                    );
                    if let Some(host) = &palette {
                        palette_ui::draw(f, host.model());
                    }
                    if let Some(menu) = &language_menu {
                        draw_language(f, menu);
                    }
                    if let Some(panel) = &settings_panel {
                        settings_ui::draw(f, panel);
                    }
                    if !settings_panel.as_ref().is_some_and(|panel| panel.is_open()) {
                        binary_offer.draw(f, view);
                    }
                })?;
                if let Some(host) = wrap {
                    host.painted();
                }
            }
        }
        Ok(())
    }));
    let mut result = match attempted {
        Ok(result) => result,
        Err(payload) => {
            startup.ticket.terminal_panic = Some(payload);
            startup.ticket.terminal_failure = Some("TUI startup or event loop panicked; exact owner retained".into());
            Err(std::io::Error::other("TUI startup or event loop panicked; exact owner retained"))
        }
    };
    if terminal_touched || terminal.is_some() {
        // A partial terminal setup restores through the same guard. Restoration
        // errors never replace the first actual startup I/O error.
        let restored = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = crossterm::execute!(
                std::io::stdout(),
                crossterm::event::DisableMouseCapture,
                crossterm::event::DisableBracketedPaste
            );
            ratatui::restore();
            // Terminal Drop is also inside the borrow guard, before the
            // original source owner can move into a refusal or cleanup slot.
            drop(terminal.take());
        }));
        if let Err(payload) = restored {
            if startup.ticket.terminal_panic.is_none() {
                startup.ticket.terminal_panic = Some(payload);
            }
            startup.ticket.terminal_failure = Some("TUI terminal restoration panicked; exact owner retained".into());
            result = combine_shutdown_results(
                result,
                Err(std::io::Error::other("TUI terminal restoration panicked; exact owner retained")),
            );
        }
    }
    if !startup.worker_admitted()
        && startup.ticket.terminal_panic.is_none()
        && let Err(error) = result
    {
        return ControlFlow::Break(startup.refuse(error));
    }
    if let Some(document) = &mut startup.ticket.document {
        let deadline_result =
            document.begin_shutdown(Duration::from_secs(35)).map(|_| ()).map_err(std::io::Error::other);
        result = combine_shutdown_results(result, deadline_result);
    }
    let aux_stop = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> std::io::Result<_> {
        let settings_shutdown = if let Some(panel) = &mut settings_panel {
            let shutdown = panel.stop();
            Some(std::thread::Builder::new().name("kwil-settings-cleanup".into()).spawn(move || shutdown.shutdown())?)
        } else {
            None
        };
        if let Some(host) = &mut palette {
            let handles = host.stop();
            std::thread::Builder::new().name("kwil-palette-cleanup".into()).spawn(move || {
                for handle in handles {
                    let _ = handle.join();
                }
            })?;
        }
        Ok(settings_shutdown)
    }));
    let settings_shutdown = match aux_stop {
        Ok(Ok(shutdown)) => shutdown,
        Ok(Err(error)) => {
            let (ticket, budget) = startup.into_parts();
            let error = combine_shutdown_results(result, Err(error)).unwrap_err();
            return ControlFlow::Continue(Err(retain_shutdown_error(ticket, Some(budget), error)));
        }
        Err(payload) => {
            if startup.ticket.terminal_panic.is_none() {
                startup.ticket.terminal_panic = Some(payload);
            }
            startup.ticket.terminal_failure = Some("TUI auxiliary cleanup setup panicked; exact owner retained".into());
            None
        }
    };
    if startup.ticket.terminal_panic.is_some() {
        let (ticket, budget) = startup.into_parts();
        let error = result.err().unwrap_or_else(|| std::io::Error::other("TUI panicked before ownership transfer"));
        return ControlFlow::Continue(Err(retain_shutdown_error(ticket, Some(budget), error)));
    }
    let (ticket, budget) = startup.into_parts();
    let cleanup_result = shutdown_bounded_ticket(ticket, Some(budget));
    if let Some(shutdown) = settings_shutdown {
        let settings_result = shutdown
            .join()
            .map_err(|_| std::io::Error::other("settings shutdown worker panicked"))
            .and_then(|result| result.map_err(std::io::Error::other));
        return ControlFlow::Continue(combine_shutdown_results(
            combine_shutdown_results(result, cleanup_result),
            settings_result,
        ));
    }
    ControlFlow::Continue(combine_shutdown_results(result, cleanup_result))
}
#[cfg(test)]
fn shutdown_bounded_workspace(
    view: Workspace,
    search: search_ui::SearchHost,
    rx: mpsc::Receiver<Msg>,
    document: DocumentLane,
    wrap: Option<wrap_ui::Runtime>,
    highlighting: Option<kwil_view_model::highlight_session::HighlightSession>,
    deadline: std::time::Instant,
) -> ControlFlow<test_support::TestCleanupFailure, ShutdownTicket> {
    let ticket = ShutdownTicket {
        document: Some(document),
        highlighting,
        search_binding: None,
        search_release_refusal: None,
        terminal_panic: None,
        wrap,
        view: Some(view),
        search: Some(search),
        worker: None,
        rx: Some(rx),
        terminal_failure: None,
        #[cfg(test)]
        stopped: None,
        #[cfg(test)]
        document_started: false,
    };
    test_support::finish_bounded_ticket_off_ui(ticket, deadline)
}

fn shutdown_bounded_ticket(ticket: ShutdownTicket, budget: Option<AllocationBudget>) -> std::io::Result<()> {
    // Production reserves the same process-owned retry slot before launching
    // its existing cleanup lane. Headless fixtures use their own identical
    // slot, allowing independent real workers to run without sharing owners.
    #[cfg(not(test))]
    {
        let mut slot = shutdown_slot().lock().unwrap_or_else(|error| error.into_inner());
        if slot.ticket.is_some() || slot.retrying || slot.terminal_failure.is_some() {
            drop(slot);
            return Err(retain_shutdown_error(ticket, budget, std::io::Error::other("TUI cleanup slot already owned")));
        }
        slot.ticket = Some(ticket);
        slot.startup_budget = budget;
        drop(slot);
        retry_shutdown_slot(shutdown_slot(), Duration::from_secs(35), finish_shutdown)
    }
    #[cfg(test)]
    {
        let owner_slot =
            std::sync::Mutex::new(ShutdownSlot { ticket: Some(ticket), startup_budget: budget, ..Default::default() });
        let result = retry_shutdown_slot(&owner_slot, Duration::from_secs(35), finish_shutdown);
        if let Err(error) = result {
            let mut owner = owner_slot.into_inner().unwrap_or_else(|error| error.into_inner());
            return Err(retain_shutdown_error(owner.ticket.take().unwrap(), owner.startup_budget.take(), error));
        }
        Ok(())
    }
}

/// Runs only on the current retained cleanup executor. Every receive and both
/// worker settlements share ONE absolute deadline; no automatic executor,
/// timer, polling or zero-time UI settlement is installed on failure.
fn finish_bounded_shutdown(ticket: &mut ShutdownTicket, timeout: Duration, explicit_retry: bool) -> Result<(), String> {
    use kwil_view_model::workspace_document::{DocumentShutdownReason, DocumentShutdownStatus};
    let document = ticket.document.as_mut().expect("bounded cleanup owner");
    let deadline = document.begin_shutdown(timeout)?;
    let remaining = || deadline.saturating_duration_since(std::time::Instant::now());
    if remaining().is_zero() {
        return Err("TUI shutdown original absolute deadline reached; exact owner retained".into());
    }
    if !explicit_retry && let Some(reason) = document.settlement_reason {
        return Err(format!("Document settlement remains {reason:?}; explicit retry_pending_shutdown required"));
    }
    if document.replacement.is_some() {
        return Err("Live replacement acknowledgement remains retained; cleanup blocked".into());
    }
    if ticket.search_release_refusal.is_some() {
        return Err("Exact refused search-input receipt is retained; cleanup blocked".into());
    }
    if !explicit_retry && let Some(reason) = &document.bind_reason {
        return Err(format!("Document shutdown binding remains refused: {reason:?}; exact owners retained"));
    }
    if let Some(status) = document
        .transport
        .as_ref()
        .map(kwil_view_model::workspace_document::WorkspaceDocumentTransport::status)
        .or_else(|| {
            document.shutdown.as_ref().map(kwil_view_model::workspace_document::WorkspaceDocumentShutdown::status)
        })
        && status.failure.is_some()
        && !status.retry_pending
        && !status.retry_acknowledgement_required
    {
        document.retry = None;
    }
    if explicit_retry && document.wrap_failed() && document.retry.is_none() {
        document.retry_wrap(remaining())?;
    }
    let mut keys = KeymapEngine::new(Preset::VsCode, &[]).expect("bundled keymap");
    if let Some(wrap) = &mut ticket.wrap {
        wrap.rollback_off_ui(remaining()).map_err(|error| error.to_string())?;
    }
    if document.transport.is_some() {
        let view = ticket.view.as_mut().expect("document owner retains its Workspace");
        let search = ticket.search.as_mut().expect("document owner retains search receipts");
        if let Some(wrap) = &mut ticket.wrap {
            view.begin_wrap_shutdown();
            while !view.wrap_cleanup_ready() {
                if view.wrap_settlement_error().is_some() && document.retry.is_none() {
                    return Err("Wrap settlement failed; exact error retained, explicit retry required".into());
                }
                wrap.shutdown_step(view, remaining());
                if view.wrap_cleanup_ready() {
                    break;
                }
                receive_bounded_document(
                    document,
                    view,
                    search,
                    &mut keys,
                    ticket.rx.as_ref().expect("document event receiver"),
                    &remaining,
                )?;
            }
        }
        // Frontend layout roots retire on this existing executor before the
        // sealed bundle can release Workspace/document roots.
        drop(ticket.wrap.take());
        if let Some(highlighting) = &mut ticket.highlighting {
            highlighting
                .settle_until(deadline)
                .map_err(|reason| format!("Highlight retirement: {reason:?}; exact owner retained"))?;
        }
        drop(ticket.highlighting.take());
        let transport = document.transport.take().expect("checked live transport");
        let view = ticket.view.take().expect("original Workspace");
        match view.into_document_shutdown(transport) {
            ControlFlow::Continue(mut shutdown) => {
                // The expected identity is captured before the search worker's
                // first close. Source release and spool cleanup are separate.
                let binding = ticket.search_binding.take().unwrap_or_else(|| search.source_release_binding());
                if let Err(binding) = shutdown.bind_search_sources(binding) {
                    ticket.search_binding = Some(binding);
                    document.shutdown = Some(shutdown);
                    return Err("Document search binding refused; exact key and bundle retained".into());
                }
                document.shutdown = Some(shutdown);
            }
            ControlFlow::Break(refusal) => {
                ticket.view = Some(refusal.workspace);
                document.transport = Some(refusal.transport);
                document.bind_reason = Some(refusal.reason);
                return Err("Document shutdown binding refused; exact Workspace, transport and reason retained".into());
            }
        }
    }
    if document.shutdown.is_some() {
        if let Some(binding) = ticket.search_binding.take()
            && let Err(binding) = document.shutdown.as_mut().unwrap().bind_search_sources(binding)
        {
            ticket.search_binding = Some(binding);
            return Err("Exact document search binding remains refused".into());
        }
        loop {
            let status = document.shutdown.as_mut().unwrap().settle_until(deadline);
            match status {
                Ok(DocumentShutdownStatus::ReplacementReceiptPending) => {
                    let receipt = document
                        .shutdown
                        .as_ref()
                        .unwrap()
                        .replacement_receipt()
                        .ok_or("Missing exact shutdown replacement receipt")?;
                    let search = ticket.search.as_mut().ok_or("Search receipt owner is missing")?;
                    if !search.shutdown_replacement_matches(receipt) {
                        return Err("Shutdown replacement receipt differs from the original search loan".into());
                    }
                    if !document.shutdown.as_mut().unwrap().acknowledge_replacement_receipt(receipt) {
                        return Err("Exact document replacement acknowledgement refused".into());
                    }
                    search.acknowledge_shutdown_replacement(receipt)?;
                }
                Ok(DocumentShutdownStatus::WrapRetryReceiptPending) => {
                    if document.retry.is_none() {
                        return Err("Wrap retry receipt has no retained original token; bundle retained".into());
                    }
                    document.acknowledge_retry()?;
                }
                Ok(DocumentShutdownStatus::PrepublicationResetPending) => {
                    return Err("Document save reset requires explicit producer retry; exact bundle retained".into());
                }
                Ok(DocumentShutdownStatus::SourcesReleased) => {
                    if ticket.worker.is_none() {
                        ticket.worker =
                            ticket.search.as_mut().ok_or("Search owner is missing")?.take_shutdown_worker()?;
                    }
                    if let Some(worker) = &ticket.worker {
                        let receipt = worker
                            .wait_inputs_released(remaining())
                            .map_err(|error| format!("Search input release: {error:?}; exact worker retained"))?;
                        if let Err(refusal) =
                            document.shutdown.as_mut().unwrap().acknowledge_search_inputs_released(receipt)
                        {
                            ticket.search_release_refusal = Some(refusal);
                            return Err("Search input release acknowledgement refused; exact receipt retained".into());
                        }
                    }
                    // Panel retry/source owners have entered the actual search
                    // worker before its input-release proof is acknowledged.
                    drop(ticket.search.take());
                }
                Ok(DocumentShutdownStatus::Complete) => {
                    document.settled = true;
                    document.settlement_reason = None;
                    document.retry = None;
                    drop(document.shutdown.take());
                    drop(document.refused_prepare.take());
                    break;
                }
                Err(reason) => {
                    #[cfg(test)]
                    {
                        document.cleanup_attempt = match reason {
                            DocumentShutdownReason::Transport(reason) => {
                                Some(test_support::CleanupAttemptFailure::Document(reason))
                            }
                            _ => None,
                        };
                    }
                    if let DocumentShutdownReason::Transport(reason) = reason {
                        document.settlement_reason = Some(reason);
                    }
                    return Err(format!("Document shutdown: {reason:?}; exact bundle and original errors retained"));
                }
            }
        }
    } else if !document.settled {
        // No worker ever started. All source roots still retire OFF UI.
        if let Some(highlighting) = &mut ticket.highlighting {
            highlighting
                .settle_until(deadline)
                .map_err(|reason| format!("Highlight retirement: {reason:?}; exact owner retained"))?;
        }
        drop(ticket.highlighting.take());
        if let Some(search) = &mut ticket.search {
            ticket.worker = search.take_shutdown_worker()?;
        }
        drop(ticket.search.take());
        drop(ticket.view.take());
        document.settled = true;
    }
    drop(ticket.rx.take());
    // Document/history/recipe roots and actual document join precede search
    // spool retirement and its actual join. All use the first deadline.
    if let Some(worker) = &mut ticket.worker {
        if explicit_retry {
            worker.retry_retirement();
        }
        worker.settle_until(deadline).map_err(|reason| {
            #[cfg(test)]
            {
                document.cleanup_attempt = Some(test_support::CleanupAttemptFailure::Search(reason));
            }
            format!("Search retirement: {reason:?}; exact worker retained")
        })?;
    }
    drop(ticket.worker.take());
    Ok(())
}
fn receive_bounded_document(
    document: &mut DocumentLane,
    view: &mut Workspace,
    search: &mut search_ui::SearchHost,
    keys: &mut KeymapEngine,
    rx: &mpsc::Receiver<Msg>,
    remaining: &impl Fn() -> Duration,
) -> Result<(), String> {
    if let Some(reason) = document.wrap_failure_reason()
        && document.retry.is_none()
    {
        document.settlement_reason = Some(reason);
        #[cfg(test)]
        {
            document.cleanup_attempt = Some(test_support::CleanupAttemptFailure::Document(reason));
        }
        return Err(format!("Wrap settlement retained ({reason:?}) in the worker slot; explicit retry required"));
    }
    let wait = remaining();
    if wait.is_zero() {
        return Err("TUI bounded document shutdown deadline reached".into());
    }
    match rx.recv_timeout(wait) {
        Ok(Msg::DocumentReady) => {
            document.ready(view, search, keys)?;
            Ok(())
        }
        Ok(_) => Ok(()),
        Err(error) => Err(format!("TUI bounded document shutdown wait: {error}; exact owner retained")),
    }
}

/// One retained shutdown owner per process. A failed drain never destroys the
/// actual Workspace, channel results or search facade on the input thread.
struct ShutdownTicket {
    document: Option<DocumentLane>,
    highlighting: Option<kwil_view_model::highlight_session::HighlightSession>,
    search_binding: Option<kwil_view_model::workspace_document::DocumentSearchBinding>,
    search_release_refusal: Option<kwil_view_model::workspace_document::SearchInputsReleaseRefusal>,
    terminal_panic: Option<Box<dyn std::any::Any + Send>>,
    wrap: Option<wrap_ui::Runtime>,
    view: Option<Workspace>,
    search: Option<search_ui::SearchHost>,
    worker: Option<kwil_view_model::search_session::SearchWorker>,
    rx: Option<mpsc::Receiver<Msg>>,
    #[cfg(test)]
    stopped: Option<mpsc::Receiver<Result<(), String>>>,
    #[cfg(test)]
    document_started: bool,
    terminal_failure: Option<String>,
}
#[derive(Default)]
struct ShutdownSlot {
    ticket: Option<ShutdownTicket>,
    retrying: bool,
    terminal_failure: Option<String>,
    terminal_panic: Option<Box<dyn std::any::Any + Send>>,
    // Source-free original counter, never a second allocation allowance.
    startup_budget: Option<AllocationBudget>,
}
fn shutdown_slot() -> &'static std::sync::Mutex<ShutdownSlot> {
    static SLOT: std::sync::OnceLock<std::sync::Mutex<ShutdownSlot>> = std::sync::OnceLock::new();
    SLOT.get_or_init(Default::default)
}
fn ensure_shutdown_idle() -> std::io::Result<()> {
    let slot = shutdown_slot().lock().unwrap_or_else(|error| error.into_inner());
    if slot.ticket.is_some() || slot.retrying || slot.terminal_failure.is_some() || slot.terminal_panic.is_some() {
        return Err(std::io::Error::other("TUI cleanup owner is retained; retry_pending_shutdown before another run"));
    }
    Ok(())
}
/// Retry the ONE process-owned cleanup ticket off the input/render thread.
/// Failure keeps that exact owner in the slot and reports its remaining phase.
/// A failed terminal acknowledgement remains failed across retries.
pub fn retry_pending_shutdown(timeout: std::time::Duration) -> std::io::Result<()> {
    retry_shutdown_slot(shutdown_slot(), timeout, |ticket, timeout| {
        if ticket.document.is_some() {
            return finish_bounded_shutdown(ticket, timeout, true);
        }
        if let Some(view) = &mut ticket.view
            && view.wrap_settlement_state() == kwil_view_model::workspace_wrap::WorkspaceWrapSettlementState::Failed
        {
            view.retry_wrap_settlement(timeout).map_err(|error| error.to_string())?;
        }
        finish_shutdown(ticket, timeout)
    })
}
enum ShutdownLaunch {
    System,
    #[cfg(test)]
    Refuse(std::io::Error),
}
fn retry_shutdown_slot(
    owner_slot: &std::sync::Mutex<ShutdownSlot>,
    timeout: std::time::Duration,
    operation: impl FnOnce(&mut ShutdownTicket, std::time::Duration) -> Result<(), String> + Send,
) -> std::io::Result<()> {
    retry_shutdown_slot_with_launch(owner_slot, timeout, operation, ShutdownLaunch::System)
}
fn retry_shutdown_slot_with_launch(
    owner_slot: &std::sync::Mutex<ShutdownSlot>,
    timeout: std::time::Duration,
    operation: impl FnOnce(&mut ShutdownTicket, std::time::Duration) -> Result<(), String> + Send,
    launch: ShutdownLaunch,
) -> std::io::Result<()> {
    {
        let mut slot = owner_slot.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(message) = &slot.terminal_failure {
            return Err(std::io::Error::other(message.clone()));
        }
        if slot.retrying {
            return Err(std::io::Error::other("TUI cleanup retry is already running"));
        }
        if slot.ticket.is_none() {
            return Ok(());
        }
        slot.retrying = true;
    }
    // The existing cleanup executor captures a BORROW of its retained slot.
    // A failed OS thread launch cannot destroy the Workspace on its caller.
    // The ownership guard remains outside the catch around &mut ticket.
    std::thread::scope(|scope| {
        let cleanup = move || {
            let mut slot = owner_slot.lock().unwrap_or_else(|error| error.into_inner());
            let ticket = slot.ticket.as_mut().expect("reserved cleanup owner");
            let result = guard_shutdown(ticket, |ticket| operation(ticket, timeout));
            if result.is_ok() {
                // Every actual worker joined and source owner retired above.
                // Only this successful OFF-UI branch removes the ticket.
                drop(slot.ticket.take());
                drop(slot.startup_budget.take());
            }
            slot.retrying = false;
            result
        };
        let launched = match launch {
            ShutdownLaunch::System => {
                std::thread::Builder::new().name("kwil-tui-cleanup".into()).spawn_scoped(scope, cleanup)
            }
            #[cfg(test)]
            ShutdownLaunch::Refuse(error) => Err(error),
        };
        let returned = match launched {
            Ok(worker) => worker.join(),
            Err(error) => {
                owner_slot.lock().unwrap_or_else(|error| error.into_inner()).retrying = false;
                return Err(error); // exact launch error, original owner unchanged
            }
        };
        match returned {
            Ok(Ok(())) => Ok(()),
            Ok(Err(message)) => Err(std::io::Error::other(format!(
                "{message}; exact TUI cleanup owner retained for retry_pending_shutdown"
            ))),
            Err(payload) => {
                let mut slot = owner_slot.lock().unwrap_or_else(|error| error.into_inner());
                slot.retrying = false;
                let message =
                    "TUI cleanup thread failed outside its ownership guard; no successful shutdown acknowledgement"
                        .to_owned();
                if let Some(ticket) = &mut slot.ticket {
                    ticket.terminal_panic = Some(payload);
                    ticket.terminal_failure = Some(message.clone());
                } else {
                    slot.terminal_panic = Some(payload);
                }
                slot.terminal_failure = Some(message.clone());
                Err(std::io::Error::other(message))
            }
        }
    })
}
fn guard_shutdown(
    ticket: &mut ShutdownTicket,
    operation: impl FnOnce(&mut ShutdownTicket) -> Result<(), String>,
) -> Result<(), String> {
    if let Some(message) = &ticket.terminal_failure {
        return Err(message.clone());
    }
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| operation(ticket))) {
        Ok(result) => result,
        Err(payload) => {
            let message =
                "TUI cleanup panicked; surviving exact ticket retained without a successful acknowledgement".to_owned();
            ticket.terminal_failure = Some(message.clone());
            ticket.terminal_panic = Some(payload);
            Err(message)
        }
    }
}
fn finish_shutdown(ticket: &mut ShutdownTicket, timeout: std::time::Duration) -> Result<(), String> {
    if ticket.document.is_some() {
        return finish_bounded_shutdown(ticket, timeout, false);
    }
    #[cfg(test)]
    return finish_legacy_shutdown(ticket, timeout);
    #[cfg(not(test))]
    Err("TUI cleanup is missing its bounded document owner".into())
}
#[cfg(test)]
fn finish_legacy_shutdown(ticket: &mut ShutdownTicket, timeout: std::time::Duration) -> Result<(), String> {
    if let Some(message) = &ticket.terminal_failure {
        return Err(message.clone());
    }
    let deadline = std::time::Instant::now() + timeout;
    let remaining = || deadline.saturating_duration_since(std::time::Instant::now());
    let mut keys = KeymapEngine::new(Preset::VsCode, &[]).expect("bundled keymap");
    if ticket.wrap.is_some() {
        return Err("Wrap cleanup requires the sealed bounded document owner".into());
    }
    if let (Some(view), Some(search)) = (&mut ticket.view, &mut ticket.search) {
        search.close(view, &mut keys);
        view.cancel_hex_input();
        drain_workspace(
            view,
            search,
            ticket.rx.as_ref().expect("workspace drain channel"),
            &mut keys,
            Workspace::prepare_hex_shutdown,
            &remaining,
            &mut ticket.terminal_failure,
        )?;
    }
    if let Some(search) = &mut ticket.search {
        ticket.worker = search.take_shutdown_worker()?;
    }
    drop(ticket.search.take());
    // Replacement undo/receipt recipes can pin the search worker's spool.
    // Loans/disposal have drained above; release these final document/result
    // owners OFF UI before waiting for that worker to retire its own files.
    drop(ticket.view.take());
    drop(ticket.rx.take());
    if let Some(worker) = ticket.worker.take() {
        worker.retry_retirement();
        if let Err(error) = worker.shutdown_until(deadline) {
            let message = error.to_string();
            ticket.worker = Some(error.into_worker());
            return Err(message);
        }
    }
    if ticket.document_started {
        match ticket.stopped.as_ref().ok_or("Missing controlled terminal receipt owner")?.recv_timeout(remaining()) {
            Ok(Ok(())) => {
                ticket.document_started = false;
            }
            Ok(Err(error)) => {
                let message = format!("TUI document worker stopped with error: {error}");
                ticket.terminal_failure = Some(message.clone());
                return Err(message);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let message =
                    "TUI document shutdown receipt channel closed without a successful acknowledgement".to_owned();
                ticket.terminal_failure = Some(message.clone());
                return Err(message);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => return Err("TUI document shutdown receipt deadline reached".into()),
        }
    }
    Ok(())
}
/// Every refused disposal, pending loan and InputClosed wake uses the SAME
/// deadline. The normal input loop never waits for raw disposal admission.
#[cfg(test)]
fn drain_workspace(
    view: &mut Workspace,
    _search: &mut search_ui::SearchHost,
    rx: &mpsc::Receiver<Msg>,
    _keys: &mut KeymapEngine,
    mut prepare: impl FnMut(&mut Workspace) -> bool,
    remaining: &impl Fn() -> std::time::Duration,
    _terminal_failure: &mut Option<String>,
) -> Result<(), String> {
    while view.search_replacement_pending() || !prepare(view) {
        let wait = remaining();
        if wait.is_zero() {
            return Err("TUI document/editor disposal deadline reached".into());
        }
        match rx.recv_timeout(wait) {
            Ok(Msg::DocumentReady) => return Err("Document wake requires its sealed transport owner".into()),
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => return Err("TUI document/editor disposal deadline reached".into()),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("TUI document/editor drain disconnected; exact owner retained".into());
            }
        }
    }
    Ok(())
}
struct RetainedShutdownError {
    message: String,
    original: std::io::Error,
    // Only a concurrent caller that bypassed the public run guard can reach
    // this fallback. Returning the error still preserves the rejected owner.
    rejected: std::sync::Mutex<Option<ShutdownTicket>>,
    rejected_budget: Option<AllocationBudget>,
}
impl std::fmt::Debug for RetainedShutdownError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, formatter)
    }
}
impl std::fmt::Display for RetainedShutdownError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let owns_rejected = self.rejected.lock().unwrap_or_else(|error| error.into_inner()).is_some();
        write!(
            formatter,
            "{}; exact TUI cleanup owner retained{}",
            self.message,
            if owns_rejected { " in this error" } else { " for retry_pending_shutdown" }
        )
    }
}
impl std::error::Error for RetainedShutdownError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.original)
    }
}
impl Drop for RetainedShutdownError {
    fn drop(&mut self) {
        let rejected = self.rejected.get_mut().unwrap_or_else(|error| error.into_inner());
        let Some(ticket) = rejected.take() else { return };
        let mut slot = shutdown_slot().lock().unwrap_or_else(|error| error.into_inner());
        if slot.ticket.is_none() && !slot.retrying {
            slot.ticket = Some(ticket);
            slot.startup_budget = self.rejected_budget.take();
        } else {
            // Abandoning an error cannot destroy sources on UI or acknowledge
            // cleanup. This exceptional quarantine remains a failed startup.
            std::mem::forget(ticket);
            if let Some(budget) = self.rejected_budget.take() {
                std::mem::forget(budget);
            }
        }
    }
}
#[cfg(test)]
fn retain_shutdown(ticket: ShutdownTicket, message: String) -> std::io::Error {
    retain_shutdown_error(ticket, None, std::io::Error::other(message))
}
fn retain_shutdown_error(
    ticket: ShutdownTicket,
    mut budget: Option<AllocationBudget>,
    original: std::io::Error,
) -> std::io::Error {
    let mut slot = shutdown_slot().lock().unwrap_or_else(|error| error.into_inner());
    let rejected = if slot.ticket.is_none() && !slot.retrying {
        slot.ticket = Some(ticket);
        slot.startup_budget = budget.take();
        None
    } else {
        Some(ticket)
    };
    std::io::Error::new(
        original.kind(),
        RetainedShutdownError {
            message: original.to_string(),
            original,
            rejected: std::sync::Mutex::new(rejected),
            rejected_budget: budget,
        },
    )
}
#[derive(Debug)]
struct CombinedShutdownError {
    primary: std::io::Error,
    cleanup: std::io::Error,
}
impl std::fmt::Display for CombinedShutdownError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}; cleanup: {}", self.primary, self.cleanup)
    }
}
impl std::error::Error for CombinedShutdownError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cleanup)
    }
}
fn combine_shutdown_results(primary: std::io::Result<()>, cleanup: std::io::Result<()>) -> std::io::Result<()> {
    match (primary, cleanup) {
        (Err(primary), Err(cleanup)) => Err(std::io::Error::other(CombinedShutdownError { primary, cleanup })),
        (Err(error), _) | (_, Err(error)) => Err(error),
        _ => Ok(()),
    }
}

enum KeyResult {
    RetryWrap,
    BodyInvoke(kwil_core_commands::vim::Invocation, bool),
    BodyExecute(&'static str, CommandArgs, u32),
    Settings,
    SettingsFile,
    Quit,
    Save(Option<std::path::PathBuf>),
    SaveAndQuit,
    Cancel,
    Changed(bool),
    File(&'static str),
    Palette(&'static str),
    Search(kwil_core_commands::vim::Invocation),
    Language,
    Hex(kwil_core_commands::vim::Invocation, bool),
}

/// Preserve Workspace's body ownership at the palette invocation boundary;
/// existing View-only key tests retain the same text adapter.
trait TuiTarget {
    fn body_blocked(&self) -> bool {
        false
    }
    fn raw_body(&self) -> bool;
    fn text_view(&self) -> &View;
    fn text_view_mut(&mut self) -> &mut View;
}
impl TuiTarget for View {
    fn raw_body(&self) -> bool {
        self.editor().is_some_and(|editor| {
            editor.document().representation() == kwil_view_model::hex_session::Representation::RawBytes
        })
    }
    fn text_view(&self) -> &View {
        self
    }
    fn text_view_mut(&mut self) -> &mut View {
        self
    }
}
impl TuiTarget for Workspace {
    fn body_blocked(&self) -> bool {
        self.wrap_input_blocked()
    }
    fn raw_body(&self) -> bool {
        self.hex_active()
    }
    fn text_view(&self) -> &View {
        self
    }
    fn text_view_mut(&mut self) -> &mut View {
        self
    }
}

fn prepare_highlighting_for_draw(host: &mut kwil_view_model::highlight_session::HighlightSession, view: &Workspace) {
    if prepared_body(view)
        || view.body_policy() == kwil_view_model::ViewBodyPolicy::Prepared
        || view.wrap_input_blocked()
    {
        return;
    }
    if host.update(view) {
        host.invalidate_painted(view, &kwil_view_model::Damage { full: true, ..Default::default() });
    }
}
fn prepare_highlighting_wrapped(
    host: &mut kwil_view_model::highlight_session::HighlightSession,
    view: &Workspace,
    wrap: Option<&mut wrap_ui::Runtime>,
) {
    if let Some(wrap) = wrap.filter(|wrap| wrap.owns_body(view)) {
        let update = wrap.syntax_update_needed(host.language_choice(view.active_id()));
        if let Some((page, context)) = wrap.host().and_then(|body| Some((body.page()?, body.context()?))) {
            if !view.wrap_input_blocked() && update {
                host.update_prepared(view, page, context);
            } else if view.wrap_input_blocked() {
                host.suspend_prepared();
            }
        } else {
            host.suspend_prepared();
            wrap.suspend_syntax();
        }
    } else {
        prepare_highlighting_for_draw(host, view);
    }
}
fn highlighting_ready_wrapped(
    host: &mut kwil_view_model::highlight_session::HighlightSession,
    view: &Workspace,
    wrap: Option<&wrap_ui::Runtime>,
) -> bool {
    if let Some(wrap) = wrap.filter(|wrap| wrap.owns_body(view)) {
        if let Some((page, context)) = wrap.host().and_then(|body| Some((body.page()?, body.context()?)))
            && !view.wrap_input_blocked()
        {
            let before = host.prepared_error().map(ToString::to_string);
            let rows = host.worker_ready_prepared(view, page, context);
            return !rows.is_empty() || before != host.prepared_error().map(ToString::to_string);
        }
        host.suspend_prepared();
        false
    } else {
        highlighting_ready(host, view)
    }
}
fn highlighting_ready(host: &mut kwil_view_model::highlight_session::HighlightSession, view: &Workspace) -> bool {
    let status = host.language_status(view);
    let rows = host.worker_ready(view);
    !rows.is_empty() || host.language_status(view) != status
}

/// The shared immutable ticket fences every row against tab/epoch changes.
/// This adapter stores only a bounded filter and one catalogue row index.
struct LanguageMenu {
    ticket: kwil_view_model::language_selection::LanguagePicker,
    query: String,
    selected: usize,
}
impl LanguageMenu {
    fn new(host: &kwil_view_model::highlight_session::HighlightSession, view: &Workspace) -> Self {
        let ticket = host.language_picker(view);
        let selected = ticket.items("").position(|row| row.selected).unwrap_or(0);
        Self { ticket, query: String::new(), selected }
    }
    fn text(&mut self, view: &mut Workspace, text: &str) {
        if self.query.len().saturating_add(text.len()) > 256 || text.chars().any(char::is_control) {
            view.set_notice("Language filter must be at most 256 bytes without control characters".into());
            return;
        }
        self.query.push_str(text);
        self.selected = 0;
    }
    fn next(&mut self, previous: bool) {
        let count = self.ticket.items(&self.query).count();
        self.selected =
            if previous { self.selected.saturating_sub(1) } else { (self.selected + 1).min(count.saturating_sub(1)) };
    }
}
fn close_language(menu: &mut Option<LanguageMenu>, keys: &mut KeymapEngine) {
    *menu = None;
    keys.set_context(kwil_core_commands::Context::Editor);
    keys.cancel_pending();
}
fn accept_language(
    menu: &mut Option<LanguageMenu>,
    host: Option<&mut kwil_view_model::highlight_session::HighlightSession>,
    view: &mut Workspace,
    keys: &mut KeymapEngine,
) {
    let Some(current) = menu.as_ref() else { return };
    if !current.ticket.is_current(view) {
        view.set_notice("Language selection cancelled: its document changed".into());
        close_language(menu, keys);
        return;
    }
    let Some(row) = current.ticket.items(&current.query).nth(current.selected) else { return };
    if let Some(host) = host {
        if host.apply_picker(view, &current.ticket, row.choice) {
            prepare_highlighting_for_draw(host, view);
        }
        let label = host.language_status(view).label();
        view.set_notice(format!("File type: {label}"));
    } else {
        view.set_notice("Language highlighting is unavailable".into());
    }
    close_language(menu, keys);
}
fn language_key(
    menu: &mut Option<LanguageMenu>,
    host: Option<&mut kwil_view_model::highlight_session::HighlightSession>,
    view: &mut Workspace,
    keys: &mut KeymapEngine,
    event: KeyEvent,
    elapsed: std::time::Duration,
    preset: Preset,
) -> KeyResult {
    keys.set_context(kwil_core_commands::Context::Palette);
    let mut action = None;
    if let Some(chord) = chord(event) {
        match keys.feed(chord) {
            VimResult::Pending => return KeyResult::Changed(false),
            VimResult::Unbound if keys.suppresses_fallback() => return KeyResult::Changed(false),
            VimResult::Error(error) => {
                view.set_notice(error);
                return KeyResult::Changed(true);
            }
            VimResult::Command(call) => match call.id {
                "palette.cancel" => action = Some(KeyCode::Esc),
                "palette.accept" => action = Some(KeyCode::Enter),
                "palette.next" => action = Some(KeyCode::Down),
                "palette.previous" => action = Some(KeyCode::Up),
                "edit.backspace" => action = Some(KeyCode::Backspace),
                _ => {
                    if call.id.starts_with("edit.") || call.id.starts_with("motion.") {
                        return KeyResult::Changed(false);
                    }
                    close_language(menu, keys);
                    return invoke(view, call, false, elapsed, preset);
                }
            },
            VimResult::Unbound => {}
        }
    }
    match action.unwrap_or(event.code) {
        KeyCode::Esc => close_language(menu, keys),
        KeyCode::Enter => accept_language(menu, host, view, keys),
        KeyCode::Down => {
            if let Some(menu) = menu {
                menu.next(false);
            }
        }
        KeyCode::Up => {
            if let Some(menu) = menu {
                menu.next(true);
            }
        }
        KeyCode::Backspace => {
            if let Some(menu) = menu {
                menu.query.pop();
                menu.selected = 0;
            }
        }
        KeyCode::Char(c)
            if !event.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER) =>
        {
            if let Some(menu) = menu {
                menu.text(view, &c.to_string());
            }
        }
        _ => return KeyResult::Changed(false),
    }
    KeyResult::Changed(true)
}
fn language_area(bounds: Rect) -> Rect {
    let width = bounds.width.min(64);
    let height = bounds.height.min(18);
    Rect { x: bounds.x + (bounds.width - width) / 2, y: bounds.y + (bounds.height - height) / 3, width, height }
}
fn draw_language_status(
    frame: &mut ratatui::Frame,
    host: &kwil_view_model::highlight_session::HighlightSession,
    view: &Workspace,
) {
    let bounds = frame.area();
    if bounds.height == 0 {
        return;
    }
    let text = format!(" {} ", host.language_status(view).label());
    let width = (text.len() as u16).min(bounds.width);
    frame.render_widget(
        Paragraph::new(text).style(Style::default().fg(Color::Rgb(0x8f, 0xe3, 0xff)).bg(Color::Rgb(0x08, 0x0d, 0x18))),
        Rect { x: bounds.right() - width, y: bounds.bottom() - 1, width, height: 1 },
    );
}
fn draw_language(frame: &mut ratatui::Frame, menu: &LanguageMenu) {
    let area = language_area(frame.area());
    if area.width < 4 || area.height < 4 {
        return;
    }
    let style = Style::default().fg(Color::Rgb(0xd6, 0xe4, 0xf0)).bg(Color::Rgb(0x08, 0x0d, 0x18));
    frame.render_widget(ratatui::widgets::Clear, area);
    frame.render_widget(Paragraph::new("").style(style), area);
    let title = format!("File type ({})  Esc close", menu.ticket.status().label());
    frame.render_widget(Paragraph::new(title).style(style), Rect { height: 1, ..area });
    frame.render_widget(
        Paragraph::new(format!("> {}", menu.query)).style(style),
        Rect { y: area.y + 1, height: 1, ..area },
    );
    let visible = area.height as usize - 3;
    let first = menu.selected.saturating_sub(visible - 1);
    for (index, row) in menu.ticket.items(&menu.query).enumerate().skip(first).take(visible) {
        let background =
            if index == menu.selected { Color::Rgb(0x19, 0x2a, 0x43) } else { Color::Rgb(0x08, 0x0d, 0x18) };
        let label = format!(
            "{} {}{}",
            if row.selected { "*" } else { " " },
            row.label,
            if row.detected { " (detected)" } else { "" }
        );
        frame.render_widget(
            Paragraph::new(label).style(style.bg(background)),
            Rect { y: area.y + 2 + (index - first) as u16, height: 1, ..area },
        );
    }
    frame.render_widget(
        Paragraph::new("Enter apply  Up/Down choose  Type to filter").style(style),
        Rect { y: area.bottom() - 1, height: 1, ..area },
    );
    frame.set_cursor_position((area.x + 2, area.y + 1));
}
fn language_mouse(
    menu: &mut Option<LanguageMenu>,
    host: Option<&mut kwil_view_model::highlight_session::HighlightSession>,
    view: &mut Workspace,
    keys: &mut KeymapEngine,
    event: &event::MouseEvent,
    bounds: Rect,
) {
    let Some(current) = menu.as_mut() else { return };
    match event.kind {
        MouseEventKind::ScrollDown => current.next(false),
        MouseEventKind::ScrollUp => current.next(true),
        MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
            let area = language_area(bounds);
            if !area.contains((event.column, event.row).into()) {
                close_language(menu, keys);
            } else if area.height >= 4 && event.row >= area.y + 2 && event.row < area.bottom() - 1 {
                let visible = area.height as usize - 3;
                let first = current.selected.saturating_sub(visible - 1);
                current.selected = first + (event.row - area.y - 2) as usize;
                accept_language(menu, host, view, keys);
            }
        }
        _ => {}
    }
}

/// Close only for the successful completion of the requested revision.
/// A prompt, failed request, failed write or concurrent edit never exits.
#[derive(Default)]
#[cfg(test)]
struct SaveFlow {
    closing: bool,
    path: Option<String>,
    revision: Option<u64>,
    refusal: Option<kwil_view_model::SaveCompletionRefusal>,
}

#[cfg(test)]
impl SaveFlow {
    fn request(
        &mut self,
        view: &mut View,
        path: Option<std::path::PathBuf>,
        quit: bool,
    ) -> Option<kwil_core_workspace::SaveRequest> {
        self.closing |= quit;
        let Some(editor) = view.editor_mut() else {
            self.closing = false;
            view.set_notice("Indexing: editing will be available when ready".into());
            return None;
        };
        if path.is_none() && editor.document().path().is_none() {
            self.path = Some(String::new());
            view.set_notice("Save as:  (Enter a path, Enter to save, Esc to cancel)".into());
            return None;
        }
        match editor.save_request(path) {
            Ok(request) => {
                self.revision = Some(request.revision);
                view.set_notice("Saving…".into());
                Some(request)
            }
            Err(error) => {
                self.closing = false;
                view.set_notice(format!("Save failed: {error}"));
                None
            }
        }
    }

    fn finish(&mut self, view: &mut View, saved: std::io::Result<kwil_core_workspace::SavedSnapshot>) -> bool {
        let revision = saved.as_ref().ok().map(|saved| saved.revision);
        let requested = self.revision.take();
        let closing = std::mem::take(&mut self.closing);
        let Some(editor) = view.editor_mut() else { return false };
        // A successful worker trim can install a new, clean revision during
        // finish_save. Compare the source revision before that installation.
        let current_revision = revision == Some(editor.document().revision());
        match editor.finish_save(saved) {
            ControlFlow::Continue(Ok(result)) => {
                let clean_revision = current_revision && !editor.document().is_dirty();
                let notice = match result.status {
                    CommandStatus::Message(message) | CommandStatus::NotImplemented(message) => message,
                    _ if !clean_revision => "Saved earlier revision; current changes remain unsaved".into(),
                    _ => "Saved".into(),
                };
                view.refresh_after_edit(result.damage);
                view.set_notice(notice);
                closing && requested.is_some() && revision == requested && clean_revision
            }
            ControlFlow::Continue(Err(error)) => {
                view.set_notice(format!("Save failed: {error}"));
                false
            }
            ControlFlow::Break(refusal) => {
                let notice = format!("Save completion retained: {:?}", refusal.reason);
                self.refusal = Some(refusal);
                view.set_notice(notice);
                false
            }
        }
    }
}

#[cfg(test)]
fn path_key(view: &mut View, prompt: &mut Option<String>, key: KeyEvent) -> KeyResult {
    let Some(path) = prompt.as_mut() else {
        return KeyResult::Changed(false);
    };
    match key.code {
        KeyCode::Esc => {
            *prompt = None;
            view.set_notice(String::new());
            return KeyResult::Cancel;
        }
        KeyCode::Enter if !path.trim().is_empty() => {
            return KeyResult::Save(prompt.take().map(Into::into));
        }
        KeyCode::Backspace => {
            path.pop();
        }
        KeyCode::Char(c)
            if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER) =>
        {
            path.push(c);
        }
        _ => {}
    }
    view.set_notice(format!("Save as: {path}  (Enter to save, Esc to cancel)"));
    KeyResult::Changed(true)
}

fn key(
    view: &mut View,
    k: KeyEvent,
    elapsed: std::time::Duration,
    keys: &mut KeymapEngine,
    preset: Preset,
) -> KeyResult {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    if let Some(chord) = chord(k) {
        match keys.feed_with_selection(chord) {
            VimResult::Pending => return KeyResult::Changed(false),
            VimResult::Command(call) => {
                return invoke(view, call, k.modifiers.contains(KeyModifiers::SHIFT), elapsed, preset);
            }
            VimResult::Error(error) => {
                view.set_notice(error);
                return KeyResult::Changed(true);
            }
            VimResult::Unbound => {
                if keys.suppresses_fallback() {
                    return KeyResult::Changed(false);
                }
                if keys.mode() != kwil_core_commands::Mode::Insert {
                    return KeyResult::Changed(false);
                }
                if ctrl && !matches!(k.code, KeyCode::Char('q' | 'Q')) {
                    return KeyResult::Changed(false);
                }
            }
        }
    }
    let command = match k.code {
        KeyCode::Char('q' | 'Q') if ctrl => return KeyResult::Quit,
        KeyCode::Char('s' | 'S') if ctrl => return KeyResult::Save(None),
        KeyCode::Esc => return KeyResult::Cancel,
        KeyCode::Char('z' | 'Z') if ctrl && k.modifiers.contains(KeyModifiers::SHIFT) => "edit.redo",
        KeyCode::Char('z') if ctrl => "edit.undo",
        KeyCode::Char('y') if ctrl => "edit.redo",
        KeyCode::Char('a') if ctrl => "edit.selectAll",
        KeyCode::Down => "motion.down",
        KeyCode::Up => "motion.up",
        KeyCode::Left if ctrl => "motion.wordBackward",
        KeyCode::Right if ctrl => "motion.wordForward",
        KeyCode::Left => "motion.left",
        KeyCode::Right => "motion.right",
        KeyCode::PageDown => "motion.pageDown",
        KeyCode::PageUp => "motion.pageUp",
        KeyCode::End if ctrl => "motion.lastLine",
        KeyCode::Home if ctrl => "motion.firstLine",
        KeyCode::End => "motion.lineEnd",
        KeyCode::Home => "motion.smartHome",
        KeyCode::Enter => "edit.newline",
        KeyCode::Backspace if ctrl => "edit.deleteWordBackward",
        KeyCode::Delete if ctrl => "edit.deleteWordForward",
        KeyCode::Backspace => "edit.backspace",
        KeyCode::Delete => "edit.delete",
        KeyCode::Tab => "edit.indent",
        KeyCode::BackTab => "edit.outdent",
        KeyCode::Char(c) if !ctrl && !k.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::SUPER) => {
            return body_execute(view, "edit.insert", Some(c.to_string()), false, elapsed);
        }
        _ => return KeyResult::Changed(false),
    };
    body_execute(view, command, None, k.modifiers.contains(KeyModifiers::SHIFT), elapsed)
}

fn prepared_body(view: &View) -> bool {
    view.editor().is_some_and(|editor| editor.settings.editor.word_wrap)
}
fn body_execute(
    view: &mut View,
    id: &'static str,
    text: Option<String>,
    extend: bool,
    at: std::time::Duration,
) -> KeyResult {
    if prepared_body(view) {
        KeyResult::BodyExecute(id, CommandArgs { text, extend, at, ..Default::default() }, 1)
    } else {
        KeyResult::Changed(execute(view, id, text, extend, at))
    }
}

fn invoke(
    view: &mut impl TuiTarget,
    call: kwil_core_commands::vim::Invocation,
    extend: bool,
    elapsed: std::time::Duration,
    preset: Preset,
) -> KeyResult {
    if view.raw_body() && !hex_ui::chrome_command(call.id) {
        return KeyResult::Hex(call, extend);
    }
    let body_blocked = view.body_blocked();
    let view = view.text_view_mut();
    if call.id == "file.setType" {
        return KeyResult::Language;
    }
    if search_ui::is_search(call.id) {
        return KeyResult::Search(call);
    }
    if call.id == "view.settings" {
        return KeyResult::Settings;
    }
    if call.id == "view.settingsFile" {
        return KeyResult::SettingsFile;
    }
    if let Some(query) = palette_ui::command_query(call.id) {
        return KeyResult::Palette(query);
    }
    if call.id == "view.retryWrap" {
        return KeyResult::RetryWrap;
    }
    if matches!(call.id, "view.zoomIn" | "view.zoomOut" | "view.zoomReset") {
        view.set_notice("Terminal font size is controlled by your terminal; use its zoom shortcuts".into());
        return KeyResult::Changed(true);
    }
    if call.id == "file.save" {
        return KeyResult::Save(None);
    }
    if call.id == "file.quit" {
        return KeyResult::Quit;
    }
    if call.id == "file.saveAndQuit" {
        return KeyResult::SaveAndQuit;
    }
    if matches!(
        call.id,
        "file.new"
            | "file.open"
            | "file.openHex"
            | "view.toggleHex"
            | "file.saveAs"
            | "file.saveAll"
            | "file.close"
            | "file.closeAll"
            | "file.reopenClosed"
            | "file.recent"
            | "file.nextTab"
            | "file.previousTab"
            | "file.restoreSession"
    ) {
        return KeyResult::File(call.id);
    }
    if body_blocked || prepared_body(view) {
        return KeyResult::BodyInvoke(call, extend);
    }
    invoke_body(view, call, extend, elapsed, preset)
}

fn invoke_body(
    view: &mut View,
    call: kwil_core_commands::vim::Invocation,
    extend: bool,
    elapsed: std::time::Duration,
    preset: Preset,
) -> KeyResult {
    if preset == Preset::Vim {
        if let Some(editor) = view.editor_mut() {
            match editor.execute_vim(&call) {
                Ok(result) => {
                    view.refresh_after_edit(result.damage);
                    match result.status {
                        CommandStatus::Message(message) | CommandStatus::NotImplemented(message) => {
                            view.set_notice(message);
                        }
                        CommandStatus::IndexNotReady => {
                            view.set_notice("Indexing: editing will be available when ready".into());
                        }
                        CommandStatus::SaveRequested => return KeyResult::Save(None),
                        CommandStatus::Applied => {}
                    }
                }
                Err(error) => {
                    view.set_notice(error.to_string());
                }
            }
        } else {
            view.set_notice("Indexing: editing will be available when ready".into());
        }
        return KeyResult::Changed(true);
    }
    KeyResult::Changed(execute(view, call.id, None, extend, elapsed))
}

fn chord(event: KeyEvent) -> Option<kwil_core_commands::KeyChord> {
    use kwil_core_commands::{Key, KeyChord, Modifiers, NamedKey as N};
    let key = match event.code {
        KeyCode::Char(c) => Key::Character(c),
        KeyCode::F(n) => Key::Function(n),
        KeyCode::Enter => Key::Named(N::Enter),
        KeyCode::Esc => Key::Named(N::Escape),
        KeyCode::Tab | KeyCode::BackTab => Key::Named(N::Tab),
        KeyCode::Backspace => Key::Named(N::Backspace),
        KeyCode::Delete => Key::Named(N::Delete),
        KeyCode::Insert => Key::Named(N::Insert),
        KeyCode::Home => Key::Named(N::Home),
        KeyCode::End => Key::Named(N::End),
        KeyCode::PageUp => Key::Named(N::PageUp),
        KeyCode::PageDown => Key::Named(N::PageDown),
        KeyCode::Left => Key::Named(N::Left),
        KeyCode::Right => Key::Named(N::Right),
        KeyCode::Up => Key::Named(N::Up),
        KeyCode::Down => Key::Named(N::Down),
        _ => return None,
    };
    Some(KeyChord::new(
        key,
        Modifiers {
            ctrl: event.modifiers.contains(KeyModifiers::CONTROL),
            alt: event.modifiers.contains(KeyModifiers::ALT),
            shift: event.modifiers.contains(KeyModifiers::SHIFT) || event.code == KeyCode::BackTab,
            super_key: event.modifiers.contains(KeyModifiers::SUPER),
        },
    ))
}

fn execute(view: &mut View, id: &str, text: Option<String>, extend: bool, at: std::time::Duration) -> bool {
    match view.execute(id, &CommandArgs { text, extend, at, ..Default::default() }, 1) {
        Ok(result) => match result.status {
            CommandStatus::Message(message) | CommandStatus::NotImplemented(message) => {
                view.set_notice(message);
            }
            CommandStatus::IndexNotReady => {
                view.set_notice("Indexing: editing will be available when ready".into());
            }
            _ => {}
        },
        Err(error) => {
            view.set_notice(error.to_string());
        }
    }
    true
}

fn wrap_body_rect(view: &Workspace, width: u16, height: u16, layout: kwil_core_commands::settings::TabLayout) -> Rect {
    let body = tabs::geometry(view, width, height, layout).body;
    Rect::new(body.x as u16, body.y as u16, body.width as u16, body.height as u16)
}
fn wrap_modal(
    view: &Workspace,
    search: &search_ui::SearchHost,
    palette: Option<&kwil_view_model::palette_session::PaletteSession>,
    settings: Option<&kwil_view_model::settings_session::SettingsSession>,
    language: bool,
) -> bool {
    binary_offer::Host::blocked(view)
        || view.file_prompt().is_some()
        || view.close_pending()
        || language
        || palette.is_some_and(|host| host.model().is_open())
        || settings.is_some_and(|host| host.is_open())
        || search.is_open()
}
fn resolve_bounded_wrap(
    document: &mut DocumentLane,
    view: &mut Workspace,
    runtime: Option<&mut wrap_ui::Runtime>,
    intent: KeyResult,
    at: Duration,
    preset: Preset,
) -> KeyResult {
    if matches!(&intent, KeyResult::RetryWrap) && (document.wrap_failed() || document.retry.is_some()) {
        match document.retry_wrap(Duration::from_secs(35)) {
            Ok(()) => {
                view.set_notice("Wrap retry requested on the existing document worker".into());
            }
            Err(error) => {
                view.set_notice(error);
            }
        }
        return KeyResult::Changed(true);
    }
    resolve_wrap(view, runtime, intent, at, preset)
}
fn resolve_wrap(
    view: &mut Workspace,
    runtime: Option<&mut wrap_ui::Runtime>,
    intent: KeyResult,
    at: std::time::Duration,
    preset: Preset,
) -> KeyResult {
    if matches!(&intent, KeyResult::RetryWrap) {
        let result = if view.wrap_settlement_state()
            == kwil_view_model::workspace_wrap::WorkspaceWrapSettlementState::Failed
        {
            view.retry_wrap_settlement(std::time::Duration::from_secs(35))
        } else {
            runtime.ok_or(kwil_view_model::visual_rows::WrapError::Closed).and_then(|runtime| runtime.retry_relative())
        };
        match result {
            Ok(()) => {
                view.set_notice("Wrap retry requested".into());
            }
            Err(error) => {
                view.set_notice(format!("Wrap retry: {error}"));
            }
        }
        return KeyResult::Changed(true);
    }
    if !matches!(&intent, KeyResult::BodyInvoke(_, _) | KeyResult::BodyExecute(_, _, _)) {
        return intent;
    }
    let Some(runtime) = runtime else {
        view.set_notice("Prepared text is starting; body input was not applied".into());
        return KeyResult::Changed(true);
    };
    if runtime.closed() || view.wrap_input_blocked() {
        view.set_notice("Wrapped body is settling or failed; input was not applied".into());
        return KeyResult::Changed(true);
    }
    let (id, count, extend, standalone) = match &intent {
        KeyResult::BodyInvoke(call, extend) => {
            if let Some(motion) = call.visual_motion() {
                let id = match motion {
                    kwil_core_commands::vim::Motion::Up => "motion.up",
                    kwil_core_commands::vim::Motion::Down => "motion.down",
                    _ => unreachable!("visual motion accessor accepts only gj/gk"),
                };
                let captured = match &call.argument {
                    kwil_core_commands::vim::Argument::VisualMotion { extend } => *extend,
                    _ => unreachable!("visual accessor validates its argument"),
                };
                (id, call.count, captured, true)
            } else {
                (call.id, call.count, *extend, preset != Preset::Vim && call.motion.is_none())
            }
        }
        KeyResult::BodyExecute(id, args, count) => (*id, *count, args.extend, true),
        _ => unreachable!(),
    };
    if standalone {
        match runtime.command(id, count, extend) {
            wrap_ui::Input::Pass => {}
            input => {
                if let Some(message) = input.notice() {
                    view.set_notice(message.into());
                }
                return KeyResult::Changed(true);
            }
        }
    }
    if let Some(message) = wrap_ui::mutation_notice(runtime.gate()) {
        view.set_notice(message.into());
        return KeyResult::Changed(true);
    }
    match intent {
        KeyResult::BodyInvoke(call, extend) => invoke_body(view, call, extend, at, preset),
        KeyResult::BodyExecute(id, args, count) => {
            match view.execute(id, &args, count) {
                Ok(result) => match result.status {
                    CommandStatus::Message(message) | CommandStatus::NotImplemented(message) => {
                        view.set_notice(message);
                    }
                    CommandStatus::IndexNotReady => {
                        view.set_notice("Indexing: editing will be available when ready".into());
                    }
                    _ => {}
                },
                Err(error) => {
                    view.set_notice(error.to_string());
                }
            }
            KeyResult::Changed(true)
        }
        _ => unreachable!(),
    }
}
#[cfg(test)]
fn draw(f: &mut ratatui::Frame, view: &View) {
    let area = f.area();
    draw_area(f, view, area, true);
}

#[cfg(test)]
fn draw_area(f: &mut ratatui::Frame, view: &View, area: Rect, status: bool) {
    draw_area_highlighted(f, view, area, status, &[]);
}

fn draw_area_highlighted(
    f: &mut ratatui::Frame,
    target: &impl TuiTarget,
    area: Rect,
    status: bool,
    highlights: &[&kwil_view_model::highlighting::Span],
) {
    if target.raw_body() {
        f.render_widget(Paragraph::new(""), area);
        return;
    }
    let view = target.text_view();
    let gutter_style = Style::default().fg(Color::Rgb(0x5b, 0x6b, 0x85));
    for row in 0..view.rows().min(area.height.saturating_sub(u16::from(status)) as usize) {
        let y = area.y + row as u16;
        let Some(text) = view.line(row) else { continue };
        let tab = view.editor().map_or(4, |editor| editor.settings.editor.tab_size as usize);
        let cells = kwil_view_model::grid::cells(text, area.width.saturating_sub(8) as usize, tab);
        let source = view
            .editor()
            .and_then(|editor| editor.line_range(view.top() + row as u64).ok())
            .map_or(0, |range| range.start)
            + view.line_offset(row);
        let selections =
            view.editor().and_then(|editor| editor.selection_ranges(view.top() + row as u64).ok()).unwrap_or_default();
        let mut spans = vec![Span::styled(format!("{:>7} ", view.line_number(row)), gutter_style)];
        for cell in cells {
            let selected = selections.iter().any(|range| {
                range.start < source + cell.bytes.end as u64 && source + (cell.bytes.start as u64) < range.end
            });
            let mut style = if selected { Style::default().bg(Color::Rgb(0x30, 0x58, 0x80)) } else { Style::default() };
            let offset = source as usize + cell.bytes.start;
            let index = highlights.partition_point(|span| span.range.end <= offset);
            if let Some(span) = highlights.get(index).filter(|span| span.range.start <= offset) {
                let rgb = kwil_view_model::highlight_session::syntax_rgb(span.class);
                style = style.fg(Color::Rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8));
            }
            spans.push(Span::styled(if cell.text == "\t" { " ".repeat(cell.width) } else { cell.text }, style));
        }
        let line = Line::from(spans);
        f.render_widget(Paragraph::new(line), Rect { x: area.x, y, width: area.width, height: 1 });
    }
    if status {
        let status = Paragraph::new(view.status_text())
            .style(Style::default().fg(Color::Rgb(0x8f, 0xe3, 0xff)).bg(Color::Rgb(0x08, 0x0d, 0x18)));
        f.render_widget(
            status,
            Rect { x: area.x, y: area.y + area.height.saturating_sub(1), width: area.width, height: 1 },
        );
    }
    if let Some(editor) = view.editor()
        && let Ok((row, byte)) = editor.byte_position(editor.primary_cursor().head)
        && row >= view.top()
        && row - view.top() < view.rows() as u64
    {
        let screen_row = (row - view.top()) as usize;
        let first = view.line_offset(screen_row);
        let cells = kwil_view_model::grid::cells(
            view.line(screen_row).unwrap_or_default(),
            area.width.saturating_sub(8) as usize,
            editor.settings.editor.tab_size as usize,
        );
        if byte >= first {
            let column = cells
                .iter()
                .find(|cell| byte - first < cell.bytes.end as u64)
                .map_or_else(|| cells.last().map_or(0, |cell| cell.column + cell.width), |cell| cell.column);
            if column + 8 < (area.width as usize) {
                f.set_cursor_position((area.x + 8 + column as u16, area.y + screen_row as u16));
            }
        }
    }
}

fn keep_visible(view: &mut View, horizontal: &mut u64, columns: usize, tab: usize) {
    let Some(editor) = view.editor() else {
        return;
    };
    let Ok((row, byte)) = editor.byte_position(editor.primary_cursor().head) else {
        return;
    };
    if row < view.top() || row - view.top() >= view.rows() as u64 {
        return;
    }
    let screen_row = (row - view.top()) as usize;
    let first = view.line_offset(screen_row);
    let cells = kwil_view_model::grid::cells(view.line(screen_row).unwrap_or_default(), columns, tab);
    let end = cells.last().map_or(0, |cell| cell.bytes.end as u64);
    let end_column = cells.last().map_or(0, |cell| cell.column + cell.width);
    if byte < first || byte > first + end || (byte == first + end && end_column >= columns) {
        *horizontal = byte;
        view.set_horizontal_window(byte, 64 << 10);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn shutdown_highlights(host: kwil_view_model::highlight_session::HighlightSession) {
        if let ControlFlow::Break(error) = host.shutdown() {
            panic!("highlight shutdown refused: {error:?}");
        }
    }
    use std::time::Duration;

    fn retire_unstarted_control(workspace: Workspace, budget: AllocationBudget) {
        let (ticket, budget) = document_ui::StartupOwner::new(workspace, budget).into_parts();
        let slot = std::sync::Mutex::new(ShutdownSlot {
            ticket: Some(ticket),
            startup_budget: Some(budget),
            ..Default::default()
        });
        let caller = std::thread::current().id();
        retry_shutdown_slot(&slot, Duration::from_secs(5), move |ticket, _| {
            assert_ne!(std::thread::current().id(), caller);
            drop(ticket.view.take());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn tui_start_refusal_preserves_original_editor_error_and_shared_budget() {
        let _process_guard = test_support::process_shutdown_guard();
        #[derive(Debug)]
        struct InitCause(std::sync::Arc<()>);
        impl std::fmt::Display for InitCause {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("original terminal init refusal")
            }
        }
        impl std::error::Error for InitCause {}
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        workspace
            .execute(
                "edit.insert",
                &CommandArgs { text: Some("original fallback owner".into()), ..Default::default() },
                1,
            )
            .unwrap();
        let document = workspace.active_id();
        let editor = workspace.editor().unwrap() as *const kwil_view_model::Editor;
        let input = workspace.editor_input_ticket(document).unwrap();
        let budget = AllocationBudget::default();
        let alias = budget.clone();
        let baseline = alias.retained_bytes();
        let marker = alias.reserve_metadata(257).unwrap();
        let marked = alias.retained_bytes();
        let cause = std::sync::Arc::new(());
        let original = cause.clone();
        let ControlFlow::Break(refusal) = run_workspace_with_terminal_init(
            workspace,
            budget,
            Box::new(|_| panic!("refused startup cannot publish first frame")),
            Box::new(|_| panic!("refused startup cannot publish external admission")),
            move |_touched| Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, InitCause(original))),
        ) else {
            panic!("pre-worker refusal must return its original inputs")
        };
        assert_eq!(refusal.workspace.active_id(), document);
        assert_eq!(refusal.workspace.editor().unwrap() as *const kwil_view_model::Editor, editor);
        assert!(refusal.workspace.editor_input_is_current(&input));
        assert_eq!(refusal.error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(std::sync::Arc::ptr_eq(
            &refusal.error.get_ref().unwrap().downcast_ref::<InitCause>().unwrap().0,
            &cause
        ));
        assert_eq!(refusal.budget.retained_bytes(), marked, "startup headers were returned, not quarantined");
        let second = alias.reserve_metadata(513).unwrap();
        assert!(refusal.budget.retained_bytes() > marked);
        assert_eq!(
            refusal.budget.retained_bytes(),
            alias.retained_bytes(),
            "same original counter, not a fallback budget"
        );
        drop(second);
        drop(marker);
        assert_eq!(refusal.budget.retained_bytes(), baseline);
        retire_unstarted_control(refusal.workspace, refusal.budget);
    }

    #[test]
    fn tui_start_header_admission_refusal_precedes_terminal_and_preserves_input() {
        let _process_guard = test_support::process_shutdown_guard();
        let workspace = Workspace::new(View::new(None)).unwrap();
        let document = workspace.active_id();
        let editor = workspace.editor().unwrap() as *const kwil_view_model::Editor;
        let budget = AllocationBudget::new(1);
        let alias = budget.clone();
        let before = alias.retained_bytes();
        let ControlFlow::Break(refusal) = run_workspace_with_terminal_init(
            workspace,
            budget,
            Box::new(|_| panic!("no first frame")),
            Box::new(|_| panic!("no external admission")),
            |_| panic!("header refusal must not touch a terminal or start a worker"),
        ) else {
            panic!("header admission must return exact input")
        };
        assert_eq!(refusal.workspace.active_id(), document);
        assert_eq!(refusal.workspace.editor().unwrap() as *const kwil_view_model::Editor, editor);
        assert!(refusal.error.get_ref().unwrap().downcast_ref::<kwil_view_model::visual_rows::WrapError>().is_some());
        assert_eq!(refusal.budget.retained_bytes(), before);
        assert_eq!(refusal.budget.retained_bytes(), alias.retained_bytes());
        retire_unstarted_control(refusal.workspace, refusal.budget);
    }

    #[test]
    fn tui_start_unsafe_retired_wrap_fence_returns_input_without_forcing_rearm() {
        let _process_guard = test_support::process_shutdown_guard();
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        let editor = workspace.editor().unwrap() as *const kwil_view_model::Editor;
        workspace.begin_wrap_shutdown();
        assert!(!workspace.unstarted_frontend_ready());
        let ControlFlow::Break(refusal) = run_workspace_with_terminal_init(
            workspace,
            AllocationBudget::default(),
            Box::new(|_| panic!("no first frame")),
            Box::new(|_| panic!("no external admission")),
            |_| panic!("unsafe owner cannot initialize a terminal"),
        ) else {
            panic!("unsafe wrap state must remain pre-worker refusal")
        };
        assert_eq!(refusal.error.kind(), std::io::ErrorKind::WouldBlock);
        assert_eq!(refusal.workspace.editor().unwrap() as *const kwil_view_model::Editor, editor);
        assert!(!refusal.workspace.unstarted_frontend_ready(), "TUI must not clear the old accepted-exit fence");
        retire_unstarted_control(refusal.workspace, refusal.budget);
    }

    #[test]
    fn tui_startup_guard_transfers_original_owner_once_to_existing_cleanup_lane() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let workspace = Workspace::new(View::new(None)).unwrap();
        let document = workspace.active_id();
        let editor = workspace.editor().unwrap() as *const kwil_view_model::Editor as usize;
        let budget = AllocationBudget::default();
        let alias = budget.clone();
        let original = document_ui::StartupOwner::new(workspace, budget);
        assert!(!original.worker_admitted());
        let (ticket, budget) = original.into_parts();
        let slot = std::sync::Mutex::new(ShutdownSlot {
            ticket: Some(ticket),
            startup_budget: Some(budget),
            ..Default::default()
        });
        let marker = alias.reserve_metadata(191).unwrap();
        assert_eq!(slot.lock().unwrap().startup_budget.as_ref().unwrap().retained_bytes(), alias.retained_bytes());
        let calls = Arc::new(AtomicUsize::new(0));
        let transferred = calls.clone();
        let caller = std::thread::current().id();
        retry_shutdown_slot(&slot, Duration::from_secs(5), move |ticket, _| {
            assert_ne!(std::thread::current().id(), caller);
            assert_eq!(transferred.fetch_add(1, Ordering::AcqRel), 0);
            assert_eq!(ticket.view.as_ref().unwrap().active_id(), document);
            assert_eq!(
                ticket.view.as_ref().unwrap().editor().unwrap() as *const kwil_view_model::Editor as usize,
                editor
            );
            drop(ticket.view.take());
            Ok(())
        })
        .unwrap();
        assert_eq!(calls.load(Ordering::Acquire), 1);
        let state = slot.lock().unwrap();
        assert!(state.ticket.is_none());
        assert!(state.startup_budget.is_none());
        drop(marker);
    }

    #[test]
    fn cleanup_launch_refusal_retains_exact_source_and_error_before_borrowed_off_ui_retry() {
        #[derive(Debug)]
        struct LaunchCause(std::sync::Arc<()>);
        impl std::fmt::Display for LaunchCause {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("controlled cleanup launch refusal")
            }
        }
        impl std::error::Error for LaunchCause {}
        let mut view = Workspace::new(View::new(None)).unwrap();
        view.execute(
            "edit.insert",
            &CommandArgs { text: Some("exact retained editor".into()), ..Default::default() },
            1,
        )
        .unwrap();
        let document = view.active_id();
        let revision = view.editor().unwrap().document().revision();
        let slot = std::sync::Mutex::new(ShutdownSlot {
            ticket: Some(ShutdownTicket {
                document: None,
                highlighting: None,
                search_binding: None,
                search_release_refusal: None,
                terminal_panic: None,
                wrap: None,
                view: Some(view),
                search: None,
                worker: None,
                rx: None,
                stopped: None,
                document_started: false,
                terminal_failure: None,
            }),
            ..Default::default()
        });
        // Capture a stable address AFTER the owner enters its retained slot.
        let owner_in_slot = slot.lock().unwrap().ticket.as_ref().unwrap().view.as_ref().unwrap().editor().unwrap()
            as *const kwil_view_model::Editor;
        let cause = std::sync::Arc::new(());
        let error = retry_shutdown_slot_with_launch(
            &slot,
            Duration::from_secs(5),
            |_, _| panic!("a refused launch cannot run cleanup"),
            ShutdownLaunch::Refuse(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                LaunchCause(cause.clone()),
            )),
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(std::sync::Arc::ptr_eq(&error.get_ref().unwrap().downcast_ref::<LaunchCause>().unwrap().0, &cause));
        {
            let state = slot.lock().unwrap();
            assert!(!state.retrying);
            let view = state.ticket.as_ref().unwrap().view.as_ref().unwrap();
            assert_eq!(view.active_id(), document);
            assert_eq!(view.editor().unwrap().document().revision(), revision);
            assert_eq!(view.editor().unwrap() as *const kwil_view_model::Editor, owner_in_slot);
        }
        let caller = std::thread::current().id();
        retry_shutdown_slot(&slot, Duration::from_secs(5), move |ticket, _| {
            assert_ne!(std::thread::current().id(), caller, "source destruction belongs to the existing cleanup lane");
            drop(ticket.view.take());
            Ok(())
        })
        .unwrap();
        let state = slot.lock().unwrap();
        assert!(state.ticket.is_none());
        assert!(!state.retrying);
    }

    #[test]
    fn shutdown_deadline_retains_exact_ticket_refuses_new_run_and_explicit_retry_drains() {
        let _process_guard = test_support::process_shutdown_guard();
        ensure_shutdown_idle().unwrap();
        let (sender, stopped) = mpsc::channel();
        let mut ticket = ShutdownTicket {
            document: None,
            highlighting: None,
            search_binding: None,
            search_release_refusal: None,
            terminal_panic: None,
            wrap: None,
            view: None,
            search: None,
            worker: None,
            rx: None,
            stopped: Some(stopped),
            document_started: true,
            terminal_failure: None,
        };
        // A held fake receipt and zero budget reproduce the failed phase
        // without sleeping, launching a terminal or waiting a real timeout.
        let message = finish_shutdown(&mut ticket, Duration::ZERO).unwrap_err();
        assert!(ticket.document_started);
        let failure = retain_shutdown(ticket, message);
        assert!(failure.to_string().contains("owner retained"));
        assert!(ensure_shutdown_idle().is_err(), "another run cannot discard a prior failed owner");
        sender.send(Ok(())).unwrap();
        retry_pending_shutdown(Duration::from_millis(100)).unwrap();
        ensure_shutdown_idle().unwrap();
    }

    #[test]
    fn disposal_drain_zero_full_and_disconnected_controls_retain_exact_editor_and_host() {
        use std::cell::Cell;
        for control in ["zero", "full", "disconnected"] {
            let mut view = Workspace::new(View::new(None)).unwrap();
            view.execute("edit.insert", &CommandArgs { text: Some("host document".into()), ..Default::default() }, 1)
                .unwrap();
            let document = view.active_id();
            let revision = view.editor().unwrap().document().revision();
            let mut search = search_ui::SearchHost::default();
            let mut keys = KeymapEngine::new(Preset::VsCode, &[]).unwrap();
            let capacity = usize::from(control != "zero");
            let (admission, requests) = mpsc::sync_channel::<Box<kwil_view_model::Editor>>(capacity);
            let requests = if control == "disconnected" {
                drop(requests);
                None
            } else {
                Some(requests)
            };
            if control == "full" {
                assert!(
                    admission
                        .try_send(Box::new(kwil_view_model::Editor::new(kwil_view_model::Document::new())))
                        .is_ok()
                );
            }
            let mut editor = kwil_view_model::Editor::new(kwil_view_model::Document::new());
            editor.replace_byte_range(0..0, b"exact rejected editor").unwrap();
            let mut owner = Some(Box::new(editor));
            let identity = (&**owner.as_ref().unwrap()) as *const kwil_view_model::Editor;
            let (events, receiver) = mpsc::channel();
            if control == "disconnected" {
                drop(events);
            } else {
                events.send(Msg::InputClosed).unwrap();
            }
            let waits = Cell::new(0);
            let refusals = Cell::new(0);
            let remaining = || {
                let count = waits.get();
                waits.set(count + 1);
                if control == "zero" || count != 0 { Duration::ZERO } else { Duration::from_secs(1) }
            };
            let mut terminal_failure = None;
            let error = drain_workspace(
                &mut view,
                &mut search,
                &receiver,
                &mut keys,
                |_| {
                    let rejected = match admission.try_send(owner.take().expect("retained admission owner")) {
                        Ok(()) => panic!("controlled admission must refuse"),
                        Err(mpsc::TrySendError::Full(owner)) => {
                            assert_ne!(control, "disconnected");
                            owner
                        }
                        Err(mpsc::TrySendError::Disconnected(owner)) => {
                            assert_eq!(control, "disconnected");
                            owner
                        }
                    };
                    refusals.set(refusals.get() + 1);
                    owner = Some(rejected);
                    false
                },
                &remaining,
                &mut terminal_failure,
            )
            .unwrap_err();
            assert!(error.contains(if control == "disconnected" { "disconnected" } else { "deadline" }));
            assert_eq!((&**owner.as_ref().unwrap()) as *const kwil_view_model::Editor, identity);
            let mut bytes = Vec::new();
            owner.as_ref().unwrap().document().snapshot().write_to(&mut bytes).unwrap();
            assert_eq!(bytes, b"exact rejected editor");
            assert_eq!(view.active_id(), document);
            assert_eq!(view.editor().unwrap().document().revision(), revision);
            assert!(refusals.get() >= 1);
            assert!(terminal_failure.is_none());
            if control == "full" {
                assert_eq!(refusals.get(), 2, "InputClosed must neither lose the owner nor start an unbounded recv");
            }
            drop(requests);
        }
    }

    #[test]
    fn failed_or_missing_terminal_acknowledgement_never_becomes_success_on_retry() {
        for disconnected in [false, true] {
            let (sender, stopped) = mpsc::channel();
            let sender = if disconnected {
                drop(sender);
                None
            } else {
                sender.send(Err("controlled Stopped failure".to_owned())).unwrap();
                Some(sender)
            };
            let mut ticket = ShutdownTicket {
                document: None,
                highlighting: None,
                search_binding: None,
                search_release_refusal: None,
                terminal_panic: None,
                wrap: None,
                view: None,
                search: None,
                worker: None,
                rx: None,
                stopped: Some(stopped),
                document_started: true,
                terminal_failure: None,
            };
            let first = finish_shutdown(&mut ticket, Duration::ZERO).unwrap_err();
            assert!(ticket.document_started, "only Stopped(Ok) can acknowledge this phase");
            assert_eq!(ticket.terminal_failure.as_deref(), Some(first.as_str()));
            if let Some(sender) = sender {
                sender.send(Ok(())).unwrap();
            }
            let slot = std::sync::Mutex::new(ShutdownSlot { ticket: Some(ticket), ..Default::default() });
            for _ in 0..2 {
                let error = retry_shutdown_slot(&slot, Duration::ZERO, finish_shutdown).unwrap_err();
                assert!(error.to_string().contains(first.as_str()));
                let state = slot.lock().unwrap();
                assert!(!state.retrying);
                let retained = state.ticket.as_ref().unwrap();
                assert!(retained.document_started);
                assert_eq!(retained.terminal_failure.as_deref(), Some(first.as_str()));
            }
        }
    }

    #[cfg(panic = "unwind")]
    #[test]
    fn controlled_retry_unwind_keeps_borrowed_ticket_and_releases_retry_slot() {
        let (_sender, stopped) = mpsc::channel();
        let view = Workspace::new(View::new(None)).unwrap();
        let document = view.active_id();
        let ticket = ShutdownTicket {
            document: None,
            highlighting: None,
            search_binding: None,
            search_release_refusal: None,
            terminal_panic: None,
            wrap: None,
            view: Some(view),
            search: Some(Default::default()),
            worker: None,
            rx: None,
            stopped: Some(stopped),
            document_started: true,
            terminal_failure: None,
        };
        let slot = std::sync::Mutex::new(ShutdownSlot { ticket: Some(ticket), ..Default::default() });
        let error = retry_shutdown_slot(&slot, Duration::ZERO, |_, _| {
            panic!("controlled test-only unwind before owner transfer")
        })
        .unwrap_err();
        assert!(error.to_string().contains("panicked"));
        {
            let state = slot.lock().unwrap();
            assert!(!state.retrying);
            assert_eq!(state.ticket.as_ref().unwrap().view.as_ref().unwrap().active_id(), document);
            assert!(state.ticket.as_ref().unwrap().terminal_failure.is_some());
            assert_eq!(
                state.ticket.as_ref().unwrap().terminal_panic.as_ref().unwrap().downcast_ref::<&'static str>(),
                Some(&"controlled test-only unwind before owner transfer")
            );
        }
        assert!(
            retry_shutdown_slot(&slot, Duration::ZERO, |_, _| {
                panic!("sticky failure must prevent another cleanup attempt")
            })
            .is_err()
        );
        let state = slot.lock().unwrap();
        assert!(!state.retrying);
        assert_eq!(state.ticket.as_ref().unwrap().view.as_ref().unwrap().active_id(), document);
    }

    #[test]
    fn empty_document_detection_only_worker_wake_redraws_language_status() {
        let view = Workspace::new(View::new(None)).unwrap();
        let (sender, receiver) = mpsc::channel();
        let mut host = kwil_view_model::highlight_session::HighlightSession::new(move || {
            let _ = sender.send(());
        })
        .unwrap();
        assert_eq!(host.language_status(&view).detection, None);
        prepare_highlighting_for_draw(&mut host, &view);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut redraw = false;
        while host.language_status(&view).detection.is_none() {
            receiver.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())).unwrap();
            redraw |= highlighting_ready(&mut host, &view);
        }
        assert_eq!(host.spans(&view).count(), 0);
        assert!(redraw, "detection-only wake must redraw status without hit-row damage");
        std::thread::spawn(move || {
            shutdown_highlights(host);
            drop(view);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn file_type_dispatch_picker_repaints_and_keeps_tab_choices_and_ticket_identity() {
        use kwil_view_model::highlighting::{Language, LanguageChoice};
        let mut view = Workspace::new(View::new(None)).unwrap();
        view.set_rows(2);
        view.execute("edit.insert", &CommandArgs { text: Some("fn main() {}".into()), ..Default::default() }, 1)
            .unwrap();
        let revision = view.editor().unwrap().document().revision();
        let first = view.active_id();
        let (tx, rx) = mpsc::channel();
        let mut host = kwil_view_model::highlight_session::HighlightSession::new(move || {
            let _ = tx.send(());
        })
        .unwrap();
        let mut keys = KeymapEngine::new(Preset::VsCode, &[binding("ctrl+l", "file.setType")]).unwrap();
        assert!(matches!(
            key(
                &mut view,
                KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL),
                Duration::ZERO,
                &mut keys,
                Preset::VsCode
            ),
            KeyResult::Language
        ));
        let mut menu = Some(LanguageMenu::new(&host, &view));
        for character in "rust".chars() {
            language_key(
                &mut menu,
                Some(&mut host),
                &mut view,
                &mut keys,
                KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
                Duration::ZERO,
                Preset::VsCode,
            );
        }
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 20)).unwrap();
        terminal.draw(|frame| draw_language(frame, menu.as_ref().unwrap())).unwrap();
        language_key(
            &mut menu,
            Some(&mut host),
            &mut view,
            &mut keys,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            Duration::ZERO,
            Preset::VsCode,
        );
        assert!(menu.is_none());
        assert_eq!(host.language_choice(first), LanguageChoice::Explicit(Language::Rust));
        assert_eq!(view.editor().unwrap().document().revision(), revision);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while host.detected_language(&view) != Some(Language::Rust) {
            rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())).unwrap();
            host.worker_ready(&view);
        }
        assert!(host.spans(&view).next().is_some(), "picker acceptance reissues actual highlighting without edits");
        let old = LanguageMenu::new(&host, &view);
        view.file_command("file.new").unwrap();
        let second = view.active_id();
        menu = Some(old);
        accept_language(&mut menu, Some(&mut host), &mut view, &mut keys);
        assert_eq!(
            host.language_choice(second),
            LanguageChoice::Automatic,
            "stale ticket never applies to another tab"
        );
        assert_eq!(host.language_choice(first), LanguageChoice::Explicit(Language::Rust));
        view.activate(first);
        menu = Some(LanguageMenu::new(&host, &view));
        menu.as_mut().unwrap().text(&mut view, "automatic");
        accept_language(&mut menu, Some(&mut host), &mut view, &mut keys);
        assert_eq!(host.language_choice(first), LanguageChoice::Automatic);
        assert_eq!(view.editor().unwrap().document().revision(), revision, "automatic resets policy, never content");
        std::thread::spawn(move || {
            shutdown_highlights(host);
            drop(view);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn language_filter_tombstones_and_paste_limits_preserve_document() {
        let mut view = Workspace::new(View::new(None)).unwrap();
        let mut host = kwil_view_model::highlight_session::HighlightSession::new(|| {}).unwrap();
        let mut menu = Some(LanguageMenu::new(&host, &view));
        let mut keys = KeymapEngine::new(Preset::VsCode, &[binding("enter", "")]).unwrap();
        language_key(
            &mut menu,
            Some(&mut host),
            &mut view,
            &mut keys,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            Duration::ZERO,
            Preset::VsCode,
        );
        assert!(menu.is_some(), "explicit unbind suppresses raw Enter acceptance");
        menu.as_mut().unwrap().text(&mut view, "python");
        menu.as_mut().unwrap().text(&mut view, &"x".repeat(256));
        assert_eq!(menu.as_ref().unwrap().query, "python");
        assert!(view.editor().unwrap().document().snapshot().is_empty());
        std::thread::spawn(move || {
            shutdown_highlights(host);
            drop(view);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn trailing_space_edit_restores_unchanged_number_highlights() {
        let mut view = Workspace::new(View::new(None)).unwrap();
        view.set_rows(1);
        view.execute("edit.insert", &CommandArgs { text: Some("123".into()), ..Default::default() }, 1).unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let mut host = kwil_view_model::highlight_session::HighlightSession::new(move || {
            let _ = sender.send(());
        })
        .unwrap();
        let wait = |host: &mut kwil_view_model::highlight_session::HighlightSession, view: &Workspace| {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                receiver.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())).unwrap();
                let changed = host.worker_ready(view);
                if host.spans(view).next().is_some() {
                    break changed;
                }
            }
        };
        prepare_highlighting_for_draw(&mut host, &view);
        assert_eq!(wait(&mut host, &view), vec![0]);
        view.execute("edit.insert", &CommandArgs { text: Some(" ".into()), ..Default::default() }, 1).unwrap();
        prepare_highlighting_for_draw(&mut host, &view);
        assert_eq!(host.spans(&view).count(), 0);
        assert_eq!(wait(&mut host, &view), vec![0], "same Number span must repaint after the plain pending frame");
        let spans: Vec<_> = host.spans(&view).collect();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(24, 2)).unwrap();
        terminal.draw(|frame| draw_area_highlighted(frame, &view, frame.area(), false, &spans)).unwrap();
        assert_ne!(terminal.backend().buffer()[(8, 0)].fg, Color::Reset);
        host.stop();
    }

    #[test]
    fn syntax_foreground_preserves_selection_background_and_horizontal_bytes() {
        use kwil_view_model::highlighting::{Class, Span as SyntaxSpan};
        let mut view = View::new(None);
        view.set_rows(1);
        view.execute("edit.insert", &CommandArgs { text: Some("abc123".into()), ..Default::default() }, 1).unwrap();
        view.execute("edit.selectAll", &Default::default(), 1).unwrap();
        view.set_horizontal_window(3, 16);
        let span = SyntaxSpan { range: 3..6, class: Class::Number };
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(24, 2)).unwrap();
        terminal.draw(|frame| draw_area_highlighted(frame, &view, frame.area(), false, &[&span])).unwrap();
        let rgb = kwil_view_model::highlight_session::syntax_rgb(Class::Number);
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(8, 0)].symbol(), "1");
        assert_eq!(buffer[(8, 0)].fg, Color::Rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8));
        assert_eq!(buffer[(8, 0)].bg, Color::Rgb(0x30, 0x58, 0x80));
    }

    fn binding(keys: &str, command: &str) -> kwil_core_commands::Binding {
        kwil_core_commands::Binding { keys: keys.into(), command: command.into(), context: None, mode: None }
    }

    #[test]
    fn zoom_in_every_keymap_explains_terminal_font_ownership_without_editing() {
        for preset in [Preset::VsCode, Preset::Vim, Preset::NotepadPlusPlus] {
            let mut view = View::new(None);
            let mut keys = KeymapEngine::new(preset, &[]).unwrap();
            let event = KeyEvent::new(KeyCode::Char('='), KeyModifiers::CONTROL);
            assert!(matches!(key(&mut view, event, Default::default(), &mut keys, preset), KeyResult::Changed(true)));
            assert!(view.status_text().contains("controlled by your terminal"));
            assert!(view.editor().unwrap().document().snapshot().is_empty());
            let mut keys = KeymapEngine::new(preset, &[binding("ctrl+=", "")]).unwrap();
            assert!(matches!(key(&mut view, event, Default::default(), &mut keys, preset), KeyResult::Changed(false)));
        }
    }

    #[test]
    fn kw20_file_commands_are_dispatched_before_editor_fallback() {
        for command in [
            "file.new",
            "file.open",
            "file.saveAs",
            "file.saveAll",
            "file.close",
            "file.closeAll",
            "file.reopenClosed",
            "file.recent",
            "file.nextTab",
            "file.previousTab",
            "file.restoreSession",
        ] {
            let mut view = View::new(None);
            let mut keys = KeymapEngine::new(Preset::VsCode, &[binding("ctrl+g", command)]).unwrap();
            let result = key(
                &mut view,
                KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL),
                Duration::ZERO,
                &mut keys,
                Preset::VsCode,
            );
            assert!(!view.status_text().contains("command not implemented"), "file workflow must intercept {command}");
            assert!(matches!(result, KeyResult::File(id) if id == command));
        }
    }

    #[test]
    fn input_shift_left_tombstone_suppresses_selection_fallback() {
        let mut view = View::new(None);
        view.set_rows(3);
        execute(&mut view, "edit.insert", Some("abc".into()), false, Duration::ZERO);
        let mut keys = KeymapEngine::new(Preset::VsCode, &[binding("shift+left", ""), binding("ctrl+q", "")]).unwrap();
        key(&mut view, KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT), Duration::ZERO, &mut keys, Preset::VsCode);
        assert!(view.editor().unwrap().primary_cursor().is_caret(), "tombstone must suppress selection fallback");
        assert_eq!(view.editor().unwrap().primary_cursor().head, 3);
    }

    #[test]
    fn input_ctrl_q_tombstone_suppresses_quit_fallback() {
        let mut view = View::new(None);
        view.set_rows(3);
        let mut keys = KeymapEngine::new(Preset::VsCode, &[binding("ctrl+q", "")]).unwrap();
        assert!(
            matches!(
                key(
                    &mut view,
                    KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
                    Duration::ZERO,
                    &mut keys,
                    Preset::VsCode
                ),
                KeyResult::Changed(false)
            ),
            "tombstone must suppress quit fallback"
        );
    }

    #[test]
    fn vim_command_status_is_visible_in_test_backend() {
        let mut view = View::new(None);
        view.set_rows(3);
        let mut keys = KeymapEngine::new(Preset::Vim, &[binding("z", "file.openFolder")]).unwrap();
        key(&mut view, KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE), Duration::ZERO, &mut keys, Preset::Vim);
        assert!(view.status_text().contains("command not implemented: file.openFolder"));
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 4)).unwrap();
        terminal.draw(|frame| draw(frame, &view)).unwrap();
        let status: String = (0..80).map(|x| terminal.backend().buffer()[(x, 3)].symbol()).collect();
        assert!(status.contains("command not implemented: file.openFolder"));
        key(&mut view, KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE), Duration::ZERO, &mut keys, Preset::Vim);
        assert!(view.status_text().contains("register is empty"));
        for c in ":unknown".chars() {
            key(&mut view, KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), Duration::ZERO, &mut keys, Preset::Vim);
        }
        key(&mut view, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), Duration::ZERO, &mut keys, Preset::Vim);
        assert!(view.status_text().contains("unknown ex command"));
        let file = SaveFile::new();
        std::fs::write(&file.0, b"not indexed yet").unwrap();
        let file_source = kwil_core_workspace::open_for_editing(&file.0).unwrap();
        let mut view = View::new(Some((file.0.clone(), file_source)));
        assert!(view.editor().is_none());
        key(&mut view, KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE), Duration::ZERO, &mut keys, Preset::Vim);
        assert!(view.status_text().contains("Indexing: editing will be available when ready"));
    }

    #[test]
    fn input_preserves_exact_notepad_line_move_and_printable_fallback() {
        let mut view = View::new(None);
        view.set_rows(3);
        execute(&mut view, "edit.insert", Some("first\nsecond".into()), false, Duration::ZERO);
        let mut keys = KeymapEngine::new(Preset::NotepadPlusPlus, &[]).unwrap();
        key(
            &mut view,
            KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL | KeyModifiers::SHIFT),
            Duration::ZERO,
            &mut keys,
            Preset::NotepadPlusPlus,
        );
        assert_eq!(view.line(0), Some(b"second".as_slice()));
        assert_eq!(view.line(1), Some(b"first".as_slice()));
        assert!(view.editor().unwrap().primary_cursor().is_caret());
        key(
            &mut view,
            KeyEvent::new(KeyCode::Char('界'), KeyModifiers::NONE),
            Duration::ZERO,
            &mut keys,
            Preset::NotepadPlusPlus,
        );
        assert!(std::str::from_utf8(view.line(0).unwrap()).unwrap().contains('界'));
        let before = view.editor().unwrap().document().revision();
        key(
            &mut view,
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::SUPER),
            Duration::ZERO,
            &mut keys,
            Preset::NotepadPlusPlus,
        );
        assert_eq!(view.editor().unwrap().document().revision(), before, "Cmd is not Ctrl or printable text");
    }

    #[test]
    fn input_chord_tombstone_blocks_restart_but_invalid_continuation_types() {
        let mut view = View::new(None);
        view.set_rows(3);
        let mut keys =
            KeymapEngine::new(Preset::VsCode, &[binding("ctrl+k ctrl+q", ""), binding("ctrl+k x", "")]).unwrap();
        for last in [
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
        ] {
            key(
                &mut view,
                KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
                Duration::ZERO,
                &mut keys,
                Preset::VsCode,
            );
            assert!(matches!(
                key(&mut view, last, Duration::ZERO, &mut keys, Preset::VsCode),
                KeyResult::Changed(false)
            ));
        }
        assert!(view.editor().unwrap().document().snapshot().is_empty());
        key(
            &mut view,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
            Duration::ZERO,
            &mut keys,
            Preset::VsCode,
        );
        key(
            &mut view,
            KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::NONE),
            Duration::ZERO,
            &mut keys,
            Preset::VsCode,
        );
        assert_eq!(view.line(0), Some(b"Y".as_slice()));
    }

    struct SaveFile(std::path::PathBuf);
    impl SaveFile {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            Self(std::env::temp_dir().join(format!(
                "kwil-input-savequit-{}-{}-{}.txt",
                std::process::id(),
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            )))
        }
    }
    impl Drop for SaveFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn vim_wq(view: &mut View) -> KeyResult {
        let mut keys = KeymapEngine::new(Preset::Vim, &[]).unwrap();
        for c in ":wq".chars() {
            key(view, KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), Duration::ZERO, &mut keys, Preset::Vim);
        }
        key(view, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), Duration::ZERO, &mut keys, Preset::Vim)
    }

    #[test]
    fn vim_wq_prompts_untitled_and_closes_only_after_successful_save() {
        let file = SaveFile::new();
        let mut view = View::new(None);
        view.set_rows(3);
        execute(&mut view, "edit.insert", Some("keep me".into()), false, Duration::ZERO);
        assert!(matches!(vim_wq(&mut view), KeyResult::SaveAndQuit));
        let mut save = SaveFlow::default();
        assert!(save.request(&mut view, None, true).is_none());
        assert!(save.closing);
        for c in file.0.to_str().unwrap().chars() {
            path_key(&mut view, &mut save.path, KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert_eq!(view.line(0), Some(b"keep me".as_slice()));
        let KeyResult::Save(path) =
            path_key(&mut view, &mut save.path, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("path must submit save")
        };
        let request = save.request(&mut view, path, false).unwrap();
        assert!(view.editor().unwrap().document().is_dirty(), "request must not mark document saved");
        let result = std::thread::spawn(move || kwil_core_workspace::save_snapshot(request)).join().unwrap();
        assert!(save.finish(&mut view, result));
        assert!(!view.editor().unwrap().document().is_dirty());
        assert_eq!(std::fs::read(&file.0).unwrap(), b"keep me");
    }

    #[test]
    fn savequit_failure_and_concurrent_edits_keep_document_open() {
        let file = SaveFile::new();
        let mut view = View::new(None);
        view.set_rows(3);
        execute(&mut view, "edit.insert", Some("before".into()), false, Duration::ZERO);
        let mut save = SaveFlow::default();
        save.request(&mut view, Some(file.0.clone()), true).unwrap();
        assert!(!save.finish(&mut view, Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"))));
        assert!(view.status_text().contains("Save failed: denied"));
        assert!(!save.closing);
        let request = save.request(&mut view, Some(file.0.clone()), true).unwrap();
        execute(&mut view, "edit.insert", Some(" after".into()), false, Duration::from_secs(1));
        assert!(!save.finish(&mut view, kwil_core_workspace::save_snapshot(request)));
        assert!(view.editor().unwrap().document().is_dirty());
        assert_eq!(view.line(0), Some(b"before after".as_slice()));
        assert_eq!(std::fs::read(&file.0).unwrap(), b"before");
        assert!(!save.closing);
    }

    #[test]
    fn savequit_cancelled_prompt_and_failed_request_do_not_close_later() {
        let file = SaveFile::new();
        let mut view = View::new(None);
        view.set_rows(3);
        let mut save = SaveFlow::default();
        assert!(save.request(&mut view, None, true).is_none());
        assert!(matches!(
            path_key(&mut view, &mut save.path, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            KeyResult::Cancel
        ));
        // The run-loop Cancel branch clears the close intent.
        save.closing = false;
        let request = save.request(&mut view, Some(file.0.clone()), false).unwrap();
        assert!(
            save.request(&mut view, Some(file.0.clone()), true).is_none(),
            "second save is rejected while worker is active"
        );
        assert!(view.status_text().contains("save already in progress"));
        assert!(!save.finish(&mut view, kwil_core_workspace::save_snapshot(request)));
        let request = save.request(&mut view, None, false).unwrap();
        assert!(!save.finish(&mut view, kwil_core_workspace::save_snapshot(request)));
    }

    #[test]
    fn savequit_unrequested_completion_never_exits_even_when_clean() {
        let file = SaveFile::new();
        let mut view = View::new(None);
        view.set_rows(3);
        let request = view.editor_mut().unwrap().save_request(Some(file.0.clone())).unwrap();
        // A close request alone must not make an unsolicited/stale completion
        // equivalent to the save requested by :wq.
        let mut save = SaveFlow { closing: true, ..Default::default() };
        assert!(!save.finish(&mut view, kwil_core_workspace::save_snapshot(request)));
        assert!(!view.editor().unwrap().document().is_dirty());
    }

    #[test]
    fn savequit_stale_completion_does_not_close_after_undo_to_clean_revision() {
        let file = SaveFile::new();
        let earlier_file = SaveFile::new();
        let mut view = View::new(None);
        view.set_rows(3);
        let mut save = SaveFlow::default();
        let request = save.request(&mut view, Some(file.0.clone()), false).unwrap();
        let saved = kwil_core_workspace::save_snapshot(request).unwrap();
        assert!(!save.finish(&mut view, Ok(saved)));
        // Produce a real earlier-revision worker result without relying on
        // the storage result's fields beyond its public revision contract.
        let stale = kwil_core_workspace::save_snapshot(
            kwil_view_model::Editor::new(kwil_view_model::Document::new())
                .save_request(Some(earlier_file.0.clone()))
                .unwrap(),
        )
        .unwrap();
        execute(&mut view, "edit.insert", Some("new revision".into()), false, Duration::ZERO);
        let request = save.request(&mut view, None, true).unwrap();
        assert_ne!(request.revision, stale.revision);
        execute(&mut view, "edit.undo", None, false, Duration::from_secs(1));
        assert!(!view.editor().unwrap().document().is_dirty());
        assert!(!save.finish(&mut view, Ok(stale)), "a duplicate old completion is not the pending :wq save");
        assert!(!save.closing);
    }

    #[test]
    fn savequit_successful_trim_installs_a_clean_saved_revision() {
        let file = SaveFile::new();
        let mut view = View::new(None);
        view.set_rows(3);
        view.editor_mut().unwrap().settings.editor.trim_trailing_whitespace_on_save = true;
        execute(&mut view, "edit.insert", Some("trim me  ".into()), false, Duration::ZERO);
        let mut save = SaveFlow::default();
        let request = save.request(&mut view, Some(file.0.clone()), true).unwrap();
        assert!(save.finish(&mut view, kwil_core_workspace::save_snapshot(request)));
        assert!(!view.editor().unwrap().document().is_dirty());
        assert_eq!(std::fs::read(&file.0).unwrap(), b"trim me");
        let mut bytes = Vec::new();
        view.editor().unwrap().document().snapshot().write_to(&mut bytes).unwrap();
        assert_eq!(bytes, b"trim me");
    }

    #[test]
    fn untitled_save_prompt_keeps_text_out_of_document_and_provides_a_path() {
        let mut view = View::new(None);
        view.set_rows(3);
        let mut prompt = Some(String::new());
        for c in "日本語.txt".chars() {
            path_key(&mut view, &mut prompt, KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert!(
            matches!(path_key(&mut view,&mut prompt,KeyEvent::new(KeyCode::Enter,KeyModifiers::NONE)),KeyResult::Save(Some(path)) if path.as_path()==std::path::Path::new("日本語.txt"))
        );
        assert!(view.editor().unwrap().document().snapshot().is_empty());
        assert!(prompt.is_none());
    }

    #[test]
    fn rendered_tabs_wide_characters_selection_and_horizontal_caret_agree() {
        let mut view = View::new(None);
        view.set_rows(3);
        execute(&mut view, "edit.insert", Some("a\t界Z".into()), false, Duration::ZERO);
        let backend = ratatui::backend::TestBackend::new(30, 4);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &view)).unwrap();
        assert_eq!(terminal.get_cursor_position().unwrap().x, 15);
        let damage = view
            .editor_mut()
            .unwrap()
            .set_cursors(vec![kwil_view_model::Cursor { anchor: 0, head: 1, desired_column: None }], 0)
            .unwrap();
        view.refresh_after_edit(damage);
        terminal.draw(|frame| draw(frame, &view)).unwrap();
        assert_eq!(terminal.backend().buffer()[(8, 0)].bg, Color::Rgb(0x30, 0x58, 0x80));
        execute(&mut view, "edit.selectAll", None, false, Duration::from_secs(1));
        execute(&mut view, "edit.insert", Some("x".repeat(100)), false, Duration::from_secs(2));
        let mut horizontal = 0;
        keep_visible(&mut view, &mut horizontal, 22, 4);
        assert!(horizontal > 0);
        terminal.draw(|frame| draw(frame, &view)).unwrap();
        assert!(terminal.backend().cursor_visible(), "horizontal scrolling must keep the caret visible");
        assert_eq!(terminal.get_cursor_position().unwrap(), ratatui::layout::Position::new(8, 0));
    }

    fn press(view: &mut View, code: KeyCode, modifiers: KeyModifiers, seconds: u64) {
        let mut keys = KeymapEngine::new(Preset::VsCode, &[]).unwrap();
        key(view, KeyEvent::new(code, modifiers), Duration::from_secs(seconds), &mut keys, Preset::VsCode);
    }

    #[test]
    fn typing_space_newline_navigation_and_undo_edit_the_document() {
        let mut view = View::new(None);
        view.set_rows(8);
        press(&mut view, KeyCode::Char('a'), KeyModifiers::NONE, 0);
        press(&mut view, KeyCode::Char(' '), KeyModifiers::NONE, 0);
        press(&mut view, KeyCode::Char('b'), KeyModifiers::NONE, 0);
        assert_eq!(view.line(0), Some(b"a b".as_slice()));
        press(&mut view, KeyCode::Enter, KeyModifiers::NONE, 1);
        press(&mut view, KeyCode::Char('X'), KeyModifiers::NONE, 2);
        assert_eq!(view.line(1), Some(b"X".as_slice()));
        press(&mut view, KeyCode::Char('z'), KeyModifiers::CONTROL, 3);
        assert_eq!(view.line(1), Some(b"".as_slice()));
        press(&mut view, KeyCode::Char('y'), KeyModifiers::CONTROL, 4);
        assert_eq!(view.line(1), Some(b"X".as_slice()));
        press(&mut view, KeyCode::Left, KeyModifiers::SHIFT, 5);
        assert!(!view.editor().unwrap().primary_cursor().is_caret());
        press(&mut view, KeyCode::Char('Y'), KeyModifiers::NONE, 6);
        assert_eq!(view.line(1), Some(b"Y".as_slice()));
    }
}
