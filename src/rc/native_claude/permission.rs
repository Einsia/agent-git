//! Answer Claude Code's permission asks from shared control through a settings PermissionRequest
//! hook. Where an organization's security default is seated, Claude Code passes user plugins
//! over on its classic events, so the module never hears these asks; a settings hook still does.
//!
//! The hook decides only with a person's answer to its own ask. Anything else, such as no
//! registration for its session, a subagent's ask, an unreachable or slow daemon, or a malformed
//! reply, decides nothing, and the terminal's dialog, which stays up while the hook waits,
//! decides alone.
//!
//! Claude Code leaves the hook running when the terminal answers, so the hook also leaves once
//! the daemon says its ask is gone (the session moved past it) or once the process that ran it
//! is gone. A hook that kept checking in would keep the answered ask pending for as long as the
//! session lives.

use super::{Process, Registration};
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
        "permission_mode": payload["permission_mode"], "tool_use": payload["tool_use_id"],
    });
    let parent = super::process(std::process::id())
        .and_then(|(_, parent)| super::process(parent))
        .map(|(parent, _)| parent);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    runtime.block_on(async {
        let mut asked_in = None;
        while registration.is_registered() && present(parent.as_ref()) {
            let reply = tokio::time::timeout(
                Duration::from_secs(2),
                super::approval::post(registration, "/permission", &body),
            )
            .await;
            // A reply with no turn is from a daemon that has not taken the ask yet.
            if let Ok(Ok(reply)) = &reply
                && reply["generation"] == registration.generation.as_str()
                && reply["id"] == id.as_str()
                && reply["gone"] == true
            {
                return None;
            }
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
                    return permitted(&reply["decision"], payload);
                }
            }
            tokio::time::sleep(CHECK_IN).await;
        }
        None
    })
}

/// Whether `parent`, the process that ran the hook as it was when the hook started, still runs.
/// A parent that could not be read at the start does not end the wait.
fn present(parent: Option<&Process>) -> bool {
    parent.is_none_or(|parent| {
        super::process(parent.pid).is_some_and(|(current, _)| current == *parent)
    })
}

