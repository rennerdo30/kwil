//! Source-only fixtures: ROOT registers/runs these and supplies fail-first evidence.
use super::*;
use crate::compare_input::{
    AdmissionError, BuildStep, CaptureKind, Captured, InputSource, SourceLease, StoreAdmission, StorePaths, begin,
};
use std::cell::RefCell;
use std::fs;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone)]
struct Budget {
    used: Rc<RefCell<Usage>>,
    limit: usize,
}
impl Budget {
    fn new() -> Self {
        Self { used: Rc::new(RefCell::new(Usage::default())), limit: usize::MAX }
    }
}
impl Admission for Budget {
    fn reserve(&mut self, n: Usage) -> Result<(), AdmissionError> {
        let mut used = self.used.borrow_mut();
        let resident = used.resident.checked_add(n.resident).ok_or(AdmissionError)?;
        if resident > self.limit {
            return Err(AdmissionError);
        }
        *used = Usage {
            resident,
            disk: used.disk.checked_add(n.disk).ok_or(AdmissionError)?,
            handles: used.handles.checked_add(n.handles).ok_or(AdmissionError)?,
        };
        Ok(())
    }
    fn release(&mut self, n: Usage) {
        let mut used = self.used.borrow_mut();
        used.resident -= n.resident;
        used.disk -= n.disk;
        used.handles -= n.handles;
    }
}
struct Text {
    bytes: Vec<u8>,
    at: usize,
    identity: InputIdentity,
}
impl SourceLease for Text {
    fn identity(&self) -> InputIdentity {
        self.identity
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
        let n = (self.bytes.len() - self.at).min(logical.len());
        match work.debit(n as u64, 1, cancel) {
            Step::Ready(()) => {}
            Step::Pending => return Step::Pending,
            Step::Cancelled => return Step::Cancelled,
            Step::Failed(error) => return Step::Failed(error),
        }
        if n == 0 {
            return Step::Ready(None);
        }
        logical[..n].copy_from_slice(&self.bytes[self.at..self.at + n]);
        self.at += n;
        Step::Ready(Some(Captured { logical: n, original: 0, kind: CaptureKind::Text, encoding: 0 }))
    }
}
fn cancel() -> AtomicBool {
    AtomicBool::new(false)
}
fn work() -> ReadWork {
    ReadWork::new(16 * INPUT_WINDOW as u64, 4096)
}
fn unique() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "kwil-read-lease-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}
fn fixture(bytes: &[u8]) -> (InputStore<Budget>, Budget) {
    fixture_with_identity(bytes, InputIdentity([11, 22, 33, 44]))
}
fn fixture_with_identity(bytes: &[u8], identity: InputIdentity) -> (InputStore<Budget>, Budget) {
    fixture_source(Text { bytes: bytes.to_vec(), at: 0, identity })
}
fn fixture_source<S: SourceLease>(source: S) -> (InputStore<Budget>, Budget) {
    let directory = unique();
    let paths = StorePaths {
        logical: directory.join("logical"),
        original: directory.join("original"),
        lines: directory.join("lines"),
        atoms: directory.join("atoms"),
        directory,
    };
    let budget = Budget::new();
    let mut build = match begin(source, StoreAdmission { aggregate: budget.clone(), paths }) {
        ControlFlow::Continue(build) => build,
        ControlFlow::Break(_) => panic!("fixture admission"),
    };
    for _ in 0..64 {
        match build.step(work(), &cancel()) {
            BuildStep::Pending(next) => build = next,
            BuildStep::Ready { store, .. } => return (store, budget),
            _ => panic!("fixture capture"),
        }
    }
    panic!("bounded fixture did not finish")
}

