//! Find / replace card, drawn with the desktop widgets: anchored at the top
//! right of the editor body like a find widget, with real fields, option
//! toggles, a match count, previous/next and close buttons. It is a visual
//! adapter over the Find package's `SearchPanel`: every click becomes one of
//! that panel's own `SearchInput`s. The host admits owned replacement recipes
//! to the document worker and acknowledges completion after editor return.
use super::*;
use chrome::Metrics;
use desktop::{Colors, Field, Hits, Icon, Sizes, Style, inset};
use kwil_core_commands::theme::Theme;
use kwil_view_model::search_panel::{SearchField, SearchInput, SearchMode, SearchPanel};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Target {
    Field(SearchField),
    /// The clear button inside the Find field.
    Clear,
    Regex,
    Case,
    Word,
    Selection,
    Previous,
    Next,
    Close,
    ReplaceNext,
    ReplaceAll,
}

/// What a click asks the host to do.
pub(super) enum Click {
    Input(SearchInput<'static>),
    /// Empty the Find query: the host sends `CLEAR_QUERY` in order, through
    /// its own `search_input`, so results update and the card stays open.
    Clear,
    Outside,
    Inside,
}

/// The panel's own inputs that empty the Find query (focus it, select all,
/// delete). Nothing is accepted, replaced or closed.
pub(super) const CLEAR_QUERY: [SearchInput<'static>; 3] =
    [SearchInput::Focus(SearchField::Query), SearchInput::SelectAll, SearchInput::Backspace];

#[derive(Default)]
pub(super) struct View {
    pub area: Option<Rect>,
    hits: Hits<Target>,
    pub hover: Option<Target>,
    /// Panel epoch the geometry was painted for.
    epoch: u64,
}

fn label(field: SearchField) -> &'static str {
    match field {
        SearchField::Query => "Find",
        SearchField::Replacement => "Replace",
        SearchField::Root => "Folder",
        SearchField::Include => "Include",
        SearchField::Exclude => "Exclude",
    }
}

/// Card rectangle: top right of the editor body, clear of the scrollbar.
pub(super) fn area(panel: &SearchPanel, size: (usize, usize), m: &Metrics, body_top: usize) -> Option<Rect> {
    let s = Sizes::new(m);
    let fields = panel.visible_fields().len().max(1);
    let w = size.0.saturating_sub(m.bar + 2 * s.gap).min(m.dip(560.0));
    let option_rows = if w < m.dip(420.0) { 2 } else { 1 };
    let h = s.gap + fields * (s.control + s.gap) + (option_rows + 1) * (s.text + s.gap / 2) + s.gap;
    let y = body_top + s.gap;
    if w < m.dip(240.0) || y + h > size.1 {
        return None;
    }
    Some(Rect { x: size.0.saturating_sub(m.bar + s.gap + w), y, w, h })
}

#[allow(clippy::too_many_arguments)]
pub(super) fn draw(
    panel: &SearchPanel,
    pixels: &mut [u32],
    size: (usize, usize),
    font: &mut font::UiFont,
    theme: &Theme,
    m: &Metrics,
    body_top: usize,
    view: &mut View,
) -> Option<Rect> {
    view.hits = Hits::default();
    view.area = None;
    if !panel.is_open() {
        return None;
    }
    let r = area(panel, size, m, body_top)?;
    let c = Colors::new(theme);
    let s = Sizes::new(m);
    let stride = size.0;
    view.area = Some(r);
    view.epoch = panel.epoch();
    desktop::card(pixels, stride, r, &c, &s);
    let inner = inset(r, s.gap);
    let options = panel.options();
    let button = s.control;
    let mut y = inner.y;
    for (index, &field) in panel.visible_fields().iter().enumerate() {
        let row = Rect { x: inner.x, y, w: inner.w, h: s.control };
        // Trailing controls: options and navigation on the query row,
        // replace actions on the replacement row.
        let mut x = row.x + row.w;
        let push = |x: &mut usize, w: usize, target: Target| {
            *x = x.saturating_sub(w);
            let rect = Rect { x: *x, w, ..row };
            *x = x.saturating_sub(s.gap / 2);
            (rect, target)
        };
        let mut controls: Vec<(Rect, Target)> = Vec::new();
        if index == 0 {
            controls.push(push(&mut x, button, Target::Close));
            controls.push(push(&mut x, button, Target::Next));
            controls.push(push(&mut x, button, Target::Previous));
        } else if field == SearchField::Replacement {
            let all = desktop::button_width(font, "Replace all", &s);
            let one = desktop::button_width(font, "Replace", &s);
            controls.push(push(&mut x, all, Target::ReplaceAll));
            controls.push(push(&mut x, one, Target::ReplaceNext));
        }
        let (text, cursor) = panel.text(field);
        let field_w = x.saturating_sub(row.x + s.gap / 2);
        let field_rect = Rect { w: field_w, ..row };
        let focused = panel.field() == field;
        // Only the Find query offers a clear button, and only with text.
        let clear = field == SearchField::Query && !text.is_empty();
        desktop::text_field(
            pixels,
            stride,
            font,
            field_rect,
            Field {
                text,
                cursor,
                placeholder: label(field),
                focused,
                hovered: view.hover == Some(Target::Field(field)),
                search: index == 0,
                clear,
            },
            &c,
            &s,
        );
        view.hits.push(field_rect, Target::Field(field));
        if clear {
            view.hits.push(desktop::field_clear(field_rect, &s), Target::Clear);
        }
        for (rect, target) in controls {
            let hovered = view.hover == Some(target);
            match target {
                Target::Close => desktop::icon_button(pixels, stride, rect, Icon::Close, hovered, true, &c, &s),
                Target::Next => desktop::icon_button(pixels, stride, rect, Icon::Down, hovered, true, &c, &s),
                Target::Previous => desktop::icon_button(pixels, stride, rect, Icon::Up, hovered, true, &c, &s),
                Target::ReplaceNext => {
                    replacement_button(pixels, stride, font, rect, "Replace", hovered, panel.can_replace(), &c, &s)
                }
                _ => {
                    replacement_button(pixels, stride, font, rect, "Replace all", hovered, panel.can_replace(), &c, &s)
                }
            }
            if !matches!(target, Target::ReplaceNext | Target::ReplaceAll) || panel.can_replace() {
                view.hits.push(rect, target);
            }
        }
        y += s.control + s.gap;
    }
    // Option toggles and the match count share the last line.
    let line = Rect { x: inner.x, y, w: inner.w, h: s.text + s.gap / 2 };
    let mut x = line.x;
    for (index, (text, on, target)) in [
        ("Aa", options.case_sensitive, Target::Case),
        ("Word", options.whole_word, Target::Word),
        (".*", options.regex, Target::Regex),
        ("Selection", options.in_selection, Target::Selection),
    ]
    .into_iter()
    .enumerate()
    {
        if inner.w < m.dip(420.0) && index == 2 {
            x = line.x;
            y += line.h + s.gap / 2;
        }
        let w = font.measure(text, 200) + s.inner * 2;
        let chip = Rect { x, y, w: w.min((line.x + line.w).saturating_sub(x)), ..line };
        let hovered = view.hover == Some(target);
        let bg = if on {
            chrome::mix(c.surface, c.accent, 0x50)
        } else if hovered {
            c.hover
        } else {
            c.control
        };
        chrome::round_rect(pixels, stride, chip, s.radius, if on { c.accent } else { c.border });
        chrome::round_rect(pixels, stride, inset(chip, s.hairline), s.radius, bg);
        let baseline = font.baseline(chip.y, chip.h);
        font.draw(pixels, stride, chip, chip.x + s.inner, baseline, text, if on { c.fg } else { c.muted });
        view.hits.push(chip, target);
        x += w + s.gap / 2;
    }
    let status = Rect { x: line.x, y: y + line.h + s.gap / 2, w: line.w, ..line };
    desktop::label_right(pixels, stride, font, status, panel.status(), c.muted);
    Some(r)
}

#[allow(clippy::too_many_arguments)]
fn replacement_button(
    pixels: &mut [u32],
    stride: usize,
    font: &mut font::UiFont,
    rect: Rect,
    text: &str,
    hovered: bool,
    enabled: bool,
    colors: &Colors,
    sizes: &Sizes,
) {
    if enabled {
        desktop::button(pixels, stride, font, rect, text, Style::Secondary, hovered, colors, sizes);
    } else {
        chrome::round_rect(pixels, stride, rect, sizes.radius, colors.border);
        chrome::round_rect(pixels, stride, inset(rect, sizes.hairline), sizes.radius, colors.control);
        let clip = inset(rect, sizes.inner.min(rect.w / 4));
        let fitted = font.fit(text, clip.w);
        let width = font.measure(&fitted, clip.w);
        let baseline = font.baseline(rect.y, rect.h);
        font.draw(pixels, stride, clip, clip.x + (clip.w - width) / 2, baseline, &fitted, colors.faint);
    }
}

/// Click against the painted geometry. A repaint for another panel epoch or
/// a different card rectangle makes older geometry inert.
pub(super) fn click(view: &View, panel: &SearchPanel, current: Option<Rect>, pointer: (usize, usize)) -> Click {
    let Some(area) = view.area else { return Click::Inside };
    if current != Some(area) || view.epoch != panel.epoch() {
        return Click::Inside;
    }
    if !area.contains(pointer) {
        return Click::Outside;
    }
    match view.hits.at(pointer) {
        Some(Target::Field(field)) => Click::Input(SearchInput::Focus(field)),
        Some(Target::Clear) => Click::Clear,
        Some(Target::Regex) => Click::Input(SearchInput::ToggleRegex),
        Some(Target::Case) => Click::Input(SearchInput::ToggleCase),
        Some(Target::Word) => Click::Input(SearchInput::ToggleWord),
        Some(Target::Selection) => Click::Input(SearchInput::ToggleSelection),
        Some(Target::Previous) => Click::Input(SearchInput::Previous),
        Some(Target::Next) => Click::Input(SearchInput::Next),
        Some(Target::Close) => Click::Input(SearchInput::Cancel),
        Some(Target::ReplaceNext) if panel.mode() == SearchMode::Replace && panel.can_replace() => {
            Click::Input(SearchInput::ReplaceNext)
        }
        Some(Target::ReplaceAll) if panel.mode() == SearchMode::Replace && panel.can_replace() => {
            Click::Input(SearchInput::ReplaceAll)
        }
        _ => Click::Inside,
    }
}

pub(super) fn hover(view: &View, pointer: (usize, usize)) -> Option<Target> {
    view.area.filter(|area| area.contains(pointer)).and_then(|_| view.hits.at(pointer))
}

#[cfg(test)]
mod replacement_dispatch_tests {
    use super::*;
    use kwil_view_model::search_session::SearchSession;
    use std::{
        sync::mpsc,
        time::{Duration, Instant},
    };

