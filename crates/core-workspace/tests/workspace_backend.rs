//! Small, deterministic backend contract checks. No frontend/native windows/benchmarks.
use kwil_core_workspace::{instance::*, journal::*, session::*};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::PathBuf,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::Duration,
};
const TIMEOUT: Duration = Duration::from_secs(5);
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "kwil-kw20-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn base(len: u64) -> JournalBase {
    JournalBase {
        document: DocumentId(17),
        revision: 1,
        original_path: None,
        original_identity: [1, 2, 3, 4],
        codec: b"raw/utf16le+bom".to_vec(),
        byte_len: len,
    }
}
fn submit(w: &JournalWorker, c: JournalCommand) {
    w.try_submit(c).unwrap();
}
fn flush(w: &JournalWorker) -> u64 {
    w.try_flush().unwrap().recv_timeout(TIMEOUT).unwrap().unwrap()
}
fn create(temp: &Temp, bytes: &[u8]) -> JournalWorker {
    let path = temp.path("original");
    fs::write(&path, bytes).unwrap();
    let w = JournalWorker::create(
        temp.path("journal"),
        base(bytes.len() as u64),
        Some(File::open(&path).unwrap()),
        Duration::from_secs(60),
    )
    .unwrap();
    assert_eq!(w.ready.recv_timeout(TIMEOUT).unwrap().unwrap(), 1);
    w
}
fn edit(w: &JournalWorker, from: u64, to: u64, offset: u64, removed: u64, bytes: &[u8]) {
    submit(
        w,
        JournalCommand::Begin(JournalEdit {
            from_revision: from,
            to_revision: to,
            offset,
            removed,
            inserted: bytes.len() as u64,
        }),
    );
    if !bytes.is_empty() {
        submit(w, JournalCommand::Bytes(bytes.to_vec()));
    }
    submit(w, JournalCommand::Commit);
}

#[test]
fn kw20_session_bounds_stable_ids_reopen_and_atomic_snapshot() {
    let temp = Temp::new();
    let mut s = Session::default();
    let id = s.open(Some(temp.path("nonexistent"))).unwrap();
    s.tab_mut(id).unwrap().revision = 9;
    assert!(s.close(id));
    assert_eq!(s.reopen_closed().unwrap(), Some(id));
    let newer = s.open(None).unwrap();
    assert!(newer.0 > id.0);
    let before = s.clone();
    let worker = SessionWorker::spawn(temp.path("session")).unwrap();
    assert_eq!(worker.loaded.recv_timeout(TIMEOUT).unwrap().unwrap(), Session::default());
    let ack = worker.try_save(before.clone()).unwrap();
    s.tab_mut(id).unwrap().revision = 10;
    ack.recv_timeout(TIMEOUT).unwrap().unwrap();
    assert_eq!(load_session(&temp.path("session")).unwrap(), before);
    worker.try_save(s.clone()).unwrap().recv_timeout(TIMEOUT).unwrap().unwrap();
    worker.shutdown().unwrap();
    assert_eq!(load_session(&temp.path("session")).unwrap(), s);
    for _ in s.tabs().len()..MAX_TABS {
        s.open(None).unwrap();
    }
    assert!(s.open(None).is_err());
    let mut f = OpenOptions::new().write(true).open(temp.path("session")).unwrap();
    f.write_all(b"BAD!").unwrap();
    assert!(load_session(&temp.path("session")).is_err());
}

#[test]
fn kw20_recovery_context_roundtrips_without_claiming_a_named_destination() {
    let temp = Temp::new();
    let name = temp.path("last-save-as.txt");
    let mut session = Session::default();
    let recovered = session.open(Some(name.clone())).unwrap();
    let tab = session.tab_mut(recovered).unwrap();
    tab.recovery_path = tab.path.take();
    tab.saved_revision = 2;
    tab.first_byte_column = 31;
    let live = session.open(Some(name.clone())).unwrap();
    assert_ne!(live, recovered, "recovery context is not an open-path identity");
    assert_eq!(session.open(Some(name.clone())).unwrap(), live);
    assert!(session.close(recovered));
    let path = temp.path("session.kws");
    store_session(&path, &session).unwrap();
    assert_eq!(&fs::read(&path).unwrap()[..4], b"KWS3");
    let mut restored = load_session(&path).unwrap();
    assert_eq!(restored, session);
    assert!(restored.closed()[0].path.is_none());
    assert_eq!(restored.closed()[0].recovery_path.as_ref(), Some(&name));
    assert_eq!(restored.reopen_closed().unwrap(), Some(recovered));
    assert!(restored.set_path(recovered, name).is_err(), "actual destinations still reject duplicates");
    let next = temp.path("new-save-as.txt");
    restored.set_path(recovered, next.clone()).unwrap();
    let tab = restored.tab_mut(recovered).unwrap();
    assert_eq!(tab.path.as_ref(), Some(&next));
    assert!(tab.recovery_path.is_none(), "explicit Save As replaces previous recovery context");
    store_session(&path, &restored).unwrap();
    assert_eq!(load_session(&path).unwrap(), restored);
}

