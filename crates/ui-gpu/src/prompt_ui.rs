//! File prompt card. "Open recent" is a real list of the session's recent
//! files; Open and Save as show an interim path field. That field is not the
//! GUI file chooser: native Open/Save As dialogs (KW-56) replace it, and its
//! "Browse…" button exists only when the host reports a native dialog.
//! Keys keep their existing path in `App::key` (`save_path`); clicks use the
//! same submit/cancel functions.
//!
//! The card is modal for as long as a prompt is open, painted or not: a click
//! that arrives before the first paint, while the window is too small for the
//! card or without a chrome face is consumed, never passed to tabs or text.
use super::*;
use chrome::Metrics;
use desktop::{Colors, Field, Hits, Paint, Sizes, Style, Surface, inset};
use kwil_core_commands::theme::Theme;
use kwil_view_model::FilePromptKind;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Target {
    Recent(usize),
    Submit,
    Cancel,
    Browse,
}

#[derive(Default)]
pub(super) struct View {
    pub area: Option<Rect>,
    hits: Hits<Target>,
    pub hover: Option<Target>,
    /// Prompt generation the geometry belongs to.
    generation: u64,
    surface: Option<Surface>,
    /// First Recent row shown; (visible rows, listed rows) of the last paint.
    first: usize,
    list: (usize, usize),
    /// Typed row and visible count the last reveal ran for.
    shown: Option<(Option<usize>, usize)>,
    /// Card-only repaint (hover, wheel).
    pub paint: Paint,
}

impl View {
    /// Pointer motion; true when the hovered target changed.
    fn track(&mut self, pointer: (usize, usize)) -> bool {
        let hover = self.area.filter(|a| a.contains(pointer)).and_then(|_| self.hits.at(pointer));
        if hover == self.hover {
            return false;
        }
        self.hover = hover;
        self.paint.request();
        true
    }

    /// Wheel over the Recent list: scrolls the rows, never the editor behind.
    fn wheel(&mut self, up: bool) -> bool {
        let (visible, total) = self.list;
        let next = desktop::wheel(self.first, up, visible, total);
        if next == self.first {
            return false;
        }
        self.first = next;
        self.hover = None;
        self.paint.request();
        true
    }
}

const RECENT_ROWS: usize = 10;

/// Card rectangle and visible Recent rows. Title and action rows are
/// reserved first; the list gets what is left, so ten recents never push the
/// card off a small window. Rows beyond it scroll (wheel, ↑/↓, or typing the
/// row's number).
fn layout(size: (usize, usize), m: &Metrics, s: &Sizes, recent: Option<usize>) -> Option<(Rect, usize)> {
    let w = size.0.saturating_sub(2 * s.gap).min(m.dip(600.0));
    let top = (m.menu + m.tabs + s.gap).min(size.1 / 4);
    let chrome = s.header + s.pad + s.control + s.pad;
    let (body, visible) = match recent {
        Some(count) => {
            let row_h = m.dip(44.0);
            let room = size.1.saturating_sub(top + chrome + s.gap);
            let visible = (room / row_h).min(count.clamp(1, RECENT_ROWS));
            (visible * row_h, visible)
        }
        None => (s.control + s.text + s.gap, 0),
    };
    if w < m.dip(240.0) || (recent.is_some() && visible == 0) || top + chrome + body > size.1 {
        return None;
    }
    Some((Rect { x: (size.0 - w) / 2, y: top, w, h: chrome + body }, visible))
}

/// Zero-based Recent row named by the typed number, if it is listed.
fn typed(text: &str, count: usize) -> Option<usize> {
    text.trim().parse::<usize>().ok().and_then(|n| n.checked_sub(1)).filter(|&index| index < count)
}

