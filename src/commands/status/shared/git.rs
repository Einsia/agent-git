//! Shared-file queries own their process tree until bounded output and exit are verified.

use crate::domain::repo::{Repo, Worktree};
use anyhow::{Context, Result, ensure};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt};

const QUERY_TIMEOUT: Duration = Duration::from_secs(5);
const PAGE_TIMEOUT: Duration = Duration::from_secs(15);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_DIAGNOSTICS: usize = 16 * 1024;

pub(super) struct Git {
    runtime: tokio::runtime::Runtime,
    deadline: Instant,
}

impl Git {
    pub(super) fn new() -> Result<Self> {
        Ok(Self {
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("shared-file query supervision is unavailable")?,
            deadline: Instant::now() + PAGE_TIMEOUT,
        })
    }

    pub(super) fn output(&self, repo: &Repo, args: &[&str], limit: usize) -> Result<Output> {
        self.run(repo.inspection_command(args), limit)
    }

    pub(super) fn common_dir(&self, repo: &Repo) -> Result<PathBuf> {
        repo.inspection_common_dir_using(&mut |command, limit| self.run(command, limit))
    }

    pub(super) fn checkout_paths(&self, repo: &Repo) -> Result<(PathBuf, PathBuf)> {
        repo.inspection_checkout_paths_using(&mut |command, limit| self.run(command, limit))
    }

    pub(super) fn worktrees(&self, repo: &Repo) -> Result<Vec<Worktree>> {
        repo.inspection_worktrees_using(&mut |command, limit| self.run(command, limit))
    }

    fn run(&self, command: Command, limit: usize) -> Result<Output> {
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .context("shared-file query deadline expired")?;
        self.runtime.block_on(execute(
            command,
            limit.min(super::MAX_OUTPUT),
            remaining.min(QUERY_TIMEOUT),
        ))
    }
}

struct Process {
    child: tokio::process::Child,
    #[cfg(unix)]
    pgid: Option<i32>,
    #[cfg(windows)]
    job: Option<crate::rc::windows_job::Job>,
}

