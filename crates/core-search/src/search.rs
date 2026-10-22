use crate::Cancellation;
use memchr::memmem;
use regex::bytes::{Regex, RegexBuilder};
use std::io;
use std::ops::Range;
use std::sync::OnceLock;
use std::sync::atomic::Ordering;

#[cfg(not(test))]
pub(crate) const BLOCK: usize = 1 << 20;
#[cfg(test)]
pub(crate) const BLOCK: usize = 128;
const MAX_PATTERN: usize = 1 << 20;

/// Borrowed, contiguous-in-offset chunks covering exactly the requested range.
/// Chunks may have any size, including one byte; empty chunks are permitted.
/// Implementations must retain an immutable snapshot for the entire operation.
/// Range validation and read failures may occur when opening the iterator or
/// reading any chunk; consumers propagate both kinds of errors to the caller.
pub trait Haystack {
    fn len(&self) -> u64;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn chunks(&self, range: Range<u64>) -> io::Result<impl Iterator<Item = io::Result<(u64, &[u8])>>>;
}

impl Haystack for kwil_core_text::TextFile {
    fn len(&self) -> u64 {
        self.len()
    }
    fn chunks(&self, range: Range<u64>) -> io::Result<impl Iterator<Item = io::Result<(u64, &[u8])>>> {
        // TextFile owns the mapping and its external-change safety contract.
        let bytes = self.bytes().map_err(io::Error::other)?;
        let bytes =
            bytes.get(range.start as usize..range.end as usize).ok_or_else(|| invalid("invalid haystack range"))?;
        Ok(bytes.chunks(BLOCK).enumerate().map(move |(i, b)| Ok((range.start + (i * BLOCK) as u64, b))))
    }
}

impl Haystack for kwil_core_text::Snapshot {
    fn len(&self) -> u64 {
        self.len()
    }
    fn chunks(&self, range: Range<u64>) -> io::Result<impl Iterator<Item = io::Result<(u64, &[u8])>>> {
        let mut at = range.start;
        Ok(self.chunks(range)?.map(move |chunk| {
            let bytes = chunk.map_err(io::Error::other)?;
            let start = at;
            at += bytes.len() as u64;
            Ok((start, bytes))
        }))
    }
}

impl Haystack for [u8] {
    fn len(&self) -> u64 {
        self.len() as u64
    }
    fn chunks(&self, range: Range<u64>) -> io::Result<impl Iterator<Item = io::Result<(u64, &[u8])>>> {
        let bytes =
            self.get(range.start as usize..range.end as usize).ok_or_else(|| invalid("invalid haystack range"))?;
        Ok(bytes.chunks(BLOCK).enumerate().map(move |(i, b)| Ok((range.start + (i * BLOCK) as u64, b))))
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    pub case_insensitive: bool,
    /// Unicode word characters (regex's `\w`), with invalid bytes non-word.
    /// Selection edges use the surrounding document, not artificial boundaries.
    pub whole_word: bool,
}

#[derive(Clone, Debug)]
enum Engine {
    Literal(Vec<u8>),
    Regex(Regex),
}

#[derive(Clone, Debug)]
pub struct Pattern {
    engine: Engine,
    options: Options,
    max_len: usize,
    captures: bool,
    pub(crate) newline_safe: bool,
}

impl Pattern {
    pub fn literal(needle: &[u8], options: Options) -> io::Result<Self> {
        if needle.is_empty() || needle.len() > MAX_PATTERN {
            return Err(invalid("literal length must be 1..=1 MiB"));
        }
        if options.case_insensitive {
            let (escaped, unicode) = match std::str::from_utf8(needle) {
                Ok(s) => (regex::escape(s), true),
                Err(_) => (needle.iter().map(|b| format!("\\x{b:02x}")).collect(), false),
            };
            let re = RegexBuilder::new(&escaped).unicode(unicode).case_insensitive(true).build().map_err(invalid)?;
            Ok(Self {
                engine: Engine::Regex(re),
                options,
                max_len: needle.len().saturating_mul(4).min(MAX_PATTERN),
                captures: false,
                newline_safe: !needle.contains(&b'\n'),
            })
        } else {
            Ok(Self {
                engine: Engine::Literal(needle.to_vec()),
                options,
                max_len: needle.len(),
                captures: false,
                newline_safe: !needle.contains(&b'\n'),
            })
        }
    }

