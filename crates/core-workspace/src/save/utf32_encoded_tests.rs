fn utf32_encoded_bytes(encoding: Encoding, units: &[u32], tail: &[u8]) -> Vec<u8> {
    let mut bytes = if encoding == Encoding::UTF32_LE { vec![0xff, 0xfe, 0, 0] } else { vec![0, 0, 0xfe, 0xff] };
    for unit in units {
        bytes.extend_from_slice(&if encoding == Encoding::UTF32_LE { unit.to_le_bytes() } else { unit.to_be_bytes() });
    }
    bytes.extend_from_slice(tail);
    bytes
}

#[test]
fn utf32_atomic_checkpoint_keeps_four_byte_bom_malformed_units_and_undo() {
    for encoding in [Encoding::UTF32_LE, Encoding::UTF32_BE] {
        let f = Fixture::new();
        let bytes = utf32_encoded_bytes(encoding, &[65, 0xd800, 0x1f600, 13, 10], &[0xaa, 0xbb]);
        let mut buffer = f.decoded(&bytes, encoding, 4);
        buffer.insert(1, "é".as_bytes()).unwrap();
        let (saved, changed) = f.save(f.request(&buffer, false), plan(encoding, true));
        assert!(!changed);
        assert!(!saved.normalized);
        assert_eq!(
            fs::read(f.path()).unwrap(),
            utf32_encoded_bytes(encoding, &[65, 0xe9, 0xd800, 0x1f600, 13, 10], &[0xaa, 0xbb])
        );
        assert_eq!(logical(&saved.snapshot), b"A\xc3\xa9\xff\xf0\x9f\x98\x80\r\n\xff");
        buffer.checkpoint_current(saved.snapshot).unwrap();
        assert!(buffer.undo());
        let (saved, changed) = f.save(f.request(&buffer, false), plan(encoding, true));
        assert!(!changed);
        assert_eq!(fs::read(f.path()).unwrap(), bytes);
        buffer.checkpoint_current(saved.snapshot).unwrap();
        assert!(buffer.redo());
        assert_eq!(logical(&buffer.snapshot()), b"A\xc3\xa9\xff\xf0\x9f\x98\x80\r\n\xff");
    }
}

#[test]
fn utf32_tail_append_marks_logical_normalization_and_installs_one_undo_step() {
    for encoding in [Encoding::UTF32_LE, Encoding::UTF32_BE] {
        let f = Fixture::new();
        let original = utf32_encoded_bytes(encoding, &[65], &[0xaa]);
        let mut buffer = f.decoded(&original, encoding, 4);
        buffer.insert(buffer.len(), b"B").unwrap();
        let before = logical(&buffer.snapshot());
        let (saved, changed) = f.save(f.request(&buffer, false), plan(encoding, true));
        let mut expected = original;
        expected.extend_from_slice(&if encoding == Encoding::UTF32_LE {
            66_u32.to_le_bytes()
        } else {
            66_u32.to_be_bytes()
        });
        assert_eq!(fs::read(f.path()).unwrap(), expected, "short tail must not be padded or discarded");
        assert!(changed);
        assert!(saved.normalized);
        let normalized =
            if encoding == Encoding::UTF32_LE { b"A\xe4\x8a\xaa\xff".as_slice() } else { b"A\xff\xff".as_slice() };
        assert_eq!(logical(&saved.snapshot), normalized);
        buffer.install_worker_edit(saved.snapshot).unwrap();
        assert_eq!(logical(&buffer.snapshot()), normalized);
        assert!(buffer.undo());
        assert_eq!(logical(&buffer.snapshot()), before);
        assert!(buffer.redo());
        assert_eq!(logical(&buffer.snapshot()), normalized);
    }
}

#[test]
fn utf32_atomic_malformed_conversion_and_index_cancellation_keep_destination() {
    for encoding in [Encoding::UTF32_LE, Encoding::UTF32_BE] {
        let f = Fixture::new();
        fs::write(f.path(), b"unchanged destination").unwrap();
        let buffer = f.decoded(&utf32_encoded_bytes(encoding, &[65, 0xd800], &[0xaa]), encoding, 4);
        let error = save_encoded_snapshot(
            f.request(&buffer, false),
            plan(Encoding::UTF8, false),
            &f.root,
            &AtomicBool::new(false),
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("malformed"));
        assert_eq!(fs::read(f.path()).unwrap(), b"unchanged destination");
        let error = save_encoded_with_index_cancellation(
            f.request(&buffer, false),
            plan(encoding, true),
            &f.root,
            &AtomicBool::new(false),
            |_, _, _| panic!("cancelled checkpoint must precede replacement"),
            || true,
        )
        .err()
        .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(fs::read(f.path()).unwrap(), b"unchanged destination");
        assert_eq!(f.private_directories(), 1, "only the live input backing remains");
    }
}
