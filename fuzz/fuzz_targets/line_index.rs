#![no_main]
#[allow(dead_code)] // This target only needs the shared line-index oracle.
#[path = "../../crates/core-text/tests/support/edits.rs"]
mod model;

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    if data.len() < 3 {
        return;
    }
    let every = data[0] as u64;
    let selector = u16::from_le_bytes([data[1], data[2]]) as usize;
    let bytes = &data[3..data.len().min(4099)];
    model::check_index(bytes, every, selector % (bytes.len() + 1));
});
