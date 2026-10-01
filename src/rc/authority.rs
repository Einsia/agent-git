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
pub(crate) struct Guard(Option<Arc<dyn Authority>>);

impl std::fmt::Debug for Guard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Guard")
            .field("enforced", &self.0.is_some())
            .finish()
    }
}

impl Guard {
    pub fn owned_machine_grant(&self) -> Result<agit_peer::cloud::ConnectionGrant, RpcError> {
        let mut grant = None;
        if self.admit(|| {
            grant = self
                .0
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
                .0
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
        Self(Some(Arc::new(authority)))
    }

    pub fn admit(&self, mut accept: impl FnMut() -> bool) -> bool {
        match &self.0 {
            Some(authority) => authority.admit(&mut accept),
            None => accept(),
        }
    }

    pub fn watch_owner(&self) -> Option<String> {
        self.0
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
            .0
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
