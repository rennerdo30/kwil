//! Memory-only registration controller over the existing durable association
//! worker. Start after first paint. No native adapter is chosen here. Never
//! call `worker_ready` on a timer: address it only from the worker wake event.
//!
//! The settings page uses one action, `apply`: it makes the OS match the checked
//! types. Internally that is Plan -> Apply, or Undo -> Plan -> Apply when a
//! registration exists (owner, 2026-10-29, KW-72: the old review/skip flow and
//! "unregister before you may change anything" made no sense). A failure between
//! Undo and the new Apply leaves no Kwil registration and the user's previous
//! handlers restored: a safe state, reported plainly. `remove_all` is Undo.
//! The palette's current-file flow keeps preview -> register_selected.
//! Dropping or stopping retires the worker, snapshots and receipts off the UI.
use kwil_core_commands::{
    associations::{
        AssociationAdapter, AssociationError, AssociationPlatform, AssociationRequest, DefaultChoice, FileType,
        Location,
        worker::{
            AssociationWorker, CurrentState, Event, Events, PlanToken, Request, RequestId, Response, Status,
            SubmitError,
        },
    },
    file_types::{Extension, ExtensionError, Selection, current_extension},
};
use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, mpsc},
    thread::{self, JoinHandle},
    time::Instant,
};

pub const MAX_NATIVE_PATH_UNITS: usize = 4096;
/// Count lifts are ROOT proposals, not runtime capability detection. Keep Base128
/// until ROOT adopts and verifies 256 types / 768 receipt changes AND read budgets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegistrationLimits {
    Base128,
    RootAdopted256,
}
impl RegistrationLimits {
    fn types(self) -> usize {
        match self {
            Self::Base128 => 128,
            Self::RootAdopted256 => 256,
        }
    }
}

/// Construct off the UI from resolved host context, never by reading environment
/// or filesystem here. MIME/UTI values must already be verified on that backend.
pub struct RegistrationContext {
    pub platform: AssociationPlatform,
    pub executable: PathBuf,
    pub content_types: BTreeMap<Extension, String>,
    pub default_choice: DefaultChoice,
    pub limits: RegistrationLimits,
}
impl RegistrationContext {
    fn validate(&self) -> io::Result<()> {
        let bounded = |path: &Path| path.is_absolute() && path.as_os_str().len() <= MAX_NATIVE_PATH_UNITS;
        if !bounded(&self.executable)
            || self.content_types.len() > 256
            || self.content_types.values().any(|value| value.is_empty() || value.len() > 255)
        {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "unbounded registration context"));
        }
        match &self.platform {
            AssociationPlatform::Linux { data_home, config_home } if !bounded(data_home) || !bounded(config_home) => {
                Err(io::Error::new(io::ErrorKind::InvalidInput, "unbounded XDG context"))
            }
            AssociationPlatform::MacOS { bundle_id } if bundle_id.len() > 255 => {
                Err(io::Error::new(io::ErrorKind::InvalidInput, "unbounded bundle identity"))
            }
            _ => Ok(()),
        }
    }
}

/// The host supplies its actual tab/document incarnation, not a title, revision,
/// or list position that can be reused after closing a tab. Save As changes path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DocumentIdentity {
    pub tab: u64,
    pub incarnation: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CurrentFile {
    identity: DocumentIdentity,
    path: PathBuf,
    extension: Extension,
}
impl CurrentFile {
    pub fn capture(identity: DocumentIdentity, path: &Path) -> Result<Self, ActionErrorKind> {
        if !path.is_absolute() || path.as_os_str().len() > MAX_NATIVE_PATH_UNITS {
            return Err(ActionErrorKind::InvalidNativePath);
        }
        let extension = current_extension(path).map_err(ActionErrorKind::Extension)?;
        Ok(Self { identity, path: path.to_owned(), extension })
    }
    pub fn extension(&self) -> &Extension {
        &self.extension
    }
    pub fn native_path(&self) -> &Path {
        &self.path
    }
    pub fn identity(&self) -> DocumentIdentity {
        self.identity
    }
    pub fn matches(&self, current: &Self) -> bool {
        self == current
    }
}

