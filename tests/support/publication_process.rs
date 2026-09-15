//! A publication fixture owns its child tree until both output pipes close and cleanup is verified.

use std::io::{self, PipeReader, Read};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

#[cfg(windows)]
use crate::publication_windows_job as windows_job;

pub const CHILD_LIMIT: Duration = Duration::from_secs(30);
pub const MODE_LIMIT: Duration = Duration::from_secs(120);
const CLEANUP_LIMIT: Duration = Duration::from_secs(4);
const OUTPUT_LIMIT: usize = 4 * 1024 * 1024;

pub fn output(command: Command, mode: &str, stage: &str, deadline: Instant) -> io::Result<Output> {
    let deadline = deadline.min(Instant::now() + CHILD_LIMIT);
    eprintln!("publication mode={mode} stage={stage} start");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = {
        let _entered = runtime.enter();
        capture(command, mode, stage, deadline)
    };
    // No asynchronous pipe reads are started; teardown must not add an unbounded runtime wait.
    runtime.shutdown_timeout(Duration::ZERO);
    match &result {
        Ok(output) => eprintln!(
            "publication mode={mode} stage={stage} done status={:?} stdout_bytes={} stderr_bytes={}",
            output.status.code(),
            output.stdout.len(),
            output.stderr.len()
        ),
        Err(error) => eprintln!("publication mode={mode} stage={stage} failed: {error}"),
    }
    result
}

struct Process {
    child: tokio::process::Child,
    #[cfg(unix)]
    group: Option<i32>,
    #[cfg(windows)]
    job: Option<windows_job::Job>,
}

impl Process {
    fn gone(&mut self) -> io::Result<bool> {
        if self.child.try_wait()?.is_none() {
            return Ok(false);
        }
        #[cfg(unix)]
        {
            Ok(self.group.is_none_or(|group| {
                let result = unsafe { libc::killpg(group, 0) };
                result == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            }))
        }
        #[cfg(windows)]
        {
            Ok(self
                .job
                .as_ref()
                .expect("a Windows fixture owns its Job")
                .active_processes()?
                == 0)
        }
    }

    fn terminate(&mut self) {
        #[cfg(unix)]
        if let Some(group) = self.group {
            unsafe { libc::killpg(group, libc::SIGKILL) };
        }
        #[cfg(windows)]
        if let Some(job) = &self.job {
            let _ = job.terminate();
        }
        let _ = self.child.start_kill();
    }

    fn cleanup(&mut self) -> io::Result<()> {
        let deadline = Instant::now() + CLEANUP_LIMIT;
        if !self.gone()? {
            self.terminate();
        }
        loop {
            if self.gone()? {
                #[cfg(unix)]
                {
                    self.group = None;
                }
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(io::Error::other("child-tree cleanup could not be verified"));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.terminate();
        #[cfg(windows)]
        if let Some(job) = self.job.take() {
            // An expired cleanup budget must not enter the ordinary Job destructor's drain loop.
            let _ = job.terminate_and_close();
        }
    }
}

#[cfg(unix)]
fn read_ready(file: &mut PipeReader, buffer: &mut [u8]) -> io::Result<Option<usize>> {
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
    // Only this reader owns the pipe; readable bytes or EOF cannot be consumed elsewhere.
    file.read(buffer).map(Some)
}

#[cfg(windows)]
fn read_ready(file: &mut PipeReader, buffer: &mut [u8]) -> io::Result<Option<usize>> {
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
    // Peek and read share a sole reader, so this read cannot wait for additional bytes.
    let count = buffer.len().min(available as usize);
    // An empty Windows pipe write can complete a read without closing the writer.
    file.read(&mut buffer[..count])
        .map(|count| (count != 0).then_some(count))
}

#[derive(Default)]
struct Streams {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    out_closed: bool,
    err_closed: bool,
}

fn drain_one(
    file: &mut PipeReader,
    bytes: &mut Vec<u8>,
    closed: &mut bool,
    remaining: usize,
) -> io::Result<()> {
    if *closed {
        return Ok(());
    }
    let mut buffer = [0; 8192];
    match read_ready(file, &mut buffer) {
        Ok(Some(0)) => *closed = true,
        Ok(Some(count)) => {
            if count > remaining {
                return Err(io::Error::other("child output exceeded its byte limit"));
            }
            bytes.extend_from_slice(&buffer[..count]);
        }
        Ok(None) => {}
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
            ) => {}
        Err(error) => return Err(error),
    }
    Ok(())
}

fn capture(command: Command, mode: &str, stage: &str, deadline: Instant) -> io::Result<Output> {
    if Instant::now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "mode deadline elapsed before spawn",
        ));
    }
    // Readiness polling requires synchronous handles with no outstanding asynchronous reads.
    let (mut stdout, stdout_writer) = io::pipe()?;
    let (mut stderr, stderr_writer) = io::pipe()?;
    let mut command = tokio::process::Command::from(command);
    command
        .stdout(stdout_writer)
        .stderr(stderr_writer)
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    let job = windows_job::Job::new()?;
    #[cfg(windows)]
    windows_job::Job::configure(&mut command);
    let child = command.spawn()?;
    // The observer must not retain writers after spawn or EOF cannot prove child closure.
    drop(command);
    let pid = child.id().expect("a newly spawned child has a process ID");
    let mut tree = Process {
        child,
        #[cfg(unix)]
        group: Some(pid as i32),
        #[cfg(windows)]
        job: Some(job),
    };
    eprintln!("publication mode={mode} stage={stage} spawned pid={pid}");
    let mut streams = Streams::default();
    let result = (|| {
        #[cfg(windows)]
        tree.job.as_ref().unwrap().attach_and_resume(&tree.child)?;
        loop {
            // Check time even when the child continually supplies ready output.
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "child deadline elapsed",
                ));
            }
            let remaining = OUTPUT_LIMIT - streams.stdout.len() - streams.stderr.len();
            drain_one(
                &mut stdout,
                &mut streams.stdout,
                &mut streams.out_closed,
                remaining,
            )?;
            let remaining = OUTPUT_LIMIT - streams.stdout.len() - streams.stderr.len();
            drain_one(
                &mut stderr,
                &mut streams.stderr,
                &mut streams.err_closed,
                remaining,
            )?;
            if let Some(status) = tree.child.try_wait()?
                && streams.out_closed
                && streams.err_closed
            {
                // Closing output does not complete a Windows fixture while its Job owns work.
                // Cleanup must not kill that work and turn an incomplete run into success.
                #[cfg(windows)]
                if tree.job.as_ref().unwrap().active_processes()? != 0 {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
                return Ok(status);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    })();
    let direct_exit = tree
        .child
        .try_wait()
        .map(|status| status.map(|status| status.code()));
    let cleanup = tree.cleanup();
    eprintln!(
        "publication mode={mode} stage={stage} direct_exit={direct_exit:?} reaped={} stdout_bytes={} stderr_bytes={} stdout_eof={} stderr_eof={}",
        cleanup.is_ok(),
        streams.stdout.len(),
        streams.stderr.len(),
        streams.out_closed,
        streams.err_closed
    );
    let status = match (result, cleanup) {
        (Ok(status), Ok(())) => status,
        (Err(error), Ok(())) => return Err(error),
        (result, Err(error)) => {
            return Err(io::Error::other(format!(
                "child result={result:?}; cleanup failed: {error}"
            )));
        }
    };
    Ok(Output {
        status,
        stdout: streams.stdout,
        stderr: streams.stderr,
    })
}
