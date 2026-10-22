//! M0 text engine benchmark: kwil-core-text vs qem on one large file.
//!
//!   textbench gen <path> <megabytes>   write a deterministic log-like fixture
//!   textbench run <path> [runs]        run both engines, each in a fresh process
//!   textbench kwil|qem <path>          one measurement, prints one JSON line

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const SCREEN: usize = 60;
const JUMP_LINE: u64 = 15_000_000;
const INDEX_THREADS: usize = 8;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("gen") => generate(Path::new(&args[2]), args[3].parse().expect("megabytes")),
        Some("run") => run_all(Path::new(&args[2]), args.get(3).map_or(5, |r| r.parse().unwrap())),
        Some("kwil") => measure_kwil(Path::new(&args[2])),
        Some("qem") => measure_qem(Path::new(&args[2])),
        _ => eprintln!("usage: textbench gen|run|kwil|qem <path> ..."),
    }
}

fn generate(path: &Path, megabytes: u64) {
    use std::io::Write;
    let target = megabytes << 20;
    let mut out = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(path).unwrap());
    let levels = ["INFO ", "DEBUG", "WARN ", "ERROR"];
    let words = ["request", "served", "cache", "miss", "user", "session", "opened", "closed", "retry", "ok"];
    let (mut written, mut i, mut seed) = (0u64, 0u64, 0x9e3779b97f4a7c15u64);
    while written < target {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let mut line = format!("2026-10-23T{:02}:{:02}:{:02}.{:03} {} line={i}", (i / 3_600_000) % 24, (i / 60_000) % 60, (i / 1000) % 60, i % 1000, levels[(seed % 4) as usize]);
        for k in 0..(seed >> 60) % 4 {
            line.push(' ');
            line.push_str(words[((seed >> (k * 8)) % 10) as usize]);
        }
        line.push('\n');
        out.write_all(line.as_bytes()).unwrap();
        written += line.len() as u64;
        i += 1;
    }
    out.flush().unwrap();
    println!("wrote {written} bytes, {i} lines to {}", path.display());
}

fn qem_sidecar(path: &Path) -> PathBuf {
    path.with_file_name(format!(".{}.qem.lineidx", path.file_name().unwrap().to_string_lossy()))
}

fn run_all(path: &Path, runs: usize) {
    let exe = std::env::current_exe().unwrap();
    for engine in ["kwil", "qem"] {
        for run in 0..=runs {
            let _ = std::fs::remove_file(qem_sidecar(path));
            let out = Command::new(&exe).args([engine, path.to_str().unwrap()]).output().unwrap();
            // Run 0 warms the OS page cache and is not reported.
            if run > 0 {
                print!("{}", String::from_utf8_lossy(&out.stdout));
            }
            if !out.status.success() {
                eprintln!("{engine} failed: {}", String::from_utf8_lossy(&out.stderr));
            }
        }
    }
    let _ = std::fs::remove_file(qem_sidecar(path));
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn measure_kwil(path: &Path) {
    let t0 = Instant::now();
    let file = kwil_core_text::TextFile::open(path).unwrap();
    let t_open = t0.elapsed();
    let first: usize = file.first_lines(SCREEN).iter().map(|l| l.len()).sum();
    let t_first = t0.elapsed();
    let t1 = Instant::now();
    let lines = file.build_index(INDEX_THREADS).line_count();
    let t_index = t1.elapsed();
    let t2 = Instant::now();
    let jump = file.lines(JUMP_LINE, SCREEN).unwrap();
    let t_jump = t2.elapsed();
    let sample = String::from_utf8_lossy(jump[0]).into_owned();
    report("kwil", file.len(), t_open, t_first, t_index, t_jump, lines as usize, first, &sample);
}

fn measure_qem(path: &Path) {
    use qem::{Document, ViewportRequest};
    let t0 = Instant::now();
    let doc = Document::open(path.to_path_buf()).unwrap();
    let t_open = t0.elapsed();
    let first: usize = doc.read_viewport(ViewportRequest::new(0, SCREEN)).rows().iter().map(|r| r.text().len()).sum();
    let t_first = t0.elapsed();
    let t1 = Instant::now();
    while doc.is_line_count_pending() {
        std::thread::sleep(Duration::from_millis(1));
    }
    let t_index = t1.elapsed();
    let lines = doc.exact_line_count().unwrap_or(0);
    let t2 = Instant::now();
    let jump = doc.read_viewport(ViewportRequest::new(JUMP_LINE as usize, SCREEN));
    let t_jump = t2.elapsed();
    let sample = jump.rows().first().map(|r| r.text().to_string()).unwrap_or_default();
    let len = std::fs::metadata(path).unwrap().len();
    report("qem", len, t_open, t_first, t_index, t_jump, lines, first, &sample);
}

#[allow(clippy::too_many_arguments)]
fn report(engine: &str, len: u64, open: Duration, first: Duration, index: Duration, jump: Duration, lines: usize, first_bytes: usize, sample: &str) {
    let gbps = len as f64 / index.as_secs_f64() / 1e9;
    println!(
        "{{\"engine\":\"{engine}\",\"open_ms\":{:.3},\"first_screen_ms\":{:.3},\"index_ms\":{:.1},\"index_gb_s\":{gbps:.2},\"jump_ms\":{:.3},\"lines\":{lines},\"first_bytes\":{first_bytes},\"peak_ws_mb\":{:.1},\"private_mb\":{:.1},\"line_15m\":{:?}}}",
        ms(open), ms(first), ms(index), ms(jump), mem().0, mem().1, sample.chars().take(40).collect::<String>()
    );
}

#[cfg(windows)]
/// (peak working set incl. touched mapped pages, private commit) in MB.
fn mem() -> (f64, f64) {
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut c: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
    let mb = (1 << 20) as f64;
    (c.PeakWorkingSetSize as f64 / mb, c.PagefileUsage as f64 / mb)
}

#[cfg(not(windows))]
fn mem() -> (f64, f64) {
    (0.0, 0.0)
}
