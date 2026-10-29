//! File commands share the existing prompt and document-worker loan.
use super::*;
use crate::document_worker::FileAction;
use kwil_core_text::encoding::Encoding;

// Canonical stateless encoding_rs names plus the engine's UTF-32 codecs.
const ENCODINGS: &[&str] = &[
    "UTF-8",
    "UTF-16LE",
    "UTF-16BE",
    "UTF-32LE",
    "UTF-32BE",
    "Big5",
    "EUC-JP",
    "EUC-KR",
    "GBK",
    "gb18030",
    "IBM866",
    "ISO-8859-2",
    "ISO-8859-3",
    "ISO-8859-4",
    "ISO-8859-5",
    "ISO-8859-6",
    "ISO-8859-7",
    "ISO-8859-8",
    "ISO-8859-8-I",
    "ISO-8859-10",
    "ISO-8859-13",
    "ISO-8859-14",
    "ISO-8859-15",
    "ISO-8859-16",
    "KOI8-R",
    "KOI8-U",
    "macintosh",
    "Shift_JIS",
    "windows-874",
    "windows-1250",
    "windows-1251",
    "windows-1252",
    "windows-1253",
    "windows-1254",
    "windows-1255",
    "windows-1256",
    "windows-1257",
    "windows-1258",
    "x-mac-cyrillic",
    "x-user-defined",
];

impl FilePromptKind {
    pub fn title(self) -> &'static str {
        match self {
            Self::Open => "Open by path",
            Self::SaveAs => "Save as path",
            Self::Recent => "Open recent",
            Self::Folder => "Open Folder",
            Self::Encoding => "Change Encoding",
            Self::ReopenEncoding => "Reopen with encoding",
            Self::SaveEncoding => "Save with encoding",
            Self::LineEnding => "Change Line Endings",
            Self::Reload => "Discard local edits and reload?",
        }
    }
    pub fn choices(self) -> &'static [&'static str] {
        match self {
            Self::Encoding => &["Reopen with encoding (clean document)", "Save with encoding (next save)"],
            Self::ReopenEncoding | Self::SaveEncoding => ENCODINGS,
            Self::LineEnding => &["LF", "CRLF"],
            Self::Reload => &["Reload from Disk (discard local edits)"],
            _ => &[],
        }
    }
}

impl Workspace {
    pub fn take_workspace_folder(&mut self) -> Option<PathBuf> {
        self.files.folder.take()
    }

    fn command_prompt(&mut self, kind: FilePromptKind) {
        self.files.next_prompt = self.files.next_prompt.wrapping_add(1);
        self.files.prompt = Some(FilePrompt {
            generation: self.files.next_prompt,
            kind,
            document: self.active_id(),
            revision: self.editor_job_revision(self.active_id()),
            text: String::new(),
        });
        self.prompt_notice();
    }

    pub(super) fn file_feature_command(&mut self, command: &str) -> io::Result<bool> {
        let kind = match command {
            "file.openFolder" => Some(FilePromptKind::Folder),
            "file.setEncoding" => Some(FilePromptKind::Encoding),
            "file.setLineEnding" => Some(FilePromptKind::LineEnding),
            "file.reload" | "file.keepChanges" => None,
            _ => return Ok(false),
        };
        if command == "file.openFolder" {
            self.command_prompt(FilePromptKind::Folder);
            return Ok(true);
        }
        if self.hex_owns_document(self.active_id()) {
            self.set_notice("Switch to text view before changing text file options".into());
            return Ok(true);
        }
        let id = self.active_id();
        let tab = self.session.tabs().iter().find(|tab| tab.id == id).unwrap();
        let named = self.editor().map_or(self.views[&id].path.is_some(), |editor| editor.document().path().is_some());
        let dirty = self.editor().map_or(tab.revision != tab.saved_revision, |editor| editor.document().is_dirty());
        if command == "file.reload" || command == "file.keepChanges" {
            if !named {
                self.set_notice("Open a file from disk first".into());
            } else if command == "file.reload" && dirty {
                self.command_prompt(FilePromptKind::Reload);
            } else {
                self.submit_file_action(if command == "file.reload" {
                    FileAction::Reload(None)
                } else {
                    FileAction::Keep
                })?;
            }
        } else {
            let mut kind = kind.unwrap();
            if kind == FilePromptKind::Encoding && (dirty || !named) {
                kind = FilePromptKind::SaveEncoding;
            }
            self.command_prompt(kind);
        }
        Ok(true)
    }

