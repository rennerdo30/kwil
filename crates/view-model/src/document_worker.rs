//! One bounded file-I/O lane shared by all tabs. Callbacks wake the frontend;
//! the UI never polls for work or launches a thread per file/save.
use crate::Document;
use kwil_core_text::{decoded_backing::DecodedPreview, encoding::SavePlan};
use provenance::journal::RecoveryPolicy;
#[path = "binary_offer.rs"]
pub mod binary_offer;

/// A save found its file changed on disk since it was opened or last saved.
/// The workspace answers it with a choice (overwrite, reload) instead of a
/// dead end.
#[derive(Debug)]
pub struct ExternalChange;
const EXTERNAL_CHANGE: &str = "the file changed on disk since it was opened";
impl std::fmt::Display for ExternalChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(EXTERNAL_CHANGE)
    }
}
impl std::error::Error for ExternalChange {}
/// The result transport re-creates errors from their text, so the type alone
/// does not survive; the fixed message identifies it.
pub(crate) fn is_external_change(error: &io::Error) -> bool {
    error.get_ref().is_some_and(|inner| inner.is::<ExternalChange>()) || error.to_string().contains(EXTERNAL_CHANGE)
}
#[cfg(test)]
mod compressed_runtime;
mod decoded;
mod encoding_runtime;
mod encoding_seed;
mod hex_runtime;
pub mod result_transport;
#[cfg(test)]
mod scheduler_tests;
#[cfg(test)]
mod source_barrier_flush_tests;
pub(crate) use encoding_runtime::document_policy;
pub mod provenance;
pub mod retirement;
pub use decoded::{PreparedDecodedDocument, prepare_decoded_document};
use kwil_core_text::{Buffer, TextFile};
use kwil_core_workspace::{SaveRequest, SavedSnapshot, session::DocumentId};
use kwil_core_workspace::{
    journal::{JournalBase, JournalService, JournalSession, RecoveredJournal, RecoveryEvent},
    session::{self, Session, Tab},
};
pub use result_transport::{
    DocumentBarrierId, DocumentBarrierReceipt, DocumentBarrierRequest, DocumentDeliveryFailure, DocumentDeliveryId,
    DocumentProducerRetryRefusal, DocumentProducerRetryToken, DocumentResultReceiveError, DocumentResultReceiver,
    DocumentResultSettlementFailure, DocumentResultSettlementReason, DocumentResultSettlementTicket,
    DocumentResultStopRefusal, DocumentResultTransportStatus, DocumentWrapRetryRefusal, DocumentWrapRetryToken,
};
// Per-worker test configuration only. The existing journal scheduler Control
// stays unchanged; no shipping worker field or fault switch is introduced.
#[cfg(test)]
enum ScheduleTestControl {
    Journal(scheduler_tests::Control),
    Save(SaveQuitTestControl),
}
#[cfg(test)]
impl ScheduleTestControl {
    fn received(
        &mut self,
        request: &DocumentRequest,
        deferred: &std::collections::VecDeque<DocumentRequest>,
        storage: Option<&Storage>,
    ) {
        if let Self::Journal(control) = self {
            control.received(request, deferred, storage);
        }
    }
    fn applying(&mut self, request: &DocumentRequest) {
        if let Self::Journal(control) = self {
            control.applying(request);
        }
    }
    fn hold_ready(&mut self, document: DocumentId, generation: u64, storage: Option<&mut Storage>) {
        if let Self::Journal(control) = self {
            control.hold_ready(document, generation, storage);
        }
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SaveQuitTestEvent {
    Ready,
    BeforeEffect { revision: u64 },
    BeforePublish { revision: u64, error: Option<io::ErrorKind> },
}

/// Holds only owner-free observations. All pauses run synchronously on the
/// original producer, outside transport locks, under the fixture's deadline.
#[cfg(test)]
#[derive(Clone)]
pub(crate) struct SaveQuitTestGate {
    pub(crate) events: mpsc::SyncSender<SaveQuitTestEvent>,
    pub(crate) release: std::sync::Arc<std::sync::Mutex<mpsc::Receiver<()>>>,
    pub(crate) holding: std::sync::Arc<AtomicBool>,
    pub(crate) deadline: std::time::Instant,
}
#[cfg(test)]
impl SaveQuitTestGate {
    pub(crate) fn pause(&self, event: SaveQuitTestEvent) {
        if !self.holding.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        self.events.try_send(event).expect("bounded per-worker save observation");
        self.release
            .lock()
            .unwrap()
            .recv_timeout(self.deadline.saturating_duration_since(std::time::Instant::now()))
            .expect("original save fixture deadline and explicit release");
    }
}
#[cfg(test)]
pub(crate) struct SaveQuitTestControl {
    pub(crate) document: DocumentId,
    pub(crate) deny_next: bool,
    pub(crate) gate: SaveQuitTestGate,
}
#[cfg(test)]
impl SaveQuitTestControl {
    fn before_effect(&mut self, document: DocumentId, revision: u64) -> io::Result<()> {
        if document != self.document {
            return Ok(());
        }
        self.gate.pause(SaveQuitTestEvent::BeforeEffect { revision });
        if std::mem::take(&mut self.deny_next) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied"));
        }
        Ok(())
    }
    fn before_publish(&self, document: DocumentId, revision: u64, result: &io::Result<SavedSnapshot>) {
        if document == self.document {
            self.gate.pause(SaveQuitTestEvent::BeforePublish {
                revision,
                error: result.as_ref().err().map(io::Error::kind),
            });
        }
    }
}
pub(crate) use result_transport::{DocumentContinuationPermit, DocumentJournalSink};
#[cfg(test)]
use std::sync::mpsc::SyncSender;
use std::{
    collections::HashMap,
    io,
    path::PathBuf,
    sync::{
        atomic::AtomicBool,
        mpsc::{self, TrySendError},
    },
    time::Duration,
};

/// Ok acknowledges bounded queue admission, not display or durable delivery.
/// Explicit inbox close retains admitted/racing requests until drain or drop;
/// the frontend must gate dispatch during exit. If the wake fails after
/// admission, the request is not retried (which could open it twice); a shutdown
/// notice is emitted and later calls return Disconnected.
pub type ExternalOpen = Box<
    dyn Fn(
            kwil_core_workspace::instance::OpenFiles,
        ) -> Result<(), TrySendError<kwil_core_workspace::instance::OpenFiles>>
        + Send,
>;
pub struct ExternalOpenInbox {
    rx: mpsc::Receiver<kwil_core_workspace::instance::OpenFiles>,
    pending: std::sync::Arc<AtomicBool>,
    closed: std::sync::Arc<AtomicBool>,
}

pub fn external_open_channel(
    wake: impl Fn() -> io::Result<()> + Send + Sync + 'static,
) -> (ExternalOpen, ExternalOpenInbox) {
    let (tx, rx) = mpsc::sync_channel(16);
    let pending = std::sync::Arc::new(AtomicBool::new(false));
    let wake_pending = pending.clone();
    let closed = std::sync::Arc::new(AtomicBool::new(false));
    let inbox_closed = closed.clone();
    let open = Box::new(move |request: kwil_core_workspace::instance::OpenFiles| {
        // Reuse the transport's native-path, count and byte limits even when
        // an embedder calls this seam without going through IPC.
        if closed.load(std::sync::atomic::Ordering::Acquire) || request.encode().is_err() {
            return Err(TrySendError::Disconnected(request));
        }
        tx.try_send(request)?;
        if !wake_pending.swap(true, std::sync::atomic::Ordering::AcqRel) && wake().is_err() {
            closed.store(true, std::sync::atomic::Ordering::Release);
            eprintln!("Kwil is closing; an admitted open request may not have been displayed");
        }
        Ok(())
    });
    (open, ExternalOpenInbox { rx, pending, closed: inbox_closed })
}

impl ExternalOpenInbox {
    /// Advisory coalesced wake state. Modal hosts leave the bounded inbox
    /// untouched and use this to reschedule its existing drain on release.
    pub fn has_pending(&self) -> bool {
        self.pending.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Close admission without waiting for a producer or its wake callback.
    /// Requests already admitted, including a producer racing this store, stay
    /// in the bounded inbox. Admission acknowledges neither display nor durable
    /// delivery; a closing frontend must not dispatch those retained requests.
    pub fn close(&self) {
        self.closed.store(true, std::sync::atomic::Ordering::Release);
    }

    pub fn drain(&self) -> Vec<kwil_core_workspace::instance::OpenFiles> {
        // Closing does not consume accepted requests. Callers can still drain
        // their bounded ownership explicitly; the GUI skips this during exit.
        // Clear first: a concurrent producer either joins this bounded drain
        // or schedules another wake, so no request can be stranded.
        self.pending.store(false, std::sync::atomic::Ordering::Release);
        (0..16).map_while(|_| self.rx.try_recv().ok()).collect()
    }
}

/// Source-free identity for one already accepted deferred save completion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SaveResumeKey {
    pub(crate) document: DocumentId,
    pub(crate) source_epoch: u64,
    pub(crate) revision: u64,
    pub(crate) generation: u64,
}

#[derive(Clone, Copy, Debug)]
pub enum FileAction {
    Reload(Option<kwil_core_text::encoding::Encoding>),
    Keep,
    Encoding(kwil_core_text::encoding::Encoding),
    LineEnding(crate::DocumentLineEnding),
}
impl FileAction {
    pub(crate) fn resets_journal(self) -> bool {
        matches!(self, Self::Reload(_) | Self::Keep | Self::Encoding(_) | Self::LineEnding(_))
    }
    pub(crate) fn notice(self) -> &'static str {
        match self {
            Self::Reload(_) => "Reloaded from disk",
            Self::Keep => "Local changes kept; disk version marked as seen",
            Self::Encoding(_) => "Encoding selected for the next save",
            Self::LineEnding(_) => "Line endings converted; save to write changes",
        }
    }
}

pub enum DocumentRequest {
    SetFolder {
        path: PathBuf,
    },
    FileAction {
        key: kwil_core_search::worker::SearchKey,
        operation: u64,
        editor: Box<crate::Editor>,
        action: FileAction,
        journal: u64,
    },
    ResumeSaveCompletion {
        key: SaveResumeKey,
    },
    /// Privately minted control through this SAME bounded request lane.
    SourceBarrier(result_transport::DocumentBarrierRequest),
    /// Concrete journal delivery; admitted metadata follows its exact receipt.
    SealedJournal(result_transport::SealedJournalRequest),
    /// Already accepted recovery merge returns through the same result slot.
    RestorePrepared(result_transport::RestorePreparedRequest),
    /// Accepted exit only: settle the shared wrap worker before this lane may
    /// clean document backings. The normal completion callback wakes the UI.
    SettleWrap {
        ticket: crate::workspace::workspace_wrap::WorkspaceWrapSettlementTicket,
        timeout: Duration,
    },
    /// Normal frontend open, including startup sources. The scalar ticket is
    /// bound to an exact path by the model before worker admission.
    OpenChecked {
        ticket: binary_offer::Ticket,
        path: PathBuf,
        file: Option<TextFile>,
    },
    ResolveBinary {
        ticket: binary_offer::Ticket,
        choice: binary_offer::Choice,
        rows: usize,
    },
    /// Recovery merged a startup placeholder into an already accepted policy.
    /// Return its never-painted source to this lane without decoding it.
    RetireSource {
        file: TextFile,
    },
    /// Explicit physical bytes, including BOM and compression headers.
    OpenRaw {
        document: DocumentId,
        revision: u64,
        path: PathBuf,
        rows: usize,
        generation: u64,
    },
    /// Exact Editor returns even on a dirty-text refusal or preparation error.
    ReopenRaw {
        document: DocumentId,
        generation: u64,
        editor: Box<crate::Editor>,
        rows: usize,
    },
    /// Symmetric transport exit. Uses the normal decompression/decode path;
    /// RawReopened returns the exact Editor on every refusal or failure.
    ReopenText {
        document: DocumentId,
        generation: u64,
        editor: Box<crate::Editor>,
    },
    HexPage {
        document: DocumentId,
        editor: Box<crate::Editor>,
        request: crate::hex_session::PageRequest,
    },
    HexEdit {
        document: DocumentId,
        editor: Box<crate::Editor>,
        request: crate::hex_session::EditRequest,
    },
    /// Only after the tab AND its retained undo history have been discarded.
    ReleaseHex {
        document: DocumentId,
        epoch: u64,
    },
    /// Final cached-editor eviction. Admission transfers ownership; rejection
    /// returns the intact request. Drop roots on this lane before fence release.
    DiscardHex {
        document: DocumentId,
        editor: Box<crate::Editor>,
    },
    SaveRaw {
        document: DocumentId,
        request: SaveRequest,
    },
    /// Ownership returns in Replaced even when validation/application fails.
    Replace {
        document: DocumentId,
        operation: u64,
        editor: Box<crate::Editor>,
        recipe: kwil_core_search::worker::ReplaceRecipe,
    },
    Format {
        key: kwil_core_search::worker::SearchKey,
        operation: u64,
        editor: Box<crate::Editor>,
        selection: bool,
        language: Option<String>,
    },
    Prepare {
        root: Option<PathBuf>,
    },
    Persist(Session),
    SessionTimer,
    Recover {
        tab: Tab,
    },
    Open {
        document: DocumentId,
        revision: u64,
        path: PathBuf,
    },
    Index {
        document: DocumentId,
        revision: u64,
        path: PathBuf,
        file: TextFile,
    },
    Save {
        document: DocumentId,
        request: SaveRequest,
        plan: SavePlan,
    },
    ValidateReopen {
        document: DocumentId,
        generation: u64,
        path: PathBuf,
        original: Option<TextFile>,
        cached: bool,
    },
    ReleaseReopen {
        document: DocumentId,
        generation: u64,
    },
    SeedJournal {
        document: DocumentId,
        revision: u64,
        snapshot: kwil_core_text::Snapshot,
        path: Option<PathBuf>,
        next_sequence: u64,
        generation: u64,
        policy: RecoveryPolicy,
    },
    Journal {
        document: DocumentId,
        receipt: crate::EditReceipt,
        path: Option<PathBuf>,
        first_sequence: u64,
        generation: u64,
        policy: RecoveryPolicy,
    },
    FinishJournal {
        document: DocumentId,
        generation: u64,
    },
    FlushJournals,
}

pub enum DocumentResult {
    FolderReady(io::Result<PathBuf>),
    ResumeSaveCompletion {
        key: SaveResumeKey,
    },
    SourceBarrier(result_transport::DocumentBarrierReceipt),
    WrapSettled(
        Result<
            crate::workspace::workspace_wrap::WorkspaceWrapSettled,
            crate::workspace::workspace_wrap::WorkspaceWrapSettlementFailure,
        >,
    ),
    BinaryOffer(binary_offer::Offer),
    BinaryRefused {
        offer: binary_offer::Offer,
        error: io::Error,
    },
    CheckedPreview {
        ticket: binary_offer::Ticket,
        preview: DecodedPreview,
    },
    CheckedRawPreview {
        ticket: binary_offer::Ticket,
        page: crate::hex_session::Page,
    },
    /// None acknowledges cancellation/retirement on the lane. No owner crosses
    /// with offers, refusals, cancellation, or first-screen text previews.
    CheckedOpened {
        ticket: binary_offer::Ticket,
        result: io::Result<Option<Box<Document>>>,
    },
    RawPreview {
        document: DocumentId,
        revision: u64,
        page: crate::hex_session::Page,
    },
    RawReopened {
        document: DocumentId,
        generation: u64,
        editor: Box<crate::Editor>,
        result: io::Result<crate::hex_session::OpenOutcome>,
    },
    HexPage {
        document: DocumentId,
        editor: Box<crate::Editor>,
        request: crate::hex_session::PageRequest,
        result: io::Result<crate::hex_session::Page>,
    },
    HexEdited {
        document: DocumentId,
        editor: Box<crate::Editor>,
        completion: crate::hex_session::EditCompletion,
    },
    Replaced {
        key: kwil_core_search::worker::SearchKey,
        operation: u64,
        editor: Box<crate::Editor>,
        result: io::Result<crate::Damage>,
    },
    SessionCheckpointDue,
    CapacityAvailable,
    Stopped(io::Result<()>),
    Prepared(io::Result<PreparedWorkspace>),
    Persisted(io::Result<()>),
    Recovered {
        document: DocumentId,
        revision: u64,
        result: io::Result<Box<Document>>,
    },
    Opened {
        document: DocumentId,
        revision: u64,
        result: io::Result<(PathBuf, TextFile)>,
    },
    Preview {
        document: DocumentId,
        revision: u64,
        path: PathBuf,
        file: TextFile,
        preview: DecodedPreview,
    },
    Indexed {
        document: DocumentId,
        revision: u64,
        result: io::Result<Box<Document>>,
    },
    Saved {
        document: DocumentId,
        revision: u64,
        result: io::Result<SavedSnapshot>,
    },
    ReopenValidated {
        document: DocumentId,
        generation: u64,
        result: io::Result<bool>,
    },
    JournalQueued {
        document: DocumentId,
        revision: u64,
        sequence: u64,
        result: io::Result<()>,
    },
    JournalDurable {
        document: DocumentId,
        revision: u64,
        sequence: u64,
        result: io::Result<()>,
    },
    JournalClosed {
        document: DocumentId,
        result: io::Result<()>,
    },
}

pub struct PreparedWorkspace {
    pub session: Session,
    pub documents: Vec<(DocumentId, io::Result<Document>)>,
}

/// Shutdown could not retire every worker-owned backing. The bounded runtime
/// is quarantined on its existing worker for the rest of this process, keeping
/// exact owners alive without unlinking files still referenced by snapshots.
/// ROOT must release frontend/source snapshots before closing this worker.
#[derive(Debug)]
pub struct PendingBackingRetirement {
    pub root: PathBuf,
    pub live_owners: usize,
    pub cleanup_pending: usize,
    cleanup_error: Option<io::Error>,
    previous_error: Option<io::Error>,
    journal_error: Option<io::Error>,
}
impl std::fmt::Display for PendingBackingRetirement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Document backing retirement incomplete: {} live owners, {} cleanup pending in {}; exact owners quarantined until process exit",
            self.live_owners,
            self.cleanup_pending,
            self.root.display()
        )?;
        for error in [&self.cleanup_error, &self.previous_error, &self.journal_error].into_iter().flatten() {
            write!(f, "; {error}")?;
        }
        Ok(())
    }
}
impl std::error::Error for PendingBackingRetirement {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.cleanup_error
            .as_ref()
            .or(self.previous_error.as_ref())
            .or(self.journal_error.as_ref())
            .map(|error| error as &(dyn std::error::Error + 'static))
    }
}

struct Storage {
    root: PathBuf,
    _ownership: std::fs::File,
    service: JournalService,
    journals: HashMap<DocumentId, JournalState>,
    /// Finished writers still pin the exact previous provenance until a new
    /// lineage is durable, or metadata durably evicts the document entirely.
    finished_journals: HashMap<DocumentId, JournalState>,
    transports: HashMap<DocumentId, TransportSeed>,
    order: HashMap<DocumentId, u64>,
    durable_session: Session,
    encoding: encoding_runtime::EncodingRuntime,
}

#[derive(Clone, Copy)]
struct TransportSeed {
    policy: RecoveryPolicy,
    revision: u64,
    len: u64,
}
impl TransportSeed {
    fn document(document: &Document) -> Self {
        Self { policy: document_policy(document), revision: document.revision(), len: document.buffer().len() }
    }
}

fn reserve_transport(storage: &mut Storage, document: DocumentId) -> io::Result<()> {
    if storage.transports.contains_key(&document) || storage.transports.len() >= session::MAX_TABS {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "previous transport lineage is not durable; editor retained",
        ));
    }
    storage.transports.try_reserve(1).map_err(io::Error::other)
}

struct JournalState {
    policy: RecoveryPolicy,
    handle: JournalSession,
    origins: Option<provenance::OriginRegistry>,
    seed_origins: Option<mpsc::Receiver<io::Result<provenance::AdmissionToken>>>,
    revision: u64,
    sequence: u64,
    ready: bool,
    flush: Option<(u64, mpsc::Receiver<io::Result<u64>>)>,
    needs_flush: bool,
    failed: bool,
    flush_due: std::time::Instant,
    generation: u64,
    promotion: Option<PathBuf>,
    /// Scheduler proof only; no authority or source owner is stored here.
    barrier_durable_sequence: Option<u64>,
}
impl JournalState {
    fn new(handle: JournalSession, revision: u64, sequence: u64, policy: RecoveryPolicy) -> Self {
        Self {
            policy,
            handle,
            origins: None,
            seed_origins: None,
            revision,
            sequence,
            ready: false,
            flush: None,
            needs_flush: true,
            failed: false,
            flush_due: std::time::Instant::now() + Duration::from_secs(2),
            generation: 0,
            promotion: None,
            barrier_durable_sequence: None,
        }
    }
}

pub struct DocumentWorker {
    tx: result_transport::RequestSender,
    journal_capacity: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    journal_controls: std::sync::Arc<JournalControlCredits>,
    save_destinations: std::sync::Arc<std::sync::Mutex<Vec<(DocumentId, PathBuf)>>>,
}

// Deferred receipts already retain one of the 16 observer credits. Seed and
// Finish requests need fixed credits now that lineage barriers must be retained
// rather than coalesced across one another. One seed per active tab plus one
// close per active/cached tab bounds deferred controls at 128 + 192; separate
// quotas prevent seeds waiting for a slot from consuming close admission.
const JOURNAL_SEED_CAPACITY: usize = session::MAX_TABS;
const JOURNAL_FINISH_CAPACITY: usize = session::MAX_TABS + session::MAX_CLOSED;
const DEFERRED_REQUEST_LIMIT: usize = JOURNAL_SEED_CAPACITY + JOURNAL_FINISH_CAPACITY + 16 + 1;
pub(super) const DEFERRED_STORAGE_CAPACITY: usize = DEFERRED_REQUEST_LIMIT.next_power_of_two();
const MAX_BARRIER_TARGETS: usize = session::MAX_TABS + JOURNAL_SEED_CAPACITY + 16;

