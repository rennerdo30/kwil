//! Tab strip over the shared tab model. Slot geometry comes from DPI chrome
//! metrics and UI text widths; the workspace still owns every tab action.
//! Editor drawing and glyph row coordinates remain relative to the body
//! framebuffer; only composition/copy uses window origins.
use super::*;
use chrome::{Hover, Metrics};
use kwil_core_commands::settings::TabLayout;
use kwil_view_model::{TabArea, TabGeometry};

/// Card row backgrounds still used by the palette and settings overlays.
pub(super) const ACTIVE: u32 = 0x192a43;
pub(super) const INACTIVE: u32 = 0x101d31;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Frame {
    pub geometry: TabGeometry,
    /// Labels of the visible slots, without the workspace's dirty prefix.
    pub titles: Vec<String>,
    pub dirty: Vec<bool>,
    pub layout: chrome::Layout,
    pub hover: Hover,
    pub kind: TabLayout,
}

/// Session tab ids in strip order.
pub(super) fn ids(view: &Workspace) -> Vec<kwil_view_model::SessionDocumentId> {
    view.session().tabs().iter().map(|tab| tab.id).collect()
}

#[allow(clippy::too_many_arguments)]
pub(super) fn frame(
    view: &Workspace,
    size: (usize, usize),
    m: &Metrics,
    ch: usize,
    kind: TabLayout,
    mut font: Option<&mut font::UiFont>,
    hover: Hover,
) -> Frame {
    let layout = chrome::layout(size, m, kind);
    let tabs = view.session().tabs();
    // Same dirty rule as the workspace title, so the dot never disagrees with it.
    let dirty: Vec<bool> = tabs
        .iter()
        .map(|tab| {
            view.view(tab.id)
                .and_then(View::editor)
                .map_or(tab.revision != tab.saved_revision, |editor| editor.document().is_dirty())
        })
        .collect();
    let names: Vec<String> = tabs
        .iter()
        .zip(&dirty)
        .map(|(tab, dirty)| {
            let title = view.tab_title(tab.id);
            match title.strip_prefix("* ") {
                Some(name) if *dirty => name.to_owned(),
                _ => title,
            }
        })
        .collect();
    let labels: Vec<usize> = names
        .iter()
        .map(|name| match font.as_deref_mut() {
            // Only up to the widest tab matters; long names are never measured in full.
            Some(font) => font.measure(name, m.tab_max),
            None => name.chars().count() * m.dip(7.0),
        })
        .map(|width| width + m.dip(22.0))
        .collect();
    let ids: Vec<_> = tabs.iter().map(|tab| tab.id).collect();
    let active = ids.iter().position(|id| *id == view.active_id());
    let geometry = chrome::tab_geometry(&layout, m, kind, &ids, active, &labels, ch);
    let visible = geometry.first..geometry.first + geometry.slots.len();
    Frame { titles: names[visible.clone()].to_vec(), dirty: dirty[visible].to_vec(), geometry, layout, hover, kind }
}

pub(super) fn rect(area: TabArea) -> Rect {
    Rect { x: area.x, y: area.y, w: area.width, h: area.height }
}

/// Which tab element is under the pointer.
pub(super) fn hover(frame: &Frame, x: usize, y: usize) -> Hover {
    let g = &frame.geometry;
    if g.first > 0 && g.previous.contains(x, y) {
        return Hover::Previous;
    }
    if g.first + g.capacity < g.total && g.next.contains(x, y) {
        return Hover::Next;
    }
    for (index, slot) in g.slots.iter().enumerate() {
        if slot.close.contains(x, y) {
            return Hover::Close(index);
        }
        if slot.area.contains(x, y) {
            return Hover::Tab(index);
        }
    }
    Hover::None
}