fn decoded_fixture(raw: &[u8], encoding: crate::encoding::Encoding, bom: usize) -> (InputStore<Budget>, Budget) {
    use crate::compare_input::{DecodedNativeLease, NativeFileLease};
    let path = unique();
    fs::write(&path, raw).unwrap();
    let native = NativeFileLease::new(File::open(&path).unwrap(), InputIdentity([11, 22, 33, 44]));
    let budget = Budget::new();
    let source = match DecodedNativeLease::try_new(
        native,
        crate::decoded_backing::Interpretation::new(encoding, bom).unwrap(),
        budget.clone(),
    ) {
        ControlFlow::Continue(source) => source,
        ControlFlow::Break(_) => panic!("decoded fixture admission"),
    };
    // Capture and store use one aggregate, including the decoder's reservation.
    let directory = unique();
    let paths = StorePaths {
        logical: directory.join("logical"),
        original: directory.join("original"),
        lines: directory.join("lines"),
        atoms: directory.join("atoms"),
        directory,
    };
    let mut build = match begin(source, StoreAdmission { aggregate: budget.clone(), paths }) {
        ControlFlow::Continue(build) => build,
        ControlFlow::Break(_) => panic!("decoded store admission"),
    };
    for _ in 0..64 {
        match build.step(work(), &cancel()) {
            BuildStep::Pending(next) => build = next,
            BuildStep::Ready { store, source } => {
                let (native, _) = source.into_parts();
                drop(native.into_file());
                // Layout/lease work must not depend on this source pathname.
                fs::remove_file(path).unwrap();
                return (store, budget);
            }
            _ => panic!("decoded capture"),
        }
    }
    panic!("bounded decoded fixture did not finish")
}
fn lease(store: &InputStore<Budget>, budget: &Budget) -> ReadLease<Budget> {
    match store.read_lease(budget.clone(), &mut work(), &cancel()) {
        ReadLeaseStep::Ready(lease) => lease,
        _ => panic!("fixture lease"),
    }
}
fn ready<T>(step: Step<T>) -> T {
    match step {
        Step::Ready(value) => value,
        Step::Failed(error) => panic!("{error}"),
        _ => panic!("expected Ready"),
    }
}
fn close_lease(mut retirement: ReadLeaseRetirement<Budget>) -> Budget {
    for _ in 0..8 {
        match retirement.step(ReadWork::new(0, 1), &cancel()) {
            ReadLeaseRetirementStep::Pending(next) => retirement = next,
            ReadLeaseRetirementStep::Ready(budget) => return budget,
            _ => panic!("lease retirement"),
        }
    }
    panic!("bounded retirement did not finish")
}
fn close_store(store: InputStore<Budget>, budget: &Budget) {
    match store.into_retirement().step(work(), &cancel()) {
        super::super::RetirementStep::Ready(_) => assert_eq!(*budget.used.borrow(), Usage::default()),
        _ => panic!("store retirement"),
    }
}

#[test]
fn cached_identity_crlf_empty_and_final_empty_line_survive_independent_queries() {
    for bytes in [&b""[..], &b"A\r\nB\n"[..]] {
        let (store, budget) = fixture(bytes);
        let read = lease(&store, &budget);
        assert_eq!(read.identity(), InputIdentity([11, 22, 33, 44]));
        assert_eq!(read.byte_len(), bytes.len() as u64);
        assert_eq!(read.line_count(), if bytes.is_empty() { 1 } else { 3 });
        let final_line = read.line_count() - 1;
        let span = ready(read.line_span(&mut LineLookup::new(final_line), &mut work(), &cancel()));
        assert_eq!(
            span,
            LineSpan {
                start: bytes.len() as u64,
                content_end: bytes.len() as u64,
                end: bytes.len() as u64,
                terminator: super::super::Terminator::None,
            }
        );
        let rank = ready(read.rank(&mut RankLookup::new(bytes.len() as u64), &mut work(), &cancel()));
        assert_eq!(rank, Rank { line: final_line, column: 0 });
        if !bytes.is_empty() {
            let first = ready(read.line_span(&mut LineLookup::new(0), &mut work(), &cancel()));
            assert_eq!(
                first,
                LineSpan { start: 0, content_end: 1, end: 3, terminator: super::super::Terminator::CrLf }
            );
        }
        close_lease(read.into_retirement());
        close_store(store, &budget);
    }
}

#[test]
fn insufficient_or_cancelled_work_never_visits_payload_and_replay_is_counted() {
    let (store, budget) = fixture(b"one\ntwo\n");
    let read = lease(&store, &budget);
    let before = read.visits();
    let mut out = [0x55; 4];
    assert!(matches!(
        read.read_at(PayloadSpace::Logical, 0, &mut out, &mut ReadWork::new(7, 1), &cancel()),
        Step::Pending
    ));
    assert!(matches!(
        read.read_at(PayloadSpace::Logical, 0, &mut out, &mut work(), &AtomicBool::new(true)),
        Step::Cancelled
    ));
    assert_eq!(out, [0x55; 4]);
    assert_eq!(read.visits(), before);
    for _ in 0..2 {
        assert_eq!(ready(read.read_at(PayloadSpace::Logical, 0, &mut out, &mut work(), &cancel())), 4);
        assert_eq!(&out, b"one\n");
        assert_eq!(ready(read.rank(&mut RankLookup::new(5), &mut work(), &cancel())), Rank { line: 1, column: 1 });
    }
    let visits = read.visits();
    assert_eq!(visits.calls[0], 2);
    assert_eq!(visits.requested_bytes[0], 8);
    assert!(visits.calls[2] >= 4);
    assert_eq!(visits.requested_bytes[2], visits.calls[2] * LINE_RECORD as u64);
    assert_eq!(visits.decoded_bytes, visits.requested_bytes[2]);
    assert_eq!(visits.charged_bytes, 2 * (8 + visits.requested_bytes[2]));
    close_lease(read.into_retirement());
    close_store(store, &budget);
}

