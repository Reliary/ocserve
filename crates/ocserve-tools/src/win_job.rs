#![cfg(windows)]

//! Windows process-tree containment for the bash tool.
//!
//! ## The bug this prevents
//!
//! On Unix the bash tool spawns the child as a process-group leader
//! (`process_group(0)`) and the timeout kill uses `killpg`, which sweeps the
//! whole tree. Windows has no process groups. `Child::kill` terminates one
//! process, so a timed-out `cmd /c something` would leave its grandchildren
//! running — the exact zombie class fixed on 2026-10-08, resurrected by the
//! platform.
//!
//! A **Job Object** with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` is the Windows
//! equivalent, and it is strictly stronger than `taskkill /T`: when the last
//! handle closes (including because the server crashed), Windows terminates
//! every process in the job. `taskkill` cannot do that.
//!
//! ## Why `CREATE_SUSPENDED`
//!
//! `std::process::Command` has no way to supply a
//! `PROC_THREAD_ATTRIBUTE_JOB_LIST` attribute list, so the process exists
//! before we can assign it. A process that is running can fork a grandchild in
//! that window, and the grandchild escapes the job.
//!
//! So the child is created **suspended**: it cannot execute a single
//! instruction, and therefore cannot create a child, until `finish` assigns the
//! job and resumes it. The window is closed by construction rather than
//! narrowed.
//!
//! The main thread handle is not exposed by `std::process::Child`, so resume
//! goes through `Thread32First`/`OpenThread`/`ResumeThread`. If any step fails
//! the child is terminated and the spawn reports an error — never a silently
//! uncontained process.

use anyhow::{Result, bail};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
};

/// `CREATE_NO_WINDOW` is deliberately not set: the child's stdio is piped, and
/// std already suppresses a console where it matters.
const SUSPENDED: u32 = CREATE_SUSPENDED;

pub struct WinJob {
    job: HANDLE,
}

// The job handle is a process-wide kernel object used only through `&self`.
unsafe impl Send for WinJob {}
unsafe impl Sync for WinJob {}

impl WinJob {
    /// Create the job and mark the command suspended. Must be called BEFORE
    /// `spawn`, and `finish` must be called immediately after.
    pub fn attach(cmd: &mut std::process::Command) -> Result<WinJob> {
        cmd.creation_flags(SUSPENDED);
        // Safety: standard job creation with null attributes and name.
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            bail!("CreateJobObjectW: {}", std::io::Error::last_os_error());
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // Safety: `job` is valid; struct and size are correct.
        let ok = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            let e = std::io::Error::last_os_error();
            unsafe { CloseHandle(job) };
            bail!("SetInformationJobObject: {e}");
        }
        Ok(WinJob { job })
    }

    /// Assign the suspended child to the job and resume it.
    pub fn finish(&self, child: &mut std::process::Child) -> Result<()> {
        let process = child.as_raw_handle() as HANDLE;
        // Safety: `process` is a live handle owned by `child`.
        if unsafe { AssignProcessToJobObject(self.job, process) } == 0 {
            let e = std::io::Error::last_os_error();
            // Never leave a suspended, uncontained process behind.
            let _ = child.kill();
            bail!("AssignProcessToJobObject: {e}");
        }
        if let Err(e) = resume_main_thread(child.id()) {
            let _ = child.kill();
            self.terminate();
            return Err(e);
        }
        Ok(())
    }

    /// Terminate every process in the job.
    pub fn terminate(&self) {
        // Safety: `job` is valid for the lifetime of `self`.
        unsafe {
            TerminateJobObject(self.job, 1);
        }
    }
}

impl Drop for WinJob {
    fn drop(&mut self) {
        // Safety: owned handle, closed exactly once. Closing is also what
        // triggers KILL_ON_JOB_CLOSE for any process still in the job.
        unsafe {
            if !self.job.is_null() && self.job != INVALID_HANDLE_VALUE {
                CloseHandle(self.job);
            }
        }
        self.job = std::ptr::null_mut();
    }
}

/// Resume every thread of `pid` that the snapshot reports as suspended.
///
/// A freshly created suspended process has exactly one thread; the loop is
/// written for the general case so a partially-resumed process cannot result
/// from an unusual loader state. `ResumeThread` on an already-running thread is
/// harmless (it decrements a suspend count that is already zero).
fn resume_main_thread(pid: u32) -> Result<()> {
    // Safety: standard toolhelp snapshot; the handle is closed below.
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snap == INVALID_HANDLE_VALUE {
        bail!(
            "CreateToolhelp32Snapshot: {}",
            std::io::Error::last_os_error()
        );
    }
    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
    let mut resumed = 0usize;
    // Safety: snapshot handle is valid; `entry` is sized as the API requires.
    let mut ok = unsafe { Thread32First(snap, &mut entry) };
    while ok != 0 {
        if entry.th32OwnerProcessID == pid {
            // Safety: opening by thread id; the handle is closed immediately.
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if !thread.is_null() {
                // Safety: `thread` is a valid handle with the resume right.
                let prev = unsafe { ResumeThread(thread) };
                unsafe { CloseHandle(thread) };
                if prev == u32::MAX {
                    let e = std::io::Error::last_os_error();
                    unsafe { CloseHandle(snap) };
                    bail!("ResumeThread({}): {e}", entry.th32ThreadID);
                }
                resumed += 1;
            }
        }
        // Safety: same snapshot and entry, advanced by the API.
        ok = unsafe { Thread32Next(snap, &mut entry) };
    }
    // Safety: snapshot handle owned here.
    unsafe { CloseHandle(snap) };
    if resumed == 0 {
        bail!(
            "no thread found for pid {pid}; the child stayed suspended. \
             This should be impossible for a just-created process."
        );
    }
    Ok(())
}
