use kwil_core_diff::{CancellationToken, LineSource, Options, compare};
use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Write},
    time::Instant,
};
struct Lines(Vec<Vec<u8>>);
impl LineSource for Lines {
    fn line_count(&self) -> usize {
        self.0.len()
    }
    fn line(&self, i: usize) -> &[u8] {
        &self.0[i]
    }
}
fn main() -> std::io::Result<()> {
    // Explicit measurement fixture, not a production file loader. Files >1 MB stay ignored.
    let root = std::path::Path::new("fixtures").join(format!("diff-100k-{}", std::process::id()));
    fs::create_dir_all(&root)?;
    let (mut a, mut b) = (File::create(root.join("left.txt"))?, File::create(root.join("right.txt"))?);
    for i in 0..100_000 {
        writeln!(a, "record {i:06}: unchanged text for comparison")?;
        if i % 100 == 50 {
            writeln!(b, "record {i:06}: changed text for comparison")?;
        } else {
            writeln!(b, "record {i:06}: unchanged text for comparison")?;
        }
    }
    drop((a, b));
    let load = |name| -> std::io::Result<Lines> {
        Ok(Lines(BufReader::new(File::open(root.join(name))?).split(b'\n').collect::<Result<_, _>>()?))
    };
    let (left, right) = (load("left.txt")?, load("right.txt")?);
    let start = Instant::now();
    let result = compare(&left, &right, &Options::default(), &CancellationToken::default()).expect("admitted fixture");
    let elapsed = start.elapsed();
    assert_eq!(result.hunks.len(), 1000);
    assert_eq!(result.row_count(), 100_000);
    println!(
        "pid={} lines=100000 changed=1000 algorithm=histogram elapsed_ms={:.3} hunks={} rows={}",
        std::process::id(),
        elapsed.as_secs_f64() * 1000.,
        result.hunks.len(),
        result.row_count()
    );
    assert!(elapsed.as_millis() <= 500, "exceeds spec hard limit");
    Ok(())
}
