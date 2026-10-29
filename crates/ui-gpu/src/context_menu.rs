//! Editor and tab context actions share the normal command path and retain
//! source identity.
use super::*;
use kwil_core_syntax::logs::Level;
use kwil_view_model::{SessionDocumentId, TabAction};
#[derive(Clone, Copy)]
enum Action {
    Command(&'static str),
    Level(Option<Level>),
    Tab(TabCommand),
}
/// Right-click on a tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TabCommand {
    Close,
    CloseOthers,
    CloseToRight,
    CloseSaved,
    CloseAll,
    Save,
    Reload,
    CompareSaved,
    CopyPath,
    Reveal,
}
struct Item {
    label: String,
    action: Action,
    enabled: bool,
    icon: icons::Icon,
}
#[derive(Default)]
pub(super) struct Context {
    source: Option<(kwil_view_model::SessionDocumentId, u64, u64)>,
    anchor: (usize, usize),
    items: Vec<Item>,
    rows: Vec<Rect>,
    selected: usize,
    surface: Option<(usize, usize, u64)>,
    /// Set for a tab's context menu: the tab it acts on.
    tab: Option<SessionDocumentId>,
}
impl Context {
    #[cfg(feature = "frame-harness")]
    pub fn probe_labels(&self) -> impl Iterator<Item = &str> {
        self.items.iter().map(|item| item.label.as_str())
    }
    pub fn is_open(&self) -> bool {
        self.source.is_some()
    }
}
impl App {
    fn context_source(&self) -> Option<(kwil_view_model::SessionDocumentId, u64, u64)> {
        Some((self.view.active_id(), self.view.editor()?.document().revision(), self.view.input_epoch()))
    }
    pub(super) fn close_context(&mut self) {
        if self.context.is_open() {
            self.context = Context::default();
            self.menu_dirty = true;
            self.request_frame();
        }
    }
    pub(super) fn open_context(&mut self) {
        if self.file_modal()
            || self.save_path.is_some()
            || self.settings_open()
            || self.palette_open()
            || self.search_panel.is_open()
            || self.logs.is_open()
            || self.welcome.visible()
        {
            return;
        }
        let Some(frame) = &self.chrome_frame else { return };
        if let Some(slot) = frame.geometry.slots.iter().find(|slot| slot.area.contains(self.pointer.0, self.pointer.1))
        {
            let document = slot.document;
            self.open_tab_context(document);
            return;
        }
        if !frame.geometry.text.contains(self.pointer.0, self.pointer.1) {
            return;
        }
        let Some(source) = self.context_source() else { return };
        let available = self.menu_available();
        let preview = self.markdown_mode() == markdown_ui::Mode::Preview;
        let mut items = Vec::with_capacity(9);
        for (label, command, enabled) in [
            ("Undo", "edit.undo", !preview),
            ("Redo", "edit.redo", !preview),
            ("Cut", "edit.cut", available.selection && !preview),
            ("Copy", "edit.copy", available.selection),
            ("Paste", "edit.paste", !preview),
            ("Select All", "edit.selectAll", !preview),
        ] {
            let icon = match command {
                "edit.undo" => icons::Icon::Undo,
                "edit.redo" => icons::Icon::Redo,
                "edit.cut" => icons::Icon::Cut,
                "edit.copy" => icons::Icon::Copy,
                "edit.paste" => icons::Icon::Paste,
                _ => icons::Icon::Text,
            };
            items.push(Item { label: label.into(), action: Action::Command(command), enabled, icon });
        }
        let row = self.pointer.1.saturating_sub(self.body_area().y) / self.ch().max(1);
        let level = self
            .view
            .line(row)
            .filter(|_| self.view.line_offset(row) == 0)
            .and_then(|line| kwil_core_syntax::logs::metadata(line).level);
        if self.is_log_document()
            && let Some(level) = level
        {
            items.push(Item {
                label: format!("Filter log: {}", level.label()),
                action: Action::Level(Some(level)),
                enabled: true,
                icon: icons::Icon::Filter,
            });
        }
        if self.is_log_document() {
            items.push(Item {
                label: "Show all log levels".into(),
                action: Action::Level(None),
                enabled: true,
                icon: icons::Icon::Filter,
            });
        }
        if self.is_markdown_document() {
            for (label, command, icon) in [
                ("Markdown: Text", "view.markdownText", icons::Icon::Text),
                ("Markdown: Preview", "view.markdownPreview", icons::Icon::Markdown),
                ("Markdown: Split", "view.markdownSplit", icons::Icon::Split),
            ] {
                items.push(Item { label: label.into(), action: Action::Command(command), enabled: true, icon });
            }
        }
        self.show_context(source, None, items);
    }
    /// Close, close several, save, reload, compare, copy the path, or show the
    /// file in the system's file manager.
    pub(super) fn open_tab_context(&mut self, document: SessionDocumentId) {
        let ids = tabs::ids(&self.view);
        let Some(at) = ids.iter().position(|id| *id == document) else { return };
        let path = self.view.tab_path(document);
        let file = path.is_some();
        let mut items = Vec::with_capacity(10);
        for (label, command, enabled, icon) in [
            ("Close", TabCommand::Close, true, icons::Icon::Close),
            ("Close Others", TabCommand::CloseOthers, ids.len() > 1, icons::Icon::Close),
            ("Close to the Right", TabCommand::CloseToRight, at + 1 < ids.len(), icons::Icon::Close),
            ("Close Saved", TabCommand::CloseSaved, ids.iter().any(|id| !self.view.tab_dirty(*id)), icons::Icon::Close),
            ("Close All", TabCommand::CloseAll, true, icons::Icon::Close),
            ("Save", TabCommand::Save, self.view.tab_dirty(document), icons::Icon::Save),
            ("Reload from Disk", TabCommand::Reload, file, icons::Icon::Undo),
            ("Compare with Saved", TabCommand::CompareSaved, file, icons::Icon::Split),
            ("Copy Path", TabCommand::CopyPath, file, icons::Icon::Copy),
            (REVEAL_LABEL, TabCommand::Reveal, file, icons::Icon::Open),
        ] {
            items.push(Item { label: label.into(), action: Action::Tab(command), enabled, icon });
        }
        self.show_context((document, 0, 0), Some(document), items);
    }
    fn show_context(
        &mut self,
        source: (SessionDocumentId, u64, u64),
        tab: Option<SessionDocumentId>,
        items: Vec<Item>,
    ) {
        self.menu_set(menu::State::Closed, false);
        self.context = Context { source: Some(source), anchor: self.pointer, items, tab, ..Default::default() };
        self.drag = None;
        self.selecting = false;
        self.composing = false;
        self.ime_destination = Some(ime::Destination::Cancelled);
        if let Some(keys) = &mut self.keys {
            keys.cancel_pending();
        }
        self.menu_dirty = true;
        self.request_frame();
    }
    fn context_activate(&mut self, index: usize) {
        let tab = self.context.tab;
        // A tab menu stays valid while its tab exists; the editor menu while
        // its document, revision and input are unchanged.
        let valid = match tab {
            Some(document) => tabs::ids(&self.view).contains(&document),
            None => self.context_source() == self.context.source,
        };
        let action = self.context.items.get(index).filter(|item| item.enabled).map(|item| item.action);
        self.close_context();
        if !valid {
            return;
        }
        match action {
            Some(Action::Command(command)) => {
                self.palette_action(kwil_core_commands::palette::PaletteAction::Command(command))
            }
            Some(Action::Level(level)) => self.filter_log_level(level),
            Some(Action::Tab(command)) => {
                if let Some(document) = tab {
                    self.tab_command(document, command);
                }
            }
            None => {}
        }
    }
    fn tab_command(&mut self, document: SessionDocumentId, command: TabCommand) {
        let tab_action = match command {
            TabCommand::Close => Some(TabAction::Close(document)),
            TabCommand::CloseOthers => Some(TabAction::CloseOthers(document)),
            TabCommand::CloseToRight => Some(TabAction::CloseToRight(document)),
            TabCommand::CloseSaved => Some(TabAction::CloseSaved),
            TabCommand::CloseAll => Some(TabAction::CloseAll),
            _ => None,
        };
        if let Some(tab_action) = tab_action {
            let geometry = self.tab_frame().geometry;
            if let Err(error) = self.view.tab_action(tab_action, &geometry) {
                self.view.set_notice(error.to_string());
            }
            self.sync_file_ui();
            self.apply(Damage { full: true, status: true, ..Default::default() });
            return;
        }
        let path = self.view.tab_path(document);
        match command {
            TabCommand::CopyPath => {
                let Some(path) = path else { return };
                let notice = if self.opts.hidden {
                    "Clipboard is disabled in hidden tests".to_owned()
                } else {
                    use kwil_view_model::Clipboard;
                    let hwnd = self.desktop_window.as_ref().map_or(0, |d| d.gfx.hwnd);
                    match kwil_view_model::ClipboardData::text(path.to_string_lossy().as_bytes())
                        .and_then(|data| clipboard::SystemClipboard(hwnd).set(data))
                    {
                        Ok(()) => format!("Copied {}", path.display()),
                        Err(error) => format!("Copy path: {error}"),
                    }
                };
                let damage = self.view.set_notice(notice);
                self.apply(damage);
            }
            TabCommand::Reveal => {
                let Some(path) = path else { return };
                if !self.opts.hidden
                    && let Err(error) = reveal(&path)
                {
                    let damage = self.view.set_notice(format!("Show in file manager: {error}"));
                    self.apply(damage);
                }
            }
            TabCommand::Save | TabCommand::Reload | TabCommand::CompareSaved => {
                // These act on the active tab, like their palette commands.
                if self.view.active_id() != document {
                    let geometry = self.tab_frame().geometry;
                    let _ = self.view.tab_action(TabAction::Activate(document), &geometry);
                    self.sync_file_ui();
                    self.apply(Damage { full: true, status: true, ..Default::default() });
                }
                let id = match command {
                    TabCommand::Save => "file.save",
                    TabCommand::Reload => "file.reload",
                    _ => "compare.saved",
                };
                self.palette_action(kwil_core_commands::palette::PaletteAction::Command(id));
            }
            _ => {}
        }
    }
    pub(super) fn context_mouse(&mut self) -> bool {
        if !self.context.is_open() {
            return false;
        }
        let surface = Some((self.size.0, self.size.1, self.ui.scale.to_bits()));
        let hit = (self.context.surface == surface)
            .then(|| self.context.rows.iter().position(|r| r.contains(self.pointer)))
            .flatten();
        if let Some(index) = hit {
            self.context_activate(index);
        } else {
            self.close_context();
        }
        true
    }
    pub(super) fn context_track(&mut self) {
        if !self.context.is_open() {
            return;
        }
        if let Some(index) = self.context.rows.iter().position(|r| r.contains(self.pointer))
            && index != self.context.selected
        {
            self.context.selected = index;
            self.menu_dirty = true;
            self.request_frame();
        }
    }
    pub(super) fn context_key(&mut self, event: &HostKey<'_>) -> bool {
        if !self.context.is_open() {
            return false;
        }
        match event.logical_key {
            Key::Named(NamedKey::Escape) => self.close_context(),
            Key::Named(NamedKey::ArrowDown) => {
                self.context.selected = (self.context.selected + 1) % self.context.items.len()
            }
            Key::Named(NamedKey::ArrowUp) => {
                self.context.selected =
                    (self.context.selected + self.context.items.len() - 1) % self.context.items.len()
            }
            Key::Named(NamedKey::Enter | NamedKey::Space) => self.context_activate(self.context.selected),
            _ => {}
        }
        self.menu_dirty = true;
        self.request_frame();
        true
    }
}
#[cfg(windows)]
const REVEAL_LABEL: &str = "Show in File Explorer";
#[cfg(target_os = "macos")]
const REVEAL_LABEL: &str = "Reveal in Finder";
#[cfg(not(any(windows, target_os = "macos")))]
const REVEAL_LABEL: &str = "Open Containing Folder";

