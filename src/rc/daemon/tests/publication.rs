use super::*;
use crate::{
    domain::{
        privacy_receipt::{
            PublicationReceipt, SupervisorPushRequest,
            outbox::{Capture, Entry},
        },
        repo::Repo,
    },
    rc::{
        cloud::{ingress::Registry, store},
        lineage::AgitSession,
        local_repository::publication::Selection,
    },
};
use agit_peer::{
    access::{Access, Policy, Principal, Resource, Rule},
    cloud::{ConnectionGrant, Device, DeviceCredential, Secret, SessionController},
    publication::{Acknowledgement, Delivery, Receipt},
};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

mod eligibility;
mod git;
mod transport;

fn request() -> Frame {
    Frame::request(
        method::SESSION_PUBLICATION_DELIVER,
        json!({"session_id":"logical", "workspace_id":"local-owner"}),
    )
}

/// A lost response and a superseded controller both retain the same durable notification.
#[test]
fn publication_delivery_retries_under_current_authority_and_recovers_receipts() {
    if crate::rc::in_isolated_test(
        "rc::daemon::tests::publication::publication_delivery_retries_under_current_authority_and_recovers_receipts",
    ) {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    // Process isolation keeps worker threads on the fixture's private repository and credentials.
    unsafe {
        std::env::set_var("AGIT_HOME", home.path());
    }
    crate::rc::select_local_authority();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(exercise(home.path(), true));
}

#[test]
fn ordinary_publication_delivery_retries_and_recovers_without_repository_keys() {
    if crate::rc::in_isolated_test(
        "rc::daemon::tests::publication::ordinary_publication_delivery_retries_and_recovers_without_repository_keys",
    ) {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("AGIT_HOME", home.path());
    }
    crate::rc::select_local_authority();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(exercise(home.path(), false));
}

async fn exercise(home: &std::path::Path, encryption_enabled: bool) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hub = format!("http://{}", listener.local_addr().unwrap());
    let owner = Principal {
        issuer: hub.clone(),
        account_id: "owner".into(),
    };
    let identity = agit_peer::Identity::generate().unwrap();
    let controller_identity = agit_peer::Identity::generate().unwrap();
    let device = Device {
        id: "executor".into(),
        owner: owner.clone(),
        machine_id: "machine".into(),
        display_name: "Fixture".into(),
        certificate: identity.certificate().clone(),
        credential_epoch: 1,
    };
    store::save(&store::Enrollment {
        identity,
        inbound_enabled: true,
        credential: DeviceCredential {
            device: device.clone(),
            token: Secret::new("fixture-device-token".into()),
        },
    })
    .unwrap();
    let machine = crate::rc::identity::identity().unwrap();
    let lineage = AgitSession::new(
        "desktop-local/project",
        "00000000-0000-0000-0000-000000000001",
        "s/work",
    )
    .unwrap();
    let repo = Repo::open_or_init(&lineage.repo_dir().unwrap()).unwrap();
    repo.git(&["config", "agit.desktopIdentity", lineage.agent_id()])
        .unwrap();
    repo.git(&[
        "config",
        "agit.desktopAuthority",
        &format!("local:{}", machine.machine_fingerprint),
    ])
    .unwrap();
    let destination =
        crate::hub::identity::RemoteIdentity::new(&hub, "00000000-0000-0000-0000-000000000002")
            .unwrap();
    Selection::prepare(&repo, Some("owner/project"), &hub)
        .unwrap()
        .unwrap()
        .bind(&repo, &destination)
        .unwrap();
    let (published, remote) = git::publication(
        &repo,
        lineage.branch(),
        destination.clone(),
        &home.join("received.git"),
        encryption_enabled,
    );
    eligibility::configure(&repo, &hub, &published);
    let mut first = SupervisorPushRequest {
        version: 1,
        request_id: uuid::Uuid::new_v4().to_string(),
        notification_id: None,
        repository: "owner/project".into(),
        branch: lineage.branch().into(),
        source: published.source.clone(),
        destination,
    };
    let capture = Capture {
        session_id: "logical".into(),
        native_session_id: "native".into(),
        runtime: "codex".into(),
        generation: 7,
        incarnation: Some("earlier-daemon".into()),
        through_seq: Some(21),
    };
    let mut unavailable = first.clone();
    unavailable.source = "e".repeat(40);
    Entry::begin(&repo, &mut unavailable, capture.clone()).unwrap();
    Entry::prepare(
        &repo,
        &unavailable,
        &PublicationReceipt {
            version: if encryption_enabled { 1 } else { 2 },
            mode: published.mode,
            repository: unavailable.repository.clone(),
            branch: unavailable.branch.clone(),
            source: unavailable.source.clone(),
            published: if encryption_enabled {
                "f".repeat(40)
            } else {
                unavailable.source.clone()
            },
            projected_session_id: None,
            destination: unavailable.destination.clone(),
            url: format!("{hub}/owner/project.git"),
            policy_digest: encryption_enabled.then(|| "policy".into()),
            recipient: encryption_enabled.then(|| "recipient".into()),
        },
    )
    .unwrap();
    Entry::begin(&repo, &mut first, capture.clone()).unwrap();
    Entry::prepare(&repo, &first, &published).unwrap();
    let mut later = first.clone();
    later.source = "d".repeat(40);
    later.notification_id = None;
    Entry::begin(&repo, &mut later, capture.clone()).unwrap();

    let mut roster = Roster::default();
    roster
        .record(
            "logical",
            serde_json::from_value(json!({
                "runtime":"codex", "thread_id":"native", "cwd":home, "workspace_id":"local-owner",
                "agit_session":lineage.to_string(), "expected_agent_id":lineage.agent_id()
            }))
            .unwrap(),
        )
        .unwrap();
    let make_daemon = || rpc_test_daemon(HashMap::new(), roster.clone());
    let daemon = make_daemon();
    daemon.lock().await.opts.local_owner = true;
    let mut grant = ConnectionGrant {
        id: "grant".into(),
        caller: owner,
        source: Device {
            id: "controller".into(),
            certificate: controller_identity.certificate().clone(),
            ..device.clone()
        },
        target: device,
        expires_at_ms: chrono::Utc::now().timestamp_millis() + 120_000,
        project_controller: None,
        session_controller: Some(SessionController {
            session_id: "native".into(),
            runtime: "codex".into(),
            generation: 1,
            access: Access::Control,
        }),
    };
    let (arrived_tx, mut arrived) = mpsc::channel(1);
    let (release_tx, mut release) = mpsc::channel(1);
    let expected_public = published.published.clone();
    let private_source = first.source.clone();
    let current_key = Arc::new(std::sync::atomic::AtomicU8::new(23));
    let served_key = current_key.clone();
    let server_hub = hub.clone();
    let server = tokio::spawn(async move {
        let mut original = None;
        let mut receipt: Option<Receipt> = None;
        let mut withheld = vec![];
        let mut attempt = 0;
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut line = String::new();
            stream.read_line(&mut line).await.unwrap();
            let request_line = line.trim().to_owned();
            let mut length = 0;
            let mut authorized = false;
            loop {
                line.clear();
                stream.read_line(&mut line).await.unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some((key, value)) = line.split_once(':') {
                    if key.eq_ignore_ascii_case("content-length") {
                        length = value.trim().parse::<usize>().unwrap();
                    }
                    if key.eq_ignore_ascii_case("authorization") {
                        authorized = value.trim() == "Bearer fixture-device-token";
                    }
                }
            }
            assert!(length <= agit_peer::publication::MAX_NOTIFICATION_BYTES);
            let mut body = vec![0; length];
            stream.read_exact(&mut body).await.unwrap();
            if !encryption_enabled {
                assert!(!request_line.contains("/privacy/"));
            }
            if let Some(mut response) =
                eligibility::response(&server_hub, &request_line, encryption_enabled)
            {
                if request_line.contains("/privacy/publishing-key ") {
                    use base64::Engine;
                    let public = base64::engine::general_purpose::STANDARD.encode(
                        crypto_box::SecretKey::from(
                            [served_key.load(std::sync::atomic::Ordering::Acquire); 32],
                        )
                        .public_key()
                        .as_bytes(),
                    );
                    response["current"]["public_key"] = json!(public);
                    response["current"]["recipient"] =
                        json!(crate::domain::privacy_key::recipient_id(&public));
                }
                respond(&mut stream, &response).await;
                continue;
            }
            assert_eq!(request_line, "POST /api/peer/publications/confirm HTTP/1.1");
            assert!(authorized);
            if encryption_enabled {
                assert!(!String::from_utf8_lossy(&body).contains(&private_source));
            }
            let delivery: Delivery = serde_json::from_slice(&body).unwrap();
            git::verify(&remote, &delivery, encryption_enabled);
            assert_eq!(delivery.notification.public_commit, expected_public);
            if let Some(ref original) = original {
                assert_eq!(&delivery.notification, original);
            } else {
                original = Some(delivery.notification.clone());
            }
            let receipt = receipt
                .get_or_insert_with(|| Receipt {
                    version: 1,
                    receipt_id: "durable-receipt".into(),
                    notification_id: delivery.notification.notification_id.clone(),
                    binding_digest: delivery.notification.digest().unwrap(),
                    repository_id: delivery.notification.repository_id.clone(),
                    public_commit: delivery.notification.public_commit.clone(),
                })
                .clone();
            if attempt == 0 {
                withheld.push(stream);
                attempt += 1;
                continue;
            }
            if attempt == 1 {
                arrived_tx.send(()).await.unwrap();
                release.recv().await.unwrap();
            }
            let body = serde_json::to_vec(&Acknowledgement {
                grant_id: delivery.grant_id,
                controller_generation: delivery.controller_generation,
                receipt,
            })
            .unwrap();
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).as_bytes()).await.unwrap();
            stream.write_all(&body).await.unwrap();
            stream.flush().await.unwrap();
            attempt += 1;
        }
    });
    let endpoint = transport::Endpoint::new(daemon.clone(), &grant).await;
    let mut connection = endpoint.connect(grant.clone(), &controller_identity).await;
    let mut wrong_workspace = request();
    wrong_workspace.params.as_mut().unwrap()["workspace_id"] = json!("another-workspace");
    assert!(
        connection
            .call(wrong_workspace)
            .await
            .unwrap_err()
            .is(ErrorCode::WorkspaceNotFound)
    );
    let mut forged = request();
    forged.params.as_mut().unwrap()["source"] = json!(first.source);
    assert!(
        connection
            .call(forged)
            .await
            .unwrap_err()
            .is(ErrorCode::MalformedFrame)
    );
    for (stream, name) in [
        ("hidden", method::SESSION_PUBLICATION_CHANGED),
        ("logical", method::COMMIT_LOCAL_SETTLED),
        ("logical", method::SESSION_PUBLICATION_CHANGED),
    ] {
        let mut hint = Frame::notification(name, json!({"session_id":stream}));
        hint.stream = Some(stream.into());
        hint.seq = Some(1);
        endpoint.outbound.send(hint);
    }
    for name in [
        method::COMMIT_LOCAL_SETTLED,
        method::SESSION_PUBLICATION_CHANGED,
    ] {
        let hint = connection.receive().await;
        assert_eq!(hint.method(), name);
        assert_eq!(hint.stream.as_deref(), Some("logical"));
        assert_eq!(hint.params.unwrap(), json!({"session_id":"logical"}));
    }
    {
        let mut state = daemon.lock().await;
        let row = state.roster.sessions.get_mut("logical").unwrap();
        row.agit_session = None;
        row.expected_agent_id = None;
    }
    let unbound = connection.call(request()).await.unwrap_err();
    assert_eq!(unbound.data.unwrap()["publication"]["readiness"], "unbound");
    {
        let mut state = daemon.lock().await;
        let row = state.roster.sessions.get_mut("logical").unwrap();
        row.agit_session = Some(lineage.to_string());
        row.expected_agent_id = Some(lineage.agent_id().into());
    }
    repo.set_auto_push(Some(false)).unwrap();
    let disabled = connection.call(request()).await.unwrap();
    assert_eq!(disabled["publication"]["readiness"], "setup_required");
    assert_eq!(disabled["publication"]["reason"], "push_disabled");
    assert!(disabled["items"].as_array().unwrap().is_empty());
    assert!(disabled["retry_after_ms"].is_null());
    repo.set_auto_push(Some(true)).unwrap();
    let consent_path = repo
        .common_dir()
        .unwrap()
        .join("agit/privacy-auto-consent.json");
    // An ordinary destination delivers without any saved consent; an encrypted one requires
    // consent matching its current account, policy and recipient.
    assert_eq!(consent_path.exists(), encryption_enabled);
    if encryption_enabled {
        let consent = std::fs::read(&consent_path).unwrap();
        let mut changed: serde_json::Value = serde_json::from_slice(&consent).unwrap();
        changed["account_id"] = json!("another-account");
        std::fs::write(&consent_path, serde_json::to_vec(&changed).unwrap()).unwrap();
        let required = connection.call(request()).await.unwrap();
        assert_eq!(
            required["publication"]["reason"], "consent_required",
            "{required}"
        );
        assert_ne!(required["publication"]["stage"], "receiver");
        std::fs::write(&consent_path, &consent).unwrap();
        current_key.store(24, std::sync::atomic::Ordering::Release);
        let rotated = connection.call(request()).await.unwrap();
        assert_eq!(rotated["publication"]["reason"], "consent_required");
        assert!(rotated["items"].as_array().unwrap().is_empty());
        let mut renewed: serde_json::Value = serde_json::from_slice(&consent).unwrap();
        use base64::Engine;
        let public = base64::engine::general_purpose::STANDARD.encode(
            crypto_box::SecretKey::from([24; 32])
                .public_key()
                .as_bytes(),
        );
        let recipient = crate::domain::privacy_envelope::ViewingRecipient::from_base64(
            crate::domain::privacy_key::recipient_id(&public),
            &public,
        )
        .unwrap();
        renewed["recipient"] = json!(recipient.fingerprint().unwrap());
        std::fs::write(&consent_path, serde_json::to_vec(&renewed).unwrap()).unwrap();
    }
    let result = connection.call(request()).await.unwrap();
    assert_eq!(result["items"][1]["status"], "retry", "{result}");
    assert_eq!(result["items"][2]["status"], "awaiting_publication");
    assert_eq!(result["items"][0]["status"], "local_state_unavailable");
    assert_eq!(result["pending"], 3);
    assert!(
        Entry::load(&repo, &first)
            .unwrap()
            .unwrap()
            .acknowledged
            .is_none()
    );

    connection.send(&request()).await;
    tokio::time::timeout(std::time::Duration::from_secs(10), arrived.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        daemon.try_lock().is_ok(),
        "network delivery must release the daemon mutex"
    );
    grant.id = "replacement".into();
    grant.session_controller.as_mut().unwrap().generation += 1;
    let _replacement = endpoint.connect(grant.clone(), &controller_identity).await;
    release_tx.send(()).await.unwrap();
    connection.expect_revoked().await;
    assert!(
        Entry::load(&repo, &first)
            .unwrap()
            .unwrap()
            .acknowledged
            .is_none()
    );

    let restarted = make_daemon();
    restarted.lock().await.opts.local_owner = true;
    drop(endpoint);
    let endpoint = transport::Endpoint::new(restarted.clone(), &grant).await;
    let mut connection = endpoint.connect(grant, &controller_identity).await;
    let accepted = connection.call(request()).await.unwrap();
    assert_eq!(accepted["items"][1]["status"], "acknowledged");
    assert_eq!(
        accepted["items"][1]["notification"]["capture"]["incarnation"],
        "earlier-daemon"
    );
    assert_eq!(accepted["pending"], 2);
    let saved = Entry::load(&repo, &first).unwrap().unwrap();
    assert!(saved.acknowledged.is_some() && saved.publication.is_some());
    assert!(accepted["items"][1]["coverage"].is_null());

    let replay = connection.call(request()).await.unwrap();
    assert_eq!(
        replay, accepted,
        "the durable response survives a lost controller reply without another HTTP request"
    );
    assert_eq!(
        Entry::pending(&repo, lineage.branch(), "native", "codex")
            .unwrap()
            .len(),
        2
    );

    let (tx, _) = mpsc::channel(1);
    let mut live = rpc_test_live("logical", 7, tx, crate::protocol::PermissionMode::Default);
    live.runtime_thread_id = Some("native".into());
    {
        let mut state = restarted.lock().await;
        state.identity.instance_id = "earlier-daemon".into();
        state.sessions.insert("logical".into(), live);
    }
    let covered = connection.call(request()).await.unwrap();
    assert_eq!(covered["items"][1]["coverage"]["through_seq"], 21);
    restarted
        .lock()
        .await
        .sessions
        .get_mut("logical")
        .unwrap()
        .generation += 1;
    let resumed = connection.call(request()).await.unwrap();
    assert!(resumed["items"][1]["coverage"].is_null());

    for index in 0..15 {
        let mut request = later.clone();
        request.source = format!("{index:040x}");
        request.notification_id = None;
        Entry::begin(&repo, &mut request, capture.clone()).unwrap();
    }
    let page = connection.call(request()).await.unwrap();
    assert_eq!(page["items"].as_array().unwrap().len(), 16);
    let mut next = request();
    next.params.as_mut().unwrap()["after"] = page["next_after"].clone();
    let last = connection.call(next).await.unwrap();
    assert_eq!(last["items"].as_array().unwrap().len(), 2);
    assert!(last["next_after"].is_null());
    server.abort();
    endpoint.stop.send_replace(true);
    assert!(connection.call(request()).await.is_err());
}

async fn respond(stream: &mut BufReader<tokio::net::TcpStream>, body: &serde_json::Value) {
    let body = serde_json::to_vec(body).unwrap();
    stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
    stream.write_all(&body).await.unwrap();
    stream.flush().await.unwrap();
}
