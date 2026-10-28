//! Small, conservative literal tokenizers, not parsers or keyword tables.
//! One span is retained at a time. Ambiguous syntax protects the remaining bytes
//! (including EOF), or refuses the edit, rather than guessing where a literal
//! ends. Comments are retained too. Interpolation expressions are never reformatted.
use crate::{FormatError, Indent, Output, Settings};
use std::ops::Range;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Python,
    JavaScript,
    Rust,
    Go,
    C,
    CSharp,
    Java,
    Shell,
    Ruby,
    Php,
    Lua,
    Kotlin,
    Swift,
}

pub fn kind(file_type: &str) -> Option<Kind> {
    Some(match file_type.trim_start_matches('.').to_ascii_lowercase().as_str() {
        "py" | "pyw" | "pyi" | "python" => Kind::Python,
        "js" | "mjs" | "cjs" | "jsx" | "javascript" | "ts" | "mts" | "cts" | "tsx" | "typescript" => Kind::JavaScript,
        "rs" | "rust" => Kind::Rust,
        "go" | "golang" => Kind::Go,
        "c" | "h" | "cc" | "hh" | "cpp" | "hpp" | "cxx" | "hxx" | "c++" => Kind::C,
        "cs" | "csharp" | "c#" => Kind::CSharp,
        "java" => Kind::Java,
        "sh" | "bash" | "zsh" | "ksh" | "shell" => Kind::Shell,
        "rb" | "rake" | "ruby" => Kind::Ruby,
        "php" => Kind::Php,
        "lua" => Kind::Lua,
        "kt" | "kts" | "kotlin" => Kind::Kotlin,
        "swift" => Kind::Swift,
        _ => return None,
    })
}

fn php_opening_tag(text: &str) -> bool {
    let long = text.get(..5).is_some_and(|name| name.eq_ignore_ascii_case("<?php"));
    // Without following tag whitespace, <?phpx is inline output when short tags
    // are disabled. Preserve it; raw prefix spelling does not establish PHP code.
    let boundary = matches!(text.as_bytes().get(5), Some(b' ' | b'\t' | b'\r' | b'\n'));
    text.starts_with("<?=") || long && boundary
}

