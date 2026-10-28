//! A synchronous read view, equivalent to retaining a checked bytes() slice
//! during one command. No cache survives the outer call or crosses threads.
//! Explicit live checks and pin transitions invalidate the cached admission.
use super::*;
use std::{cell::RefCell, collections::HashMap};

#[derive(Default)]
struct Batch {
    depth: usize,
    sources: HashMap<usize, Admission>,
}
struct Admission {
    _source: Arc<Shared>,
    generation: u64,
    invalid: bool,
}
thread_local! { static BATCH: RefCell<Batch> = RefCell::new(Batch::default()); }

/// Amortize repeated checked reads of the same immutable object within one
/// synchronous model command. First access checks the live object/path; a
/// later explicit check failure or save pin generation change refuses further
/// reads, including nested calls. A fresh command always checks again.
///
/// As with a borrowed bytes() slice, this cannot protect against an external
/// process truncating while the command reads; it does not widen TextFile's
/// documented mmap TOCTOU contract. Never use across an event-loop wakeup.
pub fn with_checked_read_batch<R>(command: impl FnOnce() -> R) -> R {
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            BATCH.with(|batch| {
                let mut b = batch.borrow_mut();
                b.depth -= 1;
                if b.depth == 0 {
                    b.sources.clear();
                }
            });
        }
    }
    BATCH.with(|batch| batch.borrow_mut().depth += 1);
    let _guard = Guard;
    command()
}

pub(super) fn admitted(file: &TextFile) -> Option<Result<(), ReadError>> {
    let key = Arc::as_ptr(&file.shared) as usize;
    let lookup = BATCH.with(|batch| {
        let mut batch = batch.borrow_mut();
        if batch.depth == 0 {
            return None;
        }
        if let Some(admission) = batch.sources.get_mut(&key) {
            admission.invalid |= admission.generation != file.shared.read_generation.load(Ordering::Relaxed);
            return Some(Some(!admission.invalid));
        }
        Some(None)
    })?;
    if let Some(valid) = lookup {
        return Some(if valid {
            Ok(())
        } else {
            Err(ReadError::Io(io::Error::other("read batch invalidated by live check or original pin transition")))
        });
    }
    let generation = file.shared.read_generation.load(Ordering::Relaxed);
    let result = match file.check_source() {
        Ok(FileState::Unchanged) => Ok(()),
        Ok(state) => Err(ReadError::Changed(state)),
        Err(e) => Err(ReadError::Io(e)),
    };
    BATCH.with(|batch| {
        batch
            .borrow_mut()
            .sources
            .insert(key, Admission { _source: file.shared.clone(), generation, invalid: result.is_err() })
    });
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!(
                "kwil-read-batch-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::write(&p, b"old").unwrap();
            Self(p)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    #[test]
    fn live_check_invalidates_outer_nested_and_fresh_commands() {
        let f = Fixture::new();
        let file = TextFile::open(&f.0).unwrap();
        with_checked_read_batch(|| {
            assert_eq!(file.bytes().unwrap(), b"old");
            std::fs::write(&f.0, b"grown").unwrap();
            assert!(matches!(file.check(), Ok(FileState::Grown { .. })));
            assert!(file.bytes().is_err());
            with_checked_read_batch(|| assert!(file.bytes().is_err()));
            assert!(file.bytes().is_err());
        });
        assert!(with_checked_read_batch(|| file.bytes()).is_err());
        let reopened = TextFile::open(&f.0).unwrap();
        assert_eq!(with_checked_read_batch(|| reopened.bytes()).unwrap(), b"grown");
    }
    #[test]
    fn pin_generation_and_unwind_never_revive_nested_admission() {
        let f = Fixture::new();
        let file = TextFile::open(&f.0).unwrap();
        with_checked_read_batch(|| {
            assert_eq!(file.bytes().unwrap(), b"old");
            let pin = file.pin_original_for_save().unwrap();
            assert!(file.bytes().is_err());
            pin.rollback().unwrap();
            with_checked_read_batch(|| assert!(file.bytes().is_err()));
        });
        let _ = std::panic::catch_unwind(|| with_checked_read_batch(|| panic!("test unwind")));
        assert_eq!(with_checked_read_batch(|| file.bytes()).unwrap(), b"old");
        assert_eq!(BATCH.with(|b| b.borrow().depth), 0);
    }
}
