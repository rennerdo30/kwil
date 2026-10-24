//! Shared application used by the GUI and console entry points.

mod file_argument;
pub mod frontend;
mod instance;
mod startup;
#[cfg(windows)]
mod windows;
use frontend::{Flags, Frontend, ProcessEnv, after_gui_failure, choose_frontend, current_platform};

use file_argument::FileArgument;
use kwil_core_text::TextFile;
use kwil_view_model::{Notify, View};
use std::io::IsTerminal;
use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const HELP: &str = "\
Kwil - a blazing fast plain text editor

Usage: kwil [options] [file[:line[:column]] ...]

Options:
  --gui        Force the window UI
  --tui        Force the terminal UI
  --renderer auto|cpu|gpu  Select the GUI rendering backend (overrides settings)
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
    renderer: Option<kwil_ui_gpu::RenderBackend>,
    files: Vec<FileArgument>,
}

fn parse_args() -> Result<Args, String> {
    parse_args_values(std::env::args_os().skip(1))
}

fn parse_args_values(mut arguments: impl Iterator<Item = std::ffi::OsString>) -> Result<Args, String> {
    let mut a = Args::default();
    let mut literal = false;
    while let Some(arg) = arguments.next() {
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
            Some("--renderer") => {
                if a.renderer.is_some() {
                    return Err("--renderer was specified more than once".into());
                }
                let value = arguments.next().ok_or("--renderer requires auto, cpu or gpu")?;
                a.renderer = Some(
                    value
                        .to_str()
                        .and_then(kwil_ui_gpu::RenderBackend::parse)
                        .ok_or("--renderer requires auto, cpu or gpu")?,
                );
            }
            Some(value) if value.starts_with("--renderer=") => {
                if a.renderer.is_some() {
                    return Err("--renderer was specified more than once".into());
                }
                a.renderer = Some(
                    kwil_ui_gpu::RenderBackend::parse(&value[11..]).ok_or("--renderer requires auto, cpu or gpu")?,
                );
            }
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
    if a.tui && a.renderer.is_some() {
        return Err("--renderer selects the GUI backend, not the terminal UI".into());
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

    let make_view = || match args.files.first() {
        Some(file) => initial_argument_view(file),
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
    // Hidden verification runs never join or claim the owner's live instance.
    let endpoint = if args.hidden {
        None
    } else {
        match instance::endpoint() {
            Ok(endpoint) => Some(endpoint),
            Err(error) => {
                eprintln!("kwil: instance setup failed: {error}");
                return ExitCode::FAILURE;
            }
        }
    };
    if let Some(endpoint) = &endpoint {
        match instance::try_forward(endpoint, &requests) {
            Ok(true) => {
                #[cfg(windows)]
                if use_gui && !console_launcher {
                    // The existing UI accepted the request. Complete the console
                    // launch handshake without constructing a second window.
                    windows::first_frame();
                }
                return ExitCode::SUCCESS;
            }
            Ok(false) => {}
            Err(error) => {
                eprintln!("kwil: cannot contact the running instance: {error}");
                return ExitCode::FAILURE;
            }
        }
    }
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
    let instance_owner = match endpoint {
        Some(endpoint) => match instance::claim(endpoint, &requests) {
            Ok(instance::Start::Primary(owner)) => Some(owner),
            Ok(instance::Start::Forwarded) => {
                #[cfg(windows)]
                if use_gui {
                    windows::first_frame();
                }
                return ExitCode::SUCCESS;
            }
            Err(error) => {
                eprintln!("kwil: cannot own an editor instance: {error}");
                return ExitCode::FAILURE;
            }
        },
        None => None,
    };
    let remaining = remaining_initial_files(&requests);
    let external_ready = || {
        if let Some(owner) = &instance_owner {
            owner.ready(remaining.clone())
        } else {
            initial_delivery(remaining.clone())
        }
    };
    if use_gui {
        let view = match make_view() {
            Ok(v) => v,
            Err(e) => {
                eprintln!("kwil: {e}");
                return ExitCode::FAILURE;
            }
        };

        let opts = kwil_ui_gpu::GuiOptions { hidden: args.hidden, renderer: args.renderer };
        match kwil_ui_gpu::run_with_external_open(view, opts, on_first_frame(true), Box::new(|_| {}), external_ready())
        {
            Ok(()) => return ExitCode::SUCCESS,
            Err(e @ kwil_ui_gpu::GuiError::Settings(_)) => {
                presentation.report(&format!("kwil: {e}"), |message| eprintln!("{message}"), dialog);
                return ExitCode::FAILURE;
            }
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
    if let Err(error) = validate_hidden_tui_root(
        args.hidden,
        std::env::var_os("KWIL_WORKSPACE_ROOT").as_deref().map(std::path::Path::new),
    ) {
        eprintln!("kwil: {error}");
        return ExitCode::FAILURE;
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

fn initial_view(path: &std::path::Path) -> Result<View, String> {
    let file = TextFile::open(path).map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    // The checked document worker owns binary classification and decoding.
    // Startup must neither reject binary encodings nor expose physical bytes.
    Ok(View::from_preview(path.to_path_buf(), file, Vec::new()))
}

fn initial_argument_view(file: &FileArgument) -> Result<View, String> {
    let mut view = initial_view(&file.path)?;
    if let Some(line) = file.line {
        view.execute(
            "navigation.goToLine",
            &kwil_view_model::CommandArgs { line: Some(line), column: file.column, ..Default::default() },
            1,
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(view)
}

fn remaining_initial_files(
    requests: &kwil_core_workspace::instance::OpenFiles,
) -> kwil_core_workspace::instance::OpenFiles {
    // The first request already owns initial_argument_view, including its
    // deferred position. Only later CLI files belong to the delivery callback.
    // Forwarding to an existing instance continues to use the complete batch.
    kwil_core_workspace::instance::OpenFiles { files: requests.files.iter().skip(1).cloned().collect() }
}

fn initial_delivery(requests: kwil_core_workspace::instance::OpenFiles) -> Box<dyn FnOnce(kwil_ui_gpu::ExternalOpen)> {
    Box::new(move |open| {
        if !requests.files.is_empty()
            && let Err(error) = open(requests)
        {
            eprintln!("kwil: initial file delivery failed: {error}");
        }
    })
}

fn validate_hidden_tui_root(hidden: bool, root: Option<&std::path::Path>) -> Result<(), &'static str> {
    if hidden && root.is_none_or(|root| !root.is_absolute()) {
        Err("hidden terminal runs require an absolute KWIL_WORKSPACE_ROOT; live recovery storage was not accessed")
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod isolation_tests {
    use super::*;
    fn await_initial(
        workspace: &mut kwil_view_model::Workspace,
        receiver: &std::sync::mpsc::Receiver<kwil_view_model::document_worker::DocumentResult>,
        done: impl Fn(&kwil_view_model::Workspace) -> bool,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !done(workspace) {
            let remaining =
                deadline.checked_duration_since(std::time::Instant::now()).expect("bounded initial delivery");
            workspace.document_result(receiver.recv_timeout(remaining).expect("initial delivery worker wake"));
        }
    }

    #[test]
    fn initial_delivery_does_not_reoffer_cancelled_binary_and_preserves_cli_locations() {
        use kwil_core_workspace::instance::{OpenFile, OpenFiles};
        use kwil_view_model::{
            Workspace,
            document_worker::{DocumentResult, binary_offer::Choice},
        };
        for binary in [true, false] {
            let root = std::env::temp_dir().join(format!(
                "kwil-initial-delivery-{}-{}-{binary}",
                std::process::id(),
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
            ));
            std::fs::create_dir(&root).unwrap();
            let first = root.join("first.bin");
            let later = root.join("later.txt");
            std::fs::write(&first, if binary { b"header\0binary\x01\n".as_slice() } else { b"first\nsecond\n" })
                .unwrap();
            std::fs::write(&later, b"one\nsecond\nthird\n").unwrap();
            let requests = OpenFiles {
                files: vec![
                    OpenFile { path: first.clone(), line: Some(2), column: Some(3) },
                    OpenFile { path: later.clone(), line: Some(2), column: Some(3) },
                ],
            };
            let view =
                initial_argument_view(&FileArgument { path: first.clone(), line: Some(2), column: Some(3) }).unwrap();
            let mut workspace = Workspace::new(view).unwrap();
            workspace.set_rows(3);
            let original = workspace.active_id();
            let (sender, receiver) = std::sync::mpsc::channel();
            workspace
                .start_document_worker_at(Some(root.join("state")), move |result| {
                    let _ = sender.send(result);
                })
                .unwrap();
            await_initial(&mut workspace, &receiver, |w| {
                if binary { w.binary_offer().is_some() } else { w.editor().is_some() }
            });
            if !binary {
                assert_eq!(workspace.editor().unwrap().primary_cursor().head, 8, "first CLI position is applied once");
            }
            let remaining = remaining_initial_files(&requests);
            assert_eq!(remaining.files, requests.files[1..]);
            let (open, inbox) = kwil_view_model::document_worker::external_open_channel(|| Ok(()));
            initial_delivery(remaining)(open);
            if binary {
                let offer = workspace.binary_offer().unwrap();
                workspace.resolve_binary_offer(offer.ticket, Choice::Cancel, 3).unwrap();
                await_initial(&mut workspace, &receiver, |w| !w.binary_offer_pending(original) && w.editor().is_some());
            }
            for request in inbox.drain() {
                workspace.open_files(request).unwrap();
            }
            await_initial(&mut workspace, &receiver, |w| {
                w.editor().is_some_and(|e| e.document().path() == Some(later.as_path()))
            });
            assert_eq!(workspace.editor().unwrap().primary_cursor().head, 6, "later CLI line/column is preserved");
            if binary {
                assert!(
                    workspace.session().tabs().iter().all(|tab| tab.path.as_ref() != Some(&first)),
                    "Cancel must not reoffer initial file"
                );
                // A genuine later open of that same native path remains valid.
                workspace
                    .open_files(OpenFiles { files: vec![OpenFile { path: first, line: None, column: None }] })
                    .unwrap();
                await_initial(&mut workspace, &receiver, |w| w.binary_offer().is_some());
                let offer = workspace.binary_offer().unwrap();
                assert_ne!(offer.ticket.document(), original);
                workspace.resolve_binary_offer(offer.ticket, Choice::Cancel, 3).unwrap();
                await_initial(&mut workspace, &receiver, |w| !w.binary_offer_pending(offer.ticket.document()));
            }
            await_initial(&mut workspace, &receiver, |w| !w.search_replacement_pending());
            while !workspace.prepare_hex_shutdown() {
                workspace.document_result(receiver.recv_timeout(std::time::Duration::from_secs(10)).unwrap());
            }
            drop(workspace);
            loop {
                if let DocumentResult::Stopped(result) =
                    receiver.recv_timeout(std::time::Duration::from_secs(10)).unwrap()
                {
                    result.unwrap();
                    break;
                }
            }
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn cold_start_binary_waits_for_worker_choice_without_text_interpretation() {
        let path = std::env::temp_dir().join(format!(
            "kwil-startup-binary-{}-{}.bin",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::write(&path, [0, 0, 0xfe, 0xff, 0x41]).unwrap();
        let mut view = initial_view(&path).expect("binary startup reaches the checked document worker");
        assert_eq!(view.file().unwrap().kind().unwrap(), kwil_core_text::FileKind::Binary);
        assert!(
            kwil_core_text::encoding::EncodedFile::new(view.file().unwrap().clone()).is_err(),
            "fixture witnesses the old eager text-detection refusal"
        );
        view.set_rows(2);
        assert!(view.editor().is_none());
        assert!(
            view.line(0).unwrap_or_default().is_empty(),
            "unclassified physical bytes cannot reach the paintable preview"
        );
        drop(view);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn renderer_cli_accepts_all_modes_and_rejects_ambiguous_or_missing_values() {
        use kwil_ui_gpu::RenderBackend;
        for (name, mode) in [("auto", RenderBackend::Auto), ("cpu", RenderBackend::Cpu), ("gpu", RenderBackend::Gpu)] {
            let parsed = parse_args_values(["--renderer", name].into_iter().map(std::ffi::OsString::from)).unwrap();
            assert_eq!(parsed.renderer, Some(mode));
            let parsed =
                parse_args_values([format!("--renderer={name}")].into_iter().map(std::ffi::OsString::from)).unwrap();
            assert_eq!(parsed.renderer, Some(mode));
        }
        for args in [
            vec!["--renderer"],
            vec!["--renderer", "other"],
            vec!["--renderer=GPU"],
            vec!["--renderer=cpu", "--renderer=gpu"],
            vec!["--tui", "--renderer=cpu"],
        ] {
            assert!(parse_args_values(args.into_iter().map(std::ffi::OsString::from)).is_err());
        }
        let literal = parse_args_values(["--", "--renderer"].into_iter().map(std::ffi::OsString::from)).unwrap();
        assert!(literal.renderer.is_none());
        assert_eq!(literal.files.len(), 1);
    }
    #[test]
    fn hidden_terminal_missing_empty_and_relative_roots_fail_closed() {
        for root in [None, Some(std::path::Path::new("")), Some(std::path::Path::new("relative"))] {
            assert!(validate_hidden_tui_root(true, root).is_err());
        }
        assert!(validate_hidden_tui_root(false, None).is_ok());
        assert!(validate_hidden_tui_root(true, Some(&std::env::temp_dir())).is_ok());
    }
}