pub(super) fn draw(
    frame: &Frame,
    pixels: &mut [u32],
    stride: usize,
    font: Option<&mut font::UiFont>,
    theme: &kwil_core_commands::theme::Theme,
    m: &Metrics,
) {
    use kwil_core_commands::theme::UiRole;
    let color = |role| theme_ui::rgb(theme, role);
    let (strip_bg, active_bg) = (color(UiRole::TabBackground), color(UiRole::ActiveTabBackground));
    let (fg, active_fg) = (color(UiRole::TabForeground), color(UiRole::ActiveTabForeground));
    let (accent, border) = (color(UiRole::Accent), color(UiRole::Border));
    let g = &frame.geometry;
    let strip = rect(g.chrome);
    let horizontal = frame.kind == TabLayout::Horizontal;
    chrome::fill_clipped(pixels, stride, strip, strip_bg);
    // Hairline between the strip and the editor; the active tab covers it.
    let edge = if horizontal {
        Rect { x: strip.x, y: (strip.y + strip.h).saturating_sub(m.line), w: strip.w, h: m.line.min(strip.h) }
    } else {
        Rect { x: (strip.x + strip.w).saturating_sub(m.line), y: strip.y, w: m.line.min(strip.w), h: strip.h }
    };
    chrome::fill_clipped(pixels, stride, edge, chrome::mix(strip_bg, border, 0xa0));
    let mut font = font;
    for (index, (slot, title)) in g.slots.iter().zip(&frame.titles).enumerate() {
        let area = rect(slot.area);
        let hovered = matches!(frame.hover, Hover::Tab(i) | Hover::Close(i) if i == index);
        let bg = if slot.active {
            active_bg
        } else if hovered {
            chrome::mix(strip_bg, fg, 0x16)
        } else {
            strip_bg
        };
        let text_fg = if slot.active {
            active_fg
        } else if hovered {
            fg
        } else {
            chrome::mix(strip_bg, fg, 0xb4)
        };
        let gap = m.dip(2.0);
        if horizontal && slot.active {
            // The active tab is a raised sheet joined to the editor below it:
            // rounded top corners, square bottom, no hairline underneath.
            let sheet = Rect { x: area.x + gap, y: area.y + m.dip(4.0), w: area.w.saturating_sub(2 * gap), h: 0 };
            let sheet = Rect { h: (area.y + area.h).saturating_sub(sheet.y), ..sheet };
            top_rounded(pixels, stride, sheet, m.dip(6.0), bg);
        } else if horizontal && hovered {
            let inset = m.dip(5.0).min(area.h / 4);
            let pill = Rect {
                x: area.x + gap,
                y: area.y + inset,
                w: area.w.saturating_sub(2 * gap),
                h: area.h.saturating_sub(2 * inset),
            };
            chrome::round_rect(pixels, stride, pill, m.radius, bg);
        } else if slot.active || hovered {
            let inset = m.dip(3.0);
            let pill = Rect {
                x: area.x + inset,
                y: area.y + inset / 2,
                w: area.w.saturating_sub(2 * inset + m.line),
                h: area.h.saturating_sub(inset),
            };
            chrome::round_rect(pixels, stride, pill, m.radius, bg);
            if slot.active {
                let h = area.h / 2;
                let bar = Rect { x: pill.x, y: area.y + (area.h - h) / 2, w: m.indicator, h };
                chrome::round_rect(pixels, stride, bar, m.indicator / 2, accent);
            }
        }
        if !slot.active && horizontal && !hovered {
            // Quiet separators between inactive neighbours only.
            let next_active = g.slots.get(index + 1).is_some_and(|next| next.active);
            let next_hovered = matches!(frame.hover, Hover::Tab(i) | Hover::Close(i) if i == index + 1);
            if !next_active && !next_hovered {
                let inset = area.h / 4;
                chrome::fill_clipped(
                    pixels,
                    stride,
                    Rect {
                        x: (area.x + area.w).saturating_sub(m.line),
                        y: area.y + inset,
                        w: m.line,
                        h: area.h - 2 * inset,
                    },
                    chrome::mix(strip_bg, border, 0x90),
                );
            }
        }
        let close = slot.close;
        if let Some(font) = font.as_deref_mut() {
            let icon =
                if title.to_ascii_lowercase().ends_with(".md") { icons::Icon::Markdown } else { icons::Icon::File };
            icons::draw(
                pixels,
                stride,
                icon,
                ((area.x + m.pad + m.dip(8.0)) as f32, (area.y + area.h / 2) as f32),
                m.dip(16.0),
                text_fg,
            );
            let left = area.x + m.pad + m.dip(22.0);
            let room = close.x.saturating_sub(left + m.dip(4.0));
            let label = font.fit(title, room);
            let baseline = font.baseline(area.y, area.h);
            font.draw(pixels, stride, area, left, baseline, &label, text_fg);
        }
        let close_hovered = frame.hover == Hover::Close(index);
        if frame.dirty.get(index).copied().unwrap_or(false) && !close_hovered {
            let center = (close.x as f32 + close.width as f32 / 2.0, close.y as f32 + close.height as f32 / 2.0);
            chrome::disc(pixels, stride, center, m.dip(4.0) as f32, text_fg);
        } else {
            if close_hovered {
                chrome::round_rect(pixels, stride, rect(close), m.radius, chrome::mix(bg, fg, 0x34));
            }
            let mark = if slot.active || hovered { text_fg } else { chrome::mix(strip_bg, fg, 0x78) };
            icons::draw(
                pixels,
                stride,
                icons::Icon::Close,
                ((close.x + close.width / 2) as f32, (close.y + close.height / 2) as f32),
                m.dip(16.0),
                mark,
            );
        }
    }
    for (area, enabled, left, hover) in
        [(g.previous, g.first > 0, true, Hover::Previous), (g.next, g.first + g.capacity < g.total, false, Hover::Next)]
    {
        if area.width == 0 || area.height == 0 {
            continue;
        }
        let r = rect(area);
        chrome::fill_clipped(pixels, stride, r, strip_bg);
        if enabled && frame.hover == hover {
            let inset = m.dip(4.0).min(r.h / 4);
            chrome::round_rect(
                pixels,
                stride,
                Rect {
                    x: r.x + inset / 2,
                    y: r.y + inset,
                    w: r.w.saturating_sub(inset),
                    h: r.h.saturating_sub(2 * inset),
                },
                m.radius,
                chrome::mix(strip_bg, fg, 0x2c),
            );
        }
        let mark = if enabled { fg } else { chrome::mix(strip_bg, fg, 0x50) };
        icons::draw(
            pixels,
            stride,
            if left { icons::Icon::Back } else { icons::Icon::Forward },
            ((area.x + area.width / 2) as f32, (area.y + area.height / 2) as f32),
            m.dip(16.0),
            mark,
        );
    }
}

