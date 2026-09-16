//! Cloud endpoints terminate peer TLS; transport workers only carry packets.

use crate::{Authority, Connector, Opening, Worker};
use agit_peer::{
    Identity,
    client::{Client, join_data, verified_transport},
    cloud::{Device, DeviceCredential, Secret},
    transport::{Role, authenticate},
};
use anyhow::ensure;
use std::sync::Arc;

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
    let config = api.data_config(source)?;
    let (dialed, raw, _) = verified_transport(api.connect(account, &source.device, target), || {
        worker.open(config.clone())
    })
    .await?;
    let raw = join_data(raw, &dialed.link_id, dialed.ticket).await?;
    authenticate(
        raw,
        identity,
        &dialed.connection.grant.target.certificate,
        Role::Controller,
    )
    .await
}
