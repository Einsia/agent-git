use super::*;
use agit_tunnel::{Packet, PacketSink, PacketSource};
use futures_util::{SinkExt, StreamExt};
use std::sync::atomic::{AtomicU8, Ordering};

const QUEUED: u8 = 0;
const WRITING: u8 = 1;
const CANCELLED: u8 = 2;

struct Write {
    packet: Packet,
    deadline: tokio::time::Instant,
    delivery: Arc<AtomicU8>,
    _budget: Option<Arc<tokio::sync::OwnedSemaphorePermit>>,
}

const HANDSHAKE: Duration = Duration::from_secs(20);
const HEALTH_INTERVAL: Duration = Duration::from_secs(15);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(15);

struct Task(tokio::task::JoinHandle<()>);
impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn publish(
    status: &watch::Sender<Status>,
    events: &broadcast::Sender<Event>,
    update: impl FnOnce(&mut Status),
) {
    status.send_modify(update);
    let _ = events.send(Event::State {
        status: status.borrow().clone(),
    });
}

pub(super) async fn run(
    worker: Worker,
    route: Arc<dyn Connector>,
    mut fingerprint: Option<String>,
    mut requests: mpsc::Receiver<Request>,
    status: watch::Sender<Status>,
    events: broadcast::Sender<Event>,
    shared_budget: Option<Arc<tokio::sync::Semaphore>>,
) {
    let mut backoff = Duration::from_millis(250);
    loop {
        publish(&status, &events, |s| {
            s.state = State::Connecting;
            s.worker_pid = None;
        });
        let opening = std::time::Instant::now();
        let connecting = route.open(&worker);
        tokio::pin!(connecting);
        let connected = loop {
            tokio::select! {
                result = &mut connecting => break result,
                request = requests.recv() => match request {
                    Some(request) => request.fail("not_sent", "peer is connecting"),
                    None => return,
                },
            }
        };
        let result = match connected {
            Ok(connection) => {
                let transport_ms = opening.elapsed().as_secs_f64() * 1000.0;
                let pid = connection.worker_pid;
                let (mut sink, mut source) = connection.split();
                let describing = std::time::Instant::now();
                let description = {
                    let handshake = tokio::time::timeout(
                        HANDSHAKE,
                        describe(&mut sink, &mut source, route.authority()),
                    );
                    tokio::pin!(handshake);
                    loop {
                        tokio::select! {
                            result = &mut handshake => break result.context("peer handshake timed out").and_then(|r| r),
                            request = requests.recv() => match request {
                                Some(request) => request.fail("not_sent", "peer handshake is pending"),
                                None => return,
                            },
                        }
                    }
                };
                {
                    let current = status.borrow();
                    crate::diagnostics::record(serde_json::json!({
                        "event": "controller.peer_handshake",
                        "peer_id": current.peer_id,
                        "route_id": current.route_id,
                        "worker_pid": pid,
                        "succeeded": description.is_ok(),
                        "transport_ms": transport_ms,
                        "describe_ms": describing.elapsed().as_secs_f64() * 1000.0,
                        "elapsed_ms": opening.elapsed().as_secs_f64() * 1000.0,
                    }));
                }
                match description {
                    Ok(description) => {
                        let actual = description["machine"]["machine_fingerprint"]
                            .as_str()
                            .unwrap_or("");
                        if actual.is_empty()
                            || fingerprint
                                .as_deref()
                                .is_some_and(|expected| expected != actual)
                        {
                            publish(&status, &events, |s| {
                                s.state = State::Rejected;
                                s.error = Some("peer fingerprint changed or is missing".into());
                            });
                            return;
                        }
                        fingerprint = Some(actual.into());
                        publish(&status, &events, |s| {
                            s.generation += 1;
                            s.state = State::Online;
                            s.description = Some(description);
                            s.worker_pid = Some(pid);
                            s.error = None;
                        });
                        backoff = Duration::from_millis(250);
                        online(
                            sink,
                            source,
                            &mut requests,
                            &status,
                            &events,
                            shared_budget.clone(),
                        )
                        .await
                    }
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(error),
        };
        publish(&status, &events, |s| {
            s.state = State::Backoff;
            s.worker_pid = None;
            s.error = result.err().map(|e| e.to_string());
        });
        // Jitter spreads retries across daemons without giving transport workers
        // responsibility for connection policy.
        let jitter = u128::from(uuid::Uuid::new_v4().as_bytes()[0]);
        let pause = tokio::time::sleep(
            backoff + Duration::from_millis((backoff.as_millis() * jitter / 1024) as u64),
        );
        tokio::pin!(pause);
        loop {
            tokio::select! {
                _ = &mut pause => break,
                request = requests.recv() => match request {
                    Some(request) => request.fail("not_sent", "peer is reconnecting"),
                    None => return,
                },
            }
        }
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

async fn describe(
    sink: &mut PacketSink,
    source: &mut PacketSource,
    authority: Authority,
) -> anyhow::Result<Value> {
    let id = uuid::Uuid::new_v4().to_string();
    let frame =
        serde_json::json!({"jsonrpc":"2.0","id":id,"method":"machine.describe","params":{}});
    sink.send(Packet::Text(frame.to_string())).await?;
    loop {
        let packet = source
            .next()
            .await
            .context("peer closed during handshake")??;
        let Some(frame) = decode(packet)? else {
            continue;
        };
        if frame["id"].as_str() != Some(&id) {
            continue;
        }
        ensure!(frame.get("error").is_none(), "peer rejected discovery");
        let description = frame
            .get("result")
            .context("peer omitted its description")?
            .clone();
        ensure!(
            description["protocol_version"] == 1 && description["authority"] == authority.as_str(),
            "unsupported peer executor protocol"
        );
        ensure!(
            description["instance_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty()),
            "peer instance is missing"
        );
        ensure!(
            description.to_string().len() <= 65536,
            "peer description exceeds the limit"
        );
        return Ok(description);
    }
}

fn decode(packet: Packet) -> anyhow::Result<Option<Value>> {
    match packet {
        Packet::Text(text) => {
            let frame: Value = serde_json::from_str(&text).context("peer sent malformed JSON")?;
            ensure!(
                frame["jsonrpc"] == "2.0" && frame.is_object(),
                "peer sent an invalid RPC frame"
            );
            Ok(Some(frame))
        }
        Packet::Ping(_) | Packet::Pong(_) => Ok(None),
        _ => anyhow::bail!("peer closed or sent a non-RPC packet"),
    }
}

async fn online(
    mut sink: PacketSink,
    mut source: PacketSource,
    requests: &mut mpsc::Receiver<Request>,
    status: &watch::Sender<Status>,
    events: &broadcast::Sender<Event>,
    shared_budget: Option<Arc<tokio::sync::Semaphore>>,
) -> anyhow::Result<()> {
    let (writes, mut queued) = mpsc::channel::<Write>(MAX_PENDING);
    let (failed, mut failure) = oneshot::channel();
    let _writer = Task(tokio::spawn(async move {
        while let Some(write) = queued.recv().await {
            if write.deadline <= tokio::time::Instant::now() {
                let _ = write.delivery.compare_exchange(
                    QUEUED,
                    CANCELLED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
                continue;
            }
            if write
                .delivery
                .compare_exchange(QUEUED, WRITING, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                continue;
            }
            match tokio::time::timeout(Duration::from_secs(30), sink.send(write.packet)).await {
                Ok(Ok(())) => {}
                _ => {
                    let _ = failed.send(());
                    return;
                }
            }
        }
    }));
    let mut pending = HashMap::<String, Request>::new();
    let event_budget = Arc::new(tokio::sync::Semaphore::new(QUEUE_BYTES));
    let mut health = tokio::time::interval(HEALTH_INTERVAL);
    health.tick().await;
    let mut expiration = tokio::time::interval(Duration::from_millis(50));
    let mut health_request: Option<(String, tokio::time::Instant)> = None;
    let result = async {
        loop {
            tokio::select! {
                request = requests.recv() => {
                    let Some(mut request) = request else { return Ok(()) };
                    if request.generation != status.borrow().generation {
                        request.fail("not_sent", "peer connection generation changed; reattach before sending");
                        continue;
                    }
                    if request.reply.is_closed() || request.deadline <= tokio::time::Instant::now() {
                        request.fail("not_sent", "request expired before admission");
                        continue;
                    }
                    if pending.len() >= MAX_PENDING {
                        request.fail("not_sent", "peer pending request capacity exhausted");
                        continue;
                    }
                    let write = Write { packet: request.packet.take().expect("admission owns its packet"), deadline: request.deadline, delivery: request.delivery.clone(), _budget: Some(request._budget.clone()) };
                    if writes.try_send(write).is_err() {
                        request.fail("not_sent", "peer write queue is full or closed");
                        continue;
                    }
                    pending.insert(request.id.clone(), request);
                }
                packet = source.next() => {
                    let packet = packet.context("peer disconnected")??;
                    let bytes = packet.len().max(1) as u32;
                    let Some(frame) = decode(packet)? else { continue };
                    if let Some(id) = frame.get("id") {
                        let id = id.as_str().context("peer response identity is invalid")?;
                        ensure!(frame.get("method").is_none() && (frame.get("result").is_some() ^ frame.get("error").is_some()), "peer response is malformed");
                        if let Some(error) = frame.get("error") {
                            ensure!(error["code"].as_i64().is_some_and(|code| i32::try_from(code).is_ok()) && error["message"].is_string(), "peer RPC error is malformed");
                        }
                        if health_request.as_ref().is_some_and(|(health_id, _)| health_id == id) {
                            let known = status.borrow().description.clone();
                            ensure!(frame.get("result") == known.as_ref(), "peer identity or capabilities changed; reconnect to refresh discovery");
                            health_request = None;
                        } else if let Some(request) = pending.remove(id) {
                            let _ = request.reply.send(Ok(frame));
                        }
                    } else {
                        ensure!(frame["method"].is_string() && frame.get("result").is_none() && frame.get("error").is_none(), "peer notification is malformed");
                        let permit = event_budget.clone().try_acquire_many_owned(bytes).context("peer event byte budget exhausted; reconnect and replay")?;
                        let shared = shared_budget.as_ref().map(|budget| budget.clone().try_acquire_many_owned(bytes)).transpose().context("host event byte budget exhausted; reconnect and replay")?;
                        let current = status.borrow();
                        let _ = events.send(Event::Frame { peer_id: current.peer_id.clone(), route_id: current.route_id.clone(), generation: current.generation, frame: Arc::new(frame), budget: Arc::new(permit), shared_budget: shared.map(Arc::new) });
                    }
                }
                _ = &mut failure => anyhow::bail!("tunnel could not confirm a write"),
                _ = expiration.tick() => {
                    let now = tokio::time::Instant::now();
                    let expired = pending.iter().filter(|(_, request)| request.deadline <= now).map(|(id, _)| id.clone()).collect::<Vec<_>>();
                    for id in expired {
                        if let Some(request) = pending.remove(&id) {
                            let state = request.delivery.compare_exchange(QUEUED, CANCELLED, Ordering::AcqRel, Ordering::Acquire).unwrap_or_else(|state| state);
                            let outcome = if state == WRITING { "unknown" } else { "not_sent" };
                            request.fail(outcome, "peer request timed out; inspect the operation before retrying");
                        }
                    }
                    if health_request.as_ref().is_some_and(|(_, deadline)| *deadline <= now) {
                        anyhow::bail!("peer health check timed out");
                    }
                }
                _ = health.tick(), if health_request.is_none() => {
                    let id = uuid::Uuid::new_v4().to_string();
                    let frame = serde_json::json!({"jsonrpc":"2.0","id":id,"method":"machine.describe","params":{}});
                    writes.try_send(Write { packet: Packet::Text(frame.to_string()), deadline: tokio::time::Instant::now() + HEALTH_TIMEOUT, delivery: Arc::new(AtomicU8::new(QUEUED)), _budget: None }).map_err(|_| anyhow::anyhow!("peer health queue is unavailable"))?;
                    health_request = Some((id, tokio::time::Instant::now() + HEALTH_TIMEOUT));
                }
            }
        }
    }.await;
    let message = result
        .as_ref()
        .err()
        .map(ToString::to_string)
        .unwrap_or_else(|| "peer stopped".into());
    for (_, request) in pending {
        let state = request
            .delivery
            .compare_exchange(QUEUED, CANCELLED, Ordering::AcqRel, Ordering::Acquire)
            .unwrap_or_else(|state| state);
        request.fail(
            if state == WRITING {
                "unknown"
            } else {
                "not_sent"
            },
            message.clone(),
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn independent_routes_share_retained_event_budget() {
        let packet = serde_json::json!({"jsonrpc":"2.0","method":"item.completed","params":{"text":"content"}}).to_string();
        let budget = Arc::new(tokio::sync::Semaphore::new(packet.len()));
        let send = || async {
            let sink = futures_util::sink::drain().sink_map_err(|never| match never {});
            let source = futures_util::stream::iter([Ok(Packet::Text(packet.clone()))]);
            let (_sender, mut requests) = mpsc::channel(1);
            let (status, _) = watch::channel(Status {
                peer_id: "peer".into(),
                route_id: uuid::Uuid::new_v4().to_string(),
                generation: 1,
                state: State::Online,
                description: None,
                worker_pid: None,
                error: None,
            });
            let (events, mut receiver) = broadcast::channel(1);
            let error = online(
                Box::pin(sink),
                Box::pin(source),
                &mut requests,
                &status,
                &events,
                Some(budget.clone()),
            )
            .await
            .unwrap_err();
            (receiver.try_recv().ok(), error)
        };
        let (first, _) = send().await;
        assert!(matches!(first, Some(Event::Frame { .. })));
        assert_eq!(budget.available_permits(), 0);
        let (second, error) = send().await;
        assert!(second.is_none());
        assert!(
            error
                .to_string()
                .contains("host event byte budget exhausted")
        );
        drop(first);
        assert_eq!(budget.available_permits(), packet.len());
        assert!(matches!(send().await.0, Some(Event::Frame { .. })));
    }

    #[tokio::test]
    async fn an_expired_queued_mutation_never_reaches_the_transport() {
        let calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let sent = calls.clone();
        let (release, blocked) = oneshot::channel::<()>();
        let sink = futures_util::sink::unfold(Some(blocked), move |mut blocked, packet: Packet| {
            let sent = sent.clone();
            async move {
                let Packet::Text(text) = packet else {
                    unreachable!()
                };
                let frame: Value = serde_json::from_str(&text).unwrap();
                sent.lock()
                    .unwrap()
                    .push(frame["id"].as_str().unwrap().to_owned());
                if let Some(blocked) = blocked.take() {
                    let _ = blocked.await;
                }
                Ok::<_, anyhow::Error>(blocked)
            }
        });
        let (responses, receiver) = mpsc::channel::<Packet>(16);
        let source = futures_util::stream::unfold(receiver, |mut receiver| async move {
            receiver.recv().await.map(|packet| (Ok(packet), receiver))
        });
        let (requests, mut queued) = mpsc::channel(16);
        let (status, _) = watch::channel(Status {
            peer_id: "a".into(),
            route_id: "test-route".into(),
            generation: 1,
            state: State::Online,
            description: None,
            worker_pid: None,
            error: None,
        });
        let (events, _) = broadcast::channel(16);
        let actor = tokio::spawn(async move {
            online(
                Box::pin(sink),
                Box::pin(source),
                &mut queued,
                &status,
                &events,
                None,
            )
            .await
        });
        async fn submit(
            requests: &mpsc::Sender<Request>,
            id: &str,
            budget: Duration,
        ) -> oneshot::Receiver<Result<Value, Failure>> {
            let (reply, result) = oneshot::channel();
            let permit = Arc::new(tokio::sync::Semaphore::new(1))
                .acquire_owned()
                .await
                .unwrap();
            requests.send(Request {
                id: id.into(), generation: 1, packet: Some(Packet::Text(serde_json::json!({"jsonrpc":"2.0","id":id,"method":"turn.start","params":{}}).to_string())),
                deadline: tokio::time::Instant::now() + budget, reply,
                _budget: Arc::new(permit), delivery: Arc::new(AtomicU8::new(QUEUED)),
            }).await.unwrap();
            result
        }
        let first = submit(&requests, "first", Duration::from_secs(5)).await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while calls.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let expired = submit(&requests, "expired", Duration::from_millis(30)).await;
        assert_eq!(expired.await.unwrap().unwrap_err().outcome, "not_sent");
        release.send(()).unwrap();
        let third = submit(&requests, "third", Duration::from_secs(5)).await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while calls.lock().unwrap().len() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(*calls.lock().unwrap(), ["first", "third"]);
        for id in ["first", "third"] {
            responses
                .send(Packet::Text(
                    serde_json::json!({"jsonrpc":"2.0","id":id,"result":{}}).to_string(),
                ))
                .await
                .unwrap();
        }
        assert!(first.await.unwrap().is_ok());
        assert!(third.await.unwrap().is_ok());
        actor.abort();
    }
}
