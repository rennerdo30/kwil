//! Bounded text primitives. Display columns share the frontend grid traversal.
//! Invalid UTF-8 stays byte-exact and occupies one four-column escape per byte.

use kwil_core_text::Snapshot;
use std::{io, ops::Range};

pub(crate) const WINDOW: u64 = 64 << 10;

pub(crate) fn read(s: &Snapshot, range: Range<u64>, limit: u64) -> io::Result<Vec<u8>> {
    if range.end.saturating_sub(range.start) > limit {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "operation exceeds bounded text window"));
    }
    let mut result = Vec::with_capacity((range.end - range.start) as usize);
    for chunk in s.chunks(range)? {
        result.extend_from_slice(chunk.map_err(io::Error::other)?);
    }
    Ok(result)
}

pub(crate) fn byte(s: &Snapshot, at: u64) -> io::Result<Option<u8>> {
    if at >= s.len() {
        return Ok(None);
    }
    Ok(s.chunks(at..at + 1)?.next().transpose().map_err(io::Error::other)?.map(|c| c[0]))
}

pub(crate) fn character(s: &Snapshot, at: u64) -> io::Result<(char, u64)> {
    if at >= s.len() {
        return Ok(('\0', at));
    }
    let bytes = read(s, at..(at + 4).min(s.len()), 4)?;
    let width = match bytes[0] {
        0..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 1,
    };
    if width <= bytes.len()
        && let Ok(text) = std::str::from_utf8(&bytes[..width])
    {
        return Ok((text.chars().next().unwrap(), at + width as u64));
    }
    Ok(('\u{fffd}', at + 1))
}

pub(crate) fn next(s: &Snapshot, at: u64) -> io::Result<u64> {
    if byte(s, at)? == Some(b'\r') && byte(s, at + 1)? == Some(b'\n') {
        return Ok(at + 2);
    }
    Ok(character(s, at)?.1)
}

pub(crate) fn previous(s: &Snapshot, at: u64) -> io::Result<u64> {
    if at == 0 {
        return Ok(0);
    }
    if byte(s, at - 1)? == Some(b'\n') && at >= 2 && byte(s, at - 2)? == Some(b'\r') {
        return Ok(at - 2);
    }
    let start = at.saturating_sub(4);
    let bytes = read(s, start..at, 4)?;
    for i in (0..bytes.len()).rev() {
        if bytes[i] & 0xc0 != 0x80 {
            if std::str::from_utf8(&bytes[i..]).is_ok() {
                return Ok(start + i as u64);
            }
            break;
        }
    }
    Ok(at - 1)
}

pub(crate) fn row(s: &Snapshot, at: u64) -> io::Result<u64> {
    s.offset_to_line_col(at)
        .map_err(io::Error::other)?
        .map(|p| p.0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "cursor outside document"))
}

pub(crate) fn line(s: &Snapshot, row: u64) -> io::Result<Range<u64>> {
    let start = s.line_start(row).map_err(io::Error::other)?.unwrap_or(s.len());
    let mut end = s.line_start(row + 1).map_err(io::Error::other)?.map_or(s.len(), |at| at - 1);
    if end > start && byte(s, end - 1)? == Some(b'\r') {
        end -= 1;
    }
    Ok(start..end)
}

pub(crate) fn line_full(s: &Snapshot, row: u64) -> io::Result<Range<u64>> {
    let start = s.line_start(row).map_err(io::Error::other)?.unwrap_or(s.len());
    let end = s.line_start(row + 1).map_err(io::Error::other)?.unwrap_or(s.len());
    Ok(start..end)
}

pub(crate) fn column(s: &Snapshot, at: u64, tab: u8) -> io::Result<u64> {
    let bounds = line(s, row(s, at)?)?;
    let target = at.min(bounds.end) - bounds.start;
    if target == 0 {
        return Ok(0);
    }
    if target > WINDOW {
        return Err(column_limit());
    }
    let (bytes, complete) = column_bytes(s, bounds)?;
    let mut found = None;
    crate::grid::visit(&bytes, tab as usize, |_, range, col, width| {
        if target <= range.start as u64 {
            found = Some(col as u64);
            return false;
        }
        if !complete && range.end == bytes.len() {
            return false;
        }
        if target <= range.end as u64 {
            found = Some(if target == range.end as u64 { (col + width) as u64 } else { col as u64 });
            return false;
        }
        true
    });
    found.ok_or_else(column_limit)
}

