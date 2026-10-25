//! Source-only regression fixtures. ROOT owns execution and fail-first evidence.
use super::*;
use std::cell::RefCell;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Clone)]
struct Budget {
    usage: Rc<RefCell<Usage>>,
    resident: usize,
    disk: u64,
    handles: usize,
}
impl Budget {
    fn new() -> Self {
        Self {
            usage: Rc::new(RefCell::new(Usage::default())),
            resident: usize::MAX,
            disk: u64::MAX,
            handles: usize::MAX,
        }
    }
}
impl Admission for Budget {
    fn reserve(&mut self, additional: Usage) -> Result<(), AdmissionError> {
        let mut usage = self.usage.borrow_mut();
        let resident = usage.resident.checked_add(additional.resident).ok_or(AdmissionError)?;
        let disk = usage.disk.checked_add(additional.disk).ok_or(AdmissionError)?;
        let handles = usage.handles.checked_add(additional.handles).ok_or(AdmissionError)?;
        if resident > self.resident || disk > self.disk || handles > self.handles {
            return Err(AdmissionError);
        }
        *usage = Usage { resident, disk, handles };
        Ok(())
    }
    fn release(&mut self, released: Usage) {
        let mut usage = self.usage.borrow_mut();
        usage.resident -= released.resident;
        usage.disk -= released.disk;
        usage.handles -= released.handles;
    }
}

