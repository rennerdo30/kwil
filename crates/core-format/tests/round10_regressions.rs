use kwil_core_format::{Indent, Language, Settings, format, format_selection};

const SQL_WITNESS: &str = "select 1 -- comment\r'\n, 'a\nfrom b\n ' /* ' */;";
const SHELL_WITNESS: &str = "cat <<EOF\nfirst\\\nEOF\nraw  \nEOF\n";

fn sql_literal_is_preserved_or_refused(input: &str, settings: &Settings) {
    match format(input.as_bytes(), Language::Sql, "sql", None, settings) {
        Ok(result) => assert!(
            String::from_utf8(result.bytes).unwrap().contains("'a\nfrom b\n '"),
            "SQL literal bytes changed after CR comment"
        ),
        Err(error) => {
            assert!(error.message.contains("CR line-comment boundary") && error.message.contains("dialect"), "{error}")
        }
    }
}

#[test]
fn sql_cr_comment_terminator_cannot_hide_literal_context() {
    sql_literal_is_preserved_or_refused(SQL_WITNESS, &Settings::default());
    for comment in
        ["-- comment\r'", "-- \r'", "--\r'", "-- comment\r\r'", "-- comment\r\n'", "-- comment\r", "-- comment\r\n"]
    {
        let input = format!("select 1 {comment}\n, 'a\nfrom b\n ' /* ' */;");
        let error = format(input.as_bytes(), Language::Sql, "sql", None, &Settings::default()).unwrap_err();
        assert!(
            error.message.contains("CR line-comment boundary") && error.message.contains("external formatter"),
            "{error}"
        );
    }
}

fn settings(mask: u8) -> Settings {
    Settings {
        reindent: mask & 1 != 0,
        trim_trailing_whitespace: mask & 2 != 0,
        normalize_line_endings: mask & 4 != 0,
        final_newline: mask & 8 != 0,
        indent: Indent::Spaces(4),
        ..Settings::default()
    }
}

#[test]
fn sql_comment_boundaries_preserve_common_lf_eof_and_quoted_cr() {
    sql_literal_is_preserved_or_refused(SQL_WITNESS, &Settings::default());
    for mask in 0..16 {
        let settings = settings(mask);
        sql_literal_is_preserved_or_refused(SQL_WITNESS, &settings);
        for (input, expected, eol) in [
            ("select 1 -- comment '/*\nfrom t;\n", "SELECT 1 -- comment '/*\nFROM t;", "\n"),
            ("select 1 -- comment", "SELECT 1 -- comment", "\n"),
            ("select 'a\rfrom b' from t;\n", "SELECT 'a\rfrom b'\rFROM t;", "\r"),
            ("select 1 /* -- comment\r ' */ from t;\n", "SELECT 1 /* -- comment\r ' */\rFROM t;", "\r"),
            ("select 1 -- comment\nfrom t;", "SELECT 1 -- comment\nFROM t;", "\n"),
        ] {
            let result = format(input.as_bytes(), Language::Sql, "sql", None, &settings).unwrap();
            let ending = if settings.final_newline { eol } else { "" };
            assert_eq!(result.bytes, format!("{expected}{ending}").as_bytes());
        }
        let error =
            format(SQL_WITNESS.as_bytes(), Language::Sql, "sql", Some(0..SQL_WITNESS.len()), &settings).unwrap_err();
        assert!(error.message.contains("not supported for a selection"));
    }
}

fn shell_body_exact(input: &str, settings: &Settings) {
    let result = format(input.as_bytes(), Language::Generic, "sh", None, settings).unwrap();
    assert!(
        String::from_utf8(result.bytes).unwrap().contains("raw  \n"),
        "shell raw body bytes changed after continued delimiter"
    );
}

