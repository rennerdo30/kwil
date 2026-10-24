//! Append-only hot-exit journal. Disk operations belong to this worker.
//! A base is streamed once, edits append bounded chunks; no file-sized Vec or per-key rewrite.
//! Model integration must retain/retry a command on queue Full and only call an edit
//! durable after a successful flush receipt. Recovery is a fixed-length snapshot.
use crate::session::{Decoder, DocumentId, checksum, invalid, private_new, put_path, put32, put64, sync_parent};
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread,
    time::{Duration, Instant},
};

pub const JOURNAL_CHUNK: usize = 64 * 1024;
pub const JOURNAL_QUEUE: usize = 16;
const MAX_RECORD: usize = JOURNAL_CHUNK + 1;
const MAGIC: &[u8; 4] = b"KWJ1";
const ENCODED_BASE: u8 = 11;
const ENCODED_REPLACEMENT: u8 = 12;
const ENCODED_VERSION: u8 = 1;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalBase {
    pub document: DocumentId,
    pub revision: u64,
    /// Informational original name. Recovery uses the embedded raw base, never this path.
    pub original_path: Option<PathBuf>,
    /// Caller-defined stable original identity (e.g. volume/file ID + mtime); never a resident buffer.
    pub original_identity: [u64; 4],
    /// Opaque model codec tag, including encoding/BOM policy. Bytes stay in original encoding.
    pub codec: Vec<u8>,
    /// Physical base payload length. Raw journals also use this as their logical length.
    pub byte_len: u64,
}
impl JournalBase {
    /// Explicitly opt into encoded framing. Codec bytes remain opaque, including
    /// tags resembling envelopes; no raw constructor ever detects a codec tag.
    /// `logical_len` counts model UTF-8 bytes plus opaque atoms, not stored bytes.
    pub fn with_logical_len(self, logical_len: u64) -> io::Result<EncodedJournalBase> {
        base_bytes(&self)?;
        Ok(EncodedJournalBase { base: self, logical_len })
    }
}
/// Validated metadata for a versioned encoded base. The backend never decodes
/// the payload or codec; the model is responsible for provenance restoration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodedJournalBase {
    base: JournalBase,
    logical_len: u64,
}
impl EncodedJournalBase {
    pub fn base(&self) -> &JournalBase {
        &self.base
    }
    pub fn logical_len(&self) -> u64 {
        self.logical_len
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JournalEdit {
    pub from_revision: u64,
    pub to_revision: u64,
    pub offset: u64,
    pub removed: u64,
    pub inserted: u64,
}
/// One model transaction/revision, regardless of how many replacements it contains.
/// Replacement coordinates are sequential: each refers to the preceding result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JournalBatch {
    pub from_revision: u64,
    pub to_revision: u64,
    pub replacements: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JournalReplacement {
    pub offset: u64,
    pub removed: u64,
    pub inserted: u64,
}
#[derive(Debug)]
pub enum JournalCommand {
    Begin(JournalEdit),
    /// At most JOURNAL_CHUNK bytes. Large pastes stream chunks under one Begin/Commit.
    Bytes(Vec<u8>),
    Commit,
    /// BeginBatch -> (BeginReplacement -> Bytes* -> EndReplacement)* -> CommitBatch.
    /// No new revision or recoverable replacement is published before CommitBatch.
    BeginBatch(JournalBatch),
    BeginReplacement(JournalReplacement),
    /// Coordinates and `inserted` are logical; Bytes supplies exactly payload_len
    /// physical bytes. Used inside BeginBatch, with the same outer commit gate.
    BeginEncodedReplacement {
        replacement: JournalReplacement,
        payload_len: u64,
    },
    EndReplacement,
    CommitBatch,
    /// Receipt means sync_all completed, covering all preceding committed edits.
    Flush(SyncSender<io::Result<u64>>),
    Finish,
    Cancel,
}
pub struct JournalWorker {
    tx: Option<SyncSender<JournalCommand>>,
    cancel: Arc<AtomicBool>,
    /// Ready only after the complete base has been fsynced. UI polls; never waits for it.
    pub ready: Receiver<io::Result<u64>>,
    pub finished: Receiver<io::Result<u64>>,
}
impl JournalWorker {
    /// `source` must be an immutable/pinned original opened on an opening worker.
    /// None is valid only for an empty new document. Never pass a live mutable pathname.
    /// The worker streams the source in 64 KiB; it does not retain a model Snapshot.
    pub fn create(path: PathBuf, base: JournalBase, source: Option<File>, interval: Duration) -> io::Result<Self> {
        struct FileReader {
            file: File,
            first: bool,
        }
        impl Read for FileReader {
            fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
                if self.first {
                    self.file.seek(SeekFrom::Start(0))?;
                    self.first = false;
                }
                self.file.read(b)
            }
        }
        Self::start(
            path,
            base,
            None,
            source.map(|file| Box::new(FileReader { file, first: true }) as Box<dyn Read + Send>),
            interval,
        )
    }
    /// Streams an immutable model snapshot through Read on the journal worker.
    /// Reader starts at byte zero and must supply exactly base.byte_len raw bytes.
    /// Its ownership must be cheap (persistent tree/disk references), never a GB Vec.
    pub fn create_from_reader(
        path: PathBuf,
        base: JournalBase,
        source: Box<dyn Read + Send>,
        interval: Duration,
    ) -> io::Result<Self> {
        Self::start(path, base, None, Some(source), interval)
    }
    /// Encoded counterpart of create_from_reader. Supplies exactly the physical
    /// base.byte_len bytes, including a bounded EOF probe; logical_len seeds
    /// editing coordinates. Serialization/restoration belongs to the caller.
    pub fn create_encoded_from_reader(
        path: PathBuf,
        base: EncodedJournalBase,
        source: Box<dyn Read + Send>,
        interval: Duration,
    ) -> io::Result<Self> {
        Self::start(path, base.base, Some(base.logical_len), Some(source), interval)
    }
    fn start(
        path: PathBuf,
        base: JournalBase,
        logical_len: Option<u64>,
        source: Option<Box<dyn Read + Send>>,
        interval: Duration,
    ) -> io::Result<Self> {
        if interval.is_zero() {
            return Err(invalid("zero journal interval"));
        }
        let (tx, rx) = mpsc::sync_channel(JOURNAL_QUEUE);
        let (ready_tx, ready) = mpsc::sync_channel(1);
        let (done_tx, finished) = mpsc::sync_channel(1);
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = cancel.clone();
        thread::Builder::new().name("kwil-journal".into()).spawn(move || {
            let result = (|| {
                let _ownership = journal_lock(&path)?;
                let mut f = private_new(&path)?;
                seed(&mut f, &base, logical_len, source, &stop)?;
                f.sync_all()?;
                if let Some(parent) = path.parent() {
                    sync_parent(parent)?;
                }
                let _ = ready_tx.try_send(Ok(base.revision));
                run(&mut f, State::new(base.revision, logical_len.unwrap_or(base.byte_len)), rx, interval, &stop)
            })();
            if let Err(e) = &result {
                let _ = ready_tx.try_send(Err(io::Error::new(e.kind(), e.to_string())));
            }
            let _ = done_tx.try_send(result);
        })?;
        Ok(Self { tx: Some(tx), cancel, ready, finished })
    }
    /// Nonblocking, bounded ingress. Caller retains command on Full/Disconnected.
    pub fn try_submit(&self, command: JournalCommand) -> Result<(), TrySendError<JournalCommand>> {
        // Reject oversize before it enters the queue. This is a producer contract error.
        if matches!(&command, JournalCommand::Bytes(b) if b.is_empty() || b.len() > JOURNAL_CHUNK) {
            return Err(TrySendError::Disconnected(command));
        }
        match &self.tx {
            Some(tx) => tx.try_send(command),
            None => Err(TrySendError::Disconnected(command)),
        }
    }
    pub fn try_flush(&self) -> Result<Receiver<io::Result<u64>>, TrySendError<JournalCommand>> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.try_submit(JournalCommand::Flush(tx))?;
        Ok(rx)
    }
    /// Stops admission, drains queued commands, syncs and exits. Poll/wait `finished`
    /// on shutdown worker with caller's timeout; never block the UI/crash signal handler.
    pub fn finish(&mut self) {
        self.tx.take();
    }
    /// Cancels seeding/recovery-sized work between chunks. Incomplete tails remain recoverable.
    pub fn cancel(&mut self) {
        self.cancel.store(true, Ordering::Release);
        self.tx.take();
    }

