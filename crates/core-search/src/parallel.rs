use crate::search::{BLOCK, invalid};
use crate::{Cancellation, Haystack, Match, Pattern, Progress, SearchMode, SearchSummary, search, worker_count};
use std::cell::Cell;
use std::io;
use std::ops::Range;
use std::sync::atomic::Ordering;
use std::sync::mpsc::sync_channel;

/// Editor default: half the logical CPUs, capped at four. Patterns proven not
/// to consume LF are partitioned at nearby line boundaries; other patterns or
/// files without nearby boundaries use the serial engine. Sharding arbitrary
/// byte ranges would change non-overlap semantics (e.g. `aa` in `aaaaa`).
pub fn search_default<H: Haystack + Sync + ?Sized>(
    source: &H,
    pattern: &Pattern,
    range: Range<u64>,
    mode: SearchMode,
    cancel: &Cancellation,
    on_hit: impl FnMut(Match),
    progress: impl FnMut(Progress),
) -> io::Result<SearchSummary> {
    search_with_threads(source, pattern, range, mode, worker_count(), cancel, on_hit, progress)
}

/// Explicit worker override for measurement or caller admission, capped by the
/// same headroom policy as the default. Count progress is merged immediately;
/// hit batches are bounded and drained in shard order, applying backpressure
/// to later shards instead of buffering a file's worth of out-of-order hits.
#[allow(clippy::too_many_arguments)]
pub fn search_with_threads<H: Haystack + Sync + ?Sized>(
    source: &H,
    pattern: &Pattern,
    range: Range<u64>,
    mode: SearchMode,
    requested: usize,
    cancel: &Cancellation,
    mut on_hit: impl FnMut(Match),
    mut progress: impl FnMut(Progress),
) -> io::Result<SearchSummary> {
    if requested == 0 {
        return Err(invalid("search requires at least one worker"));
    }
    let shards = partitions(source, pattern, range.clone(), requested.min(worker_count()), cancel)?;
    let workers = shards.len();
    if workers == 1 {
        return search(source, pattern, range, mode, cancel, on_hit, progress);
    }
    let total = range.end - range.start;
    if mode == SearchMode::Count {
        return std::thread::scope(|scope| {
            let (tx, rx) = sync_channel(workers * 2);
            for (id, shard) in shards.into_iter().enumerate() {
                let (tx, pattern) = (tx.clone(), pattern.clone());
                let final_end = range.end;
                scope.spawn(move || {
                    let duplicates = Cell::new(0);
                    let result = search(
                        source,
                        &pattern,
                        shard.clone(),
                        SearchMode::Hits,
                        cancel,
                        |m| {
                            if m.len == 0 && m.offset == shard.end && shard.end < final_end {
                                duplicates.set(duplicates.get() + 1);
                            }
                        },
                        |p| {
                            let _ =
                                tx.send(CountEvent::Progress(id, Progress { count: p.count - duplicates.get(), ..p }));
                        },
                    )
                    .map(|mut s| {
                        s.count -= duplicates.get();
                        s
                    });
                    let _ = tx.send(CountEvent::Done(id, result));
                });
            }
            drop(tx);
            let mut latest = vec![(0, 0); workers];
            let mut result = SearchSummary { workers, ..SearchSummary::default() };
            let mut error = None;
            for event in rx {
                match event {
                    CountEvent::Progress(i, p) => latest[i] = (p.count, p.scanned),
                    CountEvent::Done(i, Ok(s)) => {
                        latest[i] = (s.count, s.scanned);
                        result.cancelled |= s.cancelled;
                    }
                    CountEvent::Done(_, Err(e)) => {
                        if error.is_none() {
                            error = Some(e);
                        }
                    }
                }
                result.count = latest.iter().map(|p| p.0).sum();
                result.scanned = latest.iter().map(|p| p.1).sum();
                progress(Progress { count: result.count, scanned: result.scanned, total });
            }
            result.cancelled |= cancel.load(Ordering::Relaxed);
            error.map_or(Ok(result), Err)
        });
    }
    std::thread::scope(|scope| {
        let mut receivers = Vec::with_capacity(workers);
        for shard in shards {
            let (tx, rx) = sync_channel(2);
            receivers.push((shard.clone(), rx));
            let pattern = pattern.clone();
            let final_end = range.end;
            scope.spawn(move || {
                let mut batch = Vec::with_capacity(256);
                let duplicates = Cell::new(0);
                let result = search(
                    source,
                    &pattern,
                    shard.clone(),
                    SearchMode::Hits,
                    cancel,
                    |m| {
                        if m.len == 0 && m.offset == shard.end && shard.end < final_end {
                            duplicates.set(duplicates.get() + 1);
                            return;
                        }
                        batch.push(m);
                        if batch.len() == 256 {
                            let full = std::mem::replace(&mut batch, Vec::with_capacity(256));
                            if tx.send(HitEvent::Batch(full)).is_err() {
                                cancel.store(true, Ordering::Relaxed);
                            }
                        }
                    },
                    |_| {},
                )
                .map(|mut s| {
                    s.count -= duplicates.get();
                    s
                });
                if !batch.is_empty() {
                    let _ = tx.send(HitEvent::Batch(batch));
                }
                let _ = tx.send(HitEvent::Done(result));
            });
        }
        let mut result = SearchSummary { workers, ..SearchSummary::default() };
        let mut emitted = 0;
        let mut error = None;
        for (shard, rx) in receivers {
            for event in rx {
                match event {
                    HitEvent::Batch(batch) => {
                        let mut through = 0;
                        for m in batch {
                            if !cancel.load(Ordering::Relaxed) {
                                on_hit(m);
                                emitted += 1;
                            }
                            through = m.offset - shard.start;
                        }
                        progress(Progress { count: emitted, scanned: result.scanned + through, total });
                    }
                    HitEvent::Done(Ok(s)) => {
                        result.count += s.count;
                        result.scanned += s.scanned;
                        result.cancelled |= s.cancelled;
                    }
                    HitEvent::Done(Err(e)) => {
                        if error.is_none() {
                            error = Some(e);
                        }
                    }
                }
            }
        }
        result.cancelled |= cancel.load(Ordering::Relaxed);
        progress(Progress { count: result.count, scanned: result.scanned, total });
        error.map_or(Ok(result), Err)
    })
}

