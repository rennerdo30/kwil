//! Deferred Unicode shaping/rasterization. This module is used only by a worker
//! after the CPU first frame. Font discovery and glyph fallback never block input.
use std::collections::HashMap;
use swash::{
    CacheKey, FontRef,
    scale::{Render, ScaleContext, Source, StrikeWith, image::Content},
    shape::ShapeContext,
    text::{Codepoint, Script},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const MAX_FONTS: usize = 16;
const MAX_FONT_BYTES: usize = 64 * 1024 * 1024;

/// Includes negative results; a missing glyph must not rescan the font database
/// on every repaint. Bounded independently of document length.
#[derive(Default)]
pub(crate) struct FallbackCache(HashMap<String, Option<fontdb::ID>>);
impl FallbackCache {
    fn resolve(&mut self, cluster: &str, lookup: impl FnOnce() -> Option<fontdb::ID>) -> Option<fontdb::ID> {
        if self.0.len() >= 4096 {
            self.0.clear();
        }
        *self.0.entry(cluster.to_owned()).or_insert_with(lookup)
    }
}

struct Face {
    data: Vec<u8>,
    offset: u32,
    key: CacheKey,
}
impl Face {
    fn font(&self) -> FontRef<'_> {
        FontRef { data: &self.data, offset: self.offset, key: self.key }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TileKey {
    pub text: String,
    pub columns: usize,
    pub fg: u32,
    pub bg: u32,
}
pub(crate) struct Tile {
    pub key: TileKey,
    pub pixels: Vec<u32>,
}

pub(crate) struct Rasterizer {
    db: fontdb::Database,
    fallback: FallbackCache,
    faces: HashMap<fontdb::ID, Face>,
    bytes: usize,
    shape: ShapeContext,
    scale: ScaleContext,
    px: f32,
    cw: usize,
    ch: usize,
    baseline: i32,
    primary: Option<fontdb::ID>,
}
impl Rasterizer {
    pub fn new(family: &str, px: f32, cw: usize, ch: usize, baseline: i32) -> Self {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        let primary = db.query(&fontdb::Query {
            families: &[fontdb::Family::Name(family), fontdb::Family::Monospace],
            ..Default::default()
        });
        Self {
            db,
            primary,
            fallback: FallbackCache::default(),
            faces: HashMap::new(),
            bytes: 0,
            shape: ShapeContext::new(),
            scale: ScaleContext::new(),
            px,
            cw,
            ch,
            baseline,
        }
    }
    fn face_for(&mut self, text: &str) -> Option<fontdb::ID> {
        let covers = |font: FontRef<'_>| {
            text.chars()
                .filter(|c| !matches!(*c, '\u{200d}' | '\u{fe0f}' | '\u{fe0e}'))
                .all(|c| font.charmap().map(c) != 0)
        };
        let db = &self.db;
        let id = self.fallback.resolve(text, || {
            self.primary.into_iter().chain(db.faces().map(|f| f.id)).find(|id| {
                db.with_face_data(*id, |data, index| FontRef::from_index(data, index as usize).is_some_and(covers))
                    .unwrap_or(false)
            })
        })?;
        if !self.faces.contains_key(&id) {
            if self.faces.len() >= MAX_FONTS {
                return None;
            }
            let remaining = MAX_FONT_BYTES.saturating_sub(self.bytes);
            let face = db.with_face_data(id, |data, index| {
                if data.len() > remaining {
                    return None;
                }
                let font = FontRef::from_index(data, index as usize)?;
                Some(Face { offset: font.offset, key: CacheKey::new(), data: data.to_vec() })
            })??;
            self.bytes += face.data.len();
            self.faces.insert(id, face);
        }
        Some(id)
    }
    pub fn rasterize(&mut self, key: TileKey) -> Tile {
        let width = key.columns * self.cw;
        let mut pixels = vec![key.bg; width * self.ch];
        // Itemize by fallback face and script; adjacent clusters shape together,
        // preserving joining and ligatures within each same-script font run.
        let mut runs: Vec<(String, Option<fontdb::ID>, Script, usize, usize)> = Vec::new();
        let mut col = 0;
        for g in key.text.graphemes(true) {
            let id = self.face_for(g);
            let script = g
                .chars()
                .map(|c| c.script())
                .find(|s| !matches!(s, Script::Common | Script::Inherited))
                .unwrap_or(Script::Latin);
            let n = UnicodeWidthStr::width(g).max(1);
            if let Some(last) = runs.last_mut().filter(|r| r.1 == id && r.2 == script) {
                last.0.push_str(g);
                last.4 += n;
            } else {
                runs.push((g.into(), id, script, col, n));
            }
            col += n;
        }
        for (text, id, script, column, columns) in runs {
            let Some(id) = id else {
                // Visible missing-font box; never silently discard source text.
                let left = column * self.cw;
                let right = ((column + columns) * self.cw).min(width);
                for y in 2..self.ch.saturating_sub(2) {
                    for x in left..right {
                        if y == 2 || y == self.ch - 3 || x == left || x + 1 == right {
                            pixels[y * width + x] = key.fg;
                        }
                    }
                }
                continue;
            };
            let font = self.faces[&id].font();
            let mut shaper = self.shape.builder(font).script(script).size(self.px).build();
            shaper.add_str(&text);
            let mut glyphs = Vec::new();
            shaper.shape_with(|cluster| {
                let start = cluster.source.start as usize;
                let preceding = &text[..start.min(text.len())];
                let x = (column + UnicodeWidthStr::width(preceding)) * self.cw;
                let mut advance = 0.0;
                for g in cluster.glyphs {
                    glyphs.push((g.id, x as f32 + advance + g.x, g.y));
                    advance += g.advance;
                }
            });
            let mut scaler = self.scale.builder(font).size(self.px).hint(true).build();
            for (id, x, y) in glyphs {
                let Some(image) =
                    Render::new(&[Source::ColorOutline(0), Source::ColorBitmap(StrikeWith::BestFit), Source::Outline])
                        .render(&mut scaler, id)
                else {
                    continue;
                };
                let left = x.round() as i32 + image.placement.left;
                let top = self.baseline - y.round() as i32 - image.placement.top;
                for gy in 0..image.placement.height as usize {
                    for gx in 0..image.placement.width as usize {
                        let (x, y) = (left + gx as i32, top + gy as i32);
                        if x < 0 || y < 0 || x as usize >= width || y as usize >= self.ch {
                            continue;
                        }
                        let i = gy * image.placement.width as usize + gx;
                        let (color, alpha) = match image.content {
                            Content::Mask => (key.fg, image.data[i]),
                            Content::Color => (
                                (u32::from(image.data[i * 4]) << 16)
                                    | (u32::from(image.data[i * 4 + 1]) << 8)
                                    | u32::from(image.data[i * 4 + 2]),
                                image.data[i * 4 + 3],
                            ),
                            Content::SubpixelMask => (key.fg, image.data[i * 4]),
                        };
                        let dst = &mut pixels[y as usize * width + x as usize];
                        *dst = super::font::blend(*dst, color, alpha);
                    }
                }
            }
        }
        Tile { key, pixels }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fallback_cache_remembers_hits_and_misses() {
        let mut cache = FallbackCache::default();
        let id = fontdb::ID::dummy();
        let count = std::cell::Cell::new(0);
        for _ in 0..3 {
            assert_eq!(
                cache.resolve("界", || {
                    count.set(count.get() + 1);
                    Some(id)
                }),
                Some(id)
            );
            assert_eq!(
                cache.resolve("missing", || {
                    count.set(count.get() + 1);
                    None
                }),
                None
            );
        }
        assert_eq!(count.get(), 2);
    }
}
