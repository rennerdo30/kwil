//! Desktop overlay widgets shared by settings, palette, prompts, search and
//! file-type registration: cards, text fields, buttons, switches, segmented
//! choices, checkboxes, list rows and keycaps. Sizes come from the DPI chrome
//! metrics, text from the proportional chrome face and colours from
//! `theme_ui`; editor zoom never reaches them.
//!
//! An overlay starts every paint with `card`, which repaints its whole
//! rectangle opaquely; widgets then draw only onto those fresh pixels (rows
//! first, controls on top). Painting an overlay twice therefore gives the
//! same pixels, and its damage stays the overlay's own rectangle. Widgets do
//! not fill their bounding box, so a control on a selected or hovered row
//! keeps that row's background around its rounded corners. Text is fitted or wrapped from a bounded
//! prefix only; nothing measures or rasterizes a full user string.
use super::*;
use chrome::Metrics;
use kwil_core_commands::theme::{Theme, UiRole};

/// Scalars any one widget examines; chrome strings are clipped far earlier.
pub(super) const SCALARS: usize = 256;

/// Role-derived colours. Lifts are blends toward the foreground, so the light
/// theme gets the same hierarchy without separate constants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Colors {
    /// Behind rounded card corners: the editor background.
    pub backdrop: u32,
    pub surface: u32,
    pub sidebar: u32,
    pub control: u32,
    pub control_hover: u32,
    pub hover: u32,
    pub selected: u32,
    pub fg: u32,
    pub muted: u32,
    pub faint: u32,
    pub border: u32,
    pub accent: u32,
    pub on_accent: u32,
    pub error: u32,
}

impl Colors {
    pub fn new(theme: &Theme) -> Self {
        let rgb = |role| theme_ui::rgb(theme, role);
        let panel = rgb(UiRole::PanelBackground);
        let fg = rgb(UiRole::PanelForeground);
        let surface = chrome::mix(panel, fg, 0x0a);
        Self {
            backdrop: rgb(UiRole::Background),
            surface,
            sidebar: panel,
            control: chrome::mix(panel, fg, 0x16),
            control_hover: chrome::mix(panel, fg, 0x24),
            hover: chrome::mix(surface, fg, 0x10),
            selected: chrome::mix(surface, rgb(UiRole::Selection), 0xa0),
            fg,
            muted: chrome::mix(surface, fg, 0xa8),
            faint: chrome::mix(surface, fg, 0x5c),
            border: rgb(UiRole::Border),
            accent: rgb(UiRole::Accent),
            on_accent: panel,
            error: rgb(UiRole::Error),
        }
    }
}

/// Physical sizes for one monitor scale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Sizes {
    pub hairline: usize,
    pub radius: usize,
    pub card_radius: usize,
    /// Outer padding of a card section.
    pub pad: usize,
    /// Inner padding of a control.
    pub inner: usize,
    pub gap: usize,
    /// Height of fields and buttons: a comfortable click target.
    pub control: usize,
    /// Sidebar and list navigation rows.
    pub nav: usize,
    /// One line of 14 DIP chrome text.
    pub text: usize,
    pub header: usize,
    pub footer: usize,
    pub switch_w: usize,
    pub switch_h: usize,
    pub check: usize,
    pub focus: usize,
    pub indicator: usize,
    pub icon: usize,
    pub thumb: usize,
}

impl Sizes {
    pub fn new(m: &Metrics) -> Self {
        Self {
            hairline: m.line,
            radius: m.dip(4.0),
            card_radius: m.dip(8.0),
            pad: m.dip(16.0),
            inner: m.dip(10.0),
            gap: m.dip(8.0),
            control: m.dip(32.0),
            nav: m.dip(36.0),
            text: m.dip(20.0),
            header: m.dip(52.0),
            footer: m.dip(40.0),
            switch_w: m.dip(40.0),
            switch_h: m.dip(20.0),
            check: m.dip(18.0),
            focus: m.dip(2.0),
            indicator: m.dip(3.0),
            icon: m.dip(10.0),
            thumb: m.dip(4.0),
        }
    }
}

pub(super) fn inset(r: Rect, by: usize) -> Rect {
    let (dx, dy) = (by.min(r.w / 2), by.min(r.h / 2));
    Rect { x: r.x + dx, y: r.y + dy, w: r.w - 2 * dx, h: r.h - 2 * dy }
}

/// Bounded, control-escaped prefix of untrusted text.
pub(super) fn visible(text: &str, scalars: usize) -> String {
    let prefix: String = text.chars().take(scalars).collect();
    kwil_view_model::palette_session::display_text(&prefix)
}

/// Hit rectangles recorded while drawing, in paint order; the topmost wins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Hits<T> {
    items: Vec<(Rect, T)>,
}

impl<T> Default for Hits<T> {
    fn default() -> Self {
        Self { items: Vec::new() }
    }
}

