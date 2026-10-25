//! Small source fixtures only. ROOT runs these after exports/dependency/review.
//! All comparisons take the external route, including tiny fixtures.
use super::*;
use crate::store::{AlignmentPaths, RetirementStep};
use kwil_core_text::compare_input::{
    self as input, AdmissionError, BuildStep as InputBuildStep, CaptureKind, Captured, InputStore, SourceLease,
    StoreAdmission, StorePaths,
};
use std::{cell::RefCell, io, path::PathBuf, rc::Rc};

#[derive(Default)]
struct Ledger {
    live: Usage,
    peak: Usage,
    limit: Usage,
    refuse: bool,
}
#[derive(Clone)]
struct Budget(Rc<RefCell<Ledger>>);
impl Budget {
    fn new() -> Self {
        Self(Rc::new(RefCell::new(Ledger {
            limit: Usage { resident: 16 * 1024 * 1024, disk: 16 * 1024 * 1024, handles: 32 },
            ..Ledger::default()
        })))
    }
}
impl Admission for Budget {
    fn reserve(&mut self, add: Usage) -> Result<(), AdmissionError> {
        let mut x = self.0.borrow_mut();
        let resident = x.live.resident.checked_add(add.resident).ok_or(AdmissionError)?;
        let disk = x.live.disk.checked_add(add.disk).ok_or(AdmissionError)?;
        let handles = x.live.handles.checked_add(add.handles).ok_or(AdmissionError)?;
        if x.refuse || resident > x.limit.resident || disk > x.limit.disk || handles > x.limit.handles {
            return Err(AdmissionError);
        }
        x.live = Usage { resident, disk, handles };
        x.peak.resident = x.peak.resident.max(resident);
        x.peak.disk = x.peak.disk.max(disk);
        x.peak.handles = x.peak.handles.max(handles);
        Ok(())
    }
    fn release(&mut self, usage: Usage) {
        let mut x = self.0.borrow_mut();
        x.live.resident -= usage.resident;
        x.live.disk -= usage.disk;
        x.live.handles -= usage.handles;
    }
}
#[derive(Clone, Copy)]
struct Atom<'a> {
    logical: &'a [u8],
    raw: &'a [u8],
    kind: CaptureKind,
    encoding: u64,
}
fn text(bytes: &[u8]) -> Atom<'_> {
    Atom { logical: bytes, raw: &[], kind: CaptureKind::Text, encoding: 0 }
}
struct Lease<'a> {
    atoms: &'a [Atom<'a>],
    at: usize,
    credit: usize,
    identity: InputIdentity,
}
impl SourceLease for Lease<'_> {
    fn identity(&self) -> InputIdentity {
        self.identity
    }
    fn validate(&mut self, work: &mut ReadWork, cancel: &AtomicBool) -> Step<()> {
        work.debit(0, 0, cancel)
    }
    fn capture(
        &mut self,
        logical: &mut [u8],
        original: &mut [u8],
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<Option<Captured>> {
        gate!(work.debit(0, 0, cancel));
        let Some(atom) = self.atoms.get(self.at) else {
            return Step::Ready(None);
        };
        let charge = (atom.logical.len() + atom.raw.len()) * 2;
        let n = (charge - self.credit).min(work.bytes.min(4096) as usize);
        gate!(work.debit(n as u64, 0, cancel));
        self.credit += n;
        if self.credit < charge {
            return Step::Pending;
        }
        assert!(atom.logical.len() <= logical.len() && atom.raw.len() <= original.len());
        logical[..atom.logical.len()].copy_from_slice(atom.logical);
        original[..atom.raw.len()].copy_from_slice(atom.raw);
        self.at += 1;
        self.credit = 0;
        Step::Ready(Some(Captured {
            logical: atom.logical.len(),
            original: atom.raw.len(),
            kind: atom.kind,
            encoding: atom.encoding,
        }))
    }
}
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    root: PathBuf,
    budget: Budget,
}
impl Fixture {
    fn new() -> Self {
        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("kwil-external-source-{}-{serial}", std::process::id()));
        // Exclusive ownership; a collision fails without deleting anything.
        std::fs::create_dir(&root).unwrap();
        Self { root, budget: Budget::new() }
    }
    fn paths(&self, name: &str) -> AlignmentPaths {
        let directory = self.root.join(name);
        AlignmentPaths { files: ["forward", "reverse", "stack", "runs", "hunks"].map(|n| directory.join(n)), directory }
    }
    fn input(&self, name: &str, atoms: &[Atom<'_>], identity: u64) -> InputStore<Budget> {
        let directory = self.root.join(name);
        let paths = StorePaths {
            logical: directory.join("logical"),
            original: directory.join("original"),
            lines: directory.join("lines"),
            atoms: directory.join("atoms"),
            directory,
        };
        let lease = Lease { atoms, at: 0, credit: 0, identity: InputIdentity([identity, 0, 0, 0]) };
        let mut build = match input::begin(lease, StoreAdmission { aggregate: self.budget.clone(), paths }) {
            std::ops::ControlFlow::Continue(b) => b,
            std::ops::ControlFlow::Break(rejected) => panic!("fixture admission: {}", rejected.error),
        };
        let cancel = AtomicBool::new(false);
        for _ in 0..100_000 {
            match build.step(ReadWork::new(257, 3), &cancel) {
                InputBuildStep::Pending(next) => build = next,
                InputBuildStep::Ready { store, source } => {
                    assert_eq!(source.identity(), InputIdentity([identity, 0, 0, 0]));
                    return store;
                }
                _ => panic!("fixture input build failed"),
            }
        }
        panic!("bounded fixture input did not progress")
    }
    fn build(
        &self,
        a: InputStore<Budget>,
        b: InputStore<Budget>,
        options: StreamOptions,
        quantum: u64,
    ) -> DiskComparison<InputStore<Budget>, InputStore<Budget>, Budget> {
        let mut build = match begin(
            a,
            b,
            options,
            AlignmentAdmission { aggregate: self.budget.clone(), paths: self.paths("align") },
        ) {
            ControlFlow::Continue(b) => b,
            ControlFlow::Break(_) => panic!("alignment admission"),
        };
        let cancel = AtomicBool::new(false);
        for _ in 0..100_000 {
            assert_eq!(build.progress().quality, Quality::Pending);
            let step = build.step(ReadWork::new(quantum, 2), &cancel);
            match step.state() {
                BuildState::Pending => build = continued(step.into_pending()),
                BuildState::Ready => return continued(step.into_ready()),
                BuildState::Failed => panic!("alignment failed: {}", continued(step.into_terminal()).error.unwrap()),
                BuildState::Cancelled => panic!("unexpected cancel"),
            }
        }
        panic!("bounded external fixture did not progress")
    }
    fn finish(self) {
        assert_eq!(self.budget.0.borrow().live, Usage::default());
        assert_eq!(std::fs::read_dir(&self.root).unwrap().count(), 0);
        std::fs::remove_dir(self.root).unwrap();
    }
}
fn continued<B, C>(flow: ControlFlow<B, C>) -> C {
    match flow {
        ControlFlow::Continue(owner) => owner,
        ControlFlow::Break(_) => panic!("wrong ownership lane"),
    }
}
fn retire_alignment(mut retirement: AlignmentRetirement<Budget>) {
    let cancel = AtomicBool::new(false);
    for _ in 0..32 {
        match retirement.step(ReadWork::new(0, 1), &cancel) {
            RetirementStep::Pending(next) => retirement = next,
            RetirementStep::Ready(_) => return,
            _ => panic!("alignment retirement failed"),
        }
    }
    panic!("retirement did not progress")
}
fn retire_input(store: InputStore<Budget>) {
    let mut retirement = store.into_retirement();
    let cancel = AtomicBool::new(false);
    for _ in 0..32 {
        match retirement.step(ReadWork::new(0, 1), &cancel) {
            input::RetirementStep::Pending(next) => retirement = next,
            input::RetirementStep::Ready(_) => return,
            _ => panic!("input retirement failed"),
        }
    }
    panic!("input retirement did not progress")
}
fn retire_result<L: InputSource, R: InputSource>(result: DiskComparison<L, R, Budget>) -> (L, R) {
    let (a, b, _, retirement) = result.into_parts();
    retire_alignment(retirement);
    (a, b)
}
fn rows<L: InputSource, R: InputSource>(result: &mut DiskComparison<L, R, Budget>, quantum: u64) -> Vec<AlignedRow> {
    let mut out = Vec::new();
    let cancel = AtomicBool::new(false);
    while out.len() < result.row_count() as usize {
        let mut query = PageQuery::new(out.len() as u64, PAGE_RECORDS);
        let mut page = Page::default();
        for _ in 0..100_000 {
            let old = page.clone();
            match result.page(&mut query, &mut page, &mut ReadWork::new(quantum, 1), &cancel) {
                Step::Pending => {
                    assert_eq!(page.rows, old.rows);
                    assert_eq!(page.quality, Quality::Pending);
                }
                Step::Ready(()) => {
                    assert_eq!(page.quality, Quality::Complete);
                    break;
                }
                _ => panic!("page lookup failed"),
            }
        }
        assert!(page.len > 0);
        out.extend(page.rows[..page.len].iter().map(|r| r.unwrap()));
    }
    out
}

