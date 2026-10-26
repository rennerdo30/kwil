//! Streaming log queries for native GUI/TUI hosts. All source reads and the
//! final release of accepted snapshots belong to one sleeping worker.
//!
//! Coordinates are zero-based logical UTF-8 bytes and lines (LF separates
//! lines, including an empty final line), matching `core-text::Snapshot`.
//! `page_start` is a matching-row ordinal, not a source line number. Each query
//! rescans its immutable snapshot. The opt-in viewport API instead retains a
//! worker-owned disk coordinate index; scrolling never replays a completed pass.
//!
//! Viewport admission: caller supplies the existing shared search disk policy.
//! One 32-byte record per match (no copied text), one 64 KiB write buffer, one
//! 16 KiB metadata prefix, scalar scan state and one retained Snapshot. Pages
//! contain <=4096 rows, <=1 MiB raw bytes and <=8192 syntax spans. The old hit
//! API retains its 256-hit/512-byte snippet bounds. A worker may
//! hold a new page while a previous bounded report/host page still exists;
//! these fixed multiplicities, not source length, bound RAM. Snapshot tree
//! storage stays charged to its original owner. Both legacy and viewport modes
//! share the existing active/pending/four-retired/latest-report controls.
//!
//! The process-wide four-owner registry includes failed unlink owners. Every
//! attempted record is charged before write, including partial failure; only
//! exact unlink/NotFound releases it. A failed owner blocks fresh admission
//! until a worker retries retirement successfully. No polling or UI I/O.
//! Cancellation is checked between <=64 KiB source chunks and each page row;
//! source/filter changes retire, same-key scrolling resumes the partial cursor.
//! No append/rotation identity is inferred: ROOT must change its epoch/revision.
//! Disk quota failure is explicit, never an apparently complete truncated view.
use kwil_core_runtime::{NativeExitObservation, NativeExitState, NativeObservationError};
use kwil_core_syntax::logs::{self, MAX_PREFIX_BYTES};
pub use kwil_core_syntax::logs::{Level, Metadata, Timestamp};
use kwil_core_text::Snapshot;
use std::{
    io::{self, Read, Seek, SeekFrom, Write},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::Instant,
};

pub const PAGE_HITS: usize = 256;
pub const PREVIEW_BYTES: usize = 512;
pub const VIEWPORT_BYTES: usize = 1024 * 1024;
pub const VIEWPORT_ROWS: usize = 4096;
pub const ROW_BYTES: usize = 64 * 1024;
const CHUNK_BYTES: u64 = 64 * 1024;
const PROGRESS_BYTES: u64 = 4 * CHUNK_BYTES;
const RETIRED: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Filter {
    pub levels: u8,
    pub from: Option<Timestamp>,
    pub to: Option<Timestamp>,
    /// Missing level/timestamp passes its respective constraint when true.
    /// Known fields must still pass their constraints. With no time bounds,
    /// a missing timestamp does not itself exclude a row.
    pub keep_unknown: bool,
}

impl Default for Filter {
    fn default() -> Self {
        Self {
            levels: Level::ALL.iter().fold(0, |mask, level| mask | level.bit()),
            from: None,
            to: None,
            keep_unknown: true,
        }
    }
}

impl Filter {
    fn matches(&self, metadata: &Metadata) -> bool {
        let level = metadata.level.map_or(self.keep_unknown, |level| self.levels & level.bit() != 0);
        let time = if self.from.is_none() && self.to.is_none() {
            true
        } else {
            metadata.timestamp.map_or(self.keep_unknown, |time| {
                self.from.is_none_or(|from| time >= from) && self.to.is_none_or(|to| time <= to)
            })
        };
        level && time
    }
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub line: u64,
    pub offset: u64,
    pub metadata: Metadata,
    /// Worker-built escaped source prefix, at most PREVIEW_BYTES UTF-8 bytes.
    /// No source lookup is needed when drawing a result or reporting progress.
    pub preview: String,
}

#[derive(Clone, Debug)]
pub struct Page {
    pub hits: Vec<Hit>,
    /// Matching rows seen so far; final and exact only when `exact` is true.
    pub total: u64,
    pub exact: bool,
}

#[derive(Debug)]
pub struct Report {
    pub document: u64,
    pub revision: u64,
    pub epoch: u64,
    pub generation: u64,
    pub result: Result<Page, String>,
}

/// Byte windows, not truncated result snippets. `horizontal` is a logical byte
/// displacement in each source line, just like Workspace's horizontal window.
/// Both edges move backwards to a crossing valid UTF-8 scalar; invalid bytes
/// remain exact. Grid cells supply byte mappings, never displayed-string offsets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Viewport {
    pub top: u64,
    pub rows: usize,
    pub horizontal: u64,
    pub row_bytes: usize,
}
impl Viewport {
    fn valid(self) -> bool {
        (1..=VIEWPORT_ROWS).contains(&self.rows)
            && (4..=ROW_BYTES).contains(&self.row_bytes)
            && self.rows.checked_mul(self.row_bytes).is_some_and(|bytes| bytes <= VIEWPORT_BYTES)
    }
}
#[derive(Clone, Debug)]
pub struct ViewportRow {
    pub line: u64,
    pub line_start: u64,
    /// End excludes LF and a preceding CR, including at EOF.
    pub line_end: u64,
    pub offset: u64,
    pub bytes: Vec<u8>,
    /// Existing log lexer classes, in absolute logical source coordinates.
    pub spans: Vec<kwil_core_syntax::Span>,
}
#[derive(Clone, Debug)]
pub struct ViewportPage {
    pub viewport: Viewport,
    pub rows: Vec<ViewportRow>,
    pub total: u64,
    pub exact: bool,
    pub max_line_bytes: u64,
}
#[derive(Debug)]
pub struct ViewportReport {
    pub document: u64,
    pub revision: u64,
    pub epoch: u64,
    pub generation: u64,
    pub result: Result<ViewportPage, String>,
}

#[derive(Clone, Copy)]
struct Key {
    document: u64,
    revision: u64,
    epoch: u64,
    generation: u64,
}

struct Request {
    key: Key,
    snapshot: Snapshot,
    filter: Filter,
    page_start: u64,
    viewport: Option<Viewport>,
    retire_only: bool,
}

impl Request {
    fn report(&self, result: Result<Page, String>) -> Report {
        Report {
            document: self.key.document,
            revision: self.key.revision,
            epoch: self.key.epoch,
            generation: self.key.generation,
            result,
        }
    }
}

/// A rejected source is returned to its owner, never destroyed on the input
/// thread. Retain this bounded retry slot, or move it to an off-UI shutdown
/// owner. A String-only error cannot safely express bounded backpressure for
/// an owned Snapshot. Retrying may use a newer filter/source identity.
pub struct SubmitError {
    pub snapshot: Snapshot,
    pub reason: &'static str,
}

impl std::fmt::Debug for SubmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubmitError").field("reason", &self.reason).finish_non_exhaustive()
    }
}
impl std::fmt::Display for SubmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.reason)
    }
}
impl std::error::Error for SubmitError {}

#[derive(Default)]
struct Mailbox {
    pending: Option<Request>,
    retired: [Option<Request>; RETIRED],
    report: Option<Report>,
    viewport_report: Option<ViewportReport>,
}

struct Shared {
    mailbox: Mutex<Mailbox>,
    latest: AtomicU64,
    closed: AtomicBool,
    retry_wake: AtomicBool,
    retire_index: AtomicBool,
    retirement_error: Mutex<Option<String>>,
    // A retained token prevents lost notifications when stop/submit races with
    // recv. try_send coalesces tokens; neither UI nor worker waits to send.
    signal: mpsc::SyncSender<()>,
    completed: AtomicBool,
    completion: Condvar,
    native_exit: NativeExitObservation,
}
struct Completion<'a>(&'a Shared);
impl Drop for Completion<'_> {
    fn drop(&mut self) {
        let _mailbox = self.0.mailbox.lock().unwrap_or_else(|error| error.into_inner());
        self.0.completed.store(true, Ordering::Release);
        self.0.completion.notify_all();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogSettlementReason {
    TimedOut,
    ThreadExitPending,
    WorkerPanicked,
    NativeObservation(NativeObservationError),
}

/// Explicit producer-owned OFF-UI retirement, separate from worker settlement.
/// A retained handle OR original worker panic refuses this capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogRetirementRetryReason {
    WorkerNotJoined,
    TimedOut,
    /// Registered unlink failed, or a metadata lock is currently unavailable.
    Failed,
}

impl Shared {
    fn notify(&self) {
        let _ = self.signal.try_send(());
    }

    fn cancelled(&self, generation: u64) -> bool {
        self.closed.load(Ordering::Acquire) || self.latest.load(Ordering::Acquire) != generation
    }

    fn publish(&self, report: Report, wake: &dyn Fn()) {
        let mut mailbox = self.mailbox.lock().unwrap_or_else(|error| error.into_inner());
        if self.cancelled(report.generation) {
            return;
        }
        let needs_wake = mailbox.report.is_none();
        mailbox.report = Some(report);
        drop(mailbox);
        if needs_wake {
            wake();
        }
    }

    fn publish_viewport(&self, request: &Request, result: Result<ViewportPage, String>, wake: &dyn Fn()) {
        let mut mailbox = self.mailbox.lock().unwrap_or_else(|error| error.into_inner());
        if self.cancelled(request.key.generation) {
            return;
        }
        let needs_wake = mailbox.viewport_report.is_none();
        let key = request.key;
        mailbox.viewport_report = Some(ViewportReport {
            document: key.document,
            revision: key.revision,
            epoch: key.epoch,
            generation: key.generation,
            result,
        });
        drop(mailbox);
        if needs_wake {
            wake();
        }
    }
}

/// Check after joining off UI. Failed unlinks retain exact paths and charges
/// in the bounded process registry, and are retried before later admission.
pub struct RetirementStatus(Arc<Shared>);
impl RetirementStatus {
    pub fn result(&self) -> Result<(), String> {
        self.0.retirement_error.lock().unwrap_or_else(|e| e.into_inner()).clone().map_or(Ok(()), Err)
    }
}

/// Construct after the first frame. `wake` only queues a frontend event; call
/// `drain` on that event. One active request, one latest pending request, four
/// retirement slots and one report slot. While destroying a retired batch the
/// worker can additionally own at most four superseded requests; no submit
/// waits for their destructors. A caller retains at most one rejected source.
pub struct LogSession {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    generation: u64,
    panic: Option<Box<dyn std::any::Any + Send + 'static>>,
    native_retired: bool,
    settled: bool,
}

