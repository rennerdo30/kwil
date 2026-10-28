//! Bounded, frontend-independent Markdown preview. See the crate README for edit integration.

pub mod bounded;
pub mod image_decode;

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use std::{ops::Range, sync::Arc};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub const MAX_PREVIEW_BYTES: usize = 20 * 1024 * 1024;
pub const MAX_MODEL_ITEMS: usize = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewError {
    TooLargeForPreview,
    InvalidRange,
    ModelLimit,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Style {
    pub emphasis: bool,
    pub strong: bool,
    pub strikethrough: bool,
    pub link: Option<Arc<str>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InlineKind {
    Text(String),
    Code(String),
    SoftBreak,
    HardBreak,
    Image { alt: String, url: String },
}

/// A flat run. Styles compose (e.g. strong emphasis inside a link).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub kind: InlineKind,
    pub style: Style,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alignment {
    None,
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListMarker {
    Bullet(char),
    Ordered { number: u64, delimiter: char },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockKind {
    Heading(u8),
    Paragraph,
    ListItem {
        marker: ListMarker,
        task: Option<bool>,
    },
    CodeBlock {
        language: Option<String>,
        raw: String,
    },
    /// First row is the header. Cells retain inline styles.
    Table {
        alignments: Vec<Alignment>,
        rows: Vec<Vec<Vec<Span>>>,
    },
    ThematicBreak,
    Image {
        alt: String,
        url: String,
    },
    Html(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub kind: BlockKind,
    pub spans: Vec<Span>,
    /// Zero-based, half-open document coordinates. Byte ranges begin at the
    /// source line start (including quote/list markers); ends follow CommonMark offsets.
    pub source_lines: Range<usize>,
    pub source_bytes: Range<usize>,
    pub quote_depth: usize,
    /// Zero outside a list, one for an outer item, two for its child, etc.
    pub list_depth: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    pub blocks: Vec<Block>,
    pub source_bytes: Range<usize>,
}

pub fn parse(document: &str) -> Result<Model, PreviewError> {
    parse_section(document, 0..document.len())
}

fn options() -> Options {
    Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS | Options::ENABLE_STRIKETHROUGH
}

fn admit(document: &str) -> Result<(), PreviewError> {
    if document.len() > MAX_PREVIEW_BYTES { Err(PreviewError::TooLargeForPreview) } else { Ok(()) }
}

// Sparse counts, rather than one offset per source line. Each lookup scans <= 4096 bytes.
struct Lines<'a> {
    bytes: &'a [u8],
    checkpoints: Vec<usize>,
}
impl<'a> Lines<'a> {
    fn new(text: &'a str) -> Self {
        let mut result = Self { bytes: text.as_bytes(), checkpoints: vec![0] };
        let mut line = 0;
        for i in 0..result.bytes.len() {
            line += usize::from(result.is_break(i));
            if (i + 1) % 4096 == 0 {
                result.checkpoints.push(line);
            }
        }
        result
    }
    fn is_break(&self, i: usize) -> bool {
        self.bytes[i] == b'\n' || (self.bytes[i] == b'\r' && self.bytes.get(i + 1) != Some(&b'\n'))
    }
    fn at(&self, byte: usize) -> usize {
        let chunk = byte / 4096;
        self.checkpoints[chunk] + (chunk * 4096..byte).filter(|&i| self.is_break(i)).count()
    }
    fn range(&self, bytes: &Range<usize>) -> Range<usize> {
        self.at(bytes.start)..self.at(bytes.end.saturating_sub(1).max(bytes.start)) + 1
    }
}

struct Builder<'a> {
    source: &'a str,
    origin: usize,
    lines: Lines<'a>,
    blocks: Vec<Block>,
    active: Vec<usize>,
    lists: Vec<Option<u64>>,
    items: Vec<usize>,
    quotes: usize,
    quote_starts: Vec<(usize, Range<usize>)>,
    styles: Vec<Style>,
    style: Style,
    table: Option<usize>,
    image: Option<(String, String, usize)>,
}

impl Builder<'_> {
    fn block(&mut self, kind: BlockKind, range: Range<usize>) -> usize {
        let start = range.start + self.origin;
        let start = self.lines.bytes[..start].iter().rposition(|&b| b == b'\n' || b == b'\r').map_or(0, |i| i + 1);
        let bytes = start..range.end + self.origin;
        let index = self.blocks.len();
        self.blocks.push(Block {
            kind,
            spans: vec![],
            source_lines: self.lines.range(&bytes),
            source_bytes: bytes,
            quote_depth: self.quotes,
            list_depth: self.lists.len(),
        });
        index
    }
    fn push(&mut self, kind: BlockKind, range: Range<usize>) {
        let index = self.block(kind, range);
        self.active.push(index);
    }
    fn span(&mut self, kind: InlineKind) {
        let span = Span { kind, style: self.style.clone() };
        if let Some(i) = self.table {
            if let BlockKind::Table { rows, .. } = &mut self.blocks[i].kind
                && let Some(cell) = rows.last_mut().and_then(|r| r.last_mut())
            {
                cell.push(span);
            }
        } else if let Some(&i) = self.active.last() {
            self.blocks[i].spans.push(span);
        }
    }
    fn text(&mut self, text: &str) {
        if let Some((_, alt, _)) = &mut self.image {
            alt.push_str(text);
            return;
        }
        if let Some(&i) = self.active.last() {
            match &mut self.blocks[i].kind {
                BlockKind::CodeBlock { raw, .. } | BlockKind::Html(raw) => {
                    raw.push_str(text);
                    return;
                }
                _ => {}
            }
        }
        self.span(InlineKind::Text(text.into()));
    }
    fn start(&mut self, tag: Tag<'_>, range: Range<usize>) {
        match tag {
            Tag::Paragraph => {
                if let Some(&i) = self.items.last().filter(|&&i| {
                    self.blocks[i].spans.is_empty()
                        && i + 1 == self.blocks.len()
                        && self.blocks[i].quote_depth == self.quotes
                        && self.active.last() == Some(&i)
                }) {
                    self.active.push(i);
                } else {
                    self.push(BlockKind::Paragraph, range);
                }
            }
            Tag::Heading { level, .. } => self.push(BlockKind::Heading(level as u8), range),
            Tag::BlockQuote(_) => {
                self.quote_starts.push((self.blocks.len(), range));
                self.quotes += 1;
            }
            Tag::List(number) => self.lists.push(number),
            Tag::Item => {
                let raw = self.source[range.clone()].trim_start();
                let marker = if let Some(number) = self.lists.last_mut().and_then(Option::as_mut) {
                    let n = *number;
                    *number = number.saturating_add(1);
                    let delimiter = raw.chars().find(|c| !c.is_ascii_digit()).unwrap_or('.');
                    ListMarker::Ordered { number: n, delimiter }
                } else {
                    ListMarker::Bullet(raw.chars().next().unwrap_or('-'))
                };
                self.push(BlockKind::ListItem { marker, task: None }, range);
                self.items.push(*self.active.last().unwrap());
            }
            Tag::CodeBlock(kind) => {
                let language = match kind {
                    CodeBlockKind::Indented => None,
                    CodeBlockKind::Fenced(s) => s.split_whitespace().next().map(str::to_owned),
                };
                self.push(BlockKind::CodeBlock { language, raw: String::new() }, range);
            }
            Tag::HtmlBlock => self.push(BlockKind::Html(String::new()), range),
            Tag::Table(alignments) => {
                self.push(
                    BlockKind::Table {
                        alignments: alignments
                            .into_iter()
                            .map(|a| match a {
                                pulldown_cmark::Alignment::None => Alignment::None,
                                pulldown_cmark::Alignment::Left => Alignment::Left,
                                pulldown_cmark::Alignment::Center => Alignment::Center,
                                pulldown_cmark::Alignment::Right => Alignment::Right,
                            })
                            .collect(),
                        rows: vec![],
                    },
                    range,
                );
                self.table = self.active.last().copied();
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(i) = self.table
                    && let BlockKind::Table { rows, .. } = &mut self.blocks[i].kind
                {
                    rows.push(vec![]);
                }
            }
            Tag::TableCell => {
                if let Some(i) = self.table
                    && let BlockKind::Table { rows, .. } = &mut self.blocks[i].kind
                {
                    rows.last_mut().unwrap().push(vec![]);
                }
            }
            Tag::Emphasis | Tag::Strong | Tag::Strikethrough | Tag::Link { .. } => {
                self.styles.push(self.style.clone());
                match tag {
                    Tag::Emphasis => self.style.emphasis = true,
                    Tag::Strong => self.style.strong = true,
                    Tag::Strikethrough => self.style.strikethrough = true,
                    Tag::Link { dest_url, .. } => self.style.link = Some(Arc::from(dest_url.as_ref())),
                    _ => unreachable!(),
                }
            }
            Tag::Image { dest_url, .. } => {
                if let Some((_, _, depth)) = &mut self.image {
                    *depth += 1;
                } else {
                    self.image = Some((dest_url.into_string(), String::new(), 1));
                }
            }
            _ => {} // Extensions not enabled in options().
        }
    }
    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::Heading(_) => {
                let i = self.active.pop().unwrap();
                if self.blocks[i].kind == BlockKind::Paragraph
                    && self.blocks[i].spans.len() == 1
                    && self.blocks[i].spans[0].style == Style::default()
                    && let InlineKind::Image { alt, url } = &self.blocks[i].spans[0].kind
                {
                    self.blocks[i].kind = BlockKind::Image { alt: alt.clone(), url: url.clone() };
                    self.blocks[i].spans.clear();
                }
            }
            TagEnd::CodeBlock | TagEnd::HtmlBlock => {
                self.active.pop();
            }
            TagEnd::Table => {
                self.active.pop();
                self.table = None;
            }
            TagEnd::BlockQuote(_) => {
                let (count, range) = self.quote_starts.pop().unwrap();
                if count == self.blocks.len() {
                    self.block(BlockKind::Paragraph, range);
                }
                self.quotes -= 1;
            }
            TagEnd::List(_) => {
                self.lists.pop();
            }
            TagEnd::Item => {
                self.items.pop();
                self.active.pop();
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                self.style = self.styles.pop().unwrap();
            }
            TagEnd::Image => {
                let (url, alt, depth) = self.image.take().unwrap();
                if depth == 1 {
                    self.span(InlineKind::Image { alt, url });
                } else {
                    self.image = Some((url, alt, depth - 1));
                }
            }
            _ => {}
        }
    }
}

