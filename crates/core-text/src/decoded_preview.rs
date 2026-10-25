//! Read-only first-screen text; full lossless backing preparation follows it.
//! Escape strings here are presentation only, never editable model storage.

use super::*;

pub struct DecodedPreview {
    pub text: Vec<u8>,
    pub source: Range<u64>,
    pub complete: bool,
}

/// Worker-only bounded first block. Never builds a full index or private file.
/// Ends at a complete encoded atom, so a clipped prefix cannot invent malformed
/// input. Actual malformed bytes are displayed individually as ASCII escapes.
pub fn decode_preview(
    source: &TextFile,
    interpretation: Interpretation,
    cancel: &AtomicBool,
) -> Result<DecodedPreview, BackingError> {
    cancelled(cancel)?;
    Interpretation::new(interpretation.encoding, interpretation.bom_len as usize)?;
    check_source(source)?;
    let bytes = source.bytes()?;
    let start = interpretation.bom_len as usize;
    if start > bytes.len() || (start != 0 && !bytes.starts_with(interpretation.encoding.bom_bytes())) {
        return Err(BackingError::InvalidDescriptor);
    }
    let end = block_end(bytes, start, interpretation.encoding)?;
    let raw = &bytes[start..end];
    let mut decoder = interpretation.encoding.decoder();
    let mut scratch = [0; SCRATCH];
    let mut at = 0;
    let mut text = Vec::with_capacity(raw.len());
    loop {
        cancelled(cancel)?;
        let (result, read, written) = decoder.decode_to_utf8_without_replacement(&raw[at..], &mut scratch, true);
        at += read;
        text.extend_from_slice(&scratch[..written]);
        match result {
            DecoderResult::InputEmpty => break,
            DecoderResult::OutputFull => {}
            DecoderResult::Malformed(length, after) => {
                let finish = at - after as usize;
                for &byte in &raw[finish - length as usize..finish] {
                    const HEX: &[u8; 16] = b"0123456789ABCDEF";
                    text.extend_from_slice(&[b'\\', b'x', HEX[(byte >> 4) as usize], HEX[(byte & 15) as usize]]);
                }
            }
        }
    }
    check_source(source)?;
    Ok(DecodedPreview { text, source: start as u64..end as u64, complete: end == bytes.len() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn preview(raw: &[u8], interpretation: Interpretation) -> DecodedPreview {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "kwil-decoded-preview-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, raw).unwrap();
        let source = TextFile::open(&path).unwrap();
        let result = decode_preview(&source, interpretation, &AtomicBool::new(false)).unwrap();
        drop(source);
        std::fs::remove_file(path).unwrap();
        result
    }

    #[test]
    fn first_preview_decodes_utf16_and_exposes_original_malformed_bytes() {
        let result = preview(b"\xff\xfeA\0\0\xd8B\0\r\0\n\0", Interpretation::new(Encoding::UTF16_LE, 2).unwrap());
        assert_eq!(result.text, b"A\\x00\\xD8B\r\n");
        assert_eq!(result.source, 2..12);
        assert!(result.complete);
    }

    #[test]
    fn first_preview_never_finalizes_a_crossing_character_or_scans_the_tail() {
        let mut raw = vec![b'a'; CHECKPOINT_RAW_BYTES - 1];
        raw.extend_from_slice("界".as_bytes());
        raw.extend_from_slice(b"tail");
        let result = preview(&raw, Interpretation::new(Encoding::UTF8, 0).unwrap());
        assert_eq!(result.source.end, (CHECKPOINT_RAW_BYTES - 1) as u64);
        assert!(result.text.iter().all(|&b| b == b'a'));
        assert!(!result.complete);
        let malformed = preview(&vec![0xff; CHECKPOINT_RAW_BYTES + 1], Interpretation::new(Encoding::UTF8, 0).unwrap());
        assert_eq!(malformed.source.end, CHECKPOINT_RAW_BYTES as u64);
        assert_eq!(malformed.text.len(), 4 * CHECKPOINT_RAW_BYTES);
        assert!(malformed.text.as_chunks::<4>().0.iter().all(|s| s == b"\\xFF"));
    }
}
