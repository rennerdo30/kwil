//! One bounded file-I/O lane shared by all tabs. Callbacks wake the frontend;
//! the UI never polls for work or launches a thread per file/save.
use crate::Document;
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
    SeedJournal {
        document: DocumentId,
        revision: u64,
        snapshot: kwil_core_text::Snapshot,
        path: Option<PathBuf>,
        next_sequence: u64,
    },
    Journal {
        document: DocumentId,
        receipt: crate::EditReceipt,
        path: Option<PathBuf>,
        first_sequence: u64,
    },
    FinishJournal {
        document: DocumentId,
    },
    FlushJournals,
}

pub enum DocumentResult {
    Prepared(io::Result<PreparedWorkspace>),
    Persisted(io::Result<()>),
    Recovered { document: DocumentId, revision: u64, result: io::Result<Box<Document>> },
    Opened { document: DocumentId, revision: u64, result: io::Result<(PathBuf, TextFile)> },
    Indexed { document: DocumentId, revision: u64, result: io::Result<Box<Document>> },
    Saved { document: DocumentId, revision: u64, result: io::Result<SavedSnapshot> },
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
}

struct JournalState {
    handle: JournalSession,
    revision: u64,
    sequence: u64,
    ready: bool,
    flush: Option<(u64, mpsc::Receiver<io::Result<u64>>)>,
    needs_flush: bool,
    failed: bool,
}
impl JournalState {
    fn new(handle: JournalSession, revision: u64, sequence: u64) -> Self {
        Self { handle, revision, sequence, ready: false, flush: None, needs_flush: true, failed: false }
    }
}

pub struct DocumentWorker {
    tx: SyncSender<DocumentRequest>,
}

impl DocumentWorker {
    /// The model retains one receipt on Full and centrally gates subsequent
    /// document mutations. This callback only transfers ownership; no I/O or
    /// replacement iteration runs on the UI thread.
    pub fn observer(&self, document: DocumentId, path: Option<PathBuf>, first_sequence: u64) -> crate::EditObserver {
        let sender = self.tx.clone();
        Box::new(move |receipt| {
            match sender.try_send(DocumentRequest::Journal { document, receipt, path: path.clone(), first_sequence }) {
                Ok(()) => Ok(()),
                Err(
                    TrySendError::Full(DocumentRequest::Journal { receipt, .. })
                    | TrySendError::Disconnected(DocumentRequest::Journal { receipt, .. }),
                ) => Err(receipt),
                _ => unreachable!("journal observer submits only journal receipts"),
            }
        })
    }
    pub fn spawn(complete: impl Fn(DocumentResult) + Send + 'static) -> io::Result<Self> {
        let (tx, rx) = mpsc::sync_channel(16);
        std::thread::Builder::new().name("kwil-documents".into()).spawn(move || {
            let mut storage: Option<Storage> = None;
            loop {
                let waiting = storage.as_ref().is_some_and(|state| {
                    state.journals.values().any(|journal| {
                        !journal.failed && (!journal.ready || journal.flush.is_some() || journal.needs_flush)
                    })
                });
                let request = if waiting {
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
                                .as_ref()
                                .ok_or_else(|| io::Error::other("Session recovery is unavailable"))
                                .and_then(|s| session::store_session(&s.root.join("session.kws"), &session));
                            complete(DocumentResult::Persisted(result));
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
                                } else if tab.revision == tab.saved_revision {
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
                            complete(DocumentResult::Saved {
                                document,
                                revision,
                                result: kwil_core_workspace::save_snapshot(request),
                            });
                        }
                        DocumentRequest::SeedJournal { document, revision, snapshot, path, next_sequence } => {
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
                        DocumentRequest::Journal { document, receipt, path, first_sequence } => {
                            let (revision, sequence) = (receipt.after_revision, receipt.sequence);
                            let result = storage
                                .as_mut()
                                .ok_or_else(|| io::Error::other("Recovery unavailable"))
                                .and_then(|storage| journal_receipt(storage, document, receipt, path, first_sequence));
                            complete(DocumentResult::JournalQueued { document, revision, sequence, result });
                        }
                        DocumentRequest::FinishJournal { document } => {
                            let result = if let Some(storage) = &mut storage
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
                                }
                            }
                        }
                    }
                }
                if let Some(storage) = &mut storage {
                    poll_journals(storage, &complete);
                }
            }
            if let Some(mut storage) = storage {
                storage.service.finish();
                // Shutdown is on this worker. UI never waits for seeding/fsync.
                let _ = storage.service.finished.recv_timeout(Duration::from_secs(30));
            }
        })?;
        Ok(Self { tx })
    }

    /// Full admission returns ownership. Callers must retain/retry or explicitly
    /// reject it; a rejected save must clear the model's in-flight save state.
    pub fn try_submit(&self, request: DocumentRequest) -> Result<(), TrySendError<DocumentRequest>> {
        self.tx.try_send(request)
    }
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
        let metadata =
            Tab { id: *id, path: None, revision: 0, saved_revision: 0, cursor: 0, anchor: 0, first_visible_line: 0 };
        let (mut document, recovered, base) = recover_document(path.clone(), &metadata)?;
        let summary = recovered.summary();
        let path = base.original_path.filter(|path| !session.tabs().iter().any(|t| t.path.as_ref() == Some(path)));
        document.path = path.clone();
        session.restore_tab(Tab {
            id: *id,
            path,
            revision: summary.revision,
            saved_revision: summary.revision.wrapping_add(1),
            cursor: 0,
            anchor: 0,
            first_visible_line: 0,
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
        } else if tab.revision != tab.saved_revision {
            Err(io::Error::other("Unsaved document has no complete recovery journal"))
        } else {
            Ok(Document::new())
        };
        documents.push((tab.id, result));
    }
    Ok((
        PreparedWorkspace { session, documents },
        Storage { root, _ownership: ownership, service, journals, order: HashMap::new() },
    ))
}

fn seed_journal(
    storage: &mut Storage,
    document: DocumentId,
    revision: u64,
    snapshot: kwil_core_text::Snapshot,
    path: Option<PathBuf>,
    sequence: u64,
) -> io::Result<()> {
    if storage.journals.contains_key(&document) {
        return Ok(());
    }
    let sequence = *storage.order.entry(document).or_insert(sequence);
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
    storage.journals.insert(document, JournalState::new(handle, revision, sequence));
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
) -> io::Result<()> {
    use kwil_core_workspace::journal::{JournalBatch, JournalCommand as C, JournalReplacement};
    seed_journal(
        storage,
        document,
        receipt.before_revision,
        receipt.before.clone(),
        path,
        first_sequence.saturating_sub(1),
    )?;
    let journal = storage.journals.get_mut(&document).unwrap();
    let result = (|| {
        if journal.failed
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
        if journal.needs_flush && journal.flush.is_none() && !journal.failed {
            match journal.handle.try_flush() {
                Ok(receiver) => {
                    journal.flush = Some((journal.sequence, receiver));
                    journal.needs_flush = false;
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
    document.buffer = buffer;
    document.path = tab.path.clone();
    // Recovery is dirty even when metadata lagged behind the journal.
    document.revision = revision;
    document.saved_revision =
        if tab.saved_revision == revision { revision.wrapping_add(1) } else { tab.saved_revision };
    document.next_revision = revision.checked_add(2).ok_or_else(|| io::Error::other("Recovery revision overflow"))?;
    Ok((document, recovered, base.ok_or_else(|| io::Error::other("Missing recovery base"))?))
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
}
