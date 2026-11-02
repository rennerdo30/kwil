//! Log control glue shared by frontend hosts. Matching, metadata detection and
//! follow indexing remain in the existing worker services.
use crate::log_session::{Filter, TextQuery, ViewportRow};
use unicode_segmentation::UnicodeSegmentation;

pub enum FieldInput<'a> {
    Text(&'a str),
    Left,
    Right,
    Home,
    End,
    Backspace,
    Delete,
    SelectAll,
}

/// One bounded single-line control, with a caret on a grapheme boundary.
/// Hosts retain the text so they can restore their last applied fields on Esc.
pub fn edit_field(
    text: &mut String,
    cursor: &mut usize,
    all: &mut bool,
    input: FieldInput<'_>,
) -> Result<(), &'static str> {
    let left = text.grapheme_indices(true).map(|(at, _)| at).take_while(|at| *at < *cursor).last().unwrap_or(0);
    let right = text.grapheme_indices(true).map(|(at, _)| at).find(|at| *at > *cursor).unwrap_or(text.len());
    match input {
        FieldInput::Text(value) => {
            if value.is_empty() {
                return Ok(());
            }
            if value.chars().any(char::is_control) {
                return Err("Log fields accept one line of text");
            }
            if (if *all { 0 } else { text.len() }) + value.len() > 256 {
                return Err("Log field is too long (256 UTF-8 bytes maximum)");
            }
            if *all {
                text.clear();
                *cursor = 0;
            }
            text.insert_str(*cursor, value);
            *cursor += value.len();
        }
        FieldInput::Backspace | FieldInput::Delete if *all => {
            text.clear();
            *cursor = 0;
        }
        FieldInput::Backspace => {
            text.replace_range(left..*cursor, "");
            *cursor = left;
        }
        FieldInput::Delete => {
            text.replace_range(*cursor..right, "");
        }
        FieldInput::Left => *cursor = left,
        FieldInput::Right => *cursor = right,
        FieldInput::Home => *cursor = 0,
        FieldInput::End => *cursor = text.len(),
        FieldInput::SelectAll => {
            *all = true;
            return Ok(());
        }
    }
    *all = false;
    // Insertion/deletion may join adjacent graphemes; snap forward once.
    *cursor = text.grapheme_indices(true).map(|(at, _)| at).find(|at| *at >= *cursor).unwrap_or(text.len());
    Ok(())
}

/// Validate bounded controls without compiling or running a query on the UI.
pub fn filter_from_fields(
    mut filter: Filter,
    query: &str,
    regex: bool,
    from: &str,
    to: &str,
    keep_unknown: Option<bool>,
) -> Result<Filter, &'static str> {
    let parse = |text: &str| {
        if text.trim().is_empty() {
            Ok(None)
        } else {
            kwil_core_syntax::logs::parse_timestamp(text.trim())
                .map(Some)
                .ok_or("Use an ISO date/time with timezone, log4j UTC time, or Unix timestamp")
        }
    };
    filter.from = parse(from)?;
    filter.to = parse(to)?;
    filter.keep_unknown = keep_unknown.unwrap_or(filter.from.is_none() && filter.to.is_none());
    if filter.from.zip(filter.to).is_some_and(|(a, b)| a > b) {
        return Err("Time range end precedes start");
    }
    filter.text = TextQuery::new(query, regex)?;
    Ok(filter)
}

/// A projection can expose only a byte window. Copy must never silently lose
/// a prefix, suffix, invalid byte or NUL. This borrows the worker's bounded row.
pub fn copy_text(row: &ViewportRow) -> Result<&str, &'static str> {
    if row.offset != row.line_start || row.offset.checked_add(row.bytes.len() as u64) != Some(row.line_end) {
        return Err("Only a byte window is loaded; open original to copy the full row");
    }
    let text = std::str::from_utf8(&row.bytes).map_err(|_| "Invalid UTF-8; open original to copy bytes")?;
    if text.contains('\0') {
        return Err("NUL cannot be preserved by the system clipboard");
    }
    Ok(text)
}
