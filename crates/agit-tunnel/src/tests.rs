use super::*;
use futures_util::{SinkExt, StreamExt};
use protocol::*;
use tokio::io::AsyncWriteExt;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn record_read_cancellation_preserves_partial_input() {
    let (mut writer, input) = tokio::io::duplex(128);
    let mut input = Reader::new(BufReader::new(input), 128);
    writer.write_all(b"part").await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(10), input.record())
            .await
            .is_err()
    );
    writer.write_all(b"ial\nnext\n").await.unwrap();
    assert_eq!(input.record().await.unwrap().unwrap(), b"partial\n");
    assert_eq!(input.record().await.unwrap().unwrap(), b"next\n");
}

#[tokio::test]
async fn record_limits_reject_unterminated_or_truncated_input() {
    for bytes in [
        b"too-long-without-newline".as_slice(),
        b"partial".as_slice(),
    ] {
        let mut input = Reader::new(BufReader::new(bytes), 8);
        assert!(input.record().await.is_err());
    }
}

#[test]
fn ssh_configuration_cannot_inject_options_or_shell_expansions() {
    assert!(
        Config::Ssh {
            host: "-oProxyCommand=bad".into(),
            command: vec!["cat".into()]
        }
        .validate()
        .is_err()
    );
    assert_eq!(worker::quote("/a'b/$(test)"), "'/a'\\''b/$(test)'");
}

#[tokio::test]
async fn websocket_worker_preserves_messages_and_write_receipts() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
        assert_eq!(
            socket.next().await.unwrap().unwrap(),
            Message::Text("request".into())
        );
        socket
            .send(Message::Binary(vec![0, 255, 42].into()))
            .await
            .unwrap();
        socket.next().await;
    });
    let connection = Connection::in_process(Config::WebSocket {
        url: format!("ws://{address}"),
        headers: vec![],
    })
    .await
    .unwrap();
    let (mut sink, mut source) = connection.split();
    sink.send(Packet::Text("request".into())).await.unwrap();
    assert_eq!(
        source.next().await.unwrap().unwrap(),
        Packet::Binary(vec![0, 255, 42])
    );
    drop(sink);
    drop(source);
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn congested_receiver_cannot_turn_a_missing_write_ack_into_success() {
    let (client, server) = tokio::io::duplex(65536);
    let (input, output) = tokio::io::split(client);
    let (worker_input, mut worker_output) = tokio::io::split(server);
    let task = tokio::spawn(async move {
        let mut input = Reader::new(BufReader::new(worker_input), MAX_RECORD);
        input.read::<Command>().await.unwrap();
        write(
            &mut worker_output,
            &Event::Connected {
                version: VERSION,
                worker_pid: 1,
            },
        )
        .await
        .unwrap();
        input.read::<Command>().await.unwrap();
        for _ in 0..=QUEUE_CAP {
            if write(
                &mut worker_output,
                &Event::Received {
                    packet: Packet::Text("event".into()),
                },
            )
            .await
            .is_err()
            {
                break;
            }
        }
    });
    let owner = Arc::new(Owner {
        stop: None,
        tasks: Mutex::new(vec![task.abort_handle()]),
        failure: Default::default(),
    });
    let connection = Connection::attach(
        Config::Ssh {
            host: "test".into(),
            command: vec!["cat".into()],
        },
        BufReader::new(input),
        output,
        owner,
    )
    .await
    .unwrap();
    let (mut sink, _source) = connection.split();
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        sink.send(Packet::Text("request".into())),
    )
    .await
    .unwrap();
    assert!(result.unwrap_err().to_string().contains("unknown"));
}
