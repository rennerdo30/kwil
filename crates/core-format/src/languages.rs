use crate::{FormatError, Output, Settings, ending, line_at};

pub fn json(text: &str, settings: &Settings) -> Result<String, FormatError> {
    // RawValue validates without a Value tree, floating-point conversion, key
    // sorting, duplicate-key loss or a copy of every string.
    serde_json::from_str::<&serde_json::value::RawValue>(text).map_err(|e| FormatError::new("JSON", &e, e.line()))?;
    let mut out = Output::new(settings.max_output_bytes);
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut depth: usize = 0;
    let eol = ending(text);
    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\r' | b'\n' | b'\t' => i += 1,
            b'"' => {
                let end = quoted_end(text, i, b'"', false, "JSON")?;
                out.push(&text[i..end])?;
                i = end;
            }
            b'{' | b'[' => {
                out.push(&text[i..i + 1])?;
                depth += 1;
                if depth > 128 {
                    return Err(FormatError::new("JSON", "nesting exceeds 128 levels", line_at(bytes, i)));
                }
                i += 1;
                while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
                    i += 1;
                }
                if !matches!(bytes.get(i), Some(b'}' | b']')) {
                    out.push(eol)?;
                    out.indent(depth, settings)?;
                }
            }
            b'}' | b']' => {
                depth -= 1;
                if !out.text.ends_with(['{', '[']) {
                    out.push(eol)?;
                    out.indent(depth, settings)?;
                }
                out.push(&text[i..i + 1])?;
                i += 1;
            }
            b',' => {
                out.push(",")?;
                out.push(eol)?;
                out.indent(depth, settings)?;
                i += 1;
            }
            b':' => {
                out.push(": ")?;
                i += 1;
            }
            _ => {
                let start = i;
                while i < bytes.len() && !matches!(bytes[i], b',' | b']' | b'}' | b' ' | b'\t' | b'\r' | b'\n') {
                    i += 1;
                }
                out.push(&text[start..i])?;
            }
        }
    }
    Ok(out.text)
}

pub fn toml(text: &str) -> Result<String, FormatError> {
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| FormatError::new("TOML", &e, line_at(text.as_bytes(), e.span().map_or(0, |s| s.start))))?;
    // Normalize only whitespace-only decor. Table::fmt clears comments too.
    use toml_edit::visit_mut::VisitMut;
    struct Formatter;
    impl VisitMut for Formatter {
        fn visit_inline_table_mut(&mut self, table: &mut toml_edit::InlineTable) {
            let count = table.len();
            for (i, (mut key, value)) in table.iter_mut().enumerate() {
                if key.leaf_decor().prefix().and_then(toml_edit::RawString::as_str).is_none_or(|s| s.trim().is_empty())
                {
                    key.leaf_decor_mut().set_prefix(" ");
                }
                if i + 1 == count
                    && value.decor().suffix().and_then(toml_edit::RawString::as_str).is_none_or(|s| s.trim().is_empty())
                {
                    value.decor_mut().set_suffix(" ");
                }
            }
            toml_edit::visit_mut::visit_inline_table_mut(self, table);
        }
        fn visit_table_like_kv_mut(&mut self, mut key: toml_edit::KeyMut<'_>, item: &mut toml_edit::Item) {
            let decor = key.leaf_decor_mut();
            if decor.suffix().and_then(toml_edit::RawString::as_str).is_none_or(|s| s.trim().is_empty()) {
                decor.set_suffix(" ");
            }
            if let Some(value) = item.as_value_mut()
                && value.decor().prefix().and_then(toml_edit::RawString::as_str).is_none_or(|s| s.trim().is_empty())
            {
                value.decor_mut().set_prefix(" ");
            }
            toml_edit::visit_mut::visit_table_like_kv_mut(self, key, item);
        }
    }
    Formatter.visit_document_mut(&mut doc);
    Ok(doc.to_string())
}

fn quoted_end(text: &str, start: usize, quote: u8, doubled: bool, kind: &str) -> Result<usize, FormatError> {
    let b = text.as_bytes();
    let mut i = start + 1;
    while i < b.len() {
        if b[i] == b'\\' {
            i += 2;
            continue;
        }
        if b[i] == quote {
            if doubled && b.get(i + 1) == Some(&quote) {
                i += 2;
                continue;
            }
            return Ok(i + 1);
        }
        i += 1;
    }
    Err(FormatError::new(kind, "unterminated quoted string", line_at(b, start)))
}

#[derive(Debug)]
struct Tag<'a> {
    start: usize,
    end: usize,
    name: &'a str,
    closing: bool,
    leaf: bool,
    pair: Option<usize>,
    preserve: bool,
}

