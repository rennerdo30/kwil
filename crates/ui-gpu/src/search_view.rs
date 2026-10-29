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
    Row(usize),
    PagePrevious,
    PageNext,
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
    rows_revision: u64,
    painted_revision: u64,
    rows: Vec<FileRow>,
    painted: Vec<(usize, FileTicket)>,
    file_first: u64,
    files_seen: u64,
    pending: bool,
    file_intent: Option<FileTicket>,
    key: Option<kwil_view_model::search_session::SearchKey>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileTicket {
    key: kwil_view_model::search_session::SearchKey,
    path: std::path::PathBuf,
    range: std::ops::Range<u64>,
    line: u64,
}
impl FileTicket {
    fn from_action(action: kwil_view_model::search_session::SearchAction) -> Option<Self> {
        match action {
            kwil_view_model::search_session::SearchAction::OpenFile { key, path, range, line_number } => {
                Some(Self { key, path, range, line: line_number })
            }
            _ => None,
        }
    }
}
struct FileRow {
    ticket: Option<FileTicket>,
    label: String,
    preview: String,
}
impl View {
    pub(super) fn invalidate_painted_tickets(&mut self) {
        // A rejected edit may leave the accepted worker query unchanged.
        // Keep its bounded display page: there may be no further worker wake
        // to repopulate it. Only a fresh paint can authorize its rows again.
        self.rows_revision = self.rows_revision.checked_add(1).expect("folder paint generation exhausted");
        self.painted.clear();
        self.file_intent = None;
    }
    pub(super) fn sync_query(&mut self, session: &kwil_view_model::search_session::SearchSession, panel: &SearchPanel) {
        if self.key != session.key() {
            self.capture_results(session, panel);
        }
    }
    pub(super) fn can_page(&self, session: &kwil_view_model::search_session::SearchSession) -> bool {
        self.key == session.key() && !session.is_pending() && !self.rows.is_empty()
    }
    #[cfg(test)]
    pub(super) fn row_text(&self, row: usize) -> Option<(&str, &str)> {
        self.rows.get(row).map(|row| (row.label.as_str(), row.preview.as_str()))
    }
    #[cfg(all(test, windows))]
    pub(super) fn painted_row_rect(&self, row: usize) -> Option<Rect> {
        self.hits.iter().find_map(|(rect, target)| (*target == Target::Row(row)).then_some(*rect))
    }
    #[cfg(test)]
    pub(super) fn paint_test_rows(&mut self, panel: &SearchPanel, area: Rect) {
        self.hits = Hits::default();
        self.area = Some(area);
        self.epoch = panel.epoch();
        self.painted_revision = self.rows_revision;
        self.painted.clear();
        for (row, data) in self.rows.iter().enumerate() {
            if let Some(ticket) = &data.ticket {
                self.painted.push((row, ticket.clone()));
                self.hits.push(Rect { x: area.x, y: area.y + row, w: area.w, h: 1 }, Target::Row(row));
            }
        }
    }
    pub(super) fn admit_file_row(
        &mut self,
        panel: &SearchPanel,
        row: usize,
        action: kwil_view_model::search_session::SearchAction,
    ) -> bool {
        self.file_intent = None;
        if self.epoch != panel.epoch() || !self.painted_action_matches(row, action) {
            return false;
        }
        self.file_intent = self.painted.iter().find(|(index, _)| *index == row).map(|(_, ticket)| ticket.clone());
        self.file_intent.is_some()
    }
    pub(super) fn consume_file_intent(&mut self, action: &kwil_view_model::search_session::SearchAction) -> bool {
        let Some(ticket) = self.file_intent.take() else {
            return false;
        };
        match action {
            kwil_view_model::search_session::SearchAction::OpenFile { key, path, range, line_number } => {
                ticket.key == *key && ticket.path == *path && ticket.range == *range && ticket.line == *line_number
            }
            _ => false,
        }
    }
    /// Copy one bounded worker page, never the disk-backed result collection.
    pub(super) fn capture_results(
        &mut self,
        session: &kwil_view_model::search_session::SearchSession,
        panel: &SearchPanel,
    ) {
        use kwil_view_model::search_session::FileEvent;
        self.rows_revision = self.rows_revision.checked_add(1).expect("folder paint generation exhausted");
        self.key = session.key();
        let state = session.state();
        self.file_first = state.file_first;
        self.files_seen = state.files_seen;
        self.pending = session.is_pending();
        // Strip the model's typed request root before bounded display decoding.
        // Native FileTicket paths remain the authority for every result action.
        self.rows = state
            .files
            .iter()
            .take(kwil_view_model::search_session::PAGE_HITS)
            .enumerate()
            .map(|(index, row)| match row {
                FileEvent::Hit(hit) => FileRow {
                    ticket: session.file_action(index, 0).and_then(FileTicket::from_action),
                    label: format!(
                        "{}:{}",
                        desktop::visible(&panel.result_path(&hit.path).to_string_lossy(), 256),
                        hit.line_number
                    ),
                    preview: desktop::visible(&String::from_utf8_lossy(&hit.line), 256),
                },
                FileEvent::Error { path, message } => FileRow {
                    ticket: None,
                    label: desktop::visible(&panel.result_path(path).to_string_lossy(), 256),
                    preview: desktop::visible(message, 256),
                },
            })
            .collect();
    }
    pub(super) fn painted_action_matches(
        &self,
        row: usize,
        action: kwil_view_model::search_session::SearchAction,
    ) -> bool {
        self.rows_revision == self.painted_revision
            && FileTicket::from_action(action)
                .is_some_and(|ticket| self.painted.iter().any(|(index, painted)| *index == row && *painted == ticket))
    }
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
    if panel.mode() == SearchMode::Files {
        return files_layout(panel, size, m, body_top).map(|layout| layout.rect);
    }
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
    if panel.mode() == SearchMode::Files {
        return draw_files(panel, pixels, size, font, theme, m, body_top, view);
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
        option_chip(pixels, stride, font, chip, text, on, view.hover == Some(target), &c, &s);
        view.hits.push(chip, target);
        x += w + s.gap / 2;
    }
    let status = Rect { x: line.x, y: y + line.h + s.gap / 2, w: line.w, ..line };
    desktop::label_right(pixels, stride, font, status, panel.status(), c.muted);
    Some(r)
}

