use kwil_core_format::{Indent, Language, Settings, format};

fn sql_refuses_unproven_boundary(input: &str) {
    let error = format(input.as_bytes(), Language::Sql, "sql", None, &Settings::default()).unwrap_err();
    assert!(error.message.contains("comment boundary"), "{error}");
    assert!(error.message.contains("dialect") && error.message.contains("external formatter"), "{error}");
}

#[test]
fn sql_mysql_minus_operators_cannot_hide_a_literal_opener() {
    sql_refuses_unproven_boundary("select 1--2, 'a\nfrom b\n ' /* ' */;");
}

#[test]
fn sql_comment_boundaries_are_proven_without_entering_literals_or_comments() {
    for tail in ["2", "x", "+2", "'from '", "/", "\u{a0}from", ""] {
        sql_refuses_unproven_boundary(&format!("select 1--{tail}"));
    }
    for ending in ["\n", "\r\n", "\r"] {
        sql_refuses_unproven_boundary(&format!("select 1--2, 'a{ending}from b{ending} ' /* ' */;"));
    }
    // Shared comment boundaries keep their original token bytes. The newline
    // after -- is itself sufficient; controls are permitted by the same rule.
    for separator in [" ", "\t", "\u{b}", "\u{c}"] {
        let input = format!("select 1--{separator}keep 'from'\nfrom t;");
        let result = format(input.as_bytes(), Language::Sql, "sql", None, &Settings::default()).unwrap();
        assert_eq!(result.bytes, format!("SELECT 1--{separator}keep 'from'\nFROM t;\n").as_bytes());
    }
    let result = format(b"select 1--\nfrom t;", Language::Sql, "sql", None, &Settings::default()).unwrap();
    assert_eq!(result.bytes, b"SELECT 1--\nFROM t;\n");
    for (atom, nested) in [
        ("'--2 from'", false),
        ("'a\n--2 from b\n '", false),
        ("\"--from\"", false),
        ("[--from]", false),
        ("$$--2 from$$", false),
        ("$tag$--2 from$tag$", false),
        ("/* --2 'from' */ 1", false),
        ("/* outer /* --2 */ 'from' */ 1", true),
    ] {
        let input = format!("select {atom} from t;");
        if nested {
            // R8: retain this case as an explicit ambiguity refusal instead of
            // the former positive that silently selected nested-comment grammar.
            sql_refuses_unproven_boundary(&input);
            continue;
        }
        if atom.starts_with(['$', '[']) {
            let error = format(input.as_bytes(), Language::Sql, "sql", None, &Settings::default()).unwrap_err();
            assert!(error.message.contains("dialect") && error.message.contains("external formatter"), "{error}");
            continue;
        }
        let result = format(input.as_bytes(), Language::Sql, "sql", None, &Settings::default()).unwrap();
        assert_eq!(result.bytes, format!("SELECT {atom}\nFROM t;\n").as_bytes(), "{atom}");
    }
    // This admission rule is SQL-only; CSS custom-property atoms retain --.
    let css = format(b"a{--from:1;color:red}", Language::Css, "css", None, &Settings::default()).unwrap();
    assert_eq!(css.bytes, b"a{\n  --from:1;\n  color:red\n}\n");
}

#[test]
fn php_mixed_case_halt_compiler_preserves_payload_eof() {
    let input = b"<?php\n__HALT_COMPILER();\npayload  ";
    let result = format(input, Language::Generic, "php", None, &Settings::default()).unwrap();
    assert_eq!(result.bytes, input);
    assert_eq!(result.replacement, None);
}

#[test]
fn php_halt_case_variants_protect_data_without_disabling_code_tidy() {
    for name in ["__HaLt_CoMpIlEr", "__HALT_COMPILER", "__halt_compiler", "__halt_COMPILER"] {
        for ending in ["\n", "\r\n", "\r"] {
            for payload in ["payload  ", "\tkeep  \r\nlast  ", "'\"/* literal data  "] {
                let input = format!("<?php{ending}{name}();{ending}{payload}");
                for settings in [
                    Settings::default(),
                    Settings { reindent: true, indent: Indent::Tabs { width: 4 }, ..Settings::default() },
                ] {
                    let result = format(input.as_bytes(), Language::Generic, "php", None, &settings).unwrap();
                    assert_eq!(result.bytes, input.as_bytes(), "{name}, ending={ending:?}");
                    assert_eq!(result.replacement, None);
                }
            }
        }
    }
    // A proven literal/comment span containing a halt spelling does not end code.
    // The final ordinary line still trims and gains its configured final newline.
    for literal in ["'__HALT_COMPILER();'", "\"__HaLt_CoMpIlEr();\""] {
        let input = format!("<?php\n$s = {literal};  \n$x = 1;  ");
        let result = format(input.as_bytes(), Language::Generic, "php", None, &Settings::default()).unwrap();
        assert_eq!(result.bytes, format!("<?php\n$s = {literal};\n$x = 1;\n").as_bytes());
    }
    for comment in ["/* __HALT_COMPILER(); */", "// __HALT_COMPILER();"] {
        let input = format!("<?php\n{comment}\n$x = 1;  ");
        let result = format(input.as_bytes(), Language::Generic, "php", None, &Settings::default()).unwrap();
        assert_eq!(result.bytes, format!("<?php\n{comment}\n$x = 1;\n").as_bytes());
    }
    let input = b"<?php\n$x = 1;  \n__hAlT_cOmPiLeR();\npayload  ";
    let result = format(input, Language::Generic, "php", None, &Settings::default()).unwrap();
    assert_eq!(result.bytes, b"<?php\n$x = 1;\n__hAlT_cOmPiLeR();\npayload  ");
    assert_eq!(result.replacement, Some(0..input.len()));
}
