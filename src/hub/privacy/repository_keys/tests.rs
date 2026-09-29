use super::*;
use serde_json::{Value, json};
use std::io::{Read, Write};

const AGENT: &str = "00000000-0000-0000-0000-000000000001";
const COMMIT: &str = "1234567890123456789012345678901234567890";

fn key() -> KeyRecord {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/privacy-web-key.json"
    ))
    .unwrap();
    serde_json::from_value(fixture["record"].clone()).unwrap()
}

fn config(version: u64, keys: Vec<KeyRecord>) -> RepositoryKeyConfig {
    RepositoryKeyConfig {
        agent_id: AGENT.into(),
        config_version: version,
        current_recipient: keys
            .iter()
            .find(|key| key.current)
            .map(|key| key.recipient.clone()),
        keys,
    }
}

fn server(
    responses: Vec<(u16, Value)>,
) -> (Client, RemoteIdentity, std::thread::JoinHandle<Vec<String>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let hub = format!("http://{}", listener.local_addr().unwrap());
    let worker = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for (status, response) in responses {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "expected repository-key request"
                        );
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(e) => panic!("{e}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                let count = stream.read(&mut buffer).unwrap();
                assert!(count > 0);
                request.extend_from_slice(&buffer[..count]);
                if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                    let size = head
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .map(|v| v.trim().parse::<usize>().unwrap())
                        .unwrap_or(0);
                    if request.len() >= end + 4 + size {
                        break;
                    }
                }
            }
            let body = response.to_string();
            write!(stream, "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            requests.push(String::from_utf8(request).unwrap());
        }
        requests
    });
    (
        Client::for_hub(&hub),
        RemoteIdentity::new(&hub, AGENT).unwrap(),
        worker,
    )
}

#[test]
fn anonymous_reader_requests_exact_publication_without_account_lookup() {
    let record = key();
    let (client, identity, worker) = server(vec![(
        200,
        json!({"agent_id":AGENT,"commit":COMMIT,"session_id":"public-session","key":record}),
    )]);
    client
        .publication_unlock_key("alice/app", &identity, COMMIT, &record.recipient)
        .unwrap();
    let requests = worker.join().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with(&format!(
        "GET /api/agents/alice/app/privacy/keys/{}?ref={COMMIT} ",
        record.recipient
    )));
    assert!(!requests[0].to_ascii_lowercase().contains("authorization:"));
    assert!(
        requests[0]
            .to_ascii_lowercase()
            .contains(&format!("x-agentgit-expected-agent-id: {AGENT}"))
    );
}

#[test]
fn writer_gets_unpublished_public_key_and_empty_config_is_actionable() {
    let record = key();
    let (anonymous, identity, worker) = server(vec![
        (
            200,
            json!({"agent_id":AGENT,"config_version":7,"current":{"recipient":record.recipient,"public_key_algorithm":"x25519","public_key":record.key.public_key}}),
        ),
        (
            200,
            json!({"agent_id":AGENT,"config_version":8,"current":null}),
        ),
    ]);
    let client = Client::for_hub_with_token(anonymous.base(), "synthetic-writer-token");
    let response = client
        .repository_publishing_key("alice/app", &identity)
        .unwrap();
    assert_eq!(response.config_version, 7);
    assert_eq!(
        response.require_current("alice/app").unwrap().public_key,
        record.key.public_key
    );
    let empty = client
        .repository_publishing_key("alice/app", &identity)
        .unwrap();
    assert!(
        empty
            .require_current("alice/app")
            .err()
            .unwrap()
            .to_string()
            .contains("agit privacy init alice/app")
    );
    for request in worker.join().unwrap() {
        assert!(request.starts_with("GET /api/agents/alice/app/privacy/publishing-key "));
        assert!(request.contains("Bearer synthetic-writer-token"));
    }
}

