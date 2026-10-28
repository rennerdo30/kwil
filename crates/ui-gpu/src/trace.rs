//! Startup marks are opt-in and include Windows loader time.
pub(crate) fn mark(label: &str) {
    if std::env::var_os("KWIL_TRACE_STARTUP").is_some() {
        eprintln!("kwil-startup {label} {:.3} ms", age_ms());
    }
}

#[cfg(windows)]
fn age_ms() -> f64 {
    use windows_sys::Win32::{
        Foundation::FILETIME,
        System::{
            SystemInformation::GetSystemTimePreciseAsFileTime,
            Threading::{GetCurrentProcess, GetProcessTimes},
        },
    };
    let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut start, mut end, mut kernel, mut user, mut now) = (zero, zero, zero, zero, zero);
    unsafe {
        GetProcessTimes(GetCurrentProcess(), &mut start, &mut end, &mut kernel, &mut user);
        GetSystemTimePreciseAsFileTime(&mut now);
    }
    let ticks = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
    ticks(now).saturating_sub(ticks(start)) as f64 / 10_000.0
}

#[cfg(not(windows))]
fn age_ms() -> f64 {
    // Portable relative marks; Linux/macOS launch timestamps belong to the harness.
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_secs_f64() * 1000.0
}
