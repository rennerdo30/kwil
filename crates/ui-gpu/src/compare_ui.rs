//! Side-by-side read-only body; all source reads and refinement are worker-owned.
use super::*;
use chrome::{Metrics, tok};
use kwil_core_commands::theme::UiRole;
use kwil_view_model::compare_session::{Action, Kind, Session};
use std::sync::Arc;

impl App {
    pub(super) fn compare_click(&mut self) {
        let body = self.body_area();
        if body.contains(self.pointer.0, self.pointer.1) {
            let ch = self.font.as_ref().map_or(1, |font| font.ch);
            self.view.compare.expand_row((self.pointer.1 - body.y) / ch);
        }
    }
    pub(super) fn compare_ready(&mut self) {
        let previous = self.view.compare.page.as_ref().map(|p| (p.top, p.total, p.ignore_whitespace, p.collapsed));
        if !self.view.poll_compare() {
            return;
        }
        let current = self.view.compare.page.as_ref().map(|p| (p.top, p.total, p.ignore_whitespace, p.collapsed));
        let rows = self.view.rows();
        let scroll = match (previous, current) {
            (Some((a, n, w, c)), Some((b, m, x, d))) if (n, w, c) == (m, x, d) && a.abs_diff(b) < rows => {
                b as i64 - a as i64
            }
            _ => 0,
        };
        let dirty = if scroll > 0 {
            rows.saturating_sub(scroll as usize)..rows
        } else if scroll < 0 {
            0..scroll.unsigned_abs() as usize
        } else {
            0..rows
        };
        // The header shows the comparison; the status bar keeps its editor fields.
        self.view.set_notice(String::new());
        self.apply(Damage { full: !self.view.compare.open, scroll, rows: dirty.collect(), status: true });
    }
    /// The header toggle under `point`, while a comparison is open.
    pub(super) fn compare_toggle_at(&self, point: (usize, usize)) -> Option<Toggle> {
        self.view.compare.open.then(|| self.compare_header.hits.at(point)).flatten()
    }
    /// The left pane's name: the active tab, without its unsaved marker.
    pub(super) fn compare_left_title(&self) -> String {
        let title = self.view.tab_title(self.view.active_id());
        title.strip_prefix("* ").map_or(title.clone(), str::to_owned)
    }
    pub(super) fn dispatch_compare(&mut self, command: &str) -> bool {
        if !command.starts_with("compare.") {
            return false;
        }
        let proxy = self.proxy.clone();
        self.view.compare.set_wake(Arc::new(move || {
            let _ = proxy.send_event(UserEvent::Notice(String::new()));
        }));
        self.close_search();
        let was_open = self.view.compare.open;
        self.view.compare_command(command);
        // The shared command reports the comparison as a notice (the TUI shows
        // it); the GUI header already says it, so only errors stay.
        if self.view.compare.open && self.view.notice() == self.view.compare.header() {
            self.view.set_notice(String::new());
        }
        self.drag = None;
        self.selecting = false;
        self.sync_file_ui();
        self.apply(Damage {
            full: !was_open || !self.view.compare.open || command == "compare.files",
            status: true,
            ..Default::default()
        });
        true
    }
    pub(super) fn compare_key(&mut self, key: &HostKey<'_>) -> bool {
        if !self.view.compare.open
            || self.palette_open()
            || self.search_panel.is_open()
            || self.settings_open()
            || self.menu.state.is_open()
        {
            return false;
        }
        if (self.mods.control_key() || self.mods.super_key())
            && matches!(&key.logical_key, Key::Character(c) if c.eq_ignore_ascii_case("p"))
        {
            return false;
        }
        if let Some(chord) = input::chord(&key.logical_key, self.mods)
            && let Some(keys) = &mut self.keys
            && let kwil_core_commands::vim::VimResult::Command(call) = keys.feed_with_selection(chord)
            && self.dispatch_compare(call.id)
        {
            return true;
        }
        match &key.logical_key {
            Key::Named(NamedKey::Escape) => self.view.compare.close(),
            Key::Named(NamedKey::F7) => {
                self.view.compare.action(if self.mods.shift_key() { Action::Previous } else { Action::Next })
            }
            Key::Named(NamedKey::ArrowDown) => self.view.compare.action(Action::Scroll(1)),
            Key::Named(NamedKey::ArrowUp) => self.view.compare.action(Action::Scroll(-1)),
            Key::Named(NamedKey::PageDown) => {
                self.view.compare.action(Action::Scroll(self.view.rows().saturating_sub(2) as i64))
            }
            Key::Named(NamedKey::PageUp) => {
                self.view.compare.action(Action::Scroll(-(self.view.rows().saturating_sub(2) as i64)))
            }
            Key::Named(NamedKey::Enter | NamedKey::Space) => self.view.compare.expand_row(0),
            Key::Character(c) if c == " " => self.view.compare.expand_row(0),
            _ => {}
        }
        self.apply(Damage { full: !self.view.compare.open, status: true, ..Default::default() });
        true
    }
}
/// The compare header: what is compared (one label over each pane), how much
/// differs, and the two view options as toggle chips. It reserves height from
/// the body like the log filter strip, so the rows, scroll copies and clicks
/// below it stay ordinary body rows; it repaints only when what it shows
/// changes, and damages only its own rectangle.
#[derive(Default)]
pub(super) struct Header {
    shown: Option<Shown>,
    pub hits: desktop::Hits<Toggle>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Toggle {
    Whitespace,
    Unchanged,
}

impl Toggle {
    pub fn command(self) -> &'static str {
        match self {
            Self::Whitespace => "compare.toggleIgnoreWhitespace",
            Self::Unchanged => "compare.toggleUnchanged",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Whitespace => "Ignore whitespace",
            Self::Unchanged => "Hide unchanged",
        }
    }
}

