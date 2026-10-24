//! Bounded, allocation-free record metadata and viewport-local log highlighting.
//!
//! Offset-less `yyyy-MM-dd HH:mm:ss[,fraction]` log4j timestamps mean UTC.
//! ISO timestamps with `T` require `Z` or an explicit offset. Time-only records
//! can have a severity, but never receive an invented date. JSON metadata comes
//! only from literal, unescaped top-level keys, never strings or nested payloads.
use crate::{Class, Error, Span};
use std::ops::Range;

pub const MAX_PREFIX_BYTES: usize = 16_384;
// Independent of document/line size; exhaustion is explicit, never silent.
const MAX_SPANS: usize = 8192;
const MAX_JSON_DEPTH: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
}
impl Level {
    pub const ALL: &'static [Level] = &[Self::Trace, Self::Debug, Self::Info, Self::Warn, Self::Error, Self::Fatal];
    pub const fn bit(self) -> u8 {
        1 << self as u8
    }
    pub const fn label(self) -> &'static str {
        match self {
            Self::Trace => "TRACE",
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
            Self::Fatal => "FATAL",
        }
    }
    pub(crate) fn parse(bytes: &[u8]) -> Option<Self> {
        Self::ALL.iter().copied().find(|level| bytes.eq_ignore_ascii_case(level.label().as_bytes())).or_else(|| {
            if bytes.eq_ignore_ascii_case(b"warning") {
                Some(Self::Warn)
            } else if bytes.eq_ignore_ascii_case(b"critical") {
                Some(Self::Fatal)
            } else {
                None
            }
        })
    }
    pub(crate) fn class(self) -> Class {
        match self {
            Self::Error | Self::Fatal => Class::Error,
            Self::Warn => Class::Constant,
            _ => Class::Keyword,
        }
    }
}

/// Ordered integer nanoseconds since the Unix epoch, UTC (no floating point).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(pub i128);

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Metadata {
    pub level: Option<Level>,
    pub timestamp: Option<Timestamp>,
}

/// Parse an entire timestamp, at most 40 bytes. No whitespace trimming, guessing
/// of dates, leap-second normalization or fractional truncation is performed.
/// Unix syntax: signed/unsigned 10/13/16/19 digit integers (s/ms/us/ns), or
/// 10-digit seconds with 1..=9 fractional digits. Short ambiguous integers,
/// exponent notation and excess precision are rejected.
pub fn parse_timestamp(text: &str) -> Option<Timestamp> {
    let b = text.as_bytes();
    if b.is_empty() || b.len() > 40 {
        return None;
    }
    if b.get(4) != Some(&b'-') {
        return unix(b);
    }
    if b.len() < 19 || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let year = digits(&b[..4])? as i64;
    let month = digits(&b[5..7])? as i64;
    let day = digits(&b[8..10])? as i64;
    let hour = digits(&b[11..13])?;
    let minute = digits(&b[14..16])?;
    let second = digits(&b[17..19])?;
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => return None,
    };
    if !(1..=days).contains(&day) || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let mut at = 19;
    let fraction = fraction(b, &mut at)?;
    let offset = match b.get(at) {
        Some(b'Z' | b'z') if at + 1 == b.len() => 0,
        Some(b'+' | b'-') => {
            let sign = if b[at] == b'+' { 1 } else { -1 };
            let rest = &b[at + 1..];
            let (h, m) = match rest.len() {
                5 if rest[2] == b':' => (digits(&rest[..2])?, digits(&rest[3..])?),
                4 => (digits(&rest[..2])?, digits(&rest[2..])?),
                _ => return None,
            };
            if h > 23 || m > 59 {
                return None;
            }
            sign * (h * 3600 + m * 60)
        }
        None if b[10] == b' ' => 0,
        _ => return None,
    };
    // Gregorian civil date to days since 1970-01-01; Euclidean division also
    // handles January/February of year 0000 without unsigned underflow.
    let y = year - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yo = y - era * 400;
    let mp = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let days = era * 146097 + yo * 365 + yo / 4 - yo / 100 + doy - 719468;
    Some(Timestamp((days as i128 * 86400 + hour * 3600 + minute * 60 + second - offset) * 1_000_000_000 + fraction))
}

