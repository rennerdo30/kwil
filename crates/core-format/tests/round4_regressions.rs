use kwil_core_format::{Indent, Language, Settings, format};

#[test]
fn sql_unicode_identifier_atoms_are_not_split_into_keywords() {
    for identifier in ["éfrom", "fromé", "aéfrom", "e\u{301}from", "漢where", "𐐀from", "where\u{301}", "él$from"]
    {
        let input = format!("select {identifier} from t;\n");
        match format(input.as_bytes(), Language::Sql, "sql", None, &Settings::default()) {
            Ok(result) => assert_eq!(result.bytes, format!("SELECT {identifier}\nFROM t;\n").as_bytes()),
            Err(error) => assert!(error.message.contains("identifier") && error.message.contains("dialect"), "{error}"),
        }
    }
}

fn unchanged_shell(input: &str, settings: &Settings) {
    let result = format(input.as_bytes(), Language::Generic, "sh", None, settings).unwrap();
    assert_eq!(result.bytes, input.as_bytes());
    assert_eq!(result.replacement, None);
}

#[test]
fn shell_heredoc_header_requires_a_proven_lexical_newline() {
    for quote in ['"', '\'', '`'] {
        let input = format!("printf '%s' <<EOF {quote}first\nEOF\nsecond  \nthird{quote}\nbody\nEOF\n");
        unchanged_shell(&input, &Settings::default());
    }
}

#[test]
fn uncertain_heredoc_headers_preserve_literals_with_every_tidy_option() {
    let settings = Settings { reindent: true, indent: Indent::Tabs { width: 4 }, ..Settings::default() };
    for header in ["\"first", "'first", "`first", "\\\n\"first", "$(printf \"first"] {
        let input = format!("printf '%s' <<EOF {header}\nEOF\n    second  \nthird\"\nbody\nEOF\n");
        unchanged_shell(&input, &settings);
    }
}

#[test]
fn sql_unicode_literals_and_comments_still_format() {
    let result = format(
        "select 'éfrom', \"漢where\" from t /* éwhere */;".as_bytes(),
        Language::Sql,
        "sql",
        None,
        &Settings::default(),
    )
    .unwrap();
    assert_eq!(result.bytes, "SELECT 'éfrom', \"漢where\"\nFROM t /* éwhere */;\n".as_bytes());
}
