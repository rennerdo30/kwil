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
    validate_snapshot_save_cancellable(snapshot, plan, || false)
}

/// Worker preflight with cooperative cancellation, including empty snapshots.
/// The callback is polled between bounded decode/write chunks and can simply
/// read the worker's cancellation flag. It is also polled during sink output.
pub fn validate_snapshot_save_cancellable(
    snapshot: &Snapshot,
    plan: SavePlan,
    mut cancelled: impl FnMut() -> bool,
) -> Result<(), EncodingError> {
    stream(snapshot, plan, &mut io::sink(), &mut cancelled)
}

/// All character, malformed-source and endpoint failures occur before the
/// first writer call. Disk failure during writing is the atomic-save envelope's
/// responsibility. Scratch and writer requests are bounded, regardless of size.
pub fn write_snapshot(snapshot: &Snapshot, plan: SavePlan, writer: &mut impl Write) -> Result<(), EncodingError> {
    write_snapshot_cancellable(snapshot, plan, writer, || false)
}

/// Cancellable preflight followed by cancellable output. Character failures
/// and cancellation during preflight leave the destination writer untouched;
/// cancellation during output belongs to the atomic envelope's rollback path.
pub fn write_snapshot_cancellable(
    snapshot: &Snapshot,
    plan: SavePlan,
    writer: &mut impl Write,
    mut cancelled: impl FnMut() -> bool,
) -> Result<(), EncodingError> {
    stream(snapshot, plan, &mut io::sink(), &mut cancelled)?;
    stream(snapshot, plan, writer, &mut cancelled)
}