fn digits(bytes: &[u8]) -> Option<i128> {
    if bytes.is_empty() {
        return None;
    }
    bytes.iter().try_fold(0i128, |n, &b| b.is_ascii_digit().then(|| n * 10 + (b - b'0') as i128))
}
fn fraction(bytes: &[u8], at: &mut usize) -> Option<i128> {
    if !matches!(bytes.get(*at), Some(b'.' | b',')) {
        return Some(0);
    }
    *at += 1;
    let start = *at;
    while bytes.get(*at).is_some_and(u8::is_ascii_digit) {
        *at += 1;
    }
    let len = *at - start;
    if !(1..=9).contains(&len) {
        return None;
    }
    Some(digits(&bytes[start..*at])? * 10i128.pow(9 - len as u32))
}
fn unix(bytes: &[u8]) -> Option<Timestamp> {
    let negative = bytes.first() == Some(&b'-');
    let b = if matches!(bytes.first(), Some(b'+' | b'-')) { &bytes[1..] } else { bytes };
    let count = b.iter().take_while(|b| b.is_ascii_digit()).count();
    if !matches!(count, 10 | 13 | 16 | 19) {
        return None;
    }
    let whole = digits(&b[..count])?;
    let value = if count == b.len() {
        whole
            * match count {
                10 => 1_000_000_000,
                13 => 1_000_000,
                16 => 1000,
                _ => 1,
            }
    } else {
        if count != 10 || b[count] != b'.' {
            return None;
        }
        let mut at = count;
        let frac = fraction(b, &mut at)?;
        if at != b.len() {
            return None;
        }
        whole * 1_000_000_000 + frac
    };
    Some(Timestamp(if negative { -value } else { value }))
}

/// Inspect only the first record's first MAX_PREFIX_BYTES bytes. No allocation,
/// source reads or scan into the message for severity. Unknown stays `None`.
pub fn metadata(prefix: &[u8]) -> Metadata {
    let bounded = &prefix[..prefix.len().min(MAX_PREFIX_BYTES)];
    let end = bounded.iter().position(|b| matches!(b, b'\r' | b'\n')).unwrap_or(bounded.len());
    let line = &bounded[..end];
    let complete = end < bounded.len() || prefix.len() <= MAX_PREFIX_BYTES;
    let start = spaces(line, bom(line));
    if line.get(start) == Some(&b'{') {
        let mut result = Metadata::default();
        // Infallible metadata sink; cancellation is not needed for <=16 KiB.
        let _ = json(line, start, complete, &|| false, &mut |_, _| Ok(()), Some(&mut result));
        result
    } else {
        header(line, complete).metadata
    }
}

