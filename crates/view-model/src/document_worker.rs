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

pub enum DocumentRequest {
    Prepare { root: Option<PathBuf> },
    Persist(Session),
    Recover { tab: Tab },
    Open { document: DocumentId, revision: u64, path: PathBuf },
    Index { document: DocumentId, revision: u64, path: PathBuf, file: TextFile },
    Save { document: DocumentId, request: SaveRequest },
}

pub enum DocumentResult {
    Prepared(io::Result<PreparedWorkspace>),
    Persisted(io::Result<()>),
    Recovered { document: DocumentId, revision: u64, result: io::Result<Document> },
    Opened { document: DocumentId, revision: u64, result: io::Result<(PathBuf, TextFile)> },
    Indexed { document: DocumentId, revision: u64, result: io::Result<Document> },
    Saved { document: DocumentId, revision: u64, result: io::Result<SavedSnapshot> },
}

pub struct PreparedWorkspace {
    pub session: Session,
    pub documents: Vec<(DocumentId, io::Result<Document>)>,
}

struct Storage {
    root: PathBuf,
    _ownership: std::fs::File,
    service: JournalService,
    journals: HashMap<DocumentId, JournalSession>,
}

pub struct DocumentWorker {
    tx: SyncSender<DocumentRequest>,
}

impl DocumentWorker {
    pub fn spawn(complete: impl Fn(DocumentResult) + Send + 'static) -> io::Result<Self> {
        let (tx, rx) = mpsc::sync_channel(16);
        std::thread::Builder::new().name("kwil-documents".into()).spawn(move || {
            let mut storage: Option<Storage> = None;
            while let Ok(request) = rx.recv() {
                match request {
                    DocumentRequest::Prepare { root } => {
                        let prepared = prepare(root).map(|(prepared, state)| {
                            storage = Some(state);
                            prepared
                        });
                        complete(DocumentResult::Prepared(prepared));
                    }
                    DocumentRequest::Persist(session) => {
                        let result = storage
                            .as_ref()
                            .ok_or_else(|| io::Error::other("Session recovery is unavailable"))
                            .and_then(|s| session::store_session(&s.root.join("session.kws"), &session));
                        complete(DocumentResult::Persisted(result));
                    }
                    DocumentRequest::Recover { tab } => {
                        let result = (|| {
                            let storage = storage.as_mut().ok_or_else(|| io::Error::other("Recovery unavailable"))?;
                            let path = storage.root.join(format!("{}.kwj", tab.id.0));
                            if path.try_exists()? {
                                let (document, recovered, _) = recover_document(path, &tab)?;
                                let handle = resume(&storage.service, recovered)?;
                                storage.journals.insert(tab.id, handle);
                                Ok(document)
                            } else if let Some(path) = &tab.path {
                                kwil_core_workspace::open_for_editing(path).and_then(|file| index(path.clone(), file))
                            } else if tab.revision == tab.saved_revision {
                                Ok(Document::new())
                            } else {
                                Err(io::Error::other("Closed document has no complete recovery journal"))
                            }
                        })();
                        complete(DocumentResult::Recovered { document: tab.id, revision: tab.revision, result });
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
                                complete(DocumentResult::Indexed { document, revision, result });
                            }
                            Err(error) => complete(DocumentResult::Opened { document, revision, result: Err(error) }),
                        }
                    }
                    DocumentRequest::Index { document, revision, path, file } => {
                        complete(DocumentResult::Indexed { document, revision, result: index(path, file) });
                    }
                    DocumentRequest::Save { document, request } => {
                        let revision = request.revision;
                        complete(DocumentResult::Saved {
                            document,
                            revision,
                            result: kwil_core_workspace::save_snapshot(request),
                        });
                    }
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
    let root = match root {
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
    };
    std::fs::create_dir_all(&root)?;
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
                journals.insert(tab.id, handle);
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
    Ok((PreparedWorkspace { session, documents }, Storage { root, _ownership: ownership, service, journals }))
}

fn recover_document(path: PathBuf, tab: &Tab) -> io::Result<(Document, RecoveredJournal, JournalBase)> {
    let mut buffer = Buffer::with_add_memory_limit(0);
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
                edit = Some((recipe, 0));
            }
            RecoveryEvent::EditBytes(bytes) => {
                let (recipe, written) = edit.as_mut().ok_or_else(|| io::Error::other("Missing recovery edit"))?;
                buffer.insert(recipe.offset + *written, bytes)?;
                *written += bytes.len() as u64;
            }
            RecoveryEvent::CommitEdit => {
                buffer.commit();
                edit = None;
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
    use kwil_core_workspace::journal::{JournalCommand, JournalEdit, JournalSeed};

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
    fn malformed_recovery_override_never_uses_default_storage() {
        assert!(prepare(Some("relative-recovery-directory".into())).is_err());
    }
}
