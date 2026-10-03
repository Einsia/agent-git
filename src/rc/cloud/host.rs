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
    device: Option<Device>,
    presence: tokio::task::JoinHandle<()>,
    enrollment: Option<tokio::task::JoinHandle<()>>,
    publication: tokio::task::JoinHandle<()>,
    changed: watch::Sender<()>,
}
impl Drop for Task {
    fn drop(&mut self) {
        self.presence.abort();
        self.publication.abort();
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
        let publication_directory = store::directory().ok();
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
                            let enrollment = store::load(&hub)?;
                            let renamed = enrollment.as_ref().is_some_and(|enrollment| {
                                enrollment.inbound_enabled
                                    && super::super::identity::identity().is_ok_and(|identity| {
                                        identity.display_name
                                            != enrollment.credential.device.display_name
                                    })
                            });
                            let device = enrollment
                                .filter(|enrollment| enrollment.inbound_enabled)
                                .map(|enrollment| enrollment.credential.device);
                            Ok((hub, pending || renamed, device))
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
                retire_changed_enrollments(&mut tasks, &origins);
                for (hub, pending, device) in origins {
                    let task = tasks.entry(hub.clone()).or_insert_with(|| {
                        let (changed, receiver) = watch::channel(());
                        Task {
                            device,
                            presence: tokio::spawn(run_executor(
                                hub.clone(),
                                worker.clone(),
                                incoming.clone(),
                                slots.clone(),
                                log.clone(),
                                receiver,
                            )),
                            enrollment: None,
                            publication: spawn_publication(
                                &hub,
                                publication_directory.as_ref(),
                                &log,
                            ),
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
                    if task.publication.is_finished() {
                        task.publication =
                            spawn_publication(&hub, publication_directory.as_ref(), &log);
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

fn retire_changed_enrollments(
    tasks: &mut HashMap<String, Task>,
    origins: &[(String, bool, Option<Device>)],
) {
    tasks.retain(|hub, task| {
        origins.iter().any(|(origin, _, device)| {
            origin == hub && same_enrollment(task.device.as_ref(), device.as_ref())
        })
    });
}

fn same_enrollment(left: Option<&Device>, right: Option<&Device>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => {
            left.id == right.id
                && left.owner == right.owner
                && left.machine_id == right.machine_id
                && left.certificate == right.certificate
                && left.credential_epoch == right.credential_epoch
        }
        _ => false,
    }
}

fn spawn_publication(
    hub: &str,
    directory: Option<&std::path::PathBuf>,
    log: &Option<super::super::diagnostics::Log>,
) -> tokio::task::JoinHandle<()> {
    let (hub, directory, log) = (hub.to_owned(), directory.cloned(), log.clone());
    tokio::spawn(async move {
        if let Some(directory) = directory {
            super::publication::run(hub, directory, log).await;
        }
    })
}

fn record(log: &Option<super::super::diagnostics::Log>, event: &str, metadata: serde_json::Value) {
    if let Some(log) = log {
        log.record(event, metadata);
    }
}

async fn receive_presence(
    presence: &mut Presence,
    children: &mut tokio::task::JoinSet<()>,
) -> anyhow::Result<PresenceEvent> {
    // Reaping offer workers must not cancel a consumed ping's pending pong write.
    let receiving = presence.next();
    tokio::pin!(receiving);
    loop {
        tokio::select! {
            Some(_) = children.join_next(), if !children.is_empty() => {},
            offer = &mut receiving => return offer,
        }
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
            let transport_timing = raw.connect_timing;
            let mut presence = Presence::open(raw).await?;
            record(&log, "cloud.presence_connected", serde_json::json!({"hub":hub,"device_id":enrollment.credential.device.id,"epoch":presence.epoch,"worker_pid":presence.worker_pid,"transport_timing":transport_timing}));
            backoff = Duration::from_millis(250);
            loop {
                tokio::select! {
                    _ = changed.changed() => {
                        lifetime.send_replace(());
                        stopped.borrow_and_update();
                        children.abort_all();
                        return Ok(());
                    }
                    offer = receive_presence(&mut presence, &mut children) => {
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
                                let inline_grant = grant.is_some();
                                let verification_source = if grant.is_some() { "presence" } else { "http" };
                                let verification = async {
                                    let started = Instant::now();
                                    let grant = api.offered_grant(&enrollment.credential, &grant_token, grant.map(|grant| *grant)).await?;
                                    ensure!(grant.source.id == source_id, "cloud offer source does not match its grant");
                                    Ok::<_, anyhow::Error>((grant, started.elapsed().as_secs_f64() * 1000.0))
                                };
                                let transport = |config| async {
                                    let started = Instant::now();
                                    let raw = worker.open(config).await?;
                                    Ok::<_, anyhow::Error>((raw, started.elapsed().as_secs_f64() * 1000.0))
                                };
                                // A join-bearing upgrade must follow admission; speculative sockets carry no ticket.
                                let ((grant, verification_ms), (raw, transport_ms), transport_reopened) = if inline_grant {
                                    let grant = verification.await?;
                                    let config = api.admitted_data_config(&enrollment.credential, &grant.0, &ticket)?;
                                    (grant, transport(config).await?, false)
                                } else {
                                    verified_transport(verification, || transport(config.clone())).await?
                                };
                                let transport_timing = raw.connect_timing;
                                phase = "relay_pair";
                                let paired = Instant::now();
                                let raw = join_data(raw, &link_id, ticket).await?;
                                let pairing_ms = paired.elapsed().as_secs_f64() * 1000.0;
                                phase = "endpoint_tls";
                                let authenticated = Instant::now();
                                let connection = authenticate(raw, &enrollment.identity, &grant.source.certificate, Role::Executor).await?;
                                record(&log, "cloud.endpoint_authenticated", serde_json::json!({"hub":hub,"link_id":link_id,"grant_id":grant.id,"source_id":source_id,"worker_pid":connection.worker_pid,"enrollment_ms":enrollment_ms,"verification_source":verification_source,"verification_ms":verification_ms,"transport_ms":transport_ms,"transport_timing":transport_timing,"transport_reopened":transport_reopened,"pairing_ms":pairing_ms,"tls_ms":authenticated.elapsed().as_secs_f64()*1000.0,"total_ms":started.elapsed().as_secs_f64()*1000.0}));
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
    pub fn new(api: Client, target: Device) -> crate::Result<Self> {
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

#[cfg(test)]
mod presence_tests {
    use super::*;
    use agit_tunnel::Packet;
    use futures_util::poll;
    use tokio::sync::oneshot;

    /// An enrollment replacement ends the lifetimes held by old Cloud connections and workers.
    #[tokio::test]
    async fn replacing_an_enrollment_retires_cloud_authority_without_interrupting_a_rename() {
        let device = Device {
            id: "device".into(),
            owner: agit_peer::access::Principal {
                issuer: "https://cloud.example".into(),
                account_id: "owner".into(),
            },
            machine_id: "machine".into(),
            display_name: "original".into(),
            certificate: agit_peer::Identity::generate()
                .unwrap()
                .certificate()
                .clone(),
            credential_epoch: 1,
        };
        let (lifetime, mut stopped) = watch::channel(());
        let presence = tokio::spawn(async move {
            let _lifetime = lifetime;
            std::future::pending::<()>().await;
        });
        let (publication, mut publication_stopped) = watch::channel(());
        let publication = tokio::spawn(async move {
            let _lifetime = publication;
            std::future::pending::<()>().await;
        });
        let hub = device.owner.issuer.clone();
        let mut tasks = HashMap::from([(
            hub.clone(),
            Task {
                device: Some(device.clone()),
                presence,
                enrollment: None,
                publication,
                changed: watch::channel(()).0,
            },
        )]);
        let mut renamed = device.clone();
        renamed.display_name = "renamed".into();
        retire_changed_enrollments(&mut tasks, &[(hub.clone(), false, Some(renamed.clone()))]);
        assert_eq!(tasks.len(), 1);
        assert!(stopped.has_changed().is_ok());
        renamed.owner.account_id = "new-owner".into();
        retire_changed_enrollments(&mut tasks, &[(hub, false, Some(renamed))]);
        assert!(tasks.is_empty());
        assert!(
            tokio::time::timeout(Duration::from_secs(1), stopped.changed())
                .await
                .unwrap()
                .is_err()
        );
        assert!(
            tokio::time::timeout(Duration::from_secs(1), publication_stopped.changed())
                .await
                .unwrap()
                .is_err()
        );
    }

    /// Finishing an offer worker cannot leave a consumed heartbeat's reply unpolled.
    #[tokio::test]
    async fn offer_completion_keeps_the_inflight_heartbeat_alive() {
        let (packets, incoming) = mpsc::unbounded_channel();
        let source = futures_util::stream::unfold(incoming, |mut incoming| async move {
            incoming.recv().await.map(|packet| (Ok(packet), incoming))
        });
        let (pong_started, started) = oneshot::channel();
        let (release_pong, pong_released) = oneshot::channel();
        let (written, mut writes) = mpsc::unbounded_channel();
        let sink = futures_util::sink::unfold(
            (Some(pong_started), pong_released, written),
            |(mut started, mut released, written), packet| async move {
                if matches!(packet, Packet::Pong(_)) {
                    started.take().unwrap().send(()).unwrap();
                    (&mut released).await.unwrap();
                }
                written.send(packet).unwrap();
                Ok::<_, anyhow::Error>((started, released, written))
            },
        );
        packets
            .send(Packet::Text(
                serde_json::to_string(&PresenceEvent::Ready {
                    epoch: "presence".into(),
                })
                .unwrap(),
            ))
            .unwrap();
        let mut presence = Presence::open(agit_tunnel::Connection::from_parts(
            0,
            Box::pin(sink),
            Box::pin(source),
        ))
        .await
        .unwrap();
        let (finish_offer, offer_finished) = oneshot::channel();
        let (completed, completion) = oneshot::channel();
        let mut children = tokio::task::JoinSet::new();
        children.spawn(async move {
            offer_finished.await.unwrap();
            completed.send(()).unwrap();
        });
        packets.send(Packet::Ping(vec![1])).unwrap();
        let mut receiving = Box::pin(receive_presence(&mut presence, &mut children));
        assert!(poll!(&mut receiving).is_pending());
        started.await.unwrap();
        finish_offer.send(()).unwrap();
        completion.await.unwrap();
        tokio::task::yield_now().await;
        assert!(poll!(&mut receiving).is_pending());
        release_pong.send(()).unwrap();
        assert!(poll!(&mut receiving).is_pending());
        assert_eq!(writes.try_recv().unwrap(), Packet::Pong(vec![1]));
        packets
            .send(Packet::Text(
                serde_json::to_string(&PresenceEvent::Offer {
                    link_id: "link".into(),
                    source_id: "source".into(),
                    ticket: Secret::new("ticket".into()),
                    grant_token: Secret::new("grant".into()),
                    grant: None,
                })
                .unwrap(),
            ))
            .unwrap();
        assert!(matches!(
            receiving.await.unwrap(),
            PresenceEvent::Offer { .. }
        ));
        assert!(children.is_empty());
    }
}
