//! Worker-side parsers. Callers must never invoke these on startup or the UI thread.
use crate::theme::*;
use serde::{
    Deserialize,
    de::{self, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Map, Value};
use std::{collections::BTreeMap, fmt};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Native {
    version: u32,
    name: String,
    base: String,
    #[serde(default)]
    ui: BTreeMap<String, String>,
    #[serde(default)]
    syntax: BTreeMap<String, NativeStyle>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeStyle {
    foreground: Option<String>,
    background: Option<String>,
    font_style: Option<String>,
}
pub(crate) fn bounded_source(bytes: &[u8]) -> Result<&str, ThemeError> {
    if bytes.len() > MAX_THEME_BYTES {
        return Err(error("theme exceeds 1 MiB"));
    }
    std::str::from_utf8(bytes).map_err(|_| error("theme must be UTF-8"))
}
pub(crate) fn parse_toml(bytes: &[u8]) -> Result<Theme, ThemeError> {
    let source = bounded_source(bytes)?;
    let native: Native = toml::from_str(source).map_err(|e| error(format!("theme TOML: {e}")))?;
    if native.version != 1 {
        return Err(error("unsupported theme TOML version"));
    }
    let scheme = match native.base.as_str() {
        "dark" => Scheme::Dark,
        "light" => Scheme::Light,
        _ => return Err(error("base must be dark or light")),
    };
    let mut theme = Theme::builtin(scheme);
    theme.name = native.name;
    for (key, color) in native.ui {
        let role = UiRole::from_key(&key).ok_or_else(|| error(format!("unsupported UI colour: {key}")))?;
        theme.ui.set(role, Rgba::parse(&color)?);
    }
    for (scope, style) in native.syntax {
        theme.syntax.insert(
            scope,
            SyntaxStyle {
                foreground: style.foreground.map(|c| Rgba::parse(&c)).transpose()?,
                background: style.background.map(|c| Rgba::parse(&c)).transpose()?,
                font_style: style.font_style.map(|s| font_flags(&s)).transpose()?,
            },
        );
    }
    theme.validate()?;
    Ok(theme)
}
fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}
pub(crate) fn to_toml(theme: &Theme) -> Result<String, ThemeError> {
    theme.validate()?;
    let mut text = format!(
        "version = 1\nname = {}\nbase = {}\n\n[ui]\n",
        quote(&theme.name),
        quote(if theme.scheme == Scheme::Dark { "dark" } else { "light" })
    );
    for role in UiRole::ALL {
        text.push_str(&format!("{} = {}\n", quote(role.key()), quote(&theme.ui.get(*role).hex())));
    }
    for (scope, style) in &theme.syntax {
        text.push_str(&format!("\n[syntax.{}]\n", quote(scope)));
        if let Some(c) = style.foreground {
            text.push_str(&format!("foreground = {}\n", quote(&c.hex())));
        }
        if let Some(c) = style.background {
            text.push_str(&format!("background = {}\n", quote(&c.hex())));
        }
        if let Some(flags) = style.font_style {
            let words: Vec<_> = [(1, "bold"), (2, "italic"), (4, "underline"), (8, "strikethrough")]
                .into_iter()
                .filter_map(|(f, s)| (flags & f != 0).then_some(s))
                .collect();
            text.push_str(&format!("font_style = {}\n", quote(&words.join(" "))));
        }
    }
    if text.len() > MAX_THEME_BYTES {
        return Err(error("serialized theme exceeds 1 MiB"));
    }
    Ok(text)
}
fn font_flags(style: &str) -> Result<u8, ThemeError> {
    let mut flags = 0;
    for word in style.split_whitespace() {
        flags |= match word {
            "bold" => 1,
            "italic" => 2,
            "underline" => 4,
            "strikethrough" => 8,
            _ => return Err(error(format!("unsupported font style: {word}"))),
        };
    }
    Ok(flags)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportFormat {
    VsCodeJson,
    TextMate,
}
pub(crate) fn import(format: ImportFormat, base: Scheme, bytes: &[u8]) -> Result<Theme, ThemeError> {
    let source = bounded_source(bytes)?;
    let value = match format {
        ImportFormat::VsCodeJson => {
            serde_json::from_str::<UniqueJson>(source)
                .map_err(|e| {
                    error(format!("VS Code import requires strict JSON (no JSONC/comments/trailing commas): {e}"))
                })?
                .0
        }
        ImportFormat::TextMate => xml_plist(source)?,
    };
    import_value(format, base, value)
}
// serde_json::Value alone silently keeps the last duplicate key. Reject such
// ambiguous input at every nesting level, using the parser's normal depth cap.
struct UniqueJson(Value);
impl<'de> Deserialize<'de> for UniqueJson {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = UniqueJson;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("JSON value with unique keys")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut out = Map::new();
                while let Some((k, v)) = map.next_entry::<String, UniqueJson>()? {
                    if out.insert(k, v.0).is_some() {
                        return Err(de::Error::custom("duplicate JSON key"));
                    }
                }
                Ok(UniqueJson(Value::Object(out)))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut out = Vec::new();
                while let Some(v) = seq.next_element::<UniqueJson>()? {
                    out.push(v.0);
                }
                Ok(UniqueJson(Value::Array(out)))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::String(v.into())))
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::Bool(v)))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::Number(v.into())))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::Number(v.into())))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
                serde_json::Number::from_f64(v)
                    .map(|n| UniqueJson(Value::Number(n)))
                    .ok_or_else(|| de::Error::custom("invalid number"))
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::Null))
            }
        }
        d.deserialize_any(V)
    }
}
fn object(v: Value) -> Result<Map<String, Value>, ThemeError> {
    if let Value::Object(m) = v { Ok(m) } else { Err(error("expected theme object/dictionary")) }
}
fn string(v: Value) -> Result<String, ThemeError> {
    if let Value::String(s) = v { Ok(s) } else { Err(error("expected string")) }
}
fn finish(map: &Map<String, Value>) -> Result<(), ThemeError> {
    if let Some(key) = map.keys().next() { Err(error(format!("unsupported import field: {key}"))) } else { Ok(()) }
}
fn import_value(format: ImportFormat, base: Scheme, v: Value) -> Result<Theme, ThemeError> {
    let mut root = object(v)?;
    let name = root.remove("name").map(string).transpose()?.unwrap_or_else(|| "Imported theme".into());
    for key in ["$schema", "uuid", "author"] {
        if let Some(value) = root.remove(key) {
            string(value)?;
        }
    }
    let mut theme = Theme::builtin(match root.remove("type").map(string).transpose()?.as_deref() {
        None => base,
        Some("dark") => Scheme::Dark,
        Some("light") => Scheme::Light,
        Some(_) => return Err(error("unsupported theme type")),
    });
    theme.name = name;
    if format == ImportFormat::VsCodeJson {
        if let Some(colors) = root.remove("colors") {
            for (key, value) in object(colors)? {
                let role = match key.as_str() {
                    "editor.background" => UiRole::Background,
                    "editor.foreground" => UiRole::Foreground,
                    "editorGutter.background" => UiRole::GutterBackground,
                    "editorLineNumber.foreground" => UiRole::GutterForeground,
                    "statusBar.background" => UiRole::StatusBackground,
                    "statusBar.foreground" => UiRole::StatusForeground,
                    "editor.selectionBackground" => UiRole::Selection,
                    "editorCursor.foreground" => UiRole::Cursor,
                    "editor.lineHighlightBackground" => UiRole::LineHighlight,
                    "editorWhitespace.foreground" => UiRole::Whitespace,
                    "tab.inactiveBackground" => UiRole::TabBackground,
                    "tab.inactiveForeground" => UiRole::TabForeground,
                    "tab.activeBackground" => UiRole::ActiveTabBackground,
                    "tab.activeForeground" => UiRole::ActiveTabForeground,
                    "quickInput.background" => UiRole::PanelBackground,
                    "quickInput.foreground" => UiRole::PanelForeground,
                    "focusBorder" => UiRole::Border,
                    "textLink.foreground" => UiRole::Accent,
                    "editor.findMatchBackground" => UiRole::SearchMatch,
                    "diffEditor.insertedTextBackground" => UiRole::Added,
                    "diffEditor.removedTextBackground" => UiRole::Removed,
                    "errorForeground" => UiRole::Error,
                    _ => return Err(error(format!("unsupported VS Code UI colour: {key}"))),
                };
                theme.ui.set(role, Rgba::parse(&string(value)?)?);
            }
        }
    }
    let rules = root.remove(if format == ImportFormat::VsCodeJson { "tokenColors" } else { "settings" });
    finish(&root)?; // include, semantic tokens and unknown metadata cannot be discarded.
    if let Some(rules) = rules {
        let Value::Array(rules) = rules else {
            return Err(error("token colours/settings must be an array; external files unsupported"));
        };
        if rules.len() > MAX_SCOPES {
            return Err(error("too many imported rules"));
        }
        for rule in rules {
            let mut rule = object(rule)?;
            if let Some(name) = rule.remove("name") {
                string(name)?;
            }
            let scope = rule.remove("scope");
            let mut settings = object(rule.remove("settings").ok_or_else(|| error("rule needs settings"))?)?;
            finish(&rule)?;
            if scope.is_none() {
                for (key, value) in settings {
                    let role = match key.as_str() {
                        "background" => UiRole::Background,
                        "foreground" => UiRole::Foreground,
                        "caret" => UiRole::Cursor,
                        "selection" => UiRole::Selection,
                        "invisibles" => UiRole::Whitespace,
                        "lineHighlight" => UiRole::LineHighlight,
                        _ => return Err(error(format!("unsupported global token setting: {key}"))),
                    };
                    theme.ui.set(role, Rgba::parse(&string(value)?)?);
                }
                continue;
            }
            let style = SyntaxStyle {
                foreground: settings
                    .remove("foreground")
                    .map(string)
                    .transpose()?
                    .map(|v| Rgba::parse(&v))
                    .transpose()?,
                background: settings
                    .remove("background")
                    .map(string)
                    .transpose()?
                    .map(|v| Rgba::parse(&v))
                    .transpose()?,
                font_style: settings
                    .remove("fontStyle")
                    .map(string)
                    .transpose()?
                    .map(|s| font_flags(&s))
                    .transpose()?,
            };
            finish(&settings)?;
            let scopes = match scope.unwrap() {
                Value::String(s) => vec![s],
                Value::Array(a) => a.into_iter().map(string).collect::<Result<Vec<_>, _>>()?,
                _ => return Err(error("scope must be a string or string array")),
            };
            if scopes.is_empty() {
                return Err(error("empty scope array"));
            }
            for selectors in scopes {
                for scope in selectors.split(',').map(str::trim) {
                    validate_scope(scope)?;
                    // Repeated selectors are ordered, property-wise overlays.
                    let old = theme.syntax.entry(scope.into()).or_default();
                    if style.foreground.is_some() {
                        old.foreground = style.foreground;
                    }
                    if style.background.is_some() {
                        old.background = style.background;
                    }
                    if style.font_style.is_some() {
                        old.font_style = style.font_style;
                    }
                    if theme.syntax.len() > MAX_SCOPES {
                        return Err(error("too many expanded scopes"));
                    }
                }
            }
        }
    }
    theme.validate()?;
    Ok(theme)
}

