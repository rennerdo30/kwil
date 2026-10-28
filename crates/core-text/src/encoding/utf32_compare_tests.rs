#[test]
fn utf32_native_one_byte_build_preserves_crlf_atoms_tails_and_distinct_codec_ids() {
    use crate::encoding::{Encoding, utf32_tests::raw};
    for (encoding, key) in [(Encoding::UTF32_LE, 39), (Encoding::UTF32_BE, 40)] {
        let bytes = raw(encoding, &[65, 13, 10, 0x1f600, 0xd800], true, &[0xaa, 0xbb, 0xcc]);
        // decoded_native uses ReadWork::new(1,1), including BOM/decode prepayment.
        let (mut store, path) = decoded_native(&bytes, encoding, 4);
        let mut logical = [0; 9];
        assert_eq!(payload(&mut store, PayloadSpace::Logical, 0, &mut logical), 9);
        assert_eq!(&logical, b"A\r\n\xf0\x9f\x98\x80\xff\xff");
        assert_eq!(line(&store, 0), LineSpan { start: 0, content_end: 1, end: 3, terminator: Terminator::CrLf });
        for (at, span, kind, original) in [
            (3, (3, 7), CaptureKind::Preserved, raw(encoding, &[0x1f600], false, &[])),
            (7, (7, 8), CaptureKind::Opaque, raw(encoding, &[0xd800], false, &[])),
            (8, (8, 9), CaptureKind::Opaque, vec![0xaa, 0xbb, 0xcc]),
        ] {
            let atom = ready(store.atom_span(&mut AtomLookup::new(at), &mut work(), &cancel())).unwrap();
            assert_eq!((atom.start, atom.end), span);
            assert_eq!(atom.kind, kind);
            assert_eq!(atom.encoding, key);
            let mut actual = vec![0; original.len()];
            payload(&mut store, PayloadSpace::Original, atom.original_start, &mut actual);
            assert_eq!(actual, original);
        }
        retire(store.into_retirement());
        fs::remove_file(path).unwrap();
        let marker_as_text = raw(encoding, &[0xfeff, 65], false, &[]);
        let (mut store, path) = decoded_native(&marker_as_text, encoding, 0);
        let mut logical = [0; 4];
        payload(&mut store, PayloadSpace::Logical, 0, &mut logical);
        assert_eq!(&logical, b"\xef\xbb\xbfA");
        retire(store.into_retirement());
        fs::remove_file(path).unwrap();
    }
}
