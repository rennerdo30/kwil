//! Bounded, event-driven per-user association service. All adapter construction,
//! planning and journal/native I/O happen on its single worker. No native adapter
//! is selected here: the host supplies a factory and a private journal directory.
//!
//! Review the immutable `Response::Planned` snapshot, then submit only its token.
//! Tokens are local to this worker incarnation; restart never resumes Apply.
//! An immutable, synced full receipt precedes every backend write. Until Undo
//! retires it, even a failed/interrupted Apply prevents another transaction.
//! Undo preserves the original Before bytes. Append-only repair records durably
//! update owned After signatures before writes, so interrupted repair/Undo retry.
//! Native restoration must pass the backend durability barrier before retirement;
//! a failed/unsupported barrier retains that full receipt even if reads show Before.
//! Records are never replaced, renamed or deleted. History has a finite limit;
//! reaching it requires explicit host maintenance while the service is stopped.
//!
//! Like the backend, checks are optimistic, not an OS-wide compare-and-swap.
//! The journal lock serializes workers using this directory, not external apps.
//! On Windows the host must supply an application-data directory with a private
//! per-user ACL. Unix additionally enforces ownership and private permissions.

use super::{AssociationAdapter, AssociationError, AssociationPlan, AssociationReceipt, AssociationRequest};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{SystemTime, UNIX_EPOCH},
};

pub const QUEUE_CAPACITY: usize = 8;
pub const MAX_JOURNAL_TRANSACTIONS: u64 = 32;
pub const MAX_REPAIR_RECORDS: usize = 32;
pub const MAX_JOURNAL_ENTRIES: usize = MAX_JOURNAL_TRANSACTIONS as usize * 2 + MAX_REPAIR_RECORDS + 1;

/// Background startup entry point; the host verifies and guards this directory.
pub fn repair_startup<A: AssociationAdapter, F: FnOnce() -> Result<A, AssociationError>>(
    journal_directory: PathBuf,
    factory: F,
) -> Result<(), WorkerError> {
    // Same lock, receipt validation, repair replay and write-ahead publication as
    // the settings worker, without starting the UI/controller or another thread.
    let journal = Journal::open(journal_directory)?;
    let _engine = Engine::new(factory()?, journal)?;
    Ok(())
}
pub const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;
pub(super) const MAX_PATH: usize = 4096;

/// Opaque review identity. Only the worker's current plan can be applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlanToken {
    epoch: u128,
    instance: u64,
    process: u32,
    generation: u64,
}

#[derive(Clone, Debug)]
pub enum Request {
    Plan(AssociationRequest),
    Apply {
        token: PlanToken,
    },
    Cancel {
        token: PlanToken,
    },
    Status,
    /// Uses the durable receipt, including after restarting the service.
    Undo {
        journal_id: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmitError {
    Full,
    Closed,
    TooLarge,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerError {
    pub message: String,
    /// Present when an immutable receipt must be inspected/retried with Undo.
    pub recovery: Option<u64>,
}
impl std::fmt::Display for WorkerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for WorkerError {}
impl From<AssociationError> for WorkerError {
    fn from(e: AssociationError) -> Self {
        failure(e.to_string())
    }
}
impl From<io::Error> for WorkerError {
    fn from(e: io::Error) -> Self {
        failure(e.to_string())
    }
}
fn failure(message: impl Into<String>) -> WorkerError {
    WorkerError { message: message.into(), recovery: None }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CurrentState {
    Before,
    Applied,
    Conflict,
    Unreadable(String),
}
#[derive(Clone, Debug)]
pub struct RecoveryStatus {
    pub journal_id: u64,
    pub receipt: AssociationReceipt,
    /// In the same order as `receipt.pending()`; inspection never mutates.
    pub current: Vec<CurrentState>,
}
#[derive(Clone, Debug)]
pub struct Status {
    pub pending_plan: Option<PlanToken>,
    pub recovery: Option<RecoveryStatus>,
    pub transactions_left: u64,
}
#[derive(Clone, Debug)]
pub enum Response {
    Planned { token: PlanToken, plan: AssociationPlan },
    Applied { journal_id: u64 },
    Cancelled,
    Undone { journal_id: u64 },
    Status(Status),
}
#[derive(Clone, Debug)]
pub struct Event {
    pub request_id: RequestId,
    pub result: Result<Response, WorkerError>,
}

struct Mailbox {
    requests: VecDeque<(RequestId, Request)>,
    events: VecDeque<Event>,
    outstanding: usize,
    next: u64,
    closing: bool,
    receiver_open: bool,
}
struct Shared {
    mailbox: Mutex<Mailbox>,
    ready: Condvar,
}
impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Mailbox> {
        self.mailbox.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn close(&self) {
        self.lock().closing = true;
        self.ready.notify_all();
    }
}

/// UI-side handle. Submission and Drop perform no filesystem/native work and
/// never wait for queue space, the adapter, or the worker to finish.
pub struct AssociationWorker {
    shared: Arc<Shared>,
    join: Option<JoinHandle<()>>,
}
pub struct Events {
    shared: Arc<Shared>,
}
impl AssociationWorker {
    /// Starts exactly one thread. `factory` runs there, and its adapter need not
    /// be Send. `wake` must only post a nonblocking UI event (no waiting/joining).
    /// It runs outside the mailbox lock, only on empty-to-nonempty transitions.
    /// Opening/validating the journal also runs on the worker; initialization
    /// failures are returned for each accepted request, without native writes.
    pub fn spawn<A, F, W>(journal_directory: PathBuf, factory: F, wake: W) -> io::Result<(Self, Events)>
    where
        A: AssociationAdapter + 'static,
        F: FnOnce() -> Result<A, AssociationError> + Send + 'static,
        W: Fn() + Send + 'static,
    {
        let shared = Arc::new(Shared {
            mailbox: Mutex::new(Mailbox {
                requests: VecDeque::new(),
                events: VecDeque::new(),
                outstanding: 0,
                next: 0,
                closing: false,
                receiver_open: true,
            }),
            ready: Condvar::new(),
        });
        let worker_shared = shared.clone();
        let join = thread::Builder::new().name("kwil-associations".into()).spawn(move || {
            let mut engine = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                Journal::open(journal_directory).and_then(|journal| {
                    factory().map_err(WorkerError::from).and_then(|adapter| Engine::new(adapter, journal))
                })
            }))
            .unwrap_or_else(|_| Err(failure("association worker initialization panicked")));
            loop {
                let work = {
                    let mut m = worker_shared.lock();
                    while m.requests.is_empty() && !m.closing {
                        m = worker_shared.ready.wait(m).unwrap_or_else(|e| e.into_inner());
                    }
                    m.requests.pop_front()
                };
                let Some((request_id, request)) = work else { break };
                let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match &mut engine {
                    Ok(engine) => engine.execute(request),
                    Err(error) => Err(error.clone()),
                })) {
                    Ok(result) => result,
                    Err(_) => {
                        let mut error = failure("association adapter panicked; restart and inspect durable recovery");
                        if let Ok(engine) = &engine {
                            error.recovery = engine.journal.active.as_ref().map(|r| r.id);
                        }
                        engine = Err(error.clone());
                        Err(error)
                    }
                };
                let should_wake = {
                    let mut m = worker_shared.lock();
                    if m.receiver_open {
                        let empty = m.events.is_empty();
                        m.events.push_back(Event { request_id, result });
                        empty
                    } else {
                        m.outstanding -= 1;
                        false
                    }
                };
                if should_wake {
                    // A failed host notification cannot strand accepted work.
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(&wake));
                }
            }
        })?;
        Ok((Self { shared: shared.clone(), join: Some(join) }, Events { shared }))
    }

    pub fn try_submit(&self, request: Request) -> Result<RequestId, SubmitError> {
        if let Request::Plan(request) = &request
            && !bounded_request(request)
        {
            return Err(SubmitError::TooLarge);
        }
        let mut m = self.shared.lock();
        if m.closing || !m.receiver_open {
            return Err(SubmitError::Closed);
        }
        if m.outstanding == QUEUE_CAPACITY {
            return Err(SubmitError::Full);
        }
        let next = m.next.checked_add(1).ok_or(SubmitError::Closed)?;
        m.next = next;
        let id = RequestId(next);
        m.outstanding += 1;
        m.requests.push_back((id, request));
        drop(m);
        self.shared.ready.notify_one();
        Ok(id)
    }

    /// Call after moving this handle off the UI thread. Closes admission and
    /// drains every accepted request, even with undrained/closed event output.
    /// Drop instead detaches the same draining worker and never joins it.
    pub fn shutdown(mut self) -> Result<(), WorkerError> {
        self.shared.close();
        if let Some(join) = self.join.take() {
            if join.thread().id() == thread::current().id() {
                return Err(failure("cannot join association worker from its wake callback"));
            }
            join.join().map_err(|_| failure("association worker panicked; inspect durable recovery on restart"))?;
        }
        Ok(())
    }
}
impl Drop for AssociationWorker {
    fn drop(&mut self) {
        self.shared.close();
    }
}
impl Events {
    pub fn try_recv(&self) -> Option<Event> {
        let mut m = self.shared.lock();
        let event = m.events.pop_front()?;
        m.outstanding -= 1;
        Some(event)
    }
}
impl Drop for Events {
    fn drop(&mut self) {
        let mut m = self.shared.lock();
        m.receiver_open = false;
        m.closing = true;
        let undrained = m.events.len();
        m.outstanding -= undrained;
        m.events.clear();
        drop(m);
        self.shared.ready.notify_all();
    }
}

fn bounded_path(path: &Path) -> bool {
    path.as_os_str().len() <= MAX_PATH
}
pub(super) fn bounded_request(request: &AssociationRequest) -> bool {
    use super::AssociationPlatform;
    bounded_path(&request.executable)
        && !request.types.is_empty()
        && request.types.len() <= super::MAX_TYPES
        && request
            .types
            .iter()
            .all(|t| t.extension.len() <= 255 && t.content_type.as_ref().is_none_or(|s| s.len() <= 255))
        && match &request.platform {
            AssociationPlatform::Windows => true,
            AssociationPlatform::MacOS { bundle_id } => bundle_id.len() <= 255,
            AssociationPlatform::Linux { data_home, config_home } => {
                bounded_path(data_home) && bounded_path(config_home)
            }
        }
}

