//! Status-anchored language list. The shared ticket owns document identity;
//! painted rows additionally own their surface and list generation.
use super::*;
use desktop::{Colors, Hits, Sizes, Surface};
use kwil_view_model::language_selection::{LanguagePicker, LanguagePickerItem};

#[derive(Default)]
pub(super) struct LanguagePickerUi {
    ticket: Option<LanguagePicker>,
    rows: Vec<LanguagePickerItem>,
    selected: usize,
    first: usize,
    hover: Option<usize>,
    generation: u64,
    painted: Option<(Surface, u64)>,
    hits: Hits<usize>,
    pub area: Option<Rect>,
    pub dirty: bool,
}

#[derive(Clone, Copy)]
enum Move {
    Previous,
    Next,
    Home,
    End,
}

impl LanguagePickerUi {
    pub fn is_open(&self) -> bool {
        self.ticket.is_some()
    }

    fn changed(&mut self) {
        self.generation = self.generation.checked_add(1).expect("language list generation exhausted");
        self.painted = None;
        self.dirty = true;
    }

    fn open(&mut self, ticket: LanguagePicker) {
        self.rows = ticket.items("").collect();
        self.selected = self.rows.iter().position(|row| row.selected).unwrap_or(0);
        self.first = 0;
        self.hover = None;
        self.ticket = Some(ticket);
        self.changed();
    }

    fn close(&mut self) {
        self.ticket = None;
        self.hits = Hits::default();
        self.hover = None;
        self.changed();
    }

    fn navigate(&mut self, action: Move) -> bool {
        let last = self.rows.len().saturating_sub(1);
        let next = match action {
            Move::Previous => self.selected.saturating_sub(1),
            Move::Next => self.selected.saturating_add(1).min(last),
            Move::Home => 0,
            Move::End => last,
        };
        if next == self.selected {
            return false;
        }
        self.selected = next;
        self.hover = None;
        self.changed();
        true
    }

    fn hit(&self, pointer: (usize, usize), surface: Surface, view: &Workspace) -> Option<usize> {
        if self.painted != Some((surface, self.generation)) || !self.ticket.as_ref()?.is_current(view) {
            return None;
        }
        self.hits.at(pointer).filter(|index| *index < self.rows.len())
    }
}

/// The card grows upward from the status field. Very small windows keep the
/// keyboard list usable while refusing to record impossible pointer targets.
fn card(size: (usize, usize), anchor: Rect, m: &chrome::Metrics, count: usize) -> Option<(Rect, usize)> {
    let s = Sizes::new(m);
    let width = m.dip(320.0).min(size.0.saturating_sub(2 * s.gap));
    let bottom = anchor.y.min(size.1).saturating_sub(s.gap);
    let fixed = s.header + s.footer + 2 * s.hairline;
    let visible = bottom.saturating_sub(s.gap + fixed) / s.nav.max(1);
    let visible = visible.min(10).min(count);
    if width < m.dip(180.0) || visible == 0 {
        return None;
    }
    let height = fixed + visible * s.nav;
    let right = (anchor.x + anchor.w).min(size.0.saturating_sub(s.gap));
    let x = right.saturating_sub(width).max(s.gap);
    Some((Rect { x, y: bottom - height, w: width, h: height }, visible))
}