#[derive(Clone, Copy)]
struct Mark {
    start: usize,
    end: usize,
    class: Class,
}
#[derive(Default)]
struct Header {
    metadata: Metadata,
    marks: [Option<Mark>; 8],
    count: usize,
    end: usize,
}
impl Header {
    fn mark(&mut self, start: usize, end: usize, class: Class) {
        if self.count < self.marks.len() {
            self.marks[self.count] = Some(Mark { start, end, class });
            self.count += 1;
        }
        self.end = end;
    }
}
fn bom(bytes: &[u8]) -> usize {
    if bytes.starts_with(b"\xef\xbb\xbf") { 3 } else { 0 }
}
fn spaces(bytes: &[u8], mut at: usize) -> usize {
    while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
        at += 1;
    }
    at
}
fn boundary(b: Option<&u8>) -> bool {
    b.is_none_or(|b| b.is_ascii_whitespace() || matches!(b, b']' | b')' | b':' | b'|'))
}
fn timestamp_at(bytes: &[u8], start: usize, complete: bool) -> Option<(usize, Timestamp)> {
    let mut end = start;
    // The single internal space in log4j's date/time is significant.
    while end < bytes.len() && end - start < 40 {
        let b = bytes[end];
        if b.is_ascii_digit()
            || matches!(b, b'-' | b'+' | b'T' | b't' | b'Z' | b'z' | b':' | b'.' | b',')
            || (end == start + 10 && b == b' ')
        {
            end += 1;
        } else {
            break;
        }
    }
    if !boundary(bytes.get(end)) || (end == bytes.len() && !complete) {
        return None;
    }
    Some((end, parse_timestamp(std::str::from_utf8(&bytes[start..end]).ok()?)?))
}
fn level_at(bytes: &[u8], at: usize, complete: bool) -> Option<(usize, Level)> {
    let len = bytes[at..].iter().take_while(|b| b.is_ascii_alphabetic()).count();
    let end = at + len;
    if !boundary(bytes.get(end)) || (end == bytes.len() && !complete) {
        return None;
    }
    Some((end, Level::parse(&bytes[at..end])?))
}
fn clock_at(bytes: &[u8], start: usize, complete: bool) -> Option<usize> {
    let b = bytes.get(start..start.checked_add(8)?)?;
    if b[2] != b':' || b[5] != b':' || digits(&b[..2])? > 23 || digits(&b[3..5])? > 59 || digits(&b[6..])? > 59 {
        return None;
    }
    let mut end = start + 8;
    fraction(bytes, &mut end)?;
    (boundary(bytes.get(end)) && (end < bytes.len() || complete)).then_some(end)
}
fn thread_token(bytes: &[u8]) -> bool {
    !matches!(bytes, b"true" | b"false" | b"null")
        && bytes.iter().any(u8::is_ascii_alphabetic)
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'#' | b'/' | b'@' | b' '))
}
fn header(bytes: &[u8], complete: bool) -> Header {
    let mut h = Header::default();
    let mut at = spaces(bytes, bom(bytes));
    let wrapped = bytes.get(at) == Some(&b'[');
    let start = at + usize::from(wrapped);
    if let Some((end, stamp)) = timestamp_at(bytes, start, complete)
        && (!wrapped || bytes.get(end) == Some(&b']'))
    {
        h.metadata.timestamp = Some(stamp);
        h.mark(start, end, Class::Number);
        at = spaces(bytes, end + usize::from(wrapped));
    } else if let Some(end) = clock_at(bytes, start, complete)
        && (!wrapped || bytes.get(end) == Some(&b']'))
    {
        // Clock styling/level parsing is useful without inventing a date.
        h.mark(start, end, Class::Number);
        at = spaces(bytes, end + usize::from(wrapped));
    }
    // A bracketed thread may precede a level, but arbitrary message words may
    // not. Keep tentative marks only when a following level proves the header.
    let tentative = h.count;
    for _ in 0..4 {
        let bracket = bytes.get(at) == Some(&b'[');
        let token = at + usize::from(bracket);
        if let Some((end, level)) = level_at(bytes, token, complete)
            && (!bracket || bytes.get(end) == Some(&b']'))
        {
            h.metadata.level = Some(level);
            h.mark(token, end, level.class());
            at = spaces(bytes, end + usize::from(bracket));
            break;
        }
        if !bracket {
            break;
        }
        let Some(close) = bytes[token..].iter().position(|&b| b == b']') else {
            break;
        };
        if !thread_token(&bytes[token..token + close]) {
            break;
        }
        h.mark(token, token + close, Class::Variable);
        at = spaces(bytes, token + close + 1);
    }
    if h.metadata.level.is_none() {
        h.count = tentative;
        h.end = h.marks[..tentative].iter().flatten().last().map_or(0, |m| m.end);
        return h;
    }
    if bytes.get(at) == Some(&b'[') {
        let start = at + 1;
        if let Some(close) = bytes[start..].iter().position(|&b| b == b']')
            && thread_token(&bytes[start..start + close])
        {
            h.mark(start, start + close, Class::Variable);
            at = spaces(bytes, start + close + 1);
        }
    }
    // A logger name is structural only when followed by the standard separator.
    let start = at;
    while bytes.get(at).is_some_and(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'$' | b'-' | b'/')) {
        at += 1;
    }
    let next = spaces(bytes, at);
    let token = &bytes[start..at];
    if at > start
        && !token.ends_with(b"Exception")
        && !token.ends_with(b"Error")
        && (bytes.get(next) == Some(&b':') || bytes.get(next..next.saturating_add(2)) == Some(b"- "))
    {
        h.mark(start, at, Class::Type);
    }
    h
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    None,
    Level,
    Timestamp,
    Thread,
    Logger,
}
fn field(key: &[u8]) -> Field {
    if key.eq_ignore_ascii_case(b"level") || key.eq_ignore_ascii_case(b"severity") {
        Field::Level
    } else if [b"timestamp".as_slice(), b"time", b"ts", b"@timestamp"].iter().any(|k| key.eq_ignore_ascii_case(k)) {
        Field::Timestamp
    } else if key.eq_ignore_ascii_case(b"thread") || key.eq_ignore_ascii_case(b"thread_name") {
        Field::Thread
    } else if key.eq_ignore_ascii_case(b"logger") || key.eq_ignore_ascii_case(b"logger_name") {
        Field::Logger
    } else {
        Field::None
    }
}