#[test]
fn shell_continued_physical_delimiter_is_body_data() {
    shell_body_exact(SHELL_WITNESS, &Settings::default());
    for kind in ["sh", "bash", "shell", "ksh", "zsh"] {
        for slash_count in [1, 3, 5] {
            let slashes = "\\".repeat(slash_count);
            let input = format!("cat <<EOF\nfirst{slashes}\nEOF\nraw  \nEOF\n");
            let result = format(input.as_bytes(), Language::Generic, kind, None, &Settings::default()).unwrap();
            assert_eq!(result.bytes, input.as_bytes());
        }
    }
}

#[test]
fn shell_logical_delimiters_keep_parity_tabs_queue_cr_and_eof_contracts() {
    shell_body_exact(SHELL_WITNESS, &Settings::default());
    for mask in 0..16 {
        let settings = settings(mask);
        for input in [
            SHELL_WITNESS,
            "cat <<-EOF\nfirst\\\nEOF\n\traw  \n\tEOF\n",
            "cat <<EOF <<'B'\nfirst\\\nEOF\nraw  \nEOF\nsecond  \nB\n",
            "cat <<A <<-B\na\\\nA\nraw  \nA\n\tb\\\nB\n\traw  \n\tB\n",
            "cat <<EOF\nfirst\\\r\nraw  \nEOF\n",
            "cat <<EOF\nEOF\r\nraw  \nEOF\n",
        ] {
            let result = format(input.as_bytes(), Language::Generic, "sh", None, &settings).unwrap();
            assert_eq!(result.bytes, input.as_bytes(), "mask={mask}, input={input:?}");
            let at = input.find("raw  ").unwrap();
            let selected = format(input.as_bytes(), Language::Generic, "sh", Some(at..at + 6), &settings).unwrap();
            assert_eq!(selected.bytes, b"raw  \n");
            assert_eq!(selected.replacement, None);
            let contextless =
                format_selection(b"raw  \n", Language::Generic, "sh", at, input.len(), &settings).unwrap();
            assert_eq!(contextless.bytes, b"raw  \n");
            assert_eq!(contextless.replacement, None);
        }
        // Joined fragments may form the real delimiter, exposing ordinary code.
        // Even slash parity and quoted headers must not join physical lines.
        for (header, body) in [
            ("<<EOF", "body  \nE\\\nOF\n"),
            ("<<EOF", "body  \nE\\\nO\\\nF\n"),
            ("<<-EOF", "body  \n\\\n\tEOF\n"),
            ("<<'EOF'", "first\\\nEOF\n"),
            ("<<\"EOF\"", "first\\\nEOF\n"),
            ("<<-'EOF'", "first\\\n\tEOF\n"),
            ("<<EOF", "first\\\\\nEOF\n"),
            ("<<EOF", "first\\\\\\\\\nEOF\n"),
        ] {
            let input = format!("cat {header}\n{body}\techo ok  ");
            let result = format(input.as_bytes(), Language::Generic, "sh", None, &settings).unwrap();
            let indent = if settings.reindent { "    " } else { "\t" };
            let trailing = if settings.trim_trailing_whitespace { "" } else { "  " };
            let ending = if settings.final_newline { "\n" } else { "" };
            assert_eq!(result.bytes, format!("cat {header}\n{body}{indent}echo ok{trailing}{ending}").as_bytes());
        }
        // Unsupported headers and incomplete logical lines preserve EOF too.
        for input in [
            "cat <<EOF\nfirst\\\nEOF\nraw  ",
            "cat <<EOF\nE\\\nOF",
            "cat <<EOF\nfirst\\",
            "cat <<EOF\nfirst\\\n",
            "cat <<EOF\nEOF",
            "cat <<EOF\r\nraw  ",
            "cat <<`EOF`\nEOF\nraw  ",
            "cat <<E'OF'\nEOF\nraw  ",
            "cat <<\\EOF\nEOF\nraw  ",
            "cat <<EOF 'open\nEOF\nraw  ",
            "cat <<-EOF\nE\\\n\tOF\nraw  ",
        ] {
            let result = format(input.as_bytes(), Language::Generic, "sh", None, &settings).unwrap();
            assert_eq!(result.bytes, input.as_bytes(), "mask={mask}, input={input:?}");
            assert_eq!(result.replacement, None);
        }
    }
}
