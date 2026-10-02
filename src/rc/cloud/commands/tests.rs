use super::*;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

#[tokio::test]
async fn explicit_inbound_switches_accounts_and_recovers_revocation_without_rotating_healthy_credentials()
 {
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

        super::super::super::identity::set_display_name("office-machine").unwrap();
        assert!(!enroll_pending(&hub).await.unwrap());
        let renamed = store::load(&hub).unwrap().unwrap();
        assert_eq!(renamed.credential.device.display_name, "office-machine");
        assert_eq!(
            renamed.credential.token.expose(),
            first.credential.token.expose()
        );
        assert_eq!(
            renamed.credential.device.credential_epoch,
            first.credential.device.credential_epoch
        );
        assert_eq!(renamed.identity.certificate(), first.identity.certificate());

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
        let outgoing = controller(&hub).await.unwrap();
        assert_eq!(
            outgoing.credential.token.expose(),
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

        let old_owner = restored.credential.device.owner.clone();
        store::grant(Rule {
            principal: Principal {
                issuer: hub.clone(),
                account_id: "collaborator".into(),
            },
            resource: Resource::Project("shared-project".into()),
            access: Access::Control,
        })
        .unwrap();
        let other_hub = Rule {
            principal: Principal {
                issuer: "https://other.example".into(),
                account_id: "other-owner".into(),
            },
            resource: Resource::Machine,
            access: Access::Admin,
        };
        store::grant(other_hub.clone()).unwrap();
        let mut credential = crate::infra::credentials::load_checked(&hub)
            .unwrap()
            .unwrap();
        crate::infra::credentials::remove(&hub).unwrap();
        assert!(store::request_inbound(&hub).is_err());
        assert_eq!(
            store::load(&hub).unwrap().unwrap().credential.device.owner,
            old_owner
        );

        credential.account_id = Some("new-owner".into());
        credential.access_token = "fixture-new-account".into();
        crate::infra::credentials::save(&hub, &credential).unwrap();
        assert!(
            controller(&hub).await.is_err(),
            "outgoing access cannot reuse another account's device"
        );
        let policy_lock =
            store::private_lock(&store::directory().unwrap().join("cloud-access.lock")).unwrap();
        fs2::FileExt::try_lock_exclusive(&policy_lock).unwrap();
        assert!(store::request_inbound(&hub).is_err());
        let interrupted = store::load(&hub).unwrap().unwrap();
        assert!(!interrupted.inbound_enabled);
        assert_eq!(interrupted.credential.device.owner, old_owner);
        drop(policy_lock);
        store::request_inbound(&hub).unwrap();
        assert!(store::load(&hub).unwrap().is_none());
        assert_eq!(
            serde_json::to_value(store::policy().unwrap().rules()).unwrap(),
            json!([other_hub.clone()])
        );
        assert!(store::inbound_pending(&hub).unwrap());
        assert!(enroll_pending(&hub).await.unwrap());
        let switched = store::load(&hub).unwrap().unwrap();
        assert_eq!(switched.credential.device.owner.account_id, "new-owner");
        assert_ne!(switched.credential.device.id, restored.credential.device.id);
        assert_eq!(
            switched.credential.device.machine_id,
            restored.credential.device.machine_id
        );
        assert_ne!(
            switched.identity.certificate(),
            restored.identity.certificate()
        );
        assert!(switched.inbound_enabled);
        assert!(
            !store::policy()
                .unwrap()
                .rules()
                .iter()
                .any(|rule| rule.principal == old_owner)
        );
        store::request_inbound(&hub).unwrap();
        assert!(!enroll_pending(&hub).await.unwrap());
        assert_eq!(
            store::load(&hub)
                .unwrap()
                .unwrap()
                .credential
                .token
                .expose(),
            switched.credential.token.expose()
        );

        credential.account_id = Some("owner".into());
        credential.access_token = "fixture-account".into();
        crate::infra::credentials::save(&hub, &credential).unwrap();
        inbound(&hub, true).await.unwrap();
        let returned = store::load(&hub).unwrap().unwrap();
        assert_eq!(returned.credential.device.owner, old_owner);
        assert_eq!(
            returned.credential.device.machine_id,
            first.credential.device.machine_id
        );
        assert!(returned.inbound_enabled);
        assert!(
            store::policy()
                .unwrap()
                .rules()
                .iter()
                .any(|rule| rule.principal == other_hub.principal
                    && rule.resource == other_hub.resource
                    && rule.access == other_hub.access)
        );
        assert!(
            !store::policy()
                .unwrap()
                .rules()
                .iter()
                .any(|rule| rule.principal.account_id == "new-owner"
                    || rule.principal.account_id == "collaborator")
        );
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
        let (mut registrations, mut pages, mut renames) = (0, 0, 0);
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
            let mut authorization = String::new();
            loop {
                line.clear();
                socket.read_line(&mut line).await.unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some((key, value)) = line.split_once(':')
                    && key.eq_ignore_ascii_case("authorization")
                {
                    authorization = value.trim().to_owned();
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
                let (account, id) = match authorization.as_str() {
                    "Bearer fixture-account" => ("owner", "executor"),
                    "Bearer fixture-new-account" => ("new-owner", "new-executor"),
                    _ => panic!("unexpected enrollment authorization"),
                };
                device = json!({"id":id, "owner":{"issuer":issuer,"account_id":account},
                    "machine_id":body["machine_id"], "display_name":body["display_name"],
                    "certificate":body["certificate"], "credential_epoch":registrations});
                revoked = false;
                json!({"device":device, "token":format!("fixture-device-{registrations}")})
            } else if request.starts_with("PATCH /api/peer/devices/executor ") {
                renames += 1;
                let body: Value = serde_json::from_slice(&body).unwrap();
                device["display_name"] = body["display_name"].clone();
                json!({"ok":true})
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
        (registrations, pages, renames)
    });
    let output = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "rc::cloud::commands::tests::explicit_inbound_switches_accounts_and_recovers_revocation_without_rotating_healthy_credentials", "--nocapture"])
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
        (4, 6, 1),
        "only explicit recovery or account switching may register again; discovery must follow pagination"
    );
}