/// Parse a complete block-aligned section, without copying the input document.
/// References resolve within the section. Use `block_boundaries` to handle global definitions.
pub fn parse_section(document: &str, bytes: Range<usize>) -> Result<Model, PreviewError> {
    parse_section_with_limit(document, bytes, MAX_MODEL_ITEMS)
}

fn parse_section_with_limit(document: &str, bytes: Range<usize>, event_limit: usize) -> Result<Model, PreviewError> {
    admit(document)?;
    let source = document.get(bytes.clone()).ok_or(PreviewError::InvalidRange)?;
    let mut b = Builder {
        source,
        origin: bytes.start,
        lines: Lines::new(document),
        blocks: vec![],
        active: vec![],
        lists: vec![],
        items: vec![],
        quotes: 0,
        quote_starts: vec![],
        styles: vec![],
        style: Style::default(),
        table: None,
        image: None,
    };
    for (count, (event, range)) in Parser::new_ext(source, options()).into_offset_iter().enumerate() {
        if count >= event_limit {
            return Err(PreviewError::ModelLimit);
        }
        match event {
            Event::Start(tag) => b.start(tag, range),
            Event::End(tag) => b.end(tag),
            Event::Text(s) | Event::Html(s) | Event::InlineHtml(s) => b.text(&s),
            Event::Code(s) => {
                if b.image.is_some() {
                    b.text(&s);
                } else {
                    b.span(InlineKind::Code(s.into_string()));
                }
            }
            Event::SoftBreak | Event::HardBreak if b.image.is_some() => b.text(" "),
            Event::SoftBreak => b.span(InlineKind::SoftBreak),
            Event::HardBreak => b.span(InlineKind::HardBreak),
            Event::Rule => {
                b.block(BlockKind::ThematicBreak, range);
            }
            Event::TaskListMarker(checked) => {
                if let Some(&i) = b.items.last()
                    && let BlockKind::ListItem { task, .. } = &mut b.blocks[i].kind
                {
                    *task = Some(checked);
                }
            }
            _ => {}
        }
    }
    Ok(Model { blocks: b.blocks, source_bytes: bytes })
}

