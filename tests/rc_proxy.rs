#![cfg(feature = "rc")]

use agit::protocol::{Frame, RcRegister, RcRegisterResult, VERSION, method};
use agit::rc::link::{Link, LinkEvent};
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::{Message, handshake::client::Request};

const TEST_URL: &str = "AGIT_PROXY_TEST_URL";
const REGISTERED: &str = "proxy.test.registered";

fn registration() -> RcRegister {
    RcRegister {
        protocol_version: VERSION,
        machine_fingerprint: "proxy-test".into(),
        display_name: "proxy-test".into(),
        agit_version: "test".into(),
        platform: "test".into(),
        capabilities: vec![],
        features: vec![],
        workspaces: vec![],
        last_seq: Default::default(),
    }
}

// Environment selection runs in a child so concurrent tests cannot inherit proxy changes.
#[test]
fn rc_link_child() {
    let Ok(url) = std::env::var(TEST_URL) else {
        return;
    };
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let link = Link::new(&url, "rc-test-token");
        let (outbound_tx, mut outbound) = agit::rc::outbound::channel();
        for epoch in 1..=2 {
            let (events, mut receiver) = tokio::sync::mpsc::channel(4);
            let reason = tokio::time::timeout(
                Duration::from_secs(10),
                link.run_once(epoch, registration(), &mut outbound, events, |_| {
                    assert_eq!(
                        outbound_tx.send(Frame::notification(
                            REGISTERED,
                            serde_json::json!({ "epoch": epoch }),
                        )),
                        agit::rc::outbound::Sent::Queued
                    );
                }),
            )
            .await
            .expect("the connection attempt must complete");
            assert!(
                matches!(receiver.try_recv(), Ok(LinkEvent::Connected { epoch: e, .. }) if e == epoch),
                "registration failed: {reason}"
            );
        }
    });
}

#[tokio::test]
#[ignore = "requires AGIT_PROXY_TEST_LIVE_HUB and a working HTTP CONNECT proxy"]
async fn live_https_proxy_reaches_hub_authentication() {
    let hub = std::env::var("AGIT_PROXY_TEST_LIVE_HUB").expect("set the HTTPS hub to probe");
    assert!(hub.starts_with("https://"));
    assert!(
        std::env::var_os("HTTPS_PROXY")
            .or_else(|| std::env::var_os("https_proxy"))
            .is_some()
    );
    let link = Link::new(&hub, "invalid-proxy-probe-token");
    let (_outbound_tx, mut outbound) = agit::rc::outbound::channel();
    let (events, _receiver) = tokio::sync::mpsc::channel(4);
    let reason = tokio::time::timeout(
        Duration::from_secs(15),
        link.run_once(1, registration(), &mut outbound, events, |_| {
            panic!("an invalid token must not register a machine");
        }),
    )
    .await
    .unwrap();
    assert!(
        reason.contains("HTTP error: 401"),
        "TLS must reach the hub's authentication boundary: {reason}"
    );
}

