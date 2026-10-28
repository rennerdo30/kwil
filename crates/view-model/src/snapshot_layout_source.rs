//! Worker-owned Snapshot adapter for visual_rows::task::CooperativeSource.
//! Concrete query types serve the sole cooperative layout engine. Source storage stays in ROOT's
//! existing document loan; all adapter/query/scratch admission uses its SAME
//! AllocationBudget. There is no fallback allowance or source-sized copy.
use crate::visual_rows::task::{
    CooperativeSource, Provenance, SourceMetadata, TaskBytes, TaskError, TaskPoll, WorkCredit, same_budget,
};
use crate::visual_rows::{AllocationBudget, MetadataReservation, SOURCE_WINDOW, SourceIdentity, WrapError};
use kwil_core_text::compare_input::{InputError, LineSpan, Rank, ReadWork, Step};
use kwil_core_text::{
    Snapshot,
    worker_layout::{LayoutLineQuery, LayoutProvenanceQuery, LayoutRankQuery, LayoutReadQuery},
};
use std::{
    marker::PhantomData,
    mem::size_of,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

// This nonce identifies the actual adapter owner, not its cached document
// identity, source bytes or current stack address. It survives moves, cannot
// be forged outside this module and never wraps/reuses a retired owner's key.
static NEXT_READ_OWNER: AtomicU64 = AtomicU64::new(1);

#[must_use]
pub struct SnapshotLayoutSource<O = Snapshot> {
    owner: O,
    identity: SourceIdentity,
    read_owner: u64,
    budget: AllocationBudget,
    _metadata: MetadataReservation,
    _worker: PhantomData<Rc<()>>,
}
#[must_use]
pub struct SnapshotLayoutRefusal {
    pub snapshot: Snapshot,
    pub error: WrapError,
}
/// Cached access only. The owner is moved, never cloned by this adapter.
pub trait SnapshotOwner {
    fn snapshot(&self) -> &Snapshot;
}
impl SnapshotOwner for Snapshot {
    fn snapshot(&self) -> &Snapshot {
        self
    }
}
impl<T: SnapshotOwner> SnapshotOwner for Arc<T> {
    fn snapshot(&self) -> &Snapshot {
        T::snapshot(self.as_ref())
    }
}
impl<T: SnapshotOwner + ?Sized> SnapshotOwner for &T {
    fn snapshot(&self) -> &Snapshot {
        T::snapshot(*self)
    }
}
#[must_use]
pub struct SnapshotOwnerRefusal<O> {
    pub owner: O,
    pub error: WrapError,
}
impl SnapshotLayoutSource {
    pub fn try_new(
        snapshot: Snapshot,
        identity: SourceIdentity,
        budget: &AllocationBudget,
    ) -> Result<Self, SnapshotLayoutRefusal> {
        Self::try_with_owner(snapshot, identity, budget)
            .map_err(|refused| SnapshotLayoutRefusal { snapshot: refused.owner, error: refused.error })
    }
    pub fn into_snapshot(self) -> Snapshot {
        self.into_owner()
    }
}
impl<O: SnapshotOwner> SnapshotLayoutSource<O> {
    pub fn try_with_owner(
        owner: O,
        identity: SourceIdentity,
        budget: &AllocationBudget,
    ) -> Result<Self, SnapshotOwnerRefusal<O>> {
        // Actual GENERIC adapter and return/refusal overlap. Query payloads
        // retain separate admission, all against this SAME host aggregate.
        let bytes = size_of::<Self>()
            .checked_add(size_of::<LayoutLineQuery>())
            .and_then(|n| n.checked_add(size_of::<LayoutRankQuery>()))
            .and_then(|n| n.checked_add(size_of::<LayoutProvenanceQuery>()))
            .and_then(|n| n.checked_add(size_of::<SnapshotReadQuery>()))
            .and_then(|n| n.checked_mul(3))
            .and_then(|n| n.checked_add(size_of::<SnapshotOwnerRefusal<O>>()))
            .and_then(|n| n.checked_add(LayoutReadQuery::extra_resident_bytes()))
            .and_then(|n| n.checked_add(size_of::<AtomicU64>()));
        let Some(bytes) = bytes else {
            return Err(SnapshotOwnerRefusal {
                owner,
                error: WrapError::Admission("Snapshot owner metadata overflow"),
            });
        };
        let metadata = match budget.reserve_metadata(bytes) {
            Ok(metadata) => metadata,
            Err(error) => return Err(SnapshotOwnerRefusal { owner, error }),
        };
        let read_owner = match NEXT_READ_OWNER.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1)) {
            Ok(nonce) => nonce,
            Err(_) => {
                return Err(SnapshotOwnerRefusal {
                    owner,
                    error: WrapError::Admission("Snapshot read owner key exhausted"),
                });
            }
        };
        Ok(Self { owner, identity, read_owner, budget: budget.clone(), _metadata: metadata, _worker: PhantomData })
    }
    pub fn into_owner(self) -> O {
        self.owner
    }
    /// A scoped borrow for typed adapter composition; never a new owner slot.
    pub fn owner(&self) -> &O {
        &self.owner
    }
    /// No temporary check handle survives Pending.
    pub fn temporary_handles() -> usize {
        LayoutReadQuery::temporary_handles()
    }
}
pub struct SnapshotReadQuery {
    read_owner: u64,
    core: LayoutReadQuery,
    length: usize,
    logical: TaskBytes,
    original: TaskBytes,
    returned: Option<usize>,
    credit: WorkCredit,
    _metadata: MetadataReservation,
}
impl<O: SnapshotOwner> CooperativeSource for SnapshotLayoutSource<O> {
    type LineQuery = LayoutLineQuery;
    type RankQuery = LayoutRankQuery;
    type ReadQuery = SnapshotReadQuery;
    type ProvenanceQuery = LayoutProvenanceQuery;
    fn metadata(&self) -> SourceMetadata {
        SourceMetadata {
            identity: self.identity,
            len: self.owner.snapshot().len(),
            line_count: self.owner.snapshot().line_count(),
        }
    }
    fn new_line_query(&self, line: u64) -> Self::LineQuery {
        LayoutLineQuery::new(line)
    }
    fn new_rank_query(&self, at: u64) -> Self::RankQuery {
        LayoutRankQuery::new(at)
    }
    fn new_read_query(&self, at: u64, length: usize, budget: &AllocationBudget) -> Result<Self::ReadQuery, WrapError> {
        if !same_budget(&self.budget, budget) {
            return Err(WrapError::Invalid("Snapshot scratch aggregate changed"));
        }
        if length > SOURCE_WINDOW {
            return Err(WrapError::Admission("Snapshot read window bound"));
        }
        let metadata =
            budget.reserve_metadata(3 * size_of::<SnapshotReadQuery>() + LayoutReadQuery::extra_resident_bytes())?;
        let logical = TaskBytes::try_new(budget, length)?;
        // A complete preserved atom can have more original than logical bytes.
        // Reuse the existing cursor's documented 64 KiB original-window bound.
        let original = TaskBytes::try_new(budget, SOURCE_WINDOW)?;
        Ok(SnapshotReadQuery {
            read_owner: self.read_owner,
            core: LayoutReadQuery::new(at, length),
            length,
            logical,
            original,
            returned: None,
            credit: WorkCredit::default(),
            _metadata: metadata,
        })
    }
    fn new_provenance_query(&self, at: u64) -> Self::ProvenanceQuery {
        LayoutProvenanceQuery::new(at)
    }
    fn poll_line(&self, query: &mut Self::LineQuery, work: &mut ReadWork, cancel: &AtomicBool) -> TaskPoll<LineSpan> {
        query.poll(self.owner.snapshot(), work, cancel).into()
    }
    fn poll_rank(&self, query: &mut Self::RankQuery, work: &mut ReadWork, cancel: &AtomicBool) -> TaskPoll<Rank> {
        query.poll(self.owner.snapshot(), work, cancel).into()
    }
    fn poll_read(
        &self,
        query: &mut Self::ReadQuery,
        output: &mut [u8],
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> TaskPoll<usize> {
        match work.debit(0, 0, cancel) {
            Step::Ready(()) => {}
            Step::Pending => return TaskPoll::Pending,
            Step::Cancelled => return TaskPoll::Cancelled,
            Step::Failed(error) => return TaskPoll::Failed(error.into()),
        }
        // Check even before the first core poll and after returned is Some:
        // staging never authorizes publication through a different source.
        if query.read_owner != self.read_owner {
            return TaskPoll::Failed(InputError::QueryChanged.into());
        }
        if output.len() != query.length {
            return TaskPoll::Failed(TaskError::Layout(WrapError::Invalid("changed Snapshot read output")));
        }
        if query.returned.is_none() {
            match query.core.poll(self.owner.snapshot(), &mut query.logical, &mut query.original, work, cancel) {
                Step::Ready(chunk) => query.returned = Some(chunk.map_or(0, |chunk| chunk.logical_len)),
                Step::Pending => return TaskPoll::Pending,
                Step::Cancelled => return TaskPoll::Cancelled,
                Step::Failed(error) => return TaskPoll::Failed(error.into()),
            }
        }
        let count = query.returned.unwrap();
        match query.credit.poll(ReadWork::new(count as u64, 1), work, cancel) {
            TaskPoll::Ready(_) => {}
            TaskPoll::Pending => return TaskPoll::Pending,
            TaskPoll::Cancelled => return TaskPoll::Cancelled,
            TaskPoll::Failed(error) => return TaskPoll::Failed(error),
        }
        output[..count].copy_from_slice(&query.logical[..count]);
        TaskPoll::Ready(count)
    }
    fn poll_provenance(
        &self,
        query: &mut Self::ProvenanceQuery,
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> TaskPoll<Provenance> {
        match query.poll(self.owner.snapshot(), work, cancel) {
            Step::Ready(span) => TaskPoll::Ready(Provenance { source: span.source, opaque: span.opaque }),
            Step::Pending => TaskPoll::Pending,
            Step::Cancelled => TaskPoll::Cancelled,
            Step::Failed(error) => TaskPoll::Failed(error.into()),
        }
    }
}

#[cfg(test)]
#[path = "snapshot_layout_source_tests.rs"]
mod tests;
