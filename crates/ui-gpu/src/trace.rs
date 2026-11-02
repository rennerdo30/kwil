//! Startup marks are opt-in and include Windows loader time.
pub fn mark(label: &str) {
    if std::env::var_os("KWIL_TRACE_STARTUP").is_some() {
        #[cfg(not(feature = "frame-harness"))]
        eprintln!("kwil-startup {label} {:.3} ms", age_ms());
        #[cfg(feature = "frame-harness")]
        event(label);
    }
}

#[cfg(feature = "frame-harness")]
fn event(label: &str) {
    use std::io::Write;
    let ns = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    #[cfg(windows)]
    let Some(mut sink) = trace_sink().as_ref().map(|sink| sink.lock().unwrap()) else { return };
    #[cfg(not(windows))]
    let mut sink = std::io::stderr().lock();
    let _ = writeln!(sink, "kwil-startup {label} {:.3} ms", age_ms());
    let _ = writeln!(sink, "kwil-speed\t{}\t{ns}\t{label}", std::process::id());
}

// GUI readiness intentionally releases stdio on Windows. The opt-in trace must
// keep its own non-inheritable handle to observe content AFTER that readiness.
#[cfg(all(windows, feature = "frame-harness"))]
fn trace_sink() -> &'static Option<std::sync::Mutex<std::fs::File>> {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use windows_sys::Win32::{
        Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle},
        System::{
            Console::{GetStdHandle, STD_ERROR_HANDLE},
            Threading::GetCurrentProcess,
        },
    };
    static SINK: std::sync::OnceLock<Option<std::sync::Mutex<std::fs::File>>> = std::sync::OnceLock::new();
    SINK.get_or_init(|| {
        let mut duplicate = std::ptr::null_mut();
        let process = unsafe { GetCurrentProcess() };
        if unsafe {
            DuplicateHandle(
                process,
                GetStdHandle(STD_ERROR_HANDLE),
                process,
                &mut duplicate,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        } == 0
        {
            return None;
        }
        Some(std::sync::Mutex::new(std::fs::File::from(unsafe { OwnedHandle::from_raw_handle(duplicate.cast()) })))
    })
}

// Only the isolated benchmark's known first-row sentinels are recorded. A
// chrome frame or a retained/loading/covered body cannot claim file content.
#[cfg(feature = "frame-harness")]
pub(super) fn content_mark(row: Option<&[u8]>, drawn: bool, blocked: bool) -> Option<&'static str> {
    if !drawn || blocked {
        return None;
    }
    match row? {
        b"kwil-speed-file" => Some("file content presented"),
        b"kwil-speed-forward" => Some("forward content presented"),
        _ => None,
    }
}

#[cfg(feature = "frame-harness")]
pub(super) fn presented(label: Option<&'static str>) {
    use std::sync::atomic::{AtomicU8, Ordering};
    static SEEN: AtomicU8 = AtomicU8::new(0);
    if std::env::var_os("KWIL_TRACE_STARTUP").is_some()
        && let Some(label) = label
    {
        let bit = if label == "file content presented" { 1 } else { 2 };
        if SEEN.fetch_or(bit, Ordering::Relaxed) & bit == 0 {
            mark(label);
        }
    }
}

#[cfg(all(test, feature = "frame-harness"))]
mod tests {
    use super::*;
    #[test]
    fn content_timing_requires_the_fixture_row_to_be_drawn_and_uncovered() {
        for (drawn, blocked) in [(false, false), (true, true), (false, true)] {
            assert_eq!(content_mark(Some(b"kwil-speed-file"), drawn, blocked), None);
        }
        assert_eq!(content_mark(None, true, false), None);
        assert_eq!(content_mark(Some(b"other file"), true, false), None);
        assert_eq!(content_mark(Some(b"kwil-speed-file"), true, false), Some("file content presented"));
        assert_eq!(content_mark(Some(b"kwil-speed-forward"), true, false), Some("forward content presented"));
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
