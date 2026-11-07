//! A project the workspace owner shared with a group (`params.origin = "project_share"`, set only
//! by the Hub): each mention starts a new Claude Code session in that project on the owner's
//! computer, run for someone who is not the owner.
//!
//! The session is recorded as a share and stays one through resume and takeover.

use crate::protocol::{ErrorCode, RpcError};

/// The only origin a session start may carry.
pub(crate) const ORIGIN: &str = "project_share";

/// Whether a session start is a share session: `Ok(false)` without an origin, `Ok(true)` for
/// [`ORIGIN`], and a refusal for any other origin.
pub(crate) fn origin(origin: Option<&str>) -> Result<bool, RpcError> {
    match origin {
        None => Ok(false),
        Some(ORIGIN) => Ok(true),
        Some(_) => Err(refused(
            "this device doesn't know where this session comes from; nothing was started",
            "origin_unknown",
        )),
    }
}

/// A share session start refused before anything is launched: `reason` names the rule.
pub(crate) fn refused(message: &str, reason: &str) -> RpcError {
    let mut error = RpcError::new(ErrorCode::Forbidden, message);
    error.data = Some(serde_json::json!({
        "outcome": "not_sent",
        "retryable": false,
        "reason": reason,
    }));
    error
}

#[cfg(test)]
mod tests {
    /// The Hub starts a shared project only on a device that says it takes the origin: a device
    /// that ignored it would start an ordinary session for the owner.
    #[test]
    fn this_device_says_it_takes_shared_projects() {
        let features = crate::rc::build_identity::DaemonIdentity::current()
            .unwrap()
            .rpc_features;
        assert!(
            features.iter().any(|feature| feature == "project-share-v1"),
            "{features:?}"
        );
    }
}
