use super::*;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

type Socket = WebSocketStream<tokio::net::TcpStream>;
const WAIT: Duration = Duration::from_secs(5);
fn controller() -> Arc<Controller> {
    Arc::new(Controller::new(Worker {
        executable: PathBuf::new(),
        args: vec![],
    }))
}
async fn endpoint() -> (Config, mpsc::Receiver<Socket>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel(8);
    let task = tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let socket = tokio_tungstenite::accept_async(socket).await.unwrap();
            if tx.send(socket).await.is_err() {
                break;
            }
        }
    });
    (
        Config::WebSocket {
            url,
            headers: vec![],
            direct: false,
        },
        rx,
        task,
    )
}
async fn read(socket: &mut Socket) -> Value {
    let message = tokio::time::timeout(WAIT, socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    serde_json::from_str(message.to_text().unwrap()).unwrap()
}
async fn response(socket: &mut Socket, id: &Value, result: Value) {
    socket
        .send(Message::Text(
            json!({"jsonrpc":"2.0","id":id,"result":result})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
}
async fn handshake(incoming: &mut mpsc::Receiver<Socket>, fingerprint: &str) -> Socket {
    let mut socket = tokio::time::timeout(WAIT, incoming.recv())
        .await
        .unwrap()
        .unwrap();
    let request = read(&mut socket).await;
    assert_eq!(request["method"], "machine.describe");
    response(&mut socket, &request["id"], json!({"protocol_version":1,"authority":"local-owner","instance_id":"instance","machine":{"machine_fingerprint":fingerprint}})).await;
    socket
}
fn request(
    controller: &Arc<Controller>,
    peer: &str,
    value: &str,
    timeout: Duration,
) -> tokio::task::JoinHandle<Result<Value, Failure>> {
    let controller = controller.clone();
    let peer = peer.to_owned();
    let value = value.to_owned();
    tokio::spawn(async move {
        controller
            .request(&peer, "turn.start".into(), json!({"input":value}), timeout)
            .await
    })
}

#[tokio::test]
async fn responses_correlate_out_of_order_and_events_keep_peer_namespaces() {
    let controller = controller();
    let (config, mut incoming, server) = endpoint().await;
    controller
        .connect("a".into(), config.clone(), None)
        .unwrap();
    let mut a = handshake(&mut incoming, "same-host").await;
    controller.connect("b".into(), config, None).unwrap();
    let mut b = handshake(&mut incoming, "same-host").await;
    controller.ready("a", WAIT).await.unwrap();
    controller.ready("b", WAIT).await.unwrap();
    let mut events = controller.subscribe();
    let first = request(&controller, "a", "first", WAIT);
    let second = request(&controller, "a", "second", WAIT);
    let one = read(&mut a).await;
    let two = read(&mut a).await;
    assert_ne!(one["id"], two["id"]);
    response(&mut a, &two["id"], two["params"].clone()).await;
    response(&mut a, &one["id"], one["params"].clone()).await;
    assert_eq!(first.await.unwrap().unwrap()["result"]["input"], "first");
    assert_eq!(second.await.unwrap().unwrap()["result"]["input"], "second");
    let event =
        json!({"jsonrpc":"2.0","method":"item.delta","stream":"same-session","seq":1,"params":{}});
    a.send(Message::Text(event.to_string().into()))
        .await
        .unwrap();
    b.send(Message::Text(event.to_string().into()))
        .await
        .unwrap();
    let mut ids = Vec::new();
    for _ in 0..2 {
        let Event::Frame {
            peer_id,
            generation,
            frame,
            ..
        } = tokio::time::timeout(WAIT, events.recv())
            .await
            .unwrap()
            .unwrap()
        else {
            panic!("expected event")
        };
        assert_eq!(*frame, event);
        assert_eq!(generation, 1);
        ids.push(peer_id);
    }
    ids.sort();
    assert_eq!(ids, ["a", "b"]);
    drop(controller);
    server.abort();
}

#[tokio::test]
async fn lost_mutation_is_unknown_and_reconnect_does_not_replay_it() {
    let controller = controller();
    let (config, mut incoming, server) = endpoint().await;
    controller
        .connect("a".into(), config.clone(), None)
        .unwrap();
    let mut socket = handshake(&mut incoming, "host").await;
    controller.ready("a", WAIT).await.unwrap();
    let operation = request(&controller, "a", "execute once", WAIT);
    let wire = read(&mut socket).await;
    assert_eq!(wire["method"], "turn.start");
    drop(socket);
    let failure = operation.await.unwrap().unwrap_err();
    assert_eq!(failure.outcome, "unknown");
    assert!(
        wire["id"]
            .as_str()
            .unwrap()
            .starts_with(&format!("{}:", failure.operation_id))
    );
    let mut socket = handshake(&mut incoming, "host").await;
    let status = controller.ready("a", WAIT).await.unwrap();
    assert_eq!(status.generation, 2);
    controller
        .connect("a".into(), config, Some("host".into()))
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), socket.next())
            .await
            .is_err()
    );
    drop(controller);
    server.abort();
}

