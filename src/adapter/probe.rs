//! Questions agit asks a runtime the person installed: `codex --help`, `codex features list`,
//! `codex app-server`.
//!
//! Such a runtime is often a script that starts the real program (an npm shim: on Windows
//! `codex.cmd` starts cmd.exe, which starts node, which starts codex), and the program may start
//! helpers of its own, such as a plugin's `git ls-remote`. Ending only the process agit started
//! leaves the rest running, in whatever folder it was started from, after agit has returned; and a
//! helper that inherited the output keeps a reader waiting until it exits. So each question runs
//! as an [`Asked`] process, in a process group of its own (unix) or a kill-on-close job
//! (Windows), and everything in that group or job is ended when agit is done with the answer, or
//! when its time is up. A question whose answer doesn't depend on where Codex runs also runs from
//! a folder of no consequence to the person. One that does, such as `features list`, which applies
//! a trusted project's own configuration, runs in that project's folder.
//!
//! What a runtime starts to outlive the question on purpose is left alone. A shared background
//! server detaches: it starts a new session (unix) or breaks away from the job (Windows), so it is
//! in neither, is never ended, and is never waited for. The person's other conversations may
//! already be using it.

use std::io::Read as _;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// A runtime process agit started to ask it something, with everything it starts in turn that
/// doesn't detach. Dropping it ends them all.
pub(crate) struct Asked {
    child: Child,
    #[cfg(all(windows, feature = "rc"))]
    job: Option<crate::rc::windows_job::Job>,
}

