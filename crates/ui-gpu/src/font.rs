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

/// Proportional faces for menus, tabs and the status bar, first found wins.
/// Read on the font worker; only the editor font follows editor zoom.
const UI_CANDIDATES: &[(&str, usize)] = &[
    (r"C:\Windows\Fonts\segoeui.ttf", 0),
    ("/System/Library/Fonts/SFNS.ttf", 0),
    ("/System/Library/Fonts/Helvetica.ttc", 0),
    ("/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf", 0),
    ("/usr/share/fonts/noto/NotoSans-Regular.ttf", 0),
    ("/usr/share/fonts/google-noto/NotoSans-Regular.ttf", 0),
    ("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", 0),
    ("/usr/share/fonts/TTF/DejaVuSans.ttf", 0),
    ("/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf", 0),
    ("/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf", 0),
];

/// Wide-coverage face for chrome text the primary face cannot draw (for
/// example Japanese file names). Loaded on a worker only after a miss.
const UI_FALLBACK_CANDIDATES: &[(&str, usize)] = &[
    (r"C:\Windows\Fonts\YuGothR.ttc", 0),
    (r"C:\Windows\Fonts\meiryo.ttc", 0),
    (r"C:\Windows\Fonts\msgothic.ttc", 0),
    (r"C:\Windows\Fonts\msyh.ttc", 0),
    (r"C:\Windows\Fonts\malgun.ttf", 0),
    ("/System/Library/Fonts/Hiragino Sans GB.ttc", 0),
    ("/System/Library/Fonts/PingFang.ttc", 0),
    ("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", 0),
    ("/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc", 0),
    ("/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc", 0),
];

/// Hard bounds of the chrome glyph cache: entries and retained coverage bytes.
const UI_GLYPH_LIMIT: usize = 4096;
const UI_GLYPH_BYTES: usize = 4 << 20;
/// Scalars one text operation may examine. Chrome strings are clipped to a
/// few hundred pixels, so work never follows the length of a notice or name
/// (zero-advance marks included).
const UI_SCALARS: usize = 256;

/// Glyphs every chrome string relies on stay resident through eviction.
fn pinned(c: char) -> bool {
    c.is_ascii() || matches!(c, '…' | '●' | '×')
}

struct UiFace {
    data: Vec<u8>,
    offset: u32,
    key: swash::CacheKey,
}

/// Every face the editor font can start from, as chrome fallbacks.
fn editor_candidates() -> impl Iterator<Item = (&'static str, usize)> {
    CANDIDATES.iter().map(|(path, index, _)| (*path, *index as usize))
}

impl UiFace {
    fn read<'a>(candidates: impl IntoIterator<Item = (&'a str, usize)>) -> Option<UiFace> {
        candidates.into_iter().find_map(|(path, index)| Self::from_data(std::fs::read(path).ok()?, index))
    }
    /// Worker-only chain: proportional faces, then every face the editor font
    /// can start from, then any installed sans or monospace face. Whenever the
    /// editor font loads, chrome text therefore has a face too.
    fn chain() -> Option<UiFace> {
        Self::chain_from(UI_CANDIDATES.iter().copied(), editor_candidates(), Self::system)
    }
    fn chain_from<'a>(
        ui: impl IntoIterator<Item = (&'a str, usize)>,
        editor: impl IntoIterator<Item = (&'a str, usize)>,
        system: impl FnOnce() -> Option<UiFace>,
    ) -> Option<UiFace> {
        Self::read(ui).or_else(|| Self::read(editor)).or_else(system)
    }
    fn system() -> Option<UiFace> {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        [fontdb::Family::SansSerif, fontdb::Family::Monospace].into_iter().find_map(|family| {
            let id = db.query(&fontdb::Query { families: &[family], ..Default::default() })?;
            db.with_face_data(id, |data, index| Self::from_data(data.to_vec(), index as usize)).flatten()
        })
    }
    fn from_data(data: Vec<u8>, index: usize) -> Option<UiFace> {
        let font = swash::FontRef::from_index(&data, index)?;
        let (offset, key) = (font.offset, font.key);
        Some(UiFace { data, offset, key })
    }
    fn font(&self) -> swash::FontRef<'_> {
        swash::FontRef { data: &self.data, offset: self.offset, key: self.key }
    }
}

