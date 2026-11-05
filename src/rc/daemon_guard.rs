//! A daemon outlives whoever started it, so nothing it writes may be what ends it.
//!
//! Every `agit` process takes SIGPIPE's default, which suits `agit log | head`. In the daemon that
//! default turns a write to any peer that went away (a child's stdin, a client's socket) into the
//! end of the process, with no line anywhere saying why. Standard output is the other half: with
//! the signal out of the way, a `println!` that can't be written panics. So the daemon catches the
//! signal, its own output goes through a thread that ignores every failure, and a panic leaves a
//! line in the daemon's log.

use std::io::Write;

/// How large the log `spawn_daemon` made for this daemon grows before it starts over.
#[cfg(unix)]
const LOG_BYTES: u64 = 8 * 1024 * 1024;
pub(super) const LOG_PREFIX: &str = "agitd-";
pub(super) const LOG_SUFFIX: &str = ".log";

/// Holds this process's output for as long as it runs as a daemon. When it stops, stdout and stderr
/// go back to where they led and what is still on its way gets there, so a daemon that refuses to
/// start can say why: the process would otherwise exit with its last words in the pipe.
#[must_use]
pub(super) struct Guard {
    #[cfg(unix)]
    output: Option<Output>,
}

#[cfg(unix)]
struct Output {
    stdout: libc::c_int,
    stderr: libc::c_int,
    drained: std::sync::mpsc::Receiver<()>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(output) = self.output.take() {
            unsafe {
                libc::dup2(output.stdout, 1);
                libc::dup2(output.stderr, 2);
                libc::close(output.stdout);
                libc::close(output.stderr);
            }
            // A child that outlives the daemon keeps the pipe open, so this doesn't wait for it.
            let _ = output
                .drained
                .recv_timeout(std::time::Duration::from_millis(500));
        }
    }
}

/// Makes this process a daemon nothing it writes can end, until the guard goes. Called once,
/// first thing.
pub(super) fn install() -> Guard {
    #[cfg(unix)]
    catch_sigpipe();
    let guard = Guard {
        #[cfg(unix)]
        output: pump_output(),
    };
    std::panic::set_hook(Box::new(|info| {
        // Thread and place only: a panic's message can quote what the daemon was handling.
        let thread = std::thread::current();
        let thread = thread.name().unwrap_or("unnamed");
        let (file, line) = info
            .location()
            .map_or(("unknown", 0), |at| (at.file(), at.line()));
        super::diagnostics::event(
            "daemon.panic",
            serde_json::json!({"thread": thread, "file": file, "line": line}),
        );
        let _ = writeln!(
            std::io::stderr(),
            "agitd: panic in {thread} at {file}:{line}"
        );
    }));
    guard
}

/// Catches SIGPIPE, so a write to a peer that went away fails on its own. A handler and not
/// `SIG_IGN`: the kernel puts a caught signal back to its default in every program this process
/// starts, however it was started, while an ignored one is inherited.
#[cfg(unix)]
fn catch_sigpipe() {
    extern "C" fn ignore(_: libc::c_int) {}
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = ignore as extern "C" fn(libc::c_int) as usize;
        action.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGPIPE, &action, std::ptr::null_mut());
    }
}

/// Puts this process's stdout and stderr on a pipe a thread here drains into where stderr went,
/// so writing them can't fail: the thread ignores a closed or full target. A redirect to a file
/// alone would only move the failure to a full disk.
#[cfg(unix)]
fn pump_output() -> Option<Output> {
    use std::os::fd::FromRawFd;
    let limit = super::rc_dir()
        .is_ok_and(|folder| is_own_log(&folder))
        .then_some(LOG_BYTES);
    let mut ends = [0; 2];
    unsafe {
        let stdout = libc::fcntl(1, libc::F_DUPFD_CLOEXEC, 3);
        let stderr = libc::fcntl(2, libc::F_DUPFD_CLOEXEC, 3);
        let target = libc::fcntl(2, libc::F_DUPFD_CLOEXEC, 3);
        if stdout < 0 || stderr < 0 || target < 0 || libc::pipe(ends.as_mut_ptr()) != 0 {
            return None;
        }
        libc::fcntl(ends[0], libc::F_SETFD, libc::FD_CLOEXEC);
        let source = std::fs::File::from_raw_fd(ends[0]);
        let target = std::fs::File::from_raw_fd(target);
        let (done, drained) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("agitd-output".into())
            .spawn(move || {
                drain(source, target, limit);
                let _ = done.send(());
            });
        // Only now: without the thread nothing reads the pipe, and output stays where it led.
        if thread.is_err() {
            libc::close(ends[1]);
            libc::close(stdout);
            libc::close(stderr);
            return None;
        }
        libc::dup2(ends[1], 1);
        libc::dup2(ends[1], 2);
        libc::close(ends[1]);
        Some(Output {
            stdout,
            stderr,
            drained,
        })
    }
}

