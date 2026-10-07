use super::{Registration, descriptor_path, directory};
use anyhow::{Context, ensure};
use fs2::FileExt;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{
    Request, Response, StatusCode,
    body::{Bytes, Incoming},
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    convert::Infallible,
    io::{Read, Write},
    os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{net::UnixListener, sync::Notify};

pub(super) const MAX_BODY: usize = 4 * 1024 * 1024;
const MAX_PEERS: usize = 128;
const MAX_OPERATIONS: usize = 4096;
const MAX_EVENTS: usize = 1024;
const FRESH: Duration = Duration::from_secs(3);
static HUB: OnceLock<Mutex<Weak<Hub>>> = OnceLock::new();

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Descriptor {
    pub(super) socket: PathBuf,
    pub(super) token: String,
    pub(super) owner: super::Process,
}

/// Prefer a live daemon and retain the selected namespace across total disconnection.
pub(super) fn discover_descriptor(preferred: Option<&Path>) -> crate::Result<PathBuf> {
    let home = crate::infra::config::agit_home()?;
    let candidates = ["rc", "desktop-rc"].map(|name| home.join(name).join("claude-native.json"));
    Ok(select_descriptor(&candidates, preferred).unwrap_or(descriptor_path()?))
}

fn select_descriptor(candidates: &[PathBuf], preferred: Option<&Path>) -> Option<PathBuf> {
    preferred
        .filter(|path| candidates.iter().any(|candidate| candidate == path))
        .into_iter()
        .chain(candidates.iter().map(PathBuf::as_path))
        .find(|path| {
            (|| -> crate::Result<bool> {
                let file = super::super::native_inbox::open_regular(path)?;
                let descriptor: Descriptor =
                    serde_json::from_reader(file.take(super::RECORD_LIMIT))?;
                Ok(super::process(descriptor.owner.pid)
                    .is_some_and(|(owner, _)| owner == descriptor.owner)
                    && std::fs::symlink_metadata(&descriptor.socket)?
                        .file_type()
                        .is_socket())
            })()
            .unwrap_or(false)
        })
        .map(Path::to_path_buf)
        .or_else(|| {
            preferred
                .filter(|path| candidates.iter().any(|candidate| candidate == path))
                .map(Path::to_path_buf)
        })
}

/// Metadata only: the transcript is read through the canonical native tailer.
#[derive(Debug, Clone, Deserialize)]
pub struct Snapshot {
    pub registration: Registration,
    pub session: String,
    pub cwd: PathBuf,
    pub generation: String,
    pub version: u32,
    pub turn: Option<String>,
    pub permission_mode: Option<String>,
    pub model: Option<String>,
    #[serde(default)]
    pub model_controls: Option<ModelControls>,
    #[serde(default)]
    pub commands: Vec<crate::protocol::SlashCommand>,
    pub saturated: bool,
    pub last_seq: u64,
    #[serde(default)]
    pub latest_seq: Option<u64>,
    pub events: Vec<Value>,
    pub results: Vec<Value>,
    pub approvals: Vec<Value>,
    pub tools: Vec<Value>,
    pub compacting: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ModelControls {
    pub selected: String,
    pub locked: bool,
    pub choices: Vec<String>,
    pub pending: Option<String>,
}

impl Snapshot {
    fn validate(&self) -> crate::Result<()> {
        ensure!(
            self.version == 1
                && self.session == self.registration.session
                && self.cwd.canonicalize().ok().as_ref() == Some(&self.registration.cwd)
                && self.generation == self.registration.generation
                && self.registration.descriptor == descriptor_path()?
                && self.registration.is_registered(),
            "native session registration is no longer current"
        );
        ensure!(
            self.events.len() <= MAX_EVENTS
                && self.results.len() <= MAX_OPERATIONS
                && self.commands.len() <= 128
                && self.latest_seq.is_none_or(|latest| latest >= self.last_seq),
            "native control metadata exceeds its bound"
        );
        Ok(())
    }
}

pub struct Update {
    pub snapshot: Snapshot,
    pub events: Vec<Value>,
}

struct Operation {
    order: u64,
    command: Value,
    digest: String,
    claimed: bool,
    result: Option<Value>,
}

struct PeerState {
    baseline: u64,
    snapshot: Snapshot,
    seen: Instant,
    acknowledged: u64,
    events: VecDeque<Value>,
    attached: bool,
    operations: HashMap<String, Operation>,
    next_operation: u64,
    decisions: HashMap<String, Value>,
}

fn pending_commands(operations: &HashMap<String, Operation>) -> Vec<Value> {
    let mut pending: Vec<_> = operations
        .values()
        .filter(|operation| !operation.claimed && operation.result.is_none())
        .collect();
    // Native settings and subsequent prompts must retain their submission order.
    pending.sort_unstable_by_key(|operation| operation.order);
    pending
        .into_iter()
        .take(32)
        .map(|operation| operation.command.clone())
        .collect()
}

struct Peer {
    state: Mutex<PeerState>,
    changed: Notify,
}
struct Hub {
    peers: Mutex<HashMap<String, Arc<Peer>>>,
    claims: PathBuf,
    closed: AtomicBool,
}

/// The listener owns the descriptor and socket for exactly one daemon lifetime.
pub struct Endpoint {
    hub: Arc<Hub>,
    worker: tokio::task::JoinHandle<()>,
    directory: tempfile::TempDir,
    descriptor_path: PathBuf,
    descriptor: Descriptor,
}

impl Endpoint {
    pub fn start() -> crate::Result<Self> {
        let directory = tempfile::Builder::new()
            .prefix("agit-claude-")
            .tempdir_in("/tmp")?;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
        let descriptor = Descriptor {
            socket: directory.path().join("control.sock"),
            owner: super::process(std::process::id())
                .context("cannot verify the native control listener process")?
                .0,
            token: format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            ),
        };
        let listener = UnixListener::bind(&descriptor.socket)?;
        std::fs::set_permissions(&descriptor.socket, std::fs::Permissions::from_mode(0o600))?;
        let claims = directory_for_claims()?;
        let hub = Arc::new(Hub {
            peers: Mutex::default(),
            claims,
            closed: AtomicBool::new(false),
        });
        let descriptor_path = descriptor_path()?;
        write_private(&descriptor_path, &descriptor)?;
        let worker_hub = hub.clone();
        let token = descriptor.token.clone();
        let worker = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                if !stream
                    .peer_cred()
                    .is_ok_and(|credentials| credentials.uid() == unsafe { libc::geteuid() })
                {
                    continue;
                }
                let hub = worker_hub.clone();
                let authorization = format!("Bearer {token}");
                tokio::spawn(async move {
                    let service = service_fn(move |request| {
                        serve(hub.clone(), authorization.clone(), request)
                    });
                    let _ = tokio::time::timeout(
                        Duration::from_secs(10),
                        hyper::server::conn::http1::Builder::new()
                            .keep_alive(false)
                            .serve_connection(TokioIo::new(stream), service),
                    )
                    .await;
                });
            }
        });
        *HUB.get_or_init(Mutex::default).lock().unwrap() = Arc::downgrade(&hub);
        Ok(Self {
            hub,
            worker,
            directory,
            descriptor_path,
            descriptor,
        })
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.hub.closed.store(true, Ordering::Release);
        self.worker.abort();
        // A replacement listener's descriptor belongs to that listener, even during shutdown.
        if std::fs::read(&self.descriptor_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Descriptor>(&bytes).ok())
            .is_some_and(|descriptor| descriptor.token == self.descriptor.token)
        {
            let _ = std::fs::remove_file(&self.descriptor_path);
        }
        for peer in self.hub.peers.lock().unwrap().values() {
            peer.changed.notify_waiters();
        }
        let _ = self.directory.path();
    }
}