pub(super) fn draw(
    state: &mut LanguagePickerUi,
    pixels: &mut [u32],
    size: (usize, usize),
    anchor: Rect,
    font: &mut font::UiFont,
    m: &chrome::Metrics,
) -> Option<Rect> {
    state.hits = Hits::default();
    state.painted = None;
    state.area = None;
    if !state.is_open() {
        return None;
    }
    let (area, visible) = card(size, anchor, m, state.rows.len())?;
    state.area = Some(area);
    state.first = desktop::reveal(state.first, state.selected, visible, state.rows.len());
    let c = Colors::new(theme_ui::chrome());
    let s = Sizes::new(m);
    let stride = size.0;
    desktop::card(pixels, stride, area, &c, &s);
    let header = Rect { x: area.x + s.pad, y: area.y, w: area.w - 2 * s.pad, h: s.header };
    icons::draw(
        pixels,
        stride,
        icons::Icon::Text,
        (header.x as f32 + s.icon as f32, (header.y + header.h / 2) as f32),
        2 * s.icon,
        c.muted,
    );
    desktop::label(
        pixels,
        stride,
        font,
        Rect { x: header.x + 3 * s.icon, w: header.w.saturating_sub(3 * s.icon), ..header },
        "Language",
        c.fg,
    );
    desktop::hline(pixels, stride, area.x, area.y + s.header, area.w, &c, &s);
    for (index, item) in state.rows.iter().enumerate().skip(state.first).take(visible) {
        let row = Rect {
            x: area.x + s.hairline,
            y: area.y + s.header + s.hairline + (index - state.first) * s.nav,
            w: area.w - 2 * s.hairline,
            h: s.nav,
        };
        desktop::row(pixels, stride, row, index == state.selected, state.hover == Some(index), &c, &s);
        state.hits.push(row, index);
        let label = Rect { x: row.x + s.pad + 2 * s.icon, w: row.w.saturating_sub(2 * s.pad + 2 * s.icon), ..row };
        if item.selected {
            icons::draw(
                pixels,
                stride,
                icons::Icon::Check,
                ((row.x + s.pad) as f32, (row.y + row.h / 2) as f32),
                2 * s.icon,
                c.accent,
            );
        }
        let suffix = if item.detected { "Detected" } else { "" };
        let suffix_width = if suffix.is_empty() { 0 } else { font.width(suffix) + s.pad };
        desktop::label(
            pixels,
            stride,
            font,
            Rect { w: label.w.saturating_sub(suffix_width), ..label },
            item.label,
            c.fg,
        );
        if item.detected {
            desktop::label_right(pixels, stride, font, label, suffix, c.muted);
        }
    }
    let footer = Rect { x: area.x + s.pad, y: area.y + area.h - s.footer, w: area.w - 2 * s.pad, h: s.footer };
    desktop::hline(pixels, stride, area.x, footer.y, area.w, &c, &s);
    desktop::label(pixels, stride, font, footer, "↑ ↓ select · Enter apply · Esc close", c.muted);
    state.painted = Some((Surface::new(size, m), state.generation));
    Some(area)
}

impl App {
    pub(super) fn language_highlights_ready(&mut self) -> Damage {
        if let Some(damage) = self.wrap_highlights_ready() {
            return damage;
        }
        if self.view.hex_active() {
            return Damage::default();
        }
        let rows = self.highlighting.as_mut().map_or_else(Vec::new, |host| host.worker_ready(&self.view));
        Damage { rows, status: true, ..Default::default() }
    }

    pub(super) fn apply_language_mappings(
        &mut self,
        mappings: kwil_view_model::language_selection::AcceptedLanguageMappings,
    ) {
        self.close_language();
        let wrapped = self.wrap_owns_text() || self.view.wrap_input_blocked();
        if let Some(host) = &mut self.highlighting {
            host.apply_mappings(mappings);
            if !wrapped && !self.view.hex_active() {
                host.update(&self.view);
            }
        }
        if wrapped {
            self.wrap_update_highlights();
        }
        self.apply(Damage { status: true, ..Default::default() });
    }

    pub(super) fn language_open(&self) -> bool {
        self.language_picker.is_open()
    }

    pub(super) fn language_owns_input(&self) -> bool {
        self.language_open() || self.language_picker.area.is_some()
    }

