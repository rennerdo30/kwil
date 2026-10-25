//! Exclusively owned directories for TUI fixtures.

use std::{
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

pub(crate) fn directory(prefix: &str) -> io::Result<PathBuf> {
    directory_in(&std::env::temp_dir(), prefix, &NEXT_DIRECTORY)
}

fn directory_in(parent: &Path, prefix: &str, next: &AtomicU64) -> io::Result<PathBuf> {
    // Sequence numbers avoid clock collisions; exclusive creation also rejects
    // occupied roots left by a previous process with the same PID.
    for _ in 0..128 {
        let sequence = next.fetch_add(1, Ordering::Relaxed);
        let root = parent.join(format!("{prefix}-{}-{sequence}", std::process::id()));
        match std::fs::create_dir(&root) {
            Ok(()) => return Ok(root),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "TUI fixture directory namespace exhausted"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_same_prefix_claims_distinct_roots() {
        let first = directory("kwil-tui-fixture-distinct").unwrap();
        let second = directory("kwil-tui-fixture-distinct").unwrap();
        assert_ne!(first, second);
        assert!(first.is_dir());
        assert!(second.is_dir());
        std::fs::remove_dir(first).unwrap();
        std::fs::remove_dir(second).unwrap();
    }

    #[test]
    fn directory_counter_replay_skips_existing_owners_and_preserves_sentinels() {
        let parent = directory("kwil-tui-fixture-replay").unwrap();
        let next = AtomicU64::new(0);
        let first = directory_in(&parent, "child", &next).unwrap();
        let second = directory_in(&parent, "child", &next).unwrap();
        std::fs::write(first.join("owner.bin"), b"first owner").unwrap();
        std::fs::write(second.join("owner.bin"), b"second owner").unwrap();

        // Replay the candidate sequence to force two collisions without timing.
        let replay = AtomicU64::new(0);
        let third = directory_in(&parent, "child", &replay).unwrap();
        assert_ne!(first, third);
        assert_ne!(second, third);
        assert!(third.is_dir());
        assert_eq!(replay.load(Ordering::Relaxed), 3, "both occupied owners were skipped");
        assert_eq!(std::fs::read(first.join("owner.bin")).unwrap(), b"first owner");
        assert_eq!(std::fs::read(second.join("owner.bin")).unwrap(), b"second owner");
        std::fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn directory_foreign_error_is_returned_without_retry() {
        let parent = directory("kwil-tui-fixture-error").unwrap();
        let next = AtomicU64::new(0);
        let error = directory_in(&parent.join("missing-parent"), "child", &next).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(next.load(Ordering::Relaxed), 1, "only occupied candidates may be retried");
        std::fs::remove_dir(parent).unwrap();
    }
}
