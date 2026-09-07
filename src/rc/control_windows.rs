//! Local daemon control through a current-user Windows named pipe.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_NO_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED,
    ERROR_PIPE_LISTENING, GENERIC_READ, GENERIC_WRITE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile,
    SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT, WriteFile,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId,
    GetNamedPipeServerProcessId, PIPE_NOWAIT, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
    PeekNamedPipe, SetNamedPipeHandleState,
};

pub use super::control_protocol::{Reply, Request, SessionLine, Status};
use super::windows_security::{self as security, Handle};

const TIMEOUT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(10);
const FRAME_LIMIT: usize = 4 * 1024 * 1024;
const PIPE_BUFFER: u32 = 65536;

pub fn socket_path() -> crate::Result<PathBuf> {
    socket_path_for(&super::rc_dir()?)
}

pub fn socket_path_for(rc_dir: &Path) -> crate::Result<PathBuf> {
    use sha2::{Digest, Sha256};

    let sid = security::current_sid()?;
    let mut hash = Sha256::new();
    hash.update(sid.as_bytes());
    hash.update(security::directory_identity(rc_dir)?);
    Ok(PathBuf::from(format!(
        r"\\.\pipe\agit-rc-{}",
        hex::encode(hash.finalize())
    )))
}

pub fn pid_path() -> crate::Result<PathBuf> {
    Ok(super::rc_dir()?.join("agitd.pid"))
}

pub fn write_pidfile() -> crate::Result<()> {
    std::fs::write(pid_path()?, std::process::id().to_string())?;
    Ok(())
}

pub fn clear_pidfile() {
    if let Ok(path) = pid_path() {
        let _ = std::fs::remove_file(path);
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Presence {
    Running(u32),
    Absent,
    Unclear(String),
}

pub fn running_pid() -> Option<u32> {
    match presence() {
        Presence::Running(pid) => Some(pid),
        _ => None,
    }
}

pub fn presence() -> Presence {
    match super::rc_dir() {
        Ok(directory) => presence_in(&directory),
        Err(error) => Presence::Unclear(error.to_string()),
    }
}

pub fn presence_in(rc_dir: &Path) -> Presence {
    let result = (|| -> crate::Result<(Reply, u32)> {
        let path = socket_path_for(rc_dir)?;
        exchange(&path, &Request::Status, TIMEOUT)
    })();
    match result {
        Ok((_, pid)) => Presence::Running(pid),
        Err(error)
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|error| error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32)) =>
        {
            Presence::Absent
        }
        Err(error) => Presence::Unclear(error.to_string()),
    }
}

pub fn ask(request: &Request) -> crate::Result<Reply> {
    ask_with_timeout(request, TIMEOUT)
}

pub(crate) fn ask_with_timeout(request: &Request, timeout: Duration) -> crate::Result<Reply> {
    exchange(&socket_path()?, request, timeout).map(|(reply, _)| reply)
}

fn exchange(path: &Path, request: &Request, timeout: Duration) -> crate::Result<(Reply, u32)> {
    let deadline = Instant::now() + timeout;
    let path = security::wide(path)?;
    let sid = security::current_sid()?;
    let handle = loop {
        let raw = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                std::ptr::null_mut(),
            )
        };
        match Handle::new(raw) {
            Ok(handle) => break handle,
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => pause(deadline)?,
            Err(error) => return Err(error.into()),
        }
    };
    let mut pid = 0;
    if unsafe { GetNamedPipeServerProcessId(handle.0, &mut pid) } == 0 {
        return Err(io::Error::last_os_error().into());
    }
    security::require_process_user(pid, &sid)?;
    let mode = PIPE_NOWAIT;
    if unsafe { SetNamedPipeHandleState(handle.0, &mode, std::ptr::null(), std::ptr::null()) } == 0
    {
        return Err(io::Error::last_os_error().into());
    }
    write_frame(&handle, request, deadline)?;
    let reply = read_frame(&handle, deadline)?;
    Ok((serde_json::from_slice(&reply)?, pid))
}

pub struct Listener {
    handle: Arc<Handle>,
    sid: String,
}

pub fn listen() -> crate::Result<Listener> {
    let path = security::wide(socket_path()?)?;
    let sid = security::current_sid()?;
    let descriptor = security::private_descriptor(&sid, false)?;
    let attributes = security::attributes(&descriptor);
    let handle = Handle::new(unsafe {
        CreateNamedPipeW(
            path.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_NOWAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            PIPE_BUFFER,
            PIPE_BUFFER,
            TIMEOUT.as_millis() as u32,
            &attributes,
        )
    })?;
    Ok(Listener {
        handle: Arc::new(handle),
        sid,
    })
}

