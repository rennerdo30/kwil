use crate::Cancellation;
use memchr::memmem;
use regex::bytes::{Regex, RegexBuilder};
use std::io;
use std::ops::Range;
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
    fn chunks(
        &self,
        range: Range<u64>,
    ) -> io::Result<impl Iterator<Item = io::Result<(u64, std::borrow::Cow<'_, [u8]>)>>>;
}

impl Haystack for kwil_core_text::TextFile {
    fn len(&self) -> u64 {
        self.len()
    }
    fn chunks(
        &self,
        range: Range<u64>,
    ) -> io::Result<impl Iterator<Item = io::Result<(u64, std::borrow::Cow<'_, [u8]>)>>> {
        if range.start > range.end || range.end > self.len() {
            return Err(invalid("invalid haystack range"));
        }
        let mut at = range.start;
        Ok(std::iter::from_fn(move || {
            if at == range.end {
                return None;
            }
            let end = (at + BLOCK as u64).min(range.end);
            let start = at;
            at = end;
            Some(self.read_range(start..end).map(|bytes| (start, bytes)).map_err(io::Error::other))
        }))
    }
}

impl Haystack for kwil_core_text::Snapshot {
    fn len(&self) -> u64 {
        self.len()
    }
    fn chunks(
        &self,
        range: Range<u64>,
    ) -> io::Result<impl Iterator<Item = io::Result<(u64, std::borrow::Cow<'_, [u8]>)>>> {
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
    fn chunks(
        &self,
        range: Range<u64>,
    ) -> io::Result<impl Iterator<Item = io::Result<(u64, std::borrow::Cow<'_, [u8]>)>>> {
        let bytes =
            self.get(range.start as usize..range.end as usize).ok_or_else(|| invalid("invalid haystack range"))?;
        Ok(bytes
            .chunks(BLOCK)
            .enumerate()
            .map(move |(i, b)| Ok((range.start + (i * BLOCK) as u64, std::borrow::Cow::Borrowed(b)))))
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
    max_len: usize,
    source_limit: Option<usize>,
    captures: bool,
    pub(crate) newline_safe: bool,
    cr_safe: bool,
}

impl Pattern {
    pub fn literal(needle: &[u8], options: Options) -> io::Result<Self> {
        if needle.is_empty() || needle.len() > MAX_PATTERN {
            return Err(invalid("literal length must be 1..=1 MiB"));
        }
        if options.case_insensitive || options.whole_word {
            let (escaped, unicode) = match std::str::from_utf8(needle) {
                Ok(s) => (regex::escape(s), true),
                Err(_) => (needle.iter().map(|b| format!("\\x{b:02x}")).collect(), false),
            };
            // Boundary rejection belongs inside the matcher: a rejected start
            // must not consume an overlapping candidate. Repeated find calls
            // from start + 1 would rescan long periodic needles. The compiled
            // assertions let regex retry in one scan, emitting non-overlapping
            // accepted hits. Ordinary exact literals keep the memmem fast path.
            let expression = if options.whole_word { whole_word_expression(&escaped)? } else { escaped };
            let hir = regex_syntax::ParserBuilder::new()
                .utf8(false)
                .unicode(unicode)
                .case_insensitive(options.case_insensitive)
                .build()
                .parse(&expression)
                .map_err(invalid)?;
            let max_len = supported_match_len(hir.properties().maximum_len().unwrap_or(0))?;
            let re = RegexBuilder::new(&expression)
                .unicode(unicode)
                .case_insensitive(options.case_insensitive)
                .build()
                .map_err(invalid)?;
            Ok(Self {
                engine: Engine::Regex(re),
                max_len,
                source_limit: None,
                captures: false,
                newline_safe: !needle.contains(&b'\n'),
                cr_safe: !needle.contains(&b'\r'),
            })
        } else {
            Ok(Self {
                engine: Engine::Literal(needle.to_vec()),
                max_len: needle.len(),
                source_limit: None,
                captures: false,
                newline_safe: !needle.contains(&b'\n'),
                cr_safe: !needle.contains(&b'\r'),
            })
        }
    }

    /// Finite regexes derive their overlap from the maximum possible match
    /// length in bytes, including Unicode case folding. Maxima above 1 MiB
    /// return a limit error; the caller's bound never truncates finite matches.
    ///
    /// Unbounded expressions that cannot consume LF search capped lines.
    /// CR also ends a capped line when the pattern cannot consume CR.
    /// Lines exceeding `max_match_bytes` (1..=1 MiB) are skipped, with an
    /// actionable warning in SearchSummary; other lines are still searched.
    /// Newline-consuming unbounded expressions instead cap the entire source,
    /// including selection searches, and return an error on larger sources.
    /// Line-capped anchors default to multiline mode; explicit flags and \A/\z are retained.
    /// Anchors, captures, Unicode and zero-width matches retain regex semantics
    /// within this contract. The range restricts matches, not anchor context.
    pub fn regex(expression: &str, options: Options, max_match_bytes: usize) -> io::Result<Self> {
        if !(1..=MAX_PATTERN).contains(&max_match_bytes) {
            return Err(invalid("regex match bound must be 1..=1 MiB"));
        }
        let hir = regex_syntax::ParserBuilder::new()
            .crlf(true)
            .utf8(false)
            .case_insensitive(options.case_insensitive)
            .build()
            .parse(expression)
            .map_err(invalid)?;
        let maximum = if hir.properties().minimum_len().is_none() {
            Some(0) // The expression matches nothing, rather than an unbounded string.
        } else {
            hir.properties().maximum_len()
        };
        let source_limit = maximum.is_none().then_some(max_match_bytes);
        let max_len = supported_match_len(maximum.unwrap_or(max_match_bytes))?;
        let newline_safe = !consumes_delimiter(&hir, b'\n');
        let cr_safe = !consumes_delimiter(&hir, b'\r');
        let constrained = if options.whole_word { whole_word_expression(expression)? } else { expression.to_owned() };
        let re = RegexBuilder::new(&constrained)
            .crlf(true)
            .multi_line(source_limit.is_some() && newline_safe)
            .case_insensitive(options.case_insensitive)
            .size_limit(8 << 20)
            .dfa_size_limit(2 << 20)
            .build()
            .map_err(invalid)?;
        Ok(Self { engine: Engine::Regex(re), max_len, source_limit, captures: true, newline_safe, cr_safe })
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
                expand_captures(&caps, replacement)
            }
        }
    }
}