fn xml_plist(source: &str) -> Result<Value, ThemeError> {
    use quick_xml::{Reader, events::Event};
    let mut reader = Reader::from_str(source);
    let mut nodes = 0;
    // A small structural plist reader on top of a real XML tokenizer. No DTD,
    // external entity resolver, filesystem include or network path exists.
    fn next<'a>(r: &mut Reader<&'a [u8]>, mut allow_declaration: bool) -> Result<Event<'a>, ThemeError> {
        loop {
            let event = r.read_event().map_err(|e| error(format!("tmTheme XML: {e}")))?;
            match &event {
                Event::Comment(_) => continue,
                Event::Decl(d) => {
                    if !allow_declaration {
                        return Err(error("XML declaration is only allowed once before the plist root"));
                    }
                    allow_declaration = false;
                    if d.version().map_err(|e| error(e.to_string()))?.as_ref() != b"1.0" {
                        return Err(error("only XML 1.0 is supported"));
                    }
                    if let Some(encoding) = d.encoding() {
                        if !encoding.map_err(|e| error(e.to_string()))?.eq_ignore_ascii_case(b"utf-8") {
                            return Err(error("only UTF-8 XML is supported"));
                        }
                    }
                    continue;
                }
                Event::Text(t) if t.as_ref().iter().all(u8::is_ascii_whitespace) => continue,
                Event::DocType(_) => return Err(error("tmTheme DTDs/entities are unsupported; remove the DOCTYPE")),
                _ => return Ok(event),
            }
        }
    }
    fn text(r: &mut Reader<&[u8]>, tag: &[u8]) -> Result<String, ThemeError> {
        let mut out = String::new();
        loop {
            match r.read_event().map_err(|e| error(e.to_string()))? {
                Event::Text(t) => out.push_str(&t.xml10_content().map_err(|e| error(e.to_string()))?),
                Event::CData(t) => out.push_str(&t.decode().map_err(|e| error(e.to_string()))?),
                Event::GeneralRef(t) => {
                    let name = t.decode().map_err(|e| error(e.to_string()))?;
                    let encoded = format!("&{name};");
                    out.push_str(&quick_xml::escape::unescape(&encoded).map_err(|e| error(e.to_string()))?);
                }
                Event::End(e) if e.name().as_ref() == tag => return Ok(out),
                _ => return Err(error("unsupported XML inside theme string")),
            }
        }
    }
    fn value(r: &mut Reader<&[u8]>, event: Event<'_>, depth: usize, nodes: &mut usize) -> Result<Value, ThemeError> {
        *nodes += 1;
        if depth > 16 || *nodes > 16384 {
            return Err(error("tmTheme plist depth/node bound exceeded"));
        }
        let Event::Start(start) = event else {
            if let Event::Empty(e) = event {
                if e.attributes().next().is_some() {
                    return Err(error("unsupported plist attributes"));
                }
                return match e.name().as_ref() {
                    b"string" => Ok(Value::String(String::new())),
                    b"dict" => Ok(Value::Object(Map::new())),
                    b"array" => Ok(Value::Array(Vec::new())),
                    _ => Err(error("unsupported plist value")),
                };
            }
            return Err(error("expected plist value"));
        };
        if start.attributes().next().is_some() {
            return Err(error("unsupported plist attributes"));
        }
        match start.name().as_ref() {
            b"string" => Ok(Value::String(text(r, b"string")?)),
            b"array" => {
                let mut out = Vec::new();
                loop {
                    let event = next(r, false)?;
                    if matches!(&event, Event::End(e) if e.name().as_ref() == b"array") {
                        return Ok(Value::Array(out));
                    }
                    out.push(value(r, event, depth + 1, nodes)?);
                }
            }
            b"dict" => {
                let mut out = Map::new();
                loop {
                    let event = next(r, false)?;
                    if matches!(&event, Event::End(e) if e.name().as_ref() == b"dict") {
                        return Ok(Value::Object(out));
                    }
                    if !matches!(&event, Event::Start(e) if e.name().as_ref() == b"key" && e.attributes().next().is_none())
                    {
                        return Err(error("expected plist key"));
                    }
                    let key = text(r, b"key")?;
                    let event = next(r, false)?;
                    if out.insert(key, value(r, event, depth + 1, nodes)?).is_some() {
                        return Err(error("duplicate plist key"));
                    }
                }
            }
            _ => Err(error("unsupported plist value type")),
        }
    }
    let Event::Start(root) = next(&mut reader, true)? else {
        return Err(error("expected XML plist"));
    };
    if root.name().as_ref() != b"plist" {
        return Err(error("expected plist root"));
    }
    for attribute in root.attributes() {
        let attribute = attribute.map_err(|e| error(e.to_string()))?;
        if attribute.key.as_ref() != b"version" || attribute.value.as_ref() != b"1.0" {
            return Err(error("unsupported plist root attribute"));
        }
    }
    let event = next(&mut reader, false)?;
    let result = value(&mut reader, event, 0, &mut nodes)?;
    if !matches!(next(&mut reader, false)?, Event::End(e) if e.name().as_ref() == b"plist")
        || !matches!(next(&mut reader, false)?, Event::Eof)
    {
        return Err(error("trailing or unclosed plist"));
    }
    Ok(result)
}
