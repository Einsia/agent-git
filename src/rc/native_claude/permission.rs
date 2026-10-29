//! Answer Claude Code's permission asks from shared control through a settings PermissionRequest
//! hook. Where an organization's security default is seated, Claude Code passes user plugins
//! over on its classic events, so the module never hears these asks; a settings hook still does.
//!
//! The hook decides only with a person's answer to its own ask. Anything else, such as no
//! registration for its session, a subagent's ask, an unreachable or slow daemon, or a malformed
//! reply, decides nothing, and the terminal's dialog, which stays up while the hook waits,
//! decides alone.

use super::Registration;
use serde_json::{Value, json};
use std::time::Duration;

/// How often a waiting hook checks in with the daemon; well inside the lease on its ask.
const CHECK_IN: Duration = Duration::from_secs(1);

/// The decision for a PermissionRequest hook `payload`, or `None` to leave the ask to the
/// terminal.
pub(crate) fn decide(payload: &Value) -> Option<Value> {
    if payload
        .get("agent_id")
        .is_some_and(|agent| !agent.is_null())
    {
        return None;
    }
    let session = payload["session_id"].as_str()?;
    decide_for(&super::ancestor_registration(session)?, payload)
}

pub(super) fn decide_for(registration: &Registration, payload: &Value) -> Option<Value> {
    let tool = payload["tool_name"]
        .as_str()
        .filter(|tool| !tool.is_empty())?;
    let id = uuid::Uuid::new_v4().to_string();
    let body = json!({
        "session": registration.session, "generation": registration.generation,
        "cwd": registration.cwd, "id": id, "tool": tool, "input": payload["tool_input"],
        "permission_mode": payload["permission_mode"],
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    runtime.block_on(async {
        let mut asked_in = None;
        while registration.is_registered() {
            let reply = tokio::time::timeout(
                Duration::from_secs(2),
                super::approval::post(registration, "/permission", &body),
            )
            .await;
            // A reply with no turn is from a daemon that has not taken the ask yet.
            if let Ok(Ok(reply)) = reply
                && reply["generation"] == registration.generation.as_str()
                && reply["id"] == id.as_str()
                && !reply["turn"].is_null()
            {
                // An answer counts only for the turn the ask was first taken in; a daemon that
                // lost the ask and took it again in a later turn holds a different question.
                if *asked_in.get_or_insert_with(|| reply["turn"].clone()) != reply["turn"] {
                    return None;
                }
                if !reply["decision"].is_null() {
                    return permitted(&reply["decision"]);
                }
            }
            tokio::time::sleep(CHECK_IN).await;
        }
        None
    })
}

/// What of an answer from shared control the hook hands Claude Code: allow, or deny with the
/// person's message when it is within the bound a native message has, and nothing else. A field such as `updatedPermissions` would add standing
/// rules on this computer, and `updatedInput` would rewrite the call the person answered; an
/// allow that carries `updatedInput` (a question answered there) decides nothing, since allowing
/// it without its answers would run the question unanswered, and the terminal asks instead.
fn permitted(decision: &Value) -> Option<Value> {
    match decision["behavior"].as_str()? {
        "allow" if decision.get("updatedInput").is_none() => Some(json!({"behavior": "allow"})),
        "deny" => Some(
            match decision["message"]
                .as_str()
                .filter(|message| message.len() <= crate::rc::native_inbox::MAX_MESSAGE)
            {
                Some(message) => json!({"behavior": "deny", "message": message}),
                None => json!({"behavior": "deny"}),
            },
        ),
        _ => None,
    }
}

/// Reports the session's live permission mode, which its settings hooks see and its module may
/// not.
pub(crate) fn report_mode(session: &str, mode: &str) {
    if let Some(registration) = super::ancestor_registration(session) {
        report_mode_to(&registration, mode);
    }
}

pub(super) fn report_mode_to(registration: &Registration, mode: &str) {
    let body = json!({
        "session": registration.session, "generation": registration.generation,
        "cwd": registration.cwd, "permission_mode": mode,
    });
    if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        // The timer needs the runtime it is created in, so it is created inside it.
        let _ = runtime.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(1),
                super::approval::post(registration, "/mode", &body),
            )
            .await
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hook passes on only allow, or deny with its message within bound. A hook that passed
    /// the daemon's answer through would hand Claude Code standing permission rules, a rewritten
    /// tool call or an unbounded message, and one that reduced an answered question to a bare
    /// allow would run it unanswered.
    #[test]
    fn only_an_allow_or_a_deny_with_its_message_leaves_the_hook() {
        let rules =
            json!([{"type": "addRules", "rules": [{"toolName": "Bash"}], "behavior": "allow"}]);
        assert_eq!(
            permitted(&json!({"behavior": "allow", "updatedPermissions": rules})),
            Some(json!({"behavior": "allow"}))
        );
        assert_eq!(
            permitted(
                &json!({"behavior": "deny", "message": "not now", "updatedPermissions": rules,
                "updatedInput": {"command": "true"}, "interrupt": true})
            ),
            Some(json!({"behavior": "deny", "message": "not now"}))
        );
        assert_eq!(
            permitted(&json!({"behavior": "allow", "updatedInput": {"command": "true"}})),
            None
        );
        let long = "x".repeat(crate::rc::native_inbox::MAX_MESSAGE + 1);
        assert_eq!(
            permitted(&json!({"behavior": "deny", "message": long})),
            Some(json!({"behavior": "deny"}))
        );
        assert_eq!(permitted(&json!({"behavior": "ask"})), None);
    }
}