impl Listener {
    pub fn incoming(&self) -> Incoming<'_> {
        Incoming(self)
    }
}

pub struct Incoming<'a>(&'a Listener);

impl Iterator for Incoming<'_> {
    type Item = io::Result<Stream>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let connected = unsafe { ConnectNamedPipe(self.0.handle.0, std::ptr::null_mut()) } != 0;
            let error = io::Error::last_os_error();
            if connected {
                std::thread::sleep(POLL);
                continue;
            }
            if error.raw_os_error() == Some(ERROR_PIPE_CONNECTED as i32) {
                let mut pid = 0;
                let result =
                    if unsafe { GetNamedPipeClientProcessId(self.0.handle.0, &mut pid) } == 0 {
                        Err(io::Error::last_os_error())
                    } else {
                        security::require_process_user(pid, &self.0.sid)
                    };
                if let Err(error) = result {
                    unsafe { DisconnectNamedPipe(self.0.handle.0) };
                    return Some(Err(error));
                }
                return Some(Ok(Stream {
                    handle: self.0.handle.clone(),
                }));
            }
            match error.raw_os_error().map(|code| code as u32) {
                Some(ERROR_PIPE_LISTENING) => std::thread::sleep(POLL),
                Some(ERROR_NO_DATA) => {
                    unsafe { DisconnectNamedPipe(self.0.handle.0) };
                }
                _ => return Some(Err(error)),
            }
        }
    }
}

pub struct Stream {
    handle: Arc<Handle>,
}

impl Drop for Stream {
    fn drop(&mut self) {
        unsafe { DisconnectNamedPipe(self.handle.0) };
    }
}

pub fn serve_one(stream: &mut Stream, handle: impl FnOnce(Request) -> Reply) -> crate::Result<()> {
    let request = read_frame(&stream.handle, Instant::now() + TIMEOUT)?;
    let reply = handle(serde_json::from_slice(&request)?);
    let deadline = Instant::now() + TIMEOUT;
    write_frame(&stream.handle, &reply, deadline)?;
    // Disconnect discards unread bytes; the client closes only after consuming its reply.
    loop {
        let mut available = 0;
        if unsafe {
            PeekNamedPipe(
                stream.handle.0,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            )
        } == 0
        {
            let error = io::Error::last_os_error();
            if matches!(
                error.raw_os_error().map(|code| code as u32),
                Some(ERROR_BROKEN_PIPE | ERROR_NO_DATA)
            ) {
                return Ok(());
            }
            return Err(error.into());
        }
        pause(deadline)?;
    }
}

fn read_frame(handle: &Handle, deadline: Instant) -> io::Result<Vec<u8>> {
    let mut frame = Vec::new();
    loop {
        let mut bytes = [0u8; 4096];
        let mut count = 0;
        let read = unsafe {
            ReadFile(
                handle.0,
                bytes.as_mut_ptr(),
                bytes.len() as u32,
                &mut count,
                std::ptr::null_mut(),
            )
        };
        if read == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_NO_DATA as i32) {
                return Err(error);
            }
        }
        if count > 0 {
            let start = frame.len();
            frame.extend_from_slice(&bytes[..count as usize]);
            if frame.len() > FRAME_LIMIT {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "local RC frame exceeds the size limit",
                ));
            }
            if let Some(end) = bytes[..count as usize]
                .iter()
                .position(|byte| *byte == b'\n')
            {
                frame.truncate(start + end);
                return Ok(frame);
            }
        } else {
            pause(deadline)?;
        }
        if Instant::now() >= deadline {
            return Err(timed_out());
        }
    }
}

fn write_frame(
    handle: &Handle,
    value: &impl serde::Serialize,
    deadline: Instant,
) -> crate::Result<()> {
    let mut frame = serde_json::to_vec(value)?;
    frame.push(b'\n');
    if frame.len() > FRAME_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "local RC frame exceeds the size limit",
        )
        .into());
    }
    let mut offset = 0;
    while offset < frame.len() {
        let mut count = 0;
        let wrote = unsafe {
            WriteFile(
                handle.0,
                frame[offset..].as_ptr(),
                (frame.len() - offset) as u32,
                &mut count,
                std::ptr::null_mut(),
            )
        };
        if wrote == 0 {
            return Err(io::Error::last_os_error().into());
        }
        offset += count as usize;
        if offset < frame.len() {
            pause(deadline)?;
        }
    }
    Ok(())
}

fn timed_out() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "local RC pipe did not answer before the deadline",
    )
}

fn pause(deadline: Instant) -> io::Result<()> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(timed_out());
    }
    std::thread::sleep(POLL.min(left));
    Ok(())
}
