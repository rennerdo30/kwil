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
        Some("exit") => std::process::exit(2),
        Some("invalid") => std::io::stdout().write_all(&[255]).unwrap(),
        Some("flood") => std::io::stdout().write_all(&[b'x'; 2048]).unwrap(),
        _ => std::process::exit(3),
    }
}
