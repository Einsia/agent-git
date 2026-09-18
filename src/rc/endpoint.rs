//! Shared executor endpoint routing keeps transport admission outside the executor.

mod cloud;
mod output;

use output::ClientOutput;

use super::admission::{Admission, Work};
use super::local::{Listener, Stream};
use crate::protocol::{CallerClaim, ErrorCode, Frame, RequestId, RpcError};
use anyhow::{Context, ensure};
use std::{collections::HashMap, path::PathBuf};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::mpsc,
};

pub const WORKSPACE: &str = "local-owner";
pub const MAX_FRAME: usize = 8 * 1024 * 1024;
const MAX_CLIENTS: usize = 16;
const CLIENT_QUEUE: usize = 256;
const MAX_PENDING: usize = 64;

/// Read a bounded record without allocating an attacker-selected line length.
async fn read_record<R: AsyncBufRead + Unpin>(reader: &mut R) -> crate::Result<Option<Vec<u8>>> {
    let mut record = Vec::new();
    loop {
        let bytes = reader.fill_buf().await?;
        if bytes.is_empty() {
            ensure!(record.is_empty(), "incomplete RPC record");
            return Ok(None);
        }
        let end = bytes.iter().position(|b| *b == b'\n').map(|n| n + 1);
        let take = end.unwrap_or(bytes.len());
        ensure!(
            record.len() + take <= MAX_FRAME,
            "RPC record exceeds size limit"
        );
        record.extend_from_slice(&bytes[..take]);
        reader.consume(take);
        if end.is_some() {
            return Ok(Some(record));
        }
    }
}

pub(super) fn validate_request(frame: &Frame) -> Result<(), RpcError> {
    if frame.id.is_none()
        || frame.method.is_none()
        || frame.result.is_some()
        || frame.error.is_some()
        || frame.caller.is_some()
        || frame.stream.is_some()
        || frame.seq.is_some()
    {
        return Err(RpcError::new(
            ErrorCode::MalformedFrame,
            "executor RPC accepts requests without caller metadata",
        ));
    }
    if frame
        .params_workspace_id()
        .is_some_and(|id| id != WORKSPACE)
    {
        return Err(RpcError::new(
            ErrorCode::WorkspaceNotFound,
            "executor RPC cannot address another workspace",
        ));
    }
    Ok(())
}

fn authorize(mut frame: Frame, client: u64) -> Result<Frame, RpcError> {
    validate_request(&frame)?;
    frame.caller = Some(CallerClaim {
        account_id: Some(format!("local:{client}")),
        username: None,
        role: "owner".into(),
        workspace_id: WORKSPACE.into(),
    });
    Ok(frame)
}

enum Incoming {
    Request(u64, Box<Frame>),
    Closed(u64),
}
struct Client {
    output: ClientOutput,
    task: tokio::task::JoinHandle<()>,
    peers: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    peer_slots: std::sync::Arc<tokio::sync::Semaphore>,
    cloud: Option<super::cloud::ingress::Client>,
}

struct MessageReceipt {
    digest: Vec<u8>,
    response: Option<Frame>,
    created: std::time::Instant,
}

fn message_key(frame: &Frame) -> Option<String> {
    if !matches!(frame.method(), "turn.start" | "turn.steer") {
        return None;
    }
    let params = frame.params.as_ref()?;
    let id = params.get("client_msg_id")?.as_str()?;
    let session = params.get("session_id")?.as_str()?;
    Some(serde_json::json!([frame.method(), session, id]).to_string())
}