/// Everything the header shows; a repaint happens only when this changes.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Shown {
    area: Rect,
    left: String,
    right: String,
    saved: bool,
    summary: Option<(usize, bool, bool)>,
    notice: String,
    hover: Option<Toggle>,
    scale: u64,
}

const HINTS: &str = "F7 next · Shift+F7 previous · Esc close";

/// Summary band (count, chips, key hints) and the pane-label band below it.
fn bands(m: &Metrics) -> (usize, usize) {
    (m.dip(tok::LIST_ROW), m.dip(tok::TEXT + tok::XS))
}

/// Reserves the header at the top of `source`; the rest is the compare body.
pub(super) fn layout(
    source: kwil_view_model::TabArea,
    m: &Metrics,
    open: bool,
) -> (Option<Rect>, kwil_view_model::TabArea) {
    if !open || source.width == 0 || source.height == 0 {
        return (None, source);
    }
    let (summary, sides) = bands(m);
    let height = (summary + sides).min(source.height);
    let bar = Rect { x: source.x, y: source.y, w: source.width, h: height };
    (Some(bar), kwil_view_model::TabArea { y: source.y + height, height: source.height - height, ..source })
}

/// "1,998": counts read faster with digit groups.
fn grouped(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub(super) fn count_label(changes: usize) -> String {
    match changes {
        0 => "No differences".into(),
        1 => "1 change".into(),
        n => format!("{} changes", grouped(n)),
    }
}

/// Side labels: a name the user knows (the tab, the picked file) and, muted,
/// which version of it this pane shows.
pub(super) fn side_labels<'a>(session: &'a Session, left: &'a str) -> [(&'a str, &'static str); 2] {
    let right = session.page.as_ref().map_or("", |page| page.right_title.as_str());
    [(left, "Editor"), (right, if session.against_saved { "Saved on disk" } else { "On disk" })]
}

