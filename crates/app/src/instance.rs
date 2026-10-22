//! Startup ownership and a bounded bridge from local IPC to either frontend.
//! Socket/pipe I/O happens before the event loop or on the dedicated IPC workers.
use kwil_core_workspace::instance::{InstanceEndpoint, InstanceServer, OpenFiles, send_files};
use kwil_view_model::document_worker::ExternalOpen;
use std::{
    cell::RefCell,
    io,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, TrySendError},
    },
    time::Duration,
};

const CONNECT_TIMEOUT: Duration = Duration::from_millis(250);

pub enum Start {
    Forwarded,
    Primary(Owner),
}

pub fn endpoint() -> io::Result<InstanceEndpoint> {
    let namespace = match std::env::var_os("KWIL_INSTANCE_NAMESPACE") {
        Some(value) => value.into_string().map_err(|_| io::Error::other("Invalid instance namespace"))?,
        None => "kwil".into(),
    };
    #[cfg(windows)]
    let runtime_directory = PathBuf::new(); // Named pipes use the current token SID.
    #[cfg(not(windows))]
    let runtime_directory = runtime_root(
        std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )?;
    Ok(InstanceEndpoint { runtime_directory, namespace })
}

#[cfg(any(not(windows), test))]
fn runtime_root(runtime: Option<PathBuf>, home: Option<PathBuf>) -> io::Result<PathBuf> {
    // A malformed explicit runtime path must not select another user's profile.
    let root = runtime.or(home).ok_or_else(|| io::Error::other("No private instance directory available"))?;
    if !root.is_absolute() {
        return Err(io::Error::other("Instance directory must be absolute"));
    }
    Ok(root)
}

pub fn try_forward(endpoint: &InstanceEndpoint, files: &OpenFiles) -> io::Result<bool> {
    match send_files(endpoint, files, CONNECT_TIMEOUT) {
        Ok(()) => Ok(true),
        Err(error) if absent_owner(&error) => Ok(false),
        // A timeout, refused admission, or failed authentication never permits
        // deleting the endpoint or opening a competing editor instance.
        Err(error) => Err(error),
    }
}

fn absent_owner(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused)
}

/// The caller has not entered a UI event loop yet. Concurrent first launches
/// race on the backend's OS ownership primitive, never on file existence.
pub fn claim(endpoint: InstanceEndpoint, files: &OpenFiles) -> io::Result<Start> {
    let server = InstanceServer::spawn(endpoint.clone())?;
    match server.ready.recv_timeout(Duration::from_millis(500)) {
        Ok(Ok(())) => Ok(Start::Primary(Owner::new(server))),
        Ok(Err(error)) => {
            server.shutdown();
            // Another first launch may have won between connect and bind.
            // The peer is still authenticated by send_files.
            if send_files(&endpoint, files, CONNECT_TIMEOUT).is_ok() { Ok(Start::Forwarded) } else { Err(error) }
        }
        Err(error) => {
            server.shutdown();
            Err(io::Error::new(io::ErrorKind::TimedOut, format!("Instance initialization: {error}")))
        }
    }
}

type Route = Arc<Mutex<Option<ExternalOpen>>>;

pub struct Owner {
    server: InstanceServer,
    requests: Rc<RefCell<Option<Receiver<OpenFiles>>>>,
    route: Route,
    stopped: Arc<AtomicBool>,
}
impl Owner {
    fn new(mut server: InstanceServer) -> Self {
        let (_, empty) = std::sync::mpsc::channel();
        let requests = std::mem::replace(&mut server.requests, empty);
        Self {
            server,
            requests: Rc::new(RefCell::new(Some(requests))),
            route: Arc::new(Mutex::new(None)),
            stopped: Arc::new(AtomicBool::new(false)),
        }
    }

    /// One relay is started after the first frame. GUI-to-TUI fallback updates
    /// its destination, rather than creating another IPC worker or queue.
    pub fn ready(&self, initial: OpenFiles) -> Box<dyn FnOnce(ExternalOpen)> {
        let requests = self.requests.clone();
        let route = self.route.clone();
        let stopped = self.stopped.clone();
        Box::new(move |open| {
            if !initial.files.is_empty()
                && let Err(error) = open(initial)
            {
                eprintln!("kwil: initial file delivery failed: {error}");
            }
            *route.lock().unwrap_or_else(|error| error.into_inner()) = Some(open);
            if let Some(requests) = requests.borrow_mut().take() {
                let result = std::thread::Builder::new().name("kwil-open-relay".into()).spawn(move || {
                    for request in requests {
                        if !deliver(&route, &stopped, request) {
                            break;
                        }
                    }
                });
                if let Err(error) = result {
                    eprintln!("kwil: cannot start file-open relay: {error}");
                }
            }
        })
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        self.server.shutdown();
        // Backend shutdown drops the request sender and wakes the blocking relay.
        // No event-loop thread waits on a filesystem operation or thread join.
    }
}

fn deliver(route: &Route, stopped: &AtomicBool, mut request: OpenFiles) -> bool {
    loop {
        if stopped.load(Ordering::Acquire) {
            return false;
        }
        let result = {
            let route = route.lock().unwrap_or_else(|error| error.into_inner());
            match route.as_ref() {
                Some(open) => open(request),
                None => return false,
            }
        };
        match result {
            Ok(()) => return true,
            Err(TrySendError::Full(retained)) => {
                request = retained;
                // Only while an accepted action is awaiting bounded capacity.
                // At idle this worker blocks on recv and has zero timer wakeups.
                std::thread::park_timeout(Duration::from_millis(2));
            }
            Err(TrySendError::Disconnected(_)) => {
                eprintln!("kwil: editor closed before a queued file-open request could be delivered");
                return false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn instance_errors_never_treat_busy_or_foreign_owners_as_absent() {
        for kind in [
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::TimedOut,
            io::ErrorKind::WouldBlock,
            io::ErrorKind::AddrInUse,
            io::ErrorKind::InvalidData,
        ] {
            assert!(!absent_owner(&io::Error::from(kind)));
        }
        assert!(absent_owner(&io::Error::from(io::ErrorKind::NotFound)));
        assert!(absent_owner(&io::Error::from(io::ErrorKind::ConnectionRefused)));
    }

    #[test]
    fn instance_delivery_retries_the_exact_bounded_request_and_stops_on_close() {
        use std::sync::atomic::AtomicUsize;
        let attempts = Arc::new(AtomicUsize::new(0));
        let count = attempts.clone();
        let expected = OpenFiles {
            files: vec![kwil_core_workspace::instance::OpenFile {
                path: std::env::temp_dir().join("native file"),
                line: Some(9),
                column: Some(2),
            }],
        };
        let original = expected.clone();
        let open: ExternalOpen = Box::new(move |request| {
            assert_eq!(request, original);
            if count.fetch_add(1, Ordering::SeqCst) == 0 { Err(TrySendError::Full(request)) } else { Ok(()) }
        });
        let route = Arc::new(Mutex::new(Some(open)));
        let stopped = AtomicBool::new(false);
        assert!(deliver(&route, &stopped, expected.clone()));
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        stopped.store(true, Ordering::Release);
        assert!(!deliver(&route, &stopped, expected));
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn instance_runtime_override_does_not_fall_back_when_malformed() {
        let home = std::env::temp_dir();
        assert!(runtime_root(Some("relative".into()), Some(home.clone())).is_err());
        assert!(runtime_root(Some(PathBuf::new()), Some(home.clone())).is_err());
        assert_eq!(runtime_root(None, Some(home.clone())).unwrap(), home);
        assert!(runtime_root(None, None).is_err());
    }
}