    pub(super) fn close_language(&mut self) {
        if !self.language_open() {
            return;
        }
        self.language_picker.close();
        self.ime_destination = Some(ime::Destination::Cancelled);
        self.search_ime = None;
        self.logs_ime = None;
        self.composing = false;
        if let Some(keys) = &mut self.keys {
            keys.cancel_pending();
            keys.set_context(kwil_core_commands::Context::Editor);
        }
        self.request_frame();
    }

    pub(super) fn sync_language_identity(&mut self) {
        if self.language_picker.ticket.as_ref().is_some_and(|ticket| !ticket.is_current(&self.view)) {
            self.close_language();
        }
    }

    pub(super) fn dispatch_language(&mut self, command: &str) -> bool {
        if command != "file.setType" {
            return false;
        }
        if self.file_modal() || self.save_path.is_some() || self.welcome.visible() {
            return true;
        }
        let Some(host) = &self.highlighting else {
            let damage = self.view.set_notice("Highlighting is starting; try the language picker again shortly".into());
            self.apply(damage);
            return true;
        };
        let ticket = host.language_picker(&self.view);
        self.close_context();
        self.close_search();
        self.close_logs();
        self.menu_set(menu::State::Closed, false);
        self.goto = None;
        self.goto_epoch = self.goto_epoch.checked_add(1).expect("goto input epoch exhausted");
        if let Some(palette) = &mut self.palette {
            let _ = palette.input(kwil_view_model::palette_session::PaletteInput::Cancel);
        }
        if let Some(panel) = &mut self.settings_panel {
            panel.close();
        }
        self.settings_view = Default::default();
        self.settings_epoch = self.settings_epoch.checked_add(1).expect("settings input epoch exhausted");
        self.palette_dirty = true;
        self.ime_destination = Some(ime::Destination::Cancelled);
        self.search_ime = None;
        self.logs_ime = None;
        self.cancel_logs_composition();
        self.composing = false;
        self.drag = None;
        self.selecting = false;
        self.wheel.clear();
        if let Some(keys) = &mut self.keys {
            keys.cancel_pending();
            keys.set_context(kwil_core_commands::Context::Palette);
        }
        self.language_picker.open(ticket);
        self.update_pointer_cursor();
        self.request_frame();
        true
    }

    fn accept_language(&mut self, index: usize) {
        let wrapped = self.wrap_owns_text() || self.view.wrap_input_blocked();
        let ticket = self.language_picker.ticket.clone();
        let row = self.language_picker.rows.get(index).copied();
        if let (Some(ticket), Some(row), Some(host)) = (ticket, row, &mut self.highlighting)
            && ticket.is_current(&self.view)
        {
            // Same choice still needs fresh status and an update after closing;
            // a changed choice reissues highlighting without a document edit.
            host.apply_picker(&self.view, &ticket, row.choice);
            if !wrapped {
                host.update(&self.view);
            }
        }
        if wrapped {
            self.wrap_update_highlights();
        }
        self.close_language();
        self.apply(Damage { status: true, ..Default::default() });
    }

    pub(super) fn language_key(&mut self, event: &HostKey<'_>) -> bool {
        if !self.language_owns_input() {
            return false;
        }
        self.sync_language_identity();
        if !self.language_open() {
            return true;
        }
        let action = match event.logical_key {
            Key::Named(NamedKey::ArrowUp) => Some(Move::Previous),
            Key::Named(NamedKey::ArrowDown) => Some(Move::Next),
            Key::Named(NamedKey::Home) => Some(Move::Home),
            Key::Named(NamedKey::End) => Some(Move::End),
            Key::Named(NamedKey::Enter) if !event.repeat => {
                self.accept_language(self.language_picker.selected);
                None
            }
            Key::Named(NamedKey::Escape) if !event.repeat => {
                self.close_language();
                None
            }
            _ => None,
        };
        if let Some(action) = action
            && self.language_picker.navigate(action)
        {
            self.request_frame();
        }
        // All keys belong to the list, including editor bindings and text.
        true
    }