/// Paints the header into `area` when what it shows changed (or `force`),
/// recording toggle hit rectangles. Returns the painted rectangle for damage.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_header(
    header: &mut Header,
    session: &Session,
    left: &str,
    pixels: &mut [u32],
    stride: usize,
    area: Rect,
    font: &mut font::UiFont,
    theme: &kwil_core_commands::theme::Theme,
    m: &Metrics,
    hover: Option<Toggle>,
    force: bool,
) -> Option<Rect> {
    let shown = Shown {
        area,
        left: left.to_owned(),
        right: session.page.as_ref().map_or_else(String::new, |p| p.right_title.clone()),
        saved: session.against_saved,
        summary: session.page.as_ref().map(|p| (p.changes, p.ignore_whitespace, p.collapsed)),
        notice: session.notice.clone(),
        hover,
        scale: m.scale.to_bits(),
    };
    if !force && header.shown.as_ref() == Some(&shown) {
        return None;
    }
    header.shown = Some(shown);
    header.hits = desktop::Hits::default();
    let c = desktop::Colors::new(theme);
    let s = desktop::Sizes::new(m);
    let ground = c.sidebar;
    chrome::fill_clipped(pixels, stride, area, ground);
    let (summary_h, sides_h) = bands(m);
    let band = Rect { h: summary_h.min(area.h), ..area };
    let right_edge = band.x + band.w.saturating_sub(s.lg);
    let start = band.x + s.lg;
    let clip = Rect { x: start, w: right_edge.saturating_sub(start), ..band };
    // 1. The state: how much differs, or why there is nothing to show.
    let mut x = match &session.page {
        _ if !session.notice.is_empty() => {
            let color = if session.page.is_none() { c.error } else { c.fg };
            desktop::label(pixels, stride, font, clip, &session.notice, color)
        }
        None => desktop::label(pixels, stride, font, clip, "Comparing…", c.muted),
        Some(page) => desktop::label(pixels, stride, font.title(), clip, &count_label(page.changes), c.fg),
    };
    // 2. The options, as toggles that show their state.
    if let Some(page) = &session.page {
        x += s.xl;
        let h = m.dip(tok::TEXT + tok::XS).min(band.h);
        for (toggle, on) in [(Toggle::Whitespace, page.ignore_whitespace), (Toggle::Unchanged, page.collapsed)] {
            let w = chip_width(font, toggle.label(), on, &s);
            if x + w > right_edge {
                break;
            }
            let chip = Rect { x, y: band.y + (band.h - h) / 2, w, h };
            draw_chip(pixels, stride, font, chip, toggle.label(), on, hover == Some(toggle), ground, &c, &s);
            header.hits.push(chip, toggle);
            x += w + s.sm;
        }
    }
    // 3. Key hints: short, muted and right-aligned; dropped whole when the
    // window is too narrow, never cut mid-word.
    let small = font.small();
    let hints = small.measure(HINTS, usize::MAX);
    if x + s.xl + hints <= right_edge {
        let clip = Rect { x: right_edge - hints, w: hints, ..band };
        desktop::label(pixels, stride, small, clip, HINTS, c.muted);
    }
    // 4. Pane labels, aligned to the two panes and split where they split.
    let sides = Rect { y: band.y + band.h, h: sides_h.min(area.h.saturating_sub(band.h)), ..area };
    let half = area.w / 2;
    for (index, (name, tag)) in side_labels(session, left).into_iter().enumerate() {
        let pane = Rect { x: sides.x + index * half + s.md, w: half.saturating_sub(2 * s.md), ..sides };
        side_label(pixels, stride, font, pane, name, tag, &c, &s);
    }
    desktop::vline(pixels, stride, sides.x + half, sides.y, sides.h, &c, &s);
    desktop::hline(pixels, stride, area.x, (area.y + area.h).saturating_sub(s.hairline), area.w, &c, &s);
    Some(area)
}

fn chip_width(font: &mut font::UiFont, text: &str, on: bool, s: &desktop::Sizes) -> usize {
    font.measure(text, 400) + 2 * s.md + if on { s.lg + s.xs } else { 0 }
}