    #[test]
    fn replacement_buttons_follow_query_readiness_and_emit_typed_inputs() {
        let mut workspace = Workspace::new(kwil_view_model::View::new(None)).unwrap();
        workspace
            .execute("edit.insert", &CommandArgs { text: Some("one one".into()), ..Default::default() }, 1)
            .unwrap();
        let (tx, rx) = mpsc::channel();
        let mut session = SearchSession::new(move || {
            let _ = tx.send(());
        })
        .unwrap();
        let mut panel = SearchPanel::default();
        panel.command(&mut session, &workspace, "search.replace", &Default::default()).unwrap();
        let area = Rect { x: 0, y: 0, w: 100, h: 40 };
        let one = Rect { x: 0, y: 0, w: 50, h: 40 };
        let all = Rect { x: 50, ..one };
        let mut view = View { area: Some(area), epoch: panel.epoch(), ..Default::default() };
        view.hits.push(one, Target::ReplaceNext);
        view.hits.push(all, Target::ReplaceAll);
        assert!(matches!(click(&view, &panel, Some(area), (10, 10)), Click::Inside));
        panel.input(&mut session, &workspace, SearchInput::Text("one")).unwrap();
        view.epoch = panel.epoch();
        assert!(matches!(click(&view, &panel, Some(area), (60, 10)), Click::Inside), "pending matches are disabled");
        let deadline = Instant::now() + Duration::from_secs(5);
        while panel.is_pending() {
            rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).expect("bounded search readiness");
            panel.worker_ready(&mut session, &workspace);
        }
        assert!(matches!(click(&view, &panel, Some(area), (10, 10)), Click::Input(SearchInput::ReplaceNext)));
        assert!(matches!(click(&view, &panel, Some(area), (60, 10)), Click::Input(SearchInput::ReplaceAll)));
        panel.input(&mut session, &workspace, SearchInput::Focus(SearchField::Replacement)).unwrap();
        assert!(
            matches!(click(&view, &panel, Some(area), (10, 10)), Click::Inside),
            "old input epoch geometry is inert"
        );
        panel.take_shutdown(&mut session).unwrap_or_else(|_| panic!("shutdown ownership")).shutdown().unwrap();
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use kwil_core_commands::settings::SearchSettings;

