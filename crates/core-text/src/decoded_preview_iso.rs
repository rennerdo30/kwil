use super::*;
use crate::encoding::{DisplaySegment, EncodedFile};

pub(super) fn iso_preview(source: &TextFile, cancel: &AtomicBool) -> Result<DecodedPreview, BackingError> {
    cancelled(cancel)?;
    let file = EncodedFile::new_with_encoding(source.clone(), crate::encoding::Encoding::ISO2022_JP)?;
    let decoded = file.decode_range(0..source.len().min(CHECKPOINT_RAW_BYTES as u64))?;
    let mut text = Vec::new();
    for segment in decoded.segments {
        match segment {
            DisplaySegment::Text(s) => text.extend_from_slice(s.as_bytes()),
            DisplaySegment::Invalid(raw) => {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                for byte in raw.iter().copied() {
                    text.extend_from_slice(&[b'\\', b'x', HEX[(byte >> 4) as usize], HEX[(byte & 15) as usize]]);
                }
            }
        }
    }
    check_source(source)?;
    cancelled(cancel)?;
    Ok(DecodedPreview { text, complete: decoded.source.end == source.len(), source: decoded.source })
}
