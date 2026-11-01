#![no_main]
#[path = "../../crates/core-text/tests/support/encodings.rs"]
mod model;

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    let Some((&selector, data)) = data.split_first() else {
        return;
    };
    let bytes = &data[..data.len().min(512)];
    model::preserve_utf8(bytes);
    let text = format!("K{}", String::from_utf8_lossy(bytes));
    let label = model::LABELS[selector as usize % model::LABELS.len()];
    model::roundtrip(&text, label, selector & 128 != 0);
    model::roundtrip(&model::representable_text(bytes, label), label, selector & 128 != 0);
});
