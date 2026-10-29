use super::*;
use kwil_core_commands::{
    settings::Settings,
    theme::{Rgba, UiRole},
};
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

struct Fixture {
    root: PathBuf,
    settings: SettingsSession,
    themes: ThemeHost,
    wake: mpsc::Receiver<()>,
}
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "kwil-theme-commands-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("settings.toml");
        std::fs::write(
            &path,
            "# keep this comment\n[editor]\ntab_size = 7\n[theme]\nname = 'dark'\nfollow_os = false\n",
        )
        .unwrap();
        let (sender, wake) = mpsc::sync_channel(1);
        let tx = sender.clone();
        let settings = SettingsSession::start(
            path,
            Arc::new(move || {
                let _ = tx.try_send(());
            }),
        )
        .unwrap();
        let mut fixture = Self { root, settings, themes: ThemeHost::default(), wake };
        fixture.wait(|f| f.settings.document().is_some());
        fixture
            .themes
            .start(fixture.settings.path(), move || {
                let _ = sender.try_send(());
            })
            .unwrap();
        fixture.wait(|f| f.root.join("theme-active.bin").exists());
        fixture
    }
    fn pump(&mut self) {
        self.settings.worker_ready();
        if let Some(applied) = self.settings.take_applied() {
            self.themes.settings_ready(&applied.settings.theme);
        }
        self.themes.settings_progress(&self.settings);
        self.themes.worker_ready();
    }
    fn wait(&mut self, condition: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            self.pump();
            if condition(self) {
                return;
            }
            self.wake
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("theme/settings event deadline");
        }
    }
    fn command(&mut self, id: &str) {
        assert!(self.themes.command(id, Some(&mut self.settings)));
    }
    fn input(&mut self, input: PaletteInput<'_>) {
        self.themes.input(input, Some(&mut self.settings));
    }
    fn saved(&self) -> Settings {
        Settings::load(self.settings.path()).unwrap().settings
    }
    fn cache(&self) -> Option<kwil_core_commands::theme_cache::CachedTheme> {
        kwil_core_commands::theme_cache::read_active_cache(&self.root.join("theme-active.bin")).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.themes.worker.take();
        // SettingsSession's ordinary owned shutdown preserves accepted writes.
        self.settings.stop().shutdown().unwrap();
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}

#[test]
fn theme_picker_previews_cancels_and_persists_only_on_enter() {
    let mut f = Fixture::new();
    let original = std::fs::read(f.settings.path()).unwrap();
    let cache = f.cache();
    f.command("theme.select");
    f.input(PaletteInput::Next);
    assert_eq!(f.themes.active(false).scheme(), Scheme::Light);
    assert_eq!(std::fs::read(f.settings.path()).unwrap(), original);
    assert_eq!(f.cache(), cache, "hover must never write the active cache");
    f.input(PaletteInput::Cancel);
    assert_eq!(f.themes.active(false).scheme(), Scheme::Dark);
    assert_eq!(std::fs::read(f.settings.path()).unwrap(), original);
    f.command("theme.preview");
    f.input(PaletteInput::Next);
    f.input(PaletteInput::Accept);
    assert_eq!(f.themes.active(false).scheme(), Scheme::Light, "accepted preview stays visible during save");
    f.wait(|f| f.saved().theme.name == "light" && f.cache().is_some_and(|c| c.theme.scheme() == Scheme::Light));
    assert!(!f.themes.is_open());
    assert!(!f.saved().theme.follow_os);
    assert_eq!(f.saved().editor.tab_size, 7);
    assert!(std::fs::read_to_string(f.settings.path()).unwrap().contains("# keep this comment"));
}

