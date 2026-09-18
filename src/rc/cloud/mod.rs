//! Cloud admission supervises tunnels without owning harness execution.

pub mod access;
mod commands;
mod delegation;
pub mod host;
pub mod ingress;
pub mod resources;
pub mod store;
pub use commands::{Args, run};

/// Only transports are shared; callers load current credentials for every request.
#[derive(Default)]
pub struct Clients(
    std::sync::Mutex<std::collections::VecDeque<(String, agit_peer::client::Client)>>,
);

impl Clients {
    pub fn get(&self, hub: &str) -> crate::Result<agit_peer::client::Client> {
        let mut clients = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((_, client)) = clients.iter().find(|(origin, _)| origin == hub) {
            return Ok(client.clone());
        }
        let client = agit_peer::client::Client::new(hub)?;
        if clients.len() >= 16 {
            clients.pop_front();
        }
        clients.push_back((hub.into(), client.clone()));
        Ok(client)
    }
}

#[derive(serde::Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum OwnerRequest {
    Status { hub: String },
    Devices { hub: String, after: Option<String> },
    Enroll { hub: String, name: Option<String> },
    Inbound { hub: String, enabled: bool },
}

pub async fn manage(clients: &Clients, request: OwnerRequest) -> crate::Result<serde_json::Value> {
    match request {
        OwnerRequest::Status { hub } => {
            tokio::task::spawn_blocking(move || store::status(&hub)).await?
        }
        OwnerRequest::Devices { hub, after } => {
            let api = clients.get(&hub)?;
            let token = account_token(api.origin(), false).await?;
            let page = match api.devices(&token, after.as_deref()).await {
                Err(error)
                    if error
                        .downcast_ref::<agit_peer::client::HttpFailure>()
                        .is_some_and(|error| error.status == 401) =>
                {
                    let token = account_token(api.origin(), true).await?;
                    api.devices(&token, after.as_deref()).await?
                }
                result => result?,
            };
            Ok(serde_json::to_value(page)?)
        }
        OwnerRequest::Enroll { hub, name } => commands::enroll(&hub, name).await,
        OwnerRequest::Inbound { hub, enabled } => commands::inbound(&hub, enabled).await,
    }
}

pub async fn account_token(hub: &str, refresh: bool) -> crate::Result<agit_peer::cloud::Secret> {
    let hub = hub.to_owned();
    tokio::task::spawn_blocking(move || {
        let credential = crate::infra::credentials::load_checked(&hub)?.ok_or_else(|| {
            anyhow::anyhow!("sign in to this cloud before enrolling or connecting a peer")
        })?;
        let client = crate::hub::Client::for_credential(&hub, &credential);
        if refresh || client.access_expired() {
            anyhow::ensure!(
                client.refresh_access()?,
                "sign in to this cloud before connecting a peer"
            );
        }
        client
            .checked_access_token()?
            .map(agit_peer::cloud::Secret::new)
            .ok_or_else(|| {
                anyhow::anyhow!("sign in to this cloud before enrolling or connecting a peer")
            })
    })
    .await?
}
