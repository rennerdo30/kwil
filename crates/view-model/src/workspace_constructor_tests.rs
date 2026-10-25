//! SOURCE ONLY. ROOT runs constructor ownership controls in its isolated freeze.
use super::*;
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "kwil-constructor-{}-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir(&root).unwrap();
        Self(root)
    }
    fn view(&self) -> View {
        let path = self.0.join("original.txt");
        std::fs::write(&path, b"original source\n").unwrap();
        let file = kwil_core_text::TextFile::open(&path).unwrap();
        assert!(file.index().is_none());
        View::new(Some((path, file)))
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("Constructor fixture retained: {}", self.0.display());
            return;
        }
        assert!(self.0.is_absolute() && self.0.starts_with(std::env::temp_dir()));
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}
fn refused(flow: ControlFlow<WorkspaceInitRefusal, Workspace>) -> WorkspaceInitRefusal {
    match flow {
        ControlFlow::Break(refusal) => refusal,
        ControlFlow::Continue(workspace) => {
            std::mem::forget(workspace);
            panic!("constructor accepted an actual refusal fixture");
        }
    }
}
fn retire<T: Send>(owner: T) {
    std::thread::scope(|scope| scope.spawn(move || drop(owner)).join().unwrap());
}

#[test]
fn workspace_constructor_prepublication_path_refusal_returns_original_view_and_error() {
    let scratch = Scratch::new();
    let mut view = scratch.view();
    let source = view.file().unwrap().path().as_os_str().as_encoded_bytes().as_ptr() as usize;
    let incarnation = Arc::downgrade(&view.input_incarnation);
    view.path = Some(PathBuf::from("invalid\0startup"));
    let refusal = refused(Workspace::try_new(view));
    assert!(matches!(refusal.error.kind(), io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData));
    assert!(refusal.workspace.views.is_empty());
    assert!(refusal.workspace.session.tabs().is_empty());
    assert!(refusal.workspace.files.queue.is_empty());
    assert!(!refusal.workspace.files.document_started);
    assert!(refusal.workspace.files.worker.is_none());
    let retained = refusal.input.as_ref().expect("the caller's input must remain before publication");
    assert!(std::sync::Weak::ptr_eq(&incarnation, &Arc::downgrade(&retained.input_incarnation)));
    assert_eq!(retained.file().unwrap().path().as_os_str().as_encoded_bytes().as_ptr() as usize, source);
    assert_eq!(incarnation.strong_count(), 1, "the incoming View was not cloned or destroyed");
    retire(refusal);
    assert!(incarnation.upgrade().is_none(), "original source retirement occurs only on the off-UI executor");
}

