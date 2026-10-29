//! Command palette and file finder card: a query field with a mode-aware
//! placeholder, result rows with categories and real shortcut keycaps, and a
//! quiet hint line. Text is the DPI chrome face; editor zoom never applies.
//! Matching, discovery and actions stay in the palette host (`palette_ui`).
use super::*;
use chrome::Metrics;
use desktop::{Colors, Field, Hits, Paint, Sizes, Surface, inset};
use kwil_core_commands::palette::{Palette, PaletteAction, PaletteMode, PaletteRow};
use kwil_core_commands::theme::Theme;
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hit {
    Row(usize),
    Clear,
}

/// Geometry of the last paint; clicks resolve against exactly these rows.
#[derive(Default)]
pub(super) struct View {
    pub area: Option<Rect>,
    hits: Hits<Hit>,
    pub hover: Option<usize>,
    first: usize,
    /// Selection and visible row count the last reveal ran for: a resize or
    /// DPI change that shrinks the list reveals the unchanged selection too.
    shown: Option<(usize, usize)>,
    /// Actions of the painted rows, from `first`. A click acts only when the
    /// model still holds the painted action at that index, so a same-length
    /// result replacement that has not been painted yet can never execute a
    /// row the user did not see.
    painted: Vec<PaletteAction>,
    surface: Option<Surface>,
    /// Card-only repaint (hover).
    pub paint: Paint,
}

impl View {
    fn painted_action(&self, row: usize) -> Option<&PaletteAction> {
        row.checked_sub(self.first).and_then(|slot| self.painted.get(slot))
    }

    /// Pointer motion; true when the hovered row changed and the card asked
    /// for a repaint.
    fn track(&mut self, pointer: (usize, usize), rows: &[PaletteRow], surface: Surface) -> bool {
        let hover = match click(self, pointer, rows, surface) {
            Click::Row(row) => Some(row),
            _ => None,
        };
        if hover == self.hover {
            return false;
        }
        self.hover = hover;
        self.paint.request();
        true
    }
}

/// Query the clear button restores: the mode prefix stays, so clearing a
/// command search keeps the command palette in command mode.
pub(super) fn cleared_query(query: &str) -> &'static str {
    match query.as_bytes().first() {
        Some(b'>') => ">",
        Some(b':') => ":",
        Some(b'#') => "#",
        _ => "",
    }
}

/// Top of the card: below the menu and tab strip, as an anchored command bar.
fn card(size: (usize, usize), m: &Metrics, s: &Sizes, rows: usize) -> Option<(Rect, usize)> {
    let w = size.0.saturating_sub(2 * s.gap).min(m.dip(640.0));
    let top = (m.menu + m.tabs + s.gap).min(size.1 / 4);
    let row_h = m.dip(36.0);
    let chrome = s.pad + s.control + s.gap + s.gap + s.footer;
    let room = size.1.saturating_sub(top + s.gap + chrome);
    let visible = (room / row_h).min(12).min(rows.max(1)).max(1);
    let h = chrome + visible * row_h;
    if w < m.dip(200.0) || top + h > size.1 {
        return None;
    }
    Some((Rect { x: (size.0 - w) / 2, y: top, w, h }, visible))
}

fn placeholder(mode: PaletteMode) -> &'static str {
    match mode {
        PaletteMode::Commands => "Type a command",
        PaletteMode::Line => "Line number, or line:column",
        PaletteMode::Tabs => "Switch to an open tab",
        PaletteMode::Files => "Search files by name",
    }
}