/// ↑/↓ moves the typed number through the listed rows; returns the new number.
fn step(text: &str, count: usize, up: bool) -> Option<usize> {
    if count == 0 {
        return None;
    }
    let index = match typed(text, count) {
        Some(index) if up => index.saturating_sub(1),
        Some(index) => (index + 1).min(count - 1),
        None if up => count - 1,
        None => 0,
    };
    Some(index + 1)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn draw(
    view_model: &Workspace,
    text: &str,
    pixels: &mut [u32],
    size: (usize, usize),
    font: &mut font::UiFont,
    theme: &Theme,
    m: &Metrics,
    native_dialog: bool,
    view: &mut View,
) -> Option<Rect> {
    view.hits = Hits::default();
    view.area = None;
    let prompt = view_model.file_prompt()?;
    let recent: Vec<&Path> = view_model.session().recent().iter().take(RECENT_ROWS).map(PathBuf::as_path).collect();
    draw_card(prompt.kind, prompt.generation, &recent, text, pixels, size, font, theme, m, native_dialog, view)
}

#[allow(clippy::too_many_arguments)]
fn draw_card(
    kind: FilePromptKind,
    generation: u64,
    recent: &[&Path],
    text: &str,
    pixels: &mut [u32],
    size: (usize, usize),
    font: &mut font::UiFont,
    theme: &Theme,
    m: &Metrics,
    native_dialog: bool,
    view: &mut View,
) -> Option<Rect> {
    view.hits = Hits::default();
    view.area = None;
    let c = Colors::new(theme);
    let s = Sizes::new(m);
    let row_h = m.dip(44.0);
    let listing = matches!(kind, FilePromptKind::Recent).then_some(recent.len());
    let (area, visible) = layout(size, m, &s, listing)?;
    if view.generation != generation {
        // A new prompt starts at the top of its list.
        (view.first, view.shown, view.hover) = (0, None, None);
    }
    view.area = Some(area);
    view.generation = generation;
    view.surface = Some(Surface::new(size, m));
    let stride = size.0;
    desktop::card(pixels, stride, area, &c, &s);
    let inner = inset(area, s.pad);
    let title = match kind {
        FilePromptKind::Open => "Open by path",
        FilePromptKind::SaveAs => "Save as path",
        FilePromptKind::Recent => "Open recent",
    };
    let pen = desktop::label(pixels, stride, font, Rect { h: s.text, ..inner }, title, c.fg);
    let mut y = inner.y + s.header - s.pad;
    let chosen = typed(text, recent.len()).filter(|_| matches!(kind, FilePromptKind::Recent));
    match kind {
        FilePromptKind::Recent => {
            // Keyboard route, stated on the card: the typed number is the row.
            let (hint, color) = if !text.trim().is_empty() && chosen.is_none() {
                (format!("No recent file {}", desktop::visible(text.trim(), 16)), c.error)
            } else {
                ("Type a number or use ↑ ↓, then Enter".to_owned(), c.faint)
            };
            let hint_rect =
                Rect { x: pen + s.gap, y: inner.y, w: (inner.x + inner.w).saturating_sub(pen + s.gap), h: s.text };
            desktop::label_right(pixels, stride, font, hint_rect, &hint, color);
            if recent.is_empty() {
                desktop::label(pixels, stride, font, Rect { y, h: s.text, ..inner }, "No recent files yet.", c.muted);
            }
            if view.shown != Some((chosen, visible)) {
                if let Some(index) = chosen {
                    view.first = desktop::reveal(view.first, index, visible, recent.len());
                }
                view.shown = Some((chosen, visible));
            }
            view.first = view.first.min(recent.len().saturating_sub(visible));
            view.list = (visible, recent.len());
            let scrolling = recent.len() > visible;
            let list = Rect { x: inner.x - s.gap, y, w: inner.w + 2 * s.gap, h: visible * row_h };
            let rows_w = if scrolling { list.w.saturating_sub(s.thumb + s.gap) } else { list.w };
            let number_w = font.measure("10", list.w) + s.gap;
            for (index, path) in recent.iter().enumerate().skip(view.first).take(visible) {
                let r = Rect { x: list.x, y, w: rows_w, h: row_h };
                let target = Target::Recent(index + 1);
                desktop::row(pixels, stride, r, chosen == Some(index), view.hover == Some(target), &c, &s);
                let number = Rect { x: r.x + s.pad, w: number_w, ..r };
                desktop::label(pixels, stride, font, number, &(index + 1).to_string(), c.muted);
                let name =
                    path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
                let folder = path.parent().map(|p| p.display().to_string()).unwrap_or_default();
                let left = number.x + number.w;
                let line = Rect { x: left, y: r.y + s.gap / 2, w: (r.x + r.w).saturating_sub(left + s.pad), h: s.text };
                desktop::label(pixels, stride, font, line, &name, c.fg);
                desktop::label(pixels, stride, font, Rect { y: line.y + s.text - s.gap / 4, ..line }, &folder, c.faint);
                view.hits.push(r, target);
                y += row_h;
            }
            desktop::scroll_thumb(
                pixels,
                stride,
                Rect { x: list.x + rows_w, w: list.w - rows_w, ..list },
                view.first,
                visible,
                recent.len(),
                &c,
                &s,
            );
        }
        _ => {
            let browse_w = if native_dialog { desktop::button_width(font, "Browse…", &s) + s.gap } else { 0 };
            let field = Rect { x: inner.x, y, w: inner.w.saturating_sub(browse_w), h: s.control };
            desktop::text_field(
                pixels,
                stride,
                font,
                field,
                Field {
                    text,
                    cursor: text.len(),
                    placeholder: "Full path",
                    focused: true,
                    hovered: false,
                    search: false,
                    clear: false,
                },
                &c,
                &s,
            );
            if native_dialog {
                let b = Rect { x: field.x + field.w + s.gap, w: browse_w - s.gap, ..field };
                desktop::button(
                    pixels,
                    stride,
                    font,
                    b,
                    "Browse…",
                    Style::Secondary,
                    view.hover == Some(Target::Browse),
                    &c,
                    &s,
                );
                view.hits.push(b, Target::Browse);
            }
            y += s.control + s.gap / 2;
            let note = "Type or paste a full path. Enter confirms, Esc cancels.";
            desktop::label(pixels, stride, font, Rect { y, h: s.text, ..inner }, note, c.faint);
        }
    }
    let buttons = Rect { y: (area.y + area.h).saturating_sub(s.pad + s.control), h: s.control, ..inner };
    let mut x = buttons.x + buttons.w;
    let primary = match kind {
        // Enter opens the typed row; the button says so once there is one.
        FilePromptKind::Recent => chosen.map(|_| "Open"),
        FilePromptKind::Open => Some("Open"),
        FilePromptKind::SaveAs => Some("Save"),
    };
    if let Some(text) = primary {
        let bw = desktop::button_width(font, text, &s);
        x = x.saturating_sub(bw);
        let r = Rect { x, w: bw, ..buttons };
        desktop::button(pixels, stride, font, r, text, Style::Primary, view.hover == Some(Target::Submit), &c, &s);
        view.hits.push(r, Target::Submit);
        x = x.saturating_sub(s.gap);
    }
    let bw = desktop::button_width(font, "Cancel", &s);
    let r = Rect { x: x.saturating_sub(bw), w: bw, ..buttons };
    desktop::button(pixels, stride, font, r, "Cancel", Style::Secondary, view.hover == Some(Target::Cancel), &c, &s);
    view.hits.push(r, Target::Cancel);
    Some(area)
}

/// What a click means while a prompt is open. Every outcome is consumed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PromptClick {
    /// No geometry for this prompt and surface yet: repaint, act on nothing.
    Repaint,
    /// Outside the card or on no control: modal, nothing happens.
    Consumed,
    Act(Target),
}

