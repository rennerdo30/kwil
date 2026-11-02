//! Fresh-process measurement of the shipped immutable capture + external diff.
//! Generation is a separate process so its memory is never counted as compare.
use kwil_core_diff::{store, stream};
use kwil_core_text::{
    Buffer, TextFile,
    compare_input::{
        self as input, Admission, AdmissionError, DecodedNativeLease, InputIdentity, InputStore, NativeFileLease,
        ReadWork, SnapshotLease, SourceLease, StoreAdmission, StorePaths, Usage,
    },
};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    fs::{self, OpenOptions},
    io::{self, Write},
    ops::ControlFlow,
    path::{Path, PathBuf},
    rc::Rc,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

const LINE_BYTES: usize = 1024;
const CHANGES: u64 = 16;
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
type Model = stream::DiskComparison<InputStore<Budget>, InputStore<Budget>, Budget>;

#[derive(Default)]
struct Ledger {
    live: Usage,
    peak: Usage,
}
#[derive(Clone, Default)]
struct Budget(Rc<RefCell<Ledger>>);
impl Admission for Budget {
    fn reserve(&mut self, add: Usage) -> std::result::Result<(), AdmissionError> {
        let mut x = self.0.borrow_mut();
        let next = Usage {
            resident: x.live.resident.checked_add(add.resident).ok_or(AdmissionError)?,
            disk: x.live.disk.checked_add(add.disk).ok_or(AdmissionError)?,
            handles: x.live.handles.checked_add(add.handles).ok_or(AdmissionError)?,
        };
        if next.resident > 64 * 1024 * 1024 || next.handles > 32 {
            return Err(AdmissionError);
        }
        x.live = next;
        x.peak.resident = x.peak.resident.max(next.resident);
        x.peak.disk = x.peak.disk.max(next.disk);
        x.peak.handles = x.peak.handles.max(next.handles);
        Ok(())
    }
    fn release(&mut self, n: Usage) {
        let mut x = self.0.borrow_mut();
        x.live.resident -= n.resident;
        x.live.disk -= n.disk;
        x.live.handles -= n.handles;
    }
}
fn quantum() -> ReadWork {
    // Same cooperative work slice as the production compare worker.
    ReadWork::new(8 * 1024 * 1024, 32768)
}
fn lane<B, C>(flow: ControlFlow<B, C>) -> C {
    match flow {
        ControlFlow::Continue(c) => c,
        ControlFlow::Break(_) => panic!("incorrect ownership lane"),
    }
}
fn emit(value: Value) -> Result<()> {
    println!("{value}");
    io::stdout().flush()?;
    Ok(())
}
fn peak_memory() -> io::Result<Value> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::{
            ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX},
            Threading::GetCurrentProcess,
        };
        let mut c: PROCESS_MEMORY_COUNTERS_EX = unsafe { std::mem::zeroed() };
        c.cb = std::mem::size_of_val(&c) as u32;
        // SAFETY: current process pseudo-handle and a correctly sized output.
        if unsafe {
            GetProcessMemoryInfo(GetCurrentProcess(), (&mut c as *mut PROCESS_MEMORY_COUNTERS_EX).cast(), c.cb)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(json!({"peak_rss_bytes": c.PeakWorkingSetSize, "peak_private_bytes": c.PeakPagefileUsage,
            "private_bytes": c.PrivateUsage}))
    }
    #[cfg(target_os = "linux")]
    {
        let mut r: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: live writable output, RUSAGE_SELF has no other preconditions.
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut r) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(json!({"peak_rss_bytes": r.ru_maxrss as u64 * 1024, "peak_private_bytes": null}))
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        Err(io::Error::other("compare memory measurement supports Linux and Windows"))
    }
}
struct Clock {
    start: Instant,
    last: Instant,
    limit: Duration,
    cancel: AtomicBool,
}
impl Clock {
    fn poll(&mut self, phase: &str) -> Result<()> {
        if self.start.elapsed() >= self.limit {
            self.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        if self.last.elapsed() >= Duration::from_secs(5) {
            emit(json!({"event":"progress", "phase":phase,
                "wall_seconds":self.start.elapsed().as_secs_f64(), "memory":peak_memory()?}))?;
            self.last = Instant::now();
        }
        Ok(())
    }
}
fn retire_input(mut retirement: input::Retirement<Budget>) -> Result<()> {
    loop {
        match retirement.step(quantum(), &AtomicBool::new(false)) {
            input::RetirementStep::Ready(_) => return Ok(()),
            input::RetirementStep::Pending(next) => retirement = next,
            input::RetirementStep::Failed { error, .. } => return Err(error.into()),
            _ => return Err("unexpected cleanup cancellation".into()),
        }
    }
}
fn retire_alignment(mut retirement: store::AlignmentRetirement<Budget>) -> Result<()> {
    loop {
        match retirement.step(quantum(), &AtomicBool::new(false)) {
            store::RetirementStep::Ready(_) => return Ok(()),
            store::RetirementStep::Pending(next) => retirement = next,
            store::RetirementStep::Failed { error, .. } => return Err(error.into()),
            _ => return Err("unexpected cleanup cancellation".into()),
        }
    }
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
        drop(self.into_parts().0.into_file());
    }
}
fn capture<S: CaptureOwner>(
    source: S,
    root: PathBuf,
    budget: Budget,
    clock: &mut Clock,
    phase: &str,
) -> Result<Option<InputStore<Budget>>> {
    let paths = StorePaths {
        logical: root.join("logical"),
        original: root.join("original"),
        lines: root.join("lines"),
        atoms: root.join("atoms"),
        directory: root,
    };
    let mut build = match input::begin(source, StoreAdmission { aggregate: budget, paths }) {
        ControlFlow::Continue(b) => b,
        ControlFlow::Break(r) => {
            r.source.retire();
            return Err(r.error.into());
        }
    };
    loop {
        clock.poll(phase)?;
        match build.step(quantum(), &clock.cancel) {
            input::BuildStep::Pending(next) => build = next,
            input::BuildStep::Ready { store, source } => {
                source.retire();
                return Ok(Some(store));
            }
            input::BuildStep::Cancelled { source, retirement } => {
                source.retire();
                retire_input(retirement)?;
                return Ok(None);
            }
            input::BuildStep::Failed { source, retirement, error } => {
                source.retire();
                retire_input(retirement)?;
                return Err(error.into());
            }
        }
    }
}
fn align(
    a: InputStore<Budget>,
    b: InputStore<Budget>,
    root: PathBuf,
    budget: Budget,
    clock: &mut Clock,
) -> Result<Option<Model>> {
    let paths = store::AlignmentPaths {
        files: ["forward", "reverse", "stack", "runs", "hunks"].map(|n| root.join(n)),
        directory: root,
    };
    let mut build = match stream::begin(
        a,
        b,
        stream::StreamOptions::default(),
        store::AlignmentAdmission { aggregate: budget, paths },
    ) {
        ControlFlow::Continue(b) => b,
        ControlFlow::Break(r) => {
            retire_input(r.left.into_retirement())?;
            retire_input(r.right.into_retirement())?;
            return Err(r.error.into());
        }
    };
    loop {
        clock.poll("align")?;
        let step = build.step(quantum(), &clock.cancel);
        match step.state() {
            stream::BuildState::Pending => build = lane(step.into_pending()),
            stream::BuildState::Ready => return Ok(Some(lane(step.into_ready()))),
            _ => {
                let t = lane(step.into_terminal());
                retire_alignment(t.retirement)?;
                retire_input(t.left.into_retirement())?;
                retire_input(t.right.into_retirement())?;
                return match t.error {
                    Some(e) => Err(e.into()),
                    None => Ok(None),
                };
            }
        }
    }
}
fn generate(root: &Path, mib: u64) -> Result<()> {
    if mib > 8 && std::env::var("GITHUB_ACTIONS").as_deref() != Ok("true") {
        return Err("fixtures above 8 MiB are hosted-only".into());
    }
    if mib == 0 {
        return Err("fixture must not be empty".into());
    }
    fs::create_dir(root)?;
    let lines = mib.checked_mul(1024 * 1024).ok_or("fixture overflow")? / LINE_BYTES as u64;
    // One fixed 1 MiB output buffer; actual writes (not sparse placeholders).
    for side in ["left", "right"] {
        let mut file = OpenOptions::new().write(true).create_new(true).open(root.join(side))?;
        let mut buffer = vec![b'x'; 1024 * 1024];
        let mut first = 0;
        while first < lines {
            let count = (lines - first).min(1024) as usize;
            for i in 0..count {
                let n = first + i as u64;
                let row = &mut buffer[i * LINE_BYTES..(i + 1) * LINE_BYTES];
                let label = format!("row={n:016} ");
                row[..label.len()].copy_from_slice(label.as_bytes());
                row[64] =
                    if side == "right" && (1..=CHANGES).any(|k| n == lines * k / (CHANGES + 1)) { b'Y' } else { b'x' };
                row[LINE_BYTES - 1] = b'\n';
            }
            file.write_all(&buffer[..count * LINE_BYTES])?;
            first += count as u64;
        }
        file.sync_all()?;
    }
    emit(json!({"event":"generated", "bytes_per_file":mib * 1024 * 1024, "lines_per_file":lines,
        "line_bytes":LINE_BYTES, "scattered_replacements":CHANGES}))
}
fn compare(root: &Path, seconds: u64) -> Result<()> {
    let budget = Budget::default();
    let start = Instant::now();
    let mut clock = Clock { start, last: start, limit: Duration::from_secs(seconds), cancel: AtomicBool::new(false) };
    let bytes = fs::metadata(root.join("left"))?.len();
    if bytes > 8 * 1024 * 1024 && std::env::var("GITHUB_ACTIONS").as_deref() != Ok("true") {
        return Err("comparison inputs above 8 MiB are hosted-only".into());
    }
    let left = Buffer::from_file(TextFile::open_saved_version(&root.join("left"))?)?.snapshot();
    let lease = SnapshotLease::try_new(left, 0..bytes, InputIdentity([1, 0, 0, 0]), budget.clone())
        .map_err(|r| r.error)?
        .with_comparison_text_runs();
    let mut phases = json!({});
    let Some(a) = capture(lease, root.join("capture-left"), budget.clone(), &mut clock, "capture-left")? else {
        return report(false, "capture-left", bytes, &clock, &budget, phases);
    };
    phases["left_capture_seconds"] = json!(start.elapsed().as_secs_f64());
    let right_start = Instant::now();
    let file = TextFile::open_saved_version(&root.join("right"))?;
    let detection = kwil_core_text::encoding::detect(&file)?;
    let interpretation = kwil_core_text::decoded_backing::Interpretation::new(detection.encoding, detection.bom_len)?;
    let native = NativeFileLease::from_text_file(&file, InputIdentity([2, 0, 0, 0]))?;
    let source = lane(DecodedNativeLease::try_new(native, interpretation, budget.clone()));
    drop(file);
    let b = match capture(source, root.join("capture-right"), budget.clone(), &mut clock, "capture-right") {
        Ok(Some(b)) => b,
        other => {
            retire_input(a.into_retirement())?;
            other?;
            return report(false, "capture-right", bytes, &clock, &budget, phases);
        }
    };
    phases["right_capture_seconds"] = json!(right_start.elapsed().as_secs_f64());
    let align_start = Instant::now();
    let model = align(a, b, root.join("alignment"), budget.clone(), &mut clock)?;
    phases["alignment_seconds"] = json!(align_start.elapsed().as_secs_f64());
    let complete = model.is_some();
    if let Some(model) = model {
        let correct = model.hunk_count() == CHANGES && model.row_count() == bytes / LINE_BYTES as u64 + 1;
        phases["hunks"] = json!(model.hunk_count());
        phases["rows"] = json!(model.row_count());
        let (a, b, _, retirement) = model.into_parts();
        retire_alignment(retirement)?;
        retire_input(a.into_retirement())?;
        retire_input(b.into_retirement())?;
        if !correct {
            return Err("incorrect fixture alignment or hunk count".into());
        }
    }
    report(complete, "align", bytes, &clock, &budget, phases)
}
fn report(complete: bool, phase: &str, bytes: u64, clock: &Clock, budget: &Budget, phases: Value) -> Result<()> {
    let x = budget.0.borrow();
    assert_eq!(x.live, Usage::default(), "all capture/alignment owners retired");
    emit(json!({"event":"result", "complete":complete, "stop_phase":phase, "bytes_per_file":bytes,
        "wall_seconds":clock.start.elapsed().as_secs_f64(), "limit_seconds":clock.limit.as_secs(),
        "memory":peak_memory()?, "admitted_peak_resident_bytes":x.peak.resident,
        "scratch_peak_bytes":x.peak.disk, "phases":phases}))
}
fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let command = args.next().ok_or("expected generate or measure")?;
    let root = PathBuf::from(args.next().ok_or("expected fixture directory")?);
    let n = args.next().ok_or("expected MiB or timeout seconds")?.parse()?;
    if args.next().is_some() {
        return Err("unexpected arguments".into());
    }
    emit(json!({"event":"start", "pid":std::process::id(), "command":command}))?;
    match command.as_str() {
        "generate" => generate(&root, n),
        "measure" => compare(&root, n),
        _ => Err("expected generate or measure".into()),
    }
}
