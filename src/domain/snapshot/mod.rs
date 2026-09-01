//! Snapshot: what `agit commit` produces, and the version ID itself.
//!
//! # A version comes from an act of the author, not from session structure
//!
//! Binding the version to "the hash of the last **closed** turn" loses the trailing turn: a turn
//! closes only once the person speaks again, so the most valuable stretch of work never reaches a
//! version. Codex barely covers it with `task_complete`; Claude Code has no equivalent at all.
//!
//! The root cause is binding the version to session structure. A snapshot binds it back to the
//! author: **you say this is a version, so this is a version** — covering everything in the
//! transcript at the moment of the commit, with no distinction between open and closed.
//!
//! # id = git commit hash
//!
//! A snapshot id is the SHA-1 of the git commit, with an `agit-` prefix. A git commit hash is
//! already a content address: it covers the whole parent → tree → blobs tree, and one changed
//! byte changes it. No second layer of hashing is needed.
//!
//! The `agit-` prefix serves two purposes: a version ID is distinguishable from a git SHA at a
//! glance, and in `agit clone x/y:Z` a `Z` that starts with it is a version, anything else a
//! branch name. The separator is a hyphen and not an underscore — `agit_` + 64 hex matches the
//! backend secret rule `\bagit_[0-9a-f]{64}\b`, so a version ID written into a commit message is
//! rejected as a leaked token (observed).
//!
//! # Session identity = the root snapshot id of this branch
//!
//! `session` records **the id of this branch's first snapshot**; once a session occupies a branch
//! it is never reassigned to another one (the server-side gate judges by it — see gate ④ in the
//! backend's `gitsync::routes`).
//!
//! The runtime session id cannot serve: [`crate::domain::install`] mints a new UUID on every
//! install, so the same agent cloned onto two machines necessarily has different runtime ids, and
//! binding on it makes a commit to the same branch from the second machine read as a changed
//! session. Fast-forward alone cannot serve either — FF only requires the new commit to descend
//! from the old tip, and a descendant commit is free to replace the files wholesale. The binding
//! must be at the content layer.
//!
//! The root snapshot's `session` is its own commit hash. A commit hash cannot be computed until
//! the commit exists (the tree it covers contains `snapshot.json` itself), so the root snapshot's
//! `session` first holds a **content hash** `hash(cwd + transcript)` in its place — a value that
//! is available ahead of the commit and is stable for the same content. Later snapshots inherit
//! `session` from the tip and compute nothing.
//!
//! `session` does not enter the commit hash (it is not an input to the git commit), so there is
//! no circular dependency.

use crate::Result;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Prefix of a snapshot id.
pub const ID_PREFIX: &str = "agit-";

/// Hex length of an id. 20 bytes = 160 bits, the same magnitude as git's SHA-1.
pub const ID_HEX_LEN: usize = 40;

/// File name of the snapshot metadata at the agent repo root.
pub const FILE: &str = "snapshot.json";

/// File name of a branch's transcript at the agent repo root: one envelope per line
/// (see [`crate::domain::transcript`]).
///
/// With one session per branch, a directory level split by runtime carries nothing, and the path
/// no longer has to be recorded in `snapshot.json` — fixing it to this one name removes a place
/// that can disagree with the facts.
pub const TRANSCRIPT_FILE: &str = "transcript.jsonl";

/// File name of the resume VIEW at the agent repo root: the slices of
/// [`crate::domain::transcript::view_of_live`] packed together, rewritten whole at every commit
/// (a stateless derivative; git carries the history).
pub const VIEW_FILE: &str = "view.jsonl";

/// Compute a session identity: the root snapshot's `session` field.
///
/// `hash(cwd + transcript)` — no parent (that is the git commit's business) and no session itself
/// (which would be circular). Stable for the same (cwd, transcript bytes), and available ahead of
/// the commit.
pub fn session_hash(cwd: &str, transcript: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(cwd.as_bytes());
    h.update([0u8]);
    h.update(transcript);
    format!("{ID_PREFIX}{}", &hex::encode(h.finalize())[..ID_HEX_LEN])
}

