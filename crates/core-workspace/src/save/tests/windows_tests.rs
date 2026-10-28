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
    assert!(
        windows_security::same_dacl(dacl(&mut descriptor(&f.path())).as_deref(), dacl(&mut before).as_deref()).unwrap()
    );
    assert!(warnings.is_empty(), "{warnings:?}");
}

#[test]
fn windows_save_warns_when_final_dacl_is_not_preserved() {
    let f = Fixture::new();
    fs::write(f.path(), b"old").unwrap();
    let original = open_for_editing(&f.path()).unwrap();
    let buffer = Buffer::from_file(original.clone()).unwrap();
    let saved = windows_security::with_failed_dacl_copy(|| {
        commit_with_operations(
            f.request(original, &buffer),
            |s, f| s.write_to(f).map(|_| None),
            |_| Ok(()),
            |temp, destination, backup| {
                replace(temp, destination, backup)?;
                // The OS changes the DACL and the product's explicit copying is
                // injected to fail. The final mismatch must reach the frontend.
                set_null_dacl(destination);
                Ok(())
            },
            sync_parent,
        )
    })
    .unwrap();
    assert!(saved.warnings.iter().any(|w| w.contains("DACL")), "{:?}", saved.warnings);
}

fn set_distinct_protected_dacl(path: &Path) {
    // Keep this filtered token's user fully authorized to replace the file.
    // A small Authenticated Users grant makes the protected DACL distinct
    // from a newly created sibling without importing a system file's policy.
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        Storage::FileSystem::{FILE_ALL_ACCESS, FILE_READ_DATA},
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };
    let mut token = std::ptr::null_mut();
    let mut user = [0usize; 128];
    let mut needed = 0;
    let mut authenticated_users = [0u32; 17];
    let mut sid_size = std::mem::size_of_val(&authenticated_users) as u32;
    let mut acl = [0u32; 256];
    let mut sd = [0usize; 8];
    unsafe {
        assert_ne!(OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token), 0);
        let ok = GetTokenInformation(
            token,
            TokenUser,
            user.as_mut_ptr().cast(),
            std::mem::size_of_val(&user) as u32,
            &mut needed,
        );
        CloseHandle(token);
        assert_ne!(ok, 0);
        let user_sid = (*(user.as_ptr().cast::<TOKEN_USER>())).User.Sid;
        assert_ne!(
            CreateWellKnownSid(
                WinAuthenticatedUserSid,
                std::ptr::null_mut(),
                authenticated_users.as_mut_ptr().cast(),
                &mut sid_size
            ),
            0
        );
        assert_ne!(InitializeAcl(acl.as_mut_ptr().cast(), std::mem::size_of_val(&acl) as u32, 2), 0);
        assert_ne!(AddAccessAllowedAceEx(acl.as_mut_ptr().cast(), 2, 0, FILE_ALL_ACCESS, user_sid), 0);
        assert_ne!(
            AddAccessAllowedAceEx(
                acl.as_mut_ptr().cast(),
                2,
                0,
                FILE_READ_DATA,
                authenticated_users.as_mut_ptr().cast()
            ),
            0
        );
        assert_ne!(InitializeSecurityDescriptor(sd.as_mut_ptr().cast(), 1), 0);
        assert_ne!(SetSecurityDescriptorDacl(sd.as_mut_ptr().cast(), 1, acl.as_ptr().cast(), 0), 0);
        assert_ne!(
            SetFileSecurityW(
                wide(path).as_ptr(),
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                sd.as_mut_ptr().cast()
            ),
            0
        );
    }
}

#[test]
fn windows_full_save_preserves_protected_dacl_without_warning() {
    let f = Fixture::new();
    fs::write(f.path(), b"old").unwrap();
    set_distinct_protected_dacl(&f.path());
    let mut before = descriptor(&f.path());
    let original = open_for_editing(&f.path()).unwrap();
    let mut buffer = Buffer::from_file(original.clone()).unwrap();
    buffer.insert(0, b"new ").unwrap();
    let saved = save_snapshot(f.request(original, &buffer)).unwrap();
    let mut after = descriptor(&f.path());
    assert!(windows_security::same_dacl(dacl(&mut before).as_deref(), dacl(&mut after).as_deref()).unwrap());
    assert!(saved.warnings.is_empty(), "{:?}", saved.warnings);
    assert_eq!(fs::read(f.path()).unwrap(), b"new old");
}

fn set_null_dacl(path: &Path) {
    let mut sd = [0usize; 8];
    unsafe {
        assert_ne!(InitializeSecurityDescriptor(sd.as_mut_ptr().cast(), 1), 0);
        assert_ne!(SetSecurityDescriptorDacl(sd.as_mut_ptr().cast(), 1, std::ptr::null_mut(), 0), 0);
        assert_ne!(
            SetFileSecurityW(
                wide(path).as_ptr(),
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                sd.as_mut_ptr().cast()
            ),
            0
        );
    }
}

#[test]
fn windows_save_repairs_dacl_changed_by_replacement() {
    let f = Fixture::new();
    fs::write(f.path(), b"old").unwrap();
    set_distinct_protected_dacl(&f.path());
    let mut before = descriptor(&f.path());
    let original = open_for_editing(&f.path()).unwrap();
    let buffer = Buffer::from_file(original.clone()).unwrap();
    let saved = commit_with_operations(
        f.request(original, &buffer),
        |s, f| s.write_to(f).map(|_| None),
        |_| Ok(()),
        |temp, destination, backup| {
            replace(temp, destination, backup)?;
            // Deterministically model OS-dependent replacement ACL changes.
            set_null_dacl(destination);
            Ok(())
        },
        sync_parent,
    )
    .unwrap();
    assert!(
        windows_security::same_dacl(dacl(&mut before).as_deref(), dacl(&mut descriptor(&f.path())).as_deref()).unwrap()
    );
    assert!(saved.warnings.is_empty(), "{:?}", saved.warnings);
}
