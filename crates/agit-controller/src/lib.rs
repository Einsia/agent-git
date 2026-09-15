//! Peer connection ownership and RPC routing, independent of local execution.
//!
//! Hosts authenticate callers before using this API. A tunnel transports packets;
//! it neither grants authority nor proves that a remote operation executed.

mod peer;
mod route;
pub use route::{Authority, Connector, Opening};

use agit_tunnel::{Config, Connection};
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

const MAX_PEERS: usize = 64;
const MAX_PENDING: usize = 64;
const EVENT_CAPACITY: usize = 512;
const QUEUE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone)]
pub struct Worker {
    pub executable: PathBuf,
    pub args: Vec<String>,
}
impl Worker {
    pub async fn open(&self, config: Config) -> anyhow::Result<Connection> {
        #[cfg(test)]
        if self.executable.as_os_str().is_empty() {
            return Connection::in_process(config).await;
        }
        let args = self.args.iter().map(String::as_str).collect::<Vec<_>>();
        Connection::open(config, &self.executable, &args).await
    }
}

#[derive(Clone, Serialize, Debug)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Connecting,
    Online,
    Backoff,
    Rejected,
    Stopped,
}

#[derive(Clone, Serialize, Debug)]
pub struct Status {
    pub peer_id: String,
    pub route_id: String,
    pub generation: u64,
    pub state: State,
    pub description: Option<Value>,
    pub worker_pid: Option<u32>,
    pub error: Option<String>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Target {
    pub peer_id: String,
    pub route_id: String,
    pub generation: u64,
}
impl Status {
    pub fn target(&self) -> Target {
        Target {
            peer_id: self.peer_id.clone(),
            route_id: self.route_id.clone(),
            generation: self.generation,
        }
    }
}

#[derive(Clone, Serialize, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    State {
        status: Status,
    },
    Frame {
        peer_id: String,
        route_id: String,
        generation: u64,
        frame: Arc<Value>,
        #[serde(skip)]
        budget: Arc<tokio::sync::OwnedSemaphorePermit>,
    },
}
impl Event {
    pub fn peer_id(&self) -> &str {
        match self {
            Self::State { status } => &status.peer_id,
            Self::Frame { peer_id, .. } => peer_id,
        }
    }
}

/// A failure certifies whether the request could have reached the executor.
#[derive(Debug, Clone, Serialize)]
pub struct Failure {
    pub operation_id: String,
    pub outcome: &'static str,
    pub message: String,
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: {} (operation {})",
            self.outcome, self.message, self.operation_id
        )
    }
}
impl std::error::Error for Failure {}

struct Request {
    id: String,
    generation: u64,
    packet: Option<agit_tunnel::Packet>,
    _budget: Arc<tokio::sync::OwnedSemaphorePermit>,
    delivery: Arc<std::sync::atomic::AtomicU8>,
    deadline: tokio::time::Instant,
    reply: oneshot::Sender<Result<Value, Failure>>,
}
impl Request {
    fn fail(self, outcome: &'static str, message: impl Into<String>) {
        let _ = self.reply.send(Err(Failure {
            operation_id: self.id,
            outcome,
            message: message.into(),
        }));
    }
}