/// Discover enclosing top-level blocks plus one neighbour on each side in the
/// **new** document. This is an O(n) parser scan, but allocates no render spans.
/// Reference definitions conservatively select the entire document.
pub fn block_boundaries(document: &str, changed_lines: Range<usize>) -> Result<Range<usize>, PreviewError> {
    admit(document)?;
    let lines = Lines::new(document);
    let count = lines.at(document.len()) + 1;
    if changed_lines.start > changed_lines.end || changed_lines.start >= count || changed_lines.end > count {
        return Err(PreviewError::InvalidRange);
    }
    let parser = Parser::new_ext(document, options());
    if parser.reference_definitions().iter().next().is_some() {
        return Ok(0..document.len());
    }
    let mut roots: Vec<Range<usize>> = vec![];
    let mut depth = 0;
    for (count, (event, range)) in parser.into_offset_iter().enumerate() {
        if count >= MAX_MODEL_ITEMS {
            return Err(PreviewError::ModelLimit);
        }
        match event {
            Event::Start(_) => {
                if depth == 0 {
                    roots.push(range);
                }
                depth += 1;
            }
            Event::End(_) => depth -= 1,
            Event::Rule if depth == 0 => roots.push(range),
            _ => {}
        }
    }
    if roots.is_empty() {
        return Ok(0..document.len());
    }
    let end = changed_lines.end.max(changed_lines.start + 1);
    let first = roots.iter().position(|r| lines.range(r).end > changed_lines.start).unwrap_or(roots.len() - 1);
    let last = roots.iter().rposition(|r| lines.range(r).start < end).unwrap_or(first).max(first);
    Ok(roots[first.saturating_sub(1)].start..roots[(last + 1).min(roots.len() - 1)].end)
}

