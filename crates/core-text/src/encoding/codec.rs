//! One internal codec protocol for backing, atom replay, imports and saves.
//! The Rs branch delegates unchanged. Fixed-width Unicode state is inline;
//! OutputFull retains a completed unit and InputEmpty retains an incomplete one.

use super::{Codec, Encoding};
use encoding_rs::{DecoderResult, EncoderResult};

pub(crate) enum Decoder {
    Rs(encoding_rs::Decoder),
    Utf32 { encoding: Encoding, pending: [u8; 4], len: u8 },
}

impl Decoder {
    pub(super) fn new(encoding: Encoding) -> Self {
        match encoding.0 {
            Codec::Rs(codec) => Self::Rs(codec.new_decoder_without_bom_handling()),
            Codec::Utf32Le | Codec::Utf32Be => Self::Utf32 { encoding, pending: [0; 4], len: 0 },
        }
    }

    pub(crate) fn is_neutral(&self) -> bool {
        match self {
            Self::Rs(decoder) => decoder.latin1_byte_compatible_up_to(b"").is_some(),
            Self::Utf32 { len, .. } => *len == 0,
        }
    }

    pub(crate) fn decode_to_utf8_without_replacement(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        last: bool,
    ) -> (DecoderResult, usize, usize) {
        let Self::Utf32 { encoding, pending, len } = self else {
            let Self::Rs(decoder) = self else { unreachable!() };
            return decoder.decode_to_utf8_without_replacement(input, output, last);
        };
        let mut read = 0;
        let mut written = 0;
        loop {
            while *len < 4 && read < input.len() {
                pending[*len as usize] = input[read];
                *len += 1;
                read += 1;
            }
            if *len != 4 {
                if last && *len != 0 {
                    let malformed = *len;
                    *len = 0;
                    return (DecoderResult::Malformed(malformed, 0), read, written);
                }
                return (DecoderResult::InputEmpty, read, written);
            }
            let Some(character) = char::from_u32(encoding.unit(pending, 0).unwrap()) else {
                *len = 0;
                return (DecoderResult::Malformed(4, 0), read, written);
            };
            if output.len() - written < character.len_utf8() {
                return (DecoderResult::OutputFull, read, written);
            }
            let count = character.encode_utf8(&mut output[written..]).len();
            written += count;
            *len = 0;
        }
    }
}

pub(crate) enum Encoder {
    Rs(encoding_rs::Encoder),
    Fixed(Encoding),
}

impl Encoder {
    pub(super) fn new(encoding: Encoding) -> Self {
        if encoding.fixed_width() != 1 {
            Self::Fixed(encoding)
        } else {
            let Codec::Rs(codec) = encoding.0 else { unreachable!() };
            Self::Rs(codec.new_encoder())
        }
    }

    pub(crate) fn encode_from_utf8_without_replacement(
        &mut self,
        text: &str,
        output: &mut [u8],
        last: bool,
    ) -> (EncoderResult, usize, usize) {
        let Self::Fixed(encoding) = self else {
            let Self::Rs(encoder) = self else { unreachable!() };
            return encoder.encode_from_utf8_without_replacement(text, output, last);
        };
        let mut written = 0;
        for (at, character) in text.char_indices() {
            let mut bytes = [0; 4];
            let count = if encoding.fixed_width() == 4 {
                bytes = if *encoding == Encoding::UTF32_LE {
                    (character as u32).to_le_bytes()
                } else {
                    (character as u32).to_be_bytes()
                };
                4
            } else {
                let mut units = [0; 2];
                let units = character.encode_utf16(&mut units);
                for (i, unit) in units.iter().enumerate() {
                    let pair = if *encoding == Encoding::UTF16_LE { unit.to_le_bytes() } else { unit.to_be_bytes() };
                    bytes[i * 2..i * 2 + 2].copy_from_slice(&pair);
                }
                units.len() * 2
            };
            if output.len() - written < count {
                return (EncoderResult::OutputFull, at, written);
            }
            output[written..written + count].copy_from_slice(&bytes[..count]);
            written += count;
        }
        (EncoderResult::InputEmpty, text.len(), written)
    }
}