fn directory_for_claims() -> crate::Result<PathBuf> {
    let directory = directory()?.join("claims");
    crate::infra::config::create_state_dir(&directory)?;
    Ok(directory)
}

fn write_private(path: &Path, value: &impl Serialize) -> crate::Result<()> {
    let parent = path
        .parent()
        .context("native control metadata needs a parent directory")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut temporary, value)?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary.persist(path)?;
    super::super::native_inbox::sync_directory(parent)
}

async fn serve(
    hub: Arc<Hub>,
    authorization: String,
    request: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let allowed = request.method() == hyper::Method::POST
        && request
            .headers()
            .get(hyper::header::AUTHORIZATION)
            .is_some_and(|value| value.as_bytes() == authorization.as_bytes());
    let path = request.uri().path().to_owned();
    let result = if allowed {
        match Limited::new(request.into_body(), MAX_BODY).collect().await {
            Ok(body) => {
                let bytes = body.to_bytes();
                tokio::task::spawn_blocking(move || hub.handle(&path, &bytes))
                    .await
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("native control handler stopped")))
            }
            Err(_) => Err(anyhow::anyhow!("native control request exceeds its bound")),
        }
    } else {
        Err(anyhow::anyhow!("native control authentication failed"))
    };
    let (status, body) = match result {
        Ok(body) => (StatusCode::OK, body),
        // These errors are consumed by the module's quiet reconnect path, never a web banner.
        Err(_) => (StatusCode::CONFLICT, json!({"retry":true})),
    };
    Ok(Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(serde_json::to_vec(&body).unwrap())))
        .unwrap())
}

impl Hub {
    fn handle(&self, path: &str, bytes: &[u8]) -> crate::Result<Value> {
        ensure!(
            !self.closed.load(Ordering::Acquire),
            "native control listener has stopped"
        );
        if path == "/poll" {
            return self.poll(serde_json::from_slice(bytes)?);
        }
        let body: Value = serde_json::from_slice(bytes)?;
        let generation = body
            .get("generation")
            .and_then(Value::as_str)
            .context("missing native generation")?;
        let peer = self
            .peers
            .lock()
            .unwrap()
            .get(generation)
            .cloned()
            .context("native control has not registered")?;
        let mut state = peer.state.lock().unwrap();
        state.snapshot.validate()?;
        ensure!(
            body["session"].as_str() == Some(&state.snapshot.session)
                && body["cwd"].as_str() == state.snapshot.cwd.to_str(),
            "native target changed"
        );
        let id = body
            .get("id")
            .and_then(Value::as_str)
            .context("missing operation identity")?;
        match path {
            "/claim" => {
                ensure!(state.attached, "no RC client owns native control");
                let Some(operation) = state.operations.get_mut(id) else {
                    let result = stored_result(&self.claims, generation, id)?;
                    return Ok(json!({"generation":generation, "id":id,
                        "claimed":false, "result":result}));
                };
                let claimed = if operation.claimed {
                    false
                } else {
                    claim(&self.claims, generation, id, &operation.digest)?
                };
                operation.claimed = true;
                Ok(json!({"generation":generation, "id":id, "claimed":claimed,
                    "result":operation.result}))
            }
            "/approval" => {
                let pending = state
                    .snapshot
                    .approvals
                    .iter()
                    .any(|approval| approval["id"].as_str() == Some(id));
                let decision = if pending {
                    state.decisions.get(id).cloned()
                } else {
                    None
                };
                Ok(json!({"generation":generation, "id":id, "decision":decision}))
            }
            _ => anyhow::bail!("unknown native control endpoint"),
        }
    }

