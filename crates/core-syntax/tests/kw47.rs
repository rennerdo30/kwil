use kwil_core_syntax::detection::{DetectionOrigin as Origin, Detector, LanguageMapping};
use kwil_core_syntax::{Class, Config, Engine, Error, Grammar, GrammarProvider, Language, Mode, TextSource};
use std::{
    borrow::Cow,
    io,
    path::Path,
    sync::{Arc, Mutex},
};

fn detected(path: &str, line: &str) -> kwil_core_syntax::detection::Detection {
    Detector::new(&[]).unwrap().detect(Path::new(path), None, &line.as_bytes().to_vec(), &|| false).unwrap()
}

#[test]
fn detection_precedence() {
    let detector = Detector::new(&[
        LanguageMapping { pattern: "**/special/*.rs".into(), language: Language::Ruby },
        LanguageMapping { pattern: "*.rs".into(), language: Language::Python },
    ])
    .unwrap();
    let source = b"#!/bin/bash # vim: ft=lua\n".to_vec();
    let choose = |path: &str, picker| detector.detect(Path::new(path), picker, &source, &|| false).unwrap();
    assert_eq!(choose("main.rs", Some(Language::Generic)).language, Language::Generic);
    assert_eq!(choose("main.rs", Some(Language::Generic)).origin, Origin::Picker);
    assert_eq!(choose("main.rs", None).language, Language::Python);
    assert_eq!(choose("main.rs", None).origin, Origin::UserMapping);
    assert_eq!(choose("src/special/main.RS", None).language, Language::Ruby);
    assert_eq!(choose("main.unknown", None).language, Language::Lua);
    assert_eq!(choose("main.unknown", None).origin, Origin::Modeline);
    for line in
        ["# vim: set filetype=python :", "# vi: ft=python", "# -*- mode: python; coding: utf-8 -*-", "# -*- Python -*-"]
    {
        assert_eq!(detected("main.rs", line).language, Language::Python, "{line}");
        assert_eq!(detected("main.rs", line).origin, Origin::Modeline);
    }
    for (line, language) in [
        ("#!/usr/bin/env -S python3 -u", Language::Python),
        ("#!/bin/bash", Language::Bash),
        ("<?xml version='1.0'?>", Language::Xml),
    ] {
        assert_eq!(detected("main.rs", line).language, language);
        assert_eq!(detected("main.rs", line).origin, Origin::FirstLine);
    }
    assert_eq!(detected("main.rs", "{\"foo\": 1}").language, Language::Rust);
    assert_eq!(detected("main.rs", "{\"foo\": 1}").origin, Origin::Filename);
    for (line, language) in [
        ("{\"foo\": 1}", Language::Json),
        ("<root>x</root>", Language::Xml),
        ("[section]", Language::Ini),
        ("key=value", Language::Ini),
    ] {
        assert_eq!(detected("unknown", line).language, language);
        assert_eq!(detected("unknown", line).origin, Origin::Content);
    }
    assert!(detected("unknown", "2026-10-23 ERROR failed").log_levels);
    assert!(detected("server.log", "hello").log_levels);
    assert_eq!(detected("unknown", "unclassified words").language, Language::Generic);
    assert_eq!(detected("script", "#!/usr/bin/perl").language, Language::Generic);
    assert_eq!(detected("script", "#!/usr/bin/env -u python ruby").language, Language::Ruby);
    assert_eq!(detected("script", "#!/usr/bin/env NODE_ENV=python bash").language, Language::Bash);
    assert_eq!(Language::from_id("nonexistent"), None);
    assert_eq!(Language::ALL.len(), 32);
    assert_eq!(Language::CATALOGUE.len(), 33);
    assert!(Language::Generic.bundled().is_none());
    let bare = Detector::new(&[LanguageMapping { pattern: "foo".into(), language: Language::Json }]).unwrap();
    assert_eq!(bare.detect(Path::new("a.FOO"), None, &source, &|| false).unwrap().language, Language::Json);
    assert!(Detector::new(&[LanguageMapping { pattern: "[".into(), language: Language::Json }]).is_err());
}

struct ReadProbe {
    bytes: Vec<u8>,
    fragment: usize,
    reads: Mutex<Vec<(usize, usize)>>,
}
impl TextSource for ReadProbe {
    fn len(&self) -> usize {
        self.bytes.len()
    }
    fn chunk(&self, offset: usize, max_len: usize) -> io::Result<Cow<'_, [u8]>> {
        self.reads.lock().unwrap().push((offset, max_len));
        Ok(Cow::Borrowed(&self.bytes[offset..self.bytes.len().min(offset + max_len.min(self.fragment))]))
    }
}

