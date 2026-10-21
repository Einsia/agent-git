//! Execution admission is transport independent and cannot arrive over the wire.

use crate::protocol::{ErrorCode, RpcError};
use std::sync::Arc;

pub(crate) trait Authority: Send + Sync {
    /// Hold the authority's read lease while invoking the nonblocking acceptance step.
    fn admit(&self, accept: &mut dyn FnMut() -> bool) -> bool;

    fn publication_grant(&self) -> Option<agit_peer::cloud::ConnectionGrant> {
        None
    }

    fn owned_machine_grant(&self) -> Option<agit_peer::cloud::ConnectionGrant> {
        None
    }

    fn watch_owner(&self) -> Option<String> {
        None
    }

    fn project(&self) -> Option<(&str, &std::path::Path)> {
        None
    }
}

#[derive(Clone, Default)]
pub(crate) struct Guard {
    authority: Option<Arc<dyn Authority>>,
    /// Closes when the requester can no longer receive a response. Nothing is ever sent on it.
    requester: Option<tokio::sync::watch::Receiver<()>>,
}

impl std::fmt::Debug for Guard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Guard")
            .field("enforced", &self.authority.is_some())
            .field("requester", &self.requester.is_some())
            .finish()
    }
}

impl Guard {
    pub fn owned_machine_grant(&self) -> Result<agit_peer::cloud::ConnectionGrant, RpcError> {
        let mut grant = None;
        if self.admit(|| {
            grant = self
                .authority
                .as_ref()
                .and_then(|authority| authority.owned_machine_grant());
            grant.is_some()
        }) {
            Ok(grant.expect("admitted owned-machine grant"))
        } else {
            Err(RpcError::new(
                ErrorCode::Forbidden,
                "project publication requires current device-owner authority",
            ))
        }
    }

    pub fn publication_grant(&self) -> Result<agit_peer::cloud::ConnectionGrant, RpcError> {
        let mut grant = None;
        if self.admit(|| {
            grant = self
                .authority
                .as_ref()
                .and_then(|authority| authority.publication_grant());
            grant.is_some()
        }) {
            Ok(grant.expect("admitted publication grant"))
        } else {
            Err(RpcError::new(
                ErrorCode::Forbidden,
                "publication delivery requires current session-controller authority",
            ))
        }
    }

    pub fn new(authority: impl Authority + 'static) -> Self {
        Self {
            authority: Some(Arc::new(authority)),
            requester: None,
        }
    }

    /// Ties the request to its requester's connection, whose sender closes when it leaves.
    pub fn requested_by(&mut self, requester: tokio::sync::watch::Receiver<()>) {
        self.requester = Some(requester);
    }

    /// True once the requester has left. Only reads may stop on it: a mutation may already have
    /// reached the harness, and its receipt must still be recorded.
    pub fn departed(&self) -> bool {
        self.requester
            .as_ref()
            .is_some_and(|requester| requester.has_changed().is_err())
    }

    /// Resolves once the requester has left; never for a request without a tracked requester.
    pub async fn until_departed(&self) {
        let Some(mut requester) = self.requester.clone() else {
            return std::future::pending().await;
        };
        while requester.changed().await.is_ok() {}
    }

    pub fn admit(&self, mut accept: impl FnMut() -> bool) -> bool {
        match &self.authority {
            Some(authority) => authority.admit(&mut accept),
            None => accept(),
        }
    }

    pub fn watch_owner(&self) -> Option<String> {
        self.authority
            .as_ref()
            .and_then(|authority| authority.watch_owner())
    }

    pub fn check(&self) -> Result<(), RpcError> {
        if self.admit(|| true) {
            Ok(())
        } else {
            Err(RpcError::new(
                ErrorCode::Forbidden,
                "request authority expired before execution admission",
            ))
        }
    }

    pub fn check_project(&self, id: &str, path: &std::path::Path) -> Result<(), RpcError> {
        if self
            .authority
            .as_ref()
            .and_then(|authority| authority.project())
            .is_some_and(|(allowed_id, allowed_path)| allowed_id != id || allowed_path != path)
        {
            return Err(RpcError::new(
                ErrorCode::Forbidden,
                "project binding changed outside delegated authority",
            ));
        }
        Ok(())
    }
}