    fn poll(&self, mut snapshot: Snapshot) -> crate::Result<Value> {
        snapshot.validate()?;
        let peer = {
            let mut peers = self.peers.lock().unwrap();
            if !peers.contains_key(&snapshot.generation) {
                peers.retain(|_, peer| {
                    let state = peer.state.lock().unwrap();
                    state.attached || state.seen.elapsed() < FRESH
                });
                ensure!(peers.len() < MAX_PEERS, "native control peer limit reached");
                // Before attachment, native history and the current snapshot establish the baseline.
                let state = PeerState {
                    baseline: 0,
                    snapshot: snapshot.clone(),
                    seen: Instant::now(),
                    acknowledged: snapshot
                        .events
                        .first()
                        .and_then(|event| event["seq"].as_u64())
                        .unwrap_or(snapshot.last_seq.saturating_add(1))
                        .saturating_sub(1),
                    events: VecDeque::new(),
                    attached: false,
                    operations: HashMap::new(),
                    next_operation: 0,
                    decisions: HashMap::new(),
                };
                peers.insert(
                    snapshot.generation.clone(),
                    Arc::new(Peer {
                        state: Mutex::new(state),
                        changed: Notify::new(),
                    }),
                );
            }
            peers[&snapshot.generation].clone()
        };
        let mut state = peer.state.lock().unwrap();
        ensure!(
            state.snapshot.registration == snapshot.registration,
            "native generation belongs to another writer"
        );
        let (acknowledged, events) = lifecycle_batch(
            &snapshot.events,
            snapshot.last_seq,
            state.acknowledged,
            state.baseline,
            state.attached.then_some(MAX_EVENTS - state.events.len()),
        )?;
        let mut acknowledged_results = Vec::new();
        for result in &snapshot.results {
            let Some(id) = result["id"].as_str() else {
                continue;
            };
            let operation = state.operations.get_mut(id);
            record_result(
                &self.claims,
                &snapshot.generation,
                id,
                operation
                    .as_ref()
                    .map(|operation| operation.digest.as_str()),
                result,
            )?;
            if let Some(operation) = operation {
                if let Some(existing) = &operation.result {
                    ensure!(existing == result, "native result changed");
                }
                operation.result = Some(result.clone());
                operation.claimed = true;
            }
            acknowledged_results.push(id.to_owned());
        }
        state.events.extend(events);
        state.acknowledged = acknowledged;
        state.decisions.retain(|id, _| {
            snapshot
                .approvals
                .iter()
                .any(|approval| approval["id"].as_str() == Some(id))
        });
        snapshot.events.clear();
        snapshot.results.clear();
        state.snapshot = snapshot;
        state.seen = Instant::now();
        let commands = pending_commands(&state.operations);
        let reply = json!({"generation":state.snapshot.generation, "ack_seq":acknowledged,
            "ack_results":acknowledged_results, "commands":commands});
        drop(state);
        peer.changed.notify_waiters();
        Ok(reply)
    }
}

// Backpressure delays lifecycle acknowledgement without delaying the current control snapshot.
fn lifecycle_batch(
    incoming: &[Value],
    last_seq: u64,
    acknowledged: u64,
    baseline: u64,
    capacity: Option<usize>,
) -> crate::Result<(u64, Vec<Value>)> {
    let mut sequence = acknowledged;
    let mut accepted = acknowledged;
    let mut events = Vec::new();
    for event in incoming {
        let next = event["seq"]
            .as_u64()
            .context("missing native event sequence")?;
        if next <= sequence {
            continue;
        }
        ensure!(
            next == sequence + 1 && next <= last_seq,
            "native lifecycle sequence is incomplete"
        );
        sequence = next;
        if next <= baseline || capacity.is_none_or(|capacity| events.len() < capacity) {
            accepted = next;
            if next > baseline && capacity.is_some() {
                events.push(event.clone());
            }
        }
    }
    ensure!(
        sequence == last_seq,
        "native lifecycle delivery is incomplete"
    );
    Ok((accepted, events))
}

// The existence of a durable claim forbids dispatch again after a daemon or module restart.
fn claim(directory: &Path, generation: &str, id: &str, digest: &str) -> crate::Result<bool> {
    let path = receipt_path(directory, generation, id)?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer(
        &mut file,
        &json!({"generation":generation, "id":id, "digest":digest}),
    )?;
    file.flush()?;
    file.as_file().sync_all()?;
    match file.persist_noclobber(&path) {
        Ok(_) => {}
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
        Err(error) => return Err(error.into()),
    }
    super::super::native_inbox::sync_directory(directory)?;
    Ok(true)
}

fn receipt_path(directory: &Path, generation: &str, id: &str) -> crate::Result<PathBuf> {
    Ok(directory.join(format!(
        "{}.json",
        hex::encode(Sha256::digest(serde_json::to_vec(&(generation, id))?))
    )))
}

fn read_receipt(path: &Path) -> crate::Result<Value> {
    let mut bytes = Vec::new();
    super::super::native_inbox::open_regular(path)?
        .take(4097)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 4096,
        "native control receipt exceeds its bound"
    );
    Ok(serde_json::from_slice(&bytes)?)
}

