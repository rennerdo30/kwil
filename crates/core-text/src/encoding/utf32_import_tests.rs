#[test]
fn utf32_preserved_validation_uses_shared_decoder_and_rejects_forged_endpoints() {
    use crate::encoding::utf32_tests::raw;
    for encoding in [Encoding::UTF32_LE, Encoding::UTF32_BE] {
        let logical = "😀".as_bytes();
        let bytes = raw(encoding, &[0x1f600], false, &[]);
        let mut offsets = Vec::new();
        offsets.extend_from_slice(&0_u32.to_le_bytes());
        for _ in 1..4 {
            offsets.extend_from_slice(&ATOM_INTERIOR.to_le_bytes());
        }
        offsets.extend_from_slice(&4_u32.to_le_bytes());
        validate_recovery_preserved(encoding, logical, &bytes, &offsets, &mut || Ok(())).unwrap();
        let mut wrong = offsets.clone();
        wrong[4..8].copy_from_slice(&1_u32.to_le_bytes());
        assert!(validate_recovery_preserved(encoding, logical, &bytes, &wrong, &mut || Ok(())).is_err());
        let malformed = raw(encoding, &[0xd800], false, &[]);
        assert!(validate_recovery_preserved(encoding, logical, &malformed, &offsets, &mut || Ok(())).is_err());
        let mut visits = 0;
        let error = validate_recovery_preserved(encoding, logical, &bytes, &offsets, &mut || {
            visits += 1;
            if visits == 4 { Err(io::Error::new(io::ErrorKind::Interrupted, "UTF32 import cancel")) } else { Ok(()) }
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    }
}
