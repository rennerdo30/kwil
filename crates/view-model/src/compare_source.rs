//! Comparison read ownership and cooperative work prepayment.
//!
//! ROOT registration: `pub mod compare_source;` plus the core read reexports.
//! This is NOT yet a WrapSource implementation: the current LayoutEngine has
//! monolithic fill/locate/cluster loops and no resumable step. Translating
//! cooperative Pending into io::Error (including WouldBlock) would lose its
//! Reader/GraphemeStepper continuation. ROOT must supply a shared-engine
//! continuation/source hook before connecting this owner to layout.
//!
//! No duplicate wrapping/alignment implementation or permissive unbounded
//! synchronous adapter is provided. The existing word source is untouched.
use crate::visual_rows::SourceIdentity;
use kwil_core_text::compare_input::{
    Admission, InputError, InputIdentity, InputStore, ReadLease, ReadLeaseRetirement, ReadLeaseStep, ReadWork, Step,
};
use std::{
    alloc::Layout,
    mem::size_of,
    sync::{Arc, Mutex, atomic::AtomicBool},
};

/// Running/latest desired/ready and retirement owners are bounded explicitly.
/// The private Arc cannot be downgraded to Weak, which could otherwise keep its
/// allocation alive beyond explicit lease settlement and header-charge release.
pub const COMPARE_SOURCE_OWNERS: usize = 5;
struct Shared<A: Admission> {
    lease: ReadLease<A>,
    clone_admission: Mutex<()>,
}

/// Owned worker source slot. Clone only through try_clone; no public Arc/Weak.
/// Dropping a slot does no file close/unlink or aggregate budget release. If
/// the last slot is misused, core's ManuallyDrop quarantine preserves those
/// resources. Successful cleanup MUST use try_into_retirement on the worker.
#[must_use]
pub struct CompareSource<A: Admission> {
    shared: Option<Arc<Shared<A>>>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceOwnerLimit;

#[must_use]
pub enum CompareSourceStep<A: Admission> {
    Pending(A),
    Cancelled(A),
    Rejected { admission: A, error: InputError },
    Ready(CompareSource<A>),
    Interrupted(ReadLeaseRetirement<A>),
    Failed { retirement: ReadLeaseRetirement<A>, error: InputError },
}

/// Exact Arc allocation layout including alignment padding, bounded slot
/// headers, construction/result overlap and scalar prepayment state. The core
/// lease prepays and retains this additional charge before handle cloning OR
/// Arc construction. No independent wrap budget substitutes for the caller's
/// invocation aggregate.
pub fn compare_source_transfer_charge<A: Admission>() -> Option<usize> {
    let (layout, _) = Layout::new::<[usize; 2]>().extend(Layout::new::<Shared<A>>()).ok()?;
    layout
        .pad_to_align()
        .size()
        .checked_add(size_of::<CompareSource<A>>().checked_mul(COMPARE_SOURCE_OWNERS)?)?
        .checked_add(size_of::<CompareSourceStep<A>>())?
        .checked_add(size_of::<ReadCredit>())
}

/// Worker-only factory; A is the SAME shared invocation aggregate as store's.
/// Core may use non-Send accounting. Only this production ownership boundary
/// requires Send+Sync+'static, matching the eventual WrapSource requirement.
pub fn begin_compare_source<A: Admission + Send + Sync + 'static>(
    store: &InputStore<A>,
    admission: A,
    work: &mut ReadWork,
    cancel: &AtomicBool,
) -> CompareSourceStep<A> {
    let Some(transfer) = compare_source_transfer_charge::<A>() else {
        return CompareSourceStep::Rejected { admission, error: InputError::Overflow };
    };
    match store.read_lease_with_transfer(admission, transfer, work, cancel) {
        ReadLeaseStep::Ready(lease) => CompareSourceStep::Ready(CompareSource {
            shared: Some(Arc::new(Shared { lease, clone_admission: Mutex::new(()) })),
        }),
        ReadLeaseStep::Pending(admission) => CompareSourceStep::Pending(admission),
        ReadLeaseStep::Cancelled(admission) => CompareSourceStep::Cancelled(admission),
        ReadLeaseStep::Rejected { admission, error } => CompareSourceStep::Rejected { admission, error },
        ReadLeaseStep::Interrupted(retirement) => CompareSourceStep::Interrupted(retirement),
        ReadLeaseStep::Failed { retirement, error } => CompareSourceStep::Failed { retirement, error },
    }
}

