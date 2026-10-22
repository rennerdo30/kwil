//! A single admitted measurement, with fixture generation outside the timed region.
use std::{hint::black_box, time::Instant};

fn main() {
    const TARGET: usize = 5 * 1024 * 1024;
    let paragraph = "A paragraph with *emphasis*, **strong**, ~~strike~~ and [a link](https://example.org). ";
    let mut section = String::from(
        "# Example\n\n> A quoted paragraph.\n\n- [x] A task\n- [ ] Another task\n\n| Left | Right |\n| :--- | ---: |\n| a | b |\n\n```rust\nlet value = 42;\n```\n\n",
    );
    section.push_str(paragraph);
    for _ in 0..32 {
        section
            .push_str("This is plain body text in a moderately long paragraph, with ordinary prose and punctuation. ");
    }
    section.push_str("\n\n");
    let document = section.repeat(TARGET.div_ceil(section.len()));
    let start = Instant::now();
    let model = kwil_core_markdown::parse(black_box(&document)).expect("admitted fixture");
    let elapsed = start.elapsed();
    println!(
        "bytes={} blocks={} parse_ms={:.3} MiB_per_second={:.2}",
        document.len(),
        model.blocks.len(),
        elapsed.as_secs_f64() * 1000.0,
        document.len() as f64 / (1024.0 * 1024.0) / elapsed.as_secs_f64()
    );
    black_box(model);
}