    pub(super) fn submit_command_prompt(&mut self, prompt: FilePrompt) -> io::Result<()> {
        let result = (|| {
            if prompt.kind == FilePromptKind::Folder {
                if self.files.queue.len() >= 256 {
                    return Err(io::Error::other("File worker is busy; retry"));
                }
                let path = PathBuf::from(&prompt.text);
                if !path.is_absolute() {
                    return Err(io::Error::other("Enter an absolute folder path"));
                }
                self.files.queue.push_back(DocumentRequest::SetFolder { path });
                self.pump_files();
                return Ok(());
            }
            if prompt.document != self.active_id() || self.editor_job_revision(prompt.document) != prompt.revision {
                return Err(io::Error::other("Document changed; reopen the file command"));
            }
            let index = prompt
                .text
                .trim()
                .parse::<usize>()
                .ok()
                .and_then(|n| n.checked_sub(1))
                .filter(|&n| n < prompt.kind.choices().len())
                .ok_or_else(|| io::Error::other("Choose a listed number, then Enter"))?;
            let action = match prompt.kind {
                FilePromptKind::Encoding => {
                    let kind = if index == 0 { FilePromptKind::ReopenEncoding } else { FilePromptKind::SaveEncoding };
                    if index == 0
                        && (self.views[&prompt.document].path.is_none()
                            || self.editor().map_or_else(
                                || {
                                    let tab = self.session.tabs().iter().find(|tab| tab.id == prompt.document).unwrap();
                                    tab.revision != tab.saved_revision
                                },
                                |editor| editor.document().is_dirty(),
                            ))
                    {
                        return Err(io::Error::other("Save your changes before reopening with an encoding"));
                    }
                    self.command_prompt(kind);
                    return Ok(());
                }
                FilePromptKind::ReopenEncoding => {
                    FileAction::Reload(Some(Encoding::for_label(ENCODINGS[index]).map_err(io::Error::other)?))
                }
                FilePromptKind::SaveEncoding => {
                    FileAction::Encoding(Encoding::for_label(ENCODINGS[index]).map_err(io::Error::other)?)
                }
                FilePromptKind::LineEnding => FileAction::LineEnding(if index == 0 {
                    crate::DocumentLineEnding::Lf
                } else {
                    crate::DocumentLineEnding::CrLf
                }),
                FilePromptKind::Reload => FileAction::Reload(None),
                _ => unreachable!(),
            };
            self.submit_file_action(action)
        })();
        if result.is_err() && self.files.prompt.is_none() {
            self.files.prompt = Some(prompt);
        }
        result
    }

    fn submit_file_action(&mut self, action: FileAction) -> io::Result<()> {
        let id = self.active_id();
        let revision = self.editor_job_revision(id);
        let key = kwil_core_search::worker::SearchKey { document_id: id.0, revision, generation: 0 };
        self.submit_editor_job(key, PendingEditorAction::File(action))
    }