pub(crate) fn expand_captures(caps: &regex::bytes::Captures<'_>, replacement: &[u8]) -> io::Result<Vec<u8>> {
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
            Some(start + replacement[start..].iter().take_while(|b| b.is_ascii_alphanumeric() || **b == b'_').count())
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

fn supported_match_len(len: usize) -> io::Result<usize> {
    if len > MAX_PATTERN {
        return Err(invalid("regex maximum match length exceeds 1 MiB; use a smaller bounded repetition"));
    }
    Ok(len.max(1))
}

fn whole_word_expression(expression: &str) -> io::Result<String> {
    // AST serialization removes comments while retaining capture syntax and
    // inline flags. HIR serialization would drop groups under {0} and renumber
    // later captures. Scoped Unicode assertions also preserve word boundaries
    // around byte literals compiled with Unicode disabled.
    let ast = regex_syntax::ast::parse::Parser::new().parse(expression).map_err(invalid)?;
    Ok(format!(r"(?u:\b)(?:{ast})(?u:\b)"))
}

fn consumes_delimiter(hir: &regex_syntax::hir::Hir, byte: u8) -> bool {
    use regex_syntax::hir::{Class, HirKind};
    match hir.kind() {
        HirKind::Empty | HirKind::Look(_) => false,
        HirKind::Literal(lit) => lit.0.contains(&byte),
        HirKind::Class(Class::Unicode(c)) => {
            c.iter().any(|r| r.start() <= char::from(byte) && r.end() >= char::from(byte))
        }
        HirKind::Class(Class::Bytes(c)) => c.iter().any(|r| r.start() <= byte && r.end() >= byte),
        HirKind::Repetition(r) => consumes_delimiter(&r.sub, byte),
        HirKind::Capture(c) => consumes_delimiter(&c.sub, byte),
        HirKind::Concat(v) | HirKind::Alternation(v) => v.iter().any(|h| consumes_delimiter(h, byte)),
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
    /// At least one capped line was skipped. Counts/hits cover the other lines.
    pub skipped_long_lines: bool,
    pub line_limit: usize,
}

impl SearchSummary {
    pub fn warning(&self) -> Option<String> {
        self.skipped_long_lines.then(|| format!(
            "Search incomplete: lines longer than {} bytes were skipped for this unbounded regex; use a bounded repetition such as .{{0,4096}}",
            self.line_limit
        ))
    }
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
    if range.start > range.end || range.end > source.len() {
        return Err(invalid("invalid search range"));
    }
    if let Some(limit) = pattern.source_limit {
        if pattern.newline_safe {
            return search_lines(source, pattern, range, mode, cancel, on_hit, progress);
        }
        if source.len() > limit as u64 {
            return Err(invalid(format!(
                "newline-consuming unbounded regex requires a source of at most {limit} bytes; use a bounded repetition such as .{{0,4096}} for larger files"
            )));
        }
    }
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

// One capped content line plus its preceding and following CR/LF delimiters.
// Delimiter context prevents artificial \A/\z and word-boundary matches.
struct LineSearch<'a, F> {
    pattern: &'a Pattern,
    range: Range<u64>,
    mode: SearchMode,
    cancel: &'a Cancellation,
    on_hit: F,
    result: SearchSummary,
    start: u64,
    len: u64,
    prefix: usize,
    bytes: Vec<u8>,
}

impl<F: FnMut(Match)> LineSearch<'_, F> {
    fn append(&mut self, bytes: &[u8]) {
        self.len += bytes.len() as u64;
        if self.len <= self.result.line_limit as u64 {
            self.bytes.extend_from_slice(bytes);
        } else {
            self.bytes.truncate(self.prefix);
        }
    }

    fn finish(&mut self, delimiter: &[u8], final_line: bool) -> io::Result<()> {
        let next = self.start + self.len + delimiter.len() as u64;
        if self.start <= self.range.end && (next > self.range.start || final_line && next == self.range.start) {
            if self.len > self.result.line_limit as u64 {
                self.result.skipped_long_lines = true;
            } else {
                self.bytes.extend_from_slice(delimiter);
                let base = self.start - self.prefix as u64;
                let from = (self.range.start.max(self.start) - base) as usize;
                self.pattern.visit(&self.bytes, from, |m| {
                    let offset = base + m.start as u64;
                    let end = base + m.end as u64;
                    if offset > self.range.end || offset >= next && !(final_line && offset == next) {
                        return Ok(false);
                    }
                    if self.cancel.load(Ordering::Relaxed) {
                        return Ok(false);
                    }
                    if end <= self.range.end {
                        self.result.count += 1;
                        if self.mode == SearchMode::Hits {
                            (self.on_hit)(Match { offset, len: end - offset });
                        }
                    }
                    Ok(true)
                })?;
            }
        }
        self.start = next;
        self.len = 0;
        self.prefix = delimiter.len();
        self.bytes.clear();
        self.bytes.extend_from_slice(delimiter);
        Ok(())
    }
}

fn search_lines<H: Haystack + ?Sized>(
    source: &H,
    pattern: &Pattern,
    range: Range<u64>,
    mode: SearchMode,
    cancel: &Cancellation,
    on_hit: impl FnMut(Match),
    mut progress: impl FnMut(Progress),
) -> io::Result<SearchSummary> {
    let limit = pattern.source_limit.expect("line search requires an unbounded pattern");
    // At most limit+4 outside either selection edge is enough to find the
    // complete intersecting line, or prove it exceeds the cap. No prefix replay.
    let lo = range.start.saturating_sub(limit as u64 + 4);
    let hi = range.end.saturating_add(limit as u64 + 4).min(source.len());
    let total = range.end - range.start;
    let mut state = LineSearch {
        pattern,
        range,
        mode,
        cancel,
        on_hit,
        result: SearchSummary { workers: 1, line_limit: limit, ..SearchSummary::default() },
        start: lo,
        len: 0,
        prefix: 0,
        bytes: Vec::with_capacity(limit + 4),
    };
    let mut expected = lo;
    let mut cr = false;
    for chunk in source.chunks(lo..hi)? {
        let (at, bytes) = chunk?;
        if at != expected || bytes.len() as u64 > hi - expected {
            return Err(invalid("invalid haystack chunks"));
        }
        expected += bytes.len() as u64;
        for part in bytes.chunks(BLOCK) {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let mut cursor = 0;
            if cr {
                let paired = part.first() == Some(&b'\n');
                state.finish(if paired { b"\r\n" } else { b"\r" }, false)?;
                cursor = usize::from(paired);
                cr = false;
            }
            for n in memchr::memchr2_iter(b'\r', b'\n', part) {
                if n < cursor || part[n] == b'\r' && !pattern.cr_safe {
                    continue;
                }
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                state.append(&part[cursor..n]);
                if part[n] == b'\r' && n + 1 == part.len() {
                    cr = true;
                    cursor = n + 1;
                    break;
                }
                let delimiter: &[u8] =
                    if part[n] == b'\r' && part.get(n + 1) == Some(&b'\n') { b"\r\n" } else { &part[n..n + 1] };
                state.finish(delimiter, false)?;
                cursor = n + delimiter.len();
            }
            if !cancel.load(Ordering::Relaxed) {
                state.append(&part[cursor..]);
            }
            state.result.scanned =
                (state.start + state.len + u64::from(cr)).saturating_sub(state.range.start).min(total);
            progress(Progress { scanned: state.result.scanned, total, count: state.result.count });
        }
        if cancel.load(Ordering::Relaxed) {
            break;
        }
    }
    if !cancel.load(Ordering::Relaxed) {
        if expected != hi {
            return Err(invalid("incomplete haystack chunks"));
        }
        if cr {
            state.finish(b"\r", false)?;
        }
        state.finish(b"", hi == source.len())?;
        state.result.scanned = total;
        progress(Progress { scanned: total, total, count: state.result.count });
    }
    state.result.cancelled = cancel.load(Ordering::Relaxed);
    Ok(state.result)
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
        out.extend_from_slice(&b);
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
                visit(&bytes, lo, range.end, true)?;
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
        let (at, owner) = chunk?;
        let mut chunk = owner.as_ref();
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