/// Turn a git commit SHA into a snapshot id (prefix added).
pub fn id_from_sha(sha: &str) -> String {
    format!("{ID_PREFIX}{sha}")
}

/// Take the git commit SHA out of a snapshot id (prefix stripped).
pub fn sha_from_id(id: &str) -> Option<&str> {
    id.strip_prefix(ID_PREFIX)
}

/// The short form of a snapshot id or of a `parent` (for display).
pub fn short(id: &str) -> String {
    short_hash(id)
}

fn short_hash(h: &str) -> String {
    h.chars().take(ID_PREFIX.len() + 8).collect()
}

/// `agit-` plus exactly 40 lowercase hex characters.
pub fn is_bare_id(s: &str) -> bool {
    match s.strip_prefix(ID_PREFIX) {
        Some(hex) => {
            hex.len() == ID_HEX_LEN
                && hex
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }
        None => false,
    }
}

/// `snapshot.json`: the snapshot metadata at the agent repo root.
///
/// Deliberately **absent**:
///
/// * **`parent`** — a git commit already records its parent; snapshot.json does not repeat that
///   layer.
/// * **`signer` / `key` / `sig`** — there are no signatures. Content integrity comes from the git
///   commit hash, and the commit's author from git's `user.name` / `user.email`.
/// * **the runtime's local session id** — a local identifier for "this install on this machine",
///   not a durable identity. Recording it only tempts others into using it as one.
/// * `version` — that is the snapshot id (the commit hash); self-referential.
/// * `captured_at` / `signed_at` — unverifiable self-report. For a timestamp, ask the git commit.
///
/// The four kinds of commit (PRD section "the four kinds of commit").
///
/// The default is `turn`: every commit in an older repo is a settlement, which is semantically
/// equivalent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// The settlement of one user turn.
    #[default]
    Turn,
    /// The two-parent commit of `agit merge`.
    Merge,
    /// `agit cherry-pick` / `agit revert`: changes the VIEW only.
    View,
    /// `agit commit -m`: changes the shared files only (memory/skills/AGENTS.md).
    File,
}

/// Confidence level of the code anchor (PRD section "code anchor").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Completeness {
    /// A sha exists and the worktree was clean at settlement: a checkout restores it.
    Exact,
    /// Dirty at settlement: only the base commit can be restored.
    Partial,
    /// Unverifiable (turns backfilled by `import` are uniformly this).
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    /// The session identity of this branch: **the value claimed by its root snapshot**
    /// (`agit-` + 40 hex).
    ///
    /// The root snapshot claims `hash(cwd + transcript bytes)` (a commit SHA does not exist yet at
    /// that point); later snapshots inherit from the tip.
    pub session: String,
    /// The runtime that ran this session: `codex` / `claude-code`.
    ///
    /// Required: the envelope's `_source` and the VIEW slices both parse against it, and nothing
    /// else can supply it once it is missing.
    pub runtime: String,
    /// The working directory the session ran in. The source of truth for "which project this
    /// memory belongs to".
    pub cwd: String,
    /// The corresponding code state, `<origin>@<short-sha>`. Absent when cwd is not a git repo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// The commit kind. Defaults to turn (equivalent semantics in older repos).
    #[serde(default)]
    pub kind: Kind,
    /// Which turn of this session line this is (1-based). A commit on the file line has no turn
    /// ordinal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u32>,
    /// Confidence level of the code anchor at this turn's settlement. Meaningful only when
    /// `code` exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completeness: Option<Completeness>,
    /// The phase summary added by `--milestone`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub milestone: Option<String>,
    /// Baseline byte count of the live transcript once settlement completes (what `doctor` tests
    /// "still append-only" against).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_bytes: Option<u64>,
    /// The runtime instances registered on this branch (the logical session id is fixed, the
    /// instance varies). Element form: `<runtime>/<local session id>`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runtime_instances: Vec<String>,
}

