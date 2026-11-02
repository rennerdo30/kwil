use super::*;
use crate::Buffer;
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;

static NEXT: AtomicUsize = AtomicUsize::new(0);
struct TempDir(PathBuf);
impl TempDir {
    fn new() -> Self {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
        std::fs::create_dir_all(&base).unwrap();
        let root = base.join(format!("kwil-encoding-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir(&root).unwrap();
        Self(root)
    }
    fn open(&self, name: &str, bytes: &[u8]) -> TextFile {
        let path = self.0.join(name);
        std::fs::write(&path, bytes).unwrap();
        TextFile::open(&path).unwrap()
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

fn encode(text: &str, encoding: Encoding, bom: bool) -> Vec<u8> {
    let mut out = Vec::new();
    if bom {
        out.extend_from_slice(encoding.bom_bytes());
    }
    if encoding.utf16() {
        for unit in text.encode_utf16() {
            out.extend_from_slice(&if encoding == Encoding::UTF16_LE {
                unit.to_le_bytes()
            } else {
                unit.to_be_bytes()
            });
        }
    } else {
        let Codec::Rs(codec) = encoding.0 else { panic!("encoding_rs fixture oracle only") };
        let (bytes, _, errors) = codec.encode(text);
        assert!(!errors);
        out.extend_from_slice(&bytes);
    }
    out
}

#[test]
fn index_memory_limit_decode_refuses_without_publishing_and_can_retry() {
    let dir = TempDir::new();
    let file = dir.open("small-utf16", &encode("one\ntwo", Encoding::UTF16_LE, true));
    let encoded = EncodedFile::new(file).unwrap();
    let error = encoded.build_index_with_memory_limit(1).unwrap_err();
    assert!(error.to_string().contains("1-byte memory limit"));
    assert!(encoded.index().is_none());
    assert_eq!(encoded.progress().scanned, 0);
    assert_eq!(encoded.build_index_with_memory_limit(256).unwrap().line_count(), 2);
}

#[test]
fn index_memory_limit_decode_bounds_checkpoint_growth() {
    let dir = TempDir::new();
    let file = dir.open("small-legacy", &vec![b'x'; STRIDE * 5]);
    let encoded = EncodedFile::new_with_encoding(file, Encoding::for_label("windows-1252").unwrap()).unwrap();
    assert!(encoded.build_index_with_memory_limit(128).is_err());
    assert!(encoded.index().is_none());
    let index = encoded.build_index_with_memory_limit(512).unwrap();
    assert_eq!(index.line_count(), 1);
    assert!(index.allocated_bytes() <= 512);
}

#[test]
fn index_memory_limit_file_error_reaches_caller_without_partial_index() {
    let dir = TempDir::new();
    let file = dir.open("small-utf8", b"one\ntwo");
    let error = file.build_index_with_memory_limit(4, 1).unwrap_err();
    assert!(error.to_string().contains("1-byte memory limit"));
    assert!(file.index().is_none());
    assert_eq!(file.progress().scanned, 0);
    assert_eq!(file.build_index_with_memory_limit(4, 4096).unwrap().line_count(), 2);
}

fn text(range: &DecodedRange<'_>) -> String {
    range
        .segments
        .iter()
        .map(|segment| match segment {
            DisplaySegment::Text(text) => text.to_string(),
            DisplaySegment::Invalid(raw) => raw.iter().map(|byte| format!("\\x{byte:02X}")).collect(),
        })
        .collect()
}

fn roundtrip(label: &str, bom: bool, invalid: bool) {
    let dir = TempDir::new();
    let encoding = Encoding::for_label(label).unwrap();
    let content = if label == "shift_jis" {
        "日本語の文字を正しく表示します。\r\nneedle\n日本語の続き\r終わり"
    } else if label == "windows-1252" {
        "Français café — résumé €\r\nneedle\ncrème brûlée\rfin"
    } else {
        "Hello world café\r\nneedle\nThe final line\rend"
    };
    let mut bytes = encode(content, encoding, bom);
    if invalid {
        bytes.extend_from_slice(b"\ninvalid\xff\xc0\x80\xe2");
    }
    let file = dir.open("fixture", &bytes);
    let detected = EncodedFile::new(file.clone()).unwrap();
    let encoded = if invalid { detected.reinterpret(Encoding::UTF8).unwrap() } else { detected };
    assert_eq!(encoded.detection.encoding, encoding);
    assert_eq!(encoded.detection.bom_len, if bom { encoding.bom_bytes().len() } else { 0 });
    assert_eq!(encoded.detection.endings.style(), EndingStyle::Mixed);
    encoded.index_in_background(|r| r.unwrap()).join().unwrap();
    assert_eq!(encoded.progress().scanned, bytes.len() as u64);
    assert_eq!(encoded.index().unwrap().line_count(), if invalid { 5 } else { 4 });
    let mut buffer = Buffer::from_file(file).unwrap();
    let mut saved = Vec::new();
    encoded.write_to(&buffer.snapshot(), encoded.save_plan(), &mut saved).unwrap();
    assert_eq!(saved, bytes, "no-edit byte-exact roundtrip {label}");
    let needle = encode("needle", encoding, false);
    let at = bytes.windows(needle.len()).position(|window| window == needle).unwrap();
    buffer.delete(at as u64..(at + needle.len()) as u64).unwrap();
    let replacement = if label == "shift_jis" { "EDIT変更" } else { "EDIT café" };
    buffer.insert(at as u64, replacement.as_bytes()).unwrap();
    let mut expected = bytes[..at].to_vec();
    expected.extend_from_slice(&encode(replacement, encoding, false));
    expected.extend_from_slice(&bytes[at + needle.len()..]);
    saved.clear();
    encoded.write_to(&buffer.snapshot(), encoded.save_plan(), &mut saved).unwrap();
    assert_eq!(saved, expected, "only edited bytes change {label}");
}

macro_rules! fixture {
    ($name:ident, $label:literal, $bom:literal, $invalid:literal) => {
        #[test]
        fn $name() {
            roundtrip($label, $bom, $invalid);
        }
    };
}
fixture!(utf8_plain_roundtrip, "utf-8", false, false);
fixture!(utf8_bom_roundtrip, "utf-8", true, false);
fixture!(utf8_invalid_roundtrip, "utf-8", false, true);
fixture!(utf8_bom_invalid_roundtrip, "utf-8", true, true);
fixture!(utf16_le_roundtrip, "utf-16le", false, false);
fixture!(utf16_le_bom_roundtrip, "utf-16le", true, false);
fixture!(utf16_be_roundtrip, "utf-16be", false, false);
fixture!(utf16_be_bom_roundtrip, "utf-16be", true, false);
fixture!(shift_jis_roundtrip, "shift_jis", false, false);
fixture!(windows1252_roundtrip, "windows-1252", false, false);

#[test]
fn samples_are_bounded_and_ignore_artificial_utf8_edges() {
    let dir = TempDir::new();
    let mut bytes = vec![b'a'; SAMPLE * 4 + 1];
    // A real scalar crosses each artificial sample edge.
    bytes[SAMPLE - 1..SAMPLE + 2].copy_from_slice("日".as_bytes());
    let tail = bytes.len() - SAMPLE;
    bytes[tail - 1..tail + 2].copy_from_slice("本".as_bytes());
    bytes[SAMPLE * 2] = 0xff; // Outside both samples, intentionally not read.
    let source = EncodedFile::new(dir.open("sample", &bytes)).unwrap();
    assert_eq!(source.detection.encoding, Encoding::UTF8);
    assert!(source.detection.sample_is_utf8);
    assert_eq!(source.detection.sampled_bytes, SAMPLE * 2);
    let decoded = source.decode_range(SAMPLE as u64 * 2..SAMPLE as u64 * 2 + 1).unwrap_err();
    assert!(matches!(decoded, EncodingError::IndexNotReady));
    source.build_index().unwrap();
    let decoded = source.decode_range(SAMPLE as u64 * 2..SAMPLE as u64 * 2 + 1).unwrap();
    assert_eq!(text(&decoded), "\\xFF");
    assert!(decoded.had_errors);
    assert!(matches!(source.decode_range(0..MAX_VIEWPORT_BYTES as u64 + 1), Err(EncodingError::InvalidRange)));
    let empty = source.decode_range(0..0).unwrap();
    assert!(empty.segments.is_empty());
}

#[test]
fn utf8_display_borrows_and_retains_invalid_bytes() {
    let dir = TempDir::new();
    let bytes = b"\xef\xbb\xbfhello\xff\xfeok\xf0\x9f";
    let source = EncodedFile::new(dir.open("invalid", bytes)).unwrap();
    let decoded = source.decode_range(0..bytes.len() as u64).unwrap();
    assert_eq!(decoded.source, 3..bytes.len() as u64);
    assert_eq!(text(&decoded), "hello\\xFF\\xFEok\\xF0\\x9F");
    assert!(matches!(decoded.segments[0], DisplaySegment::Text(Cow::Borrowed(_))));
    let mut original = Vec::new();
    for segment in decoded.segments {
        match segment {
            DisplaySegment::Text(t) => original.extend_from_slice(t.as_bytes()),
            DisplaySegment::Invalid(b) => original.extend_from_slice(&b),
        }
    }
    assert_eq!(original, bytes[3..]);
}

#[test]
fn checkpoints_and_split_multibyte_ranges() {
    let dir = TempDir::new();
    for (i, (label, unit)) in [
        ("shift_jis", "日本語"),
        ("euc-jp", "日本語"),
        ("gbk", "中文"),
        ("big5", "中文"),
        ("gb18030", "😀中"),
        ("koi8-r", "Россия"),
        ("utf-8", "😀日"),
        ("utf-16le", "😀日"),
        ("utf-16be", "😀日"),
    ]
    .into_iter()
    .enumerate()
    {
        let encoding = Encoding::for_label(label).unwrap();
        let unit_bytes = encode(unit, encoding, false);
        let mut bytes = Vec::new();
        while bytes.len() < STRIDE * 3 + 25 {
            bytes.extend_from_slice(&unit_bytes);
        }
        let source = EncodedFile::new(dir.open(&format!("range-{i}"), &bytes)).unwrap().reinterpret(encoding).unwrap();
        source.build_index().unwrap();
        assert!(source.index().unwrap().checkpoint_count() >= 3);
        assert!(source.index().unwrap().allocated_bytes() < 2048);
        for checkpoint in &source.index().unwrap().checkpoints {
            let at = checkpoint.offset as usize;
            let end = (at + 37).min(bytes.len());
            let decoded = source.decode_range(at as u64..end as u64).unwrap();
            assert!(!decoded.had_errors, "{label} checkpoint {at}");
            let Codec::Rs(codec) = encoding.0 else { panic!("encoding_rs fixture oracle only") };
            let expected = codec
                .decode_without_bom_handling_and_without_replacement(
                    &bytes[decoded.source.start as usize..decoded.source.end as usize],
                )
                .unwrap();
            assert_eq!(text(&decoded), expected, "{label}");
        }
        // Starts and ends one byte inside a character, including surrogates.
        let middle = STRIDE / unit_bytes.len() * unit_bytes.len();
        let decoded = source.decode_range((middle + 1) as u64..(middle + unit_bytes.len() - 1) as u64).unwrap();
        assert!(!decoded.had_errors, "split range {label}");
        assert_eq!(
            encode(&text(&decoded), encoding, false),
            bytes[decoded.source.start as usize..decoded.source.end as usize]
        );
    }
}

#[test]
fn utf16_line_index_counts_units_and_mixed_breaks() {
    let dir = TempDir::new();
    for (i, encoding) in [Encoding::UTF16_LE, Encoding::UTF16_BE].into_iter().enumerate() {
        let prefix = "\u{0a00}\u{010a}😀\r\nsecond\rthird\n";
        let content = prefix.repeat(1100);
        let bytes = encode(&content, encoding, true);
        let source = EncodedFile::new(dir.open(&format!("utf16-{i}"), &bytes)).unwrap();
        source.build_index().unwrap();
        assert_eq!(source.index().unwrap().line_count(), 3301);
        assert_eq!(source.detection.endings.crlf, source.detection.endings.cr);
        assert_eq!(source.detection.endings.lf, source.detection.endings.cr);
        assert_eq!(
            source.line_start(1).unwrap(),
            Some(2 + encode("\u{0a00}\u{010a}😀\r\n", encoding, false).len() as u64)
        );
        assert_eq!(text(&source.decode_lines(3072, 3).unwrap().unwrap()), prefix);
        assert!(source.line_start(3301).unwrap().is_none());
        assert!(source.decode_lines(2, 0).unwrap().unwrap().segments.is_empty());
    }
}

#[test]
fn streaming_save_handles_added_utf8_split_across_pieces() {
    let dir = TempDir::new();
    let source = EncodedFile::new(dir.open("empty", b"\xff\xfe")).unwrap();
    let mut buffer = Buffer::from_file(source.file.clone()).unwrap();
    for &byte in "日本語😀\r\n".as_bytes() {
        buffer.insert(buffer.len(), &[byte]).unwrap();
    }
    let mut saved = Vec::new();
    source.write_to(&buffer.snapshot(), source.save_plan(), &mut saved).unwrap();
    assert_eq!(saved, encode("日本語😀\r\n", Encoding::UTF16_LE, true));
}

#[test]
fn conversion_plans_preserve_bom_and_normalize_across_chunks() {
    let dir = TempDir::new();
    let bytes = encode("café\r\n日本語\rthird\n", Encoding::UTF16_BE, true);
    let source = EncodedFile::new(dir.open("convert", &bytes)).unwrap();
    let mut buffer = Buffer::from_file(source.file.clone()).unwrap();
    buffer.insert(buffer.len(), b"extra\r").unwrap();
    buffer.insert(buffer.len(), b"\nnext\r").unwrap();
    let plan = source.save_plan().convert_to(Encoding::UTF8, true).change_line_endings(LineEnding::Lf);
    let mut saved = Vec::new();
    source.write_to(&buffer.snapshot(), plan, &mut saved).unwrap();
    assert_eq!(saved, b"\xef\xbb\xbfcaf\xc3\xa9\n\xe6\x97\xa5\xe6\x9c\xac\xe8\xaa\x9e\nthird\nextra\nnext\n");
    for ending in [LineEnding::Cr, LineEnding::CrLf] {
        saved.clear();
        let plan = plan.change_line_endings(ending).convert_to(Encoding::UTF16_LE, false);
        source.write_to(&buffer.snapshot(), plan, &mut saved).unwrap();
        assert_eq!(
            saved,
            encode(&"café\n日本語\nthird\nextra\nnext\n".replace('\n', ending.text()), Encoding::UTF16_LE, false)
        );
    }
}

#[test]
fn unmappable_and_invalid_conversion_leave_writer_untouched() {
    let dir = TempDir::new();
    let source = EncodedFile::new(dir.open("win", b"cafe"))
        .unwrap()
        .reinterpret(Encoding::for_label("windows-1252").unwrap())
        .unwrap();
    let mut buffer = Buffer::from_file(source.file.clone()).unwrap();
    buffer.insert(4, "😀".as_bytes()).unwrap();
    let mut saved = b"sentinel".to_vec();
    let error = source.write_to(&buffer.snapshot(), source.save_plan(), &mut saved).unwrap_err();
    assert!(matches!(error, EncodingError::Unmappable { character: '😀', encoding: "windows-1252" }));
    assert_eq!(saved, b"sentinel");
    let invalid = EncodedFile::new(dir.open("bad", b"\xef\xbb\xbfok\xff")).unwrap();
    let buffer = Buffer::from_file(invalid.file.clone()).unwrap();
    let error = invalid
        .write_to(&buffer.snapshot(), invalid.save_plan().convert_to(Encoding::UTF16_LE, true), &mut saved)
        .unwrap_err();
    assert!(matches!(error, EncodingError::MalformedSource));
    assert_eq!(saved, b"sentinel");
}

#[test]
fn reinterpretation_changes_display_without_modifying_source() {
    let dir = TempDir::new();
    let bytes = encode("日本語", Encoding::for_label("shift_jis").unwrap(), false);
    let source = EncodedFile::new(dir.open("override", &bytes)).unwrap().reinterpret(Encoding::UTF8).unwrap();
    assert!(source.decode_range(0..bytes.len() as u64).unwrap().had_errors);
    let sjis = source.reinterpret(Encoding::for_label("shift_jis").unwrap()).unwrap();
    assert!(sjis.index().is_none());
    assert_eq!(text(&sjis.decode_range(0..bytes.len() as u64).unwrap()), "日本語");
    let buffer = Buffer::from_file(sjis.file.clone()).unwrap();
    let mut saved = Vec::new();
    sjis.write_to(&buffer.snapshot(), sjis.save_plan(), &mut saved).unwrap();
    assert_eq!(saved, bytes);
    assert_eq!(Encoding::for_label("iso-2022-jp").unwrap(), Encoding::ISO2022_JP);
}

#[test]
fn changed_sources_and_invalid_ranges_are_refused() {
    let dir = TempDir::new();
    let source = EncodedFile::new(dir.open("changed", b"abc\n")).unwrap();
    source.build_index().unwrap();
    assert!(matches!(source.decode_range(4..5), Err(EncodingError::InvalidRange)));
    let other = Buffer::from_file(dir.open("other", b"other")).unwrap();
    assert!(matches!(source.validate_save(&other.snapshot(), source.save_plan()), Err(EncodingError::WrongSource)));
    std::fs::write(dir.0.join("changed"), b"rewritten").unwrap();
    assert!(matches!(source.decode_range(0..1), Err(EncodingError::Read(ReadError::Changed(_)))));
    assert!(matches!(source.line_start(0), Err(EncodingError::Read(ReadError::Changed(_)))));
    assert!(matches!(source.build_index(), Err(EncodingError::Read(ReadError::Changed(_)))));
}

#[test]
fn giant_lines_require_column_ranges() {
    let dir = TempDir::new();
    let bytes = vec![b'x'; MAX_VIEWPORT_BYTES + 32];
    let source = EncodedFile::new(dir.open("giant", &bytes)).unwrap();
    source.build_index().unwrap();
    assert!(matches!(source.decode_lines(0, 1), Err(EncodingError::ViewportTooLarge)));
    assert_eq!(text(&source.decode_range(900_000..900_080).unwrap()), "x".repeat(80));
}

#[test]
fn ending_changes_preserve_malformed_bytes_and_cross_piece_crlf() {
    let dir = TempDir::new();
    let source = EncodedFile::new(dir.open("invalid-endings", b"\xef\xbb\xbfok\xff\r\n\xc0\rthird\nlast\r")).unwrap();
    let mut buffer = Buffer::from_file(source.file.clone()).unwrap();
    buffer.insert(buffer.len(), b"\nadded\xff").unwrap();
    // Added invalid UTF-8 is forbidden; original malformed bytes are retained.
    assert!(matches!(source.validate_save(&buffer.snapshot(), source.save_plan()), Err(EncodingError::InvalidUtf8)));
    buffer.delete(buffer.len() - 1..buffer.len()).unwrap();
    let plan = source.save_plan().change_line_endings(LineEnding::Lf);
    let mut saved = Vec::new();
    source.write_to(&buffer.snapshot(), plan, &mut saved).unwrap();
    assert_eq!(saved, b"\xef\xbb\xbfok\xff\n\xc0\nthird\nlast\nadded");
}

#[test]
fn line_ending_plan_is_one_undo_step_over_mixed_sources() {
    let dir = TempDir::new();
    let bytes = encode("one\r\ntwo\rthree\nlast\r", Encoding::UTF16_LE, true);
    let source = EncodedFile::new(dir.open("undo-endings", &bytes)).unwrap();
    let mut buffer = Buffer::from_file(source.file.clone()).unwrap();
    buffer.insert(buffer.len(), b"\nadded\r\n").unwrap();
    let before = buffer.snapshot();
    buffer.begin();
    let mut delta = 0i128;
    let mut edits = 0;
    source
        .plan_line_ending_edits(&before, LineEnding::Lf, |edit| {
            let start = (edit.range.start as i128 + delta) as u64;
            let end = (edit.range.end as i128 + delta) as u64;
            buffer.delete(start..end)?;
            buffer.insert(start, edit.replacement.as_bytes())?;
            delta += edit.replacement.len() as i128 - (edit.range.end - edit.range.start) as i128;
            edits += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(edits, 4);
    buffer.commit();
    let mut saved = Vec::new();
    source.write_to(&buffer.snapshot(), source.save_plan(), &mut saved).unwrap();
    assert_eq!(saved, encode("one\ntwo\nthree\nlast\nadded\n", Encoding::UTF16_LE, true));
    assert!(buffer.undo());
    let mut restored = Vec::new();
    source.write_to(&buffer.snapshot(), source.save_plan(), &mut restored).unwrap();
    let mut expected = bytes.clone();
    expected.extend_from_slice(&encode("\nadded\r\n", Encoding::UTF16_LE, false));
    assert_eq!(restored, expected);
}

#[test]
fn save_rejects_edits_inside_encoded_characters() {
    let dir = TempDir::new();
    for (i, encoding) in [Encoding::UTF16_LE, Encoding::for_label("shift_jis").unwrap()].into_iter().enumerate() {
        let bytes = encode("日本語", encoding, false);
        let source = EncodedFile::new(dir.open(&format!("split-{i}"), &bytes)).unwrap().reinterpret(encoding).unwrap();
        let mut buffer = Buffer::from_file(source.file.clone()).unwrap();
        buffer.insert(1, b"x").unwrap();
        assert!(matches!(
            source.validate_save(&buffer.snapshot(), source.save_plan()),
            Err(EncodingError::SplitCharacter(1))
        ));
    }
}

#[test]
fn review_explicit_override_recovers_ambiguous_utf32_detection() {
    let dir = TempDir::new();
    let bytes = encode("\0Hello\r\n", Encoding::UTF16_LE, true);
    assert!(bytes.starts_with(b"\xff\xfe\0\0"));
    let file = dir.open("utf16-leading-nul", &bytes);
    let automatic = EncodedFile::new(file.clone()).unwrap();
    assert_eq!(automatic.detection().encoding, Encoding::UTF32_LE);
    assert_eq!(automatic.detection().bom_len, 4);
    let source = EncodedFile::new_with_encoding(file.clone(), Encoding::UTF16_LE).unwrap();
    assert_eq!(source.detection().encoding, Encoding::UTF16_LE);
    assert_eq!(source.detection().bom_len, 2);
    assert_eq!(text(&source.decode_range(0..bytes.len() as u64).unwrap()), "\0Hello\r\n");
    let buffer = Buffer::from_file(file).unwrap();
    let mut saved = Vec::new();
    source.write_to(&buffer.snapshot(), source.save_plan(), &mut saved).unwrap();
    assert_eq!(saved, bytes);
    assert_eq!(source.reinterpret(Encoding::UTF16_LE).unwrap().detection().encoding, Encoding::UTF16_LE);

    // ISO is now detected and decoded, while an explicit UTF-8 override still
    // reinterprets the retained raw TextFile without changing its bytes.
    let (iso, _, errors) = encoding_rs::ISO_2022_JP.encode("日本語のメールです。");
    assert!(!errors);
    let file = dir.open("override-iso", &iso);
    let automatic = EncodedFile::new(file.clone()).unwrap();
    assert_eq!(automatic.detection().encoding, Encoding::ISO2022_JP);
    assert_eq!(text(&automatic.decode_range(0..iso.len() as u64).unwrap()), "日本語のメールです。");
    let explicit = EncodedFile::new_with_encoding(file, Encoding::UTF8).unwrap();
    assert_eq!(explicit.detection().encoding, Encoding::UTF8);
}

#[test]
fn review_conversion_decodes_logically_adjacent_original_pieces() {
    let dir = TempDir::new();
    for (i, (encoding, bytes, deletion, expected)) in [
        (Encoding::UTF8, b"\xc3x\xa9Z".as_slice(), 1..2, "éZ"),
        (Encoding::for_label("shift_jis").unwrap(), b"\x93x\xfaZ".as_slice(), 1..2, "日Z"),
        (Encoding::UTF16_LE, b"\x3d\xd8x\0\0\xdeZ\0".as_slice(), 2..4, "😀Z"),
    ]
    .into_iter()
    .enumerate()
    {
        let file = dir.open(&format!("joined-{i}"), bytes);
        let source = EncodedFile::new_with_encoding(file.clone(), encoding).unwrap();
        let mut buffer = Buffer::from_file(file).unwrap();
        buffer.delete(deletion).unwrap();
        let target = if encoding == Encoding::UTF16_LE { Encoding::UTF8 } else { Encoding::UTF16_BE };
        let plan = source.save_plan().convert_to(target, true);
        let mut saved = Vec::new();
        source.write_to(&buffer.snapshot(), plan, &mut saved).unwrap();
        assert_eq!(saved, encode(expected, target, true));

        // An actual transition to added UTF-8 finalizes the original stream;
        // a dangling lead byte must not be completed from added text.
        if encoding == Encoding::UTF8 {
            buffer.insert(1, b"x").unwrap();
            let mut untouched = b"sentinel".to_vec();
            assert!(matches!(
                source.write_to(&buffer.snapshot(), plan, &mut untouched),
                Err(EncodingError::MalformedSource)
            ));
            assert_eq!(untouched, b"sentinel");
            buffer.undo();
            buffer.delete(1..buffer.len()).unwrap();
            assert!(matches!(source.validate_save(&buffer.snapshot(), plan), Err(EncodingError::MalformedSource)));
        }
    }
}

#[test]
fn iso2022_round_trip_and_edited_shifted_text() {
    let dir = TempDir::new();
    let raw = b"\x1b$BF|K\\8l\x1b(B\r\n";
    let source = EncodedFile::new(dir.open("iso-roundtrip", raw)).unwrap();
    assert_eq!(source.detection().encoding.name(), "ISO-2022-JP");
    source.build_index().unwrap();
    assert_eq!(text(&source.decode_range(0..raw.len() as u64).unwrap()), "日本語\r\n");
    let mut buffer = Buffer::from_file(source.file.clone()).unwrap();
    let mut saved = Vec::new();
    source.write_to(&buffer.snapshot(), source.save_plan(), &mut saved).unwrap();
    assert_eq!(saved, raw);
    buffer.insert(5, "本X".as_bytes()).unwrap();
    saved.clear();
    source.write_to(&buffer.snapshot(), source.save_plan(), &mut saved).unwrap();
    let (decoded, errors) = encoding_rs::ISO_2022_JP.decode_without_bom_handling(&saved);
    assert!(!errors);
    assert_eq!(decoded, "日本X本語\r\n");
}

#[test]
fn iso2022_random_access_sparse_checkpoints_keep_shifted_charset() {
    let dir = TempDir::new();
    let mut raw = Vec::new();
    let mut positions = Vec::new();
    for i in 0..180000 {
        positions.push(raw.len() as u64);
        raw.extend_from_slice(if i % 2 == 0 { b"\x1b$BF|K\\8l\x1b(B\r\n" } else { b"\x1b(J\\~\x1b(B abc\n" });
    }
    let source = EncodedFile::new(dir.open("iso-random", &raw)).unwrap();
    let index = source.build_index().unwrap();
    assert!(index.checkpoint_count() <= raw.len() / STRIDE + 2);
    assert!(index.allocated_bytes() < 4096);
    for i in [170000, 7, 12345, 81920, 179998, 90001] {
        let decoded = source.decode_lines(i as u64, 1).unwrap().unwrap();
        assert_eq!(decoded.source.start, positions[i]);
        assert_eq!(text(&decoded), if i % 2 == 0 { "日本語\r\n" } else { "¥‾ abc\n" });
    }
    let raw = [b"\x1b$B".as_slice(), &b"F|".repeat(80000), b"\x1b(B"].concat();
    let source = EncodedFile::new(dir.open("iso-long-shift", &raw)).unwrap();
    source.build_index().unwrap();
    assert_eq!(text(&source.decode_range(140003..140005).unwrap()), "日");
}

#[test]
fn iso2022_malformed_input_is_lossless_and_conversion_refuses() {
    let dir = TempDir::new();
    let encoding = Encoding::for_label("iso-2022-jp").unwrap();
    for (i, raw) in
        [b"\x1b$BF|\xff\x1b(B".as_slice(), b"\x1b$BF", b"\x1b$BF|\x1b", b"\x1b$BF|\x1b(X"].into_iter().enumerate()
    {
        let source = EncodedFile::new_with_encoding(dir.open(&format!("iso-malformed-{i}"), raw), encoding).unwrap();
        source.build_index().unwrap();
        let display = source.decode_range(0..raw.len() as u64).unwrap();
        assert!(display.had_errors);
        assert!(display.segments.iter().any(|s| matches!(s, DisplaySegment::Invalid(_))));
        let buffer = Buffer::from_file(source.file.clone()).unwrap();
        let mut saved = Vec::new();
        source.write_to(&buffer.snapshot(), source.save_plan(), &mut saved).unwrap();
        assert_eq!(saved, raw);
        saved.clear();
        assert!(
            source
                .write_to(&buffer.snapshot(), source.save_plan().convert_to(Encoding::UTF8, false), &mut saved)
                .is_err()
        );
        assert!(saved.is_empty());
    }
}

#[test]
fn iso2022_decoded_backing_roundtrip_edit_and_malformed() {
    use crate::decoded_backing::{DecodedBacking, Interpretation};
    use std::sync::atomic::AtomicBool;
    let dir = TempDir::new();
    let encoding = Encoding::for_label("iso-2022-jp").unwrap();
    for (i, raw) in [b"\x1b$BF|K\\8l\x1b(B".as_slice(), b"\x1b$BF|\xff\x1b(B"].into_iter().enumerate() {
        let file = dir.open(&format!("iso-backing-{i}"), raw);
        let owner =
            DecodedBacking::prepare(&file, Interpretation::new(encoding, 0).unwrap(), &dir.0, &AtomicBool::new(false))
                .unwrap();
        let mut buffer = Buffer::from_decoded(owner.snapshot()).unwrap();
        let plan = SavePlan { encoding, bom: false, line_ending: None };
        let mut saved = Vec::new();
        write_snapshot(&buffer.snapshot(), plan, &mut saved).unwrap();
        assert_eq!(saved, raw);
        if i == 0 {
            assert_eq!(owner.snapshot().logical_bytes(), "日本語".as_bytes());
            let mut cursor = crate::WorkerSourceCursor::try_new(buffer.snapshot(), 0..buffer.len(), |_| Ok(true))
                .unwrap_or_else(|_| panic!("ISO cursor"));
            let mut collected = Vec::new();
            loop {
                let mut logical = [0; 16];
                let mut raw = [0; 16];
                match cursor.fill(&mut logical, &mut raw, &AtomicBool::new(false), |_| Ok(true)).unwrap() {
                    crate::WorkerSourceStep::Ready(Some(chunk)) => {
                        assert_eq!(chunk.kind, crate::WorkerSourceKind::Text);
                        collected.extend_from_slice(&logical[..chunk.logical_len]);
                    }
                    crate::WorkerSourceStep::Ready(None) => break,
                    crate::WorkerSourceStep::Pending => panic!("unlimited source admission"),
                }
            }
            assert_eq!(collected, "日本語".as_bytes());
            drop(cursor.into_snapshot());
            buffer.insert(3, b"X").unwrap();
            saved.clear();
            write_snapshot(&buffer.snapshot(), plan, &mut saved).unwrap();
            let (text, errors) = encoding_rs::ISO_2022_JP.decode_without_bom_handling(&saved);
            assert!(!errors);
            assert_eq!(text, "日X本語");
            buffer.insert(buffer.len(), "日".as_bytes()).unwrap();
            saved.clear();
            write_snapshot(&buffer.snapshot(), plan, &mut saved).unwrap();
            let (text, errors) = encoding_rs::ISO_2022_JP.decode_without_bom_handling(&saved);
            assert!(!errors, "appending shifted text must coalesce the original final reset");
            assert_eq!(text, "日X本語日");
            let mut shifted = Buffer::from_decoded(owner.snapshot()).unwrap();
            shifted.insert(3, "日".as_bytes()).unwrap();
            saved.clear();
            write_snapshot(&shifted.snapshot(), plan, &mut saved).unwrap();
            let (text, errors) = encoding_rs::ISO_2022_JP.decode_without_bom_handling(&saved);
            assert!(!errors, "new shifted text must join the shifted original");
            assert_eq!(text, "日日本語");
            drop(shifted);
        } else {
            assert!(owner.snapshot().logical_bytes().contains(&0xff));
            assert!(
                write_snapshot(&buffer.snapshot(), plan.convert_to(Encoding::UTF8, false), &mut Vec::new()).is_err()
            );
        }
        drop(buffer);
        assert!(owner.try_retire().is_ok());
    }
}

#[test]
fn iso2022_save_conversion_finishes_escape_and_rejects_unmappable() {
    let encoding = Encoding::for_label("iso-2022-jp").unwrap();
    let mut buffer = Buffer::empty();
    buffer.insert(0, "日".as_bytes()).unwrap();
    let plan = SavePlan { encoding, bom: false, line_ending: None };
    let mut saved = Vec::new();
    write_snapshot(&buffer.snapshot(), plan, &mut saved).unwrap();
    assert_eq!(saved, b"\x1b$BF|\x1b(B");
    buffer.insert(buffer.len(), "😀".as_bytes()).unwrap();
    saved.clear();
    assert!(matches!(write_snapshot(&buffer.snapshot(), plan, &mut saved), Err(EncodingError::Unmappable { .. })));
    assert!(saved.is_empty());
}

#[test]
fn iso2022_backing_sparse_replay_and_first_preview() {
    use crate::decoded_backing::{Boundary, DecodedBacking, Interpretation, decode_preview};
    use std::sync::atomic::AtomicBool;
    let dir = TempDir::new();
    let encoding = Encoding::for_label("iso-2022-jp").unwrap();
    let raw = [b"\x1b$B".as_slice(), &b"F|".repeat(80000), b"\x1b(B"].concat();
    let file = dir.open("iso-backing-seek", &raw);
    let cancel = AtomicBool::new(false);
    let interpretation = Interpretation::new(encoding, 0).unwrap();
    let preview = decode_preview(&file, interpretation, &cancel).unwrap();
    assert!(!preview.complete);
    assert!(std::str::from_utf8(&preview.text).unwrap().chars().all(|c| c == '日'));
    let owner = DecodedBacking::prepare(&file, interpretation, &dir.0, &cancel).unwrap();
    let origin = owner.snapshot();
    assert!(origin.checkpoint_count() <= 5);
    for at in [70000, 4, 65536, 79999, 40000] {
        let logical = at * 3;
        let raw = at * 2 + 3;
        assert_eq!(origin.resolve_logical(logical).unwrap(), Boundary::Exact { logical, raw });
        let atom = origin.atom_at(logical).unwrap().unwrap();
        assert_eq!(atom.content, crate::decoded_backing::AtomContent::Text("日"));
        let mut position = crate::decoded_backing::ResumeBoundary::new(&origin, logical);
        let boundary = position.poll(&origin, &cancel, &mut |_| Ok(true)).unwrap().unwrap();
        assert_eq!(boundary, Boundary::Exact { logical, raw });
    }
    drop(origin);
    assert!(owner.try_retire().is_ok());
}
