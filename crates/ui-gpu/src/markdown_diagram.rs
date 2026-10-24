//! Native Mermaid drawing; no windows, timers, workers or external renderers.
//!
//! ROOT hook: export core `pub mod diagram`, add UI `mod markdown_diagram`.
//! On the existing Markdown worker, for CodeBlock { language: Some("mermaid"),
//! raw }, call diagram::parse(raw), retaining raw even on success. Cache results
//! by block/revision; bound aggregate diagram admission to the Markdown model
//! budget (suggest at most 32 diagrams / 64 KiB total Diagram::text_bytes()).
//! On width/font/DPI changes call prepare for admitted blocks encountered by Flow,
//! cache Layout by revision/block/width/ui.scale/font.id(). Use layout.placement()
//! to reserve its rows even when skipped; it gives the signed draw origin and
//! next Flow line/y in constant time. Stop traversal at Flow::done(). Draw only
//! when that virtual rectangle intersects the damaged visible preview. Pass its
//! signed, scroll-adjusted origin and the preview/damage intersection as clip.
//! Reuse layout on scroll/theme changes; invalidate on resize/DPI/font replacement.
//! On parse OR prepare error render notice(error) followed by the ORIGINAL fenced
//! code via Flow::text(raw, ..., true). Never call a failed layout diagram-success.
//! UI text is literal and proportional; colours use desktop's theme hierarchy.

use crate::{Rect, chrome, desktop, font};
use kwil_core_commands::theme::Theme;
use kwil_core_markdown::diagram::{Diagram, Direction, Kind, Shape, Stroke};
use std::fmt;

