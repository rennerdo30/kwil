use super::*;
use crate::CommandArgs;
use kwil_core_workspace::window_session::WindowSession;
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "kwil-two-windows-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        Self(root)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            assert!(self.0.is_absolute());
            assert_eq!(self.0.parent(), Some(std::env::temp_dir().as_path()));
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
}

fn drive(workspace: &mut Workspace, rx: &mpsc::Receiver<DocumentResult>, done: impl Fn(&Workspace) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !done(workspace) {
        let event =
            rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).expect("bounded worker completion");
        workspace.test_document_result(event);
    }
}
fn start(
    root: &std::path::Path,
    window: WindowSession,
) -> (Workspace, mpsc::Receiver<DocumentResult>, mpsc::Receiver<Vec<String>>) {
    let (tx, rx) = mpsc::channel();
    let (restored, windows) = mpsc::channel();
    let mut workspace = Workspace::new(View::new(None)).unwrap();
    workspace.use_window_session(window, move |_, ids| {
        restored.send(ids).unwrap();
        Ok(())
    });
    workspace.set_session_restore(true);
    workspace
        .start_document_worker_at(Some(root.to_owned()), move |event| {
            let _ = tx.send(event);
        })
        .unwrap();
    drive(&mut workspace, &rx, Workspace::recovery_ready);
    (workspace, rx, windows)
}
fn stop(mut workspace: Workspace, rx: mpsc::Receiver<DocumentResult>) {
    workspace.request_window_close(false).unwrap();
    assert!(!workspace.close_confirmation_pending(), "persistent windows hot exit without discarding dirty tabs");
    drive(&mut workspace, &rx, Workspace::wants_exit);
    drop(workspace);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let DocumentResult::Stopped(result) =
            rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap()
        {
            result.unwrap();
            break;
        }
    }
}
fn fill(workspace: &mut Workspace, text: &str) -> Session {
    workspace
        .execute("edit.insert", &CommandArgs { text: Some(format!("{text}\nfirst🌿")), ..Default::default() }, 1)
        .unwrap();
    let first = workspace.active_id();
    workspace.file_command("file.new").unwrap();
    workspace
        .execute("edit.insert", &CommandArgs { text: Some(format!("{text}\nsecond日本語")), ..Default::default() }, 2)
        .unwrap();
    workspace.activate(first);
    workspace.set_horizontal_window(2, 64 << 10);
    workspace.sync_all();
    workspace.session.clone()
}
fn check(workspace: &Workspace, expected: &Session, label: &str) {
    assert_eq!(workspace.session.tabs(), expected.tabs());
    assert_eq!(workspace.session.active(), expected.active());
    for (i, tab) in workspace.session.tabs().iter().enumerate() {
        let view = workspace.view(tab.id).unwrap();
        let editor = view.editor().unwrap();
        let mut bytes = Vec::new();
        editor.document().snapshot().write_to(&mut bytes).unwrap();
        assert_eq!(bytes, format!("{label}\n{}", if i == 0 { "first🌿" } else { "second日本語" }).as_bytes());
        assert!(editor.document().is_dirty());
        assert_eq!(editor.primary_cursor().head, tab.cursor);
        assert_eq!(view.first_byte_column, tab.first_byte_column);
    }
}

#[test]
fn two_windows_restore_disjoint_dirty_tabs_and_skip_the_live_secondary() {
    let fixture = Fixture::new();
    let (mut primary, primary_rx, _) = start(&fixture.0, WindowSession::Primary);
    let (mut secondary, secondary_rx, _) = start(&fixture.0, WindowSession::New);
    let primary_expected = fill(&mut primary, "primary");
    let secondary_expected = fill(&mut secondary, "secondary");
    assert_eq!(primary_expected.tabs()[0].id, secondary_expected.tabs()[0].id, "IDs are window-local");
    let id = std::fs::read_dir(fixture.0.join("windows"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name()
        .into_string()
        .unwrap();
    stop(primary, primary_rx);
    // A fresh primary while the other window is live must neither launch nor claim it.
    let (primary, rx, discovered) = start(&fixture.0, WindowSession::Primary);
    assert!(discovered.try_recv().is_err());
    check(&primary, &primary_expected, "primary");
    stop(primary, rx);
    stop(secondary, secondary_rx);
    let (primary, rx, discovered) = start(&fixture.0, WindowSession::Primary);
    assert_eq!(discovered.recv_timeout(Duration::from_secs(1)).unwrap(), vec![id.clone()]);
    let (secondary, secondary_rx, _) = start(&fixture.0, WindowSession::Restore(id));
    check(&primary, &primary_expected, "primary");
    check(&secondary, &secondary_expected, "secondary");
    stop(primary, rx);
    stop(secondary, secondary_rx);
}
