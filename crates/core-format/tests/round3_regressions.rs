use kwil_core_format::{Indent, Language, Settings, format};

fn unchanged(input: &str, kind: &str, settings: &Settings) {
    let result = format(input.as_bytes(), Language::Generic, kind, None, settings).unwrap();
    assert_eq!(result.bytes, input.as_bytes(), "{kind}");
    assert_eq!(result.replacement, None);
}

#[test]
fn php_heredoc_closes_before_following_expression() {
    for marker in ["END", "  END"] {
        for opener in ["<<<END", "<<<'END'", "<<<\"END\""] {
            for body in ["body  ", "ENDé\nbody  "] {
                let body =
                    if marker.starts_with(' ') { format!("  {}", body.replace('\n', "\n  ")) } else { body.into() };
                let input = format!("<?php\n$s = {opener}\n{body}\n{marker} . \"first\nsecond  \nthird\";\n");
                unchanged(&input, "php", &Settings::default());
            }
        }
    }
}

#[test]
fn php_attributes_are_not_hash_comments() {
    let input = "<?php\n#[Attr(\"first\nsecond  \nthird\")]\nfunction f() {}\n";
    unchanged(input, "php", &Settings::default());
}

#[test]
fn javascript_unicode_line_terminators_end_line_comments() {
    for sep in ['\u{2028}', '\u{2029}'] {
        for kind in ["javascript", "typescript", "js", "ts"] {
            let input = format!("// comment{sep}const s = `first\nsecond  \nthird`;\n");
            unchanged(&input, kind, &Settings::default());
        }
    }
}

#[test]
fn shell_carriage_returns_are_argument_bytes() {
    for kind in ["shell", "sh", "bash", "zsh", "ksh"] {
        for input in [
            "printf foo\rbar\n",
            "printf foo\r\n",
            "# comment\rnot-a-new-line\nprintf foo\rbar\n",
            "printf foo\r#bar\"first\nsecond  \nthird\"\n",
        ] {
            unchanged(input, kind, &Settings::default());
        }
    }
}

#[test]
fn sql_hash_dialects_are_preserved_or_refused() {
    for input in ["select 1; # from users\nselect 2;\n", "select x #>> '{key}' from t;\n", "select * from #temp;\n"] {
        match format(input.as_bytes(), Language::Sql, "sql", None, &Settings::default()) {
            Ok(result) => {
                assert_eq!(result.bytes, input.as_bytes());
                assert_eq!(result.replacement, None);
            }
            Err(error) => assert!(error.message.contains("dialect"), "{error}"),
        }
    }
}

#[test]
fn affected_boundaries_remain_safe_with_all_tidy_options() {
    let settings = Settings { reindent: true, indent: Indent::Tabs { width: 4 }, ..Settings::default() };
    for (kind, input) in [
        ("php", "<?php\n$s = <<<END\nbody  \nEND . \"first\n    second  \nthird\";\n"),
        ("php", "<?php\n#[Attr(\"first\n    second  \nthird\")]\n"),
        ("ts", "// comment\u{2028}const s = `first\n    second  \nthird`;\n"),
        ("sh", "printf foo\rbar\n"),
    ] {
        unchanged(input, kind, &settings);
    }
}
