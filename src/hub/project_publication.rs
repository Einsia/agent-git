//! Publication plans use current account authority and immutable device identities.

use crate::protocol::ProjectPublicationBind;
use agit_peer::cloud::Device;
use serde::Deserialize;

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub(crate) struct Target {
    pub web_workspace_id: String,
    pub web_project_id: String,
    pub workspace_id: String,
    pub project_id: String,
    pub local_path: String,
    pub repository: String,
    pub repository_id: String,
    pub encryption_enabled: bool,
}

impl Target {
    pub(crate) fn request(&self) -> ProjectPublicationBind {
        ProjectPublicationBind {
            workspace_id: self.workspace_id.clone(),
            project_id: self.project_id.clone(),
            local_path: self.local_path.clone(),
            repository: self.repository.clone(),
            repository_id: self.repository_id.clone(),
            encryption_enabled: self.encryption_enabled,
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct Page {
    pub device: Device,
    pub entries: Vec<Target>,
    pub next_cursor: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct Current {
    pub device: Device,
    pub entry: Target,
}

impl super::Client {
    pub(crate) fn project_publication_plan(
        &self,
        device: &str,
        after: Option<&str>,
    ) -> crate::Result<Page> {
        let mut path = format!(
            "api/peer/devices/{}/project-publications",
            super::client::urlencode(device)
        );
        if let Some(after) = after {
            path.push_str(&format!("?after={}", super::client::urlencode(after)));
        }
        self.get(&path)
    }

    pub(crate) fn current_project_publication(&self, target: &Target) -> crate::Result<Current> {
        self.get(&format!(
            "api/workspaces/{}/projects/{}/publication",
            super::client::urlencode(&target.web_workspace_id),
            super::client::urlencode(&target.web_project_id)
        ))
    }
}