#[test]
fn kw20_recovery_context_validation_preserves_native_path_bounds_and_exclusive_storage() {
    let temp = Temp::new();
    let mut session = Session::default();
    let id = session.open(None).unwrap();
    session.tab_mut(id).unwrap().recovery_path = Some("relative.txt".into());
    assert!(session.validate().is_err());
    session.tab_mut(id).unwrap().recovery_path = Some(temp.path("x"));
    assert!(session.validate().is_ok());
    session.tab_mut(id).unwrap().path = Some(temp.path("other"));
    assert!(session.validate().is_err(), "one native path per tab preserves the existing metadata cap");
    session.tab_mut(id).unwrap().path = None;
    session.tab_mut(id).unwrap().recovery_path = Some(temp.path(&"x".repeat(MAX_PATH_BYTES + 1)));
    assert!(session.validate().is_err());
}

#[test]
fn kw20_closed_history_never_silently_evicts_dirty_document() {
    let mut session = Session::default();
    let dirty = session.open(None).unwrap();
    session.tab_mut(dirty).unwrap().revision = 1;
    assert!(session.close(dirty));
    for _ in 1..MAX_CLOSED {
        let id = session.open(None).unwrap();
        assert!(session.close(id));
    }
    let current = session.open(None).unwrap();
    assert!(!session.close(current));
    assert_eq!(session.closed().back().unwrap().id, dirty);
    assert!(session.discard_closed(dirty));
    assert!(session.close(current));
}

#[test]
fn kw20_orphan_journal_tab_retains_identity_and_advances_allocator() {
    let mut s = Session::default();
    let tab = Tab {
        id: DocumentId(900),
        path: None,
        recovery_path: None,
        revision: 17,
        saved_revision: 1,
        cursor: 9,
        anchor: 9,
        first_visible_line: 0,
        first_byte_column: 0,
    };
    s.restore_tab(tab.clone()).unwrap();
    assert_eq!(s.active(), Some(tab.id));
    assert_eq!(s.tabs()[0], tab);
    assert!(s.restore_tab(tab).is_err());
    assert_eq!(s.open(None).unwrap(), DocumentId(901));
}

