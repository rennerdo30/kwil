use kwil_core_format::{Indent, Language, Settings, format};

fn sql_refuses_nested_boundary(input: &str) {
    let error = format(input.as_bytes(), Language::Sql, "sql", None, &Settings::default()).unwrap_err();
    assert!(error.message.contains("nested block comment boundary"), "{error}");
    assert!(error.message.contains("dialect") && error.message.contains("external formatter"), "{error}");
}

#[test]
fn sql_sqlite_comment_closure_cannot_hide_a_literal_opener() {
    sql_refuses_nested_boundary("select 1 /* outer /* inner */ , 'a */\nfrom b\n ' /* ' */;");
}

#[test]
fn sql_nested_boundaries_are_refused_without_disabling_proven_atoms() {
    for input in [
        "select /* outer /* inner */ 'ignored */ 1;",
        "select /* one /* two */ three */ 1;",
        "select /* one /* two */ 1;",
        "select /* one ' /* from */ where */ 1;",
        "select /* outer /* --2 */ 'from' */ 1 from t;",
    ] {
        sql_refuses_nested_boundary(input);
    }
    for ending in ["\n", "\r\n", "\r"] {
        sql_refuses_nested_boundary(&format!("select 1 /* outer /* inner */ , 'a */{ending}from b{ending} ' /* ' */;"));
    }
    for atom in [
        "/* nonnested 'from' --2 */ 1",
        "'/* outer /* inner */ from */'",
        "\"/* /* from */\"",
        "[/* /* from */]",
        "$$/* outer /* from */ */$$",
        "$tag$/* outer /* from */ */$tag$",
    ] {
        let input = format!("select {atom} from t;");
        let result = format(input.as_bytes(), Language::Sql, "sql", None, &Settings::default()).unwrap();
        assert_eq!(result.bytes, format!("SELECT {atom}\nFROM t;\n").as_bytes(), "{atom}");
    }
    let result =
        format(b"select 1 -- /* /* 'from' */\nfrom t;", Language::Sql, "sql", None, &Settings::default()).unwrap();
    assert_eq!(result.bytes, b"SELECT 1 -- /* /* 'from' */\nFROM t;\n");
    // CSS has a proven nonnested comment grammar; this is not SQL admission.
    let result = format(b"a{/* outer /* inner */color:red}", Language::Css, "css", None, &Settings::default()).unwrap();
    assert_eq!(result.bytes, b"a{\n  /* outer /* inner */color:red\n}\n");
}

#[test]
fn php_prefix_without_a_tag_boundary_is_inline_output() {
    let input = b"<?phpx  ";
    let result = format(input, Language::Generic, "php", None, &Settings::default()).unwrap();
    assert_eq!(result.bytes, input);
    assert_eq!(result.replacement, None);
}

#[test]
fn php_requires_proven_opening_whitespace_and_retains_benign_code_tidy() {
    for tag in ["<?php", "<?PHP", "<?PhP"] {
        for tail in ["x  ", "7  ", "_data  ", "é  ", "", "\u{b}payload  ", "\u{c}payload  ", "x\n\tpayload  "] {
            let input = format!("{tag}{tail}");
            for settings in [
                Settings::default(),
                Settings { reindent: true, indent: Indent::Tabs { width: 4 }, ..Settings::default() },
            ] {
                let result = format(input.as_bytes(), Language::Generic, "php", None, &settings).unwrap();
                assert_eq!(result.bytes, input.as_bytes(), "{tag}, tail={tail:?}");
                assert_eq!(result.replacement, None);
            }
        }
        for boundary in [" ", "\t", "\n", "\r", "\r\n"] {
            let input = format!("{tag}{boundary}$x = 1;  ");
            let ending = if boundary.contains('\r') { boundary } else { "\n" };
            let result = format(input.as_bytes(), Language::Generic, "php", None, &Settings::default()).unwrap();
            assert_eq!(result.bytes, format!("{tag}{boundary}$x = 1;{ending}").as_bytes());
        }
    }
    for (input, expected) in [("<?php    \n$x = 1;  ", "<?php\n$x = 1;\n"), ("<?=1 + 2;  ", "<?=1 + 2;\n")] {
        let result = format(input.as_bytes(), Language::Generic, "php", None, &Settings::default()).unwrap();
        assert_eq!(result.bytes, expected.as_bytes());
    }
    for input in ["", "<p><?phpx  \ninline  ", "<?xml version='1.0'?>data  ", "<? echo 'inline';  "] {
        let result = format(input.as_bytes(), Language::Generic, "php", None, &Settings::default()).unwrap();
        assert_eq!(result.bytes, input.as_bytes());
        assert_eq!(result.replacement, None);
    }
    let input = b"<?phpx\ninline  ";
    let result = format(input, Language::Generic, "php", Some(7..input.len()), &Settings::default()).unwrap();
    assert_eq!(result.bytes, b"inline  ");
    assert_eq!(result.replacement, None);
}