fn poll(cancelled: &mut impl FnMut() -> bool) -> Result<(), EncodingError> {
    if cancelled() { Err(EncodingError::Cancelled) } else { Ok(()) }
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

fn raw(
    output: &mut TextOutput<'_, impl Write>,
    bytes: &[u8],
    cancelled: &mut impl FnMut() -> bool,
) -> Result<(), EncodingError> {
    poll(cancelled)?;
    if output.pending_cr {
        output.encode(output.plan.line_ending.unwrap().text())?;
        output.pending_cr = false;
    }
    for chunk in bytes.chunks(64 << 10) {
        poll(cancelled)?;
        output.writer.write_all(chunk)?;
    }
    Ok(())
}

fn decoded(
    origin: &DecodedSnapshot,
    range: Range<u64>,
    output: &mut TextOutput<'_, impl Write>,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<(), EncodingError> {
    poll(cancelled)?;
    let start = endpoint(origin, range.start)?;
    poll(cancelled)?;
    let end = endpoint(origin, range.end)?;
    poll(cancelled)?;
    if output.plan.encoding != origin.interpretation().encoding {
        let mut input = Utf8Input::default();
        for chunk in origin.logical_bytes()[range.start as usize..range.end as usize].chunks(16 << 10) {
            poll(cancelled)?;
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
            poll(cancelled)?;
            raw(output, &origin.raw_bytes()[range.start as usize..range.end as usize], cancelled)?;
        } else {
            // Decode atoms only to recognize newline characters, preserving
            // every other atom's bytes, including malformed runs and aliases.
            let mut run = None;
            let mut next_poll = 0;
            for atom in origin.atoms(middle).map_err(backing_error)? {
                let atom = atom.map_err(backing_error)?;
                if atom.raw.end >= next_poll {
                    poll(cancelled)?;
                    next_poll = atom.raw.end.saturating_add(16 << 10);
                }
                if matches!(atom.content, AtomContent::Text("\r" | "\n")) {
                    if let Some(begin) = run.take() {
                        raw(output, &origin.raw_bytes()[begin..atom.raw.start as usize], cancelled)?;
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
                    raw(output, &origin.raw_bytes()[begin..atom.raw.end as usize], cancelled)?;
                }
            }
        }
    }
    if last < range.end {
        text(output, &origin.logical_bytes()[last as usize..range.end as usize])?;
    }
    Ok(())
}

fn preserved(
    encoding: crate::encoding::Encoding,
    bytes: &[u8],
    raw_bytes: &[u8],
    raw_offsets: &[u8],
    output: &mut TextOutput<'_, impl Write>,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<(), EncodingError> {
    poll(cancelled)?;
    let valid = std::str::from_utf8(bytes).map_err(|_| EncodingError::MalformedSource)?;
    if output.plan.encoding != encoding {
        return output.feed(valid);
    }
    if output.plan.line_ending.is_none() {
        return raw(output, raw_bytes, cancelled);
    }
    let offset = |at: usize| u32::from_le_bytes(raw_offsets[at * 4..at * 4 + 4].try_into().unwrap()) as usize;
    let base = offset(0);
    let mut first = 0;
    // Newline scalars always have exact atom endpoints, even if an adjacent
    // legacy atom decodes to several scalars. Never inspect raw byte values
    // to guess whether a legacy trail byte is a newline.
    for at in memchr::memchr2_iter(b'\r', b'\n', bytes) {
        poll(cancelled)?;
        if first < at {
            raw(output, &raw_bytes[offset(first) - base..offset(at) - base], cancelled)?;
        }
        text(output, &bytes[at..at + 1])?;
        first = at + 1;
    }
    if first < bytes.len() {
        raw(output, &raw_bytes[offset(first) - base..offset(bytes.len()) - base], cancelled)?;
    }
    Ok(())
}

fn stream(
    snapshot: &Snapshot,
    plan: SavePlan,
    writer: &mut impl Write,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<(), EncodingError> {
    poll(cancelled)?;
    if plan.bom && plan.encoding.bom().is_empty() {
        return Err(EncodingError::UnsupportedEncoding(format!("BOM for {}", plan.encoding.name())));
    }
    if plan.bom {
        writer.write_all(plan.encoding.bom())?;
    }
    let mut output = TextOutput::new(plan, writer);
    let mut input = Utf8Input::default();
    for chunk in snapshot.source_chunks(0..snapshot.len())? {
        poll(cancelled)?;
        match chunk? {
            SourceChunk::Decoded { origin, range, .. } => {
                input.finish()?;
                decoded(origin, range, &mut output, cancelled)?;
            }
            SourceChunk::Opaque { encoding, raw_bytes, .. } => {
                input.finish()?;
                if plan.encoding != encoding {
                    return Err(EncodingError::MalformedSource);
                }
                raw(&mut output, raw_bytes, cancelled)?;
            }
            SourceChunk::Preserved { encoding, bytes, raw_bytes, raw_offsets } => {
                input.finish()?;
                preserved(encoding, bytes, raw_bytes, raw_offsets, &mut output, cancelled)?;
            }
            SourceChunk::Original { bytes, .. } | SourceChunk::Added(bytes) => {
                for chunk in bytes.chunks(16 << 10) {
                    poll(cancelled)?;
                    input.feed(chunk, |s| output.feed(s))?;
                }
            }
        }
    }
    input.finish()?;
    poll(cancelled)?;
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
    fn decoded_big5_same_codec_partial_start_is_unmappable_before_writer() {
        let fixture = Fixture::new();
        let encoding = Encoding::for_label("big5").unwrap();
        for (atom, character) in
            [(b"\x88\x62", '\u{304}'), (b"\x88\x64", '\u{30c}'), (b"\x88\xa3", '\u{304}'), (b"\x88\xa5", '\u{30c}')]
        {
            let mut raw = atom.to_vec();
            raw.extend_from_slice(b"tail");
            let buffer = fixture.decoded(&raw, encoding, 0);
            let partial = buffer.snapshot().slice(2..buffer.len()).unwrap();
            // A leading valid piece ensures a missing preflight would touch
            // the writer before reaching the partial expansion.
            let mut prefix = Buffer::empty();
            prefix.insert(0, b"prefix").unwrap();
            let snapshot = prefix.snapshot().concat(&partial).unwrap();
            assert!(matches!(validate_snapshot_save(&snapshot, plan(encoding, false)),
                Err(EncodingError::Unmappable { character: actual, encoding: "Big5" }) if actual == character));
            let mut writer = Untouched::default();
            assert!(matches!(write_snapshot(&snapshot, plan(encoding, false), &mut writer),
                Err(EncodingError::Unmappable { character: actual, encoding: "Big5" }) if actual == character));
            assert_eq!(writer.calls, 0);
        }
    }

    #[test]
    fn decoded_big5_same_codec_partial_end_is_unmappable_before_writer() {
        let fixture = Fixture::new();
        let encoding = Encoding::for_label("big5").unwrap();
        for (atom, character) in [(b"\x88\x62", 'Ê'), (b"\x88\x64", 'Ê'), (b"\x88\xa3", 'ê'), (b"\x88\xa5", 'ê')] {
            let mut raw = b"prefix".to_vec();
            raw.extend_from_slice(atom);
            let buffer = fixture.decoded(&raw, encoding, 0);
            let snapshot = buffer.snapshot().slice(0..8).unwrap(); // prefix + first two-byte scalar
            assert!(matches!(validate_snapshot_save(&snapshot, plan(encoding, false)),
                Err(EncodingError::Unmappable { character: actual, encoding: "Big5" }) if actual == character));
            let mut writer = Untouched::default();
            assert!(matches!(write_snapshot(&snapshot, plan(encoding, false), &mut writer),
                Err(EncodingError::Unmappable { character: actual, encoding: "Big5" }) if actual == character));
            assert_eq!(writer.calls, 0);
        }
    }

    #[test]
    fn decoded_foreign_alternating_spans_pack_resources_and_charge_metadata() {
        let fixture = Fixture::new();
        let bytes = b"A\xff".repeat(16);
        let source = fixture.decoded(&bytes, Encoding::UTF8, 0);
        let mut target = Buffer::with_add_memory_limit(0);
        target.set_undo_memory_limit(0);
        target.insert_snapshot_range(0, &source.snapshot(), 0..source.len()).unwrap();
        let snapshot = target.snapshot();
        let resources = snapshot.storage_resources();
        let paths = crate::piece_tree::spill_paths(&snapshot.root);
        assert_eq!(paths.len(), 3, "count actual backing files/maps/handles, not only cached metrics");
        assert!(paths.iter().all(|path| path.exists()));
        assert_eq!(resources.tree_nodes, 1, "short mixed spans must share one packed leaf");
        assert_eq!(resources.blocks, 3);
        assert_eq!(resources.spilled_blocks, 3, "one logical/raw/table backing, not four files per pair");
        assert_eq!(target.add_memory_bytes(), 0);
        assert_eq!(saved(&snapshot, plan(Encoding::UTF8, false)), bytes);
        let mut resident = Buffer::with_add_memory_limit(1024);
        resident.set_undo_memory_limit(0);
        resident.insert_snapshot_range(0, &source.snapshot(), 0..source.len()).unwrap();
        let payload = 32 + 16 + 4 * (32 + 1);
        assert_eq!(resident.add_memory_bytes(), payload, "all logical/raw/table bytes are budgeted");
        assert!(resident.snapshot().retained_bytes() > payload, "resource metadata is charged too");
        assert_eq!(resident.snapshot().storage_resources().spilled_blocks, 0);
        // Clip and reimport mixed storage under a foreign zero cap. Check raw
        // endpoints after both valid and opaque bytes, plus line metrics.
        let clip = resident.snapshot().slice(1..7).unwrap();
        let mut foreign = Buffer::with_add_memory_limit(0);
        foreign.insert_snapshot_range(0, &clip, 0..clip.len()).unwrap();
        assert_eq!(saved(&foreign.snapshot(), plan(Encoding::UTF8, false)), bytes[1..7]);
        assert_eq!(foreign.snapshot().storage_resources().spilled_blocks, 3);
    }

    #[test]
    fn decoded_foreign_alternating_spans_use_one_monotonic_origin_cursor() {
        let fixture = Fixture::new();
        let bytes = b"A\xff".repeat(16);
        let source = fixture.decoded(&bytes, Encoding::UTF8, 0);
        let mut target = Buffer::with_add_memory_limit(1024);
        target.insert_snapshot_range(0, &source.snapshot(), 0..source.len()).unwrap();
        let work = target.import_work();
        assert_eq!(work.streams, 1, "checkpoint replay must not restart for each opaque run");
        assert_eq!(work.atoms, 32);
        assert_eq!(work.raw_bytes, bytes.len() as u64);
        assert_eq!(work.repeated_raw_bytes, 0);
        assert_eq!(saved(&target.snapshot(), plan(Encoding::UTF8, false)), bytes);
    }

    #[test]
    fn decoded_foreign_mixed_table_boundary_keeps_scalar_stream_and_line_metrics() {
        let fixture = Fixture::new();
        // 18KiB crosses one packed table boundary, inside a four-byte scalar.
        let bytes = b"\xf0\x9f\x98\x80\n\xff".repeat(3000);
        let source = fixture.decoded(&bytes, Encoding::UTF8, 0);
        let mut target = Buffer::with_add_memory_limit(0);
        target.set_undo_memory_limit(0);
        target.insert_snapshot_range(0, &source.snapshot(), 0..source.len()).unwrap();
        assert_eq!(target.snapshot().storage_resources().blocks, 6);
        assert_eq!(target.line_count(), 3001);
        assert_eq!(target.line_start(3000).unwrap(), Some(17_999));
        // The previous LF starts this line at 16379; its opaque atom at 16379
        // occupies the first byte column before the next four-byte scalar.
        assert_eq!(target.offset_to_line_col(16_383).unwrap(), Some((2730, 4)));
        assert_eq!(target.offset_to_line_col(17_999).unwrap(), Some((3000, 0)));
        assert_eq!(saved(&target.snapshot(), plan(Encoding::UTF8, false)), bytes);
        let source_snapshot = target.snapshot();
        target.delete(0..target.len()).unwrap();
        target.insert_snapshot_range(0, &source_snapshot, 0..source_snapshot.len()).unwrap();
        assert!(Arc::ptr_eq(target.snapshot().root.as_ref().unwrap(), source_snapshot.root.as_ref().unwrap()));
        assert_eq!(target.add_memory_bytes(), 0);
    }

    #[test]
    fn snapshot_cancellation_includes_empty_snapshots_and_decoded_spans() {
        let fixture = Fixture::new();
        let empty = fixture.decoded(b"", Encoding::UTF8, 0);
        for snapshot in [Snapshot::default(), empty.snapshot(), empty.snapshot().slice(0..0).unwrap()] {
            assert!(matches!(
                validate_snapshot_save_cancellable(&snapshot, plan(Encoding::UTF8, false), || true),
                Err(EncodingError::Cancelled)
            ));
            let mut writer = Untouched::default();
            assert!(matches!(
                write_snapshot_cancellable(&snapshot, plan(Encoding::UTF16_LE, true), &mut writer, || true),
                Err(EncodingError::Cancelled)
            ));
            assert_eq!(writer.calls, 0);
        }
    }

    #[test]
    fn snapshot_cancellation_polls_inside_all_full_preflight_paths() {
        let fixture = Fixture::new();
        let text = "x".repeat(70_000);
        for (source_encoding, target_encoding, ending) in [
            (Encoding::UTF8, Encoding::UTF8, None),
            (Encoding::UTF16_LE, Encoding::UTF8, None),
            (Encoding::UTF8, Encoding::UTF8, Some(LineEnding::CrLf)),
        ] {
            let source = fixture.decoded(&encode(&text, source_encoding), source_encoding, 0);
            let mut polls = 0;
            let mut writer = Untouched::default();
            let mut target = plan(target_encoding, false);
            target.line_ending = ending;
            let result = write_snapshot_cancellable(&source.snapshot(), target, &mut writer, || {
                polls += 1;
                polls == 7
            });
            assert!(matches!(result, Err(EncodingError::Cancelled)), "{source_encoding:?} {ending:?}: {result:?}");
            assert_eq!(polls, 7);
            assert_eq!(writer.calls, 0, "full sink preflight must remain cancellable");
        }
        let mut ordinary = Buffer::empty();
        ordinary.insert(0, text.as_bytes()).unwrap();
        let mut polls = 0;
        let mut writer = Untouched::default();
        assert!(matches!(
            write_snapshot_cancellable(&ordinary.snapshot(), plan(Encoding::UTF8, false), &mut writer, || {
                polls += 1;
                polls == 4
            }),
            Err(EncodingError::Cancelled)
        ));
        assert_eq!(writer.calls, 0);
    }

    #[test]
    fn snapshot_cancellation_during_output_stops_at_bounded_writer_chunks() {
        let fixture = Fixture::new();
        let source = fixture.decoded(&vec![b'x'; 70_000], Encoding::UTF8, 0);
        let cancelled = Cell::new(false);
        struct CancelWriter<'a> {
            flag: &'a Cell<bool>,
            bytes: usize,
        }
        impl Write for CancelWriter<'_> {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                assert!(bytes.len() <= 65536);
                self.bytes += bytes.len();
                self.flag.set(true);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut writer = CancelWriter { flag: &cancelled, bytes: 0 };
        assert!(matches!(
            write_snapshot_cancellable(&source.snapshot(), plan(Encoding::UTF8, false), &mut writer, || cancelled
                .get()),
            Err(EncodingError::Cancelled)
        ));
        assert!(writer.bytes > 0 && writer.bytes <= 65536);
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
    fn ordinary_import_big5_alias_survives_foreign_retirement_and_charges_all_payloads() {
        let encoding = Encoding::for_label("big5").unwrap();
        for (limit, resident, spills) in [(0, 0, 3), (1, 0, 3), (8, 7, 1), (31, 31, 0)] {
            let fixture = Fixture::new();
            let source = fixture.decoded(b"\x88\x62Z", encoding, 0);
            let mut target = Buffer::with_add_memory_limit(limit);
            target.set_undo_memory_limit(0);
            let prepared = target.snapshot_importer().prepare(&source.snapshot(), 0..source.len()).unwrap();
            assert_eq!(target.add_memory_bytes(), resident, "logical 5 + raw 2 + six u32 endpoints");
            assert_eq!(prepared.storage_resources().blocks, 3);
            assert_eq!(prepared.storage_resources().spilled_blocks, spills);
            target.insert_snapshot_range(0, &prepared, 0..prepared.len()).unwrap();
            assert!(Arc::ptr_eq(target.snapshot().root.as_ref().unwrap(), prepared.root.as_ref().unwrap()));
            assert_eq!(target.add_memory_bytes(), resident, "inserting prepared storage must not recharge it");
            drop(source);
            assert!(fixture.owners.borrow_mut().pop().unwrap().try_retire().is_ok());
            let snapshot = target.snapshot();
            assert!(snapshot.originals.is_empty(), "no foreign origin or whole-source pin survives");
            assert_eq!(saved(&snapshot, plan(encoding, false)), b"\x88\x62Z");
            assert_eq!(saved(&snapshot, plan(Encoding::UTF8, false)), "Ê\u{304}Z".as_bytes());
            assert_eq!(saved(&snapshot, plan(Encoding::UTF16_LE, false)), encode("Ê\u{304}Z", Encoding::UTF16_LE));
            let chunks: Vec<_> = snapshot.source_chunks(0..snapshot.len()).unwrap().map(Result::unwrap).collect();
            let SourceChunk::Preserved { bytes, raw_bytes, raw_offsets, .. } = &chunks[0] else {
                panic!("valid alias must retain typed valid provenance");
            };
            assert_eq!(*bytes, "Ê\u{304}".as_bytes());
            assert_eq!(*raw_bytes, b"\x88\x62");
            let entries: Vec<_> =
                raw_offsets.as_chunks::<4>().0.iter().map(|entry| u32::from_le_bytes(*entry)).collect();
            assert_eq!(entries, [0, 1 << 31, 1 << 31, 1 << 31, 2]);
            assert!(matches!(&chunks[1], SourceChunk::Added(bytes) if *bytes == b"Z"));
            target.delete(0..target.len()).unwrap();
            drop(prepared);
            assert_eq!(target.add_memory_bytes(), resident, "external snapshot owns all three payloads");
            drop(chunks);
            drop(snapshot);
            assert_eq!(target.add_memory_bytes(), 0);
        }
    }

    #[test]
    fn ordinary_import_alias_partial_scalars_and_foreign_reimport_never_borrow_raw_endpoints() {
        let fixture = Fixture::new();
        let encoding = Encoding::for_label("big5").unwrap();
        let source = fixture.decoded(b"\x88\x62Z", encoding, 0);
        let owner = Buffer::with_add_memory_limit(128);
        let imported = owner.snapshot_importer().prepare(&source.snapshot(), 0..source.len()).unwrap();
        for source in [source.snapshot(), imported.clone()] {
            for range in [0..2, 2..4, 2..5] {
                let target = Buffer::with_add_memory_limit(0);
                let partial = target.snapshot_importer().prepare(&source, range.clone()).unwrap();
                let expected = &"Ê\u{304}Z".as_bytes()[range.start as usize..range.end as usize];
                assert_eq!(saved(&partial, plan(Encoding::UTF8, false)), expected);
                assert!(
                    partial
                        .source_chunks(0..partial.len())
                        .unwrap()
                        .all(|chunk| matches!(chunk.unwrap(), SourceChunk::Added(_)))
                );
                let mut writer = Untouched::default();
                assert!(matches!(
                    write_snapshot(&partial, plan(encoding, false), &mut writer),
                    Err(EncodingError::Unmappable { .. })
                ));
                assert_eq!(writer.calls, 0);
            }
            for range in [1..4, 0..3] {
                let target = Buffer::with_add_memory_limit(0);
                assert!(target.snapshot_importer().prepare(&source, range).is_err());
                assert_eq!(target.add_memory_bytes(), 0);
            }
        }
        let second = Buffer::with_add_memory_limit(0);
        let reimported = second.snapshot_importer().prepare(&imported, 0..imported.len()).unwrap();
        drop((source, imported));
        assert!(fixture.owners.borrow_mut().pop().unwrap().try_retire().is_ok());
        assert_eq!(saved(&reimported, plan(encoding, false)), b"\x88\x62Z");
        // Slicing a destination block must obey the same partial-atom rule.
        assert_eq!(saved(&reimported.slice(2..4).unwrap(), plan(Encoding::UTF8, false)), "\u{304}".as_bytes());
        let mut writer = Untouched::default();
        assert!(matches!(
            write_snapshot(&reimported.slice(2..4).unwrap(), plan(encoding, false), &mut writer),
            Err(EncodingError::Unmappable { .. })
        ));
        assert_eq!(writer.calls, 0);
    }

    #[test]
    fn ordinary_import_alias_malformed_and_newlines_keep_independent_contracts() {
        let fixture = Fixture::new();
        let encoding = Encoding::for_label("big5").unwrap();
        let source = fixture.decoded(b"\x88\x62\r\n\xffZ", encoding, 0);
        let mut target = Buffer::with_add_memory_limit(0);
        target.set_undo_memory_limit(0);
        target.insert_snapshot_range(0, &source.snapshot(), 0..source.len()).unwrap();
        drop(source);
        assert!(fixture.owners.borrow_mut().pop().unwrap().try_retire().is_ok());
        let snapshot = target.snapshot();
        assert_eq!(saved(&snapshot, plan(encoding, false)), b"\x88\x62\r\n\xffZ");
        let mut normalized = plan(encoding, false);
        normalized.line_ending = Some(LineEnding::Lf);
        assert_eq!(saved(&snapshot, normalized), b"\x88\x62\n\xffZ");
        let mut writer = Untouched::default();
        assert!(matches!(
            write_snapshot(&snapshot, plan(Encoding::UTF8, false), &mut writer),
            Err(EncodingError::MalformedSource)
        ));
        assert_eq!(writer.calls, 0);
        assert_eq!(target.line_count(), 2);
        assert_eq!(target.line_start(1).unwrap(), Some(6));
        target.begin();
        target.delete(0..4).unwrap();
        target.cancel();
        assert_eq!(saved(&target.snapshot(), plan(encoding, false)), b"\x88\x62\r\n\xffZ");
    }

    #[test]
    fn ordinary_import_alias_blocks_bound_payloads_and_explicit_retirement() {
        let fixture = Fixture::new();
        let encoding = Encoding::for_label("big5").unwrap();
        let raw = b"\x88\x62".repeat(4096);
        let source = fixture.decoded(&raw, encoding, 0);
        let target = Buffer::with_add_memory_limit(0);
        let snapshot = target.snapshot_importer().prepare(&source.snapshot(), 0..source.len()).unwrap();
        let paths = crate::piece_tree::spill_paths(&snapshot.root);
        assert_eq!(snapshot.storage_resources().blocks, 6, "atom packing leaves no split alias at the table boundary");
        assert_eq!(paths.len(), 6);
        for chunk in snapshot.source_chunks(0..snapshot.len()).unwrap() {
            let SourceChunk::Preserved { bytes, raw_bytes, raw_offsets, .. } = chunk.unwrap() else {
                panic!("alias lost");
            };
            assert!(bytes.len() < 16384 && raw_bytes.len() <= 65536 && raw_offsets.len() <= 65536);
            assert_eq!(raw_offsets.len(), (bytes.len() + 1) * 4);
        }
        drop(source);
        assert!(fixture.owners.borrow_mut().pop().unwrap().try_retire().is_ok());
        assert_eq!(saved(&snapshot, plan(encoding, false)), raw);
        let limits =
            crate::SpillRetirementLimits { max_block_references: 6, max_metadata_bytes: 1 << 20, max_path_bytes: 4096 };
        assert!(
            matches!(snapshot.retain_spills(limits, || true), Err(error) if error.kind() == io::ErrorKind::Interrupted)
        );
        let mut retirement = snapshot.retain_spills(limits, || false).unwrap();
        assert_eq!(retirement.retire().live, 6);
        drop(snapshot);
        assert!(paths.iter().all(|path| path.exists()), "last snapshot drop cannot unlink explicitly leased payloads");
        let report = retirement.retire();
        assert_eq!((report.retired, report.live, report.cleanup_pending), (6, 0, 0));
        assert!(paths.iter().all(|path| !path.exists()));
    }

    #[test]
    fn ordinary_import_alias_save_preflight_stays_cancellable() {
        let fixture = Fixture::new();
        let encoding = Encoding::for_label("big5").unwrap();
        let source = fixture.decoded(b"\x88\x62Z", encoding, 0);
        let target = Buffer::with_add_memory_limit(0);
        let snapshot = target.snapshot_importer().prepare(&source.snapshot(), 0..source.len()).unwrap();
        for target_encoding in [encoding, Encoding::UTF8] {
            let mut writer = Untouched::default();
            let mut polls = 0;
            assert!(matches!(
                write_snapshot_cancellable(&snapshot, plan(target_encoding, false), &mut writer, || {
                    polls += 1;
                    polls == 3
                }),
                Err(EncodingError::Cancelled)
            ));
            assert_eq!(writer.calls, 0);
        }
    }

    #[test]
    fn ordinary_import_alias_legacy_encoded_file_refuses_typed_valid_source() {
        let fixture = Fixture::new();
        let encoding = Encoding::for_label("big5").unwrap();
        let source = fixture.decoded(b"\x88\x62Z", encoding, 0);
        let target = Buffer::with_add_memory_limit(0);
        let imported = target.snapshot_importer().prepare(&source.snapshot(), 0..source.len()).unwrap();
        let path = fixture.directory.join("legacy-raw-api");
        std::fs::write(&path, b"plain").unwrap();
        let old = crate::encoding::EncodedFile::new(TextFile::open(&path).unwrap()).unwrap();
        let mut writer = Untouched::default();
        assert!(matches!(old.write_to(&imported, old.save_plan(), &mut writer), Err(EncodingError::WrongSource)));
        assert_eq!(writer.calls, 0);
    }

    #[test]
    fn ordinary_alias_reimport_into_tracked_worker_keeps_capacity_and_retirement() {
        use crate::worker_edit::{WorkerDepotLimits, WorkerEditDepot, WorkerEditPlan};
        let fixture = Fixture::new();
        let encoding = Encoding::for_label("big5").unwrap();
        let source = fixture.decoded(b"\x88\x62Z", encoding, 0);
        let first = Buffer::with_add_memory_limit(128);
        let ordinary = first.snapshot_importer().prepare(&source.snapshot(), 0..source.len()).unwrap();
        drop(source);
        assert!(fixture.owners.borrow_mut().pop().unwrap().try_retire().is_ok());
        for mode in 0..3 {
            let target = Buffer::with_add_memory_limit(0);
            let mut depot =
                WorkerEditDepot::new(WorkerDepotLimits { disk_bytes: 4 << 20, metadata_bytes: 8 << 20, slots: 1 })
                    .unwrap();
            let baseline = depot.usage();
            let admission_plan = WorkerEditPlan {
                disk_bytes: 1 << 20,
                recipe_bytes: 0,
                metadata_bytes: 4 << 20,
                max_new_blocks: if mode == 1 { 2 } else { 3 },
                max_certificates: 8,
                max_tree_nodes: 128,
                max_path_bytes: 4096,
            };
            let admission = depot.admit(admission_plan).unwrap();
            let cancel = AtomicBool::new(false);
            let mut op = target
                .worker_edit_seed()
                .unwrap()
                .begin(admission, &mut depot, &cancel)
                .map_err(|refusal| refusal.error)
                .unwrap();
            if mode == 2 {
                cancel.store(true, Ordering::Relaxed);
            }
            let result = op.import(&ordinary, 0..ordinary.len(), &cancel);
            if mode == 0 {
                let imported = result.unwrap();
                let paths = crate::piece_tree::spill_paths(&imported.root);
                assert_eq!(paths.len(), 3);
                assert_eq!(target.add_memory_bytes(), 0);
                let (token, resources) = op.seal(imported.clone(), &cancel).unwrap();
                assert_eq!(saved(token.snapshot(), plan(encoding, false)), b"\x88\x62Z");
                let resources = resources.try_retire(&mut depot).expect_err("live token holds cleanup authority");
                drop((imported, token, op));
                assert!(resources.try_retire(&mut depot).is_ok());
                assert!(paths.iter().all(|path| !path.exists()));
            } else {
                assert_eq!(
                    result.err().unwrap().kind(),
                    if mode == 1 { io::ErrorKind::WouldBlock } else { io::ErrorKind::Interrupted }
                );
                assert!(op.reserve(0, 0).is_err(), "failed/cancelled import stays poisoned");
                let resources = op.rollback();
                drop(op);
                assert!(resources.try_retire(&mut depot).is_ok());
            }
            assert_eq!(depot.usage(), baseline, "all disk and metadata reservations return exactly");
            assert!(depot.try_close().is_ok());
        }
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