fn prompt_click(view: &View, generation: u64, surface: Surface, pointer: (usize, usize)) -> PromptClick {
    let Some(area) = view.area else { return PromptClick::Repaint };
    if view.generation != generation || view.surface != Some(surface) {
        return PromptClick::Repaint;
    }
    if !area.contains(pointer) {
        return PromptClick::Consumed;
    }
    view.hits.at(pointer).map_or(PromptClick::Consumed, PromptClick::Act)
}

impl App {
    /// Left click while a file prompt is open; true whenever a prompt is open,
    /// so tabs, menus and text never see a click meant for the modal card.
    pub(super) fn prompt_mouse(&mut self) -> bool {
        let Some(generation) = self.view.file_prompt().map(|prompt| prompt.generation) else { return false };
        match prompt_click(&self.prompt_view, generation, self.overlay_surface(), self.pointer) {
            PromptClick::Repaint => self.palette_redraw(),
            PromptClick::Consumed => {}
            PromptClick::Act(Target::Recent(number)) => {
                self.save_path = Some(number.to_string());
                self.prompt_submit();
            }
            PromptClick::Act(Target::Submit) => self.prompt_submit(),
            PromptClick::Act(Target::Cancel) => {
                file_prompt::cancel(&mut self.view);
                self.sync_file_ui();
                self.update_file_prompt();
            }
            PromptClick::Act(Target::Browse) => self.prompt_browse(),
        }
        true
    }

