//! Worker-only continuations for the shared visual-row engine.
//!
//! API candidate for ROOT. This module is deliberately not registered in lib.rs.
//! ROOT's registration is a child of visual_rows (it uses that engine's private
//! admitted row/page/checkpoint types): #[path = "visual_task.rs"] mod task;
//! Source/index adapters must implement the cooperative contract below; an OS
//! WouldBlock is an error, never a scheduling signal. No synchronous fallback
//! is supplied. Query state is owned by the invocation, not a source-global
//! mutable slot. Associated query types are statically sized and must include
//! every retained cursor/decoder/record, rather than an extensible byte buffer.
//!
//! Charges share AllocationBudget with the engine, pages and source admission.
//! Constructor admission covers by-value construction/return overlap. Source
//! windows and source-read staging use fallible admitted heap storage; only
//! bounded display/scalar scratch and query headers are inline. An
//! adapter with additional resident storage must reserve it in that same
//! aggregate before construction and retain that reservation with its query.
use crate::grid::{GraphemeStep, GraphemeStepper};
use crate::visual_rows::{
    AllocationBudget, DISPLAY_BYTES, MetadataReservation, SOURCE_WINDOW, SourceIdentity, WrapError,
};
use kwil_core_text::compare_input::{InputError, LineSpan, Rank, ReadWork, Step, Terminator};
use std::{
    marker::PhantomData,
    mem::size_of,
    ops::{ControlFlow, Range},
    rc::Rc,
    sync::atomic::AtomicBool,
};

