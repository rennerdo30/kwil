use kwil_core_format::{ExternalFormatter, Indent, Language, Settings, format, trim_trailing_whitespace};
use std::time::{Duration, Instant};

fn generic(input: &str, kind: &str) -> kwil_core_format::FormatResult {
    format(input.as_bytes(), Language::Generic, kind, None, &Settings::default()).unwrap()
}

#[test]
fn css_decoded_url_identifiers_keep_original_bytes() {
    for name in [r"\75rl", r"\000075rl", r"\75 rl", r"u\72l", r"ur\6c", r"\55RL", "URL", "uRl"] {
        let value = format!("{name}(data:text/plain,{{x;y}})");
        let input = format!("a{{background:{value};color:red}}");
        let result = format(input.as_bytes(), Language::Css, "css", None, &Settings::default()).unwrap();
        assert!(std::str::from_utf8(&result.bytes).unwrap().contains(&value), "{name}: {:?}", result.bytes);
        assert_eq!(format(&result.bytes, Language::Css, "css", None, &Settings::default()).unwrap().replacement, None);
    }
}

#[test]
fn python_triple_raw_and_f_strings() {
    for literal in
        ["\"\"\"a  \nb\"\"\"", "r'''a  \r\nb'''", "f\"\"\"a  \n{n}\"\"\"", "fr'''a  \n{n}'''", "f\"{f\"{n}\"}  \""]
    {
        let input = format!("s = {literal}\n");
        assert_eq!(generic(&input, "python").bytes, input.as_bytes(), "{literal}");
    }
}

#[test]
fn javascript_template_literals() {
    for literal in ["`a  \nb`", "`a  \r\n${n}`", "`outer ${`inner  \n${n}`} tail`"] {
        let input = format!("const s = {literal};\n");
        for kind in ["javascript", "typescript", "js", "ts"] {
            assert_eq!(generic(&input, kind).bytes, input.as_bytes(), "{kind}: {literal}");
        }
    }
}

#[test]
fn common_language_multiline_literals() {
    for (kind, literal) in [
        ("rust", "r###\"a  \r\nb\"###"),
        ("go", "`a  \r\nb`"),
        ("cpp", "R\"tag(a  \r\nb)tag\""),
        ("csharp", "\"\"\"a  \r\nb\"\"\""),
        ("java", "\"\"\"\na  \r\nb\n\"\"\""),
        ("lua", "[==[a  \r\nb]==]"),
        ("kotlin", "\"\"\"a  \r\nb\"\"\""),
        ("swift", "#\"\"\"\na  \r\nb\n\"\"\"#"),
        ("ruby", "\"a  \r\nb\""),
        ("php", "\"a  \r\nb\""),
    ] {
        let input = format!("s = {literal};\n");
        assert_eq!(generic(&input, kind).bytes, input.as_bytes(), "{kind}");
    }
}

#[test]
fn shell_ruby_php_heredocs() {
    for (kind, input) in [
        ("sh", "cat <<'EOF'\n  a  \r\n\tb  \nEOF\n"),
        ("bash", "cat <<-EOF\n\ta  \r\n\tEOF\n"),
        ("shell", "cat <<A <<'B'\na  \nA\nb  \nB\n"),
        ("ruby", "s = <<~EOF\n  a  \r\n  EOF\n"),
        ("php", "<?php\n$s = <<<'EOF'\n  a  \r\nEOF;\n"),
        ("php", "<?php\n$s = <<<EOF\n  a  \r\nEOF;\n"),
    ] {
        assert_eq!(generic(input, kind).bytes, input.as_bytes(), "{kind}");
    }
}

#[test]
fn unrecognized_types_never_tidy_or_add_newline() {
    for kind in ["unknown-extension", "", "csv", "data", "markdown", "mdx", "makefile"] {
        let input = "\tvalue  \r\nlast  ";
        let result = generic(input, kind);
        assert_eq!(result.bytes, input.as_bytes(), "{kind}");
        assert_eq!(result.replacement, None);
        assert_eq!(
            result.notices,
            [
                "No safe formatter for this file type. Configure an external formatter, or use 'Trim trailing whitespace' explicitly."
            ]
        );
    }
}