// Fixed-depth streaming token scanner, not a JSON DOM or full validator. It
// stops on malformed structure or depth >32, retaining only proven tokens.
// Object states: 0 key/end, 1 colon, 2 value, 3 comma/end; array: 2/3.
fn json(
    bytes: &[u8],
    start: usize,
    complete: bool,
    cancelled: &dyn Fn() -> bool,
    emit: &mut dyn FnMut(Range<usize>, Class) -> Result<(), Error>,
    mut metadata: Option<&mut Metadata>,
) -> Result<usize, Error> {
    let mut kinds = [0u8; MAX_JSON_DEPTH];
    let mut states = [0u8; MAX_JSON_DEPTH];
    let mut depth = 0;
    let mut at = start;
    let mut pending = Field::None;
    loop {
        if cancelled() {
            return Err(Error::Cancelled);
        }
        at = spaces(bytes, at);
        let Some(&b) = bytes.get(at) else {
            return Ok(at);
        };
        let begin = at;
        if b == b'{' || b == b'[' {
            if depth == MAX_JSON_DEPTH || (depth > 0 && states[depth - 1] != 2) {
                return Ok(bytes.len());
            }
            if depth > 0 {
                states[depth - 1] = 3;
            }
            kinds[depth] = b;
            states[depth] = if b == b'{' { 0 } else { 2 };
            depth += 1;
            pending = Field::None;
            at += 1;
            emit(begin..at, Class::Punctuation)?;
        } else if depth == 0 {
            return Ok(at);
        } else if b == b'}' || b == b']' {
            if kinds[depth - 1] != if b == b'}' { b'{' } else { b'[' } || !matches!(states[depth - 1], 0 | 2 | 3) {
                return Ok(bytes.len());
            }
            depth -= 1;
            pending = Field::None;
            at += 1;
            emit(begin..at, Class::Punctuation)?;
            if depth == 0 {
                return Ok(at);
            }
        } else if b == b':' {
            if states[depth - 1] != 1 {
                return Ok(bytes.len());
            }
            states[depth - 1] = 2;
            at += 1;
            emit(begin..at, Class::Punctuation)?;
        } else if b == b',' {
            if states[depth - 1] != 3 {
                return Ok(bytes.len());
            }
            states[depth - 1] = if kinds[depth - 1] == b'{' { 0 } else { 2 };
            pending = Field::None;
            at += 1;
            emit(begin..at, Class::Punctuation)?;
        } else if b == b'"' {
            let (end, closed, escaped) = string_end(bytes, at, cancelled)?;
            at = end;
            let key = kinds[depth - 1] == b'{' && states[depth - 1] == 0;
            if !key && states[depth - 1] != 2 {
                return Ok(bytes.len());
            }
            let mut class = if key { Class::Property } else { Class::String };
            if key {
                pending =
                    if depth == 1 && closed && !escaped { field(&bytes[begin + 1..end - 1]) } else { Field::None };
                states[depth - 1] = 1;
            } else {
                if closed && !escaped && depth == 1 {
                    let value = &bytes[begin + 1..end - 1];
                    match pending {
                        Field::Level => {
                            if let Some(level) = Level::parse(value) {
                                class = level.class();
                                if let Some(m) = metadata.as_deref_mut() {
                                    m.level.get_or_insert(level);
                                }
                            }
                        }
                        Field::Timestamp => {
                            if let Some(stamp) = std::str::from_utf8(value).ok().and_then(parse_timestamp) {
                                class = Class::Number;
                                if let Some(m) = metadata.as_deref_mut() {
                                    m.timestamp.get_or_insert(stamp);
                                }
                            }
                        }
                        Field::Thread => class = Class::Variable,
                        Field::Logger => class = Class::Type,
                        Field::None => {}
                    }
                }
                pending = Field::None;
                states[depth - 1] = 3;
            }
            emit(begin..at, class)?;
            if !closed {
                return Ok(at);
            }
        } else {
            if states[depth - 1] != 2 {
                return Ok(bytes.len());
            }
            while at < bytes.len() && !bytes[at].is_ascii_whitespace() && !matches!(bytes[at], b',' | b'}' | b']') {
                at += 1;
            }
            let token = &bytes[begin..at];
            let number = json_number(token);
            if !number && !matches!(token, b"true" | b"false" | b"null") {
                return Ok(bytes.len());
            }
            if number
                && depth == 1
                && pending == Field::Timestamp
                && (at < bytes.len() || complete)
                && let Some(stamp) = std::str::from_utf8(token).ok().and_then(parse_timestamp)
                && let Some(m) = metadata.as_deref_mut()
            {
                m.timestamp.get_or_insert(stamp);
            }
            emit(begin..at, if number { Class::Number } else { Class::Constant })?;
            pending = Field::None;
            states[depth - 1] = 3;
        }
    }
}

