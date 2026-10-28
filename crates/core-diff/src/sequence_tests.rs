//! Small, unexecuted source fixtures. ROOT owns compiler/test admission.
use super::*;
use crate::store::{AlignmentAdmission, AlignmentPaths, RetirementStep};
use kwil_core_text::compare_input::{AdmissionError, Usage};
use std::{
    cell::{Cell, RefCell},
    ops::ControlFlow,
    path::PathBuf,
    rc::Rc,
    sync::atomic::{AtomicU64, Ordering},
};

#[derive(Clone, Default)]
struct Budget(Rc<RefCell<Usage>>);
impl Admission for Budget {
    fn reserve(&mut self, add: Usage) -> Result<(), AdmissionError> {
        let mut live = self.0.borrow_mut();
        let next = Usage {
            resident: live.resident.checked_add(add.resident).ok_or(AdmissionError)?,
            disk: live.disk.checked_add(add.disk).ok_or(AdmissionError)?,
            handles: live.handles.checked_add(add.handles).ok_or(AdmissionError)?,
        };
        if next.resident > 1024 * 1024 || next.disk > 4 * 1024 * 1024 || next.handles > 5 {
            return Err(AdmissionError);
        }
        *live = next;
        Ok(())
    }
    fn release(&mut self, released: Usage) {
        let mut live = self.0.borrow_mut();
        live.resident -= released.resident;
        live.disk -= released.disk;
        live.handles -= released.handles;
    }
}

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    root: PathBuf,
    records: Records<Budget>,
    budget: Budget,
}
impl Fixture {
    fn new() -> Self {
        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("kwil-sequence-{}-{serial}", std::process::id()));
        let paths = AlignmentPaths {
            files: ["forward", "reverse", "stack", "runs", "hunks"].map(|n| root.join(n)),
            directory: root.clone(),
        };
        let resident = paths.capacity().unwrap()
            + size_of::<Self>() * 3
            + size_of::<Sequence<Equality>>() * 3
            + transient_charge::<Equality>().unwrap()
            + size_of::<Pair<'_>>() * 3
            + size_of::<[Option<Event>; 32]>();
        let budget = Budget::default();
        let mut records = match Records::begin(AlignmentAdmission { aggregate: budget.clone(), paths }, resident) {
            ControlFlow::Continue(records) => records,
            ControlFlow::Break(_) => panic!("fixture admission"),
        };
        let cancel = AtomicBool::new(false);
        assert!(matches!(records.open(&mut ReadWork::new(0, 8), &cancel), Step::Ready(())));
        Self { root, records, budget }
    }
    fn finish(self) {
        let Self { root, records, budget } = self;
        let mut retirement = records.retirement();
        let cancel = AtomicBool::new(false);
        for _ in 0..20 {
            match retirement.step(ReadWork::new(0, 1), &cancel) {
                RetirementStep::Pending(next) => retirement = next,
                RetirementStep::Ready(_) => {
                    assert!(!root.exists());
                    assert_eq!(*budget.0.borrow(), Usage::default());
                    return;
                }
                _ => panic!("fixture retirement"),
            }
        }
        panic!("fixture retirement did not progress");
    }
}

struct Equality {
    a: u64,
    b: u64,
    stage: u8,
    credit: u8,
    left: u64,
}
struct Pair<'a> {
    left: &'a [u8],
    right: &'a [u8],
    descriptors: bool,
    starts: Cell<usize>,
    completions: usize,
}
impl<'a> Pair<'a> {
    fn new(left: &'a [u8], right: &'a [u8]) -> Self {
        Self { left, right, descriptors: false, starts: Cell::new(0), completions: 0 }
    }
}
impl SequencePair for Pair<'_> {
    type Equality = Equality;
    fn lengths(&self) -> [u64; 2] {
        [self.left.len() as u64, self.right.len() as u64]
    }
    fn begin_equal(&self, a: u64, b: u64) -> Equality {
        self.starts.set(self.starts.get() + 1);
        Equality { a, b, stage: 0, credit: 0, left: 0 }
    }
    fn equal<A: Admission>(
        &mut self,
        state: &mut Equality,
        records: &mut Records<A>,
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<bool> {
        if self.descriptors {
            if state.stage == 0 {
                state.left = gate!(records.read(store::RUNS, state.a, work, cancel))[0];
                state.stage = 1;
            }
            if state.stage == 1 {
                let right = gate!(records.read(store::RUNS, 8 + state.b, work, cancel))[0];
                assert_eq!(right, u64::from(self.right[state.b as usize]));
                assert_eq!(state.left, u64::from(self.left[state.a as usize]));
                state.stage = 2;
            }
        }
        while state.credit < 2 {
            gate!(work.debit(1, 0, cancel));
            state.credit += 1;
        }
        self.completions += 1;
        Step::Ready(self.left[state.a as usize] == self.right[state.b as usize])
    }
}