    /// Worker validates the frozen committed prefix, discards only an interrupted
    /// uncommitted tail, then appends. Checksum damage and an incomplete base are
    /// errors and never truncated. Only call after the model accepted recovery.
    pub fn resume(path: PathBuf, expected_document: DocumentId, interval: Duration) -> io::Result<Self> {
        if interval.is_zero() {
            return Err(invalid("zero journal interval"));
        }
        let (tx, rx) = mpsc::sync_channel(JOURNAL_QUEUE);
        let (ready_tx, ready) = mpsc::sync_channel(1);
        let (done_tx, finished) = mpsc::sync_channel(1);
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = cancel.clone();
        thread::Builder::new().name("kwil-journal".into()).spawn(move || {
            let result = (|| {
                let recovered = RecoveredJournal::open(path, expected_document, &stop, |_| Ok(()))?;
                let mut file = recovered.file;
                let _ownership = recovered.ownership;
                let summary = recovered.summary;
                let _ = ready_tx.try_send(Ok(summary.revision));
                run(&mut file, State::new(summary.revision, summary.byte_len), rx, interval, &stop)
            })();
            if let Err(e) = &result {
                let _ = ready_tx.try_send(Err(io::Error::new(e.kind(), e.to_string())));
            }
            let _ = done_tx.try_send(result);
        })?;
        Ok(Self { tx: Some(tx), cancel, ready, finished })
    }
}
impl Drop for JournalWorker {
    fn drop(&mut self) {
        self.tx.take();
    }
}

fn journal_lock(path: &Path) -> io::Result<File> {
    // Byte-range locks on Windows also deny concurrent ReadFile. Lock a sidecar,
    // never the append-only data file that frozen recovery readers must inspect.
    let mut name = path.as_os_str().to_os_string();
    name.push(".lock");
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options.open(PathBuf::from(name))?;
    lock.try_lock().map_err(io::Error::other)?;
    Ok(lock)
}

/// Worker-only transport handoff. Both journal writers must have finished.
/// The fixed sibling is an unselected seed until this atomic replacement;
/// recovery continues to select the canonical journal at every earlier cut.
/// No failure deletes or relabels an uncertain staged journal.
pub fn promote_finished_seed(staged: &Path, canonical: &Path, document: DocumentId) -> io::Result<()> {
    promote_finished_seed_with(staged, canonical, document, crate::session::atomic_replace, sync_parent)
}

fn promote_finished_seed_with(
    staged: &Path,
    canonical: &Path,
    document: DocumentId,
    replace: impl FnOnce(&Path, &Path) -> io::Result<()>,
    sync: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let mut expected = canonical.as_os_str().to_os_string();
    expected.push(".next");
    if document.0 == 0
        || !canonical.is_absolute()
        || !staged.is_absolute()
        || staged != Path::new(&expected)
        || staged.parent() != canonical.parent()
        || canonical.file_name().and_then(|name| name.to_str()) != Some(format!("{}.kwj", document.0).as_str())
    {
        return Err(invalid("transport seed promotion requires the exact canonical .kwj.next sibling"));
    }
    let parent = canonical.parent().ok_or_else(|| invalid("transport journal parent missing"))?;
    let _canonical_lock = journal_lock(canonical)?;
    let _staged_lock = journal_lock(staged)?;
    let metadata = std::fs::symlink_metadata(staged)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(invalid("transport staged seed must be a regular private file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(invalid("transport staged seed permissions are not private"));
        }
    }
    match std::fs::symlink_metadata(canonical) {
        Ok(metadata) if !metadata.is_file() || metadata.file_type().is_symlink() => {
            return Err(invalid("canonical transport journal must be a regular file"));
        }
        Ok(_) => (),
        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
        Err(error) => return Err(error),
    }
    if pending_path(staged).try_exists()? || pending_path(canonical).try_exists()? {
        return Err(invalid("transport promotion refused while pending journal data exists"));
    }
    let file = OpenOptions::new().read(true).write(true).open(staged)?;
    let mut reader = JournalReader { file: file.try_clone()?, limit: file.metadata()?.len() };
    let mut complete = false;
    let summary = reader.recover(&AtomicBool::new(false), |event| {
        match event {
            RecoveryEvent::Base(base) | RecoveryEvent::EncodedBase { base, .. } if base.document == document => (),
            RecoveryEvent::BaseBytes(_) => (),
            RecoveryEvent::BaseComplete => complete = true,
            _ => return Err(invalid("transport promotion requires an authenticated seed with no appended edits")),
        }
        Ok(())
    })?;
    if !complete || summary.tail != RecoveryTail::Complete || summary.committed_end != reader.limit {
        return Err(invalid("transport staged seed is incomplete; canonical recovery retained"));
    }
    file.sync_all()?;
    drop(reader);
    drop(file);
    replace(staged, canonical)?;
    sync(parent).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("transport journal promotion committed; parent durability uncertain: {error}"),
        )
    })
}

