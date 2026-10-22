//! CLI paths must work without a display or a terminal.
use std::process::{Command, Stdio};

fn output(binary: &str, arg: &str) -> std::process::Output {
    let mut command = Command::new(binary);
    command.arg(arg).env("KWIL_NO_UPDATE", "1").stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW, including the console entry point.
    }
    #[allow(unused_mut)]
    let mut child = command.spawn().unwrap();
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::Threading::WaitForSingleObject;
        if unsafe { WaitForSingleObject(child.as_raw_handle().cast(), 10000) } != 0 {
            let pid = child.id();
            let _ = child.kill();
            let _ = child.wait();
            panic!("CLI PID {pid} exceeded timeout");
        }
        child.wait_with_output().unwrap()
    }
    #[cfg(not(windows))]
    {
        let pid = child.id();
        let (send, recv) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = send.send(child.wait_with_output());
        });
        // Bounded CLI checks. Tests never launch a window.
        match recv.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(result) => result.unwrap(),
            Err(e) => {
                unsafe extern "C" {
                    fn kill(pid: i32, signal: i32) -> i32;
                }
                // Only the child PID recorded from this test's spawn.
                unsafe { kill(pid as i32, 9) };
                let _ = recv.recv_timeout(std::time::Duration::from_secs(5));
                panic!("CLI PID {pid} exceeded timeout: {e}");
            }
        }
    }
}
fn version(binary: &str) {
    let out = output(binary, "--version");
    assert!(out.status.success(), "{:?}", out);
    assert_eq!(String::from_utf8(out.stdout).unwrap().trim(), format!("kwil {}", env!("CARGO_PKG_VERSION")));
}
fn help(binary: &str) {
    let out = output(binary, "--help");
    assert!(out.status.success(), "{:?}", out);
    assert!(String::from_utf8(out.stdout).unwrap().contains("Usage: kwil [options] [file[:line[:column]] ...]"));
}
#[test]
fn gui_version() {
    version(env!("CARGO_BIN_EXE_kwil"));
}
#[test]
fn gui_help() {
    help(env!("CARGO_BIN_EXE_kwil"));
}
#[cfg(all(windows, feature = "windows-console"))]
#[test]
fn console_version() {
    version(env!("CARGO_BIN_EXE_kwil-console"));
}
#[cfg(all(windows, feature = "windows-console"))]
#[test]
fn console_help() {
    help(env!("CARGO_BIN_EXE_kwil-console"));
}
