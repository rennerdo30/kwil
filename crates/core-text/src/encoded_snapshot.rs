//! Save logical snapshots with independently interpreted immutable origins.
//! Unlike EncodedFile's raw-byte API, ordinary originals are logical UTF-8:
//! a leading U+FEFF is data. Only the explicit SavePlan emits an output BOM.

use super::{EncodingError, SavePlan, TextOutput, Utf8Input};
use crate::decoded_backing::{AtomContent, AtomKind, Boundary, DecodedSnapshot};
use crate::{Snapshot, SourceChunk};
use std::io::{self, Write};
use std::ops::Range;

/// Preflight on a worker before creating/opening the destination. No bytes are
/// retained. Use the same immutable snapshot and plan for the eventual write.
pub fn validate_snapshot_save(snapshot: &Snapshot, plan: SavePlan) -> Result<(), EncodingError> {
    stream(snapshot, plan, &mut io::sink())
}

/// All character, malformed-source and endpoint failures occur before the
/// first writer call. Disk failure during writing is the atomic-save envelope's
/// responsibility. Scratch and writer requests are bounded, regardless of size.
pub fn write_snapshot(snapshot: &Snapshot, plan: SavePlan, writer: &mut impl Write) -> Result<(), EncodingError> {
    validate_snapshot_save(snapshot, plan)?;
    stream(snapshot, plan, writer)
}

fn backing_error(error: crate::decoded_backing::BackingError) -> EncodingError {
    EncodingError::Io(io::Error::other(error))
}

/// Validate scalar endpoints without pretending the interior of a Big5
/// multi-scalar atom has a raw boundary. The partial valid text is re-encoded.
fn endpoint(origin: &DecodedSnapshot, at: u64) -> Result<Boundary, EncodingError> {
    let boundary = origin.resolve_logical(at).map_err(backing_error)?;
    if let Boundary::Interior { logical, kind, .. } = &boundary {
        if *kind != AtomKind::Text {
            return Err(EncodingError::SplitCharacter(at));
        }
        let text = std::str::from_utf8(&origin.logical_bytes()[logical.start as usize..logical.end as usize])
            .map_err(|_| EncodingError::MalformedSource)?;
        if !text.is_char_boundary((at - logical.start) as usize) {
            return Err(EncodingError::SplitCharacter(at));
        }
    }
    Ok(boundary)
}

fn text(output: &mut TextOutput<'_, impl Write>, bytes: &[u8]) -> Result<(), EncodingError> {
    // A decoded opaque FF is never mistaken for literal U+FFFD or escape text.
    let text = std::str::from_utf8(bytes).map_err(|_| EncodingError::MalformedSource)?;
    output.feed(text)
}

fn raw(output: &mut TextOutput<'_, impl Write>, bytes: &[u8]) -> Result<(), EncodingError> {
    if output.pending_cr {
        output.encode(output.plan.line_ending.unwrap().text())?;
        output.pending_cr = false;
    }
    for chunk in bytes.chunks(64 << 10) {
        output.writer.write_all(chunk)?;
    }
    Ok(())
}

