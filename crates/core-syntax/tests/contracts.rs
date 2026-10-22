use kwil_core_syntax::{
    Class, Config, Engine, Error, InputEdit, Language, Mode, Point, Request, TextSource, Worker, detect,
};
use std::{
    borrow::Cow,
    io,
    ops::Range,
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

fn run(language: Language, text: &str) -> kwil_core_syntax::Highlights {
    Engine::new(Config::default())
        .unwrap()
        .highlight(&text.as_bytes().to_vec(), language, 1, None, None, 0..text.len(), &|| false)
        .unwrap()
}
fn at(h: &kwil_core_syntax::Highlights, range: Range<usize>, class: Class) {
    assert!(
        h.spans.iter().any(|s| s.range.start <= range.start && s.range.end >= range.end && s.class == class),
        "missing {class:?} at {range:?}: {:?}",
        h.spans
    );
}

#[test]
#[cfg(feature = "lang-rust")]
fn rust_ranges() {
    let h = run(Language::Rust, "fn main() { let x = 42; // hi\n}");
    assert_eq!(h.mode, Mode::TreeSitter);
    at(&h, 0..2, Class::Keyword);
    at(&h, 3..7, Class::Function);
    at(&h, 20..22, Class::Number);
    at(&h, 24..29, Class::Comment);
}
#[test]
#[cfg(feature = "lang-python")]
fn python_ranges() {
    let h = run(Language::Python, "def greet():\n    return \"hi\" # ok\n");
    at(&h, 0..3, Class::Keyword);
    at(&h, 4..9, Class::Function);
    at(&h, 24..28, Class::String);
    at(&h, 29..33, Class::Comment);
}
#[test]
#[cfg(feature = "lang-json")]
fn json_ranges() {
    let h = run(Language::Json, "{\"key\": 42, \"ok\": true}");
    at(&h, 1..6, Class::String);
    at(&h, 8..10, Class::Number);
    at(&h, 18..22, Class::Constant);
}
#[test]
#[cfg(feature = "lang-markdown")]
fn markdown_ranges() {
    let h = run(Language::Markdown, "# Title\n\n[ref]: https://example.com\n");
    at(&h, 2..7, Class::Heading);
    at(&h, 16..35, Class::Link);
}

#[test]
fn detection() {
    for (name, line, expected) in [
        ("main.RS", "", Language::Rust),
        ("Dockerfile", "", Language::Dockerfile),
        ("Dockerfile.dev", "", Language::Dockerfile),
        ("Makefile", "", Language::Make),
        ("Cargo.lock", "", Language::Toml),
        ("a.tsx", "", Language::Tsx),
        ("a.ps1", "", Language::PowerShell),
        ("a.bat", "", Language::Batch),
        ("script", "#!/usr/bin/env -S python3 -u", Language::Python),
        ("script", "#!/bin/bash", Language::Bash),
        ("script", "#!/usr/bin/env node", Language::JavaScript),
        ("a", "<?xml version=\"1.0\"?>", Language::Xml),
        ("a", "<!DOCTYPE html>", Language::Html),
        ("a", "diff --git a/a b/a", Language::Diff),
        ("main.rs", "#!/usr/bin/python3", Language::Rust),
    ] {
        assert_eq!(detect(name, line.as_bytes()), Some(expected), "{name}: {line}");
    }
    assert_eq!(detect("unknown", b"plain text"), None);
}

#[test]
#[cfg(feature = "lang-rust")]
fn incremental_changed_region_equals_fresh() {
    let before = b"fn main() { let x = 1; }\nfn later() {}\n".to_vec();
    let after = b"fn main() { let x = 12; }\nfn later() {}\n".to_vec();
    let edit = InputEdit {
        start_byte: 21,
        old_end_byte: 21,
        new_end_byte: 22,
        start_position: Point::new(0, 21),
        old_end_position: Point::new(0, 21),
        new_end_position: Point::new(0, 22),
    };
    let mut engine = Engine::new(Config { margin_bytes: 0, ..Config::default() }).unwrap();
    engine.highlight(&before, Language::Rust, 1, None, None, 0..before.len(), &|| false).unwrap();
    let updated = engine.highlight(&after, Language::Rust, 2, Some(1), Some(&edit), 20..22, &|| false).unwrap();
    let fresh = Engine::new(Config { margin_bytes: 0, ..Config::default() })
        .unwrap()
        .highlight(&after, Language::Rust, 2, None, None, 20..22, &|| false)
        .unwrap();
    assert!(updated.incremental);
    assert_eq!(updated.queried, 20..22);
    assert_eq!(updated.spans, fresh.spans);
    at(&updated, 20..22, Class::Number);
}

struct Chunks(Vec<u8>);
impl TextSource for Chunks {
    fn len(&self) -> usize {
        self.0.len()
    }
    fn chunk(&self, offset: usize, max: usize) -> io::Result<Cow<'_, [u8]>> {
        Ok(Cow::Borrowed(&self.0[offset..self.0.len().min(offset + max.min(3))]))
    }
}
#[test]
#[cfg(feature = "lang-rust")]
fn fragmented_text_and_range_clipping() {
    let source = Chunks(b"fn main() { /* comment across chunks */ let x = 12; }".to_vec());
    let h = Engine::new(Config::default())
        .unwrap()
        .highlight(&source, Language::Rust, 1, None, None, 17..23, &|| false)
        .unwrap();
    assert_eq!(h.spans, vec![kwil_core_syntax::Span { range: 17..23, class: Class::Comment }]);
}