#[test]
fn theme_import_path_enters_catalogue_and_saved_selection_survives_missing_source() {
    let mut f = Fixture::new();
    let path = f.root.join("ocean.toml");
    let source = "version=1\nname='Ocean'\nbase='dark'\n[ui]\n'editor.background'='#123456'\n[syntax.tag]\nforeground='#abcdef'\n";
    std::fs::write(&path, source).unwrap();
    f.command("theme.import");
    f.input(PaletteInput::Text(path.to_str().unwrap()));
    f.input(PaletteInput::Accept);
    f.wait(|f| f.themes.catalogue.len() == 3);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
    f.command("theme.select");
    f.input(PaletteInput::Text("Ocean"));
    assert_eq!(f.themes.rows().len(), 1);
    let theme = f.themes.active(false);
    assert_eq!(theme.ui().get(UiRole::Background), Rgba::rgb(0x123456));
    assert_eq!(syntax_rgb(&theme, crate::highlighting::Class::Tag), 0xabcdef);
    f.input(PaletteInput::Accept);
    f.wait(|f| f.saved().theme.name.starts_with("ocean-") && f.cache().is_some_and(|c| c.theme.name() == "Ocean"));
    let name = f.saved().theme.name;
    std::fs::remove_file(f.root.join("themes").join(format!("{name}.toml"))).unwrap();
    f.themes.worker.take();
    let (tx, wake) = mpsc::sync_channel(1);
    f.themes
        .start(f.settings.path(), move || {
            let _ = tx.try_send(());
        })
        .unwrap();
    wake.recv_timeout(Duration::from_secs(5)).unwrap();
    f.themes.worker_ready();
    assert_eq!(f.themes.active(false).ui().get(UiRole::Background), Rgba::rgb(0x123456));
    assert!(f.themes.status.contains("unavailable"));
}

#[test]
fn theme_follow_system_saves_toggle_and_cancel_resolves_latest_os_side() {
    let mut f = Fixture::new();
    f.command("theme.followSystem");
    f.wait(|f| f.saved().theme.follow_os && f.themes.settings.follow_os);
    f.themes.os_changed(Scheme::Light);
    assert_eq!(f.themes.active(false).scheme(), Scheme::Light);
    f.command("theme.select");
    f.input(PaletteInput::Text("dark"));
    assert_eq!(f.themes.active(false).scheme(), Scheme::Dark);
    f.themes.os_changed(Scheme::Light);
    f.input(PaletteInput::Cancel);
    assert_eq!(f.themes.active(false).scheme(), Scheme::Light);
    f.command("theme.followSystem");
    f.wait(|f| !f.saved().theme.follow_os && !f.themes.settings.follow_os);
}

#[test]
fn theme_bad_import_reports_error_and_keeps_committed_theme() {
    let mut f = Fixture::new();
    let path = f.root.join("bad.json");
    std::fs::write(&path, "{bad json}").unwrap();
    f.command("theme.import");
    f.input(PaletteInput::Text(path.to_str().unwrap()));
    f.input(PaletteInput::Accept);
    f.wait(|f| f.themes.status.contains("strict JSON"));
    assert_eq!(f.themes.catalogue.len(), 2);
    assert_eq!(f.themes.active(false).scheme(), Scheme::Dark);
    assert_eq!(f.saved().theme.name, "dark");
}

#[test]
fn theme_path_imports_use_existing_json_and_textmate_parsers() {
    let mut f = Fixture::new();
    for (index, (file, source, label)) in [
        ("json-theme.json", r##"{"name":"JSON Ocean","type":"dark","colors":{"editor.background":"#112233"}}"##, "JSON Ocean"),
        ("plist-theme.tmTheme", r##"<?xml version="1.0"?><plist version="1.0"><dict><key>name</key><string>Plist Ocean</string><key>settings</key><array><dict><key>settings</key><dict><key>background</key><string>#223344</string></dict></dict></array></dict></plist>"##, "Plist Ocean"),
    ].into_iter().enumerate() {
        let path = f.root.join(file);
        std::fs::write(&path, source).unwrap();
        f.command("theme.import");
        f.input(PaletteInput::Text(path.to_str().unwrap()));
        f.input(PaletteInput::Accept);
        f.wait(|f| f.themes.catalogue.len() == index + 3);
        assert!(f.themes.catalogue.iter().any(|entry| entry.theme.name() == label));
    }
}

#[test]
fn theme_failed_settings_save_restores_previous_theme_and_explains_error() {
    let mut f = Fixture::new();
    std::fs::rename(f.settings.path(), f.root.join("preserved-settings.toml")).unwrap();
    std::fs::create_dir(f.settings.path()).unwrap();
    f.command("theme.select");
    f.input(PaletteInput::Next);
    f.input(PaletteInput::Accept);
    f.wait(|f| f.settings.last_error().is_some());
    assert_eq!(f.themes.active(false).scheme(), Scheme::Dark);
    assert!(f.themes.status.contains("Theme settings"));
    assert!(f.root.join("preserved-settings.toml").is_file());
    // The explicit failed-save contract refuses shutdown; do not treat it as
    // an unexpected teardown panic for this fixture.
    let _ = f.settings.stop().shutdown();
}