fn base_bytes(base: &JournalBase) -> io::Result<Vec<u8>> {
    if base.document.0 == 0 || base.codec.len() > 128 {
        return Err(invalid("base identity or codec"));
    }
    let mut b = vec![1];
    put64(&mut b, base.document.0);
    put64(&mut b, base.revision);
    put64(&mut b, base.byte_len);
    for v in base.original_identity {
        put64(&mut b, v);
    }
    put32(&mut b, base.codec.len() as u32);
    b.extend_from_slice(&base.codec);
    put_path(&mut b, base.original_path.as_deref())?;
    Ok(b)
}
fn base_frame(base: &JournalBase, logical_len: Option<u64>) -> io::Result<Vec<u8>> {
    let raw = base_bytes(base)?;
    let Some(logical_len) = logical_len else { return Ok(raw) };
    let mut b = vec![ENCODED_BASE, ENCODED_VERSION];
    put64(&mut b, logical_len);
    b.extend_from_slice(&raw[1..]);
    Ok(b)
}
fn decode_base(b: &[u8]) -> io::Result<(JournalBase, Option<u64>)> {
    let mut r = Decoder::new(b);
    let logical_len = match r.take(1)?[0] {
        1 => None,
        ENCODED_BASE => {
            if r.take(1)? != [ENCODED_VERSION] {
                return Err(invalid("encoded base version"));
            }
            Some(r.u64()?)
        }
        _ => return Err(invalid("missing journal base")),
    };
    let document = DocumentId(r.u64()?);
    let revision = r.u64()?;
    let byte_len = r.u64()?;
    let original_identity = [r.u64()?, r.u64()?, r.u64()?, r.u64()?];
    let n = r.count(128)?;
    let codec = r.take(n)?.to_vec();
    let original_path = r.path()?;
    r.end()?;
    if document.0 == 0 {
        return Err(invalid("zero document ID"));
    }
    Ok((JournalBase { document, revision, byte_len, original_identity, original_path, codec }, logical_len))
}
fn seed(
    f: &mut File,
    base: &JournalBase,
    logical_len: Option<u64>,
    mut source: Option<Box<dyn Read + Send>>,
    stop: &AtomicBool,
) -> io::Result<()> {
    write_frame(f, &base_frame(base, logical_len)?)?;
    let mut left = base.byte_len;
    let mut b = vec![0; JOURNAL_CHUNK + 1];
    b[0] = 2;
    if source.is_none() && left != 0 {
        return Err(invalid("missing base source"));
    }
    while left != 0 {
        cancelled(stop)?;
        let n = left.min(JOURNAL_CHUNK as u64) as usize;
        source.as_mut().unwrap().read_exact(&mut b[1..n + 1])?;
        write_frame(f, &b[..n + 1])?;
        left -= n as u64;
    }
    // Legacy raw readers historically consume exactly byte_len and ignore any
    // suffix. Encoded framing requires an exact physical payload, even at zero.
    if logical_len.is_some() {
        cancelled(stop)?;
        if let Some(reader) = &mut source {
            require_eof(reader)?;
        }
    }
    write_frame(f, &[3])
}
fn require_eof(reader: &mut dyn Read) -> io::Result<()> {
    let mut byte = [0];
    if reader.read(&mut byte)? != 0 {
        return Err(invalid("too many encoded base bytes"));
    }
    Ok(())
}
fn cancelled(stop: &AtomicBool) -> io::Result<()> {
    if stop.load(Ordering::Acquire) {
        Err(io::Error::new(io::ErrorKind::Interrupted, "journal cancelled"))
    } else {
        Ok(())
    }
}
fn write_frame(f: &mut File, b: &[u8]) -> io::Result<()> {
    if b.is_empty() || b.len() > MAX_RECORD {
        return Err(invalid("journal record limit"));
    }
    f.write_all(MAGIC)?;
    f.write_all(&(b.len() as u32).to_le_bytes())?;
    f.write_all(&checksum(b).to_le_bytes())?;
    f.write_all(b)
}
fn edit_bytes(edit: JournalEdit) -> Vec<u8> {
    let mut b = vec![4];
    for v in [edit.from_revision, edit.to_revision, edit.offset, edit.removed, edit.inserted] {
        put64(&mut b, v);
    }
    b
}
fn decode_edit(b: &[u8]) -> io::Result<JournalEdit> {
    let mut r = Decoder::new(b);
    if r.take(1)? != [4] {
        return Err(invalid("edit header"));
    }
    let e = JournalEdit {
        from_revision: r.u64()?,
        to_revision: r.u64()?,
        offset: r.u64()?,
        removed: r.u64()?,
        inserted: r.u64()?,
    };
    r.end()?;
    Ok(e)
}
fn batch_bytes(batch: JournalBatch) -> Vec<u8> {
    let mut b = vec![7];
    for v in [batch.from_revision, batch.to_revision, batch.replacements] {
        put64(&mut b, v);
    }
    b
}
fn decode_batch(b: &[u8]) -> io::Result<JournalBatch> {
    let mut r = Decoder::new(b);
    if r.take(1)? != [7] {
        return Err(invalid("batch header"));
    }
    let batch = JournalBatch { from_revision: r.u64()?, to_revision: r.u64()?, replacements: r.u64()? };
    r.end()?;
    Ok(batch)
}
fn replacement_bytes(replacement: JournalReplacement) -> Vec<u8> {
    let mut b = vec![8];
    for v in [replacement.offset, replacement.removed, replacement.inserted] {
        put64(&mut b, v);
    }
    b
}
fn decode_replacement(b: &[u8]) -> io::Result<JournalReplacement> {
    let mut r = Decoder::new(b);
    if r.take(1)? != [8] {
        return Err(invalid("replacement header"));
    }
    let replacement = JournalReplacement { offset: r.u64()?, removed: r.u64()?, inserted: r.u64()? };
    r.end()?;
    Ok(replacement)
}
fn encoded_replacement_bytes(replacement: JournalReplacement, payload_len: u64) -> Vec<u8> {
    let mut b = vec![ENCODED_REPLACEMENT, ENCODED_VERSION];
    for v in [replacement.offset, replacement.removed, replacement.inserted, payload_len] {
        put64(&mut b, v);
    }
    b
}
fn decode_encoded_replacement(b: &[u8]) -> io::Result<(JournalReplacement, u64)> {
    let mut r = Decoder::new(b);
    if r.take(2)? != [ENCODED_REPLACEMENT, ENCODED_VERSION] {
        return Err(invalid("encoded replacement version/header"));
    }
    let replacement = JournalReplacement { offset: r.u64()?, removed: r.u64()?, inserted: r.u64()? };
    let payload_len = r.u64()?;
    r.end()?;
    Ok((replacement, payload_len))
}
#[derive(Clone, Copy)]
struct ReplacementState {
    replacement: JournalReplacement,
    payload_len: u64,
    received: u64,
}
#[derive(Clone, Copy)]
struct BatchState {
    header: JournalBatch,
    completed: u64,
    len: u64,
    pending: Option<ReplacementState>,
}
#[derive(Clone, Copy)]
struct State {
    revision: u64,
    len: u64,
    pending: Option<(JournalEdit, u64)>,
    batch: Option<BatchState>,
}
impl State {
    fn new(revision: u64, len: u64) -> Self {
        Self { revision, len, pending: None, batch: None }
    }
    fn begin(&mut self, e: JournalEdit) -> io::Result<()> {
        if self.pending.is_some()
            || self.batch.is_some()
            || e.from_revision != self.revision
            || e.to_revision == e.from_revision
            || e.offset > self.len
            || e.removed > self.len - e.offset
            || self.len.checked_sub(e.removed).and_then(|n| n.checked_add(e.inserted)).is_none()
        {
            return Err(invalid("journal edit revision/range/order"));
        }
        self.pending = Some((e, 0));
        Ok(())
    }
    fn bytes(&mut self, n: usize) -> io::Result<()> {
        if let Some(batch) = &mut self.batch {
            let pending = batch.pending.as_mut().ok_or_else(|| invalid("bytes outside batch replacement"))?;
            pending.received = pending.received.checked_add(n as u64).ok_or_else(|| invalid("edit overflow"))?;
            if pending.received > pending.payload_len {
                return Err(invalid("too many replacement bytes"));
            }
            return Ok(());
        }
        let (e, nbytes) = self.pending.as_mut().ok_or_else(|| invalid("bytes outside edit"))?;
        *nbytes = nbytes.checked_add(n as u64).ok_or_else(|| invalid("edit overflow"))?;
        if *nbytes > e.inserted {
            return Err(invalid("too many edit bytes"));
        }
        Ok(())
    }
    fn commit(&mut self) -> io::Result<JournalEdit> {
        let (e, n) = self.pending.ok_or_else(|| invalid("commit outside edit"))?;
        if n != e.inserted {
            return Err(invalid("incomplete edit"));
        }
        self.pending = None;
        self.revision = e.to_revision;
        self.len = self.len - e.removed + e.inserted;
        Ok(e)
    }
    fn begin_batch(&mut self, header: JournalBatch) -> io::Result<()> {
        if self.pending.is_some()
            || self.batch.is_some()
            || header.from_revision != self.revision
            || header.to_revision == header.from_revision
            || header.replacements == 0
        {
            return Err(invalid("journal batch revision/order/count"));
        }
        self.batch = Some(BatchState { header, completed: 0, len: self.len, pending: None });
        Ok(())
    }
    fn begin_replacement(&mut self, replacement: JournalReplacement) -> io::Result<()> {
        self.begin_payload(replacement, replacement.inserted)
    }
    fn begin_payload(&mut self, replacement: JournalReplacement, payload_len: u64) -> io::Result<()> {
        let batch = self.batch.as_mut().ok_or_else(|| invalid("replacement outside batch"))?;
        if batch.pending.is_some()
            || batch.completed >= batch.header.replacements
            || replacement.offset > batch.len
            || replacement.removed > batch.len - replacement.offset
            || batch.len.checked_sub(replacement.removed).and_then(|n| n.checked_add(replacement.inserted)).is_none()
        {
            return Err(invalid("batch replacement range/order"));
        }
        batch.pending = Some(ReplacementState { replacement, payload_len, received: 0 });
        Ok(())
    }
    fn end_replacement(&mut self) -> io::Result<()> {
        let batch = self.batch.as_mut().ok_or_else(|| invalid("replacement end outside batch"))?;
        let pending = batch.pending.ok_or_else(|| invalid("replacement end without begin"))?;
        if pending.received != pending.payload_len {
            return Err(invalid("incomplete replacement"));
        }
        let replacement = pending.replacement;
        batch.pending = None;
        batch.len = batch.len - replacement.removed + replacement.inserted;
        batch.completed += 1;
        Ok(())
    }
    fn commit_batch(&mut self) -> io::Result<JournalBatch> {
        let batch = self.batch.ok_or_else(|| invalid("batch commit outside batch"))?;
        if batch.pending.is_some() || batch.completed != batch.header.replacements {
            return Err(invalid("incomplete batch"));
        }
        self.revision = batch.header.to_revision;
        self.len = batch.len;
        self.batch = None;
        Ok(batch.header)
    }
}
fn run(
    f: &mut File,
    mut state: State,
    rx: Receiver<JournalCommand>,
    interval: Duration,
    stop: &AtomicBool,
) -> io::Result<u64> {
    let mut dirty = false;
    let mut due = Instant::now();
    loop {
        cancelled(stop)?;
        let command = if dirty {
            match rx.recv_timeout(due.saturating_duration_since(Instant::now())) {
                Ok(c) => Some(c),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    f.sync_all()?;
                    dirty = false;
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => None,
            }
        } else {
            rx.recv().ok()
        };
        let Some(command) = command else {
            f.sync_all()?;
            return Ok(state.revision);
        };
        match command {
            JournalCommand::Begin(e) => {
                state.begin(e)?;
                write_frame(f, &edit_bytes(e))?;
            }
            JournalCommand::Bytes(b) => {
                if b.len() > JOURNAL_CHUNK {
                    return Err(invalid("edit chunk limit"));
                }
                state.bytes(b.len())?;
                let mut frame = Vec::with_capacity(b.len() + 1);
                frame.push(5);
                frame.extend(b);
                write_frame(f, &frame)?;
            }
            JournalCommand::Commit => {
                state.commit()?;
                write_frame(f, &[6])?;
            }
            JournalCommand::BeginBatch(b) => {
                state.begin_batch(b)?;
                write_frame(f, &batch_bytes(b))?;
            }
            JournalCommand::BeginReplacement(r) => {
                state.begin_replacement(r)?;
                write_frame(f, &replacement_bytes(r))?;
            }
            JournalCommand::BeginEncodedReplacement { replacement, payload_len } => {
                state.begin_payload(replacement, payload_len)?;
                write_frame(f, &encoded_replacement_bytes(replacement, payload_len))?;
            }
            JournalCommand::EndReplacement => {
                state.end_replacement()?;
                write_frame(f, &[9])?;
            }
            JournalCommand::CommitBatch => {
                state.commit_batch()?;
                write_frame(f, &[10])?;
            }
            JournalCommand::Flush(reply) => {
                let result = f.sync_all().map(|_| state.revision);
                let failed = result.is_err();
                let _ = reply.try_send(result);
                if failed {
                    return Err(io::Error::other("journal flush failed"));
                }
                dirty = false;
                continue;
            }
            JournalCommand::Finish => {
                f.sync_all()?;
                return Ok(state.revision);
            }
            JournalCommand::Cancel => return Err(io::Error::new(io::ErrorKind::Interrupted, "journal cancelled")),
        }
        if !dirty {
            due = Instant::now() + interval;
            dirty = true;
        }
    }
}