fn decoded(
    origin: &DecodedSnapshot,
    range: Range<u64>,
    output: &mut TextOutput<'_, impl Write>,
) -> Result<(), EncodingError> {
    let start = endpoint(origin, range.start)?;
    let end = endpoint(origin, range.end)?;
    if output.plan.encoding != origin.interpretation().encoding {
        let mut input = Utf8Input::default();
        for chunk in origin.logical_bytes()[range.start as usize..range.end as usize].chunks(16 << 10) {
            input.feed(chunk, |s| output.feed(s)).map_err(|error| match error {
                EncodingError::InvalidUtf8 => EncodingError::MalformedSource,
                other => other,
            })?;
        }
        return input.finish().map_err(|_| EncodingError::MalformedSource);
    }
    let mut first = range.start;
    if let Boundary::Interior { logical, .. } = start {
        first = logical.end.min(range.end);
        text(output, &origin.logical_bytes()[range.start as usize..first as usize])?;
    }
    let last = match end {
        Boundary::Interior { logical, .. } => logical.start.max(first),
        _ => range.end,
    };
    if first < last {
        let middle = first..last;
        if output.plan.line_ending.is_none() {
            let range = origin.raw_range(middle).map_err(backing_error)?;
            raw(output, &origin.raw_bytes()[range.start as usize..range.end as usize])?;
        } else {
            // Decode atoms only to recognize newline characters, preserving
            // every other atom's bytes, including malformed runs and aliases.
            let mut run = None;
            for atom in origin.atoms(middle).map_err(backing_error)? {
                let atom = atom.map_err(backing_error)?;
                if matches!(atom.content, AtomContent::Text("\r" | "\n")) {
                    if let Some(begin) = run.take() {
                        raw(output, &origin.raw_bytes()[begin..atom.raw.start as usize])?;
                    }
                    if let AtomContent::Text(s) = atom.content {
                        output.feed(s)?;
                    }
                } else {
                    run.get_or_insert(atom.raw.start as usize);
                }
                if atom.logical.end == last
                    && let Some(begin) = run.take()
                {
                    raw(output, &origin.raw_bytes()[begin..atom.raw.end as usize])?;
                }
            }
        }
    }
    if last < range.end {
        text(output, &origin.logical_bytes()[last as usize..range.end as usize])?;
    }
    Ok(())
}

