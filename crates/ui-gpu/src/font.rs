//! Monospace ASCII fast path: glyphs are rasterized once, then cached as
//! ready-to-copy cell tiles per (byte, fg, bg). Non-ASCII shaping and font
//! fallback are M1 work (KW-11).

use crate::shaping::{Rasterizer, Tile, TileKey};
use std::collections::HashMap;

/// System monospace fonts, first found wins: (path, index in collection).
const CANDIDATES: &[(&str, u32, &str)] = &[
    (r"C:\Windows\Fonts\consola.ttf", 0, "Consolas"),
    (r"C:\Windows\Fonts\CascadiaMono.ttf", 0, "Cascadia Mono"),
    ("/System/Library/Fonts/SFNSMono.ttf", 0, "SF Mono"),
    ("/System/Library/Fonts/Menlo.ttc", 0, "Menlo"),
    ("/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf", 0, "DejaVu Sans Mono"),
    ("/usr/share/fonts/dejavu-sans-mono-fonts/DejaVuSansMono.ttf", 0, "DejaVu Sans Mono"),
    ("/usr/share/fonts/TTF/DejaVuSansMono.ttf", 0, "DejaVu Sans Mono"),
    ("/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf", 0, "Liberation Mono"),
    ("/usr/share/fonts/truetype/noto/NotoSansMono-Regular.ttf", 0, "Noto Sans Mono"),
];

pub struct Font {
    pub cw: usize,
    pub ch: usize,
    family: String,
    glyphs: Vec<(fontdue::Metrics, Vec<u8>)>,
    baseline: i32,
    tiles: HashMap<(u8, u32, u32), Vec<u32>>,
    pub px: f32,
    pub generation: u64,
    unicode: HashMap<TileKey, Vec<u32>>,
    waiting: HashMap<TileKey, Vec<usize>>,
    sender: Option<std::sync::mpsc::SyncSender<TileKey>>,
    unicode_bytes: usize,
    pub tab_size: usize,
    retry_rows: std::collections::HashSet<usize>,
    canceled: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// Parsed on the font worker while the event loop creates its window. Only
/// rasterization waits for that window's actual monitor scale.
struct PreparedFont {
    face: fontdue::Font,
    family: String,
}

impl Font {
    pub fn load(px: f32) -> Option<Font> {
        let prepared = Self::prepare_default()?;
        Self::from_face(prepared.face, px, &prepared.family)
    }
    fn prepare_default() -> Option<PreparedFont> {
        let (data, index, family) =
            CANDIDATES.iter().find_map(|(p, i, family)| Some((std::fs::read(p).ok()?, *i, *family)))?;
        let settings = fontdue::FontSettings { collection_index: index, ..Default::default() };
        Some(PreparedFont { face: fontdue::Font::from_bytes(data, settings).ok()?, family: family.into() })
    }
    pub fn startup(size: std::sync::mpsc::Receiver<f32>) -> Option<Font> {
        let prepared = Self::prepare_default()?;
        let px = size.recv().ok()?;
        Self::from_face(prepared.face, px, &prepared.family)
    }
    pub fn load_named(px: f32, family: &str) -> Option<Font> {
        if family == "monospace" {
            return Self::load(px);
        }
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        let id = db.query(&fontdb::Query { families: &[fontdb::Family::Name(family)], ..Default::default() })?;
        db.with_face_data(id, |data, index| Self::from_bytes(data.to_vec(), index, px, family))?
    }
    fn from_bytes(data: Vec<u8>, index: u32, px: f32, family: &str) -> Option<Font> {
        let settings = fontdue::FontSettings { collection_index: index, ..Default::default() };
        let font = fontdue::Font::from_bytes(data, settings).ok()?;
        Self::from_face(font, px, family)
    }
    fn from_face(font: fontdue::Font, px: f32, family: &str) -> Option<Font> {
        let lm = font.horizontal_line_metrics(px)?;
        let glyphs: Vec<_> = (32u8..127).map(|c| font.rasterize(c as char, px)).collect();
        let cw = glyphs[(b'M' - 32) as usize].0.advance_width.round().max(1.0) as usize;
        let ch = (lm.ascent - lm.descent + lm.line_gap).ceil() as usize + 2;
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Some(Font {
            cw,
            ch,
            family: family.into(),
            glyphs,
            baseline: lm.ascent.ceil() as i32 + 1,
            tiles: HashMap::new(),
            px,
            generation: NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            unicode: HashMap::new(),
            waiting: HashMap::new(),
            sender: None,
            unicode_bytes: 0,
            tab_size: 4,
            retry_rows: Default::default(),
            canceled: Default::default(),
        })
    }

