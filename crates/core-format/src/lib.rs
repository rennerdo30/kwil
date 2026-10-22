//! Bounded formatting for the editor's worker pool. No UI or drawing dependencies.
//!
//! Call `format` on a worker with a versioned buffer snapshot. Apply `replacement`
//! and `bytes` together in ONE editor transaction, only if that version is current.
//! The caller supplies only the admitted document/selection, never copies a huge
//! file just to discover the size guard. All limits have non-overridable ceilings.

mod external;
mod languages;
mod process_tree;

pub use external::ExternalFormatter;
use std::{collections::BTreeMap, fmt, ops::Range};

pub const MAX_INPUT_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    Json,
    Xml,
    Html,
    Yaml,
    Toml,
    Css,
    Sql,
    Generic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Indent {
    Spaces(usize),
    Tabs { width: usize },
}
impl Indent {
    fn width(self) -> usize {
        match self {
            Self::Spaces(n) | Self::Tabs { width: n } => n,
        }
    }
    fn unit(self) -> String {
        match self {
            Self::Spaces(n) => " ".repeat(n),
            Self::Tabs { .. } => "\t".into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub indent: Indent,
    /// Generic reindent is opt-in: leading whitespace can be significant.
    pub reindent: bool,
    pub trim_trailing_whitespace: bool,
    pub normalize_line_endings: bool,
    pub final_newline: bool,
    pub max_input_bytes: usize,
    pub max_output_bytes: usize,
    /// Exact file-type keys; no process is started without an entry.
    pub external: BTreeMap<String, ExternalFormatter>,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            indent: Indent::Spaces(2),
            reindent: false,
            trim_trailing_whitespace: true,
            normalize_line_endings: true,
            final_newline: true,
            max_input_bytes: MAX_INPUT_BYTES,
            max_output_bytes: MAX_OUTPUT_BYTES,
            external: BTreeMap::new(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct FormatError {
    pub message: String,
    pub line: usize,
}
impl FormatError {
    pub(crate) fn new(kind: &str, message: impl fmt::Display, line: usize) -> Self {
        Self { message: format!("{kind}: {message} at line {line}"), line }
    }
}
impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for FormatError {}

#[derive(Debug, PartialEq, Eq)]
pub struct FormatResult {
    /// Replacement payload only; untouched parts of the document are never copied.
    pub bytes: Vec<u8>,
    /// One replacement, applied with `bytes` in one undo transaction; None = no edit.
    pub replacement: Option<Range<usize>>,
    pub notices: Vec<String>,
}

/// Synchronous worker entry point. `selection` uses UTF-8 byte boundaries in
/// `input`. The editor must check document size BEFORE materializing `input`.
pub fn format(
    input: &[u8],
    language: Language,
    file_type: &str,
    selection: Option<Range<usize>>,
    settings: &Settings,
) -> Result<FormatResult, FormatError> {
    check_size(input.len(), settings)?;
    let text = std::str::from_utf8(input)
        .map_err(|e| FormatError::new("Format", "input is not UTF-8", line_at(input, e.valid_up_to())))?;
    let selected = selection.is_some();
    let range = selection.unwrap_or(0..input.len());
    let fragment =
        text.get(range.clone()).ok_or_else(|| FormatError::new("Format", "invalid selection byte range", 1))?;
    if selected
        && (!matches!(language, Language::Json | Language::Xml | Language::Html | Language::Generic)
            || settings.external.contains_key(file_type))
    {
        return Err(FormatError::new("Format", "not supported for a selection", line_at(input, range.start)));
    }
    let offset = line_at(input, range.start) - 1;
    // These tidy-only formats have semantic whitespace, including embedded code
    // and EOF. Without a parser proving a region safe, retain its exact bytes.
    let preserve_tidy = language == Language::Yaml
        || (language == Language::Generic
            && matches!(
                file_type.trim_start_matches('.').to_ascii_lowercase().as_str(),
                "markdown"
                    | "md"
                    | "mdown"
                    | "mkd"
                    | "mkdn"
                    | "commonmark"
                    | "mdx"
                    | "rst"
                    | "restructuredtext"
                    | "makefile"
                    | "make"
                    | "diff"
                    | "patch"
            ));
    let mut notices = Vec::new();
    let result = (|| {
        let formatted = if let Some(command) = settings.external.get(file_type) {
            external::run(fragment, command, settings.max_output_bytes)?
        } else {
            match language {
                Language::Json => languages::json(fragment, settings)?,
                Language::Xml | Language::Html => {
                    languages::markup(fragment, language == Language::Html, selected, settings)?
                }
                Language::Toml => languages::toml(fragment)?,
                Language::Css => languages::css(fragment, settings)?,
                Language::Sql => languages::sql(fragment, settings)?,
                Language::Yaml => {
                    notices.push(
                        "YAML: tidy only, no structural YAML formatting yet; indentation and trailing whitespace preserved to protect scalar semantics"
                            .into(),
                    );
                    tidy(fragment, settings, true)?
                }
                Language::Generic => tidy(fragment, settings, preserve_tidy)?,
            }
        };
        // Syntax-aware formatters preserve all literal whitespace. Only generic
        // tidy trims; normalizing structured literals would change their values.
        let mut output = Output::new(settings.max_output_bytes);
        output.push(&formatted)?;
        if !selected && !preserve_tidy && settings.final_newline && !formatted.ends_with(['\n', '\r']) {
            output.push(ending(text))?;
        }
        let bytes = output.text.into_bytes();
        Ok(FormatResult { replacement: (bytes != fragment.as_bytes()).then_some(range), bytes, notices })
    })();
    result.map_err(|mut e: FormatError| {
        if offset > 0 {
            e.line += offset;
            e.message = format!("{} (document line {})", e.message, e.line);
        }
        e
    })
}

/// Admission probe for callers holding a file-backed snapshot.
pub fn check_size(size: usize, settings: &Settings) -> Result<(), FormatError> {
    if !(1..=16).contains(&settings.indent.width())
        || settings.max_input_bytes > MAX_INPUT_BYTES
        || settings.max_output_bytes > MAX_OUTPUT_BYTES
    {
        return Err(FormatError::new("Format", "invalid settings or limits exceed hard memory ceiling", 1));
    }
    if size > settings.max_input_bytes {
        return Err(FormatError::new(
            "Format",
            format!("input exceeds {} byte formatting limit; use a smaller selection", settings.max_input_bytes),
            1,
        ));
    }
    Ok(())
}

/// Format an already-admitted small selection from a file of ANY size. The
/// caller reads only these bytes from its snapshot. `start_byte` and `start_line`
/// locate the fragment in the original document; no surrounding text is needed.
pub fn format_selection(
    input: &[u8],
    language: Language,
    file_type: &str,
    start_byte: usize,
    start_line: usize,
    settings: &Settings,
) -> Result<FormatResult, FormatError> {
    let end = start_byte
        .checked_add(input.len())
        .ok_or_else(|| FormatError::new("Format", "selection offset overflow", start_line))?;
    let mut result = format(input, language, file_type, Some(0..input.len()), settings).map_err(|mut e| {
        e.line = e.line.saturating_add(start_line.saturating_sub(1));
        e.message = format!("{} (document line {})", e.message, e.line);
        e
    })?;
    if result.replacement.is_some() {
        result.replacement = Some(start_byte..end);
    }
    Ok(result)
}

pub(crate) fn line_at(bytes: &[u8], pos: usize) -> usize {
    1 + bytes[..pos.min(bytes.len())].iter().filter(|&&b| b == b'\n').count()
}
pub(crate) fn ending(text: &str) -> &'static str {
    match text.bytes().position(|b| b == b'\r' || b == b'\n') {
        Some(i) if text.as_bytes()[i] == b'\r' => {
            if text.as_bytes().get(i + 1) == Some(&b'\n') {
                "\r\n"
            } else {
                "\r"
            }
        }
        _ => "\n",
    }
}

pub(crate) struct Output {
    pub text: String,
    cap: usize,
}
impl Output {
    pub fn new(cap: usize) -> Self {
        Self { text: String::new(), cap }
    }
    pub fn push(&mut self, s: &str) -> Result<(), FormatError> {
        if s.len() > self.cap.saturating_sub(self.text.len()) {
            return Err(FormatError::new("Format", "output exceeds formatting memory limit", 1));
        }
        // Exact reserve avoids geometric growth beyond the configured ceiling.
        let required = self.text.len() + s.len();
        if required > self.text.capacity() {
            let target = required.max(self.text.capacity().saturating_mul(2)).max(1024).min(self.cap);
            self.text.reserve_exact(target - self.text.len());
        }
        self.text.push_str(s);
        Ok(())
    }
    pub fn indent(&mut self, depth: usize, settings: &Settings) -> Result<(), FormatError> {
        if depth > 128 {
            return Err(FormatError::new("Format", "nesting exceeds 128 levels", 1));
        }
        for _ in 0..depth {
            self.push(&settings.indent.unit())?;
        }
        Ok(())
    }
}

fn tidy(text: &str, settings: &Settings, preserve: bool) -> Result<String, FormatError> {
    let mut out = Output::new(settings.max_output_bytes);
    if preserve {
        out.push(text)?;
        return Ok(out.text);
    }
    let eol = ending(text);
    let mut start = 0;
    let bytes = text.as_bytes();
    while start < bytes.len() {
        let end = (start..bytes.len()).find(|&i| matches!(bytes[i], b'\r' | b'\n')).unwrap_or(bytes.len());
        let mut next = end;
        if next < bytes.len() {
            next += 1;
            if bytes[end] == b'\r' && bytes.get(next) == Some(&b'\n') {
                next += 1;
            }
        }
        let line = &text[start..end];
        let line = if settings.trim_trailing_whitespace { line.trim_end_matches([' ', '\t']) } else { line };
        if settings.reindent {
            let leading = line.len() - line.trim_start_matches([' ', '\t']).len();
            let mut columns = 0;
            for b in line[..leading].bytes() {
                columns += if b == b'\t' { settings.indent.width() - columns % settings.indent.width() } else { 1 };
            }
            match settings.indent {
                Indent::Spaces(_) => {
                    for _ in 0..columns {
                        out.push(" ")?;
                    }
                }
                Indent::Tabs { width } => {
                    for _ in 0..columns / width {
                        out.push("\t")?;
                    }
                    for _ in 0..columns % width {
                        out.push(" ")?;
                    }
                }
            }
            out.push(&line[leading..])?;
        } else {
            out.push(line)?;
        }
        if next > end {
            out.push(if settings.normalize_line_endings { eol } else { &text[end..next] })?;
        }
        start = next;
    }
    Ok(out.text)
}
