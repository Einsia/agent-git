//! Typed mock-Hub discovery shared by session publication fixtures.

use serde_json::{Value, json};

pub fn route(
    hub: &str,
    username: &str,
    method: &str,
    target: &str,
    body: &[u8],
) -> Option<(u16, Value)> {
    match (method, target) {
        ("GET", "/api/auth/me") => Some((
            200,
            json!({
                "account_id":"account-1", "username":username,
            }),
        )),
        ("POST", "/api/privacy/policy-sources/resolve") => {
            let request: Value = serde_json::from_slice(body).unwrap();
            assert_eq!(request["version"], 1);
            let now = chrono::Utc::now();
            Some((
                200,
                json!({
                    "version":1, "hub":hub,
                    "repository":request["repository"], "agent_id":request["agent_id"],
                    "account_id":"account-1", "request_id":request["request_id"],
                    "owner_id":"owner-1", "revision":"revision-1",
                    "issued_at":now, "expires_at":now + chrono::Duration::minutes(5),
                    "sources":[],
                }),
            ))
        }
        ("GET", target) if target.starts_with("/api/agents/") && !target.contains("/privacy/") => {
            let slug = target.strip_prefix("/api/agents/").unwrap();
            let (owner, name) = slug.split_once('/').unwrap();
            assert!(!name.contains('/'));
            Some((
                200,
                json!({
                    "agent_id":"00000000-0000-0000-0000-000000000001", "owner":owner, "name":name,
                    "clone_url":format!("{hub}/{slug}.git"), "visibility":"private",
                }),
            ))
        }
        _ => None,
    }
}