    pub fn enable_unicode(&mut self, _family: String, ready: impl Fn(Tile, u64) + Send + 'static) {
        // The retained font can differ from a failed configured-family request.
        // Always shape combining clusters with the actual surviving primary face.
        let family = self.family.clone();
        if self.sender.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::sync_channel::<TileKey>(256);
        let (px, cw, ch, baseline, generation) = (self.px, self.cw, self.ch, self.baseline, self.generation);
        self.sender = Some(tx);
        let canceled = self.canceled.clone();
        std::thread::spawn(move || {
            // Discovery is lazy even on this worker: ASCII-only files never scan fonts.
            let mut raster = None;
            for key in rx {
                if canceled.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                let raster = raster.get_or_insert_with(|| Rasterizer::new(&family, px, cw, ch, baseline));
                ready(raster.rasterize(key), generation);
            }
        });
    }
    pub fn tab_size(&self) -> usize {
        self.tab_size
    }
    pub fn invalidate_rows(&mut self) {
        for rows in self.waiting.values_mut() {
            rows.clear();
        }
        self.retry_rows.clear();
    }
    pub fn scroll_rows(&mut self, delta: i64, visible: usize) {
        for rows in self.waiting.values_mut() {
            move_rows(rows, delta, self.ch, visible);
        }
        let mut retry = self.retry_rows.drain().collect();
        move_rows(&mut retry, delta, self.ch, visible);
        self.retry_rows.extend(retry);
    }
    #[cfg(feature = "frame-harness")]
    pub fn pending_glyphs(&self) -> usize {
        self.waiting.len() + self.retry_rows.len()
    }
    #[cfg(feature = "frame-harness")]
    pub fn unicode_enabled(&self) -> bool {
        self.sender.is_some()
    }
    pub fn accept(&mut self, tile: Tile) -> Vec<usize> {
        let mut rows = self.waiting.remove(&tile.key).unwrap_or_default();
        rows.extend(self.retry_rows.drain());
        let bytes = tile.pixels.len() * 4;
        if self.unicode_bytes + bytes > 8 * 1024 * 1024 {
            self.unicode.clear();
            self.unicode_bytes = 0;
        }
        self.unicode_bytes += bytes;
        self.unicode.insert(tile.key, tile.pixels);
        rows
    }
    pub fn unicode_tile(&mut self, key: TileKey, y: usize) -> Option<&[u32]> {
        if !self.unicode.contains_key(&key) {
            if let Some(rows) = self.waiting.get_mut(&key) {
                if !rows.contains(&y) {
                    rows.push(y);
                }
            } else if let Some(tx) = &self.sender {
                if tx.try_send(key.clone()).is_ok() {
                    self.waiting.insert(key.clone(), vec![y]);
                } else {
                    self.retry_rows.insert(y);
                }
            }
        }
        self.unicode.get(&key).map(Vec::as_slice)
    }

