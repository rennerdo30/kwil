#[test]
fn utf32_replay_origins_detached_malformed_tail_and_failed_revision_are_lossless() {
    for (encoding, original, malformed) in [
        (
            Encoding::UTF32_LE,
            b"\xff\xfe\0\0A\0\0\0\0\xf6\x01\0\0\xd8\0\0B\0\0\0\xaa\xbb\xcc".as_slice(),
            b"\0\xd8\0\0".as_slice(),
        ),
        (
            Encoding::UTF32_BE,
            b"\0\0\xfe\xff\0\0\0A\0\x01\xf6\0\0\0\xd8\0\0\0\0B\xaa\xbb\xcc".as_slice(),
            b"\0\0\xd8\0".as_slice(),
        ),
    ] {
        let fixture = Fixture::new();
        let source = Buffer::from_decoded(fixture.origin(original, encoding, 4));
        let mut foreign = Buffer::empty();
        foreign.insert_snapshot_range(0, &source.snapshot(), 0..source.len()).unwrap();
        let detached =
            foreign.snapshot().slice(5..6).unwrap().concat(&foreign.snapshot().slice(7..8).unwrap()).unwrap();
        let mixed = source.snapshot().concat(&detached).unwrap();
        let mut registry = registry();
        let packed = capsule(&mut registry, mixed.clone(), 0..mixed.len());
        let mut state = fixture.state();
        accepted(&mut state, &packed, 0..0);
        let mut expected = original.to_vec();
        expected.extend_from_slice(malformed);
        expected.extend_from_slice(&[0xaa, 0xbb, 0xcc]);
        assert_eq!(saved(&state.snapshot().unwrap(), encoding, true), expected);
        assert!(
            saved(&state.snapshot().unwrap().slice(0..5).unwrap(), Encoding::UTF8, false).starts_with("A😀".as_bytes())
        );
        state.abandon().unwrap();
        fixture.assert_empty();
        let mut state = fixture.state();
        let cancel = AtomicBool::new(false);
        let mut revision = state.begin_revision(&cancel).unwrap();
        let mut truncated = packed.bytes.clone();
        truncated.pop();
        assert!(
            revision.apply_capsule(&mut Cursor::new(&truncated), truncated.len() as u64, packed.logical, 0..0).is_err()
        );
        assert!(revision.commit().is_err());
        assert_eq!(state.snapshot().unwrap().len(), 0);
        state.abandon().unwrap();
        fixture.assert_empty();
    }
}
