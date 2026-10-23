use kwil_core_format::{
    ExternalFormatter, FormatResult, Indent, Language, MAX_INPUT_BYTES, MAX_OUTPUT_BYTES, Settings, check_size, format,
};
use std::{
    ops::Range,
    time::{Duration, Instant},
};

fn run(text: &str, lang: Language) -> FormatResult {
    format(text.as_bytes(), lang, "", None, &Settings::default()).unwrap()
}
fn golden(input: &str, expected: &str, lang: Language) {
    let once = run(input, lang);
    assert_eq!(String::from_utf8_lossy(&once.bytes), expected);
    let twice = run(expected, lang);
    assert_eq!(twice.bytes, once.bytes);
    assert_eq!(twice.replacement, None);
}

#[test]
fn json_golden() {
    golden(
        r#"{ "z":1e+09,"a":-0,"z":123456789012345678901234567890,"s":"a  \n b","empty":[] }"#,
        "{\n  \"z\": 1e+09,\n  \"a\": -0,\n  \"z\": 123456789012345678901234567890,\n  \"s\": \"a  \\n b\",\n  \"empty\": []\n}\n",
        Language::Json,
    );
}

#[test]
fn json_indent_and_file_style() {
    let s = Settings { indent: Indent::Tabs { width: 4 }, ..Settings::default() };
    let result = format(b"{\r\n\"a\":[1]}", Language::Json, "", None, &s).unwrap();
    assert_eq!(result.bytes, b"{\r\n\t\"a\": [\r\n\t\t1\r\n\t]\r\n}\r\n");
    assert_eq!(result.replacement, Some(0..11));
}

#[test]
fn xml_golden() {
    golden(
        "<root x = 'a > b'><item a=\"v\">text  here</item><empty/></root>",
        "<root x = 'a > b'>\n  <item a=\"v\">text  here</item>\n  <empty/>\n</root>\n",
        Language::Xml,
    );
}

#[test]
fn html_golden_and_raw_blocks() {
    golden(
        "<div><p>Hello <b>world</b> !</p><pre> x  \n y </pre><script>if (a < b) { x++; }</script><style>a { color: red; }</style><br></div>",
        "<div><p>Hello <b>world</b> !</p><pre> x  \n y </pre><script>if (a < b) { x++; }</script><style>a { color: red; }</style><br></div>\n",
        Language::Html,
    );
}

#[test]
fn toml_golden() {
    golden(
        "# retained\nz=1 # inline\na={x=2,y='raw'}\n",
        "# retained\nz = 1 # inline\na = { x = 2, y = 'raw' }\n",
        Language::Toml,
    );
}

#[test]
fn yaml_tidy_preserves_semantics() {
    let input = "a: |+\n    text  \n\nlist:\n  - value\n";
    let s = Settings { reindent: true, indent: Indent::Tabs { width: 4 }, ..Settings::default() };
    let a = format(input.as_bytes(), Language::Yaml, "", None, &s).unwrap();
    assert_eq!(a.bytes, input.as_bytes());
    assert_eq!(a.replacement, None);
    assert!(a.notices[0].contains("tidy only"));
    assert_eq!(format(&a.bytes, Language::Yaml, "", None, &s).unwrap(), a);
}

#[test]
fn css_golden() {
    golden(
        "a{color:red;background:url('a;b')}/* keep */",
        "a{\n  color:red;\n  background:url('a;b')\n}\n/* keep */\n",
        Language::Css,
    );
}

#[test]
fn sql_golden() {
    golden(
        "select 'where  x', a from items where a=1 order by a;",
        "SELECT 'where  x', a\nFROM items\nWHERE a=1\nORDER BY a;\n",
        Language::Sql,
    );
}

#[test]
fn generic_golden() {
    let a = format(b"  a  \r\n\tb \n", Language::Generic, "rust", None, &Settings::default()).unwrap();
    assert_eq!(a.bytes, b"  a\r\n\tb\r\n");
    assert_eq!(format(&a.bytes, Language::Generic, "rust", None, &Settings::default()).unwrap().replacement, None);
}

#[test]
fn generic_optional_steps_and_visual_indent() {
    let input = b"\t x \r\n    y\n";
    let s = Settings {
        reindent: true,
        indent: Indent::Spaces(4),
        final_newline: false,
        normalize_line_endings: false,
        trim_trailing_whitespace: false,
        ..Settings::default()
    };
    let a = format(input, Language::Generic, "rust", None, &s).unwrap();
    assert_eq!(a.bytes, b"     x \r\n    y\n");
    assert_eq!(format(&a.bytes, Language::Generic, "rust", None, &s).unwrap().bytes, a.bytes);
    let s = Settings { reindent: false, ..s };
    assert_eq!(format(input, Language::Generic, "rust", None, &s).unwrap().bytes, input);
    let s = Settings { reindent: true, indent: Indent::Tabs { width: 4 }, ..s };
    assert_eq!(format(b"     x", Language::Generic, "rust", None, &s).unwrap().bytes, b"\t x");
}