#[test]
fn mutation_uses_deleted_configuration_version_and_never_replays_conflict() {
    let record = key();
    let empty = config(8, vec![]);
    let (client, identity, worker) = server(vec![
        (200, serde_json::to_value(&empty).unwrap()),
        (
            409,
            json!({"kind":"conflict","error":"configuration changed"}),
        ),
    ]);
    let before = client
        .repository_key_config("alice/app", &identity)
        .unwrap();
    let error = client
        .mutate_repository_key(
            "alice/app",
            &identity,
            &before,
            KeyOperation::Initialize { key: &record.key },
        )
        .err()
        .unwrap();
    assert_eq!(error.downcast_ref::<ApiError>().unwrap().status, 409);
    assert!(error.to_string().contains("refresh and reconfirm"));
    let requests = worker.join().unwrap();
    assert_eq!(requests.len(), 2);
    let body: Value = serde_json::from_str(requests[1].split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(body["expected_agent_id"], AGENT);
    assert_eq!(body["expected_version"], 8);
    assert_eq!(body["operation"], "initialize");
    assert_eq!(body["key"], serde_json::to_value(record.key).unwrap());
}

#[test]
fn private_refusal_and_mismatched_reader_identity_do_not_fall_back() {
    let record = key();
    let valid =
        json!({"agent_id":AGENT,"commit":COMMIT,"session_id":"public-session","key":record});
    let mut responses = vec![(
        404,
        json!({"kind":"not_found","error":"publication key unavailable"}),
    )];
    for (pointer, replacement) in [
        ("/agent_id", json!("00000000-0000-0000-0000-000000000002")),
        ("/commit", json!("2345678901234567890123456789012345678901")),
        ("/key/recipient", json!("another-recipient")),
        ("/key/public_key_algorithm", json!("ed25519")),
    ] {
        let mut wrong = valid.clone();
        *wrong.pointer_mut(pointer).unwrap() = replacement;
        responses.push((200, wrong));
    }
    let count = responses.len();
    let (client, identity, worker) = server(responses);
    for n in 0..count {
        let error = client
            .publication_unlock_key("alice/app", &identity, COMMIT, &record.recipient)
            .err()
            .unwrap();
        if n == 0 {
            assert_eq!(error.downcast_ref::<ApiError>().unwrap().status, 404);
        }
    }
    assert_eq!(worker.join().unwrap().len(), count);
}

#[test]
fn mutation_checks_identity_version_recipient_and_historical_material() {
    let record = key();
    let previous = config(8, vec![]);
    let next = config(9, vec![record.clone()]);
    let valid = serde_json::to_value(&next).unwrap();
    let mut responses = vec![(200, valid.clone())];
    for (pointer, replacement) in [
        ("/agent_id", json!("00000000-0000-0000-0000-000000000002")),
        ("/config_version", json!(10)),
        ("/current_recipient", Value::Null),
        ("/keys/0/kdf/opslimit", json!(2)),
    ] {
        let mut wrong = valid.clone();
        *wrong.pointer_mut(pointer).unwrap() = replacement;
        responses.push((200, wrong));
    }
    let count = responses.len();
    let (client, identity, worker) = server(responses);
    for n in 0..count {
        let result = client.mutate_repository_key(
            "alice/app",
            &identity,
            &previous,
            KeyOperation::Initialize { key: &record.key },
        );
        assert_eq!(result.is_ok(), n == 0);
    }
    assert_eq!(worker.join().unwrap().len(), count);
}

#[test]
fn publisher_type_rejects_private_material_and_local_binding_rejects_another_hub() {
    let record = key();
    assert!(
        serde_json::from_value::<PublishingKey>(serde_json::to_value(record).unwrap()).is_err()
    );
    let client = Client::for_hub("https://first.invalid");
    let other = RemoteIdentity::new("https://second.invalid", AGENT).unwrap();
    assert!(client.repository_key_config("alice/app", &other).is_err());
    assert!(
        client
            .publication_unlock_key("alice/app", &other, "main", "recipient")
            .is_err()
    );
}

#[test]
fn rewrap_rotation_and_revocation_preserve_other_history() {
    let old = key();
    let previous = config(11, vec![old.clone()]);
    let mut rewrapped = old.key.clone();
    rewrapped.kdf.opslimit = 2;
    let rewrap = KeyOperation::Rewrap {
        recipient: &old.recipient,
        key: &rewrapped,
    };
    validate_operation(&previous, &rewrap).unwrap();
    let mut changed = old.clone();
    changed.key = rewrapped.clone();
    verify_mutation(&previous, &rewrap, &config(12, vec![changed])).unwrap();

    let mut fresh = old.clone();
    fresh.key.public_key = {
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        STANDARD.encode(x25519_dalek::x25519(
            [77; 32],
            x25519_dalek::X25519_BASEPOINT_BYTES,
        ))
    };
    fresh.recipient = recipient_id(&fresh.key.public_key);
    let rotate = KeyOperation::Rotate { key: &fresh.key };
    validate_operation(&previous, &rotate).unwrap();
    let mut historical = old.clone();
    historical.current = false;
    let after = config(12, vec![historical.clone(), fresh.clone()]);
    after.validate().unwrap();
    verify_mutation(&previous, &rotate, &after).unwrap();
    historical.key.kdf.opslimit = 2;
    assert!(
        verify_mutation(
            &previous,
            &rotate,
            &config(12, vec![historical, fresh.clone()])
        )
        .is_err()
    );
    assert!(
        validate_operation(
            &previous,
            &KeyOperation::Rewrap {
                recipient: &old.recipient,
                key: &fresh.key
            }
        )
        .is_err()
    );
    assert!(validate_operation(&previous, &KeyOperation::Rotate { key: &old.key }).is_err());
    let revoked = KeyOperation::Revoke {
        recipient: &fresh.recipient,
    };
    let mut only_old = old.clone();
    only_old.current = false;
    let after_revoke = config(13, vec![only_old]);
    verify_mutation(&after, &revoked, &after_revoke).unwrap();
    validate_operation(&after_revoke, &KeyOperation::Initialize { key: &fresh.key }).unwrap();
    assert!(
        validate_operation(&after_revoke, &KeyOperation::Initialize { key: &old.key }).is_err()
    );
}

#[test]
fn unavailable_transport_retains_its_failure_without_key_fallback() {
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let hub = format!("http://{}", socket.local_addr().unwrap());
    drop(socket);
    let client = Client::for_hub(&hub);
    let identity = RemoteIdentity::new(&hub, AGENT).unwrap();
    let error = client
        .repository_publishing_key("alice/app", &identity)
        .err()
        .unwrap();
    assert!(error.downcast_ref::<ApiError>().is_none());
    assert!(!error.to_string().contains("not configured"));
}
