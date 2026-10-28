use kwil_core_syntax::{
    Class, Config, Engine, Error, Grammar, GrammarProvider, Language, Mode, Span, TextSource, detect,
    detection::{DetectionOrigin, Detector, LanguageMapping},
    logs::{Level, MAX_PREFIX_BYTES, Metadata, Timestamp, metadata, parse_timestamp},
};
use std::{
    borrow::Cow,
    io,
    ops::Range,
    path::Path,
    sync::{Arc, Mutex},
};

fn highlight(text: &str) -> Vec<Span> {
    Engine::new(Config { margin_bytes: 0, ..Config::default() })
        .unwrap()
        .highlight(&text.as_bytes().to_vec(), Language::Log, 1, None, None, 0..text.len(), &|| false)
        .unwrap()
        .spans
}
fn span(range: Range<usize>, class: Class) -> Span {
    Span { range, class }
}
fn detected(path: &str, bytes: &[u8]) -> kwil_core_syntax::detection::Detection {
    Detector::new(&[]).unwrap().detect(Path::new(path), None, &bytes.to_vec(), &|| false).unwrap()
}

#[test]
fn level_contract_masks_labels_and_aliases() {
    assert_eq!(Level::ALL.len(), 6);
    assert_eq!(Level::ALL.iter().fold(0u8, |mask, level| mask | level.bit()), 0b11_1111);
    for (index, &level) in Level::ALL.iter().enumerate() {
        assert_eq!(level.bit(), 1 << index);
        assert_eq!(metadata(format!("[{}] message", level.label()).as_bytes()).level, Some(level));
    }
    assert_eq!(metadata(b"WARNING: low disk").level, Some(Level::Warn));
    assert_eq!(metadata(b"critical: failed").level, Some(Level::Fatal));
}

#[test]
fn timestamp_epoch_precision_and_timezone_equivalence() {
    assert_eq!(parse_timestamp("1970-01-01T00:00:00Z"), Some(Timestamp(0)));
    assert_eq!(parse_timestamp("1969-12-31T23:59:59.999999999Z"), Some(Timestamp(-1)));
    assert_eq!(parse_timestamp("2000-01-01T00:00:00Z"), Some(Timestamp(946_684_800_000_000_000)));
    let utc = parse_timestamp("2024-02-29T15:34:56.123456789Z").unwrap();
    for text in
        ["2024-03-01T00:34:56.123456789+09:00", "2024-02-29T10:04:56.123456789-0530", "2024-02-29 15:34:56,123456789"]
    {
        assert_eq!(parse_timestamp(text), Some(utc), "{text}");
    }
    assert!(Timestamp(-1) < Timestamp(0));
    assert_eq!(parse_timestamp("1970-01-01 01:00:00,000"), Some(Timestamp(3_600_000_000_000)));
}

#[test]
fn timestamp_numeric_units_are_exact_and_bounded() {
    let nanos = Timestamp(1_700_000_000_123_456_789);
    assert_eq!(parse_timestamp("1700000000.123456789"), Some(nanos));
    assert_eq!(parse_timestamp("1700000000123456789"), Some(nanos));
    assert_eq!(parse_timestamp("1700000000123"), Some(Timestamp(1_700_000_000_123_000_000)));
    assert_eq!(parse_timestamp("1700000000123456"), Some(Timestamp(1_700_000_000_123_456_000)));
    assert_eq!(parse_timestamp("-1700000000.123456789"), Some(Timestamp(-nanos.0)));
    assert_eq!(parse_timestamp("+1700000000"), Some(Timestamp(1_700_000_000_000_000_000)));
    for text in [
        "42",
        "170000000",
        "17000000000",
        "1700000000.",
        "1700000000.1234567890",
        "1700000000e1",
        " 1700000000",
        "1700000000 ",
    ] {
        assert_eq!(parse_timestamp(text), None, "{text}");
    }
}