fn stored_result(directory: &Path, generation: &str, id: &str) -> crate::Result<Option<Value>> {
    let receipt = read_receipt(&receipt_path(directory, generation, id)?)?;
    ensure!(
        receipt["generation"] == generation && receipt["id"] == id,
        "native receipt belongs to another operation"
    );
    Ok(receipt.get("result").cloned())
}

fn record_result(
    directory: &Path,
    generation: &str,
    id: &str,
    digest: Option<&str>,
    result: &Value,
) -> crate::Result<()> {
    ensure!(
        super::super::native_inbox::valid_id(id),
        "native result needs an exact operation identity"
    );
    ensure!(
        matches!(
            result["outcome"].as_str(),
            Some(
                "accepted"
                    | "command_completed"
                    | "applied"
                    | "not_sent"
                    | "unknown"
                    | "requested"
                    | "no_longer_active"
            )
        ),
        "unrecognized native result"
    );
    let path = receipt_path(directory, generation, id)?;
    if !path.try_exists()? {
        ensure!(
            result["outcome"] == "not_sent",
            "native result preceded its durable claim"
        );
        claim(directory, generation, id, digest.unwrap_or(""))?;
    }
    let mut receipt = read_receipt(&path)?;
    ensure!(
        receipt["generation"] == generation
            && receipt["id"] == id
            && digest.is_none_or(|digest| receipt["digest"] == digest),
        "native result belongs to another operation"
    );
    if let Some(existing) = receipt.get("result") {
        ensure!(existing == result, "native result changed");
    }
    receipt["result"] = result.clone();
    write_private(&path, &receipt)
}

/// One supervisor holds native control; additional RC viewers share that supervisor.
pub struct Client {
    peer: Arc<Peer>,
    hub: Arc<Hub>,
    _lock: std::fs::File,
}

impl Client {
    pub(crate) fn unwritten_transcript(session: &str, cwd: &Path) -> Option<PathBuf> {
        let cwd = cwd.canonicalize().ok()?;
        Self::registered_sessions()
            .into_iter()
            .find_map(|registration| {
                if registration.session != session || registration.cwd != cwd {
                    return None;
                }
                let path = registration.transcript_target().ok()?;
                (!path.try_exists().ok()?).then_some(path)
            })
    }

    /// A live registration identifies empty sessions without creating transcript content.
    pub(crate) fn registered_sessions() -> Vec<Registration> {
        let Some(hub) = HUB.get().and_then(|hub| hub.lock().unwrap().upgrade()) else {
            return vec![];
        };
        if hub.closed.load(Ordering::Acquire) {
            return vec![];
        }
        hub.peers
            .lock()
            .unwrap()
            .values()
            .filter_map(|peer| {
                let state = peer.state.lock().unwrap();
                (state.seen.elapsed() < FRESH && state.snapshot.validate().is_ok())
                    .then(|| state.snapshot.registration.clone())
            })
            .collect()
    }

    /// A registered plugin can reconnect only to the daemon it explicitly selected.
    pub fn reconnecting(session: &str, cwd: &Path) -> bool {
        (|| -> crate::Result<bool> {
            let registry = crate::adapter::claude_code::sessions_dir()?.canonicalize()?;
            let live = crate::rc::claude_inbox::discover_in(&registry, session)
                .context("native writer is unavailable")?;
            let registration =
                super::read_registration(&directory()?.join(format!("{}.json", live.pid())))?;
            if registration.session != session
                || registration.cwd != cwd.canonicalize()?
                || registration.descriptor != descriptor_path()?
                || !registration.is_current()
            {
                return Ok(false);
            }
            let hub = HUB
                .get()
                .and_then(|hub| hub.lock().unwrap().upgrade())
                .context("native control listener is unavailable")?;
            Ok(!hub.closed.load(Ordering::Acquire)
                && !hub.peers.lock().unwrap().values().any(|peer| {
                    let state = peer.state.lock().unwrap();
                    state.snapshot.registration == registration
                        && (state.attached || state.snapshot.saturated)
                }))
        })()
        .unwrap_or(false)
    }

    pub fn available(session: &str, cwd: &Path) -> bool {
        let Some(hub) = HUB.get().and_then(|hub| hub.lock().unwrap().upgrade()) else {
            return false;
        };
        let Ok(cwd) = cwd.canonicalize() else {
            return false;
        };
        if hub.closed.load(Ordering::Acquire) {
            return false;
        }
        hub.peers
            .lock()
            .unwrap()
            .values()
            .filter(|peer| {
                let state = peer.state.lock().unwrap();
                !state.attached
                    && !state.snapshot.saturated
                    && state.snapshot.session == session
                    && state.snapshot.registration.cwd == cwd
                    && state.seen.elapsed() < FRESH
                    && state.snapshot.validate().is_ok()
            })
            .count()
            == 1
    }

    pub fn attach(session: &str, cwd: &Path) -> crate::Result<Self> {
        let hub = HUB
            .get()
            .and_then(|hub| hub.lock().unwrap().upgrade())
            .context("native control listener is not running")?;
        let cwd = cwd.canonicalize()?;
        let peers = hub.peers.lock().unwrap();
        let matching: Vec<_> = peers
            .values()
            .filter(|peer| {
                let state = peer.state.lock().unwrap();
                state.snapshot.session == session
                    && state.snapshot.registration.cwd == cwd
                    && state.seen.elapsed() < FRESH
                    && state.snapshot.validate().is_ok()
            })
            .cloned()
            .collect();
        ensure!(
            matching.len() == 1,
            "native control does not identify a unique current writer"
        );
        let peer = matching.into_iter().next().unwrap();
        let mut state = peer.state.lock().unwrap();
        ensure!(
            !state.attached && !state.snapshot.saturated,
            "native control is already held or exhausted"
        );
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(directory()?.join(format!("{}.control", state.snapshot.generation)))?;
        lock.try_lock_exclusive()
            .context("another supervisor controls the native writer")?;
        state.attached = true;
        // The current snapshot already includes lifecycle changes awaiting paged delivery.
        state.baseline = state.snapshot.latest_seq.unwrap_or(state.snapshot.last_seq);
        state.events.clear();
        drop(state);
        Ok(Self {
            peer,
            hub: hub.clone(),
            _lock: lock,
        })
    }

