//! Window chrome: DPI-scaled metrics, layout, the status model and small
//! anti-aliased primitives. Every size here comes from the monitor scale only;
//! editor zoom changes the text grid inside the body and nothing in the chrome.
use super::*;
use kwil_core_commands::settings::TabLayout;
use kwil_view_model::{SessionDocumentId, TabAction, TabArea, TabGeometry, TabSlot};

/// Logical size of menu, tab and status text.
pub(super) const UI_TEXT_DIP: f64 = 15.0;

/// Physical pixel sizes for one monitor scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Metrics {
    pub scale: f64,
    pub menu: usize,
    pub tabs: usize,
    pub status: usize,
    /// Scrollbar track thickness; the whole track is the hit area.
    pub bar: usize,
    pub min_thumb: usize,
    /// Gap between track edge and thumb.
    pub inset: usize,
    pub pad: usize,
    pub close: usize,
    pub glyph: usize,
    pub indicator: usize,
    pub tab_min: usize,
    pub tab_max: usize,
    pub nav: usize,
    pub side: usize,
    pub row: usize,
    pub item: usize,
    pub separator: usize,
    pub menu_min: usize,
    pub gap: usize,
    pub radius: usize,
    pub line: usize,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new(1.0)
    }
}

impl Metrics {
    pub fn new(scale: f64) -> Self {
        let scale = if scale.is_finite() && scale > 0.0 { scale.clamp(0.5, 8.0) } else { 1.0 };
        let d = |dip: f64| ((dip * scale).round() as usize).max(1);
        Self {
            scale,
            menu: d(30.0),
            tabs: d(35.0),
            status: d(26.0),
            bar: d(16.0),
            min_thumb: d(32.0),
            inset: d(3.0),
            pad: d(12.0),
            close: d(20.0),
            glyph: d(8.0),
            indicator: d(2.0),
            tab_min: d(96.0),
            tab_max: d(240.0),
            nav: d(28.0),
            side: d(220.0),
            row: d(32.0),
            item: d(28.0),
            separator: d(9.0),
            menu_min: d(240.0),
            gap: d(32.0),
            radius: d(4.0),
            line: d(1.0),
        }
    }
    pub fn text_px(&self) -> f32 {
        (UI_TEXT_DIP * self.scale) as f32
    }
    pub fn dip(&self, dip: f64) -> usize {
        ((dip * self.scale).round() as usize).max(1)
    }
}

/// Window regions in physical pixels. The body holds gutter, text and bars.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Layout {
    pub menu: TabArea,
    pub strip: TabArea,
    pub body: TabArea,
    pub status: TabArea,
}

pub(super) fn layout(size: (usize, usize), m: &Metrics, tabs: TabLayout) -> Layout {
    let (w, h) = size;
    let menu_h = m.menu.min(h);
    let status_h = m.status.min(h - menu_h);
    let content = h - menu_h - status_h;
    let menu = TabArea { x: 0, y: 0, width: w, height: menu_h };
    let status = TabArea { x: 0, y: h - status_h, width: w, height: status_h };
    let (strip, body) = match tabs {
        TabLayout::Horizontal => {
            let strip_h = m.tabs.min(content);
            (
                TabArea { x: 0, y: menu_h, width: w, height: strip_h },
                TabArea { x: 0, y: menu_h + strip_h, width: w, height: content - strip_h },
            )
        }
        TabLayout::Vertical => {
            let side = m.side.min(w / 3);
            (
                TabArea { x: 0, y: menu_h, width: side, height: content },
                TabArea { x: side, y: menu_h, width: w - side, height: content },
            )
        }
    };
    Layout { menu, strip, body, status }
}

/// Inset of the close button from the right slot edge, keeping it centred
/// vertically in a horizontal tab.
fn close_inset(m: &Metrics, height: usize) -> usize {
    height.saturating_sub(m.close) / 2
}

/// Natural width of a horizontal tab whose label is `label` pixels wide.
pub(super) fn tab_width(m: &Metrics, label: usize) -> usize {
    (m.pad + label + m.pad / 2 + m.close + close_inset(m, m.tabs)).clamp(m.tab_min, m.tab_max)
}