#[test]
fn timestamp_rejects_malformed_dates_offsets_and_precision() {
    for text in [
        "2023-02-29T00:00:00Z",
        "1900-02-29T00:00:00Z",
        "2024-04-31T00:00:00Z",
        "2024-00-01T00:00:00Z",
        "2024-13-01T00:00:00Z",
        "2024-01-00T00:00:00Z",
        "2024-01-01T24:00:00Z",
        "2024-01-01T00:60:00Z",
        "2024-01-01T00:00:60Z",
        "2024-01-01T00:00:00+24:00",
        "2024-01-01T00:00:00+09:60",
        "2024-01-01T00:00:00+09",
        "2024-01-01T00:00:00",
        "2024-01-01T00:00:00.Z",
        "2024-01-01T00:00:00.1234567890Z",
        "12:34:56",
        "2024-01-01",
        "2024-01-01T00:00:00Zjunk",
        "2024-01-01T00:00:00Z\n",
    ] {
        assert_eq!(parse_timestamp(text), None, "{text}");
    }
    assert!(parse_timestamp("2000-02-29T00:00:00Z").is_some());
    assert!(parse_timestamp("0000-02-29T00:00:00Z").is_some());
    assert!(parse_timestamp(&"1".repeat(41)).is_none());
}

#[test]
fn metadata_headers_not_message_words() {
    let stamp = parse_timestamp("2026-10-24T03:34:56.123Z");
    assert_eq!(
        metadata(b"[2026-10-24T12:34:56.123+09:00] [main] ERROR app.Server - failed"),
        Metadata { level: Some(Level::Error), timestamp: stamp }
    );
    assert_eq!(
        metadata(b"2026-10-24 03:34:56,123 WARN [main] app.Server - failed"),
        Metadata { level: Some(Level::Warn), timestamp: stamp }
    );
    assert_eq!(metadata(b"1700000000.125 INFO ready").timestamp, parse_timestamp("1700000000.125"));
    for line in [
        b"the ERROR is in the message".as_slice(),
        b"the info is useful",
        b"ERRORcode failed",
        b"[ERRORish] failed",
        b"{\"message\":\"ERROR\"}",
    ] {
        assert_eq!(metadata(line), Metadata::default(), "{line:?}");
    }
    assert_eq!(metadata(b"2026-10-24 03:34:56 message mentions ERROR").level, None);
    assert_eq!(metadata(b"12:34:56,123 ERROR failed"), Metadata { level: Some(Level::Error), timestamp: None });
    assert_eq!(metadata(b"unknown\nERROR second record"), Metadata::default());
}

