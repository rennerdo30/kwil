//! Tiny disposable fixtures only. Source prepared under KW-20's source-only
//! grant; execution and fail-first mutation checks require parent admission.
use kwil_core_workspace::{SaveDestination, paths_may_alias, resolve_save_destination};
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "kwil-save-destination-{}-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        // Atomic create, never adopt or clean up a preexisting directory.
        fs::create_dir(&path).unwrap();
        Self(fs::canonicalize(path).unwrap())
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn file(&self, name: &str) -> PathBuf {
        let path = self.path(name);
        fs::write(&path, b"tiny fixture").unwrap();
        path
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        // Only the directory atomically created by this fixture is owned.
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn resolve(path: &Path) -> SaveDestination {
    resolve_save_destination(path).unwrap()
}

fn equivalent(a: &SaveDestination, b: &SaveDestination) {
    assert!(a.equivalent(b), "{a:?} must conflict with {b:?}");
    assert!(b.equivalent(a), "equivalence must be symmetric");
}

#[test]
fn existing_hardlinks_share_identity_even_with_distinct_names() {
    let fixture = Fixture::new();
    let original = fixture.file("original");
    let alias = fixture.path("hardlink");
    fs::hard_link(&original, &alias).unwrap();
    assert!(!paths_may_alias(&original, &alias));
    let original = resolve(&original);
    let alias = resolve(&alias);
    assert_ne!(original.canonical_path(), alias.canonical_path());
    equivalent(&original, &alias);
    assert!(original.exists());
}

#[test]
fn distinct_regular_files_and_missing_names_remain_distinct() {
    let fixture = Fixture::new();
    let a = resolve(&fixture.file("a"));
    let b = resolve(&fixture.file("b"));
    assert!(!a.equivalent(&b));
    assert!(!b.equivalent(&a));
    let missing_a = resolve(&fixture.path("missing-a"));
    let missing_b = resolve(&fixture.path("missing-b"));
    assert!(!missing_a.exists());
    assert!(!missing_a.equivalent(&missing_b));
    assert!(!a.equivalent(&missing_a));
    equivalent(&missing_a, &resolve(missing_a.requested_path()));
}

#[test]
fn missing_resolution_retains_native_request_and_only_canonicalizes_parent() {
    let fixture = Fixture::new();
    let directory = fixture.path("dir");
    fs::create_dir(&directory).unwrap();
    let requested = directory.join("..").join("not-created.txt");
    let destination = resolve(&requested);
    assert_eq!(destination.requested_path(), requested);
    assert_eq!(destination.canonical_path(), fixture.path("not-created.txt"));
    assert!(!destination.exists());
    assert!(!fixture.path("not-created.txt").exists());
    assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 1);
}

#[test]
fn missing_parent_and_file_parent_fail_without_creating_anything() {
    let fixture = Fixture::new();
    let missing = fixture.path("absent-parent").join("new.txt");
    assert_eq!(resolve_save_destination(&missing).unwrap_err().kind(), io::ErrorKind::NotFound);
    assert!(!fixture.path("absent-parent").exists());
    let file = fixture.file("not-a-directory");
    assert!(resolve_save_destination(&file.join("new.txt")).is_err());
    assert_eq!(fs::read(&file).unwrap(), b"tiny fixture");
}

#[test]
fn relative_oversized_directory_and_directory_syntax_are_refused() {
    let fixture = Fixture::new();
    assert_eq!(resolve_save_destination(Path::new("relative.txt")).unwrap_err().kind(), io::ErrorKind::InvalidInput,);
    let oversized = fixture.path(&"x".repeat(32 * 1024 + 1));
    assert_eq!(resolve_save_destination(&oversized).unwrap_err().kind(), io::ErrorKind::InvalidInput);
    assert!(paths_may_alias(&oversized, &fixture.path("normal")));
    assert!(resolve_save_destination(&fixture.0).is_err());
    // Append raw syntax: PathBuf::push can normalize a verbatim Windows path.
    let mut native_with_dot = fixture.path("missing").into_os_string();
    native_with_dot.push("/.");
    assert!(resolve_save_destination(Path::new(&native_with_dot)).is_err());
    let mut native_with_slash = fixture.path("missing").into_os_string();
    native_with_slash.push("/");
    let with_slash = PathBuf::from(native_with_slash);
    assert!(resolve_save_destination(&with_slash).is_err());
}

