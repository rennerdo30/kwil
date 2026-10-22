use kwil_core_text::TextFile;
use std::path::Path;
use std::time::Instant;

pub const SCREEN: usize = 60;
pub const JUMP: u64 = 15_000_000;
pub const THREADS: usize = 4;

pub fn measure(path: &Path) -> std::io::Result<serde_json::Value> {
    let start = Instant::now();
    let file = TextFile::open(path)?;
    let first: usize =
        std::hint::black_box(file.first_lines(SCREEN).map_err(std::io::Error::other)?).iter().map(|l| l.len()).sum();
    let open_ms = start.elapsed().as_secs_f64() * 1000.0;
    assert!(first > 0);
    let start = Instant::now();
    let count = file.build_index(THREADS).map_err(std::io::Error::other)?.line_count();
    let index_ms = start.elapsed().as_secs_f64() * 1000.0;
    assert!(count > JUMP);
    let start = Instant::now();
    let jump = file.lines(JUMP, SCREEN).map_err(std::io::Error::other)?.expect("index ready and fixture has 15M lines");
    assert!(String::from_utf8_lossy(jump[0]).contains("line=15000000"));
    let jump_ms = start.elapsed().as_secs_f64() * 1000.0;
    Ok(
        serde_json::json!({"open_first_ms": open_ms, "index_gb_s": file.len() as f64 / index_ms / 1e6, "index_ms":index_ms, "jump_ms":jump_ms, "private_mb":crate::private_bytes()? as f64 / 1048576.0, "lines":count}),
    )
}
