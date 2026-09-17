//! Relay readiness gates received TLS bytes without delaying the controller's ClientHello.

use super::REQUEST_TIMEOUT;
use crate::{
    Identity, PeerCertificate,
    cloud::{DataJoin, DataReady, Secret},
    identity::HANDSHAKE_TIMEOUT,
    transport::{ByteStream, framed},
};
use agit_tunnel::{Connection, Packet};
use anyhow::{Context, ensure};
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::{sync::oneshot, time::Instant};

/// Send the relay join and TLS ClientHello in order, with independent pairing and TLS deadlines.
/// The duration records relay readiness; TLS setup overlaps it and must not be added to it.
pub async fn join_controller(
    connection: Connection,
    link_id: &str,
    ticket: Secret,
    identity: &Identity,
    peer: &PeerCertificate,
) -> anyhow::Result<(Connection, Duration)> {
    let started = Instant::now();
    let deadline = started + REQUEST_TIMEOUT;
    let pid = connection.worker_pid;
    let (mut sink, source) = connection.split();
    tokio::time::timeout_at(
        deadline,
        sink.send(Packet::Text(serde_json::to_string(&DataJoin { ticket })?)),
    )
    .await
    .context("cloud relay join timed out")??;

    let (ready, received) = oneshot::channel();
    let source = futures_util::stream::try_unfold(
        (source, Some((link_id.to_owned(), ready))),
        |(mut source, mut pending)| async move {
            loop {
                let Some(packet) = source.next().await else {
                    return Ok(None);
                };
                let packet = packet?;
                if pending.is_some() {
                    match packet {
                        Packet::Text(text) => {
                            let frame: DataReady = serde_json::from_str(&text)
                                .context("invalid cloud relay ready frame")?;
                            let (link, ready) = pending.take().unwrap();
                            ensure!(
                                frame.link_id == link,
                                "cloud relay paired another connection"
                            );
                            let _ = ready.send(Instant::now());
                            continue;
                        }
                        Packet::Ping(_) | Packet::Pong(_) => {}
                        _ => anyhow::bail!("cloud tunnel closed or sent an invalid handshake"),
                    }
                }
                return Ok(Some((packet, (source, pending))));
            }
        },
    );
    let stream = ByteStream::new(Connection::from_parts(pid, sink, Box::pin(source)));
    let handshake = identity.connecting(peer, stream)?;
    let ready = tokio::time::timeout_at(deadline, received);
    tokio::pin!(handshake, ready);
    let (stream, paired_at) = tokio::select! {
        stream = &mut handshake => {
            let stream = stream.context("peer authentication failed")?;
            let paired_at = ready.await.context("cloud relay pairing timed out")?
                .context("cloud relay closed before readiness")?;
            (stream, paired_at)
        }
        paired_at = &mut ready => {
            let paired_at = paired_at.context("cloud relay pairing timed out")?
                .context("cloud relay closed before readiness")?;
            let stream = tokio::time::timeout_at(paired_at + HANDSHAKE_TIMEOUT, handshake)
                .await.context("peer authentication timed out")?
                .context("peer authentication failed")?;
            (stream, paired_at)
        }
    };
    Ok((framed(pid, stream), paired_at.duration_since(started)))
}

#[cfg(test)]
mod tests;
