//! Machine output stays bounded while both upload pipes make progress.

use super::{Outcome, ProcessOutput};
use std::fs::File;
use std::io::{self, Read, Write};
use std::process::Child;
use std::time::{Duration, Instant};

const CAPTURE_LIMIT: usize = 1024 * 1024;
const EXIT_DRAIN_GRACE: Duration = Duration::from_secs(1);

struct Pipe {
    file: File,
    bytes: Vec<u8>,
    closed: bool,
    error: Option<String>,
}

impl Pipe {
    fn new(file: File) -> Self {
        Self {
            file,
            bytes: Vec::new(),
            closed: false,
            error: None,
        }
    }

    fn pump(&mut self, forward: bool) -> io::Result<bool> {
        if self.closed {
            return Ok(false);
        }
        let mut buffer = [0; 8192];
        match read_ready(&mut self.file, &mut buffer) {
            Ok(Some(0)) => self.closed = true,
            Ok(Some(count)) => {
                let kept = count.min(CAPTURE_LIMIT - self.bytes.len());
                self.bytes.extend_from_slice(&buffer[..kept]);
                if kept != count {
                    self.error.get_or_insert_with(|| {
                        "publication output exceeded its capture limit".into()
                    });
                }
                if forward {
                    let mut sink = io::stderr();
                    let _ = sink.write_all(&buffer[..count]);
                    let _ = sink.flush();
                }
                return Ok(true);
            }
            Ok(None) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => {
                self.error = Some(format!("cannot read publication output: {error}"));
                return Err(error);
            }
        }
        Ok(false)
    }
}

#[cfg(unix)]
fn read_ready(file: &mut File, buffer: &mut [u8]) -> io::Result<Option<usize>> {
    use std::os::fd::AsRawFd;
    let mut fd = libc::pollfd {
        fd: file.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut fd, 1, 0) };
    if ready < 0 {
        return Err(io::Error::last_os_error());
    }
    if ready == 0 {
        return Ok(None);
    }
    // This pipe has a sole reader, so readable data or EOF cannot be consumed elsewhere.
    file.read(buffer).map(Some)
}

#[cfg(windows)]
fn read_ready(file: &mut File, buffer: &mut [u8]) -> io::Result<Option<usize>> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE;
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;
    let mut available = 0;
    if unsafe {
        PeekNamedPipe(
            file.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut available,
            std::ptr::null_mut(),
        )
    } == 0
    {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) {
            Ok(Some(0))
        } else {
            Err(error)
        };
    }
    if available == 0 {
        return Ok(None);
    }
    let count = buffer.len().min(available as usize);
    file.read(&mut buffer[..count]).map(Some)
}

