//! Deterministic, bounded-buffer fixture generators (ported from spike/textbench gen).
use std::io::{self, Write};
use std::path::Path;

pub const VERSION: &str = "v1";
pub const LOG: &str = "log-1g.log";

pub fn log(path: &Path, target: u64) -> io::Result<()> {
    let mut out = io::BufWriter::with_capacity(1 << 20, std::fs::File::create(path)?);
    let levels = ["INFO ", "DEBUG", "WARN ", "ERROR"];
    let words = ["request", "served", "cache", "miss", "user", "session", "opened", "closed", "retry", "ok"];
    let (mut written, mut i, mut seed) = (0u64, 0u64, 0x9e3779b97f4a7c15u64);
    while written < target {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let mut line = format!(
            "2026-10-23T{:02}:{:02}:{:02}.{:03} {} line={i}",
            (i / 3_600_000) % 24,
            (i / 60_000) % 60,
            (i / 1000) % 60,
            i % 1000,
            levels[(seed % 4) as usize]
        );
        for k in 0..(seed >> 60) % 4 {
            line.push(' ');
            line.push_str(words[((seed >> (k * 8)) % 10) as usize]);
        }
        line.push('\n');
        out.write_all(line.as_bytes())?;
        written += line.len() as u64;
        i += 1;
    }
    out.flush()
}

pub fn generate(dir: &Path, all: bool) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    if !std::fs::metadata(dir.join(LOG)).is_ok_and(|m| m.len() >= 1 << 30) {
        log(&dir.join(LOG), 1 << 30)?;
    }
    if !all {
        return Ok(());
    }
    let stamp = dir.join(format!("complete-{VERSION}"));
    if stamp.exists() {
        return Ok(());
    }
    let mut csv = io::BufWriter::with_capacity(1 << 20, std::fs::File::create(dir.join("csv-20m.csv"))?);
    for i in 0..20_000_000u64 {
        writeln!(csv, "{i},value{},{}", i % 100, i % 7)?;
    }
    csv.flush()?;
    let mut json = io::BufWriter::new(std::fs::File::create(dir.join("one-line-100m.json"))?);
    json.write_all(b"{\"data\":\"")?;
    let block = [b'x'; 64 * 1024];
    let mut left = 100 * 1024 * 1024 - 11;
    while left > 0 {
        let n = left.min(block.len());
        json.write_all(&block[..n])?;
        left -= n;
    }
    json.write_all(b"\"}")?;
    json.flush()?;
    let enc = dir.join("mixed-encoding");
    std::fs::create_dir_all(&enc)?;
    std::fs::write(enc.join("utf8.txt"), "hello\r\n日本語\nemoji 🦀\r\n")?;
    std::fs::write(enc.join("utf16le.txt"), [0xff, 0xfe, b'h', 0, b'i', 0, 10, 0])?;
    std::fs::write(enc.join("windows1252.txt"), b"caf\xe9\r\n\x80\n")?;
    std::fs::write(enc.join("mixed-invalid.txt"), b"utf8\n\xff\xfe\x82\xa0\r\n")?;
    let tree = dir.join("tree-100k");
    for shard in 0..100 {
        let d = tree.join(format!("{shard:03}"));
        std::fs::create_dir_all(&d)?;
        for i in 0..1000 {
            std::fs::write(d.join(format!("file-{i:04}.txt")), b"small fixture\n")?;
        }
    }
    std::fs::write(stamp, VERSION)
}
