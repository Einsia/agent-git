use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};

pub(super) fn configure(repo: &Repo, hub: &str, published: &PublicationReceipt) {
    unsafe {
        std::env::set_var("AGIT_HUB_URL", hub);
    }
    crate::infra::credentials::save(
        hub,
        &crate::infra::credentials::HubCredential {
            account_id: Some("account-1".into()),
            username: "owner".into(),
            email: None,
            hub: Some(hub.into()),
            access_token: "fixture-account-token".into(),
            access_expires_at: "2999-01-01T00:00:00Z".into(),
            refresh_token: "fixture-refresh".into(),
            refresh_expires_at: "2999-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    repo.set_auto_push(Some(true)).unwrap();
    // Ordinary automatic publication needs no saved consent, so only encrypted fixtures write one.
    if !published.mode.is_encrypted() {
        return;
    }
    let directory = repo.common_dir().unwrap().join("agit");
    std::fs::create_dir_all(&directory).unwrap();
    let mut policy = crate::domain::privacy::PrivacyPolicy::load(repo).unwrap();
    policy
        .mandatory
        .push(crate::domain::privacy::mandatory::MandatoryPolicy {
            version: 1,
            id: "hub-policy".into(),
            revision: crate::domain::privacy_envelope::digest_json(&json!({
                "version":1, "hub":hub, "account_id":"account-1", "repository":"owner/project",
                "owner_id":"owner-1", "revision":"revision-1", "sources":[],
            }))
            .unwrap(),
            exclude: vec![],
            memory_exclude: vec![],
        });
    std::fs::write(
        directory.join("privacy-auto-consent.json"),
        serde_json::to_vec(&json!({
            "version":1, "hub":hub, "account":"owner", "account_id":"account-1",
            "agent_id":published.destination.agent_id, "url":published.url, "visibility":"private",
            "policy_digest":policy.digest().unwrap(), "recipient":published.recipient,
        }))
        .unwrap(),
    )
    .unwrap();
}

pub(super) fn response(
    hub: &str,
    request: &str,
    body: &[u8],
    encryption_enabled: bool,
) -> Option<serde_json::Value> {
    let mut parts = request.split_whitespace();
    let (method, target) = (parts.next()?, parts.next()?);
    match (method, target) {
        ("GET", "/api/auth/me") => Some(json!({"account_id":"account-1", "username":"owner"})),
        ("GET", "/api/agents/owner/project") => Some(json!({
            "agent_id":"00000000-0000-0000-0000-000000000002", "owner":"owner", "name":"project",
            "clone_url":format!("{hub}/owner/project.git"), "visibility":"private", "encryption_enabled":encryption_enabled,
        })),
        ("GET", "/owner/project.git/info/refs?service=git-receive-pack") => Some(json!({})),
        ("GET", "/api/agents/owner/project/privacy/publishing-key") => {
            let public = STANDARD.encode(
                crypto_box::SecretKey::from([23; 32])
                    .public_key()
                    .as_bytes(),
            );
            Some(
                json!({"agent_id":"00000000-0000-0000-0000-000000000002", "config_version":1,
                "current":{"recipient":crate::domain::privacy_key::recipient_id(&public), "public_key_algorithm":"x25519", "public_key":public}}),
            )
        }
        ("POST", "/api/privacy/policy-sources/resolve") => {
            let request: serde_json::Value = serde_json::from_slice(body).unwrap();
            let now = chrono::Utc::now();
            Some(
                json!({"version":1, "hub":hub, "repository":request["repository"],
                "agent_id":request["agent_id"], "request_id":request["request_id"],
                "account_id":"account-1", "owner_id":"owner-1", "revision":"revision-1",
                "issued_at":now, "expires_at":now + chrono::Duration::minutes(5), "sources":[]}),
            )
        }
        _ => None,
    }
}
