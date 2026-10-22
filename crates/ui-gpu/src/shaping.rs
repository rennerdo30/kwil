//! Deferred Unicode shaping/rasterization. This module is used only by a worker
//! after the CPU first frame. Font discovery and glyph fallback never block input.
use std::collections::HashMap;
use swash::{
    CacheKey, FontRef,
    scale::{Render, ScaleContext, Source, StrikeWith, image::Content},
    shape::{Direction, ShapeContext},
    text::{
        Codepoint, JoiningType, Script,
        cluster::{CharCluster, Parser, Token},
    },
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const MAX_FONTS: usize = 16;
const MAX_FONT_BYTES: usize = 64 * 1024 * 1024;

pub(crate) fn needs_bidi(text: &str) -> bool {
    text.chars().any(|c| c.bidi_class().needs_resolution())
}
struct DirectionalRun<'a> {
    text: &'a str,
    direction: Direction,
}
fn directional_runs(text: &str) -> Vec<DirectionalRun<'_>> {
    let bidi = unicode_bidi::ParagraphBidiInfo::new(text, None);
    let (levels, ranges) = bidi.visual_runs(0..text.len());
    ranges
        .into_iter()
        .map(|range| DirectionalRun {
            text: &text[range.clone()],
            direction: if levels[range.start].is_rtl() { Direction::RightToLeft } else { Direction::LeftToRight },
        })
        .collect()
}
struct ColumnMap {
    boundaries: Vec<(usize, usize)>,
    columns: usize,
}
impl ColumnMap {
    fn new(text: &str) -> Self {
        let mut boundaries = vec![(0, 0)];
        let mut columns = 0;
        for (start, g) in text.grapheme_indices(true) {
            columns += UnicodeWidthStr::width(g).max(1);
            boundaries.push((start + g.len(), columns));
        }
        Self { boundaries, columns }
    }
    fn before(&self, offset: usize) -> usize {
        self.boundaries[self.boundaries.partition_point(|b| b.0 <= offset).saturating_sub(1)].1
    }
    fn origin(&self, start: usize, end: usize, direction: Direction) -> usize {
        if direction == Direction::RightToLeft { self.columns - self.before(end) } else { self.before(start) }
    }
}
struct ShapeRun {
    text: String,
    id: Option<fontdb::ID>,
    script: Script,
    direction: Direction,
    columns: usize,
}
struct ShapedCluster {
    start: usize,
    end: usize,
    advance: f32,
    glyphs: Vec<swash::shape::cluster::Glyph>,
}
fn place_cursive(
    clusters: &[ShapedCluster],
    direction: Direction,
    left: f32,
    width: f32,
) -> (Vec<(swash::GlyphId, f32, f32)>, f32) {
    let total = clusters.iter().map(|c| c.advance).sum::<f32>();
    let scale = if total > 0.0 { width / total } else { 1.0 };
    let mut before = 0.0;
    let mut glyphs = Vec::new();
    for c in clusters {
        let advance = c.advance;
        let origin = if direction == Direction::RightToLeft { total - before - advance } else { before };
        let mut within = 0.0;
        for g in &c.glyphs {
            glyphs.push((g.id, left + (origin + within + g.x) * scale, g.y));
            within += g.advance;
        }
        before += advance;
    }
    (glyphs, scale)
}

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
        // Resolve UAX9 levels first. Itemize each directional run by font/script,
        // then reverse RTL *items* and anchor complete shaped clusters visually.
        // Swash intentionally returns RTL clusters in logical order.
        let mut runs = Vec::new();
        for directional in directional_runs(&key.text) {
            let mut items: Vec<ShapeRun> = Vec::new();
            for g in directional.text.graphemes(true) {
                let id = self.face_for(g);
                let script = g
                    .chars()
                    .map(|c| c.script())
                    .find(|s| !matches!(s, Script::Common | Script::Inherited))
                    .unwrap_or(Script::Latin);
                let n = UnicodeWidthStr::width(g).max(1);
                if let Some(last) = items.last_mut().filter(|r| r.id == id && r.script == script) {
                    last.text.push_str(g);
                    last.columns += n;
                } else {
                    items.push(ShapeRun { text: g.into(), id, script, direction: directional.direction, columns: n });
                }
            }
            if directional.direction == Direction::RightToLeft {
                items.reverse();
            }
            runs.extend(items);
        }
        let mut column = 0;
        for run in runs {
            let ShapeRun { text, id, script, direction, columns } = run;
            let left_column = column;
            column += columns;
            let Some(id) = id else {
                // Visible missing-font box; never silently discard source text.
                let left = left_column * self.cw;
                let right = (column * self.cw).min(width);
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
            let mut shaper = self.shape.builder(font).script(script).direction(direction).size(self.px).build();
            // Mirror punctuation in RTL while keeping original source offsets.
            let mut parser = Parser::new(
                script,
                text.char_indices().map(|(i, ch)| Token {
                    ch: if direction == Direction::RightToLeft { ch.mirror().unwrap_or(ch) } else { ch },
                    offset: i as u32,
                    len: ch.len_utf8() as u8,
                    info: ch.properties().into(),
                    data: 0,
                }),
            );
            let mut cluster = CharCluster::new();
            while parser.next(&mut cluster) {
                cluster.map(|ch| font.charmap().map(ch));
                shaper.add_cluster(&cluster);
            }
            let mut clusters = Vec::new();
            shaper.shape_with(|cluster| {
                clusters.push(ShapedCluster {
                    start: cluster.source.start as usize,
                    end: cluster.source.end as usize,
                    advance: cluster.advance(),
                    glyphs: cluster.glyphs.to_vec(),
                });
            });
            let cursive = text.chars().any(|c| {
                matches!(
                    c.joining_type(),
                    JoiningType::L | JoiningType::R | JoiningType::D | JoiningType::Alaph | JoiningType::DalathRish
                )
            });
            let (glyphs, scale_x) = if cursive {
                // Keep joining advances intact across the whole font/script item.
                // Independently snapping each cluster to a cell breaks Arabic
                // connections when fallback supplies a proportional font.
                place_cursive(&clusters, direction, (left_column * self.cw) as f32, (columns * self.cw) as f32)
            } else {
                let positions = ColumnMap::new(&text);
                let mut glyphs = Vec::new();
                for cluster in &clusters {
                    let x = ((left_column + positions.origin(cluster.start, cluster.end, direction)) * self.cw) as f32;
                    let mut advance = 0.0;
                    for g in &cluster.glyphs {
                        glyphs.push((g.id, x + advance + g.x, g.y));
                        advance += g.advance;
                    }
                }
                (glyphs, 1.0)
            };
            let mut scaler = self.scale.builder(font).size(self.px).hint(true).build();
            for (id, x, y) in glyphs {
                let Some(image) =
                    Render::new(&[Source::ColorOutline(0), Source::ColorBitmap(StrikeWith::BestFit), Source::Outline])
                        .transform((scale_x != 1.0).then(|| swash::zeno::Transform::scale(scale_x, 1.0)))
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
    fn cursive_positions_preserve_joins_and_mark_offsets() {
        let glyph = |id, advance, x, y| swash::shape::cluster::Glyph { id, advance, x, y, ..Default::default() };
        let clusters = vec![
            ShapedCluster {
                start: 0,
                end: 2,
                advance: 5.0,
                glyphs: vec![glyph(1, 5.0, 0.0, 0.0), glyph(2, 0.0, -2.0, 3.0)],
            },
            ShapedCluster { start: 2, end: 4, advance: 9.0, glyphs: vec![glyph(3, 9.0, 0.0, 0.0)] },
            ShapedCluster { start: 4, end: 6, advance: 6.0, glyphs: vec![glyph(4, 6.0, 0.0, 0.0)] },
        ];
        let (placed, scale) = place_cursive(&clusters, Direction::RightToLeft, 0.0, 80.0);
        assert_eq!(scale, 4.0);
        assert_eq!(placed, vec![(1, 60.0, 0.0), (2, 72.0, 3.0), (3, 24.0, 0.0), (4, 0.0, 0.0)]);
        // Unequal natural advances still abut exactly after fitting the run.
        assert_eq!(placed[3].1 + clusters[2].advance * scale, placed[2].1);
        assert_eq!(placed[2].1 + clusters[1].advance * scale, placed[0].1);
        assert_eq!(placed[0].1 + clusters[0].advance * scale, 80.0);
    }
    fn visual_clusters(text: &str) -> String {
        let mut result = String::new();
        for run in directional_runs(text) {
            let map = ColumnMap::new(run.text);
            let mut clusters = run
                .text
                .grapheme_indices(true)
                .map(|(start, g)| (map.origin(start, start + g.len(), run.direction), g))
                .collect::<Vec<_>>();
            clusters.sort_by_key(|c| c.0);
            for (_, g) in clusters {
                result.push_str(g);
            }
        }
        result
    }
    #[test]
    fn hebrew_arabic_and_marks_are_in_visual_cluster_order() {
        assert_eq!(visual_clusters("אבג"), "גבא");
        assert_eq!(visual_clusters("مرحبا"), "ابحرم");
        assert_eq!(visual_clusters("א\u{05b7}ב"), "בא\u{05b7}");
        assert!(directional_runs("مرحبا").iter().all(|r| r.direction == Direction::RightToLeft));
    }
    #[test]
    fn mixed_latin_rtl_and_digits_keep_directional_runs() {
        assert!(needs_bidi("abc אבג 12"));
        assert!(!needs_bidi("abc 日本語 e\u{301}"));
        assert_eq!(visual_clusters("abc אבג 12"), "abc 12 גבא");
    }
    #[test]
    fn rtl_ligature_anchors_use_full_cluster_end() {
        let map = ColumnMap::new("سلاهم");
        assert_eq!(map.origin(2, 6, Direction::RightToLeft), 2);
        assert_eq!(map.origin(0, 2, Direction::RightToLeft), 4);
        assert_eq!(map.origin(8, 10, Direction::RightToLeft), 0);
    }
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