    /// Wheel while a file prompt is open (LIB-HOOKS-R1 hunk 6): scrolls the
    /// Recent list; consumed either way, the editor behind a modal stays put.
    pub(super) fn prompt_wheel(&mut self, up: bool) -> bool {
        if self.view.file_prompt().is_none() {
            return false;
        }
        if self.prompt_view.wheel(up) {
            self.request_frame();
        }
        true
    }

    /// ↑/↓ in "Open recent" (LIB-HOOKS-R1 hunk 7): steps the typed number
    /// that Enter submits; the card reveals and highlights that row. The
    /// caller's `update_file_prompt` repaints.
    pub(super) fn prompt_step(&mut self, up: bool) {
        if !self.view.file_prompt().is_some_and(|prompt| prompt.kind == FilePromptKind::Recent) {
            return;
        }
        let count = self.view.session().recent().len().min(RECENT_ROWS);
        if let Some(text) = self.save_path.as_mut()
            && let Some(number) = step(text, count, up)
        {
            *text = number.to_string();
        }
    }

    /// Same sequence as Enter in `App::key`.
    fn prompt_submit(&mut self) {
        self.update_file_prompt();
        let damage = file_prompt::submit(&mut self.view);
        self.sync_file_ui();
        self.apply(damage);
    }

    /// KW-56 native dialog entry. `draw` shows Browse… only when the host
    /// passes `native_dialog`, so this stub is unreachable until ROOT's dialog
    /// work replaces its body.
    fn prompt_browse(&mut self) {}

