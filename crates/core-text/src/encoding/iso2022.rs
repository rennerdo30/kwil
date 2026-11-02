//! ISO-2022-JP restart state at complete character boundaries. Mapping stays
//! with encoding_rs; only escape designation and scalar framing live here.
use super::{Decoder, EncodingError};
use encoding_rs::DecoderResult;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum State {
    #[default]
    Ascii,
    Roman,
    Katakana,
    Jis0208,
    Raw,
}

impl State {
    pub(crate) fn escape(self) -> &'static [u8] {
        match self {
            Self::Ascii | Self::Raw => b"\x1b(B",
            Self::Roman => b"\x1b(J",
            Self::Katakana => b"\x1b(I",
            Self::Jis0208 => b"\x1b$B",
        }
    }

    pub(crate) fn from_byte(byte: u8) -> Self {
        match byte {
            0 => Self::Ascii,
            1 => Self::Roman,
            2 => Self::Katakana,
            3 => Self::Jis0208,
            4 => Self::Raw,
            _ => unreachable!("private ISO checkpoint state"),
        }
    }

    pub(crate) fn decoder(self) -> Decoder {
        let mut decoder = Decoder::Rs(encoding_rs::ISO_2022_JP.new_decoder_without_bom_handling());
        if self != Self::Ascii && self != Self::Raw {
            // A checkpoint follows a scalar, with the ISO output flag clear.
            // Emit a throwaway scalar after designating the active charset.
            let dummy = if self == Self::Jis0208 { b"!!".as_slice() } else { b"!" };
            let mut scratch = [0; 16];
            decoder.decode_to_utf8_without_replacement(self.escape(), &mut scratch, false);
            decoder.decode_to_utf8_without_replacement(dummy, &mut scratch, false);
        }
        decoder
    }
}

pub(crate) struct Atom {
    pub raw: usize,
    pub text: [u8; 8],
    pub len: usize,
    pub state: State,
}

/// A valid ISO atom has at most one designation before a scalar. Consecutive
/// designations are errors in encoding_rs/WHATWG. Include a final designation
/// with the last scalar so an untouched save retains its otherwise empty tail.
pub(crate) fn atom(raw: &[u8], state: State, eof: bool) -> Result<Atom, EncodingError> {
    if raw.is_empty() {
        return Err(EncodingError::InvalidRange);
    }
    if state == State::Raw {
        return Ok(Atom { raw: 1, text: [0xff; 8], len: 1, state });
    }
    let mut decoder = state.decoder();
    let mut next = state;
    let mut text = [0; 8];
    let mut consumed = 0;
    while consumed < raw.len().min(8) {
        let (result, read, written) = decoder.decode_to_utf8_without_replacement(
            &raw[consumed..consumed + 1],
            &mut text,
            eof && consumed + 1 == raw.len(),
        );
        consumed += read;
        if matches!(result, DecoderResult::Malformed(..)) || read == 0 {
            return Err(EncodingError::MalformedSource);
        }
        if consumed == 3 && raw[0] == 0x1b {
            next = designation(&raw[..3]).ok_or(EncodingError::MalformedSource)?;
        }
        if written != 0 {
            if eof && raw.len() - consumed == 3 && designation(&raw[consumed..]).is_some() {
                let (result, read, count) =
                    decoder.decode_to_utf8_without_replacement(&raw[consumed..], &mut [0; 16], true);
                if result != DecoderResult::InputEmpty || count != 0 || read != 3 {
                    return Err(EncodingError::MalformedSource);
                }
                next = designation(&raw[consumed..]).unwrap();
                consumed += 3;
            }
            return Ok(Atom { raw: consumed, text, len: written, state: next });
        }
    }
    Err(EncodingError::MalformedSource)
}

fn designation(bytes: &[u8]) -> Option<State> {
    match bytes {
        b"\x1b(B" => Some(State::Ascii),
        b"\x1b(J" => Some(State::Roman),
        b"\x1b(I" => Some(State::Katakana),
        b"\x1b$B" | b"\x1b$@" => Some(State::Jis0208),
        _ => None,
    }
}