    pub(super) fn language_mouse(&mut self) {
        if !self.language_open() {
            return;
        }
        let surface = Surface::new(self.pending_resize.unwrap_or(self.size), &self.ui);
        if let Some(index) = self.language_picker.hit(self.pointer, surface, &self.view) {
            self.accept_language(index);
        } else if self.language_picker.area.is_none_or(|area| !area.contains(self.pointer)) {
            self.close_language();
        }
    }

    pub(super) fn language_track(&mut self) {
        if !self.language_open() {
            return;
        }
        let surface = Surface::new(self.pending_resize.unwrap_or(self.size), &self.ui);
        let hover = self.language_picker.hit(self.pointer, surface, &self.view);
        if self.language_picker.hover != hover {
            self.language_picker.hover = hover;
            self.language_picker.dirty = true;
            self.request_frame();
        }
    }

    pub(super) fn language_wheel(&mut self, delta: MouseScrollDelta) {
        let y = match delta {
            MouseScrollDelta::LineDelta(_, y) => f64::from(y),
            MouseScrollDelta::PixelDelta(point) => point.y,
        };
        if y != 0.0 && self.language_picker.navigate(if y > 0.0 { Move::Previous } else { Move::Next }) {
            self.request_frame();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn shutdown_highlights(host: kwil_view_model::highlight_session::HighlightSession) {
        if let std::ops::ControlFlow::Break(error) = host.shutdown() {
            panic!("highlight shutdown refused: {error:?}");
        }
    }
    use kwil_view_model::highlight_session::HighlightSession;
    use std::time::{Duration, Instant};

    fn bytes(app: &App) -> Vec<u8> {
        let snapshot = app.view.editor().unwrap().document().snapshot();
        snapshot.chunks(0..snapshot.len()).unwrap().flat_map(|chunk| chunk.unwrap().to_vec()).collect()
    }

    fn press(app: &mut App, key: NamedKey) {
        app.key_after_settings(None, &HostKey { logical_key: Key::Named(key), text: None, repeat: false });
    }

    #[test]
    fn host_palette_menu_keymap_paths_own_input_and_apply_without_editing() {
        use kwil_view_model::highlighting::{Language, LanguageChoice};
        let (mut app, _) = crate::search_ui::host_dispatch_tests::host("unchanged source");
        app.highlighting = Some(HighlightSession::new(|| {}).unwrap());
        let revision = app.view.editor().unwrap().document().revision();
        let original = bytes(&app);
        let document = app.view.active_id();
        app.dispatch_palette("navigation.goToLine");
        assert!(app.goto.is_some());
        app.palette_action(kwil_core_commands::palette::PaletteAction::Command("file.setType"));
        assert!(app.language_open());
        assert!(app.goto.is_none());
        app.ime_preedit("composition");
        assert!(!app.composing);
        assert!(matches!(app.ime_capture(), ime::Destination::Cancelled));
        app.key_after_settings(
            None,
            &HostKey { logical_key: Key::Character("x".into()), text: Some("x"), repeat: false },
        );
        app.edit(input::Action::Text("cannot edit behind list".into()));
        assert_eq!(bytes(&app), original);
        let row = app
            .language_picker
            .rows
            .iter()
            .position(|row| row.choice == LanguageChoice::Explicit(Language::Python))
            .unwrap();
        app.language_picker.selected = row;
        app.pending = Damage::default();
        press(&mut app, NamedKey::Enter);
        assert!(!app.language_open());
        assert_eq!(
            app.highlighting.as_ref().unwrap().language_choice(document),
            LanguageChoice::Explicit(Language::Python)
        );
        assert!(app.pending.status, "choice updates status without a document edit");
        assert_eq!(app.view.editor().unwrap().document().revision(), revision);
        assert_eq!(bytes(&app), original);
        assert!(
            chrome::language_segment(app.highlighting.as_ref().unwrap().language_status(&app.view)).command
                == Some("file.setType")
        );
        app.run_menu_command(None, "file.setType");
        assert!(app.language_open());
        press(&mut app, NamedKey::Escape);
        app.settings.bindings.push(kwil_core_commands::Binding {
            keys: "ctrl+l".into(),
            command: "file.setType".into(),
            context: Some(kwil_core_commands::Context::Editor),
            mode: None,
        });
        app.keys = None;
        app.mods = ModifiersState::CONTROL;
        app.key_after_settings(None, &HostKey { logical_key: Key::Character("l".into()), text: None, repeat: false });
        assert!(app.language_open(), "configured keymap reaches the same picker");
        shutdown_highlights(app.highlighting.take().unwrap());
    }

    #[test]
    fn host_rejects_stale_tab_click_and_keeps_overlay_input_until_restore() {
        use kwil_view_model::highlighting::LanguageChoice;
        let (mut app, _) = crate::search_ui::host_dispatch_tests::host("original tab");
        app.highlighting = Some(HighlightSession::new(|| {}).unwrap());
        app.dispatch_language("file.setType");
        let surface = app.overlay_surface();
        let area = Rect { x: 100, y: 100, w: 250, h: 36 };
        app.language_picker.area = Some(area);
        app.language_picker.hits.push(area, 1);
        app.language_picker.painted = Some((surface, app.language_picker.generation));
        app.pointer = (120, 110);
        app.view.file_command("file.new").unwrap();
        let new_document = app.view.active_id();
        app.language_mouse();
        assert_eq!(app.highlighting.as_ref().unwrap().language_choice(new_document), LanguageChoice::Automatic);
        app.apply(Damage { status: true, ..Default::default() });
        assert!(!app.language_open(), "host closes the invalid ticket");
        assert!(app.language_owns_input(), "old painted pixels cannot click through before restore");
        press(&mut app, NamedKey::Enter);
        assert_eq!(app.view.active_id(), new_document);
        app.language_picker.area.take();
        assert!(!app.language_owns_input());
        shutdown_highlights(app.highlighting.take().unwrap());
    }

    #[test]
    fn host_mapping_reload_and_syntax_ready_refresh_unchanged_revision_status() {
        use kwil_view_model::{highlighting::Language, language_selection::AcceptedLanguageMappings};
        let (mut app, _) = crate::search_ui::host_dispatch_tests::host("const value = 1;");
        let directory = std::env::temp_dir().join(format!(
            "kwil-language-host-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("source.custom");
        std::fs::write(&path, b"const value = 1;").unwrap();
        let file = kwil_core_text::TextFile::open(&path).unwrap();
        file.build_index(1).unwrap();
        app.view = Workspace::new(View::new(Some((path.clone(), file)))).unwrap();
        app.view.set_rows(3);
        let revision = app.view.editor().unwrap().document().revision();
        let (sender, receiver) = std::sync::mpsc::channel();
        app.highlighting = Some(
            HighlightSession::new(move || {
                let _ = sender.send(());
            })
            .unwrap(),
        );
        for language in [Language::Python, Language::Rust] {
            let accepted = AcceptedLanguageMappings::prepare(&std::collections::BTreeMap::from([(
                "custom".into(),
                language.id().into(),
            )]))
            .unwrap();
            app.pending = Damage::default();
            app.apply_language_mappings(accepted);
            assert!(app.pending.status);
            let deadline = Instant::now() + Duration::from_secs(5);
            while app.highlighting.as_ref().unwrap().detected_language(&app.view) != Some(language) {
                receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
                let damage = app.language_highlights_ready();
                assert!(damage.status, "syntax-only detection changes still damage status");
                app.apply(damage);
            }
            assert_eq!(app.highlighting.as_ref().unwrap().language_status(&app.view).label(), language.id());
            assert_eq!(app.view.editor().unwrap().document().revision(), revision);
        }
        shutdown_highlights(app.highlighting.take().unwrap());
        drop(app);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn painted_language_actions_reject_tab_epoch_surface_and_unpainted_navigation() {
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        let host = HighlightSession::new(|| {}).unwrap();
        let mut picker = LanguagePickerUi::default();
        picker.open(host.language_picker(&workspace));
        let m = chrome::Metrics::new(1.0);
        let surface = Surface::new((800, 600), &m);
        let area = Rect { x: 200, y: 200, w: 250, h: 36 };
        let point = (220, 210);
        let paint = |picker: &mut LanguagePickerUi| {
            picker.hits = Hits::default();
            picker.hits.push(area, 0);
            picker.painted = Some((surface, picker.generation));
        };
        assert_eq!(picker.hit(point, surface, &workspace), None, "no action before paint");
        paint(&mut picker);
        assert_eq!(picker.hit(point, surface, &workspace), Some(0));
        assert_eq!(picker.hit(point, Surface::new((801, 600), &m), &workspace), None, "resize before paint");
        assert_eq!(
            picker.hit(point, Surface::new((800, 600), &chrome::Metrics::new(1.5)), &workspace),
            None,
            "DPI before paint"
        );
        picker.navigate(Move::Next);
        assert_eq!(picker.hit(point, surface, &workspace), None, "selection may move the visible window");
        paint(&mut picker);
        let original = workspace.active_id();
        workspace.file_command("file.new").unwrap();
        assert_eq!(picker.hit(point, surface, &workspace), None, "same revision in another tab");
        assert!(workspace.activate(original));
        assert_eq!(picker.hit(point, surface, &workspace), None, "returning to a tab cannot revive its ticket");
        shutdown_highlights(host);
    }

    #[test]
    fn keyboard_navigation_reaches_catalogue_edges_and_close_revokes_painted_actions() {
        let workspace = Workspace::new(View::new(None)).unwrap();
        let host = HighlightSession::new(|| {}).unwrap();
        let mut picker = LanguagePickerUi::default();
        picker.open(host.language_picker(&workspace));
        assert_eq!(picker.rows.len(), picker.ticket.as_ref().unwrap().items("").count());
        picker.navigate(Move::End);
        assert_eq!(picker.selected, picker.rows.len() - 1);
        assert!(!picker.navigate(Move::Next));
        picker.navigate(Move::Previous);
        assert_eq!(picker.selected, picker.rows.len() - 2);
        picker.navigate(Move::Home);
        assert_eq!(picker.selected, 0);
        assert!(!picker.navigate(Move::Previous));
        let surface = Surface::new((800, 600), &chrome::Metrics::new(1.0));
        picker.hits.push(Rect { x: 20, y: 20, w: 100, h: 40 }, 0);
        picker.painted = Some((surface, picker.generation));
        picker.close();
        assert!(!picker.is_open());
        assert_eq!(picker.hit((30, 30), surface, &workspace), None);
        shutdown_highlights(host);
    }

    #[test]
    fn status_card_fits_surface_and_reveals_every_catalogue_row_at_monitor_scales() {
        for scale in [1.0, 1.25, 1.5, 2.0] {
            let m = chrome::Metrics::new(scale);
            for size in [(800usize, 600usize), (1280, 720), (3840, 2160), (160, 90)] {
                let anchor = Rect {
                    x: size.0.saturating_sub(m.dip(100.0)),
                    y: size.1.saturating_sub(m.status),
                    w: m.dip(100.0),
                    h: m.status,
                };
                let Some((area, visible)) = card(size, anchor, &m, 33) else { continue };
                assert!(area.x + area.w <= size.0);
                assert!(area.y + area.h < anchor.y);
                assert!(area.w as f64 / scale >= 180.0);
                assert!(visible > 0 && visible <= 10);
                for selected in 0..33 {
                    let first = desktop::reveal(0, selected, visible, 33);
                    assert!(selected >= first && selected < first + visible);
                }
            }
        }
    }
}
