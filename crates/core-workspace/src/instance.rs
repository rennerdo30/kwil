//! Worker-only, bounded local IPC. Paths use native Unix bytes / Windows UTF-16.
//! No bearer secret or credential files: peer identity is checked by the OS.
//! The app resolves relative CLI paths before forwarding. Position numbers are 1-based.
//!
//! Protocol: a launcher sends one frame and reads a one-byte acknowledgement. `KWI1`
//! is the original request. `KWI2` adds a launcher-chosen wait token (`kwil --wait`):
//! after the acknowledgement the primary keeps that connection open and later writes
//! one outcome byte when every file of the request is closed. Closing the connection
//! without an outcome means the primary died. A launcher that never asks to wait
//! sends `KWI1`, so it still works against, and is still served by, any version.
use crate::session::{Decoder, invalid, put_path, put32, put64};
use std::{
    collections::HashMap,
    io,
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex,
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};
pub const MAX_OPEN_FILES: usize = 128;
pub const MAX_IPC_BYTES: usize = 64 * 1024;
/// Launchers that may wait on one primary at the same time.
pub const MAX_WAITS: usize = 8;
const IO_TIMEOUT: Duration = Duration::from_millis(250);
/// A launcher gives up waiting after a week; no editing session needs more.
const WAIT_LIMIT: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenFile {
    pub path: PathBuf,
    pub line: Option<u64>,
    pub column: Option<u64>,
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OpenFiles {
    pub files: Vec<OpenFile>,
    /// Non-zero token chosen by a `--wait` launcher. The primary reports it back
    /// through [`Waits::complete`] when every file of this request is closed.
    pub wait: Option<u64>,
}
impl OpenFiles {
    pub fn encode(&self) -> io::Result<Vec<u8>> {
        if self.files.len() > MAX_OPEN_FILES {
            return Err(invalid("IPC file count"));
        }
        let mut b = match self.wait {
            None => b"KWI1".to_vec(),
            Some(0) => return Err(invalid("IPC wait token")),
            Some(token) => {
                let mut b = b"KWI2".to_vec();
                put64(&mut b, token);
                b
            }
        };
        put32(&mut b, self.files.len() as u32);
        for f in &self.files {
            if !f.path.is_absolute()
                || f.line == Some(0)
                || f.column == Some(0)
                || f.column.is_some() && f.line.is_none()
            {
                return Err(invalid("IPC path or position"));
            }
            put_path(&mut b, Some(&f.path))?;
            put64(&mut b, f.line.unwrap_or(0));
            put64(&mut b, f.column.unwrap_or(0));
            if b.len() > MAX_IPC_BYTES {
                return Err(invalid("IPC message size"));
            }
        }
        Ok(b)
    }
    pub fn decode(b: &[u8]) -> io::Result<Self> {
        if b.len() > MAX_IPC_BYTES {
            return Err(invalid("IPC message size"));
        }
        let mut r = Decoder::new(b);
        let wait = match r.take(4)? {
            b"KWI1" => None,
            b"KWI2" => match r.u64()? {
                0 => return Err(invalid("IPC wait token")),
                token => Some(token),
            },
            _ => return Err(invalid("IPC version")),
        };
        let n = r.count(MAX_OPEN_FILES)?;
        let mut files = Vec::with_capacity(n);
        for _ in 0..n {
            let path = r.path()?.ok_or_else(|| invalid("IPC missing path"))?;
            let line = match r.u64()? {
                0 => None,
                n => Some(n),
            };
            let column = match r.u64()? {
                0 => None,
                n => Some(n),
            };
            if !path.is_absolute() || column.is_some() && line.is_none() {
                return Err(invalid("IPC path/position"));
            }
            files.push(OpenFile { path, line, column });
        }
        r.end()?;
        Ok(Self { files, wait })
    }
}

/// What a waiting launcher learns when it may return.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitOutcome {
    /// Every file of the request is closed (or the editor quit normally).
    Closed,
    /// A file could not be opened.
    OpenFailed,
}
impl WaitOutcome {
    fn byte(self) -> u8 {
        match self {
            Self::Closed => 0,
            Self::OpenFailed => 1,
        }
    }
    fn from_byte(b: u8) -> io::Result<Self> {
        match b {
            0 => Ok(Self::Closed),
            1 => Ok(Self::OpenFailed),
            _ => Err(invalid("IPC wait outcome")),
        }
    }
}

