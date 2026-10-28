//! Unexecuted ownership/prepayment fixtures. Prepared-row integration requires
//! ROOT's shared-engine continuation hook; no test-only unbounded adapter.
use super::*;
use kwil_core_text::compare_input::{
    AdmissionError, BuildStep, CaptureKind, Captured, PayloadSpace, ReadLeaseRetirementStep, RetirementStep,
    SourceLease, StoreAdmission, StorePaths, Usage, begin, read_lease_resident_charge,
};
use std::{
    ops::ControlFlow,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

#[derive(Clone)]
struct Budget {
    used: Arc<Mutex<Usage>>,
    limit: usize,
}
impl Budget {
    fn new() -> Self {
        Self { used: Arc::new(Mutex::new(Usage::default())), limit: usize::MAX }
    }
    fn usage(&self) -> Usage {
        *self.used.lock().unwrap()
    }
}
impl Admission for Budget {
    fn reserve(&mut self, additional: Usage) -> Result<(), AdmissionError> {
        let mut used = self.used.lock().unwrap();
        let resident = used.resident.checked_add(additional.resident).ok_or(AdmissionError)?;
        let disk = used.disk.checked_add(additional.disk).ok_or(AdmissionError)?;
        let handles = used.handles.checked_add(additional.handles).ok_or(AdmissionError)?;
        if resident > self.limit {
            return Err(AdmissionError);
        }
        *used = Usage { resident, disk, handles };
        Ok(())
    }
    fn release(&mut self, released: Usage) {
        let mut used = self.used.lock().unwrap();
        used.resident -= released.resident;
        used.disk -= released.disk;
        used.handles -= released.handles;
    }
}
struct Text {
    at: usize,
}
impl SourceLease for Text {
    fn identity(&self) -> InputIdentity {
        InputIdentity([101, 202, 303, 404])
    }
    fn validate(&mut self, work: &mut ReadWork, cancel: &AtomicBool) -> Step<()> {
        work.debit(0, 1, cancel)
    }
    fn capture(
        &mut self,
        logical: &mut [u8],
        _: &mut [u8],
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<Option<Captured>> {
        let bytes = b"A\r\nB\n";
        let n = bytes.len() - self.at;
        match work.debit(n as u64, 1, cancel) {
            Step::Ready(()) => {}
            Step::Pending => return Step::Pending,
            Step::Cancelled => return Step::Cancelled,
            Step::Failed(error) => return Step::Failed(error),
        }
        if n == 0 {
            return Step::Ready(None);
        }
        logical[..n].copy_from_slice(&bytes[self.at..]);
        self.at += n;
        Step::Ready(Some(Captured { logical: n, original: 0, kind: CaptureKind::Text, encoding: 0 }))
    }
}
fn cancel() -> AtomicBool {
    AtomicBool::new(false)
}
fn work() -> ReadWork {
    ReadWork::new(1024 * 1024, 4096)
}
fn fixture() -> (InputStore<Budget>, Budget) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let directory = std::env::temp_dir().join(format!(
        "kwil-compare-source-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let paths = StorePaths {
        logical: directory.join("logical"),
        original: directory.join("original"),
        lines: directory.join("lines"),
        atoms: directory.join("atoms"),
        directory,
    };
    let budget = Budget::new();
    let mut build = match begin(Text { at: 0 }, StoreAdmission { aggregate: budget.clone(), paths }) {
        ControlFlow::Continue(build) => build,
        _ => panic!("fixture begin"),
    };
    for _ in 0..32 {
        match build.step(work(), &cancel()) {
            BuildStep::Pending(next) => build = next,
            BuildStep::Ready { store, .. } => return (store, budget),
            _ => panic!("fixture build"),
        }
    }
    panic!("bounded fixture did not finish")
}
fn source(store: &InputStore<Budget>, budget: &Budget) -> CompareSource<Budget> {
    match begin_compare_source(store, budget.clone(), &mut work(), &cancel()) {
        CompareSourceStep::Ready(source) => source,
        _ => panic!("fixture source"),
    }
}
fn retire_source(source: CompareSource<Budget>) -> Budget {
    let retirement = match source.try_into_retirement() {
        Ok(retirement) => retirement,
        Err(_) => panic!("unexpected held lease"),
    };
    match retirement.step(work(), &cancel()) {
        ReadLeaseRetirementStep::Ready(budget) => budget,
        _ => panic!("source retirement"),
    }
}
fn retire_store(store: InputStore<Budget>, budget: &Budget) {
    match store.into_retirement().step(work(), &cancel()) {
        RetirementStep::Ready(_) => assert_eq!(budget.usage(), Usage::default()),
        _ => panic!("store retirement"),
    }
}

#[test]
fn source_preserves_all_identity_words_and_is_send_sync_for_production_accounting() {
    fn send_sync<T: Send + Sync + 'static>() {}
    send_sync::<CompareSource<Budget>>();
    let (store, budget) = fixture();
    let source = source(&store, &budget);
    assert_eq!(source.input_identity(), InputIdentity([101, 202, 303, 404]));
    assert_eq!(
        source.identity(),
        SourceIdentity { document_id: 101, source_epoch: 202, incarnation: 303, revision: 404 }
    );
    assert_eq!(source.lease().byte_len(), 5);
    assert_eq!(source.lease().line_count(), 3);
    retire_source(source);
    retire_store(store, &budget);
}

#[test]
fn read_owner_nonce_survives_moves_and_clones_but_not_equal_identity_leases() {
    let (store, budget) = fixture();
    let first = source(&store, &budget);
    let nonce = first.read_owner_id();
    let clone = first.try_clone().unwrap_or_else(|_| panic!("available source slot"));
    let moved = first;
    let second = source(&store, &budget);
    assert_eq!(moved.input_identity(), second.input_identity());
    assert_eq!(moved.read_owner_id(), nonce);
    assert_eq!(clone.read_owner_id(), nonce);
    assert_ne!(second.read_owner_id(), nonce);
    drop(clone);
    retire_source(moved);
    retire_source(second);
    retire_store(store, &budget);
}

#[test]
fn read_owner_nonce_exhaustion_never_wraps_or_reuses_an_id() {
    let counter = AtomicU64::new(u64::MAX - 1);
    assert_eq!(next_read_owner(&counter), Some(u64::MAX - 1));
    assert_eq!(next_read_owner(&counter), None);
    assert_eq!(next_read_owner(&counter), None);
    assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
}

#[test]
fn arc_header_is_admitted_before_construction_with_exact_refusal_token() {
    let (store, budget) = fixture();
    let baseline = budget.usage();
    let mut denied = budget.clone();
    // Enough for a standalone read lease, insufficient for its Arc/slots.
    denied.limit = baseline.resident + read_lease_resident_charge::<Budget>(0).unwrap();
    match begin_compare_source(&store, denied, &mut work(), &cancel()) {
        CompareSourceStep::Rejected { admission, error: InputError::Admission(_) } => {
            assert!(Arc::ptr_eq(&admission.used, &budget.used));
        }
        _ => panic!("expected header refusal"),
    }
    assert_eq!(budget.usage(), baseline);
    let source = source(&store, &budget);
    let extra = compare_source_transfer_charge::<Budget>().unwrap();
    assert_eq!(source.lease().usage().resident, read_lease_resident_charge::<Budget>(extra).unwrap());
    assert_eq!(budget.usage().handles, baseline.handles + 4);
    retire_source(source);
    assert_eq!(budget.usage(), baseline);
    retire_store(store, &budget);
}

#[test]
fn all_source_slots_are_bounded_and_retirement_refuses_exact_held_owner() {
    let (store, budget) = fixture();
    let source = source(&store, &budget);
    let baseline = budget.usage();
    // Fixed inline slots; no growing vector of owners in the fixture either.
    let mut slots: [Option<CompareSource<Budget>>; COMPARE_SOURCE_OWNERS - 1] = std::array::from_fn(|_| None);
    for slot in &mut slots {
        *slot = Some(source.try_clone().unwrap());
    }
    assert_eq!(source.owner_count(), COMPARE_SOURCE_OWNERS);
    assert!(matches!(source.try_clone(), Err(SourceOwnerLimit)));
    assert_eq!(budget.usage(), baseline);
    let source = match source.try_into_retirement() {
        Err(source) => source,
        Ok(_) => panic!("must retain exact held source"),
    };
    assert!(Arc::ptr_eq(source.shared.as_ref().unwrap(), slots[0].as_ref().unwrap().shared.as_ref().unwrap()));
    let mut out = [0; 5];
    assert!(matches!(
        source.lease().read_at(PayloadSpace::Logical, 0, &mut out, &mut work(), &cancel()),
        Step::Ready(5)
    ));
    assert_eq!(&out, b"A\r\nB\n");
    for slot in &mut slots {
        drop(slot.take());
    }
    assert_eq!(source.owner_count(), 1);
    assert_eq!(budget.usage(), baseline);
    let returned = retire_source(source);
    assert!(Arc::ptr_eq(&returned.used, &budget.used));
    retire_store(store, &budget);
}

#[test]
fn paused_destructors_remain_physical_owners_until_their_arc_decrements() {
    let (store, budget) = fixture();
    let source = source(&store, &budget);
    let baseline = budget.usage();
    let mut dropping_a = source.try_clone().unwrap();
    let mut dropping_b = source.try_clone().unwrap();
    let held_c = source.try_clone().unwrap();
    let held_d = source.try_clone().unwrap();
    let physical = || Arc::strong_count(source.shared.as_ref().unwrap());
    let mut replacement = None;

    // Reenter at the actual Drop path's before-decrement boundary. Two
    // overlapping destructors are suspended while clone admission runs. No
    // timing assumptions, helper source Arc, polling, threads or stress loop.
    dropping_a.drop_with(|| {
        assert_eq!(physical(), COMPARE_SOURCE_OWNERS);
        assert_eq!(source.owner_count(), physical());
        assert!(matches!(source.try_clone(), Err(SourceOwnerLimit)));
        assert_eq!(physical(), COMPARE_SOURCE_OWNERS);
        dropping_b.drop_with(|| {
            assert_eq!(physical(), COMPARE_SOURCE_OWNERS);
            assert!(matches!(source.try_clone(), Err(SourceOwnerLimit)));
            assert_eq!(physical(), COMPARE_SOURCE_OWNERS);
        });
        // B has now decremented its Arc; A is still a physical owner.
        assert_eq!(physical(), COMPARE_SOURCE_OWNERS - 1);
        replacement = Some(source.try_clone().unwrap());
        assert_eq!(physical(), COMPARE_SOURCE_OWNERS);
        assert!(matches!(source.try_clone(), Err(SourceOwnerLimit)));
        assert_eq!(physical(), COMPARE_SOURCE_OWNERS);
    });
    assert_eq!(physical(), COMPARE_SOURCE_OWNERS - 1);
    assert_eq!(budget.usage(), baseline);
    drop(replacement);
    drop((dropping_a, dropping_b, held_c, held_d));
    assert_eq!(source.owner_count(), 1);
    assert_eq!(budget.usage(), baseline);
    retire_source(source);
    retire_store(store, &budget);
}

#[test]
fn cooperative_credit_accumulates_scalars_before_io_and_counts_repeated_reads() {
    let (store, budget) = fixture();
    let source = source(&store, &budget);
    let mut credit = ReadCredit::new(ReadWork::new(10, 1));
    let before = source.lease().visits();
    let mut out = [0x55; 5];
    assert!(matches!(credit.step(&mut ReadWork::new(3, 1), &cancel()), Step::Pending));
    assert!(matches!(credit.step(&mut ReadWork::new(6, 0), &cancel()), Step::Pending));
    assert_eq!(source.lease().visits(), before);
    assert_eq!(out, [0x55; 5]);
    let mut paid = match credit.step(&mut ReadWork::new(1, 0), &cancel()) {
        Step::Ready(work) => work,
        _ => panic!("exact accumulated envelope"),
    };
    assert!(matches!(source.lease().read_at(PayloadSpace::Logical, 0, &mut out, &mut paid, &cancel()), Step::Ready(5)));
    assert_eq!(paid, ReadWork::default());
    assert_eq!(out, *b"A\r\nB\n");
    assert!(matches!(credit.step(&mut ReadWork::default(), &cancel()), Step::Pending));
    assert!(matches!(
        source.lease().read_at(PayloadSpace::Logical, 0, &mut out, &mut work(), &cancel()),
        Step::Ready(5)
    ));
    assert_eq!(source.lease().visits().calls[0], 2);
    assert_eq!(source.lease().visits().requested_bytes[0], 10);
    assert_eq!(source.lease().visits().charged_bytes, 20);
    retire_source(source);
    retire_store(store, &budget);
}

#[test]
fn cancellation_and_insufficient_work_before_creation_or_read_perform_no_io() {
    let (store, budget) = fixture();
    let baseline = budget.usage();
    assert!(matches!(
        begin_compare_source(&store, budget.clone(), &mut ReadWork::new(0, 3), &cancel()),
        CompareSourceStep::Pending(_)
    ));
    assert!(matches!(
        begin_compare_source(&store, budget.clone(), &mut work(), &AtomicBool::new(true)),
        CompareSourceStep::Cancelled(_)
    ));
    assert_eq!(budget.usage(), baseline);
    let source = source(&store, &budget);
    let mut credit = ReadCredit::new(ReadWork::new(10, 1));
    assert!(matches!(credit.step(&mut ReadWork::new(9, 1), &cancel()), Step::Pending));
    assert!(matches!(credit.step(&mut work(), &AtomicBool::new(true)), Step::Cancelled));
    assert_eq!(credit.paid(), ReadWork::default());
    assert_eq!(source.lease().visits().calls, [0; 4]);
    retire_source(source);
    retire_store(store, &budget);
}