fn next(
    solver: &mut Sequence<Equality>,
    pair: &mut Pair<'_>,
    records: &mut Records<Budget>,
    quantum: u64,
) -> Option<Event> {
    let cancel = AtomicBool::new(false);
    for _ in 0..20_000 {
        match solver.next(pair, records, &mut ReadWork::new(quantum, 2), &cancel) {
            Step::Pending => {}
            Step::Ready(event) => return event,
            other => panic!("sequence failed: {other:?}"),
        }
    }
    panic!("small sequence did not progress");
}

fn distance(left: &[u8], right: &[u8]) -> u64 {
    assert!(left.len() <= 8 && right.len() <= 8);
    let mut dp = [[0_u64; 9]; 9];
    for (i, row) in dp.iter_mut().enumerate() {
        row[0] = i as u64;
    }
    for (j, entry) in dp[0].iter_mut().enumerate() {
        *entry = j as u64;
    }
    for i in 0..left.len() {
        for j in 0..right.len() {
            dp[i + 1][j + 1] = if left[i] == right[j] { dp[i][j] } else { 1 + dp[i][j + 1].min(dp[i + 1][j]) };
        }
    }
    dp[left.len()][right.len()]
}

fn solve(fixture: &mut Fixture, pair: &mut Pair<'_>, quantum: u64, layout: Layout) -> [Option<Event>; 32] {
    let mut solver = Sequence::new(pair.lengths(), layout);
    let mut events = [None; 32];
    let mut count = 0;
    let mut position = [0, 0];
    let mut edits = 0;
    while let Some(event) = next(&mut solver, pair, &mut fixture.records, quantum) {
        let rect = match event {
            Event::Equal(rect) | Event::Changed(rect) => rect,
        };
        assert_eq!(position, [rect.a, rect.c]);
        match event {
            Event::Equal(_) => {
                assert_eq!(rect.n(), rect.m());
                assert!(rect.n() > 0);
                assert_eq!(&pair.left[rect.a as usize..rect.b as usize], &pair.right[rect.c as usize..rect.d as usize]);
            }
            Event::Changed(_) => {
                assert!(rect.n() == 0 || rect.m() == 0);
                assert!(rect.n() > 0 || rect.m() > 0);
                edits += rect.n() + rect.m();
            }
        }
        position = [rect.b, rect.d];
        events[count] = Some(event);
        count += 1;
        solver.acknowledge(event).unwrap();
    }
    assert_eq!(position, pair.lengths());
    assert_eq!(edits, distance(pair.left, pair.right));
    assert_eq!(pair.starts.get(), pair.completions);
    events
}

#[test]
fn exact_small_dp_and_one_byte_order() {
    let mut fixture = Fixture::new();
    for (left, right) in [
        (&b""[..], &b""[..]),
        (b"", b"ab"),
        (b"ab", b""),
        (b"abc", b"abc"),
        (b"abc", b"xyz"),
        (b"ab", b"ba"),
        (b"abab", b"baba"),
        (b"kitten", b"sitten"),
        (b"abca", b"axya"),
        (b"aabba", b"abaab"),
    ] {
        let wide = solve(&mut fixture, &mut Pair::new(left, right), 4096, Layout::default());
        let tiny = solve(&mut fixture, &mut Pair::new(left, right), 1, Layout { forward: 3, reverse: 7, stack: 11 });
        assert_eq!(wide, tiny);
    }
    fixture.finish();
}

