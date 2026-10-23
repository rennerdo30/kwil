use kwil_core_format::{ExternalFormatter, Language, Settings, format};
use std::time::{Duration, Instant};

fn out(s: &str, language: Language, file_type: &str) -> String {
    String::from_utf8(format(s.as_bytes(), language, file_type, None, &Settings::default()).unwrap().bytes).unwrap()
}

#[test]
fn markdown_hard_break() {
    for key in ["markdown", "md", "mdown", "commonmark", "mdx", "rst"] {
        assert_eq!(out("hello  \nworld\n", Language::Generic, key), "hello  \nworld\n");
    }
}

#[test]
fn markdown_code_bytes() {
    let text = "text  \r\n```rust\n\t x  \r\n```\n\n    code  \r\n\tcode\n~~~\n  raw  \r\n~~~";
    let settings = Settings { reindent: true, ..Settings::default() };
    let result = format(text.as_bytes(), Language::Generic, "markdown", None, &settings).unwrap();
    assert_eq!(result.bytes, text.as_bytes());
}

#[test]
fn html_inline_labels() {
    let s = "<div><label>a</label><label>b</label></div>";
    assert_eq!(out(s, Language::Html, "html"), format!("{s}\n"));
}

#[test]
fn html_inline_images() {
    let s = "<div><img src='a'><img src='b'></div>";
    assert_eq!(out(s, Language::Html, "html"), format!("{s}\n"));
}

#[test]
fn html_unknown_mixed_and_existing_spaces() {
    for s in [
        "<div><widget><p>a</p><p>b</p></widget></div>",
        "<div>before<p>a</p>after</div>",
        "<div><span>a</span> \n <img src='b'> </div>",
        "<div><p>a</p><p>b</p></div>",
        "<div style='white-space:pre'>\n<p>a</p>   <p>b</p>\n</div>",
    ] {
        assert_eq!(out(s, Language::Html, "html"), format!("{s}\n"));
    }
    let a = out("<div>\n<p>a</p>\n<p>b</p>\n</div>", Language::Html, "html");
    assert_eq!(a, "<div>\n  <p>a</p>\n  <p>b</p>\n</div>\n");
    assert_eq!(out(&a, Language::Html, "html"), a);
}

#[test]
fn xml_mixed_content_and_inherited_space() {
    for s in ["<r><a/>\n\u{a0}<b/></r>", "<r xml:space='preserve'><a><b/> \r\n<c/></a></r>"] {
        let eol = if s.contains("\r\n") { "\r\n" } else { "\n" };
        assert_eq!(out(s, Language::Xml, "xml"), format!("{s}{eol}"));
    }
}

#[test]
fn sql_newline_literal_concat() {
    for s in ["select 'a'\n'b';", "select 'a' \r\n  'b';", "select 'a' /*keep*/\n'b';"] {
        let result = out(s, Language::Sql, "sql");
        assert!(result.contains(&s[7..]), "{result:?}");
    }
}

#[test]
fn sql_nested_comments() {
    let s = "select /* outer /* inner */ 'ignored */ 1;";
    assert_eq!(out(s, Language::Sql, "sql"), "SELECT /* outer /* inner */ 'ignored */ 1;\n");
}

#[test]
fn yaml_no_final_break_literal() {
    for s in ["a: |+\n  text", "a: |+\r\n  text  \n\n", "a: >-\n  text  ", "a: 1\r\nb: 2\n"] {
        assert_eq!(out(s, Language::Yaml, "yaml"), s);
    }
}

#[test]
fn toml_crlf() {
    let s = "a=1\r\n[[items]]\r\nx='''first\r\nsecond'''\r\n";
    let a = out(s, Language::Toml, "toml");
    assert!(!a.replace("\r\n", "").contains('\n'), "{a:?}");
    assert!(a.contains("'''first\r\nsecond'''"));
    assert_eq!(out(&a, Language::Toml, "toml"), a);
    // Structural CRLF must not rewrite deliberately LF-only literal content.
    let a = out("a=1\r\nx=\"\"\"first\nsecond\"\"\"\r\n", Language::Toml, "toml");
    assert_eq!(a, "a = 1\r\nx = \"\"\"first\nsecond\"\"\"\r\n");
    assert_eq!(
        out("x=\"\"\"first\nsecond\"\"\"\r\na=1\r\n", Language::Toml, "toml"),
        "x = \"\"\"first\nsecond\"\"\"\r\na = 1\r\n"
    );
}

#[test]
fn css_escaped_semicolon() {
    for value in [r"foo\;bar", r"foo\{bar\}", r"foo\3b bar", r"url(data:image/svg+xml;x;y)", r"url(a\)b;c)"] {
        let a = out(&format!("a{{--x:{value}}}"), Language::Css, "css");
        assert!(a.contains(value), "{a:?}");
        assert_eq!(out(&a, Language::Css, "css"), a);
    }
}

#[test]
fn protected_literals_idempotent() {
    for (language, s) in [
        (Language::Json, r#"{"n":-0,"n":999999999999999999999999999999999,"s":"\u0061\\\""}"#),
        (Language::Xml, "<r xml:space='preserve'>a<a/> b<![CDATA[ <raw> ]]><!--c-->&amp;</r>"),
        (Language::Html, "<div>before<b>x</b>after<pre> x\n y</pre></div>"),
        (Language::Toml, "s=\"\"\"first\n second  \n\"\"\"\n[[a]]\nx=1\n[[a]]\nx=2\n"),
        (Language::Css, "a{content:';}';background:url('a;b}');/* c;} */color:red}"),
        (Language::Sql, "select 'where', \"from\", $tag$select WHERE$tag$, $$from$$ from t;"),
    ] {
        let a = out(s, language, "");
        assert_eq!(out(&a, language, ""), a, "{language:?}");
    }
}

#[test]
fn external_timeout_terminates_grandchild() {
    let dir = tempfile::tempdir().unwrap();
    let receipt = dir.path().join("grandchild");
    let mut settings = Settings::default();
    settings.external.insert(
        "wrapper".into(),
        ExternalFormatter {
            command: env!("CARGO_BIN_EXE_format-test-helper").into(),
            args: vec!["wrapper".into(), receipt.to_str().unwrap().into()],
            timeout: Duration::from_secs(2),
        },
    );
    let error = format(b"x", Language::Generic, "wrapper", None, &settings).unwrap_err();
    assert!(error.message.contains("timed out"), "{error}");
    // The grandchild holds an OS lock before writing its PID. Releasing the lock
    // proves it stopped executing, without a PID-reuse race or Unix zombie race.
    let file = std::fs::OpenOptions::new().read(true).write(true).open(&receipt).unwrap();
    let pid = std::fs::read_to_string(receipt.with_extension("pid")).unwrap();
    eprintln!("owned grandchild PID {pid}; {error}");
    assert!(!pid.is_empty(), "grandchild must be ready before timeout");
    let start = Instant::now();
    let mut stopped_promptly = false;
    while start.elapsed() < Duration::from_millis(500) {
        if file.try_lock().is_ok() {
            stopped_promptly = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    // Broken-copy runs still clean up: fixture lifetime is capped at six seconds.
    if !stopped_promptly {
        let cleanup = Instant::now();
        while cleanup.elapsed() < Duration::from_secs(7) {
            if file.try_lock().is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    assert!(stopped_promptly, "owned grandchild {pid} survived the timeout");
}
