//! Process creation time includes the executable loader, unlike Instant::now at main.
use std::sync::Mutex;

#[derive(Debug, Default)]
pub struct Trace {
    first: Mutex<Option<f64>>,
    index: Mutex<Option<f64>>,
}

impl Trace {
    pub fn launch() -> Self {
        eprintln!("t_launch={:.3} ms", process_age_ms());
        Self::default()
    }

    pub fn first_frame(&self) {
        let mut first = self.first.lock().unwrap();
        if first.is_none() {
            let ms = process_age_ms();
            *first = Some(ms);
            eprintln!("t_first_frame={ms:.3} ms");
        }
    }

    pub fn index_done(&self) {
        let mut index = self.index.lock().unwrap();
        if index.is_none() {
            *index = Some(process_age_ms());
        }
    }

    pub fn finish(&self) {
        let first = self.first.lock().unwrap().expect("no successful first frame");
        let index = self.index.lock().unwrap().expect("index completion missing");
        eprintln!("t_index_done={index:.3} ms");
        eprintln!("startup: t_launch → t_first_frame={first:.3} → t_index_done={index:.3} ms");
    }
}

#[cfg(windows)]
pub fn process_age_ms() -> f64 {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let z = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut created, mut exit, mut kernel, mut user, mut now) = (z, z, z, z, z);
    unsafe {
        assert_ne!(GetProcessTimes(GetCurrentProcess(), &mut created, &mut exit, &mut kernel, &mut user), 0);
        GetSystemTimePreciseAsFileTime(&mut now);
    }
    let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
    t(now).saturating_sub(t(created)) as f64 / 10_000.0
}

#[cfg(target_os = "linux")]
pub fn process_age_ms() -> f64 {
    let stat = std::fs::read_to_string("/proc/self/stat").expect("process stat");
    // comm can contain spaces and parentheses; fields after its final ')' start at field 3.
    let start: f64 = stat.rsplit_once(')').unwrap().1.split_whitespace().nth(19).unwrap().parse().unwrap();
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    assert_eq!(unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut now) }, 0);
    (now.tv_sec as f64 + now.tv_nsec as f64 / 1e9 - start / ticks).max(0.0) * 1000.0
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn process_age_ms() -> f64 {
    // macOS: proc_pidinfo gives process creation wall time (microseconds).
    #[cfg(target_os = "macos")]
    {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let n = unsafe {
            libc::proc_pidinfo(
                libc::getpid(),
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                std::mem::size_of_val(&info) as i32,
            )
        };
        assert_eq!(n as usize, std::mem::size_of_val(&info));
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64();
        (now - info.pbi_start_tvsec as f64 - info.pbi_start_tvusec as f64 / 1e6) * 1000.0
    }
    #[cfg(not(target_os = "macos"))]
    {
        panic!("process creation clock unsupported on this platform")
    }
}
