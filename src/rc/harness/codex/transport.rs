//! Client disconnect and private executor shutdown have different lifetimes.

use super::super::proc::{Proc, QueuedLine};
use crate::rc::native_codex::Client;
use serde_json::Value;

pub(super) enum Transport {
    Private(Box<Proc>),
    Shared(Box<Client>),
}

impl From<Proc> for Transport {
    fn from(proc: Proc) -> Self {
        Self::Private(Box::new(proc))
    }
}

impl Transport {
    pub fn shared(&self) -> bool {
        matches!(self, Self::Shared(_))
    }

    pub async fn write_line(&mut self, value: &Value) -> crate::Result<()> {
        match self {
            Self::Private(proc) => proc.write_line(value).await,
            Self::Shared(client) => client.send(value).await,
        }
    }

    pub async fn next(&mut self) -> Option<QueuedLine> {
        match self {
            Self::Private(proc) => proc.next().await,
            Self::Shared(client) => client.next().await,
        }
    }

    pub async fn wait(&mut self) -> Option<i32> {
        match self {
            Self::Private(proc) => proc.wait().await,
            Self::Shared(_) => None,
        }
    }

    pub async fn shutdown(&mut self) -> crate::Result<()> {
        match self {
            Self::Private(proc) => proc.shutdown().await,
            // The shared server is not this client's to stop; closing ends only the subscription.
            Self::Shared(client) => {
                client.close().await;
                Ok(())
            }
        }
    }

    #[cfg(test)]
    pub fn available_pending_bytes(&self) -> usize {
        match self {
            Self::Private(proc) => proc.available_pending_bytes(),
            Self::Shared(_) => panic!("budget inspection requires a private test process"),
        }
    }

    #[cfg(test)]
    pub fn fail_shutdowns(
        &mut self,
        count: usize,
    ) -> std::sync::Arc<std::sync::atomic::AtomicUsize> {
        match self {
            Self::Private(proc) => proc.fail_shutdowns(count),
            Self::Shared(_) => panic!("shutdown injection requires a private test process"),
        }
    }
}