#[test]
fn lexical_tables() {
    for (language, text, keyword, comment) in [
        (Language::Rust, "let x = 42; // hi", 0..3, 12..17),
        (Language::Python, "def f(): # hi", 0..3, 9..13),
        (Language::Sql, "SELECT 42 -- hi", 0..6, 10..15),
        (Language::PowerShell, "function f { <# hi #> }", 0..8, 13..21),
        (Language::Batch, "IF x REM hi", 0..2, 5..11),
        (Language::Lua, "local x --[[ hi ]]", 0..5, 8..18),
    ] {
        let mut engine = Engine::new(Config { parse_limit: 0, ..Config::default() }).unwrap();
        let h = engine.highlight(&text.as_bytes().to_vec(), language, 1, None, None, 0..text.len(), &|| false).unwrap();
        assert_eq!(h.mode, Mode::Lexical);
        at(&h, keyword, Class::Keyword);
        at(&h, comment, Class::Comment);
    }
    let mut engine = Engine::new(Config { parse_limit: 0, ..Config::default() }).unwrap();
    let text = b"let s = \"a\\\"b\"; let n = 42;".to_vec();
    let h = engine.highlight(&text, Language::Rust, 1, None, None, 0..text.len(), &|| false).unwrap();
    at(&h, 8..14, Class::String);
    at(&h, 24..26, Class::Number);
}

struct Huge {
    reads: Mutex<Vec<Range<usize>>>,
}
impl TextSource for Huge {
    fn len(&self) -> usize {
        1024 * 1024 * 1024
    }
    fn chunk(&self, offset: usize, max: usize) -> io::Result<Cow<'_, [u8]>> {
        let size = max.min(self.len() - offset);
        self.reads.lock().unwrap().push(offset..offset + size);
        let pattern = b"let n = 42; // log\n";
        Ok(Cow::Owned((offset..offset + size).map(|i| pattern[i % pattern.len()]).collect()))
    }
}
#[test]
fn huge_source_is_window_bounded() {
    let source = Huge { reads: Mutex::new(Vec::new()) };
    let start = 900_000_000;
    let h = Engine::new(Config::default())
        .unwrap()
        .highlight(&source, Language::Rust, 1, None, None, start..start + 1024, &|| false)
        .unwrap();
    assert_eq!(h.mode, Mode::Lexical);
    assert!(!h.spans.is_empty());
    let reads = source.reads.lock().unwrap();
    assert!(reads.iter().all(|r| r.start >= start - 4096 && r.end <= start + 1024 + 4096));
    assert_eq!(reads.iter().map(Range::len).sum::<usize>(), 1024 + 8192);
}

#[test]
fn every_bundled_query_compiles() {
    for &language in Language::ALL {
        if let Some(grammar) = language.bundled() {
            tree_sitter::Query::new(&grammar.language, &grammar.highlights)
                .unwrap_or_else(|e| panic!("{}: {e}", language.id()));
        }
    }
}

