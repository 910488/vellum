//! Keeps the bridge's children from outliving it.
//!
//! Codex Desktop restarts its App Server by killing the bridge, not by closing
//! stdin, so the bridge's own cleanup never runs. On macOS the Official and
//! Enhanced cores were then reparented to launchd and kept holding thread
//! writer locks, and Desktop showed the thread as "open in another app".
//!
//! Windows: the bridge joins a kill-on-close Job Object, so every descendant
//! dies with it however it ends. Unix: each child leads its own process group,
//! and SIGTERM/SIGINT/SIGHUP stop those groups before the bridge exits. A
//! SIGKILLed bridge on Unix still leaves them; nothing in-process can catch it.

/// Call once, before spawning anything.
pub(crate) fn bind_children_to_this_process() {
    #[cfg(target_os = "windows")]
    windows::join_kill_on_close_job();
    #[cfg(unix)]
    unix::install_signal_handlers();
}

/// Puts the child in its own process group so its own children (node REPLs,
/// code-mode hosts) are stopped with it. No-op on Windows, where the job
/// already covers them.
pub(crate) fn own_process_group(command: &mut std::process::Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(not(unix))]
    let _ = command;
}

pub(crate) fn own_tokio_process_group(command: &mut tokio::process::Command) {
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(not(unix))]
    let _ = command;
}

/// Tracks a child spawned with [`own_process_group`] until it is dropped,
/// which stops the child's whole group.
pub(crate) struct ProcessGroupGuard {
    #[cfg_attr(not(unix), allow(dead_code))]
    pid: u32,
}

impl ProcessGroupGuard {
    pub(crate) fn adopt(pid: u32) -> Self {
        #[cfg(unix)]
        unix::register(pid);
        Self { pid }
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        unix::stop_group(self.pid);
    }
}

#[cfg(target_os = "windows")]
mod windows {
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    pub(super) fn join_kill_on_close_job() {
        // SAFETY: plain Win32 calls on a job handle this function owns. The
        // handle is deliberately never closed: the kernel closes it when the
        // bridge exits, and that close is what kills the job.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return;
            }
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            // Breakaway stays allowed so a descendant that asks for it (a
            // sandbox helper, an updater) still starts instead of failing.
            limits.BasicLimitInformation.LimitFlags =
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
            let configured = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of!(limits).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if configured == 0 || AssignProcessToJobObject(job, GetCurrentProcess()) == 0 {
                eprintln!(
                    "vellum-codex-app-server: cannot bind children to the bridge: {}",
                    std::io::Error::last_os_error()
                );
            }
        }
    }
}

#[cfg(unix)]
mod unix {
    use std::sync::atomic::{AtomicI32, Ordering};

    // A fixed table so the signal handler never allocates or locks. The bridge
    // runs at most two cores, a relay and a delegated command.
    static GROUPS: [AtomicI32; 8] = [const { AtomicI32::new(0) }; 8];

    pub(super) fn register(pid: u32) {
        // Never 0: `kill(-0)` would signal the bridge's own group.
        let Ok(pid @ 1..) = i32::try_from(pid) else {
            return;
        };
        for slot in &GROUPS {
            if slot
                .compare_exchange(0, pid, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return;
            }
        }
    }

    pub(super) fn stop_group(pid: u32) {
        let Ok(pid @ 1..) = i32::try_from(pid) else {
            return;
        };
        for slot in &GROUPS {
            if slot
                .compare_exchange(pid, 0, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                // SAFETY: signals a process group this bridge created.
                unsafe { libc::kill(-pid, libc::SIGTERM) };
                return;
            }
        }
    }

    extern "C" fn on_terminate(signal: libc::c_int) {
        for slot in &GROUPS {
            let pid = slot.swap(0, Ordering::SeqCst);
            if pid > 0 {
                // SAFETY: kill and _exit are async-signal-safe.
                unsafe { libc::kill(-pid, libc::SIGTERM) };
            }
        }
        unsafe { libc::_exit(128 + signal) };
    }

    pub(super) fn install_signal_handlers() {
        for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            // SAFETY: installs a handler that only touches atomics and calls
            // async-signal-safe functions.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = on_terminate as extern "C" fn(libc::c_int) as usize;
                libc::sigemptyset(&mut action.sa_mask);
                libc::sigaction(signal, &action, std::ptr::null_mut());
            }
        }
    }
}
