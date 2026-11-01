//! One query through the shipped catalogue and matcher, after discovery settles.
use kwil_core_commands::palette::{Palette, PaletteMode, PaletteResults, PaletteWorker};
use kwil_core_workspace::catalogue::{self, Cancellation, WalkEnd, WalkLimits};
use std::io;
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

pub fn measure(root: &Path, files: usize) -> io::Result<serde_json::Value> {
    let (tx, rx) = mpsc::sync_channel(1);
    let mut worker = PaletteWorker::new(
        Default::default(),
        Arc::new(move || {
            let _ = tx.try_send(());
        }),
    )?;
    let result = (|| {
        let mut feed = worker.feed();
        let report = catalogue::walk(root, WalkLimits::default(), &Cancellation::default(), |path| {
            let label = path.strip_prefix(root).unwrap().to_string_lossy().into_owned().into();
            if feed.push(path, label).is_err() { ControlFlow::Break(()) } else { ControlFlow::Continue(()) }
        });
        if report.end != WalkEnd::Complete || report.errors != 0 || report.files != files {
            return Err(io::Error::other(format!("incomplete palette fixture: {report:?}")));
        }
        // A blank query acknowledges the last feed batch; discovery is excluded
        // from query-to-results time. Never time acceptance of an old cache.
        let mut palette = Palette::new(32);
        let initial = palette.open("").map_err(|e| io::Error::other(format!("{e:?}")))?.unwrap();
        worker.request(initial.clone()).map_err(|e| io::Error::other(format!("{e:?}")))?;
        wait(&worker, &rx, |r| r.generation == initial.generation && r.query.is_empty() && r.total == files)?;

        let started = Instant::now();
        let query = palette.set_query("file-0000").map_err(|e| io::Error::other(format!("{e:?}")))?.unwrap();
        worker.request(query.clone()).map_err(|e| io::Error::other(format!("{e:?}")))?;
        let results = wait(&worker, &rx, |r| r.generation == query.generation && r.query == query.query)?;
        if !palette.apply_results(results.clone()) || !palette.results_are_fresh() || results.total == 0 {
            return Err(io::Error::other("palette did not apply fresh nonempty results"));
        }
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        Ok(serde_json::json!({"files": files, "query": query.query.as_ref(), "matches": results.total,
            "rows": results.rows.len(), "query_results_ms": elapsed_ms, "target_ms": 16,
            "within_target": elapsed_ms <= 16.0}))
    })();
    if worker.stop().unwrap().join().is_err() {
        return Err(io::Error::other("palette worker panicked"));
    }
    result
}

fn wait(
    worker: &PaletteWorker,
    rx: &mpsc::Receiver<()>,
    ready: impl Fn(&PaletteResults) -> bool,
) -> io::Result<Arc<PaletteResults>> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let results = worker.latest();
        if results.mode == PaletteMode::Files && ready(&results) {
            return Ok(results);
        }
        rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).map_err(io::Error::other)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tiny_tree_measures_fresh_query_instead_of_blank_catalogue_cache() {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "kwil-bench-palette-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        crate::fixtures::tree(&root, 2, 3).unwrap();
        let result = measure(&root, 6).unwrap();
        assert_eq!(result["query"], "file-0000");
        assert_eq!(result["files"], 6);
        assert_eq!(result["matches"], 2, "must measure fresh filtered results, not the six cached files");
        assert_eq!(result["rows"], 2);
        assert!(result["query_results_ms"].as_f64().unwrap().is_finite());
        let resolved = root.canonicalize().unwrap();
        assert_eq!(resolved.parent(), Some(std::env::temp_dir().canonicalize().unwrap().as_path()));
        std::fs::remove_dir_all(resolved).unwrap();
    }
}
