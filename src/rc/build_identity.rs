//! Immutable build and process identities for local daemon negotiation.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const BUILD_ID: &str = env!("AGIT_BUILD_ID");
pub const RPC_FEATURES: &[&str] = &[
    "peer-control-v1",
    "terminal-open-idempotency-v1",
    "terminal-input-idempotency-v1",
    "history-v2",
    "archive-history-tail-v1",
    "safe-restart-v1",
    "local-settlement-v1",
    "publication-delivery-v1",
    "session-publication-status-v1",
    "session-permissions-v1",
    "session-permission-capabilities-v1",
    "session-approvals-v1",
    #[cfg(unix)]
    "native-claude-control-v1",
    "codex-turn-interrupt-fence-v1",
    "project-publication-bind-v1",
    "project_bind_create",
    "source-catalog-v1",
    "source-catalog-resolve-v1",
    "source-catalog-delta-v1",
    "session-share-v1",
    // `session.resume` takes `attach_only` (join a running conversation, never launch one) and
    // replies `launched: true` when it started the runtime.
    "session-resume-attach-only-v1",
    // Watching a conversation, joining it (attach_only), and taking over or resuming it save and
    // upload it only as the folder it runs in allows: never one the person dropped or that ran
    // before the folder was on, nothing while the folder is off or doesn't upload by itself, and
    // with no folder only inside a project an RC workspace binds. Queued turns are judged again
    // before they are saved, and every automatic push is held to the same rule.
    "observed-capture-consent-v1",
    // session.start takes origin project_share: a project the owner shared with a group, started
    // only in Claude Code and never for the owner, and recorded as such through resume and
    // takeover. Any other origin is refused (`rc::project_share`).
    "project-share-v1",
    // A message a non-owner leaves waiting for the next turn is delivered only while that account
    // keeps sending this session admitted requests, or `session.enqueue.keep`; one left longer
    // than the limit without either is dropped as access_ended.
    "queued-keep-v1",
    // session.resume takes `permission_mode`, the mode a conversation it launches runs in; only
    // the owner may name one looser than the mode it would otherwise run in, judged as
    // `session.setPermissionMode` judges a switch.
    "resume-permission-mode-v1",
    // `fs.readSessionImage` answers anyone who may read a conversation with an image one of its
    // agent replies shows, from inside its folder (`rc::local_history::session_image`).
    "session-image-read-v1",
    // The device sends `session.list.changed {workspace_id, project_id}` when a project's list
    // changes, to each connection whose own `session.list` shows a row that changed; a page that
    // sees this feature reads the list on the event instead of on a timer.
    "session-list-changed-v1",
    // A session controller grant may carry `delivery_only`: the device then admits only
    // `session.publication.deliver` for that session on the connection, sends it no events, and
    // takes no ownership of the session from the controller that holds it.
    "publication-delivery-only-v1",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonIdentity {
    pub instance_id: String,
    pub build_id: String,
    pub executable: PathBuf,
    pub rpc_features: Vec<String>,
}

impl DaemonIdentity {
    pub fn current() -> std::io::Result<Self> {
        Ok(Self {
            instance_id: uuid::Uuid::new_v4().to_string(),
            build_id: BUILD_ID.into(),
            executable: std::env::current_exe()?,
            rpc_features: RPC_FEATURES
                .iter()
                .map(|feature| (*feature).into())
                .collect(),
        })
    }
}