fn message_digest(frame: &Frame) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    Sha256::digest(serde_json::to_vec(&frame.params).unwrap_or_default()).to_vec()
}
impl Drop for Client {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn attach(
    socket: Stream,
    client: u64,
    input: mpsc::Sender<Incoming>,
    controller: &agit_controller::Controller,
) -> Client {
    let (reader, mut writer) = tokio::io::split(socket);
    let (output, mut messages) = ClientOutput::channel(None);
    let peers = std::sync::Arc::new(std::sync::Mutex::new(
        std::collections::HashSet::<String>::new(),
    ));
    let interests = peers.clone();
    let event_output = output.clone();
    let mut events = controller.subscribe();
    let task = tokio::spawn(async move {
        let read = async {
            let mut reader = BufReader::new(reader);
            while let Some(record) = read_record(&mut reader).await? {
                let frame: Frame = serde_json::from_slice(&record)?;
                input
                    .send(Incoming::Request(client, Box::new(frame)))
                    .await?;
            }
            Ok::<_, anyhow::Error>(())
        };
        let write = async {
            while let Some(message) = messages.next().await? {
                writer.write_all(message.record.as_bytes()).await?;
                writer.write_all(b"\n").await?;
            }
            Ok::<_, anyhow::Error>(())
        };
        let forward = async {
            loop {
                // Lag terminates this client so its next attachment can replay.
                // Other clients retain independent cursors and output budgets.
                let event = events.recv().await.map_err(|_| ())?;
                let subscribed = interests
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .contains(event.peer_id());
                if subscribed {
                    event_output
                        .send_timeout(
                            super::peers::notification(&event).to_json(),
                            std::time::Duration::from_secs(2),
                        )
                        .await?;
                }
            }
            #[allow(unreachable_code)]
            Ok::<_, ()>(())
        };
        tokio::select! { _ = read => {}, _ = write => {}, _ = forward => {} }
        let _ = input.send(Incoming::Closed(client)).await;
    });
    Client {
        output,
        task,
        peers,
        peer_slots: std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_PENDING)),
        cloud: None,
    }
}

