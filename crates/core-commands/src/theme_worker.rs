//! Opt-in post-first-frame worker. One replaceable desired-state slot and one
//! immutable result slot; no work queue, async runtime, polling or idle timer.
pub use crate::theme_parse::ImportFormat;
use crate::{
    theme::*,
    theme_cache::{self, CachedTheme},
    theme_parse,
};
use kwil_core_workspace::FileWatcher;
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
    time::Duration,
};

#[derive(Clone, Debug)]
pub struct ThemeWorkerConfig {
    /// Explicit isolated location, not inferred from settings or environment.
    pub user_directory: PathBuf,
    pub active_cache: PathBuf,
}
#[derive(Clone, Debug)]
pub enum ThemeRequest {
    Reload,
    Preview(ThemeId),
    Cancel,
    Apply,
    Select(ThemeSelection),
    OsChanged(Scheme),
    /// Terminal hosts refresh the OS preference on terminal focus, without polling.
    RefreshSystem,
    /// Captured bytes only. Import never opens external source/include paths.
    /// A new ID is required; existing theme files cannot be overwritten.
    Import {
        id: String,
        format: ImportFormat,
        base: Scheme,
        source: Arc<[u8]>,
    },
    /// Explicit user-selected source. Reading and parsing stay on this worker.
    ImportPath {
        id: String,
        path: PathBuf,
        base: Scheme,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThemeSnapshot {
    pub revision: u64,
    pub catalogue: Arc<[ThemeEntry]>,
    pub active: Arc<Theme>,
    pub session: ThemeSession,
    /// At most 16 diagnostics, each at most 512 UTF-8 bytes.
    pub diagnostics: Arc<[String]>,
}
#[derive(Clone)]
struct Import {
    id: String,
    format: ImportFormat,
    base: Scheme,
    source: Arc<[u8]>,
    path: Option<PathBuf>,
}
#[derive(Clone)]
struct Desired {
    revision: u64,
    reload: u64,
    session: ThemeSession,
    import: Option<Import>,
    refresh_system: bool,
}
struct State {
    desired: Desired,
    pending: bool,
    stop: bool,
    snapshot: Arc<ThemeSnapshot>,
}
struct Shared {
    state: Mutex<State>,
    ready: Condvar,
}
impl Shared {
    fn invalidate(&self) {
        let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if !s.stop {
            s.desired.reload = s.desired.reload.wrapping_add(1);
            s.desired.revision = s.desired.revision.wrapping_add(1);
            s.pending = true;
            self.ready.notify_one();
        }
    }
}
pub struct ThemeWorker {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum PublishStage {
    Cache,
    Snapshot,
}
impl ThemeWorker {
    /// Starts no parsing or disk access on the caller. Host calls after first
    /// frame and wakes its event loop in `wake` (do not draw in the callback).
    pub fn start(
        config: ThemeWorkerConfig,
        selection: ThemeSelection,
        scheme: Scheme,
        wake: impl Fn() + Send + 'static,
    ) -> Result<Self, ThemeError> {
        Self::start_with(config, selection, scheme, None, true, wake, |_| {})
    }
    /// Seed the post-first-frame worker from the already validated startup
    /// cache. Invalid/deleted source themes then retain those displayed pixels
    /// rather than briefly falling back to a different built-in palette.
    pub fn start_cached(
        config: ThemeWorkerConfig,
        cached: CachedTheme,
        scheme: Scheme,
        wake: impl Fn() + Send + 'static,
    ) -> Result<Self, ThemeError> {
        cached.theme.validate()?;
        Self::start_with(config, cached.selection, scheme, Some(cached.theme), true, wake, |_| {})
    }
    fn start_with(
        config: ThemeWorkerConfig,
        selection: ThemeSelection,
        scheme: Scheme,
        initial_theme: Option<Theme>,
        watch_files: bool,
        wake: impl Fn() + Send + 'static,
        before_publish: impl Fn(PublishStage) + Send + 'static,
    ) -> Result<Self, ThemeError> {
        selection.validate()?;
        for path in [&config.user_directory, &config.active_cache] {
            if !path.is_absolute() || path.as_os_str().len() > 4096 {
                return Err(error("theme worker requires bounded absolute paths"));
            }
        }
        if config.active_cache.starts_with(&config.user_directory) {
            return Err(error("active cache must be outside watched theme directory"));
        }
        let session = ThemeSession::new(selection, scheme);
        let catalogue = builtins();
        let initial = initial_theme.map(Arc::new).unwrap_or_else(|| {
            catalogue
                .iter()
                .find(|t| &t.id == session.active_id())
                .map(|t| t.theme.clone())
                .unwrap_or_else(|| Arc::new(Theme::builtin(scheme)))
        });
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                desired: Desired {
                    revision: 1,
                    reload: 1,
                    session: session.clone(),
                    import: None,
                    refresh_system: false,
                },
                pending: true,
                stop: false,
                snapshot: Arc::new(ThemeSnapshot {
                    revision: 0,
                    catalogue: catalogue.into(),
                    active: initial,
                    session,
                    diagnostics: Arc::from([]),
                }),
            }),
            ready: Condvar::new(),
        });
        let inner = shared.clone();
        let worker = thread::Builder::new()
            .name("kwil-theme".into())
            .spawn(move || run(config, inner, watch_files, wake, before_publish))?;
        Ok(Self { shared, worker: Some(worker) })
    }
    /// Reduces actions into a complete desired state before replacing the slot:
    /// Preview -> Apply and Select -> OsChanged cannot lose their earlier action.
    pub fn request(&self, request: ThemeRequest) -> Result<u64, ThemeError> {
        match &request {
            ThemeRequest::Preview(id) => id.validate()?,
            ThemeRequest::Select(s) => s.validate()?,
            ThemeRequest::Import { id, source, .. } => {
                ThemeId::user(id)?;
                if source.len() > MAX_THEME_BYTES {
                    return Err(error("theme import exceeds 1 MiB"));
                }
            }
            ThemeRequest::ImportPath { id, path, .. } => {
                ThemeId::user(id)?;
                if !path.is_absolute() || path.as_os_str().len() > 4096 {
                    return Err(error("theme import requires a bounded absolute path"));
                }
            }
            _ => {}
        }
        let mut s = self.shared.state.lock().unwrap_or_else(|p| p.into_inner());
        if s.stop {
            return Err(error("theme worker stopped"));
        }
        match request {
            ThemeRequest::Reload => s.desired.reload = s.desired.reload.wrapping_add(1),
            ThemeRequest::Preview(id) => s.desired.session.preview(id),
            ThemeRequest::Cancel => s.desired.session.cancel(),
            ThemeRequest::Apply => s.desired.session.apply(),
            ThemeRequest::Select(selection) => s.desired.session.select(selection),
            ThemeRequest::OsChanged(scheme) => s.desired.session.os_changed(scheme),
            ThemeRequest::RefreshSystem => s.desired.refresh_system = true,
            ThemeRequest::Import { id, format, base, source } => {
                s.desired.import = Some(Import { id, format, base, source, path: None })
            }
            ThemeRequest::ImportPath { id, path, base } => {
                s.desired.import = Some(Import {
                    id,
                    path: Some(path),
                    base,
                    format: ImportFormat::VsCodeJson,
                    source: Arc::from([]),
                });
            }
        }
        s.desired.revision = s.desired.revision.wrapping_add(1);
        s.pending = true;
        let revision = s.desired.revision;
        self.shared.ready.notify_one();
        Ok(revision)
    }
    pub fn invalidate(&self) {
        self.shared.invalidate();
    }
    /// One Arc clone; the caller never drains an unbounded result queue.
    pub fn snapshot(&self) -> Arc<ThemeSnapshot> {
        self.shared.state.lock().unwrap_or_else(|p| p.into_inner()).snapshot.clone()
    }
}
impl Drop for ThemeWorker {
    fn drop(&mut self) {
        {
            let mut s = self.shared.state.lock().unwrap_or_else(|p| p.into_inner());
            s.stop = true;
            self.shared.ready.notify_one();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
fn builtins() -> Vec<ThemeEntry> {
    vec![
        ThemeEntry { id: ThemeId::Dark, theme: Arc::new(Theme::builtin(Scheme::Dark)) },
        ThemeEntry { id: ThemeId::Light, theme: Arc::new(Theme::builtin(Scheme::Light)) },
    ]
}
fn diagnostic(out: &mut Vec<String>, message: impl ToString) {
    if out.len() >= 16 {
        return;
    }
    let mut text = message.to_string();
    let mut end = text.len().min(512);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    out.push(text);
}
fn read_source(path: &Path) -> Result<Vec<u8>, ThemeError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > MAX_THEME_BYTES as u64 {
        return Err(error("theme must be a regular, non-symlink file <=1 MiB"));
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() || file.metadata()?.file_type().is_symlink() {
        return Err(error("theme is not a regular non-symlink file"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_THEME_BYTES as u64 + 1).read_to_end(&mut bytes)?;
    theme_parse::bounded_source(&bytes)?;
    Ok(bytes)
}
fn catalogue(
    directory: &Path,
    previous: &[ThemeEntry],
    diagnostics: &mut Vec<String>,
) -> Result<Vec<ThemeEntry>, ThemeError> {
    let metadata = fs::symlink_metadata(directory)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(error("theme directory must be a real directory"));
    }
    let mut paths = Vec::new();
    for (n, entry) in fs::read_dir(directory)?.enumerate() {
        if n >= 1024 {
            return Err(error("theme directory exceeds 1024 entries"));
        }
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("toml") {
            continue;
        }
        let id =
            ThemeId::user(path.file_stem().and_then(|s| s.to_str()).ok_or_else(|| error("theme ID is not UTF-8"))?)?;
        paths.push((id, path));
        if paths.len() + 2 > MAX_THEMES {
            return Err(error("theme catalogue exceeds 256 entries including built-ins"));
        }
    }
    paths.sort_by(|a, b| a.1.cmp(&b.1));
    let mut out = builtins();
    for (id, path) in paths {
        match read_source(&path).and_then(|b| theme_parse::parse_toml(&b)) {
            Ok(theme) => out.push(ThemeEntry { id, theme: Arc::new(theme) }),
            Err(e) => {
                diagnostic(diagnostics, format!("{}: {e}", path.file_name().unwrap().to_string_lossy()));
                if let Some(old) = previous.iter().find(|t| t.id == id) {
                    out.push(old.clone());
                }
            }
        }
    }
    Ok(out)
}
fn run(
    config: ThemeWorkerConfig,
    shared: Arc<Shared>,
    watch_files: bool,
    wake: impl Fn(),
    before_publish: impl Fn(PublishStage),
) {
    let mut startup_errors = Vec::new();
    let setup = (|| {
        fs::create_dir_all(&config.user_directory)?;
        fs::create_dir_all(config.active_cache.parent().ok_or_else(|| error("cache needs parent"))?)?;
        Ok::<_, ThemeError>(())
    })();
    if let Err(e) = setup {
        diagnostic(&mut startup_errors, e);
    }
    // Public startup always enables the native watcher. Contract tests may
    // instead provide explicit invalidations so OS event delivery is not
    // mistaken for spontaneous worker activity.
    let _watcher = if watch_files {
        let invalidations = shared.clone();
        match FileWatcher::directory(&config.user_directory, Duration::from_millis(30), move |_| {
            invalidations.invalidate()
        }) {
            Ok(watcher) => Some(watcher),
            Err(e) => {
                diagnostic(&mut startup_errors, format!("theme live reload unavailable: {e}"));
                None
            }
        }
    } else {
        None
    };
    let mut reload = 0;
    let mut themes = builtins();
    let mut last_written: Option<CachedTheme> = None;
    // A displayed hover preview must never become the fallback for a missing
    // committed source. Seed this independently from the validated startup view.
    let mut last_committed = shared.state.lock().unwrap_or_else(|p| p.into_inner()).snapshot.active.clone();
    // Selection is supplied by accepted settings. Never replace its persisted
    // fallback with a default before the selected source has been resolved.
    if let Ok(Some(cached)) = theme_cache::read_active_cache(&config.active_cache)
        && cached.selection
            == shared.state.lock().unwrap_or_else(|p| p.into_inner()).desired.session.selection().clone()
    {
        last_committed = Arc::new(cached.theme);
    }
    let mut reload_errors = Vec::new();
    let mut import_errors = Vec::new();
    loop {
        let mut wanted = {
            let mut s = shared.state.lock().unwrap_or_else(|p| p.into_inner());
            while !s.pending && !s.stop {
                s = shared.ready.wait(s).unwrap_or_else(|p| p.into_inner());
            }
            if !s.pending && s.stop {
                break;
            }
            s.pending = false;
            let wanted = s.desired.clone();
            s.desired.import = None;
            s.desired.refresh_system = false;
            wanted
        };
        if wanted.refresh_system {
            let scheme = system_scheme();
            wanted.session.os_changed(scheme);
            let mut s = shared.state.lock().unwrap_or_else(|p| p.into_inner());
            if s.desired.revision == wanted.revision {
                s.desired.session.os_changed(scheme);
            }
        }
        let mut diagnostics = startup_errors.clone();
        let mut import_changed = false;
        if let Some(import) = wanted.import {
            import_errors.clear();
            let result = (|| {
                if let Some(path) = &import.path {
                    let source = read_source(path)?;
                    match path.extension().and_then(|s| s.to_str()).unwrap_or("").to_ascii_lowercase().as_str() {
                        "toml" => theme_parse::parse_toml(&source),
                        "json" => theme_parse::import(ImportFormat::VsCodeJson, import.base, &source),
                        "tmtheme" => theme_parse::import(ImportFormat::TextMate, import.base, &source),
                        _ => Err(error("choose a .toml, .json or .tmTheme theme file")),
                    }
                } else {
                    theme_parse::import(import.format, import.base, &import.source)
                }
            })()
            .and_then(|theme| theme_parse::to_toml(&theme))
            .and_then(|source| {
                // Parsing is complete before creating an output. Atomic creation
                // refuses an existing ID; user themes are never replaced.
                let path = config.user_directory.join(format!("{}.toml", import.id));
                theme_cache::atomic_write_new(&path, source.as_bytes())
            });
            match result {
                Ok(()) => import_changed = true,
                Err(e) => diagnostic(&mut import_errors, e),
            }
        }
        for message in &import_errors {
            diagnostic(&mut diagnostics, message);
        }
        if wanted.reload != reload || import_changed {
            reload_errors.clear();
            match catalogue(&config.user_directory, &themes, &mut reload_errors) {
                Ok(catalogue) => themes = catalogue,
                Err(e) => diagnostic(&mut reload_errors, e),
            }
            reload = wanted.reload;
        }
        for message in &reload_errors {
            diagnostic(&mut diagnostics, message);
        }
        let find = |id: &ThemeId| themes.iter().find(|t| &t.id == id).map(|t| t.theme.clone());
        let active = find(wanted.session.active_id());
        let committed = find(wanted.session.committed_id());
        if active.is_none() || committed.is_none() {
            diagnostic(&mut diagnostics, "selected theme is unavailable; retaining last valid committed theme/cache");
        }
        // Never publish an obsolete completion. All slow work is outside the
        // mutex; requests can always replace the desired slot promptly.
        before_publish(PublishStage::Cache);
        if shared.state.lock().unwrap_or_else(|p| p.into_inner()).desired.revision != wanted.revision {
            continue;
        }
        if let Some(theme) = committed {
            last_committed = theme.clone();
            let cached = CachedTheme { selection: wanted.session.selection().clone(), theme: (*theme).clone() };
            if last_written.as_ref() != Some(&cached) {
                match theme_cache::encode(&cached)
                    .and_then(|bytes| theme_cache::atomic_write(&config.active_cache, &bytes))
                {
                    Ok(()) => last_written = Some(cached),
                    Err(e) => diagnostic(&mut diagnostics, format!("active theme cache: {e}")),
                }
            }
        }
        before_publish(PublishStage::Snapshot);
        let published = {
            let mut s = shared.state.lock().unwrap_or_else(|p| p.into_inner());
            if s.desired.revision != wanted.revision {
                false
            } else {
                let active = active.unwrap_or_else(|| last_committed.clone());
                s.snapshot = Arc::new(ThemeSnapshot {
                    revision: wanted.revision,
                    catalogue: themes.clone().into(),
                    active,
                    session: wanted.session,
                    diagnostics: diagnostics.into(),
                });
                true
            }
        };
        if published {
            wake();
        }
    }
}

fn system_scheme() -> Scheme {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
        let key: Vec<u16> =
            "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize\0".encode_utf16().collect();
        let value: Vec<u16> = "AppsUseLightTheme\0".encode_utf16().collect();
        let mut data = 0u32;
        let mut size = 4u32;
        let result = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_DWORD,
                std::ptr::null_mut(),
                (&mut data as *mut u32).cast(),
                &mut size,
            )
        };
        if result == 0 {
            return if data == 0 { Scheme::Dark } else { Scheme::Light };
        }
    }
    // Terminals without an OS preference API commonly expose their background.
    if std::env::var("COLORFGBG")
        .ok()
        .and_then(|v| v.rsplit(';').next().and_then(|n| n.parse::<u8>().ok()))
        .is_some_and(|n| n >= 7)
    {
        Scheme::Light
    } else {
        Scheme::Dark
    }
}

#[cfg(test)]
#[path = "theme_worker_tests.rs"]
mod tests;
