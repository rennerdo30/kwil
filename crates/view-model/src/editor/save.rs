use super::*;
use kwil_core_workspace::{SaveRequest, SavedSnapshot};
use std::path::PathBuf;

impl Editor {
    /// O(1) request creation, including trim-on-save. Workers scan/transform
    /// the immutable source while writing, then return a rebased result.
    pub fn save_request(&mut self, path: Option<PathBuf>) -> io::Result<SaveRequest> {
        if self.saving {
            return Err(io::Error::other("save already in progress"));
        }
        let path = path
            .or_else(|| self.document.path.clone())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "save requires a path"))?;
        let target_markdown = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "md" | "markdown" | "mdown" | "mkd"));
        self.end_burst();
        self.saving = true;
        Ok(SaveRequest {
            snapshot: self.document.snapshot(),
            path,
            original: self.document.original.clone(),
            revision: self.document.revision,
            trim_trailing_whitespace: self.settings.editor.trim_trailing_whitespace_on_save
                && !self.is_markdown()
                && !target_markdown,
        })
    }

    /// Complete on the UI/model thread without rescanning/copying file bytes.
    /// Concurrent edits stay dirty; their original pieces were pinned by save.
    /// A byte-equivalent current version rebases in O(1), preserving history.
    pub fn finish_save(&mut self, result: io::Result<SavedSnapshot>) -> io::Result<CommandResult> {
        let saved = match result {
            Ok(saved) => saved,
            Err(error) => {
                self.saving = false;
                return Err(error);
            }
        };
        let current = self.document.revision == saved.revision;
        if current && saved.trimmed_rows.is_some() && self.flush_edit_receipt().is_err() {
            self.receipts.deferred_save = Some(saved);
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "save completion awaits journal admission"));
        }
        self.saving = false;
        let mut damage = Damage { status: true, ..Damage::default() };
        if let Some(rows) = saved.trimmed_rows {
            if current {
                let old = self.cursor_rows()?;
                let before = self.state();
                let source = self.document.snapshot();
                for cursor in &mut self.cursors {
                    for position in [&mut cursor.anchor, &mut cursor.head] {
                        let (row, column) = source
                            .offset_to_line_col(*position)
                            .map_err(io::Error::other)?
                            .ok_or_else(|| io::Error::other("cursor outside source"))?;
                        let bounds = text::line(&saved.snapshot, row)?;
                        *position = (bounds.start + column).min(bounds.end);
                    }
                    cursor.desired_column = None;
                }
                let transition = Transition {
                    before: source,
                    after: saved.snapshot.clone(),
                    recipe: Recipe::Trim(
                        saved.trim_deletions.ok_or_else(|| io::Error::other("trim completion lacks deletion spool"))?,
                    ),
                };
                self.document.buffer.install_worker_edit(saved.snapshot)?;
                self.document.changed();
                self.box_column = None;
                self.normalize();
                self.redo.clear();
                self.undo.push_back(HistoryEntry {
                    before: before.clone(),
                    after: self.state(),
                    edited: rows.clone(),
                    structural: false,
                    transition: transition.clone(),
                });
                self.charge_model_history();
                self.publish(transition, before.revision, self.document.revision, EditKind::WorkerTrim);
                self.document.saved_revision = self.document.revision;
                damage = self.finish_damage(old, Some(rows))?;
            } else {
                // Disk contains a transformed version not represented by the
                // live undo chain. Never label its untrimmed source clean.
                self.document.mark_unsaved();
            }
        } else {
            if current {
                self.document.buffer.checkpoint_current(saved.snapshot)?;
            }
            self.document.saved_revision = saved.revision;
        }
        self.document.original = Some(saved.file);
        self.document.path = Some(saved.path);
        let status = if saved.warnings.is_empty() {
            CommandStatus::Applied
        } else {
            CommandStatus::Message(saved.warnings.join("; "))
        };
        Ok(CommandResult { damage, status })
    }

    pub fn has_pending_save_completion(&self) -> bool {
        self.receipts.deferred_save.is_some()
    }
    /// Retry a retained transformed completion after journal queue admission.
    pub fn finish_pending_save(&mut self) -> io::Result<Option<CommandResult>> {
        self.receipts.deferred_save.take().map(|saved| self.finish_save(Ok(saved))).transpose()
    }
}