const MAX_GEOMETRY: usize = 1_000_000;
const MAX_LABEL_LINES: usize = 3;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Error {
    InvalidGeometry,
    TooNarrow { required: usize, available: usize },
    LabelTooWide,
    StaleFont,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidGeometry => f.write_str("diagram geometry exceeds the bounded native canvas"),
            Self::TooNarrow { required, available } => {
                write!(f, "diagram needs {required} pixels; pane has {available}")
            }
            Self::LabelTooWide => f.write_str("diagram label needs more than three native text lines"),
            Self::StaleFont => f.write_str("diagram layout needs rebuilding for the current font"),
        }
    }
}
pub(super) fn notice(error: &impl fmt::Display) -> String {
    format!("Mermaid preview unavailable: {error}. Original fenced code follows.")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Point {
    x: i64,
    y: i64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Text {
    rect: Rect,
    lines: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Node {
    rect: Rect,
    shape: Shape,
    text: Text,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Edge {
    points: Vec<Point>,
    text: Text,
    stroke: Stroke,
}

/// Small immutable vector scene, never a diagram-sized pixel buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Layout {
    width: usize,
    height: usize,
    font: u64,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    lifelines: Vec<(Point, Point)>,
    line: usize,
    arrow: usize,
}
impl Layout {
    pub fn height(&self) -> usize {
        self.height
    }
    /// Exactly equivalent to reserving ceil(height / row_height) Flow::next()
    /// calls, including when the viewport starts halfway through the diagram.
    #[cfg(test)]
    pub fn placement(&self, line: usize, skip: usize, y: usize, row_height: usize) -> Result<Placement, Error> {
        if row_height == 0 {
            return Err(Error::InvalidGeometry);
        }
        let rows = self.height.div_ceil(row_height);
        let next_line = line.checked_add(rows).ok_or(Error::InvalidGeometry)?;
        let skipped = skip.saturating_sub(line);
        let offset = skipped.checked_mul(row_height).ok_or(Error::InvalidGeometry)?;
        let origin_y = i64::try_from(y)
            .map_err(|_| Error::InvalidGeometry)?
            .checked_sub(i64::try_from(offset).map_err(|_| Error::InvalidGeometry)?)
            .ok_or(Error::InvalidGeometry)?;
        let advance = next_line.saturating_sub(skip).min(rows).checked_mul(row_height).ok_or(Error::InvalidGeometry)?;
        let next_y = y.checked_add(advance).ok_or(Error::InvalidGeometry)?;
        Ok(Placement { origin_y, next_line, next_y, after_skip: next_line > skip })
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Placement {
    pub origin_y: i64,
    pub next_line: usize,
    pub next_y: usize,
    /// False means the whole diagram is before the scroll boundary: do not draw.
    /// True still requires intersection with the visible preview/damage rectangle.
    pub after_skip: bool,
}

#[derive(Clone, Copy)]
struct Sizes {
    pad: usize,
    gap: usize,
    text: usize,
    line: usize,
    arrow: usize,
    label_width: usize,
}
impl Sizes {
    fn new(ui: &chrome::Metrics) -> Self {
        Self {
            pad: ui.dip(12.0),
            gap: ui.dip(24.0),
            text: ui.dip(22.0),
            line: ui.dip(1.0),
            arrow: ui.dip(6.0),
            label_width: ui.dip(180.0),
        }
    }
}

/// UI-font metrics only; no file reads. Do this once per admitted cache key.
pub(super) fn prepare(
    diagram: &Diagram,
    width: usize,
    font: &mut font::UiFont,
    ui: &chrome::Metrics,
) -> Result<Layout, Error> {
    let id = font.id();
    build(diagram, width, Sizes::new(ui), id, &mut |text| font.width(text))
}

fn wrapped(text: &str, s: Sizes, measure: &mut impl FnMut(&str) -> usize) -> Result<(Vec<String>, usize), Error> {
    let mut lines = vec![String::new()];
    let mut width = 0;
    // Parser caps each label at 256 scalars. Character wrapping retains every
    // scalar; unlike fit/ellipsis this never promises a complete truncated label.
    for c in text.chars() {
        let count = lines.len();
        let line = lines.last_mut().ok_or(Error::InvalidGeometry)?;
        let mut candidate = line.clone();
        candidate.push(c);
        let w = measure(&candidate);
        if w > s.label_width {
            if line.is_empty() || count >= MAX_LABEL_LINES {
                return Err(Error::LabelTooWide);
            }
            let split = line
                .char_indices()
                .rev()
                .find(|&(at, c)| at > 0 && c.is_whitespace())
                .map(|(at, c)| at + c.len_utf8())
                .filter(|&at| at < line.len());
            let mut next = if let Some(at) = split { line.split_off(at) } else { String::new() };
            next.push(c);
            lines.push(next);
            if measure(lines.last().unwrap()) > s.label_width {
                return Err(Error::LabelTooWide);
            }
        } else {
            *line = candidate;
        }
    }
    for line in &lines {
        width = width.max(measure(line));
    }
    Ok((lines, width))
}

fn build(
    diagram: &Diagram,
    width: usize,
    mut s: Sizes,
    font: u64,
    measure: &mut impl FnMut(&str) -> usize,
) -> Result<Layout, Error> {
    if width == 0
        || width > MAX_GEOMETRY
        || [s.pad, s.gap, s.text, s.line, s.arrow, s.label_width].iter().any(|&size| size == 0 || size > MAX_GEOMETRY)
    {
        return Err(Error::InvalidGeometry);
    }
    if width < 4 * s.pad {
        return Err(Error::TooNarrow { required: 4 * s.pad, available: width });
    }
    // Fit labels before allocating geometry. Horizontal graphs must reserve
    // every rank; sequence self messages also need a readable return lane.
    // Preserve direction and full labels: never rotate, elide or crop a graph.
    let slots = match diagram.kind() {
        Kind::Flow(Direction::LR | Direction::RL) => ranks(diagram).iter().max().copied().unwrap_or(0) + 1,
        Kind::Sequence => diagram.nodes().len() + 1,
        _ => 1,
    };
    if slots > 1 {
        let horizontal = matches!(diagram.kind(), Kind::Flow(Direction::LR | Direction::RL));
        let columns = if horizontal { slots + 3 } else { slots };
        let label = (width.saturating_sub(2 * s.pad) / columns).saturating_sub(2 * s.pad);
        s.label_width = s.label_width.min(label.max(1));
    }
    let mut nodes = Vec::new();
    for node in diagram.nodes() {
        let (lines, w) = wrapped(&node.label, s, measure)?;
        let (mut w, mut h) = (w + 2 * s.pad, lines.len() * s.text + 2 * s.pad);
        if node.shape == Shape::Decision {
            w *= 2;
            h *= 2;
        }
        nodes.push(Node {
            rect: Rect { x: 0, y: 0, w: w.max(4 * s.pad), h },
            shape: node.shape,
            text: Text { rect: Rect::default(), lines },
        });
    }
    let mut edges = Vec::new();
    for edge in diagram.edges() {
        let (lines, w) = wrapped(&edge.label, s, measure)?;
        edges.push(Edge {
            points: Vec::new(),
            text: Text { rect: Rect { x: 0, y: 0, w, h: lines.len() * s.text }, lines },
            stroke: edge.stroke,
        });
    }
    let mut scene =
        Layout { width, height: 0, font, nodes, edges, lifelines: Vec::new(), line: s.line, arrow: s.arrow };
    match diagram.kind() {
        Kind::Flow(direction) => flow(diagram, &mut scene, direction, s)?,
        Kind::Sequence => sequence(diagram, &mut scene, s)?,
    }
    for node in &mut scene.nodes {
        let inset = if node.shape == Shape::Decision { node.rect.w / 4 } else { s.pad };
        node.text.rect = Rect {
            x: node.rect.x + inset,
            y: node.rect.y + (node.rect.h - node.text.lines.len() * s.text) / 2,
            w: node.rect.w - 2 * inset,
            h: node.text.lines.len() * s.text,
        };
    }
    validate(&scene)?;
    Ok(scene)
}

/// Reachability collapses strongly connected components. A cycle is rendered in
/// one rank, with routed return/self arrows, never dropped or iterated to convergence.
fn ranks(diagram: &Diagram) -> Vec<usize> {
    let n = diagram.nodes().len();
    let mut reach = [[false; 64]; 64];
    for (i, row) in reach.iter_mut().enumerate().take(n) {
        row[i] = true;
    }
    for edge in diagram.edges() {
        reach[edge.from][edge.to] = true;
    }
    for k in 0..n {
        for i in 0..n {
            for j in 0..n {
                let via = reach[i][k] && reach[k][j];
                if via {
                    reach[i][j] = true;
                }
            }
        }
    }
    let component: Vec<_> = (0..n).map(|i| (0..=i).find(|&j| reach[i][j] && reach[j][i]).unwrap_or(i)).collect();
    let mut rank = vec![0; n];
    for _ in 0..n {
        for edge in diagram.edges() {
            let (a, b) = (component[edge.from], component[edge.to]);
            if a != b {
                rank[b] = rank[b].max(rank[a] + 1);
            }
        }
    }
    component.iter().map(|&c| rank[c]).collect()
}

fn flow(diagram: &Diagram, scene: &mut Layout, direction: Direction, s: Sizes) -> Result<(), Error> {
    let horizontal = matches!(direction, Direction::LR | Direction::RL);
    let reversed = matches!(direction, Direction::RL | Direction::BT);
    let ranks = ranks(diagram);
    let layers = ranks.iter().copied().max().unwrap_or(0) + 1;
    let cross = scene.nodes.iter().map(|n| if horizontal { n.rect.h } else { n.rect.w }).max().unwrap_or(1);
    let mut primary = vec![0; layers];
    let mut counts = vec![0; layers];
    let mut cross_centers = Vec::new();
    for (index, node) in scene.nodes.iter().enumerate() {
        let rank = ranks[index];
        primary[rank] = primary[rank].max(if horizontal { node.rect.w } else { node.rect.h });
        cross_centers.push(s.pad + counts[rank] * (cross + s.gap) + cross / 2);
        counts[rank] += 1;
    }
    let widest = counts.iter().copied().max().unwrap_or(1);
    let cross_body = widest * cross + widest.saturating_sub(1) * s.gap;
    let mut outgoing = vec![0usize; layers];
    let mut route_lanes = Vec::new();
    // Labels at distinct target rows/columns share a primary lane. Overlapping
    // intervals (including duplicate arrows) require separate lanes. This packs
    // branches without spending the longest label width for every edge.
    type LabelLane = (usize, Vec<(usize, usize)>);
    let mut label_lanes: Vec<Vec<LabelLane>> = vec![Vec::new(); layers];
    let mut label_slots = vec![None; diagram.edges().len()];
    for (index, (edge, draw)) in diagram.edges().iter().zip(&scene.edges).enumerate() {
        let rank = ranks[edge.from];
        route_lanes.push(outgoing[rank]);
        outgoing[rank] += 1;
        if ranks[edge.to] != rank + 1 || draw.text.rect.w == 0 {
            continue;
        }
        let along = if horizontal { draw.text.rect.w } else { draw.text.rect.h };
        let across = if horizontal { draw.text.rect.h } else { draw.text.rect.w };
        let center = cross_centers[edge.to];
        let interval = (center.saturating_sub(across / 2 + s.pad / 2), center + across.div_ceil(2) + s.pad / 2);
        let lanes = &mut label_lanes[rank];
        let slot = lanes
            .iter()
            .position(|(_, intervals)| intervals.iter().all(|&(lo, hi)| interval.1 <= lo || hi <= interval.0))
            .unwrap_or(lanes.len());
        if slot == lanes.len() {
            lanes.push((0, Vec::new()));
        }
        lanes[slot].0 = lanes[slot].0.max(along);
        lanes[slot].1.push(interval);
        label_slots[index] = Some(slot);
    }
    let gaps: Vec<_> = (0..layers)
        .map(|rank| {
            (outgoing[rank] + 1) * s.pad + label_lanes[rank].iter().map(|(size, _)| size + s.pad).sum::<usize>()
        })
        .collect();
    let outside = diagram.edges().iter().filter(|e| ranks[e.to] != ranks[e.from] + 1).count();
    let cross_label = scene
        .edges
        .iter()
        .filter(|e| e.text.rect.w != 0)
        .map(|e| if horizontal { e.text.rect.h } else { e.text.rect.w })
        .max()
        .unwrap_or(0);
    let outside_step = cross_label + s.pad;
    let cross_extent = 2 * s.pad + cross_body + if outside == 0 { 0 } else { s.gap + outside * outside_step };
    let margin = if outside == 0 { 2 * s.pad } else { s.pad + gaps.iter().copied().max().unwrap_or(s.pad) };
    let primary_extent = 2 * margin + primary.iter().sum::<usize>() + gaps[..layers - 1].iter().sum::<usize>();
    let (natural_width, height) =
        if horizontal { (primary_extent, cross_extent) } else { (cross_extent, primary_extent) };
    if natural_width > scene.width {
        return Err(Error::TooNarrow { required: natural_width, available: scene.width });
    }
    scene.height = height;
    let point = |p: usize, c: usize| {
        let p = if reversed { primary_extent - p } else { p };
        if horizontal { Point { x: p as i64, y: c as i64 } } else { Point { x: c as i64, y: p as i64 } }
    };
    let mut centers = Vec::new();
    for (index, node) in scene.nodes.iter_mut().enumerate() {
        let rank = ranks[index];
        let p = margin + primary[..rank].iter().sum::<usize>() + gaps[..rank].iter().sum::<usize>() + primary[rank] / 2;
        let c = cross_centers[index];
        let center = point(p, c);
        node.rect.x = center.x as usize - node.rect.w / 2;
        node.rect.y = center.y as usize - node.rect.h / 2;
        centers.push((p, c));
    }
    let mut outside_index = 0;
    for (i, (edge, draw)) in diagram.edges().iter().zip(&mut scene.edges).enumerate() {
        let (ap, ac) = centers[edge.from];
        let (bp, bc) = centers[edge.to];
        let rank = ranks[edge.from];
        let a = &scene.nodes[edge.from].rect;
        let b = &scene.nodes[edge.to].rect;
        let ah = if horizontal { a.w / 2 } else { a.h / 2 };
        let bh = if horizontal { b.w / 2 } else { b.h / 2 };
        let exit = ap + primary[rank] / 2 + (route_lanes[i] + 1) * s.pad;
        let enter = bp - primary[ranks[edge.to]] / 2 - s.pad;
        draw.points.push(point(ap + ah, ac));
        draw.points.push(point(exit, ac));
        let label_center = if ranks[edge.to] == rank + 1 {
            draw.points.push(point(exit, bc));
            let offset = label_slots[i].map_or(0, |slot| {
                (outgoing[rank] + 1) * s.pad
                    + label_lanes[rank][..slot].iter().map(|(size, _)| size + s.pad).sum::<usize>()
                    + label_lanes[rank][slot].0 / 2
            });
            point(ap + primary[rank] / 2 + offset, bc)
        } else {
            let side = s.pad + cross_body + s.gap + outside_index * outside_step;
            outside_index += 1;
            draw.points.push(point(exit, side));
            draw.points.push(point(enter, side));
            draw.points.push(point(enter, bc));
            point((exit + enter) / 2, side + cross_label / 2)
        };
        draw.points.push(point(bp - bh, bc));
        draw.text.rect.x = (label_center.x as usize).saturating_sub(draw.text.rect.w / 2);
        draw.text.rect.y = (label_center.y as usize).saturating_sub(draw.text.rect.h / 2);
    }
    Ok(())
}
fn sequence(diagram: &Diagram, scene: &mut Layout, s: Sizes) -> Result<(), Error> {
    let nw = scene.nodes.iter().map(|n| n.rect.w).max().unwrap_or(1);
    let nh = scene.nodes.iter().map(|n| n.rect.h).max().unwrap_or(1);
    let ew = scene.edges.iter().map(|e| e.text.rect.w).max().unwrap_or(0);
    let self_width = diagram
        .edges()
        .iter()
        .zip(&scene.edges)
        .filter(|(edge, _)| edge.from == edge.to)
        .map(|(_, draw)| draw.text.rect.w)
        .max()
        .unwrap_or(0);
    let step = (nw + s.gap).max(ew + 2 * s.pad).max(self_width + s.gap + s.pad);
    let base = 2 * s.pad + scene.nodes.len() * step;
    // Earlier participants' self messages fit inside their existing column.
    // Only a return beyond the final column needs additional canvas width.
    let natural = diagram
        .edges()
        .iter()
        .zip(&scene.edges)
        .filter(|(edge, _)| edge.from == edge.to)
        .map(|(edge, draw)| s.pad + edge.from * step + step / 2 + draw.text.rect.w + s.gap + s.pad)
        .max()
        .unwrap_or(0)
        .max(base);
    if natural > scene.width {
        return Err(Error::TooNarrow { required: natural, available: scene.width });
    }
    let mut y = s.pad + nh + s.gap;
    for (i, node) in scene.nodes.iter_mut().enumerate() {
        node.rect.x = s.pad + i * step + (step - node.rect.w) / 2;
        node.rect.y = s.pad;
    }
    for (edge, draw) in diagram.edges().iter().zip(&mut scene.edges) {
        let a = (s.pad + edge.from * step + step / 2) as i64;
        let b = (s.pad + edge.to * step + step / 2) as i64;
        draw.text.rect.y = y;
        y += draw.text.rect.h + s.pad;
        if a == b {
            // A self message belongs wholly to its participant. Using the
            // longest message in the scene made this leg touch the next lifeline.
            let right = a + (draw.text.rect.w + s.gap) as i64;
            draw.points = vec![
                Point { x: a, y: y as i64 },
                Point { x: right, y: y as i64 },
                Point { x: right, y: (y + s.gap) as i64 },
                Point { x: a, y: (y + s.gap) as i64 },
            ];
            draw.text.rect.x = a as usize + s.pad;
            y += s.gap;
        } else {
            draw.points = vec![Point { x: a, y: y as i64 }, Point { x: b, y: y as i64 }];
            draw.text.rect.x = ((a + b) as usize / 2).saturating_sub(draw.text.rect.w / 2);
        }
        y += 2 * s.gap;
    }
    scene.height = y + s.pad;
    for i in 0..scene.nodes.len() {
        let x = (s.pad + i * step + step / 2) as i64;
        scene.lifelines.push((Point { x, y: (s.pad + nh) as i64 }, Point { x, y: y as i64 }));
    }
    Ok(())
}

fn validate(scene: &Layout) -> Result<(), Error> {
    if scene.width > MAX_GEOMETRY || scene.height == 0 || scene.height > MAX_GEOMETRY {
        return Err(Error::InvalidGeometry);
    }
    let rect = |r: Rect| {
        r.x.checked_add(r.w).is_some_and(|x| x <= scene.width)
            && r.y.checked_add(r.h).is_some_and(|y| y <= scene.height)
    };
    let point = |p: Point| p.x >= 0 && p.y >= 0 && p.x < scene.width as i64 && p.y < scene.height as i64;
    if scene.nodes.iter().any(|n| !rect(n.rect) || !rect(n.text.rect))
        || scene.edges.iter().any(|e| {
            !rect(e.text.rect)
                || e.points.len() < 2
                || e.points.iter().any(|&p| !point(p))
                || e.points.windows(2).any(|p| p[0].x != p[1].x && p[0].y != p[1].y)
        })
        || scene.lifelines.iter().any(|&(a, b)| !point(a) || !point(b))
    {
        return Err(Error::InvalidGeometry);
    }
    Ok(())
}

fn intersection(a: Rect, b: Rect) -> Rect {
    let x = a.x.max(b.x);
    let y = a.y.max(b.y);
    Rect {
        x,
        y,
        w: a.x.saturating_add(a.w).min(b.x.saturating_add(b.w)).saturating_sub(x),
        h: a.y.saturating_add(a.h).min(b.y.saturating_add(b.h)).saturating_sub(y),
    }
}

/// Bounds every primitive to BOTH the damage clip and actual framebuffer.
struct Canvas<'a> {
    pixels: &'a mut [u32],
    stride: usize,
    clip: Rect,
    origin: Point,
}
impl Canvas<'_> {
    fn point(&self, p: Point) -> Point {
        Point { x: p.x + self.origin.x, y: p.y + self.origin.y }
    }
    fn pixel(&mut self, x: i64, y: i64, color: u32) {
        if x >= self.clip.x as i64
            && y >= self.clip.y as i64
            && x < (self.clip.x + self.clip.w) as i64
            && y < (self.clip.y + self.clip.h) as i64
        {
            self.pixels[y as usize * self.stride + x as usize] = color;
        }
    }
    fn fill(&mut self, r: Rect, color: u32) {
        let a = self.point(Point { x: r.x as i64, y: r.y as i64 });
        let x = a.x.max(self.clip.x as i64);
        let y = a.y.max(self.clip.y as i64);
        let right = (a.x + r.w as i64).min((self.clip.x + self.clip.w) as i64);
        let bottom = (a.y + r.h as i64).min((self.clip.y + self.clip.h) as i64);
        if x >= right || y >= bottom {
            return;
        }
        for yy in y as usize..bottom as usize {
            self.pixels[yy * self.stride + x as usize..yy * self.stride + right as usize].fill(color);
        }
    }
    /// All layout routes are axis aligned. Bound traversal to the visible clip,
    /// so scrolling past a million-pixel virtual path does not walk that path.
    fn line(&mut self, a: Point, b: Point, color: u32, dashed: bool, thickness: usize) {
        let (a, b) = (self.point(a), self.point(b));
        if a.x == b.x {
            if a.x >= (self.clip.x + self.clip.w) as i64 || a.x + thickness as i64 <= self.clip.x as i64 {
                return;
            }
            let lo = a.y.min(b.y).max(self.clip.y as i64);
            let hi = a.y.max(b.y).min((self.clip.y + self.clip.h) as i64 - 1);
            for y in lo..=hi {
                if !dashed || (y - a.y.min(b.y)) / 5 % 2 == 0 {
                    for dx in 0..thickness {
                        self.pixel(a.x + dx as i64, y, color);
                    }
                }
            }
        } else if a.y == b.y {
            if a.y >= (self.clip.y + self.clip.h) as i64 || a.y + thickness as i64 <= self.clip.y as i64 {
                return;
            }
            let lo = a.x.min(b.x).max(self.clip.x as i64);
            let hi = a.x.max(b.x).min((self.clip.x + self.clip.w) as i64 - 1);
            for x in lo..=hi {
                if !dashed || (x - a.x.min(b.x)) / 5 % 2 == 0 {
                    for dy in 0..thickness {
                        self.pixel(x, a.y + dy as i64, color);
                    }
                }
            }
        }
    }
    fn arrow(&mut self, a: Point, b: Point, color: u32, size: usize) {
        let (dx, dy) = ((b.x - a.x).signum(), (b.y - a.y).signum());
        let end = self.point(b);
        for back in 0..size as i64 {
            for across in -back / 2..=back / 2 {
                self.pixel(end.x - dx * back - dy * across, end.y - dy * back + dx * across, color);
            }
        }
    }
    fn node(&mut self, node: &Node, c: &desktop::Colors, thickness: usize) {
        let r = node.rect;
        let center = self.point(Point { x: (r.x + r.w / 2) as i64, y: (r.y + r.h / 2) as i64 });
        let top = self.point(Point { x: r.x as i64, y: r.y as i64 });
        // Scan only visible rows and spans. Diamonds and rounded boxes are
        // actually rasterized as their shapes, rather than a text approximation.
        let first = top.y.max(self.clip.y as i64);
        let last = (top.y + r.h as i64).min((self.clip.y + self.clip.h) as i64);
        for y in first..last {
            let local = (y - top.y) as usize;
            let inset = match node.shape {
                Shape::Box => 0,
                Shape::Rounded => {
                    let radius = (r.h / 5).min(r.w / 5);
                    let d = local.min(r.h - 1 - local);
                    radius.saturating_sub(d)
                }
                Shape::Decision => {
                    let distance = (y - center.y).unsigned_abs() as usize;
                    distance * (r.w / 2) / (r.h / 2).max(1)
                }
            };
            let left = top.x + inset.min(r.w / 2) as i64;
            let right = top.x + r.w as i64 - inset.min(r.w / 2) as i64;
            for x in left.max(self.clip.x as i64)..right.min((self.clip.x + self.clip.w) as i64) {
                let border = local < thickness
                    || local + thickness >= r.h
                    || x < left + thickness as i64
                    || x + thickness as i64 >= right;
                self.pixel(x, y, if border { c.accent } else { c.control });
            }
        }
    }
    fn text(&mut self, text: &Text, font: &mut font::UiFont, color: u32, background: Option<u32>) {
        if text.rect.w == 0 {
            return;
        }
        if let Some(bg) = background {
            self.fill(text.rect, bg);
        }
        let line_height = text.rect.h / text.lines.len().max(1);
        for (i, line) in text.lines.iter().enumerate() {
            let top = self.point(Point { x: text.rect.x as i64, y: (text.rect.y + i * line_height) as i64 });
            // UiFont uses unsigned baselines. Lines wholly above zero are
            // invisible; a partially visible line can still have a valid baseline.
            let baseline = top.y + font.baseline(0, line_height) as i64;
            if top.x < 0
                || baseline < 0
                || top.y + line_height as i64 <= self.clip.y as i64
                || top.y >= (self.clip.y + self.clip.h) as i64
            {
                continue;
            }
            let x = top.x as usize;
            let clip = intersection(
                self.clip,
                Rect {
                    x,
                    y: top.y.max(0) as usize,
                    w: text.rect.w,
                    h: (top.y + line_height as i64).max(0) as usize - top.y.max(0) as usize,
                },
            );
            if clip.w != 0 && clip.h != 0 {
                font.draw(self.pixels, self.stride, clip, x, baseline as usize, line, color);
            }
        }
    }
}

/// Caller schedules damage; this draws once and never requests another frame.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw(
    layout: &Layout,
    pixels: &mut [u32],
    stride: usize,
    origin: (i64, i64),
    clip: Rect,
    font: &mut font::UiFont,
    theme: &Theme,
) -> Result<(), Error> {
    if layout.font != font.id() {
        return Err(Error::StaleFont);
    }
    if stride == 0 || stride > i32::MAX as usize {
        return Err(Error::InvalidGeometry);
    }
    let safe = i32::MAX as i64 - MAX_GEOMETRY as i64;
    if origin.0 < -safe || origin.1 < -safe || origin.0 > safe || origin.1 > safe {
        return Err(Error::InvalidGeometry);
    }
    let clip = intersection(clip, Rect { x: 0, y: 0, w: stride, h: (pixels.len() / stride).min(safe as usize) });
    if clip.w == 0 || clip.h == 0 {
        return Ok(());
    }
    let c = desktop::Colors::new(theme);
    let mut canvas = Canvas { pixels, stride, clip, origin: Point { x: origin.0, y: origin.1 } };
    canvas.fill(Rect { x: 0, y: 0, w: layout.width, h: layout.height }, c.sidebar);
    for &(a, b) in &layout.lifelines {
        canvas.line(a, b, c.faint, true, layout.line);
    }
    for edge in &layout.edges {
        for segment in edge.points.windows(2) {
            canvas.line(segment[0], segment[1], c.muted, edge.stroke == Stroke::Dashed, layout.line);
        }
        // Zero-length final legs occur when two nodes share a centerline.
        if let Some(&end) = edge.points.last()
            && let Some(&before) = edge.points.iter().rev().skip(1).find(|&&p| p != end)
        {
            canvas.arrow(before, end, c.accent, layout.arrow);
        }
    }
    for edge in &layout.edges {
        canvas.text(&edge.text, font, c.fg, Some(c.sidebar));
    }
    for node in &layout.nodes {
        canvas.node(node, &c, layout.line);
        canvas.text(&node.text, font, c.fg, None);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kwil_core_markdown::diagram::parse;
    fn layout(source: &str, width: usize) -> Result<Layout, Error> {
        build(&parse(source).unwrap(), width, Sizes::new(&chrome::Metrics::new(1.0)), 1, &mut |s| s.chars().count() * 7)
    }
    #[test]
    fn deterministic_cycles_self_edges_and_duplicate_routes_are_not_lost() {
        let source = "flowchart TD; A[Start]-->B{Ready?}; B-- no -->A; B-->B; A-->B";
        let a = layout(source, 1600).unwrap();
        assert_eq!(a, layout(source, 1600).unwrap());
        assert_eq!(a.nodes.len(), 2);
        assert_eq!(a.edges.len(), 4);
        assert!(a.edges.iter().all(|e| e.points.len() >= 4));
        assert_ne!(a.edges[0].points, a.edges[3].points);
        validate(&a).unwrap();
        let b = layout("graph TD; A--yes-->B; B--no-->A; A--again-->A", 1600).unwrap();
        for (i, edge) in b.edges.iter().enumerate() {
            for other in &b.edges[i + 1..] {
                let overlap = intersection(edge.text.rect, other.text.rect);
                assert!(overlap.w == 0 || overlap.h == 0, "return labels need distinct lanes");
            }
        }
    }
    #[test]
    fn all_directions_preserve_arrows_and_mirror_node_order() {
        let mut scenes = Vec::new();
        for direction in ["TD", "TB", "LR", "RL", "BT"] {
            let a = layout(&format!("graph {direction}; A-->B-->C"), 4096).unwrap();
            assert_eq!(a.edges.len(), 2);
            assert!(a.edges.iter().all(|e| e.points.windows(2).all(|p| p[0].x == p[1].x || p[0].y == p[1].y)));
            validate(&a).unwrap();
            scenes.push(a);
        }
        assert!(scenes[0].nodes[0].rect.y < scenes[0].nodes[2].rect.y);
        assert!(scenes[2].nodes[0].rect.x < scenes[2].nodes[2].rect.x);
        assert!(scenes[3].nodes[0].rect.x > scenes[3].nodes[2].rect.x);
        assert!(scenes[4].nodes[0].rect.y > scenes[4].nodes[2].rect.y);
    }
    #[test]
    fn sequence_message_order_dashes_labels_and_self_loop_geometry() {
        let a = layout(
            "sequenceDiagram; participant A as Alice; participant B as Bob; A->>B: Hello; B-->>A: Hi; A->>A: Think",
            1200,
        )
        .unwrap();
        assert_eq!(a.lifelines.len(), 2);
        assert_eq!(a.edges.len(), 3);
        assert_eq!(a.edges[1].stroke, Stroke::Dashed);
        assert_eq!(a.edges[2].points.len(), 4);
        assert!(a.edges[0].points[0].y < a.edges[1].points[0].y);
        assert_eq!(a.edges[0].text.lines, ["Hello"]);
        validate(&a).unwrap();
    }
    #[test]
    fn self_message_returns_before_the_neighbouring_lifeline() {
        let a = layout("sequenceDiagram; participant Editor; participant Worker; Editor->>Worker: Immutable snapshot; Worker-->>Editor: Versioned render model; Editor->>Editor: Reject stale result", 600).unwrap();
        let next_lifeline = a.lifelines[1].0.x;
        let self_message = &a.edges[2];
        assert!(
            self_message.points.iter().all(|point| point.x < next_lifeline),
            "Editor self message must not look like a Worker interaction"
        );
        assert_eq!(self_message.points.first().unwrap().x, self_message.points.last().unwrap().x);
        assert_eq!(self_message.text.lines.concat(), "Reject stale result");
        validate(&a).unwrap();
    }
    #[test]
    fn showcase_flow_and_sequence_fit_a_split_pane_without_lost_labels() {
        let source = "flowchart LR; A[Open file] --> B{Preview enabled?}; B -->|Yes| C[Parse on worker]; B -->|No| D[Text editor]; C --> E[Native preview]";
        let a = layout(source, 600).unwrap();
        assert_eq!(a.nodes.len(), 5);
        assert_eq!(a.edges.len(), 4);
        assert_eq!(a.nodes[1].text.lines.concat(), "Preview enabled?");
        let sequence = "sequenceDiagram; participant Editor; participant Worker; Editor->>Worker: Immutable snapshot; Worker-->>Editor: Versioned render model; Editor->>Editor: Reject stale result";
        let b = layout(sequence, 600).unwrap();
        assert_eq!(b.edges.len(), 3);
        assert_eq!(b.edges[1].text.lines.concat(), "Versioned render model");
        validate(&a).unwrap();
        validate(&b).unwrap();
        for source in [source, sequence] {
            let diagram = parse(source).unwrap();
            let scaled = build(&diagram, 900, Sizes::new(&chrome::Metrics::new(1.5)), 1, &mut |text| {
                (text.chars().count() as f64 * 10.5).ceil() as usize
            })
            .unwrap();
            validate(&scaled).unwrap();
            for (node, original) in scaled.nodes.iter().zip(diagram.nodes()) {
                assert_eq!(node.text.lines.concat(), original.label);
            }
            for (edge, original) in scaled.edges.iter().zip(diagram.edges()) {
                assert_eq!(edge.text.lines.concat(), original.label);
            }
        }
    }
    #[test]
    fn admission_rejects_bad_geometry_narrow_panes_and_unrenderable_labels() {
        assert_eq!(layout("graph TD; A", 0), Err(Error::InvalidGeometry));
        assert_eq!(layout("graph TD; A", usize::MAX), Err(Error::InvalidGeometry));
        assert!(matches!(layout("graph LR; A-->B", 20), Err(Error::TooNarrow { .. })));
        let source = format!("graph TD; A[{}]", "x".repeat(200));
        assert_eq!(layout(&source, 4096), Err(Error::LabelTooWide));
    }
    #[test]
    fn dense_graph_preserves_content_within_canvas_and_oversized_sizes_are_rejected() {
        let mut source = String::from("graph TD;");
        for i in 0..63 {
            source.push_str(&format!("N{i}-->N{};", i + 1));
        }
        for _ in 0..65 {
            source.push_str(&format!("N0--{}-->N63;", "x".repeat(60)));
        }
        let diagram = parse(&source).unwrap();
        let result =
            build(&diagram, MAX_GEOMETRY, Sizes::new(&chrome::Metrics::new(8.0)), 1, &mut |s| s.chars().count() * 56)
                .unwrap();
        assert_eq!(result.nodes.len(), diagram.nodes().len());
        assert_eq!(result.edges.len(), diagram.edges().len());
        assert!(result.width <= MAX_GEOMETRY && result.height <= MAX_GEOMETRY);
        validate(&result).unwrap();
        for (node, original) in result.nodes.iter().zip(diagram.nodes()) {
            assert_eq!(node.text.lines.concat(), original.label);
        }
        for (edge, original) in result.edges.iter().zip(diagram.edges()) {
            assert_eq!(edge.text.lines.concat(), original.label);
        }
        let oversized = Sizes { pad: MAX_GEOMETRY + 1, ..Sizes::new(&chrome::Metrics::new(1.0)) };
        let mut measured = false;
        assert_eq!(
            build(&diagram, MAX_GEOMETRY, oversized, 1, &mut |_| {
                measured = true;
                1
            }),
            Err(Error::InvalidGeometry)
        );
        assert!(!measured, "reject oversized metrics before text or scene allocation");
    }
    #[test]
    fn clipped_primitives_touch_only_visible_pixels_with_signed_scroll_origin() {
        let mut pixels = vec![0xabcdef; 12 * 10 + 3];
        let clip = Rect { x: 3, y: 2, w: 4, h: 5 };
        let mut c = Canvas { pixels: &mut pixels, stride: 12, clip, origin: Point { x: -10, y: -20 } };
        c.fill(Rect { x: 0, y: 0, w: 100, h: 100 }, 1);
        c.line(Point { x: 15, y: 0 }, Point { x: 15, y: 1_000_000 }, 2, false, 2);
        c.line(Point { x: 0, y: 24 }, Point { x: 1_000_000, y: 24 }, 3, true, 1);
        c.arrow(Point { x: 10, y: 24 }, Point { x: 15, y: 24 }, 4, 6);
        for (i, p) in pixels.iter().enumerate() {
            if i < 120 && clip.contains((i % 12, i / 12)) {
                assert_ne!(*p, 0xabcdef);
            } else {
                assert_eq!(*p, 0xabcdef, "outside clip pixel {i}");
            }
        }
    }
    #[test]
    fn dpi_changes_layout_and_resizing_rechecks_admission() {
        let diagram = parse("graph TD; A-->B").unwrap();
        let make = |scale, width| {
            build(&diagram, width, Sizes::new(&chrome::Metrics::new(scale)), 1, &mut |s| {
                (s.len() as f64 * 7.0 * scale) as usize
            })
        };
        let a = make(1.0, 1000).unwrap();
        let b = make(2.0, 2000).unwrap();
        assert_eq!(b.height(), 2 * a.height());
        assert!(matches!(make(2.0, 20), Err(Error::TooNarrow { .. })));
    }
    #[test]
    fn flow_reservation_matches_next_and_partially_scrolled_origin() {
        let a = layout("graph TD; A-->B", 1000).unwrap();
        let rows = a.height().div_ceil(25);
        for (line, skip) in [(0, 0), (0, 3), (0, rows + 10), (12, 5), (12, 15)] {
            let p = a.placement(line, skip, 40, 25).unwrap();
            let (mut next, mut y) = (line, 40);
            for _ in 0..rows {
                next += 1;
                if next > skip {
                    y += 25;
                }
            }
            assert_eq!((p.next_line, p.next_y), (next, y));
            assert_eq!(p.origin_y, 40 - (skip.saturating_sub(line) * 25) as i64);
            assert_eq!(p.after_skip, next > skip);
        }
        assert_eq!(a.placement(0, 0, 0, 0), Err(Error::InvalidGeometry));
        assert_eq!(a.placement(usize::MAX, 0, 0, 25), Err(Error::InvalidGeometry));
    }
}
