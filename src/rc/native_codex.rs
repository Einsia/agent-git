//! Source-scoped clients share a native server without owning its lifetime.

use anyhow::{Context, ensure};
use futures_util::{Sink, SinkExt, StreamExt};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Semaphore, mpsc};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::{self, Message, protocol::WebSocketConfig};

use super::harness::proc::{Line, MAX_LINE_BYTES, MAX_PENDING_BYTES, QueuedLine, queue_line};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const START_TIMEOUT: Duration = Duration::from_secs(30);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub(crate) struct SharedServiceUnsupported;
impl std::fmt::Display for SharedServiceUnsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the selected Codex runtime cannot use its shared app-server listener")
    }
}
impl std::error::Error for SharedServiceUnsupported {}

/// Coordinates are resolved locally before a native request can be sent.
#[derive(Clone, Debug)]
pub struct Source {
    home: PathBuf,
    executable: PathBuf,
    socket: PathBuf,
}

impl Source {
    pub fn new(home: &Path, executable: &Path, socket: Option<&Path>) -> crate::Result<Self> {
        let home = home
            .canonicalize()
            .context("cannot resolve Codex runtime home")?;
        ensure!(home.is_dir(), "Codex runtime home is not a directory");
        let executable = executable
            .canonicalize()
            .context("cannot resolve Codex executable")?;
        ensure!(executable.is_file(), "Codex executable is not a file");
        let socket = match socket {
            Some(path) => {
                ensure!(path.is_absolute(), "Codex socket path must be absolute");
                path.to_path_buf()
            }
            None => home.join("app-server-control/app-server-control.sock"),
        };
        Ok(Self {
            home,
            executable,
            socket,
        })
    }