/// Whether stderr is a log `spawn_daemon` made in `folder`. Anything else is the caller's own
/// redirect, which can hold what came before this daemon, so it is only ever added to.
#[cfg(unix)]
fn is_own_log(folder: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let stderr = unsafe {
        let mut stat: libc::stat = std::mem::zeroed();
        if libc::fstat(2, &mut stat) != 0 {
            return false;
        }
        (stat.st_dev as u64, stat.st_ino as u64)
    };
    let Ok(entries) = std::fs::read_dir(folder) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        name.starts_with(LOG_PREFIX)
            && name.ends_with(LOG_SUFFIX)
            && entry
                .metadata()
                .is_ok_and(|log| (log.dev(), log.ino()) == stderr)
    })
}

/// Copies `source` into `target` for as long as `source` is open, ignoring every failure to write.
/// With a `limit`, a `target` that is a file starts over past it, so a long-lived daemon's log
/// stays bounded.
#[cfg(unix)]
fn drain(mut source: std::fs::File, mut target: std::fs::File, limit: Option<u64>) {
    use std::io::{Read, Seek};
    let mut buffer = [0_u8; 8192];
    loop {
        let read = match source.read(&mut buffer) {
            Ok(0) => return,
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        if limit.is_some_and(|limit| {
            target
                .metadata()
                .is_ok_and(|metadata| metadata.is_file() && metadata.len() >= limit)
        }) {
            let _ = target.set_len(0);
            let _ = target.rewind();
            let _ = target.write_all(b"agitd: output log started over\n");
        }
        let _ = target.write_all(&buffer[..read]);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::fd::IntoRawFd;

    const CHILD: &str = "AGIT_DAEMON_GUARD_TEST_CHILD";
    const TEST: &str =
        "rc::daemon_guard::tests::a_daemon_survives_a_closed_stdout_and_a_peer_that_hangs_up";

    /// A daemon whose stdout and stderr lead to a reader that is gone, and whose peer hangs up in
    /// the middle of a write, goes on: printing and writing fail on their own, and neither ends
    /// the process. What it starts gets SIGPIPE at its default. Without the handler the first
    /// failing write ends the process with no word; without the output thread the first `println!`
    /// panics.
    #[test]
    fn a_daemon_survives_a_closed_stdout_and_a_peer_that_hangs_up() {
        let Some(done) = std::env::var_os(CHILD) else {
            let folder = tempfile::tempdir().unwrap();
            let done = folder.path().join("done");
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env(CHILD, &done)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            assert_eq!(
                std::fs::read_to_string(&done).ok().as_deref(),
                Some("true"),
                "{status}"
            );
            return;
        };

        // As `main` leaves it for every `agit` process; a test binary starts with it ignored.
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
        // Stdout and stderr lead to a pipe nobody reads any more.
        let (read, write) = std::io::pipe().unwrap();
        drop(read);
        let write = write.into_raw_fd();
        unsafe {
            libc::dup2(write, 1);
            libc::dup2(write, 2);
            libc::close(write);
        }
        let _guard = install();

        for _ in 0..200 {
            println!("what a push prints");
            eprintln!("what a push prints");
        }
        // A client that hung up, and a child that went away before reading its stdin.
        let (mut client, peer) = std::os::unix::net::UnixStream::pair().unwrap();
        drop(peer);
        assert!(client.write_all(b"an answer").is_err());
        let mut child = std::process::Command::new("true")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        child.wait().unwrap();
        assert!(stdin.write_all(&[0; 1 << 17]).is_err());

        // A program it starts dies of a closed pipe as it would anywhere: `yes` ends by the signal
        // at the default, and exits with an error of its own if the signal were ignored.
        use std::os::unix::process::ExitStatusExt;
        let (read, write) = std::io::pipe().unwrap();
        drop(read);
        let spawned = std::process::Command::new("yes")
            .stdout(write)
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        std::fs::write(&done, (spawned.signal() == Some(libc::SIGPIPE)).to_string()).unwrap();
    }

    /// A panic leaves the thread and the place in the daemon's log, and nothing of its message,
    /// which can quote what the daemon was handling.
    #[test]
    fn a_panic_logs_where_it_happened_and_not_its_message() {
        const NAME: &str =
            "rc::daemon_guard::tests::a_panic_logs_where_it_happened_and_not_its_message";
        const CANARY: &str = "CANARY-PANIC-MESSAGE-7c1e";
        let Some(log) = std::env::var_os("AGIT_DAEMON_GUARD_TEST_LOG") else {
            let folder = tempfile::tempdir().unwrap();
            let path = folder.path().join("agitd.log");
            let file = std::fs::File::create(&path).unwrap();
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--nocapture"])
                .env("AGIT_DAEMON_GUARD_TEST_LOG", &path)
                .stdout(file.try_clone().unwrap())
                .stderr(file)
                .status()
                .unwrap();
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(
                text.contains("agitd: panic in worker at src/rc/daemon_guard.rs:"),
                "{text}"
            );
            assert!(!text.contains(CANARY), "{text}");
            return;
        };
        let _ = log;
        let _guard = install();
        let thread = std::thread::Builder::new()
            .name("worker".into())
            .spawn(|| panic!("{CANARY}"))
            .unwrap();
        assert!(thread.join().is_err());
        // The output thread writes on its own time.
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    /// A daemon that stops says so: what it printed reaches the log before it exits, and so does
    /// what it prints after, such as why it refused to start. Without the guard both can be lost
    /// with the process.
    #[test]
    fn a_daemon_that_stops_loses_nothing_it_printed() {
        const NAME: &str = "rc::daemon_guard::tests::a_daemon_that_stops_loses_nothing_it_printed";
        let Some(log) = std::env::var_os("AGIT_DAEMON_GUARD_TEST_STOP") else {
            let folder = tempfile::tempdir().unwrap();
            let path = folder.path().join("agitd.log");
            let file = std::fs::File::create(&path).unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--nocapture"])
                .env("AGIT_DAEMON_GUARD_TEST_STOP", &path)
                .stdout(file.try_clone().unwrap())
                .stderr(file)
                .status()
                .unwrap();
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(status.success(), "{text}");
            assert!(text.contains("while it ran"), "{text}");
            assert!(text.contains("why it stopped"), "{text}");
            return;
        };
        let guard = install();
        eprintln!("while it ran");
        drop(guard);
        // Back on the log itself: nothing has to outrun the process's exit to get there.
        eprintln!("why it stopped");
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::fstat(2, &mut stat) }, 0);
        use std::os::unix::fs::MetadataExt;
        if stat.st_ino as u64 != std::fs::metadata(&log).unwrap().ino() {
            std::process::exit(1);
        }
        std::process::exit(0);
    }

    /// The daemon's output log starts over past its limit instead of growing for as long as the
    /// daemon runs.
    #[test]
    fn the_daemons_output_log_stays_bounded() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("agitd.log");
        let target = std::fs::File::create(&path).unwrap();
        let (read, mut write) = std::io::pipe().unwrap();
        let source = unsafe {
            use std::os::fd::FromRawFd;
            std::fs::File::from_raw_fd(read.into_raw_fd())
        };
        let thread = std::thread::spawn(move || drain(source, target, Some(1000)));
        for _ in 0..100 {
            write.write_all(&[b'x'; 100]).unwrap();
        }
        drop(write);
        thread.join().unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() < 10_000 - 1000);
    }

    /// Only the log `spawn_daemon` made starts over. A file the caller redirected stderr to can
    /// hold what came before this daemon, so a full one only grows; so does a file in the same
    /// folder that `spawn_daemon` didn't name.
    #[test]
    fn a_daemon_only_starts_over_the_log_made_for_it() {
        const NAME: &str = "rc::daemon_guard::tests::a_daemon_only_starts_over_the_log_made_for_it";
        if std::env::var_os("AGIT_DAEMON_GUARD_TEST_OWN").is_none() {
            let home = tempfile::tempdir().unwrap();
            let folder = home.path().join("rc");
            std::fs::create_dir_all(&folder).unwrap();
            let history = vec![b'h'; LOG_BYTES as usize];
            for (path, own) in [
                (folder.join("agitd-a.log"), true),
                (folder.join("agitd-a.txt"), false),
                (home.path().join("shared.log"), false),
            ] {
                std::fs::write(&path, &history).unwrap();
                let log = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap();
                let status = std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", NAME, "--nocapture"])
                    .env("AGIT_DAEMON_GUARD_TEST_OWN", "1")
                    .env("AGIT_HOME", home.path())
                    .stdout(std::process::Stdio::null())
                    .stderr(log)
                    .status()
                    .unwrap();
                assert!(status.success(), "{}", path.display());
                let text = std::fs::read(&path).unwrap();
                assert!(text.ends_with(b"a line\n"), "{}", path.display());
                assert_eq!(text.len() < history.len(), own, "{}", path.display());
            }
            return;
        }
        let guard = install();
        eprintln!("a line");
        drop(guard);
    }
}