/// Production seed admission. The reader owns an immutable, cheap snapshot and
/// starts at byte zero. One service handles every document; do not spawn one
/// JournalWorker per tab. Records waiting for a seed spill to its `.pending` file.
pub struct JournalSeed {
    pub path: PathBuf,
    pub base: JournalBase,
    pub source: Box<dyn Read + Send>,
}
/// Explicit encoded seed admission; physical and logical lengths never travel
/// inside the caller's opaque codec. Uses the same single service worker/queue.
pub struct EncodedJournalSeed {
    pub path: PathBuf,
    pub base: EncodedJournalBase,
    pub source: Box<dyn Read + Send>,
}
impl std::fmt::Debug for EncodedJournalSeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncodedJournalSeed").field("path", &self.path).field("base", &self.base).finish_non_exhaustive()
    }
}
impl std::fmt::Debug for JournalSeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JournalSeed").field("path", &self.path).field("base", &self.base).finish_non_exhaustive()
    }
}

/// Prepared on a recovery worker before admission to the shared writer. Holding
/// this value pins writer ownership; no second GB scan occurs on the service.
pub struct RecoveredJournal {
    path: PathBuf,
    file: File,
    ownership: File,
    base: JournalBase,
    summary: RecoverySummary,
}
impl std::fmt::Debug for RecoveredJournal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecoveredJournal")
            .field("path", &self.path)
            .field("summary", &self.summary)
            .finish_non_exhaustive()
    }
}
impl RecoveredJournal {
    pub fn open(
        path: PathBuf,
        expected: DocumentId,
        cancel: &AtomicBool,
        mut emit: impl FnMut(RecoveryEvent<'_>) -> io::Result<()>,
    ) -> io::Result<Self> {
        let ownership = journal_lock(&path)?;
        let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
        let pending = merge_pending_for_recovery(&path, &mut file, expected, cancel)?;
        let mut reader = JournalReader { limit: file.metadata()?.len(), file: file.try_clone()? };
        let mut base = None;
        let summary = reader.recover(cancel, |event| {
            if let RecoveryEvent::Base(b) | RecoveryEvent::EncodedBase { base: b, .. } = &event {
                if b.document != expected {
                    return Err(invalid("journal document identity mismatch"));
                }
                base = Some((*b).clone());
            }
            emit(event)
        })?;
        cancelled(cancel)?;
        if summary.tail == RecoveryTail::Interrupted {
            file.set_len(summary.committed_end)?;
            file.sync_all()?;
        }
        if pending {
            std::fs::remove_file(pending_path(&path))?;
            if let Some(parent) = path.parent() {
                sync_parent(parent)?;
            }
        }
        let mut file = file;
        file.seek(SeekFrom::Start(summary.committed_end))?;
        Ok(Self { path, file, ownership, base: base.ok_or_else(|| invalid("missing recovery base"))?, summary })
    }
    pub fn summary(&self) -> RecoverySummary {
        self.summary
    }
}

enum ServiceRequest {
    Create {
        seed: JournalSeed,
        logical_len: Option<u64>,
        token: u64,
        ready: SyncSender<io::Result<u64>>,
        done: SyncSender<io::Result<u64>>,
    },
    Resume {
        recovered: RecoveredJournal,
        token: u64,
        ready: SyncSender<io::Result<u64>>,
        done: SyncSender<io::Result<u64>>,
    },
    Command(u64, JournalCommand),
    Wake,
}
pub struct JournalSession {
    tx: Option<SyncSender<ServiceRequest>>,
    token: u64,
    closing: Arc<AtomicBool>,
    pub ready: Receiver<io::Result<u64>>,
    pub finished: Receiver<io::Result<u64>>,
}
impl JournalSession {
    /// Each command is admitted completely or returned intact. On Full retain and
    /// retry this exact command before later commands for this document; do not
    /// restart the batch or skip a payload chunk. Other documents may proceed.
    pub fn try_submit(&self, command: JournalCommand) -> Result<(), TrySendError<JournalCommand>> {
        if self.closing.load(Ordering::Acquire)
            || matches!(&command,JournalCommand::Bytes(b) if b.is_empty() || b.len()>JOURNAL_CHUNK)
        {
            return Err(TrySendError::Disconnected(command));
        }
        let Some(tx) = &self.tx else {
            return Err(TrySendError::Disconnected(command));
        };
        tx.try_send(ServiceRequest::Command(self.token, command)).map_err(|e| match e {
            TrySendError::Full(ServiceRequest::Command(_, c)) => TrySendError::Full(c),
            TrySendError::Disconnected(ServiceRequest::Command(_, c)) => TrySendError::Disconnected(c),
            _ => unreachable!(),
        })
    }
    pub fn try_flush(&self) -> Result<Receiver<io::Result<u64>>, TrySendError<JournalCommand>> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.try_submit(JournalCommand::Flush(tx))?;
        Ok(rx)
    }
    /// Retain/retry on Full. Completion waits for a pending seed and drains its
    /// spooled edits; no dirty inactive document is silently dropped.
    pub fn try_finish(&mut self) -> Result<(), TrySendError<JournalCommand>> {
        self.try_submit(JournalCommand::Finish)?;
        self.tx.take();
        Ok(())
    }
    pub fn try_cancel(&mut self) -> Result<(), TrySendError<JournalCommand>> {
        self.try_submit(JournalCommand::Cancel)?;
        self.tx.take();
        Ok(())
    }
}