#[test]
fn giant_line_reads_are_bounded_and_use_disk_rank_not_line_materialization() {
    let bytes = vec![b'x'; INPUT_WINDOW + 17];
    let (store, budget) = fixture(&bytes);
    let read = lease(&store, &budget);
    let mut out = [0; 17];
    assert_eq!(ready(read.read_at(PayloadSpace::Logical, INPUT_WINDOW as u64, &mut out, &mut work(), &cancel())), 17);
    assert_eq!(out, [b'x'; 17]);
    assert_eq!(
        ready(read.rank(&mut RankLookup::new(bytes.len() as u64), &mut work(), &cancel())),
        Rank { line: 0, column: bytes.len() as u64 }
    );
    assert!(read.visits().requested_bytes.iter().sum::<u64>() < INPUT_WINDOW as u64);
    close_lease(read.into_retirement());
    close_store(store, &budget);
}

#[test]
fn pread_queries_resume_records_without_replaying_unpaid_bytes() {
    let (store, budget) = fixture(b"a\nb\nc\n");
    let read = lease(&store, &budget);
    let mut lookup = RankLookup::new(3);
    let mut answer = None;
    for _ in 0..256 {
        match read.rank(&mut lookup, &mut ReadWork::new(1, 1), &cancel()) {
            Step::Pending => {}
            Step::Ready(rank) => {
                answer = Some(rank);
                break;
            }
            _ => panic!("rank resume"),
        }
    }
    assert_eq!(answer, Some(Rank { line: 1, column: 1 }));
    let visits = read.visits();
    assert_eq!(visits.returned_bytes[2], visits.decoded_bytes);
    assert_eq!(visits.charged_bytes, 2 * visits.requested_bytes[2]);
    close_lease(read.into_retirement());
    close_store(store, &budget);
}

#[test]
fn admission_refusal_and_partial_clone_failure_preserve_exact_owners_and_error() {
    let (store, budget) = fixture(b"x");
    let baseline = *budget.used.borrow();
    let mut denied = budget.clone();
    denied.limit = baseline.resident;
    match store.read_lease(denied, &mut work(), &cancel()) {
        ReadLeaseStep::Rejected { admission, error: InputError::Admission(_) } => {
            assert!(Rc::ptr_eq(&admission.used, &budget.used));
        }
        _ => panic!("expected exact refusal"),
    }
    assert_eq!(*budget.used.borrow(), baseline);
    let mut cloned = 0;
    let failed = store.read_lease_with(budget.clone(), &mut work(), &cancel(), |file, index| {
        assert_eq!(index, cloned);
        assert_eq!(budget.used.borrow().handles, baseline.handles + READ_LEASE_HANDLES);
        if index == 2 {
            return Err(io::Error::from_raw_os_error(1234));
        }
        cloned += 1;
        file.try_clone()
    });
    let retirement = match failed {
        ReadLeaseStep::Failed { retirement, error: InputError::Io(error) } => {
            assert_eq!(error.raw_os_error(), Some(1234));
            assert_eq!(retirement.open_handles(), 2);
            retirement
        }
        _ => panic!("expected partial owner"),
    };
    assert_eq!(cloned, 2);
    close_lease(retirement);
    assert_eq!(*budget.used.borrow(), baseline);
    close_store(store, &budget);
}

