use crate::FormatError;
use std::{
    io::{Read, Seek, SeekFrom, Write},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub struct ExternalFormatter {
    pub command: String,
    pub args: Vec<String>,
    pub timeout: Duration,
}

pub fn run(text: &str, config: &ExternalFormatter, cap: usize) -> Result<String, FormatError> {
    if config.timeout.is_zero() || config.timeout > Duration::from_secs(120) {
        return Err(FormatError::new("External", "timeout must be in (0, 120s]", 1));
    }
    // Anonymous private temporary files avoid pipe writer/reader deadlocks and
    // detached blocking threads when a grandchild inherits an output handle.
    let io_error = |e| FormatError::new("External", e, 1);
    let mut input = tempfile::tempfile().map_err(io_error)?;
    let mut output = tempfile::tempfile().map_err(io_error)?;
    let error = tempfile::tempfile().map_err(io_error)?;
    input.write_all(text.as_bytes()).map_err(io_error)?;
    input.rewind().map_err(io_error)?;
    let mut command = Command::new(&config.command);
    command
        .args(&config.args)
        .stdin(Stdio::from(input))
        .stdout(output.try_clone().map_err(io_error)?)
        .stderr(error.try_clone().map_err(io_error)?);
    let (mut child, mut tree) = crate::process_tree::Tree::spawn(&mut command).map_err(io_error)?;
    let pid = child.id();
    let start = Instant::now();
    loop {
        let status = tree.finished(&mut child);
        let sizes = output.metadata().and_then(|o| error.metadata().map(|e| (o.len(), e.len())));
        let failure = match &sizes {
            Ok((out, err)) if *out > cap as u64 || *err > 64 * 1024 => Some("output exceeds configured limit"),
            Err(_) => Some("cannot inspect formatter output"),
            _ if start.elapsed() >= config.timeout => Some("formatter timed out"),
            _ if status.is_err() => Some("cannot wait for formatter"),
            _ => None,
        };
        if let Some(message) = failure {
            let killed = tree.terminate();
            // Also covers a containment API failure for the immediate child.
            let _ = child.kill();
            let waited = child.wait();
            return Err(FormatError::new(
                "External",
                format!("{message} (PID {pid}; kill={}, reap={})", killed.is_ok(), waited.is_ok()),
                1,
            ));
        }
        if status.map_err(io_error)? {
            // A wrapper may exit before its descendants. Stop the group before
            // reaping its leader (Unix PID reservation) or reading stdout.
            let stopped = tree.terminate();
            let status = child.wait().map_err(io_error)?;
            stopped.map_err(io_error)?;
            if !status.success() {
                return Err(FormatError::new("External", format!("formatter exited with {status} (PID {pid})"), 1));
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(10)); // Worker only, never the UI.
    }
    output.seek(SeekFrom::Start(0)).map_err(io_error)?;
    let mut bytes = Vec::new();
    output.take(cap as u64 + 1).read_to_end(&mut bytes).map_err(io_error)?;
    if bytes.len() > cap {
        return Err(FormatError::new("External", "output exceeds configured limit", 1));
    }
    String::from_utf8(bytes).map_err(|_| FormatError::new("External", "formatter output is not UTF-8", 1))
}
