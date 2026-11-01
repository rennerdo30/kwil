//! Explicit save destinations never inherit the process working directory.
use super::*;
use std::path::Path;
use std::sync::OnceLock;

fn home_folder() -> Option<PathBuf> {
    static HOME: OnceLock<Option<PathBuf>> = OnceLock::new();
    HOME.get_or_init(dirs::home_dir).clone()
}
fn documents_folder() -> Option<PathBuf> {
    static DOCUMENTS: OnceLock<Option<PathBuf>> = OnceLock::new();
    DOCUMENTS.get_or_init(dirs::document_dir).clone()
}

fn resolve(base: Option<PathBuf>, path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.into());
    }
    // Windows drive-relative (C:foo) and rooted (\\foo) paths must not ask
    // the OS to fill in a current drive or per-drive working directory.
    if path.components().any(|part| matches!(part, std::path::Component::Prefix(_) | std::path::Component::RootDir)) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Enter a full path or a relative file name"));
    }
    let base = base.filter(|path| path.is_absolute()).ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "Home folder unavailable; enter an absolute save path")
    })?;
    Ok(base.join(path))
}

impl Workspace {
    fn last_file_folder(&self) -> Option<PathBuf> {
        self.files.save_folder.clone().or_else(|| {
            self.session
                .recent()
                .iter()
                .find(|path| path.is_absolute())
                .and_then(|path| path.parent())
                .map(PathBuf::from)
        })
    }

    /// Native chooser start folder: last accepted destination, then Documents,
    /// then home. Never allow a chooser to silently substitute the cwd.
    pub fn native_file_directory(&self) -> io::Result<PathBuf> {
        self.last_file_folder()
            .or_else(documents_folder)
            .or_else(home_folder)
            .filter(|path| path.is_absolute())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Documents and home folders unavailable"))
    }

    /// Shared by the terminal preview and submission, including pasted paths.
    pub fn resolve_typed_save_path(&self, path: &Path) -> io::Result<PathBuf> {
        resolve(self.last_file_folder().or_else(home_folder), path)
    }

    pub(super) fn remember_file_folder(&mut self, path: &Path) {
        if path.is_absolute() {
            self.files.save_folder = path.parent().map(PathBuf::from);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_location_relative_and_absolute_paths_never_use_cwd() {
        let home = std::env::temp_dir().join("kw56-fake-home");
        assert_eq!(resolve(Some(home.clone()), Path::new("notes.txt")).unwrap(), home.join("notes.txt"));
        let absolute = std::env::temp_dir().join("explicit.txt");
        assert_eq!(resolve(None, &absolute).unwrap(), absolute);
        assert!(resolve(None, Path::new("notes.txt")).is_err());
        assert!(resolve(Some(PathBuf::from("relative-home")), Path::new("notes.txt")).is_err());
        #[cfg(windows)]
        for ambiguous in [r"C:notes.txt", r"\notes.txt"] {
            assert!(resolve(Some(home.clone()), Path::new(ambiguous)).is_err());
        }
    }

    #[test]
    fn save_location_typed_submission_queues_the_previewed_absolute_target() {
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        let folder = std::env::temp_dir().join("kw56-fake-last-folder");
        workspace.remember_file_folder(&folder.join("previous.txt"));
        workspace
            .execute("edit.insert", &crate::CommandArgs { text: Some("keep".into()), ..Default::default() }, 1)
            .unwrap();
        workspace.file_command("file.save").unwrap();
        workspace.file_prompt_mut().unwrap().text = "next.txt".into();
        let preview = workspace.resolve_typed_save_path(Path::new("next.txt")).unwrap();
        workspace.submit_file_prompt().unwrap();
        let Some(DocumentRequest::Save { request, .. }) = workspace.files.queue.back() else { panic!("queued save") };
        assert_eq!(request.path, preview);
        assert_eq!(request.path, folder.join("next.txt"));
        assert!(workspace.editor().unwrap().document().is_dirty());
        assert!(workspace.editor().unwrap().document().path().is_none(), "only a successful receipt names the buffer");
    }

    #[test]
    fn save_location_last_folder_drives_native_and_typed_destinations() {
        let mut workspace = Workspace::new(View::new(None)).unwrap();
        let folder = std::env::temp_dir().join("kw56-fake-last-folder");
        workspace.remember_file_folder(&folder.join("previous.txt"));
        assert_eq!(workspace.native_file_directory().unwrap(), folder);
        assert_eq!(workspace.resolve_typed_save_path(Path::new("next.txt")).unwrap(), folder.join("next.txt"));
        workspace.file_command("file.save").unwrap();
        workspace.cancel_file_prompt();
        assert_eq!(workspace.native_file_directory().unwrap(), folder);
    }
}
