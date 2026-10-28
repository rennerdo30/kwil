use kwil_core_format::{Indent, Language, Settings, format};

fn sql_or_refusal(input: &str, expected: &str) {
    match format(input.as_bytes(), Language::Sql, "sql", None, &Settings::default()) {
        Ok(result) => assert_eq!(result.bytes, expected.as_bytes()),
        Err(error) => {
            assert!(error.message.contains("dialect") && error.message.contains("external formatter"), "{error}")
        }
    }
}

#[test]
fn sql_parameter_names_are_not_keyword_tokens() {
    for prefix in [":", "@", "?"] {
        for name in ["from", "where", "order", "select"] {
            let parameter = format!("{prefix}{name}");
            sql_or_refusal(&format!("select {parameter} from t;\n"), &format!("SELECT {parameter}\nFROM t;\n"));
        }
    }
}

#[test]
fn sql_prefixed_literals_are_preserved_or_refused() {
    for literal in [
        "q'[a 'from' b]'",
        "Q'{a 'where' b}'",
        "nq'<a 'order' b>'",
        "q'!a 'select' b!'",
        "N'a''from''b'",
        "U&'a\\0027from'",
    ] {
        sql_or_refusal(&format!("select {literal} from t;\n"), &format!("SELECT {literal}\nFROM t;\n"));
    }
}

#[test]
fn cpp_spliced_block_comments_cannot_hide_raw_string_openers() {
    for splice in ["\\\n", "\\\r\n", "\\ \n", "??/\n"] {
        let input = format!("/* comment *{splice}/\nconst char* s = R\"tag(first */\n    second  \nthird)tag\";\n");
        for kind in ["cpp", "c++", "hpp"] {
            for settings in [
                Settings::default(),
                Settings { reindent: true, indent: Indent::Tabs { width: 4 }, ..Settings::default() },
            ] {
                let result = format(input.as_bytes(), Language::Generic, kind, None, &settings).unwrap();
                assert_eq!(result.bytes, input.as_bytes(), "{kind} {splice:?}");
                assert_eq!(result.replacement, None);
            }
        }
    }
}