pub(super) fn capture(mut child: Child) -> ProcessOutput {
    let stdout = child.stdout.take().expect("publication stdout is piped");
    let stderr = child.stderr.take().expect("publication stderr is piped");
    #[cfg(unix)]
    let (stdout, stderr) = {
        use std::os::fd::OwnedFd;
        (
            File::from(OwnedFd::from(stdout)),
            File::from(OwnedFd::from(stderr)),
        )
    };
    #[cfg(windows)]
    let (stdout, stderr) = {
        use std::os::windows::io::OwnedHandle;
        (
            File::from(OwnedHandle::from(stdout)),
            File::from(OwnedHandle::from(stderr)),
        )
    };
    let mut stdout = Pipe::new(stdout);
    let mut stderr = Pipe::new(stderr);
    let mut status = None;
    let mut deadline = None;
    let mut error = None;
    loop {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            error = Some("publication output did not close after the Git process stopped".into());
            break;
        }
        let progress = match (stdout.pump(false), stderr.pump(true)) {
            (Ok(out), Ok(err)) => out || err,
            _ => {
                let _ = child.kill();
                error = Some("publication output could not be read completely".into());
                break;
            }
        };
        if status.is_none() {
            match child.try_wait() {
                Ok(Some(exit)) => {
                    status = Some(exit);
                    deadline = Some(Instant::now() + EXIT_DRAIN_GRACE);
                }
                Ok(None) => {}
                Err(wait_error) => {
                    let _ = child.kill();
                    error = Some(format!(
                        "cannot observe publication process exit: {wait_error}"
                    ));
                    break;
                }
            }
        }
        if status.is_some() && stdout.closed && stderr.closed {
            break;
        }
        if !progress {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    // Pipe ownership ends here even when a descendant retains a writer; no reader is joined.
    if status.is_none() {
        let until = Instant::now() + EXIT_DRAIN_GRACE;
        let _ = child.kill();
        loop {
            match child.try_wait() {
                Ok(Some(exit)) => {
                    status = Some(exit);
                    break;
                }
                _ if Instant::now() < until => std::thread::sleep(Duration::from_millis(1)),
                _ => break,
            }
        }
    }
    let complete = error.is_none()
        && stdout.error.is_none()
        && stderr.error.is_none()
        && stdout.closed
        && stderr.closed
        && status.is_some();
    let error = error.or(stdout.error).or(stderr.error);
    ProcessOutput {
        outcome: Outcome {
            code: status.and_then(|status| status.code()).unwrap_or(1),
            stderr: String::from_utf8_lossy(&stderr.bytes).into_owned(),
        },
        stdout: stdout.bytes,
        complete,
        error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    const MODE: &str = "AGIT_PUBLICATION_OUTPUT_CHILD";
    const CHILD_LIMIT: Duration = Duration::from_secs(30);

    fn command(mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "hub::git::publication_output::tests::output_child",
                "--nocapture",
            ])
            .env(MODE, mode)
            .stdin(Stdio::null());
        command
    }

    fn child(mode: &str) -> Child {
        command(mode)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    #[test]
    fn output_child() {
        match std::env::var(MODE).as_deref() {
            Ok("flood") => {
                io::stdout().write_all(&vec![b'X'; 128 * 1024]).unwrap();
                io::stderr().write_all(&vec![b'Y'; 128 * 1024]).unwrap();
            }
            Ok("overflow") => {
                io::stdout()
                    .write_all(&vec![b'X'; CAPTURE_LIMIT + 8192])
                    .unwrap();
            }
            Ok("parent") => {
                let path = std::path::PathBuf::from(
                    std::env::var_os("AGIT_PUBLICATION_OUTPUT_SIGNALS").unwrap(),
                );
                let _writer = command("writer").spawn().unwrap();
                let deadline = Instant::now() + CHILD_LIMIT;
                while !path.join("ready").exists() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(1));
                }
                assert!(path.join("ready").exists());
                std::process::exit(0);
            }
            Ok("writer") => {
                let path = std::path::PathBuf::from(
                    std::env::var_os("AGIT_PUBLICATION_OUTPUT_SIGNALS").unwrap(),
                );
                std::fs::write(path.join("ready"), b"").unwrap();
                let deadline = Instant::now() + CHILD_LIMIT;
                while !path.join("release").exists() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(1));
                }
                std::fs::write(path.join("finished"), b"").unwrap();
            }
            _ => {}
        }
    }

    #[test]
    fn both_pipes_drain_and_overflow_remains_incomplete_after_successful_exit() {
        let result = capture(child("flood"));
        assert!(result.complete, "capture: {:?}", result.error);
        assert!(result.outcome.ok());
        assert_eq!(
            result.stdout.iter().filter(|byte| **byte == b'X').count(),
            128 * 1024
        );
        assert_eq!(
            result
                .outcome
                .stderr
                .bytes()
                .filter(|byte| *byte == b'Y')
                .count(),
            128 * 1024
        );
        let result = capture(child("overflow"));
        assert!(result.outcome.ok());
        assert!(!result.complete);
        assert!(result.error.as_deref().unwrap().contains("capture limit"));
        assert_eq!(result.stdout.len(), CAPTURE_LIMIT);
    }

    #[test]
    fn inherited_writers_cannot_extend_the_wait_after_git_exits() {
        let signals = tempfile::tempdir().unwrap();
        let child = command("parent")
            .env("AGIT_PUBLICATION_OUTPUT_SIGNALS", signals.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let result = capture(child);
        let released_early = signals.path().join("finished").exists();
        std::fs::write(signals.path().join("release"), b"").unwrap();
        let deadline = Instant::now() + CHILD_LIMIT;
        while !signals.path().join("finished").exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            signals.path().join("finished").exists(),
            "owned writer must finish"
        );
        assert!(
            !released_early,
            "capture must return while the writer still holds its pipes"
        );
        assert!(result.outcome.ok());
        assert!(!result.complete);
        assert!(result.error.as_deref().unwrap().contains("did not close"));
    }
}