    pub(super) fn finish_file_action(&mut self, id: DocumentId, _success: bool) -> Option<FileAction> {
        let action = self.files.loans.remove(&id).and_then(|loan| loan.file_action);
        // Reinstall the destination after either outcome: a failed worker
        // conversion may already have closed its previous recovery journal.
        if action.is_some_and(FileAction::resets_journal) {
            self.files.observed.remove(&id);
            self.files.seeded.remove(&id);
            self.files.durable.remove(&id);
            self.files.replacement_journals.remove(&id);
            self.files.journal_failed.remove(&id);
            self.hex.seed.insert(id);
        }
        action
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    };
    use std::time::{Duration, Instant};
    fn drive(workspace: &mut Workspace, rx: &mpsc::Receiver<DocumentResult>, done: impl Fn(&Workspace) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !done(workspace) {
            let result = rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|error| panic!("{}: {error}", workspace.notice()));
            workspace.test_document_result(result);
        }
    }
    fn choose(workspace: &mut Workspace, choice: &str) {
        workspace.file_prompt_mut().unwrap().text = choice.into();
        workspace.submit_file_prompt().unwrap();
    }
    fn bytes(workspace: &Workspace) -> Vec<u8> {
        let mut bytes = Vec::new();
        workspace.editor().unwrap().document().snapshot().write_to(&mut bytes).unwrap();
        bytes
    }
    #[test]
    fn file_commands_use_shared_encoding_save_reload_keep_and_folder_workers() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "kwil-file-commands-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("source.txt");
        std::fs::write(&path, b"caf\xe9\r\nnext\n").unwrap();
        let (tx, rx) = mpsc::channel();
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        workspace
            .start_document_worker_at(Some(root.join("state")), move |result| {
                tx.send(result).unwrap();
            })
            .unwrap();
        drive(&mut workspace, &rx, Workspace::recovery_ready);
        let id = workspace.open_path(path.clone()).unwrap();
        drive(&mut workspace, &rx, |w| w.editor().is_some() && !w.document_busy(id));
        workspace.file_command("file.setEncoding").unwrap();
        choose(&mut workspace, "1");
        let cp = ENCODINGS.iter().position(|&name| name == "windows-1252").unwrap() + 1;
        choose(&mut workspace, &cp.to_string());
        drive(&mut workspace, &rx, |w| !w.document_busy(id));
        assert_eq!(bytes(&workspace), "café\r\nnext\n".as_bytes());
        workspace.save_document(id, None).unwrap();
        drive(&mut workspace, &rx, |w| !w.file_saves_pending());
        assert_eq!(std::fs::read(&path).unwrap(), b"caf\xe9\r\nnext\n", "unchanged regions stay byte exact");
        workspace.file_command("file.setEncoding").unwrap();
        choose(&mut workspace, "2");
        choose(&mut workspace, "2");
        drive(&mut workspace, &rx, |w| !w.document_busy(id));
        assert!(workspace.editor().unwrap().document().is_dirty());
        assert_eq!(std::fs::read(&path).unwrap(), b"caf\xe9\r\nnext\n", "encoding selection waits for save");
        workspace.save_document(id, None).unwrap();
        drive(&mut workspace, &rx, |w| !w.file_saves_pending());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            "café\r\nnext\n".encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<_>>()
        );
        for (choice, text) in [("2", "café\r\nnext\r\n"), ("1", "café\nnext\n")] {
            workspace.file_command("file.setLineEnding").unwrap();
            choose(&mut workspace, choice);
            drive(&mut workspace, &rx, |w| !w.document_busy(id));
            assert_eq!(bytes(&workspace), text.as_bytes());
            workspace.save_document(id, None).unwrap();
            drive(&mut workspace, &rx, |w| !w.file_saves_pending());
            assert_eq!(
                std::fs::read(&path).unwrap(),
                text.encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<_>>()
            );
        }
        std::fs::rename(&path, root.join("previous-one.txt")).unwrap();
        std::fs::write(&path, b"disk one\n").unwrap();
        workspace.file_command("file.reload").unwrap();
        drive(&mut workspace, &rx, |w| !w.document_busy(id));
        assert_eq!(bytes(&workspace), b"disk one\n");
        workspace
            .execute("edit.insert", &crate::CommandArgs { text: Some("local ".into()), ..Default::default() }, 1)
            .unwrap();
        std::fs::rename(&path, root.join("previous-two.txt")).unwrap();
        std::fs::write(&path, b"disk two\n").unwrap();
        workspace.file_command("file.keepChanges").unwrap();
        drive(&mut workspace, &rx, |w| !w.document_busy(id));
        assert_eq!(bytes(&workspace), b"local disk one\n");
        assert!(workspace.editor().unwrap().document().is_dirty());
        assert_eq!(
            workspace.editor().unwrap().document().original.as_ref().unwrap().check().unwrap(),
            kwil_core_text::FileState::Unchanged
        );
        workspace.file_command("file.reload").unwrap();
        assert_eq!(workspace.file_prompt().unwrap().kind, FilePromptKind::Reload);
        workspace.cancel_file_prompt();
        assert_eq!(bytes(&workspace), b"local disk one\n");
        workspace.file_command("file.reload").unwrap();
        choose(&mut workspace, "1");
        drive(&mut workspace, &rx, |w| !w.document_busy(id));
        assert_eq!(bytes(&workspace), b"disk two\n");
        assert!(!workspace.editor().unwrap().document().is_dirty());
        let malformed = root.join("malformed.txt");
        std::fs::write(&malformed, b"a\xff\r\n").unwrap();
        let malformed_id = workspace.open_path(malformed.clone()).unwrap();
        drive(&mut workspace, &rx, |w| w.editor().is_some() && !w.document_busy(malformed_id));
        workspace.file_command("file.setEncoding").unwrap();
        choose(&mut workspace, "1");
        choose(&mut workspace, "1");
        drive(&mut workspace, &rx, |w| !w.document_busy(malformed_id));
        workspace.file_command("file.setLineEnding").unwrap();
        choose(&mut workspace, "1");
        drive(&mut workspace, &rx, |w| !w.document_busy(malformed_id));
        workspace.save_document(malformed_id, None).unwrap();
        drive(&mut workspace, &rx, |w| !w.file_saves_pending());
        assert_eq!(std::fs::read(&malformed).unwrap(), b"a\xff\n", "ending conversion keeps opaque invalid bytes");
        workspace.file_command("file.openFolder").unwrap();
        choose(&mut workspace, root.to_str().unwrap());
        drive(&mut workspace, &rx, |w| w.files.folder.is_some());
        assert_eq!(workspace.take_workspace_folder(), Some(root.clone()));
        drop(workspace);
        while !matches!(rx.recv_timeout(Duration::from_secs(15)).unwrap(), DocumentResult::Stopped(Ok(()))) {}
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_commands_picker_lists_supported_encodings_and_no_cr_or_iso2022jp() {
        for label in ENCODINGS {
            Encoding::for_label(label).unwrap();
        }
        assert!(!ENCODINGS.iter().any(|label| label.eq_ignore_ascii_case("ISO-2022-JP")));
        assert_eq!(FilePromptKind::LineEnding.choices(), &["LF", "CRLF"]);
    }
}