    /// Hover repaints only the card (`Paint`, no body restore).
    pub(super) fn prompt_track(&mut self) {
        if self.prompt_view.track(self.pointer) {
            self.request_frame();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ten_recents_fit_small_windows_and_every_row_stays_reachable() {
        // R2: 800x600 at 100 % (and the same physical window at 150 %) used to
        // need 629 px and drew nothing.
        for scale in [1.0, 1.25, 1.5, 2.0] {
            let m = Metrics::new(scale);
            let s = Sizes::new(&m);
            let logical = ((800.0 * scale) as usize, (600.0 * scale) as usize);
            for size in [logical, (800, 600), (640, 480)] {
                let (area, visible) = layout(size, &m, &s, Some(RECENT_ROWS))
                    .unwrap_or_else(|| panic!("{size:?} x{scale}: no Recent card"));
                assert!(
                    area.y + area.h <= size.1 && area.x + area.w <= size.0,
                    "{size:?} x{scale}: card leaves the window"
                );
                assert!((1..=RECENT_ROWS).contains(&visible));
                // Every listed row can be brought into view by the wheel …
                let mut view = View { list: (visible, RECENT_ROWS), ..View::default() };
                while view.wheel(false) {}
                assert_eq!(view.first + visible, RECENT_ROWS, "{size:?} x{scale}: the last row is reachable");
                // … and by the keyboard: ↑/↓ steps the number Enter submits.
                let mut text = String::new();
                let mut seen = Vec::new();
                for _ in 0..RECENT_ROWS {
                    text = step(&text, RECENT_ROWS, false).unwrap().to_string();
                    seen.push(typed(&text, RECENT_ROWS).unwrap());
                }
                assert_eq!(seen, (0..RECENT_ROWS).collect::<Vec<_>>());
            }
        }
        let m = Metrics::new(1.0);
        let (_, visible) = layout((800, 600), &m, &Sizes::new(&m), Some(RECENT_ROWS)).expect("800x600 shows Recent");
        assert!(visible < RECENT_ROWS, "800x600 really needs scrolling: {visible}");
        assert_eq!(step("1", 10, true), Some(1), "↑ stops at the first row");
        assert_eq!(step("10", 10, false), Some(10), "↓ stops at the last row");
        assert_eq!(step("", 10, true), Some(10));
        assert_eq!(step("x", 0, false), None);
    }

    #[test]
    fn prompt_clicks_are_consumed_before_the_first_paint_and_on_stale_geometry() {
        // R2: an unpainted prompt used to return "not consumed", so the click
        // reached tab actions and editor pointer handling.
        let m = Metrics::new(1.0);
        let surface = Surface::new((800, 600), &m);
        let unpainted = View::default();
        assert_eq!(prompt_click(&unpainted, 3, surface, (400, 300)), PromptClick::Repaint);
        let mut view = View {
            area: Some(Rect { x: 100, y: 73, w: 600, h: 300 }),
            generation: 3,
            surface: Some(surface),
            ..View::default()
        };
        view.hits.push(Rect { x: 108, y: 125, w: 584, h: 44 }, Target::Recent(1));
        assert_eq!(prompt_click(&view, 3, surface, (200, 140)), PromptClick::Act(Target::Recent(1)));
        assert_eq!(prompt_click(&view, 3, surface, (10, 10)), PromptClick::Consumed, "tab strip under a modal");
        assert_eq!(prompt_click(&view, 3, surface, (400, 360)), PromptClick::Consumed, "card background");
        assert_eq!(prompt_click(&view, 4, surface, (200, 140)), PromptClick::Repaint, "another prompt");
        let resized = Surface::new((800, 300), &m);
        assert_eq!(prompt_click(&view, 3, resized, (200, 140)), PromptClick::Repaint, "resized before repaint");
    }

    #[test]
    #[cfg(windows)]
    fn recent_card_paints_a_bounded_navigable_list_at_800x600() {
        let paths: Vec<PathBuf> = (1..=RECENT_ROWS).map(|n| PathBuf::from(format!("E:/work/notes-{n}.md"))).collect();
        let recent: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
        for (size, scale) in [((800, 600), 1.0), ((1200, 900), 1.5), ((800, 600), 1.5)] {
            let m = Metrics::new(scale);
            let mut font = font::UiFont::load(m.text_px()).expect("Windows chrome face");
            let mut pixels = vec![0u32; size.0 * size.1];
            let mut view = View::default();
            let mut paint = |text: &str, view: &mut View| {
                let area = draw_card(
                    FilePromptKind::Recent,
                    7,
                    &recent,
                    text,
                    &mut pixels,
                    size,
                    &mut font,
                    &theme_ui::chrome(),
                    &m,
                    false,
                    view,
                )
                .expect("ten recents fit");
                let rows: Vec<usize> = view
                    .hits
                    .iter()
                    .filter_map(|(rect, target)| {
                        assert!(area.contains((rect.x, rect.y)));
                        assert!(
                            area.contains((rect.x + rect.w - 1, rect.y + rect.h - 1)),
                            "{target:?} leaves the card"
                        );
                        match target {
                            Target::Recent(n) => Some(*n),
                            _ => None,
                        }
                    })
                    .collect();
                (area, rows)
            };
            let (area, first) = paint("", &mut view);
            assert!(area.y + area.h <= size.1);
            assert_eq!(first.first(), Some(&1));
            // Typing (or stepping to) the last number reveals it.
            let (_, last) = paint("10", &mut view);
            assert!(last.contains(&10), "{size:?} x{scale}: typed row revealed: {last:?}");
            assert!(view.hits.iter().any(|(_, t)| *t == Target::Submit), "a typed row offers Open");
            let cancel = view.hits.iter().find_map(|(r, t)| (*t == Target::Cancel).then_some(*r)).unwrap();
            for (rect, target) in view.hits.iter() {
                if matches!(target, Target::Recent(_)) {
                    assert!(rect.y + rect.h <= cancel.y, "rows run into the action row");
                }
            }
        }
    }

    #[test]
    fn hover_and_wheel_request_card_repaints_once() {
        let mut view = View { area: Some(Rect { x: 0, y: 0, w: 100, h: 100 }), list: (3, 10), ..View::default() };
        view.hits.push(Rect { x: 0, y: 0, w: 100, h: 20 }, Target::Recent(1));
        assert!(view.track((5, 5)));
        assert!(view.paint.take());
        assert!(!view.track((6, 6)), "same hover: no repaint");
        assert!(view.wheel(false) && view.paint.take(), "a wheel step repaints the card");
        assert_eq!(view.hover, None, "rows moved under the pointer");
        while view.wheel(false) {}
        view.paint.take();
        assert!(!view.wheel(false) && !view.paint.pending(), "the end of the list asks for nothing");
    }
}