#[derive(Clone, Debug, Default)]
struct UiGlyph {
    advance: f32,
    left: i32,
    top: i32,
    width: usize,
    height: usize,
    alpha: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fallback {
    Unneeded,
    Wanted,
    Requested,
    Installed,
    Unavailable,
}

/// Chrome text face. Glyphs are coverage masks blended at draw time, so
/// theme changes never retire a cache; sizes come from the monitor scale only.
pub struct UiFont {
    id: u64,
    face: UiFace,
    fallback: Option<UiFace>,
    fallback_state: Fallback,
    pub px: f32,
    pub descent: usize,
    cap: f32,
    glyphs: HashMap<char, UiGlyph>,
    /// Coverage bytes retained by `glyphs`.
    bytes: usize,
    #[cfg(test)]
    rasterized: usize,
}

impl UiFont {
    /// Runs on the font worker: the file is read while the window is created,
    /// then the ASCII set is rasterized once the window reports its scale.
    pub fn startup(px: std::sync::mpsc::Receiver<f32>) -> Option<UiFont> {
        let face = UiFace::chain()?;
        Self::from_face(face, px.recv().ok()?)
    }
    /// Worker-only (reads files).
    pub fn load(px: f32) -> Option<UiFont> {
        Self::from_face(UiFace::chain()?, px)
    }
    fn from_face(face: UiFace, px: f32) -> Option<UiFont> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let px = if px.is_finite() { px.clamp(4.0, 160.0) } else { 14.0 };
        let metrics = face.font().metrics(&[]).scale(px);
        let mut font = UiFont {
            id: NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            face,
            fallback: None,
            fallback_state: Fallback::Unneeded,
            px,
            descent: metrics.descent.abs().ceil() as usize,
            cap: if metrics.cap_height > 0.0 { metrics.cap_height } else { metrics.ascent * 0.7 },
            glyphs: HashMap::new(),
            bytes: 0,
            #[cfg(test)]
            rasterized: 0,
        };
        font.prepare((' '..='~').chain(['…', '●', '×']));
        Some(font)
    }

    /// Identity of this face; fallback results are bound only to the face
    /// that asked for them.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The worker reads one wide-coverage face after a glyph miss.
    pub fn read_fallback() -> Option<(Vec<u8>, usize)> {
        UI_FALLBACK_CANDIDATES.iter().find_map(|(path, index)| {
            let data = std::fs::read(path).ok()?;
            swash::FontRef::from_index(&data, *index)?;
            Some((data, *index))
        })
    }
    /// True once per font when a glyph was missing and no fallback was tried.
    pub fn take_fallback_request(&mut self) -> bool {
        let wanted = self.fallback_state == Fallback::Wanted;
        if wanted {
            self.fallback_state = Fallback::Requested;
        }
        wanted
    }
    pub fn install_fallback(&mut self, face: Option<(Vec<u8>, usize)>) {
        self.fallback = face.and_then(|(data, index)| UiFace::from_data(data, index));
        self.fallback_state = if self.fallback.is_some() { Fallback::Installed } else { Fallback::Unavailable };
        // Glyphs drawn as missing boxes are rasterized again from the new face.
        let charmap = self.face.font().charmap();
        self.glyphs.retain(|c, _| c.is_ascii() || charmap.map(*c) != 0);
        self.bytes = self.glyphs.values().map(|g| g.alpha.len()).sum();
    }

