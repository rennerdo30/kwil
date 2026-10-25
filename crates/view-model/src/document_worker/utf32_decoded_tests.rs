#[test]
fn utf32_decoded_open_paints_bounded_preview_then_retains_lossless_backing() {
    for (encoding, raw, preview) in [
        (
            Encoding::UTF32_LE,
            b"\xff\xfe\0\0A\0\0\0\0\xf6\x01\0\0\xd8\0\0\xaa".as_slice(),
            b"A\xf0\x9f\x98\x80\\x00\\xD8\\x00\\x00\\xAA".as_slice(),
        ),
        (
            Encoding::UTF32_BE,
            b"\0\0\xfe\xff\0\0\0A\0\x01\xf6\0\0\0\xd8\0\xaa".as_slice(),
            b"A\xf0\x9f\x98\x80\\x00\\x00\\xD8\\x00\\xAA".as_slice(),
        ),
    ] {
        let fixture = Fixture::new();
        let (path, file) = fixture.file(raw);
        let prepared = prepare_decoded_document(path, file, None, &fixture.0, &AtomicBool::new(false), |page| {
            assert_eq!(page.text, preview);
            assert_eq!(page.source, 4..raw.len() as u64);
            assert_eq!(std::fs::read_dir(&fixture.0)?.count(), 1, "preview precedes private full-file backing");
            Ok(())
        })
        .unwrap();
        assert_eq!(prepared.document.save_plan().encoding, encoding);
        let mut logical = Vec::new();
        prepared.document.snapshot().write_to(&mut logical).unwrap();
        assert_eq!(logical, b"A\xf0\x9f\x98\x80\xff\xff");
        let mut saved = Vec::new();
        kwil_core_text::encoding::write_snapshot(
            &prepared.document.snapshot(),
            prepared.document.save_plan(),
            &mut saved,
        )
        .unwrap();
        assert_eq!(saved, raw);
        drop(prepared.document);
        assert!(prepared.backing.try_retire().is_ok());
    }
}
