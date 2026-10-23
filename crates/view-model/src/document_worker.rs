//! One bounded file-I/O lane shared by all tabs. Callbacks wake the frontend;
//! the UI never polls for work or launches a thread per file/save.
use crate::Document;
mod decoded;
pub use decoded::{PreparedDecodedDocument, prepare_decoded_document};
use kwil_core_text::{Buffer, TextFile};
use kwil_core_workspace::{SaveRequest, SavedSnapshot, session::DocumentId};
use kwil_core_workspace::{
    journal::{JournalBase, JournalService, JournalSession, RecoveredJournal, RecoveryEvent},
    session::{self, Session, Tab},
};
use std::{
    collections::HashMap,
    io,
    path::PathBuf,
    sync::{
        atomic::AtomicBool,
        mpsc::{self, SyncSender, TrySendError},
    },
    time::Duration,
};

/// Ok acknowledges bounded queue admission, not display. If shutdown races the
/// wake after admission, the accepted request is not retried (which could open
/// it twice); a shutdown notice is emitted and later calls return Disconnected.
pub type ExternalOpen = Box<
    dyn Fn(
            kwil_core_workspace::instance::OpenFiles,
        ) -> Result<(), TrySendError<kwil_core_workspace::instance::OpenFiles>>
        + Send,
>;
pub struct ExternalOpenInbox {
    rx: mpsc::Receiver<kwil_core_workspace::instance::OpenFiles>,
    pending: std::sync::Arc<AtomicBool>,
}

pub fn external_open_channel(
    wake: impl Fn() -> io::Result<()> + Send + Sync + 'static,
) -> (ExternalOpen, ExternalOpenInbox) {
    let (tx, rx) = mpsc::sync_channel(16);
    let pending = std::sync::Arc::new(AtomicBool::new(false));
    let wake_pending = pending.clone();
    let closed = AtomicBool::new(false);
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
    (open, ExternalOpenInbox { rx, pending })
}

impl ExternalOpenInbox {
    pub fn drain(&self) -> Vec<kwil_core_workspace::instance::OpenFiles> {
        // Clear first: a concurrent producer either joins this bounded drain
        // or schedules another wake, so no request can be stranded.
        self.pending.store(false, std::sync::atomic::Ordering::Release);
        (0..16).map_while(|_| self.rx.try_recv().ok()).collect()
    }
}

pub enum DocumentRequest {
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
    },
    Journal {
        document: DocumentId,
        receipt: crate::EditReceipt,
        path: Option<PathBuf>,
        first_sequence: u64,
        generation: u64,
    },
    FinishJournal {
        document: DocumentId,
        generation: u64,
    },
    FlushJournals,
}

pub enum DocumentResult {
    SessionCheckpointDue,
    CapacityAvailable,
    Stopped(io::Result<()>),
    Prepared(io::Result<PreparedWorkspace>),
    Persisted(io::Result<()>),
    Recovered { document: DocumentId, revision: u64, result: io::Result<Box<Document>> },
    Opened { document: DocumentId, revision: u64, result: io::Result<(PathBuf, TextFile)> },
    Indexed { document: DocumentId, revision: u64, result: io::Result<Box<Document>> },
    Saved { document: DocumentId, revision: u64, result: io::Result<SavedSnapshot> },
    ReopenValidated { document: DocumentId, generation: u64, result: io::Result<bool> },
    JournalQueued { document: DocumentId, revision: u64, sequence: u64, result: io::Result<()> },
    JournalDurable { document: DocumentId, revision: u64, sequence: u64, result: io::Result<()> },
    JournalClosed { document: DocumentId, result: io::Result<()> },
}

pub struct PreparedWorkspace {
    pub session: Session,
    pub documents: Vec<(DocumentId, io::Result<Document>)>,
}

struct Storage {
    root: PathBuf,
    _ownership: std::fs::File,
    service: JournalService,
    journals: HashMap<DocumentId, JournalState>,
    order: HashMap<DocumentId, u64>,
    durable_session: Session,
}

