use super::*;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

#[tokio::test]
async fn explicit_inbound_recovers_revocation_without_rotating_healthy_credentials() {
    if let Ok(hub) = std::env::var("AGIT_RECOVERY_TEST_HUB") {
        super::super::super::select_local_authority();
        crate::infra::credentials::save(
            &hub,
            &crate::infra::credentials::HubCredential {
                account_id: Some("owner".into()),
                username: "owner".into(),
                email: None,
                hub: Some(hub.clone()),
                access_token: "fixture-account".into(),
                access_expires_at: "2999-01-01T00:00:00Z".into(),
                refresh_token: "fixture-refresh".into(),
                refresh_expires_at: "2999-01-01T00:00:00Z".into(),
            },
        )
        .unwrap();
        let api = Client::new(&hub).unwrap();
        store::request_inbound(&hub).unwrap();
        assert!(enroll_pending(&hub).await.unwrap());
        let first = store::load(&hub).unwrap().unwrap();
        assert!(first.inbound_enabled);
        assert!(!store::inbound_pending(&hub).unwrap());
        assert!(!enroll_pending(&hub).await.unwrap());

        store::request_inbound(&hub).unwrap();
        assert!(!enroll_pending(&hub).await.unwrap());
        let healthy = store::load(&hub).unwrap().unwrap();
        assert_eq!(
            first.credential.token.expose(),
            healthy.credential.token.expose()
        );
        assert_eq!(first.identity.certificate(), healthy.identity.certificate());
        assert!(!store::inbound_pending(&hub).unwrap());

        api.revoke(
            &Secret::new("fixture-account".into()),
            &first.credential.device,
        )
        .await
        .unwrap();
        assert!(
            !enroll_pending(&hub).await.unwrap(),
            "background retry must preserve revocation"
        );
        let controller = controller(&hub).await.unwrap();
        assert_eq!(
            controller.credential.token.expose(),
            first.credential.token.expose()
        );
        store::request_inbound(&hub).unwrap();
        assert!(enroll_pending(&hub).await.unwrap());
        let restored = store::load(&hub).unwrap().unwrap();
        assert_eq!(restored.credential.device.id, first.credential.device.id);
        assert_eq!(
            restored.credential.device.machine_id,
            first.credential.device.machine_id
        );
        assert!(
            restored.credential.device.credential_epoch > first.credential.device.credential_epoch
        );
        assert_ne!(
            restored.identity.certificate(),
            first.identity.certificate()
        );
        assert!(!store::inbound_pending(&hub).unwrap());
        return;
    }

    let home = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hub = format!("http://{}", listener.local_addr().unwrap());
    let issuer = hub.clone();
    let (stop, mut stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let mut device = Value::Null;
        let mut revoked = false;
        let (mut registrations, mut pages) = (0, 0);
        loop {
            let socket = tokio::select! {
                _ = &mut stopped => break,
                socket = listener.accept() => socket.unwrap().0,
            };
            let mut socket = BufReader::new(socket);
            let mut line = String::new();
            socket.read_line(&mut line).await.unwrap();
            let request = line.clone();
            let mut length = 0;
            loop {
                line.clear();
                socket.read_line(&mut line).await.unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some((key, value)) = line.split_once(':')
                    && key.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse::<usize>().unwrap();
                }
            }
            let mut body = vec![0; length];
            socket.read_exact(&mut body).await.unwrap();
            let reply = if request.starts_with("POST /api/peer/devices ") {
                registrations += 1;
                let body: Value = serde_json::from_slice(&body).unwrap();
                device = json!({"id":"executor", "owner":{"issuer":issuer,"account_id":"owner"},
                    "machine_id":body["machine_id"], "display_name":body["display_name"],
                    "certificate":body["certificate"], "credential_epoch":registrations});
                revoked = false;
                json!({"device":device, "token":format!("fixture-device-{registrations}")})
            } else if request.starts_with("DELETE /api/peer/devices/executor ") {
                revoked = true;
                json!({"ok":true})
            } else if request.starts_with("GET /api/peer/devices") {
                pages += 1;
                if request.contains("?after=before ") {
                    json!({"devices":if revoked { vec![] } else { vec![json!({"device":device,"online":false})] },"next_cursor":null})
                } else {
                    json!({"devices":[],"next_cursor":"before"})
                }
            } else {
                panic!("unexpected Cloud request: {request}")
            };
            let body = serde_json::to_vec(&reply).unwrap();
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            socket.write_all(&body).await.unwrap();
        }
        (registrations, pages)
    });
    let output = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "rc::cloud::commands::tests::explicit_inbound_recovers_revocation_without_rotating_healthy_credentials", "--nocapture"])
        .env("AGIT_RECOVERY_TEST_HUB", &hub).env("AGIT_HOME", home.path())
        .env("AGIT_HUB_URL", &hub).env("NO_PROXY", "127.0.0.1").env("no_proxy", "127.0.0.1")
        .output().await.unwrap();
    stop.send(()).unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        server.await.unwrap(),
        (2, 4),
        "only explicit recovery may register again; discovery must follow pagination"
    );
}
