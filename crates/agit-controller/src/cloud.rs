//! Cloud endpoints terminate peer TLS; transport workers only carry packets.

use crate::{Authority, Connector, Opening, Worker};
use agit_peer::{
    Identity,
    client::{Client, join_data},
    cloud::{Device, DeviceCredential, Secret},
    transport::{Role, authenticate},
};
use anyhow::{Context, ensure};
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
            let mut after = None;
            let mut target = None;
            for _ in 0..64 {
                let page = self
                    .api
                    .devices(&self.credentials.account, after.as_deref())
                    .await?;
                target = page
                    .devices
                    .into_iter()
                    .map(|row| row.device)
                    .find(|device| device.id == self.target.id);
                if target.is_some() || page.next_cursor.is_none() {
                    break;
                }
                after = page.next_cursor;
            }
            let target = target.context("cloud target is no longer available")?;
            ensure!(
                target.owner == self.target.owner
                    && target.machine_id == self.target.machine_id
                    && target.certificate == self.target.certificate,
                "cloud target identity changed; approve the new identity before reconnecting"
            );
            dial(
                &self.api,
                worker,
                &self.credentials.account,
                &self.credentials.device,
                &self.credentials.identity,
                &target,
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
    let dialed = api.connect(account, &source.device, target).await?;
    let raw = worker.open(api.data_config(source)?).await?;
    let raw = join_data(raw, &dialed.link_id, dialed.ticket).await?;
    authenticate(
        raw,
        identity,
        &dialed.connection.grant.target.certificate,
        Role::Controller,
    )
    .await
}