pub fn tidy(
    text: &str,
    range: Range<usize>,
    selected: bool,
    kind: Kind,
    settings: &Settings,
) -> Result<String, FormatError> {
    let mut out = Output::new(settings.max_output_bytes);
    if kind == Kind::Php && !php_opening_tag(text) {
        // Admission applies to the whole unit, including empty input and EOF;
        // no raw-prefix/short-tag assumption may turn inline output into code.
        out.push(&text[range])?;
        return Ok(out.text);
    }
    let mut scanner = Scanner { text, kind, pos: 0, refusal: None };
    let mut span = scanner.next();
    let b = text.as_bytes();
    let mut start = range.start;
    let mut eol = None;
    let mut eof_protected = false;
    while start < range.end {
        // Shell's lexer splits commands on LF. CR (including the CR in CRLF)
        // is an ordinary argument byte, not a structural line ending.
        let end =
            (start..range.end).find(|&i| b[i] == b'\n' || kind != Kind::Shell && b[i] == b'\r').unwrap_or(range.end);
        let mut next = end;
        if next < range.end {
            next += 1;
            if b[end] == b'\r' && next < range.end && b[next] == b'\n' {
                next += 1;
            }
        }
        let line = &text[start..end];
        let leading = start + line.len() - line.trim_start_matches([' ', '\t']).len();
        let trailing = start + line.trim_end_matches([' ', '\t']).len();
        // A selection boundary is not a physical line ending. Trimming its
        // final spaces could join tokens to following, unselected bytes.
        let partial_final_line = selected
            && end == range.end
            && b.get(end).is_some_and(|&c| c != b'\n' && (kind == Kind::Shell || c != b'\r'));
        let partial_crlf =
            selected && next == range.end && b.get(next) == Some(&b'\n') && text.get(end..next) == Some("\r");
        let (mut protect_indent, mut protect_trim, mut protect_eol) = (false, partial_final_line, partial_crlf);
        while let Some(token) = &span {
            if token.start > next {
                break;
            }
            let overlaps = |a, z| a < z && token.start < z && token.end > a;
            protect_indent |= overlaps(start, leading);
            protect_trim |= overlaps(trailing, end);
            protect_eol |= overlaps(end, next);
            eof_protected |= next == text.len() && token.end > text.len();
            if token.end > next {
                break;
            }
            span = scanner.next();
        }
        let stop = if settings.trim_trailing_whitespace && !protect_trim { trailing } else { end };
        // Python indentation uses fixed eight-column tab stops, independent of
        // editor preferences. A literal-only lexer cannot prove its reindent safe.
        if settings.reindent && !selected && !protect_indent && kind != Kind::Python {
            let leading = leading.min(stop);
            let mut columns = 0;
            for &c in &b[start..leading] {
                columns += if c == b'\t' { settings.indent.width() - columns % settings.indent.width() } else { 1 };
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
            out.push(&text[leading..stop])?;
        } else {
            out.push(&text[start..stop])?;
        }
        if next > end {
            let original = &text[end..next];
            // Literal line endings do not set the style for structural endings.
            if !protect_eol && settings.normalize_line_endings {
                out.push(eol.get_or_insert(original))?;
            } else {
                out.push(original)?;
            }
        }
        start = next;
    }
    if let Some(at) = scanner.refusal {
        // No replacement escapes this function when lexical proof failed, even
        // if the selected lines preceded the uncertain interpolation boundary.
        return Err(FormatError::new(
            "PHP",
            "heredoc interpolation boundary is unproven; configure an external formatter",
            crate::line_at(b, at).saturating_sub(crate::line_at(b, range.start) - 1).max(1),
        ));
    }
    let final_ending = text.ends_with('\n') || kind != Kind::Shell && text.ends_with('\r');
    if !selected && settings.final_newline && !eof_protected && !final_ending {
        out.push(eol.unwrap_or("\n"))?;
    }
    Ok(out.text)
}

struct Scanner<'a> {
    text: &'a str,
    kind: Kind,
    pos: usize,
    refusal: Option<usize>,
}
impl Scanner<'_> {
    fn uncertain(&self) -> usize {
        self.text.len() + 1
    }

    fn line_end(&self, at: usize) -> usize {
        self.text[at..]
            .find(|c| match self.kind {
                Kind::Shell => c == '\n',
                Kind::JavaScript => matches!(c, '\r' | '\n' | '\u{2028}' | '\u{2029}'),
                _ => matches!(c, '\r' | '\n'),
            })
            .map_or(self.text.len(), |n| at + n)
    }

    fn after_line(&self, at: usize) -> usize {
        let end = self.line_end(at);
        if self.text.as_bytes().get(end..end + 2) == Some(b"\r\n") {
            end + 2
        } else if end < self.text.len() {
            end + 1
        } else {
            end
        }
    }

    fn block_comment(&self, start: usize, nested: bool) -> usize {
        let mut depth = 1;
        let mut i = start + 2;
        while i < self.text.len() {
            if self.kind == Kind::C && (self.text.as_bytes()[i] == b'\\' || self.text[i..].starts_with("??")) {
                // C/C++ translation phases splice lines before recognizing
                // comments. A raw-byte lexer cannot prove the closing boundary
                // after such input; preserve the suffix rather than hide later
                // string openers inside a falsely extended comment span.
                return self.uncertain();
            } else if self.text[i..].starts_with("*/") {
                depth -= 1;
                i += 2;
                if depth == 0 {
                    return i;
                }
            } else if nested && self.text[i..].starts_with("/*") {
                depth += 1;
                i += 2;
            } else {
                i += self.text[i..].chars().next().unwrap().len_utf8();
            }
        }
        self.uncertain()
    }

    fn quoted(&self, start: usize, quote: u8, count: usize, escapes: bool, doubled: bool, interpolate: bool) -> usize {
        let b = self.text.as_bytes();
        let mut i = start + count;
        while i < b.len() {
            if interpolate
                && (b[i] == b'{'
                    || (b[i] == b'\\' && self.kind == Kind::Swift)
                    || self.kind == Kind::Shell && matches!(b[i], b'$' | b'`'))
            {
                // Nested expression grammars can contain this same delimiter.
                return self.uncertain();
            }
            if escapes && b[i] == b'\\' {
                i += 1;
                if i < b.len() {
                    i += self.text[i..].chars().next().unwrap().len_utf8();
                }
            } else if b[i..].starts_with(&b[start..start + count]) {
                if doubled && b.get(i + 1) == Some(&quote) {
                    i += 2;
                } else {
                    return i + count;
                }
            } else {
                i += self.text[i..].chars().next().unwrap().len_utf8();
            }
        }
        self.uncertain()
    }

    fn raw_end(&self, body: usize, delimiter: &str) -> usize {
        self.text[body..].find(delimiter).map_or(self.uncertain(), |n| body + n + delimiter.len())
    }

    fn lua_long(&self, start: usize) -> Option<usize> {
        let b = self.text.as_bytes();
        if b.get(start) != Some(&b'[') {
            return None;
        }
        let mut i = start + 1;
        while b.get(i) == Some(&b'=') {
            i += 1;
        }
        if b.get(i) != Some(&b'[') {
            return None;
        }
        // Bound delimiter comparisons as well as memory. Very long delimiters
        // are uncertain and preserve the suffix.
        if i - start > 256 {
            return Some(self.uncertain());
        }
        let mut search = i + 1;
        while let Some(n) = self.text[search..].find(']') {
            let close = search + n;
            let len = i - start + 1;
            if b.get(close + 1..close + len - 1) == Some(&b[start + 1..i]) && b.get(close + len - 1) == Some(&b']') {
                return Some(close + len);
            }
            search = close + 1;
        }
        Some(self.uncertain())
    }

    fn shell_heredoc_line(&self, start: usize, delimiter: &str, strip: bool, quoted: bool) -> Option<(usize, bool)> {
        // Match the logical line without materializing it. Unquoted backslash-LF
        // removal precedes delimiter matching (and leading-tab stripping). Paired
        // backslashes quote each other, so only odd parity removes a following LF.
        // CR is ordinary shell data, including the CR in backslash-CRLF.
        let b = self.text.as_bytes();
        let mut at = start;
        let mut matched = Some(0);
        let mut leading = true;
        while at < b.len() {
            if !quoted && b[at] == b'\\' {
                if b.get(at + 1) == Some(&b'\n') {
                    at += 2;
                    continue;
                }
                if b.get(at + 1) == Some(&b'\\') {
                    matched = None;
                    leading = false;
                    at += 2;
                    continue;
                }
            }
            if b[at] == b'\n' {
                return Some((at + 1, matched == Some(delimiter.len())));
            }
            if !(strip && leading && b[at] == b'\t') {
                leading = false;
                matched = matched.filter(|&n| delimiter.as_bytes().get(n) == Some(&b[at])).map(|n| n + 1);
            }
            at += 1;
        }
        // A missing logical newline leaves EOF unproven; retain it exactly,
        // including a final escaped newline or a delimiter-looking EOF fragment.
        None
    }

    fn heredoc(&mut self, start: usize) -> usize {
        let b = self.text.as_bytes();
        let header_end = self.line_end(start);
        let mut i = start;
        let mut delimiters = Vec::new(); // Fixed ceiling of 16 pending heredocs.
        while i < header_end {
            if matches!(b[i], b' ' | b'\t') {
                i += 1;
                continue;
            }
            if !self.text[i..].starts_with("<<") {
                // Physical LF starts the body only when the complete header is
                // proven to consist of simple heredoc operators/delimiters and
                // whitespace. Quotes, continuations, substitutions, comments or
                // any other unparsed token may defer the shell's lexical NEWLINE.
                // Preserve the entire uncertain suffix; never search its quoted
                // argument text for what merely looks like a body terminator.
                return self.uncertain();
            }
            if delimiters.len() == 16 {
                return self.uncertain();
            }
            i += 2;
            if self.kind == Kind::Php {
                if b.get(i) != Some(&b'<') {
                    return self.uncertain();
                }
                i += 1;
            }
            let mut strip = false;
            if b.get(i) == Some(&b'-') || (self.kind == Kind::Ruby && b.get(i) == Some(&b'~')) {
                strip = true;
                i += 1;
            }
            while i < header_end && matches!(b[i], b' ' | b'\t') {
                i += 1;
            }
            let quote = b.get(i).copied().filter(|c| matches!(c, b'\'' | b'"' | b'`'));
            if self.kind == Kind::Shell && quote == Some(b'`') {
                // Command substitution does not prove a literal delimiter word.
                return self.uncertain();
            }
            if quote.is_some() {
                i += 1;
            }
            let word = i;
            while i < header_end && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            if word == i {
                return self.uncertain();
            }
            let delimiter = &self.text[word..i];
            if let Some(q) = quote {
                if b.get(i) != Some(&q) {
                    return self.uncertain();
                }
                i += 1;
            } else if i < header_end && !matches!(b[i], b' ' | b'\t' | b';' | b'<' | b')') {
                return self.uncertain();
            }
            delimiters.push((delimiter, strip, quote == Some(b'\''), quote.is_some()));
        }
        let mut body = self.after_line(start);
        for (delimiter, strip, nowdoc, quoted) in delimiters {
            loop {
                if body >= b.len() {
                    return self.uncertain();
                }
                if self.kind == Kind::Shell {
                    let Some((next, closes)) = self.shell_heredoc_line(body, delimiter, strip, quoted) else {
                        return self.uncertain();
                    };
                    body = next;
                    if closes {
                        break;
                    }
                    continue;
                }
                let end = self.line_end(body);
                let original = &self.text[body..end];
                let line = if self.kind == Kind::Php || (self.kind == Kind::Ruby && strip) {
                    original.trim_start_matches([' ', '\t'])
                } else if strip {
                    original.trim_start_matches('\t')
                } else {
                    original
                };
                let closes = if self.kind == Kind::Php {
                    line.strip_prefix(delimiter).is_some_and(|rest| {
                        rest.as_bytes().first().is_none_or(|c| !c.is_ascii_alphanumeric() && *c != b'_' && *c < 128)
                    })
                } else {
                    line == delimiter
                };
                if closes {
                    if self.kind == Kind::Php {
                        // PHP permits expressions immediately after the closing
                        // label. Resume scanning at its token boundary so a
                        // following string opener cannot be hidden by this span.
                        return body + original.len() - line.len() + delimiter.len();
                    }
                    body = self.after_line(body);
                    break;
                }
                if self.kind == Kind::Php && !nowdoc {
                    // A physical label is a terminator only in OUTER literal
                    // context. PHP interpolation can enter nested script strings,
                    // comments and braces containing that same physical label.
                    // Rather than invent a partial PHP expression parser, refuse
                    // unescaped dollar syntax before trusting any later label.
                    // Nowdocs have no interpolation. Heredoc \$ and \\ escapes
                    // are resolved in constant state; other escapes cannot hide
                    // a later interpolation introducer (including \{$...}).
                    let mut at = body;
                    while at < end {
                        if b[at] == b'\\' && b.get(at + 1).is_some_and(|c| matches!(c, b'$' | b'\\')) {
                            at += 2;
                        } else if b[at] == b'$' {
                            self.refusal = Some(at);
                            return self.uncertain();
                        } else {
                            at += 1;
                        }
                    }
                }
                body = self.after_line(body);
            }
        }
        body
    }
}