impl<T: Copy + PartialEq> Hits<T> {
    pub fn push(&mut self, rect: Rect, target: T) {
        if rect.w > 0 && rect.h > 0 {
            self.items.push((rect, target));
        }
    }
    pub fn at(&self, point: (usize, usize)) -> Option<T> {
        self.items.iter().rev().find(|(rect, _)| rect.contains(point)).map(|(_, target)| *target)
    }
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.items.len()
    }
    pub fn iter(&self) -> impl Iterator<Item = &(Rect, T)> {
        self.items.iter()
    }
}

/// Card-local repaint request (hover, wheel, list scroll). It joins the host's
/// render admission like `palette_dirty`, but never restores the body under a
/// card: the card repaints itself idempotently and damages only its rectangle.
/// Without it a hover or wheel change only schedules `RedrawRequested`, and
/// `render` returns early because no damage or dirty flag is set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Paint(bool);

impl Paint {
    pub fn request(&mut self) {
        self.0 = true;
    }
    pub fn pending(self) -> bool {
        self.0
    }
    pub fn take(&mut self) -> bool {
        std::mem::take(&mut self.0)
    }
}

/// Window size and monitor scale a paint was made for. Hit geometry recorded
/// for another surface is stale: after a resize or DPI change, and before the
/// next paint, a click must not act on whatever now lies under an old rectangle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Surface {
    size: (usize, usize),
    scale: u64,
}

impl Surface {
    pub fn new(size: (usize, usize), m: &Metrics) -> Self {
        Self { size, scale: m.scale.to_bits() }
    }
}

/// Rounded card with a hairline border. The corners are first painted with
/// the editor background, so a redraw never blends onto its previous pixels.
pub(super) fn card(buf: &mut [u32], stride: usize, rect: Rect, c: &Colors, s: &Sizes) {
    chrome::fill_clipped(buf, stride, rect, c.backdrop);
    chrome::round_rect(buf, stride, rect, s.card_radius, c.border);
    chrome::round_rect(buf, stride, inset(rect, s.hairline), s.card_radius.saturating_sub(s.hairline), c.surface);
}

pub(super) fn hline(buf: &mut [u32], stride: usize, x: usize, y: usize, w: usize, c: &Colors, s: &Sizes) {
    chrome::fill_clipped(buf, stride, Rect { x, y, w, h: s.hairline }, c.border);
}

pub(super) fn vline(buf: &mut [u32], stride: usize, x: usize, y: usize, h: usize, c: &Colors, s: &Sizes) {
    chrome::fill_clipped(buf, stride, Rect { x, y, w: s.hairline, h }, c.border);
}

/// `text` fitted into `clip` with an ellipsis, vertically centred. Returns the
/// pen position after the drawn text.
pub(super) fn label(
    buf: &mut [u32],
    stride: usize,
    font: &mut font::UiFont,
    clip: Rect,
    text: &str,
    color: u32,
) -> usize {
    if clip.w == 0 || clip.h == 0 {
        return clip.x;
    }
    let text = font.fit(&visible(text, SCALARS), clip.w);
    let baseline = font.baseline(clip.y, clip.h);
    font.draw(buf, stride, clip, clip.x, baseline, &text, color)
}

/// Right-aligned fitted text.
pub(super) fn label_right(
    buf: &mut [u32],
    stride: usize,
    font: &mut font::UiFont,
    clip: Rect,
    text: &str,
    color: u32,
) -> usize {
    if clip.w == 0 || clip.h == 0 {
        return clip.x;
    }
    let text = font.fit(&visible(text, SCALARS), clip.w);
    let width = font.measure(&text, clip.w);
    let baseline = font.baseline(clip.y, clip.h);
    let x = clip.x + clip.w - width;
    font.draw(buf, stride, clip, x, baseline, &text, color);
    x
}

/// Greedy word wrap into at most `lines` lines of `width` pixels. The last
/// line carries an ellipsis when text remains; only a bounded prefix is read.
pub(super) fn wrap(font: &mut font::UiFont, text: &str, width: usize, lines: usize) -> Vec<String> {
    let lines = lines.max(1);
    let text = visible(text, SCALARS.saturating_mul(lines));
    let space = font.width(" ");
    let mut out: Vec<String> = Vec::new();
    let mut words = text.split(' ').filter(|word| !word.is_empty()).peekable();
    let (mut line, mut used) = (String::new(), 0usize);
    while let Some(word) = words.peek().copied() {
        if out.len() + 1 == lines {
            let mut rest = std::mem::take(&mut line);
            for word in words.by_ref() {
                if !rest.is_empty() {
                    rest.push(' ');
                }
                rest.push_str(word);
            }
            out.push(font.fit(&rest, width));
            return out;
        }
        let w = font.measure(word, width.saturating_add(1));
        if line.is_empty() && w > width {
            out.push(font.fit(word, width));
            words.next();
            continue;
        }
        let next = if line.is_empty() { w } else { used + space + w };
        if next <= width {
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
            used = next;
            words.next();
        } else {
            out.push(std::mem::take(&mut line));
            used = 0;
        }
    }
    if !line.is_empty() {
        out.push(line);
    }
    out
}