/// Independent fixture normalizer: no Cursor/Equality/engine methods used.
fn reference(bytes: &[u8], options: StreamOptions) -> Vec<Vec<u16>> {
    let mut lines = Vec::new();
    let mut start = 0;
    for end in bytes.iter().enumerate().filter_map(|(i, b)| (*b == b'\n').then_some(i + 1)) {
        lines.push(&bytes[start..end]);
        start = end;
    }
    lines.push(&bytes[start..]);
    lines
        .into_iter()
        .map(|line| {
            let terminator = if line.ends_with(b"\r\n") { 2 } else { usize::from(line.ends_with(b"\n")) };
            let mut content = &line[..line.len() - terminator];
            if options.whitespace == Whitespace::Trailing {
                let end = content.iter().rposition(|b| ![b' ', b'\t', 0x0b, 0x0c].contains(b)).map_or(0, |i| i + 1);
                content = &content[..end];
            }
            let mut output = Vec::new();
            let mut was_space = false;
            for b in content {
                let horizontal = [b' ', b'\t', 0x0b, 0x0c].contains(b);
                if options.whitespace == Whitespace::All && horizontal {
                    continue;
                }
                if options.whitespace == Whitespace::Amount && horizontal {
                    if !was_space {
                        output.push(u16::from(b' '));
                    }
                } else {
                    output.push(u16::from(*b));
                }
                was_space = horizontal;
            }
            output.push(256 + if options.ignore_line_endings && terminator == 2 { 1 } else { terminator as u16 });
            output
        })
        .collect()
}
fn optimum(a: &[Vec<u16>], b: &[Vec<u16>]) -> usize {
    let mut previous = vec![0; b.len() + 1];
    for x in a {
        let mut current = vec![0; b.len() + 1];
        for (j, y) in b.iter().enumerate() {
            current[j + 1] = if x == y { previous[j] + 1 } else { previous[j + 1].max(current[j]) };
        }
        previous = current;
    }
    a.len() + b.len() - previous[b.len()] * 2
}
#[test]
fn exact_external_alignment_matches_independent_dp_with_one_byte_quanta() {
    let cases: &[(&[u8], &[u8])] = &[
        (b"", b""),
        (b"", b"x\n"),
        (b"x\n", b""),
        (b"a\nb\n", b"x\ny\n"),
        (b"a\nb\na\n", b"b\na\nb\n"),
        (b"head\ntail\n", b"head\nnew\ntail\n"),
        (b"x\nx\nx\n", b"x\ny\nx\n"),
        (b"a\nb\n", b"b\nb\n"),
        (b"a\r \n", b"a\r\n"),
        (b"x \t\r\na  b\nend", b"x\na\tb\r\nend\n"),
    ];
    for &(left, right) in cases {
        for whitespace in [Whitespace::Exact, Whitespace::Trailing, Whitespace::All, Whitespace::Amount] {
            for ignore_line_endings in [false, true] {
                let options = StreamOptions { whitespace, ignore_line_endings };
                let f = Fixture::new();
                let a_atoms = [text(left)];
                let b_atoms = [text(right)];
                // Empty source means no zero-length capture atom.
                let a = f.input("left", if left.is_empty() { &[] } else { &a_atoms }, 1);
                let b = f.input("right", if right.is_empty() { &[] } else { &b_atoms }, 2);
                let mut result = f.build(a, b, options, 1);
                let actual = rows(&mut result, 1);
                let (a, b) = (reference(left, options), reference(right, options));
                let (mut l, mut r, mut cost) = (0, 0, 0);
                for (index, row) in actual.iter().enumerate() {
                    assert_eq!(row.aligned, index as u64);
                    match row.kind {
                        Kind::Equal | Kind::Modify => {
                            assert_eq!(row.left, Some(l as u64));
                            assert_eq!(row.right, Some(r as u64));
                            if row.kind == Kind::Equal {
                                assert_eq!(a[l], b[r]);
                            } else {
                                cost += 2;
                            }
                            l += 1;
                            r += 1;
                        }
                        Kind::Insert => {
                            assert_eq!(row.left, None);
                            assert_eq!(row.right, Some(r as u64));
                            r += 1;
                            cost += 1;
                        }
                        Kind::Delete => {
                            assert_eq!(row.left, Some(l as u64));
                            assert_eq!(row.right, None);
                            l += 1;
                            cost += 1;
                        }
                    }
                }
                assert_eq!((l, r), (a.len(), b.len()));
                assert_eq!(cost, optimum(&a, &b));
                let (a, b) = retire_result(result);
                retire_input(a);
                retire_input(b);
                f.finish();
            }
        }
    }
}
#[test]
fn cross_window_whitespace_and_opaque_aliases_never_use_marker_or_hash_equality() {
    let mut a = vec![b'x'; 769];
    a.extend_from_slice(b" \t\r\n");
    let mut b = vec![b'x'; 769];
    b.extend_from_slice(b"\r\n");
    let f = Fixture::new();
    let left = f.input("left", &[text(&a)], 1);
    let right = f.input("right", &[text(&b)], 2);
    let mut result =
        f.build(left, right, StreamOptions { whitespace: Whitespace::Trailing, ..StreamOptions::default() }, 3);
    assert!(rows(&mut result, 3).iter().all(|r| r.kind == Kind::Equal));
    let (a, b) = retire_result(result);
    retire_input(a);
    retire_input(b);
    f.finish();
    for (kind, codec, raw, changed) in [
        (CaptureKind::Opaque, 7, &b"different"[..], true),
        (CaptureKind::Opaque, 8, &b"same"[..], true),
        (CaptureKind::Opaque, 7, &b"same"[..], false),
        (CaptureKind::Preserved, 8, &b"different"[..], false),
    ] {
        let f = Fixture::new();
        let a = Atom { logical: b"\xff", raw: b"same", kind, encoding: 7 };
        let b = Atom { logical: b"\xff", raw, kind, encoding: codec };
        let left = f.input("left", &[a], 1);
        let right = f.input("right", &[b], 2);
        let mut result = f.build(left, right, StreamOptions::default(), 1);
        assert_eq!(rows(&mut result, 1).iter().any(|r| r.kind != Kind::Equal), changed);
        let (a, b) = retire_result(result);
        retire_input(a);
        retire_input(b);
        f.finish();
    }
    // Text 0xff and opaque 0xff must differ even with identical marker bytes.
    let f = Fixture::new();
    let opaque = Atom { logical: b"\xff", raw: b"same", kind: CaptureKind::Opaque, encoding: 7 };
    let left = f.input("left", &[opaque], 1);
    let right = f.input("right", &[text(b"\xff")], 2);
    let mut result = f.build(left, right, StreamOptions::default(), 1);
    assert!(rows(&mut result, 1).iter().any(|r| r.kind != Kind::Equal));
    let (a, b) = retire_result(result);
    retire_input(a);
    retire_input(b);
    f.finish();
}