#[derive(Clone, Copy)]
struct SourceBarrierTarget {
    document: DocumentId,
    generation: u64,
    sequence: u64,
}
pub(super) struct SourceBarrierTracker {
    request: result_transport::DocumentBarrierRequest,
    targets: [Option<SourceBarrierTarget>; MAX_BARRIER_TARGETS],
    failed: bool,
}
impl SourceBarrierTracker {
    fn new(request: result_transport::DocumentBarrierRequest, storage: Option<&mut Storage>) -> Self {
        let mut tracker = Self { request, targets: [None; MAX_BARRIER_TARGETS], failed: false };
        if let Some(storage) = storage {
            for (&document, journal) in &mut storage.journals {
                tracker.record_journal(document, journal);
            }
        }
        tracker
    }
    fn record_journal(&mut self, document: DocumentId, journal: &mut JournalState) {
        let target = SourceBarrierTarget { document, generation: journal.generation, sequence: journal.sequence };
        if let Some(slot) =
            self.targets.iter_mut().find(|target| target.as_ref().is_some_and(|target| target.document == document))
        {
            *slot = Some(target);
        } else if let Some(slot) = self.targets.iter_mut().find(|target| target.is_none()) {
            *slot = Some(target);
        } else {
            self.failed = true;
        }
        // A barrier is a real flush trigger, not a new readiness timer. Later
        // receipts do not expand this target, so a younger Finish cannot form
        // a wait cycle with acknowledgement of this marker.
        if !journal.failed {
            let durable = journal.barrier_durable_sequence.is_some_and(|durable| durable >= target.sequence);
            let flushing_target = journal.flush.as_ref().is_some_and(|(sequence, _)| *sequence >= target.sequence);
            if !durable && !flushing_target {
                journal.needs_flush = true;
            }
            // Drain an existing need immediately, including a redundant flush
            // whose target was already durable. An adequate outstanding flush
            // alone must not manufacture a second successor flush.
            if journal.needs_flush {
                journal.flush_due = std::time::Instant::now();
            }
        }
    }
    fn record(&mut self, document: DocumentId, storage: Option<&mut Storage>) {
        if let Some(journal) = storage.and_then(|storage| storage.journals.get_mut(&document)) {
            self.record_journal(document, journal);
        }
    }
    fn expedite_pending_flush(&self, document: DocumentId, storage: Option<&mut Storage>) {
        if let Some(journal) = storage.and_then(|storage| storage.journals.get_mut(&document))
            && !journal.failed
            && journal.needs_flush
            && self
                .targets
                .iter()
                .flatten()
                .any(|target| target.document == document && target.generation == journal.generation)
        {
            // Accepted younger work may create a need after marker capture.
            // Advance only that existing need; never expand the fixed target,
            // replace its receiver, or manufacture a successor obligation.
            journal.flush_due = std::time::Instant::now();
        }
    }
    fn ready(&self, storage: Option<&Storage>) -> bool {
        self.failed
            || self.targets.iter().flatten().all(|target| {
                storage.and_then(|storage| storage.journals.get(&target.document)).is_none_or(|journal| {
                    journal.failed
                        || journal.generation != target.generation
                        || (journal.ready
                            && journal.barrier_durable_sequence.is_some_and(|sequence| sequence >= target.sequence)
                            && journal.flush.is_none()
                            && !journal.needs_flush)
                })
            })
    }
}

fn unpack_sealed(request: DocumentRequest) -> (DocumentRequest, Option<crate::visual_rows::MetadataReservation>) {
    match request {
        DocumentRequest::SealedJournal(packet) => {
            let result_transport::SealedJournalRequest {
                document,
                receipt,
                path,
                first_sequence,
                generation,
                policy,
                charge,
            } = packet;
            (DocumentRequest::Journal { document, receipt, path, first_sequence, generation, policy }, Some(charge))
        }
        request => (request, None),
    }
}

struct JournalControlCredits {
    seeds: std::sync::atomic::AtomicUsize,
    finishes: std::sync::atomic::AtomicUsize,
}

impl JournalControlCredits {
    fn new() -> Self {
        Self {
            seeds: std::sync::atomic::AtomicUsize::new(JOURNAL_SEED_CAPACITY),
            finishes: std::sync::atomic::AtomicUsize::new(JOURNAL_FINISH_CAPACITY),
        }
    }

    fn for_request(&self, request: &DocumentRequest) -> Option<&std::sync::atomic::AtomicUsize> {
        match request {
            DocumentRequest::SeedJournal { .. } => Some(&self.seeds),
            DocumentRequest::FinishJournal { .. } => Some(&self.finishes),
            _ => None,
        }
    }

    fn release(&self, request: &DocumentRequest) {
        if let Some(credit) = self.for_request(request) {
            let previous = credit.fetch_add(1, std::sync::atomic::Ordering::Release);
            let maximum = if matches!(request, DocumentRequest::SeedJournal { .. }) {
                JOURNAL_SEED_CAPACITY
            } else {
                JOURNAL_FINISH_CAPACITY
            };
            debug_assert!(previous < maximum, "journal control credit returned twice");
        }
    }
}

fn journal_document(request: &DocumentRequest) -> Option<DocumentId> {
    match request {
        DocumentRequest::FileAction { key, .. } => Some(DocumentId(key.document_id)),
        DocumentRequest::SealedJournal(packet) => Some(packet.document),
        DocumentRequest::ResumeSaveCompletion { key } => Some(key.document),
        DocumentRequest::SeedJournal { document, .. }
        | DocumentRequest::Journal { document, .. }
        | DocumentRequest::FinishJournal { document, .. } => Some(*document),
        _ => None,
    }
}

/// Only the first pending operation for a document may change its journal
/// lineage. Other documents can still advance while this one's seed is busy.
/// `before` excludes younger successors when selecting a deferred request.
fn journal_can_run(
    request: &DocumentRequest,
    deferred: &std::collections::VecDeque<DocumentRequest>,
    before: usize,
    storage: Option<&Storage>,
) -> bool {
    let Some(document) = journal_document(request) else { return true };
    if deferred.iter().take(before).any(|older| journal_document(older) == Some(document)) {
        return false;
    }
    if matches!(request, DocumentRequest::FileAction { .. })
        && storage.is_some_and(|storage| storage.transports.contains_key(&document))
    {
        return false;
    }
    // A scalar completion respects preceding same-document journal work, but
    // cannot consume a journal slot or require a new journal lineage.
    if matches!(request, DocumentRequest::ResumeSaveCompletion { .. } | DocumentRequest::FileAction { .. }) {
        return true;
    }
    let Some(storage) = storage else { return true };
    if matches!(request, DocumentRequest::Journal { .. } | DocumentRequest::SealedJournal(_))
        && storage.journals.get(&document).is_some_and(|journal| !journal.ready && !journal.failed)
    {
        return false;
    }
    matches!(request, DocumentRequest::FinishJournal { .. })
        || storage.journals.contains_key(&document)
        || storage.journals.len() < session::MAX_TABS
}

impl DocumentWorker {
    pub(crate) fn set_save_destinations(&self, destinations: Vec<(DocumentId, PathBuf)>) {
        debug_assert!(destinations.len() <= session::MAX_TABS * 2);
        *self.save_destinations.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = destinations;
    }
    /// The model retains one receipt on Full and centrally gates subsequent
    /// document mutations. This callback only transfers ownership; no I/O or
    /// replacement iteration runs on the UI thread.
    pub fn observer(
        &self,
        document: DocumentId,
        path: Option<PathBuf>,
        first_sequence: u64,
        generation: u64,
        policy: RecoveryPolicy,
    ) -> crate::EditObserver {
        let sender = self.tx.clone();
        let capacity = self.journal_capacity.clone();
        Box::new(move |receipt| {
            if capacity
                .try_update(std::sync::atomic::Ordering::AcqRel, std::sync::atomic::Ordering::Acquire, |left| {
                    left.checked_sub(1)
                })
                .is_err()
            {
                return Err(receipt);
            }
            let request =
                DocumentRequest::Journal { document, receipt, path: path.clone(), first_sequence, generation, policy };
            let gate = sender.lock_sender();
            let delivered = gate.try_send(request);
            drop(gate);
            match delivered {
                std::ops::ControlFlow::Continue(()) => Ok(()),
                std::ops::ControlFlow::Break(
                    TrySendError::Full(DocumentRequest::Journal { receipt, .. })
                    | TrySendError::Disconnected(DocumentRequest::Journal { receipt, .. }),
                ) => {
                    capacity.fetch_add(1, std::sync::atomic::Ordering::Release);
                    Err(receipt)
                }
                _ => unreachable!("journal observer submits only journal receipts"),
            }
        })
    }
    pub fn spawn(complete: impl Fn(DocumentResult) + Send + 'static) -> io::Result<Self> {
        Self::spawn_inner(
            complete,
            #[cfg(test)]
            None,
        )
    }

    fn spawn_inner(
        complete: impl Fn(DocumentResult) + Send + 'static,
        #[cfg(test)] schedule_control: Option<scheduler_tests::Control>,
    ) -> io::Result<Self> {
        Self::spawn_transport_inner(
            complete,
            None,
            #[cfg(test)]
            schedule_control.map(ScheduleTestControl::Journal),
        )
    }

    /// One worker-owned result slot, shared with the frontend's wrap aggregate.
    /// Wakes carry no document ownership. Receive/close/settle contracts live in
    /// `result_transport`; the legacy callback constructor remains unchanged.
    pub fn spawn_bounded(
        budget: &crate::visual_rows::AllocationBudget,
        wake: impl Fn() -> io::Result<()> + Send + Sync + 'static,
    ) -> io::Result<(Self, DocumentResultReceiver)> {
        let (shared, receiver) = result_transport::Shared::new(budget, wake)?;
        let delivery = shared.clone();
        let worker = Self::spawn_transport_inner(
            move |result| delivery.publish(result),
            Some(shared),
            #[cfg(test)]
            None,
        )?;
        Ok((worker, receiver))
    }

    #[cfg(test)]
    pub(crate) fn spawn_bounded_save_quit_test(
        budget: &crate::visual_rows::AllocationBudget,
        wake: impl Fn() -> io::Result<()> + Send + Sync + 'static,
        control: SaveQuitTestControl,
    ) -> io::Result<(Self, DocumentResultReceiver)> {
        let (shared, receiver) = result_transport::Shared::new(budget, wake)?;
        let delivery = shared.clone();
        let worker = Self::spawn_transport_inner(
            move |result| delivery.publish(result),
            Some(shared),
            Some(ScheduleTestControl::Save(control)),
        )?;
        Ok((worker, receiver))
    }