#[test]
fn ui_prefilter_uses_only_paths_without_requiring_an_existing_parent() {
    // No fixture, stat, canonicalization, cwd lookup, or filesystem setup here.
    // In particular, failure to resolve these parents must not affect the UI.
    #[cfg(windows)]
    let (a, same, different) = (
        Path::new(r"C:\kwil-path-only-absent\a.txt"),
        Path::new(r"c:/KWIL-PATH-ONLY-ABSENT/A.TXT"),
        Path::new(r"C:\kwil-path-only-absent\b.txt"),
    );
    #[cfg(unix)]
    let (a, same, different) = (
        Path::new("/kwil-path-only-absent/a.txt"),
        Path::new("/kwil-path-only-absent/./a.txt"),
        Path::new("/kwil-path-only-absent/b.txt"),
    );
    assert!(paths_may_alias(a, same));
    assert!(!paths_may_alias(a, different));
    assert!(paths_may_alias(Path::new("relative"), different));
}

#[test]
fn refresh_all_reservations_discovers_missing_to_created_hardlink_transition() {
    let fixture = Fixture::new();
    let pending_path = fixture.path("pending");
    let stale_pending = resolve(&pending_path);
    fixture.file("pending");
    let candidate_path = fixture.path("candidate-hardlink");
    fs::hard_link(&pending_path, &candidate_path).unwrap();
    let candidate = resolve(&candidate_path);
    // Refreshing only the new candidate cannot compare its ID with Missing.
    assert!(!stale_pending.equivalent(&candidate));
    let refreshed_pending = resolve(stale_pending.requested_path());
    assert!(refreshed_pending.exists());
    equivalent(&refreshed_pending, &candidate);
    equivalent(&stale_pending, &refreshed_pending); // same reserved pathname
}

fn directory_symlink(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    let result = std::os::unix::fs::symlink(target, link);
    #[cfg(windows)]
    let result = std::os::windows::fs::symlink_dir(target, link);
    match result {
        Ok(()) => true,
        #[cfg(windows)]
        Err(error) if error.raw_os_error() == Some(1314) || error.kind() == io::ErrorKind::PermissionDenied => {
            eprintln!("symlink check skipped: permission/privilege unavailable");
            false
        }
        Err(error) if error.kind() == io::ErrorKind::Unsupported => {
            eprintln!("symlink check skipped: fixture filesystem does not support symlinks");
            false
        }
        Err(error) => panic!("fixture symlink failed: {error}"),
    }
}

#[test]
fn symlinked_parent_resolves_existing_and_missing_destinations_where_permitted() {
    let fixture = Fixture::new();
    let real = fixture.path("real");
    let linked = fixture.path("linked");
    fs::create_dir(&real).unwrap();
    if !directory_symlink(&real, &linked) {
        return;
    }
    fs::write(real.join("existing"), b"x").unwrap();
    equivalent(&resolve(&real.join("existing")), &resolve(&linked.join("existing")));
    equivalent(&resolve(&real.join("missing")), &resolve(&linked.join("missing")));
    assert_eq!(resolve(&linked.join("missing")).canonical_path(), real.join("missing"));
}

#[test]
fn refresh_uses_original_requested_path_when_parent_symlink_moves() {
    let fixture = Fixture::new();
    let first = fixture.path("first");
    let second = fixture.path("second");
    let linked = fixture.path("linked");
    fs::create_dir(&first).unwrap();
    fs::create_dir(&second).unwrap();
    if !directory_symlink(&first, &linked) {
        return;
    }
    let stale = resolve(&linked.join("new"));
    #[cfg(windows)]
    fs::remove_dir(&linked).unwrap();
    #[cfg(unix)]
    fs::remove_file(&linked).unwrap();
    assert!(directory_symlink(&second, &linked));
    let refreshed = resolve(stale.requested_path());
    assert!(!stale.equivalent(&refreshed));
    equivalent(&refreshed, &resolve(&second.join("new")));
}

#[test]
fn dangling_destination_symlink_is_refused_where_permitted() {
    let fixture = Fixture::new();
    let link = fixture.path("dangling");
    #[cfg(unix)]
    std::os::unix::fs::symlink(fixture.path("absent"), &link).unwrap();
    #[cfg(windows)]
    {
        match std::os::windows::fs::symlink_file(fixture.path("absent"), &link) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(1314) || error.kind() == io::ErrorKind::PermissionDenied => {
                return;
            }
            Err(error) => panic!("fixture symlink failed: {error}"),
        }
    }
    assert!(resolve_save_destination(&link).is_err());
    assert!(!fixture.path("absent").exists());
}

#[cfg(unix)]
#[test]
fn unix_invalid_unicode_path_roundtrips_missing_and_existing() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    let fixture = Fixture::new();
    let path = fixture.0.join(OsString::from_vec(b"invalid-\xff-name".to_vec()));
    let missing = resolve(&path);
    assert_eq!(missing.requested_path().as_os_str(), path.as_os_str());
    assert_eq!(missing.canonical_path().as_os_str(), path.as_os_str());
    fs::write(&path, b"x").unwrap();
    let existing = resolve(&path);
    assert_eq!(existing.canonical_path().as_os_str(), path.as_os_str());
    equivalent(&missing, &existing);
    equivalent(&existing, &resolve(&path));
    assert!(paths_may_alias(&path, &path));
}