    pub fn tile(&mut self, c: u8, fg: u32, bg: u32) -> &[u32] {
        let c = if (32..127).contains(&c) { c } else { b'?' };
        let (cw, ch, baseline) = (self.cw, self.ch, self.baseline);
        let glyphs = &self.glyphs;
        self.tiles.entry((c, fg, bg)).or_insert_with(|| {
            let mut t = vec![bg; cw * ch];
            let (m, bmp) = &glyphs[(c - 32) as usize];
            let top = baseline - m.height as i32 - m.ymin;
            for gy in 0..m.height {
                for gx in 0..m.width {
                    let (x, y) = (m.xmin + gx as i32, top + gy as i32);
                    if x >= 0 && y >= 0 && (x as usize) < cw && (y as usize) < ch {
                        t[y as usize * cw + x as usize] = blend(bg, fg, bmp[gy * m.width + gx]);
                    }
                }
            }
            t
        })
    }
}

pub(crate) fn blend(bg: u32, fg: u32, a: u8) -> u32 {
    let a = a as u32;
    let ch = |s: u32| {
        let (b, f) = ((bg >> s) & 0xff, (fg >> s) & 0xff);
        ((b * (255 - a) + f * a) / 255) << s
    };
    ch(16) | ch(8) | ch(0)
}
impl Drop for Font {
    fn drop(&mut self) {
        self.canceled.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}
fn move_rows(rows: &mut Vec<usize>, delta: i64, ch: usize, visible: usize) {
    let shift = delta * ch as i64;
    rows.retain_mut(|y| {
        // The status field is fixed while text pixels scroll.
        if *y >= visible * ch {
            return true;
        }
        let next = *y as i64 - shift;
        if (0..(visible * ch) as i64).contains(&next) {
            *y = next as usize;
            true
        } else {
            false
        }
    });
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(windows)]
    fn startup_font_uses_requested_monitor_pixels_before_first_frame() {
        for scale in [1.25, 1.5, 2.0] {
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            let worker = std::thread::spawn(move || Font::startup(rx));
            let expected = crate::zoom::Zoom::default().physical_px(14.0, scale);
            crate::zoom::start_font_rasterization(tx, 14.0, scale, crate::zoom::Zoom::default()).unwrap();
            let font = worker.join().unwrap().expect("Windows default monospace font");
            assert_eq!(font.px, expected, "first-frame rasterization must use the actual monitor DPI");
            let reference = Font::load(expected).unwrap();
            assert_eq!((font.cw, font.ch), (reference.cw, reference.ch));
            assert!(font.cw > Font::load(14.0).unwrap().cw);
        }
    }
    #[test]
    fn deferred_glyph_damage_follows_scroll_copy() {
        let mut rows = vec![0, 40, 100, 180];
        move_rows(&mut rows, 2, 20, 10);
        assert_eq!(rows, vec![0, 60, 140]);
        move_rows(&mut rows, -3, 20, 10);
        assert_eq!(rows, vec![60, 120]);
    }
    fn waiting_font(ys: Vec<usize>) -> (Font, TileKey) {
        let key = TileKey { text: "界".into(), columns: 2, fg: 1, bg: 0 };
        let font = Font {
            cw: 1,
            ch: 10,
            family: "monospace".into(),
            glyphs: vec![],
            baseline: 8,
            tiles: HashMap::new(),
            px: 8.0,
            generation: 1,
            unicode: HashMap::new(),
            waiting: HashMap::from([(key.clone(), ys)]),
            sender: None,
            unicode_bytes: 0,
            tab_size: 4,
            retry_rows: Default::default(),
            canceled: Default::default(),
        };
        (font, key)
    }
    #[test]
    fn glyph_arrival_between_queued_scroll_and_paint_damages_moved_row() {
        let (mut font, key) = waiting_font(vec![50]);
        let mut pending = crate::Damage::default();
        crate::queue_damage(
            &mut pending,
            crate::Damage { scroll: 2, rows: vec![8, 9], ..Default::default() },
            10,
            Some(&mut font),
        );
        let arrived = crate::glyph_damage(&mut font, Tile { key, pixels: vec![0; 20] }, 10);
        assert_eq!(arrived.rows, vec![3]);
        crate::queue_damage(&mut pending, arrived, 10, Some(&mut font));
        crate::queue_damage(
            &mut pending,
            crate::Damage { scroll: 1, rows: vec![9], ..Default::default() },
            10,
            Some(&mut font),
        );
        assert!(pending.rows.contains(&2));
        assert!(!pending.rows.contains(&5));
        assert_eq!(pending.scroll, 3);
    }
    #[test]
    fn queued_scroll_preserves_fixed_status_glyph_damage() {
        let (mut font, key) = waiting_font(vec![100]);
        let mut pending = crate::Damage::default();
        crate::queue_damage(&mut pending, crate::Damage { scroll: 2, ..Default::default() }, 10, Some(&mut font));
        let arrived = crate::glyph_damage(&mut font, Tile { key, pixels: vec![0; 20] }, 10);
        assert!(arrived.status);
        assert!(arrived.rows.is_empty());
    }
}