impl<A: Admission> CompareSource<A> {
    fn shared(&self) -> &Shared<A> {
        self.shared.as_deref().expect("live comparison source slot")
    }
    /// Identity is copied verbatim in the same four-word order. No hash,
    /// reconstructed filename, new incarnation or source capture is involved.
    pub fn input_identity(&self) -> InputIdentity {
        self.shared().lease.identity()
    }
    pub fn identity(&self) -> SourceIdentity {
        let [document_id, source_epoch, incarnation, revision] = self.input_identity().0;
        SourceIdentity { document_id, source_epoch, incarnation, revision }
    }
    /// Borrowing does not create a new source owner. Every access still takes
    /// explicit ReadWork and returns cooperative Step, preserving Pending.
    pub fn lease(&self) -> &ReadLease<A> {
        &self.shared().lease
    }
    pub fn owner_count(&self) -> usize {
        Arc::strong_count(self.shared.as_ref().expect("live comparison source slot"))
    }
    pub fn try_clone(&self) -> Result<Self, SourceOwnerLimit> {
        let shared = self.shared.as_ref().expect("live comparison source slot");
        // All increments serialize on the mutex INSIDE the already admitted
        // Arc allocation. Drops only reduce the authoritative strong count;
        // they cannot return a separate permit before releasing their Arc.
        // A successful Arc::clone while holding this guard is linearization.
        let _admission = shared.clone_admission.lock().map_err(|_| SourceOwnerLimit)?;
        if Arc::strong_count(shared) >= COMPARE_SOURCE_OWNERS {
            return Err(SourceOwnerLimit);
        }
        Ok(Self { shared: Some(Arc::clone(shared)) })
    }
    fn drop_with(&mut self, before_arc_drop: impl FnOnce()) {
        if let Some(shared) = self.shared.take() {
            // Keep this physical owner counted until Arc's real decrement.
            // The no-op production callback also gives fixtures a precise,
            // deterministic pause before that decrement, without a new Arc.
            before_arc_drop();
            drop(shared);
        }
    }
    /// Worker-only. A still-held slot refuses retirement and returns this exact
    /// owner. No lease, Arc allocation or budget is released on that refusal.
    /// Once successful, finish ReadLeaseRetirement before unlinking InputStore.
    pub fn try_into_retirement(mut self) -> Result<ReadLeaseRetirement<A>, Self> {
        let shared = self.shared.take().expect("live comparison source slot");
        match Arc::try_unwrap(shared) {
            Ok(shared) => Ok(shared.lease.into_retirement()),
            Err(shared) => {
                self.shared = Some(shared);
                Err(self)
            }
        }
    }
}
impl<A: Admission> Drop for CompareSource<A> {
    fn drop(&mut self) {
        // Last-slot destruction only reaches core's quarantine, never its
        // explicit worker retirement or aggregate release operation.
        self.drop_with(|| {});
    }
}

/// Fixed scalar accumulation for ONE finite, justified source-access envelope.
/// This is not a claimed bound for LayoutEngine::fill/scroll/navigate: those
/// operations currently have no finite quantum. A future shared continuation
/// must request each bounded access before entering it and keep its task state
/// across Pending. No buffered source data or growing work log is retained.
pub struct ReadCredit {
    required: ReadWork,
    paid: ReadWork,
}
impl ReadCredit {
    pub fn new(required: ReadWork) -> Self {
        Self { required, paid: ReadWork::default() }
    }
    pub fn paid(&self) -> ReadWork {
        self.paid
    }
    /// Pending/cancel perform no I/O. Ready transfers the exact reserved work
    /// into the upcoming bounded access and resets this credit for its next
    /// explicit use. Unspent credit must never fund another source owner/task.
    pub fn step(&mut self, work: &mut ReadWork, cancel: &AtomicBool) -> Step<ReadWork> {
        let bytes = work.bytes.min(self.required.bytes - self.paid.bytes);
        let operations = work.operations.min(self.required.operations - self.paid.operations);
        match work.debit(bytes, operations, cancel) {
            Step::Ready(()) => {}
            Step::Pending => return Step::Pending,
            Step::Cancelled => {
                self.paid = ReadWork::default();
                return Step::Cancelled;
            }
            Step::Failed(error) => return Step::Failed(error),
        }
        self.paid.bytes += bytes;
        self.paid.operations += operations;
        if self.paid == self.required {
            self.paid = ReadWork::default();
            Step::Ready(self.required)
        } else {
            Step::Pending
        }
    }
}

#[cfg(test)]
#[path = "compare_source_tests.rs"]
mod tests;
