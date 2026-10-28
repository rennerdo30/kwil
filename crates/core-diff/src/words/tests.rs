//! Source-authored acceptance fixtures. ROOT owns execution and fail-first runs.
use super::*;
use crate::{
    store::{AlignmentAdmission, AlignmentPaths, RetirementStep},
    stream::{self, BuildState, DiskComparison, Page, PageQuery, StreamOptions},
};
use kwil_core_text::compare_input::{
    self as input, Admission, AdmissionError, AtomLookup, AtomSpan, CaptureKind, Captured, InputIdentity, InputSource,
    InputStore, LineLookup, LineSpan, PayloadSpace, Rank, RankLookup, ReadWork, SourceLease, Step, StoreAdmission,
    StorePaths, Usage,
};
use std::{
    cell::RefCell,
    ops::ControlFlow,
    path::PathBuf,
    rc::Rc,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

#[derive(Default)]
struct Ledger {
    live: Usage,
    peak: Usage,
    refuse: bool,
}
#[derive(Clone, Default)]
struct Budget(Rc<RefCell<Ledger>>);
impl Admission for Budget {
    fn reserve(&mut self, u: Usage) -> Result<(), AdmissionError> {
        let mut l = self.0.borrow_mut();
        if l.refuse {
            return Err(AdmissionError);
        }
        l.live.resident = l.live.resident.checked_add(u.resident).ok_or(AdmissionError)?;
        l.live.disk = l.live.disk.checked_add(u.disk).ok_or(AdmissionError)?;
        l.live.handles = l.live.handles.checked_add(u.handles).ok_or(AdmissionError)?;
        l.peak.resident = l.peak.resident.max(l.live.resident);
        l.peak.disk = l.peak.disk.max(l.live.disk);
        l.peak.handles = l.peak.handles.max(l.live.handles);
        Ok(())
    }
    fn release(&mut self, u: Usage) {
        let mut l = self.0.borrow_mut();
        l.live.resident -= u.resident;
        l.live.disk -= u.disk;
        l.live.handles -= u.handles;
    }
}
#[derive(Clone, Copy)]
struct Atom<'a> {
    logical: &'a [u8],
    raw: &'a [u8],
    kind: CaptureKind,
    encoding: u64,
}
fn text(b: &[u8]) -> Atom<'_> {
    Atom { logical: b, raw: &[], kind: CaptureKind::Text, encoding: 0 }
}
struct Lease<'a> {
    atoms: &'a [Atom<'a>],
    at: usize,
    id: InputIdentity,
}
impl SourceLease for Lease<'_> {
    fn identity(&self) -> InputIdentity {
        self.id
    }
    fn validate(&mut self, w: &mut ReadWork, c: &AtomicBool) -> Step<()> {
        w.debit(0, 0, c)
    }
    fn capture(&mut self, l: &mut [u8], r: &mut [u8], w: &mut ReadWork, c: &AtomicBool) -> Step<Option<Captured>> {
        let Some(a) = self.atoms.get(self.at) else {
            return Step::Ready(None);
        };
        match w.debit((a.logical.len() + a.raw.len()) as u64 * 2, 1, c) {
            Step::Ready(()) => {}
            Step::Pending => return Step::Pending,
            Step::Cancelled => return Step::Cancelled,
            Step::Failed(e) => return Step::Failed(e),
        }
        l[..a.logical.len()].copy_from_slice(a.logical);
        r[..a.raw.len()].copy_from_slice(a.raw);
        self.at += 1;
        Step::Ready(Some(Captured {
            logical: a.logical.len(),
            original: a.raw.len(),
            kind: a.kind,
            encoding: a.encoding,
        }))
    }
}

