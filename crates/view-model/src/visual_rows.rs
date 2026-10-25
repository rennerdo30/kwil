//! Exact source coordinates and bounded prepared visual rows (decision 0003).
//!
//! ROOT hooks: `pub mod visual_rows; pub mod wrap_session;` in lib.rs. Raw
//! `View::line/line_source` are deliberately not changed. Wrapped renderers use
//! `LayoutPage::rows/atoms/syntax_windows`, never re-layout display payloads.
#[path = "visual_task.rs"]
pub mod task;

#[cfg(test)]
use crate::grid::{GraphemeStep, GraphemeStepper};
use kwil_core_text::Snapshot;
// Public source-contract types let frontend fixtures implement concrete query
// adapters without adding a core-text dependency or altering manifests.
pub use kwil_core_text::compare_input::{InputError, LineSpan, Rank, ReadWork, Terminator};
use std::{
    fmt, io,
    mem::size_of,
    ops::{ControlFlow, Deref, Range},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
pub use task::{
    CooperativeSource, Provenance, SourceMetadata, TaskBytes, TaskError, TaskOutput, TaskPoll, TaskRequest, TaskStep,
    VisualTask, WorkCredit,
};
#[cfg(test)]
use unicode_width::UnicodeWidthStr;

pub const MAX_COLUMNS: usize = 4096;
pub const MAX_ROWS: usize = 4096;
pub const SOURCE_WINDOW: usize = 64 * 1024;
pub const DISPLAY_BYTES: usize = 256;
pub const MAX_PAGE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_ATOMS: usize = 262_144;
pub const MAX_OVERSCAN: usize = 8;
pub const MAX_CHECKPOINTS: usize = 4096;
pub const MAX_CHECKPOINT_BYTES: usize = 1024 * 1024;
pub const MAX_SYNTAX_WINDOWS: usize = 128;
pub const MAX_SYNTAX_BYTES: usize = 256 * 1024;
pub const MAX_CURSORS: usize = 4096;
pub const MAX_SESSION_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug)]
pub enum WrapError {
    Admission(&'static str),
    Invalid(&'static str),
    Io(io::Error),
    /// Preserve the original typed source failure and its I/O payload.
    Source(InputError),
    Cancelled,
    Stale,
    Busy,
    Closed,
    TimedOut,
    WorkerPanicked,
}
impl fmt::Display for WrapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for WrapError {}
impl From<io::Error> for WrapError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
impl From<TaskError> for WrapError {
    fn from(error: TaskError) -> Self {
        match error {
            TaskError::Layout(error) => error,
            TaskError::Source(error) => Self::Source(error),
        }
    }
}
pub type WrapResult<T> = Result<T, WrapError>;

/// All wrap allocations in a host share this counter, including old+new buffers
/// during growth. Source storage is owned by the existing document subsystem.
#[derive(Clone)]
pub struct AllocationBudget(Arc<BudgetState>);
struct BudgetState {
    used: AtomicUsize,
    peak: AtomicUsize,
    limit: usize,
    #[cfg(test)]
    allocations: AtomicUsize,
}
impl Default for AllocationBudget {
    fn default() -> Self {
        Self::new(MAX_SESSION_BYTES)
    }
}
impl AllocationBudget {
    pub fn new(limit: usize) -> Self {
        let overhead = size_of::<BudgetState>() + 2 * size_of::<usize>();
        Self(Arc::new(BudgetState {
            used: AtomicUsize::new(overhead),
            peak: AtomicUsize::new(overhead),
            limit: limit.min(MAX_SESSION_BYTES),
            #[cfg(test)]
            allocations: AtomicUsize::new(0),
        }))
    }
    pub fn retained_bytes(&self) -> usize {
        self.0.used.load(Ordering::Acquire)
    }
    pub fn peak_bytes(&self) -> usize {
        self.0.peak.load(Ordering::Acquire)
    }
    /// Reserve fixed frontend ownership metadata before constructing its cells.
    /// Keep this move-only token beside those cells through refusal and cleanup.
    /// Source buffers and allocation growth still require their own admission.
    pub fn reserve_metadata(&self, bytes: usize) -> WrapResult<MetadataReservation> {
        let total = bytes
            .checked_add(size_of::<MetadataReservation>())
            .ok_or(WrapError::Admission("metadata reservation overflow"))?;
        Ok(MetadataReservation { charge: self.reserve(total)? })
    }
    fn available_bytes(&self) -> usize {
        self.0.limit.saturating_sub(self.retained_bytes())
    }
    #[cfg(test)]
    pub(crate) fn allocation_attempts(&self) -> usize {
        self.0.allocations.load(Ordering::Acquire)
    }
    pub(crate) fn reserve(&self, bytes: usize) -> WrapResult<AllocationCharge> {
        let mut before = self.retained_bytes();
        loop {
            let after = before.checked_add(bytes).ok_or(WrapError::Admission("session byte overflow"))?;
            if after > self.0.limit {
                return Err(WrapError::Admission("wrap session allocation limit"));
            }
            match self.0.used.compare_exchange_weak(before, after, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => {
                    self.0.peak.fetch_max(after, Ordering::AcqRel);
                    return Ok(AllocationCharge { budget: self.clone(), bytes });
                }
                Err(current) => before = current,
            }
        }
    }
}
/// Fixed metadata charge in the same aggregate used by document and wrap lanes.
/// It neither allocates payload storage nor creates a second allowance. Moving
/// an owner must move its reservation; only retiring that owner releases it.
#[must_use = "retain beside the admitted metadata until its owners are retired"]
pub struct MetadataReservation {
    charge: AllocationCharge,
}
impl MetadataReservation {
    /// Includes this token's own header, charged before it is constructed.
    pub fn reserved_bytes(&self) -> usize {
        self.charge.bytes
    }
}
pub(crate) struct AllocationCharge {
    budget: AllocationBudget,
    bytes: usize,
}

impl AllocationCharge {
    pub(crate) fn retained_bytes(&self) -> usize {
        self.budget.retained_bytes()
    }
    pub(crate) fn reserve_transfer(&self, bytes: usize) -> WrapResult<Self> {
        self.budget.reserve(bytes)
    }
    #[cfg(test)]
    pub(crate) fn record_transfer_allocation(&self) {
        self.budget.0.allocations.fetch_add(1, Ordering::AcqRel);
    }
    #[cfg(test)]
    pub(crate) fn charged_bytes(&self) -> usize {
        self.bytes
    }
    fn reduce_to(&mut self, bytes: usize) {
        assert!(bytes <= self.bytes);
        self.budget.0.used.fetch_sub(self.bytes - bytes, Ordering::AcqRel);
        self.bytes = bytes;
    }
}
impl Drop for AllocationCharge {
    fn drop(&mut self) {
        self.budget.0.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

/// Growth reserves the entire permitted replacement allocation BEFORE asking
/// Vec for storage, while the old allocation remains charged. Actual capacity
/// is inspected before moving any elements. No infallible Vec growth is used.
pub(crate) struct AccountedVec<T> {
    pub(crate) data: Vec<T>,
    charge: AllocationCharge,
    max_bytes: usize,
    max_count: usize,
    aggregate: Option<AllocationBudget>,
    aggregate_charge: Option<AllocationCharge>,
}
impl<T> AccountedVec<T> {
    pub(crate) fn new(budget: &AllocationBudget, max_count: usize, max_bytes: usize) -> WrapResult<Self> {
        Ok(Self {
            data: Vec::new(),
            charge: budget.reserve(0)?,
            max_bytes,
            max_count,
            aggregate: None,
            aggregate_charge: None,
        })
    }
    pub(crate) fn with_aggregate(
        budget: &AllocationBudget,
        aggregate: &AllocationBudget,
        max_count: usize,
        max_bytes: usize,
    ) -> WrapResult<Self> {
        let mut value = Self::new(budget, max_count, max_bytes)?;
        value.aggregate = Some(aggregate.clone());
        value.aggregate_charge = Some(aggregate.reserve(0)?);
        Ok(value)
    }
    pub(crate) fn reserve_len(&mut self, length: usize) -> WrapResult<()> {
        if length > self.max_count {
            return Err(WrapError::Admission("record count limit"));
        }
        if length <= self.data.capacity() {
            return Ok(());
        }
        let size = size_of::<T>().max(1);
        // The old allocation stays charged in BOTH budgets until replacement.
        // Admit the aggregate peak before even asking the allocator for storage.
        let admitted_bytes =
            self.aggregate.as_ref().map_or(self.max_bytes, |budget| self.max_bytes.min(budget.available_bytes()));
        let cap = admitted_bytes / size;
        if length > cap {
            return Err(WrapError::Admission("record byte limit"));
        }
        let wanted = length.max(self.data.capacity().saturating_mul(2).max(4)).min(cap).min(self.max_count);
        let mut aggregate_charge = self.aggregate.as_ref().map(|budget| budget.reserve(admitted_bytes)).transpose()?;
        let mut replacement_charge = self.charge.budget.reserve(admitted_bytes)?;
        let mut replacement = Vec::new();
        #[cfg(test)]
        {
            self.charge.budget.0.allocations.fetch_add(1, Ordering::AcqRel);
            if let Some(aggregate) = &self.aggregate {
                aggregate.0.allocations.fetch_add(1, Ordering::AcqRel);
            }
        }
        replacement.try_reserve_exact(wanted).map_err(|_| WrapError::Admission("record allocation failed"))?;
        let actual = replacement.capacity().checked_mul(size).ok_or(WrapError::Admission("capacity byte overflow"))?;
        if actual > admitted_bytes {
            return Err(WrapError::Admission("allocator exceeded admitted capacity"));
        }
        replacement_charge.reduce_to(actual);
        if let Some(charge) = &mut aggregate_charge {
            charge.reduce_to(actual);
        }
        replacement.append(&mut self.data);
        self.data = replacement;
        self.charge = replacement_charge;
        self.aggregate_charge = aggregate_charge;
        Ok(())
    }
    pub(crate) fn push(&mut self, value: T) -> WrapResult<()> {
        self.reserve_len(self.data.len().checked_add(1).ok_or(WrapError::Admission("record count overflow"))?)?;
        self.data.push(value);
        Ok(())
    }
    pub(crate) fn bytes(&self) -> usize {
        self.charge.bytes
    }
    pub(crate) fn into_parts(self) -> (Vec<T>, AllocationCharge) {
        // Only non-page owners may transfer storage into a different wrapper.
        assert!(self.aggregate.is_none());
        (self.data, self.charge)
    }
}
impl<T> Deref for AccountedVec<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.data
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct SourceIdentity {
    pub document_id: u64,
    pub source_epoch: u64,
    pub incarnation: u64,
    pub revision: u64,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct LayoutGeometry {
    pub columns: u16,
    pub rows: u16,
    pub tab_size: u16,
    pub font_generation: u64,
    pub width_policy_generation: u64,
    pub settings_generation: u64,
}
impl LayoutGeometry {
    pub fn validate(self) -> WrapResult<()> {
        if usize::from(self.columns) > MAX_COLUMNS || usize::from(self.rows) > MAX_ROWS || self.tab_size == 0 {
            return Err(WrapError::Invalid("invalid wrap geometry"));
        }
        Ok(())
    }
    pub fn is_empty(self) -> bool {
        self.columns == 0 || self.rows == 0
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum BodyOwner {
    #[default]
    Text,
    Raw,
    MarkdownPreview,
    LogProjection,
    EditorLoan,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct LayoutKey {
    pub source: SourceIdentity,
    pub input_epoch: u64,
    pub layout_generation: u64,
    pub body_generation: u64,
    pub body: BodyOwner,
    pub geometry: LayoutGeometry,
}
impl LayoutKey {
    pub fn validate(self) -> WrapResult<()> {
        self.geometry.validate()?;
        if self.body != BodyOwner::Text {
            return Err(WrapError::Stale);
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CaretAffinity {
    Upstream,
    #[default]
    Downstream,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TopAnchor {
    pub source_byte: u64,
    pub affinity: CaretAffinity,
    pub intra_row_offset: u16,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AtomKind {
    Normal,
    InvalidByte,
    Opaque,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaintAtom {
    pub source: Range<u64>,
    pub cells: Range<u16>,
    pub kind: AtomKind,
    payload: [u8; DISPLAY_BYTES],
    payload_len: u16,
    whitespace: bool,
    tab: bool,
    byte_exact: bool,
}
impl PaintAtom {
    pub fn text(&self) -> &str {
        std::str::from_utf8(&self.payload[..usize::from(self.payload_len)]).expect("prepared UTF-8 payload")
    }
    pub fn is_tab(&self) -> bool {
        self.tab
    }
    pub fn is_whitespace(&self) -> bool {
        self.whitespace
    }
    /// Compatibility for a CURRENT byte cursor produced by scalar Left/Right.
    /// It does not authorize hits or NEW navigation targets inside a cluster.
    pub fn known_caret_cell(&self, byte: u64) -> Option<u16> {
        if byte == self.source.start {
            return Some(self.cells.start);
        }
        if byte == self.source.end {
            return Some(self.cells.end);
        }
        if byte < self.source.start
            || byte > self.source.end
            || self.kind != AtomKind::Normal
            || !self.byte_exact
            || self.source.end - self.source.start != u64::from(self.payload_len)
        {
            return None;
        }
        let local = usize::try_from(byte - self.source.start).ok()?;
        self.text().is_char_boundary(local).then_some(self.cells.start)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisualRow {
    pub logical_line: u64,
    pub source_coverage: Range<u64>,
    pub continuation: bool,
    pub hard_line_end: bool,
    pub ordinal_in_line: u64,
    pub atoms: Range<usize>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VisualPoint {
    pub row: usize,
    pub cell: u16,
    pub affinity: CaretAffinity,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourcePoint {
    pub byte: u64,
    pub affinity: CaretAffinity,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScrollExtent {
    pub source_position: u64,
    pub source_total: u64,
    pub exact_visual_total: bool,
}

/// Allocation-free damage description for both hosts. `scroll` is visual-row
/// displacement; overlays add their own bounded cursor/search/syntax damage.
pub struct PageDelta<'a> {
    pub full: bool,
    pub scroll: i64,
    previous: Option<&'a LayoutPage>,
    current: &'a LayoutPage,
}
impl PageDelta<'_> {
    pub fn changed_rows(&self) -> impl Iterator<Item = usize> + '_ {
        (0..usize::from(self.current.key.geometry.rows)).filter(move |&row| {
            if self.full {
                return true;
            }
            let Some(previous) = self.previous else { return true };
            let before = row as i64 + self.scroll;
            before < 0
                || before >= previous.rows.len() as i64
                || row >= self.current.rows.len()
                || !self.current.same_row(row, previous, before as usize)
        })
    }
}

pub struct LayoutPage {
    pub key: LayoutKey,
    pub anchor: TopAnchor,
    rows: AccountedVec<VisualRow>,
    atoms: AccountedVec<PaintAtom>,
    syntax: AccountedVec<Range<u64>>,
    pub at_start: bool,
    pub at_end: bool,
    _aggregate_header: AllocationCharge,
    _header: AllocationCharge,
}
impl fmt::Debug for LayoutPage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LayoutPage")
            .field("key", &self.key)
            .field("anchor", &self.anchor)
            .field("rows", &self.rows.data)
            .field("retained_bytes", &self.retained_bytes())
            .finish()
    }
}
impl LayoutPage {
    pub fn delta_from<'a>(&'a self, previous: Option<&'a Self>) -> PageDelta<'a> {
        let mut delta = PageDelta { full: true, scroll: 0, previous, current: self };
        let Some(previous) = previous.filter(|previous| previous.key == self.key) else { return delta };
        let candidate = self
            .rows
            .first()
            .and_then(|first| {
                previous.rows.iter().position(|row| {
                    row.logical_line == first.logical_line && row.source_coverage == first.source_coverage
                })
            })
            .map(|index| index as i64)
            .or_else(|| {
                previous
                    .rows
                    .first()
                    .and_then(|first| {
                        self.rows.iter().position(|row| {
                            row.logical_line == first.logical_line && row.source_coverage == first.source_coverage
                        })
                    })
                    .map(|index| -(index as i64))
            });
        let Some(scroll) = candidate else { return delta };
        if scroll.unsigned_abs() >= u64::from(self.key.geometry.rows.max(1)) {
            return delta;
        }
        let translation = (0..self.rows.len()).all(|row| {
            let before = row as i64 + scroll;
            before < 0 || before >= previous.rows.len() as i64 || self.same_row(row, previous, before as usize)
        });
        if translation {
            delta.full = false;
            delta.scroll = scroll;
        }
        delta
    }
    fn same_row(&self, row: usize, previous: &Self, before: usize) -> bool {
        let (current, old) = (&self.rows[row], &previous.rows[before]);
        current.logical_line == old.logical_line
            && current.source_coverage == old.source_coverage
            && current.continuation == old.continuation
            && current.hard_line_end == old.hard_line_end
            && self.atoms(row) == previous.atoms(before)
    }
    pub fn rows(&self) -> &[VisualRow] {
        &self.rows
    }
    pub fn atoms(&self, row: usize) -> &[PaintAtom] {
        self.rows.get(row).map_or(&[], |r| &self.atoms[r.atoms.clone()])
    }
    /// Admitted normal-atom windows, with ZERO additional syntax margin. ROOT's
    /// wrapped request policy must preserve the unwrapped policy separately.
    pub fn syntax_windows(&self) -> &[Range<u64>] {
        &self.syntax
    }
    pub fn check_syntax_request(&self, requested: Range<u64>, margin_bytes: usize) -> WrapResult<()> {
        if margin_bytes != 0 {
            return Err(WrapError::Admission("wrapped syntax has zero extra context"));
        }
        if requested.start > requested.end
            || requested.end - requested.start > SOURCE_WINDOW as u64
            || !self.syntax.iter().any(|admitted| admitted.start <= requested.start && requested.end <= admitted.end)
        {
            return Err(WrapError::Admission("syntax request is outside admitted normal atoms"));
        }
        Ok(())
    }
    pub fn retained_bytes(&self) -> usize {
        self._header.bytes + self.rows.bytes() + self.atoms.bytes() + self.syntax.bytes()
    }
    #[cfg(test)]
    pub(crate) fn aggregate_bytes(&self) -> usize {
        self._aggregate_header.retained_bytes()
    }
    #[cfg(test)]
    pub(crate) fn aggregate_peak_bytes(&self) -> usize {
        self._aggregate_header.budget.peak_bytes()
    }
    pub fn hit_test(&self, row: usize, cell: usize) -> Option<SourcePoint> {
        let r = self.rows.get(row)?;
        for atom in self.atoms(row) {
            if cell < usize::from(atom.cells.end) {
                return Some(SourcePoint { byte: atom.source.start, affinity: CaretAffinity::Downstream });
            }
        }
        Some(SourcePoint { byte: r.source_coverage.end, affinity: CaretAffinity::Upstream })
    }
    pub fn source_to_screen(&self, byte: u64, affinity: CaretAffinity) -> Option<VisualPoint> {
        for (row, r) in self.rows.iter().enumerate() {
            if byte < r.source_coverage.start || byte > r.source_coverage.end {
                continue;
            }
            if byte == r.source_coverage.end && !r.hard_line_end && affinity == CaretAffinity::Downstream {
                continue;
            }
            if byte == r.source_coverage.start && r.continuation && affinity == CaretAffinity::Upstream && row > 0 {
                continue;
            }
            for atom in self.atoms(row) {
                if byte == atom.source.start {
                    return Some(VisualPoint { row, cell: atom.cells.start, affinity });
                }
                if byte > atom.source.start && byte < atom.source.end {
                    return atom.known_caret_cell(byte).map(|cell| VisualPoint { row, cell, affinity });
                }
            }
            return Some(VisualPoint { row, cell: self.atoms(row).last().map_or(0, |a| a.cells.end), affinity });
        }
        None
    }
    /// Allocation-free selection/search painting, including opaque atom overlap.
    pub fn selection_cells(&self, row: usize, source: Range<u64>) -> impl Iterator<Item = Range<u16>> + '_ {
        self.atoms(row)
            .iter()
            .filter(move |a| source.start < source.end && source.start < a.source.end && a.source.start < source.end)
            .map(|a| a.cells.clone())
    }
    pub fn scroll_extent(&self, source_total: u64) -> ScrollExtent {
        ScrollExtent { source_position: self.anchor.source_byte, source_total, exact_visual_total: false }
    }
}

/// Cached immutable transport metadata. run_layout consumes the EXACT Arc on
/// the existing worker and recovers it in WorkerExit, without cloning it.
/// Source-specific query types remain concrete inside VisualTask<S>.
pub trait WrapSource: Send + Sync + 'static {
    fn identity(&self) -> SourceIdentity;
    fn len(&self) -> u64;
    fn line_count(&self) -> u64;
    fn source_metadata(&self) -> SourceMetadata {
        SourceMetadata { identity: self.identity(), len: self.len(), line_count: self.line_count() }
    }
    fn run_layout(
        self: Arc<Self>,
        engine: LayoutEngine,
        request: TaskRequest,
        driver: &mut dyn TaskDriver,
    ) -> WorkerExit;
    #[cfg(test)]
    fn run_test(
        &self,
        engine: LayoutEngine,
        request: TaskRequest,
        control: &dyn LayoutControl,
    ) -> (LayoutEngine, WrapResult<TaskOutput>) {
        fixtures::drive(fixtures::Source::ordinary(self), engine, request, control)
    }
    // Independent reference oracle ONLY. Production has no synchronous source
    // operation and cannot dispatch through these fixture methods.
    #[cfg(test)]
    fn line_start(&self, line: u64) -> io::Result<Option<u64>>;
    #[cfg(test)]
    fn line_of(&self, byte: u64) -> io::Result<u64>;
    #[cfg(test)]
    fn read(&self, at: u64, output: &mut [u8]) -> io::Result<usize>;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Borrowed erasure only: no query/task Box and no source-owning erased slot.
pub trait TaskStepper {
    fn step(&mut self, work: ReadWork, cancel: &AtomicBool) -> TaskStep;
    fn read_bytes(&self) -> u64;
}
impl<S: CooperativeSource> TaskStepper for VisualTask<S> {
    fn step(&mut self, work: ReadWork, cancel: &AtomicBool) -> TaskStep {
        VisualTask::step(self, work, cancel)
    }
    fn read_bytes(&self) -> u64 {
        VisualTask::read_bytes(self)
    }
}
pub trait TaskDriver {
    /// Only active, finite-work computation is resumed. Pending is work-credit
    /// exhaustion, never external readiness, a timer, or an idle event poll.
    fn drive(&mut self, task: &mut dyn TaskStepper) -> TaskEnd;
}
pub enum TaskEnd {
    Ready,
    Cancelled,
    Failed(TaskError),
}
pub enum WorkerEnd {
    Refused(WrapError),
    Finished(TaskEnd),
}
#[must_use]
pub struct WorkerExit {
    pub engine: LayoutEngine,
    pub source: Arc<dyn WrapSource>,
    pub request: Option<TaskRequest>,
    pub output: Option<TaskOutput>,
    pub end: WorkerEnd,
}
pub trait SessionSource: CooperativeSource {
    fn into_transport(self) -> Arc<dyn WrapSource>;
}
/// Caller pre-admits driver/return-carrier construction overlap (WrapSession's
/// fixed allocation). Task/query storage is admitted by begin_task in the SAME
/// aggregate. All paths below, including refusal, execute on that worker.
pub fn run_cooperative<S: SessionSource>(
    source: S,
    engine: LayoutEngine,
    request: TaskRequest,
    driver: &mut dyn TaskDriver,
) -> WorkerExit {
    let mut task = match engine.begin_task(source, request) {
        ControlFlow::Break(refused) => {
            return WorkerExit {
                engine: refused.engine,
                source: refused.source.into_transport(),
                request: Some(refused.request),
                output: None,
                end: WorkerEnd::Refused(refused.error),
            };
        }
        ControlFlow::Continue(task) => task,
    };
    let end = driver.drive(&mut task);
    // into_parts destroys ALL continuation/query owners before returning. No
    // query or borrowed snapshot may escape with the transport Arc to the UI.
    let (engine, source, request, output) = task.into_parts();
    WorkerExit { engine, source: source.into_transport(), request, output, end: WorkerEnd::Finished(end) }
}

/// Snapshot transport helper. The existing Arc moves INTO the worker-only
/// adapter and back out. No Snapshot clone, Arc clone, unwrap or downcast.
pub fn run_snapshot<T: WrapSource + crate::snapshot_layout_source::SnapshotOwner>(
    owner: Arc<T>,
    engine: LayoutEngine,
    request: TaskRequest,
    driver: &mut dyn TaskDriver,
) -> WorkerExit {
    let identity = owner.identity();
    let source =
        match crate::snapshot_layout_source::SnapshotLayoutSource::try_with_owner(owner, identity, engine.budget()) {
            Ok(source) => source,
            Err(refused) => {
                return WorkerExit {
                    engine,
                    source: refused.owner,
                    request: Some(request),
                    output: None,
                    end: WorkerEnd::Refused(refused.error),
                };
            }
        };
    run_cooperative(source, engine, request, driver)
}
impl<T: WrapSource + crate::snapshot_layout_source::SnapshotOwner> SessionSource
    for crate::snapshot_layout_source::SnapshotLayoutSource<Arc<T>>
{
    fn into_transport(self) -> Arc<dyn WrapSource> {
        self.into_owner()
    }
}
pub struct SnapshotSource {
    identity: SourceIdentity,
    snapshot: Snapshot,
}
impl SnapshotSource {
    pub fn new(identity: SourceIdentity, snapshot: Snapshot) -> Self {
        Self { identity, snapshot }
    }
}
impl WrapSource for SnapshotSource {
    fn identity(&self) -> SourceIdentity {
        self.identity
    }
    fn len(&self) -> u64 {
        self.snapshot.len()
    }
    fn line_count(&self) -> u64 {
        self.snapshot.line_count()
    }
    fn run_layout(
        self: Arc<Self>,
        engine: LayoutEngine,
        request: TaskRequest,
        driver: &mut dyn TaskDriver,
    ) -> WorkerExit {
        run_snapshot(self, engine, request, driver)
    }
    #[cfg(test)]
    fn run_test(
        &self,
        engine: LayoutEngine,
        request: TaskRequest,
        control: &dyn LayoutControl,
    ) -> (LayoutEngine, WrapResult<TaskOutput>) {
        match crate::snapshot_layout_source::SnapshotLayoutSource::try_with_owner(self, self.identity, engine.budget())
        {
            Ok(source) => fixtures::drive(source, engine, request, control),
            Err(refused) => (engine, Err(refused.error)),
        }
    }
    #[cfg(test)]
    fn line_start(&self, line: u64) -> io::Result<Option<u64>> {
        self.snapshot.line_start(line).map_err(io::Error::other)
    }
    #[cfg(test)]
    fn line_of(&self, byte: u64) -> io::Result<u64> {
        self.snapshot
            .offset_to_line_col(byte)
            .map_err(io::Error::other)?
            .map(|p| p.0)
            .ok_or_else(|| io::Error::other("wrap offset outside snapshot"))
    }
    #[cfg(test)]
    fn read(&self, at: u64, output: &mut [u8]) -> io::Result<usize> {
        if output.len() > SOURCE_WINDOW {
            return Err(io::Error::other("wrap read exceeds source window"));
        }
        let end =
            at.checked_add(output.len() as u64).ok_or_else(|| io::Error::other("wrap read overflow"))?.min(self.len());
        let mut copied = 0;
        for chunk in self.snapshot.chunks(at..end)? {
            let chunk = chunk.map_err(io::Error::other)?;
            output[copied..copied + chunk.len()].copy_from_slice(chunk);
            copied += chunk.len();
        }
        Ok(copied)
    }
}
impl crate::snapshot_layout_source::SnapshotOwner for SnapshotSource {
    fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }
}

pub trait LayoutControl {
    fn cancelled(&self) -> bool;
    fn progress(&self, scanned: u64, target: u64);
}
pub struct NoCancellation;
impl LayoutControl for NoCancellation {
    fn cancelled(&self) -> bool {
        false
    }
    fn progress(&self, _: u64, _: u64) {}
}

#[cfg(test)]
struct Reader<'a> {
    source: &'a dyn WrapSource,
    control: &'a dyn LayoutControl,
    bytes: [u8; SOURCE_WINDOW],
    start: u64,
    length: usize,
    chunk_limit: usize,
    read_bytes: u64,
    line_cache: Option<(u64, u64, u64)>,
}
#[cfg(test)]
impl<'a> Reader<'a> {
    fn new(source: &'a dyn WrapSource, control: &'a dyn LayoutControl, chunk_limit: usize) -> Self {
        Self {
            source,
            control,
            bytes: [0; SOURCE_WINDOW],
            start: 0,
            length: 0,
            chunk_limit,
            read_bytes: 0,
            line_cache: None,
        }
    }
    fn check(&self) -> WrapResult<()> {
        if self.control.cancelled() { Err(WrapError::Cancelled) } else { Ok(()) }
    }
    fn byte(&mut self, at: u64) -> WrapResult<Option<u8>> {
        self.check()?;
        if at >= self.source.len() {
            return Ok(None);
        }
        if at < self.start || at - self.start >= self.length as u64 {
            self.start = at;
            let wanted = self.chunk_limit.min((self.source.len() - at).min(SOURCE_WINDOW as u64) as usize);
            self.length = self.source.read(at, &mut self.bytes[..wanted])?;
            if self.length == 0 || self.length > wanted {
                return Err(WrapError::Invalid("short/invalid source window"));
            }
            self.read_bytes = self
                .read_bytes
                .checked_add(self.length as u64)
                .ok_or(WrapError::Admission("source read progress overflow"))?;
            // Count actual bytes read, not the farthest offset touched: the
            // bounded EOL probe must not report a giant seek as 100% scanned.
            self.control.progress(self.read_bytes, self.source.len());
        }
        Ok(Some(self.bytes[(at - self.start) as usize]))
    }
    fn scalar(&mut self, at: u64, end: u64) -> WrapResult<Scalar> {
        let first = self.byte(at)?.ok_or(WrapError::Invalid("missing scalar"))?;
        let length = match first {
            0..=0x7f => 1,
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => 0,
        };
        let mut bytes = [0; 4];
        bytes[0] = first;
        if length == 0 || at.saturating_add(length as u64) > end {
            return Ok(Scalar { bytes, length: 1, valid: false });
        }
        for (i, byte) in bytes.iter_mut().enumerate().take(length).skip(1) {
            *byte = self.byte(at + i as u64)?.ok_or(WrapError::Invalid("truncated scalar"))?;
        }
        let valid = std::str::from_utf8(&bytes[..length]).is_ok();
        Ok(Scalar { bytes, length: if valid { length } else { 1 }, valid })
    }
    fn previous_scalar(&mut self, end: u64, start: u64) -> WrapResult<(u64, Scalar)> {
        for length in 1..=4u64 {
            if end < start.saturating_add(length) {
                break;
            }
            let at = end - length;
            let scalar = self.scalar(at, end)?;
            if scalar.valid && scalar.length as u64 == length {
                return Ok((at, scalar));
            }
        }
        Err(WrapError::Invalid("invalid grapheme pre-context"))
    }
    fn payload(&mut self, range: Range<u64>) -> WrapResult<([u8; DISPLAY_BYTES], u16)> {
        let length =
            usize::try_from(range.end - range.start).map_err(|_| WrapError::Admission("payload address overflow"))?;
        if length > DISPLAY_BYTES {
            return Err(WrapError::Admission("payload exceeds admitted display"));
        }
        let mut bytes = [0; DISPLAY_BYTES];
        for (i, byte) in bytes.iter_mut().enumerate().take(length) {
            *byte = self.byte(range.start + i as u64)?.ok_or(WrapError::Invalid("missing payload byte"))?;
        }
        Ok((bytes, length as u16))
    }
}
#[cfg(test)]
struct Scalar {
    bytes: [u8; 4],
    length: usize,
    valid: bool,
}
#[cfg(test)]
impl Scalar {
    fn text(&self) -> &str {
        std::str::from_utf8(&self.bytes[..self.length]).expect("validated scalar")
    }
}

#[cfg(test)]
struct ClusterStream {
    next: u64,
    end: u64,
    context_start: u64,
    stepper: GraphemeStepper,
}
#[cfg(test)]
impl ClusterStream {
    fn new(start: u64, end: u64) -> WrapResult<Self> {
        Ok(Self {
            next: start,
            end,
            context_start: start,
            stepper: GraphemeStepper::new(start, end).map_err(WrapError::Invalid)?,
        })
    }
    fn next(&mut self, reader: &mut Reader<'_>) -> WrapResult<Option<(Range<u64>, bool)>> {
        if self.next >= self.end {
            return Ok(None);
        }
        let start = self.next;
        let first = reader.scalar(start, self.end)?;
        if !first.valid {
            self.next += 1;
            self.context_start = self.next;
            self.stepper = GraphemeStepper::new(self.next, self.end).map_err(WrapError::Invalid)?;
            return Ok(Some((start..self.next, false)));
        }
        let mut at = start;
        loop {
            reader.check()?;
            let scalar = reader.scalar(at, self.end)?;
            if !scalar.valid {
                self.next = at;
                self.stepper = GraphemeStepper::new(at, self.end).map_err(WrapError::Invalid)?;
                self.context_start = at;
                return Ok(Some((start..at, true)));
            }
            loop {
                match self.stepper.step(at, scalar.text()).map_err(WrapError::Invalid)? {
                    GraphemeStep::Boundary(end) => {
                        self.next = end;
                        return Ok(Some((start..end, true)));
                    }
                    GraphemeStep::End => {
                        self.next = self.end;
                        return Ok(Some((start..self.end, true)));
                    }
                    GraphemeStep::Scalar(next) => {
                        at = next;
                        break;
                    }
                    GraphemeStep::Context(end) => {
                        let (context, scalar) = reader.previous_scalar(end, self.context_start)?;
                        self.stepper.provide_context(context, scalar.text()).map_err(WrapError::Invalid)?;
                    }
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Checkpoint {
    byte: u64,
    logical_line: u64,
    ordinal: u64,
}
pub struct LayoutEngine {
    budget: AllocationBudget,
    key: Option<LayoutKey>,
    checkpoints: AccountedVec<Checkpoint>,
    chunk_limit: usize,
    checkpoint_capacity: usize,
    page_allocation: Option<AllocationBudget>,
    _scratch: AllocationCharge,
}
impl LayoutEngine {
    pub fn new(budget: AllocationBudget) -> WrapResult<Self> {
        Self::with_chunk_limit(budget, SOURCE_WINDOW)
    }
    /// Smaller chunks are useful for deterministic boundary/short-read proofs.
    pub fn with_chunk_limit(budget: AllocationBudget, chunk_limit: usize) -> WrapResult<Self> {
        Self::with_limits(budget, chunk_limit, MAX_CHECKPOINTS)
    }
    pub fn with_limits(budget: AllocationBudget, chunk_limit: usize, checkpoint_capacity: usize) -> WrapResult<Self> {
        if !(1..=SOURCE_WINDOW).contains(&chunk_limit) {
            return Err(WrapError::Invalid("invalid source chunk limit"));
        }
        if !(1..=MAX_CHECKPOINTS).contains(&checkpoint_capacity) {
            return Err(WrapError::Invalid("invalid checkpoint capacity"));
        }
        let scratch = budget.reserve(size_of::<Self>())?;
        Ok(Self {
            checkpoints: AccountedVec::new(&budget, MAX_CHECKPOINTS, MAX_CHECKPOINT_BYTES)?,
            budget,
            key: None,
            chunk_limit,
            checkpoint_capacity,
            page_allocation: None,
            _scratch: scratch,
        })
    }
    pub fn checkpoint_count(&self) -> usize {
        self.checkpoints.len()
    }
    pub fn checkpoint_bytes(&self) -> usize {
        self.checkpoints.bytes()
    }
    pub fn budget(&self) -> &AllocationBudget {
        &self.budget
    }
    #[cfg(test)]
    fn prepare(&mut self, key: LayoutKey, source: &dyn WrapSource) -> WrapResult<()> {
        key.validate()?;
        if key.source != source.identity() || source.line_count() == 0 {
            return Err(WrapError::Stale);
        }
        if self.key != Some(key) {
            self.checkpoints.data.clear();
            self.key = Some(key);
        }
        Ok(())
    }
    fn remember(&mut self, cp: Checkpoint) -> WrapResult<()> {
        if self.checkpoints.contains(&cp) {
            return Ok(());
        }
        if self.checkpoints.len() == self.checkpoint_capacity {
            self.checkpoints.data.remove(0);
        }
        self.checkpoints.push(cp)
    }
    #[cfg(test)]
    fn line_bounds(reader: &mut Reader<'_>, logical: u64) -> WrapResult<Range<u64>> {
        reader.check()?;
        if let Some((cached, start, end)) = reader.line_cache
            && cached == logical
        {
            return Ok(start..end);
        }
        let start = reader.source.line_start(logical)?.ok_or(WrapError::Invalid("missing logical line"))?;
        let mut end = match reader.source.line_start(logical + 1)? {
            Some(next) => next.checked_sub(1).ok_or(WrapError::Invalid("invalid line index"))?,
            None => reader.source.len(),
        };
        if end > start && reader.byte(end - 1)? == Some(b'\r') {
            end -= 1;
        }
        if start > end || end > reader.source.len() {
            return Err(WrapError::Invalid("reversed logical line"));
        }
        reader.line_cache = Some((logical, start, end));
        Ok(start..end)
    }
    #[cfg(test)]
    fn atom(
        reader: &mut Reader<'_>,
        range: Range<u64>,
        valid: bool,
        column: usize,
        geometry: LayoutGeometry,
    ) -> WrapResult<PaintAtom> {
        let mut payload = [0; DISPLAY_BYTES];
        let mut length: u16;
        let mut kind = AtomKind::Normal;
        let mut byte_exact = valid;
        let (width, whitespace, tab) = if !valid {
            let byte = reader.byte(range.start)?.ok_or(WrapError::Invalid("invalid byte missing"))?;
            const HEX: &[u8] = b"0123456789ABCDEF";
            payload[..4].copy_from_slice(&[b'\\', b'x', HEX[(byte >> 4) as usize], HEX[(byte & 15) as usize]]);
            length = 4;
            kind = AtomKind::InvalidByte;
            byte_exact = false;
            (4, false, false)
        } else if range.end - range.start > DISPLAY_BYTES as u64 {
            payload[0] = b'?';
            length = 1;
            kind = AtomKind::Opaque;
            byte_exact = false;
            (1, false, false)
        } else {
            (payload, length) = reader.payload(range.clone())?;
            let text = std::str::from_utf8(&payload[..usize::from(length)])
                .map_err(|_| WrapError::Invalid("non-UTF8 prepared cluster"))?;
            if text == "\t" {
                (usize::from(geometry.tab_size) - column % usize::from(geometry.tab_size), true, true)
            } else if text.chars().any(char::is_control) {
                byte_exact = false;
                let byte = payload[0];
                const HEX: &[u8] = b"0123456789ABCDEF";
                payload[..4].copy_from_slice(&[b'\\', b'x', HEX[(byte >> 4) as usize], HEX[(byte & 15) as usize]]);
                length = 4;
                (4, false, false)
            } else {
                (UnicodeWidthStr::width(text).max(1), text.chars().all(char::is_whitespace), false)
            }
        };
        let end = column.checked_add(width).ok_or(WrapError::Admission("cell overflow"))?;
        Ok(PaintAtom {
            source: range,
            cells: column as u16..end.min(u16::MAX as usize) as u16,
            kind,
            payload,
            payload_len: length,
            whitespace,
            tab,
            byte_exact,
        })
    }
    #[cfg(test)]
    fn row(&mut self, reader: &mut Reader<'_>, cp: Checkpoint) -> WrapResult<BuiltRow> {
        let geometry = self.key.expect("prepared key").geometry;
        let line = Self::line_bounds(reader, cp.logical_line)?;
        let mut atoms = match &self.page_allocation {
            Some(aggregate) => AccountedVec::with_aggregate(&self.budget, aggregate, MAX_COLUMNS + 1, MAX_PAGE_BYTES)?,
            None => AccountedVec::new(&self.budget, MAX_COLUMNS + 1, MAX_PAGE_BYTES)?,
        };
        let mut stream = ClusterStream::new(cp.byte, line.end)?;
        let (mut column, mut whitespace_break) = (0usize, None);
        let mut end = cp.byte;
        while let Some((range, valid)) = stream.next(reader)? {
            let atom = Self::atom(reader, range, valid, column, geometry)?;
            let next_column = usize::from(atom.cells.end);
            if next_column > usize::from(geometry.columns) && !atoms.is_empty() {
                if !atom.whitespace
                    && let Some((count, byte)) = whitespace_break
                {
                    atoms.data.truncate(count);
                    end = byte;
                }
                break;
            }
            end = atom.source.end;
            column = next_column;
            let white = atom.whitespace;
            atoms.push(atom)?;
            if white {
                whitespace_break = Some((atoms.len(), end));
            }
            if column > usize::from(geometry.columns) {
                break;
            }
        }
        // Clipping does not change the original source atom's endpoints.
        for atom in &mut atoms.data {
            atom.cells.end = atom.cells.end.min(geometry.columns);
        }
        let row = VisualRow {
            logical_line: cp.logical_line,
            source_coverage: cp.byte..end,
            continuation: cp.byte != line.start,
            hard_line_end: end == line.end,
            ordinal_in_line: cp.ordinal,
            atoms: 0..atoms.len(),
        };
        if end == cp.byte && cp.byte != line.end {
            return Err(WrapError::Invalid("wrap failed to advance"));
        }
        self.remember(cp)?;
        Ok(BuiltRow { row, atoms })
    }
    #[cfg(test)]
    fn next_checkpoint(reader: &Reader<'_>, row: &VisualRow) -> Option<Checkpoint> {
        if row.hard_line_end {
            (row.logical_line + 1 < reader.source.line_count()).then(|| Checkpoint {
                byte: 0,
                logical_line: row.logical_line + 1,
                ordinal: 0,
            })
        } else {
            Some(Checkpoint {
                byte: row.source_coverage.end,
                logical_line: row.logical_line,
                ordinal: row.ordinal_in_line + 1,
            })
        }
    }
    #[cfg(test)]
    fn next_row(&mut self, reader: &mut Reader<'_>, row: &VisualRow) -> WrapResult<Option<BuiltRow>> {
        let Some(mut cp) = Self::next_checkpoint(reader, row) else { return Ok(None) };
        if row.hard_line_end {
            cp.byte = Self::line_bounds(reader, cp.logical_line)?.start;
        }
        self.row(reader, cp).map(Some)
    }
    #[cfg(test)]
    fn locate(&mut self, reader: &mut Reader<'_>, point: SourcePoint) -> WrapResult<BuiltRow> {
        if point.byte > reader.source.len() {
            return Err(WrapError::Invalid("source point outside document"));
        }
        let logical = reader.source.line_of(point.byte)?;
        let line = Self::line_bounds(reader, logical)?;
        let cp = self
            .checkpoints
            .iter()
            .filter(|cp| {
                cp.logical_line == logical
                    && (cp.byte < point.byte || cp.byte == point.byte && point.affinity == CaretAffinity::Downstream)
            })
            .max_by_key(|cp| cp.byte)
            .copied()
            .unwrap_or(Checkpoint { byte: line.start, logical_line: logical, ordinal: 0 });
        let mut row = self.row(reader, cp)?;
        loop {
            let end = row.row.source_coverage.end;
            if point.byte < end
                || row.row.hard_line_end
                || point.byte == end && point.affinity == CaretAffinity::Upstream
            {
                return Ok(row);
            }
            row = self.next_row(reader, &row.row)?.ok_or(WrapError::Invalid("missing continuation"))?;
        }
    }
    #[cfg(test)]
    fn previous_row(&mut self, reader: &mut Reader<'_>, row: &VisualRow) -> WrapResult<Option<BuiltRow>> {
        if row.source_coverage.start == 0 {
            return Ok(None);
        }
        if row.continuation {
            return self
                .locate(reader, SourcePoint { byte: row.source_coverage.start, affinity: CaretAffinity::Upstream })
                .map(Some);
        }
        if row.logical_line == 0 {
            return Ok(None);
        }
        let previous = Self::line_bounds(reader, row.logical_line - 1)?;
        self.locate(reader, SourcePoint { byte: previous.end, affinity: CaretAffinity::Upstream }).map(Some)
    }
    #[cfg(test)]
    pub fn reference_fill(
        &mut self,
        key: LayoutKey,
        source: &dyn WrapSource,
        anchor: TopAnchor,
        control: &dyn LayoutControl,
    ) -> WrapResult<LayoutPage> {
        self.prepare(key, source)?;
        // The independent page budget includes its own counter metadata and
        // every vector's old+replacement capacities, not just final lengths.
        let aggregate_metadata = size_of::<BudgetState>() + 2 * size_of::<usize>();
        let header = self.budget.reserve(size_of::<LayoutPage>() + aggregate_metadata)?;
        let aggregate = AllocationBudget::new(MAX_PAGE_BYTES);
        self.page_allocation = Some(aggregate.clone());
        let result = self.fill_admitted(key, source, anchor, control, aggregate, header);
        // All error paths release this temporary construction scope as well.
        self.page_allocation = None;
        result
    }
    #[cfg(test)]
    fn fill_admitted(
        &mut self,
        key: LayoutKey,
        source: &dyn WrapSource,
        anchor: TopAnchor,
        control: &dyn LayoutControl,
        aggregate: AllocationBudget,
        header: AllocationCharge,
    ) -> WrapResult<LayoutPage> {
        let aggregate_header = aggregate.reserve(size_of::<LayoutPage>())?;
        let mut page = LayoutPage {
            key,
            anchor,
            rows: AccountedVec::with_aggregate(&self.budget, &aggregate, MAX_ROWS + 2 * MAX_OVERSCAN, MAX_PAGE_BYTES)?,
            atoms: AccountedVec::with_aggregate(&self.budget, &aggregate, MAX_ATOMS, MAX_PAGE_BYTES)?,
            syntax: AccountedVec::with_aggregate(
                &self.budget,
                &aggregate,
                MAX_SYNTAX_WINDOWS,
                MAX_SYNTAX_WINDOWS * size_of::<Range<u64>>(),
            )?,
            at_start: false,
            at_end: false,
            _header: header,
            _aggregate_header: aggregate_header,
        };
        if key.geometry.is_empty() {
            return Ok(page);
        }
        let mut reader = Reader::new(source, control, self.chunk_limit);
        let mut row = self.locate(&mut reader, SourcePoint { byte: anchor.source_byte, affinity: anchor.affinity })?;
        page.anchor.source_byte = row.row.source_coverage.start;
        // An anchor identifies this selected row, not the original cursor's
        // side of a wrap boundary. Its normalized start must refill that row.
        page.anchor.affinity = CaretAffinity::Downstream;
        page.at_start = page.anchor.source_byte == 0;
        for _ in 0..key.geometry.rows {
            page.rows.reserve_len(page.rows.len() + 1)?;
            page.atoms.reserve_len(
                page.atoms.len().checked_add(row.atoms.len()).ok_or(WrapError::Admission("page atom overflow"))?,
            )?;
            let first = page.atoms.len();
            page.atoms.data.append(&mut row.atoms.data);
            row.row.atoms = first..page.atoms.len();
            page.rows.data.push(row.row);
            let last = page.rows.last().expect("appended row");
            if last.hard_line_end && last.logical_line + 1 == source.line_count() {
                page.at_end = true;
                break;
            }
            if page.rows.len() == usize::from(key.geometry.rows) {
                break;
            }
            row = self.next_row(&mut reader, last)?.ok_or(WrapError::Invalid("missing next visual row"))?;
        }
        let mut syntax_bytes = 0usize;
        for atom in &page.atoms.data {
            if atom.kind == AtomKind::Opaque {
                continue;
            }
            let bytes = usize::try_from(atom.source.end - atom.source.start)
                .map_err(|_| WrapError::Admission("syntax span overflow"))?;
            let Some(next_bytes) = syntax_bytes.checked_add(bytes) else { break };
            if next_bytes > MAX_SYNTAX_BYTES {
                break;
            }
            if let Some(last) = page.syntax.data.last_mut()
                && last.end == atom.source.start
                && atom.source.end - last.start <= SOURCE_WINDOW as u64
            {
                last.end = atom.source.end;
                syntax_bytes = next_bytes;
                continue;
            }
            if page.syntax.len() == MAX_SYNTAX_WINDOWS {
                break;
            }
            page.syntax.push(atom.source.clone())?;
            syntax_bytes = next_bytes;
        }
        Ok(page)
    }
    #[cfg(test)]
    pub fn reference_scroll(
        &mut self,
        key: LayoutKey,
        source: &dyn WrapSource,
        anchor: TopAnchor,
        delta: i64,
        control: &dyn LayoutControl,
    ) -> WrapResult<LayoutPage> {
        self.prepare(key, source)?;
        if key.geometry.is_empty() {
            return self.reference_fill(key, source, anchor, control);
        }
        let anchor = {
            let mut reader = Reader::new(source, control, self.chunk_limit);
            let mut row =
                self.locate(&mut reader, SourcePoint { byte: anchor.source_byte, affinity: anchor.affinity })?;
            for _ in 0..delta.unsigned_abs() {
                reader.check()?;
                let next = if delta > 0 {
                    self.next_row(&mut reader, &row.row)?
                } else {
                    self.previous_row(&mut reader, &row.row)?
                };
                match next {
                    Some(next) => row = next,
                    None => break,
                }
            }
            TopAnchor {
                source_byte: row.row.source_coverage.start,
                affinity: CaretAffinity::Downstream,
                intra_row_offset: anchor.intra_row_offset,
            }
        };
        self.reference_fill(key, source, anchor, control)
    }
}
struct BuiltRow {
    row: VisualRow,
    atoms: AccountedVec<PaintAtom>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VisualMotion {
    Up,
    Down,
    PageUp,
    PageDown,
}
/// One visual movement operation; its count and selection-extension policy
/// travel together through worker resolution without changing FIFO semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NavigationCommand {
    pub motion: VisualMotion,
    pub count: u32,
    pub extend: bool,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VisualCursor {
    pub anchor: u64,
    pub head: u64,
    pub anchor_affinity: CaretAffinity,
    pub head_affinity: CaretAffinity,
    pub desired_cell: Option<u16>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NavigationFence {
    pub key: LayoutKey,
    pub cursor_set_generation: u64,
    pub intent_generation: u64,
}
pub struct CursorSet {
    pub(crate) cursors: AccountedVec<VisualCursor>,
    pub primary: usize,
}
impl fmt::Debug for CursorSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CursorSet").field("cursors", &self.cursors.data).field("primary", &self.primary).finish()
    }
}
impl CursorSet {
    pub fn new(budget: &AllocationBudget, cursors: &[VisualCursor], primary: usize) -> WrapResult<Self> {
        if cursors.is_empty() || cursors.len() > MAX_CURSORS || primary >= cursors.len() {
            return Err(WrapError::Invalid("invalid visual cursor set"));
        }
        let mut data = AccountedVec::new(budget, MAX_CURSORS, MAX_CURSORS * size_of::<VisualCursor>())?;
        data.reserve_len(cursors.len())?;
        data.data.extend_from_slice(cursors);
        Ok(Self { cursors: data, primary })
    }
    pub fn cursors(&self) -> &[VisualCursor] {
        &self.cursors
    }
    pub fn retained_bytes(&self) -> usize {
        self.cursors.bytes()
    }
    pub fn normalized(&self, budget: &AllocationBudget) -> WrapResult<Self> {
        let mut ordered: AccountedVec<(VisualCursor, usize)> =
            AccountedVec::new(budget, MAX_CURSORS, MAX_CURSORS * size_of::<(VisualCursor, usize)>())?;
        ordered.reserve_len(self.cursors.len())?;
        ordered.data.extend(self.cursors.iter().copied().enumerate().map(|(index, cursor)| (cursor, index)));
        ordered.data.sort_unstable_by_key(|(c, index)| (c.anchor.min(c.head), c.anchor.max(c.head), *index));
        let mut merged: AccountedVec<VisualCursor> =
            AccountedVec::new(budget, MAX_CURSORS, MAX_CURSORS * size_of::<VisualCursor>())?;
        merged.reserve_len(ordered.len())?;
        let mut primary = 0;
        let mut primary_merged = false;
        for (cursor, original) in ordered.data.drain(..) {
            let is_primary = original == self.primary;
            let previous_is_primary = !is_primary && primary_merged && primary + 1 == merged.len();
            if let Some(last) = merged.data.last_mut() {
                let previous = *last;
                let last_start = last.anchor.min(last.head);
                let last_end = last.anchor.max(last.head);
                let start = cursor.anchor.min(cursor.head);
                let end = cursor.anchor.max(cursor.head);
                if start < last_end || start == last_start && end == last_end {
                    let reverse = if is_primary { cursor.head < cursor.anchor } else { last.head < last.anchor };
                    let a = last_start.min(start);
                    let b = last_end.max(end);
                    last.anchor = if reverse { b } else { a };
                    last.head = if reverse { a } else { b };
                    let (preferred, fallback) =
                        if previous_is_primary { (previous, cursor) } else { (cursor, previous) };
                    let affinity = |byte| {
                        if byte == preferred.head {
                            preferred.head_affinity
                        } else if byte == preferred.anchor {
                            preferred.anchor_affinity
                        } else if byte == fallback.head {
                            fallback.head_affinity
                        } else {
                            fallback.anchor_affinity
                        }
                    };
                    last.anchor_affinity = affinity(last.anchor);
                    last.head_affinity = affinity(last.head);
                    if is_primary {
                        last.desired_cell = cursor.desired_cell;
                        primary = merged.len() - 1;
                        primary_merged = true;
                    }
                    continue;
                }
            }
            if is_primary {
                primary = merged.len();
                primary_merged = true;
            }
            merged.data.push(cursor);
        }
        Ok(Self { cursors: merged, primary })
    }
}
pub struct ResolvedNavigation {
    pub fence: NavigationFence,
    pub expected: CursorSet,
    pub targets: CursorSet,
    pub reveal: LayoutPage,
    pub(crate) _header: AllocationCharge,
}
impl fmt::Debug for ResolvedNavigation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedNavigation").field("fence", &self.fence).field("targets", &self.targets).finish()
    }
}
#[cfg(test)]
impl LayoutEngine {
    pub fn reference_navigate(
        &mut self,
        fence: NavigationFence,
        source: &dyn WrapSource,
        expected: CursorSet,
        command: NavigationCommand,
        control: &dyn LayoutControl,
    ) -> WrapResult<ResolvedNavigation> {
        let NavigationCommand { motion, count, extend } = command;
        self.prepare(fence.key, source)?;
        if fence.key.geometry.is_empty() {
            return Err(WrapError::Invalid("navigation in empty geometry"));
        }
        let mut targets = CursorSet::new(&self.budget, expected.cursors(), expected.primary)?;
        {
            let mut reader = Reader::new(source, control, self.chunk_limit);
            for target in &mut targets.cursors.data {
                if target.head > source.len() || target.anchor > source.len() {
                    return Err(WrapError::Invalid("cursor outside source"));
                }
                let mut row =
                    self.locate(&mut reader, SourcePoint { byte: target.head, affinity: target.head_affinity })?;
                let mut goal = row.atoms.last().map_or(0, |a| a.cells.end);
                for atom in &row.atoms.data {
                    if target.head > atom.source.start && target.head < atom.source.end {
                        goal = atom
                            .known_caret_cell(target.head)
                            .ok_or(WrapError::Invalid("cursor has no exact prepared scalar boundary"))?;
                        break;
                    }
                    if target.head == atom.source.start {
                        goal = atom.cells.start;
                        break;
                    }
                }
                let goal = target.desired_cell.unwrap_or(goal);
                let distance = u64::from(count.max(1))
                    .checked_mul(if matches!(motion, VisualMotion::PageUp | VisualMotion::PageDown) {
                        u64::from(fence.key.geometry.rows)
                    } else {
                        1
                    })
                    .ok_or(WrapError::Admission("navigation distance overflow"))?;
                for _ in 0..distance {
                    reader.check()?;
                    let next = if matches!(motion, VisualMotion::Down | VisualMotion::PageDown) {
                        self.next_row(&mut reader, &row.row)?
                    } else {
                        self.previous_row(&mut reader, &row.row)?
                    };
                    match next {
                        Some(next) => row = next,
                        None => break,
                    }
                }
                let atom = row.atoms.iter().find(|a| goal < a.cells.end);
                let (head, affinity) = atom.map_or((row.row.source_coverage.end, CaretAffinity::Upstream), |a| {
                    (a.source.start, CaretAffinity::Downstream)
                });
                target.head = head;
                target.head_affinity = affinity;
                target.desired_cell = Some(goal);
                if !extend {
                    target.anchor = head;
                    target.anchor_affinity = affinity;
                }
            }
        }
        let targets = targets.normalized(&self.budget)?;
        let primary = targets.cursors[targets.primary];
        let reveal = self.reference_fill(
            fence.key,
            source,
            TopAnchor { source_byte: primary.head, affinity: primary.head_affinity, intra_row_offset: 0 },
            control,
        )?;
        let header = self.budget.reserve(size_of::<ResolvedNavigation>())?;
        Ok(ResolvedNavigation { fence, expected, targets, reveal, _header: header })
    }
}

/// Deterministic fixture bridge ONLY. Its old source operations are bounded
/// table/arithmetic or tiny immutable fixture operations, not production I/O.
/// There is deliberately no production blanket synchronous source adapter.
#[cfg(test)]
pub mod fixtures {
    use super::*;
    use kwil_core_text::compare_input::{LineSpan, Rank, Terminator};
    use std::{
        ops::Deref,
        sync::Mutex,
        thread::{self, ThreadId},
    };
    use task::{Provenance, TaskBytes, TaskPoll, WorkCredit};

    #[derive(Default)]
    pub struct Probe {
        pub created: AtomicUsize,
        pub polls: AtomicUsize,
        pub provenance: AtomicUsize,
        pub dropped: AtomicUsize,
        pub max_owners: AtomicUsize,
        pub drop_thread: Mutex<Option<ThreadId>>,
        pub mixed_drop_threads: AtomicBool,
    }
    pub trait Owner: Deref
    where
        Self::Target: WrapSource,
    {
        fn owner_count(&self) -> usize;
    }
    impl<T: WrapSource> Owner for Arc<T> {
        fn owner_count(&self) -> usize {
            Arc::strong_count(self)
        }
    }
    impl<T: WrapSource + ?Sized> Owner for &T {
        fn owner_count(&self) -> usize {
            0
        }
    }
    pub struct Source<O> {
        owner: O,
        probe: Option<Arc<Probe>>,
    }
    pub struct Query {
        at: u64,
        length: usize,
        credit: WorkCredit,
        staging: Option<TaskBytes>,
        returned: Option<usize>,
        probe: Option<Arc<Probe>>,
    }
    impl Drop for Query {
        fn drop(&mut self) {
            if let Some(probe) = &self.probe {
                probe.dropped.fetch_add(1, Ordering::Relaxed);
                let current = thread::current().id();
                let mut previous = probe.drop_thread.lock().unwrap();
                if previous.is_some_and(|previous| previous != current) {
                    probe.mixed_drop_threads.store(true, Ordering::Relaxed);
                }
                *previous = Some(current);
            }
        }
    }
    impl<O: Owner> Source<O>
    where
        O::Target: WrapSource,
    {
        pub fn ordinary(owner: O) -> Self {
            Self { owner, probe: None }
        }
        fn query(&self, at: u64, length: usize, staging: Option<TaskBytes>) -> Query {
            if let Some(probe) = &self.probe {
                probe.created.fetch_add(1, Ordering::Relaxed);
                probe.max_owners.fetch_max(self.owner.owner_count(), Ordering::Relaxed);
            }
            Query { at, length, credit: WorkCredit::default(), staging, returned: None, probe: self.probe.clone() }
        }
        fn access(&self) {
            if let Some(probe) = &self.probe {
                probe.polls.fetch_add(1, Ordering::Relaxed);
                probe.max_owners.fetch_max(self.owner.owner_count(), Ordering::Relaxed);
            }
        }
    }
    macro_rules! paid {
        ($value:expr) => {
            match $value {
                TaskPoll::Ready(value) => value,
                TaskPoll::Pending => return TaskPoll::Pending,
                TaskPoll::Cancelled => return TaskPoll::Cancelled,
                TaskPoll::Failed(error) => return TaskPoll::Failed(error),
            }
        };
    }
    fn io<T>(result: std::io::Result<T>) -> TaskPoll<T> {
        match result {
            Ok(value) => TaskPoll::Ready(value),
            Err(error) => TaskPoll::Failed(TaskError::Source(InputError::Io(error))),
        }
    }
    impl<O: Owner> CooperativeSource for Source<O>
    where
        O::Target: WrapSource,
    {
        type LineQuery = Query;
        type RankQuery = Query;
        type ReadQuery = Query;
        type ProvenanceQuery = Query;
        fn metadata(&self) -> SourceMetadata {
            WrapSource::source_metadata(&*self.owner)
        }
        fn new_line_query(&self, at: u64) -> Query {
            self.query(at, 0, None)
        }
        fn new_rank_query(&self, at: u64) -> Query {
            self.query(at, 0, None)
        }
        fn new_provenance_query(&self, at: u64) -> Query {
            self.query(at, 0, None)
        }
        fn new_read_query(&self, at: u64, length: usize, budget: &AllocationBudget) -> WrapResult<Query> {
            Ok(self.query(at, length, Some(TaskBytes::try_new(budget, length)?)))
        }
        fn poll_line(&self, q: &mut Query, work: &mut ReadWork, cancel: &AtomicBool) -> TaskPoll<LineSpan> {
            self.access();
            paid!(q.credit.poll(ReadWork::new(size_of::<LineSpan>() as u64 + 2, 4), work, cancel));
            let start = match paid!(io(self.owner.line_start(q.at))) {
                Some(start) => start,
                None => return TaskPoll::Failed(InputError::Range.into()),
            };
            let next = paid!(io(self.owner.line_start(q.at + 1)));
            let end = next.unwrap_or(self.owner.len());
            let mut terminator = Terminator::None;
            let mut content_end = end;
            if next.is_some() {
                content_end = end - 1;
                terminator = Terminator::Lf;
                if content_end > start {
                    let mut byte = [0];
                    let n = paid!(io(self.owner.read(content_end - 1, &mut byte)));
                    if n == 1 && byte[0] == b'\r' {
                        content_end -= 1;
                        terminator = Terminator::CrLf;
                    }
                }
            }
            TaskPoll::Ready(LineSpan { start, end, content_end, terminator })
        }
        fn poll_rank(&self, q: &mut Query, work: &mut ReadWork, cancel: &AtomicBool) -> TaskPoll<Rank> {
            self.access();
            paid!(q.credit.poll(ReadWork::new(16, 2), work, cancel));
            let line = paid!(io(self.owner.line_of(q.at)));
            let start = match paid!(io(self.owner.line_start(line))) {
                Some(start) => start,
                None => return TaskPoll::Failed(InputError::Range.into()),
            };
            TaskPoll::Ready(Rank { line, column: q.at - start })
        }
        fn poll_read(
            &self,
            q: &mut Query,
            output: &mut [u8],
            work: &mut ReadWork,
            cancel: &AtomicBool,
        ) -> TaskPoll<usize> {
            self.access();
            if q.length != output.len() {
                return TaskPoll::Failed(InputError::QueryChanged.into());
            }
            if q.returned.is_none() {
                paid!(q.credit.poll(ReadWork::new(q.length as u64, 1), work, cancel));
                let n = paid!(io(self.owner.read(q.at, q.staging.as_mut().unwrap())));
                if n > q.length {
                    return TaskPoll::Failed(InputError::SourceContract.into());
                }
                q.returned = Some(n);
            }
            let n = q.returned.unwrap();
            paid!(q.credit.poll(ReadWork::new(n as u64, 1), work, cancel));
            output[..n].copy_from_slice(&q.staging.as_ref().unwrap()[..n]);
            TaskPoll::Ready(n)
        }
        fn poll_provenance(&self, q: &mut Query, work: &mut ReadWork, cancel: &AtomicBool) -> TaskPoll<Provenance> {
            self.access();
            paid!(q.credit.poll(ReadWork::new(1, 1), work, cancel));
            if let Some(probe) = &self.probe {
                probe.provenance.fetch_add(1, Ordering::Relaxed);
            }
            // All sources opting into this FIXTURE helper declare ordinary
            // bytes. This is explicit fixture provenance, never marker sniffing.
            if q.at >= self.owner.len() {
                return TaskPoll::Failed(InputError::Range.into());
            }
            TaskPoll::Ready(Provenance { source: 0..self.owner.len(), opaque: false })
        }
    }
    impl<T: WrapSource> SessionSource for Source<Arc<T>> {
        fn into_transport(self) -> Arc<dyn WrapSource> {
            self.owner
        }
    }
    pub fn run<T: WrapSource>(
        owner: Arc<T>,
        engine: LayoutEngine,
        request: TaskRequest,
        driver: &mut dyn TaskDriver,
    ) -> WorkerExit {
        run_with_probe(owner, None, engine, request, driver)
    }
    struct Concrete<T>(Arc<T>);
    impl<T: WrapSource + CooperativeSource> CooperativeSource for Concrete<T> {
        type LineQuery = T::LineQuery;
        type RankQuery = T::RankQuery;
        type ReadQuery = T::ReadQuery;
        type ProvenanceQuery = T::ProvenanceQuery;
        fn metadata(&self) -> SourceMetadata {
            CooperativeSource::metadata(self.0.as_ref())
        }
        fn new_line_query(&self, line: u64) -> Self::LineQuery {
            self.0.new_line_query(line)
        }
        fn new_rank_query(&self, at: u64) -> Self::RankQuery {
            self.0.new_rank_query(at)
        }
        fn new_read_query(&self, at: u64, length: usize, budget: &AllocationBudget) -> WrapResult<Self::ReadQuery> {
            self.0.new_read_query(at, length, budget)
        }
        fn new_provenance_query(&self, at: u64) -> Self::ProvenanceQuery {
            self.0.new_provenance_query(at)
        }
        fn poll_line(
            &self,
            query: &mut Self::LineQuery,
            work: &mut ReadWork,
            cancel: &AtomicBool,
        ) -> TaskPoll<LineSpan> {
            self.0.poll_line(query, work, cancel)
        }
        fn poll_rank(&self, query: &mut Self::RankQuery, work: &mut ReadWork, cancel: &AtomicBool) -> TaskPoll<Rank> {
            self.0.poll_rank(query, work, cancel)
        }
        fn poll_read(
            &self,
            query: &mut Self::ReadQuery,
            output: &mut [u8],
            work: &mut ReadWork,
            cancel: &AtomicBool,
        ) -> TaskPoll<usize> {
            self.0.poll_read(query, output, work, cancel)
        }
        fn poll_provenance(
            &self,
            query: &mut Self::ProvenanceQuery,
            work: &mut ReadWork,
            cancel: &AtomicBool,
        ) -> TaskPoll<Provenance> {
            self.0.poll_provenance(query, work, cancel)
        }
    }
    impl<T: WrapSource + CooperativeSource> SessionSource for Concrete<T> {
        fn into_transport(self) -> Arc<dyn WrapSource> {
            self.0
        }
    }
    pub fn run_concrete<T: WrapSource + CooperativeSource>(
        owner: Arc<T>,
        engine: LayoutEngine,
        request: TaskRequest,
        driver: &mut dyn TaskDriver,
    ) -> WorkerExit {
        run_cooperative(Concrete(owner), engine, request, driver)
    }
    pub fn run_with_probe<T: WrapSource>(
        owner: Arc<T>,
        probe: Option<Arc<Probe>>,
        engine: LayoutEngine,
        request: TaskRequest,
        driver: &mut dyn TaskDriver,
    ) -> WorkerExit {
        run_cooperative(Source { owner, probe }, engine, request, driver)
    }
    /// Snapshot-backed fixture hooks execute at REAL production query access.
    /// No synchronous line/index/read operation substitutes for a core query.
    pub trait SnapshotHooks: WrapSource + crate::snapshot_layout_source::SnapshotOwner {
        fn before_snapshot_query(&self) {}
        fn before_snapshot_read(&self, length: usize) -> io::Result<()>;
    }
    struct Observed<Q> {
        core: Q,
        probe: Option<Arc<Probe>>,
        read_hooked: bool,
    }
    impl<Q> Drop for Observed<Q> {
        fn drop(&mut self) {
            if let Some(probe) = &self.probe {
                probe.dropped.fetch_add(1, Ordering::Relaxed);
                let current = thread::current().id();
                let mut previous = probe.drop_thread.lock().unwrap();
                if previous.is_some_and(|previous| previous != current) {
                    probe.mixed_drop_threads.store(true, Ordering::Relaxed);
                }
                *previous = Some(current);
            }
        }
    }
    type SnapshotAdapter<T> = crate::snapshot_layout_source::SnapshotLayoutSource<Arc<T>>;
    struct ObservedSnapshot<T> {
        inner: SnapshotAdapter<T>,
        probe: Option<Arc<Probe>>,
    }
    impl<T: SnapshotHooks> ObservedSnapshot<T> {
        fn query<Q>(&self, core: Q) -> Observed<Q> {
            if let Some(probe) = &self.probe {
                probe.created.fetch_add(1, Ordering::Relaxed);
                probe.max_owners.fetch_max(Arc::strong_count(self.inner.owner()), Ordering::Relaxed);
            }
            Observed { core, probe: self.probe.clone(), read_hooked: false }
        }
        fn access(&self) {
            self.inner.owner().before_snapshot_query();
            if let Some(probe) = &self.probe {
                probe.polls.fetch_add(1, Ordering::Relaxed);
                probe.max_owners.fetch_max(Arc::strong_count(self.inner.owner()), Ordering::Relaxed);
            }
        }
    }
    impl<T: SnapshotHooks> CooperativeSource for ObservedSnapshot<T> {
        type LineQuery = Observed<<SnapshotAdapter<T> as CooperativeSource>::LineQuery>;
        type RankQuery = Observed<<SnapshotAdapter<T> as CooperativeSource>::RankQuery>;
        type ReadQuery = Observed<<SnapshotAdapter<T> as CooperativeSource>::ReadQuery>;
        type ProvenanceQuery = Observed<<SnapshotAdapter<T> as CooperativeSource>::ProvenanceQuery>;
        fn metadata(&self) -> SourceMetadata {
            self.inner.metadata()
        }
        fn new_line_query(&self, line: u64) -> Self::LineQuery {
            self.query(self.inner.new_line_query(line))
        }
        fn new_rank_query(&self, at: u64) -> Self::RankQuery {
            self.query(self.inner.new_rank_query(at))
        }
        fn new_read_query(&self, at: u64, length: usize, budget: &AllocationBudget) -> WrapResult<Self::ReadQuery> {
            Ok(self.query(self.inner.new_read_query(at, length, budget)?))
        }
        fn new_provenance_query(&self, at: u64) -> Self::ProvenanceQuery {
            self.query(self.inner.new_provenance_query(at))
        }
        fn poll_line(&self, q: &mut Self::LineQuery, work: &mut ReadWork, cancel: &AtomicBool) -> TaskPoll<LineSpan> {
            self.access();
            self.inner.poll_line(&mut q.core, work, cancel)
        }
        fn poll_rank(&self, q: &mut Self::RankQuery, work: &mut ReadWork, cancel: &AtomicBool) -> TaskPoll<Rank> {
            self.access();
            self.inner.poll_rank(&mut q.core, work, cancel)
        }
        fn poll_read(
            &self,
            q: &mut Self::ReadQuery,
            output: &mut [u8],
            work: &mut ReadWork,
            cancel: &AtomicBool,
        ) -> TaskPoll<usize> {
            self.access();
            if !q.read_hooked {
                if let Err(error) = self.inner.owner().before_snapshot_read(output.len()) {
                    return TaskPoll::Failed(InputError::Io(error).into());
                }
                q.read_hooked = true;
            }
            self.inner.poll_read(&mut q.core, output, work, cancel)
        }
        fn poll_provenance(
            &self,
            q: &mut Self::ProvenanceQuery,
            work: &mut ReadWork,
            cancel: &AtomicBool,
        ) -> TaskPoll<Provenance> {
            self.access();
            let result = self.inner.poll_provenance(&mut q.core, work, cancel);
            if matches!(&result, TaskPoll::Ready(_))
                && let Some(probe) = &self.probe
            {
                probe.provenance.fetch_add(1, Ordering::Relaxed);
            }
            result
        }
    }
    impl<T: SnapshotHooks> SessionSource for ObservedSnapshot<T> {
        fn into_transport(self) -> Arc<dyn WrapSource> {
            self.inner.into_owner()
        }
    }
    pub fn run_snapshot<T: SnapshotHooks>(
        owner: Arc<T>,
        probe: Option<Arc<Probe>>,
        engine: LayoutEngine,
        request: TaskRequest,
        driver: &mut dyn TaskDriver,
    ) -> WorkerExit {
        let identity = owner.identity();
        match SnapshotAdapter::try_with_owner(owner, identity, engine.budget()) {
            Ok(inner) => run_cooperative(ObservedSnapshot { inner, probe }, engine, request, driver),
            Err(refused) => WorkerExit {
                engine,
                source: refused.owner,
                request: Some(request),
                output: None,
                end: WorkerEnd::Refused(refused.error),
            },
        }
    }
    struct Driver<'a> {
        control: &'a dyn LayoutControl,
        total: u64,
    }
    impl TaskDriver for Driver<'_> {
        fn drive(&mut self, task: &mut dyn TaskStepper) -> TaskEnd {
            let cancel = AtomicBool::new(false);
            loop {
                cancel.store(self.control.cancelled(), Ordering::Release);
                if cancel.load(Ordering::Acquire) {
                    return TaskEnd::Cancelled;
                }
                let step = task.step(ReadWork::new(SOURCE_WINDOW as u64, SOURCE_WINDOW as u64), &cancel);
                if self.control.cancelled() {
                    return TaskEnd::Cancelled;
                }
                self.control.progress(task.read_bytes(), self.total);
                match step {
                    TaskStep::Pending => {}
                    TaskStep::Ready => return TaskEnd::Ready,
                    TaskStep::Cancelled => return TaskEnd::Cancelled,
                    TaskStep::Failed(error) => return TaskEnd::Failed(error),
                }
            }
        }
    }
    pub fn drive<S: CooperativeSource>(
        source: S,
        engine: LayoutEngine,
        request: TaskRequest,
        control: &dyn LayoutControl,
    ) -> (LayoutEngine, WrapResult<TaskOutput>) {
        let total = source.metadata().len;
        let mut task = match engine.begin_task(source, request) {
            ControlFlow::Continue(task) => task,
            ControlFlow::Break(refused) => return (refused.engine, Err(refused.error)),
        };
        let end = Driver { control, total }.drive(&mut task);
        let (engine, source, request, output) = task.into_parts();
        drop((source, request));
        let result = match end {
            TaskEnd::Ready => output.ok_or(WrapError::Invalid("ready fixture without output")),
            TaskEnd::Cancelled => {
                drop(output);
                Err(WrapError::Cancelled)
            }
            TaskEnd::Failed(error) => {
                drop(output);
                Err(error.into())
            }
        };
        (engine, result)
    }
    pub(super) fn borrowed(
        engine: &mut LayoutEngine,
        source: &dyn WrapSource,
        request: TaskRequest,
        control: &dyn LayoutControl,
    ) -> WrapResult<TaskOutput> {
        // Actual temporary engine header, never an uncharged dummy allowance.
        let charge = engine.budget.reserve(size_of::<LayoutEngine>())?;
        let replacement = LayoutEngine {
            budget: engine.budget.clone(),
            key: None,
            checkpoints: AccountedVec::new(&engine.budget, MAX_CHECKPOINTS, MAX_CHECKPOINT_BYTES)?,
            chunk_limit: engine.chunk_limit,
            checkpoint_capacity: engine.checkpoint_capacity,
            page_allocation: None,
            _scratch: charge,
        };
        let owned = std::mem::replace(engine, replacement);
        let (recovered, result) = source.run_test(owned, request, control);
        *engine = recovered;
        result
    }
}

// Test-only convenience calls execute the cooperative engine; the separately
// named reference_* methods above remain an independent differential oracle.
#[cfg(test)]
impl LayoutEngine {
    pub fn fill(
        &mut self,
        key: LayoutKey,
        source: &dyn WrapSource,
        anchor: TopAnchor,
        control: &dyn LayoutControl,
    ) -> WrapResult<LayoutPage> {
        match fixtures::borrowed(self, source, TaskRequest::Fill { key, anchor }, control)?.into_page() {
            ControlFlow::Continue(page) => Ok(page),
            ControlFlow::Break(owner) => {
                drop(owner);
                Err(WrapError::Invalid("fixture page tag"))
            }
        }
    }
    pub fn scroll(
        &mut self,
        key: LayoutKey,
        source: &dyn WrapSource,
        anchor: TopAnchor,
        delta: i64,
        control: &dyn LayoutControl,
    ) -> WrapResult<LayoutPage> {
        match fixtures::borrowed(self, source, TaskRequest::Scroll { key, anchor, delta }, control)?.into_page() {
            ControlFlow::Continue(page) => Ok(page),
            ControlFlow::Break(owner) => {
                drop(owner);
                Err(WrapError::Invalid("fixture page tag"))
            }
        }
    }
    pub fn navigate(
        &mut self,
        fence: NavigationFence,
        source: &dyn WrapSource,
        expected: CursorSet,
        command: NavigationCommand,
        control: &dyn LayoutControl,
    ) -> WrapResult<ResolvedNavigation> {
        match fixtures::borrowed(self, source, TaskRequest::Navigate { fence, expected, command }, control)?
            .into_navigation()
        {
            ControlFlow::Continue(navigation) => Ok(navigation),
            ControlFlow::Break(owner) => {
                drop(owner);
                Err(WrapError::Invalid("fixture navigation tag"))
            }
        }
    }
}

#[cfg(test)]
mod metadata_reservation_tests {
    use super::*;

    #[test]
    fn metadata_reservation_moves_with_retained_failure_owner() {
        let budget = AllocationBudget::new(4096);
        let before = budget.retained_bytes();
        let reservation = budget.reserve_metadata(128).unwrap();
        let charged = 128 + size_of::<MetadataReservation>();
        assert_eq!(reservation.reserved_bytes(), charged);
        assert_eq!(budget.retained_bytes(), before + charged);
        let failure = Err::<(), _>(reservation);
        assert_eq!(budget.retained_bytes(), before + charged);
        let retained = match failure {
            Err(owner) => owner,
            Ok(()) => panic!("fixture retains the exact metadata owner"),
        };
        assert_eq!(retained.reserved_bytes(), charged);
        drop(retained);
        assert_eq!(budget.retained_bytes(), before);
    }

    #[test]
    fn metadata_refusal_preserves_existing_charge_and_accounts_overlap() {
        let token = size_of::<MetadataReservation>();
        let overhead = size_of::<BudgetState>() + 2 * size_of::<usize>();
        let budget = AllocationBudget::new(overhead + 2 * (64 + token));
        let first = budget.reserve_metadata(64).unwrap();
        let second = budget.reserve_metadata(64).unwrap();
        let full = budget.retained_bytes();
        assert!(matches!(budget.reserve_metadata(0), Err(WrapError::Admission(_))));
        assert_eq!(budget.retained_bytes(), full);
        drop(first);
        let replacement = budget.reserve_metadata(64).unwrap();
        assert_eq!(budget.retained_bytes(), full);
        drop((second, replacement));
        assert_eq!(budget.retained_bytes(), overhead);
    }

    #[test]
    fn metadata_overflow_refuses_without_charging() {
        let budget = AllocationBudget::default();
        let before = budget.retained_bytes();
        assert!(matches!(budget.reserve_metadata(usize::MAX), Err(WrapError::Admission(_))));
        assert_eq!(budget.retained_bytes(), before);
    }
    #[test]
    fn task_failure_mapping_preserves_typed_source_and_original_io_payload() {
        let error = WrapError::from(TaskError::Source(InputError::Io(io::Error::from_raw_os_error(123))));
        match error {
            WrapError::Source(InputError::Io(error)) => assert_eq!(error.raw_os_error(), Some(123)),
            other => panic!("source error erased: {other:?}"),
        }
        let error = WrapError::from(TaskError::Source(InputError::Io(io::Error::new(
            io::ErrorKind::WouldBlock,
            "actual OS error",
        ))));
        assert!(matches!(error, WrapError::Source(InputError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock));
        assert!(matches!(
            WrapError::from(TaskError::Source(InputError::QueryChanged)),
            WrapError::Source(InputError::QueryChanged)
        ));
        assert!(matches!(
            WrapError::from(TaskError::Source(InputError::SourceChanged)),
            WrapError::Source(InputError::SourceChanged)
        ));
        assert!(matches!(WrapError::from(TaskError::Layout(WrapError::Busy)), WrapError::Busy));
    }
}
