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
    /// State ramp (`chrome::ramp`), each over the surface a row or control sits on.
    pub hover: u32,
    pub pressed: u32,
    pub selected: u32,
    /// The theme's selection role; rows blend it over their own ground.
    pub selection: u32,
    /// Focus ring colour; drawn `Sizes::focus` thick.
    pub focus_ring: u32,
    pub fg: u32,
    pub muted: u32,
    pub faint: u32,
    /// Text and glyphs of a control that cannot act.
    pub disabled: u32,
    pub border: u32,
    pub accent: u32,
    /// Accent surface under the pointer; accents need the pressed step to read.
    pub accent_hover: u32,
    pub on_accent: u32,
    pub error: u32,
}

impl Colors {
    pub fn new(theme: &Theme) -> Self {
        let rgb = |role| theme_ui::rgb(theme, role);
        let panel = rgb(UiRole::PanelBackground);
        let fg = rgb(UiRole::PanelForeground);
        let surface = chrome::mix(panel, fg, 0x0a);
        let control = chrome::mix(panel, fg, 0x16);
        Self {
            backdrop: rgb(UiRole::Background),
            surface,
            sidebar: panel,
            control,
            control_hover: chrome::hover_color(control, fg),
            hover: chrome::hover_color(surface, fg),
            pressed: chrome::pressed_color(surface, fg),
            selected: chrome::selected_color(surface, rgb(UiRole::Selection)),
            selection: rgb(UiRole::Selection),
            focus_ring: rgb(UiRole::Accent),
            fg,
            muted: chrome::mix(surface, fg, 0xa8),
            faint: chrome::mix(surface, fg, chrome::ramp::FAINT),
            disabled: chrome::disabled_color(surface, fg),
            border: rgb(UiRole::Border),
            accent: rgb(UiRole::Accent),
            accent_hover: chrome::pressed_color(rgb(UiRole::Accent), fg),
            on_accent: panel,
            error: rgb(UiRole::Error),
        }
    }
}