/// One fixed I/O worker, one global queue (16 x <=64KiB payloads), at most
/// MAX_TABS live journals. Seeds and pending-record merges advance round-robin.
/// Drop cancels; call finish and poll finished for a graceful hot exit.
pub struct JournalService {
    tx: Option<SyncSender<ServiceRequest>>,
    closing: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    next: std::sync::atomic::AtomicU64,
    pub finished: Receiver<io::Result<()>>,
}
impl JournalService {
    pub fn spawn(interval: Duration) -> io::Result<Self> {
        if interval.is_zero() {
            return Err(invalid("zero journal interval"));
        }
        let (tx, rx) = mpsc::sync_channel(JOURNAL_QUEUE);
        let (done, finished) = mpsc::sync_channel(1);
        let closing = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_closing = closing.clone();
        let worker_stop = stop.clone();
        thread::Builder::new().name("kwil-journals".into()).spawn(move || {
            let result = service_loop(rx, interval, &worker_closing, &worker_stop);
            let _ = done.try_send(result);
        })?;
        Ok(Self { tx: Some(tx), closing, cancel: stop, next: std::sync::atomic::AtomicU64::new(1), finished })
    }
    pub fn try_create(&self, seed: JournalSeed) -> Result<JournalSession, TrySendError<Box<JournalSeed>>> {
        self.try_create_base(seed, None)
    }
    pub fn try_create_encoded(
        &self,
        seed: EncodedJournalSeed,
    ) -> Result<JournalSession, TrySendError<Box<EncodedJournalSeed>>> {
        let logical_len = seed.base.logical_len;
        let raw = JournalSeed { path: seed.path, base: seed.base.base, source: seed.source };
        self.try_create_base(raw, Some(logical_len)).map_err(|e| {
            let wrap = |seed: Box<JournalSeed>| {
                Box::new(EncodedJournalSeed {
                    path: seed.path,
                    base: EncodedJournalBase { base: seed.base, logical_len },
                    source: seed.source,
                })
            };
            match e {
                TrySendError::Full(seed) => TrySendError::Full(wrap(seed)),
                TrySendError::Disconnected(seed) => TrySendError::Disconnected(wrap(seed)),
            }
        })
    }
    fn try_create_base(
        &self,
        seed: JournalSeed,
        logical_len: Option<u64>,
    ) -> Result<JournalSession, TrySendError<Box<JournalSeed>>> {
        if self.closing.load(Ordering::Acquire)
            || seed.base.codec.len() > 128
            || seed.base.document.0 == 0
            || seed.base.original_path.as_deref().is_some_and(|p| crate::session::validate_path(p).is_err())
        {
            return Err(TrySendError::Disconnected(Box::new(seed)));
        }
        let token = self.next.fetch_add(1, Ordering::Relaxed);
        let (ready_tx, ready) = mpsc::sync_channel(1);
        let (done, finished) = mpsc::sync_channel(1);
        let request = ServiceRequest::Create { seed, logical_len, token, ready: ready_tx, done };
        self.tx.as_ref().unwrap().try_send(request).map_err(|e| match e {
            TrySendError::Full(ServiceRequest::Create { seed, .. }) => TrySendError::Full(Box::new(seed)),
            TrySendError::Disconnected(ServiceRequest::Create { seed, .. }) => {
                TrySendError::Disconnected(Box::new(seed))
            }
            _ => unreachable!(),
        })?;
        Ok(JournalSession { tx: self.tx.clone(), token, closing: self.closing.clone(), ready, finished })
    }
    pub fn try_resume(
        &self,
        recovered: RecoveredJournal,
    ) -> Result<JournalSession, TrySendError<Box<RecoveredJournal>>> {
        if self.closing.load(Ordering::Acquire) {
            return Err(TrySendError::Disconnected(Box::new(recovered)));
        }
        let token = self.next.fetch_add(1, Ordering::Relaxed);
        let (ready_tx, ready) = mpsc::sync_channel(1);
        let (done, finished) = mpsc::sync_channel(1);
        self.tx
            .as_ref()
            .unwrap()
            .try_send(ServiceRequest::Resume { recovered, token, ready: ready_tx, done })
            .map_err(|e| match e {
                TrySendError::Full(ServiceRequest::Resume { recovered, .. }) => TrySendError::Full(Box::new(recovered)),
                TrySendError::Disconnected(ServiceRequest::Resume { recovered, .. }) => {
                    TrySendError::Disconnected(Box::new(recovered))
                }
                _ => unreachable!(),
            })?;
        Ok(JournalSession { tx: self.tx.clone(), token, closing: self.closing.clone(), ready, finished })
    }
    pub fn finish(&mut self) {
        self.closing.store(true, Ordering::Release);
        if let Some(tx) = self.tx.take() {
            let _ = tx.try_send(ServiceRequest::Wake);
        }
    }
    pub fn cancel(&mut self) {
        self.cancel.store(true, Ordering::Release);
        self.finish();
    }
}
impl Drop for JournalService {
    fn drop(&mut self) {
        self.cancel();
    }
}

