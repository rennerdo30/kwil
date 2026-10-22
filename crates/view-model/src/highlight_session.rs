//! One frontend-owned highlighting worker, with bounded disjoint viewport
//! windows. A giant offscreen line gap never expands a source request.
use crate::{Workspace, highlighting::*};
use std::{
    collections::{BTreeMap, VecDeque},
    ops::Range,
    sync::Arc,
};

const WINDOW: usize = MAX_WINDOW_BYTES - 8192;

#[derive(PartialEq, Eq)]
struct Identity {
    document: DocumentId,
    revision: u64,
    path: std::path::PathBuf,
    windows: Vec<Range<usize>>,
}

pub struct HighlightSession {
    worker: Option<HighlightWorker>,
    identity: Option<Identity>,
    generation: u64,
    key: Option<HighlightKey>,
    pending: VecDeque<Range<usize>>,
    rejected: Option<HighlightRequest>,
    in_flight: bool,
    source: Option<Arc<dyn kwil_core_syntax::TextSource>>,
    path: Arc<std::path::PathBuf>,
    mappings: Arc<[LanguageMapping]>,
    results: Vec<Arc<PublishedHighlights>>,
    painted: BTreeMap<u64, Vec<(Range<usize>, Class)>>,
}

impl HighlightSession {
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Result<Self, kwil_core_syntax::Error> {
        Ok(Self {
            worker: Some(HighlightWorker::new(Default::default(), wake)?),
            identity: None,
            generation: 0,
            key: None,
            pending: VecDeque::new(),
            rejected: None,
            in_flight: false,
            source: None,
            path: Arc::new(Default::default()),
            mappings: Arc::from([]),
            results: Vec::new(),
            painted: BTreeMap::new(),
        })
    }

    /// O(visible rows), cached metadata only. Requests never contain a whole
    /// line merely because its suffix is visible far from its start.
    pub fn update(&mut self, view: &Workspace) -> bool {
        let Some(editor) = view.editor() else { return false };
        let windows = windows(
            (0..view.rows())
                .filter_map(|row| view.line_source(row))
                .filter_map(|range| Some(usize::try_from(range.start).ok()?..usize::try_from(range.end).ok()?)),
        );
        let next = Identity {
            document: view.active_id(),
            revision: editor.document().revision(),
            path: editor.document().path().unwrap_or(std::path::Path::new("")).to_path_buf(),
            windows,
        };
        if self.identity.as_ref() == Some(&next) {
            return false;
        }
        // A rejected request shares this source; release that extra handle
        // before handing the last frontend reference to the worker.
        self.rejected = None;
        if let Some(source) = self.source.take()
            && let Some(worker) = &self.worker
            && let Err(source) = worker.retire_source(source)
        {
            self.source = Some(source);
            return false;
        }
        self.generation = self.generation.checked_add(1).expect("highlight viewport identity exhausted");
        self.key = Some(HighlightKey {
            document_id: next.document,
            revision: next.revision,
            viewport_generation: self.generation,
        });
        self.pending = next.windows.iter().cloned().collect();
        if self.identity.as_ref().is_none_or(|old| old.document != next.document) {
            self.painted.clear();
        }
        self.painted.retain(|row, _| *row >= view.top() && *row < view.top().saturating_add(view.rows() as u64));
        self.results.clear();
        // Retire superseded handles off the input thread, including rejected
        // requests the syntax worker never accepted.
        self.source = Some(Arc::new(SnapshotTextSource::new(editor.document().snapshot())));
        self.path = Arc::new(next.path.clone());
        self.identity = Some(next);
        self.in_flight = false;
        if let Some(worker) = &self.worker {
            worker.cancel();
        }
        self.pump();
        true
    }

