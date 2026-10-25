//! Small fixtures with an independent fixed-width byte oracle. NOT runtime evidence.
use super::*;
use crate::Buffer;
use crate::decoded_backing::{DecodedBacking, DecodedSnapshot, Interpretation};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize};

pub(crate) fn raw(encoding: Encoding, units: &[u32], bom: bool, tail: &[u8]) -> Vec<u8> {
    let mut bytes = if bom {
        if encoding == Encoding::UTF32_LE { vec![0xff, 0xfe, 0, 0] } else { vec![0, 0, 0xfe, 0xff] }
    } else {
        Vec::new()
    };
    for unit in units {
        bytes.extend_from_slice(&if encoding == Encoding::UTF32_LE { unit.to_le_bytes() } else { unit.to_be_bytes() });
    }
    bytes.extend_from_slice(tail);
    bytes
}

pub(crate) struct Fixture {
    pub(crate) directory: PathBuf,
    next: Cell<usize>,
    owners: RefCell<Vec<DecodedBacking>>,
}
impl Fixture {
    pub(crate) fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let directory = std::env::temp_dir().join(format!(
            "kwil-utf32-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        Self { directory, next: Cell::new(0), owners: RefCell::new(Vec::new()) }
    }
    pub(crate) fn file(&self, bytes: &[u8]) -> TextFile {
        let id = self.next.get();
        self.next.set(id + 1);
        let path = self.directory.join(format!("source-{id}"));
        std::fs::write(&path, bytes).unwrap();
        TextFile::open(&path).unwrap()
    }
    pub(crate) fn origin(&self, bytes: &[u8], encoding: Encoding, bom: usize) -> DecodedSnapshot {
        let owner = DecodedBacking::prepare(
            &self.file(bytes),
            Interpretation::new(encoding, bom).unwrap(),
            &self.directory,
            &AtomicBool::new(false),
        )
        .unwrap();
        let snapshot = owner.snapshot();
        self.owners.borrow_mut().push(owner);
        snapshot
    }
    pub(crate) fn buffer(&self, bytes: &[u8], encoding: Encoding, bom: usize) -> Buffer {
        Buffer::from_decoded(self.origin(bytes, encoding, bom))
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for owner in self.owners.get_mut().drain(..) {
            assert!(owner.try_retire().is_ok(), "UTF32 fixture still has live origin pins");
        }
        std::fs::remove_dir_all(&self.directory).unwrap();
    }
}

#[test]
fn utf32_labels_boms_binary_classification_and_utf16_override() {
    let fixture = Fixture::new();
    for (label, encoding, marker) in [
        (" \tUtF-32lE\r\n", Encoding::UTF32_LE, [0xff, 0xfe, 0, 0]),
        ("UTF-32BE", Encoding::UTF32_BE, [0, 0, 0xfe, 0xff]),
    ] {
        assert_eq!(Encoding::for_label(label).unwrap(), encoding);
        assert_eq!(Encoding::for_label(encoding.name()).unwrap(), encoding);
        assert_eq!(encoding.bom_bytes(), marker);
        for units in [&[][..], &[0, 65, 0x1f600][..]] {
            let bytes = raw(encoding, units, true, &[]);
            let file = fixture.file(&bytes);
            assert_eq!(file.kind().unwrap(), crate::FileKind::Text);
            let source = EncodedFile::new(file).unwrap();
            assert_eq!(source.detection().encoding, encoding);
            assert_eq!(source.detection().bom_len, 4);
        }
        let source = EncodedFile::new_with_encoding(fixture.file(&raw(encoding, &[65], false, &[])), encoding).unwrap();
        assert_eq!(source.detection().bom_len, 0);
    }
    let source = EncodedFile::new_with_encoding(fixture.file(b"\xff\xfe\0\0A\0"), Encoding::UTF16_LE).unwrap();
    assert_eq!(source.detection().bom_len, 2);
    let view = source.decode_range(0..6).unwrap();
    assert_eq!(view.segments, vec![DisplaySegment::Text(Cow::Borrowed("\0A"))]);
    assert!(Encoding::for_label("utf-32").is_err());
    assert!(Encoding::for_label("iso-2022-jp").is_err());
}

#[test]
fn utf32_decoder_every_split_output_exhaustion_and_malformed_ownership() {
    for encoding in [Encoding::UTF32_LE, Encoding::UTF32_BE] {
        let input = raw(encoding, &[65, 0, 0xe9, 0x1f600, 0x10ffff, 0xfffe, 0xffff, 0xfeff], false, &[]);
        let expected = "A\0é😀\u{10ffff}\u{fffe}\u{ffff}\u{feff}".as_bytes();
        for split in 0..=input.len() {
            let mut decoder = encoding.decoder();
            let mut decoded = Vec::new();
            for (bytes, last) in [(&input[..split], false), (&input[split..], true)] {
                let mut at = 0;
                loop {
                    let mut output = [0; 4];
                    let (result, read, written) =
                        decoder.decode_to_utf8_without_replacement(&bytes[at..], &mut output, last);
                    decoded.extend_from_slice(&output[..written]);
                    at += read;
                    match result {
                        DecoderResult::InputEmpty => break,
                        DecoderResult::OutputFull => assert!(read != 0 || written != 0),
                        other => panic!("valid UTF32 split {split}: {other:?}"),
                    }
                }
                assert_eq!(at, bytes.len());
            }
            assert!(decoder.is_neutral());
            assert_eq!(decoded, expected, "{encoding:?} split {split}");
        }
        let astral = raw(encoding, &[0x1f600], false, &[]);
        let mut decoder = encoding.decoder();
        assert_eq!(
            decoder.decode_to_utf8_without_replacement(&astral, &mut [0; 3], false),
            (DecoderResult::OutputFull, 4, 0)
        );
        assert!(!decoder.is_neutral());
        assert_eq!(decoder.decode_to_utf8_without_replacement(b"", &mut [], true), (DecoderResult::OutputFull, 0, 0));
        let mut out = [0; 4];
        assert_eq!(decoder.decode_to_utf8_without_replacement(b"", &mut out, true), (DecoderResult::InputEmpty, 0, 4));
        assert_eq!(&out, "😀".as_bytes());
        for invalid in [0xd800, 0xdfff, 0x110000, 0xffffffff] {
            let mut decoder = encoding.decoder();
            let bytes = raw(encoding, &[invalid], false, &[]);
            assert_eq!(
                decoder.decode_to_utf8_without_replacement(&bytes, &mut out, true),
                (DecoderResult::Malformed(4, 0), 4, 0)
            );
            assert!(decoder.is_neutral());
        }
        for len in 1..=3 {
            let mut decoder = encoding.decoder();
            assert_eq!(
                decoder.decode_to_utf8_without_replacement(&[0xaa; 3][..len], &mut out, false),
                (DecoderResult::InputEmpty, len, 0)
            );
            assert_eq!(
                decoder.decode_to_utf8_without_replacement(b"", &mut out, true),
                (DecoderResult::Malformed(len as u8, 0), 0, 0)
            );
            assert!(decoder.is_neutral());
        }
    }
}

#[test]
fn utf32_encoder_small_output_never_partially_consumes_scalar() {
    for encoding in [Encoding::UTF32_LE, Encoding::UTF32_BE] {
        let mut encoder = encoding.encoder();
        let mut small = [0x55; 3];
        assert_eq!(
            encoder.encode_from_utf8_without_replacement("😀", &mut small, false),
            (EncoderResult::OutputFull, 0, 0)
        );
        assert_eq!(small, [0x55; 3]);
        let mut output = [0; 4];
        assert_eq!(
            encoder.encode_from_utf8_without_replacement("😀", &mut output, true),
            (EncoderResult::InputEmpty, 4, 4)
        );
        assert_eq!(output.as_slice(), raw(encoding, &[0x1f600], false, &[]));
    }
}

#[test]
fn utf32_original_byte_index_and_viewport_ignore_newline_bytes_inside_units() {
    let fixture = Fixture::new();
    for encoding in [Encoding::UTF32_LE, Encoding::UTF32_BE] {
        // 0A/0D inside non-newline scalars and invalid units must not count.
        let bytes = raw(encoding, &[65, 0x0a00, 0x1000d, 0xd80a, 13, 10, 66, 13, 67, 10], true, &[0x0a]);
        let source = EncodedFile::new(fixture.file(&bytes)).unwrap();
        assert_eq!(source.detection().endings, EndingCounts { lf: 1, crlf: 1, cr: 1 });
        source.build_index().unwrap();
        assert_eq!(source.index().unwrap().line_count(), 4);
        for (line, at) in [4, 28, 36, 44].into_iter().enumerate() {
            assert_eq!(source.line_start(line as u64).unwrap(), Some(at));
        }
        assert_eq!(source.line_start(4).unwrap(), None);
        let view = source.decode_range(9..11).unwrap();
        assert_eq!(view.source, 8..12);
        assert!(!view.had_errors);
        let view = source.decode_range(16..20).unwrap();
        assert!(view.had_errors);
        assert_eq!(view.segments, vec![DisplaySegment::Invalid(&bytes[16..20])]);
        let view = source.decode_range(44..45).unwrap();
        assert_eq!(view.segments, vec![DisplaySegment::Invalid(&bytes[44..45])]);
    }
}

#[test]
fn utf32_original_piece_conversion_carries_units_but_finalizes_before_added_text() {
    let fixture = Fixture::new();
    for encoding in [Encoding::UTF32_LE, Encoding::UTF32_BE] {
        let bytes = raw(encoding, &[0x1f600, 90], false, &[]);
        for split in 1..4 {
            let mut interrupted = bytes.clone();
            interrupted.insert(split, b'x');
            let file = fixture.file(&interrupted);
            let source = EncodedFile::new_with_encoding(file.clone(), encoding).unwrap();
            let mut buffer = Buffer::from_file(file).unwrap();
            buffer.delete(split as u64..split as u64 + 1).unwrap();
            let target = source.save_plan().convert_to(Encoding::UTF8, false);
            let mut saved = Vec::new();
            source.write_to(&buffer.snapshot(), target, &mut saved).unwrap();
            assert_eq!(saved, "😀Z".as_bytes());
            buffer.insert(split as u64, b"!").unwrap();
            let mut saved = b"untouched".to_vec();
            assert!(matches!(
                source.write_to(&buffer.snapshot(), target, &mut saved),
                Err(EncodingError::MalformedSource)
            ));
            assert_eq!(saved, b"untouched");
        }
    }
}

#[test]
fn utf32_original_index_keeps_crlf_together_at_the_byte_checkpoint() {
    let fixture = Fixture::new();
    for encoding in [Encoding::UTF32_LE, Encoding::UTF32_BE] {
        let mut units = vec![65; STRIDE / 4 - 1];
        units.extend_from_slice(&[13, 10, 66, 10]);
        let bytes = raw(encoding, &units, true, &[]);
        let source = EncodedFile::new(fixture.file(&bytes)).unwrap();
        source.build_index().unwrap();
        assert_eq!(source.line_start(1).unwrap(), Some((STRIDE + 8) as u64));
        assert_eq!(source.line_start(2).unwrap(), Some(bytes.len() as u64));
        let index = source.index().unwrap();
        assert_eq!(index.line_count(), 3);
        assert_eq!(index.checkpoints[1].offset, (STRIDE + 8) as u64);
        assert_eq!(index.checkpoints[1].line, 1);
        assert_eq!(
            source.decode_lines(1, 1).unwrap().unwrap().segments,
            vec![DisplaySegment::Text(Cow::Borrowed("B\n"))]
        );
    }
}

#[test]
fn utf32_raw_save_endings_split_crlf_and_boundary_refusal() {
    let fixture = Fixture::new();
    for encoding in [Encoding::UTF32_LE, Encoding::UTF32_BE] {
        let bytes = raw(encoding, &[65, 13, 10, 0xd80a, 66, 13, 67, 10], true, &[0xff]);
        let file = fixture.file(&bytes);
        let source = EncodedFile::new(file.clone()).unwrap();
        let mut buffer = Buffer::from_file(file).unwrap();
        buffer.insert(12, b"x").unwrap(); // split between CR and LF without changing source bytes
        buffer.delete(12..13).unwrap();
        let mut saved = Vec::new();
        source.write_to(&buffer.snapshot(), source.save_plan(), &mut saved).unwrap();
        assert_eq!(saved, bytes);
        saved.clear();
        source
            .write_to(&buffer.snapshot(), source.save_plan().change_line_endings(LineEnding::Lf), &mut saved)
            .unwrap();
        assert_eq!(saved, raw(encoding, &[65, 10, 0xd80a, 66, 10, 67, 10], true, &[0xff]));
        buffer.delete(5..6).unwrap();
        let mut saved = b"untouched".to_vec();
        assert!(matches!(
            source.write_to(&buffer.snapshot(), source.save_plan(), &mut saved),
            Err(EncodingError::SplitCharacter(_))
        ));
        assert_eq!(saved, b"untouched");
    }
}

#[test]
fn utf32_original_line_ending_edits_keep_byte_ranges_and_one_undo_transaction() {
    let fixture = Fixture::new();
    for encoding in [Encoding::UTF32_LE, Encoding::UTF32_BE] {
        let bytes = raw(encoding, &[65, 13, 10, 0x0a00, 66, 13, 67, 10], true, &[]);
        let source = EncodedFile::new(fixture.file(&bytes)).unwrap();
        let mut buffer = Buffer::from_file(source.file.clone()).unwrap();
        buffer.insert(buffer.len(), b"D\r\n").unwrap();
        let before = buffer.snapshot();
        buffer.begin();
        let mut delta = 0i128;
        let mut changes = 0;
        source
            .plan_line_ending_edits(&before, LineEnding::Lf, |edit| {
                let start = (edit.range.start as i128 + delta) as u64;
                let end = (edit.range.end as i128 + delta) as u64;
                buffer.delete(start..end)?;
                buffer.insert(start, edit.replacement.as_bytes())?;
                delta += edit.replacement.len() as i128 - (edit.range.end - edit.range.start) as i128;
                changes += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(changes, 3);
        buffer.commit();
        let mut saved = Vec::new();
        source.write_to(&buffer.snapshot(), source.save_plan(), &mut saved).unwrap();
        assert_eq!(saved, raw(encoding, &[65, 10, 0x0a00, 66, 10, 67, 10, 68, 10], true, &[]));
        assert!(buffer.undo());
        saved.clear();
        source.write_to(&buffer.snapshot(), source.save_plan(), &mut saved).unwrap();
        let mut expected = bytes;
        expected.extend_from_slice(&raw(encoding, &[68, 13, 10], false, &[]));
        assert_eq!(saved, expected);
    }
}