    pub fn home(&self) -> &Path {
        &self.home
    }
    pub fn socket(&self) -> &Path {
        &self.socket
    }
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    /// Native startup owns stale socket recovery; an accepting listener is never replaced.
    pub async fn connect_or_start(&self) -> crate::Result<Client> {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let capacity = std::mem::size_of::<libc::sockaddr_un>()
                - std::mem::offset_of!(libc::sockaddr_un, sun_path);
            if self.socket.as_os_str().as_bytes().len() >= capacity {
                ensure!(
                    self.socket == self.home.join("app-server-control/app-server-control.sock"),
                    "the registered Codex endpoint exceeds the local socket path limit"
                );
                return Err(SharedServiceUnsupported.into());
            }
        }
        match self.connect().await {
            Ok(client) => return Ok(client),
            Err(error) if socket_absent(&error) || self.stale_socket(&error).await => {}
            Err(error) => return Err(error),
        }
        ensure!(
            self.socket == self.home.join("app-server-control/app-server-control.sock"),
            "The registered Codex endpoint is unavailable; start its server before reconnecting"
        );
        self.require_shared_service().await?;
        let mut starter = self.spawn_persistent_server()?;
        let deadline = tokio::time::Instant::now() + START_TIMEOUT;
        loop {
            match self.connect().await {
                Ok(mut client) => {
                    client.starter = Some(starter);
                    return Ok(client);
                }
                Err(error) => {
                    let pending = socket_absent(&error)
                        || error.chain().any(|cause| {
                            cause.downcast_ref::<std::io::Error>().is_some_and(|error| {
                                error.kind() == std::io::ErrorKind::ConnectionRefused
                            })
                        });
                    if !pending || tokio::time::Instant::now() >= deadline {
                        return Err(error.context("Cannot attach to the shared Codex server"));
                    }
                    if let Some(status) = starter.try_wait()? {
                        // A competing launcher can own the reservation before its listener opens.
                        if status.success() {
                            return Err(error.context("Codex server exited before becoming ready"));
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    async fn stale_socket(&self, error: &anyhow::Error) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{FileTypeExt, MetadataExt};
            let refused = error.chain().any(|cause| {
                cause
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::ConnectionRefused)
            });
            if !refused {
                return false;
            }
            tokio::fs::symlink_metadata(&self.socket)
                .await
                .is_ok_and(|metadata| {
                    metadata.file_type().is_socket() && metadata.uid() == unsafe { libc::geteuid() }
                })
        }
        #[cfg(not(unix))]
        {
            let _ = error;
            false
        }
    }

    async fn require_shared_service(&self) -> crate::Result<()> {
        let output = tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio::process::Command::from(crate::infra::background::command(&self.executable))
                .args(["app-server", "--help"])
                .env("CODEX_HOME", &self.home)
                .stdin(std::process::Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .context("Codex shared service capability probe timed out")??;
        ensure!(
            output.status.success(),
            "Codex shared service capability probe failed"
        );
        let help = String::from_utf8_lossy(&output.stdout);
        if !help.contains("--listen") {
            return Err(SharedServiceUnsupported.into());
        }
        Ok(())
    }

    fn spawn_persistent_server(&self) -> crate::Result<tokio::process::Child> {
        let mut command = tokio::process::Command::new(&self.executable);
        command
            .args(["app-server", "--listen", "unix://"])
            .env("CODEX_HOME", &self.home)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(false);
        if std::env::var_os("CODEX_INTERNAL_ORIGINATOR_OVERRIDE").is_none() {
            command.env("CODEX_INTERNAL_ORIGINATOR_OVERRIDE", "codex_cli_rs");
        }
        // A shared executor must not inherit one conversation's settlement identity.
        for name in [
            "AGIT_SESSION",
            "AGIT_EXPECTED_AGENT_ID",
            "AGIT_LOCAL_AGENT_ID",
            "AGIT_MERGE_TX",
            "AGIT_MERGE_GENERATION",
            "AGIT_SETTLEMENT_NATIVE",
            "AGIT_SETTLEMENT_ARCHIVE_ROLE",
            "AGIT_RC_SUPERVISOR_COMMIT_RESULT",
            "AGIT_RC_SUPERVISOR_PREPARED",
            "AGIT_RC",
            "AGIT_RC_SUPERVISED_HOOK",
        ] {
            command.env_remove(name);
        }
        #[cfg(unix)]
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        #[cfg(windows)]
        command.creation_flags(0x00000008 | 0x00000200);
        command
            .spawn()
            .context("cannot start the persistent Codex server")
    }

    pub async fn connect(&self) -> crate::Result<Client> {
        tokio::time::timeout(CONNECT_TIMEOUT, self.connect_inner())
            .await
            .context("Codex shared-server connection timed out")?
    }

    #[cfg(unix)]
    async fn connect_inner(&self) -> crate::Result<Client> {
        let stream = tokio::net::UnixStream::connect(&self.socket)
            .await
            .context("cannot connect to the Codex control socket")?;
        ensure!(
            stream.peer_cred()?.uid() == unsafe { libc::geteuid() },
            "Codex control socket belongs to another OS account"
        );
        Client::handshake(stream).await
    }

    #[cfg(windows)]
    async fn connect_inner(&self) -> crate::Result<Client> {
        tokio::fs::symlink_metadata(&self.socket)
            .await
            .context("cannot inspect the Codex control socket")?;
        // The native proxy validates and opens Windows' Unix-domain socket transport.
        // It is a byte relay, so the WebSocket handshake still belongs to this client.
        let mut child = tokio::process::Command::new(&self.executable)
            .args(["app-server", "proxy", "--sock"])
            .arg(&self.socket)
            .env("CODEX_HOME", &self.home)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .context("Codex proxy stdin unavailable")?;
        let stdout = child
            .stdout
            .take()
            .context("Codex proxy stdout unavailable")?;
        let mut client = Client::handshake(tokio::io::join(stdout, stdin)).await?;
        client.proxy = Some(child);
        Ok(client)
    }

    #[cfg(not(any(unix, windows)))]
    async fn connect_inner(&self) -> crate::Result<Client> {
        anyhow::bail!("Native shared Codex sockets are unsupported on this platform")
    }
}

fn socket_absent(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    })
}

type Writer = std::pin::Pin<Box<dyn Sink<Message, Error = tungstenite::Error> + Send + Sync>>;

/// Dropping a client closes its subscription transport, never the shared executor.
pub struct Client {
    writer: Writer,
    lines: mpsc::Receiver<QueuedLine>,
    reader: tokio::task::JoinHandle<()>,
    // Child drop reaps the launcher without terminating the independent executor.
    starter: Option<tokio::process::Child>,
    #[cfg(windows)]
    proxy: Option<tokio::process::Child>,
}

impl Client {
    async fn handshake<S>(stream: S) -> crate::Result<Self>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Sync + Unpin + 'static,
    {
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_LINE_BYTES))
            .max_frame_size(Some(MAX_LINE_BYTES));
        let (socket, _) = tokio_tungstenite::client_async_with_config(
            "ws://codex-app-server/rpc",
            stream,
            Some(config),
        )
        .await
        .context("Codex shared-server WebSocket handshake failed")?;
        Ok(Self::from_socket(socket))
    }

    fn from_socket<S>(socket: WebSocketStream<S>) -> Self
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Sync + Unpin + 'static,
    {
        let (writer, mut reader) = socket.split();
        let (tx, lines) = mpsc::channel(1024);
        let budget = Arc::new(Semaphore::new(MAX_PENDING_BYTES));
        let reader = tokio::spawn(async move {
            while let Some(frame) = reader.next().await {
                let (line, bytes, terminal) = match frame {
                    Ok(Message::Text(text)) => match serde_json::from_str::<Value>(&text) {
                        Ok(value) if value.is_object() => (Line::Json(value), text.len(), false),
                        _ => (
                            Line::Fatal("Codex shared server sent invalid JSON-RPC".into()),
                            1,
                            true,
                        ),
                    },
                    Ok(Message::Close(_)) => break,
                    Ok(Message::Ping(_) | Message::Pong(_)) => continue,
                    Ok(_) => (
                        Line::Fatal("Codex shared server sent a non-text frame".into()),
                        1,
                        true,
                    ),
                    Err(_) => (
                        Line::Fatal("Codex shared-server transport failed".into()),
                        1,
                        true,
                    ),
                };
                if !queue_line(&tx, &budget, line, bytes).await || terminal {
                    return;
                }
            }
            queue_line(&tx, &budget, Line::Eof, 1).await;
        });
        Self {
            writer: Box::pin(writer),
            lines,
            reader,
            starter: None,
            #[cfg(windows)]
            proxy: None,
        }
    }

    #[cfg(all(test, unix))]
    pub(crate) fn started_pid(&self) -> Option<u32> {
        self.starter.as_ref().and_then(tokio::process::Child::id)
    }

    pub async fn send(&mut self, value: &Value) -> crate::Result<()> {
        let payload = serde_json::to_string(value)?;
        ensure!(
            payload.len() <= MAX_LINE_BYTES,
            "Codex request exceeds the transport limit"
        );
        self.writer.send(Message::Text(payload.into())).await?;
        Ok(())
    }

    pub(crate) async fn next(&mut self) -> Option<QueuedLine> {
        self.lines.recv().await
    }

    pub async fn close(&mut self) -> crate::Result<()> {
        let result = tokio::time::timeout(CLOSE_TIMEOUT, self.writer.close()).await;
        self.reader.abort();
        result.context("Codex client close timed out")??;
        Ok(())
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;

    fn source(home: &Path, socket: &Path) -> Source {
        Source::new(home, &std::env::current_exe().unwrap(), Some(socket)).unwrap()
    }

    #[tokio::test]
    async fn an_unaddressable_standard_socket_uses_the_private_runtime_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("native-home-".repeat(16));
        std::fs::create_dir(&home).unwrap();
        let standard = Source::new(&home, &std::env::current_exe().unwrap(), None).unwrap();
        let error = standard.connect_or_start().await.err().unwrap();
        assert!(error.is::<SharedServiceUnsupported>());

        let custom = source(&home, &home.join("custom.sock"));
        let error = custom.connect_or_start().await.err().unwrap();
        assert!(
            !error.is::<SharedServiceUnsupported>(),
            "a registered endpoint cannot select an unrelated private executor"
        );
    }

    #[tokio::test]
    async fn clients_share_one_listener_and_closing_one_preserves_the_other() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let mut tasks = vec![];
            for _ in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                tasks.push(tokio::spawn(async move {
                    let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                    while let Some(Ok(Message::Text(text))) = socket.next().await {
                        let request: Value = serde_json::from_str(&text).unwrap();
                        socket
                            .send(Message::Text(
                                json!({"id":request["id"],
                            "result":{"thread":{"id":"shared-thread"}}})
                                .to_string()
                                .into(),
                            ))
                            .await
                            .unwrap();
                    }
                }));
            }
            for task in tasks {
                task.await.unwrap();
            }
        });
        let source = source(dir.path(), &path);
        let mut a = source.connect().await.unwrap();
        let mut b = source.connect_or_start().await.unwrap();
        for client in [&mut a, &mut b] {
            client
                .send(&json!({"id":1,"method":"thread/resume"}))
                .await
                .unwrap();
            assert!(
                matches!(client.next().await.unwrap().into_line(), Line::Json(v)
                if v["result"]["thread"]["id"] == "shared-thread")
            );
        }
        a.close().await.unwrap();
        b.send(&json!({"id":2,"method":"thread/read"}))
            .await
            .unwrap();
        assert!(matches!(b.next().await.unwrap().into_line(), Line::Json(v) if v["id"] == 2));
        b.close().await.unwrap();
        tokio::time::timeout(CONNECT_TIMEOUT, server)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn malformed_native_frames_fail_closed_without_becoming_notices() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            socket.send(Message::Text("not-json".into())).await.unwrap();
        });
        let mut client = source(dir.path(), &path).connect().await.unwrap();
        assert!(matches!(
            client.next().await.unwrap().into_line(),
            Line::Fatal(_)
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn an_existing_non_codex_listener_is_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            use tokio::io::AsyncWriteExt;
            stream
                .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
        });
        let result = source(dir.path(), &path).connect_or_start().await;
        assert!(result.is_err());
        assert!(!socket_absent(&result.err().unwrap()));
        assert!(path.exists());
        server.await.unwrap();
    }
}
