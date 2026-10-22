//! One pass per query, no warm-up or benchmark loop. Generate fixtures with
//! spike/textbench's documented gen command; run with --release.
use kwil_core_search::{Options, Pattern, SearchMode, search_with_threads, worker_count};
use kwil_core_text::TextFile;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

fn main() -> std::io::Result<()> {
    let path = std::env::args_os().nth(1).expect("usage: count_fixture <fixture path>");
    let file = TextFile::open(std::path::Path::new(&path))?;
    println!("bytes={} editor_default_workers={} one pass per query/configuration", file.len(), worker_count());
    let queries = [
        ("literal rare: line=20353525", Pattern::literal(b"line=20353525", Options::default())?),
        ("literal common: INFO", Pattern::literal(b"INFO", Options::default())?),
        ("regex: ERROR line=[0-9]{1,8}", Pattern::regex(r"ERROR line=[0-9]{1,8}", Options::default(), 64)?),
    ];
    for (name, pattern) in queries {
        for requested in [1, worker_count()] {
            let cancel = Arc::new(AtomicBool::new(false));
            let start = Instant::now();
            let result = search_with_threads(
                &file,
                &pattern,
                0..file.len(),
                SearchMode::Count,
                requested,
                &cancel,
                |_| {},
                |_| {},
            )?;
            println!(
                "{name}: workers={} count={} elapsed_ms={:.3}",
                result.workers,
                result.count,
                start.elapsed().as_secs_f64() * 1000.0
            );
        }
    }
    Ok(())
}