fn unique(label: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "kwil-compare-input-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}
fn paths() -> StorePaths {
    let directory = unique("store");
    StorePaths {
        logical: directory.join("logical"),
        original: directory.join("original"),
        lines: directory.join("lines"),
        atoms: directory.join("atoms"),
        directory,
    }
}
fn admission() -> StoreAdmission<Budget> {
    StoreAdmission { aggregate: Budget::new(), paths: paths() }
}
fn cancel() -> AtomicBool {
    AtomicBool::new(false)
}
fn work() -> ReadWork {
    ReadWork::new(8 * INPUT_WINDOW as u64, 128)
}
fn ready<T>(step: Step<T>) -> T {
    match step {
        Step::Ready(value) => value,
        Step::Failed(error) => panic!("{error}"),
        Step::Pending => panic!("unexpected Pending"),
        Step::Cancelled => panic!("unexpected Cancelled"),
    }
}

struct Text {
    bytes: &'static [u8],
    at: usize,
    chunk: usize,
    loan: Arc<()>,
    validations: usize,
    change_on_seal: bool,
    error: Option<io::Error>,
}
impl Text {
    fn new(bytes: &'static [u8], chunk: usize) -> Self {
        Self { bytes, at: 0, chunk, loan: Arc::new(()), validations: 0, change_on_seal: false, error: None }
    }
}
impl SourceLease for Text {
    fn identity(&self) -> InputIdentity {
        InputIdentity([1, 2, 3, 4])
    }
    fn validate(&mut self, work: &mut ReadWork, cancel: &AtomicBool) -> Step<()> {
        match work.debit(0, 1, cancel) {
            Step::Ready(()) => {}
            other => return other,
        }
        self.validations += 1;
        if self.change_on_seal && self.validations == 2 {
            Step::Failed(InputError::SourceChanged)
        } else {
            Step::Ready(())
        }
    }
    fn capture(
        &mut self,
        logical: &mut [u8],
        _: &mut [u8],
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<Option<Captured>> {
        if let Some(error) = self.error.take() {
            return Step::Failed(error.into());
        }
        let n = (self.bytes.len() - self.at).min(self.chunk).min(logical.len()).min(work.bytes as usize);
        if self.at != self.bytes.len() && n == 0 {
            return Step::Pending;
        }
        match work.debit(n as u64, 0, cancel) {
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

fn build<S: SourceLease>(source: S) -> (InputStore<Budget>, S) {
    build_with(source, admission(), work())
}
fn build_with<S: SourceLease>(
    source: S,
    admission: StoreAdmission<Budget>,
    slice: ReadWork,
) -> (InputStore<Budget>, S) {
    let mut build = match begin(source, admission) {
        ControlFlow::Continue(build) => build,
        ControlFlow::Break(error) => panic!("{}", error.error),
    };
    // Bounded fixture driver, never an unbounded busy loop. Production workers
    // yield Pending back to their scheduler instead of polling this way.
    for _ in 0..200_000 {
        match build.step(slice, &cancel()) {
            BuildStep::Pending(next) => build = next,
            BuildStep::Ready { store, source } => return (store, source),
            BuildStep::Failed { error, retirement, .. } => {
                retire(retirement);
                panic!("{error}");
            }
            BuildStep::Cancelled { retirement, .. } => {
                retire(retirement);
                panic!("cancelled fixture");
            }
        }
    }
    panic!("fixture made no bounded progress");
}
fn retire(mut retirement: Retirement<Budget>) -> StoreAdmission<Budget> {
    for _ in 0..32 {
        match retirement.step(work(), &cancel()) {
            RetirementStep::Ready(admission) => {
                assert_eq!(*admission.aggregate.usage.borrow(), Usage::default());
                return admission;
            }
            RetirementStep::Pending(next) | RetirementStep::Cancelled(next) => retirement = next,
            RetirementStep::Failed { error, .. } => panic!("{error}"),
        }
    }
    panic!("retirement failed to progress");
}
fn line(store: &InputStore<Budget>, index: u64) -> LineSpan {
    ready(store.line_span(&mut LineLookup::new(index), &mut work(), &cancel()))
}
fn payload(store: &mut InputStore<Budget>, space: PayloadSpace, at: u64, out: &mut [u8]) -> usize {
    ready(store.read_at(space, at, out, &mut work(), &cancel()))
}

#[test]
fn short_chunks_preserve_crlf_full_span_literal_cr_and_final_empty_line() {
    let (mut store, source) = build(Text::new(b"a\r\nb\rX\n\nlast\r\n", 1));
    assert_eq!(source.validations, 2);
    assert_eq!(store.byte_len(), 14);
    assert_eq!(store.line_count(), 5);
    assert_eq!(line(&store, 0), LineSpan { start: 0, content_end: 1, end: 3, terminator: Terminator::CrLf });
    assert_eq!(line(&store, 1), LineSpan { start: 3, content_end: 6, end: 7, terminator: Terminator::Lf });
    assert_eq!(line(&store, 2), LineSpan { start: 7, content_end: 7, end: 8, terminator: Terminator::Lf });
    assert_eq!(line(&store, 3), LineSpan { start: 8, content_end: 12, end: 14, terminator: Terminator::CrLf });
    assert_eq!(line(&store, 4), LineSpan { start: 14, content_end: 14, end: 14, terminator: Terminator::None });
    let mut out = [0; 32];
    let n = payload(&mut store, PayloadSpace::Logical, 0, &mut out);
    assert_eq!(&out[..n], b"a\r\nb\rX\n\nlast\r\n");
    for at in 0..=store.byte_len() {
        let rank = ready(store.rank(&mut RankLookup::new(at), &mut work(), &cancel()));
        let expected = [0, 3, 7, 8, 14].iter().rposition(|start| *start <= at).unwrap();
        assert_eq!(rank, Rank { line: expected as u64, column: at - [0, 3, 7, 8, 14][expected] });
    }
    retire(store.into_retirement());
}

#[test]
fn empty_and_final_literal_cr_are_real_lines() {
    for text in [b"".as_slice(), b"\r".as_slice(), b"a\n".as_slice()] {
        let (store, _) = build(Text::new(text, 1));
        assert_eq!(line(&store, 0).start, 0);
        if text == b"\r" {
            assert_eq!(line(&store, 0).content_end, 1);
        }
        if text == b"a\n" {
            assert_eq!(store.line_count(), 2);
            assert_eq!(line(&store, 1).start, 2);
        }
        retire(store.into_retirement());
    }
}

/// Generate a giant line without a whole-file test buffer.
struct Repeated {
    remaining: u64,
    at: u64,
}
impl SourceLease for Repeated {
    fn identity(&self) -> InputIdentity {
        InputIdentity([9, 0, 0, 0])
    }
    fn validate(&mut self, work: &mut ReadWork, cancel: &AtomicBool) -> Step<()> {
        work.debit(0, 0, cancel)
    }
    fn capture(
        &mut self,
        logical: &mut [u8],
        _: &mut [u8],
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<Option<Captured>> {
        let n = self.remaining.min(logical.len() as u64).min(work.bytes) as usize;
        if self.remaining != 0 && n == 0 {
            return Step::Pending;
        }
        match work.debit(n as u64, 0, cancel) {
            Step::Ready(()) => {}
            Step::Pending => return Step::Pending,
            Step::Cancelled => return Step::Cancelled,
            Step::Failed(error) => return Step::Failed(error),
        }
        if n == 0 {
            return Step::Ready(None);
        }
        logical[..n].fill(b'x');
        self.remaining -= n as u64;
        self.at += n as u64;
        Step::Ready(Some(Captured { logical: n, original: 0, kind: CaptureKind::Text, encoding: 0 }))
    }
}

#[test]
fn giant_line_has_fixed_resident_windows_and_disk_rank() {
    let a = admission();
    let expected = resident_charge::<Repeated, Budget>(&a.paths).unwrap();
    let len = 3 * INPUT_WINDOW as u64 + 13;
    let (mut store, _) = build_with(Repeated { remaining: len, at: 0 }, a, work());
    assert_eq!(store.usage().resident, expected);
    assert_eq!(store.line_count(), 1);
    assert_eq!(line(&store, 0).content_end, len);
    let rank = ready(store.rank(&mut RankLookup::new(len), &mut work(), &cancel()));
    assert_eq!(rank, Rank { line: 0, column: len });
    let mut out = [0; 13];
    assert_eq!(payload(&mut store, PayloadSpace::Logical, len - 13, &mut out), 13);
    assert_eq!(&out, b"xxxxxxxxxxxxx");
    retire(store.into_retirement());
}

#[derive(Clone, Copy)]
struct Part {
    logical: &'static [u8],
    original: &'static [u8],
    kind: CaptureKind,
    encoding: u64,
}
struct Parts {
    parts: &'static [Part],
    next: usize,
    paid: usize,
}
impl SourceLease for Parts {
    fn identity(&self) -> InputIdentity {
        InputIdentity([11, 0, 0, 0])
    }
    fn validate(&mut self, work: &mut ReadWork, cancel: &AtomicBool) -> Step<()> {
        work.debit(0, 0, cancel)
    }
    fn capture(
        &mut self,
        logical: &mut [u8],
        raw: &mut [u8],
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<Option<Captured>> {
        if self.next == self.parts.len() {
            return Step::Ready(None);
        }
        let part = self.parts[self.next];
        let total = part.logical.len() + part.original.len();
        let n = (total - self.paid).min(work.bytes as usize);
        match work.debit(n as u64, 0, cancel) {
            Step::Ready(()) => {}
            Step::Pending => return Step::Pending,
            Step::Cancelled => return Step::Cancelled,
            Step::Failed(error) => return Step::Failed(error),
        }
        self.paid += n;
        if self.paid != total {
            return Step::Pending;
        }
        logical[..part.logical.len()].copy_from_slice(part.logical);
        raw[..part.original.len()].copy_from_slice(part.original);
        self.next += 1;
        self.paid = 0;
        Step::Ready(Some(Captured {
            logical: part.logical.len(),
            original: part.original.len(),
            kind: part.kind,
            encoding: part.encoding,
        }))
    }
}

#[test]
fn fragmented_opaque_and_alias_provenance_never_uses_sentinel_equality() {
    static PARTS: [Part; 5] = [
        Part { logical: b"A", original: b"", kind: CaptureKind::Text, encoding: 0 },
        Part { logical: b"\xff", original: b"\x81", kind: CaptureKind::Opaque, encoding: 17 },
        Part { logical: b"\xff", original: b"\x82", kind: CaptureKind::Opaque, encoding: 17 },
        Part { logical: "Ê\u{304}".as_bytes(), original: b"\x88\x62", kind: CaptureKind::Preserved, encoding: 23 },
        Part { logical: b"\r\n", original: b"", kind: CaptureKind::Text, encoding: 0 },
    ];
    let (mut store, _) = build_with(Parts { parts: &PARTS, next: 0, paid: 0 }, admission(), ReadWork::new(1, 1));
    let first = ready(store.atom_span(&mut AtomLookup::new(1), &mut work(), &cancel())).unwrap();
    let second = ready(store.atom_span(&mut AtomLookup::new(2), &mut work(), &cancel())).unwrap();
    assert_eq!(first.kind, CaptureKind::Opaque);
    assert_eq!(second.kind, CaptureKind::Opaque);
    let mut raw = [0; 1];
    payload(&mut store, PayloadSpace::Original, first.original_start, &mut raw);
    assert_eq!(raw, [0x81]);
    payload(&mut store, PayloadSpace::Original, second.original_start, &mut raw);
    assert_eq!(raw, [0x82]);
    let alias = ready(store.atom_span(&mut AtomLookup::new(4), &mut work(), &cancel())).unwrap();
    assert_eq!((alias.start, alias.end, alias.original_start, alias.original_end), (3, 7, 2, 4));
    assert_eq!(alias.kind, CaptureKind::Preserved);
    assert_eq!(line(&store, 0).terminator, Terminator::CrLf);
    assert!(ready(store.atom_span(&mut AtomLookup::new(store.byte_len()), &mut work(), &cancel())).is_none());
    retire(store.into_retirement());
}

#[test]
fn pending_output_is_untouched_and_one_byte_budgets_resume_all_lookups() {
    let (mut store, _) = build_with(Text::new(b"aa\nbb\n", 1), admission(), ReadWork::new(1, 1));
    let mut out = [0xa5; 6];
    assert!(matches!(
        store.read_at(PayloadSpace::Logical, 0, &mut out, &mut ReadWork::default(), &cancel()),
        Step::Pending
    ));
    assert_eq!(out, [0xa5; 6]);
    for _ in 0..32 {
        match store.read_at(PayloadSpace::Logical, 0, &mut out, &mut ReadWork::new(1, 1), &cancel()) {
            Step::Pending => assert_eq!(out, [0xa5; 6]),
            Step::Ready(6) => break,
            _ => panic!("bad read step"),
        }
    }
    assert_eq!(&out, b"aa\nbb\n");
    let mut line = LineLookup::new(1);
    let mut rank = RankLookup::new(5);
    let mut atom = AtomLookup::new(3);
    let mut line_result = None;
    let mut rank_result = None;
    let mut atom_result = None;
    for _ in 0..1000 {
        if line_result.is_none() {
            match store.line_span(&mut line, &mut ReadWork::new(1, 1), &cancel()) {
                Step::Ready(value) => line_result = Some(value),
                Step::Pending => {}
                _ => panic!("line query failed"),
            }
        }
        if rank_result.is_none() {
            match store.rank(&mut rank, &mut ReadWork::new(1, 1), &cancel()) {
                Step::Ready(value) => rank_result = Some(value),
                Step::Pending => {}
                _ => panic!("rank query failed"),
            }
        }
        if atom_result.is_none() {
            match store.atom_span(&mut atom, &mut ReadWork::new(1, 1), &cancel()) {
                Step::Ready(value) => atom_result = value,
                Step::Pending => {}
                _ => panic!("atom query failed"),
            }
        }
        if line_result.is_some() && rank_result.is_some() && atom_result.is_some() {
            break;
        }
    }
    assert_eq!(line_result.unwrap().start, 3);
    assert_eq!(rank_result.unwrap(), Rank { line: 1, column: 2 });
    assert_eq!(atom_result.unwrap().start, 3);
    retire(store.into_retirement());
}

#[test]
fn admission_refusal_returns_exact_loan_and_paths_without_io() {
    let source = Text::new(b"abc", 1);
    let loan = source.loan.clone();
    let mut a = admission();
    a.aggregate.resident = 0;
    let directory = a.paths.directory.clone();
    let aggregate = a.aggregate.usage.clone();
    match begin(source, a) {
        ControlFlow::Break(rejected) => {
            assert!(Arc::ptr_eq(&loan, &rejected.source.loan));
            assert_eq!(rejected.source.at, 0);
            assert_eq!(rejected.admission.paths.directory, directory);
            assert!(Rc::ptr_eq(&aggregate, &rejected.admission.aggregate.usage));
            assert!(matches!(rejected.error, InputError::Admission(_)));
        }
        ControlFlow::Continue(_) => panic!("admission did not refuse"),
    }
    assert!(!directory.exists());
    assert_eq!(*aggregate.borrow(), Usage::default());
}

#[test]
fn cancellation_returns_exact_source_and_retains_private_cleanup_owner() {
    let source = Text::new(b"abc", 1);
    let loan = source.loan.clone();
    let a = admission();
    let aggregate = a.aggregate.usage.clone();
    let directory = a.paths.directory.clone();
    let mut build = match begin(source, a) {
        ControlFlow::Continue(build) => build,
        ControlFlow::Break(_) => panic!("begin"),
    };
    build = match build.step(ReadWork::new(0, 5), &cancel()) {
        BuildStep::Pending(build) => build,
        _ => panic!("expected pause after private creates"),
    };
    assert!(directory.exists());
    let retirement = match build.step(ReadWork::default(), &AtomicBool::new(true)) {
        BuildStep::Cancelled { source, retirement } => {
            assert!(Arc::ptr_eq(&loan, &source.loan));
            assert_eq!(source.at, 0);
            retirement
        }
        _ => panic!("expected cancelled build"),
    };
    assert!(aggregate.borrow().resident > 0);
    assert!(directory.exists());
    retire(retirement);
    assert!(!directory.exists());
}

#[derive(Debug)]
struct Marker(Arc<()>);
impl std::fmt::Display for Marker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("source marker")
    }
}
impl std::error::Error for Marker {}

#[test]
fn source_changed_and_io_payload_survive_failure_with_the_loan() {
    for changed in [false, true] {
        let marker = Arc::new(());
        let mut source = Text::new(b"a\n", 1);
        source.change_on_seal = changed;
        if !changed {
            source.error = Some(io::Error::new(io::ErrorKind::PermissionDenied, Marker(marker.clone())));
        }
        let loan = source.loan.clone();
        let mut build = match begin(source, admission()) {
            ControlFlow::Continue(build) => build,
            ControlFlow::Break(_) => panic!("begin"),
        };
        let mut failed = false;
        for _ in 0..32 {
            match build.step(work(), &cancel()) {
                BuildStep::Pending(next) => build = next,
                BuildStep::Failed { source, error, retirement } => {
                    assert!(Arc::ptr_eq(&loan, &source.loan));
                    match error {
                        InputError::SourceChanged if changed => {}
                        InputError::Io(error) if !changed => {
                            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
                            assert!(Arc::ptr_eq(
                                &error.get_ref().unwrap().downcast_ref::<Marker>().unwrap().0,
                                &marker
                            ));
                        }
                        _ => panic!("wrong error type"),
                    }
                    retire(retirement);
                    failed = true;
                    break;
                }
                _ => panic!("expected failure"),
            }
        }
        assert!(failed, "source failure must terminate within the fixture budget");
    }
}

#[test]
fn short_disk_read_and_query_error_leave_output_and_store_available() {
    let (mut store, _) = build(Text::new(b"abcdef", 6));
    // Deliberately broken private copy: force a positive short read, then EOF.
    store.resources_for_test_truncate_logical(2);
    let mut out = [0x7b; 6];
    let error = match store.read_at(PayloadSpace::Logical, 0, &mut out, &mut work(), &cancel()) {
        Step::Failed(InputError::Io(error)) => error,
        _ => panic!("expected short read failure"),
    };
    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    assert_eq!(out, [0x7b; 6]);
    assert_eq!(store.byte_len(), 6);
    assert_eq!(line(&store, 0).content_end, 6);
    assert!(matches!(store.rank(&mut RankLookup::new(7), &mut work(), &cancel()), Step::Failed(InputError::Range)));
    retire(store.into_retirement());
}

#[test]
fn cleanup_failure_retains_owner_capacity_and_never_recurses_into_other_files() {
    let (store, _) = build(Text::new(b"abc", 1));
    let retirement = store.into_retirement();
    let directory = retirement.paths().directory.clone();
    let other = directory.join("other-owner-probe");
    fs::write(&other, b"preserve").unwrap();
    let held = retirement.usage();
    let retirement = match retirement.step(work(), &cancel()) {
        RetirementStep::Failed { retirement, error: InputError::Io(_) } => retirement,
        _ => panic!("nonempty directory must retain cleanup owner"),
    };
    assert_eq!(retirement.usage(), held);
    assert_eq!(fs::read(&other).unwrap(), b"preserve");
    // The test owns this probe; production retirement never removes it.
    fs::remove_file(other).unwrap();
    retire(retirement);
    assert!(!directory.exists());
}

#[test]
fn existing_directory_and_its_files_are_never_adopted_for_cleanup() {
    let a = admission();
    let directory = a.paths.directory.clone();
    let other = directory.join("logical");
    fs::create_dir(&directory).unwrap();
    fs::write(&other, b"foreign").unwrap();
    let build = match begin(Text::new(b"x", 1), a) {
        ControlFlow::Continue(build) => build,
        ControlFlow::Break(_) => panic!("begin"),
    };
    let retirement = match build.step(work(), &cancel()) {
        BuildStep::Failed { retirement, error: InputError::Io(error), .. } => {
            assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
            retirement
        }
        _ => panic!("must refuse existing directory"),
    };
    retire(retirement);
    assert_eq!(fs::read(&other).unwrap(), b"foreign");
    fs::remove_file(other).unwrap();
    fs::remove_dir(directory).unwrap();
}

#[test]
fn native_invocation_handle_is_captured_without_a_buffer_index_and_rechecks_stamp() {
    let path = unique("native");
    fs::write(&path, b"a\r\nb\n").unwrap();
    let lease = NativeFileLease::new(File::open(&path).unwrap(), InputIdentity([44, 0, 0, 0]));
    let (store, lease) = build_with(lease, admission(), ReadWork::new(7, 1));
    assert_eq!(store.line_count(), 3);
    assert_eq!(line(&store, 0).terminator, Terminator::CrLf);
    retire(store.into_retirement());
    drop(lease.into_file());

    let file = OpenOptions::new().read(true).write(true).open(&path).unwrap();
    let mut lease = NativeFileLease::new(file, InputIdentity([45, 0, 0, 0]));
    ready(lease.validate(&mut work(), &cancel()));
    OpenOptions::new().write(true).open(&path).unwrap().set_len(1).unwrap();
    assert!(matches!(lease.validate(&mut work(), &cancel()), Step::Failed(InputError::SourceChanged)));
    drop(lease.into_file());
    fs::remove_file(path).unwrap();
}

#[test]
fn buffered_disk_records_cross_windows_without_early_seal_or_lost_final_line() {
    static LINES: [u8; 2200] = [b'\n'; 2200];
    let (store, _) = build(Text::new(&LINES, 1));
    assert_eq!(store.byte_len(), LINES.len() as u64);
    assert_eq!(store.atom_count(), LINES.len() as u64);
    assert_eq!(store.line_count(), LINES.len() as u64 + 1);
    assert_eq!(line(&store, 2048).start, 2048);
    assert_eq!(
        line(&store, 2200),
        LineSpan { start: 2200, content_end: 2200, end: 2200, terminator: Terminator::None }
    );
    let atom = ready(store.atom_span(&mut AtomLookup::new(2199), &mut work(), &cancel())).unwrap();
    assert_eq!((atom.start, atom.end), (2199, 2200));
    retire(store.into_retirement());
}

#[test]
fn disk_admission_refuses_growth_before_the_crossing_write() {
    let mut a = admission();
    a.aggregate.disk = 4 * INPUT_WINDOW as u64;
    let source = Repeated { remaining: INPUT_WINDOW as u64 + 1, at: 0 };
    let mut build = match begin(source, a) {
        ControlFlow::Continue(build) => build,
        ControlFlow::Break(_) => panic!("begin"),
    };
    let mut failed = false;
    for _ in 0..32 {
        match build.step(work(), &cancel()) {
            BuildStep::Pending(next) => build = next,
            BuildStep::Failed { error: InputError::Admission(_), source, retirement } => {
                assert_eq!(source.remaining, 0);
                assert_eq!(fs::metadata(&retirement.paths().logical).unwrap().len(), INPUT_WINDOW as u64);
                retire(retirement);
                failed = true;
                break;
            }
            _ => panic!("expected growth admission refusal"),
        }
    }
    assert!(failed);
}

#[test]
fn cancelled_pending_payload_read_keeps_caller_output_and_allows_new_query() {
    let (mut store, _) = build(Text::new(b"abcdef", 6));
    let mut out = [0x55; 6];
    assert!(matches!(
        store.read_at(PayloadSpace::Logical, 0, &mut out, &mut ReadWork::new(2, 1), &cancel()),
        Step::Pending
    ));
    assert!(matches!(
        store.read_at(PayloadSpace::Logical, 1, &mut out, &mut work(), &cancel()),
        Step::Failed(InputError::QueryChanged)
    ));
    assert!(matches!(
        store.read_at(PayloadSpace::Logical, 0, &mut out, &mut work(), &AtomicBool::new(true)),
        Step::Cancelled
    ));
    assert_eq!(out, [0x55; 6]);
    assert_eq!(payload(&mut store, PayloadSpace::Logical, 1, &mut out), 5);
    assert_eq!(&out[..5], b"bcdef");
    retire(store.into_retirement());
}

#[test]
fn snapshot_cursor_adapter_admits_metadata_and_preserves_exact_returned_root() {
    let mut buffer = crate::Buffer::empty();
    buffer.insert(0, b"right\r\n").unwrap();
    buffer.insert(0, b"left\n").unwrap();
    let snapshot = buffer.snapshot();
    let root = snapshot.root.as_ref().unwrap().clone();
    let a = admission();
    let source = match SnapshotLease::try_new(snapshot, 0..12, InputIdentity([60, 0, 0, 0]), a.aggregate.clone()) {
        Ok(source) => source,
        Err(error) => panic!("{}", error.error),
    };
    let (mut store, source) = build_with(source, a, ReadWork::new(1, 1));
    let (snapshot, _) = source.into_parts();
    assert!(Arc::ptr_eq(snapshot.root.as_ref().unwrap(), &root));
    let mut out = [0; 12];
    payload(&mut store, PayloadSpace::Logical, 0, &mut out);
    assert_eq!(&out, b"left\nright\r\n");
    assert_eq!(line(&store, 1).terminator, Terminator::CrLf);
    retire(store.into_retirement());
}

#[test]
fn snapshot_constructor_refusal_returns_root_and_does_not_visit_source() {
    let mut buffer = crate::Buffer::empty();
    buffer.insert(0, b"x").unwrap();
    let snapshot = buffer.snapshot();
    let root = snapshot.root.as_ref().unwrap().clone();
    let mut budget = Budget::new();
    budget.resident = 0;
    let refused = match SnapshotLease::try_new(snapshot, 0..1, InputIdentity([61, 0, 0, 0]), budget) {
        Err(refused) => refused,
        Ok(_) => panic!("must refuse cursor construction"),
    };
    assert!(Arc::ptr_eq(refused.snapshot.root.as_ref().unwrap(), &root));
    assert!(matches!(refused.error, InputError::Admission(_)));
    assert_eq!(*refused.aggregate.usage.borrow(), Usage::default());
}

fn decoded_native(raw: &[u8], encoding: crate::encoding::Encoding, bom: usize) -> (InputStore<Budget>, PathBuf) {
    let path = unique("decoded-native");
    fs::write(&path, raw).unwrap();
    let native = NativeFileLease::new(File::open(&path).unwrap(), InputIdentity([70, 0, 0, 0]));
    let a = admission();
    let source = match DecodedNativeLease::try_new(
        native,
        crate::decoded_backing::Interpretation::new(encoding, bom).unwrap(),
        a.aggregate.clone(),
    ) {
        ControlFlow::Continue(source) => source,
        ControlFlow::Break(error) => panic!("{}", error.error),
    };
    let (store, source) = build_with(source, a, ReadWork::new(1, 1));
    let (native, _) = source.into_parts();
    drop(native.into_file());
    (store, path)
}

#[test]
fn selected_native_utf16_preserves_crlf_raw_atoms_and_explicit_bom_policy() {
    let (mut store, path) = decoded_native(b"\xff\xfeA\0\r\0\n\0B\0", crate::encoding::Encoding::UTF16_LE, 2);
    let mut out = [0; 4];
    payload(&mut store, PayloadSpace::Logical, 0, &mut out);
    assert_eq!(&out, b"A\r\nB");
    assert_eq!(store.line_count(), 2);
    assert_eq!(line(&store, 0), LineSpan { start: 0, content_end: 1, end: 3, terminator: Terminator::CrLf });
    let atom = ready(store.atom_span(&mut AtomLookup::new(0), &mut work(), &cancel())).unwrap();
    assert_eq!(atom.kind, CaptureKind::Preserved);
    let mut raw = [0; 2];
    payload(&mut store, PayloadSpace::Original, atom.original_start, &mut raw);
    assert_eq!(raw, [b'A', 0]);
    retire(store.into_retirement());
    fs::remove_file(path).unwrap();

    let (mut store, path) = decoded_native(b"\xef\xbb\xbfX", crate::encoding::Encoding::UTF8, 0);
    let mut out = [0; 4];
    payload(&mut store, PayloadSpace::Logical, 0, &mut out);
    assert_eq!(&out, b"\xef\xbb\xbfX");
    retire(store.into_retirement());
    fs::remove_file(path).unwrap();
}

#[test]
fn selected_native_invalid_atoms_have_distinct_exact_original_payloads() {
    let (mut store, path) = decoded_native(b"\x81\x82", crate::encoding::Encoding::UTF8, 0);
    let mut logical = [0; 2];
    payload(&mut store, PayloadSpace::Logical, 0, &mut logical);
    assert_eq!(logical, [0xff, 0xff]);
    let first = ready(store.atom_span(&mut AtomLookup::new(0), &mut work(), &cancel())).unwrap();
    let second = ready(store.atom_span(&mut AtomLookup::new(1), &mut work(), &cancel())).unwrap();
    assert_eq!(first.kind, CaptureKind::Opaque);
    assert_eq!(second.kind, CaptureKind::Opaque);
    let mut a = [0; 1];
    let mut b = [0; 1];
    payload(&mut store, PayloadSpace::Original, first.original_start, &mut a);
    payload(&mut store, PayloadSpace::Original, second.original_start, &mut b);
    assert_eq!(a, [0x81]);
    assert_eq!(b, [0x82]);
    assert_ne!(a, b);
    retire(store.into_retirement());
    fs::remove_file(path).unwrap();
}

#[test]
fn selected_native_big5_multiscalar_alias_stays_one_exact_atom() {
    let encoding = crate::encoding::Encoding::for_label("big5").unwrap();
    let (mut store, path) = decoded_native(b"\x88\x62", encoding, 0);
    let mut out = [0; 4];
    payload(&mut store, PayloadSpace::Logical, 0, &mut out);
    assert_eq!(&out, "Ê\u{304}".as_bytes());
    let atom = ready(store.atom_span(&mut AtomLookup::new(3), &mut work(), &cancel())).unwrap();
    assert_eq!((atom.start, atom.end, atom.original_start, atom.original_end), (0, 4, 0, 2));
    retire(store.into_retirement());
    fs::remove_file(path).unwrap();
}

#[test]
fn sealed_store_releases_all_snapshot_provenance_pins_with_the_returned_lease() {
    // Existing decoding is used ONLY to create this tiny immutable fixture,
    // never to index/capture a production comparison input.
    let path = unique("snapshot-opaque");
    fs::write(&path, b"\x81\x82").unwrap();
    let raw = crate::TextFile::open(&path).unwrap();
    let owner = crate::decoded_backing::DecodedBacking::prepare(
        &raw,
        crate::decoded_backing::Interpretation::new(crate::encoding::Encoding::UTF8, 0).unwrap(),
        &std::env::temp_dir(),
        &cancel(),
    )
    .unwrap();
    let buffer = crate::Buffer::from_decoded(owner.snapshot());
    let snapshot = buffer.snapshot();
    let a = admission();
    let source = match SnapshotLease::try_new(snapshot, 0..2, InputIdentity([80, 0, 0, 0]), a.aggregate.clone()) {
        Ok(source) => source,
        Err(error) => panic!("{}", error.error),
    };
    let (mut store, source) = build_with(source, a, ReadWork::new(1, 1));
    let (snapshot, _) = source.into_parts();
    drop(snapshot);
    drop(buffer);
    drop(raw);
    // Must succeed WHILE the sealed comparison store still exists.
    assert!(owner.try_retire().is_ok(), "sealed input retained foreign provenance pins");
    fs::remove_file(path).unwrap();
    let mut logical = [0; 2];
    payload(&mut store, PayloadSpace::Logical, 0, &mut logical);
    assert_eq!(logical, [0xff, 0xff]);
    let mut raw = [0; 2];
    payload(&mut store, PayloadSpace::Original, 0, &mut raw);
    assert_eq!(raw, [0x81, 0x82]);
    retire(store.into_retirement());
}

#[test]
fn selected_native_constructor_refusal_preserves_exact_invocation_handle() {
    let path = unique("native-refusal");
    fs::write(&path, b"keep").unwrap();
    let native = NativeFileLease::new(File::open(&path).unwrap(), InputIdentity([90, 0, 0, 0]));
    let mut budget = Budget::new();
    budget.resident = 0;
    let refused = match DecodedNativeLease::try_new(
        native,
        crate::decoded_backing::Interpretation::new(crate::encoding::Encoding::UTF8, 0).unwrap(),
        budget,
    ) {
        ControlFlow::Break(refused) => refused,
        ControlFlow::Continue(_) => panic!("must refuse native window allocation"),
    };
    assert_eq!(refused.native.identity(), InputIdentity([90, 0, 0, 0]));
    assert!(matches!(refused.error, InputError::Admission(_)));
    assert_eq!(*refused.aggregate.usage.borrow(), Usage::default());
    let file = refused.native.into_file();
    assert_eq!(file.metadata().unwrap().len(), 4);
    drop(file);
    fs::remove_file(path).unwrap();
}

#[test]
fn source_os_error_code_is_not_wrapped_or_converted_to_work_pending() {
    let mut source = Text::new(b"x", 1);
    source.error = Some(io::Error::from_raw_os_error(5));
    let build = match begin(source, admission()) {
        ControlFlow::Continue(build) => build,
        ControlFlow::Break(_) => panic!("begin"),
    };
    match build.step(work(), &cancel()) {
        BuildStep::Failed { error: InputError::Io(error), retirement, .. } => {
            assert_eq!(error.raw_os_error(), Some(5));
            retire(retirement);
        }
        _ => panic!("original OS error must be returned as an error"),
    }
}

include!("../encoding/utf32_compare_tests.rs");