/// Failure owns the original facade; Drop never joins or performs native I/O.
pub struct LogShutdownError {
    owner: Mutex<Option<LogSession>>,
    reason: LogSettlementReason,
}
impl LogShutdownError {
    pub fn reason(&self) -> LogSettlementReason {
        self.reason
    }
    pub fn into_session(self) -> LogSession {
        self.owner.into_inner().unwrap_or_else(|error| error.into_inner()).expect("original shutdown owner")
    }
}
impl std::fmt::Display for LogShutdownError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(out, "worker shutdown {:?}; exact owner retained", self.reason)
    }
}
impl std::fmt::Debug for LogShutdownError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, out)
    }
}
impl std::error::Error for LogShutdownError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.reason {
            LogSettlementReason::NativeObservation(error) => Some(error),
            _ => None,
        }
    }
}

/// Original log session, including the native observation and spool status.
pub struct LogShutdown {
    session: LogSession,
}
impl LogShutdown {
    /// Explicit OFF-UI UNBOUNDED compatibility join and native-right cleanup.
    pub fn join(self) -> Result<(), LogShutdownError> {
        self.session.shutdown()
    }
    pub fn settle_until(&mut self, deadline: Instant) -> Result<(), LogSettlementReason> {
        self.session.settle_until(deadline)
    }
    pub fn into_session(self) -> LogSession {
        self.session
    }
}

