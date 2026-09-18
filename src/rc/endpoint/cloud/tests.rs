use super::*;
use crate::protocol::{ErrorCode, Frame};
use agit_peer::{
    Identity,
    access::{Access, Policy, Principal, Resource, Rule},
    cloud::{ConnectionGrant, Device},
    transport::framed,
};
use serde_json::json;
use std::time::Duration;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};

async fn receive(source: &mut agit_tunnel::PacketSource) -> Frame {
    let packet = tokio::time::timeout(Duration::from_secs(5), source.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let Packet::Text(text) = packet else {
        panic!("expected an RPC frame")
    };
    serde_json::from_str(&text).unwrap()
}

#[tokio::test]
async fn encrypted_cloud_ingress_filters_fanout_and_cannot_break_owner_rpc() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("owner.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let principal = Principal {
        issuer: "https://cloud.example".into(),
        account_id: "operator".into(),
    };
    let policy = Policy::new(
        1,
        vec![Rule {
            principal: principal.clone(),
            resource: Resource::Session("visible".into()),
            access: Access::Control,
        }],
    )
    .unwrap();
    let (admission, incoming) = mpsc::channel(1);
    let ingress = Ingress::fixed(incoming, ingress::Registry::fixed(policy));
    let (out, outbound) = crate::rc::outbound::channel();
    let (events, mut requests) = mpsc::channel(16);
    let server = tokio::spawn(super::super::serve_described(
        listener,
        outbound,
        events,
        json!({"authority":"local-owner", "diagnostic_log":"private-path"}),
        crate::rc::peers::controller().unwrap(),
        None,
        Some(ingress),
        super::super::Admission::default(),
    ));
    let source_identity = Identity::generate().unwrap();
    let target_identity = Identity::generate().unwrap();
    let device = |id: &str, identity: &Identity| Device {
        id: id.into(),
        machine_id: id.into(),
        display_name: id.into(),
        owner: principal.clone(),
        certificate: identity.certificate().clone(),
        credential_epoch: 1,
    };
    let grant = ConnectionGrant {
        id: "test-grant".into(),
        caller: principal.clone(),
        source: device("source", &source_identity),
        target: device("target", &target_identity),
        expires_at_ms: i64::MAX,
        session_controller: None,
        project_controller: None,
    };
    let (source, target) = tokio::io::duplex(65536);
    let (source, target) = tokio::join!(
        source_identity.connect(target_identity.certificate(), source),
        target_identity.accept(source_identity.certificate(), target),
    );
    let (mut sink, mut source) = framed(0, source.unwrap()).split();
    let (_lifetime, stopped) = watch::channel(());
    assert!(
        admission
            .send(host::Authenticated {
                connection: framed(0, target.unwrap()),
                grant,
                stopped,
                renewal: None,
            })
            .await
            .is_ok()
    );

    let describe = Frame::request("machine.describe", json!({}));
    sink.send(Packet::Text(describe.to_json())).await.unwrap();
    let description = receive(&mut source).await.result.unwrap();
    assert_eq!(description["authority"], "cloud-principal");
    assert!(description.get("diagnostic_log").is_none());

    let list = Frame::request(
        "session.list",
        json!({"workspace_id":"local-owner","include_local":true}),
    );
    sink.send(Packet::Text(list.to_json())).await.unwrap();
    let Some(crate::rc::link::LinkEvent::Frame { frame, .. }) = requests.recv().await else {
        panic!("missing catalog request")
    };
    let claim = frame.caller.as_ref().unwrap();
    assert_eq!(claim.role, "viewer");
    assert!(
        claim
            .account_id
            .as_ref()
            .unwrap()
            .contains("https://cloud.example")
    );
    out.send(Frame::response(
        frame.id.unwrap(),
        json!({"sessions":[], "local":[
            {"runtime_session_id":"visible","runtime":"codex","cwd":"/allowed"},
            {"runtime_session_id":"hidden","runtime":"codex","cwd":"/private"},
        ]}),
    ));
    let catalog = receive(&mut source).await.result.unwrap();
    assert_eq!(catalog["local"].as_array().unwrap().len(), 1);
    assert_eq!(catalog["local"][0]["runtime_session_id"], "visible");

    for session in ["hidden", "visible"] {
        let mut event = Frame::notification("item.completed", json!({"session":session}));
        event.stream = Some(session.into());
        event.seq = Some(1);
        out.send(event);
    }
    assert_eq!(
        receive(&mut source).await.stream.as_deref(),
        Some("visible")
    );

    let mut owner = BufReader::new(UnixStream::connect(&path).await.unwrap());
    owner
        .get_mut()
        .write_all(format!("{}\n", describe.to_json()).as_bytes())
        .await
        .unwrap();
    let mut response = String::new();
    owner.read_line(&mut response).await.unwrap();

    let subscribe = Frame::request("session.subscribe", json!({"session_id":"visible"}));
    sink.send(Packet::Text(subscribe.to_json())).await.unwrap();
    let Some(crate::rc::link::LinkEvent::Frame { frame, .. }) = requests.recv().await else {
        panic!("missing subscription request")
    };
    let replay_count = super::super::CLIENT_QUEUE * 2;
    let frames = (1..=replay_count)
        .map(|seq| {
            let mut event =
                Frame::notification("item.delta", json!({"text":"x".repeat(40 * 1024)}));
            event.stream = Some("visible".into());
            event.seq = Some(seq as u64);
            event
        })
        .collect();
    let slots = Arc::new(tokio::sync::Semaphore::new(1));
    out.send_replay_response(
        Frame::response(frame.id.unwrap(), json!({})),
        frames,
        slots.clone().try_acquire_owned().unwrap(),
    );
    let mut live = Frame::notification("item.completed", json!({}));
    live.stream = Some("visible".into());
    live.seq = Some(replay_count as u64 + 1);
    out.send(live);
    // A stalled Cloud replay must neither reach nor block another endpoint.
    response.clear();
    tokio::time::timeout(Duration::from_secs(5), owner.read_line(&mut response))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Frame>(&response).unwrap().seq,
        Some(replay_count as u64 + 1)
    );
    assert_eq!(slots.available_permits(), 0);
    assert_eq!(receive(&mut source).await.id, subscribe.id);
    sink.send(Packet::Text(describe.to_json())).await.unwrap();
    let mut next_seq = 1;
    let mut replied = false;
    while next_seq <= replay_count + 1 || !replied {
        let frame = receive(&mut source).await;
        if frame.id == describe.id {
            assert!(
                next_seq <= replay_count,
                "RPC receipt waited behind the entire replay"
            );
            replied = true;
        } else {
            assert_eq!(frame.seq, Some(next_seq as u64));
            next_seq += 1;
        }
    }
    assert_eq!(slots.available_permits(), 1);

    for (method, params) in [
        ("session.history", json!({"session_id":"hidden"})),
        ("peer.list", json!({})),
    ] {
        sink.send(Packet::Text(Frame::request(method, params).to_json()))
            .await
            .unwrap();
        assert!(
            receive(&mut source)
                .await
                .error
                .unwrap()
                .is(ErrorCode::Forbidden)
        );
        assert!(requests.try_recv().is_err());
    }
    let turn = Frame::request(
        "turn.start",
        json!({"session_id":"visible","client_msg_id":"intent","message":"hello"}),
    );
    sink.send(Packet::Text(turn.to_json())).await.unwrap();
    let Some(crate::rc::link::LinkEvent::Frame { frame, .. }) = requests.recv().await else {
        panic!("missing control request")
    };
    assert_eq!(frame.caller.unwrap().role, "operator");
    sink.send(Packet::Text(turn.to_json())).await.unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(5), source.next())
        .await
        .unwrap();
    assert!(closed.is_none() || closed.unwrap().is_err());
    assert!(requests.try_recv().is_err());

    owner
        .get_mut()
        .write_all(format!("{}\n", describe.to_json()).as_bytes())
        .await
        .unwrap();
    response.clear();
    tokio::time::timeout(Duration::from_secs(5), owner.read_line(&mut response))
        .await
        .unwrap()
        .unwrap();
    let response: Frame = serde_json::from_str(&response).unwrap();
    assert_eq!(response.result.unwrap()["authority"], "local-owner");
    assert!(!server.is_finished());
    server.abort();
}

