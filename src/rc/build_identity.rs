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
    "source-catalog-v1",
    "source-catalog-resolve-v1",
    "source-catalog-delta-v1",
    "session-share-v1",
    // Taking over, sharing, resuming or messaging a Claude Code or Codex conversation reads the
    // transcript's own permission mode, so one started in a terminal without permission checks
    // stays owner-only to drive; a non-owner's resume starts no looser than Default; and a
    // terminal attached for native control stays owner-only to drive, its mode being unknown.
    "resume-caller-gate-v1",
    // `session.resume` takes `attach_only` (join a running conversation, never launch one) and
    // replies `launched: true` when it started the runtime.
    "session-resume-attach-only-v1",
    // A non-owner (any caller the device doesn't see as the owner) may add words to a turn only
    // when it runs in Default or Plan: turn.start, turn.steer, session.enqueue (live and native),
    // queued next-turn delivery, approval notes, and the source and inbox paths. Control-ceiling
    // tokens count as non-owners: the Hub's narrow() forwards them as role operator, and
    // is_owner() is role == "owner". A mode that can't be read is refused, and a Claude Code
    // terminal's mode is never read. Checked at the RPC and again at delivery (a non-owner's
    // approval note is replaced at delivery, in every mode). ExitPlanMode is not covered.
    "nonowner-turn-mode-v1",
    // A non-owner's turn can't start work that outlives it, in any mode, in a Claude Code or
    // OpenCode session agit drives; a turn the device can't attribute is held to the same rule.
    "nonowner-selfstart-refused-v1",
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
