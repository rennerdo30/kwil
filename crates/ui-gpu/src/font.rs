//! Monospace ASCII fast path: glyphs are rasterized once, then cached as
//! ready-to-copy cell tiles per (byte, fg, bg). Non-ASCII shaping and font
//! fallback are M1 work (KW-11).

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
}

impl Font {
    pub fn load(px: f32) -> Option<Font> {
        let (data, index) = CANDIDATES.iter().find_map(|(p, i)| Some((std::fs::read(p).ok()?, *i)))?;
        let settings = fontdue::FontSettings { collection_index: index, ..Default::default() };
        let font = fontdue::Font::from_bytes(data, settings).ok()?;
        let lm = font.horizontal_line_metrics(px)?;
        let glyphs: Vec<_> = (32u8..127).map(|c| font.rasterize(c as char, px)).collect();
        let cw = glyphs[(b'M' - 32) as usize].0.advance_width.round().max(1.0) as usize;
        let ch = (lm.ascent - lm.descent + lm.line_gap).ceil() as usize + 2;
        Some(Font { cw, ch, glyphs, baseline: lm.ascent.ceil() as i32 + 1, tiles: HashMap::new() })
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

fn blend(bg: u32, fg: u32, a: u8) -> u32 {
    let a = a as u32;
    let ch = |s: u32| {
        let (b, f) = ((bg >> s) & 0xff, (fg >> s) & 0xff);
        ((b * (255 - a) + f * a) / 255) << s
    };
    ch(16) | ch(8) | ch(0)
}