struct FilesLayout {
    rect: Rect,
    fields: Vec<SearchField>,
    rows: usize,
}
fn files_layout(panel: &SearchPanel, size: (usize, usize), m: &Metrics, body_top: usize) -> Option<FilesLayout> {
    let s = Sizes::new(m);
    let room = size.1.saturating_sub(body_top + m.status + 2 * s.gap);
    let w =
        size.0.saturating_sub(m.bar + 2 * s.gap).min(if size.0 >= m.dip(1000.0) { size.0 / 2 } else { m.dip(520.0) });
    // At small heights show Query and the active folder/glob field. The
    // header chips and Tab retain access to all four fields without hiding
    // the result list or placing a giant modal over the editor body.
    let compact = room < m.dip(460.0);
    let fields = if compact {
        vec![
            SearchField::Query,
            match panel.field() {
                SearchField::Include => SearchField::Include,
                SearchField::Exclude => SearchField::Exclude,
                _ => SearchField::Root,
            },
        ]
    } else {
        panel.visible_fields().to_vec()
    };
    let fixed = 2 * s.gap
        + s.control
        + s.gap
        + fields.len() * (s.control + s.gap)
        + s.text
        + s.gap
        + s.text
        + s.gap
        + s.control
        + s.gap;
    let row_h = 2 * s.text + s.gap / 2;
    let height_cap = room.saturating_sub((room / 8).min(m.dip(56.0))).min(m.dip(620.0));
    let rows = height_cap.saturating_sub(fixed) / row_h;
    if w < m.dip(300.0) || rows == 0 {
        return None;
    }
    let rows = rows.min(8);
    let h = fixed + rows * row_h;
    Some(FilesLayout {
        rect: Rect { x: size.0.saturating_sub(m.bar + s.gap + w), y: body_top + s.gap, w, h },
        fields,
        rows,
    })
}

