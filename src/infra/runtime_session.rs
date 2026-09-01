//! Discovery of the current process's runtime session.
//!
//! The runtime hands the current transcript's identity to child processes through environment
//! variables; that is more reliable than workspace/CWD, because one directory can host several
//! sessions at once. This module discovers and reads the runtime identity only; it prints no CLI
//! text.

use crate::adapter;
use crate::domain::{link, store::Store};

/// The current session located from the runtime environment variables.
#[derive(Debug, Clone)]
pub struct Current {
    pub runtime: &'static str,
    pub session_id: String,
    pub cwd: Option<String>,
    /// Completed turn count when the transcript parses; None while it is being written or its
    /// format is unknown.
    pub completed_turns: Option<usize>,
    /// The store already holds complete evidence that this session is managed.
    pub managed: bool,
}

/// Environment variables the harness uses to hand a child process the current transcript's id,
/// in lookup order.
///
/// Claude Code exports `CLAUDE_CODE_SESSION_ID`; `CLAUDE_SESSION_ID` is agit's own name for it,
/// carried only by child processes agit starts itself. Recognizing only the latter leaves `@`,
/// `import @` and the `new` guard with nothing to resolve inside Claude Code. Both are recognized,
/// the real one first.
pub const ENV_SESSIONS: &[(&str, &str)] = &[
    ("CLAUDE_CODE_SESSION_ID", "claude-code"),
    ("CLAUDE_SESSION_ID", "claude-code"),
    ("CODEX_SESSION_ID", "codex"),
    ("OPENCODE_SESSION_ID", "opencode"),
];

/// The form of `AGIT_SESSION`: `<owner>/<name>@<branch>`.
pub fn encode_env(repo: &str, branch: &str) -> String {
    format!("{repo}@{branch}")
}

/// Decode `AGIT_SESSION`.
pub fn decode_env(value: &str) -> Option<(String, String)> {
    let (repo, branch) = value.split_once('@')?;
    if repo.is_empty() || branch.is_empty() || !repo.contains('/') {
        return None;
    }
    Some((repo.to_string(), branch.to_string()))
}

/// Whether the environment carries a valid AgentGit session identity.
pub fn has_managed_env() -> bool {
    std::env::var("AGIT_SESSION")
        .ok()
        .and_then(|value| decode_env(&value))
        .is_some()
}

/// The runtime session in the current process environment that resolves to a real transcript.
///
/// A stale environment variable with no transcript is not a live session. The store is read
/// through the read-only `open`, so a protective check never creates `~/.agit`.
pub fn current() -> Option<Current> {
    let store = Store::open().ok().flatten();

    for &(env, runtime) in ENV_SESSIONS {
        let Ok(session_id) = std::env::var(env) else {
            continue;
        };
        if session_id.trim().is_empty() {
            continue;
        }

        let Ok(adapter) = adapter::get(runtime) else {
            continue;
        };
        let Some(path) = adapter.resolve(&session_id, None) else {
            // The environment can hold a deleted id; no transcript means no live session.
            continue;
        };
        if !path.is_file() {
            continue;
        }

        let parsed = adapter.parse_at(&path).ok();
        let (cwd, completed_turns) = match parsed {
            Some(session) => {
                let turns = crate::domain::turn::completed_count(&session);
                (session.cwd, Some(turns))
            }
            None => (None, None),
        };
        let managed = store
            .as_ref()
            .and_then(|s| link::get(s, runtime, &session_id))
            .is_some_and(|link| link::is_managed(&link));

        return Some(Current {
            runtime,
            session_id,
            cwd,
            completed_turns,
            managed,
        });
    }
    None
}

/// The current runtime session when it has not been adopted into AgentGit.
pub fn unmanaged() -> Option<Current> {
    // `AGIT_SESSION` is the identity AgentGit injects itself; even before the runtime's native
    // session id has a complete link written, a managed session started by `run`/`resume`/`new`
    // must not be treated as one that "needs import".
    if has_managed_env() {
        return None;
    }
    current().filter(|session| !session.managed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Claude Code exports `CLAUDE_CODE_SESSION_ID`; child processes agit starts itself carry
    /// `CLAUDE_SESSION_ID`. This pins that both are recognized — otherwise `@` does not resolve
    /// inside a real Claude Code session.
    #[test]
    fn both_claude_session_variables_are_recognized() {
        assert!(ENV_SESSIONS.contains(&("CLAUDE_CODE_SESSION_ID", "claude-code")));
        assert!(ENV_SESSIONS.contains(&("CLAUDE_SESSION_ID", "claude-code")));
    }

    #[test]
    fn session_env_roundtrip() {
        let value = encode_env("me/payments", "refund-fix");
        assert_eq!(value, "me/payments@refund-fix");
        assert_eq!(
            decode_env(&value),
            Some(("me/payments".into(), "refund-fix".into()))
        );
        assert!(decode_env("no-slash@x").is_none());
        assert!(decode_env("a/b@").is_none());
    }
}
