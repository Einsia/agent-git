//! A worker performs one connection attempt and reports transport outcomes.

use super::{PacketSink, PacketSource, protocol::*};
use anyhow::{Context, bail, ensure};
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

const IO_TIMEOUT: Duration = Duration::from_secs(30);

pub async fn run<R, W>(input: R, mut output: W) -> crate::Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut input = Reader::new(input, MAX_RECORD);
    let result = run_inner(&mut input, &mut output).await;
    if let Err(error) = &result {
        let message: String = error.to_string().chars().take(512).collect();
        let _ =
            tokio::time::timeout(IO_TIMEOUT, write(&mut output, &Event::Failed { message })).await;
    }
    result
}

async fn run_inner<R, W>(input: &mut Reader<R>, output: &mut W) -> crate::Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let Some(Command::Open { version, config }) =
        tokio::time::timeout(IO_TIMEOUT, input.read()).await??
    else {
        bail!("tunnel requires an opening configuration");
    };
    ensure!(version == VERSION, "unsupported tunnel worker protocol");
    config.validate()?;
    let (mut sink, mut source) = tokio::time::timeout(IO_TIMEOUT, connect(config)).await??;
    tokio::time::timeout(
        IO_TIMEOUT,
        write(
            output,
            &Event::Connected {
                version: VERSION,
                worker_pid: std::process::id(),
            },
        ),
    )
    .await??;
    let (events, mut pending) = tokio::sync::mpsc::channel(8);
    let written = events.clone();
    let sending = async {
        loop {
            let Some(command) = input.read::<Command>().await? else {
                return Ok(());
            };
            let Command::Send { serial, packet } = command else {
                bail!("tunnel is already connected")
            };
            packet.validate()?;
            tokio::time::timeout(IO_TIMEOUT, sink.send(packet)).await??;
            written.send(Event::Written { serial }).await?;
        }
    };
    let receiving = async {
        while let Some(packet) = source.next().await {
            let packet = packet?;
            packet.validate()?;
            events.send(Event::Received { packet }).await?;
        }
        bail!("tunnel peer disconnected")
    };
    let writing = async {
        while let Some(event) = pending.recv().await {
            tokio::time::timeout(IO_TIMEOUT, write(output, &event)).await??;
        }
        Ok(())
    };
    // Independent reads and writes keep a full-duplex peer from deadlocking
    // when both sides send more than the transport's socket buffer can hold.
    tokio::select! { result = sending => result, result = receiving => result, result = writing => result }
}

async fn connect(config: Config) -> crate::Result<(PacketSink, PacketSource)> {
    match config {
        Config::WebSocket {
            url,
            headers,
            direct,
        } => {
            let mut request = url
                .into_client_request()
                .context("invalid WebSocket tunnel request")?;
            for (key, value) in headers {
                request.headers_mut().insert(
                    http::header::HeaderName::from_bytes(key.as_bytes())
                        .context("invalid tunnel header name")?,
                    http::HeaderValue::from_str(&value).context("invalid tunnel header value")?,
                );
            }
            let socket = super::websocket::connect(request, direct).await?;
            let (sink, source) = socket.split();
            let sink = sink
                .with(|packet| async move { Ok::<_, anyhow::Error>(to_websocket(packet)) })
                .sink_map_err(anyhow::Error::from);
            let source = source.filter_map(|message| async move {
                match message {
                    Ok(message) => from_websocket(message).map(Ok),
                    Err(error) => Some(Err(error.into())),
                }
            });
            Ok((Box::pin(sink), Box::pin(source)))
        }
        Config::Ssh {
            host,
            command: remote,
        } => {
            let mut command = tokio::process::Command::new("ssh");
            command
                .args([
                    "-T",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ConnectTimeout=10",
                    "-o",
                    "ServerAliveInterval=15",
                    "-o",
                    "ServerAliveCountMax=3",
                ])
                .arg(host)
                .arg(format!(
                    "exec {}",
                    remote
                        .iter()
                        .map(|arg| quote(arg))
                        .collect::<Vec<_>>()
                        .join(" ")
                ));
            // The SSH child stays in the worker's process group so killing the
            // worker also terminates the channel after an abrupt worker failure.
            command
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true);
            let mut child = command.spawn().context("cannot start SSH tunnel")?;
            let stdin = child.stdin.take().context("SSH input is unavailable")?;
            let stdout = child.stdout.take().context("SSH output is unavailable")?;
            let owner = std::sync::Arc::new(SshChild(std::sync::Mutex::new(Some(child))));
            let sink_owner = owner.clone();
            let sink = futures_util::sink::unfold(
                (stdin, sink_owner),
                |(mut stdin, owner), packet| async move {
                    let Packet::Text(text) = packet else {
                        bail!("SSH tunnel requires a text record")
                    };
                    ensure!(
                        !text.contains('\n') && !text.contains('\r'),
                        "SSH tunnel requires one record per packet"
                    );
                    stdin.write_all(text.as_bytes()).await?;
                    stdin.write_all(b"\n").await?;
                    stdin.flush().await?;
                    Ok::<_, anyhow::Error>((stdin, owner))
                },
            );
            let source = futures_util::stream::unfold(
                (Reader::new(BufReader::new(stdout), MAX_PAYLOAD + 1), owner),
                |(mut reader, owner)| async move {
                    match reader.record().await {
                        Ok(Some(bytes)) => Some((
                            String::from_utf8(bytes)
                                .map(|s| Packet::Text(s.trim_end_matches(['\r', '\n']).to_owned()))
                                .map_err(anyhow::Error::from),
                            (reader, owner),
                        )),
                        Ok(None) => None,
                        Err(error) => Some((Err(error), (reader, owner))),
                    }
                },
            );
            Ok((Box::pin(sink), Box::pin(source)))
        }
    }
}

struct SshChild(std::sync::Mutex<Option<tokio::process::Child>>);
impl Drop for SshChild {
    fn drop(&mut self) {
        if let Some(mut child) = self
            .0
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = child.start_kill();
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
        }
    }
}

pub(super) fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub(super) fn to_websocket(packet: Packet) -> Message {
    match packet {
        Packet::Text(text) => Message::Text(text.into()),
        Packet::Binary(bytes) => Message::Binary(bytes.into()),
        Packet::Ping(bytes) => Message::Ping(bytes.into()),
        Packet::Pong(bytes) => Message::Pong(bytes.into()),
        Packet::Close(close) => Message::Close(close.map(|close| {
            tokio_tungstenite::tungstenite::protocol::CloseFrame {
                code: close.code.into(),
                reason: close.reason.into(),
            }
        })),
    }
}

pub(super) fn from_websocket(message: Message) -> Option<Packet> {
    Some(match message {
        Message::Text(text) => Packet::Text(text.to_string()),
        Message::Binary(bytes) => Packet::Binary(bytes.to_vec()),
        Message::Ping(bytes) => Packet::Ping(bytes.to_vec()),
        Message::Pong(bytes) => Packet::Pong(bytes.to_vec()),
        Message::Close(reason) => Packet::Close(reason.map(|r| Close {
            code: r.code.into(),
            reason: r.reason.to_string(),
        })),
        Message::Frame(_) => return None,
    })
}