#[allow(clippy::too_many_arguments)]
fn draw_files(
    panel: &SearchPanel,
    pixels: &mut [u32],
    size: (usize, usize),
    font: &mut font::UiFont,
    theme: &Theme,
    m: &Metrics,
    body_top: usize,
    view: &mut View,
) -> Option<Rect> {
    let layout = files_layout(panel, size, m, body_top)?;
    let r = layout.rect;
    let c = Colors::new(theme);
    let s = Sizes::new(m);
    let stride = size.0;
    view.area = Some(r);
    view.epoch = panel.epoch();
    view.painted_revision = view.rows_revision;
    view.painted.clear();
    desktop::card(pixels, stride, r, &c, &s);
    let inner = inset(r, s.gap);
    let mut y = inner.y;
    let close = Rect { x: inner.x + inner.w - s.control, y, w: s.control, h: s.control };
    desktop::icon_button(pixels, stride, close, Icon::Close, view.hover == Some(Target::Close), true, &c, &s);
    view.hits.push(close, Target::Close);
    let mut x = inner.x;
    for field in [SearchField::Root, SearchField::Include, SearchField::Exclude] {
        let target = Target::Field(field);
        let w = desktop::button_width(font, label(field), &s);
        let rect = Rect { x, y, w: w.min(close.x.saturating_sub(x + s.gap / 2)), h: s.control };
        desktop::button(pixels, stride, font, rect, label(field), Style::Secondary, view.hover == Some(target), &c, &s);
        view.hits.push(rect, target);
        x += w + s.gap / 2;
    }
    y += s.control + s.gap;
    for field in layout.fields {
        let rect = Rect { x: inner.x, y, w: inner.w, h: s.control };
        let (text, cursor) = panel.text(field);
        let clear = field == SearchField::Query && !text.is_empty();
        desktop::text_field(
            pixels,
            stride,
            font,
            rect,
            Field {
                text,
                cursor,
                placeholder: label(field),
                focused: panel.field() == field,
                hovered: view.hover == Some(Target::Field(field)),
                search: field == SearchField::Query,
                clear,
            },
            &c,
            &s,
        );
        view.hits.push(rect, Target::Field(field));
        if clear {
            view.hits.push(desktop::field_clear(rect, &s), Target::Clear);
        }
        y += s.control + s.gap;
    }
    let options = panel.options();
    x = inner.x;
    for (text, on, target) in [
        ("Aa", options.case_sensitive, Target::Case),
        ("Word", options.whole_word, Target::Word),
        (".*", options.regex, Target::Regex),
    ] {
        let w = desktop::button_width(font, text, &s);
        let rect = Rect { x, y, w, h: s.text };
        option_chip(pixels, stride, font, rect, text, on, view.hover == Some(target), &c, &s);
        view.hits.push(rect, target);
        x += w + s.gap / 2;
    }
    y += s.text + s.gap;
    let status = Rect { x: inner.x, y, w: inner.w, h: s.text };
    desktop::label_right(pixels, stride, font, status, panel.status(), c.muted);
    y += s.text + s.gap;
    let first = panel.selected().saturating_sub(layout.rows.saturating_sub(1));
    for index in first..(first + layout.rows).min(view.rows.len()) {
        let row = &view.rows[index];
        let rect = Rect { x: inner.x, y, w: inner.w, h: 2 * s.text + s.gap / 2 };
        let bg = if index == panel.selected() {
            c.selected
        } else if view.hover == Some(Target::Row(index)) {
            c.hover
        } else {
            c.surface
        };
        fill(pixels, stride, rect, bg);
        let clip = inset(rect, s.gap / 2);
        let text = font.fit(&row.label, clip.w);
        font.draw(
            pixels,
            stride,
            clip,
            clip.x,
            font.baseline(y, s.text),
            &text,
            if row.ticket.is_some() { c.fg } else { c.error },
        );
        let mono = font.monospace();
        let text = mono.fit(&row.preview, clip.w);
        mono.draw(pixels, stride, clip, clip.x, mono.baseline(y + s.text, s.text), &text, c.muted);
        if let Some(ticket) = &row.ticket {
            view.painted.push((index, ticket.clone()));
            view.hits.push(rect, Target::Row(index));
        }
        y += rect.h;
    }
    if view.rows.is_empty() {
        let rect = Rect { x: inner.x, y, w: inner.w, h: s.text };
        desktop::label_right(
            pixels,
            stride,
            font,
            rect,
            if panel.is_pending() { "Searching files…" } else { "Find in files · Enter a query" },
            c.faint,
        );
    }
    let y = r.y + r.h - s.gap - s.control;
    let previous = Rect { x: inner.x, y, w: s.control, h: s.control };
    let next = Rect { x: previous.x + previous.w + s.gap / 2, ..previous };
    let enabled = !view.pending && !panel.is_pending() && !view.rows.is_empty();
    desktop::icon_button(
        pixels,
        stride,
        previous,
        Icon::Up,
        view.hover == Some(Target::PagePrevious),
        enabled && view.file_first != 0,
        &c,
        &s,
    );
    desktop::icon_button(
        pixels,
        stride,
        next,
        Icon::Down,
        view.hover == Some(Target::PageNext),
        enabled && view.file_first.saturating_add(32) < view.files_seen,
        &c,
        &s,
    );
    if enabled && view.file_first != 0 {
        view.hits.push(previous, Target::PagePrevious);
    }
    if enabled && view.file_first.saturating_add(32) < view.files_seen {
        view.hits.push(next, Target::PageNext);
    }
    let help =
        Rect { x: next.x + next.w + s.gap, y, w: inner.w.saturating_sub(2 * s.control + 2 * s.gap), h: s.control };
    desktop::label_right(pixels, stride, font, help, "↑↓ Select · Enter Open · PgUp/PgDn · Esc Cancel", c.muted);
    Some(r)
}

