//! Local Git reads retain process ownership through their deadline and verified cleanup.

use anyhow::{Context, Result, ensure};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

const ORIGIN_TIMEOUT: Duration = Duration::from_secs(5);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_DIAGNOSTICS: usize = 16 * 1024;

/// Saved-history reads share a command deadline, including their final ref verification.
#[derive(Clone, Copy)]
pub(crate) struct Deadline {
    expires: Instant,
}

const SEARCH_TIMEOUT: Duration = Duration::from_secs(30);

impl Deadline {
    pub(crate) fn new() -> Self {
        Self {
            expires: Instant::now() + SEARCH_TIMEOUT,
        }
    }

    pub(crate) fn expired(self) -> bool {
        Instant::now() >= self.expires
    }

    #[cfg(test)]
    pub(crate) fn at(expires: Instant) -> Self {
        Self { expires }
    }

    pub(crate) fn output(
        self,
        command: Command,
        input: Option<&[u8]>,
        limit: usize,
    ) -> Result<Output> {
        ensure!(!self.expired(), "local Git command deadline expired");
        ensure!(
            tokio::runtime::Handle::try_current().is_err(),
            "synchronous local Git inspection requires a blocking caller"
        );
        let deadline = self.expires.min(Instant::now() + ORIGIN_TIMEOUT);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("local Git supervision is unavailable")?;
        runtime.block_on(execute_with_input(command, input, limit, deadline))
    }
}

/// Internal batch pipes report EPIPE without changing the caller's terminal SIGPIPE policy.
/// The owned current-thread runtime keeps every write and mask restoration on this thread.
#[cfg(unix)]
struct InputSignals {
    blocked: libc::sigset_t,
    previous: libc::sigset_t,
    already_pending: bool,
    generated: bool,
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
}

#[cfg(unix)]
impl InputSignals {
    fn block() -> Result<Self> {
        let mut guard = Self {
            blocked: unsafe { std::mem::zeroed() },
            previous: unsafe { std::mem::zeroed() },
            already_pending: true,
            generated: false,
            _thread: std::marker::PhantomData,
        };
        unsafe {
            libc::sigemptyset(&mut guard.blocked);
            libc::sigaddset(&mut guard.blocked, libc::SIGPIPE);
        }
        let error =
            unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &guard.blocked, &mut guard.previous) };
        if error != 0 {
            // No mask was installed, so this value must not restore an uninitialized mask.
            std::mem::forget(guard);
            return Err(std::io::Error::from_raw_os_error(error))
                .context("local Git input signal protection is unavailable");
        }
        let mut pending = unsafe { std::mem::zeroed() };
        if unsafe { libc::sigpending(&mut pending) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("local Git input signal state is unavailable");
        }
        guard.already_pending = unsafe { libc::sigismember(&pending, libc::SIGPIPE) } == 1;
        Ok(guard)
    }

    fn observe(&mut self, result: &std::io::Result<()>) {
        self.generated |= result
            .as_ref()
            .is_err_and(|error| error.kind() == std::io::ErrorKind::BrokenPipe);
    }
}

#[cfg(unix)]
impl Drop for InputSignals {
    fn drop(&mut self) {
        let mut pending = unsafe { std::mem::zeroed() };
        if self.generated
            && !self.already_pending
            && unsafe { libc::sigpending(&mut pending) } == 0
            && unsafe { libc::sigismember(&pending, libc::SIGPIPE) } == 1
        {
            let mut signal = 0;
            unsafe { libc::sigwait(&self.blocked, &mut signal) };
        }
        unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &self.previous, std::ptr::null_mut()) };
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
                "code-origin Git process termination could not be verified"
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
        .context("code-origin Git output could not be read")?;
    ensure!(
        bytes.len() <= limit,
        "code-origin Git output exceeds its limit"
    );
    Ok(bytes)
}

#[cfg(test)]
async fn execute(command: Command, limit: usize, deadline: Instant) -> Result<Output> {
    execute_with_input(command, None, limit, deadline).await
}

