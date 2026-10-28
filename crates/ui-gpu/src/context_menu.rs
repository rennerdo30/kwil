//! Editor context actions share the normal command path and retain source identity.
use super::*;
use kwil_core_syntax::logs::Level;
#[derive(Clone, Copy)]
enum Action {
    Command(&'static str),
    Level(Option<Level>),
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
        self.menu_set(menu::State::Closed, false);
        self.context = Context { source: Some(source), anchor: self.pointer, items, ..Default::default() };
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
        let valid = self.context_source() == self.context.source;
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
            None => {}
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