#[tokio::test]
#[allow(
    clippy::result_large_err,
    reason = "tungstenite fixes the handshake callback error type"
)]
async fn environment_proxy_selection_registers_and_reconnects() {
    for (variable, no_proxy, bind) in [
        ("HTTP_PROXY", None, "127.0.0.1:0"),
        ("http_proxy", None, "127.0.0.1:0"),
        ("ALL_PROXY", None, "127.0.0.1:0"),
        ("all_proxy", None, "127.0.0.1:0"),
        ("HTTP_PROXY", Some(("NO_PROXY", "127.0.0.1")), "127.0.0.1:0"),
        ("HTTP_PROXY", Some(("NO_PROXY", "*")), "127.0.0.1:0"),
        (
            "HTTP_PROXY",
            Some(("NO_PROXY", "ignored.test, *, other.test")),
            "[::1]:0",
        ),
        ("HTTP_PROXY", Some(("no_proxy", "*")), "127.0.0.1:0"),
    ] {
        let bypass = no_proxy.is_some();
        let listener = TcpListener::bind(bind).await.unwrap();
        let address = listener.local_addr().unwrap();
        let host = if bypass {
            address.to_string()
        } else {
            "unresolvable.invalid:8765".to_owned()
        };
        let url = format!("http://{host}");
        let server = tokio::spawn(async move {
            for epoch in 1_u64..=2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                if !bypass {
                    let mut bytes = Vec::new();
                    while !bytes.ends_with(b"\r\n\r\n") {
                        assert!(bytes.len() < 8192);
                        bytes.push(stream.read_u8().await.unwrap());
                    }
                    let connect = String::from_utf8(bytes).unwrap();
                    assert!(connect.starts_with(&format!("CONNECT {host} HTTP/1.1\r\n")));
                    assert!(!connect.contains("rc-test-token"));
                    stream
                        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                        .await
                        .unwrap();
                }
                let mut socket =
                    tokio_tungstenite::accept_hdr_async(stream, |request: &Request, response| {
                        assert_eq!(request.headers()["host"], host);
                        assert_eq!(request.headers()["authorization"], "Bearer rc-test-token");
                        assert!(request.headers().contains_key("user-agent"));
                        assert!(request.headers().contains_key("x-agit-protocol"));
                        assert!(!request.headers().contains_key("proxy-authorization"));
                        Ok(response)
                    })
                    .await
                    .unwrap();
                let message = socket.next().await.unwrap().unwrap();
                let frame = Frame::from_json(message.to_text().unwrap()).unwrap();
                assert_eq!(frame.method.as_deref(), Some(method::RC_REGISTER));
                let reply = Frame::response(
                    frame.id.unwrap(),
                    RcRegisterResult {
                        connection_id: "proxy-test".into(),
                        accepted_features: vec![],
                        workspaces: vec![],
                        persisted_seq: Default::default(),
                        server_time: "2026-09-10T00:00:00Z".into(),
                    },
                );
                socket
                    .send(Message::Text(reply.to_json().into()))
                    .await
                    .unwrap();
                // Closing before acknowledgement can discard an in-flight registration response.
                tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        let message = socket
                            .next()
                            .await
                            .expect("the registered client must keep the connection open")
                            .expect("the registered client must send a valid frame");
                        match message {
                            Message::Text(text) => {
                                let frame = Frame::from_json(&text).unwrap();
                                assert!(frame.is_notification());
                                if frame.method.as_deref() == Some(REGISTERED) {
                                    assert_eq!(frame.params.unwrap()["epoch"], epoch);
                                    break;
                                }
                                assert_eq!(frame.method.as_deref(), Some(method::RC_HEARTBEAT));
                            }
                            Message::Ping(_) | Message::Pong(_) => {}
                            other => panic!("unexpected message before acknowledgement: {other:?}"),
                        }
                    }
                })
                .await
                .expect("the client must acknowledge its registration");
                socket.close(None).await.unwrap();
            }
        });
        let mut child = tokio::process::Command::new(std::env::current_exe().unwrap());
        child
            .args(["--exact", "rc_link_child", "--nocapture"])
            .kill_on_drop(true)
            .env(TEST_URL, url);
        for name in [
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
            "NO_PROXY",
            "no_proxy",
            "REQUEST_METHOD",
        ] {
            child.env_remove(name);
        }
        child.env(variable, format!("http://{address}"));
        if !cfg!(windows) && variable == "HTTP_PROXY" {
            child.env("http_proxy", "http://127.0.0.1:9");
        }
        if let Some((name, value)) = no_proxy {
            child.env(variable, "http://127.0.0.1:9").env(name, value);
            if !cfg!(windows) && name == "NO_PROXY" {
                child.env("no_proxy", "unrelated.test");
            }
        }
        let output = tokio::time::timeout(Duration::from_secs(15), child.output())
            .await
            .unwrap()
            .unwrap();
        assert!(
            output.status.success(),
            "{variable}, bypass={bypass}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
    }
}
