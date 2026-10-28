//! DACL preservation is judged on the committed destination. ReplaceFileW can
//! preserve it even if setting it on the temporary file was refused.
use super::{Path, io, wide};
use windows_sys::Win32::{
    Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
    Security::*,
    Storage::FileSystem::*,
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

pub(super) struct Descriptor(Vec<u32>);

impl Descriptor {
    pub(super) fn read(path: &Path) -> io::Result<Self> {
        // A security descriptor is bounded independently of document size.
        let mut data = vec![0u32; 16384];
        let mut needed = 0;
        // SAFETY: terminated path and 64 KiB of aligned writable storage.
        if unsafe {
            GetFileSecurityW(
                wide(path).as_ptr(),
                OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                data.as_mut_ptr().cast(),
                65536,
                &mut needed,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(data))
    }

    fn ptr(&mut self) -> PSECURITY_DESCRIPTOR {
        self.0.as_mut_ptr().cast()
    }

    fn dacl(&mut self) -> io::Result<Option<&[u8]>> {
        let mut present = 0;
        let mut defaulted = 0;
        let mut acl = std::ptr::null_mut();
        // SAFETY: self owns the valid descriptor and the returned ACL storage.
        if unsafe { GetSecurityDescriptorDacl(self.ptr(), &mut present, &mut acl, &mut defaulted) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if present == 0 {
            return Ok(None);
        }
        if acl.is_null() {
            return Ok(Some(&[]));
        }
        // SAFETY: Windows validated the descriptor; AclSize covers this ACL.
        Ok(Some(unsafe { std::slice::from_raw_parts(acl.cast(), (*acl).AclSize as usize) }))
    }

    pub(super) fn copy_to(&mut self, target: &Path) {
        // Owner and group are independent of the DACL. Do not let a foreign
        // owner (e.g. Administrators/TrustedInstaller) block permission copying.
        // Never enable privileges: only transfer identities belonging to this
        // token, after obtaining WRITE_OWNER, and only if they actually differ.
        if let Ok(mut current) = Self::read(target) {
            self.copy_token_identity(target, &mut current);
        }
        let mut control = 0;
        let mut revision = 0;
        // SAFETY: a valid owned descriptor and writable scalar outputs.
        let ok = unsafe { GetSecurityDescriptorControl(self.ptr(), &mut control, &mut revision) };
        if ok == 0 {
            return;
        }
        let protection = if control & SE_DACL_PROTECTED != 0 {
            PROTECTED_DACL_SECURITY_INFORMATION
        } else {
            UNPROTECTED_DACL_SECURITY_INFORMATION
        };
        // SAFETY: valid descriptor and terminated path. Failure is not final:
        // ReplaceFileW also merges the original DACL; verify after replacement.
        unsafe { SetFileSecurityW(wide(target).as_ptr(), DACL_SECURITY_INFORMATION | protection, self.ptr()) };
    }

    fn copy_token_identity(&mut self, target: &Path, current: &mut Self) {
        let mut token = std::ptr::null_mut();
        // SAFETY: current process pseudo-handle and writable output handle.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return;
        }
        let mut flags = 0;
        for (kind, information, get) in [
            (TokenUser, OWNER_SECURITY_INFORMATION, GetSecurityDescriptorOwner as SidGetter),
            (TokenPrimaryGroup, GROUP_SECURITY_INFORMATION, GetSecurityDescriptorGroup as SidGetter),
        ] {
            let mut source = std::ptr::null_mut();
            let mut destination = std::ptr::null_mut();
            let mut defaulted = 0;
            // Pointer-bearing token structures require pointer-aligned storage.
            let mut identity = [0usize; 128];
            let mut needed = 0;
            // SAFETY: descriptors remain alive; each token structure begins
            // with a SID pointer whose storage remains in identity below.
            unsafe {
                if get(self.ptr(), &mut source, &mut defaulted) != 0
                    && get(current.ptr(), &mut destination, &mut defaulted) != 0
                    && !source.is_null()
                    && !destination.is_null()
                    && EqualSid(source, destination) == 0
                    && GetTokenInformation(
                        token,
                        kind,
                        identity.as_mut_ptr().cast(),
                        std::mem::size_of_val(&identity) as u32,
                        &mut needed,
                    ) != 0
                    && EqualSid(source, *(identity.as_ptr().cast::<PSID>())) != 0
                {
                    flags |= information;
                }
            }
        }
        // SAFETY: exactly the token handle opened above, never the pseudo-handle.
        unsafe { CloseHandle(token) };
        if flags == 0 {
            return;
        }
        // SAFETY: terminated path. Opening with WRITE_OWNER checks actual
        // access without changing privileges or attempting to take ownership.
        let handle = unsafe {
            CreateFileW(
                wide(target).as_ptr(),
                WRITE_OWNER,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            // SAFETY: owned live handle and valid descriptor. A filesystem can
            // still refuse; owner/group failures intentionally remain quiet.
            unsafe {
                SetKernelObjectSecurity(handle, flags, self.ptr());
                CloseHandle(handle);
            }
        }
    }

    pub(super) fn verify_dacl(mut self, path: &Path, warnings: &mut Vec<String>) {
        let result = Self::read(path).and_then(|mut saved| Ok(self.dacl()? == saved.dacl()?));
        match result {
            Ok(true) => {}
            Ok(false) => {
                warnings.push("saved file permissions differ: the original DACL could not be preserved".into())
            }
            Err(error) => warnings.push(format!("could not verify saved file permissions (DACL): {error}")),
        }
    }
}

type SidGetter = unsafe extern "system" fn(PSECURITY_DESCRIPTOR, *mut PSID, *mut i32) -> i32;