impl Process {
    fn gone(&mut self) -> bool {
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            return false;
        }
        #[cfg(unix)]
        {
            self.pgid.is_none_or(|pgid| {
                (unsafe { libc::killpg(pgid, 0) }) == -1
                    && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            })
        }
        #[cfg(windows)]
        {
            self.job
                .as_ref()
                .is_some_and(|job| job.active_processes().is_ok_and(|count| count == 0))
        }
        #[cfg(not(any(unix, windows)))]
        false
    }

    fn terminate(&mut self) {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid {
            unsafe { libc::killpg(pgid, libc::SIGKILL) };
        }
        #[cfg(windows)]
        if let Some(job) = &self.job {
            let _ = job.terminate();
        }
        let _ = self.child.start_kill();
    }

    async fn cleanup(&mut self) -> Result<()> {
        if !self.gone() {
            self.terminate();
        }
        let deadline = Instant::now() + CLEANUP_TIMEOUT;
        loop {
            if self.gone() {
                #[cfg(unix)]
                {
                    self.pgid = None;
                }
                return Ok(());
            }
            ensure!(
                Instant::now() < deadline,
                "shared-file Git process termination could not be verified"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.terminate();
        #[cfg(windows)]
        if let Some(job) = self.job.take() {
            let _ = job.terminate_and_close();
        }
    }
}

async fn read(mut stream: impl AsyncRead + Unpin, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    (&mut stream)
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .context("shared-file Git output could not be read")?;
    ensure!(
        bytes.len() <= limit,
        "shared-file Git output exceeds its limit"
    );
    Ok(bytes)
}

async fn execute(command: Command, limit: usize, timeout: Duration) -> Result<Output> {
    ensure!(
        cfg!(any(unix, windows)),
        "shared-file Git supervision is unavailable on this platform"
    );
    let mut command = tokio::process::Command::from(command);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    let job = crate::rc::windows_job::Job::new()
        .context("shared-file Git process ownership is unavailable")?;
    #[cfg(windows)]
    crate::rc::windows_job::Job::configure(&mut command);
    let child = command.spawn().context("shared-file Git could not start")?;
    #[cfg(unix)]
    let pgid = child.id().map(|id| id as i32);
    let mut process = Process {
        child,
        #[cfg(unix)]
        pgid,
        #[cfg(windows)]
        job: Some(job),
    };
    #[cfg(windows)]
    if process
        .job
        .as_ref()
        .expect("a Windows query owns its Job")
        .attach_and_resume(&process.child)
        .is_err()
    {
        process.cleanup().await?;
        anyhow::bail!("shared-file Git process ownership could not be established");
    }
    let result = tokio::time::timeout(timeout, async {
        let stdout = process.child.stdout.take().expect("query stdout is piped");
        let stderr = process.child.stderr.take().expect("query stderr is piped");
        let (stdout, stderr) =
            tokio::try_join!(read(stdout, limit), read(stderr, MAX_DIAGNOSTICS))?;
        let status = process
            .child
            .wait()
            .await
            .context("shared-file Git could not be reaped")?;
        #[cfg(windows)]
        {
            let drained = match &process.job {
                Some(job) => job.wait_empty_within(CLEANUP_TIMEOUT).await.is_ok(),
                None => false,
            };
            ensure!(drained, "shared-file Git left subprocesses running");
        }
        ensure!(process.gone(), "shared-file Git left subprocesses running");
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    })
    .await
    .map_err(|_| anyhow::anyhow!("shared-file Git exceeded its time limit"))
    .and_then(|result| result);
    process.cleanup().await?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const CHILD: &str = "commands::status::shared::git::tests::supervised_query_child";

    fn child(mode: &str, pid: &std::path::Path) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", CHILD, "--ignored", "--nocapture"])
            .env("AGIT_TEST_SHARED_QUERY_MODE", mode)
            .env("AGIT_TEST_SHARED_QUERY_PID", pid);
        command
    }

    #[test]
    #[ignore = "owned child for bounded shared-file query supervision"]
    fn supervised_query_child() {
        let Ok(mode) = std::env::var("AGIT_TEST_SHARED_QUERY_MODE") else {
            return;
        };
        let pid = std::env::var_os("AGIT_TEST_SHARED_QUERY_PID").unwrap();
        std::fs::write(pid, std::process::id().to_string()).unwrap();
        match mode.as_str() {
            "stdout" => {
                std::io::stdout().write_all(&[b'x'; 4096]).unwrap();
                std::io::stdout().flush().unwrap();
            }
            "stderr" => {
                std::io::stderr()
                    .write_all(&[b'x'; MAX_DIAGNOSTICS + 1])
                    .unwrap();
                std::io::stderr().flush().unwrap();
            }
            "complete" => return,
            "wait" => (),
            _ => panic!("unknown shared-file query fixture"),
        }
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    fn assert_reaped(pid: u32) {
        #[cfg(unix)]
        {
            assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::Foundation::{
                CloseHandle, ERROR_INVALID_PARAMETER, WAIT_OBJECT_0,
            };
            use windows_sys::Win32::System::Threading::{
                OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
            };
            let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
            if !process.is_null() {
                let state = unsafe { WaitForSingleObject(process, 0) };
                unsafe { CloseHandle(process) };
                assert_eq!(state, WAIT_OBJECT_0, "query child is still running");
            } else {
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(ERROR_INVALID_PARAMETER as i32)
                );
            }
        }
    }

    #[test]
    fn queries_cap_both_streams_and_reap_timeout_and_overflow_children() {
        let root = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        for mode in ["wait", "stdout", "stderr", "complete"] {
            let pid = root.path().join(mode);
            let started = Instant::now();
            let result = runtime.block_on(execute(
                child(mode, &pid),
                if mode == "stdout" { 256 } else { 4096 },
                QUERY_TIMEOUT,
            ));
            assert!(started.elapsed() < QUERY_TIMEOUT + CLEANUP_TIMEOUT + Duration::from_secs(2));
            let pid: u32 = std::fs::read_to_string(pid).unwrap().parse().unwrap();
            assert_reaped(pid);
            match mode {
                "complete" => assert!(result.unwrap().status.success()),
                "wait" => assert!(result.unwrap_err().to_string().contains("time limit")),
                _ => assert!(result.unwrap_err().to_string().contains("output exceeds")),
            }
        }
    }

    #[test]
    fn page_deadline_refuses_another_process_before_spawn() {
        let root = tempfile::tempdir().unwrap();
        let pid = root.path().join("unstarted");
        let mut git = Git::new().unwrap();
        git.deadline = Instant::now() - Duration::from_secs(1);
        assert!(
            git.run(child("complete", &pid), 4096)
                .unwrap_err()
                .to_string()
                .contains("deadline expired")
        );
        assert!(!pid.exists());
    }
}
