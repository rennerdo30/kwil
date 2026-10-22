use kwil_bench::text::{JUMP, SCREEN, THREADS};
use kwil_core_text::TextFile;
use std::path::PathBuf;
use std::sync::OnceLock;

fn path() -> PathBuf {
    PathBuf::from(std::env::var_os("KWIL_FIXTURES").expect("generate fixtures on demand and set KWIL_FIXTURES"))
        .join(kwil_bench::fixtures::LOG)
}

fn main() {
    eprintln!(
        "Spec §3 target / hard: open+first ≤40/100 ms; index ≥2/1 GB/s; jump ≤1 frame/16 ms; clean private ≤64/256 MiB"
    );
    divan::main();
}

#[divan::bench(sample_count = 3, sample_size = 1)]
fn open_first_screen() {
    let file = TextFile::open(&path()).unwrap();
    divan::black_box(file.first_lines(SCREEN).unwrap());
}

#[divan::bench(sample_count = 3, sample_size = 1)]
fn line_index(bencher: divan::Bencher) {
    bencher
        .with_inputs(|| TextFile::open(&path()).unwrap())
        .input_counter(|f| divan::counter::BytesCount::new(f.len()))
        .bench_values(|f| {
            divan::black_box(f.build_index(THREADS).unwrap());
        });
}

#[divan::bench(sample_count = 3, sample_size = 1)]
fn jump_to_line_15m(bencher: divan::Bencher) {
    static FILE: OnceLock<TextFile> = OnceLock::new();
    let f = FILE.get_or_init(|| {
        let f = TextFile::open(&path()).unwrap();
        f.build_index(THREADS).unwrap();
        f
    });
    bencher.bench(|| assert!(divan::black_box(f.lines(JUMP, SCREEN).unwrap()).is_some()));
}

// Private memory is sampled in a fresh-process harness, rather than divan's
// shared process (which retains allocator state and the jump fixture).
// TODO: search, diff, piece-tree edit, palette, second-instance handoff.