#[derive(Debug)]
struct ReviewIncarnation;
/// Retained controller capability plus a checked, nonwrapping local generation.
/// Clone this opaque value into actions; it is deliberately not Copy. Keeping an
/// old action alive also keeps its incarnation allocation alive across replacement.
#[derive(Clone, Debug)]
pub struct ReviewId {
    incarnation: Arc<ReviewIncarnation>,
    generation: u64,
}
impl PartialEq for ReviewId {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.incarnation, &other.incarnation) && self.generation == other.generation
    }
}
impl Eq for ReviewId {}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegistrationAction {
    Preview { extensions: Vec<Extension>, current: Option<CurrentFile> },
    RegisterSelected { review: ReviewId, current: Option<CurrentFile> },
    Skip,
    Status,
    Unregister { journal_id: u64 },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActionErrorKind {
    Busy,
    Queue(SubmitError),
    EmptySelection,
    UnadoptedCapacity { selected: usize, limit: usize },
    MissingContentTypes(Vec<Extension>),
    ExtendNotAdopted { journal_id: u64 },
    StaleReview,
    StaleCurrentFile,
    InvalidNativePath,
    Extension(ExtensionError),
    Exhausted,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionError {
    pub kind: ActionErrorKind,
    pub action: RegistrationAction,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtensionOutcome {
    Planned,
    /// Every required binding matched at Plan time only. This observation lives
    /// in Preview; it does not verify availability at Apply or a later Status.
    UnchangedAtPreview,
    /// Only changed receipt locations were applied/observed in their after-state.
    /// Unchanged required bindings and effective defaults have NOT been verified.
    ReceiptApplied,
    /// The backend has no complete-binding inspection result. In particular, an
    /// empty receipt cannot refresh an earlier UnchangedAtPreview observation.
    VerificationIncomplete,
    NeedsInspection,
    /// Receipt locations only; no claim about unchanged or externally owned data.
    RestoredVisible,
    PartiallyApplied,
    Conflict,
    Unreadable,
}
#[derive(Clone, Debug, PartialEq, Eq)]
/// Counters cover changed durable-receipt locations only, not every binding
/// required for an extension to remain openable. Preview is a separate snapshot.
pub struct ExtensionStatus {
    pub extension: Extension,
    pub outcome: ExtensionOutcome,
    pub changed_values: usize,
    pub applied: usize,
    pub before: usize,
    pub conflicts: usize,
    pub unreadable: usize,
}
/// What `FileTypeRegistration::remove_all_blocking` did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoveAllOutcome {
    NothingRegistered,
    Removed,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegistrationStatus {
    Idle,
    Pending,
    Ready {
        review: ReviewId,
    },
    Skipped,
    Cancelled,
    Applied {
        journal_id: u64,
    },
    Unregistered {
        journal_id: u64,
    },
    RetainedRecovery {
        journal_id: u64,
        transactions_left: u64,
    },
    /// Journal availability, not OS file-type availability or default ownership.
    NoRetainedReceipt {
        transactions_left: u64,
    },
    Failed {
        message: String,
        recovery: Option<u64>,
    },
}
#[derive(Clone, Debug)]
pub struct Preview {
    pub review: ReviewId,
    pub extensions: Vec<ExtensionStatus>,
    pub notes: Vec<String>,
    pub default_choice: DefaultChoice,
}

struct Review {
    id: ReviewId,
    token: PlanToken,
    selected: Vec<Extension>,
    current: Option<CurrentFile>,
    accepted: bool,
}
enum PendingKind {
    Preview { selected: Vec<Extension>, current: Option<CurrentFile> },
    Apply,
    Cancel,
    Status,
    Undo,
}
struct Pending {
    id: RequestId,
    kind: PendingKind,
}
/// What `apply` still has to do after the current worker request finishes.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Chain {
    None,
    /// Plan done -> apply it at once (no separate review click).
    ApplyAfterPlan,
    /// Undo done -> plan and apply these types.
    PlanAfterUndo(Vec<Extension>),
}
struct Resources {
    worker: AssociationWorker,
    events: Events,
    context: RegistrationContext,
    selection: Selection,
    review: Option<Review>,
    preview: Option<Preview>,
    pending: Option<Pending>,
    incarnation: Arc<ReviewIncarnation>,
    next_review: u64,
    recovery: Option<u64>,
    rows: Vec<ExtensionStatus>,
    status: RegistrationStatus,
    /// Types Kwil has registered right now, from the receipt and the OS state;
    /// `None` until known (or when the platform's receipt cannot name them).
    registered: Option<Vec<Extension>>,
    chain: Chain,
    #[cfg(test)]
    submit_error: Option<SubmitError>,
}
impl Resources {
    fn submit(&self, request: Request) -> Result<RequestId, SubmitError> {
        #[cfg(test)]
        if let Some(error) = self.submit_error {
            return Err(error);
        }
        self.worker.try_submit(request)
    }
}

/// A single retirement mailbox, independent of the worker's eight-event queue.
/// At most one large backend event is retired at once. If full, leave the event
/// in the backend; the retirement lane posts one capacity wake after releasing it.
#[derive(Default)]
struct Retired {
    event: Option<Event>,
    resources: Option<Resources>,
    closing: bool,
    waiting: bool,
}
#[derive(Default)]
struct RetirementQueue {
    state: Mutex<Retired>,
    ready: Condvar,
}
pub struct RegistrationRetirement {
    join: Option<JoinHandle<Result<(), String>>>,
}
impl RegistrationRetirement {
    /// Off UI, after the event loop. Wait for accepted requests and journal I/O.
    pub fn shutdown(mut self) -> Result<(), String> {
        self.join
            .take()
            .map_or(Ok(()), |join| join.join().map_err(|_| "registration retirement panicked".to_owned())?)
    }
}
pub struct FileTypeRegistration {
    resources: Option<Resources>,
    retirement: Arc<RetirementQueue>,
    join: Option<JoinHandle<Result<(), String>>>,
}
impl FileTypeRegistration {
    pub fn start<A, F>(
        journal: PathBuf,
        context: RegistrationContext,
        factory: F,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> io::Result<Self>
    where
        A: AssociationAdapter + 'static,
        F: FnOnce() -> Result<A, AssociationError> + Send + 'static,
    {
        context.validate()?;
        let worker_wake = wake.clone();
        let (worker, events) = AssociationWorker::spawn(journal, factory, move || worker_wake())?;
        let retirement = Arc::new(RetirementQueue::default());
        let queue = retirement.clone();
        let join = thread::Builder::new().name("kwil-association-retire".into()).spawn(move || {
            loop {
                let (event, resources, should_wake, closing) = {
                    let mut state = queue.state.lock().unwrap_or_else(|error| error.into_inner());
                    while state.event.is_none() && !state.closing {
                        state = queue.ready.wait(state).unwrap_or_else(|error| error.into_inner());
                    }
                    let event = state.event.take();
                    let should_wake = std::mem::take(&mut state.waiting);
                    (event, state.resources.take(), should_wake, state.closing)
                };
                drop(event); // plans/receipt bytes are never destroyed on the UI
                if should_wake {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| wake()));
                }
                if closing {
                    if let Some(resources) = resources {
                        let Resources { worker, events, .. } = resources;
                        let result = worker.shutdown().map_err(|error| error.to_string());
                        drop(events);
                        return result;
                    }
                    return Ok(());
                }
            }
        })?;
        Ok(Self {
            resources: Some(Resources {
                worker,
                events,
                context,
                selection: Selection::default(),
                review: None,
                preview: None,
                pending: None,
                incarnation: Arc::new(ReviewIncarnation),
                next_review: 0,
                recovery: None,
                rows: Vec::new(),
                status: RegistrationStatus::Idle,
                registered: None,
                chain: Chain::None,
                #[cfg(test)]
                submit_error: None,
            }),
            retirement,
            join: Some(join),
        })
    }
    fn state(&self) -> &Resources {
        self.resources.as_ref().expect("live registration session")
    }
    fn state_mut(&mut self) -> &mut Resources {
        self.resources.as_mut().expect("live registration session")
    }
    pub fn selection(&self) -> &Selection {
        &self.state().selection
    }
    pub fn registers_build_folder(&self) -> bool {
        kwil_core_commands::associations::windows_paths::is_build_executable(&self.state().context.executable)
    }
    /// Edits are pure. register_selected rechecks the exact selected snapshot.
    pub fn selection_mut(&mut self) -> &mut Selection {
        &mut self.state_mut().selection
    }
    pub fn registration_status(&self) -> &RegistrationStatus {
        &self.state().status
    }
    pub fn extension_status(&self) -> &[ExtensionStatus] {
        &self.state().rows
    }
    pub fn reviewed_preview(&self) -> Option<&Preview> {
        self.state().preview.as_ref()
    }
    pub fn preview(&mut self) -> Result<RequestId, ActionError> {
        self.submit(RegistrationAction::Preview { extensions: self.selection().extensions().to_vec(), current: None })
    }
    /// Palette host captures from the actual current document; never from title.
    /// At accept it must recapture and pass the current native path + identity.
    pub fn preview_current(&mut self, current: CurrentFile) -> Result<RequestId, ActionError> {
        self.submit(RegistrationAction::Preview { extensions: vec![current.extension.clone()], current: Some(current) })
    }
    pub fn register_selected(
        &mut self,
        review: ReviewId,
        current: Option<CurrentFile>,
    ) -> Result<RequestId, ActionError> {
        self.submit(RegistrationAction::RegisterSelected { review, current })
    }
    pub fn skip(&mut self) -> Result<Option<RequestId>, ActionError> {
        let action = RegistrationAction::Skip;
        if self.state().pending.is_some() {
            return Err(ActionError { kind: ActionErrorKind::Busy, action });
        }
        if self.state().review.is_none() || self.state().recovery.is_some() {
            self.state_mut().status = RegistrationStatus::Skipped;
            return Ok(None);
        }
        self.submit(action).map(Some)
    }
    pub fn status(&mut self) -> Result<RequestId, ActionError> {
        self.submit(RegistrationAction::Status)
    }
    /// Explicit whole-registration unregister, not a silent update or per-row Undo.
    pub fn unregister(&mut self, journal_id: u64) -> Result<RequestId, ActionError> {
        self.submit(RegistrationAction::Unregister { journal_id })
    }
    /// Types Kwil has registered right now, once a status or action reported it.
    pub fn registered(&self) -> Option<&[Extension]> {
        self.state().registered.as_deref()
    }
    /// A Kwil registration receipt exists, so "Remove all" has something to do,
    /// even when no type is fully registered any more (e.g. a half-finished undo).
    pub fn has_registration(&self) -> bool {
        self.state().recovery.is_some()
    }
    /// (to add, to remove) when the checked types are applied.
    pub fn change_counts(&self) -> (usize, usize) {
        let selected = self.selection().extensions();
        match self.registered() {
            Some(registered) => (
                selected.iter().filter(|e| !registered.contains(e)).count(),
                registered.iter().filter(|e| !selected.contains(e)).count(),
            ),
            None => (selected.len(), 0),
        }
    }
    /// Make the operating system match the checked types: one click, no review.
    pub fn apply(&mut self) -> Result<(), ActionError> {
        if self.state().pending.is_some() {
            return Err(ActionError { kind: ActionErrorKind::Busy, action: RegistrationAction::Status });
        }
        let target = self.selection().extensions().to_vec();
        let result = match self.state().recovery {
            Some(journal_id) => {
                self.state_mut().chain = if target.is_empty() { Chain::None } else { Chain::PlanAfterUndo(target) };
                self.unregister(journal_id).map(drop)
            }
            None if target.is_empty() => Ok(()),
            None => {
                self.state_mut().chain = Chain::ApplyAfterPlan;
                self.preview().map(drop)
            }
        };
        if result.is_err() {
            self.state_mut().chain = Chain::None;
        }
        result
    }
    /// Remove everything Kwil registered and restore the previous handlers.
    pub fn remove_all(&mut self) -> Result<(), ActionError> {
        let Some(journal_id) = self.state().recovery else { return Ok(()) };
        self.state_mut().chain = Chain::None;
        self.unregister(journal_id).map(drop)
    }
    /// "Remove all" without a window (`kwil --unregister-file-types`, run by the
    /// Windows uninstaller): ask for the status, then undo the retained receipt.
    /// `wake` receives this service's wake callback; `deadline` bounds both steps.
    pub fn remove_all_blocking(
        &mut self,
        wake: &mpsc::Receiver<()>,
        deadline: Instant,
    ) -> Result<RemoveAllOutcome, String> {
        self.status().map_err(|error| format!("{:?}", error.kind))?;
        self.settle_before(wake, deadline)?;
        if !self.has_registration() {
            return match self.registration_status() {
                RegistrationStatus::Failed { message, .. } => Err(message.clone()),
                _ => Ok(RemoveAllOutcome::NothingRegistered),
            };
        }
        self.remove_all().map_err(|error| format!("{:?}", error.kind))?;
        self.settle_before(wake, deadline)?;
        match self.registration_status() {
            RegistrationStatus::Unregistered { .. } => Ok(RemoveAllOutcome::Removed),
            RegistrationStatus::Failed { message, .. } => Err(message.clone()),
            other => Err(format!("unexpected registration state {other:?}")),
        }
    }
    fn settle_before(&mut self, wake: &mpsc::Receiver<()>, deadline: Instant) -> Result<(), String> {
        while self.state().pending.is_some() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || wake.recv_timeout(left).is_err() {
                return Err("the registration worker did not answer in time".into());
            }
            self.worker_ready();
        }
        Ok(())
    }
    /// Continue `apply` after a finished worker request; any other outcome ends it.
    fn continue_chain(&mut self) {
        let chain = std::mem::replace(&mut self.state_mut().chain, Chain::None);
        let status = self.state().status.clone();
        let result = match (chain, status) {
            (Chain::ApplyAfterPlan, RegistrationStatus::Ready { review }) => {
                self.register_selected(review, None).map(drop)
            }
            (Chain::PlanAfterUndo(extensions), RegistrationStatus::Unregistered { .. }) => {
                self.state_mut().chain = Chain::ApplyAfterPlan;
                self.submit(RegistrationAction::Preview { extensions, current: None }).map(drop)
            }
            _ => Ok(()),
        };
        if let Err(error) = result {
            let state = self.state_mut();
            state.chain = Chain::None;
            let message = match error.kind {
                ActionErrorKind::StaleReview => "The checked types changed while applying; apply again.".to_owned(),
                kind => format!("Applying stopped: {kind:?}"),
            };
            state.status = RegistrationStatus::Failed { message, recovery: state.recovery };
        }
    }
    fn submit(&mut self, action: RegistrationAction) -> Result<RequestId, ActionError> {
        let result = self.prepare(&action);
        let (request, kind) = match result {
            Ok(prepared) => prepared,
            Err(kind) => return Err(ActionError { kind, action }),
        };
        match self.state().submit(request) {
            Ok(id) => {
                let state = self.state_mut();
                if matches!(kind, PendingKind::Preview { .. }) {
                    state.review = None;
                    state.preview = None;
                    state.rows.clear();
                } else if matches!(kind, PendingKind::Apply)
                    && let Some(review) = &mut state.review
                {
                    review.accepted = true;
                }
                state.pending = Some(Pending { id, kind });
                state.status = RegistrationStatus::Pending;
                Ok(id)
            }
            Err(error) => Err(ActionError { kind: ActionErrorKind::Queue(error), action }),
        }
    }
    fn prepare(&self, action: &RegistrationAction) -> Result<(Request, PendingKind), ActionErrorKind> {
        let state = self.state();
        if state.pending.is_some() {
            return Err(ActionErrorKind::Busy);
        }
        match action {
            RegistrationAction::Preview { extensions, current } => {
                if let Some(journal_id) = state.recovery {
                    if state
                        .review
                        .as_ref()
                        .is_some_and(|review| review.selected == *extensions && review.current == *current)
                    {
                        return Ok((Request::Status, PendingKind::Status));
                    }
                    return Err(ActionErrorKind::ExtendNotAdopted { journal_id });
                }
                if extensions.is_empty() {
                    return Err(ActionErrorKind::EmptySelection);
                }
                let limit = state.context.limits.types();
                if extensions.len() > limit {
                    return Err(ActionErrorKind::UnadoptedCapacity { selected: extensions.len(), limit });
                }
                let missing = extensions
                    .iter()
                    .filter(|extension| {
                        !matches!(state.context.platform, AssociationPlatform::Windows)
                            && !state.context.content_types.contains_key(*extension)
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                if !missing.is_empty() {
                    return Err(ActionErrorKind::MissingContentTypes(missing));
                }
                if state.next_review == u64::MAX {
                    return Err(ActionErrorKind::Exhausted);
                }
                let request = AssociationRequest {
                    platform: state.context.platform.clone(),
                    executable: state.context.executable.clone(),
                    default_choice: state.context.default_choice,
                    types: extensions
                        .iter()
                        .map(|extension| FileType {
                            extension: extension.as_str().into(),
                            content_type: state.context.content_types.get(extension).cloned(),
                        })
                        .collect(),
                };
                Ok((
                    Request::Plan(request),
                    PendingKind::Preview { selected: extensions.clone(), current: current.clone() },
                ))
            }
            RegistrationAction::RegisterSelected { review, current } => {
                let reviewed = state
                    .review
                    .as_ref()
                    .filter(|candidate| candidate.id == *review)
                    .ok_or(ActionErrorKind::StaleReview)?;
                if reviewed.current != *current {
                    return Err(ActionErrorKind::StaleCurrentFile);
                }
                if reviewed.current.is_none() && reviewed.selected != state.selection.extensions() {
                    return Err(ActionErrorKind::StaleReview);
                }
                // Repeated explicit acceptance reports recovery/status, no second write.
                if state.recovery.is_some() {
                    return Ok((Request::Status, PendingKind::Status));
                }
                if reviewed.accepted {
                    return Err(ActionErrorKind::StaleReview);
                }
                Ok((Request::Apply { token: reviewed.token }, PendingKind::Apply))
            }
            RegistrationAction::Skip => {
                let reviewed = state.review.as_ref().ok_or(ActionErrorKind::StaleReview)?;
                Ok((Request::Cancel { token: reviewed.token }, PendingKind::Cancel))
            }
            RegistrationAction::Status => Ok((Request::Status, PendingKind::Status)),
            RegistrationAction::Unregister { journal_id } => {
                Ok((Request::Undo { journal_id: *journal_id }, PendingKind::Undo))
            }
        }
    }
    /// One accepted request at a time. One addressed wake consumes at most one
    /// event; a retirement-capacity wake resumes it without polling or a timer.
    pub fn worker_ready(&mut self) -> bool {
        let retirement = self.retirement.clone();
        let mut retired = retirement.state.lock().unwrap_or_else(|error| error.into_inner());
        if retired.event.is_some() {
            retired.waiting = true;
            return false;
        }
        let Some(event) = self.state().events.try_recv() else {
            return false;
        };
        if self.accept(&event) {
            self.continue_chain();
        }
        retired.event = Some(event);
        retirement.ready.notify_one();
        true
    }
    /// True when the event answered our pending request (and was applied).
    fn accept(&mut self, event: &Event) -> bool {
        let state = self.state_mut();
        let Some(pending) = state.pending.take() else { return false };
        if pending.id != event.request_id {
            state.pending = Some(pending); // foreign event cannot consume our ownership
            return false;
        }
        match &event.result {
            Err(error) => {
                state.recovery = error.recovery.or(state.recovery);
                if matches!(pending.kind, PendingKind::Apply | PendingKind::Undo) {
                    for row in &mut state.rows {
                        row.outcome = ExtensionOutcome::NeedsInspection;
                    }
                }
                state.status = RegistrationStatus::Failed { message: error.message.clone(), recovery: state.recovery };
            }
            Ok(Response::Planned { token, plan }) => {
                let PendingKind::Preview { selected, current } = pending.kind else {
                    return true;
                };
                let Some(generation) = state.next_review.checked_add(1) else {
                    state.status = RegistrationStatus::Failed {
                        message: "registration review generation exhausted; replace controller".into(),
                        recovery: state.recovery,
                    };
                    return true;
                };
                state.next_review = generation;
                let id = ReviewId { incarnation: state.incarnation.clone(), generation };
                state.rows = selected
                    .iter()
                    .map(|extension| {
                        let changed_values = plan
                            .changes()
                            .iter()
                            .filter(|change| affects(&state.context, extension, &change.location))
                            .count();
                        ExtensionStatus {
                            extension: extension.clone(),
                            changed_values,
                            outcome: if changed_values == 0 {
                                ExtensionOutcome::UnchangedAtPreview
                            } else {
                                ExtensionOutcome::Planned
                            },
                            applied: 0,
                            before: 0,
                            conflicts: 0,
                            unreadable: 0,
                        }
                    })
                    .collect();
                state.preview = Some(Preview {
                    review: id.clone(),
                    extensions: state.rows.clone(),
                    notes: plan.notes.clone(),
                    default_choice: plan.default_choice,
                });
                state.review = Some(Review { id: id.clone(), token: *token, selected, current, accepted: false });
                state.status = RegistrationStatus::Ready { review: id };
            }
            Ok(Response::Applied { journal_id }) => {
                state.recovery = Some(*journal_id);
                if let Some(review) = &state.review {
                    let mut registered = review.selected.clone();
                    registered.sort();
                    state.registered = Some(registered);
                }
                for row in &mut state.rows {
                    row.outcome = if row.changed_values == 0 {
                        ExtensionOutcome::VerificationIncomplete
                    } else {
                        ExtensionOutcome::ReceiptApplied
                    };
                    row.applied = row.changed_values;
                }
                state.status = RegistrationStatus::Applied { journal_id: *journal_id };
            }
            Ok(Response::Cancelled) => {
                state.review = None;
                state.preview = None;
                state.status = RegistrationStatus::Cancelled;
            }
            Ok(Response::Undone { journal_id }) => {
                state.review = None;
                state.preview = None;
                state.recovery = None;
                state.registered = Some(Vec::new());
                for row in &mut state.rows {
                    row.outcome = ExtensionOutcome::RestoredVisible;
                    row.before = row.changed_values;
                    row.applied = 0;
                }
                state.status = RegistrationStatus::Unregistered { journal_id: *journal_id };
            }
            Ok(Response::Status(status)) => inspect(state, status),
        }
        true
    }
    fn retire(&mut self) {
        if let Some(resources) = self.resources.take() {
            let mut retired = self.retirement.state.lock().unwrap_or_else(|error| error.into_inner());
            retired.resources = Some(resources);
            retired.closing = true;
            self.retirement.ready.notify_one();
        }
    }
    pub fn stop(mut self) -> RegistrationRetirement {
        self.retire();
        RegistrationRetirement { join: self.join.take() }
    }
}
impl Drop for FileTypeRegistration {
    fn drop(&mut self) {
        self.retire();
    }
}

fn affects(context: &RegistrationContext, extension: &Extension, location: &Location) -> bool {
    match (&context.platform, location) {
        (AssociationPlatform::Windows, Location::WindowsValue { subkey, .. }) => {
            let extension = extension.as_str();
            subkey
                .strip_prefix("Kwil.")
                .is_some_and(|tail| tail == extension || tail.strip_suffix("\\shell\\open\\command") == Some(extension))
                || subkey.strip_prefix('.').and_then(|tail| tail.strip_suffix("\\OpenWithProgids")) == Some(extension)
        }
        (AssociationPlatform::MacOS { .. }, Location::MacRole { uti, .. }) => {
            context.content_types.get(extension) == Some(uti)
        }
        (AssociationPlatform::Linux { data_home, config_home }, Location::UserFile(path)) => {
            path.strip_prefix(data_home).is_ok_and(|tail| tail == Path::new("applications/kwil.desktop"))
                || path.strip_prefix(config_home).is_ok_and(|tail| tail == Path::new("mimeapps.list"))
        }
        _ => false,
    }
}
/// Types whose "Open with" binding (Windows) or role (macOS) Kwil owns right
/// now. Linux receipts name a shared desktop file, not single types: `None`.
fn registered_types(context: &RegistrationContext, status: &Status) -> Option<Vec<Extension>> {
    let Some(recovery) = &status.recovery else { return Some(Vec::new()) };
    let mut found = Vec::new();
    for (change, current) in recovery.receipt.pending().iter().zip(&recovery.current) {
        if *current != CurrentState::Applied {
            continue;
        }
        let name = match (&context.platform, &change.location) {
            (AssociationPlatform::Windows, Location::WindowsValue { subkey, .. }) => {
                subkey.strip_prefix('.').and_then(|tail| tail.strip_suffix("\\OpenWithProgids")).map(str::to_owned)
            }
            (AssociationPlatform::MacOS { .. }, Location::MacRole { uti, .. }) => context
                .content_types
                .iter()
                .find(|(_, value)| *value == uti)
                .map(|(extension, _)| extension.as_str().to_owned()),
            (AssociationPlatform::Linux { .. }, _) => return None,
            _ => None,
        };
        if let Some(extension) = name.and_then(|name| Extension::parse(&name).ok()) {
            found.push(extension);
        }
    }
    found.sort();
    found.dedup();
    Some(found)
}
fn inspect(state: &mut Resources, status: &Status) {
    state.registered = registered_types(&state.context, status);
    if let Some(registered) = &state.registered {
        // The checkboxes start from what is really registered, not an old draft.
        state.selection = Selection::from_extensions(registered.iter().cloned());
    }
    state.recovery = status.recovery.as_ref().map(|recovery| recovery.journal_id);
    if let Some(recovery) = &status.recovery {
        // Restart may have an aggregate receipt without per-extension metadata.
        // Do not invent rows/membership; durable batch metadata is an Extend proposal.
        for row in &mut state.rows {
            row.applied = 0;
            row.before = 0;
            row.conflicts = 0;
            row.unreadable = 0;
            for (change, current) in recovery.receipt.pending().iter().zip(&recovery.current) {
                if !affects(&state.context, &row.extension, &change.location) {
                    continue;
                }
                match current {
                    CurrentState::Before => row.before += 1,
                    CurrentState::Applied => row.applied += 1,
                    CurrentState::Conflict => row.conflicts += 1,
                    CurrentState::Unreadable(_) => row.unreadable += 1,
                }
            }
            row.outcome = if row.unreadable != 0 {
                ExtensionOutcome::Unreadable
            } else if row.conflicts != 0 {
                ExtensionOutcome::Conflict
            } else if row.changed_values == 0 {
                ExtensionOutcome::VerificationIncomplete
            } else if row.before == row.changed_values {
                ExtensionOutcome::RestoredVisible
            } else if row.applied == row.changed_values {
                ExtensionOutcome::ReceiptApplied
            } else {
                ExtensionOutcome::PartiallyApplied
            };
        }
        state.status = RegistrationStatus::RetainedRecovery {
            journal_id: recovery.journal_id,
            transactions_left: status.transactions_left,
        };
    } else {
        state.status = RegistrationStatus::NoRetainedReceipt { transactions_left: status.transactions_left };
    }
}

#[cfg(test)]
#[path = "file_type_registration_tests.rs"]
mod tests;
