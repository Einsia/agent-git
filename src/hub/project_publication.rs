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
    #[serde(default)]
    pub encryption_enabled: Option<bool>,
    /// The account that made this folder binding on the web; absent for a binding made before
    /// the Hub recorded it.
    #[serde(default)]
    pub bound_by: Option<String>,
    /// Whether the device's account is the only account that can bind folders in the workspace.
    #[serde(default)]
    pub sole_binder: Option<bool>,
}

impl Target {
    /// Whether this binding is the device's own account's choice, `account` as the device's
    /// sign-in names it: made by that account, or, for a binding older than the Hub's record of
    /// who made it, in a workspace where no other account can bind a folder.
    pub(crate) fn bound_by_account(&self, account: &str) -> bool {
        match &self.bound_by {
            Some(bound_by) => bound_by == account,
            None => self.sole_binder == Some(true),
        }
    }
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
