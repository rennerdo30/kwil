//! One-shot KW-10 measurement. Generates an ignored, file-backed 1 GiB fixture,
//! builds the sparse index on a worker, then decodes exactly 60 middle lines.
//! Run once: cargo run -p kwil-core-text --example encoding_viewport --release -j 4 --locked
use kwil_core_text::TextFile;
use kwil_core_text::encoding::{DisplaySegment, EncodedFile, Encoding};
use std::io::{self, BufWriter, Write};
use std::time::Instant;

#[cfg(windows)]
fn private_bytes() -> Option<usize> {
    use std::ffi::c_void;
    #[repr(C)]
    struct Counters {
        size: u32,
        faults: u32,
        peak_working: usize,
        working: usize,
        peak_paged: usize,
        paged: usize,
        peak_nonpaged: usize,
        nonpaged: usize,
        pagefile: usize,
        peak_pagefile: usize,
        private: usize,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn K32GetProcessMemoryInfo(process: *mut c_void, counters: *mut Counters, size: u32) -> i32;
    }
    // SAFETY: the struct matches PROCESS_MEMORY_COUNTERS_EX and is writable;
    // the pseudo handle belongs to this process and needs no closing.
    unsafe {
        let mut counters: Counters = std::mem::zeroed();
        counters.size = std::mem::size_of::<Counters>() as u32;
        if K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.size) == 0 {
            None
        } else {
            Some(counters.private)
        }
    }
}
#[cfg(not(windows))]
fn private_bytes() -> Option<usize> {
    None
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let fixture_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
    std::fs::create_dir_all(&fixture_dir)?;
    let path = fixture_dir.join(format!("encoding-sjis-1g-{}.txt", std::process::id()));
    let encoding = Encoding::for_label("shift_jis")?;
    let (encoded, _, errors) = encoding_rs::SHIFT_JIS.encode("日本語の文字を正しく表示します。 middle viewport ");
    assert!(!errors);
    let mut line = encoded.into_owned();
    assert!(line.len() < 64);
    line.resize(63, b' ');
    line.push(b'\n');
    let block = line.repeat((1 << 20) / 64);
    let fixture_bytes: u64 = 1 << 30;
    let mut writer = BufWriter::with_capacity(1 << 20, std::fs::File::create_new(&path)?);
    for _ in 0..fixture_bytes / block.len() as u64 {
        writer.write_all(&block)?;
    }
    writer.flush()?;
    drop(writer);
    drop(block);
    println!("pid={} fixture_bytes={fixture_bytes}", std::process::id());
    let private_before = private_bytes();
    let opened = Instant::now();
    let source = EncodedFile::new(TextFile::open(&path)?)?;
    assert_eq!(source.detection().encoding, encoding);
    let open_ms = opened.elapsed().as_secs_f64() * 1000.0;
    let indexed = Instant::now();
    source
        .index_in_background(|result| result.unwrap())
        .join()
        .map_err(|_| io::Error::other("index worker panicked"))?;
    let index_ms = indexed.elapsed().as_secs_f64() * 1000.0;
    let index = source.index().unwrap();
    let middle = index.line_count() / 2;
    let decoded = Instant::now();
    let viewport = source.decode_lines(middle, 60)?.unwrap();
    let viewport_ms = decoded.elapsed().as_secs_f64() * 1000.0;
    let raw_bytes = viewport.source.end - viewport.source.start;
    let text: String = viewport
        .segments
        .iter()
        .map(|segment| match segment {
            DisplaySegment::Text(text) => text.as_ref(),
            DisplaySegment::Invalid(_) => panic!("valid fixture"),
        })
        .collect();
    assert_eq!(text.lines().count(), 60);
    assert_eq!(raw_bytes, 60 * 64);
    assert_eq!(text, encoding_rs::SHIFT_JIS.decode_without_bom_handling(&line).0.repeat(60));
    println!(
        "open_ms={open_ms:.3} index_ms={index_ms:.3} viewport_ms={viewport_ms:.6} viewport_source_bytes={raw_bytes} decoded_bytes={} lines={} checkpoints={} index_allocated_bytes={} private_before={private_before:?} private_after={:?}",
        text.len(),
        index.line_count(),
        index.checkpoint_count(),
        index.allocated_bytes(),
        private_bytes()
    );
    drop(viewport);
    drop(source);
    std::fs::remove_file(path)?;
    Ok(())
}
