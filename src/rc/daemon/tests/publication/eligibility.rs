use super::*;

pub(super) fn configure(repo: &Repo, hub: &str) {
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
}

pub(super) fn response(hub: &str, request: &str) -> Option<serde_json::Value> {
    let mut parts = request.split_whitespace();
    let (method, target) = (parts.next()?, parts.next()?);
    match (method, target) {
        ("GET", "/api/auth/me") => Some(json!({"account_id":"account-1", "username":"owner"})),
        ("GET", "/api/agents/owner/project") => Some(json!({
            "agent_id":"00000000-0000-0000-0000-000000000002", "owner":"owner", "name":"project",
            "clone_url":format!("{hub}/owner/project.git"), "visibility":"private",
        })),
        ("GET", "/owner/project.git/info/refs?service=git-receive-pack") => Some(json!({})),
        _ => None,
    }
}
