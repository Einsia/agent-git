#![cfg(all(unix, feature = "worker"))]

use agit_tunnel::{Config, Connection, Packet};
use futures_util::{SinkExt, StreamExt};
use std::{path::Path, time::Duration};
use tokio_tungstenite::tungstenite::Message;

async fn echo() -> (Config, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
        while let Some(Ok(message)) = socket.next().await {
            if !matches!(message, Message::Text(_) | Message::Binary(_)) {
                break;
            }
            if socket.send(message).await.is_err() {
                break;
            }
        }
    });
    (
        Config::WebSocket {
            url: format!("ws://{address}"),
            headers: vec![],
            direct: false,
        },
        task,
    )
}

async fn reaped(pid: u32) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while unsafe { libc::kill(pid as i32, 0) } == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("worker must be reaped");
}

#[tokio::test]
async fn killing_one_worker_preserves_another_connection_and_reaps_both() {
    let (first, first_server) = echo().await;
    let (second, second_server) = echo().await;
    let executable = Path::new(env!("CARGO_BIN_EXE_agit-tunnel"));
    let first = Connection::open(first, executable, &[]).await.unwrap();
    let second = Connection::open(second, executable, &[]).await.unwrap();
    let first_pid = first.worker_pid;
    let second_pid = second.worker_pid;
    assert_ne!(first_pid, std::process::id());
    assert_ne!(first_pid, second_pid);
    let (first_sink, mut first_source) = first.split();
    let (mut second_sink, mut second_source) = second.split();
    assert_eq!(unsafe { libc::kill(first_pid as i32, libc::SIGKILL) }, 0);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), first_source.next())
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    second_sink
        .send(Packet::Text("still alive".into()))
        .await
        .unwrap();
    assert_eq!(
        second_source.next().await.unwrap().unwrap(),
        Packet::Text("still alive".into())
    );
    drop((first_sink, first_source, second_sink, second_source));
    reaped(first_pid).await;
    reaped(second_pid).await;
    tokio::time::timeout(Duration::from_secs(5), first_server)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), second_server)
        .await
        .unwrap()
        .unwrap();
}
