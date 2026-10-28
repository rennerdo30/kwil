fn utf32_journal_bytes(encoding: Encoding, units: &[u32], tail: &[u8]) -> Vec<u8> {
    let mut bytes = if encoding == Encoding::UTF32_LE { vec![0xff, 0xfe, 0, 0] } else { vec![0, 0, 0xfe, 0xff] };
    for unit in units {
        bytes.extend_from_slice(&if encoding == Encoding::UTF32_LE { unit.to_le_bytes() } else { unit.to_be_bytes() });
    }
    bytes.extend_from_slice(tail);
    bytes
}

#[test]
fn utf32_journal_policy_bom_compression_and_all_endings_roundtrip() {
    for encoding in [Encoding::UTF32_LE, Encoding::UTF32_BE] {
        for preferred_ending in [LineEnding::Lf, LineEnding::CrLf, LineEnding::Cr] {
            for compression in [
                kwil_core_workspace::compressed::CompressionFormat::Raw,
                kwil_core_workspace::compressed::CompressionFormat::Gzip,
            ] {
                let policy = RecoveryPolicy { raw_bytes: false, encoding, bom: true, preferred_ending, compression };
                let tag = policy.to_codec().unwrap();
                assert_eq!(RecoveryPolicy::from_codec(&tag).unwrap(), policy);
                assert_eq!(&tag[8..], encoding.name().as_bytes());
                let raw_policy = RecoveryPolicy { raw_bytes: true, ..policy };
                assert!(raw_policy.to_codec().is_err(), "raw transport must not gain text transforms");
            }
        }
    }
}

#[test]
fn utf32_authenticated_journal_recovers_edit_and_exact_invalid_tail_provenance() {
    for encoding in [Encoding::UTF32_LE, Encoding::UTF32_BE] {
        let fixture = Fixture::new();
        let original = utf32_journal_bytes(encoding, &[65, 0xd800, 0x1f600, 13, 10], &[0xaa, 0xbb, 0xcc]);
        let mut registry = registry();
        let base = seed(&mut registry, fixture.original(&original, encoding, 4));
        let policy = RecoveryPolicy {
            raw_bytes: false,
            encoding,
            bom: true,
            preferred_ending: LineEnding::CrLf,
            compression: kwil_core_workspace::compressed::CompressionFormat::Raw,
        };
        let insertion = text(&mut registry, "é".as_bytes());
        fixture.write(&base, policy, &[EditBatch { from: 9, to: 10, parts: vec![part(1, 0, insertion)] }]);
        let mut state = fixture.state();
        state.recover(&mut fixture.reader(), &AtomicBool::new(false)).unwrap();
        assert_eq!(state.summary().unwrap().policy, policy);
        let buffer = state.take_buffer(&AtomicBool::new(false)).unwrap();
        assert_eq!(
            save(&buffer.snapshot(), encoding, true),
            utf32_journal_bytes(encoding, &[65, 0xe9, 0xd800, 0x1f600, 13, 10], &[0xaa, 0xbb, 0xcc])
        );
        assert!(save(&buffer.snapshot().slice(0..3).unwrap(), Encoding::UTF8, false).starts_with("Aé".as_bytes()));
        drop(buffer);
        state.abandon().unwrap();
        assert_eq!(fixture.count(), 0);
    }
}
