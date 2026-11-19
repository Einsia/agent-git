//! A Stop is admitted only after its bounded input reaches EOF.

use std::fs::File;
use std::io::{self, IsTerminal as _, Read as _};
use std::time::{Duration, Instant};

const BYTE_LIMIT: usize = 1024 * 1024;
const DEADLINE: Duration = Duration::from_secs(2);

#[derive(Debug, thiserror::Error)]
#[error(
    "automatic saving did not receive a complete Stop payload within its input limits; this Stop was not queued; run `agit save` to save it explicitly"
)]
pub(super) struct Unavailable;

pub(super) fn read() -> Result<Option<String>, Unavailable> {
    let stdin = io::stdin();
    if stdin.is_terminal() {
        return Err(Unavailable);
    }
    let mut input = unbuffered(&stdin).map_err(|_| Unavailable)?;
    let deadline = Instant::now() + DEADLINE;
    let mut bytes = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        if Instant::now() >= deadline {
            return Err(Unavailable);
        }
        match read_ready(&mut input, &mut buffer) {
            Ok(Some(0)) => break,
            Ok(Some(count)) => {
                if count > BYTE_LIMIT.saturating_sub(bytes.len()) {
                    return Err(Unavailable);
                }
                bytes.extend_from_slice(&buffer[..count]);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(Unavailable),
        }
    }
    if bytes.is_empty() {
        return Ok(None);
    }
    let payload = String::from_utf8(bytes).map_err(|_| Unavailable)?;
    serde_json::from_str::<serde_json::Value>(&payload).map_err(|_| Unavailable)?;
    Ok(Some(payload))
}

#[cfg(unix)]
fn unbuffered(stdin: &io::Stdin) -> io::Result<File> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    // The duplicate owns its descriptor and cannot close the process's stdin.
    let descriptor = unsafe { libc::dup(stdin.as_raw_fd()) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

#[cfg(windows)]
fn unbuffered(stdin: &io::Stdin) -> io::Result<File> {
    use std::os::windows::io::{AsRawHandle as _, BorrowedHandle};
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    let handle = stdin.as_raw_handle();
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "stdin is unavailable",
        ));
    }
    // Readiness must describe every retained byte, with no Stdin buffer hiding later input.
    let borrowed = unsafe { BorrowedHandle::borrow_raw(handle) };
    Ok(File::from(borrowed.try_clone_to_owned()?))
}

#[cfg(unix)]
fn read_ready(input: &mut File, bytes: &mut [u8]) -> io::Result<Option<usize>> {
    use std::os::fd::AsRawFd as _;
    let mut fd = libc::pollfd {
        fd: input.as_raw_fd(),
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
    // No other reader consumes stdin while the hook admits its one payload.
    input.read(bytes).map(Some)
}

#[cfg(windows)]
fn read_ready(input: &mut File, bytes: &mut [u8]) -> io::Result<Option<usize>> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE;
    use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_PIPE, GetFileType};
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;
    let handle = input.as_raw_handle();
    if unsafe { GetFileType(handle) } != FILE_TYPE_PIPE {
        return input.read(bytes).map(Some);
    }
    let mut available = 0;
    if unsafe {
        PeekNamedPipe(
            handle,
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
    let count = bytes.len().min(available as usize);
    input.read(&mut bytes[..count]).map(Some)
}