/// Name, then its muted tag. The tag says which side this is, so it keeps its
/// room and the name is the one that gets an ellipsis.
#[allow(clippy::too_many_arguments)]
fn side_label(
    pixels: &mut [u32],
    stride: usize,
    font: &mut font::UiFont,
    pane: Rect,
    name: &str,
    tag: &str,
    c: &desktop::Colors,
    s: &desktop::Sizes,
) {
    let tag_w = font.small().measure(tag, pane.w);
    let name_room = pane.w.saturating_sub(tag_w + s.sm);
    let mut x = pane.x;
    if !name.is_empty() && name_room > 0 {
        x = desktop::label(pixels, stride, font, Rect { w: name_room, ..pane }, name, c.fg) + s.sm;
    }
    let clip = Rect { x, w: (pane.x + pane.w).saturating_sub(x), ..pane };
    desktop::label(pixels, stride, font.small(), clip, tag, c.muted);
}

/// A toggle chip: on is a selection-tinted pill with a check, off is an
/// outlined pill with muted text. Hover lifts either.
#[allow(clippy::too_many_arguments)]
fn draw_chip(
    pixels: &mut [u32],
    stride: usize,
    font: &mut font::UiFont,
    chip: Rect,
    text: &str,
    on: bool,
    hovered: bool,
    ground: u32,
    c: &desktop::Colors,
    s: &desktop::Sizes,
) {
    let radius = s.pill(chip.h);
    let fill = if on { chrome::selected_color(ground, c.selection) } else { ground };
    let fill = if hovered { chrome::hover_color(fill, c.fg) } else { fill };
    chrome::round_rect(pixels, stride, chip, radius, if on { fill } else { c.border });
    chrome::round_rect(pixels, stride, desktop::inset(chip, s.hairline), radius.saturating_sub(s.hairline), fill);
    let mut x = chip.x + s.md;
    if on {
        let center = ((x + s.sm) as f32, (chip.y + chip.h / 2) as f32);
        icons::draw(pixels, stride, icons::Icon::Check, center, s.lg, c.accent);
        x += s.lg + s.xs;
    }
    let clip = Rect { x, w: (chip.x + chip.w).saturating_sub(x + s.md), ..chip };
    desktop::label(pixels, stride, font, clip, text, if on { c.fg } else { c.muted });
}

const EXPAND_HINT: &str = "Click to expand";

/// A collapsed run of unchanged lines: a full-width quiet band in the chrome
/// face with a chevron, so it reads as a control and not as file text. The
/// label keeps the chrome size at any editor zoom; the row clips it.
fn draw_collapsed(pixels: &mut [u32], width: usize, row: Rect, hidden: usize, ui: Option<&mut font::UiFont>, gutter: usize) {
    let band = chrome::mix(bg(), fg(), chrome::ramp::HOVER);
    fill(pixels, width, row, band);
    let Some(font) = ui else { return };
    let size = (font.px as usize).min(row.h).max(1);
    let muted = chrome::mix(band, fg(), 0xa8);
    let gap = size / 3;
    let center = (gutter.saturating_sub(size + gap) as f32 + size as f32 / 2.0, (row.y + row.h / 2) as f32);
    icons::draw(pixels, width, icons::Icon::Down, center, size, muted);
    let unit = if hidden == 1 { "unchanged line" } else { "unchanged lines" };
    let clip = Rect { x: gutter, w: width.saturating_sub(gutter), ..row };
    let end = desktop::label(pixels, width, font, clip, &format!("{} {unit}", grouped(hidden)), muted);
    let small = font.small();
    let hint = Rect { x: end + size, w: (clip.x + clip.w).saturating_sub(end + size), ..row };
    if small.measure(EXPAND_HINT, hint.w) < hint.w {
        desktop::label(pixels, width, small, hint, EXPAND_HINT, chrome::mix(band, fg(), chrome::ramp::FAINT));
    }
}