fn string_end(bytes: &[u8], start: usize, cancelled: &dyn Fn() -> bool) -> Result<(usize, bool, bool), Error> {
    let mut at = start + 1;
    let mut escaped = false;
    while at < bytes.len() {
        if at.is_multiple_of(1024) && cancelled() {
            return Err(Error::Cancelled);
        }
        match bytes[at] {
            b'"' => return Ok((at + 1, true, escaped)),
            b'\\' => {
                escaped = true;
                at = bytes.len().min(at + 2);
            }
            _ => at += 1,
        }
    }
    Ok((at, false, escaped))
}
fn json_number(bytes: &[u8]) -> bool {
    let mut at = usize::from(bytes.first() == Some(&b'-'));
    if bytes.get(at) == Some(&b'0') {
        at += 1;
    } else {
        if !bytes.get(at).is_some_and(|b| matches!(b, b'1'..=b'9')) {
            return false;
        }
        while bytes.get(at).is_some_and(u8::is_ascii_digit) {
            at += 1;
        }
    }
    if bytes.get(at) == Some(&b'.') {
        at += 1;
        let start = at;
        while bytes.get(at).is_some_and(u8::is_ascii_digit) {
            at += 1;
        }
        if at == start {
            return false;
        }
    }
    if matches!(bytes.get(at), Some(b'e' | b'E')) {
        at += 1;
        if matches!(bytes.get(at), Some(b'+' | b'-')) {
            at += 1;
        }
        let start = at;
        while bytes.get(at).is_some_and(u8::is_ascii_digit) {
            at += 1;
        }
        if at == start {
            return false;
        }
    }
    at == bytes.len()
}

