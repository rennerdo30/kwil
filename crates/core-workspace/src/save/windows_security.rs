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

    pub(super) fn verify_dacl(self, path: &Path, warnings: &mut Vec<String>) {
        self.verify_against(Self::read(path), warnings);
    }

    fn verify_against(mut self, saved: io::Result<Self>, warnings: &mut Vec<String>) {
        let result = saved.and_then(|mut saved| same_dacl(self.dacl()?, saved.dacl()?));
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

#[derive(Debug, PartialEq, Eq)]
enum Permissions {
    Unrestricted,
    Entries(Vec<AceRun>),
}

#[derive(Debug, PartialEq, Eq)]
enum AceRun {
    // Within consecutive allows (or denies), order and duplicate entries do
    // not affect access checks. Never commute an allow across a deny: SIDs
    // can overlap through group membership, changing the access decision.
    Standard { kind: u8, rights: std::collections::BTreeMap<(Vec<u8>, u8), u32> },
    // Object/callback/unknown ACEs carry additional semantics. Retain their
    // complete payload and position rather than guessing at equivalence.
    Other(Vec<u8>),
}

pub(super) fn same_dacl(before: Option<&[u8]>, after: Option<&[u8]>) -> io::Result<bool> {
    Ok(normalize_dacl(before)? == normalize_dacl(after)?)
}

fn normalize_dacl(acl: Option<&[u8]>) -> io::Result<Permissions> {
    let Some(acl) = acl.filter(|acl| !acl.is_empty()) else {
        return Ok(Permissions::Unrestricted);
    };
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid DACL or ACE layout");
    if acl.len() < 8 {
        return Err(invalid());
    }
    let count = u16::from_le_bytes([acl[4], acl[5]]);
    let mut offset = 8;
    let mut runs = Vec::new();
    let mut seen = std::collections::BTreeMap::<_, u32>::new();
    for _ in 0..count {
        let header = acl.get(offset..offset + 4).ok_or_else(invalid)?;
        let size = u16::from_le_bytes([header[2], header[3]]) as usize;
        if size < 4 {
            return Err(invalid());
        }
        let ace = acl.get(offset..offset + size).ok_or_else(invalid)?;
        offset += size;
        let kind = ace[0];
        if kind != 0 && kind != 1 {
            // Unknown ACEs may alter the access-check context.
            seen.clear();
            runs.push(AceRun::Other(ace.to_vec()));
            continue;
        }
        // ACCESS_ALLOWED_ACE and ACCESS_DENIED_ACE: header, mask, SID.
        if ace.len() < 16 {
            return Err(invalid());
        }
        let sid_len = 8 + 4 * ace[9] as usize;
        let sid = ace.get(8..8 + sid_len).ok_or_else(invalid)?.to_vec();
        let mut mask = u32::from_le_bytes(ace[4..8].try_into().unwrap());
        // INHERITED_ACE records provenance, not access semantics. All actual
        // inheritance/propagation flags remain part of the permission key.
        let flags = ace[1] & !(INHERITED_ACE as u8);
        // A repeated grant/denial to the same trustee cannot affect bits the
        // earlier ACE already decided, even across a run of the other type.
        // Subtract only identical trustee/type/propagation keys, never infer
        // group membership or commute a new permission across a deny.
        let previous = seen.entry((kind, sid.clone(), flags)).or_default();
        let original_mask = mask;
        mask &= !*previous;
        *previous |= original_mask;
        if mask == 0 {
            continue;
        }
        if !matches!(runs.last(), Some(AceRun::Standard { kind: previous, .. }) if *previous == kind) {
            runs.push(AceRun::Standard { kind, rights: Default::default() });
        }
        if let Some(AceRun::Standard { rights, .. }) = runs.last_mut() {
            *rights.entry((sid, flags)).or_default() |= mask;
        }
    }
    // Ignore unused allocation bytes beyond the last ACE, including ACL size
    // and reserved fields. An empty ACL still denies all (unlike a null DACL).
    Ok(Permissions::Entries(runs))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real self-relative security descriptors, including ACL allocation slack.
    fn descriptor(aces: &[(u8, u8, u32, u32)], slack: usize) -> Descriptor {
        let size = 8 + aces.len() * 20 + slack;
        let mut bytes = vec![0u8; 20 + size];
        bytes[0] = 1;
        bytes[2..4].copy_from_slice(&(SE_SELF_RELATIVE | SE_DACL_PRESENT).to_le_bytes());
        bytes[16..20].copy_from_slice(&20u32.to_le_bytes());
        bytes[20] = 2;
        bytes[22..24].copy_from_slice(&(size as u16).to_le_bytes());
        bytes[24..26].copy_from_slice(&(aces.len() as u16).to_le_bytes());
        for (index, &(kind, flags, mask, trustee)) in aces.iter().enumerate() {
            let ace = &mut bytes[28 + index * 20..][..20];
            ace[0] = kind;
            ace[1] = flags;
            ace[2..4].copy_from_slice(&20u16.to_le_bytes());
            ace[4..8].copy_from_slice(&mask.to_le_bytes());
            ace[8..16].copy_from_slice(&[1, 1, 0, 0, 0, 0, 0, 5]);
            ace[16..20].copy_from_slice(&trustee.to_le_bytes());
        }
        bytes[28 + aces.len() * 20..].fill(0xa5);
        Descriptor(bytes.as_chunks::<4>().0.iter().map(|b| u32::from_le_bytes(*b)).collect())
    }

    fn warnings(before: &[(u8, u8, u32, u32)], after: &[(u8, u8, u32, u32)], slack: usize) -> Vec<String> {
        let mut warnings = Vec::new();
        descriptor(before, 0).verify_against(Ok(descriptor(after, slack)), &mut warnings);
        warnings
    }

    #[test]
    fn equivalent_inherited_duplicates_and_unused_acl_bytes_are_quiet() {
        let before = [(0, 0, 3, 18), (0, 0, 7, 11)];
        let after = [(0, 16, 7, 11), (0, 0, 3, 18), (0, 16, 3, 18), (0, 0, 7, 11)];
        assert!(warnings(&before, &after, 16).is_empty());
        assert!(warnings(&before, &before, 16).is_empty());
        let interleaved = [(0, 0, 3, 18), (1, 0, 1, 11)];
        assert!(warnings(&interleaved, &[interleaved[0], interleaved[1], (0, 16, 3, 18)], 0).is_empty());
    }

    #[test]
    fn missing_or_extra_allow_warns() {
        let before = [(0, 0, 3, 18), (0, 0, 7, 11)];
        assert!(!warnings(&before, &before[..1], 0).is_empty());
        assert!(!warnings(&before[..1], &before, 0).is_empty());
    }

    #[test]
    fn changed_deny_and_changed_allow_deny_precedence_warn() {
        let before = [(1, 0, 1, 18), (0, 0, 3, 18)];
        assert!(!warnings(&before, &[(1, 0, 2, 18), before[1]], 0).is_empty());
        assert!(!warnings(&before, &[before[1], before[0]], 0).is_empty());
        assert!(!warnings(&[(0, 0, 3, 18)], &[(0, INHERIT_ONLY_ACE as u8, 3, 18)], 0).is_empty());
    }
}