    /// Regex search uses overlap, never a whole-file or whole-line buffer.
    ///
    /// Contract: every possible match must fit `max_match_bytes` (1..=1 MiB).
    /// 64 KiB is a useful UI default. Unbounded expressions such as `.*` are
    /// supported only when the caller can guarantee this bound for its input.
    /// Longer matches are outside this API's contract. Detected overlong matches
    /// return an error, but matches longer than the bound may be missed silently
    /// across window borders. Callers must not assume every violation errors.
    /// Use line-oriented find-in-files when complete bounded lines are needed.
    /// Anchors, captures, Unicode and zero-width matches retain regex semantics
    /// within this contract. The range restricts matches, not anchor context.
    pub fn regex(expression: &str, options: Options, max_match_bytes: usize) -> io::Result<Self> {
        if !(1..=MAX_PATTERN).contains(&max_match_bytes) {
            return Err(invalid("regex match bound must be 1..=1 MiB"));
        }
        let constrained =
            if options.whole_word { format!(r"(?u:\b)(?:{expression})(?u:\b)") } else { expression.to_owned() };
        let re = RegexBuilder::new(&constrained)
            .case_insensitive(options.case_insensitive)
            .size_limit(8 << 20)
            .dfa_size_limit(2 << 20)
            .build()
            .map_err(invalid)?;
        let hir = regex_syntax::ParserBuilder::new()
            .utf8(false)
            .case_insensitive(options.case_insensitive)
            .build()
            .parse(expression)
            .map_err(invalid)?;
        Ok(Self {
            engine: Engine::Regex(re),
            options,
            max_len: max_match_bytes,
            captures: true,
            newline_safe: !consumes_newline(&hir),
        })
    }

    pub(crate) fn visit(
        &self,
        bytes: &[u8],
        start: usize,
        mut hit: impl FnMut(Range<usize>) -> io::Result<bool>,
    ) -> io::Result<()> {
        match &self.engine {
            Engine::Literal(needle) => {
                let finder = memmem::Finder::new(needle);
                for at in finder.find_iter(&bytes[start..]) {
                    if !hit(start + at..start + at + needle.len())? {
                        break;
                    }
                }
            }
            Engine::Regex(re) => {
                // find_iter over a sliced suffix would invent ^ and \b boundaries.
                let mut at = start;
                let mut previous_end = None;
                while at <= bytes.len() {
                    let Some(m) = re.find_at(bytes, at) else {
                        break;
                    };
                    if m.is_empty() && previous_end == Some(m.end()) {
                        at = m.end() + 1;
                        continue;
                    }
                    if !hit(m.range())? {
                        break;
                    }
                    previous_end = Some(m.end());
                    at = m.end() + usize::from(m.is_empty());
                }
            }
        }
        Ok(())
    }