fn inline_text(spans: &[Span]) -> String {
    let mut out = String::new();
    for span in spans {
        match &span.kind {
            InlineKind::Text(s) | InlineKind::Code(s) => out.push_str(s),
            InlineKind::SoftBreak => out.push(' '),
            InlineKind::HardBreak => out.push('\n'),
            InlineKind::Image { alt, url } => {
                out.push_str(&format!("![{alt}]({url})"));
            }
        }
    }
    out
}

/// Plain text fallback, wrapped by grapheme clusters and terminal display cells.
/// Width zero is treated as one. Overwide clusters occupy a line on their own.
pub fn render_text(model: &Model, width: usize) -> String {
    let width = width.max(1);
    let mut out = String::new();
    for block in &model.blocks {
        if !out.is_empty() {
            out.push('\n');
        }
        let quote = "> ".repeat(block.quote_depth.min(width));
        let indent = "  ".repeat(block.list_depth.saturating_sub(1).min(width));
        let mut prefix = format!("{quote}{indent}");
        let mut raw = false;
        let body = match &block.kind {
            BlockKind::Heading(n) => {
                prefix.push_str(&"#".repeat((*n).into()));
                prefix.push(' ');
                inline_text(&block.spans)
            }
            BlockKind::Paragraph => {
                if block.list_depth > 0 {
                    prefix.push_str("  ");
                }
                inline_text(&block.spans)
            }
            BlockKind::ListItem { marker, task } => {
                match marker {
                    ListMarker::Bullet(c) => prefix.push_str(&format!("{c} ")),
                    ListMarker::Ordered { number, delimiter } => prefix.push_str(&format!("{number}{delimiter} ")),
                }
                if let Some(done) = task {
                    prefix.push_str(if *done { "[x] " } else { "[ ] " });
                }
                inline_text(&block.spans)
            }
            BlockKind::CodeBlock { language, raw: text } => {
                raw = true;
                format!(
                    "```{}\n{}```",
                    language.as_deref().unwrap_or(""),
                    if text.ends_with('\n') { text.clone() } else { format!("{text}\n") }
                )
            }
            BlockKind::Table { rows, .. } => rows
                .iter()
                .map(|r| format!("| {} |", r.iter().map(|c| inline_text(c)).collect::<Vec<_>>().join(" | ")))
                .collect::<Vec<_>>()
                .join("\n"),
            BlockKind::ThematicBreak => "---".into(),
            BlockKind::Image { alt, url } => format!("![{alt}]({url})"),
            BlockKind::Html(text) => {
                raw = true;
                text.trim_end_matches('\n').into()
            }
        };
        let continuation = if matches!(block.kind, BlockKind::Heading(_) | BlockKind::ListItem { .. }) {
            format!("{quote}{}", " ".repeat(prefix.width() - quote.width()))
        } else {
            prefix.clone()
        };
        wrap(&mut out, &body, &prefix, &continuation, width, raw);
    }
    out
}

