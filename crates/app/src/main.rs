//! Binary: argument parsing, picks a frontend, wires threads.

use kwil_core_text::TextFile;
use kwil_view_model::{Notify, View};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const HELP: &str = "\
Kwil - a blazing fast plain text editor

Usage: kwil [options] [file]

Options:
  --gui        Force the window UI
  --tui        Force the terminal UI
  --trace-startup  Trace process-age startup marks to stderr, then exit after indexing
  --update     Check for an update now and install it
  --version    Print the version
  -h, --help   Print this help

Keys: arrows, PageUp/PageDown, Ctrl+Home/End to move; Ctrl+Q to quit (Esc in the terminal UI).
Auto-update: checked once a day after start-up; set KWIL_NO_UPDATE=1 or put a
kwil.no-update file next to the executable to turn it off.";

#[derive(Default)]
struct Args {
    gui: bool,
    tui: bool,
    hidden: bool,
    trace_startup: bool,
    #[cfg(feature = "frame-harness")]
    frame_script: bool,
    update: bool,
    file: Option<PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args::default();
    for arg in std::env::args_os().skip(1) {
        match arg.to_str() {
            Some("--gui") => a.gui = true,
            Some("--tui") => a.tui = true,
            // Agent and test runs: window composed but never shown (Windows).
            Some("--hidden") => a.hidden = true,
            Some("--trace-startup") => a.trace_startup = true,
            #[cfg(feature = "frame-harness")]
            Some("--frame-script") => a.frame_script = true,
            Some("--update") => a.update = true,
            Some("--version" | "-V") => {
                println!("kwil {VERSION}");
                std::process::exit(0);
            }
            Some("--help" | "-h") => {
                println!("{HELP}");
                std::process::exit(0);
            }
            Some(s) if s.starts_with("--") => return Err(format!("unknown option {s}")),
            _ if a.file.is_none() => a.file = Some(PathBuf::from(arg)),
            _ => return Err("only one file can be opened for now".into()),
        }
    }
    if a.gui && a.tui {
        return Err("--gui and --tui exclude each other".into());
    }
    Ok(a)
}

/// Spec §6: GUI when a desktop is available, TUI when not. The check is a
/// few environment lookups, well under 1 ms.
fn desktop_available() -> bool {
    let has = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
    let over_ssh = has("SSH_CONNECTION") || has("SSH_TTY");
    if cfg!(any(target_os = "linux", target_os = "freebsd", target_os = "openbsd", target_os = "netbsd")) {
        // X forwarding over SSH sets DISPLAY, which counts as a desktop.
        has("WAYLAND_DISPLAY") || has("DISPLAY")
    } else {
        !over_ssh
    }
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("kwil: {e}\n\n{HELP}");
            return ExitCode::from(2);
        }
    };
    if args.update {
        return match kwil_update::disabled_reason(VERSION) {
            Some(why) => {
                eprintln!("kwil: updates are off ({why})");
                ExitCode::FAILURE
            }
            None => match kwil_update::check_and_install(VERSION) {
                Ok(kwil_update::Outcome::UpToDate) => {
                    println!("kwil {VERSION} is up to date");
                    ExitCode::SUCCESS
                }
                Ok(kwil_update::Outcome::Installed(v)) => {
                    println!("Updated to {v}; restart Kwil to use it");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("kwil: update failed: {e}");
                    ExitCode::FAILURE
                }
            },
        };
    }

    let trace = args.trace_startup.then(|| std::sync::Arc::new(kwil_bench::startup::Trace::launch()));

    let open = |path: &PathBuf| match TextFile::open(path) {
        Ok(f) => Ok(View::new(Some((path.clone(), f)))),
        Err(e) => Err(format!("cannot open {}: {e}", path.display())),
    };
    let make_view = || match &args.file {
        Some(p) => open(p),
        None => Ok(View::new(None)),
    };
    let on_first_frame = || -> Box<dyn FnOnce(Notify)> {
        let measuring = args.trace_startup;
        Box::new(move |notify: Notify| {
            if !measuring {
                kwil_update::spawn_background(VERSION, notify);
            }
        })
    };
    let in_terminal = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();

    let use_gui = args.gui || (!args.tui && desktop_available());
    if use_gui {
        let view = match make_view() {
            Ok(v) => v,
            Err(e) => {
                eprintln!("kwil: {e}");
                return ExitCode::FAILURE;
            }
        };
        if !args.trace_startup {
            detach_console_if_own();
        }
        let opts = kwil_ui_gpu::GuiOptions {
            hidden: args.hidden,
            trace: trace.clone(),
            #[cfg(feature = "frame-harness")]
            frame_script: args.frame_script,
        };
        match kwil_ui_gpu::run(view, opts, on_first_frame()) {
            Ok(()) => return ExitCode::SUCCESS,
            // Spec §6: fall back to the TUI when the window fails and a
            // terminal is attached.
            Err(e) if !args.gui && in_terminal => eprintln!("kwil: {e}; using the terminal UI"),
            Err(e) => {
                eprintln!("kwil: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
    if !in_terminal {
        eprintln!("kwil: the terminal UI needs an interactive terminal");
        return ExitCode::FAILURE;
    }
    let view = match make_view() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("kwil: {e}");
            return ExitCode::FAILURE;
        }
    };
    match kwil_ui_tui::run_traced(view, on_first_frame(), trace) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kwil: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Windows: the binary is a console program so the TUI works in a shell.
/// Started from Explorer, Windows gives it a console of its own; drop that
/// console when the GUI runs. A console shared with a shell stays.
/// (KW-13 decides the final launcher shape.)
#[cfg(windows)]
fn detach_console_if_own() {
    use windows_sys::Win32::System::Console::{FreeConsole, GetConsoleProcessList};
    let mut pids = [0u32; 2];
    let n = unsafe { GetConsoleProcessList(pids.as_mut_ptr(), 2) };
    if n == 1 {
        unsafe { FreeConsole() };
    }
}

#[cfg(not(windows))]
fn detach_console_if_own() {}
