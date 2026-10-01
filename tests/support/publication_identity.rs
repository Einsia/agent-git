//! Mock identity routes shared by session publication fixtures.

use serde_json::{Value, json};

pub fn route(
    hub: &str,
    username: &str,
    method: &str,
    target: &str,
    _body: &[u8],
) -> Option<(u16, Value)> {
    match (method, target) {
        ("GET", "/api/auth/me") => Some((
            200,
            json!({
                "account_id":"account-1", "username":username,
            }),
        )),
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
