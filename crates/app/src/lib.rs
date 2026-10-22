//! Shared application used by the GUI and console entry points.

mod file_argument;
pub mod frontend;
mod startup;
#[cfg(windows)]
mod windows;
use frontend::{Flags, Frontend, ProcessEnv, after_gui_failure, choose_frontend, current_platform};

use file_argument::FileArgument;
use kwil_core_text::TextFile;
use kwil_view_model::{Notify, View};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const HELP: &str = "\
Kwil - a blazing fast plain text editor

Usage: kwil [options] [file[:line[:column]] ...]

Options:
  --gui        Force the window UI
  --tui        Force the terminal UI
  --update     Check for an update now and install it
  --version    Print the version
  -h, --help   Print this help
  --           Treat following arguments as literal file names

Keys: arrows, PageUp/PageDown, Ctrl+Home/End to move; Ctrl+Q to quit (Esc in the terminal UI).
Auto-update: checked once a day after start-up; set KWIL_NO_UPDATE=1 or put a
kwil.no-update file next to the executable to turn it off.";

#[derive(Default)]
struct Args {
    gui: bool,
    tui: bool,
    hidden: bool,
    update: bool,
    files: Vec<FileArgument>,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args::default();
    let mut literal = false;
    for arg in std::env::args_os().skip(1) {
        if literal {
            a.files.push(FileArgument::parse(arg, true)?);
            if a.files.len() > kwil_core_workspace::instance::MAX_OPEN_FILES {
                return Err("too many files (maximum 128)".into());
            }
            continue;
        }
        match arg.to_str() {
            Some("--") => literal = true,
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
            _ => {
                a.files.push(FileArgument::parse(arg, false)?);
                if a.files.len() > kwil_core_workspace::instance::MAX_OPEN_FILES {
                    return Err("too many files (maximum 128)".into());
                }
            }
        }
    }
    if a.gui && a.tui {
        return Err("--gui and --tui exclude each other".into());
    }
    if !a.files.is_empty() {
        let directory =
            std::env::current_dir().map_err(|error| format!("cannot resolve working directory: {error}"))?;
        a.files = a.files.into_iter().map(|file| file.resolve(&directory)).collect::<Result<_, _>>()?;
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
    let platform = current_platform();
    #[cfg(windows)]
    let streams = windows::standard_streams();
    #[cfg(not(windows))]
    let streams = if std::io::stderr().is_terminal() {
        startup::StandardStreams::Terminal
    } else {
        startup::StandardStreams::Redirected
    };
    let presentation =
        startup::error_presentation(std::env::args_os().skip(1), console_launcher, streams, &ProcessEnv, platform);
    let dialog = |message: &str| {
        #[cfg(windows)]
        windows::show_error(message);
        #[cfg(not(windows))]
        let _ = message;
    };
    if let Err(code) =
        startup::recover(presentation, kwil_update::recover_on_startup, |message| eprintln!("{message}"), dialog)
    {
        return code;
    }
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
    let make_view = || match args.files.first() {
        Some(file) => open(&file.path),
        None => Ok(View::new(None)),
    };
    let requests = kwil_core_workspace::instance::OpenFiles {
        files: args
            .files
            .iter()
            .map(|file| kwil_core_workspace::instance::OpenFile {
                path: file.path.clone(),
                line: file.line,
                column: file.column,
            })
            .collect(),
    };
    if let Err(error) = requests.encode() {
        eprintln!("kwil: {error}");
        return ExitCode::from(2);
    }
    let external_ready = || {
        let requests = requests.clone();
        Box::new(move |open: kwil_ui_gpu::ExternalOpen| {
            if !requests.files.is_empty() {
                open(requests);
            }
        }) as Box<dyn FnOnce(kwil_ui_gpu::ExternalOpen)>
    };
    let on_first_frame = |_gui: bool| -> Box<dyn FnOnce(Notify)> {
        Box::new(move |notify: Notify| {
            #[cfg(windows)]
            if _gui {
                windows::first_frame();
            }
            #[cfg(all(windows, debug_assertions))]
            if !_gui && std::env::var_os("KWIL_TRACE_LAUNCH").is_some() {
                eprintln!("kwil: TUI ready");
            }
            kwil_update::spawn_background(VERSION, notify);
        })
    };
    let in_terminal = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();

    let frontend = choose_frontend(&ProcessEnv, Flags { gui: args.gui, tui: args.tui }, platform);
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
                presentation.report(&format!("kwil: {message}"), |message| eprintln!("{message}"), dialog);
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
        match kwil_ui_gpu::run_with_external_open(view, opts, on_first_frame(true), Box::new(|_| {}), external_ready())
        {
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
                presentation.report(&format!("kwil: {message}"), |message| eprintln!("{message}"), dialog);
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
    match kwil_ui_tui::run_with_external_open(view, on_first_frame(false), external_ready()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kwil: {e}");
            ExitCode::FAILURE
        }
    }
}
