use kwil_core_markdown::*;

fn model(s: &str) -> Model {
    parse(s).unwrap()
}
fn text(spans: &[Span]) -> String {
    spans
        .iter()
        .map(|s| match &s.kind {
            InlineKind::Text(s) | InlineKind::Code(s) => s.as_str(),
            _ => "\n",
        })
        .collect()
}

#[test]
fn headings_paragraphs_quotes_and_rules() {
    let m = model("# Title\n\npara\n\n> > quoted\n\n---\n");
    assert_eq!(m.blocks.len(), 4);
    assert_eq!(m.blocks[0].kind, BlockKind::Heading(1));
    assert_eq!(m.blocks[1].kind, BlockKind::Paragraph);
    assert_eq!(m.blocks[2].quote_depth, 2);
    assert_eq!(text(&m.blocks[2].spans), "quoted");
    assert_eq!(m.blocks[3].kind, BlockKind::ThematicBreak);
}

#[test]
fn inline_styles_links_code_and_breaks() {
    let m = model("plain *em* **strong** ~~gone~~ `code` [***both***](https://example.org)\nsoft  \nhard\n");
    let s = &m.blocks[0].spans;
    assert!(s.iter().any(|s| s.style.emphasis && s.kind == InlineKind::Text("em".into())));
    assert!(s.iter().any(|s| s.style.strong && s.kind == InlineKind::Text("strong".into())));
    assert!(s.iter().any(|s| s.style.strikethrough));
    assert!(s.iter().any(|s| s.kind == InlineKind::Code("code".into())));
    assert!(
        s.iter().any(|s| s.style.emphasis && s.style.strong && s.style.link.as_deref() == Some("https://example.org"))
    );
    assert!(s.iter().any(|s| s.kind == InlineKind::SoftBreak));
    assert!(s.iter().any(|s| s.kind == InlineKind::HardBreak));
    assert!(s.iter().any(|s| s.kind == InlineKind::Text("plain ".into()) && s.style == Style::default()));
}

#[test]
fn list_markers_tasks_nesting_and_continuations() {
    let m = model("3) [x] done\n4) [ ] todo\n   - nested\n\n     continuation\n\n+ bullet\n");
    assert!(matches!(
        m.blocks[0].kind,
        BlockKind::ListItem { marker: ListMarker::Ordered { number: 3, delimiter: ')' }, task: Some(true) }
    ));
    assert!(matches!(
        m.blocks[1].kind,
        BlockKind::ListItem { marker: ListMarker::Ordered { number: 4, .. }, task: Some(false) }
    ));
    assert_eq!(m.blocks[2].list_depth, 2);
    assert_eq!(text(&m.blocks[2].spans), "nested");
    assert_eq!(m.blocks[3].list_depth, 2);
    assert_eq!(m.blocks[3].kind, BlockKind::Paragraph);
    assert!(matches!(m.blocks[4].kind, BlockKind::ListItem { marker: ListMarker::Bullet('+'), task: None }));
}

#[test]
fn fenced_and_indented_code_are_raw() {
    let m = model("```rust extra\nlet x = 1;\n\n```\n\n    raw <tag>\n");
    assert_eq!(m.blocks[0].kind, BlockKind::CodeBlock { language: Some("rust".into()), raw: "let x = 1;\n\n".into() });
    assert_eq!(m.blocks[1].kind, BlockKind::CodeBlock { language: None, raw: "raw <tag>\n".into() });
    assert!(m.blocks.iter().all(|b| b.spans.is_empty()));
}

#[test]
fn tables_keep_alignment_and_styled_cells() {
    let m = model("| a | b | c | d |\n|:--|:-:|--:|---|\n| *x* | `y` | [z](u) | end |\n");
    let BlockKind::Table { alignments, rows } = &m.blocks[0].kind else { panic!("table") };
    assert_eq!(*alignments, vec![Alignment::Left, Alignment::Center, Alignment::Right, Alignment::None]);
    assert_eq!(rows.len(), 2);
    assert_eq!(text(&rows[0][0]), "a");
    assert!(rows[1][0][0].style.emphasis);
    assert_eq!(rows[1][1][0].kind, InlineKind::Code("y".into()));
    assert_eq!(rows[1][2][0].style.link.as_deref(), Some("u"));
}

