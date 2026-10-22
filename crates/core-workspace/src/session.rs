//! Bounded metadata only. No document contents or snapshots are retained here.
//! All filesystem entry points are worker-only; `SessionWorker::spawn` does no disk I/O.
use std::{
    collections::VecDeque,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, SyncSender, TrySendError},
    thread::{self, JoinHandle},
};

pub const MAX_TABS: usize = 128;
pub const MAX_RECENT: usize = 64;
pub const MAX_CLOSED: usize = 64;
pub const MAX_PATH_BYTES: usize = 32 * 1024;
const MAX_METADATA: usize = 9 * 1024 * 1024;

/// Allocated monotonically within a session, persisted across restarts. Never reuse an ID.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct DocumentId(pub u64);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Tab {
    pub id: DocumentId,
    pub path: Option<PathBuf>,
    pub revision: u64,
    pub saved_revision: u64,
    /// Byte offsets in the original encoding, including BOM.
    pub cursor: u64,
    pub anchor: u64,
    pub first_visible_line: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Session {
    next_id: u64,
    tabs: Vec<Tab>,
    active: Option<DocumentId>,
    recent: VecDeque<PathBuf>,
    closed: VecDeque<Tab>,
}
impl Default for Session {
    fn default() -> Self {
        Self { next_id: 1, tabs: Vec::new(), active: None, recent: VecDeque::new(), closed: VecDeque::new() }
    }
}
impl Session {
    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }
    pub fn active(&self) -> Option<DocumentId> {
        self.active
    }
    pub fn recent(&self) -> &VecDeque<PathBuf> {
        &self.recent
    }
    pub fn closed(&self) -> &VecDeque<Tab> {
        &self.closed
    }
    pub fn tab_mut(&mut self, id: DocumentId) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }
    /// Updates Save As metadata and recents without changing document identity.
    pub fn set_path(&mut self, id: DocumentId, path: PathBuf) -> io::Result<()> {
        validate_path(&path)?;
        if !path.is_absolute() || self.tabs.iter().any(|t| t.id != id && t.path.as_ref() == Some(&path)) {
            return Err(invalid("relative or duplicate tab path"));
        }
        let tab = self.tab_mut(id).ok_or_else(|| invalid("unknown document ID"))?;
        tab.path = Some(path.clone());
        self.touch_recent(path);
        Ok(())
    }
    pub fn activate(&mut self, id: DocumentId) -> bool {
        if self.tabs.iter().any(|t| t.id == id) {
            self.active = Some(id);
            true
        } else {
            false
        }
    }
    pub fn open(&mut self, path: Option<PathBuf>) -> io::Result<DocumentId> {
        if let Some(path) = &path {
            validate_path(path)?;
            if !path.is_absolute() {
                return Err(invalid("session paths must be absolute"));
            }
            if let Some(id) = self.tabs.iter().find(|t| t.path.as_ref() == Some(path)).map(|t| t.id) {
                self.active = Some(id);
                return Ok(id);
            }
        }
        if self.tabs.len() == MAX_TABS {
            return Err(invalid("tab limit reached"));
        }
        let id = DocumentId(self.next_id);
        self.next_id = self.next_id.checked_add(1).ok_or_else(|| invalid("document ID exhausted"))?;
        if let Some(path) = &path {
            self.touch_recent(path.clone());
        }
        self.tabs.push(Tab { id, path, revision: 0, saved_revision: 0, cursor: 0, anchor: 0, first_visible_line: 0 });
        self.active = Some(id);
        Ok(id)
    }
    pub fn close(&mut self, id: DocumentId) -> bool {
        // A bounded history must not silently evict unrecoverable unsaved work.
        // The frontend can reopen/save or explicitly discard that entry first.
        if self.closed.len() == MAX_CLOSED && self.closed.back().is_some_and(|t| t.revision != t.saved_revision) {
            return false;
        }
        let Some(index) = self.tabs.iter().position(|t| t.id == id) else {
            return false;
        };
        let tab = self.tabs.remove(index);
        self.closed.push_front(tab);
        self.closed.truncate(MAX_CLOSED);
        if self.active == Some(id) {
            self.active = self.tabs.get(index.min(self.tabs.len().saturating_sub(1))).map(|t| t.id);
        }
        true
    }
    /// Call only after the model/user explicitly discarded this closed document.
    /// This changes metadata only; journal retention/deletion remains caller-owned.
    pub fn discard_closed(&mut self, id: DocumentId) -> bool {
        let Some(i) = self.closed.iter().position(|t| t.id == id) else {
            return false;
        };
        self.closed.remove(i);
        true
    }
    /// Reuses the stable ID and journal. Closing must not delete that journal.
    pub fn reopen_closed(&mut self) -> io::Result<Option<DocumentId>> {
        if self.tabs.len() == MAX_TABS {
            return Err(invalid("tab limit reached"));
        }
        let Some(tab) = self.closed.pop_front() else {
            return Ok(None);
        };
        if self.tabs.iter().any(|t| t.path.is_some() && t.path == tab.path) {
            self.closed.push_front(tab);
            return Err(invalid("path is already open"));
        }
        let id = tab.id;
        self.tabs.push(tab);
        self.active = Some(id);
        Ok(Some(id))
    }
    fn touch_recent(&mut self, path: PathBuf) {
        self.recent.retain(|p| p != &path);
        self.recent.push_front(path);
        self.recent.truncate(MAX_RECENT);
    }
    pub fn validate(&self) -> io::Result<()> {
        if self.next_id == 0 {
            return Err(invalid("zero next document ID"));
        }
        if self.tabs.len() > MAX_TABS || self.closed.len() > MAX_CLOSED || self.recent.len() > MAX_RECENT {
            return Err(invalid("session limit"));
        }
        let mut ids = std::collections::HashSet::new();
        let mut paths = std::collections::HashSet::new();
        for tab in self.tabs.iter().chain(self.closed.iter()) {
            if tab.id.0 == 0 || tab.id.0 >= self.next_id || !ids.insert(tab.id) {
                return Err(invalid("invalid document ID"));
            }
            if let Some(path) = &tab.path {
                validate_path(path)?;
                if !path.is_absolute() {
                    return Err(invalid("relative session path"));
                }
            }
        }
        for tab in &self.tabs {
            if let Some(path) = &tab.path
                && !paths.insert(path)
            {
                return Err(invalid("duplicate tab path"));
            }
        }
        for path in &self.recent {
            validate_path(path)?;
            if !path.is_absolute() {
                return Err(invalid("relative recent path"));
            }
        }
        if self.active.is_some_and(|id| !self.tabs.iter().any(|t| t.id == id)) {
            return Err(invalid("invalid active tab"));
        }
        Ok(())
    }
    fn encode(&self) -> io::Result<Vec<u8>> {
        self.validate()?;
        let mut b = Vec::new();
        b.extend_from_slice(b"KWS1");
        put64(&mut b, self.next_id);
        put64(&mut b, self.active.map_or(0, |id| id.0));
        let closed: Vec<_> = self.closed.iter().collect();
        for group in [self.tabs.iter().collect::<Vec<_>>(), closed] {
            put32(&mut b, group.len() as u32);
            for t in group {
                put64(&mut b, t.id.0);
                for v in [t.revision, t.saved_revision, t.cursor, t.anchor, t.first_visible_line] {
                    put64(&mut b, v);
                }
                put_path(&mut b, t.path.as_deref())?;
            }
        }
        put32(&mut b, self.recent.len() as u32);
        for p in &self.recent {
            put_path(&mut b, Some(p))?;
        }
        let checksum = checksum(&b);
        put32(&mut b, checksum);
        Ok(b)
    }
    fn decode(b: &[u8]) -> io::Result<Self> {
        if b.len() < 8 || b.len() > MAX_METADATA {
            return Err(invalid("metadata size"));
        }
        let (body, tail) = b.split_at(b.len() - 4);
        if checksum(body) != u32::from_le_bytes(tail.try_into().unwrap()) {
            return Err(invalid("metadata checksum"));
        }
        let mut r = Decoder::new(body);
        if r.take(4)? != b"KWS1" {
            return Err(invalid("metadata version"));
        }
        let next_id = r.u64()?;
        let active = match r.u64()? {
            0 => None,
            id => Some(DocumentId(id)),
        };
        let mut groups = Vec::new();
        for cap in [MAX_TABS, MAX_CLOSED] {
            let n = r.count(cap)?;
            let mut group = Vec::with_capacity(n);
            for _ in 0..n {
                group.push(Tab {
                    id: DocumentId(r.u64()?),
                    revision: r.u64()?,
                    saved_revision: r.u64()?,
                    cursor: r.u64()?,
                    anchor: r.u64()?,
                    first_visible_line: r.u64()?,
                    path: r.path()?,
                });
            }
            groups.push(group);
        }
        let n = r.count(MAX_RECENT)?;
        let mut recent = VecDeque::new();
        for _ in 0..n {
            recent.push_back(r.path()?.ok_or_else(|| invalid("missing recent path"))?);
        }
        r.end()?;
        let closed = groups.pop().unwrap().into();
        let tabs = groups.pop().unwrap();
        let session = Self { next_id, active, tabs, closed, recent };
        session.validate()?;
        Ok(session)
    }
}