    fn spawn_transport_inner(
        complete: impl Fn(DocumentResult) + Send + 'static,
        transport: Option<std::sync::Arc<result_transport::Shared>>,
        #[cfg(test)] mut schedule_control: Option<ScheduleTestControl>,
    ) -> io::Result<Self> {
        let (tx, rx) = mpsc::sync_channel(16);
        let tx = result_transport::RequestSender::new(tx, transport.as_ref());
        let journal_capacity = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(16));
        let capacity = journal_capacity.clone();
        let journal_controls = std::sync::Arc::new(JournalControlCredits::new());
        let controls = journal_controls.clone();
        let save_destinations = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let destinations = save_destinations.clone();
        let mut deferred = std::collections::VecDeque::new();
        deferred.try_reserve_exact(DEFERRED_REQUEST_LIMIT).map_err(io::Error::other)?;
        if deferred.capacity() > DEFERRED_STORAGE_CAPACITY {
            return Err(io::Error::other("deferred document metadata capacity"));
        }
        let worker_transport = transport.clone();
        let worker = std::thread::Builder::new().name("kwil-documents".into()).spawn(move || {
            let _exit = result_transport::WorkerExit(worker_transport);
            if let Some(shared) = &_exit.0 {
                shared.register_worker();
            }
            // Existing requests drain on shutdown. Request-scoped cancellation
            // remains an integration task; never cancel an accepted save merely
            // because the UI released its sender.
            let cancel = AtomicBool::new(false);
            let mut storage: Option<Storage> = None;
            let mut hex = hex_runtime::HexRuntime::new();
            let mut binary = binary_offer::Runtime::new();
            let mut source_barrier: Option<SourceBarrierTracker> = None;
            let mut session_due = None;
            // Worker-owned until the UI publishes the admitted tab and releases
            // this reservation. UI registry refreshes cannot erase it early.
            let mut reopening: Option<(DocumentId, u64, PathBuf)> = None;
            let mut shutdown_deadline = None;
            let mut shutdown_error = None;
            loop {
                let waiting = session_due.is_some()
                    || storage.as_ref().is_some_and(|state| {
                        state.journals.values().any(|journal| {
                            !journal.failed && (!journal.ready || journal.flush.is_some() || journal.needs_flush)
                        })
                    });
                let barrier_index =
                    deferred.iter().position(|request| matches!(request, DocumentRequest::SourceBarrier(_)));
                let available = deferred.iter().enumerate().find_map(|(index, request)| {
                    let ready = if matches!(request, DocumentRequest::SourceBarrier(_)) {
                        index == 0
                            && session_due.is_none()
                            && source_barrier.as_ref().is_some_and(|barrier| barrier.ready(storage.as_ref()))
                    } else {
                        journal_can_run(request, &deferred, index, storage.as_ref())
                    };
                    ready.then_some(index)
                });
                let from_deferred = available.is_some();
                let precedes_barrier = available.zip(barrier_index).is_some_and(|(index, barrier)| index < barrier);
                let request = if let Some(index) = available {
                    deferred.remove(index)
                } else if let Some(deadline) = shutdown_deadline {
                    if deferred.is_empty() && !waiting {
                        break;
                    }
                    if std::time::Instant::now() >= deadline {
                        shutdown_error =
                            Some(io::Error::other("Document shutdown timed out draining admitted recovery work"));
                        break;
                    }
                    // Only active seed/flush work remains. Reuse the lane's
                    // existing bounded readiness wait; never spin on a closed inbox.
                    std::thread::park_timeout(Duration::from_millis(25));
                    None
                } else if waiting {
                    match rx.recv_timeout(Duration::from_millis(25)) {
                        Ok(request) => Some(request),
                        Err(mpsc::RecvTimeoutError::Timeout) => None,
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            shutdown_deadline = Some(std::time::Instant::now() + Duration::from_secs(30));
                            session_due = None;
                            if let Some(storage) = &mut storage {
                                for journal in storage.journals.values_mut() {
                                    journal.flush_due = std::time::Instant::now();
                                }
                            }
                            None
                        }
                    }
                } else {
                    match rx.recv() {
                        Ok(request) => Some(request),
                        Err(_) => {
                            shutdown_deadline = Some(std::time::Instant::now() + Duration::from_secs(30));
                            session_due = None;
                            None
                        }
                    }
                };
                if let Some(request) = request {
                    if let DocumentRequest::SourceBarrier(marker) = &request {
                        if source_barrier.is_none() {
                            source_barrier = Some(SourceBarrierTracker::new(*marker, storage.as_mut()));
                        }
                        if session_due.is_some() {
                            session_due = Some(std::time::Instant::now());
                        }
                    }
                    let preceding_journal = precedes_barrier.then(|| journal_document(&request)).flatten();
                    #[cfg(test)]
                    if !from_deferred && let Some(control) = &mut schedule_control {
                        control.received(&request, &deferred, storage.as_ref());
                    }
                    let can_run = if matches!(&request, DocumentRequest::SourceBarrier(_)) {
                        deferred.is_empty()
                            && session_due.is_none()
                            && source_barrier.as_ref().is_some_and(|barrier| barrier.ready(storage.as_ref()))
                    } else {
                        journal_can_run(&request, &deferred, deferred.len(), storage.as_ref())
                    };
                    if !from_deferred && !can_run {
                        debug_assert!(deferred.len() < DEFERRED_REQUEST_LIMIT);
                        deferred.push_back(request);
                        complete(DocumentResult::CapacityAvailable);
                        // Readiness/flush polling must still happen when the
                        // inbox supplies a successor to a blocked document.
                        if let Some(storage) = &mut storage {
                            poll_journals(storage, &complete);
                        }
                        continue;
                    }
                    controls.release(&request);
                    if let Some(shared) = &_exit.0 {
                        shared.request_capacity_available();
                    }
                    let (request, mut sealed_charge) = unpack_sealed(request);
                    #[cfg(test)]
                    if let Some(control) = &mut schedule_control {
                        control.applying(&request);
                    }
                    match request {
                        DocumentRequest::ResumeSaveCompletion { key } => {
                            complete(DocumentResult::ResumeSaveCompletion { key });
                        }
                        DocumentRequest::SourceBarrier(marker) => {
                            let tracker = source_barrier.take().expect("received barrier has one fixed tracker");
                            #[cfg(test)]
                            if tracker.failed || tracker.request != marker {
                                eprintln!(
                                    "document worker failed barrier: tracker_failed={} marker_equal={}",
                                    tracker.failed,
                                    tracker.request == marker,
                                );
                            }
                            let receipt = if tracker.failed || tracker.request != marker {
                                result_transport::DocumentBarrierReceipt::failed(marker)
                            } else {
                                result_transport::DocumentBarrierReceipt::reached(marker)
                            };
                            complete(DocumentResult::SourceBarrier(receipt));
                        }
                        DocumentRequest::RestorePrepared(packet) => {
                            complete(DocumentResult::Prepared(Ok(*packet.prepared)));
                        }
                        DocumentRequest::SealedJournal(_) => {
                            unreachable!("sealed packet unpacked before canonical journal application")
                        }
                        DocumentRequest::SettleWrap { ticket, timeout } => {
                            complete(DocumentResult::WrapSettled(ticket.settle(timeout)));
                        }
                        DocumentRequest::RetireSource { file } => {
                            drop(file);
                            complete(DocumentResult::CapacityAvailable);
                        }
                        DocumentRequest::OpenChecked { ticket, path, file } => {
                            let detected = if storage.is_some() {
                                binary.detect(ticket, path, file)
                            } else {
                                Err(io::Error::other("Workspace storage is not prepared"))
                            };
                            match detected {
                                Ok(binary_offer::Detection::Offer(offer)) => {
                                    complete(DocumentResult::BinaryOffer(offer))
                                }
                                other => {
                                    let result = other.and_then(|detected| {
                                        let binary_offer::Detection::Text(pending) = detected else { unreachable!() };
                                        let storage = storage.as_mut().expect("prepared checked open");
                                        storage.encoding.open(pending.path, pending.file, None, &cancel, |preview| {
                                            complete(DocumentResult::CheckedPreview { ticket, preview });
                                            Ok(())
                                        })
                                    });
                                    complete(DocumentResult::CheckedOpened {
                                        ticket,
                                        result: result.map(|d| Some(Box::new(d))),
                                    });
                                }
                            }
                        }
                        DocumentRequest::ResolveBinary { ticket, choice, rows } => {
                            let result: io::Result<Option<Box<Document>>> = (|| {
                                let pending = binary.pending(ticket)?;
                                if choice == binary_offer::Choice::Cancel {
                                    drop(binary.take(ticket)?);
                                    return Ok(None);
                                }
                                binary_offer::Runtime::unchanged(pending)?;
                                let storage = storage
                                    .as_mut()
                                    .ok_or_else(|| io::Error::other("Workspace storage is not prepared"))?;
                                // The pending slot keeps the exact source until
                                // admission succeeds, including backing refusal.
                                let retained =
                                    binary_offer::Pending { path: pending.path.clone(), file: pending.file.clone() };
                                let document = match choice {
                                    binary_offer::Choice::Hex => storage.encoding.open_raw_file(
                                        ticket.document(),
                                        retained,
                                        rows.min(crate::hex::MAX_ROWS),
                                        ticket.generation(),
                                        &cancel,
                                        |page| complete(DocumentResult::CheckedRawPreview { ticket, page }),
                                    ),
                                    binary_offer::Choice::Text => {
                                        storage.encoding.open(retained.path, retained.file, None, &cancel, |preview| {
                                            complete(DocumentResult::CheckedPreview { ticket, preview });
                                            Ok(())
                                        })
                                    }
                                    binary_offer::Choice::Cancel => unreachable!(),
                                }?;
                                Ok(Some(Box::new(document)))
                            })();
                            if result.as_ref().is_err_and(|error| error.kind() == io::ErrorKind::WouldBlock)
                                && let Ok(pending) = binary.pending(ticket)
                            {
                                complete(DocumentResult::BinaryRefused {
                                    offer: binary_offer::Offer { ticket, len: pending.file.len() },
                                    error: result.err().unwrap(),
                                });
                            } else {
                                // A stale choice never removes a newer source.
                                drop(binary.take(ticket));
                                complete(DocumentResult::CheckedOpened { ticket, result });
                            }
                        }
                        DocumentRequest::OpenRaw { document, revision, path, rows, generation } => {
                            let result = storage
                                .as_mut()
                                .ok_or_else(|| io::Error::other("Workspace storage is not prepared"))
                                .and_then(|storage| {
                                    storage.encoding.open_raw(document, path, rows, generation, &cancel, |page| {
                                        complete(DocumentResult::RawPreview { document, revision, page });
                                    })
                                });
                            complete(DocumentResult::Indexed { document, revision, result: result.map(Box::new) });
                        }
                        DocumentRequest::ReopenRaw { document, generation, mut editor, rows } => {
                            let before_epoch = editor.document().source_epoch;
                            let revision = editor.document().revision();
                            let active_journal = storage.as_ref().is_some_and(|s| s.journals.contains_key(&document));
                            let result = if editor.document().representation()
                                == crate::hex_session::Representation::DecodedText
                                && !editor.document().is_dirty()
                                && active_journal
                            {
                                Err(io::Error::new(
                                    io::ErrorKind::WouldBlock,
                                    "finish the old transport journal before raw reopen",
                                ))
                            } else {
                                hex_runtime::reopen(&mut editor, |path| {
                                    let storage = storage
                                        .as_mut()
                                        .ok_or_else(|| io::Error::other("Workspace storage is not prepared"))?;
                                    reserve_transport(storage, document)?;
                                    storage.encoding.open_raw(document, path, rows, generation, &cancel, |page| {
                                        complete(DocumentResult::RawPreview { document, revision, page });
                                    })
                                })
                            };
                            if result.as_ref().is_ok_and(|outcome| *outcome == crate::hex_session::OpenOutcome::Ready)
                                && editor.document().source_epoch != before_epoch
                            {
                                storage
                                    .as_mut()
                                    .expect("prepared transport storage")
                                    .transports
                                    .insert(document, TransportSeed::document(editor.document()));
                            }
                            complete(DocumentResult::RawReopened { document, generation, editor, result });
                        }
                        DocumentRequest::ReopenText { document, generation, mut editor } => {
                            let before_epoch = editor.document().source_epoch;
                            let revision = editor.document().revision();
                            let active_journal = storage.as_ref().is_some_and(|s| s.journals.contains_key(&document));
                            let result = if editor.document().representation()
                                == crate::hex_session::Representation::RawBytes
                                && !editor.document().is_dirty()
                                && active_journal
                            {
                                Err(io::Error::new(
                                    io::ErrorKind::WouldBlock,
                                    "finish the old transport journal before text reopen",
                                ))
                            } else {
                                hex_runtime::reopen_text(&mut editor, |path| {
                                    let storage = storage
                                        .as_mut()
                                        .ok_or_else(|| io::Error::other("Workspace storage is not prepared"))?;
                                    reserve_transport(storage, document)?;
                                    let file = kwil_core_workspace::open_for_editing(&path)?;
                                    storage.encoding.open(path.clone(), file.clone(), None, &cancel, |mut preview| {
                                        // Normal preview decoding can expand malformed bytes
                                        // into escapes. Bound the text-exit cache as well as
                                        // its encoded input, without clipping a UTF8 scalar.
                                        let limit = crate::hex_session::MAX_PAGE_BYTES;
                                        if preview.text.len() > limit {
                                            let mut end = limit;
                                            while end > 0 && preview.text[end] & 0xc0 == 0x80 {
                                                end -= 1;
                                            }
                                            preview.text.truncate(end);
                                            preview.text.shrink_to_fit();
                                            preview.complete = false;
                                        }
                                        complete(DocumentResult::Preview { document, revision, path, file, preview });
                                        Ok(())
                                    })
                                })
                            };
                            if result.as_ref().is_ok_and(|outcome| *outcome == crate::hex_session::OpenOutcome::Ready)
                                && editor.document().source_epoch != before_epoch
                            {
                                storage
                                    .as_mut()
                                    .expect("prepared transport storage")
                                    .transports
                                    .insert(document, TransportSeed::document(editor.document()));
                            }
                            complete(DocumentResult::RawReopened { document, generation, editor, result });
                        }
                        DocumentRequest::HexPage { document, editor, request } => {
                            let result = hex_runtime::page(document, &editor, request, &cancel);
                            complete(DocumentResult::HexPage { document, editor, request, result });
                        }
                        DocumentRequest::HexEdit { document, mut editor, request } => {
                            let completion = hex.edit(document, &mut editor, &request);
                            complete(DocumentResult::HexEdited { document, editor, completion });
                        }
                        DocumentRequest::ReleaseHex { document, epoch } => hex.release(document, epoch),
                        DocumentRequest::DiscardHex { document, editor } => {
                            hex.discard(document, editor);
                            complete(DocumentResult::CapacityAvailable);
                        }
                        DocumentRequest::SaveRaw { document, request } => {
                            let revision = request.revision;
                            let mut reserved =
                                destinations.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
                            if let Some((owner, _, path)) = &reopening {
                                reserved.push((*owner, path.clone()));
                            }
                            let result = validate_save_destination(document, &request.path, &reserved).and_then(|()| {
                                storage
                                    .as_mut()
                                    .ok_or_else(|| io::Error::other("Workspace storage is not prepared"))?
                                    .encoding
                                    .save_raw(request, &cancel)
                            });
                            complete(DocumentResult::Saved { document, revision, result });
                        }
                        DocumentRequest::SetFolder { path } => {
                            let result = std::fs::metadata(&path).and_then(|metadata| {
                                if path.is_absolute() && metadata.is_dir() {
                                    Ok(path)
                                } else {
                                    Err(io::Error::other("Choose an existing absolute folder path"))
                                }
                            });
                            complete(DocumentResult::FolderReady(result));
                        }
                        DocumentRequest::FileAction { key, operation, mut editor, action, journal } => {
                            let document = DocumentId(key.document_id);
                            let mut reseed = false;
                            let result = (|| {
                                editor.admit_worker_loan(key.revision)?;
                                let state =
                                    storage.as_mut().ok_or_else(|| io::Error::other("Workspace is not ready"))?;
                                if action.resets_journal() {
                                    reserve_transport(state, document)?;
                                }
                                match action {
                                    FileAction::Reload(encoding) => {
                                        editor.raw_reopen_ready()?;
                                        let path = editor
                                            .document()
                                            .path()
                                            .ok_or_else(|| io::Error::other("Open a file from disk first"))?
                                            .to_path_buf();
                                        let file = kwil_core_workspace::open_for_editing(&path)?;
                                        let prepared =
                                            state.encoding.open(path, file, encoding, &cancel, |_| Ok(()))?;
                                        finish_journal(state, document, journal)?;
                                        reseed = true;
                                        editor.install_raw_reopen(prepared);
                                    }
                                    FileAction::Encoding(encoding) => {
                                        editor.raw_reopen_ready()?;
                                        finish_journal(state, document, journal)?;
                                        reseed = true;
                                        editor.select_save_encoding(encoding);
                                    }
                                    FileAction::Keep => {
                                        editor.raw_reopen_ready()?;
                                        let changed = editor.document().original.as_ref().is_some_and(|original| {
                                            original.check().ok() != Some(kwil_core_text::FileState::Unchanged)
                                        });
                                        let path = editor
                                            .document()
                                            .path()
                                            .ok_or_else(|| io::Error::other("Open a file from disk first"))?;
                                        let seen = kwil_core_workspace::open_for_editing(path)?;
                                        finish_journal(state, document, journal)?;
                                        reseed = true;
                                        editor.reset_file_policy_observer();
                                        editor.document.original = Some(seen);
                                        if changed {
                                            editor.document.mark_unsaved();
                                        }
                                    }
                                    FileAction::LineEnding(ending) => {
                                        editor.raw_reopen_ready()?;
                                        finish_journal(state, document, journal)?;
                                        reseed = true;
                                        editor.reset_file_policy_observer();
                                        let converted = editor.convert_line_endings(ending);
                                        editor.reset_file_policy_observer();
                                        converted?;
                                    }
                                }
                                Ok(crate::Damage { full: true, status: true, ..Default::default() })
                            })();
                            if reseed {
                                storage
                                    .as_mut()
                                    .unwrap()
                                    .transports
                                    .insert(document, TransportSeed::document(editor.document()));
                            }
                            complete(DocumentResult::Replaced { key, operation, editor, result });
                        }
                        DocumentRequest::Replace { document, operation, mut editor, recipe } => {
                            let key = recipe.key;
                            let result = if document.0 != key.document_id {
                                Err(io::Error::new(
                                    io::ErrorKind::InvalidInput,
                                    "Replacement document identity mismatch",
                                ))
                            } else {
                                editor.apply_search_replacement(recipe)
                            };
                            complete(DocumentResult::Replaced { key, operation, editor, result });
                        }
                        DocumentRequest::Format { key, operation, mut editor, selection, language } => {
                            let result = editor.apply_formatting(selection, language);
                            complete(DocumentResult::Replaced { key, operation, editor, result });
                        }
                        DocumentRequest::Prepare { root } => {
                            let prepared = prepare(root).map(|(prepared, state)| {
                                storage = Some(state);
                                prepared
                            });
                            complete(DocumentResult::Prepared(prepared));
                        }
                        DocumentRequest::Persist(session) => {
                            if let Some(storage) = &mut storage {
                                storage.order.retain(|id, _| {
                                    session.tabs().iter().chain(session.closed()).any(|tab| tab.id == *id)
                                });
                            }
                            let result = storage
                                .as_mut()
                                .ok_or_else(|| io::Error::other("Session recovery is unavailable"))
                                .and_then(|s| persist_session(s, &session));
                            complete(DocumentResult::Persisted(result));
                        }
                        DocumentRequest::SessionTimer => {
                            if session_due.is_none() {
                                let now = std::time::Instant::now();
                                // A pending marker drains accepted checkpoint work;
                                // younger requests must not restart its debounce.
                                session_due =
                                    Some(if source_barrier.is_some() { now } else { now + Duration::from_secs(2) });
                            }
                        }
                        DocumentRequest::Recover { tab } => {
                            let result = (|| {
                                let storage =
                                    storage.as_mut().ok_or_else(|| io::Error::other("Recovery unavailable"))?;
                                let path = storage.root.join(format!("{}.kwj", tab.id.0));
                                if path.try_exists()? {
                                    let (document, recovered, _, origins) =
                                        recover_prepared(&storage.root, &mut storage.encoding, path, &tab)?;
                                    let handle = resume(&storage.service, recovered)?;
                                    let revision = document.revision();
                                    let mut journal =
                                        JournalState::new(handle, revision, 0, document_policy(&document));
                                    journal.origins = origins;
                                    storage.journals.insert(tab.id, journal);
                                    Ok(document)
                                } else if let Some(path) = &tab.path {
                                    kwil_core_workspace::open_for_editing(path).and_then(|file| {
                                        storage.encoding.open(path.clone(), file, None, &cancel, |_| Ok(()))
                                    })
                                } else if tab.revision == tab.saved_revision && tab.recovery_path.is_none() {
                                    Ok(Document::new())
                                } else {
                                    Err(io::Error::other("Closed document has no complete recovery journal"))
                                }
                            })();
                            complete(DocumentResult::Recovered {
                                document: tab.id,
                                revision: tab.revision,
                                result: result.map(Box::new),
                            });
                        }
                        DocumentRequest::Open { document, revision, path } => {
                            match kwil_core_workspace::open_for_editing(&path) {
                                Ok(file) => {
                                    let result = match storage.as_mut() {
                                        Some(storage) => storage.encoding.open(
                                            path.clone(),
                                            file.clone(),
                                            None,
                                            &cancel,
                                            |preview| {
                                                complete(DocumentResult::Preview {
                                                    document,
                                                    revision,
                                                    path,
                                                    file,
                                                    preview,
                                                });
                                                Ok(())
                                            },
                                        ),
                                        None => Err(io::Error::other("Workspace storage is not prepared")),
                                    };
                                    complete(DocumentResult::Indexed {
                                        document,
                                        revision,
                                        result: result.map(Box::new),
                                    });
                                }
                                Err(error) => {
                                    complete(DocumentResult::Opened { document, revision, result: Err(error) })
                                }
                            }
                        }
                        DocumentRequest::Index { document, revision, path, file } => {
                            let result = match storage.as_mut() {
                                Some(storage) => {
                                    storage.encoding.open(path.clone(), file.clone(), None, &cancel, |preview| {
                                        complete(DocumentResult::Preview { document, revision, path, file, preview });
                                        Ok(())
                                    })
                                }
                                None => Err(io::Error::other("Workspace storage is not prepared")),
                            };
                            complete(DocumentResult::Indexed { document, revision, result: result.map(Box::new) });
                        }
                        DocumentRequest::Save { document, request, plan } => {
                            let revision = request.revision;
                            let mut reserved =
                                destinations.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
                            if let Some((owner, _, path)) = &reopening {
                                reserved.push((*owner, path.clone()));
                            }
                            let result = validate_save_destination(document, &request.path, &reserved).and_then(|()| {
                                #[cfg(test)]
                                if let Some(ScheduleTestControl::Save(control)) = &mut schedule_control {
                                    control.before_effect(document, revision)?;
                                }
                                match storage.as_mut() {
                                    Some(storage) => storage.encoding.save(request, plan, &cancel),
                                    None => Err(io::Error::other("Workspace storage is not prepared")),
                                }
                            });
                            #[cfg(test)]
                            if let Some(ScheduleTestControl::Save(control)) = &schedule_control {
                                control.before_publish(document, revision, &result);
                            }
                            complete(DocumentResult::Saved { document, revision, result });
                        }
                        DocumentRequest::ValidateReopen { document, generation, path, original, cached } => {
                            let reserved =
                                destinations.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
                            let result = (|| {
                                if reopening.is_some() {
                                    return Err(io::Error::other("Closed-tab admission already in progress"));
                                }
                                validate_save_destination(document, &path, &reserved)?;
                                if let Some(original) = &original
                                    && original.check()? != kwil_core_text::FileState::Unchanged
                                {
                                    return Err(io::Error::other(
                                        "Closed file changed on disk; closed contents retained",
                                    ));
                                }
                                // A recovered cached editor without an original
                                // cannot prove its named backing. Preserve its
                                // bytes but require Save As when attaching it.
                                let keep_path = !cached || original.is_some();
                                if keep_path {
                                    reopening = Some((document, generation, path));
                                }
                                Ok(keep_path)
                            })();
                            complete(DocumentResult::ReopenValidated { document, generation, result });
                        }
                        DocumentRequest::ReleaseReopen { document, generation } => {
                            if reopening
                                .as_ref()
                                .is_some_and(|(owner, token, _)| *owner == document && *token == generation)
                            {
                                reopening = None;
                            }
                        }
                        DocumentRequest::SeedJournal {
                            document,
                            revision,
                            snapshot,
                            path,
                            next_sequence,
                            generation,
                            policy,
                        } => {
                            let result = storage
                                .as_mut()
                                .ok_or_else(|| io::Error::other("Recovery unavailable"))
                                .and_then(|storage| {
                                    seed_journal(
                                        storage,
                                        document,
                                        SeedSnapshot { revision, snapshot },
                                        path,
                                        next_sequence.saturating_sub(1),
                                        generation,
                                        policy,
                                    )
                                });
                            if let Err(error) = result {
                                complete(DocumentResult::JournalDurable {
                                    document,
                                    revision,
                                    sequence: next_sequence.saturating_sub(1),
                                    result: Err(error),
                                });
                            }
                            #[cfg(test)]
                            if let Some(control) = &mut schedule_control {
                                control.hold_ready(document, generation, storage.as_mut());
                            }
                        }
                        DocumentRequest::Journal { document, receipt, path, first_sequence, generation, policy } => {
                            if let Some(state) = storage.as_mut() {
                                let seeded = seed_journal(
                                    state,
                                    document,
                                    SeedSnapshot {
                                        revision: receipt.before_revision,
                                        snapshot: receipt.before.clone(),
                                    },
                                    path.clone(),
                                    first_sequence.saturating_sub(1),
                                    generation,
                                    policy,
                                );
                                if seeded.is_ok()
                                    && state
                                        .journals
                                        .get(&document)
                                        .is_some_and(|journal| !journal.ready && !journal.failed)
                                {
                                    // A first receipt may create its own seed.
                                    // Keep it ahead of already deferred successors.
                                    let request = if let Some(charge) = sealed_charge.take() {
                                        DocumentRequest::SealedJournal(result_transport::SealedJournalRequest {
                                            document,
                                            receipt,
                                            path,
                                            first_sequence,
                                            generation,
                                            policy,
                                            charge,
                                        })
                                    } else {
                                        DocumentRequest::Journal {
                                            document,
                                            receipt,
                                            path,
                                            first_sequence,
                                            generation,
                                            policy,
                                        }
                                    };
                                    deferred.push_front(request);
                                    if preceding_journal.is_some()
                                        && let Some(barrier) = &mut source_barrier
                                    {
                                        barrier.record(document, Some(state));
                                    }
                                    // Poll readiness once before the next event-driven wait.
                                    poll_journals(state, &complete);
                                    continue;
                                }
                                if let Err(error) = seeded {
                                    capacity.fetch_add(1, std::sync::atomic::Ordering::Release);
                                    complete(DocumentResult::JournalQueued {
                                        document,
                                        revision: receipt.after_revision,
                                        sequence: receipt.sequence,
                                        result: Err(error),
                                    });
                                    continue;
                                }
                            }
                            let (revision, sequence) = (receipt.after_revision, receipt.sequence);
                            let result = storage
                                .as_mut()
                                .ok_or_else(|| io::Error::other("Recovery unavailable"))
                                .and_then(|storage| {
                                    journal_receipt(
                                        storage,
                                        document,
                                        receipt,
                                        path,
                                        first_sequence,
                                        generation,
                                        policy,
                                    )
                                });
                            if result.is_ok()
                                && let Some(barrier) = &source_barrier
                            {
                                barrier.expedite_pending_flush(document, storage.as_mut());
                            }
                            capacity.fetch_add(1, std::sync::atomic::Ordering::Release);
                            complete(DocumentResult::JournalQueued { document, revision, sequence, result });
                        }
                        DocumentRequest::FinishJournal { document, generation } => {
                            let result = storage
                                .as_mut()
                                .map_or(Ok(None), |storage| finish_journal(storage, document, generation));
                            let result = result.map(|durable| {
                                if let Some((revision, sequence)) = durable {
                                    // Finish acknowledges the final sync after all earlier
                                    // journal commands. Publish its exact boundary before
                                    // the close result; this journal cannot be flushed again.
                                    complete(DocumentResult::JournalDurable {
                                        document,
                                        revision,
                                        sequence,
                                        result: Ok(()),
                                    });
                                }
                            });
                            complete(DocumentResult::JournalClosed { document, result });
                        }
                        DocumentRequest::FlushJournals => {
                            if let Some(storage) = &mut storage {
                                for journal in storage.journals.values_mut() {
                                    journal.needs_flush = true;
                                    journal.flush_due = std::time::Instant::now();
                                }
                            }
                        }
                    }
                    if let Some(document) = preceding_journal
                        && let Some(barrier) = &mut source_barrier
                    {
                        barrier.record(document, storage.as_mut());
                    }
                }
                if let Some(storage) = &mut storage {
                    poll_journals(storage, &complete);
                    // Explicit event-driven pass after worker-held receipts and
                    // plans unwind; no cleanup timer or UI-thread unlink.
                    let retirement = storage.encoding.retire();
                    if let Some(error) = retirement.first_error {
                        eprintln!("Decoded backing cleanup retained for retry: {error}");
                    }
                }
                if session_due.is_some_and(|due| std::time::Instant::now() >= due) {
                    session_due = None;
                    complete(DocumentResult::SessionCheckpointDue);
                }
            }
            // Every admitted receipt gets a terminal failure if a seed/slot
            // cannot drain. A failed shutdown is never reported as a clean stop.
            for request in deferred {
                controls.release(&request);
                let (request, _sealed_charge) = unpack_sealed(request);
                match request {
                    DocumentRequest::ResumeSaveCompletion { key } => {
                        complete(DocumentResult::ResumeSaveCompletion { key });
                    }
                    DocumentRequest::SourceBarrier(marker) => complete(DocumentResult::SourceBarrier(
                        result_transport::DocumentBarrierReceipt::failed(marker),
                    )),
                    DocumentRequest::DiscardHex { document, editor } => hex.discard(document, editor),
                    DocumentRequest::Journal { document, receipt, .. } => {
                        capacity.fetch_add(1, std::sync::atomic::Ordering::Release);
                        complete(DocumentResult::JournalQueued {
                            document,
                            revision: receipt.after_revision,
                            sequence: receipt.sequence,
                            result: Err(io::Error::other("Recovery receipt could not drain before shutdown")),
                        });
                    }
                    DocumentRequest::SeedJournal { document, revision, next_sequence, .. } => {
                        complete(DocumentResult::JournalDurable {
                            document,
                            revision,
                            sequence: next_sequence.saturating_sub(1),
                            result: Err(io::Error::other("Recovery seed could not drain before shutdown")),
                        });
                    }
                    DocumentRequest::FinishJournal { document, .. } => {
                        complete(DocumentResult::JournalClosed {
                            document,
                            result: Err(io::Error::other("Recovery close did not drain")),
                        });
                    }
                    _ => unreachable!("only recovery requests are deferred"),
                }
            }
            // Last unopened offer owners retire here, never with UI teardown.
            drop(binary);
            let stopped = if let Some(mut storage) = storage {
                if shutdown_error.is_none() && storage.journals.values().any(|journal| journal.failed) {
                    shutdown_error =
                        Some(io::Error::other("Recovery failed before document shutdown; changes remain unsaved"));
                }
                storage.service.finish();
                // Shutdown is on this worker. UI never waits for seeding/fsync.
                let result = storage
                    .service
                    .finished
                    .recv_timeout(Duration::from_secs(30))
                    .map_err(|_| io::Error::other("Journal shutdown timed out; pending seeds are not durable"))
                    .and_then(|result| result);
                storage.journals.clear();
                storage.finished_journals.clear();
                let retirement = storage.encoding.retire();
                if retirement.live != 0 || retirement.cleanup_pending != 0 || retirement.first_error.is_some() {
                    let error = PendingBackingRetirement {
                        root: storage.root.clone(),
                        live_owners: retirement.live,
                        cleanup_pending: retirement.cleanup_pending,
                        cleanup_error: retirement.first_error,
                        previous_error: shutdown_error.take(),
                        journal_error: result.err(),
                    };
                    eprintln!("{error}");
                    // No indefinite shutdown wait, new thread, global registry
                    // or last-snapshot Drop cleanup. This finite !Send registry
                    // retains every exact resource within its admission cap.
                    std::mem::forget(storage.encoding);
                    Err(io::Error::other(error))
                } else {
                    result
                }
            } else {
                Ok(())
            };
            complete(DocumentResult::Stopped(shutdown_error.map_or(stopped, Err)));
        })?;
        if let Some(transport) = transport {
            transport.install_worker(worker);
        }
        Ok(Self { tx, journal_capacity, journal_controls, save_destinations })
    }

    /// Full admission returns ownership. Callers must retain/retry or explicitly
    /// reject it; a rejected save must clear the model's in-flight save state.
    pub fn try_submit(&self, request: DocumentRequest) -> Result<(), TrySendError<Box<DocumentRequest>>> {
        let receipt_credit = matches!(&self.tx, result_transport::RequestSender::Bounded(_))
            && matches!(&request, DocumentRequest::Journal { .. });
        if receipt_credit
            && self
                .journal_capacity
                .try_update(std::sync::atomic::Ordering::AcqRel, std::sync::atomic::Ordering::Acquire, |left| {
                    left.checked_sub(1)
                })
                .is_err()
        {
            return Err(TrySendError::Full(Box::new(request)));
        }
        let control = self.journal_controls.for_request(&request);
        if control.is_some_and(|credit| {
            credit
                .try_update(std::sync::atomic::Ordering::AcqRel, std::sync::atomic::Ordering::Acquire, |left| {
                    left.checked_sub(1)
                })
                .is_err()
        }) {
            if receipt_credit {
                self.journal_capacity.fetch_add(1, std::sync::atomic::Ordering::Release);
            }
            return Err(TrySendError::Full(Box::new(request)));
        }
        let gate = self.tx.lock_sender();
        let delivered = gate.try_send(request);
        drop(gate);
        let error = match delivered {
            std::ops::ControlFlow::Continue(()) => return Ok(()),
            std::ops::ControlFlow::Break(error) => error,
        };
        Err(match error {
            TrySendError::Full(request) => {
                if receipt_credit {
                    self.journal_capacity.fetch_add(1, std::sync::atomic::Ordering::Release);
                }
                if let Some(credit) = control {
                    credit.fetch_add(1, std::sync::atomic::Ordering::Release);
                }
                TrySendError::Full(Box::new(request))
            }
            TrySendError::Disconnected(request) => {
                if receipt_credit {
                    self.journal_capacity.fetch_add(1, std::sync::atomic::Ordering::Release);
                }
                if let Some(credit) = control {
                    credit.fetch_add(1, std::sync::atomic::Ordering::Release);
                }
                TrySendError::Disconnected(Box::new(request))
            }
        })
    }

    #[cfg(test)]
    pub(crate) fn replacement_test_channel(capacity: usize) -> (Self, mpsc::Receiver<DocumentRequest>) {
        let (tx, rx) = mpsc::sync_channel(capacity);
        (
            Self {
                tx: result_transport::RequestSender::new(tx, None),
                journal_capacity: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(16)),
                journal_controls: std::sync::Arc::new(JournalControlCredits::new()),
                save_destinations: Default::default(),
            },
            rx,
        )
    }
}

impl Drop for DocumentWorker {
    fn drop(&mut self) {
        // Bounded observers share the sole sender gate. Existing accepted work
        // still drains; a retained result is never destroyed by this UI drop.
        self.tx.close_bounded();
    }
}

fn validate_save_destination(
    document: DocumentId,
    path: &std::path::Path,
    reserved: &[(DocumentId, PathBuf)],
) -> io::Result<()> {
    // All filesystem resolution runs on the one document worker, freshly for
    // each save. A formerly missing destination may have been created by the
    // preceding save, or another native spelling may now identify that file.
    let destination = kwil_core_workspace::resolve_save_destination(path)?;
    for (owner, path) in reserved {
        if *owner == document {
            continue;
        }
        let other = kwil_core_workspace::resolve_save_destination(path)?;
        if destination.equivalent(&other) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("Save target is open or reserved by tab {}; use a different Save As path", owner.0),
            ));
        }
    }
    Ok(())
}

fn prepare(root: Option<PathBuf>) -> io::Result<(PreparedWorkspace, Storage)> {
    let root = resolve_root(root, std::env::var_os("KWIL_WORKSPACE_ROOT"))?;
    std::fs::create_dir_all(&root)?;
    prepare_root(root)
}

