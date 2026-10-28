//! Sealed worker transactions over the existing Buffer and History.
//!
//! This module never starts a thread. Depot, operation and cleanup handles stay
//! on the document worker. Tokens can cross a bounded lane; rejected tokens and
//! admission are returned intact. Host publication reservations stage the REAL
//! model history, journal batch and outbox before entering the final commit.
//!
//! ```
//! use kwil_core_text::worker_edit::{PreparedWorkerEdit, WorkerEditSeed,
//!     WorkerEditDepot, WorkerEditOperation, WorkerEditResources};
//! fn assert_send<T: Send>() {}
//! assert_send::<PreparedWorkerEdit>();
//! let _types: Option<(WorkerEditSeed, WorkerEditDepot, WorkerEditOperation,
//!     WorkerEditResources)> = None;
//! ```
//! ```compile_fail
//! use kwil_core_text::worker_edit::PreparedWorkerEdit;
//! fn forge(token: PreparedWorkerEdit) { let _ = token.after; }
//! ```
//! ```compile_fail
//! use kwil_core_text::worker_edit::PreparedWorkerEdit;
//! fn duplicate(token: PreparedWorkerEdit) { let _ = token.clone(); }
//! ```
//! ```compile_fail
//! use kwil_core_text::worker_edit::WorkerEditOperation;
//! fn frontend(operation: WorkerEditOperation) {
//!     std::thread::spawn(move || drop(operation));
//! }
//! ```
//! ```compile_fail
//! use kwil_core_text::worker_edit::WorkerEditDepot;
//! fn frontend(depot: WorkerEditDepot) { std::thread::spawn(move || drop(depot)); }
//! ```
//! ```compile_fail
//! use kwil_core_text::worker_edit::WorkerEditResources;
//! fn frontend(owner: WorkerEditResources) { std::thread::spawn(move || drop(owner)); }
//! ```
use crate::piece_tree::{self, AddBudget, WorkerAllocation};
use crate::undo::PreparedHistory;
use crate::{Buffer, Snapshot, SpillRetirement, SpillRetirementReport};
use std::{
    cell::RefCell,
    io,
    marker::PhantomData,
    ops::Range,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone, Copy, Debug)]
pub struct WorkerDepotLimits {
    pub disk_bytes: u64,
    pub metadata_bytes: usize,
    pub slots: usize,
}
/// Conservative PEAK reservation, including unused temporaries. No value here
/// is a user-facing editor setting. Payload disk reserves the worst spill case,
/// independent of concurrent AddBudget users. External recipe capacity remains
/// held until its owner explicitly confirms successful spool retirement.
#[derive(Clone, Copy, Debug)]
pub struct WorkerEditPlan {
    pub disk_bytes: u64,
    pub recipe_bytes: u64,
    pub metadata_bytes: usize,
    pub max_new_blocks: usize,
    pub max_certificates: usize,
    pub max_tree_nodes: usize,
    pub max_path_bytes: usize,
}
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct WorkerCapacityUsage {
    pub disk_bytes: u64,
    pub metadata_bytes: usize,
    pub slots: usize,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Admitted,
    Preparing,
    Sealed,
    Poisoned,
    Retired,
}
struct Slot {
    id: u64,
    plan: WorkerEditPlan,
    state: State,
    allocation: Option<WorkerAllocation>,
    pins: Arc<()>,
    external_released: bool,
    capacity: Rc<RefCell<WorkerCapacityUsage>>,
    vm_metadata: usize,
    install_metadata: usize,
    #[cfg(test)]
    admission_allocations: usize,
}
pub(crate) fn slot_metadata() -> usize {
    std::mem::size_of::<RefCell<Slot>>() + 4 * std::mem::size_of::<usize>()
}
impl Drop for Slot {
    fn drop(&mut self) {
        if self.state == State::Retired {
            // Retired Resources/Admission handles may still retain this Rc and
            // token-pin allocation after the depot frees its registry slot.
            self.capacity.borrow_mut().metadata_bytes -= slot_metadata();
        }
    }
}
/// Stable rescue registry. A slot is inserted BEFORE allocation scratch or any
/// filesystem call. Unwind only changes its state; exact resources never leave
/// this registry. Dropping an undrained depot deliberately leaks its owners and
/// reservations; it performs no unlink and is not reported as successful cleanup.
pub struct WorkerEditDepot {
    limits: WorkerDepotLimits,
    usage: Rc<RefCell<WorkerCapacityUsage>>,
    next: u64,
    slots: Vec<Rc<RefCell<Slot>>>,
    recovered: Vec<(SpillRetirement, u64)>,
    _worker: PhantomData<Rc<()>>,
}
impl WorkerEditDepot {
    pub fn new(limits: WorkerDepotLimits) -> io::Result<Self> {
        let overhead = limits
            .slots
            .checked_mul(std::mem::size_of::<Rc<RefCell<Slot>>>() + std::mem::size_of::<(SpillRetirement, u64)>())
            .and_then(|n| {
                n.checked_add(
                    std::mem::size_of::<Self>()
                        + std::mem::size_of::<RefCell<WorkerCapacityUsage>>()
                        + 2 * std::mem::size_of::<usize>(),
                )
            })
            .ok_or_else(overflow)?;
        if overhead > limits.metadata_bytes {
            return Err(capacity());
        }
        let mut slots = Vec::new();
        let mut recovered = Vec::new();
        slots.try_reserve_exact(limits.slots).map_err(io::Error::other)?;
        recovered.try_reserve_exact(limits.slots).map_err(io::Error::other)?;
        Ok(Self {
            limits,
            usage: Rc::new(RefCell::new(WorkerCapacityUsage { metadata_bytes: overhead, ..Default::default() })),
            next: 0,
            slots,
            recovered,
            _worker: PhantomData,
        })
    }
    pub fn usage(&self) -> WorkerCapacityUsage {
        *self.usage.borrow()
    }
    /// Transfer the actual existing recovery lease, not a leased=true assertion.
    /// Failure returns that exact lease, preserving its cleanup authority.
    pub fn attach_recovered(&mut self, lease: SpillRetirement) -> Result<(), (SpillRetirement, io::Error)> {
        let charge = lease.metadata_reserved();
        let bytes = match lease.worker_disk_bytes() {
            Ok(n) => n,
            Err(error) => return Err((lease, error)),
        };
        let Some(disk) = self.usage.borrow().disk_bytes.checked_add(bytes) else {
            return Err((lease, overflow()));
        };
        let Some(total) = self.usage.borrow().metadata_bytes.checked_add(charge) else {
            return Err((lease, overflow()));
        };
        if self.recovered.len() >= self.limits.slots
            || total > self.limits.metadata_bytes
            || disk > self.limits.disk_bytes
        {
            return Err((lease, capacity()));
        }
        self.usage.borrow_mut().metadata_bytes = total;
        self.usage.borrow_mut().disk_bytes = disk;
        self.recovered.push((lease, bytes));
        Ok(())
    }
    pub fn admit(&mut self, plan: WorkerEditPlan) -> io::Result<WorkerEditAdmission> {
        let recipe_disk = if plan.recipe_bytes == 0 { 0 } else { piece_tree::worker_disk_charge(plan.recipe_bytes)? };
        // Slot Rc allocation (header+RefCell+Slot) and private Arc token pin
        // are INCLUDED in the admitted plan before either heap object exists.
        if recipe_disk > plan.disk_bytes || plan.metadata_bytes < slot_metadata() {
            return Err(capacity());
        }
        let disk = self.usage.borrow().disk_bytes.checked_add(plan.disk_bytes).ok_or_else(overflow)?;
        let metadata = self.usage.borrow().metadata_bytes.checked_add(plan.metadata_bytes).ok_or_else(overflow)?;
        let next = self.next.checked_add(1).ok_or_else(overflow)?;
        if disk > self.limits.disk_bytes
            || metadata > self.limits.metadata_bytes
            || self.slots.len() >= self.limits.slots
        {
            return Err(capacity());
        }
        // Rc/slot allocation is covered by the reservation before it occurs;
        // there is no filesystem resource yet if the allocator aborts here.
        self.usage.borrow_mut().disk_bytes = disk;
        self.usage.borrow_mut().metadata_bytes = metadata;
        self.usage.borrow_mut().slots += 1;
        self.next = next;
        let slot = Rc::new(RefCell::new(Slot {
            id: next,
            plan,
            state: State::Admitted,
            allocation: None,
            pins: Arc::new(()),
            external_released: plan.recipe_bytes == 0,
            capacity: self.usage.clone(),
            vm_metadata: 0,
            install_metadata: 0,
            #[cfg(test)]
            admission_allocations: 0,
        }));
        self.slots.push(slot.clone()); // preallocated bounded registry
        Ok(WorkerEditAdmission { slot })
    }
    fn owns(&self, slot: &Rc<RefCell<Slot>>) -> bool {
        self.slots.iter().any(|entry| Rc::ptr_eq(entry, slot))
    }
    fn covers(&self, storage: &Arc<piece_tree::Storage>) -> bool {
        self.recovered.iter().any(|(lease, _)| lease.covers_worker_storage(storage))
            || self.slots.iter().any(|slot| {
                let slot = slot.borrow();
                slot.state == State::Sealed && slot.allocation.as_ref().is_some_and(|a| a.owns_storage(storage))
            })
    }
    /// Explicit worker pass. Dropping a result never runs this. Capacity remains
    /// reserved on live pins, partial-file errors, and unacknowledged recipe IO.
    pub fn retire(&mut self) -> SpillRetirementReport {
        let mut report = SpillRetirementReport::default();
        let mut at = 0;
        while at < self.slots.len() {
            let mut slot = self.slots[at].borrow_mut();
            if matches!(slot.state, State::Admitted | State::Preparing)
                || Arc::strong_count(&slot.pins) != 1
                || !slot.external_released
            {
                report.live += 1;
                at += 1;
                continue;
            }
            let done = if let Some(allocation) = &mut slot.allocation {
                let pass = allocation.retire();
                let done = allocation.is_empty();
                merge(&mut report, pass);
                done
            } else {
                true
            };
            if !done {
                at += 1;
                continue;
            }
            let plan = slot.plan;
            slot.allocation.take(); // free retained vector capacities BEFORE quota release
            slot.state = State::Retired;
            drop(slot);
            self.slots.swap_remove(at);
            self.usage.borrow_mut().disk_bytes -= plan.disk_bytes;
            self.usage.borrow_mut().metadata_bytes -= plan.metadata_bytes - slot_metadata();
            self.usage.borrow_mut().slots -= 1;
        }
        let mut at = 0;
        while at < self.recovered.len() {
            let charge = self.recovered[at].0.metadata_reserved();
            let disk = self.recovered[at].1;
            merge(&mut report, self.recovered[at].0.retire());
            if self.recovered[at].0.retained() == 0 {
                drop(self.recovered.swap_remove(at));
                self.usage.borrow_mut().metadata_bytes -= charge;
                self.usage.borrow_mut().disk_bytes -= disk;
            } else {
                at += 1;
            }
        }
        report
    }
    pub fn try_close(mut self) -> Result<(), Self> {
        self.retire();
        if self.slots.is_empty() && self.recovered.is_empty() { Ok(()) } else { Err(self) }
    }
}
impl Drop for WorkerEditDepot {
    fn drop(&mut self) {
        // Retain exact quarantine paths/handles/accounting on loss of worker.
        // The explicit try_close result is the retriable shutdown protocol.
        if !self.slots.is_empty() || !self.recovered.is_empty() {
            std::mem::forget(self.usage.clone()); // preserve orphan reservations too
        }
        for slot in self.slots.drain(..) {
            std::mem::forget(slot);
        }
        for lease in self.recovered.drain(..) {
            std::mem::forget(lease);
        }
    }
}
pub struct WorkerEditAdmission {
    slot: Rc<RefCell<Slot>>,
}
impl Drop for WorkerEditAdmission {
    fn drop(&mut self) {
        let mut slot = self.slot.borrow_mut();
        if slot.state == State::Admitted {
            slot.state = State::Poisoned;
            slot.external_released = true; // begin never succeeded; no recipe creation authorized
        }
    }
}
/// O(1) capture. Strong identities prevent pointer recycling; the core epoch
/// separately rejects edit/undo ABA, transaction and policy changes.
pub struct WorkerEditSeed {
    before: Snapshot,
    added: Arc<AddBudget>,
    owner: Arc<()>,
    epoch: u64,
}
pub struct WorkerEditOperation {
    seed: WorkerEditSeed,
    slot: Rc<RefCell<Slot>>,
    _worker: PhantomData<Rc<()>>,
}
impl WorkerEditSeed {
    pub fn begin(
        self,
        admission: WorkerEditAdmission,
        depot: &mut WorkerEditDepot,
        cancelled: &AtomicBool,
    ) -> Result<WorkerEditOperation, BeginRefusal> {
        let result = (|| {
            poll(cancelled)?;
            if !depot.owns(&admission.slot) || admission.slot.borrow().state != State::Admitted {
                return Err(invalid("wrong depot or consumed admission"));
            }
            let plan = admission.slot.borrow().plan;
            // Constructing the bounded vectors happens only after rescue slot
            // registration and the aggregate reservation above.
            let mut allocation = WorkerAllocation::new(plan)?;
            allocation.certify_baseline(&self.before.root, &mut |storage| depot.covers(storage), cancelled)?;
            let mut slot = admission.slot.borrow_mut();
            slot.allocation = Some(allocation);
            slot.state = State::Preparing;
            Ok(())
        })();
        if let Err(error) = result {
            return Err(BeginRefusal { seed: self, admission, error });
        }
        Ok(WorkerEditOperation { seed: self, slot: admission.slot.clone(), _worker: PhantomData })
    }
}
pub struct BeginRefusal {
    pub seed: WorkerEditSeed,
    pub admission: WorkerEditAdmission,
    pub error: io::Error,
}