struct JournalState {
    handle: JournalSession,
    revision: u64,
    sequence: u64,
    ready: bool,
    flush: Option<(u64, mpsc::Receiver<io::Result<u64>>)>,
    needs_flush: bool,
    failed: bool,
    flush_due: std::time::Instant,
    generation: u64,
}
impl JournalState {
    fn new(handle: JournalSession, revision: u64, sequence: u64) -> Self {
        Self {
            handle,
            revision,
            sequence,
            ready: false,
            flush: None,
            needs_flush: true,
            failed: false,
            flush_due: std::time::Instant::now() + Duration::from_secs(2),
            generation: 0,
        }
    }
}

pub struct DocumentWorker {
    tx: SyncSender<DocumentRequest>,
    journal_capacity: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    save_destinations: std::sync::Arc<std::sync::Mutex<Vec<(DocumentId, PathBuf)>>>,
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
            match sender.try_send(DocumentRequest::Journal {
                document,
                receipt,
                path: path.clone(),
                first_sequence,
                generation,
            }) {
                Ok(()) => Ok(()),
                Err(
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
        let (tx, rx) = mpsc::sync_channel(16);
        let journal_capacity = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(16));
        let capacity = journal_capacity.clone();
        let save_destinations = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let destinations = save_destinations.clone();
        std::thread::Builder::new().name("kwil-documents".into()).spawn(move || {
            let mut storage: Option<Storage> = None;
            let mut deferred = std::collections::VecDeque::new();
            let mut session_due = None;
            // Worker-owned until the UI publishes the admitted tab and releases
            // this reservation. UI registry refreshes cannot erase it early.
            let mut reopening: Option<(DocumentId, u64, PathBuf)> = None;
            loop {
                let waiting = session_due.is_some()
                    || storage.as_ref().is_some_and(|state| {
                        state.journals.values().any(|journal| {
                            !journal.failed && (!journal.ready || journal.flush.is_some() || journal.needs_flush)
                        })
                    });
                let available = deferred.iter().position(|request| {
                    let document = match request {
                        DocumentRequest::Journal { document, .. } | DocumentRequest::SeedJournal { document, .. } => {
                            document
                        }
                        _ => return true,
                    };
                    storage.as_ref().is_none_or(|storage| {
                        storage.journals.contains_key(document) || storage.journals.len() < session::MAX_TABS
                    })
                });
                let request = if let Some(index) = available {
                    deferred.remove(index)
                } else if waiting {
                    match rx.recv_timeout(Duration::from_millis(25)) {
                        Ok(request) => Some(request),
                        Err(mpsc::RecvTimeoutError::Timeout) => None,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                } else {
                    match rx.recv() {
                        Ok(request) => Some(request),
                        Err(_) => break,
                    }
                };
                if let Some(request) = request {
                    let needs_slot = match &request {
                        DocumentRequest::Journal { document, .. } | DocumentRequest::SeedJournal { document, .. } => {
                            storage.as_ref().is_some_and(|storage| {
                                !storage.journals.contains_key(document) && storage.journals.len() >= session::MAX_TABS
                            })
                        }
                        _ => false,
                    };
                    if needs_slot {
                        if let DocumentRequest::SeedJournal { document, .. } = &request {
                            deferred.retain(
                                |old| !matches!(old,DocumentRequest::SeedJournal {document:old,..} if old==document),
                            );
                        }
                        deferred.push_back(request);
                        complete(DocumentResult::CapacityAvailable);
                        continue;
                    }
                    match request {
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
                                session_due = Some(std::time::Instant::now() + Duration::from_secs(2));
                            }
                        }
                        DocumentRequest::Recover { tab } => {
                            let result = (|| {
                                let storage =
                                    storage.as_mut().ok_or_else(|| io::Error::other("Recovery unavailable"))?;
                                let path = storage.root.join(format!("{}.kwj", tab.id.0));
                                if path.try_exists()? {
                                    let (document, recovered, _) = recover_document(path, &tab)?;
                                    let handle = resume(&storage.service, recovered)?;
                                    let revision = document.revision();
                                    storage.journals.insert(tab.id, JournalState::new(handle, revision, 0));
                                    Ok(document)
                                } else if let Some(path) = &tab.path {
                                    kwil_core_workspace::open_for_editing(path)
                                        .and_then(|file| index(path.clone(), file))
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
                                    complete(DocumentResult::Opened {
                                        document,
                                        revision,
                                        result: Ok((path.clone(), file.clone())),
                                    });
                                    let result = index(path, file);
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
                            complete(DocumentResult::Indexed {
                                document,
                                revision,
                                result: index(path, file).map(Box::new),
                            });
                        }
                        DocumentRequest::Save { document, request } => {
                            let revision = request.revision;
                            let mut reserved =
                                destinations.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
                            if let Some((owner, _, path)) = &reopening {
                                reserved.push((*owner, path.clone()));
                            }
                            let result = validate_save_destination(document, &request.path, &reserved)
                                .and_then(|()| kwil_core_workspace::save_snapshot(request));
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
                        } => {
                            let result = storage
                                .as_mut()
                                .ok_or_else(|| io::Error::other("Recovery unavailable"))
                                .and_then(|storage| {
                                    seed_journal(
                                        storage,
                                        document,
                                        revision,
                                        snapshot,
                                        path,
                                        next_sequence.saturating_sub(1),
                                        generation,
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
                        }
                        DocumentRequest::Journal { document, receipt, path, first_sequence, generation } => {
                            let (revision, sequence) = (receipt.after_revision, receipt.sequence);
                            let result = storage
                                .as_mut()
                                .ok_or_else(|| io::Error::other("Recovery unavailable"))
                                .and_then(|storage| {
                                    journal_receipt(storage, document, receipt, path, first_sequence, generation)
                                });
                            capacity.fetch_add(1, std::sync::atomic::Ordering::Release);
                            complete(DocumentResult::JournalQueued { document, revision, sequence, result });
                        }
                        DocumentRequest::FinishJournal { document, generation } => {
                            let result = if let Some(storage) = &mut storage
                                && storage
                                    .journals
                                    .get(&document)
                                    .is_some_and(|journal| journal.generation <= generation)
                                && let Some(journal) = storage.journals.remove(&document)
                            {
                                submit(&journal.handle, kwil_core_workspace::journal::JournalCommand::Finish).and_then(
                                    |_| {
                                        journal
                                            .handle
                                            .finished
                                            .recv_timeout(Duration::from_secs(30))
                                            .map_err(|_| {
                                                io::Error::other("Journal close timed out; recovery retained")
                                            })?
                                            .map(|_| ())
                                    },
                                )
                            } else {
                                Ok(())
                            };
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
                }
                if let Some(storage) = &mut storage {
                    poll_journals(storage, &complete);
                }
                if session_due.is_some_and(|due| std::time::Instant::now() >= due) {
                    session_due = None;
                    complete(DocumentResult::SessionCheckpointDue);
                }
            }
            let stopped = if let Some(mut storage) = storage {
                storage.service.finish();
                // Shutdown is on this worker. UI never waits for seeding/fsync.
                storage
                    .service
                    .finished
                    .recv_timeout(Duration::from_secs(30))
                    .map_err(|_| io::Error::other("Journal shutdown timed out; pending seeds are not durable"))
                    .and_then(|result| result)
            } else {
                Ok(())
            };
            complete(DocumentResult::Stopped(stopped));
        })?;
        Ok(Self { tx, journal_capacity, save_destinations })
    }

    /// Full admission returns ownership. Callers must retain/retry or explicitly
    /// reject it; a rejected save must clear the model's in-flight save state.
    pub fn try_submit(&self, request: DocumentRequest) -> Result<(), TrySendError<Box<DocumentRequest>>> {
        self.tx.try_send(request).map_err(|error| match error {
            TrySendError::Full(request) => TrySendError::Full(Box::new(request)),
            TrySendError::Disconnected(request) => TrySendError::Disconnected(Box::new(request)),
        })
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
        let (mut document, recovered, base) = recover_document(path.clone(), &metadata)?;
        // Orphan metadata never proves a saved state, including revision zero.
        // Reserve an unreachable saved marker on the Document itself: sync
        // must not turn the Session dirty flag back into a clean document.
        mark_recovered_unsaved(&mut document)?;
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
        orphans.insert(*id, (document, recovered));
    }
    for tab in session.tabs() {
        let result = if paths.contains_key(&tab.id) {
            let recovered = match orphans.remove(&tab.id) {
                Some(recovered) => Ok(recovered),
                None => recover_document(paths[&tab.id].clone(), tab).map(|(d, r, _)| (d, r)),
            };
            recovered.and_then(|(document, recovered)| {
                let handle = resume(&service, recovered)?;
                let revision = document.revision();
                journals.insert(tab.id, JournalState::new(handle, revision, 0));
                Ok(document)
            })
        } else if let Some(path) = &tab.path {
            kwil_core_workspace::open_for_editing(path).and_then(|file| index(path.clone(), file))
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
        Storage { root, _ownership: ownership, service, journals, order: HashMap::new(), durable_session },
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
    Ok(())
}

fn seed_journal(
    storage: &mut Storage,
    document: DocumentId,
    revision: u64,
    snapshot: kwil_core_text::Snapshot,
    path: Option<PathBuf>,
    sequence: u64,
    generation: u64,
) -> io::Result<()> {
    if let Some(journal) = storage.journals.get_mut(&document) {
        journal.generation = journal.generation.max(generation);
        return Ok(());
    }
    let sequence = (*storage.order.entry(document).or_insert(sequence)).max(sequence);
    let journal_path = storage.root.join(format!("{}.kwj", document.0));
    let handle = if journal_path.try_exists()? {
        let recovered = RecoveredJournal::open(journal_path, document, &AtomicBool::new(false), |_| Ok(()))?;
        if recovered.summary().revision != revision || recovered.summary().byte_len != snapshot.len() {
            return Err(io::Error::other("Journal revision disagrees with reopened document; recovery retained"));
        }
        resume(&storage.service, recovered)?
    } else {
        let mut seed = kwil_core_workspace::journal::JournalSeed {
            path: journal_path,
            base: JournalBase {
                document,
                revision,
                original_path: path,
                original_identity: [0; 4],
                codec: b"raw-v1".to_vec(),
                byte_len: snapshot.len(),
            },
            source: Box::new(SnapshotReader::new(snapshot)),
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match storage.service.try_create(seed) {
                Ok(handle) => break handle,
                Err(TrySendError::Full(value)) if std::time::Instant::now() < deadline => {
                    seed = *value;
                    std::thread::park_timeout(Duration::from_millis(2));
                }
                Err(_) => return Err(io::Error::other("Journal seed admission failed; edits remain unsaved")),
            }
        }
    };
    let mut state = JournalState::new(handle, revision, sequence);
    state.generation = generation;
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

struct JournalBytes<'a>(&'a JournalSession);
impl io::Write for JournalBytes<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        for chunk in bytes.chunks(kwil_core_workspace::journal::JOURNAL_CHUNK) {
            submit(self.0, kwil_core_workspace::journal::JournalCommand::Bytes(chunk.to_vec()))?;
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn journal_receipt(
    storage: &mut Storage,
    document: DocumentId,
    receipt: crate::EditReceipt,
    path: Option<PathBuf>,
    first_sequence: u64,
    generation: u64,
) -> io::Result<()> {
    use kwil_core_workspace::journal::{JournalBatch, JournalCommand as C, JournalReplacement};
    seed_journal(
        storage,
        document,
        receipt.before_revision,
        receipt.before.clone(),
        path,
        first_sequence.saturating_sub(1),
        generation,
    )?;
    let journal = storage.journals.get_mut(&document).unwrap();
    let result = (|| {
        if journal.failed
            || journal.generation != generation
            || journal.sequence.checked_add(1) != Some(receipt.sequence)
            || journal.revision != receipt.before_revision
        {
            return Err(io::Error::other("Journal receipt gap, duplicate or mismatched revision; recovery retained"));
        }
        submit(
            &journal.handle,
            C::BeginBatch(JournalBatch {
                from_revision: receipt.before_revision,
                to_revision: receipt.after_revision,
                replacements: receipt.replacement_count(),
            }),
        )?;
        receipt.for_each_replacement(&mut |replacement| {
            submit(
                &journal.handle,
                C::BeginReplacement(JournalReplacement {
                    offset: replacement.range.start,
                    removed: replacement.range.end - replacement.range.start,
                    inserted: replacement.insert.len(),
                }),
            )?;
            replacement.insert.write_to(&mut JournalBytes(&journal.handle))?;
            submit(&journal.handle, C::EndReplacement)
        })?;
        submit(&journal.handle, C::CommitBatch)?;
        journal.revision = receipt.after_revision;
        journal.sequence = receipt.sequence;
        journal.needs_flush = true;
        Ok(())
    })();
    if result.is_err() {
        journal.failed = true;
    } else {
        storage.order.insert(document, receipt.sequence);
    }
    result
}

fn poll_journals(storage: &mut Storage, complete: &impl Fn(DocumentResult)) {
    for (&document, journal) in &mut storage.journals {
        if journal.failed {
            continue;
        }
        if !journal.ready {
            match journal.handle.ready.try_recv() {
                Ok(Ok(_)) => journal.ready = true,
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
        let disk = kwil_core_workspace::open_for_editing(path).and_then(|file| index(path.clone(), file));
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

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
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
        drop(storage);
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
        worker.try_submit(DocumentRequest::Save { document: DocumentId(2), request: request() }).unwrap();
        assert!(
            matches!(receiver.recv_timeout(Duration::from_secs(5)).unwrap(), DocumentResult::Saved { result: Err(error), .. } if error.kind() == io::ErrorKind::AlreadyExists)
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"protected");
        worker.try_submit(DocumentRequest::ReleaseReopen { document: DocumentId(1), generation: 7 }).unwrap();
        worker.try_submit(DocumentRequest::Save { document: DocumentId(2), request: request() }).unwrap();
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
        worker.set_save_destinations(vec![(DocumentId(1), path.clone())]);
        let mut editor = crate::Editor::new(Document::new());
        editor
            .execute("edit.insert", &crate::CommandArgs { text: Some("overwrite".into()), ..Default::default() }, 1)
            .unwrap();
        let request = editor.save_request(Some(alias.clone())).unwrap();
        let revision = request.revision;
        worker.try_submit(DocumentRequest::Save { document: DocumentId(2), request }).unwrap();
        match receiver.recv_timeout(Duration::from_secs(5)).unwrap() {
            DocumentResult::Saved { document, revision: source, result } => {
                assert_eq!(document, DocumentId(2));
                assert_eq!(source, revision);
                let error = result.err().expect("native alias must be rejected");
                assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
                assert!(editor.finish_save(Err(error)).is_err());
            }
            _ => panic!("expected addressed worker save rejection"),
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"protected");
        assert_eq!(std::fs::read(&alias).unwrap(), b"protected");
        // A later request sees the latest reservation set, not cached identities.
        worker.set_save_destinations(Vec::new());
        let request = editor.save_request(Some(scratch.0.join("different.txt"))).unwrap();
        worker.try_submit(DocumentRequest::Save { document: DocumentId(2), request }).unwrap();
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
            editor.finish_save(Ok(saved)).unwrap();
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
        workspace.document_result(DocumentResult::Prepared(Ok(prepared)));
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
                file.build_index(1).unwrap();
                View::new(Some((original.clone(), file)))
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
                if let RecoveryEvent::Base(base) = event {
                    base_path = base.original_path.clone();
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
        assert!(prepare(Some(scratch.0.clone())).is_err());
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
        let accepted = inbox.drain();
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
    fn observer_full_retains_the_successful_edit_and_gates_only_later_mutations() {
        let (tx, rx) = mpsc::sync_channel(16);
        let capacity = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(1));
        let worker = DocumentWorker { tx, journal_capacity: capacity.clone(), save_destinations: Default::default() };
        let mut editor = crate::Editor::new(Document::new());
        editor.set_edit_observer(Some(worker.observer(DocumentId(1), None, 1, 1))).unwrap();
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

    fn drive(
        workspace: &mut crate::Workspace,
        receiver: &mpsc::Receiver<DocumentResult>,
        done: impl Fn(&crate::Workspace) -> bool,
    ) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !done(workspace) {
            let wait = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!wait.is_zero(), "workspace condition timed out: {}", workspace.status_text());
            let result = receiver.recv_timeout(wait).expect("worker result before deadline");
            workspace.document_result(result);
        }
    }

    fn wait_stopped(receiver: &mpsc::Receiver<DocumentResult>) {
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
