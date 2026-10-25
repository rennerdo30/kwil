fn utf32_gzip_bytes(encoding: Encoding, units: &[u32], tail: &[u8]) -> Vec<u8> {
    let mut bytes = if encoding == Encoding::UTF32_LE { vec![0xff, 0xfe, 0, 0] } else { vec![0, 0, 0xfe, 0xff] };
    for unit in units {
        bytes.extend_from_slice(&if encoding == Encoding::UTF32_LE { unit.to_le_bytes() } else { unit.to_be_bytes() });
    }
    bytes.extend_from_slice(tail);
    bytes
}

#[test]
fn utf32_gzip_roundtrip_uses_four_byte_bom_and_lossless_checkpoint() {
    for encoding in [Encoding::UTF32_LE, Encoding::UTF32_BE] {
        let f = Fixture::new();
        let bytes = utf32_gzip_bytes(encoding, &[65, 0xd800, 0x1f600, 13, 10], &[0xaa, 0xbb, 0xcc]);
        let mut buffer = f.open(&gzip(&bytes), encoding, 4);
        buffer.insert(1, "é".as_bytes()).unwrap();
        let (saved, changed) = f.save(f.request(&buffer, false), plan(encoding, true));
        assert!(!changed);
        assert!(!saved.normalized);
        assert_eq!(
            unzip(&fs::read(f.path()).unwrap()),
            utf32_gzip_bytes(encoding, &[65, 0xe9, 0xd800, 0x1f600, 13, 10], &[0xaa, 0xbb, 0xcc])
        );
        assert_eq!(logical(&saved.snapshot), b"A\xc3\xa9\xff\xf0\x9f\x98\x80\r\n\xff");
        buffer.checkpoint_current(saved.snapshot).unwrap();
        assert!(buffer.undo());
        let (saved, changed) = f.save(f.request(&buffer, false), plan(encoding, true));
        assert!(!changed);
        assert_eq!(unzip(&fs::read(f.path()).unwrap()), bytes);
        buffer.checkpoint_current(saved.snapshot).unwrap();
    }
}

#[test]
fn utf32_gzip_malformed_conversion_refuses_before_destination_replace() {
    for encoding in [Encoding::UTF32_LE, Encoding::UTF32_BE] {
        let f = Fixture::new();
        let bytes = utf32_gzip_bytes(encoding, &[65, 0xd800], &[0xaa]);
        let original = gzip(&bytes);
        let buffer = f.open(&original, encoding, 4);
        let error = save_compressed_snapshot(
            f.request(&buffer, false),
            plan(Encoding::UTF8, false),
            &f.root,
            &AtomicBool::new(false),
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("malformed"));
        assert_eq!(fs::read(f.path()).unwrap(), original);
    }
}
