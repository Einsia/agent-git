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

    /// Whether opening this transport started a Codex process: always for a private server; for
    /// a shared one, only when no server was already listening.
    pub fn started_process(&self) -> bool {
        match self {
            Self::Private(_) => true,
            Self::Shared(client) => client.started_server(),
        }
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Pins what `launched` on a resume reply rests on for Codex: a shared transport that
    /// connected to a server already listening started nothing, while one whose connection
    /// started the server, and every private server, did. A transport that answered `true` for
    /// every shared connection told a page it had started a conversation that was already running.
    #[tokio::test]
    async fn a_codex_transport_says_whether_opening_it_started_a_process() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let mut sockets = vec![];
            for _ in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                sockets.push(tokio_tungstenite::accept_async(stream).await.unwrap());
            }
            sockets
        });
        let source = crate::rc::native_codex::Source::new(
            dir.path(),
            &std::env::current_exe().unwrap(),
            Some(&path),
        )
        .unwrap();

        let mut joined = Transport::Shared(Box::new(source.connect_or_start().await.unwrap()));
        assert!(!joined.started_process());

        let starter = tokio::process::Command::new("true").spawn().unwrap();
        let mut started = Transport::Shared(Box::new(
            source.connect().await.unwrap().with_starter(starter),
        ));
        assert!(started.started_process());

        let mut private =
            Transport::from(Proc::spawn("cat", &[], &PathBuf::from("/"), &[]).unwrap());
        assert!(private.started_process());

        joined.shutdown().await.unwrap();
        started.shutdown().await.unwrap();
        private.shutdown().await.unwrap();
        drop(server.await.unwrap());
    }
}