#[tokio::test]
async fn exhausted_cloud_output_closes_only_its_client_without_waiting() {
    let (stop, mut stopped) = watch::channel(());
    let (output, _receiver) = ClientOutput::channel(Some(stop));
    for _ in 0..super::super::CLIENT_QUEUE {
        output.try_send("first".into()).unwrap();
    }
    assert!(
        tokio::time::timeout(
            Duration::from_millis(100),
            output.send_timeout("second".into(), Duration::from_secs(30))
        )
        .await
        .unwrap()
        .is_err()
    );
    stopped.changed().await.unwrap();
}

#[tokio::test]
async fn renewal_recovers_transport_failure_but_refusal_revokes_before_queued_cleanup() {
    use crate::rc::ticket::ticket_authorized;
    use agit_peer::cloud::{DeviceCredential, Secret};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let (refused, refusal) = tokio::sync::oneshot::channel();
    let (recovered, recovery) = tokio::sync::oneshot::channel();
    let (allow_refusal, allowed) = tokio::sync::oneshot::channel();
    let principal = Principal {
        issuer: origin.clone(),
        account_id: "operator".into(),
    };
    let identity = Identity::generate().unwrap();
    let device = Device {
        id: "executor".into(),
        machine_id: "executor".into(),
        display_name: "executor".into(),
        owner: principal.clone(),
        certificate: identity.certificate().clone(),
        credential_epoch: 1,
    };
    let grant = ConnectionGrant {
        id: "test-grant".into(),
        caller: principal.clone(),
        source: device.clone(),
        target: device.clone(),
        expires_at_ms: chrono::Utc::now().timestamp_millis() + 2_000,
        session_controller: None,
        project_controller: None,
    };
    let renewed_grant = ConnectionGrant {
        expires_at_ms: grant.expires_at_ms + 2_000,
        ..grant.clone()
    };
    let expected_expiry = renewed_grant.expires_at_ms;
    let server = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        for attempt in 0..3 {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = BufReader::new(socket);
            let mut line = String::new();
            socket.read_line(&mut line).await.unwrap();
            assert!(line.starts_with("POST /api/peer/grants/renew "));
            let mut length = 0;
            loop {
                line.clear();
                socket.read_line(&mut line).await.unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
            }
            socket.read_exact(&mut vec![0; length]).await.unwrap();
            if attempt == 0 {
                continue;
            }
            if attempt == 1 {
                let body = serde_json::to_vec(&renewed_grant).unwrap();
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
                socket.write_all(&body).await.unwrap();
            } else {
                recovered.send(()).unwrap();
                allowed.await.unwrap();
                socket
                    .write_all(
                        b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .unwrap();
                refused.send(()).unwrap();
                break;
            }
        }
    });
    let registry = ingress::Registry::fixed(Policy::default());
    let guard = registry.client(principal, grant.expires_at_ms);
    let frame = guard
        .authorize(Frame::request("session.list", json!({})))
        .unwrap();
    let authority = frame.authority.clone();
    assert!(authority.admit(|| true));
    let (ticket, _receipt) = ticket_authorized::<()>(authority.clone());
    let lease = guard.lease();
    let (input, _pending) = mpsc::channel(1);
    assert!(input.try_send(Incoming::Closed(99)).is_ok());
    let (_peer, executor) = tokio::io::duplex(1024);
    let (_lifetime, stopped) = watch::channel(());
    let client = attach(
        host::Authenticated {
            connection: framed(0, executor),
            grant,
            stopped,
            renewal: Some(host::Renewal {
                api: agit_peer::client::Client::new(&origin).unwrap(),
                credential: DeviceCredential {
                    device,
                    token: Secret::new("test-device-token".into()),
                },
                token: Secret::new("test-grant-token".into()),
            }),
        },
        guard,
        1,
        input,
        None,
    );
    tokio::time::timeout(Duration::from_secs(5), recovery)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(lease.expires_at_ms(), expected_expiry);
    assert!(authority.admit(|| true));
    allow_refusal.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), refusal)
        .await
        .expect("executor must request renewal")
        .unwrap();
    tokio::time::timeout(Duration::from_millis(500), async {
        while authority.admit(|| true) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("renewal refusal must revoke authority before waiting for input capacity");
    assert!(lease.deadline() > tokio::time::Instant::now());
    assert!(!client.task.is_finished());
    assert!(!ticket.accept());
    assert!(
        client
            .cloud
            .as_ref()
            .unwrap()
            .authorize(Frame::request("session.list", json!({})))
            .is_err()
    );
    server.await.unwrap();
}