#[test]
fn malformed_input_has_line_number() {
    for (lang, input, line) in [
        (Language::Json, "{\n}", 1), // valid control checked below
        (Language::Json, "{\n\"a\":}", 2),
        (Language::Xml, "<a>\n</b>", 2),
        (Language::Html, "<div>\n</span>", 2),
        (Language::Toml, "a=1\nb=]", 2),
        (Language::Css, "a {\n x: 'bad", 2),
        (Language::Sql, "SELECT 1\nWHERE x='bad", 2),
    ] {
        if input == "{\n}" {
            assert!(format(input.as_bytes(), lang, "", None, &Settings::default()).is_ok());
            continue;
        }
        let e = format(input.as_bytes(), lang, "", None, &Settings::default()).unwrap_err();
        assert_eq!(e.line, line, "{e}");
        assert!(e.message.contains(&format!("line {line}")));
    }
    let e = format(b"{ // comment\n}", Language::Json, "", None, &Settings::default()).unwrap_err();
    assert!(e.message.starts_with("JSON:"));
}

fn selection(input: &str, range: Range<usize>, lang: Language) -> FormatResult {
    format(input.as_bytes(), lang, "rust", Some(range), &Settings::default()).unwrap()
}

#[test]
fn selection_is_one_replacement_without_touching_surroundings() {
    let input = "prefix {\"a\":1} suffix";
    let a = selection(input, 7..14, Language::Json);
    assert_eq!(a.bytes, b"{\n  \"a\": 1\n}");
    assert_eq!(a.replacement, Some(7..14));
    let mut applied = input.to_owned();
    applied.replace_range(a.replacement.unwrap(), std::str::from_utf8(&a.bytes).unwrap());
    assert_eq!(applied, "prefix {\n  \"a\": 1\n} suffix");
    // R6: an embedded XML fragment has no proven inherited xml:space context.
    // The former reindent expectation silently discarded that context.
    let a = selection("x<a><b/></a>y", 1..12, Language::Xml);
    assert_eq!(a.bytes, b"<a><b/></a>");
    assert_eq!(a.replacement, None);
    // Trimming a partial final line would merge the selected `a` with `y`.
    let a = selection("x a  y", 1..5, Language::Generic);
    assert_eq!(a.bytes, b" a  ");
    assert_eq!(a.replacement, None);
    // Complete/root XML and generic selections ending at true EOF still format.
    assert_eq!(selection("<a><b/></a>", 0..11, Language::Xml).bytes, b"<a>\n  <b/>\n</a>");
    assert_eq!(
        kwil_core_format::format_selection(b"<a><b/></a>", Language::Xml, "xml", 0, 1, &Settings::default())
            .unwrap()
            .bytes,
        b"<a>\n  <b/>\n</a>"
    );
    assert_eq!(selection("x a  ", 1..5, Language::Generic).bytes, b" a");
}

#[test]
fn unsupported_and_invalid_selection_errors() {
    for lang in [Language::Css, Language::Sql, Language::Toml, Language::Yaml] {
        let e = format(b"x", lang, "", Some(0..1), &Settings::default()).unwrap_err();
        assert!(e.message.contains("not supported for a selection"));
    }
    for range in [1..2, 4..8, Range { start: 3, end: 1 }] {
        assert!(format("éx".as_bytes(), Language::Generic, "", Some(range), &Settings::default()).is_err());
    }
    let e = format(b"prefix\n{\"x\":}", Language::Json, "", Some(7..13), &Settings::default()).unwrap_err();
    assert_eq!(e.line, 2);
}

#[test]
fn memory_and_settings_guards() {
    assert!(check_size(MAX_INPUT_BYTES + 1, &Settings::default()).unwrap_err().message.contains("formatting limit"));
    for s in [
        Settings { max_input_bytes: MAX_INPUT_BYTES + 1, ..Settings::default() },
        Settings { max_output_bytes: MAX_OUTPUT_BYTES + 1, ..Settings::default() },
        Settings { indent: Indent::Spaces(0), ..Settings::default() },
    ] {
        assert!(check_size(0, &s).is_err());
    }
    let s = Settings { max_output_bytes: 4, ..Settings::default() };
    assert!(format(b"{\"a\":1}", Language::Json, "", None, &s).unwrap_err().message.contains("output exceeds"));
    assert!(format(&[255], Language::Generic, "", None, &Settings::default()).unwrap_err().message.contains("UTF-8"));
    let deep = format!("{}0{}", "[".repeat(129), "]".repeat(129));
    assert!(format(deep.as_bytes(), Language::Json, "", None, &Settings::default()).is_err());
}