    fn expand(&self, bytes: &[u8], at: Range<usize>, replacement: &[u8]) -> io::Result<Vec<u8>> {
        match &self.engine {
            Engine::Literal(needle) => {
                if bytes.get(at) != Some(needle.as_slice()) {
                    return Err(invalid("match no longer exists"));
                }
                Ok(replacement.to_vec())
            }
            Engine::Regex(re) => {
                let caps = re.captures_at(bytes, at.start).ok_or_else(|| invalid("match no longer exists"))?;
                if caps.get(0).unwrap().range() != at {
                    return Err(invalid("match does not belong to this pattern"));
                }
                if !self.captures {
                    return Ok(replacement.to_vec());
                }
                let mut out = Vec::new();
                // Implement regex's documented $name/${name}/$$ interpolation,
                // checking BEFORE each append so repeated captures cannot grow
                // a Vec to template_size * match_size.
                let mut i = 0;
                while i < replacement.len() {
                    if replacement[i] != b'$' {
                        append(&mut out, &replacement[i..i + 1])?;
                        i += 1;
                        continue;
                    }
                    if replacement.get(i + 1) == Some(&b'$') {
                        append(&mut out, b"$")?;
                        i += 2;
                        continue;
                    }
                    let braced = replacement.get(i + 1) == Some(&b'{');
                    let start = i + if braced { 2 } else { 1 };
                    let end = if braced {
                        replacement[start..].iter().position(|b| *b == b'}').map(|n| start + n)
                    } else {
                        Some(
                            start
                                + replacement[start..]
                                    .iter()
                                    .take_while(|b| b.is_ascii_alphanumeric() || **b == b'_')
                                    .count(),
                        )
                    };
                    let Some(end) = end.filter(|e| braced || *e > start) else {
                        append(&mut out, b"$")?;
                        i += 1;
                        continue;
                    };
                    if let Ok(name) = std::str::from_utf8(&replacement[start..end]) {
                        let capture = match name.parse::<usize>() {
                            Ok(n) => caps.get(n),
                            Err(_) => caps.name(name),
                        };
                        if let Some(capture) = capture {
                            append(&mut out, capture.as_bytes())?;
                        }
                    }
                    i = end + usize::from(braced);
                }
                Ok(out)
            }
        }
    }
}

fn consumes_newline(hir: &regex_syntax::hir::Hir) -> bool {
    use regex_syntax::hir::{Class, HirKind};
    match hir.kind() {
        HirKind::Empty | HirKind::Look(_) => false,
        HirKind::Literal(lit) => lit.0.contains(&b'\n'),
        HirKind::Class(Class::Unicode(c)) => c.iter().any(|r| r.start() <= '\n' && r.end() >= '\n'),
        HirKind::Class(Class::Bytes(c)) => c.iter().any(|r| r.start() <= b'\n' && r.end() >= b'\n'),
        HirKind::Repetition(r) => consumes_newline(&r.sub),
        HirKind::Capture(c) => consumes_newline(&c.sub),
        HirKind::Concat(v) | HirKind::Alternation(v) => v.iter().any(consumes_newline),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Match {
    pub offset: u64,
    pub len: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchMode {
    Hits,
    Count,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SearchSummary {
    pub count: u64,
    pub scanned: u64,
    pub cancelled: bool,
    pub workers: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct Progress {
    pub scanned: u64,
    pub total: u64,
    pub count: u64,
}

/// Streams non-overlapping hits in increasing offset order. Count mode never
/// invokes `on_hit` or retains hits. Progress is emitted per bounded work block,
/// plus completion; frontends should coalesce these wakeups for their status UI.
pub fn search<H: Haystack + ?Sized>(
    source: &H,
    pattern: &Pattern,
    range: Range<u64>,
    mode: SearchMode,
    cancel: &Cancellation,
    mut on_hit: impl FnMut(Match),
    mut progress: impl FnMut(Progress),
) -> io::Result<SearchSummary> {
    let mut result = SearchSummary { workers: 1, ..SearchSummary::default() };
    let mut next = range.start;
    let mut last_end = None;
    let total = range.end.saturating_sub(range.start);
    windows(source, range.clone(), pattern.max_len, cancel, |bytes, base, safe_end, final_window| {
        let from = (next.max(base) - base) as usize;
        if from <= bytes.len() {
            pattern.visit(bytes, from, |m| {
                let start = base + m.start as u64;
                let end = base + m.end as u64;
                if start >= safe_end && !(final_window && start == safe_end) {
                    return Ok(false);
                }
                if cancel.load(Ordering::Relaxed) {
                    return Ok(false);
                }
                if end > range.end {
                    return Ok(true);
                }
                if m.len() > pattern.max_len {
                    return Err(invalid("regex match exceeds declared bound"));
                }
                if m.is_empty() && last_end == Some(start) {
                    next = start + 1;
                    return Ok(true);
                }
                if pattern.options.whole_word && !pattern.captures && !whole_word(bytes, m.clone()) {
                    return Ok(true);
                }
                result.count += 1;
                last_end = Some(end);
                next = end + u64::from(m.is_empty());
                if mode == SearchMode::Hits {
                    on_hit(Match { offset: start, len: end - start });
                }
                Ok(true)
            })?;
        }
        next = next.max(safe_end);
        result.scanned = safe_end.saturating_sub(range.start).min(total);
        progress(Progress { scanned: result.scanned, total, count: result.count });
        Ok(())
    })?;
    result.cancelled = cancel.load(Ordering::Relaxed);
    Ok(result)
}

/// Each edit is against the same snapshot. The editor applies this ordered
/// stream as one transaction (or applies an admitted list in reverse order).
/// No mutation, hit list or edit list is retained here. A callback may spool to
/// disk for replace-all; callers collecting a Vec must impose their own cap.
#[derive(Debug, PartialEq, Eq)]
pub struct Edit {
    pub offset: u64,
    pub len: u64,
    pub new_bytes: Vec<u8>,
}

pub fn plan_replace<H: Haystack + ?Sized>(
    source: &H,
    pattern: &Pattern,
    matches: impl IntoIterator<Item = Match>,
    replacement: &[u8],
    cancel: &Cancellation,
    mut emit: impl FnMut(Edit) -> io::Result<()>,
) -> io::Result<()> {
    // A single replacement is bounded too, before capture expansion.
    if replacement.len() > MAX_PATTERN {
        return Err(invalid("replacement exceeds 1 MiB"));
    }
    let mut previous_end = 0;
    let mut previous = None;
    for m in matches {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let end =
            m.offset.checked_add(m.len).filter(|e| *e <= source.len()).ok_or_else(|| invalid("invalid match range"))?;
        if m.offset < previous_end || previous == Some(m.offset) || m.len > pattern.max_len as u64 {
            return Err(invalid("unordered, overlapping or oversized matches"));
        }
        // Keep enough surrounding context for anchors and capture verification.
        let lo = m.offset.saturating_sub(4);
        let hi = end.saturating_add(pattern.max_len as u64 + 4).min(source.len());
        let mut bytes = Vec::with_capacity((hi - lo) as usize);
        copy_range(source, lo..hi, &mut bytes)?;
        let at = (m.offset - lo) as usize..(end - lo) as usize;
        let new_bytes = pattern.expand(&bytes, at, replacement)?;
        emit(Edit { offset: m.offset, len: m.len, new_bytes })?;
        previous_end = end;
        previous = Some(m.offset);
    }
    Ok(())
}

fn append(out: &mut Vec<u8>, bytes: &[u8]) -> io::Result<()> {
    if bytes.len() > (8 << 20) - out.len() {
        return Err(invalid("expanded replacement exceeds 8 MiB"));
    }
    out.extend_from_slice(bytes);
    Ok(())
}

pub(crate) fn invalid(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, e.to_string())
}

pub(crate) fn copy_range<H: Haystack + ?Sized>(source: &H, range: Range<u64>, out: &mut Vec<u8>) -> io::Result<()> {
    let mut expected = range.start;
    for chunk in source.chunks(range.clone())? {
        let (at, b) = chunk?;
        if at != expected || b.len() as u64 > range.end - expected {
            return Err(invalid("invalid haystack chunks"));
        }
        out.extend_from_slice(b);
        expected += b.len() as u64;
    }
    if expected != range.end {
        return Err(invalid("incomplete haystack chunks"));
    }
    Ok(())
}

/// Supplies bounded windows with 4 bytes of left context and max_len + 4 of
/// right context. Only starts before safe_end belong to a non-final window.
pub(crate) fn windows<H: Haystack + ?Sized>(
    source: &H,
    range: Range<u64>,
    max_len: usize,
    cancel: &Cancellation,
    mut visit: impl FnMut(&[u8], u64, u64, bool) -> io::Result<()>,
) -> io::Result<()> {
    if range.start > range.end || range.end > source.len() {
        return Err(invalid("invalid search range"));
    }
    let lo = range.start.saturating_sub(4);
    let hi = range.end.saturating_add(max_len as u64 + 4).min(source.len());
    // Most log lines are one borrowed chunk. Avoid an allocation and copy for
    // every pattern on every line, while retaining the same source contract.
    if hi - lo <= (BLOCK + max_len + 8) as u64 {
        let mut chunks = source.chunks(lo..hi)?;
        if let Some((at, bytes)) = chunks.next().transpose()?
            && at == lo
            && bytes.len() as u64 == hi - lo
        {
            for chunk in chunks {
                let (at, b) = chunk?;
                if at != hi || !b.is_empty() {
                    return Err(invalid("invalid haystack chunks"));
                }
            }
            if !cancel.load(Ordering::Relaxed) {
                visit(bytes, lo, range.end, true)?;
            }
            return Ok(());
        }
    }
    let mut base = lo;
    let mut expected = lo;
    let mut buf = Vec::with_capacity(BLOCK + max_len + 8);
    let pad = max_len + 4;
    let mut finished_direct = false;
    for chunk in source.chunks(lo..hi)? {
        let (at, mut chunk) = chunk?;
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        if at != expected || chunk.len() as u64 > hi - expected {
            return Err(invalid("invalid haystack chunks"));
        }
        expected += chunk.len() as u64;
        while !chunk.is_empty() {
            if cancel.load(Ordering::Relaxed) {
                return Ok(());
            }
            let part_len = chunk.len().min(BLOCK);
            // Borrow the main body of a large chunk. Only the unresolved tail
            // and the next chunk's prefix are copied for boundary matching.
            if part_len >= 2 * pad + 8 {
                let at = expected - chunk.len() as u64;
                let part = &chunk[..part_len];
                if !buf.is_empty() {
                    buf.extend_from_slice(&part[..pad + 4]);
                    visit(&buf, base, (at + 4).min(range.end), false)?;
                    buf.clear();
                }
                let final_part = at + part_len as u64 == hi;
                let safe = if final_part { range.end } else { (at + (part_len - pad) as u64).min(range.end) };
                visit(part, at, safe, final_part)?;
                if final_part {
                    finished_direct = true;
                } else {
                    let tail = part_len - pad - 4;
                    buf.extend_from_slice(&part[tail..]);
                    base = at + tail as u64;
                }
                chunk = &chunk[part_len..];
                continue;
            }
            let n = chunk.len().min(BLOCK + max_len + 8 - buf.len());
            buf.extend_from_slice(&chunk[..n]);
            chunk = &chunk[n..];
            if buf.len() == BLOCK + max_len + 8 {
                let safe = (base + (buf.len() - max_len - 4) as u64).min(range.end);
                visit(&buf, base, safe, false)?;
                let discard = (safe.saturating_sub(4) - base) as usize;
                if discard == 0 {
                    break;
                }
                buf.copy_within(discard.., 0);
                buf.truncate(buf.len() - discard);
                base += discard as u64;
            }
        }
    }
    if expected != hi {
        return Err(invalid("incomplete haystack chunks"));
    }
    if !finished_direct && !cancel.load(Ordering::Relaxed) {
        visit(&buf, base, range.end, true)?;
    }
    Ok(())
}

fn whole_word(bytes: &[u8], m: Range<usize>) -> bool {
    (word_before(&bytes[..m.start]) != word_after(&bytes[m.start..]))
        && (word_before(&bytes[..m.end]) != word_after(&bytes[m.end..]))
}
fn word_before(bytes: &[u8]) -> bool {
    (1..=4.min(bytes.len()))
        .find_map(|n| std::str::from_utf8(&bytes[bytes.len() - n..]).ok().and_then(|s| s.chars().next()))
        .is_some_and(word_char)
}
fn word_after(bytes: &[u8]) -> bool {
    (1..=4.min(bytes.len()))
        .find_map(|n| std::str::from_utf8(&bytes[..n]).ok().and_then(|s| s.chars().next()))
        .is_some_and(word_char)
}
fn word_char(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_alphanumeric() || c == '_';
    }
    static WORD: OnceLock<regex::Regex> = OnceLock::new();
    WORD.get_or_init(|| regex::Regex::new(r"^\w$").unwrap()).is_match(c.encode_utf8(&mut [0; 4]))
}
