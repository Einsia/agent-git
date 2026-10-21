//! Cloud sockets carry authenticated principals through the shared executor mux.

use super::{Client, ClientOutput, Incoming, MAX_FRAME, MAX_PENDING};
use crate::protocol::Frame;
use crate::rc::{
    cloud::{host, ingress, refusal::Refusal},
    diagnostics::Log,
};
use agit_peer::cloud::ConnectionGrant;
use agit_tunnel::Packet;
use anyhow::{Context, ensure};
use futures_util::{SinkExt, StreamExt};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{
    sync::{mpsc, watch},
    time::Instant,
};

/// How long a refused controller may take to send the request its refusal answers.
const REFUSAL_WINDOW: Duration = Duration::from_secs(5);
const MAX_REFUSALS: usize = 16;
const REMEMBERED_REFUSALS: usize = 256;

#[cfg(all(test, unix))]
mod tests;

pub(super) struct Ingress {
    pub incoming: mpsc::Receiver<host::Authenticated>,
    pub registry: ingress::Registry,
    refused: HashMap<String, (Instant, Refusal)>,
    refusals: Arc<tokio::sync::Semaphore>,
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
            refused: HashMap::new(),
            refusals: Arc::new(tokio::sync::Semaphore::new(MAX_REFUSALS)),
            _service: Some(service),
        })
    }

    #[cfg(all(test, unix))]
    pub fn fixed(
        incoming: mpsc::Receiver<host::Authenticated>,
        registry: ingress::Registry,
    ) -> Self {
        Self {
            incoming,
            registry,
            refused: HashMap::new(),
            refusals: Arc::new(tokio::sync::Semaphore::new(MAX_REFUSALS)),
            _service: None,
        }
    }

    /// A lease that redials before its refusal can change is answered without a new admission.
    pub fn remembered(&mut self, grant: &ConnectionGrant) -> Option<Refusal> {
        let now = Instant::now();
        self.refused.retain(|_, (until, _)| *until > now);
        self.refused
            .get(&lease_scope(grant))
            .map(|(_, refusal)| refusal.clone())
    }

    pub fn remember(&mut self, grant: &ConnectionGrant, refusal: &Refusal) {
        let Some(retention) = refusal.retention() else {
            return;
        };
        if self.refused.len() >= REMEMBERED_REFUSALS
            && let Some(oldest) = self
                .refused
                .iter()
                .min_by_key(|(_, (until, _))| *until)
                .map(|(scope, _)| scope.clone())
        {
            self.refused.remove(&oldest);
        }
        self.refused.insert(
            lease_scope(grant),
            (Instant::now() + retention, refusal.clone()),
        );
    }

    /// Answers the controller's first request with the refusal, then closes the tunnel.
    ///
    /// A controller that understands the refusal stops or paces its redials; one that does not
    /// sees a failed handshake, exactly as when the tunnel closes unanswered. Returns false when
    /// too many refusals are in flight and the tunnel is closed unanswered.
    pub fn refuse(&self, accepted: host::Authenticated, refusal: &Refusal) -> bool {
        let Ok(permit) = self.refusals.clone().try_acquire_owned() else {
            return false;
        };
        let error = refusal.rpc_error();
        tokio::spawn(async move {
            let _permit = permit;
            let (mut sink, mut source) = accepted.connection.split();
            let _ = tokio::time::timeout(REFUSAL_WINDOW, async {
                while let Some(Ok(packet)) = source.next().await {
                    let Packet::Text(text) = packet else {
                        if matches!(packet, Packet::Ping(_) | Packet::Pong(_)) {
                            continue;
                        }
                        return;
                    };
                    let Ok(frame) = serde_json::from_str::<Frame>(&text) else {
                        return;
                    };
                    if let Some(id) = frame.id.filter(|_| frame.method.is_some()) {
                        let response = Frame::error_response(id, error);
                        let _ = sink.send(Packet::Text(response.to_json())).await;
                        return;
                    }
                }
            })
            .await;
        });
        true
    }
}