#[test]
fn explicit_trim_is_separate_warned_and_preserves_endings() {
    let input = b"s = \"\"\"a  \r\nb\"\"\"  \nlast  ";
    let result = trim_trailing_whitespace(input, &Settings::default()).unwrap();
    assert_eq!(result.bytes, b"s = \"\"\"a\r\nb\"\"\"\nlast");
    assert_eq!(result.replacement, Some(0..input.len()));
    assert!(result.notices[0].contains("may change literals"));
    assert_eq!(trim_trailing_whitespace(&result.bytes, &Settings::default()).unwrap().replacement, None);
}

#[test]
fn only_proven_code_whitespace_changes() {
    let input = "\tlet s = r#\"a  \n\t b  \r\nend\"#;  \n\tlet n = 1;  ";
    let settings = Settings { reindent: true, indent: Indent::Spaces(4), ..Settings::default() };
    let result = format(input.as_bytes(), Language::Generic, "rust", None, &settings).unwrap();
    assert_eq!(result.bytes, b"    let s = r#\"a  \n\t b  \r\nend\"#;\n    let n = 1;\n");
    assert_eq!(format(&result.bytes, Language::Generic, "rust", None, &settings).unwrap().replacement, None);
}

#[test]
fn uncertain_literals_include_eof_and_contextless_selections() {
    for (kind, input) in [
        ("py", "s = \"\"\"a  \nb  "),
        ("js", "const r = /a['\"]  \nraw  "),
        ("js", "const x = <span>  \nraw  "),
        ("sh", "cat <<EOF\na  \nb  "),
        ("cs", "$\"a {f(\"b\")}  \nraw  "),
        ("rb", "%Q{a  \nb  }"),
        ("php", "<p>  \nraw  "),
        ("php", "<?xml version='1.0'?>  \nraw  "),
        ("sh", "s=\"$(printf \"a  \nb\")\"\n"),
        ("cpp", "int n=1'000; const char*s=\"a'  \nb\";\n"),
        ("c", "// trigraph ??/\n\"not a string  \nraw  "),
    ] {
        assert_eq!(generic(input, kind).bytes, input.as_bytes(), "{kind}");
    }
    let input = "s = \"\"\"\na  \nb\n\"\"\"\n";
    let inside = input.find("a  ").unwrap();
    let result =
        format(input.as_bytes(), Language::Generic, "py", Some(inside..inside + 3), &Settings::default()).unwrap();
    assert_eq!(result.bytes, b"a  ");
    assert_eq!(result.replacement, None);
    let result =
        kwil_core_format::format_selection(b"a  ", Language::Generic, "py", 100, 5, &Settings::default()).unwrap();
    assert_eq!(result.bytes, b"a  ");
    assert_eq!(result.replacement, None);
    assert!(result.notices[0].contains("context"));
}

#[test]
fn successful_external_formatter_reaps_and_stops_descendant() {
    let dir = tempfile::tempdir().unwrap();
    let receipt = dir.path().join("success-grandchild");
    let mut settings = Settings::default();
    let command = env!("CARGO_BIN_EXE_format-test-helper").to_owned();
    settings.external.insert(
        "custom".into(),
        ExternalFormatter { command: command.clone(), args: vec!["echo".into()], timeout: Duration::from_secs(3) },
    );
    // On XNU this leaves just the unreaped leader, which must be accepted as a
    // zombie-only group. The same test then asserts live descendant cleanup.
    assert_eq!(format(b"success\n", Language::Generic, "custom", None, &settings).unwrap().bytes, b"success\n");
    settings.external.insert(
        "custom".into(),
        ExternalFormatter {
            command,
            args: vec!["wrapper-success".into(), receipt.to_str().unwrap().into()],
            timeout: Duration::from_secs(3),
        },
    );
    let result = format(b"x", Language::Generic, "custom", None, &settings);
    // Always wait for the fixture's bounded natural lifetime even if containment
    // is deliberately broken for a fail-first run. Never target a reused PID.
    let file = std::fs::OpenOptions::new().read(true).write(true).open(&receipt).unwrap();
    let pid = std::fs::read_to_string(receipt.with_extension("pid")).unwrap();
    eprintln!("owned success grandchild PID {pid}");
    let start = Instant::now();
    let mut promptly = false;
    while start.elapsed() < Duration::from_secs(7) {
        if file.try_lock().is_ok() {
            promptly = start.elapsed() < Duration::from_millis(500);
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(promptly, "owned grandchild {pid} survived successful formatter exit");
    assert_eq!(result.unwrap().bytes, b"formatted\n");
}
