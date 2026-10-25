#[test]
fn utf32_capsule_serialization_keeps_canonical_label_four_byte_bom_and_raw_tail() {
    for (encoding, raw) in [
        (Encoding::UTF32_LE, b"\xff\xfe\0\0A\0\0\0\0\xd8\0\0\xaa\xbb\xcc".as_slice()),
        (Encoding::UTF32_BE, b"\0\0\xfe\xff\0\0\0A\0\0\xd8\0\xaa\xbb\xcc".as_slice()),
    ] {
        let fixture = Fixture::new();
        let buffer = fixture.decoded(raw, encoding, 4);
        for width in [1, 3, 17] {
            let registry = registry();
            let snapshot = buffer.snapshot();
            let (bytes, token) = serialize(&registry, plan(&registry, &snapshot), width);
            let (logical, records) = inspect(&bytes, &[]);
            assert_eq!(logical, 3);
            assert!(records.iter().any(|record| matches!(record, Record::Definition { encoding: label, bom: 4, raw: original, .. } if label == encoding.name() && original == raw)));
            assert!(records.iter().any(|record| matches!(record, Record::Reference { range, .. } if *range == (0..3))));
            drop(token);
        }
    }
}