/// Each admission uses a fresh grant, so a refusal is remembered for the controller lease and
/// resource it names: a new lease or a new owner generation is always admitted afresh.
fn lease_scope(grant: &ConnectionGrant) -> String {
    serde_json::json!([
        grant.caller,
        grant.source.id,
        grant.source.credential_epoch,
        grant.source.certificate.fingerprint(),
        grant.target.id,
        grant.session_controller,
        grant.project_controller,
    ])
    .to_string()
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
    let (stop, mut stopped) = watch::channel(());
    let (output, mut messages) = ClientOutput::channel(stop);
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
            let mut first_response = true;
            while let Some(message) = messages.next().await? {
                let super::output::Next::Write(message) = message else {
                    if let super::output::Next::Recover(frame) = message {
                        if let Some(log) = &log {
                            log.record("cloud.event_recovery_requested", serde_json::json!({"client_id":client,"stream":frame.params.as_ref().and_then(|p|p.get("session_id")),"after_seq":frame.params.as_ref().and_then(|p|p.get("after_seq"))}));
                        }
                        input.send(Incoming::Request(client, frame)).await?;
                    }
                    continue;
                };
                let opening = first_response;
                first_response = false;
                let projecting = std::time::Instant::now();
                if opening && let Some(log) = &log {
                    log.record(
                        "cloud.initial_response_dequeued",
                        serde_json::json!({"client_id":client}),
                    );
                }
                if let Some(text) = projection.project(&message.record)? {
                    let frame_bytes = text.len();
                    let started = std::time::Instant::now();
                    if (opening || frame_bytes >= 16 * 1024)
                        && let Some(log) = &log
                    {
                        log.record(
                            "cloud.write_started",
                            serde_json::json!({"client_id":client,"frame_bytes":frame_bytes,"initial_response":opening,"projection_ms":projecting.elapsed().as_secs_f64()*1000.0}),
                        );
                    }
                    let result = sink.send(Packet::Text(text)).await;
                    if (opening
                        || frame_bytes >= 16 * 1024
                        || result.is_err()
                        || started.elapsed() >= std::time::Duration::from_secs(1))
                        && let Some(log) = &log
                    {
                        log.record("cloud.write_completed", serde_json::json!({"client_id":client,"frame_bytes":frame_bytes,"elapsed_ms":started.elapsed().as_secs_f64()*1000.0,"succeeded":result.is_ok()}));
                    }
                    result?;
                }
            }
            Ok::<_, anyhow::Error>(())
        };
        let (reason, failure) = tokio::select! {
            result = renew_authority(renewal, grant, lease.clone(), log.as_ref()) => {
                if let (Some(log), Err(error)) = (&log, result) {
                    log.record("cloud.authorization_renewal_failed", serde_json::json!({"grant_id":accepted.grant.id,"reason":format!("{error:#}")}));
                }
                ("authorization_expired", None)
            },
            result = read => ("reader_closed", result.err()),
            result = write => ("writer_closed", result.err()),
            _ = stopped.changed() => ("output_capacity", None),
            _ = lifetime.changed() => ("enrollment_stopped", None),
        };
        // Revocation must precede cleanup that can wait for capacity in the shared input queue.
        drop(projection);
        if let Some(log) = log {
            log.record(
                "cloud.client_closed",
                serde_json::json!({"client_id":client,"reason":reason,"error":failure.map(|error| format!("{error:#}")),"grant_id":accepted.grant.id,"grant_expires_at_ms":lease.expires_at_ms()}),
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
        active: std::time::Instant::now(),
        requester: watch::channel(()).0,
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
        let mut retry = std::time::Duration::from_millis(250);
        let renewed = loop {
            match tokio::time::timeout_at(
                deadline,
                renewal
                    .api
                    .renew(&renewal.credential, &renewal.token, &grant),
            )
            .await?
            {
                Ok(renewed) => break renewed,
                Err(error) if agit_peer::client::is_transient(&error) => {
                    if let Some(log) = log {
                        log.record("cloud.authorization_renewal_retry", serde_json::json!({
                            "grant_id":grant.id,"reason":format!("{error:#}"),
                            "remaining_ms":deadline.saturating_duration_since(tokio::time::Instant::now()).as_millis(),
                        }));
                    }
                    // Retries never extend authority beyond the last verified grant.
                    tokio::time::sleep_until((tokio::time::Instant::now() + retry).min(deadline))
                        .await;
                    retry = (retry * 2).min(std::time::Duration::from_secs(2));
                }
                Err(error) => return Err(error),
            }
        };
        lease.renew(renewed.expires_at_ms)?;
        if let Some(log) = log {
            log.record("cloud.authorization_renewed", serde_json::json!({"grant_id":renewed.id,"grant_expires_at_ms":renewed.expires_at_ms}));
        }
        grant = renewed;
    }
}