struct Peer {
    route_key: String,
    authority: Authority,
    budget: Arc<tokio::sync::Semaphore>,
    requests: mpsc::Sender<Request>,
    status: watch::Receiver<Status>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub struct Controller {
    worker: Worker,
    peers: Mutex<HashMap<String, Arc<Peer>>>,
    events: broadcast::Sender<Event>,
}
impl Drop for Controller {
    fn drop(&mut self) {
        for peer in self
            .peers
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
        {
            peer.task.abort();
        }
    }
}
impl Controller {
    pub fn new(worker: Worker) -> Self {
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        Self {
            worker,
            peers: Mutex::new(HashMap::new()),
            events,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Reattaching a UI reuses the daemon's existing connection and fingerprint.
    pub fn connect(
        &self,
        id: String,
        config: Config,
        fingerprint: Option<String>,
    ) -> anyhow::Result<()> {
        config.validate()?;
        let route = Arc::new(route::Static {
            key: serde_json::to_string(&config)?,
            config,
        });
        self.connect_with(id, route, fingerprint)
    }

    /// Each connection attempt obtains fresh admission material from its route.
    pub fn connect_with(
        &self,
        id: String,
        route: Arc<dyn Connector>,
        fingerprint: Option<String>,
    ) -> anyhow::Result<()> {
        ensure!(!id.is_empty() && id.len() <= 256, "invalid peer identity");
        let route_key = route.key().to_owned();
        let authority = route.authority();
        ensure!(
            !route_key.is_empty() && route_key.len() <= 131072,
            "invalid peer route identity"
        );
        let mut peers = self
            .peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(peer) = peers.get(&id) {
            ensure!(
                peer.route_key == route_key && peer.authority == authority,
                "peer configuration changed; disconnect it before replacing its configuration"
            );
            if let Some(expected) = fingerprint.as_deref()
                && let Some(description) = &peer.status.borrow().description
            {
                ensure!(
                    description["machine"]["machine_fingerprint"].as_str() == Some(expected),
                    "peer fingerprint changed"
                );
            }
            return Ok(());
        }
        ensure!(peers.len() < MAX_PEERS, "peer capacity exhausted");
        let status = Status {
            peer_id: id.clone(),
            route_id: uuid::Uuid::new_v4().to_string(),
            generation: 0,
            state: State::Connecting,
            description: None,
            worker_pid: None,
            error: None,
        };
        let (status_tx, status_rx) = watch::channel(status);
        let (requests, receiver) = mpsc::channel(MAX_PENDING);
        let worker = self.worker.clone();
        let events = self.events.clone();
        let task = tokio::spawn(peer::run(
            worker,
            route,
            fingerprint,
            receiver,
            status_tx,
            events,
        ));
        peers.insert(
            id,
            Arc::new(Peer {
                route_key,
                authority,
                budget: Arc::new(tokio::sync::Semaphore::new(QUEUE_BYTES)),
                requests,
                status: status_rx,
                task,
            }),
        );
        Ok(())
    }

    fn peer(&self, id: &str) -> anyhow::Result<Arc<Peer>> {
        self.peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned()
            .context("unknown peer")
    }

    pub fn list(&self) -> Vec<Status> {
        self.peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .map(|p| p.status.borrow().clone())
            .collect()
    }

    pub fn disconnect(&self, id: &str) {
        if let Some(peer) = self
            .peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id)
        {
            peer.task.abort();
            let mut status = peer.status.borrow().clone();
            status.state = State::Stopped;
            status.worker_pid = None;
            let _ = self.events.send(Event::State { status });
        }
    }

    pub async fn ready(&self, id: &str, timeout: Duration) -> anyhow::Result<Status> {
        let peer = self.peer(id)?;
        let mut status = peer.status.clone();
        tokio::time::timeout(timeout, async {
            loop {
                let current = status.borrow_and_update().clone();
                match current.state {
                    State::Online => return Ok(current),
                    State::Rejected | State::Stopped => {
                        anyhow::bail!("{}", current.error.as_deref().unwrap_or("peer stopped"))
                    }
                    _ => {}
                }
                status.changed().await.context("peer supervisor stopped")?;
            }
        })
        .await
        .context("peer connection timed out")?
    }

    /// Resolve the current route for an in-process host without a cached attachment.
    pub async fn request(
        &self,
        peer_id: &str,
        method: String,
        params: Value,
        budget: Duration,
    ) -> Result<Value, Failure> {
        let peer = self.peer(peer_id).map_err(|error| Failure {
            operation_id: uuid::Uuid::new_v4().to_string(),
            outcome: "not_sent",
            message: error.to_string(),
        })?;
        let target = peer.status.borrow().target();
        self.request_at(target, method, params, budget).await
    }

    /// Cached clients must fence writes to the route and generation they observed.
    pub async fn request_at(
        &self,
        target: Target,
        method: String,
        params: Value,
        budget: Duration,
    ) -> Result<Value, Failure> {
        let operation_id = uuid::Uuid::new_v4().to_string();
        let deadline = tokio::time::Instant::now()
            + budget.clamp(Duration::from_millis(1), Duration::from_secs(120));
        let mut result = self
            .request_once(&target, &method, &params, deadline, &operation_id)
            .await;
        let connection_changed = self.peer(&target.peer_id).ok().is_some_and(|peer| {
            let status = peer.status.borrow();
            status.route_id == target.route_id
                && (!matches!(status.state, State::Online) || status.generation > target.generation)
        });
        let retryable = result
            .as_ref()
            .err()
            .is_some_and(|failure| failure.outcome == "unknown" || connection_changed);
        if retryable
            && is_read(&method)
            && tokio::time::Instant::now() < deadline
            && let Ok(peer) = self.peer(&target.peer_id)
        {
            let mut status = peer.status.clone();
            let reconnected = tokio::time::timeout_at(deadline, async {
                loop {
                    let current = status.borrow_and_update().clone();
                    if current.route_id != target.route_id
                        || matches!(current.state, State::Rejected | State::Stopped)
                    {
                        return None;
                    }
                    if matches!(current.state, State::Online)
                        && current.generation > target.generation
                    {
                        return Some(current.target());
                    }
                    if status.changed().await.is_err() {
                        return None;
                    }
                }
            })
            .await
            .ok()
            .flatten();
            if let Some(target) = reconnected {
                result = self
                    .request_once(&target, &method, &params, deadline, &operation_id)
                    .await;
            }
        }
        result.map_err(|mut failure| {
            failure.operation_id = operation_id;
            failure
        })
    }

    async fn request_once(
        &self,
        target: &Target,
        method: &str,
        params: &Value,
        deadline: tokio::time::Instant,
        operation_id: &str,
    ) -> Result<Value, Failure> {
        let id = format!("{operation_id}:{}", uuid::Uuid::new_v4());
        let failure = |outcome, message: String| Failure {
            operation_id: id.clone(),
            outcome,
            message,
        };
        if method.is_empty() || method.starts_with("peer.") || method.len() > 128 {
            return Err(failure("not_sent", "invalid executor method".into()));
        }
        let peer = self
            .peer(&target.peer_id)
            .map_err(|e| failure("not_sent", e.to_string()))?;
        if peer.status.borrow().route_id != target.route_id {
            return Err(failure(
                "not_sent",
                "peer route changed; reattach before sending".into(),
            ));
        }
        let packet = serde_json::json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
            .to_string();
        if packet.len() > agit_tunnel::protocol::MAX_PAYLOAD {
            return Err(failure(
                "not_sent",
                "RPC request exceeds the frame limit".into(),
            ));
        }
        let permit = peer
            .budget
            .clone()
            .try_acquire_many_owned(packet.len().max(1) as u32)
            .map_err(|_| failure("not_sent", "peer request byte budget exhausted".into()))?;
        let (reply, receipt) = oneshot::channel();
        let request = Request {
            id: id.clone(),
            generation: target.generation,
            packet: Some(agit_tunnel::Packet::Text(packet)),
            _budget: Arc::new(permit),
            delivery: Arc::new(std::sync::atomic::AtomicU8::new(0)),
            deadline,
            reply,
        };
        peer.requests
            .try_send(request)
            .map_err(|_| failure("not_sent", "peer request queue is full or closed".into()))?;
        // The actor owns delivery classification. Its deadline includes waiting for
        // a writer, and a cancelled caller does not release an admitted mutation.
        receipt.await.map_err(|_| {
            failure(
                "unknown",
                "peer supervisor stopped; inspect the operation before retrying".into(),
            )
        })?
    }
}

fn is_read(method: &str) -> bool {
    matches!(
        method,
        "machine.describe"
            | "workspace.list"
            | "session.list"
            | "session.history"
            | "session.goal.read"
            | "session.commands"
            | "runtime.models"
            | "fs.readDirectory"
            | "fs.readFile"
    )
}

#[cfg(test)]
mod tests;