#[test]
fn cancel_before_source_read() {
    let error = Engine::new(Config::default())
        .unwrap()
        .highlight(&Vec::new(), Language::Rust, 1, None, None, 0..0, &|| true)
        .unwrap_err();
    assert!(matches!(error, Error::Cancelled));
}

struct Gated {
    bytes: Vec<u8>,
    entered: mpsc::SyncSender<()>,
    release: Mutex<mpsc::Receiver<()>>,
    entered_once: std::sync::atomic::AtomicBool,
}
impl TextSource for Gated {
    fn len(&self) -> usize {
        self.bytes.len()
    }
    fn chunk(&self, offset: usize, max: usize) -> io::Result<Cow<'_, [u8]>> {
        if offset == 0 && !self.entered_once.swap(true, std::sync::atomic::Ordering::Relaxed) {
            let _ = self.entered.try_send(());
            self.release.lock().unwrap().recv_timeout(Duration::from_secs(3)).map_err(io::Error::other)?;
        }
        self.bytes.chunk(offset, max)
    }
}
fn request(source: Arc<dyn TextSource>, revision: u64) -> Request {
    let len = source.len();
    Request { source, language: Language::Rust, revision, previous_revision: None, edit: None, visible: 0..len }
}
#[test]
fn worker_cancels_stale_work_and_keeps_previous() {
    let (wake, events) = mpsc::channel();
    let mut worker = Worker::new(Config::default(), move || {
        let _ = wake.send(());
    })
    .unwrap();
    worker.submit(request(Arc::new(b"let x = 1;".to_vec()), 1)).unwrap();
    events.recv_timeout(Duration::from_secs(5)).unwrap();
    let previous = worker.take_update().unwrap().unwrap();
    at(&previous, 0..3, Class::Keyword);
    let (entered, wait) = mpsc::sync_channel(1);
    let (release, gate) = mpsc::sync_channel(1);
    worker
        .submit(request(
            Arc::new(Gated {
                bytes: b"let stale = 2;".to_vec(),
                entered,
                release: Mutex::new(gate),
                entered_once: std::sync::atomic::AtomicBool::new(false),
            }),
            2,
        ))
        .unwrap();
    wait.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(Arc::ptr_eq(worker.previous().unwrap(), &previous));
    // Coalescing three revisions must keep the last one; no parse lock on submit.
    worker.submit(request(Arc::new(b"let skipped = 3;".to_vec()), 3)).unwrap();
    let (new_entered, new_wait) = mpsc::sync_channel(1);
    let (new_release, new_gate) = mpsc::sync_channel(1);
    worker
        .submit(request(
            Arc::new(Gated {
                bytes: b"let newest = 4;".to_vec(),
                entered: new_entered,
                release: Mutex::new(new_gate),
                entered_once: std::sync::atomic::AtomicBool::new(false),
            }),
            4,
        ))
        .unwrap();
    release.send(()).unwrap();
    new_wait.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(events.try_recv().is_err(), "cancelled parse emitted a stale wake");
    new_release.send(()).unwrap();
    events.recv_timeout(Duration::from_secs(5)).unwrap();
    let newest = worker.take_update().unwrap().unwrap();
    assert_eq!(newest.revision, 4);
    at(&newest, 0..3, Class::Keyword);
    assert!(events.try_recv().is_err());
    worker.shutdown().unwrap();
}

#[test]
fn rejects_oversized_window_and_bad_source() {
    let mut engine = Engine::new(Config { max_window_bytes: 8, margin_bytes: 0, ..Config::default() }).unwrap();
    assert!(matches!(
        engine.highlight(&b"123456789".to_vec(), Language::Rust, 1, None, None, 0..9, &|| false),
        Err(Error::InvalidRange)
    ));
    struct Broken;
    impl TextSource for Broken {
        fn len(&self) -> usize {
            8
        }
        fn chunk(&self, _: usize, _: usize) -> io::Result<Cow<'_, [u8]>> {
            Ok(Cow::Borrowed(b""))
        }
    }
    assert!(matches!(engine.highlight(&Broken, Language::Rust, 2, None, None, 0..8, &|| false), Err(Error::Io(_))));
}
