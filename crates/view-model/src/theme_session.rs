//! Shared command host. Files and parsing belong to ThemeWorker; accepted
//! settings belong to SettingsSession. Preview never edits either settings or cache.
use crate::{palette_session::PaletteInput, settings_session::SettingsSession};
use kwil_core_commands::{
    settings::ThemeSettings,
    settings_ui::SettingValue,
    theme::{Scheme, Theme, ThemeEntry, ThemeId, ThemeSelection},
    theme_worker::{ThemeRequest, ThemeWorker, ThemeWorkerConfig},
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

pub struct ThemeHost {
    worker: Option<ThemeWorker>,
    catalogue: Arc<[ThemeEntry]>,
    committed: Arc<Theme>,
    pending: Option<Arc<Theme>>,
    selected: ThemeId,
    settings: ThemeSettings,
    scheme: Scheme,
    open: bool,
    import: bool,
    query: String,
    cursor: usize,
    row: usize,
    pub status: String,
}

#[cfg(test)]
#[path = "theme_session_tests.rs"]
mod tests;
impl Default for ThemeHost {
    fn default() -> Self {
        let dark = Arc::new(Theme::builtin(Scheme::Dark));
        Self {
            worker: None,
            catalogue: vec![
                ThemeEntry { id: ThemeId::Dark, theme: dark.clone() },
                ThemeEntry { id: ThemeId::Light, theme: Arc::new(Theme::builtin(Scheme::Light)) },
            ]
            .into(),
            committed: dark,
            pending: None,
            selected: ThemeId::Dark,
            settings: ThemeSettings::default(),
            scheme: Scheme::Dark,
            open: false,
            import: false,
            query: String::new(),
            cursor: 0,
            row: 0,
            status: String::new(),
        }
    }
}
fn id(name: &str) -> ThemeId {
    match name {
        "dark" => ThemeId::Dark,
        "light" => ThemeId::Light,
        name => ThemeId::User(name.into()),
    }
}
pub fn name(id: &ThemeId) -> &str {
    match id {
        ThemeId::Dark => "dark",
        ThemeId::Light => "light",
        ThemeId::User(s) => s,
    }
}
/// The syntax engine currently exposes broad classes; map them explicitly to
/// the theme's matching scopes rather than tinting the old dark colours.
pub fn syntax_rgb(theme: &Theme, class: crate::highlighting::Class) -> u32 {
    let c = theme
        .style(class.name())
        .foreground
        .unwrap_or_else(|| theme.ui().get(kwil_core_commands::theme::UiRole::Foreground));
    (u32::from(c.0) << 16) | (u32::from(c.1) << 8) | u32::from(c.2)
}
impl ThemeHost {
    /// Called after first frame with the existing settings location.
    pub fn start(&mut self, settings: &Path, wake: impl Fn() + Send + 'static) -> Result<(), String> {
        let parent = settings.parent().ok_or("settings path needs a parent")?;
        self.worker = Some(
            ThemeWorker::start(
                ThemeWorkerConfig {
                    user_directory: parent.join("themes"),
                    active_cache: parent.join("theme-active.bin"),
                },
                self.selection(),
                self.scheme,
                wake,
            )
            .map_err(|e| e.to_string())?,
        );
        Ok(())
    }
    fn selection(&self) -> ThemeSelection {
        if self.settings.follow_os {
            ThemeSelection::follow_os()
        } else {
            ThemeSelection::Named(id(&self.settings.name))
        }
    }
    fn request(&mut self, request: ThemeRequest) {
        if let Some(worker) = &self.worker
            && let Err(error) = worker.request(request)
        {
            self.status = error.to_string();
        }
    }
    pub fn settings_ready(&mut self, settings: &ThemeSettings) {
        if self.pending.is_some() || self.status.starts_with("Saving ") {
            self.status = "Theme settings saved".into();
        }
        self.pending = None;
        if &self.settings != settings {
            self.settings = settings.clone();
            self.request(ThemeRequest::Select(self.selection()));
        }
        self.resolve_committed();
    }
    pub fn os_changed(&mut self, scheme: Scheme) {
        if self.scheme != scheme {
            self.scheme = scheme;
            self.request(ThemeRequest::OsChanged(scheme));
            self.resolve_committed();
        }
    }
    fn resolve_committed(&mut self) {
        self.selected = self.selection().resolve(self.scheme).clone();
        if let Some(entry) = self.catalogue.iter().find(|e| e.id == self.selected) {
            self.committed = entry.theme.clone();
        }
    }
    pub fn worker_ready(&mut self) {
        let Some(worker) = &self.worker else { return };
        let snapshot = worker.snapshot();
        self.scheme = snapshot.session.scheme();
        let selected = self.rows().get(self.row).map(|e| e.id.clone());
        if self.status.starts_with("Importing") && snapshot.catalogue.len() > self.catalogue.len() {
            self.status = "Theme imported — use Select Theme to choose it".into();
        }
        self.catalogue = snapshot.catalogue.clone();
        self.committed = snapshot.active.clone();
        self.resolve_committed();
        if let Some(selected) = selected {
            self.row = self.rows().iter().position(|e| e.id == selected).unwrap_or(0);
        }
        if let Some(error) = snapshot.diagnostics.first() {
            self.status = error.clone();
        }
    }
    pub fn is_open(&self) -> bool {
        self.open
    }
    pub fn is_started(&self) -> bool {
        self.worker.is_some()
    }
    pub fn settings_progress(&mut self, settings: &SettingsSession) -> bool {
        if (self.pending.is_some() || self.status.starts_with("Saving "))
            && let Some(error) = settings.last_error()
        {
            self.pending = None;
            self.status = format!("Theme settings: {error}");
            return true;
        }
        false
    }
    pub fn refresh_system(&mut self) {
        self.request(ThemeRequest::RefreshSystem);
    }
    pub fn importing(&self) -> bool {
        self.import
    }
    pub fn query(&self) -> &str {
        &self.query
    }
    pub fn cursor(&self) -> usize {
        self.cursor
    }
    pub fn selected(&self) -> usize {
        self.row
    }
    pub fn rows(&self) -> Vec<&ThemeEntry> {
        let query = self.query.to_lowercase();
        self.catalogue
            .iter()
            .filter(|e| e.theme.name().to_lowercase().contains(&query) || name(&e.id).to_lowercase().contains(&query))
            .collect()
    }
    pub fn active(&self, remote: bool) -> Arc<Theme> {
        let theme = if self.open && !self.import {
            self.rows().get(self.row).map_or(&self.committed, |e| &e.theme)
        } else {
            self.pending.as_ref().unwrap_or(&self.committed)
        };
        let mut displayed = (**theme).clone();
        for (key, value) in &self.settings.colors {
            if let (Some(role), Ok(color)) =
                (kwil_core_commands::theme::UiRole::from_key(key), kwil_core_commands::theme::Rgba::parse(value))
            {
                displayed.set_ui(role, color);
            }
        }
        Arc::new(if remote { displayed.for_remote() } else { displayed })
    }
    pub fn command(&mut self, command: &str, settings: Option<&mut SettingsSession>) -> bool {
        if !matches!(command, "theme.select" | "theme.preview" | "theme.import" | "theme.followSystem") {
            return false;
        }
        self.status.clear();
        if command == "theme.followSystem" {
            let result = settings.ok_or("Settings are starting; try again shortly".to_string()).and_then(|panel| {
                panel.edit_document(|doc| doc.edit("theme.follow_os", SettingValue::Bool(!self.settings.follow_os)))
            });
            self.status = result.err().unwrap_or_else(|| "Saving Follow System Theme…".into());
            return true;
        }
        self.import = command == "theme.import";
        self.open = true;
        self.query.clear();
        self.cursor = 0;
        self.row = self.rows().iter().position(|e| e.id == self.selected).unwrap_or(0);
        self.status = if self.import {
            "Enter a .toml, .json or .tmTheme file path"
        } else {
            "↑ ↓ preview · Enter save · Esc restore"
        }
        .into();
        true
    }
    pub fn select(&mut self, row: usize) {
        self.row = row.min(self.rows().len().saturating_sub(1));
    }
    pub fn input(&mut self, input: PaletteInput<'_>, settings: Option<&mut SettingsSession>) {
        if !self.open {
            return;
        }
        use unicode_segmentation::UnicodeSegmentation;
        match input {
            PaletteInput::Cancel => {
                self.open = false;
                self.status.clear();
            }
            PaletteInput::Next if !self.import => self.select(self.row.saturating_add(1)),
            PaletteInput::Previous if !self.import => self.select(self.row.saturating_sub(1)),
            PaletteInput::Text(text)
                if self.query.len() + text.len() <= 4096 && !text.chars().any(char::is_control) =>
            {
                self.query.insert_str(self.cursor, text);
                self.cursor += text.len();
                self.row = 0;
            }
            PaletteInput::Backspace if self.cursor > 0 => {
                let previous = self.query[..self.cursor].grapheme_indices(true).next_back().map_or(0, |(i, _)| i);
                self.query.replace_range(previous..self.cursor, "");
                self.cursor = previous;
                self.row = 0;
            }
            PaletteInput::Delete if self.cursor < self.query.len() => {
                let end = self.cursor + self.query[self.cursor..].graphemes(true).next().unwrap().len();
                self.query.replace_range(self.cursor..end, "");
                self.row = 0;
            }
            PaletteInput::Left if self.cursor > 0 => {
                self.cursor = self.query[..self.cursor].grapheme_indices(true).next_back().map_or(0, |(i, _)| i)
            }
            PaletteInput::Right if self.cursor < self.query.len() => {
                self.cursor += self.query[self.cursor..].graphemes(true).next().unwrap().len()
            }
            PaletteInput::Home => self.cursor = 0,
            PaletteInput::End => self.cursor = self.query.len(),
            PaletteInput::Accept => {
                if self.import {
                    let path = PathBuf::from(self.query.trim().trim_matches('"'));
                    let result = (|| {
                        let worker =
                            self.worker.as_ref().ok_or("Theme worker is starting; try again shortly".to_string())?;
                        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("imported");
                        let clean: String = stem
                            .chars()
                            .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                            .take(24)
                            .collect();
                        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                        let id = format!(
                            "{}-{}-{}",
                            clean,
                            std::process::id(),
                            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        );
                        worker
                            .request(ThemeRequest::ImportPath { id, path, base: self.committed.scheme() })
                            .map_err(|e| e.to_string())?;
                        Ok::<_, String>(())
                    })();
                    match result {
                        Ok(()) => {
                            self.open = false;
                            self.status = "Importing theme… Use Select Theme to choose it".into();
                        }
                        Err(error) => self.status = error,
                    }
                } else if let Some(entry) = self.rows().get(self.row) {
                    let preview = entry.theme.clone();
                    let chosen = name(&entry.id).to_owned();
                    let result =
                        settings.ok_or("Settings are starting; try again shortly".to_string()).and_then(|panel| {
                            panel.edit_document(|doc| {
                                doc.edit("theme.name", SettingValue::Text(chosen.clone()))?;
                                doc.edit("theme.follow_os", SettingValue::Bool(false))
                            })
                        });
                    match result {
                        Ok(()) => {
                            self.pending = Some(preview);
                            self.open = false;
                            self.status = format!("Saving theme: {chosen}…");
                        }
                        Err(error) => self.status = error,
                    }
                }
            }
            _ => {}
        }
    }
}