enum CountEvent {
    Progress(usize, Progress),
    Done(usize, io::Result<SearchSummary>),
}
enum HitEvent {
    Batch(Vec<Match>),
    Done(io::Result<SearchSummary>),
}

fn partitions<H: Haystack + ?Sized>(
    source: &H,
    pattern: &Pattern,
    range: Range<u64>,
    workers: usize,
    cancel: &Cancellation,
) -> io::Result<Vec<Range<u64>>> {
    if range.start > range.end || range.end > source.len() {
        return Err(invalid("invalid search range"));
    }
    if workers <= 1 || !pattern.newline_safe || range.end - range.start < (BLOCK * workers) as u64 {
        return Ok(vec![range]);
    }
    let mut cuts = vec![range.start];
    for i in 1..workers {
        if cancel.load(Ordering::Relaxed) {
            return Ok(vec![range]);
        }
        let nominal = range.start + (range.end - range.start) / workers as u64 * i as u64;
        let end = nominal.saturating_add(256 << 10).min(range.end);
        let mut expected = nominal;
        let mut boundary = None;
        for chunk in source.chunks(nominal..end)? {
            let (at, bytes) = chunk?;
            if at != expected || bytes.len() as u64 > end - expected {
                return Err(invalid("invalid haystack chunks"));
            }
            expected += bytes.len() as u64;
            if let Some(n) = memchr::memchr(b'\n', bytes) {
                boundary = Some(at + n as u64 + 1);
                break;
            }
        }
        let Some(cut) = boundary.filter(|cut| *cut > *cuts.last().unwrap() && *cut < range.end) else {
            return Ok(vec![range]);
        };
        cuts.push(cut);
    }
    cuts.push(range.end);
    Ok(cuts.windows(2).map(|c| c[0]..c[1]).collect())
}