/// Parked `--wait` launchers of one primary. Each owns its connection on its own
/// thread, blocked in `recv`: no polling and no timer while a file stays open.
pub struct Waits {
    state: Mutex<WaitState>,
    idle: Condvar,
}
#[derive(Default)]
struct WaitState {
    slots: HashMap<u64, SyncSender<WaitOutcome>>,
    /// Tokens whose files were opened in the editor. Only these can have been
    /// edited, so only these count as closed when the primary quits.
    shown: std::collections::HashSet<u64>,
    live: usize,
    closed: bool,
}
impl Waits {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { state: Mutex::new(WaitState::default()), idle: Condvar::new() })
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, WaitState> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }
    /// Reserves `token` and returns the receiver its outcome arrives on. None for
    /// token 0, a duplicate, a full table or a closed one. The server calls this for
    /// every wait request; it is public so an embedder or a test can host a waiter.
    pub fn admit(&self, token: u64) -> Option<Receiver<WaitOutcome>> {
        let mut state = self.lock();
        if token == 0 || state.closed || state.slots.len() >= MAX_WAITS || state.slots.contains_key(&token) {
            return None;
        }
        let (tx, rx) = mpsc::sync_channel(1);
        state.slots.insert(token, tx);
        Some(rx)
    }
    fn forget(&self, token: u64) {
        let mut state = self.lock();
        state.slots.remove(&token);
        state.shown.remove(&token);
    }
    /// Hands the connection to a thread that writes the outcome once it is known.
    fn park(
        self: &Arc<Self>,
        token: u64,
        rx: Receiver<WaitOutcome>,
        finish: impl FnOnce(WaitOutcome) + Send + 'static,
    ) {
        self.lock().live += 1;
        let waits = self.clone();
        let spawned = thread::Builder::new().name("kwil-wait".into()).spawn(move || {
            // A sender dropped without a send (table cleared) means a normal exit.
            finish(rx.recv().unwrap_or(WaitOutcome::Closed));
            waits.lock().live -= 1;
            waits.idle.notify_all();
        });
        if spawned.is_err() {
            // The unmoved closure was dropped with the failed spawn, closing the
            // connection: the launcher reports a lost editor rather than hanging.
            self.forget(token);
            self.lock().live -= 1;
        }
    }
    /// The files of `token` are open in the editor, so the user can edit them.
    pub fn shown(&self, token: u64) {
        let mut state = self.lock();
        if state.slots.contains_key(&token) {
            state.shown.insert(token);
        }
    }
    /// Reports that the request with `token` is finished. Unknown tokens are ignored.
    pub fn complete(&self, token: u64, outcome: WaitOutcome) {
        let mut state = self.lock();
        state.shown.remove(&token);
        if let Some(tx) = state.slots.remove(&token) {
            let _ = tx.try_send(outcome);
        }
    }
    /// The primary is quitting. Launchers whose files were shown get `outcome`;
    /// a request the user never saw (the editor failed to start, or quit before
    /// it arrived) reports `OpenFailed`, so Git never proceeds with an unedited
    /// message.
    pub fn complete_all(&self, outcome: WaitOutcome) {
        let mut state = self.lock();
        state.closed = true;
        let shown = std::mem::take(&mut state.shown);
        for (token, tx) in state.slots.drain() {
            let _ = tx.try_send(if shown.contains(&token) { outcome } else { WaitOutcome::OpenFailed });
        }
    }
    /// Waits until every parked launcher has been told. Exit path only; bounded.
    pub fn drain(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = self.lock();
        while state.live > 0 {
            let Some(left) = deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) else {
                return false;
            };
            state = self.idle.wait_timeout(state, left).unwrap_or_else(|error| error.into_inner()).0;
        }
        true
    }
}

#[derive(Clone, Debug)]
pub struct InstanceEndpoint {
    /// Unix: private existing parent, endpoint directory created 0700 beneath it.
    /// Windows: unused; named pipe identity is derived from the current token SID.
    pub runtime_directory: PathBuf,
    /// ASCII letters, numbers, dash/underscore only, max 48. Separates profiles/tests.
    pub namespace: String,
}
impl InstanceEndpoint {
    fn validate(&self) -> io::Result<()> {
        if self.namespace.is_empty()
            || self.namespace.len() > 48
            || !self.namespace.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(invalid("IPC namespace"));
        }
        Ok(())
    }
}
pub struct InstanceServer {
    stop: platform::Stop,
    pub ready: Receiver<io::Result<()>>,
    pub requests: Receiver<OpenFiles>,
    pub finished: Receiver<io::Result<()>>,
    /// Launchers parked by `--wait`; the app completes them as their files close.
    pub waits: Arc<Waits>,
}
impl InstanceServer {
    /// Spawns without opening disk/endpoints on the caller. Bind result comes via ready.
    pub fn spawn(endpoint: InstanceEndpoint) -> io::Result<Self> {
        endpoint.validate()?;
        let (stop, signal) = platform::stop_pair()?;
        let (ready_tx, ready) = mpsc::sync_channel(1);
        let (tx, requests) = mpsc::sync_channel(16);
        let (done_tx, finished) = mpsc::sync_channel(1);
        let waits = Waits::new();
        let served = waits.clone();
        thread::Builder::new().name("kwil-instance".into()).spawn(move || {
            let result = platform::serve(endpoint, signal, ready_tx, tx, served);
            let _ = done_tx.try_send(result);
        })?;
        Ok(Self { stop, ready, requests, finished, waits })
    }
    /// Wakes the blocking accept, cancels pending client I/O and releases only our endpoint.
    /// Completion is reported by finished; caller chooses a shutdown timeout off the UI.
    /// Parked `--wait` launchers are told the editor closed normally.
    pub fn shutdown(&self) {
        self.stop.signal();
        self.waits.complete_all(WaitOutcome::Closed);
    }
}
impl Drop for InstanceServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}
/// Worker-only. Acknowledgement means queued at the receiver; the frontend still must
/// consume `requests`, wake its event loop and open files. Timeout is not permission to
/// delete an endpoint/start a conflicting instance. Bind uses OS ownership separately.
pub fn send_files(endpoint: &InstanceEndpoint, request: &OpenFiles, timeout: Duration) -> io::Result<()> {
    endpoint.validate()?;
    let b = request.encode()?;
    platform::send(endpoint, &b, timeout).map(drop)
}