pub fn markup(text: &str, html: bool, selection: bool, settings: &Settings) -> Result<String, FormatError> {
    let kind = if html { "HTML" } else { "XML" };
    let eol = ending(text);
    let mut tags: Vec<Tag<'_>> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    let mut i = 0;
    let mut roots = 0;
    while i < text.len() {
        if tags.len() >= 262_144 {
            return Err(FormatError::new(kind, "markup token limit exceeded", line_at(text.as_bytes(), i)));
        }
        if !text[i..].starts_with('<') {
            let end = text[i..].find('<').map_or(text.len(), |n| i + n);
            if !text[i..end].trim().is_empty() || (!text[i..end].contains(['\n', '\r']) && end > i) {
                if let Some(&parent) = stack.last() {
                    tags[parent].preserve = true;
                } else if !text[i..end].trim().is_empty() {
                    return Err(FormatError::new(kind, "text outside root element", line_at(text.as_bytes(), i)));
                }
            }
            i = end;
            continue;
        }
        let start = i;
        let special_end = if text[i..].starts_with("<!--") {
            Some("-->")
        } else if text[i..].starts_with("<![CDATA[") {
            if let Some(&parent) = stack.last() {
                tags[parent].preserve = true;
            }
            Some("]]>")
        } else if text[i..].starts_with("<?") {
            Some("?>")
        } else {
            None
        };
        if let Some(delimiter) = special_end {
            i = text[i + 2..]
                .find(delimiter)
                .map(|n| i + 2 + n + delimiter.len())
                .ok_or_else(|| FormatError::new(kind, "unterminated markup", line_at(text.as_bytes(), start)))?;
            tags.push(Tag { start, end: i, name: "", closing: false, leaf: true, pair: None, preserve: true });
            continue;
        }
        // A quoted '>' in an attribute does not end the tag.
        i += 1;
        while i < text.len() && text.as_bytes()[i] != b'>' {
            if matches!(text.as_bytes()[i], b'\'' | b'"') {
                // XML/HTML attributes do not use backslash quote escaping.
                let quote = text.as_bytes()[i] as char;
                i = text[i + 1..].find(quote).map(|end| i + end + 2).ok_or_else(|| {
                    FormatError::new(kind, "unterminated attribute value", line_at(text.as_bytes(), i))
                })?;
            } else {
                i += 1;
            }
        }
        if i == text.len() {
            return Err(FormatError::new(kind, "unterminated tag", line_at(text.as_bytes(), start)));
        }
        i += 1;
        let inside = &text[start + 1..i - 1];
        if inside.starts_with('!') {
            // DTD internal subsets can contain nested '>'; do not guess.
            if inside.contains('[') {
                return Err(FormatError::new(
                    kind,
                    "DTD internal subsets require an external formatter",
                    line_at(text.as_bytes(), start),
                ));
            }
            tags.push(Tag { start, end: i, name: "", closing: false, leaf: true, pair: None, preserve: true });
            continue;
        }
        let closing = inside.starts_with('/');
        let name = inside.trim_start_matches('/').split([' ', '\t', '\r', '\n', '/']).next().unwrap_or("");
        if name.is_empty()
            || !name.chars().next().is_some_and(|c| c.is_alphabetic() || matches!(c, ':' | '_'))
            || !name.chars().all(|c| c.is_alphanumeric() || matches!(c, ':' | '-' | '_' | '.'))
        {
            return Err(FormatError::new(kind, "invalid element name", line_at(text.as_bytes(), start)));
        }
        let lower = name.to_ascii_lowercase();
        if !html {
            validate_xml_attributes(inside, name, closing)
                .map_err(|message| FormatError::new(kind, message, line_at(text.as_bytes(), start)))?;
        }
        let leaf = inside.ends_with('/')
            || (html
                && matches!(
                    lower.as_str(),
                    "area"
                        | "base"
                        | "br"
                        | "col"
                        | "embed"
                        | "hr"
                        | "img"
                        | "input"
                        | "link"
                        | "meta"
                        | "param"
                        | "source"
                        | "track"
                        | "wbr"
                ));
        let index = tags.len();
        let protected =
            matches!(lower.as_str(), "pre" | "script" | "style" | "textarea") || inside.contains("xml:space");
        tags.push(Tag { start, end: i, name, closing, leaf, pair: None, preserve: protected });
        if html
            && matches!(lower.as_str(), "a" | "b" | "i" | "em" | "strong" | "span" | "code" | "small" | "sub" | "sup")
            && let Some(&parent) = stack.last()
        {
            tags[parent].preserve = true;
        }
        if closing {
            let open = stack
                .pop()
                .ok_or_else(|| FormatError::new(kind, "unexpected closing tag", line_at(text.as_bytes(), start)))?;
            if !(tags[open].name == name || html && tags[open].name.eq_ignore_ascii_case(name)) {
                return Err(FormatError::new(kind, "mismatched closing tag", line_at(text.as_bytes(), start)));
            }
            tags[open].pair = Some(index);
            tags[index].pair = Some(open);
            if index == open + 1 && start > tags[open].end {
                tags[open].preserve = true;
            }
        } else if !leaf {
            if stack.is_empty() {
                roots += 1;
            }
            if stack.len() >= 128 {
                return Err(FormatError::new(kind, "nesting exceeds 128 levels", line_at(text.as_bytes(), start)));
            }
            stack.push(index);
            if html && matches!(lower.as_str(), "pre" | "script" | "style" | "textarea") {
                // Skip raw text entirely, including '<' operators and fake tags.
                let needle = format!("</{lower}");
                let rest = &text[i..];
                let close = rest.as_bytes().windows(needle.len()).enumerate().find_map(|(at, w)| {
                    (w.eq_ignore_ascii_case(needle.as_bytes())
                        && rest
                            .as_bytes()
                            .get(at + needle.len())
                            .is_some_and(|b| b.is_ascii_whitespace() || *b == b'>'))
                    .then_some(at)
                });
                let close = close
                    .ok_or_else(|| FormatError::new(kind, "unclosed raw element", line_at(text.as_bytes(), start)))?;
                tags[index].preserve = true;
                i += close;
            }
        } else if stack.is_empty() {
            roots += 1;
        }
    }
    if let Some(&open) = stack.last() {
        return Err(FormatError::new(kind, "unclosed element", line_at(text.as_bytes(), tags[open].start)));
    }
    if (selection || !html) && roots != 1 {
        return Err(FormatError::new(kind, "expected one complete root element", 1));
    }
    let mut out = Output::new(settings.max_output_bytes);
    let mut depth: usize = 0;
    let mut t = 0;
    while t < tags.len() {
        let tag = &tags[t];
        if tag.closing {
            depth -= 1;
        }
        if !out.text.is_empty() {
            out.push(eol)?;
        }
        out.indent(depth, settings)?;
        if tag.preserve
            && let Some(end) = tag.pair
        {
            out.push(&text[tag.start..tags[end].end])?;
            t = end + 1;
            continue;
        }
        out.push(&text[tag.start..tag.end])?;
        if !tag.closing && !tag.leaf {
            depth += 1;
        }
        t += 1;
    }
    Ok(out.text)
}

