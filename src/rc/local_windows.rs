//! Owner RPC uses local named pipes with the same framing as Unix sockets.

use crate::infra::windows_security as security;
use std::{io, os::windows::io::AsRawHandle, path::PathBuf, time::Duration};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use windows_sys::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_NO_DATA, ERROR_PIPE_BUSY,
    ERROR_PIPE_NOT_CONNECTED,
};
use windows_sys::Win32::Storage::FileSystem::SECURITY_IDENTIFICATION;
use windows_sys::Win32::System::Pipes::{GetNamedPipeClientProcessId, GetNamedPipeServerProcessId};

pub type Stream = NamedPipeServer;

pub struct Listener {
    pending: Stream,
    path: PathBuf,
    sid: String,
}

fn create(path: &std::path::Path, sid: &str, first: bool) -> io::Result<Stream> {
    let descriptor = security::private_descriptor(sid, false)?;
    let mut attributes = security::attributes(&descriptor);
    // The descriptor lives through CreateNamedPipe; the kernel copies its ACL.
    unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(path, (&raw mut attributes).cast())
    }
}

pub fn listen() -> crate::Result<Listener> {
    let path = super::rpc_path()?;
    let sid = security::current_sid()?;
    Ok(Listener {
        pending: create(&path, &sid, true)?,
        path,
        sid,
    })
}

impl Listener {
    pub async fn accept(&mut self) -> io::Result<(Stream, ())> {
        loop {
            match self.pending.connect().await {
                Ok(()) => break,
                Err(error)
                    if matches!(
                        error.raw_os_error().map(|code| code as u32),
                        Some(ERROR_BROKEN_PIPE | ERROR_NO_DATA | ERROR_PIPE_NOT_CONNECTED)
                    ) =>
                {
                    let next = create(&self.path, &self.sid, false)?;
                    self.pending = next;
                }
                Err(error) => return Err(error),
            }
        }
        let next = create(&self.path, &self.sid, false)?;
        Ok((std::mem::replace(&mut self.pending, next), ()))
    }
}

pub fn authenticate_client(socket: &Stream) -> io::Result<()> {
    let mut pid = 0;
    if unsafe { GetNamedPipeClientProcessId(socket.as_raw_handle(), &mut pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    security::require_process_user(pid, &security::current_sid()?)
}

fn connect() -> crate::Result<NamedPipeClient> {
    connect_at(&super::rpc_path()?)
}

fn connect_at(path: &std::path::Path) -> crate::Result<NamedPipeClient> {
    let client = ClientOptions::new()
        .security_qos_flags(SECURITY_IDENTIFICATION)
        .open(path)?;
    let mut pid = 0;
    if unsafe { GetNamedPipeServerProcessId(client.as_raw_handle(), &mut pid) } == 0 {
        return Err(io::Error::last_os_error().into());
    }
    security::require_process_user(pid, &security::current_sid()?)?;
    Ok(client)
}

async fn wait_ready() -> crate::Result<NamedPipeClient> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        match connect() {
            Ok(client) => return Ok(client),
            Err(error) => {
                let retryable = error.downcast_ref::<io::Error>().is_some_and(|error| {
                    matches!(
                        error.raw_os_error().map(|code| code as u32),
                        Some(ERROR_FILE_NOT_FOUND | ERROR_PIPE_BUSY)
                    )
                });
                if !retryable || tokio::time::Instant::now() >= deadline {
                    return Err(error.context(format!(
                        "agitd did not become ready; inspect agitd-*.log in {}",
                        super::super::rc_dir()?.display()
                    )));
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The daemon must not retain the launcher's inheritable SSH pipe handles.
pub(super) fn spawn_daemon(log: std::fs::File) -> crate::Result<()> {
    use std::mem::{size_of, size_of_val};
    use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle};
    use windows_sys::Win32::System::Threading::{
        CREATE_BREAKAWAY_FROM_JOB, CreateProcessW, DETACHED_PROCESS, DeleteProcThreadAttributeList,
        EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, InitializeProcThreadAttributeList,
        PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION, STARTF_USESTDHANDLES,
        STARTUPINFOEXW, UpdateProcThreadAttribute,
    };
    let duplicate = |file: &std::fs::File| -> io::Result<security::Handle> {
        let mut handle = std::ptr::null_mut();
        let process = unsafe { GetCurrentProcess() };
        if unsafe {
            DuplicateHandle(
                process,
                file.as_raw_handle(),
                process,
                &mut handle,
                0,
                1,
                DUPLICATE_SAME_ACCESS,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        security::Handle::new(handle)
    };
    let input = duplicate(&std::fs::File::open("NUL")?)?;
    let output = duplicate(&log)?;
    let mut handles = [input.0, output.0];
    let mut bytes = 0;
    unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut bytes) };
    if bytes == 0 {
        return Err(io::Error::last_os_error().into());
    }
    let mut storage = vec![0usize; bytes.div_ceil(size_of::<usize>())];
    let list = storage.as_mut_ptr().cast();
    if unsafe { InitializeProcThreadAttributeList(list, 1, 0, &mut bytes) } == 0 {
        return Err(io::Error::last_os_error().into());
    }
    struct Attributes(*mut std::ffi::c_void);
    impl Drop for Attributes {
        fn drop(&mut self) {
            unsafe { DeleteProcThreadAttributeList(self.0) };
        }
    }
    let _attributes = Attributes(list);
    if unsafe {
        UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            handles.as_mut_ptr().cast(),
            size_of_val(&handles),
            std::ptr::null_mut(),
            std::ptr::null(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error().into());
    }
    use windows_sys::Win32::System::JobObjects::{
        JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, QueryInformationJobObject,
    };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    let queried = unsafe {
        QueryInformationJobObject(
            std::ptr::null_mut(),
            JobObjectExtendedLimitInformation,
            (&raw mut limits).cast(),
            size_of_val(&limits) as u32,
            std::ptr::null_mut(),
        )
    };
    // A host-imposed Job can prohibit breakaway; process creation must respect its limits.
    let breakaway = if queried != 0
        && limits.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_BREAKAWAY_OK != 0
    {
        CREATE_BREAKAWAY_FROM_JOB
    } else {
        0
    };
    let executable = std::env::current_exe()?;
    let application = security::wide(&executable)?;
    let mut arguments = std::ffi::OsString::from("\"");
    arguments.push(&executable);
    arguments.push("\" rc local start");
    let mut command_line = security::wide(arguments)?;
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = input.0;
    startup.StartupInfo.hStdOutput = output.0;
    startup.StartupInfo.hStdError = output.0;
    startup.lpAttributeList = list;
    let mut process = PROCESS_INFORMATION::default();
    // Detach only the owner daemon; its harnesses retain their own kill-on-close Jobs.
    if unsafe {
        CreateProcessW(
            application.as_ptr(),
            command_line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            DETACHED_PROCESS | breakaway | EXTENDED_STARTUPINFO_PRESENT,
            std::ptr::null(),
            std::ptr::null(),
            &startup.StartupInfo,
            &mut process,
        )
    } == 0
    {
        return Err(io::Error::last_os_error().into());
    }
    let _process = security::Handle::new(process.hProcess)?;
    let _thread = security::Handle::new(process.hThread)?;
    Ok(())
}

pub(super) fn wait_ready_sync() -> crate::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(wait_ready())?;
    Ok(())
}

pub(super) fn bridge() -> crate::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        let socket = connect()?;
        forward(socket, tokio::io::stdin(), tokio::io::stdout()).await?;
        Ok(())
    });
    // Stdin may still block in the runtime's input thread after RPC output closes.
    runtime.shutdown_background();
    result
}