fn stream(snapshot: &Snapshot, plan: SavePlan, writer: &mut impl Write) -> Result<(), EncodingError> {
    if plan.bom && plan.encoding.bom().is_empty() {
        return Err(EncodingError::UnsupportedEncoding(format!("BOM for {}", plan.encoding.name())));
    }
    if plan.bom {
        writer.write_all(plan.encoding.bom())?;
    }
    let mut output = TextOutput::new(plan, writer);
    let mut input = Utf8Input::default();
    for chunk in snapshot.source_chunks(0..snapshot.len())? {
        match chunk? {
            SourceChunk::Decoded { origin, range, .. } => {
                input.finish()?;
                decoded(origin, range, &mut output)?;
            }
            SourceChunk::Opaque { encoding, raw_bytes, .. } => {
                input.finish()?;
                if plan.encoding != encoding {
                    return Err(EncodingError::MalformedSource);
                }
                raw(&mut output, raw_bytes)?;
            }
            SourceChunk::Original { bytes, .. } | SourceChunk::Added(bytes) => {
                for chunk in bytes.chunks(16 << 10) {
                    input.feed(chunk, |s| output.feed(s))?;
                }
            }
        }
    }
    input.finish()?;
    output.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoded_backing::{DecodedBacking, Interpretation};
    use crate::encoding::{Encoding, LineEnding};
    use crate::{Buffer, TextFile};
    use std::cell::{Cell, RefCell};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    struct Fixture {
        directory: PathBuf,
        next: Cell<usize>,
        owners: RefCell<Vec<DecodedBacking>>,
    }
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let directory = std::env::temp_dir().join(format!(
                "kwil-logical-save-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&directory).unwrap();
            Self { directory, next: Cell::new(0), owners: RefCell::new(Vec::new()) }
        }
        fn decoded(&self, bytes: &[u8], encoding: Encoding, bom: usize) -> Buffer {
            let next = self.next.get();
            self.next.set(next + 1);
            let path = self.directory.join(format!("source-{next}"));
            std::fs::write(&path, bytes).unwrap();
            let source = TextFile::open(&path).unwrap();
            let owner = DecodedBacking::prepare(
                &source,
                Interpretation::new(encoding, bom).unwrap(),
                &self.directory,
                &AtomicBool::new(false),
            )
            .unwrap();
            let buffer = Buffer::from_decoded(owner.snapshot());
            self.owners.borrow_mut().push(owner);
            buffer
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            for owner in self.owners.get_mut().drain(..) {
                assert!(owner.try_retire().is_ok(), "live decoded root after test");
            }
            std::fs::remove_dir_all(&self.directory).unwrap();
        }
    }
    fn plan(encoding: Encoding, bom: bool) -> SavePlan {
        SavePlan { encoding, bom, line_ending: None }
    }
    fn saved(snapshot: &Snapshot, plan: SavePlan) -> Vec<u8> {
        let mut output = Vec::new();
        write_snapshot(snapshot, plan, &mut output).unwrap();
        output
    }
    fn encode(text: &str, encoding: Encoding) -> Vec<u8> {
        if encoding == Encoding::UTF16_LE || encoding == Encoding::UTF16_BE {
            text.encode_utf16()
                .flat_map(|u| if encoding == Encoding::UTF16_LE { u.to_le_bytes() } else { u.to_be_bytes() })
                .collect()
        } else {
            let (bytes, _, errors) = encoding.0.encode(text);
            assert!(!errors);
            bytes.into_owned()
        }
    }

    #[test]
    fn decoded_utf16_append_uses_logical_coordinates_and_one_bom() {
        let fixture = Fixture::new();
        let mut buffer = fixture.decoded(b"\xff\xfeA\0", Encoding::UTF16_LE, 2);
        assert_eq!(buffer.len(), 1);
        buffer.insert(1, b"x").unwrap();
        assert_eq!(saved(&buffer.snapshot(), plan(Encoding::UTF16_LE, true)), b"\xff\xfeA\0x\0");
        assert!(buffer.undo());
        assert_eq!(saved(&buffer.snapshot(), plan(Encoding::UTF16_LE, true)), b"\xff\xfeA\0");
    }

    #[test]
    fn decoded_all_stateless_families_preserve_original_and_encode_added_text() {
        let fixture = Fixture::new();
        let cases = [
            ("utf-8", "😀é"),
            ("utf-16le", "😀é"),
            ("utf-16be", "😀é"),
            ("shift_jis", "日本"),
            ("euc-jp", "日本"),
            ("euc-kr", "한국"),
            ("gbk", "中文"),
            ("gb18030", "😀中文"),
            ("big5", "中文"),
            ("windows-1252", "café"),
            ("koi8-r", "Привет"),
            ("ibm866", "Привет"),
            ("macintosh", "café"),
            ("x-mac-cyrillic", "Привет"),
            ("iso-8859-2", "ąć"),
            ("iso-8859-3", "ħ"),
            ("iso-8859-4", "Ā"),
            ("iso-8859-5", "Привет"),
            ("iso-8859-6", "ع"),
            ("iso-8859-7", "Ω"),
            ("iso-8859-8", "א"),
            ("iso-8859-8-i", "א"),
            ("iso-8859-10", "Ā"),
            ("iso-8859-13", "Ā"),
            ("iso-8859-14", "Ŵ"),
            ("iso-8859-15", "€"),
            ("iso-8859-16", "Ș"),
            ("koi8-u", "Ї"),
            ("windows-874", "ก"),
            ("windows-1250", "ą"),
            ("windows-1251", "Я"),
            ("windows-1253", "Ω"),
            ("windows-1254", "Ğ"),
            ("windows-1255", "א"),
            ("windows-1256", "ع"),
            ("windows-1257", "Ā"),
            ("windows-1258", "Ơ"),
            ("x-user-defined", "\u{f780}"),
        ];
        for (label, text) in cases {
            let encoding = Encoding::for_label(label).unwrap();
            let original = encode(&format!("{text}\r\na\rb\nc"), encoding);
            let mut buffer = fixture.decoded(&original, encoding, 0);
            assert_eq!(saved(&buffer.snapshot(), plan(encoding, false)), original, "{label}");
            buffer.insert(buffer.len(), b"x").unwrap();
            let mut expected = original;
            expected.extend(encode("x", encoding));
            assert_eq!(saved(&buffer.snapshot(), plan(encoding, false)), expected, "{label}");
            assert_eq!(
                saved(&buffer.snapshot(), plan(Encoding::UTF8, false)),
                format!("{text}\r\na\rb\ncx").as_bytes(),
                "{label}"
            );
        }
    }

    #[test]
    fn decoded_malformed_and_alias_atoms_round_trip_without_replacement() {
        let fixture = Fixture::new();
        for (label, bytes, bom) in [
            ("utf-8", &b"\xef\xbb\xbf\xef\xbf\xbd\\xFF\xff\xc0\r\n"[..], 3),
            ("utf-16le", &b"\xff\xfe\xfd\xff\\\0x\0F\0F\0\0\xd8A\0\xff"[..], 2),
            ("utf-16be", &b"\xfe\xff\xff\xfd\xd8\0\0A\xff"[..], 2),
            ("shift_jis", &b"A\x82 \xff\r\n"[..], 0),
            ("big5", &b"\x88\x62\xff\r\n"[..], 0),
            ("gb18030", &b"\x81\x30A\xff"[..], 0),
            ("euc-jp", &b"\x8f\xff\r\n"[..], 0),
        ] {
            let encoding = Encoding::for_label(label).unwrap();
            let mut buffer = fixture.decoded(bytes, encoding, bom);
            assert_eq!(saved(&buffer.snapshot(), plan(encoding, bom != 0)), bytes, "{label}");
            buffer.insert(buffer.len(), b"x").unwrap();
            let mut expected = bytes.to_vec();
            expected.extend(encode("x", encoding));
            assert_eq!(saved(&buffer.snapshot(), plan(encoding, bom != 0)), expected, "{label}");
        }
    }

    #[derive(Default)]
    struct Untouched {
        calls: usize,
    }
    impl Write for Untouched {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.calls += 1;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.calls += 1;
            Ok(())
        }
    }
    #[test]
    fn decoded_preflight_unmappable_malformed_and_partial_scalar_touch_no_writer() {
        let fixture = Fixture::new();
        let buffer = fixture.decoded("a😀".as_bytes(), Encoding::UTF8, 0);
        let malformed = fixture.decoded(b"a\xff", Encoding::UTF8, 0);
        let partial = buffer.snapshot().slice(0..2).unwrap();
        for (snapshot, target) in [
            (buffer.snapshot(), Encoding::for_label("windows-1252").unwrap()),
            (malformed.snapshot(), Encoding::UTF16_LE),
            (partial, Encoding::UTF8),
        ] {
            let mut writer = Untouched::default();
            assert!(
                write_snapshot(
                    &snapshot,
                    plan(target, target != Encoding::for_label("windows-1252").unwrap()),
                    &mut writer
                )
                .is_err()
            );
            assert_eq!(writer.calls, 0);
        }
        assert!(matches!(
            validate_snapshot_save(&buffer.snapshot().slice(2..5).unwrap(), plan(Encoding::UTF8, false)),
            Err(EncodingError::SplitCharacter(_))
        ));
    }

    #[test]
    fn decoded_big5_expansion_partial_text_never_invents_raw_boundaries() {
        let fixture = Fixture::new();
        let encoding = Encoding::for_label("big5").unwrap();
        let buffer = fixture.decoded(b"\x88\x62!", encoding, 0);
        let snapshot = buffer.snapshot();
        assert_eq!(saved(&snapshot.slice(0..2).unwrap(), plan(Encoding::UTF8, false)), "Ê".as_bytes());
        assert_eq!(
            saved(&snapshot.slice(2..4).unwrap(), plan(Encoding::UTF16_LE, false)),
            encode("\u{304}", Encoding::UTF16_LE)
        );
        assert_eq!(saved(&snapshot.slice(0..4).unwrap(), plan(encoding, false)), b"\x88\x62");
        assert!(matches!(
            validate_snapshot_save(&snapshot.slice(1..4).unwrap(), plan(encoding, false)),
            Err(EncodingError::SplitCharacter(1))
        ));
    }

    #[test]
    fn decoded_two_origins_and_multiple_checkpoint_generations_keep_undo_provenance() {
        let fixture = Fixture::new();
        let mut buffer = fixture.decoded(b"\xff\xfeA\0", Encoding::UTF16_LE, 2);
        let clipboard = buffer.snapshot();
        buffer.insert(1, b"x").unwrap();
        let checkpoint = fixture.decoded(b"\xff\xfeA\0x\0", Encoding::UTF16_LE, 2).snapshot();
        buffer.checkpoint_current(checkpoint).unwrap();
        buffer.insert_snapshot_range(2, &clipboard, 0..1).unwrap();
        assert_eq!(saved(&buffer.snapshot(), plan(Encoding::UTF16_LE, true)), b"\xff\xfeA\0x\0A\0");
        let mut origins = Vec::new();
        for chunk in buffer.snapshot().source_chunks(0..3).unwrap() {
            if let SourceChunk::Decoded { origin, range, .. } = chunk.unwrap() {
                origins.push((origin.clone(), range));
            }
        }
        assert_eq!(origins.len(), 2);
        assert!(!origins[0].0.same_origin(&origins[1].0));
        // Save again, keeping clipboard/history from both earlier generations.
        let second_checkpoint = fixture.decoded(b"\xff\xfeA\0x\0A\0", Encoding::UTF16_LE, 2).snapshot();
        buffer.checkpoint_current(second_checkpoint).unwrap();
        buffer.insert_snapshot_range(3, &clipboard, 0..1).unwrap();
        assert_eq!(saved(&buffer.snapshot(), plan(Encoding::UTF16_LE, true)), b"\xff\xfeA\0x\0A\0A\0");
        buffer.delete(0..4).unwrap();
        buffer.insert_snapshot_range(0, &clipboard, 0..1).unwrap();
        assert_eq!(buffer.add_memory_bytes(), 1, "only prior added x remains in history");
        assert_eq!(saved(&buffer.snapshot(), plan(Encoding::UTF16_LE, true)), b"\xff\xfeA\0");
        assert!(buffer.undo());
        assert!(buffer.is_empty());
        assert!(buffer.undo());
        assert_eq!(saved(&buffer.snapshot(), plan(Encoding::UTF16_LE, true)), b"\xff\xfeA\0x\0A\0A\0");
        assert!(buffer.undo());
        assert_eq!(saved(&buffer.snapshot(), plan(Encoding::UTF16_LE, true)), b"\xff\xfeA\0x\0A\0");
        assert!(buffer.undo());
        assert!(buffer.undo());
        assert_eq!(saved(&buffer.snapshot(), plan(Encoding::UTF16_LE, true)), b"\xff\xfeA\0");
    }

    #[test]
    fn decoded_own_full_range_paste_shares_root_after_delete_all() {
        let fixture = Fixture::new();
        let mut buffer = fixture.decoded(b"A\0\n\0B\0", Encoding::UTF16_LE, 0);
        let clipboard = buffer.snapshot();
        buffer.set_undo_memory_limit(0);
        buffer.delete(0..buffer.len()).unwrap();
        buffer.insert_snapshot_range(0, &clipboard, 0..clipboard.len()).unwrap();
        assert!(Arc::ptr_eq(buffer.snapshot().root.as_ref().unwrap(), clipboard.root.as_ref().unwrap()));
        assert_eq!(buffer.add_memory_bytes(), 0);
        assert_eq!(buffer.line_start(1).unwrap(), Some(2));
    }

    #[test]
    fn decoded_newline_normalization_preserves_opaque_and_split_crlf() {
        let fixture = Fixture::new();
        let mut buffer = fixture.decoded(b"\xff\xfeA\0\r\0\n\0\0\xd8B\0\r\0C\0\n\0", Encoding::UTF16_LE, 2);
        // Split CRLF across immutable pieces without changing its text.
        let suffix = buffer.snapshot().slice(2..buffer.len()).unwrap();
        buffer.delete(2..buffer.len()).unwrap();
        buffer.insert_snapshot_range(buffer.len(), &suffix, 0..suffix.len()).unwrap();
        let mut target = plan(Encoding::UTF16_LE, true);
        target.line_ending = Some(LineEnding::Lf);
        assert_eq!(saved(&buffer.snapshot(), target), b"\xff\xfeA\0\n\0\0\xd8B\0\n\0C\0\n\0");
        assert_eq!(buffer.line_count(), 3);
        assert_eq!(buffer.offset_to_line_col(5).unwrap(), Some((1, 2)));
    }

    #[test]
    fn logical_raw_original_bom_is_data_and_old_encodedfile_keeps_its_contract() {
        let fixture = Fixture::new();
        let path = fixture.directory.join("logical-original");
        std::fs::write(&path, b"\xef\xbb\xbfA").unwrap();
        let file = TextFile::open(&path).unwrap();
        let buffer = Buffer::from_file(file.clone()).unwrap();
        assert_eq!(saved(&buffer.snapshot(), plan(Encoding::UTF8, true)), b"\xef\xbb\xbf\xef\xbb\xbfA");
        let encoded = crate::encoding::EncodedFile::new(file).unwrap();
        let mut bytes = Vec::new();
        encoded.write_to(&buffer.snapshot(), encoded.save_plan(), &mut bytes).unwrap();
        assert_eq!(bytes, b"\xef\xbb\xbfA");
        let decoded = fixture.decoded(b"A\0", Encoding::UTF16_LE, 0);
        let mut writer = Untouched::default();
        assert!(matches!(
            encoded.write_to(&decoded.snapshot(), encoded.save_plan(), &mut writer),
            Err(EncodingError::WrongSource)
        ));
        assert_eq!(writer.calls, 0);
    }

    #[test]
    fn decoded_foreign_opaque_import_releases_origin_and_obeys_caps_for_all_payloads() {
        let fixture = Fixture::new();
        let raw = b"A\0\0\xd8B\0\xff";
        let source = fixture.decoded(raw, Encoding::UTF16_LE, 0);
        let mut target = Buffer::with_add_memory_limit(0);
        target.set_undo_memory_limit(0);
        let prepared = target.snapshot_importer().prepare(&source.snapshot(), 0..source.len()).unwrap();
        target.insert_snapshot_range(0, &prepared, 0..prepared.len()).unwrap();
        drop((source, prepared));
        let owner = fixture.owners.borrow_mut().pop().unwrap();
        assert!(owner.try_retire().is_ok(), "foreign paste must release the complete source/index");
        assert_eq!(target.add_memory_bytes(), 0);
        assert_eq!(target.undo_memory_bytes(), 0);
        assert_eq!(saved(&target.snapshot(), plan(Encoding::UTF16_LE, false)), raw);
        let mut atoms = Vec::new();
        let snapshot = target.snapshot();
        for chunk in snapshot.source_chunks(0..snapshot.len()).unwrap() {
            if let SourceChunk::Opaque { bytes, raw_bytes, raw_offsets, .. } = chunk.unwrap() {
                let base = u32::from_le_bytes(raw_offsets[..4].try_into().unwrap()) as usize;
                assert_eq!(raw_offsets.len(), (bytes.len() + 1) * 4);
                for window in raw_offsets.windows(8).step_by(4) {
                    let start = u32::from_le_bytes(window[..4].try_into().unwrap()) as usize - base;
                    let end = u32::from_le_bytes(window[4..].try_into().unwrap()) as usize - base;
                    atoms.push(raw_bytes[start..end].to_vec());
                }
            }
        }
        assert_eq!(atoms, [vec![0, 0xd8], vec![0xff]]);
        let mut writer = Untouched::default();
        assert!(matches!(
            write_snapshot(&snapshot, plan(Encoding::UTF8, false), &mut writer),
            Err(EncodingError::MalformedSource)
        ));
        assert_eq!(writer.calls, 0);
        // Independent split/cut/copy must use the raw endpoint table, not the
        // FF sentinel or one-byte-per-raw-byte assumptions.
        let opaque = snapshot.slice(1..2).unwrap().concat(&snapshot.slice(3..4).unwrap()).unwrap();
        assert_eq!(saved(&opaque, plan(Encoding::UTF16_LE, false)), b"\0\xd8\xff");
        let mut second = Buffer::with_add_memory_limit(1);
        second.insert_snapshot_range(0, &opaque, 0..opaque.len()).unwrap();
        assert!(second.add_memory_bytes() <= 1);
        assert_eq!(saved(&second.snapshot(), plan(Encoding::UTF16_LE, false)), b"\0\xd8\xff");
    }

    #[test]
    fn decoded_foreign_grouped_opaque_blocks_bound_tables_and_reimport_memory() {
        let fixture = Fixture::new();
        let source = fixture.decoded(&vec![0xff; 40_000], Encoding::UTF8, 0);
        let mut first = Buffer::with_add_memory_limit(400_000);
        first.set_undo_memory_limit(0);
        first.insert_snapshot_range(0, &source.snapshot(), 0..source.len()).unwrap();
        let snapshot = first.snapshot();
        let mut blocks = 0;
        for chunk in snapshot.source_chunks(0..snapshot.len()).unwrap() {
            if let SourceChunk::Opaque { bytes, raw_bytes, raw_offsets, .. } = chunk.unwrap() {
                blocks += 1;
                assert!(raw_offsets.len() <= 65536 && raw_bytes.len() <= 65536 && bytes.len() <= 65536);
                assert_eq!(raw_offsets.len(), (bytes.len() + 1) * 4);
            } else {
                panic!("opaque run lost typed provenance");
            }
        }
        assert_eq!(blocks, 3, "bound table entries and coalesce atoms into blocks");
        assert_eq!(first.add_memory_bytes(), 40_000 * 6 + blocks * 4, "logical+raw+endpoint payload all charged");
        let mut target = Buffer::with_add_memory_limit(0);
        target.set_undo_memory_limit(0);
        target.insert_snapshot_range(0, &snapshot, 0..snapshot.len()).unwrap();
        assert_eq!(target.add_memory_bytes(), 0);
        drop((source, first, snapshot));
        let owner = fixture.owners.borrow_mut().pop().unwrap();
        assert!(owner.try_retire().is_ok());
        assert_eq!(
            saved(&target.snapshot().slice(16_380..16_390).unwrap(), plan(Encoding::UTF8, false)),
            vec![0xff; 10]
        );
        assert_eq!(saved(&target.snapshot(), plan(Encoding::UTF8, false)), vec![0xff; 40_000]);
    }

    #[test]
    fn decoded_foreign_valid_text_reencodes_and_resident_bytes_release_after_all_refs() {
        let fixture = Fixture::new();
        let source = fixture.decoded("😀\u{fffd}\\xFF".as_bytes(), Encoding::UTF8, 0);
        let mut target = Buffer::with_add_memory_limit(64);
        target.set_undo_memory_limit(0);
        target.insert_snapshot_range(0, &source.snapshot(), 0..source.len()).unwrap();
        let snapshot = target.snapshot();
        assert_eq!(target.add_memory_bytes(), snapshot.len() as usize);
        assert_eq!(saved(&snapshot, plan(Encoding::UTF16_BE, false)), encode("😀\u{fffd}\\xFF", Encoding::UTF16_BE));
        assert!(
            snapshot
                .source_chunks(0..snapshot.len())
                .unwrap()
                .all(|chunk| matches!(chunk.unwrap(), SourceChunk::Added(_)))
        );
        target.delete(0..target.len()).unwrap();
        assert_eq!(target.add_memory_bytes(), snapshot.len() as usize, "external snapshot retains capped blocks");
        drop(snapshot);
        assert_eq!(target.add_memory_bytes(), 0);
    }

    #[test]
    fn decoded_sparse_lines_and_stream_requests_stay_bounded_after_edits() {
        let fixture = Fixture::new();
        let mut text = vec![b'x'; 300_003];
        for at in [0, 65_535, 65_536, 200_001, 300_002] {
            text[at] = b'\n';
        }
        let encoded = encode(std::str::from_utf8(&text).unwrap(), Encoding::UTF16_LE);
        let mut buffer = fixture.decoded(&encoded, Encoding::UTF16_LE, 0);
        buffer.insert(100_000, "é".as_bytes()).unwrap();
        let starts = [0, 1, 65_536, 65_537, 200_004, 300_005];
        for (line, start) in starts.into_iter().enumerate() {
            assert_eq!(buffer.line_start(line as u64).unwrap(), Some(start));
            assert_eq!(buffer.offset_to_line_col(start).unwrap(), Some((line as u64, 0)));
        }
        struct Bounded {
            bytes: usize,
        }
        impl Write for Bounded {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                assert!(bytes.len() <= 65536, "unbounded save request");
                self.bytes += bytes.len();
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut writer = Bounded { bytes: 0 };
        write_snapshot(&buffer.snapshot(), plan(Encoding::UTF16_LE, false), &mut writer).unwrap();
        assert_eq!(writer.bytes, encoded.len() + 2);
    }
}
