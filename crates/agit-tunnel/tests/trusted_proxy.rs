#![cfg(feature = "worker")]

use agit_tunnel::{
    Config, Packet,
    protocol::{Command, Event, MAX_RECORD, Reader, VERSION, write},
};
use futures_util::{SinkExt, StreamExt};
use std::{process::Stdio, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};

#[tokio::test]
async fn explicit_direct_transport_bypasses_worker_environment_proxy() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_url = format!("http://{}", proxy.local_addr().unwrap());
        let proxy_server = tokio::spawn(async move {
            let (mut socket, _) = proxy.accept().await.unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                header.push(socket.read_u8().await.unwrap());
                assert!(header.len() < 8192);
            }
            assert!(header.starts_with(b"CONNECT "));
            socket
                .write_all(b"HTTP/1.1 403 Forbidden\r\n\r\n")
                .await
                .unwrap();
        });
        let relay = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay_url = format!("ws://{}", relay.local_addr().unwrap());
        let relay_server = tokio::spawn(async move {
            let (socket, _) = relay.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
            let message = socket.next().await.unwrap().unwrap();
            socket.send(message).await.unwrap();
            let _ = socket.next().await;
        });
        for direct in [false, true] {
            let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_agit-tunnel"));
            for key in [
                "HTTP_PROXY",
                "http_proxy",
                "HTTPS_PROXY",
                "https_proxy",
                "ALL_PROXY",
                "all_proxy",
            ] {
                command.env(key, &proxy_url);
            }
            let mut child = command
                .env("NO_PROXY", "")
                .env("no_proxy", "")
                .env_remove("AGIT_TUNNEL_CONNECT_TIMING")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let mut input = child.stdin.take().unwrap();
            let mut output = Reader::new(BufReader::new(child.stdout.take().unwrap()), MAX_RECORD);
            let config = Config::WebSocket {
                url: relay_url.clone(),
                headers: vec![],
                direct,
            };
            if !direct {
                let encoded = serde_json::to_value(&config).unwrap();
                assert!(
                    encoded.get("direct").is_none(),
                    "default worker messages retain their existing shape"
                );
            }
            write(
                &mut input,
                &Command::Open {
                    version: VERSION,
                    config,
                },
            )
            .await
            .unwrap();
            let wire = output.read::<serde_json::Value>().await.unwrap().unwrap();
            assert!(
                wire.get("timing").is_none(),
                "diagnostics require parent opt-in"
            );
            let event: Event = serde_json::from_value(wire).unwrap();
            if direct {
                assert!(
                    matches!(event, Event::Connected { timing: None, .. }),
                    "{event:?}"
                );
                let packet = Packet::Text("direct relay".into());
                write(
                    &mut input,
                    &Command::Send {
                        serial: 1,
                        packet: packet.clone(),
                    },
                )
                .await
                .unwrap();
                loop {
                    match output.read::<Event>().await.unwrap().unwrap() {
                        Event::Received { packet: response } => {
                            assert_eq!(response, packet);
                            break;
                        }
                        Event::Written { serial: 1 } => {}
                        event => panic!("unexpected worker event: {event:?}"),
                    }
                }
            } else {
                let Event::Failed { message } = event else {
                    panic!("default transport ignored its proxy");
                };
                assert!(message.contains("proxy"), "{message}");
            }
            drop(input);
            child.wait().await.unwrap();
        }
        proxy_server.await.unwrap();
        relay_server.await.unwrap();
    })
    .await
    .expect("worker proxy routing must complete");
}
