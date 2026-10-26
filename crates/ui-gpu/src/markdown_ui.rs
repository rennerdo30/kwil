//! Bounded native Markdown host. One worker, one rejection owner, no UI reads.
use super::*;
use kwil_core_markdown::{BlockKind, InlineKind, ListMarker, Model, Span};
use kwil_core_runtime::{NativeExitObservation, NativeExitState, NativeObservationError};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Mode {
    #[default]
    Text,
    Preview,
    Split,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModeAction {
    Unhandled,
    Unavailable,
    Set(Mode),
}
fn mode_action(command: &str, current: Mode, markdown_source: bool) -> ModeAction {
    let mode = match command {
        "view.toggleMarkdown" => match current {
            Mode::Text => Mode::Split,
            Mode::Split => Mode::Preview,
            Mode::Preview => Mode::Text,
        },
        "view.markdownText" => Mode::Text,
        "view.markdownPreview" => Mode::Preview,
        "view.markdownSplit" => Mode::Split,
        _ => return ModeAction::Unhandled,
    };
    if mode == Mode::Text || markdown_source { ModeAction::Set(mode) } else { ModeAction::Unavailable }
}
fn controls_available(markdown_source: bool, current: Mode) -> bool {
    markdown_source || current != Mode::Text
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Key {
    document: kwil_view_model::SessionDocumentId,
    revision: u64,
    epoch: u64,
    base: u64,
}
struct Work {
    key: Key,
    snapshot: kwil_core_text::Snapshot,
    path: Option<AdmittedPath>,
}
// Field order matters: capacity credit outlives the allocation it funds.
struct AdmittedPath {
    value: std::path::PathBuf,
    _charge: kwil_view_model::visual_rows::MetadataReservation,
}
impl AdmittedPath {
    fn copy(
        source: &std::path::Path,
        budget: &kwil_view_model::visual_rows::AllocationBudget,
    ) -> kwil_view_model::visual_rows::WrapResult<Self> {
        use kwil_view_model::visual_rows::WrapError;
        // Fresh capacity is admitted while the old cache/request stays live.
        // Inspect allocator surplus; retain the whole admitted envelope.
        let bytes = source.as_os_str().len().checked_mul(2).ok_or(WrapError::Admission("Markdown path overflow"))?;
        let charge = budget.reserve_metadata(bytes)?;
        let mut value = std::path::PathBuf::new();
        value
            .try_reserve_exact(source.as_os_str().len())
            .map_err(|_| WrapError::Admission("Markdown path allocation"))?;
        if value.capacity() > bytes {
            return Err(WrapError::Admission("Markdown path capacity exceeded admission"));
        }
        value.push(source);
        if value.capacity() > bytes {
            return Err(WrapError::Admission("Markdown path capacity exceeded admission"));
        }
        Ok(Self { value, _charge: charge })
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rejection {
    Full,
    Disconnected,
}
impl Rejection {
    fn notice(self) -> &'static str {
        match self {
            Self::Full => "Preview could not accept this request. Restart Kwil to retry.",
            Self::Disconnected => "Preview stopped. Restart Kwil to retry.",
        }
    }
}
struct RejectedWork {
    reason: Rejection,
    work: Work,
}
fn arc_allocation_bytes<T>() -> Option<usize> {
    std::alloc::Layout::new::<(std::sync::atomic::AtomicUsize, std::sync::atomic::AtomicUsize)>()
        .extend(std::alloc::Layout::new::<T>())
        .ok()
        .map(|(layout, _)| layout.pad_to_align().size())
}
pub(super) struct Completion {
    key: Key,
    result: Result<Model, String>,
    images: markdown_images::Images,
    diagrams: Diagrams,
    highlights: CodeHighlights,
}
#[cfg(test)]
pub(super) fn notification_fixture_completion(document: kwil_view_model::SessionDocumentId) -> Completion {
    // A completed value with no Snapshot/worker capability, as after run's
    // existing explicit Snapshot drop. No source extraction is used by tests.
    Completion {
        key: Key { document, revision: 0, epoch: 0, base: 0 },
        result: Err("original computed completion".into()),
        images: Default::default(),
        diagrams: Default::default(),
        highlights: Default::default(),
    }
}
type CodeHighlights = std::collections::HashMap<usize, Vec<kwil_core_syntax::Span>>;
type Diagrams = std::collections::HashMap<usize, Result<kwil_core_markdown::diagram::Diagram, String>>;
type DiagramLayouts = std::collections::HashMap<usize, ((usize, u64, u64), Result<markdown_diagram::Layout, String>)>;
struct Lane {
    tx: Option<mpsc::SyncSender<Work>>,
    thread: Option<std::thread::JoinHandle<()>>,
    retirement_pending: bool,
}
#[derive(Default)]
struct Terminal {
    done: std::sync::Mutex<bool>,
    changed: std::sync::Condvar,
    native: NativeExitObservation,
}
struct ExitGuard(Arc<Terminal>);
impl Drop for ExitGuard {
    fn drop(&mut self) {
        *self.0.done.lock().unwrap_or_else(|error| error.into_inner()) = true;
        self.0.changed.notify_all();
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettlementReason {
    TimedOut,
    ThreadExitPending,
    WorkerPanicked,
    Native(NativeObservationError),
}
#[derive(Default)]
pub(super) struct Host {
    modes: std::collections::HashMap<kwil_view_model::SessionDocumentId, Mode>,
    lane: Option<Lane>,
    terminal: Option<Arc<Terminal>>,
    rejected: Option<RejectedWork>,
    budget: Option<kwil_view_model::visual_rows::AllocationBudget>,
    panic: Option<Box<dyn std::any::Any + Send>>,
    deadline: Option<std::time::Instant>,
    native_error: Option<NativeObservationError>,
    stop: Arc<AtomicBool>,
    busy: bool,
    desired: Option<Key>,
    current: Option<Key>,
    base_path: Option<AdmittedPath>,
    path_generation: u64,
    model: Option<Model>,
    images: markdown_images::Images,
    diagrams: Diagrams,
    layouts: DiagramLayouts,
    highlights: CodeHighlights,
    error: Option<String>,
    admission_notice: Option<&'static str>,
    scroll: usize,
    #[cfg(feature = "frame-harness")]
    probe_target: Option<u64>,
    // Last: source/path owners must be destroyed before fixed metadata credit.
    _terminal_charge: Option<kwil_view_model::visual_rows::MetadataReservation>,
}
impl Host {
    fn metadata_bytes() -> Option<usize> {
        // Declared Rust headers/transfer overlap only. This does not bound
        // std channel internals, native stacks, parser heaps or source storage.
        let sizes = [
            std::mem::size_of::<Self>().checked_mul(2)?,
            arc_allocation_bytes::<Terminal>()?,
            arc_allocation_bytes::<AtomicBool>()?,
            std::mem::size_of::<ExitGuard>().checked_mul(2)?,
            std::mem::size_of::<Lane>().checked_mul(2)?,
            std::mem::size_of::<Work>().checked_mul(2)?,
            std::mem::size_of::<mpsc::TrySendError<Work>>(),
            std::mem::size_of::<Result<(), mpsc::TrySendError<Work>>>(),
            std::mem::size_of::<Option<RejectedWork>>(),
            std::mem::size_of::<Option<AdmittedPath>>().checked_mul(2)?,
            std::mem::size_of::<kwil_view_model::visual_rows::WrapResult<(Option<AdmittedPath>, Option<AdmittedPath>)>>(
            ),
            std::mem::size_of::<kwil_view_model::visual_rows::WrapResult<AdmittedPath>>().checked_mul(2)?,
            std::mem::size_of::<Result<(), SettlementReason>>().checked_mul(2)?,
            NativeExitObservation::operation_stack_bytes()?,
            std::mem::size_of::<std::time::Instant>(), // effective deadline passed into wait closure
        ];
        sizes.into_iter().try_fold(0usize, usize::checked_add)
    }
    pub fn with_budget(
        budget: &kwil_view_model::visual_rows::AllocationBudget,
    ) -> kwil_view_model::visual_rows::WrapResult<Self> {
        let bytes = Self::metadata_bytes()
            .ok_or(kwil_view_model::visual_rows::WrapError::Admission("Markdown native-exit metadata overflow"))?;
        let charge = budget.reserve_metadata(bytes)?;
        Ok(Self {
            terminal: Some(Arc::new(Terminal::default())),
            budget: Some(budget.clone()),
            _terminal_charge: Some(charge),
            ..Self::default()
        })
    }
    fn capture_blocked(&self) -> bool {
        self.rejected.is_some() || self.deadline.is_some() || self.stop.load(Ordering::Acquire)
    }
    fn notice(&self) -> Option<&str> {
        self.rejected
            .as_ref()
            .map(|rejected| rejected.reason.notice())
            .or(self.admission_notice)
            .or(self.error.as_deref())
    }
    /// The only source-capturing submit entry. No Work exists on the occupied,
    /// closed-sender or allocation-refusal paths. All preparation precedes capture.
    fn submit_with(
        &mut self,
        mut key: Key,
        path: Option<&std::path::Path>,
        capture: impl FnOnce() -> kwil_core_text::Snapshot,
    ) {
        if self.capture_blocked() {
            return;
        }
        if self.lane.as_ref().and_then(|lane| lane.tx.as_ref()).is_none() {
            self.admission_notice = Some("Preview unavailable: worker sender is closed.");
            return;
        }
        let changed = path != self.base_path.as_ref().map(|path| path.value.as_path());
        if changed {
            let Some(generation) = self.path_generation.checked_add(1) else {
                self.admission_notice = Some("Preview path generation exhausted; reopen the application");
                return;
            };
            key.base = generation;
        } else {
            key.base = self.path_generation;
        }
        let needs_work = !self.busy && (self.desired != Some(key) || self.current != Some(key));
        let Some(budget) = &self.budget else {
            self.admission_notice = Some("Preview unavailable: request metadata was not admitted before startup.");
            return;
        };
        // Hold both fresh allocations AND the old cache until BOTH admissions
        // succeed. A refusal preserves its allocation, generation and desired key.
        let prepared = (|| {
            let cache = if changed { path.map(|path| AdmittedPath::copy(path, budget)).transpose()? } else { None };
            let request =
                if needs_work { path.map(|path| AdmittedPath::copy(path, budget)).transpose()? } else { None };
            Ok::<_, kwil_view_model::visual_rows::WrapError>((cache, request))
        })();
        let (cache, request) = match prepared {
            Ok(paths) => paths,
            Err(_) => {
                self.admission_notice = Some("Preview unavailable: path metadata admission refused.");
                return;
            }
        };
        if changed {
            self.base_path = cache;
            self.path_generation = key.base;
        }
        self.admission_notice = None;
        if self.desired != Some(key) {
            if self.desired.is_none_or(|old| old.document != key.document) {
                self.scroll = 0;
            }
            self.desired = Some(key);
            self.current = None;
            self.model = None;
            self.error = None;
        }
        if !needs_work {
            return;
        }
        // No callbacks or fallible preparation between the empty-slot gate,
        // capture and ownership transfer. The sender was checked before capture.
        let tx = self.lane.as_ref().unwrap().tx.as_ref().unwrap();
        let work = Work { key, snapshot: capture(), path: request };
        match tx.try_send(work) {
            Ok(()) => self.busy = true,
            Err(mpsc::TrySendError::Full(work)) => self.rejected = Some(RejectedWork { reason: Rejection::Full, work }),
            Err(mpsc::TrySendError::Disconnected(work)) => {
                self.rejected = Some(RejectedWork { reason: Rejection::Disconnected, work });
            }
        }
    }
    pub fn panic_payload(&self) -> Option<&(dyn std::any::Any + Send)> {
        self.panic.as_deref()
    }
    /// Cached registration evidence only; querying it never performs native I/O.
    fn registration_failure(&mut self) -> Option<NativeObservationError> {
        let error = self.terminal.as_ref()?.native.registration_error()?;
        Some(*self.native_error.get_or_insert(error))
    }
    /// OFF UI; borrow the original host through timeout, panic and retry.
    pub fn settle_until(&mut self, deadline: std::time::Instant) -> Result<(), SettlementReason> {
        self.settle_deadline_with(
            deadline,
            |terminal, thread, deadline| terminal.native.wait_until(thread, deadline),
            NativeExitObservation::retire_after_join,
        )
    }
    #[cfg(test)]
    fn settle_with(
        &mut self,
        deadline: std::time::Instant,
        mut query: impl FnMut(&Terminal, &std::thread::JoinHandle<()>) -> Result<NativeExitState, NativeObservationError>,
        retire: impl FnMut(&NativeExitObservation) -> Result<(), NativeObservationError>,
    ) -> Result<(), SettlementReason> {
        self.settle_deadline_with(deadline, |terminal, thread, _| query(terminal, thread), retire)
    }
    fn settle_deadline_with(
        &mut self,
        deadline: std::time::Instant,
        mut query: impl FnMut(
            &Terminal,
            &std::thread::JoinHandle<()>,
            std::time::Instant,
        ) -> Result<NativeExitState, NativeObservationError>,
        mut retire: impl FnMut(&NativeExitObservation) -> Result<(), NativeObservationError>,
    ) -> Result<(), SettlementReason> {
        let deadline = self.deadline.map_or(deadline, |stored| stored.min(deadline));
        self.deadline = Some(deadline);
        self.stop.store(true, Ordering::Release);
        if let Some(lane) = &mut self.lane {
            drop(lane.tx.take());
        }
        self.retire_native_with(deadline, &mut retire)?;
        if self.panic.is_some() {
            return Err(SettlementReason::WorkerPanicked);
        }
        if self.lane.is_none() {
            return self.retire_rejected(deadline);
        }
        Self::check_deadline(deadline)?;
        if self.lane.as_ref().unwrap().thread.is_none() {
            return self.retire_rejected(deadline);
        }
        {
            let terminal = self.terminal.as_ref().expect("admitted before worker startup");
            let done = terminal.done.lock().unwrap_or_else(|error| error.into_inner());
            let (done, _) = terminal
                .changed
                .wait_timeout_while(done, deadline.saturating_duration_since(std::time::Instant::now()), |done| !*done)
                .unwrap_or_else(|error| error.into_inner());
            if !*done {
                return Err(SettlementReason::TimedOut);
            }
        }
        Self::check_deadline(deadline)?;
        let observed =
            query(self.terminal.as_ref().unwrap(), self.lane.as_ref().unwrap().thread.as_ref().unwrap(), deadline);
        let observed = match observed {
            Err(error) => return Err(self.retain_native_error(error)),
            Ok(state) => state,
        };
        Self::check_deadline(deadline)?;
        if observed == NativeExitState::Pending {
            return Err(SettlementReason::ThreadExitPending);
        }
        let lane = self.lane.as_mut().unwrap();
        if let Err(panic) = lane.thread.take().unwrap().join() {
            self.panic = Some(panic);
        }
        lane.retirement_pending = true;
        self.retire_native_with(deadline, &mut retire)?;
        if self.panic.is_some() {
            return Err(SettlementReason::WorkerPanicked);
        }
        self.retire_rejected(deadline)
    }
    fn retire_rejected(&mut self, deadline: std::time::Instant) -> Result<(), SettlementReason> {
        self.retire_rejected_with(deadline, std::time::Instant::now)
    }
    fn retire_rejected_with(
        &mut self,
        deadline: std::time::Instant,
        mut now: impl FnMut() -> std::time::Instant,
    ) -> Result<(), SettlementReason> {
        if self.rejected.is_some() {
            if now() >= deadline {
                return Err(SettlementReason::TimedOut);
            }
            // OFF UI, after EVERY native/sticky-panic gate. Work drops its
            // source and path before path credit; Host retains fixed credit.
            // Destruction is not interruptible and ignored unlink errors are
            // unchanged. An overrun reports failure, not a renewed deadline.
            if let Some(RejectedWork { work, .. }) = self.rejected.take() {
                drop(work);
            }
            if now() >= deadline {
                return Err(SettlementReason::TimedOut);
            }
        }
        Ok(())
    }
    fn check_deadline(deadline: std::time::Instant) -> Result<(), SettlementReason> {
        if std::time::Instant::now() >= deadline { Err(SettlementReason::TimedOut) } else { Ok(()) }
    }
    fn retain_native_error(&mut self, error: NativeObservationError) -> SettlementReason {
        SettlementReason::Native(*self.native_error.get_or_insert(error))
    }
    fn retire_native_with(
        &mut self,
        deadline: std::time::Instant,
        retire: impl FnOnce(&NativeExitObservation) -> Result<(), NativeObservationError>,
    ) -> Result<(), SettlementReason> {
        if self.lane.as_ref().is_some_and(|lane| lane.retirement_pending) {
            Self::check_deadline(deadline)?;
            if let Err(error) = retire(&self.terminal.as_ref().unwrap().native) {
                return Err(self.retain_native_error(error));
            }
            self.lane.as_mut().unwrap().retirement_pending = false;
            Self::check_deadline(deadline)?;
        }
        Ok(())
    }
    #[cfg(feature = "frame-harness")]
    pub fn ready(&self) -> bool {
        !self.capture_blocked()
            && self.model.as_ref().is_some_and(|model| !model.blocks.is_empty())
            && self.current == self.desired
    }

    fn mode(&self, id: kwil_view_model::SessionDocumentId) -> Mode {
        self.modes.get(&id).copied().unwrap_or_default()
    }
    fn receive(&mut self, completion: Completion) {
        if self.capture_blocked() {
            return;
        }
        self.busy = false;
        if self.desired != Some(completion.key) {
            return;
        }
        self.current = Some(completion.key);
        self.images = completion.images;
        self.diagrams = completion.diagrams;
        self.layouts.clear();
        self.highlights = completion.highlights;
        match completion.result {
            Ok(model) => {
                self.model = Some(model);
                self.error = None;
            }
            Err(error) => {
                self.model = None;
                self.error = Some(error);
            }
        }
    }
}
fn code_highlights(model: &Model, stop: &AtomicBool) -> CodeHighlights {
    use kwil_core_syntax::{Config, Engine, Language};
    let mut result = CodeHighlights::new();
    let Ok(mut engine) = Engine::new(Config { parse_limit: 0, margin_bytes: 0, ..Default::default() }) else {
        return result;
    };
    let mut count = 0usize;
    for (index, block) in model.blocks.iter().enumerate() {
        if stop.load(Ordering::Acquire) {
            break;
        }
        let BlockKind::CodeBlock { language: Some(language), raw } = &block.kind else {
            continue;
        };
        let language = language.split_whitespace().next().unwrap_or("");
        let Some(language) = Language::ALL.iter().copied().find(|candidate| {
            candidate.id().eq_ignore_ascii_case(language)
                || candidate.extensions().split_whitespace().any(|ext| ext.eq_ignore_ascii_case(language))
        }) else {
            continue;
        };
        let source = raw.as_bytes().to_vec();
        if let Ok(highlighted) = engine
            .highlight(&source, language, index as u64, None, None, 0..source.len(), &|| stop.load(Ordering::Acquire))
        {
            let spans: Vec<_> = highlighted.spans.into_iter().take(8192usize.saturating_sub(count)).collect();
            count += spans.len();
            result.insert(index, spans);
        }
        if count >= 8192 {
            break;
        }
    }
    result
}
fn diagrams(model: &Model) -> Diagrams {
    let mut diagrams = Diagrams::new();
    let mut retained = 0usize;
    for (index, block) in model.blocks.iter().enumerate() {
        if let BlockKind::CodeBlock { language: Some(language), raw } = &block.kind
            && language.eq_ignore_ascii_case("mermaid")
        {
            if diagrams.len() >= 32 {
                break;
            }
            let parsed =
                kwil_core_markdown::diagram::parse(raw).map_err(|error| error.to_string()).and_then(|diagram| {
                    let bytes = diagram.text_bytes();
                    if retained.saturating_add(bytes) > 64 * 1024 {
                        Err("Diagram text exceeds the shared 64 KiB limit".into())
                    } else {
                        retained += bytes;
                        Ok(diagram)
                    }
                });
            diagrams.insert(index, parsed);
        }
    }
    diagrams
}
fn parse(snapshot: &kwil_core_text::Snapshot, stop: &AtomicBool) -> Result<Model, String> {
    use kwil_core_markdown::bounded::{PREVIEW_SOURCE_BYTES, preview_model};
    if snapshot.len() > PREVIEW_SOURCE_BYTES as u64 {
        return Err(
            "Preview unavailable: this document exceeds the 256 KiB preview limit. Text editing remains available."
                .into(),
        );
    }
    let mut bytes = Vec::with_capacity(snapshot.len() as usize);
    for chunk in snapshot.chunks(0..snapshot.len()).map_err(|e| e.to_string())? {
        if stop.load(Ordering::Acquire) {
            return Err("Preview cancelled".into());
        }
        bytes.extend_from_slice(chunk.map_err(|e| e.to_string())?);
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| "Preview unavailable: malformed or opaque text cannot be interpreted as Markdown".to_owned())?;
    preview_model(text, &|| stop.load(Ordering::Acquire)).map_err(|e| format!("Preview unavailable: {e:?}"))
}
impl App {
    // Menus keep the Text escape available after Save As changes the suffix.
    pub(super) fn is_markdown_document(&self) -> bool {
        controls_available(self.markdown_source_eligible(), self.markdown_mode())
    }
    fn markdown_source_eligible(&self) -> bool {
        match self.highlighting.as_ref().and_then(|host| host.detected_language(&self.view)) {
            Some(language) => language == kwil_core_syntax::Language::Markdown,
            None => self.view.editor().is_some_and(|editor| editor.is_markdown()),
        }
    }

    pub(super) fn preview_blocks_command(&mut self, command: &str) -> bool {
        if self.markdown_mode() != Mode::Preview || !command.starts_with("edit.") || command == "edit.copy" {
            return false;
        }
        let damage = self.view.set_notice("Switch to Text or Split to edit".into());
        self.apply(damage);
        true
    }
    #[cfg(feature = "frame-harness")]
    pub(super) fn markdown_probe_click(&mut self, el: &ActiveEventLoop, mode: Mode) -> bool {
        let command = match mode {
            Mode::Text => "view.markdownText",
            Mode::Preview => "view.markdownPreview",
            Mode::Split => "view.markdownSplit",
        };
        let Some(tool) = self.menu.toolbar.iter().find(|tool| tool.command == command) else {
            return false;
        };
        self.pointer = (tool.rect.x + tool.rect.w / 2, tool.rect.y + tool.rect.h / 2);
        self.menu_mouse(el)
    }
    /// The next paint measures preceding blocks without painting or extra IO,
    /// then starts exactly at this source section using the production layout.
    #[cfg(feature = "frame-harness")]
    pub(super) fn markdown_probe_position(&mut self, source_line: u64) {
        self.markdown.probe_target = Some(source_line);
        self.markdown.scroll = 0;
        self.apply(Damage { full: true, ..Default::default() });
    }
    pub(super) fn markdown_mode(&self) -> Mode {
        self.markdown.mode(self.view.active_id())
    }
    pub(super) fn dispatch_markdown(&mut self, command: &str) -> bool {
        let mode = match mode_action(command, self.markdown_mode(), self.markdown_source_eligible()) {
            ModeAction::Unhandled => return false,
            ModeAction::Unavailable => return true,
            ModeAction::Set(mode) => mode,
        };
        self.set_markdown_mode(mode);
        true
    }
    fn set_markdown_mode(&mut self, mode: Mode) {
        let id = self.view.active_id();
        self.markdown.modes.insert(id, mode);
        self.markdown.scroll = 0;
        self.drag = None;
        self.selecting = false;
        self.ime_destination = Some(ime::Destination::Cancelled);
        self.composing = false;
        if let Some(keys) = &mut self.keys {
            keys.cancel_pending();
        }
        let rows = self.text_rows();
        self.view.set_rows(rows);
        self.apply(Damage { full: true, status: true, ..Default::default() });
    }
    pub(super) fn sync_markdown(&mut self) {
        // This precedes even Text-mode desired clearing and path comparison.
        if self.markdown.capture_blocked() {
            return;
        }
        if self.markdown_mode() == Mode::Text {
            self.markdown.desired = None;
            return;
        }
        if self.markdown.registration_failure().is_some() {
            self.markdown.busy = false;
            return;
        }
        let Some(editor) = self.view.editor() else {
            return;
        };
        let key = Key {
            document: self.view.active_id(),
            revision: editor.document().revision(),
            epoch: self.view.input_epoch(),
            base: self.markdown.path_generation,
        };
        if self.markdown.lane.is_none() {
            let Some(terminal) = self.markdown.terminal.clone() else {
                self.markdown.error = Some("Markdown shutdown metadata was not admitted before startup".into());
                return;
            };
            let (tx, rx) = mpsc::sync_channel::<Work>(1);
            let proxy = self.proxy.clone();
            let stop = self.markdown.stop.clone();
            match std::thread::Builder::new().name("kwil-markdown".into()).spawn(move || {
                let exit = ExitGuard(terminal);
                // This inner scope destroys the receiver and every queued or
                // active Snapshot before the terminal acknowledgement.
                {
                    let rx = rx;
                    if exit.0.native.register_current().is_err() {
                        // Exact failed/partial registration stays in Terminal;
                        // a racing admitted source stays on this SAME receiver.
                        // Drain without parsing until the normal sender close.
                        while let Ok(work) = rx.recv() {
                            drop(work);
                        }
                        return;
                    }
                    while let Ok(work) = rx.recv() {
                        if stop.load(Ordering::Acquire) {
                            break;
                        }
                        let result = parse(&work.snapshot, &stop);
                        let images = result.as_ref().map_or_else(
                            |_| Default::default(),
                            |model| {
                                markdown_images::load_images(
                                    model,
                                    work.path.as_ref().map(|path| path.value.as_path()),
                                    &stop,
                                )
                            },
                        );
                        let diagrams = result.as_ref().map(diagrams).unwrap_or_default();
                        let highlights = result.as_ref().map(|model| code_highlights(model, &stop)).unwrap_or_default();
                        // Snapshot and its file/spill leases retire on this worker.
                        drop(work.snapshot);
                        if proxy
                            .send_computed_value(
                                UserEvent::MarkdownReady(Box::new(Completion {
                                    key: work.key,
                                    result,
                                    images,
                                    diagrams,
                                    highlights,
                                })),
                                drop,
                            )
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }) {
                Ok(thread) => {
                    self.markdown.lane = Some(Lane { tx: Some(tx), thread: Some(thread), retirement_pending: false })
                }
                Err(error) => {
                    self.markdown.error = Some(error.to_string());
                    return;
                }
            }
        }
        if self.markdown.registration_failure().is_some() {
            self.markdown.busy = false;
            return;
        }
        self.markdown.submit_with(key, editor.document().path(), || editor.document().snapshot());
    }
    pub(super) fn markdown_ready(&mut self, completion: Completion) {
        self.markdown.receive(completion);
        self.apply(Damage { full: true, ..Default::default() });
    }
    pub(super) fn markdown_pointer(&mut self) -> bool {
        let eligible =
            self.view.editor().is_some_and(|editor| editor.is_markdown()) || self.markdown_mode() != Mode::Text;
        if !eligible {
            return false;
        }
        preview_area(self).is_some_and(|area| area.contains(self.pointer))
    }
    pub(super) fn markdown_scroll_key(&mut self, delta: i64) {
        self.markdown.scroll = self.markdown.scroll.saturating_add_signed(delta as isize).min(1_048_576);
        self.apply(Damage { full: true, ..Default::default() });
    }
    pub(super) fn markdown_wheel(&mut self, delta: i64) -> bool {
        if self.markdown_mode() == Mode::Text || !preview_area(self).is_some_and(|area| area.contains(self.pointer)) {
            return false;
        }
        self.markdown.scroll = self.markdown.scroll.saturating_add_signed(delta as isize).min(1_048_576);
        self.apply(Damage { full: true, ..Default::default() });
        true
    }
}
pub(super) fn preview_area(app: &App) -> Option<Rect> {
    let body = chrome::layout(app.size, &app.ui, app.settings.tabs.layout).body;
    let mut rect = tabs::rect(body);
    match app.markdown_mode() {
        Mode::Text => None,
        Mode::Preview => Some(rect),
        Mode::Split => {
            let half = rect.w / 2;
            rect.x += half;
            rect.w -= half;
            Some(rect)
        }
    }
}
/// Row coordinates are always body-line units, including headings and images.
/// Signed placement clips partially scrolled blocks without changing their layout.
struct Flow<'a> {
    pixels: &'a mut [u32],
    stride: usize,
    clip: Rect,
    font: &'a mut font::UiFont,
    theme: &'a kwil_core_commands::theme::Theme,
    images: &'a markdown_images::Images,
    x: usize,
    left: usize,
    right: usize,
    origin: usize,
    line: usize,
    skip: usize,
    height: usize,
    step: usize,
    heading: bool,
    bold: bool,
    paint: bool,
    fence: bool,
    background_line: Option<usize>,
    foreground: Option<u32>,
    quote: usize,
    quote_left: usize,
    unit: usize,
    after_cr: bool,
}
impl Flow<'_> {
    fn y(&self) -> i64 {
        self.origin as i64 + (self.line as i64 - self.skip as i64) * self.height as i64
    }
    fn next(&mut self) {
        self.advance(self.step);
    }
    fn advance(&mut self, rows: usize) {
        self.line += rows;
        self.x = self.left;
    }
    fn visible(&self) -> bool {
        self.paint
            && self.y() < (self.clip.y + self.clip.h) as i64
            && self.y() + (self.height * self.step) as i64 > self.clip.y as i64
    }
    fn done(&self) -> bool {
        self.paint && self.y() >= (self.clip.y + self.clip.h) as i64
    }
    fn fill(&mut self, x: usize, y: i64, width: usize, height: usize, color: u32) {
        if !self.paint {
            return;
        }
        let top = y.max(self.clip.y as i64);
        let bottom = (y + height as i64).min((self.clip.y + self.clip.h) as i64);
        let left = x.max(self.clip.x);
        let right = x.saturating_add(width).min(self.clip.x + self.clip.w);
        if bottom > top && right > left {
            chrome::fill_clipped(
                self.pixels,
                self.stride,
                Rect { x: left, y: top as usize, w: right - left, h: (bottom - top) as usize },
                color,
            );
        }
    }
    fn role(&self, role: kwil_core_commands::theme::UiRole) -> u32 {
        theme_ui::rgb(self.theme, role)
    }
    fn row(&mut self) {
        if !self.visible() || self.background_line == Some(self.line) {
            return;
        }
        self.background_line = Some(self.line);
        if self.fence {
            self.fill(
                self.left.saturating_sub(self.unit),
                self.y(),
                self.right - self.left + 2 * self.unit,
                self.height * self.step,
                self.role(kwil_core_commands::theme::UiRole::TabBackground),
            );
        }
        for depth in 0..self.quote {
            self.fill(
                self.quote_left + depth * self.unit * 2,
                self.y(),
                (self.unit / 4).max(1),
                self.height * self.step,
                self.role(kwil_core_commands::theme::UiRole::Accent),
            );
        }
    }
    fn width(&mut self, text: &str, code: bool) -> usize {
        let face = if code {
            self.font.monospace()
        } else if self.heading {
            self.font.heading()
        } else {
            &mut *self.font
        };
        face.width(text)
    }
    fn text(&mut self, text: &str, style: &kwil_core_markdown::Style, code: bool) {
        use unicode_segmentation::UnicodeSegmentation;
        // Each shaping operation stays below UiFont's scalar limit. Graphemes
        // are admitted on the worker and never split by long-word wrapping.
        for word in text.split_inclusive(char::is_whitespace) {
            let mut piece = String::new();
            let mut scalars = 0;
            for cluster in word.graphemes(true) {
                if self.done() {
                    return;
                }
                if cluster == "\n" && self.after_cr {
                    self.after_cr = false;
                    continue;
                }
                self.after_cr = cluster == "\r";
                if cluster == "\n" || cluster == "\r\n" || cluster == "\r" {
                    if !piece.is_empty() {
                        self.run(&piece, style, code);
                        piece.clear();
                    }
                    scalars = 0;
                    self.row();
                    self.next();
                } else if cluster == "\t" && code {
                    if !piece.is_empty() {
                        self.run(&piece, style, code);
                        piece.clear();
                    }
                    scalars = 0;
                    let cell = self.width(" ", true).max(1);
                    let column = (self.x - self.left) / cell;
                    for _ in 0..4 - column % 4 {
                        self.run(" ", style, true);
                    }
                } else {
                    let count = cluster.chars().count();
                    if scalars + count > 64 && !piece.is_empty() {
                        self.run(&piece, style, code);
                        piece.clear();
                        scalars = 0;
                    }
                    piece.push_str(cluster);
                    scalars += count;
                }
            }
            if !piece.is_empty() {
                self.run(&piece, style, code);
            }
        }
    }
    fn run(&mut self, text: &str, style: &kwil_core_markdown::Style, code: bool) {
        use unicode_segmentation::UnicodeSegmentation;
        if self.done() {
            return;
        }
        let width = self.width(text, code);
        let available = self.right.saturating_sub(self.left);
        if width > available && text.graphemes(true).take(2).count() > 1 {
            for cluster in text.graphemes(true) {
                self.run(cluster, style, code);
            }
            return;
        }
        if self.x.saturating_add(width) > self.right && self.x > self.left {
            self.next();
        }
        // Prose wrap collapses leading whitespace; code retains every space.
        if !code && self.x == self.left && text.chars().all(char::is_whitespace) {
            return;
        }
        if self.visible() {
            self.row();
            let fg = self.foreground.unwrap_or_else(|| {
                self.role(if style.link.is_some() {
                    kwil_core_commands::theme::UiRole::Accent
                } else {
                    kwil_core_commands::theme::UiRole::PanelForeground
                })
            });
            if code && !self.fence {
                self.fill(
                    self.x,
                    self.y(),
                    width,
                    self.height,
                    self.role(kwil_core_commands::theme::UiRole::TabBackground),
                );
            }
            let y = self.y();
            let face = if code {
                self.font.monospace()
            } else if self.heading {
                self.font.heading()
            } else {
                &mut *self.font
            };
            // baseline() is unsigned, but partially visible rows need a signed top.
            let baseline = y + face.baseline(0, self.height * self.step) as i64;
            if baseline >= 0 {
                let clip = Rect {
                    x: self.left.max(self.clip.x),
                    y: self.clip.y,
                    w: self.right.min(self.clip.x + self.clip.w).saturating_sub(self.left.max(self.clip.x)),
                    h: self.clip.h,
                };
                face.draw_styled(self.pixels, self.stride, clip, self.x, baseline as usize, text, fg, style.emphasis);
                if style.strong || self.bold {
                    face.draw_styled(
                        self.pixels,
                        self.stride,
                        clip,
                        self.x + 1,
                        baseline as usize,
                        text,
                        fg,
                        style.emphasis,
                    );
                }
                if style.strikethrough {
                    self.fill(self.x, y + (self.height * self.step / 2) as i64, width, 1, fg);
                }
                if style.link.is_some() {
                    self.fill(self.x, baseline + 2, width, 1, fg);
                }
            }
        }
        self.x += width;
    }
    fn image(&mut self, alt: &str, url: &str) {
        let Some(Ok(image)) = self.images.get(url) else {
            let error =
                self.images.get(url).and_then(|r| r.as_ref().err()).map_or("Image limit reached", String::as_str);
            self.text(&format!("Image unavailable ({url}): {alt}. {error}"), &Default::default(), false);
            return;
        };
        if self.x != self.left {
            self.next();
        }
        let (iw, ih) = (image.width as usize, image.height as usize);
        let width = iw.min(self.right.saturating_sub(self.left)).max(1);
        let height = (ih * width / iw).max(1);
        let y = self.y();
        if self.paint {
            let from_y = (self.clip.y as i64 - y).max(0) as usize;
            let to_y = height.min(((self.clip.y + self.clip.h) as i64 - y).max(0) as usize);
            for dy in from_y..to_y {
                let sy = dy * ih / height;
                for dx in 0..width.min(self.clip.x + self.clip.w - self.left) {
                    let sx = dx * iw / width;
                    let from = (sy * iw + sx) * 4;
                    let rgba = &image.rgba[from..from + 4];
                    let color = (u32::from(rgba[0]) << 16) | (u32::from(rgba[1]) << 8) | u32::from(rgba[2]);
                    let target = &mut self.pixels[(y + dy as i64) as usize * self.stride + self.left + dx];
                    *target = font::blend(*target, color, rgba[3]);
                }
            }
        }
        self.advance(height.div_ceil(self.height));
    }
    fn spans(&mut self, spans: &[Span]) {
        for span in spans {
            if self.done() {
                break;
            }
            match &span.kind {
                InlineKind::Text(text) => self.text(text, &span.style, false),
                InlineKind::Code(text) => self.text(text, &span.style, true),
                InlineKind::SoftBreak => self.text(" ", &span.style, false),
                InlineKind::HardBreak => self.next(),
                InlineKind::Image { alt, url } => self.image(alt, url),
            }
        }
    }
    fn task_marker(&mut self, checked: bool) {
        self.row();
        let side = (2 * self.unit).min(self.height.saturating_sub(self.unit / 2)).max(4);
        let thickness = (self.unit / 6).max(1);
        let (x, y) = (self.x, self.y() + ((self.height - side) / 2) as i64);
        let accent = self.role(kwil_core_commands::theme::UiRole::Accent);
        let fg = self.role(kwil_core_commands::theme::UiRole::PanelForeground);
        if checked {
            self.fill(x, y, side, side, accent);
            let color = self.role(kwil_core_commands::theme::UiRole::PanelBackground);
            let (a, b) = (side / 4, side / 2);
            for offset in 0..side / 4 {
                self.fill(x + a + offset, y + (b + offset) as i64, thickness, thickness, color);
            }
            for offset in 0..side / 2 {
                self.fill(
                    x + a + side / 4 + offset,
                    y + (b + side / 4) as i64 - offset as i64,
                    thickness,
                    thickness,
                    color,
                );
            }
        } else {
            self.fill(x, y, side, thickness, fg);
            self.fill(x, y + (side - thickness) as i64, side, thickness, fg);
            self.fill(x, y, thickness, side, fg);
            self.fill(x + side - thickness, y, thickness, side, fg);
        }
        self.x += side + self.unit;
    }
    fn cell(&mut self, spans: &[Span], left: usize, right: usize, paint: bool, bold: bool) -> (usize, usize) {
        let mut cell = Flow {
            pixels: self.pixels,
            stride: self.stride,
            clip: self.clip,
            font: self.font,
            theme: self.theme,
            images: self.images,
            x: left,
            left,
            right,
            origin: self.origin,
            line: self.line,
            skip: self.skip,
            height: self.height,
            step: 1,
            heading: false,
            bold,
            paint,
            fence: false,
            background_line: None,
            foreground: None,
            quote: 0,
            quote_left: left,
            unit: self.unit,
            after_cr: false,
        };
        cell.spans(spans);
        (cell.line - self.line + 1, cell.x.saturating_sub(left))
    }
    fn table(&mut self, alignments: &[kwil_core_markdown::Alignment], rows: &[Vec<Vec<Span>>]) {
        use kwil_core_markdown::Alignment;
        let columns = alignments.len().max(1);
        let available = self.right.saturating_sub(self.left);
        let inset = self.unit;
        if available / columns < 6 * self.unit {
            self.text("Table shown as stacked cells to fit this pane", &Default::default(), false);
            self.next();
            for row in rows {
                for (index, cell) in row.iter().enumerate() {
                    self.text(&format!("{}. ", index + 1), &Default::default(), false);
                    self.spans(cell);
                    self.next();
                    if self.done() {
                        return;
                    }
                }
                self.next();
            }
            return;
        }
        let left = self.left;
        let border = self.role(kwil_core_commands::theme::UiRole::Border);
        for (row_index, row) in rows.iter().enumerate() {
            if self.done() {
                break;
            }
            let mut row_lines = 1;
            let mut metrics = Vec::with_capacity(row.len().min(columns));
            for (index, cell) in row.iter().enumerate().take(columns) {
                let a = left + index * available / columns + inset;
                let b = left + (index + 1) * available / columns - inset;
                let measured = self.cell(cell, a, b, false, row_index == 0);
                row_lines = row_lines.max(measured.0);
                metrics.push(measured);
            }
            let row_height = (row_lines + 1) * self.height;
            let y = self.y();
            self.fill(
                left,
                y,
                available,
                row_height,
                self.role(if row_index % 2 == 0 {
                    kwil_core_commands::theme::UiRole::TabBackground
                } else {
                    kwil_core_commands::theme::UiRole::PanelBackground
                }),
            );
            for (index, cell) in row.iter().enumerate().take(columns) {
                let a = left + index * available / columns + inset;
                let b = left + (index + 1) * available / columns - inset;
                // Use the exact run advances from layout, including rounding at
                // style/word boundaries. Measuring a whole span differs by a
                // pixel and can spuriously wrap a right-aligned cell.
                let width = if metrics[index].0 == 1 { metrics[index].1.min(b - a) } else { b - a };
                let offset = match alignments.get(index) {
                    Some(Alignment::Center) => (b - a - width) / 2,
                    Some(Alignment::Right) => b - a - width,
                    _ => 0,
                };
                self.cell(cell, a + offset, b, self.paint, row_index == 0);
            }
            // Text decorations and inline-code backgrounds stay inside the
            // grid. Paint shared borders last so they cannot erase a rule.
            self.fill(left, y, available, 1, border);
            for index in 0..=columns {
                self.fill(left + index * available / columns, y, 1, row_height, border);
            }
            self.advance(row_lines + 1);
            self.fill(left, self.y(), available, 1, border);
        }
    }
}
#[allow(clippy::too_many_arguments)]
pub(super) fn draw(
    host: &mut Host,
    mode: Mode,
    eligible: bool,
    top: u64,
    pixels: &mut [u32],
    size: (usize, usize),
    area: Rect,
    font: &mut font::UiFont,
    ui: &chrome::Metrics,
    theme: &kwil_core_commands::theme::Theme,
) -> Option<Rect> {
    if mode == Mode::Text {
        return None;
    }
    let _ = eligible;
    let bg = theme_ui::rgb(theme, kwil_core_commands::theme::UiRole::PanelBackground);
    let mut pane = area;
    if mode == Mode::Split {
        let half = pane.w / 2;
        pane.x += half;
        pane.w -= half;
    }
    if pane.w == 0 || pane.h == 0 {
        return Some(area);
    }
    chrome::fill_clipped(pixels, size.0, pane, bg);
    let pad = ui.dip(24.0).min(pane.w / 8);
    let mut flow = Flow {
        pixels,
        stride: size.0,
        clip: pane,
        font,
        theme,
        images: &host.images,
        x: pane.x + pad,
        left: pane.x + pad,
        right: pane.x + pane.w.saturating_sub(pad),
        origin: pane.y + pad,
        line: 0,
        skip: host.scroll,
        height: ui.dip(25.0).max(1),
        step: 1,
        heading: false,
        bold: false,
        paint: true,
        fence: false,
        background_line: None,
        foreground: None,
        quote: 0,
        quote_left: pane.x + pad,
        unit: ui.dip(8.0).max(1),
        after_cr: false,
    };
    #[cfg(feature = "frame-harness")]
    {
        if host.probe_target.is_some() {
            flow.paint = false;
        }
    }
    if host.rejected.is_some() || host.admission_notice.is_some() {
        // A latched notice belongs to the pane, independent of model scrolling.
        flow.skip = 0;
        flow.text(host.notice().unwrap(), &Default::default(), false);
    } else if let Some(model) = &host.model {
        let anchor = if mode == Mode::Split { top as usize } else { 0 };
        for (index, block) in model.blocks.iter().enumerate() {
            if block.source_lines.end <= anchor {
                continue;
            }
            if flow.done() {
                break;
            }
            #[cfg(feature = "frame-harness")]
            if host.probe_target.is_some_and(|target| block.source_lines.end as u64 > target) {
                host.scroll = flow.line;
                flow.skip = flow.line;
                host.probe_target = None;
                flow.paint = true;
            }
            flow.heading = matches!(block.kind, BlockKind::Heading(1 | 2));
            flow.step = if flow.heading { 2 } else { 1 };
            flow.bold = matches!(block.kind, BlockKind::Heading(1 | 3..=6));
            flow.quote = block.quote_depth.min(8);
            flow.left = pane.x + pad + (flow.quote + block.list_depth.saturating_sub(1)).min(12) * 2 * flow.unit;
            flow.left = flow.left.min(flow.right.saturating_sub(flow.unit));
            flow.x = flow.left;
            flow.background_line = None;
            match &block.kind {
                BlockKind::Heading(_) | BlockKind::Paragraph => flow.spans(&block.spans),
                BlockKind::ListItem { marker, task } => {
                    if let Some(checked) = task {
                        flow.task_marker(*checked);
                    } else {
                        let mark = match marker {
                            ListMarker::Bullet(_) => "• ".into(),
                            ListMarker::Ordered { number, .. } => format!("{number}. "),
                        };
                        flow.text(&mark, &Default::default(), false);
                    }
                    flow.left = flow.x; // hanging indent survives every wrapped line
                    flow.spans(&block.spans);
                }
                BlockKind::CodeBlock { language, raw } => {
                    let mut drawn = false;
                    if language.as_deref().is_some_and(|s| s.eq_ignore_ascii_case("mermaid")) {
                        let width = flow.right.saturating_sub(flow.left);
                        let key = (width, flow.font.id(), ui.scale.to_bits());
                        if host.layouts.get(&index).is_none_or(|(old, _)| *old != key) {
                            let layout = match host.diagrams.get(&index) {
                                Some(Ok(diagram)) => {
                                    markdown_diagram::prepare(diagram, width, flow.font, ui).map_err(|e| e.to_string())
                                }
                                Some(Err(error)) => Err(error.clone()),
                                None => Err("Diagram count exceeds the bounded preview limit".into()),
                            };
                            host.layouts.insert(index, (key, layout));
                        }
                        match &host.layouts[&index].1 {
                            Ok(layout) => {
                                if flow.paint {
                                    let y = flow.y();
                                    let _ = markdown_diagram::draw(
                                        layout,
                                        flow.pixels,
                                        flow.stride,
                                        (flow.left as i64, y),
                                        pane,
                                        flow.font,
                                        theme,
                                    );
                                }
                                flow.advance(layout.height().div_ceil(flow.height));
                                drawn = true;
                            }
                            Err(error) => {
                                flow.text(&markdown_diagram::notice(error), &Default::default(), false);
                                flow.next();
                            }
                        }
                    }
                    if !drawn {
                        flow.fence = true;
                        flow.row();
                        flow.text(language.as_deref().unwrap_or("code"), &Default::default(), false);
                        flow.next();
                        let mut at = 0;
                        for span in host.highlights.get(&index).into_iter().flatten() {
                            if span.range.start < at
                                || span.range.end > raw.len()
                                || !raw.is_char_boundary(span.range.start)
                                || !raw.is_char_boundary(span.range.end)
                            {
                                continue;
                            }
                            flow.foreground = None;
                            flow.text(&raw[at..span.range.start], &Default::default(), true);
                            flow.foreground = theme
                                .style(span.class.name())
                                .foreground
                                .map(|c| (u32::from(c.0) << 16) | (u32::from(c.1) << 8) | u32::from(c.2));
                            flow.text(&raw[span.range.clone()], &Default::default(), true);
                            at = span.range.end;
                            if flow.done() {
                                break;
                            }
                        }
                        flow.foreground = None;
                        flow.text(&raw[at..], &Default::default(), true);
                        flow.row();
                        flow.fence = false;
                    }
                }
                BlockKind::Table { rows, alignments } => flow.table(alignments, rows),
                BlockKind::ThematicBreak => flow.fill(
                    flow.left,
                    flow.y() + (flow.height / 2) as i64,
                    flow.right - flow.left,
                    1,
                    flow.role(kwil_core_commands::theme::UiRole::Border),
                ),
                BlockKind::Image { alt, url } => flow.image(alt, url),
                BlockKind::Html(raw) => {
                    flow.text("HTML is displayed as inert source", &Default::default(), false);
                    flow.next();
                    flow.text(raw, &Default::default(), true);
                }
            }
            // Lists use one row per item; paragraphs have one breathing row.
            let compact = matches!(block.kind, BlockKind::ListItem { .. });
            flow.next();
            flow.step = 1;
            if !compact {
                flow.next();
            }
        }
        if !flow.done() {
            let visible = pane.h.saturating_sub(pad) / flow.height;
            let last = flow.line.saturating_sub(visible.saturating_sub(1));
            if host.scroll > last {
                host.scroll = last;
                return draw(host, mode, eligible, top, pixels, size, area, font, ui, theme);
            }
        }
    } else {
        flow.text(host.notice().unwrap_or("Preparing Markdown preview…"), &Default::default(), false);
    }
    Some(area)
}
#[cfg(test)]
mod tests {
    use super::*;
    use kwil_view_model::visual_rows::{AllocationBudget, MetadataReservation};
    use std::{
        cell::Cell,
        path::Path,
        time::{Duration, Instant},
    };

    fn request_key() -> Key {
        Key { document: kwil_view_model::SessionDocumentId(71), revision: 1, epoch: 1, base: 0 }
    }
    // A real channel, held without a consumer, makes Full independent of
    // scheduling. Disconnected is established before any source capture.
    fn request_lane(host: &mut Host, full: bool) -> Option<mpsc::Receiver<Work>> {
        let (tx, rx) = mpsc::sync_channel(1);
        if full {
            assert!(tx.try_send(Work { key: request_key(), snapshot: Default::default(), path: None }).is_ok());
        }
        host.lane = Some(Lane { tx: Some(tx), thread: None, retirement_pending: false });
        if full {
            Some(rx)
        } else {
            drop(rx);
            None
        }
    }
    fn rejected_address(host: &Host) -> *const Work {
        &host.rejected.as_ref().expect("the exact failed request must remain owned").work
    }
    fn empty_completion(key: Key) -> Completion {
        Completion {
            key,
            result: Err("late result".into()),
            images: Default::default(),
            diagrams: Default::default(),
            highlights: Default::default(),
        }
    }
    fn decoded_source()
    -> (kwil_core_text::decoded_backing::DecodedBacking, kwil_core_text::Snapshot, std::path::PathBuf) {
        // Only public source APIs; no protected lifetime/allocator seam needed.
        // All fixture storage creation and original-buffer destruction are OFF UI.
        std::thread::spawn(|| {
            use kwil_core_text::{
                Buffer,
                decoded_backing::{DecodedBacking, Interpretation},
                encoding::Encoding,
            };
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "kwil-markdown-rejected-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&root).unwrap();
            let source = b"# retained source\n";
            let backing = DecodedBacking::prepare_from_reader(
                &mut &source[..],
                source.len() as u64,
                Interpretation::new(Encoding::UTF8, 0).unwrap(),
                &root,
                &AtomicBool::new(false),
            )
            .unwrap();
            let buffer = Buffer::from_decoded(backing.snapshot());
            let snapshot = buffer.snapshot();
            drop(buffer);
            (backing, snapshot, root)
        })
        .join()
        .unwrap()
    }
    #[test]
    fn full_and_disconnected_keep_exact_source_and_path_credit_until_off_ui_settlement() {
        use kwil_core_text::decoded_backing::RetirementError;
        for full in [true, false] {
            let (backing, snapshot, root) = decoded_source();
            let budget = AllocationBudget::default();
            let baseline = budget.retained_bytes();
            let mut host = std::mem::ManuallyDrop::new(Host::with_budget(&budget).unwrap());
            let fixed = budget.retained_bytes();
            let rx = request_lane(&mut host, full);
            let mut source = Some(snapshot);
            let captures = Cell::new(0);
            let path = Path::new("retained-preview.md");
            host.submit_with(request_key(), Some(path), || {
                captures.set(captures.get() + 1);
                source.take().unwrap()
            });
            let rejected = host.rejected.as_ref().expect("base defect loses this source owner");
            assert_eq!(rejected.reason, if full { Rejection::Full } else { Rejection::Disconnected });
            assert_eq!(rejected.work.key, host.desired.unwrap());
            assert_eq!(rejected.work.snapshot.len(), b"# retained source\n".len() as u64);
            let request_path = rejected.work.path.as_ref().unwrap();
            let request_credit = request_path._charge.reserved_bytes();
            let cache_credit = host.base_path.as_ref().unwrap()._charge.reserved_bytes();
            assert!(request_path.value.capacity() + std::mem::size_of::<MetadataReservation>() <= request_credit);
            assert_eq!(request_path.value, path);
            assert_eq!(budget.retained_bytes(), fixed + request_credit + cache_credit);
            let address = rejected_address(&host);
            let path_address = request_path.value.as_os_str().as_encoded_bytes().as_ptr();
            let desired = host.desired;
            let generation = host.path_generation;
            let charged = budget.retained_bytes();
            for key in [
                Key { revision: 2, ..request_key() },
                Key { document: kwil_view_model::SessionDocumentId(72), epoch: 3, ..request_key() },
            ] {
                host.submit_with(key, Some(Path::new("different/save-as.md")), || {
                    captures.set(captures.get() + 1);
                    panic!("occupied slot must gate capture")
                });
                host.receive(empty_completion(key));
            }
            host.receive(empty_completion(desired.unwrap()));
            assert_eq!(captures.get(), 1);
            assert_eq!(host.desired, desired);
            assert_eq!(host.path_generation, generation);
            assert_eq!(rejected_address(&host), address);
            assert_eq!(
                host.rejected
                    .as_ref()
                    .unwrap()
                    .work
                    .path
                    .as_ref()
                    .unwrap()
                    .value
                    .as_os_str()
                    .as_encoded_bytes()
                    .as_ptr(),
                path_address
            );
            assert_eq!(
                host.notice(),
                Some(if full { Rejection::Full.notice() } else { Rejection::Disconnected.notice() })
            );
            assert_eq!(budget.retained_bytes(), charged);
            let host = std::mem::ManuallyDrop::into_inner(host);
            let observer = budget.clone();
            let (pinned, settled, after_source, final_bytes, retired) = std::thread::spawn(move || {
                let backing = match backing.try_retire() {
                    Err(RetirementError::LiveSnapshots(owner)) => owner,
                    _ => panic!("rejected request must still pin the original backing"),
                };
                // Drains only the fixture's prefilled empty request, OFF UI.
                drop(rx);
                let mut host = host;
                let settled = host.settle_until(Instant::now() + Duration::from_secs(5));
                let after_source = observer.retained_bytes();
                let retired = backing.try_retire().is_ok();
                drop(host);
                let final_bytes = observer.retained_bytes();
                std::fs::remove_dir(root).unwrap();
                (true, settled, after_source, final_bytes, retired)
            })
            .join()
            .unwrap();
            assert!(pinned);
            assert_eq!(settled, Ok(()));
            assert_eq!(after_source, fixed + cache_credit, "only retired request capacity releases its credit");
            assert_eq!(final_bytes, baseline);
            assert!(retired, "source retirement precedes host/header credit release");
        }
    }
    #[test]
    fn rejected_app_sync_preserves_latch_even_in_text_mode() {
        let (app, events) = crate::search_ui::host_dispatch_tests::host("original");
        let mut app = std::mem::ManuallyDrop::new(app);
        request_lane(&mut app.markdown, false);
        app.markdown.submit_with(request_key(), None, Default::default);
        let original = app.markdown.desired;
        let address = rejected_address(&app.markdown);
        let notice = app.markdown.notice().unwrap().to_owned();
        for mode in [Mode::Text, Mode::Split, Mode::Preview, Mode::Text] {
            let document = app.view.active_id();
            app.markdown.modes.insert(document, mode);
            app.sync_markdown();
            app.markdown.receive(empty_completion(original.unwrap()));
            assert_eq!(app.markdown.desired, original, "gate precedes Text-mode desired clearing");
            assert_eq!(rejected_address(&app.markdown), address);
            assert_eq!(app.markdown.notice(), Some(notice.as_str()));
        }
        let App {
            document: mut failure,
            view,
            wrap,
            markdown,
            logs,
            highlighting,
            search_panel,
            search,
            external_inbox,
            external_open,
            ..
        } = std::mem::ManuallyDrop::into_inner(app);
        failure.attach(shutdown::Owners {
            view,
            wrap,
            markdown,
            logs,
            highlighting,
            search_panel,
            search,
            external_inbox,
            external_open,
        });
        std::thread::spawn(move || {
            assert_eq!(
                failure.markdown.as_mut().unwrap().settle_until(Instant::now() + Duration::from_secs(5)),
                Ok(())
            );
            drop(failure);
        })
        .join()
        .unwrap();
        drop(events);
    }
    #[test]
    fn both_path_admissions_precede_capture_and_refusal_preserves_old_cache() {
        const LIMIT: usize = 64 * 1024;
        let budget = AllocationBudget::new(LIMIT);
        let baseline = budget.retained_bytes();
        let mut host = std::mem::ManuallyDrop::new(Host::with_budget(&budget).unwrap());
        let (tx, rx) = mpsc::sync_channel(1);
        host.lane = Some(Lane { tx: Some(tx), thread: None, retirement_pending: false });
        host.submit_with(request_key(), Some(Path::new("old.md")), Default::default);
        let first = rx.try_recv().expect("first request accepted");
        host.busy = false;
        host.current = host.desired;
        let old_key = host.desired;
        let old_generation = host.path_generation;
        let old_address = host.base_path.as_ref().unwrap().value.as_os_str().as_encoded_bytes().as_ptr();
        let target = Path::new("longer/new-preview-path.md");
        let one_copy = target.as_os_str().len() * 2 + std::mem::size_of::<MetadataReservation>();
        // One candidate fits; the second does not. The old cache AND accepted
        // request still own their allocations during this attempted growth.
        let filler = budget
            .reserve_metadata(
                LIMIT - budget.retained_bytes() - (2 * one_copy - 1) - std::mem::size_of::<MetadataReservation>(),
            )
            .unwrap();
        let charged = budget.retained_bytes();
        let captures = Cell::new(0);
        host.submit_with(Key { revision: 2, ..request_key() }, Some(target), || {
            captures.set(captures.get() + 1);
            Default::default()
        });
        assert_eq!(captures.get(), 0);
        assert_eq!(host.desired, old_key);
        assert_eq!(host.path_generation, old_generation);
        assert_eq!(host.base_path.as_ref().unwrap().value.as_os_str().as_encoded_bytes().as_ptr(), old_address);
        assert_eq!(budget.retained_bytes(), charged, "failed candidates return their credit");
        assert!(budget.peak_bytes() >= charged + one_copy, "first candidate was live when second admission refused");
        assert!(rx.try_recv().is_err());
        drop(filler);
        host.submit_with(Key { revision: 2, ..request_key() }, Some(target), || {
            captures.set(captures.get() + 1);
            Default::default()
        });
        let second = rx.try_recv().expect("retry after capacity release is accepted");
        assert_eq!(captures.get(), 1);
        assert_eq!(host.path_generation, old_generation + 1);
        assert_eq!(host.base_path.as_ref().unwrap().value, target);
        let request_path = second.path.as_ref().unwrap();
        assert!(
            request_path.value.capacity() + std::mem::size_of::<MetadataReservation>()
                <= request_path._charge.reserved_bytes()
        );
        let host = std::mem::ManuallyDrop::into_inner(host);
        std::thread::spawn(move || drop((first, second, rx, host))).join().unwrap();
        assert_eq!(budget.retained_bytes(), baseline);
    }
    #[test]
    fn fixed_header_admission_and_aligned_arc_layout_are_not_source_admission() {
        #[repr(align(64))]
        struct Aligned;
        assert_eq!(arc_allocation_bytes::<Aligned>(), Some(64));
        let bytes = Host::metadata_bytes().unwrap() + std::mem::size_of::<MetadataReservation>();
        let probe = AllocationBudget::new(64 * 1024);
        let baseline = probe.retained_bytes();
        let budget = AllocationBudget::new(baseline + bytes - 1);
        assert!(Host::with_budget(&budget).is_err());
        assert_eq!(budget.retained_bytes(), baseline, "refusal occurs before Terminal/stop allocations");
    }
    #[test]
    fn no_lane_timeout_retains_owner_and_original_deadline_on_retry() {
        let budget = AllocationBudget::default();
        let mut host = std::mem::ManuallyDrop::new(Host::with_budget(&budget).unwrap());
        request_lane(&mut host, false);
        host.submit_with(request_key(), Some(Path::new("retained.md")), Default::default);
        drop(host.lane.take());
        let address = rejected_address(&host);
        let charged = budget.retained_bytes();
        let deadline = Instant::now();
        assert_eq!(host.settle_until(deadline), Err(SettlementReason::TimedOut));
        assert_eq!(host.settle_until(deadline + Duration::from_secs(5)), Err(SettlementReason::TimedOut));
        assert_eq!(host.deadline, Some(deadline));
        assert_eq!(rejected_address(&host), address);
        assert_eq!(budget.retained_bytes(), charged);
        let host = std::mem::ManuallyDrop::into_inner(host);
        // Explicit test cleanup of a failed owner, never a renewed settlement.
        std::thread::spawn(move || drop(host)).join().unwrap();
    }
    #[test]
    fn no_lane_success_and_destructor_overrun_use_the_original_deadline() {
        for overrun in [false, true] {
            let budget = AllocationBudget::default();
            let mut host = Host::with_budget(&budget).unwrap();
            request_lane(&mut host, false);
            host.submit_with(request_key(), Some(Path::new("retained.md")), Default::default);
            drop(host.lane.take());
            let charged = budget.retained_bytes();
            let request_credit = host.rejected.as_ref().unwrap().work.path.as_ref().unwrap()._charge.reserved_bytes();
            let observer = budget.clone();
            let (result, bytes, empty, stored) = std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                let result = if overrun {
                    host.deadline = Some(deadline);
                    let calls = Cell::new(0);
                    host.retire_rejected_with(deadline, || {
                        let before = calls.get();
                        calls.set(before + 1);
                        if before == 0 { deadline - Duration::from_millis(1) } else { deadline }
                    })
                } else {
                    host.settle_until(deadline)
                };
                (result, observer.retained_bytes(), host.rejected.is_none(), host.deadline)
            })
            .join()
            .unwrap();
            assert_eq!(result, if overrun { Err(SettlementReason::TimedOut) } else { Ok(()) });
            assert_eq!(bytes, charged - request_credit);
            assert!(empty, "an overrun cannot reconstruct an already destroyed request");
            assert!(stored.is_some());
        }
    }
    #[test]
    fn pending_query_and_retirement_failures_quarantine_rejection_before_sticky_panic() {
        for panic_worker in [false, true] {
            let budget = AllocationBudget::default();
            let mut host = std::mem::ManuallyDrop::new(Host::with_budget(&budget).unwrap());
            request_lane(&mut host, false);
            host.submit_with(request_key(), Some(Path::new("native-pending.md")), Default::default);
            let request_credit = host.rejected.as_ref().unwrap().work.path.as_ref().unwrap()._charge.reserved_bytes();
            let charged = budget.retained_bytes();
            let deadline = Instant::now() + Duration::from_secs(5);
            let (entered, entered_rx) = mpsc::sync_channel(1);
            let (release, release_rx) = mpsc::sync_channel(1);
            let terminal = host.terminal.as_ref().unwrap().clone();
            let thread = std::thread::spawn(move || {
                let exit = ExitGuard(terminal);
                let registration = exit.0.native.register_current();
                let _ = entered.send(registration);
                let _ = release_rx.recv_timeout(deadline.saturating_duration_since(Instant::now()));
                if panic_worker {
                    panic!("original rejected-request worker panic");
                }
            });
            let original = thread.thread().id();
            host.lane.as_mut().unwrap().thread = Some(thread);
            let registration = entered_rx.recv_timeout(deadline.saturating_duration_since(Instant::now()));
            // Script terminal acknowledgement independently of native exit;
            // injected Pending/error MUST NOT consume the still-gated handle.
            *host.terminal.as_ref().unwrap().done.lock().unwrap() = true;
            let query_error = NativeObservationError::MachRetire { code: 61 };
            let pending = host.settle_with(deadline, |_, _| Ok(NativeExitState::Pending), |_| panic!("not joined"));
            let failed = host.settle_with(deadline, |_, _| Err(query_error), |_| panic!("not joined"));
            let retained_thread = host.lane.as_ref().unwrap().thread.as_ref().map(|thread| thread.thread().id());
            let retained = host.rejected.is_some();
            let retained_bytes = budget.retained_bytes();
            // Release and ORIGINAL fixture join precede all assertions. This
            // explicit fixture join is not production native-exit acceptance.
            let released = release.send(());
            let host = std::mem::ManuallyDrop::into_inner(host);
            let observer = budget.clone();
            let (retirement_failed, second, held_after_failure, empty, final_bytes, sticky, stored, first_native) =
                std::thread::spawn(move || {
                    let mut host = std::mem::ManuallyDrop::new(host);
                    if let Err(payload) = host.lane.as_mut().unwrap().thread.take().unwrap().join() {
                        host.panic = Some(payload);
                    }
                    host.lane.as_mut().unwrap().retirement_pending = true;
                    let payload = host.panic.as_deref().map(std::ptr::from_ref);
                    let retirement_failed = host.settle_with(
                        deadline,
                        |_, _| panic!("already joined"),
                        |_| Err(NativeObservationError::MachRetire { code: 73 }),
                    );
                    let held_after_failure = (
                        host.rejected.is_some(),
                        observer.retained_bytes(),
                        host.lane.as_ref().unwrap().retirement_pending,
                    );
                    let second = host.settle_with(
                        deadline,
                        |_, _| panic!("already joined"),
                        NativeExitObservation::retire_after_join,
                    );
                    let sticky = payload
                        .zip(host.panic.as_deref().map(std::ptr::from_ref))
                        .is_some_and(|(before, after)| std::ptr::eq(before, after));
                    let empty = host.rejected.is_none();
                    let stored = host.deadline;
                    let first_native = host.native_error;
                    let final_bytes = observer.retained_bytes();
                    // Off-UI cleanup of the sticky failure is fixture-owned;
                    // production retains it in the original Failure quarantine.
                    drop(std::mem::ManuallyDrop::into_inner(host));
                    (retirement_failed, second, held_after_failure, empty, final_bytes, sticky, stored, first_native)
                })
                .join()
                .unwrap();
            assert!(matches!(registration, Ok(Ok(()))));
            assert!(released.is_ok());
            assert_eq!(pending, Err(SettlementReason::ThreadExitPending));
            assert_eq!(failed, Err(SettlementReason::Native(query_error)));
            assert_eq!(retained_thread, Some(original));
            assert!(retained);
            assert_eq!(retained_bytes, charged);
            assert_eq!(retirement_failed, Err(SettlementReason::Native(query_error)), "retain the first native error");
            assert_eq!(held_after_failure, (true, charged, true));
            assert_eq!(second, if panic_worker { Err(SettlementReason::WorkerPanicked) } else { Ok(()) });
            assert_eq!(empty, !panic_worker);
            assert_eq!(final_bytes, if panic_worker { charged } else { charged - request_credit });
            assert_eq!(sticky, panic_worker, "the exact joined panic survives native retirement and retry");
            assert_eq!(stored, Some(deadline));
            assert_eq!(first_native, Some(query_error));
        }
    }
    #[test]
    fn successful_join_path_retires_rejection_only_after_native_right() {
        let budget = AllocationBudget::default();
        let mut host = Host::with_budget(&budget).unwrap();
        request_lane(&mut host, false);
        host.submit_with(request_key(), Some(Path::new("join-order.md")), Default::default);
        let terminal = host.terminal.as_ref().unwrap().clone();
        host.lane.as_mut().unwrap().thread = Some(std::thread::spawn(move || {
            let exit = ExitGuard(terminal);
            exit.0.native.register_current().unwrap();
        }));
        let charged = budget.retained_bytes();
        let request_credit = host.rejected.as_ref().unwrap().work.path.as_ref().unwrap()._charge.reserved_bytes();
        let observer = budget.clone();
        let (result, retired, empty, after, repeated) = std::thread::spawn(move || {
            let calls = Cell::new(0);
            let deadline = Instant::now() + Duration::from_secs(5);
            // Scripted Exited tests owner ordering only. Existing real native
            // TLS controls remain the authority for actual exit observation.
            let result = host.settle_with(
                deadline,
                |_, _| Ok(NativeExitState::Exited),
                |native| {
                    calls.set(calls.get() + 1);
                    assert_eq!(observer.retained_bytes(), charged, "request credit survives through right retirement");
                    native.retire_after_join()
                },
            );
            let after = observer.retained_bytes();
            let repeated = host.settle_until(deadline);
            (result, calls.get(), host.rejected.is_none(), after, repeated)
        })
        .join()
        .unwrap();
        assert_eq!(result, Ok(()));
        assert_eq!(retired, 1);
        assert!(empty);
        assert_eq!(after, charged - request_credit);
        assert_eq!(repeated, Ok(()));
    }
    #[test]
    fn joined_markdown_panic_retries_right_retirement_before_sticky_failure() {
        let budget = kwil_view_model::visual_rows::AllocationBudget::default();
        let mut host = std::mem::ManuallyDrop::new(Host::with_budget(&budget).unwrap());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        host.deadline = Some(deadline);
        let terminal = host.terminal.as_ref().unwrap().clone();
        let thread: std::thread::JoinHandle<()> = std::thread::spawn(move || {
            let exit = ExitGuard(terminal);
            exit.0.native.register_current().unwrap();
            panic!("original Markdown worker panic");
        });
        // Explicit tiny fixture join establishes the retirement precondition;
        // it is not a production bounded-settlement or native-exit claim.
        host.panic = Some(thread.join().unwrap_err());
        assert_eq!(host.panic_payload().unwrap().downcast_ref::<&str>(), Some(&"original Markdown worker panic"));
        host.lane = Some(Lane { tx: None, thread: None, retirement_pending: true });
        let payload = std::ptr::from_ref(host.panic_payload().unwrap());
        let original = NativeObservationError::MachRetire { code: 73 };
        assert_eq!(host.retire_native_with(deadline, |_| Err(original)), Err(SettlementReason::Native(original)));
        assert!(host.lane.as_ref().unwrap().retirement_pending);
        assert_eq!(host.settle_until(deadline), Err(SettlementReason::WorkerPanicked));
        assert!(!host.lane.as_ref().unwrap().retirement_pending);
        assert!(std::ptr::eq(payload, std::ptr::from_ref(host.panic_payload().unwrap())));
        assert_eq!(host.native_error, Some(original));
        drop(std::mem::ManuallyDrop::into_inner(host));
    }
    #[test]
    fn save_as_plain_text_keeps_an_exit_without_admitting_another_preview() {
        for old in [Mode::Preview, Mode::Split] {
            assert!(controls_available(false, old), "Text escape must remain offered");
            assert_eq!(mode_action("view.markdownText", old, false), ModeAction::Set(Mode::Text));
            assert_eq!(mode_action("view.markdownPreview", old, false), ModeAction::Unavailable);
            assert_eq!(mode_action("view.markdownSplit", old, false), ModeAction::Unavailable);
        }
        assert_eq!(mode_action("view.toggleMarkdown", Mode::Preview, false), ModeAction::Set(Mode::Text));
        assert!(!controls_available(false, Mode::Text));
        assert_eq!(mode_action("view.toggleMarkdown", Mode::Text, false), ModeAction::Unavailable);
        assert_eq!(mode_action("file.save", Mode::Preview, false), ModeAction::Unhandled);
    }
    fn render(source: &str, height: usize, scroll: usize) -> Vec<u32> {
        render_scaled(source, 540, height, scroll, 1.0)
    }
    fn render_scaled(source: &str, width: usize, height: usize, scroll: usize, scale: f64) -> Vec<u32> {
        let mut host = Host { model: Some(kwil_core_markdown::parse(source).unwrap()), scroll, ..Default::default() };
        let ui = chrome::Metrics::new(scale);
        let mut font = font::UiFont::load(ui.text_px()).expect("test needs the platform UI font");
        let mut pixels = vec![0x123456; width * height];
        draw(
            &mut host,
            Mode::Preview,
            true,
            0,
            &mut pixels,
            (width, height),
            Rect { x: 0, y: 0, w: width, h: height },
            &mut font,
            &ui,
            &kwil_core_commands::theme::Theme::builtin(kwil_core_commands::theme::Scheme::Dark),
        );
        pixels
    }
    #[test]
    fn latched_notices_paint_at_pane_origin_without_changing_model_scroll() {
        let ui = chrome::Metrics::new(1.0);
        let mut font = font::UiFont::load(ui.text_px()).expect("test needs the platform UI font");
        let theme = kwil_core_commands::theme::Theme::builtin(kwil_core_commands::theme::Scheme::Dark);
        let bg = theme_ui::rgb(&theme, kwil_core_commands::theme::UiRole::PanelBackground);
        let area = Rect { x: 24, y: 32, w: 720, h: 240 };
        let (width, height) = (800, 320);
        for reason in [Some(Rejection::Full), Some(Rejection::Disconnected), None] {
            for mode in [Mode::Preview, Mode::Split] {
                let mut unscrolled = None;
                for scroll in [0, 1024] {
                    let mut host = Host {
                        // A retained model must not take priority or have its
                        // scroll corrected just to make the notice visible.
                        model: Some(kwil_core_markdown::parse("# Retained model\n").unwrap()),
                        scroll,
                        rejected: reason.map(|reason| RejectedWork {
                            reason,
                            work: Work { key: request_key(), snapshot: Default::default(), path: None },
                        }),
                        admission_notice: Some("Preview unavailable: path metadata admission refused."),
                        ..Default::default()
                    };
                    let mut pixels = vec![0x123456; width * height];
                    draw(&mut host, mode, true, 0, &mut pixels, (width, height), area, &mut font, &ui, &theme);
                    assert_eq!(host.scroll, scroll, "notice painting must preserve ordinary model scroll");
                    let pane_x = area.x + if mode == Mode::Split { area.w / 2 } else { 0 };
                    let pane_w = if mode == Mode::Split { area.w - area.w / 2 } else { area.w };
                    let pad = ui.dip(24.0).min(pane_w / 8);
                    assert!(
                        (area.y + pad..area.y + pad + ui.dip(25.0)).any(|y| {
                            pixels[y * width + pane_x + pad..y * width + pane_x + pane_w - pad]
                                .iter()
                                .any(|pixel| *pixel != bg)
                        }),
                        "{reason:?} notice must paint glyph pixels at the pane origin at scroll {scroll}"
                    );
                    if let Some(expected) = &unscrolled {
                        assert_eq!(&pixels, expected, "{reason:?} notice pixels must be independent of model scroll");
                    } else {
                        unscrolled = Some(pixels);
                    }
                }
            }
        }
    }
    #[test]
    fn checked_and_unchecked_tasks_have_distinct_native_markers() {
        let checked = render("- [x] Task\n", 150, 0);
        let unchecked = render("- [ ] Task\n", 150, 0);
        assert_ne!(checked, unchecked, "task state must survive missing symbol-font glyphs");
        let theme = kwil_core_commands::theme::Theme::builtin(kwil_core_commands::theme::Scheme::Dark);
        let accent = theme_ui::rgb(&theme, kwil_core_commands::theme::UiRole::Accent);
        assert!(checked[24 * 540..49 * 540].contains(&accent), "checked state has a filled native checkbox");
        assert!(!unchecked[24 * 540..49 * 540].contains(&accent), "unchecked state stays hollow");
    }
    #[test]
    fn inline_code_background_never_erases_table_rules() {
        let pixels = render("| Header |\n| --- |\n| `inline_code()` |\n", 250, 0);
        let theme = kwil_core_commands::theme::Theme::builtin(kwil_core_commands::theme::Scheme::Dark);
        let border = theme_ui::rgb(&theme, kwil_core_commands::theme::UiRole::Border);
        for x in 24..516 {
            assert_eq!(pixels[74 * 540 + x], border, "rule at x={x}");
        }
    }
    #[test]
    fn right_aligned_showcase_cell_uses_one_line_at_fractional_dpi() {
        let pixels = render_scaled(
            "| Feature | Example | Status |\n| :--- | :---: | ---: |\n| Escapes | A \\| B | Literal pipe |\n",
            1898,
            400,
            0,
            1.5,
        );
        let theme = kwil_core_commands::theme::Theme::builtin(kwil_core_commands::theme::Scheme::Dark);
        let bg = theme_ui::rgb(&theme, kwil_core_commands::theme::UiRole::PanelBackground);
        // Header starts at36px, body at112px; its second row is vertical padding.
        for y in 151..185 {
            for x in 1270..1840 {
                assert_eq!(pixels[y * 1898 + x], bg, "unnecessary wrapped text at ({x},{y})");
            }
        }
    }
    #[test]
    fn scrolled_heading_and_wrapped_content_match_full_canvas_crop() {
        let source = format!("# Heading\n\n{}", "A paragraph with **strong**, *emphasis* and `code`.\n\n".repeat(24));
        let full = render(&source, 2400, 0);
        let viewport = render(&source, 250, 4);
        for y in 24..240 {
            assert_eq!(&viewport[y * 540..(y + 1) * 540], &full[(y + 4 * 25) * 540..(y + 4 * 25 + 1) * 540], "row {y}");
        }
    }
    #[test]
    fn table_paints_stable_cell_boundaries_instead_of_spaces() {
        let pixels = render("| Left | Center | Right |\n| :--- | :---: | ---: |\n| A | B | C |\n", 400, 0);
        let theme = kwil_core_commands::theme::Theme::builtin(kwil_core_commands::theme::Scheme::Dark);
        let border = theme_ui::rgb(&theme, kwil_core_commands::theme::UiRole::Border);
        for x in [24, 188, 352, 516] {
            assert_eq!(pixels[30 * 540 + x], border, "column edge {x}");
        }
    }
    #[test]
    fn whole_source_and_invalid_input_are_explicit() {
        let stop = AtomicBool::new(false);
        let mut buffer = kwil_core_text::Buffer::default();
        buffer.insert(0, b"# Heading\n\n[global][ref]\n\n[ref]: https://example.com\n").unwrap();
        let model = parse(&buffer.snapshot(), &stop).unwrap();
        assert!(matches!(model.blocks[0].kind, BlockKind::Heading(1)));
        assert!(
            model.blocks.iter().flat_map(|b| &b.spans).any(|s| s.style.link.as_deref() == Some("https://example.com"))
        );
        buffer.insert(0, &vec![b'x'; 256 * 1024]).unwrap();
        assert!(parse(&buffer.snapshot(), &stop).unwrap_err().contains("256 KiB"));
        let mut buffer = kwil_core_text::Buffer::default();
        buffer.insert(0, b"\xff").unwrap();
        assert!(parse(&buffer.snapshot(), &stop).unwrap_err().contains("opaque"));
    }
    #[test]
    fn stale_completion_never_replaces_current_tab() {
        let key = Key { document: kwil_view_model::SessionDocumentId(1), revision: 1, epoch: 1, base: 0 };
        let mut host = Host { desired: Some(Key { revision: 2, ..key }), busy: true, ..Default::default() };
        host.receive(Completion {
            key,
            result: Ok(kwil_core_markdown::parse("# stale").unwrap()),
            images: Default::default(),
            diagrams: Default::default(),
            highlights: Default::default(),
        });
        assert!(!host.busy);
        assert!(host.model.is_none());
        assert!(host.current.is_none());
    }
}