    fn prepare(&mut self, chars: impl Iterator<Item = char>) {
        let resident = self.glyphs.len();
        let missing = admit(&mut self.glyphs, chars, UI_GLYPH_LIMIT, pinned);
        if self.glyphs.len() != resident {
            // Admission evicted entries; account their bytes once.
            self.bytes = self.glyphs.values().map(|g| g.alpha.len()).sum();
        }
        if missing.is_empty() {
            return;
        }
        let mut context = swash::scale::ScaleContext::new();
        let primary = self.face.font();
        for c in missing {
            let mut font = primary;
            let mut id = primary.charmap().map(c);
            if id == 0 && !c.is_control() {
                match (&self.fallback, self.fallback_state) {
                    (Some(fallback), _) if fallback.font().charmap().map(c) != 0 => {
                        font = fallback.font();
                        id = font.charmap().map(c);
                    }
                    (None, Fallback::Unneeded) => self.fallback_state = Fallback::Wanted,
                    _ => {}
                }
            }
            let advance = font.glyph_metrics(&[]).scale(self.px).advance_width(id);
            let mut scaler = context.builder(font).size(self.px).hint(true).build();
            let image = swash::scale::Render::new(&[swash::scale::Source::Outline]).render(&mut scaler, id);
            #[cfg(test)]
            {
                self.rasterized += 1;
            }
            let mut glyph = image.map_or_else(
                || UiGlyph { advance, ..Default::default() },
                |image| {
                    use swash::scale::image::Content;
                    let (width, height) = (image.placement.width as usize, image.placement.height as usize);
                    let alpha = match image.content {
                        Content::Mask => image.data,
                        Content::SubpixelMask => image.data.as_chunks::<4>().0.iter().map(|p| p[0]).collect(),
                        Content::Color => image.data.as_chunks::<4>().0.iter().map(|p| p[3]).collect(),
                    };
                    UiGlyph { advance, left: image.placement.left, top: image.placement.top, width, height, alpha }
                },
            );
            if glyph.alpha.len() != glyph.width * glyph.height {
                glyph = UiGlyph { advance, ..Default::default() };
            }
            if self.bytes + glyph.alpha.len() > UI_GLYPH_BYTES && !pinned(c) {
                self.glyphs.retain(|c, _| pinned(*c));
                self.bytes = self.glyphs.values().map(|g| g.alpha.len()).sum();
                if self.bytes + glyph.alpha.len() > UI_GLYPH_BYTES {
                    // Refused: keep the advance so layout stays stable, draw nothing.
                    glyph = UiGlyph { advance, ..Default::default() };
                }
            }
            self.bytes += glyph.alpha.len();
            self.glyphs.insert(c, glyph);
        }
    }

    fn advance(&mut self, c: char) -> f32 {
        if !self.glyphs.contains_key(&c) {
            self.prepare(std::iter::once(c));
        }
        self.glyphs.get(&c).map_or(0.0, |glyph| glyph.advance)
    }

    /// Width of `text`, at most `cap`. Stops at the cap or after `UI_SCALARS`
    /// scalars, so only what could be visible is ever rasterized.
    pub fn measure(&mut self, text: &str, cap: usize) -> usize {
        let mut used = 0.0;
        for c in text.chars().take(UI_SCALARS) {
            used += self.advance(c);
            if used.ceil() as usize >= cap {
                return cap;
            }
        }
        (used.ceil() as usize).min(cap)
    }

    /// Advance of a short chrome string (labels, fields) in physical pixels.
    pub fn width(&mut self, text: &str) -> usize {
        self.measure(text, usize::MAX)
    }

    /// Fits `text` into `max` pixels as a prefix of `text`, followed by an
    /// ellipsis when it was cut. Never examines more than `UI_SCALARS` scalars.
    pub fn fit(&mut self, text: &str, max: usize) -> String {
        let ellipsis = self.advance('…');
        let (mut used, mut out, mut widths) = (0.0f32, String::new(), Vec::new());
        let mut cut = false;
        for (index, c) in text.chars().enumerate() {
            let advance = if index < UI_SCALARS { self.advance(c) } else { f32::INFINITY };
            if (used + advance).ceil() as usize > max {
                cut = true;
                break;
            }
            used += advance;
            out.push(c);
            widths.push(advance);
        }
        if !cut {
            return out;
        }
        while ((used + ellipsis).ceil() as usize > max || widths.len() >= UI_SCALARS)
            && let Some(advance) = widths.pop()
        {
            used -= advance;
            out.pop();
        }
        if (used + ellipsis).ceil() as usize <= max {
            out.push('…');
        }
        out
    }