#[test]
fn pending_creation_and_retirement_keep_all_reservations_until_explicit_completion() {
    let (store, budget) = fixture(b"x");
    let baseline = *budget.used.borrow();
    let returned = match store.read_lease(budget.clone(), &mut ReadWork::new(0, 3), &cancel()) {
        ReadLeaseStep::Pending(admission) => admission,
        _ => panic!("creation must prepay all clone work"),
    };
    assert!(Rc::ptr_eq(&returned.used, &budget.used));
    assert_eq!(*budget.used.borrow(), baseline);
    let read = lease(&store, &budget);
    let admitted = *budget.used.borrow();
    let retirement = match read.into_retirement().step(ReadWork::new(0, 0), &cancel()) {
        ReadLeaseRetirementStep::Pending(retirement) => retirement,
        _ => panic!("retirement must wait"),
    };
    assert_eq!(retirement.open_handles(), 4);
    assert_eq!(*budget.used.borrow(), admitted);
    let returned = close_lease(retirement);
    assert!(Rc::ptr_eq(&returned.used, &budget.used));
    assert_eq!(*budget.used.borrow(), baseline);
    close_store(store, &budget);
}

#[test]
fn lease_never_reopens_original_or_store_paths() {
    let (store, budget) = fixture(b"sealed");
    let read = lease(&store, &budget);
    // On Unix rename proves path independence; Windows may refuse renaming an
    // open directory. This fixture never needs to unlink an open store.
    let directory = store.resources.paths.directory.clone();
    let moved = unique();
    let renamed = fs::rename(&directory, &moved).is_ok();
    let mut out = [0; 6];
    assert_eq!(ready(read.read_at(PayloadSpace::Logical, 0, &mut out, &mut work(), &cancel())), 6);
    assert_eq!(&out, b"sealed");
    if renamed {
        fs::rename(&moved, &directory).unwrap();
    }
    close_lease(read.into_retirement());
    close_store(store, &budget);
}

#[test]
fn utf16_logical_capture_preserves_crlf_offsets_and_raw_atom_endpoints() {
    let (store, budget) = decoded_fixture(b"\xff\xfeA\0\r\0\n\0B\0", crate::encoding::Encoding::UTF16_LE, 2);
    let read = lease(&store, &budget);
    let mut out = [0; 4];
    assert_eq!(ready(read.read_at(PayloadSpace::Logical, 0, &mut out, &mut work(), &cancel())), 4);
    assert_eq!(&out, b"A\r\nB");
    assert_eq!(
        ready(read.line_span(&mut LineLookup::new(0), &mut work(), &cancel())),
        LineSpan { start: 0, content_end: 1, end: 3, terminator: super::super::Terminator::CrLf }
    );
    assert_eq!(ready(read.rank(&mut RankLookup::new(3), &mut work(), &cancel())), Rank { line: 1, column: 0 });
    let atom = ready(read.atom_span(&mut AtomLookup::new(0), &mut work(), &cancel())).unwrap();
    assert_eq!(atom.kind, CaptureKind::Preserved);
    assert_eq!((atom.start, atom.end, atom.original_end - atom.original_start), (0, 1, 2));
    let mut original = [0; 2];
    assert_eq!(
        ready(read.read_at(PayloadSpace::Original, atom.original_start, &mut original, &mut work(), &cancel())),
        2
    );
    assert_eq!(original, [b'A', 0]);
    close_lease(read.into_retirement());
    close_store(store, &budget);
}

#[test]
fn opaque_sentinels_keep_distinct_original_bytes_and_preserved_alias_stays_whole() {
    let (store, budget) = decoded_fixture(b"\x81\x82", crate::encoding::Encoding::UTF8, 0);
    let read = lease(&store, &budget);
    let mut logical = [0; 2];
    ready(read.read_at(PayloadSpace::Logical, 0, &mut logical, &mut work(), &cancel()));
    assert_eq!(logical, [0xff, 0xff]);
    let mut original_bytes = [0; 2];
    for at in 0..2 {
        let atom = ready(read.atom_span(&mut AtomLookup::new(at), &mut work(), &cancel())).unwrap();
        assert_eq!(atom.kind, CaptureKind::Opaque);
        assert_ne!(atom.encoding, 0);
        assert_eq!((atom.start, atom.end), (at, at + 1));
        let mut original = [0];
        ready(read.read_at(PayloadSpace::Original, atom.original_start, &mut original, &mut work(), &cancel()));
        original_bytes[at as usize] = original[0];
    }
    assert_eq!(original_bytes, [0x81, 0x82]);
    assert!(read.visits().calls[3] >= 2);
    assert_eq!(read.visits().requested_bytes[1], 2);
    close_lease(read.into_retirement());
    close_store(store, &budget);

    let (store, budget) = decoded_fixture(b"\x88\x62", crate::encoding::Encoding::for_label("big5").unwrap(), 0);
    let read = lease(&store, &budget);
    let atom = ready(read.atom_span(&mut AtomLookup::new(3), &mut work(), &cancel())).unwrap();
    assert_eq!(atom.kind, CaptureKind::Preserved);
    assert_eq!((atom.start, atom.end, atom.original_start, atom.original_end), (0, 4, 0, 2));
    close_lease(read.into_retirement());
    close_store(store, &budget);
}

