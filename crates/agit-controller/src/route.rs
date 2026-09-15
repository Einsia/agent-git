//! Routes establish channels; the controller owns retries and request outcomes.

use super::{Connection, Worker};
use std::{future::Future, pin::Pin};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Authority {
    LocalOwner,
    CloudPrincipal,
}

impl Authority {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::LocalOwner => "local-owner",
            Self::CloudPrincipal => "cloud-principal",
        }
    }
}

pub type Opening<'a> = Pin<Box<dyn Future<Output = anyhow::Result<Connection>> + Send + 'a>>;

pub trait Connector: Send + Sync {
    /// A stable key names configuration without retaining one-use tickets.
    fn key(&self) -> &str;
    fn authority(&self) -> Authority;
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