/// `labels` maps command ids to their current effective shortcut (the menu's
/// label table), so keycaps follow user overrides and tombstones.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw(
    palette: &Palette,
    pixels: &mut [u32],
    size: (usize, usize),
    font: &mut font::UiFont,
    theme: &Theme,
    m: &Metrics,
    labels: &HashMap<&'static str, String>,
    view: &mut View,
) -> Option<Rect> {
    view.hits = Hits::default();
    view.painted.clear();
    view.area = None;
    if !palette.is_open() {
        return None;
    }
    let c = Colors::new(theme);
    let s = Sizes::new(m);
    let rows = palette.rows();
    let (area, visible) = card(size, m, &s, rows.len())?;
    view.area = Some(area);
    view.surface = Some(Surface::new(size, m));
    let stride = size.0;
    desktop::card(pixels, stride, area, &c, &s);
    let inner = inset(area, s.hairline);
    let field = Rect { x: inner.x + s.gap, y: inner.y + s.gap, w: inner.w - 2 * s.gap, h: s.control };
    // The clear button empties the query after its mode prefix.
    let clear = palette.query().len() > cleared_query(palette.query()).len();
    desktop::text_field(
        pixels,
        stride,
        font,
        field,
        Field {
            text: palette.query(),
            cursor: palette.cursor(),
            placeholder: placeholder(palette.mode()),
            focused: true,
            hovered: false,
            search: true,
            clear,
        },
        &c,
        &s,
    );
    if clear {
        view.hits.push(desktop::field_clear(field, &s), Hit::Clear);
    }
    let list_top = field.y + field.h + s.gap;
    desktop::hline(pixels, stride, inner.x, list_top, inner.w, &c, &s);
    let row_h = m.dip(36.0);
    let selected = palette.selected().unwrap_or(0);
    if view.shown != Some((selected, visible)) {
        view.first = desktop::reveal(view.first, selected, visible, rows.len());
        view.shown = Some((selected, visible));
    }
    view.first = view.first.min(rows.len().saturating_sub(visible));
    let list = Rect { x: inner.x + s.gap / 2, y: list_top + s.gap / 2, w: inner.w - s.gap, h: visible * row_h };
    if rows.is_empty() {
        let text = if palette.results_are_fresh() { "No matches" } else { "Searching…" };
        let r = Rect { x: list.x + s.pad, y: list.y, w: list.w, h: row_h };
        desktop::label(pixels, stride, font, r, text, c.muted);
    }
    for (slot, row) in rows.iter().enumerate().skip(view.first).take(visible) {
        let r = Rect { x: list.x, y: list.y + (slot - view.first) * row_h, w: list.w, h: row_h };
        let chosen = palette.selected() == Some(slot);
        desktop::row(pixels, stride, r, chosen, view.hover == Some(slot), &c, &s);
        view.hits.push(r, Hit::Row(slot));
        view.painted.push(row.action.clone());
        let text = Rect { x: r.x + s.pad, w: r.w.saturating_sub(2 * s.pad), ..r };
        // Right side: shortcut keycaps for commands, else the category.
        let keys = match row.action {
            PaletteAction::Command(id) => labels.get(id).filter(|keys| !keys.is_empty()),
            _ => None,
        };
        let right_w = match keys {
            Some(keys) => desktop::keycaps_width(font, keys, &s, text.w / 3),
            None => font.measure(&row.category, text.w / 3),
        };
        let title = Rect { w: text.w.saturating_sub(right_w + s.pad), ..text };
        desktop::label(pixels, stride, font, title, &row.title, c.fg);
        let right = Rect { x: text.x + text.w - right_w, w: right_w, ..text };
        match keys {
            Some(keys) => {
                desktop::keycaps(pixels, stride, font, right, right.x, keys, &c, &s);
            }
            None => {
                desktop::label_right(pixels, stride, font, right, &row.category, c.faint);
            }
        }
    }
    desktop::scroll_thumb(
        pixels,
        stride,
        Rect { x: list.x + list.w - s.thumb, w: s.thumb, ..list },
        view.first,
        visible,
        rows.len(),
        &c,
        &s,
    );
    let footer =
        Rect { x: inner.x + s.pad, y: list.y + list.h + s.gap / 2, w: inner.w - 2 * s.pad, h: s.footer - s.gap };
    let hint = if palette.results_are_fresh() {
        "↑↓  Navigate     Enter  Open     Esc  Close"
    } else {
        "Searching…     Esc  Close"
    };
    desktop::label(pixels, stride, font, footer, hint, c.faint);
    Some(area)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Click {
    Outside,
    Inside,
    /// Geometry from another window size or monitor scale: repaint, act on nothing.
    Stale,
    Row(usize),
    Clear,
}

/// What a click at `pointer` means against the last painted geometry. A row
/// acts only if the model still holds the action that was painted there.
pub(super) fn click(view: &View, pointer: (usize, usize), rows: &[PaletteRow], surface: Surface) -> Click {
    let Some(area) = view.area else { return Click::Outside };
    if view.surface != Some(surface) {
        return Click::Stale;
    }
    if !area.contains(pointer) {
        return Click::Outside;
    }
    match view.hits.at(pointer) {
        Some(Hit::Clear) => Click::Clear,
        Some(Hit::Row(row))
            if view.painted_action(row).is_some_and(|painted| rows.get(row).is_some_and(|r| r.action == *painted)) =>
        {
            Click::Row(row)
        }
        _ => Click::Inside,
    }
}

impl App {
    /// Replaces the cell-grid mouse mapping of `palette_mouse`.
    pub(super) fn palette_pointer(&mut self) {
        use kwil_view_model::palette_session::PaletteInput;
        let surface = self.overlay_surface();
        let Some(palette) = &mut self.palette else { return };
        match click(&self.palette_view, self.pointer, palette.model().rows(), surface) {
            Click::Outside => self.palette_input(PaletteInput::Cancel),
            Click::Inside => {}
            Click::Stale => self.palette_redraw(),
            Click::Clear => self.palette_clear(),
            Click::Row(target) => {
                let selected = palette.model().selected().unwrap_or(0);
                for _ in selected..target {
                    let _ = palette.input(PaletteInput::Next);
                }
                for _ in target..selected {
                    let _ = palette.input(PaletteInput::Previous);
                }
                self.palette_input(PaletteInput::Accept);
            }
        }
    }

    /// The query field's clear button: one replacement of the query by its
    /// mode prefix through the session's own `open`, which advances the input
    /// epoch (late IME commits are fenced) and requests fresh results. The
    /// palette stays open and no row is accepted.
    fn palette_clear(&mut self) {
        let Some(palette) = &mut self.palette else { return };
        let query = cleared_query(palette.model().query());
        let result = palette.open(query);
        if self.ime_destination.is_some() || self.composing {
            self.ime_destination = Some(ime::Destination::Cancelled);
            self.composing = false;
        }
        if let Err(error) = result {
            let damage = self.view.set_notice(format!("Palette: {error:?}"));
            self.apply(damage);
        }
        self.palette_redraw();
    }

    /// Hover repaints only the card (`Paint`, no body restore).
    pub(super) fn palette_track(&mut self) {
        if self.themes.is_open() {
            self.theme_pointer(false);
            return;
        }
        let surface = self.overlay_surface();
        let Some(palette) = &self.palette else { return };
        if self.palette_view.track(self.pointer, palette.model().rows(), surface) {
            self.request_frame();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_card_fits_every_window_and_follows_dpi_not_zoom() {
        for scale in [1.0, 1.5, 2.0] {
            let m = Metrics::new(scale);
            let s = Sizes::new(&m);
            for size in [(400, 300), (800, 600), (3840, 2160), (1200, 260)] {
                if let Some((area, visible)) = card(size, &m, &s, 500) {
                    assert!(area.x + area.w <= size.0 && area.y + area.h <= size.1);
                    assert!((1..=12).contains(&visible));
                }
            }
        }
        let (a, b) = (Metrics::new(1.0), Metrics::new(2.0));
        let one = card((1600, 1000), &a, &Sizes::new(&a), 3).unwrap().0;
        let two = card((3200, 2000), &b, &Sizes::new(&b), 3).unwrap().0;
        assert_eq!((one.w * 2, one.h * 2), (two.w, two.h));
    }

    fn row(title: &str, action: PaletteAction) -> PaletteRow {
        PaletteRow { title: title.into(), category: "Test".into(), action }
    }

    fn commands(ids: &[&'static str]) -> Vec<PaletteRow> {
        ids.iter().map(|&id| row(id, PaletteAction::Command(id))).collect()
    }

    /// Installs `rows` as fresh results for `request`, exactly as the
    /// worker's `PaletteReady` path does (`apply_results`).
    #[cfg(windows)]
    fn install(palette: &mut Palette, request: &kwil_core_commands::palette::PaletteQuery, rows: Vec<PaletteRow>) {
        let total = rows.len();
        let results = kwil_core_commands::palette::PaletteResults {
            generation: request.generation,
            mode: palette.mode(),
            query: request.query.clone(),
            rows: rows.into(),
            total,
        };
        assert!(palette.apply_results(std::sync::Arc::new(results)), "results match the open query");
    }

    /// An open command palette holding `rows` as fresh results.
    #[cfg(windows)]
    fn palette_with(rows: Vec<PaletteRow>) -> Palette {
        let mut palette = Palette::new(64);
        let request = palette.open(">").unwrap().expect("command mode asks the worker");
        install(&mut palette, &request, rows);
        palette
    }

    #[test]
    fn clicks_resolve_only_against_the_painted_rows() {
        let m = Metrics::new(1.0);
        let surface = Surface::new((800, 600), &m);
        let painted = commands(&["file.open", "file.save", "file.close"]);
        let mut view = View::default();
        assert_eq!(click(&view, (5, 5), &painted, surface), Click::Outside);
        view.area = Some(Rect { x: 0, y: 0, w: 100, h: 100 });
        view.surface = Some(surface);
        view.painted = painted.iter().map(|r| r.action.clone()).collect();
        view.hits.push(Rect { x: 0, y: 40, w: 100, h: 20 }, Hit::Row(2));
        view.hits.push(Rect { x: 80, y: 0, w: 20, h: 20 }, Hit::Clear);
        assert_eq!(click(&view, (10, 45), &painted, surface), Click::Row(2));
        // Same length, different rows (review R4): nothing executes.
        let replaced = commands(&["view.zoomIn", "view.zoomOut", "file.quit"]);
        assert_eq!(click(&view, (10, 45), &replaced, surface), Click::Inside, "results replaced since the paint");
        assert_eq!(click(&view, (10, 45), &painted[..2], surface), Click::Inside, "results shrank since the paint");
        assert_eq!(click(&view, (10, 5), &painted, surface), Click::Inside, "the query field is not a row");
        assert_eq!(click(&view, (85, 5), &painted, surface), Click::Clear);
        assert_eq!(click(&view, (150, 45), &painted, surface), Click::Outside);
        let resized = Surface::new((800, 300), &m);
        assert_eq!(click(&view, (10, 45), &painted, resized), Click::Stale, "geometry from another window size");
        let rescaled = Surface::new((800, 600), &Metrics::new(1.5));
        assert_eq!(click(&view, (10, 45), &painted, rescaled), Click::Stale, "geometry from another monitor scale");
    }

    #[test]
    fn clearing_keeps_the_palette_mode_and_empties_only_the_search_text() {
        for (query, mode) in [
            (">sav", PaletteMode::Commands),
            (":12", PaletteMode::Line),
            ("#read", PaletteMode::Tabs),
            ("main.rs", PaletteMode::Files),
        ] {
            let mut palette = Palette::new(64);
            palette.open(query).unwrap();
            let cleared = cleared_query(palette.query());
            assert!(cleared.len() < query.len(), "{query}: the clear button is offered");
            // `palette_clear` applies exactly this through `PaletteSession::open`.
            palette.open(cleared).unwrap();
            assert!(palette.is_open(), "{query}: clearing never closes the palette");
            assert_eq!(palette.mode(), mode, "{query}: clearing keeps the mode");
            assert_eq!((palette.query(), palette.cursor()), (cleared, cleared.len()));
            assert_eq!(palette.selected(), None, "{query}: clearing accepts and selects nothing");
            assert!(palette.query().len() <= cleared_query(palette.query()).len(), "nothing is left to clear");
        }
    }

    #[test]
    #[cfg(windows)]
    fn painted_rows_survive_same_length_replacement_only_by_identity() {
        // R4: paint set A, install same-length set B without a repaint, click an A row.
        let m = Metrics::new(1.0);
        let size = (800, 600);
        let mut font = font::UiFont::load(m.text_px()).expect("Windows chrome face");
        let mut palette = palette_with(commands(&["file.open", "file.save", "file.close"]));
        let mut view = View::default();
        let mut pixels = vec![0u32; size.0 * size.1];
        let labels = HashMap::new();
        draw(&palette, &mut pixels, size, &mut font, &theme_ui::chrome(), &m, &labels, &mut view).expect("card fits");
        let rect = view.hits.iter().find_map(|(rect, hit)| (*hit == Hit::Row(1)).then_some(*rect)).expect("row 1");
        let point = (rect.x + rect.w / 2, rect.y + rect.h / 2);
        let surface = Surface::new(size, &m);
        assert_eq!(click(&view, point, palette.rows(), surface), Click::Row(1));
        let request = palette.open(">").unwrap().unwrap();
        install(&mut palette, &request, commands(&["view.zoomIn", "view.zoomOut", "file.quit"]));
        assert_eq!(palette.rows().len(), 3, "same count as the painted set");
        assert_eq!(click(&view, point, palette.rows(), surface), Click::Inside, "an unpainted B row never runs");
        // After the repaint the B row is what the user sees and may run.
        draw(&palette, &mut pixels, size, &mut font, &theme_ui::chrome(), &m, &labels, &mut view).unwrap();
        assert_eq!(click(&view, point, palette.rows(), surface), Click::Row(1));
        assert_eq!(view.painted_action(1), Some(&PaletteAction::Command("view.zoomOut")));
    }

    #[test]
    #[cfg(windows)]
    fn resizing_keeps_an_unchanged_selection_among_the_painted_rows() {
        // R7: select row 9 of 14 at 800x800 (12 rows), shrink to 800x300 (3 rows),
        // grow back and change monitor scale; Enter's row stays painted.
        let ids =
            ["a.0", "a.1", "a.2", "a.3", "a.4", "a.5", "a.6", "a.7", "a.8", "a.9", "a.10", "a.11", "a.12", "a.13"];
        let mut palette = palette_with(commands(&ids));
        for _ in 0..9 {
            palette.next();
        }
        assert_eq!(palette.selected(), Some(9));
        let mut view = View::default();
        let labels = HashMap::new();
        let mut counts = Vec::new();
        for (size, scale) in
            [((800, 800), 1.0), ((800, 300), 1.0), ((800, 800), 1.0), ((1200, 450), 1.5), ((800, 300), 1.0)]
        {
            let m = Metrics::new(scale);
            let mut font = font::UiFont::load(m.text_px()).expect("Windows chrome face");
            let mut pixels = vec![0u32; size.0 * size.1];
            draw(&palette, &mut pixels, size, &mut font, &theme_ui::chrome(), &m, &labels, &mut view)
                .expect("card fits");
            let rows: Vec<usize> = view
                .hits
                .iter()
                .filter_map(|(_, hit)| match hit {
                    Hit::Row(row) => Some(*row),
                    Hit::Clear => None,
                })
                .collect();
            assert!(rows.contains(&9), "selected row 9 not painted at {size:?} x{scale}: {rows:?}");
            counts.push(rows.len());
        }
        assert!(counts[1] < counts[0], "the short window really paints fewer rows: {counts:?}");
    }

    #[test]
    #[cfg(windows)]
    fn hover_asks_for_one_card_repaint_per_change() {
        // R1: hover reaches render admission through `Paint`; an unchanged hover never repeats it.
        let m = Metrics::new(1.0);
        let size = (800, 600);
        let mut font = font::UiFont::load(m.text_px()).expect("Windows chrome face");
        let palette = palette_with(commands(&["file.open", "file.save"]));
        let mut view = View::default();
        let mut pixels = vec![0u32; size.0 * size.1];
        draw(&palette, &mut pixels, size, &mut font, &theme_ui::chrome(), &m, &HashMap::new(), &mut view).unwrap();
        let rect = view.hits.iter().find_map(|(rect, hit)| (*hit == Hit::Row(1)).then_some(*rect)).unwrap();
        let point = (rect.x + 4, rect.y + rect.h / 2);
        let surface = Surface::new(size, &m);
        assert!(view.track(point, palette.rows(), surface));
        assert_eq!(view.hover, Some(1));
        assert!(view.paint.take(), "a hover change is admitted as a card repaint");
        assert!(!view.track(point, palette.rows(), surface), "an unchanged hover asks for nothing");
        assert!(!view.paint.pending(), "no idle repeat");
        assert!(view.track((0, 0), palette.rows(), surface), "leaving the rows clears the hover");
        assert_eq!(view.hover, None);
    }
}
