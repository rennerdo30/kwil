use super::*;
use std::{
    fs,
    io::Cursor,
    sync::atomic::{AtomicU64, AtomicUsize},
};

const TIMEOUT: Duration = Duration::from_secs(5);
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "kwil-logical-journal-{}-{}",
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
fn base(byte_len: u64) -> JournalBase {
    JournalBase {
        document: DocumentId(17),
        revision: 9,
        original_path: None,
        original_identity: [1, 2, 3, 4],
        codec: b"provenance-capsule/v1".to_vec(),
        byte_len,
    }
}
fn commands() -> Vec<JournalCommand> {
    vec![
        JournalCommand::BeginBatch(JournalBatch { from_revision: 9, to_revision: 2, replacements: 3 }),
        JournalCommand::BeginEncodedReplacement {
            replacement: JournalReplacement { offset: 3, removed: 0, inserted: 4 },
            payload_len: 2,
        },
        JournalCommand::Bytes(vec![0x91, 0x92]),
        JournalCommand::EndReplacement,
        JournalCommand::BeginReplacement(JournalReplacement { offset: 6, removed: 1, inserted: 1 }),
        JournalCommand::Bytes(vec![b'Z']),
        JournalCommand::EndReplacement,
        JournalCommand::BeginEncodedReplacement {
            replacement: JournalReplacement { offset: 0, removed: 2, inserted: 0 },
            payload_len: 3,
        },
        JournalCommand::Bytes(vec![0xa1, 0xa2, 0xa3]),
        JournalCommand::EndReplacement,
        JournalCommand::CommitBatch,
    ]
}
fn fixture(path: &Path) -> (u64, Vec<u8>) {
    let mut file = private_new(path).unwrap();
    seed(
        &mut file,
        &base(6),
        Some(3),
        Some(Box::new(Cursor::new(vec![0xff, 0xfe, 65, 0, 0xff, 0]))),
        &AtomicBool::new(false),
    )
    .unwrap();
    let base_end = file.stream_position().unwrap();
    let (tx, rx) = mpsc::sync_channel(JOURNAL_QUEUE);
    for command in commands() {
        tx.try_send(command).unwrap();
    }
    drop(tx);
    let result = run(&mut file, State::new(9, 3), rx, Duration::from_secs(60), &AtomicBool::new(false));
    assert!(result.is_ok(), "physical payload completion is independent of logical insert length: {result:?}");
    assert_eq!(result.unwrap(), 2);
    drop(file);
    (base_end, fs::read(path).unwrap())
}
fn batch_event(event: &RecoveryEvent<'_>) -> bool {
    matches!(
        event,
        RecoveryEvent::BeginBatch(_)
            | RecoveryEvent::BeginReplacement(_)
            | RecoveryEvent::BeginEncodedReplacement { .. }
            | RecoveryEvent::EditBytes(_)
            | RecoveryEvent::EndReplacement
            | RecoveryEvent::CommitBatch
            | RecoveryEvent::BeginEdit(_)
            | RecoveryEvent::CommitEdit
    )
}

