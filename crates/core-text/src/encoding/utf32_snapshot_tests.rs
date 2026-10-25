#[test]
fn utf32_logical_save_import_undo_retirement_and_exact_tail_bytes() {
    use crate::encoding::utf32_tests::raw;
    for encoding in [Encoding::UTF32_LE, Encoding::UTF32_BE] {
        let fixture = Fixture::new();
        let bytes = raw(encoding, &[65, 0x1f600, 0xd800, 66], true, &[0xaa, 0xbb, 0xcc]);
        let mut source = fixture.decoded(&bytes, encoding, 4);
        let before = source.snapshot();
        source.insert(1, "é".as_bytes()).unwrap();
        assert_eq!(
            saved(&source.snapshot(), plan(encoding, true)),
            raw(encoding, &[65, 0xe9, 0x1f600, 0xd800, 66], true, &[0xaa, 0xbb, 0xcc])
        );
        assert!(source.undo());
        assert_eq!(saved(&source.snapshot(), plan(encoding, true)), bytes);
        assert!(source.redo());
        let mut target = Buffer::with_add_memory_limit(0);
        target.set_undo_memory_limit(0);
        target.insert_snapshot_range(0, &source.snapshot(), 0..source.len()).unwrap();
        drop((source, before));
        let owner = fixture.owners.borrow_mut().pop().unwrap();
        assert!(owner.try_retire().is_ok(), "ordinary UTF32 import must own its raw atoms");
        assert_eq!(target.add_memory_bytes(), 0);
        assert_eq!(
            saved(&target.snapshot(), plan(encoding, true)),
            raw(encoding, &[65, 0xe9, 0x1f600, 0xd800, 66], true, &[0xaa, 0xbb, 0xcc])
        );
        let snapshot = target.snapshot();
        let mut writer = Untouched::default();
        assert!(matches!(
            write_snapshot(&snapshot, plan(Encoding::UTF8, false), &mut writer),
            Err(EncodingError::MalformedSource)
        ));
        assert_eq!(writer.calls, 0);
        // The astral scalar is four logical bytes; its interior is not a raw endpoint.
        let split = snapshot.slice(4..6).unwrap();
        let mut writer = Untouched::default();
        assert!(matches!(
            write_snapshot(&split, plan(encoding, true), &mut writer),
            Err(EncodingError::SplitCharacter(_)) | Err(EncodingError::InvalidUtf8)
        ));
        assert_eq!(writer.calls, 0);
    }
}

#[test]
fn utf32_logical_save_mixed_origins_and_added_crlf_preserve_invalid_units() {
    use crate::encoding::utf32_tests::raw;
    let fixture = Fixture::new();
    for encoding in [Encoding::UTF32_LE, Encoding::UTF32_BE] {
        let mut left = fixture.decoded(&raw(encoding, &[65, 13], true, &[]), encoding, 4);
        let right = fixture.decoded(&raw(encoding, &[10, 0xd80a, 66, 13, 67, 10], false, &[]), encoding, 0);
        left.insert_snapshot_range(left.len(), &right.snapshot(), 0..right.len()).unwrap();
        left.insert(left.len(), b"\r").unwrap();
        left.insert(left.len(), b"\n").unwrap();
        assert_eq!(
            saved(&left.snapshot(), plan(encoding, true)),
            raw(encoding, &[65, 13, 10, 0xd80a, 66, 13, 67, 10, 13, 10], true, &[])
        );
        for ending in [LineEnding::Lf, LineEnding::CrLf, LineEnding::Cr] {
            let normalized: Vec<u32> = [
                vec![65],
                ending.text().chars().map(|c| c as u32).collect(),
                vec![0xd80a, 66],
                ending.text().chars().map(|c| c as u32).collect(),
                vec![67],
                ending.text().chars().map(|c| c as u32).collect(),
                ending.text().chars().map(|c| c as u32).collect(),
            ]
            .concat();
            assert_eq!(
                saved(&left.snapshot(), plan(encoding, true).change_line_endings(ending)),
                raw(encoding, &normalized, true, &[])
            );
        }
    }
}

#[test]
fn utf32_logical_conversion_and_cancellation_leave_writer_untouched() {
    use crate::encoding::utf32_tests::raw;
    let fixture = Fixture::new();
    for encoding in [Encoding::UTF32_LE, Encoding::UTF32_BE] {
        let source = fixture.decoded(&raw(encoding, &[65, 0xe9, 0x1f600], true, &[]), encoding, 4);
        assert_eq!(saved(&source.snapshot(), plan(Encoding::UTF8, false)), "Aé😀".as_bytes());
        assert_eq!(saved(&source.snapshot(), plan(Encoding::UTF16_BE, false)), b"\0A\0\xe9\xd8\x3d\xde\0");
        let other = if encoding == Encoding::UTF32_LE { Encoding::UTF32_BE } else { Encoding::UTF32_LE };
        assert_eq!(saved(&source.snapshot(), plan(other, true)), raw(other, &[65, 0xe9, 0x1f600], true, &[]));
        let mut writer = Untouched::default();
        assert!(matches!(
            write_snapshot(&source.snapshot(), plan(Encoding::for_label("windows-1252").unwrap(), false), &mut writer),
            Err(EncodingError::Unmappable { .. })
        ));
        assert_eq!(writer.calls, 0);
        let mut writer = Untouched::default();
        assert!(matches!(
            write_snapshot_cancellable(&source.snapshot(), plan(encoding, true), &mut writer, || true),
            Err(EncodingError::Cancelled)
        ));
        assert_eq!(writer.calls, 0);
        let mut empty = Buffer::empty();
        empty.insert(0, "Aé😀".as_bytes()).unwrap();
        assert_eq!(saved(&empty.snapshot(), plan(encoding, true)), raw(encoding, &[65, 0xe9, 0x1f600], true, &[]));
    }
}

#[test]
fn utf32_snapshot_long_preflight_and_output_are_bounded_and_cancellable() {
    use crate::encoding::utf32_tests::raw;
    let fixture = Fixture::new();
    let bytes = raw(Encoding::UTF32_BE, &vec![0xd800; 20_000], true, &[]);
    let source = fixture.decoded(&bytes, Encoding::UTF32_BE, 4);
    let mut polls = 0;
    let mut writer = Untouched::default();
    assert!(matches!(
        write_snapshot_cancellable(
            &source.snapshot(),
            plan(Encoding::UTF32_BE, true).change_line_endings(LineEnding::Lf),
            &mut writer,
            || {
                polls += 1;
                polls > 8
            }
        ),
        Err(EncodingError::Cancelled)
    ));
    assert_eq!(writer.calls, 0, "cancelled full preflight must not reach destination");
    struct Bounded {
        bytes: usize,
    }
    impl Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            assert!(bytes.len() <= 64 << 10);
            self.bytes += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Bounded { bytes: 0 };
    write_snapshot(&source.snapshot(), plan(Encoding::UTF32_BE, true), &mut writer).unwrap();
    assert_eq!(writer.bytes, bytes.len());
    let cancel = Cell::new(false);
    struct Cancelling<'a>(&'a Cell<bool>);
    impl Write for Cancelling<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.set(true);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    assert!(matches!(
        write_snapshot_cancellable(
            &source.snapshot(),
            plan(Encoding::UTF32_BE, true),
            &mut Cancelling(&cancel),
            || cancel.get()
        ),
        Err(EncodingError::Cancelled)
    ));
}
