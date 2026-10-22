use kwil_core_markdown::{BlockKind, InlineKind, parse, render_text};

#[test]
fn cjk_wraps_by_terminal_cells() {
    let model = parse("你好").unwrap();
    assert_eq!(render_text(&model, 2), "你\n好\n");
    assert_eq!(render_text(&model, 1), "你\n好\n");
    assert_eq!(render_text(&model, 0), "你\n好\n");
    assert_eq!(render_text(&parse("```\n你好\n```").unwrap(), 2), "``\n`\n你\n好\n``\n`\n");
}

#[test]
fn combining_marks_stay_with_their_base() {
    let model = parse("e\u{301}e\u{301}").unwrap();
    assert_eq!(render_text(&model, 1), "e\u{301}\ne\u{301}\n");
}

#[test]
fn emoji_zwj_clusters_are_never_split() {
    let model = parse("👩‍💻👨‍👩‍👧‍👦!").unwrap();
    assert_eq!(render_text(&model, 2), "👩‍💻\n👨‍👩‍👧‍👦\n!\n");
    assert_eq!(render_text(&model, 1), "👩‍💻\n👨‍👩‍👧‍👦\n!\n");
}

#[test]
fn list_and_quote_prefixes_share_the_cell_budget() {
    let model = parse("> - 你好").unwrap();
    assert_eq!(render_text(&model, 6), "> - 你\n>   好\n");
    // An overwide cluster is alone, without prefix cells pushing it further over budget.
    assert_eq!(render_text(&model, 1), "你\n好\n");
    assert_eq!(render_text(&model, 2), "你\n好\n");
    assert_eq!(render_text(&parse("> e\u{301}e\u{301}").unwrap(), 3), "> e\u{301}\n> e\u{301}\n");
    assert_eq!(render_text(&parse("- 👩‍💻👩‍💻").unwrap(), 4), "- 👩‍💻\n  👩‍💻\n");
}

#[test]
fn linked_image_keeps_its_target_and_source_range() {
    let source = "[![alt](pic.png)](https://target.test)";
    let model = parse(source).unwrap();
    let block = &model.blocks[0];
    assert_eq!(block.kind, BlockKind::Paragraph);
    assert_eq!(block.spans.len(), 1);
    assert_eq!(block.spans[0].kind, InlineKind::Image { alt: "alt".into(), url: "pic.png".into() });
    assert_eq!(block.spans[0].style.link.as_deref(), Some("https://target.test"));
    assert_eq!(block.source_lines, 0..1);
    assert_eq!(block.source_bytes, 0..source.len());
}
