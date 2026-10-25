//! One frontend-owned highlighting worker, with bounded disjoint viewport
//! windows. A giant offscreen line gap never expands a source request.
use crate::{
    Workspace,
    highlighting::*,
    language_selection::{AcceptedLanguageMappings, LanguagePicker, LanguageStatus},
    visual_rows::{
        AtomKind, BodyOwner, LayoutKey, LayoutPage, MAX_SYNTAX_BYTES, MAX_SYNTAX_WINDOWS, TopAnchor, WrapError,
    },
    wrap_host::WrapHostContext,
};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    ops::Range,
    sync::Arc,
};

const WINDOW: usize = MAX_WINDOW_BYTES - 8192;

#[derive(Clone, Copy, PartialEq, Eq)]
struct PreparedIdentity {
    key: LayoutKey,
    path_generation: u64,
    anchor: TopAnchor,
}

#[derive(PartialEq, Eq)]
struct Identity {
    document: DocumentId,
    source_epoch: u64,
    input_epoch: u64,
    revision: u64,
    path: std::path::PathBuf,
    settings_generation: u64,
    choice: LanguageChoice,
    windows: Vec<Range<usize>>,
    prepared: Option<PreparedIdentity>,
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
    snapshot: Option<Arc<SnapshotTextSource>>,
    path: Arc<std::path::PathBuf>,
    mappings: Arc<[LanguageMapping]>,
    settings_generation: u64,
    choices: HashMap<DocumentId, LanguageChoice>,
    results: Vec<Arc<PublishedHighlights>>,
    last_detection: Option<Detection>,
    painted: BTreeMap<u64, Vec<(Range<usize>, Class)>>,
    prepared_suspended: bool,
    prepared_error: Option<WrapError>,
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
            snapshot: None,
            path: Arc::new(Default::default()),
            mappings: Arc::from([]),
            settings_generation: 0,
            choices: HashMap::new(),
            results: Vec::new(),
            last_detection: None,
            painted: BTreeMap::new(),
            prepared_suspended: false,
            prepared_error: None,
        })
    }

    /// Accept only a SETTINGS-worker-prepared table. On rejection ROOT retains
    /// its last accepted settings and this session's last accepted mappings.
    /// A true return requires `update(view)` and status damage even when the
    /// document revision is unchanged. Equal typed tables are a no-op.
    pub fn apply_mappings(&mut self, mappings: AcceptedLanguageMappings) -> bool {
        if self.mappings.as_ref() == mappings.mappings() {
            return false;
        }
        self.mappings = mappings.shared();
        self.settings_generation = self.settings_generation.checked_add(1).expect("syntax settings identity exhausted");
        true
    }

    pub fn language_choice(&self, document: DocumentId) -> LanguageChoice {
        self.choices.get(&document).copied().unwrap_or_default()
    }

    /// Document-addressed programmatic hook. Interactive actions should use
    /// `apply_picker`, which additionally validates the captured input epoch.
    /// Overrides follow DocumentId through Save As and bounded closed-tab
    /// history. Automatic removes the override, exposing current detection.
    pub fn set_language_choice(&mut self, view: &Workspace, document: DocumentId, choice: LanguageChoice) -> bool {
        self.retain_choices(view);
        if view.view(document).is_none() || self.language_choice(document) == choice {
            return false;
        }
        if choice == LanguageChoice::Automatic {
            self.choices.remove(&document);
        } else {
            self.choices.insert(document, choice);
        }
        true
    }

    fn retain_choices(&mut self, view: &Workspace) {
        self.choices.retain(|document, _| {
            view.view(*document).is_some() || view.session().closed().iter().any(|tab| tab.id == *document)
        });
    }

    pub fn language_status(&self, view: &Workspace) -> LanguageStatus {
        LanguageStatus { choice: self.language_choice(view.active_id()), detection: self.detection(view) }
    }

    pub fn language_picker(&self, view: &Workspace) -> LanguagePicker {
        LanguagePicker::capture(view, self.language_status(view))
    }

    /// ROOT calls `update` and schedules status damage after a true return.
    pub fn apply_picker(&mut self, view: &Workspace, picker: &LanguagePicker, choice: LanguageChoice) -> bool {
        picker.is_current(view) && self.set_language_choice(view, picker.document(), choice)
    }

    /// A policy/path/tab change hides superseded data immediately, including
    /// while bounded source retirement delays admitting its next request.
    fn current_key(&self, view: &Workspace) -> Option<HighlightKey> {
        if view.hex_active() || self.prepared_suspended {
            return None;
        }
        let identity = self.identity.as_ref()?;
        let editor = view.editor()?;
        (identity.document == view.active_id()
            && identity.source_epoch == editor.document().source_epoch
            && identity.input_epoch == view.input_epoch()
            && identity.revision == editor.document().revision()
            && identity.path == editor.document().path().unwrap_or(std::path::Path::new(""))
            && identity.settings_generation == self.settings_generation
            && identity.choice == self.language_choice(view.active_id()))
        .then_some(self.key)
        .flatten()
    }

    /// O(visible rows), cached metadata only. Requests never contain a whole
    /// line merely because its suffix is visible far from its start.
    pub fn update(&mut self, view: &Workspace) -> bool {
        if view.hex_active() {
            // Raw bodies (including preview/loans) never acquire syntax pins,
            // even if their cached text windows are empty. Keep existing text
            // handles bounded here: the next text update or shutdown retires
            // them on the worker, never by dropping the last source on input.
            return false;
        }
        self.retain_choices(view);
        if self.worker.is_none() {
            return false;
        }
        let Some(editor) = view.editor() else { return false };
        let windows = windows(
            (0..view.rows())
                .filter_map(|row| view.line_source(row))
                .filter_map(|range| Some(usize::try_from(range.start).ok()?..usize::try_from(range.end).ok()?)),
        );
        let next = Identity {
            document: view.active_id(),
            source_epoch: editor.document().source_epoch,
            input_epoch: view.input_epoch(),
            revision: editor.document().revision(),
            path: editor.document().path().unwrap_or(std::path::Path::new("")).to_path_buf(),
            settings_generation: self.settings_generation,
            choice: self.language_choice(view.active_id()),
            windows,
            prepared: None,
        };
        self.update_identity(view, next)
    }

    fn update_identity(&mut self, view: &Workspace, next: Identity) -> bool {
        let Some(editor) = view.editor() else { return false };
        let resumed = std::mem::replace(&mut self.prepared_suspended, false);
        if !resumed && self.identity.as_ref() == Some(&next) {
            return false;
        }
        if self.identity.as_ref().is_none_or(|old| {
            old.document != next.document
                || old.source_epoch != next.source_epoch
                || old.revision != next.revision
                || old.path != next.path
                || old.settings_generation != next.settings_generation
                || old.choice != next.choice
                || matches!((old.prepared, next.prepared), (Some(old), Some(next)) if old.key.source != next.key.source)
        }) {
            self.last_detection = None;
        }
        let source_changed = self.identity.as_ref().is_none_or(|old| {
            old.document != next.document
                || old.source_epoch != next.source_epoch
                || old.revision != next.revision
                || match (old.prepared, next.prepared) {
                    (Some(old), Some(next)) => old.key.source.incarnation != next.key.source.incarnation,
                    _ => false,
                }
        });
        if source_changed {
            // Release duplicate typed handles while `source` still owns this same
            // object, BEFORE retirement can race with the worker's destruction.
            self.snapshot = None;
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
        }
        self.generation = self.generation.checked_add(1).expect("highlight viewport identity exhausted");
        self.key = Some(HighlightKey {
            document_id: next.document,
            revision: next.revision,
            viewport_generation: self.generation,
        });
        self.pending = next.windows.iter().cloned().collect();
        if self.pending.is_empty() {
            // Empty files/blank visible rows still need worker-owned detection
            // for their status and mappings, even though there are no spans.
            self.pending.push_back(0..0);
        }
        if self.identity.as_ref().is_none_or(|old| old.document != next.document || old.prepared != next.prepared) {
            self.painted.clear();
        }
        if next.prepared.is_none() {
            self.painted.retain(|row, _| *row >= view.top() && *row < view.top().saturating_add(view.rows() as u64));
        }
        self.results.clear();
        // Retire superseded handles off the input thread, including rejected
        // requests the syntax worker never accepted.
        self.rejected = None;
        if source_changed {
            let source = Arc::new(SnapshotTextSource::from_editor(editor));
            self.source = Some(source.clone());
            self.snapshot = Some(source);
        }
        self.path = Arc::new(next.path.clone());
        self.identity = Some(next);
        self.in_flight = false;
        if let Some(worker) = &self.worker {
            worker.cancel();
        }
        self.pump();
        true
    }

    /// Use the freshly prepared current context, including its complete layout
    /// and source identity. Neither the context's incarnation nor input epoch
    /// can be reconstructed from raw Workspace counters. No source I/O here.
    pub fn update_prepared(&mut self, view: &Workspace, page: &LayoutPage, current: WrapHostContext) -> bool {
        if !prepared_current(view, page, current) || self.worker.is_none() {
            self.suspend_prepared();
            self.prepared_error = Some(WrapError::Stale);
            return false;
        }
        let windows = match prepared_windows(page) {
            Ok(windows) => windows,
            Err(error) => {
                self.suspend_prepared();
                self.prepared_error = Some(error);
                return false;
            }
        };
        self.retain_choices(view);
        self.prepared_error = None;
        let editor = view.editor().expect("validated prepared editor");
        let next = Identity {
            document: view.active_id(),
            source_epoch: editor.document().source_epoch,
            input_epoch: view.input_epoch(),
            revision: editor.document().revision(),
            path: editor.document().path().unwrap_or(std::path::Path::new("")).to_path_buf(),
            settings_generation: self.settings_generation,
            choice: self.language_choice(view.active_id()),
            windows,
            prepared: Some(PreparedIdentity {
                key: page.key,
                path_generation: current.path_generation,
                anchor: page.anchor,
            }),
        };
        self.update_identity(view, next)
    }

    /// Call while the wrapped page is pending, or another body owns paint.
    /// Keep the existing source for bounded worker retirement; never obtain a
    /// replacement snapshot or fall back to logical viewport reads.
    pub fn suspend_prepared(&mut self) {
        if !self.prepared_suspended {
            if let Some(worker) = &self.worker {
                worker.cancel();
            }
            self.prepared_suspended = true;
        }
        self.pending.clear();
        self.rejected = None; // source/snapshot still pin this exact owner
        self.in_flight = false;
        self.results.clear();
        self.painted.clear();
    }

    /// Admission refusal is explicit; it never becomes a partial syntax page.
    pub fn prepared_error(&self) -> Option<&WrapError> {
        self.prepared_error.as_ref()
    }

    fn prepared_key(&self, view: &Workspace, page: &LayoutPage, current: WrapHostContext) -> Option<HighlightKey> {
        if !prepared_current(view, page, current) {
            return None;
        }
        let identity = self.identity.as_ref()?;
        let prepared = identity.prepared?;
        (prepared.key == current.key
            && prepared.path_generation == current.path_generation
            && prepared.anchor == page.anchor
            && identity
                .windows
                .iter()
                .map(|range| range.start as u64..range.end as u64)
                .eq(page.syntax_windows().iter().cloned()))
        .then(|| self.current_key(view))
        .flatten()
    }

    /// Wake-driven only. Damage addresses visual rows and their prepared cell
    /// mappings; opaque coverage and gaps never authorize a syntax read.
    pub fn worker_ready_prepared(
        &mut self,
        view: &Workspace,
        page: &LayoutPage,
        current: WrapHostContext,
    ) -> Vec<usize> {
        // A failed admission/parse is retried by an explicit host update, not
        // by an unrelated or late worker wake repeatedly resubmitting it.
        if self.prepared_suspended && self.prepared_error.is_some() {
            return Vec::new();
        }
        self.update_prepared(view, page, current);
        let Some(key) = self.prepared_key(view, page, current) else { return Vec::new() };
        let update = self.worker.as_ref().and_then(|worker| worker.take_update(key));
        let mut changed = Vec::new();
        if let Some(update) = update {
            self.in_flight = false;
            let result = match update {
                Ok(result) => result,
                Err(error) => {
                    let changed = self
                        .painted
                        .iter()
                        .filter_map(|(row, styles)| (!styles.is_empty()).then_some(*row as usize))
                        .collect();
                    self.suspend_prepared();
                    self.prepared_error = Some(WrapError::Io(std::io::Error::other(error)));
                    return changed;
                }
            };
            if result.key == key {
                self.last_detection = Some(result.detection);
                self.results.retain(|result| result.key == key);
                self.results.push(result);
                for (row, metadata) in page.rows().iter().enumerate() {
                    let mut styles = Vec::new();
                    for result in &self.results {
                        let requested = &result.highlights.requested;
                        if requested.end as u64 <= metadata.source_coverage.start
                            || requested.start as u64 >= metadata.source_coverage.end
                        {
                            continue;
                        }
                        for atom in page.atoms(row).iter().filter(|atom| atom.kind != AtomKind::Opaque) {
                            let spans = &result.highlights.spans;
                            let first = spans.partition_point(|span| span.range.end as u64 <= atom.source.start);
                            for span in
                                spans[first..].iter().take_while(|span| (span.range.start as u64) < atom.source.end)
                            {
                                styles.push((usize::from(atom.cells.start)..usize::from(atom.cells.end), span.class));
                            }
                        }
                    }
                    let previous = self.painted.entry(row as u64).or_default();
                    if *previous != styles {
                        *previous = styles;
                        changed.push(row);
                    }
                }
            }
        }
        self.pump();
        changed
    }

    /// Absolute source spans, guarded by the full current prepared identity.
    /// The painter maps these through page atoms, never cached logical lines.
    pub fn spans_prepared<'a>(
        &'a self,
        view: &Workspace,
        page: &LayoutPage,
        current: WrapHostContext,
    ) -> impl Iterator<Item = &'a Span> {
        let key = self.prepared_key(view, page, current);
        self.results
            .iter()
            .filter(move |result| Some(result.key) == key)
            .flat_map(|result| result.highlights.spans.iter())
    }

    fn pump(&mut self) {
        if self.in_flight || self.prepared_suspended {
            return;
        }
        let Some(worker) = &self.worker else { return };
        let Some(identity) = &self.identity else { return };
        // Do not mix a previous key with newly accepted policy if the caller
        // has not updated yet, or if source retirement asks it to retry.
        if identity.settings_generation != self.settings_generation
            || identity.choice != self.language_choice(identity.document)
        {
            return;
        }
        let request = self.rejected.take().or_else(|| {
            Some(HighlightRequest {
                key: self.key?,
                source: self.source.as_ref()?.clone(),
                path: self.path.clone(),
                choice: identity.choice,
                mappings: self.mappings.clone(),
                visible: self.pending.pop_front()?,
                previous_revision: None,
                edit: None,
                snapshot: self.snapshot.clone(),
            })
        });
        let Some(request) = request else { return };
        let submitted = if identity.prepared.is_some() {
            worker.submit_with_context_margin(request, 0)
        } else {
            worker.submit(request)
        };
        match submitted {
            Ok(_) => self.in_flight = true,
            Err(error) => self.rejected = Some(error.request),
        }
    }

    /// Call only on the worker wake. Each accepted result styles one bounded
    /// window, then the same worker advances to the next visible window.
    pub fn worker_ready(&mut self, view: &Workspace) -> Vec<usize> {
        if self.prepared_suspended || self.identity.as_ref().is_some_and(|identity| identity.prepared.is_some()) {
            return Vec::new();
        }
        if view.hex_active() {
            // A late text wake must not advance queued windows while raw owns
            // the body. Settings may still be accepted for the next text view.
            return Vec::new();
        }
        self.update(view);
        let Some(key) = self.current_key(view) else { return Vec::new() };
        let update = self.worker.as_ref().and_then(|worker| worker.take_update(key));
        let mut changed = Vec::new();
        if let Some(update) = update {
            self.in_flight = false;
            if let Ok(result) = update
                && result.key == key
            {
                self.last_detection = Some(result.detection);
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

    pub fn detection(&self, view: &Workspace) -> Option<Detection> {
        if view.hex_active() {
            return None;
        }
        let identity = self.identity.as_ref()?;
        let editor = view.editor()?;
        // Language belongs to a source/policy, not to a viewport. A resize or
        // scroll may invalidate spans while keeping accepted detection valid.
        (identity.document == view.active_id()
            && identity.source_epoch == editor.document().source_epoch
            && identity.revision == editor.document().revision()
            && identity.path == editor.document().path().unwrap_or(std::path::Path::new(""))
            && identity.settings_generation == self.settings_generation
            && identity.choice == self.language_choice(view.active_id()))
        .then_some(self.last_detection)
        .flatten()
    }

    pub fn detected_language(&self, view: &Workspace) -> Option<Language> {
        self.detection(view).map(|detection| detection.language)
    }

    pub fn spans<'a>(&'a self, view: &'a Workspace) -> impl Iterator<Item = &'a Span> {
        let key = self
            .current_key(view)
            .filter(|_| self.identity.as_ref().is_some_and(|identity| identity.prepared.is_none()));
        self.results
            .iter()
            .filter(move |result| Some(result.key) == key)
            .flat_map(|result| result.highlights.spans.iter())
    }

    pub fn invalidate_painted(&mut self, view: &Workspace, damage: &crate::Damage) {
        if damage.full {
            self.painted.clear();
        }
        for row in &damage.rows {
            let row = if self.identity.as_ref().is_some_and(|identity| identity.prepared.is_some()) {
                *row as u64
            } else {
                view.top() + *row as u64
            };
            self.painted.remove(&row);
        }
    }

    /// Consume on a shutdown owner after the GUI loop, never on input/render.
    pub fn shutdown(mut self) -> Result<(), String> {
        let result =
            self.worker.take().map_or(Ok(()), |worker| worker.shutdown().map_err(|_| "syntax worker panicked".into()));
        self.source.take();
        self.rejected.take();
        self.snapshot.take();
        result
    }
    pub fn stop(&mut self) {
        if let Some(worker) = self.worker.take() {
            let source = self.source.take();
            let rejected = self.rejected.take();
            let snapshot = self.snapshot.take();
            std::thread::spawn(move || {
                let _ = worker.shutdown();
                drop((source, rejected, snapshot));
            });
        }
    }
}
impl Drop for HighlightSession {
    fn drop(&mut self) {
        self.stop();
    }
}

fn prepared_current(view: &Workspace, page: &LayoutPage, current: WrapHostContext) -> bool {
    let Some(editor) = view.editor() else { return false };
    current.enabled
        && current.key == page.key
        && current.key.body == BodyOwner::Text
        && current.key.validate().is_ok()
        && !current.key.geometry.is_empty()
        && !view.hex_owns_document(view.active_id())
        && !view.document_busy(view.active_id())
        && editor.document().representation == crate::hex_session::Representation::DecodedText
        && editor.settings.editor.word_wrap
        && current.key.source.document_id == view.active_id().0
        && current.key.source.source_epoch == editor.document().source_epoch
        && current.key.source.revision == editor.document().revision()
}

fn prepared_windows(page: &LayoutPage) -> Result<Vec<Range<usize>>, WrapError> {
    if page.syntax_windows().len() > MAX_SYNTAX_WINDOWS {
        return Err(WrapError::Admission("prepared syntax window count exceeded"));
    }
    let mut bytes = 0usize;
    let mut windows = Vec::with_capacity(page.syntax_windows().len());
    for range in page.syntax_windows() {
        page.check_syntax_request(range.clone(), 0)?;
        let start = usize::try_from(range.start).map_err(|_| WrapError::Admission("syntax offset overflow"))?;
        let end = usize::try_from(range.end).map_err(|_| WrapError::Admission("syntax offset overflow"))?;
        bytes = bytes.checked_add(end - start).ok_or(WrapError::Admission("syntax size overflow"))?;
        if bytes > MAX_SYNTAX_BYTES || windows.last().is_some_and(|previous: &Range<usize>| previous.end > start) {
            return Err(WrapError::Admission("prepared syntax windows exceed admission"));
        }
        windows.push(start..end);
    }
    // Coverage is never a request extent. Also never silently highlight only a
    // prefix when the page could not admit every non-opaque atom for syntax.
    let mut window = 0;
    for row in 0..page.rows().len() {
        for atom in page.atoms(row).iter().filter(|atom| atom.kind != AtomKind::Opaque) {
            while window < windows.len() && windows[window].end as u64 <= atom.source.start {
                window += 1;
            }
            if windows
                .get(window)
                .is_none_or(|range| range.start as u64 > atom.source.start || (range.end as u64) < atom.source.end)
            {
                return Err(WrapError::Admission("prepared syntax omits normal atom coverage"));
            }
        }
    }
    Ok(windows)
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

    fn policy_fixture() -> (Workspace, HighlightSession, mpsc::Receiver<()>) {
        let mut view = Workspace::new(View::new(None)).unwrap();
        view.set_rows(2);
        view.execute(
            "edit.insert",
            &CommandArgs { text: Some("value = 123 // comment".into()), ..Default::default() },
            1,
        )
        .unwrap();
        view.editor_mut().unwrap().document.path = Some("code.rs".into());
        let (sender, receiver) = mpsc::channel();
        let host = HighlightSession::new(move || {
            let _ = sender.send(());
        })
        .unwrap();
        (view, host, receiver)
    }

    fn drain_policy(host: &mut HighlightSession, view: &Workspace, receiver: &mpsc::Receiver<()>) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while host.in_flight || !host.pending.is_empty() || host.rejected.is_some() {
            receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
            host.worker_ready(view);
        }
    }

    fn prepared_fixture(
        text: &str,
        columns: u16,
        rows: u16,
    ) -> (Workspace, HighlightSession, mpsc::Receiver<()>, WrapHostContext, LayoutPage) {
        use crate::visual_rows::{LayoutGeometry, SourceIdentity};
        let mut view = Workspace::new(View::new(None)).unwrap();
        view.execute("edit.insert", &CommandArgs { text: Some(text.into()), ..Default::default() }, 1).unwrap();
        view.editor_mut().unwrap().settings.editor.word_wrap = true;
        // No cached logical paint rows: a wrapped syntax fallback to line_source
        // would produce no requests/highlights and fail the controls below.
        view.set_rows(0);
        let editor = view.editor().unwrap();
        let current = WrapHostContext {
            enabled: true,
            key: LayoutKey {
                source: SourceIdentity {
                    document_id: view.active_id().0,
                    source_epoch: editor.document().source_epoch,
                    incarnation: 17,
                    revision: editor.document().revision(),
                },
                input_epoch: 101,
                layout_generation: 11,
                body_generation: 3,
                body: BodyOwner::Text,
                geometry: LayoutGeometry {
                    columns,
                    rows,
                    tab_size: 4,
                    font_generation: 2,
                    width_policy_generation: 3,
                    settings_generation: 4,
                },
            },
            path_generation: 7,
            cursor_generation: 1,
        };
        let page = prepared_page(&view, current, TopAnchor::default());
        let (sender, events) = mpsc::channel();
        let host = HighlightSession::new(move || {
            let _ = sender.send(());
        })
        .unwrap();
        (view, host, events, current, page)
    }

    fn prepared_page(view: &Workspace, current: WrapHostContext, anchor: TopAnchor) -> LayoutPage {
        use crate::{
            snapshot_layout_source::SnapshotLayoutSource,
            visual_rows::{AllocationBudget, LayoutEngine, MAX_SESSION_BYTES, NoCancellation, TaskRequest, fixtures},
        };
        let budget = AllocationBudget::new(MAX_SESSION_BYTES);
        let source =
            SnapshotLayoutSource::try_new(view.editor().unwrap().document().snapshot(), current.key.source, &budget)
                .unwrap_or_else(|refused| panic!("{}", refused.error));
        let engine = LayoutEngine::new(budget).unwrap();
        let (_, output) =
            fixtures::drive(source, engine, TaskRequest::Fill { key: current.key, anchor }, &NoCancellation);
        match output.unwrap().into_page() {
            std::ops::ControlFlow::Continue(page) => page,
            std::ops::ControlFlow::Break(_) => panic!("prepared fixture returned navigation"),
        }
    }

    fn drain_prepared(
        host: &mut HighlightSession,
        view: &Workspace,
        events: &mpsc::Receiver<()>,
        current: WrapHostContext,
        page: &LayoutPage,
    ) -> Vec<usize> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut changed = Vec::new();
        while host.in_flight || !host.pending.is_empty() || host.rejected.is_some() {
            events.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
            changed.extend(host.worker_ready_prepared(view, page, current));
        }
        changed
    }

    #[test]
    fn prepared_visual_rows_use_exact_windows_and_cells_without_cached_logical_lines() {
        let (view, mut host, events, current, page) = prepared_fixture("1\t22 333 4444 5", 5, 8);
        assert_eq!(view.rows(), 0);
        assert!(view.line_source(0).is_none());
        assert!(page.rows().len() > 1);
        assert!(host.update_prepared(&view, &page, current));
        let changed = drain_prepared(&mut host, &view, &events, current, &page);
        assert!(changed.len() > 1, "one logical line must damage several distinct visual rows");
        assert!(host.spans_prepared(&view, &page, current).any(|span| span.class == Class::Number));
        assert_eq!(host.spans(&view).count(), 0, "legacy paint API must not expose wrapped state");
        assert!(host.results.iter().all(|result| result.highlights.queried == result.highlights.requested));
        assert!(host.results.iter().all(|result| {
            page.check_syntax_request(
                result.highlights.requested.start as u64..result.highlights.requested.end as u64,
                0,
            )
            .is_ok()
        }));
        assert!(host.painted.keys().all(|row| (*row as usize) < page.rows().len()));
        assert!(host.worker_ready_prepared(&view, &page, current).is_empty(), "unchanged wake produces no damage");
        assert!(!host.update_prepared(&view, &page, current));
        host.shutdown().unwrap();
    }

    #[test]
    fn prepared_opaque_coverage_is_preserved_but_contributes_no_syntax_or_gap_context() {
        let text = format!("1 a{} 2", "\u{301}".repeat(300));
        let (view, mut host, events, current, page) = prepared_fixture(&text, 8, 8);
        let opaque: Vec<_> = (0..page.rows().len())
            .flat_map(|row| page.atoms(row))
            .filter(|atom| atom.kind == AtomKind::Opaque)
            .map(|atom| atom.source.clone())
            .collect();
        assert_eq!(opaque.len(), 1);
        assert!(opaque[0].end - opaque[0].start > 256);
        assert!(
            page.rows()
                .iter()
                .any(|row| row.source_coverage.start <= opaque[0].start && row.source_coverage.end >= opaque[0].end)
        );
        assert!(page.syntax_windows().len() >= 2, "normal text on both sides stays disjoint");
        host.set_language_choice(&view, view.active_id(), LanguageChoice::Explicit(Language::Generic));
        host.update_prepared(&view, &page, current);
        drain_prepared(&mut host, &view, &events, current, &page);
        assert!(host.spans_prepared(&view, &page, current).filter(|span| span.class == Class::Number).count() >= 2);
        for result in &host.results {
            assert_eq!(result.highlights.queried, result.highlights.requested);
            assert!(opaque.iter().all(|opaque| result.highlights.queried.end as u64 <= opaque.start
                || result.highlights.queried.start as u64 >= opaque.end));
        }
        host.shutdown().unwrap();
    }

    #[test]
    fn prepared_reflow_tab_font_settings_and_path_generations_hide_late_results() {
        let (view, mut host, events, current, page) = prepared_fixture("1 22 333 4444", 5, 8);
        host.update_prepared(&view, &page, current);
        drain_prepared(&mut host, &view, &events, current, &page);
        let old = host.results[0].clone();
        for index in 0..9 {
            let mut next = current;
            match index {
                0 => next.key.geometry.columns += 1,
                1 => next.key.geometry.tab_size += 1,
                2 => next.key.geometry.font_generation += 1,
                3 => next.key.geometry.width_policy_generation += 1,
                4 => next.key.geometry.settings_generation += 1,
                5 => next.key.layout_generation += 1,
                6 => next.key.source.incarnation += 1,
                7 => next.key.input_epoch += 1,
                _ => next.path_generation += 1,
            }
            assert_eq!(
                host.spans_prepared(&view, &page, next).count(),
                0,
                "old page or path cannot paint after generation {index}"
            );
            let next_page = prepared_page(&view, next, TopAnchor::default());
            assert!(host.update_prepared(&view, &next_page, next));
            host.results.push(old.clone());
            assert_eq!(
                host.spans_prepared(&view, &next_page, next).count(),
                0,
                "late result cannot cross generation {index}"
            );
            drain_prepared(&mut host, &view, &events, next, &next_page);
            assert!(host.spans_prepared(&view, &next_page, next).next().is_some());
        }
        host.shutdown().unwrap();
    }

    #[test]
    fn prepared_raw_excluded_pending_and_stale_source_wakes_never_pump_legacy_work() {
        let (mut view, mut host, events, current, page) = prepared_fixture("1 22 333", 5, 4);
        let document = view.active_id();
        host.update_prepared(&view, &page, current);
        // Result is published, but the UI has not consumed its wake yet.
        events.recv_timeout(Duration::from_secs(5)).unwrap();
        let source = host.source.as_ref().unwrap().clone();
        for body in [BodyOwner::Raw, BodyOwner::MarkdownPreview, BodyOwner::LogProjection, BodyOwner::EditorLoan] {
            let mut excluded = current;
            excluded.key.body = body;
            excluded.key.body_generation += 1;
            assert!(host.worker_ready_prepared(&view, &page, excluded).is_empty());
            assert!(host.spans_prepared(&view, &page, excluded).next().is_none());
            assert!(!host.in_flight && host.pending.is_empty() && host.rejected.is_none());
            assert!(Arc::ptr_eq(host.source.as_ref().unwrap(), &source));
        }
        // Even a forged Text body cannot override actual raw Workspace ownership.
        view.editor_mut().unwrap().document.representation = crate::hex_session::Representation::RawBytes;
        assert!(!host.update_prepared(&view, &page, current));
        assert!(host.worker_ready_prepared(&view, &page, current).is_empty());
        assert!(host.results.is_empty());
        assert!(view.hex_owns_document(document));
        assert!(view.editor_mut().is_none(), "raw ownership must keep the ordinary Workspace editor inaccessible");
        // This test owns a metadata-only representation transition. Restore
        // through that exact View, as the raw-body guard deliberately prevents
        // obtaining the normal Workspace editor while raw owns the body.
        view.view_mut(document).unwrap().editor_mut().unwrap().document.representation =
            crate::hex_session::Representation::DecodedText;
        assert!(!view.hex_owns_document(document));
        let restored = view.editor().expect("decoded editor must be available again").document();
        assert_eq!(restored.source_epoch, current.key.source.source_epoch);
        assert_eq!(restored.revision(), current.key.source.revision);
        assert!(
            Arc::ptr_eq(host.source.as_ref().unwrap(), &source),
            "representation guard must retain the exact syntax source owner"
        );
        host.suspend_prepared();
        assert_eq!(host.spans_prepared(&view, &page, current).count(), 0);
        assert!(host.worker_ready(&view).is_empty(), "late wake must not call raw logical update");
        view.editor_mut().unwrap().document.source_epoch = crate::document::next_source_epoch();
        assert!(!host.update_prepared(&view, &page, current));
        assert!(host.prepared_error().is_some());
        host.shutdown().unwrap();
    }

    #[test]
    fn prepared_same_revision_tabs_picker_and_path_policy_invalidate_spans() {
        let (mut view, mut host, events, mut current, page) = prepared_fixture("1 22 333", 8, 4);
        host.update_prepared(&view, &page, current);
        drain_prepared(&mut host, &view, &events, current, &page);
        let source = host.source.as_ref().unwrap().clone();
        let old_key = host.key;
        view.editor_mut().unwrap().document.path = Some("renamed.py".into());
        current.path_generation += 1;
        assert_eq!(host.spans_prepared(&view, &page, current).count(), 0);
        host.update_prepared(&view, &page, current);
        drain_prepared(&mut host, &view, &events, current, &page);
        assert_ne!(host.key, old_key);
        assert!(
            Arc::ptr_eq(host.source.as_ref().unwrap(), &source),
            "path and policy never duplicate source ownership"
        );
        host.set_language_choice(&view, view.active_id(), LanguageChoice::Explicit(Language::Log));
        assert_eq!(host.spans_prepared(&view, &page, current).count(), 0);
        host.update_prepared(&view, &page, current);
        drain_prepared(&mut host, &view, &events, current, &page);
        assert_eq!(host.detected_language(&view), Some(Language::Log));
        let mappings = AcceptedLanguageMappings::prepare(&BTreeMap::from([("py".into(), "rust".into())])).unwrap();
        assert!(host.apply_mappings(mappings));
        assert_eq!(host.spans_prepared(&view, &page, current).count(), 0);
        host.update_prepared(&view, &page, current);
        drain_prepared(&mut host, &view, &events, current, &page);
        assert_eq!(host.detected_language(&view), Some(Language::Log), "explicit picker survives mapping reload");
        let mut other = View::new(None);
        other.execute("edit.insert", &CommandArgs { text: Some("1 22 333".into()), ..Default::default() }, 1).unwrap();
        other.editor_mut().unwrap().settings.editor.word_wrap = true;
        let revision = view.editor().unwrap().document().revision();
        view.open(other).unwrap();
        assert_eq!(view.editor().unwrap().document().revision(), revision);
        assert!(!host.update_prepared(&view, &page, current));
        assert!(host.worker_ready_prepared(&view, &page, current).is_empty());
        assert_eq!(host.spans_prepared(&view, &page, current).count(), 0);
        host.shutdown().unwrap();
    }

    #[test]
    fn prepared_admission_refuses_omitted_normal_coverage_at_window_and_byte_limits() {
        // Each opaque cluster separates normal syntax windows. Layout may
        // preserve its full paint coverage while syntax admission reaches 128.
        let separated = format!("1a{}", "\u{301}".repeat(300)).repeat(MAX_SYNTAX_WINDOWS + 1);
        // Each small display cluster fits the normal payload allowance, but
        // its exact source bytes exceed the page's 256 KiB syntax allowance.
        let dense = format!("a{}", "\u{301}".repeat(64)).repeat(2200);
        for text in [separated, dense] {
            let (view, mut host, _events, current, page) = prepared_fixture(&text, 64, 512);
            assert!(page.at_end, "control must retain the complete fixture as paint coverage");
            assert!(matches!(prepared_windows(&page), Err(WrapError::Admission(_))));
            assert!(!host.update_prepared(&view, &page, current));
            assert!(host.prepared_error().is_some());
            assert!(host.source.is_none() && host.snapshot.is_none(), "refusal precedes source pinning");
            assert!(host.pending.is_empty() && !host.in_flight);
            assert_eq!(host.spans_prepared(&view, &page, current).count(), 0);
            host.shutdown().unwrap();
        }
    }

    #[test]
    fn prepared_all_opaque_page_runs_detection_without_any_syntax_extent() {
        let text = format!("a{}", "\u{301}".repeat(300));
        let (view, mut host, events, current, page) = prepared_fixture(&text, 8, 4);
        assert!(page.syntax_windows().is_empty());
        assert!(page.rows().iter().any(|row| row.source_coverage == (0..text.len() as u64)));
        host.set_language_choice(&view, view.active_id(), LanguageChoice::Explicit(Language::Rust));
        assert!(host.update_prepared(&view, &page, current));
        assert!(drain_prepared(&mut host, &view, &events, current, &page).is_empty());
        assert_eq!(host.detected_language(&view), Some(Language::Rust));
        assert!(
            host.results
                .iter()
                .all(|result| result.highlights.requested.is_empty() && result.highlights.queried.is_empty())
        );
        assert_eq!(host.spans_prepared(&view, &page, current).count(), 0);
        host.shutdown().unwrap();
    }

    #[test]
    fn viewport_reissue_keeps_language_but_rejects_changed_source_and_policy() {
        let (mut view, mut host, receiver) = policy_fixture();
        view.editor_mut().unwrap().document.path = Some("server.log.gz".into());
        let id = view.active_id();
        host.set_language_choice(&view, id, LanguageChoice::Explicit(Language::Log));
        host.update(&view);
        drain_policy(&mut host, &view, &receiver);
        assert_eq!(host.detected_language(&view), Some(Language::Log));
        view.set_rows(0);
        assert!(host.update(&view));
        assert!(host.results.is_empty(), "new viewport has no accepted spans yet");
        assert_eq!(host.detected_language(&view), Some(Language::Log), "language survives pending viewport work");
        view.editor_mut().unwrap().document.source_epoch = crate::document::next_source_epoch();
        assert_eq!(host.detected_language(&view), None, "replacement source never inherits accepted detection");
        host.update(&view);
        drain_policy(&mut host, &view, &receiver);
        host.set_language_choice(&view, id, LanguageChoice::Explicit(Language::Generic));
        assert_eq!(host.detected_language(&view), None, "policy change is immediately authoritative");
        host.shutdown().unwrap();
    }

    #[test]
    fn raw_body_late_wake_and_mapping_reload_never_schedule_or_pin_syntax() {
        use crate::hex_session::Representation;
        let (mut view, mut host, receiver) = policy_fixture();
        let document = view.active_id();
        // Metadata-only transport transitions isolate this session guard from
        // the separately tested document-worker raw reopen machinery.
        view.editor_mut().unwrap().document.representation = Representation::RawBytes;
        view.set_rows(0);
        assert!(view.hex_active());
        assert!(!host.update(&view));
        assert!(host.worker_ready(&view).is_empty());
        assert_eq!(host.generation, 0);
        assert!(host.key.is_none());
        assert!(host.identity.is_none());
        assert!(host.source.is_none(), "fresh raw body cannot acquire a syntax source pin");
        assert!(host.snapshot.is_none());
        assert!(host.pending.is_empty());
        assert!(host.rejected.is_none());
        assert!(!host.in_flight);

        view.view_mut(document).unwrap().editor_mut().unwrap().document.representation = Representation::DecodedText;
        view.set_rows(2);
        assert!(host.update(&view));
        drain_policy(&mut host, &view, &receiver);
        assert_eq!(host.detected_language(&view), Some(Language::Rust));
        assert!(host.spans(&view).next().is_some());
        let generation = host.generation;
        let key = host.key;
        let source = Arc::as_ptr(host.source.as_ref().unwrap());
        let snapshot = Arc::as_ptr(host.snapshot.as_ref().unwrap());
        // A late wake could otherwise pump another disjoint text window.
        host.pending.push_back(0..1);
        view.editor_mut().unwrap().document.representation = Representation::RawBytes;
        view.set_rows(0);
        assert_eq!(host.current_key(&view), None);
        assert_eq!(host.detection(&view), None, "same-revision cached text detection is hidden in raw mode");
        assert_eq!(host.spans(&view).count(), 0, "same-revision cached text spans are hidden in raw mode");
        assert!(host.worker_ready(&view).is_empty());
        assert!(!host.in_flight, "late wake cannot pump queued text work for a raw body");
        let mappings = AcceptedLanguageMappings::prepare(&BTreeMap::from([("rs".into(), "python".into())])).unwrap();
        assert!(host.apply_mappings(mappings));
        assert!(!host.update(&view));
        assert!(host.worker_ready(&view).is_empty());
        assert_eq!(host.generation, generation);
        assert_eq!(host.key, key);
        assert_eq!(host.pending.iter().cloned().collect::<Vec<_>>(), vec![0..1]);
        assert!(!host.in_flight);
        assert!(host.rejected.is_none());
        assert!(
            std::ptr::addr_eq(Arc::as_ptr(host.source.as_ref().unwrap()), source),
            "retain the existing bounded text source for worker retirement, never replace it with raw"
        );
        assert_eq!(Arc::as_ptr(host.snapshot.as_ref().unwrap()), snapshot);

        view.view_mut(document).unwrap().editor_mut().unwrap().document.representation = Representation::DecodedText;
        view.set_rows(2);
        assert!(host.update(&view));
        assert!(host.generation > generation);
        drain_policy(&mut host, &view, &receiver);
        assert_eq!(host.detected_language(&view), Some(Language::Python));
        assert_eq!(host.detection(&view).unwrap().origin, DetectionOrigin::UserMapping);
        assert!(host.spans(&view).next().is_some());
        host.worker.take().unwrap().shutdown().unwrap();
    }

    #[test]
    fn accepted_mapping_reload_and_picker_reissue_without_document_edits() {
        let (view, mut host, receiver) = policy_fixture();
        let document = view.active_id();
        let revision = view.editor().unwrap().document().revision();
        assert!(host.update(&view));
        drain_policy(&mut host, &view, &receiver);
        assert_eq!(host.detected_language(&view), Some(Language::Rust));
        assert!(host.spans(&view).next().is_some());
        let old_key = host.key;

        let table = BTreeMap::from([("rs".into(), "python".into())]);
        let mappings = AcceptedLanguageMappings::prepare(&table).unwrap();
        assert!(host.apply_mappings(mappings.clone()));
        assert_eq!(host.detection(&view), None, "old policy detection must be hidden before update");
        assert_eq!(host.spans(&view).count(), 0, "old policy spans must be hidden before update");
        assert!(host.update(&view));
        assert_ne!(host.key, old_key);
        drain_policy(&mut host, &view, &receiver);
        assert_eq!(host.detection(&view).unwrap().origin, DetectionOrigin::UserMapping);
        assert_eq!(host.detected_language(&view), Some(Language::Python));
        assert!(!host.apply_mappings(mappings));
        assert!(!host.update(&view), "equal accepted mappings must not wake parsing");

        let mapped_key = host.key;
        let picker = host.language_picker(&view);
        assert!(host.apply_picker(&view, &picker, LanguageChoice::Explicit(Language::Generic)));
        assert_eq!(host.language_status(&view).label(), "generic");
        assert_eq!(host.detection(&view), None);
        assert_eq!(host.spans(&view).count(), 0);
        assert!(host.update(&view));
        assert_ne!(host.key, mapped_key);
        drain_policy(&mut host, &view, &receiver);
        assert_eq!(host.detection(&view).unwrap().origin, DetectionOrigin::Picker);
        assert_eq!(host.detected_language(&view), Some(Language::Generic));

        // Reload during an explicit override must not replace that override.
        let new_table = BTreeMap::from([("rs".into(), "log".into())]);
        assert!(host.apply_mappings(AcceptedLanguageMappings::prepare(&new_table).unwrap()));
        assert!(host.update(&view));
        drain_policy(&mut host, &view, &receiver);
        assert_eq!(host.detected_language(&view), Some(Language::Generic));
        assert!(host.set_language_choice(&view, document, LanguageChoice::Automatic));
        assert!(host.update(&view));
        drain_policy(&mut host, &view, &receiver);
        let detection = host.detection(&view).unwrap();
        assert_eq!(detection.language, Language::Log);
        assert!(detection.log_levels);
        assert_eq!(detection.origin, DetectionOrigin::UserMapping);
        assert_eq!(view.editor().unwrap().document().revision(), revision);
        assert!(host.apply_mappings(AcceptedLanguageMappings::default()));
        assert!(host.update(&view));
        drain_policy(&mut host, &view, &receiver);
        assert_eq!(host.detected_language(&view), Some(Language::Rust), "removing mappings restores detection");
        host.worker.take().unwrap().shutdown().unwrap();
    }

    #[test]
    fn explicit_choice_survives_save_as_and_old_path_results_are_never_exposed() {
        let (mut view, mut host, receiver) = policy_fixture();
        let document = view.active_id();
        let revision = view.editor().unwrap().document().revision();
        assert!(host.set_language_choice(&view, document, LanguageChoice::Explicit(Language::Python)));
        assert!(host.update(&view));
        drain_policy(&mut host, &view, &receiver);
        let old_key = host.key;
        view.editor_mut().unwrap().document.path = Some("renamed.log".into());
        assert_eq!(host.detected_language(&view), None);
        assert_eq!(host.spans(&view).count(), 0, "path-only Save As invalidates already-published spans");
        assert_eq!(host.language_choice(document), LanguageChoice::Explicit(Language::Python));
        assert!(host.update(&view));
        assert_ne!(host.key, old_key);
        drain_policy(&mut host, &view, &receiver);
        assert_eq!(host.detected_language(&view), Some(Language::Python));
        assert_eq!(host.detection(&view).unwrap().origin, DetectionOrigin::Picker);
        assert!(host.set_language_choice(&view, document, LanguageChoice::Automatic));
        assert!(host.update(&view));
        drain_policy(&mut host, &view, &receiver);
        assert_eq!(host.detected_language(&view), Some(Language::Log));
        assert_eq!(view.editor().unwrap().document().revision(), revision);
        host.worker.take().unwrap().shutdown().unwrap();
    }

    #[test]
    fn same_revision_tabs_keep_choices_and_reject_stale_picker_and_result() {
        let (mut view, mut host, receiver) = policy_fixture();
        let first = view.active_id();
        assert!(host.set_language_choice(&view, first, LanguageChoice::Explicit(Language::Python)));
        assert!(host.update(&view));
        drain_policy(&mut host, &view, &receiver);
        let first_result = host.results[0].clone();
        let picker = host.language_picker(&view);
        let mut other = View::new(None);
        other.set_rows(2);
        other
            .execute(
                "edit.insert",
                &CommandArgs { text: Some("value = 123 // comment".into()), ..Default::default() },
                1,
            )
            .unwrap();
        let second = view.open(other).unwrap();
        assert_eq!(view.editor().unwrap().document().revision(), first_result.key.revision);
        assert!(!host.apply_picker(&view, &picker, LanguageChoice::Explicit(Language::Lua)));
        assert_eq!(host.spans(&view).count(), 0);
        assert!(host.set_language_choice(&view, second, LanguageChoice::Explicit(Language::Generic)));
        assert!(host.update(&view));
        drain_policy(&mut host, &view, &receiver);
        assert_eq!(host.detected_language(&view), Some(Language::Generic));
        host.results.push(first_result.clone());
        assert_eq!(host.detected_language(&view), Some(Language::Generic), "late cross-tab results stay hidden");
        assert_eq!(host.spans(&view).count(), host.results[0].highlights.spans.len());

        assert!(view.activate(first));
        assert!(!host.apply_picker(&view, &picker, LanguageChoice::Explicit(Language::Lua)), "old activation epoch");
        assert!(host.update(&view));
        // Same DocumentId/revision is insufficient after a new generation.
        host.results.push(first_result);
        assert_eq!(host.detection(&view), None);
        assert_eq!(host.spans(&view).count(), 0);
        drain_policy(&mut host, &view, &receiver);
        assert_eq!(host.detected_language(&view), Some(Language::Python));
        assert_eq!(host.language_choice(second), LanguageChoice::Explicit(Language::Generic));
        assert!(!host.set_language_choice(&view, DocumentId(u64::MAX), LanguageChoice::Explicit(Language::Rust)));
        host.worker.take().unwrap().shutdown().unwrap();
    }

    #[test]
    fn empty_document_still_detects_and_reissues_a_language_pick() {
        let mut view = Workspace::new(View::new(None)).unwrap();
        view.set_rows(1);
        view.editor_mut().unwrap().document.path = Some("empty.rs".into());
        let (sender, receiver) = mpsc::channel();
        let mut host = HighlightSession::new(move || {
            let _ = sender.send(());
        })
        .unwrap();
        assert!(host.update(&view));
        drain_policy(&mut host, &view, &receiver);
        assert_eq!(host.detected_language(&view), Some(Language::Rust));
        assert_eq!(host.spans(&view).count(), 0);
        let picker = host.language_picker(&view);
        assert!(host.apply_picker(&view, &picker, LanguageChoice::Explicit(Language::Generic)));
        assert!(host.update(&view));
        drain_policy(&mut host, &view, &receiver);
        assert_eq!(host.detected_language(&view), Some(Language::Generic));
        assert_eq!(host.detection(&view).unwrap().origin, DetectionOrigin::Picker);
        host.worker.take().unwrap().shutdown().unwrap();
    }

    #[test]
    fn frontend_typing_preserves_incremental_tree() {
        let mut view = Workspace::new(View::new(None)).unwrap();
        view.set_rows(3);
        view.execute(
            "edit.insert",
            &CommandArgs { text: Some("fn main() { let n = 1; }\n".into()), ..Default::default() },
            1,
        )
        .unwrap();
        view.editor_mut().unwrap().document.path = Some("incremental.rs".into());
        let (sender, receiver) = mpsc::channel();
        let mut host = HighlightSession::new(move || {
            let _ = sender.send(());
        })
        .unwrap();
        let drain = |host: &mut HighlightSession, view: &Workspace| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while host.in_flight || !host.pending.is_empty() || host.rejected.is_some() {
                receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
                host.worker_ready(view);
            }
        };
        host.update(&view);
        drain(&mut host, &view);
        assert_eq!(host.results[0].highlights.mode, Mode::TreeSitter);
        let cursors = view.editor().unwrap().cursors().to_vec();
        view.editor_mut().unwrap().set_cursors(cursors, 0).unwrap();
        let accepting = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let gate = accepting.clone();
        let observed = calls.clone();
        view.editor_mut()
            .unwrap()
            .set_edit_observer(Some(Box::new(move |receipt| {
                observed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if gate.load(std::sync::atomic::Ordering::Relaxed) { Ok(()) } else { Err(receipt) }
            })))
            .unwrap();
        view.execute("edit.insert", &CommandArgs { text: Some("// next".into()), ..Default::default() }, 1).unwrap();
        host.update(&view);
        drain(&mut host, &view);
        assert!(host.results[0].highlights.incremental, "frontend typing must retain and edit its prior tree");
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert!(view.editor().unwrap().has_pending_edit_receipt());
        assert_eq!(view.editor().unwrap().next_edit_sequence(), 2);
        assert_eq!(view.editor_mut().unwrap().undo().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
        accepting.store(true, std::sync::atomic::Ordering::Relaxed);
        view.editor_mut().unwrap().flush_edit_receipt().unwrap();
        for command in ["edit.undo", "edit.redo"] {
            view.execute(command, &CommandArgs::default(), 1).unwrap();
            host.update(&view);
            drain(&mut host, &view);
            assert!(host.results[0].highlights.incremental, "{command} must stage its exact transaction");
        }
        for _ in 0..2 {
            view.execute("edit.insert", &CommandArgs { text: Some(" ".into()), ..Default::default() }, 1).unwrap();
        }
        host.update(&view);
        drain(&mut host, &view);
        assert!(!host.results[0].highlights.incremental, "missing transaction chain must reset");
        view.execute("edit.insert", &CommandArgs { text: Some(" ".into()), ..Default::default() }, 1).unwrap();
        host.update(&view);
        drain(&mut host, &view);
        assert!(host.results[0].highlights.incremental, "incremental parsing resumes after a reset");
        host.worker.take().unwrap().shutdown().unwrap();
    }

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