struct Engine<A> {
    adapter: A,
    journal: Journal,
    pending: Option<(PlanToken, AssociationPlan)>,
    identity: PlanToken,
}
impl<A: AssociationAdapter> Engine<A> {
    fn new(mut adapter: A, mut journal: Journal) -> Result<Self, WorkerError> {
        if journal.active.as_ref().is_some_and(|r| r.receipt.owner() != adapter.owner()) {
            return Err(failure("journal belongs to another user/backend; refusing recovery"));
        }
        if let Some(executable) = adapter.repair_executable() {
            journal.repair_owned(&mut adapter, &executable)?;
        }
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let identity = PlanToken {
            epoch: SystemTime::now().duration_since(UNIX_EPOCH).map_err(|e| failure(e.to_string()))?.as_nanos(),
            instance: NEXT.fetch_add(1, Ordering::Relaxed),
            process: std::process::id(),
            generation: 0,
        };
        Ok(Self { adapter, journal, pending: None, identity })
    }
    fn execute(&mut self, request: Request) -> Result<Response, WorkerError> {
        // Includes temporary native reads, bounded request/response metadata and
        // rollback/serialization overlap. Retained payloads have separate leases.
        let result = super::Admission::acquire(super::WORKSPACE_BYTES)
            .map_err(WorkerError::from)
            .and_then(|_work| self.run(request));
        result.map_err(|mut e| {
            e.recovery = self.journal.active.as_ref().map(|r| r.id);
            e
        })
    }
    fn run(&mut self, request: Request) -> Result<Response, WorkerError> {
        match request {
            Request::Plan(request) => {
                self.pending = None;
                if self.journal.active.is_some() {
                    return Err(failure("Undo pending receipt before planning another transaction"));
                }
                if !bounded_request(&request) {
                    return Err(failure("association request exceeds bounds"));
                }
                self.identity.generation =
                    self.identity.generation.checked_add(1).ok_or_else(|| failure("plan generation exhausted"))?;
                let plan = super::plan(&mut self.adapter, &request)?;
                validate_plan(&plan)?;
                let token = self.identity;
                self.pending = Some((token, plan.clone()));
                Ok(Response::Planned { token, plan })
            }
            Request::Cancel { token } => {
                self.reviewed(token)?;
                self.pending = None;
                Ok(Response::Cancelled)
            }
            Request::Apply { token } => {
                let plan = self.reviewed(token)?.clone();
                self.pending = None; // Every apply attempt consumes review, including failure.
                if self.journal.active.is_some() {
                    return Err(failure("Undo pending receipt before Apply"));
                }
                if self.adapter.owner() != plan.owner {
                    return Err(failure("association backend identity changed"));
                }
                // Fresh whole-plan preconditions before journaling; backend checks
                // again globally and immediately before each native write.
                for change in &plan.changes {
                    if super::read_one(&mut self.adapter, &change.location)? != change.before {
                        return Err(failure("association state changed; review a new plan"));
                    }
                }
                let id = self
                    .journal
                    .begin(AssociationReceipt { owner: plan.owner.clone(), remaining: plan.changes.clone() })?;
                // Keep the ORIGINAL full receipt even if backend apply attempts
                // automatic rollback. Its in-memory partial receipt is not a
                // substitute for a synced journal after a crash.
                super::apply(&mut self.adapter, &plan).map_err(|e| WorkerError::from(e.error))?;
                Ok(Response::Applied { journal_id: id })
            }
            Request::Undo { journal_id } => {
                if self.journal.poisoned {
                    return Err(failure("journal persistence failed; restart and inspect before Undo"));
                }
                let record = self
                    .journal
                    .active
                    .as_ref()
                    .filter(|r| r.id == journal_id)
                    .ok_or_else(|| failure("no matching pending journal receipt"))?;
                let mut receipt = record.receipt.clone();
                let errors = super::rollback(&mut self.adapter, &mut receipt);
                if !errors.is_empty() || !receipt.is_rolled_back() {
                    return Err(failure(format!(
                        "Undo incomplete: {}",
                        errors.iter().map(ToString::to_string).collect::<Vec<_>>().join("; ")
                    )));
                }
                self.adapter.sync_restored(record.receipt.pending())?;
                self.journal.retire()?;
                self.pending = None;
                Ok(Response::Undone { journal_id })
            }
            Request::Status => {
                let recovery = self.journal.active.as_ref().map(|r| RecoveryStatus {
                    journal_id: r.id,
                    receipt: r.receipt.clone(),
                    current: r
                        .receipt
                        .pending()
                        .iter()
                        .map(|c| match super::read_one(&mut self.adapter, &c.location) {
                            Ok(v) if v == c.before => CurrentState::Before,
                            Ok(v) if v == c.after => CurrentState::Applied,
                            Ok(_) => CurrentState::Conflict,
                            Err(e) => CurrentState::Unreadable(e.to_string()),
                        })
                        .collect(),
                });
                Ok(Response::Status(Status {
                    pending_plan: self.pending.as_ref().map(|(t, _)| *t),
                    recovery,
                    transactions_left: MAX_JOURNAL_TRANSACTIONS.saturating_sub(self.journal.last),
                }))
            }
        }
    }
    fn reviewed(&self, token: PlanToken) -> Result<&AssociationPlan, WorkerError> {
        self.pending
            .as_ref()
            .filter(|(t, _)| *t == token)
            .map(|(_, p)| p)
            .ok_or_else(|| failure("review token is stale, cancelled, consumed or belongs to another worker"))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    version: u32,
    id: u64,
    receipt: AssociationReceipt,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepairRecord {
    version: u32,
    id: u64,
    owner: String,
    // Before here is the preceding After, never the user's original Before.
    changes: super::SharedChanges,
}
struct Journal {
    directory: PathBuf,
    _lock: File,
    active: Option<Intent>,
    last: u64,
    poisoned: bool,
    repairs: Vec<(u64, u64)>,
    #[cfg(test)]
    fail_sync: bool,
}
impl Drop for Journal {
    fn drop(&mut self) {
        // Closing one handle need not release the lock while a duplicated or
        // inherited handle still refers to the same open file description.
        let _ = self._lock.unlock();
    }
}
impl Journal {
    fn open(directory: PathBuf) -> Result<Self, WorkerError> {
        let _workspace = super::Admission::acquire(super::WORKSPACE_BYTES)?;
        check_directory(&directory)?;
        let lock_path = directory.join("worker.lock");
        let lock = match open_regular(&lock_path, true) {
            Ok(lock) => lock,
            Err(e) if e.kind() == io::ErrorKind::NotFound => private_new(&lock_path)?,
            Err(e) => return Err(e.into()),
        };
        lock.try_lock().map_err(|e| failure(format!("association journal is already in use: {e}")))?;
        let mut journal = Self {
            directory,
            _lock: lock,
            active: None,
            last: 0,
            poisoned: false,
            repairs: Vec::new(),
            #[cfg(test)]
            fail_sync: false,
        };
        let mut ids = Vec::new();
        let mut retired = Vec::new();
        for (count, entry) in fs::read_dir(&journal.directory)?.enumerate() {
            if count >= MAX_JOURNAL_ENTRIES {
                return Err(failure("journal directory exceeds record limit"));
            }
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_str().ok_or_else(|| failure("unexpected journal entry"))?;
            if name == "worker.lock" {
                continue;
            }
            let (stem, suffix) = name.split_once('.').ok_or_else(|| failure("unexpected journal entry"))?;
            if stem.len() != 20 || !stem.bytes().all(|c| c.is_ascii_digit()) {
                return Err(failure("invalid journal record name"));
            }
            let id: u64 = stem.parse().map_err(|_| failure("invalid journal id"))?;
            if id == 0 || id > MAX_JOURNAL_TRANSACTIONS {
                return Err(failure("journal id exceeds limit"));
            }
            journal.last = journal.last.max(id);
            match suffix {
                "intent" => ids.push(id),
                "retired" => {
                    let bytes = read_record(&entry.path(), 0)?;
                    debug_assert!(bytes.is_empty());
                    retired.push(id);
                }
                suffix if suffix.starts_with("repair-") => {
                    let sequence = &suffix[7..];
                    if sequence.len() != 8 || !sequence.bytes().all(|b| b.is_ascii_digit()) {
                        return Err(failure("invalid repair record name"));
                    }
                    let sequence: u64 = sequence.parse().map_err(|_| failure("invalid repair sequence"))?;
                    if sequence == 0
                        || sequence > MAX_REPAIR_RECORDS as u64
                        || journal.repairs.len() >= MAX_REPAIR_RECORDS
                    {
                        return Err(failure("repair history exceeds limit"));
                    }
                    journal.repairs.push((id, sequence));
                }
                _ => return Err(failure("unexpected journal record kind")),
            }
        }
        if retired.iter().any(|id| !ids.contains(id)) {
            return Err(failure("retirement marker has no intent"));
        }
        if journal.repairs.iter().any(|(id, _)| !ids.contains(id)) {
            return Err(failure("repair record has no original intent"));
        }
        journal.repairs.sort_unstable();
        ids.sort_unstable();
        for id in ids {
            // Check even retired files: never follow special files or hide corruption.
            let bytes = read_record(&journal.path(id, "intent"), MAX_RECORD_BYTES)?;
            let mut intent: Intent =
                serde_json::from_slice(&bytes).map_err(|e| failure(format!("invalid journal intent {id}: {e}")))?;
            if intent.version != 1 || intent.id != id {
                return Err(failure("journal version/id mismatch"));
            }
            validate_receipt(&intent.receipt)?;
            for (index, (_, sequence)) in journal.repairs.iter().filter(|(owner, _)| *owner == id).enumerate() {
                if *sequence != index as u64 + 1 {
                    return Err(failure("repair history has a gap"));
                }
                let repair = journal.read_repair(id, *sequence)?;
                advance_receipt(&mut intent.receipt, &repair)?;
            }
            if !retired.contains(&id) {
                if journal.active.is_some() {
                    return Err(failure("multiple pending receipts; refusing ambiguous recovery"));
                }
                journal.active = Some(intent);
            }
        }
        Ok(journal)
    }
    fn path(&self, id: u64, suffix: &str) -> PathBuf {
        self.directory.join(format!("{id:020}.{suffix}"))
    }
    fn read_repair(&self, id: u64, sequence: u64) -> Result<RepairRecord, WorkerError> {
        let _workspace = super::Admission::acquire(super::WORKSPACE_BYTES)?;
        let bytes = read_record(&self.path(id, &format!("repair-{sequence:08}")), MAX_RECORD_BYTES)?;
        let repair: RepairRecord =
            serde_json::from_slice(&bytes).map_err(|e| failure(format!("invalid repair record: {e}")))?;
        if repair.version != 1 || repair.id != id {
            return Err(failure("repair version/id mismatch"));
        }
        validate_receipt(&AssociationReceipt { owner: repair.owner.clone(), remaining: repair.changes.clone() })?;
        if repair
            .changes
            .iter()
            .any(|c| !super::windows_paths::repair_location(&c.location) || c.before.is_none() || c.after.is_none())
        {
            return Err(failure("repair record is outside owned command/icon values"));
        }
        Ok(repair)
    }
    fn replay_repair(
        &self,
        adapter: &mut impl AssociationAdapter,
        sequence: u64,
        repair: &RepairRecord,
    ) -> Result<(), WorkerError> {
        if repair.owner != adapter.owner() {
            return Err(failure("repair belongs to another user/backend"));
        }
        // A preceding process may have exited after writing complete bytes but
        // before its flush. Establish durability again before replaying them.
        open_regular(&self.path(repair.id, &format!("repair-{sequence:08}")), true)?.sync_all()?;
        sync_directory(&self.directory)?;
        let mut changed = false;
        for change in repair.changes.iter() {
            if super::read_one(adapter, &change.location)? == change.before {
                adapter.write(&change.location, change.after.as_ref())?;
                changed = true;
            }
        }
        if changed {
            adapter.notify_changes();
        }
        Ok(())
    }
    fn repair_owned(&mut self, adapter: &mut impl AssociationAdapter, executable: &Path) -> Result<(), WorkerError> {
        if let Some(active) = &self.active {
            let id = active.id;
            // Replaying a durable record is idempotent and recognizes both sides
            // of an interrupted backend write; foreign signatures are untouched.
            for (_, sequence) in self.repairs.iter().filter(|(owner, _)| *owner == id) {
                self.replay_repair(adapter, *sequence, &self.read_repair(id, *sequence)?)?;
            }
            let changes =
                super::windows_paths::receipt_repairs(adapter, executable, &self.active.as_ref().unwrap().receipt)?;
            if !changes.is_empty() {
                if self.repairs.len() >= MAX_REPAIR_RECORDS {
                    return Err(failure("repair history is full; new repair refused"));
                }
                let sequence = self.repairs.iter().filter(|(owner, _)| *owner == id).count() as u64 + 1;
                let repair = RepairRecord { version: 1, id, owner: adapter.owner(), changes };
                let bytes = bounded_json(&repair)?;
                self.poisoned = true;
                let mut file = private_new(&self.path(id, &format!("repair-{sequence:08}")))?;
                file.write_all(&bytes)?;
                #[cfg(test)]
                if self.fail_sync {
                    return Err(failure("injected repair sync failure"));
                }
                file.sync_all()?;
                sync_directory(&self.directory)?;
                self.repairs.push((id, sequence));
                advance_receipt(&mut self.active.as_mut().unwrap().receipt, &repair)?;
                self.poisoned = false;
                // The durable After is published before the first hive write.
                self.replay_repair(adapter, sequence, &repair)?;
            }
        }
        super::windows_paths::repair(adapter, executable)?;
        Ok(())
    }
    fn begin(&mut self, receipt: AssociationReceipt) -> Result<u64, WorkerError> {
        if self.poisoned || self.active.is_some() {
            return Err(failure("journal needs recovery before another Apply"));
        }
        let id = self
            .last
            .checked_add(1)
            .filter(|id| *id <= MAX_JOURNAL_TRANSACTIONS)
            .ok_or_else(|| failure("journal history is full; stopped-service maintenance required"))?;
        let intent = Intent { version: 1, id, receipt };
        let bytes = bounded_json(&intent)?;
        self.poisoned = true; // Any partial publication requires restart/inspection.
        self.last = id;
        let mut file = private_new(&self.path(id, "intent"))?;
        // Retain the full receipt in this process even when write/sync fails.
        // No native write can occur unless ALL persistence steps below succeed.
        self.active = Some(intent);
        file.write_all(&bytes)?;
        #[cfg(test)]
        if self.fail_sync {
            return Err(failure("injected journal sync failure"));
        }
        file.sync_all()?;
        sync_directory(&self.directory)?;
        self.poisoned = false;
        Ok(id)
    }
    fn retire(&mut self) -> Result<(), WorkerError> {
        if self.poisoned {
            return Err(failure("journal persistence failed; restart and inspect before retiring"));
        }
        let id = self.active.as_ref().ok_or_else(|| failure("no recovery receipt"))?.id;
        let path = self.path(id, "retired");
        let marker = match private_new(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                let f = open_regular(&path, true)?;
                if f.metadata()?.len() != 0 {
                    return Err(failure("invalid retirement marker"));
                }
                f
            }
            Err(e) => return Err(e.into()),
        };
        // An empty immutable marker can only be created AFTER complete Undo
        // and successful backend durability verification of restored state.
        // A crash before/during sync either retains the original receipt for
        // idempotent retry, or observes this safe retirement. Never rename away.
        marker.sync_all()?;
        sync_directory(&self.directory)?;
        self.active = None;
        Ok(())
    }
}

fn advance_receipt(receipt: &mut AssociationReceipt, repair: &RepairRecord) -> Result<(), WorkerError> {
    if receipt.owner != repair.owner {
        return Err(failure("repair owner does not match original receipt"));
    }
    let (mut changes, admission) = receipt.remaining.copied()?;
    for update in repair.changes.iter() {
        let change = changes
            .iter_mut()
            .find(|change| change.location == update.location)
            .ok_or_else(|| failure("repair location is absent from original receipt"))?;
        if change.after != update.before {
            return Err(failure("repair history does not match preceding After"));
        }
        change.after = update.after.clone();
    }
    receipt.remaining = super::SharedChanges::admitted(changes, admission);
    Ok(())
}

pub(super) fn validate_plan(plan: &AssociationPlan) -> Result<(), WorkerError> {
    if plan.owner.is_empty() || plan.owner.len() > MAX_PATH {
        return Err(failure("plan owner exceeds bounds"));
    }
    let receipt = AssociationReceipt { owner: plan.owner.clone(), remaining: plan.changes.clone() };
    validate_receipt(&receipt)?;
    json_size(plan)?;
    // The complete durable envelope, not just the change list, must fit before
    // planning returns or any byte copies/native writes are attempted.
    #[derive(Serialize)]
    struct Envelope<'a> {
        version: u32,
        id: u64,
        receipt: &'a AssociationReceipt,
    }
    json_size(&Envelope { version: 1, id: MAX_JOURNAL_TRANSACTIONS, receipt: &receipt })?;
    Ok(())
}
pub(super) fn validate_receipt(receipt: &AssociationReceipt) -> Result<(), WorkerError> {
    use super::Location;
    if receipt.owner.is_empty() || receipt.owner.len() > MAX_PATH || receipt.remaining.len() > super::MAX_CHANGES {
        return Err(failure("receipt exceeds bounds"));
    }
    for (index, c) in receipt.remaining.iter().enumerate() {
        if receipt.remaining[..index].iter().any(|earlier| earlier.location == c.location) {
            return Err(failure("duplicate receipt location"));
        }
        let location_ok = match &c.location {
            Location::WindowsValue { subkey, name } => subkey.len() <= MAX_PATH && name.len() <= MAX_PATH,
            Location::UserFile(path) => path.is_absolute() && bounded_path(path),
            Location::MacRole { uti, .. } => uti.len() <= 255,
        };
        if !location_ok || c.before.iter().chain(c.after.iter()).any(|v| v.bytes.len() > super::MAX_VALUE_BYTES) {
            return Err(failure("receipt location/value exceeds bounds"));
        }
    }
    json_size(receipt)?;
    Ok(())
}
/// Count escaped strings, byte-array digits, separators and envelope exactly,
/// with no output buffer. Denial happens before reserve/serialization growth.
fn json_size(value: &impl Serialize) -> Result<usize, WorkerError> {
    struct Counter(usize);
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > MAX_RECORD_BYTES.saturating_sub(self.0) {
                return Err(io::Error::other("association record exceeds size limit"));
            }
            self.0 += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut count = Counter(0);
    serde_json::to_writer(&mut count, value).map_err(|e| failure(e.to_string()))?;
    Ok(count.0)
}
#[derive(Debug)]
struct RecordBytes {
    bytes: Vec<u8>,
    _admission: super::Admission,
}
#[cfg(test)]
static RECORD_ALLOCATION_ATTEMPTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
impl std::ops::Deref for RecordBytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.bytes
    }
}
fn bounded_json(value: &impl Serialize) -> Result<RecordBytes, WorkerError> {
    let length = json_size(value)?;
    let admission = super::Admission::acquire(length)?;
    struct Bounded {
        bytes: Vec<u8>,
        length: usize,
    }
    impl Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.length.saturating_sub(self.bytes.len()) {
                return Err(io::Error::other("association serialization changed after preflight"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut out = Bounded { bytes: Vec::new(), length };
    #[cfg(test)]
    RECORD_ALLOCATION_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
    out.bytes.try_reserve_exact(length).map_err(|_| failure("record allocation failed"))?;
    serde_json::to_writer(&mut out, value).map_err(|e| failure(e.to_string()))?;
    if out.bytes.len() != length {
        return Err(failure("association serialization changed after preflight"));
    }
    Ok(RecordBytes { bytes: out.bytes, _admission: admission })
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
/// Pure traversal shared with path fixtures: accumulate a Windows volume/UNC
/// prefix, but do not probe it until RootDir makes it a rooted directory.
fn directory_steps(directory: &Path, mut inspect: impl FnMut(&Path) -> io::Result<()>) -> io::Result<()> {
    if !directory.is_absolute() || !bounded_path(directory) {
        return Err(invalid("journal requires a bounded absolute private directory"));
    }
    let mut part = PathBuf::new();
    for component in directory.components() {
        if matches!(component, Component::ParentDir | Component::CurDir) {
            return Err(invalid("journal path must be normalized"));
        }
        part.push(component);
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        inspect(&part)?;
    }
    Ok(())
}
fn check_directory(directory: &Path) -> io::Result<()> {
    directory_steps(directory, |part| {
        let m = fs::symlink_metadata(part)?;
        if !m.is_dir() || is_link(&m) {
            return Err(invalid("journal ancestors must be real directories"));
        }
        Ok(())
    })?;
    let m = fs::symlink_metadata(directory)?;
    private_metadata(&m, true)
}
fn is_link(m: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        m.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        m.file_type().is_symlink()
    }
}
fn private_metadata(m: &fs::Metadata, directory: bool) -> io::Result<()> {
    if is_link(m) || if directory { !m.is_dir() } else { !m.is_file() } {
        return Err(invalid("journal path is not a regular private file/directory"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o077 != 0 || (!directory && m.nlink() != 1) {
            return Err(invalid("journal requires privately owned files/directories without hard links"));
        }
    }
    Ok(())
}
fn options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_WRITE_THROUGH};
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_WRITE_THROUGH);
    }
    options
}
fn open_regular(path: &Path, write: bool) -> io::Result<File> {
    // Refuse FIFOs/devices/symlinks before open can block. Unix flags also guard
    // a final-component swap; verify the opened descriptor, not just the path.
    private_metadata(&fs::symlink_metadata(path)?, false)?;
    let file = options().write(write).open(path)?;
    private_metadata(&file.metadata()?, false)?;
    Ok(file)
}
fn private_new(path: &Path) -> io::Result<File> {
    let file = options().write(true).create_new(true).open(path)?;
    private_metadata(&file.metadata()?, false)?;
    Ok(file)
}
fn read_record(path: &Path, limit: usize) -> Result<RecordBytes, WorkerError> {
    let mut file = open_regular(path, false)?;
    let length = file.metadata()?.len();
    if length > limit as u64 || length > MAX_RECORD_BYTES as u64 {
        return Err(failure("journal record exceeds size limit"));
    }
    let length = length as usize;
    let admission = super::Admission::acquire(length)?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(length).map_err(|_| failure("record allocation failed"))?;
    bytes.resize(length, 0);
    file.read_exact(&mut bytes)?;
    let mut extra = [0u8; 1];
    if file.read(&mut extra)? != 0 {
        return Err(failure("journal record grew beyond size limit"));
    }
    Ok(RecordBytes { bytes, _admission: admission })
}
fn sync_directory(directory: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(directory)?.sync_all()
    }
    // Windows records use CREATE_NEW + WRITE_THROUGH + FlushFileBuffers via
    // sync_all. There is no rename/replace metadata publication step.
    #[cfg(not(unix))]
    {
        let _ = directory;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::{AssociationPlatform, DefaultChoice, FileType, Location, StoredValue};
    use super::*;
    use std::{sync::mpsc, time::Duration};

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "kwil-association-worker-{}-{}-{}",
                std::process::id(),
                SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let mut builder = fs::DirBuilder::new();
            builder.recursive(false);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(&path).unwrap();
            #[cfg(unix)]
            let path = path.canonicalize().unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
    #[derive(Default)]
    struct FakeState {
        repair_target: Option<PathBuf>,
        values: Vec<(Location, StoredValue)>,
        writes: usize,
        reads: usize,
        expected_changes: Option<usize>,
        fail_from: Option<usize>,
        panic_at: Option<usize>,
        durable: Option<PathBuf>,
        durable_values: Vec<(Location, StoredValue)>,
        fail_barrier: bool,
        crash_at_barrier: bool,
        barriers: usize,
    }
    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<FakeState>>);
    impl AssociationAdapter for Fake {
        fn windows_registration_keys(&mut self) -> Result<Vec<String>, AssociationError> {
            Ok(if self.0.lock().unwrap().repair_target.is_some() { vec!["Kwil.txt".into()] } else { Vec::new() })
        }
        fn repair_executable(&self) -> Option<PathBuf> {
            self.0.lock().unwrap().repair_target.clone()
        }
        fn owner(&self) -> String {
            "isolated-fake-user".into()
        }
        fn read(&mut self, location: &Location) -> Result<Option<StoredValue>, AssociationError> {
            Ok(self.0.lock().unwrap().values.iter().find(|(k, _)| k == location).map(|(_, v)| v.clone()))
        }
        fn read_bounded(
            &mut self,
            location: &Location,
            budget: &mut super::super::ValueBudget,
        ) -> Result<Option<StoredValue>, AssociationError> {
            let mut state = self.0.lock().unwrap();
            state.reads += 1;
            state.values.iter().find(|(k, _)| k == location).map(|(_, v)| budget.copy(v.kind, &v.bytes)).transpose()
        }
        fn write(&mut self, location: &Location, value: Option<&StoredValue>) -> Result<(), AssociationError> {
            let mut s = self.0.lock().unwrap();
            if let Some(path) = &s.durable {
                let bytes =
                    read_record(path, MAX_RECORD_BYTES).expect("intent must be readable before EVERY native write");
                let intent: Intent = serde_json::from_slice(&bytes).unwrap();
                assert!(intent.receipt.pending().iter().any(|c| c.location == *location));
                assert_eq!(
                    intent.receipt.pending().len(),
                    s.expected_changes.unwrap_or(3),
                    "full planned receipt, not a partial result"
                );
            }
            s.writes += 1;
            if s.panic_at == Some(s.writes) {
                drop(s); // Fake panic must not poison our test's values lock.
                panic!("simulated process interruption before next write");
            }
            if s.fail_from.is_some_and(|n| s.writes >= n) {
                return Err(super::super::err("injected write/rollback failure"));
            }
            s.values.retain(|(k, _)| k != location);
            if let Some(v) = value {
                s.values.push((location.clone(), v.clone()));
            }
            Ok(())
        }
        fn sync_restored(&mut self, changes: &[super::super::Change]) -> Result<(), AssociationError> {
            let mut s = self.0.lock().unwrap();
            s.barriers += 1;
            assert_eq!(changes.len(), s.expected_changes.unwrap_or(3), "barrier must cover the original full receipt");
            for change in changes {
                assert_eq!(
                    s.values.iter().find(|(l, _)| *l == change.location).map(|(_, v)| v),
                    change.before.as_ref()
                );
            }
            if s.crash_at_barrier {
                s.values = s.durable_values.clone();
                drop(s);
                panic!("simulated crash before backend durability barrier");
            }
            if s.fail_barrier {
                return Err(super::super::err("injected backend durability barrier failure"));
            }
            s.durable_values = s.values.clone();
            Ok(())
        }
    }
    fn request() -> AssociationRequest {
        AssociationRequest {
            platform: AssociationPlatform::Windows,
            executable: std::env::temp_dir().join("isolated-kwil.exe"),
            types: vec![FileType { extension: "txt".into(), content_type: None }],
            default_choice: DefaultChoice::KeepCurrent,
        }
    }
    fn engine(scratch: &Scratch, fake: &Fake) -> Engine<Fake> {
        Engine::new(fake.clone(), Journal::open(scratch.0.clone()).unwrap()).unwrap()
    }
    #[test]
    fn explorer_worker_startup_repairs_legacy_receipt_and_unregisters_after_restart() {
        let _guard = crate::associations::test_admission_guard();
        let scratch = Scratch::new();
        let fake = Fake::default();
        let location = Location::WindowsValue { subkey: r"Kwil.txt\shell\open\command".into(), name: String::new() };
        let old = super::super::ValueBudget::new(super::super::MAX_VALUE_BYTES)
            .registry(r#""\\?\E:\old\kwil-custom-build.exe" "%1""#)
            .unwrap();
        let receipt = AssociationReceipt {
            owner: fake.owner(),
            remaining: super::super::SharedChanges::admitted(
                vec![super::super::Change { location: location.clone(), before: None, after: Some(old.clone()) }],
                super::super::Admission::acquire(super::super::WORKSPACE_BYTES).unwrap(),
            ),
        };
        let mut journal = Journal::open(scratch.0.clone()).unwrap();
        journal.begin(receipt).unwrap();
        let original = fs::read(journal.path(1, "intent")).unwrap();
        drop(journal);
        {
            let mut state = fake.0.lock().unwrap();
            state.values.push((location, old));
            state.repair_target = Some(PathBuf::from(r"E:\new\kwil.exe"));
            state.expected_changes = Some(1);
        }
        let mut first = engine(&scratch, &fake);
        assert_eq!(status(&mut first).recovery.unwrap().current, vec![CurrentState::Applied]);
        assert_eq!(fake.0.lock().unwrap().writes, 1);
        drop(first);
        let mut resumed = engine(&scratch, &fake);
        assert_eq!(status(&mut resumed).recovery.unwrap().current, vec![CurrentState::Applied]);
        resumed.execute(Request::Undo { journal_id: 1 }).unwrap();
        assert!(fake.0.lock().unwrap().values.is_empty());
        assert_eq!(fs::read(resumed.journal.path(1, "intent")).unwrap(), original);
    }
    fn planned(engine: &mut Engine<Fake>) -> (PlanToken, AssociationPlan) {
        match engine.execute(Request::Plan(request())).unwrap() {
            Response::Planned { token, plan } => (token, plan),
            other => panic!("unexpected {other:?}"),
        }
    }
    fn seed_repair_receipt(scratch: &Scratch, fake: &Fake, name: &str) -> (Location, StoredValue) {
        let location = Location::WindowsValue { subkey: r"Kwil.txt\shell\open\command".into(), name: String::new() };
        let previous = StoredValue::text("exact previous foreign handler");
        let missing = scratch.0.join("missing-A").join(name);
        let old = super::super::ValueBudget::new(super::super::MAX_VALUE_BYTES)
            .registry(&format!("\"{}\" \"%1\"", missing.display()))
            .unwrap();
        let receipt = AssociationReceipt {
            owner: fake.owner(),
            remaining: super::super::SharedChanges::admitted(
                vec![super::super::Change {
                    location: location.clone(),
                    before: Some(previous.clone()),
                    after: Some(old.clone()),
                }],
                super::super::Admission::acquire(super::super::WORKSPACE_BYTES).unwrap(),
            ),
        };
        Journal::open(scratch.0.clone()).unwrap().begin(receipt).unwrap();
        let mut state = fake.0.lock().unwrap();
        state.values.push((location.clone(), old));
        state.expected_changes = Some(1);
        (location, previous)
    }
    #[test]
    fn review_repaired_a_to_b_then_launch_c_preserves_unregister_ownership() {
        let _guard = crate::associations::test_admission_guard();
        for interruption in ["none", "flush", "backend"] {
            let scratch = Scratch::new();
            let fake = Fake::default();
            let (location, previous) = seed_repair_receipt(&scratch, &fake, "kwil.exe");
            let original = fs::read(scratch.0.join("00000000000000000001.intent")).unwrap();
            let binaries = Scratch::new();
            let b = binaries.0.join("B-kwil.exe");
            fs::write(&b, b"existing B").unwrap();
            fake.0.lock().unwrap().repair_target = Some(b.clone());
            if interruption == "none" {
                drop(engine(&scratch, &fake));
            } else {
                let mut journal = Journal::open(scratch.0.clone()).unwrap();
                journal.fail_sync = interruption == "flush";
                fake.0.lock().unwrap().fail_from = (interruption == "backend").then_some(1);
                assert!(journal.repair_owned(&mut fake.clone(), &b).is_err());
                assert_eq!(fake.0.lock().unwrap().writes, usize::from(interruption == "backend"));
                assert!(journal.path(1, "repair-00000001").is_file());
            }
            fake.0.lock().unwrap().fail_from = None;
            fake.0.lock().unwrap().repair_target = Some(binaries.0.join("C-kwil.exe"));
            let mut resumed = engine(&scratch, &fake);
            assert_eq!(status(&mut resumed).recovery.unwrap().current, vec![CurrentState::Applied]);
            resumed.execute(Request::Undo { journal_id: 1 }).unwrap();
            assert_eq!(fake.0.lock().unwrap().values, vec![(location, previous)]);
            assert_eq!(fs::read(resumed.journal.path(1, "intent")).unwrap(), original);
        }
    }
    #[test]
    fn review_background_startup_repairs_receipt_owned_custom_executable_without_settings_page() {
        let _guard = crate::associations::test_admission_guard();
        let scratch = Scratch::new();
        let fake = Fake::default();
        let (location, previous) = seed_repair_receipt(&scratch, &fake, "kwil-custom-build.exe");
        let target = scratch.0.join("current-kwil.exe");
        fake.0.lock().unwrap().repair_target = Some(target.clone());
        let lock = Journal::open(scratch.0.clone()).unwrap();
        assert!(
            repair_startup::<Fake, _>(scratch.0.clone(), || panic!("locked journal must refuse before factory"))
                .is_err()
        );
        assert_eq!(fake.0.lock().unwrap().writes, 0);
        drop(lock);
        let factory = fake.clone();
        repair_startup(scratch.0.clone(), move || Ok(factory)).unwrap();
        let expected = super::super::ValueBudget::new(super::super::MAX_VALUE_BYTES)
            .registry(&format!("\"{}\" \"%1\"", target.display()))
            .unwrap();
        assert_eq!(fake.0.lock().unwrap().values, vec![(location.clone(), expected)]);
        let mut resumed = engine(&scratch, &fake);
        resumed.execute(Request::Undo { journal_id: 1 }).unwrap();
        assert_eq!(fake.0.lock().unwrap().values, vec![(location, previous)]);
    }
    fn status(engine: &mut Engine<Fake>) -> Status {
        match engine.execute(Request::Status).unwrap() {
            Response::Status(status) => status,
            other => panic!("unexpected {other:?}"),
        }
    }
    fn receive(events: &Events, wake: &mpsc::Receiver<()>) -> Event {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(event) = events.try_recv() {
                return event;
            }
            // A previous event may have been consumed just before its wake was
            // posted. Ignore that stale notification; always wait, never spin.
            wake.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                .expect("bounded worker completion");
        }
    }

    fn applied_durability_fixture(scratch: &Scratch, fake: &Fake) -> (Engine<Fake>, AssociationPlan) {
        let mut e = engine(scratch, fake);
        let (_, baseline) = planned(&mut e);
        // Two prior typed values plus one absent value exercise both replacement
        // and removal. Simulate the OS independently flushing the applied state.
        fake.0.lock().unwrap().values = baseline.changes[..2]
            .iter()
            .enumerate()
            .map(|(i, c)| (c.location.clone(), StoredValue { kind: 20 + i as u32, bytes: vec![0, 255, i as u8] }))
            .collect();
        let (token, plan) = planned(&mut e);
        e.execute(Request::Apply { token }).unwrap();
        let mut s = fake.0.lock().unwrap();
        s.durable_values = s.values.clone();
        (e, plan)
    }

    #[test]
    fn budget_all240_and_256_use_one_original_durable_intent_and_exact_undo() {
        let _admission_case = crate::associations::test_admission_guard();
        for count in [240, 256] {
            let scratch = Scratch::new();
            let fake = Fake::default();
            let mut e = engine(&scratch, &fake);
            let mut request = super::super::budget_tests::all240_request();
            for n in 240..count {
                request.types.push(FileType { extension: format!("extra{n}"), content_type: None });
            }
            let expected = count * 3;
            let seed = match e.execute(Request::Plan(request.clone())).unwrap() {
                Response::Planned { plan, .. } => plan,
                other => panic!("unexpected {other:?}"),
            };
            let originals: Vec<_> = seed
                .changes()
                .iter()
                .enumerate()
                .filter(|(i, _)| i % 3 != 2)
                .map(|(i, c)| {
                    (c.location.clone(), StoredValue { kind: 60 + i as u32, bytes: vec![0, 255, i as u8, 0, 128] })
                })
                .collect();
            {
                let mut state = fake.0.lock().unwrap();
                state.expected_changes = Some(expected);
                state.values = originals.clone();
                state.durable = Some(e.journal.path(1, "intent"));
            }
            drop(seed);
            let (token, reviewed) = match e.execute(Request::Plan(request)).unwrap() {
                Response::Planned { token, plan } => (token, plan),
                other => panic!("unexpected {other:?}"),
            };
            assert_eq!(reviewed.changes().len(), expected);
            assert!(matches!(e.execute(Request::Apply { token }).unwrap(), Response::Applied { journal_id: 1 }));
            assert_eq!(fake.0.lock().unwrap().writes, expected);
            assert_eq!(e.journal.last, 1, "one transaction, never split batches");
            assert_eq!(e.journal.active.as_ref().unwrap().receipt.pending(), reviewed.changes());
            let intent_bytes = fs::read(e.journal.path(1, "intent")).unwrap();
            let intent: Intent = serde_json::from_slice(&intent_bytes).unwrap();
            assert_eq!(intent.receipt.pending(), reviewed.changes());
            drop(intent);
            drop(e);
            let mut e = engine(&scratch, &fake);
            assert_eq!(status(&mut e).recovery.unwrap().receipt.pending().len(), expected);
            assert!(matches!(e.execute(Request::Undo { journal_id: 1 }).unwrap(), Response::Undone { journal_id: 1 }));
            assert_eq!(fake.0.lock().unwrap().barriers, 1);
            assert!(e.journal.active.is_none());
            assert_eq!(fs::read(e.journal.path(1, "intent")).unwrap(), intent_bytes, "original receipt is immutable");
            assert!(e.journal.path(1, "retired").exists());
            let state = fake.0.lock().unwrap();
            assert_eq!(state.values.len(), originals.len());
            for (location, original) in originals {
                assert_eq!(state.values.iter().find(|(p, _)| *p == location).map(|(_, v)| v), Some(&original));
            }
        }
    }

    #[test]
    fn budget_all240_failed_apply_and_cleanup_keep_full_durable_recovery() {
        let _admission_case = crate::associations::test_admission_guard();
        let scratch = Scratch::new();
        let fake = Fake::default();
        let mut e = engine(&scratch, &fake);
        let (token, plan) = match e.execute(Request::Plan(super::super::budget_tests::all240_request())).unwrap() {
            Response::Planned { token, plan } => (token, plan),
            other => panic!("unexpected {other:?}"),
        };
        {
            let mut state = fake.0.lock().unwrap();
            state.expected_changes = Some(720);
            state.durable = Some(e.journal.path(1, "intent"));
            state.fail_from = Some(2);
        }
        let failure = e.execute(Request::Apply { token }).unwrap_err();
        assert_eq!(failure.recovery, Some(1));
        assert_eq!(fake.0.lock().unwrap().values.len(), 1);
        assert_eq!(e.journal.active.as_ref().unwrap().receipt.pending(), plan.changes());
        assert!(!e.journal.path(1, "retired").exists());
        let bytes = fs::read(e.journal.path(1, "intent")).unwrap();
        drop(e);
        fake.0.lock().unwrap().fail_from = None;
        let mut e = engine(&scratch, &fake);
        e.execute(Request::Undo { journal_id: 1 }).unwrap();
        assert!(fake.0.lock().unwrap().values.is_empty());
        assert_eq!(fs::read(e.journal.path(1, "intent")).unwrap(), bytes);
        assert_eq!(e.journal.last, 1);
    }

    #[test]
    fn budget_worker_257_refuses_before_reads_or_intent() {
        let _admission_case = crate::associations::test_admission_guard();
        let scratch = Scratch::new();
        let fake = Fake::default();
        let mut e = engine(&scratch, &fake);
        let mut request = super::super::budget_tests::all240_request();
        for n in 240..257 {
            request.types.push(FileType { extension: format!("extra{n}"), content_type: None });
        }
        assert!(!bounded_request(&request));
        assert!(e.execute(Request::Plan(request)).is_err());
        assert_eq!((fake.0.lock().unwrap().reads, e.journal.last), (0, 0));
        assert!(!e.journal.path(1, "intent").exists());
    }

    #[test]
    fn budget_serializer_16mib_preflight_denies_before_output_allocation() {
        let _admission_case = crate::associations::test_admission_guard();
        // JSON byte-array digits expand 4 MiB + 1 bytes beyond a 16 MiB
        // serialized record. The input is generated at runtime, never checked in.
        let value = vec![255u8; MAX_RECORD_BYTES / 4 + 1];
        let before = RECORD_ALLOCATION_ATTEMPTS.load(Ordering::Relaxed);
        assert!(bounded_json(&value).is_err());
        assert_eq!(RECORD_ALLOCATION_ATTEMPTS.load(Ordering::Relaxed), before);
        let at_limit = "x".repeat(MAX_RECORD_BYTES - 2); // quotes are part of the cap
        let encoded = bounded_json(&at_limit).unwrap();
        assert_eq!(encoded.len(), MAX_RECORD_BYTES);
        assert_eq!(RECORD_ALLOCATION_ATTEMPTS.load(Ordering::Relaxed), before + 1);
        drop(encoded);
        let too_large = "x".repeat(MAX_RECORD_BYTES - 1);
        assert!(bounded_json(&too_large).is_err());
        assert_eq!(RECORD_ALLOCATION_ATTEMPTS.load(Ordering::Relaxed), before + 1);
    }

    #[cfg(windows)]
    #[test]
    fn budget_directory_steps_canonical_windows_prefix_waits_for_root() {
        let _admission_case = crate::associations::test_admission_guard();
        let path = Path::new(r"\\?\C:\Users\example\AppData\Roaming\kwil\kwil-associations-v1");
        let mut steps = Vec::new();
        directory_steps(path, |part| {
            steps.push(part.to_owned());
            Ok(())
        })
        .unwrap();
        assert_eq!(steps.len(), 7);
        assert_eq!(steps[0].as_os_str(), std::ffi::OsStr::new(r"\\?\C:\"));
        assert_eq!(steps.last().unwrap(), path);
        assert!(!steps.iter().any(|part| part.as_os_str() == std::ffi::OsStr::new(r"\\?\C:")));
        for part in &steps {
            assert!(part.is_absolute());
        }
    }

    #[cfg(windows)]
    #[test]
    fn budget_directory_steps_drive_and_unc_keep_every_actual_ancestor() {
        let _admission_case = crate::associations::test_admission_guard();
        for (path, root) in [
            (r"C:\kwil\journal", r"C:\"),
            (r"\\server\share\kwil\journal", r"\\server\share\"),
            (r"\\?\UNC\server\share\kwil\journal", r"\\?\UNC\server\share\"),
        ] {
            let mut steps = Vec::new();
            directory_steps(Path::new(path), |part| {
                steps.push(part.to_owned());
                Ok(())
            })
            .unwrap();
            assert_eq!(steps.len(), 3);
            assert_eq!(steps[0].as_os_str(), std::ffi::OsStr::new(root));
            assert_eq!(steps[1], Path::new(root).join("kwil"));
            assert_eq!(steps[2], Path::new(path));
        }
    }

    #[cfg(windows)]
    #[test]
    fn budget_directory_steps_relative_and_parent_refusals_are_preserved() {
        let _admission_case = crate::associations::test_admission_guard();
        for path in [r"journal", r"C:journal", r"\journal"] {
            let mut probes = 0;
            assert!(
                directory_steps(Path::new(path), |_| {
                    probes += 1;
                    Ok(())
                })
                .is_err()
            );
            assert_eq!(probes, 0);
        }
        assert!(directory_steps(Path::new(r"\\?\C:\kwil\..\journal"), |_| Ok(())).is_err());
    }

    #[test]
    fn undo_makes_restored_values_and_removals_durable_before_retirement() {
        let _admission_case = crate::associations::test_admission_guard();
        let scratch = Scratch::new();
        let fake = Fake::default();
        let (mut e, plan) = applied_durability_fixture(&scratch, &fake);
        e.execute(Request::Undo { journal_id: 1 }).unwrap();
        {
            let mut s = fake.0.lock().unwrap();
            // Crash discards visible, unflushed state; only durable state survives.
            s.values = s.durable_values.clone();
            for change in plan.changes() {
                assert_eq!(
                    s.values.iter().find(|(l, _)| *l == change.location).map(|(_, v)| v),
                    change.before.as_ref(),
                    "retirement must never outlive durable restoration"
                );
            }
            assert_eq!(s.barriers, 1);
        }
        drop(e);
        assert!(status(&mut engine(&scratch, &fake)).recovery.is_none());
    }

    #[test]
    fn failed_backend_barrier_keeps_full_recovery_and_retries_already_visible_before() {
        let _admission_case = crate::associations::test_admission_guard();
        let scratch = Scratch::new();
        let fake = Fake::default();
        let (mut e, _) = applied_durability_fixture(&scratch, &fake);
        let intent = fs::read(e.journal.path(1, "intent")).unwrap();
        fake.0.lock().unwrap().fail_barrier = true;
        let error = e.execute(Request::Undo { journal_id: 1 }).unwrap_err();
        assert_eq!(error.recovery, Some(1));
        assert!(error.message.contains("durability barrier failure"));
        assert!(!e.journal.path(1, "retired").exists(), "failed barrier must precede marker creation");
        assert_eq!(fs::read(e.journal.path(1, "intent")).unwrap(), intent);
        let recovery = status(&mut e).recovery.unwrap();
        assert_eq!(recovery.receipt.pending().len(), 3);
        assert!(recovery.current.iter().all(|c| *c == CurrentState::Before));
        assert!(e.execute(Request::Plan(request())).is_err());
        drop(e);
        fake.0.lock().unwrap().fail_barrier = false;
        let writes = fake.0.lock().unwrap().writes;
        let mut e = engine(&scratch, &fake);
        e.execute(Request::Undo { journal_id: 1 }).unwrap();
        let s = fake.0.lock().unwrap();
        assert_eq!(s.writes, writes, "already-visible Before needs a barrier, not another write");
        assert_eq!(s.barriers, 2);
        assert_eq!(s.durable_values, s.values);
    }

    #[test]
    fn crash_before_backend_barrier_preserves_receipt_for_applied_state_on_restart() {
        let _admission_case = crate::associations::test_admission_guard();
        let scratch = Scratch::new();
        let fake = Fake::default();
        let (mut e, _) = applied_durability_fixture(&scratch, &fake);
        fake.0.lock().unwrap().crash_at_barrier = true;
        let crashed =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| e.execute(Request::Undo { journal_id: 1 })));
        assert!(crashed.is_err(), "fault must reach the barrier before retirement");
        assert!(!e.journal.path(1, "retired").exists());
        drop(e);
        fake.0.lock().unwrap().crash_at_barrier = false;
        let mut e = engine(&scratch, &fake);
        let recovery = status(&mut e).recovery.unwrap();
        assert_eq!(recovery.receipt.pending().len(), 3);
        assert!(recovery.current.iter().all(|c| *c == CurrentState::Applied));
        e.execute(Request::Undo { journal_id: 1 }).unwrap();
        assert!(status(&mut e).recovery.is_none());
    }

    #[test]
    fn unsupported_backend_durability_never_retires_recovery() {
        let _admission_case = crate::associations::test_admission_guard();
        // Intentionally inherit the safe default instead of a fake success.
        struct Unverified(Fake);
        impl AssociationAdapter for Unverified {
            fn owner(&self) -> String {
                self.0.owner()
            }
            fn read(&mut self, l: &Location) -> Result<Option<StoredValue>, AssociationError> {
                self.0.read(l)
            }
            fn read_bounded(
                &mut self,
                l: &Location,
                b: &mut super::super::ValueBudget,
            ) -> Result<Option<StoredValue>, AssociationError> {
                self.0.read_bounded(l, b)
            }
            fn write(&mut self, l: &Location, v: Option<&StoredValue>) -> Result<(), AssociationError> {
                self.0.write(l, v)
            }
        }
        let scratch = Scratch::new();
        let fake = Fake::default();
        let (e, _) = applied_durability_fixture(&scratch, &fake);
        drop(e);
        let mut e = Engine::new(Unverified(fake), Journal::open(scratch.0.clone()).unwrap()).unwrap();
        let error = e.execute(Request::Undo { journal_id: 1 }).unwrap_err();
        assert_eq!(error.recovery, Some(1));
        assert!(error.message.contains("cannot verify durable"));
        assert!(!e.journal.path(1, "retired").exists());
        drop(e);
        assert_eq!(Journal::open(scratch.0.clone()).unwrap().active.as_ref().unwrap().receipt.pending().len(), 3);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_sync_rejects_raced_resources_in_bounded_subprocesses() {
        let _admission_case = crate::associations::test_admission_guard();
        use std::process::{Command, Stdio};
        const CASE: &str = "KWIL_ASSOCIATION_SYNC_TEST_CASE";
        const ROOT: &str = "KWIL_ASSOCIATION_SYNC_TEST_ROOT";
        if let Some(case) = std::env::var_os(CASE) {
            let root = PathBuf::from(std::env::var_os(ROOT).expect("explicit isolated test root"));
            assert!(root.is_absolute());
            assert_eq!(root.parent().unwrap(), std::env::temp_dir().canonicalize().unwrap());
            assert!(root.file_name().unwrap().to_str().unwrap().starts_with("kwil-association-worker-"));
            check_directory(&root).unwrap();
            linux_sync_raced_resource_case(&root, case.to_str().unwrap());
            return;
        }
        for case in [
            "file-fifo",
            "file-symlink",
            "file-directory",
            "ancestor-fifo",
            "ancestor-symlink",
            "regular",
            "missing",
            "missing-parents",
        ] {
            // Parent owns the fixture, so even a killed mutant child is cleaned.
            let scratch = Scratch::new();
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "associations::worker::tests::linux_sync_rejects_raced_resources_in_bounded_subprocesses",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(CASE, case)
                .env(ROOT, &scratch.0)
                .stdin(Stdio::null())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap();
            eprintln!("own association sync subprocess PID {}, case {case}, timeout 5s", child.id());
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        assert!(status.success(), "association sync child {case} failed: {status}");
                        break;
                    }
                    Ok(None) if std::time::Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                    result => {
                        // This exact Child is the only termination target.
                        let _ = child.kill();
                        child.wait().unwrap();
                        panic!("association sync child {case} blocked or failed to wait: {result:?}");
                    }
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn linux_sync_raced_resource_case(root: &Path, case: &str) {
        use super::super::{Change, LinuxAssociations};
        use std::{
            ffi::{CString, OsStr},
            os::{
                fd::AsRawFd,
                unix::{
                    ffi::OsStrExt,
                    fs::{DirBuilderExt, symlink},
                },
            },
        };
        struct Raced {
            fake: Fake,
            native: LinuxAssociations,
            root: PathBuf,
            case: String,
        }
        impl AssociationAdapter for Raced {
            fn owner(&self) -> String {
                self.fake.owner()
            }
            fn read(&mut self, l: &Location) -> Result<Option<StoredValue>, AssociationError> {
                self.fake.read(l)
            }
            fn read_bounded(
                &mut self,
                l: &Location,
                b: &mut super::super::ValueBudget,
            ) -> Result<Option<StoredValue>, AssociationError> {
                self.fake.read_bounded(l, b)
            }
            fn write(&mut self, l: &Location, v: Option<&StoredValue>) -> Result<(), AssociationError> {
                self.fake.write(l, v)
            }
            fn sync_restored(&mut self, changes: &[Change]) -> Result<(), AssociationError> {
                // The adversary replacement occurs AFTER rollback's last read
                // and restoration. No live registry/desktop data is involved.
                let config = self.root.join("config");
                let file = config.join("mimeapps.list");
                let fifo = |path: &Path| {
                    let path = CString::new(path.as_os_str().as_bytes()).unwrap();
                    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
                };
                match self.case.as_str() {
                    "ancestor-fifo" => fifo(&config),
                    "ancestor-symlink" => symlink(self.root.join("target-dir"), &config).unwrap(),
                    "missing-parents" => {}
                    case => {
                        fs::create_dir(&config).unwrap();
                        match case {
                            "file-fifo" => fifo(&file),
                            "file-symlink" => symlink(self.root.join("target"), &file).unwrap(),
                            "file-directory" => fs::create_dir(&file).unwrap(),
                            "regular" => fs::write(&file, b"previous bytes").unwrap(),
                            "missing" => {}
                            _ => panic!("unknown isolated sync case"),
                        }
                    }
                }
                self.native.sync_restored(changes)
            }
        }
        let journal_path = root.join("journal");
        fs::DirBuilder::new().mode(0o700).create(&journal_path).unwrap();
        fs::write(root.join("target"), b"foreign target untouched").unwrap();
        fs::create_dir(root.join("target-dir")).unwrap();
        let location = Location::UserFile(root.join("config/mimeapps.list"));
        let after = StoredValue::text("applied fake value");
        let before = (case == "regular").then(|| StoredValue::text("previous bytes"));
        let fake = Fake::default();
        fake.0.lock().unwrap().values.push((location.clone(), after.clone()));
        let receipt = AssociationReceipt {
            owner: fake.owner(),
            remaining: super::super::SharedChanges::admitted(
                vec![Change { location, before, after: Some(after) }],
                super::super::Admission::acquire(super::super::WORKSPACE_BYTES).unwrap(),
            ),
        };
        let mut journal = Journal::open(journal_path.clone()).unwrap();
        journal.begin(receipt).unwrap();
        let intent = fs::read(journal.path(1, "intent")).unwrap();
        drop(journal);
        let native = LinuxAssociations::new(root.join("data"), root.join("config")).unwrap();
        let adapter = Raced { fake, native, root: root.to_owned(), case: case.into() };
        let (wake_tx, wake_rx) = mpsc::channel();
        let (worker, events) = AssociationWorker::spawn(
            journal_path.clone(),
            move || Ok(adapter),
            move || {
                let _ = wake_tx.send(());
            },
        )
        .unwrap();
        worker.try_submit(Request::Undo { journal_id: 1 }).unwrap();
        let result = receive(&events, &wake_rx).result;
        let should_succeed = matches!(case, "regular" | "missing" | "missing-parents");
        if should_succeed {
            assert!(matches!(result.unwrap(), Response::Undone { journal_id: 1 }));
        } else {
            let error = result.unwrap_err();
            assert_eq!(error.recovery, Some(1));
            assert!(!error.message.contains("panicked"), "must reject the resource, not panic");
        }
        worker.shutdown().unwrap();
        let journal = Journal::open(journal_path).unwrap();
        assert_eq!(journal.active.is_none(), should_succeed);
        assert_eq!(fs::read(journal.path(1, "intent")).unwrap(), intent);
        assert_eq!(journal.path(1, "retired").exists(), should_succeed);
        assert_eq!(fs::read(root.join("target")).unwrap(), b"foreign target untouched");
        // Directory-only open is an admission contract: a FIFO must be refused
        // by openat itself, before acquiring a non-directory descriptor.
        if case == "ancestor-fifo" {
            let parent = File::open(root).unwrap();
            let error = super::super::linux_open_sync_at(parent.as_raw_fd(), OsStr::new("config"), true).unwrap_err();
            assert_eq!(error.raw_os_error(), Some(libc::ENOTDIR));
        }
    }

    #[test]
    fn plan_is_read_only_and_apply_requires_current_reviewed_generation() {
        let _admission_case = crate::associations::test_admission_guard();
        let scratch = Scratch::new();
        let fake = Fake::default();
        let mut e = engine(&scratch, &fake);
        let (first, _) = planned(&mut e);
        let (second, mut visible_plan) = planned(&mut e);
        assert_ne!(first, second);
        assert!(e.execute(Request::Apply { token: first }).is_err());
        assert_eq!(fake.0.lock().unwrap().writes, 0);
        assert_eq!(fs::read_dir(&scratch.0).unwrap().count(), 1, "planning only creates worker lock");
        // The frontend snapshot is never the authority passed back to Apply.
        let (mut tampered, admission) = visible_plan.changes.copied().unwrap();
        tampered[0].after = Some(StoredValue::text("tampered frontend copy"));
        visible_plan.changes = super::super::SharedChanges::admitted(tampered, admission);
        e.execute(Request::Apply { token: second }).unwrap();
        assert_ne!(fake.0.lock().unwrap().values[0].1, StoredValue::text("tampered frontend copy"));
        assert!(e.execute(Request::Apply { token: second }).is_err(), "review is consumed");
        assert_eq!(fake.0.lock().unwrap().writes, 3);
    }

    #[test]
    fn cancellation_failed_replan_and_fresh_preconditions_invalidate_review() {
        let _admission_case = crate::associations::test_admission_guard();
        let scratch = Scratch::new();
        let fake = Fake::default();
        let mut e = engine(&scratch, &fake);
        let (token, _) = planned(&mut e);
        e.execute(Request::Cancel { token }).unwrap();
        assert!(e.execute(Request::Apply { token }).is_err());
        let (token, _) = planned(&mut e);
        let mut invalid = request();
        invalid.default_choice = DefaultChoice::MakeDefault;
        assert!(e.execute(Request::Plan(invalid)).is_err(), "protected Windows defaults stay forbidden");
        assert!(e.execute(Request::Apply { token }).is_err());
        let (token, plan) = planned(&mut e);
        fake.0.lock().unwrap().values.push((plan.changes[0].location.clone(), StoredValue::text("foreign")));
        assert!(e.execute(Request::Apply { token }).is_err());
        assert!(e.pending.is_none());
        assert!(e.journal.active.is_none());
        assert_eq!(fake.0.lock().unwrap().writes, 0);
    }

    #[test]
    fn persistence_failure_precedes_all_native_writes_and_preserves_obstruction() {
        let _admission_case = crate::associations::test_admission_guard();
        let scratch = Scratch::new();
        let fake = Fake::default();
        let mut e = engine(&scratch, &fake);
        let (token, _) = planned(&mut e);
        let obstruction = e.journal.path(1, "intent");
        let mut file = private_new(&obstruction).unwrap();
        file.write_all(b"preserve preexisting incomplete intent").unwrap();
        drop(file);
        assert!(e.execute(Request::Apply { token }).is_err());
        assert_eq!(fake.0.lock().unwrap().writes, 0);
        assert_eq!(fs::read(&obstruction).unwrap(), b"preserve preexisting incomplete intent");
        assert!(e.execute(Request::Undo { journal_id: 1 }).is_err());
        drop(e);
        assert!(Journal::open(scratch.0.clone()).is_err(), "corrupt intent never silently reset");
    }

    #[test]
    fn journal_owner_drop_releases_lock_while_inherited_descriptor_lives() {
        let _admission_case = crate::associations::test_admission_guard();
        let scratch = Scratch::new();
        let fake = Fake::default();
        let owner = engine(&scratch, &fake);
        // A concurrent Unix fork can retain this open-file description until
        // exec, even though normal file descriptors are close-on-exec.
        let inherited = owner.journal._lock.try_clone().unwrap();
        assert!(Journal::open(scratch.0.clone()).is_err());
        drop(owner);
        let replacement = engine(&scratch, &fake);
        drop(inherited);
        assert!(Journal::open(scratch.0.clone()).is_err(), "replacement must retain its own exclusive lock");
        drop(replacement);
        assert!(Journal::open(scratch.0.clone()).is_ok());
    }

    #[test]
    fn journal_sync_failure_blocks_apply_and_undo_until_restart() {
        let _admission_case = crate::associations::test_admission_guard();
        let scratch = Scratch::new();
        let fake = Fake::default();
        let mut e = engine(&scratch, &fake);
        let (token, _) = planned(&mut e);
        e.journal.fail_sync = true;
        let error = e.execute(Request::Apply { token }).unwrap_err();
        assert_eq!(error.recovery, Some(1));
        assert_eq!(fake.0.lock().unwrap().writes, 0);
        assert!(e.execute(Request::Undo { journal_id: 1 }).is_err());
        assert_eq!(fake.0.lock().unwrap().writes, 0, "a volatile receipt cannot authorize writes");
        drop(e);
        // A complete intent that survived a failed sync is conservative recovery;
        // no native mutation happened, so every entry is already Before.
        let mut e = engine(&scratch, &fake);
        assert!(status(&mut e).recovery.unwrap().current.iter().all(|c| *c == CurrentState::Before));
        e.execute(Request::Undo { journal_id: 1 }).unwrap();
        assert_eq!(fake.0.lock().unwrap().writes, 0);
    }

    #[test]
    fn mid_apply_failure_retains_full_durable_receipt_for_restart_undo() {
        let _admission_case = crate::associations::test_admission_guard();
        let scratch = Scratch::new();
        let fake = Fake::default();
        let mut e = engine(&scratch, &fake);
        let (token, _) = planned(&mut e);
        {
            let mut s = fake.0.lock().unwrap();
            s.durable = Some(e.journal.path(1, "intent"));
            s.fail_from = Some(2); // First write succeeds; next + automatic rollback fail.
        }
        let error = e.execute(Request::Apply { token }).unwrap_err();
        assert_eq!(error.recovery, Some(1));
        assert_eq!(status(&mut e).recovery.unwrap().receipt.pending().len(), 3);
        assert_eq!(fake.0.lock().unwrap().values.len(), 1);
        assert!(e.execute(Request::Plan(request())).is_err());
        let intent = fs::read(e.journal.path(1, "intent")).unwrap();
        drop(e);
        fake.0.lock().unwrap().fail_from = None;
        let mut resumed = engine(&scratch, &fake);
        assert_eq!(
            status(&mut resumed).recovery.unwrap().current,
            vec![CurrentState::Applied, CurrentState::Before, CurrentState::Before]
        );
        resumed.execute(Request::Undo { journal_id: 1 }).unwrap();
        assert!(fake.0.lock().unwrap().values.is_empty());
        assert_eq!(fs::read(resumed.journal.path(1, "intent")).unwrap(), intent, "immutable old values survive Undo");
        drop(resumed);
        assert!(status(&mut engine(&scratch, &fake)).recovery.is_none());
    }

    #[test]
    fn interrupted_undo_restores_old_values_on_retry_and_refuses_foreign_edits() {
        let _admission_case = crate::associations::test_admission_guard();
        let scratch = Scratch::new();
        let fake = Fake::default();
        let mut e = engine(&scratch, &fake);
        // Use nonempty, typed old values to prove restart retains exact bytes.
        let (_, baseline) = planned(&mut e);
        let original: Vec<_> = baseline
            .changes
            .iter()
            .enumerate()
            .map(|(i, c)| (c.location.clone(), StoredValue { kind: i as u32 + 20, bytes: vec![0, 255, i as u8] }))
            .collect();
        fake.0.lock().unwrap().values = original.clone();
        let (token, p) = planned(&mut e);
        e.execute(Request::Apply { token }).unwrap();
        assert!(e.execute(Request::Undo { journal_id: 2 }).is_err(), "id must match exact receipt");
        {
            let mut s = fake.0.lock().unwrap();
            s.values[0].1 = StoredValue::text("foreign");
            s.fail_from = Some(s.writes + 2); // One successful Undo, then a failure.
        }
        let error = e.execute(Request::Undo { journal_id: 1 }).unwrap_err();
        assert_eq!(error.recovery, Some(1));
        assert_eq!(fake.0.lock().unwrap().values[0].1, StoredValue::text("foreign"));
        drop(e);
        {
            let mut s = fake.0.lock().unwrap();
            s.fail_from = None;
            // A foreign change never gets overwritten on restart either.
        }
        let mut e = engine(&scratch, &fake);
        assert!(e.execute(Request::Undo { journal_id: 1 }).is_err());
        assert!(status(&mut e).recovery.unwrap().current.contains(&CurrentState::Conflict));
        {
            let mut s = fake.0.lock().unwrap();
            let value = s.values.iter_mut().find(|(l, _)| *l == p.changes[0].location).unwrap();
            value.1 = p.changes[0].after.clone().unwrap();
        }
        e.execute(Request::Undo { journal_id: 1 }).unwrap();
        let s = fake.0.lock().unwrap();
        for (location, value) in original {
            assert_eq!(s.values.iter().find(|(l, _)| *l == location).unwrap().1, value);
        }
    }

    #[test]
    fn restart_invalidates_review_and_exclusive_lock_prevents_parallel_workers() {
        let _admission_case = crate::associations::test_admission_guard();
        let scratch = Scratch::new();
        let fake = Fake::default();
        let mut e = engine(&scratch, &fake);
        let (old, _) = planned(&mut e);
        assert!(Journal::open(scratch.0.clone()).is_err());
        drop(e);
        let mut e = engine(&scratch, &fake);
        let (new, _) = planned(&mut e);
        assert_ne!(old, new);
        assert!(e.execute(Request::Apply { token: old }).is_err());
        assert_eq!(fake.0.lock().unwrap().writes, 0);
    }

    #[test]
    fn worker_constructs_adapter_off_caller_and_bounds_undrained_event_credit() {
        let _admission_case = crate::associations::test_admission_guard();
        let scratch = Scratch::new();
        let caller = thread::current().id();
        let fake = Fake::default();
        let (constructed_tx, constructed_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (wake_tx, wake_rx) = mpsc::channel();
        let (worker, events) = AssociationWorker::spawn(
            scratch.0.clone(),
            move || {
                constructed_tx.send(thread::current().id()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(3)).unwrap();
                // A non-Send adapter can be constructed on this worker.
                struct Local(Fake, std::rc::Rc<()>);
                impl AssociationAdapter for Local {
                    fn owner(&self) -> String {
                        let _ = &self.1;
                        self.0.owner()
                    }
                    fn read(&mut self, l: &Location) -> Result<Option<StoredValue>, AssociationError> {
                        self.0.read(l)
                    }
                    fn read_bounded(
                        &mut self,
                        l: &Location,
                        b: &mut super::super::ValueBudget,
                    ) -> Result<Option<StoredValue>, AssociationError> {
                        self.0.read_bounded(l, b)
                    }
                    fn write(&mut self, l: &Location, v: Option<&StoredValue>) -> Result<(), AssociationError> {
                        self.0.write(l, v)
                    }
                }
                Ok(Local(fake, std::rc::Rc::new(())))
            },
            move || {
                let _ = wake_tx.send(());
            },
        )
        .unwrap();
        assert_ne!(constructed_rx.recv_timeout(Duration::from_secs(3)).unwrap(), caller);
        for _ in 0..QUEUE_CAPACITY {
            worker.try_submit(Request::Status).unwrap();
        }
        assert_eq!(worker.try_submit(Request::Status), Err(SubmitError::Full));
        release_tx.send(()).unwrap();
        wake_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(worker.try_submit(Request::Status), Err(SubmitError::Full), "undrained results retain their credit");
        worker.shutdown().unwrap(); // Must not require event consumption to drain.
        assert_eq!(wake_rx.try_iter().count(), 0, "one coalesced wake for undrained output");
        let mut ids = Vec::new();
        while let Some(event) = events.try_recv() {
            event.result.unwrap();
            ids.push(event.request_id.0);
        }
        assert_eq!(ids, (1..=QUEUE_CAPACITY as u64).collect::<Vec<_>>());
    }

    #[test]
    fn receiver_close_inside_wake_cannot_deadlock_or_abandon_accepted_apply() {
        let _admission_case = crate::associations::test_admission_guard();
        let scratch = Scratch::new();
        let fake = Fake::default();
        let (wake_tx, wake_rx) = mpsc::channel();
        let (closed_tx, closed_rx) = mpsc::channel();
        let saved_events: Arc<Mutex<Option<Events>>> = Arc::default();
        let close_events = saved_events.clone();
        let adapter = fake.clone();
        let (worker, events) = AssociationWorker::spawn(
            scratch.0.clone(),
            move || Ok(adapter),
            move || {
                // Reenter the mailbox via receiver Drop, without worker lock held.
                if let Some(events) = close_events.lock().unwrap().take() {
                    drop(events);
                    let _ = closed_tx.send(());
                }
                let _ = wake_tx.send(());
            },
        )
        .unwrap();
        worker.try_submit(Request::Plan(request())).unwrap();
        let token = match receive(&events, &wake_rx).result.unwrap() {
            Response::Planned { token, .. } => token,
            other => panic!("unexpected {other:?}"),
        };
        // Block the callback's mutex so accepted Apply and Status are queued
        // before the callback closes output. No timing or slow-stage loop.
        let mut guard = saved_events.lock().unwrap();
        *guard = Some(events);
        worker.try_submit(Request::Apply { token }).unwrap();
        worker.try_submit(Request::Status).unwrap();
        drop(guard);
        closed_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(worker.try_submit(Request::Status), Err(SubmitError::Closed));
        worker.shutdown().unwrap();
        assert_eq!(fake.0.lock().unwrap().writes, 3);
        assert_eq!(status(&mut engine(&scratch, &fake)).recovery.unwrap().journal_id, 1);
    }

    #[test]
    fn adapter_interruption_is_reported_and_restart_undo_uses_prejournaled_receipt() {
        let _admission_case = crate::associations::test_admission_guard();
        let scratch = Scratch::new();
        let fake = Fake::default();
        fake.0.lock().unwrap().panic_at = Some(2);
        let adapter = fake.clone();
        let (wake_tx, wake_rx) = mpsc::channel();
        let (worker, events) = AssociationWorker::spawn(
            scratch.0.clone(),
            move || Ok(adapter),
            move || {
                let _ = wake_tx.send(());
            },
        )
        .unwrap();
        worker.try_submit(Request::Plan(request())).unwrap();
        let token = match receive(&events, &wake_rx).result.unwrap() {
            Response::Planned { token, .. } => token,
            other => panic!("unexpected {other:?}"),
        };
        worker.try_submit(Request::Apply { token }).unwrap();
        let error = receive(&events, &wake_rx).result.unwrap_err();
        assert_eq!(error.recovery, Some(1));
        worker.shutdown().unwrap();
        assert_eq!(fake.0.lock().unwrap().values.len(), 1);
        fake.0.lock().unwrap().panic_at = None;
        let mut e = engine(&scratch, &fake);
        e.execute(Request::Undo { journal_id: 1 }).unwrap();
        assert!(fake.0.lock().unwrap().values.is_empty());
    }

    #[test]
    fn drop_is_nonblocking_and_still_drains_accepted_work() {
        let _admission_case = crate::associations::test_admission_guard();
        let scratch = Scratch::new();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (wake_tx, wake_rx) = mpsc::channel();
        let (worker, events) = AssociationWorker::spawn(
            scratch.0.clone(),
            move || {
                started_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(3)).unwrap();
                Ok(Fake::default())
            },
            move || {
                let _ = wake_tx.send(());
            },
        )
        .unwrap();
        started_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        worker.try_submit(Request::Status).unwrap();
        // If Drop joined, the finite factory gate would panic before this send.
        drop(worker);
        release_tx.send(()).unwrap();
        receive(&events, &wake_rx).result.unwrap();
        // Events owns no thread; do not reopen storage before detached exit.
        drop(events);
    }

    #[test]
    fn bounded_request_records_paths_and_history_fail_closed() {
        let _admission_case = crate::associations::test_admission_guard();
        let scratch = Scratch::new();
        let fake = Fake::default();
        let mut e = engine(&scratch, &fake);
        let mut request = request();
        request.types[0].extension = "x".repeat(256);
        assert!(!bounded_request(&request));
        assert!(e.execute(Request::Plan(request)).is_err());
        let (token, _) = planned(&mut e);
        e.journal.last = MAX_JOURNAL_TRANSACTIONS;
        assert!(e.execute(Request::Apply { token }).is_err());
        assert_eq!(fake.0.lock().unwrap().writes, 0);
        let oversized = scratch.0.join("oversized");
        private_new(&oversized).unwrap().set_len(MAX_RECORD_BYTES as u64 + 1).unwrap();
        assert!(read_record(&oversized, MAX_RECORD_BYTES).is_err());
        assert!(Journal::open(PathBuf::from("relative-path")).is_err());
    }

    #[test]
    fn foreign_receipt_owner_cannot_recover_or_write() {
        let _admission_case = crate::associations::test_admission_guard();
        let scratch = Scratch::new();
        let fake = Fake::default();
        let mut journal = Journal::open(scratch.0.clone()).unwrap();
        let plan = super::super::plan(&mut fake.clone(), &request()).unwrap();
        journal.begin(AssociationReceipt { owner: "another-user".into(), remaining: plan.changes }).unwrap();
        drop(journal);
        let reopened = Journal::open(scratch.0.clone()).unwrap();
        assert!(Engine::new(fake.clone(), reopened).is_err());
        assert_eq!(fake.0.lock().unwrap().writes, 0);
    }

    #[cfg(unix)]
    #[test]
    fn unix_special_symlink_hardlink_and_public_paths_are_rejected_before_open() {
        let _admission_case = crate::associations::test_admission_guard();
        use std::{
            ffi::CString,
            os::unix::{
                ffi::OsStrExt,
                fs::{PermissionsExt, symlink},
            },
        };
        let scratch = Scratch::new();
        let fifo = scratch.0.join("fifo");
        let c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        assert!(read_record(&fifo, 64).is_err(), "must reject without blocking on FIFO");
        let target = scratch.0.join("target");
        private_new(&target).unwrap();
        let link = scratch.0.join("link");
        symlink(&target, &link).unwrap();
        assert!(read_record(&link, 64).is_err());
        let hard = scratch.0.join("hard");
        fs::hard_link(&target, &hard).unwrap();
        assert!(read_record(&hard, 64).is_err());
        let directory_link = scratch.0.join("directory-link");
        symlink(&scratch.0, &directory_link).unwrap();
        assert!(Journal::open(directory_link).is_err());
        fs::set_permissions(&scratch.0, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Journal::open(scratch.0.clone()).is_err());
    }
}