fn wrap(out: &mut String, text: &str, first: &str, continuation: &str, width: usize, raw: bool) {
    let mut prefix = first;
    for line in text.split('\n') {
        let mut pending = String::new();
        let mut used = 0;
        let units: Vec<&str> = if raw { vec![line] } else { line.split_whitespace().collect() };
        let mut capacity = width.saturating_sub(prefix.width().min(width - 1));
        for word in units {
            let length = word.width();
            if !pending.is_empty() && used + 1 + length > capacity {
                write_line(out, prefix, &pending, width);
                prefix = continuation;
                pending.clear();
                used = 0;
            }
            if !pending.is_empty() {
                pending.push(' ');
                used += 1;
            }
            for cluster in word.graphemes(true) {
                let cells = cluster.width();
                if !pending.is_empty() && used + cells > capacity {
                    write_line(out, prefix, &pending, width);
                    prefix = continuation;
                    pending.clear();
                    used = 0;
                }
                if pending.is_empty() {
                    // Shorten the prefix to leave room for this whole cluster.
                    // If it exceeds the entire width, omit the prefix entirely.
                    let budget = width.saturating_sub(cells.max(1));
                    capacity = width - prefix_cells(prefix, budget).width();
                }
                pending.push_str(cluster);
                used += cells;
            }
        }
        write_line(out, prefix, &pending, width);
        prefix = continuation;
    }
}

fn write_line(out: &mut String, prefix: &str, text: &str, width: usize) {
    out.push_str(prefix_cells(prefix, width.saturating_sub(text.width().max(1))));
    out.push_str(text);
    out.push('\n');
}

fn prefix_cells(prefix: &str, budget: usize) -> &str {
    let mut used = 0;
    let mut end = 0;
    for (offset, cluster) in prefix.grapheme_indices(true) {
        let cells = cluster.width();
        if used + cells > budget {
            break;
        }
        used += cells;
        end = offset + cluster.len();
    }
    &prefix[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragmented_markup_stops_at_event_limit() {
        let input = "*one* **two**";
        assert_eq!(parse_section_with_limit(input, 0..input.len(), 3), Err(PreviewError::ModelLimit));
        assert!(parse_section_with_limit(input, 0..input.len(), 20).is_ok());
    }
}

pub mod diagram;
