//! Worker-only, bounded local IPC. Paths use native Unix bytes / Windows UTF-16.
//! No bearer secret or credential files: peer identity is checked by the OS.
//! The app resolves relative CLI paths before forwarding. Position numbers are 1-based.
use crate::session::{Decoder, invalid, put_path, put32, put64};
use std::{
    io,
    path::PathBuf,
    sync::mpsc::{self, Receiver},
    thread,
    time::Duration,
};
pub const MAX_OPEN_FILES: usize = 128;
pub const MAX_IPC_BYTES: usize = 64 * 1024;
const IO_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenFile {
    pub path: PathBuf,
    pub line: Option<u64>,
    pub column: Option<u64>,
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OpenFiles {
    pub files: Vec<OpenFile>,
}
impl OpenFiles {
    pub fn encode(&self) -> io::Result<Vec<u8>> {
        if self.files.len() > MAX_OPEN_FILES {
            return Err(invalid("IPC file count"));
        }
        let mut b = b"KWI1".to_vec();
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
        if r.take(4)? != b"KWI1" {
            return Err(invalid("IPC version"));
        }
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
        Ok(Self { files })
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
}
impl InstanceServer {
    /// Spawns without opening disk/endpoints on the caller. Bind result comes via ready.
    pub fn spawn(endpoint: InstanceEndpoint) -> io::Result<Self> {
        endpoint.validate()?;
        let (stop, signal) = platform::stop_pair()?;
        let (ready_tx, ready) = mpsc::sync_channel(1);
        let (tx, requests) = mpsc::sync_channel(16);
        let (done_tx, finished) = mpsc::sync_channel(1);
        thread::Builder::new().name("kwil-instance".into()).spawn(move || {
            let result = platform::serve(endpoint, signal, ready_tx, tx);
            let _ = done_tx.try_send(result);
        })?;
        Ok(Self { stop, ready, requests, finished })
    }
    /// Wakes the blocking accept, cancels pending client I/O and releases only our endpoint.
    /// Completion is reported by finished; caller chooses a shutdown timeout off the UI.
    pub fn shutdown(&self) {
        self.stop.signal();
    }
}
impl Drop for InstanceServer {
    fn drop(&mut self) {
        self.stop.signal();
    }
}
/// Worker-only. Acknowledgement means queued at the receiver; the frontend still must
/// consume `requests`, wake its event loop and open files. Timeout is not permission to
/// delete an endpoint/start a conflicting instance. Bind uses OS ownership separately.
pub fn send_files(endpoint: &InstanceEndpoint, request: &OpenFiles, timeout: Duration) -> io::Result<()> {
    endpoint.validate()?;
    let b = request.encode()?;
    platform::send(endpoint, &b, timeout)
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
    pub fn serve(
        e: InstanceEndpoint,
        stop: Signal,
        ready: SyncSender<io::Result<()>>,
        tx: SyncSender<OpenFiles>,
    ) -> io::Result<()> {
        let init = (|| {
            let user = User::current()?;
            let name = name(&e, &user)?;
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
            let pipe = Handle::new(unsafe {
                CreateNamedPipeW(
                    name.as_ptr(),
                    PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                    1,
                    MAX_IPC_BYTES as u32 + 4,
                    MAX_IPC_BYTES as u32 + 4,
                    250,
                    &attrs,
                )
            })?;
            Ok((pipe, user))
        })();
        let (pipe, user) = match init {
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
            let _ = (|| {
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
                let accepted = tx.try_send(request).is_ok();
                write(pipe.0, stop.0.0, &[u8::from(accepted)], deadline)?;
                // Named-pipe disconnect discards unread output. Wait for the
                // client's receipt under the same deadline, never FlushFileBuffers.
                let mut received = [0];
                read(pipe.0, stop.0.0, &mut received, deadline)
            })();
            unsafe {
                DisconnectNamedPipe(pipe.0);
            }
        }
    }
    pub fn send(e: &InstanceEndpoint, b: &[u8], timeout: Duration) -> io::Result<()> {
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
        Ok(())
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
    pub fn serve(
        e: InstanceEndpoint,
        stop: UnixStream,
        ready: SyncSender<io::Result<()>>,
        tx: SyncSender<OpenFiles>,
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
            let deadline = Instant::now() + IO_TIMEOUT;
            let _ = (|| {
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
                let accepted = tx.try_send(request).is_ok();
                write_deadline(&mut stream, &[u8::from(accepted)], deadline)?;
                Ok::<_, io::Error>(())
            })();
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
    pub fn send(e: &InstanceEndpoint, b: &[u8], timeout: Duration) -> io::Result<()> {
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
        Ok(())
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
            stage("set read timeout", s.set_read_timeout(Some(remaining(deadline)?)))?;
            let n = stage("read", s.read(&mut b[p..]))?;
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
            stage("set write timeout", s.set_write_timeout(Some(remaining(deadline)?)))?;
            let n = stage("write", s.write(&b[p..]))?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "IPC closed"));
            }
            p += n;
        }
        Ok(())
    }
    // A blocking UnixStream::connect can hang on a full accept backlog. Create
    // nonblocking, wait only to the caller's deadline, then restore blocking I/O.
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
        stage("restore blocking", stream.set_nonblocking(false))?;
        Ok(stream)
    }
}
