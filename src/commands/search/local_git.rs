//! Origin queries share one deadline and retain process ownership through verified cleanup.

use crate::domain::repo::Repo;
use anyhow::{Context, Result, ensure};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

const ORIGIN_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_OUTPUT: usize = 4097;

pub(super) struct Git {
    runtime: tokio::runtime::Runtime,
    deadline: Instant,
}

impl Git {
    pub(super) fn new() -> Result<Self> {
        let deadline = Instant::now() + ORIGIN_TIMEOUT;
        Ok(Self {
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("code-origin query supervision is unavailable")?,
            deadline,
        })
    }

    pub(super) fn output(&self, repo: &Repo, args: &[&str], limit: usize) -> Result<Output> {
        self.run(repo.inspection_command(args), limit)
    }

    pub(super) fn effective_origin(&self, repo: &Repo) -> Result<Output> {
        self.run(repo.inspection_code_origin_command(), MAX_OUTPUT)
    }

    fn run(&self, command: Command, limit: usize) -> Result<Output> {
        ensure!(
            Instant::now() < self.deadline,
            "code-origin query deadline expired"
        );
        self.runtime.block_on(crate::infra::local_git::output_until(
            command,
            limit.min(MAX_OUTPUT),
            self.deadline,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);
    const MAX_DIAGNOSTICS: usize = 16 * 1024;

    const CHILD: &str = "commands::search::local_git::tests::supervised_query_child";

    fn child(mode: &str, pid: &std::path::Path) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", CHILD, "--ignored", "--nocapture"])
            .env("AGIT_TEST_ORIGIN_QUERY_MODE", mode)
            .env("AGIT_TEST_ORIGIN_QUERY_PID", pid);
        command
    }

    #[test]
    #[ignore = "owned child for bounded code-origin query supervision"]
    fn supervised_query_child() {
        let Ok(mode) = std::env::var("AGIT_TEST_ORIGIN_QUERY_MODE") else {
            return;
        };
        let pid = std::env::var_os("AGIT_TEST_ORIGIN_QUERY_PID").unwrap();
        std::fs::write(pid, std::process::id().to_string()).unwrap();
        match mode.as_str() {
            "stdout" => {
                std::io::stdout().write_all(&[b'x'; 4096]).unwrap();
                std::io::stdout().flush().unwrap();
            }
            "oversized" => {
                std::io::stdout()
                    .write_all(&[b'x'; MAX_OUTPUT + 1])
                    .unwrap();
                std::io::stdout().flush().unwrap();
            }
            "stderr" => {
                std::io::stderr()
                    .write_all(&[b'x'; MAX_DIAGNOSTICS + 1])
                    .unwrap();
                std::io::stderr().flush().unwrap();
            }
            "complete" => {
                std::io::stdout()
                    .write_all(b"SYNTHETIC-origin-output\n")
                    .unwrap();
                std::io::stdout().flush().unwrap();
                std::io::stderr()
                    .write_all(b"SYNTHETIC-origin-diagnostic\n")
                    .unwrap();
                std::io::stderr().flush().unwrap();
                return;
            }
            "wait" => (),
            _ => panic!("unknown code-origin query fixture"),
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
        for mode in ["wait", "stdout", "stderr", "complete"] {
            let pid = root.path().join(mode);
            let started = Instant::now();
            let git = Git::new().unwrap();
            let result = git.run(child(mode, &pid), if mode == "stdout" { 256 } else { 4096 });
            assert!(started.elapsed() < ORIGIN_TIMEOUT + CLEANUP_TIMEOUT + Duration::from_secs(2));
            let pid: u32 = std::fs::read_to_string(pid).unwrap().parse().unwrap();
            assert_reaped(pid);
            match mode {
                "complete" => {
                    let output = result.unwrap();
                    assert!(output.status.success());
                    assert!(
                        output
                            .stdout
                            .windows(b"SYNTHETIC-origin-output\n".len())
                            .any(|part| part == b"SYNTHETIC-origin-output\n")
                    );
                    assert_eq!(output.stderr, b"SYNTHETIC-origin-diagnostic\n");
                }
                "wait" => assert!(result.unwrap_err().to_string().contains("time limit")),
                _ => assert!(result.unwrap_err().to_string().contains("output exceeds")),
            }
        }
    }

    #[test]
    fn origin_deadline_refuses_another_process_before_spawn() {
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

    #[test]
    fn origin_queries_keep_their_deadline_after_success_and_timeout() {
        assert_eq!(ORIGIN_TIMEOUT, Duration::from_secs(5));
        let root = tempfile::tempdir().unwrap();
        let mut git = Git::new().unwrap();
        let deadline = git.deadline;
        let complete = root.path().join("complete");
        assert!(
            git.run(child("complete", &complete), MAX_OUTPUT)
                .unwrap()
                .status
                .success()
        );
        assert_reaped(std::fs::read_to_string(complete).unwrap().parse().unwrap());
        assert_eq!(git.deadline, deadline);

        let waiting = root.path().join("waiting");
        let command = child("wait", &waiting);
        let remaining = Duration::from_millis(1500);
        let allowed = remaining + CLEANUP_TIMEOUT + Duration::from_secs(1);
        assert!(allowed < ORIGIN_TIMEOUT);
        let started = Instant::now();
        git.deadline = started + remaining;
        let deadline = git.deadline;
        let error = git.run(command, MAX_OUTPUT).unwrap_err();
        let elapsed = started.elapsed();
        assert!(error.to_string().contains("time limit"), "{error:#}");
        assert_reaped(std::fs::read_to_string(waiting).unwrap().parse().unwrap());
        assert_eq!(git.deadline, deadline);
        assert!(elapsed < allowed, "query renewed its deadline: {elapsed:?}");

        let unstarted = root.path().join("unstarted");
        let error = git
            .run(child("complete", &unstarted), MAX_OUTPUT)
            .unwrap_err();
        assert!(error.to_string().contains("deadline expired"), "{error:#}");
        assert!(!unstarted.exists());
    }

    #[test]
    fn origin_output_limit_cannot_be_expanded_by_the_caller() {
        assert_eq!(MAX_OUTPUT, 4097);
        let root = tempfile::tempdir().unwrap();
        let pid = root.path().join("oversized");
        let error = Git::new()
            .unwrap()
            .run(child("oversized", &pid), usize::MAX)
            .unwrap_err();
        assert!(error.to_string().contains("output exceeds"), "{error:#}");
        assert_reaped(std::fs::read_to_string(pid).unwrap().parse().unwrap());
    }
}