#[test]
fn kw20_journal_interrupted_tail_keeps_only_committed_raw_bytes() {
    let temp = Temp::new();
    let raw = b"\xff\xfea\0\x80\xff\r\n";
    let mut w = create(&temp, raw);
    edit(&w, 1, 2, 2, 2, b"z\0");
    assert_eq!(flush(&w), 2);
    submit(
        &w,
        JournalCommand::Begin(JournalEdit { from_revision: 2, to_revision: 3, offset: 0, removed: 0, inserted: 3 }),
    );
    submit(&w, JournalCommand::Bytes(vec![9]));
    flush(&w);
    w.finish();
    assert_eq!(w.finished.recv_timeout(TIMEOUT).unwrap().unwrap(), 2);
    let mut reader = JournalReader::open(&temp.path("journal")).unwrap();
    let mut original = Vec::new();
    let mut edits = Vec::new();
    let summary = reader
        .recover(&AtomicBool::new(false), |e| {
            match e {
                RecoveryEvent::BaseBytes(b) => original.extend_from_slice(b),
                RecoveryEvent::BeginEdit(e) => edits.push(e),
                _ => (),
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(original, raw);
    assert_eq!(edits.len(), 1);
    assert_eq!(summary.revision, 2);
    assert_eq!(summary.tail, RecoveryTail::Interrupted);
    let mut resumed = JournalWorker::resume(temp.path("journal"), DocumentId(17), Duration::from_secs(60)).unwrap();
    assert_eq!(resumed.ready.recv_timeout(TIMEOUT).unwrap().unwrap(), 2);
    edit(&resumed, 2, 4, 0, 0, b"Q");
    flush(&resumed);
    resumed.finish();
    resumed.finished.recv_timeout(TIMEOUT).unwrap().unwrap();
    let summary =
        JournalReader::open(&temp.path("journal")).unwrap().recover(&AtomicBool::new(false), |_| Ok(())).unwrap();
    assert_eq!(summary.revision, 4);
    assert_eq!(summary.tail, RecoveryTail::Complete);
}

#[test]
fn kw20_journal_partial_frame_checksum_and_concurrent_append_snapshot() {
    let temp = Temp::new();
    let mut w = create(&temp, b"\xef\xbb\xbfA\xff");
    edit(&w, 1, 2, 3, 1, b"B");
    flush(&w);
    let mut frozen = JournalReader::open(&temp.path("journal")).unwrap();
    edit(&w, 2, 3, 3, 1, b"C");
    flush(&w);
    let snapshot = frozen.recover(&AtomicBool::new(false), |_| Ok(())).unwrap();
    assert_eq!(snapshot.revision, 2);
    w.finish();
    w.finished.recv_timeout(TIMEOUT).unwrap().unwrap();
    let journal = temp.path("journal");
    let len = fs::metadata(&journal).unwrap().len();
    let mut f = OpenOptions::new().append(true).open(&journal).unwrap();
    f.write_all(b"KWJ1\x20\0").unwrap();
    drop(f);
    let summary = JournalReader::open(&journal).unwrap().recover(&AtomicBool::new(false), |_| Ok(())).unwrap();
    assert_eq!(summary.revision, 3);
    assert_eq!(summary.tail, RecoveryTail::Interrupted);
    let mut f = OpenOptions::new().read(true).write(true).open(&journal).unwrap();
    f.set_len(len).unwrap();
    // Flip an identity byte that remains semantically valid; only CRC detects it.
    f.seek(SeekFrom::Start(37)).unwrap();
    let mut byte = [0];
    f.read_exact(&mut byte).unwrap();
    byte[0] ^= 1;
    f.seek(SeekFrom::Start(37)).unwrap();
    f.write_all(&byte).unwrap();
    drop(f);
    assert!(JournalReader::open(&journal).unwrap().recover(&AtomicBool::new(false), |_| Ok(())).is_err());
}

#[test]
fn kw20_incomplete_base_never_claims_recovery_and_revision_order_rejected() {
    let temp = Temp::new();
    let mut w = create(&temp, b"a");
    submit(
        &w,
        JournalCommand::Begin(JournalEdit { from_revision: 99, to_revision: 100, offset: 0, removed: 0, inserted: 0 }),
    );
    assert!(w.finished.recv_timeout(TIMEOUT).unwrap().is_err());
    w.finish();
    let f = OpenOptions::new().write(true).open(temp.path("journal")).unwrap();
    f.set_len(25).unwrap();
    drop(f);
    assert!(JournalReader::open(&temp.path("journal")).unwrap().recover(&AtomicBool::new(false), |_| Ok(())).is_err());
}

#[test]
fn kw20_ipc_size_malformed_and_native_path_roundtrip() {
    let temp = Temp::new();
    let r = OpenFiles {
        files: vec![OpenFile { path: temp.path("file:123"), line: Some(42), column: Some(7) }],
        wait: None,
    };
    let b = r.encode().unwrap();
    assert_eq!(OpenFiles::decode(&b).unwrap(), r);
    for cut in [0, 3, 7, b.len() - 1] {
        assert!(OpenFiles::decode(&b[..cut]).is_err());
    }
    let mut bad = b.clone();
    bad.extend([1]);
    assert!(OpenFiles::decode(&bad).is_err());
    let mut bad = b.clone();
    bad[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(OpenFiles::decode(&bad).is_err());
    assert!(OpenFiles::decode(&vec![0; MAX_IPC_BYTES + 1]).is_err());
    assert!(OpenFiles { files: vec![r.files[0].clone(); MAX_OPEN_FILES + 1], wait: None }.encode().is_err());
    let invalid = OpenFiles {
        files: vec![OpenFile { path: PathBuf::from("relative"), line: None, column: Some(1) }],
        wait: None,
    };
    assert!(invalid.encode().is_err());
    #[cfg(unix)]
    {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};
        let path = temp.0.join(OsString::from_vec(vec![0xff, b'x']));
        let r = OpenFiles { files: vec![OpenFile { path, line: None, column: None }], wait: None };
        assert_eq!(OpenFiles::decode(&r.encode().unwrap()).unwrap(), r);
    }
    #[cfg(windows)]
    {
        use std::{ffi::OsString, os::windows::ffi::OsStringExt};
        let path = temp.0.join(OsString::from_wide(&[0xd800, 120]));
        let r = OpenFiles { files: vec![OpenFile { path, line: None, column: None }], wait: None };
        assert_eq!(OpenFiles::decode(&r.encode().unwrap()).unwrap(), r);
    }
}

#[test]
fn kw20_ipc_receiver_delivery_duplicate_owner_and_own_endpoint_cleanup() {
    let temp = Temp::new();
    let endpoint =
        InstanceEndpoint { runtime_directory: temp.0.clone(), namespace: format!("test-{}", std::process::id()) };
    let first = InstanceServer::spawn(endpoint.clone()).unwrap();
    first.ready.recv_timeout(TIMEOUT).unwrap().unwrap();
    let duplicate = InstanceServer::spawn(endpoint.clone()).unwrap();
    assert!(duplicate.ready.recv_timeout(TIMEOUT).unwrap().is_err());
    assert!(duplicate.finished.recv_timeout(TIMEOUT).unwrap().is_err());
    drop(duplicate);
    let request =
        OpenFiles { files: vec![OpenFile { path: temp.path("file"), line: Some(12), column: Some(3) }], wait: None };
    send_files(&endpoint, &request, TIMEOUT).unwrap();
    assert_eq!(first.requests.recv_timeout(TIMEOUT).unwrap(), request);
    first.shutdown();
    first.finished.recv_timeout(TIMEOUT).unwrap().unwrap();
    drop(first);
    let second = InstanceServer::spawn(endpoint.clone()).unwrap();
    second.ready.recv_timeout(TIMEOUT).unwrap().unwrap();
    send_files(&endpoint, &request, TIMEOUT).unwrap();
    assert_eq!(second.requests.recv_timeout(TIMEOUT).unwrap(), request);
    second.shutdown();
    second.finished.recv_timeout(TIMEOUT).unwrap().unwrap();
}

#[test]
fn kw20_seed_readiness_bounded_queue_and_chunked_reader() {
    use std::sync::{Arc, mpsc};
    struct Gated {
        gate: mpsc::Receiver<()>,
        bytes: u64,
        max: Arc<AtomicU64>,
    }
    impl Read for Gated {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            if self.bytes == 0 {
                return Ok(0);
            }
            self.max.fetch_max(b.len() as u64, Ordering::Relaxed);
            self.gate.recv_timeout(TIMEOUT).map_err(std::io::Error::other)?;
            let n = b.len().min(self.bytes as usize);
            b[..n].fill(0xff);
            self.bytes -= n as u64;
            Ok(n)
        }
    }
    let temp = Temp::new();
    let (release, gate) = mpsc::sync_channel(4);
    let max = Arc::new(AtomicU64::new(0));
    let mut w = JournalWorker::create_from_reader(
        temp.path("journal"),
        base((JOURNAL_CHUNK * 2 + 3) as u64),
        Box::new(Gated { gate, bytes: (JOURNAL_CHUNK * 2 + 3) as u64, max: max.clone() }),
        Duration::from_secs(60),
    )
    .unwrap();
    assert!(matches!(w.ready.try_recv(), Err(mpsc::TryRecvError::Empty)));
    submit(
        &w,
        JournalCommand::Begin(JournalEdit { from_revision: 1, to_revision: 2, offset: 0, removed: 0, inserted: 15 }),
    );
    for _ in 0..15 {
        submit(&w, JournalCommand::Bytes(vec![1]));
    }
    assert!(matches!(w.try_submit(JournalCommand::Commit), Err(mpsc::TrySendError::Full(JournalCommand::Commit))));
    assert!(matches!(
        w.try_submit(JournalCommand::Bytes(vec![0; JOURNAL_CHUNK + 1])),
        Err(mpsc::TrySendError::Disconnected(_))
    ));
    for _ in 0..3 {
        release.send(()).unwrap();
    }
    assert_eq!(w.ready.recv_timeout(TIMEOUT).unwrap().unwrap(), 1);
    w.finish();
    assert_eq!(w.finished.recv_timeout(TIMEOUT).unwrap().unwrap(), 1);
    assert_eq!(max.load(Ordering::Relaxed), JOURNAL_CHUNK as u64);
    let summary =
        JournalReader::open(&temp.path("journal")).unwrap().recover(&AtomicBool::new(false), |_| Ok(())).unwrap();
    assert_eq!(summary.tail, RecoveryTail::Interrupted);
    assert_eq!(summary.revision, 1);
}

#[test]
fn kw20_shared_service_interleaves_documents_spills_and_bounds_global_queue() {
    use std::sync::{Arc, Mutex, mpsc};
    struct Source {
        id: u8,
        left: usize,
        gate: Option<mpsc::Receiver<()>>,
        reads: Arc<Mutex<Vec<u8>>>,
    }
    impl Read for Source {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            if let Some(gate) = self.gate.take() {
                gate.recv_timeout(TIMEOUT).map_err(std::io::Error::other)?;
            }
            if self.left == 0 {
                return Ok(0);
            }
            assert!(b.len() <= JOURNAL_CHUNK);
            self.reads.lock().unwrap().push(self.id);
            let n = b.len().min(self.left);
            b[..n].fill(self.id);
            self.left -= n;
            Ok(n)
        }
    }
    let temp = Temp::new();
    let reads = Arc::new(Mutex::new(Vec::new()));
    let (release, gate) = mpsc::sync_channel(1);
    let (entered_tx, entered) = mpsc::sync_channel(1);
    struct Entered {
        inner: Source,
        entered: Option<mpsc::SyncSender<()>>,
    }
    impl Read for Entered {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            if let Some(tx) = self.entered.take() {
                let _ = tx.send(());
            }
            self.inner.read(b)
        }
    }
    let mut service = JournalService::spawn(Duration::from_secs(60)).unwrap();
    let a = service
        .try_create(JournalSeed {
            path: temp.path("a"),
            base: base((JOURNAL_CHUNK * 12) as u64),
            source: Box::new(Entered {
                inner: Source { id: 1, left: JOURNAL_CHUNK * 12, gate: Some(gate), reads: reads.clone() },
                entered: Some(entered_tx),
            }),
        })
        .unwrap();
    entered.recv_timeout(TIMEOUT).unwrap();
    let mut bbase = base(1);
    bbase.document = DocumentId(18);
    let b = service
        .try_create(JournalSeed {
            path: temp.path("b"),
            base: bbase,
            source: Box::new(Source { id: 2, left: 1, gate: None, reads: reads.clone() }),
        })
        .unwrap();
    a.try_submit(JournalCommand::Begin(JournalEdit {
        from_revision: 1,
        to_revision: 2,
        offset: 0,
        removed: 0,
        inserted: 14,
    }))
    .unwrap();
    for _ in 0..14 {
        a.try_submit(JournalCommand::Bytes(vec![9])).unwrap();
    }
    assert!(matches!(
        a.try_submit(JournalCommand::Commit),
        Err(std::sync::mpsc::TrySendError::Full(JournalCommand::Commit))
    ));
    release.send(()).unwrap();
    b.ready.recv_timeout(TIMEOUT).unwrap().unwrap();
    // The second seed was read before the first GB-shaped stream finished.
    a.try_submit(JournalCommand::Commit).unwrap();
    let bflush = b.try_flush().unwrap();
    assert_eq!(bflush.recv_timeout(TIMEOUT).unwrap().unwrap(), 1);
    service.finish();
    service.finished.recv_timeout(TIMEOUT).unwrap().unwrap();
    assert_eq!(a.finished.recv_timeout(TIMEOUT).unwrap().unwrap(), 2);
    assert_eq!(b.finished.recv_timeout(TIMEOUT).unwrap().unwrap(), 1);
    let log = reads.lock().unwrap();
    assert!(log.iter().position(|&id| id == 2).unwrap() < log.iter().rposition(|&id| id == 1).unwrap());
    drop(log);
    let mut inserted = Vec::new();
    let summary = JournalReader::open(&temp.path("a"))
        .unwrap()
        .recover(&AtomicBool::new(false), |e| {
            if let RecoveryEvent::EditBytes(bytes) = e {
                inserted.extend_from_slice(bytes);
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(summary.revision, 2);
    assert_eq!(inserted, vec![9; 14]);
    assert!(!temp.path("a.pending").exists());
    let recovered =
        RecoveredJournal::open(temp.path("a"), DocumentId(17), &AtomicBool::new(false), |_| Ok(())).unwrap();
    let mut resumed = JournalService::spawn(Duration::from_secs(60)).unwrap();
    let handle = resumed.try_resume(recovered).unwrap();
    assert_eq!(handle.ready.recv_timeout(TIMEOUT).unwrap().unwrap(), 2);
    resumed.finish();
    resumed.finished.recv_timeout(TIMEOUT).unwrap().unwrap();
}

#[test]
fn kw20_shared_service_cancel_pending_seed_never_reports_durability() {
    use std::sync::mpsc;
    struct Source {
        gate: mpsc::Receiver<()>,
        entered: mpsc::SyncSender<()>,
    }
    impl Read for Source {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            let _ = self.entered.send(());
            self.gate.recv_timeout(TIMEOUT).map_err(std::io::Error::other)?;
            b.fill(7);
            Ok(b.len())
        }
    }
    let temp = Temp::new();
    let (release, gate) = mpsc::sync_channel(1);
    let (entered_tx, entered) = mpsc::sync_channel(1);
    let mut service = JournalService::spawn(Duration::from_secs(60)).unwrap();
    let doc = service
        .try_create(JournalSeed {
            path: temp.path("cancelled"),
            base: base((JOURNAL_CHUNK * 2) as u64),
            source: Box::new(Source { gate, entered: entered_tx }),
        })
        .unwrap();
    entered.recv_timeout(TIMEOUT).unwrap();
    assert!(matches!(doc.ready.try_recv(), Err(mpsc::TryRecvError::Empty)));
    service.cancel();
    release.send(()).unwrap();
    service.finished.recv_timeout(TIMEOUT).unwrap().unwrap();
    assert!(doc.ready.recv_timeout(TIMEOUT).unwrap().is_err());
    assert!(doc.finished.recv_timeout(TIMEOUT).unwrap().is_err());
    assert!(temp.path("cancelled.pending").exists());
    assert!(
        JournalReader::open(&temp.path("cancelled")).unwrap().recover(&AtomicBool::new(false), |_| Ok(())).is_err()
    );
}

#[test]
fn kw20_recovery_finishes_interrupted_pending_merge_without_duplicate_edits() {
    let temp = Temp::new();
    let mut worker = create(&temp, b"\xff\xfe");
    let path = temp.path("journal");
    let base_end =
        JournalReader::open(&path).unwrap().recover(&AtomicBool::new(false), |_| Ok(())).unwrap().committed_end;
    edit(&worker, 1, 2, 2, 0, b"A\0");
    flush(&worker);
    worker.finish();
    worker.finished.recv_timeout(TIMEOUT).unwrap().unwrap();
    let bytes = fs::read(&path).unwrap();
    let pending = &bytes[base_end as usize..];
    fs::write(temp.path("journal.pending"), pending).unwrap();
    let file = OpenOptions::new().write(true).open(&path).unwrap();
    file.set_len(base_end + (pending.len() / 2) as u64).unwrap();
    drop(file);
    let mut commits = 0;
    let recovered = RecoveredJournal::open(path.clone(), DocumentId(17), &AtomicBool::new(false), |e| {
        if let RecoveryEvent::CommitEdit = e {
            commits += 1;
        }
        Ok(())
    })
    .unwrap();
    assert_eq!(commits, 1);
    assert_eq!(recovered.summary().revision, 2);
    assert!(!temp.path("journal.pending").exists());
    drop(recovered);
    // A stale pending file may reappear after its deletion was not persisted.
    // It must not truncate newer edits already durably appended after that merge.
    let mut worker = JournalWorker::resume(path.clone(), DocumentId(17), Duration::from_secs(60)).unwrap();
    worker.ready.recv_timeout(TIMEOUT).unwrap().unwrap();
    edit(&worker, 2, 3, 4, 0, b"B\0");
    flush(&worker);
    worker.finish();
    worker.finished.recv_timeout(TIMEOUT).unwrap().unwrap();
    fs::write(temp.path("journal.pending"), pending).unwrap();
    let recovered = RecoveredJournal::open(path, DocumentId(17), &AtomicBool::new(false), |_| Ok(())).unwrap();
    assert_eq!(recovered.summary().revision, 3);
}

fn batch_commands(commit: bool) -> Vec<JournalCommand> {
    let mut commands = vec![
        JournalCommand::BeginBatch(JournalBatch { from_revision: 1, to_revision: 9, replacements: 2 }),
        JournalCommand::BeginReplacement(JournalReplacement { offset: 1, removed: 1, inserted: 2 }),
        JournalCommand::Bytes(vec![0xff, 0]),
        JournalCommand::EndReplacement,
        // Offset four is legal only in the intermediate five-byte document.
        JournalCommand::BeginReplacement(JournalReplacement { offset: 4, removed: 1, inserted: 1 }),
        JournalCommand::Bytes(vec![b'Z']),
        JournalCommand::EndReplacement,
    ];
    if commit {
        commands.push(JournalCommand::CommitBatch);
    }
    commands
}

#[test]
fn kw20_batch_recovery_is_atomic_at_each_frame_boundary_and_frozen_snapshot() {
    let temp = Temp::new();
    let mut worker = create(&temp, b"abcd");
    let path = temp.path("journal");
    let base_end =
        JournalReader::open(&path).unwrap().recover(&AtomicBool::new(false), |_| Ok(())).unwrap().committed_end;
    for command in batch_commands(false) {
        submit(&worker, command);
    }
    assert_eq!(flush(&worker), 1);
    let mut frozen = JournalReader::open(&path).unwrap();
    submit(&worker, JournalCommand::CommitBatch);
    assert_eq!(flush(&worker), 9);
    worker.finish();
    worker.finished.recv_timeout(TIMEOUT).unwrap().unwrap();
    let mut events = 0;
    let old = frozen
        .recover(&AtomicBool::new(false), |e| {
            if matches!(
                e,
                RecoveryEvent::BeginBatch(_)
                    | RecoveryEvent::BeginReplacement(_)
                    | RecoveryEvent::EditBytes(_)
                    | RecoveryEvent::EndReplacement
                    | RecoveryEvent::CommitBatch
            ) {
                events += 1;
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(events, 0);
    assert_eq!(old.revision, 1);
    assert_eq!(old.byte_len, 4);
    assert_eq!(old.tail, RecoveryTail::Interrupted);
    let bytes = fs::read(&path).unwrap();
    let mut cursor = base_end as usize;
    let mut cuts = vec![cursor];
    while cursor < bytes.len() {
        let n = u32::from_le_bytes(bytes[cursor + 4..cursor + 8].try_into().unwrap()) as usize;
        cursor += 12 + n;
        if cursor < bytes.len() {
            cuts.push(cursor);
        }
    }
    for (i, cut) in cuts.into_iter().enumerate() {
        let prefix = temp.path(&format!("prefix-{i}"));
        fs::write(&prefix, &bytes[..cut]).unwrap();
        let mut emitted = 0;
        let summary = JournalReader::open(&prefix)
            .unwrap()
            .recover(&AtomicBool::new(false), |e| {
                if matches!(
                    e,
                    RecoveryEvent::BeginBatch(_)
                        | RecoveryEvent::BeginReplacement(_)
                        | RecoveryEvent::EditBytes(_)
                        | RecoveryEvent::EndReplacement
                        | RecoveryEvent::CommitBatch
                ) {
                    emitted += 1;
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(emitted, 0, "partial batch exposed at boundary {i}");
        assert_eq!(summary.revision, 1);
        assert_eq!(summary.byte_len, 4);
        assert_eq!(summary.committed_end, base_end);
    }
    let mut model = Vec::new();
    let mut current = None;
    let mut replacement_bytes = Vec::new();
    let mut batch_commits = 0;
    let summary = JournalReader::open(&path)
        .unwrap()
        .recover(&AtomicBool::new(false), |e| {
            match e {
                RecoveryEvent::BaseBytes(b) => model.extend_from_slice(b),
                RecoveryEvent::BeginBatch(b) => {
                    assert_eq!(b, JournalBatch { from_revision: 1, to_revision: 9, replacements: 2 })
                }
                RecoveryEvent::BeginReplacement(r) => {
                    current = Some(r);
                    replacement_bytes.clear();
                }
                RecoveryEvent::EditBytes(b) => replacement_bytes.extend_from_slice(b),
                RecoveryEvent::EndReplacement => {
                    let r = current.take().unwrap();
                    model.splice(r.offset as usize..(r.offset + r.removed) as usize, replacement_bytes.iter().copied());
                }
                RecoveryEvent::CommitBatch => batch_commits += 1,
                _ => (),
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(model, [b'a', 0xff, 0, b'c', b'Z']);
    assert_eq!(batch_commits, 1);
    assert_eq!(summary.revision, 9);
    assert_eq!(summary.byte_len, 5);
}

#[test]
fn kw20_batch_service_spill_interruption_and_resume_keep_one_model_revision() {
    use std::sync::mpsc;
    struct Gated {
        gate: mpsc::Receiver<()>,
        entered: mpsc::SyncSender<()>,
        data: std::io::Cursor<Vec<u8>>,
    }
    impl Read for Gated {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            let _ = self.entered.send(());
            self.gate.recv_timeout(TIMEOUT).map_err(std::io::Error::other)?;
            self.data.read(b)
        }
    }
    let temp = Temp::new();
    let path = temp.path("batch");
    let (release, gate) = mpsc::sync_channel(1);
    let (entered_tx, entered) = mpsc::sync_channel(1);
    let mut service = JournalService::spawn(Duration::from_secs(60)).unwrap();
    let doc = service
        .try_create(JournalSeed {
            path: path.clone(),
            base: base(4),
            source: Box::new(Gated { gate, entered: entered_tx, data: std::io::Cursor::new(b"abcd".to_vec()) }),
        })
        .unwrap();
    entered.recv_timeout(TIMEOUT).unwrap();
    for command in batch_commands(false) {
        doc.try_submit(command).unwrap();
    }
    release.send(()).unwrap();
    assert_eq!(doc.ready.recv_timeout(TIMEOUT).unwrap().unwrap(), 1);
    assert_eq!(doc.try_flush().unwrap().recv_timeout(TIMEOUT).unwrap().unwrap(), 1);
    service.finish();
    service.finished.recv_timeout(TIMEOUT).unwrap().unwrap();
    assert_eq!(doc.finished.recv_timeout(TIMEOUT).unwrap().unwrap(), 1);
    let mut exposed = 0;
    let recovered = RecoveredJournal::open(path.clone(), DocumentId(17), &AtomicBool::new(false), |e| {
        if matches!(
            e,
            RecoveryEvent::BeginBatch(_)
                | RecoveryEvent::BeginReplacement(_)
                | RecoveryEvent::EditBytes(_)
                | RecoveryEvent::EndReplacement
                | RecoveryEvent::CommitBatch
        ) {
            exposed += 1;
        }
        Ok(())
    })
    .unwrap();
    assert_eq!(exposed, 0);
    assert_eq!(recovered.summary().revision, 1);
    assert_eq!(recovered.summary().byte_len, 4);
    let mut resumed = JournalService::spawn(Duration::from_secs(60)).unwrap();
    let doc = resumed.try_resume(recovered).unwrap();
    doc.ready.recv_timeout(TIMEOUT).unwrap().unwrap();
    for command in batch_commands(true) {
        doc.try_submit(command).unwrap();
    }
    assert_eq!(doc.try_flush().unwrap().recv_timeout(TIMEOUT).unwrap().unwrap(), 9);
    resumed.finish();
    resumed.finished.recv_timeout(TIMEOUT).unwrap().unwrap();
    let summary = JournalReader::open(&path).unwrap().recover(&AtomicBool::new(false), |_| Ok(())).unwrap();
    assert_eq!(summary.revision, 9);
    assert_eq!(summary.byte_len, 5);
    assert_eq!(summary.tail, RecoveryTail::Complete);
}

#[test]
fn kw20_batch_refuses_bad_count_order_and_nested_single_edit_without_partial_commit() {
    for kind in 0..3 {
        let temp = Temp::new();
        let mut worker = create(&temp, b"abcd");
        submit(&worker, JournalCommand::BeginBatch(JournalBatch { from_revision: 1, to_revision: 9, replacements: 2 }));
        match kind {
            0 => submit(&worker, JournalCommand::CommitBatch),
            1 => submit(&worker, JournalCommand::Bytes(vec![1])),
            _ => submit(
                &worker,
                JournalCommand::Begin(JournalEdit {
                    from_revision: 1,
                    to_revision: 2,
                    offset: 0,
                    removed: 0,
                    inserted: 0,
                }),
            ),
        }
        assert!(worker.finished.recv_timeout(TIMEOUT).unwrap().is_err());
        worker.finish();
        let mut exposed = 0;
        let summary = JournalReader::open(&temp.path("journal"))
            .unwrap()
            .recover(&AtomicBool::new(false), |e| {
                if matches!(
                    e,
                    RecoveryEvent::BeginBatch(_)
                        | RecoveryEvent::BeginReplacement(_)
                        | RecoveryEvent::EditBytes(_)
                        | RecoveryEvent::EndReplacement
                        | RecoveryEvent::CommitBatch
                ) {
                    exposed += 1;
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(exposed, 0);
        assert_eq!(summary.revision, 1);
        assert_eq!(summary.byte_len, 4);
        assert_eq!(summary.tail, RecoveryTail::Interrupted);
    }
}

#[cfg(unix)]
#[test]
fn kw20_unix_stale_recovery_and_replaced_endpoint_is_not_removed() {
    use std::os::unix::{fs::DirBuilderExt, net::UnixListener};
    let temp = Temp::new();
    let namespace = "stale".to_string();
    let dir = temp.path(&namespace);
    std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
    let socket = dir.join("open.sock");
    let stale = UnixListener::bind(&socket).unwrap();
    drop(stale);
    let endpoint = InstanceEndpoint { runtime_directory: temp.0.clone(), namespace };
    let first = InstanceServer::spawn(endpoint).unwrap();
    first.ready.recv_timeout(TIMEOUT).unwrap().unwrap();
    fs::remove_file(&socket).unwrap();
    let replacement = UnixListener::bind(&socket).unwrap();
    first.shutdown();
    first.finished.recv_timeout(TIMEOUT).unwrap().unwrap();
    assert!(socket.exists());
    drop(replacement);
}

fn wait_file(temp: &Temp) -> OpenFile {
    OpenFile { path: temp.path("COMMIT_EDITMSG"), line: None, column: None }
}
fn wait_endpoint(temp: &Temp, name: &str) -> InstanceEndpoint {
    InstanceEndpoint { runtime_directory: temp.0.clone(), namespace: format!("{name}-{}", std::process::id()) }
}
fn start_waiter(
    endpoint: &InstanceEndpoint,
    request: OpenFiles,
) -> std::sync::mpsc::Receiver<Result<WaitOutcome, WaitError>> {
    let endpoint = endpoint.clone();
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(send_files_and_wait(&endpoint, &request, TIMEOUT));
    });
    finished
}

#[test]
fn kw90_wait_frame_roundtrips_and_a_launcher_without_wait_still_sends_the_old_frame() {
    let temp = Temp::new();
    let plain = OpenFiles { files: vec![wait_file(&temp)], wait: None };
    let waiting = OpenFiles { wait: Some(0x1122_3344_5566_7788), ..plain.clone() };
    let (old, new) = (plain.encode().unwrap(), waiting.encode().unwrap());
    // A launcher that does not wait must keep sending exactly the frame every earlier
    // version understands, and a new primary must keep decoding such a frame.
    assert_eq!(&old[..4], b"KWI1");
    assert_eq!(&new[..4], b"KWI2");
    assert_eq!(OpenFiles::decode(&old).unwrap(), plain);
    assert_eq!(OpenFiles::decode(&new).unwrap(), waiting);
    assert_eq!(new.len(), old.len() + 8, "the wait frame differs from the old one by the token only");
    assert_eq!(new[12..], old[4..], "file list layout is unchanged");
    // Token 0 means "none" and is never valid on the wire; the frame cannot be cut short.
    assert!(OpenFiles { wait: Some(0), ..plain }.encode().is_err());
    let mut zero = new.clone();
    zero[4..12].copy_from_slice(&0u64.to_le_bytes());
    assert!(OpenFiles::decode(&zero).is_err());
    for cut in [5, 11, 12, new.len() - 1] {
        assert!(OpenFiles::decode(&new[..cut]).is_err());
    }
    let mut unknown = new;
    unknown[3] = b'9';
    assert!(OpenFiles::decode(&unknown).is_err());
}

#[test]
fn kw90_a_waiter_returns_only_when_the_primary_completes_it_and_others_are_not_blocked() {
    let temp = Temp::new();
    let endpoint = wait_endpoint(&temp, "wait-close");
    let server = InstanceServer::spawn(endpoint.clone()).unwrap();
    server.ready.recv_timeout(TIMEOUT).unwrap().unwrap();
    let request = OpenFiles { files: vec![wait_file(&temp)], wait: Some(41) };
    let waiter = start_waiter(&endpoint, request.clone());
    assert_eq!(server.requests.recv_timeout(TIMEOUT).unwrap(), request, "the primary gets the files and the token");
    assert!(waiter.recv_timeout(Duration::from_millis(400)).is_err(), "the tab is still open: the waiter blocks");
    // A parked waiter must not hold the endpoint: a plain launch and a second waiter still get through.
    let plain = OpenFiles { files: vec![wait_file(&temp)], wait: None };
    send_files(&endpoint, &plain, TIMEOUT).unwrap();
    assert_eq!(server.requests.recv_timeout(TIMEOUT).unwrap(), plain);
    let second = OpenFiles { files: vec![wait_file(&temp)], wait: Some(42) };
    let second_waiter = start_waiter(&endpoint, second.clone());
    assert_eq!(server.requests.recv_timeout(TIMEOUT).unwrap(), second);
    // A reused token is refused, never allowed to steal the first waiter's answer.
    let reused = send_files_and_wait(&endpoint, &request, TIMEOUT);
    assert!(matches!(reused, Err(WaitError::NotDelivered(_))), "{reused:?}");
    // The tab closes: exactly its own waiter wakes.
    server.waits.complete(41, WaitOutcome::Closed);
    assert_eq!(waiter.recv_timeout(TIMEOUT).unwrap().unwrap(), WaitOutcome::Closed);
    assert!(second_waiter.recv_timeout(Duration::from_millis(300)).is_err(), "the other file is still open");
    server.waits.complete(42, WaitOutcome::OpenFailed);
    assert_eq!(second_waiter.recv_timeout(TIMEOUT).unwrap().unwrap(), WaitOutcome::OpenFailed);
    server.shutdown();
    server.finished.recv_timeout(TIMEOUT).unwrap().unwrap();
}

#[test]
fn kw90_a_waiter_returns_when_the_primary_exits() {
    let temp = Temp::new();
    let endpoint = wait_endpoint(&temp, "wait-exit");
    let server = InstanceServer::spawn(endpoint.clone()).unwrap();
    server.ready.recv_timeout(TIMEOUT).unwrap().unwrap();
    let waiter = start_waiter(&endpoint, OpenFiles { files: vec![wait_file(&temp)], wait: Some(7) });
    server.requests.recv_timeout(TIMEOUT).unwrap();
    assert!(waiter.recv_timeout(Duration::from_millis(300)).is_err());
    // The editor showed the file (KW-93: only a shown request may report success).
    server.waits.shown(7);
    // Quitting normally answers every parked launcher: success, not a hang.
    server.shutdown();
    assert_eq!(waiter.recv_timeout(TIMEOUT).unwrap().unwrap(), WaitOutcome::Closed);
    assert!(server.waits.drain(TIMEOUT), "the answer reached the launcher before the process may exit");
    server.finished.recv_timeout(TIMEOUT).unwrap().unwrap();
    // A launcher that arrives after shutdown began is not parked forever either.
    let late = send_files_and_wait(&endpoint, &OpenFiles { wait: Some(8), ..Default::default() }, TIMEOUT);
    assert!(matches!(late, Err(WaitError::NotDelivered(_))), "{late:?}");
}
