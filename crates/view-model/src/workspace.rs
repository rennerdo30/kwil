//! Open views retain their own cursor, undo history and viewport. Worker
//! completions address a document ID, never whichever tab happens to be active.
use crate::{Damage, View};
use kwil_core_workspace::session::{DocumentId, Session};
use std::{
    collections::HashMap,
    io,
    ops::{Deref, DerefMut},
    path::PathBuf,
};

pub struct Workspace {
    session: Session,
    views: HashMap<DocumentId, View>,
}

impl Workspace {
    pub fn new(view: View) -> io::Result<Self> {
        let mut workspace = Self { session: Session::default(), views: HashMap::new() };
        workspace.open(view)?;
        Ok(workspace)
    }

    pub fn session(&self) -> &Session {
        &self.session
    }
    pub fn active_id(&self) -> DocumentId {
        self.session.active().expect("workspace always has a tab")
    }
    pub fn view(&self, id: DocumentId) -> Option<&View> {
        self.views.get(&id)
    }
    pub fn view_mut(&mut self, id: DocumentId) -> Option<&mut View> {
        self.views.get_mut(&id)
    }

    pub fn open(&mut self, mut view: View) -> io::Result<DocumentId> {
        if let Some(path) = &view.path {
            view.path = Some(std::path::absolute(path)?);
        }
        let id = self.session.open(view.path.clone())?;
        // Reopening an already-open path activates its existing unsaved view.
        self.views.entry(id).or_insert(view);
        Ok(id)
    }

    pub fn activate(&mut self, id: DocumentId) -> bool {
        self.sync_active();
        self.session.activate(id)
    }

    pub fn cycle(&mut self, backwards: bool) -> Damage {
        self.sync_active();
        let tabs = self.session.tabs();
        let current = tabs.iter().position(|tab| tab.id == self.active_id()).unwrap_or(0);
        let next = if backwards { (current + tabs.len() - 1) % tabs.len() } else { (current + 1) % tabs.len() };
        self.session.activate(tabs[next].id);
        Damage { full: true, status: true, ..Default::default() }
    }

    /// Refuse dirty close until the caller saves or explicitly chooses discard.
    pub fn close(&mut self, id: DocumentId, discard: bool) -> io::Result<bool> {
        let Some(view) = self.views.get(&id) else { return Ok(false) };
        if !discard && view.editor().is_some_and(|editor| editor.document().is_dirty()) {
            return Err(io::Error::other("Unsaved changes: save or explicitly discard before closing"));
        }
        // Prepare the replacement before mutating the last tab.
        if self.views.len() == 1 {
            self.open(View::new(None))?;
        }
        self.sync_active();
        if discard && let Some(tab) = self.session.tab_mut(id) {
            tab.saved_revision = tab.revision;
        }
        if !self.session.close(id) {
            return Err(io::Error::other(
                "Closed-tab history contains unsaved work; recover it before closing another tab",
            ));
        }
        self.views.remove(&id);
        Ok(true)
    }

    pub fn closed_path(&self) -> Option<PathBuf> {
        self.session.closed().iter().find_map(|tab| tab.path.clone())
    }

    pub fn sync_active(&mut self) {
        let id = self.active_id();
        let Some(view) = self.views.get(&id) else { return };
        if let Some(tab) = self.session.tab_mut(id) {
            tab.path = view.path.clone();
            tab.first_visible_line = view.top();
            if let Some(editor) = view.editor() {
                tab.revision = editor.document().revision();
                tab.saved_revision = editor.document().saved_revision();
                tab.cursor = editor.primary_cursor().head;
                tab.anchor = editor.primary_cursor().anchor;
            }
        }
    }
}

impl Deref for Workspace {
    type Target = View;
    fn deref(&self) -> &View {
        &self.views[&self.active_id()]
    }
}
impl DerefMut for Workspace {
    fn deref_mut(&mut self) -> &mut View {
        let id = self.active_id();
        self.views.get_mut(&id).expect("active view exists")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CommandArgs;

    #[test]
    fn tabs_keep_their_text_undo_and_cursor_and_refuse_dirty_close() {
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        let first = workspace.active_id();
        workspace.execute("edit.insert", &CommandArgs { text: Some("first".into()), ..Default::default() }, 1).unwrap();
        let second = workspace.open(View::new(None)).unwrap();
        workspace
            .execute("edit.insert", &CommandArgs { text: Some("second".into()), ..Default::default() }, 1)
            .unwrap();
        workspace.activate(first);
        assert_eq!(workspace.editor().unwrap().primary_cursor().head, 5);
        let mut bytes = Vec::new();
        workspace.editor().unwrap().document().snapshot().write_to(&mut bytes).unwrap();
        assert_eq!(bytes, b"first");
        assert!(workspace.close(second, false).is_err());
        assert!(workspace.view(second).unwrap().editor().unwrap().document().is_dirty());
        workspace.execute("edit.undo", &CommandArgs::default(), 1).unwrap();
        assert!(workspace.editor().unwrap().document().snapshot().is_empty());
        assert!(workspace.close(first, false).unwrap());
        assert_eq!(workspace.active_id(), second);
    }

    #[test]
    fn cycling_and_last_close_always_leave_an_active_view() {
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        let first = workspace.active_id();
        let second = workspace.open(View::new(None)).unwrap();
        workspace.cycle(false);
        assert_eq!(workspace.active_id(), first);
        workspace.cycle(true);
        assert_eq!(workspace.active_id(), second);
        workspace.close(first, false).unwrap();
        workspace.close(second, false).unwrap();
        assert_eq!(workspace.session().tabs().len(), 1);
        assert!(workspace.editor().unwrap().document().snapshot().is_empty());
    }
}
