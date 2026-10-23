use kwil_core_format::{Indent, Language, Settings, format, format_selection};

fn sql_refuses(input: &str) {
    let error = format(input.as_bytes(), Language::Sql, "sql", None, &Settings::default()).unwrap_err();
    assert!(error.message.contains("dialect") && error.message.contains("external formatter"), "{error}");
}

fn sql_literal_is_preserved_or_refused(input: &str, literal: &str) {
    match format(input.as_bytes(), Language::Sql, "sql", None, &Settings::default()) {
        Err(error) => {
            assert!(error.message.contains("dialect") && error.message.contains("external formatter"), "{error}")
        }
        Ok(result) => {
            assert!(String::from_utf8(result.bytes).unwrap().contains(literal), "literal bytes changed: {input:?}")
        }
    }
}

#[test]
fn sql_sqlite_dollar_parameters_cannot_hide_literal_openers() {
    sql_literal_is_preserved_or_refused("select $tag$, 'a$tag$\nfrom b\n ' /* ' */;", "'a$tag$\nfrom b\n '");
    for tag in ["$$", "$tag$", "$a1_$", "$from", "$1", "$", "x$from", "1$tag$"] {
        for ending in ["\n", "\r\n", "\r"] {
            sql_refuses(&format!("select {tag}, 'a{tag}{ending}from b{ending} ' /* ' */;"));
        }
    }
}

#[test]
fn sql_boundary_proof_retains_common_quotes_comments_and_complete_atoms() {
    sql_literal_is_preserved_or_refused("select ARRAY['a]\nfrom b\n '] /* ' */;", "'a]\nfrom b\n '");
    for input in [
        "select $$from$$ from t;",
        "select $tag$from$tag$ from t;",
        "select $from from t;",
        "select foo$from from t;",
        "select 1$from from t;",
        "select $tag$ from t;",
        "select :from from t;",
        "select @from from t;",
        "select ?from from t;",
        "select q'[from]' from t;",
        "select /* outer /* inner */ 1;",
        "select 1--2;",
        "select [from] from t;",
    ] {
        sql_refuses(input);
    }
    for atom in [
        "'$tag$ from $$ /* --2 */'",
        "'it''s $from'",
        "\"$tag$ from $$\"",
        "`$tag$ from $$`",
        "'[from] ARRAY[''x'']'",
        "/* $tag$ 'from' $$ */ 1",
        "fromage",
        "_from",
        "from1",
        "1.25",
    ] {
        let input = format!("select {atom} from t;");
        let result = format(input.as_bytes(), Language::Sql, "sql", None, &Settings::default()).unwrap();
        assert_eq!(result.bytes, format!("SELECT {atom}\nFROM t;\n").as_bytes(), "{atom}");
        let second = format(&result.bytes, Language::Sql, "sql", None, &Settings::default()).unwrap();
        assert_eq!(second.bytes, result.bytes);
    }
    let result =
        format(b"select 1 -- $tag$ $$ 'from'\nfrom t;", Language::Sql, "sql", None, &Settings::default()).unwrap();
    assert_eq!(result.bytes, b"SELECT 1 -- $tag$ $$ 'from'\nFROM t;\n");
}

fn php_refuses(input: &str, selection: Option<std::ops::Range<usize>>, settings: &Settings) {
    let error = format(input.as_bytes(), Language::Generic, "php", selection, settings).unwrap_err();
    assert!(
        error.message.contains("heredoc interpolation boundary") && error.message.contains("external formatter"),
        "{error}"
    );
}

fn php_raw_body_is_preserved_or_refused(input: &str, selection: Option<std::ops::Range<usize>>, settings: &Settings) {
    match format(input.as_bytes(), Language::Generic, "php", selection, settings) {
        Err(error) => assert!(
            error.message.contains("heredoc interpolation boundary") && error.message.contains("external formatter"),
            "{error}"
        ),
        Ok(result) => assert!(
            String::from_utf8(result.bytes).unwrap().contains("raw  "),
            "significant raw body whitespace changed"
        ),
    }
}

