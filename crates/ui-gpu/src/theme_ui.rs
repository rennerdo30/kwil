//! Theme colours and the native command picker. No parser or source I/O runs here.
use super::*;
use kwil_core_commands::theme::{Rgba, Scheme, Theme, UiRole};
use std::sync::Arc;

pub(super) fn chrome() -> Arc<Theme> {
    ACTIVE.with(|theme| theme.borrow().clone())
}

thread_local! { static ACTIVE: std::cell::RefCell<Arc<Theme>> = std::cell::RefCell::new(Arc::new(Theme::builtin(Scheme::Dark))); }

pub(super) fn select(theme: Arc<Theme>) -> bool {
    ACTIVE.with(|selected| {
        if *selected.borrow() == theme {
            return false;
        }
        selected.replace(theme);
        true
    })
}

pub(super) fn syntax_rgb(class: kwil_view_model::highlighting::Class) -> u32 {
    kwil_view_model::theme_session::syntax_rgb(&chrome(), class)
}

impl App {
    pub(super) fn dispatch_theme(&mut self, command: &str) -> bool {
        if !self.themes.command(command, self.settings_panel.as_mut()) {
            return false;
        }
        self.close_language();
        self.close_search();
        if let Some(palette) = &mut self.palette {
            let _ = palette.input(kwil_view_model::palette_session::PaletteInput::Cancel);
        }
        self.goto = None;
        self.goto_epoch += 1;
        self.menu_set(menu::State::Closed, false);
        self.drag = None;
        self.selecting = false;
        if let Some(keys) = &mut self.keys {
            keys.cancel_pending();
            keys.set_context(kwil_core_commands::Context::Palette);
        }
        self.view.set_notice(self.themes.status.clone());
        self.theme_redraw();
        true
    }
    pub(super) fn theme_redraw(&mut self) {
        self.apply_native_theme();
        self.palette_redraw();
        self.apply(Damage { status: true, ..Default::default() });
    }
    pub(super) fn theme_pointer(&mut self, accept: bool) {
        let row = self.theme_hits.iter().find_map(|(area, row)| area.contains(self.pointer).then_some(*row));
        if let Some(row) = row {
            if row == self.themes.selected() && !accept {
                return;
            }
            self.themes.select(row);
            if accept {
                self.palette_input(kwil_view_model::palette_session::PaletteInput::Accept);
            }
            self.theme_redraw();
        } else if accept && self.palette_area.is_none_or(|area| !area.contains(self.pointer)) {
            self.palette_input(kwil_view_model::palette_session::PaletteInput::Cancel);
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn draw(
    host: &kwil_view_model::theme_session::ThemeHost,
    hits: &mut Vec<(Rect, usize)>,
    pixels: &mut [u32],
    size: (usize, usize),
    font: &mut font::UiFont,
    m: &chrome::Metrics,
) -> Option<Rect> {
    hits.clear();
    if !host.is_open() {
        return None;
    }
    let c = desktop::Colors::new(&chrome());
    let s = desktop::Sizes::new(m);
    let rows = host.rows();
    let width = m.dip(620.0).min(size.0.saturating_sub(2 * s.gap));
    let top = m.menu + m.tabs + s.gap;
    let fixed = s.header + s.control + 3 * s.gap + 2 * s.footer;
    let visible = ((size.1.saturating_sub(top + fixed + s.gap)) / s.nav.max(1)).min(10).min(rows.len());
    if width < m.dip(200.0) || size.1 < top + fixed {
        return None;
    }
    let area = Rect {
        x: (size.0 - width) / 2,
        y: top,
        w: width,
        h: fixed + if host.importing() { 0 } else { visible * s.nav },
    };
    desktop::card(pixels, size.0, area, &c, &s);
    let inner = Rect { x: area.x + s.pad, y: area.y, w: area.w.saturating_sub(2 * s.pad), h: s.header };
    desktop::label(
        pixels,
        size.0,
        font,
        inner,
        if host.importing() { "Import Theme" } else { "Select Theme — live preview" },
        c.fg,
    );
    let field = Rect { y: inner.y + s.header, h: s.control, ..inner };
    desktop::text_field(
        pixels,
        size.0,
        font,
        field,
        desktop::Field {
            text: host.query(),
            cursor: host.cursor(),
            placeholder: if host.importing() {
                "Absolute theme file path"
            } else {
                "Filter built-in and imported themes"
            },
            focused: true,
            hovered: false,
            search: !host.importing(),
            clear: false,
        },
        &c,
        &s,
    );
    let first = host.selected().saturating_sub(visible.saturating_sub(1));
    if !host.importing() {
        for (index, entry) in rows.iter().enumerate().skip(first).take(visible) {
            let row = Rect { y: field.y + field.h + s.gap + (index - first) * s.nav, h: s.nav, ..inner };
            desktop::row(pixels, size.0, row, index == host.selected(), false, &c, &s);
            desktop::label(
                pixels,
                size.0,
                font,
                row,
                &format!("{}  ({})", entry.theme.name(), kwil_view_model::theme_session::name(&entry.id)),
                c.fg,
            );
            hits.push((row, index));
        }
    }
    let footer = Rect { y: area.y + area.h - 2 * s.footer, h: s.footer, ..inner };
    desktop::label(pixels, size.0, font, footer, &host.status, c.muted);
    desktop::label(
        pixels,
        size.0,
        font,
        Rect { y: footer.y + s.footer, ..footer },
        "↑ ↓ preview · Enter apply and save · Esc restore",
        c.muted,
    );
    Some(area)
}

pub(super) fn rgb(theme: &Theme, role: UiRole) -> u32 {
    let Rgba(r, g, b, _) = theme.ui().get(role);
    (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrome_palette_matches_the_painted_editor_colours() {
        // A mismatch would put a differently coloured frame around the text.
        let theme = &chrome();
        for (role, painted) in [
            (UiRole::Background, crate::bg()),
            (UiRole::Foreground, crate::fg()),
            (UiRole::GutterBackground, crate::gutter_bg()),
            (UiRole::GutterForeground, crate::gutter_fg()),
            (UiRole::StatusBackground, crate::status_bg()),
            (UiRole::StatusForeground, crate::status_fg()),
        ] {
            assert_eq!(rgb(theme, role), painted, "{role:?}");
        }
    }

    #[test]
    fn theme_commands_preview_and_cancel_through_gui_palette_input() {
        use kwil_view_model::palette_session::PaletteInput;
        let (mut app, _events) = crate::search_ui::host_dispatch_tests::host("keep this text\n");
        let before = app.view.line(0).unwrap().to_vec();
        app.palette_action(kwil_core_commands::palette::PaletteAction::Command("theme.select"));
        assert!(app.themes.is_open());
        app.pending = Damage::default();
        app.palette_input(PaletteInput::Next);
        assert_eq!(chrome().scheme(), Scheme::Light);
        assert!(app.pending.full, "an actual theme switch redraws the full editor");
        app.pending = Damage::default();
        app.palette_input(PaletteInput::Left);
        assert!(!app.pending.full, "unchanged preview does not force a full redraw");
        app.palette_input(PaletteInput::Cancel);
        assert!(!app.themes.is_open());
        assert_eq!(chrome().scheme(), Scheme::Dark);
        assert_eq!(app.view.line(0).unwrap(), before);
        app.palette_action(kwil_core_commands::palette::PaletteAction::Command("theme.import"));
        assert!(app.themes.importing());
        assert!(app.palette_open());
    }
}