enum SeedPhase {
    Seed { reader: Box<dyn Read + Send>, left: u64, exact: bool },
    Merge { offset: u64 },
    Ready,
}
struct ServiceEntry {
    path: PathBuf,
    file: File,
    _ownership: File,
    spool: Option<File>,
    base: JournalBase,
    state: State,
    phase: SeedPhase,
    ready: Option<SyncSender<io::Result<u64>>>,
    done: SyncSender<io::Result<u64>>,
    finishing: bool,
    dirty: bool,
    due: Instant,
}
fn pending_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".pending");
    PathBuf::from(name)
}
impl ServiceEntry {
    fn seed(
        seed: JournalSeed,
        logical_len: Option<u64>,
        ready: SyncSender<io::Result<u64>>,
        done: SyncSender<io::Result<u64>>,
        interval: Duration,
    ) -> io::Result<Self> {
        let ownership = journal_lock(&seed.path)?;
        let mut file = private_new(&seed.path)?;
        write_frame(&mut file, &base_frame(&seed.base, logical_len)?)?;
        let mut options = OpenOptions::new();
        options.read(true).append(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let spool = options.open(pending_path(&seed.path))?;
        Ok(Self {
            path: seed.path,
            file,
            _ownership: ownership,
            spool: Some(spool),
            state: State::new(seed.base.revision, logical_len.unwrap_or(seed.base.byte_len)),
            phase: SeedPhase::Seed { reader: seed.source, left: seed.base.byte_len, exact: logical_len.is_some() },
            base: seed.base,
            ready: Some(ready),
            done,
            finishing: false,
            dirty: true,
            due: Instant::now() + interval,
        })
    }
    fn resume(
        r: RecoveredJournal,
        ready: SyncSender<io::Result<u64>>,
        done: SyncSender<io::Result<u64>>,
        interval: Duration,
    ) -> Self {
        let _ = ready.try_send(Ok(r.summary.revision));
        Self {
            path: r.path,
            file: r.file,
            _ownership: r.ownership,
            spool: None,
            base: r.base,
            state: State::new(r.summary.revision, r.summary.byte_len),
            phase: SeedPhase::Ready,
            ready: None,
            done,
            finishing: false,
            dirty: false,
            due: Instant::now() + interval,
        }
    }
    fn command(&mut self, command: JournalCommand, interval: Duration) -> io::Result<()> {
        if self.finishing {
            return Err(invalid("command after journal finish"));
        }
        let file = if matches!(self.phase, SeedPhase::Ready) { &mut self.file } else { self.spool.as_mut().unwrap() };
        match command {
            JournalCommand::Begin(e) => {
                self.state.begin(e)?;
                write_frame(file, &edit_bytes(e))?;
            }
            JournalCommand::Bytes(bytes) => {
                if bytes.is_empty() || bytes.len() > JOURNAL_CHUNK {
                    return Err(invalid("edit chunk limit"));
                }
                self.state.bytes(bytes.len())?;
                let mut b = Vec::with_capacity(bytes.len() + 1);
                b.push(5);
                b.extend(bytes);
                write_frame(file, &b)?;
            }
            JournalCommand::Commit => {
                self.state.commit()?;
                write_frame(file, &[6])?;
            }
            JournalCommand::BeginBatch(b) => {
                self.state.begin_batch(b)?;
                write_frame(file, &batch_bytes(b))?;
            }
            JournalCommand::BeginReplacement(r) => {
                self.state.begin_replacement(r)?;
                write_frame(file, &replacement_bytes(r))?;
            }
            JournalCommand::BeginEncodedReplacement { replacement, payload_len } => {
                self.state.begin_payload(replacement, payload_len)?;
                write_frame(file, &encoded_replacement_bytes(replacement, payload_len))?;
            }
            JournalCommand::EndReplacement => {
                self.state.end_replacement()?;
                write_frame(file, &[9])?;
            }
            JournalCommand::CommitBatch => {
                self.state.commit_batch()?;
                write_frame(file, &[10])?;
            }
            JournalCommand::Flush(reply) => {
                if !matches!(self.phase, SeedPhase::Ready) {
                    let _ = reply.try_send(Err(io::Error::new(io::ErrorKind::WouldBlock, "base still seeding")));
                    return Ok(());
                }
                let result = self.file.sync_all();
                let _ = reply.try_send(result.as_ref().map(|_| self.state.revision).map_err(copy_error));
                result?;
                self.dirty = false;
                return Ok(());
            }
            JournalCommand::Finish => {
                self.finishing = true;
                return Ok(());
            }
            JournalCommand::Cancel => return Err(io::Error::new(io::ErrorKind::Interrupted, "journal cancelled")),
        }
        if !self.dirty {
            self.due = Instant::now() + interval;
            self.dirty = true;
        }
        Ok(())
    }
    fn advance(&mut self) -> io::Result<()> {
        match &mut self.phase {
            SeedPhase::Seed { reader, left, exact } => {
                if *left != 0 {
                    let n = (*left).min(JOURNAL_CHUNK as u64) as usize;
                    let mut b = vec![0; n + 1];
                    b[0] = 2;
                    reader.read_exact(&mut b[1..])?;
                    write_frame(&mut self.file, &b)?;
                    *left -= n as u64;
                    return Ok(());
                }
                if *exact {
                    require_eof(reader)?;
                }
                write_frame(&mut self.file, &[3])?;
                self.file.sync_all()?;
                if let Some(p) = self.path.parent() {
                    sync_parent(p)?;
                }
                self.phase = SeedPhase::Merge { offset: 0 };
            }
            SeedPhase::Merge { offset } => {
                let spool = self.spool.as_ref().unwrap();
                let len = spool.metadata()?.len();
                if *offset < len {
                    let n = (len - *offset).min(JOURNAL_CHUNK as u64) as usize;
                    let mut bytes = vec![0; n];
                    read_positioned(spool, *offset, &mut bytes)?;
                    self.file.write_all(&bytes)?;
                    *offset += n as u64;
                    return Ok(());
                }
                self.file.sync_all()?;
                self.spool.take();
                std::fs::remove_file(pending_path(&self.path))?;
                if let Some(parent) = self.path.parent() {
                    sync_parent(parent)?;
                }
                self.phase = SeedPhase::Ready;
                self.dirty = false;
                if let Some(ready) = self.ready.take() {
                    let _ = ready.try_send(Ok(self.state.revision));
                }
            }
            SeedPhase::Ready => (),
        }
        Ok(())
    }
    fn complete(mut self, result: io::Result<u64>) {
        // Never remove pending data on failure/cancel. It is evidence of an
        // incomplete seed, and readiness must remain an error.
        if let Some(ready) = self.ready.take() {
            let error = result.as_ref().err().map(copy_error).unwrap_or_else(|| invalid("incomplete journal base"));
            let _ = ready.try_send(Err(error));
        }
        let done = self.done.clone();
        drop(self);
        let _ = done.try_send(result);
    }
}
fn copy_error(e: &io::Error) -> io::Error {
    io::Error::new(e.kind(), e.to_string())
}
fn read_positioned(file: &File, mut offset: u64, mut bytes: &mut [u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        #[cfg(unix)]
        let n = {
            use std::os::unix::fs::FileExt;
            file.read_at(bytes, offset)?
        };
        #[cfg(windows)]
        let n = {
            use std::os::windows::fs::FileExt;
            file.seek_read(bytes, offset)?
        };
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "pending journal truncated"));
        }
        offset += n as u64;
        bytes = &mut bytes[n..];
    }
    Ok(())
}
// A crash may leave a complete base and only part of the pending stream copied.
// Check the common prefix, append the missing suffix, and preserve any newer
// records beyond an old pending file resurrected by directory crash recovery.
fn merge_pending_for_recovery(
    path: &Path,
    file: &mut File,
    expected: DocumentId,
    cancel: &AtomicBool,
) -> io::Result<bool> {
    let pending = match File::open(pending_path(path)) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let limit = file.metadata()?.len();
    file.seek(SeekFrom::Start(0))?;
    let (base, _) = decode_base(&read_frame(file, limit)?.ok_or_else(|| invalid("incomplete journal base"))?)?;
    if base.document != expected {
        return Err(invalid("journal document identity mismatch"));
    }
    let mut len = 0u64;
    loop {
        cancelled(cancel)?;
        let b = read_frame(file, limit)?.ok_or_else(|| invalid("incomplete journal base"))?;
        match b[0] {
            2 if b.len() > 1 => {
                len = len.checked_add((b.len() - 1) as u64).ok_or_else(|| invalid("base overflow"))?;
                if len > base.byte_len {
                    return Err(invalid("base length"));
                }
            }
            3 if b.len() == 1 && len == base.byte_len => break,
            _ => return Err(invalid("base record order")),
        }
    }
    let base_end = file.stream_position()?;
    let pending_len = pending.metadata()?.len();
    let copied = limit - base_end;
    let common = copied.min(pending_len);
    let mut offset = 0;
    let mut a = vec![0; JOURNAL_CHUNK];
    let mut b = vec![0; JOURNAL_CHUNK];
    while offset < common {
        cancelled(cancel)?;
        let n = (common - offset).min(JOURNAL_CHUNK as u64) as usize;
        read_positioned(file, base_end + offset, &mut a[..n])?;
        read_positioned(&pending, offset, &mut b[..n])?;
        if a[..n] != b[..n] {
            return Err(invalid("pending journal prefix mismatch"));
        }
        offset += n as u64;
    }
    file.seek(SeekFrom::End(0))?;
    offset = copied;
    while offset < pending_len {
        cancelled(cancel)?;
        let n = (pending_len - offset).min(JOURNAL_CHUNK as u64) as usize;
        read_positioned(&pending, offset, &mut a[..n])?;
        file.write_all(&a[..n])?;
        offset += n as u64;
    }
    file.sync_all()?;
    Ok(true)
}
fn service_loop(
    rx: Receiver<ServiceRequest>,
    interval: Duration,
    closing: &AtomicBool,
    cancel: &AtomicBool,
) -> io::Result<()> {
    use std::collections::{HashMap, VecDeque};
    let mut entries = HashMap::<u64, ServiceEntry>::new();
    let mut order = VecDeque::new();
    let mut draining = false;
    loop {
        if cancel.load(Ordering::Acquire) {
            for (_, e) in entries.drain() {
                let result = if matches!(e.phase, SeedPhase::Ready) {
                    e.file.sync_all().map(|_| e.state.revision)
                } else {
                    Err(io::Error::new(io::ErrorKind::Interrupted, "journal seed cancelled"))
                };
                e.complete(result);
            }
            return Ok(());
        }
        let busy = entries.values().any(|e| !matches!(e.phase, SeedPhase::Ready));
        let request = if busy || closing.load(Ordering::Acquire) {
            match rx.try_recv() {
                Ok(r) => Some(r),
                Err(mpsc::TryRecvError::Empty) => {
                    if closing.load(Ordering::Acquire) {
                        draining = true;
                    }
                    None
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    draining = true;
                    None
                }
            }
        } else if let Some(due) = entries.values().filter(|e| e.dirty).map(|e| e.due).min() {
            match rx.recv_timeout(due.saturating_duration_since(Instant::now())) {
                Ok(r) => Some(r),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    draining = true;
                    None
                }
            }
        } else {
            match rx.recv() {
                Ok(r) => Some(r),
                Err(_) => {
                    draining = true;
                    None
                }
            }
        };
        if let Some(request) = request {
            match request {
                ServiceRequest::Create { seed, logical_len, token, ready, done } => {
                    if entries.len() >= crate::session::MAX_TABS
                        || entries.values().any(|e| e.base.document == seed.base.document)
                    {
                        let _ = ready.try_send(Err(invalid("journal document limit/duplicate")));
                        let _ = done.try_send(Err(invalid("journal document limit/duplicate")));
                    } else {
                        match ServiceEntry::seed(seed, logical_len, ready.clone(), done.clone(), interval) {
                            Ok(entry) => {
                                entries.insert(token, entry);
                                order.push_back(token);
                            }
                            Err(e) => {
                                let _ = ready.try_send(Err(copy_error(&e)));
                                let _ = done.try_send(Err(e));
                            }
                        }
                    }
                }
                ServiceRequest::Resume { recovered, token, ready, done } => {
                    if entries.len() >= crate::session::MAX_TABS
                        || entries.values().any(|e| e.base.document == recovered.base.document)
                    {
                        let _ = ready.try_send(Err(invalid("journal document limit/duplicate")));
                        let _ = done.try_send(Err(invalid("journal document limit/duplicate")));
                    } else {
                        entries.insert(token, ServiceEntry::resume(recovered, ready, done, interval));
                        order.push_back(token);
                    }
                }
                ServiceRequest::Command(token, command) => {
                    if let Some(entry) = entries.get_mut(&token) {
                        if let Err(e) = entry.command(command, interval) {
                            entries.remove(&token).unwrap().complete(Err(e));
                        }
                    } else if let JournalCommand::Flush(reply) = command {
                        let _ = reply.try_send(Err(invalid("closed journal")));
                    }
                }
                ServiceRequest::Wake => (),
            }
        }
        if draining {
            for entry in entries.values_mut() {
                entry.finishing = true;
            }
        }
        if let Some(token) = order.pop_front()
            && let Some(entry) = entries.get_mut(&token)
        {
            match entry.advance() {
                Ok(()) => order.push_back(token),
                Err(e) => entries.remove(&token).unwrap().complete(Err(e)),
            }
        }
        let mut remove = Vec::new();
        for (&token, entry) in &mut entries {
            if entry.finishing && matches!(entry.phase, SeedPhase::Ready) {
                let result = entry.file.sync_all().map(|_| entry.state.revision);
                remove.push((token, result));
            } else if entry.dirty && Instant::now() >= entry.due {
                match entry.file.sync_all().and_then(|_| entry.spool.as_ref().map_or(Ok(()), File::sync_all)) {
                    Ok(()) => {
                        entry.dirty = false;
                        entry.due = Instant::now() + interval;
                    }
                    Err(e) => remove.push((token, Err(e))),
                }
            }
        }
        for (token, result) in remove {
            if let Some(e) = entries.remove(&token) {
                e.complete(result);
            }
        }
        if draining && entries.is_empty() {
            return Ok(());
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryTail {
    Complete,
    Interrupted,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecoverySummary {
    pub revision: u64,
    /// Current logical model length; encoded payload sizes need not match it.
    pub byte_len: u64,
    pub committed_end: u64,
    pub tail: RecoveryTail,
}
/// Callbacks run on the recovery worker. Each edit is validated completely before
/// BeginEdit is emitted. A model should stage callbacks and publish at CommitEdit.
/// For a batch, no batch event (including BeginBatch) is emitted until the entire
/// outer commit is verified. Apply replacement coordinates sequentially; publish
/// the model revision only at CommitBatch. Bytes are still bounded to 64 KiB.
pub enum RecoveryEvent<'a> {
    Base(&'a JournalBase),
    /// Distinct from raw Base: the caller must explicitly restore the codec's
    /// encoded representation. BaseBytes still streams physical payload bytes.
    EncodedBase {
        base: &'a JournalBase,
        logical_len: u64,
    },
    BaseBytes(&'a [u8]),
    BaseComplete,
    BeginEdit(JournalEdit),
    EditBytes(&'a [u8]),
    CommitEdit,
    BeginBatch(JournalBatch),
    BeginReplacement(JournalReplacement),
    BeginEncodedReplacement {
        replacement: JournalReplacement,
        payload_len: u64,
    },
    EndReplacement,
    CommitBatch,
}
pub struct JournalReader {
    file: File,
    limit: u64,
}
impl JournalReader {
    /// Worker-only. Captures the length so an active writer cannot extend this recovery.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let limit = file.metadata()?.len();
        Ok(Self { file, limit })
    }
    pub fn recover(
        &mut self,
        cancel: &AtomicBool,
        mut emit: impl FnMut(RecoveryEvent<'_>) -> io::Result<()>,
    ) -> io::Result<RecoverySummary> {
        self.file.seek(SeekFrom::Start(0))?;
        let Some(header) = read_frame(&mut self.file, self.limit)? else {
            return Err(invalid("incomplete journal base"));
        };
        let (base, logical_len) = decode_base(&header)?;
        let base_start = self.file.stream_position()?;
        let mut base_len = 0u64;
        loop {
            cancelled(cancel)?;
            let Some(b) = read_frame(&mut self.file, self.limit)? else {
                return Err(invalid("incomplete journal base"));
            };
            match b[0] {
                2 if b.len() > 1 => {
                    base_len = base_len.checked_add((b.len() - 1) as u64).ok_or_else(|| invalid("base overflow"))?;
                    if base_len > base.byte_len {
                        return Err(invalid("base length"));
                    }
                }
                3 if b.len() == 1 && base_len == base.byte_len => break,
                _ => return Err(invalid("base record order")),
            }
        }
        let base_end = self.file.stream_position()?;
        match logical_len {
            Some(logical_len) => emit(RecoveryEvent::EncodedBase { base: &base, logical_len })?,
            None => emit(RecoveryEvent::Base(&base))?,
        }
        replay(&mut self.file, base_start, base_end, cancel, |b| {
            if b[0] == 2 {
                emit(RecoveryEvent::BaseBytes(&b[1..]))?;
            }
            Ok(())
        })?;
        emit(RecoveryEvent::BaseComplete)?;
        let mut state = State::new(base.revision, logical_len.unwrap_or(base.byte_len));
        let mut committed_end = base_end;
        let mut edit_start = base_end;
        loop {
            cancelled(cancel)?;
            let position = self.file.stream_position()?;
            if position == self.limit {
                return Ok(RecoverySummary {
                    revision: state.revision,
                    byte_len: state.len,
                    committed_end,
                    tail: if state.pending.is_some() || state.batch.is_some() {
                        RecoveryTail::Interrupted
                    } else {
                        RecoveryTail::Complete
                    },
                });
            }
            let Some(b) = read_frame(&mut self.file, self.limit)? else {
                return Ok(RecoverySummary {
                    revision: state.revision,
                    byte_len: state.len,
                    committed_end,
                    tail: RecoveryTail::Interrupted,
                });
            };
            match b[0] {
                4 => {
                    let edit = decode_edit(&b)?;
                    state.begin(edit)?;
                    edit_start = self.file.stream_position()?;
                }
                5 if b.len() > 1 => state.bytes(b.len() - 1)?,
                6 if b.len() == 1 => {
                    let edit = state.commit()?;
                    let end = self.file.stream_position()?;
                    emit(RecoveryEvent::BeginEdit(edit))?;
                    replay(&mut self.file, edit_start, end, cancel, |b| {
                        if b[0] == 5 {
                            emit(RecoveryEvent::EditBytes(&b[1..]))?;
                        }
                        Ok(())
                    })?;
                    emit(RecoveryEvent::CommitEdit)?;
                    committed_end = end;
                }
                7 => {
                    state.begin_batch(decode_batch(&b)?)?;
                    edit_start = self.file.stream_position()?;
                }
                8 => state.begin_replacement(decode_replacement(&b)?)?,
                ENCODED_REPLACEMENT => {
                    let (replacement, payload_len) = decode_encoded_replacement(&b)?;
                    state.begin_payload(replacement, payload_len)?;
                }
                9 if b.len() == 1 => state.end_replacement()?,
                10 if b.len() == 1 => {
                    let batch = state.commit_batch()?;
                    let end = self.file.stream_position()?;
                    emit(RecoveryEvent::BeginBatch(batch))?;
                    replay(&mut self.file, edit_start, end, cancel, |b| {
                        match b[0] {
                            8 => emit(RecoveryEvent::BeginReplacement(decode_replacement(b)?))?,
                            ENCODED_REPLACEMENT => {
                                let (replacement, payload_len) = decode_encoded_replacement(b)?;
                                emit(RecoveryEvent::BeginEncodedReplacement { replacement, payload_len })?;
                            }
                            5 => emit(RecoveryEvent::EditBytes(&b[1..]))?,
                            9 => emit(RecoveryEvent::EndReplacement)?,
                            10 => (),
                            _ => return Err(invalid("batch replay order")),
                        }
                        Ok(())
                    })?;
                    emit(RecoveryEvent::CommitBatch)?;
                    committed_end = end;
                }
                _ => return Err(invalid("journal record order")),
            }
        }
    }
}
fn read_frame(f: &mut File, limit: u64) -> io::Result<Option<Vec<u8>>> {
    let pos = f.stream_position()?;
    if limit.saturating_sub(pos) < 12 {
        return Ok(None);
    }
    let mut h = [0u8; 12];
    f.read_exact(&mut h)?;
    if &h[..4] != MAGIC {
        return Err(invalid("journal magic"));
    }
    let n = u32::from_le_bytes(h[4..8].try_into().unwrap()) as usize;
    if n == 0 || n > MAX_RECORD {
        return Err(invalid("journal frame size"));
    }
    if limit.saturating_sub(pos + 12) < n as u64 {
        return Ok(None);
    }
    let mut b = vec![0; n];
    f.read_exact(&mut b)?;
    if checksum(&b) != u32::from_le_bytes(h[8..12].try_into().unwrap()) {
        return Err(invalid("journal checksum"));
    }
    Ok(Some(b))
}
fn replay(
    f: &mut File,
    start: u64,
    end: u64,
    cancel: &AtomicBool,
    mut emit: impl FnMut(&[u8]) -> io::Result<()>,
) -> io::Result<()> {
    f.seek(SeekFrom::Start(start))?;
    while f.stream_position()? < end {
        cancelled(cancel)?;
        let b = read_frame(f, end)?.ok_or_else(|| invalid("changed journal snapshot"))?;
        emit(&b)?;
    }
    Ok(())
}

#[cfg(test)]
mod logical_tests;

#[cfg(test)]
mod transport_promotion_tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "kwil-transport-promotion-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn paths(&self) -> (PathBuf, PathBuf) {
            (self.0.join("7.kwj"), self.0.join("7.kwj.next"))
        }
        fn seed(&self, path: &Path, bytes: &[u8]) {
            let mut file = private_new(path).unwrap();
            let base = JournalBase {
                document: DocumentId(7),
                revision: 0,
                original_path: None,
                original_identity: [0; 4],
                codec: b"raw-v1".to_vec(),
                byte_len: bytes.len() as u64,
            };
            seed(&mut file, &base, None, Some(Box::new(io::Cursor::new(bytes.to_vec()))), &AtomicBool::new(false))
                .unwrap();
            file.sync_all().unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            if !std::thread::panicking() {
                std::fs::remove_dir_all(&self.0).unwrap();
            }
        }
    }

    #[test]
    fn transport_promotion_retains_old_authority_before_replace_and_selects_new_after() {
        let fixture = Fixture::new();
        let (canonical, staged) = fixture.paths();
        fixture.seed(&canonical, b"old recovery");
        fixture.seed(&staged, b"new recovery");
        let old = std::fs::read(&canonical).unwrap();
        let new = std::fs::read(&staged).unwrap();
        let error = promote_finished_seed_with(
            &staged,
            &canonical,
            DocumentId(7),
            |_, _| Err(io::Error::other("injected pre-replace refusal")),
            |_| panic!("no sync after refusal"),
        );
        assert!(error.is_err());
        assert_eq!(std::fs::read(&canonical).unwrap(), old);
        assert_eq!(std::fs::read(&staged).unwrap(), new);
        promote_finished_seed(&staged, &canonical, DocumentId(7)).unwrap();
        assert_eq!(std::fs::read(&canonical).unwrap(), new);
        assert!(!staged.exists());
    }

    #[test]
    fn transport_promotion_refuses_live_writers_pending_data_and_wrong_siblings() {
        let fixture = Fixture::new();
        let (canonical, staged) = fixture.paths();
        fixture.seed(&canonical, b"old");
        fixture.seed(&staged, b"new");
        let old = std::fs::read(&canonical).unwrap();
        for path in [&canonical, &staged] {
            let lock = journal_lock(path).unwrap();
            assert!(promote_finished_seed(&staged, &canonical, DocumentId(7)).is_err());
            drop(lock);
            assert_eq!(std::fs::read(&canonical).unwrap(), old);
            assert!(staged.exists());
        }
        assert!(promote_finished_seed(&fixture.0.join("other"), &canonical, DocumentId(7)).is_err());
        assert!(promote_finished_seed(&staged, &canonical, DocumentId(8)).is_err());
        let pending = pending_path(&staged);
        drop(private_new(&pending).unwrap());
        assert!(promote_finished_seed(&staged, &canonical, DocumentId(7)).is_err());
        assert!(pending.exists());
        assert!(staged.exists());
        assert_eq!(std::fs::read(canonical).unwrap(), old);
    }

    #[test]
    fn transport_promotion_truncated_or_appended_seed_never_replaces_old_recovery() {
        for corrupt_tail in [true, false] {
            let fixture = Fixture::new();
            let (canonical, staged) = fixture.paths();
            fixture.seed(&canonical, b"old");
            fixture.seed(&staged, b"new");
            let old = std::fs::read(&canonical).unwrap();
            let mut file = OpenOptions::new().write(true).open(&staged).unwrap();
            if corrupt_tail {
                file.set_len(file.metadata().unwrap().len() - 1).unwrap();
            } else {
                file.seek(SeekFrom::End(0)).unwrap();
                write_frame(
                    &mut file,
                    &edit_bytes(JournalEdit { from_revision: 0, to_revision: 1, offset: 0, removed: 0, inserted: 1 }),
                )
                .unwrap();
                write_frame(&mut file, &[5, b'x']).unwrap();
                write_frame(&mut file, &[6]).unwrap();
            }
            drop(file);
            assert!(promote_finished_seed(&staged, &canonical, DocumentId(7)).is_err());
            assert!(staged.exists());
            assert_eq!(std::fs::read(canonical).unwrap(), old);
        }
    }

    #[test]
    fn transport_promotion_reports_post_replace_durability_uncertain_as_committed() {
        let fixture = Fixture::new();
        let (canonical, staged) = fixture.paths();
        fixture.seed(&canonical, b"old");
        fixture.seed(&staged, b"new");
        let new = std::fs::read(&staged).unwrap();
        let error =
            promote_finished_seed_with(&staged, &canonical, DocumentId(7), crate::session::atomic_replace, |_| {
                Err(io::Error::other("injected directory sync failure"))
            })
            .unwrap_err();
        assert!(error.to_string().contains("promotion committed"));
        assert_eq!(std::fs::read(canonical).unwrap(), new);
        assert!(!staged.exists());
    }

    #[cfg(unix)]
    #[test]
    fn transport_promotion_refuses_symlink_and_nonprivate_staged_source() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let fixture = Fixture::new();
        let (canonical, staged) = fixture.paths();
        fixture.seed(&canonical, b"old");
        let target = fixture.0.join("other-seed");
        fixture.seed(&target, b"new");
        symlink(&target, &staged).unwrap();
        assert!(promote_finished_seed(&staged, &canonical, DocumentId(7)).is_err());
        std::fs::remove_file(&staged).unwrap();
        std::fs::rename(target, &staged).unwrap();
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(promote_finished_seed(&staged, &canonical, DocumentId(7)).is_err());
        assert!(staged.exists());
    }
}