#[test]
fn bounded_first_line_read() {
    let mut bytes = vec![b'x'; 128 * 1024];
    bytes[70 * 1024..70 * 1024 + 16].copy_from_slice(b" vim: ft=python ");
    let probe = ReadProbe { bytes, fragment: 8192, reads: Mutex::new(Vec::new()) };
    let detector = Detector::new(&[]).unwrap();
    let result = detector.detect(Path::new("unknown"), None, &probe, &|| false).unwrap();
    assert_eq!(result.language, Language::Generic);
    let reads = probe.reads.lock().unwrap();
    assert!(reads.iter().all(|&(at, max)| at + max <= 64 * 1024));
    assert_eq!(reads.len(), 8);
    drop(reads);
    let probe = ReadProbe { bytes: b"hello\n# vim: ft=rust".to_vec(), fragment: 3, reads: Mutex::new(Vec::new()) };
    assert_eq!(detector.detect(Path::new("unknown"), None, &probe, &|| false).unwrap().language, Language::Generic);
    assert_eq!(probe.reads.lock().unwrap().len(), 2);
    probe.reads.lock().unwrap().clear();
    assert_eq!(
        detector.detect(Path::new("unknown"), Some(Language::Python), &probe, &|| false).unwrap().language,
        Language::Python
    );
    assert!(probe.reads.lock().unwrap().is_empty(), "picker must not read source");
    let mapping = Detector::new(&[LanguageMapping { pattern: "*.foo".into(), language: Language::Python }]).unwrap();
    mapping.detect(Path::new("a.foo"), None, &probe, &|| false).unwrap();
    assert!(probe.reads.lock().unwrap().is_empty(), "user mapping must not read source");
    assert!(matches!(detector.detect(Path::new("unknown"), None, &probe, &|| true), Err(Error::Cancelled)));
}

#[test]
fn generic_real_spans() {
    let mut engine = Engine::new(Config::default()).unwrap();
    for marker in ["#", "//", ";", "--", "/*"] {
        let text = format!("fn (42) \"# string\" {marker} comment */\n[3]");
        let h = engine
            .highlight(&text.as_bytes().to_vec(), Language::Generic, 1, None, None, 0..text.len(), &|| false)
            .unwrap();
        assert_eq!(h.mode, Mode::Lexical);
        let classified = |start, end, class| h.spans.iter().any(|s| s.range == (start..end) && s.class == class);
        assert!(classified(3, 4, Class::Punctuation), "{marker}: {:?}", h.spans);
        assert!(classified(4, 6, Class::Number));
        assert!(classified(8, 18, Class::String));
        assert!(h.spans.iter().any(|s| s.range.start == 19 && s.class == Class::Comment));
        assert!(h.spans.iter().all(|s| s.range.start >= 3), "generic must not invent Rust keywords");
    }
    let text = b"ERROR failed: [42]".to_vec();
    let h = engine.highlight(&text, Language::Generic, 2, None, None, 0..text.len(), &|| false).unwrap();
    assert!(h.spans.iter().any(|s| s.range == (0..5) && s.class == Class::Error));
    // Generic never requests a fake known-language grammar from a provider.
    struct NoGrammar;
    impl GrammarProvider for NoGrammar {
        fn grammar(&self, _: Language) -> Result<Option<Grammar>, Error> {
            panic!("Generic requested a grammar")
        }
    }
    Engine::with_provider(Config::default(), Arc::new(NoGrammar))
        .unwrap()
        .highlight(&text, Language::Generic, 1, None, None, 0..text.len(), &|| false)
        .unwrap();
}

#[test]
fn generic_large_source_reads_only_margin() {
    struct Huge(Mutex<Vec<std::ops::Range<usize>>>);
    impl TextSource for Huge {
        fn len(&self) -> usize {
            1024 * 1024 * 1024
        }
        fn chunk(&self, offset: usize, max: usize) -> io::Result<Cow<'_, [u8]>> {
            self.0.lock().unwrap().push(offset..offset + max);
            Ok(Cow::Owned(vec![b'('; max]))
        }
    }
    let source = Huge(Mutex::new(Vec::new()));
    let result = Engine::new(Config::default())
        .unwrap()
        .highlight(&source, Language::Generic, 1, None, None, 900_000_000..900_000_032, &|| false)
        .unwrap();
    assert_eq!(result.mode, Mode::Lexical);
    assert_eq!(result.spans.len(), 32);
    assert_eq!(source.0.lock().unwrap().iter().map(|r| r.len()).sum::<usize>(), 32 + 8192);
    assert!(source.0.lock().unwrap().iter().all(|r| r.start >= 900_000_000 - 4096 && r.end <= 900_000_032 + 4096));
}
