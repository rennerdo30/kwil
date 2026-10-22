//! Worker-only file discovery. No file contents are read and symlinks are not followed.
//!
//! Run `walk` on a parent-owned worker, streaming each path directly into the
//! palette's bounded feed. Returning `ControlFlow::Break` stops discovery; the
//! cancellation token is also checked before each iterator advance and callback.
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Default)]
pub struct Cancellation(Arc<AtomicBool>);

impl Cancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct WalkLimits {
    pub max_files: usize,
    /// Sum of native path bytes emitted, including the root portion.
    pub max_path_bytes: usize,
    pub max_depth: usize,
}

impl Default for WalkLimits {
    fn default() -> Self {
        Self { max_files: 100_000, max_path_bytes: 32 * 1024 * 1024, max_depth: 64 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WalkEnd {
    Complete,
    Cancelled,
    Capacity,
    ConsumerStopped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WalkReport {
    pub files: usize,
    pub path_bytes: usize,
    /// Includes invalid ignore patterns and inaccessible entries; never stored as an unbounded list.
    pub errors: usize,
    pub end: WalkEnd,
}

/// Streams exact OS paths, honoring `.gitignore`, `.ignore` and nested ignores
/// even outside a Git checkout. Hidden files are included unless ignored.
/// Parent ignore files are honored; user-global Git ignores are excluded so the
/// catalogue is determined by workspace files rather than machine Git settings.
/// Cancellation is cooperative: an in-flight OS directory operation cannot be interrupted.
pub fn walk(
    root: &Path,
    limits: WalkLimits,
    cancel: &Cancellation,
    mut emit: impl FnMut(PathBuf) -> ControlFlow<()>,
) -> WalkReport {
    let mut report = WalkReport { files: 0, path_bytes: 0, errors: 0, end: WalkEnd::Complete };
    if cancel.is_cancelled() {
        report.end = WalkEnd::Cancelled;
        return report;
    }
    let mut walker = ignore::WalkBuilder::new(root);
    walker
        .hidden(false)
        .follow_links(false)
        .parents(true)
        .git_global(false)
        .require_git(false)
        .max_depth(Some(limits.max_depth));
    // This iterator is single-threaded: the caller owns the worker budget.
    let mut entries = walker.build();
    loop {
        if cancel.is_cancelled() {
            report.end = WalkEnd::Cancelled;
            break;
        }
        let Some(entry) = entries.next() else {
            break;
        };
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                report.errors = report.errors.saturating_add(1);
                continue;
            }
        };
        if entry.error().is_some() {
            report.errors = report.errors.saturating_add(1);
        }
        if entry.depth() == limits.max_depth && entry.file_type().is_some_and(|kind| kind.is_dir()) {
            // Conservatively report truncation even for an empty directory:
            // discovering whether it has children would exceed the depth bound.
            report.end = WalkEnd::Capacity;
        }
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let bytes = entry.path().as_os_str().as_encoded_bytes().len();
        if report.files >= limits.max_files || bytes > limits.max_path_bytes.saturating_sub(report.path_bytes) {
            report.end = WalkEnd::Capacity;
            break;
        }
        if cancel.is_cancelled() {
            report.end = WalkEnd::Cancelled;
            break;
        }
        report.files += 1;
        report.path_bytes += bytes;
        if emit(entry.into_path()).is_break() {
            report.end = WalkEnd::ConsumerStopped;
            break;
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "kw24-catalogue-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let resolved = self.0.canonicalize().unwrap();
            let temp = std::env::temp_dir().canonicalize().unwrap();
            assert_eq!(resolved.parent(), Some(temp.as_path()));
            assert!(resolved.file_name().unwrap().to_str().unwrap().starts_with("kw24-catalogue-"));
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn ignores_nested_paths_and_streams_exact_unicode_paths() {
        let f = Fixture::new();
        fs::create_dir(f.0.join("nested")).unwrap();
        fs::write(f.0.join(".gitignore"), "*.tmp\n").unwrap();
        fs::write(f.0.join("nested/.ignore"), "skip.txt\n").unwrap();
        for name in ["東京.rs", "hidden.tmp", "nested/hidden.tmp", "nested/keep.txt", "nested/skip.txt"] {
            fs::write(f.0.join(name), "").unwrap();
        }
        let mut paths = Vec::new();
        let report = walk(&f.0, WalkLimits::default(), &Cancellation::default(), |p| {
            paths.push(p);
            ControlFlow::Continue(())
        });
        assert_eq!(report.end, WalkEnd::Complete);
        assert!(paths.contains(&f.0.join("東京.rs")));
        assert!(paths.contains(&f.0.join("nested/keep.txt")));
        assert!(!paths.contains(&f.0.join("hidden.tmp")), "root gitignore must hide matching files");
        assert!(!paths.contains(&f.0.join("nested/skip.txt")));
        let mut nested = Vec::new();
        walk(&f.0.join("nested"), WalkLimits::default(), &Cancellation::default(), |p| {
            nested.push(p);
            ControlFlow::Continue(())
        });
        assert!(!nested.contains(&f.0.join("nested/hidden.tmp")), "parent .gitignore applies to nested root");
    }

    #[test]
    fn cancellation_stops_before_start_and_after_first_emit() {
        let f = Fixture::new();
        fs::write(f.0.join("one"), "").unwrap();
        fs::write(f.0.join("two"), "").unwrap();
        let cancel = Cancellation::default();
        let report = walk(&f.0, WalkLimits::default(), &cancel, |_| {
            cancel.cancel();
            ControlFlow::Continue(())
        });
        assert_eq!(report.end, WalkEnd::Cancelled, "catalogue continued after cancellation");
        assert_eq!(report.files, 1);
        let report =
            walk(&f.0.join("does-not-exist"), WalkLimits::default(), &cancel, |_| panic!("cancelled walk emitted"));
        assert_eq!(report.files, 0);
        assert_eq!(report.errors, 0);
        assert_eq!(report.end, WalkEnd::Cancelled);
    }

    #[test]
    fn bounds_and_consumer_stop_are_explicit() {
        let f = Fixture::new();
        for name in ["one", "two", "three"] {
            fs::write(f.0.join(name), "").unwrap();
        }
        let report = walk(&f.0, WalkLimits { max_files: 1, ..WalkLimits::default() }, &Cancellation::default(), |_| {
            ControlFlow::Continue(())
        });
        assert_eq!(report.files, 1, "catalogue file bound exceeded");
        assert_eq!(report.end, WalkEnd::Capacity);
        let report =
            walk(&f.0, WalkLimits { max_path_bytes: 0, ..WalkLimits::default() }, &Cancellation::default(), |_| {
                panic!("path budget exceeded")
            });
        assert_eq!(report.end, WalkEnd::Capacity);
        let report = walk(&f.0, WalkLimits::default(), &Cancellation::default(), |_| ControlFlow::Break(()));
        assert_eq!(report.files, 1);
        assert_eq!(report.end, WalkEnd::ConsumerStopped);
    }

    #[test]
    fn depth_bound_and_os_path_identity_are_preserved() {
        let f = Fixture::new();
        fs::create_dir(f.0.join("nested")).unwrap();
        fs::write(f.0.join("nested/deep"), "").unwrap();
        #[cfg(windows)]
        let name = {
            use std::os::windows::ffi::OsStringExt;
            std::ffi::OsString::from_wide(&[0x0061, 0xD800, 0x0062])
        };
        #[cfg(all(unix, not(target_os = "macos")))]
        let name = {
            use std::os::unix::ffi::OsStringExt;
            std::ffi::OsString::from_vec(vec![b'a', 0xff, b'b'])
        };
        // APFS rejects invalid UTF-8 filenames with EILSEQ before the walk can
        // observe them. Exercise exact native Unicode identity there; Linux
        // retains the arbitrary-byte contract above, Windows the surrogate one.
        #[cfg(target_os = "macos")]
        let name = std::ffi::OsString::from("日本語-log.rs");
        let path = f.0.join(name);
        fs::write(&path, "").unwrap();
        let mut emitted = Vec::new();
        let report = walk(&f.0, WalkLimits { max_depth: 1, ..WalkLimits::default() }, &Cancellation::default(), |p| {
            emitted.push(p);
            ControlFlow::Continue(())
        });
        assert_eq!(report.end, WalkEnd::Capacity);
        assert_eq!(emitted, [path], "catalogue changed native path identity");
    }
}