fn resolve_root(root: Option<PathBuf>, override_root: Option<std::ffi::OsString>) -> io::Result<PathBuf> {
    Ok(match root.or_else(|| override_root.map(PathBuf::from)) {
        Some(root) if root.is_absolute() => root,
        Some(_) => return Err(io::Error::other("Recovery directory must be absolute")),
        None => {
            let exe = std::env::current_exe()?;
            kwil_core_commands::settings::Settings::path(&exe)
                .map_err(io::Error::other)?
                .parent()
                .ok_or_else(|| io::Error::other("No recovery directory"))?
                .join("workspace")
        }
    })
}

fn prepare_root(root: PathBuf) -> io::Result<(PreparedWorkspace, Storage)> {
    let ownership = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("workspace.lock"))?;
    ownership.try_lock().map_err(io::Error::other)?;
    // Reserve owner/path bookkeeping before opening any decoded document.
    // This is a metadata admission ceiling, not a resident allocation or a
    // file-size limit. Registry slots retain owners until snapshots retire.
    let mut encoding = encoding_runtime::EncodingRuntime::new(
        root.clone(),
        retirement::BackingLimits {
            max_backings: session::MAX_TABS + session::MAX_CLOSED,
            max_private_path_bytes: session::MAX_PATH_BYTES,
            max_metadata_bytes: 256 * 1024 * 1024,
        },
    )?;
    let cancel = AtomicBool::new(false);
    let mut session = session::load_session(&root.join("session.kws"))?;
    let service = JournalService::spawn(Duration::from_secs(2))?;
    let mut documents = Vec::new();
    let mut orphans = HashMap::new();
    let mut journals = HashMap::new();
    let mut paths = HashMap::new();
    for entry in std::fs::read_dir(&root)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "kwj") {
            let id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse::<u64>().ok())
                .filter(|n| *n > 0)
                .ok_or_else(|| io::Error::other("Invalid recovery document identity"))?;
            // Only orphan scanning honors retirement. Explicit metadata wins
            // after a crash before replacement, or an intentional reopen.
            if !session.tabs().iter().chain(session.closed()).any(|tab| tab.id == DocumentId(id))
                && is_retired(&root, DocumentId(id))?
            {
                continue;
            }
            if paths.len() >= session::MAX_TABS + session::MAX_CLOSED {
                return Err(io::Error::other("Recovery journal limit"));
            }
            paths.insert(DocumentId(id), path);
        }
    }
    // Orphans arise if a journal fsync beats the metadata write. Recover their
    // persisted identity; never allocate a colliding new ID or delete them.
    for (id, path) in &paths {
        if session.tabs().iter().chain(session.closed()).any(|t| t.id == *id) {
            continue;
        }
        let metadata = Tab {
            id: *id,
            path: None,
            recovery_path: None,
            revision: 0,
            saved_revision: 0,
            cursor: 0,
            anchor: 0,
            first_visible_line: 0,
            first_byte_column: 0,
        };
        let recovered = recover_prepared(&root, &mut encoding, path.clone(), &metadata).and_then(
            |(mut document, recovered, base, origins)| {
                mark_recovered_unsaved(&mut document)?;
                Ok((document, recovered, base, origins))
            },
        );
        let (document, recovered, base, origins) = match recovered {
            Ok(recovered) => recovered,
            Err(error) => {
                // Keep its identity reserved and show an addressed failure.
                // A bad orphan must not discard the initialized I/O lane or
                // prevent unrelated documents from opening and saving.
                session.restore_tab(Tab { saved_revision: 1, ..metadata })?;
                orphans.insert(*id, Err(error));
                continue;
            }
        };
        // Orphan metadata never proves a saved state, including revision zero.
        // Reserve an unreachable saved marker on the Document itself: sync
        // must not turn the Session dirty flag back into a clean document.
        let recovery_path = base.original_path;
        // Keep the old name as recovery context, never as an unverified
        // writable backing on the recovered orphan document.
        session.restore_tab(Tab {
            id: *id,
            path: None,
            recovery_path,
            revision: document.revision(),
            saved_revision: document.saved_revision(),
            cursor: 0,
            anchor: 0,
            first_visible_line: 0,
            first_byte_column: 0,
        })?;
        orphans.insert(*id, Ok((document, recovered, origins)));
    }
    for tab in session.tabs() {
        let result = if paths.contains_key(&tab.id) {
            let recovered = match orphans.remove(&tab.id) {
                Some(recovered) => recovered,
                None => recover_prepared(&root, &mut encoding, paths[&tab.id].clone(), tab)
                    .map(|(d, r, _, origins)| (d, r, origins)),
            };
            recovered.and_then(|(document, recovered, origins)| {
                let handle = resume(&service, recovered)?;
                let revision = document.revision();
                let mut journal = JournalState::new(handle, revision, 0, document_policy(&document));
                journal.origins = origins;
                journals.insert(tab.id, journal);
                Ok(document)
            })
        } else if let Some(path) = &tab.path {
            kwil_core_workspace::open_for_editing(path)
                .and_then(|file| encoding.open(path.clone(), file, None, &cancel, |_| Ok(())))
        } else if tab.revision != tab.saved_revision || tab.recovery_path.is_some() {
            Err(io::Error::other("Unsaved document has no complete recovery journal"))
        } else {
            Ok(Document::new())
        };
        documents.push((tab.id, result));
    }
    for (id, document) in &documents {
        if document.as_ref().is_ok_and(|document| document.path().is_none())
            && let Some(tab) = session.tab_mut(*id)
            && let Some(path) = tab.path.take()
        {
            tab.recovery_path = Some(path);
        }
    }
    let durable_session = session::load_session(&root.join("session.kws"))?;
    Ok((
        PreparedWorkspace { session, documents },
        Storage {
            root,
            _ownership: ownership,
            service,
            journals,
            finished_journals: HashMap::new(),
            transports: HashMap::new(),
            order: HashMap::new(),
            durable_session,
            encoding,
        },
    ))
}

/// Retire only identities proven evicted from prior durable closed history.
/// These small markers retain all journal data; cleanup policy is separate.
fn retire_evicted(storage: &Storage, next: &Session) -> io::Result<()> {
    use std::io::Write;
    for tab in storage.durable_session.closed() {
        if next.tabs().iter().chain(next.closed()).any(|current| current.id == tab.id) {
            continue;
        }
        let mut marker = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(storage.root.join(format!("{}.retired", tab.id.0)))?;
        marker.write_all(b"kwil-retired-v1\n")?;
        marker.sync_all()?;
    }
    // Directory synchronization is available on Unix. Windows marker sync_all
    // precedes the atomic write-through session replacement below.
    #[cfg(unix)]
    std::fs::File::open(&storage.root)?.sync_all()?;
    Ok(())
}

fn is_retired(root: &std::path::Path, id: DocumentId) -> io::Result<bool> {
    use std::io::Read;
    let path = root.join(format!("{}.retired", id.0));
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::with_capacity(32);
    file.take(32).read_to_end(&mut bytes)?;
    if bytes != b"kwil-retired-v1\n" {
        return Err(io::Error::other("Incomplete recovery retirement marker; journal retained"));
    }
    Ok(true)
}

fn persist_session(storage: &mut Storage, next: &Session) -> io::Result<()> {
    retire_evicted(storage, next)?;
    session::store_session(&storage.root.join("session.kws"), next)?;
    storage.durable_session = next.clone();
    storage.finished_journals.retain(|id, _| next.tabs().iter().chain(next.closed()).any(|tab| tab.id == *id));
    storage.transports.retain(|id, _| next.tabs().iter().chain(next.closed()).any(|tab| tab.id == *id));
    Ok(())
}

fn finish_journal(storage: &mut Storage, document: DocumentId, generation: u64) -> io::Result<Option<(u64, u64)>> {
    if !storage.journals.get(&document).is_some_and(|journal| journal.generation <= generation) {
        return Ok(None);
    }
    if storage.transports.contains_key(&document) {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "transport seed is not durable; journal and editor retained",
        ));
    }
    if storage.finished_journals.contains_key(&document)
        || storage.finished_journals.len() >= session::MAX_TABS + session::MAX_CLOSED
    {
        return Err(io::Error::new(io::ErrorKind::WouldBlock, "previous journal owner capacity retained"));
    }
    storage.finished_journals.try_reserve(1).map_err(io::Error::other)?;
    let journal = storage.journals.get_mut(&document).expect("journal to finish");
    let result = submit(&journal.handle, kwil_core_workspace::journal::JournalCommand::Finish).and_then(|_| {
        journal
            .handle
            .finished
            .recv_timeout(Duration::from_secs(30))
            .map_err(|_| io::Error::other("Journal close timed out; exact previous owners retained"))?
            .and_then(|revision| {
                if revision == journal.revision {
                    Ok(())
                } else {
                    Err(io::Error::other("Journal finish revision mismatch; exact owner retained"))
                }
            })
    });
    if let Err(error) = result {
        journal.failed = true;
        return Err(error);
    }
    let journal = storage.journals.remove(&document).expect("finished journal still owned");
    let durable = (journal.revision, journal.sequence);
    storage.finished_journals.insert(document, journal);
    Ok(Some(durable))
}

struct SeedSnapshot {
    revision: u64,
    snapshot: kwil_core_text::Snapshot,
}

fn seed_journal(
    storage: &mut Storage,
    document: DocumentId,
    source: SeedSnapshot,
    path: Option<PathBuf>,
    sequence: u64,
    generation: u64,
    policy: RecoveryPolicy,
) -> io::Result<()> {
    let SeedSnapshot { revision, snapshot } = source;
    if let Some(journal) = storage.journals.get_mut(&document) {
        if journal.policy.raw_bytes != policy.raw_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "journal transport policy changed; prior journal retained",
            ));
        }
        journal.generation = journal.generation.max(generation);
        return Ok(());
    }
    let transport = storage.transports.get(&document).copied();
    if let Some(expected) = transport
        && (expected.policy != policy
            || expected.revision != revision
            || expected.len != snapshot.len()
            || sequence != 0)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "transport seed identity or initial sequence mismatch; prior recovery retained",
        ));
    }
    let sequence =
        if transport.is_some() { 0 } else { (*storage.order.entry(document).or_insert(sequence)).max(sequence) };
    let canonical = storage.root.join(format!("{}.kwj", document.0));
    let promotion = transport.map(|_| {
        let mut path = canonical.as_os_str().to_os_string();
        path.push(".next");
        PathBuf::from(path)
    });
    if let Some(staged) = &promotion {
        let mut pending = staged.as_os_str().to_os_string();
        pending.push(".pending");
        if staged.try_exists()? || PathBuf::from(pending).try_exists()? {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "unselected transport .next recovery retained; reconcile before another transition",
            ));
        }
    }
    let journal_path = promotion.as_ref().unwrap_or(&canonical).clone();
    let mut origins = Some(provenance::OriginRegistry::new(session::MAX_TABS + session::MAX_CLOSED, 8 * 1024 * 1024));
    let mut seed_origins = None;
    let handle = if promotion.is_none() && journal_path.try_exists()? {
        let metadata = storage
            .durable_session
            .tabs()
            .iter()
            .chain(storage.durable_session.closed())
            .find(|tab| tab.id == document)
            .cloned()
            .unwrap_or(Tab {
                id: document,
                path: path.clone(),
                recovery_path: None,
                revision,
                saved_revision: revision,
                cursor: 0,
                anchor: 0,
                first_visible_line: 0,
                first_byte_column: 0,
            });
        let (replayed, recovered, _, restored_origins) =
            recover_prepared(&storage.root, &mut storage.encoding, journal_path, &metadata)?;
        if document_policy(&replayed).raw_bytes != policy.raw_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "reopened journal transport policy disagrees; recovery retained",
            ));
        }
        if recovered.summary().revision != revision || recovered.summary().byte_len != snapshot.len() {
            return Err(io::Error::other("Journal revision disagrees with reopened document; recovery retained"));
        }
        if !snapshots_equal(&replayed.snapshot(), &snapshot)? {
            return Err(io::Error::other("Journal bytes disagree with reopened document; recovery retained"));
        }
        origins = restored_origins;
        drop(replayed);
        resume(&storage.service, recovered)?
    } else {
        let logical_len = snapshot.len();
        let directory = origins.as_ref().expect("new origin directory");
        let plan = directory.plan(snapshot, 0..logical_len, std::sync::Arc::new(AtomicBool::new(false)))?;
        let physical_len = plan.physical_len();
        let (source, admission) = encoding_seed::SeedSource::new(directory.start(plan)?);
        seed_origins = Some(admission);
        let mut seed = kwil_core_workspace::journal::EncodedJournalSeed {
            path: journal_path,
            base: JournalBase {
                document,
                revision,
                original_path: path,
                original_identity: [0; 4],
                codec: policy.to_codec()?,
                byte_len: physical_len,
            }
            .with_logical_len(logical_len)?,
            source: Box::new(source),
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match storage.service.try_create_encoded(seed) {
                Ok(handle) => break handle,
                Err(TrySendError::Full(value)) if std::time::Instant::now() < deadline => {
                    seed = *value;
                    std::thread::park_timeout(Duration::from_millis(2));
                }
                Err(_) => return Err(io::Error::other("Journal seed admission failed; edits remain unsaved")),
            }
        }
    };
    let mut state = JournalState::new(handle, revision, sequence, policy);
    state.origins = origins;
    state.seed_origins = seed_origins;
    state.generation = generation;
    state.promotion = promotion;
    storage.journals.insert(document, state);
    Ok(())
}

fn submit(handle: &JournalSession, mut command: kwil_core_workspace::journal::JournalCommand) -> io::Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match handle.try_submit(command) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Full(value)) if std::time::Instant::now() < deadline => {
                command = value;
                std::thread::park_timeout(Duration::from_millis(2));
            }
            Err(_) => return Err(io::Error::other("Journal queue unavailable; current changes are not durable")),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum JournalTransportPhase {
    Count,
    Counted,
    Visit,
    Plan,
    Copy,
    Admit,
    EndReplacement,
    Commit,
    Finished,
    Failed,
}

/// One worker-owned transport block. Capsule records and source pieces can be
/// tiny; their boundaries must not become individual queue/disk operations.
/// Drop deliberately does NOT flush: only an explicit successful boundary can
/// authorize capsule admission or a journal replacement/batch terminator.
struct JournalBytes<F> {
    send: F,
    pending: Vec<u8>,
    failed: bool,
    #[cfg(any(test, debug_assertions))]
    diagnostics: JournalTransportDiagnostics,
}

#[cfg(any(test, debug_assertions))]
#[derive(Debug)]
struct JournalTransportDiagnostics {
    started: std::time::Instant,
    phases: u16,
    writes: u64,
    input_bytes: u64,
    commands: u64,
    queued_bytes: u64,
    max_buffer: usize,
}

impl<F: FnMut(Vec<u8>) -> io::Result<()>> JournalBytes<F> {
    fn new(send: F) -> Self {
        Self {
            send,
            pending: Vec::with_capacity(kwil_core_workspace::journal::JOURNAL_CHUNK),
            failed: false,
            #[cfg(any(test, debug_assertions))]
            diagnostics: JournalTransportDiagnostics {
                started: std::time::Instant::now(),
                phases: 0,
                writes: 0,
                input_bytes: 0,
                commands: 0,
                queued_bytes: 0,
                max_buffer: 0,
            },
        }
    }

    fn check(&self) -> io::Result<()> {
        if self.failed { Err(io::Error::other("Journal transport previously failed")) } else { Ok(()) }
    }

    fn flush_pending(&mut self) -> io::Result<()> {
        self.check()?;
        if self.pending.is_empty() {
            return Ok(());
        }
        let bytes =
            std::mem::replace(&mut self.pending, Vec::with_capacity(kwil_core_workspace::journal::JOURNAL_CHUNK));
        #[cfg(any(test, debug_assertions))]
        let len = bytes.len() as u64;
        if let Err(error) = (self.send)(bytes) {
            self.failed = true;
            return Err(error);
        }
        #[cfg(any(test, debug_assertions))]
        {
            self.diagnostics.commands = self.diagnostics.commands.saturating_add(1);
            self.diagnostics.queued_bytes = self.diagnostics.queued_bytes.saturating_add(len);
        }
        Ok(())
    }

    /// io::copy alone does not flush a short final block. Reader failure also
    /// poisons the transport, so trailing partial bytes can never be retried as
    /// a complete capsule. The caller may admit only after this returns Ok.
    fn copy_from(&mut self, reader: &mut impl io::Read) -> io::Result<u64> {
        self.check()?;
        let result = io::copy(reader, self).and_then(|copied| self.flush_pending().map(|()| copied));
        if result.is_err() {
            self.failed = true;
            self.pending.clear();
        }
        result
    }

    // At most ten phase records per receipt, independent of edit/span count.
    // Debug production logging is opt-in; only its boolean switch is cached.
    // No release logging or per-piece diagnostics.
    fn diagnose(&mut self, _document: DocumentId, _sequence: u64, _phase: JournalTransportPhase) {
        #[cfg(all(debug_assertions, not(test)))]
        {
            static TRACE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            if !TRACE.get_or_init(|| std::env::var("KWIL_TRACE_JOURNAL_TRANSPORT").is_ok_and(|value| value == "1")) {
                return;
            }
        }
        #[cfg(any(test, debug_assertions))]
        {
            let bit = 1_u16 << (_phase as u8);
            if self.diagnostics.phases & bit == 0 {
                self.diagnostics.phases |= bit;
                eprintln!(
                    "journal transport diagnostics: document={} sequence={} phase={_phase:?} elapsed={:?} writes={} input_bytes={} commands={} queued_bytes={} max_buffer={}",
                    _document.0,
                    _sequence,
                    self.diagnostics.started.elapsed(),
                    self.diagnostics.writes,
                    self.diagnostics.input_bytes,
                    self.diagnostics.commands,
                    self.diagnostics.queued_bytes,
                    self.diagnostics.max_buffer,
                );
            }
        }
    }
}

impl<F: FnMut(Vec<u8>) -> io::Result<()>> io::Write for JournalBytes<F> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.check()?;
        #[cfg(any(test, debug_assertions))]
        {
            self.diagnostics.writes = self.diagnostics.writes.saturating_add(1);
            self.diagnostics.input_bytes = self.diagnostics.input_bytes.saturating_add(bytes.len() as u64);
        }
        let mut remaining = bytes;
        while !remaining.is_empty() {
            let n = remaining.len().min(kwil_core_workspace::journal::JOURNAL_CHUNK - self.pending.len());
            self.pending.extend_from_slice(&remaining[..n]);
            remaining = &remaining[n..];
            #[cfg(any(test, debug_assertions))]
            {
                self.diagnostics.max_buffer = self.diagnostics.max_buffer.max(self.pending.len());
            }
            if self.pending.len() == kwil_core_workspace::journal::JOURNAL_CHUNK {
                self.flush_pending()?;
            }
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.flush_pending()
    }
}

#[cfg(test)]
mod journal_transport_tests {
    use super::*;
    use kwil_core_workspace::journal::JOURNAL_CHUNK;
    use std::io::{Read, Write};

    struct ShortReads<R>(R);
    impl<R: Read> Read for ShortReads<R> {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            let len = output.len().min(3);
            self.0.read(&mut output[..len])
        }
    }

    #[test]
    fn journal_transport_short_reads_coalesce_with_exact_final_flush() {
        let input: Vec<u8> = (0..2 * JOURNAL_CHUNK + 17).map(|n| n as u8).collect();
        let mut frames = Vec::new();
        let mut transport = JournalBytes::new(|bytes| {
            frames.push(bytes);
            Ok(())
        });
        assert_eq!(transport.copy_from(&mut ShortReads(io::Cursor::new(&input))).unwrap(), input.len() as u64);
        transport.flush().unwrap(); // Empty flush emits no extra command.
        assert_eq!(transport.diagnostics.commands, 3);
        assert_eq!(transport.diagnostics.max_buffer, JOURNAL_CHUNK);
        assert!(transport.diagnostics.writes > transport.diagnostics.commands);
        drop(transport);
        assert_eq!(frames.iter().map(Vec::len).collect::<Vec<_>>(), vec![JOURNAL_CHUNK, JOURNAL_CHUNK, 17]);
        assert_eq!(frames.concat(), input);
    }

    #[test]
    fn journal_transport_failed_trailing_flush_cannot_admit_or_retry() {
        let input = vec![0xa5; JOURNAL_CHUNK + 17];
        let (mut calls, mut accepted, mut admitted) = (0, Vec::new(), false);
        let mut transport = JournalBytes::new(|bytes| {
            calls += 1;
            if calls == 2 {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "refused trailing block"));
            }
            accepted.push(bytes);
            Ok(())
        });
        let result = transport.copy_from(&mut ShortReads(io::Cursor::new(&input))).map(|_| admitted = true);
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::BrokenPipe);
        assert!(!admitted);
        assert!(transport.flush().is_err());
        assert!(transport.write_all(b"must not follow failure").is_err());
        assert_eq!(transport.diagnostics.commands, 1);
        drop(transport);
        assert_eq!(calls, 2, "failed payload is never retried or acknowledged");
        assert_eq!(accepted, vec![input[..JOURNAL_CHUNK].to_vec()]);
    }

    #[test]
    fn journal_transport_reader_error_discards_unacknowledged_tail() {
        struct FailsAfterBytes(io::Cursor<Vec<u8>>);
        impl Read for FailsAfterBytes {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                if self.0.position() == self.0.get_ref().len() as u64 {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "capsule reader failed"));
                }
                self.0.read(output)
            }
        }
        let mut frames = Vec::new();
        let mut transport = JournalBytes::new(|bytes| {
            frames.push(bytes);
            Ok(())
        });
        let error = transport.copy_from(&mut FailsAfterBytes(io::Cursor::new(vec![7; JOURNAL_CHUNK + 17])));
        assert_eq!(error.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        assert!(transport.pending.is_empty());
        assert!(transport.flush().is_err());
        drop(transport);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0], vec![7; JOURNAL_CHUNK]);
    }

    #[test]
    fn journal_transport_explicit_boundaries_do_not_merge_replacements() {
        let mut frames = Vec::new();
        let mut transport = JournalBytes::new(|bytes| {
            frames.push(bytes);
            Ok(())
        });
        transport.write_all(b"a").unwrap();
        transport.write_all(b"bc").unwrap();
        assert_eq!(transport.diagnostics.commands, 0);
        transport.flush().unwrap();
        transport.write_all(b"next").unwrap();
        transport.flush().unwrap();
        drop(transport);
        assert_eq!(frames, vec![b"abc".to_vec(), b"next".to_vec()]);
    }
}