#[tokio::test]
async fn timeout_on_one_peer_does_not_delay_another_peer() {
    let controller = controller();
    let (a, mut incoming_a, server_a) = endpoint().await;
    let (b, mut incoming_b, server_b) = endpoint().await;
    controller.connect("a".into(), a, None).unwrap();
    controller.connect("b".into(), b, None).unwrap();
    let mut a = handshake(&mut incoming_a, "a").await;
    let mut b = handshake(&mut incoming_b, "b").await;
    controller.ready("a", WAIT).await.unwrap();
    controller.ready("b", WAIT).await.unwrap();
    let blocked = request(&controller, "a", "blocked", Duration::from_millis(100));
    let healthy = request(&controller, "b", "healthy", WAIT);
    read(&mut a).await;
    let wire = read(&mut b).await;
    response(&mut b, &wire["id"], json!({"ok":true})).await;
    assert_eq!(healthy.await.unwrap().unwrap()["result"]["ok"], true);
    assert_eq!(blocked.await.unwrap().unwrap_err().outcome, "unknown");
    assert!(matches!(
        controller.ready("b", WAIT).await.unwrap().state,
        State::Online
    ));
    drop(controller);
    server_a.abort();
    server_b.abort();
}

#[tokio::test]
async fn a_reconnected_transport_cannot_change_the_pinned_executor() {
    let controller = controller();
    let (config, mut incoming, server) = endpoint().await;
    controller
        .connect("a".into(), config, Some("original".into()))
        .unwrap();
    let socket = handshake(&mut incoming, "original").await;
    controller.ready("a", WAIT).await.unwrap();
    drop(socket);
    let _replacement = handshake(&mut incoming, "replacement").await;
    assert!(
        controller
            .ready("a", WAIT)
            .await
            .unwrap_err()
            .to_string()
            .contains("fingerprint")
    );
    assert_eq!(
        request(&controller, "a", "do not execute", WAIT)
            .await
            .unwrap()
            .unwrap_err()
            .outcome,
        "not_sent"
    );
    drop(controller);
    server.abort();
}

#[tokio::test]
async fn dropping_the_controller_closes_transport_without_a_ui_owner() {
    let controller = controller();
    let (config, mut incoming, server) = endpoint().await;
    controller.connect("a".into(), config, None).unwrap();
    let mut socket = handshake(&mut incoming, "host").await;
    controller.ready("a", WAIT).await.unwrap();
    drop(controller);
    let end = tokio::time::timeout(WAIT, socket.next()).await.unwrap();
    assert!(end.is_none() || end.unwrap().is_err());
    server.abort();
}

#[tokio::test]
async fn read_requests_retry_once_after_reconnect_with_a_fresh_wire_identity() {
    let controller = controller();
    let (config, mut incoming, server) = endpoint().await;
    controller.connect("a".into(), config, None).unwrap();
    let mut socket = handshake(&mut incoming, "host").await;
    controller.ready("a", WAIT).await.unwrap();
    let client = controller.clone();
    let read_request = tokio::spawn(async move {
        client
            .request("a", "session.list".into(), json!({}), WAIT)
            .await
    });
    let first = read(&mut socket).await;
    drop(socket);
    let mut socket = handshake(&mut incoming, "host").await;
    let retry = read(&mut socket).await;
    assert_eq!(retry["method"], first["method"]);
    assert_eq!(retry["params"], first["params"]);
    assert_ne!(retry["id"], first["id"]);
    response(&mut socket, &retry["id"], json!({"sessions":[]})).await;
    assert_eq!(
        read_request.await.unwrap().unwrap()["result"],
        json!({"sessions":[]})
    );
    drop(controller);
    server.abort();
}