#[derive(Debug)]
pub enum TaskError {
    Layout(WrapError),
    Source(InputError),
}
impl From<WrapError> for TaskError {
    fn from(error: WrapError) -> Self {
        Self::Layout(error)
    }
}
impl From<InputError> for TaskError {
    fn from(error: InputError) -> Self {
        Self::Source(error)
    }
}
#[must_use]
pub enum TaskPoll<T> {
    Pending,
    Ready(T),
    Cancelled,
    Failed(TaskError),
}
impl<T> From<Step<T>> for TaskPoll<T> {
    fn from(step: Step<T>) -> Self {
        match step {
            Step::Pending => Self::Pending,
            Step::Ready(value) => Self::Ready(value),
            Step::Cancelled => Self::Cancelled,
            Step::Failed(error) => Self::Failed(error.into()),
        }
    }
}
macro_rules! ready {
    ($step:expr) => {
        match $step {
            TaskPoll::Ready(value) => value,
            TaskPoll::Pending => return TaskPoll::Pending,
            TaskPoll::Cancelled => return TaskPoll::Cancelled,
            TaskPoll::Failed(error) => return TaskPoll::Failed(error),
        }
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceMetadata {
    pub identity: SourceIdentity,
    pub len: u64,
    pub line_count: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provenance {
    /// Exact containing logical source atom. Opaque atoms are never split.
    pub source: Range<u64>,
    pub opaque: bool,
}

/// All functions run on the EXISTING worker. metadata/new_* do no source I/O
/// or traversal. new_read_query may allocate only through its explicit shared
/// budget argument and returns admission refusal before any source access.
/// Queries own admitted resumable source/index state.
/// They are never reset after Pending. Every source, index, provenance, decoder,
/// replay and copy visit is charged BEFORE access, including repeated reads.
/// Other new_* constructors remain allocation-free. No adapter may silently
/// allocate in any query constructor or create an independent allowance.
///
/// poll_read leaves output untouched on Pending, cancellation AND error. A
/// ReadLease adapter must stage its OS read before publishing; ReadLease's raw
/// read_at only promises this for Pending/cancellation. Ready returns a nonzero
/// short prefix unless at EOF. LineSpan carries the complete terminator, with
/// literal CR at EOF remaining content. An ordinary byte source must explicitly
/// return nonopaque provenance, never infer it from a display marker's bytes.
pub trait CooperativeSource {
    type LineQuery;
    type RankQuery;
    type ReadQuery;
    type ProvenanceQuery;
    fn metadata(&self) -> SourceMetadata;
    fn new_line_query(&self, line: u64) -> Self::LineQuery;
    fn new_rank_query(&self, byte: u64) -> Self::RankQuery;
    fn new_read_query(&self, at: u64, length: usize, budget: &AllocationBudget) -> Result<Self::ReadQuery, WrapError>;
    fn new_provenance_query(&self, byte: u64) -> Self::ProvenanceQuery;
    fn poll_line(&self, query: &mut Self::LineQuery, work: &mut ReadWork, cancel: &AtomicBool) -> TaskPoll<LineSpan>;
    fn poll_rank(&self, query: &mut Self::RankQuery, work: &mut ReadWork, cancel: &AtomicBool) -> TaskPoll<Rank>;
    fn poll_read(
        &self,
        query: &mut Self::ReadQuery,
        output: &mut [u8],
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> TaskPoll<usize>;
    fn poll_provenance(
        &self,
        query: &mut Self::ProvenanceQuery,
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> TaskPoll<Provenance>;
}

/// One exact bounded access envelope; credit cannot fund a different access.
/// Accumulation permits a 4-byte scalar or fixed record with one-byte quanta.
/// Cancellation destroys its prepaid credit and never performs the access.
#[derive(Default)]
pub struct WorkCredit {
    required: Option<ReadWork>,
    paid: ReadWork,
}
impl WorkCredit {
    pub fn poll(&mut self, required: ReadWork, work: &mut ReadWork, cancel: &AtomicBool) -> TaskPoll<ReadWork> {
        if self.required.is_some_and(|old| old != required) {
            return TaskPoll::Failed(WrapError::Invalid("changed prepaid access").into());
        }
        self.required = Some(required);
        let bytes = work.bytes.min(required.bytes - self.paid.bytes);
        let operations = work.operations.min(required.operations - self.paid.operations);
        match work.debit(bytes, operations, cancel) {
            Step::Ready(()) => {}
            Step::Cancelled => {
                *self = Self::default();
                return TaskPoll::Cancelled;
            }
            Step::Pending => return TaskPoll::Pending,
            Step::Failed(error) => return TaskPoll::Failed(error.into()),
        }
        self.paid.bytes += bytes;
        self.paid.operations += operations;
        if self.paid == required {
            *self = Self::default();
            TaskPoll::Ready(required)
        } else {
            TaskPoll::Pending
        }
    }
}

struct WindowRead<Q> {
    at: u64,
    wanted: usize,
    query: Q,
    returned: Option<usize>,
    copied: usize,
}

/// One exact, fallibly allocated worker scratch window. Capacity is admitted
/// before the allocator is called, inspected afterward, and held in the same
/// AllocationBudget through every pending/error owner. resize initializes the
/// HEAP allocation; no [u8; SOURCE_WINDOW] value is constructed on the stack.
pub struct TaskBytes {
    storage: crate::visual_rows::AccountedVec<u8>,
}
impl TaskBytes {
    pub fn try_new(budget: &AllocationBudget, length: usize) -> Result<Self, WrapError> {
        if length > SOURCE_WINDOW {
            return Err(WrapError::Admission("scratch window bound"));
        }
        let mut storage = crate::visual_rows::AccountedVec::new(budget, length, length)?;
        storage.reserve_len(length)?;
        storage.data.resize(length, 0);
        Ok(Self { storage })
    }
    pub fn capacity(&self) -> usize {
        self.storage.data.capacity()
    }
}
impl std::ops::Deref for TaskBytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.storage.data
    }
}
impl std::ops::DerefMut for TaskBytes {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.storage.data
    }
}

/// Source adapters can reject an accidentally different aggregate before
/// allocating scratch. Pointer equality is used only for budget ownership,
/// never as document/source identity or content equality.
pub(crate) fn same_budget(left: &AllocationBudget, right: &AllocationBudget) -> bool {
    std::sync::Arc::ptr_eq(&left.0, &right.0)
}

/// Retains BOTH windows through a pause. Staging isolates source OS failures;
/// cache publication occurs only after its admitted copy has completed.
pub struct ReaderContinuation<S: CooperativeSource> {
    bytes: TaskBytes,
    staging: TaskBytes,
    budget: AllocationBudget,
    start: u64,
    length: usize,
    chunk_limit: usize,
    window: Option<WindowRead<S::ReadQuery>>,
    line_query: Option<(u64, S::LineQuery)>,
    line_cache: Option<(u64, LineSpan)>,
    provenance_query: Option<(u64, S::ProvenanceQuery)>,
    provenance_cache: Option<Provenance>,
    read_bytes: u64,
}
impl<S: CooperativeSource> ReaderContinuation<S> {
    fn try_new(budget: &AllocationBudget, chunk_limit: usize) -> Result<Self, WrapError> {
        Ok(Self {
            bytes: TaskBytes::try_new(budget, SOURCE_WINDOW)?,
            staging: TaskBytes::try_new(budget, SOURCE_WINDOW)?,
            budget: budget.clone(),
            start: 0,
            length: 0,
            chunk_limit,
            window: None,
            line_query: None,
            line_cache: None,
            provenance_query: None,
            provenance_cache: None,
            read_bytes: 0,
        })
    }
    pub fn read_bytes(&self) -> u64 {
        self.read_bytes
    }
    fn check(work: &mut ReadWork, cancel: &AtomicBool) -> TaskPoll<()> {
        work.debit(0, 0, cancel).into()
    }
    pub fn byte(&mut self, source: &S, at: u64, work: &mut ReadWork, cancel: &AtomicBool) -> TaskPoll<Option<u8>> {
        ready!(Self::check(work, cancel));
        let metadata = source.metadata();
        if at >= metadata.len {
            return TaskPoll::Ready(None);
        }
        if self.window.is_none() && (at < self.start || at - self.start >= self.length as u64) {
            let wanted = self.chunk_limit.min((metadata.len - at).min(SOURCE_WINDOW as u64) as usize);
            let query = match source.new_read_query(at, wanted, &self.budget) {
                Ok(query) => query,
                Err(error) => return TaskPoll::Failed(error.into()),
            };
            self.window = Some(WindowRead { at, wanted, query, returned: None, copied: 0 });
        }
        if let Some(window) = &mut self.window {
            if window.at != at {
                return TaskPoll::Failed(WrapError::Invalid("changed pending byte query").into());
            }
            if window.returned.is_none() {
                let count =
                    ready!(source.poll_read(&mut window.query, &mut self.staging[..window.wanted], work, cancel));
                if count == 0 || count > window.wanted {
                    return TaskPoll::Failed(WrapError::Invalid("short/invalid source window").into());
                }
                self.read_bytes = match self.read_bytes.checked_add(count as u64) {
                    Some(value) => value,
                    None => return TaskPoll::Failed(WrapError::Admission("source read progress overflow").into()),
                };
                window.returned = Some(count);
                self.length = 0;
            }
            let count = window.returned.expect("completed read");
            while window.copied < count {
                let n = (count - window.copied).min(work.bytes.min(SOURCE_WINDOW as u64) as usize);
                if n == 0 || work.operations == 0 {
                    return TaskPoll::Pending;
                }
                ready!(TaskPoll::from(work.debit(n as u64, 1, cancel)));
                let end = window.copied + n;
                self.bytes[window.copied..end].copy_from_slice(&self.staging[window.copied..end]);
                window.copied = end;
            }
            self.start = window.at;
            self.length = count;
            self.window = None;
        }
        ready!(TaskPoll::from(work.debit(1, 1, cancel)));
        TaskPoll::Ready(Some(self.bytes[(at - self.start) as usize]))
    }
    pub fn line(&mut self, source: &S, line: u64, work: &mut ReadWork, cancel: &AtomicBool) -> TaskPoll<LineSpan> {
        ready!(Self::check(work, cancel));
        if let Some((old, span)) = self.line_cache
            && old == line
        {
            return TaskPoll::Ready(span);
        }
        if self.line_query.is_none() {
            self.line_query = Some((line, source.new_line_query(line)));
        }
        let (old, query) = self.line_query.as_mut().expect("line query");
        if *old != line {
            return TaskPoll::Failed(WrapError::Invalid("changed pending line query").into());
        }
        let span = ready!(source.poll_line(query, work, cancel));
        let terminator = match span.terminator {
            Terminator::None => 0,
            Terminator::Lf => 1,
            Terminator::CrLf => 2,
        };
        if span.start > span.content_end
            || span.content_end > span.end
            || span.end > source.metadata().len
            || span.end - span.content_end != terminator
        {
            return TaskPoll::Failed(WrapError::Invalid("invalid complete line metadata").into());
        }
        self.line_cache = Some((line, span));
        self.line_query = None;
        TaskPoll::Ready(span)
    }
    pub fn provenance(
        &mut self,
        source: &S,
        at: u64,
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> TaskPoll<Provenance> {
        ready!(Self::check(work, cancel));
        if let Some(span) = &self.provenance_cache
            && span.source.contains(&at)
        {
            return TaskPoll::Ready(span.clone());
        }
        if self.provenance_query.is_none() {
            self.provenance_query = Some((at, source.new_provenance_query(at)));
        }
        let (old, query) = self.provenance_query.as_mut().expect("provenance query");
        if *old != at {
            return TaskPoll::Failed(WrapError::Invalid("changed pending provenance query").into());
        }
        let span = ready!(source.poll_provenance(query, work, cancel));
        if !span.source.contains(&at) || span.source.end > source.metadata().len {
            return TaskPoll::Failed(WrapError::Invalid("invalid source provenance span").into());
        }
        self.provenance_cache = Some(span.clone());
        self.provenance_query = None;
        TaskPoll::Ready(span)
    }
}

#[derive(Clone, Copy)]
pub struct Scalar {
    bytes: [u8; 4],
    pub length: usize,
    pub valid: bool,
}
impl Scalar {
    fn text(&self) -> &str {
        std::str::from_utf8(&self.bytes[..self.length]).expect("validated scalar")
    }
}
pub struct ScalarRead {
    at: u64,
    end: u64,
    bytes: [u8; 4],
    loaded: usize,
    wanted: usize,
    credit: WorkCredit,
}
impl ScalarRead {
    fn new(at: u64, end: u64) -> Self {
        Self { at, end, bytes: [0; 4], loaded: 0, wanted: 1, credit: WorkCredit::default() }
    }
    fn poll<S: CooperativeSource>(
        &mut self,
        reader: &mut ReaderContinuation<S>,
        source: &S,
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> TaskPoll<Scalar> {
        if self.loaded == 0 {
            self.bytes[0] = match ready!(reader.byte(source, self.at, work, cancel)) {
                Some(value) => value,
                None => return TaskPoll::Failed(WrapError::Invalid("missing scalar").into()),
            };
            self.loaded = 1;
            self.wanted = match self.bytes[0] {
                0..=0x7f => 1,
                0xc2..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf4 => 4,
                _ => 0,
            };
            if self.wanted == 0 || self.at.saturating_add(self.wanted as u64) > self.end {
                return TaskPoll::Ready(Scalar { bytes: self.bytes, length: 1, valid: false });
            }
        }
        while self.loaded < self.wanted {
            let provenance = ready!(reader.provenance(source, self.at + self.loaded as u64, work, cancel));
            if provenance.opaque {
                return TaskPoll::Ready(Scalar { bytes: self.bytes, length: 1, valid: false });
            }
            self.bytes[self.loaded] = match ready!(reader.byte(source, self.at + self.loaded as u64, work, cancel)) {
                Some(value) => value,
                None => return TaskPoll::Failed(WrapError::Invalid("truncated scalar").into()),
            };
            self.loaded += 1;
        }
        ready!(self.credit.poll(ReadWork::new(self.wanted as u64, 1), work, cancel));
        let valid = std::str::from_utf8(&self.bytes[..self.wanted]).is_ok();
        TaskPoll::Ready(Scalar { bytes: self.bytes, length: if valid { self.wanted } else { 1 }, valid })
    }
}

struct PreviousScalar {
    end: u64,
    start: u64,
    length: u64,
    scalar: Option<ScalarRead>,
}
impl PreviousScalar {
    fn new(end: u64, start: u64) -> Self {
        Self { end, start, length: 1, scalar: None }
    }
    fn poll<S: CooperativeSource>(
        &mut self,
        reader: &mut ReaderContinuation<S>,
        source: &S,
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> TaskPoll<(u64, Scalar)> {
        while self.length <= 4 && self.end >= self.start.saturating_add(self.length) {
            let at = self.end - self.length;
            let query = self.scalar.get_or_insert_with(|| ScalarRead::new(at, self.end));
            let scalar = ready!(query.poll(reader, source, work, cancel));
            if scalar.valid && scalar.length as u64 == self.length {
                return TaskPoll::Ready((at, scalar));
            }
            self.length += 1;
            self.scalar = None;
        }
        TaskPoll::Failed(WrapError::Invalid("invalid grapheme pre-context").into())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cluster {
    pub source: Range<u64>,
    pub valid: bool,
    pub opaque: bool,
}
enum ClusterPhase {
    Start,
    Provenance,
    Scalar,
    Step,
    Context(u64),
    Provide(u64, Scalar),
}

/// The grapheme cursor, scalar in flight, backward context query and exact scan
/// offset ALL survive Pending. There is no cluster-sized buffer or restart.
pub struct GraphemeContinuation {
    next: u64,
    end: u64,
    context_start: u64,
    stepper: GraphemeStepper,
    start: u64,
    at: u64,
    scalar_query: Option<ScalarRead>,
    scalar: Option<Scalar>,
    previous: Option<PreviousScalar>,
    phase: ClusterPhase,
    credit: WorkCredit,
}
impl GraphemeContinuation {
    pub fn new(start: u64, end: u64) -> Result<Self, WrapError> {
        Ok(Self {
            next: start,
            end,
            context_start: start,
            stepper: GraphemeStepper::new(start, end).map_err(WrapError::Invalid)?,
            start,
            at: start,
            scalar_query: None,
            scalar: None,
            previous: None,
            phase: ClusterPhase::Start,
            credit: WorkCredit::default(),
        })
    }
    fn finish(&mut self, end: u64, valid: bool, opaque: bool) -> TaskPoll<Option<Cluster>> {
        self.next = end;
        self.phase = ClusterPhase::Start;
        self.scalar_query = None;
        self.scalar = None;
        self.previous = None;
        TaskPoll::Ready(Some(Cluster { source: self.start..end, valid, opaque }))
    }
    fn reset(&mut self, at: u64) -> Result<(), WrapError> {
        self.stepper = GraphemeStepper::new(at, self.end).map_err(WrapError::Invalid)?;
        self.context_start = at;
        Ok(())
    }
    pub fn poll<S: CooperativeSource>(
        &mut self,
        reader: &mut ReaderContinuation<S>,
        source: &S,
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> TaskPoll<Option<Cluster>> {
        loop {
            ready!(ReaderContinuation::<S>::check(work, cancel));
            match self.phase {
                ClusterPhase::Start => {
                    if self.next >= self.end {
                        return TaskPoll::Ready(None);
                    }
                    self.start = self.next;
                    self.at = self.next;
                    self.phase = ClusterPhase::Provenance;
                }
                ClusterPhase::Provenance => {
                    let provenance = ready!(reader.provenance(source, self.at, work, cancel));
                    if provenance.opaque {
                        if self.at != self.start {
                            if let Err(error) = self.reset(self.at) {
                                return TaskPoll::Failed(error.into());
                            }
                            return self.finish(self.at, true, false);
                        }
                        if provenance.source.start != self.at || provenance.source.end > self.end {
                            return TaskPoll::Failed(WrapError::Invalid("opaque source endpoint is not exact").into());
                        }
                        if let Err(error) = self.reset(provenance.source.end) {
                            return TaskPoll::Failed(error.into());
                        }
                        return self.finish(provenance.source.end, false, true);
                    }
                    // The scalar may span multiple provenance pieces. Once
                    // its lead has been classified, do not replay that query
                    // while ScalarRead is paused on a continuation byte: that
                    // would evict the tail's provenance and spend each tiny
                    // quantum resolving the same two pieces forever.
                    self.phase = ClusterPhase::Scalar;
                }
                ClusterPhase::Scalar => {
                    let query = self.scalar_query.get_or_insert_with(|| ScalarRead::new(self.at, self.end));
                    let scalar = ready!(query.poll(reader, source, work, cancel));
                    self.scalar_query = None;
                    if !scalar.valid {
                        let end = if self.at == self.start { self.at + 1 } else { self.at };
                        if let Err(error) = self.reset(end) {
                            return TaskPoll::Failed(error.into());
                        }
                        return self.finish(end, self.at != self.start, false);
                    }
                    self.scalar = Some(scalar);
                    self.phase = ClusterPhase::Step;
                }
                ClusterPhase::Step => {
                    let scalar = self.scalar.expect("retained scalar");
                    ready!(self.credit.poll(ReadWork::new(scalar.length as u64, 1), work, cancel));
                    match self.stepper.step(self.at, scalar.text()) {
                        Ok(GraphemeStep::Boundary(end)) => return self.finish(end, true, false),
                        Ok(GraphemeStep::End) => return self.finish(self.end, true, false),
                        Ok(GraphemeStep::Scalar(next)) => {
                            self.at = next;
                            self.scalar = None;
                            self.phase = ClusterPhase::Provenance;
                        }
                        Ok(GraphemeStep::Context(end)) => self.phase = ClusterPhase::Context(end),
                        Err(message) => return TaskPoll::Failed(WrapError::Invalid(message).into()),
                    }
                }
                ClusterPhase::Context(end) => {
                    let query = self.previous.get_or_insert_with(|| PreviousScalar::new(end, self.context_start));
                    let (at, scalar) = ready!(query.poll(reader, source, work, cancel));
                    self.previous = None;
                    self.phase = ClusterPhase::Provide(at, scalar);
                }
                ClusterPhase::Provide(at, scalar) => {
                    ready!(self.credit.poll(ReadWork::new(scalar.length as u64, 1), work, cancel));
                    if let Err(message) = self.stepper.provide_context(at, scalar.text()) {
                        return TaskPoll::Failed(WrapError::Invalid(message).into());
                    }
                    self.phase = ClusterPhase::Step;
                }
            }
        }
    }
}

/// Constructor refusal transfers the exact source owner back. No Arc clone,
/// unsafe Send, hidden thread or final-drop source settlement is introduced.
#[must_use]
pub struct Refused<S> {
    pub source: S,
    pub error: WrapError,
}

/// Source-independent worker state. The higher-level shared engine owns this
/// value, so every yield retains the source, pending reader and provenance query
/// together. Drop/into_source MUST run on that existing owning worker.
#[must_use]
pub struct TaskContext<S: CooperativeSource> {
    pub source: S,
    pub reader: ReaderContinuation<S>,
    metadata: SourceMetadata,
    _header: MetadataReservation,
    _worker: PhantomData<Rc<()>>,
}
impl<S: CooperativeSource> TaskContext<S> {
    pub fn try_new(source: S, budget: &AllocationBudget, chunk_limit: usize) -> Result<Self, Refused<S>> {
        let metadata = source.metadata();
        if !(1..=SOURCE_WINDOW).contains(&chunk_limit) || metadata.line_count == 0 {
            return Err(Refused { source, error: WrapError::Invalid("invalid continuation metadata/chunk") });
        }
        let Some(bytes) = size_of::<Self>()
            .checked_mul(3)
            .and_then(|n| n.checked_add(3 * size_of::<GraphemeContinuation>() + DISPLAY_BYTES))
        else {
            return Err(Refused { source, error: WrapError::Admission("continuation header overflow") });
        };
        let header = match budget.reserve_metadata(bytes) {
            Ok(header) => header,
            Err(error) => return Err(Refused { source, error }),
        };
        let reader = match ReaderContinuation::try_new(budget, chunk_limit) {
            Ok(reader) => reader,
            Err(error) => return Err(Refused { source, error }),
        };
        Ok(Self { source, reader, metadata, _header: header, _worker: PhantomData })
    }
    pub fn check(&self, work: &mut ReadWork, cancel: &AtomicBool) -> TaskPoll<()> {
        ready!(ReaderContinuation::<S>::check(work, cancel));
        if self.source.metadata() != self.metadata {
            return TaskPoll::Failed(WrapError::Stale.into());
        }
        TaskPoll::Ready(())
    }
    pub fn into_source(self) -> S {
        self.source
    }
}

/// ReadLease has an all-or-error OS read, followed by a separately prepaid
/// publication. Holding the returned prefix in the query guarantees output is
/// unchanged even when the next quantum cannot fund the caller transfer.
pub struct LeaseReadQuery {
    read_owner: u64,
    at: u64,
    length: usize,
    bytes: TaskBytes,
    returned: Option<usize>,
    io_credit: WorkCredit,
    copy_credit: WorkCredit,
    _metadata: MetadataReservation,
}
impl<A: kwil_core_text::compare_input::Admission> CooperativeSource for crate::compare_source::CompareSource<A> {
    type LineQuery = kwil_core_text::compare_input::LineLookup;
    type RankQuery = kwil_core_text::compare_input::RankLookup;
    type ReadQuery = LeaseReadQuery;
    type ProvenanceQuery = kwil_core_text::compare_input::AtomLookup;
    fn metadata(&self) -> SourceMetadata {
        SourceMetadata {
            identity: self.identity(),
            len: self.lease().byte_len(),
            line_count: self.lease().line_count(),
        }
    }
    fn new_line_query(&self, line: u64) -> Self::LineQuery {
        Self::LineQuery::new(line)
    }
    fn new_rank_query(&self, byte: u64) -> Self::RankQuery {
        Self::RankQuery::new(byte)
    }
    fn new_read_query(&self, at: u64, length: usize, budget: &AllocationBudget) -> Result<Self::ReadQuery, WrapError> {
        let metadata = budget.reserve_metadata(3 * size_of::<LeaseReadQuery>())?;
        Ok(LeaseReadQuery {
            read_owner: self.read_owner_id(),
            at,
            length,
            bytes: TaskBytes::try_new(budget, length)?,
            returned: None,
            io_credit: WorkCredit::default(),
            copy_credit: WorkCredit::default(),
            _metadata: metadata,
        })
    }
    fn new_provenance_query(&self, byte: u64) -> Self::ProvenanceQuery {
        Self::ProvenanceQuery::new(byte)
    }
    fn poll_line(&self, query: &mut Self::LineQuery, work: &mut ReadWork, cancel: &AtomicBool) -> TaskPoll<LineSpan> {
        self.lease().line_span(query, work, cancel).into()
    }
    fn poll_rank(&self, query: &mut Self::RankQuery, work: &mut ReadWork, cancel: &AtomicBool) -> TaskPoll<Rank> {
        self.lease().rank(query, work, cancel).into()
    }
    fn poll_read(
        &self,
        query: &mut Self::ReadQuery,
        output: &mut [u8],
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> TaskPoll<usize> {
        ready!(TaskPoll::from(work.debit(0, 0, cancel)));
        // CompareSource clones share this private Shared owner's nonce. A
        // separately acquired source with equal InputIdentity must never use
        // this query, including when its OS read already staged a prefix.
        // No source clone, pointer-derived key or additional slot is needed.
        if query.read_owner != self.read_owner_id() {
            return TaskPoll::Failed(InputError::QueryChanged.into());
        }
        if output.len() != query.length || query.length > SOURCE_WINDOW {
            return TaskPoll::Failed(WrapError::Invalid("changed lease read query").into());
        }
        if query.returned.is_none() {
            let required = ReadWork::new(query.length as u64 * 2, 1);
            let mut paid = ready!(query.io_credit.poll(required, work, cancel));
            let count = match self.lease().read_at(
                kwil_core_text::compare_input::PayloadSpace::Logical,
                query.at,
                &mut query.bytes[..query.length],
                &mut paid,
                cancel,
            ) {
                Step::Ready(count) => count,
                Step::Pending => {
                    return TaskPoll::Failed(WrapError::Invalid("lease exceeded prepaid read envelope").into());
                }
                Step::Cancelled => return TaskPoll::Cancelled,
                Step::Failed(error) => return TaskPoll::Failed(error.into()),
            };
            query.returned = Some(count);
        }
        let count = query.returned.expect("retained lease read");
        ready!(query.copy_credit.poll(ReadWork::new(count as u64, 1), work, cancel));
        output[..count].copy_from_slice(&query.bytes[..count]);
        TaskPoll::Ready(count)
    }
    fn poll_provenance(
        &self,
        query: &mut Self::ProvenanceQuery,
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> TaskPoll<Provenance> {
        let span = ready!(TaskPoll::from(self.lease().atom_span(query, work, cancel)));
        match span {
            Some(span) => TaskPoll::Ready(Provenance {
                source: span.start..span.end,
                opaque: span.kind == kwil_core_text::compare_input::CaptureKind::Opaque,
            }),
            None => TaskPoll::Failed(WrapError::Invalid("missing source provenance").into()),
        }
    }
}

use crate::visual_rows::{
    AccountedVec, AtomKind, BuiltRow, CaretAffinity, Checkpoint, LayoutEngine, LayoutGeometry, LayoutKey, MAX_COLUMNS,
    MAX_PAGE_BYTES, PaintAtom, SourcePoint, VisualRow,
};
use unicode_width::UnicodeWidthStr;

enum RowPhase {
    Line,
    Cluster,
    Payload,
    Atom,
    Clip,
    Remember,
}

/// Greedy row construction extracted from LayoutEngine::row. The only growing
/// collection is the existing admitted row atom vector; giant cluster text is
/// never retained. Whitespace rewind, tab restart and clipped atom endpoints
/// have the same coordinate meaning as the synchronous engine.
struct RowContinuation {
    cp: Checkpoint,
    line: Option<LineSpan>,
    stream: Option<GraphemeContinuation>,
    cluster: Option<Cluster>,
    payload: [u8; DISPLAY_BYTES],
    payload_done: usize,
    atoms: AccountedVec<PaintAtom>,
    column: usize,
    whitespace_break: Option<(usize, u64)>,
    end: u64,
    clip: usize,
    phase: RowPhase,
    credit: WorkCredit,
}
impl RowContinuation {
    fn new(engine: &LayoutEngine, cp: Checkpoint) -> Result<Self, WrapError> {
        let atoms = match &engine.page_allocation {
            Some(aggregate) => {
                AccountedVec::with_aggregate(&engine.budget, aggregate, MAX_COLUMNS + 1, MAX_PAGE_BYTES)?
            }
            None => AccountedVec::new(&engine.budget, MAX_COLUMNS + 1, MAX_PAGE_BYTES)?,
        };
        Ok(Self {
            cp,
            line: None,
            stream: None,
            cluster: None,
            payload: [0; DISPLAY_BYTES],
            payload_done: 0,
            atoms,
            column: 0,
            whitespace_break: None,
            end: cp.byte,
            clip: 0,
            phase: RowPhase::Line,
            credit: WorkCredit::default(),
        })
    }
    fn poll<S: CooperativeSource>(
        &mut self,
        engine: &mut LayoutEngine,
        context: &mut TaskContext<S>,
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> TaskPoll<()> {
        let geometry = engine.key.expect("prepared key").geometry;
        loop {
            ready!(context.check(work, cancel));
            match self.phase {
                RowPhase::Line => {
                    let line = ready!(context.reader.line(&context.source, self.cp.logical_line, work, cancel));
                    if self.cp.byte < line.start || self.cp.byte > line.content_end {
                        return TaskPoll::Failed(WrapError::Invalid("checkpoint outside line content").into());
                    }
                    self.stream = match GraphemeContinuation::new(self.cp.byte, line.content_end) {
                        Ok(stream) => Some(stream),
                        Err(error) => return TaskPoll::Failed(error.into()),
                    };
                    self.line = Some(line);
                    self.phase = RowPhase::Cluster;
                }
                RowPhase::Cluster => {
                    let cluster = ready!(self.stream.as_mut().expect("row stream").poll(
                        &mut context.reader,
                        &context.source,
                        work,
                        cancel
                    ));
                    let Some(cluster) = cluster else {
                        self.phase = RowPhase::Clip;
                        continue;
                    };
                    self.payload_done = 0;
                    self.payload = [0; DISPLAY_BYTES];
                    self.cluster = Some(cluster);
                    self.phase = RowPhase::Payload;
                }
                RowPhase::Payload => {
                    let cluster = self.cluster.as_ref().expect("row cluster");
                    let length = cluster.source.end - cluster.source.start;
                    let wanted = if cluster.opaque || length > DISPLAY_BYTES as u64 {
                        0
                    } else if cluster.valid {
                        length as usize
                    } else {
                        1
                    };
                    while self.payload_done < wanted {
                        self.payload[self.payload_done] = match ready!(context.reader.byte(
                            &context.source,
                            cluster.source.start + self.payload_done as u64,
                            work,
                            cancel
                        )) {
                            Some(value) => value,
                            None => return TaskPoll::Failed(WrapError::Invalid("missing payload byte").into()),
                        };
                        self.payload_done += 1;
                    }
                    self.phase = RowPhase::Atom;
                }
                RowPhase::Atom => {
                    // Fixed <=256-byte payload scans plus admitted vector-growth
                    // relocation. No source access is covered by this credit.
                    let relocation = if self.atoms.len() == self.atoms.data.capacity() { self.atoms.len() } else { 0 };
                    let bytes = (self.payload_done * 4 + size_of::<PaintAtom>()) as u64
                        + relocation as u64 * size_of::<PaintAtom>() as u64;
                    ready!(self.credit.poll(ReadWork::new(bytes, self.payload_done as u64 + 1), work, cancel));
                    let cluster = self.cluster.as_ref().expect("row cluster");
                    let atom = match atom_from_payload(cluster, self.payload, self.payload_done, self.column, geometry)
                    {
                        Ok(atom) => atom,
                        Err(error) => return TaskPoll::Failed(error.into()),
                    };
                    let next_column = usize::from(atom.cells.end);
                    if next_column > usize::from(geometry.columns) && !self.atoms.is_empty() {
                        if !atom.whitespace
                            && let Some((count, byte)) = self.whitespace_break
                        {
                            self.atoms.data.truncate(count);
                            self.end = byte;
                        }
                        self.phase = RowPhase::Clip;
                        continue;
                    }
                    self.end = atom.source.end;
                    self.column = next_column;
                    let whitespace = atom.whitespace;
                    if let Err(error) = self.atoms.push(atom) {
                        return TaskPoll::Failed(error.into());
                    }
                    if whitespace {
                        self.whitespace_break = Some((self.atoms.len(), self.end));
                    }
                    self.phase =
                        if self.column > usize::from(geometry.columns) { RowPhase::Clip } else { RowPhase::Cluster };
                }
                RowPhase::Clip => {
                    while self.clip < self.atoms.len() {
                        ready!(self.credit.poll(ReadWork::new(size_of::<PaintAtom>() as u64, 1), work, cancel));
                        self.atoms.data[self.clip].cells.end =
                            self.atoms.data[self.clip].cells.end.min(geometry.columns);
                        self.clip += 1;
                    }
                    if self.end == self.cp.byte && self.cp.byte != self.line.expect("row line").content_end {
                        return TaskPoll::Failed(WrapError::Invalid("wrap failed to advance").into());
                    }
                    self.phase = RowPhase::Remember;
                }
                RowPhase::Remember => {
                    // Checkpoint storage is capped at 4096 records. Include the
                    // full search/eviction and possible replacement overlap.
                    let count = engine.checkpoints.len() as u64;
                    ready!(self.credit.poll(
                        ReadWork::new((count * 3 + 1) * size_of::<Checkpoint>() as u64, count + 1),
                        work,
                        cancel
                    ));
                    if let Err(error) = engine.remember(self.cp) {
                        return TaskPoll::Failed(error.into());
                    }
                    return TaskPoll::Ready(());
                }
            }
        }
    }
    fn into_row(self) -> BuiltRow {
        let line = self.line.expect("completed row");
        let row = VisualRow {
            logical_line: self.cp.logical_line,
            source_coverage: self.cp.byte..self.end,
            continuation: self.cp.byte != line.start,
            hard_line_end: self.end == line.content_end,
            ordinal_in_line: self.cp.ordinal,
            atoms: 0..self.atoms.len(),
        };
        BuiltRow { row, atoms: self.atoms }
    }
}

fn atom_from_payload(
    cluster: &Cluster,
    mut payload: [u8; DISPLAY_BYTES],
    payload_length: usize,
    column: usize,
    geometry: LayoutGeometry,
) -> Result<PaintAtom, WrapError> {
    let mut length = payload_length as u16;
    let mut kind = AtomKind::Normal;
    let mut byte_exact = cluster.valid;
    let (width, whitespace, tab) = if cluster.opaque || cluster.source.end - cluster.source.start > DISPLAY_BYTES as u64
    {
        payload[0] = b'?';
        length = 1;
        kind = AtomKind::Opaque;
        byte_exact = false;
        (1, false, false)
    } else if !cluster.valid {
        const HEX: &[u8] = b"0123456789ABCDEF";
        let byte = payload[0];
        payload[..4].copy_from_slice(&[b'\\', b'x', HEX[(byte >> 4) as usize], HEX[(byte & 15) as usize]]);
        length = 4;
        kind = AtomKind::InvalidByte;
        byte_exact = false;
        (4, false, false)
    } else {
        let text = std::str::from_utf8(&payload[..usize::from(length)])
            .map_err(|_| WrapError::Invalid("non-UTF8 prepared cluster"))?;
        if text == "\t" {
            (usize::from(geometry.tab_size) - column % usize::from(geometry.tab_size), true, true)
        } else if text.chars().any(char::is_control) {
            const HEX: &[u8] = b"0123456789ABCDEF";
            let byte = payload[0];
            payload[..4].copy_from_slice(&[b'\\', b'x', HEX[(byte >> 4) as usize], HEX[(byte & 15) as usize]]);
            length = 4;
            byte_exact = false;
            (4, false, false)
        } else {
            (UnicodeWidthStr::width(text).max(1), text.chars().all(char::is_whitespace), false)
        }
    };
    let end = column.checked_add(width).ok_or(WrapError::Admission("cell overflow"))?;
    Ok(PaintAtom {
        source: cluster.source.clone(),
        cells: column as u16..end.min(u16::MAX as usize) as u16,
        kind,
        payload,
        payload_len: length,
        whitespace,
        tab,
        byte_exact,
    })
}

enum SeekKind {
    Locate(SourcePoint),
    Next(VisualRow),
    Previous(VisualRow),
}
enum SeekPhase {
    Start,
    Rank,
    PreviousLine,
    Line,
    Checkpoints,
    Build,
    Decide,
    Done,
}
struct SeekContinuation<S: CooperativeSource> {
    kind: SeekKind,
    point: Option<SourcePoint>,
    rank: Option<S::RankQuery>,
    logical: u64,
    cp: Option<Checkpoint>,
    checkpoint_index: usize,
    building: Option<RowContinuation>,
    result: Option<BuiltRow>,
    phase: SeekPhase,
    credit: WorkCredit,
}

use crate::visual_rows::{
    BudgetState, CursorSet, LayoutPage, MAX_ATOMS, MAX_OVERSCAN, MAX_ROWS, MAX_SYNTAX_BYTES, MAX_SYNTAX_WINDOWS,
    NavigationCommand, NavigationFence, ResolvedNavigation, TopAnchor, VisualMotion,
};

pub enum TaskRequest {
    Fill { key: LayoutKey, anchor: TopAnchor },
    Scroll { key: LayoutKey, anchor: TopAnchor, delta: i64 },
    Navigate { fence: NavigationFence, expected: CursorSet, command: NavigationCommand },
}
impl TaskRequest {
    fn key(&self) -> LayoutKey {
        match self {
            Self::Fill { key, .. } | Self::Scroll { key, .. } => *key,
            Self::Navigate { fence, .. } => fence.key,
        }
    }
}
/// One exact page owner plus optional navigation-only ownership. Constructors
/// are private: a navigation result never creates a second page slot. A wrong
/// extraction returns this COMPLETE carrier, including its header admission.
#[must_use]
pub struct TaskOutput {
    page: LayoutPage,
    navigation: Option<NavigationOutput>,
    _header: MetadataReservation,
}
struct NavigationOutput {
    fence: NavigationFence,
    expected: CursorSet,
    targets: CursorSet,
    header: crate::visual_rows::AllocationCharge,
}
impl TaskOutput {
    fn page(page: LayoutPage, header: MetadataReservation) -> Self {
        Self { page, navigation: None, _header: header }
    }
    fn navigation(page: LayoutPage, navigation: NavigationOutput, header: MetadataReservation) -> Self {
        Self { page, navigation: Some(navigation), _header: header }
    }
    pub fn into_page(self) -> ControlFlow<Self, LayoutPage> {
        if self.navigation.is_some() {
            return ControlFlow::Break(self);
        }
        ControlFlow::Continue(self.page)
    }
    pub fn into_navigation(mut self) -> ControlFlow<Self, ResolvedNavigation> {
        let Some(navigation) = self.navigation.take() else { return ControlFlow::Break(self) };
        ControlFlow::Continue(ResolvedNavigation {
            fence: navigation.fence,
            expected: navigation.expected,
            targets: navigation.targets,
            reveal: self.page,
            _header: navigation.header,
        })
    }
}
#[must_use]
pub struct TaskRefusal<S> {
    pub engine: LayoutEngine,
    pub source: S,
    pub request: TaskRequest,
    pub error: WrapError,
}
#[must_use]
pub enum TaskStep {
    Pending,
    Ready,
    Cancelled,
    Failed(TaskError),
}
enum TaskPhase {
    Start,
    ScrollLocate,
    ScrollMove,
    NavigationStart,
    NavigationLocate,
    NavigationGoal,
    NavigationMove,
    NavigationTarget,
    NavigationNormalize,
    FillStart,
    FillLocate,
    FillAppend,
    FillNext,
    Syntax,
    Finish,
    Done,
}

/// ONE owned operation on the existing worker. No task step borrows a source
/// from a temporary stack frame. Terminal outcomes retain the complete task;
/// only explicit worker settlement extracts its engine/source/request/output.
/// Relative FIFO and desired-fill replacement stay with the existing host.
#[must_use]
pub struct VisualTask<S: CooperativeSource> {
    owner: AccountedVec<VisualTaskState<S>>,
    _header: MetadataReservation,
}

// This is the one heap-resident worker owner. TaskBytes removed every large
// inline byte window BEFORE placing this state on the heap: construction and
// settlement therefore also avoid temporary 64/128 KiB stack values.
struct VisualTaskState<S: CooperativeSource> {
    engine: LayoutEngine,
    context: TaskContext<S>,
    request: Option<TaskRequest>,
    output: Option<TaskOutput>,
    key: LayoutKey,
    anchor: TopAnchor,
    phase: TaskPhase,
    seek: Option<SeekContinuation<S>>,
    row: Option<BuiltRow>,
    page: Option<LayoutPage>,
    targets: Option<CursorSet>,
    cursor: usize,
    atom_index: usize,
    goal: u16,
    remaining: u64,
    syntax_index: usize,
    syntax_bytes: usize,
    credit: WorkCredit,
}
impl LayoutEngine {
    /// Admission is source-free and returns all supplied owners on refusal.
    /// ROOT may expose convenience begin_fill/scroll/navigate wrappers around
    /// this single constructor; no alternate wrapping implementation is needed.
    pub fn begin_task<S: CooperativeSource>(
        self,
        source: S,
        request: TaskRequest,
    ) -> ControlFlow<TaskRefusal<S>, VisualTask<S>> {
        let key = request.key();
        let metadata = source.metadata();
        if let Err(error) = key.validate() {
            return ControlFlow::Break(TaskRefusal { engine: self, source, request, error });
        }
        if key.source != metadata.identity || metadata.line_count == 0 {
            return ControlFlow::Break(TaskRefusal { engine: self, source, request, error: WrapError::Stale });
        }
        let Some(bytes) = size_of::<VisualTask<S>>()
            .checked_mul(3)
            .and_then(|bytes| bytes.checked_add(3 * size_of::<VisualTaskState<S>>()))
        else {
            return ControlFlow::Break(TaskRefusal {
                engine: self,
                source,
                request,
                error: WrapError::Admission("task header overflow"),
            });
        };
        let header = match self.budget.reserve_metadata(bytes) {
            Ok(header) => header,
            Err(error) => return ControlFlow::Break(TaskRefusal { engine: self, source, request, error }),
        };
        // Reserve and allocate the entire resident state BEFORE constructing it
        // or moving source/request into it. Every refusal below returns the
        // originals; the temporary empty heap owner unwinds its own charge.
        let mut owner = match AccountedVec::new(&self.budget, 1, size_of::<VisualTaskState<S>>()) {
            Ok(owner) => owner,
            Err(error) => return ControlFlow::Break(TaskRefusal { engine: self, source, request, error }),
        };
        if let Err(error) = owner.reserve_len(1) {
            return ControlFlow::Break(TaskRefusal { engine: self, source, request, error });
        }
        let context = match TaskContext::try_new(source, &self.budget, self.chunk_limit) {
            Ok(context) => context,
            Err(Refused { source, error }) => {
                return ControlFlow::Break(TaskRefusal { engine: self, source, request, error });
            }
        };
        let mut engine = self;
        if engine.key != Some(key) {
            engine.checkpoints.data.clear();
            engine.key = Some(key);
        }
        let anchor = match &request {
            TaskRequest::Fill { anchor, .. } | TaskRequest::Scroll { anchor, .. } => *anchor,
            TaskRequest::Navigate { .. } => TopAnchor::default(),
        };
        owner.data.push(VisualTaskState {
            engine,
            context,
            request: Some(request),
            output: None,
            key,
            anchor,
            phase: TaskPhase::Start,
            seek: None,
            row: None,
            page: None,
            targets: None,
            cursor: 0,
            atom_index: 0,
            goal: 0,
            remaining: 0,
            syntax_index: 0,
            syntax_bytes: 0,
            credit: WorkCredit::default(),
        });
        ControlFlow::Continue(VisualTask { owner, _header: header })
    }
}
impl<S: CooperativeSource> VisualTask<S> {
    pub fn metadata(&self) -> SourceMetadata {
        self.owner.data[0].context.metadata
    }
    pub fn read_bytes(&self) -> u64 {
        self.owner.data[0].context.reader.read_bytes()
    }
    pub fn step(&mut self, mut work: ReadWork, cancel: &AtomicBool) -> TaskStep {
        match self.owner.data[0].poll(&mut work, cancel) {
            TaskPoll::Pending => TaskStep::Pending,
            TaskPoll::Ready(()) => TaskStep::Ready,
            TaskPoll::Cancelled => TaskStep::Cancelled,
            TaskPoll::Failed(error) => TaskStep::Failed(error),
        }
    }
    /// Worker-only, including failure/cancellation/refusal settlement. On
    /// navigation success expected cursor ownership moves into the result;
    /// otherwise the exact unconsumed request is returned for retry/reporting.
    pub fn into_parts(mut self) -> (LayoutEngine, S, Option<TaskRequest>, Option<TaskOutput>) {
        let mut state = self.owner.data.pop().expect("owned task state");
        state.engine.page_allocation = None;
        (state.engine, state.context.into_source(), state.request, state.output)
    }
    /// Fixed source-level stack envelope for ROOT's debug validation. Payload
    /// capacities are separate admitted heap allocations, never part of these
    /// sizes. No consuming step returns a task-sized enum anymore.
    pub fn stack_envelope() -> TaskStackEnvelope {
        TaskStackEnvelope {
            owner: size_of::<Self>(),
            state: size_of::<VisualTaskState<S>>(),
            reader: size_of::<ReaderContinuation<S>>(),
            read_query: size_of::<S::ReadQuery>(),
            status: size_of::<TaskStep>(),
        }
    }
}
#[derive(Debug)]
pub struct TaskStackEnvelope {
    pub owner: usize,
    pub state: usize,
    pub reader: usize,
    pub read_query: usize,
    pub status: usize,
}
impl<S: CooperativeSource> VisualTaskState<S> {
    fn seek(&mut self, kind: impl FnOnce() -> SeekKind, work: &mut ReadWork, cancel: &AtomicBool) -> TaskPoll<()> {
        if self.seek.is_none() {
            self.seek = Some(SeekContinuation::new(kind()));
        }
        self.seek.as_mut().expect("active seek").poll(&mut self.engine, &mut self.context, work, cancel)
    }
    fn take_seek(&mut self) -> Option<BuiltRow> {
        self.seek.take().expect("completed seek").result
    }
    fn command(&self) -> NavigationCommand {
        match self.request.as_ref().expect("navigation request") {
            TaskRequest::Navigate { command, .. } => *command,
            _ => unreachable!("navigation phase"),
        }
    }
    fn poll(&mut self, work: &mut ReadWork, cancel: &AtomicBool) -> TaskPoll<()> {
        loop {
            ready!(self.context.check(work, cancel));
            match self.phase {
                TaskPhase::Start => match self.request.as_ref().expect("task request") {
                    TaskRequest::Fill { .. } => self.phase = TaskPhase::FillStart,
                    TaskRequest::Scroll { delta, .. } => {
                        self.remaining = delta.unsigned_abs();
                        self.phase =
                            if self.key.geometry.is_empty() { TaskPhase::FillStart } else { TaskPhase::ScrollLocate };
                    }
                    TaskRequest::Navigate { .. } => {
                        if self.key.geometry.is_empty() {
                            return TaskPoll::Failed(WrapError::Invalid("navigation in empty geometry").into());
                        }
                        self.phase = TaskPhase::NavigationStart;
                    }
                },
                TaskPhase::ScrollLocate => {
                    let point = SourcePoint { byte: self.anchor.source_byte, affinity: self.anchor.affinity };
                    ready!(self.seek(|| SeekKind::Locate(point), work, cancel));
                    self.row = self.take_seek();
                    self.phase = TaskPhase::ScrollMove;
                }
                TaskPhase::ScrollMove => {
                    if self.remaining == 0 {
                        self.anchor.source_byte = self.row.as_ref().expect("scroll row").row.source_coverage.start;
                        self.anchor.affinity = CaretAffinity::Downstream;
                        self.row = None;
                        self.phase = TaskPhase::FillStart;
                        continue;
                    }
                    let row = self.row.as_ref().expect("scroll row").row.clone();
                    let forward = matches!(self.request, Some(TaskRequest::Scroll { delta, .. }) if delta > 0);
                    ready!(self.seek(
                        || if forward { SeekKind::Next(row) } else { SeekKind::Previous(row) },
                        work,
                        cancel
                    ));
                    match self.take_seek() {
                        Some(row) => {
                            self.row = Some(row);
                            self.remaining -= 1;
                        }
                        None => self.remaining = 0,
                    }
                }
                TaskPhase::NavigationStart => {
                    let expected = match self.request.as_ref().expect("navigation request") {
                        TaskRequest::Navigate { expected, .. } => expected,
                        _ => unreachable!(),
                    };
                    ready!(self.credit.poll(
                        ReadWork::new(
                            (expected.cursors.len() * size_of::<crate::visual_rows::VisualCursor>()) as u64,
                            expected.cursors.len() as u64
                        ),
                        work,
                        cancel
                    ));
                    self.targets = match CursorSet::new(&self.engine.budget, expected.cursors(), expected.primary) {
                        Ok(targets) => Some(targets),
                        Err(error) => return TaskPoll::Failed(error.into()),
                    };
                    self.phase = TaskPhase::NavigationLocate;
                }
                TaskPhase::NavigationLocate => {
                    let targets = self.targets.as_ref().expect("navigation targets");
                    if self.cursor == targets.cursors.len() {
                        self.phase = TaskPhase::NavigationNormalize;
                        continue;
                    }
                    let target = targets.cursors[self.cursor];
                    if target.head > self.context.metadata.len || target.anchor > self.context.metadata.len {
                        return TaskPoll::Failed(WrapError::Invalid("cursor outside source").into());
                    }
                    ready!(self.seek(
                        || SeekKind::Locate(SourcePoint { byte: target.head, affinity: target.head_affinity }),
                        work,
                        cancel
                    ));
                    self.row = self.take_seek();
                    self.goal = self.row.as_ref().expect("navigation row").atoms.last().map_or(0, |a| a.cells.end);
                    self.atom_index = 0;
                    self.phase = TaskPhase::NavigationGoal;
                }
                TaskPhase::NavigationGoal => {
                    let target = self.targets.as_ref().expect("targets").cursors[self.cursor];
                    let row = self.row.as_ref().expect("navigation row");
                    while self.atom_index < row.atoms.len() {
                        ready!(self.credit.poll(ReadWork::new(size_of::<PaintAtom>() as u64, 1), work, cancel));
                        let atom = &row.atoms[self.atom_index];
                        self.atom_index += 1;
                        if target.head > atom.source.start && target.head < atom.source.end {
                            self.goal = match atom.known_caret_cell(target.head) {
                                Some(goal) => goal,
                                None => {
                                    return TaskPoll::Failed(
                                        WrapError::Invalid("cursor has no exact prepared scalar boundary").into(),
                                    );
                                }
                            };
                            break;
                        }
                        if target.head == atom.source.start {
                            self.goal = atom.cells.start;
                            break;
                        }
                    }
                    self.goal = target.desired_cell.unwrap_or(self.goal);
                    let command = self.command();
                    self.remaining = u64::from(command.count.max(1))
                        * if matches!(command.motion, VisualMotion::PageUp | VisualMotion::PageDown) {
                            u64::from(self.key.geometry.rows)
                        } else {
                            1
                        };
                    self.phase = TaskPhase::NavigationMove;
                }
                TaskPhase::NavigationMove => {
                    if self.remaining == 0 {
                        self.atom_index = 0;
                        self.phase = TaskPhase::NavigationTarget;
                        continue;
                    }
                    let row = self.row.as_ref().expect("navigation row").row.clone();
                    let forward = matches!(self.command().motion, VisualMotion::Down | VisualMotion::PageDown);
                    ready!(self.seek(
                        || if forward { SeekKind::Next(row) } else { SeekKind::Previous(row) },
                        work,
                        cancel
                    ));
                    match self.take_seek() {
                        Some(row) => {
                            self.row = Some(row);
                            self.remaining -= 1;
                        }
                        None => self.remaining = 0,
                    }
                }
                TaskPhase::NavigationTarget => {
                    let row = self.row.as_ref().expect("navigation row");
                    let mut point =
                        SourcePoint { byte: row.row.source_coverage.end, affinity: CaretAffinity::Upstream };
                    while self.atom_index < row.atoms.len() {
                        ready!(self.credit.poll(ReadWork::new(size_of::<PaintAtom>() as u64, 1), work, cancel));
                        let atom = &row.atoms[self.atom_index];
                        self.atom_index += 1;
                        if self.goal < atom.cells.end {
                            point = SourcePoint { byte: atom.source.start, affinity: CaretAffinity::Downstream };
                            break;
                        }
                    }
                    let extend = self.command().extend;
                    let target = &mut self.targets.as_mut().expect("targets").cursors.data[self.cursor];
                    target.head = point.byte;
                    target.head_affinity = point.affinity;
                    target.desired_cell = Some(self.goal);
                    if !extend {
                        target.anchor = point.byte;
                        target.anchor_affinity = point.affinity;
                    }
                    self.cursor += 1;
                    self.row = None;
                    self.phase = TaskPhase::NavigationLocate;
                }
                TaskPhase::NavigationNormalize => {
                    let targets = self.targets.as_ref().expect("targets");
                    let count = targets.cursors.len() as u64;
                    // Existing normalization is bounded by MAX_CURSORS=4096.
                    // Fund its sort, merge and old+new copies explicitly. It
                    // performs no source work and is not an unlimited bridge.
                    let visits = count * (u64::from(u64::BITS - count.leading_zeros()) + 8) * 4;
                    ready!(self.credit.poll(
                        ReadWork::new(visits * size_of::<crate::visual_rows::VisualCursor>() as u64, visits),
                        work,
                        cancel
                    ));
                    let normalized = match targets.normalized(&self.engine.budget) {
                        Ok(value) => value,
                        Err(error) => return TaskPoll::Failed(error.into()),
                    };
                    let primary = normalized.cursors[normalized.primary];
                    self.anchor =
                        TopAnchor { source_byte: primary.head, affinity: primary.head_affinity, intra_row_offset: 0 };
                    self.targets = Some(normalized);
                    self.phase = TaskPhase::FillStart;
                }
                TaskPhase::FillStart => {
                    let aggregate_metadata = size_of::<BudgetState>() + 2 * size_of::<usize>();
                    let header = match self.engine.budget.reserve(size_of::<LayoutPage>() + aggregate_metadata) {
                        Ok(value) => value,
                        Err(error) => return TaskPoll::Failed(error.into()),
                    };
                    let aggregate = AllocationBudget::new(MAX_PAGE_BYTES);
                    let page = (|| -> Result<LayoutPage, WrapError> {
                        Ok(LayoutPage {
                            key: self.key,
                            anchor: self.anchor,
                            rows: AccountedVec::with_aggregate(
                                &self.engine.budget,
                                &aggregate,
                                MAX_ROWS + 2 * MAX_OVERSCAN,
                                MAX_PAGE_BYTES,
                            )?,
                            atoms: AccountedVec::with_aggregate(
                                &self.engine.budget,
                                &aggregate,
                                MAX_ATOMS,
                                MAX_PAGE_BYTES,
                            )?,
                            syntax: AccountedVec::with_aggregate(
                                &self.engine.budget,
                                &aggregate,
                                MAX_SYNTAX_WINDOWS,
                                MAX_SYNTAX_WINDOWS * size_of::<Range<u64>>(),
                            )?,
                            at_start: false,
                            at_end: false,
                            _header: header,
                            _aggregate_header: aggregate.reserve(size_of::<LayoutPage>())?,
                        })
                    })();
                    self.page = match page {
                        Ok(page) => Some(page),
                        Err(error) => return TaskPoll::Failed(error.into()),
                    };
                    self.engine.page_allocation = Some(aggregate);
                    self.phase = if self.key.geometry.is_empty() { TaskPhase::Finish } else { TaskPhase::FillLocate };
                }
                TaskPhase::FillLocate => {
                    let point = SourcePoint { byte: self.anchor.source_byte, affinity: self.anchor.affinity };
                    ready!(self.seek(|| SeekKind::Locate(point), work, cancel));
                    self.row = self.take_seek();
                    let page = self.page.as_mut().expect("fill page");
                    page.anchor.source_byte = self.row.as_ref().expect("fill row").row.source_coverage.start;
                    page.anchor.affinity = CaretAffinity::Downstream;
                    page.at_start = page.anchor.source_byte == 0;
                    self.phase = TaskPhase::FillAppend;
                }
                TaskPhase::FillAppend => {
                    let row = self.row.as_ref().expect("fill row");
                    let page = self.page.as_mut().expect("fill page");
                    let relocation = if page.atoms.len() + row.atoms.len() > page.atoms.data.capacity() {
                        page.atoms.len()
                    } else {
                        0
                    };
                    let row_relocation = if page.rows.len() == page.rows.data.capacity() { page.rows.len() } else { 0 };
                    ready!(self.credit.poll(
                        ReadWork::new(
                            ((row.atoms.len() + relocation) * size_of::<PaintAtom>()
                                + (1 + row_relocation) * size_of::<VisualRow>()) as u64,
                            row.atoms.len() as u64 + 1
                        ),
                        work,
                        cancel
                    ));
                    if let Err(error) = page.rows.reserve_len(page.rows.len() + 1) {
                        return TaskPoll::Failed(error.into());
                    }
                    let Some(length) = page.atoms.len().checked_add(row.atoms.len()) else {
                        return TaskPoll::Failed(WrapError::Admission("page atom overflow").into());
                    };
                    if let Err(error) = page.atoms.reserve_len(length) {
                        return TaskPoll::Failed(error.into());
                    }
                    let mut row = self.row.take().expect("admitted fill row");
                    let first = page.atoms.len();
                    page.atoms.data.append(&mut row.atoms.data);
                    row.row.atoms = first..page.atoms.len();
                    page.rows.data.push(row.row);
                    let last = page.rows.last().expect("appended row");
                    if last.hard_line_end && last.logical_line + 1 == self.context.metadata.line_count {
                        page.at_end = true;
                    }
                    self.phase = if page.at_end || page.rows.len() == usize::from(self.key.geometry.rows) {
                        TaskPhase::Syntax
                    } else {
                        TaskPhase::FillNext
                    };
                }
                TaskPhase::FillNext => {
                    let last = self.page.as_ref().expect("fill page").rows.last().expect("last fill row").clone();
                    ready!(self.seek(|| SeekKind::Next(last), work, cancel));
                    self.row = self.take_seek();
                    if self.row.is_none() {
                        return TaskPoll::Failed(WrapError::Invalid("missing next visual row").into());
                    }
                    self.phase = TaskPhase::FillAppend;
                }
                TaskPhase::Syntax => {
                    let page = self.page.as_mut().expect("fill page");
                    while self.syntax_index < page.atoms.len() {
                        let relocation = if page.syntax.len() == page.syntax.data.capacity() {
                            page.syntax.len() * size_of::<Range<u64>>()
                        } else {
                            0
                        };
                        ready!(self.credit.poll(
                            ReadWork::new((size_of::<PaintAtom>() + relocation) as u64, 1),
                            work,
                            cancel
                        ));
                        let atom = &page.atoms[self.syntax_index];
                        self.syntax_index += 1;
                        if atom.kind == AtomKind::Opaque {
                            continue;
                        }
                        let bytes = match usize::try_from(atom.source.end - atom.source.start) {
                            Ok(bytes) => bytes,
                            Err(_) => return TaskPoll::Failed(WrapError::Admission("syntax span overflow").into()),
                        };
                        let Some(next) = self.syntax_bytes.checked_add(bytes) else { break };
                        if next > MAX_SYNTAX_BYTES {
                            break;
                        }
                        if let Some(last) = page.syntax.data.last_mut()
                            && last.end == atom.source.start
                            && atom.source.end - last.start <= SOURCE_WINDOW as u64
                        {
                            last.end = atom.source.end;
                            self.syntax_bytes = next;
                            continue;
                        }
                        if page.syntax.len() == MAX_SYNTAX_WINDOWS {
                            break;
                        }
                        if let Err(error) = page.syntax.push(atom.source.clone()) {
                            return TaskPoll::Failed(error.into());
                        }
                        self.syntax_bytes = next;
                    }
                    self.phase = TaskPhase::Finish;
                }
                TaskPhase::Finish => {
                    // Charge the actual carrier layout and construction/return
                    // overlap BEFORE consuming any page/request/cursor owner.
                    // This reservation travels with the output after task
                    // settlement, including through a wrong-tag extraction.
                    let output_header = match self.engine.budget.reserve_metadata(3 * size_of::<TaskOutput>()) {
                        Ok(header) => header,
                        Err(error) => return TaskPoll::Failed(error.into()),
                    };
                    if matches!(self.request, Some(TaskRequest::Navigate { .. })) {
                        let header = match self.engine.budget.reserve(size_of::<ResolvedNavigation>()) {
                            Ok(value) => value,
                            Err(error) => return TaskPoll::Failed(error.into()),
                        };
                        let Some(TaskRequest::Navigate { fence, expected, .. }) = self.request.take() else {
                            unreachable!()
                        };
                        self.output = Some(TaskOutput::navigation(
                            self.page.take().expect("reveal page"),
                            NavigationOutput {
                                fence,
                                expected,
                                targets: self.targets.take().expect("normalized targets"),
                                header,
                            },
                            output_header,
                        ));
                    } else {
                        self.output = Some(TaskOutput::page(self.page.take().expect("fill page"), output_header));
                    }
                    self.engine.page_allocation = None;
                    self.phase = TaskPhase::Done;
                }
                TaskPhase::Done => return TaskPoll::Ready(()),
            }
        }
    }
}
impl<S: CooperativeSource> SeekContinuation<S> {
    fn new(kind: SeekKind) -> Self {
        Self {
            kind,
            point: None,
            rank: None,
            logical: 0,
            cp: None,
            checkpoint_index: 0,
            building: None,
            result: None,
            phase: SeekPhase::Start,
            credit: WorkCredit::default(),
        }
    }
    fn poll(
        &mut self,
        engine: &mut LayoutEngine,
        context: &mut TaskContext<S>,
        work: &mut ReadWork,
        cancel: &AtomicBool,
    ) -> TaskPoll<()> {
        loop {
            ready!(context.check(work, cancel));
            match self.phase {
                SeekPhase::Start => match &self.kind {
                    SeekKind::Locate(point) => {
                        self.point = Some(*point);
                        self.phase = SeekPhase::Rank;
                    }
                    SeekKind::Previous(row) => {
                        if row.source_coverage.start == 0 || !row.continuation && row.logical_line == 0 {
                            self.phase = SeekPhase::Done;
                        } else if row.continuation {
                            self.point = Some(SourcePoint {
                                byte: row.source_coverage.start,
                                affinity: CaretAffinity::Upstream,
                            });
                            self.phase = SeekPhase::Rank;
                        } else {
                            self.logical = row.logical_line - 1;
                            self.phase = SeekPhase::PreviousLine;
                        }
                    }
                    SeekKind::Next(row) => {
                        if row.hard_line_end {
                            if row.logical_line + 1 == context.metadata.line_count {
                                self.phase = SeekPhase::Done;
                            } else {
                                self.logical = row.logical_line + 1;
                                self.phase = SeekPhase::Line;
                            }
                        } else {
                            self.cp = Some(Checkpoint {
                                byte: row.source_coverage.end,
                                logical_line: row.logical_line,
                                ordinal: row.ordinal_in_line + 1,
                            });
                            self.phase = SeekPhase::Build;
                        }
                    }
                },
                SeekPhase::Rank => {
                    let point = self.point.expect("seek point");
                    if point.byte > context.metadata.len {
                        return TaskPoll::Failed(WrapError::Invalid("source point outside document").into());
                    }
                    let query = self.rank.get_or_insert_with(|| context.source.new_rank_query(point.byte));
                    let rank = ready!(context.source.poll_rank(query, work, cancel));
                    if rank.line >= context.metadata.line_count {
                        return TaskPoll::Failed(WrapError::Invalid("source rank outside document").into());
                    }
                    self.logical = rank.line;
                    self.rank = None;
                    self.phase = SeekPhase::Line;
                }
                SeekPhase::PreviousLine => {
                    let line = ready!(context.reader.line(&context.source, self.logical, work, cancel));
                    self.point = Some(SourcePoint { byte: line.content_end, affinity: CaretAffinity::Upstream });
                    self.phase = SeekPhase::Rank;
                }
                SeekPhase::Line => {
                    let line = ready!(context.reader.line(&context.source, self.logical, work, cancel));
                    self.cp = Some(Checkpoint { byte: line.start, logical_line: self.logical, ordinal: 0 });
                    self.phase = if self.point.is_some() { SeekPhase::Checkpoints } else { SeekPhase::Build };
                }
                SeekPhase::Checkpoints => {
                    let point = self.point.expect("locate point");
                    while self.checkpoint_index < engine.checkpoints.len() {
                        ready!(self.credit.poll(ReadWork::new(size_of::<Checkpoint>() as u64, 1), work, cancel));
                        let cp = engine.checkpoints[self.checkpoint_index];
                        if cp.logical_line == self.logical
                            && (cp.byte < point.byte
                                || cp.byte == point.byte && point.affinity == CaretAffinity::Downstream)
                            && cp.byte >= self.cp.expect("initial checkpoint").byte
                        {
                            self.cp = Some(cp);
                        }
                        self.checkpoint_index += 1;
                    }
                    self.phase = SeekPhase::Build;
                }
                SeekPhase::Build => {
                    if self.building.is_none() {
                        self.building = match RowContinuation::new(engine, self.cp.expect("seek checkpoint")) {
                            Ok(row) => Some(row),
                            Err(error) => return TaskPoll::Failed(error.into()),
                        };
                    }
                    ready!(self.building.as_mut().expect("seek row").poll(engine, context, work, cancel));
                    self.result = Some(self.building.take().expect("completed seek row").into_row());
                    self.phase = SeekPhase::Decide;
                }
                SeekPhase::Decide => {
                    let row = &self.result.as_ref().expect("located row").row;
                    if let Some(point) = self.point
                        && !(point.byte < row.source_coverage.end
                            || row.hard_line_end
                            || point.byte == row.source_coverage.end && point.affinity == CaretAffinity::Upstream)
                    {
                        self.cp = Some(Checkpoint {
                            byte: row.source_coverage.end,
                            logical_line: row.logical_line,
                            ordinal: row.ordinal_in_line + 1,
                        });
                        self.result = None;
                        self.phase = SeekPhase::Build;
                    } else {
                        self.phase = SeekPhase::Done;
                    }
                }
                SeekPhase::Done => return TaskPoll::Ready(()),
            }
        }
    }
}

#[cfg(test)]
#[path = "visual_task_tests.rs"]
mod tests;
