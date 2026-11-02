use super::*;
use crate::encoding::iso2022::{self, State};

pub(super) fn decoded_iso(
    origin: &DecodedSnapshot,
    range: Range<u64>,
    output: &mut TextOutput<'_, impl Write>,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<(), EncodingError> {
    let start = endpoint(origin, range.start)?;
    let end = endpoint(origin, range.end)?;
    let (Boundary::Exact { raw: first, .. }, Boundary::Exact { raw: last, .. }) = (start, end) else {
        return Err(EncodingError::SplitCharacter(range.start));
    };
    let mut state = origin.iso_state_at(first).map_err(backing_error)?;
    if output.plan.line_ending.is_none() {
        output.iso_begin(state)?;
        raw(output, &origin.raw_bytes()[first as usize..last as usize], cancelled)?;
        output.iso_raw_state = Some(origin.iso_state_at(last).map_err(backing_error)?);
        return Ok(());
    }
    if state == State::Raw {
        return Err(EncodingError::MalformedSource);
    }
    for atom in origin.atoms(range).map_err(backing_error)? {
        poll(cancelled)?;
        let atom = atom.map_err(backing_error)?;
        let next = iso2022::atom(atom.raw_bytes, state, atom.raw.end == origin.raw_bytes().len() as u64)?.state;
        if let AtomContent::Text("\r" | "\n") = atom.content {
            let AtomContent::Text(text) = atom.content else { unreachable!() };
            output.feed(text)?;
        } else {
            output.iso_begin(state)?;
            raw(output, atom.raw_bytes, cancelled)?;
            output.iso_raw_state = Some(next);
        }
        state = next;
    }
    Ok(())
}