#[test]
fn workspace_constructor_session_refusal_keeps_input_outside_partial_workspace() {
    let scratch = Scratch::new();
    let view = scratch.view();
    let incarnation = Arc::downgrade(&view.input_incarnation);
    let mut empty = Workspace::empty();
    for _ in 0..kwil_core_workspace::session::MAX_TABS {
        empty.session.open(None).unwrap();
    }
    let refusal = refused(empty.initialize(view));
    assert_eq!(refusal.error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(refusal.error.to_string(), "tab limit reached");
    assert_eq!(refusal.workspace.session.tabs().len(), kwil_core_workspace::session::MAX_TABS);
    assert!(refusal.workspace.views.is_empty());
    assert!(refusal.input.as_ref().unwrap().file().is_some());
    assert_eq!(incarnation.strong_count(), 1);
    retire(refusal);
    assert!(incarnation.upgrade().is_none());
}

#[test]
fn workspace_constructor_checked_open_refusal_returns_published_original_in_partial_workspace() {
    let scratch = Scratch::new();
    let view = scratch.view();
    let source = view.file().unwrap().path().as_os_str().as_encoded_bytes().as_ptr() as usize;
    let incarnation = Arc::downgrade(&view.input_incarnation);
    let mut empty = Workspace::empty();
    empty.binary.next_generation = u64::MAX;
    let refusal = refused(empty.initialize(view));
    assert_eq!(refusal.error.kind(), io::ErrorKind::Other);
    assert_eq!(refusal.error.to_string(), "Binary open identity exhausted");
    assert!(refusal.input.is_none(), "one incoming source cannot also remain in the prepublication input");
    assert_eq!(refusal.workspace.views.len(), 1);
    assert_eq!(refusal.workspace.session.tabs().len(), 1);
    let id = refusal.workspace.active_id();
    let retained = &refusal.workspace.views[&id];
    assert!(std::sync::Weak::ptr_eq(&incarnation, &Arc::downgrade(&retained.input_incarnation)));
    assert_eq!(retained.file().unwrap().path().as_os_str().as_encoded_bytes().as_ptr() as usize, source);
    assert_eq!(incarnation.strong_count(), 1);
    assert!(refusal.workspace.files.queue.is_empty());
    assert!(refusal.workspace.binary.flights.is_empty());
    assert_eq!(refusal.workspace.binary.next_generation, u64::MAX);
    assert!(!refusal.workspace.files.document_started);
    assert!(refusal.workspace.files.worker.is_none());
    retire(refusal);
    assert!(incarnation.upgrade().is_none());
}

#[test]
fn workspace_constructor_success_keeps_original_file_in_exact_initial_checked_request() {
    let scratch = Scratch::new();
    let view = scratch.view();
    let source = view.file().unwrap().path().as_os_str().as_encoded_bytes().as_ptr() as usize;
    let incarnation = Arc::downgrade(&view.input_incarnation);
    let workspace = match Workspace::try_new(view) {
        ControlFlow::Continue(workspace) => workspace,
        ControlFlow::Break(refusal) => {
            retire(refusal);
            panic!("the original file-backed view must initialize");
        }
    };
    let id = workspace.active_id();
    assert_eq!(workspace.views.len(), 1);
    assert_eq!(workspace.session.tabs().len(), 1);
    assert!(workspace.views[&id].file().is_none());
    assert_eq!(incarnation.strong_count(), 1);
    assert_eq!(workspace.files.queue.len(), 1);
    assert_eq!(workspace.binary.flights.len(), 1);
    let DocumentRequest::OpenChecked { ticket, path, file: Some(file) } = workspace.files.queue.front().unwrap() else {
        panic!("constructor must transfer the original source once into the existing request");
    };
    assert_eq!(file.path().as_os_str().as_encoded_bytes().as_ptr() as usize, source);
    assert_eq!(ticket.document(), id);
    assert_eq!(ticket.revision(), 0);
    assert_eq!(ticket.generation(), 1);
    assert_eq!(file.path(), path);
    let flight = &workspace.binary.flights[&id];
    assert_eq!(flight.ticket, *ticket);
    assert!(matches!(flight.phase, BinaryPhase::Opening));
    assert!(!flight.preview_ready);
    assert!(flight.return_to.is_none());
    assert!(!workspace.files.document_started);
    assert!(workspace.files.worker.is_none());
    assert!(workspace.unstarted_frontend_ready());
    retire(workspace);
    assert!(incarnation.upgrade().is_none());
}

// Old-source fail-first item: copy only this test into an existing Workspace
// test module. It uses no new API and witnesses the former UI owner drop.
#[test]
fn workspace_constructor_legacy_failure_quarantines_original_owner_without_error_replacement() {
    let mut view = View::new(None);
    view.editor_mut().unwrap().insert_text("fatal legacy owner", Duration::ZERO).unwrap();
    let incarnation = std::sync::Arc::downgrade(&view.input_incarnation);
    view.path = Some(PathBuf::from("invalid\0startup"));
    let error = match Workspace::new(view) {
        Err(error) => error,
        Ok(workspace) => {
            std::mem::forget(workspace);
            panic!("invalid constructor path accepted");
        }
    };
    assert!(matches!(error.kind(), io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData));
    assert_eq!(incarnation.strong_count(), 1, "legacy fatal refusal must not destroy its original source on UI");
    // Deliberate bounded fixture quarantine models the approved transitional
    // fatal compatibility path. It is not retirement or v1 cleanup evidence.
}