#[test]
fn images_and_html_remain_inert() {
    let m = model(
        "![*alt* text](pic.png)\n\nbefore ![inline](x) after\n\n<div>\n<script>bad()</script>\n</div>\n\ninline <b>html</b>\n",
    );
    assert_eq!(m.blocks[0].kind, BlockKind::Image { alt: "alt text".into(), url: "pic.png".into() });
    assert!(m.blocks[1].spans.iter().any(|s| s.kind == InlineKind::Image { alt: "inline".into(), url: "x".into() }));
    assert_eq!(m.blocks[2].kind, BlockKind::Html("<div>\n<script>bad()</script>\n</div>\n".into()));
    assert_eq!(text(&m.blocks[3].spans), "inline <b>html</b>");
}

#[test]
fn source_ranges_use_document_lines_and_crlf() {
    let input = "# h\r\n\r\n> quote\r\n> next\r\n\r\n```\r\nx\r\n```\r\n";
    let m = model(input);
    assert_eq!(m.blocks.iter().map(|b| b.source_lines.clone()).collect::<Vec<_>>(), vec![0..1, 2..4, 5..8]);
    assert_eq!(&input[m.blocks[1].source_bytes.clone()], "> quote\r\n> next\r\n");
    assert_eq!(model("a\rb\r").blocks[0].source_lines, 0..2);
}

#[test]
fn section_parse_preserves_global_coordinates() {
    let input = "# first\n\n## second\n\nbody\n";
    let start = input.find("##").unwrap();
    let m = parse_section(input, start..input.len()).unwrap();
    assert_eq!(m.source_bytes, start..input.len());
    assert_eq!(m.blocks[0].kind, BlockKind::Heading(2));
    assert_eq!(m.blocks[0].source_lines, 2..3);
    assert_eq!(m.blocks[0].source_bytes.start, start);
    assert_eq!(parse_section("é", 1..2), Err(PreviewError::InvalidRange));
    assert_eq!(parse_section("hi", 3..4), Err(PreviewError::InvalidRange));
    assert_eq!(parse_section("hi", std::ops::Range { start: 2, end: 1 }), Err(PreviewError::InvalidRange));
    assert!(parse_section(input, 0..0).unwrap().blocks.is_empty());
}

#[test]
fn boundaries_keep_complete_containers_and_neighbours() {
    let input = "# head\n\npara\n\n```\ninside\n\nend\n```\n\n- list\n  - child\n\nafter\n\nlast\n";
    let r = block_boundaries(input, 5..6).unwrap();
    assert_eq!(&input[r], "para\n\n```\ninside\n\nend\n```\n\n- list\n  - child\n\n");
    let r = block_boundaries(input, 11..12).unwrap();
    assert!(input[r].contains("- list\n  - child"));
    assert_eq!(block_boundaries("", 0..1).unwrap(), 0..0);
    assert_eq!(block_boundaries("x\n", 9..10), Err(PreviewError::InvalidRange));
    assert_eq!(block_boundaries("x\n", std::ops::Range { start: 1, end: 0 }), Err(PreviewError::InvalidRange));
}

#[test]
fn reference_definitions_force_global_scope_and_footnotes_stay_off() {
    let input = "[text][id]\n\nother\n\n[id]: https://example.org\n";
    assert_eq!(block_boundaries(input, 2..3).unwrap(), 0..input.len());
    assert_eq!(model(input).blocks[0].spans[0].style.link.as_deref(), Some("https://example.org"));
    // With footnotes off this is a regular CommonMark reference link, not a footnote.
    let plain = model("[^a]\n\n[^a]: note\n");
    assert_eq!(text(&plain.blocks[0].spans), "^a");
    assert_eq!(plain.blocks[0].spans[0].style.link.as_deref(), Some("note"));
}

#[test]
fn size_guard_checks_document_before_section() {
    let input = "x".repeat(MAX_PREVIEW_BYTES + 1);
    assert_eq!(parse(&input), Err(PreviewError::TooLargeForPreview));
    assert_eq!(parse_section(&input, 0..1), Err(PreviewError::TooLargeForPreview));
    assert_eq!(block_boundaries(&input, 0..1), Err(PreviewError::TooLargeForPreview));
}