    pub fn snapshot(&self) -> crate::Result<Snapshot> {
        ensure!(
            !self.hub.closed.load(Ordering::Acquire),
            "native control listener has stopped"
        );
        let state = self.peer.state.lock().unwrap();
        state.snapshot.validate()?;
        Ok(state.snapshot.clone())
    }

    pub async fn next_update(&self) -> crate::Result<Update> {
        ensure!(
            !self.hub.closed.load(Ordering::Acquire),
            "native control listener has stopped"
        );
        let notified = self.peer.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        {
            let mut state = self.peer.state.lock().unwrap();
            state.snapshot.validate()?;
            if !state.events.is_empty() {
                return Ok(Update {
                    snapshot: state.snapshot.clone(),
                    events: state.events.drain(..).collect(),
                });
            }
        }
        let _ = tokio::time::timeout(FRESH, notified).await;
        ensure!(
            !self.hub.closed.load(Ordering::Acquire),
            "native control listener has stopped"
        );
        let mut state = self.peer.state.lock().unwrap();
        state.snapshot.validate()?;
        Ok(Update {
            snapshot: state.snapshot.clone(),
            events: state.events.drain(..).collect(),
        })
    }

    pub fn submit(&self, id: &str, method: &str, payload: Value) -> crate::Result<()> {
        ensure!(
            !self.hub.closed.load(Ordering::Acquire),
            "native control listener has stopped"
        );
        ensure!(
            super::super::native_inbox::valid_id(id)
                && matches!(method, "prompt" | "abort" | "model"),
            "invalid native operation"
        );
        if method == "prompt" {
            ensure!(
                payload["text"]
                    .as_str()
                    .is_some_and(|text| !text.trim().is_empty() && text.len() <= 128 * 1024),
                "native prompt must be nonempty and fit the control limit"
            );
        } else if method == "abort" {
            ensure!(
                payload["turnId"].as_str().is_some_and(|turn| {
                    super::super::harness::validate_native_turn_id(turn).is_ok()
                }),
                "native interruption needs an exact turn identity"
            );
        } else {
            ensure!(
                payload["model"]
                    .as_str()
                    .is_some_and(|model| !model.is_empty()
                        && model.len() <= 256
                        && !model.chars().any(char::is_control)),
                "native model must be an identifier"
            );
        }
        let mut state = self.peer.state.lock().unwrap();
        state.snapshot.validate()?;
        ensure!(
            state.seen.elapsed() < FRESH && !state.snapshot.saturated,
            "native control is reconnecting"
        );
        let mut command = payload
            .as_object()
            .context("native command needs an object")?
            .clone();
        command.insert("id".into(), json!(id));
        command.insert("method".into(), json!(method));
        command.insert("session".into(), json!(state.snapshot.session));
        command.insert("generation".into(), json!(state.snapshot.generation));
        command.insert("cwd".into(), json!(state.snapshot.cwd));
        let command = Value::Object(command);
        let digest = hex::encode(Sha256::digest(serde_json::to_vec(&command)?));
        if let Some(existing) = state.operations.get(id) {
            ensure!(
                existing.digest == digest,
                "native operation identity belongs to different content"
            );
            return Ok(());
        }
        // Completed receipts stay on disk, so memory bounds apply only to pending work.
        state
            .operations
            .retain(|_, operation| operation.result.is_none());
        ensure!(
            state.operations.len() < MAX_OPERATIONS,
            "native operation identity limit reached"
        );
        let path = receipt_path(&self.hub.claims, &state.snapshot.generation, id)?;
        let claimed = path.try_exists()?;
        let receipt = if claimed {
            Some(read_receipt(&path)?)
        } else {
            None
        };
        if let Some(receipt) = &receipt {
            ensure!(
                receipt["digest"] == digest,
                "native operation identity belongs to different content"
            );
        }
        let result = receipt.and_then(|receipt| receipt.get("result").cloned());
        let order = state.next_operation;
        state.next_operation = order
            .checked_add(1)
            .context("native operation sequence exhausted")?;
        state.operations.insert(
            id.into(),
            Operation {
                order,
                command,
                digest,
                claimed,
                result,
            },
        );
        Ok(())
    }

    pub fn result(&self, id: &str) -> Option<Value> {
        let state = self.peer.state.lock().unwrap();
        state
            .operations
            .get(id)
            .and_then(|operation| operation.result.clone())
            .or_else(|| {
                stored_result(&self.hub.claims, &state.snapshot.generation, id)
                    .ok()
                    .flatten()
            })
    }

