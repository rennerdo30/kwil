use kwil_core_workspace::{
    session::{Session, load_session, store_session},
    window_session::{WindowSession, claim_window, restorable_windows},
};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "kwil-window-sessions-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        Self(root)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        assert!(self.0.is_absolute());
        assert_eq!(self.0.parent(), Some(std::env::temp_dir().as_path()));
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn separate_windows_preserve_legacy_metadata_and_never_share_journal_ownership() {
    let root = Scratch::new();
    let mut legacy = Session::default();
    legacy.open(None).unwrap();
    store_session(&root.0.join("session.kws"), &legacy).unwrap();
    let primary = claim_window(&root.0, &WindowSession::Primary).unwrap();
    let secondary = claim_window(&root.0, &WindowSession::New).unwrap();
    let id = secondary.root().file_name().unwrap().to_str().unwrap().to_owned();
    assert_ne!(secondary.root(), primary.root());
    assert_eq!(load_session(&primary.root().join("session.kws")).unwrap(), legacy);
    let mut tabs = Session::default();
    let first = tabs.open(None).unwrap();
    let second = tabs.open(None).unwrap();
    tabs.activate(first);
    tabs.tab_mut(second).unwrap().cursor = 37;
    store_session(&secondary.root().join("session.kws"), &tabs).unwrap();
    // Both sessions may allocate document 1, but their recovery bytes are disjoint.
    std::fs::write(primary.root().join("1.kwj"), b"primary journal").unwrap();
    std::fs::write(secondary.root().join("1.kwj"), b"secondary journal").unwrap();
    assert!(claim_window(&root.0, &WindowSession::Primary).is_err());
    assert!(claim_window(&root.0, &WindowSession::Restore(id.clone())).is_err());
    assert!(restorable_windows(&primary).unwrap().is_empty(), "live windows must not be relaunched");
    drop(secondary);
    assert_eq!(restorable_windows(&primary).unwrap(), vec![id.clone()]);
    let restored = claim_window(&root.0, &WindowSession::Restore(id)).unwrap();
    assert_eq!(load_session(&restored.root().join("session.kws")).unwrap(), tabs);
    assert_eq!(std::fs::read(restored.root().join("1.kwj")).unwrap(), b"secondary journal");
    assert_eq!(std::fs::read(primary.root().join("1.kwj")).unwrap(), b"primary journal");
    assert_eq!(load_session(&primary.root().join("session.kws")).unwrap(), legacy);
}

#[test]
fn restore_refuses_invalid_and_missing_window_ids_without_creating_a_store() {
    let root = Scratch::new();
    for id in ["../primary", "window-../../", "window-", "window-123"] {
        assert!(claim_window(&root.0, &WindowSession::Restore(id.into())).is_err());
    }
    assert!(!root.0.join("windows").exists());
    assert!(claim_window(std::path::Path::new("relative"), &WindowSession::New).is_err());
}