impl LogSession {
    /// Declared facade/shared headers, aligned Arc allocation, retirement and
    /// completion guards and one typed return. The existing sync-channel's
    /// opaque std allocation, projection/index and source accounting remain
    /// separate; this does not claim to size private std channel internals.
    /// Reserve this in host aggregate metadata BEFORE construction.
    pub const fn retained_metadata_bytes() -> usize {
        match Self::checked_retained_metadata_bytes() {
            Some(bytes) => bytes,
            None => usize::MAX,
        }
    }
    pub const fn checked_retained_metadata_bytes() -> Option<usize> {
        #[repr(C)]
        struct Allocation {
            strong: std::sync::atomic::AtomicUsize,
            weak: std::sync::atomic::AtomicUsize,
            data: Shared,
        }
        let bytes = Some(0usize);
        let bytes = metadata_add(bytes, std::mem::size_of::<Self>());
        let bytes = metadata_add(bytes, std::mem::size_of::<Allocation>());
        let bytes = metadata_add(bytes, std::mem::size_of::<WorkerRetirement>());
        let bytes = metadata_add(bytes, std::mem::size_of::<Completion<'static>>());
        let bytes = metadata_add(bytes, std::mem::size_of::<Result<(), LogSettlementReason>>());
        let bytes = metadata_add(bytes, std::mem::size_of::<Result<(), LogRetirementRetryReason>>());
        let bytes = metadata_add(bytes, std::mem::size_of::<Result<(), LogRetirementRetryReason>>());
        let bytes = metadata_add(bytes, std::mem::size_of::<LogShutdownError>());
        let bytes = metadata_add(bytes, std::mem::size_of::<Result<(), LogShutdownError>>());
        let bytes = metadata_add(bytes, std::mem::size_of::<LogShutdown>());
        match NativeExitObservation::operation_stack_bytes() {
            Some(scratch) => metadata_add(metadata_add(bytes, scratch), std::mem::size_of::<&Instant>()),
            None => None,
        }
    }
    /// Exact fixed facade + aligned existing Shared allocation growth, prepaid
    /// by the host's aggregate metadata admission before starting this worker.
    /// Scalars/Condvar/panic slot add no new allocation.
    pub const fn settlement_metadata_bytes() -> usize {
        #[allow(dead_code)]
        struct BeforeSession {
            shared: Arc<Shared>,
            worker: Option<JoinHandle<()>>,
            generation: u64,
        }
        #[allow(dead_code)]
        struct BeforeShared {
            mailbox: Mutex<Mailbox>,
            latest: AtomicU64,
            closed: AtomicBool,
            retry_wake: AtomicBool,
            retire_index: AtomicBool,
            retirement_error: Mutex<Option<String>>,
            signal: mpsc::SyncSender<()>,
        }
        #[repr(C)]
        struct Allocation<T> {
            strong: std::sync::atomic::AtomicUsize,
            weak: std::sync::atomic::AtomicUsize,
            data: T,
        }
        std::mem::size_of::<Self>() - std::mem::size_of::<BeforeSession>() + std::mem::size_of::<Allocation<Shared>>()
            - std::mem::size_of::<Allocation<BeforeShared>>()
    }
    pub fn start(wake: impl Fn() + Send + Sync + 'static) -> io::Result<Self> {
        Self::start_with_index_limit(0, wake)
    }

    /// `max_index_bytes` is an explicit aggregate disk admission limit for the
    /// log projection registry, not a source-size or RAM limit. Zero keeps the
    /// old hit-only API. No directory, source read or file open happens here.
    pub fn start_with_index_limit(max_index_bytes: u64, wake: impl Fn() + Send + Sync + 'static) -> io::Result<Self> {
        Self::checked_retained_metadata_bytes().ok_or(io::Error::from(io::ErrorKind::OutOfMemory))?;
        let (signal, ready) = mpsc::sync_channel(1);
        let shared = Arc::new(Shared {
            mailbox: Mutex::new(Mailbox::default()),
            latest: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            retry_wake: AtomicBool::new(false),
            retire_index: AtomicBool::new(false),
            retirement_error: Mutex::new(None),
            signal,
            completed: AtomicBool::new(false),
            native_exit: NativeExitObservation::new(),
            completion: Condvar::new(),
        });
        let worker_shared = shared.clone();
        let worker = thread::Builder::new().name("kwil-log".into()).spawn(move || {
            let _completion = Completion(&worker_shared);
            if worker_shared.native_exit.register_current().is_err() {
                worker_shared.closed.store(true, Ordering::Release);
            }
            let _retirement = WorkerRetirement(worker_shared.clone());
            worker_loop(&worker_shared, ready, &wake, max_index_bytes);
        })?;
        Ok(Self { shared, worker: Some(worker), generation: 0, panic: None, native_retired: false, settled: false })
    }

    /// Nonblocking: takes ownership only on acceptance. A full/busy mailbox
    /// returns the owned source for retry after the worker wake. Only accepted
    /// submissions advance generation and invalidate prior reports. `epoch`
    /// distinguishes rotation/reopen even when document/revision are reused.
    #[allow(clippy::too_many_arguments)]
    pub fn submit(
        &mut self,
        document: u64,
        revision: u64,
        epoch: u64,
        snapshot: Snapshot,
        filter: Filter,
        page_start: u64,
    ) -> Result<u64, SubmitError> {
        self.submit_mode(document, revision, epoch, snapshot, filter, page_start, None, false)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn submit_viewport(
        &mut self,
        document: u64,
        revision: u64,
        epoch: u64,
        snapshot: Snapshot,
        filter: Filter,
        viewport: Viewport,
    ) -> Result<u64, SubmitError> {
        if !viewport.valid() {
            return Err(SubmitError { snapshot, reason: "invalid bounded log viewport" });
        }
        self.submit_mode(document, revision, epoch, snapshot, filter, viewport.top, Some(viewport), false)
    }

    /// Admit a rejected, now obsolete source solely for worker destruction.
    /// No source scan, file open, or projection resurrection after close.
    pub fn retire_snapshot(&mut self, snapshot: Snapshot) -> Result<u64, SubmitError> {
        self.submit_mode(0, 0, 0, snapshot, Filter::default(), 0, None, true)
    }

    #[allow(clippy::too_many_arguments)]
    fn submit_mode(
        &mut self,
        document: u64,
        revision: u64,
        epoch: u64,
        snapshot: Snapshot,
        filter: Filter,
        page_start: u64,
        viewport: Option<Viewport>,
        retire_only: bool,
    ) -> Result<u64, SubmitError> {
        let Some(generation) = self.generation.checked_add(1) else {
            return Err(SubmitError { snapshot, reason: "log generation exhausted" });
        };
        let Ok(mut mailbox) = self.shared.mailbox.try_lock() else {
            self.shared.retry_wake.store(true, Ordering::Release);
            self.shared.notify();
            return Err(SubmitError { snapshot, reason: "log mailbox busy; retry on wake" });
        };
        if self.shared.closed.load(Ordering::Acquire) {
            return Err(SubmitError { snapshot, reason: "log session stopped" });
        }
        if mailbox.pending.is_some() {
            let Some(slot) = mailbox.retired.iter().position(Option::is_none) else {
                self.shared.retry_wake.store(true, Ordering::Release);
                self.shared.notify();
                return Err(SubmitError { snapshot, reason: "log retirement full; retry on wake" });
            };
            mailbox.retired[slot] = mailbox.pending.take();
        }
        mailbox.pending = Some(Request {
            key: Key { document, revision, epoch, generation },
            snapshot,
            filter,
            page_start,
            viewport,
            retire_only,
        });
        mailbox.report = None;
        mailbox.viewport_report = None;
        self.generation = generation;
        self.shared.latest.store(generation, Ordering::Release);
        drop(mailbox);
        self.shared.notify();
        Ok(generation)
    }

    /// Returns at most one bounded report. Empty on lock contention; a new
    /// wake is requested so a host never needs polling or a retry timer.
    pub fn drain(&self) -> Vec<Report> {
        let Ok(mut mailbox) = self.shared.mailbox.try_lock() else {
            self.shared.retry_wake.store(true, Ordering::Release);
            self.shared.notify();
            return Vec::new();
        };
        mailbox.report.take().into_iter().collect()
    }

    pub fn drain_viewport(&self) -> Vec<ViewportReport> {
        let Ok(mut mailbox) = self.shared.mailbox.try_lock() else {
            self.shared.retry_wake.store(true, Ordering::Release);
            self.shared.notify();
            return Vec::new();
        };
        mailbox.viewport_report.take().into_iter().collect()
    }

    /// Closing/filter edits cancel without a source clone or any I/O on UI.
    /// A subsequent accepted request is ordered after this retirement fence.
    pub fn cancel_viewport(&self) {
        self.shared.retire_index.store(true, Ordering::Release);
        self.shared.notify();
    }

    pub fn retirement_status(&self) -> RetirementStatus {
        RetirementStatus(self.shared.clone())
    }

    /// Explicit OFF-UI UNBOUNDED compatibility join. Never call from a bounded
    /// shutdown protocol; failure retains the complete original owner.
    pub fn shutdown(mut self) -> Result<(), LogShutdownError> {
        self.shutdown_in_place().map_err(|reason| LogShutdownError { owner: Mutex::new(Some(self)), reason })
    }
    pub fn shutdown_until(mut self, deadline: Instant) -> Result<(), LogShutdownError> {
        self.settle_until(deadline).map_err(|reason| LogShutdownError { owner: Mutex::new(Some(self)), reason })
    }
    pub(crate) fn shutdown_in_place(&mut self) -> Result<(), LogSettlementReason> {
        self.close();
        if let Some(handle) = self.worker.take()
            && let Err(panic) = handle.join()
        {
            self.panic = Some(panic);
        }
        self.finish_join_with(None, NativeExitObservation::retire_after_join)
    }
    /// OFF UI. Existing completion ACK, then one shared native deadline wait,
    /// using the ORIGINAL deadline. ACK/is_finished alone
    /// never authorizes join. Every refusal retains the exact original owners.
    pub fn settle_until(&mut self, deadline: Instant) -> Result<(), LogSettlementReason> {
        self.settle_with(
            deadline,
            |observation, handle| observation.wait_until(handle, deadline),
            NativeExitObservation::retire_after_join,
        )
    }
    fn settle_with(
        &mut self,
        deadline: Instant,
        query: impl FnOnce(&NativeExitObservation, &JoinHandle<()>) -> Result<NativeExitState, NativeObservationError>,
        retire: impl FnOnce(&NativeExitObservation) -> Result<(), NativeObservationError>,
    ) -> Result<(), LogSettlementReason> {
        self.close();
        if self.worker.is_some() {
            let shared = &self.shared;
            let guard = shared.mailbox.lock().unwrap_or_else(|error| error.into_inner());
            let (guard, _) = shared
                .completion
                .wait_timeout_while(guard, deadline.saturating_duration_since(Instant::now()), |_| {
                    !shared.completed.load(Ordering::Acquire)
                })
                .unwrap_or_else(|error| error.into_inner());
            let completed = shared.completed.load(Ordering::Acquire);
            drop(guard);
            if !completed {
                return Err(LogSettlementReason::TimedOut);
            }
            // Query before consuming anything, even after a cleanup ACK. It also
            // retains original registration errors and any partial owned right.
            match query(&shared.native_exit, self.worker.as_ref().expect("original worker handle")) {
                Ok(NativeExitState::Pending) => return Err(LogSettlementReason::ThreadExitPending),
                Err(error) => return Err(LogSettlementReason::NativeObservation(error)),
                Ok(NativeExitState::Exited) => {}
            }
            if Instant::now() >= deadline {
                return Err(LogSettlementReason::TimedOut);
            }
            if let Err(panic) = self.worker.take().expect("native-exited original handle").join() {
                self.panic = Some(panic);
            }
        }
        // thread=None means the ORIGINAL join already returned, including panic;
        // it does not imply native-right retirement completed on a prior call.
        self.finish_join_with(Some(deadline), retire)
    }
    fn finish_join_with(
        &mut self,
        deadline: Option<Instant>,
        retire: impl FnOnce(&NativeExitObservation) -> Result<(), NativeObservationError>,
    ) -> Result<(), LogSettlementReason> {
        self.finish_join_with_clock(deadline, retire, Instant::now)
    }
    fn finish_join_with_clock(
        &mut self,
        deadline: Option<Instant>,
        retire: impl FnOnce(&NativeExitObservation) -> Result<(), NativeObservationError>,
        mut now: impl FnMut() -> Instant,
    ) -> Result<(), LogSettlementReason> {
        // Once the right is retired, sticky diagnostics precede deadline-only
        // no-op retries. No consumed panic payload can turn into fake success.
        if self.native_retired {
            if self.panic.is_some() {
                return Err(LogSettlementReason::WorkerPanicked);
            }
            if let Some(error) = self.shared.native_exit.registration_error() {
                return Err(LogSettlementReason::NativeObservation(error));
            }
            if self.settled {
                return Ok(());
            }
        }
        if !self.settled && deadline.is_some_and(|deadline| now() >= deadline) {
            return Err(LogSettlementReason::TimedOut);
        }
        if !self.native_retired {
            if deadline.is_some_and(|deadline| now() >= deadline) {
                return Err(LogSettlementReason::TimedOut);
            }
            retire(&self.shared.native_exit).map_err(LogSettlementReason::NativeObservation)?;
            self.native_retired = true;
            if deadline.is_some_and(|deadline| now() >= deadline) {
                return Err(LogSettlementReason::TimedOut);
            }
        }
        if self.panic.is_some() {
            return Err(LogSettlementReason::WorkerPanicked);
        }
        if let Some(error) = self.shared.native_exit.registration_error() {
            return Err(LogSettlementReason::NativeObservation(error));
        }
        self.settled = true;
        Ok(())
    }
    /// Scalar-only original native retirement evidence; no native query or I/O.
    pub fn first_native_retirement_error(&self) -> Option<NativeObservationError> {
        self.shared.native_exit.first_retirement_error()
    }
    pub fn panic_payload(&self) -> Option<&(dyn std::any::Any + Send + 'static)> {
        self.panic.as_deref()
    }

    /// Explicit OFF-UI retry after this original worker has successfully joined.
    /// This is the SAME process-private inactive-registry capability used by
    /// IndexSpool admission/retirement: one pass over at most four registered
    /// owners, never active entries, caller-supplied paths or a directory scan.
    ///
    /// The first String diagnostic stays unchanged on every failed/late retry;
    /// only an on-time full reap clears it. It is existing formatted diagnostic
    /// evidence, NOT the original io::Error. An OS unlink cannot be preempted:
    /// late success retires its exact charge but still returns TimedOut.
    pub fn retry_retirement_until(&mut self, deadline: Instant) -> Result<(), LogRetirementRetryReason> {
        self.retry_retirement_with(
            deadline,
            || {
                let mut leases = match INDEX_LEASES.try_lock() {
                    Ok(leases) => leases,
                    Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
                    Err(std::sync::TryLockError::WouldBlock) => {
                        return Err(if Instant::now() >= deadline {
                            LogRetirementRetryReason::TimedOut
                        } else {
                            LogRetirementRetryReason::Failed
                        });
                    }
                };
                reap_indexes_until_with(&mut leases, deadline, |path| std::fs::remove_file(path), Instant::now)
            },
            Instant::now,
        )
    }

    fn retry_retirement_with(
        &mut self,
        deadline: Instant,
        reap: impl FnOnce() -> Result<(), LogRetirementRetryReason>,
        mut now: impl FnMut() -> Instant,
    ) -> Result<(), LogRetirementRetryReason> {
        // Only the successful settle join can consume the handle without
        // installing a sticky panic. Do not stop, join or replace anything here.
        if self.worker.is_some()
            || !self.native_retired
            || self.panic.is_some()
            || self.shared.native_exit.registration_error().is_some()
            || !self.shared.completed.load(Ordering::Acquire)
        {
            return Err(LogRetirementRetryReason::WorkerNotJoined);
        }
        if now() >= deadline {
            return Err(LogRetirementRetryReason::TimedOut);
        }
        let result = reap();
        if now() >= deadline {
            return Err(LogRetirementRetryReason::TimedOut);
        }
        result?;
        // Never hold this UI-readable status lock across filesystem I/O, and
        // never wait unboundedly for it. A later explicit pass can clear status.
        let mut diagnostic = match self.shared.retirement_error.try_lock() {
            Ok(diagnostic) => diagnostic,
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                return Err(if now() >= deadline {
                    LogRetirementRetryReason::TimedOut
                } else {
                    LogRetirementRetryReason::Failed
                });
            }
        };
        if now() >= deadline {
            return Err(LogRetirementRetryReason::TimedOut);
        }
        diagnostic.take();
        Ok(())
    }

    /// Nonblocking whole-session ownership transfer. No raw handle escapes;
    /// join/retirement belong to the explicit OFF-UI receiver of this ticket.
    pub fn stop(self) -> LogShutdown {
        self.close();
        LogShutdown { session: self }
    }

    fn close(&self) {
        self.shared.closed.store(true, Ordering::Release);
        self.shared.notify();
    }
}

impl Drop for LogSession {
    fn drop(&mut self) {
        self.close();
        if self.worker.is_some() || !self.settled || self.panic.is_some() {
            // Misuse quarantine, NOT successful retirement: retain the SAME
            // shared allocation/source headers/native right and exact handle.
            // No native I/O, source destruction, new allocation or cleanup thread.
            std::mem::forget(Arc::clone(&self.shared));
            std::mem::forget(self.worker.take());
            std::mem::forget(self.panic.take());
        }
    }
}

// Checked declared Rust-value accounting, not compiler/native-stack usage.
const fn metadata_add(bytes: Option<usize>, extra: usize) -> Option<usize> {
    match bytes {
        Some(bytes) => bytes.checked_add(extra),
        None => None,
    }
}

#[cfg(test)]
#[path = "log_session/bounded_settlement_tests.rs"]
mod bounded_settlement_tests;

struct WorkerRetirement(Arc<Shared>);
impl Drop for WorkerRetirement {
    fn drop(&mut self) {
        self.0.closed.store(true, Ordering::Release);
        let old = {
            let mut mailbox = self.0.mailbox.lock().unwrap_or_else(|error| error.into_inner());
            std::mem::take(&mut *mailbox)
        };
        drop(old);
    }
}

fn worker_loop(shared: &Shared, ready: mpsc::Receiver<()>, wake: &dyn Fn(), index_limit: u64) {
    let mut index: Option<ProjectionIndex> = None;
    while ready.recv().is_ok() {
        loop {
            if shared.closed.load(Ordering::Acquire) {
                retire_index(&mut index, shared);
                return;
            }
            if shared.retire_index.swap(false, Ordering::AcqRel) {
                retire_index(&mut index, shared);
            }
            let (request, retired) = {
                let mut mailbox = shared.mailbox.lock().unwrap_or_else(|error| error.into_inner());
                (mailbox.pending.take(), std::mem::take(&mut mailbox.retired))
            };
            let freed = retired.iter().any(Option::is_some);
            drop(retired); // potentially expensive last Snapshot drop is off UI.
            let retry = shared.retry_wake.swap(false, Ordering::AcqRel);
            if freed || retry {
                wake();
            }
            let Some(request) = request else { break };
            if request.retire_only {
                continue;
            }
            let generation = request.key.generation;
            if let Some(viewport) = request.viewport {
                // A pending same-source viewport resumes a cancelled partial
                // pass. Different source/filter retires before new admission.
                if index.as_ref().is_some_and(|old| !old.matches(&request)) {
                    retire_index(&mut index, shared);
                }
                let result = (|| {
                    if index.is_none() {
                        index = Some(ProjectionIndex::new(&request, index_limit)?);
                    }
                    index.as_mut().unwrap().query(
                        viewport,
                        &|| shared.cancelled(generation) || shared.retire_index.load(Ordering::Acquire),
                        |page| shared.publish_viewport(&request, Ok(page), wake),
                    )
                })();
                match result {
                    Ok(Some(page)) => shared.publish_viewport(&request, Ok(page), wake),
                    Ok(None) => {}
                    Err(error) => {
                        retire_index(&mut index, shared);
                        shared.publish_viewport(&request, Err(error.to_string()), wake);
                    }
                }
                continue;
            }
            retire_index(&mut index, shared);
            let result =
                scan(&request, || shared.cancelled(generation), |page| shared.publish(request.report(Ok(page)), wake));
            if let Some(result) = result {
                shared.publish(request.report(result), wake);
            }
            drop(request);
        }
    }
    retire_index(&mut index, shared);
}

// Same storage model as core-search::worker_spool, whose edit-recipe codec
// cannot represent a random-access row index. No owner profile fallback,
// directory traversal, source copies, or UI-visible file handles. The registry
// bounds live AND failed-cleanup owners machine-process-wide; disk is charged
// before writes and released only after exact unlink (NotFound is retired).
const INDEX_RECORD: u64 = 32;
const INDEX_OWNERS: usize = 4;
struct IndexLease {
    path: std::path::PathBuf,
    bytes: u64,
    active: bool,
}
static INDEX_LEASES: Mutex<[Option<IndexLease>; INDEX_OWNERS]> = Mutex::new([const { None }; INDEX_OWNERS]);
static INDEX_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn reap_indexes(leases: &mut [Option<IndexLease>; INDEX_OWNERS]) -> io::Result<()> {
    reap_indexes_with(leases, |path| std::fs::remove_file(path))
}
fn reap_indexes_with(
    leases: &mut [Option<IndexLease>; INDEX_OWNERS],
    mut remove: impl FnMut(&std::path::Path) -> io::Result<()>,
) -> io::Result<()> {
    for entry in leases {
        if let Some(lease) = entry
            && !lease.active
        {
            match remove(&lease.path) {
                Ok(()) => *entry = None,
                Err(error) if error.kind() == io::ErrorKind::NotFound => *entry = None,
                Err(error) => {
                    return Err(io::Error::new(
                        error.kind(),
                        format!(
                            "log index cleanup {} ({} bytes still charged): {error}",
                            lease.path.display(),
                            lease.bytes
                        ),
                    ));
                }
            }
        }
    }
    Ok(())
}

/// One existing registered-reap pass, with deadline checks around each actual
/// unlink. Keep its original successful/NotFound retirement and failed-charge
/// semantics. The injected clock/remove seams only mock slow/failing I/O in
/// source tests; production invokes them synchronously on the OFF-UI producer.
fn reap_indexes_until_with(
    leases: &mut [Option<IndexLease>; INDEX_OWNERS],
    deadline: Instant,
    mut remove: impl FnMut(&std::path::Path) -> io::Result<()>,
    mut now: impl FnMut() -> Instant,
) -> Result<(), LogRetirementRetryReason> {
    let mut late = false;
    let result = reap_indexes_with(leases, |path| {
        if now() >= deadline {
            late = true;
            return Err(io::Error::from(io::ErrorKind::TimedOut));
        }
        let result = remove(path);
        late |= now() >= deadline;
        result
    });
    if late || now() >= deadline {
        Err(LogRetirementRetryReason::TimedOut)
    } else {
        result.map_err(|_| LogRetirementRetryReason::Failed)
    }
}

struct IndexSpool {
    slot: usize,
    file: Option<io::BufWriter<std::fs::File>>,
    count: u64,
    limit: u64,
}
impl IndexSpool {
    fn new(limit: u64) -> io::Result<Self> {
        if limit < INDEX_RECORD {
            return Err(invalid("log viewport disk admission is required"));
        }
        let mut leases = INDEX_LEASES.lock().unwrap_or_else(|e| e.into_inner());
        reap_indexes(&mut leases)?;
        let slot = leases
            .iter()
            .position(Option::is_none)
            .ok_or_else(|| io::Error::new(io::ErrorKind::WouldBlock, "log index owner registry full"))?;
        let id = INDEX_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
        let path = std::env::temp_dir().join(format!("kwil-log-{}-{stamp}-{id}.spool", std::process::id()));
        if path.as_os_str().as_encoded_bytes().len() > 32768 {
            return Err(invalid("log index path exceeds cap"));
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path)?;
        leases[slot] = Some(IndexLease { path, bytes: 0, active: true });
        Ok(Self { slot, file: Some(io::BufWriter::with_capacity(CHUNK_BYTES as usize, file)), count: 0, limit })
    }
    fn append(&mut self, line: u64, start: u64, end: u64) -> io::Result<()> {
        if end < start {
            return Err(invalid("invalid log index source range"));
        }
        {
            let mut leases = INDEX_LEASES.lock().unwrap_or_else(|e| e.into_inner());
            let used = leases
                .iter()
                .flatten()
                .try_fold(0u64, |n, l| n.checked_add(l.bytes))
                .ok_or_else(|| invalid("log index quota overflow"))?;
            if INDEX_RECORD > self.limit.saturating_sub(used) {
                return Err(io::Error::new(
                    io::ErrorKind::StorageFull,
                    "log filter index disk quota exceeded; clear the filter",
                ));
            }
            // The whole attempted record stays charged on partial write.
            leases[self.slot].as_mut().unwrap().bytes += INDEX_RECORD;
        }
        let mut record = [0; INDEX_RECORD as usize];
        record[..8].copy_from_slice(&line.to_le_bytes());
        record[8..16].copy_from_slice(&start.to_le_bytes());
        record[16..24].copy_from_slice(&end.to_le_bytes());
        self.file.as_mut().unwrap().write_all(&record)?;
        self.count += 1;
        Ok(())
    }
    fn read(&mut self, ordinal: u64) -> io::Result<(u64, u64, u64)> {
        if ordinal >= self.count {
            return Err(invalid("log index ordinal out of range"));
        }
        let offset = ordinal.checked_mul(INDEX_RECORD).ok_or_else(|| invalid("log index seek overflow"))?;
        let writer = self.file.as_mut().ok_or_else(|| invalid("log index retired"))?;
        writer.flush()?;
        let file = writer.get_mut();
        file.seek(SeekFrom::Start(offset))?;
        let mut record = [0; INDEX_RECORD as usize];
        file.read_exact(&mut record)?;
        if record[24..].iter().any(|b| *b != 0) {
            return Err(invalid("invalid log index record"));
        }
        let line = u64::from_le_bytes(record[..8].try_into().unwrap());
        let start = u64::from_le_bytes(record[8..16].try_into().unwrap());
        let end = u64::from_le_bytes(record[16..24].try_into().unwrap());
        if end < start {
            return Err(invalid("invalid log index coordinates"));
        }
        Ok((line, start, end))
    }
    fn append_position(&mut self) -> io::Result<()> {
        self.file.as_mut().ok_or_else(|| invalid("log index retired"))?.seek(SeekFrom::End(0))?;
        Ok(())
    }
    fn retire(&mut self) -> io::Result<()> {
        // No retry of buffered writes on Drop: flush errors retain the charge,
        // and into_parts closes the handle without issuing any further write.
        if let Some(file) = self.file.take() {
            let (file, _buffer) = file.into_parts();
            drop(file);
        }
        let mut leases = INDEX_LEASES.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(lease) = &mut leases[self.slot] {
            lease.active = false;
        }
        reap_indexes(&mut leases)
    }
}
impl Drop for IndexSpool {
    fn drop(&mut self) {
        // Explicit normal retirement reports errors; unwinding still leaves a
        // retryable exact registry owner rather than losing its path or quota.
        if self.file.is_some() {
            let _ = self.retire();
        }
    }
}

fn retire_index(index: &mut Option<ProjectionIndex>, shared: &Shared) {
    if let Some(mut old) = index.take()
        && let Err(error) = old.spool.retire()
    {
        *shared.retirement_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(error.to_string());
    }
    // Snapshot and prefix destruction occur here on this same worker.
}

struct ProjectionIndex {
    key: Key,
    filter: Filter,
    snapshot: Snapshot,
    spool: IndexSpool,
    prefix: Vec<u8>,
    offset: u64,
    start: u64,
    line: u64,
    last: Option<u8>,
    exact: bool,
    max_line_bytes: u64,
    #[cfg(test)]
    scanned_bytes: u64,
}
impl ProjectionIndex {
    fn new(request: &Request, limit: u64) -> io::Result<Self> {
        if request.filter.from.zip(request.filter.to).is_some_and(|(a, b)| a > b) {
            return Err(invalid("log time range starts after its end"));
        }
        Ok(Self {
            key: request.key,
            filter: request.filter,
            snapshot: request.snapshot.clone(),
            spool: IndexSpool::new(limit)?,
            prefix: Vec::with_capacity(MAX_PREFIX_BYTES),
            offset: 0,
            start: 0,
            line: 0,
            last: None,
            exact: false,
            max_line_bytes: 0,
            #[cfg(test)]
            scanned_bytes: 0,
        })
    }
    fn matches(&self, request: &Request) -> bool {
        self.key.document == request.key.document
            && self.key.revision == request.key.revision
            && self.key.epoch == request.key.epoch
            && self.filter == request.filter
            && self.snapshot.len() == request.snapshot.len()
    }
    fn end_line(&mut self) -> io::Result<()> {
        let end = self.offset - u64::from(self.last == Some(b'\r'));
        let prefix = if self.offset - self.start == self.prefix.len() as u64 {
            self.prefix.strip_suffix(b"\r").unwrap_or(&self.prefix)
        } else {
            &self.prefix
        };
        if self.filter.matches(&logs::metadata(prefix)) {
            self.spool.append(self.line, self.start, end)?;
            self.max_line_bytes = self.max_line_bytes.max(end - self.start);
        }
        self.prefix.clear();
        self.last = None;
        Ok(())
    }
    fn bytes(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        #[cfg(test)]
        {
            self.scanned_bytes += bytes.len() as u64;
        }
        while !bytes.is_empty() {
            let end = bytes.iter().position(|b| *b == b'\n').unwrap_or(bytes.len());
            let take = end.min(MAX_PREFIX_BYTES - self.prefix.len());
            self.prefix.extend_from_slice(&bytes[..take]);
            if end != 0 {
                self.last = Some(bytes[end - 1]);
            }
            self.offset += end as u64;
            if end == bytes.len() {
                break;
            }
            self.end_line()?;
            self.offset += 1;
            self.line += 1;
            self.start = self.offset;
            bytes = &bytes[end + 1..];
        }
        Ok(())
    }
    fn query(
        &mut self,
        viewport: Viewport,
        cancelled: &dyn Fn() -> bool,
        mut progress: impl FnMut(ViewportPage),
    ) -> io::Result<Option<ViewportPage>> {
        let snapshot = self.snapshot.clone(); // all owners remain worker-side
        let mut next_progress = self.offset.saturating_add(PROGRESS_BYTES);
        self.spool.append_position()?;
        while self.offset < snapshot.len() {
            if cancelled() {
                return Ok(None);
            }
            let end = snapshot.len().min(self.offset.saturating_add(CHUNK_BYTES));
            for chunk in snapshot.chunks(self.offset..end).map_err(io::Error::other)? {
                if cancelled() {
                    return Ok(None);
                }
                self.bytes(chunk.map_err(io::Error::other)?)?;
            }
            if cancelled() {
                return Ok(None);
            }
            if self.offset >= next_progress && self.offset < snapshot.len() {
                let Some(page) = self.page(viewport, cancelled)? else {
                    return Ok(None);
                };
                progress(page);
                self.spool.append_position()?;
                next_progress = self.offset.saturating_add(PROGRESS_BYTES);
            }
        }
        if cancelled() {
            return Ok(None);
        }
        if !self.exact {
            self.end_line()?;
            self.exact = true;
        }
        self.page(viewport, cancelled)
    }
    fn page(&mut self, viewport: Viewport, cancelled: &dyn Fn() -> bool) -> io::Result<Option<ViewportPage>> {
        let mut page = ViewportPage {
            viewport,
            rows: Vec::with_capacity(viewport.rows),
            total: self.spool.count,
            exact: self.exact,
            max_line_bytes: self.max_line_bytes,
        };
        let mut engine = kwil_core_syntax::Engine::new(kwil_core_syntax::Config {
            parse_limit: 0,
            margin_bytes: 0,
            max_window_bytes: ROW_BYTES,
            max_capture_bytes: ROW_BYTES,
        })
        .map_err(io::Error::other)?;
        let mut span_count = 0;
        for ordinal in viewport.top..viewport.top.saturating_add(viewport.rows as u64).min(self.spool.count) {
            if cancelled() {
                return Ok(None);
            }
            let (line, start, end) = self.spool.read(ordinal)?;
            if end > self.snapshot.len() {
                return Err(invalid("log index extends beyond snapshot"));
            }
            let desired = start + viewport.horizontal.min(end - start);
            let from = desired.saturating_sub(3).max(start);
            let to = end.min(desired.saturating_add(viewport.row_bytes as u64 + 3));
            let mut bytes = Vec::with_capacity((to - from) as usize);
            for chunk in self.snapshot.chunks(from..to).map_err(io::Error::other)? {
                if cancelled() {
                    return Ok(None);
                }
                bytes.extend_from_slice(chunk.map_err(io::Error::other)?);
            }
            let left = crate::grid::boundary(&bytes, (desired - from) as usize);
            let right = crate::grid::boundary(&bytes, (left + viewport.row_bytes).min(bytes.len()));
            let offset = from + left as u64;
            let raw = bytes[left..right].to_vec();
            // A mid-line window must never be treated as a new log header.
            let base = usize::from(offset != start);
            let local = LogWindow { bytes: &raw, base };
            let highlights = engine.highlight(
                &local,
                kwil_core_syntax::Language::Log,
                self.key.revision,
                None,
                None,
                base..base + raw.len(),
                cancelled,
            );
            let mut spans = match highlights {
                Ok(highlights) => highlights.spans,
                Err(_) if cancelled() => return Ok(None),
                Err(error) => return Err(io::Error::other(error)),
            };
            spans.sort_unstable_by_key(|span| (span.range.start, span.range.end));
            span_count += spans.len();
            if span_count > 8192 {
                return Err(invalid("log viewport syntax span budget exceeded"));
            }
            let offset_usize =
                usize::try_from(offset).map_err(|_| invalid("log source coordinate exceeds platform"))?;
            usize::try_from(offset + raw.len() as u64)
                .map_err(|_| invalid("log source coordinate exceeds platform"))?;
            let spans = spans
                .into_iter()
                .map(|span| kwil_core_syntax::Span {
                    range: offset_usize + (span.range.start - base)..offset_usize + (span.range.end - base),
                    class: span.class,
                })
                .collect();
            page.rows.push(ViewportRow { line, line_start: start, line_end: end, offset, bytes: raw, spans });
        }
        if cancelled() { Ok(None) } else { Ok(Some(page)) }
    }
}
struct LogWindow<'a> {
    bytes: &'a [u8],
    base: usize,
}
impl kwil_core_syntax::TextSource for LogWindow<'_> {
    fn len(&self) -> usize {
        self.base + self.bytes.len()
    }
    fn chunk(&self, offset: usize, max_len: usize) -> io::Result<std::borrow::Cow<'_, [u8]>> {
        let at = offset
            .checked_sub(self.base)
            .filter(|at| *at <= self.bytes.len())
            .ok_or_else(|| invalid("invalid log syntax window"))?;
        Ok(std::borrow::Cow::Borrowed(&self.bytes[at..self.bytes.len().min(at.saturating_add(max_len))]))
    }
}