#[allow(clippy::too_many_arguments)]
fn option_chip(
    pixels: &mut [u32],
    stride: usize,
    font: &mut font::UiFont,
    rect: Rect,
    text: &str,
    on: bool,
    hovered: bool,
    c: &Colors,
    s: &Sizes,
) {
    let bg = if on {
        chrome::mix(c.surface, c.accent, 0x50)
    } else if hovered {
        c.hover
    } else {
        c.control
    };
    chrome::round_rect(pixels, stride, rect, s.radius, if on { c.accent } else { c.border });
    chrome::round_rect(pixels, stride, inset(rect, s.hairline), s.radius, bg);
    font.draw(
        pixels,
        stride,
        rect,
        rect.x + s.inner,
        font.baseline(rect.y, rect.h),
        text,
        if on { c.fg } else { c.muted },
    );
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
        Some(Target::Row(row)) if view.painted_revision == view.rows_revision => {
            Click::Input(SearchInput::SelectRow(row))
        }
        Some(Target::PagePrevious) if view.painted_revision == view.rows_revision => {
            Click::Input(SearchInput::PagePrevious)
        }
        Some(Target::PageNext) if view.painted_revision == view.rows_revision => Click::Input(SearchInput::PageNext),
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
    #[cfg(windows)]
    fn compact_folder_options_paint_their_labels() {
        let m = Metrics::new(1.5);
        let s = Sizes::new(&m);
        let c = Colors::new(&theme_ui::chrome());
        let mut font = font::UiFont::load(m.text_px()).expect("Windows chrome face");
        for text in ["Aa", "Word", ".*"] {
            let rect = Rect { x: 0, y: 0, w: desktop::button_width(&mut font, text, &s), h: s.text };
            let mut labelled = vec![0; rect.w * rect.h];
            let mut empty = labelled.clone();
            option_chip(&mut labelled, rect.w, &mut font, rect, text, false, false, &c, &s);
            option_chip(&mut empty, rect.w, &mut font, rect, "", false, false, &c, &s);
            assert!(labelled != empty, "{text} must produce actual label pixels at compact height");
        }
    }

    #[test]
    fn folder_geometry_preserves_results_at_minimum_size_and_fractional_dpi() {
        let mut panel = SearchPanel::default();
        panel.open(SearchMode::Files, &Default::default()).unwrap();
        for scale in [1.0, 1.25, 2.0] {
            let m = Metrics::new(scale);
            for (w, h) in [(640.0, 480.0), (800.0, 600.0), (1440.0, 900.0)] {
                let size = (m.dip(w), m.dip(h));
                let layout =
                    files_layout(&panel, size, &m, m.menu + m.tabs).expect("minimum geometry retains result rows");
                assert!(layout.rows >= 1);
                assert!(layout.rect.x + layout.rect.w <= size.0 - m.bar);
                assert!(layout.rect.y + layout.rect.h <= size.1 - m.status);
                assert!(
                    layout.rect.w * layout.rect.h < size.0 * size.1 * 3 / 5,
                    "adaptive card leaves the document visible"
                );
                for field in [SearchField::Root, SearchField::Include, SearchField::Exclude] {
                    panel.deferred_input(SearchInput::Focus(field)).unwrap();
                    let layout = files_layout(&panel, size, &m, m.menu + m.tabs).unwrap();
                    assert!(layout.fields.contains(&field));
                }
            }
        }
        assert!(area(&panel, (200, 150), &Metrics::default(), 65).is_none());
    }

    #[test]
    fn a_new_worker_page_invalidates_same_length_same_epoch_painted_tickets() {
        use kwil_view_model::search_session::{SearchAction, SearchKey};
        let key = SearchKey { document_id: 0, revision: 0, generation: 1 };
        let ticket = FileTicket { key, path: std::path::PathBuf::from("native-path"), range: 10..16, line: 2 };
        let mut panel = SearchPanel::default();
        panel.open(SearchMode::Files, &Default::default()).unwrap();
        let area = Rect { x: 10, y: 10, w: 100, h: 100 };
        let mut view = View { rows_revision: 1, ..Default::default() };
        view.rows.push(FileRow { ticket: Some(ticket.clone()), label: "display".into(), preview: "preview".into() });
        view.paint_test_rows(&panel, area);
        let action =
            || SearchAction::OpenFile { key, path: ticket.path.clone(), range: ticket.range.clone(), line_number: 2 };
        assert!(view.painted_action_matches(0, action()));
        assert!(matches!(click(&view, &panel, Some(area), (10, 10)), Click::Input(SearchInput::SelectRow(0))));
        view.rows_revision += 1;
        assert!(!view.painted_action_matches(0, action()));
        assert!(matches!(click(&view, &panel, Some(area), (10, 10)), Click::Inside));
        assert!(!view.admit_file_row(&panel, 0, action()));
        view.paint_test_rows(&panel, area);
        assert!(view.admit_file_row(&panel, 0, action()));
        let different =
            SearchAction::OpenFile { key, path: "another-native-path".into(), range: 10..16, line_number: 2 };
        assert!(!view.consume_file_intent(&different), "display labels never authorize a different native path");
        assert!(!view.consume_file_intent(&action()), "an admission is consumed only once");
    }

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
        let area = draw(&panel, &mut pixels, size, &mut font, &theme_ui::chrome(), &m, 60, &mut view).expect("fits");
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
        draw(&panel, &mut pixels, size, &mut font, &theme_ui::chrome(), &m, 60, &mut view).unwrap();
        assert!(!view.hits.iter().any(|(_, t)| *t == Target::Clear));
    }
}