fn external_settings(mode: &str, timeout: Duration) -> Settings {
    let mut s = Settings::default();
    s.external.insert(
        "custom".into(),
        ExternalFormatter {
            command: env!("CARGO_BIN_EXE_format-test-helper").into(),
            args: vec![mode.into()],
            timeout,
        },
    );
    s
}

#[test]
fn external_stdin_stdout_on_worker() {
    let s = external_settings("echo", Duration::from_secs(3));
    let result = std::thread::spawn(move || format(b"exact  \r\n", Language::Generic, "custom", None, &s))
        .join()
        .unwrap()
        .unwrap();
    assert_eq!(result.bytes, b"exact  \r\n");
    assert_eq!(result.replacement, None);
    let s = external_settings("echo", Duration::from_secs(3));
    assert!(format(b"x", Language::Generic, "custom", Some(0..1), &s).unwrap_err().message.contains("not supported"));
}

#[test]
fn external_timeout_reaps_child() {
    let s = external_settings("sleep", Duration::from_millis(60));
    let start = Instant::now();
    let e = format(b"x", Language::Generic, "custom", None, &s).unwrap_err();
    assert!(e.message.contains("timed out"), "{e}");
    assert!(e.message.contains("reap=true"), "{e}");
    assert!(start.elapsed() < Duration::from_secs(3));
}

#[test]
fn external_failures_are_messages() {
    for (mode, expected) in [("exit", "exited"), ("invalid", "UTF-8"), ("flood", "output exceeds")] {
        let mut s = external_settings(mode, Duration::from_secs(3));
        s.max_output_bytes = 1024;
        assert!(format(b"x", Language::Generic, "custom", None, &s).unwrap_err().message.contains(expected));
    }
    let mut s = external_settings("echo", Duration::ZERO);
    assert!(format(b"x", Language::Generic, "custom", None, &s).unwrap_err().message.contains("timeout must"));
    s.external.get_mut("custom").unwrap().command = "kwil-nonexistent-formatter-helper".into();
    s.external.get_mut("custom").unwrap().timeout = Duration::from_secs(1);
    assert!(format(b"x", Language::Generic, "custom", None, &s).is_err());
}

#[test]
fn markup_preserves_whitespace_text_and_xml_space() {
    golden("<a path='c:\\'/>", "<a path='c:\\'/>\n", Language::Xml);
    golden("<r><a>  </a><b>\n </b></r>", "<r>\n  <a>  </a>\n  <b>\n </b>\n</r>\n", Language::Xml);
    golden("<p><b>a</b> <b>b</b></p>", "<p><b>a</b> <b>b</b></p>\n", Language::Html);
    golden("<r xml:space='preserve'><a/>\n <b/>  </r>", "<r xml:space='preserve'><a/>\n <b/>  </r>\n", Language::Xml);
    for input in ["<a b=>x</a>", "<a b='x'b='y'/>", "<a b='x' b='y'/>", "<0/>", "<a></a x='y'>"] {
        assert!(format(input.as_bytes(), Language::Xml, "", None, &Settings::default()).is_err(), "{input}");
    }
}

#[test]
fn sql_preserves_dialect_literals_identifiers_and_comments() {
    let input = "select [from], $tag$where  x$tag$, $$select\n x$$ from t -- keep where\nwhere x='it''s  x';";
    let error = format(input.as_bytes(), Language::Sql, "", None, &Settings::default()).unwrap_err();
    assert!(error.message.contains("dialect") && error.message.contains("external formatter"), "{error}");
    let error = format(b"select [from] from t;", Language::Sql, "", None, &Settings::default()).unwrap_err();
    assert!(error.message.contains("dialect") && error.message.contains("external formatter"), "{error}");
    golden(
        "select 'where  x', \"from\" from t /* keep where */ where x='it''s  x';",
        "SELECT 'where  x', \"from\"\nFROM t /* keep where */\nWHERE x='it''s  x';\n",
        Language::Sql,
    );
    assert!(
        format(b"select $$unterminated", Language::Sql, "", None, &Settings::default())
            .unwrap_err()
            .message
            .contains("line 1")
    );
}

#[test]
fn huge_document_small_selection_is_admitted_without_document_copy() {
    let at = MAX_INPUT_BYTES * 100;
    let a =
        kwil_core_format::format_selection(b"{\"x\":1}", Language::Json, "", at, 10000, &Settings::default()).unwrap();
    assert_eq!(a.replacement, Some(at..at + 7));
    assert_eq!(a.bytes, b"{\n  \"x\": 1\n}");
    assert!(
        kwil_core_format::format_selection(b"xxx", Language::Generic, "", usize::MAX, 1, &Settings::default()).is_err()
    );
}
