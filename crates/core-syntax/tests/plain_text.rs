use kwil_core_syntax::detection::{DetectionOrigin, Detector, LanguageMapping};
use kwil_core_syntax::{Config, Engine, Language};

#[test]
fn plain_text_disables_highlighting_while_unknown_files_keep_generic_colours() {
    let source = b"// comment\nlet answer = 42; \"string\" [value]\n".to_vec();
    let mut engine = Engine::new(Config::default()).unwrap();
    let generic = engine.highlight(&source, Language::Generic, 1, None, None, 0..source.len(), &|| false).unwrap();
    assert!(!generic.spans.is_empty());
    for alias in ["plaintext", "plain", "text", "fundamental"] {
        let language = Language::from_id(alias).unwrap();
        let detector = Detector::new(&[LanguageMapping { pattern: "*.rs".into(), language }]).unwrap();
        let detection = detector.detect(std::path::Path::new("code.rs"), None, &source, &|| false).unwrap();
        assert_eq!(detection.origin, DetectionOrigin::UserMapping);
        assert_eq!(detection.language, language);
        let plain = engine.highlight(&source, language, 1, None, None, 0..source.len(), &|| false).unwrap();
        assert!(plain.spans.is_empty(), "{alias} must disable syntax colours");
        let window = engine.highlight_window(&source, language, 1, 3..source.len(), 0, &|| false).unwrap();
        assert!(window.spans.is_empty());
    }
}
