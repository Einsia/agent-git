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

    let mut owner = BufReader::new(UnixStream::connect(&path).await.unwrap());
    owner
        .get_mut()
        .write_all(format!("{}\n", describe.to_json()).as_bytes())
        .await
        .unwrap();
    let mut response = String::new();
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
    let (sender, _receiver) = mpsc::channel(1);
    let (stop, mut stopped) = watch::channel(());
    let output = ClientOutput {
        sender,
        bytes: Arc::new(tokio::sync::Semaphore::new(MAX_FRAME)),
        stop: Some(stop),
    };
    output.try_send("first".into()).unwrap();
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
