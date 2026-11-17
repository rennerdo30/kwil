//! `kwil --wait` end to end: the real launcher binary against a real instance server,
//! all in a private test namespace and scratch profile. The launcher is hidden and
//! never starts an editor (`KWIL_NO_DETACHED_EDITOR`); here the test is the editor and
//! "closes the tab" by answering over the instance IPC. Every process is one this test
//! started, every wait has a limit, and a process that outlives it is killed by PID.
use kwil_core_workspace::instance::{InstanceEndpoint, InstanceServer, WaitOutcome};
use std::{
    io::{BufRead, BufReader, Read},
    path::PathBuf,
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

const LIMIT: Duration = Duration::from_secs(20);

struct Scratch {
    root: PathBuf,
    namespace: String,
}
impl Scratch {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!("kwil-kw90-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["profile", "local", "workspace", "runtime"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        Self { root, namespace: format!("kwil-test-{}-{tag}", std::process::id()) }
    }
    fn endpoint(&self) -> InstanceEndpoint {
        InstanceEndpoint { runtime_directory: self.root.join("runtime"), namespace: self.namespace.clone() }
    }
    fn file(&self) -> PathBuf {
        let path = self.root.join("COMMIT_EDITMSG");
        std::fs::write(&path, "message\n").unwrap();
        path
    }
    fn isolate(&self, command: &mut Command) {
        command
            .env("KWIL_INSTANCE_NAMESPACE", &self.namespace)
            .env("KWIL_NO_UPDATE", "1")
            .env("KWIL_NO_DETACHED_EDITOR", "1")
            .env("KWIL_WORKSPACE_ROOT", self.root.join("workspace"))
            .env("APPDATA", self.root.join("profile"))
            .env("LOCALAPPDATA", self.root.join("local"))
            .env("XDG_RUNTIME_DIR", self.root.join("runtime"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }
    }
    fn launcher(&self, arguments: &[&str]) -> Child {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kwil"));
        command.args(arguments);
        self.isolate(&mut command);
        let child = command.spawn().unwrap();
        eprintln!("kwil launcher PID {}: {arguments:?}", child.id());
        child
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn server(scratch: &Scratch) -> InstanceServer {
    let server = InstanceServer::spawn(scratch.endpoint()).unwrap();
    server.ready.recv_timeout(LIMIT).unwrap().unwrap();
    server
}

fn exit_within(child: &mut Child, limit: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}
/// A launcher that does not return in time is killed (our own child) and fails the test.
fn finished(child: &mut Child) -> (ExitStatus, String) {
    let Some(status) = exit_within(child, LIMIT) else {
        let pid = child.id();
        let _ = child.kill();
        let _ = child.wait();
        panic!("launcher PID {pid} did not return within {LIMIT:?}");
    };
    let mut stderr = String::new();
    let _ = child.stderr.take().unwrap().read_to_string(&mut stderr);
    (status, stderr)
}

#[test]
fn kw90_launcher_returns_only_after_the_tab_is_closed() {
    let scratch = Scratch::new("close");
    let server = server(&scratch);
    let file = scratch.file();
    let mut launcher = scratch.launcher(&["--hidden", "--wait", file.to_str().unwrap()]);
    let request = server.requests.recv_timeout(LIMIT).expect("the launcher hands the file to the editor");
    assert_eq!(request.files[0].path, file);
    let token = request.wait.expect("a waiting launcher sends its token");
    assert!(
        exit_within(&mut launcher, Duration::from_millis(1500)).is_none(),
        "the tab is open: Git must keep waiting"
    );
    server.waits.complete(token, WaitOutcome::Closed);
    let (status, stderr) = finished(&mut launcher);
    assert_eq!(status.code(), Some(0), "{stderr}");
}

#[test]
fn kw90_launcher_reports_a_file_that_could_not_be_opened() {
    let scratch = Scratch::new("failed");
    let server = server(&scratch);
    let file = scratch.file();
    let mut launcher = scratch.launcher(&["--hidden", "--wait", file.to_str().unwrap()]);
    let token = server.requests.recv_timeout(LIMIT).unwrap().wait.unwrap();
    server.waits.complete(token, WaitOutcome::OpenFailed);
    let (status, stderr) = finished(&mut launcher);
    assert_eq!(status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("could not be opened"), "{stderr}");
}

#[test]
fn kw90_launcher_returns_with_success_when_the_editor_quits_normally() {
    let scratch = Scratch::new("quit");
    let server = server(&scratch);
    let file = scratch.file();
    let mut launcher = scratch.launcher(&["--hidden", "--wait", file.to_str().unwrap()]);
    server.requests.recv_timeout(LIMIT).unwrap();
    assert!(exit_within(&mut launcher, Duration::from_millis(500)).is_none());
    server.shutdown();
    assert!(server.waits.drain(LIMIT));
    let (status, stderr) = finished(&mut launcher);
    assert_eq!(status.code(), Some(0), "{stderr}");
}

/// Runs only when the crash test starts it as its own child: a primary that accepts a
/// waiting launcher and then sits there until the parent kills it.
#[test]
fn kw90_helper_primary() {
    let (Some(namespace), Some(runtime)) =
        (std::env::var_os("KW90_HELPER_NAMESPACE"), std::env::var_os("KW90_HELPER_RUNTIME"))
    else {
        return;
    };
    let endpoint =
        InstanceEndpoint { runtime_directory: PathBuf::from(runtime), namespace: namespace.into_string().unwrap() };
    let server = InstanceServer::spawn(endpoint).unwrap();
    server.ready.recv_timeout(LIMIT).unwrap().unwrap();
    println!("kw90 ready");
    server.requests.recv_timeout(Duration::from_secs(60)).unwrap();
    println!("kw90 request");
    std::thread::sleep(Duration::from_secs(60));
}

#[test]
fn kw90_launcher_returns_with_its_own_code_when_the_editor_crashes() {
    let scratch = Scratch::new("crash");
    let mut helper = Command::new(std::env::current_exe().unwrap());
    helper
        .args(["kw90_helper_primary", "--exact", "--nocapture", "--test-threads=1"])
        .env("KW90_HELPER_NAMESPACE", &scratch.namespace)
        .env("KW90_HELPER_RUNTIME", scratch.root.join("runtime"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        helper.creation_flags(0x08000000);
    }
    let mut helper = helper.spawn().unwrap();
    eprintln!("kw90 helper primary PID {}", helper.id());
    let (lines, received) = mpsc::channel();
    let output = helper.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(output).lines().map_while(Result::ok) {
            if lines.send(line).is_err() {
                break;
            }
        }
    });
    let expect = |text: &str| {
        let deadline = Instant::now() + LIMIT;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            if received.recv_timeout(left).is_ok_and(|line| line.trim() == text) {
                return;
            }
        }
        panic!("the helper primary never said {text:?}");
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        expect("kw90 ready");
        let file = scratch.file();
        let mut launcher = scratch.launcher(&["--hidden", "--wait", file.to_str().unwrap()]);
        expect("kw90 request");
        assert!(exit_within(&mut launcher, Duration::from_millis(500)).is_none());
        // The editor dies without answering: no outcome byte, only a dropped connection.
        helper.kill().unwrap();
        let (status, stderr) = finished(&mut launcher);
        assert_eq!(status.code(), Some(3), "{stderr}");
        assert!(stderr.contains("exited before the file was closed"), "{stderr}");
    }));
    let _ = helper.kill();
    let _ = helper.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[test]
fn kw90_launcher_without_wait_still_hands_over_with_the_original_frame_and_returns_at_once() {
    let scratch = Scratch::new("plain");
    let server = server(&scratch);
    let file = scratch.file();
    let mut launcher = scratch.launcher(&["--hidden", file.to_str().unwrap()]);
    let (status, stderr) = finished(&mut launcher);
    assert_eq!(status.code(), Some(0), "{stderr}");
    let request = server.requests.recv_timeout(LIMIT).unwrap();
    assert_eq!((request.files[0].path.clone(), request.wait), (file, None));
}

#[test]
fn kw90_wait_without_a_file_or_without_an_editor_fails_with_a_clear_message() {
    let scratch = Scratch::new("errors");
    let mut no_file = scratch.launcher(&["--hidden", "--wait"]);
    let (status, stderr) = finished(&mut no_file);
    assert_eq!(status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("--wait needs at least one file"), "{stderr}");
    // No editor runs and none may be started: fail fast instead of waiting for nothing.
    let file = scratch.file();
    let mut no_editor = scratch.launcher(&["--hidden", "--wait", file.to_str().unwrap()]);
    let (status, stderr) = finished(&mut no_editor);
    assert_eq!(status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("no running instance"), "{stderr}");
}
