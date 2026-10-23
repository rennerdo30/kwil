//! Process containment for one owned formatter invocation.
use std::{
    io,
    process::{Child, Command},
};

#[cfg(unix)]
pub struct Tree {
    pgid: libc::pid_t,
}

#[cfg(unix)]
impl Tree {
    pub fn spawn(command: &mut Command) -> io::Result<(Child, Self)> {
        use std::os::unix::process::CommandExt;
        // std calls setpgid(0, 0) in the child before exec, so no wrapper can
        // create a descendant outside containment during process startup.
        let child = command.process_group(0).spawn()?;
        let pgid = child.id() as libc::pid_t;
        Ok((child, Self { pgid }))
    }

    pub fn finished(&self, child: &mut Child) -> io::Result<bool> {
        // Observe without reaping: the leader's reserved PID prevents this
        // process-group ID from being reused until killpg has run.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result =
            unsafe { libc::waitid(libc::P_PID, child.id(), &mut info, libc::WEXITED | libc::WNOHANG | libc::WNOWAIT) };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { info.si_pid() } != 0)
    }

    pub fn terminate(&mut self) -> io::Result<()> {
        if self.pgid == 0 {
            return Ok(());
        }
        let pgid = self.pgid;
        self.pgid = 0;
        // Only the group established for this exact child; never our own group.
        if unsafe { libc::killpg(pgid, libc::SIGKILL) } != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH)
                && !(error.raw_os_error() == Some(libc::EPERM) && group_has_no_live_members(pgid)?)
            {
                return Err(error);
            }
        }
        Ok(())
    }
}

// EPERM normally means containment failed, and must not be ignored. XNU also
// returns it when killpg's zombie filter leaves no signalable processes. Inspect
// only our reserved group, before external::run reaps its leader. An empty or
// zombie-only group cannot fork between the inspection and the reap.
#[cfg(target_os = "macos")]
fn group_has_no_live_members(pgid: libc::pid_t) -> io::Result<bool> {
    let mut pids: [libc::pid_t; 4096] = [0; 4096];
    unsafe { *libc::__error() = 0 };
    let count = unsafe { libc::proc_listpgrppids(pgid, pids.as_mut_ptr().cast(), std::mem::size_of_val(&pids) as i32) };
    if count < 0 || (count == 0 && io::Error::last_os_error().raw_os_error() != Some(0)) {
        return Err(io::Error::last_os_error());
    }
    if count as usize >= pids.len() {
        return Err(io::Error::other("formatter group inspection exceeded PID capacity"));
    }
    for &pid in &pids[..count as usize] {
        if pid == 0 {
            continue;
        }
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of_val(&info) as i32;
        let read = unsafe {
            libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&mut info as *mut libc::proc_bsdinfo).cast(), size)
        };
        if read == 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            continue; // It exited between enumeration and inspection.
        }
        if read != size {
            return Err(io::Error::other("cannot inspect formatter group member"));
        }
        if info.pbi_pgid == pgid as u32 && info.pbi_status != libc::SZOMB {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn group_has_no_live_members(_pgid: libc::pid_t) -> io::Result<bool> {
    // Other supported kernels report ESRCH for an empty group. Keep EPERM fatal.
    Ok(false)
}

#[cfg(windows)]
pub struct Tree {
    job: std::os::windows::io::OwnedHandle,
}

#[cfg(windows)]
impl Tree {
    pub fn spawn(command: &mut Command) -> io::Result<(Child, Self)> {
        use std::os::windows::{
            io::{AsRawHandle, FromRawHandle, OwnedHandle},
            process::CommandExt,
        };
        use windows_sys::Win32::System::{
            JobObjects::*,
            Threading::{CREATE_NO_WINDOW, CREATE_SUSPENDED},
        };
        // SAFETY: Win32 output structures are zero-initializable. The job handle
        // is private, non-inherited, and immediately put under RAII ownership.
        let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = unsafe { OwnedHandle::from_raw_handle(raw) };
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if unsafe {
            SetInformationJobObject(
                job.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let tree = Self { job };
        // Suspended launch closes the spawn/assign race even for fast wrappers.
        let mut child = command.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED).spawn()?;
        let attach = if unsafe { AssignProcessToJobObject(tree.job.as_raw_handle(), child.as_raw_handle()) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            resume_owned_thread(child.id())
        };
        if let Err(error) = attach {
            let killed = child.kill();
            let waited = child.wait();
            return Err(io::Error::other(format!(
                "formatter PID {} containment: {error}; kill={}, reap={}",
                child.id(),
                killed.is_ok(),
                waited.is_ok()
            )));
        }
        Ok((child, tree))
    }

    pub fn finished(&self, child: &mut Child) -> io::Result<bool> {
        Ok(child.try_wait()?.is_some())
    }

    pub fn terminate(&mut self) -> io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        if unsafe { windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job.as_raw_handle(), 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(windows)]
fn resume_owned_thread(pid: u32) -> io::Result<()> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::{
        Foundation::INVALID_HANDLE_VALUE,
        System::{Diagnostics::ToolHelp::*, Threading::*},
    };
    // Rust 1.99 does not expose Child's primary-thread handle on stable. Take a
    // single snapshot and open only the suspended thread of our retained child.
    let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let snapshot = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of_val(&entry) as u32;
    let mut found = unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) };
    while found != 0 {
        if entry.th32OwnerProcessID == pid {
            let raw = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if raw.is_null() {
                return Err(io::Error::last_os_error());
            }
            let thread = unsafe { OwnedHandle::from_raw_handle(raw) };
            if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
                return Err(io::Error::last_os_error());
            }
            return Ok(());
        }
        found = unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) };
    }
    Err(io::Error::other("owned suspended formatter thread missing"))
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = self.terminate();
    }
}
