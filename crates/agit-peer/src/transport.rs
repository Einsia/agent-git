//! Endpoint TLS runs in the daemon; the independent worker sees only ciphertext.

use crate::{Identity, PeerCertificate, protocol};
use agit_tunnel::{Connection, Packet};
use anyhow::{Context, ensure};
use futures_util::{SinkExt, StreamExt};
use std::{
    pin::Pin,
    task::{Context as TaskContext, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf},
    sync::mpsc,
};

const CHUNK_SIZE: usize = 64 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// Dropping the endpoint closes the pipe and cancels both transport pumps.
pub struct ByteStream {
    stream: DuplexStream,
    tasks: Vec<tokio::task::JoinHandle<anyhow::Result<()>>>,
}

impl Drop for ByteStream {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl ByteStream {
    pub fn new(connection: Connection) -> Self {
        let (mut sink, mut source) = connection.split();
        let (stream, transport) = tokio::io::duplex(CHUNK_SIZE);
        let (mut input, mut output) = tokio::io::split(transport);
        let (controls, mut pending) = mpsc::channel(4);
        let sending = async move {
            let mut bytes = vec![0; CHUNK_SIZE];
            loop {
                let packet = tokio::select! {
                    count = input.read(&mut bytes) => {
                        let count = count?;
                        if count == 0 { return Ok(()) }
                        Packet::Binary(bytes[..count].to_vec())
                    }
                    Some(packet) = pending.recv() => packet,
                };
                tokio::time::timeout(IO_TIMEOUT, sink.send(packet)).await??;
            }
        };
        let receiving = async move {
            while let Some(packet) = source.next().await {
                match packet? {
                    Packet::Binary(bytes) => {
                        ensure!(
                            bytes.len() <= CHUNK_SIZE,
                            "peer tunnel chunk exceeds its size limit"
                        );
                        tokio::time::timeout(IO_TIMEOUT, output.write_all(&bytes)).await??;
                    }
                    Packet::Ping(bytes) => {
                        controls
                            .try_send(Packet::Pong(bytes))
                            .context("peer heartbeat queue is full")?;
                    }
                    Packet::Pong(_) => {}
                    Packet::Close(_) => return Ok(()),
                    _ => anyhow::bail!("peer tunnel accepts encrypted binary data only"),
                }
            }
            Ok(())
        };
        // Both halves must close together or an idle writer can hide transport EOF.
        let task = tokio::spawn(async move {
            tokio::select! {
                result = sending => result,
                result = receiving => result,
            }
        });
        Self {
            stream,
            tasks: vec![task],
        }
    }
}

impl AsyncRead for ByteStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(context, buffer)
    }
}

impl AsyncWrite for ByteStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(context, buffer)
    }
    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(context)
    }
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(context)
    }
}

#[derive(Clone, Copy)]
pub enum Role {
    Controller,
    Executor,
}

pub async fn authenticate(
    connection: Connection,
    identity: &Identity,
    peer: &PeerCertificate,
    role: Role,
) -> anyhow::Result<Connection> {
    let pid = connection.worker_pid;
    let stream = ByteStream::new(connection);
    let stream = match role {
        Role::Controller => tokio_rustls::TlsStream::Client(identity.connect(peer, stream).await?),
        Role::Executor => tokio_rustls::TlsStream::Server(identity.accept(peer, stream).await?),
    };
    Ok(framed(pid, stream))
}

pub fn framed<S>(worker_pid: u32, stream: S) -> Connection
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (input, output) = tokio::io::split(stream);
    let sink = futures_util::sink::unfold(output, |mut output, packet| async move {
        let Packet::Text(text) = packet else {
            anyhow::bail!("authenticated endpoint requires a JSON frame")
        };
        let frame: serde_json::Value =
            serde_json::from_str(&text).context("invalid endpoint JSON")?;
        protocol::write(&mut output, &frame).await?;
        Ok::<_, anyhow::Error>(output)
    });
    let source = futures_util::stream::unfold(Some(protocol::Reader::new(input)), |reader| async {
        let mut reader = reader?;
        match reader.read::<serde_json::Value>().await {
            Ok(Some(frame)) => Some((Ok(Packet::Text(frame.to_string())), Some(reader))),
            Ok(None) => None,
            Err(error) => Some((Err(error), None)),
        }
    });
    Connection::from_parts(worker_pid, Box::pin(sink), Box::pin(source))
}

#[cfg(test)]
mod tests;
