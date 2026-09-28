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
/// carried only by child processes agit starts itself. Both support unmanaged-session safety
/// checks and stale AGIT_SESSION rejection; neither selects an ordinary command target.
pub const ENV_SESSIONS: &[(&str, &str)] = &[
    ("CODEBUDDY_SESSION_ID", "workbuddy"),
    ("HERMES_SESSION_ID", "hermes"),
    ("OPENCLAW_SESSION_ID", "openclaw"),
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

/// One runtime session variable and the native session it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Named {
    /// The environment variable that carries the identity.
    pub variable: &'static str,
    pub runtime: &'static str,
    pub session_id: String,
}

/// Every distinct session the runtime variables name, in lookup order.
///
/// A runtime that keeps its sessions in files must have that transcript on disk, so a variable
/// holding a deleted id names nothing. A runtime whose sessions live in a database is taken at its
/// variable's word: opening that database, even read-only, can create WAL coordination files, and
/// the callers of this function promise to leave native state untouched.
///
/// A nested runtime inherits its parent's variables, so more than one entry means the environment
/// alone does not say which conversation this process belongs to. Callers report that instead of
/// choosing one; none of them turns an entry into a command target.
pub fn named() -> Vec<Named> {
    use crate::adapter::native_snapshot::{self, Limits, Unavailable};
    let mut named: Vec<Named> = Vec::new();
    for &(variable, runtime) in ENV_SESSIONS {
        let Ok(session_id) = std::env::var(variable) else {
            continue;
        };
        if session_id.trim().is_empty()
            || named
                .iter()
                .any(|seen| seen.runtime == runtime && seen.session_id == session_id)
        {
            continue;
        }
        match native_snapshot::lookup_files_without_database(
            runtime,
            &session_id,
            Limits::default(),
        ) {
            Ok(_) | Err(Unavailable::Unsupported) => named.push(Named {
                variable,
                runtime,
                session_id,
            }),
            Err(_) => {}
        }
    }
    named
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
