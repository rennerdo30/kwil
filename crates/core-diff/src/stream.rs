//! Exact external-memory Myers bisect. No product size cap or coarse result.
//!
//! Forward/reverse frontiers, DFS tasks, alignment runs and hunk ranks are
//! fixed-width disk records. Scratch disk is O(N+M), resident state is fixed.
//! Worst-case comparison work remains O((N+M)D), plus streaming line equality.
//! This worker path leaves the existing bounded imara/similar kernel untouched.
use crate::store::{self, AlignmentAdmission, AlignmentRetirement, Records, attempt, gate};
use crate::{Kind, Whitespace};
use kwil_core_text::compare_input::{
    Admission, AtomLookup, AtomSpan, CaptureKind, InputError, InputIdentity, InputSource, LineLookup, LineSpan,
    PayloadSpace, ReadWork, Step, Terminator, Usage,
};
use std::{
    mem::{ManuallyDrop, size_of},
    ops::{ControlFlow, Range},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

// Scalar identity only; no retained comparison/input registry or heap storage.
static NEXT_COMPARISON: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug)]
pub struct StreamOptions {
    pub whitespace: Whitespace,
    pub ignore_line_endings: bool,
}
impl Default for StreamOptions {
    fn default() -> Self {
        Self { whitespace: Whitespace::Exact, ignore_line_endings: false }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quality {
    Pending,
    Complete,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Opening,
    Aligning,
    Complete,
}
#[derive(Clone, Copy, Debug)]
pub struct Progress {
    pub phase: Phase,
    pub committed_left: u64,
    pub committed_right: u64,
    pub committed_rows: u64,
    pub disk_bytes: u64,
    /// Only Complete certifies all differences. A pending prefix is provisional.
    pub quality: Quality,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Symbol {
    Byte(u8),
    Opaque { encoding: u64, length: u64 },
    Original(u8),
}
fn space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | 0x0b | 0x0c)
}

/// One serial payload query per input. Its effective length never changes on
/// Pending, even if a later quantum has fewer bytes available.
struct Cursor {
    lookup: LineLookup,
    span: Option<LineSpan>,
    trimmed: bool,
    at: u64,
    end: u64,
    atom_lookup: Option<AtomLookup>,
    atom: Option<AtomSpan>,
    original: Option<(u64, u64)>,
    buffer: [u8; 256],
    buffer_at: u64,
    buffer_len: usize,
    buffer_space: PayloadSpace,
    query: Option<(PayloadSpace, u64, usize)>,
    in_space: bool,
}
impl Cursor {
    fn new(line: u64) -> Self {
        Self {
            lookup: LineLookup::new(line),
            span: None,
            trimmed: false,
            at: 0,
            end: 0,
            atom_lookup: None,
            atom: None,
            original: None,
            buffer: [0; 256],
            buffer_at: 0,
            buffer_len: 0,
            buffer_space: PayloadSpace::Logical,
            query: None,
            in_space: false,
        }
    }
    fn prepare(
        &mut self,
        source: &mut impl InputSource,
        options: StreamOptions,
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<LineSpan> {
        if self.span.is_none() {
            let span = gate!(source.line_span(&mut self.lookup, work, cancel));
            if span.start > span.content_end || span.content_end > span.end || span.end > source.byte_len() {
                return Step::Failed(InputError::SourceContract);
            }
            self.at = span.start;
            self.end = span.content_end;
            self.span = Some(span);
        }
        if !self.trimmed {
            if options.whitespace == Whitespace::Trailing {
                while self.end > self.at {
                    // Backward windows are stable across quantum boundaries.
                    let start = self.end.saturating_sub(256).max(self.at);
                    let byte =
                        gate!(self.byte(source, PayloadSpace::Logical, self.end - 1, start..self.end, work, cancel));
                    if !space(byte) {
                        break;
                    }
                    self.end -= 1;
                }
            }
            self.trimmed = true;
            self.buffer_len = 0;
        }
        Step::Ready(self.span.unwrap())
    }
    fn byte(
        &mut self,
        source: &mut impl InputSource,
        which: PayloadSpace,
        at: u64,
        window: Range<u64>,
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<u8> {
        let (start, end) = (window.start, window.end);
        loop {
            if self.query.is_none()
                && self.buffer_space == which
                && at >= self.buffer_at
                && at - self.buffer_at < self.buffer_len as u64
            {
                gate!(work.debit(1, 0, cancel));
                return Step::Ready(self.buffer[(at - self.buffer_at) as usize]);
            }
            let (which, start, len) = *self.query.get_or_insert((which, start, (end - start).min(256) as usize));
            if len == 0 {
                return Step::Failed(InputError::SourceContract);
            }
            let n = gate!(source.read_at(which, start, &mut self.buffer[..len], work, cancel));
            if n == 0 || n > len {
                return Step::Failed(InputError::SourceContract);
            }
            self.query = None;
            self.buffer_space = which;
            self.buffer_at = start;
            self.buffer_len = n;
            if at < start || at - start >= n as u64 {
                // A source may legally return short prefixes. Preserve them while
                // advancing the same bounded backwards request toward the target.
                self.query = Some((which, start + n as u64, (end - start - n as u64).min(256) as usize));
                continue;
            }
            gate!(work.debit(1, 0, cancel));
            return Step::Ready(self.buffer[(at - start) as usize]);
        }
    }
    fn next(
        &mut self,
        source: &mut impl InputSource,
        options: StreamOptions,
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<Option<Symbol>> {
        loop {
            gate!(work.debit(0, 0, cancel));
            if let Some((at, end)) = self.original {
                if at == end {
                    self.original = None;
                    self.at += 1;
                    self.atom = None;
                    continue;
                }
                let b = gate!(self.byte(source, PayloadSpace::Original, at, at..end, work, cancel));
                self.original = Some((at + 1, end));
                return Step::Ready(Some(Symbol::Original(b)));
            }
            if self.at == self.end {
                return Step::Ready(None);
            }
            if !self.atom.is_some_and(|a| self.at >= a.start && self.at < a.end) {
                let lookup = self.atom_lookup.get_or_insert_with(|| AtomLookup::new(self.at));
                self.atom = gate!(source.atom_span(lookup, work, cancel));
                self.atom_lookup = None;
                if !self.atom.is_some_and(|a| self.at >= a.start && self.at < a.end) {
                    return Step::Failed(InputError::SourceContract);
                }
            }
            let atom = self.atom.unwrap();
            if atom.kind == CaptureKind::Opaque {
                gate!(work.debit(1, 0, cancel));
                if atom.end - atom.start != 1 || atom.original_start >= atom.original_end {
                    return Step::Failed(InputError::SourceContract);
                }
                self.in_space = false;
                self.original = Some((atom.original_start, atom.original_end));
                return Step::Ready(Some(Symbol::Opaque {
                    encoding: atom.encoding,
                    length: atom.original_end - atom.original_start,
                }));
            }
            let b = gate!(self.byte(source, PayloadSpace::Logical, self.at, self.at..self.end, work, cancel));
            self.at += 1;
            if space(b) {
                if options.whitespace == Whitespace::All {
                    continue;
                }
                if options.whitespace == Whitespace::Amount {
                    if self.in_space {
                        continue;
                    }
                    self.in_space = true;
                    return Step::Ready(Some(Symbol::Byte(b' ')));
                }
            } else {
                self.in_space = false;
            }
            return Step::Ready(Some(Symbol::Byte(b)));
        }
    }
}

struct Equality {
    left: Cursor,
    right: Cursor,
    prepared: u8,
    endings: [Terminator; 2],
    symbol: Option<Option<Symbol>>,
    other: Option<Option<Symbol>>,
}
impl Equality {
    fn new(left: u64, right: u64) -> Self {
        Self {
            left: Cursor::new(left),
            right: Cursor::new(right),
            prepared: 0,
            endings: [Terminator::None; 2],
            symbol: None,
            other: None,
        }
    }
    fn step(
        &mut self,
        left: &mut impl InputSource,
        right: &mut impl InputSource,
        options: StreamOptions,
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<bool> {
        if self.prepared == 0 {
            self.endings[0] = gate!(self.left.prepare(left, options, work, cancel)).terminator;
            self.prepared = 1;
        }
        if self.prepared == 1 {
            self.endings[1] = gate!(self.right.prepare(right, options, work, cancel)).terminator;
            self.prepared = 2;
        }
        if self.prepared == 2 {
            gate!(work.debit(1, 0, cancel));
            let normalize = |e| if options.ignore_line_endings && e == Terminator::CrLf { Terminator::Lf } else { e };
            if normalize(self.endings[0]) != normalize(self.endings[1]) {
                return Step::Ready(false);
            }
            self.prepared = 3;
        }
        loop {
            if self.symbol.is_none() {
                self.symbol = Some(gate!(self.left.next(left, options, work, cancel)));
            }
            if self.other.is_none() {
                self.other = Some(gate!(self.right.next(right, options, work, cancel)));
            }
            gate!(work.debit(1, 0, cancel));
            let (a, b) = (self.symbol.take().unwrap(), self.other.take().unwrap());
            if a != b {
                return Step::Ready(false);
            }
            if a.is_none() {
                return Step::Ready(true);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Rect {
    a: u64,
    b: u64,
    c: u64,
    d: u64,
}
impl Rect {
    fn n(self) -> u64 {
        self.b - self.a
    }
    fn m(self) -> u64 {
        self.d - self.c
    }
    fn words(self, equal: bool) -> [u64; 8] {
        [self.a, self.b, self.c, self.d, u64::from(equal), 0, 0, 0]
    }
    fn decode(w: [u64; 8]) -> Result<(Self, bool), InputError> {
        if w[0] > w[1] || w[2] > w[3] || w[4] > 1 {
            return Err(InputError::CorruptStore);
        }
        Ok((Self { a: w[0], b: w[1], c: w[2], d: w[3] }, w[4] == 1))
    }
}

/// One depth uses opposite-parity slots from the preceding depth. No file
/// clearing pass and no stale frontier reads: depth/parity bounds gate access.
struct Bisect {
    rect: Rect,
    limit: u64,
    depth: u64,
    k: i128,
    reverse: bool,
    stage: u8,
    candidates: [Option<u64>; 2],
    x: u64,
    y: u64,
    equality: Option<Equality>,
}
impl Bisect {
    fn new(rect: Rect) -> Result<Self, InputError> {
        let total = rect.n().checked_add(rect.m()).ok_or(InputError::Overflow)?;
        Ok(Self {
            rect,
            limit: total / 2 + total % 2,
            depth: 0,
            k: 0,
            reverse: false,
            stage: 0,
            candidates: [None; 2],
            x: 0,
            y: 0,
            equality: None,
        })
    }
    fn index(&self, k: i128) -> Result<u64, InputError> {
        u64::try_from(k + i128::from(self.limit)).map_err(|_| InputError::Overflow)
    }
    fn valid_k(k: i128, depth: u64) -> bool {
        k.abs() <= i128::from(depth) && (k + i128::from(depth)) % 2 == 0
    }
    fn candidate(&self, x: u64, k: i128) -> Option<u64> {
        let y = i128::from(x) - k;
        (x <= self.rect.n() && y >= 0 && y <= i128::from(self.rect.m())).then_some(x)
    }
    fn step<A: Admission>(
        &mut self,
        records: &mut Records<A>,
        left: &mut impl InputSource,
        right: &mut impl InputSource,
        options: StreamOptions,
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<(u64, u64)> {
        loop {
            gate!(work.debit(0, 0, cancel));
            let file = if self.reverse { store::REVERSE } else { store::FORWARD };
            match self.stage {
                0 => {
                    self.candidates = [None; 2];
                    if self.depth == 0 {
                        self.x = 0;
                        self.y = 0;
                        self.stage = 3;
                        continue;
                    }
                    self.stage = 1;
                }
                1 => {
                    if self.k < i128::from(self.depth) {
                        let w = gate!(records.read(file, attempt!(self.index(self.k + 1)), work, cancel));
                        if w[1] == 1 {
                            self.candidates[0] = self.candidate(w[0], self.k);
                        }
                    }
                    self.stage = 2;
                }
                2 => {
                    if self.k > -i128::from(self.depth) {
                        let w = gate!(records.read(file, attempt!(self.index(self.k - 1)), work, cancel));
                        if w[1] == 1 {
                            self.candidates[1] = w[0].checked_add(1).and_then(|x| self.candidate(x, self.k));
                        }
                    }
                    if let Some(x) = self.candidates.into_iter().flatten().max() {
                        self.x = x;
                        self.y = (i128::from(x) - self.k) as u64;
                        self.stage = 3;
                    } else {
                        self.stage = 6;
                    }
                }
                3 => {
                    if self.x == self.rect.n() || self.y == self.rect.m() {
                        self.stage = 4;
                        continue;
                    }
                    let (a, b) = if self.reverse {
                        (self.rect.b - self.x - 1, self.rect.d - self.y - 1)
                    } else {
                        (self.rect.a + self.x, self.rect.c + self.y)
                    };
                    let equal = gate!(
                        self.equality
                            .get_or_insert_with(|| Equality::new(a, b))
                            .step(left, right, options, work, cancel)
                    );
                    self.equality = None;
                    if equal {
                        self.x += 1;
                        self.y += 1;
                    } else {
                        self.stage = 4;
                    }
                }
                4 => {
                    gate!(records.write(
                        file,
                        attempt!(self.index(self.k)),
                        [self.x, 1, 0, 0, 0, 0, 0, 0],
                        work,
                        cancel
                    ));
                    self.stage = 5;
                }
                5 => {
                    let delta = i128::from(self.rect.n()) - i128::from(self.rect.m());
                    let odd = delta % 2 != 0;
                    let other_depth = if !self.reverse && odd {
                        self.depth.checked_sub(1)
                    } else if self.reverse && !odd {
                        Some(self.depth)
                    } else {
                        None
                    };
                    if let Some(depth) = other_depth {
                        let other_k = delta - self.k;
                        if Self::valid_k(other_k, depth) {
                            let opposite = if self.reverse { store::FORWARD } else { store::REVERSE };
                            let w = gate!(records.read(opposite, attempt!(self.index(other_k)), work, cancel));
                            if w[1] == 1 && w[0] > self.rect.n() {
                                return Step::Failed(InputError::CorruptStore);
                            }
                            if w[1] == 1 && self.x >= self.rect.n() - w[0] {
                                let x = if self.reverse { w[0] } else { self.x };
                                let k = if self.reverse { other_k } else { self.k };
                                let y = (i128::from(x) - k) as u64;
                                if (x == 0 && y == 0) || (x == self.rect.n() && y == self.rect.m()) {
                                    return Step::Failed(InputError::CorruptStore);
                                }
                                return Step::Ready((self.rect.a + x, self.rect.c + y));
                            }
                        }
                    }
                    self.stage = 7;
                }
                6 => {
                    gate!(records.write(file, attempt!(self.index(self.k)), [0; 8], work, cancel));
                    self.stage = 7;
                }
                _ => {
                    gate!(work.debit(1, 0, cancel));
                    if self.k < i128::from(self.depth) {
                        self.k += 2;
                    } else if !self.reverse {
                        self.reverse = true;
                        self.k = -i128::from(self.depth);
                    } else {
                        if self.depth == self.limit {
                            return Step::Failed(InputError::CorruptStore);
                        }
                        self.depth += 1;
                        self.reverse = false;
                        self.k = -i128::from(self.depth);
                    }
                    self.stage = 0;
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AlignedRun {
    pub left: u64,
    pub right: u64,
    pub rows: u64,
    pub length: u64,
    pub kind: Kind,
}
impl AlignedRun {
    fn words(self) -> [u64; 8] {
        [self.left, self.right, self.rows, self.length, tag(self.kind), 0, 0, 0]
    }
    fn decode(w: [u64; 8]) -> Result<Self, InputError> {
        let kind = match w[4] {
            0 => Kind::Equal,
            1 => Kind::Insert,
            2 => Kind::Delete,
            3 => Kind::Modify,
            _ => return Err(InputError::CorruptStore),
        };
        if w[3] == 0 || w[2].checked_add(w[3]).is_none() {
            return Err(InputError::CorruptStore);
        }
        Ok(Self { left: w[0], right: w[1], rows: w[2], length: w[3], kind })
    }
}
fn tag(kind: Kind) -> u64 {
    match kind {
        Kind::Equal => 0,
        Kind::Insert => 1,
        Kind::Delete => 2,
        Kind::Modify => 3,
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskHunk {
    pub left: Range<u64>,
    pub right: Range<u64>,
    pub rows: Range<u64>,
}

#[derive(Clone, Copy)]
enum BuildPhase {
    Open,
    Pop,
    Prefix,
    PrefixEmit,
    Suffix,
    QueueSuffix,
    Residual,
    Bisect,
    QueueRight,
    QueueLeft,
    Finish,
}

#[must_use]
pub struct AlignmentBuild<L: InputSource, R: InputSource, A: Admission> {
    left: ManuallyDrop<L>,
    right: ManuallyDrop<R>,
    records: Records<A>,
    options: StreamOptions,
    identities: [InputIdentity; 2],
    nonce: u64,
    phase: BuildPhase,
    stack: u64,
    rect: Rect,
    untrimmed: Rect,
    prefix: u64,
    suffix: u64,
    equal_task: bool,
    equality: Option<Equality>,
    bisect: Option<Bisect>,
    split: (u64, u64),
    emitted: [u64; 2],
    rows: u64,
    runs: u64,
    hunks: u64,
    pending_change: Option<Rect>,
    flush_stage: u8,
}
pub struct Rejected<L, R, A: Admission> {
    pub left: L,
    pub right: R,
    pub options: StreamOptions,
    pub admission: AlignmentAdmission<A>,
    pub error: InputError,
}
#[must_use]
pub struct BuildStep<L: InputSource, R: InputSource, A: Admission> {
    state: BuildState,
    pending: Option<AlignmentBuild<L, R, A>>,
    ready: Option<DiskComparison<L, R, A>>,
    terminal: Option<TerminalOwners<L, R, A>>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildState {
    Pending,
    Ready,
    Cancelled,
    Failed,
}
/// Exact terminal owners. None error means cancellation; Some preserves the
/// original failure. No automatic resource close or budget release is added.
pub struct TerminalOwners<L, R, A: Admission> {
    pub left: L,
    pub right: R,
    pub options: StreamOptions,
    pub retirement: AlignmentRetirement<A>,
    pub error: Option<InputError>,
}
impl<L: InputSource, R: InputSource, A: Admission> BuildStep<L, R, A> {
    fn pending(build: AlignmentBuild<L, R, A>) -> Self {
        Self { state: BuildState::Pending, pending: Some(build), ready: None, terminal: None }
    }
    fn ready(comparison: DiskComparison<L, R, A>) -> Self {
        Self { state: BuildState::Ready, pending: None, ready: Some(comparison), terminal: None }
    }
    fn terminal(owners: TerminalOwners<L, R, A>) -> Self {
        let state = if owners.error.is_some() { BuildState::Failed } else { BuildState::Cancelled };
        Self { state, pending: None, ready: None, terminal: Some(owners) }
    }
    pub fn state(&self) -> BuildState {
        self.state
    }
    /// Wrong-tag extraction returns the complete untouched carrier. It never
    /// drops an input, comparison, admission or retirement owner.
    pub fn into_pending(mut self) -> ControlFlow<Self, AlignmentBuild<L, R, A>> {
        if self.state == BuildState::Pending {
            ControlFlow::Continue(self.pending.take().expect("pending lane"))
        } else {
            ControlFlow::Break(self)
        }
    }
    pub fn into_ready(mut self) -> ControlFlow<Self, DiskComparison<L, R, A>> {
        if self.state == BuildState::Ready {
            ControlFlow::Continue(self.ready.take().expect("ready lane"))
        } else {
            ControlFlow::Break(self)
        }
    }
    pub fn into_terminal(mut self) -> ControlFlow<Self, TerminalOwners<L, R, A>> {
        if matches!(self.state, BuildState::Cancelled | BuildState::Failed) {
            ControlFlow::Continue(self.terminal.take().expect("terminal lane"))
        } else {
            ControlFlow::Break(self)
        }
    }
}

/// Includes the actual transferred path capacities, two by-value state/outcome
/// overlaps, fixed lookup/equality state, and one caller page query/output.
/// Sources and caller path-construction overlap remain invocation charges.
pub fn resident_charge<L: InputSource, R: InputSource, A: Admission>(paths: &store::AlignmentPaths) -> Option<usize> {
    paths
        .capacity()?
        .checked_add(size_of::<AlignmentBuild<L, R, A>>().checked_mul(3)?)?
        .checked_add(size_of::<BuildStep<L, R, A>>().checked_mul(2)?)?
        .checked_add(size_of::<DiskComparison<L, R, A>>().checked_mul(2)?)?
        .checked_add(size_of::<PageQuery>().checked_mul(3)?)?
        .checked_add(size_of::<Page>().checked_mul(3)?)?
        .checked_add(size_of::<crate::words::Refinement>().checked_mul(3)?)?
        .checked_add(size_of::<store::RetirementStep<A>>())
}
pub fn begin<L: InputSource, R: InputSource, A: Admission>(
    left: L,
    right: R,
    options: StreamOptions,
    admission: AlignmentAdmission<A>,
) -> ControlFlow<Rejected<L, R, A>, AlignmentBuild<L, R, A>> {
    let resident = match resident_charge::<L, R, A>(&admission.paths) {
        Some(n) => n,
        None => return ControlFlow::Break(Rejected { left, right, options, admission, error: InputError::Overflow }),
    };
    let nonce = match NEXT_COMPARISON.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1)) {
        Ok(n) => n,
        Err(_) => return ControlFlow::Break(Rejected { left, right, options, admission, error: InputError::Overflow }),
    };
    let records = match Records::begin(admission, resident) {
        ControlFlow::Continue(r) => r,
        ControlFlow::Break((admission, error)) => {
            return ControlFlow::Break(Rejected { left, right, options, admission, error });
        }
    };
    let identities = [left.identity(), right.identity()];
    let rect = Rect { a: 0, b: left.line_count(), c: 0, d: right.line_count() };
    ControlFlow::Continue(AlignmentBuild {
        left: ManuallyDrop::new(left),
        right: ManuallyDrop::new(right),
        records,
        options,
        identities,
        nonce,
        phase: BuildPhase::Open,
        stack: 0,
        rect,
        untrimmed: rect,
        prefix: 0,
        suffix: 0,
        equal_task: false,
        equality: None,
        bisect: None,
        split: (0, 0),
        emitted: [0; 2],
        rows: 0,
        runs: 0,
        hunks: 0,
        pending_change: None,
        flush_stage: 0,
    })
}

impl<L: InputSource, R: InputSource, A: Admission> AlignmentBuild<L, R, A> {
    pub fn progress(&self) -> Progress {
        Progress {
            phase: if matches!(self.phase, BuildPhase::Open) { Phase::Opening } else { Phase::Aligning },
            committed_left: self.emitted[0],
            committed_right: self.emitted[1],
            committed_rows: self.rows,
            disk_bytes: self.records.usage().disk,
            quality: Quality::Pending,
        }
    }
    pub fn into_parts(self) -> (L, R, StreamOptions, AlignmentRetirement<A>) {
        (
            ManuallyDrop::into_inner(self.left),
            ManuallyDrop::into_inner(self.right),
            self.options,
            self.records.retirement(),
        )
    }
    pub fn step(mut self, mut work: ReadWork, cancel: &AtomicBool) -> BuildStep<L, R, A> {
        loop {
            match self.advance(&mut work, cancel) {
                Step::Pending => return BuildStep::pending(self),
                Step::Cancelled => {
                    let (left, right, options, retirement) = self.into_parts();
                    return BuildStep::terminal(TerminalOwners { left, right, options, retirement, error: None });
                }
                Step::Failed(error) => {
                    let (left, right, options, retirement) = self.into_parts();
                    return BuildStep::terminal(TerminalOwners {
                        left,
                        right,
                        options,
                        retirement,
                        error: Some(error),
                    });
                }
                Step::Ready(false) => {}
                Step::Ready(true) => {
                    return BuildStep::ready(DiskComparison {
                        left: self.left,
                        right: self.right,
                        records: self.records,
                        options: self.options,
                        identities: self.identities,
                        rows: self.rows,
                        runs: self.runs,
                        hunks: self.hunks,
                        query: None,
                        next_query: 0,
                        nonce: self.nonce,
                    });
                }
            }
        }
    }

    fn equal(&mut self, a: u64, b: u64, work: &mut ReadWork, cancel: &AtomicBool) -> Step<bool> {
        let result = self.equality.get_or_insert_with(|| Equality::new(a, b)).step(
            &mut *self.left,
            &mut *self.right,
            self.options,
            work,
            cancel,
        );
        if matches!(result, Step::Ready(_)) {
            self.equality = None;
        }
        result
    }
    fn push_task(&mut self, rect: Rect, equal: bool, work: &mut ReadWork, cancel: &AtomicBool) -> Step<()> {
        let next = attempt!(self.stack.checked_add(1).ok_or(InputError::Overflow));
        gate!(self.records.write(store::STACK, self.stack, rect.words(equal), work, cancel));
        self.stack = next;
        Step::Ready(())
    }
    fn write_run(&mut self, run: AlignedRun, work: &mut ReadWork, cancel: &AtomicBool) -> Step<()> {
        let next_rows = attempt!(self.rows.checked_add(run.length).ok_or(InputError::Overflow));
        let next_runs = attempt!(self.runs.checked_add(1).ok_or(InputError::Overflow));
        gate!(self.records.write(store::RUNS, self.runs, run.words(), work, cancel));
        self.rows = next_rows;
        self.runs = next_runs;
        self.emitted[0] = attempt!(
            run.left.checked_add(if run.kind == Kind::Insert { 0 } else { run.length }).ok_or(InputError::Overflow)
        );
        self.emitted[1] = attempt!(
            run.right.checked_add(if run.kind == Kind::Delete { 0 } else { run.length }).ok_or(InputError::Overflow)
        );
        Step::Ready(())
    }
    /// Accumulate adjacent one-sided residuals into one hunk before pairing.
    /// The disk path never treats an unmatched whole rectangle as a coarse edit.
    fn flush_change(&mut self, work: &mut ReadWork, cancel: &AtomicBool) -> Step<()> {
        let Some(rect) = self.pending_change else {
            return Step::Ready(());
        };
        let paired = rect.n().min(rect.m());
        if self.flush_stage == 0 {
            let hunk_rows = attempt!(rect.n().max(rect.m()).checked_add(self.rows).ok_or(InputError::Overflow));
            let next = attempt!(self.hunks.checked_add(1).ok_or(InputError::Overflow));
            gate!(self.records.write(
                store::HUNKS,
                self.hunks,
                [rect.a, rect.b, rect.c, rect.d, self.rows, hunk_rows, 0, 0],
                work,
                cancel
            ));
            self.hunks = next;
            self.flush_stage = 1;
        }
        if self.flush_stage == 1 {
            if paired > 0 {
                gate!(self.write_run(
                    AlignedRun { left: rect.a, right: rect.c, rows: self.rows, length: paired, kind: Kind::Modify },
                    work,
                    cancel
                ));
            }
            self.flush_stage = 2;
        }
        if self.flush_stage == 2 {
            if rect.n() > paired {
                gate!(self.write_run(
                    AlignedRun {
                        left: rect.a + paired,
                        right: rect.c + paired,
                        rows: self.rows,
                        length: rect.n() - paired,
                        kind: Kind::Delete
                    },
                    work,
                    cancel
                ));
            }
            self.flush_stage = 3;
        }
        if self.flush_stage == 3 {
            if rect.m() > paired {
                gate!(self.write_run(
                    AlignedRun {
                        left: rect.b,
                        right: rect.c + paired,
                        rows: self.rows,
                        length: rect.m() - paired,
                        kind: Kind::Insert
                    },
                    work,
                    cancel
                ));
            }
            self.flush_stage = 0;
            self.pending_change = None;
        }
        Step::Ready(())
    }
    fn emit_equal(&mut self, a: u64, c: u64, length: u64, work: &mut ReadWork, cancel: &AtomicBool) -> Step<()> {
        if length > 0 {
            gate!(self.flush_change(work, cancel));
            gate!(self.write_run(
                AlignedRun { left: a, right: c, rows: self.rows, length, kind: Kind::Equal },
                work,
                cancel
            ));
        }
        Step::Ready(())
    }
    fn advance(&mut self, work: &mut ReadWork, cancel: &AtomicBool) -> Step<bool> {
        gate!(work.debit(0, 0, cancel));
        if self.identities != [self.left.identity(), self.right.identity()] {
            return Step::Failed(InputError::SourceChanged);
        }
        match self.phase {
            BuildPhase::Open => {
                gate!(self.records.open(work, cancel));
                gate!(self.push_task(self.rect, false, work, cancel));
                self.phase = BuildPhase::Pop;
            }
            BuildPhase::Pop => {
                if self.stack == 0 {
                    self.phase = BuildPhase::Finish;
                    return Step::Ready(false);
                }
                let w = gate!(self.records.read(store::STACK, self.stack - 1, work, cancel));
                let (rect, equal) = attempt!(Rect::decode(w));
                if rect.b > self.left.line_count() || rect.d > self.right.line_count() || equal && rect.n() != rect.m()
                {
                    return Step::Failed(InputError::CorruptStore);
                }
                self.stack -= 1;
                self.rect = rect;
                self.untrimmed = rect;
                self.equal_task = equal;
                self.prefix = 0;
                self.suffix = 0;
                self.phase = if equal { BuildPhase::PrefixEmit } else { BuildPhase::Prefix };
            }
            BuildPhase::Prefix => {
                if self.rect.a == self.rect.b || self.rect.c == self.rect.d {
                    self.phase = BuildPhase::PrefixEmit;
                } else if gate!(self.equal(self.rect.a, self.rect.c, work, cancel)) {
                    self.rect.a += 1;
                    self.rect.c += 1;
                    self.prefix += 1;
                } else {
                    self.phase = BuildPhase::PrefixEmit;
                }
            }
            BuildPhase::PrefixEmit => {
                let length = if self.equal_task { self.rect.n() } else { self.prefix };
                gate!(self.emit_equal(self.untrimmed.a, self.untrimmed.c, length, work, cancel));
                self.phase = if self.equal_task { BuildPhase::Pop } else { BuildPhase::Suffix };
            }
            BuildPhase::Suffix => {
                if self.rect.a == self.rect.b || self.rect.c == self.rect.d {
                    self.phase = BuildPhase::QueueSuffix;
                } else if gate!(self.equal(self.rect.b - 1, self.rect.d - 1, work, cancel)) {
                    self.rect.b -= 1;
                    self.rect.d -= 1;
                    self.suffix += 1;
                } else {
                    self.phase = BuildPhase::QueueSuffix;
                }
            }
            BuildPhase::QueueSuffix => {
                if self.suffix > 0 {
                    gate!(self.push_task(
                        Rect { a: self.rect.b, b: self.untrimmed.b, c: self.rect.d, d: self.untrimmed.d },
                        true,
                        work,
                        cancel
                    ));
                }
                self.phase = BuildPhase::Residual;
            }
            BuildPhase::Residual => {
                if self.rect.n() == 0 || self.rect.m() == 0 {
                    gate!(work.debit(1, 0, cancel));
                    if self.rect.n() > 0 || self.rect.m() > 0 {
                        if let Some(prev) = &mut self.pending_change {
                            if prev.b != self.rect.a || prev.d != self.rect.c {
                                return Step::Failed(InputError::CorruptStore);
                            }
                            prev.b = self.rect.b;
                            prev.d = self.rect.d;
                        } else {
                            self.pending_change = Some(self.rect);
                        }
                    }
                    self.phase = BuildPhase::Pop;
                } else {
                    self.bisect = Some(attempt!(Bisect::new(self.rect)));
                    self.phase = BuildPhase::Bisect;
                }
            }
            BuildPhase::Bisect => {
                self.split = gate!(self.bisect.as_mut().unwrap().step(
                    &mut self.records,
                    &mut *self.left,
                    &mut *self.right,
                    self.options,
                    work,
                    cancel
                ));
                self.bisect = None;
                self.phase = BuildPhase::QueueRight;
            }
            BuildPhase::QueueRight => {
                gate!(self.push_task(
                    Rect { a: self.split.0, b: self.rect.b, c: self.split.1, d: self.rect.d },
                    false,
                    work,
                    cancel
                ));
                self.phase = BuildPhase::QueueLeft;
            }
            BuildPhase::QueueLeft => {
                gate!(self.push_task(
                    Rect { a: self.rect.a, b: self.split.0, c: self.rect.c, d: self.split.1 },
                    false,
                    work,
                    cancel
                ));
                self.phase = BuildPhase::Pop;
            }
            BuildPhase::Finish => {
                gate!(self.flush_change(work, cancel));
                if self.emitted != [self.left.line_count(), self.right.line_count()] {
                    return Step::Failed(InputError::CorruptStore);
                }
                return Step::Ready(true);
            }
        }
        Step::Ready(false)
    }
}

pub const PAGE_RECORDS: usize = 64;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AlignedRow {
    pub aligned: u64,
    pub left: Option<u64>,
    pub right: Option<u64>,
    pub kind: Kind,
}
#[derive(Clone, Debug)]
pub struct Page {
    pub quality: Quality,
    pub start: u64,
    pub len: usize,
    pub rows: [Option<AlignedRow>; PAGE_RECORDS],
}
impl Default for Page {
    fn default() -> Self {
        Self { quality: Quality::Pending, start: 0, len: 0, rows: [None; PAGE_RECORDS] }
    }
}
/// Fixed admitted query state. The comparison accepts only one disk query at a
/// time. A different request returns QueryChanged until explicit abandon.
pub struct PageQuery {
    start: u64,
    limit: usize,
    page: Page,
    low: u64,
    high: u64,
    found: Option<AlignedRun>,
    active: bool,
    lease: Option<(u64, u64)>,
    publish: usize,
    row_credit: usize,
}
impl PageQuery {
    pub fn new(start: u64, limit: usize) -> Self {
        Self {
            start,
            limit,
            page: Page { start, ..Page::default() },
            low: 0,
            high: 0,
            found: None,
            active: false,
            lease: None,
            publish: 0,
            row_credit: 0,
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum QueryKey {
    Page(u64, usize),
    Navigate(u64, bool),
}
pub struct NavigationQuery {
    row: u64,
    forward: bool,
    low: u64,
    high: u64,
    stage: u8,
    lease: Option<(u64, u64)>,
}
impl NavigationQuery {
    pub fn new(row: u64, forward: bool) -> Self {
        Self { row, forward, low: 0, high: 0, stage: 0, lease: None }
    }
}
#[must_use]
pub struct DiskComparison<L: InputSource, R: InputSource, A: Admission> {
    left: ManuallyDrop<L>,
    right: ManuallyDrop<R>,
    records: Records<A>,
    options: StreamOptions,
    identities: [InputIdentity; 2],
    rows: u64,
    runs: u64,
    hunks: u64,
    query: Option<(QueryKey, u64)>,
    next_query: u64,
    nonce: u64,
}
impl<L: InputSource, R: InputSource, A: Admission> DiskComparison<L, R, A> {
    pub fn row_count(&self) -> u64 {
        self.rows
    }
    pub fn hunk_count(&self) -> u64 {
        self.hunks
    }
    pub fn quality(&self) -> Quality {
        Quality::Complete
    }
    pub fn identities(&self) -> [InputIdentity; 2] {
        self.identities
    }
    pub fn options(&self) -> StreamOptions {
        self.options
    }
    pub fn usage(&self) -> Usage {
        self.records.usage()
    }
    /// Borrow only on the worker, serially with the source's other queries.
    pub fn inputs_mut(&mut self) -> (&mut L, &mut R) {
        (&mut *self.left, &mut *self.right)
    }
    pub fn into_parts(self) -> (L, R, StreamOptions, AlignmentRetirement<A>) {
        (
            ManuallyDrop::into_inner(self.left),
            ManuallyDrop::into_inner(self.right),
            self.options,
            self.records.retirement(),
        )
    }
    pub fn abandon_query(&mut self) {
        self.records.abandon();
        self.query = None;
    }
    fn claim(&mut self, key: QueryKey, lease: &mut Option<(u64, u64)>) -> Result<(), InputError> {
        match (self.query, *lease) {
            (Some((active, id)), Some((nonce, request))) if nonce == self.nonce && active == key && id == request => {
                Ok(())
            }
            (None, None) => {
                self.next_query = self.next_query.checked_add(1).ok_or(InputError::Overflow)?;
                *lease = Some((self.nonce, self.next_query));
                self.query = Some((key, self.next_query));
                Ok(())
            }
            _ => Err(InputError::QueryChanged),
        }
    }
    /// Output stays byte-for-byte unchanged on Pending/Cancelled/Failed.
    pub fn page(
        &mut self,
        query: &mut PageQuery,
        out: &mut Page,
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<()> {
        gate!(work.debit(0, 0, cancel));
        if query.limit > PAGE_RECORDS || query.start > self.rows {
            return Step::Failed(InputError::Range);
        }
        attempt!(self.claim(QueryKey::Page(query.start, query.limit), &mut query.lease));
        let count = query.limit.min((self.rows - query.start).min(PAGE_RECORDS as u64) as usize);
        loop {
            if query.page.len == count {
                // Publication/copy is bounded but also cooperatively prepaid.
                while query.publish < size_of::<Page>() {
                    let n = (size_of::<Page>() - query.publish).min(work.bytes.min(4096) as usize);
                    if n == 0 {
                        return Step::Pending;
                    }
                    gate!(work.debit(n as u64, 0, cancel));
                    query.publish += n;
                }
                gate!(work.debit(0, 0, cancel));
                query.page.quality = Quality::Complete;
                *out = query.page.clone();
                self.query = None;
                return Step::Ready(());
            }
            let row = query.start + query.page.len as u64;
            if query.found.is_some_and(|r| row < r.rows || row >= r.rows + r.length) {
                query.found = None;
            }
            if query.found.is_none() {
                if !query.active {
                    query.low = 0;
                    query.high = self.runs;
                    query.active = true;
                }
                while query.low < query.high {
                    let mid = query.low + (query.high - query.low) / 2;
                    let run = attempt!(AlignedRun::decode(gate!(self.records.read(store::RUNS, mid, work, cancel))));
                    if row < run.rows {
                        query.high = mid;
                    } else if row >= run.rows + run.length {
                        query.low = mid + 1;
                    } else {
                        query.found = Some(run);
                        break;
                    }
                }
                query.active = false;
                if query.found.is_none() {
                    return Step::Failed(InputError::CorruptStore);
                }
            }
            while query.row_credit < size_of::<AlignedRow>() {
                let n = (size_of::<AlignedRow>() - query.row_credit).min(work.bytes.min(256) as usize);
                if n == 0 {
                    return Step::Pending;
                }
                gate!(work.debit(n as u64, 0, cancel));
                query.row_credit += n;
            }
            let run = query.found.unwrap();
            let delta = row - run.rows;
            let left = if run.kind == Kind::Insert {
                None
            } else {
                Some(attempt!(run.left.checked_add(delta).ok_or(InputError::Overflow)))
            };
            let right = if run.kind == Kind::Delete {
                None
            } else {
                Some(attempt!(run.right.checked_add(delta).ok_or(InputError::Overflow)))
            };
            query.page.rows[query.page.len] = Some(AlignedRow { aligned: row, left, right, kind: run.kind });
            query.page.len += 1;
            query.row_credit = 0;
        }
    }
    /// Strict F7/Shift+F7 in uncollapsed global alignment coordinates; no wrap.
    pub fn navigate(
        &mut self,
        query: &mut NavigationQuery,
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> Step<Option<DiskHunk>> {
        gate!(work.debit(0, 0, cancel));
        attempt!(self.claim(QueryKey::Navigate(query.row, query.forward), &mut query.lease));
        if query.stage == 0 {
            query.high = self.hunks;
            query.stage = 1;
        }
        while query.stage == 1 && query.low < query.high {
            let mid = query.low + (query.high - query.low) / 2;
            let w = gate!(self.records.read(store::HUNKS, mid, work, cancel));
            if if query.forward { w[4] <= query.row } else { w[4] < query.row } {
                query.low = mid + 1;
            } else {
                query.high = mid;
            }
        }
        query.stage = 2;
        let index =
            if query.forward { (query.low < self.hunks).then_some(query.low) } else { query.low.checked_sub(1) };
        let result = if let Some(index) = index {
            let w = gate!(self.records.read(store::HUNKS, index, work, cancel));
            if w[0] > w[1] || w[2] > w[3] || w[4] >= w[5] || w[5] > self.rows {
                return Step::Failed(InputError::CorruptStore);
            }
            Some(DiskHunk { left: w[0]..w[1], right: w[2]..w[3], rows: w[4]..w[5] })
        } else {
            None
        };
        self.query = None;
        Step::Ready(result)
    }
}

#[cfg(test)]
#[path = "stream_tests.rs"]
mod tests;
