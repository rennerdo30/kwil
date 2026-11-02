use std::{io, path::Path, process::Command};

/// Runs on the existing document worker after the primary store is claimed.
/// A child bypasses primary IPC and must claim its own store before recovery.
pub(crate) fn restore(
    base: &Path,
    ids: Vec<String>,
    hidden: bool,
    renderer: Option<kwil_ui_gpu::RenderBackend>,
) -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let mut failure = None;
    for id in ids {
        let mut command = Command::new(&exe);
        command.args(["--gui", "--restore-window", &id]).env("KWIL_WORKSPACE_ROOT", base);
        if hidden {
            command.arg("--hidden");
        }
        if let Some(renderer) = renderer {
            command.args(["--renderer", renderer.as_str()]);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000); // CREATE_NO_WINDOW; GUI visibility is controlled by --hidden
        }
        match command.spawn() {
            Ok(child) => eprintln!("kwil: restored window {id}, PID {}", child.id()),
            Err(error) => {
                eprintln!("kwil: cannot reopen saved window {id}: {error}; recovery retained");
                failure = Some(error);
            }
        }
    }
    failure.map_or(Ok(()), Err)
}
