//! Platform bits the supervisor process owns for itself, as opposed to the
//! per-service mechanics in the process runtime: the job object that outlives
//! a crash, and spawning our own binary as a detached supervisor.

use anyhow::Context;

#[cfg(unix)]
pub fn spawn_detached(cmd: &mut std::process::Command) -> anyhow::Result<std::process::Child> {
    cmd.spawn().context("spawn detached supervisor")
}

#[cfg(windows)]
pub fn spawn_detached(cmd: &mut std::process::Command) -> anyhow::Result<std::process::Child> {
    use std::os::windows::process::CommandExt;
    // DETACHED_PROCESS: no console attachment.
    // CREATE_NEW_PROCESS_GROUP: own group so it doesn't share our Ctrl+C.
    // CREATE_BREAKAWAY_FROM_JOB: escape any job we (or our parent) sit in that
    // has KILL_ON_JOB_CLOSE, so the supervisor outlives this CLI.
    const DETACHED_PROCESS: u32 = 0x00000008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x01000000;
    const ERROR_ACCESS_DENIED: i32 = 5;

    cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB);
    match cmd.spawn() {
        Ok(c) => Ok(c),
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) => {
            // The outer job (a shell wrapper, a terminal multiplexer, or this
            // session's harness) does not allow CREATE_BREAKAWAY_FROM_JOB.
            // Retry without it: the supervisor will be assigned to that job
            // and may die when it closes if KILL_ON_JOB_CLOSE is set.
            eprintln!(
                "arig: outer job denied breakaway; supervisor will inherit it (closing this shell may kill it)"
            );
            cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
            cmd.spawn()
                .context("spawn detached supervisor (no breakaway)")
        }
        Err(e) => Err(anyhow::Error::new(e).context("spawn detached supervisor")),
    }
}

#[cfg(windows)]
pub mod win {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    /// RAII guard that holds the job object handle. Children assigned to this
    /// job are killed when the handle is closed (including on parent crash).
    pub struct JobGuard {
        handle: HANDLE,
    }

    impl JobGuard {
        pub fn new() -> anyhow::Result<Self> {
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if handle.is_null() {
                    anyhow::bail!("CreateJobObjectW failed");
                }

                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let ok = SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if ok == 0 {
                    CloseHandle(handle);
                    anyhow::bail!("SetInformationJobObject failed");
                }

                // Assign ourselves so children inherit the job
                AssignProcessToJobObject(handle, GetCurrentProcess());

                Ok(Self { handle })
            }
        }
    }

    impl Drop for JobGuard {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.handle);
            }
        }
    }
}
