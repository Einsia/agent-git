use super::*;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

#[tokio::test]
async fn authenticated_endpoints_exchange_frames_over_an_opaque_stream() {
    let a = Identity::generate().unwrap();
    let b = Identity::generate().unwrap();
    let (left, right) = duplex(4096);
    let (left, right) = tokio::join!(
        a.connect(b.certificate(), left),
        b.accept(a.certificate(), right),
    );
    let (mut left, mut right) = (left.unwrap(), right.unwrap());
    let frame = json!({"method":"session.list","id":"request-1"});
    protocol::write(&mut left, &frame).await.unwrap();
    assert_eq!(
        protocol::Reader::new(&mut right)
            .read::<Value>()
            .await
            .unwrap(),
        Some(frame)
    );
    protocol::write(&mut right, &json!({"id":"request-1","result":[]}))
        .await
        .unwrap();
    assert_eq!(
        protocol::Reader::new(&mut left)
            .read::<Value>()
            .await
            .unwrap()
            .unwrap()["id"],
        "request-1"
    );
}

#[tokio::test]
async fn a_relay_cannot_substitute_an_unapproved_server() {
    let client = Identity::generate().unwrap();
    let approved = Identity::generate().unwrap();
    let substitute = Identity::generate().unwrap();
    let (left, right) = duplex(4096);
    let (result, _) = tokio::join!(
        client.connect(approved.certificate(), left),
        substitute.accept(client.certificate(), right),
    );
    assert!(result.is_err());
}

#[tokio::test]
async fn an_unapproved_client_cannot_complete_endpoint_authentication() {
    let client = Identity::generate().unwrap();
    let approved = Identity::generate().unwrap();
    let server = Identity::generate().unwrap();
    let (left, right) = duplex(4096);
    let (_, result) = tokio::join!(
        client.connect(server.certificate(), left),
        server.accept(approved.certificate(), right),
    );
    assert!(result.is_err());
}

#[test]
fn persisted_identity_must_match_its_private_key() {
    let identity = Identity::generate().unwrap();
    let recovered = Identity::from_der(
        identity.certificate().as_der().to_vec(),
        identity.private_key_der().to_vec(),
    )
    .unwrap();
    assert_eq!(identity.certificate(), recovered.certificate());
    let other = Identity::generate().unwrap();
    assert!(
        Identity::from_der(
            identity.certificate().as_der().to_vec(),
            other.private_key_der().to_vec()
        )
        .is_err()
    );
}

#[tokio::test]
async fn oversized_frame_is_rejected_before_reading_its_body() {
    let (mut output, input) = duplex(32);
    output
        .write_u32((MAX_FRAME_BYTES + 1) as u32)
        .await
        .unwrap();
    assert!(protocol::Reader::new(input).read::<Value>().await.is_err());
}

#[tokio::test]
async fn a_partial_header_is_not_a_clean_disconnect() {
    let (mut output, input) = duplex(32);
    output.write_all(&[0, 0]).await.unwrap();
    drop(output);
    let mut reader = protocol::Reader::new(input);
    assert!(reader.read::<Value>().await.is_err());
    assert!(reader.read::<Value>().await.is_err());
}

#[tokio::test(start_paused = true)]
async fn canceling_partial_reads_preserves_the_next_frame_boundary() {
    for split in [1, 3, 5, 7] {
        let (mut output, input) = duplex(128);
        let body = br#"{"a":1}"#;
        let mut record = (body.len() as u32).to_be_bytes().to_vec();
        record.extend_from_slice(body);
        output.write_all(&record[..split]).await.unwrap();
        let mut reader = protocol::Reader::new(input);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), reader.read::<Value>())
                .await
                .is_err()
        );
        output.write_all(&record[split..]).await.unwrap();
        protocol::write(&mut output, &json!({"b":2})).await.unwrap();
        assert_eq!(reader.read::<Value>().await.unwrap(), Some(json!({"a":1})));
        assert_eq!(reader.read::<Value>().await.unwrap(), Some(json!({"b":2})));
        drop(output);
        assert_eq!(reader.read::<Value>().await.unwrap(), None);
    }
}

#[tokio::test(start_paused = true)]
async fn silent_endpoint_authentication_expires() {
    let identity = Identity::generate().unwrap();
    let peer = Identity::generate().unwrap();
    let (left, mut right) = duplex(4096);
    let drain = tokio::spawn(async move {
        let mut bytes = Vec::new();
        let _ = right.read_to_end(&mut bytes).await;
    });
    let result = identity.connect(peer.certificate(), left).await;
    assert!(result.unwrap_err().to_string().contains("timed out"));
    drain.await.unwrap();
}
