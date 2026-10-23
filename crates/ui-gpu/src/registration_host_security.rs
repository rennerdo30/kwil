//! Bounded, pure parsing of OS-returned self-relative descriptors and SIDs.
//! Kept separate so hostile descriptor fixtures need no Windows/native calls.
use std::path::{Path, PathBuf};

/// Operations retain the same native guards in both branches. Implementations
/// must pin and review original+canonical ancestry before returning an existing
/// directory, and create/reopen/verify a private child without changing an
/// already-existing object's ACL. Missing intermediate ancestors must fail.
pub(super) trait ProfileDirectories {
    type Guard;
    fn present(&mut self, path: &Path) -> Result<bool, String>;
    fn pin_reviewed_existing(&mut self, path: &Path) -> Result<(PathBuf, Vec<Self::Guard>), String>;
    fn create_private_verified(&mut self, path: &Path) -> Result<Self::Guard, String>;
}

/// Exactly one missing final profile directory is supported, never a search for
/// another profile or an existing ancestor higher in the tree. Only NotFound
/// may select the creation branch; permission/type/reparse errors must propagate.
pub(super) fn profile_parent<B: ProfileDirectories>(
    backend: &mut B,
    parent: &Path,
) -> Result<(PathBuf, Vec<B::Guard>), String> {
    if backend.present(parent)? {
        return backend.pin_reviewed_existing(parent);
    }
    let leaf = parent.file_name().ok_or("Settings parent needs a final directory name")?;
    let ancestor = parent.parent().ok_or("Settings parent needs an existing ancestor")?;
    let (canonical, mut guards) = backend.pin_reviewed_existing(ancestor)?;
    let child = canonical.join(leaf);
    guards.push(backend.create_private_verified(&child)?);
    Ok((child, guards))
}

pub(super) const MAX_DESCRIPTOR: usize = 65_536;
const FULL_FILE_ACCESS: u32 = 0x001f_01ff;
const MUTATION: u32 = 0x500d_0156; // generic all/write, delete, owner/DACL, file writes

pub(super) fn sid(bytes: &[u8]) -> Result<&[u8], String> {
    if bytes.len() < 8 || bytes[0] != 1 || bytes[1] > 15 {
        return Err("Invalid bounded SID".into());
    }
    bytes.get(..8 + 4 * usize::from(bytes[1])).ok_or_else(|| "Truncated SID".into())
}

pub(super) fn sid_text(bytes: &[u8]) -> Result<String, String> {
    let bytes = sid(bytes)?;
    let authority = bytes[2..8].iter().fold(0u64, |n, b| (n << 8) | u64::from(*b));
    let mut text = format!("S-1-{authority}");
    for part in bytes[8..].as_chunks::<4>().0 {
        text.push_str(&format!("-{}", u32::from_le_bytes(*part)));
    }
    Ok(text)
}

fn word(bytes: &[u8], at: usize) -> Result<u16, String> {
    let b = bytes.get(at..at + 2).ok_or("Truncated security descriptor")?;
    Ok(u16::from_le_bytes([b[0], b[1]]))
}
fn dword(bytes: &[u8], at: usize) -> Result<u32, String> {
    let b = bytes.get(at..at + 4).ok_or("Truncated security descriptor")?;
    Ok(u32::from_le_bytes(b.try_into().unwrap()))
}