    /// Baseline that centres capital letters in a box of `height` at `top`.
    pub fn baseline(&self, top: usize, height: usize) -> usize {
        top + ((height as f32 + self.cap) / 2.0).round() as usize
    }

    /// Blends `text` onto `buf` from pen position `x` on `baseline`, clipped to
    /// `clip`. Glyphs are prepared one at a time and only until the pen leaves
    /// the clip. Returns the pen position after the drawn text.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        buf: &mut [u32],
        stride: usize,
        clip: crate::Rect,
        x: usize,
        baseline: usize,
        text: &str,
        color: u32,
    ) -> usize {
        let (right, bottom) = (clip.x + clip.w, (clip.y + clip.h).min(buf.len() / stride.max(1)));
        let mut pen = x as f32;
        for c in text.chars().take(UI_SCALARS) {
            if pen >= right as f32 {
                break;
            }
            let advance = self.advance(c);
            if let Some(glyph) = self.glyphs.get(&c) {
                let left = pen.round() as i32 + glyph.left;
                let top = baseline as i32 - glyph.top;
                for gy in 0..glyph.height {
                    let y = top + gy as i32;
                    if y < clip.y as i32 || y >= bottom as i32 {
                        continue;
                    }
                    let row = y as usize * stride;
                    for gx in 0..glyph.width {
                        let px = left + gx as i32;
                        let a = glyph.alpha[gy * glyph.width + gx];
                        if a == 0 || px < clip.x as i32 || px >= right.min(stride) as i32 {
                            continue;
                        }
                        let dst = &mut buf[row + px as usize];
                        *dst = blend(*dst, color, a);
                    }
                }
            }
            pen += advance;
        }
        pen.ceil() as usize
    }

    /// Pen offset of the `index`-th character, for mnemonic underlines.
    pub fn offset_of(&mut self, text: &str, index: usize) -> (usize, usize) {
        let before: String = text.chars().take(index).collect();
        let at: String = text.chars().skip(index).take(1).collect();
        (self.width(&before), self.width(&at))
    }
}

/// Characters `required` still needs rasterized, never more than the cache can
/// hold. When admitting would overflow it, unpinned entries are evicted first
/// and the needed set is recomputed, so a cached glyph of this very text is
/// rebuilt; whatever still does not fit is refused, never over-admitted.
fn admit<V>(
    cache: &mut HashMap<char, V>,
    required: impl Iterator<Item = char>,
    limit: usize,
    pinned: impl Fn(char) -> bool,
) -> Vec<char> {
    let mut required: Vec<char> = required.collect();
    required.sort_unstable();
    required.dedup();
    let missing = |cache: &HashMap<char, V>| -> Vec<char> {
        required.iter().copied().filter(|c| !cache.contains_key(c)).collect()
    };
    let first = missing(cache);
    if first.is_empty() || cache.len() + first.len() <= limit {
        return first;
    }
    cache.retain(|c, _| pinned(*c));
    let mut missing = missing(cache);
    missing.truncate(limit.saturating_sub(cache.len()));
    missing
}

/// One worker job at a time, coalescing requests made meanwhile into a single
/// follow-up (the editor font reload uses the same rule).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Coalesce {
    running: bool,
    again: bool,
}

impl Coalesce {
    /// Returns true when the caller must start a job now.
    pub(crate) fn request(&mut self) -> bool {
        if self.running {
            self.again = true;
            return false;
        }
        self.running = true;
        true
    }
    /// The running job finished. Returns true when a newer request is waiting;
    /// the caller then starts exactly one replacement job.
    pub(crate) fn finished(&mut self) -> bool {
        self.running = std::mem::take(&mut self.again);
        self.running
    }
}

/// The wide-coverage face read: at most one worker per App. Each job is
/// fenced to the `UiFont::id` that asked; a result for an older face is never
/// installed into a newer one, which gets one coalesced retry instead.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FallbackJobs {
    running: Option<u64>,
    wanted: Option<u64>,
}

