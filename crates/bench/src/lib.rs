//! Core-text measurements; GUI tracing and frame scripts belong to ui-gpu.

#[cfg(feature = "harness")]
pub mod fixtures;
#[cfg(feature = "harness")]
pub mod text;

/// Current private memory, excluding read-only file-backed page cache.
pub fn private_bytes() -> std::io::Result<u64> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let mut c: PROCESS_MEMORY_COUNTERS_EX = unsafe { std::mem::zeroed() };
        c.cb = std::mem::size_of_val(&c) as u32;
        if unsafe {
            GetProcessMemoryInfo(GetCurrentProcess(), (&mut c as *mut PROCESS_MEMORY_COUNTERS_EX).cast(), c.cb)
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(c.PrivateUsage as u64)
    }
    #[cfg(target_os = "linux")]
    {
        // Anonymous resident pages, including heap/stacks. File-backed clean
        // pages are OS page cache and explicitly outside the §3 budget.
        let smaps = std::fs::read_to_string("/proc/self/smaps")?;
        let mut total = 0;
        for line in smaps.lines() {
            if let Some(rest) = line.strip_prefix("Anonymous:") {
                total +=
                    rest.split_whitespace().next().unwrap_or("0").parse::<u64>().map_err(std::io::Error::other)? * 1024;
            }
        }
        Ok(total)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        Err(std::io::Error::other("private memory sampling supported on Windows/Linux only"))
    }
}
