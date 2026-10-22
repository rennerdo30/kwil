//! Worker-only detection. No filesystem operations; at most one bounded prefix
//! is inspected. Settings matching/compilation never belongs on the UI thread.
use crate::{Error, Language, TextSource, read_chunk};
use globset::{GlobBuilder, GlobMatcher};
use std::path::Path;

pub const FIRST_LINE_LIMIT: usize = 64 * 1024;
pub const MAX_MAPPINGS: usize = 256;
pub const MAX_PATTERN_BYTES: usize = 1024;
pub const MAX_PATH_BYTES: usize = 32 * 1024;

impl Language {
    /// Stable ids and common picker/modeline aliases; unsupported names are
    /// rejected, never silently mapped to an unrelated known grammar.
    pub fn from_id(id: &str) -> Option<Self> {
        let id = id.trim();
        Self::CATALOGUE.iter().copied().find(|l| l.id().eq_ignore_ascii_case(id)).or_else(|| {
            Some(match id.to_ascii_lowercase().as_str() {
                "text" | "plain" | "plaintext" | "fundamental" => Self::Generic,
                "c++" => Self::Cpp,
                "c#" | "csharp" | "cs" => Self::CSharp,
                "js" => Self::JavaScript,
                "ts" => Self::TypeScript,
                "sh" | "shell" | "zsh" => Self::Bash,
                "py" => Self::Python,
                "rs" => Self::Rust,
                "ps1" | "pwsh" => Self::PowerShell,
                "dosbatch" | "bat" => Self::Batch,
                "makefile" => Self::Make,
                "conf" | "dosini" => Self::Ini,
                _ => return None,
            })
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetectionOrigin {
    Picker,
    UserMapping,
    Modeline,
    FirstLine,
    Filename,
    Content,
    Generic,
}

/// Logs retain the Generic language identity, whose lexer recognizes levels.
/// This does not claim an audited tree-sitter log grammar exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Detection {
    pub language: Language,
    pub origin: DetectionOrigin,
    pub log_levels: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LanguageMapping {
    pub pattern: String,
    pub language: Language,
}

struct Mapping {
    matcher: GlobMatcher,
    basename: bool,
    language: Language,
}

/// Build once on a worker after settings reload. First matching entry wins;
/// callers supply a stable order (the settings table's sorted order suffices).
/// Bare extensions from the existing settings page also work (`foo` -> `*.foo`).
pub struct Detector {
    mappings: Vec<Mapping>,
}
impl Detector {
    pub fn new(mappings: &[LanguageMapping]) -> Result<Self, Error> {
        if mappings.len() > MAX_MAPPINGS {
            return Err(Error::InvalidConfig);
        }
        let mut compiled = Vec::with_capacity(mappings.len());
        for mapping in mappings {
            let pattern = &mapping.pattern;
            if pattern.is_empty() || pattern.len() > MAX_PATTERN_BYTES {
                return Err(Error::InvalidConfig);
            }
            let pattern = if pattern.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'+')) {
                format!("*.{pattern}")
            } else if pattern.starts_with('.') && !pattern[1..].contains(['*', '?', '[', '{', '/', '\\']) {
                format!("*{pattern}")
            } else {
                pattern.replace('\\', "/")
            };
            let glob = GlobBuilder::new(&pattern)
                .case_insensitive(true)
                .literal_separator(true)
                .build()
                .map_err(|e| Error::Grammar(format!("invalid language glob: {e}")))?;
            compiled.push(Mapping {
                matcher: glob.compile_matcher(),
                basename: !pattern.contains('/'),
                language: mapping.language,
            });
        }
        Ok(Self { mappings: compiled })
    }

    pub fn detect(
        &self,
        path: &Path,
        picker: Option<Language>,
        source: &dyn TextSource,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Detection, Error> {
        let selected = |language, origin| Detection { language, origin, log_levels: false };
        if cancelled() {
            return Err(Error::Cancelled);
        }
        if let Some(language) = picker {
            return Ok(selected(language, DetectionOrigin::Picker));
        }
        // Source paths come from bounded session metadata. Check again for
        // standalone consumers before normalizing or glob matching them.
        if path.as_os_str().len() > MAX_PATH_BYTES {
            return Err(Error::InvalidRange);
        }
        let normalized = path.to_string_lossy().replace('\\', "/");
        let basename = normalized.rsplit('/').next().unwrap_or("");
        for mapping in &self.mappings {
            if cancelled() {
                return Err(Error::Cancelled);
            }
            if mapping.matcher.is_match(if mapping.basename { basename } else { &normalized }) {
                return Ok(selected(mapping.language, DetectionOrigin::UserMapping));
            }
        }
        let prefix = first_line(source, cancelled)?;
        let line = String::from_utf8_lossy(&prefix);
        let line = line.trim_start_matches('\u{feff}').trim();
        if let Some(language) = modeline(line) {
            return Ok(selected(language, DetectionOrigin::Modeline));
        }
        if let Some(language) = first_line_language(line) {
            return Ok(selected(language, DetectionOrigin::FirstLine));
        }
        // Legacy detect with no content only considers the audited name table.
        if let Some(language) = crate::detect(path, b"") {
            return Ok(selected(language, DetectionOrigin::Filename));
        }
        if log_filename(basename) {
            return Ok(Detection { language: Language::Generic, origin: DetectionOrigin::Filename, log_levels: true });
        }
        if let Some((language, log_levels)) = sniff(line) {
            return Ok(Detection { language, origin: DetectionOrigin::Content, log_levels });
        }
        Ok(selected(Language::Generic, DetectionOrigin::Generic))
    }
}

/// At most 64 KiB in total, honoring fragmented sources and stopping at the
/// first LF/CR. A single source read may include bytes after that newline, but
/// they are never inspected for detection.
fn first_line(source: &dyn TextSource, cancelled: &dyn Fn() -> bool) -> Result<Vec<u8>, Error> {
    let limit = source.len().min(FIRST_LINE_LIMIT);
    let mut bytes = Vec::new();
    let mut offset = 0;
    while offset < limit {
        if cancelled() {
            return Err(Error::Cancelled);
        }
        let chunk = read_chunk(source, offset, limit - offset)?;
        let end = chunk.iter().position(|b| matches!(b, b'\n' | b'\r')).unwrap_or(chunk.len());
        bytes.extend_from_slice(&chunk[..end]);
        offset += chunk.len();
        if end != chunk.len() {
            break;
        }
    }
    Ok(bytes)
}

fn mode_name(text: &str) -> &str {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+' | '#' | '.'))).next().unwrap_or("")
}
fn modeline(line: &str) -> Option<Language> {
    let lower = line.to_ascii_lowercase();
    for marker in ["vim:", "vi:", "ex:"] {
        if let Some((before, rest)) = lower.split_once(marker)
            && (before.is_empty() || before.ends_with(|c: char| c.is_ascii_whitespace()))
        {
            for field in rest.split(|c: char| c.is_ascii_whitespace() || c == ':') {
                for prefix in ["ft=", "filetype=", "syntax="] {
                    if let Some(value) = field.strip_prefix(prefix)
                        && let Some(language) = Language::from_id(mode_name(value))
                    {
                        return Some(language);
                    }
                }
            }
        }
    }
    let (_, rest) = lower.split_once("-*-")?;
    let (body, _) = rest.split_once("-*-")?;
    for field in body.split(';') {
        if let Some((key, value)) = field.split_once(':') {
            if key.trim() == "mode" {
                return Language::from_id(mode_name(value.trim()));
            }
        } else if !body.contains(':') {
            return Language::from_id(mode_name(field.trim()));
        }
    }
    None
}

fn first_line_language(line: &str) -> Option<Language> {
    // Do not mistake arguments or `env VAR=python` for the interpreter.
    if let Some(rest) = line.strip_prefix("#!") {
        let mut tokens = rest.split_whitespace();
        let program = tokens.next()?.rsplit('/').next()?;
        let interpreter = if program == "env" {
            loop {
                let token = tokens.next()?;
                match token {
                    // These options consume a value; it is not an interpreter.
                    "-u" | "--unset" | "-C" | "--chdir" => {
                        tokens.next()?;
                    }
                    "--" => break tokens.next()?,
                    "-i" | "--ignore-environment" | "-S" | "--split-string" => {}
                    value if value.starts_with("--unset=") || value.starts_with("--chdir=") || value.contains('=') => {}
                    value if value.starts_with('-') => return None,
                    value => break value,
                }
            }
        } else {
            program
        };
        let interpreter = interpreter.rsplit('/').next()?;
        return Some(match interpreter {
            "python" | "python2" | "python3" => Language::Python,
            s if s.starts_with("python3.") && s[8..].bytes().all(|b| b.is_ascii_digit()) => Language::Python,
            "ruby" => Language::Ruby,
            "bash" | "sh" | "zsh" | "dash" => Language::Bash,
            "node" | "nodejs" | "deno" => Language::JavaScript,
            "pwsh" | "powershell" => Language::PowerShell,
            "lua" | "luajit" => Language::Lua,
            "php" => Language::Php,
            "runhaskell" | "runghc" => Language::Haskell,
            _ => return None,
        });
    }
    if line.starts_with("<?xml") {
        Some(Language::Xml)
    } else if line.to_ascii_lowercase().starts_with("<!doctype html") {
        Some(Language::Html)
    } else if line.starts_with("<?php") {
        Some(Language::Php)
    } else if line.starts_with("diff --git ") {
        Some(Language::Diff)
    } else {
        None
    }
}

pub(crate) fn log_class(token: &[u8]) -> Option<crate::Class> {
    use crate::Class;
    if [b"ERROR".as_slice(), b"FATAL", b"CRITICAL"].iter().any(|s| token.eq_ignore_ascii_case(s)) {
        Some(Class::Error)
    } else if [b"WARN".as_slice(), b"WARNING"].iter().any(|s| token.eq_ignore_ascii_case(s)) {
        Some(Class::Constant)
    } else if [b"INFO".as_slice(), b"DEBUG", b"TRACE"].iter().any(|s| token.eq_ignore_ascii_case(s)) {
        Some(Class::Keyword)
    } else {
        None
    }
}
fn sniff(line: &str) -> Option<(Language, bool)> {
    let bytes = line.as_bytes();
    let after_open = line.get(1..).unwrap_or("").trim_start();
    if line.starts_with('{') && (after_open.is_empty() || after_open.starts_with(['"', '}']))
        || line.starts_with('[')
            && (after_open.is_empty()
                || after_open.starts_with(['{', '"', ']', '['])
                || after_open.as_bytes().first().is_some_and(u8::is_ascii_digit)
                || after_open.starts_with("true")
                || after_open.starts_with("false")
                || after_open.starts_with("null"))
    {
        return Some((Language::Json, false));
    }
    if line.starts_with('<') && bytes.get(1).is_some_and(u8::is_ascii_alphabetic) && line.contains('>') {
        return Some((Language::Xml, false));
    }
    // Timestamped records and bracketed levels are logs rather than INI
    // sections. Inspect only this already-bounded first line, never a file scan.
    let level_prefix = line.trim_start_matches(['[', '(', ' ']);
    let level_prefix = level_prefix.split(|c: char| !c.is_ascii_alphabetic()).next().unwrap_or("");
    if timestamp_prefix(line.as_bytes()) || log_class(level_prefix.as_bytes()).is_some() {
        return Some((Language::Generic, true));
    }
    if line.starts_with('[')
        && line.find(']').is_some_and(|i| {
            i > 1 && line[i + 1..].trim().starts_with(['#', ';']) || i > 1 && line[i + 1..].trim().is_empty()
        })
        || line.split_once('=').is_some_and(|(key, _)| {
            !key.trim().is_empty()
                && key.trim().bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
        })
    {
        return Some((Language::Ini, false));
    }
    if line.split(|c: char| !c.is_ascii_alphabetic()).any(|s| log_class(s.as_bytes()).is_some()) {
        return Some((Language::Generic, true));
    }
    None
}

fn log_filename(basename: &str) -> bool {
    let name = basename.to_ascii_lowercase();
    if name == "log" || name.ends_with(".log") {
        return true;
    }
    // Numeric/date rotations are still logs. Compressed suffixes belong to the
    // separate compression worker; never interpret compressed bytes as text.
    name.rsplit_once(".log.").is_some_and(|(_, suffix)| {
        !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'-'))
    })
}

fn clock_prefix(bytes: &[u8]) -> bool {
    if bytes.len() < 8
        || bytes[2] != b':'
        || bytes[5] != b':'
        || ![0, 1, 3, 4, 6, 7].into_iter().all(|i| bytes[i].is_ascii_digit())
    {
        return false;
    }
    let pair = |i| (bytes[i] - b'0') * 10 + bytes[i + 1] - b'0';
    pair(0) <= 23 && pair(3) <= 59 && pair(6) <= 60 && bytes.get(8).is_none_or(|b| !b.is_ascii_digit())
}

fn timestamp_prefix(bytes: &[u8]) -> bool {
    let bytes = bytes.strip_prefix(b"[").unwrap_or(bytes);
    // Time-only logs, ISO 8601 and common slash-separated application dates.
    if clock_prefix(bytes) {
        return true;
    }
    if bytes.len() >= 19
        && matches!(bytes[4], b'-' | b'/')
        && bytes[7] == bytes[4]
        && matches!(bytes[10], b'T' | b't' | b' ')
        && [0, 1, 2, 3, 5, 6, 8, 9].into_iter().all(|i| bytes[i].is_ascii_digit())
        && clock_prefix(&bytes[11..])
    {
        return true;
    }
    // RFC 3164 style syslog (Oct 23 05:57:00 ...), including a single-digit day.
    if bytes.len() >= 14
        && [b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov", b"Dec"]
            .iter()
            .any(|month| bytes[..3].eq_ignore_ascii_case(*month))
        && bytes[3] == b' '
    {
        let rest = &bytes[4..];
        let spaces = rest.iter().take_while(|b| **b == b' ').count();
        let rest = &rest[spaces..];
        let day = rest.iter().take_while(|b| b.is_ascii_digit()).count();
        return (1..=2).contains(&day) && rest.get(day) == Some(&b' ') && clock_prefix(&rest[day + 1..]);
    }
    false
}
