pub(crate) fn workspaces_url(hub: &str) -> String {
    format!("{}/workspaces", hub.trim().trim_end_matches('/'))
}