async fn forward(
    socket: NamedPipeClient,
    mut input: impl tokio::io::AsyncRead + Unpin,
    mut output: impl tokio::io::AsyncWrite + Unpin,
) -> io::Result<()> {
    let (mut reader, mut writer) = tokio::io::split(socket);
    // Named pipes cannot half-close; EOF on either owner closes the connection.
    tokio::select! {
        result = copy_flushed(&mut reader, &mut output) => result,
        result = copy_flushed(&mut input, &mut writer) => result,
    }
}

async fn copy_flushed(
    reader: &mut (impl tokio::io::AsyncRead + Unpin),
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut bytes = [0; 32768];
    loop {
        let count = reader.read(&mut bytes).await?;
        if count == 0 {
            return Ok(());
        }
        writer.write_all(&bytes[..count]).await?;
        writer.flush().await?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn bridge_input_eof_closes_the_pipe_with_stdout_still_open() {
        let path = PathBuf::from(format!(r"\\.\pipe\agit-rpc-eof-{}", uuid::Uuid::new_v4()));
        let sid = security::current_sid().unwrap();
        let mut server = create(&path, &sid, true).unwrap();
        let client = connect_at(&path).unwrap();
        let (output, mut output_reader) = tokio::io::duplex(64);
        tokio::time::timeout(Duration::from_secs(5), async {
            let peer = async {
                server.connect().await.unwrap();
                let mut byte = [0];
                assert_eq!(server.read(&mut byte).await.unwrap(), 0);
            };
            let bridge = async { forward(client, tokio::io::empty(), output).await.unwrap() };
            tokio::join!(peer, bridge);
            let mut byte = [0];
            assert_eq!(output_reader.read(&mut byte).await.unwrap(), 0);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn owner_rpc_pipe_preserves_frames_across_connections() {
        let path = PathBuf::from(format!(r"\\.\pipe\agit-rpc-test-{}", uuid::Uuid::new_v4()));
        let sid = security::current_sid().unwrap();
        let mut listener = Listener {
            pending: create(&path, &sid, true).unwrap(),
            path: path.clone(),
            sid,
        };
        for _ in 0..2 {
            let client = async {
                let mut socket = connect_at(&path).unwrap();
                socket.write_all(b"request\n").await.unwrap();
                let mut output = [0; 9];
                socket.read_exact(&mut output).await.unwrap();
                assert_eq!(&output, b"response\n");
            };
            let server = async {
                let (mut socket, _) = listener.accept().await.unwrap();
                authenticate_client(&socket).unwrap();
                let mut input = [0; 8];
                socket.read_exact(&mut input).await.unwrap();
                assert_eq!(&input, b"request\n");
                socket.write_all(b"response\n").await.unwrap();
                let mut end = [0];
                assert_eq!(socket.read(&mut end).await.unwrap(), 0);
            };
            tokio::time::timeout(Duration::from_secs(5), async {
                tokio::join!(client, server)
            })
            .await
            .unwrap();
        }
    }
}
