use crate::protocol::HubWorkspace;

pub(crate) fn workspaces_url(hub: &str) -> String {
    format!("{}/workspaces", hub.trim().trim_end_matches('/'))
}

pub(crate) fn connected_guidance(hub: &str, workspaces: &[HubWorkspace]) -> Vec<String> {
    let index = workspaces_url(hub);
    if workspaces.is_empty() {
        return vec![format!(
            "agitd: create a workspace at {index}; confirm the dialog names this machine, cancel if it does not, then bind your project folder"
        )];
    }
    let mut lines = vec![format!("agitd: manage workspaces at {index}")];
    for workspace in workspaces {
        if let Ok(id) = uuid::Uuid::parse_str(&workspace.workspace_id) {
            lines.push(format!("agitd: open workspace {index}/{id}/live"));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_links_preserve_the_hub_route_and_reject_non_id_paths() {
        let workspace = |id: &str| HubWorkspace {
            workspace_id: id.into(),
            name: "synthetic workspace".into(),
            projects: vec![],
        };
        let id = "00000000-0000-0000-0000-000000000001";
        let lines = connected_guidance(
            "https://hub.test:8443/base/",
            &[workspace(id), workspace("../outside?token=synthetic")],
        );
        assert!(lines.iter().any(|line| {
            line.ends_with(&format!("https://hub.test:8443/base/workspaces/{id}/live"))
        }));
        assert!(lines.iter().all(|line| !line.contains("synthetic")));
        assert!(
            connected_guidance("https://hub.test/", &[])[0]
                .contains("create a workspace at https://hub.test/workspaces")
        );
    }
}