/// The scanner retains one prefix, one page, and scalar counters. Even skipped
/// or filtered-out giant lines are visited in bounded pieces, without joining.
struct Scan {
    prefix: Vec<u8>,
    page: Page,
    line: u64,
    line_start: u64,
    offset: u64,
}

impl Scan {
    fn new() -> Self {
        Self {
            prefix: Vec::with_capacity(MAX_PREFIX_BYTES),
            page: Page { hits: Vec::with_capacity(PAGE_HITS), total: 0, exact: false },
            line: 0,
            line_start: 0,
            offset: 0,
        }
    }

    fn end_line(&mut self, filter: &Filter, page_start: u64) {
        // A CR is a line terminator only immediately before LF/EOF. A full
        // prefix on a giant line is left intact; a later CR is never copied.
        let prefix = if self.offset - self.line_start == self.prefix.len() as u64 {
            self.prefix.strip_suffix(b"\r").unwrap_or(&self.prefix)
        } else {
            &self.prefix
        };
        let metadata = logs::metadata(prefix);
        if filter.matches(&metadata) {
            if self.page.total >= page_start && self.page.hits.len() < PAGE_HITS {
                self.page.hits.push(Hit {
                    line: self.line,
                    offset: self.line_start,
                    metadata,
                    preview: preview(prefix),
                });
            }
            self.page.total += 1;
        }
        self.prefix.clear();
    }

