//! External-memory host backend. All owners and I/O stay on the compare worker.
use super::{Action, Kind, PAGE_ROWS, Page, ROW_BYTES, Row};
use kwil_core_commands::settings::CompareSettings;
use kwil_core_diff::{self as diff, store, stream, words};
use kwil_core_text::{
    Snapshot, TextFile,
    compare_input::{
        self as input, Admission, AdmissionError, DecodedNativeLease, InputIdentity, InputSource, InputStore,
        LineLookup, LineSpan, NativeFileLease, PayloadSpace, ReadWork, SnapshotLease, SourceLease, Step,
        StoreAdmission, StorePaths, Usage,
    },
};
use std::{
    cell::RefCell,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    ops::ControlFlow,
    path::PathBuf,
    rc::Rc,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

type Model = stream::DiskComparison<InputStore<Budget>, InputStore<Budget>, Budget>;
// Includes two published pages, worst-case clipped word ranges, host paths,
// projection/query buffers, decoder/index scratch and worker stack. The engine
// additionally admits its actual fixed state against the SAME 64 MiB budget.
const HOST_RESIDENT: usize = 40 * 1024 * 1024;
#[derive(Clone)]
struct Budget(Rc<RefCell<Usage>>);
impl Budget {
    fn new() -> Self {
        Self(Rc::new(RefCell::new(Usage { resident: HOST_RESIDENT, handles: 2, ..Usage::default() })))
    }
}
impl Admission for Budget {
    fn reserve(&mut self, n: Usage) -> Result<(), AdmissionError> {
        let mut live = self.0.borrow_mut();
        let next = Usage {
            resident: live.resident.checked_add(n.resident).ok_or(AdmissionError)?,
            disk: live.disk.checked_add(n.disk).ok_or(AdmissionError)?,
            handles: live.handles.checked_add(n.handles).ok_or(AdmissionError)?,
        };
        if next.resident > 64 * 1024 * 1024 || next.handles > 32 {
            return Err(AdmissionError);
        }
        *live = next;
        Ok(())
    }
    fn release(&mut self, n: Usage) {
        let mut live = self.0.borrow_mut();
        live.resident -= n.resident;
        live.disk -= n.disk;
        live.handles -= n.handles;
    }
}
fn quantum() -> ReadWork {
    ReadWork::new(8 * 1024 * 1024, 32768)
}
fn error(e: impl std::fmt::Display) -> String {
    format!("Compare: {e}")
}
fn lane<B, C>(value: ControlFlow<B, C>) -> C {
    match value {
        ControlFlow::Continue(value) => value,
        ControlFlow::Break(_) => unreachable!("checked ownership lane"),
    }
}
fn drive<T>(cancel: &AtomicBool, mut poll: impl FnMut(&mut ReadWork) -> Step<T>) -> Result<T, String> {
    loop {
        match poll(&mut quantum()) {
            Step::Ready(value) => return Ok(value),
            Step::Pending => std::thread::yield_now(),
            Step::Cancelled => return Err("Comparison cancelled".into()),
            Step::Failed(e) => return Err(error(e)),
        }
        if cancel.load(Ordering::Relaxed) {
            return Err("Comparison cancelled".into());
        }
    }
}
fn retire_input(store: InputStore<Budget>) {
    retire_capture(store.into_retirement());
}
fn retire_alignment(mut retirement: store::AlignmentRetirement<Budget>) {
    loop {
        match retirement.step(quantum(), &AtomicBool::new(false)) {
            store::RetirementStep::Ready(_) => return,
            store::RetirementStep::Pending(next) | store::RetirementStep::Cancelled(next) => retirement = next,
            store::RetirementStep::Failed { retirement, error: e } => {
                eprintln!("Compare alignment cleanup retained at {}: {e}", retirement.paths().directory.display());
                return;
            }
        }
    }
}
fn retire_model(model: Model) {
    let (left, right, _, retirement) = model.into_parts();
    retire_alignment(retirement);
    retire_input(left);
    retire_input(right);
}
fn capture(
    snapshot: Snapshot,
    root: PathBuf,
    budget: Budget,
    id: u64,
    cancel: &AtomicBool,
) -> Result<InputStore<Budget>, String> {
    let len = snapshot.len();
    let source = SnapshotLease::try_new(snapshot, 0..len, InputIdentity([id, 0, 0, 0]), budget.clone())
        .map_err(|e| error(e.error))?
        .with_comparison_text_runs();
    capture_source(source, root, budget, cancel)
}
trait CaptureOwner: SourceLease {
    fn retire(self);
}
impl CaptureOwner for SnapshotLease<Budget> {
    fn retire(self) {
        drop(self.into_parts());
    }
}
impl CaptureOwner for DecodedNativeLease<Budget> {
    fn retire(self) {
        let (native, _) = self.into_parts();
        drop(native.into_file());
    }
}
fn capture_source<S: CaptureOwner>(
    source: S,
    root: PathBuf,
    budget: Budget,
    cancel: &AtomicBool,
) -> Result<InputStore<Budget>, String> {
    let paths = StorePaths {
        logical: root.join("logical"),
        original: root.join("original"),
        lines: root.join("lines"),
        atoms: root.join("atoms"),
        directory: root,
    };
    let mut build = match input::begin(source, StoreAdmission { aggregate: budget, paths }) {
        ControlFlow::Continue(b) => b,
        ControlFlow::Break(rejected) => {
            rejected.source.retire();
            return Err(error(rejected.error));
        }
    };
    loop {
        match build.step(quantum(), cancel) {
            input::BuildStep::Pending(next) => {
                build = next;
                std::thread::yield_now();
            }
            input::BuildStep::Ready { store, source } => {
                source.retire();
                return Ok(store);
            }
            input::BuildStep::Cancelled { source, retirement } => {
                source.retire();
                retire_capture(retirement);
                return Err("Comparison cancelled".into());
            }
            input::BuildStep::Failed { source, retirement, error: e } => {
                source.retire();
                retire_capture(retirement);
                return Err(error(e));
            }
        }
    }
}
fn retire_capture(mut retirement: input::Retirement<Budget>) {
    loop {
        match retirement.step(quantum(), &AtomicBool::new(false)) {
            input::RetirementStep::Ready(_) => return,
            input::RetirementStep::Pending(next) | input::RetirementStep::Cancelled(next) => retirement = next,
            input::RetirementStep::Failed { retirement, error: e } => {
                eprintln!("Compare capture cleanup retained at {}: {e}", retirement.paths().directory.display());
                return;
            }
        }
    }
}
fn paths(root: PathBuf) -> store::AlignmentPaths {
    store::AlignmentPaths {
        files: ["forward", "reverse", "stack", "runs", "hunks"].map(|n| root.join(n)),
        directory: root,
    }
}
fn align(
    left: InputStore<Budget>,
    right: InputStore<Budget>,
    options: stream::StreamOptions,
    admission: store::AlignmentAdmission<Budget>,
    cancel: &AtomicBool,
) -> Result<Model, String> {
    let mut build = match stream::begin(left, right, options, admission) {
        ControlFlow::Continue(b) => b,
        ControlFlow::Break(r) => {
            retire_input(r.left);
            retire_input(r.right);
            return Err(error(r.error));
        }
    };
    loop {
        let step = build.step(quantum(), cancel);
        match step.state() {
            stream::BuildState::Ready => return Ok(lane(step.into_ready())),
            stream::BuildState::Pending => {
                build = lane(step.into_pending());
                std::thread::yield_now();
            }
            _ => {
                let terminal = lane(step.into_terminal());
                retire_alignment(terminal.retirement);
                retire_input(terminal.left);
                retire_input(terminal.right);
                return Err(terminal.error.map_or_else(|| "Comparison cancelled".into(), error));
            }
        }
    }
}

// The collapse projection is fixed-width disk metadata too. Never retain an
// input-sized list of hunks, segments or expanded ranges in the host.
struct Projection {
    file: Option<File>,
    root: PathBuf,
    count: u64,
    total: u64,
    disk: u64,
    budget: Budget,
}
impl Projection {
    fn new(root: PathBuf, mut budget: Budget) -> Result<Self, String> {
        fs::create_dir(&root).map_err(error)?;
        budget.reserve(Usage { handles: 1, ..Usage::default() }).map_err(|e| error(input::InputError::from(e)))?;
        let mut this = Self { file: None, root, count: 0, total: 0, disk: 0, budget };
        this.file = Some(
            OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(this.root.join("projection"))
                .map_err(error)?,
        );
        Ok(this)
    }
    fn record(&mut self, i: u64) -> Result<[u64; 4], String> {
        let mut bytes = [0u8; 32];
        let file = self.file.as_mut().unwrap();
        file.seek(SeekFrom::Start(i.checked_mul(32).ok_or("Compare coordinate overflow")?)).map_err(error)?;
        file.read_exact(&mut bytes).map_err(error)?;
        Ok(std::array::from_fn(|i| u64::from_le_bytes(bytes[i * 8..i * 8 + 8].try_into().unwrap())))
    }
    fn write(&mut self, i: u64, r: [u64; 4]) -> Result<(), String> {
        let end = i.checked_add(1).and_then(|n| n.checked_mul(32)).ok_or("Compare coordinate overflow")?;
        let disk = end.checked_add(65535).ok_or("Compare coordinate overflow")? / 65536 * 65536;
        if disk > self.disk {
            self.budget
                .reserve(Usage { disk: disk - self.disk, ..Usage::default() })
                .map_err(|e| error(input::InputError::from(e)))?;
            self.disk = disk;
        }
        let mut bytes = [0u8; 32];
        for (i, n) in r.into_iter().enumerate() {
            bytes[i * 8..i * 8 + 8].copy_from_slice(&n.to_le_bytes());
        }
        let file = self.file.as_mut().unwrap();
        file.seek(SeekFrom::Start(i * 32)).map_err(error)?;
        file.write_all(&bytes).map_err(error)
    }
    fn append(&mut self, start: u64, end: u64, hidden: bool) -> Result<(), String> {
        if start == end {
            return Ok(());
        }
        self.write(self.count, [self.total, start, end, u64::from(hidden)])?;
        self.count += 1;
        self.total =
            self.total.checked_add(if hidden { 1 } else { end - start }).ok_or("Compare coordinate overflow")?;
        Ok(())
    }
    fn find(&mut self, value: u64, raw: bool) -> Result<(u64, [u64; 4]), String> {
        let (mut low, mut high) = (0, self.count);
        while low < high {
            let mid = low + (high - low) / 2;
            let r = self.record(mid)?;
            let start = r[usize::from(raw)];
            let len = if raw || r[3] == 0 { r[2] - r[1] } else { 1 };
            if value < start {
                high = mid;
            } else if value >= start + len {
                low = mid + 1;
            } else {
                return Ok((mid, r));
            }
        }
        Err("Compare projection coordinate out of range".into())
    }
    fn raw(&mut self, display: u64) -> Result<(u64, u64), String> {
        let (_, r) = self.find(display, false)?;
        Ok((r[1] + if r[3] == 0 { display - r[0] } else { 0 }, if r[3] == 0 { 0 } else { r[2] - r[1] }))
    }
    fn display(&mut self, row: u64) -> Result<u64, String> {
        let (_, r) = self.find(row, true)?;
        Ok(r[0] + if r[3] == 0 { row - r[1] } else { 0 })
    }
    fn expand(&mut self, display: u64, cancel: &AtomicBool) -> Result<(), String> {
        let (i, mut r) = self.find(display, false)?;
        if r[3] == 0 {
            return Ok(());
        }
        let extra = r[2] - r[1] - 1;
        r[3] = 0;
        self.write(i, r)?;
        for j in i + 1..self.count {
            // Finish this projection mutation atomically; cancellation abandons
            // the session, so no partial projection can reach the UI.
            if cancel.load(Ordering::Relaxed) {
                return Err("Comparison cancelled".into());
            }
            let mut next = self.record(j)?;
            next[0] += extra;
            self.write(j, next)?;
        }
        self.total += extra;
        Ok(())
    }
}
impl Drop for Projection {
    fn drop(&mut self) {
        drop(self.file.take());
        let path = self.root.join("projection");
        if let Err(e) = fs::remove_file(&path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("Compare projection cleanup {}: {e}", path.display());
        }
        if let Err(e) = fs::remove_dir(&self.root) {
            eprintln!("Compare directory cleanup {}: {e}", self.root.display());
        }
        self.budget.release(Usage { handles: 1, disk: self.disk, ..Usage::default() });
    }
}

pub(super) struct LargeState {
    model: Option<Model>,
    projection: Projection,
    budget: Budget,
    top: u64,
    collapsed: bool,
    context: u64,
    title: String,
}
impl Drop for LargeState {
    fn drop(&mut self) {
        if let Some(model) = self.model.take() {
            retire_model(model);
        }
    }
}
impl LargeState {
    pub(super) fn open(
        left: Snapshot,
        path: PathBuf,
        cancel: &AtomicBool,
        settings: &CompareSettings,
    ) -> Result<Self, String> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let budget = Budget::new();
        let root = std::env::temp_dir().join(format!(
            "kwil-large-compare-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        if root.as_os_str().len() > 4096 {
            return Err("Compare temporary path is too long".into());
        }
        let projection = Projection::new(root, budget.clone())?;
        let a = capture(left, projection.root.join("left"), budget.clone(), 1, cancel)?;
        let right = (|| {
            let file = TextFile::open_saved_version(&path).map_err(error)?;
            let detection = kwil_core_text::encoding::detect(&file).map_err(error)?;
            let interpretation =
                kwil_core_text::decoded_backing::Interpretation::new(detection.encoding, detection.bom_len)
                    .map_err(error)?;
            let native = NativeFileLease::from_text_file(&file, InputIdentity([2, 0, 0, 0])).map_err(error)?;
            let source = match DecodedNativeLease::try_new(native, interpretation, budget.clone()) {
                ControlFlow::Continue(source) => source,
                ControlFlow::Break(rejected) => {
                    drop(rejected.native.into_file());
                    return Err(error(rejected.error));
                }
            };
            drop(file);
            capture_source(source, projection.root.join("right"), budget.clone(), cancel)
        })();
        let b = match right {
            Ok(b) => b,
            Err(e) => {
                retire_input(a);
                return Err(e);
            }
        };
        let options = stream::StreamOptions {
            whitespace: if settings.ignore_whitespace { diff::Whitespace::All } else { diff::Whitespace::Exact },
            ..Default::default()
        };
        let model = align(
            a,
            b,
            options,
            store::AlignmentAdmission { aggregate: budget.clone(), paths: paths(projection.root.join("align")) },
            cancel,
        )?;
        let mut state = Self {
            model: Some(model),
            projection,
            budget,
            top: 0,
            collapsed: settings.collapse_unchanged,
            context: settings.context_lines as u64,
            title: path.file_name().unwrap_or_default().to_string_lossy().chars().take(100).collect(),
        };
        state.rebuild(cancel)?;
        let first = state.hunk(1, false, cancel)?.or(state.hunk(0, true, cancel)?);
        if let Some(hunk) = first {
            state.top = state.projection.display(hunk.rows.start.saturating_sub(state.context))?;
        }
        Ok(state)
    }
    fn hunk(&mut self, row: u64, forward: bool, cancel: &AtomicBool) -> Result<Option<stream::DiskHunk>, String> {
        let mut query = stream::NavigationQuery::new(row, forward);
        drive(cancel, |work| self.model.as_mut().unwrap().navigate(&mut query, work, cancel))
    }
    fn rebuild(&mut self, cancel: &AtomicBool) -> Result<(), String> {
        self.projection.count = 0;
        self.projection.total = 0;
        let rows = self.model.as_ref().unwrap().row_count();
        if !self.collapsed {
            return self.projection.append(0, rows, false);
        }
        let mut hunk = self.hunk(1, false, cancel)?.or(self.hunk(0, true, cancel)?);
        let mut at = 0;
        let mut visible_end = 0;
        while let Some(current) = hunk {
            let start = current.rows.start.saturating_sub(self.context);
            if start > visible_end {
                self.projection.append(at, visible_end, false)?;
                self.projection.append(visible_end, start, true)?;
                at = start;
            }
            visible_end = (current.rows.end.saturating_add(self.context)).min(rows);
            hunk = self.hunk(current.rows.start, true, cancel)?;
        }
        self.projection.append(at, visible_end, false)?;
        self.projection.append(visible_end, rows, true)?;
        self.top = self.top.min(self.projection.total.saturating_sub(1));
        Ok(())
    }
    pub(super) fn action(&mut self, action: Action, cancel: &AtomicBool) -> Result<(), String> {
        let row = self.projection.raw(self.top)?.0;
        match action {
            Action::Scroll(delta) => {
                self.top = self.top.saturating_add_signed(delta).min(self.projection.total.saturating_sub(1))
            }
            Action::Next | Action::Previous => {
                if let Some(hunk) = self.hunk(row, matches!(action, Action::Next), cancel)? {
                    self.top = self.projection.display(hunk.rows.start)?;
                }
            }
            Action::Unchanged => {
                self.collapsed = !self.collapsed;
                self.rebuild(cancel)?;
                self.top = self.projection.display(row)?;
            }
            Action::Expand(index) => self.projection.expand(index as u64, cancel)?,
            Action::Whitespace => {
                let (left, right, mut options, retirement) = self.model.take().unwrap().into_parts();
                retire_alignment(retirement);
                options.whitespace = if options.whitespace == diff::Whitespace::Exact {
                    diff::Whitespace::All
                } else {
                    diff::Whitespace::Exact
                };
                self.model = Some(align(
                    left,
                    right,
                    options,
                    store::AlignmentAdmission {
                        aggregate: self.budget.clone(),
                        paths: paths(self.projection.root.join("align")),
                    },
                    cancel,
                )?);
                self.top = 0;
                self.rebuild(cancel)?;
            }
        }
        Ok(())
    }
    fn aligned(&mut self, at: u64, cancel: &AtomicBool) -> Result<stream::AlignedRow, String> {
        let mut query = stream::PageQuery::new(at, 1);
        let mut page = stream::Page::default();
        drive(cancel, |work| self.model.as_mut().unwrap().page(&mut query, &mut page, work, cancel))?;
        page.rows[0].ok_or_else(|| "Compare alignment row missing".into())
    }
    pub(super) fn page(&mut self, cancel: &AtomicBool) -> Result<Page, String> {
        let mut rows = Vec::with_capacity(PAGE_ROWS);
        for index in self.top..self.projection.total.min(self.top + PAGE_ROWS as u64) {
            let (at, hidden) = self.projection.raw(index)?;
            let aligned = self.aligned(at, cancel)?;
            let mut row = Row {
                left: aligned.left.map(|n| n as usize),
                right: aligned.right.map(|n| n as usize),
                kind: aligned.kind,
                hidden: hidden as usize,
                text: [Vec::new(), Vec::new()],
                words: [Vec::new(), Vec::new()],
            };
            if hidden == 0 {
                let mut spans = [None, None];
                let (left, right) = self.model.as_mut().unwrap().inputs_mut();
                for (side, source, line) in [(0, left, aligned.left), (1, right, aligned.right)] {
                    if let Some(line) = line {
                        let mut query = LineLookup::new(line);
                        let span = drive(cancel, |work| source.line_span(&mut query, work, cancel))?;
                        let end = span.content_end.min(span.start + ROW_BYTES as u64);
                        row.text[side].resize((end - span.start) as usize, 0);
                        let mut offset = 0;
                        while offset < row.text[side].len() {
                            let n = drive(cancel, |work| {
                                source.read_at(
                                    PayloadSpace::Logical,
                                    span.start + offset as u64,
                                    &mut row.text[side][offset..],
                                    work,
                                    cancel,
                                )
                            })?;
                            if n == 0 {
                                return Err("Compare source ended before its line".into());
                            }
                            offset += n;
                        }
                        spans[side] = Some(span);
                    }
                }
                if let [Some(left), Some(right)] = spans
                    && aligned.kind == Kind::Modify
                {
                    row.words = self.refine(aligned, left, right, cancel)?;
                }
            }
            rows.push(row);
        }
        Ok(Page {
            rows,
            top: self.top as usize,
            total: self.projection.total as usize,
            changes: self.model.as_ref().unwrap().hunk_count() as usize,
            ignore_whitespace: self.model.as_ref().unwrap().options().whitespace != diff::Whitespace::Exact,
            collapsed: self.collapsed,
            right_title: self.title.clone(),
        })
    }
    fn refine(
        &mut self,
        row: stream::AlignedRow,
        left: LineSpan,
        right: LineSpan,
        cancel: &AtomicBool,
    ) -> Result<[Vec<std::ops::Range<usize>>; 2], String> {
        let hunk = self.hunk(row.aligned.saturating_add(1), false, cancel)?.ok_or("Compare hunk missing")?;
        let backend = words::WordBackend::Similar32TokensExternalExact;
        let request = words::WordRequest { row, left, right, hunk, backend };
        let comparison = self.model.take().unwrap();
        let mut build = match words::begin(
            comparison,
            request,
            store::AlignmentAdmission {
                aggregate: self.budget.clone(),
                paths: paths(self.projection.root.join("words")),
            },
        ) {
            ControlFlow::Continue(b) => b,
            ControlFlow::Break(r) => {
                self.model = Some(r.comparison);
                return Err(error(r.error));
            }
        };
        let (terminal, result) = (|| {
            loop {
                let step = build.step(quantum(), cancel);
                match step.state() {
                    words::WordBuildState::Pending => {
                        build = lane(step.into_build());
                        std::thread::yield_now();
                    }
                    words::WordBuildState::Ready => {
                        build = lane(step.into_build());
                        break;
                    }
                    words::WordBuildState::Terminal => {
                        let terminal = lane(step.into_terminal());
                        let error = terminal.error.as_ref().map_or_else(|| "Comparison cancelled".into(), error);
                        return (terminal, Err(error));
                    }
                }
            }
            let mut ranges = [Vec::new(), Vec::new()];
            let mut continuation = None;
            loop {
                let mut query = words::WordPageQuery::new(backend, continuation);
                let mut page = words::WordPage::empty(backend);
                if let Err(e) = drive(cancel, |work| build.page(&mut query, &mut page, work, cancel)) {
                    return (build.into_parts(), Err(e));
                }
                for change in page.records[..page.len].iter().flatten() {
                    for (side, span, range) in [(0, left, &change.left), (1, right, &change.right)] {
                        let start = range.start.max(span.start);
                        let end = range.end.min(span.start + ROW_BYTES as u64).min(span.content_end);
                        if start < end {
                            ranges[side].push((start - span.start) as usize..(end - span.start) as usize);
                        }
                    }
                }
                if page.coverage.complete {
                    break;
                }
                continuation = page.continuation;
            }
            (build.into_parts(), Ok(ranges))
        })();
        self.model = Some(terminal.comparison);
        let mut retirement = terminal.retirement;
        loop {
            match retirement.step(quantum(), &AtomicBool::new(false)) {
                words::WordRetirementStep::Ready(_) => break,
                words::WordRetirementStep::Pending(next) | words::WordRetirementStep::Cancelled(next) => {
                    retirement = next
                }
                words::WordRetirementStep::Failed { retirement, error: e } => {
                    eprintln!("Compare words cleanup retained at {}: {e}", retirement.paths().directory.display());
                    return Err(error(e));
                }
            }
        }
        result
    }
}
