//! Monospace ASCII fast path: glyphs are rasterized once, then cached as
//! ready-to-copy cell tiles per (byte, fg, bg). Non-ASCII shaping and font
//! fallback are M1 work (KW-11).

use crate::shaping::{Rasterizer, Tile, TileKey};
use std::collections::HashMap;

/// System monospace fonts, first found wins: (path, index in collection).
const CANDIDATES: &[(&str, u32)] = &[
    (r"C:\Windows\Fonts\consola.ttf", 0),
    (r"C:\Windows\Fonts\CascadiaMono.ttf", 0),
    ("/System/Library/Fonts/SFNSMono.ttf", 0),
    ("/System/Library/Fonts/Menlo.ttc", 0),
    ("/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf", 0),
    ("/usr/share/fonts/dejavu-sans-mono-fonts/DejaVuSansMono.ttf", 0),
    ("/usr/share/fonts/TTF/DejaVuSansMono.ttf", 0),
    ("/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf", 0),
    ("/usr/share/fonts/truetype/noto/NotoSansMono-Regular.ttf", 0),
];

pub struct Font {
    pub cw: usize,
    pub ch: usize,
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
}

impl Font {
    pub fn load(px: f32) -> Option<Font> {
        let (data, index) = CANDIDATES.iter().find_map(|(p, i)| Some((std::fs::read(p).ok()?, *i)))?;
        Self::from_bytes(data, index, px)
    }
    pub fn load_named(px: f32, family: &str) -> Option<Font> {
        if family == "monospace" {
            return Self::load(px);
        }
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        let id = db.query(&fontdb::Query { families: &[fontdb::Family::Name(family)], ..Default::default() })?;
        db.with_face_data(id, |data, index| Self::from_bytes(data.to_vec(), index, px))?
    }
    fn from_bytes(data: Vec<u8>, index: u32, px: f32) -> Option<Font> {
        let settings = fontdue::FontSettings { collection_index: index, ..Default::default() };
        let font = fontdue::Font::from_bytes(data, settings).ok()?;
        let lm = font.horizontal_line_metrics(px)?;
        let glyphs: Vec<_> = (32u8..127).map(|c| font.rasterize(c as char, px)).collect();
        let cw = glyphs[(b'M' - 32) as usize].0.advance_width.round().max(1.0) as usize;
        let ch = (lm.ascent - lm.descent + lm.line_gap).ceil() as usize + 2;
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Some(Font {
            cw,
            ch,
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
        })
    }

    pub fn enable_unicode(&mut self, family: String, ready: impl Fn(Tile, u64) + Send + 'static) {
        if self.sender.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::sync_channel::<TileKey>(256);
        let (px, cw, ch, baseline, generation) = (self.px, self.cw, self.ch, self.baseline, self.generation);
        self.sender = Some(tx);
        std::thread::spawn(move || {
            // Discovery is lazy even on this worker: ASCII-only files never scan fonts.
            let mut raster = None;
            for key in rx {
                let raster = raster.get_or_insert_with(|| Rasterizer::new(&family, px, cw, ch, baseline));
                ready(raster.rasterize(key), generation);
            }
        });
    }
    pub fn tab_size(&self) -> usize {
        self.tab_size
    }
    pub fn accept(&mut self, tile: Tile) -> Vec<usize> {
        let rows = self.waiting.remove(&tile.key).unwrap_or_default();
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
            } else if let Some(tx) = &self.sender
                && tx.try_send(key.clone()).is_ok()
            {
                self.waiting.insert(key.clone(), vec![y]);
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
