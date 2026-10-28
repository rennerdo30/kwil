use super::*;
use crate::encoding::utf32_tests::{Fixture, raw};

#[test]
fn utf32_backing_scalars_noncharacters_invalid_units_and_each_tail_keep_raw_partitions() {
    let fixture = Fixture::new();
    for encoding in [Encoding::UTF32_LE, Encoding::UTF32_BE] {
        for tail in [&[][..], &[0xaa][..], &[0xaa, 0xbb][..], &[0xaa, 0xbb, 0xcc][..]] {
            let bytes = raw(
                encoding,
                &[65, 0, 0x1f600, 0x10ffff, 0xfffe, 0xfeff, 0xd800, 0xdfff, 0x110000, 0xffffffff],
                true,
                tail,
            );
            let origin = fixture.origin(&bytes, encoding, 4);
            let mut expected = "A\0😀\u{10ffff}\u{fffe}\u{feff}".as_bytes().to_vec();
            expected.extend_from_slice(&[0xff; 4]);
            if !tail.is_empty() {
                expected.push(0xff);
            }
            assert_eq!(origin.logical_bytes(), expected);
            assert_eq!(origin.raw_bytes(), bytes);
            assert_eq!(origin.resolve_raw(1).unwrap(), Boundary::Bom { raw: 0..4 });
            let mut raw_at = 4;
            let mut logical_at = 0;
            for atom in origin.atoms(0..expected.len() as u64).unwrap() {
                let atom = atom.unwrap();
                assert_eq!(atom.raw.start, raw_at);
                assert_eq!(atom.logical.start, logical_at);
                assert_eq!(atom.raw_bytes, &bytes[atom.raw.start as usize..atom.raw.end as usize]);
                assert_eq!(
                    origin.resolve_logical(atom.logical.start).unwrap(),
                    Boundary::Exact { logical: logical_at, raw: raw_at }
                );
                for at in atom.raw.start + 1..atom.raw.end {
                    assert!(
                        matches!(origin.resolve_raw(at).unwrap(), Boundary::Interior { raw, .. } if raw == atom.raw)
                    );
                }
                raw_at = atom.raw.end;
                logical_at = atom.logical.end;
            }
            assert_eq!(raw_at, bytes.len() as u64);
            assert_eq!(logical_at, expected.len() as u64);
            assert_eq!(origin.raw_range(0..logical_at).unwrap(), 4..raw_at);
            assert!(origin.atom_at(logical_at).unwrap().is_none());
        }
    }
}

#[test]
fn utf32_backing_and_preview_cut_only_complete_units_at_64k_and_keep_real_tail() {
    let fixture = Fixture::new();
    for encoding in [Encoding::UTF32_LE, Encoding::UTF32_BE] {
        let mut units = vec![65; CHECKPOINT_RAW_BYTES / 4 - 1];
        units.extend_from_slice(&[0x1f600, 0xd800, 13, 10, 0x10ffff]);
        let bytes = raw(encoding, &units, true, &[0xbb, 0xcc, 0xdd]);
        let origin = fixture.origin(&bytes, encoding, 4);
        for i in 1..origin.checkpoint_count() {
            let distance = origin.checkpoint(i).0 - origin.checkpoint(i - 1).0;
            assert!(distance > 0 && distance <= CHECKPOINT_RAW_BYTES as u64);
            if i + 1 != origin.checkpoint_count() {
                assert_eq!(origin.checkpoint(i).0 % 4, 0);
            }
        }
        let preview =
            decode_preview(&fixture.file(&bytes), Interpretation::new(encoding, 4).unwrap(), &AtomicBool::new(false))
                .unwrap();
        assert_eq!(preview.source, 4..(CHECKPOINT_RAW_BYTES + 4) as u64);
        assert!(!preview.complete);
        assert!(preview.text.ends_with("😀".as_bytes()));
        let small = raw(encoding, &[0xfffd, 0xd800, 92, 120, 70, 70], false, &[0xaa]);
        let preview =
            decode_preview(&fixture.file(&small), Interpretation::new(encoding, 0).unwrap(), &AtomicBool::new(false))
                .unwrap();
        let escaped: String = raw(encoding, &[0xd800], false, &[]).iter().map(|b| format!("\\x{b:02X}")).collect();
        assert_eq!(preview.text, format!("�{escaped}\\xFF\\xAA").as_bytes());
        assert!(preview.complete);
    }
}