#[test]
fn metadata_json_ignores_strings_nested_keys_and_escaped_keys() {
    let text = br#"{"message":"fake \"level\":\"FATAL\"","payload":{"severity":"ERROR","timestamp":"2000-01-01T00:00:00Z"},"level":"info","ts":1700000000.125}"#;
    assert_eq!(metadata(text), Metadata { level: Some(Level::Info), timestamp: parse_timestamp("1700000000.125") });
    for text in [
        br#"{"message":"level ERROR timestamp 2026-10-24T00:00:00Z"}"#.as_slice(),
        br#"{"payload":{"level":"ERROR"},"level":"unknown"}"#,
        br#"{"lev\u0065l":"ERROR","message":"severity"}"#,
        br#"{"message":"{\"severity\":\"ERROR\"}"}"#,
        br#"{"level":"ERR\u004fR"}"#,
    ] {
        assert_eq!(metadata(text), Metadata::default(), "{text:?}");
    }
    assert_eq!(
        metadata(br#"{"severity":"WARNING","@timestamp":"1970-01-01T09:00:00+09:00"}"#),
        Metadata { level: Some(Level::Warn), timestamp: Some(Timestamp(0)) }
    );
    assert_eq!(metadata(br#"{"level":"info","level":"ERROR"}"#).level, Some(Level::Info), "first valid field wins");
    assert_eq!(metadata(br#"{"timestamp":"2023-02-29T00:00:00Z","level":"error"}"#).timestamp, None);
}

#[test]
fn metadata_prefix_cap_does_not_turn_partial_tokens_into_levels() {
    let mut record = vec![b' '; MAX_PREFIX_BYTES - 5];
    record.extend_from_slice(b"ERRORcode message");
    assert_eq!(metadata(&record), Metadata::default());
    let mut record = vec![b' '; MAX_PREFIX_BYTES];
    record.extend_from_slice(b"2026-10-24T00:00:00Z ERROR");
    assert_eq!(metadata(&record), Metadata::default());
    let mut json = br#"{"message":""#.to_vec();
    json.resize(MAX_PREFIX_BYTES + 16, b'x');
    json.extend_from_slice(br#"","severity":"ERROR"}"#);
    assert_eq!(metadata(&json), Metadata::default());
}

#[test]
fn exact_log4j_header_spans() {
    let text = "2026-10-24 12:34:56,123 INFO [main] app.Server - ready";
    assert_eq!(
        highlight(text),
        vec![
            span(0..23, Class::Number),
            span(24..28, Class::Keyword),
            span(30..34, Class::Variable),
            span(36..46, Class::Type)
        ]
    );
    assert_eq!(highlight("[WARN] ready"), vec![span(1..5, Class::Constant)]);
    assert_eq!(highlight("ERROR failed"), vec![span(0..5, Class::Error)]);
    assert!(highlight("message ERROR WARNING INFO FATAL").is_empty());
}

#[test]
fn exact_nested_json_payload_spans() {
    let text = "INFO {\"a\":[1,{\"ok\":true}],\"s\":\"ERROR\"}";
    assert_eq!(
        highlight(text),
        vec![
            span(0..4, Class::Keyword),
            span(5..6, Class::Punctuation),
            span(6..9, Class::Property),
            span(9..10, Class::Punctuation),
            span(10..11, Class::Punctuation),
            span(11..12, Class::Number),
            span(12..13, Class::Punctuation),
            span(13..14, Class::Punctuation),
            span(14..18, Class::Property),
            span(18..19, Class::Punctuation),
            span(19..23, Class::Constant),
            span(23..24, Class::Punctuation),
            span(24..25, Class::Punctuation),
            span(25..26, Class::Punctuation),
            span(26..29, Class::Property),
            span(29..30, Class::Punctuation),
            span(30..37, Class::String),
            span(37..38, Class::Punctuation),
        ]
    );
}

#[test]
fn single_json_array_values_are_not_thread_names() {
    for literal in ["true", "false", "null", "1"] {
        let text = format!("INFO [{literal}]");
        let start = 6;
        assert_eq!(
            highlight(&text),
            vec![
                span(0..4, Class::Keyword),
                span(5..6, Class::Punctuation),
                span(start..start + literal.len(), if literal == "1" { Class::Number } else { Class::Constant }),
                span(text.len() - 1..text.len(), Class::Punctuation)
            ]
        );
    }
}

#[test]
fn embedded_json_after_message_brackets_is_still_highlighted() {
    let text = "DEBUG processing [request id] result {\"n\":-1.5e2,\"ok\":false}";
    let spans = highlight(text);
    let start = text.find("\"n\"").unwrap();
    assert!(spans.contains(&span(start..start + 3, Class::Property)));
    let start = text.find("-1.5e2").unwrap();
    assert!(spans.contains(&span(start..start + 6, Class::Number)));
    assert_eq!(metadata(text.as_bytes()).level, Some(Level::Debug));
    let generic = Engine::new(Config::default())
        .unwrap()
        .highlight(&b"message ERROR INFO FATAL".to_vec(), Language::Generic, 1, None, None, 0..24, &|| false)
        .unwrap();
    assert!(generic.spans.is_empty());
}

#[test]
fn crlf_and_cr_headers_resume_after_window_fragment() {
    let text = b"partial ERROR\r\nWARN ready\rERROR failed".to_vec();
    let mut engine = Engine::new(Config { margin_bytes: 0, ..Config::default() }).unwrap();
    let h = engine.highlight(&text, Language::Log, 1, None, None, 8..text.len(), &|| false).unwrap();
    assert_eq!(h.spans, vec![span(15..19, Class::Constant), span(26..31, Class::Error)]);
}

#[test]
fn json_semantic_values_and_prose_strings_have_exact_spans() {
    let text = "{\"level\":\"error\",\"logger\":\"app\",\"thread\":\"main\",\"timestamp\":\"1970-01-01T00:00:00Z\"}";
    let spans = highlight(text);
    for (token, class) in [
        ("\"error\"", Class::Error),
        ("\"app\"", Class::Type),
        ("\"main\"", Class::Variable),
        ("\"1970-01-01T00:00:00Z\"", Class::Number),
    ] {
        let start = text.find(token).unwrap();
        assert!(spans.contains(&span(start..start + token.len(), class)), "{token}: {spans:?}");
    }
    let text = r#"INFO "fake {\"level\":\"ERROR\"}" done"#;
    assert_eq!(highlight(text), vec![span(0..4, Class::Keyword), span(5..text.len() - 5, Class::String)]);
}

#[test]
fn exceptions_stack_traces_and_invalid_json_are_bounded() {
    for text in [
        "\tat app.Server.run(Server.java:42)",
        "Caused by: java.io.IOException: broken",
        "Traceback (most recent call last):",
    ] {
        let start = text.find(|c: char| !c.is_ascii_whitespace()).unwrap();
        assert_eq!(highlight(text), vec![span(start..text.len(), Class::Error)]);
    }
    assert_eq!(highlight("java.lang.IllegalStateException: failed"), vec![span(0..31, Class::Error)]);
    assert_eq!(
        highlight("ERROR java.lang.IllegalStateException: failed"),
        vec![span(0..5, Class::Error), span(6..37, Class::Error)]
    );
    for text in ["INFO {\"x\":", "DEBUG {\"x\":\"unterminated", "ERROR {]", "INFO [1e+,]", "INFO {\"x\":01}"] {
        let spans = highlight(text);
        assert!(spans.iter().all(|s| s.range.start < s.range.end && s.range.end <= text.len()));
        assert!(spans.windows(2).all(|s| s[0].range.end <= s[1].range.start));
    }
}

#[test]
fn log_detection_picker_mapping_and_grammar_identity() {
    assert_eq!(Language::ALL.len(), 32);
    assert!(!Language::ALL.contains(&Language::Log));
    assert_eq!(Language::CATALOGUE.len(), 34);
    assert_eq!(Language::from_id("LOG"), Some(Language::Log));
    assert!(Language::Log.bundled().is_none());
    for path in ["server.LOG", "server.log.1", "server.log.2026-10-24", "log4j", "log4j.2", "server.log4j"] {
        let d = detected(path, b"arbitrary message");
        assert_eq!((d.language, d.log_levels, d.origin), (Language::Log, true, DetectionOrigin::Filename));
        assert_eq!(detect(path, b""), Some(Language::Log));
    }
    for text in [
        b"2026-10-24T00:00:00Z INFO ready".as_slice(),
        br#"{"timestamp":"2026-10-24T00:00:00Z","severity":"info","message":"ok"}"#,
    ] {
        assert_eq!(detected("unknown", text).language, Language::Log);
        assert_eq!(detect("unknown", text), Some(Language::Log));
    }
    for text in ["random ERROR text", "2026-10-24T00:00:00Z ready", "2026-02-30T00:00:00Z INFO", "INFOish ready"] {
        assert!(!detected("unknown", text.as_bytes()).log_levels, "{text}");
    }
    assert_eq!(detected("main.rs", b"2026-10-24T00:00:00Z INFO ready").language, Language::Rust);
    assert_eq!(detected("log4j.xml", b"<configuration/>").language, Language::Xml);
    assert!(!detected("server.log.gz", b"not decoded").log_levels);
    let detector = Detector::new(&[LanguageMapping { pattern: "*.log".into(), language: Language::Generic }]).unwrap();
    assert!(!detector.detect(Path::new("server.log"), None, &b"ERROR text".to_vec(), &|| false).unwrap().log_levels);
    assert!(detector.detect(Path::new("main.rs"), Some(Language::Log), &Vec::new(), &|| false).unwrap().log_levels);
    let mut late = vec![b' '; MAX_PREFIX_BYTES];
    late.extend_from_slice(b"2026-10-24T00:00:00Z ERROR");
    assert!(!detected("unknown", &late).log_levels);
}

#[test]
fn engine_log_never_requests_a_grammar_and_clips_spans() {
    struct NoGrammar;
    impl GrammarProvider for NoGrammar {
        fn grammar(&self, _: Language) -> Result<Option<Grammar>, Error> {
            panic!("log requested grammar")
        }
    }
    let mut engine = Engine::with_provider(Config::default(), Arc::new(NoGrammar)).unwrap();
    let text = b"ERROR failed".to_vec();
    let h = engine.highlight(&text, Language::Log, 1, None, None, 2..4, &|| false).unwrap();
    assert_eq!(h.mode, Mode::Lexical);
    assert!(!h.incremental);
    assert_eq!(h.spans, vec![span(2..4, Class::Error)]);
    assert!(matches!(
        engine.highlight(&text, Language::Log, 2, None, None, 0..text.len(), &|| true),
        Err(Error::Cancelled)
    ));
}

#[test]
fn log_huge_source_reads_only_admitted_window_and_no_fake_fragment_header() {
    struct Huge(Mutex<Vec<Range<usize>>>);
    impl TextSource for Huge {
        fn len(&self) -> usize {
            1_000_000_000
        }
        fn chunk(&self, offset: usize, max: usize) -> io::Result<Cow<'_, [u8]>> {
            self.0.lock().unwrap().push(offset..offset + max);
            let mut bytes = vec![b'x'; max.min(3)];
            // A fake level at the start of a cropped long line is not a header.
            if offset == 900_000_000 {
                bytes.copy_from_slice(b"ERR");
            }
            Ok(Cow::Owned(bytes))
        }
    }
    let source = Huge(Mutex::new(Vec::new()));
    let result = Engine::new(Config { margin_bytes: 0, ..Config::default() })
        .unwrap()
        .highlight(&source, Language::Log, 1, None, None, 900_000_000..900_000_032, &|| false)
        .unwrap();
    assert!(result.spans.is_empty());
    assert!(source.0.lock().unwrap().iter().all(|r| r.start >= 900_000_000 && r.end <= 900_000_032));
}

#[test]
fn giant_line_span_depth_limits_and_midscan_cancellation() {
    let mut text = "INFO {\"x\":".to_owned();
    text.push_str(&"[".repeat(40));
    text.push('1');
    assert!(highlight(&text).len() <= 36);
    let mut text = b"INFO [".to_vec();
    for _ in 0..9000 {
        text.extend_from_slice(b"0,");
    }
    text.extend_from_slice(b"0]");
    let mut engine = Engine::new(Config { margin_bytes: 0, ..Config::default() }).unwrap();
    assert!(matches!(
        engine.highlight(&text, Language::Log, 1, None, None, 0..text.len(), &|| false),
        Err(Error::QueryLimit)
    ));
    let polls = std::cell::Cell::new(0);
    assert!(matches!(
        engine.highlight(&text, Language::Log, 1, None, None, 0..text.len(), &|| {
            polls.set(polls.get() + 1);
            polls.get() > 12
        }),
        Err(Error::Cancelled)
    ));
}
