use super::tokens::{Equality, Scanner, Token};
use super::{Coverage, WORD_PAGE_RECORDS, WordChange};
use crate::{
    Kind,
    sequence::{Event, Layout, Sequence, SequencePair},
    store::{self, AlignmentAdmission, AlignmentPaths, AlignmentRetirement, Records, attempt, gate},
    stream::{AlignedRow, DiskComparison, DiskHunk, NavigationQuery, Page, PageQuery},
};
use kwil_core_text::compare_input::{
    Admission, AdmissionError, AtomLookup, AtomSpan, INPUT_WINDOW, InputError, InputIdentity, InputSource, InputStore,
    LineLookup, LineSpan, PayloadSpace, Rank, RankLookup, ReadWork, Step, Usage,
};
use std::{
    mem::size_of,
    ops::ControlFlow,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

/// Admission requires both sources to have no pending payload read. Observation
/// must use cached state without I/O, mutation or allocation. After this idle
/// claim, the owned comparison excludes new foreign reads for the invocation.
/// Only the word worker's pending payload may be abandoned. Implementors must
/// reset in bounded time without reading, closing, allocating, or changing IDs.
/// There is deliberately no default implementation for test/source wrappers.
pub trait WordInput: InputSource {
    fn has_pending_payload_read(&self) -> bool;
    fn abandon_word_read(&mut self);
}
impl<A: Admission> WordInput for InputStore<A> {
    fn has_pending_payload_read(&self) -> bool {
        InputStore::has_pending_payload_read(self)
    }
    fn abandon_word_read(&mut self) {
        self.abandon_payload_read();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WordBackend {
    Similar32,
    Similar32TokensExternalExact,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WordPendingReason {
    WorkExhausted,
    Similar32AuditRequired,
}

/// ROOT must replace this gate only after the pinned allocation and compiled
/// worker-stack evidence in similar_audit.md is accepted. It is not a budget.
pub const SIMILAR32_AUDIT_ACCEPTED: bool = false;

/// Full line spans include EOL. No viewport prefix can certify a complete row.
#[derive(Clone, Debug)]
pub struct WordRequest {
    pub row: AlignedRow,
    pub left: LineSpan,
    pub right: LineSpan,
    pub hunk: DiskHunk,
    pub backend: WordBackend,
}

/// One aggregate reservation precedes every word-state/file access. Records
/// grows charged disk blocks before access, within a checked linear ceiling.
pub struct WordBank<A: Admission> {
    aggregate: A,
    cap: Usage,
    charged: Usage,
    live: Usage,
}
impl<A: Admission> Admission for WordBank<A> {
    fn reserve(&mut self, n: Usage) -> Result<(), AdmissionError> {
        let next = Usage {
            resident: self.live.resident.checked_add(n.resident).ok_or(AdmissionError)?,
            disk: self.live.disk.checked_add(n.disk).ok_or(AdmissionError)?,
            handles: self.live.handles.checked_add(n.handles).ok_or(AdmissionError)?,
        };
        if next.resident > self.cap.resident || next.disk > self.cap.disk || next.handles > self.cap.handles {
            return Err(AdmissionError);
        }
        if next.disk > self.charged.disk {
            let additional = next.disk - self.charged.disk;
            self.aggregate.reserve(Usage { disk: additional, ..Usage::default() })?;
            self.charged.disk = next.disk;
        }
        self.live = next;
        Ok(())
    }
    fn release(&mut self, n: Usage) {
        self.live.resident -= n.resident;
        self.live.disk -= n.disk;
        self.live.handles -= n.handles;
    }
}

#[must_use]
pub struct WordRetirement<A: Admission> {
    inner: AlignmentRetirement<WordBank<A>>,
}
pub enum WordRetirementStep<A: Admission> {
    Pending(WordRetirement<A>),
    Cancelled(WordRetirement<A>),
    Failed { retirement: WordRetirement<A>, error: InputError },
    Ready(AlignmentAdmission<A>),
}
impl<A: Admission> WordRetirement<A> {
    pub fn paths(&self) -> &AlignmentPaths {
        self.inner.paths()
    }
    pub fn step(self, work: ReadWork, cancel: &AtomicBool) -> WordRetirementStep<A> {
        match self.inner.step(work, cancel) {
            store::RetirementStep::Pending(inner) => WordRetirementStep::Pending(Self { inner }),
            store::RetirementStep::Cancelled(inner) => WordRetirementStep::Cancelled(Self { inner }),
            store::RetirementStep::Failed { retirement: inner, error } => {
                WordRetirementStep::Failed { retirement: Self { inner }, error }
            }
            store::RetirementStep::Ready(AlignmentAdmission { mut aggregate, paths }) => {
                aggregate.aggregate.release(aggregate.charged);
                WordRetirementStep::Ready(AlignmentAdmission { aggregate: aggregate.aggregate, paths })
            }
        }
    }
}

pub struct WordRejected<L: WordInput, R: WordInput, C: Admission, A: Admission> {
    pub comparison: DiskComparison<L, R, C>,
    pub request: WordRequest,
    pub admission: AlignmentAdmission<A>,
    pub error: InputError,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WordContinuation {
    nonce: u64,
    comparison: u64,
    event: u64,
    positions: [u64; 2],
    generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WordPage {
    pub records: [Option<WordChange>; WORD_PAGE_RECORDS],
    pub len: usize,
    pub coverage: Coverage,
    pub continuation: Option<WordContinuation>,
    pub backend: WordBackend,
}
impl WordPage {
    pub fn empty(backend: WordBackend) -> Self {
        Self {
            records: std::array::from_fn(|_| None),
            len: 0,
            coverage: Coverage { left: 0..0, right: 0..0, complete: false },
            continuation: None,
            backend,
        }
    }
}

/// Construct once per continuation; it belongs to exactly one invocation.
pub struct WordPageQuery {
    continuation: Option<WordContinuation>,
    owner: Option<(u64, u64)>,
    event: u64,
    positions: [u64; 2],
    page: WordPage,
    publish: usize,
    started: bool,
    current: Option<[u64; 8]>,
    credit: usize,
}
impl WordPageQuery {
    pub fn new(backend: WordBackend, continuation: Option<WordContinuation>) -> Self {
        Self {
            continuation,
            owner: None,
            event: 0,
            positions: [0; 2],
            page: WordPage::empty(backend),
            publish: 0,
            started: false,
            current: None,
            credit: 0,
        }
    }
}

struct Tracked<'a, S: WordInput> {
    source: &'a mut S,
    pending: &'a mut bool,
}
impl<S: WordInput> InputSource for Tracked<'_, S> {
    fn identity(&self) -> InputIdentity {
        self.source.identity()
    }
    fn byte_len(&self) -> u64 {
        self.source.byte_len()
    }
    fn line_count(&self) -> u64 {
        self.source.line_count()
    }
    fn original_len(&self) -> u64 {
        self.source.original_len()
    }
    fn read_at(
        &mut self,
        s: PayloadSpace,
        at: u64,
        out: &mut [u8],
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<usize> {
        let result = self.source.read_at(s, at, out, work, cancel);
        match &result {
            Step::Pending => *self.pending = true,
            Step::Ready(_) => *self.pending = false,
            _ => {}
        }
        result
    }
    fn line_span(&self, q: &mut LineLookup, w: &mut ReadWork, c: &AtomicBool) -> Step<LineSpan> {
        self.source.line_span(q, w, c)
    }
    fn atom_span(&self, q: &mut AtomLookup, w: &mut ReadWork, c: &AtomicBool) -> Step<Option<AtomSpan>> {
        self.source.atom_span(q, w, c)
    }
    fn rank(&self, q: &mut RankLookup, w: &mut ReadWork, c: &AtomicBool) -> Step<Rank> {
        self.source.rank(q, w, c)
    }
}
impl<S: WordInput> WordInput for Tracked<'_, S> {
    fn has_pending_payload_read(&self) -> bool {
        self.source.has_pending_payload_read()
    }
    fn abandon_word_read(&mut self) {
        if *self.pending {
            self.source.abandon_word_read();
            *self.pending = false;
        }
    }
}

struct Pair<'a, L: WordInput, R: WordInput> {
    left: Tracked<'a, L>,
    right: Tracked<'a, R>,
    lengths: [u64; 2],
    right_base: u64,
}
impl<L: WordInput, R: WordInput> SequencePair for Pair<'_, L, R> {
    type Equality = Equality;
    fn lengths(&self) -> [u64; 2] {
        self.lengths
    }
    fn begin_equal(&self, a: u64, b: u64) -> Equality {
        Equality::new(a, b)
    }
    fn equal<A: Admission>(
        &mut self,
        e: &mut Equality,
        records: &mut Records<A>,
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<bool> {
        e.step(&mut self.left, &mut self.right, records, self.right_base, work, cancel)
    }
}

struct Small {
    bytes: [[u8; 4096]; 2],
    filled: [usize; 2],
    events: Vec<[u64; 8]>,
    index: usize,
    credit: u64,
    computed: bool,
}
impl Small {
    fn new() -> Self {
        Self { bytes: [[0; 4096]; 2], filled: [0; 2], events: Vec::new(), index: 0, credit: 0, computed: false }
    }
}

#[derive(Clone, Copy)]
enum Phase {
    Open,
    Row,
    Hunk,
    LeftSpan,
    RightSpan,
    ScanLeft,
    ScanRight,
    Solve,
    SmallRead,
    SmallSolve,
    Ready,
}

#[must_use]
pub struct WordBuild<L: WordInput, R: WordInput, C: Admission, A: Admission> {
    comparison: DiskComparison<L, R, C>,
    request: WordRequest,
    records: Records<WordBank<A>>,
    nonce: u64,
    comparison_nonce: u64,
    identities: [InputIdentity; 2],
    phase: Phase,
    row_query: PageQuery,
    row_page: Page,
    hunk_query: NavigationQuery,
    lookups: [LineLookup; 2],
    scanners: [Scanner; 2],
    pending: [bool; 2],
    lengths: [u64; 2],
    right_base: u64,
    token: Option<Token>,
    sequence: Option<Sequence<Equality>>,
    event: Option<Event>,
    events: u64,
    mapped: [u64; 4],
    map_stage: usize,
    small: Small,
    query: Option<u64>,
    next_query: u64,
    generation: u64,
}

/// Exact owners returned on every terminal path; no Drop performs cleanup.
pub struct WordTerminal<L: WordInput, R: WordInput, C: Admission, A: Admission> {
    pub comparison: DiskComparison<L, R, C>,
    pub request: WordRequest,
    pub retirement: WordRetirement<A>,
    pub error: Option<InputError>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WordBuildState {
    Pending,
    Ready,
    Terminal,
}
pub struct WordBuildStep<L: WordInput, R: WordInput, C: Admission, A: Admission> {
    state: WordBuildState,
    build: Option<WordBuild<L, R, C, A>>,
    terminal: Option<WordTerminal<L, R, C, A>>,
}
impl<L: WordInput, R: WordInput, C: Admission, A: Admission> WordBuildStep<L, R, C, A> {
    pub fn state(&self) -> WordBuildState {
        self.state
    }
    pub fn pending_reason(&self) -> Option<WordPendingReason> {
        if self.state != WordBuildState::Pending {
            return None;
        }
        Some(
            if !SIMILAR32_AUDIT_ACCEPTED && self.build.as_ref().is_some_and(|b| matches!(b.phase, Phase::SmallSolve)) {
                WordPendingReason::Similar32AuditRequired
            } else {
                WordPendingReason::WorkExhausted
            },
        )
    }
    pub fn into_build(mut self) -> ControlFlow<Self, WordBuild<L, R, C, A>> {
        match self.build.take() {
            Some(b) => ControlFlow::Continue(b),
            None => ControlFlow::Break(self),
        }
    }
    pub fn into_terminal(mut self) -> ControlFlow<Self, WordTerminal<L, R, C, A>> {
        match self.terminal.take() {
            Some(t) => ControlFlow::Continue(t),
            None => ControlFlow::Break(self),
        }
    }
}

static NEXT: AtomicU64 = AtomicU64::new(0);
fn round(bytes: u64) -> Option<u64> {
    bytes.checked_add(INPUT_WINDOW as u64 - 1).map(|v| v / (INPUT_WINDOW as u64) * (INPUT_WINDOW as u64))
}

/// Five bank limits, including contiguous token descriptors and filesystem
/// block rounding: [4(S+4),4(S+4),4(S+4),S,2S+2] 64-byte records.
pub fn word_charge<L: WordInput, R: WordInput, C: Admission, A: Admission>(
    paths: &AlignmentPaths,
    request: &WordRequest,
) -> Option<Usage> {
    let n = request.left.end.checked_sub(request.left.start)?;
    let m = request.right.end.checked_sub(request.right.start)?;
    let s = n.checked_add(m)?;
    let scratch = s.checked_add(4)?.checked_mul(4)?;
    let counts = [scratch, scratch, scratch, s, s.checked_mul(2)?.checked_add(2)?];
    let disk = counts
        .into_iter()
        .try_fold(0u64, |total, c| total.checked_add(round(c.checked_mul(store::RECORD)?)?.max(INPUT_WINDOW as u64)))?;
    let resident = paths
        .capacity()?
        .checked_mul(3)?
        .checked_add(size_of::<WordBuild<L, R, C, A>>().checked_mul(4)?)?
        .checked_add(size_of::<WordBuildStep<L, R, C, A>>().checked_mul(3)?)?
        .checked_add(size_of::<WordPageQuery>().checked_mul(3)?)?
        .checked_add(size_of::<WordPage>().checked_mul(3)?)?
        .checked_add(size_of::<WordRetirementStep<A>>().checked_mul(3)?)?
        .checked_add(size_of::<Pair<'_, L, R>>().checked_mul(3)?)?
        .checked_add(crate::sequence::transient_charge::<Equality>()?)?
        .checked_add(if request.backend == WordBackend::Similar32 { SIMILAR_HEAP_ENVELOPE } else { 0 })?;
    Some(Usage { resident, disk, handles: 5 })
}

/// Source audit in similar_audit.md; stack qualification remains a ROOT gate.
pub const SIMILAR_HEAP_ENVELOPE: usize = 16 * 1024 * 1024;

/// Begin either rejects the unchanged owners or continues with the word build.
pub type WordBeginResult<L, R, C, A> = ControlFlow<WordRejected<L, R, C, A>, WordBuild<L, R, C, A>>;

pub fn begin<L: WordInput, R: WordInput, C: Admission, A: Admission>(
    mut comparison: DiskComparison<L, R, C>,
    request: WordRequest,
    mut admission: AlignmentAdmission<A>,
) -> WordBeginResult<L, R, C, A> {
    let reject = |comparison, request, admission, error| {
        ControlFlow::Break(WordRejected { comparison, request, admission, error })
    };
    if comparison.has_active_query() {
        return reject(comparison, request, admission, InputError::QueryChanged);
    }
    let (left, right) = comparison.inputs_mut();
    if left.has_pending_payload_read() || right.has_pending_payload_read() {
        return reject(comparison, request, admission, InputError::QueryChanged);
    }
    let valid = |s: LineSpan| s.start <= s.content_end && s.content_end <= s.end;
    if request.row.kind != Kind::Modify
        || request.row.left.is_none()
        || request.row.right.is_none()
        || !valid(request.left)
        || !valid(request.right)
        || request.hunk.left.start > request.hunk.left.end
        || request.hunk.right.start > request.hunk.right.end
        || !request.hunk.rows.contains(&request.row.aligned)
    {
        return reject(comparison, request, admission, InputError::Range);
    }
    if request.backend == WordBackend::Similar32
        && (request.left.end - request.left.start > 4096
            || request.right.end - request.right.start > 4096
            || request.hunk.left.end - request.hunk.left.start > 64
            || request.hunk.right.end - request.hunk.right.start > 64)
    {
        return reject(comparison, request, admission, InputError::Range);
    }
    let Some(cap) = word_charge::<L, R, C, A>(&admission.paths, &request) else {
        return reject(comparison, request, admission, InputError::Overflow);
    };
    let nonce = match NEXT.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1)) {
        Ok(n) => n,
        Err(_) => return reject(comparison, request, admission, InputError::Overflow),
    };
    let charged = Usage { disk: INPUT_WINDOW as u64 * 5, ..cap };
    if let Err(e) = admission.aggregate.reserve(charged) {
        return reject(comparison, request, admission, e.into());
    }
    let bank = WordBank { aggregate: admission.aggregate, cap, charged, live: Usage::default() };
    let records = match Records::begin(AlignmentAdmission { aggregate: bank, paths: admission.paths }, cap.resident) {
        ControlFlow::Continue(r) => r,
        ControlFlow::Break((AlignmentAdmission { mut aggregate, paths }, error)) => {
            aggregate.aggregate.release(aggregate.charged);
            return reject(comparison, request, AlignmentAdmission { aggregate: aggregate.aggregate, paths }, error);
        }
    };
    let identities = comparison.identities();
    let comparison_nonce = comparison.comparison_nonce();
    let row_query = PageQuery::new(request.row.aligned, 1);
    let hunk_query = NavigationQuery::new(request.row.aligned.saturating_add(1), false);
    let lookups = [LineLookup::new(request.row.left.unwrap()), LineLookup::new(request.row.right.unwrap())];
    let scanners =
        [Scanner::new(request.left.start..request.left.end), Scanner::new(request.right.start..request.right.end)];
    let right_base = 0;
    ControlFlow::Continue(WordBuild {
        comparison,
        request,
        records,
        nonce,
        comparison_nonce,
        identities,
        phase: Phase::Open,
        row_query,
        row_page: Page::default(),
        hunk_query,
        lookups,
        scanners,
        pending: [false; 2],
        lengths: [0; 2],
        right_base,
        token: None,
        sequence: None,
        event: None,
        events: 0,
        mapped: [0; 4],
        map_stage: 0,
        small: Small::new(),
        query: None,
        next_query: 0,
        generation: 0,
    })
}

fn spans_equal(a: &DiskHunk, b: &DiskHunk) -> bool {
    a.left == b.left && a.right == b.right && a.rows == b.rows
}

impl<L: WordInput, R: WordInput, C: Admission, A: Admission> WordBuild<L, R, C, A> {
    pub fn request(&self) -> &WordRequest {
        &self.request
    }
    pub fn token_counts(&self) -> [u64; 2] {
        self.lengths
    }
    pub fn usage(&self) -> Usage {
        self.records.usage()
    }
    fn reset_sources(&mut self) {
        let (left, right) = self.comparison.inputs_mut();
        if self.pending[0] {
            left.abandon_word_read();
            self.pending[0] = false;
        }
        if self.pending[1] {
            right.abandon_word_read();
            self.pending[1] = false;
        }
    }
    pub fn into_parts(mut self) -> WordTerminal<L, R, C, A> {
        self.reset_sources();
        if matches!(self.phase, Phase::Row | Phase::Hunk) {
            self.comparison.abandon_query();
        }
        self.records.abandon();
        WordTerminal {
            comparison: self.comparison,
            request: self.request,
            retirement: WordRetirement { inner: self.records.retirement() },
            error: None,
        }
    }
    pub fn step(mut self, mut work: ReadWork, cancel: &AtomicBool) -> WordBuildStep<L, R, C, A> {
        match self.advance(&mut work, cancel) {
            Step::Pending => WordBuildStep { state: WordBuildState::Pending, build: Some(self), terminal: None },
            Step::Ready(()) => WordBuildStep { state: WordBuildState::Ready, build: Some(self), terminal: None },
            Step::Cancelled => {
                WordBuildStep { state: WordBuildState::Terminal, build: None, terminal: Some(self.into_parts()) }
            }
            Step::Failed(error) => {
                let mut terminal = self.into_parts();
                terminal.error = Some(error);
                WordBuildStep { state: WordBuildState::Terminal, build: None, terminal: Some(terminal) }
            }
        }
    }
    fn identities_valid(&mut self) -> Result<(), InputError> {
        let (left, right) = self.comparison.inputs_mut();
        if [left.identity(), right.identity()] != self.identities { Err(InputError::SourceChanged) } else { Ok(()) }
    }
    fn advance(&mut self, work: &mut ReadWork, cancel: &AtomicBool) -> Step<()> {
        loop {
            gate!(work.debit(0, 0, cancel));
            attempt!(self.identities_valid());
            match self.phase {
                Phase::Open => {
                    gate!(self.records.open(work, cancel));
                    self.phase = Phase::Row;
                }
                Phase::Row => {
                    gate!(self.comparison.page(&mut self.row_query, &mut self.row_page, work, cancel));
                    if self.row_page.len != 1 || self.row_page.rows[0] != Some(self.request.row) {
                        return Step::Failed(InputError::Range);
                    }
                    self.phase = Phase::Hunk;
                }
                Phase::Hunk => {
                    let hunk = gate!(self.comparison.navigate(&mut self.hunk_query, work, cancel));
                    if !hunk.as_ref().is_some_and(|h| spans_equal(h, &self.request.hunk)) {
                        return Step::Failed(InputError::Range);
                    }
                    self.phase = Phase::LeftSpan;
                }
                Phase::LeftSpan | Phase::RightSpan => {
                    let i = usize::from(matches!(self.phase, Phase::RightSpan));
                    let (left, right) = self.comparison.inputs_mut();
                    let actual = if i == 0 {
                        gate!(left.line_span(&mut self.lookups[0], work, cancel))
                    } else {
                        gate!(right.line_span(&mut self.lookups[1], work, cancel))
                    };
                    if actual != if i == 0 { self.request.left } else { self.request.right } {
                        return Step::Failed(InputError::Range);
                    }
                    self.phase = if i == 0 { Phase::RightSpan } else { Phase::ScanLeft };
                }
                Phase::ScanLeft | Phase::ScanRight => {
                    let i = usize::from(matches!(self.phase, Phase::ScanRight));
                    if self.token.is_none() {
                        let (left, right) = self.comparison.inputs_mut();
                        self.token = if i == 0 {
                            gate!(self.scanners[0].next(
                                &mut Tracked { source: left, pending: &mut self.pending[0] },
                                work,
                                cancel
                            ))
                        } else {
                            gate!(self.scanners[1].next(
                                &mut Tracked { source: right, pending: &mut self.pending[1] },
                                work,
                                cancel
                            ))
                        };
                        if self.token.is_none() {
                            if i == 0 {
                                self.right_base = self.lengths[0];
                                self.phase = Phase::ScanRight;
                            } else if self.request.backend == WordBackend::Similar32 {
                                if self.scanners.iter().any(|s| s.opaque) {
                                    return Step::Failed(InputError::SourceContract);
                                }
                                self.phase = Phase::SmallRead;
                            } else {
                                self.sequence = Some(Sequence::new(self.lengths, Layout::default()));
                                self.phase = Phase::Solve;
                            }
                            continue;
                        }
                    }
                    let index = attempt!(
                        self.lengths[i]
                            .checked_add(if i == 0 { 0 } else { self.right_base })
                            .ok_or(InputError::Overflow)
                    );
                    gate!(self.records.write(store::RUNS, index, self.token.unwrap().words(), work, cancel));
                    self.lengths[i] = attempt!(self.lengths[i].checked_add(1).ok_or(InputError::Overflow));
                    self.token = None;
                }
                Phase::Solve => {
                    if self.event.is_none() {
                        let (left, right) = self.comparison.inputs_mut();
                        let [lp, rp] = &mut self.pending;
                        let mut pair = Pair {
                            left: Tracked { source: left, pending: lp },
                            right: Tracked { source: right, pending: rp },
                            lengths: self.lengths,
                            right_base: self.right_base,
                        };
                        self.event =
                            gate!(self.sequence.as_mut().unwrap().next(&mut pair, &mut self.records, work, cancel));
                        if self.event.is_none() {
                            self.phase = Phase::Ready;
                            continue;
                        }
                    }
                    gate!(self.write_event(work, cancel));
                    attempt!(self.sequence.as_mut().unwrap().acknowledge(self.event.take().unwrap()));
                    self.map_stage = 0;
                }
                Phase::SmallRead => {
                    for i in 0..2 {
                        let span = if i == 0 { self.request.left } else { self.request.right };
                        let len = (span.end - span.start) as usize;
                        while self.small.filled[i] < len {
                            let at = self.small.filled[i];
                            let n = (len - at).min(256);
                            let (left, right) = self.comparison.inputs_mut();
                            let out = &mut self.small.bytes[i][at..at + n];
                            let n = if i == 0 {
                                gate!(Tracked { source: left, pending: &mut self.pending[0] }.read_at(
                                    PayloadSpace::Logical,
                                    span.start + at as u64,
                                    out,
                                    work,
                                    cancel
                                ))
                            } else {
                                gate!(Tracked { source: right, pending: &mut self.pending[1] }.read_at(
                                    PayloadSpace::Logical,
                                    span.start + at as u64,
                                    out,
                                    work,
                                    cancel
                                ))
                            };
                            if n == 0 || n > out.len() {
                                return Step::Failed(InputError::SourceContract);
                            }
                            self.small.filled[i] += n;
                        }
                    }
                    self.phase = Phase::SmallSolve;
                }
                Phase::SmallSolve => {
                    if !SIMILAR32_AUDIT_ACCEPTED {
                        return Step::Pending;
                    }
                    if !self.small.computed {
                        // All indivisible visits/steps are acquired before entry.
                        let cost = attempt!(similar_work(self.lengths, self.small.filled));
                        let n = (cost - self.small.credit).min(work.bytes);
                        gate!(work.debit(n, 0, cancel));
                        self.small.credit += n;
                        if self.small.credit < cost {
                            return Step::Pending;
                        }
                        gate!(work.debit(0, 1, cancel));
                        let [left, right] = &self.small.bytes;
                        let diff = similar::TextDiff::configure()
                            .diff_words(&left[..self.small.filled[0]], &right[..self.small.filled[1]]);
                        let mut at = [self.request.left.start, self.request.right.start];
                        // The admitted output overlaps both TextDiffSides, all ops,
                        // Compact/Capture growth and source arrays until diff drops.
                        self.small.events = Vec::with_capacity(self.lengths[0] as usize + self.lengths[1] as usize + 1);
                        for change in diff.iter_all_changes() {
                            let mut end = at;
                            let n = change.value().len() as u64;
                            let equal = match change.tag() {
                                similar::ChangeTag::Equal => {
                                    end[0] += n;
                                    end[1] += n;
                                    true
                                }
                                similar::ChangeTag::Delete => {
                                    end[0] += n;
                                    false
                                }
                                similar::ChangeTag::Insert => {
                                    end[1] += n;
                                    false
                                }
                            };
                            self.small.events.push([at[0], end[0], at[1], end[1], u64::from(equal), 0, 0, 0]);
                            at = end;
                        }
                        if at != [self.request.left.end, self.request.right.end] {
                            return Step::Failed(InputError::SourceContract);
                        }
                        self.small.computed = true;
                    }
                    while self.small.index < self.small.events.len() {
                        gate!(self.records.write(
                            store::HUNKS,
                            self.events,
                            self.small.events[self.small.index],
                            work,
                            cancel
                        ));
                        self.events = attempt!(self.events.checked_add(1).ok_or(InputError::Overflow));
                        self.small.index += 1;
                    }
                    self.small.events = Vec::new();
                    self.phase = Phase::Ready;
                }
                Phase::Ready => return Step::Ready(()),
            }
        }
    }
    fn write_event(&mut self, work: &mut ReadWork, cancel: &AtomicBool) -> Step<()> {
        let event = self.event.unwrap();
        let (rect, equal) = match event {
            Event::Equal(r) => (r, true),
            Event::Changed(r) => (r, false),
        };
        let indices = [rect.a, rect.b, rect.c, rect.d];
        while self.map_stage < 4 {
            let i = self.map_stage;
            let side = usize::from(i >= 2);
            let index = indices[i];
            if index > self.lengths[side] {
                return Step::Failed(InputError::CorruptStore);
            }
            self.mapped[i] = if index == self.lengths[side] {
                if side == 0 { self.request.left.end } else { self.request.right.end }
            } else {
                let at = attempt!(
                    index.checked_add(if side == 0 { 0 } else { self.right_base }).ok_or(InputError::Overflow)
                );
                attempt!(Token::decode(gate!(self.records.read(store::RUNS, at, work, cancel)))).start
            };
            self.map_stage += 1;
        }
        let [a, b, c, d] = self.mapped;
        let words = [a, b, c, d, u64::from(equal), 0, 0, 0];
        gate!(self.records.write(store::HUNKS, self.events, words, work, cancel));
        self.events = attempt!(self.events.checked_add(1).ok_or(InputError::Overflow));
        Step::Ready(())
    }
    /// Abandons only this word owner's disk query. No source/line query touched.
    pub fn abandon_page(&mut self) {
        self.records.abandon();
        self.query = None;
    }

    /// Output remains unchanged for every non-Ready result. Ready certifies this
    /// page's contiguous coverage; only coverage.complete certifies full EOF.
    pub fn page(
        &mut self,
        query: &mut WordPageQuery,
        out: &mut WordPage,
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<()> {
        let result = self.page_inner(query, out, work, cancel);
        if matches!(result, Step::Cancelled | Step::Failed(_)) {
            self.reset_sources();
        }
        result
    }
    fn page_inner(
        &mut self,
        q: &mut WordPageQuery,
        out: &mut WordPage,
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<()> {
        gate!(work.debit(0, 0, cancel));
        attempt!(self.identities_valid());
        if !matches!(self.phase, Phase::Ready) {
            return Step::Pending;
        }
        if q.page.backend != self.request.backend {
            return Step::Failed(InputError::QueryChanged);
        }
        match (self.query, q.owner) {
            (None, None) => {
                if let Some(c) = &q.continuation {
                    if c.nonce != self.nonce
                        || c.comparison != self.comparison_nonce
                        || c.generation != self.generation
                        || c.event > self.events
                    {
                        return Step::Failed(InputError::QueryChanged);
                    }
                } else if self.generation != 0 {
                    return Step::Failed(InputError::QueryChanged);
                }
                self.next_query = attempt!(self.next_query.checked_add(1).ok_or(InputError::Overflow));
                self.query = Some(self.next_query);
                q.owner = Some((self.nonce, self.next_query));
            }
            (Some(g), Some((n, other))) if n == self.nonce && g == other => {}
            _ => return Step::Failed(InputError::QueryChanged),
        }
        if !q.started {
            q.event = q.continuation.as_ref().map_or(0, |c| c.event);
            q.positions =
                q.continuation.as_ref().map_or([self.request.left.start, self.request.right.start], |c| c.positions);
            q.page.coverage.left = q.positions[0]..q.positions[0];
            q.page.coverage.right = q.positions[1]..q.positions[1];
            q.started = true;
        }
        while q.event < self.events {
            if q.current.is_none() {
                q.current = Some(gate!(self.records.read(store::HUNKS, q.event, work, cancel)));
            }
            while q.credit < 256 {
                let n = (256 - q.credit).min(work.bytes.min(256) as usize);
                if n == 0 {
                    return Step::Pending;
                }
                gate!(work.debit(n as u64, 0, cancel));
                q.credit += n;
            }
            let w = q.current.unwrap();
            if w[0] != q.positions[0]
                || w[2] != q.positions[1]
                || w[0] > w[1]
                || w[2] > w[3]
                || w[1] > self.request.left.end
                || w[3] > self.request.right.end
                || w[4] > 1
            {
                return Step::Failed(InputError::CorruptStore);
            }
            let merge = w[4] == 0
                && q.page.len != 0
                && q.page.records[q.page.len - 1]
                    .as_ref()
                    .is_some_and(|last| last.left.end == w[0] && last.right.end == w[2]);
            // Finish the last edit even when it opened slot 64. A replacement
            // may arrive as adjacent deletion/insertion events with an empty
            // opposite-side anchor; splitting there invents a second edit on
            // the next page. Equal coverage may also advance to the next edit.
            // Park that first nonmergeable edit without consuming its event or
            // coordinates. The existing fixed lookahead is already charged.
            if w[4] == 0 && q.page.len == WORD_PAGE_RECORDS && !merge {
                break;
            }
            q.current = None;
            q.credit = 0;
            if w[4] == 0 {
                if merge {
                    let last = q.page.records[q.page.len - 1].as_mut().unwrap();
                    last.left.end = w[1];
                    last.right.end = w[3];
                } else {
                    q.page.records[q.page.len] = Some(WordChange { left: w[0]..w[1], right: w[2]..w[3] });
                    q.page.len += 1;
                }
            }
            q.positions = [w[1], w[3]];
            q.event += 1;
        }
        q.page.coverage.left.end = q.positions[0];
        q.page.coverage.right.end = q.positions[1];
        let complete = q.event == self.events;
        if complete && q.positions != [self.request.left.end, self.request.right.end] {
            return Step::Failed(InputError::CorruptStore);
        }
        q.page.coverage.complete = complete;
        let generation = attempt!(self.generation.checked_add(1).ok_or(InputError::Overflow));
        q.page.continuation = (!complete).then_some(WordContinuation {
            nonce: self.nonce,
            comparison: self.comparison_nonce,
            event: q.event,
            positions: q.positions,
            generation,
        });
        while q.publish < size_of::<WordPage>() {
            let n = (size_of::<WordPage>() - q.publish).min(work.bytes.min(4096) as usize);
            if n == 0 {
                return Step::Pending;
            }
            gate!(work.debit(n as u64, 0, cancel));
            q.publish += n;
        }
        gate!(work.debit(0, 0, cancel));
        *out = q.page.clone();
        self.query = None;
        self.generation = generation;
        Step::Ready(())
    }
}

/// Deliberately conservative finite source bound, including pathological hash
/// buckets, recursive searches and compaction shifts. See the audit derivation.
fn similar_work(lengths: [u64; 2], bytes: [usize; 2]) -> Result<u64, InputError> {
    let s = lengths[0].checked_add(lengths[1]).and_then(|v| v.checked_add(4)).ok_or(InputError::Overflow)?;
    let b =
        (bytes[0] as u64).checked_add(bytes[1] as u64).and_then(|v| v.checked_add(4)).ok_or(InputError::Overflow)?;
    s.checked_mul(s)
        .and_then(|v| v.checked_mul(s))
        .and_then(|v| v.checked_mul(b))
        .and_then(|v| v.checked_mul(256))
        .ok_or(InputError::Overflow)
}
