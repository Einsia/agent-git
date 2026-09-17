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
    CREATE_NO_WINDOW, CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
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
pub struct Job {
    handle: OwnedHandle,
    drain_on_drop: bool,
    #[cfg(any(test, feature = "test-support"))]
    nonempty_accounting: std::sync::atomic::AtomicBool,
    #[cfg(any(test, feature = "test-support"))]
    pending_nonempty_queries: std::sync::atomic::AtomicU32,
    #[cfg(any(test, feature = "test-support"))]
    termination_requests: std::sync::atomic::AtomicU32,
}

impl Job {
    pub fn new() -> io::Result<Self> {
        let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = Self {
            handle: OwnedHandle(raw),
            drain_on_drop: true,
            #[cfg(any(test, feature = "test-support"))]
            nonempty_accounting: std::sync::atomic::AtomicBool::new(false),
            #[cfg(any(test, feature = "test-support"))]
            pending_nonempty_queries: std::sync::atomic::AtomicU32::new(0),
            #[cfg(any(test, feature = "test-support"))]
            termination_requests: std::sync::atomic::AtomicU32::new(0),
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
    pub fn configure(command: &mut tokio::process::Command) {
        command.creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW);
    }

    pub fn attach_and_resume(&self, child: &tokio::process::Child) -> io::Result<()> {
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

    pub fn active_processes(&self) -> io::Result<u32> {
        #[cfg(any(test, feature = "test-support"))]
        if self
            .nonempty_accounting
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return Ok(1);
        }
        #[cfg(any(test, feature = "test-support"))]
        if self
            .pending_nonempty_queries
            .fetch_update(
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
                |pending| pending.checked_sub(1),
            )
            .is_ok()
        {
            return Ok(1);
        }
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

    pub fn terminate(&self) -> io::Result<()> {
        #[cfg(any(test, feature = "test-support"))]
        self.termination_requests
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let terminated = unsafe { TerminateJobObject(self.handle.0, 1) };
        if terminated == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Request termination and close the kill-on-close owner without draining accounting.
    /// The result reports only the termination request, never proof that the processes exited.
    /// A bounded caller must preserve an unverified-cleanup error after its deadline expires.
    pub fn terminate_and_close(mut self) -> io::Result<()> {
        let requested = self.terminate();
        self.drain_on_drop = false;
        requested
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn force_nonempty_accounting(&self) {
        self.nonempty_accounting
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    pub async fn wait_empty(&self) -> io::Result<()> {
        loop {
            if self.active_processes()? == 0 {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// Natural exit accounting must become empty without terminating the owned processes.
    pub async fn wait_empty_within(&self, within: std::time::Duration) -> io::Result<()> {
        tokio::time::timeout(within, self.wait_empty())
            .await
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "process tree exit was not verified",
                )
            })?
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        if !self.drain_on_drop {
            return;
        }
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

#[cfg(test)]
mod tests {
    use super::Job;
    use std::future::{Future, poll_fn};
    use std::io;
    use std::sync::atomic::Ordering;
    use std::task::Poll;
    use std::time::Duration;

    fn bounded_job_fixture() -> Job {
        let mut job = Job::new().unwrap();
        job.drain_on_drop = false;
        job
    }

    #[tokio::test]
    async fn natural_job_wait_yields_until_empty_without_termination() {
        let job = bounded_job_fixture();
        assert_eq!(job.active_processes().unwrap(), 0);
        job.pending_nonempty_queries.store(1, Ordering::Relaxed);
        let waiting = job.wait_empty_within(Duration::from_secs(2));
        tokio::pin!(waiting);
        poll_fn(|cx| {
            assert!(waiting.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("natural Job wait exceeded its watchdog")
            .unwrap();
        assert_eq!(job.pending_nonempty_queries.load(Ordering::Relaxed), 0);
        assert_eq!(job.active_processes().unwrap(), 0);
        assert_eq!(job.termination_requests.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn natural_job_wait_refuses_persistent_accounting_without_termination() {
        let job = bounded_job_fixture();
        job.force_nonempty_accounting();
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            job.wait_empty_within(Duration::from_millis(50)),
        )
        .await
        .expect("natural Job wait exceeded its watchdog")
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(job.termination_requests.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn enclosing_deadline_withholds_output_while_job_accounting_is_nonempty() {
        let job = bounded_job_fixture();
        job.force_nonempty_accounting();
        let operation = tokio::time::timeout(Duration::from_millis(50), async {
            job.wait_empty_within(Duration::from_secs(30)).await?;
            Ok::<_, io::Error>(b"accepted-output")
        });
        let result = tokio::time::timeout(Duration::from_secs(5), operation)
            .await
            .expect("enclosing deadline exceeded its watchdog");
        assert!(result.is_err(), "nonempty Job accounting admitted output");
        assert_eq!(job.termination_requests.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn cancellation_withholds_output_while_job_accounting_is_nonempty() {
        let job = bounded_job_fixture();
        job.force_nonempty_accounting();
        let (cancel, cancelled) = tokio::sync::oneshot::channel::<()>();
        let operation = async {
            tokio::select! {
                biased;
                signal = cancelled => {
                    signal.expect("cancellation sender disappeared");
                    Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"))
                }
                result = async {
                    job.wait_empty_within(Duration::from_secs(30)).await?;
                    Ok::<_, io::Error>(b"accepted-output")
                } => result,
            }
        };
        tokio::pin!(operation);
        poll_fn(|cx| {
            assert!(operation.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        cancel.send(()).unwrap();
        let error = tokio::time::timeout(Duration::from_secs(5), operation)
            .await
            .expect("cancellation exceeded its watchdog")
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(job.termination_requests.load(Ordering::Relaxed), 0);
    }
}