pub(super) fn draw(
    session: &Session,
    pixels: &mut [u32],
    width: usize,
    font: &mut Font,
    mut ui: Option<&mut font::UiFont>,
    dirty: impl Fn(usize) -> bool,
) {
    if width == 0 {
        return;
    }
    let rows = pixels.len() / width / font.ch;
    for row in (0..rows).filter(|&row| dirty(row)) {
        fill(pixels, width, Rect { x: 0, y: row * font.ch, w: width, h: font.ch }, bg());
    }
    // Before the first page the header says "Comparing…"; the body stays empty.
    let Some(page) = &session.page else { return };
    let half = width / 2;
    let gutter = 6 * font.cw;
    // Line fills are the theme's diff roles over the editor background; a
    // changed word is the same role pushed further from it, keeping its hue.
    let theme = theme_ui::chrome();
    let (added, removed) =
        (theme_ui::rgb_over_background(&theme, UiRole::Added), theme_ui::rgb_over_background(&theme, UiRole::Removed));
    let word_fill = |role: u32| chrome::emphasize(bg(), role);
    for (index, row) in page.rows.iter().take(rows).enumerate().filter(|(index, _)| dirty(*index)) {
        let y = index * font.ch;
        if row.hidden > 0 {
            let rect = Rect { x: 0, y, w: width, h: font.ch };
            draw_collapsed(pixels, width, rect, row.hidden, ui.as_deref_mut(), gutter);
            continue;
        }
        for side in 0..2 {
            let x = side * half;
            let color = match (row.kind, side) {
                (Kind::Delete | Kind::Modify, 0) => removed,
                (Kind::Insert | Kind::Modify, 1) => added,
                _ => bg(),
            };
            fill(pixels, width, Rect { x, y, w: half, h: font.ch }, color);
            let number = if side == 0 { row.left } else { row.right };
            let label = number.map_or_else(|| "      ".into(), |n| format!("{:>5} ", n + 1));
            draw_text(pixels, width, font, x, y, label.as_bytes(), half / font.cw, gutter_fg(), color);
            let columns = half.saturating_sub(gutter + font.cw) / font.cw;
            draw_text(pixels, width, font, x + gutter, y, &row.text[side], columns, fg(), color);
            for cell in text::cells(&row.text[side], columns, font.tab_size()) {
                let words = &row.words[side];
                if words
                    .get(words.partition_point(|r| r.end <= cell.bytes.start))
                    .is_some_and(|r| r.start < cell.bytes.end)
                {
                    let word_bg = word_fill(color);
                    let cx = x + gutter + cell.column * font.cw;
                    fill(pixels, width, Rect { x: cx, y, w: cell.width * font.cw, h: font.ch }, word_bg);
                    draw_text(pixels, width, font, cx, y, cell.text.as_bytes(), cell.width, fg(), word_bg);
                }
            }
        }
        fill(pixels, width, Rect { x: half, y, w: 1, h: font.ch }, gutter_fg());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn collapsed_session(hidden: usize) -> Session {
        use kwil_view_model::compare_session::{Page, Row};
        let mut session = Session::default();
        session.page.replace(Page {
            rows: vec![Row {
                left: None,
                right: None,
                kind: Kind::Equal,
                text: Default::default(),
                words: Default::default(),
                hidden,
            }],
            top: 0,
            total: 1,
            changes: 0,
            ignore_whitespace: false,
            collapsed: true,
            right_title: String::new(),
        });
        session
    }

    #[test]
    fn collapsed_region_is_a_full_width_band_labelled_in_the_chrome_face() {
        let (Some(mut font), Some(mut ui)) = (Font::load(14.0), font::UiFont::load(15.0)) else { return };
        let width = 1000;
        let mut pixels = vec![0; width * font.ch];
        draw(&collapsed_session(1998), &mut pixels, width, &mut font, Some(&mut ui), |_| true);
        let band = chrome::mix(bg(), fg(), chrome::ramp::HOVER);
        assert_ne!(band, bg(), "the band stands apart from file text");
        for y in 0..font.ch {
            let row = &pixels[y * width..(y + 1) * width];
            assert_eq!(row[0], band, "the band starts at the left edge");
            assert_eq!(row[width - 1], band, "and spans both panes");
            assert_eq!(row[width / 2], band, "with no pane divider through it");
        }
        let gutter = 6 * font.cw;
        let ink = |from: usize, to: usize| {
            (0..font.ch).any(|y| pixels[y * width + from..y * width + to].iter().any(|p| *p != band))
        };
        assert!(ink(0, gutter), "a chevron marks it as expandable");
        assert!(ink(gutter, width / 2), "the count is drawn after the gutter");
        let mut plain = collapsed_session(1998);
        plain.page.as_mut().unwrap().rows[0].hidden = 0;
        let mut other = vec![0; width * font.ch];
        draw(&plain, &mut other, width, &mut font, Some(&mut ui), |_| true);
        assert!(other.iter().all(|p| *p != band), "only collapsed rows get the band");
    }

    #[test]
    fn compare_header_reserves_its_own_rows_above_the_body() {
        let m = Metrics::new(1.5);
        let source = kwil_view_model::TabArea { x: 10, y: 80, width: 1200, height: 700 };
        assert_eq!(layout(source, &m, false), (None, source), "closed: nothing reserved");
        let (bar, body) = layout(source, &m, true);
        let bar = bar.expect("open: a header");
        assert_eq!((bar.x, bar.y, bar.w), (source.x, source.y, source.width));
        assert_eq!(body.y, bar.y + bar.h, "the body starts right below the header");
        assert_eq!(body.height + bar.h, source.height);
        assert_eq!((body.x, body.width), (source.x, source.width), "panes split where the header splits");
        let (summary, sides) = bands(&m);
        assert_eq!(bar.h, summary + sides);
        let tiny = kwil_view_model::TabArea { height: 20, ..source };
        assert_eq!(layout(tiny, &m, true).1.height, 0, "never more than the body");
    }

    #[test]
    fn compare_header_names_both_sides_counts_changes_and_hits_its_toggles() {
        let Some(mut ui) = font::UiFont::load(15.0) else { return };
        assert_eq!(count_label(0), "No differences");
        assert_eq!(count_label(1), "1 change");
        assert_eq!(count_label(1998), "1,998 changes");
        assert_eq!(count_label(1_234_567), "1,234,567 changes");
        let mut session = session_with_one_change();
        session.page.as_mut().unwrap().right_title = "sample.rs".into();
        assert_eq!(side_labels(&session, "sample.rs"), [("sample.rs", "Editor"), ("sample.rs", "On disk")]);
        session.against_saved = true;
        assert_eq!(side_labels(&session, "sample.rs"), [("sample.rs", "Editor"), ("sample.rs", "Saved on disk")]);

        let m = Metrics::new(1.0);
        let theme = theme_ui::chrome();
        let (width, height) = (1200, 200);
        let source = kwil_view_model::TabArea { x: 0, y: 40, width, height: 160 };
        let bar = layout(source, &m, true).0.unwrap();
        let mut pixels = vec![0; width * height];
        let mut header = Header::default();
        let painted =
            draw_header(&mut header, &session, "sample.rs", &mut pixels, width, bar, &mut ui, &theme, &m, None, false);
        assert_eq!(painted, Some(bar), "the first paint damages exactly the header");
        assert!(pixels[..bar.y * width].iter().all(|p| *p == 0), "nothing above the header is touched");
        assert!(pixels[(bar.y + bar.h) * width..].iter().all(|p| *p == 0), "nothing below it either");
        // Two toggles in the summary band, in order, apart, each the real command.
        let (summary, _) = bands(&m);
        let hits: Vec<_> = header.hits.iter().copied().collect();
        assert_eq!(hits.iter().map(|(_, t)| *t).collect::<Vec<_>>(), [Toggle::Whitespace, Toggle::Unchanged]);
        for (rect, toggle) in &hits {
            assert!(rect.y >= bar.y && rect.y + rect.h <= bar.y + summary, "{toggle:?} sits in the summary band");
            assert!(rect.h >= m.dip(tok::TEXT), "{toggle:?} is a real click target");
            assert_eq!(header.hits.at((rect.x + rect.w / 2, rect.y + rect.h / 2)), Some(*toggle));
        }
        assert!(hits[0].0.x + hits[0].0.w < hits[1].0.x, "toggles do not overlap");
        assert_eq!(Toggle::Whitespace.command(), "compare.toggleIgnoreWhitespace");
        assert_eq!(Toggle::Unchanged.command(), "compare.toggleUnchanged");
        let ground = desktop::Colors::new(&theme).sidebar;
        let ink_rows = |pixels: &[u32], x0: usize, x1: usize, y0: usize, y1: usize| {
            (y0..y1).filter(|y| pixels[y * width + x0..y * width + x1].iter().any(|p| *p != ground)).count()
        };
        // The count comes first, in the title face: taller ink than the body-size chips.
        let count = ink_rows(&pixels, bar.x, hits[0].0.x, bar.y, bar.y + summary);
        let mut body_size = vec![ground; width * height];
        desktop::label(&mut body_size, width, &mut ui, Rect { x: 0, y: 0, w: 400, h: summary }, "1 change", 0);
        assert!(count > ink_rows(&body_size, 0, 400, 0, summary), "the count is larger than body text");
        // One label over each pane, split where the panes split.
        let sides = (bar.y + summary, bar.y + bar.h - 1);
        assert!(ink_rows(&pixels, bar.x, bar.x + width / 2 - 2, sides.0, sides.1) > 0, "left pane label");
        assert!(ink_rows(&pixels, bar.x + width / 2 + 2, bar.x + width, sides.0, sides.1) > 0, "right pane label");
        // Damage-only: unchanged content repaints nothing; hover or a forced repaint does.
        let mut again = |hover, force| {
            draw_header(&mut header, &session, "sample.rs", &mut pixels, width, bar, &mut ui, &theme, &m, hover, force)
        };
        assert_eq!(again(None, false), None);
        assert_eq!(again(Some(Toggle::Unchanged), false), Some(bar));
        assert_eq!(again(Some(Toggle::Unchanged), true), Some(bar));
        // A narrow window keeps every toggle inside the header.
        let narrow = layout(kwil_view_model::TabArea { width: 300, ..source }, &m, true).0.unwrap();
        let mut small = vec![0; width * height];
        draw_header(&mut header, &session, "sample.rs", &mut small, width, narrow, &mut ui, &theme, &m, None, false);
        assert!(header.hits.iter().all(|(r, _)| r.x + r.w <= narrow.x + narrow.w));
    }

    fn session_with_one_change() -> Session {
        use kwil_view_model::compare_session::{Page, Row};
        let mut session = Session::default();
        session.page.replace(Page {
            rows: vec![Row {
                left: Some(0),
                right: Some(0),
                kind: Kind::Modify,
                text: [b"old".to_vec(), b"new".to_vec()],
                words: [std::iter::once(0..3).collect(), std::iter::once(0..3).collect()],
                hidden: 0,
            }],
            top: 0,
            total: 1,
            changes: 1,
            ignore_whitespace: false,
            collapsed: false,
            right_title: String::new(),
        });
        session
    }

    #[test]
    fn diff_rows_use_the_theme_diff_roles_in_the_dark_and_light_themes() {
        use kwil_core_commands::theme::{Scheme, Theme};
        let Some(mut font) = Font::load(14.0) else { return };
        let previous = theme_ui::chrome();
        for scheme in [Scheme::Dark, Scheme::Light] {
            let theme = Arc::new(Theme::builtin(scheme));
            theme_ui::select(theme.clone());
            let session = session_with_one_change();
            let width = 800;
            let mut pixels = vec![0; width * font.ch];
            draw(&session, &mut pixels, width, &mut font, None, |_| true);
            let last = |side: usize| pixels[(side + 1) * width / 2 - 2];
            assert_eq!(last(0), theme_ui::rgb(&theme, UiRole::Removed), "{scheme:?} left fill is the removed role");
            assert_eq!(last(1), theme_ui::rgb(&theme, UiRole::Added), "{scheme:?} right fill is the added role");
            let word = |side: usize| pixels[side * width / 2 + 6 * font.cw + 1];
            assert_ne!(word(0), last(0), "{scheme:?} changed words are stronger than the line");
            assert_ne!(word(1), last(1));
        }
        // An imported theme's translucent diff role is composited, not painted opaque.
        let mut theme = Theme::builtin(Scheme::Dark);
        let translucent = kwil_core_commands::theme::Rgba(0x9b, 0xb9, 0x55, 0x33);
        theme.set_ui(UiRole::Added, translucent);
        let theme = Arc::new(theme);
        theme_ui::select(theme.clone());
        let width = 800;
        let mut pixels = vec![0; width * font.ch];
        draw(&session_with_one_change(), &mut pixels, width, &mut font, None, |_| true);
        let kwil_core_commands::theme::Rgba(r, g, b, _) = translucent.over(theme.ui().get(UiRole::Background));
        let expected = (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b);
        assert_eq!(pixels[width - 2], expected, "translucent added role is composited over the background");
        theme_ui::select(previous);
    }

    #[test]
    fn compare_gui_prompt_keeps_tab_and_body_is_read_only() {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "kwil-gui-compare-{}-{}.txt",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let disk = (0..40).map(|n| format!("disk word {n}\n")).collect::<String>();
        let buffer = disk.replace("disk word 0\n", "buffer word 0\n").replace("disk word 30\n", "buffer word 30\n");
        std::fs::write(&path, disk).unwrap();
        let (mut app, events) = crate::search_ui::host_dispatch_tests::host(&buffer);
        let document = app.view.active_id();
        let revision = app.view.editor().unwrap().document().revision();
        app.palette_action(kwil_core_commands::palette::PaletteAction::Command("compare.files"));
        app.view.file_prompt_mut().unwrap().text = path.to_string_lossy().into();
        app.view.submit_file_prompt().unwrap();
        app.sync_file_ui();
        events.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        app.compare_ready();
        assert_eq!(app.view.active_id(), document);
        assert_eq!(app.view.compare.page.as_ref().unwrap().changes, 2);
        for (shift, expected) in [(false, 30), (true, 0)] {
            app.mods = if shift { ModifiersState::SHIFT } else { ModifiersState::empty() };
            assert!(app.compare_key(&HostKey { logical_key: Key::Named(NamedKey::F7), text: None, repeat: false }));
            events.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
            app.compare_ready();
            assert_eq!(app.view.compare.page.as_ref().unwrap().rows[0].left, Some(expected));
        }
        for (command, expected) in [("compare.nextChange", 30), ("compare.previousChange", 0)] {
            app.palette_action(kwil_core_commands::palette::PaletteAction::Command(command));
            events.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
            app.compare_ready();
            assert_eq!(app.view.compare.page.as_ref().unwrap().rows[0].left, Some(expected));
        }
        let hidden = app.view.compare.page.as_ref().unwrap().rows.iter().position(|r| r.hidden > 0).unwrap();
        let body = app.body_area();
        let ch = app.font.as_ref().map_or(1, |font| font.ch);
        app.pointer = (body.x, body.y + hidden * ch);
        app.compare_click();
        events.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        app.compare_ready();
        assert_eq!(app.view.compare.page.as_ref().unwrap().rows[hidden].hidden, 0);
        app.edit(input::Action::Text("must not insert".into()));
        app.palette_action(kwil_core_commands::palette::PaletteAction::Command("edit.deleteLine"));
        assert_eq!(app.view.editor().unwrap().document().revision(), revision);
        app.dispatch_compare("compare.close");
        events.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        app.compare_ready();
        assert!(!app.view.compare.open);
        std::fs::remove_file(path).unwrap();
    }
}
