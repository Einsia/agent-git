//! The host supervises independent presence and data workers for each cloud origin.

use super::store;
use agit_controller::{Authority, Connector, Opening, Worker};
use agit_peer::{
    client::{Client, Presence, join_data, verified_transport},
    cloud::{ConnectionGrant, Device, DeviceCredential, PresenceEvent, Secret},
    transport::{Role, authenticate},
};
use anyhow::{Context, ensure};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, watch};

pub struct Authenticated {
    pub connection: agit_tunnel::Connection,
    pub grant: ConnectionGrant,
    pub stopped: watch::Receiver<()>,
    pub renewal: Option<Renewal>,
}

pub struct Renewal {
    pub api: Client,
    pub credential: DeviceCredential,
    pub token: Secret,
}

pub struct Service {
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Service {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Task {
    presence: tokio::task::JoinHandle<()>,
    enrollment: Option<tokio::task::JoinHandle<()>>,
    changed: watch::Sender<()>,
}
impl Drop for Task {
    fn drop(&mut self) {
        self.presence.abort();
        if let Some(enrollment) = &self.enrollment {
            enrollment.abort();
        }
    }
}

impl Service {
    pub(in crate::rc) fn start(
        worker: Worker,
        incoming: mpsc::Sender<Authenticated>,
        log: Option<super::super::diagnostics::Log>,
    ) -> Self {
        let task = tokio::spawn(async move {
            let mut tasks = HashMap::<String, Task>::new();
            let slots = Arc::new(tokio::sync::Semaphore::new(16));
            let mut refresh = tokio::time::interval(Duration::from_secs(5));
            refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                refresh.tick().await;
                let origins = match tokio::task::spawn_blocking(|| {
                    store::origins()?
                        .into_iter()
                        .map(|hub| {
                            let pending = store::inbound_pending(&hub)?;
                            Ok((hub, pending))
                        })
                        .collect::<crate::Result<Vec<_>>>()
                })
                .await
                {
                    Ok(Ok(origins)) => origins,
                    _ => {
                        record(&log, "cloud.enrollment_read_failed", serde_json::json!({}));
                        continue;
                    }
                };
                tasks.retain(|hub, _| origins.iter().any(|(origin, _)| origin == hub));
                for (hub, pending) in origins {
                    let task = tasks.entry(hub.clone()).or_insert_with(|| {
                        let (changed, receiver) = watch::channel(());
                        Task {
                            presence: tokio::spawn(run_executor(
                                hub.clone(),
                                worker.clone(),
                                incoming.clone(),
                                slots.clone(),
                                log.clone(),
                                receiver,
                            )),
                            enrollment: None,
                            changed,
                        }
                    });
                    if task.presence.is_finished() {
                        task.presence = tokio::spawn(run_executor(
                            hub.clone(),
                            worker.clone(),
                            incoming.clone(),
                            slots.clone(),
                            log.clone(),
                            task.changed.subscribe(),
                        ));
                    }
                    if pending && task.enrollment.as_ref().is_none_or(|job| job.is_finished()) {
                        let (changed, log) = (task.changed.clone(), log.clone());
                        task.enrollment = Some(tokio::spawn(async move {
                            match super::commands::enroll_pending(&hub).await {
                                Ok(replaced) => {
                                    record(
                                        &log,
                                        "cloud.enrollment_ready",
                                        serde_json::json!({"hub":hub,"credential_changed":replaced}),
                                    );
                                    if replaced {
                                        changed.send_replace(());
                                    }
                                }
                                Err(error) => record(
                                    &log,
                                    "cloud.enrollment_failed",
                                    serde_json::json!({"hub":hub,"http_status":error.downcast_ref::<agit_peer::client::HttpFailure>().map(|error| error.status)}),
                                ),
                            }
                        }));
                    }
                }
            }
        });
        Self { task }
    }
}

fn record(log: &Option<super::super::diagnostics::Log>, event: &str, metadata: serde_json::Value) {
    if let Some(log) = log {
        log.record(event, metadata);
    }
}

async fn run_executor(
    hub: String,
    worker: Worker,
    incoming: mpsc::Sender<Authenticated>,
    slots: Arc<tokio::sync::Semaphore>,
    log: Option<super::super::diagnostics::Log>,
    mut changed: watch::Receiver<()>,
) {
    let (lifetime, mut stopped) = watch::channel(());
    let mut backoff = Duration::from_millis(250);
    let mut children = tokio::task::JoinSet::new();
    loop {
        let attempt = async {
            let origin = hub.clone();
            let enrollment = tokio::task::spawn_blocking(move || store::load(&origin)).await??
                .context("cloud enrollment is missing")?;
            ensure!(enrollment.inbound_enabled, "inbound cloud connections are disabled");
            let api = Client::new(&hub)?;
            let raw = worker.open(api.presence_config(&enrollment.credential)?).await?;
            let mut presence = Presence::open(raw).await?;
            record(&log, "cloud.presence_connected", serde_json::json!({"hub":hub,"device_id":enrollment.credential.device.id,"epoch":presence.epoch,"worker_pid":presence.worker_pid}));
            backoff = Duration::from_millis(250);
            loop {
                tokio::select! {
                    _ = changed.changed() => {
                        lifetime.send_replace(());
                        stopped.borrow_and_update();
                        children.abort_all();
                        return Ok(());
                    }
                    Some(_) = children.join_next(), if !children.is_empty() => {},
                    offer = presence.next() => {
                        let PresenceEvent::Offer { link_id, source_id, ticket, grant_token, grant } = offer? else { continue };
                        let Ok(permit) = slots.clone().try_acquire_owned() else {
                            record(&log, "cloud.offer_capacity", serde_json::json!({"hub":hub,"link_id":link_id}));
                            continue;
                        };
                        let started = Instant::now();
                        let (hub, worker, incoming, log, stopped, api) = (hub.clone(), worker.clone(), incoming.clone(), log.clone(), stopped.clone(), api.clone());
                        children.spawn(async move {
                            let _permit = permit;
                            let mut phase = "enrollment";
                            let accepted = async {
                                let origin = hub.clone();
                                let enrollment = tokio::task::spawn_blocking(move || store::load(&origin)).await??
                                    .context("cloud enrollment is missing")?;
                                ensure!(enrollment.inbound_enabled, "inbound cloud connections are disabled");
                                let enrollment_ms = started.elapsed().as_secs_f64() * 1000.0;
                                phase = "admission";
                                let config = api.data_config(&enrollment.credential)?;
                                let verification_source = if grant.is_some() { "presence" } else { "http" };
                                let verification = async {
                                    let started = Instant::now();
                                    let grant = api.offered_grant(&enrollment.credential, &grant_token, grant.map(|grant| *grant)).await?;
                                    ensure!(grant.source.id == source_id, "cloud offer source does not match its grant");
                                    Ok::<_, anyhow::Error>((grant, started.elapsed().as_secs_f64() * 1000.0))
                                };
                                let transport = || async {
                                    let started = Instant::now();
                                    let raw = worker.open(config.clone()).await?;
                                    Ok::<_, anyhow::Error>((raw, started.elapsed().as_secs_f64() * 1000.0))
                                };
                                // Raw transport carries no executor authority before grant validation and endpoint TLS.
                                let ((grant, verification_ms), (raw, transport_ms), transport_reopened) = verified_transport(verification, transport).await?;
                                phase = "relay_pair";
                                let paired = Instant::now();
                                let raw = join_data(raw, &link_id, ticket).await?;
                                let pairing_ms = paired.elapsed().as_secs_f64() * 1000.0;
                                phase = "endpoint_tls";
                                let authenticated = Instant::now();
                                let connection = authenticate(raw, &enrollment.identity, &grant.source.certificate, Role::Executor).await?;
                                record(&log, "cloud.endpoint_authenticated", serde_json::json!({"hub":hub,"link_id":link_id,"grant_id":grant.id,"source_id":source_id,"worker_pid":connection.worker_pid,"enrollment_ms":enrollment_ms,"verification_source":verification_source,"verification_ms":verification_ms,"transport_ms":transport_ms,"transport_reopened":transport_reopened,"pairing_ms":pairing_ms,"tls_ms":authenticated.elapsed().as_secs_f64()*1000.0,"total_ms":started.elapsed().as_secs_f64()*1000.0}));
                                let renewal = Some(Renewal { api, credential: enrollment.credential, token: grant_token });
                                phase = "ingress";
                                incoming.try_send(Authenticated { connection, grant, stopped, renewal })
                                    .map_err(|_| anyhow::anyhow!("executor ingress is full or stopped"))?;
                                Ok::<_, anyhow::Error>(())
                            }.await;
                            if accepted.is_err() { record(&log, "cloud.endpoint_rejected", serde_json::json!({"hub":hub,"link_id":link_id,"source_id":source_id,"phase":phase,"total_ms":started.elapsed().as_secs_f64()*1000.0})); }
                        });
                    }
                }
            }
            #[allow(unreachable_code)]
            Ok::<_, anyhow::Error>(())
        }.await;
        record(
            &log,
            "cloud.presence_disconnected",
            serde_json::json!({"hub":hub,"failed":attempt.is_err(),"retry_ms":backoff.as_millis()}),
        );
        if attempt.is_ok() {
            backoff = Duration::from_millis(250);
            continue;
        }
        let jitter = u64::from(uuid::Uuid::new_v4().as_bytes()[0]);
        tokio::select! {
            _ = tokio::time::sleep(backoff + Duration::from_millis(jitter)) => {}
            _ = changed.changed() => {
                lifetime.send_replace(());
                stopped.borrow_and_update();
                children.abort_all();
                backoff = Duration::from_millis(250);
                continue;
            }
        }
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

pub struct Route {
    key: String,
    api: Client,
    target: Device,
}

impl Route {
    pub fn new(hub: &str, target: Device) -> crate::Result<Self> {
        let api = Client::new(hub)?;
        ensure!(
            target.owner.issuer == api.origin(),
            "cloud target issuer mismatch"
        );
        let key = serde_json::json!([api.origin(), target.id, target.certificate.fingerprint()])
            .to_string();
        Ok(Self { key, api, target })
    }
}

impl Connector for Route {
    fn key(&self) -> &str {
        &self.key
    }
    fn authority(&self) -> Authority {
        Authority::CloudPrincipal
    }
    fn open<'a>(&'a self, worker: &'a Worker) -> Opening<'a> {
        Box::pin(async move {
            let source = super::commands::controller(self.api.origin()).await?;
            let identity = Arc::new(source.identity);
            let mut refresh = false;
            loop {
                let credentials = Arc::new(agit_controller::cloud::Credentials {
                    identity: identity.clone(),
                    device: source.credential.clone(),
                    account: super::account_token(self.api.origin(), refresh).await?,
                });
                let route = agit_controller::cloud::Route::new(
                    self.api.clone(),
                    credentials,
                    self.target.clone(),
                )?;
                match route.open(worker).await {
                    Err(error)
                        if !refresh
                            && error
                                .downcast_ref::<agit_peer::client::HttpFailure>()
                                .is_some_and(|error| error.status == 401) =>
                    {
                        refresh = true;
                    }
                    result => return result,
                }
            }
        })
    }
}