    pub fn answer(&self, response: &crate::protocol::ApprovalResponse) -> crate::Result<()> {
        let id = &response.approval_id;
        ensure!(
            !self.hub.closed.load(Ordering::Acquire),
            "native control listener has stopped"
        );
        let mut state = self.peer.state.lock().unwrap();
        state.snapshot.validate()?;
        let approval = state
            .snapshot
            .approvals
            .iter()
            .find(|approval| state.seen.elapsed() < FRESH && approval["id"].as_str() == Some(id))
            .context("native approval is no longer pending")?;
        let decision = approval_decision(approval, response)?;
        if let Some(existing) = state.decisions.get(id) {
            ensure!(
                existing == &decision,
                "a native approval decision is already in flight"
            );
        } else {
            let key = format!("approval:{id}");
            let digest = hex::encode(Sha256::digest(serde_json::to_vec(&decision)?));
            if !claim(&self.hub.claims, &state.snapshot.generation, &key, &digest)? {
                let receipt = read_receipt(&receipt_path(
                    &self.hub.claims,
                    &state.snapshot.generation,
                    &key,
                )?)?;
                ensure!(
                    receipt["digest"].as_str() == Some(&digest),
                    "another native approval decision is already in flight"
                );
            }
            state.decisions.insert(id.clone(), decision);
        }
        Ok(())
    }
}

fn approval_decision(
    approval: &Value,
    response: &crate::protocol::ApprovalResponse,
) -> crate::Result<Value> {
    use crate::protocol::{ApprovalDecision, ApprovalScope};
    ensure!(
        response.scope == ApprovalScope::Once,
        "native approval requires a one-shot decision"
    );
    if response.decision == ApprovalDecision::Deny {
        let mut decision = json!({"behavior":"deny"});
        if let Some(message) = &response.message {
            decision["message"] = json!(message);
        }
        return Ok(decision);
    }
    let mut decision = json!({"behavior":"allow"});
    if approval["tool"] == "AskUserQuestion" {
        let questions = approval["input"]["questions"]
            .as_array()
            .context("native questions are missing")?;
        let answers = response
            .answers
            .as_ref()
            .context("native questions require answers")?;
        ensure!(
            !questions.is_empty()
                && answers.len() == questions.len()
                && questions
                    .iter()
                    .all(
                        |question| question["question"].as_str().is_some_and(|key| answers
                            .get(key)
                            .is_some_and(|values| !values.is_empty()
                                && (question["multiSelect"] == true || values.len() == 1)
                                && values.iter().all(|value| !value.trim().is_empty())))
                    ),
            "answers must match the pending native questions"
        );
        let mut input = approval["input"].clone();
        input["answers"] = json!(
            answers
                .iter()
                .map(|(key, values)| (key.clone(), values.join(", ")))
                .collect::<std::collections::BTreeMap<_, _>>()
        );
        decision["updatedInput"] = input;
    } else {
        ensure!(
            response.answers.is_none(),
            "this native approval does not accept answers"
        );
    }
    Ok(decision)
}

