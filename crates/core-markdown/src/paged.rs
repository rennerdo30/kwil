//! Worker-owned preview pages. The source is streamed once into a disk block
//! index; scrolling reads and parses only a bounded neighbourhood. No source,
//! per-line table or document-sized render model is retained here.
use crate::{Block, BlockKind, Model, bounded::PREVIEW_SOURCE_BYTES};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    ops::Range,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

const CHUNK: usize = 64 * 1024;
const RECORD_BYTES: u64 = 40;
const MAX_RECORDS: u64 = 128;
static NEXT: AtomicU64 = AtomicU64::new(0);
type ReadSource<'a> = dyn FnMut(Range<u64>, &mut Vec<u8>) -> Result<(), String> + 'a;

#[derive(Clone, Copy, Default)]
struct Record {
    start: u64,
    end: u64,
    line: u64,
    end_line: u64,
    safe_end: bool,
}
impl Record {
    fn write(self, out: &mut impl Write) -> Result<(), String> {
        for n in [self.start, self.end, self.line, self.end_line, u64::from(self.safe_end)] {
            out.write_all(&n.to_le_bytes()).map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

pub struct Page {
    pub model: Model,
    pub total_lines: u64,
    /// Bytes passed to CommonMark, independent of document size. Zero for the
    /// explicit source fallback of an indivisible block larger than admission.
    pub parsed_bytes: usize,
}

/// Initial layout estimate, replaced by the frontend's measured block height.
/// This touches fixed metadata only, never source or the rest of the document.
pub fn estimated_rows(block: &Block, columns: usize) -> usize {
    block.source_lines.len().max(block.source_bytes.len().div_ceil(columns.max(1))).saturating_add(2)
}

pub struct Pager {
    // Close before unlinking (Windows). This object is created/dropped off UI.
    file: Option<File>,
    path: PathBuf,
    count: u64,
    total_lines: u64,
    definitions: String,
}
impl Drop for Pager {
    fn drop(&mut self) {
        drop(self.file.take());
        let _ = std::fs::remove_file(&self.path);
    }
}
impl Pager {
    pub fn build(len: u64, read: &mut ReadSource<'_>, cancelled: &dyn Fn() -> bool) -> Result<Self, String> {
        let path = std::env::temp_dir().join(format!(
            "kwil-markdown-{}-{}.idx",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let file = OpenOptions::new().read(true).write(true).create_new(true).open(&path).map_err(|e| e.to_string())?;
        let mut pager = Self { file: Some(file), path, count: 0, total_lines: 1, definitions: String::new() };
        let mut out = std::io::BufWriter::new(pager.file.as_mut().unwrap());
        let mut scanner = Boundaries::default();
        let mut bytes = Vec::with_capacity(CHUNK);
        let mut prefix = Vec::with_capacity(8192);
        let mut line = 0;
        let mut line_start = 0;
        let mut block = Record::default();
        let mut after_cr = false;
        let mut count = 0;
        let mut references = References::default();
        for start in (0..len).step_by(CHUNK) {
            if cancelled() {
                return Err("Preview cancelled".into());
            }
            bytes.clear();
            let end = len.min(start.saturating_add(CHUNK as u64));
            read(start..end, &mut bytes)?;
            if bytes.len() as u64 != end - start {
                return Err("Short Markdown source read".into());
            }
            for (i, &byte) in bytes.iter().enumerate() {
                let at = start + i as u64;
                if after_cr && byte == b'\n' {
                    after_cr = false;
                    line_start = at + 1;
                    continue;
                }
                after_cr = byte == b'\r';
                if matches!(byte, b'\n' | b'\r') {
                    references.line(
                        &prefix,
                        scanner.fence.is_none() && scanner.html.is_none(),
                        at - line_start > prefix.len() as u64,
                    )?;
                    if scanner.line(&prefix, at - line_start > prefix.len() as u64) && line_start > block.start {
                        Record { end: line_start, end_line: line, safe_end: true, ..block }.write(&mut out)?;
                        count += 1;
                        block = Record { start: line_start, line, ..Default::default() };
                    }
                    line += 1;
                    if at + 1 - block.start >= CHUNK as u64 {
                        Record { end: at + 1, end_line: line, ..block }.write(&mut out)?;
                        count += 1;
                        block = Record { start: at + 1, line, ..Default::default() };
                    }
                    line_start = at + 1;
                    prefix.clear();
                } else {
                    if prefix.len() < 8192 {
                        prefix.push(byte);
                    }
                    // A single arbitrarily long line must not grow the scanner.
                    if at + 1 - block.start >= CHUNK as u64 {
                        Record { end: at + 1, end_line: line + 1, ..block }.write(&mut out)?;
                        count += 1;
                        block = Record { start: at + 1, line, ..Default::default() };
                    }
                }
            }
        }
        references.line(
            &prefix,
            scanner.fence.is_none() && scanner.html.is_none(),
            len - line_start > prefix.len() as u64,
        )?;
        references.finish()?;
        if !prefix.is_empty()
            && scanner.line(&prefix, len - line_start > prefix.len() as u64)
            && line_start > block.start
        {
            Record { end: line_start, end_line: line, safe_end: true, ..block }.write(&mut out)?;
            count += 1;
            block = Record { start: line_start, line, ..Default::default() };
        }
        // An empty final record closes a forced fragment ending exactly at EOF.
        Record { end: len, end_line: line + 1, safe_end: true, ..block }.write(&mut out)?;
        count += 1;
        out.flush().map_err(|e| e.to_string())?;
        drop(out);
        pager.count = count;
        pager.total_lines = line + 1;
        pager.definitions = references.definitions;
        Ok(pager)
    }
    fn record(&mut self, index: u64) -> Result<Record, String> {
        let file = self.file.as_mut().unwrap();
        file.seek(SeekFrom::Start(index * RECORD_BYTES)).map_err(|e| e.to_string())?;
        let mut bytes = [0; RECORD_BYTES as usize];
        file.read_exact(&mut bytes).map_err(|e| e.to_string())?;
        let n = |i: usize| u64::from_le_bytes(bytes[i * 8..i * 8 + 8].try_into().unwrap());
        Ok(Record { start: n(0), end: n(1), line: n(2), end_line: n(3), safe_end: n(4) != 0 })
    }
    pub fn page(
        &mut self,
        top: u64,
        rows: usize,
        read: &mut ReadSource<'_>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Page, String> {
        if cancelled() {
            return Err("Preview cancelled".into());
        }
        let top = top.min(self.total_lines.saturating_sub(1));
        let (mut lo, mut hi) = (0, self.count);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.record(mid)?.end_line <= top {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let target = lo.min(self.count - 1);
        let mut first = target;
        // At most four fragments can belong to an admitted 256 KiB block.
        while first > 0 && target - first < 4 && !self.record(first - 1)?.safe_end {
            first -= 1;
        }
        let start = self.record(first)?;
        let mut end = start;
        let mut safe_end = None;
        let mut next = first;
        let mut complete = first == 0 || self.record(first - 1)?.safe_end;
        let limit_line = top.saturating_add(rows.clamp(32, 256) as u64 * 3);
        while next < self.count && next - first < MAX_RECORDS {
            let candidate = self.record(next)?;
            if candidate.end - start.start > CHUNK as u64 {
                break;
            }
            end = candidate;
            next += 1;
            if candidate.safe_end {
                safe_end = Some(candidate);
            }
            if candidate.safe_end && candidate.end_line >= limit_line {
                break;
            }
        }
        // Never parse a partial fence/list/HTML block as independent Markdown.
        // Oversized indivisible blocks use a clearly labelled bounded source view.
        if let Some(last) = safe_end.filter(|last| last.end_line > top) {
            end = last;
        }
        complete &= end.safe_end && end.end_line > top;
        if !complete {
            let fragment = self.record(target)?;
            let mut bytes = Vec::with_capacity(CHUNK);
            read(fragment.start..fragment.end, &mut bytes)?;
            // Forced byte boundaries may bisect UTF-8; omit only edge fragments.
            let leading = bytes.iter().take(3).take_while(|&b| b & 0xc0 == 0x80).count();
            bytes.drain(..leading);
            while std::str::from_utf8(&bytes).is_err_and(|e| e.error_len().is_none()) {
                bytes.pop();
            }
            let raw =
                std::str::from_utf8(&bytes).map_err(|_| "Preview unavailable: malformed or opaque text".to_string())?;
            use unicode_segmentation::UnicodeSegmentation;
            if raw.chars().any(|c| c.is_control() && !matches!(c, '\r' | '\n' | '\t'))
                || raw.graphemes(true).any(|c| c.len() > 256)
            {
                return Err("Preview unavailable: opaque text or oversized grapheme".into());
            }
            let block = Block {
                kind: BlockKind::CodeBlock { language: Some("Large block — source".into()), raw: raw.into() },
                spans: vec![],
                source_bytes: fragment.start as usize..fragment.end as usize,
                source_lines: fragment.line as usize..fragment.end_line as usize,
                quote_depth: 0,
                list_depth: 0,
            };
            return Ok(Page {
                model: Model { blocks: vec![block], source_bytes: fragment.start as usize..fragment.end as usize },
                total_lines: self.total_lines,
                parsed_bytes: 0,
            });
        }
        let range = start.start..end.end;
        let mut bytes = Vec::with_capacity((range.end - range.start) as usize);
        read(range.clone(), &mut bytes)?;
        // Definitions outside the page still resolve links. Their separate
        // fixed allowance is included in the existing parser admission.
        if bytes.len() + self.definitions.len() > PREVIEW_SOURCE_BYTES {
            return Err("Preview page and reference definitions exceed 256 KiB".into());
        }
        let source_len = bytes.len();
        let prefix_bytes = self.definitions.len();
        let prefix_lines = self.definitions.bytes().filter(|&b| b == b'\n').count();
        if prefix_bytes != 0 {
            let mut combined = self.definitions.as_bytes().to_vec();
            combined.extend_from_slice(&bytes);
            bytes = combined;
        }
        let text =
            std::str::from_utf8(&bytes).map_err(|_| "Preview unavailable: malformed or opaque text".to_string())?;
        let mut model =
            crate::bounded::preview_model(text, cancelled).map_err(|e| format!("Preview unavailable: {e:?}"))?;
        model.blocks.retain(|block| {
            block.source_bytes.start >= prefix_bytes && block.source_bytes.start < prefix_bytes + source_len
        });
        for block in &mut model.blocks {
            block.source_bytes = block.source_bytes.start - prefix_bytes + range.start as usize
                ..block.source_bytes.end - prefix_bytes + range.start as usize;
            block.source_lines = block.source_lines.start - prefix_lines + start.line as usize
                ..block.source_lines.end - prefix_lines + start.line as usize;
        }
        model.source_bytes = range.start as usize..range.end as usize;
        Ok(Page { model, total_lines: self.total_lines, parsed_bytes: bytes.len() })
    }
}

// Only candidate definition paragraphs are parsed during indexing. Ordinary
// offscreen blocks never enter CommonMark. Canonical definitions cannot open a
// fence or merge into the selected page. Keep first-definition-wins semantics.
#[derive(Default)]
struct References {
    pending: Vec<u8>,
    definitions: String,
    labels: Vec<String>,
}
impl References {
    fn line(&mut self, prefix: &[u8], eligible: bool, truncated: bool) -> Result<(), String> {
        let text = prefix.trim_ascii_start();
        if !self.pending.is_empty() && (text.is_empty() || !eligible) {
            self.finish()?;
        }
        let candidate = eligible && text.starts_with(b"[") && text.windows(2).any(|w| w == b"]:");
        if candidate || !self.pending.is_empty() {
            if truncated || self.pending.len() + prefix.len() > 32 * 1024 {
                return Err("Markdown reference definition exceeds the bounded allowance".into());
            }
            self.pending.extend_from_slice(prefix);
            self.pending.push(b'\n');
        }
        Ok(())
    }
    fn finish(&mut self) -> Result<(), String> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let text = std::str::from_utf8(&self.pending).map_err(|_| "Malformed Markdown reference definition")?;
        let parser = pulldown_cmark::Parser::new(text);
        for (label, definition) in parser.reference_definitions().iter() {
            if self.labels.iter().any(|s| s == label) {
                continue;
            }
            if definition.dest.len() > 4096 || definition.title.as_ref().is_some_and(|s| s.len() > 4096) {
                return Err("Markdown reference target exceeds the bounded allowance".into());
            }
            let original_label = label.to_string();
            let label = label.replace('\\', "\\\\").replace('[', "\\[").replace(']', "\\]");
            let dest =
                definition.dest.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('\\', "\\\\");
            let line = format!("\n[{label}]: <{dest}>\n");
            if self.definitions.len() + line.len() > 32 * 1024 {
                return Err("Markdown reference definitions exceed the shared 32 KiB allowance".into());
            }
            self.definitions.push_str(&line);
            self.labels.push(original_label);
        }
        self.pending.clear();
        Ok(())
    }
}

/// Conservative top-level blank boundaries. A loose list stays together until
/// the next unindented non-list line. Raw HTML terminators and fence run lengths
/// survive read chunks; indented/list fences cannot accidentally end a block.
#[derive(Default)]
struct Boundaries {
    blank: bool,
    list: bool,
    quote: bool,
    fence: Option<(u8, usize)>,
    html: Option<&'static [u8]>,
}
impl Boundaries {
    fn line(&mut self, prefix: &[u8], truncated: bool) -> bool {
        let text = prefix.trim_ascii();
        let indent = prefix
            .iter()
            .take_while(|b| b.is_ascii_whitespace())
            .fold(0, |column, &b| if b == b'\t' { (column / 4 + 1) * 4 } else { column + 1 });
        if let Some((mark, width)) = self.fence {
            if text.iter().take_while(|&&b| b == mark).count() >= width
                && text.iter().skip_while(|&&b| b == mark).all(u8::is_ascii_whitespace)
                && !truncated
            {
                self.fence = None;
            }
            return false;
        }
        if let Some(end) = self.html {
            if (end.is_empty() && text.is_empty())
                || (!end.is_empty() && text.windows(end.len()).any(|w| w.eq_ignore_ascii_case(end)))
            {
                self.html = None;
                self.blank = text.is_empty();
            }
            return false;
        }
        if text.is_empty() && !truncated {
            self.blank = true;
            return false;
        }
        let list_item = matches!(text, [b'-' | b'+' | b'*', b' ' | b'\t', ..])
            || text.iter().take_while(|b| b.is_ascii_digit()).count().checked_sub(1).is_some_and(|_| {
                let n = text.iter().take_while(|b| b.is_ascii_digit()).count();
                n <= 9 && matches!(text.get(n..), Some([b'.' | b')', b' ' | b'\t', ..]))
            });
        let quoted = text.starts_with(b">");
        let safe = self.blank
            && indent <= 3
            && (!self.list || (indent == 0 && !list_item))
            && (!self.quote || (indent == 0 && !quoted));
        if safe {
            self.list = false;
            self.quote = false;
        }
        self.blank = false;
        self.list |= list_item;
        self.quote |= quoted;
        // Quotes/indented continuations stay in their enclosing block. Matching
        // fence markers are intentionally conservative inside list containers.
        let mark = text.first().copied().unwrap_or(0);
        let width = text.iter().take_while(|&&b| b == mark).count();
        if matches!(mark, b'`' | b'~') && width >= 3 && (indent <= 3 || self.list) {
            if mark != b'`' || !text[width..].contains(&b'`') {
                self.fence = Some((mark, width));
            }
        } else if indent <= 3 && text.starts_with(b"<") {
            let lower: Vec<_> = text.iter().map(u8::to_ascii_lowercase).collect();
            let end: &'static [u8] = if lower.starts_with(b"<!--") {
                b"-->"
            } else if lower.starts_with(b"<![cdata[") {
                b"]]>"
            } else if lower.starts_with(b"<?") {
                b"?>"
            } else if lower.starts_with(b"<!") {
                b">"
            } else if [b"script".as_slice(), b"pre", b"style", b"textarea"].iter().any(|tag| {
                lower.get(1..).is_some_and(|s| {
                    s.starts_with(tag) && s.get(tag.len()).is_some_and(|b| b.is_ascii_whitespace() || *b == b'>')
                })
            }) {
                if lower.starts_with(b"<script") {
                    b"</script>"
                } else if lower.starts_with(b"<pre") {
                    b"</pre>"
                } else if lower.starts_with(b"<style") {
                    b"</style>"
                } else {
                    b"</textarea>"
                }
            } else {
                b""
            };
            if end.is_empty() || !lower.windows(end.len()).any(|w| w == end) {
                self.html = Some(end);
            }
        }
        safe
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn read(text: &str) -> impl FnMut(Range<u64>, &mut Vec<u8>) -> Result<(), String> + '_ {
        |range, out| {
            out.extend_from_slice(&text.as_bytes()[range.start as usize..range.end as usize]);
            Ok(())
        }
    }
    #[test]
    fn fences_loose_lists_and_raw_html_keep_complete_boundaries() {
        let text = "# Before\n\n````rust\na\n\n# inside\n```\n\nb\n````\n\n- one\n\n  continued\n\n- two\n\n<script>\n\n# inert\n\n</script>\n\n# After\n";
        let mut pager = Pager::build(text.len() as u64, &mut read(text), &|| false).unwrap();
        for top in [4, 13, 21] {
            let page = pager.page(top, 1, &mut read(text), &|| false).unwrap();
            let expected = crate::bounded::preview_model(text, &|| false).unwrap();
            for block in &page.model.blocks {
                assert!(expected.blocks.contains(block), "partial block at {top}: {block:?}");
            }
        }
    }
    #[test]
    fn references_outside_the_page_resolve_without_copying_the_document() {
        let text = format!(
            "[remote]: https://example.com \"Title\"\n\n{}# Deep\n\n[link][remote]\n",
            "paragraph\n\n".repeat(200)
        );
        let mut pager = Pager::build(text.len() as u64, &mut read(&text), &|| false).unwrap();
        let page = pager.page(402, 1, &mut read(&text), &|| false).unwrap();
        assert!(
            page.model
                .blocks
                .iter()
                .flat_map(|b| &b.spans)
                .any(|s| s.style.link.as_deref() == Some("https://example.com"))
        );
        assert!(page.model.blocks.iter().all(|b| b.source_lines.start >= 402));
    }
    #[test]
    fn oversized_fence_uses_bounded_source_instead_of_false_headings() {
        let text = format!("```\n{}\n```\n\n# After\n", "# code\n\n".repeat(50_000));
        let mut pager = Pager::build(text.len() as u64, &mut read(&text), &|| false).unwrap();
        let page = pager.page(40_000, 10, &mut read(&text), &|| false).unwrap();
        assert_eq!(page.parsed_bytes, 0);
        assert!(matches!(page.model.blocks[0].kind, BlockKind::CodeBlock { .. }));
        assert!(page.model.source_bytes.len() <= CHUNK);
        let after = pager.page(100_004, 10, &mut read(&text), &|| false).unwrap();
        assert!(after.model.blocks.iter().any(|b| matches!(b.kind, BlockKind::Heading(1))));
    }
    #[test]
    fn crlf_and_bare_cr_preserve_source_coordinates() {
        for sep in ["\r\n", "\r", "\n"] {
            let text = ["# Before", "", "paragraph", "", "# After", ""].join(sep);
            let mut pager = Pager::build(text.len() as u64, &mut read(&text), &|| false).unwrap();
            let page = pager.page(4, 10, &mut read(&text), &|| false).unwrap();
            assert_eq!(page.model.blocks[0].source_lines, 4..5);
            assert_eq!(page.model.blocks[0].source_bytes.start, text.find("# After").unwrap());
        }
    }
    #[test]
    fn cancellation_retires_the_private_index() {
        let text = "# Heading\n";
        assert!(Pager::build(text.len() as u64, &mut read(text), &|| true).is_err());
    }
}
