use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("generate") => kwil_bench::fixtures::generate(
            Path::new(args.get(2).ok_or("fixture directory required")?),
            args.iter().any(|a| a == "--all"),
        )?,
        Some("measure") => {
            println!("{}", kwil_bench::text::measure(Path::new(args.get(2).ok_or("log file required")?))?)
        }
        Some("palette") => {
            // The full tree is for hosted runners only, never the owner's PC.
            if std::env::var("GITHUB_ACTIONS").as_deref() != Ok("true") {
                return Err("the 100k-file palette case runs only on GitHub Actions".into());
            }
            let root = Path::new(args.get(2).ok_or("runner temp tree directory required")?);
            kwil_bench::fixtures::tree(root, 100, 1000)?;
            println!("{}", kwil_bench::palette::measure(root, 100_000)?);
        }
        _ => return Err("usage: kwil-bench generate <dir> [--all] | measure <log> | palette <runner-temp-tree>".into()),
    }
    Ok(())
}