#[test]
fn no_accidental_drop_closes_handles_or_releases_the_lease_charge() {
    let (store, budget) = fixture(b"x");
    let read = lease(&store, &budget);
    let usage = read.usage();
    let before = *budget.used.borrow();
    // A compile-derived predicate checks the COMPLETE field destruction path,
    // including Budget's Rc, without actually leaking handles in the fixture.
    assert!(!std::mem::needs_drop::<ReadLease<Budget>>());
    assert!(!std::mem::needs_drop::<ReadLeaseRetirement<Budget>>());
    assert!(!std::mem::needs_drop::<LeaseResources<Budget>>());
    assert_eq!(*budget.used.borrow(), before);
    assert_eq!(read.usage(), usage);
    for file in read.resources.files.iter().flatten() {
        assert!(file.metadata().is_ok());
    }
    close_lease(read.into_retirement());
    close_store(store, &budget);
}

#[test]
fn record_envelope_covers_worst_short_read_revisits_and_prepaid_decode() {
    let (store, budget) = fixture(b"x");
    let read = lease(&store, &budget);
    read.visits.read_limit.store(1, Ordering::Relaxed);
    let mut paid = read.line_work_bound();
    let bound = paid;
    let span = ready(read.line_span(&mut LineLookup::new(0), &mut paid, &cancel()));
    assert_eq!(span.start, 0);
    let visits = read.visits();
    assert_eq!(visits.calls[2], LINE_RECORD as u64);
    assert_eq!(visits.requested_bytes[2], LINE_RECORD as u64);
    assert_eq!(visits.decoded_bytes, LINE_RECORD as u64);
    assert_eq!(visits.charged_bytes, LINE_RECORD as u64 * (LINE_RECORD as u64 + 1));
    assert_eq!(paid, ReadWork::default());
    assert_eq!(bound.operations, visits.calls[2]);
    close_lease(read.into_retirement());
    close_store(store, &budget);
}

#[test]
fn unpaid_or_cancelled_rank_and_provenance_do_not_visit_disk_records() {
    let (store, budget) = decoded_fixture(b"\x81", crate::encoding::Encoding::UTF8, 0);
    let read = lease(&store, &budget);
    let before = read.visits();
    assert!(matches!(read.rank(&mut RankLookup::new(0), &mut ReadWork::new(4096, 0), &cancel()), Step::Pending));
    assert!(matches!(read.atom_span(&mut AtomLookup::new(0), &mut ReadWork::new(4096, 0), &cancel()), Step::Pending));
    assert!(matches!(read.rank(&mut RankLookup::new(0), &mut work(), &AtomicBool::new(true)), Step::Cancelled));
    assert!(matches!(read.atom_span(&mut AtomLookup::new(0), &mut work(), &AtomicBool::new(true)), Step::Cancelled));
    assert_eq!(read.visits(), before);
    close_lease(read.into_retirement());
    close_store(store, &budget);
}

#[test]
fn cancellation_between_clones_returns_partial_owner_without_closing_or_releasing() {
    let (store, budget) = fixture(b"x");
    let baseline = *budget.used.borrow();
    let cancelled = AtomicBool::new(false);
    let outcome = store.read_lease_with(budget.clone(), &mut work(), &cancelled, |file, index| {
        let file = file.try_clone()?;
        if index == 1 {
            cancelled.store(true, Ordering::Relaxed);
        }
        Ok(file)
    });
    let retirement = match outcome {
        ReadLeaseStep::Interrupted(retirement) => retirement,
        _ => panic!("expected explicit partial cancellation owner"),
    };
    assert_eq!(retirement.open_handles(), 2);
    assert_eq!(budget.used.borrow().handles, baseline.handles + 4);
    let retirement = match retirement.step(work(), &cancelled) {
        ReadLeaseRetirementStep::Cancelled(retirement) => retirement,
        _ => panic!("cancelled retirement must retain handles"),
    };
    assert_eq!(retirement.open_handles(), 2);
    close_lease(retirement);
    assert_eq!(*budget.used.borrow(), baseline);
    close_store(store, &budget);
}