fn json_candidate(bytes: &[u8], at: usize) -> bool {
    let next = spaces(bytes, at + 1);
    let Some(&first) = bytes.get(next) else {
        return true;
    };
    if bytes[at] == b'{' {
        return matches!(first, b'"' | b'}');
    }
    if matches!(first, b'"' | b'{' | b'[' | b']' | b'-' | b'0'..=b'9') {
        return true;
    }
    [b"true".as_slice(), b"false", b"null"].iter().any(|literal| {
        bytes[next..].starts_with(literal)
            && bytes.get(next + literal.len()).is_none_or(|b| b.is_ascii_whitespace() || matches!(b, b',' | b']'))
    })
}

pub(crate) fn highlight(
    bytes: &[u8],
    base: usize,
    requested: Range<usize>,
    cancelled: &dyn Fn() -> bool,
) -> Result<Vec<Span>, Error> {
    let mut spans = Vec::new();
    let mut emit = |range: Range<usize>, class| {
        let range = (base + range.start).max(requested.start)..(base + range.end).min(requested.end);
        if !range.is_empty() {
            if spans.len() == MAX_SPANS {
                return Err(Error::QueryLimit);
            }
            spans.push(Span { range, class });
        }
        Ok(())
    };
    let mut line_start = 0;
    for line in bytes.split_inclusive(|b| matches!(*b, b'\r' | b'\n')) {
        if cancelled() {
            return Err(Error::Cancelled);
        }
        let mut at = 0;
        // A viewport can begin mid-message. Never reinterpret its first byte as
        // a record header. The next physical newline restores header framing.
        if base == 0 || line_start != 0 {
            let prefix_len = line.len().min(MAX_PREFIX_BYTES);
            let h = header(&line[..prefix_len], line.len() <= MAX_PREFIX_BYTES);
            for mark in h.marks[..h.count].iter().flatten() {
                emit(line_start + mark.start..line_start + mark.end, mark.class)?;
            }
            at = h.end;
        }
        let mut local_emit = |range: Range<usize>, class| emit(line_start + range.start..line_start + range.end, class);
        let first = spaces(line, at);
        // Stack continuations are line-anchored; words in normal prose are not.
        if line[first..].starts_with(b"at ")
            || line[first..].starts_with(b"Caused by:")
            || line[first..].starts_with(b"Suppressed:")
            || line[first..].starts_with(b"Traceback (")
        {
            let end = line.len() - usize::from(matches!(line.last(), Some(b'\r' | b'\n')));
            local_emit(first..end, Class::Error)?;
        } else {
            while at < line.len() {
                if cancelled() {
                    return Err(Error::Cancelled);
                }
                match line[at] {
                    b'{' | b'[' if json_candidate(line, at) => {
                        at = json(line, at, true, cancelled, &mut local_emit, None)?;
                    }
                    b'"' => {
                        let end = string_end(line, at, cancelled)?.0;
                        local_emit(at..end, Class::String)?;
                        at = end;
                    }
                    b if b.is_ascii_alphabetic() => {
                        let start = at;
                        while line.get(at).is_some_and(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'$'))
                        {
                            at += 1;
                        }
                        let token = &line[start..at];
                        if (token.ends_with(b"Exception") || token.ends_with(b"Error"))
                            && (line.get(at) == Some(&b':') || start == first)
                        {
                            local_emit(start..at, Class::Error)?;
                        }
                    }
                    _ => at += 1,
                }
            }
        }
        line_start += line.len();
    }
    Ok(spans)
}