impl WorkerEditOperation {
    pub fn operation_id(&self) -> u64 {
        self.slot.borrow().id
    }
    fn checked<R>(&mut self, f: impl FnOnce(&WorkerEditSeed, &mut WorkerAllocation) -> io::Result<R>) -> io::Result<R> {
        let mut slot = self.slot.borrow_mut();
        if slot.state != State::Preparing {
            return Err(invalid("worker operation is sealed or poisoned"));
        }
        // Arm rescue BEFORE calling arbitrary read/packing work. A caller that
        // catches unwind while retaining the operation still cannot reuse it.
        slot.state = State::Poisoned;
        let result = f(&self.seed, slot.allocation.as_mut().expect("registered allocation"));
        if result.is_ok() {
            slot.state = State::Preparing;
        }
        result
    }
    /// VM packing/recipe scratch is part of this REAL pre-admitted peak, not
    /// capacity obtained after file creation. Payload admission is additional.
    pub fn reserve(&mut self, recipe_bytes: u64, metadata_bytes: usize) -> io::Result<()> {
        let mut slot = self.slot.borrow_mut();
        if slot.state != State::Preparing {
            return Err(invalid("worker operation is sealed or poisoned"));
        }
        if recipe_bytes > slot.plan.recipe_bytes || metadata_bytes > slot.plan.metadata_bytes {
            slot.state = State::Poisoned;
            return Err(capacity());
        }
        let result = slot.allocation.as_mut().expect("registered allocation").reserve_external(metadata_bytes);
        if result.is_err() {
            slot.state = State::Poisoned;
        } else {
            slot.vm_metadata = slot.vm_metadata.max(metadata_bytes);
        }
        result
    }
    pub fn import(&mut self, source: &Snapshot, range: Range<u64>, cancelled: &AtomicBool) -> io::Result<Snapshot> {
        self.checked(|seed, allocation| {
            poll(cancelled)?;
            if range.start > range.end || range.end > source.len() {
                return Err(invalid("import range"));
            }
            allocation.charge_tree(&source.root)?;
            piece_tree::recovery_cut(&source.root, range.start, &mut || poll(cancelled))?;
            piece_tree::recovery_cut(&source.root, range.end, &mut || poll(cancelled))?;
            let selected = source.slice(range)?;
            let root = piece_tree::worker_import(selected.root, &seed.added, allocation, cancelled)?;
            allocation.snapshot(root, cancelled)
        })
    }
    pub fn seal(
        &mut self,
        after: Snapshot,
        cancelled: &AtomicBool,
    ) -> io::Result<(PreparedWorkerEdit, WorkerEditResources)> {
        self.checked(|seed, allocation| {
            allocation.validate(&after, &seed.added, &seed.owner, cancelled)?;
            allocation.claim(cancelled)?;
            poll(cancelled)
        })?;
        let mut slot = self.slot.borrow_mut();
        slot.state = State::Sealed;
        let token = PreparedWorkerEdit {
            before: self.seed.before.clone(),
            after,
            owner: self.seed.owner.clone(),
            added: self.seed.added.clone(),
            epoch: self.seed.epoch,
            operation: slot.id,
            pin: slot.pins.clone(),
        };
        Ok((token, WorkerEditResources { slot: self.slot.clone() }))
    }
    /// Idempotent transfer of the SAME depot registration, also after seal.
    pub fn rollback(&mut self) -> WorkerEditResources {
        let mut slot = self.slot.borrow_mut();
        if !matches!(slot.state, State::Sealed | State::Retired) {
            slot.state = State::Poisoned;
        }
        WorkerEditResources { slot: self.slot.clone() }
    }
}
impl Drop for WorkerEditOperation {
    fn drop(&mut self) {
        let mut slot = self.slot.borrow_mut();
        if slot.state == State::Preparing {
            slot.state = State::Poisoned;
        }
        // Armed rescue: all resources are ALREADY in slot. No allocation/IO.
    }
}
pub struct WorkerEditResources {
    slot: Rc<RefCell<Slot>>,
}
impl WorkerEditResources {
    fn reserve_admission<P>(&self, token: &PreparedWorkerEdit) -> io::Result<InstallCharge> {
        let bytes = InstallAdmission::<P>::metadata_bytes();
        let mut slot = self.slot.borrow_mut();
        if slot.state != State::Sealed || slot.id != token.operation || !Arc::ptr_eq(&slot.pins, &token.pin) {
            return Err(invalid("install resources do not own sealed token"));
        }
        let next = slot.install_metadata.checked_add(bytes).ok_or_else(overflow)?;
        let peak = slot.vm_metadata.checked_add(next).ok_or_else(overflow)?;
        slot.allocation.as_mut().ok_or_else(|| invalid("missing sealed allocation"))?.reserve_external(peak)?;
        slot.install_metadata = next;
        Ok(InstallCharge { slot: self.slot.clone(), bytes })
    }
    pub fn operation_id(&self) -> u64 {
        self.slot.borrow().id
    }
    pub fn reserved_usage(&self) -> WorkerCapacityUsage {
        let slot = self.slot.borrow();
        *slot.capacity.borrow()
    }
    /// Enumerate exact owned partial/complete paths on the worker for an
    /// identifiable shutdown quarantine. Never grants ownership of collisions.
    pub fn visit_quarantine_paths(&self, mut visit: impl FnMut(&std::path::Path)) {
        if let Some(allocation) = &self.slot.borrow().allocation {
            allocation.visit_owned_paths(&mut visit);
        }
    }
    /// Call only AFTER exact VM recipe spool cleanup succeeded (or if no file
    /// was created). Dropping this handle never releases external disk quota.
    pub fn external_spool_retired(&mut self) {
        self.slot.borrow_mut().external_released = true;
    }
    pub fn try_retire(self, depot: &mut WorkerEditDepot) -> Result<(), Self> {
        if !depot.owns(&self.slot) {
            let retired = self.slot.borrow().state == State::Retired;
            return if retired { Ok(()) } else { Err(self) };
        }
        depot.retire();
        if depot.owns(&self.slot) { Err(self) } else { Ok(()) }
    }
}
/// No public constructor/Clone/after mutation. Returned tokens are Send; the
/// depot's physical ledger, not token Drop, owns explicit cleanup.
pub struct PreparedWorkerEdit {
    before: Snapshot,
    after: Snapshot,
    owner: Arc<()>,
    added: Arc<AddBudget>,
    epoch: u64,
    operation: u64,
    pin: Arc<()>,
}
impl PreparedWorkerEdit {
    pub fn operation_id(&self) -> u64 {
        self.operation
    }
    pub fn snapshot(&self) -> &Snapshot {
        &self.after
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallRefusalReason {
    WrongOwner,
    Stale,
    Compound,
    Exhausted,
    Cancelled,
    Admission,
}
/// Implemented by the existing host lane's OWNED staged model-history/journal/
/// outbox permit. check verifies its live reservation and revision/key bindings.
/// Called ONLY before mutation. Publication itself is NOT a callback from core.
pub trait WorkerPublication {
    fn check(&self) -> io::Result<()>;
}
pub struct InstallAdmission<P> {
    inner: Box<InstallAdmissionInner<P>>,
}
struct InstallAdmissionInner<P> {
    history: PreparedHistory,
    publication: P,
    owner: Arc<()>,
    before: Snapshot,
    epoch: u64,
    operation: u64,
    token_pin: Arc<()>,
    _charge: InstallCharge,
}
impl<P> InstallAdmission<P> {
    /// Include this exact layout for EACH concurrently live admission, in
    /// addition to VM scratch, when reserving the operation metadata peak.
    pub fn metadata_bytes() -> usize {
        std::alloc::Layout::new::<InstallAdmissionInner<P>>().size()
    }
}
struct InstallCharge {
    slot: Rc<RefCell<Slot>>,
    bytes: usize,
}
impl Drop for InstallCharge {
    fn drop(&mut self) {
        let mut slot = self.slot.borrow_mut();
        slot.install_metadata = slot.install_metadata.checked_sub(self.bytes).expect("owned install metadata charge");
        // The real plan/high-water reservation stays held until worker cleanup;
        // dropping this guard never unlinks or frees unretired capacity.
    }
}
pub struct AdmissionRefusal<P> {
    pub publication: P,
    pub error: io::Error,
}
pub struct InstallRefusal<P> {
    pub reason: InstallRefusalReason,
    pub token: PreparedWorkerEdit,
    pub admission: InstallAdmission<P>,
}
/// The buffer HAS committed. Publication delivery failure must retain this
/// value for idempotent redelivery, never retry installation or report unchanged.
pub struct InstalledWorkerEdit<P> {
    pub snapshot: Snapshot,
    pub history_depth: (usize, usize),
    pub operation_id: u64,
    pub publication: P,
    _pin: Arc<()>,
}
pub struct CommittedPresentationFailure<P> {
    pub committed: InstalledWorkerEdit<P>,
    pub error: io::Error,
}
impl<P> InstalledWorkerEdit<P> {
    pub fn presentation_failed(self, error: io::Error) -> CommittedPresentationFailure<P> {
        CommittedPresentationFailure { committed: self, error }
    }
}
impl Buffer {
    pub fn worker_edit_seed(&self) -> io::Result<WorkerEditSeed> {
        if self.transaction.is_some() {
            return Err(invalid("open compound transaction"));
        }
        if self.worker_epoch == u64::MAX {
            return Err(overflow());
        }
        Ok(WorkerEditSeed {
            before: self.snapshot(),
            added: self.added.clone(),
            owner: self.owner.clone(),
            epoch: self.worker_epoch,
        })
    }
    fn worker_refusal(&self, token: &PreparedWorkerEdit) -> Option<InstallRefusalReason> {
        if !Arc::ptr_eq(&self.owner, &token.owner) || !Arc::ptr_eq(&self.added, &token.added) {
            Some(InstallRefusalReason::WrongOwner)
        } else if self.transaction.is_some() {
            Some(InstallRefusalReason::Compound)
        } else if self.worker_epoch == u64::MAX {
            Some(InstallRefusalReason::Exhausted)
        } else if self.worker_epoch != token.epoch || !same_snapshot(&self.snapshot, &token.before) {
            Some(InstallRefusalReason::Stale)
        } else {
            None
        }
    }
    /// Prepares capacity in the ACTUAL existing history, with the host's real
    /// staged publication permit owned as a value. Every fallible check precedes
    /// root replacement. This method and install run on the authority worker.
    pub fn admit_worker_install<P: WorkerPublication>(
        &mut self,
        token: &PreparedWorkerEdit,
        resources: &WorkerEditResources,
        model_bytes: usize,
        publication: P,
    ) -> Result<InstallAdmission<P>, AdmissionRefusal<P>> {
        let result = (|| {
            if self.worker_refusal(token).is_some() {
                return Err(invalid("worker install preflight state"));
            }
            publication.check()?;
            let charge = resources.reserve_admission::<P>(token)?;
            let before = self.snapshot();
            let history = self.history.prepare_worker(before, token.after.clone(), model_bytes)?;
            Ok((history, charge))
        })();
        match result {
            Ok((history, charge)) => {
                let inner = Box::new(InstallAdmissionInner {
                    history,
                    publication,
                    owner: self.owner.clone(),
                    before: self.snapshot(),
                    epoch: self.worker_epoch,
                    operation: token.operation,
                    token_pin: token.pin.clone(),
                    _charge: charge,
                });
                #[cfg(test)]
                {
                    resources.slot.borrow_mut().admission_allocations += 1;
                }
                Ok(InstallAdmission { inner })
            }
            Err(error) => Err(AdmissionRefusal { publication, error }),
        }
    }
    pub fn install_prepared_edit<P: WorkerPublication>(
        &mut self,
        token: PreparedWorkerEdit,
        admission: InstallAdmission<P>,
        cancelled: &AtomicBool,
    ) -> Result<InstalledWorkerEdit<P>, InstallRefusal<P>> {
        let reason = self.worker_refusal(&token).or_else(|| {
            if cancelled.load(Ordering::Acquire) {
                Some(InstallRefusalReason::Cancelled)
            } else if !Arc::ptr_eq(&admission.inner.owner, &self.owner)
                || admission.inner.epoch != self.worker_epoch
                || admission.inner.operation != token.operation
                || !Arc::ptr_eq(&admission.inner.token_pin, &token.pin)
                || !same_snapshot(&admission.inner.before, &self.snapshot)
                || admission.inner.publication.check().is_err()
            {
                Some(InstallRefusalReason::Admission)
            } else {
                None
            }
        });
        let reason = reason.or_else(|| cancelled.load(Ordering::Acquire).then_some(InstallRefusalReason::Cancelled));
        if let Some(reason) = reason {
            return Err(InstallRefusal { reason, token, admission });
        }
        // No fallible callback/read/import/reserve from this point. Existing
        // history eviction/destruction occurs on this persistent document worker.
        let InstallAdmissionInner { history, publication, .. } = *admission.inner;
        self.worker_epoch += 1; // exhaustion checked above; never wraps
        self.typing = None;
        self.snapshot = token.after;
        self.history.install_prepared(history);
        Ok(InstalledWorkerEdit {
            snapshot: self.snapshot(),
            history_depth: self.history.depth(),
            operation_id: token.operation,
            publication,
            _pin: token.pin,
        })
    }
}
fn same_snapshot(a: &Snapshot, b: &Snapshot) -> bool {
    Arc::ptr_eq(&a.originals, &b.originals)
        && match (&a.root, &b.root) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
}
fn merge(into: &mut SpillRetirementReport, from: SpillRetirementReport) {
    into.retired += from.retired;
    into.live += from.live;
    into.cleanup_pending += from.cleanup_pending;
    if into.first_error.is_none() {
        into.first_error = from.first_error;
    }
}
pub(crate) fn poll(cancelled: &AtomicBool) -> io::Result<()> {
    if cancelled.load(Ordering::Acquire) { Err(io::ErrorKind::Interrupted.into()) } else { Ok(()) }
}
pub(crate) fn capacity() -> io::Error {
    io::Error::new(io::ErrorKind::WouldBlock, "worker capacity not admitted")
}
pub(crate) fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
pub(crate) fn overflow() -> io::Error {
    invalid("nonwrapping worker epoch/capacity exhausted")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::piece_tree::WorkerFault;
    use crate::{
        RecoveryAssembler, RecoveryLimits, SpillRetirementLimits,
        encoding::{self, Encoding, SavePlan},
    };
    use std::panic::{AssertUnwindSafe, catch_unwind};

    #[test]
    fn worker_edit_repeated_rollback_after_retirement_preserves_terminal_state_and_exact_slot_quota() {
        let buffer = Buffer::empty();
        let mut depot = depot();
        let before = depot.usage();
        let mut op = operation(&buffer, &mut depot);
        let resources = op.rollback();
        let id = resources.operation_id();
        depot.retire();
        assert!(depot.slots.is_empty(), "retired registration must already be removed");
        let retained = WorkerCapacityUsage { metadata_bytes: before.metadata_bytes + slot_metadata(), ..before };
        assert_eq!(depot.usage(), retained, "only the still-live slot/pin allocations remain charged");
        for _ in 0..3 {
            let again = op.rollback();
            assert_eq!(again.operation_id(), id);
            assert!(Rc::ptr_eq(&resources.slot, &again.slot), "rollback returns the same resource identity");
            assert!(
                matches!(resources.slot.borrow().state, State::Retired),
                "rollback cannot resurrect a terminal slot as poisoned"
            );
            assert!(again.try_retire(&mut depot).is_ok(), "retired handles acknowledge cleanup idempotently");
            let report = depot.retire();
            assert_eq!(report.live, 0);
            assert_eq!(report.cleanup_pending, 0);
            assert!(depot.slots.is_empty());
            assert_eq!(depot.usage(), retained, "repetition neither releases nor resurrects quota");
        }
        drop(op);
        assert_eq!(depot.usage(), retained, "the original resource handle still owns the slot");
        drop(resources);
        assert_eq!(depot.usage(), before, "last slot/pin drop returns exactly its retained metadata charge");
        assert!(depot.try_close().is_ok());
    }

    fn decoded_fixture(
        original: &[u8],
        encoding: Encoding,
        bom: usize,
    ) -> (std::path::PathBuf, crate::decoded_backing::DecodedBacking, Buffer) {
        use crate::decoded_backing::{DecodedBacking, Interpretation};
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "kwil-worker-copy-fixture-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        eprintln!("owned decoded fixture: {}", path.display());
        let owner = DecodedBacking::prepare_from_reader(
            &mut std::io::Cursor::new(original),
            original.len() as u64,
            Interpretation::new(encoding, bom).unwrap(),
            &path,
            &AtomicBool::new(false),
        )
        .unwrap();
        let buffer = Buffer::from_decoded(owner.snapshot());
        (path, owner, buffer)
    }

    #[test]
    fn worker_edit_decoded_alias_copy_deduplicates_origin_and_holds_only_private_pins() {
        let encoding = Encoding::for_label("big5").unwrap();
        let (path, owner, foreign) = decoded_fixture(b"\x88\x62Z", encoding, 0);
        let second_view = Buffer::from_decoded(owner.snapshot());
        let buffer = Buffer::with_add_memory_limit(0);
        let mut depot = depot();
        let mut op = operation(&buffer, &mut depot);
        let first = op.import(&foreign.snapshot(), 0..foreign.len(), &AtomicBool::new(false)).unwrap();
        let second = op.import(&second_view.snapshot(), 0..second_view.len(), &AtomicBool::new(false)).unwrap();
        assert_eq!(paths(&op).len(), 3, "same origin behind different indexes is copied once");
        let private_origin = match first.source_chunks(0..first.len()).unwrap().next().unwrap().unwrap() {
            crate::piece_tree::SourceChunk::Decoded { origin, .. } => origin.clone(),
            _ => panic!("valid alias must keep decoded text provenance"),
        };
        assert!(!private_origin.same_origin(&owner.snapshot()));
        drop((foreign, second_view));
        assert!(owner.try_retire().is_ok(), "weak identity must not pin foreign files");
        let (token, resources) = op.seal(first.concat(&second).unwrap(), &AtomicBool::new(false)).unwrap();
        let mut out = Vec::new();
        encoding::write_snapshot(token.snapshot(), SavePlan { encoding, bom: false, line_ending: None }, &mut out)
            .unwrap();
        assert_eq!(out, b"\x88\x62Z\x88\x62Z");
        let owned = paths(&op);
        drop((token, first, second, op, buffer));
        assert!(depot.retire().live > 0, "private decoded range pin must defer cleanup");
        assert!(owned.iter().all(|p| p.exists()));
        drop(private_origin);
        assert!(resources.try_retire(&mut depot).is_ok());
        assert!(owned.iter().all(|p| !p.exists()));
        assert!(depot.try_close().is_ok());
        std::fs::remove_dir(path).unwrap();
    }

    #[test]
    fn worker_edit_decoded_partial_alias_keeps_scalar_semantics_without_inventing_raw_offsets() {
        let encoding = Encoding::for_label("big5").unwrap();
        let (path, owner, foreign) = decoded_fixture(b"\x88\x62Z", encoding, 0);
        let buffer = Buffer::empty();
        let mut depot = depot();
        let mut op = operation(&buffer, &mut depot);
        // Interior of the two-scalar Big5 atom, but at a valid UTF8 scalar boundary.
        let partial = op.import(&foreign.snapshot(), 2..4, &AtomicBool::new(false)).unwrap();
        drop(foreign);
        assert!(owner.try_retire().is_ok());
        let (token, _) = op.seal(partial, &AtomicBool::new(false)).unwrap();
        assert_eq!(raw(token.snapshot()), "\u{304}".as_bytes());
        let mut out = Vec::new();
        assert!(
            encoding::write_snapshot(token.snapshot(), SavePlan { encoding, bom: false, line_ending: None }, &mut out)
                .is_err()
        );
        assert!(out.is_empty(), "partial alias cannot borrow the complete atom's raw bytes");
        drop((token, op, buffer));
        assert!(depot.try_close().is_ok());
        std::fs::remove_dir(path).unwrap();
    }

    #[test]
    fn worker_edit_decoded_cancel_partial_failure_and_unwind_keep_exact_registered_paths() {
        for failure in
            [WorkerFault::Write, WorkerFault::DecodedCopyCancel, WorkerFault::DecodedCopyPanic, WorkerFault::Map]
        {
            let (path, owner, foreign) = decoded_fixture(b"\x88\x62Z", Encoding::for_label("big5").unwrap(), 0);
            let buffer = Buffer::with_add_memory_limit(0);
            let mut depot = depot();
            let mut op = operation(&buffer, &mut depot);
            fault(&op, failure);
            let cancel = AtomicBool::new(false);
            let attempt = catch_unwind(AssertUnwindSafe(|| op.import(&foreign.snapshot(), 0..foreign.len(), &cancel)));
            if failure == WorkerFault::DecodedCopyPanic {
                assert!(attempt.is_err());
            } else {
                let result = match attempt {
                    Ok(result) => result,
                    Err(_) => panic!("unexpected unwind"),
                };
                let error = result.err().expect("copy fault must refuse");
                if failure == WorkerFault::DecodedCopyCancel {
                    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
                }
            }
            assert!(op.reserve(0, 0).is_err(), "failed copy poisons even after caught unwind");
            let owned = paths(&op);
            assert_eq!(owned.len(), 3);
            assert!(owned.iter().any(|p| p.exists()), "partial files remain registered");
            let mut resources = op.rollback();
            let mut quarantine = Vec::new();
            resources.visit_quarantine_paths(|p| quarantine.push(p.to_owned()));
            assert!(quarantine.iter().any(|p| p.is_dir()));
            fault(&op, WorkerFault::Cleanup);
            let reserved = depot.usage();
            drop((op, foreign, buffer));
            assert!(owner.try_retire().is_ok(), "failure retains no foreign generation");
            assert!(depot.retire().cleanup_pending > 0);
            assert_eq!(depot.usage(), reserved, "failed unlink holds exact capacity");
            resources.slot.borrow_mut().allocation.as_mut().unwrap().fault = WorkerFault::None;
            resources.external_spool_retired();
            assert!(resources.try_retire(&mut depot).is_ok());
            assert!(quarantine.iter().all(|p| !p.exists()));
            assert!(depot.try_close().is_ok());
            std::fs::remove_dir(path).unwrap();
        }
    }

    #[test]
    fn worker_edit_seal_rejects_certified_baseline_split_decoded_scalar() {
        let (path, owner, buffer) = decoded_fixture(b"\xe9\0", Encoding::UTF16_LE, 0);
        let baseline = buffer.snapshot();
        let history = buffer.history_depth();
        let mut depot = depot();
        let mut op = operation(&buffer, &mut depot);
        let candidate = baseline.slice(0..1).unwrap();
        assert_eq!(candidate.len(), 1);
        assert!(
            op.seal(candidate, &AtomicBool::new(false)).is_err(),
            "certification does not authorize a split UTF8 scalar"
        );
        assert!(same_snapshot(&baseline, &buffer.snapshot()));
        assert_eq!(buffer.history_depth(), history);
        drop((baseline, buffer, op));
        assert!(depot.try_close().is_ok());
        assert!(owner.try_retire().is_ok());
        std::fs::remove_dir(path).unwrap();
    }

    #[test]
    fn worker_edit_epoch_terminal_reservation_covers_near_max_compound_and_history_routes() {
        for remaining in [1, 2, 3] {
            for terminal in 0..5 {
                let mut buffer = text(b"a");
                buffer.worker_epoch = u64::MAX - remaining;
                let baseline = buffer.snapshot();
                let history = buffer.history_depth();
                if remaining == 1 {
                    assert!(catch_unwind(AssertUnwindSafe(|| buffer.begin())).is_err());
                    assert!(buffer.transaction.is_none());
                    assert_eq!(buffer.worker_epoch, u64::MAX - 1);
                    assert!(same_snapshot(&baseline, &buffer.snapshot()));
                    assert_eq!(buffer.history_depth(), history);
                    continue;
                }
                assert!(buffer.begin());
                if remaining == 2 {
                    assert!(buffer.insert(1, b"X").is_err());
                    assert!(same_snapshot(&baseline, &buffer.snapshot()));
                } else {
                    buffer.insert(1, b"X").unwrap();
                }
                assert_eq!(buffer.worker_epoch, u64::MAX - 1);
                // No other mutation (including nested begin) may consume the
                // reserved terminal epoch or alter the compound before refusal.
                assert!(catch_unwind(AssertUnwindSafe(|| buffer.begin())).is_err());
                assert!(buffer.delete(0..1).is_err());
                assert!(buffer.insert_snapshot_range(0, &Snapshot::default(), 0..0).is_err());
                assert!(buffer.insert_typing(0, b"Y", std::time::Duration::ZERO).is_err());
                assert!(catch_unwind(AssertUnwindSafe(|| buffer.set_undo_memory_limit(0))).is_err());
                match terminal {
                    0 => {
                        assert!(buffer.commit());
                    }
                    1 => {
                        assert!(buffer.commit_coalesced(false));
                    }
                    2 => {
                        assert!(buffer.cancel());
                        assert_eq!(bytes(&buffer.snapshot()), b"a");
                    }
                    3 => {
                        assert!(buffer.undo());
                    }
                    4 => {
                        buffer.redo();
                    }
                    _ => unreachable!(),
                }
                assert_eq!(buffer.worker_epoch, u64::MAX);
                assert!(buffer.transaction.is_none(), "terminal route must always finish");
                assert!(buffer.insert(0, b"Z").is_err());
            }
        }
        for redo in [false, true] {
            let mut buffer = text(b"a");
            if redo {
                assert!(buffer.undo());
            }
            buffer.worker_epoch = u64::MAX - 1;
            if redo {
                assert!(buffer.redo());
            } else {
                assert!(buffer.undo());
            }
            assert_eq!(buffer.worker_epoch, u64::MAX, "history operation spends one epoch");
        }
    }

    #[test]
    fn worker_edit_all_nonterminal_routes_preserve_a_compound_terminal_epoch() {
        for route in 0..12 {
            let mut buffer = text(b"a");
            buffer.worker_epoch = u64::MAX - 3;
            assert!(buffer.begin());
            buffer.insert(1, b"X").unwrap();
            let before = buffer.snapshot();
            let history = buffer.history_depth();
            let refused = match route {
                0 => buffer.insert(0, b"Y").is_err(),
                1 => buffer.insert_snapshot_range(0, &Snapshot::default(), 0..0).is_err(),
                2 => buffer.delete(0..1).is_err(),
                3 => buffer.insert_typing(0, b"Y", std::time::Duration::ZERO).is_err(),
                4 => buffer.checkpoint_current(Snapshot::default()).is_err(),
                5 => buffer.install_worker_edit(Snapshot::default()).is_err(),
                6 => buffer.enable_resident_additions(1).is_err(),
                _ => catch_unwind(AssertUnwindSafe(|| match route {
                    7 => {
                        buffer.begin();
                    }
                    8 => buffer.charge_latest_model_history(usize::MAX),
                    9 => buffer.end_typing_burst(),
                    10 => buffer.set_undo_memory_limit(0),
                    11 => buffer.recovery_install(Snapshot::default()),
                    _ => unreachable!(),
                }))
                .is_err(),
            };
            assert!(refused, "route{route} must preflight before mutation");
            assert!(same_snapshot(&before, &buffer.snapshot()), "route{route}");
            assert_eq!(buffer.worker_epoch, u64::MAX - 1, "route{route}");
            assert_eq!(buffer.history_depth(), history, "route{route}");
            assert!(buffer.transaction.is_some());
            assert!(buffer.cancel());
            assert_eq!(bytes(&buffer.snapshot()), b"a");
            assert_eq!(buffer.worker_epoch, u64::MAX);
            assert!(buffer.transaction.is_none());
        }
    }

    #[test]
    fn worker_edit_rescue_slot_and_pin_metadata_refuse_before_allocating_admission() {
        let mut depot = depot();
        let before = depot.usage();
        for metadata in [0, slot_metadata() - 1] {
            let mut p = plan();
            p.metadata_bytes = metadata;
            assert!(depot.admit(p).is_err());
            assert_eq!(depot.usage(), before);
            assert!(depot.slots.is_empty());
        }
        let mut p = plan();
        p.metadata_bytes = slot_metadata();
        let admission = depot.admit(p).unwrap();
        assert_eq!(depot.usage().metadata_bytes - before.metadata_bytes, slot_metadata());
        assert_eq!(depot.usage().slots, 1);
        drop(admission);
        assert!(depot.try_close().is_ok());
        let buffer = Buffer::empty();
        let mut depot = self::depot();
        let before = depot.usage();
        let mut op = operation(&buffer, &mut depot);
        let resources = op.rollback();
        drop(op);
        depot.retire();
        assert_eq!(
            depot.usage().metadata_bytes - before.metadata_bytes,
            slot_metadata(),
            "retired handles still retain actual slot/pin allocations"
        );
        drop(resources);
        assert_eq!(depot.usage(), before);
        assert!(depot.try_close().is_ok());
    }

    #[test]
    fn worker_edit_resident_fallback_reservation_returns_once_on_success_and_unwind() {
        for unwind in [false, true] {
            let buffer = Buffer::with_add_memory_limit(1);
            let budget = buffer.added.clone();
            let source = text(b"X");
            let mut depot = depot();
            let mut op = operation(&buffer, &mut depot);
            if unwind {
                fault(&op, WorkerFault::ResidentPanic);
            }
            let attempt =
                catch_unwind(AssertUnwindSafe(|| op.import(&source.snapshot(), 0..1, &AtomicBool::new(false))));
            let first = if unwind {
                assert!(attempt.is_err());
                assert_eq!(budget.used.load(Ordering::Relaxed), 0);
                None
            } else {
                let first = match attempt {
                    Ok(Ok(snapshot)) => snapshot,
                    _ => panic!("resident import failed"),
                };
                assert_eq!(budget.used.load(Ordering::Relaxed), 1);
                Some(first)
            };
            assert_eq!(op.slot.borrow().allocation.as_ref().unwrap().worker_reserved_disk(), 0);
            assert!(paths(&op).is_empty());
            if !unwind {
                let second = op.import(&source.snapshot(), 0..1, &AtomicBool::new(false)).unwrap();
                assert_eq!(
                    op.slot.borrow().allocation.as_ref().unwrap().worker_reserved_disk(),
                    piece_tree::ADD_BLOCK as u64
                );
                assert_eq!(paths(&op).len(), 1, "shared cap fallback retains one whole physical charge");
                drop(second);
            }
            fault(&op, WorkerFault::None);
            drop((first, op, buffer));
            assert!(depot.try_close().is_ok());
            assert_eq!(budget.used.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn worker_edit_each_physical_file_uses_checked_64k_disk_units() {
        let unit = piece_tree::ADD_BLOCK as u64;
        for (bytes, charge) in [(0, unit), (1, unit), (unit, unit), (unit + 1, 2 * unit)] {
            assert_eq!(piece_tree::worker_disk_charge(bytes).unwrap(), charge);
        }
        assert!(piece_tree::worker_disk_charge(u64::MAX).is_err());
        let source = text(b"ab");
        for cap in [unit - 1, unit] {
            let buffer = Buffer::with_add_memory_limit(0);
            let mut depot = depot();
            let mut p = plan();
            p.disk_bytes = cap;
            let a = depot.admit(p).unwrap();
            let mut op = buffer
                .worker_edit_seed()
                .unwrap()
                .begin(a, &mut depot, &AtomicBool::new(false))
                .map_err(|r| r.error)
                .unwrap();
            let first = op.import(&source.snapshot(), 0..1, &AtomicBool::new(false));
            if cap < unit {
                assert!(first.is_err());
                assert!(paths(&op).is_empty());
            } else {
                assert!(first.is_ok());
                assert!(op.import(&source.snapshot(), 1..2, &AtomicBool::new(false)).is_err());
                assert_eq!(paths(&op).len(), 1, "second tiny file requires a second whole unit before create");
            }
            drop((first, op, buffer));
            assert!(depot.try_close().is_ok());
        }
        let mut recovered = Buffer::with_add_memory_limit(0);
        recovered.insert(0, b"a").unwrap();
        recovered.insert(1, b"b").unwrap();
        let lease = recovered.snapshot().retain_spills(lease_limits(), || false).unwrap();
        assert_eq!(lease.worker_disk_bytes().unwrap(), 2 * unit, "recovered files use identical units");
        let mut tiny =
            WorkerEditDepot::new(WorkerDepotLimits { disk_bytes: unit, metadata_bytes: 4 << 20, slots: 2 }).unwrap();
        let (mut lease, error) = tiny.attach_recovered(lease).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(tiny.usage().disk_bytes, 0);
        drop(recovered);
        assert_eq!(lease.retire().retired, 2);
        assert!(tiny.try_close().is_ok());
        let mut overflow_depot =
            WorkerEditDepot::new(WorkerDepotLimits { disk_bytes: u64::MAX, metadata_bytes: 128 << 20, slots: 2 })
                .unwrap();
        let mut huge = plan();
        huge.disk_bytes = u64::MAX;
        let admitted = overflow_depot.admit(huge).unwrap();
        let before = overflow_depot.usage();
        let mut extra = plan();
        extra.disk_bytes = unit;
        assert!(overflow_depot.admit(extra).is_err(), "aggregate disk addition must not wrap");
        assert_eq!(overflow_depot.usage(), before);
        drop(admitted);
        assert!(overflow_depot.try_close().is_ok());
    }

    #[test]
    fn worker_edit_decoded_three_file_capacity_refuses_before_any_creation() {
        let unit = piece_tree::ADD_BLOCK as u64;
        for cap in [2 * unit, 3 * unit - 1, 3 * unit] {
            let (path, owner, foreign) = decoded_fixture(b"\x88\x62Z", Encoding::for_label("big5").unwrap(), 0);
            let buffer = Buffer::with_add_memory_limit(0);
            let mut depot = depot();
            let mut p = plan();
            p.disk_bytes = cap;
            let a = depot.admit(p).unwrap();
            let mut op = buffer
                .worker_edit_seed()
                .unwrap()
                .begin(a, &mut depot, &AtomicBool::new(false))
                .map_err(|r| r.error)
                .unwrap();
            let result = op.import(&foreign.snapshot(), 0..foreign.len(), &AtomicBool::new(false));
            if cap < 3 * unit {
                assert!(result.is_err());
                assert!(paths(&op).is_empty());
                assert_eq!(op.slot.borrow().allocation.as_ref().unwrap().creates, 0);
            } else {
                assert!(result.is_ok());
                assert_eq!(paths(&op).len(), 3);
                assert_eq!(op.slot.borrow().allocation.as_ref().unwrap().worker_reserved_disk(), 3 * unit);
            }
            drop((result, foreign, op, buffer));
            assert!(owner.try_retire().is_ok());
            assert!(depot.try_close().is_ok());
            std::fs::remove_dir(path).unwrap();
        }
    }

    #[test]
    fn worker_edit_prefix_claim_cancellation_and_unwind_release_only_acquired_claims() {
        for failure in [WorkerFault::ClaimPrefixCancel, WorkerFault::ClaimPrefixPanic] {
            let buffer = Buffer::with_add_memory_limit(0);
            let source = text(b"ab");
            let mut depot = depot();
            let mut op = operation(&buffer, &mut depot);
            let a = op.import(&source.snapshot(), 0..1, &AtomicBool::new(false)).unwrap();
            let b = op.import(&source.snapshot(), 1..2, &AtomicBool::new(false)).unwrap();
            let owned = paths(&op);
            fault(&op, failure);
            let cancel = AtomicBool::new(false);
            let attempt = catch_unwind(AssertUnwindSafe(|| op.seal(a.concat(&b).unwrap(), &cancel)));
            if failure == WorkerFault::ClaimPrefixPanic {
                assert!(attempt.is_err());
            } else {
                match attempt {
                    Ok(result) => assert!(result.is_err()),
                    Err(_) => panic!("unexpected unwind"),
                }
            }
            assert!(op.reserve(0, 0).is_err());
            let first = a.retain_spills(lease_limits(), || false).unwrap();
            let second = b.retain_spills(lease_limits(), || false).unwrap();
            assert!(owned.iter().all(|p| p.exists()));
            fault(&op, WorkerFault::None);
            drop((first, second, a, b, op, buffer));
            assert!(depot.try_close().is_ok());
            assert!(owned.iter().all(|p| !p.exists()));
        }
    }
    fn plan() -> WorkerEditPlan {
        WorkerEditPlan {
            disk_bytes: 4 << 20,
            recipe_bytes: 0,
            metadata_bytes: 16 << 20,
            max_new_blocks: 128,
            max_certificates: 128,
            max_tree_nodes: 4096,
            max_path_bytes: 4096,
        }
    }
    fn depot() -> WorkerEditDepot {
        WorkerEditDepot::new(WorkerDepotLimits { disk_bytes: 32 << 20, metadata_bytes: 128 << 20, slots: 8 }).unwrap()
    }
    fn operation(buffer: &Buffer, depot: &mut WorkerEditDepot) -> WorkerEditOperation {
        let seed = buffer.worker_edit_seed().unwrap();
        let admission = depot.admit(plan()).unwrap();
        seed.begin(admission, depot, &AtomicBool::new(false)).map_err(|r| r.error).unwrap()
    }
    fn text(bytes: &[u8]) -> Buffer {
        let mut b = Buffer::empty();
        b.insert(0, bytes).unwrap();
        b
    }
    fn bytes(snapshot: &Snapshot) -> Vec<u8> {
        let mut bytes = Vec::new();
        snapshot.write_to(&mut bytes).unwrap();
        bytes
    }
    fn raw(snapshot: &Snapshot) -> Vec<u8> {
        let mut out = Vec::new();
        encoding::write_snapshot(
            snapshot,
            SavePlan { encoding: Encoding::UTF8, bom: false, line_ending: None },
            &mut out,
        )
        .unwrap();
        out
    }
    fn paths(operation: &WorkerEditOperation) -> Vec<std::path::PathBuf> {
        operation.slot.borrow().allocation.as_ref().unwrap().worker_paths()
    }
    fn fault(operation: &WorkerEditOperation, fault: WorkerFault) {
        operation.slot.borrow_mut().allocation.as_mut().unwrap().fault = fault;
    }
    fn lease_limits() -> SpillRetirementLimits {
        SpillRetirementLimits { max_block_references: 64, max_metadata_bytes: 4 << 20, max_path_bytes: 4096 }
    }
    /// Fixture owns actual preallocated journal/model/outbox staging memory.
    /// It tests core ownership and late reservation refusal; existing host lane
    /// integration and durable restart framing remain separate acceptance.
    struct Publication {
        ready: Arc<AtomicBool>,
        journal: Box<[u8]>,
        outbox: Box<[u8]>,
    }
    impl WorkerPublication for Publication {
        fn check(&self) -> io::Result<()> {
            if self.ready.load(Ordering::Acquire) { Ok(()) } else { Err(capacity()) }
        }
    }
    fn publication() -> Publication {
        Publication {
            ready: Arc::new(AtomicBool::new(true)),
            journal: vec![7; 32].into_boxed_slice(),
            outbox: vec![8; 32].into_boxed_slice(),
        }
    }

    #[test]
    fn worker_edit_admission_layout_boundary_concurrent_retry_and_refusal_preserve_exact_owners() {
        let bytes = InstallAdmission::<Publication>::metadata_bytes();
        assert!(bytes > 0);
        assert!(
            std::mem::size_of::<InstallRefusal<Publication>>() <= 128,
            "by-value refusal contains a token and one-word admission, independent of P size"
        );
        // Empty operation: no descriptors/tree/index. Its known fixed scratch
        // and slot overhead leave exactly the tested number of Box layouts.
        let fixed = 4 * piece_tree::ADD_BLOCK
            + std::mem::size_of::<WorkerAllocation>()
            + 1024
            + slot_metadata()
            + piece_tree::recovery_tree_work(&None).unwrap();
        let vm = 256;
        for allowance in [bytes - 1, bytes, 2 * bytes - 1, 2 * bytes] {
            let mut buffer = Buffer::empty();
            let before = buffer.snapshot();
            let mut depot = depot();
            let mut p = plan();
            p.max_new_blocks = 0;
            p.max_certificates = 0;
            p.max_tree_nodes = 0;
            p.max_path_bytes = 0;
            p.metadata_bytes = fixed + vm + allowance;
            let a = depot.admit(p).unwrap();
            let mut op = buffer
                .worker_edit_seed()
                .unwrap()
                .begin(a, &mut depot, &AtomicBool::new(false))
                .map_err(|r| r.error)
                .unwrap();
            op.reserve(0, vm).unwrap();
            let (token, resources) = op.seal(buffer.snapshot(), &AtomicBool::new(false)).unwrap();
            let original_pin = token.pin.clone();
            let publication = publication();
            let publication_pin = publication.ready.clone();
            let journal = publication.journal.as_ptr();
            let first = buffer.admit_worker_install(&token, &resources, 0, publication);
            if allowance < bytes {
                let refusal = first.err().expect("one byte short must refuse before Box/history allocation");
                assert_eq!(refusal.error.kind(), io::ErrorKind::WouldBlock);
                assert!(Arc::ptr_eq(&refusal.publication.ready, &publication_pin));
                assert_eq!(refusal.publication.journal.as_ptr(), journal);
                assert_eq!(resources.slot.borrow().admission_allocations, 0);
                assert_eq!(resources.slot.borrow().install_metadata, 0);
                assert!(same_snapshot(&before, &buffer.snapshot()));
                assert_eq!(buffer.history_depth(), (0, 0));
                assert!(Arc::ptr_eq(&token.pin, &original_pin));
                drop((refusal, token, resources, original_pin, op, buffer));
                assert!(depot.try_close().is_ok());
                continue;
            }
            let first = first.map_err(|r| r.error).unwrap();
            let allowed = if allowance >= 2 * bytes { 2 } else { 1 };
            let second = if allowed == 2 {
                Some(
                    buffer
                        .admit_worker_install(&token, &resources, 0, self::publication())
                        .map_err(|r| r.error)
                        .unwrap(),
                )
            } else {
                None
            };
            let publication = self::publication();
            let retry_pin = publication.ready.clone();
            let retry_journal = publication.journal.as_ptr();
            let denied = buffer
                .admit_worker_install(&token, &resources, 0, publication)
                .err()
                .expect("same-token concurrent admissions must share the actual layout cap");
            assert_eq!(denied.error.kind(), io::ErrorKind::WouldBlock);
            assert!(Arc::ptr_eq(&denied.publication.ready, &retry_pin));
            assert_eq!(denied.publication.journal.as_ptr(), retry_journal);
            assert_eq!(resources.slot.borrow().install_metadata, allowed * bytes);
            assert_eq!(resources.slot.borrow().admission_allocations, allowed);
            drop(first);
            assert_eq!(resources.slot.borrow().install_metadata, (allowed - 1) * bytes);
            let retry =
                buffer.admit_worker_install(&token, &resources, 0, denied.publication).map_err(|r| r.error).unwrap();
            let inner = (&*retry.inner) as *const InstallAdmissionInner<Publication>;
            let allocations = resources.slot.borrow().admission_allocations;
            let refused = buffer.install_prepared_edit(token, retry, &AtomicBool::new(true)).err().unwrap();
            assert_eq!(refused.reason, InstallRefusalReason::Cancelled);
            assert_eq!((&*refused.admission.inner) as *const InstallAdmissionInner<Publication>, inner);
            assert!(Arc::ptr_eq(&refused.token.pin, &original_pin));
            assert!(Arc::ptr_eq(&refused.admission.inner.publication.ready, &retry_pin));
            assert_eq!(refused.admission.inner.publication.journal.as_ptr(), retry_journal);
            assert_eq!(
                resources.slot.borrow().admission_allocations,
                allocations,
                "refused install must not allocate a new error container"
            );
            assert!(same_snapshot(&before, &buffer.snapshot()));
            assert_eq!(buffer.history_depth(), (0, 0));
            let installed = buffer
                .install_prepared_edit(refused.token, refused.admission, &AtomicBool::new(false))
                .unwrap_or_else(|r| panic!("{:?}", r.reason));
            assert_eq!(
                resources.slot.borrow().admission_allocations,
                allocations,
                "successful install consumes the preboxed inner without allocating"
            );
            assert_eq!(resources.slot.borrow().install_metadata, (allowed - 1) * bytes);
            drop((second, installed, original_pin, resources, op, buffer));
            assert!(depot.try_close().is_ok());
        }
    }

    #[test]
    fn worker_edit_admission_resources_require_exact_sealed_slot_before_layout_allocation() {
        let mut buffer = Buffer::empty();
        let mut first = depot();
        let mut second = depot();
        let mut a = operation(&buffer, &mut first);
        let mut b = operation(&buffer, &mut second);
        let (token, resources) = a.seal(buffer.snapshot(), &AtomicBool::new(false)).unwrap();
        let (other, foreign) = b.seal(buffer.snapshot(), &AtomicBool::new(false)).unwrap();
        assert_eq!(token.operation_id(), other.operation_id());
        let publication = publication();
        let pin = publication.ready.clone();
        let denied = buffer.admit_worker_install(&token, &foreign, 0, publication).err().unwrap();
        assert!(Arc::ptr_eq(&denied.publication.ready, &pin));
        assert_eq!(resources.slot.borrow().admission_allocations, 0);
        assert_eq!(foreign.slot.borrow().admission_allocations, 0);
        assert_eq!(resources.slot.borrow().install_metadata, 0);
        assert_eq!(foreign.slot.borrow().install_metadata, 0);
        let prepared =
            buffer.admit_worker_install(&token, &resources, 0, denied.publication).map_err(|r| r.error).unwrap();
        assert_eq!(resources.slot.borrow().admission_allocations, 1);
        drop((prepared, token, other, resources, foreign, a, b, buffer));
        assert!(first.try_close().is_ok());
        assert!(second.try_close().is_ok());
    }
    fn admission(
        buffer: &mut Buffer,
        token: &PreparedWorkerEdit,
        resources: &WorkerEditResources,
    ) -> InstallAdmission<Publication> {
        buffer.admit_worker_install(token, resources, 256, publication()).map_err(|r| r.error).unwrap()
    }
    #[test]
    fn worker_edit_actual_install_one_history_undo_redo_and_presentation_failure() {
        let mut buffer = Buffer::with_add_memory_limit(0);
        let mut depot = depot();
        let source = text(b"replacement");
        let mut op = operation(&buffer, &mut depot);
        let after = op.import(&source.snapshot(), 0..source.len(), &AtomicBool::new(false)).unwrap();
        let (token, _resources) = op.seal(after, &AtomicBool::new(false)).unwrap();
        let id = token.operation_id();
        let prepared = admission(&mut buffer, &token, &_resources);
        let installed = buffer
            .install_prepared_edit(token, prepared, &AtomicBool::new(false))
            .unwrap_or_else(|r| panic!("{:?}", r.reason));
        assert_eq!(installed.operation_id, id);
        assert_eq!(installed.history_depth, (1, 0));
        assert_eq!(installed.publication.journal.len(), 32);
        assert_eq!(installed.publication.outbox.len(), 32);
        let failed = installed.presentation_failed(io::ErrorKind::BrokenPipe.into());
        assert_eq!(bytes(&buffer.snapshot()), b"replacement", "presentation failure is already committed");
        assert!(buffer.undo());
        assert!(buffer.is_empty());
        assert!(buffer.redo());
        assert_eq!(bytes(&buffer.snapshot()), b"replacement");
        drop((failed, op, buffer));
        assert!(depot.try_close().is_ok());
    }
    #[test]
    fn worker_edit_wrong_owner_stale_empty_root_aba_and_compound_return_exact_token() {
        for kind in 0..3 {
            let mut buffer = Buffer::empty();
            let mut other = Buffer::empty();
            let mut depot = depot();
            let mut op = operation(&buffer, &mut depot);
            let (token, _resources) = op.seal(buffer.snapshot(), &AtomicBool::new(false)).unwrap();
            let pin = token.pin.clone();
            let id = token.operation_id();
            let prepared = admission(&mut buffer, &token, &_resources);
            if kind == 1 {
                buffer.insert(0, b"x").unwrap();
                assert!(buffer.undo());
            }
            if kind == 2 {
                assert!(buffer.begin());
            }
            let destination = if kind == 0 { &mut other } else { &mut buffer };
            let history = destination.history_depth();
            let before = destination.snapshot();
            let refusal = destination.install_prepared_edit(token, prepared, &AtomicBool::new(false)).err().unwrap();
            assert_eq!(
                refusal.reason,
                [InstallRefusalReason::WrongOwner, InstallRefusalReason::Stale, InstallRefusalReason::Compound][kind]
            );
            assert_eq!(refusal.token.operation_id(), id);
            assert!(Arc::ptr_eq(&pin, &refusal.token.pin));
            assert!(same_snapshot(&before, &destination.snapshot()));
            assert_eq!(destination.history_depth(), history);
            if kind == 2 {
                assert!(destination.transaction.is_some());
            }
            drop((refusal, pin, op));
            drop((buffer, other));
            assert!(depot.try_close().is_ok());
        }
    }
    #[test]
    fn worker_edit_all_mutator_epoch_routes_and_exhaustion_are_fail_closed() {
        for route in 0..16 {
            let mut buffer = Buffer::empty();
            let seed = buffer.worker_edit_seed().unwrap();
            match route {
                0 => buffer.insert(0, b"").unwrap(),
                1 => buffer.insert_snapshot_range(0, &Snapshot::default(), 0..0).unwrap(),
                2 => buffer.delete(0..0).unwrap(),
                3 => {
                    buffer.begin();
                }
                4 => {
                    buffer.commit();
                }
                5 => {
                    buffer.cancel();
                }
                6 => buffer.charge_latest_model_history(0),
                7 => buffer.end_typing_burst(),
                8 => {
                    buffer.undo();
                }
                9 => {
                    buffer.redo();
                }
                10 => buffer.set_undo_memory_limit(0),
                11 => buffer.checkpoint_current(Snapshot::default()).unwrap(),
                12 => buffer.install_worker_edit(Snapshot::default()).unwrap(),
                13 => buffer.insert_typing(0, b"", std::time::Duration::ZERO).unwrap(),
                14 => buffer.recovery_install(Snapshot::default()),
                15 => {
                    assert!(buffer.enable_resident_additions(1).is_err());
                }
                _ => unreachable!(),
            }
            assert!(buffer.worker_epoch > seed.epoch, "route {route}");
            if route == 3 {
                assert!(buffer.worker_edit_seed().is_err(), "compound capture must refuse");
            }
        }
        let mut buffer = text(b"unchanged");
        buffer.worker_epoch = u64::MAX;
        let before = buffer.snapshot();
        let history = buffer.history_depth();
        assert!(buffer.worker_edit_seed().is_err());
        assert!(buffer.insert(0, b"X").is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| buffer.begin())).is_err());
        assert!(same_snapshot(&before, &buffer.snapshot()));
        assert_eq!(buffer.history_depth(), history);
        assert!(buffer.transaction.is_none());
    }
    #[test]
    fn worker_edit_nonempty_root_aba_and_directory_identity_are_independent_fences() {
        for directory_only in [false, true] {
            let mut buffer = text(b"base");
            let mut depot = depot();
            let mut op = operation(&buffer, &mut depot);
            let (token, _resources) = op.seal(buffer.snapshot(), &AtomicBool::new(false)).unwrap();
            let prepared = admission(&mut buffer, &token, &_resources);
            if directory_only {
                // Controlled private test fault: same epoch/root, different
                // directory identity. Public mutators cannot bypass the epoch.
                buffer.snapshot.originals = Arc::new(buffer.snapshot.originals.as_ref().clone());
            } else {
                buffer.insert(buffer.len(), b"x").unwrap();
                assert!(buffer.undo());
            }
            let before = buffer.snapshot();
            let history = buffer.history_depth();
            let refusal = buffer.install_prepared_edit(token, prepared, &AtomicBool::new(false)).err().unwrap();
            assert_eq!(refusal.reason, InstallRefusalReason::Stale);
            assert!(same_snapshot(&before, &buffer.snapshot()));
            assert_eq!(buffer.history_depth(), history);
            drop((before, refusal, op, buffer));
            assert!(depot.try_close().is_ok());
        }
    }
    #[test]
    fn worker_edit_admission_binds_exact_token_even_when_depot_ids_coincide() {
        let mut buffer = Buffer::empty();
        let mut first = depot();
        let mut second = depot();
        let x = text(b"X");
        let y = text(b"Y");
        let mut a = operation(&buffer, &mut first);
        let mut b = operation(&buffer, &mut second);
        let ax = a.import(&x.snapshot(), 0..1, &AtomicBool::new(false)).unwrap();
        let by = b.import(&y.snapshot(), 0..1, &AtomicBool::new(false)).unwrap();
        let (ta, _ra) = a.seal(ax, &AtomicBool::new(false)).unwrap();
        let (tb, _rb) = b.seal(by, &AtomicBool::new(false)).unwrap();
        assert_eq!(ta.operation_id(), tb.operation_id(), "local ids intentionally collide across depots");
        let aa = admission(&mut buffer, &ta, &_ra);
        let refusal = buffer.install_prepared_edit(tb, aa, &AtomicBool::new(false)).err().unwrap();
        assert_eq!(refusal.reason, InstallRefusalReason::Admission);
        assert!(buffer.is_empty());
        assert_eq!(buffer.history_depth(), (0, 0));
        let installed = buffer
            .install_prepared_edit(ta, refusal.admission, &AtomicBool::new(false))
            .unwrap_or_else(|r| panic!("{:?}", r.reason));
        assert_eq!(bytes(&buffer.snapshot()), b"X");
        assert!(buffer.undo());
        assert!(buffer.is_empty());
        assert!(buffer.redo());
        assert_eq!(bytes(&buffer.snapshot()), b"X");
        drop((refusal.token, installed, buffer, a, b));
        assert!(first.try_close().is_ok());
        assert!(second.try_close().is_ok());
    }
    #[test]
    fn worker_edit_full_candidate_validation_rejects_foreign_and_same_owner_grafts() {
        for own in [false, true] {
            let mut buffer = Buffer::empty();
            let foreign = text(b"untracked");
            let mut depot = depot();
            let mut op = operation(&buffer, &mut depot);
            let after = if own {
                buffer.insert(0, b"untracked").unwrap();
                buffer.snapshot()
            } else {
                foreign.snapshot()
            };
            let before = buffer.snapshot();
            assert_eq!(op.seal(after, &AtomicBool::new(false)).err().unwrap().kind(), io::ErrorKind::InvalidInput);
            assert!(same_snapshot(&before, &buffer.snapshot()));
            assert!(op.seal(Snapshot::default(), &AtomicBool::new(false)).is_err(), "poison cannot mint token");
            drop((op, buffer, foreign));
            assert!(depot.try_close().is_ok());
        }
    }
    #[test]
    fn worker_edit_disk_and_metadata_admission_precede_create_and_growth() {
        let buffer = Buffer::with_add_memory_limit(0);
        let source = text(b"1234");
        for cap in [0, 3] {
            let mut depot = depot();
            let mut p = plan();
            p.disk_bytes = cap;
            let seed = buffer.worker_edit_seed().unwrap();
            let a = depot.admit(p).unwrap();
            let mut op = seed.begin(a, &mut depot, &AtomicBool::new(false)).map_err(|r| r.error).unwrap();
            assert_eq!(
                op.import(&source.snapshot(), 0..4, &AtomicBool::new(false)).err().unwrap().kind(),
                io::ErrorKind::WouldBlock
            );
            assert_eq!(op.slot.borrow().allocation.as_ref().unwrap().creates, 0);
            assert!(paths(&op).is_empty());
            drop(op);
            assert!(depot.try_close().is_ok());
        }
        let mut depot = depot();
        let mut p = plan();
        p.metadata_bytes = 0;
        let seed = buffer.worker_edit_seed().unwrap();
        assert!(depot.admit(p).is_err(), "zero metadata refuses before rescue-slot allocation");
        drop(seed);
        assert!(depot.try_close().is_ok());
    }
    #[test]
    fn worker_edit_partial_write_map_and_cleanup_failure_keep_quarantine_capacity() {
        for failure in [WorkerFault::Write, WorkerFault::Map] {
            let buffer = Buffer::with_add_memory_limit(0);
            let source = text(b"partial");
            let mut depot = depot();
            let mut op = operation(&buffer, &mut depot);
            fault(&op, failure);
            assert!(op.import(&source.snapshot(), 0..source.len(), &AtomicBool::new(false)).is_err());
            let paths = paths(&op);
            assert_eq!(paths.len(), 1);
            assert!(paths[0].exists());
            let usage = depot.usage();
            eprintln!("owned partial worker fixture: {}", paths[0].display());
            fault(&op, WorkerFault::Cleanup);
            let pass = depot.retire();
            assert_eq!(pass.cleanup_pending, 1);
            assert_eq!(depot.usage(), usage);
            assert!(paths[0].exists());
            fault(&op, WorkerFault::None);
            assert_eq!(depot.retire().retired, 1);
            assert!(!paths[0].exists());
            assert_eq!(depot.usage().disk_bytes, 0);
            drop(op);
            assert!(depot.try_close().is_ok());
        }
    }
    #[test]
    fn worker_edit_create_collision_never_conveys_foreign_file_ownership() {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "kwil-worker-collision-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&path).unwrap();
        std::io::Write::write_all(&mut file, b"foreign owner sentinel").unwrap();
        drop(file);
        let buffer = Buffer::with_add_memory_limit(0);
        let source = text(b"replacement");
        let mut depot = depot();
        let mut op = operation(&buffer, &mut depot);
        op.slot.borrow_mut().allocation.as_mut().unwrap().collision_path = Some(path.clone());
        assert!(op.import(&source.snapshot(), 0..source.len(), &AtomicBool::new(false)).is_err());
        assert_eq!(op.slot.borrow().allocation.as_ref().unwrap().creates, 0);
        let resources = op.rollback();
        let mut owned = Vec::new();
        resources.visit_quarantine_paths(|p| owned.push(p.to_path_buf()));
        assert!(owned.is_empty(), "attempted name is not an owned file");
        drop((op, resources));
        assert!(depot.try_close().is_ok());
        assert_eq!(std::fs::read(&path).unwrap(), b"foreign owner sentinel");
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn worker_edit_caught_unwind_with_live_operation_is_poisoned_and_rescued() {
        let buffer = Buffer::with_add_memory_limit(0);
        let source = text(b"unwind");
        let mut depot = depot();
        let mut op = operation(&buffer, &mut depot);
        fault(&op, WorkerFault::AfterOpenPanic);
        assert!(
            catch_unwind(AssertUnwindSafe(|| op.import(&source.snapshot(), 0..source.len(), &AtomicBool::new(false))))
                .is_err()
        );
        let paths = paths(&op);
        assert_eq!(paths.len(), 1);
        assert!(paths[0].exists());
        fault(&op, WorkerFault::None);
        assert_eq!(
            op.import(&source.snapshot(), 0..source.len(), &AtomicBool::new(false)).err().unwrap().kind(),
            io::ErrorKind::InvalidInput
        );
        let id = op.operation_id();
        assert_eq!(op.rollback().operation_id(), id);
        assert_eq!(op.rollback().operation_id(), id);
        drop(op);
        assert_eq!(depot.retire().retired, 1);
        assert!(!paths[0].exists());
        assert!(depot.try_close().is_ok());
    }
    #[test]
    fn worker_edit_cancellation_during_write_and_after_claim_preserves_exact_owner() {
        for failure in [WorkerFault::CancelAfterWrite, WorkerFault::CancelAfterClaim] {
            let buffer = Buffer::with_add_memory_limit(0);
            let source = text(b"cancel");
            let mut depot = depot();
            let mut op = operation(&buffer, &mut depot);
            let cancelled = AtomicBool::new(false);
            fault(&op, failure);
            let result =
                op.import(&source.snapshot(), 0..source.len(), &cancelled).and_then(|after| op.seal(after, &cancelled));
            assert_eq!(result.err().unwrap().kind(), io::ErrorKind::Interrupted);
            let paths = paths(&op);
            assert_eq!(paths.len(), 1);
            assert!(paths[0].exists());
            let resources = op.rollback();
            let id = resources.operation_id();
            assert_eq!(id, op.operation_id());
            assert_eq!(depot.usage().disk_bytes, plan().disk_bytes);
            assert!(buffer.is_empty());
            drop((op, resources));
            assert_eq!(depot.retire().retired, 1);
            assert!(!paths[0].exists());
            assert!(depot.try_close().is_ok());
        }
    }
    #[test]
    fn worker_edit_worker_only_retirement_survives_frontend_last_token_drop() {
        let buffer = Buffer::with_add_memory_limit(0);
        let source = text(b"private");
        let mut depot = depot();
        let mut op = operation(&buffer, &mut depot);
        let imported = op.import(&source.snapshot(), 0..source.len(), &AtomicBool::new(false)).unwrap();
        let (token, _resources) = op.seal(imported, &AtomicBool::new(false)).unwrap();
        let paths = paths(&op);
        drop(op);
        assert_eq!(depot.retire().retired, 0);
        std::thread::spawn(move || drop(token)).join().unwrap();
        assert!(paths.iter().all(|p| p.exists()));
        assert_eq!(depot.retire().retired, 1);
        assert!(paths.iter().all(|p| !p.exists()));
        assert!(depot.try_close().is_ok());
    }
    #[test]
    fn worker_edit_baseline_lease_new_only_claims_and_foreign_spill_copy() {
        let mut buffer = Buffer::with_add_memory_limit(0);
        buffer.insert(0, b"base").unwrap();
        let mut depot = depot();
        let lease = buffer.snapshot().retain_spills(lease_limits(), || false).unwrap();
        depot.attach_recovered(lease).map_err(|(_, e)| e).unwrap();
        let baseline = buffer.snapshot();
        let baseline_paths = piece_tree::spill_paths(&baseline.root);
        let mut foreign = Buffer::with_add_memory_limit(0);
        foreign.insert(0, b"foreign").unwrap();
        let foreign_snapshot = foreign.snapshot();
        let foreign_paths = piece_tree::spill_paths(&foreign_snapshot.root);
        let mut foreign_lease = foreign_snapshot.retain_spills(lease_limits(), || false).unwrap();
        let mut op = operation(&buffer, &mut depot);
        let imported = op.import(&foreign_snapshot, 0..foreign_snapshot.len(), &AtomicBool::new(false)).unwrap();
        let new_paths = paths(&op);
        assert_eq!(new_paths.len(), 1);
        assert_ne!(new_paths, foreign_paths);
        let repeated = imported.concat(&imported).unwrap();
        let after = baseline.concat(&repeated).unwrap();
        let (token, _resources) = op.seal(after, &AtomicBool::new(false)).unwrap();
        assert_eq!(paths(&op).len(), 1, "one claim per actual allocation, not per logical use");
        assert!(baseline.retain_spills(lease_limits(), || false).is_err(), "baseline lease still exclusive");
        let prepared = admission(&mut buffer, &token, &_resources);
        drop((foreign, foreign_snapshot));
        assert_eq!(foreign_lease.retire().retired, 1);
        buffer
            .install_prepared_edit(token, prepared, &AtomicBool::new(false))
            .unwrap_or_else(|r| panic!("{:?}", r.reason));
        assert_eq!(bytes(&buffer.snapshot()), b"baseforeignforeign");
        assert!(buffer.undo());
        assert!(buffer.redo());
        drop((op, imported, repeated, baseline, buffer));
        assert!(depot.try_close().is_ok());
        assert!(new_paths.iter().chain(&baseline_paths).all(|p| !p.exists()));
    }
    #[test]
    fn worker_edit_all_opaque_children_and_raw_projection_have_real_ownership() {
        let mut replay = RecoveryAssembler::new(RecoveryLimits {
            add_memory_bytes: 0,
            undo_memory_bytes: 1 << 20,
            max_origins: 0,
            max_records: 64,
            max_pieces: 64,
            max_blocks: 64,
            max_metadata_bytes: 8 << 20,
        })
        .unwrap();
        let mut revision = replay.begin_revision();
        revision.begin_fragment().unwrap();
        revision.append_bytes(b"A\xff\xc2\xc2", || false).unwrap();
        revision.replace(0..0, || false).unwrap();
        revision.promote_raw_v1(|| false).unwrap();
        revision.commit(|| false).unwrap();
        let foreign = replay.finish(|| false).unwrap();
        let mut lease = foreign.snapshot().retain_spills(lease_limits(), || false).unwrap();
        let mut buffer = Buffer::with_add_memory_limit(0);
        let mut depot = depot();
        let mut op = operation(&buffer, &mut depot);
        let imported = op.import(&foreign.snapshot(), 0..foreign.len(), &AtomicBool::new(false)).unwrap();
        assert_eq!(paths(&op).len(), 3, "logical, raw AND offsets each registered before IO");
        let (token, _resources) = op.seal(imported, &AtomicBool::new(false)).unwrap();
        let prepared = admission(&mut buffer, &token, &_resources);
        drop(foreign);
        assert_eq!(lease.retire().retired, 3);
        buffer
            .install_prepared_edit(token, prepared, &AtomicBool::new(false))
            .unwrap_or_else(|r| panic!("{:?}", r.reason));
        assert_eq!(raw(&buffer.snapshot()), b"A\xff\xc2\xc2");
        assert!(buffer.undo());
        assert!(buffer.redo());
        assert_eq!(raw(&buffer.snapshot()), b"A\xff\xc2\xc2");
        drop((buffer, op));
        assert!(depot.try_close().is_ok());
    }
    #[test]
    fn worker_edit_decoded_utf16_big5_import_owns_payloads_before_foreign_retirement() {
        use crate::decoded_backing::{DecodedBacking, Interpretation};
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        for (original, encoding, bom) in [
            (b"\xff\xfeA\0\0\xd8B\0".as_slice(), Encoding::UTF16_LE, 2),
            (b"\x88\x62Z".as_slice(), Encoding::for_label("big5").unwrap(), 0),
        ] {
            let path = std::env::temp_dir().join(format!(
                "kwil-worker-provenance-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            let owner = DecodedBacking::prepare_from_reader(
                &mut std::io::Cursor::new(original),
                original.len() as u64,
                Interpretation::new(encoding, bom).unwrap(),
                &path,
                &AtomicBool::new(false),
            )
            .unwrap();
            let foreign = Buffer::from_decoded(owner.snapshot());
            let mut buffer = Buffer::with_add_memory_limit(0);
            let mut depot = depot();
            let mut op = operation(&buffer, &mut depot);
            let after = op.import(&foreign.snapshot(), 0..foreign.len(), &AtomicBool::new(false)).unwrap();
            let (token, _resources) = op.seal(after, &AtomicBool::new(false)).unwrap();
            let prepared = admission(&mut buffer, &token, &_resources);
            drop(foreign);
            assert!(owner.try_retire().is_ok(), "foreign full origin must not be pinned by copied payloads");
            buffer
                .install_prepared_edit(token, prepared, &AtomicBool::new(false))
                .unwrap_or_else(|r| panic!("{:?}", r.reason));
            let mut out = Vec::new();
            encoding::write_snapshot(
                &buffer.snapshot(),
                SavePlan { encoding, bom: bom != 0, line_ending: None },
                &mut out,
            )
            .unwrap();
            assert_eq!(out, original);
            assert!(buffer.undo());
            assert!(buffer.redo());
            out.clear();
            encoding::write_snapshot(
                &buffer.snapshot(),
                SavePlan { encoding, bom: bom != 0, line_ending: None },
                &mut out,
            )
            .unwrap();
            assert_eq!(out, original);
            drop((buffer, op));
            assert!(depot.try_close().is_ok());
            std::fs::remove_dir(path).unwrap();
        }
    }
    #[test]
    fn worker_edit_claim_overlap_rolls_back_fresh_prefix_only() {
        let buffer = Buffer::with_add_memory_limit(0);
        let source = text(b"ab");
        let mut depot = depot();
        let mut op = operation(&buffer, &mut depot);
        let a = op.import(&source.snapshot(), 0..1, &AtomicBool::new(false)).unwrap();
        let b = op.import(&source.snapshot(), 1..2, &AtomicBool::new(false)).unwrap();
        let other = b.retain_spills(lease_limits(), || false).unwrap();
        assert!(op.seal(a.concat(&b).unwrap(), &AtomicBool::new(false)).is_err());
        let prefix = a.retain_spills(lease_limits(), || false).unwrap();
        assert!(
            b.retain_spills(lease_limits(), || false).is_err(),
            "attempt must not clear preexisting competing claim"
        );
        drop((prefix, other, a, b, op));
        assert!(depot.try_close().is_ok());
    }
    #[test]
    fn worker_edit_late_publication_refusal_cancel_and_small_history_keep_contracts() {
        for small in [false, true] {
            let mut buffer = Buffer::empty();
            if small {
                buffer.set_undo_memory_limit(0);
            }
            let source = text(b"Z");
            let mut depot = depot();
            let mut op = operation(&buffer, &mut depot);
            let after = op.import(&source.snapshot(), 0..1, &AtomicBool::new(false)).unwrap();
            let (token, _resources) = op.seal(after, &AtomicBool::new(false)).unwrap();
            let prepared = admission(&mut buffer, &token, &_resources);
            let ready = prepared.inner.publication.ready.clone();
            ready.store(false, Ordering::Release);
            let refusal = buffer.install_prepared_edit(token, prepared, &AtomicBool::new(false)).err().unwrap();
            assert_eq!(refusal.reason, InstallRefusalReason::Admission);
            assert!(buffer.is_empty());
            ready.store(true, Ordering::Release);
            let refusal =
                buffer.install_prepared_edit(refusal.token, refusal.admission, &AtomicBool::new(true)).err().unwrap();
            assert_eq!(refusal.reason, InstallRefusalReason::Cancelled);
            assert!(buffer.is_empty());
            let installed = buffer
                .install_prepared_edit(refusal.token, refusal.admission, &AtomicBool::new(false))
                .unwrap_or_else(|r| panic!("{:?}", r.reason));
            assert_eq!(installed.history_depth, if small { (0, 0) } else { (1, 0) });
            drop((buffer, installed, op));
            assert!(depot.try_close().is_ok());
        }
    }
    #[test]
    fn worker_edit_external_recipe_reservation_is_held_until_exact_owner_ack() {
        let buffer = Buffer::empty();
        let mut depot = depot();
        let mut p = plan();
        p.recipe_bytes = 64;
        let a = depot.admit(p).unwrap();
        let seed = buffer.worker_edit_seed().unwrap();
        let mut op = seed.begin(a, &mut depot, &AtomicBool::new(false)).map_err(|r| r.error).unwrap();
        op.reserve(64, 4096).unwrap();
        let mut resources = op.rollback();
        let usage = depot.usage();
        drop(op);
        depot.retire();
        assert_eq!(depot.usage(), usage);
        resources.external_spool_retired();
        depot.retire();
        assert_eq!(depot.usage().disk_bytes, 0);
        assert!(depot.try_close().is_ok());
    }
    #[test]
    fn worker_edit_5001_streamed_imports_install_once_without_total_4096_cap() {
        let mut buffer = Buffer::with_add_memory_limit(5001);
        let budget = buffer.added.clone();
        let source = text(b"X");
        let mut depot = depot();
        let mut p = plan();
        p.metadata_bytes = 96 << 20;
        p.max_new_blocks = 5001;
        p.max_tree_nodes = 12000;
        let a = depot.admit(p).unwrap();
        let mut op = buffer
            .worker_edit_seed()
            .unwrap()
            .begin(a, &mut depot, &AtomicBool::new(false))
            .map_err(|r| r.error)
            .unwrap();
        op.reserve(0, 8 << 20).unwrap();
        let mut after = Snapshot::default();
        for _ in 0..5001 {
            let inserted = op.import(&source.snapshot(), 0..1, &AtomicBool::new(false)).unwrap();
            after = after.concat(&inserted).unwrap();
        }
        assert_eq!(after.len(), 5001);
        assert_eq!(buffer.add_memory_bytes(), 5001);
        assert!(paths(&op).is_empty());
        let (token, _resources) = op.seal(after, &AtomicBool::new(false)).unwrap();
        let prepared = admission(&mut buffer, &token, &_resources);
        let installed = buffer
            .install_prepared_edit(token, prepared, &AtomicBool::new(false))
            .unwrap_or_else(|r| panic!("{:?}", r.reason));
        assert_eq!(installed.history_depth, (1, 0));
        assert!(buffer.undo());
        assert!(buffer.is_empty());
        assert!(buffer.redo());
        assert_eq!(bytes(&buffer.snapshot()), vec![b'X'; 5001]);
        drop((buffer, installed, op));
        assert!(depot.try_close().is_ok());
        assert_eq!(budget.used.load(Ordering::Relaxed), 0);
    }
    #[test]
    fn worker_edit_shared_add_budget_activation_and_saved_original_guard_remain_intact() {
        let mut buffer = Buffer::with_add_memory_limit(0);
        let mut depot = depot();
        let mut op = operation(&buffer, &mut depot);
        let source = text(b"XY");
        buffer.enable_resident_additions(2).unwrap();
        let imported = op.import(&source.snapshot(), 0..2, &AtomicBool::new(false)).unwrap();
        assert_eq!(buffer.add_memory_bytes(), 2);
        assert!(paths(&op).is_empty());
        let second = op.import(&source.snapshot(), 0..2, &AtomicBool::new(false)).unwrap();
        assert_eq!(paths(&op).len(), 1);
        let (token, _resources) = op.seal(imported.concat(&second).unwrap(), &AtomicBool::new(false)).unwrap();
        assert!(
            buffer.admit_worker_install(&token, &_resources, 0, publication()).is_err(),
            "activation changed core epoch"
        );
        assert!(buffer.install_worker_edit(imported.clone()).is_err(), "saved-original guard is not a W2 mode flag");
        drop((token, imported, second, op, buffer));
        assert!(depot.try_close().is_ok());
    }
}