impl Drop for Client {
    fn drop(&mut self) {
        let mut state = self.peer.state.lock().unwrap();
        state.attached = false;
        // Undispatched commands do not survive detachment and acquire a different supervisor.
        state.operations.retain(|_, operation| operation.claimed);
        state.decisions.clear();
        state.events.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Discovery and attachment must work before the native writer creates its first record.
    /// A synthetic transcript would make completion evidence and the live tail disagree.
    #[test]
    fn empty_native_sessions_attach_without_materializing_history() {
        if crate::rc::in_isolated_test(
            "rc::native_claude::transport::tests::empty_native_sessions_attach_without_materializing_history",
        ) {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("AGIT_HOME", root.path().join("agit"));
            std::env::set_var("CLAUDE_CONFIG_DIR", root.path());
        }
        crate::rc::with_agit_home(root.path(), || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let cwd = root.path().canonicalize().unwrap();
                    let registry = cwd.join("sessions");
                    let socket = cwd.join("native.sock");
                    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
                    let (process, _) = super::super::process(std::process::id()).unwrap();
                    let session = uuid::Uuid::new_v4().to_string();
                    crate::rc::claude_inbox::fixture::register(
                        &registry,
                        process.pid,
                        &session,
                        &socket,
                        None,
                    );
                    let record_path = registry.join(format!("{}.json", process.pid));
                    let mut record: Value =
                        serde_json::from_slice(&std::fs::read(&record_path).unwrap()).unwrap();
                    record["cwd"] = json!(cwd);
                    std::fs::write(&record_path, record.to_string()).unwrap();
                    let endpoint = Endpoint::start().unwrap();
                    let registration = Registration {
                        version: 1,
                        session: session.clone(),
                        cwd: cwd.clone(),
                        generation: uuid::Uuid::new_v4().to_string(),
                        revision: process.birth.clone(),
                        process,
                        registry,
                        descriptor: descriptor_path().unwrap(),
                    };
                    super::super::publish(&directory().unwrap(), &registration).unwrap();
                    let lineage = crate::rc::lineage::AgitSession::new(
                        "fixture/native",
                        "00000000-0000-0000-0000-000000000001",
                        "work",
                    )
                    .unwrap();
                    let repo =
                        crate::domain::repo::Repo::init(&lineage.repo_dir().unwrap()).unwrap();
                    repo.git(&[
                        "-c",
                        "user.name=Fixture",
                        "-c",
                        "user.email=fixture@example.invalid",
                        "commit",
                        "--allow-empty",
                        "-m",
                        "Initialize capture fixture",
                    ])
                    .unwrap();
                    repo.git(&["branch", "work"]).unwrap();
                    crate::hub::identity::pin(
                        &repo,
                        &crate::hub::identity::RemoteIdentity::new(
                            "https://hub.invalid",
                            lineage.agent_id(),
                        )
                        .unwrap(),
                    )
                    .unwrap();
                    let store = crate::domain::store::Store::open_or_init().unwrap();
                    let mut claim =
                        crate::domain::link::Link::new("claude-code", &session, Some(&cwd));
                    claim.owner = Some("fixture".into());
                    claim.agent = Some("native".into());
                    claim.branch = Some("work".into());
                    crate::domain::link::write(&store, &claim).unwrap();
                    let resolve_capture = || {
                        crate::rc::capture::resolve(
                            "claude-code",
                            &session,
                            &registration.cwd,
                            Some(&lineage),
                            None,
                        )
                    };
                    assert!(resolve_capture().is_err());
                    let snapshot: Snapshot = serde_json::from_value(json!({
                        "registration":registration, "session":session, "cwd":cwd,
                        "generation":registration.generation, "version":1, "saturated":false,
                        "last_seq":0, "events":[], "results":[], "approvals":[], "tools":[],
                        "compacting":false, "permission_mode":"default"
                    }))
                    .unwrap();
                    endpoint.hub.poll(snapshot).unwrap();
                    assert_eq!(Client::registered_sessions(), vec![registration.clone()]);
                    let empty_history = crate::rc::local_history::read(json!({
                        "runtime":"claude-code", "session_id":session, "cwd":cwd,
                        "view":"conversation"
                    }))
                    .unwrap();
                    assert_eq!(empty_history["items"], json!([]));
                    assert_eq!(empty_history["has_more"], false);
                    assert_eq!(
                        resolve_capture().unwrap().unwrap().to_string(),
                        lineage.to_string()
                    );
                    let mut driver = crate::rc::harness::claude_native::ClaudeNativeDriver::attach(
                        crate::rc::harness::LaunchSpec {
                            cwd,
                            resume_from: Some(session.clone()),
                            agit_session: None,
                            model: None,
                            dangerous: false,
                            permission_mode: None,
                        },
                    )
                    .unwrap();
                    let path = driver.transcript_path().unwrap();
                    assert!(!path.exists());
                    assert!(registration.transcript_path().is_err());
                    assert!(matches!(driver.next_event().await,
                        Some(crate::rc::harness::HarnessEvent::Ready { runtime_thread_id, .. })
                        if runtime_thread_id == session));
                    let mut tailer = crate::rc::tail::Tailer::new(&path, false);
                    assert!(tailer.poll().unwrap().is_empty());
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    let prompt = json!({"type":"user","sessionId":session,
                        "message":{"role":"user","content":"First remote message"}});
                    std::fs::write(&path, format!("{prompt}\n")).unwrap();
                    assert_eq!(registration.transcript_path().unwrap(), path);
                    assert_eq!(tailer.poll().unwrap()[0].text, prompt.to_string());
                    assert!(tailer.poll().unwrap().is_empty());
                    record["sessionId"] = json!(uuid::Uuid::new_v4().to_string());
                    std::fs::write(record_path, record.to_string()).unwrap();
                    assert!(Client::registered_sessions().is_empty());
                    assert!(registration.transcript_target().is_err());
                });
        });
    }

    #[test]
    fn question_decisions_preserve_native_input_and_reject_unmatched_answers() {
        use crate::protocol::{ApprovalDecision, ApprovalResponse, ApprovalScope};
        let approval = json!({"tool":"AskUserQuestion","input":{
            "questions":[{"question":"Which labels?","multiSelect":true,
                "options":[{"label":"One"},{"label":"Two"}]}],"metadata":{"preview":"native"}}});
        let mut response = ApprovalResponse {
            approval_id: "question".into(),
            session_id: "session".into(),
            decision: ApprovalDecision::Allow,
            scope: ApprovalScope::Once,
            message: None,
            by: None,
            answers: Some(std::collections::BTreeMap::from([(
                "Which labels?".into(),
                vec!["One".into(), "Two".into()],
            )])),
        };
        let decision = approval_decision(&approval, &response).unwrap();
        assert_eq!(
            decision["updatedInput"]["questions"],
            approval["input"]["questions"]
        );
        assert_eq!(
            decision["updatedInput"]["metadata"],
            approval["input"]["metadata"]
        );
        assert_eq!(
            decision["updatedInput"]["answers"]["Which labels?"],
            "One, Two"
        );
        assert!(approval_decision(&json!({"tool":"Bash"}), &response).is_err());
        response
            .answers
            .as_mut()
            .unwrap()
            .insert("Other question".into(), vec!["Yes".into()]);
        assert!(approval_decision(&approval, &response).is_err());
        response.answers = None;
        assert!(approval_decision(&approval, &response).is_err());
        response.decision = ApprovalDecision::Deny;
        response.message = Some("Ask about the selected project".into());
        assert_eq!(
            approval_decision(&approval, &response).unwrap(),
            json!({
            "behavior":"deny","message":"Ask about the selected project"})
        );
    }

    #[test]
    fn lifecycle_backpressure_acknowledges_only_deliverable_events_and_accepts_retries() {
        let page = (1..=MAX_EVENTS as u64)
            .map(|seq| json!({"seq":seq,"kind":"tool_completed","id":seq.to_string()}))
            .collect::<Vec<_>>();
        let (ack, first) = lifecycle_batch(&page, 1024, 0, 0, Some(3)).unwrap();
        assert_eq!(ack, 3);
        assert_eq!(first, page[..3]);
        assert_eq!(
            lifecycle_batch(&page, 1024, ack, 0, Some(0)).unwrap(),
            (ack, vec![])
        );
        let (ack, rest) = lifecycle_batch(&page, 1024, ack, 0, Some(MAX_EVENTS)).unwrap();
        assert_eq!(ack, 1024);
        assert_eq!(rest, page[3..]);
        assert_eq!(
            lifecycle_batch(&page, 1024, ack, 0, Some(MAX_EVENTS)).unwrap(),
            (ack, vec![])
        );
        assert!(lifecycle_batch(&page[1..], 1024, 0, 0, Some(0)).is_err());
        assert_eq!(
            lifecycle_batch(&page, 1024, 0, 0, None).unwrap(),
            (1024, vec![])
        );
    }

    #[test]
    fn attachment_baseline_skips_paged_history_already_represented_by_the_live_snapshot() {
        let page = (1..=MAX_EVENTS as u64)
            .map(|seq| json!({"seq":seq,"kind":"turn_started","turn":seq.to_string()}))
            .collect::<Vec<_>>();
        assert_eq!(
            lifecycle_batch(&page, 1024, 0, 1100, Some(0)).unwrap(),
            (1024, vec![])
        );
        let next = (1025..=1200)
            .map(|seq| json!({"seq":seq,"kind":"turn_started","turn":seq.to_string()}))
            .collect::<Vec<_>>();
        let (ack, events) = lifecycle_batch(&next, 1200, 1024, 1100, Some(3)).unwrap();
        assert_eq!(ack, 1103);
        assert_eq!(
            events
                .iter()
                .map(|event| event["seq"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            vec![1101, 1102, 1103]
        );
    }

    #[test]
    fn pending_native_commands_keep_submission_order_across_poll_batches() {
        let mut operations: HashMap<_, _> = (0..40)
            .rev()
            .map(|order| {
                let id = uuid::Uuid::new_v4().to_string();
                (
                    id,
                    Operation {
                        order,
                        command: json!({"order":order}),
                        digest: String::new(),
                        claimed: false,
                        result: None,
                    },
                )
            })
            .collect();
        assert_eq!(
            pending_commands(&operations),
            (0..32)
                .map(|order| json!({"order":order}))
                .collect::<Vec<_>>()
        );
        for operation in operations.values_mut() {
            operation.claimed = operation.order < 32;
        }
        assert_eq!(
            pending_commands(&operations),
            (32..40)
                .map(|order| json!({"order":order}))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn discovery_keeps_a_live_owner_and_replaces_a_stale_daemon_lifetime() {
        let directory = tempfile::tempdir().unwrap();
        let candidates = ["cloud.json", "local.json"].map(|name| directory.path().join(name));
        let (owner, _) = super::super::process(std::process::id()).unwrap();
        let sockets = ["cloud.sock", "local.sock"].map(|name| directory.path().join(name));
        let _listeners = sockets
            .each_ref()
            .map(|path| std::os::unix::net::UnixListener::bind(path).unwrap());
        let descriptors = sockets.map(|socket| Descriptor {
            socket,
            token: "fixture".into(),
            owner: owner.clone(),
        });
        for (path, descriptor) in candidates.iter().zip(&descriptors) {
            write_private(path, descriptor).unwrap();
        }
        assert_eq!(
            select_descriptor(&candidates, None),
            Some(candidates[0].clone())
        );
        assert_eq!(
            select_descriptor(&candidates, Some(&candidates[1])),
            Some(candidates[1].clone())
        );
        let mut replaced = descriptors[1].clone();
        replaced.owner.birth.seconds += 1;
        write_private(&candidates[1], &replaced).unwrap();
        assert_eq!(
            select_descriptor(&candidates, Some(&candidates[1])),
            Some(candidates[0].clone())
        );
        std::fs::remove_file(&descriptors[0].socket).unwrap();
        assert_eq!(select_descriptor(&candidates, None), None);
        assert_eq!(
            select_descriptor(&candidates, Some(&candidates[1])),
            Some(candidates[1].clone())
        );
    }

    #[test]
    fn durable_claim_never_repeats_after_receipt_loss_or_partial_write() {
        let directory = tempfile::tempdir().unwrap();
        assert!(claim(directory.path(), "generation", "operation", "digest").unwrap());
        assert!(!claim(directory.path(), "generation", "operation", "digest").unwrap());
        assert!(!claim(directory.path(), "generation", "operation", "different").unwrap());
        let path = std::fs::read_dir(directory.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        std::fs::write(path, b"{").unwrap();
        assert!(
            !claim(directory.path(), "generation", "operation", "digest").unwrap(),
            "an unreadable receipt cannot authorize another native side effect"
        );
        assert!(claim(directory.path(), "new-generation", "operation", "digest").unwrap());
        for outcome in ["accepted", "command_completed", "applied"] {
            let id = uuid::Uuid::new_v4().to_string();
            let result = json!({"id":id,"outcome":outcome});
            assert!(claim(directory.path(), "generation", &id, "digest").unwrap());
            record_result(directory.path(), "generation", &id, Some("digest"), &result).unwrap();
            assert!(!claim(directory.path(), "generation", &id, "digest").unwrap());
            assert_eq!(
                read_receipt(&receipt_path(directory.path(), "generation", &id).unwrap()).unwrap()
                    ["result"],
                result
            );
            assert_eq!(
                stored_result(directory.path(), "generation", &id).unwrap(),
                Some(result)
            );
            assert!(stored_result(directory.path(), "another-generation", &id).is_err());
        }
        let unsent = uuid::Uuid::new_v4().to_string();
        record_result(
            directory.path(),
            "generation",
            &unsent,
            Some("digest"),
            &json!({"id":unsent,"outcome":"not_sent"}),
        )
        .unwrap();
        assert!(
            !claim(directory.path(), "generation", &unsent, "digest").unwrap(),
            "a native refusal before claim still retires its operation identity"
        );
    }
}
