//! Non-elevated Windows save security regressions.
use super::*;
use windows_sys::Win32::Security::*;

fn descriptor(path: &Path) -> Vec<u32> {
    let mut data = vec![0u32; 16384];
    let mut needed = 0;
    unsafe {
        assert_ne!(
            GetFileSecurityW(
                wide(path).as_ptr(),
                OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                data.as_mut_ptr().cast(),
                65536,
                &mut needed
            ),
            0,
            "{}",
            io::Error::last_os_error()
        );
    }
    data
}

fn dacl(data: &mut [u32]) -> Option<Vec<u8>> {
    let mut present = 0;
    let mut defaulted = 0;
    let mut acl = std::ptr::null_mut();
    unsafe {
        assert_ne!(GetSecurityDescriptorDacl(data.as_mut_ptr().cast(), &mut present, &mut acl, &mut defaulted), 0);
        if present == 0 {
            return None;
        }
        if acl.is_null() {
            return Some(Vec::new());
        }
        Some(std::slice::from_raw_parts(acl.cast(), (*acl).AclSize as usize).to_vec())
    }
}

fn owner(data: &mut [u32]) -> Vec<u8> {
    let mut sid = std::ptr::null_mut();
    let mut defaulted = 0;
    unsafe {
        assert_ne!(GetSecurityDescriptorOwner(data.as_mut_ptr().cast(), &mut sid, &mut defaulted), 0);
        std::slice::from_raw_parts(sid.cast(), GetLengthSid(sid) as usize).to_vec()
    }
}

#[test]
fn windows_user_owned_save_is_quiet_and_preserves_dacl() {
    let f = Fixture::new();
    fs::write(f.path(), b"old").unwrap();
    let mut before = descriptor(&f.path());
    // Accept the owner assigned by Windows policy (user or Administrators).
    // A normal filtered token must never need WRITE_OWNER just to set up a test.
    let actual_owner = owner(&mut before);
    assert!(!actual_owner.is_empty());
    println!("Testing save with the OS-assigned owner ({} SID bytes)", actual_owner.len());
    let original = open_for_editing(&f.path()).unwrap();
    let mut buffer = Buffer::from_file(original.clone()).unwrap();
    buffer.insert(0, b"new ").unwrap();
    let saved = save_snapshot(f.request(original, &buffer)).unwrap();
    // Windows can materialize inherited duplicates. Production verification
    // compares permissions while the independent raw descriptor read ensures
    // we are checking the actual destination, not the pre-replace temp.
    assert!(
        windows_security::same_dacl(dacl(&mut before).as_deref(), dacl(&mut descriptor(&f.path())).as_deref(),)
            .unwrap()
    );
    assert_eq!(fs::read(f.path()).unwrap(), b"new old");
    assert!(saved.warnings.is_empty(), "{:?}", saved.warnings);
}

fn foreign_source() -> PathBuf {
    PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32/notepad.exe")
}

#[test]
fn windows_foreign_owner_copy_is_quiet_when_dacl_is_preserved() {
    let f = Fixture::new();
    fs::write(f.path(), b"replacement").unwrap();
    // Read only: never modify the Windows installation or request elevation.
    let source = foreign_source();
    let mut before = descriptor(&source);
    assert_ne!(owner(&mut before), owner(&mut descriptor(&f.path())));
    let mut warnings = Vec::new();
    preserve_metadata(&source, &f.path(), &fs::metadata(&source).unwrap(), &mut warnings).unwrap();
    assert_eq!(dacl(&mut descriptor(&f.path())), dacl(&mut before));
    assert!(warnings.is_empty(), "{warnings:?}");
}

#[test]
fn windows_save_warns_when_final_dacl_is_not_preserved() {
    let f = Fixture::new();
    fs::write(f.path(), b"old").unwrap();
    let original = open_for_editing(&f.path()).unwrap();
    let buffer = Buffer::from_file(original.clone()).unwrap();
    let saved = commit_with_operations(
        f.request(original, &buffer),
        |s, f| s.write_to(f).map(|_| None),
        |_| Ok(()),
        |temp, destination, backup| {
            replace(temp, destination, backup)?;
            // Simulate a filesystem/replacement that did not retain the DACL.
            // A null DACL grants access only on this disposable test file.
            let mut sd = [0usize; 8];
            unsafe {
                assert_ne!(InitializeSecurityDescriptor(sd.as_mut_ptr().cast(), 1), 0);
                assert_ne!(SetSecurityDescriptorDacl(sd.as_mut_ptr().cast(), 1, std::ptr::null_mut(), 0), 0);
                assert_ne!(
                    SetFileSecurityW(
                        wide(destination).as_ptr(),
                        DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                        sd.as_mut_ptr().cast()
                    ),
                    0
                );
            }
            Ok(())
        },
        sync_parent,
    )
    .unwrap();
    assert!(saved.warnings.iter().any(|w| w.contains("DACL")), "{:?}", saved.warnings);
}

#[test]
fn windows_replacefile_security_probe() {
    let f = Fixture::new();
    fs::write(f.path(), b"old").unwrap();
    let actual_owner = owner(&mut descriptor(&f.path()));
    assert!(!actual_owner.is_empty());
    println!("Testing replacement with the OS-assigned owner ({} SID bytes)", actual_owner.len());
    // Give the original a different DACL from a newly created sibling, so
    // this detects preservation rather than comparing identical defaults.
    let mut foreign = descriptor(&foreign_source());
    unsafe {
        assert_ne!(
            SetFileSecurityW(
                wide(&f.path()).as_ptr(),
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                foreign.as_mut_ptr().cast()
            ),
            0
        );
    }
    let replacement = f.0.join("replacement");
    fs::write(&replacement, b"new").unwrap();
    let mut before = descriptor(&f.path());
    let mut temp = descriptor(&replacement);
    assert_ne!(dacl(&mut before), dacl(&mut temp));
    replace(&replacement, &f.path(), Some(&f.0.join("backup"))).unwrap();
    let mut after = descriptor(&f.path());
    assert_eq!(dacl(&mut after), dacl(&mut before));
    println!("ReplaceFileW preserves a distinct original DACL without manual copying");
}