impl Iterator for Scanner<'_> {
    type Item = Range<usize>;
    fn next(&mut self) -> Option<Self::Item> {
        let b = self.text.as_bytes();
        if self.pos == 0 && self.kind == Kind::Java && self.text.contains("\\u") {
            // Java translates Unicode escapes before tokenizing even comments
            // and literal delimiters. Without that preprocessing pass there is
            // no proven lexical context anywhere in this compilation unit.
            self.pos = self.uncertain();
            return Some(0..self.pos);
        }
        while self.pos < b.len() {
            let start = self.pos;
            let c = b[start];
            let rest = &self.text[start..];
            let k = self.kind;
            let line_start = start == 0 || matches!(b[start - 1], b'\r' | b'\n');
            let hash_comment = matches!(k, Kind::Python | Kind::Ruby | Kind::Php)
                || (k == Kind::Shell && (start == 0 || b" \t\n;|&()".contains(&b[start - 1])));
            let slash_comments = !matches!(k, Kind::Python | Kind::Shell | Kind::Ruby | Kind::Lua);
            let end = if k == Kind::Php && rest.starts_with("#[") {
                // Attributes may contain multiline literals.
                self.uncertain()
            } else if c == b'#' && hash_comment || slash_comments && rest.starts_with("//") {
                // A spliced line comment can hide literal delimiters on the next
                // line. Preserve the suffix when continuation semantics vary.
                let end = self.line_end(start);
                if self.text[start..end].contains('\\')
                    || k == Kind::Php && self.text[start..end].contains("?>")
                    || k == Kind::C && self.text[start..end].contains("??")
                {
                    self.uncertain()
                } else {
                    end
                }
            } else if slash_comments && rest.starts_with("/*") {
                self.block_comment(start, matches!(k, Kind::Rust | Kind::Kotlin | Kind::Swift))
            } else if k == Kind::Lua && rest.starts_with("--") {
                self.lua_long(start + 2).unwrap_or_else(|| self.line_end(start))
            } else if k == Kind::Lua && c == b'[' {
                if let Some(end) = self.lua_long(start) {
                    end
                } else {
                    self.pos += 1;
                    continue;
                }
            } else if matches!(k, Kind::Shell | Kind::Ruby | Kind::Php) && rest.starts_with("<<") {
                self.heredoc(start)
            } else if k == Kind::Ruby && line_start && (rest.starts_with("=begin") || rest.starts_with("__END__")) {
                // Embedded documentation and Ruby's trailing data section.
                self.uncertain()
            } else if k == Kind::Php
                && (rest.starts_with("?>")
                    || rest
                        .get(.."__halt_compiler".len())
                        .is_some_and(|name| name.eq_ignore_ascii_case("__halt_compiler")))
            {
                // PHP keywords are ASCII case-insensitive. Any halt spelling
                // ends proven code context; its following data is opaque to tidy.
                self.uncertain() // Inline HTML/data is literal output.
            } else if k == Kind::Rust && (rest.starts_with("r\"") || rest.starts_with("r#")) {
                let hashes = b[start + 1..].iter().take_while(|&&c| c == b'#').count();
                let open = start + 1 + hashes;
                if hashes > 255 || b.get(open) != Some(&b'"') {
                    self.uncertain()
                } else {
                    self.raw_end(open + 1, &format!("\"{}", "#".repeat(hashes)))
                }
            } else if k == Kind::C && rest.starts_with("R\"") {
                let after = start + 2;
                if let Some(n) = self.text[after..].find('(').filter(|&n| n <= 16) {
                    let delimiter = &self.text[after..after + n];
                    self.raw_end(after + n + 1, &format!("){delimiter}\""))
                } else {
                    self.uncertain()
                }
            } else if k == Kind::Swift && c == b'#' {
                let hashes = b[start..].iter().take_while(|&&c| c == b'#').count();
                let open = start + hashes;
                if hashes > 255 || b.get(open) != Some(&b'"') {
                    self.uncertain()
                } else {
                    let quotes = if self.text[open..].starts_with("\"\"\"") { 3 } else { 1 };
                    let end = self.raw_end(open + quotes, &format!("{}{}", "\"".repeat(quotes), "#".repeat(hashes)));
                    if self.text[open + quotes..end.min(b.len())].contains('\\') { self.uncertain() } else { end }
                }
            } else if k == Kind::CSharp && c == b'@' && b.get(start + 1) == Some(&b'"') {
                self.quoted(start + 1, b'"', 1, false, true, false)
            } else if k == Kind::CSharp && c == b'$' {
                self.uncertain() // Interpolated/verbatim/raw combinations.
            } else if c == b'\''
                && k == Kind::Rust
                && b.get(start + 1).is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_')
                && b.get(start + 2) != Some(&b'\'')
            {
                self.pos += 1;
                continue; // Lifetime, not a character literal.
            } else if k == Kind::C && c == b'\'' && start > 0 && b[start - 1].is_ascii_alphanumeric() {
                self.uncertain() // C++ digit separators can resemble quotes.
            } else if matches!(c, b'\'' | b'"')
                || c == b'`'
                    && matches!(
                        k,
                        Kind::JavaScript | Kind::Go | Kind::Shell | Kind::Ruby | Kind::Php | Kind::Kotlin | Kind::Swift
                    )
            {
                let triple = c != b'`'
                    && matches!(k, Kind::Python | Kind::CSharp | Kind::Java | Kind::Kotlin | Kind::Swift)
                    && b.get(start..start + 3).is_some_and(|s| s.iter().all(|&q| q == c));
                let count = if triple {
                    if k == Kind::CSharp { b[start..].iter().take_while(|&&q| q == c).count() } else { 3 }
                } else {
                    1
                };
                let prefix_start = self.text[..start]
                    .char_indices()
                    .rev()
                    .find(|(_, c)| !c.is_ascii_alphabetic())
                    .map_or(0, |(n, c)| n + c.len_utf8());
                let python_f = k == Kind::Python
                    && self.text[prefix_start..start].bytes().any(|c| matches!(c, b'f' | b'F' | b't' | b'T'));
                let interpolate = python_f
                    || matches!(
                        k,
                        Kind::JavaScript | Kind::Ruby | Kind::Php | Kind::Kotlin | Kind::Swift | Kind::Shell
                    ) && c != b'\'';
                let escapes = !(c == b'`' && k == Kind::Go
                    || triple && matches!(k, Kind::Kotlin | Kind::CSharp)
                    || c == b'\'' && k == Kind::Shell);
                if count > 255 { self.uncertain() } else { self.quoted(start, c, count, escapes, false, interpolate) }
            } else if c == b'\\'
                || matches!(k, Kind::JavaScript | Kind::Ruby | Kind::Swift) && c == b'/'
                || k == Kind::Ruby && c == b'%'
                || k == Kind::Shell && matches!(c, b'$' | b'`')
                || k == Kind::JavaScript && c == b'<'
                || k == Kind::C && rest.starts_with("??")
            {
                // Continuations, regex/percent literals, substitutions and JSX
                // need a grammar to disambiguate. No edits to this suffix.
                self.uncertain()
            } else {
                self.pos += self.text[start..].chars().next().unwrap().len_utf8();
                continue;
            };
            self.pos = end;
            return Some(start..end);
        }
        None
    }
}
