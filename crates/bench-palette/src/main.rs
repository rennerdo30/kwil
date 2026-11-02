use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // A separate workspace and lockfile keep palette code/dependencies out of
    // the process whose core-text private memory is measured.
    if std::env::var("GITHUB_ACTIONS").as_deref() != Ok("true") {
        return Err("the 100k-file palette case runs only on GitHub Actions".into());
    }
    let root = std::env::args_os().nth(1).ok_or("usage: kwil-palette-bench <runner-temp-tree>")?;
    let root = Path::new(&root);
    kwil_bench_palette::fixtures::tree(root, 100, 1000)?;
    println!("{}", kwil_bench_palette::palette::measure(root, 100_000)?);
    Ok(())
}
