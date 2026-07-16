//! Append-only audit log: who, when, against which agent, did what.
//!
//! JSONL, one record per line, opened with `O_APPEND` — the kernel guarantees every write lands at
//! end of file, so concurrent appends from several threads never truncate each other (provided one
//! write emits one whole line, which is why the line is assembled first and written once).
//!
//! Append-only means there is **no delete interface**: rotation and archival are logrotate's job,
//! outside. The Hub never goes back and edits it. Denied requests get recorded too — "who tried and
//! did not get in" is often worth more than "who got in".

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Most bytes a query looks back over. The log grows without bound, but "what happened recently"
/// only needs the tail.
const TAIL_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub when: String,
    /// Username; anonymous requests record "anonymous".
    pub actor: String,
    /// Action name, see the constants below.
    pub action: String,
    /// The agent involved; null for actions unrelated to a specific agent (login and friends).
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub detail: String,
}

// Action names — fixed strings; the frontend and grep both depend on them.
pub const LOGIN: &str = "login";
pub const LOGIN_FAILED: &str = "login.failed";
pub const LOGOUT: &str = "logout";
pub const USER_ADD: &str = "user.add";
pub const AGENT_CREATE: &str = "agent.create";
pub const AGENT_DELETE: &str = "agent.delete";
pub const AGENT_RENAME: &str = "agent.rename";
pub const AGENT_VISIBILITY: &str = "agent.visibility";
pub const MEMBER_ADD: &str = "member.add";
pub const MEMBER_REMOVE: &str = "member.remove";
pub const TOKEN_CREATE: &str = "token.create";
pub const TOKEN_REVOKE: &str = "token.revoke";
pub const GIT_FETCH: &str = "git.fetch";
pub const GIT_PUSH: &str = "git.push";
pub const DENIED: &str = "denied";

pub fn log_path(root: &Path) -> PathBuf {
    root.join("audit.log")
}

/// Append one record. A failed write **does not interrupt the request** (a broken audit log should
/// not take the Hub down), but it does shout on stderr.
pub fn append(root: &Path, actor: &str, action: &str, agent: Option<&str>, detail: &str) {
    let e = Entry {
        when: super::store::now_iso(),
        actor: actor.to_string(),
        action: action.to_string(),
        agent: agent.map(|s| s.to_string()),
        detail: detail.to_string(),
    };
    if let Err(err) = try_append(root, &e) {
        eprintln!("cannot write the audit log ({}): {err}", log_path(root).display());
    }
}

fn try_append(root: &Path, e: &Entry) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    super::store::ensure_root(root)?;
    // Assemble the whole line, then write it once: O_APPEND + a single write = concurrent appends
    // that do not interleave.
    let mut line = serde_json::to_string(e).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    line.push('\n');
    let mut f = std::fs::OpenOptions::new().append(true).create(true).mode(0o600).open(log_path(root))?;
    f.write_all(line.as_bytes())
}

/// Read the tail: filtered by agent, at most limit records, newest first.
pub fn query(root: &Path, agent: Option<&str>, limit: usize) -> Vec<Entry> {
    let Some(text) = read_tail(&log_path(root)) else {
        return vec![];
    };
    let mut out: Vec<Entry> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<Entry>(l).ok())
        .filter(|e| match agent {
            Some(a) => e.agent.as_deref() == Some(a),
            None => true,
        })
        .collect();
    out.reverse(); // newest first
    out.truncate(limit);
    out
}

/// Read only the trailing TAIL_BYTES and drop the possibly-severed first line. A log that grows to
/// several GB never gets pulled into memory whole.
fn read_tail(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let truncated = len > TAIL_BYTES;
    if truncated {
        f.seek(SeekFrom::Start(len - TAIL_BYTES)).ok()?;
    }
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    match truncated {
        // Seeking into the middle usually lands mid-line, so drop that first half-line.
        true => text.split_once('\n').map(|(_, rest)| rest.to_string()),
        false => Some(text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_and_queries_newest_first() {
        let d = tempfile::tempdir().unwrap();
        append(d.path(), "alice", AGENT_CREATE, Some("x"), "visibility=private");
        append(d.path(), "bob", GIT_PUSH, Some("y"), "");
        append(d.path(), "alice", LOGIN, None, "");

        let all = query(d.path(), None, 10);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].action, LOGIN, "newest first");
        assert_eq!(all[2].action, AGENT_CREATE);

        let only_x = query(d.path(), Some("x"), 10);
        assert_eq!(only_x.len(), 1);
        assert_eq!(only_x[0].actor, "alice");
        assert_eq!(only_x[0].detail, "visibility=private");

        assert_eq!(query(d.path(), None, 2).len(), 2, "limit takes effect");
        assert!(query(d.path(), Some("nope"), 10).is_empty());
    }

    #[test]
    fn log_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        append(d.path(), "alice", LOGIN, None, "");
        let mode = std::fs::metadata(log_path(d.path())).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn append_only_never_rewrites_history() {
        let d = tempfile::tempdir().unwrap();
        append(d.path(), "alice", LOGIN, None, "one");
        let first = std::fs::read_to_string(log_path(d.path())).unwrap();
        append(d.path(), "bob", LOGIN, None, "two");
        let second = std::fs::read_to_string(log_path(d.path())).unwrap();
        assert!(second.starts_with(&first), "old lines must stay verbatim at the front");
        assert_eq!(second.lines().count(), 2);
    }

    #[test]
    fn missing_log_is_empty_not_an_error() {
        let d = tempfile::tempdir().unwrap();
        assert!(query(d.path(), None, 10).is_empty());
    }

    #[test]
    fn detail_with_newline_cannot_forge_a_row() {
        // detail is user-controlled (agent names, usernames). JSON encoding escapes the newline
        // away; otherwise "\n{...}" could forge a whole record into the audit log.
        let d = tempfile::tempdir().unwrap();
        append(d.path(), "eve", LOGIN, None, "x\n{\"actor\":\"root\",\"action\":\"login\"}");
        let raw = std::fs::read_to_string(log_path(d.path())).unwrap();
        assert_eq!(raw.lines().count(), 1, "one record is one record; a newline must not split it in two");
        let q = query(d.path(), None, 10);
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].actor, "eve");
    }
}