/// Builds the shared tab model from chrome metrics. `labels` holds the UI
/// text width of each session tab; `ch` is the editor row height, used only
/// for the text grid and scroll copies.
pub(super) fn tab_geometry(
    lay: &Layout,
    m: &Metrics,
    kind: TabLayout,
    ids: &[SessionDocumentId],
    active: Option<usize>,
    labels: &[usize],
    ch: usize,
) -> TabGeometry {
    let ch = ch.max(1);
    let strip = lay.strip;
    let total = ids.len();
    let mut geometry = TabGeometry {
        body: lay.body,
        chrome: strip,
        text: TabArea {
            x: lay.body.x,
            y: lay.body.y,
            width: lay.body.width.saturating_sub(m.bar),
            height: lay.body.height.saturating_sub(m.bar) / ch * ch,
        },
        previous: TabArea::default(),
        next: TabArea::default(),
        slots: Vec::new(),
        first: 0,
        capacity: 0,
        total,
        cell_height: ch,
    };
    let active = active.unwrap_or(0).min(total.saturating_sub(1));
    if total == 0 || strip.width == 0 || strip.height == 0 {
        return geometry;
    }
    match kind {
        TabLayout::Horizontal => {
            let natural: Vec<usize> = (0..total).map(|i| tab_width(m, labels.get(i).copied().unwrap_or(0))).collect();
            let overflow = natural.iter().sum::<usize>() > strip.width;
            let available = if overflow { strip.width.saturating_sub(2 * m.nav) } else { strip.width };
            if available == 0 {
                return geometry;
            }
            // Greedy pages; the active tab's page is shown in full.
            let (mut start, mut used, mut page) = (0, 0, None);
            for (index, width) in natural.iter().enumerate() {
                let width = (*width).min(available);
                if index > start && used + width > available {
                    if (start..index).contains(&active) {
                        page = Some(start..index);
                        break;
                    }
                    start = index;
                    used = 0;
                }
                used += width;
            }
            let page = page.unwrap_or(start..total);
            let mut x = strip.x;
            for index in page.clone().take(kwil_core_workspace::session::MAX_TABS) {
                let width = natural[index].min(available);
                let area = TabArea { x, y: strip.y, width, height: strip.height };
                let inset = close_inset(m, strip.height);
                let size = m.close.min(width).min(strip.height);
                let close = TabArea {
                    x: area.x + width.saturating_sub(inset + size),
                    y: area.y + (strip.height - size) / 2,
                    width: size,
                    height: size,
                };
                geometry.slots.push(TabSlot { document: ids[index], area, close, active: index == active });
                x += width;
            }
            geometry.first = page.start;
            geometry.capacity = geometry.slots.len();
            if overflow {
                let right = strip.x + strip.width;
                let nav = m.nav.min(strip.width / 2);
                geometry.previous = TabArea { x: right - 2 * nav, y: strip.y, width: nav, height: strip.height };
                geometry.next = TabArea { x: right - nav, y: strip.y, width: nav, height: strip.height };
            }
        }
        TabLayout::Vertical => {
            let row = m.row.max(1);
            let overflow = total.saturating_mul(row) > strip.height;
            let nav = if overflow { row.min(strip.height) } else { 0 };
            let capacity = ((strip.height - nav) / row).min(kwil_core_workspace::session::MAX_TABS);
            if capacity == 0 {
                return geometry;
            }
            geometry.first = active / capacity * capacity;
            for (local, (index, id)) in ids.iter().enumerate().skip(geometry.first).take(capacity).enumerate() {
                let area = TabArea { x: strip.x, y: strip.y + nav + local * row, width: strip.width, height: row };
                let size = m.close.min(row).min(strip.width);
                let close = TabArea {
                    x: area.x + strip.width.saturating_sub(m.pad / 2 + size),
                    y: area.y + (row - size) / 2,
                    width: size,
                    height: size,
                };
                geometry.slots.push(TabSlot { document: *id, area, close, active: index == active });
            }
            geometry.capacity = geometry.slots.len();
            if overflow {
                let half = strip.width / 2;
                geometry.previous = TabArea { x: strip.x, y: strip.y, width: half, height: nav };
                geometry.next = TabArea { x: strip.x + half, y: strip.y, width: strip.width - half, height: nav };
            }
        }
    }
    geometry
}