/// Filled rectangle with anti-aliased top corners only. Stays inside `r`.
fn top_rounded(buf: &mut [u32], stride: usize, r: Rect, radius: usize, color: u32) {
    let radius = radius.min(r.w / 2).min(r.h / 2);
    if radius == 0 {
        chrome::fill_clipped(buf, stride, r, color);
        return;
    }
    // Rounded cap of twice the radius, then the square remainder over its
    // lower corners; every pixel written lies inside `r`.
    chrome::round_rect(buf, stride, Rect { h: 2 * radius, ..r }, radius, color);
    chrome::fill_clipped(buf, stride, Rect { y: r.y + radius, h: r.h - radius, ..r }, color);
}

pub(super) fn composite(
    source: &[u32],
    source_stride: usize,
    target: &mut [u32],
    target_stride: usize,
    origin: TabArea,
    area: Rect,
) {
    if area.w == 0 || area.h == 0 {
        return;
    }
    for row in area.y..area.y + area.h {
        let from = row * source_stride + area.x;
        let to = (origin.y + row) * target_stride + origin.x + area.x;
        target[to..to + area.w].copy_from_slice(&source[from..from + area.w]);
    }
}

pub(super) fn translated(area: Rect, origin: TabArea) -> Rect {
    Rect { x: area.x + origin.x, y: area.y + origin.y, ..area }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_composition_preserves_chrome_and_translates_only_damaged_rows() {
        let stride = 40;
        for origin in [TabArea { x: 0, y: 4, width: 40, height: 20 }, TabArea { x: 12, y: 0, width: 28, height: 24 }] {
            let mut target = vec![0xabcdef; stride * 28];
            let source = vec![0x123456; origin.width * origin.height];
            let row = Rect { x: 0, y: 4, w: origin.width, h: 2 };
            composite(&source, origin.width, &mut target, stride, origin, row);
            let damage = translated(row, origin);
            for y in 0..28 {
                for x in 0..stride {
                    let painted = x >= damage.x && x < damage.x + damage.w && y >= damage.y && y < damage.y + damage.h;
                    assert_eq!(target[y * stride + x], if painted { 0x123456 } else { 0xabcdef });
                }
            }
        }
    }

    #[test]
    fn active_tab_sheet_rounds_only_its_top_and_stays_inside_its_slot() {
        let (w, h) = (60, 30);
        let mut pixels = vec![0; w * h];
        let r = Rect { x: 10, y: 5, w: 40, h: 20 };
        top_rounded(&mut pixels, w, r, 6, 0xffffff);
        assert_eq!(pixels[5 * w + 10], 0, "top-left corner is rounded off");
        assert_eq!(pixels[24 * w + 10], 0xffffff, "bottom-left corner is square to join the editor");
        for y in 0..h {
            for x in 0..w {
                if !r.contains((x, y)) {
                    assert_eq!(pixels[y * w + x], 0, "painted outside the slot at {x},{y}");
                }
            }
        }
    }

    #[test]
    fn hover_targets_match_the_hit_test_and_dirty_tabs_keep_their_name() {
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        workspace.file_command("file.new").unwrap();
        workspace.execute("edit.insert", &CommandArgs { text: Some("x".into()), ..Default::default() }, 1).unwrap();
        let m = Metrics::new(1.0);
        let frame = frame(&workspace, (1000, 700), &m, 18, TabLayout::Horizontal, None, Hover::None);
        assert_eq!(frame.titles.len(), 2);
        assert!(frame.dirty[1] && !frame.titles[1].starts_with('*'), "dirty state is a dot, not a '* ' prefix");
        assert!(frame.titles.iter().all(|title| !title.contains("[x]")));
        for (index, slot) in frame.geometry.slots.iter().enumerate() {
            let (cx, cy) = (slot.close.x + slot.close.width / 2, slot.close.y + slot.close.height / 2);
            assert_eq!(hover(&frame, cx, cy), Hover::Close(index));
            assert_eq!(frame.geometry.hit(cx, cy), Some(kwil_view_model::TabAction::Close(slot.document)));
            assert_eq!(hover(&frame, slot.area.x + 3, cy), Hover::Tab(index));
        }
        assert_eq!(hover(&frame, 500, 400), Hover::None, "the editor body is not a tab target");
    }
}