impl FallbackJobs {
    /// Face `id` missed a glyph. Returns the id to start a job for now, if any.
    pub(crate) fn request(&mut self, id: u64) -> Option<u64> {
        if self.running.is_some() {
            self.wanted = Some(id);
            return None;
        }
        self.running = Some(id);
        Some(id)
    }
    /// The job for `id` finished while `current` is the installed face.
    /// Returns whether to bind the result, and a job to start for `current`.
    pub(crate) fn finished(&mut self, id: u64, current: Option<u64>) -> (bool, Option<u64>) {
        self.running = None;
        let bind = current == Some(id);
        let next = self.wanted.take().filter(|wanted| !bind && current == Some(*wanted));
        self.running = next;
        (bind, next)
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
    fn ui_glyph_eviction_rebuilds_cached_glyphs_of_the_current_text() {
        // 'é' is cached; the new text needs it plus enough new glyphs to overflow.
        let mut cache: HashMap<char, ()> = HashMap::new();
        cache.insert('a', ());
        cache.insert('é', ());
        for c in (0x4e00u32..0x4e00 + 6).filter_map(char::from_u32) {
            cache.insert(c, ());
        }
        let text = "aé界\u{3042}\u{3044}";
        let missing = admit(&mut cache, text.chars(), 10, |c: char| c.is_ascii());
        assert!(missing.contains(&'é'), "a cached glyph of this text was evicted and must be rebuilt");
        assert!(missing.contains(&'界') && missing.contains(&'\u{3042}'));
        assert!(!missing.contains(&'a'), "ASCII survives eviction");
        // Without overflow nothing is evicted and only new glyphs are missing.
        let mut cache: HashMap<char, ()> = [('a', ()), ('é', ())].into();
        assert_eq!(admit(&mut cache, "aé界".chars(), 10, |c: char| c.is_ascii()), vec!['界']);
        assert!(cache.contains_key(&'é'));
    }

    #[test]
    fn dpi_font_reloads_run_one_job_and_coalesce_the_rest() {
        let mut jobs = Coalesce::default();
        assert!(jobs.request(), "first change starts a job");
        let started = (0..20).filter(|_| jobs.request()).count();
        assert_eq!(started, 0, "changes during a job never start another thread");
        assert!(jobs.finished(), "exactly one follow-up for the latest scale");
        assert!(!jobs.request(), "the follow-up is still running");
        assert!(jobs.finished(), "the request during the follow-up is honoured once");
        assert!(!jobs.finished(), "then the guard is idle");
        assert!(jobs.request());
    }

    #[test]
    fn ui_face_chain_covers_every_editor_start_face() {
        // A missing proportional face must never leave menus blank while the
        // editor font itself loaded from one of its candidates. This drives the
        // production chain with only its proportional list made unavailable.
        let unavailable = [("Z:/no/such/ui-face.ttf", 0usize)];
        let face = UiFace::chain_from(unavailable, editor_candidates(), || None);
        if CANDIDATES.iter().any(|(path, ..)| std::path::Path::new(path).exists()) {
            let face = face.expect("an existing editor face is a usable chrome face");
            let font = UiFont::from_face(face, 14.0).unwrap();
            assert!(font.glyphs.get(&'M').is_some_and(|g| g.advance > 0.0), "labels have visible glyphs");
            assert!(UiFace::chain().is_some(), "the production chain finds a face on this machine");
        }
        // The system scan is the last link, reached only when both lists fail.
        let mut scanned = false;
        assert!(
            UiFace::chain_from(unavailable, unavailable, || {
                scanned = true;
                None
            })
            .is_none()
        );
        assert!(scanned);
    }

    #[test]
    fn glyph_admission_never_exceeds_the_cache_bound() {
        let mut cache: HashMap<char, ()> = ('a'..='z').map(|c| (c, ())).collect();
        cache.extend(('\u{4e00}'..'\u{4e40}').map(|c| (c, ())));
        let required = ('\u{5000}'..'\u{6000}').chain("abc".chars());
        let missing = admit(&mut cache, required, 100, |c: char| c.is_ascii());
        assert!(cache.len() + missing.len() <= 100, "{} + {}", cache.len(), missing.len());
        assert!(cache.contains_key(&'a'), "pinned glyphs stay resident");
    }

    #[test]
    #[cfg(windows)]
    fn huge_distinct_notice_rasterizes_only_a_visible_prefix_with_a_stable_ellipsis() {
        let mut font = UiFont::load(14.0).expect("Windows chrome face");
        let resident = font.glyphs.len();
        // 6000 distinct scalars: far more than the cache could ever hold.
        let text: String = (0x4e00u32..0x4e00 + 6000).filter_map(char::from_u32).collect();
        font.rasterized = 0;
        let fitted = font.fit(&text, 300);
        assert!(fitted.ends_with('…'));
        let prefix = fitted.trim_end_matches('…');
        assert!(text.starts_with(prefix) && !prefix.is_empty(), "a true prefix of the notice");
        assert!(font.rasterized <= UI_SCALARS, "rasterized {}", font.rasterized);
        assert!(font.measure(&fitted, usize::MAX) <= 300);
        assert!(font.glyphs.len() <= UI_GLYPH_LIMIT && font.bytes <= UI_GLYPH_BYTES);
        assert_eq!(font.fit(&text, 300), fitted, "the same input fits to the same prefix");
        // Zero-advance marks cannot make work follow the input length.
        let marks: String = std::iter::once('a').chain(std::iter::repeat_n('\u{301}', 50_000)).collect();
        font.rasterized = 0;
        let fitted = font.fit(&marks, 300);
        assert!(fitted.chars().count() <= UI_SCALARS && fitted.starts_with('a') && fitted.ends_with('…'));
        assert!(font.rasterized <= 2, "one combining glyph, not 50 000 lookups");
        let mut pixels = vec![0u32; 400 * 40];
        // Drawing the fitted combining-mark string must actually reach the
        // marker, not truncate scalar 257 after fit has promised an ellipsis.
        let clip = crate::Rect { x: 0, y: 0, w: 400, h: 40 };
        let end = font.draw(&mut pixels, 400, clip, 0, 30, &fitted, 0xffffff);
        let prefix = fitted.strip_suffix('…').unwrap();
        let mut prefix_pixels = vec![0u32; 400 * 40];
        let prefix_end = font.draw(&mut prefix_pixels, 400, clip, 0, 30, prefix, 0xffffff);
        assert!(end > prefix_end && pixels != prefix_pixels, "rendered ellipsis must survive the scalar limit");
        font.rasterized = 0;
        font.draw(&mut pixels, 400, crate::Rect { x: 0, y: 0, w: 120, h: 40 }, 0, 30, &text, 0xffffff);
        assert!(font.rasterized <= UI_SCALARS, "drawing stops at the clip");
        assert!(font.glyphs.len() >= resident, "pinned glyphs were never evicted");
        assert!(font.glyphs.contains_key(&'…') && font.glyphs.contains_key(&'M'));
    }

    #[test]
    fn fallback_reads_stay_single_and_never_bind_a_newer_face() {
        let mut jobs = FallbackJobs::default();
        assert_eq!(jobs.request(1), Some(1), "face 1 starts the only read");
        // Two DPI reloads complete and both new faces miss glyphs meanwhile.
        assert_eq!(jobs.request(2), None);
        assert_eq!(jobs.request(3), None);
        // The read for face 1 lands while face 3 is installed.
        assert_eq!(jobs.finished(1, Some(3)), (false, Some(3)), "stale result is dropped; one retry for face 3");
        assert_eq!(jobs.request(3), None, "still one read at a time");
        assert_eq!(jobs.finished(3, Some(3)), (true, None), "the current face binds its own result");
        // A result for a face that is no longer installed starts nothing new.
        assert_eq!(jobs.request(4), Some(4));
        assert_eq!(jobs.finished(4, Some(5)), (false, None));
        assert_eq!(jobs.request(5), Some(5), "the guard is free again");
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
