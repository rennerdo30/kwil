//! Thin, virtualized chrome. Editor drawing and glyph row coordinates remain
//! relative to the body framebuffer; only composition/copy uses window origins.
use super::*;
use kwil_view_model::{TabArea, TabGeometry, TabMetrics};

pub(super) const ACTIVE: u32 = 0x192a43;
pub(super) const INACTIVE: u32 = 0x101d31;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Frame {
    pub geometry: TabGeometry,
    pub titles: Vec<String>,
}

pub(super) fn body(
    size: (usize, usize),
    cw: usize,
    ch: usize,
    layout: kwil_core_commands::settings::TabLayout,
) -> TabArea {
    use kwil_core_commands::settings::TabLayout;
    let (w, h) = size;
    let content = h.saturating_sub(ch);
    match layout {
        TabLayout::Horizontal => {
            let top = ch.min(content);
            TabArea { x: 0, y: top, width: w, height: content - top }
        }
        TabLayout::Vertical => {
            let left = cw.saturating_mul(26).min(w / 3);
            TabArea { x: left, y: 0, width: w - left, height: content }
        }
    }
}

pub(super) fn frame(
    view: &Workspace,
    size: (usize, usize),
    cw: usize,
    ch: usize,
    layout: kwil_core_commands::settings::TabLayout,
) -> Frame {
    let geometry = view.tab_geometry(
        TabMetrics { width: size.0, height: size.1, cell_width: cw, cell_height: ch, scrollbar: BAR },
        layout,
    );
    let titles = geometry.slots.iter().map(|slot| view.tab_title(slot.document)).collect();
    Frame { geometry, titles }
}

pub(super) fn rect(area: TabArea) -> Rect {
    Rect { x: area.x, y: area.y, w: area.width, h: area.height }
}

pub(super) fn draw(frame: &Frame, pixels: &mut [u32], stride: usize, font: &mut Font) {
    fill(pixels, stride, rect(frame.geometry.chrome), INACTIVE);
    for (slot, title) in frame.geometry.slots.iter().zip(&frame.titles) {
        let bg = if slot.active { ACTIVE } else { INACTIVE };
        fill(pixels, stride, rect(slot.area), bg);
        let columns = slot.close.x.saturating_sub(slot.area.x) / font.cw;
        draw_text(pixels, stride, font, slot.area.x, slot.area.y, title.as_bytes(), columns, FG, bg);
        draw_text(pixels, stride, font, slot.close.x, slot.close.y, b"[x]", slot.close.width / font.cw, GUTTER_FG, bg);
    }
    for (area, enabled, text) in [
        (frame.geometry.previous, frame.geometry.first > 0, b"<".as_slice()),
        (frame.geometry.next, frame.geometry.first + frame.geometry.capacity < frame.geometry.total, b">".as_slice()),
    ] {
        if enabled && area.height >= font.ch {
            draw_text(pixels, stride, font, area.x, area.y, text, area.width / font.cw, STATUS_FG, INACTIVE);
        }
    }
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
}
