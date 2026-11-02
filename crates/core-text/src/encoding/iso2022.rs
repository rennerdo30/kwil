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
            let _ = decoder.decode_to_utf8_without_replacement(self.escape(), &mut scratch, false);
            let _ = decoder.decode_to_utf8_without_replacement(dummy, &mut scratch, false);
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
pub(crate) fn frame(raw: &[u8], state: State, eof: bool) -> Result<(usize, State), EncodingError> {
    if raw.is_empty() {
        return Err(EncodingError::InvalidRange);
    }
    if state == State::Raw {
        return Ok((1, state));
    }
    let (prefix, mut next) = if raw[0] == 0x1b {
        (3, designation(raw.get(..3).ok_or(EncodingError::MalformedSource)?).ok_or(EncodingError::MalformedSource)?)
    } else {
        (0, state)
    };
    let mut count = prefix + if next == State::Jis0208 { 2 } else { 1 };
    if count > raw.len() {
        return Err(EncodingError::MalformedSource);
    }
    if eof
        && raw.len() - count == 3
        && let Some(state) = designation(&raw[count..])
    {
        next = state;
        count += 3;
    }
    Ok((count, next))
}

pub(crate) fn atom(raw: &[u8], state: State, eof: bool) -> Result<Atom, EncodingError> {
    let (count, next) = frame(raw, state, eof)?;
    if state == State::Raw {
        return Ok(Atom { raw: 1, text: [0xff; 8], len: 1, state });
    }
    let mut text = [0; 8];
    let (result, read, written) = state.decoder().decode_to_utf8_without_replacement(&raw[..count], &mut text, true);
    if result != DecoderResult::InputEmpty || read != count || written == 0 {
        return Err(EncodingError::MalformedSource);
    }
    Ok(Atom { raw: count, text, len: written, state: next })
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