/// Pages can hold different tab counts, so page arrows activate the tab that
/// borders the current page instead of jumping by this page's capacity.
pub(super) fn resolve_page(geometry: &TabGeometry, action: TabAction, ids: &[SessionDocumentId]) -> Option<TabAction> {
    match action {
        TabAction::PreviousPage => {
            geometry.first.checked_sub(1).and_then(|i| ids.get(i)).map(|id| TabAction::Activate(*id))
        }
        TabAction::NextPage => ids.get(geometry.first + geometry.capacity).map(|id| TabAction::Activate(*id)),
        other => Some(other),
    }
}

/// What the pointer is over; hover changes damage only chrome and status.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Hover {
    #[default]
    None,
    Title(usize),
    Tab(usize),
    Close(usize),
    Previous,
    Next,
    Status(usize),
    VerticalThumb,
    HorizontalThumb,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Segment {
    pub text: String,
    pub command: Option<&'static str>,
    pub warn: bool,
}

/// Left notice plus right-aligned fields, left to right. Every field comes
/// from a typed accessor; file paths never pass through this text.
pub(super) fn status(view: &Workspace, zoom: u32) -> (String, Vec<Segment>) {
    let segment = |text: String, command, warn| Segment { text, command, warn };
    let mut segments = Vec::new();
    if let Some(recovery) = view.recovery_notice() {
        segments.push(segment(recovery.to_owned(), None, true));
    }
    if view
        .editor()
        .is_some_and(|editor| editor.document().compression == kwil_core_workspace::compressed::CompressionFormat::Gzip)
    {
        segments.push(segment("Gzip · decoded text".into(), None, false));
    }
    if zoom != 100 {
        segments.push(segment(format!("Zoom {zoom}%"), Some("view.zoomReset"), false));
    }
    if let Some(percent) = view.indexing_percent() {
        segments.push(segment(format!("Indexing {percent}%"), None, false));
    } else if let Some(lines) = view.line_count() {
        let unit = if lines == 1 { "line" } else { "lines" };
        segments.push(segment(format!("{lines} {unit}"), None, false));
    }
    if let Some((row, column)) = view.status_position() {
        let text = format!("Ln {}, Col {}", row.saturating_add(1), column.saturating_add(1));
        segments.push(segment(text, Some("navigation.goToLine"), false));
    }
    (view.notice().to_owned(), segments)
}

/// Window title: name, folder and dirty marker; the full path lives here
/// instead of the status bar.
pub(super) fn window_title(view: &Workspace) -> String {
    let active = view.active_id();
    let Some(tab) = view.session().tabs().iter().find(|tab| tab.id == active) else { return "Kwil".into() };
    let title = view.tab_title(active);
    let (dirty, name) = match title.strip_prefix("* ") {
        Some(name) if view.editor().is_some_and(|e| e.document().is_dirty()) || tab.revision != tab.saved_revision => {
            (true, name.to_owned())
        }
        _ => (false, title),
    };
    let folder =
        tab.path.as_ref().or(tab.recovery_path.as_ref()).and_then(|p| p.parent()).map(|p| p.display().to_string());
    let mark = if dirty { "\u{25CF} " } else { "" };
    match folder.filter(|f| !f.is_empty()) {
        Some(folder) => format!("{mark}{name} - {folder} - Kwil"),
        None => format!("{mark}{name} - Kwil"),
    }
}

pub(super) fn mix(bg: u32, fg: u32, amount: u8) -> u32 {
    font::blend(bg, fg, amount)
}

fn rows(buf: &[u32], stride: usize) -> usize {
    buf.len() / stride.max(1)
}

/// Solid rectangle with anti-aliased rounded corners.
pub(super) fn round_rect(buf: &mut [u32], stride: usize, r: Rect, radius: usize, color: u32) {
    let radius = radius.min(r.w / 2).min(r.h / 2);
    if radius == 0 {
        fill_clipped(buf, stride, r, color);
        return;
    }
    let rad = radius as f32;
    let (x0, y0, x1, y1) = (r.x as f32 + rad, r.y as f32 + rad, (r.x + r.w) as f32 - rad, (r.y + r.h) as f32 - rad);
    for y in r.y..(r.y + r.h).min(rows(buf, stride)) {
        let corner_row = y < r.y + radius || y >= r.y + r.h - radius;
        let row = &mut buf[y * stride..(y + 1) * stride];
        let end = (r.x + r.w).min(stride);
        for (x, pixel) in row.iter_mut().enumerate().take(end).skip(r.x) {
            if !corner_row || (x >= r.x + radius && x < r.x + r.w - radius) {
                *pixel = color;
                continue;
            }
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            let (dx, dy) = (px.clamp(x0, x1) - px, py.clamp(y0, y1) - py);
            let coverage = (rad + 0.5 - (dx * dx + dy * dy).sqrt()).clamp(0.0, 1.0);
            if coverage > 0.0 {
                *pixel = mix(*pixel, color, (coverage * 255.0) as u8);
            }
        }
    }
}