/// Shows the file selected in the system's file manager.
fn reveal(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(windows)]
    let mut command = {
        use std::os::windows::process::CommandExt;
        let mut command = std::process::Command::new("explorer.exe");
        // explorer parses "/select," and the quoted path as one argument.
        command.raw_arg(format!("/select,\"{}\"", path.display()));
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = std::process::Command::new("open");
        command.arg("-R").arg(path);
        command
    };
    #[cfg(not(any(windows, target_os = "macos")))]
    let mut command = {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(path.parent().unwrap_or(path));
        command
    };
    // The file manager outlives this call. Reap the launcher off the UI thread
    // so it never lingers as a zombie on Unix.
    let mut child = command.spawn()?;
    std::thread::Builder::new().name("kwil-reveal".into()).spawn(move || {
        let _ = child.wait();
    })?;
    Ok(())
}

pub(super) fn draw(
    context: &mut Context,
    pixels: &mut [u32],
    size: (usize, usize),
    font: &mut font::UiFont,
    ui: &chrome::Metrics,
    theme: &kwil_core_commands::theme::Theme,
) -> Option<Rect> {
    if !context.is_open() {
        return None;
    }
    let colors = desktop::Colors::new(theme);
    let sizes = desktop::Sizes::new(ui);
    let row = ui.dip(32.0);
    let pad = ui.dip(8.0);
    let width = ui.dip(280.0).min(size.0);
    let height = (context.items.len() * row + 2 * pad).min(size.1);
    let area = Rect {
        x: context.anchor.0.min(size.0.saturating_sub(width)),
        y: context.anchor.1.min(size.1.saturating_sub(height)),
        w: width,
        h: height,
    };
    desktop::card(pixels, size.0, area, &colors, &sizes);
    context.rows.clear();
    for (index, item) in context.items.iter().enumerate() {
        let y = area.y + pad + index * row;
        if y + row > area.y + area.h {
            break;
        }
        let rect = Rect { x: area.x + pad, y, w: width.saturating_sub(2 * pad), h: row };
        if index == context.selected {
            chrome::fill_clipped(pixels, size.0, rect, colors.control_hover);
        }
        let edge = ui.dip(18.0).min(rect.h);
        icons::draw(
            pixels,
            size.0,
            item.icon,
            ((rect.x + pad + edge / 2) as f32, (rect.y + rect.h / 2) as f32),
            edge,
            if item.enabled { colors.fg } else { colors.faint },
        );
        let text_rect = Rect { x: rect.x + edge + 2 * pad, w: rect.w.saturating_sub(edge + 2 * pad), ..rect };
        desktop::label(
            pixels,
            size.0,
            font,
            desktop::inset(text_rect, pad / 2),
            &item.label,
            if item.enabled { colors.fg } else { colors.faint },
        );
        context.rows.push(rect);
    }
    context.surface = Some((size.0, size.1, ui.scale.to_bits()));
    Some(area)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn right_click_on_a_tab_opens_the_tab_menu_and_acts_on_that_tab() {
        let (mut app, _events) = crate::search_ui::host_dispatch_tests::host("first");
        let first = app.view.active_id();
        let second = app.view.open(View::new(None)).unwrap();
        app.view.open(View::new(None)).unwrap();
        let frame = app.tab_frame();
        let slot = frame.geometry.slots.iter().find(|slot| slot.document == second).unwrap().clone();
        app.chrome_frame = Some(frame);
        app.pointer = (slot.area.x + 2, slot.area.y + slot.area.height / 2);
        app.open_context();
        assert_eq!(app.context.tab, Some(second));
        let labels: Vec<_> = app.context.items.iter().map(|item| (item.label.as_str(), item.enabled)).collect();
        assert_eq!(
            labels,
            [
                ("Close", true),
                ("Close Others", true),
                ("Close to the Right", true),
                ("Close Saved", true),
                ("Close All", true),
                ("Save", false),
                ("Reload from Disk", false),
                ("Compare with Saved", false),
                ("Copy Path", false),
                (REVEAL_LABEL, false),
            ],
            "an untitled, unsaved-free tab: file actions are disabled"
        );
        let others = app.context.items.iter().position(|item| item.label == "Close Others").unwrap();
        app.context_activate(others);
        assert!(!app.context.is_open());
        // The first tab has unsaved text: Kwil asks before closing it.
        assert!(app.view.close_pending(), "unsaved tabs are never closed silently");
        assert!(tabs::ids(&app.view).contains(&first));
        app.view.cancel_file_prompt();
        app.view.discard_pending_close().ok();
        // A clean tab to the right closes at once.
        let third = *tabs::ids(&app.view).last().unwrap();
        app.open_tab_context(second);
        let right = app.context.items.iter().position(|item| item.label == "Close to the Right").unwrap();
        app.context_activate(right);
        assert!(!tabs::ids(&app.view).contains(&third), "{:?}", tabs::ids(&app.view));
        assert!(tabs::ids(&app.view).contains(&second));
    }
}
