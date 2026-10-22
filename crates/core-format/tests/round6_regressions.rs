use kwil_core_format::{FormatResult, Language, Settings, format, format_selection};

fn apply(input: &str, result: &FormatResult) -> String {
    let mut applied = input.to_owned();
    if let Some(range) = &result.replacement {
        applied.replace_range(range.clone(), std::str::from_utf8(&result.bytes).unwrap());
    }
    applied
}

#[test]
fn sql_backslash_quote_ambiguity_is_preserved_or_refused() {
    let input = r"select '\', 'from ' /* ' */;";
    match format(input.as_bytes(), Language::Sql, "sql", None, &Settings::default()) {
        Ok(result) => assert_eq!(result.bytes, input.as_bytes()),
        Err(error) => assert!(error.message.contains("backslash") && error.message.contains("dialect"), "{error}"),
    }
}

#[test]
fn generic_selection_does_not_trim_token_separator_at_partial_line_end() {
    let input = "fn f(value: i32) { let n = value as i64; }";
    let start = input.rfind("value ").unwrap();
    let result =
        format(input.as_bytes(), Language::Generic, "rust", Some(start..start + "value ".len()), &Settings::default())
            .unwrap();
    assert_eq!(apply(input, &result), input);
    assert_eq!(result.bytes, b"value ");
    assert_eq!(result.replacement, None);
}

#[test]
fn xml_selection_does_not_lose_inherited_space_context() {
    let input = "<r xml:space=\"preserve\"><a><b/></a></r>";
    let fragment = "<a><b/></a>";
    let start = input.find(fragment).unwrap();
    let result =
        format(input.as_bytes(), Language::Xml, "xml", Some(start..start + fragment.len()), &Settings::default())
            .unwrap();
    assert_eq!(apply(input, &result), input);
    assert_eq!(result.bytes, fragment.as_bytes());
    assert_eq!(result.replacement, None);
    let result = format_selection(fragment.as_bytes(), Language::Xml, "xml", start, 1, &Settings::default()).unwrap();
    assert_eq!(result.bytes, fragment.as_bytes());
    assert_eq!(result.replacement, None);
    assert!(result.notices[0].contains("xml:space context"));
}

#[test]
fn java_unicode_preprocessing_cannot_hide_text_block_openers() {
    for escape in [r"\u002a", r"\uuuu002a"] {
        let input = format!("class T {{\n  /* {escape}/ static String s = \"\"\"\n*/\n\tkeep\n\"\"\";\n}}\n");
        let settings = Settings { reindent: true, ..Settings::default() };
        let result = format(input.as_bytes(), Language::Generic, "java", None, &settings).unwrap();
        assert_eq!(result.bytes, input.as_bytes());
        assert_eq!(result.replacement, None);
    }
}