/// Worker-only load. Missing means a new session; corruption is never silently reset.
pub fn load_session(path: &Path) -> io::Result<Session> {
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Session::default()),
        Err(e) => return Err(e),
    };
    if f.metadata()?.len() > MAX_METADATA as u64 {
        return Err(invalid("metadata size"));
    }
    let mut b = Vec::new();
    (&mut f).take(MAX_METADATA as u64 + 1).read_to_end(&mut b)?;
    Session::decode(&b)
}

/// Worker-only atomic replacement, serialized across workers/processes by an OS lock.
pub fn store_session(path: &Path, session: &Session) -> io::Result<()> {
    let bytes = session.encode()?;
    let parent = path.parent().ok_or_else(|| invalid("metadata parent"))?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path.with_extension("session-lock"))?;
    lock.try_lock().map_err(io::Error::other)?;
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let temp = parent.join(format!(
        ".kwil-session-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    // Never clean up a preexisting name if create_new refused it.
    let mut f = private_new(&temp)?;
    let result = (|| {
        f.write_all(&bytes)?;
        f.sync_all()?;
        drop(f);
        atomic_replace(&temp, path)?;
        sync_parent(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}
type SessionWrite = (Session, SyncSender<io::Result<()>>);
pub struct SessionWorker {
    tx: Option<SyncSender<SessionWrite>>,
    pub loaded: Receiver<io::Result<Session>>,
    join: Option<JoinHandle<()>>,
}
impl SessionWorker {
    pub fn spawn(path: PathBuf) -> io::Result<Self> {
        let (tx, rx) = mpsc::sync_channel::<SessionWrite>(2);
        let (events_tx, loaded) = mpsc::sync_channel(1);
        let join = thread::Builder::new().name("kwil-session".into()).spawn(move || {
            let _ = events_tx.try_send(load_session(&path));
            while let Ok((s, reply)) = rx.recv() {
                let _ = reply.try_send(store_session(&path, &s));
            }
        })?;
        Ok(Self { tx: Some(tx), loaded, join: Some(join) })
    }
    pub fn try_save(&self, session: Session) -> Result<Receiver<io::Result<()>>, TrySendError<Session>> {
        let (reply, result) = mpsc::sync_channel(1);
        if let Err(error) = session.validate() {
            let _ = reply.try_send(Err(error));
            return Ok(result);
        }
        self.tx.as_ref().unwrap().try_send((session, reply)).map(|_| result).map_err(|e| match e {
            TrySendError::Full((s, _)) => TrySendError::Full(s),
            TrySendError::Disconnected((s, _)) => TrySendError::Disconnected(s),
        })
    }
    /// Worker/shutdown thread only: drains queued writes and joins. Never call from UI input.
    pub fn shutdown(mut self) -> io::Result<()> {
        self.tx.take();
        self.join.take().unwrap().join().map_err(|_| io::Error::other("session worker panicked"))
    }
}
impl Drop for SessionWorker {
    fn drop(&mut self) {
        self.tx.take(); /* detached worker drains; explicit shutdown joins */
    }
}

pub(crate) fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
pub(crate) fn put32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}
pub(crate) fn put64(b: &mut Vec<u8>, v: u64) {
    b.extend_from_slice(&v.to_le_bytes());
}
pub(crate) fn validate_path(p: &Path) -> io::Result<()> {
    if p.as_os_str().len() > MAX_PATH_BYTES {
        return Err(invalid("path size"));
    }
    let bytes = path_bytes(p);
    #[cfg(unix)]
    let nul = bytes.contains(&0);
    #[cfg(windows)]
    let nul = bytes.as_chunks::<2>().0.contains(&[0, 0]);
    if bytes.is_empty() || bytes.len() > MAX_PATH_BYTES || nul {
        return Err(invalid("path size or NUL"));
    }
    Ok(())
}
#[cfg(unix)]
fn path_bytes(p: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    p.as_os_str().as_bytes().to_vec()
}
#[cfg(windows)]
fn path_bytes(p: &Path) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str().encode_wide().flat_map(u16::to_le_bytes).collect()
}
pub(crate) fn put_path(b: &mut Vec<u8>, p: Option<&Path>) -> io::Result<()> {
    if let Some(p) = p {
        validate_path(p)?;
        let bytes = path_bytes(p);
        put32(b, bytes.len() as u32);
        b.extend(bytes);
    } else {
        put32(b, 0);
    }
    Ok(())
}
pub(crate) struct Decoder<'a> {
    b: &'a [u8],
    pos: usize,
}
impl<'a> Decoder<'a> {
    pub(crate) fn new(b: &'a [u8]) -> Self {
        Self { b, pos: 0 }
    }
    pub(crate) fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or_else(|| invalid("length overflow"))?;
        let b = self.b.get(self.pos..end).ok_or_else(|| invalid("truncated record"))?;
        self.pos = end;
        Ok(b)
    }
    pub(crate) fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub(crate) fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub(crate) fn count(&mut self, cap: usize) -> io::Result<usize> {
        let n = self.u32()? as usize;
        if n > cap {
            return Err(invalid("count limit"));
        }
        Ok(n)
    }
    pub(crate) fn path(&mut self) -> io::Result<Option<PathBuf>> {
        let n = self.count(MAX_PATH_BYTES)?;
        if n == 0 {
            return Ok(None);
        }
        let b = self.take(n)?;
        #[cfg(unix)]
        let path = {
            use std::os::unix::ffi::OsStringExt;
            if b.contains(&0) {
                return Err(invalid("NUL path"));
            }
            PathBuf::from(OsString::from_vec(b.to_vec()))
        };
        #[cfg(windows)]
        let path = {
            use std::os::windows::ffi::OsStringExt;
            if n % 2 != 0 {
                return Err(invalid("UTF-16 path size"));
            }
            let wide: Vec<u16> = b.as_chunks::<2>().0.iter().map(|p| u16::from_le_bytes([p[0], p[1]])).collect();
            if wide.contains(&0) {
                return Err(invalid("NUL path"));
            }
            PathBuf::from(OsString::from_wide(&wide))
        };
        Ok(Some(path))
    }
    pub(crate) fn end(&self) -> io::Result<()> {
        if self.pos == self.b.len() { Ok(()) } else { Err(invalid("trailing bytes")) }
    }
}
pub(crate) fn checksum(b: &[u8]) -> u32 {
    const fn table() -> [u32; 256] {
        let mut entries = [0u32; 256];
        let mut i = 0;
        while i < 256 {
            let mut crc = i as u32;
            let mut bit = 0;
            while bit < 8 {
                crc = (crc >> 1) ^ (0xedb88320u32 & (0u32.wrapping_sub(crc & 1)));
                bit += 1;
            }
            entries[i] = crc;
            i += 1;
        }
        entries
    }
    const TABLE: [u32; 256] = table();
    let mut crc = !0u32;
    for &v in b {
        crc = (crc >> 8) ^ TABLE[((crc as u8) ^ v) as usize];
    }
    !crc
}
pub(crate) fn private_new(path: &Path) -> io::Result<File> {
    let mut o = OpenOptions::new();
    o.write(true).read(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(path)
}
pub(crate) fn sync_parent(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()?;
    }
    let _ = path;
    Ok(())
}
pub(crate) fn atomic_replace(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::rename(from, to)
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW};
        let a: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
        let b: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
        if unsafe { MoveFileExW(a.as_ptr(), b.as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}