pub(crate) fn at_column(s: &Snapshot, row: u64, target: u64, tab: u8) -> io::Result<u64> {
    let bounds = line(s, row)?;
    if target == 0 {
        return Ok(bounds.start);
    }
    let (bytes, complete) = column_bytes(s, bounds.clone())?;
    let mut found = None;
    crate::grid::visit(&bytes, tab as usize, |_, range, col, width| {
        if target == col as u64 {
            found = Some(range.start);
            return false;
        }
        if !complete && range.end == bytes.len() {
            return false;
        }
        if target <= (col + width) as u64 {
            found = Some(if target == (col + width) as u64 { range.end } else { range.start });
            return false;
        }
        true
    });
    match found {
        Some(offset) => Ok(bounds.start + offset as u64),
        None if complete => Ok(bounds.end),
        None => Err(column_limit()),
    }
}

fn column_limit() -> io::Error {
    io::Error::other("column exceeds bounded text window")
}

fn column_bytes(s: &Snapshot, bounds: Range<u64>) -> io::Result<(Vec<u8>, bool)> {
    let end = bounds.end.min(bounds.start.saturating_add(WINDOW));
    let mut bytes = read(s, bounds.start..end, WINDOW)?;
    let complete = end == bounds.end;
    if !complete {
        // Keep three bytes of lookahead inside the read budget to align scalars.
        // The last grapheme can continue beyond this window, so queries must
        // stop before using its width rather than mistake this cap for EOL.
        let end = crate::grid::boundary(&bytes, bytes.len() - 3);
        bytes.truncate(end);
    }
    Ok((bytes, complete))
}

fn class(c: char) -> u8 {
    if c.is_whitespace() {
        0
    } else if c.is_alphanumeric()
        || c == '_'
        || matches!(c as u32, 0x300..=0x36f | 0x1ab0..=0x1aff | 0x1dc0..=0x1dff | 0xfe20..=0xfe2f)
    {
        1
    } else {
        2
    }
}

pub(crate) fn word_forward(s: &Snapshot, at: u64) -> io::Result<u64> {
    let mut p = at;
    let initial = class(character(s, p)?.0);
    while p < s.len() && p - at < WINDOW && class(character(s, p)?.0) == initial {
        p = next(s, p)?;
    }
    while p < s.len() && p - at < WINDOW && class(character(s, p)?.0) == 0 {
        p = next(s, p)?;
    }
    Ok(p)
}

pub(crate) fn word_backward(s: &Snapshot, at: u64) -> io::Result<u64> {
    let mut p = previous(s, at)?;
    while p > 0 && at - p < WINDOW && class(character(s, p)?.0) == 0 {
        p = previous(s, p)?;
    }
    let initial = class(character(s, p)?.0);
    while p > 0 && at - p < WINDOW {
        let before = previous(s, p)?;
        if class(character(s, before)?.0) != initial {
            break;
        }
        p = before;
    }
    Ok(p)
}

pub(crate) fn word_range(s: &Snapshot, at: u64, around: bool) -> io::Result<Range<u64>> {
    let mut start = at.min(s.len());
    let initial = class(character(s, start)?.0);
    while start > 0 && at - start < WINDOW {
        let p = previous(s, start)?;
        if class(character(s, p)?.0) != initial {
            break;
        }
        start = p;
    }
    let mut end = at.min(s.len());
    while end < s.len() && end - start < WINDOW && class(character(s, end)?.0) == initial {
        end = next(s, end)?;
    }
    if around {
        while end < s.len() && end - start < WINDOW && class(character(s, end)?.0) == 0 {
            end = next(s, end)?;
        }
    }
    Ok(start..end)
}