pub(super) fn fill_clipped(buf: &mut [u32], stride: usize, r: Rect, color: u32) {
    for y in r.y..(r.y + r.h).min(rows(buf, stride)) {
        let row = &mut buf[y * stride..(y + 1) * stride];
        row[r.x.min(stride)..(r.x + r.w).min(stride)].fill(color);
    }
}

/// Anti-aliased line segment of `width` pixels.
pub(super) fn line(buf: &mut [u32], stride: usize, a: (f32, f32), b: (f32, f32), width: f32, color: u32) {
    let half = width / 2.0 + 1.0;
    let (left, right) = ((a.0.min(b.0) - half).max(0.0) as usize, (a.0.max(b.0) + half).ceil() as usize);
    let (top, bottom) = ((a.1.min(b.1) - half).max(0.0) as usize, (a.1.max(b.1) + half).ceil() as usize);
    let (vx, vy) = (b.0 - a.0, b.1 - a.1);
    let length = (vx * vx + vy * vy).max(f32::EPSILON);
    for y in top..bottom.min(rows(buf, stride)) {
        for x in left..right.min(stride) {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            let t = (((px - a.0) * vx + (py - a.1) * vy) / length).clamp(0.0, 1.0);
            let (dx, dy) = (px - (a.0 + t * vx), py - (a.1 + t * vy));
            let coverage = (width / 2.0 + 0.5 - (dx * dx + dy * dy).sqrt()).clamp(0.0, 1.0);
            if coverage > 0.0 {
                let dst = &mut buf[y * stride + x];
                *dst = mix(*dst, color, (coverage * 255.0) as u8);
            }
        }
    }
}

/// Anti-aliased filled circle.
pub(super) fn disc(buf: &mut [u32], stride: usize, center: (f32, f32), radius: f32, color: u32) {
    let (left, top) = ((center.0 - radius - 1.0).max(0.0) as usize, (center.1 - radius - 1.0).max(0.0) as usize);
    let (right, bottom) = ((center.0 + radius + 1.0).ceil() as usize, (center.1 + radius + 1.0).ceil() as usize);
    for y in top..bottom.min(rows(buf, stride)) {
        for x in left..right.min(stride) {
            let (dx, dy) = (x as f32 + 0.5 - center.0, y as f32 + 0.5 - center.1);
            let coverage = (radius + 0.5 - (dx * dx + dy * dy).sqrt()).clamp(0.0, 1.0);
            if coverage > 0.0 {
                let dst = &mut buf[y * stride + x];
                *dst = mix(*dst, color, (coverage * 255.0) as u8);
            }
        }
    }
}

