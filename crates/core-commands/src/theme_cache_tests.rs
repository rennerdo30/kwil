use super::*;
#[test]
fn cache_failed_publication_keeps_previous_bytes_contract() {
    let root = std::env::temp_dir().join(format!(
        "kwil-theme-write-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    let path = root.join("active.bin");
    fs::write(&path, b"previous valid cache").unwrap();
    let result = atomic_write_with(&path, b"replacement", |_, _| {
        Err(std::io::Error::other("fixture atomic replacement failure"))
    });
    assert!(result.is_err());
    assert_eq!(fs::read(&path).unwrap(), b"previous valid cache");
    assert_eq!(fs::read_dir(&root).unwrap().count(), 1, "exclusive temp must be removed on failed publication");
    atomic_write(&path, b"new complete cache").unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"new complete cache");
    assert!(atomic_write_new(&path, b"must not overwrite").is_err());
    assert_eq!(fs::read(&path).unwrap(), b"new complete cache");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn cache_structural_validation_survives_valid_checksum_contract() {
    let cached = CachedTheme { selection: ThemeSelection::Named(ThemeId::Dark), theme: Theme::builtin(Scheme::Dark) };
    let original = encode(&cached).unwrap();
    let rehash = |bytes: &mut Vec<u8>| {
        let len = bytes.len() - HEADER;
        bytes[10..14].copy_from_slice(&(len as u32).to_le_bytes());
        let check = hash(&bytes[HEADER..]);
        bytes[14..22].copy_from_slice(&check.to_le_bytes());
    };
    // Unlike random corruption, these payloads pass the integrity checksum;
    // only the structural decoder can reject them.
    let mut bad_tag = original.clone();
    bad_tag[HEADER] = 255;
    rehash(&mut bad_tag);
    assert!(decode(&bad_tag).is_err());
    let mut long_name = original.clone();
    long_name[HEADER + 2..HEADER + 4].copy_from_slice(&129u16.to_le_bytes());
    rehash(&mut long_name);
    assert!(decode(&long_name).is_err());
    let mut trailing = original.clone();
    trailing.push(0);
    rehash(&mut trailing);
    assert!(decode(&trailing).is_err());
    let mut too_many_scopes = original.clone();
    let count_offset = HEADER + 2 + 2 + cached.theme.name.len() + 1 + UI_ROLE_COUNT * 4;
    too_many_scopes[count_offset..count_offset + 2].copy_from_slice(&((MAX_SCOPES + 1) as u16).to_le_bytes());
    rehash(&mut too_many_scopes);
    assert!(decode(&too_many_scopes).is_err());
}

#[cfg(unix)]
#[test]
fn cache_startup_refuses_fifo_and_symlinks_without_blocking_contract() {
    use std::{
        ffi::CString,
        os::unix::{ffi::OsStrExt, fs::symlink},
        process::{Command, Stdio},
        time::{Duration, Instant},
    };
    const CHILD: &str = "KWIL_THEME_CACHE_FIFO_CONTRACT_CHILD";
    // The test binary is its own bounded helper. It reads only one explicitly
    // named fixture, so a blocking open cannot hang the parent test process.
    if let Some(target) = std::env::var_os(CHILD) {
        let path = std::path::PathBuf::from(target);
        let root = path.parent().expect("fixture directory");
        assert_eq!(root.parent(), Some(std::env::temp_dir().canonicalize().unwrap().as_path()));
        assert!(root.file_name().unwrap().to_string_lossy().starts_with("kwil-theme-cache-fifo-"));
        assert!(!fs::symlink_metadata(root).unwrap().file_type().is_symlink());
        assert!(matches!(
            path.file_name().and_then(|n| n.to_str()),
            Some("cache.fifo" | "linked-fifo" | "linked-cache")
        ));
        assert!(read_active_cache(&path).is_err(), "nonregular/symlink cache must be rejected");
        return;
    }
    struct Root(std::path::PathBuf);
    impl Drop for Root {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
    let path = std::env::temp_dir().join(format!(
        "kwil-theme-cache-fifo-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    fs::create_dir(&path).unwrap();
    let root = Root(path.canonicalize().unwrap());
    let fifo = root.0.join("cache.fifo");
    let fifo_c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
    let linked_fifo = root.0.join("linked-fifo");
    symlink(&fifo, &linked_fifo).unwrap();
    let regular = root.0.join("regular.bin");
    let cached = CachedTheme { selection: ThemeSelection::Named(ThemeId::Dark), theme: Theme::builtin(Scheme::Dark) };
    fs::write(&regular, encode(&cached).unwrap()).unwrap();
    let linked_cache = root.0.join("linked-cache");
    symlink(&regular, &linked_cache).unwrap();
    for target in [&fifo, &linked_fifo, &linked_cache] {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "theme_cache::tests::cache_startup_refuses_fifo_and_symlinks_without_blocking_contract",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, target)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        eprintln!(
            "own cache fixture subprocess PID {}, timeout 2s: {}",
            child.id(),
            target.file_name().unwrap().to_string_lossy()
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "fixture subprocess failed: {status}");
                break;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("startup cache open blocked on {}; own subprocess was stopped", target.display());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