#[tokio::test]
async fn a_stalled_rpc_does_not_disconnect_other_sessions_on_the_same_peer() {
    let controller = controller();
    let (config, mut incoming, server) = endpoint().await;
    controller.connect("a".into(), config, None).unwrap();
    let mut socket = handshake(&mut incoming, "host").await;
    controller.ready("a", WAIT).await.unwrap();
    let blocked = request(
        &controller,
        "a",
        "blocked session",
        Duration::from_millis(100),
    );
    let stale = read(&mut socket).await;
    assert_eq!(blocked.await.unwrap().unwrap_err().outcome, "unknown");
    let healthy = request(&controller, "a", "other session", WAIT);
    let fresh = read(&mut socket).await;
    response(&mut socket, &stale["id"], json!({"stale":true})).await;
    response(&mut socket, &fresh["id"], json!({"healthy":true})).await;
    assert_eq!(
        healthy.await.unwrap().unwrap()["result"],
        json!({"healthy":true})
    );
    assert_eq!(controller.ready("a", WAIT).await.unwrap().generation, 1);
    drop(controller);
    server.abort();
}

#[tokio::test]
async fn a_cached_target_cannot_write_after_reconnect_or_route_replacement() {
    let controller = controller();
    let (config, mut incoming, server) = endpoint().await;
    controller
        .connect("a".into(), config.clone(), None)
        .unwrap();
    let socket = handshake(&mut incoming, "host").await;
    let old = controller.ready("a", WAIT).await.unwrap().target();
    drop(socket);
    let mut socket = handshake(&mut incoming, "host").await;
    let current = controller.ready("a", WAIT).await.unwrap().target();
    assert_eq!(old.route_id, current.route_id);
    assert!(current.generation > old.generation);
    assert_eq!(
        controller
            .request_at(old.clone(), "turn.start".into(), json!({}), WAIT)
            .await
            .unwrap_err()
            .outcome,
        "not_sent"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(30), socket.next())
            .await
            .is_err()
    );
    controller.disconnect("a");
    controller.connect("a".into(), config, None).unwrap();
    let mut replacement = handshake(&mut incoming, "host").await;
    let new = controller.ready("a", WAIT).await.unwrap().target();
    assert_ne!(new.route_id, old.route_id);
    assert_eq!(
        controller
            .request_at(old, "turn.start".into(), json!({}), WAIT)
            .await
            .unwrap_err()
            .outcome,
        "not_sent"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(30), replacement.next())
            .await
            .is_err()
    );
    drop(controller);
    server.abort();
}

#[tokio::test]
async fn malformed_errors_cannot_be_reported_as_successful_mutations() {
    let controller = controller();
    let (config, mut incoming, server) = endpoint().await;
    controller.connect("a".into(), config, None).unwrap();
    let mut socket = handshake(&mut incoming, "host").await;
    controller.ready("a", WAIT).await.unwrap();
    let operation = request(&controller, "a", "mutation", WAIT);
    let wire = read(&mut socket).await;
    socket
        .send(Message::Text(
            json!({"jsonrpc":"2.0","id":wire["id"],"error":null})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    assert_eq!(operation.await.unwrap().unwrap_err().outcome, "unknown");
    drop(controller);
    server.abort();
}

#[tokio::test]
async fn reconnect_invokes_the_route_again_without_reusing_admission_material() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Fresh {
        config: Config,
        attempts: AtomicUsize,
    }
    impl Connector for Fresh {
        fn key(&self) -> &str {
            "fresh-admission"
        }
        fn authority(&self) -> Authority {
            Authority::LocalOwner
        }
        fn open<'a>(&'a self, worker: &'a Worker) -> Opening<'a> {
            Box::pin(async move {
                let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
                ensure!(attempt >= 2, "admission is temporarily unavailable");
                worker.open(self.config.clone()).await
            })
        }
    }
    let controller = controller();
    let (config, mut incoming, server) = endpoint().await;
    let route = Arc::new(Fresh {
        config,
        attempts: AtomicUsize::new(0),
    });
    controller
        .connect_with("cloud".into(), route.clone(), None)
        .unwrap();
    let socket = handshake(&mut incoming, "target").await;
    assert_eq!(controller.ready("cloud", WAIT).await.unwrap().generation, 1);
    assert_eq!(route.attempts.load(Ordering::SeqCst), 3);
    drop(socket);
    let _socket = handshake(&mut incoming, "target").await;
    assert_eq!(controller.ready("cloud", WAIT).await.unwrap().generation, 2);
    assert_eq!(route.attempts.load(Ordering::SeqCst), 4);
    drop(controller);
    server.abort();
}