/// Draws the status bar and returns clickable field rectangles.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_status(
    pixels: &mut [u32],
    stride: usize,
    area: TabArea,
    font: Option<&mut font::UiFont>,
    theme: &kwil_core_commands::theme::Theme,
    m: &Metrics,
    notice: &str,
    segments: &[Segment],
    hover: Hover,
) -> Vec<(Rect, usize, &'static str)> {
    use kwil_core_commands::theme::UiRole;
    let bg = theme_ui::rgb(theme, UiRole::StatusBackground);
    let fg = theme_ui::rgb(theme, UiRole::StatusForeground);
    let rect = tabs::rect(area);
    fill_clipped(pixels, stride, rect, bg);
    let mut hits = Vec::new();
    let Some(font) = font else { return hits };
    if area.height == 0 {
        return hits;
    }
    let baseline = font.baseline(area.y, area.height);
    let field_pad = m.dip(8.0);
    let mut right = area.x + area.width.saturating_sub(m.pad / 2);
    for (index, segment) in segments.iter().enumerate().rev() {
        let width = font.width(&segment.text) + 2 * field_pad;
        if width + m.pad > right.saturating_sub(area.x) {
            break;
        }
        let field = Rect { x: right - width, y: area.y, w: width, h: area.height };
        if let Some(command) = segment.command {
            if hover == Hover::Status(index) {
                let inset = m.dip(3.0).min(area.height / 4);
                round_rect(
                    pixels,
                    stride,
                    Rect { x: field.x, y: field.y + inset, w: field.w, h: field.h - 2 * inset },
                    m.radius,
                    mix(bg, fg, 0x2c),
                );
            }
            hits.push((field, index, command));
        }
        let color = if segment.warn { theme_ui::rgb(theme, UiRole::Error) } else { fg };
        font.draw(pixels, stride, rect, field.x + field_pad, baseline, &segment.text, color);
        right = field.x;
    }
    if !notice.is_empty() {
        let left = area.x + m.pad;
        let room = right.saturating_sub(left + m.pad);
        let text = font.fit(notice, room);
        font.draw(pixels, stride, rect, left, baseline, &text, fg);
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(tabs: usize) -> Workspace {
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        for _ in 1..tabs {
            workspace.file_command("file.new").unwrap();
        }
        workspace
    }

    fn ids(workspace: &Workspace) -> Vec<SessionDocumentId> {
        workspace.session().tabs().iter().map(|tab| tab.id).collect()
    }

    #[test]
    fn editor_zoom_changes_only_the_text_grid_never_menu_tabs_or_status() {
        let workspace = workspace(3);
        let ids = ids(&workspace);
        for kind in [TabLayout::Horizontal, TabLayout::Vertical] {
            for scale in [1.0, 1.25, 1.5, 2.0] {
                let m = Metrics::new(scale);
                let lay = layout((1600, 1000), &m, kind);
                let labels = [80, 120, 60];
                // Zoomed out, default and zoomed in editor rows.
                let geometries: Vec<_> =
                    [9, 20, 61].map(|ch| tab_geometry(&lay, &m, kind, &ids, Some(1), &labels, ch)).into();
                for (geometry, ch) in geometries.iter().zip([9, 20, 61]) {
                    assert_eq!(geometry.chrome, geometries[0].chrome);
                    assert_eq!(geometry.body, geometries[0].body);
                    assert_eq!(geometry.slots, geometries[0].slots);
                    assert_eq!(geometry.text.height % ch, 0, "text rows are whole editor rows");
                    assert_eq!(geometry.cell_height, ch, "scroll copies move whole editor rows");
                    let (source, target) = geometry.scroll_copy(1).unwrap();
                    assert_eq!(source.y - target.y, ch);
                }
                // Chrome is comfortable in logical pixels at every scale.
                let dip = |px: usize| px as f64 / scale;
                assert!((28.0..=32.0).contains(&dip(lay.menu.height)), "menu {} at {scale}", lay.menu.height);
                assert!((24.0..=28.0).contains(&dip(lay.status.height)), "status {} at {scale}", lay.status.height);
                if kind == TabLayout::Horizontal {
                    assert!((32.0..=36.0).contains(&dip(lay.strip.height)), "tabs {} at {scale}", lay.strip.height);
                    assert_eq!(lay.body.y, lay.strip.y + lay.strip.height);
                }
                assert_eq!(lay.status.y + lay.status.height, 1000);
                assert_eq!(lay.menu.y, 0);
            }
        }
    }

    #[test]
    fn active_tab_is_always_visible_and_page_arrows_reach_every_tab() {
        let workspace = workspace(40);
        let ids = ids(&workspace);
        let m = Metrics::new(1.5);
        let lay = layout((1200, 800), &m, TabLayout::Horizontal);
        // Mixed label widths make pages hold different tab counts.
        let labels: Vec<usize> = (0..40).map(|i| [40, 300, 120, 500][i % 4]).collect();
        let mut seen = std::collections::HashSet::new();
        let mut active = 0;
        for _ in 0..40 {
            let geometry = tab_geometry(&lay, &m, TabLayout::Horizontal, &ids, Some(active), &labels, 20);
            let slot = geometry.slots.iter().find(|slot| slot.active).expect("active tab is on the visible page");
            assert_eq!(slot.document, ids[active]);
            let right = geometry.previous.x;
            for pair in geometry.slots.windows(2) {
                assert_eq!(pair[0].area.x + pair[0].area.width, pair[1].area.x, "slots abut without overlap");
            }
            for slot in &geometry.slots {
                assert!(slot.area.x + slot.area.width <= right, "slots never run under the page arrows");
                assert!(slot.area.width >= m.tab_min.min(right));
                let close = slot.close;
                assert!(close.x >= slot.area.x && close.x + close.width <= slot.area.x + slot.area.width);
                assert!(close.width >= m.dip(18.0), "close target stays a real button");
                assert_eq!(
                    geometry.hit(close.x + close.width / 2, close.y + close.height / 2),
                    Some(TabAction::Close(slot.document))
                );
                assert_eq!(geometry.hit(slot.area.x + 2, slot.area.y + 2), Some(TabAction::Activate(slot.document)));
                seen.insert(slot.document);
            }
            let next = geometry.hit(geometry.next.x + 2, geometry.next.y + 2);
            let Some(TabAction::Activate(id)) = next.and_then(|action| resolve_page(&geometry, action, &ids)) else {
                break;
            };
            assert_eq!(ids.iter().position(|candidate| *candidate == id), Some(geometry.first + geometry.capacity));
            // Previous from the new page returns to the last tab of this page.
            let following =
                tab_geometry(&lay, &m, TabLayout::Horizontal, &ids, ids.iter().position(|c| *c == id), &labels, 20);
            let back = following.hit(following.previous.x + 2, following.previous.y + 2).unwrap();
            assert_eq!(
                resolve_page(&following, back, &ids),
                Some(TabAction::Activate(ids[geometry.first + geometry.capacity - 1]))
            );
            active = ids.iter().position(|candidate| *candidate == id).unwrap();
        }
        assert_eq!(seen.len(), 40, "every tab is reachable through the page arrows");
    }

    #[test]
    fn status_shows_fields_and_notice_but_never_the_full_path() {
        let mut workspace = workspace(1);
        workspace
            .execute("edit.insert", &CommandArgs { text: Some("one\ntwo".into()), ..Default::default() }, 1)
            .unwrap();
        workspace.set_notice("Saved  |  but with a pipe".into());
        let (notice, segments) = status(&workspace, 100);
        assert_eq!(notice, "Saved  |  but with a pipe");
        assert!(segments.iter().any(|s| s.text == "Ln 2, Col 4" && s.command == Some("navigation.goToLine")));
        assert!(!segments.iter().any(|s| s.text.starts_with("Zoom")), "default zoom stays quiet");
        let (_, zoomed) = status(&workspace, 121);
        assert!(zoomed.iter().any(|s| s.text == "Zoom 121%" && s.command == Some("view.zoomReset")));
        workspace.set_notice(String::new());
        let (notice, _) = status(&workspace, 100);
        assert!(notice.is_empty(), "no notice means an empty left side, not a path dump");
    }

    #[test]
    fn status_fields_come_from_typed_accessors_not_display_text() {
        // Untrusted text (a notice here; a path like `a  |  b.txt` the same way)
        // that looks like status fields must never become fields or warnings.
        let mut workspace = workspace(1);
        workspace.execute("edit.insert", &CommandArgs { text: Some("x".into()), ..Default::default() }, 1).unwrap();
        for text in ["7 lines  |  Ln 9, Col 9", "Recovery pending", "~ indexing 42%", "a  |  b.txt"] {
            workspace.set_notice(text.into());
            let (notice, segments) = status(&workspace, 100);
            assert_eq!(notice, text, "the whole notice stays a notice");
            let fields: Vec<&str> = segments.iter().map(|s| s.text.as_str()).collect();
            assert_eq!(fields, ["1 line", "Ln 1, Col 2"], "fields reflect the document only");
            assert!(segments.iter().all(|s| !s.warn), "no forged recovery warning");
        }
    }

    #[test]
    fn primitives_stay_inside_the_buffer_and_target() {
        let (w, h) = (40, 30);
        let mut pixels = vec![0; w * h];
        round_rect(&mut pixels, w, Rect { x: 30, y: 20, w: 30, h: 30 }, 6, 0xffffff);
        line(&mut pixels, w, (-10.0, -10.0), (100.0, 100.0), 3.0, 0xffffff);
        disc(&mut pixels, w, (39.0, 29.0), 8.0, 0xffffff);
        let mut pixels = vec![0; w * h];
        round_rect(&mut pixels, w, Rect { x: 4, y: 4, w: 20, h: 10 }, 5, 0xffffff);
        assert_eq!(pixels[9 * w + 14], 0xffffff, "interior is solid");
        assert_eq!(pixels[4 * w + 4], 0, "corner is rounded off");
        assert_eq!(pixels[3 * w + 14], 0, "nothing outside the rectangle");
    }
}
