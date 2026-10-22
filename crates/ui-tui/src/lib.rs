//! Terminal frontend.
//!
//! Same model as the GUI (`kwil_view_model::View`). Event-driven: one
//! thread blocks on terminal input and forwards events into a channel that
//! also carries worker results, so the UI thread sleeps until something
//! happens. ratatui writes only changed cells, so the TUI stays fast over
//! SSH.

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use kwil_core_commands::{Preset, keymap::KeymapEngine, settings::Settings, vim::VimResult};
use kwil_view_model::{CommandArgs, CommandStatus, Notify, View, Workspace};
mod file_prompt;
mod palette_ui;
mod settings_ui;
mod tabs;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::sync::mpsc;

enum Msg {
    HighlightsReady,
    PaletteRoot(Result<std::path::PathBuf, String>),
    PaletteReady,
    SettingsPath(Result<std::path::PathBuf, String>),
    SettingsPanelReady,
    Input(Event),
    Document(kwil_view_model::document_worker::DocumentResult),
    OpenExternal,
    Notice(String),
    InputClosed,
    Settings(Box<Settings>),
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
    let mut view = Workspace::new(view)?;
    let (tx, rx) = mpsc::channel::<Msg>();
    let external_sender = tx.clone();
    let (external_open, external_inbox) = kwil_view_model::document_worker::external_open_channel(move || {
        external_sender
            .send(Msg::OpenExternal)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "TUI closed"))
    });
    let mut terminal = ratatui::init();
    crossterm::execute!(
        std::io::stdout(),
        crossterm::event::EnableMouseCapture,
        crossterm::event::EnableBracketedPaste
    )?;
    {
        let t = tx.clone();
        std::thread::spawn(move || {
            loop {
                match event::read() {
                    Ok(ev) => {
                        if t.send(Msg::Input(ev)).is_err() {
                            break;
                        }
                    }
                    Err(_) => {
                        let _ = t.send(Msg::InputClosed);
                        break;
                    }
                }
            }
        });
    }

    let mut palette: Option<kwil_view_model::palette_session::PaletteSession> = None;
    let mut highlighting: Option<kwil_view_model::highlight_session::HighlightSession> = None;
    let mut settings_panel: Option<kwil_view_model::settings_session::SettingsSession> = None;
    let mut settings_file_pending = false;
    let result = (|| -> std::io::Result<()> {
        let size = terminal.size()?;
        let geometry = tabs::geometry(&view, size.width, size.height, Settings::default().tabs.layout);
        view.set_rows(geometry.body.height);
        terminal.draw(|f| tabs::draw(f, &view, Settings::default().tabs.layout))?;
        let document_sender = tx.clone();
        view.start_document_worker(move |result| {
            let _ = document_sender.send(Msg::Document(result));
        })?;
        let t = tx.clone();
        on_first_frame(Box::new(move |msg| {
            let _ = t.send(Msg::Notice(msg));
        }));
        on_external_ready(external_open);
        let highlight_sender = tx.clone();
        highlighting = kwil_view_model::highlight_session::HighlightSession::new(move || {
            let _ = highlight_sender.send(Msg::HighlightsReady);
        })
        .ok();
        if let Some(host) = &mut highlighting {
            host.update(&view);
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
        let mut input_identity = (view.active_id(), view.input_epoch());
        while let Ok(msg) = rx.recv() {
            let user_input = matches!(&msg, Msg::Input(_));
            let changed = match msg {
                Msg::HighlightsReady => highlighting.as_mut().is_some_and(|host| !host.worker_ready(&view).is_empty()),
                Msg::PaletteRoot(root) => {
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
                            false
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
                            && (report.errors > 0 || report.end != kwil_core_workspace::catalogue::WalkEnd::Complete)
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
                Msg::InputClosed => break,
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
                            let _ = tx.send(Msg::Settings(Box::new(applied.settings)));
                        }
                        if settings_file_pending && panel.source_exists() && !panel.is_saving() && !panel.is_dirty() {
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
                Msg::Settings(next) => {
                    if next.keymap == settings.keymap && next.bindings == settings.bindings {
                        settings = *next;
                    } else if let Ok(next_keys) = KeymapEngine::new(next.keymap, &next.bindings) {
                        keys = next_keys;
                        settings = *next;
                    }
                    view.set_session_restore(settings.files.restore_session);
                    if let Some(editor) = view.editor_mut() {
                        editor.settings = settings.clone();
                    }
                    true
                }
                Msg::Document(result) => {
                    view.document_result(result);
                    if let Some(editor) = view.editor_mut() {
                        editor.settings = settings.clone();
                    }
                    if view.wants_exit() {
                        break;
                    }
                    true
                }
                Msg::OpenExternal => {
                    let requests = external_inbox.drain();
                    if requests.is_empty() {
                        continue;
                    }
                    for request in requests {
                        if let Err(error) = view.open_files(request) {
                            view.set_notice(format!("Open failed: {error}"));
                        }
                    }
                    true
                }
                Msg::Notice(n) => {
                    view.set_notice(n);
                    true
                }
                Msg::Input(Event::Resize(_, _)) => true,
                Msg::Input(Event::Key(k)) if k.kind != KeyEventKind::Release => {
                    let intent = if let Some(panel) = settings_panel.as_mut().filter(|panel| panel.is_open()) {
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
                        file_prompt::key(&mut view, k)
                    } else if let Some(host) = palette.as_mut().filter(|host| host.model().is_open()) {
                        palette_ui::key(&mut view, host, k, &mut keys, started.elapsed(), settings.keymap)
                    } else if view.close_pending() && k.code == KeyCode::Esc {
                        KeyResult::Cancel
                    } else {
                        key(&mut view, k, started.elapsed(), &mut keys, settings.keymap)
                    };
                    match intent {
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
                            palette_ui::open(&mut view, palette.as_mut(), &mut keys, query);
                            true
                        }
                        KeyResult::Quit => {
                            let discard = view.close_pending() && k.modifiers.contains(KeyModifiers::SHIFT);
                            if let Err(error) = view.request_window_close(discard) {
                                view.set_notice(error.to_string());
                            }
                            if view.wants_exit() {
                                break;
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
                                break;
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
                    } else if let Some(host) = palette.as_mut().filter(|host| host.model().is_open()) {
                        let _ = palette_ui::input(
                            &mut view,
                            host,
                            &mut keys,
                            kwil_view_model::palette_session::PaletteInput::Text(&text),
                            started.elapsed(),
                            settings.keymap,
                        );
                        true
                    } else if keys.mode() == kwil_core_commands::Mode::Insert {
                        execute(&mut view, "edit.insert", Some(text), false, started.elapsed());
                        true
                    } else {
                        false
                    }
                }
                Msg::Input(Event::Mouse(_)) if settings_panel.as_ref().is_some_and(|panel| panel.is_open()) => true,
                Msg::Input(Event::Mouse(m)) if palette.as_ref().is_some_and(|host| host.model().is_open()) => {
                    let size = terminal.size()?;
                    palette_ui::mouse(
                        &mut view,
                        palette.as_mut().unwrap(),
                        &mut keys,
                        &m,
                        Rect::new(0, 0, size.width, size.height),
                    )
                }
                Msg::Input(Event::Mouse(m))
                    if {
                        let size = terminal.size()?;
                        tabs::mouse(&mut view, &m, size.width, size.height, settings.tabs.layout)
                    } =>
                {
                    true
                }
                Msg::Input(Event::Mouse(m)) => match m.kind {
                    MouseEventKind::ScrollDown => view.scroll(3) != Default::default(),
                    MouseEventKind::ScrollUp => view.scroll(-3) != Default::default(),
                    _ => false,
                },
                Msg::Input(_) => false,
            };
            palette_ui::sync_input_identity(&view, &mut keys, &mut input_identity);
            if let Some(host) = palette.as_mut().filter(|host| host.model().is_open())
                && let Err(error) = host.refresh_tabs(&view)
            {
                view.set_notice(format!("Palette: {error:?}"));
            }
            if changed {
                if user_input {
                    view.checkpoint_session();
                }
                let size = terminal.size()?;
                let geometry = tabs::geometry(&view, size.width, size.height, settings.tabs.layout);
                if view.rows() != geometry.body.height {
                    view.set_rows(geometry.body.height);
                }
                if !palette.as_ref().is_some_and(|host| host.model().is_open()) {
                    keep_visible(
                        &mut view,
                        &mut horizontal,
                        geometry.body.width.saturating_sub(8),
                        settings.editor.tab_size as usize,
                    );
                }
                // ratatui diffs against the previous frame and writes only
                // the changed cells.
                if let Some(editor) = view.editor_mut() {
                    editor.settings = settings.clone();
                }
                if let Some(host) = &mut highlighting {
                    prepare_highlighting_for_draw(host, &view);
                }
                let spans: Vec<_> = highlighting.as_ref().into_iter().flat_map(|host| host.spans(&view)).collect();
                terminal.draw(|f| {
                    tabs::draw_highlighted(f, &view, settings.tabs.layout, &spans);
                    if let Some(host) = &palette {
                        palette_ui::draw(f, host.model());
                    }
                    if let Some(panel) = &settings_panel {
                        settings_ui::draw(f, panel);
                    }
                })?;
            }
        }
        Ok(())
    })();

    if let Some(host) = &mut highlighting {
        host.stop();
    }
    let settings_shutdown = if let Some(panel) = &mut settings_panel {
        let shutdown = panel.stop();
        Some(std::thread::spawn(move || shutdown.shutdown()))
    } else {
        None
    };

    if let Some(host) = &mut palette {
        let handles = host.stop();
        std::thread::spawn(move || {
            for handle in handles {
                let _ = handle.join();
            }
        });
    }
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::DisableMouseCapture,
        crossterm::event::DisableBracketedPaste
    );
    ratatui::restore();
    if let Some(shutdown) = settings_shutdown {
        shutdown
            .join()
            .map_err(|_| std::io::Error::other("settings shutdown worker panicked"))?
            .map_err(std::io::Error::other)?;
    }
    result
}

enum KeyResult {
    Settings,
    SettingsFile,
    Quit,
    Save(Option<std::path::PathBuf>),
    SaveAndQuit,
    Cancel,
    Changed(bool),
    File(&'static str),
    Palette(&'static str),
}

fn prepare_highlighting_for_draw(host: &mut kwil_view_model::highlight_session::HighlightSession, view: &Workspace) {
    if host.update(view) {
        host.invalidate_painted(view, &kwil_view_model::Damage { full: true, ..Default::default() });
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
            Ok(result) => {
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
            Err(error) => {
                view.set_notice(format!("Save failed: {error}"));
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
            return KeyResult::Changed(execute(view, "edit.insert", Some(c.to_string()), false, elapsed));
        }
        _ => return KeyResult::Changed(false),
    };
    KeyResult::Changed(execute(view, command, None, k.modifiers.contains(KeyModifiers::SHIFT), elapsed))
}

fn invoke(
    view: &mut View,
    call: kwil_core_commands::vim::Invocation,
    extend: bool,
    elapsed: std::time::Duration,
    preset: Preset,
) -> KeyResult {
    if call.id == "view.settings" {
        return KeyResult::Settings;
    }
    if call.id == "view.settingsFile" {
        return KeyResult::SettingsFile;
    }
    if let Some(query) = palette_ui::command_query(call.id) {
        return KeyResult::Palette(query);
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
    view: &View,
    area: Rect,
    status: bool,
    highlights: &[&kwil_view_model::highlighting::Span],
) {
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
    use std::time::Duration;

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
