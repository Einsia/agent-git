//! Execution admission is transport independent and cannot arrive over the wire.

use crate::protocol::{ErrorCode, RpcError};
use std::sync::Arc;

pub(crate) trait Authority: Send + Sync {
    /// Hold the authority's read lease while invoking the nonblocking acceptance step.
    fn admit(&self, accept: &mut dyn FnMut() -> bool) -> bool;
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
    pub fn new(authority: impl Authority + 'static) -> Self {
        Self(Some(Arc::new(authority)))
    }

    pub fn admit(&self, mut accept: impl FnMut() -> bool) -> bool {
        match &self.0 {
            Some(authority) => authority.admit(&mut accept),
            None => accept(),
        }
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
}