fn validate_xml_attributes(inside: &str, name: &str, closing: bool) -> Result<(), &'static str> {
    let mut rest = if closing { &inside[1 + name.len()..] } else { &inside[name.len()..] };
    if closing {
        return if rest.trim().is_empty() { Ok(()) } else { Err("attributes on closing tag") };
    }
    if let Some(stripped) = rest.strip_suffix('/') {
        rest = stripped;
    }
    let mut names = std::collections::BTreeSet::new();
    while !rest.is_empty() {
        if !rest.starts_with(char::is_whitespace) {
            return Err("missing whitespace before attribute");
        }
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        let end = rest.find(|c: char| c.is_whitespace() || c == '=').unwrap_or(rest.len());
        let attr = &rest[..end];
        if attr.is_empty()
            || !attr.chars().next().is_some_and(|c| c.is_alphabetic() || matches!(c, ':' | '_'))
            || !attr.chars().all(|c| c.is_alphanumeric() || matches!(c, ':' | '_' | '-' | '.'))
        {
            return Err("invalid attribute name");
        }
        if !names.insert(attr) {
            return Err("duplicate attribute");
        }
        rest = rest[end..].trim_start();
        rest = rest.strip_prefix('=').ok_or("expected '=' after attribute")?.trim_start();
        let quote = rest.as_bytes().first().copied().ok_or("missing attribute value")?;
        if !matches!(quote, b'\'' | b'"') {
            return Err("attribute value must be quoted");
        }
        let end = rest[1..].find(quote as char).ok_or("unterminated attribute value")? + 1;
        if rest[1..end].contains('<') {
            return Err("'<' in attribute value");
        }
        rest = &rest[end + 1..];
    }
    Ok(())
}

/// Conservative lexical formatters: retain strings, comments and every
/// punctuation token. Reject unbalanced syntax, never attempt dialect rewrites.
pub fn css(text: &str, settings: &Settings) -> Result<String, FormatError> {
    lexical(text, settings, false)
}
pub fn sql(text: &str, settings: &Settings) -> Result<String, FormatError> {
    lexical(text, settings, true)
}

