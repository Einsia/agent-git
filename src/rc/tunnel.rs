//! Adapt the Hub wire protocol to the independent tunnel worker contract.

use agit_tunnel::{Config, Connection, Packet, protocol::Close};
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use std::{
    path::{Path, PathBuf},
    pin::Pin,
};
use tokio_tungstenite::tungstenite::{
    Message, handshake::client::Request, protocol::frame::CloseFrame,
};

type SocketSink = Pin<Box<dyn Sink<Message, Error = anyhow::Error> + Send>>;
type SocketSource = Pin<Box<dyn Stream<Item = crate::Result<Message>> + Send>>;

pub fn worker_executable() -> crate::Result<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        Ok(format!("/proc/{}/exe", std::process::id()).into())
    }
    #[cfg(not(target_os = "linux"))]
    {
        Ok(std::env::current_exe()?)
    }
}

pub async fn connect(
    request: Request,
    executable: Option<&Path>,
) -> crate::Result<(SocketSink, SocketSource)> {
    let config = Config::WebSocket {
        url: request.uri().to_string(),
        direct: false,
        headers: request
            .headers()
            .iter()
            .map(|(k, v)| Ok((k.to_string(), v.to_str()?.to_owned())))
            .collect::<crate::Result<_>>()?,
    };
    #[cfg(feature = "cli")]
    crate::telemetry::allow_uploads();
    #[cfg(feature = "cli")]
    let started = std::time::Instant::now();
    let result = async {
        match executable {
            Some(executable) => Connection::open(config, executable, &["rc", "tunnel"]).await,
            None => {
                #[cfg(test)]
                {
                    Connection::in_process(config).await
                }
                #[cfg(not(test))]
                {
                    Connection::open(config, &worker_executable()?, &["rc", "tunnel"]).await
                }
            }
        }
    }
    .await;
    #[cfg(feature = "cli")]
    crate::telemetry::operation(
        crate::telemetry::Operation::RcConnect,
        result.is_ok(),
        started.elapsed(),
        None,
    );
    let connection: Connection = result?;
    let (sink, source) = connection.split();
    let sink = sink.with(|message| async move {
        Ok::<_, anyhow::Error>(match message {
            Message::Text(s) => Packet::Text(s.to_string()),
            Message::Binary(b) => Packet::Binary(b.to_vec()),
            Message::Ping(b) => Packet::Ping(b.to_vec()),
            Message::Pong(b) => Packet::Pong(b.to_vec()),
            Message::Close(c) => Packet::Close(c.map(|c| Close {
                code: c.code.into(),
                reason: c.reason.to_string(),
            })),
            Message::Frame(_) => anyhow::bail!("unsupported raw WebSocket frame"),
        })
    });
    let source = source.map(|packet| {
        packet.map(|packet| match packet {
            Packet::Text(s) => Message::Text(s.into()),
            Packet::Binary(b) => Message::Binary(b.into()),
            Packet::Ping(b) => Message::Ping(b.into()),
            Packet::Pong(b) => Message::Pong(b.into()),
            Packet::Close(close) => Message::Close(close.map(|close| CloseFrame {
                code: close.code.into(),
                reason: close.reason.into(),
            })),
        })
    });
    Ok((Box::pin(sink), Box::pin(source)))
}