async fn execute_with_input(
    command: Command,
    input: Option<&[u8]>,
    limit: usize,
    deadline: Instant,
) -> Result<Output> {
    ensure!(
        Instant::now() < deadline,
        "code-origin query deadline expired"
    );
    ensure!(
        cfg!(any(unix, windows)),
        "code-origin Git supervision is unavailable on this platform"
    );
    let mut command = tokio::process::Command::from(command);
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    let job = crate::rc::windows_job::Job::new()
        .context("code-origin Git process ownership is unavailable")?;
    #[cfg(windows)]
    crate::rc::windows_job::Job::configure(&mut command);
    let child = command.spawn().context("code-origin Git could not start")?;
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
        anyhow::bail!("code-origin Git process ownership could not be established");
    }
    let result = tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), async {
        let stdout = process.child.stdout.take().expect("query stdout is piped");
        let stderr = process.child.stderr.take().expect("query stderr is piped");
        let stdin = process.child.stdin.take();
        let writer = async {
            if let (Some(mut stdin), Some(input)) = (stdin, input) {
                #[cfg(target_vendor = "apple")]
                {
                    use std::os::fd::AsRawFd;
                    const F_SETNOSIGPIPE: libc::c_int = 73;
                    // Process-directed pipe signals require protection on the owned descriptor.
                    if unsafe { libc::fcntl(stdin.as_raw_fd(), F_SETNOSIGPIPE, 1) } == -1 {
                        return Err(std::io::Error::last_os_error())
                            .context("local Git input pipe protection is unavailable");
                    }
                }
                #[cfg(unix)]
                let mut signals = InputSignals::block()?;
                let written = stdin.write_all(input).await;
                #[cfg(unix)]
                signals.observe(&written);
                written.context("local Git input could not be written")?;
                let flushed = stdin.flush().await;
                #[cfg(unix)]
                signals.observe(&flushed);
                flushed.context("local Git input could not be flushed")?;
                let closed = stdin.shutdown().await;
                #[cfg(unix)]
                signals.observe(&closed);
                closed.context("local Git input could not be closed")?;
            }
            Ok::<(), anyhow::Error>(())
        };
        let ((), stdout, stderr) =
            tokio::try_join!(writer, read(stdout, limit), read(stderr, MAX_DIAGNOSTICS))?;
        let status = process
            .child
            .wait()
            .await
            .context("code-origin Git could not be reaped")?;
        ensure!(process.gone(), "code-origin Git left subprocesses running");
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    })
    .await
    .map_err(|_| anyhow::anyhow!("code-origin Git exceeded its time limit"))
    .and_then(|result| result);
    process.cleanup().await?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    const CHILD: &str = "infra::local_git::tests::supervised_query_child";

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
            "duplex" => {
                std::io::stdout()
                    .write_all(&vec![b'o'; 256 * 1024])
                    .unwrap();
                std::io::stdout().flush().unwrap();
                std::io::stderr()
                    .write_all(&[b'e'; MAX_DIAGNOSTICS])
                    .unwrap();
                std::io::stderr().flush().unwrap();
                let mut input = Vec::new();
                std::io::stdin().read_to_end(&mut input).unwrap();
                assert_eq!(input, vec![b'i'; 256 * 1024]);
                std::io::stdout()
                    .write_all(b"SYNTHETIC-input-complete\n")
                    .unwrap();
                std::io::stdout().flush().unwrap();
                return;
            }
            "reject-input" => return,
            #[cfg(unix)]
            "sigpipe-parent" => {
                unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
                let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(0);
                let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
                let unblocked = std::thread::spawn(move || {
                    let mut mask = unsafe { std::mem::zeroed() };
                    unsafe {
                        libc::sigemptyset(&mut mask);
                        libc::sigaddset(&mut mask, libc::SIGPIPE);
                    }
                    assert_eq!(
                        unsafe {
                            libc::pthread_sigmask(libc::SIG_UNBLOCK, &mask, std::ptr::null_mut())
                        },
                        0
                    );
                    ready_tx.send(()).unwrap();
                    let _ = release_rx.recv();
                });
                ready_rx.recv_timeout(ORIGIN_TIMEOUT).unwrap();
                let inner = std::path::PathBuf::from(
                    std::env::var_os("AGIT_TEST_ORIGIN_QUERY_PID").unwrap(),
                )
                .with_extension("inner");
                let error = Deadline::new()
                    .output(
                        child("reject-input", &inner),
                        Some(&vec![b'i'; 1024 * 1024]),
                        4096,
                    )
                    .unwrap_err();
                assert!(
                    error.to_string().contains("input could not be"),
                    "{error:#}"
                );
                assert_reaped(std::fs::read_to_string(inner).unwrap().parse().unwrap());
                let mut mask = unsafe { std::mem::zeroed() };
                assert_eq!(
                    unsafe {
                        libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), &mut mask)
                    },
                    0
                );
                assert_eq!(unsafe { libc::sigismember(&mask, libc::SIGPIPE) }, 0);
                assert_eq!(
                    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) },
                    libc::SIG_DFL
                );
                let mut outer = InputSignals::block().unwrap();
                assert_eq!(
                    unsafe { libc::pthread_kill(libc::pthread_self(), libc::SIGPIPE) },
                    0
                );
                outer.generated = true;
                let nested = InputSignals::block().unwrap();
                assert!(nested.already_pending);
                drop(nested);
                let mut pending = unsafe { std::mem::zeroed() };
                assert_eq!(unsafe { libc::sigpending(&mut pending) }, 0);
                assert_eq!(unsafe { libc::sigismember(&pending, libc::SIGPIPE) }, 1);
                drop(outer);
                release_tx.send(()).unwrap();
                unblocked.join().unwrap();
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
                started + ORIGIN_TIMEOUT,
            ));
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
    fn saved_reads_drain_output_while_writing_and_close_input_before_waiting() {
        let root = tempfile::tempdir().unwrap();
        let pid = root.path().join("duplex");
        let input = vec![b'i'; 256 * 1024];
        let output = Deadline::new()
            .output(child("duplex", &pid), Some(&input), 512 * 1024)
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let expected = vec![b'o'; 256 * 1024];
        assert!(
            output
                .stdout
                .windows(expected.len())
                .any(|part| part == expected)
        );
        assert!(
            output
                .stdout
                .windows(b"SYNTHETIC-input-complete\n".len())
                .any(|part| part == b"SYNTHETIC-input-complete\n")
        );
        assert_eq!(output.stderr, vec![b'e'; MAX_DIAGNOSTICS]);
        assert_reaped(std::fs::read_to_string(pid).unwrap().parse().unwrap());
    }

    #[test]
    fn blocked_input_obeys_the_operation_deadline_and_reaps_its_child() {
        assert_eq!(SEARCH_TIMEOUT, Duration::from_secs(30));
        assert_eq!(ORIGIN_TIMEOUT, Duration::from_secs(5));
        assert_eq!(CLEANUP_TIMEOUT, Duration::from_secs(2));
        let root = tempfile::tempdir().unwrap();
        let pid = root.path().join("blocked-input");
        let started = Instant::now();
        let error = Deadline::new()
            .output(child("wait", &pid), Some(&vec![b'i'; 1024 * 1024]), 4096)
            .unwrap_err();
        assert!(error.to_string().contains("time limit"), "{error:#}");
        assert!(started.elapsed() < ORIGIN_TIMEOUT + CLEANUP_TIMEOUT + Duration::from_secs(2));
        assert_reaped(std::fs::read_to_string(pid).unwrap().parse().unwrap());
    }

    #[test]
    fn successful_child_exit_cannot_hide_an_incomplete_input_write() {
        let root = tempfile::tempdir().unwrap();
        let pid = root.path().join("rejected-input");
        let error = Deadline::new()
            .output(
                child("reject-input", &pid),
                Some(&vec![b'i'; 1024 * 1024]),
                4096,
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("input could not be"),
            "{error:#}"
        );
        assert_reaped(std::fs::read_to_string(pid).unwrap().parse().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn internal_input_failure_preserves_the_default_terminal_sigpipe_policy() {
        let root = tempfile::tempdir().unwrap();
        let pid = root.path().join("sigpipe-parent");
        let output = Deadline::new()
            .output(child("sigpipe-parent", &pid), None, 4096)
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_reaped(std::fs::read_to_string(pid).unwrap().parse().unwrap());
    }

    #[test]
    fn shared_deadline_and_nested_runtime_refuse_before_spawning() {
        let root = tempfile::tempdir().unwrap();
        let pid = root.path().join("unstarted");
        let deadline = Deadline::at(Instant::now() - Duration::from_secs(1));
        assert!(
            deadline
                .output(child("complete", &pid), None, 4096)
                .unwrap_err()
                .to_string()
                .contains("deadline expired")
        );
        assert!(!pid.exists());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            assert!(
                Deadline::new()
                    .output(child("complete", &pid), None, 4096)
                    .unwrap_err()
                    .to_string()
                    .contains("blocking caller")
            );
        });
        assert!(!pid.exists());
    }

    #[cfg(windows)]
    #[test]
    fn unverified_job_cleanup_stays_an_error_and_does_not_block_drop() {
        let root = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let started = Instant::now();
        runtime.block_on(async {
            let mut command =
                tokio::process::Command::from(child("wait", &root.path().join("pid")));
            command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            let job = crate::rc::windows_job::Job::new().unwrap();
            crate::rc::windows_job::Job::configure(&mut command);
            let child = command.spawn().unwrap();
            let pid = child.id().unwrap();
            let mut process = Process {
                child,
                job: Some(job),
            };
            process
                .job
                .as_ref()
                .unwrap()
                .attach_and_resume(&process.child)
                .unwrap();
            process.job.as_ref().unwrap().force_nonempty_accounting();
            assert!(
                process
                    .cleanup()
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("termination could not be verified")
            );
            drop(process);
            assert_reaped(pid);
        });
        assert!(started.elapsed() < CLEANUP_TIMEOUT + Duration::from_secs(2));
    }
}
