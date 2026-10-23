use kwil_core_format::{Language, Settings, format};

#[test]
fn selected_crlf_prefix_does_not_normalize_into_an_extra_line_break() {
    let input = b"a\nb\r\nc";
    let result = format(input, Language::Generic, "rust", Some(0..4), &Settings::default()).unwrap();
    assert_eq!(result.bytes, b"a\nb\r");
    assert_eq!(result.replacement, None);
}

#[test]
fn selection_uses_actual_following_bytes_to_allow_physical_line_end_trim() {
    for (kind, ending) in [("rust", "\n"), ("rust", "\r\n"), ("rust", "\r"), ("sh", "\n")] {
        let input = format!("value  {ending}next");
        let result = format(input.as_bytes(), Language::Generic, kind, Some(0..7), &Settings::default()).unwrap();
        assert_eq!(result.bytes, b"value", "{kind}: {ending:?}");
        assert_eq!(result.replacement, Some(0..7));
        let mut applied = input.clone();
        applied.replace_range(0..7, std::str::from_utf8(&result.bytes).unwrap());
        assert_eq!(applied, format!("value{ending}next"));
    }
    // In shell, CR is an argument byte, so trimming would join argument data.
    let result = format(b"value  \rnext", Language::Generic, "sh", Some(0..7), &Settings::default()).unwrap();
    assert_eq!(result.bytes, b"value  ");
    assert_eq!(result.replacement, None);
}