impl Snapshot {
    /// Build a snapshot holding the required fields only; every v2 extension field takes its
    /// default.
    pub fn new(session: String, runtime: String, cwd: String) -> Self {
        Snapshot {
            session,
            runtime,
            cwd,
            code: None,
            kind: Kind::Turn,
            turn: None,
            completeness: None,
            milestone: None,
            baseline_bytes: None,
            runtime_instances: Vec::new(),
        }
    }
}

pub fn path_in(repo_root: &Path) -> PathBuf {
    repo_root.join(FILE)
}

/// Read the `snapshot.json` in the workspace.
pub fn read(repo_root: &Path) -> Option<Snapshot> {
    serde_json::from_str(&std::fs::read_to_string(path_in(repo_root)).ok()?).ok()
}

/// Read the `snapshot.json` at a git ref (without switching the workspace).
pub fn read_at_ref(repo: &crate::domain::repo::Repo, git_ref: &str) -> Option<Snapshot> {
    serde_json::from_str(&repo.show(git_ref, FILE)?).ok()
}

/// Read the workspace's snapshot.
pub fn resolve(repo_root: &Path) -> Result<Snapshot> {
    read(repo_root).with_context(|| format!("no readable {FILE} in {}", repo_root.display()))
}

/// The code state a working directory corresponds to, `<origin>@<short-sha>`.
///
/// Either part missing returns None: a sha alone cannot be resolved by anyone else (there is no
/// telling which repo to look in), and an origin alone matches no particular version. Half an
/// answer is easier to misuse than no answer.
pub fn code_of(cwd: &Path) -> Option<String> {
    let origin = git_field(cwd, &["remote", "get-url", "origin"])?;
    let sha = git_field(cwd, &["rev-parse", "--short", "HEAD"])?;
    Some(format!("{origin}@{sha}"))
}

fn git_field(dir: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_hash_is_stable_and_deterministic() {
        let h = session_hash("/w", b"hello");
        assert!(h.starts_with(ID_PREFIX));
        assert_eq!(h.len(), ID_PREFIX.len() + ID_HEX_LEN);
        assert_eq!(h, session_hash("/w", b"hello"), "must be deterministic");
        assert_ne!(h, session_hash("/w", b"world"), "content changes the hash");
        assert_ne!(h, session_hash("/other", b"hello"), "cwd changes the hash");
    }

    #[test]
    fn id_from_sha_and_back() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let id = id_from_sha(sha);
        assert_eq!(id, "agit-0123456789abcdef0123456789abcdef01234567");
        assert_eq!(sha_from_id(&id), Some(sha));
        assert_eq!(sha_from_id("no-prefix"), None);
    }

    #[test]
    fn is_bare_id_checks_shape() {
        assert!(is_bare_id(&format!("{ID_PREFIX}{}", "a".repeat(40))));
        assert!(!is_bare_id(&format!("{ID_PREFIX}{}", "A".repeat(40)))); // uppercase
        assert!(!is_bare_id(&format!("{ID_PREFIX}{}", "a".repeat(39)))); // too short
        assert!(!is_bare_id("no-prefix"));
    }

    /// A snapshot without `runtime` does not parse — this pins the criterion for "a repo in the
    /// legacy layout", whose snapshots carry no `runtime` field.
    #[test]
    fn a_snapshot_without_runtime_does_not_parse() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            path_in(d.path()),
            "{\"session\":\"sessions/codex/AB.jsonl\",\"cwd\":\"/r\"}\n",
        )
        .unwrap();
        assert!(read(d.path()).is_none(), "no runtime means no read");
    }

    #[test]
    fn short_trims_to_prefix_plus_8() {
        let id = format!("{ID_PREFIX}{}", "3f9c8a12".repeat(5));
        assert_eq!(short(&id), "agit-3f9c8a12");
    }
}