    fn pump(&mut self) {
        if self.in_flight {
            return;
        }
        let Some(worker) = &self.worker else { return };
        let request = self.rejected.take().or_else(|| {
            Some(HighlightRequest {
                key: self.key?,
                source: self.source.as_ref()?.clone(),
                path: self.path.clone(),
                choice: LanguageChoice::Automatic,
                mappings: self.mappings.clone(),
                visible: self.pending.pop_front()?,
                previous_revision: None,
                edit: None,
            })
        });
        let Some(request) = request else { return };
        match worker.submit(request) {
            Ok(_) => self.in_flight = true,
            Err(error) => self.rejected = Some(error.request),
        }
    }

    /// Call only on the worker wake. Each accepted result styles one bounded
    /// window, then the same worker advances to the next visible window.
    pub fn worker_ready(&mut self, view: &Workspace) -> Vec<usize> {
        self.update(view);
        let Some(key) = self.key else { return Vec::new() };
        let update = self.worker.as_ref().and_then(|worker| worker.take_update(key));
        let mut changed = Vec::new();
        if let Some(update) = update {
            self.in_flight = false;
            if let Ok(result) = update
                && view.editor().is_some_and(|editor| {
                    key.document_id == view.active_id() && key.revision == editor.document().revision()
                })
            {
                for row in 0..view.rows() {
                    let Some(source) = view.line_source(row) else { continue };
                    let source = source.start as usize..source.end as usize;
                    if source.start >= result.highlights.requested.end
                        || source.end <= result.highlights.requested.start
                    {
                        continue;
                    }
                    let styles: Vec<_> = result
                        .highlights
                        .spans
                        .iter()
                        .filter_map(|span| {
                            let start = span.range.start.max(source.start);
                            let end = span.range.end.min(source.end);
                            (start < end).then_some((
                                start.saturating_sub(source.start)..end.saturating_sub(source.start),
                                span.class,
                            ))
                        })
                        .collect();
                    let previous = self.painted.entry(view.top() + row as u64).or_default();
                    if *previous != styles {
                        *previous = styles;
                        changed.push(row);
                    }
                }
                self.results.push(result);
            }
        }
        self.pump();
        changed
    }

    pub fn spans<'a>(&'a self, view: &'a Workspace) -> impl Iterator<Item = &'a Span> {
        self.results
            .iter()
            .filter(|result| {
                view.editor().is_some_and(|editor| {
                    result.key.document_id == view.active_id() && result.key.revision == editor.document().revision()
                })
            })
            .flat_map(|result| result.highlights.spans.iter())
    }

    pub fn invalidate_painted(&mut self, view: &Workspace, damage: &crate::Damage) {
        if damage.full {
            self.painted.clear();
        }
        for row in &damage.rows {
            self.painted.remove(&(view.top() + *row as u64));
        }
    }

    pub fn stop(&mut self) {
        if let Some(worker) = self.worker.take() {
            let source = self.source.take();
            let rejected = self.rejected.take();
            std::thread::spawn(move || {
                let _ = worker.shutdown();
                drop((source, rejected));
            });
        }
    }
}
impl Drop for HighlightSession {
    fn drop(&mut self) {
        self.stop();
    }
}

fn windows(ranges: impl Iterator<Item = Range<usize>>) -> Vec<Range<usize>> {
    let mut result: Vec<Range<usize>> = Vec::new();
    for range in ranges {
        let mut start = range.start;
        while start < range.end {
            let end = range.end.min(start.saturating_add(WINDOW));
            if let Some(last) = result.last_mut()
                && end.saturating_sub(last.start) <= WINDOW
            {
                last.end = last.end.max(end);
            } else {
                result.push(start..end);
            }
            start = end;
        }
    }
    result
}

