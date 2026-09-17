use super::*;
use crate::transport::{Role, authenticate};
use tokio::sync::mpsc;

fn connection(outgoing: mpsc::Sender<Packet>, incoming: mpsc::Receiver<Packet>) -> Connection {
    let sink = futures_util::sink::unfold(outgoing, |sender, packet| async move {
        sender.send(packet).await?;
        Ok::<_, anyhow::Error>(sender)
    });
    let source = futures_util::stream::unfold(incoming, |mut incoming| async {
        incoming.recv().await.map(|packet| (Ok(packet), incoming))
    });
    Connection::from_parts(41, Box::pin(sink), Box::pin(source))
}

/// ClientHello must precede readiness without consuming the post-pairing TLS allowance.
#[tokio::test(start_paused = true)]
async fn pipelined_tls_preserves_delayed_pairing_and_rejects_another_link() {
    let controller = Identity::generate().unwrap();
    let executor = Identity::generate().unwrap();
    for ready_link in ["expected", "unexpected"] {
        let (left_tx, left_rx) = mpsc::channel(4);
        let (right_tx, right_rx) = mpsc::channel(4);
        let left = connection(left_tx, right_rx);
        let right = connection(right_tx, left_rx);
        let relay = async {
            let (mut sink, mut source) = right.split();
            let Packet::Text(join) = source.next().await.unwrap().unwrap() else {
                panic!("relay join must precede TLS bytes")
            };
            let join: DataJoin = serde_json::from_str(&join).unwrap();
            assert_eq!(join.ticket.expose(), "ticket");
            let first = tokio::time::timeout(Duration::from_secs(1), source.next())
                .await
                .expect("ClientHello waited for relay readiness")
                .unwrap()
                .unwrap();
            assert!(matches!(first, Packet::Binary(_)));
            tokio::time::sleep(Duration::from_secs(20)).await;
            sink.send(Packet::Text(
                serde_json::to_string(&DataReady {
                    link_id: ready_link.into(),
                })
                .unwrap(),
            ))
            .await
            .unwrap();
            let source = futures_util::stream::once(async { Ok(first) }).chain(source);
            authenticate(
                Connection::from_parts(41, sink, Box::pin(source)),
                &executor,
                controller.certificate(),
                Role::Executor,
            )
            .await
        };
        let (left, right) = tokio::join!(
            join_controller(
                left,
                "expected",
                Secret::new("ticket".into()),
                &controller,
                executor.certificate(),
            ),
            relay
        );
        if ready_link != "expected" {
            assert!(left.is_err());
            assert!(right.is_err());
            continue;
        }
        let (left, pairing) = left.unwrap();
        assert!(pairing >= Duration::from_secs(20));
        assert!(pairing < REQUEST_TIMEOUT);
        assert_eq!(left.worker_pid, 41);
        let (mut sender, _receiver) = left.split();
        let (_sender, mut receiver) = right.unwrap().split();
        let frame = Packet::Text(serde_json::json!({"private":"payload"}).to_string());
        sender.send(frame.clone()).await.unwrap();
        assert_eq!(receiver.next().await.unwrap().unwrap(), frame);
    }
}