/// Private directory: exactly one inheritable full-control user ACE, protected
/// from parent inheritance. Existing records: same sole user, inherited allowed.
/// Settings parent: owned by user, no other untrusted principal can mutate it.
/// SYSTEM/admin are trusted for the settings parent only. No ACL is repaired.
pub(super) fn review(bytes: &[u8], user: &[u8], private: bool, directory: bool) -> Result<(), String> {
    let user = sid(user)?;
    if bytes.len() < 20 || bytes.len() > MAX_DESCRIPTOR || bytes[0] != 1 {
        return Err("Invalid bounded security descriptor".into());
    }
    let control = word(bytes, 2)?;
    if control & 0x8004 != 0x8004 || (private && directory && control & 0x1000 == 0) {
        return Err("Missing self-relative, present, protected DACL".into());
    }
    let owner = dword(bytes, 4)? as usize;
    if owner < 20 || sid(bytes.get(owner..).ok_or("Invalid owner offset")?)? != user {
        return Err("Registration storage is not owned by the token user".into());
    }
    let offset = dword(bytes, 16)? as usize;
    if offset < 20 {
        return Err("Null or invalid DACL".into());
    }
    let acl = bytes.get(offset..).ok_or("Invalid DACL offset")?;
    if acl.len() < 8 || !matches!(acl[0], 2 | 4) {
        return Err("Invalid ACL revision/header".into());
    }
    let size = usize::from(word(acl, 2)?);
    let acl = acl.get(..size).filter(|b| b.len() >= 8).ok_or("Truncated ACL")?;
    let count = usize::from(word(acl, 4)?);
    if count > (acl.len() - 8) / 8 || (private && count != 1) {
        return Err("Unexpected ACE count".into());
    }
    let mut cursor = 8;
    for _ in 0..count {
        let ace_size = usize::from(word(acl, cursor + 2)?);
        if ace_size < 16 || ace_size % 4 != 0 {
            return Err("Invalid ACE size".into());
        }
        let ace = acl.get(cursor..cursor + ace_size).ok_or("Truncated ACE")?;
        if !matches!(ace[0], 0 | 1) || ace[1] & !0x1f != 0 {
            return Err("Unsupported ACE".into());
        }
        let trustee = sid(&ace[8..])?;
        if trustee.len() + 8 != ace.len() {
            return Err("Unexpected ACE payload".into());
        }
        let mask = dword(ace, 4)?;
        if private {
            if ace[0] != 0
                || trustee != user
                || mask != FULL_FILE_ACCESS
                || ace[1] & 0x0c != 0
                || (directory && ace[1] & 3 != 3)
            {
                return Err("Journal ACL must grant only the token user full inheritable access".into());
            }
        } else if ace[0] == 0 && ace[1] & 8 == 0 && mask & MUTATION != 0 && trustee != user {
            let text = sid_text(trustee)?;
            if text != "S-1-5-18" && text != "S-1-5-32-544" {
                return Err("Settings parent permits mutation by another principal".into());
            }
        }
        cursor += ace_size;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    const USER: &[u8] = &[1, 1, 0, 0, 0, 0, 0, 5, 21, 0, 0, 0];
    fn descriptor(flags: u8, mask: u32) -> Vec<u8> {
        let mut b = vec![0; 20];
        b[0] = 1;
        b[2..4].copy_from_slice(&0x9004u16.to_le_bytes());
        b[4..8].copy_from_slice(&20u32.to_le_bytes());
        b.extend_from_slice(USER);
        let offset = b.len() as u32;
        b[16..20].copy_from_slice(&offset.to_le_bytes());
        b.extend_from_slice(&[2, 0, 28, 0, 1, 0, 0, 0, 0, flags, 20, 0]);
        b.extend_from_slice(&mask.to_le_bytes());
        b.extend_from_slice(USER);
        b
    }
    fn append_ace(b: &mut Vec<u8>, trustee: &[u8], flags: u8, mask: u32) {
        let offset = dword(b, 16).unwrap() as usize;
        let count = word(&b[offset..], 4).unwrap() + 1;
        let size = (8 + trustee.len()) as u16;
        b.extend_from_slice(&[0, flags]);
        b.extend_from_slice(&size.to_le_bytes());
        b.extend_from_slice(&mask.to_le_bytes());
        b.extend_from_slice(trustee);
        let total = (b.len() - offset) as u16;
        b[offset + 2..offset + 4].copy_from_slice(&total.to_le_bytes());
        b[offset + 4..offset + 6].copy_from_slice(&count.to_le_bytes());
    }
    /// Synthetic conventional inherited ACL shape, not an owner-profile sample:
    /// token user, SYSTEM and administrators full control; creator-owner only
    /// inherits to children. The policy has not changed from R1.
    fn appdata_descriptor() -> Vec<u8> {
        let mut b = descriptor(0x13, FULL_FILE_ACCESS);
        b[2..4].copy_from_slice(&0x8004u16.to_le_bytes());
        append_ace(&mut b, &[1, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0], 0x13, FULL_FILE_ACCESS);
        append_ace(&mut b, &[1, 2, 0, 0, 0, 0, 0, 5, 32, 0, 0, 0, 32, 2, 0, 0], 0x13, FULL_FILE_ACCESS);
        append_ace(&mut b, &[1, 1, 0, 0, 0, 0, 0, 3, 0, 0, 0, 0], 0x1b, FULL_FILE_ACCESS);
        b
    }

    #[derive(Clone)]
    struct Node {
        acl: Vec<u8>,
        reparse: bool,
    }
    struct FakeDirectories {
        nodes: std::collections::BTreeMap<PathBuf, Node>,
        operations: Vec<(&'static str, PathBuf)>,
        raced_child: Option<Node>,
        probe_error: bool,
    }
    impl FakeDirectories {
        fn new() -> Self {
            Self {
                nodes: std::collections::BTreeMap::new(),
                operations: Vec::new(),
                raced_child: None,
                probe_error: false,
            }
        }
        fn put(&mut self, path: &Path, acl: Vec<u8>) {
            self.nodes.insert(path.to_owned(), Node { acl, reparse: false });
        }
        fn creations(&self) -> usize {
            self.operations.iter().filter(|(op, _)| *op == "create").count()
        }
    }
    impl ProfileDirectories for FakeDirectories {
        type Guard = PathBuf;
        fn present(&mut self, path: &Path) -> Result<bool, String> {
            self.operations.push(("present", path.to_owned()));
            if self.probe_error {
                return Err("injected access denied".into());
            }
            Ok(self.nodes.contains_key(path))
        }
        fn pin_reviewed_existing(&mut self, path: &Path) -> Result<(PathBuf, Vec<PathBuf>), String> {
            self.operations.push(("pin-review", path.to_owned()));
            let node = self.nodes.get(path).ok_or("missing immediate ancestor")?;
            if node.reparse {
                return Err("reparse point".into());
            }
            review(&node.acl, USER, false, true)?;
            Ok((path.to_owned(), vec![path.to_owned()]))
        }
        fn create_private_verified(&mut self, path: &Path) -> Result<PathBuf, String> {
            self.operations.push(("create", path.to_owned()));
            let candidate =
                self.raced_child.take().unwrap_or(Node { acl: descriptor(3, FULL_FILE_ACCESS), reparse: false });
            let node = self.nodes.entry(path.to_owned()).or_insert(candidate);
            self.operations.push(("reopen-verify", path.to_owned()));
            if node.reparse {
                return Err("reparse point on reopen".into());
            }
            review(&node.acl, USER, true, true)?;
            Ok(path.to_owned())
        }
    }
    #[test]
    fn first_run_creates_only_final_profile_under_reviewed_appdata() {
        let ancestor = Path::new("/profile/AppData/Roaming");
        let parent = ancestor.join("kwil");
        let mut fake = FakeDirectories::new();
        fake.put(ancestor, appdata_descriptor());
        let (actual, guards) = profile_parent(&mut fake, &parent).unwrap();
        assert_eq!(actual, parent);
        assert_eq!(guards, [ancestor.to_owned(), parent.clone()]);
        assert_eq!(
            fake.operations,
            [
                ("present", parent.clone()),
                ("pin-review", ancestor.to_owned()),
                ("create", parent.clone()),
                ("reopen-verify", parent.clone())
            ]
        );
        review(&fake.nodes[&parent].acl, USER, true, true).unwrap();
        assert_eq!(fake.nodes.len(), 2);
    }
    #[test]
    fn existing_profile_is_reviewed_without_creation_or_acl_changes() {
        let parent = Path::new("/profile/AppData/Roaming/kwil");
        let original = appdata_descriptor();
        let mut fake = FakeDirectories::new();
        fake.put(parent, original.clone());
        assert_eq!(profile_parent(&mut fake, parent).unwrap().0, parent);
        assert_eq!(fake.creations(), 0);
        assert_eq!(fake.nodes[parent].acl, original);
    }
    #[test]
    fn two_missing_levels_do_not_search_upward_or_create_a_chain() {
        let mut fake = FakeDirectories::new();
        fake.put(Path::new("/profile/AppData"), appdata_descriptor());
        assert!(profile_parent(&mut fake, Path::new("/profile/AppData/missing/kwil")).is_err());
        assert_eq!(fake.creations(), 0);
        assert_eq!(fake.operations.len(), 2);
        assert_eq!(fake.nodes.len(), 1);
    }
    #[test]
    fn untrusted_or_reparse_ancestor_never_reaches_creation() {
        let ancestor = Path::new("/profile/AppData/Roaming");
        for reparse in [false, true] {
            let mut fake = FakeDirectories::new();
            let mut acl = appdata_descriptor();
            if !reparse {
                append_ace(&mut acl, &[1, 1, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0], 0, 2);
            }
            fake.put(ancestor, acl);
            fake.nodes.get_mut(ancestor).unwrap().reparse = reparse;
            assert!(profile_parent(&mut fake, &ancestor.join("kwil")).is_err());
            assert_eq!(fake.creations(), 0);
        }
    }
    #[test]
    fn unsafe_existing_profile_is_never_repaired_or_redirected() {
        let parent = Path::new("/profile/AppData/Roaming/kwil");
        let mut acl = appdata_descriptor();
        acl[20 + 8] ^= 1; // Foreign descriptor owner, not an ACE edit.
        let mut fake = FakeDirectories::new();
        fake.put(parent, acl.clone());
        assert!(profile_parent(&mut fake, parent).is_err());
        assert_eq!(fake.creations(), 0);
        assert_eq!(fake.nodes[parent].acl, acl);
    }
    #[test]
    fn raced_existing_child_must_pass_private_reopen_review() {
        let ancestor = Path::new("/profile/AppData/Roaming");
        let parent = ancestor.join("kwil");
        for reparse in [false, true] {
            let mut fake = FakeDirectories::new();
            fake.put(ancestor, appdata_descriptor());
            let acl = appdata_descriptor();
            fake.raced_child = Some(Node { acl: acl.clone(), reparse });
            assert!(profile_parent(&mut fake, &parent).is_err());
            assert_eq!(fake.creations(), 1);
            assert_eq!(fake.nodes[&parent].acl, acl);
        }
    }
    #[test]
    fn probe_errors_do_not_become_missing_profile_fallback() {
        let mut fake = FakeDirectories::new();
        fake.probe_error = true;
        assert_eq!(
            profile_parent(&mut fake, Path::new("/profile/AppData/Roaming/kwil")).unwrap_err(),
            "injected access denied"
        );
        assert_eq!(fake.operations.len(), 1);
        assert_eq!(fake.creations(), 0);
    }
    #[test]
    fn accepts_exact_private_descriptor_and_sid_text() {
        assert_eq!(sid_text(USER).unwrap(), "S-1-5-21");
        assert!(review(&descriptor(3, FULL_FILE_ACCESS), USER, true, true).is_ok());
    }
    #[test]
    fn rejects_every_truncation_and_null_or_unprotected_dacl() {
        let good = descriptor(3, FULL_FILE_ACCESS);
        for end in 0..good.len() {
            assert!(review(&good[..end], USER, true, true).is_err(), "{end}");
        }
        for (at, value) in [(16, 0u32), (2, 0x8004u32)] {
            let mut bad = good.clone();
            if at == 2 {
                bad[2..4].copy_from_slice(&(value as u16).to_le_bytes());
            } else {
                bad[at..at + 4].copy_from_slice(&value.to_le_bytes());
            }
            assert!(review(&bad, USER, true, true).is_err());
        }
    }
    #[test]
    fn rejects_noninheritance_inherit_only_and_foreign_journal_ace() {
        for flags in [0, 1, 2, 11] {
            assert!(review(&descriptor(flags, FULL_FILE_ACCESS), USER, true, true).is_err());
        }
        let mut bad = descriptor(3, FULL_FILE_ACCESS);
        *bad.last_mut().unwrap() = 1;
        assert!(review(&bad, USER, true, true).is_err());
        assert!(review(&descriptor(3, 0x1f01fe), USER, true, true).is_err());
    }
    #[test]
    fn rejects_foreign_parent_writer_but_allows_read_only() {
        let mut b = descriptor(0, 2);
        *b.last_mut().unwrap() = 1;
        assert!(review(&b, USER, false, true).is_err());
        let mask_at = 20 + USER.len() + 12;
        b[mask_at..mask_at + 4].copy_from_slice(&0x0012_0089u32.to_le_bytes());
        assert!(review(&b, USER, false, true).is_ok());
    }
    #[test]
    fn sid_length_and_authority_are_bounded() {
        for b in [&[][..], &[1, 16, 0, 0, 0, 0, 0, 5][..], &USER[..11]] {
            assert!(sid(b).is_err());
        }
        assert!(review(&vec![0; MAX_DESCRIPTOR + 1], USER, true, true).is_err());
    }
}
