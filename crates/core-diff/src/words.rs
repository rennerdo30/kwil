//! Row-scoped worker refinement. `request` remains a source-free capability
//! probe; executable work starts at `begin` with exact comparison ownership.
mod tokens;
mod worker;
use crate::{Kind, stream::AlignedRow};
use kwil_core_text::compare_input::InputError;
use std::ops::Range;
pub use worker::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Coverage {
    pub left: Range<u64>,
    pub right: Range<u64>,
    pub complete: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotApplicable {
    EqualRow,
    OneSidedRow,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingReason {
    BackendRequired,
    WorkExhausted,
}
pub const WORD_PAGE_RECORDS: usize = 64;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WordChange {
    pub left: Range<u64>,
    pub right: Range<u64>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefinementState {
    NotApplicable,
    Pending,
    Ready,
    Failed,
}
/// Fixed unboxed carrier. The invocation admits this full resident size and
/// its construction/output overlap before use; the line engine includes it in
/// resident_charge. Private constructors keep state and optional fields valid.
pub struct Refinement {
    state: RefinementState,
    coverage: Option<Coverage>,
    reason: Option<PendingReason>,
    not_applicable: Option<NotApplicable>,
    records: [Option<WordChange>; WORD_PAGE_RECORDS],
    len: usize,
    error: Option<InputError>,
}
impl Refinement {
    pub fn state(&self) -> RefinementState {
        self.state
    }
    pub fn coverage(&self) -> Option<&Coverage> {
        self.coverage.as_ref()
    }
    pub fn pending_reason(&self) -> Option<PendingReason> {
        self.reason
    }
    pub fn not_applicable_reason(&self) -> Option<NotApplicable> {
        self.not_applicable
    }
    pub fn records(&self) -> &[Option<WordChange>] {
        &self.records[..self.len]
    }
    pub fn error(&self) -> Option<&InputError> {
        self.error.as_ref()
    }
}
/// Does not access either source or take its serial query owner. Coordinates
/// are original logical byte ranges supplied by the visible-line lookup owner.
/// ROOT must wire a real token backend before treating word refinement as done.
pub fn request(row: AlignedRow, left: Range<u64>, right: Range<u64>) -> Refinement {
    let not_applicable = if row.kind == Kind::Equal {
        Some(NotApplicable::EqualRow)
    } else if row.left.is_none() || row.right.is_none() {
        Some(NotApplicable::OneSidedRow)
    } else {
        None
    };
    let applicable = not_applicable.is_none();
    Refinement {
        state: if applicable { RefinementState::Pending } else { RefinementState::NotApplicable },
        coverage: applicable.then_some(Coverage {
            left: left.start..left.start,
            right: right.start..right.start,
            complete: false,
        }),
        reason: applicable.then_some(PendingReason::BackendRequired),
        not_applicable,
        records: std::array::from_fn(|_| None),
        len: 0,
        error: None,
    }
}

#[cfg(test)]
#[path = "words/tests.rs"]
mod tests;
