//! Windows console attachment and the console-to-GUI startup handshake.
use std::io::{BufRead, Write};
use std::process::{Command, ExitCode, Stdio};

pub fn attach_parent_console() {
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    // Never allocate a console: Explorer launches must not flash a window.
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

pub fn gui_child() -> bool {
    std::env::var_os("KWIL_CONSOLE_LAUNCH").is_some()
}

pub fn first_frame() {
    if gui_child() {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "kwil: GUI ready");
        let _ = out.flush();
    }
}

/// Wait for first-frame acknowledgement, not the lifetime of the editor.
/// On startup failure the console process still owns the terminal and can
/// fall back without racing the shell for terminal input.
pub fn launch_gui() -> Result<ExitCode, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?.with_file_name("kwil.exe");
    let mut child = Command::new(&exe)
        .args(std::env::args_os().skip(1))
        .env("KWIL_CONSOLE_LAUNCH", "1")
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", exe.display()))?;
    let pid = child.id();
    #[cfg(debug_assertions)]
    if std::env::var_os("KWIL_TRACE_LAUNCH").is_some() {
        eprintln!("kwil: GUI PID {pid}");
    }
    let stdout = child.stdout.take().unwrap();
    let (send, recv) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let result = std::io::BufReader::new(stdout).read_line(&mut line).map(|_| line);
        let _ = send.send(result);
    });
    match recv.recv_timeout(std::time::Duration::from_secs(15)) {
        Ok(Ok(line)) if line == "kwil: GUI ready\n" => Ok(ExitCode::SUCCESS),
        Ok(result) => {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::System::Threading::WaitForSingleObject;
            // A malformed acknowledgement must not turn this into an unbounded wait.
            if unsafe { WaitForSingleObject(child.as_raw_handle().cast(), 1000) } != 0 {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("GUI startup failed (PID {pid})"));
            }
            let status = child.wait().map_err(|e| e.to_string())?;
            if status.code() == Some(75) {
                Err(result
                    .ok()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "GUI initialization failed".into())
                    .trim()
                    .to_owned())
            } else {
                Ok(ExitCode::from(status.code().unwrap_or(1).clamp(1, 255) as u8))
            }
        }
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(format!("GUI startup timed out (PID {pid}): {e}"))
        }
    }
}

pub fn report_gui_failure(error: &impl std::fmt::Display) {
    // The launcher owns fallback and emits the one-line notice.
    let _ = writeln!(std::io::stdout(), "{error}");
}

pub fn show_error(message: &str, hidden: bool) {
    use std::io::IsTerminal;
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    if hidden || gui_child() || std::io::stderr().is_terminal() {
        return;
    }
    let message: Vec<u16> = message.encode_utf16().chain(Some(0)).collect();
    let title: Vec<u16> = "Kwil".encode_utf16().chain(Some(0)).collect();
    unsafe { MessageBoxW(std::ptr::null_mut(), message.as_ptr(), title.as_ptr(), MB_OK | MB_ICONERROR) };
}