fn journal_receipt(
    storage: &mut Storage,
    document: DocumentId,
    receipt: crate::EditReceipt,
    path: Option<PathBuf>,
    first_sequence: u64,
    generation: u64,
    policy: RecoveryPolicy,
) -> io::Result<()> {
    use kwil_core_workspace::journal::{JournalBatch, JournalCommand as C, JournalReplacement};
    seed_journal(
        storage,
        document,
        SeedSnapshot { revision: receipt.before_revision, snapshot: receipt.before.clone() },
        path,
        first_sequence.saturating_sub(1),
        generation,
        policy,
    )?;
    let journal = storage.journals.get_mut(&document).unwrap();
    let handle = &journal.handle;
    let mut transport = JournalBytes::new(|bytes| submit(handle, C::Bytes(bytes)));
    let result = (|| {
        if journal.failed
            || journal.generation != generation
            || journal.sequence.checked_add(1) != Some(receipt.sequence)
            || journal.revision != receipt.before_revision
        {
            return Err(io::Error::other("Journal receipt gap, duplicate or mismatched revision; recovery retained"));
        }
        // Replay admits cumulative tree-construction work. Dense recipe edits
        // share a bounded span instead of forcing one tree rewrite/capsule per
        // character. The observer's original receipt/undo recipe stays intact.
        let mut replacements = 0_u64;
        transport.diagnose(document, receipt.sequence, JournalTransportPhase::Count);
        visit_journal_replacements(&receipt, &mut |_| {
            replacements =
                replacements.checked_add(1).ok_or_else(|| io::Error::other("Journal replacement count overflow"))?;
            Ok(())
        })?;
        transport.diagnose(document, receipt.sequence, JournalTransportPhase::Counted);
        #[cfg(test)]
        eprintln!(
            "journal receipt diagnostics: document={} sequence={} recipe_edits={} emitted_spans={replacements}",
            document.0,
            receipt.sequence,
            receipt.replacement_count()
        );
        if journal.origins.is_none() {
            // Preserve the existing raw-v1 wire lineage. Encoded capsules must
            // not be appended under a raw base that legacy replay cannot decode.
            submit(
                &journal.handle,
                C::BeginBatch(JournalBatch {
                    from_revision: receipt.before_revision,
                    to_revision: receipt.after_revision,
                    replacements,
                }),
            )?;
            transport.diagnose(document, receipt.sequence, JournalTransportPhase::Visit);
            visit_journal_replacements(&receipt, &mut |replacement| {
                submit(
                    &journal.handle,
                    C::BeginReplacement(JournalReplacement {
                        offset: replacement.range.start,
                        removed: replacement.range.end - replacement.range.start,
                        inserted: replacement.insert.len(),
                    }),
                )?;
                transport.diagnose(document, receipt.sequence, JournalTransportPhase::Copy);
                replacement.insert.write_to(&mut transport)?;
                transport.flush_pending()?;
                transport.diagnose(document, receipt.sequence, JournalTransportPhase::EndReplacement);
                submit(&journal.handle, C::EndReplacement)
            })?;
            transport.flush_pending()?;
            transport.diagnose(document, receipt.sequence, JournalTransportPhase::Commit);
            submit(&journal.handle, C::CommitBatch)?;
            journal.revision = receipt.after_revision;
            journal.sequence = receipt.sequence;
            journal.needs_flush = true;
            return Ok(());
        }
        let origins = journal.origins.as_mut().expect("encoded origin directory");
        let mut batch = origins.begin_batch()?;
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        submit(
            &journal.handle,
            C::BeginBatch(JournalBatch {
                from_revision: receipt.before_revision,
                to_revision: receipt.after_revision,
                replacements,
            }),
        )?;
        #[cfg(test)]
        let (mut text_records, mut origin_records, mut opaque_records) = (0_u64, 0_u64, 0_u64);
        transport.diagnose(document, receipt.sequence, JournalTransportPhase::Visit);
        visit_journal_replacements(&receipt, &mut |replacement| {
            transport.diagnose(document, receipt.sequence, JournalTransportPhase::Plan);
            #[cfg(test)]
            {
                for chunk in replacement.insert.snapshot.source_chunks(replacement.insert.range.clone())? {
                    match chunk.map_err(io::Error::other)? {
                        kwil_core_text::SourceChunk::Original { .. }
                        | kwil_core_text::SourceChunk::Added(_)
                        | kwil_core_text::SourceChunk::Preserved { .. } => text_records += 1,
                        kwil_core_text::SourceChunk::Decoded { .. } => origin_records += 1,
                        kwil_core_text::SourceChunk::Opaque { .. } => opaque_records += 1,
                    }
                }
            }
            let logical_len = replacement.insert.len();
            let plan = batch.plan(replacement.insert.snapshot, replacement.insert.range, cancel.clone())?;
            submit(
                &journal.handle,
                C::BeginEncodedReplacement {
                    replacement: JournalReplacement {
                        offset: replacement.range.start,
                        removed: replacement.range.end - replacement.range.start,
                        inserted: logical_len,
                    },
                    payload_len: plan.physical_len(),
                },
            )?;
            let mut reader = plan.start()?;
            // io::copy's fixed scratch is bounded independently of the file.
            // Coalesce its short record reads, then explicitly queue the final
            // block BEFORE admission or any replacement/batch terminator.
            transport.diagnose(document, receipt.sequence, JournalTransportPhase::Copy);
            transport.copy_from(&mut reader)?;
            transport.diagnose(document, receipt.sequence, JournalTransportPhase::Admit);
            reader.admit()?;
            transport.diagnose(document, receipt.sequence, JournalTransportPhase::EndReplacement);
            submit(&journal.handle, C::EndReplacement)
        })?;
        #[cfg(test)]
        eprintln!(
            "journal capsule diagnostics: sequence={} text_records={text_records} origin_records={origin_records} opaque_records={opaque_records}",
            receipt.sequence
        );
        transport.flush_pending()?;
        transport.diagnose(document, receipt.sequence, JournalTransportPhase::Commit);
        submit(&journal.handle, C::CommitBatch)?;
        batch.commit()?;
        journal.revision = receipt.after_revision;
        journal.sequence = receipt.sequence;
        journal.needs_flush = true;
        Ok(())
    })();
    transport.diagnose(
        document,
        receipt.sequence,
        if result.is_err() { JournalTransportPhase::Failed } else { JournalTransportPhase::Finished },
    );
    if result.is_err() {
        journal.failed = true;
    } else {
        storage.order.insert(document, receipt.sequence);
    }
    result
}

/// Worker-only streaming journal encoding. Source spans retain the receipt's
/// authenticated final snapshot, including typed raw/opaque origins. At most
/// one span is pending and neither recipe records nor file bytes are collected.
fn visit_journal_replacements(
    receipt: &crate::EditReceipt,
    emit: &mut impl FnMut(crate::ByteReplacement) -> io::Result<()>,
) -> io::Result<()> {
    const SPAN_BYTES: u64 = 64 << 10;
    let ascending = receipt.kind == crate::EditKind::Undo;
    let mut pending: Option<crate::ByteReplacement> = None;
    receipt.for_each_replacement(&mut |next| {
        if let Some(group) = &mut pending {
            let merged = if ascending {
                // Undo recipes address the intermediate version AFTER earlier
                // edits. A merged group addresses its version BEFORE those
                // internal edits, so remove the group's accumulated delta.
                let delta = group.insert.len() as i128 - (group.range.end - group.range.start) as i128;
                let start = next.range.start as i128 - delta;
                let end = next.range.end as i128 - delta;
                if start >= group.range.end as i128
                    && end <= u64::MAX as i128
                    && next.insert.range.start >= group.insert.range.end
                    && start - group.range.end as i128 == (next.insert.range.start - group.insert.range.end) as i128
                {
                    Some((group.range.start..end as u64, group.insert.range.start..next.insert.range.end))
                } else {
                    None
                }
            } else if next.range.end <= group.range.start
                && next.insert.range.end <= group.insert.range.start
                && group.range.start - next.range.end == group.insert.range.start - next.insert.range.end
            {
                Some((next.range.start..group.range.end, next.insert.range.start..group.insert.range.end))
            } else {
                None
            };
            if let Some((target, source)) = merged
                && target.end - target.start <= SPAN_BYTES
                && source.end - source.start <= SPAN_BYTES
            {
                group.range = target;
                group.insert.range = source;
                return Ok(());
            }
        }
        if let Some(previous) = pending.replace(next) {
            emit(previous)?;
        }
        Ok(())
    })?;
    if let Some(last) = pending {
        emit(last)?;
    }
    Ok(())
}

fn promote_transport_seed(
    root: &std::path::Path,
    service: &JournalService,
    document: DocumentId,
    journal: &mut JournalState,
) -> io::Result<()> {
    let staged = journal.promotion.as_ref().expect("staged transport seed");
    submit(&journal.handle, kwil_core_workspace::journal::JournalCommand::Finish)?;
    journal
        .handle
        .finished
        .recv_timeout(Duration::from_secs(30))
        .map_err(|_| io::Error::other("staged transport close timed out; prior recovery and owners retained"))??;
    let canonical = root.join(format!("{}.kwj", document.0));
    kwil_core_workspace::journal::promote_finished_seed(staged, &canonical, document)?;
    let recovered = RecoveredJournal::open(canonical, document, &AtomicBool::new(false), |_| Ok(()))?;
    let handle = resume(service, recovered)?;
    handle
        .ready
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| io::Error::other("promoted transport resume timed out; canonical new recovery retained"))??;
    journal.handle = handle;
    journal.promotion = None;
    Ok(())
}

fn poll_journals(storage: &mut Storage, complete: &impl Fn(DocumentResult)) {
    for (&document, journal) in &mut storage.journals {
        if journal.failed {
            continue;
        }
        if !journal.ready {
            match journal.handle.ready.try_recv() {
                Ok(Ok(_)) => {
                    let admitted = if let Some(receiver) = journal.seed_origins.take() {
                        receiver
                            .try_recv()
                            .map_err(|_| io::Error::other("Journal seed completed without origin admission"))
                            .and_then(|token| token)
                            .and_then(|token| {
                                journal
                                    .origins
                                    .as_mut()
                                    .ok_or_else(|| io::Error::other("Journal seed origin registry missing"))?
                                    .admit(token)
                            })
                    } else {
                        Ok(())
                    };
                    if let Err(error) = admitted {
                        journal.failed = true;
                        complete(DocumentResult::JournalDurable {
                            document,
                            revision: journal.revision,
                            sequence: journal.sequence,
                            result: Err(error),
                        });
                        continue;
                    }
                    if journal.promotion.is_some() {
                        if let Err(error) = promote_transport_seed(&storage.root, &storage.service, document, journal) {
                            journal.failed = true;
                            complete(DocumentResult::JournalDurable {
                                document,
                                revision: journal.revision,
                                sequence: journal.sequence,
                                result: Err(error),
                            });
                            continue;
                        }
                        // The old receipt counter belongs to the old coordinate
                        // system. Publish the new zero boundary only AFTER its
                        // durable seed, atomic authority promotion and resume.
                        storage.order.insert(document, journal.sequence);
                        storage.transports.remove(&document);
                        storage.finished_journals.remove(&document);
                        journal.needs_flush = false;
                        journal.flush_due = std::time::Instant::now() + Duration::from_secs(2);
                        complete(DocumentResult::JournalDurable {
                            document,
                            revision: journal.revision,
                            sequence: journal.sequence,
                            result: Ok(()),
                        });
                    } else {
                        storage.finished_journals.remove(&document);
                    }
                    journal.ready = true;
                    journal.barrier_durable_sequence = Some(journal.sequence);
                }
                Err(mpsc::TryRecvError::Empty) => continue,
                outcome => {
                    journal.failed = true;
                    let error = match outcome {
                        Ok(Err(error)) => error,
                        _ => io::Error::other("Journal seed worker disconnected"),
                    };
                    complete(DocumentResult::JournalDurable {
                        document,
                        revision: journal.revision,
                        sequence: journal.sequence,
                        result: Err(error),
                    });
                    continue;
                }
            }
        }
        if let Some((sequence, receiver)) = journal.flush.take() {
            match receiver.try_recv() {
                Ok(Ok(revision)) => {
                    journal.barrier_durable_sequence =
                        Some(journal.barrier_durable_sequence.map_or(sequence, |previous| previous.max(sequence)));
                    if sequence == journal.sequence && revision == journal.revision {
                        complete(DocumentResult::JournalDurable { document, revision, sequence, result: Ok(()) });
                    } else {
                        journal.needs_flush = true;
                        journal.flush_due = std::time::Instant::now();
                    }
                }
                Err(mpsc::TryRecvError::Empty) => journal.flush = Some((sequence, receiver)),
                outcome => {
                    journal.failed = true;
                    let error = match outcome {
                        Ok(Err(error)) => error,
                        _ => io::Error::other("Journal flush worker disconnected"),
                    };
                    complete(DocumentResult::JournalDurable {
                        document,
                        revision: journal.revision,
                        sequence: journal.sequence,
                        result: Err(error),
                    });
                }
            }
        }
        if journal.needs_flush
            && journal.flush.is_none()
            && !journal.failed
            && std::time::Instant::now() >= journal.flush_due
        {
            match journal.handle.try_flush() {
                Ok(receiver) => {
                    journal.flush = Some((journal.sequence, receiver));
                    journal.needs_flush = false;
                    journal.flush_due = std::time::Instant::now() + Duration::from_secs(2);
                }
                Err(TrySendError::Full(_)) => {}
                Err(_) => {
                    journal.failed = true;
                    complete(DocumentResult::JournalDurable {
                        document,
                        revision: journal.revision,
                        sequence: journal.sequence,
                        result: Err(io::Error::other("Journal flush admission failed")),
                    });
                }
            }
        }
    }
}

fn recover_prepared(
    root: &std::path::Path,
    encoding: &mut encoding_runtime::EncodingRuntime,
    path: PathBuf,
    tab: &Tab,
) -> io::Result<(Document, RecoveredJournal, JournalBase, Option<provenance::OriginRegistry>)> {
    recover_prepared_with_add_limit(root, encoding, path, tab, 8 * 1024 * 1024)
}

fn recover_prepared_with_add_limit(
    root: &std::path::Path,
    encoding: &mut encoding_runtime::EncodingRuntime,
    path: PathBuf,
    tab: &Tab,
    add_memory_bytes: usize,
) -> io::Result<(Document, RecoveredJournal, JournalBase, Option<provenance::OriginRegistry>)> {
    use provenance::{
        decode::DecodeLimits,
        journal::{JournalRecovery, JournalReplayLimits},
        replay::ReplayLimits,
    };
    let max_origins = session::MAX_TABS + session::MAX_CLOSED;
    let path_cap = root
        .as_os_str()
        .as_encoded_bytes()
        .len()
        .checked_add(64)
        .ok_or_else(|| io::Error::other("Recovery private path overflow"))?;
    let recovery = JournalRecovery::new(
        root.to_owned(),
        JournalReplayLimits {
            replay: ReplayLimits {
                core: kwil_core_text::RecoveryLimits {
                    add_memory_bytes,
                    undo_memory_bytes: usize::try_from(
                        kwil_core_commands::settings::Settings::default().undo.memory_limit_bytes,
                    )
                    .unwrap_or(usize::MAX),
                    max_origins,
                    max_records: 1 << 20,
                    max_pieces: 1 << 20,
                    max_blocks: 1 << 20,
                    max_metadata_bytes: 64 * 1024 * 1024,
                },
                decode: DecodeLimits {
                    max_physical_bytes: u64::MAX,
                    max_logical_bytes: u64::MAX,
                    max_records: 1 << 20,
                },
                max_origin_storage_bytes: u64::MAX,
                max_private_path_bytes: path_cap,
            },
            max_payload_bytes: u64::MAX,
            max_base_metadata_bytes: session::MAX_PATH_BYTES + 256,
            max_document_bytes: u64::MAX,
            max_batches: 1 << 20,
            max_replacements: 1 << 20,
        },
    )?;
    match encoding.recover(recovery, path.clone(), tab, max_origins, 8 * 1024 * 1024, &AtomicBool::new(false)) {
        Ok((document, journal, base, origins)) => Ok((document, journal, base, Some(origins))),
        // The authenticated consumer identifies the old raw base explicitly.
        // Other framing, checksum, policy and cap failures retain their error.
        Err(error) if error.kind() == io::ErrorKind::Unsupported => {
            recover_document(path, tab).and_then(|(document, journal, base)| {
                encoding.retain_recovered_spills(&document.snapshot(), &AtomicBool::new(false))?;
                Ok((document, journal, base, None))
            })
        }
        Err(error) => Err(error),
    }
}

fn recover_document(path: PathBuf, tab: &Tab) -> io::Result<(Document, RecoveredJournal, JournalBase)> {
    let mut buffer = Buffer::with_add_memory_limit(0);
    buffer.set_undo_memory_limit(0);
    let mut edit = None;
    let mut base = None;
    let recovered = RecoveredJournal::open(path, tab.id, &AtomicBool::new(false), |event| {
        match event {
            RecoveryEvent::EncodedBase { .. } | RecoveryEvent::BeginEncodedReplacement { .. } => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "Lossless recovery requires provenance replay; journal retained",
                ));
            }
            RecoveryEvent::Base(metadata) => {
                base = Some(metadata.clone());
            }
            RecoveryEvent::BaseComplete => {}
            RecoveryEvent::BaseBytes(bytes) => {
                buffer.insert(buffer.len(), bytes)?;
            }
            RecoveryEvent::BeginEdit(recipe) => {
                buffer.begin();
                buffer.delete(recipe.offset..recipe.offset + recipe.removed)?;
                edit = Some((recipe.offset, 0));
            }
            RecoveryEvent::EditBytes(bytes) => {
                let (offset, written) = edit.as_mut().ok_or_else(|| io::Error::other("Missing recovery edit"))?;
                buffer.insert(*offset + *written, bytes)?;
                *written += bytes.len() as u64;
            }
            RecoveryEvent::CommitEdit => {
                buffer.commit();
                edit = None;
            }
            RecoveryEvent::BeginBatch(_) => {
                buffer.begin();
            }
            RecoveryEvent::BeginReplacement(recipe) => {
                buffer.delete(recipe.offset..recipe.offset + recipe.removed)?;
                edit = Some((recipe.offset, 0));
            }
            RecoveryEvent::EndReplacement => {
                edit = None;
            }
            RecoveryEvent::CommitBatch => {
                buffer.commit();
            }
        }
        Ok(())
    })?;
    buffer.set_undo_memory_limit(0);
    let revision = recovered.summary().revision;
    let mut document = Document::new();
    // Replay stays spill-only. Import immutable disk pieces into a fresh normal
    // interactive budget without copying bytes or keeping replay undo history.
    let snapshot = buffer.snapshot();
    let mut interactive = Buffer::empty();
    interactive.set_undo_memory_limit(0);
    interactive.insert_snapshot_range(0, &snapshot, 0..snapshot.len())?;
    interactive.set_undo_memory_limit(
        usize::try_from(kwil_core_commands::settings::Settings::default().undo.memory_limit_bytes)
            .unwrap_or(usize::MAX),
    );
    document.buffer = interactive;
    // A dirty journal has no verified pre-crash original. Keep its bytes
    // unnamed so Save requires an explicit destination instead of adopting
    // the current disk contents as a fresh baseline. Clean recovery below
    // restores the name only after byte equality supplies a valid original.
    // Recovery is dirty even when metadata lagged behind the journal.
    document.revision = revision;
    document.saved_revision = if tab.revision == tab.saved_revision && tab.revision == revision {
        revision
    } else if tab.saved_revision == revision {
        revision.wrapping_add(1)
    } else {
        tab.saved_revision
    };
    document.next_revision = revision
        .max(document.saved_revision)
        .checked_add(2)
        .ok_or_else(|| io::Error::other("Recovery revision overflow"))?;
    // Restore newline policy from a bounded sample before interactive editing.
    let mut previous = 0;
    'sample: for chunk in snapshot.chunks(0..snapshot.len().min(64 << 10))? {
        for &byte in chunk.map_err(io::Error::other)? {
            if byte == b'\n' {
                if previous == b'\r' {
                    document.line_ending = crate::DocumentLineEnding::CrLf;
                }
                break 'sample;
            }
            previous = byte;
        }
    }
    if !document.is_dirty()
        && let Some(path) = &tab.path
    {
        let disk = match kwil_core_workspace::open_for_editing(path) {
            Ok(file) => {
                // A raw-v1 journal has no decoded-coordinate policy. Never
                // attach its raw index under the default UTF-8 save policy
                // when the actual disk interpretation is different or unclear.
                let encoded = kwil_core_text::encoding::EncodedFile::new(file.clone()).map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::Unsupported,
                        format!("Legacy raw-v1 clean recovery cannot verify the encoding: {error}; journal retained"),
                    )
                })?;
                if encoded.detection().encoding != kwil_core_text::encoding::Encoding::UTF8
                    || file.bytes().map_err(io::Error::other)?.contains(&0)
                {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "Legacy raw-v1 clean recovery requires unambiguous UTF-8 without NUL bytes; journal retained",
                    ));
                }
                index(path.clone(), file)
            }
            Err(error) => Err(error),
        };
        match disk {
            Ok(mut disk) if snapshots_equal(&snapshot, &disk.snapshot())? => {
                disk.revision = document.revision;
                disk.saved_revision = document.saved_revision;
                disk.next_revision = document.next_revision;
                document = disk;
            }
            _ => {
                document.path = None;
                mark_recovered_unsaved(&mut document)?;
            }
        }
    }
    if document.path.is_none() && tab.recovery_path.is_some() && !document.is_dirty() {
        mark_recovered_unsaved(&mut document)?;
    }
    Ok((document, recovered, base.ok_or_else(|| io::Error::other("Missing recovery base"))?))
}

fn mark_recovered_unsaved(document: &mut Document) -> io::Result<()> {
    document.next_revision.checked_add(1).ok_or_else(|| io::Error::other("Recovery revision overflow"))?;
    document.mark_unsaved();
    Ok(())
}

fn snapshots_equal(a: &kwil_core_text::Snapshot, b: &kwil_core_text::Snapshot) -> io::Result<bool> {
    use std::io::Read;
    if a.len() != b.len() {
        return Ok(false);
    }
    let mut left = SnapshotReader::new(a.clone());
    let mut right = SnapshotReader::new(b.clone());
    let (mut aa, mut bb) = ([0u8; 64 << 10], [0u8; 64 << 10]);
    loop {
        let n = left.read(&mut aa)?;
        if n == 0 {
            return Ok(true);
        }
        right.read_exact(&mut bb[..n])?;
        if aa[..n] != bb[..n] {
            return Ok(false);
        }
    }
}

fn resume(service: &JournalService, mut recovered: RecoveredJournal) -> io::Result<JournalSession> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match service.try_resume(recovered) {
            Ok(session) => return Ok(session),
            Err(TrySendError::Full(value)) if std::time::Instant::now() < deadline => {
                recovered = *value;
                std::thread::park_timeout(Duration::from_millis(2));
            }
            Err(_) => return Err(io::Error::other("Journal resume admission failed; recovery retained")),
        }
    }
}

fn index(path: PathBuf, file: TextFile) -> io::Result<Document> {
    file.build_index(1).map_err(io::Error::other)?;
    Document::from_file(path, file)
}