/// Physical sizes for one monitor scale: the DIP token table (`chrome::tok`)
/// scaled by the monitor DPI. Editor zoom never reaches them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Sizes {
    pub hairline: usize,
    /// Spacing scale: 2, 4, 8, 12, 16, 24, 32 DIP.
    pub xxs: usize,
    pub xs: usize,
    pub sm: usize,
    pub md: usize,
    pub lg: usize,
    pub xl: usize,
    pub xxl: usize,
    /// Radii: controls, rows and chips (`radius`, the small step); cards,
    /// menus, popups and the tab sheet (`card_radius`, the medium step). A
    /// pill is `pill(height)`; an inner fill is `fill_radius(outer)`.
    pub radius: usize,
    pub card_radius: usize,
    /// Outer padding of a card section (`lg`).
    pub pad: usize,
    /// Inner padding of a control.
    pub inner: usize,
    /// Gap between siblings (`sm`).
    pub gap: usize,
    /// Height of fields and buttons: a comfortable click target.
    pub control: usize,
    /// Dropdown and context menu items.
    pub menu_item: usize,
    /// Palette, picker and settings navigation rows; `list_row_tall` holds two lines.
    pub list_row: usize,
    pub list_row_tall: usize,
    /// One line of chrome text.
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
        use chrome::tok;
        let spacing = [tok::XXS, tok::XS, tok::SM, tok::MD, tok::LG, tok::XL, tok::XXL].map(|dip| m.dip(dip));
        Self {
            hairline: m.line,
            xxs: spacing[0],
            xs: spacing[1],
            sm: spacing[2],
            md: spacing[3],
            lg: spacing[4],
            xl: spacing[5],
            xxl: spacing[6],
            radius: m.dip(tok::RADIUS_SM),
            card_radius: m.dip(tok::RADIUS_MD),
            pad: spacing[4],
            inner: m.dip(10.0),
            gap: spacing[2],
            control: m.dip(tok::CONTROL),
            menu_item: m.dip(tok::MENU_ITEM),
            list_row: m.dip(tok::LIST_ROW),
            list_row_tall: m.dip(tok::LIST_ROW_TALL),
            text: m.dip(tok::TEXT),
            header: m.dip(52.0),
            footer: m.dip(40.0),
            switch_w: m.dip(40.0),
            switch_h: m.dip(20.0),
            check: m.dip(18.0),
            focus: m.dip(tok::XXS),
            indicator: m.dip(tok::INDICATOR),
            icon: m.dip(10.0),
            thumb: m.dip(tok::XS),
        }
    }

    /// Radius of a fully rounded end for a control `h` tall.
    pub fn pill(&self, h: usize) -> usize {
        h / 2
    }

    /// Radius of the fill inside a one-hairline border of radius `outer`.
    pub fn fill_radius(&self, outer: usize) -> usize {
        outer.saturating_sub(self.hairline)
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
    chrome::round_rect(buf, stride, inset(rect, s.hairline), s.fill_radius(s.card_radius), c.surface);
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
    chrome::round_rect(buf, stride, inset(rect, s.hairline), s.fill_radius(s.radius), bg);
    if field.focused {
        // Windows 11 field focus: a thicker accent edge along the bottom.
        let y = (rect.y + rect.h).saturating_sub(s.focus + s.hairline);
        chrome::fill_clipped(
            buf,
            stride,
            Rect { x: rect.x + s.radius, y, w: rect.w.saturating_sub(2 * s.radius), h: s.focus },
            c.focus_ring,
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
    if !field.focused {
        // An inactive field has no caret to explain horizontal scrolling.
        // Preserve its identifying suffix and mark any omitted prefix.
        let text = font.fit_tail(field.text, clip.w);
        font.draw(buf, stride, clip, left, baseline, &text, c.fg);
        return None;
    }
    // Cells are bounded by the widest Latin advance, so the window fits in
    // pixels too; wide CJK cells (2 columns) are narrower than two `M`s.
    let columns = (clip.w / font.width("M").max(1)).max(1);
    let window = kwil_view_model::palette_session::text_window(field.text, field.cursor, columns);
    font.draw(buf, stride, clip, left, baseline, &window.text, c.fg);
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

/// Keep the full control height for accents and descenders. Inner padding
/// controls the label's horizontal fit, not its vertical ink bounds.
pub(super) fn button_clip(rect: Rect, padding: usize) -> Rect {
    let dx = padding.min(rect.w / 4);
    Rect { x: rect.x + dx, w: rect.w - 2 * dx, ..rect }
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
    focused: bool,
    c: &Colors,
    s: &Sizes,
) {
    let (bg, fg, edge) = match style {
        Style::Primary => (if hovered { c.accent_hover } else { c.accent }, c.on_accent, None),
        Style::Secondary => (if hovered { c.control_hover } else { c.control }, c.fg, Some(c.border)),
        Style::Danger => (if hovered { c.control_hover } else { c.control }, c.error, Some(c.border)),
        Style::Subtle => (if hovered { c.hover } else { c.surface }, c.fg, None),
    };
    match edge {
        Some(edge) => {
            chrome::round_rect(buf, stride, rect, s.radius, edge);
            chrome::round_rect(buf, stride, inset(rect, s.hairline), s.fill_radius(s.radius), bg);
        }
        // A resting subtle button is text on whatever row it sits on.
        None if style == Style::Subtle && !hovered => {}
        None => chrome::round_rect(buf, stride, rect, s.radius, bg),
    }
    let clip = button_clip(rect, s.inner);
    let fitted = font.fit(text, clip.w);
    let width = font.measure(&fitted, clip.w);
    let baseline = font.baseline(rect.y, rect.h);
    font.draw(buf, stride, clip, clip.x + (clip.w - width) / 2, baseline, &fitted, fg);
    if focused {
        focus_ring(buf, stride, rect, s.radius, bg, c, s);
    }
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
    let kind = match kind {
        Icon::Close => icons::Icon::Close,
        Icon::Back => icons::Icon::Back,
        Icon::Forward => icons::Icon::Forward,
        Icon::Up => icons::Icon::Up,
        Icon::Down => icons::Icon::Down,
        Icon::Search => icons::Icon::Search,
        Icon::Check => icons::Icon::Check,
    };
    let _ = (bg, s); // SVG masks blend against the actual destination surface.
    icons::draw(buf, stride, kind, center, size.saturating_mul(3) / 2, color);
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
    let color = if enabled { c.fg } else { c.disabled };
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
    focused: bool,
    c: &Colors,
    s: &Sizes,
) -> Rect {
    let (w, h) = (s.switch_w.min(rect.w), s.switch_h.min(rect.h));
    let track = Rect { x: rect.x, y: rect.y + (rect.h - h) / 2, w, h };
    let radius = s.pill(h);
    let knob = (h as f32 / 2.0 - s.focus as f32 * 1.5).max(1.0) * if hovered { 1.12 } else { 1.0 };
    let cy = track.y as f32 + h as f32 / 2.0;
    if on {
        let fill = if hovered { c.accent_hover } else { c.accent };
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
    if focused {
        focus_ring(buf, stride, track, radius, if on { c.accent } else { c.control }, c, s);
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
    focused: Option<usize>,
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
    chrome::round_rect(buf, stride, inset(outer, s.hairline), s.fill_radius(s.radius), c.control);
    for (index, (rect, option)) in rects.iter().zip(options).enumerate() {
        let chosen = selected == Some(index);
        let fg = if chosen {
            if hovered == Some(index) {
                chrome::round_rect(buf, stride, *rect, s.radius, c.accent_hover);
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
        let clip = button_clip(*rect, s.inner / 2);
        let text = font.fit(option, clip.w);
        let width = font.measure(&text, clip.w);
        let baseline = font.baseline(rect.y, rect.h);
        font.draw(buf, stride, clip, clip.x + (clip.w - width) / 2, baseline, &text, fg);
    }
    if let Some(index) = focused.filter(|index| *index < rects.len()) {
        let over = if selected == Some(index) { c.accent } else { c.control };
        focus_ring(buf, stride, rects[index], s.radius, over, c, s);
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
    focused: bool,
    enabled: bool,
    c: &Colors,
    s: &Sizes,
) -> Rect {
    let size = s.check.min(rect.h);
    let r = Rect { x: rect.x, y: rect.y + (rect.h - size) / 2, w: size, h: size };
    if checked {
        let fill = if !enabled {
            c.disabled
        } else if hovered {
            c.accent_hover
        } else {
            c.accent
        };
        chrome::round_rect(buf, stride, r, s.radius, fill);
        let center = (r.x as f32 + size as f32 / 2.0, r.y as f32 + size as f32 / 2.0);
        icon(buf, stride, Icon::Check, center, size / 2, c.on_accent, fill, s);
    } else {
        chrome::round_rect(buf, stride, r, s.radius, if enabled { c.muted } else { c.disabled });
        let fill = if hovered && enabled { c.control_hover } else { c.control };
        chrome::round_rect(buf, stride, inset(r, s.hairline), s.fill_radius(s.radius), fill);
    }
    if focused && enabled {
        focus_ring(buf, stride, r, s.radius, if checked { c.accent } else { c.control }, c, s);
    }
    let text_rect = Rect { x: r.x + size + s.gap, y: rect.y, w: rect.w.saturating_sub(size + s.gap), h: rect.h };
    label(buf, stride, font, text_rect, text, if enabled { c.fg } else { c.disabled });
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
            chrome::round_rect(buf, stride, inner, s.fill_radius(s.radius), c.control);
            font.draw(buf, stride, chip, pen + s.inner / 2, baseline, part, c.fg);
            pen += w;
        }
    }
    pen
}

/// What a row shows. A list's keyboard position is its selected row (the
/// Selection fill plus the accent indicator), so rows never draw a focus
/// ring: mouse and keyboard highlights look the same in every list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct RowState {
    pub selected: bool,
    pub hovered: bool,
}

impl RowState {
    pub fn new(selected: bool, hovered: bool) -> Self {
        Self { selected, hovered }
    }
}

/// The ground a row sits on and the corner shape it takes there. Every list
/// (palette, menus, settings, sidebar, search and log results) derives its
/// hover and selected tint from the same ramp over its own ground, so a
/// selected row is the Selection role plus the accent indicator everywhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RowLook {
    pub hover: u32,
    pub selected: u32,
    pub accent: u32,
    pub radius: usize,
}

impl RowLook {
    pub fn on(c: &Colors, ground: u32, radius: usize) -> Self {
        Self {
            hover: chrome::hover_color(ground, c.fg),
            selected: chrome::selected_color(ground, c.selection),
            accent: c.accent,
            radius,
        }
    }
}

/// Accent bar at the leading edge of a selected row.
pub(super) fn row_indicator(buf: &mut [u32], stride: usize, rect: Rect, look: &RowLook, s: &Sizes) {
    let h = (rect.h / 2).max(1);
    let bar = Rect { x: rect.x, y: rect.y + (rect.h - h) / 2, w: s.indicator.min(rect.w), h };
    chrome::round_rect(buf, stride, bar, s.pill(s.indicator), look.accent);
}

/// Row tint only: the selection or hover fill without the indicator or ring,
/// for lists whose text is painted on a grid that would overwrite them.
pub(super) fn row_fill(buf: &mut [u32], stride: usize, rect: Rect, state: RowState, look: &RowLook) {
    if state.selected {
        chrome::round_rect(buf, stride, rect, look.radius, look.selected);
    } else if state.hovered {
        chrome::round_rect(buf, stride, rect, look.radius, look.hover);
    }
}

/// The one list-row painter: hover tint, or selection with the accent indicator.
pub(super) fn row_in(buf: &mut [u32], stride: usize, rect: Rect, state: RowState, look: &RowLook, s: &Sizes) {
    row_fill(buf, stride, rect, state, look);
    if state.selected {
        row_indicator(buf, stride, rect, look, s);
    }
}

/// List row on a card or popup surface.
pub(super) fn row(buf: &mut [u32], stride: usize, rect: Rect, state: RowState, c: &Colors, s: &Sizes) {
    row_in(buf, stride, rect, state, &RowLook::on(c, c.surface, s.radius), s);
}

/// Row on the sidebar colour: the same painter over the sidebar ground.
pub(super) fn nav_row(buf: &mut [u32], stride: usize, rect: Rect, state: RowState, c: &Colors, s: &Sizes) {
    chrome::fill_clipped(buf, stride, rect, c.sidebar);
    row_in(buf, stride, rect, state, &RowLook::on(c, c.sidebar, s.radius), s);
}

/// Keyboard focus ring: accent, `Sizes::focus` thick, inside `rect` so the
/// widget's own damage always covers it, following the widget's `radius`.
/// `over` is the fill it sits on; an accent fill takes the on-accent text
/// colour (readable on accent by construction), so the ring stays visible.
pub(super) fn focus_ring(buf: &mut [u32], stride: usize, rect: Rect, radius: usize, over: u32, c: &Colors, s: &Sizes) {
    let color = if over == c.accent || over == c.accent_hover { c.on_accent } else { c.focus_ring };
    chrome::ring(buf, stride, rect, radius, s.focus, color);
}

/// Frame of a menu or dropdown list: rounded `card_radius`, a hairline border
/// and the surface fill. Popups sit over content that the host restores
/// before every repaint, so unlike `card` it never paints the editor backdrop;
/// its outer corner is hard-edged so repainting it over itself changes nothing
/// (the harness checks that). Rows inside it start `Sizes::xs` in.
pub(super) fn popup(buf: &mut [u32], stride: usize, rect: Rect, c: &Colors, s: &Sizes) {
    chrome::round_rect_hard(buf, stride, rect, s.card_radius, c.border);
    chrome::round_rect(buf, stride, inset(rect, s.hairline), s.fill_radius(s.card_radius), c.surface);
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
    chrome::round_rect(buf, stride, thumb, s.pill(s.thumb), c.faint);
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
        self.logs.body_paint.pending()
            || self.logs.paint.pending()
            || self.settings_view.paint.pending()
            || self.palette_view.paint.pending()
            || self.prompt_view.paint.pending()
            || self.welcome_view.paint.pending()
    }

    /// Consumed once per admitted render; the open cards then repaint in place.
    pub(super) fn take_overlay_paint(&mut self) {
        self.logs.paint.take();
        self.logs.body_paint.take();
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
        Colors::new(&theme_ui::chrome())
    }

    #[test]
    fn polish_unfocused_path_shows_a_leading_ellipsis_and_the_complete_tail() {
        let Some(mut font) = font::UiFont::load(15.0) else { return };
        let s = Sizes::new(&Metrics::new(1.0));
        let c = colors();
        let expected_text = "…folder";
        let width = font.width(expected_text) + 2 * s.inner;
        let rect = Rect { x: 0, y: 0, w: width, h: s.control };
        let path = format!("{}folder", "W".repeat(300));
        let mut actual = vec![c.surface; width * rect.h];
        let mut expected = actual.clone();
        let field = Field {
            text: &path,
            cursor: path.len(),
            placeholder: "",
            focused: false,
            hovered: false,
            search: false,
            clear: false,
        };
        text_field(&mut actual, width, &mut font, rect, field, &c, &s);
        text_field(&mut expected, width, &mut font, rect, Field { text: "", ..field }, &c, &s);
        let clip = Rect { x: s.inner, w: width - 2 * s.inner, ..rect };
        font.draw(&mut expected, width, clip, clip.x, font.baseline(0, rect.h), expected_text, c.fg);
        assert!(actual == expected, "the omitted prefix needs a visible ellipsis and pixel-fitted suffix");
    }

    #[test]
    fn polish_buttons_preserve_complete_label_ink_at_fractional_scales() {
        for scale in [0.5, 0.75, 1.0, 1.25, 1.5, 1.75, 2.0, 3.0, 4.0, 8.0] {
            let m = Metrics::new(scale);
            let s = Sizes::new(&m);
            let Some(mut font) = font::UiFont::load(m.text_px()) else { continue };
            let c = colors();
            for text in
                ["Register selected", "Unregister selected", "Register all text types", "Change", "Open settings file"]
            {
                let width = font.width(text) + 2 * s.pad;
                let rect = Rect { x: 0, y: 0, w: width, h: s.control };
                let mut actual = vec![c.surface; width * rect.h];
                button(&mut actual, width, &mut font, rect, text, Style::Subtle, false, false, &c, &s);
                let mut expected = vec![c.surface; actual.len()];
                let text_width = font.width(text);
                font.draw(&mut expected, width, rect, (width - text_width) / 2, font.baseline(0, rect.h), text, c.fg);
                assert!(actual == expected, "complete label ink at scale {scale}: {text}");
            }
        }
    }

    #[test]
    fn colours_come_from_the_theme_roles_and_keep_a_visible_hierarchy() {
        let theme = &theme_ui::chrome();
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
    fn metrics_and_sizes_read_one_token_table() {
        use chrome::tok;
        for scale in [0.5, 1.0, 1.25, 1.5, 2.0, 3.0] {
            let (m, s) = (Metrics::new(scale), Sizes::new(&Metrics::new(scale)));
            // Every number that both structs expose is one token, never two.
            assert_eq!((m.md, m.xxl, m.radius), (s.md, s.xxl, s.radius), "spacing and radius at {scale}");
            assert_eq!((m.item, m.row, m.indicator), (s.menu_item, s.control, s.indicator), "heights at {scale}");
            assert_eq!((s.gap, s.pad, s.focus, s.thumb), (s.sm, s.lg, s.xxs, s.xs), "semantic names stay aliases");
            assert_eq!(s.text, m.dip(tok::TEXT));
            assert_eq!(m.text_px(), (tok::BODY * m.scale) as f32);
        }
        let s = Sizes::new(&Metrics::new(1.0));
        let scale = [s.xxs, s.xs, s.sm, s.md, s.lg, s.xl, s.xxl];
        assert_eq!(scale, [2, 4, 8, 12, 16, 24, 32]);
        assert_eq!((s.menu_item, s.list_row, s.list_row_tall), (32, 36, 44));
        assert_eq!((s.radius, s.card_radius, s.fill_radius(s.radius), s.pill(s.control)), (4, 8, 3, 16));
    }

    #[test]
    fn state_ramp_is_shared_by_cards_and_theme_coloured_chrome() {
        for theme in [theme_ui::chrome(), std::sync::Arc::new(Theme::builtin(kwil_core_commands::theme::Scheme::Light))]
        {
            let c = Colors::new(&theme);
            let fg = c.fg;
            assert_eq!(c.hover, chrome::mix(c.surface, fg, chrome::ramp::HOVER));
            assert_eq!(c.pressed, chrome::pressed_color(c.surface, fg));
            assert_eq!(c.disabled, chrome::disabled_color(c.surface, fg));
            assert_eq!(c.focus_ring, c.accent);
            assert_eq!(c.faint, chrome::mix(c.surface, fg, 0x7a));
            // Hover < pressed < selected on the same surface, and disabled text still reads as text.
            let distance = |a: u32, b: u32| {
                (0..3).map(|i| (i32::from((a >> (8 * i)) as u8) - i32::from((b >> (8 * i)) as u8)).abs()).sum::<i32>()
            };
            let (h, p) = (distance(c.surface, c.hover), distance(c.surface, c.pressed));
            assert!(0 < h && h < p, "hover {h} then pressed {p}");
            assert!(distance(c.surface, c.disabled) > distance(c.surface, c.faint) / 2);
        }
    }

    #[test]
    fn emphasized_diff_fill_keeps_the_role_hue_and_stays_inside_the_channel_range() {
        for scheme in [kwil_core_commands::theme::Scheme::Dark, kwil_core_commands::theme::Scheme::Light] {
            let theme = Theme::builtin(scheme);
            let base = theme_ui::rgb(&theme, UiRole::Background);
            for role in [UiRole::Added, UiRole::Removed] {
                let line = theme_ui::rgb(&theme, role);
                let word = chrome::emphasize(base, line);
                assert_ne!(word, line, "a changed word is stronger than its line");
                // Further from the editor background than the line fill is, on every channel.
                for shift in [16, 8, 0] {
                    let d = |v: u32| i32::from((v >> shift) as u8);
                    assert!((d(word) - d(base)).abs() >= (d(line) - d(base)).abs());
                }
            }
        }
        // A saturated role near the channel limit keeps its hue: red stays twice green.
        let word = chrome::emphasize(0x000000, 0xc06000);
        let (r, g) = ((word >> 16) & 0xff, (word >> 8) & 0xff);
        assert!(r == 0xff && r.abs_diff(2 * g) <= 2, "hue kept, got {word:06x}");
    }

    #[test]
    fn sizes_follow_monitor_dpi_only() {
        let (one, two) = (Sizes::new(&Metrics::new(1.0)), Sizes::new(&Metrics::new(2.0)));
        assert_eq!(two.control, 2 * one.control);
        assert!(one.control >= 32 && one.list_row >= 32, "comfortable click targets at 100%");
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
        row(&mut first, w, inset(rect, 4), RowState::new(true, false), &c, &s);
        let mut second = first.clone();
        card(&mut second, w, rect, &c, &s);
        row(&mut second, w, inset(rect, 4), RowState::new(true, false), &c, &s);
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
    fn a_focused_button_paints_the_ring_and_an_unfocused_one_does_not() {
        let mut font = font::UiFont::load(15.0).expect("Windows chrome face");
        let c = colors();
        let s = Sizes::new(&Metrics::new(1.0));
        let (w, h) = (160, 60);
        let rect = Rect { x: 20, y: 12, w: 120, h: s.control };
        for style in [Style::Primary, Style::Secondary, Style::Danger, Style::Subtle] {
            let mut paint = |focused| {
                let mut pixels = vec![c.surface; w * h];
                button(&mut pixels, w, &mut font, rect, "Save", style, false, focused, &c, &s);
                pixels
            };
            let (with, without) = (paint(true), paint(false));
            // An accent-filled button takes the on-accent ring so it stays visible.
            let ring = if style == Style::Primary { c.on_accent } else { c.focus_ring };
            let at = |pixels: &[u32], dy: usize| pixels[(rect.y + dy) * w + rect.x + rect.w / 2];
            for dy in 0..s.focus {
                assert_eq!(at(&with, dy), ring, "{style:?}: ring row {dy}");
                assert_ne!(at(&without, dy), ring, "{style:?}: an unfocused button has no ring at row {dy}");
            }
            assert_ne!(at(&with, s.focus), ring, "{style:?}: the ring is {} thick, not a fill", s.focus);
            for (i, (a, b)) in with.iter().zip(&without).enumerate() {
                if !rect.contains((i % w, i / w)) {
                    assert_eq!(a, b, "{style:?}: the ring leaked outside the button at {i}");
                }
            }
        }
    }

    #[test]
    fn a_popup_repaints_to_the_same_pixels_and_blends_nothing_at_its_corner() {
        // The host may draw a popup again over its own pixels; the first
        // paint over content must equal the repaint over the first paint.
        let (w, h) = (120, 90);
        let c = colors();
        let s = Sizes::new(&Metrics::new(2.0));
        let rect = Rect { x: 10, y: 10, w: 100, h: 70 };
        let mut once = vec![0x445566; w * h];
        popup(&mut once, w, rect, &c, &s);
        let mut twice = once.clone();
        popup(&mut twice, w, rect, &c, &s);
        assert_eq!(once, twice, "a second paint changed pixels");
        assert_eq!(once[rect.y * w + rect.x], 0x445566, "the extreme corner shows the content, not the frame");
        assert_eq!(once[rect.y * w + rect.x + rect.w / 2], c.border, "hairline border on the edge");
        assert_eq!(once[(rect.y + s.hairline) * w + rect.x + rect.w / 2], c.surface, "surface inside it");
    }

    #[test]
    fn keyboard_and_pointer_highlights_paint_the_same_row() {
        // A list's keyboard position is its selected row; no extra ring.
        let (w, h) = (120, 60);
        let c = colors();
        let s = Sizes::new(&Metrics::new(1.0));
        let rect = Rect { x: 10, y: 10, w: 100, h: s.list_row };
        let mut pixels = vec![0x123456; w * h];
        row(&mut pixels, w, rect, RowState::new(true, false), &c, &s);
        let top = pixels[rect.y * w + rect.x + rect.w / 2];
        assert_ne!(top, c.focus_ring, "a selected row has no focus ring on its edge");
        assert_eq!(top, chrome::selected_color(c.surface, c.selection), "the edge is the selected fill");
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
