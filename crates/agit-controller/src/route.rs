//! Routes establish channels; the controller owns retries and request outcomes.

use super::{Connection, Worker};
use std::{future::Future, pin::Pin};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Authority {
    LocalOwner,
    CloudPrincipal,
    #[cfg(feature = "cloud")]
    CloudSessionController,
    /// A receipt-only session grant, with target identity verified before opening completes.
    #[cfg(feature = "cloud")]
    CloudSessionDelivery,
    #[cfg(feature = "cloud")]
    CloudProjectController,
}

impl Authority {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::LocalOwner => "local-owner",
            Self::CloudPrincipal => "cloud-principal",
            #[cfg(feature = "cloud")]
            Self::CloudSessionController | Self::CloudSessionDelivery => {
                agit_peer::cloud::SESSION_CONTROLLER_AUTHORITY
            }
            #[cfg(feature = "cloud")]
            Self::CloudProjectController => agit_peer::cloud::PROJECT_CONTROLLER_AUTHORITY,
        }
    }

    pub(crate) fn describes_machine(self) -> bool {
        match self {
            #[cfg(feature = "cloud")]
            Self::CloudSessionDelivery => false,
            _ => true,
        }
    }
}

pub type Opening<'a> = Pin<Box<dyn Future<Output = anyhow::Result<Connection>> + Send + 'a>>;

pub trait Connector: Send + Sync {
    /// A stable key names configuration without retaining one-use tickets.
    fn key(&self) -> &str;
    fn authority(&self) -> Authority;
    /// Utility routes can opt out of session events while retaining RPC and terminal traffic.
    fn session_events(&self) -> bool {
        true
    }
    fn open<'a>(&'a self, worker: &'a Worker) -> Opening<'a>;
}

pub(super) struct Static {
    pub key: String,
    pub config: agit_tunnel::Config,
}

impl Connector for Static {
    fn key(&self) -> &str {
        &self.key
    }
    fn authority(&self) -> Authority {
        Authority::LocalOwner
    }
    fn open<'a>(&'a self, worker: &'a Worker) -> Opening<'a> {
        Box::pin(worker.open(self.config.clone()))
    }
}