/// Owns the immutable tree; read calls borrow at most the caller's chunk size.
pub struct SnapshotReader {
    snapshot: kwil_core_text::Snapshot,
    offset: u64,
}
impl SnapshotReader {
    pub fn new(snapshot: kwil_core_text::Snapshot) -> Self {
        Self { snapshot, offset: 0 }
    }
}
impl io::Read for SnapshotReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let end = self.snapshot.len().min(self.offset.saturating_add(output.len() as u64));
        let mut copied = 0;
        for chunk in self.snapshot.chunks(self.offset..end)? {
            let chunk = chunk.map_err(io::Error::other)?;
            output[copied..copied + chunk.len()].copy_from_slice(chunk);
            copied += chunk.len();
        }
        self.offset += copied as u64;
        Ok(copied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kwil_core_workspace::journal::{JournalBatch, JournalCommand, JournalEdit, JournalReplacement, JournalSeed};

    fn replacement_journal_span_oracle(receipt: &crate::EditReceipt, expected_groups: Option<usize>) {
        let mut replay = Vec::new();
        receipt.before.write_to(&mut replay).unwrap();
        let mut expected = Vec::new();
        receipt.after.write_to(&mut expected).unwrap();
        let mut groups = 0;
        visit_journal_replacements(receipt, &mut |replacement| {
            assert!(replacement.range.end - replacement.range.start <= 64 << 10);
            assert!(replacement.insert.len() <= 64 << 10);
            let mut inserted = Vec::new();
            replacement.insert.write_to(&mut inserted)?;
            replay.splice(replacement.range.start as usize..replacement.range.end as usize, inserted);
            groups += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(
            replay, expected,
            "grouped sequential-coordinate replay must reproduce the authenticated after snapshot"
        );
        if let Some(count) = expected_groups {
            assert_eq!(groups, count);
        } else {
            assert!(groups >= 2, "large affected spans must stay in bounded separate groups");
        }
    }

    fn replacement_journal_receipts(source: &str, edits: Vec<kwil_core_search::Edit>) -> Vec<crate::EditReceipt> {
        use kwil_core_search::worker::{ReplaceRecipe, ReplacementEdits, SearchKey};
        let mut editor = crate::Editor::new(Document::new());
        editor.insert_text(source, Duration::ZERO).unwrap();
        let (sender, receiver) = mpsc::sync_channel(3);
        editor
            .set_edit_observer(Some(Box::new(move |receipt| {
                sender.try_send(receipt).map_err(|error| match error {
                    TrySendError::Full(receipt) | TrySendError::Disconnected(receipt) => receipt,
                })
            })))
            .unwrap();
        editor
            .apply_search_replacement(ReplaceRecipe {
                key: SearchKey { document_id: 1, revision: editor.document().revision(), generation: 1 },
                edits: ReplacementEdits::Inline(std::sync::Arc::new(edits)),
            })
            .unwrap();
        editor.undo().unwrap();
        editor.redo().unwrap();
        (0..3).map(|_| receiver.recv_timeout(Duration::from_secs(1)).unwrap()).collect()
    }

    #[test]
    fn replacement_journal_spans_preserve_gaps_and_mixed_size_undo_redo() {
        let edit = |offset, len, bytes: &[u8]| kwil_core_search::Edit { offset, len, new_bytes: bytes.to_vec() };
        let receipts = replacement_journal_receipts(
            "a---a---a---a",
            vec![edit(0, 1, b"bc"), edit(4, 1, b""), edit(8, 1, b"long"), edit(12, 1, b"z")],
        );
        for receipt in receipts {
            assert_eq!(receipt.replacement_count(), 4, "original observer recipe is unchanged");
            replacement_journal_span_oracle(&receipt, Some(1));
        }
        let receipts = replacement_journal_receipts(
            "a---a---a",
            vec![edit(0, 0, b"prefix"), edit(4, 1, b""), edit(9, 0, b"suffix")],
        );
        for receipt in receipts {
            replacement_journal_span_oracle(&receipt, Some(1));
        }
    }

    #[test]
    fn replacement_journal_spans_split_at_bound_without_losing_coordinate_shifts() {
        let source = "a".repeat((128 << 10) + 1);
        let edit = |offset, len, bytes: &[u8]| kwil_core_search::Edit { offset, len, new_bytes: bytes.to_vec() };
        let receipts = replacement_journal_receipts(
            &source,
            vec![edit(0, 1, b"start"), edit(4, 1, b""), edit(64 << 10, 1, b"middle"), edit(128 << 10, 1, b"end")],
        );
        for receipt in receipts {
            replacement_journal_span_oracle(&receipt, None);
        }
    }

    pub(super) struct Scratch(pub(super) PathBuf);
    impl Scratch {
        pub(super) fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "kwil-documents-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir(&root).unwrap();
            Self(root)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            if std::thread::panicking() {
                eprintln!("Failed fixture retained at {}", self.0.display());
                return;
            }
            for entry in std::fs::read_dir(&self.0).unwrap() {
                let path = entry.unwrap().path();
                assert!(path.is_file(), "fixture cleanup only removes its own files");
                std::fs::remove_file(path).unwrap();
            }
            std::fs::remove_dir(&self.0).unwrap();
        }
    }

    fn fixture_journal(root: &std::path::Path, tab: &Tab, bytes: &[u8]) {
        let mut buffer = Buffer::empty();
        buffer.insert(0, bytes).unwrap();
        let mut service = JournalService::spawn(Duration::from_secs(1)).unwrap();
        let handle = service
            .try_create(JournalSeed {
                path: root.join(format!("{}.kwj", tab.id.0)),
                base: JournalBase {
                    document: tab.id,
                    revision: tab.revision,
                    original_path: tab.path.clone(),
                    original_identity: [0; 4],
                    codec: b"raw-v1".to_vec(),
                    byte_len: bytes.len() as u64,
                },
                source: Box::new(SnapshotReader::new(buffer.snapshot())),
            })
            .unwrap();
        handle.ready.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
        service.finish();
        service.finished.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
    }

    fn recovery_tab(path: Option<PathBuf>) -> Tab {
        let mut session = Session::default();
        let id = session.open(path).unwrap();
        session.tab_mut(id).unwrap().revision = 3;
        session.tab_mut(id).unwrap().saved_revision = 2;
        session.tabs()[0].clone()
    }

    fn stop_storage(mut storage: Storage) {
        storage.service.finish();
        storage.service.finished.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
        storage.journals.clear();
        storage.finished_journals.clear();
        let _ = storage.encoding.retire();
        drop(storage);
    }

    #[test]
    fn malformed_orphan_does_not_disable_unrelated_decoded_open_and_save() {
        let root = Scratch::new();
        let corrupt = root.0.join("91.kwj");
        let corrupt_bytes = b"malformed recovery journal";
        std::fs::write(&corrupt, corrupt_bytes).unwrap();
        let source = root.0.join("unrelated.txt");
        std::fs::write(&source, b"unrelated").unwrap();
        let output = root.0.join("new-work.txt");
        let (tx, rx) = mpsc::channel();
        let worker = DocumentWorker::spawn(move |result| {
            let _ = tx.send(result);
        })
        .unwrap();
        worker.try_submit(DocumentRequest::Prepare { root: Some(root.0.clone()) }).unwrap();
        let failed_recoveries = loop {
            if let DocumentResult::Prepared(result) = rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                break result
                    .ok()
                    .map(|prepared| prepared.documents.iter().filter(|(_, result)| result.is_err()).count());
            }
        };
        worker
            .try_submit(DocumentRequest::Open { document: DocumentId(92), revision: 0, path: source.clone() })
            .unwrap();
        let opened = loop {
            if let DocumentResult::Indexed { result, .. } = rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                break result.is_ok();
            }
        };
        let mut editor = crate::Editor::new(Document::new());
        editor
            .execute("edit.insert", &crate::CommandArgs { text: Some("new work".into()), ..Default::default() }, 1)
            .unwrap();
        let request = editor.save_request(Some(output.clone())).unwrap();
        worker
            .try_submit(DocumentRequest::Save {
                document: DocumentId(93),
                request,
                plan: editor.document().save_plan(),
            })
            .unwrap();
        let saved = loop {
            if let DocumentResult::Saved { result, .. } = rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                break result.is_ok();
            }
        };
        drop(editor);
        drop(worker);
        wait_stopped(&rx);
        assert_eq!(failed_recoveries, Some(1), "retain the failed orphan as an addressed recovery item");
        assert!(opened, "bad recovery must not block unrelated decoded opening");
        assert!(saved, "bad recovery must not block new-document Save As");
        assert_eq!(std::fs::read(output).unwrap(), b"new work");
        assert_eq!(std::fs::read(source).unwrap(), b"unrelated");
        assert_eq!(std::fs::read(corrupt).unwrap(), corrupt_bytes);
    }

    #[test]
    fn production_save_normalization_is_an_undoable_receipted_edit() {
        let root = Scratch::new();
        let path = root.0.join("joined-opaque.txt");
        std::fs::write(&path, b"\xef\xbb\xbf\xc2X\xa2").unwrap();
        let (_, mut storage) = prepare_root(root.0.clone()).unwrap();
        let cancel = AtomicBool::new(false);
        let document =
            storage.encoding.open(path.clone(), TextFile::open(&path).unwrap(), None, &cancel, |_| Ok(())).unwrap();
        let mut editor = crate::Editor::new(document);
        let receipts = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = receipts.clone();
        editor
            .set_edit_observer(Some(Box::new(move |receipt| {
                recorded.lock().unwrap().push(receipt);
                Ok(())
            })))
            .unwrap();
        editor.set_cursors(vec![crate::Cursor { anchor: 1, head: 1, desired_column: None }], 0).unwrap();
        editor.execute("edit.delete", &crate::CommandArgs::default(), 1).unwrap();
        let before_revision = editor.document().revision();
        let request = editor.save_request(None).unwrap();
        let saved = storage.encoding.save(request, editor.document().save_plan(), &cancel).unwrap();
        editor.finish_save_for_test(Ok(saved)).unwrap();
        let after_revision = editor.document().revision();
        let recorded = receipts.lock().unwrap();
        let receipt_count = recorded.len();
        let save_transition = recorded.last().cloned();
        drop(recorded);
        let mut saved_text = Vec::new();
        editor.document().snapshot().write_to(&mut saved_text).unwrap();
        editor.execute("edit.undo", &crate::CommandArgs::default(), 2).unwrap();
        let mut undone = Vec::new();
        editor.document().snapshot().write_to(&mut undone).unwrap();
        editor.execute("edit.redo", &crate::CommandArgs::default(), 3).unwrap();
        let mut redone = Vec::new();
        editor.document().snapshot().write_to(&mut redone).unwrap();
        drop(editor);
        drop(receipts);
        let disk = std::fs::read(&path).unwrap();
        // Release captured snapshot pins before explicit worker retirement.
        let transition = save_transition
            .map(|receipt| (receipt.before_revision, receipt.after_revision, receipt.replacement_count()));
        stop_storage(storage);
        assert_ne!(after_revision, before_revision, "normalization requires a real model revision");
        assert_eq!(receipt_count, 2, "normalization requires its own exact journal receipt");
        assert_eq!(transition, Some((before_revision, after_revision, 1)));
        assert_eq!(disk, b"\xef\xbb\xbf\xc2\xa2");
        assert_eq!(saved_text, "¢".as_bytes());
        assert_eq!(undone, b"\xff\xff", "undo must restore both original opaque atoms");
        assert_eq!(redone, "¢".as_bytes());
    }

    #[test]
    fn normalized_save_caret_preserves_crlf_boundary_through_undo_redo_and_delete() {
        let root = Scratch::new();
        let path = root.0.join("normalized-crlf.txt");
        std::fs::write(&path, b"\xef\xbb\xbf\xc2X\xa2 \r\n").unwrap();
        let (_, mut storage) = prepare_root(root.0.clone()).unwrap();
        let cancel = AtomicBool::new(false);
        let document =
            storage.encoding.open(path.clone(), TextFile::open(&path).unwrap(), None, &cancel, |_| Ok(())).unwrap();
        let mut editor = crate::Editor::new(document);
        editor.settings.editor.trim_trailing_whitespace_on_save = true;
        editor.set_cursors(vec![crate::Cursor { anchor: 1, head: 1, desired_column: None }], 0).unwrap();
        editor.execute("edit.delete", &Default::default(), 1).unwrap();
        editor.set_cursors(vec![crate::Cursor { anchor: 3, head: 3, desired_column: None }], 0).unwrap();
        let request = editor.save_request(None).unwrap();
        let saved = storage.encoding.save(request, editor.document().save_plan(), &cancel).unwrap();
        editor.finish_save_for_test(Ok(saved)).unwrap();
        let saved_caret = editor.cursors()[0].head;
        editor.execute("edit.undo", &Default::default(), 2).unwrap();
        let undone_caret = editor.cursors()[0].head;
        editor.execute("edit.redo", &Default::default(), 3).unwrap();
        let redone_caret = editor.cursors()[0].head;
        editor.execute("edit.delete", &Default::default(), 4).unwrap();
        let mut deleted = Vec::new();
        editor.document().snapshot().write_to(&mut deleted).unwrap();
        drop(editor);
        stop_storage(storage);
        assert_eq!(saved_caret, 2, "normalized caret must precede the entire CRLF pair");
        assert_eq!(undone_caret, 3);
        assert_eq!(redone_caret, 2);
        assert_eq!(deleted, "¢".as_bytes(), "Delete consumes both newline bytes");
    }

    pub(super) fn prepare_test_worker(
        worker: &DocumentWorker,
        receiver: &mpsc::Receiver<DocumentResult>,
        root: &std::path::Path,
    ) {
        worker.try_submit(DocumentRequest::Prepare { root: Some(root.to_owned()) }).unwrap();
        loop {
            if let DocumentResult::Prepared(result) = receiver.recv_timeout(Duration::from_secs(5)).unwrap() {
                drop(result.unwrap());
                break;
            }
        }
    }

    #[test]
    fn production_normalized_save_backpressure_stale_and_trim_remain_ordered() {
        use std::sync::{Arc, Mutex, atomic::Ordering};
        for mode in ["pending", "stale", "trim"] {
            let root = Scratch::new();
            let path = root.0.join("normalized.txt");
            let raw = if mode == "trim" {
                b"\xef\xbb\xbf\xc2X\xa2  \n".as_slice()
            } else {
                b"\xef\xbb\xbf\xc2X\xa2".as_slice()
            };
            std::fs::write(&path, raw).unwrap();
            let (_, mut storage) = prepare_root(root.0.clone()).unwrap();
            let cancel = AtomicBool::new(false);
            let document =
                storage.encoding.open(path.clone(), TextFile::open(&path).unwrap(), None, &cancel, |_| Ok(())).unwrap();
            let mut editor = crate::Editor::new(document);
            editor.settings.editor.trim_trailing_whitespace_on_save = mode == "trim";
            let accepting = Arc::new(AtomicBool::new(mode == "stale"));
            let gate = accepting.clone();
            let receipts = Arc::new(Mutex::new(Vec::new()));
            let recorded = receipts.clone();
            editor
                .set_edit_observer(Some(Box::new(move |receipt| {
                    if !gate.load(Ordering::Relaxed) {
                        return Err(receipt);
                    }
                    recorded.lock().unwrap().push(receipt);
                    Ok(())
                })))
                .unwrap();
            editor.set_cursors(vec![crate::Cursor { anchor: 1, head: 1, desired_column: None }], 0).unwrap();
            editor.execute("edit.delete", &Default::default(), 1).unwrap();
            let source_revision = editor.document().revision();
            let request = editor.save_request(None).unwrap();
            let saved = storage.encoding.save(request, editor.document().save_plan(), &cancel).unwrap();
            assert!(saved.normalized, "the worker must distinguish normalization from trim: {mode}");
            assert!(saved.trim_deletions.is_none(), "unused trim spool must retire on worker");
            if mode == "stale" {
                accepting.store(false, Ordering::Relaxed);
                editor
                    .execute("edit.insert", &crate::CommandArgs { text: Some("!".into()), ..Default::default() }, 2)
                    .unwrap();
                let live_revision = editor.document().revision();
                editor.finish_save_for_test(Ok(saved)).unwrap();
                assert!(!editor.has_pending_save_completion());
                assert!(editor.has_pending_edit_receipt());
                assert!(editor.document().is_dirty());
                assert_eq!(editor.document().revision(), live_revision);
            } else {
                let error = editor.finish_save_for_test(Ok(saved)).unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
                assert!(editor.has_pending_save_completion());
                assert_eq!(editor.document().revision(), source_revision);
                assert!(
                    editor
                        .execute(
                            "edit.insert",
                            &crate::CommandArgs { text: Some("must not retry".into()), ..Default::default() },
                            2
                        )
                        .is_err()
                );
            }
            accepting.store(true, Ordering::Relaxed);
            editor.flush_edit_receipt().unwrap();
            if mode != "stale" {
                assert!(editor.finish_pending_save().unwrap().is_some());
            }
            let mut live = Vec::new();
            editor.document().snapshot().write_to(&mut live).unwrap();
            let recorded = receipts.lock().unwrap();
            assert_eq!(recorded.len(), 2);
            assert_eq!((recorded[0].sequence, recorded[1].sequence), (1, 2));
            let normalized_receipts =
                recorded.iter().filter(|receipt| receipt.kind == crate::EditKind::WorkerSave).count();
            drop(recorded);
            drop(editor);
            drop(receipts);
            stop_storage(storage);
            if mode == "stale" {
                assert_eq!(live, b"\xff!\xff");
                assert_eq!(normalized_receipts, 0, "stale completion emits no phantom edit");
            } else {
                assert_eq!(live, if mode == "trim" { "¢\n".as_bytes() } else { "¢".as_bytes() });
                assert_eq!(normalized_receipts, 1, "retry publishes exactly one normalization");
            }
        }
    }

    #[test]
    fn production_utf16_open_edit_save_uses_logical_coordinates_and_preserves_raw_codec() {
        let root = Scratch::new();
        let path = root.0.join("utf16.txt");
        std::fs::write(&path, b"\xff\xfeA\0").unwrap();
        let (tx, rx) = mpsc::channel();
        let worker = DocumentWorker::spawn(move |result| {
            let _ = tx.send(result);
        })
        .unwrap();
        worker.try_submit(DocumentRequest::Prepare { root: Some(root.0.clone()) }).unwrap();
        loop {
            if let DocumentResult::Prepared(result) = rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                drop(result.unwrap());
                break;
            }
        }
        worker.try_submit(DocumentRequest::Open { document: DocumentId(1), revision: 0, path: path.clone() }).unwrap();
        let mut preview_seen = false;
        let mut raw_opened = false;
        let document = loop {
            match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                DocumentResult::Preview { preview, .. } => {
                    preview_seen = preview.text == b"A";
                }
                DocumentResult::Indexed { result, .. } => {
                    break *result.unwrap();
                }
                DocumentResult::Opened { result: Ok(_), .. } => raw_opened = true,
                _ => {}
            }
        };
        let mut editor = crate::Editor::new(document);
        let initial_len = editor.document().snapshot().len();
        editor.execute("motion.lastLine", &crate::CommandArgs::default(), 0).unwrap();
        editor.execute("edit.insert", &crate::CommandArgs { text: Some("x".into()), ..Default::default() }, 1).unwrap();
        let request = editor.save_request(None).unwrap();
        let plan = editor.document().save_plan();
        worker.try_submit(DocumentRequest::Save { document: DocumentId(1), request, plan }).unwrap();
        let save_succeeded = loop {
            if let DocumentResult::Saved { result, .. } = rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                let succeeded = result.is_ok();
                let _ = editor.finish_save_for_test(result);
                break succeeded;
            }
        };
        let saved_bytes = std::fs::read(&path).unwrap();
        let saved_len = editor.document().snapshot().len();
        editor.execute("edit.undo", &crate::CommandArgs::default(), 2).unwrap();
        let mut logical = Vec::new();
        editor.document().snapshot().write_to(&mut logical).unwrap();
        drop(editor);
        drop(worker);
        wait_stopped(&rx);
        assert!(preview_seen, "decoded first screen must precede editable attachment");
        assert!(!raw_opened, "raw file bytes must not be exposed before decoded preview");
        assert!(save_succeeded, "encoded save must succeed for UTF16 append");
        assert_eq!(initial_len, 1);
        assert_eq!(saved_bytes, b"\xff\xfeA\0x\0");
        assert_eq!(saved_len, 2);
        assert_eq!(logical, b"A");
        assert!(
            !std::fs::read_dir(&root.0).unwrap().any(|entry| entry.unwrap().path().is_dir()),
            "worker retires both checkpoint generations after editor release"
        );
    }

    #[test]
    fn recovered_typing_has_resident_budget_and_can_be_undone() {
        let scratch = Scratch::new();
        let tab = recovery_tab(None);
        fixture_journal(&scratch.0, &tab, b"restored");
        let (document, _, _) = recover_document(scratch.0.join("1.kwj"), &tab).unwrap();
        let mut editor = crate::Editor::new(document);
        editor
            .execute("edit.insert", &crate::CommandArgs { text: Some("new".into()), ..Default::default() }, 1)
            .unwrap();
        assert!(editor.document().buffer().add_memory_bytes() > 0, "interactive typing must not spill every key");
        editor.execute("edit.undo", &Default::default(), 1).unwrap();
        let mut actual = Vec::new();
        editor.document().snapshot().write_to(&mut actual).unwrap();
        assert_eq!(actual, b"restored");
    }

    #[test]
    fn restored_unsaved_marker_stays_outside_future_edit_and_undo_revisions() {
        let scratch = Scratch::new();
        let mut tab = recovery_tab(None);
        tab.saved_revision = 100;
        fixture_journal(&scratch.0, &tab, b"retained recovery");
        let (document, _, _) = recover_document(scratch.0.join("1.kwj"), &tab).unwrap();
        assert!(document.next_revision > tab.saved_revision);
        let mut editor = crate::Editor::new(document);
        editor.execute("edit.insert", &crate::CommandArgs { text: Some("!".into()), ..Default::default() }, 1).unwrap();
        assert!(editor.document().is_dirty());
        editor.execute("edit.undo", &Default::default(), 1).unwrap();
        assert!(editor.document().is_dirty());
        editor.execute("edit.redo", &Default::default(), 1).unwrap();
        assert!(editor.document().is_dirty());
    }

    #[test]
    fn reopen_admission_retains_provisional_identity_until_matching_release_and_refuses_stale_backing() {
        let scratch = Scratch::new();
        let path = scratch.0.join("protected.txt");
        std::fs::write(&path, b"protected").unwrap();
        let original = kwil_core_workspace::open_for_editing(&path).unwrap();
        let (sender, receiver) = mpsc::channel();
        let worker = DocumentWorker::spawn(move |result| {
            let _ = sender.send(result);
        })
        .unwrap();
        prepare_test_worker(&worker, &receiver, &scratch.0);
        worker
            .try_submit(DocumentRequest::ValidateReopen {
                document: DocumentId(1),
                generation: 7,
                path: path.clone(),
                original: Some(original.clone()),
                cached: true,
            })
            .unwrap();
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(5)).unwrap(),
            DocumentResult::ReopenValidated { result: Ok(true), .. }
        ));
        let request = || {
            let mut editor = crate::Editor::new(Document::new());
            editor
                .execute(
                    "edit.insert",
                    &crate::CommandArgs { text: Some("replacement".into()), ..Default::default() },
                    1,
                )
                .unwrap();
            editor.save_request(Some(path.clone())).unwrap()
        };
        // Refreshing UI destinations and an old release cannot erase admission.
        worker.set_save_destinations(Vec::new());
        worker.try_submit(DocumentRequest::ReleaseReopen { document: DocumentId(1), generation: 6 }).unwrap();
        worker
            .try_submit(DocumentRequest::Save {
                document: DocumentId(2),
                request: request(),
                plan: Document::new().save_plan(),
            })
            .unwrap();
        assert!(
            matches!(receiver.recv_timeout(Duration::from_secs(5)).unwrap(), DocumentResult::Saved { result: Err(error), .. } if error.kind() == io::ErrorKind::AlreadyExists)
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"protected");
        worker.try_submit(DocumentRequest::ReleaseReopen { document: DocumentId(1), generation: 7 }).unwrap();
        worker
            .try_submit(DocumentRequest::Save {
                document: DocumentId(2),
                request: request(),
                plan: Document::new().save_plan(),
            })
            .unwrap();
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(5)).unwrap(),
            DocumentResult::Saved { result: Ok(_), .. }
        ));
        worker
            .try_submit(DocumentRequest::ValidateReopen {
                document: DocumentId(1),
                generation: 8,
                path: path.clone(),
                original: Some(original),
                cached: true,
            })
            .unwrap();
        assert!(
            matches!(
                receiver.recv_timeout(Duration::from_secs(5)).unwrap(),
                DocumentResult::ReopenValidated { result: Err(_), .. }
            ),
            "cached backing replaced by an earlier save must not attach"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
        drop(worker);
        wait_stopped(&receiver);
    }

    #[test]
    fn save_worker_refreshes_native_file_identity_before_writing_an_alias() {
        let scratch = Scratch::new();
        let path = scratch.0.join("protected.txt");
        let alias = scratch.0.join("hard-link.txt");
        std::fs::write(&path, b"protected").unwrap();
        std::fs::hard_link(&path, &alias).unwrap();
        let (sender, receiver) = mpsc::channel();
        let worker = DocumentWorker::spawn(move |result| {
            let _ = sender.send(result);
        })
        .unwrap();
        prepare_test_worker(&worker, &receiver, &scratch.0);
        worker.set_save_destinations(vec![(DocumentId(1), path.clone())]);
        let mut editor = crate::Editor::new(Document::new());
        editor
            .execute("edit.insert", &crate::CommandArgs { text: Some("overwrite".into()), ..Default::default() }, 1)
            .unwrap();
        let request = editor.save_request(Some(alias.clone())).unwrap();
        let revision = request.revision;
        worker
            .try_submit(DocumentRequest::Save { document: DocumentId(2), request, plan: Document::new().save_plan() })
            .unwrap();
        match receiver.recv_timeout(Duration::from_secs(5)).unwrap() {
            DocumentResult::Saved { document, revision: source, result } => {
                assert_eq!(document, DocumentId(2));
                assert_eq!(source, revision);
                let error = result.err().expect("native alias must be rejected");
                assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
                assert!(editor.finish_save_for_test(Err(error)).is_err());
            }
            _ => panic!("expected addressed worker save rejection"),
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"protected");
        assert_eq!(std::fs::read(&alias).unwrap(), b"protected");
        // A later request sees the latest reservation set, not cached identities.
        worker.set_save_destinations(Vec::new());
        let request = editor.save_request(Some(scratch.0.join("different.txt"))).unwrap();
        worker
            .try_submit(DocumentRequest::Save { document: DocumentId(2), request, plan: Document::new().save_plan() })
            .unwrap();
        match receiver.recv_timeout(Duration::from_secs(5)).unwrap() {
            DocumentResult::Saved { result, .. } => {
                result.unwrap();
            }
            _ => panic!("expected addressed worker save completion"),
        }
        drop(worker);
        wait_stopped(&receiver);
    }

    #[test]
    fn dirty_named_recovery_requires_save_as_without_a_verified_original() {
        for disk_bytes in [b"external\r\n\xff".as_slice(), b"recovered\r\n\xff".as_slice()] {
            let scratch = Scratch::new();
            let path = scratch.0.join("named.txt");
            let tab = recovery_tab(Some(path.clone()));
            let recovered_bytes = b"recovered\r\n\xff";
            fixture_journal(&scratch.0, &tab, recovered_bytes);
            // Even matching current bytes do not prove the pre-crash saved baseline.
            std::fs::write(&path, disk_bytes).unwrap();
            let journal_path = scratch.0.join("1.kwj");
            let journal_bytes = std::fs::read(&journal_path).unwrap();
            let (document, _, base) = recover_document(journal_path.clone(), &tab).unwrap();
            assert_eq!(base.original_path.as_deref(), Some(path.as_path()), "keep original recovery context");
            assert!(document.is_dirty(), "recovery must preserve the unsaved marker");
            assert_eq!(document.revision(), tab.revision);
            assert_eq!(document.saved_revision(), tab.saved_revision);
            assert!(document.original.is_none(), "current disk is not a verified original");
            assert_eq!(document.line_ending, crate::DocumentLineEnding::CrLf);
            assert_eq!(document.buffer().add_memory_bytes(), 0, "recovery must remain spill-only");
            let mut actual = Vec::new();
            document.snapshot().write_to(&mut actual).unwrap();
            assert_eq!(actual, recovered_bytes);
            let mut editor = crate::Editor::new(document);
            assert!(editor.save_request(None).is_err(), "unverified named recovery must require Save As");
            assert!(editor.document().path().is_none());
            assert!(editor.document().is_dirty());
            assert_eq!(std::fs::read(&path).unwrap(), disk_bytes);
            assert_eq!(std::fs::read(&journal_path).unwrap(), journal_bytes);
            let destination = scratch.0.join("recovered.txt");
            let request = editor.save_request(Some(destination.clone())).unwrap();
            let saved = kwil_core_workspace::save_snapshot(request).unwrap();
            editor.finish_save_for_test(Ok(saved)).unwrap();
            assert!(!editor.document().is_dirty());
            assert_eq!(std::fs::read(destination).unwrap(), recovered_bytes);
            assert_eq!(std::fs::read(&path).unwrap(), disk_bytes);
            assert_eq!(std::fs::read(&journal_path).unwrap(), journal_bytes);
        }
    }

    #[test]
    fn missing_context_journal_never_adopts_the_current_named_disk_as_a_baseline() {
        let scratch = Scratch::new();
        let path = scratch.0.join("last-named.txt");
        std::fs::write(&path, b"foreign current bytes").unwrap();
        let mut session = Session::default();
        let id = session.open(None).unwrap();
        session.tab_mut(id).unwrap().recovery_path = Some(path.clone());
        session::store_session(&scratch.0.join("session.kws"), &session).unwrap();
        let (prepared, storage) = prepare_root(scratch.0.clone()).unwrap();
        assert!(prepared.documents[0].1.is_err(), "missing context journal must not become a clean empty document");
        assert_eq!(prepared.session.tabs()[0].recovery_path.as_ref(), Some(&path));
        assert!(prepared.session.tabs()[0].path.is_none());
        assert_eq!(std::fs::read(path).unwrap(), b"foreign current bytes");
        stop_storage(storage);
    }

    #[test]
    fn revision_zero_orphan_stays_dirty_through_install_save_all_and_close() {
        let scratch = Scratch::new();
        let mut tab = recovery_tab(None);
        tab.revision = 0;
        tab.saved_revision = 0;
        fixture_journal(&scratch.0, &tab, b"orphan\r\n\xff");
        let (prepared, storage) = prepare_root(scratch.0.clone()).unwrap();
        stop_storage(storage);
        let mut workspace = crate::Workspace::new(crate::View::new(None)).unwrap();
        workspace.test_document_result(DocumentResult::Prepared(Ok(prepared)));
        workspace.sync_active();
        workspace.sync_all();
        let document = workspace.editor().unwrap().document();
        assert!(document.is_dirty(), "revision-zero orphan must remain unsaved after installation");
        assert_eq!(document.revision(), 0);
        assert!(document.next_revision > document.saved_revision());
        assert_eq!(document.line_ending, crate::DocumentLineEnding::CrLf);
        assert_eq!(document.buffer().add_memory_bytes(), 0);
        let mut bytes = Vec::new();
        document.snapshot().write_to(&mut bytes).unwrap();
        assert_eq!(bytes, b"orphan\r\n\xff");
        let metadata = &workspace.session().tabs()[0];
        assert_ne!(metadata.revision, metadata.saved_revision);
        assert!(workspace.status_text().contains('*'), "orphan dirty marker must be visible");
        workspace.file_command("file.saveAll").unwrap();
        assert_eq!(workspace.file_prompt().unwrap().kind, crate::FilePromptKind::SaveAs);
        workspace.cancel_file_prompt();
        assert!(workspace.close(tab.id, false).is_err());
        workspace.request_window_close(false).unwrap();
        assert!(!workspace.wants_exit(), "guarded close must retain the orphan");
        workspace.cancel_file_prompt();
        workspace
            .execute("edit.insert", &crate::CommandArgs { text: Some("!".into()), ..Default::default() }, 1)
            .unwrap();
        workspace.execute("edit.undo", &Default::default(), 1).unwrap();
        assert!(workspace.editor().unwrap().document().is_dirty());
        workspace.execute("edit.redo", &Default::default(), 1).unwrap();
        assert!(workspace.editor().unwrap().document().is_dirty());
    }

    #[test]
    fn latest_save_as_context_survives_two_restarts_without_becoming_a_writable_backing() {
        use crate::{CommandArgs, View, Workspace};
        for originally_named in [false, true] {
            let scratch = Scratch::new();
            let original = scratch.0.join("original.txt");
            let latest = scratch.0.join("latest.txt");
            let initial = if originally_named {
                std::fs::write(&original, b"original\r\n\xff").unwrap();
                let file = kwil_core_workspace::open_for_editing(&original).unwrap();
                View::from_preview(original.clone(), file, b"original\r\n\\xFF".to_vec())
            } else {
                View::new(None)
            };
            let mut workspace = Workspace::new(initial).unwrap();
            workspace.set_session_restore(true);
            let (sender, receiver) = mpsc::channel();
            workspace
                .start_document_worker_at(Some(scratch.0.clone()), move |result| {
                    let _ = sender.send(result);
                })
                .unwrap();
            drive(&mut workspace, &receiver, Workspace::recovery_ready);
            drive(&mut workspace, &receiver, |workspace| workspace.editor().is_some());
            let id = workspace.active_id();
            workspace
                .execute("edit.insert", &CommandArgs { text: Some("seed\r\n".into()), ..Default::default() }, 1)
                .unwrap();
            workspace.save_document(id, Some(latest.clone())).unwrap();
            drive(&mut workspace, &receiver, |w| w.editor().is_some_and(|e| !e.document().is_dirty()));
            workspace
                .execute("edit.insert", &CommandArgs { text: Some("unsaved".into()), ..Default::default() }, 1)
                .unwrap();
            let mut expected = Vec::new();
            workspace.editor().unwrap().document().snapshot().write_to(&mut expected).unwrap();
            workspace.request_window_close(false).unwrap();
            drive(&mut workspace, &receiver, Workspace::wants_exit);
            drop(workspace);
            wait_stopped(&receiver);
            let disk_before = std::fs::read(&latest).unwrap();
            let journal_path = scratch.0.join(format!("{}.kwj", id.0));
            let mut base_path = None;
            let journal = RecoveredJournal::open(journal_path, id, &AtomicBool::new(false), |event| {
                match event {
                    RecoveryEvent::Base(base) | RecoveryEvent::EncodedBase { base, .. } => {
                        base_path = base.original_path.clone();
                    }
                    _ => {}
                }
                Ok(())
            })
            .unwrap();
            assert_eq!(base_path, originally_named.then_some(original.clone()));
            drop(journal);
            for _ in 0..2 {
                let (sender, receiver) = mpsc::channel();
                let mut restored = Workspace::new(View::new(None)).unwrap();
                restored.set_session_restore(true);
                restored
                    .start_document_worker_at(Some(scratch.0.clone()), move |result| {
                        let _ = sender.send(result);
                    })
                    .unwrap();
                drive(&mut restored, &receiver, Workspace::recovery_ready);
                restored.sync_active();
                restored.sync_all();
                assert!(restored.session().tabs()[0].path.is_none());
                assert_eq!(
                    restored.session().tabs()[0].recovery_path.as_deref(),
                    Some(latest.as_path()),
                    "last Save As context must survive recovery and metadata sync"
                );
                assert!(restored.path.is_none());
                let document = restored.editor().unwrap().document();
                assert!(document.path().is_none());
                assert!(document.original.is_none());
                assert!(document.is_dirty());
                let mut bytes = Vec::new();
                document.snapshot().write_to(&mut bytes).unwrap();
                assert_eq!(bytes, expected);
                assert!(restored.status_text().contains("Use Save As"));
                assert!(restored.tab_title(id).contains("latest.txt"));
                restored.file_command("file.saveAll").unwrap();
                assert_eq!(restored.file_prompt().unwrap().kind, crate::FilePromptKind::SaveAs);
                restored.cancel_file_prompt();
                assert!(restored.close(id, false).is_err());
                restored.request_window_close(false).unwrap();
                drive(&mut restored, &receiver, Workspace::wants_exit);
                drop(restored);
                wait_stopped(&receiver);
                let persisted = session::load_session(&scratch.0.join("session.kws")).unwrap();
                assert!(persisted.tabs()[0].path.is_none());
                assert_eq!(persisted.tabs()[0].recovery_path.as_deref(), Some(latest.as_path()));
                assert_eq!(std::fs::read(&latest).unwrap(), disk_before);
                if originally_named {
                    assert_eq!(std::fs::read(&original).unwrap(), b"original\r\n\xff");
                }
            }
        }
    }

    #[test]
    fn orphan_named_recovery_retains_path_context_but_requires_save_as() {
        let scratch = Scratch::new();
        let path = scratch.0.join("named.txt");
        let tab = recovery_tab(Some(path.clone()));
        fixture_journal(&scratch.0, &tab, b"recovered\r\n\xff");
        std::fs::write(&path, b"external bytes").unwrap();
        let (prepared, storage) = prepare_root(scratch.0.clone()).unwrap();
        assert!(prepared.session.tabs()[0].path.is_none());
        assert_eq!(prepared.session.tabs()[0].recovery_path.as_deref(), Some(path.as_path()));
        let document = prepared.documents.into_iter().next().unwrap().1.unwrap();
        assert!(document.is_dirty());
        let mut editor = crate::Editor::new(document);
        assert!(editor.save_request(None).is_err(), "orphan recovery must also require Save As");
        let mut bytes = Vec::new();
        editor.document().snapshot().write_to(&mut bytes).unwrap();
        assert_eq!(bytes, b"recovered\r\n\xff");
        assert_eq!(std::fs::read(&path).unwrap(), b"external bytes");
        stop_storage(storage);
    }

    #[test]
    fn clean_named_recovery_conflicts_preserve_old_bytes_without_overwriting_changed_disk() {
        let scratch = Scratch::new();
        let path = scratch.0.join("named.txt");
        let mut tab = recovery_tab(Some(path.clone()));
        tab.saved_revision = tab.revision;
        fixture_journal(&scratch.0, &tab, b"old bytes");
        std::fs::write(&path, b"new bytes").unwrap();
        let (document, _, _) = recover_document(scratch.0.join("1.kwj"), &tab).unwrap();
        assert!(document.is_dirty(), "changed disk cannot make stale recovered bytes clean");
        assert!(document.path().is_none(), "conflict must require Save As");
        let mut actual = Vec::new();
        document.snapshot().write_to(&mut actual).unwrap();
        assert_eq!(actual, b"old bytes");
        assert_eq!(std::fs::read(&path).unwrap(), b"new bytes");
    }

    #[test]
    fn legacy_clean_recovery_refuses_unsafe_encoding_before_editor_exposure_and_retains_journal() {
        for (label, original) in [
            ("bomless-utf16le", b"A\0B\0C\0D\0".as_slice()),
            ("bomless-utf16be", b"\0A\0B\0C\0D".as_slice()),
            ("bom-utf16le", b"\xff\xfeA\0B\0".as_slice()),
            ("bom-utf16be", b"\xfe\xff\0A\0B".as_slice()),
            ("unsupported-utf32", b"\xff\xfe\0\0A\0\0\0".as_slice()),
            ("ambiguous-nul", b"a\0b".as_slice()),
        ] {
            let scratch = Scratch::new();
            let path = scratch.0.join(format!("{label}.txt"));
            let mut tab = recovery_tab(Some(path.clone()));
            tab.saved_revision = tab.revision;
            fixture_journal(&scratch.0, &tab, original);
            std::fs::write(&path, original).unwrap();
            let journal_path = scratch.0.join("1.kwj");
            let journal_before = std::fs::read(&journal_path).unwrap();
            let error = match recover_document(journal_path.clone(), &tab) {
                Err(error) => error,
                Ok(_) => panic!("legacy clean {label} must be refused before exposing an editor/save request"),
            };
            assert_eq!(error.kind(), io::ErrorKind::Unsupported, "{label}: {error}");
            assert!(error.to_string().contains("journal retained"));
            assert_eq!(std::fs::read(&path).unwrap(), original, "{label}: original bytes must not change");
            assert_eq!(std::fs::read(&journal_path).unwrap(), journal_before, "{label}: journal must not change");
            let mut replayed = Vec::new();
            let recovered = RecoveredJournal::open(journal_path, tab.id, &AtomicBool::new(false), |event| {
                if let RecoveryEvent::BaseBytes(bytes) = event {
                    replayed.extend_from_slice(bytes);
                }
                Ok(())
            })
            .unwrap();
            assert_eq!(recovered.summary().revision, tab.revision);
            assert_eq!(replayed, original, "{label}: refused journal must remain recoverable");
        }
    }

    #[test]
    fn legacy_clean_recovery_keeps_verified_utf8_edit_save_and_journal_bytes() {
        let scratch = Scratch::new();
        let path = scratch.0.join("utf8.txt");
        let mut tab = recovery_tab(Some(path.clone()));
        tab.saved_revision = tab.revision;
        let original = "plain 🌿\r\n".as_bytes();
        fixture_journal(&scratch.0, &tab, original);
        std::fs::write(&path, original).unwrap();
        let journal_path = scratch.0.join("1.kwj");
        let journal_before = std::fs::read(&journal_path).unwrap();
        let (document, recovered, _) = recover_document(journal_path.clone(), &tab).unwrap();
        assert!(!document.is_dirty());
        assert_eq!(document.path(), Some(path.as_path()));
        assert!(document.original.is_some());
        let mut editor = crate::Editor::new(document);
        editor.execute("edit.insert", &crate::CommandArgs { text: Some("x".into()), ..Default::default() }, 1).unwrap();
        let saved = kwil_core_workspace::save_snapshot(editor.save_request(None).unwrap()).unwrap();
        editor.finish_save_for_test(Ok(saved)).unwrap();
        assert!(!editor.document().is_dirty());
        assert_eq!(std::fs::read(&path).unwrap(), [b"x".as_slice(), original].concat());
        assert_eq!(std::fs::read(&journal_path).unwrap(), journal_before);
        drop((editor, recovered));
    }

    #[test]
    fn unchanged_clean_named_recovery_keeps_original_path_and_clean_state() {
        let scratch = Scratch::new();
        let path = scratch.0.join("named.txt");
        let mut tab = recovery_tab(Some(path.clone()));
        tab.saved_revision = tab.revision;
        fixture_journal(&scratch.0, &tab, b"same\r\n");
        std::fs::write(&path, b"same\r\n").unwrap();
        let (document, _, _) = recover_document(scratch.0.join("1.kwj"), &tab).unwrap();
        assert!(!document.is_dirty());
        assert_eq!(document.path(), Some(path.as_path()));
        assert_eq!(document.line_ending, crate::DocumentLineEnding::CrLf);
        assert!(document.original.is_some(), "clean restored disk state must retain external-change checks");
    }

    #[test]
    fn recovered_crlf_controls_newline_insertion() {
        let scratch = Scratch::new();
        let tab = recovery_tab(None);
        fixture_journal(&scratch.0, &tab, b"one\r\ntwo\r\n");
        let (document, _, _) = recover_document(scratch.0.join("1.kwj"), &tab).unwrap();
        let mut editor = crate::Editor::new(document);
        editor.execute("edit.newline", &Default::default(), 1).unwrap();
        let mut actual = Vec::new();
        editor.document().snapshot().write_to(&mut actual).unwrap();
        assert_eq!(actual, b"\r\none\r\ntwo\r\n");
    }

    #[test]
    fn durable_closed_eviction_does_not_return_as_orphan_and_marked_explicit_reopen_survives() {
        let scratch = Scratch::new();
        let mut prior = Session::default();
        let id = prior.open(None).unwrap();
        fixture_journal(&scratch.0, &prior.tabs()[0], b"retained");
        assert!(prior.close(id));
        session::store_session(&scratch.0.join("session.kws"), &prior).unwrap();
        let (_, mut storage) = prepare_root(scratch.0.clone()).unwrap();
        let mut next = prior.clone();
        for _ in 0..session::MAX_CLOSED {
            let id = next.open(None).unwrap();
            assert!(next.close(id));
        }
        // Simulate a crash between marker fsync and the metadata replacement.
        retire_evicted(&storage, &next).unwrap();
        stop_storage(storage);
        let (prepared, state) = prepare_root(scratch.0.clone()).unwrap();
        assert!(prepared.session.closed().iter().any(|tab| tab.id == id));
        stop_storage(state);
        let mut reopened = prior.clone();
        assert_eq!(reopened.reopen_closed().unwrap(), Some(id));
        session::store_session(&scratch.0.join("session.kws"), &reopened).unwrap();
        let (prepared, state) = prepare_root(scratch.0.clone()).unwrap();
        assert!(prepared.documents.iter().any(|(document, result)| *document == id && result.is_ok()));
        stop_storage(state);
        session::store_session(&scratch.0.join("session.kws"), &prior).unwrap();
        let (_, state) = prepare_root(scratch.0.clone()).unwrap();
        storage = state;
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(scratch.0.join("session.session-lock"))
            .unwrap();
        lock.try_lock().unwrap();
        assert!(
            persist_session(&mut storage, &next).is_err(),
            "metadata failure after marker must not acknowledge durability"
        );
        assert_eq!(storage.durable_session, prior);
        assert_eq!(session::load_session(&scratch.0.join("session.kws")).unwrap(), prior);
        drop(lock);
        persist_session(&mut storage, &next).unwrap();
        stop_storage(storage);
        let (prepared, state) = prepare_root(scratch.0.clone()).unwrap();
        assert!(!prepared.session.tabs().iter().any(|tab| tab.id == id));
        assert_eq!(prepared.session.closed().len(), session::MAX_CLOSED);
        assert!(scratch.0.join("1.kwj").is_file(), "retirement never deletes journal data");
        stop_storage(state);
    }

    #[test]
    fn recovery_streams_invalid_bytes_and_adopts_orphan_identity() {
        let scratch = Scratch::new();
        let mut buffer = Buffer::empty();
        buffer.insert(0, b"\xffbefore\r\n").unwrap();
        let snapshot = buffer.snapshot();
        let mut service = JournalService::spawn(Duration::from_secs(1)).unwrap();
        let id = DocumentId(7);
        let handle = service
            .try_create(JournalSeed {
                path: scratch.0.join("7.kwj"),
                base: JournalBase {
                    document: id,
                    revision: 3,
                    original_path: None,
                    original_identity: [0; 4],
                    codec: b"raw-v1".to_vec(),
                    byte_len: snapshot.len(),
                },
                source: Box::new(SnapshotReader::new(snapshot)),
            })
            .unwrap();
        assert_eq!(handle.ready.recv_timeout(Duration::from_secs(5)).unwrap().unwrap(), 3);
        handle
            .try_submit(JournalCommand::Begin(JournalEdit {
                from_revision: 3,
                to_revision: 4,
                offset: 1,
                removed: 6,
                inserted: 3,
            }))
            .unwrap();
        handle.try_submit(JournalCommand::Bytes(b"new".to_vec())).unwrap();
        handle.try_submit(JournalCommand::Commit).unwrap();
        assert_eq!(handle.try_flush().unwrap().recv_timeout(Duration::from_secs(5)).unwrap().unwrap(), 4);
        service.finish();
        service.finished.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
        drop(handle);
        drop(service);

        let (mut prepared, mut storage) = prepare(Some(scratch.0.clone())).unwrap();
        assert_eq!(prepared.session.tabs()[0].id, id);
        let (_, document) = prepared.documents.pop().unwrap();
        let document = document.unwrap();
        let mut bytes = Vec::new();
        document.snapshot().write_to(&mut bytes).unwrap();
        assert_eq!(bytes, b"\xffnew\r\n");
        assert!(document.is_dirty());
        assert_eq!(document.buffer().add_memory_bytes(), 0, "recovered bytes spill instead of accumulating in RAM");
        assert!(prepared.session.open(None).unwrap().0 > id.0);
        storage.service.finish();
        storage.service.finished.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
        drop(document);
        drop(storage);
    }

    #[test]
    fn recovered_raw_and_encoded_spills_remain_worker_owned_after_frontend_drop() {
        use std::sync::Arc;
        for encoded in [false, true] {
            let root = Scratch::new();
            let tab = recovery_tab(None);
            let path = root.0.join(format!("{}.kwj", tab.id.0));
            if encoded {
                let mut buffer = Buffer::empty();
                buffer.insert(0, b"recovered capsule").unwrap();
                let registry = provenance::OriginRegistry::new(8, 1 << 20);
                let plan = registry.plan(buffer.snapshot(), 0..buffer.len(), Arc::new(AtomicBool::new(false))).unwrap();
                let base = JournalBase {
                    document: tab.id,
                    revision: tab.revision,
                    original_path: None,
                    original_identity: [0; 4],
                    codec: encoding_runtime::document_policy(&Document::new()).to_codec().unwrap(),
                    byte_len: plan.physical_len(),
                }
                .with_logical_len(plan.logical_len())
                .unwrap();
                let mut service = JournalService::spawn(Duration::from_secs(1)).unwrap();
                let handle = service
                    .try_create_encoded(kwil_core_workspace::journal::EncodedJournalSeed {
                        path: path.clone(),
                        base,
                        source: Box::new(registry.start(plan).unwrap()),
                    })
                    .unwrap();
                handle.ready.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
                service.finish();
                service.finished.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
            } else {
                fixture_journal(&root.0, &tab, b"legacy recovered");
            }
            let mut runtime = encoding_runtime::EncodingRuntime::new(
                root.0.clone(),
                retirement::BackingLimits {
                    max_backings: 8,
                    max_private_path_bytes: session::MAX_PATH_BYTES,
                    max_metadata_bytes: 256 * 1024 * 1024,
                },
            )
            .unwrap();
            // A zero resident budget forces real spills with a tiny fixture.
            let (document, journal, _, origins) =
                recover_prepared_with_add_limit(&root.0, &mut runtime, path, &tab, 0).unwrap();
            let blocks = runtime.retained_spill_blocks();
            assert!(blocks > 0, "every recovered spill must be leased before publication: encoded={encoded}");
            assert!(runtime.retire().live >= blocks);
            let snapshot = document.snapshot();
            std::thread::spawn(move || drop((document, snapshot, origins))).join().unwrap();
            assert_eq!(runtime.retained_spill_blocks(), blocks, "UI drop must leave the worker lease intact");
            drop(journal);
            let report = runtime.retire();
            assert_eq!(report.cleanup_pending, 0);
            assert_eq!(runtime.retained_spill_blocks(), 0);
            assert!(report.retired >= blocks);
        }
    }

    #[test]
    fn incomplete_seed_is_not_ready_and_recovery_keeps_its_file() {
        let scratch = Scratch::new();
        let path = scratch.0.join("9.kwj");
        let mut service = JournalService::spawn(Duration::from_secs(1)).unwrap();
        let handle = service
            .try_create(JournalSeed {
                path: path.clone(),
                base: JournalBase {
                    document: DocumentId(9),
                    revision: 1,
                    original_path: None,
                    original_identity: [0; 4],
                    codec: vec![],
                    byte_len: 12,
                },
                source: Box::new(io::empty()),
            })
            .unwrap();
        assert!(handle.ready.recv_timeout(Duration::from_secs(5)).unwrap().is_err());
        service.finish();
        let _ = service.finished.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(service);
        drop(handle);
        let before = std::fs::read(&path).unwrap();
        let (prepared, storage) = prepare(Some(scratch.0.clone())).unwrap();
        assert_eq!(prepared.documents.len(), 1);
        assert_eq!(prepared.documents[0].0, DocumentId(9));
        assert!(prepared.documents[0].1.is_err(), "an incomplete seed remains an addressed recovery error");
        drop(prepared);
        stop_storage(storage);
        assert_eq!(std::fs::read(path).unwrap(), before);
    }

    #[test]
    fn recovery_publishes_all_batch_replacements_or_the_previous_revision() {
        for commit in [false, true] {
            let scratch = Scratch::new();
            let mut buffer = Buffer::empty();
            buffer.insert(0, b"abcdef").unwrap();
            let snapshot = buffer.snapshot();
            let mut service = JournalService::spawn(Duration::from_secs(1)).unwrap();
            let handle = service
                .try_create(JournalSeed {
                    path: scratch.0.join("11.kwj"),
                    base: JournalBase {
                        document: DocumentId(11),
                        revision: 3,
                        original_path: None,
                        original_identity: [0; 4],
                        codec: b"raw-v1".to_vec(),
                        byte_len: snapshot.len(),
                    },
                    source: Box::new(SnapshotReader::new(snapshot)),
                })
                .unwrap();
            handle.ready.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
            for command in [
                JournalCommand::BeginBatch(JournalBatch { from_revision: 3, to_revision: 4, replacements: 2 }),
                JournalCommand::BeginReplacement(JournalReplacement { offset: 1, removed: 2, inserted: 2 }),
                JournalCommand::Bytes(b"XY".to_vec()),
                JournalCommand::EndReplacement,
                JournalCommand::BeginReplacement(JournalReplacement { offset: 4, removed: 1, inserted: 1 }),
                JournalCommand::Bytes(b"Z".to_vec()),
                JournalCommand::EndReplacement,
            ] {
                handle.try_submit(command).unwrap();
            }
            if commit {
                handle.try_submit(JournalCommand::CommitBatch).unwrap();
            }
            service.finish();
            let _ = service.finished.recv_timeout(Duration::from_secs(5)).unwrap();
            drop(handle);
            drop(service);
            let (mut prepared, mut storage) = prepare(Some(scratch.0.clone())).unwrap();
            let (_, document) = prepared.documents.pop().unwrap();
            let document = document.unwrap();
            let mut bytes = Vec::new();
            document.snapshot().write_to(&mut bytes).unwrap();
            assert_eq!(bytes, if commit { b"aXYdZf" } else { b"abcdef" });
            assert_eq!(document.revision(), if commit { 4 } else { 3 });
            storage.service.finish();
            storage.service.finished.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
            drop(document);
            drop(storage);
        }
    }

    #[test]
    fn malformed_recovery_override_never_uses_default_storage() {
        assert!(prepare(Some("relative-recovery-directory".into())).is_err());
        assert!(resolve_root(None, Some("".into())).is_err());
        assert!(resolve_root(None, Some("relative".into())).is_err());
        assert!(
            resolve_root(Some(PathBuf::new()), None).is_err(),
            "missing required isolated root must not use the profile"
        );
    }

    #[test]
    fn external_open_is_bounded_retains_rejected_requests_and_coalesces_wakes() {
        use kwil_core_workspace::instance::{OpenFile, OpenFiles};
        let wakes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = wakes.clone();
        let (open, inbox) = external_open_channel(move || {
            count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        });
        assert!(!inbox.has_pending());
        let path = std::env::temp_dir().join("kwil-virtual-日本語.txt");
        let request = OpenFiles { files: vec![OpenFile { path, line: Some(3), column: Some(5) }] };
        for _ in 0..16 {
            open(request.clone()).unwrap();
        }
        let Err(TrySendError::Full(rejected)) = open(request.clone()) else {
            panic!("seventeenth request must retain ownership on Full")
        };
        assert_eq!(rejected, request);
        assert_eq!(wakes.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert!(inbox.has_pending(), "modal deferral must leave admitted work discoverable");
        let accepted = inbox.drain();
        assert!(!inbox.has_pending());
        assert_eq!(accepted.len(), 16);
        assert!(accepted.iter().all(|r| r == &request));
        open(rejected).unwrap();
        assert_eq!(wakes.load(std::sync::atomic::Ordering::Relaxed), 2);
        assert_eq!(inbox.drain(), vec![request.clone()]);
        let malformed = OpenFiles { files: vec![OpenFile { path: "relative.txt".into(), line: None, column: None }] };
        assert!(matches!(open(malformed.clone()), Err(TrySendError::Disconnected(r)) if r == malformed));
        drop(inbox);
        assert!(matches!(open(request.clone()), Err(TrySendError::Disconnected(r)) if r == request));
    }

    #[test]
    fn external_open_close_rejects_whole_native_request_without_wake_and_retains_admitted() {
        use kwil_core_workspace::instance::{OpenFile, OpenFiles};
        let wakes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = wakes.clone();
        let (open, inbox) = external_open_channel(move || {
            count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        });
        let first_path = std::env::temp_dir().join("kwil-ipc-閉じる-日本語.txt");
        #[cfg(windows)]
        let second_path = {
            use std::os::windows::ffi::OsStringExt;
            std::env::temp_dir().join(std::ffi::OsString::from_wide(&[0x65e5, 0xd800, 46, 116, 120, 116]))
        };
        #[cfg(not(windows))]
        let second_path = std::env::temp_dir().join("kwil-ipc-第二.txt");
        let request = OpenFiles {
            files: vec![
                OpenFile { path: first_path, line: Some(7), column: Some(3) },
                OpenFile { path: second_path, line: Some(11), column: Some(5) },
            ],
        };
        open(request.clone()).unwrap();
        open(request.clone()).unwrap();
        assert_eq!(wakes.load(std::sync::atomic::Ordering::Relaxed), 1);
        inbox.close();
        inbox.close();
        assert!(matches!(open(request.clone()), Err(TrySendError::Disconnected(rejected)) if rejected == request));
        assert_eq!(wakes.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(inbox.drain(), vec![request.clone(), request.clone()]);
        assert!(matches!(open(request.clone()), Err(TrySendError::Disconnected(rejected)) if rejected == request));
        assert!(inbox.drain().is_empty());
        assert_eq!(wakes.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[test]
    fn external_open_close_during_admitted_wake_keeps_bounded_ownership() {
        use kwil_core_workspace::instance::{OpenFile, OpenFiles};
        let (entered, entered_rx) = mpsc::sync_channel(1);
        let resume = std::sync::Arc::new(AtomicBool::new(false));
        let wake_resume = resume.clone();
        let wakes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = wakes.clone();
        let (open, inbox) = external_open_channel(move || {
            count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            entered.try_send(()).unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while !wake_resume.load(std::sync::atomic::Ordering::Acquire) {
                // Mock a waiting wake without a mutex or busy loop; tolerate
                // spurious unparks under one fixed deadline, never UI polling.
                let wait =
                    deadline.checked_duration_since(std::time::Instant::now()).expect("mock wake release deadline");
                std::thread::park_timeout(wait);
            }
            Ok(())
        });
        let request = OpenFiles {
            files: vec![OpenFile {
                path: std::env::temp_dir().join("kwil-ipc-racing-close-日本語.txt"),
                line: Some(2),
                column: Some(4),
            }],
        };
        let admitted = request.clone();
        let producer = std::thread::spawn(move || {
            let result = open(admitted);
            (open, result)
        });
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        // The callback is still waiting: close must not wait for its producer.
        inbox.close();
        resume.store(true, std::sync::atomic::Ordering::Release);
        producer.thread().unpark();
        let (open, result) = producer.join().unwrap();
        result.unwrap();
        assert!(matches!(open(request.clone()), Err(TrySendError::Disconnected(rejected)) if rejected == request));
        assert_eq!(inbox.drain(), vec![request]);
        assert_eq!(wakes.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[test]
    fn observer_full_retains_the_successful_edit_and_gates_only_later_mutations() {
        let (tx, rx) = mpsc::sync_channel(16);
        let capacity = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(1));
        let worker = DocumentWorker {
            tx: result_transport::RequestSender::new(tx, None),
            journal_capacity: capacity.clone(),
            journal_controls: std::sync::Arc::new(JournalControlCredits::new()),
            save_destinations: Default::default(),
        };
        let mut editor = crate::Editor::new(Document::new());
        editor
            .set_edit_observer(Some(worker.observer(DocumentId(1), None, 1, 1, document_policy(editor.document()))))
            .unwrap();
        let args = |text: &str| crate::CommandArgs { text: Some(text.into()), ..Default::default() };
        editor.execute("edit.insert", &args("x"), 1).unwrap();
        editor.execute("edit.insert", &args("y"), 1).unwrap();
        assert!(editor.has_pending_edit_receipt());
        assert_eq!(editor.document().snapshot().len(), 2, "the second edit already succeeded");
        assert_eq!(editor.execute("edit.insert", &args("blocked"), 1).unwrap_err().kind(), io::ErrorKind::WouldBlock);
        let DocumentRequest::Journal { receipt, .. } = rx.recv_timeout(Duration::from_secs(1)).unwrap() else {
            panic!("expected receipt")
        };
        assert_eq!(receipt.sequence, 1);
        capacity.fetch_add(1, std::sync::atomic::Ordering::Release);
        editor.flush_edit_receipt().unwrap();
        assert!(!editor.has_pending_edit_receipt());
        let DocumentRequest::Journal { receipt, .. } = rx.recv_timeout(Duration::from_secs(1)).unwrap() else {
            panic!("expected retained receipt")
        };
        assert_eq!(receipt.sequence, 2);
        let mut bytes = Vec::new();
        receipt.after.write_to(&mut bytes).unwrap();
        assert_eq!(bytes, b"xy", "flushing must not retry the successful input");
    }

    pub(super) fn drive(
        workspace: &mut crate::Workspace,
        receiver: &mpsc::Receiver<DocumentResult>,
        done: impl Fn(&crate::Workspace) -> bool,
    ) {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while !done(workspace) {
            let wait = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!wait.is_zero(), "workspace condition timed out: {}", workspace.status_text());
            let result = receiver.recv_timeout(wait).expect("worker result before deadline");
            workspace.test_document_result(result);
        }
    }

    pub(super) fn wait_stopped(receiver: &mpsc::Receiver<DocumentResult>) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let DocumentResult::Stopped(result) =
                receiver.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())).unwrap()
            {
                result.unwrap();
                break;
            }
        }
    }

    #[test]
    fn production_utf16_opaque_journal_survives_two_restarts_and_save_as() {
        use crate::{CommandArgs, View, Workspace};
        let root = Scratch::new();
        let path = root.0.join("malformed-utf16.txt");
        let original = b"\xff\xfeA\0\0\xd8";
        std::fs::write(&path, original).unwrap();
        let mut id = None;
        for round in 0..3 {
            let (tx, rx) = mpsc::channel();
            let mut workspace = Workspace::new(View::new(None)).unwrap();
            workspace.set_session_restore(true);
            workspace
                .start_document_worker_at(Some(root.0.clone()), move |result| {
                    let _ = tx.send(result);
                })
                .unwrap();
            drive(&mut workspace, &rx, Workspace::recovery_ready);
            let document = if round == 0 {
                let opened = workspace.open_path(path.clone()).unwrap();
                drive(&mut workspace, &rx, |workspace| {
                    workspace.view(opened).is_some_and(|view| view.editor().is_some())
                });
                id = Some(opened);
                opened
            } else {
                id.unwrap()
            };
            workspace.activate(document);
            assert_eq!(
                workspace.view(document).unwrap().editor().unwrap().document().save_plan().encoding,
                kwil_core_text::encoding::Encoding::UTF16_LE
            );
            if round < 2 {
                workspace.execute("motion.lastLine", &CommandArgs::default(), 1).unwrap();
                workspace
                    .execute(
                        "edit.insert",
                        &CommandArgs { text: Some(if round == 0 { "x" } else { "y" }.into()), ..Default::default() },
                        2,
                    )
                    .unwrap();
                workspace.request_window_close(false).unwrap();
                drive(&mut workspace, &rx, Workspace::wants_exit);
            } else {
                let editor = workspace.view(document).unwrap().editor().unwrap();
                assert!(editor.document().path().is_none(), "dirty recovery cannot adopt current disk baseline");
                let mut logical = Vec::new();
                editor.document().snapshot().write_to(&mut logical).unwrap();
                assert_eq!(logical, b"A\xffxy");
                let output = root.0.join("saved-as.txt");
                workspace.save_document(document, Some(output.clone())).unwrap();
                drive(&mut workspace, &rx, |workspace| {
                    !workspace.view(document).unwrap().editor().unwrap().document().is_dirty()
                });
                assert_eq!(std::fs::read(output).unwrap(), b"\xff\xfeA\0\0\xd8x\0y\0");
            }
            drop(workspace);
            wait_stopped(&rx);
            assert_eq!(std::fs::read(&path).unwrap(), original, "recovery and Save As preserve the original");
        }
    }

    #[test]
    fn normal_hot_exit_restores_order_active_paths_caret_viewport_layout_and_unsaved_bytes() {
        use crate::{CommandArgs, TabMetrics, View, Workspace};
        use kwil_core_commands::settings::{Settings, TabLayout, write_settings_atomic};
        let scratch = Scratch::new();
        let settings_path = scratch.0.join("settings.toml");
        write_settings_atomic(&settings_path, None, "[tabs]\nlayout='vertical'\n[files]\nrestore_session=true\n")
            .unwrap();
        let settings = Settings::load(&settings_path).unwrap().settings;
        let named = scratch.0.join("named.txt");
        std::fs::write(&named, b"one\ntwo\nthree\nfour\nfive\nsix\n").unwrap();
        let (sender, receiver) = mpsc::channel();
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        workspace.set_rows(2);
        workspace
            .execute(
                "edit.insert",
                &CommandArgs {
                    text: Some("unsaved🌿\nline2\nline3\nline4\nline5\nline6".into()),
                    ..Default::default()
                },
                1,
            )
            .unwrap();
        workspace.set_session_restore(settings.files.restore_session);
        workspace
            .start_document_worker_at(Some(scratch.0.clone()), move |result| {
                let _ = sender.send(result);
            })
            .unwrap();
        drive(&mut workspace, &receiver, crate::Workspace::recovery_ready);
        let first = workspace.active_id();
        workspace.set_horizontal_window(2, 64 << 10);
        let second = workspace.open_path(named.clone()).unwrap();
        drive(&mut workspace, &receiver, |workspace| {
            workspace.view(second).is_some_and(|view| view.editor().is_some())
        });
        workspace.set_rows(2);
        workspace
            .execute("navigation.goToLine", &CommandArgs { line: Some(3), column: Some(2), ..Default::default() }, 1)
            .unwrap();
        workspace.execute("edit.insert", &CommandArgs { text: Some("X".into()), ..Default::default() }, 1).unwrap();
        workspace.execute("edit.undo", &CommandArgs::default(), 1).unwrap();
        workspace.execute("edit.redo", &CommandArgs::default(), 1).unwrap();
        workspace.activate(first);
        workspace.set_rows(2);
        workspace
            .execute("navigation.goToLine", &CommandArgs { line: Some(4), column: Some(2), ..Default::default() }, 1)
            .unwrap();
        workspace.sync_all();
        let expected = workspace.session().clone();
        let expected_horizontal: HashMap<_, _> =
            expected.tabs().iter().map(|tab| (tab.id, workspace.view(tab.id).unwrap().first_byte_column)).collect();
        let expected_bytes: HashMap<_, _> = expected
            .tabs()
            .iter()
            .map(|tab| {
                let mut bytes = Vec::new();
                workspace.view(tab.id).unwrap().editor().unwrap().document().snapshot().write_to(&mut bytes).unwrap();
                (tab.id, bytes)
            })
            .collect();
        workspace.request_window_close(false).unwrap();
        assert!(!workspace.wants_exit(), "a pending seed/metadata receipt is not durable hot exit");
        assert_eq!(workspace.session().tabs().len(), 2, "normal exit must not close tabs into history");
        drive(&mut workspace, &receiver, Workspace::wants_exit);
        assert_eq!(workspace.session().active(), expected.active());
        assert_eq!(workspace.session().tabs(), expected.tabs());
        drop(workspace);
        wait_stopped(&receiver);
        let persisted = session::load_session(&scratch.0.join("session.kws")).unwrap();
        assert_eq!(persisted.tabs(), expected.tabs(), "persisted metadata keeps the original path context");
        let mut recovered_tabs = expected.tabs().to_vec();
        for tab in &mut recovered_tabs {
            tab.recovery_path = tab.path.take();
        }

        let (sender, receiver) = mpsc::channel();
        let mut restarted = Workspace::new(View::new(None)).unwrap();
        let reloaded_settings = Settings::load(&settings_path).unwrap().settings;
        assert_eq!(reloaded_settings.tabs.layout, TabLayout::Vertical);
        restarted.set_session_restore(reloaded_settings.files.restore_session);
        restarted
            .start_document_worker_at(Some(scratch.0.clone()), move |result| {
                let _ = sender.send(result);
            })
            .unwrap();
        drive(&mut restarted, &receiver, crate::Workspace::recovery_ready);
        restarted.sync_all();
        assert_eq!(restarted.session().active(), expected.active());
        assert_eq!(restarted.session().tabs(), recovered_tabs, "last named recovery context must survive sync");
        assert!(restarted.view(second).unwrap().status_text().contains("Use Save As"));
        assert!(restarted.view(second).unwrap().path.is_none(), "context must not become a writable backing");
        for tab in expected.tabs() {
            assert_eq!(
                restarted.view(tab.id).unwrap().first_byte_column,
                expected_horizontal[&tab.id],
                "requested horizontal byte origin must survive recovery"
            );
            let document = restarted.view(tab.id).unwrap().editor().unwrap().document();
            let mut bytes = Vec::new();
            document.snapshot().write_to(&mut bytes).unwrap();
            assert_eq!(bytes, expected_bytes[&tab.id]);
            assert!(document.is_dirty());
        }
        let geometry = restarted.tab_geometry(
            TabMetrics { width: 800, height: 400, cell_width: 8, cell_height: 16, scrollbar: 14 },
            reloaded_settings.tabs.layout,
        );
        assert!(geometry.body.x > 0);
        assert_eq!(geometry.body.y, 0);
        drop(restarted);
        wait_stopped(&receiver);
    }
}
