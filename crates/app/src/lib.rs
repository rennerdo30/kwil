//! Shared application used by the GUI and console entry points.

pub mod frontend;
#[cfg(windows)]
mod windows;
use frontend::{Flags, Frontend, ProcessEnv, after_gui_failure, choose_frontend, current_platform};

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

pub fn run() -> ExitCode {
    #[cfg(windows)]
    windows::attach_parent_console();
    run_app(false)
}

pub fn run_console() -> ExitCode {
    run_app(true)
}

fn run_app(console_launcher: bool) -> ExitCode {
    #[cfg(not(windows))]
    let _ = console_launcher;
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

    let open = |path: &PathBuf| match TextFile::open(path) {
        Ok(f) => Ok(View::new(Some((path.clone(), f)))),
        Err(e) => Err(format!("cannot open {}: {e}", path.display())),
    };
    let make_view = || match &args.file {
        Some(p) => open(p),
        None => Ok(View::new(None)),
    };
    let on_first_frame = || -> Box<dyn FnOnce(Notify)> {
        Box::new(|notify: Notify| {
            #[cfg(windows)]
            windows::first_frame();
            kwil_update::spawn_background(VERSION, notify);
        })
    };
    let in_terminal = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();

    let frontend = choose_frontend(&ProcessEnv, Flags { gui: args.gui, tui: args.tui }, current_platform());
    if let Frontend::Error(e) = frontend {
        eprintln!("kwil: {e}");
        return ExitCode::from(2);
    }
    #[allow(unused_mut)]
    let mut use_gui = frontend == Frontend::Gui;
    #[cfg(windows)]
    if console_launcher && use_gui {
        match windows::launch_gui() {
            Ok(code) => return code,
            Err(e) if after_gui_failure(in_terminal) == Frontend::Tui => {
                eprintln!("kwil: {e}; using the terminal UI");
                use_gui = false;
            }
            Err(e) => {
                let message = format!("{e}; no interactive terminal is attached");
                eprintln!("kwil: {message}");
                #[cfg(windows)]
                windows::show_error(&message, args.hidden);
                return ExitCode::FAILURE;
            }
        }
    }
    if use_gui {
        let view = match make_view() {
            Ok(v) => v,
            Err(e) => {
                eprintln!("kwil: {e}");
                return ExitCode::FAILURE;
            }
        };

        let opts = kwil_ui_gpu::GuiOptions { hidden: args.hidden };
        match kwil_ui_gpu::run(view, opts, on_first_frame()) {
            Ok(()) => return ExitCode::SUCCESS,
            #[cfg(windows)]
            Err(e) if windows::gui_child() => {
                windows::report_gui_failure(&e);
                return ExitCode::from(75);
            }
            // Spec §6: fall back to the TUI when the window fails and a
            // terminal is attached.
            Err(e) if after_gui_failure(in_terminal) == Frontend::Tui => eprintln!("kwil: {e}; using the terminal UI"),
            Err(e) => {
                let message = format!("{e}; no interactive terminal is attached");
                eprintln!("kwil: {message}");
                #[cfg(windows)]
                windows::show_error(&message, args.hidden);
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
    match kwil_ui_tui::run(view, on_first_frame()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kwil: {e}");
            ExitCode::FAILURE
        }
    }
}
