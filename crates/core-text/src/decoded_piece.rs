//! Shared logical line metrics for one immutable decoded generation.
//! Construction scans mapped bytes on a worker; lookups replay at most 64 KiB.

use crate::LineIndex;
use crate::decoded_backing::DecodedSnapshot;
use crate::decoded_backing::{AtomKind, Boundary};
use std::io;
use std::ops::Range;
use std::sync::atomic::AtomicU64;

pub(crate) struct IndexedDecoded {
    pub origin: DecodedSnapshot,
    pub index: LineIndex,
}

impl IndexedDecoded {
    pub fn new_checked<E>(origin: DecodedSnapshot, check: &mut impl FnMut() -> Result<(), E>) -> Result<Self, E> {
        let index = LineIndex::build_checked(
            origin.logical_bytes(),
            crate::line_index::DEFAULT_EVERY,
            &AtomicU64::new(0),
            check,
        )?;
        Ok(Self { origin, index })
    }

    pub fn retained_bytes(&self) -> usize {
        self.index.memory_bytes()
            + std::mem::size_of::<Self>()
            + 2 * std::mem::size_of::<usize>()
            + self.origin.paths().directory.capacity()
            + self.origin.paths().raw.capacity()
            + self.origin.paths().logical.capacity()
            + self.origin.paths().checkpoints.capacity()
            + self.origin.source_path().as_os_str().len()
    }
}

/// Validate scalar endpoints and return the complete atoms covering them.
pub(crate) fn validate_range(origin: &DecodedSnapshot, range: Range<u64>) -> io::Result<Range<u64>> {
    if range.start > range.end || range.end > origin.logical_bytes().len() as u64 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "range outside decoded origin"));
    }
    let mut covering = range.clone();
    for (first, at) in [(true, range.start), (false, range.end)] {
        if let Boundary::Interior { logical, kind, .. } = origin.resolve_logical(at).map_err(io::Error::other)? {
            let valid = kind == AtomKind::Text
                && std::str::from_utf8(&origin.logical_bytes()[logical.start as usize..logical.end as usize])
                    .is_ok_and(|text| text.is_char_boundary((at - logical.start) as usize));
            if !valid {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("edit splits a decoded scalar at {at}"),
                ));
            }
            if first {
                covering.start = logical.start;
            } else {
                covering.end = logical.end;
            }
        }
    }
    Ok(covering)
}
