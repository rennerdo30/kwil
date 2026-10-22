use super::*;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    mpsc,
};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "kwil-theme-contract-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        Self(root)
    }
    fn config(&self) -> ThemeWorkerConfig {
        ThemeWorkerConfig { user_directory: self.0.join("themes"), active_cache: self.0.join("active.bin") }
    }
    fn start(&self, selection: ThemeSelection) -> (ThemeWorker, mpsc::Receiver<()>) {
        let (tx, rx) = mpsc::channel();
        (
            ThemeWorker::start(self.config(), selection, Scheme::Dark, move || {
                let _ = tx.send(());
            })
            .unwrap(),
            rx,
        )
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn wait(worker: &ThemeWorker, rx: &mpsc::Receiver<()>, revision: u64) -> Arc<ThemeSnapshot> {
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    loop {
        let snapshot = worker.snapshot();
        if snapshot.revision >= revision {
            return snapshot;
        }
        rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())).expect("bounded worker wake");
    }
}
#[test]
fn theme_worker_preview_never_persists_and_latest_actions_survive_contract() {
    let f = Fixture::new();
    let (worker, rx) = f.start(ThemeSelection::Named(ThemeId::Dark));
    wait(&worker, &rx, 1);
    let before = fs::read(f.config().active_cache).unwrap();
    let revision = worker.request(ThemeRequest::Preview(ThemeId::Light)).unwrap();
    let snapshot = wait(&worker, &rx, revision);
    assert_eq!(snapshot.active.scheme(), Scheme::Light);
    assert_eq!(fs::read(f.config().active_cache).unwrap(), before);
    let revision = worker.request(ThemeRequest::Cancel).unwrap();
    assert_eq!(wait(&worker, &rx, revision).active.scheme(), Scheme::Dark);
    worker.request(ThemeRequest::Preview(ThemeId::Light)).unwrap();
    let revision = worker.request(ThemeRequest::Apply).unwrap();
    assert_eq!(wait(&worker, &rx, revision).active.scheme(), Scheme::Light);
    assert_eq!(
        theme_cache::read_active_cache(&f.config().active_cache).unwrap().unwrap().selection,
        ThemeSelection::Named(ThemeId::Light)
    );
    worker.request(ThemeRequest::Select(ThemeSelection::follow_os())).unwrap();
    let revision = worker.request(ThemeRequest::OsChanged(Scheme::Light)).unwrap();
    assert_eq!(wait(&worker, &rx, revision).active.scheme(), Scheme::Light);
    assert_eq!(
        theme_cache::read_active_cache(&f.config().active_cache).unwrap().unwrap().selection,
        ThemeSelection::follow_os()
    );
}
#[test]
fn theme_worker_catalogue_bound_is_transactional_contract() {
    let f = Fixture::new();
    fs::create_dir(f.config().user_directory.clone()).unwrap();
    // Finite 255 tiny files exercise the actual 256 catalogue cap (including
    // the two built-ins); this is not a workload generator or stress loop.
    for n in 0..255 {
        fs::write(f.config().user_directory.join(format!("t{n}.toml")), b"version=1\nname='Fixture'\nbase='dark'")
            .unwrap();
    }
    let (worker, rx) = f.start(ThemeSelection::Named(ThemeId::Dark));
    let snapshot = wait(&worker, &rx, 1);
    assert_eq!(snapshot.catalogue.len(), 2);
    assert!(snapshot.diagnostics.iter().any(|s| s.contains("256")));
    fs::remove_file(f.config().user_directory.join("t254.toml")).unwrap();
    let revision = worker.request(ThemeRequest::Reload).unwrap();
    assert_eq!(wait(&worker, &rx, revision).catalogue.len(), MAX_THEMES);
}
#[test]
fn theme_worker_live_folder_reload_retains_last_good_contract() {
    let f = Fixture::new();
    let (worker, rx) = f.start(ThemeSelection::Named(ThemeId::Dark));
    let first = wait(&worker, &rx, 1);
    assert!(first.diagnostics.is_empty(), "{:?}", first.diagnostics);
    let path = f.config().user_directory.join("fixture.toml");
    fs::write(&path, b"version=1\nname='Live'\nbase='light'").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    loop {
        if worker.snapshot().catalogue.iter().any(|e| e.id == ThemeId::User("fixture".into())) {
            break;
        }
        rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            .expect("native folder create wake");
    }
    let revision =
        worker.request(ThemeRequest::Select(ThemeSelection::Named(ThemeId::User("fixture".into())))).unwrap();
    assert_eq!(wait(&worker, &rx, revision).active.name(), "Live");
    fs::write(&path, b"broken TOML").unwrap();
    let revision = worker.request(ThemeRequest::Reload).unwrap();
    let snapshot = wait(&worker, &rx, revision);
    assert_eq!(snapshot.active.name(), "Live");
    assert!(!snapshot.diagnostics.is_empty());
    fs::remove_file(&path).unwrap();
    let revision = worker.request(ThemeRequest::Reload).unwrap();
    let snapshot = wait(&worker, &rx, revision);
    assert_eq!(snapshot.active.name(), "Live");
    assert!(!snapshot.catalogue.iter().any(|e| e.id == ThemeId::User("fixture".into())));
}
#[test]
fn theme_worker_import_is_bounded_no_clobber_and_round_trips_contract() {
    let f = Fixture::new();
    let (worker, rx) = f.start(ThemeSelection::Named(ThemeId::Dark));
    wait(&worker, &rx, 1);
    assert!(
        worker
            .request(ThemeRequest::Import {
                id: "../escape".into(),
                format: ImportFormat::VsCodeJson,
                base: Scheme::Dark,
                source: Arc::from(&b"{}"[..])
            })
            .is_err()
    );
    assert!(
        worker
            .request(ThemeRequest::Import {
                id: "fixture".into(),
                format: ImportFormat::VsCodeJson,
                base: Scheme::Dark,
                source: vec![0; MAX_THEME_BYTES + 1].into()
            })
            .is_err()
    );
    let request = || ThemeRequest::Import {
        id: "fixture".into(),
        format: ImportFormat::VsCodeJson,
        base: Scheme::Dark,
        source: Arc::from(&br##"{"name":"Imported","colors":{"editor.background":"#1234"}}"##[..]),
    };
    let revision = worker.request(request()).unwrap();
    let snapshot = wait(&worker, &rx, revision);
    assert!(snapshot.catalogue.iter().any(|t| t.theme.name() == "Imported"), "{:?}", snapshot.diagnostics);
    let path = f.config().user_directory.join("fixture.toml");
    let before = fs::read(&path).unwrap();
    let revision = worker
        .request(ThemeRequest::Import {
            id: "fixture".into(),
            format: ImportFormat::VsCodeJson,
            base: Scheme::Dark,
            source: Arc::from(&br##"{"name":"Must not overwrite","colors":{"editor.background":"#ff0000"}}"##[..]),
        })
        .unwrap();
    let snapshot = wait(&worker, &rx, revision);
    assert!(!snapshot.diagnostics.is_empty());
    assert_eq!(fs::read(&path).unwrap(), before);
}
#[test]
fn theme_worker_sleep_and_snapshot_bound_contract() {
    let f = Fixture::new();
    let (worker, rx) = f.start(ThemeSelection::Named(ThemeId::Dark));
    wait(&worker, &rx, 1);
    while rx.try_recv().is_ok() {}
    // A single bounded quiet observation, not a polling loop.
    assert!(matches!(rx.recv_timeout(Duration::from_millis(100)), Err(mpsc::RecvTimeoutError::Timeout)));
    let before = worker.snapshot();
    let revision = worker.request(ThemeRequest::Preview(ThemeId::Light)).unwrap();
    let after = wait(&worker, &rx, revision);
    assert!(!Arc::ptr_eq(&before, &after));
    assert!(Arc::ptr_eq(&after, &worker.snapshot()));
    assert!(after.catalogue.len() <= MAX_THEMES && after.diagnostics.len() <= 16);
    drop(worker); // idle worker exits without another file event/timer.
}
#[test]
fn theme_worker_startup_cache_reader_never_parses_sources_contract() {
    let f = Fixture::new();
    let cache = f.config().active_cache;
    assert!(theme_cache::read_active_cache(&cache).unwrap().is_none());
    let cached = CachedTheme { selection: ThemeSelection::Named(ThemeId::Light), theme: Theme::builtin(Scheme::Light) };
    theme_cache::atomic_write(&cache, &theme_cache::encode(&cached).unwrap()).unwrap();
    assert_eq!(theme_cache::read_active_cache(&cache).unwrap(), Some(cached));
    fs::write(&cache, b"version=1\nname='Must not parse'\nbase='dark'").unwrap();
    assert!(theme_cache::read_active_cache(&cache).is_err());
    let file = fs::File::create(&cache).unwrap();
    file.set_len(theme_cache::MAX_CACHE_BYTES as u64 + 1).unwrap();
    assert!(theme_cache::read_active_cache(&cache).is_err());
}

fn obsolete_completion_at(stage: PublishStage) {
    let f = Fixture::new();
    let (at_tx, at_rx) = mpsc::channel();
    let (go_tx, go_rx) = mpsc::channel();
    let go_rx = Mutex::new(go_rx);
    let first = std::sync::atomic::AtomicBool::new(true);
    let (wake_tx, wake_rx) = mpsc::channel();
    let (seen_tx, seen_rx) = mpsc::channel();
    let seen_rx = Mutex::new(seen_rx);
    let (cache_tx, cache_rx) = mpsc::channel();
    let cache_path = f.config().active_cache;
    let worker = ThemeWorker::start_with(
        f.config(),
        ThemeSelection::Named(ThemeId::Dark),
        Scheme::Dark,
        None,
        move || {
            let _ = wake_tx.send(());
            // Hold publication stable until the observer has cloned it. A fast
            // newer completion must not hide an obsolete wake in a mutant.
            let _ = seen_rx.lock().unwrap().recv_timeout(Duration::from_secs(8));
        },
        move |at| {
            if at == stage && first.swap(false, Ordering::AcqRel) {
                at_tx.send(()).unwrap();
                go_rx.lock().unwrap().recv_timeout(Duration::from_secs(8)).unwrap();
            }
            if stage == PublishStage::Cache && at == PublishStage::Snapshot {
                cache_tx.send(theme_cache::read_active_cache(&cache_path).unwrap().unwrap().theme.scheme()).unwrap();
            }
        },
    )
    .unwrap();
    at_rx.recv_timeout(Duration::from_secs(8)).unwrap();
    // The worker is suspended after parsing the first request, before cache or
    // snapshot publication. Both newer actions must be retained in one slot.
    worker.request(ThemeRequest::Preview(ThemeId::Light)).unwrap();
    let revision = worker.request(ThemeRequest::Apply).unwrap();
    assert_eq!(worker.snapshot().revision, 0);
    go_tx.send(()).unwrap();
    if stage == PublishStage::Cache {
        assert_eq!(
            cache_rx.recv_timeout(Duration::from_secs(8)).unwrap(),
            Scheme::Light,
            "obsolete cache write must be skipped before snapshot guard"
        );
    }
    wake_rx.recv_timeout(Duration::from_secs(8)).unwrap();
    let snapshot = worker.snapshot();
    seen_tx.send(()).unwrap();
    assert_eq!(snapshot.revision, revision, "obsolete result must never wake the UI");
    assert_eq!(snapshot.active.scheme(), Scheme::Light);
    assert_eq!(
        theme_cache::read_active_cache(&f.config().active_cache).unwrap().unwrap().theme.scheme(),
        Scheme::Light
    );
}

#[test]
fn theme_worker_obsolete_cache_write_is_skipped_contract() {
    obsolete_completion_at(PublishStage::Cache);
}
#[test]
fn theme_worker_obsolete_completion_is_not_published_contract() {
    obsolete_completion_at(PublishStage::Snapshot);
}

#[test]
fn theme_worker_keeps_startup_cache_when_source_is_invalid_contract() {
    let f = Fixture::new();
    fs::create_dir(f.config().user_directory).unwrap();
    fs::write(f.config().user_directory.join("fixture.toml"), b"not TOML").unwrap();
    let mut theme = Theme::builtin(Scheme::Light);
    theme.name = "Cached fixture".into();
    let cached = CachedTheme { selection: ThemeSelection::Named(ThemeId::user("fixture").unwrap()), theme };
    theme_cache::atomic_write(&f.config().active_cache, &theme_cache::encode(&cached).unwrap()).unwrap();
    let (tx, rx) = mpsc::channel();
    let worker = ThemeWorker::start_cached(f.config(), cached.clone(), Scheme::Dark, move || {
        let _ = tx.send(());
    })
    .unwrap();
    let snapshot = wait(&worker, &rx, 1);
    assert_eq!(*snapshot.active, cached.theme);
    assert!(!snapshot.diagnostics.is_empty());
    assert_eq!(theme_cache::read_active_cache(&f.config().active_cache).unwrap(), Some(cached));
}