/// What of an answer from shared control the hook hands Claude Code for the ask in `payload`:
/// allow, or deny with the person's message when it is within the bound a native message has, and
/// nothing else. A field such as `updatedPermissions` would add standing rules on this computer,
/// and `updatedInput` would rewrite the call the person answered, so neither is passed on.
///
/// The one exception is a question answered there (AskUserQuestion): its allow carries the
/// answers, and allowing it without them would run the question unanswered. The hook builds that
/// input itself, as [`answered`] says; an allow with `updatedInput` it can't build, or for any
/// other tool, decides nothing, and the terminal asks instead.
fn permitted(decision: &Value, payload: &Value) -> Option<Value> {
    match decision["behavior"].as_str()? {
        "allow" if decision.get("updatedInput").is_none() => Some(json!({"behavior": "allow"})),
        "allow" if payload["tool_name"] == "AskUserQuestion" => {
            let input = answered(&payload["tool_input"], &decision["updatedInput"])?;
            Some(json!({"behavior": "allow", "updatedInput": input}))
        }
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

/// The input Claude Code gave this hook for its question, `asked`, unchanged but for the answers
/// in `decided`, the input shared control sent back; `None` unless every answer holds. Nothing
/// else from shared control reaches Claude Code: questions that differ from the hook's own
/// answer a question nobody asked here. Each question asked has exactly one answer, keyed by its
/// text, with no other keys, and the answers together stay within a few native messages' bound.
///
/// A single-select answer is one string, not blank and within the bound a native message has. A
/// multiSelect answer is its choices joined by ", ", as shared control joins them, and each
/// choice is checked the same way, up to a fixed number of them. Whether a question is
/// multiSelect comes from the hook's own input, never from the answer. A question the tool asks
/// always allows free text, so an answer need not be one of its option labels.
fn answered(asked: &Value, decided: &Value) -> Option<Value> {
    let questions = asked["questions"].as_array().filter(|q| !q.is_empty())?;
    if decided["questions"] != asked["questions"] {
        return None;
    }
    let answers = decided["answers"].as_object()?;
    if answers.len() != questions.len() {
        return None;
    }
    let fits = |value: &str| {
        !value.trim().is_empty() && value.len() <= crate::rc::native_inbox::MAX_MESSAGE
    };
    let mut total = 0usize;
    for question in questions {
        let answer = answers.get(question["question"].as_str()?)?.as_str()?;
        let held = if question["multiSelect"] == true {
            let choices: Vec<&str> = answer.split(", ").collect();
            choices.len() <= MAX_CHOICES && choices.iter().all(|choice| fits(choice))
        } else {
            fits(answer)
        };
        if !held {
            return None;
        }
        total += answer.len();
    }
    if total > MAX_ANSWERS {
        return None;
    }
    let mut input = asked.clone();
    input
        .as_object_mut()?
        .insert("answers".into(), Value::Object(answers.clone()));
    Some(input)
}

/// The most the answers to one ask may hold together: a few native messages' worth.
const MAX_ANSWERS: usize = 4 * crate::rc::native_inbox::MAX_MESSAGE;

/// The most choices one multiSelect answer may name.
const MAX_CHOICES: usize = 64;

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
        let bash = json!({"tool_name": "Bash", "tool_input": {"command": "ls"}});
        let permitted = |decision: &Value| permitted(decision, &bash);
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

    /// A question answered in shared control reaches Claude Code with the hook's own question and
    /// the answers only, and only when every answer holds. A hook that passed shared control's
    /// input through would let it rewrite the question; one that took any answers would hand Claude
    /// Code an extra key, a missing answer or an unbounded value; and one that let any tool's
    /// `updatedInput` through would let shared control rewrite a command.
    #[test]
    fn a_question_reaches_claude_with_its_own_input_and_checked_answers() {
        let questions = json!([
            {"question": "Which color?", "header": "Color", "multiSelect": false,
             "options": [{"label": "Red", "description": "warm"}, {"label": "Blue", "description": "cool"}]},
            {"question": "Which sizes?", "header": "Size", "multiSelect": true,
             "options": [{"label": "S", "description": ""}, {"label": "M", "description": ""}]},
        ]);
        let asked = json!({"tool_name": "AskUserQuestion",
            "tool_input": {"questions": questions, "metadata": {"source": "native"}}});
        let allow = |input: Value| {
            json!({"behavior": "allow", "updatedInput": input,
            "updatedPermissions": [{"type": "addRules"}]})
        };
        let answers = json!({"Which color?": "Blue", "Which sizes?": "S, M"});
        let decided = |answers: &Value| {
            permitted(
                &allow(
                    json!({"questions": questions, "metadata": {"source": "web"},
                    "answers": answers, "extra": true}),
                ),
                &asked,
            )
        };
        assert_eq!(
            decided(&answers),
            Some(
                json!({"behavior": "allow", "updatedInput": {"questions": questions,
                "metadata": {"source": "native"}, "answers": answers}})
            )
        );
        // Free text is an answer too.
        assert!(decided(&json!({"Which color?": "Green, please", "Which sizes?": "M"})).is_some());
        // Questions other than the hook's own decide nothing.
        let mut altered = questions.clone();
        altered[0]["question"] = json!("Which colour?");
        assert_eq!(
            permitted(
                &allow(
                    json!({"questions": altered, "answers": {"Which colour?": "Blue", "Which sizes?": "S"}})
                ),
                &asked,
            ),
            None
        );
        // Only the hook's own input says a question is multiSelect; each choice is checked.
        assert!(decided(&json!({"Which color?": "Blue", "Which sizes?": "S, "})).is_none());
        let many = vec!["S"; MAX_CHOICES + 1].join(", ");
        assert!(decided(&json!({"Which color?": "Blue", "Which sizes?": many})).is_none());
        for wrong in [
            json!({"Which color?": "Blue"}),
            json!({"Which color?": "Blue", "Which sizes?": "S", "Other": "x"}),
            json!({"Which color?": "Blue", "Which sizes?": "  "}),
            json!({"Which color?": ["Blue"], "Which sizes?": "S"}),
            json!({"Which color?": "x".repeat(crate::rc::native_inbox::MAX_MESSAGE + 1),
                "Which sizes?": "S"}),
        ] {
            assert_eq!(decided(&wrong), None, "{wrong}");
        }
        // Any other tool's input still decides nothing.
        assert_eq!(
            permitted(
                &allow(json!({"command": "rm -rf ~"})),
                &json!({"tool_name": "Bash", "tool_input": {"command": "ls"}}),
            ),
            None
        );
    }

    /// A waiting hook stops waiting once the process that ran it is gone, as when Claude Code
    /// ends the hook's shell, so a hook left behind does not keep its ask pending. A parent that
    /// could not be read at the start leaves the wait to the registration alone.
    #[test]
    fn a_waiting_hook_leaves_once_its_parent_is_gone() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let (parent, _) = super::super::process(child.id()).unwrap();
        assert!(present(Some(&parent)));
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!present(Some(&parent)));
        assert!(present(None));
    }
}