#[cfg(windows)]
#[test]
fn windows_invalid_unicode_path_roundtrips_missing_and_existing() {
    use std::{
        ffi::OsString,
        os::windows::ffi::{OsStrExt, OsStringExt},
    };
    let fixture = Fixture::new();
    let name = OsString::from_wide(&[b'x' as u16, 0xd800, b'y' as u16]);
    let path = fixture.0.join(&name);
    let missing = resolve(&path);
    assert_eq!(missing.requested_path().as_os_str(), path.as_os_str());
    assert_eq!(missing.canonical_path().file_name(), Some(name.as_os_str()));
    fs::write(&path, b"x").unwrap();
    let existing = resolve(&path);
    assert_eq!(
        existing.canonical_path().file_name().unwrap().encode_wide().collect::<Vec<_>>(),
        vec![120, 0xd800, 121]
    );
    equivalent(&missing, &existing);
    assert!(paths_may_alias(&path, &path));
}

#[cfg(windows)]
#[test]
fn windows_case_aliases_conflict_for_missing_and_existing_names() {
    let fixture = Fixture::new();
    let lower = fixture.path("case-name.txt");
    let upper = fixture.path("CASE-NAME.TXT");
    equivalent(&resolve(&lower), &resolve(&upper));
    fs::write(&lower, b"lower").unwrap();
    // Also valid in a case-sensitive directory: overrejection is intentional.
    equivalent(&resolve(&lower), &resolve(&upper));
    assert!(paths_may_alias(&lower, &upper));
}

#[cfg(windows)]
#[test]
fn windows_ordinal_unicode_case_and_dos_unc_verbatim_spellings() {
    assert!(paths_may_alias(Path::new(r"C:\absent\é.txt"), Path::new(r"c:/ABSENT/É.TXT")));
    assert!(paths_may_alias(Path::new(r"C:\absent\a\..\x"), Path::new(r"\\?\C:\absent\x")));
    assert!(paths_may_alias(Path::new(r"\\server\share\x"), Path::new(r"\\?\UNC\SERVER\SHARE\X")));
    // Ordinal casing alone cannot establish Unicode normalization aliases.
    assert!(!paths_may_alias(Path::new("C:\\absent\\é.txt"), Path::new("C:\\absent\\e\u{301}.txt")));
}

#[cfg(windows)]
#[test]
fn windows_devices_streams_trimmed_names_and_relative_drive_are_refused() {
    for name in [
        r"C:\NUL",
        r"C:\con.txt",
        r"C:\COM1",
        r"C:\a:stream",
        r"C:\trailing.",
        r"C:\trailing ",
        r"C:relative",
        r"\\.\pipe\kwil-save-test",
    ] {
        assert_eq!(resolve_save_destination(Path::new(name)).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert!(paths_may_alias(Path::new(name), Path::new(r"C:\ordinary")));
    }
}

#[cfg(unix)]
#[test]
fn unix_nonregular_socket_is_refused_without_opening_it() {
    let fixture = Fixture::new();
    let socket_path = fixture.path("socket");
    // AF_UNIX pathname limits can be shorter than the runner's temp path
    // (notably /var/folders on macOS); never use another directory as fallback.
    if socket_path.as_os_str().len() >= 100 {
        eprintln!("socket check skipped: disposable temp path exceeds portable AF_UNIX limit");
        return;
    }
    let _listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
    assert_eq!(resolve_save_destination(&socket_path).unwrap_err().kind(), io::ErrorKind::InvalidInput);
}

#[cfg(unix)]
#[test]
fn unix_parent_permission_errors_propagate_where_enforced() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let parent = fixture.path("private");
    fs::create_dir(&parent).unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0)).unwrap();
    let result = resolve_save_destination(&parent.join("new"));
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
    match result {
        Err(error) => assert_eq!(error.kind(), io::ErrorKind::PermissionDenied),
        Ok(_) => eprintln!("permission check skipped: runner can traverse mode-000 directories"),
    }
}

#[test]
fn unicode_normalization_aliases_are_decided_by_filesystem_identity_on_refresh() {
    let fixture = Fixture::new();
    let composed = fixture.path("é.txt");
    let decomposed = fixture.path("e\u{301}.txt");
    let pending = resolve(&decomposed);
    fs::write(&composed, b"x").unwrap();
    match fs::metadata(&decomposed) {
        Ok(_) => equivalent(&resolve(&composed), &resolve(pending.requested_path())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            // On byte-distinct filesystems these are separate destinations.
            fs::write(&decomposed, b"y").unwrap();
            assert!(!resolve(&composed).equivalent(&resolve(&decomposed)));
        }
        Err(error) => panic!("fixture metadata failed: {error}"),
    }
}