#[test]
fn literal_cr_at_eof_is_content_and_not_a_reconstructed_terminator() {
    let (store, budget) = fixture(b"last\r");
    let read = lease(&store, &budget);
    assert_eq!(read.line_count(), 1);
    assert_eq!(
        ready(read.line_span(&mut LineLookup::new(0), &mut work(), &cancel())),
        LineSpan { start: 0, content_end: 5, end: 5, terminator: super::super::Terminator::None }
    );
    assert_eq!(ready(read.rank(&mut RankLookup::new(5), &mut work(), &cancel())), Rank { line: 0, column: 5 });
    let mut out = [0; 5];
    ready(read.read_at(PayloadSpace::Logical, 0, &mut out, &mut work(), &cancel()));
    assert_eq!(&out, b"last\r");
    close_lease(read.into_retirement());
    close_store(store, &budget);
}

#[test]
fn read_lease_rejects_foreign_pending_atom_lookup_even_at_eof_without_io() {
    let id_a = InputIdentity([1, 2, 3, 4]);
    let id_b = InputIdentity([5, 6, 7, 8]);
    let (store_a, budget_a) = fixture_with_identity(b"ab", id_a);
    let (store_b, budget_b) = fixture_with_identity(b"a", id_b);
    let read_a = lease(&store_a, &budget_a);
    let read_b = lease(&store_b, &budget_b);
    let before_a = read_a.visits();
    let before_b = read_b.visits();
    let mut unpaid = ReadWork::default();
    let mut lookup = AtomLookup::new(1);
    assert!(matches!(read_a.atom_span(&mut lookup, &mut unpaid, &cancel()), Step::Pending));
    assert_eq!(lookup.identity, Some(id_a));
    assert!(matches!(read_b.atom_span(&mut lookup, &mut unpaid, &cancel()), Step::Failed(InputError::QueryChanged)));
    assert_eq!(lookup.identity, Some(id_a));
    assert_eq!(lookup.record.done, 0);
    assert_eq!(unpaid, ReadWork::default());
    assert_eq!(read_a.visits(), before_a);
    assert_eq!(read_b.visits(), before_b);

    let mut eof = AtomLookup::new(1);
    assert!(matches!(read_b.atom_span(&mut eof, &mut unpaid, &cancel()), Step::Ready(None)));
    assert_eq!(eof.identity, Some(id_b));
    assert!(matches!(read_a.atom_span(&mut eof, &mut unpaid, &cancel()), Step::Failed(InputError::QueryChanged)));
    assert_eq!(read_a.visits(), before_a);
    assert_eq!(read_b.visits(), before_b);
    close_lease(read_a.into_retirement());
    close_lease(read_b.into_retirement());
    close_store(store_a, &budget_a);
    close_store(store_b, &budget_b);
}

#[test]
fn input_store_rejects_foreign_pending_atom_lookup_even_at_eof_without_io() {
    let id_a = InputIdentity([1, 2, 3, 4]);
    let id_b = InputIdentity([5, 6, 7, 8]);
    let (store_a, budget_a) = fixture_with_identity(b"ab", id_a);
    let (store_b, budget_b) = fixture_with_identity(b"a", id_b);
    let mut unpaid = ReadWork::default();
    let mut lookup = AtomLookup::new(1);
    assert!(matches!(store_a.atom_span(&mut lookup, &mut unpaid, &cancel()), Step::Pending));
    assert_eq!(lookup.identity, Some(id_a));
    assert!(matches!(store_b.atom_span(&mut lookup, &mut unpaid, &cancel()), Step::Failed(InputError::QueryChanged)));
    assert_eq!(lookup.identity, Some(id_a));
    assert_eq!(lookup.record.done, 0);
    assert_eq!(unpaid, ReadWork::default());

    let mut eof = AtomLookup::new(1);
    assert!(matches!(store_b.atom_span(&mut eof, &mut unpaid, &cancel()), Step::Ready(None)));
    assert_eq!(eof.identity, Some(id_b));
    assert!(matches!(store_a.atom_span(&mut eof, &mut unpaid, &cancel()), Step::Failed(InputError::QueryChanged)));
    assert_eq!(unpaid, ReadWork::default());
    close_store(store_a, &budget_a);
    close_store(store_b, &budget_b);
}