    fn bytes(&mut self, bytes: &[u8], filter: &Filter, page_start: u64) {
        let mut rest = bytes;
        while !rest.is_empty() {
            let end = rest.iter().position(|&byte| byte == b'\n').unwrap_or(rest.len());
            let take = end.min(MAX_PREFIX_BYTES - self.prefix.len());
            self.prefix.extend_from_slice(&rest[..take]);
            self.offset += end as u64;
            if end == rest.len() {
                break;
            }
            self.end_line(filter, page_start);
            self.offset += 1;
            self.line += 1;
            self.line_start = self.offset;
            rest = &rest[end + 1..];
        }
    }
}

/// Never inserts replacement characters, splits a UTF-8 scalar or partially
/// emits an escape. Backslashes are doubled so escaped raw bytes cannot be
/// mistaken for literal input. Truncation is a prefix, never skipped content.
fn preview(mut bytes: &[u8]) -> String {
    fn push(out: &mut String, ch: char) -> bool {
        let mut encoded = [0; 4];
        let text = ch.encode_utf8(&mut encoded);
        if ch.is_control() {
            if out.len() + text.len() * 4 > PREVIEW_BYTES {
                return false;
            }
            for byte in text.bytes() {
                escape(out, byte);
            }
        } else if ch == '\\' {
            if out.len() + 2 > PREVIEW_BYTES {
                return false;
            }
            out.push_str("\\\\");
        } else {
            if out.len() + text.len() > PREVIEW_BYTES {
                return false;
            }
            out.push(ch);
        }
        true
    }
    fn escape(out: &mut String, byte: u8) {
        const HEX: &[u8; 16] = b"0123456789ABCDEF";
        out.push_str("\\x");
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 15) as usize] as char);
    }
    let mut out = String::with_capacity(PREVIEW_BYTES);
    while !bytes.is_empty() {
        let (valid, invalid) = match std::str::from_utf8(bytes) {
            Ok(text) => (text, 0),
            Err(error) => (
                std::str::from_utf8(&bytes[..error.valid_up_to()]).expect("validated UTF-8 prefix"),
                error.error_len().unwrap_or(bytes.len() - error.valid_up_to()),
            ),
        };
        for ch in valid.chars() {
            if !push(&mut out, ch) {
                return out;
            }
        }
        bytes = &bytes[valid.len()..];
        for &byte in &bytes[..invalid] {
            if out.len() + 4 > PREVIEW_BYTES {
                return out;
            }
            escape(&mut out, byte);
        }
        bytes = &bytes[invalid..];
    }
    out
}