#[test]
fn php_heredoc_terminators_require_outer_literal_context() {
    let input = "<?php\n$x = <<<END\n{$a[\"x\nEND\ny\"]}\n\"\nraw  \nEND;\n";
    for settings in [
        Settings::default(),
        Settings {
            reindent: true,
            trim_trailing_whitespace: true,
            normalize_line_endings: true,
            final_newline: true,
            indent: Indent::Tabs { width: 4 },
            ..Settings::default()
        },
        Settings { reindent: true, indent: Indent::Spaces(4), ..Settings::default() },
    ] {
        php_raw_body_is_preserved_or_refused(input, None, &settings);
        let at = input.find("raw  ").unwrap();
        php_raw_body_is_preserved_or_refused(input, Some(at..at + 5), &settings);
        php_refuses(input, Some(0..input.len()), &settings);
        // Contextless selection already has an explicit preservation contract.
        let result = format_selection(b"raw  ", Language::Generic, "php", at, input.len(), &settings).unwrap();
        assert_eq!(result.bytes, b"raw  ");
        assert_eq!(result.replacement, None);
    }
}

#[test]
fn php_heredoc_interpolation_uncertainty_is_bounded_and_nowdoc_stays_literal() {
    php_raw_body_is_preserved_or_refused(
        "<?php\n$x = <<<END\n{$a[\"x\nEND\ny\"]}\n\"\nraw  \nEND;\n",
        None,
        &Settings::default(),
    );
    let bodies = [
        "{$a[\"x\nEND\ny\"]}\n\"\n\traw  ",
        "${a[\"x\nEND\ny\"]}\nraw  ",
        "{$a[/* nested } ' */ \"x\nEND\ny\"]}\nraw  ",
        "{$a[\"{$b['x']}\nEND\ny\"]}\nraw  ",
        "\\{$a[\"x\nEND\ny\"]}\nraw  ",
        "\\\\$a\nraw  ",
        "$a\nraw  ",
        "$\nraw  ",
        "{$a[\"open  ",
    ];
    for opener in ["<<<END", "<<<\"END\""] {
        for body in bodies {
            for tail in ["\nEND;\n", "", "\nEND\n\traw  "] {
                let input = format!("<?php\n$x = {opener}\n{body}{tail}");
                php_refuses(&input, None, &Settings::default());
            }
        }
    }
    // All four tidy switches vary independently; literal bytes stay exact while
    // admitted ordinary code must still respond to those switches.
    for mask in 0..16 {
        let settings = Settings {
            reindent: mask & 1 != 0,
            trim_trailing_whitespace: mask & 2 != 0,
            normalize_line_endings: mask & 4 != 0,
            final_newline: mask & 8 != 0,
            indent: Indent::Spaces(4),
            ..Settings::default()
        };
        php_refuses("<?php\n$x = <<<END\n{$a[\"x\nEND\ny\"]}\nraw  \nEND;", None, &settings);
        for (opener, body) in [
            ("<<<'END'", "{$a[\"x\"]} /* quotes */ $a \\\n\traw  "),
            ("<<<'END'", "$a /* literal */  \r\n\traw  \r\nlast  "),
            ("<<<END", "\\$a \\\\\\$b\n\traw  "),
            ("<<<\"END\"", "quotes ' \" /* braces {} */\n\traw  "),
        ] {
            let input = format!("<?php\n$x = {opener}\n{body}\nEND;\n\t$ok = 1;  ");
            let result = format(input.as_bytes(), Language::Generic, "php", None, &settings).unwrap();
            let indent = if settings.reindent { "    " } else { "\t" };
            let trailing = if settings.trim_trailing_whitespace { "" } else { "  " };
            let ending = if settings.final_newline { "\n" } else { "" };
            assert_eq!(
                result.bytes,
                format!("<?php\n$x = {opener}\n{body}\nEND;\n{indent}$ok = 1;{trailing}{ending}").as_bytes()
            );
            let at = input.find("\traw  ").unwrap();
            let selected = format(input.as_bytes(), Language::Generic, "php", Some(at..at + 6), &settings).unwrap();
            assert_eq!(selected.bytes, b"\traw  ");
            assert_eq!(selected.replacement, None);
        }
    }
    // Incomplete headers and EOF keep the existing conservative suffix guard.
    for suffix in
        ["<<<\"END\n\traw  ", "<<<END + 'x\nEND\n\traw  ", "<<<END\nplain  ", "<<<'END'\n$a  ", "<<<END\n\\$a  "]
    {
        let input = format!("<?php\n$x = {suffix}");
        let result = format(input.as_bytes(), Language::Generic, "php", None, &Settings::default()).unwrap();
        assert_eq!(result.bytes, input.as_bytes());
        assert_eq!(result.replacement, None);
    }
}