pub async fn serve(
    listener: Listener,
    outbound: super::outbound::OutboundRx,
    events: mpsc::Sender<super::link::LinkEvent>,
    controller: std::sync::Arc<agit_controller::Controller>,
    daemon: super::build_identity::DaemonIdentity,
    admission: Admission,
) -> crate::Result<()> {
    let identity = super::identity::identity()?;
    let instance = daemon.instance_id;
    let diagnostics = match super::diagnostics::Log::open(&instance) {
        Ok(log) => {
            log.record(
                "daemon.started",
                serde_json::json!({"version":env!("CARGO_PKG_VERSION")}),
            );
            Some(log)
        }
        Err(error) => {
            eprintln!("agitd: structured diagnostics unavailable: {error}");
            None
        }
    };
    let capabilities = tokio::task::spawn_blocking(|| {
        super::harness::drivable()
            .into_iter()
            .map(|capability| (capability.runtime.clone(), capability))
            .collect::<std::collections::BTreeMap<_, _>>()
    })
    .await?;
    let description = serde_json::json!({"protocol_version":1,"authority":"local-owner","machine":identity,"instance_id":instance,"epoch":1,"workspace_id":WORKSPACE,"capabilities":capabilities,"max_frame_bytes":MAX_FRAME,"history":{"version":2,"runtimes":["codex","claude-code","opencode"],"snapshot":true}});
    let mut description = description;
    description["build_id"] = serde_json::json!(daemon.build_id);
    description["rpc_features"] = serde_json::json!(daemon.rpc_features);
    description["diagnostic_log"] = serde_json::json!(diagnostics.as_ref().map(|log| log.path()));
    let cloud = cloud::Ingress::start(diagnostics.clone())?;
    serve_described(
        listener,
        outbound,
        events,
        description,
        controller,
        diagnostics,
        Some(cloud),
        admission,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn serve_described(
    #[allow(unused_mut)] mut listener: Listener,
    mut outbound: super::outbound::OutboundRx,
    events: mpsc::Sender<super::link::LinkEvent>,
    description: serde_json::Value,
    controller: std::sync::Arc<agit_controller::Controller>,
    diagnostics: Option<super::diagnostics::Log>,
    mut cloud: Option<cloud::Ingress>,
    admission: Admission,
) -> crate::Result<()> {
    let (input, mut incoming) = mpsc::channel::<Incoming>(256);
    let mut clients = HashMap::<u64, Client>::new();
    let history_slots = std::sync::Arc::new(tokio::sync::Semaphore::new(4));
    let mut pending = HashMap::<RequestId, (u64, RequestId, Option<String>, Work)>::new();
    let mut receipts = HashMap::<String, MessageReceipt>::new();
    let mut serial = 0_u64;
    let discovery_slots = std::sync::Arc::new(tokio::sync::Semaphore::new(2));
    let peer_slots = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_PENDING * MAX_CLIENTS));
    let mut peer_states = controller.subscribe();
    let cloud_clients = std::sync::Arc::new(super::cloud::Clients::default());
    let mut cloud_admissions = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            Some(accepted) = async { cloud.as_mut().unwrap().incoming.recv().await }, if cloud.is_some() => {
                if clients.values().filter(|client| client.cloud.is_some()).count() + cloud_admissions.len() >= MAX_CLIENTS {
                    continue;
                }
                let admission = cloud.as_ref().unwrap().registry.admitted(accepted.grant.clone());
                cloud_admissions.spawn(async move { (accepted, admission.await) });
            }
            Some(admitted) = cloud_admissions.join_next(), if !cloud_admissions.is_empty() => {
                let Ok((accepted, guard)) = admitted else { continue; };
                let guard = match guard {
                    Ok(guard) => guard,
                    Err(error) => {
                        if let Some(log) = &diagnostics { log.record("cloud.controller_rejected", serde_json::json!({"grant_id":accepted.grant.id,"reason":error.to_string()})); }
                        continue;
                    }
                };
                serial = serial.checked_add(1).context("client identity exhausted")?;
                if let Some(log) = &diagnostics {
                    log.record("cloud.client_attached", serde_json::json!({"client_id":serial,"grant_id":accepted.grant.id,"source_id":accepted.grant.source.id,"principal":accepted.grant.caller,"grant_expires_at_ms":accepted.grant.expires_at_ms}));
                }
                clients.insert(serial, cloud::attach(accepted, guard, serial, input.clone(), diagnostics.clone()));
            }
            state = peer_states.recv() => {
                if let Some(log) = &diagnostics {
                    match state {
                        Ok(agit_controller::Event::State { status }) => log.peer(&status),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => log.record("peer.log_gap", serde_json::json!({"missed":missed})),
                        _ => {},
                    }
                }
            }
            accepted = listener.accept() => {
                let (socket, _) = accepted?;
                // A peer can disappear before its credentials are read. Reject
                // that connection without terminating the shared listener.
                if super::local::authenticate_client(&socket).is_err() || clients.values().filter(|client| client.cloud.is_none()).count() >= MAX_CLIENTS { continue; }
                serial = serial.checked_add(1).context("local client identity exhausted")?;
                clients.insert(serial, attach(socket, serial, input.clone(), &controller));
            }
            Some(message) = incoming.recv() => match message {
                Incoming::Closed(client) => {
                    clients.remove(&client);
                    // Accepted work remains tracked until its response, even if its viewer leaves.
                }
                Incoming::Request(client, raw) => {
                    let Some(peer) = clients.get_mut(&client) else { continue };
                    if let Some(log) = &diagnostics { log.request(client, &raw); }
                    let original_id = raw.id.clone().unwrap_or(RequestId::Num(0));
                    let authorized = if let Some(guard) = &peer.cloud {
                        match guard.authorize(*raw) {
                            Ok(frame) => Ok(frame),
                            Err(super::cloud::ingress::Rejection::Reply(error)) => Err(error),
                            Err(super::cloud::ingress::Rejection::Close) => {
                                if let Some(log) = &diagnostics { log.record("cloud.client_rejected", serde_json::json!({"client_id":client,"reason":"invalid_or_active_request_id_or_capacity"})); }
                                clients.remove(&client);
                                continue;
                            }
                        }
                    } else { authorize(*raw, client) };
                    let Some(work) = admission.enter() else {
                        let response = Frame::error_response(original_id, RpcError::new(ErrorCode::SessionBusy,
                            "local daemon is restarting; nothing was accepted"));
                        let _ = peer.output.send_timeout(response.to_json(), std::time::Duration::from_secs(2)).await;
                        continue;
                    };
                    match authorized {
                        Ok(frame) if frame.method().starts_with("peer.") => {
                            if matches!(frame.method(), "peer.connect" | "peer.connect_cloud")
                                && let Some(id) = frame.params.as_ref().and_then(|p| p.get("peer_id")).and_then(serde_json::Value::as_str) {
                                    let mut peers = peer.peers.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                                    if peers.len() >= 64 || id.len() > 256 {
                                        let _ = peer.output.try_send_work(Frame::error_response(original_id, RpcError::new(ErrorCode::SessionBusy, "peer subscription capacity exceeded")).to_json(), work);
                                        continue;
                                    }
                                    peers.insert(id.into());
                            }
                            let Ok(permit) = peer.peer_slots.clone().try_acquire_owned() else {
                                let response = Frame::error_response(original_id, RpcError::new(ErrorCode::SessionBusy, "too many peer requests"));
                                let _ = peer.output.try_send_work(response.to_json(), work);
                                continue;
                            };
                            let Ok(global_permit) = peer_slots.clone().try_acquire_owned() else {
                                let _ = peer.output.try_send_work(Frame::error_response(original_id, RpcError::new(ErrorCode::SessionBusy, "peer request capacity exhausted")).to_json(), work);
                                continue;
                            };
                            let controller = controller.clone();
                            let cloud_clients = cloud_clients.clone();
                            let output = peer.output.clone();
                            let diagnostics = diagnostics.clone();
                            tokio::spawn(async move {
                                let _permit = (permit, global_permit);
                                let response = super::peers::dispatch(&controller, &cloud_clients, frame).await;
                                if let Some(log) = &diagnostics { log.response(client, &response); }
                                let _ = output.send_work(response.to_json(), std::time::Duration::from_secs(2), work).await;
                            });
                        }
                        Ok(frame) if frame.method() == "machine.describe" => {
                            let response = Frame::response(original_id, description.clone());
                            if let Some(log) = &diagnostics { log.response(client, &response); }
                            if peer.output.send_work(response.to_json(), std::time::Duration::from_secs(2), work).await.is_err() { clients.remove(&client); }
                        }
                        Ok(frame) if matches!(frame.method(), "runtime.models" | "session.goal.read") => {
                            let output = peer.output.clone();
                            let slots = discovery_slots.clone();
                            tokio::spawn(async move {
                                let permit = tokio::time::timeout(std::time::Duration::from_secs(30), slots.acquire_owned()).await;
                                let response = if let Ok(Ok(_permit)) = permit {
                                    if let Err(error) = frame.authority.check() {
                                        let _ = output.send_work(Frame::error_response(original_id, error).to_json(), std::time::Duration::from_secs(2), work).await;
                                        return;
                                    }
                                    let is_goal = frame.method() == "session.goal.read";
                                    let params = frame.params.unwrap_or_default();
                                    let runtime = params.get("runtime").and_then(serde_json::Value::as_str).unwrap_or("");
                                    let cwd = params.get("cwd").and_then(serde_json::Value::as_str).filter(|s| !s.is_empty()).map(PathBuf::from)
                                        .unwrap_or_else(|| crate::infra::config::user_home().unwrap_or_default());
                                    let result = if is_goal { super::local_goal::read(params).await } else { super::harness::models::discover(runtime, cwd).await };
                                    match result {
                                        Ok(models) => Frame::response(original_id, models),
                                        Err(error) => Frame::error_response(original_id, RpcError::new(ErrorCode::RuntimeUnavailable, error.to_string())),
                                    }
                                } else { Frame::error_response(original_id, RpcError::new(ErrorCode::SessionBusy, "Runtime inspection is busy; try again shortly")) };
                                let _ = output.send_work(response.to_json(), std::time::Duration::from_secs(2), work).await;
                            });
                        }
                        Ok(frame) if frame.method() == "session.history" => {
                            let Ok(permit) = history_slots.clone().try_acquire_owned() else {
                                let response = Frame::error_response(original_id, super::local_history::rpc_error(super::local_history::Failure::Busy.into()));
                                let _ = peer.output.send_work(response.to_json(), std::time::Duration::from_secs(2), work).await;
                                continue;
                            };
                            let output = peer.output.clone();
                            let diagnostics = diagnostics.clone();
                            let started = std::time::Instant::now();
                            tokio::spawn(async move {
                                let result = tokio::task::spawn_blocking(move || {
                                    let _permit = permit;
                                    frame.authority.check().map_err(|error| anyhow::anyhow!(error.message))?;
                                    super::local_history::read(frame.params.unwrap_or_default())
                                }).await;
                                let response = match result {
                                    Ok(Ok(value)) => Frame::response(original_id,value),
                                    Ok(Err(error)) => Frame::error_response(original_id,super::local_history::rpc_error(error)),
                                    Err(_) => Frame::error_response(original_id,RpcError::new(ErrorCode::RuntimeUnavailable,"History reader stopped unexpectedly")),
                                };
                                if let Some(log) = &diagnostics {
                                    log.response(client, &response);
                                    log.record("history.read_completed", serde_json::json!({"client_id":client,"request_id":response.id,"elapsed_ms":started.elapsed().as_secs_f64()*1000.0}));
                                }
                                let _ = output.send_work(response.to_json(),std::time::Duration::from_secs(2), work).await;
                            });
                        }
                        Ok(mut frame) => {
                            if pending.len() >= MAX_PENDING * MAX_CLIENTS || pending.values().filter(|(owner, _, _, _)| *owner == client).count() >= MAX_PENDING {
                                clients.remove(&client);
                                continue;
                            }
                            let key = message_key(&frame).map(|key| peer.cloud.as_ref().map_or_else(|| key.clone(), |guard| guard.receipt_key(key.clone(), &frame)));
                            if let Some(key) = &key {
                                receipts.retain(|_, entry| entry.response.is_none() || entry.created.elapsed().as_secs() < 600);
                                if let Some(entry) = receipts.get(key) {
                                    let mut response = if entry.digest != message_digest(&frame) {
                                        Frame::error_response(original_id.clone(), RpcError::new(ErrorCode::SessionBusy, "message ID was already used with different content"))
                                    } else if let Some(response) = &entry.response {
                                        response.clone()
                                    } else {
                                        let mut error = RpcError::new(ErrorCode::SessionBusy, "message acceptance is pending; retry with the same message ID");
                                        error.data = Some(serde_json::json!({"outcome":"unknown"}));
                                        Frame::error_response(original_id.clone(), error)
                                    };
                                    response.id = Some(original_id);
                                    if peer.output.send_work(response.to_json(), std::time::Duration::from_secs(2), work).await.is_err() { clients.remove(&client); }
                                    continue;
                                }
                                if receipts.len() >= 4096 || key.len() > 1024 {
                                    let response = Frame::error_response(original_id, RpcError::new(ErrorCode::SessionBusy, "message retry capacity exceeded; nothing was sent"));
                                    if peer.output.send_work(response.to_json(), std::time::Duration::from_secs(2), work).await.is_err() { clients.remove(&client); }
                                    continue;
                                }
                                receipts.insert(key.clone(), MessageReceipt { digest: message_digest(&frame), response: None, created: std::time::Instant::now() });
                            }
                            let id = RequestId::fresh();
                            if let Some(log) = &diagnostics {
                                log.dispatch(client, &original_id, &id, frame.method());
                            }
                            frame.id = Some(id.clone());
                            pending.insert(id, (client, original_id, key, work));
                            events.send(super::link::LinkEvent::Frame { epoch: 1, frame: Box::new(frame) }).await?;
                        }
                        Err(error) => {
                            if peer.output.send_work(Frame::error_response(original_id, error).to_json(), std::time::Duration::from_secs(2), work).await.is_err() { clients.remove(&client); }
                        }
                    }
                }
            },
            write = outbound.next_write() => {
                let Some(mut write) = write else { break };
                let mut frame = write.frame().clone();
                let replay = write.take_replay();
                write.commit();
                if let Some(id) = &frame.id {
                    if let Some((client, original, key, work)) = pending.remove(id) {
                        if let Some(log) = &diagnostics {
                            let mut response = frame.clone();
                            response.id = Some(original.clone());
                            log.response(client, &response);
                        }
                        if let Some(key) = key {
                            // Daemon busy replies certify no native write unless they explicitly
                            // retain uncertainty. An uncertain receipt keeps its operation identity.
                            let not_sent = if let Some(error) = frame.error.as_mut().filter(|error| error.is(ErrorCode::SessionBusy)) {
                                let data = error.data.get_or_insert_with(|| serde_json::json!({}));
                                if let Some(object) = data.as_object_mut() {
                                    object.entry("outcome").or_insert_with(|| serde_json::json!("not_sent"));
                                }
                                data.get("outcome").and_then(serde_json::Value::as_str) == Some("not_sent")
                            } else {
                                false
                            };
                            if not_sent {
                                receipts.remove(&key);
                            } else if let Some(entry) = receipts.get_mut(&key) {
                                entry.response = Some(frame.clone());
                            }
                        }
                        frame.id = Some(original);
                        if let Some(peer) = clients.get(&client) {
                            if let Some(cloud) = &peer.cloud {
                                cloud.observe_response(&frame);
                            }
                            let result = if let Some(replay) = replay {
                                let frames = replay.frames.len();
                                let result = peer.output.send_replay(frame.to_json(), replay, work);
                                if let Some(log) = &diagnostics {
                                    log.record("executor.replay_queued", serde_json::json!({"client_id":client,"request_id":frame.id,"frames":frames,"succeeded":result.is_ok()}));
                                }
                                result
                            } else {
                                peer.output.send_work(frame.to_json(), std::time::Duration::from_secs(2), work).await
                            };
                            if result.is_err() {
                                if let Some(log) = &diagnostics {
                                    log.record("executor.client_closed", serde_json::json!({"client_id":client,"reason":"response_output_capacity"}));
                                }
                                clients.remove(&client);
                            }
                        }
                    }
                } else {
                    if frame.method() == crate::protocol::method::SESSION_STATUS
                        && let Some(log) = &diagnostics {
                            log.record("executor.state", serde_json::json!({"stream":frame.stream,"seq":frame.seq,"status":frame.params.as_ref().and_then(|params|params.get("status")).and_then(serde_json::Value::as_str)}));
                        }
                    let record = frame.to_json();
                    let mut closed = Vec::new();
                    for (id, peer) in &clients {
                        if peer.cloud.as_ref().is_some_and(|guard| !guard.accepts_notification(&frame)) {
                            continue;
                        }
                        // Subscription replay stays in its requester's writer. Only live
                        // notifications consume each client's event queue here.
                        if peer.output.send_timeout(record.clone(), std::time::Duration::from_secs(2)).await.is_err() {
                            if let Some(log) = &diagnostics {
                                log.record("executor.client_closed", serde_json::json!({"client_id":id,"reason":"event_output_capacity","stream":frame.stream,"seq":frame.seq}));
                            }
                            closed.push(*id);
                        }
                    }
                    for id in closed { clients.remove(&id); }
                }
            }
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use tokio::net::{UnixListener, UnixStream};

    #[tokio::test]
    async fn detached_accepted_requests_keep_the_restart_boundary_closed() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("rpc");
        let listener = UnixListener::bind(&path).unwrap();
        let (out, outbound) = super::super::outbound::channel();
        let (events, mut requests) = mpsc::channel(16);
        let admission = Admission::default();
        let server = tokio::spawn(serve_described(
            listener,
            outbound,
            events,
            serde_json::json!({}),
            super::super::peers::controller().unwrap(),
            None,
            None,
            admission.clone(),
        ));
        let mut client = UnixStream::connect(&path).await.unwrap();
        let request = Frame::request("project.bind", serde_json::json!({}));
        client
            .write_all(format!("{}\n", request.to_json()).as_bytes())
            .await
            .unwrap();
        let super::super::link::LinkEvent::Frame { frame, .. } = requests.recv().await.unwrap();
        drop(client);
        assert!(admission.freeze().is_err());
        out.send(Frame::response(
            frame.id.unwrap(),
            serde_json::json!({"ok":true}),
        ));
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Ok(frozen) = admission.freeze() {
                    drop(frozen);
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        server.abort();
    }
    #[tokio::test]
    async fn a_peer_closed_before_authentication_cannot_stop_other_clients() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("rpc");
        let listener = UnixListener::bind(&path).unwrap();
        // Queue a closed peer before the accept loop can inspect credentials.
        drop(std::os::unix::net::UnixStream::connect(&path).unwrap());
        let (_out, outbound) = super::super::outbound::channel();
        let (events, _requests) = mpsc::channel(16);
        let server = tokio::spawn(serve_described(
            listener,
            outbound,
            events,
            serde_json::json!({"instance_id":"survivor"}),
            super::super::peers::controller().unwrap(),
            None,
            None,
            Admission::default(),
        ));
        let mut client = BufReader::new(UnixStream::connect(&path).await.unwrap());
        let request = Frame::request("machine.describe", serde_json::json!({}));
        client
            .get_mut()
            .write_all(format!("{}\n", request.to_json()).as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.read_line(&mut line),
        )
        .await
        .unwrap()
        .unwrap();
        let reply: Frame = serde_json::from_str(&line).unwrap();
        assert_eq!(reply.result.unwrap()["instance_id"], "survivor");
        assert!(!server.is_finished());
        server.abort();
    }

    #[test]
    fn local_authority_refuses_hub_scope_and_forged_claim() {
        let request = |params| Frame::request("workspace.list", params);
        assert!(authorize(request(serde_json::json!({"workspace_id":"hub-owner"})), 1).is_err());
        let mut frame = request(serde_json::json!({"workspace_id":WORKSPACE}));
        frame.caller = Some(CallerClaim {
            account_id: None,
            username: None,
            role: "owner".into(),
            workspace_id: WORKSPACE.into(),
        });
        assert!(authorize(frame, 1).is_err());
        let frame = authorize(request(serde_json::json!({"workspace_id":WORKSPACE})), 4).unwrap();
        assert_eq!(frame.caller.unwrap().account_id.as_deref(), Some("local:4"));
    }
    #[tokio::test]
    async fn framing_rejects_truncated_and_oversized_records() {
        assert!(read_record(&mut &b"{}"[..]).await.is_err());
        let oversized = vec![b'x'; MAX_FRAME + 1];
        assert!(read_record(&mut oversized.as_slice()).await.is_err());
        let mut data = &b"{}\n{}\n"[..];
        assert_eq!(read_record(&mut data).await.unwrap().unwrap(), b"{}\n");
        assert!(read_record(&mut data).await.unwrap().is_some());
        assert!(read_record(&mut data).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn accepted_messages_replay_across_clients_without_a_second_native_dispatch() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("rpc");
        let listener = UnixListener::bind(&path).unwrap();
        let (out, outbound) = super::super::outbound::channel();
        let (events, mut requests) = mpsc::channel(16);
        let server = tokio::spawn(serve_described(
            listener,
            outbound,
            events,
            serde_json::json!({}),
            super::super::peers::controller().unwrap(),
            None,
            None,
            Admission::default(),
        ));
        let mut first = BufReader::new(UnixStream::connect(&path).await.unwrap());
        let request = Frame::request(
            "turn.start",
            serde_json::json!({"session_id":"session-a","client_msg_id":"message-a","message":"hello"}),
        );
        first
            .get_mut()
            .write_all(format!("{}\n", request.to_json()).as_bytes())
            .await
            .unwrap();
        let Some(super::super::link::LinkEvent::Frame { frame, .. }) = requests.recv().await else {
            panic!("missing dispatch")
        };
        let mut pending_peer = BufReader::new(UnixStream::connect(&path).await.unwrap());
        pending_peer
            .get_mut()
            .write_all(format!("{}\n", request.to_json()).as_bytes())
            .await
            .unwrap();
        let mut pending_line = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            pending_peer.read_line(&mut pending_line),
        )
        .await
        .unwrap()
        .unwrap();
        let pending_error = serde_json::from_str::<Frame>(&pending_line)
            .unwrap()
            .error
            .unwrap();
        assert!(pending_error.is(ErrorCode::SessionBusy));
        assert_eq!(pending_error.data.unwrap()["outcome"], "unknown");
        assert!(requests.try_recv().is_err());
        out.send(Frame::error_response(
            frame.id.clone().unwrap(),
            RpcError::new(ErrorCode::SessionBusy, "native admission is occupied")
                .with_hint("retry"),
        ));
        let mut rejected_line = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            first.read_line(&mut rejected_line),
        )
        .await
        .unwrap()
        .unwrap();
        let rejected_error = serde_json::from_str::<Frame>(&rejected_line)
            .unwrap()
            .error
            .unwrap();
        assert_eq!(rejected_error.data.as_ref().unwrap()["outcome"], "not_sent");
        assert_eq!(rejected_error.data.unwrap()["hint"], "retry");
        first
            .get_mut()
            .write_all(format!("{}\n", request.to_json()).as_bytes())
            .await
            .unwrap();
        let Some(super::super::link::LinkEvent::Frame { frame, .. }) =
            tokio::time::timeout(std::time::Duration::from_secs(5), requests.recv())
                .await
                .unwrap()
        else {
            panic!("missing retry dispatch")
        };
        drop(first);
        out.send(Frame::response(
            frame.id.clone().unwrap(),
            serde_json::json!({"turn_id":"turn-a"}),
        ));
        let mut second = BufReader::new(UnixStream::connect(&path).await.unwrap());
        let mut line = String::new();
        second
            .get_mut()
            .write_all(format!("{}\n", request.to_json()).as_bytes())
            .await
            .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            second.read_line(&mut line),
        )
        .await
        .unwrap()
        .unwrap();
        let response: Frame = serde_json::from_str(&line).unwrap();
        assert_eq!(response.result.unwrap()["turn_id"], "turn-a");
        assert!(requests.try_recv().is_err());
        let changed = Frame::request(
            "turn.start",
            serde_json::json!({"session_id":"session-a","client_msg_id":"message-a","message":"different"}),
        );
        second
            .get_mut()
            .write_all(format!("{}\n", changed.to_json()).as_bytes())
            .await
            .unwrap();
        line.clear();
        second.read_line(&mut line).await.unwrap();
        assert!(
            serde_json::from_str::<Frame>(&line)
                .unwrap()
                .error
                .unwrap()
                .message
                .contains("different content")
        );
        assert!(requests.try_recv().is_err());
        let uncertain = Frame::request(
            "turn.start",
            serde_json::json!({"session_id":"session-a","client_msg_id":"uncertain-message","message":"retain"}),
        );
        second
            .get_mut()
            .write_all(format!("{}\n", uncertain.to_json()).as_bytes())
            .await
            .unwrap();
        let Some(super::super::link::LinkEvent::Frame { frame, .. }) =
            tokio::time::timeout(std::time::Duration::from_secs(5), requests.recv())
                .await
                .unwrap()
        else {
            panic!("missing uncertain dispatch")
        };
        let mut error = RpcError::new(ErrorCode::SessionBusy, "acceptance is uncertain");
        error.data = Some(serde_json::json!({"outcome":"unknown"}));
        out.send(Frame::error_response(frame.id.unwrap(), error));
        for replay in [false, true] {
            if replay {
                second
                    .get_mut()
                    .write_all(format!("{}\n", uncertain.to_json()).as_bytes())
                    .await
                    .unwrap();
            }
            line.clear();
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                second.read_line(&mut line),
            )
            .await
            .unwrap()
            .unwrap();
            let error = serde_json::from_str::<Frame>(&line).unwrap().error.unwrap();
            assert_eq!(error.data.unwrap()["outcome"], "unknown");
            assert!(requests.try_recv().is_err());
        }
        server.abort();
    }
}