#[test]
fn narrow_tui_wrap_preserves_markers_code_and_hard_breaks() {
    let m = model("# Head\n\n> quote words here\n\n- [x] done now\n\n```rs\na  b\n```\n\n---\n");
    assert_eq!(
        render_text(&m, 12),
        "# Head\n\n> quote\n> words here\n\n- [x] done\n      now\n\n```rs\na  b\n```\n\n---\n"
    );
    let s = render_text(&model("abcdefghijklmnop\n\n你好世界\n\na  \nb\n"), 3);
    assert!(s.lines().all(|l| l.chars().count() <= 3));
    assert!(s.contains("a\nb"));
    assert!(!render_text(&m, 0).is_empty());
}

#[test]
fn empty_containers_and_loose_list_keep_source_order() {
    let m = model(">\n\n-\n  - child\n\n  after\n");
    assert_eq!(m.blocks[0].quote_depth, 1);
    assert!(m.blocks[0].spans.is_empty());
    assert!(matches!(m.blocks[1].kind, BlockKind::ListItem { .. }));
    assert_eq!(text(&m.blocks[2].spans), "child");
    assert_eq!(text(&m.blocks[3].spans), "after");
    assert_eq!(m.blocks[3].kind, BlockKind::Paragraph);
    let quoted = model("- > quote\n");
    assert_eq!(quoted.blocks[1].quote_depth, 1);
    assert_eq!(text(&quoted.blocks[1].spans), "quote");
}

#[test]
fn image_table_and_list_source_ranges_and_tui_text() {
    let input = "![alt](u)\n\n|h|\n|-|\n|v|\n\n- one\n- two\n";
    let m = model(input);
    assert_eq!(m.blocks.iter().map(|b| b.source_lines.clone()).collect::<Vec<_>>(), vec![0..1, 2..5, 6..7, 7..8]);
    let output = render_text(&m, 80);
    assert_eq!(output, "![alt](u)\n\n| h |\n| v |\n\n- one\n\n- two\n");
    for b in &m.blocks {
        assert!(b.source_bytes.start < b.source_bytes.end);
        assert!(b.source_bytes.end <= input.len());
    }
}

#[test]
fn unicode_entities_nested_images_and_style_restoration() {
    let m = model("é &amp; **a *b* c** d ![out ![in](inner)](outer)\n");
    assert!(text(&m.blocks[0].spans).starts_with("é & a b c d "));
    let s = &m.blocks[0].spans;
    assert!(s.iter().any(|s| s.kind == InlineKind::Text(" c".into()) && s.style.strong && !s.style.emphasis));
    assert!(s.iter().any(|s| s.kind == InlineKind::Text(" d ".into()) && s.style == Style::default()));
    assert!(s.iter().any(|s| s.kind == InlineKind::Image { alt: "out in".into(), url: "outer".into() }));
}

#[test]
fn boundaries_follow_changed_fence_and_setext_context() {
    let input = "intro\n\nheading\n===\n\n```\nrest\n\nlast\n";
    let r = block_boundaries(input, 3..4).unwrap();
    assert_eq!(r, 0..input.len());
    let r = block_boundaries(input, 8..9).unwrap();
    assert_eq!(&input[r.start..r.end], "heading\n===\n\n```\nrest\n\nlast\n");
    let m = parse_section(input, r).unwrap();
    assert_eq!(m.blocks[0].kind, BlockKind::Heading(1));
    assert!(matches!(m.blocks[1].kind, BlockKind::CodeBlock { .. }));
}

#[test]
fn sparse_line_checkpoints_cover_long_lines_and_eof() {
    let prefix = format!("{}\r\n\n", "é".repeat(3000));
    let input = format!("{prefix}# end");
    let m = parse_section(&input, prefix.len()..input.len()).unwrap();
    assert_eq!(m.blocks[0].source_lines, 2..3);
    assert_eq!(m.blocks[0].source_bytes, prefix.len()..input.len());
    let r = block_boundaries(&input, 2..2).unwrap();
    assert_eq!(r, 0..input.len());
}
