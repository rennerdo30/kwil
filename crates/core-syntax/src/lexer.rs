use crate::{Class, Error, Language, Span, TextSource, read_chunk};
use std::ops::Range;

/// Deliberately window-local: a bounded lookbehind margin recovers nearby state,
/// but an opener farther away may be unrecognised. No scan from BOF on huge files.
pub(crate) fn highlight(
    source: &dyn TextSource,
    language: Language,
    window: Range<usize>,
    requested: Range<usize>,
    cancelled: &dyn Fn() -> bool,
) -> Result<Vec<Span>, Error> {
    let mut bytes = Vec::with_capacity(window.len());
    let mut offset = window.start;
    while offset < window.end {
        if cancelled() {
            return Err(Error::Cancelled);
        }
        let chunk = read_chunk(source, offset, (window.end - offset).min(16 * 1024))?;
        offset += chunk.len();
        bytes.extend_from_slice(&chunk);
    }
    let table = language.lexical();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if cancelled() {
            return Err(Error::Cancelled);
        }
        let start = i;
        let tail = &bytes[i..];
        let class;
        if !table.open.is_empty() && tail.starts_with(table.open.as_bytes()) {
            i += table.open.len();
            while i < bytes.len() && !bytes[i..].starts_with(table.close.as_bytes()) {
                i += 1;
            }
            i = bytes.len().min(i.saturating_add(table.close.len()));
            class = if table.open.contains('"') { Class::String } else { Class::Comment };
        } else if (!table.line.is_empty() && starts_comment(tail, table.line, language))
            || (language == Language::Ini && tail.starts_with(b"#"))
            || (language == Language::Php && tail.starts_with(b"#"))
        {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            class = Class::Comment;
        } else if language == Language::Markdown && (i == 0 || bytes[i - 1] == b'\n') && bytes[i] == b'#' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            class = Class::Heading;
        } else if matches!(bytes[i], b'\'' | b'"' | b'`') {
            let quote = bytes[i];
            let triple = language == Language::Python && bytes[i..].starts_with(&[quote; 3]);
            let width = if triple { 3 } else { 1 };
            i += width;
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i = bytes.len().min(i + 2);
                } else if bytes[i..].starts_with(&[quote; 3][..width]) {
                    i += width;
                    break;
                } else {
                    i += 1;
                }
            }
            class = Class::String;
        } else if bytes[i].is_ascii_digit() && (i == 0 || !word(bytes[i - 1])) {
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'.' | b'_')) {
                i += 1;
            }
            class = Class::Number;
        } else if word(bytes[i]) {
            i += 1;
            while i < bytes.len() && word(bytes[i]) {
                i += 1;
            }
            let token = &bytes[start..i];
            let insensitive = matches!(
                language,
                Language::Sql
                    | Language::Dockerfile
                    | Language::Batch
                    | Language::PowerShell
                    | Language::Ini
                    | Language::Yaml
            );
            if !table
                .keywords
                .split_whitespace()
                .any(|k| if insensitive { token.eq_ignore_ascii_case(k.as_bytes()) } else { token == k.as_bytes() })
            {
                continue;
            }
            class = Class::Keyword;
        } else {
            i += 1;
            continue;
        }
        let range = (window.start + start).max(requested.start)..(window.start + i).min(requested.end);
        if !range.is_empty() {
            spans.push(Span { range, class });
        }
    }
    Ok(spans)
}

fn word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte >= 128
}
fn starts_comment(tail: &[u8], marker: &str, language: Language) -> bool {
    if language == Language::Batch {
        tail.len() >= 3 && tail[..3].eq_ignore_ascii_case(b"rem") && tail.get(3).is_none_or(u8::is_ascii_whitespace)
    } else {
        tail.starts_with(marker.as_bytes())
    }
}
