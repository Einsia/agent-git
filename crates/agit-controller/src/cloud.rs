//! Cloud endpoints terminate peer TLS; transport workers only carry packets.

use crate::{Authority, Connector, Opening, Worker};
use agit_peer::{
    Identity,
    client::{Client, join_controller, verified_transport},
    cloud::{
        Device, DeviceCredential, DialedConnection, ProjectController, Secret, SessionController,
    },
};
use anyhow::ensure;
use std::{sync::Arc, time::Instant};

pub struct Credentials {
    pub identity: Arc<Identity>,
    pub device: DeviceCredential,
    pub account: Secret,
}

pub struct Route {
    pub api: Client,
    pub credentials: Arc<Credentials>,
    pub target: Device,
    key: String,
}

impl Route {
    pub fn new(api: Client, credentials: Arc<Credentials>, target: Device) -> anyhow::Result<Self> {
        ensure!(
            target.owner.issuer == api.origin(),
            "cloud target issuer mismatch"
        );
        let key = serde_json::to_string(&target)?;
        Ok(Self {
            api,
            credentials,
            target,
            key,
        })
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
            dial(
                &self.api,
                worker,
                &self.credentials.account,
                &self.credentials.device,
                &self.credentials.identity,
                &self.target,
            )
            .await
        })
    }
}

pub async fn dial(
    api: &Client,
    worker: &Worker,
    account: &Secret,
    source: &DeviceCredential,
    identity: &Identity,
    target: &Device,
) -> anyhow::Result<agit_tunnel::Connection> {
    dial_admitted(
        api,
        worker,
        source,
        identity,
        target,
        api.connect(account, &source.device, target),
    )
    .await
}

/// A service route retains session authority independently of browser attachment lifetimes.
pub struct SessionRoute {
    api: Client,
    identity: Arc<Identity>,
    source: DeviceCredential,
    target: Device,
    scope: SessionController,
    key: String,
}

impl SessionRoute {
    pub fn new(
        api: Client,
        identity: Arc<Identity>,
        source: DeviceCredential,
        target: Device,
        scope: SessionController,
    ) -> anyhow::Result<Self> {
        ensure!(
            source.device.owner.issuer == api.origin()
                && target.owner == source.device.owner
                && source.device.certificate == *identity.certificate(),
            "cloud session controller identity mismatch"
        );
        let key = serde_json::to_string(&(&source.device, &target, &scope))?;
        Ok(Self {
            api,
            identity,
            source,
            target,
            scope,
            key,
        })
    }
}

impl Connector for SessionRoute {
    fn key(&self) -> &str {
        &self.key
    }

    fn authority(&self) -> Authority {
        Authority::CloudSessionController
    }

    fn open<'a>(&'a self, worker: &'a Worker) -> Opening<'a> {
        Box::pin(dial_admitted(
            &self.api,
            worker,
            &self.source,
            &self.identity,
            &self.target,
            self.api
                .connect_session(&self.source, &self.target, &self.scope),
        ))
    }
}

/// A service route retains project discovery authority independently of browser attachment lifetimes.
pub struct ProjectRoute {
    api: Client,
    identity: Arc<Identity>,
    source: DeviceCredential,
    target: Device,
    scope: ProjectController,
    key: String,
}

impl ProjectRoute {
    pub fn new(
        api: Client,
        identity: Arc<Identity>,
        source: DeviceCredential,
        target: Device,
        scope: ProjectController,
    ) -> anyhow::Result<Self> {
        ensure!(
            source.device.owner.issuer == api.origin()
                && target.owner == source.device.owner
                && source.device.certificate == *identity.certificate(),
            "cloud project controller identity mismatch"
        );
        let key = serde_json::to_string(&(&source.device, &target, &scope))?;
        Ok(Self {
            api,
            identity,
            source,
            target,
            scope,
            key,
        })
    }
}

impl Connector for ProjectRoute {
    fn key(&self) -> &str {
        &self.key
    }

    fn authority(&self) -> Authority {
        Authority::CloudProjectController
    }

    fn open<'a>(&'a self, worker: &'a Worker) -> Opening<'a> {
        Box::pin(dial_admitted(
            &self.api,
            worker,
            &self.source,
            &self.identity,
            &self.target,
            self.api
                .connect_project(&self.source, &self.target, &self.scope),
        ))
    }
}

async fn dial_admitted(
    api: &Client,
    worker: &Worker,
    source: &DeviceCredential,
    identity: &Identity,
    target: &Device,
    admission: impl std::future::Future<Output = anyhow::Result<DialedConnection>>,
) -> anyhow::Result<agit_tunnel::Connection> {
    let config = api.data_config(source)?;
    let started = Instant::now();
    let mut phase = "admission_transport";
    let mut phases = serde_json::Map::new();
    let mut link_id = None;
    let result = async {
        let ((dialed, admission_ms), (raw, transport_ms), reopened) = verified_transport(
            async {
                let started = Instant::now();
                let dialed = admission.await?;
                Ok((dialed, started.elapsed().as_secs_f64() * 1000.0))
            },
            || async {
                let started = Instant::now();
                let raw = worker.open(config.clone()).await?;
                Ok((raw, started.elapsed().as_secs_f64() * 1000.0))
            },
        )
        .await?;
        link_id = Some(dialed.link_id.clone());
        phases.insert("admission_ms".into(), admission_ms.into());
        phases.insert("transport_ms".into(), transport_ms.into());
        phases.insert("transport_reopened".into(), reopened.into());
        phase = "relay_pairing_peer_tls";
        let pairing_tls = Instant::now();
        let authenticated = join_controller(
            raw,
            &dialed.link_id,
            dialed.ticket,
            identity,
            &dialed.connection.grant.target.certificate,
        )
        .await;
        let elapsed_ms = pairing_tls.elapsed().as_secs_f64() * 1000.0;
        phases.insert("pairing_tls_ms".into(), elapsed_ms.into());
        let (connection, pairing) = authenticated?;
        let pairing_ms = pairing.as_secs_f64() * 1000.0;
        phases.insert("pairing_ms".into(), pairing_ms.into());
        phases.insert(
            "peer_tls_after_ready_ms".into(),
            (elapsed_ms - pairing_ms).max(0.0).into(),
        );
        Ok(connection)
    }
    .await;
    crate::diagnostics::record(serde_json::json!({
        "event": "controller.cloud_connect",
        "source_id": source.device.id,
        "target_id": target.id,
        "link_id": link_id,
        "succeeded": result.is_ok(),
        "phase": phase,
        "elapsed_ms": started.elapsed().as_secs_f64() * 1000.0,
        "phases": phases,
    }));
    result
}
