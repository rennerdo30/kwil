use super::*;
use kwil_core_workspace::{SaveRequest, SavedSnapshot};
use std::path::PathBuf;

impl Editor {
    /// Prepare on a worker when trim-on-save is enabled: trim can scan the
    /// whole document. Frontends submit the returned immutable request to
    /// their fixed worker pool, then send its result back as an event.
    pub fn save_request(&mut self, path: Option<PathBuf>) -> io::Result<SaveRequest> {
        if self.saving {
            return Err(io::Error::other("save already in progress"));
        }
        let path = path
            .or_else(|| self.document.path.clone())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "save requires a path"))?;
        if self.settings.editor.trim_trailing_whitespace_on_save && !self.is_markdown() {
            self.trim_trailing_whitespace()?;
        }
        self.end_burst();
        self.saving = true;
        Ok(SaveRequest {
            snapshot: self.document.snapshot(),
            path,
            original: self.document.original.clone(),
            revision: self.document.revision,
        })
    }

    /// Complete on the UI/model thread without rescanning/copying file bytes.
    /// Concurrent edits stay dirty; their original pieces were pinned by save.
    /// A byte-equivalent current version rebases in O(1), preserving history.
    pub fn finish_save(&mut self, result: io::Result<SavedSnapshot>) -> io::Result<CommandResult> {
        self.saving = false;
        let saved = result?;
        if self.document.revision == saved.revision {
            self.document.buffer.checkpoint_current(saved.snapshot)?;
        }
        self.document.saved_revision = saved.revision;
        self.document.original = Some(saved.file);
        self.document.path = Some(saved.path);
        let status = if saved.warnings.is_empty() {
            CommandStatus::Applied
        } else {
            CommandStatus::Message(saved.warnings.join("; "))
        };
        Ok(CommandResult { damage: Damage { status: true, ..Damage::default() }, status })
    }
}