#[test]
fn utf32_resume_atom_pays_each_raw_byte_once_and_retains_pending_unit() {
    for encoding in [Encoding::UTF32_LE, Encoding::UTF32_BE] {
        for (bytes, kind, logical) in [
            (raw(encoding, &[0x1f600], false, &[]), AtomKind::Text, 4),
            (raw(encoding, &[0xd800], false, &[]), AtomKind::Opaque, 1),
            (vec![0xaa, 0xbb, 0xcc], AtomKind::Opaque, 1),
        ] {
            let cancel = AtomicBool::new(false);
            let mut atom = ResumeAtom::new(encoding);
            let mut accepted = 0;
            let mut size = None;
            for _ in 0..4 {
                let mut quota = 1;
                size = atom
                    .poll(&bytes, &cancel, &mut |n| {
                        assert_eq!(n, 1);
                        if quota == 0 {
                            return Ok(false);
                        }
                        quota -= 1;
                        accepted += 1;
                        Ok(true)
                    })
                    .unwrap();
                if size.is_some() {
                    break;
                }
            }
            let size = size.expect("one-byte admitted replay must finish without root replay");
            assert_eq!(size.raw, bytes.len());
            assert_eq!(size.logical, logical);
            assert_eq!(size.kind, kind);
            assert_eq!(accepted, bytes.len());
            let mut atom = ResumeAtom::new(encoding);
            assert!(atom.poll(&bytes, &cancel, &mut |_| Ok(false)).unwrap().is_none());
            assert!(matches!(
                atom.poll(&bytes, &AtomicBool::new(true), &mut |_| panic!("cancel before admission")),
                Err(BackingError::Cancelled)
            ));
        }
    }
}

#[test]
fn utf32_admitted_boundaries_match_unmetered_and_stop_before_denied_read() {
    let fixture = Fixture::new();
    for encoding in [Encoding::UTF32_LE, Encoding::UTF32_BE] {
        let bytes = raw(encoding, &[65, 0x1f600, 0xd800, 13, 10], true, &[0xaa, 0xbb]);
        let origin = fixture.origin(&bytes, encoding, 4);
        for at in 0..=origin.logical_bytes().len() as u64 {
            let expected = origin.resolve_logical(at).unwrap();
            let mut resolver = ResumeBoundary::new(&origin, at);
            let mut actual = None;
            for _ in 0..256 {
                let mut quota = 1;
                actual = resolver
                    .poll(&origin, &AtomicBool::new(false), &mut |n| {
                        assert!(n == RECORD || n == 1);
                        if quota == 0 {
                            return Ok(false);
                        }
                        quota -= 1;
                        Ok(true)
                    })
                    .unwrap();
                if actual.is_some() {
                    break;
                }
            }
            assert_eq!(actual, Some(expected));
        }
        let error = origin
            .raw_range_admitted(0..1, &AtomicBool::new(false), |_| {
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "UTF32 admission marker"))
            })
            .unwrap_err();
        assert!(
            matches!(error, BackingError::Io(e) if e.kind() == io::ErrorKind::PermissionDenied && e.to_string() == "UTF32 admission marker")
        );
    }
}

#[test]
fn utf32_reader_framing_short_reads_and_retirement_keep_the_same_origin() {
    let fixture = Fixture::new();
    for encoding in [Encoding::UTF32_LE, Encoding::UTF32_BE] {
        let bytes = raw(encoding, &[65, 0xd800, 0x1f600], true, &[0xaa, 0xbb, 0xcc]);
        struct ByteReader<'a>(&'a [u8]);
        impl Read for ByteReader<'_> {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                let count = output.len().min(self.0.len()).min(1);
                output[..count].copy_from_slice(&self.0[..count]);
                self.0 = &self.0[count..];
                Ok(count)
            }
        }
        let mut framed = bytes.clone();
        framed.extend_from_slice(b"next frame");
        let mut input = ByteReader(&framed);
        let owner = DecodedBacking::prepare_from_reader(
            &mut input,
            bytes.len() as u64,
            Interpretation::new(encoding, 4).unwrap(),
            &fixture.directory,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(input.0, b"next frame");
        let snapshot = owner.snapshot();
        assert_eq!(snapshot.raw_bytes(), bytes);
        assert_eq!(snapshot.logical_bytes(), b"A\xff\xf0\x9f\x98\x80\xff");
        let owner = match owner.try_retire() {
            Err(RetirementError::LiveSnapshots(owner)) => owner,
            _ => panic!("live UTF32 snapshot must retain its backing"),
        };
        drop(snapshot);
        assert!(owner.try_retire().is_ok());
        assert!(matches!(
            DecodedBacking::prepare_from_reader(
                &mut &bytes[..],
                bytes.len() as u64,
                Interpretation::new(encoding, 4).unwrap(),
                &fixture.directory,
                &AtomicBool::new(true)
            ),
            Err(BackingError::Cancelled)
        ));
    }
}
