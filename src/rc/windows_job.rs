//! Windows process-tree ownership for harnesses and fenced settlement commands.
//!
//! Killing the direct `agit` child is insufficient: git may already be waiting
//! on `git-remote-https` (or another helper), and that grandchild can finish a
//! write after the websocket feature lease disappears. A Job Object owns the
//! whole descendant tree. The process is born suspended so there is no gap in
//! which it can create an unowned child before assignment.

use std::io;
use std::mem::size_of;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
    QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
};

struct OwnedHandle(HANDLE);

// Win32 kernel handles may be used and closed from any thread.
unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

/// Kill-on-close owner for one child process and every descendant it creates
/// (including descendants that create nested jobs on Windows 8+).
pub(crate) struct Job {
    handle: OwnedHandle,
}

impl Job {
    pub(crate) fn new() -> io::Result<Self> {
        let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = Self {
            handle: OwnedHandle(raw),
        };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                job.handle.0,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if configured == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    /// The suspended creation flag closes the spawn→assignment race. The
    /// child's first instruction runs only after [`attach_and_resume`] succeeds.
    pub(crate) fn configure(command: &mut tokio::process::Command) {
        command.creation_flags(CREATE_SUSPENDED);
    }

    pub(crate) fn attach_and_resume(&self, child: &tokio::process::Child) -> io::Result<()> {
        let process = child
            .raw_handle()
            .ok_or_else(|| io::Error::other("child exited before Job assignment"))?;
        let assigned = unsafe { AssignProcessToJobObject(self.handle.0, process.cast()) };
        if assigned == 0 {
            return Err(io::Error::last_os_error());
        }
        let process_id = child
            .id()
            .ok_or_else(|| io::Error::other("child exited before thread resume"))?;
        resume_primary_thread(process_id)
    }

    pub(crate) fn active_processes(&self) -> io::Result<u32> {
        let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        let queried = unsafe {
            QueryInformationJobObject(
                self.handle.0,
                JobObjectBasicAccountingInformation,
                (&raw mut accounting).cast(),
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        };
        if queried == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(accounting.ActiveProcesses)
    }

    pub(crate) fn terminate(&self) -> io::Result<()> {
        let terminated = unsafe { TerminateJobObject(self.handle.0, 1) };
        if terminated == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(crate) async fn wait_empty(&self) -> io::Result<()> {
        loop {
            if self.active_processes()? == 0 {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // The ordinary path has already reaped the direct child and observed
        // ActiveProcesses == 0. This is the cancellation/panic backstop: do not
        // close the owner while its tree can still execute.
        if self.active_processes().is_ok_and(|active| active > 0) {
            let _ = self.terminate();
            while self.active_processes().is_ok_and(|active| active > 0) {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        // `OwnedHandle` closes last. KILL_ON_JOB_CLOSE is a second fail-safe if
        // an unexpected accounting query error made the loop unprovable.
    }
}

fn resume_primary_thread(process_id: u32) -> io::Result<()> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let snapshot = OwnedHandle(snapshot);
    let mut entry = THREADENTRY32 {
        dwSize: size_of::<THREADENTRY32>() as u32,
        ..THREADENTRY32::default()
    };
    if unsafe { Thread32First(snapshot.0, &raw mut entry) } == 0 {
        return Err(io::Error::last_os_error());
    }
    loop {
        if entry.th32OwnerProcessID == process_id {
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if thread.is_null() {
                return Err(io::Error::last_os_error());
            }
            let thread = OwnedHandle(thread);
            let previous = unsafe { ResumeThread(thread.0) };
            if previous == u32::MAX {
                return Err(io::Error::last_os_error());
            }
            if previous != 1 {
                return Err(io::Error::other(format!(
                    "child primary thread had unexpected suspend count {previous}"
                )));
            }
            return Ok(());
        }
        if unsafe { Thread32Next(snapshot.0, &raw mut entry) } == 0 {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "child primary thread was not present in the system snapshot",
            ));
        }
    }
}
