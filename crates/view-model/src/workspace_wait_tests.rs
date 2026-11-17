//! `kwil --wait`: a launcher is answered when the tabs it asked for are gone.
use super::*;
use kwil_core_workspace::instance::{MAX_OPEN_FILES, OpenFile, OpenFiles, WaitOutcome, Waits};
use std::sync::mpsc::Receiver;

fn file(name: &str) -> OpenFile {
    OpenFile {
        path: std::env::temp_dir().join(format!("kw90-{}-{name}", std::process::id())),
        line: None,
        column: None,
    }
}

/// The fixture files do not exist. Finish each checked open with the terminal
/// receipt a worker would send, so the tabs are closable.
fn settle_opens(workspace: &mut Workspace) {
    while let Some(index) =
        workspace.files.queue.iter().position(|request| matches!(request, DocumentRequest::OpenChecked { .. }))
    {
        let Some(DocumentRequest::OpenChecked { ticket, .. }) = workspace.files.queue.remove(index) else {
            unreachable!()
        };
        workspace.test_document_result(DocumentResult::CheckedOpened {
            ticket,
            result: Err(io::Error::new(io::ErrorKind::NotFound, "fixture")),
        });
    }
}

fn tab_of(workspace: &Workspace, file: &OpenFile) -> DocumentId {
    workspace.session().tabs().iter().find(|tab| tab.path.as_ref() == Some(&file.path)).expect("tab is open").id
}

fn answer(rx: &Receiver<WaitOutcome>) -> Option<WaitOutcome> {
    rx.try_recv().ok()
}

fn workspace_with_waits() -> (Workspace, std::sync::Arc<Waits>) {
    let waits = Waits::new();
    let mut workspace = Workspace::new(View::new(None)).unwrap();
    workspace.set_wait_sink(waits.clone());
    (workspace, waits)
}

#[test]
fn kw90_wait_is_answered_only_when_every_requested_tab_is_closed() {
    let (mut workspace, waits) = workspace_with_waits();
    let (a, b) = (file("a"), file("b"));
    let rx = waits.admit(7).unwrap();
    workspace.open_files(OpenFiles { files: vec![a.clone(), b.clone()], wait: Some(7) }).unwrap();
    settle_opens(&mut workspace);
    assert_eq!(answer(&rx), None, "both tabs are open");
    workspace.close(tab_of(&workspace, &a), true).unwrap();
    assert_eq!(answer(&rx), None, "one requested tab is still open");
    workspace.close(tab_of(&workspace, &b), true).unwrap();
    assert_eq!(answer(&rx), Some(WaitOutcome::Closed));
}

#[test]
fn kw90_a_tab_that_was_already_open_is_waited_on_and_a_plain_open_waits_for_nothing() {
    let (mut workspace, waits) = workspace_with_waits();
    let a = file("already-open");
    workspace.open_files(OpenFiles { files: vec![a.clone()], wait: None }).unwrap();
    settle_opens(&mut workspace);
    let id = tab_of(&workspace, &a);
    let rx = waits.admit(8).unwrap();
    workspace.open_files(OpenFiles { files: vec![a.clone()], wait: Some(8) }).unwrap();
    assert_eq!(tab_of(&workspace, &a), id, "the request reuses the open tab");
    assert_eq!(answer(&rx), None);
    workspace.close(id, true).unwrap();
    assert_eq!(answer(&rx), Some(WaitOutcome::Closed));
}

#[test]
fn kw90_each_waiter_is_answered_by_its_own_files_and_a_refused_open_is_reported() {
    let (mut workspace, waits) = workspace_with_waits();
    let (a, b) = (file("first"), file("second"));
    let (rx_a, rx_b) = (waits.admit(1).unwrap(), waits.admit(2).unwrap());
    workspace.open_files(OpenFiles { files: vec![a.clone()], wait: Some(1) }).unwrap();
    workspace.open_files(OpenFiles { files: vec![b.clone()], wait: Some(2) }).unwrap();
    settle_opens(&mut workspace);
    workspace.close(tab_of(&workspace, &b), true).unwrap();
    assert_eq!((answer(&rx_a), answer(&rx_b)), (None, Some(WaitOutcome::Closed)));
    let rx_bad = waits.admit(3).unwrap();
    let too_many = OpenFiles { files: vec![file("x"); MAX_OPEN_FILES + 1], wait: Some(3) };
    assert!(workspace.open_files(too_many).is_err());
    assert_eq!(answer(&rx_bad), Some(WaitOutcome::OpenFailed), "a launcher must not wait for a file that never opened");
    workspace.close(tab_of(&workspace, &a), true).unwrap();
    assert_eq!(answer(&rx_a), Some(WaitOutcome::Closed));
}
