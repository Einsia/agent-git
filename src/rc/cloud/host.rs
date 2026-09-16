//! The host supervises independent presence and data workers for each cloud origin.

use super::store;
use agit_controller::{Authority, Connector, Opening, Worker};
use agit_peer::{
    client::{Client, Presence, join_data},
    cloud::{ConnectionGrant, Device, DeviceCredential, PresenceEvent, Secret},
    transport::{Role, authenticate},
};
use anyhow::{Context, ensure};
use std::{collections::HashMap, sync::Arc, time::Duration};
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

struct Task(tokio::task::JoinHandle<()>);
impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
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
                let origins = match tokio::task::spawn_blocking(store::origins).await {
                    Ok(Ok(origins)) => origins,
                    _ => {
                        record(&log, "cloud.enrollment_read_failed", serde_json::json!({}));
                        continue;
                    }
                };
                tasks.retain(|hub, _| origins.contains(hub));
                for hub in origins {
                    if tasks.get(&hub).is_some_and(|task| !task.0.is_finished()) {
                        continue;
                    }
                    let (worker, incoming, log, slots) =
                        (worker.clone(), incoming.clone(), log.clone(), slots.clone());
                    let origin = hub.clone();
                    tasks.insert(
                        hub,
                        Task(tokio::spawn(async move {
                            run_executor(origin, worker, incoming, slots, log).await;
                        })),
                    );
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
) {
    let (_lifetime, stopped) = watch::channel(());
    let mut backoff = Duration::from_millis(250);
    let mut children = tokio::task::JoinSet::new();
    loop {
        let attempt = async {
            super::commands::enroll_pending(&hub).await?;
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
                    Some(_) = children.join_next(), if !children.is_empty() => {},
                    offer = presence.next() => {
                        let PresenceEvent::Offer { link_id, source_id, ticket, grant_token } = offer? else { continue };
                        let Ok(permit) = slots.clone().try_acquire_owned() else {
                            record(&log, "cloud.offer_capacity", serde_json::json!({"hub":hub,"link_id":link_id}));
                            continue;
                        };
                        let (hub, worker, incoming, log, stopped) = (hub.clone(), worker.clone(), incoming.clone(), log.clone(), stopped.clone());
                        children.spawn(async move {
                            let _permit = permit;
                            let accepted = async {
                                let origin = hub.clone();
                                let enrollment = tokio::task::spawn_blocking(move || store::load(&origin)).await??
                                    .context("cloud enrollment is missing")?;
                                ensure!(enrollment.inbound_enabled, "inbound cloud connections are disabled");
                                let api = Client::new(&hub)?;
                                let grant = api.verify(&enrollment.credential, &grant_token).await?;
                                ensure!(grant.source.id == source_id, "cloud offer source does not match its grant");
                                let raw = worker.open(api.data_config(&enrollment.credential)?).await?;
                                let raw = join_data(raw, &link_id, ticket).await?;
                                let connection = authenticate(raw, &enrollment.identity, &grant.source.certificate, Role::Executor).await?;
                                record(&log, "cloud.endpoint_authenticated", serde_json::json!({"hub":hub,"link_id":link_id,"grant_id":grant.id,"source_id":source_id,"worker_pid":connection.worker_pid}));
                                let renewal = Some(Renewal { api, credential: enrollment.credential, token: grant_token });
                                incoming.try_send(Authenticated { connection, grant, stopped, renewal })
                                    .map_err(|_| anyhow::anyhow!("executor ingress is full or stopped"))?;
                                Ok::<_, anyhow::Error>(())
                            }.await;
                            if accepted.is_err() { record(&log, "cloud.endpoint_rejected", serde_json::json!({"hub":hub,"link_id":link_id,"source_id":source_id})); }
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
        let jitter = u64::from(uuid::Uuid::new_v4().as_bytes()[0]);
        tokio::time::sleep(backoff + Duration::from_millis(jitter)).await;
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
