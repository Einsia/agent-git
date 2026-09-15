//! Cloud sockets carry authenticated principals through the shared executor mux.

use super::{CLIENT_QUEUE, Client, ClientOutput, Incoming, MAX_FRAME, MAX_PENDING};
use crate::rc::{
    cloud::{host, ingress},
    diagnostics::Log,
};
use agit_tunnel::Packet;
use anyhow::{Context, ensure};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::sync::{mpsc, watch};

#[cfg(test)]
mod tests;

pub(super) struct Ingress {
    pub incoming: mpsc::Receiver<host::Authenticated>,
    pub registry: ingress::Registry,
    _service: Option<host::Service>,
}

impl Ingress {
    pub fn start(log: Option<Log>) -> crate::Result<Self> {
        let (sender, incoming) = mpsc::channel(16);
        let registry = ingress::Registry::start(log.clone());
        let service = host::Service::start(crate::rc::peers::worker()?, sender, log);
        Ok(Self {
            incoming,
            registry,
            _service: Some(service),
        })
    }

    #[cfg(test)]
    pub fn fixed(
        incoming: mpsc::Receiver<host::Authenticated>,
        registry: ingress::Registry,
    ) -> Self {
        Self {
            incoming,
            registry,
            _service: None,
        }
    }
}

pub(super) fn attach(
    accepted: host::Authenticated,
    guard: ingress::Client,
    client: u64,
    input: mpsc::Sender<Incoming>,
    log: Option<Log>,
) -> Client {
    let (mut sink, mut source) = accepted.connection.split();
    let mut lifetime = accepted.stopped;
    let (sender, mut messages) =
        mpsc::channel::<(String, tokio::sync::OwnedSemaphorePermit)>(CLIENT_QUEUE);
    let (stop, mut stopped) = watch::channel(());
    let output = ClientOutput {
        sender,
        bytes: Arc::new(tokio::sync::Semaphore::new(MAX_FRAME * 2)),
        stop: Some(stop),
    };
    let projection = guard.clone();
    let lease = guard.lease();
    let renewal = accepted.renewal;
    let grant = accepted.grant.clone();
    let task = tokio::spawn(async move {
        let read = async {
            loop {
                let packet = source.next().await.context("cloud tunnel closed")??;
                let Packet::Text(text) = packet else {
                    anyhow::bail!("cloud endpoint requires an RPC frame")
                };
                ensure!(text.len() <= MAX_FRAME, "cloud RPC frame exceeds its limit");
                let frame = serde_json::from_str(&text)?;
                input
                    .send(Incoming::Request(client, Box::new(frame)))
                    .await?;
            }
            #[allow(unreachable_code)]
            Ok::<_, anyhow::Error>(())
        };
        let write = async {
            while let Some((record, _permit)) = messages.recv().await {
                if let Some(text) = projection.project(&record)? {
                    sink.send(Packet::Text(text)).await?;
                }
            }
            Ok::<_, anyhow::Error>(())
        };
        let reason = tokio::select! {
            result = renew_authority(renewal, grant, lease.clone(), log.as_ref()) => {
                if let (Some(log), Err(error)) = (&log, result) {
                    log.record("cloud.authorization_renewal_failed", serde_json::json!({"grant_id":accepted.grant.id,"reason":error.to_string()}));
                }
                "authorization_expired"
            },
            _ = read => "reader_closed",
            _ = write => "writer_closed",
            _ = stopped.changed() => "output_capacity",
            _ = lifetime.changed() => "enrollment_stopped",
        };
        // Revocation must precede cleanup that can wait for capacity in the shared input queue.
        drop(projection);
        if let Some(log) = log {
            log.record(
                "cloud.client_closed",
                serde_json::json!({"client_id":client,"reason":reason,"grant_id":accepted.grant.id,"grant_expires_at_ms":lease.expires_at_ms()}),
            );
        }
        let _ = input.send(Incoming::Closed(client)).await;
    });
    Client {
        output,
        task,
        peers: Default::default(),
        peer_slots: Arc::new(tokio::sync::Semaphore::new(MAX_PENDING)),
        cloud: Some(guard),
    }
}

async fn renew_authority(
    renewal: Option<host::Renewal>,
    mut grant: agit_peer::cloud::ConnectionGrant,
    lease: ingress::Lease,
    log: Option<&Log>,
) -> anyhow::Result<()> {
    let Some(renewal) = renewal else {
        tokio::time::sleep_until(lease.deadline()).await;
        anyhow::bail!("cloud grant expired without a renewal authority");
    };
    loop {
        let deadline = lease.deadline();
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        tokio::time::sleep(remaining / 2).await;
        let renewed = tokio::time::timeout_at(
            deadline,
            renewal
                .api
                .renew(&renewal.credential, &renewal.token, &grant),
        )
        .await??;
        lease.renew(renewed.expires_at_ms)?;
        if let Some(log) = log {
            log.record("cloud.authorization_renewed", serde_json::json!({"grant_id":renewed.id,"grant_expires_at_ms":renewed.expires_at_ms}));
        }
        grant = renewed;
    }
}
