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
    pub fn new(origin: DecodedSnapshot) -> Self {
        let index = LineIndex::build(origin.logical_bytes(), crate::line_index::DEFAULT_EVERY, 1, &AtomicU64::new(0));
        Self { origin, index }
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

pub(crate) fn validate_range(origin: &DecodedSnapshot, range: Range<u64>) -> io::Result<()> {
    for at in [range.start, range.end] {
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
        }
    }
    Ok(())
}