impl Asked {
    /// Starts `command` as a question, in a group or job of its own. Without a working folder
    /// set on `command`, it runs from the temporary folder, so nothing left behind holds the
    /// person's folder; a question about a folder that Codex reads from the request names it
    /// there instead.
    pub(crate) fn start(mut command: Command) -> std::io::Result<Self> {
        if command.get_current_dir().is_none() {
            command.current_dir(std::env::temp_dir());
        }
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        #[cfg(all(windows, feature = "rc"))]
        let job = {
            let job = crate::rc::windows_job::Job::new()?;
            job.allow_breakaway()?;
            crate::rc::windows_job::Job::configure_std(&mut command);
            job
        };
        let child = command.spawn()?;
        #[cfg(all(windows, feature = "rc"))]
        if let Err(error) = job.attach_std(&child) {
            let mut child = child;
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        Ok(Self {
            child,
            #[cfg(all(windows, feature = "rc"))]
            job: Some(job),
        })
    }

    pub(crate) fn child(&mut self) -> &mut Child {
        &mut self.child
    }

    /// Ends every process in the group or job, the one agit started included.
    fn end(&mut self) {
        // The group's id is the started process's id, which stays the group's while any process
        // of it remains.
        #[cfg(unix)]
        unsafe {
            libc::killpg(self.child.id() as libc::pid_t, libc::SIGKILL);
        }
        #[cfg(all(windows, feature = "rc"))]
        if let Some(job) = &self.job {
            let _ = job.terminate();
        }
        let _ = self.child.kill();
    }
}

impl Drop for Asked {
    fn drop(&mut self) {
        self.end();
        let _ = self.child.wait();
        #[cfg(all(windows, feature = "rc"))]
        if let Some(job) = self.job.take() {
            let _ = job.terminate_and_close();
        }
    }
}

/// Writes `line` and a newline to a question's input. A runtime that has already exited (an old
/// Codex refusing `app-server`, one that fails at start) has closed it, and writing to a closed
/// pipe raises SIGPIPE. agit keeps that signal's default so that `agit log | head` ends quietly,
/// and the default ends agit itself, silently, in the middle of doctor or setup. So the write runs
/// with SIGPIPE ignored, as git does while it feeds a hook, and a closed input is an error here.
#[cfg(feature = "cli")]
pub(crate) fn write_line(input: &mut impl std::io::Write, line: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    let previous = unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
    let written = writeln!(input, "{line}").and_then(|()| input.flush());
    #[cfg(unix)]
    if previous != libc::SIG_ERR {
        unsafe { libc::signal(libc::SIGPIPE, previous) };
    }
    written
}

/// Runs `command` as an [`Asked`] process with no input and returns its output once it exits
/// successfully, at most `max` bytes of it. `None` when it can't be started, fails, or hasn't
/// exited and closed its output within `limit`.
///
/// When the process exits, whatever it left running in its group or job is ended too, before the
/// rest of the output is read: a helper that inherited the output would otherwise hold it open,
/// and agit would wait out the whole limit for an answer that is already complete.
pub(crate) fn output(mut command: Command, limit: Duration, max: u64) -> Option<Vec<u8>> {
    let deadline = Instant::now() + limit;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut asked = Asked::start(command).ok()?;
    let stdout = asked.child().stdout.take()?;
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut text = Vec::new();
        let _ = stdout.take(max).read_to_end(&mut text);
        let _ = sender.send(text);
    });
    let status = loop {
        if let Ok(Some(status)) = asked.child().try_wait() {
            break status;
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    asked.end();
    let stdout = receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .ok()?;
    status.success().then_some(stdout)
}

#[cfg(all(test, any(unix, all(windows, feature = "rc"))))]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    const STAND_IN: &str = "adapter::probe::tests::stand_in_runtime";
    const ROLE: &str = "AGIT_PROBE_STAND_IN";
    const DIR: &str = "AGIT_PROBE_STAND_IN_DIR";
    const ANSWER: &str = "stand-in answered";

    fn stand_in(role: &str, dir: &Path) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", STAND_IN, "--nocapture"])
            .env(ROLE, role)
            .env(DIR, dir);
        command
    }

    /// Not a test of its own: the runtime the tests below ask, started with [`ROLE`] set. As
    /// "answer" or "late" it starts a helper that keeps the output open and a server that detaches
    /// the way a shared background server does, then answers and exits, or never answers. As
    /// "helper" or "server" it records its process id and stays. As "writer" it is agit instead,
    /// writing a request to a runtime that has already exited.
    #[test]
    fn stand_in_runtime() {
        let Ok(role) = std::env::var(ROLE) else {
            return;
        };
        let dir = PathBuf::from(std::env::var_os(DIR).unwrap());
        match role.as_str() {
            #[cfg(all(unix, feature = "cli"))]
            "writer" => {
                // As agit's `main` sets it.
                unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
                let mut gone = Command::new("sh")
                    .args(["-c", "exit 0"])
                    .stdin(Stdio::piped())
                    .spawn()
                    .unwrap();
                let mut input = gone.stdin.take().unwrap();
                gone.wait().unwrap();
                assert!(write_line(&mut input, "{}").is_err());
                std::process::exit(0);
            }
            "helper" | "server" => {
                std::fs::write(dir.join(&role), std::process::id().to_string()).unwrap();
                std::thread::sleep(Duration::from_secs(120));
            }
            _ => {
                stand_in("helper", &dir).spawn().unwrap();
                let mut server = stand_in("server", &dir);
                server
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                detach(&mut server);
                server.spawn().unwrap();
                let started = Instant::now();
                while !(dir.join("helper").exists() && dir.join("server").exists()) {
                    assert!(started.elapsed() < Duration::from_secs(30));
                    std::thread::sleep(Duration::from_millis(10));
                }
                if role == "answer" {
                    println!("{ANSWER}");
                    std::process::exit(0);
                }
                std::thread::sleep(Duration::from_secs(120));
            }
        }
    }

    #[cfg(unix)]
    fn detach(command: &mut Command) {
        use std::os::unix::process::CommandExt as _;
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }

    #[cfg(windows)]
    fn detach(command: &mut Command) {
        use std::os::windows::process::CommandExt as _;
        use windows_sys::Win32::System::Threading::{CREATE_BREAKAWAY_FROM_JOB, DETACHED_PROCESS};
        command.creation_flags(CREATE_BREAKAWAY_FROM_JOB | DETACHED_PROCESS);
    }

    fn pid(dir: &Path, role: &str) -> u32 {
        std::fs::read_to_string(dir.join(role))
            .unwrap()
            .parse()
            .unwrap()
    }

    #[cfg(unix)]
    fn running(pid: u32) -> bool {
        // A process ended with its group is reaped by whoever adopts it; until then it is a
        // zombie, which signal 0 still finds, so the caller waits for it to go.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    #[cfg(windows)]
    fn running(pid: u32) -> bool {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return false;
        }
        let mut code = 0u32;
        let read = unsafe { GetExitCodeProcess(handle, &mut code) };
        unsafe { CloseHandle(handle) };
        read != 0 && code == STILL_ACTIVE as u32
    }

    #[cfg(unix)]
    fn stop(pid: u32) {
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    }

    #[cfg(windows)]
    fn stop(pid: u32) {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_TERMINATE, TerminateProcess,
        };
        let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
        if !handle.is_null() {
            unsafe {
                TerminateProcess(handle, 1);
                CloseHandle(handle);
            }
        }
    }

    /// Pins what a question leaves running once agit has its answer or gives up on it: nothing
    /// it started, except what detached on purpose. The asked runtime starts a helper that holds
    /// its output open and a server that detaches. The answer arrives as soon as the runtime exits,
    /// and at the limit there is none; either way the helper is gone afterwards and the server is
    /// still running. An implementation that ends only the started process keeps the helper and
    /// waits out the limit for its output; one that ends whatever the runtime ever started ends the
    /// person's shared server.
    #[test]
    fn a_question_ends_what_it_started_except_what_detached() {
        for role in ["answer", "late"] {
            let dir = tempfile::tempdir().unwrap();
            let limit = Duration::from_secs(if role == "answer" { 60 } else { 5 });
            let asked = Instant::now();
            let answer = output(stand_in(role, dir.path()), limit, 1 << 20);
            let helper = pid(dir.path(), "helper");
            let server = pid(dir.path(), "server");
            let helper_gone = {
                let started = Instant::now();
                while running(helper) && started.elapsed() < Duration::from_secs(10) {
                    std::thread::sleep(Duration::from_millis(20));
                }
                !running(helper)
            };
            let server_running = running(server);
            stop(server);
            match role {
                "answer" => {
                    let answer = answer.expect("the runtime answered and exited");
                    assert!(String::from_utf8_lossy(&answer).contains(ANSWER));
                    assert!(
                        asked.elapsed() < limit,
                        "the helper's open output held the answer"
                    );
                }
                _ => assert!(
                    answer.is_none(),
                    "a runtime that never answers has no answer"
                ),
            }
            assert!(helper_gone, "{role}: the helper outlived the question");
            assert!(server_running, "{role}: the detached server was ended");
        }
    }

    /// Pins that a runtime which exits before agit's first request never takes agit down with it.
    /// agit keeps SIGPIPE's default for its own output, so a plain write to the closed input ends
    /// the writing process by signal, with nothing said; through [`write_line`] it is an error the
    /// caller handles.
    #[cfg(all(unix, feature = "cli"))]
    #[test]
    fn a_runtime_that_exits_before_the_first_request_does_not_end_agit() {
        let dir = tempfile::tempdir().unwrap();
        let status = stand_in("writer", dir.path())
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(0), "{status:?}");
    }
}
