use super::*;
use std::sync::{Arc, Mutex};

fn endpoints() -> (Connection, Connection, Arc<Mutex<Vec<u8>>>) {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let (left, left_rx) = mpsc::channel(2);
    let (right, right_rx) = mpsc::channel(2);
    let connection = |outgoing: mpsc::Sender<Packet>, incoming: mpsc::Receiver<Packet>| {
        let observed = observed.clone();
        let sink = futures_util::sink::unfold(
            (outgoing, observed),
            |(sender, observed), packet| async move {
                let Packet::Binary(bytes) = &packet else {
                    panic!("the relay must only observe ciphertext")
                };
                observed.lock().unwrap().extend_from_slice(bytes);
                sender.send(packet).await?;
                Ok::<_, anyhow::Error>((sender, observed))
            },
        );
        let source = futures_util::stream::unfold(incoming, |mut incoming| async {
            incoming.recv().await.map(|packet| (Ok(packet), incoming))
        });
        Connection::from_parts(41, Box::pin(sink), Box::pin(source))
    };
    (
        connection(left, right_rx),
        connection(right, left_rx),
        observed,
    )
}

#[tokio::test]
async fn encrypted_tunnel_chunks_carry_bidirectional_frames_without_exposing_rpc_payloads() {
    let (left, right, observed) = endpoints();
    let alice = Identity::generate().unwrap();
    let bob = Identity::generate().unwrap();
    let (left, right) = tokio::join!(
        authenticate(left, &alice, bob.certificate(), Role::Controller),
        authenticate(right, &bob, alice.certificate(), Role::Executor),
    );
    let left = left.unwrap();
    assert_eq!(left.worker_pid, 41);
    let (mut left_tx, mut left_rx) = left.split();
    let (mut right_tx, mut right_rx) = right.unwrap().split();
    let marker = "private-endpoint-payload-";
    let large =
        Packet::Text(serde_json::json!({"payload": marker.repeat(CHUNK_SIZE / 2)}).to_string());
    let reply =
        Packet::Text(serde_json::json!({"reply": marker.repeat(CHUNK_SIZE / 2)}).to_string());
    let (sent, responded, received, answered) = tokio::time::timeout(IO_TIMEOUT, async {
        tokio::join!(
            left_tx.send(large.clone()),
            right_tx.send(reply.clone()),
            right_rx.next(),
            left_rx.next()
        )
    })
    .await
    .unwrap();
    sent.unwrap();
    responded.unwrap();
    assert_eq!(received.unwrap().unwrap(), large);
    assert_eq!(answered.unwrap().unwrap(), reply);
    let encrypted = observed.lock().unwrap();
    assert!(
        !encrypted
            .windows(marker.len())
            .any(|window| window == marker.as_bytes())
    );
}

#[tokio::test]
async fn a_closed_tunnel_wakes_an_idle_endpoint_reader() {
    let (left, right, _) = endpoints();
    let mut stream = ByteStream::new(left);
    drop(right);
    let mut byte = [0];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), stream.read(&mut byte))
            .await
            .expect("tunnel closure did not reach the endpoint")
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn an_unapproved_endpoint_closes_the_transport_before_rpc_frames_are_accepted() {
    let (left, right, _) = endpoints();
    let alice = Identity::generate().unwrap();
    let bob = Identity::generate().unwrap();
    let unexpected = Identity::generate().unwrap();
    let (left, right) = tokio::join!(
        authenticate(left, &alice, unexpected.certificate(), Role::Controller),
        authenticate(right, &bob, alice.certificate(), Role::Executor),
    );
    assert!(left.is_err());
    assert!(right.is_err());
}