/// One text input. `cursor` is a byte offset at a grapheme boundary of `text`.
#[derive(Clone, Copy, Debug)]
pub(super) struct Field<'a> {
    pub text: &'a str,
    pub cursor: usize,
    pub placeholder: &'a str,
    pub focused: bool,
    pub hovered: bool,
    /// Leading magnifier.
    pub search: bool,
    /// Trailing clear button. Set only where the caller registers a clear
    /// action at `field_clear`; a drawn control always has a behaviour.
    pub clear: bool,
}

/// Rect of the clear button inside a field drawn with `clear` and some text.
pub(super) fn field_clear(rect: Rect, s: &Sizes) -> Rect {
    let size = s.control.min(rect.h);
    Rect { x: (rect.x + rect.w).saturating_sub(size), y: rect.y + (rect.h - size) / 2, w: size, h: size }
}

/// Draws a field and returns the caret rectangle when focused, so IME
/// candidate windows can be placed at it.
pub(super) fn text_field(
    buf: &mut [u32],
    stride: usize,
    font: &mut font::UiFont,
    rect: Rect,
    field: Field<'_>,
    c: &Colors,
    s: &Sizes,
) -> Option<Rect> {
    let border = if field.focused { c.accent } else { c.border };
    chrome::round_rect(buf, stride, rect, s.radius, border);
    let bg = if field.hovered && !field.focused { c.control_hover } else { c.control };
    chrome::round_rect(buf, stride, inset(rect, s.hairline), s.radius.saturating_sub(s.hairline), bg);
    if field.focused {
        // Windows 11 field focus: a thicker accent edge along the bottom.
        let y = (rect.y + rect.h).saturating_sub(s.focus + s.hairline);
        chrome::fill_clipped(
            buf,
            stride,
            Rect { x: rect.x + s.radius, y, w: rect.w.saturating_sub(2 * s.radius), h: s.focus },
            c.accent,
        );
    }
    let mut left = rect.x + s.inner;
    if field.search {
        let size = s.icon.min(rect.h);
        let center = (left as f32 + size as f32 / 2.0, rect.y as f32 + rect.h as f32 / 2.0 - 1.0);
        icon(buf, stride, Icon::Search, center, size, c.muted, bg, s);
        left += size + s.gap;
    }
    let mut right = (rect.x + rect.w).saturating_sub(s.inner);
    if field.clear && !field.text.is_empty() {
        let clear = field_clear(rect, s);
        let center = (clear.x as f32 + clear.w as f32 / 2.0, clear.y as f32 + clear.h as f32 / 2.0);
        icon(buf, stride, Icon::Close, center, s.icon * 3 / 4, c.muted, bg, s);
        right = clear.x;
    }
    let clip = Rect { x: left, y: rect.y, w: right.saturating_sub(left), h: rect.h };
    if clip.w == 0 {
        return None;
    }
    let baseline = font.baseline(rect.y, rect.h);
    if field.text.is_empty() {
        let text = font.fit(field.placeholder, clip.w);
        font.draw(buf, stride, clip, left, baseline, &text, c.faint);
        return field.focused.then(|| caret(buf, stride, left, rect, c, s));
    }
    // Cells are bounded by the widest Latin advance, so the window fits in
    // pixels too; wide CJK cells (2 columns) are narrower than two `M`s.
    let columns = (clip.w / font.width("M").max(1)).max(1);
    let window = kwil_view_model::palette_session::text_window(field.text, field.cursor, columns);
    font.draw(buf, stride, clip, left, baseline, &window.text, c.fg);
    if !field.focused {
        return None;
    }
    let cells = kwil_view_model::grid::cells(window.text.as_bytes(), window.cursor, 4);
    let end = cells.last().map_or(0, |cell| cell.bytes.end).min(window.text.len());
    let offset = font.measure(&window.text[..end], clip.w);
    Some(caret(buf, stride, left + offset, rect, c, s))
}

