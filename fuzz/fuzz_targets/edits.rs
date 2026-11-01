#![no_main]
#[path = "../../crates/core-text/tests/support/edits.rs"]
mod model;

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    // Bounded input/work per invocation; edits include undo and redo commands.
    let ops: Vec<_> = data[..data.len().min(768)]
        .chunks(16)
        .map(|chunk| {
            let a = chunk.get(1).copied().unwrap_or(0) as u16;
            let b = chunk.get(2).copied().unwrap_or(0) as u16;
            let text = String::from_utf8_lossy(chunk.get(3..).unwrap_or_default()).into_owned();
            (chunk[0] % 5, a, b, text)
        })
        .collect();
    model::edit_sequence("α\r\nseed\n", &ops);
});
