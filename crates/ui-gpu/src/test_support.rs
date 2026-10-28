//! Exclusively owned temporary directories for GUI tests.
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

pub(crate) fn directory(prefix: &str) -> io::Result<PathBuf> {
    directory_in(&std::env::temp_dir(), prefix, &NEXT_DIRECTORY)
}

fn directory_in(parent: &Path, prefix: &str, next: &AtomicU64) -> io::Result<PathBuf> {
    // Sequence identity does not depend on clock resolution. Exclusive creation
    // also rejects directories left by an earlier process with the same PID.
    for _ in 0..128 {
        let sequence = next.fetch_add(1, Ordering::Relaxed);
        let root = parent.join(format!("{prefix}-{}-{sequence}", std::process::id()));
        match fs::create_dir(&root) {
            Ok(()) => return Ok(root),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "GUI fixture namespace exhausted"))
}

#[test]
fn same_prefix_allocations_are_distinct() {
    let first = directory("kwil-gui-fixture-distinct").unwrap();
    let second = directory("kwil-gui-fixture-distinct").unwrap();
    assert_ne!(first, second);
    assert_eq!(first.parent(), Some(std::env::temp_dir().as_path()));
    assert_eq!(second.parent(), first.parent());
    fs::remove_dir(first).unwrap();
    fs::remove_dir(second).unwrap();
}

#[test]
fn replayed_sequence_skips_existing_owners_without_changing_their_bytes() {
    let parent = directory("kwil-gui-fixture-collision").unwrap();
    let next = AtomicU64::new(0);
    let first = directory_in(&parent, "occupied", &next).unwrap();
    let sentinel = first.join("owner.bin");
    fs::write(&sentinel, b"existing owner").unwrap();
    let second = directory_in(&parent, "occupied", &next).unwrap();
    assert_ne!(first, second);

    let replay = AtomicU64::new(0);
    let third = directory_in(&parent, "occupied", &replay).unwrap();
    assert_ne!(third, first);
    assert_ne!(third, second);
    assert_eq!(replay.load(Ordering::Relaxed), 3, "both existing directories were skipped");
    assert_eq!(fs::read(&sentinel).unwrap(), b"existing owner");

    fs::remove_dir(third).unwrap();
    fs::remove_dir(second).unwrap();
    fs::remove_file(sentinel).unwrap();
    fs::remove_dir(first).unwrap();
    fs::remove_dir(parent).unwrap();
}

#[test]
fn errors_other_than_already_exists_are_returned_without_retry() {
    let parent = directory("kwil-gui-fixture-error").unwrap();
    let missing = parent.join("missing");
    let next = AtomicU64::new(0);
    let error = directory_in(&missing, "unused", &next).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    assert_eq!(next.load(Ordering::Relaxed), 1, "foreign I/O errors are not retried");
    assert!(!missing.exists());
    fs::remove_dir(parent).unwrap();
}
