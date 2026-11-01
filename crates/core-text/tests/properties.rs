#[path = "support/edits.rs"]
mod edits;
#[path = "support/encodings.rs"]
mod encodings;

use proptest::prelude::*;

fn text() -> impl Strategy<Value = String> {
    prop::collection::vec(prop_oneof![4 => any::<char>(), 2 => Just('\n'), 1 => Just('\r'), 1 => Just('\0')], 0..24)
        .prop_map(|chars| chars.into_iter().collect())
}

fn config() -> ProptestConfig {
    // Default only; PROPTEST_CASES can raise this on CI or lower it locally.
    ProptestConfig {
        cases: std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(32),
        failure_persistence: Some(Box::new(proptest::test_runner::FileFailurePersistence::WithSource(
            "proptest-regressions",
        ))),
        ..ProptestConfig::default()
    }
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn edits_undo_and_snapshots_match_string(initial in text(), ops in prop::collection::vec((0u8..5, any::<u16>(), any::<u16>(), text()), 0..48)) {
        edits::edit_sequence(&initial, &ops);
    }

    #[test]
    fn sparse_and_extended_line_index_match_vec(bytes in prop::collection::vec(prop_oneof![any::<u8>(), Just(b'\n'), Just(b'\r')], 0..512), every in 0u64..16, split in any::<u16>()) {
        edits::check_index(&bytes, every, split as usize % (bytes.len() + 1));
    }

    #[test]
    fn supported_encodings_roundtrip(unicode in text(), bytes in prop::collection::vec(any::<u8>(), 0..32), bom in any::<bool>()) {
        for label in encodings::LABELS {
            encodings::roundtrip(&format!("K{unicode}"), label, bom);
            encodings::roundtrip(&encodings::representable_text(&bytes, label), label, bom);
        }
    }

    #[test]
    fn malformed_utf8_is_preserved(bytes in prop::collection::vec(any::<u8>(), 0..256)) {
        encodings::preserve_utf8(&bytes);
    }

    #[test]
    fn cr_only_lines(rows in prop::collection::vec("[a-z]{0,12}", 2..16)) {
        let text = rows.join("\r");
        let mut buffer = kwil_core_text::Buffer::empty();
        buffer.insert(0, text.as_bytes()).unwrap();
        prop_assert_eq!(buffer.line_count(), rows.len() as u64);
        let index = kwil_core_text::LineIndex::build(text.as_bytes(), 1, 1, &std::sync::atomic::AtomicU64::new(0)).unwrap();
        prop_assert_eq!(index.line_count(), rows.len() as u64);
    }
}

#[test]
fn lf_crlf_piece_edges_and_empty_encodings() {
    edits::edit_sequence(
        "α\r\n\nlast",
        &[
            (0, 2, 2, "\n".into()), // Insert between CR and LF.
            (2, 0, 3, "\r\n".into()),
            (3, 0, 0, String::new()),
            (4, 0, 0, String::new()),
        ],
    );
    for label in encodings::LABELS {
        for bom in [false, true] {
            encodings::roundtrip("", label, bom);
        }
    }
}
