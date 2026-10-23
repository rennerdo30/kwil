//! Length-preserving interpretation of an already replayed raw-v1 prefix.
//! No BOM sniffing: valid UTF-8 remains text, every invalid source byte is
//! independently typed opaque. Only an explicit finish marks actual EOF.

use std::io;

pub(crate) enum Span<'a> {
    Text(&'a [u8]),
    Opaque(u8),
}

#[derive(Default)]
pub(crate) struct RawUtf8 {
    pending: [u8; 4],
    len: usize,
}

impl RawUtf8 {
    pub(crate) fn feed(
        &mut self,
        mut bytes: &[u8],
        emit: &mut impl FnMut(Span<'_>) -> io::Result<()>,
    ) -> io::Result<()> {
        while self.len != 0 && !bytes.is_empty() {
            self.pending[self.len] = bytes[0];
            self.len += 1;
            bytes = &bytes[1..];
            loop {
                match std::str::from_utf8(&self.pending[..self.len]) {
                    Ok(_) => {
                        emit(Span::Text(&self.pending[..self.len]))?;
                        self.len = 0;
                        break;
                    }
                    Err(error) => {
                        if error.error_len().is_none() {
                            debug_assert!(self.len <= 3);
                            break;
                        }
                        // The carry has no valid prefix. Consuming one invalid
                        // byte at a time preserves historical raw coordinates.
                        emit(Span::Opaque(self.pending[0]))?;
                        self.pending.copy_within(1..self.len, 0);
                        self.len -= 1;
                        if self.len == 0 {
                            break;
                        }
                    }
                }
            }
        }
        while !bytes.is_empty() {
            match std::str::from_utf8(bytes) {
                Ok(_) => {
                    emit(Span::Text(bytes))?;
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    if valid != 0 {
                        emit(Span::Text(&bytes[..valid]))?;
                    }
                    bytes = &bytes[valid..];
                    if let Some(count) = error.error_len() {
                        for &byte in &bytes[..count] {
                            emit(Span::Opaque(byte))?;
                        }
                        bytes = &bytes[count..];
                    } else {
                        debug_assert!(bytes.len() <= 3);
                        self.pending[..bytes.len()].copy_from_slice(bytes);
                        self.len = bytes.len();
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) fn finish(self, emit: &mut impl FnMut(Span<'_>) -> io::Result<()>) -> io::Result<()> {
        for &byte in &self.pending[..self.len] {
            emit(Span::Opaque(byte))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_utf8_every_split_preserves_bytes_and_individual_invalid_atoms() {
        let raw = b"\xef\xbb\xbfA\xc2\xa2\xe2\x82\xac\xf0\x90\x80\x80\xff\xc2\xc2\xf0\x90Z";
        let expected_opaque = [0xff, 0xc2, 0xc2, 0xf0, 0x90];
        for split in 0..=raw.len() {
            let mut decoder = RawUtf8::default();
            let mut exact = Vec::new();
            let mut opaque = Vec::new();
            let mut emit = |span: Span<'_>| {
                match span {
                    Span::Text(bytes) => exact.extend_from_slice(bytes),
                    Span::Opaque(byte) => {
                        exact.push(byte);
                        opaque.push(byte);
                    }
                }
                Ok(())
            };
            decoder.feed(&raw[..split], &mut emit).unwrap();
            decoder.feed(b"", &mut emit).unwrap(); // Empty chunks are not EOF.
            decoder.feed(&raw[split..], &mut emit).unwrap();
            decoder.finish(&mut emit).unwrap();
            assert_eq!(exact, raw, "split {split}");
            assert_eq!(opaque, expected_opaque, "split {split}");
        }
    }
}
