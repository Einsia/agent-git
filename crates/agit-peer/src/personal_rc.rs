//! A personal bot's saved mode authorizes one launch, never its controller connection.

use crate::{access::Principal, cloud::ConnectionGrant};
use anyhow::ensure;
use serde::{Deserialize, Serialize};

pub const VERIFY_PATH: &str = "/api/peer/personal-rc/verify-start";
pub const FEATURE: &str = "personal-rc-start-mode-v1";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartScope {
    pub authorization_id: String,
    pub project_id: String,
    pub runtime: String,
    pub start_id: String,
    pub permission_mode: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartAuthorization {
    pub scope: StartScope,
    pub grant_id: String,
    pub caller: Principal,
    pub source_id: String,
    pub target_id: String,
    pub workspace_id: String,
    pub project_path: String,
    pub bot_id: String,
    pub token_id: String,
    pub sender_id: String,
    pub expires_at_ms: i64,
}

impl StartAuthorization {
    pub fn validate(
        &self,
        grant: &ConnectionGrant,
        scope: &StartScope,
        now: i64,
    ) -> anyhow::Result<()> {
        ensure!(
            self.scope == *scope
                && self.grant_id == grant.id
                && self.caller == grant.caller
                && self.caller == grant.target.owner
                && self.caller == grant.source.owner
                && self.source_id == grant.source.id
                && self.target_id == grant.target.id
                && grant.session_controller.is_none()
                && grant.project_controller.is_none()
                && !self.workspace_id.is_empty()
                && !self.project_path.is_empty()
                && !self.bot_id.is_empty()
                && !self.token_id.is_empty()
                && !self.sender_id.is_empty()
                && matches!(scope.runtime.as_str(), "codex" | "claude-code" | "opencode")
                && matches!(
                    scope.permission_mode.as_str(),
                    "default" | "auto" | "accept_edits" | "bypass"
                )
                && self.expires_at_ms > now
                && self.expires_at_ms <= grant.expires_at_ms,
            "personal RC launch authorization does not match the request"
        );
        Ok(())
    }
}