/// Built-in syntax palette until the frontend supplies its active theme.
pub fn syntax_rgb(class: Class) -> u32 {
    use kwil_core_commands::theme::{Scheme, Theme};
    static THEME: std::sync::OnceLock<Theme> = std::sync::OnceLock::new();
    let color = THEME.get_or_init(|| Theme::builtin(Scheme::Dark)).style(class.name()).foreground;
    color.map_or(0xd6e4f0, |color| u32::from(color.0) << 16 | u32::from(color.1) << 8 | u32::from(color.2))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CommandArgs, View};
    use std::{
        sync::mpsc,
        time::{Duration, Instant},
    };

    #[test]
    fn save_as_path_changes_invalidate_detection_without_an_edit() {
        let mut view = Workspace::new(View::new(None)).unwrap();
        view.set_rows(1);
        view.execute("edit.insert", &CommandArgs { text: Some("fn main() {}".into()), ..Default::default() }, 1)
            .unwrap();
        let mut host = HighlightSession::new(|| {}).unwrap();
        assert!(host.update(&view));
        let revision = view.editor().unwrap().document().revision();
        let first_key = host.key;
        view.editor_mut().unwrap().document.path = Some("renamed.rs".into());
        assert!(host.update(&view));
        assert_ne!(host.key, first_key);
        assert_eq!(view.editor().unwrap().document().revision(), revision);
        assert_eq!(host.path.as_os_str(), std::ffi::OsStr::new("renamed.rs"));
        host.worker.take().unwrap().shutdown().unwrap();
    }

    #[test]
    fn giant_line_gaps_and_long_windows_remain_bounded() {
        let ranges =
            windows([0..80, 1_000_000_000..1_000_000_100, 2_000_000_000..2_000_000_000 + WINDOW * 2 + 1].into_iter());
        assert_eq!(ranges.len(), 5);
        assert!(ranges.iter().all(|range| range.end - range.start <= WINDOW));
        assert_eq!(ranges[1], 1_000_000_000..1_000_000_100);
        assert_eq!(ranges.last().unwrap().end, 2_000_000_000 + WINDOW * 2 + 1);
    }

    #[test]
    fn cached_windows_keep_absolute_offsets_after_horizontal_scroll() {
        let mut view = View::new(None);
        view.set_rows(2);
        view.execute("edit.insert", &CommandArgs { text: Some("abc界XYZ\nsecond".into()), ..Default::default() }, 1)
            .unwrap();
        view.set_horizontal_window(4, 16);
        for row in 0..2 {
            let source = view.line_source(row).unwrap();
            let bytes: Vec<_> = view
                .editor()
                .unwrap()
                .document()
                .snapshot()
                .chunks(source)
                .unwrap()
                .flat_map(|chunk| chunk.unwrap().to_vec())
                .collect();
            assert_eq!(bytes, view.line(row).unwrap());
            assert!(std::str::from_utf8(&bytes).is_ok());
        }
    }

    #[test]
    fn worker_results_style_visible_rows_and_only_repaint_changed_styles() {
        let mut view = Workspace::new(View::new(None)).unwrap();
        view.set_rows(3);
        view.execute("edit.insert", &CommandArgs { text: Some("1\n2\n3".into()), ..Default::default() }, 1).unwrap();
        let (sender, receiver) = mpsc::channel();
        let mut host = HighlightSession::new(move || {
            let _ = sender.send(());
        })
        .unwrap();
        let drain = |host: &mut HighlightSession, view: &Workspace| {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut rows = Vec::new();
            while host.in_flight || !host.pending.is_empty() || host.rejected.is_some() {
                receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
                rows.extend(host.worker_ready(view));
            }
            rows
        };
        assert!(host.update(&view));
        assert_eq!(drain(&mut host, &view), vec![0, 1, 2]);
        assert!(host.spans(&view).any(|span| span.class == Class::Number));
        let result =
            view.execute("edit.insert", &CommandArgs { text: Some("4".into()), ..Default::default() }, 1).unwrap();
        assert_eq!(host.spans(&view).count(), 0, "old revisions cannot style a changed document");
        host.invalidate_painted(&view, &result.damage);
        host.update(&view);
        assert_eq!(drain(&mut host, &view), vec![2]);
        assert!(!host.update(&view));
        host.worker.take().unwrap().shutdown().unwrap();
    }
}