    #[test]
    #[cfg(windows)]
    fn find_clear_button_empties_the_query_through_the_panel_and_keeps_it_open() {
        // R5: the X used to sit on the field's own target, so it only focused the field.
        let m = Metrics::new(1.0);
        let size = (1000, 600);
        let mut font = font::UiFont::load(m.text_px()).expect("Windows chrome face");
        let mut panel = SearchPanel::default();
        panel.open(SearchMode::Find, &SearchSettings::default()).unwrap();
        panel.deferred_input(SearchInput::Text("needle")).unwrap();
        let mut view = View::default();
        let mut pixels = vec![0u32; size.0 * size.1];
        let area = draw(&panel, &mut pixels, size, &mut font, theme_ui::chrome(), &m, 60, &mut view).expect("fits");
        let x = view.hits.iter().find_map(|(r, t)| (*t == Target::Clear).then_some(*r)).expect("X is clickable");
        let field = view.hits.iter().find_map(|(r, t)| (*t == Target::Field(SearchField::Query)).then_some(*r));
        let field = field.expect("query field");
        let center = (x.x + x.w / 2, x.y + x.h / 2);
        assert!(matches!(click(&view, &panel, Some(area), center), Click::Clear));
        assert!(matches!(
            click(&view, &panel, Some(area), (field.x + 4, field.y + field.h / 2)),
            Click::Input(SearchInput::Focus(SearchField::Query))
        ));
        for input in CLEAR_QUERY {
            panel.deferred_input(input).unwrap();
        }
        assert_eq!(panel.text(SearchField::Query).0, "", "the query is empty");
        assert!(panel.is_open(), "clearing keeps the card open");
        // An empty query offers no X.
        draw(&panel, &mut pixels, size, &mut font, theme_ui::chrome(), &m, 60, &mut view).unwrap();
        assert!(!view.hits.iter().any(|(_, t)| *t == Target::Clear));
    }
}