#[test]
fn held_event_serializes_one_byte_sink_and_descriptor_reads() {
    let mut fixture = Fixture::new();
    let cancel = AtomicBool::new(false);
    let mut pair = Pair::new(b"aba", b"aca");
    pair.descriptors = true;
    for (base, bytes) in [(0, pair.left), (8, pair.right)] {
        for (i, byte) in bytes.iter().enumerate() {
            assert!(matches!(
                fixture.records.write(
                    store::RUNS,
                    base + i as u64,
                    [u64::from(*byte), 0, 0, 0, 0, 0, 0, 0],
                    &mut ReadWork::new(128, 1),
                    &cancel
                ),
                Step::Ready(())
            ));
        }
    }
    let mut solver = Sequence::new(pair.lengths(), Layout::default());
    let first = next(&mut solver, &mut pair, &mut fixture.records, 1).unwrap();
    assert_eq!(first, Event::Equal(Rect { a: 0, b: 1, c: 0, d: 1 }));
    let calls = pair.starts.get();
    assert!(matches!(
        fixture.records.write(store::HUNKS, 0, [42; 8], &mut ReadWork::new(1, 1), &cancel),
        Step::Pending
    ));
    assert!(matches!(solver.acknowledge(Event::Changed(Rect::default())), Err(InputError::QueryChanged)));
    for _ in 0..256 {
        assert!(matches!(solver.next(&mut pair, &mut fixture.records, &mut ReadWork::new(0, 0), &cancel),
            Step::Ready(Some(event)) if event == first));
        assert_eq!(pair.starts.get(), calls);
        match fixture.records.write(store::HUNKS, 0, [42; 8], &mut ReadWork::new(1, 1), &cancel) {
            Step::Pending => {}
            Step::Ready(()) => {
                solver.acknowledge(first).unwrap();
                assert!(matches!(solver.acknowledge(first), Err(InputError::QueryChanged)));
                while let Some(event) = next(&mut solver, &mut pair, &mut fixture.records, 1) {
                    solver.acknowledge(event).unwrap();
                }
                assert_eq!(pair.starts.get(), pair.completions);
                fixture.finish();
                return;
            }
            other => panic!("sink failed: {other:?}"),
        }
    }
    panic!("one-byte sink did not finish");
}

#[test]
fn cancellation_keeps_pending_equality_and_sink_owned_by_the_caller() {
    let mut fixture = Fixture::new();
    let cancel = AtomicBool::new(false);
    let mut pair = Pair::new(b"a", b"a");
    let mut solver = Sequence::new(pair.lengths(), Layout::default());
    assert!(matches!(solver.next(&mut pair, &mut fixture.records, &mut ReadWork::new(257, 2), &cancel), Step::Pending));
    assert_eq!(pair.starts.get(), 1);
    assert_eq!(pair.completions, 0);
    cancel.store(true, Ordering::Relaxed);
    assert!(matches!(solver.next(&mut pair, &mut fixture.records, &mut ReadWork::new(1, 1), &cancel), Step::Cancelled));
    assert_eq!(pair.starts.get(), 1);
    cancel.store(false, Ordering::Relaxed);
    let event = next(&mut solver, &mut pair, &mut fixture.records, 1).unwrap();
    assert_eq!(pair.starts.get(), 1);
    assert_eq!(pair.completions, 1);
    assert!(matches!(fixture.records.write(store::HUNKS, 0, [9; 8], &mut ReadWork::new(1, 1), &cancel), Step::Pending));
    cancel.store(true, Ordering::Relaxed);
    assert!(matches!(solver.next(&mut pair, &mut fixture.records, &mut ReadWork::new(1, 1), &cancel), Step::Cancelled));
    cancel.store(false, Ordering::Relaxed);
    assert!(matches!(solver.next(&mut pair, &mut fixture.records, &mut ReadWork::new(0, 0), &cancel),
        Step::Ready(Some(held)) if held == event));
    // Explicitly abandon this fixture's own sink transfer before acknowledging.
    fixture.records.abandon();
    solver.acknowledge(event).unwrap();
    assert!(next(&mut solver, &mut pair, &mut fixture.records, 1).is_none());
    fixture.finish();
}

#[test]
fn record_offsets_fail_checked_before_access() {
    let mut fixture = Fixture::new();
    let mut pair = Pair::new(b"a", b"b");
    let mut solver = Sequence::new(pair.lengths(), Layout { stack: u64::MAX, ..Layout::default() });
    assert!(matches!(
        solver.next(&mut pair, &mut fixture.records, &mut ReadWork::new(1024, 4), &AtomicBool::new(false)),
        Step::Failed(InputError::Overflow)
    ));
    assert_eq!(pair.starts.get(), 0);
    fixture.finish();
}
