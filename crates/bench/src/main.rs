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
        _ => return Err("usage: kwil-bench generate <dir> [--all] | measure <log>".into()),
    }
    Ok(())
}
