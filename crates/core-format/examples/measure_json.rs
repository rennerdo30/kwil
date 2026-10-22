//! One-shot measurement. Generate fixtures outside Git, format once, report time.
use kwil_core_format::{Language, Settings, format};
use std::{
    fs::File,
    io::{BufWriter, Write},
    path::PathBuf,
    time::Instant,
};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os().nth(1).map(PathBuf::from).ok_or("supply an ignored fixtures/ output path")?;
    let mut file = BufWriter::new(File::create(&path)?);
    file.write_all(b"[")?;
    let record = br#"{"z":1e+09,"a":12345678901234567890},"#;
    let records = 10_000_000 / record.len();
    for _ in 0..records {
        file.write_all(record)?;
    }
    file.write_all(br#"{"end":true}]"#)?;
    file.flush()?;
    drop(file);
    let input = std::fs::read(path)?;
    let start = Instant::now();
    let output = format(&input, Language::Json, "json", None, &Settings::default())?;
    let elapsed = start.elapsed();
    println!(
        "input_bytes={} output_bytes={} elapsed_ms={:.3} decimal_MB_per_s={:.2}",
        input.len(),
        output.bytes.len(),
        elapsed.as_secs_f64() * 1000.0,
        input.len() as f64 / elapsed.as_secs_f64() / 1_000_000.0
    );
    Ok(())
}
