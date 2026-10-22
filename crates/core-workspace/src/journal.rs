//! Append-only raw-byte hot-exit journal. Disk operations belong to this worker.
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
const MAX_RECORD: usize = JOURNAL_CHUNK + 128;
const MAGIC: &[u8; 4] = b"KWJ1";

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
    pub byte_len: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JournalEdit {
    pub from_revision: u64,
    pub to_revision: u64,
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
    /// Receipt means sync_all completed, covering all preceding committed edits.
    Flush(SyncSender<io::Result<u64>>),
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
        Self::start(path, base, Some(source), interval)
    }
    fn start(
        path: PathBuf,
        base: JournalBase,
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
                seed(&mut f, &base, source, &stop)?;
                f.sync_all()?;
                if let Some(parent) = path.parent() {
                    sync_parent(parent)?;
                }
                let _ = ready_tx.try_send(Ok(base.revision));
                run(&mut f, State::new(base.revision, base.byte_len), rx, interval, &stop)
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
                let _ownership = journal_lock(&path)?;
                let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
                let mut reader = JournalReader { limit: file.metadata()?.len(), file: file.try_clone()? };
                let mut document = None;
                let summary = reader.recover(&stop, |e| {
                    if let RecoveryEvent::Base(b) = e {
                        document = Some(b.document);
                    }
                    Ok(())
                })?;
                if document != Some(expected_document) {
                    return Err(invalid("journal document identity mismatch"));
                }
                if summary.tail == RecoveryTail::Interrupted {
                    file.set_len(summary.committed_end)?;
                    file.sync_all()?;
                }
                file.seek(SeekFrom::Start(summary.committed_end))?;
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
fn decode_base(b: &[u8]) -> io::Result<JournalBase> {
    let mut r = Decoder::new(b);
    if r.take(1)? != [1] {
        return Err(invalid("missing journal base"));
    }
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
    Ok(JournalBase { document, revision, byte_len, original_identity, original_path, codec })
}
fn seed(
    f: &mut File,
    base: &JournalBase,
    mut source: Option<Box<dyn Read + Send>>,
    stop: &AtomicBool,
) -> io::Result<()> {
    write_frame(f, &base_bytes(base)?)?;
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
    write_frame(f, &[3])
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
#[derive(Clone, Copy)]
struct State {
    revision: u64,
    len: u64,
    pending: Option<(JournalEdit, u64)>,
}
impl State {
    fn new(revision: u64, len: u64) -> Self {
        Self { revision, len, pending: None }
    }
    fn begin(&mut self, e: JournalEdit) -> io::Result<()> {
        if self.pending.is_some()
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
        }
        if !dirty {
            due = Instant::now() + interval;
            dirty = true;
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
    pub byte_len: u64,
    pub committed_end: u64,
    pub tail: RecoveryTail,
}
/// Callbacks run on the recovery worker. Each edit is validated completely before
/// BeginEdit is emitted. A model should stage callbacks and publish at CommitEdit.
pub enum RecoveryEvent<'a> {
    Base(&'a JournalBase),
    BaseBytes(&'a [u8]),
    BaseComplete,
    BeginEdit(JournalEdit),
    EditBytes(&'a [u8]),
    CommitEdit,
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
        let base = decode_base(&header)?;
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
        emit(RecoveryEvent::Base(&base))?;
        replay(&mut self.file, base_start, base_end, cancel, |b| {
            if b[0] == 2 {
                emit(RecoveryEvent::BaseBytes(&b[1..]))?;
            }
            Ok(())
        })?;
        emit(RecoveryEvent::BaseComplete)?;
        let mut state = State::new(base.revision, base.byte_len);
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
                    tail: if state.pending.is_some() { RecoveryTail::Interrupted } else { RecoveryTail::Complete },
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