#[test]
fn kw20_encoded_mixed_payloads_replay_logical_lengths_and_decreasing_revisions() {
    let temp = Temp::new();
    let path = temp.path("mixed");
    fixture(&path);
    let (mut physical_base, mut payloads, mut headers, mut commits) = (Vec::new(), Vec::new(), Vec::new(), 0);
    let summary = JournalReader::open(&path)
        .unwrap()
        .recover(&AtomicBool::new(false), |event| {
            match event {
                RecoveryEvent::EncodedBase { base, logical_len } => {
                    assert_eq!(base.byte_len, 6, "physical base length");
                    assert_eq!(logical_len, 3, "logical base length");
                    assert_eq!(base.codec, b"provenance-capsule/v1");
                }
                RecoveryEvent::Base(_) => panic!("encoded base reached raw consumer"),
                RecoveryEvent::BaseBytes(bytes) => physical_base.extend_from_slice(bytes),
                RecoveryEvent::BeginEncodedReplacement { replacement, payload_len } => {
                    headers.push((replacement.inserted, payload_len))
                }
                RecoveryEvent::EditBytes(bytes) => {
                    assert!(bytes.len() <= JOURNAL_CHUNK);
                    payloads.extend_from_slice(bytes);
                }
                RecoveryEvent::CommitBatch => commits += 1,
                _ => (),
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(physical_base, [0xff, 0xfe, 65, 0, 0xff, 0]);
    assert_eq!(headers, [(4, 2), (0, 3)], "logical inserts and serialized payloads are independent");
    assert_eq!(payloads, [0x91, 0x92, b'Z', 0xa1, 0xa2, 0xa3]);
    assert_eq!(commits, 1);
    assert_eq!((summary.revision, summary.byte_len), (2, 5), "outer commit publishes logical result");
}

#[test]
fn kw20_encoded_interrupted_boundaries_and_frozen_reader_never_publish_batch() {
    let temp = Temp::new();
    let path = temp.path("full");
    let (base_end, bytes) = fixture(&path);
    let mut cursor = base_end as usize;
    let mut cuts = vec![cursor];
    while cursor < bytes.len() {
        let n = u32::from_le_bytes(bytes[cursor + 4..cursor + 8].try_into().unwrap()) as usize;
        cursor += 12 + n;
        if cursor < bytes.len() {
            cuts.push(cursor);
        }
    }
    cuts.push(bytes.len() - 1);
    for (index, cut) in cuts.iter().copied().enumerate() {
        let prefix = temp.path(&format!("prefix-{index}"));
        fs::write(&prefix, &bytes[..cut]).unwrap();
        let mut emitted = 0;
        let result = JournalReader::open(&prefix)
            .unwrap()
            .recover(&AtomicBool::new(false), |event| {
                emitted += usize::from(batch_event(&event));
                Ok(())
            })
            .unwrap();
        assert_eq!(emitted, 0, "no partial batch events at cut {cut}");
        assert_eq!(
            (result.revision, result.byte_len, result.committed_end),
            (9, 3, base_end),
            "prior logical revision at cut {cut}"
        );
    }
    let prefix = temp.path("frozen");
    let cut = *cuts.iter().filter(|&&c| c < bytes.len() - 1).max().unwrap();
    fs::write(&prefix, &bytes[..cut]).unwrap();
    let mut frozen = JournalReader::open(&prefix).unwrap();
    OpenOptions::new().append(true).open(&prefix).unwrap().write_all(&bytes[cut..]).unwrap();
    let result = frozen.recover(&AtomicBool::new(false), |_| Ok(())).unwrap();
    assert_eq!((result.revision, result.byte_len, result.tail), (9, 3, RecoveryTail::Interrupted));
    assert_eq!(JournalReader::open(&prefix).unwrap().recover(&AtomicBool::new(false), |_| Ok(())).unwrap().revision, 2);
}

#[test]
fn kw20_encoded_checksum_failure_emits_no_replacement_or_batch() {
    let temp = Temp::new();
    let path = temp.path("checksum");
    let (_, mut bytes) = fixture(&path);
    // Damage the final outer commit checksum, after all replacement data passed.
    let n = bytes.len();
    bytes[n - 5] ^= 1;
    fs::write(&path, bytes).unwrap();
    let mut emitted = 0;
    let result = JournalReader::open(&path).unwrap().recover(&AtomicBool::new(false), |event| {
        emitted += usize::from(batch_event(&event));
        Ok(())
    });
    assert!(result.is_err(), "outer checksum damage refused");
    assert_eq!(emitted, 0, "checksum validation precedes every batch callback");
}

#[test]
fn kw20_raw_magic_looking_codec_keeps_identical_v1_wire_and_raw_events() {
    let temp = Temp::new();
    let path = temp.path("raw");
    let mut metadata = base(2);
    metadata.codec = vec![ENCODED_BASE, ENCODED_VERSION, b'K', b'W', b'J', b'1', 0xff];
    let mut expected = vec![1];
    for n in [17u64, 9, 2, 1, 2, 3, 4] {
        expected.extend_from_slice(&n.to_le_bytes());
    }
    expected.extend_from_slice(&7u32.to_le_bytes());
    expected.extend_from_slice(&metadata.codec);
    expected.extend_from_slice(&0u32.to_le_bytes());
    assert_eq!(base_frame(&metadata, None).unwrap(), expected, "raw base wire is unchanged");
    let mut file = private_new(&path).unwrap();
    seed(&mut file, &metadata, None, Some(Box::new(Cursor::new(vec![1, 2, 3]))), &AtomicBool::new(false)).unwrap();
    drop(file);
    let mut raw = false;
    let summary = JournalReader::open(&path)
        .unwrap()
        .recover(&AtomicBool::new(false), |event| {
            match event {
                RecoveryEvent::Base(b) => {
                    assert_eq!(b.codec, metadata.codec);
                    raw = true;
                }
                RecoveryEvent::EncodedBase { .. } => panic!("opaque raw codec was auto-detected"),
                _ => (),
            }
            Ok(())
        })
        .unwrap();
    assert!(raw);
    assert_eq!(summary.byte_len, 2, "legacy seed consumes declared physical bytes and ignores suffix");
}

#[test]
fn kw20_encoded_headers_refuse_unknown_versions_layouts_identity_and_codec_bounds() {
    let metadata = base(6).with_logical_len(3).unwrap();
    assert_eq!(metadata.base().byte_len, 6);
    assert_eq!(metadata.logical_len(), 3);
    let header = base_frame(metadata.base(), Some(metadata.logical_len())).unwrap();
    let replacement = encoded_replacement_bytes(JournalReplacement { offset: 1, removed: 2, inserted: 9 }, 4);
    for kind in 0..4 {
        let mut b = header.clone();
        match kind {
            0 => b[1] = 2,
            1 => b.push(0),
            2 => b.truncate(9),
            _ => b[10..18].fill(0),
        }
        assert!(decode_base(&b).is_err(), "malformed encoded base case {kind}");
        let mut b = replacement.clone();
        match kind {
            0 => b[1] = 2,
            1 => b.push(0),
            2 => b.truncate(33),
            _ => b[0] = 0,
        }
        assert!(decode_encoded_replacement(&b).is_err(), "malformed encoded replacement case {kind}");
    }
    let mut over = base(0);
    over.codec.resize(129, 0);
    assert!(over.with_logical_len(0).is_err(), "typed constructor validates codec cap");
    let mut b = header;
    b[66..70].copy_from_slice(&129u32.to_le_bytes());
    assert!(decode_base(&b).is_err(), "wire codec cap is independently validated");
}

#[test]
fn kw20_encoded_ranges_and_logical_or_physical_overflow_are_refused() {
    let batch = JournalBatch { from_revision: 9, to_revision: 2, replacements: 2 };
    for replacement in [
        JournalReplacement { offset: 4, removed: 0, inserted: 1 },
        JournalReplacement { offset: 1, removed: 3, inserted: 1 },
        JournalReplacement { offset: 0, removed: 0, inserted: u64::MAX },
    ] {
        let mut state = State::new(9, 3);
        state.begin_batch(batch).unwrap();
        assert!(
            state.begin_payload(replacement, 0).is_err(),
            "logical range/overflow must reject before accepting payload"
        );
        assert_eq!((state.revision, state.len), (9, 3));
    }
    let mut state = State::new(9, 3);
    state.begin_batch(batch).unwrap();
    state.begin_payload(JournalReplacement { offset: 0, removed: 0, inserted: u64::MAX - 3 }, 0).unwrap();
    state.end_replacement().unwrap();
    assert!(
        state.begin_payload(JournalReplacement { offset: 0, removed: 0, inserted: 1 }, 0).is_err(),
        "sequential logical overflow"
    );
    assert_eq!((state.revision, state.len), (9, 3), "no premature outer publication");
    let mut state = State::new(9, 3);
    state.begin_batch(batch).unwrap();
    state.begin_payload(JournalReplacement { offset: 0, removed: 0, inserted: 0 }, u64::MAX).unwrap();
    state.batch.as_mut().unwrap().pending.as_mut().unwrap().received = u64::MAX;
    assert!(state.bytes(1).is_err(), "physical received count cannot wrap");
}

#[test]
fn kw20_encoded_payload_short_long_and_premature_commit_refused_before_callbacks() {
    let temp = Temp::new();
    for n in [2, 4] {
        let path = temp.path(&format!("payload-{n}"));
        let mut file = private_new(&path).unwrap();
        seed(&mut file, &base(0), Some(3), Some(Box::new(Cursor::new(Vec::<u8>::new()))), &AtomicBool::new(false))
            .unwrap();
        write_frame(&mut file, &batch_bytes(JournalBatch { from_revision: 9, to_revision: 2, replacements: 1 }))
            .unwrap();
        write_frame(
            &mut file,
            &encoded_replacement_bytes(JournalReplacement { offset: 0, removed: 1, inserted: 8 }, 3),
        )
        .unwrap();
        let mut payload = vec![5];
        payload.resize(n + 1, 42);
        write_frame(&mut file, &payload).unwrap();
        write_frame(&mut file, &[9]).unwrap();
        write_frame(&mut file, &[10]).unwrap();
        drop(file);
        let mut emitted = 0;
        assert!(
            JournalReader::open(&path)
                .unwrap()
                .recover(&AtomicBool::new(false), |event| {
                    emitted += usize::from(batch_event(&event));
                    Ok(())
                })
                .is_err(),
            "physical payload length {n} refused"
        );
        assert_eq!(emitted, 0);
    }
    let mut state = State::new(9, 3);
    state.begin_batch(JournalBatch { from_revision: 9, to_revision: 2, replacements: 1 }).unwrap();
    state.begin_payload(JournalReplacement { offset: 0, removed: 1, inserted: 8 }, 3).unwrap();
    state.bytes(2).unwrap();
    assert!(state.end_replacement().is_err(), "short physical replacement cannot finish");
    assert!(state.commit_batch().is_err(), "unfinished encoded replacement cannot commit");
    assert_eq!((state.revision, state.len), (9, 3));
}

#[test]
fn kw20_encoded_base_short_long_and_checksum_refused_before_base_events() {
    let temp = Temp::new();
    for n in [2, 4] {
        let path = temp.path(&format!("base-{n}"));
        let mut file = private_new(&path).unwrap();
        write_frame(&mut file, &base_frame(&base(3), Some(8)).unwrap()).unwrap();
        let mut payload = vec![2];
        payload.resize(n + 1, 42);
        write_frame(&mut file, &payload).unwrap();
        write_frame(&mut file, &[3]).unwrap();
        drop(file);
        let mut emitted = 0;
        assert!(
            JournalReader::open(&path)
                .unwrap()
                .recover(&AtomicBool::new(false), |_| {
                    emitted += 1;
                    Ok(())
                })
                .is_err()
        );
        assert_eq!(emitted, 0, "malformed physical base emits no metadata or data");
    }
    let path = temp.path("base-checksum");
    let (base_end, mut bytes) = fixture(&path);
    bytes[base_end as usize - 5] ^= 1;
    fs::write(&path, bytes).unwrap();
    let mut emitted = 0;
    assert!(
        JournalReader::open(&path)
            .unwrap()
            .recover(&AtomicBool::new(false), |_| {
                emitted += 1;
                Ok(())
            })
            .is_err(),
        "base completion checksum damage refused"
    );
    assert_eq!(emitted, 0, "base checksums precede all base callbacks");
}

#[test]
fn kw20_encoded_interrupted_pending_merge_and_resume_keep_prior_logical_revision() {
    let temp = Temp::new();
    let full = temp.path("full");
    let (base_end, bytes) = fixture(&full);
    let path = temp.path("pending");
    // Crash during a pending merge; the pending stream ends before outer commit.
    fs::write(&path, &bytes[..base_end as usize + 5]).unwrap();
    fs::write(pending_path(&path), &bytes[base_end as usize..bytes.len() - 13]).unwrap();
    let mut emitted = 0;
    let recovered = RecoveredJournal::open(path.clone(), DocumentId(17), &AtomicBool::new(false), |event| {
        emitted += usize::from(batch_event(&event));
        Ok(())
    })
    .unwrap();
    assert_eq!(emitted, 0, "uncommitted encoded pending batch is never replayed");
    assert_eq!((recovered.summary().revision, recovered.summary().byte_len), (9, 3));
    assert_eq!(fs::metadata(&path).unwrap().len(), base_end, "only interrupted tail is truncated");
    assert!(!pending_path(&path).exists());
    drop(recovered);
    let mut frozen = JournalReader::open(&path).unwrap();
    let mut worker = JournalWorker::resume(path.clone(), DocumentId(17), Duration::from_secs(60)).unwrap();
    assert_eq!(worker.ready.recv_timeout(TIMEOUT).unwrap().unwrap(), 9);
    for command in [
        JournalCommand::BeginBatch(JournalBatch { from_revision: 9, to_revision: 1, replacements: 1 }),
        JournalCommand::BeginEncodedReplacement {
            replacement: JournalReplacement { offset: 3, removed: 0, inserted: 2 },
            payload_len: 1,
        },
        JournalCommand::Bytes(vec![0xaa]),
        JournalCommand::EndReplacement,
        JournalCommand::CommitBatch,
    ] {
        worker.try_submit(command).unwrap();
    }
    assert_eq!(worker.try_flush().unwrap().recv_timeout(TIMEOUT).unwrap().unwrap(), 1);
    worker.finish();
    worker.finished.recv_timeout(TIMEOUT).unwrap().unwrap();
    assert_eq!(
        frozen.recover(&AtomicBool::new(false), |_| Ok(())).unwrap().revision,
        9,
        "append cannot extend a captured reader"
    );
    let summary = JournalReader::open(&path).unwrap().recover(&AtomicBool::new(false), |_| Ok(())).unwrap();
    assert_eq!((summary.revision, summary.byte_len, summary.tail), (1, 5, RecoveryTail::Complete));
}

struct Counted {
    data: Cursor<Vec<u8>>,
    consumed: Arc<AtomicUsize>,
    max_request: Arc<AtomicUsize>,
}
impl Read for Counted {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        self.max_request.fetch_max(b.len(), Ordering::Relaxed);
        let n = self.data.read(b)?;
        self.consumed.fetch_add(n, Ordering::Relaxed);
        Ok(n)
    }
}
fn counted(n: usize) -> (Box<dyn Read + Send>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let consumed = Arc::new(AtomicUsize::new(0));
    let max_request = Arc::new(AtomicUsize::new(0));
    (
        Box::new(Counted {
            data: Cursor::new(vec![1; n]),
            consumed: consumed.clone(),
            max_request: max_request.clone(),
        }),
        consumed,
        max_request,
    )
}

#[test]
fn kw20_encoded_seed_worker_and_service_check_exact_physical_eof() {
    let temp = Temp::new();
    for n in [2, 3, 5] {
        let (reader, consumed, max_request) = counted(n);
        let path = temp.path(&format!("worker-{n}"));
        let mut worker = JournalWorker::create_encoded_from_reader(
            path,
            base(3).with_logical_len(8).unwrap(),
            reader,
            Duration::from_secs(60),
        )
        .unwrap();
        let ready = worker.ready.recv_timeout(TIMEOUT).unwrap();
        assert_eq!(ready.is_ok(), n == 3, "worker physical seed size {n}");
        worker.finish();
        let done = worker.finished.recv_timeout(TIMEOUT).unwrap();
        assert_eq!(done.is_ok(), n == 3);
        assert!(consumed.load(Ordering::Relaxed) <= 4, "at most one byte beyond physical base");
        assert!(max_request.load(Ordering::Relaxed) <= JOURNAL_CHUNK);
        let (source, consumed, _) = counted(n);
        let (ready_tx, _ready) = mpsc::sync_channel(1);
        let (done_tx, _done) = mpsc::sync_channel(1);
        let mut entry = ServiceEntry::seed(
            JournalSeed { path: temp.path(&format!("service-{n}")), base: base(3), source },
            Some(8),
            ready_tx,
            done_tx,
            Duration::from_secs(60),
        )
        .unwrap();
        let result = entry.advance().and_then(|_| entry.advance());
        assert_eq!(result.is_ok(), n == 3, "service physical seed size {n}");
        assert!(consumed.load(Ordering::Relaxed) <= 4);
        if n == 3 {
            assert_eq!((entry.state.revision, entry.state.len), (9, 8));
        }
    }
}

#[test]
fn kw20_encoded_service_backpressure_drain_restart_and_bounded_stream() {
    struct Gated {
        first: bool,
        gate: Receiver<()>,
        entered: SyncSender<()>,
        reader: Box<dyn Read + Send>,
    }
    impl Read for Gated {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            if self.first {
                self.first = false;
                self.entered.try_send(()).unwrap();
                self.gate.recv_timeout(TIMEOUT).map_err(io::Error::other)?;
            }
            self.reader.read(b)
        }
    }
    let temp = Temp::new();
    let path = temp.path("bounded");
    let (reader, consumed, max_request) = counted(JOURNAL_CHUNK + 3);
    let (release, gate) = mpsc::sync_channel(1);
    let (entered_tx, entered) = mpsc::sync_channel(1);
    let mut service = JournalService::spawn(Duration::from_secs(60)).unwrap();
    let doc = service
        .try_create_encoded(EncodedJournalSeed {
            path: path.clone(),
            base: base((JOURNAL_CHUNK + 3) as u64).with_logical_len(3).unwrap(),
            source: Box::new(Gated { first: true, gate, entered: entered_tx, reader }),
        })
        .unwrap();
    entered.recv_timeout(TIMEOUT).unwrap();
    doc.try_submit(JournalCommand::BeginBatch(JournalBatch { from_revision: 9, to_revision: 2, replacements: 1 }))
        .unwrap();
    let replacement = JournalReplacement { offset: 3, removed: 0, inserted: 7 };
    doc.try_submit(JournalCommand::BeginEncodedReplacement { replacement, payload_len: 12 }).unwrap();
    for _ in 0..12 {
        doc.try_submit(JournalCommand::Bytes(vec![42])).unwrap();
    }
    doc.try_submit(JournalCommand::EndReplacement).unwrap();
    doc.try_submit(JournalCommand::CommitBatch).unwrap();
    assert!(
        matches!(
            doc.try_submit(JournalCommand::BeginEncodedReplacement { replacement, payload_len: 99 }),
            Err(TrySendError::Full(JournalCommand::BeginEncodedReplacement { payload_len: 99, .. }))
        ),
        "full queue returns exact encoded command"
    );
    release.try_send(()).unwrap();
    doc.ready.recv_timeout(TIMEOUT).unwrap().unwrap();
    assert_eq!(
        doc.try_flush().unwrap().recv_timeout(TIMEOUT).unwrap().unwrap(),
        2,
        "durability follows logical batch commit"
    );
    service.finish();
    service.finished.recv_timeout(TIMEOUT).unwrap().unwrap();
    assert_eq!(doc.finished.recv_timeout(TIMEOUT).unwrap().unwrap(), 2);
    assert_eq!(consumed.load(Ordering::Relaxed), JOURNAL_CHUNK + 3, "physical base copied completely");
    assert_eq!(max_request.load(Ordering::Relaxed), JOURNAL_CHUNK, "seed requests stay chunk bounded");
    let recovered = RecoveredJournal::open(path.clone(), DocumentId(17), &AtomicBool::new(false), |_| Ok(())).unwrap();
    assert_eq!((recovered.summary().revision, recovered.summary().byte_len), (2, 10));
    let mut service = JournalService::spawn(Duration::from_secs(60)).unwrap();
    let doc = service.try_resume(recovered).unwrap();
    doc.ready.recv_timeout(TIMEOUT).unwrap().unwrap();
    // Existing raw Begin retains physical==logical behavior after encoded resume.
    doc.try_submit(JournalCommand::Begin(JournalEdit {
        from_revision: 2,
        to_revision: 1,
        offset: 10,
        removed: 0,
        inserted: 1,
    }))
    .unwrap();
    doc.try_submit(JournalCommand::Bytes(vec![b'!'])).unwrap();
    doc.try_submit(JournalCommand::Commit).unwrap();
    service.finish();
    service.finished.recv_timeout(TIMEOUT).unwrap().unwrap();
    assert_eq!(doc.finished.recv_timeout(TIMEOUT).unwrap().unwrap(), 1);
    let mut physical = 0;
    let mut chunks = 0;
    let summary = JournalReader::open(&path)
        .unwrap()
        .recover(&AtomicBool::new(false), |event| {
            if let RecoveryEvent::BaseBytes(bytes) | RecoveryEvent::EditBytes(bytes) = event {
                assert!(bytes.len() <= JOURNAL_CHUNK);
                physical += bytes.len();
                chunks += 1;
            }
            Ok(())
        })
        .unwrap();
    assert_eq!((summary.revision, summary.byte_len), (1, 11));
    assert_eq!(physical, JOURNAL_CHUNK + 3 + 12 + 1);
    assert_eq!(chunks, 2 + 12 + 1);
}
