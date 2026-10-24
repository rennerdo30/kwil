//! One frontend-owned highlighting worker, with bounded disjoint viewport
//! windows. A giant offscreen line gap never expands a source request.
use crate::{
    Workspace,
    highlighting::*,
    language_selection::{AcceptedLanguageMappings, LanguagePicker, LanguageStatus},
};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    ops::Range,
    sync::Arc,
};

const WINDOW: usize = MAX_WINDOW_BYTES - 8192;

#[derive(PartialEq, Eq)]
struct Identity {
    document: DocumentId,
    input_epoch: u64,
    revision: u64,
    path: std::path::PathBuf,
    settings_generation: u64,
    choice: LanguageChoice,
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
    snapshot: Option<Arc<SnapshotTextSource>>,
    path: Arc<std::path::PathBuf>,
    mappings: Arc<[LanguageMapping]>,
    settings_generation: u64,
    choices: HashMap<DocumentId, LanguageChoice>,
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
            snapshot: None,
            path: Arc::new(Default::default()),
            mappings: Arc::from([]),
            settings_generation: 0,
            choices: HashMap::new(),
            results: Vec::new(),
            painted: BTreeMap::new(),
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
        if view.hex_active() {
            return None;
        }
        let identity = self.identity.as_ref()?;
        let editor = view.editor()?;
        (identity.document == view.active_id()
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
            input_epoch: view.input_epoch(),
            revision: editor.document().revision(),
            path: editor.document().path().unwrap_or(std::path::Path::new("")).to_path_buf(),
            settings_generation: self.settings_generation,
            choice: self.language_choice(view.active_id()),
            windows,
        };
        if self.identity.as_ref() == Some(&next) {
            return false;
        }
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
        if self.identity.as_ref().is_none_or(|old| old.document != next.document) {
            self.painted.clear();
        }
        self.painted.retain(|row, _| *row >= view.top() && *row < view.top().saturating_add(view.rows() as u64));
        self.results.clear();
        // Retire superseded handles off the input thread, including rejected
        // requests the syntax worker never accepted.
        let source = Arc::new(SnapshotTextSource::from_editor(editor));
        self.source = Some(source.clone());
        self.snapshot = Some(source);
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
        match worker.submit(request) {
            Ok(_) => self.in_flight = true,
            Err(error) => self.rejected = Some(error.request),
        }
    }

    /// Call only on the worker wake. Each accepted result styles one bounded
    /// window, then the same worker advances to the next visible window.
    pub fn worker_ready(&mut self, view: &Workspace) -> Vec<usize> {
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
        let key = self.current_key(view)?;
        self.results.iter().rev().find(|result| result.key == key).map(|result| result.detection)
    }

    pub fn detected_language(&self, view: &Workspace) -> Option<Language> {
        self.detection(view).map(|detection| detection.language)
    }

    pub fn spans<'a>(&'a self, view: &'a Workspace) -> impl Iterator<Item = &'a Span> {
        let key = self.current_key(view);
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
            self.painted.remove(&(view.top() + *row as u64));
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
