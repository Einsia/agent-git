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
/// Self-service signup (POST /api/register). Distinct from `user.add` (the admin CLI path), so the
/// audit log shows which door a new account came through.
pub const USER_REGISTER: &str = "user.register";
/// Self-service password change (POST /api/me/password) — the logged-in user rotated their own
/// password after proving the old one.
pub const USER_PASSWORD: &str = "user.password";
/// Admin-mediated password reset (CLI `user passwd` or POST /api/users/<u>/password) — a site admin
/// set another user's password. This is the account-recovery door, kept distinct from the
/// self-service change so the audit log shows an admin acted on someone else's credential.
pub const USER_PASSWORD_RESET: &str = "user.password.reset";
/// Two-factor lifecycle. `enroll` = a pending TOTP secret was generated (not yet active); `enable` =
/// a code confirmed it and 2FA is now active (backup codes minted); `disable` = the user turned their
/// own 2FA off (with a code or password); `admin.disable` = a site admin cleared a locked-out user's
/// 2FA (CLI `user 2fa-disable` or the admin API) — kept distinct so the log shows an admin acted.
pub const TWOFA_ENROLL: &str = "user.2fa.enroll";
pub const TWOFA_ENABLE: &str = "user.2fa.enable";
pub const TWOFA_DISABLE: &str = "user.2fa.disable";
pub const TWOFA_ADMIN_DISABLE: &str = "user.2fa.admin.disable";
/// A user published (or rotated) their public keys in the shared identity registry.
pub const IDENTITY_ENROLL: &str = "user.identity.enroll";
pub const AGENT_CREATE: &str = "agent.create";
pub const AGENT_DELETE: &str = "agent.delete";
pub const AGENT_RENAME: &str = "agent.rename";
pub const AGENT_VISIBILITY: &str = "agent.visibility";
pub const AGENT_DESCRIBE: &str = "agent.describe";
pub const AGENT_FORK: &str = "agent.fork";
/// Ownership moving is the one edit that can lock the previous owner out of their own agent, so it
/// gets its own row rather than hiding inside agent.rename's shape.
pub const AGENT_TRANSFER: &str = "agent.transfer";
pub const AGENT_ARCHIVE: &str = "agent.archive";
pub const AGENT_UNARCHIVE: &str = "agent.unarchive";
pub const AGENT_RESTORE: &str = "agent.restore";
/// The irreversible one. `agent.delete` is now a soft delete; this is the row that means the bytes
/// are gone.
pub const AGENT_PURGE: &str = "agent.purge";
pub const AGENT_STAR: &str = "agent.star";
pub const MEMBER_ADD: &str = "member.add";
pub const MEMBER_REMOVE: &str = "member.remove";
pub const ORG_CREATE: &str = "org.create";
pub const ORG_MEMBER_ADD: &str = "org.member.add";
pub const ORG_MEMBER_REMOVE: &str = "org.member.remove";
/// The invitation consent flow. `invite` = an admin issued a pending invitation; `accept` = the
/// invited user accepted and became a member; `decline` = the invited user declined (no membership);
/// `revoke` = an admin cancelled a still-pending invitation.
pub const ORG_INVITE: &str = "org.invite";
pub const ORG_INVITE_ACCEPT: &str = "org.invite.accept";
pub const ORG_INVITE_DECLINE: &str = "org.invite.decline";
pub const ORG_INVITE_REVOKE: &str = "org.invite.revoke";
/// Ownership handoff: the current owner (an org admin) promoted an existing member to admin and
/// stepped down to a plain member. Kept distinct from member edits — it can lock the old owner out of
/// managing the org.
pub const ORG_TRANSFER: &str = "org.transfer";
/// An org was deleted (refused while it still owns agents). Memberships and pending invitations go
/// with it.
pub const ORG_DELETE: &str = "org.delete";
/// An org admin published a Team-KEK generation's per-member envelopes (encryption-recipients Wave 3).
/// The detail records the generation and how many recipient envelopes were stored — ciphertext only,
/// the hub never sees the plaintext TK.
pub const ORG_KEK_PUBLISH: &str = "org.kek.publish";
/// Wave-5 opt-in escape hatches. An org owner set/cleared the offline recovery recipient, changed the
/// hub-assist escrow mode, or the hub released an escrowed content key to an ACL reader.
pub const ORG_RECOVERY_SET: &str = "org.recovery.set";
pub const ORG_RECOVERY_CLEAR: &str = "org.recovery.clear";
pub const ORG_ESCROW_MODE: &str = "org.escrow.mode";
pub const KEYS_ESCROW: &str = "keys.escrow";
pub const KEYS_RELEASE: &str = "keys.release";
pub const TOKEN_CREATE: &str = "token.create";
pub const TOKEN_REVOKE: &str = "token.revoke";
pub const GIT_FETCH: &str = "git.fetch";
pub const GIT_PUSH: &str = "git.push";
/// A content-addressed blob was uploaded to an agent (PUT /api/agent/<name>/blob). The detail is the
/// sha256 the server computed and stored.
pub const BLOB_PUT: &str = "blob.put";
/// A stored blob's bytes did not hash to the digest they are keyed under (fs corruption, or an S3
/// object swapped underneath). The read is refused (500) rather than serving bytes that don't match
/// their address, and this row is the durable trace.
pub const BLOB_CORRUPT: &str = "blob.corrupt";
/// A push the server-side secret scan turned away. The client hook is `--no-verify`-able, so this
/// row is the only durable trace that someone tried.
pub const GIT_PUSH_REJECTED: &str = "git.push.rejected";
/// Identity events. The Hub never mints an aid — it only ever reports what it read out of the store,
/// and these say what it made of it.
pub const AGENT_AID_LEARNED: &str = "agent.aid.learned";
/// The store behind a name now carries a **different** aid: the repo was recreated, or another store
/// was pushed over it. Every `.agit.toml` pinned to the old aid is now pointed at a stranger — that
/// is worth a permanent row, since the response only says so once.
pub const AGENT_AID_REPLACED: &str = "agent.aid.replaced";
/// Two agents claiming one identity. Refused, and recorded.
pub const AGENT_AID_CONFLICT: &str = "agent.aid.conflict";
pub const MR_OPEN: &str = "mr.open";
pub const MR_COMMENT: &str = "mr.comment";
pub const MR_CLOSE: &str = "mr.close";
pub const MR_MERGED: &str = "mr.merged";
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
    super::store::ensure_root(root)?;
    // Assemble the whole line, then write it once: O_APPEND + a single write = concurrent appends
    // that do not interleave.
    let mut line = serde_json::to_string(e).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    line.push('\n');
    let mut opts = std::fs::OpenOptions::new();
    opts.append(true).create(true);
    // 0600 owner-only on Unix; Windows has no mode bits (file security is by ACL), so this is a no-op there.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(log_path(root))?;
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
    #[cfg(unix)]
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
