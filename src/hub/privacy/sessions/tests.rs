use super::*;
use crate::hub::client::ApiError;
use crate::hub::privacy::grants::{GrantMutation, GrantOperation, GrantTarget};
use crate::hub::privacy::test_server::server_with_headers;
use serde_json::{Value, json};

const NO_STORE: &str = "Cache-Control: private, no-store\r\n";

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../../../tests/fixtures/privacy-session-owner-envelope.json"
    ))
    .unwrap()
}

fn public_key(fixture: &Value) -> Value {
    json!({
        "account_id": "account-alice", "key_version": 1,
        "public_key_algorithm": "x25519", "public_key": fixture["viewing_public_key"],
    })
}

/// Publication, recovery and grant retries retain the exact snapshot and confirmed membership.
#[test]
fn account_registration_and_original_read_keep_exact_session_authority() {
    let fixture = fixture();
    let config: SessionPrivacyConfig = serde_json::from_value(fixture["config"].clone()).unwrap();
    let snapshot: AcceptedSnapshot =
        serde_json::from_value(fixture["content_key_response"]["snapshot"].clone()).unwrap();
    let target = GrantTarget::from_snapshot(&snapshot).unwrap();
    let (anonymous, identity, worker) = server_with_headers(
        |hub| {
            let mut config = config.clone();
            config.privacy.hub = Some(hub.into());
            let responses = vec![
                (200, json!({"ready": true, "key": public_key(&fixture)})),
                (
                    200,
                    json!({"config": config, "owner_key": public_key(&fixture)}),
                ),
                (
                    200,
                    json!({"config": config, "owner_key": public_key(&fixture)}),
                ),
                (200, fixture["content_key_response"].clone()),
                (
                    200,
                    json!({"target": target, "policy_revision":1, "grant_revision":1, "readers":[{
                        "account_id":"account-alice", "username":"alice", "current_member":true,
                        "owner_default":true, "committer_default":true, "explicit_grant":false,
                        "revoked":false, "membership_invalidated":false, "can_read":true,
                        "session_explicit_grant":false,"session_revoked":false,"default_suppressed":false,"session_membership_invalidated":false,
                    }]}),
                ),
                (
                    200,
                    json!({"id":"frozen-preview", "target":target, "policy_revision":1, "grant_revision":1,
                    "members":[{"account_id":"account-alice", "username":"alice"},
                        {"account_id":"account-bob", "username":"bob"}],
                    "expires_at":"2999-01-01T00:00:00Z"}),
                ),
                (
                    200,
                    json!({"target":target,"policy_revision":1,"grant_revision":2}),
                ),
                (
                    200,
                    json!({"target":target,"policy_revision":1,"grant_revision":2}),
                ),
                (
                    200,
                    json!({"target":target,"policy_revision":1,"grant_revision":3}),
                ),
                (
                    409,
                    json!({"kind":"conflict", "error":"grant operation was superseded"}),
                ),
                (
                    200,
                    json!({"target":target,"policy_revision":1,"grant_revision":4}),
                ),
            ];
            let mut exchanges = Vec::new();
            for (index, response) in responses.into_iter().enumerate() {
                if (3..=6).contains(&index) {
                    exchanges.push((
                        412,
                        json!({"kind":"precondition_failed","error":"source renamed"}),
                    ));
                    exchanges.push((200, json!({"agent_id":snapshot.binding.repository_id,
                        "owner":"team","name":"renamed","clone_url":format!("{hub}/team/renamed.git")})));
                }
                exchanges.push(response);
            }
            exchanges
        },
        NO_STORE,
    );
    let client = Client::for_hub_with_token(anonymous.base(), "synthetic-account-token");
    let expected = AccountIdentity::new(client.base(), "account-alice").unwrap();
    assert!(client.ensure_session_key(&expected).unwrap().ready);
    let local = SessionPrivacy {
        scheme: crate::domain::session_privacy::SessionScheme::SessionOwnerV2,
        ..SessionPrivacy::new(Some(expected)).unwrap()
    };
    let registered = client
        .register_session_privacy("team/notes", &identity, &config.session_id, &local)
        .unwrap();
    assert_eq!(registered.config.privacy, local);
    assert_eq!(
        registered.require_owner_key().unwrap().account_id,
        "account-alice"
    );
    let loaded = client
        .registered_session_privacy("team/notes", &identity, &config.session_id)
        .unwrap()
        .unwrap();
    assert_eq!(loaded.config, registered.config);
    let snapshot: AcceptedSnapshot =
        serde_json::from_value(fixture["content_key_response"]["snapshot"].clone()).unwrap();
    let response = client
        .session_original_key("team/notes", &identity, &snapshot)
        .unwrap();
    response
        .open(
            &snapshot,
            fixture["envelope_bytes"].as_str().unwrap().as_bytes(),
        )
        .unwrap();
    let state = client
        .original_grants("team/notes", &identity, &target, 1)
        .unwrap();
    assert!(state.readers[0].owner_default && state.readers[0].can_read);
    let preview = client
        .preview_original_grants(
            "team/notes",
            &identity,
            &GrantTarget::from_snapshot(&snapshot).unwrap(),
            1,
        )
        .unwrap();
    let grant = preview.mutation(&target, 1).unwrap();
    for _ in 0..2 {
        assert_eq!(
            client
                .mutate_original_grants("team/notes", &identity, &grant)
                .unwrap()
                .grant_revision,
            2
        );
    }
    let revoke = GrantMutation::new(
        target.clone(),
        1,
        2,
        GrantOperation::Revoke {
            account_id: "account-bob".into(),
        },
    );
    client
        .mutate_original_grants("team/notes", &identity, &revoke)
        .unwrap();
    let superseded = client
        .mutate_original_grants("team/notes", &identity, &grant)
        .unwrap_err();
    assert_eq!(superseded.downcast_ref::<ApiError>().unwrap().status, 409);
    let stop = GrantMutation::new(target.clone(), 1, 3, GrantOperation::StopExplicit);
    client
        .mutate_original_grants("team/notes", &identity, &stop)
        .unwrap();
    let requests = worker.join().unwrap();
    for exchange in requests
        .windows(3)
        .filter(|exchange| exchange[1].starts_with("GET /api/repositories/"))
    {
        assert_eq!(
            exchange[0].split_once("\r\n\r\n").unwrap().1,
            exchange[2].split_once("\r\n\r\n").unwrap().1
        );
        assert_eq!(
            exchange[0].lines().next().unwrap(),
            exchange[2]
                .lines()
                .next()
                .unwrap()
                .replace("/team/renamed/", "/team/notes/")
        );
        for request in exchange {
            assert_eq!(
                request
                    .lines()
                    .filter(|line| line
                        .to_ascii_lowercase()
                        .starts_with("x-agentgit-expected-agent-id:"))
                    .count(),
                1
            );
            assert!(request.contains(&identity.agent_id));
        }
    }
    let requests: Vec<_> = requests
        .into_iter()
        .filter(|request| {
            !request.starts_with("GET /api/repositories/")
                && !request.lines().next().unwrap().contains("/team/renamed/")
        })
        .collect();
    for request in &requests {
        assert!(request.contains("Bearer synthetic-account-token"));
        assert!(
            !request.contains(
                fixture["content_key_response"]["content_key"]
                    .as_str()
                    .unwrap()
            )
        );
    }
    assert!(requests[0].starts_with("POST /api/me/session-key/ensure "));
    assert!(requests[1].starts_with(&format!(
        "POST /api/agents/team/notes/sessions/{}/privacy-config ",
        config.session_id
    )));
    let request: Value =
        serde_json::from_str(requests[1].split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(request["expected_repository_id"], identity.agent_id);
    assert!(requests[3].starts_with(&format!(
        "POST /api/agents/team/notes/sessions/{}/original-key ",
        config.session_id
    )));
    let request: Value =
        serde_json::from_str(requests[3].split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(
        request["snapshot"],
        fixture["content_key_response"]["snapshot"]
    );
    let body = |index: usize| -> Value {
        serde_json::from_str(requests[index].split_once("\r\n\r\n").unwrap().1).unwrap()
    };
    assert!(requests[4].starts_with(&format!(
        "GET /api/agents/team/notes/sessions/{}/original-grants?",
        config.session_id
    )));
    assert!(requests[4].contains("envelope_digest=sha256%3A"));
    assert_eq!(body(5)["target"]["agent_id"], identity.agent_id);
    assert_eq!(body(6), body(7));
    assert_eq!(body(6), body(9));
    assert_eq!(
        body(6)["target"]["published_commit_id"],
        snapshot.published_commit
    );
    assert_eq!(
        body(6)["target"]["envelope_digest"],
        snapshot.envelope_digest
    );
    assert_eq!(
        body(6)["action"],
        json!({"operation":"grant", "preview_id":"frozen-preview"})
    );
    assert_eq!(
        body(8)["action"],
        json!({"operation":"revoke", "account_id":"account-bob"})
    );
    assert_eq!(body(10)["action"], json!({"operation":"stop_explicit"}));
}

/// A mismatched or cacheable response cannot release keys, and service errors are not denials.
#[test]
fn key_delivery_rejects_rebinding_caching_and_preserves_error_classification() {
    let fixture = fixture();
    let snapshot: AcceptedSnapshot =
        serde_json::from_value(fixture["content_key_response"]["snapshot"].clone()).unwrap();
    let mut wrong = fixture["content_key_response"].clone();
    wrong["snapshot"]["published_commit"] = "f".repeat(40).into();
    let mut wrong_target =
        serde_json::to_value(GrantTarget::from_snapshot(&snapshot).unwrap()).unwrap();
    wrong_target["published_commit_id"] = "f".repeat(40).into();
    let (anonymous, identity, worker) = server_with_headers(
        |_| {
            vec![
                (200, wrong),
                (
                    403,
                    json!({"kind": "original_access_denied", "error": "no snapshot entitlement"}),
                ),
                (
                    503,
                    json!({"kind": "key_service_unavailable", "error": "try later"}),
                ),
                (
                    404,
                    json!({"kind":"session_privacy_unregistered","error":"register this session"}),
                ),
                (404, json!({"kind":"not_found","error":"route unavailable"})),
                (
                    404,
                    json!({"kind":"not_found","error":"source unavailable"}),
                ),
                (
                    200,
                    json!({"id":"wrong-preview", "target":wrong_target, "policy_revision":1, "grant_revision":1,
                    "members":[], "expires_at":"2999-01-01T00:00:00Z"}),
                ),
            ]
        },
        NO_STORE,
    );
    let client = Client::for_hub_with_token(anonymous.base(), "synthetic-account-token");
    let error = client
        .session_original_key("team/notes", &identity, &snapshot)
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("differs from the requested snapshot")
    );
    for (status, kind) in [
        (403, "original_access_denied"),
        (503, "key_service_unavailable"),
    ] {
        let error = client
            .session_original_key("team/notes", &identity, &snapshot)
            .unwrap_err();
        let error = error.downcast_ref::<ApiError>().unwrap();
        assert_eq!((error.status, error.kind.as_str()), (status, kind));
    }
    assert!(
        client
            .registered_session_privacy("team/notes", &identity, &snapshot.binding.session_id)
            .unwrap()
            .is_none()
    );
    assert!(
        client
            .registered_session_privacy("team/notes", &identity, &snapshot.binding.session_id)
            .is_err()
    );
    assert!(
        client
            .preview_original_grants(
                "team/notes",
                &identity,
                &GrantTarget::from_snapshot(&snapshot).unwrap(),
                1
            )
            .unwrap_err()
            .to_string()
            .contains("differs from the requested target")
    );
    worker.join().unwrap();
    let (anonymous, identity, worker) =
        server_with_headers(|_| vec![(200, fixture["content_key_response"].clone())], "");
    let client = Client::for_hub_with_token(anonymous.base(), "synthetic-account-token");
    let error = client
        .session_original_key("team/notes", &identity, &snapshot)
        .unwrap_err();
    assert!(error.to_string().contains("private, no-store"));
    worker.join().unwrap();
    let (anonymous, identity, worker) = server_with_headers(
        |_| vec![(307, json!({}))],
        "Location: http://127.0.0.1:1/another-authority\r\n",
    );
    let client = Client::for_hub_with_token(anonymous.base(), "synthetic-account-token");
    let error = client
        .session_original_key("team/notes", &identity, &snapshot)
        .unwrap_err();
    assert_eq!(error.downcast_ref::<ApiError>().unwrap().status, 307);
    worker.join().unwrap();
    let insecure = Client::for_hub_with_token("http://hub.test", "synthetic-account-token");
    let insecure_identity = RemoteIdentity::new(insecure.base(), &identity.agent_id).unwrap();
    assert!(
        insecure
            .session_original_key("team/notes", &insecure_identity, &snapshot)
            .unwrap_err()
            .to_string()
            .contains("requires HTTPS")
    );
}
