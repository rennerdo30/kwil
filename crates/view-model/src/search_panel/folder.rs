use super::*;
use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
#[derive(Default)]
pub(super) struct Folder {
    plan: Option<folder_replace::Plan>,
    excluded: BTreeSet<u64>,
    pub(super) applying: bool,
    pub(super) previewing: bool,
    index: u64,
    cursor: u64,
    applied: u64,
    skipped: u64,
    cancel: Arc<AtomicBool>,
    pub(super) action: Option<(SearchKey, u64)>,
    closed: Option<(u64, u64)>,
    last_error: Option<String>,
}
impl Folder {
    pub(super) fn reset(&mut self) {
        self.cancel.store(true, Ordering::Release);
        *self = Self::default();
    }
}
impl SearchPanel {
    pub fn folder_failed(&mut self, error: String) {
        self.folder.reset();
        self.pending = false;
        self.error = Some(error);
    }
    pub fn folder_preview(&self) -> bool {
        self.mode == SearchMode::Files && self.folder.plan.is_some()
    }
    pub fn folder_replacement_action(&self, action: &SearchAction) -> bool {
        self.folder.applying
            && (self.folder.action.is_some_and(|(key, _)| key == action.key())
                || matches!(action, SearchAction::FolderClosed { .. }) && self.request_key == Some(action.key()))
    }
    pub(super) fn folder_submit(
        &mut self,
        session: &mut SearchSession,
        task: folder_replace::Task,
    ) -> Result<(), String> {
        self.advance_epoch();
        match session.submit(SessionDocumentId(0), 0, SearchTask::Folder(task)) {
            Ok(key) => {
                self.request_key = Some(key);
                self.pending = true;
                self.ready = false;
                self.refresh_needed = false;
                Ok(())
            }
            Err(error) => {
                self.retain_rejected(error);
                Ok(())
            }
        }
    }
    pub(super) fn folder_input(
        &mut self,
        session: &mut SearchSession,
        view: &Workspace,
        input: SearchInput<'_>,
    ) -> Option<Result<SearchOutcome, String>> {
        if self.mode != SearchMode::Files {
            return None;
        }
        if self.replacement_pending.is_some() && matches!(input, SearchInput::ReplaceNext | SearchInput::ReplaceAll) {
            return Some(Err("Wait for the current document replacement before starting folder replacement".into()));
        }
        if self.folder.applying && !matches!(input, SearchInput::Cancel) {
            return Some(Err("Folder replacement is running; Esc cancels remaining files".into()));
        }
        let result = match input {
            SearchInput::ReplaceNext => {
                if self.query.text.is_empty() {
                    return Some(Err("Enter a search query before previewing replacement".into()));
                }
                self.folder.reset();
                self.folder.previewing = true;
                self.advance_epoch();
                let root = if self.root_edited {
                    PathBuf::from(&self.root_input.text)
                } else {
                    match &self.root {
                        Some(root) => root.clone(),
                        None => return Some(Err("Search root is not ready".into())),
                    }
                };
                if !root.is_absolute() {
                    return Some(Err("Search root must be absolute".into()));
                }
                self.result_root = Some(root.clone());
                let mut search = FileSearch::new(root, self.query.text.clone());
                search.literal = !self.settings.regex;
                search.options.case_insensitive = !self.settings.case_sensitive;
                search.options.whole_word = self.settings.whole_word;
                search.include = self.include.text.lines().filter(|s| !s.is_empty()).map(str::to_owned).collect();
                search.exclude = self.exclude.text.lines().filter(|s| !s.is_empty()).map(str::to_owned).collect();
                self.status = "Preparing replacement preview…".into();
                self.folder_submit(
                    session,
                    folder_replace::Task::Preview {
                        search,
                        template: self.replacement.text.as_bytes().to_vec(),
                        open: view.folder_open_documents(),
                    },
                )
            }
            SearchInput::ReplaceAll => {
                if self.pending || self.folder.plan.is_none() {
                    return Some(Err("Preview replacement before applying".into()));
                }
                self.folder.applying = true;
                self.folder.index = 0;
                self.folder.cursor = 0;
                self.folder.applied = 0;
                self.folder.skipped = 0;
                self.folder_next(session, view)
            }
            SearchInput::SelectRow(row) if self.folder.plan.is_some() => {
                if self.pending || self.paint_key != session.key() {
                    return Some(Err("Wait for the replacement preview page".into()));
                }
                let Some(FileEvent::Hit(_)) = session.state().files.get(row) else {
                    return Some(Err("A preview error cannot be included".into()));
                };
                let ordinal = self.file_first + row as u64;
                if !self.folder.excluded.remove(&ordinal) {
                    if self.folder.excluded.len() >= INPUT_BYTES / 8 {
                        return Some(Err("Too many individual exclusions; narrow the folder globs".into()));
                    }
                    self.folder.excluded.insert(ordinal);
                }
                self.selected = row;
                self.advance_epoch();
                self.folder_labels(session);
                Ok(())
            }
            SearchInput::PageNext | SearchInput::PagePrevious if self.folder.plan.is_some() => {
                if self.pending {
                    return Some(Err("Wait for the replacement preview page".into()));
                }
                let plan = self.folder.plan.clone().unwrap();
                let first = if matches!(input, SearchInput::PagePrevious) {
                    self.file_first.saturating_sub(32)
                } else {
                    self.file_first.saturating_add(32)
                };
                if first >= plan.len() && first != 0 {
                    Ok(())
                } else {
                    self.folder_submit(session, folder_replace::Task::Page { plan, page: Page { first, len: 32 } })
                }
            }
            _ => return None,
        };
        Some(result.map(|()| SearchOutcome::panel()))
    }
    fn folder_labels(&mut self, session: &SearchSession) {
        self.result_rows = session
            .state()
            .files
            .iter()
            .enumerate()
            .map(|(i, row)| match row {
                FileEvent::Hit(hit) => SearchResultRow {
                    label: format!(
                        "[{}] {} — {} matches",
                        if self.folder.excluded.contains(&(self.file_first + i as u64)) { ' ' } else { 'x' },
                        bounded_label(&self.result_path(&hit.path).to_string_lossy(), 512),
                        hit.line_number
                    ),
                    is_hit: true,
                },
                FileEvent::Error { path, message } => SearchResultRow {
                    label: format!(
                        "Skipped {}: {}",
                        bounded_label(&self.result_path(path).to_string_lossy(), 256),
                        bounded_label(message, 256)
                    ),
                    is_hit: false,
                },
            })
            .collect();
    }
    pub(super) fn folder_next(&mut self, session: &mut SearchSession, view: &Workspace) -> Result<(), String> {
        self.folder.action = None;
        self.folder.closed = None;
        let plan = self.folder.plan.clone().ok_or("Replacement preview was cancelled")?;
        while self.folder.excluded.contains(&self.folder.index) {
            self.folder.index += 1;
        }
        if self.folder.cancel.load(Ordering::Acquire) || self.folder.index >= plan.len() {
            self.folder.applying = false;
            self.pending = false;
            self.status = format!(
                "Replacement complete: {} files applied, {} skipped, {} excluded{}",
                self.folder.applied,
                self.folder.skipped,
                self.folder.excluded.len(),
                self.folder.last_error.as_ref().map_or(String::new(), |e| format!("; {e}"))
            );
            self.folder.plan = None;
            return Ok(());
        }
        self.status = format!("Replacing file {} of {}…", self.folder.index + 1, plan.len());
        self.folder_submit(
            session,
            folder_replace::Task::Apply {
                plan,
                index: self.folder.index,
                cursor: self.folder.cursor,
                open: view.folder_open_documents(),
            },
        )
    }
    pub(super) fn folder_completed(
        &mut self,
        result: Result<(), String>,
        session: &mut SearchSession,
        view: &Workspace,
    ) -> Result<(), String> {
        if let Err(error) = result {
            self.folder.skipped += 1;
            self.folder.last_error = Some(error);
        } else {
            self.folder.applied += 1;
        }
        self.folder.index += 1;
        self.folder_next(session, view)
    }
    pub(super) fn folder_ready(
        &mut self,
        update: folder_replace::Update,
        session: &mut SearchSession,
        view: &Workspace,
    ) -> SearchOutcome {
        let mut out = SearchOutcome::panel();
        let result = match update {
            folder_replace::Update::Preview { plan, .. } => {
                self.folder.previewing = false;
                self.folder.plan = Some(plan);
                self.accept_state(session.state(), view);
                self.folder_labels(session);
                self.status = format!(
                    "Preview: {} files, {} matches. Enter toggles a file; Apply replaces included files",
                    self.files_seen, self.count
                );
                Ok(())
            }
            folder_replace::Update::NeedsClosedApply { plan, index, offset, next_offset } => {
                self.folder.cursor = next_offset;
                let key = session.key().unwrap();
                self.folder.closed = Some((key.generation, index));
                out.action =
                    Some(SearchAction::FolderClosed { key, index, offset, plan, cancel: self.folder.cancel.clone() });
                Ok(())
            }
            folder_replace::Update::Applied { result, source_epoch, next_offset, .. } => {
                self.folder.cursor = next_offset;
                match result {
                    Ok(Some(recipe)) => {
                        self.folder.action = Some((recipe.key, source_epoch));
                        out.action = Some(SearchAction::Replace(recipe));
                        Ok(())
                    }
                    Ok(None) => self.folder_completed(Ok(()), session, view),
                    Err(error) => self.folder_completed(Err(error), session, view),
                }
            }
        };
        if let Err(error) = result {
            self.folder.applying = false;
            self.pending = false;
            self.error = Some(error);
        }
        out
    }
    pub(super) fn folder_closed_ready(
        &mut self,
        session: &mut SearchSession,
        view: &Workspace,
    ) -> Option<Result<SearchOutcome, String>> {
        let (generation, index) = self.folder.closed?;
        let result = view.folder_closed_result(generation, index)?;
        self.folder.closed = None;
        Some(self.folder_completed(result, session, view).map(|()| SearchOutcome::panel()))
    }
}
