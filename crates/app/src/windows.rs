//! Windows console attachment and the console-to-GUI startup handshake.
use std::io::{BufRead, Read, Write};
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
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle};
    use windows_sys::Win32::System::Console::{FreeConsole, GetConsoleCP, GetStdHandle, STD_OUTPUT_HANDLE};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // Keep a private, non-inheritable copy of the startup IPC while releasing
    // the process's standard handles and console. Only GUI readiness calls this.
    let mut ipc = if gui_child() {
        let mut handle = std::ptr::null_mut();
        let process = unsafe { GetCurrentProcess() };
        if unsafe {
            DuplicateHandle(process, GetStdHandle(STD_OUTPUT_HANDLE), process, &mut handle, 0, 0, DUPLICATE_SAME_ACCESS)
        } == 0
        {
            report_gui_failure(&std::io::Error::last_os_error());
            std::process::exit(75);
        }
        Some(std::fs::File::from(unsafe { OwnedHandle::from_raw_handle(handle.cast()) }))
    } else {
        None
    };
    if unsafe { GetConsoleCP() } != 0 && unsafe { FreeConsole() } == 0 {
        let error = std::io::Error::last_os_error();
        if let Some(out) = ipc.as_mut() {
            let _ = writeln!(out, "cannot detach GUI console: {error}");
        } else {
            eprintln!("kwil: cannot detach GUI console: {error}");
        }
        std::process::exit(75);
    }
    release_standard_handles();
    if let Some(out) = ipc.as_mut() {
        let _ = writeln!(out, "kwil: GUI ready");
        let _ = out.flush();
    }
}

fn release_standard_handles() {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
    };
    let kinds = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE];
    let handles = kinds.map(|kind| unsafe { GetStdHandle(kind) });
    for (i, kind) in kinds.into_iter().enumerate() {
        unsafe { SetStdHandle(kind, std::ptr::null_mut()) };
        let handle = handles[i];
        if !handle.is_null() && handle != INVALID_HANDLE_VALUE && !handles[..i].contains(&handle) {
            unsafe { CloseHandle(handle) };
        }
    }
}

fn protect_shell_handles() -> Result<(), String> {
    use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation};
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
    // The launcher's own shell pipes may themselves be inheritable. Clearing
    // those flags prevents extra inherited references beyond the assigned IPC
    // stdio. Command creates the child-specific handles below independently.
    for kind in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        let handle = unsafe { GetStdHandle(kind) };
        if !handle.is_null()
            && handle != INVALID_HANDLE_VALUE
            && unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } == 0
        {
            return Err(format!("cannot protect shell handle: {}", std::io::Error::last_os_error()));
        }
    }
    Ok(())
}

/// Wait for first-frame acknowledgement, not the lifetime of the editor.
/// On startup failure the console process still owns the terminal and can
/// fall back without racing the shell for terminal input.
pub fn launch_gui() -> Result<ExitCode, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?.with_file_name("kwil.exe");
    protect_shell_handles()?;
    let mut child = Command::new(&exe)
        .args(std::env::args_os().skip(1))
        .env("KWIL_CONSOLE_LAUNCH", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", exe.display()))?;
    let pid = child.id();
    #[cfg(debug_assertions)]
    if std::env::var_os("KWIL_TRACE_LAUNCH").is_some() {
        eprintln!("kwil: GUI PID {pid}");
    }
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let (error_send, errors) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.take(65536).read_to_string(&mut text);
        let _ = error_send.send(text);
    });
    let (send, recv) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let result = std::io::BufReader::new(stdout).take(65536).read_line(&mut line).map(|_| line);
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
                if let Ok(text) = errors.recv_timeout(std::time::Duration::from_secs(1))
                    && !text.trim().is_empty()
                {
                    eprintln!("{}", text.trim_end());
                }
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

pub fn standard_streams() -> crate::startup::StandardStreams {
    use crate::startup::StandardStreams;
    use std::io::IsTerminal;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
    if std::io::stderr().is_terminal() {
        StandardStreams::Terminal
    } else if [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE].into_iter().any(|kind| {
        let handle = unsafe { GetStdHandle(kind) };
        !handle.is_null() && handle != INVALID_HANDLE_VALUE
    }) {
        StandardStreams::Redirected
    } else {
        StandardStreams::Absent
    }
}

pub fn show_error(message: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    let message: Vec<u16> = message.encode_utf16().chain(Some(0)).collect();
    let title: Vec<u16> = "Kwil".encode_utf16().chain(Some(0)).collect();
    unsafe { MessageBoxW(std::ptr::null_mut(), message.as_ptr(), title.as_ptr(), MB_OK | MB_ICONERROR) };
}
