use super::*;
use crate::encoding::iso2022::{self, State};

const ISO_RECORD: usize = 24;

fn write_checkpoint(writer: &mut impl Write, raw: usize, logical: u64, state: State) -> io::Result<()> {
    checkpoint(writer, raw, logical)?;
    writer.write_all(&[state as u8, 0, 0, 0, 0, 0, 0, 0])
}

pub(super) fn build_iso(
    raw: &[u8],
    logical: &mut impl Write,
    index: &mut impl Write,
    cancel: &AtomicBool,
) -> Result<(), BackingError> {
    // Validate with the same codec as the viewport. Any malformed ISO file uses
    // the existing opaque-byte model for the whole origin, with no replacement.
    let mut decoder = State::Ascii.decoder();
    let mut scratch = [0; SCRATCH];
    let (mut at, mut output) = (0, false);
    let mut valid = true;
    loop {
        cancelled(cancel)?;
        let (result, read, written) = decoder.decode_to_utf8_without_replacement(&raw[at..], &mut scratch, true);
        at += read;
        output |= written != 0;
        match result {
            DecoderResult::Malformed(..) => {
                valid = false;
                break;
            }
            DecoderResult::InputEmpty => break,
            DecoderResult::OutputFull => {}
        }
    }
    let mut state = if valid && (raw.is_empty() || output) { State::Ascii } else { State::Raw };
    let (mut at, mut length) = (0, 0);
    write_checkpoint(index, at, length, state)?;
    while at < raw.len() {
        cancelled(cancel)?;
        let first = at;
        let initial = state;
        let limit = raw.len().min(first + CHECKPOINT_RAW_BYTES - 8);
        if state == State::Raw {
            at = limit;
            let opaque = [OPAQUE; SCRATCH];
            let mut remaining = at - first;
            while remaining != 0 {
                let count = remaining.min(opaque.len());
                logical.write_all(&opaque[..count])?;
                remaining -= count;
            }
            length += (at - first) as u64;
        } else {
            while at < limit {
                let (count, next) = iso2022::frame(&raw[at..], state, true)?;
                at += count;
                state = next;
            }
            let mut decoder = initial.decoder();
            let mut input = first;
            loop {
                cancelled(cancel)?;
                let (result, read, written) =
                    decoder.decode_to_utf8_without_replacement(&raw[input..at], &mut scratch, true);
                input += read;
                logical.write_all(&scratch[..written])?;
                length += written as u64;
                match result {
                    DecoderResult::InputEmpty => break,
                    DecoderResult::OutputFull => {}
                    DecoderResult::Malformed(..) => return Err(EncodingError::MalformedSource.into()),
                }
            }
        }
        write_checkpoint(index, at, length, state)?;
    }
    Ok(())
}

impl DecodedSnapshot {
    pub(super) fn record_size(&self) -> usize {
        if self.interpretation().encoding.iso2022() { ISO_RECORD } else { RECORD }
    }

    fn checkpoint_state(&self, index: usize) -> State {
        State::from_byte(self.storage.checkpoints[index * ISO_RECORD + 16])
    }

    pub(crate) fn iso_state_at(&self, position: u64) -> Result<State, BackingError> {
        let index = self.floor(position, false);
        let (mut raw, _) = self.checkpoint(index);
        let mut state = self.checkpoint_state(index);
        while raw < position {
            let atom = iso2022::atom(&self.raw_bytes()[raw as usize..], state, true)?;
            raw += atom.raw as u64;
            state = atom.state;
        }
        if raw != position {
            return Err(BackingError::InvalidRange);
        }
        Ok(state)
    }

    pub(super) fn iso_resolve(&self, position: u64, logical: bool) -> Result<Boundary, BackingError> {
        if position > if logical { self.logical_bytes().len() as u64 } else { self.raw_bytes().len() as u64 } {
            return Err(BackingError::InvalidRange);
        }
        let index = self.floor(position, logical);
        let (mut raw, mut at) = self.checkpoint(index);
        let mut state = self.checkpoint_state(index);
        while position != if logical { at } else { raw } {
            let atom = iso2022::atom(&self.raw_bytes()[raw as usize..], state, true)?;
            let next_raw = raw + atom.raw as u64;
            let next = at + atom.len as u64;
            if position < if logical { next } else { next_raw } {
                return Ok(Boundary::Interior {
                    logical: at..next,
                    raw: raw..next_raw,
                    kind: if state == State::Raw { AtomKind::Opaque } else { AtomKind::Text },
                });
            }
            raw = next_raw;
            at = next;
            state = atom.state;
        }
        Ok(Boundary::Exact { logical: at, raw })
    }
}

/// One resumable checkpoint lookup and prefix replay for an arbitrary raw
/// position. Sequential source iteration passes the previous atom's state.
pub(super) struct Replay {
    origin: DecodedSnapshot,
    target: u64,
    low: usize,
    high: usize,
    floor: usize,
    at: Option<u64>,
    pub state: State,
}

impl Replay {
    pub(super) fn new(origin: &DecodedSnapshot, target: u64, state: Option<State>) -> Self {
        Self {
            origin: origin.clone(),
            target,
            low: 0,
            high: if state.is_some() { 0 } else { origin.checkpoint_count() },
            floor: 0,
            at: state.map(|_| target),
            state: state.unwrap_or_default(),
        }
    }

    pub(super) fn poll(
        &mut self,
        cancel: &AtomicBool,
        admit: &mut impl FnMut(usize) -> io::Result<bool>,
    ) -> Result<Option<(AtomSize, [u8; 8])>, BackingError> {
        while self.low < self.high {
            cancelled(cancel)?;
            if !admit(ISO_RECORD)? {
                return Ok(None);
            }
            let mid = self.low + (self.high - self.low) / 2;
            let (raw, _) = self.origin.checkpoint(mid);
            if raw <= self.target {
                self.floor = mid;
                self.low = mid + 1;
            } else {
                self.high = mid;
            }
        }
        if self.at.is_none() {
            cancelled(cancel)?;
            if !admit(ISO_RECORD)? {
                return Ok(None);
            }
            self.at = Some(self.origin.checkpoint(self.floor).0);
            self.state = self.origin.checkpoint_state(self.floor);
        }
        loop {
            cancelled(cancel)?;
            let at = self.at.unwrap();
            let end = self.origin.raw_bytes().len().min(at as usize + 8);
            if !admit(end - at as usize)? {
                return Ok(None);
            }
            cancelled(cancel)?;
            // Read the admitted mapped bytes once. Framing and decoder replay
            // may revisit their input, so keep those visits on fixed scratch.
            let mut raw = [0; 8];
            let count = end - at as usize;
            raw[..count].copy_from_slice(&self.origin.raw_bytes()[at as usize..end]);
            let atom = iso2022::atom(&raw[..count], self.state, end == self.origin.raw_bytes().len())?;
            let kind = if self.state == State::Raw { AtomKind::Opaque } else { AtomKind::Text };
            self.state = atom.state;
            self.at = Some(at + atom.raw as u64);
            if at == self.target {
                return Ok(Some((AtomSize { raw: atom.raw, logical: atom.len, kind }, atom.text)));
            }
            if self.at.unwrap() > self.target {
                return Err(BackingError::InvalidRange);
            }
        }
    }
}