fn caret(buf: &mut [u32], stride: usize, x: usize, rect: Rect, c: &Colors, s: &Sizes) -> Rect {
    let h = (s.text * 4 / 5).min(rect.h);
    let r = Rect { x: x.min((rect.x + rect.w).saturating_sub(1)), y: rect.y + (rect.h - h) / 2, w: s.hairline, h };
    chrome::fill_clipped(buf, stride, r, c.fg);
    r
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Style {
    Primary,
    Secondary,
    Subtle,
    Danger,
}

pub(super) fn button_width(font: &mut font::UiFont, text: &str, s: &Sizes) -> usize {
    (font.measure(text, 400) + 2 * s.pad).max(s.control * 2)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn button(
    buf: &mut [u32],
    stride: usize,
    font: &mut font::UiFont,
    rect: Rect,
    text: &str,
    style: Style,
    hovered: bool,
    c: &Colors,
    s: &Sizes,
) {
    let (bg, fg, edge) = match style {
        Style::Primary => (if hovered { chrome::mix(c.accent, c.fg, 0x30) } else { c.accent }, c.on_accent, None),
        Style::Secondary => (if hovered { c.control_hover } else { c.control }, c.fg, Some(c.border)),
        Style::Danger => (if hovered { c.control_hover } else { c.control }, c.error, Some(c.border)),
        Style::Subtle => (if hovered { c.hover } else { c.surface }, c.fg, None),
    };
    match edge {
        Some(edge) => {
            chrome::round_rect(buf, stride, rect, s.radius, edge);
            chrome::round_rect(buf, stride, inset(rect, s.hairline), s.radius.saturating_sub(s.hairline), bg);
        }
        // A resting subtle button is text on whatever row it sits on.
        None if style == Style::Subtle && !hovered => {}
        None => chrome::round_rect(buf, stride, rect, s.radius, bg),
    }
    let clip = inset(rect, s.inner.min(rect.w / 4));
    let fitted = font.fit(text, clip.w);
    let width = font.measure(&fitted, clip.w);
    let baseline = font.baseline(rect.y, rect.h);
    font.draw(buf, stride, clip, clip.x + (clip.w - width) / 2, baseline, &fitted, fg);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Icon {
    Close,
    Back,
    Forward,
    Up,
    Down,
    Search,
    Check,
}

/// Vector glyphs; `bg` is the colour the icon sits on (for the ring cut-out).
#[allow(clippy::too_many_arguments)]
pub(super) fn icon(
    buf: &mut [u32],
    stride: usize,
    kind: Icon,
    center: (f32, f32),
    size: usize,
    color: u32,
    bg: u32,
    s: &Sizes,
) {
    let stroke = (s.focus as f32 * 0.65).max(1.0);
    let (cx, cy) = center;
    let h = size as f32 / 2.0;
    let q = size as f32 / 4.0;
    match kind {
        Icon::Close => {
            chrome::line(buf, stride, (cx - h, cy - h), (cx + h, cy + h), stroke, color);
            chrome::line(buf, stride, (cx - h, cy + h), (cx + h, cy - h), stroke, color);
        }
        Icon::Back | Icon::Forward => {
            let (tip, tail) = if kind == Icon::Back { (cx - q, cx + q) } else { (cx + q, cx - q) };
            chrome::line(buf, stride, (tail, cy - h), (tip, cy), stroke, color);
            chrome::line(buf, stride, (tip, cy), (tail, cy + h), stroke, color);
        }
        Icon::Up | Icon::Down => {
            let (tip, tail) = if kind == Icon::Up { (cy - q, cy + q) } else { (cy + q, cy - q) };
            chrome::line(buf, stride, (cx - h, tail), (cx, tip), stroke, color);
            chrome::line(buf, stride, (cx, tip), (cx + h, tail), stroke, color);
        }
        Icon::Search => {
            let r = h * 0.72;
            let (ox, oy) = (cx - h * 0.18, cy - h * 0.18);
            chrome::disc(buf, stride, (ox, oy), r, color);
            chrome::disc(buf, stride, (ox, oy), (r - stroke).max(0.5), bg);
            let d = r * std::f32::consts::FRAC_1_SQRT_2;
            chrome::line(buf, stride, (ox + d, oy + d), (cx + h, cy + h), stroke, color);
        }
        Icon::Check => {
            chrome::line(buf, stride, (cx - h, cy), (cx - q * 0.5, cy + h * 0.7), stroke * 1.2, color);
            chrome::line(buf, stride, (cx - q * 0.5, cy + h * 0.7), (cx + h, cy - h * 0.7), stroke * 1.2, color);
        }
    }
}

/// Square icon button (close, back, page arrows).
#[allow(clippy::too_many_arguments)]
pub(super) fn icon_button(
    buf: &mut [u32],
    stride: usize,
    rect: Rect,
    kind: Icon,
    hovered: bool,
    enabled: bool,
    c: &Colors,
    s: &Sizes,
) {
    let bg = if hovered && enabled { c.hover } else { c.surface };
    if hovered && enabled {
        chrome::round_rect(buf, stride, rect, s.radius, bg);
    }
    let color = if enabled { c.fg } else { c.faint };
    let center = (rect.x as f32 + rect.w as f32 / 2.0, rect.y as f32 + rect.h as f32 / 2.0);
    icon(buf, stride, kind, center, s.icon.min(rect.w / 2).min(rect.h / 2).max(1), color, bg, s);
}

/// Windows 11 style toggle switch, drawn at the left of `rect` and vertically
/// centred; the state text follows it. Returns the switch track rectangle.
#[allow(clippy::too_many_arguments)]
pub(super) fn switch(
    buf: &mut [u32],
    stride: usize,
    font: &mut font::UiFont,
    rect: Rect,
    on: bool,
    hovered: bool,
    c: &Colors,
    s: &Sizes,
) -> Rect {
    let (w, h) = (s.switch_w.min(rect.w), s.switch_h.min(rect.h));
    let track = Rect { x: rect.x, y: rect.y + (rect.h - h) / 2, w, h };
    let radius = h / 2;
    let knob = (h as f32 / 2.0 - s.focus as f32 * 1.5).max(1.0) * if hovered { 1.12 } else { 1.0 };
    let cy = track.y as f32 + h as f32 / 2.0;
    if on {
        let fill = if hovered { chrome::mix(c.accent, c.fg, 0x30) } else { c.accent };
        chrome::round_rect(buf, stride, track, radius, fill);
        let cx = (track.x + track.w) as f32 - h as f32 / 2.0;
        chrome::disc(buf, stride, (cx, cy), knob, c.on_accent);
    } else {
        chrome::round_rect(buf, stride, track, radius, c.muted);
        let fill = if hovered { c.control_hover } else { c.control };
        chrome::round_rect(buf, stride, inset(track, s.hairline), radius.saturating_sub(s.hairline), fill);
        let cx = track.x as f32 + h as f32 / 2.0;
        chrome::disc(buf, stride, (cx, cy), knob, c.muted);
    }
    let text = Rect { x: track.x + track.w + s.gap, y: rect.y, w: rect.w.saturating_sub(track.w + s.gap), h: rect.h };
    label(buf, stride, font, text, if on { "On" } else { "Off" }, c.muted);
    track
}

/// Segment rectangles for `options` inside `rect`, or None when they do not fit
/// (callers then show a single cycling choice button).
pub(super) fn segments(font: &mut font::UiFont, rect: Rect, options: &[&str], s: &Sizes) -> Option<Vec<Rect>> {
    let widths: Vec<usize> = options.iter().map(|option| font.measure(option, 400) + 2 * s.inner + s.gap).collect();
    let total: usize = widths.iter().sum::<usize>() + 2 * s.hairline;
    if options.is_empty() || total > rect.w {
        return None;
    }
    let h = s.control.min(rect.h);
    let (mut x, y) = (rect.x + s.hairline, rect.y + (rect.h - h) / 2);
    Some(
        widths
            .into_iter()
            .map(|w| {
                let segment = Rect { x, y: y + s.hairline, w, h: h.saturating_sub(2 * s.hairline) };
                x += w;
                segment
            })
            .collect(),
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn segmented(
    buf: &mut [u32],
    stride: usize,
    font: &mut font::UiFont,
    rects: &[Rect],
    options: &[&str],
    selected: Option<usize>,
    hovered: Option<usize>,
    c: &Colors,
    s: &Sizes,
) {
    let (Some(first), Some(last)) = (rects.first(), rects.last()) else { return };
    let outer = Rect {
        x: first.x - s.hairline,
        y: first.y - s.hairline,
        w: last.x + last.w + s.hairline - (first.x - s.hairline),
        h: first.h + 2 * s.hairline,
    };
    chrome::round_rect(buf, stride, outer, s.radius, c.border);
    chrome::round_rect(buf, stride, inset(outer, s.hairline), s.radius.saturating_sub(s.hairline), c.control);
    for (index, (rect, option)) in rects.iter().zip(options).enumerate() {
        let chosen = selected == Some(index);
        let fg = if chosen {
            if hovered == Some(index) {
                chrome::round_rect(buf, stride, *rect, s.radius, chrome::mix(c.accent, c.fg, 0x30));
            } else {
                chrome::round_rect(buf, stride, *rect, s.radius, c.accent);
            }
            c.on_accent
        } else {
            if hovered == Some(index) {
                chrome::round_rect(buf, stride, *rect, s.radius, c.control_hover);
            }
            c.fg
        };
        let clip = inset(*rect, s.inner / 2);
        let text = font.fit(option, clip.w);
        let width = font.measure(&text, clip.w);
        let baseline = font.baseline(rect.y, rect.h);
        font.draw(buf, stride, clip, clip.x + (clip.w - width) / 2, baseline, &text, fg);
    }
}

/// Checkbox at the left of `rect`, label after it. Returns the box rectangle.
// Used by the file-type registration page once ROOT adopts its backend.
#[allow(dead_code)]
#[allow(clippy::too_many_arguments)]
pub(super) fn checkbox(
    buf: &mut [u32],
    stride: usize,
    font: &mut font::UiFont,
    rect: Rect,
    text: &str,
    checked: bool,
    hovered: bool,
    enabled: bool,
    c: &Colors,
    s: &Sizes,
) -> Rect {
    let size = s.check.min(rect.h);
    let r = Rect { x: rect.x, y: rect.y + (rect.h - size) / 2, w: size, h: size };
    if checked {
        let fill = if !enabled {
            c.faint
        } else if hovered {
            chrome::mix(c.accent, c.fg, 0x30)
        } else {
            c.accent
        };
        chrome::round_rect(buf, stride, r, s.radius, fill);
        let center = (r.x as f32 + size as f32 / 2.0, r.y as f32 + size as f32 / 2.0);
        icon(buf, stride, Icon::Check, center, size / 2, c.on_accent, fill, s);
    } else {
        chrome::round_rect(buf, stride, r, s.radius, if enabled { c.muted } else { c.faint });
        let fill = if hovered && enabled { c.control_hover } else { c.control };
        chrome::round_rect(buf, stride, inset(r, s.hairline), s.radius.saturating_sub(s.hairline), fill);
    }
    let text_rect = Rect { x: r.x + size + s.gap, y: rect.y, w: rect.w.saturating_sub(size + s.gap), h: rect.h };
    label(buf, stride, font, text_rect, text, if enabled { c.fg } else { c.faint });
    r
}

/// Width of keycaps for `keys` ("ctrl+s ctrl+k"), at most `cap` pixels.
pub(super) fn keycaps_width(font: &mut font::UiFont, keys: &str, s: &Sizes, cap: usize) -> usize {
    let mut used = 0usize;
    for (chord_index, chord) in menu::pretty(&visible(keys, 128)).split(' ').take(8).enumerate() {
        if chord_index > 0 {
            used += s.gap;
        }
        for (part_index, part) in chord.split('+').filter(|p| !p.is_empty()).enumerate() {
            if part_index > 0 {
                used += s.hairline * 3;
            }
            used += font.measure(part, 200) + s.inner;
            if used >= cap {
                return cap;
            }
        }
    }
    used.min(cap)
}

/// Keycap chips for a shortcut, clipped to `clip`; returns the pen position.
#[allow(clippy::too_many_arguments)]
pub(super) fn keycaps(
    buf: &mut [u32],
    stride: usize,
    font: &mut font::UiFont,
    clip: Rect,
    x: usize,
    keys: &str,
    c: &Colors,
    s: &Sizes,
) -> usize {
    let h = (s.text + s.focus).min(clip.h);
    let y = clip.y + (clip.h - h) / 2;
    let right = clip.x + clip.w;
    let baseline = font.baseline(y, h);
    let mut pen = x;
    for (chord_index, chord) in menu::pretty(&visible(keys, 128)).split(' ').take(8).enumerate() {
        if chord_index > 0 {
            pen += s.gap;
        }
        for (part_index, part) in chord.split('+').filter(|p| !p.is_empty()).enumerate() {
            if part_index > 0 {
                pen += s.hairline * 3;
            }
            let w = font.measure(part, 200) + s.inner;
            if pen + w > right {
                let rest = Rect { x: pen.min(right), y: clip.y, w: right.saturating_sub(pen), h: clip.h };
                label(buf, stride, font, rest, "…", c.muted);
                return right;
            }
            let chip = Rect { x: pen, y, w, h };
            chrome::round_rect(buf, stride, chip, s.radius, c.border);
            let inner = Rect { h: h.saturating_sub(s.hairline * 2), ..inset(chip, s.hairline) };
            chrome::round_rect(buf, stride, inner, s.radius.saturating_sub(s.hairline), c.control);
            font.draw(buf, stride, chip, pen + s.inner / 2, baseline, part, c.fg);
            pen += w;
        }
    }
    pen
}

/// List or navigation row background: hover tint, or selection with an accent
/// indicator at the leading edge.
pub(super) fn row(buf: &mut [u32], stride: usize, rect: Rect, selected: bool, hovered: bool, c: &Colors, s: &Sizes) {
    if selected {
        chrome::round_rect(buf, stride, rect, s.radius, c.selected);
        let h = (rect.h / 2).max(1);
        let bar = Rect { x: rect.x, y: rect.y + (rect.h - h) / 2, w: s.indicator.min(rect.w), h };
        chrome::round_rect(buf, stride, bar, s.indicator / 2, c.accent);
    } else if hovered {
        chrome::round_rect(buf, stride, rect, s.radius, c.hover);
    }
}

/// Row background on the sidebar colour.
pub(super) fn nav_row(
    buf: &mut [u32],
    stride: usize,
    rect: Rect,
    selected: bool,
    hovered: bool,
    c: &Colors,
    s: &Sizes,
) {
    chrome::fill_clipped(buf, stride, rect, c.sidebar);
    if selected {
        chrome::round_rect(buf, stride, rect, s.radius, chrome::mix(c.sidebar, c.fg, 0x18));
        let h = (rect.h / 2).max(1);
        let bar = Rect { x: rect.x, y: rect.y + (rect.h - h) / 2, w: s.indicator.min(rect.w), h };
        chrome::round_rect(buf, stride, bar, s.indicator / 2, c.accent);
    } else if hovered {
        chrome::round_rect(buf, stride, rect, s.radius, chrome::mix(c.sidebar, c.fg, 0x0e));
    }
}

/// Thin overlay scroll thumb for a list of `total` rows showing `visible` from
/// `first`. Nothing is drawn when everything fits.
#[allow(clippy::too_many_arguments)]
pub(super) fn scroll_thumb(
    buf: &mut [u32],
    stride: usize,
    track: Rect,
    first: usize,
    visible: usize,
    total: usize,
    c: &Colors,
    s: &Sizes,
) {
    if total <= visible || track.h == 0 || visible == 0 {
        return;
    }
    let min = (s.control).min(track.h);
    let h = (track.h as u128 * visible as u128 / total as u128) as usize;
    let h = h.clamp(min, track.h);
    let room = track.h - h;
    let max_first = total - visible;
    let y = track.y + (room as u128 * first.min(max_first) as u128 / max_first.max(1) as u128) as usize;
    let thumb = Rect { x: track.x + track.w.saturating_sub(s.thumb), y, w: s.thumb.min(track.w), h };
    chrome::round_rect(buf, stride, thumb, s.thumb / 2, c.faint);
}

/// First visible index that keeps `selected` inside a window of `visible`
/// rows, moving `scroll` as little as possible and never past the end.
pub(super) fn reveal(scroll: usize, selected: usize, visible: usize, total: usize) -> usize {
    let visible = visible.max(1);
    let scroll = if selected < scroll {
        selected
    } else if selected >= scroll + visible {
        selected + 1 - visible
    } else {
        scroll
    };
    scroll.min(total.saturating_sub(visible))
}

/// Wheel step on a list: three rows, clamped to the scrollable range.
pub(super) fn wheel(scroll: usize, up: bool, visible: usize, total: usize) -> usize {
    let max = total.saturating_sub(visible.max(1));
    if up { scroll.saturating_sub(3) } else { scroll.saturating_add(3).min(max) }
}

impl App {
    /// Render admission for card-only repaints (LIB-HOOKS-R1 hunk 2). True
    /// while any overlay asked for a frame through its `Paint`.
    pub(super) fn overlay_paint_pending(&self) -> bool {
        self.settings_view.paint.pending()
            || self.palette_view.paint.pending()
            || self.prompt_view.paint.pending()
            || self.welcome_view.paint.pending()
    }

    /// Consumed once per admitted render; the open cards then repaint in place.
    pub(super) fn take_overlay_paint(&mut self) {
        self.settings_view.paint.take();
        self.palette_view.paint.take();
        self.prompt_view.paint.take();
        self.welcome_view.paint.take();
    }

    /// The surface overlays are painted for right now.
    pub(super) fn overlay_surface(&self) -> Surface {
        Surface::new(self.size, &self.ui)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colors() -> Colors {
        Colors::new(theme_ui::chrome())
    }

    #[test]
    fn colours_come_from_the_theme_roles_and_keep_a_visible_hierarchy() {
        let theme = theme_ui::chrome();
        let c = colors();
        assert_eq!(c.backdrop, theme_ui::rgb(theme, UiRole::Background));
        assert_eq!(c.accent, theme_ui::rgb(theme, UiRole::Accent));
        assert_eq!(c.border, theme_ui::rgb(theme, UiRole::Border));
        let light = Colors::new(&Theme::builtin(kwil_core_commands::theme::Scheme::Light));
        for c in [c, light] {
            let distinct = [c.surface, c.control, c.hover, c.selected, c.fg, c.muted, c.faint];
            for (i, a) in distinct.iter().enumerate() {
                for b in &distinct[i + 1..] {
                    assert_ne!(a, b, "two hierarchy levels share a colour");
                }
            }
        }
    }

    #[test]
    fn sizes_follow_monitor_dpi_only() {
        let (one, two) = (Sizes::new(&Metrics::new(1.0)), Sizes::new(&Metrics::new(2.0)));
        assert_eq!(two.control, 2 * one.control);
        assert!(one.control >= 32 && one.nav >= 32, "comfortable click targets at 100%");
        let fractional = Sizes::new(&Metrics::new(1.25));
        assert_eq!(fractional.control, 40);
    }

    #[test]
    fn reveal_and_wheel_keep_the_selection_reachable_and_bounded() {
        assert_eq!(reveal(0, 12, 5, 40), 8);
        assert_eq!(reveal(10, 3, 5, 40), 3);
        assert_eq!(reveal(10, 12, 5, 40), 10);
        assert_eq!(reveal(50, 39, 5, 40), 35, "never past the end");
        assert_eq!(reveal(0, 0, 0, 0), 0);
        assert_eq!(wheel(0, true, 5, 40), 0);
        assert_eq!(wheel(34, false, 5, 40), 35);
        assert_eq!(wheel(3, false, 50, 40), 0, "short lists never scroll");
    }

    #[test]
    fn card_repaints_are_requested_once_and_surfaces_change_with_size_and_dpi() {
        let mut paint = Paint::default();
        assert!(!paint.pending());
        paint.request();
        paint.request();
        assert!(paint.pending());
        assert!(paint.take(), "a requested card repaint is admitted once");
        assert!(!paint.take() && !paint.pending(), "an admitted request leaves no idle repeat");
        let (one, two) = (Metrics::new(1.0), Metrics::new(1.5));
        assert_eq!(Surface::new((800, 600), &one), Surface::new((800, 600), &one));
        assert_ne!(Surface::new((800, 600), &one), Surface::new((800, 300), &one), "resize");
        assert_ne!(Surface::new((800, 600), &one), Surface::new((800, 600), &two), "DPI");
    }

    #[test]
    #[cfg(windows)]
    fn clear_buttons_are_drawn_only_where_a_clear_action_exists() {
        let mut font = font::UiFont::load(14.0).expect("Windows chrome face");
        let c = colors();
        let s = Sizes::new(&Metrics::new(1.0));
        let (w, h) = (240, 52);
        let rect = Rect { x: 10, y: 10, w: 200, h: 32 };
        let paint = |clear: bool, font: &mut font::UiFont| {
            let mut pixels = vec![0; w * h];
            let field = Field {
                text: "query",
                cursor: 5,
                placeholder: "",
                focused: false,
                hovered: false,
                search: true,
                clear,
            };
            text_field(&mut pixels, w, font, rect, field, &c, &s);
            pixels
        };
        let (with, without) = (paint(true, &mut font), paint(false, &mut font));
        let button = field_clear(rect, &s);
        let differs = (button.y..button.y + button.h)
            .any(|y| (button.x..button.x + button.w).any(|x| with[y * w + x] != without[y * w + x]));
        assert!(differs, "the clear glyph is drawn only when the caller offers the action");
    }

    #[test]
    fn hits_prefer_the_topmost_target_and_skip_empty_rects() {
        let mut hits = Hits::default();
        hits.push(Rect { x: 0, y: 0, w: 100, h: 100 }, 1);
        hits.push(Rect { x: 10, y: 10, w: 10, h: 10 }, 2);
        hits.push(Rect { x: 50, y: 50, w: 0, h: 10 }, 3);
        assert_eq!(hits.at((12, 12)), Some(2));
        assert_eq!(hits.at((50, 55)), Some(1));
        assert_eq!(hits.at((200, 5)), None);
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn widgets_are_idempotent_and_stay_inside_their_rectangles() {
        // Repainting must not darken anti-aliased edges, and nothing may leak
        // outside the widget (that would need wider damage than the overlay).
        let (w, h) = (120, 60);
        let c = colors();
        let s = Sizes::new(&Metrics::new(1.0));
        let rect = Rect { x: 10, y: 10, w: 80, h: 32 };
        let mut first = vec![0x123456; w * h];
        card(&mut first, w, rect, &c, &s);
        row(&mut first, w, inset(rect, 4), true, false, &c, &s);
        let mut second = first.clone();
        card(&mut second, w, rect, &c, &s);
        row(&mut second, w, inset(rect, 4), true, false, &c, &s);
        assert_eq!(first, second, "a second paint changed pixels");
        for y in 0..h {
            for x in 0..w {
                if !rect.contains((x, y)) {
                    assert_eq!(first[y * w + x], 0x123456, "paint leaked at {x},{y}");
                }
            }
        }
    }

    #[test]
    #[cfg(windows)]
    fn text_widgets_fit_bounded_text_and_place_the_caret_inside_the_field() {
        let mut font = font::UiFont::load(14.0).expect("Windows chrome face");
        let c = colors();
        let s = Sizes::new(&Metrics::new(1.0));
        let (w, h) = (400, 60);
        let mut pixels = vec![0; w * h];
        let huge: String = "word ".repeat(100_000);
        let rect = Rect { x: 10, y: 10, w: 200, h: 32 };
        let caret = text_field(
            &mut pixels,
            w,
            &mut font,
            rect,
            Field {
                text: &huge,
                cursor: huge.len(),
                placeholder: "",
                focused: true,
                hovered: false,
                search: true,
                clear: true,
            },
            &c,
            &s,
        )
        .expect("focused field has a caret");
        assert!(rect.contains((caret.x, caret.y)), "caret {caret:?} outside {rect:?}");
        let lines = wrap(&mut font, &huge, 150, 2);
        assert_eq!(lines.len(), 2);
        assert!(lines[1].ends_with('…'));
        for line in &lines {
            assert!(font.measure(line, usize::MAX) <= 150);
        }
        assert!(segments(&mut font, Rect { x: 0, y: 0, w: 60, h: 32 }, &["horizontal", "vertical"], &s).is_none());
        let fits = segments(&mut font, Rect { x: 0, y: 0, w: 300, h: 32 }, &["auto", "on", "off"], &s).unwrap();
        assert!(fits.windows(2).all(|pair| pair[0].x + pair[0].w == pair[1].x));
    }
}