/// `None` is cancellation, never a partial result presented as completion.
/// Reads are clipped to 64 KiB so original-file change checks are repeated
/// between bounded regions, including within a single giant tree piece.
fn scan(
    request: &Request,
    mut cancelled: impl FnMut() -> bool,
    mut progress: impl FnMut(Page),
) -> Option<Result<Page, String>> {
    let mut scan = Scan::new();
    let len = request.snapshot.len(); // cached metadata, no disk read
    let mut next_progress = PROGRESS_BYTES;
    if cancelled() {
        return None;
    }
    if request.filter.from.zip(request.filter.to).is_some_and(|(from, to)| from > to) {
        return Some(Err("log time range starts after its end".into()));
    }
    while scan.offset < len {
        let end = len.min(scan.offset.saturating_add(CHUNK_BYTES));
        let chunks = match request.snapshot.chunks(scan.offset..end) {
            Ok(chunks) => chunks,
            Err(error) => return Some(Err(error.to_string())),
        };
        for chunk in chunks {
            if cancelled() {
                return None;
            }
            match chunk {
                Ok(bytes) => scan.bytes(bytes, &request.filter, request.page_start),
                Err(error) => return Some(Err(error.to_string())),
            }
        }
        if cancelled() {
            return None;
        }
        if scan.offset >= next_progress && scan.offset < len {
            progress(scan.page.clone());
            next_progress = scan.offset.saturating_add(PROGRESS_BYTES);
        }
    }
    scan.end_line(&request.filter, request.page_start);
    scan.page.exact = true;
    if cancelled() { None } else { Some(Ok(scan.page)) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kwil_core_syntax::logs::parse_timestamp;
    use kwil_core_text::Buffer;
    use std::time::{Duration, Instant};

    const WAIT: Duration = Duration::from_secs(5);
    static INDEX_TEST: Mutex<()> = Mutex::new(());

    fn viewport(top: u64, horizontal: u64) -> Viewport {
        Viewport { top, horizontal, rows: 3, row_bytes: 4096 }
    }

    #[test]
    fn projection_hides_nonmatching_rows_and_keeps_original_bytes_numbers_and_syntax() {
        let _serial = INDEX_TEST.lock().unwrap();
        let bytes = b"2026-10-24T00:00:00Z INFO hidden\r\n2026-10-24T00:00:01Z ERROR visible\r\n\
                      2026-10-24T00:00:02Z ERROR too-late\nunknown";
        let time = parse_timestamp("2026-10-24T00:00:01Z").unwrap();
        let source = snapshot(bytes);
        let request = request(
            source.clone(),
            Filter { levels: Level::Error.bit(), from: Some(time), to: Some(time), keep_unknown: false },
            0,
        );
        let mut index = ProjectionIndex::new(&request, 1 << 20).unwrap();
        let page = index.query(viewport(0, 0), &|| false, |_| {}).unwrap().unwrap();
        assert!(page.exact);
        assert_eq!(page.total, 1);
        let row = &page.rows[0];
        let expected = b"2026-10-24T00:00:01Z ERROR visible";
        let start = bytes.windows(expected.len()).position(|b| b == expected).unwrap() as u64;
        assert_eq!(
            (row.line, row.line_start, row.offset, row.line_end),
            (1, start, start, start + expected.len() as u64)
        );
        assert_eq!(row.bytes, expected);
        let severity = start as usize + 21;
        assert!(
            row.spans.iter().any(|span| span.range.contains(&severity) && span.class == kwil_core_syntax::Class::Error)
        );
        let mut unchanged = Vec::new();
        source.write_to(&mut unchanged).unwrap();
        assert_eq!(unchanged, bytes, "filtering never edits/re-encodes the snapshot");
        index.spool.retire().unwrap();
    }

    #[test]
    fn projection_pages_seek_and_cancelled_same_source_pass_resumes_without_replay() {
        let _serial = INDEX_TEST.lock().unwrap();
        let line = format!("ERROR {}\n", "x".repeat(250));
        let bytes = line.repeat(1500);
        let request = request(snapshot(bytes.as_bytes()), Filter { keep_unknown: false, ..Filter::default() }, 0);
        let mut index = ProjectionIndex::new(&request, 1 << 20).unwrap();
        let checks = std::cell::Cell::new(0);
        let cancelled = || {
            checks.set(checks.get() + 1);
            checks.get() >= 7
        };
        assert!(index.query(viewport(0, 0), &cancelled, |_| {}).unwrap().is_none());
        assert!(index.offset > 0 && index.offset < bytes.len() as u64);
        let middle = index.query(viewport(700, 0), &|| false, |_| {}).unwrap().unwrap();
        assert_eq!(middle.total, 1500);
        assert_eq!(middle.rows[0].line, 700);
        assert_eq!(middle.rows[0].offset, (700 * line.len()) as u64);
        assert_eq!(index.scanned_bytes, bytes.len() as u64, "resume must not replay a prefix");
        for top in [1499, 0, 513, 256, u64::MAX] {
            let page = index.query(viewport(top, 0), &|| false, |_| {}).unwrap().unwrap();
            assert_eq!(page.total, 1500);
            if top < 1500 {
                assert_eq!(page.rows[0].line, top);
            } else {
                assert!(page.rows.is_empty());
            }
            assert_eq!(index.scanned_bytes, bytes.len() as u64, "scroll must seek, never rescan source");
        }
        index.spool.retire().unwrap();
    }

    #[test]
    fn projection_scalar_edges_and_invalid_bytes_remain_byte_exact() {
        let _serial = INDEX_TEST.lock().unwrap();
        let mut bytes = "INFO a界🦀e\u{301}".as_bytes().to_vec();
        bytes.extend_from_slice(b"\xff\\xFF\0tail\r\nWARN end\n");
        let request = request(snapshot(&bytes), Filter::default(), 0);
        let mut index = ProjectionIndex::new(&request, 1 << 20).unwrap();
        let initial = index.query(viewport(0, 0), &|| false, |_| {}).unwrap().unwrap();
        assert_eq!(initial.rows.len(), 3);
        assert_eq!((initial.rows[2].line, initial.rows[2].offset), (2, bytes.len() as u64));
        for horizontal in 0..20 {
            let page = index
                .query(Viewport { row_bytes: 8, horizontal, ..viewport(0, 0) }, &|| false, |_| {})
                .unwrap()
                .unwrap();
            let row = &page.rows[0];
            assert_eq!(row.bytes, bytes[row.offset as usize..row.offset as usize + row.bytes.len()]);
            assert!(row.bytes.len() <= 8);
            if (7..9).contains(&horizontal) {
                assert_eq!(row.offset, 6);
            }
            if (10..13).contains(&horizontal) {
                assert_eq!(row.offset, 9);
            }
            if row.offset + row.bytes.len() as u64 <= 16 {
                assert!(
                    std::str::from_utf8(&row.bytes).is_ok(),
                    "right edge must not invent invalid bytes from a split scalar"
                );
            }
        }
        assert!(initial.rows[0].bytes.contains(&0xff));
        assert!(initial.rows[0].bytes.windows(4).any(|b| b == b"\\xFF"));
        index.spool.retire().unwrap();
    }

    #[test]
    fn viewport_reads_long_rows_beyond_preview_and_clips_both_dimensions() {
        let _serial = INDEX_TEST.lock().unwrap();
        let mut bytes = b"ERROR ".to_vec();
        bytes.extend(std::iter::repeat_n(b'x', 9000));
        bytes.extend_from_slice(b"suffix\nINFO hidden");
        let request = request(
            snapshot(&bytes),
            Filter { levels: Level::Error.bit(), keep_unknown: false, ..Filter::default() },
            0,
        );
        let mut index = ProjectionIndex::new(&request, 1 << 20).unwrap();
        let page = index
            .query(Viewport { horizontal: 2000, row_bytes: 4096, ..viewport(0, 0) }, &|| false, |_| {})
            .unwrap()
            .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.rows[0].offset, 2000);
        assert_eq!(page.rows[0].bytes, &bytes[2000..6096]);
        assert!(page.rows[0].bytes.len() > PREVIEW_BYTES, "main body must not reuse snippet truncation");
        let tail = index.query(Viewport { horizontal: 9006, ..viewport(0, 0) }, &|| false, |_| {}).unwrap().unwrap();
        assert_eq!(tail.rows[0].bytes, b"suffix");
        assert_eq!(index.scanned_bytes, bytes.len() as u64);
        assert!(!Viewport { rows: PAGE_HITS + 1, ..viewport(0, 0) }.valid());
        assert!(!Viewport { rows: PAGE_HITS, row_bytes: ROW_BYTES, ..viewport(0, 0) }.valid());
        index.spool.retire().unwrap();
    }

    #[test]
    fn viewport_worker_publishes_only_current_source_and_joins_with_index_retirement() {
        let _serial = INDEX_TEST.lock().unwrap();
        let (mut session, events, parked, release) = gated_session_with_index(1 << 20);
        let old = session
            .submit_viewport(
                7,
                11,
                1,
                snapshot(b"INFO hidden\nERROR old"),
                Filter { levels: Level::Error.bit(), keep_unknown: false, ..Filter::default() },
                viewport(0, 0),
            )
            .unwrap();
        parked.recv_timeout(WAIT).unwrap();
        let new = session
            .submit_viewport(
                7,
                11,
                2,
                snapshot(b"WARN hidden\nERROR current"),
                Filter { levels: Level::Error.bit(), keep_unknown: false, ..Filter::default() },
                viewport(0, 0),
            )
            .unwrap();
        assert_ne!(old, new);
        release.send(()).unwrap();
        let deadline = Instant::now() + WAIT;
        loop {
            events.recv_timeout(deadline.saturating_duration_since(Instant::now())).expect("viewport wake timeout");
            let mut finished = false;
            for report in session.drain_viewport() {
                assert_eq!((report.document, report.revision, report.epoch, report.generation), (7, 11, 2, new));
                let page = report.result.unwrap();
                if page.exact {
                    assert_eq!(page.total, 1);
                    assert_eq!((page.rows[0].line, page.rows[0].offset), (1, 12));
                    assert_eq!(page.rows[0].bytes, b"ERROR current");
                    finished = true;
                }
            }
            if finished {
                break;
            }
            assert!(Instant::now() < deadline);
        }
        let status = session.retirement_status();
        let shared = session.shared.clone();
        session.stop().join().unwrap();
        status.result().unwrap();
        let mailbox = shared.mailbox.lock().unwrap();
        assert!(mailbox.pending.is_none() && mailbox.retired.iter().all(Option::is_none));
        assert!(mailbox.report.is_none() && mailbox.viewport_report.is_none());
        assert!(INDEX_LEASES.lock().unwrap().iter().all(Option::is_none));
    }

    #[test]
    fn index_quota_refusal_and_partial_write_keep_the_attempted_charge_until_unlink() {
        let _serial = INDEX_TEST.lock().unwrap();
        let mut spool = IndexSpool::new(INDEX_RECORD).unwrap();
        spool.append(0, 0, 5).unwrap();
        assert!(spool.append(1, 6, 10).unwrap_err().to_string().contains("quota"));
        assert_eq!(spool.count, 1);
        spool.retire().unwrap();
        let mut spool = IndexSpool::new(1 << 20).unwrap();
        let path = INDEX_LEASES.lock().unwrap()[spool.slot].as_ref().unwrap().path.clone();
        let (file, _) = spool.file.take().unwrap().into_parts();
        drop(file);
        spool.file = Some(io::BufWriter::with_capacity(0, std::fs::File::open(path).unwrap()));
        assert!(spool.append(0, 0, 5).is_err(), "read-only fault control must fail real spool write");
        assert_eq!(spool.count, 0);
        assert_eq!(INDEX_LEASES.lock().unwrap()[spool.slot].as_ref().unwrap().bytes, INDEX_RECORD);
        spool.retire().unwrap();
        assert!(INDEX_LEASES.lock().unwrap()[spool.slot].is_none());
    }

    #[test]
    fn failed_unlink_keeps_exact_path_and_charge_and_retry_releases_only_that_owner() {
        let path = std::path::PathBuf::from("private-log-index-fault-control");
        let mut leases: [Option<IndexLease>; INDEX_OWNERS] = [const { None }; INDEX_OWNERS];
        leases[0] = Some(IndexLease { path: path.clone(), bytes: 96, active: false });
        leases[1] = Some(IndexLease { path: "still-active".into(), bytes: 32, active: true });
        let error =
            reap_indexes_with(&mut leases, |_| Err(io::Error::from(io::ErrorKind::PermissionDenied))).unwrap_err();
        assert!(error.to_string().contains("96 bytes still charged"));
        assert_eq!(leases[0].as_ref().unwrap().path, path);
        assert_eq!(leases[0].as_ref().unwrap().bytes, 96);
        let mut removed = Vec::new();
        reap_indexes_with(&mut leases, |path| {
            removed.push(path.to_owned());
            Ok(())
        })
        .unwrap();
        assert_eq!(removed, [path]);
        assert!(leases[0].is_none());
        assert_eq!(leases[1].as_ref().unwrap().bytes, 32);
    }

    fn snapshot(bytes: &[u8]) -> Snapshot {
        let mut buffer = Buffer::empty();
        buffer.insert(0, bytes).unwrap();
        buffer.snapshot()
    }

    fn request(snapshot: Snapshot, filter: Filter, page_start: u64) -> Request {
        Request {
            key: Key { document: 7, revision: 11, epoch: 13, generation: 1 },
            snapshot,
            filter,
            page_start,
            viewport: None,
            retire_only: false,
        }
    }

    fn query(bytes: &[u8], filter: Filter, page_start: u64) -> Page {
        scan(&request(snapshot(bytes), filter, page_start), || false, |_| {}).unwrap().unwrap()
    }

    #[test]
    fn mixed_text_json_log4j_preserves_logical_coordinates() {
        let lines = [
            "2026-10-24T12:00:00Z INFO plain",
            r#"{"timestamp":"2026-10-24T12:00:01Z","level":"ERROR","message":"json"}"#,
            "2026-10-24 12:00:02,000 [main] WARN app - log4j",
            "continuation without metadata",
        ];
        let text = lines.join("\r\n");
        let page = query(text.as_bytes(), Filter::default(), 0);
        assert!(page.exact);
        assert_eq!(page.total, 4);
        assert_eq!(page.hits.len(), 4);
        let mut offset = 0;
        for (index, hit) in page.hits.iter().enumerate() {
            assert_eq!((hit.line, hit.offset), (index as u64, offset));
            offset += lines[index].len() as u64 + 2;
        }
        assert_eq!(page.hits[0].metadata.level, Some(Level::Info));
        assert_eq!(page.hits[1].metadata.level, Some(Level::Error));
        assert_eq!(page.hits[2].metadata.level, Some(Level::Warn));
        assert_eq!(page.hits[3].metadata.level, None);
    }

    #[test]
    fn time_bounds_are_inclusive_and_combine_with_level_mask() {
        let text = b"2026-10-24T12:00:00Z INFO before\n2026-10-24T12:00:01Z INFO lower\n\
                     2026-10-24T12:00:02Z ERROR upper\n2026-10-24T12:00:03Z INFO after";
        let from = parse_timestamp("2026-10-24T12:00:01Z").unwrap();
        let to = parse_timestamp("2026-10-24T12:00:02Z").unwrap();
        let filter = Filter { from: Some(from), to: Some(to), keep_unknown: false, ..Filter::default() };
        let page = query(text, filter, 0);
        assert_eq!(page.total, 2);
        assert_eq!(page.hits.iter().map(|hit| hit.line).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(query(text, Filter { levels: Level::Error.bit(), ..filter }, 0).total, 1);
        assert_eq!(query(text, Filter { from: Some(to), to: Some(to), ..filter }, 0).total, 1);
        assert_eq!(query(text, Filter { from: None, to: Some(from), ..filter }, 0).total, 2);
        assert_eq!(query(text, Filter { from: Some(to), to: None, ..filter }, 0).total, 2);
        let bad = request(snapshot(text), Filter { from: Some(to), to: Some(from), ..filter }, 0);
        assert!(scan(&bad, || false, |_| {}).unwrap().unwrap_err().contains("starts after"));
    }

    #[test]
    fn unknown_policy_applies_per_constrained_field() {
        let filter = Filter {
            levels: Level::Warn.bit(),
            from: Some(Timestamp(10)),
            to: Some(Timestamp(20)),
            keep_unknown: true,
        };
        let known = |level, timestamp| Metadata { level, timestamp };
        assert!(filter.matches(&known(None, None)));
        assert!(filter.matches(&known(Some(Level::Warn), None)));
        assert!(filter.matches(&known(None, Some(Timestamp(10)))));
        assert!(!filter.matches(&known(Some(Level::Error), None)));
        assert!(!filter.matches(&known(None, Some(Timestamp(21)))));
        let strict = Filter { keep_unknown: false, ..filter };
        assert!(!strict.matches(&known(None, Some(Timestamp(15)))));
        assert!(!strict.matches(&known(Some(Level::Warn), None)));
        assert!(strict.matches(&known(Some(Level::Warn), Some(Timestamp(20)))));
        let no_time = Filter { from: None, to: None, ..strict };
        assert!(no_time.matches(&known(Some(Level::Warn), None)));
        assert_eq!(query(b"INFO known\nunknown\nERROR known", Filter { levels: 0, ..Filter::default() }, 0).total, 1);
        assert_eq!(
            query(b"INFO known\nunknown", Filter { levels: 0, keep_unknown: false, ..Filter::default() }, 0).total,
            0
        );
    }

    #[test]
    fn bounded_page_counts_every_match_and_handles_empty_final_line() {
        let text = (0..600).map(|_| "INFO record\n").collect::<String>();
        let filter = Filter { keep_unknown: false, ..Filter::default() };
        let first = query(text.as_bytes(), filter, 0);
        assert_eq!((first.hits.len(), first.total), (PAGE_HITS, 600));
        let middle = query(text.as_bytes(), filter, 250);
        assert_eq!((middle.hits.len(), middle.hits[0].line, middle.total), (PAGE_HITS, 250, 600));
        let last = query(text.as_bytes(), filter, 599);
        assert_eq!((last.hits.len(), last.hits[0].line, last.total), (1, 599, 600));
        assert!(query(text.as_bytes(), filter, u64::MAX).hits.is_empty());
        assert_eq!(query(text.as_bytes(), Filter::default(), 600).hits[0].line, 600);
        assert_eq!(query(b"", Filter::default(), 0).total, 1);
        assert_eq!(query(b"\n\n", Filter::default(), 0).total, 3);
    }

    #[test]
    fn huge_lines_only_parse_prefix_and_continue_after_chunk_boundaries() {
        // Generated below 1 MiB; no large file fixture or file-sized result.
        let mut text = b"INFO first ".to_vec();
        text.resize(CHUNK_BYTES as usize * 2 - 1, b'x');
        text.extend_from_slice(b"\r\n"); // CR/LF straddles the 64 KiB read boundary.
        let second_offset = text.len();
        text.resize(second_offset + CHUNK_BYTES as usize * 2, b'x');
        text.extend_from_slice(b" ERROR outside-prefix\nWARN last");
        let last_offset = text.len() - b"WARN last".len();
        let page = query(&text, Filter { keep_unknown: false, ..Filter::default() }, 0);
        assert_eq!(page.total, 2);
        assert_eq!(page.hits[0].metadata.level, Some(Level::Info));
        assert_eq!((page.hits[1].line, page.hits[1].offset), (2, last_offset as u64));
        assert_eq!(page.hits[1].metadata.level, Some(Level::Warn));
    }

    #[test]
    fn cancellation_checked_inside_one_giant_line_and_progress_is_inexact() {
        let text = vec![b'x'; CHUNK_BYTES as usize * 6];
        let request = request(snapshot(&text), Filter::default(), 0);
        let mut checks = 0;
        assert!(
            scan(
                &request,
                || {
                    checks += 1;
                    checks == 5
                },
                |_| {}
            )
            .is_none()
        );
        assert_eq!(checks, 5);
        let mut reports = Vec::new();
        let final_page = scan(&request, || false, |page| reports.push(page)).unwrap().unwrap();
        assert_eq!(reports.len(), 1);
        assert!(!reports[0].exact);
        assert_eq!(reports[0].total, 0); // incomplete line is never counted early.
        assert!(final_page.exact);
        assert_eq!(final_page.total, 1);
        let empty = request_for_empty();
        assert!(scan(&empty, || true, |_| panic!("cancelled scan published progress")).is_none());
    }

    fn request_for_empty() -> Request {
        request(Snapshot::default(), Filter::default(), 0)
    }

    #[test]
    fn previews_preserve_utf8_and_escape_raw_invalid_control_bytes() {
        assert_eq!(preview("INFO 日本語 🦀".as_bytes()), "INFO 日本語 🦀");
        assert_eq!(preview(b"INFO a\0\t\x1B\x7F\xFF\\xFF"), "INFO a\\x00\\x09\\x1B\\x7F\\xFF\\\\xFF");
        assert_eq!(preview(b"x\xE2\x82"), "x\\xE2\\x82");
        assert_eq!(preview("x\u{0080}".as_bytes()), "x\\xC2\\x80");
        let page = query(b"INFO a\xFF\nERROR b\t", Filter::default(), 0);
        assert_eq!(page.hits[0].preview, "INFO a\\xFF");
        assert_eq!(page.hits[1].preview, "ERROR b\\x09");
    }

    #[test]
    fn previews_never_split_scalars_or_escapes_or_exceed_512_bytes() {
        let mut prefix = vec![b'x'; PREVIEW_BYTES - 1];
        prefix.extend_from_slice("🦀tail".as_bytes());
        assert_eq!(preview(&prefix).len(), PREVIEW_BYTES - 1);
        prefix.truncate(PREVIEW_BYTES - 1);
        prefix.extend_from_slice(b"\xFFtail");
        assert_eq!(preview(&prefix).len(), PREVIEW_BYTES - 1);
        assert_eq!(preview(&vec![b'x'; MAX_PREFIX_BYTES]).len(), PREVIEW_BYTES);
        assert_eq!(preview(&vec![0xff; MAX_PREFIX_BYTES]).len(), PREVIEW_BYTES);
        let text = (0..300).map(|_| "INFO 🦀\n").collect::<String>();
        let page = query(text.as_bytes(), Filter::default(), 0);
        assert_eq!(page.hits.len(), PAGE_HITS);
        assert!(page.hits.iter().all(|hit| hit.preview.len() <= PREVIEW_BYTES));
        assert!(page.hits.iter().map(|hit| hit.preview.len()).sum::<usize>() <= 128 * 1024);
    }

    fn wait_final(session: &LogSession, events: &mpsc::Receiver<()>, generation: u64) -> Report {
        let deadline = Instant::now() + WAIT;
        for _ in 0..64 {
            for report in session.drain() {
                assert_eq!(report.generation, generation, "stale generation escaped drain");
                if match &report.result {
                    Ok(page) => page.exact,
                    Err(_) => true,
                } {
                    return report;
                }
            }
            events.recv_timeout(deadline.saturating_duration_since(Instant::now())).expect("worker wake timeout");
        }
        panic!("log worker did not produce bounded terminal report");
    }

    // The first wake deliberately parks the worker outside every mailbox lock.
    // Channels with deadlines make replacement/retirement races deterministic;
    // no timing sleeps, large workloads, spinning or stress loops are needed.
    fn gated_session() -> (LogSession, mpsc::Receiver<()>, mpsc::Receiver<()>, mpsc::SyncSender<()>) {
        gated_session_with_index(0)
    }
    fn gated_session_with_index(
        index_limit: u64,
    ) -> (LogSession, mpsc::Receiver<()>, mpsc::Receiver<()>, mpsc::SyncSender<()>) {
        let (events_tx, events_rx) = mpsc::sync_channel(16);
        let (parked_tx, parked_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let release_rx = Mutex::new(release_rx);
        let parked = AtomicBool::new(false);
        let session = LogSession::start_with_index_limit(index_limit, move || {
            let _ = events_tx.try_send(());
            if !parked.swap(true, Ordering::AcqRel) {
                parked_tx.send(()).unwrap();
                release_rx.lock().unwrap().recv_timeout(WAIT).expect("test worker gate timeout");
            }
        })
        .unwrap();
        (session, events_rx, parked_rx, release_tx)
    }

    #[test]
    fn rapid_replacement_rejects_with_owned_source_and_drops_stale_completions() {
        let (mut session, events, parked, release) = gated_session();
        let big = snapshot(&vec![b'x'; CHUNK_BYTES as usize * 6]);
        session.submit(1, 2, 3, big, Filter::default(), 0).unwrap();
        parked.recv_timeout(WAIT).unwrap();
        assert!(!session.drain().pop().unwrap().result.unwrap().exact);
        let mut newest = 0;
        for revision in 0..=RETIRED {
            newest = session
                .submit(7, revision as u64, 100 + revision as u64, snapshot(b"ERROR newest"), Filter::default(), 0)
                .unwrap();
        }
        let rejected = session.submit(9, 9, 9, snapshot(b"INFO rejected"), Filter::default(), 0).unwrap_err();
        assert_eq!(rejected.reason, "log retirement full; retry on wake");
        assert_eq!(rejected.snapshot.len(), b"INFO rejected".len() as u64);
        {
            let mailbox = session.shared.mailbox.lock().unwrap();
            assert_eq!(mailbox.retired.iter().flatten().count(), RETIRED);
            assert_eq!(mailbox.pending.as_ref().unwrap().key.generation, newest);
            assert!(mailbox.report.is_none());
        }
        release.send(()).unwrap();
        let report = wait_final(&session, &events, newest);
        assert_eq!((report.document, report.revision, report.epoch), (7, RETIRED as u64, 100 + RETIRED as u64));
        let page = report.result.unwrap();
        assert_eq!((page.total, page.hits[0].preview.as_str()), (1, "ERROR newest"));
        let shared = session.shared.clone();
        session.stop().join().unwrap();
        let mailbox = shared.mailbox.lock().unwrap();
        assert!(mailbox.pending.is_none());
        assert!(mailbox.retired.iter().all(Option::is_none));
        // Test runner is not the input thread: transfer the returned source to
        // this off-UI shutdown scope, rather than dropping an error on input.
        drop(rejected);
    }

    #[test]
    fn stop_is_nonblocking_and_worker_retires_pending_sources_before_join() {
        let (mut session, _events, parked, release) = gated_session();
        session.submit(1, 1, 1, snapshot(b"INFO active"), Filter::default(), 0).unwrap();
        parked.recv_timeout(WAIT).unwrap();
        for revision in 0..=RETIRED {
            session.submit(2, revision as u64, 2, snapshot(b"ERROR pending"), Filter::default(), 0).unwrap();
        }
        let shared = session.shared.clone();
        let join = session.stop();
        assert!(!join.session.worker.as_ref().unwrap().is_finished(), "stop must not wait for the gated worker");
        assert_eq!(shared.mailbox.lock().unwrap().retired.iter().flatten().count(), RETIRED);
        release.send(()).unwrap();
        join.join().unwrap();
        let mailbox = shared.mailbox.lock().unwrap();
        assert!(mailbox.pending.is_none());
        assert!(mailbox.retired.iter().all(Option::is_none));
        assert!(mailbox.report.is_none());
    }

    #[test]
    fn busy_submission_returns_source_and_drain_requests_an_event_retry() {
        let (events_tx, events) = mpsc::sync_channel(4);
        let mut session = LogSession::start(move || {
            let _ = events_tx.try_send(());
        })
        .unwrap();
        let shared = session.shared.clone();
        let guard = shared.mailbox.lock().unwrap();
        let rejected = session.submit(1, 1, 1, snapshot(b"INFO owned"), Filter::default(), 0).unwrap_err();
        assert_eq!(rejected.reason, "log mailbox busy; retry on wake");
        assert_eq!(rejected.snapshot.len(), 10);
        assert!(session.drain().is_empty());
        drop(guard);
        events.recv_timeout(WAIT).unwrap();
        let mut source = rejected.snapshot;
        let mut accepted = None;
        for _ in 0..64 {
            match session.submit(1, 1, 2, source, Filter::default(), 0) {
                Ok(generation) => {
                    accepted = Some(generation);
                    break;
                }
                Err(error) => {
                    assert_eq!(error.reason, "log mailbox busy; retry on wake");
                    source = error.snapshot;
                    events.recv_timeout(WAIT).unwrap();
                }
            }
        }
        let generation = accepted.expect("worker did not release metadata lock");
        let report = wait_final(&session, &events, generation);
        assert_eq!((report.document, report.revision, report.epoch), (1, 1, 2));
        session.stop().join().unwrap();
    }

    #[test]
    fn immutable_snapshot_keeps_original_content_after_buffer_edit() {
        let mut buffer = Buffer::empty();
        buffer.insert(0, b"INFO old").unwrap();
        let saved = buffer.snapshot();
        buffer.insert(0, b"ERROR new\n").unwrap();
        let page = scan(&request(saved, Filter::default(), 0), || false, |_| {}).unwrap().unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.hits[0].preview, "INFO old");
    }

    #[test]
    fn reused_document_revision_new_epoch_rejects_late_old_publication() {
        let (mut session, events, parked, release) = gated_session();
        let old = session.submit(7, 11, 1, snapshot(b"INFO old source"), Filter::default(), 0).unwrap();
        parked.recv_timeout(WAIT).unwrap();
        let new = session.submit(7, 11, 2, snapshot(b"ERROR rotated source"), Filter::default(), 0).unwrap();
        assert!(session.drain().is_empty());
        let wakes = AtomicU64::new(0);
        session.shared.publish(
            Report { document: 7, revision: 11, epoch: 1, generation: old, result: Err("late stale failure".into()) },
            &|| {
                wakes.fetch_add(1, Ordering::Relaxed);
            },
        );
        assert!(session.drain().is_empty());
        assert_eq!(wakes.load(Ordering::Relaxed), 0);
        release.send(()).unwrap();
        let report = wait_final(&session, &events, new);
        assert_eq!((report.document, report.revision, report.epoch), (7, 11, 2));
        assert_eq!(report.result.unwrap().hits[0].preview, "ERROR rotated source");
        session.stop().join().unwrap();
    }

    #[test]
    fn changed_file_reports_failure_instead_of_exact_old_count() {
        use std::fs;
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "kwil-log-session-{}-{}.log",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, b"INFO original").unwrap();
        let buffer = Buffer::from_file(kwil_core_text::TextFile::open(&path).unwrap()).unwrap();
        let old = buffer.snapshot();
        // Length changes avoid filesystem timestamp resolution races.
        fs::write(&path, b"ERROR changed file with different length").unwrap();
        let (events_tx, events) = mpsc::sync_channel(4);
        let mut session = LogSession::start(move || {
            let _ = events_tx.try_send(());
        })
        .unwrap();
        let generation = session.submit(4, 5, 6, old, Filter::default(), 0).unwrap();
        let report = wait_final(&session, &events, generation);
        assert_eq!((report.document, report.revision, report.epoch), (4, 5, 6));
        assert!(report.result.unwrap_err().contains("file changed"));
        session.stop().join().unwrap();
        drop(buffer);
        fs::remove_file(path).unwrap();
    }
}