fn lexical(text: &str, settings: &Settings, sql: bool) -> Result<String, FormatError> {
    let kind = if sql { "SQL" } else { "CSS" };
    let eol = ending(text);
    let mut out = Output::new(settings.max_output_bytes);
    let mut i = 0;
    let mut stack = Vec::new();
    let mut pending_space = false;
    let mut newline = false;
    let mut depth: usize = 0;
    let b = text.as_bytes();
    while i < b.len() {
        if b[i].is_ascii_whitespace() {
            pending_space = true;
            i += 1;
            continue;
        }
        let start = i;
        let comment = text[i..].starts_with("/*");
        let line_comment = sql && text[i..].starts_with("--");
        let bracket = sql && b[i] == b'[';
        let dollar = if sql && b[i] == b'$' {
            text[i + 1..]
                .find('$')
                .filter(|&end| text[i + 1..i + 1 + end].bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'))
                .map(|end| &text[i..i + end + 2])
        } else {
            None
        };
        let quoted = matches!(b[i], b'\'' | b'"' | b'`') || bracket || dollar.is_some();
        if comment {
            i = text[i + 2..]
                .find("*/")
                .map(|n| i + 2 + n + 2)
                .ok_or_else(|| FormatError::new(kind, "unterminated comment", line_at(b, i)))?;
        } else if line_comment {
            i = text[i..].find(['\r', '\n']).map_or(text.len(), |n| i + n);
        } else if let Some(delimiter) = dollar {
            let after = i + delimiter.len();
            i = text[after..]
                .find(delimiter)
                .map(|n| after + n + delimiter.len())
                .ok_or_else(|| FormatError::new(kind, "unterminated dollar-quoted string", line_at(b, start)))?;
        } else if quoted {
            i = quoted_end(text, i, if bracket { b']' } else { b[i] }, sql, kind)?;
        } else if b[i].is_ascii_alphabetic() || b[i] == b'_' {
            i += 1;
            while b.get(i).is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'$')) {
                i += 1;
            }
        } else {
            i += text[i..].chars().next().map_or(1, char::len_utf8);
        }
        let token = &text[start..i];
        let upper =
            if sql && !quoted && !comment && !line_comment { token.to_ascii_uppercase() } else { String::new() };
        let keyword = matches!(
            upper.as_str(),
            "SELECT"
                | "FROM"
                | "WHERE"
                | "JOIN"
                | "LEFT"
                | "RIGHT"
                | "INNER"
                | "OUTER"
                | "ON"
                | "AS"
                | "AND"
                | "OR"
                | "NOT"
                | "NULL"
                | "IS"
                | "IN"
                | "INSERT"
                | "INTO"
                | "VALUES"
                | "UPDATE"
                | "SET"
                | "DELETE"
                | "GROUP"
                | "ORDER"
                | "BY"
                | "HAVING"
                | "LIMIT"
                | "OFFSET"
                | "UNION"
                | "ALL"
                | "DISTINCT"
                | "ASC"
                | "DESC"
        );
        let clause = sql
            && stack.is_empty()
            && matches!(
                upper.as_str(),
                "FROM" | "WHERE" | "JOIN" | "GROUP" | "ORDER" | "HAVING" | "LIMIT" | "UNION" | "SET" | "VALUES"
            );
        if !sql && token == "}" {
            depth = depth.saturating_sub(1);
            newline = true;
        }
        if (newline || clause) && !out.text.is_empty() {
            out.push(eol)?;
            out.indent(depth, settings)?;
        } else if pending_space && !out.text.is_empty() {
            out.push(" ")?;
        }
        newline = false;
        pending_space = false;
        out.push(if keyword { &upper } else { token })?;
        if !quoted && !comment && !line_comment {
            match token {
                "{" | "(" | "[" => {
                    if stack.len() >= 128 {
                        return Err(FormatError::new(kind, "nesting exceeds 128 levels", line_at(b, start)));
                    }
                    stack.push((token, start));
                }
                "}" | ")" | "]" => {
                    let expected = match token {
                        "}" => "{",
                        ")" => "(",
                        _ => "[",
                    };
                    if stack.pop().is_none_or(|(open, _)| open != expected) {
                        return Err(FormatError::new(kind, "unmatched delimiter", line_at(b, start)));
                    }
                }
                _ => {}
            }
            if !sql && token == "{" {
                depth += 1;
                newline = true;
            }
            if !sql && (token == ";" && stack.last().is_some_and(|(s, _)| *s == "{") || token == "}") {
                newline = true;
            }
            if sql && token == ";" {
                newline = true;
            }
        }
        if line_comment {
            newline = true;
        }
    }
    if let Some((_, at)) = stack.last() {
        return Err(FormatError::new(kind, "unclosed delimiter", line_at(b, *at)));
    }
    Ok(out.text)
}