/// Why [`send_files_and_wait`] returned without an outcome.
#[derive(Debug)]
pub enum WaitError {
    /// Nothing was admitted: same errors as [`send_files`] (`NotFound` or
    /// `ConnectionRefused` mean there is no primary).
    NotDelivered(io::Error),
    /// The primary accepted the request, then vanished before reporting.
    Lost(io::Error),
}
/// Worker-only. Sends `request` (its `wait` token must be set), then blocks until the
/// primary reports it finished or disappears. `timeout` bounds connect and admission.
pub fn send_files_and_wait(
    endpoint: &InstanceEndpoint,
    request: &OpenFiles,
    timeout: Duration,
) -> Result<WaitOutcome, WaitError> {
    if request.wait.is_none() {
        return Err(WaitError::NotDelivered(invalid("IPC wait token")));
    }
    endpoint.validate().map_err(WaitError::NotDelivered)?;
    let b = request.encode().map_err(WaitError::NotDelivered)?;
    let connection = platform::send(endpoint, &b, timeout).map_err(WaitError::NotDelivered)?;
    connection.outcome().map_err(WaitError::Lost)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn kw20_transport_rejects_oversize_and_malformed_without_losing_owner() {
        let root = std::env::temp_dir().join(format!("kwil-kw20-wire-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let endpoint =
            InstanceEndpoint { runtime_directory: root.clone(), namespace: format!("wire-{}", std::process::id()) };
        let server = InstanceServer::spawn(endpoint.clone()).unwrap();
        server.ready.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
        assert!(platform::send(&endpoint, b"not-a-valid-message", Duration::from_secs(2)).is_err());
        assert!(platform::send(&endpoint, &vec![0; MAX_IPC_BYTES + 1], Duration::from_secs(2)).is_err());
        let valid = OpenFiles::default();
        send_files(&endpoint, &valid, Duration::from_secs(2)).unwrap();
        assert_eq!(server.requests.recv_timeout(Duration::from_secs(2)).unwrap(), valid);
        server.shutdown();
        server.finished.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn kw90_wait_table_admits_each_token_once_is_bounded_and_closes() {
        let waits = Waits::new();
        assert!(waits.admit(0).is_none(), "0 means no wait");
        let first = waits.admit(1).unwrap();
        assert!(waits.admit(1).is_none(), "a token is one launch");
        let others: Vec<_> = (2..=MAX_WAITS as u64).map(|token| waits.admit(token).unwrap()).collect();
        assert!(waits.admit(1000).is_none(), "the table is bounded");
        waits.complete(1, WaitOutcome::OpenFailed);
        assert_eq!(first.try_recv().unwrap(), WaitOutcome::OpenFailed);
        waits.complete(1, WaitOutcome::Closed);
        waits.complete(999, WaitOutcome::Closed);
        assert!(waits.admit(1).is_some(), "a completed slot is free again");
        // Token 2 was shown to the user; the others never reached the editor.
        waits.shown(2);
        waits.complete_all(WaitOutcome::Closed);
        let (shown, unseen) = others.split_first().unwrap();
        assert_eq!(shown.try_recv(), Ok(WaitOutcome::Closed), "a shown file closes normally");
        assert!(
            unseen.iter().all(|rx| rx.try_recv() == Ok(WaitOutcome::OpenFailed)),
            "a request the user never saw must not tell Git the edit is done"
        );
        assert!(waits.admit(77).is_none(), "nothing is parked on a closed primary");
        assert!(waits.drain(Duration::from_millis(10)), "no thread is parked");
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::{
        ptr::{null, null_mut},
        sync::{Arc, mpsc::SyncSender},
        time::Instant,
    };
    use windows_sys::Win32::{
        Foundation::*,
        Security::{Authorization::*, *},
        Storage::FileSystem::*,
        System::{IO::*, Pipes::*, Threading::*},
    };
    struct Handle(HANDLE);
    // SAFETY: the owned handles are kernel event/pipe/process handles. Shared events
    // only use SetEvent/waits; pipe operations remain on their owning worker.
    unsafe impl Send for Handle {}
    unsafe impl Sync for Handle {}
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    impl Handle {
        fn new(h: HANDLE) -> io::Result<Self> {
            if h.is_null() || h == INVALID_HANDLE_VALUE { Err(io::Error::last_os_error()) } else { Ok(Self(h)) }
        }
    }
    struct Local(*mut std::ffi::c_void);
    impl Drop for Local {
        fn drop(&mut self) {
            unsafe {
                LocalFree(self.0);
            }
        }
    }
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }
    pub struct Stop(Arc<Handle>);
    impl Stop {
        pub fn signal(&self) {
            unsafe {
                SetEvent(self.0.0);
            }
        }
    }
    pub struct Signal(Arc<Handle>);
    pub fn stop_pair() -> io::Result<(Stop, Signal)> {
        let event = Arc::new(Handle::new(unsafe { CreateEventW(null(), 1, 0, null()) })?);
        Ok((Stop(event.clone()), Signal(event)))
    }
    struct User(Vec<usize>);
    impl User {
        fn from_process(process: HANDLE) -> io::Result<Self> {
            let mut token = null_mut();
            if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
                return Err(io::Error::last_os_error());
            }
            let token = Handle::new(token)?;
            let mut storage = vec![0usize; 256];
            let mut needed = 0;
            if unsafe {
                GetTokenInformation(
                    token.0,
                    TokenUser,
                    storage.as_mut_ptr().cast(),
                    (storage.len() * size_of::<usize>()) as u32,
                    &mut needed,
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(Self(storage))
        }
        fn current() -> io::Result<Self> {
            Self::from_process(unsafe { GetCurrentProcess() })
        }
        fn sid(&self) -> PSID {
            unsafe { (*(self.0.as_ptr().cast::<TOKEN_USER>())).User.Sid }
        }
        fn text(&self) -> io::Result<String> {
            let mut p = null_mut();
            if unsafe { ConvertSidToStringSidW(self.sid(), &mut p) } == 0 {
                return Err(io::Error::last_os_error());
            }
            let _allocation = Local(p.cast());
            let mut len = 0;
            while len < 256 && unsafe { *p.add(len) } != 0 {
                len += 1;
            }
            if len == 256 {
                return Err(invalid("SID length"));
            }
            String::from_utf16(unsafe { std::slice::from_raw_parts(p, len) }).map_err(|_| invalid("SID encoding"))
        }
    }
    fn name(e: &InstanceEndpoint, user: &User) -> io::Result<Vec<u16>> {
        Ok(wide(&format!("\\\\.\\pipe\\kwil-{}-{}", e.namespace, user.text()?)))
    }
    fn authenticate(pipe: HANDLE, server: bool, user: &User) -> io::Result<()> {
        let mut pid = 0;
        let ok = unsafe {
            if server {
                GetNamedPipeServerProcessId(pipe, &mut pid)
            } else {
                GetNamedPipeClientProcessId(pipe, &mut pid)
            }
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let process = Handle::new(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) })?;
        let peer = User::from_process(process.0)?;
        if unsafe { EqualSid(user.sid(), peer.sid()) } == 0 {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "foreign IPC peer"));
        }
        Ok(())
    }
    // The OVERLAPPED and transfer buffers live until completion, even on cancellation.
    // No cancellation returns before GetOverlappedResult has reaped that operation.
    fn operation(
        pipe: HANDLE,
        stop: HANDLE,
        timeout: u32,
        call: impl FnOnce(*mut OVERLAPPED) -> i32,
    ) -> io::Result<u32> {
        let event = Handle::new(unsafe { CreateEventW(null(), 1, 0, null()) })?;
        let mut overlapped = OVERLAPPED { hEvent: event.0, ..Default::default() };
        let ok = call(&mut overlapped);
        if ok == 0 {
            let error = unsafe { GetLastError() };
            if error == ERROR_PIPE_CONNECTED {
                return Ok(0);
            }
            if error != ERROR_IO_PENDING {
                return Err(io::Error::from_raw_os_error(error as i32));
            }
            let handles = [stop, event.0];
            let count = if stop.is_null() { 1 } else { 2 };
            let ptr = if stop.is_null() { unsafe { handles.as_ptr().add(1) } } else { handles.as_ptr() };
            let wait = unsafe { WaitForMultipleObjects(count, ptr, 0, timeout) };
            let success = if stop.is_null() { WAIT_OBJECT_0 } else { WAIT_OBJECT_0 + 1 };
            if wait != success {
                unsafe {
                    CancelIoEx(pipe, &overlapped);
                }
                let mut transferred = 0;
                unsafe {
                    GetOverlappedResult(pipe, &overlapped, &mut transferred, 1);
                }
                return Err(io::Error::new(
                    if wait == WAIT_TIMEOUT { io::ErrorKind::TimedOut } else { io::ErrorKind::Interrupted },
                    "IPC cancelled or timed out",
                ));
            }
        }
        let mut transferred = 0;
        if unsafe { GetOverlappedResult(pipe, &overlapped, &mut transferred, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(transferred)
    }
    fn remaining(deadline: Instant) -> io::Result<u32> {
        let d = deadline.saturating_duration_since(Instant::now());
        if d.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "IPC deadline"));
        }
        Ok(d.as_millis().clamp(1, u32::MAX as u128 - 1) as u32)
    }
    fn read(pipe: HANDLE, stop: HANDLE, b: &mut [u8], deadline: Instant) -> io::Result<()> {
        let mut p = 0;
        while p < b.len() {
            let n = operation(pipe, stop, remaining(deadline)?, |o| unsafe {
                ReadFile(pipe, b[p..].as_mut_ptr(), (b.len() - p) as u32, null_mut(), o)
            })? as usize;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "IPC closed"));
            }
            p += n;
        }
        Ok(())
    }
    fn write(pipe: HANDLE, stop: HANDLE, b: &[u8], deadline: Instant) -> io::Result<()> {
        let mut p = 0;
        while p < b.len() {
            let n = operation(pipe, stop, remaining(deadline)?, |o| unsafe {
                WriteFile(pipe, b[p..].as_ptr(), (b.len() - p) as u32, null_mut(), o)
            })? as usize;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "IPC closed"));
            }
            p += n;
        }
        Ok(())
    }
    /// Only the first instance carries FILE_FLAG_FIRST_PIPE_INSTANCE: that is the
    /// single-owner guard. Later instances replace a connection parked by `--wait`.
    /// Every instance must state the same maximum: a listener plus the parked waits.
    fn create_pipe(name: &[u16], user: &User, first: bool) -> io::Result<Handle> {
        let sddl = wide(&format!("D:P(A;;GA;;;{})", user.text()?));
        let mut sd = null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION, &mut sd, null_mut())
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let _descriptor = Local(sd);
        let attrs = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd,
            bInheritHandle: 0,
        };
        Handle::new(unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 },
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                MAX_WAITS as u32 + 2,
                MAX_IPC_BYTES as u32 + 4,
                MAX_IPC_BYTES as u32 + 4,
                250,
                &attrs,
            )
        })
    }
    /// A parked launcher's end of the pipe: outcome, the client's receipt (a pipe
    /// disconnect discards unread output), then release.
    fn finish(pipe: Handle, outcome: WaitOutcome) {
        let deadline = Instant::now() + 4 * IO_TIMEOUT;
        let _ = write(pipe.0, null_mut(), &[outcome.byte()], deadline)
            .and_then(|()| read(pipe.0, null_mut(), &mut [0], deadline));
        unsafe {
            DisconnectNamedPipe(pipe.0);
        }
    }
    pub fn serve(
        e: InstanceEndpoint,
        stop: Signal,
        ready: SyncSender<io::Result<()>>,
        tx: SyncSender<OpenFiles>,
        waits: Arc<Waits>,
    ) -> io::Result<()> {
        let init: io::Result<(Handle, User, Vec<u16>)> = (|| {
            let user = User::current()?;
            let name = name(&e, &user)?;
            let pipe = create_pipe(&name, &user, true)?;
            Ok((pipe, user, name))
        })();
        let (mut pipe, user, name) = match init {
            Ok(v) => v,
            Err(e) => {
                let _ = ready.try_send(Err(io::Error::new(e.kind(), e.to_string())));
                return Err(e);
            }
        };
        let _ = ready.try_send(Ok(()));
        loop {
            if unsafe { WaitForSingleObject(stop.0.0, 0) } == WAIT_OBJECT_0 {
                return Ok(());
            }
            match operation(pipe.0, stop.0.0, INFINITE, |o| unsafe { ConnectNamedPipe(pipe.0, o) }) {
                Ok(_) => (),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(()),
                Err(e) if matches!(e.raw_os_error(),Some(n) if n==ERROR_NO_DATA as i32 || n==ERROR_BROKEN_PIPE as i32) =>
                {
                    // A launcher may disappear before ConnectNamedPipe observes it.
                    // This is a dead client, not loss of endpoint ownership.
                    unsafe {
                        DisconnectNamedPipe(pipe.0);
                    }
                    continue;
                }
                Err(e) => return Err(e),
            }
            let deadline = Instant::now() + IO_TIMEOUT;
            // A wait request keeps this instance connected; `next` takes over listening.
            let mut parked = None;
            let result = (|| {
                authenticate(pipe.0, false, &user)?;
                let mut header = [0u8; 4];
                read(pipe.0, stop.0.0, &mut header, deadline)?;
                let n = u32::from_le_bytes(header) as usize;
                if !(8..=MAX_IPC_BYTES).contains(&n) {
                    return Err(invalid("IPC frame size"));
                }
                let mut b = vec![0; n];
                read(pipe.0, stop.0.0, &mut b, deadline)?;
                let request = OpenFiles::decode(&b)?;
                let accepted = match request.wait {
                    None => tx.try_send(request).is_ok(),
                    Some(token) => match (waits.admit(token), create_pipe(&name, &user, false)) {
                        (Some(rx), Ok(next)) if tx.try_send(request).is_ok() => {
                            parked = Some((token, rx, next));
                            true
                        }
                        (admitted, _) => {
                            if admitted.is_some() {
                                waits.forget(token);
                            }
                            false
                        }
                    },
                };
                write(pipe.0, stop.0.0, &[u8::from(accepted)], deadline)?;
                // Named-pipe disconnect discards unread output. Wait for the
                // client's receipt under the same deadline, never FlushFileBuffers.
                let mut received = [0];
                read(pipe.0, stop.0.0, &mut received, deadline)
            })();
            match (result, parked) {
                (Ok(()), Some((token, rx, next))) => {
                    let connected = std::mem::replace(&mut pipe, next);
                    waits.park(token, rx, move |outcome| finish(connected, outcome));
                }
                (result, parked) => {
                    if let (Err(_), Some((token, ..))) = (result, parked) {
                        waits.forget(token);
                    }
                    unsafe {
                        DisconnectNamedPipe(pipe.0);
                    }
                }
            }
        }
    }
    /// The launcher's end after admission: it keeps the pipe only to learn the outcome.
    pub struct Connection(Handle);
    impl Connection {
        pub fn outcome(self) -> io::Result<WaitOutcome> {
            let deadline = Instant::now().checked_add(WAIT_LIMIT).ok_or_else(|| invalid("IPC timeout"))?;
            let mut byte = [0];
            read(self.0.0, null_mut(), &mut byte, deadline)?;
            let outcome = WaitOutcome::from_byte(byte[0])?;
            let _ = write(self.0.0, null_mut(), &[1], deadline);
            Ok(outcome)
        }
    }
    pub fn send(e: &InstanceEndpoint, b: &[u8], timeout: Duration) -> io::Result<Connection> {
        let user = User::current()?;
        let name = name(e, &user)?;
        let deadline = Instant::now().checked_add(timeout).ok_or_else(|| invalid("IPC timeout"))?;
        let pipe = loop {
            remaining(deadline)?;
            let h = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    null(),
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED,
                    null_mut(),
                )
            };
            if h != INVALID_HANDLE_VALUE {
                break Handle::new(h)?;
            }
            let error = unsafe { GetLastError() };
            if error != ERROR_PIPE_BUSY {
                return Err(io::Error::from_raw_os_error(error as i32));
            }
            if unsafe { WaitNamedPipeW(name.as_ptr(), remaining(deadline)?) } == 0 {
                return Err(io::Error::last_os_error());
            }
        };
        authenticate(pipe.0, true, &user)?;
        write(pipe.0, null_mut(), &(b.len() as u32).to_le_bytes(), deadline)?;
        write(pipe.0, null_mut(), b, deadline)?;
        let mut ack = [0];
        read(pipe.0, null_mut(), &mut ack, deadline)?;
        write(pipe.0, null_mut(), &[1], deadline)?;
        if ack != [1] {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "instance inbox full"));
        }
        Ok(Connection(pipe))
    }
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::{
        fs::{self, File, OpenOptions},
        io::{Read, Write},
        os::{
            fd::{AsRawFd, FromRawFd, RawFd},
            unix::{
                fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
                net::{UnixListener, UnixStream},
            },
        },
        path::Path,
        sync::mpsc::SyncSender,
        time::Instant,
    };
    unsafe extern "C" {
        fn geteuid() -> u32;
        fn poll(fds: *mut PollFd, nfds: Nfds, timeout: i32) -> i32;
    }
    #[cfg(target_os = "linux")]
    type Nfds = usize;
    #[cfg(not(target_os = "linux"))]
    type Nfds = u32;
    #[repr(C)]
    struct PollFd {
        fd: i32,
        events: i16,
        revents: i16,
    }
    fn uid() -> u32 {
        unsafe { geteuid() }
    }
    pub struct Stop(UnixStream);
    impl Stop {
        pub fn signal(&self) {
            let mut s = &self.0;
            let _ = s.write(&[1]);
        }
    }
    pub fn stop_pair() -> io::Result<(Stop, UnixStream)> {
        let (a, b) = UnixStream::pair()?;
        a.set_nonblocking(true)?;
        Ok((Stop(a), b))
    }
    fn private_dir(root: &Path) -> io::Result<()> {
        let parent = root.parent().ok_or_else(|| invalid("IPC directory parent"))?;
        let p = fs::symlink_metadata(parent)?;
        if !p.is_dir() || p.uid() != uid() || p.mode() & 0o022 != 0 {
            return Err(invalid("IPC parent must be owned and not writable by other users"));
        }
        match std::fs::DirBuilder::new().mode(0o700).create(root) {
            Ok(()) => (),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(e),
        }
        let m = fs::symlink_metadata(root)?;
        if !m.is_dir() || m.uid() != uid() || m.mode() & 0o077 != 0 {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "IPC requires owned private directory"));
        }
        Ok(())
    }
    fn dir(e: &InstanceEndpoint) -> PathBuf {
        e.runtime_directory.join(&e.namespace)
    }
    struct OwnedSocket {
        path: PathBuf,
        dev: u64,
        ino: u64,
        _lock: File,
    }
    impl Drop for OwnedSocket {
        fn drop(&mut self) {
            if let Ok(m) = fs::symlink_metadata(&self.path)
                && m.dev() == self.dev
                && m.ino() == self.ino
            {
                let _ = fs::remove_file(&self.path);
            }
        }
    }
    /// A parked launcher's end of the socket: the outcome, then close. Unlike a
    /// named pipe, a closed socket still delivers bytes already written.
    fn finish(mut stream: UnixStream, outcome: WaitOutcome) {
        let _ = write_deadline(&mut stream, &[outcome.byte()], Instant::now() + 4 * IO_TIMEOUT);
    }
    pub fn serve(
        e: InstanceEndpoint,
        stop: UnixStream,
        ready: SyncSender<io::Result<()>>,
        tx: SyncSender<OpenFiles>,
        waits: Arc<Waits>,
    ) -> io::Result<()> {
        let init = (|| {
            let directory = dir(&e);
            private_dir(&directory)?;
            let lockpath = directory.join("owner.lock");
            if let Ok(m) = fs::symlink_metadata(&lockpath)
                && (!m.is_file() || m.uid() != uid() || m.mode() & 0o077 != 0)
            {
                return Err(invalid("unsafe IPC lock"));
            }
            let lock =
                OpenOptions::new().create(true).truncate(false).read(true).write(true).mode(0o600).open(lockpath)?;
            lock.try_lock().map_err(|_| io::Error::new(io::ErrorKind::AddrInUse, "instance endpoint already owned"))?;
            let path = directory.join("open.sock");
            if let Ok(m) = fs::symlink_metadata(&path) {
                use std::os::unix::fs::FileTypeExt;
                if !m.file_type().is_socket() || m.uid() != uid() {
                    return Err(invalid("foreign IPC endpoint"));
                }
                fs::remove_file(&path)?;
            }
            let listener = UnixListener::bind(&path)?;
            listener.set_nonblocking(true)?;
            let m = fs::symlink_metadata(&path)?;
            let owner = OwnedSocket { path, dev: m.dev(), ino: m.ino(), _lock: lock };
            Ok((listener, owner))
        })();
        let (listener, _owner) = match init {
            Ok(v) => v,
            Err(e) => {
                let _ = ready.try_send(Err(io::Error::new(e.kind(), e.to_string())));
                return Err(e);
            }
        };
        let _ = ready.try_send(Ok(()));
        loop {
            let mut fds = [
                PollFd { fd: listener.as_raw_fd(), events: 1, revents: 0 },
                PollFd { fd: stop.as_raw_fd(), events: 1, revents: 0 },
            ];
            if unsafe { poll(fds.as_mut_ptr(), 2, -1) } < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if fds[1].revents != 0 {
                return Ok(());
            }
            if fds[0].revents == 0 {
                continue;
            }
            let (mut stream, _) = match listener.accept() {
                Ok(s) => s,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(e),
            };
            // Use readiness and one absolute deadline for every frame operation.
            stream.set_nonblocking(true)?;
            let deadline = Instant::now() + IO_TIMEOUT;
            let mut parked = None;
            let result = (|| {
                authenticate(stream.as_raw_fd())?;
                let mut h = [0u8; 4];
                read_deadline(&mut stream, &mut h, deadline)?;
                let n = u32::from_le_bytes(h) as usize;
                if !(8..=MAX_IPC_BYTES).contains(&n) {
                    return Err(invalid("IPC frame size"));
                }
                let mut b = vec![0; n];
                read_deadline(&mut stream, &mut b, deadline)?;
                let request = OpenFiles::decode(&b)?;
                let accepted = match request.wait {
                    None => tx.try_send(request).is_ok(),
                    Some(token) => match waits.admit(token) {
                        Some(rx) if tx.try_send(request).is_ok() => {
                            parked = Some((token, rx));
                            true
                        }
                        Some(_) => {
                            waits.forget(token);
                            false
                        }
                        None => false,
                    },
                };
                write_deadline(&mut stream, &[u8::from(accepted)], deadline)
            })();
            match (result, parked) {
                (Ok(()), Some((token, rx))) => waits.park(token, rx, move |outcome| finish(stream, outcome)),
                (Err(_), Some((token, _))) => waits.forget(token),
                _ => {}
            }
        }
    }
    /// The launcher's end after admission: it keeps the socket only to learn the outcome.
    pub struct Connection(UnixStream);
    impl Connection {
        pub fn outcome(mut self) -> io::Result<WaitOutcome> {
            let deadline = Instant::now().checked_add(WAIT_LIMIT).ok_or_else(|| invalid("IPC timeout"))?;
            let mut byte = [0];
            read_deadline(&mut self.0, &mut byte, deadline)?;
            WaitOutcome::from_byte(byte[0])
        }
    }
    #[cfg(target_os = "linux")]
    fn authenticate(fd: RawFd) -> io::Result<()> {
        #[repr(C)]
        struct Cred {
            pid: i32,
            uid: u32,
            gid: u32,
        }
        unsafe extern "C" {
            fn getsockopt(fd: i32, level: i32, name: i32, value: *mut std::ffi::c_void, len: *mut u32) -> i32;
        }
        let mut cred = Cred { pid: 0, uid: 0, gid: 0 };
        let mut len = std::mem::size_of::<Cred>() as u32;
        if unsafe { getsockopt(fd, 1, 17, (&mut cred as *mut Cred).cast(), &mut len) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if len as usize != std::mem::size_of::<Cred>() || cred.uid != uid() {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "foreign IPC peer"));
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    fn authenticate(fd: RawFd) -> io::Result<()> {
        unsafe extern "C" {
            fn getpeereid(fd: i32, uid: *mut u32, gid: *mut u32) -> i32;
        }
        let mut peer = 0;
        let mut gid = 0;
        if unsafe { getpeereid(fd, &mut peer, &mut gid) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if peer != uid() {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "foreign IPC peer"));
        }
        Ok(())
    }
    pub fn send(e: &InstanceEndpoint, b: &[u8], timeout: Duration) -> io::Result<Connection> {
        if timeout.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "IPC timeout"));
        }
        let deadline = Instant::now().checked_add(timeout).ok_or_else(|| invalid("IPC timeout"))?;
        let directory = dir(e);
        let m = fs::symlink_metadata(&directory)?;
        if !m.is_dir() || m.uid() != uid() || m.mode() & 0o077 != 0 {
            return Err(invalid("unsafe IPC directory"));
        }
        let mut s = stage("connect", connect(&directory.join("open.sock"), deadline))?;
        stage("authenticate", authenticate(s.as_raw_fd()))?;
        stage("write length", write_deadline(&mut s, &(b.len() as u32).to_le_bytes(), deadline))?;
        stage("write payload", write_deadline(&mut s, b, deadline))?;
        let mut ack = [0];
        stage("read acknowledgement", read_deadline(&mut s, &mut ack, deadline))?;
        if ack != [1] {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "instance inbox full"));
        }
        Ok(Connection(s))
    }
    // Preserve the original error and its OS code. Development builds identify
    // which platform operation failed; release builds retain no diagnostic I/O.
    fn stage<T>(name: &'static str, result: io::Result<T>) -> io::Result<T> {
        #[cfg(debug_assertions)]
        if let Err(error) = &result {
            eprintln!("Unix IPC {name}: {error:?}");
        }
        #[cfg(not(debug_assertions))]
        let _ = name;
        result
    }
    fn remaining(deadline: Instant) -> io::Result<Duration> {
        let d = deadline.saturating_duration_since(Instant::now());
        if d.is_zero() { Err(io::Error::new(io::ErrorKind::TimedOut, "IPC deadline")) } else { Ok(d) }
    }
    fn read_deadline(s: &mut UnixStream, b: &mut [u8], deadline: Instant) -> io::Result<()> {
        let mut p = 0;
        while p < b.len() {
            remaining(deadline)?;
            let n = match s.read(&mut b[p..]) {
                Ok(n) => n,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    wait_ready(s, 1, deadline)?;
                    continue;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return stage("read", Err(error)),
            };
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "IPC closed"));
            }
            p += n;
        }
        Ok(())
    }
    fn write_deadline(s: &mut UnixStream, b: &[u8], deadline: Instant) -> io::Result<()> {
        let mut p = 0;
        while p < b.len() {
            remaining(deadline)?;
            let n = match s.write(&b[p..]) {
                Ok(n) => n,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    wait_ready(s, 4, deadline)?;
                    continue;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return stage("write", Err(error)),
            };
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "IPC closed"));
            }
            p += n;
        }
        Ok(())
    }
    fn wait_ready(s: &UnixStream, events: i16, deadline: Instant) -> io::Result<()> {
        let mut fd = PollFd { fd: s.as_raw_fd(), events, revents: 0 };
        loop {
            let ms = remaining(deadline)?.as_millis().clamp(1, i32::MAX as u128) as i32;
            let result = unsafe { poll(&mut fd, 1, ms) };
            if result > 0 {
                // Read even on hangup: the peer may have queued its ACK before
                // closing. macOS rejects setsockopt on that disconnected socket.
                return Ok(());
            }
            if result == 0 {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "IPC I/O deadline"));
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    // A blocking UnixStream::connect can hang on a full accept backlog. Create
    // nonblocking and wait only to the caller's deadline. Frame I/O stays nonblocking.
    fn connect(path: &Path, deadline: Instant) -> io::Result<UnixStream> {
        use std::os::unix::ffi::OsStrExt;
        unsafe extern "C" {
            fn socket(domain: i32, kind: i32, protocol: i32) -> i32;
            fn fcntl(fd: i32, cmd: i32, ...) -> i32;
            fn connect(fd: i32, address: *const std::ffi::c_void, len: u32) -> i32;
            fn getsockopt(fd: i32, level: i32, name: i32, value: *mut std::ffi::c_void, len: *mut u32) -> i32;
        }
        let bytes = path.as_os_str().as_bytes();
        #[cfg(target_os = "linux")]
        let (cap, nonblock, progress, sol, soerror) = (108, 0x800, 115, 1, 4);
        #[cfg(not(target_os = "linux"))]
        let (cap, nonblock, progress, sol, soerror) = (104, 4, 36, 0xffff, 0x1007);
        if bytes.len() >= cap {
            return Err(invalid("Unix socket pathname too long"));
        }
        let fd = unsafe { socket(1, 1, 0) };
        if fd < 0 {
            return stage("socket", Err(io::Error::last_os_error()));
        }
        // SAFETY: freshly owned descriptor; UnixStream closes it on every error.
        let stream = unsafe { UnixStream::from_raw_fd(fd) };
        if unsafe { fcntl(fd, 2, 1) } < 0 {
            return stage("set close-on-exec", Err(io::Error::last_os_error()));
        }
        let flags = unsafe { fcntl(fd, 3) };
        if flags < 0 || unsafe { fcntl(fd, 4, flags | nonblock) } < 0 {
            return stage("set nonblocking", Err(io::Error::last_os_error()));
        }
        let mut address = [0u8; 110];
        let n = 2 + bytes.len() + 1;
        #[cfg(target_os = "linux")]
        address[..2].copy_from_slice(&1u16.to_ne_bytes());
        #[cfg(not(target_os = "linux"))]
        {
            address[0] = n as u8;
            address[1] = 1;
        }
        address[2..2 + bytes.len()].copy_from_slice(bytes);
        if unsafe { connect(fd, address.as_ptr().cast(), n as u32) } < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(progress) {
                return stage("connect syscall", Err(e));
            }
            let mut ready = PollFd { fd, events: 4, revents: 0 };
            loop {
                let ms = remaining(deadline)?.as_millis().clamp(1, i32::MAX as u128) as i32;
                let result = unsafe { poll(&mut ready, 1, ms) };
                if result > 0 {
                    break;
                }
                if result == 0 {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "IPC connect deadline"));
                }
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::Interrupted {
                    return stage("connect poll", Err(e));
                }
            }
            let mut error = 0i32;
            let mut len = 4u32;
            if unsafe { getsockopt(fd, sol, soerror, (&mut error as *mut i32).cast(), &mut len) } < 0 {
                return stage("get connect error", Err(io::Error::last_os_error()));
            }
            if error != 0 {
                return stage("pending connect error", Err(io::Error::from_raw_os_error(error)));
            }
        }
        Ok(stream)
    }
    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn ipc_reads_queued_ack_after_peer_disconnects() {
            let (mut client, mut peer) = UnixStream::pair().unwrap();
            client.set_nonblocking(true).unwrap();
            peer.write_all(&[1]).unwrap();
            drop(peer);
            let mut ack = [0];
            read_deadline(&mut client, &mut ack, Instant::now() + Duration::from_secs(2)).unwrap();
            assert_eq!(ack, [1]);
            assert_eq!(
                read_deadline(&mut client, &mut ack, Instant::now() + Duration::from_secs(2)).unwrap_err().kind(),
                io::ErrorKind::UnexpectedEof
            );
        }

        #[test]
        fn ipc_read_waits_only_until_its_absolute_deadline() {
            let (mut client, _peer) = UnixStream::pair().unwrap();
            client.set_nonblocking(true).unwrap();
            let mut ack = [0];
            assert_eq!(
                read_deadline(&mut client, &mut ack, Instant::now() + Duration::from_millis(10)).unwrap_err().kind(),
                io::ErrorKind::TimedOut
            );
        }
    }
}
