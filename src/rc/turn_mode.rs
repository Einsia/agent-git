//! Which modes a non-owner's words may run in.
//!
//! A non-owner (anyone the device does not see as the owner, an owner's control-ceiling token
//! included) may put words into a turn only when that turn runs in Default or Plan. Auto and
//! AcceptEdits run tools the owner chose to stop approving, Bypass runs with no checks, and a
//! mode the device cannot read may be any of them: each is the owner's alone. The daemon asks
//! at the RPC, and the supervisor asks again against the driver's mode when it delivers.

use crate::protocol::{ErrorCode, PermissionMode, RpcError};

/// Whether a non-owner's words may enter a turn that runs in `mode`. `None` is a mode the
/// device cannot read, and fails closed.
pub(crate) fn non_owner_may_add(mode: Option<PermissionMode>) -> bool {
    matches!(mode, Some(PermissionMode::Default | PermissionMode::Plan))
}

/// A non-owner's words were refused because of the mode the turn runs in. Nothing was sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ModeOwnerOnly {
    pub mode: Option<PermissionMode>,
}

/// The refusal's text when the mode can't be read: fixed, naming no mode.
const UNREAD: &str = "Only its owner can send to this conversation in its current mode.";

impl ModeOwnerOnly {
    /// The refusal names the mode and nothing else about the conversation; when the mode can't
    /// be read, it names no mode at all.
    pub(crate) fn rpc_error(self) -> RpcError {
        let message = match self.mode {
            Some(mode) => format!(
                "this conversation runs in {}; only its owner may continue it in that mode",
                name(mode)
            ),
            None => UNREAD.to_owned(),
        };
        let mut error = RpcError::new(ErrorCode::Forbidden, message).with_hint(
            "nothing was sent; the owner can set it to Default or Plan, or continue it themselves",
        );
        let mut data = serde_json::json!({
            "retryable": false,
            "outcome": "not_sent",
            "reason": "mode_owner_only",
        });
        // The mode as the protocol spells it (`accept_edits`, `auto`, `bypass`), so a client can
        // word the refusal itself; absent when the mode can't be read.
        if let Some(mode) = self.mode {
            data["mode"] = serde_json::json!(mode);
        }
        error.data = Some(data);
        error
    }
}

impl std::fmt::Display for ModeOwnerOnly {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.rpc_error().message)
    }
}

impl std::error::Error for ModeOwnerOnly {}

/// Refuse `caller_is_owner == false` unless the turn runs in Default or Plan.
pub(crate) fn require(
    caller_is_owner: bool,
    mode: Option<PermissionMode>,
) -> Result<(), ModeOwnerOnly> {
    if caller_is_owner || non_owner_may_add(mode) {
        return Ok(());
    }
    Err(ModeOwnerOnly { mode })
}

fn name(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Default => "Default",
        PermissionMode::AcceptEdits => "Accept edits",
        PermissionMode::Auto => "Auto",
        PermissionMode::Plan => "Plan",
        PermissionMode::Bypass => "Bypass",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the rule itself: a non-owner adds words only to a Default or Plan turn; Auto,
    /// AcceptEdits, Bypass and a mode the device cannot read are refused, `not_sent`, with reason
    /// `mode_owner_only` and a message naming only the mode. The owner is never refused.
    #[test]
    fn a_non_owner_adds_words_only_to_a_default_or_plan_turn() {
        use PermissionMode as M;
        for mode in [M::Default, M::Plan] {
            assert!(require(false, Some(mode)).is_ok(), "{mode:?}");
        }
        for (mode, spelled) in [
            (Some(M::Auto), Some("auto")),
            (Some(M::AcceptEdits), Some("accept_edits")),
            (Some(M::Bypass), Some("bypass")),
            (None, None),
        ] {
            let error = require(false, mode).unwrap_err().rpc_error();
            assert_eq!(error.code, ErrorCode::Forbidden as i32, "{mode:?}");
            let mut data = serde_json::json!({"retryable": false, "outcome": "not_sent", "reason": "mode_owner_only"});
            if let Some(spelled) = spelled {
                data["mode"] = serde_json::json!(spelled);
            }
            assert_eq!(error.data, Some(data), "{mode:?}");
            assert!(require(true, mode).is_ok(), "{mode:?}");
        }
        let unread = require(false, None).unwrap_err().rpc_error();
        assert_eq!(
            unread.message,
            "Only its owner can send to this conversation in its current mode."
        );
        assert!(unread.data.as_ref().unwrap().get("mode").is_none());
        assert_eq!(
            require(false, Some(M::Auto))
                .unwrap_err()
                .rpc_error()
                .message,
            "this conversation runs in Auto; only its owner may continue it in that mode"
        );
    }

    /// Pins that this build advertises `nonowner-turn-mode-v1` in `rpc_features`. The Hub refuses
    /// a non-owner's native-inbox message on an executor that does not promise this rule, so a
    /// build that dropped the advert would lose those messages, and one that kept the advert
    /// while dropping the rule would let them into a turn the owner set to act without asking.
    #[test]
    fn the_non_owner_turn_mode_rule_is_advertised() {
        let identity = crate::rc::build_identity::DaemonIdentity::current().unwrap();
        assert!(
            identity
                .rpc_features
                .iter()
                .any(|feature| feature == "nonowner-turn-mode-v1"),
            "{:?}",
            identity.rpc_features
        );
    }
}
