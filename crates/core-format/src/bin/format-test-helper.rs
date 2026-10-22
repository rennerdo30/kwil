//! Small cross-platform subprocess fixture, not part of the editor binary.
use std::io::{Read, Write};
fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("echo") => {
            let mut b = Vec::new();
            std::io::stdin().read_to_end(&mut b).unwrap();
            std::io::stdout().write_all(&b).unwrap();
        }
        Some("sleep") => std::thread::sleep(std::time::Duration::from_secs(2)),
        Some(mode @ ("wrapper" | "wrapper-success")) => {
            let receipt = std::env::args().nth(2).unwrap();
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command.args(["grandchild", &receipt]);
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                command.creation_flags(0x0800_0000);
            }
            // The success fixture intentionally exits before its child: the
            // formatter's containment must stop it. Natural lifetime is bounded.
            #[allow(clippy::zombie_processes)]
            let mut child = command.spawn().unwrap();
            eprintln!("owned grandchild PID {}", child.id());
            if mode == "wrapper" {
                child.wait().unwrap();
            } else {
                let start = std::time::Instant::now();
                while !std::path::Path::new(&receipt).with_extension("pid").exists() {
                    assert!(start.elapsed() < std::time::Duration::from_secs(2), "grandchild readiness timeout");
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                std::io::stdout().write_all(b"formatted\n").unwrap();
            }
        }
        Some("grandchild") => {
            let path = std::path::PathBuf::from(std::env::args().nth(2).unwrap());
            let file = std::fs::OpenOptions::new().create_new(true).read(true).write(true).open(&path).unwrap();
            file.lock().unwrap();
            std::fs::write(path.with_extension("pid"), std::process::id().to_string()).unwrap();
            std::thread::sleep(std::time::Duration::from_secs(6));
        }
        Some("exit") => std::process::exit(2),
        Some("invalid") => std::io::stdout().write_all(&[255]).unwrap(),
        Some("flood") => std::io::stdout().write_all(&[b'x'; 2048]).unwrap(),
        _ => std::process::exit(3),
    }
}