#[derive(Debug)]
struct OriginalError;
impl std::fmt::Display for OriginalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("fixture original error")
    }
}
impl std::error::Error for OriginalError {}
struct Fault {
    input: InputStore<Budget>,
    error: Option<io::Error>,
    cancel_on_read: bool,
}
impl InputSource for Fault {
    fn identity(&self) -> InputIdentity {
        self.input.identity()
    }
    fn byte_len(&self) -> u64 {
        self.input.byte_len()
    }
    fn line_count(&self) -> u64 {
        self.input.line_count()
    }
    fn original_len(&self) -> u64 {
        self.input.original_len()
    }
    fn read_at(
        &mut self,
        space: PayloadSpace,
        at: u64,
        out: &mut [u8],
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<usize> {
        gate!(work.debit(0, 1, cancel));
        if let Some(error) = self.error.take() {
            return Step::Failed(InputError::Io(error));
        }
        if self.cancel_on_read {
            cancel.store(true, Ordering::Relaxed);
        }
        self.input.read_at(space, at, out, work, cancel)
    }
    fn line_span(&self, query: &mut LineLookup, work: &mut ReadWork, cancel: &AtomicBool) -> Step<LineSpan> {
        self.input.line_span(query, work, cancel)
    }
    fn rank(&self, query: &mut input::RankLookup, work: &mut ReadWork, cancel: &AtomicBool) -> Step<input::Rank> {
        self.input.rank(query, work, cancel)
    }
    fn atom_span(&self, query: &mut AtomLookup, work: &mut ReadWork, cancel: &AtomicBool) -> Step<Option<AtomSpan>> {
        self.input.atom_span(query, work, cancel)
    }
}
#[test]
fn cancellation_io_and_admission_refusal_return_complete_input_and_store_owners() {
    for mode in 0..3 {
        let f = Fixture::new();
        let left = Fault {
            input: f.input("left", &[text(b"old\n")], 1),
            cancel_on_read: mode == 1,
            error: (mode == 0).then(|| io::Error::new(io::ErrorKind::PermissionDenied, OriginalError)),
        };
        let right = f.input("right", &[text(b"new\n")], 2);
        let admission = AlignmentAdmission { aggregate: f.budget.clone(), paths: f.paths("align") };
        if mode == 2 {
            f.budget.0.borrow_mut().refuse = true;
        }
        let mut build = match begin(left, right, StreamOptions::default(), admission) {
            ControlFlow::Continue(build) => build,
            ControlFlow::Break(rejected) => {
                assert_eq!(mode, 2);
                assert!(matches!(rejected.error, InputError::Admission(_)));
                assert_eq!(rejected.left.identity(), InputIdentity([1, 0, 0, 0]));
                retire_input(rejected.left.input);
                retire_input(rejected.right);
                f.finish();
                continue;
            }
        };
        let cancel = AtomicBool::new(false);
        for _ in 0..10_000 {
            let step = build.step(ReadWork::new(127, 4), &cancel);
            match step.state() {
                BuildState::Pending => build = continued(step.into_pending()),
                BuildState::Failed => {
                    let TerminalOwners { left, right, retirement, error, .. } = continued(step.into_terminal());
                    assert_eq!(mode, 0);
                    let InputError::Io(error) = error.unwrap() else {
                        panic!("original I/O error lost");
                    };
                    assert!(error.get_ref().unwrap().is::<OriginalError>());
                    assert_eq!(left.identity(), InputIdentity([1, 0, 0, 0]));
                    retire_alignment(retirement);
                    retire_input(left.input);
                    retire_input(right);
                    break;
                }
                BuildState::Cancelled => {
                    let TerminalOwners { left, right, retirement, .. } = continued(step.into_terminal());
                    assert_eq!(mode, 1);
                    retire_alignment(retirement);
                    retire_input(left.input);
                    retire_input(right);
                    break;
                }
                BuildState::Ready => panic!("fault published a comparison"),
            }
        }
        f.finish();
    }
}
#[test]
fn disk_growth_refusal_and_retirement_foreign_child_preserve_ownership() {
    // Admission refusal is injected after construction, at first disk growth;
    // no giant fixture or load is needed to cross the 64KiB disk extent.
    let f = Fixture::new();
    let left = f.input("left", &[text(b"a\n")], 1);
    let right = f.input("right", &[text(b"b\n")], 2);
    let paths = f.paths("align");
    let mut records = match Records::begin(AlignmentAdmission { aggregate: f.budget.clone(), paths }, 4096) {
        ControlFlow::Continue(r) => r,
        ControlFlow::Break(_) => panic!("record admission"),
    };
    let cancel = AtomicBool::new(false);
    assert!(matches!(records.open(&mut ReadWork::new(0, 6), &cancel), Step::Ready(())));
    f.budget.0.borrow_mut().refuse = true;
    assert!(matches!(
        records.write(store::FORWARD, 1024, [1; 8], &mut ReadWork::new(256, 2), &cancel),
        Step::Failed(InputError::Admission(_))
    ));
    let retirement = records.retirement();
    let foreign = retirement.paths().directory.join("foreign-child");
    std::fs::write(&foreign, b"keep").unwrap();
    let retirement = match retirement.step(ReadWork::new(0, 32), &cancel) {
        RetirementStep::Failed { retirement, error: InputError::Io(_) } => retirement,
        _ => panic!("must not recursively remove a foreign child"),
    };
    assert!(foreign.exists());
    assert!(retirement.usage().handles > 0);
    // This fixture created the child; only the fixture removes it.
    std::fs::remove_file(foreign).unwrap();
    retire_alignment(retirement);
    retire_input(left);
    retire_input(right);
    f.finish();
}
#[test]
fn pending_pages_keep_output_and_unique_query_owner_and_navigation_is_strict() {
    let f = Fixture::new();
    let left = f.input("left", &[text(b"0\nold\n2\nold2\n4\n")], 1);
    let right = f.input("right", &[text(b"0\nnew\n2\nnew2\n4\n")], 2);
    let mut result = f.build(left, right, StreamOptions::default(), 7);
    let cancel = AtomicBool::new(false);
    let mut first = PageQuery::new(0, 4);
    let mut other = PageQuery::new(0, 4);
    let mut out = Page::default();
    assert!(matches!(result.page(&mut first, &mut out, &mut ReadWork::new(1, 1), &cancel), Step::Pending));
    assert!(matches!(
        result.page(&mut other, &mut out, &mut ReadWork::new(100, 1), &cancel),
        Step::Failed(InputError::QueryChanged)
    ));
    assert_eq!(out.quality, Quality::Pending);
    assert_eq!(out.len, 0);
    result.abandon_query();
    assert!(matches!(
        result.page(&mut first, &mut out, &mut ReadWork::new(100, 1), &cancel),
        Step::Failed(InputError::QueryChanged)
    ));
    for (row, forward, expected) in
        [(0, true, Some(1)), (1, true, Some(3)), (3, true, None), (3, false, Some(1)), (1, false, None)]
    {
        let mut query = NavigationQuery::new(row, forward);
        let mut completed = false;
        for _ in 0..1000 {
            match result.navigate(&mut query, &mut ReadWork::new(1, 1), &cancel) {
                Step::Pending => {}
                Step::Ready(hunk) => {
                    assert_eq!(hunk.map(|h| h.rows.start), expected);
                    completed = true;
                    break;
                }
                _ => panic!("navigation failed"),
            }
        }
        assert!(completed, "navigation must complete under one-byte quanta");
    }
    let (a, b) = retire_result(result);
    retire_input(a);
    retire_input(b);
    f.finish();
}

#[test]
fn query_leases_cannot_cross_comparisons_with_same_inputs_and_generation() {
    let a = Fixture::new();
    let b = Fixture::new();
    let cancel = AtomicBool::new(false);
    let mut first =
        a.build(a.input("left", &[text(b"x\n")], 1), a.input("right", &[text(b"y\n")], 2), StreamOptions::default(), 7);
    let mut second =
        b.build(b.input("left", &[text(b"x\n")], 1), b.input("right", &[text(b"y\n")], 2), StreamOptions::default(), 7);
    let mut qa = PageQuery::new(0, 2);
    let mut qb = PageQuery::new(0, 2);
    let mut page = Page::default();
    assert!(matches!(first.page(&mut qa, &mut page, &mut ReadWork::new(1, 1), &cancel), Step::Pending));
    assert!(matches!(second.page(&mut qb, &mut page, &mut ReadWork::new(1, 1), &cancel), Step::Pending));
    assert!(matches!(
        second.page(&mut qa, &mut page, &mut ReadWork::new(4096, 4), &cancel),
        Step::Failed(InputError::QueryChanged)
    ));
    first.abandon_query();
    second.abandon_query();
    let mut na = NavigationQuery::new(0, true);
    let mut nb = NavigationQuery::new(0, true);
    assert!(matches!(first.navigate(&mut na, &mut ReadWork::new(1, 1), &cancel), Step::Pending));
    assert!(matches!(second.navigate(&mut nb, &mut ReadWork::new(1, 1), &cancel), Step::Pending));
    assert!(matches!(
        second.navigate(&mut na, &mut ReadWork::new(4096, 4), &cancel),
        Step::Failed(InputError::QueryChanged)
    ));
    let (x, y) = retire_result(first);
    retire_input(x);
    retire_input(y);
    a.finish();
    let (x, y) = retire_result(second);
    retire_input(x);
    retire_input(y);
    b.finish();
}

#[test]
fn opaque_original_stream_crosses_windows_and_retains_exact_pending_query() {
    for different in [false, true] {
        let raw = vec![0x80; 513];
        let mut other = raw.clone();
        if different {
            other[512] = 0x81;
        }
        let f = Fixture::new();
        let a = Atom { logical: b"\xff", raw: &raw, kind: CaptureKind::Opaque, encoding: 4 };
        let b = Atom { logical: b"\xff", raw: &other, kind: CaptureKind::Opaque, encoding: 4 };
        let left = f.input("left", &[a], 1);
        let right = f.input("right", &[b], 2);
        let mut result = f.build(left, right, StreamOptions::default(), 1);
        assert_eq!(rows(&mut result, 1).iter().any(|r| r.kind != Kind::Equal), different);
        let (a, b) = retire_result(result);
        retire_input(a);
        retire_input(b);
        f.finish();
    }
}

#[test]
fn no_work_and_cancelled_page_never_publish_complete_empty_differences() {
    let f = Fixture::new();
    let cancel = AtomicBool::new(false);
    let left = f.input("left", &[text(b"a\n")], 1);
    let right = f.input("right", &[text(b"b\n")], 2);
    let build = match begin(
        left,
        right,
        StreamOptions::default(),
        AlignmentAdmission { aggregate: f.budget.clone(), paths: f.paths("align") },
    ) {
        ControlFlow::Continue(b) => b,
        ControlFlow::Break(_) => panic!("fixture admission"),
    };
    let step = build.step(ReadWork::new(0, 0), &cancel);
    assert_eq!(step.state(), BuildState::Pending);
    // Even a wrong-tag extraction must return the exact complete carrier.
    let step = match step.into_ready() {
        ControlFlow::Break(owner) => owner,
        ControlFlow::Continue(_) => panic!("must pause"),
    };
    let build = continued(step.into_pending());
    assert_eq!(build.progress().quality, Quality::Pending);
    assert_eq!(std::fs::read_dir(&f.root).unwrap().count(), 2);
    let (left, right, _, retirement) = build.into_parts();
    retire_alignment(retirement);
    let mut result = f.build(left, right, StreamOptions::default(), 7);
    let mut query = PageQuery::new(0, 2);
    let mut output = Page::default();
    cancel.store(true, Ordering::Relaxed);
    assert!(matches!(result.page(&mut query, &mut output, &mut ReadWork::new(4096, 10), &cancel), Step::Cancelled));
    assert_eq!(output.quality, Quality::Pending);
    assert_eq!(output.len, 0);
    let (a, b) = retire_result(result);
    retire_input(a);
    retire_input(b);
    f.finish();
}

#[test]
fn unfinished_word_backend_is_explicit_for_giant_visible_ranges() {
    let row = AlignedRow { aligned: 0, left: Some(0), right: Some(0), kind: Kind::Modify };
    let refinement = crate::words::request(row, 0..u64::MAX, 0..u64::MAX);
    assert_eq!(refinement.state(), crate::words::RefinementState::Pending);
    assert_eq!(refinement.pending_reason(), Some(crate::words::PendingReason::BackendRequired));
    match refinement.coverage() {
        Some(coverage) => {
            assert!(!coverage.complete);
            assert_eq!(coverage.left, 0..0);
            assert_eq!(coverage.right, 0..0);
        }
        _ => panic!("unfinished giant-line word refinement must not be skipped or ready"),
    }
}