#[derive(Default)]
struct Stats {
    resets: usize,
    pending: bool,
    reads: usize,
    refuse: bool,
    change_identity: bool,
}
struct Source {
    store: InputStore<Budget>,
    stats: Rc<RefCell<Stats>>,
}
impl InputSource for Source {
    fn identity(&self) -> InputIdentity {
        if self.stats.borrow().change_identity {
            let mut id = self.store.identity();
            id.0[3] ^= 1;
            id
        } else {
            self.store.identity()
        }
    }
    fn byte_len(&self) -> u64 {
        self.store.byte_len()
    }
    fn original_len(&self) -> u64 {
        self.store.original_len()
    }
    fn line_count(&self) -> u64 {
        self.store.line_count()
    }
    fn read_at(&mut self, s: PayloadSpace, a: u64, o: &mut [u8], w: &mut ReadWork, c: &AtomicBool) -> Step<usize> {
        self.stats.borrow_mut().reads += 1;
        if self.stats.borrow().refuse {
            return Step::Failed(InputError::Io(std::io::Error::other("word refusal payload")));
        }
        let result = self.store.read_at(s, a, o, w, c);
        if matches!(result, Step::Pending) {
            self.stats.borrow_mut().pending = true;
        }
        result
    }
    fn line_span(&self, q: &mut LineLookup, w: &mut ReadWork, c: &AtomicBool) -> Step<LineSpan> {
        self.store.line_span(q, w, c)
    }
    fn atom_span(&self, q: &mut AtomLookup, w: &mut ReadWork, c: &AtomicBool) -> Step<Option<AtomSpan>> {
        self.store.atom_span(q, w, c)
    }
    fn rank(&self, q: &mut RankLookup, w: &mut ReadWork, c: &AtomicBool) -> Step<Rank> {
        self.store.rank(q, w, c)
    }
}
impl WordInput for Source {
    fn has_pending_payload_read(&self) -> bool {
        self.store.has_pending_payload_read()
    }
    fn abandon_word_read(&mut self) {
        self.stats.borrow_mut().resets += 1;
        self.store.abandon_payload_read();
    }
}
type Comparison = DiskComparison<Source, Source, Budget>;
type Build = WordBuild<Source, Source, Budget, Budget>;
static NEXT: AtomicU64 = AtomicU64::new(0);
fn work() -> ReadWork {
    ReadWork::new(u64::MAX, 100_000)
}
fn never() -> AtomicBool {
    AtomicBool::new(false)
}
struct Fixture {
    root: PathBuf,
    budget: Budget,
}
impl Fixture {
    fn new() -> Self {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("kwil-word-owned-{}-{id}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        Self { root, budget: Budget::default() }
    }
    fn paths(&self, n: &str) -> AlignmentPaths {
        let directory = self.root.join(n);
        AlignmentPaths { files: ["forward", "reverse", "stack", "runs", "hunks"].map(|n| directory.join(n)), directory }
    }
    fn admission(&self, n: &str) -> AlignmentAdmission<Budget> {
        AlignmentAdmission { aggregate: self.budget.clone(), paths: self.paths(n) }
    }
    fn source(&self, n: &str, atoms: &[Atom<'_>], id: u64) -> Source {
        let directory = self.root.join(n);
        let paths = StorePaths {
            logical: directory.join("logical"),
            original: directory.join("original"),
            lines: directory.join("lines"),
            atoms: directory.join("atoms"),
            directory,
        };
        let mut build = match input::begin(
            Lease { atoms, at: 0, id: InputIdentity([id, 0, 0, 0]) },
            StoreAdmission { aggregate: self.budget.clone(), paths },
        ) {
            ControlFlow::Continue(b) => b,
            ControlFlow::Break(r) => panic!("source refused {}", r.error),
        };
        for _ in 0..10_000 {
            match build.step(work(), &never()) {
                input::BuildStep::Pending(b) => build = b,
                input::BuildStep::Ready { store, .. } => return Source { store, stats: Rc::default() },
                _ => panic!("source failed"),
            }
        }
        panic!("source never ready")
    }
    fn comparison(&self, left: &[Atom<'_>], right: &[Atom<'_>]) -> Comparison {
        let mut build = match stream::begin(
            self.source("left", left, 1),
            self.source("right", right, 2),
            StreamOptions::default(),
            self.admission("line"),
        ) {
            ControlFlow::Continue(b) => b,
            ControlFlow::Break(r) => panic!("line refused {}", r.error),
        };
        for _ in 0..100_000 {
            let step = build.step(work(), &never());
            let state = step.state();
            if state == BuildState::Ready {
                return match step.into_ready() {
                    ControlFlow::Continue(c) => c,
                    ControlFlow::Break(_) => panic!("ready extraction"),
                };
            }
            build = match step.into_pending() {
                ControlFlow::Continue(b) => b,
                ControlFlow::Break(_) => panic!("line terminal"),
            };
        }
        panic!("line never ready")
    }
    fn start(&self, c: Comparison, request: WordRequest, name: &str) -> Build {
        match begin(c, request, self.admission(name)) {
            ControlFlow::Continue(b) => b,
            ControlFlow::Break(r) => panic!("word refused {}", r.error),
        }
    }
    fn close(&self, c: Comparison) {
        let (l, r, _, mut ret) = c.into_parts();
        loop {
            match ret.step(work(), &never()) {
                RetirementStep::Pending(x) => ret = x,
                RetirementStep::Ready(_) => break,
                _ => panic!("line retirement"),
            }
        }
        for source in [l, r] {
            let mut ret = source.store.into_retirement();
            loop {
                match ret.step(work(), &never()) {
                    input::RetirementStep::Pending(x) => ret = x,
                    input::RetirementStep::Ready(_) => break,
                    _ => panic!("input retirement"),
                }
            }
        }
        assert_eq!(self.budget.0.borrow().live, Usage::default());
        std::fs::remove_dir(&self.root).unwrap();
    }
}
fn word_retire(mut ret: WordRetirement<Budget>) {
    for _ in 0..1000 {
        match ret.step(work(), &never()) {
            WordRetirementStep::Pending(x) => ret = x,
            WordRetirementStep::Ready(_) => return,
            _ => panic!("word retirement"),
        }
    }
    panic!("word retirement never finished")
}
fn request_for(c: &mut Comparison, row: u64, backend: WordBackend) -> WordRequest {
    let mut q = PageQuery::new(row, 1);
    let mut p = Page::default();
    assert!(matches!(c.page(&mut q, &mut p, &mut work(), &never()), Step::Ready(())));
    let row = p.rows[0].unwrap();
    let mut q = stream::NavigationQuery::new(row.aligned + 1, false);
    let hunk = match c.navigate(&mut q, &mut work(), &never()) {
        Step::Ready(Some(h)) => h,
        _ => panic!("hunk"),
    };
    let (l, r) = c.inputs_mut();
    let left = match l.line_span(&mut LineLookup::new(row.left.unwrap()), &mut work(), &never()) {
        Step::Ready(s) => s,
        _ => panic!("left span"),
    };
    let right = match r.line_span(&mut LineLookup::new(row.right.unwrap()), &mut work(), &never()) {
        Step::Ready(s) => s,
        _ => panic!("right span"),
    };
    WordRequest { row, left, right, hunk, backend }
}
fn finish(mut b: Build, quantum: ReadWork) -> Build {
    for _ in 0..2_000_000 {
        let s = b.step(quantum, &never());
        let ready = s.state() == WordBuildState::Ready;
        b = match s.into_build() {
            ControlFlow::Continue(b) => b,
            ControlFlow::Break(s) => {
                let terminal = match s.into_terminal() {
                    ControlFlow::Continue(t) => t,
                    ControlFlow::Break(_) => panic!("missing terminal"),
                };
                panic!("word terminal {:?}", terminal.error)
            }
        };
        if ready {
            return b;
        }
    }
    panic!("word never ready")
}
fn pages(b: &mut Build, quantum: ReadWork) -> Vec<WordPage> {
    let mut output = Vec::new();
    let mut continuation = None;
    for _ in 0..100 {
        let mut q = WordPageQuery::new(b.request().backend, continuation);
        let mut page = WordPage::empty(b.request().backend);
        let unchanged = page.clone();
        let mut ready = false;
        for _ in 0..1_000_000 {
            let mut budget = quantum;
            match b.page(&mut q, &mut page, &mut budget, &never()) {
                Step::Pending => assert_eq!(page, unchanged),
                Step::Ready(()) => {
                    ready = true;
                    break;
                }
                _ => panic!("page terminal"),
            }
        }
        assert!(ready, "page must terminate");
        assert!(page.len <= 64);
        continuation = page.continuation.clone();
        let complete = page.coverage.complete;
        output.push(page);
        if complete {
            return output;
        }
    }
    panic!("continuations never complete")
}

#[test]
fn actual_similar_small_oracle_and_absolute_eol_ranges() {
    let f = Fixture::new();
    let a = b"prefix\nred fox jumps\r\n";
    let b = b"prefix\nblue fox runs\n";
    let mut c = f.comparison(&[text(a)], &[text(b)]);
    let req = request_for(&mut c, 1, WordBackend::Similar32);
    let starts = [req.left.start, req.right.start];
    let mut build = f.start(c, req, "word");
    let mut audit_required = false;
    for _ in 0..1000 {
        let step = build.step(work(), &never());
        audit_required = step.pending_reason() == Some(WordPendingReason::Similar32AuditRequired);
        let ready = step.state() == WordBuildState::Ready;
        build = match step.into_build() {
            ControlFlow::Continue(b) => b,
            ControlFlow::Break(_) => panic!("legacy terminal"),
        };
        if ready || audit_required {
            break;
        }
    }
    if !SIMILAR32_AUDIT_ACCEPTED {
        assert!(audit_required, "unqualified legacy entry must expose a typed audit gate");
        let mut out = WordPage::empty(WordBackend::Similar32);
        let unchanged = out.clone();
        let mut q = WordPageQuery::new(WordBackend::Similar32, None);
        assert!(matches!(build.page(&mut q, &mut out, &mut work(), &never()), Step::Pending));
        assert_eq!(out, unchanged);
        let t = build.into_parts();
        word_retire(t.retirement);
        f.close(t.comparison);
        // The actual Similar oracle below must not execute before ROOT qualifies
        // the heap and physical stack. This branch verifies only the audit gate.
        return;
    }
    let got = pages(&mut build, work());
    assert!(got.last().unwrap().coverage.complete);
    let diff = similar::TextDiff::configure().diff_words(&a[starts[0] as usize..], &b[starts[1] as usize..]);
    let mut expected = vec![];
    let mut at = starts;
    for ch in diff.iter_all_changes() {
        let mut end = at;
        let n = ch.value().len() as u64;
        match ch.tag() {
            similar::ChangeTag::Equal => {
                end[0] += n;
                end[1] += n;
            }
            similar::ChangeTag::Delete => end[0] += n,
            similar::ChangeTag::Insert => end[1] += n,
        }
        if ch.tag() != similar::ChangeTag::Equal {
            if let Some(last) = expected.last_mut() {
                let last: &mut WordChange = last;
                if last.left.end == at[0] && last.right.end == at[1] {
                    last.left.end = end[0];
                    last.right.end = end[1];
                } else {
                    expected.push(WordChange { left: at[0]..end[0], right: at[1]..end[1] });
                }
            } else {
                expected.push(WordChange { left: at[0]..end[0], right: at[1]..end[1] });
            }
        }
        at = end;
    }
    let actual: Vec<_> = got.iter().flat_map(|p| p.records[..p.len].iter().flatten().cloned()).collect();
    assert_eq!(actual, expected);
    let t = build.into_parts();
    word_retire(t.retirement);
    f.close(t.comparison);
}

#[test]
fn external_exact_matches_independent_dp_and_includes_empty_anchors() {
    let cases: [(&[u8], &[u8]); 4] =
        [(b"a b c\n", b"a z c\n"), (b"a a b", b"a b a"), (b" a", b"a "), (b"a\r\n", b"a\n")];
    for (a, b) in cases {
        let f = Fixture::new();
        let mut c = f.comparison(&[text(a)], &[text(b)]);
        let req = request_for(&mut c, 0, WordBackend::Similar32TokensExternalExact);
        let mut build = finish(f.start(c, req, "word"), ReadWork::new(1, 1));
        let got = pages(&mut build, ReadWork::new(37, 2));
        use similar::DiffableStr;
        let left = a.tokenize_words();
        let right = b.tokenize_words();
        let mut dp = vec![vec![0usize; right.len() + 1]; left.len() + 1];
        for i in 0..left.len() {
            for j in 0..right.len() {
                dp[i + 1][j + 1] = if left[i] == right[j] { dp[i][j] + 1 } else { dp[i + 1][j].max(dp[i][j + 1]) };
            }
        }
        let changes: Vec<_> = got.iter().flat_map(|p| p.records[..p.len].iter().flatten()).collect();
        let edited = |side: usize| -> usize {
            changes
                .iter()
                .map(|c| {
                    let r = if side == 0 { &c.left } else { &c.right };
                    let source = if side == 0 { a } else { b };
                    source[r.start as usize..r.end as usize].tokenize_words().len()
                })
                .sum()
        };
        assert_eq!(edited(0) + edited(1), left.len() + right.len() - 2 * dp[left.len()][right.len()]);
        assert_eq!(got.last().unwrap().coverage.left.end, a.len() as u64);
        assert_eq!(got.last().unwrap().coverage.right.end, b.len() as u64);
        let t = build.into_parts();
        word_retire(t.retirement);
        f.close(t.comparison);
    }
}

#[test]
fn boundaries_across_windows_invalid_utf8_and_giant_one_token() {
    use similar::DiffableStr;
    let mut bytes = vec![b'x'; 255];
    bytes.extend_from_slice("\u{2003}".as_bytes());
    bytes.extend_from_slice(b"\xe2\x98a \xf0\x9d\x9c\xff\n");
    let f = Fixture::new();
    let mut source = f.source("scanner", &[text(&bytes)], 1);
    let mut scanner = super::tokens::Scanner::new(0..bytes.len() as u64);
    let mut actual = vec![];
    for _ in 0..100_000 {
        match scanner.next(&mut source, &mut ReadWork::new(1, 1), &never()) {
            Step::Pending => {}
            Step::Ready(Some(t)) => actual.push(&bytes[t.start as usize..t.end as usize]),
            Step::Ready(None) => break,
            _ => panic!("scanner"),
        }
    }
    assert_eq!(actual, bytes.as_slice().tokenize_words());
    let giant = vec![b'g'; 16_385];
    let mut source2 = f.source("giant", &[text(&giant)], 2);
    let mut scanner = super::tokens::Scanner::new(0..giant.len() as u64);
    let token = match scanner.next(&mut source2, &mut work(), &never()) {
        Step::Ready(Some(t)) => t,
        _ => panic!("giant token"),
    };
    assert_eq!((token.start, token.end), (0, giant.len() as u64));
    assert!(matches!(scanner.next(&mut source2, &mut work(), &never()), Step::Ready(None)));
    for s in [source, source2] {
        let mut r = s.store.into_retirement();
        loop {
            match r.step(work(), &never()) {
                input::RetirementStep::Pending(n) => r = n,
                input::RetirementStep::Ready(_) => break,
                _ => panic!("retire"),
            }
        }
    }
    assert_eq!(f.budget.0.borrow().live, Usage::default());
    std::fs::remove_dir(f.root).unwrap();
}

#[test]
fn more_than_64_changes_page_with_one_byte_quanta() {
    let mut a = Vec::new();
    let mut b = Vec::new();
    for i in 0..80 {
        a.extend_from_slice(format!("a{i} ").as_bytes());
        b.extend_from_slice(format!("b{i} ").as_bytes());
    }
    let f = Fixture::new();
    let mut c = f.comparison(&[text(&a)], &[text(&b)]);
    let req = request_for(&mut c, 0, WordBackend::Similar32TokensExternalExact);
    let mut build = finish(f.start(c, req, "word"), ReadWork::new(257, 3));
    let got = pages(&mut build, ReadWork::new(1, 1));
    assert!(got.len() > 1);
    assert!(!got[0].coverage.complete);
    assert_eq!(got.iter().map(|p| p.len).sum::<usize>(), 80);
    assert_eq!(got.iter().map(|p| p.len).collect::<Vec<_>>(), [64, 16]);
    // Each word token is unique to its side and only the 80 single-space
    // tokens can match. Independent DP therefore proves LCS=80 and exactly
    // 160 token edits, represented by these 80 ordered replacement ranges.
    use similar::DiffableStr;
    let left = a.as_slice().tokenize_words();
    let right = b.as_slice().tokenize_words();
    assert_eq!([left.len(), right.len()], [160, 160]);
    let mut dp = vec![0usize; right.len() + 1];
    for old in &left {
        let mut diagonal = 0;
        for (j, new) in right.iter().enumerate() {
            let previous = dp[j + 1];
            dp[j + 1] = if old == new { diagonal + 1 } else { dp[j + 1].max(dp[j]) };
            diagonal = previous;
        }
    }
    assert_eq!(dp[right.len()], 80);
    let actual: Vec<_> = got.iter().flat_map(|p| p.records[..p.len].iter().flatten().cloned()).collect();
    assert_eq!(actual.len() * 2, left.len() + right.len() - 2 * dp[right.len()]);
    let mut at = 0u64;
    let expected: Vec<_> = (0..80)
        .map(|i| {
            let end = at + format!("a{i}").len() as u64;
            let edit = WordChange { left: at..end, right: at..end };
            at = end + 1;
            edit
        })
        .collect();
    assert_eq!(actual, expected, "paging must not split a replacement at an empty anchor");
    assert_eq!(got[0].coverage.left.end, expected[64].left.start);
    assert_eq!(got[0].coverage.right.end, expected[64].right.start);
    assert_eq!(got[0].coverage.left.start, 0);
    assert_eq!(got[0].coverage.right.start, 0);
    let last = got.last().unwrap();
    assert!(last.coverage.complete);
    assert!(last.continuation.is_none());
    assert_eq!(last.coverage.left.end, a.len() as u64);
    assert_eq!(last.coverage.right.end, b.len() as u64);
    for pair in got.windows(2) {
        assert_eq!(pair[0].coverage.left.end, pair[1].coverage.left.start);
        assert_eq!(pair[0].coverage.right.end, pair[1].coverage.right.start);
    }
    let t = build.into_parts();
    word_retire(t.retirement);
    f.close(t.comparison);
}

#[test]
fn opaque_codec_raw_length_payload_and_preserved_alias_equality() {
    let cases = [
        (7, b"\xff".as_slice(), 7, b"\xfe".as_slice()),
        (7, b"\xff".as_slice(), 8, b"\xff".as_slice()),
        (7, b"\xff".as_slice(), 7, b"\xff\xff".as_slice()),
    ];
    for (ac, ar, bc, br) in cases {
        let f = Fixture::new();
        let left =
            [text(b"x "), Atom { logical: b"\xff", raw: ar, kind: CaptureKind::Opaque, encoding: ac }, text(b" z")];
        let right =
            [text(b"x "), Atom { logical: b"\xff", raw: br, kind: CaptureKind::Opaque, encoding: bc }, text(b" z")];
        let mut c = f.comparison(&left, &right);
        let req = request_for(&mut c, 0, WordBackend::Similar32TokensExternalExact);
        let mut b = finish(f.start(c, req, "word"), ReadWork::new(71, 3));
        let got = pages(&mut b, work());
        assert_eq!(got[0].records[0], Some(WordChange { left: 2..3, right: 2..3 }));
        let t = b.into_parts();
        word_retire(t.retirement);
        f.close(t.comparison);
    }
    let f = Fixture::new();
    let left = [Atom { logical: b"alias", raw: b"A", kind: CaptureKind::Preserved, encoding: 1 }, text(b" old")];
    let right = [Atom { logical: b"alias", raw: b"B", kind: CaptureKind::Preserved, encoding: 2 }, text(b" new")];
    let mut c = f.comparison(&left, &right);
    let req = request_for(&mut c, 0, WordBackend::Similar32TokensExternalExact);
    let mut b = finish(f.start(c, req, "word"), work());
    let got = pages(&mut b, work());
    assert_eq!(got[0].records[0], Some(WordChange { left: 6..9, right: 6..9 }));
    let t = b.into_parts();
    word_retire(t.retirement);
    f.close(t.comparison);
}

#[test]
fn foreign_comparison_query_rejected_without_change_and_bad_spans_fail() {
    let f = Fixture::new();
    let mut c = f.comparison(&[text(b"old\n")], &[text(b"new\n")]);
    let req = request_for(&mut c, 0, WordBackend::Similar32TokensExternalExact);
    let mut q = PageQuery::new(0, 1);
    let mut p = Page::default();
    assert!(matches!(c.page(&mut q, &mut p, &mut ReadWork::new(0, 0), &never()), Step::Pending));
    let live = f.budget.0.borrow().live;
    let rejected = match begin(c, req, f.admission("word")) {
        ControlFlow::Break(r) => r,
        ControlFlow::Continue(_) => panic!("foreign comparison query accepted"),
    };
    assert!(matches!(rejected.error, InputError::QueryChanged));
    assert_eq!(live, f.budget.0.borrow().live);
    assert!(!f.root.join("word").exists());
    let mut c = rejected.comparison;
    assert!(matches!(c.page(&mut q, &mut p, &mut work(), &never()), Step::Ready(())));
    assert_eq!(p.len, 1);
    let mut req = rejected.request;
    req.left.content_end -= 1;
    let b = f.start(c, req, "bad-span");
    let terminal = terminal(b, &never());
    assert!(matches!(terminal.error, Some(InputError::Range)));
    word_retire(terminal.retirement);
    f.close(terminal.comparison);
}

#[test]
fn foreign_payload_query_failure_does_not_abandon_that_owner() {
    let f = Fixture::new();
    let mut c = f.comparison(&[text(b"old word\n")], &[text(b"new word\n")]);
    let req = request_for(&mut c, 0, WordBackend::Similar32TokensExternalExact);
    let stats = c.inputs_mut().0.stats.clone();
    let mut foreign = [0u8; 2];
    assert!(matches!(
        c.inputs_mut().0.read_at(PayloadSpace::Logical, 0, &mut foreign, &mut ReadWork::new(0, 0), &never()),
        Step::Pending
    ));
    let live = f.budget.0.borrow().live;
    let reads = stats.borrow().reads;
    let rejected = match begin(c, req, f.admission("word")) {
        ControlFlow::Break(r) => r,
        ControlFlow::Continue(_) => panic!("foreign payload read accepted"),
    };
    assert!(matches!(rejected.error, InputError::QueryChanged));
    assert_eq!(stats.borrow().resets, 0);
    assert_eq!(stats.borrow().reads, reads);
    assert_eq!(f.budget.0.borrow().live, live);
    assert_eq!(foreign, [0; 2]);
    assert!(!f.root.join("word").exists());
    let mut c = rejected.comparison;
    assert!(matches!(
        c.inputs_mut().0.read_at(PayloadSpace::Logical, 0, &mut foreign, &mut work(), &never()),
        Step::Ready(2)
    ));
    assert_eq!(&foreign, b"ol");
    f.close(c);
}

fn terminal(mut b: Build, cancel: &AtomicBool) -> WordTerminal<Source, Source, Budget, Budget> {
    for _ in 0..10_000 {
        let step = b.step(work(), cancel);
        if step.state() == WordBuildState::Terminal {
            return match step.into_terminal() {
                ControlFlow::Continue(t) => t,
                ControlFlow::Break(_) => panic!("terminal extraction"),
            };
        }
        b = match step.into_build() {
            ControlFlow::Continue(b) => b,
            ControlFlow::Break(_) => panic!("missing build"),
        };
    }
    panic!("terminal never returned")
}

#[test]
fn read_refusal_and_identity_change_return_original_owners() {
    for identity in [false, true] {
        let f = Fixture::new();
        let mut c = f.comparison(&[text(b"old\n")], &[text(b"new\n")]);
        let req = request_for(&mut c, 0, WordBackend::Similar32TokensExternalExact);
        let stats = c.inputs_mut().0.stats.clone();
        let b = f.start(c, req, "word");
        if identity {
            stats.borrow_mut().change_identity = true;
        } else {
            stats.borrow_mut().refuse = true;
        }
        let mut t = terminal(b, &never());
        if identity {
            assert!(matches!(t.error, Some(InputError::SourceChanged)));
        } else {
            match t.error {
                Some(InputError::Io(e)) => assert_eq!(e.to_string(), "word refusal payload"),
                _ => panic!("original I/O payload lost"),
            }
        }
        stats.borrow_mut().change_identity = false;
        stats.borrow_mut().refuse = false;
        assert!(Rc::ptr_eq(&stats, &t.comparison.inputs_mut().0.stats));
        word_retire(t.retirement);
        f.close(t.comparison);
    }
}

#[test]
fn cancel_pending_read_then_reuse_comparison_for_another_row() {
    let f = Fixture::new();
    let mut c = f.comparison(&[text(b"old one\nold two\n")], &[text(b"new one\nnew two\n")]);
    let req = request_for(&mut c, 0, WordBackend::Similar32TokensExternalExact);
    let stats = c.inputs_mut().0.stats.clone();
    stats.borrow_mut().pending = false;
    let mut b = f.start(c, req, "word-one");
    let mut observed = false;
    for _ in 0..100_000 {
        let s = b.step(ReadWork::new(1, 1), &never());
        b = match s.into_build() {
            ControlFlow::Continue(b) => b,
            ControlFlow::Break(_) => panic!("unexpected terminal"),
        };
        if stats.borrow().pending {
            observed = true;
            break;
        }
    }
    assert!(observed, "must cancel an actual pending word payload read");
    let cancel = AtomicBool::new(true);
    let t = terminal(b, &cancel);
    assert!(t.error.is_none());
    assert_eq!(stats.borrow().resets, 1);
    word_retire(t.retirement);
    let mut c = t.comparison;
    let req = request_for(&mut c, 1, WordBackend::Similar32TokensExternalExact);
    let mut b = finish(f.start(c, req, "word-two"), ReadWork::new(53, 2));
    let got = pages(&mut b, ReadWork::new(1, 1));
    assert!(got.last().unwrap().coverage.complete);
    assert_eq!(got[0].coverage.left.start, 8);
    let t = b.into_parts();
    word_retire(t.retirement);
    f.close(t.comparison);
}

#[test]
fn page_query_and_continuation_cannot_cross_word_owners() {
    let f = Fixture::new();
    let mut c = f.comparison(&[text(b"old one\nold two\n")], &[text(b"new one\nnew two\n")]);
    let req = request_for(&mut c, 0, WordBackend::Similar32TokensExternalExact);
    let mut b = finish(f.start(c, req, "one"), work());
    let mut q = WordPageQuery::new(WordBackend::Similar32TokensExternalExact, None);
    let mut out = WordPage::empty(WordBackend::Similar32TokensExternalExact);
    let old = out.clone();
    assert!(matches!(b.page(&mut q, &mut out, &mut ReadWork::new(0, 0), &never()), Step::Pending));
    assert_eq!(out, old);
    let t = b.into_parts();
    word_retire(t.retirement);
    let mut c = t.comparison;
    let req = request_for(&mut c, 1, WordBackend::Similar32TokensExternalExact);
    let mut b = finish(f.start(c, req, "two"), work());
    assert!(matches!(b.page(&mut q, &mut out, &mut work(), &never()), Step::Failed(InputError::QueryChanged)));
    assert_eq!(out, old);
    let got = pages(&mut b, work());
    assert!(got.last().unwrap().coverage.complete);
    let t = b.into_parts();
    word_retire(t.retirement);
    f.close(t.comparison);
}

#[test]
fn abandoned_page_query_cannot_steal_a_new_query_in_same_invocation() {
    let f = Fixture::new();
    let mut c = f.comparison(&[text(b"old")], &[text(b"new")]);
    let req = request_for(&mut c, 0, WordBackend::Similar32TokensExternalExact);
    let mut b = finish(f.start(c, req, "word"), work());
    let mut old = WordPageQuery::new(WordBackend::Similar32TokensExternalExact, None);
    let mut current = WordPageQuery::new(WordBackend::Similar32TokensExternalExact, None);
    let mut out = WordPage::empty(WordBackend::Similar32TokensExternalExact);
    let sentinel = out.clone();
    assert!(matches!(b.page(&mut old, &mut out, &mut ReadWork::new(0, 0), &never()), Step::Pending));
    assert!(matches!(b.page(&mut current, &mut out, &mut work(), &never()), Step::Failed(InputError::QueryChanged)));
    assert_eq!(out, sentinel);
    b.abandon_page();
    assert!(matches!(b.page(&mut current, &mut out, &mut ReadWork::new(0, 0), &never()), Step::Pending));
    assert!(matches!(b.page(&mut old, &mut out, &mut work(), &never()), Step::Failed(InputError::QueryChanged)));
    assert_eq!(out, sentinel);
    assert!(matches!(b.page(&mut current, &mut out, &mut work(), &never()), Step::Ready(())));
    assert!(out.coverage.complete);
    let t = b.into_parts();
    word_retire(t.retirement);
    f.close(t.comparison);
}

#[test]
fn aggregate_refusal_retains_exact_comparison_request_and_paths() {
    let f = Fixture::new();
    let mut c = f.comparison(&[text(b"old")], &[text(b"new")]);
    let req = request_for(&mut c, 0, WordBackend::Similar32TokensExternalExact);
    let live = f.budget.0.borrow().live;
    let id = c.identities();
    f.budget.0.borrow_mut().refuse = true;
    let r = match begin(c, req, f.admission("word")) {
        ControlFlow::Break(r) => r,
        ControlFlow::Continue(_) => panic!("refusal ignored"),
    };
    assert!(matches!(r.error, InputError::Admission(_)));
    assert_eq!(r.comparison.identities(), id);
    assert_eq!(r.admission.paths.directory, f.root.join("word"));
    assert_eq!(live, f.budget.0.borrow().live);
    f.budget.0.borrow_mut().refuse = false;
    f.close(r.comparison);
}

#[test]
fn giant_word_external_backend_retains_one_descriptor_and_full_coverage() {
    let f = Fixture::new();
    let a = vec![b'a'; 8193];
    let b = vec![b'b'; 8193];
    let mut c = f.comparison(&[text(&a)], &[text(&b)]);
    let req = request_for(&mut c, 0, WordBackend::Similar32TokensExternalExact);
    let mut build = finish(f.start(c, req, "word"), ReadWork::new(257, 3));
    assert_eq!(build.token_counts(), [1, 1]);
    assert_eq!(
        build.usage().disk,
        input::INPUT_WINDOW as u64 * 5,
        "giant tokens must not leave byte-count-sized sparse descriptor gaps"
    );
    let got = pages(&mut build, ReadWork::new(1, 1));
    assert_eq!(got.len(), 1);
    assert!(got[0].coverage.complete);
    assert_eq!(got[0].records[0], Some(WordChange { left: 0..8193, right: 0..8193 }));
    let t = build.into_parts();
    word_retire(t.retirement);
    f.close(t.comparison);
}

#[test]
fn typed_opaque_cannot_equal_literal_marker_or_a_short_raw_prefix() {
    let f = Fixture::new();
    let mut raw = vec![b'r'; 513];
    raw[512] = b'z';
    let left = [Atom { logical: b"\xff", raw: &raw, kind: CaptureKind::Opaque, encoding: 7 }];
    let right = [text(b"\xff")];
    let mut c = f.comparison(&left, &right);
    let req = request_for(&mut c, 0, WordBackend::Similar32TokensExternalExact);
    let mut b = finish(f.start(c, req, "word"), work());
    let got = pages(&mut b, work());
    assert_eq!(got[0].len, 1);
    let t = b.into_parts();
    word_retire(t.retirement);
    f.close(t.comparison);
    let f = Fixture::new();
    let other = vec![b'r'; 513];
    let right = [Atom { logical: b"\xff", raw: &other, kind: CaptureKind::Opaque, encoding: 7 }];
    let mut c = f.comparison(&left, &right);
    let req = request_for(&mut c, 0, WordBackend::Similar32TokensExternalExact);
    let mut b = finish(f.start(c, req, "word"), ReadWork::new(1, 1));
    let got = pages(&mut b, work());
    assert_eq!(got[0].len, 1);
    let t = b.into_parts();
    word_retire(t.retirement);
    f.close(t.comparison);
}

#[test]
fn retirement_failure_retains_owner_and_charge_until_retry() {
    let f = Fixture::new();
    let mut c = f.comparison(&[text(b"old")], &[text(b"new")]);
    let req = request_for(&mut c, 0, WordBackend::Similar32TokensExternalExact);
    let b = finish(f.start(c, req, "word"), work());
    let t = b.into_parts();
    let foreign = f.root.join("word").join("foreign-child");
    std::fs::write(&foreign, b"keep").unwrap();
    let live = f.budget.0.borrow().live;
    let ret = match t.retirement.step(work(), &never()) {
        WordRetirementStep::Failed { retirement, error: InputError::Io(_) } => retirement,
        _ => panic!("nonempty directory must fail cleanup"),
    };
    assert_eq!(live, f.budget.0.borrow().live);
    assert_eq!(std::fs::read(&foreign).unwrap(), b"keep");
    // This exact fixture owns the sentinel. No recursive cleanup or foreign file deletion.
    std::fs::remove_file(foreign).unwrap();
    word_retire(ret);
    f.close(t.comparison);
}

#[test]
fn foreign_payload_reads_on_either_side_reject_without_admission_or_source_work() {
    for right in [false, true] {
        for (at, len) in [(0, 9), (0, 2), (1, 8)] {
            let f = Fixture::new();
            let mut c = f.comparison(&[text(b"old word\n")], &[text(b"new word\n")]);
            let req = request_for(&mut c, 0, WordBackend::Similar32TokensExternalExact);
            let (left, other) = c.inputs_mut();
            let stats = [left.stats.clone(), other.stats.clone()];
            assert!(!left.has_pending_payload_read());
            assert!(!other.has_pending_payload_read());
            let source = if right { other } else { left };
            let mut foreign = [0xa5; 9];
            assert!(matches!(
                source.read_at(PayloadSpace::Logical, at, &mut foreign[..len], &mut ReadWork::new(1, 0), &never()),
                Step::Pending
            ));
            assert!(source.has_pending_payload_read());
            let reads = stats.each_ref().map(|s| s.borrow().reads);
            let resets = stats.each_ref().map(|s| s.borrow().resets);
            let live = f.budget.0.borrow().live;
            let peak = f.budget.0.borrow().peak;
            let rejected = match begin(c, req, f.admission("foreign-word")) {
                ControlFlow::Break(r) => r,
                ControlFlow::Continue(_) => panic!("foreign payload read accepted on side {right} at {at}/{len}"),
            };
            assert!(matches!(rejected.error, InputError::QueryChanged));
            assert_eq!(stats.each_ref().map(|s| s.borrow().reads), reads);
            assert_eq!(stats.each_ref().map(|s| s.borrow().resets), resets);
            assert_eq!(f.budget.0.borrow().live, live);
            assert_eq!(f.budget.0.borrow().peak, peak);
            assert_eq!(foreign, [0xa5; 9]);
            assert!(!f.root.join("foreign-word").exists());
            assert_eq!(rejected.admission.paths.directory, f.root.join("foreign-word"));
            assert!(Rc::ptr_eq(&rejected.admission.aggregate.0, &f.budget.0));
            let mut c = rejected.comparison;
            let (left, other) = c.inputs_mut();
            assert!(Rc::ptr_eq(&left.stats, &stats[0]));
            assert!(Rc::ptr_eq(&other.stats, &stats[1]));
            let source = if right { other } else { left };
            assert!(source.has_pending_payload_read());
            assert!(matches!(
                source.read_at(PayloadSpace::Logical, at, &mut foreign[..len], &mut work(), &never()),
                Step::Ready(n) if n == len
            ));
            let expected = if right { b"new word\n" } else { b"old word\n" };
            assert_eq!(&foreign[..len], &expected[at as usize..at as usize + len]);
            assert_eq!(&foreign[len..], &[0xa5; 9][len..]);
            assert!(!source.has_pending_payload_read());
            f.close(c);
        }
    }
}

#[test]
fn matching_foreign_payload_query_is_refused_before_word_ownership() {
    let f = Fixture::new();
    let mut c = f.comparison(&[text(b"old word\n")], &[text(b"new word\n")]);
    let req = request_for(&mut c, 0, WordBackend::Similar32TokensExternalExact);
    let stats = c.inputs_mut().0.stats.clone();
    let reads_before = stats.borrow().reads;
    let mut foreign = [0xa5; 9];
    assert!(matches!(
        c.inputs_mut().0.read_at(PayloadSpace::Logical, 0, &mut foreign, &mut ReadWork::new(0, 0), &never()),
        Step::Pending
    ));
    let live = f.budget.0.borrow().live;
    let rejected = match begin(c, req, f.admission("matched-foreign-word")) {
        ControlFlow::Break(rejected) => rejected,
        ControlFlow::Continue(_) => panic!("matching foreign payload read must be refused before word ownership"),
    };
    assert!(matches!(rejected.error, InputError::QueryChanged));
    assert_eq!(stats.borrow().resets, 0);
    assert_eq!(stats.borrow().reads, reads_before + 1);
    assert_eq!(f.budget.0.borrow().live, live);
    assert_eq!(foreign, [0xa5; 9]);
    assert!(!f.root.join("matched-foreign-word").exists());
    let mut c = rejected.comparison;
    assert!(matches!(
        c.inputs_mut().0.read_at(PayloadSpace::Logical, 0, &mut foreign, &mut work(), &never()),
        Step::Ready(9)
    ));
    assert_eq!(&foreign, b"old word\n");
    f.close(c);
}
