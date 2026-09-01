//! Session metadata: `session/meta.json`, the product of `agit commit` and the version ID itself.
//!
//! # A version comes from an act of the author, not as a by-product of session structure
//!
//! Binding the version to "the hash of the last **closed** turn" does not work: a turn closes
//! only when the person speaks again, so the trailing turn — usually the most valuable stretch of
//! work — never enters a version. Codex barely covers it with `task_complete`; Claude Code has no
//! equivalent signal at all.
//!
//! The root cause is binding the version to session structure. meta binds it back to the author:
//! **you say this is a version, so this is a version** — covering everything the transcript holds
//! at the moment of the commit, open turns and closed turns alike.
//!
//! # id = git commit hash
//!
//! A version ID is the SHA-1 of the git commit (with an `agit-` prefix). A git commit hash is
//! already a content address: it covers the whole parent → tree → blobs tree, and one changed
//! byte changes it. No second hash layer of our own.
//!
//! The `agit-` prefix has two uses: it tells a version ID from a git SHA at a glance, and in
//! `agit clone x/y:Z` a `Z` starting with it is a version, anything else a branch name. A hyphen,
//! not an underscore — `agit_` + 64 hex matches the backend secret rule `\bagit_[0-9a-f]{64}\b`,
//! and a version ID written into a commit message is then rejected as a leaked token (observed).
//!
//! # Session identity = the value this branch's root meta claims
//!
//! `session` records **the id claimed by the root meta of this branch**; once a session takes a
//! branch, that branch never changes hands (the server-side gate judges by it — see gate ④ in the
//! backend's `gitsync::routes`).
//!
//! The runtime session id cannot serve: [`crate::domain::install`] mints a new UUID on every
//! install, so the same agent cloned onto two machines necessarily carries two runtime ids, and
//! binding on that makes a commit from machine B onto the same branch look like a change of
//! session. Fast-forward alone cannot serve either — FF only requires the new commit to descend
//! from the old tip, and a descendant is free to replace the file wholesale. The binding has to
//! be at the content layer.
//!
//! The root meta's `session` is its own commit hash. But a commit hash cannot be computed until
//! the commit exists (the tree it covers contains `session/meta.json` itself), so the root meta's
//! `session` stands in with a **content hash** `hash(cwd + transcript)` — a value that can be
//! computed ahead of the commit and is stable for the same content. Later meta inherit `session`
//! from the tip and compute nothing.
//!
//! `session` does not enter the commit hash (it is not an input to the git commit), so there is
//! no circular dependency.
//!
//! # The branch shape is written here, not inferred from which files exist
//!
//! [`Line`] is fixed the moment the branch is born and written into meta. Inferring it from "is
//! there a snapshot.json in the tree" carries a fatal overlap: a new session branch imported after
//! `agit init` grows off the head of main and, until the first settlement lands, equally "has
//! commits, has no session file", so it is rejected as a file line — the W1 main flow dies there.
//! Shape is a property of the branch, so the branch declares it itself.

use crate::Result;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// Prefix of a version ID.
pub const ID_PREFIX: &str = "agit-";

/// Hex length of an id: the same width as a git SHA-1.
pub const ID_HEX_LEN: usize = 40;

/// Path of the session metadata inside the repo.
pub const FILE: &str = "session/meta.json";

/// The complete v1 event sequence: one 40-character lowercase hex event id per line.
pub const LOG_FILE: &str = "LOG";

/// The v1 resume VIEW: a sequence of event ids as well.
pub const VIEW_FILE: &str = "VIEW";

/// The complete v0 envelope JSONL. Read for history and migration only.
pub const LEGACY_LOG_FILE: &str = "session/log.jsonl";

/// The v0 resume VIEW JSONL. Read for history and migration only.
pub const LEGACY_VIEW_FILE: &str = "session/VIEW";

/// Layout-version alias for [`LEGACY_LOG_FILE`].
pub const V0_LOG_FILE: &str = LEGACY_LOG_FILE;

/// Layout-version alias for [`LEGACY_VIEW_FILE`].
pub const V0_VIEW_FILE: &str = LEGACY_VIEW_FILE;

/// Root directory of the v1 content-addressed event objects.
pub const EVENTS_DIR: &str = "events";

/// The Git attributes file that protects the content-addressed bytes.
pub const ATTRS_FILE: &str = ".gitattributes";

/// Cross-runtime storage migration commit contract.  Both the CLI and the Hub append the same
/// mechanical child to a v0 tip, so every byte that enters the commit object must be fixed.
pub const STORAGE_MIGRATION_MESSAGE: &str = "agit: migrate storage layout to v1";
pub const STORAGE_MIGRATION_MILESTONE: &str = "storage layout v1 migration";
pub const STORAGE_MIGRATION_NAME: &str = "AgentGit migration";
pub const STORAGE_MIGRATION_EMAIL: &str = "migration@agentgit.local";

/// Hex length of an event id.
pub const EVENT_ID_HEX_LEN: usize = 40;

/// Repository storage layout version.
///
/// Old meta carry no `layout` field, so the serde default must be v0; every newly created meta
/// writes v1 explicitly. A reader then never has to guess the layout from which files exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LayoutVersion {
    /// `session/log.jsonl` + `session/VIEW`; every line is a complete envelope.
    #[default]
    V0,
    /// `LOG` + `VIEW` hold event ids; the complete envelopes live in `events/`.
    V1,
}

impl LayoutVersion {
    /// The layout version written into every new meta.
    pub const CURRENT: Self = Self::V1;
}

/// Whether an event id is exactly 40 lowercase hex characters.
pub fn is_event_id(id: &str) -> bool {
    id.len() == EVENT_ID_HEX_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// event id → the v1 shard path: `events/a/b/c/d/<event-id>`.
pub fn event_path(event_id: &str) -> Result<String> {
    if !is_event_id(event_id) {
        anyhow::bail!(
            "event id must be exactly {EVENT_ID_HEX_LEN} lowercase hex characters; got `{event_id}`"
        );
    }
    let chars = event_id.as_bytes();
    Ok(format!(
        "{EVENTS_DIR}/{}/{}/{}/{}/{event_id}",
        chars[0] as char, chars[1] as char, chars[2] as char, chars[3] as char
    ))
}

/// Whether this path is owned entirely by the storage format, so it must never enter a file
/// commit as a user's shared file.
///
/// `.gitattributes` deliberately is not here: AgentGit owns only its marked blocks while users
/// own every rule outside them. Mutation paths normalize those blocks before committing the whole
/// shared file.
pub fn is_storage_path(path: &str) -> bool {
    matches!(
        path,
        FILE | LOG_FILE | VIEW_FILE | LEGACY_LOG_FILE | LEGACY_VIEW_FILE
    ) || path == EVENTS_DIR
        || path
            .strip_prefix(EVENTS_DIR)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Whether `path` is owned by the storage format at a particular snapshot.
///
/// Root `LOG`, `VIEW` and `events/` were ordinary shared paths in v0. They become reserved only
/// after a collision-checked v1 upgrade, so v0 file operations must not hide or silently unstage
/// user data that happens to use those names. `.gitattributes` remains a shared file in both
/// layouts; only its explicitly marked AgentGit blocks are managed.
pub fn is_storage_path_for(layout: LayoutVersion, path: &str) -> bool {
    match layout {
        LayoutVersion::V0 => matches!(path, FILE | LEGACY_LOG_FILE | LEGACY_VIEW_FILE),
        LayoutVersion::V1 => is_storage_path(path),
    }
}

/// The two shapes a branch takes. Fixed at birth, never converted into each other (design:
/// "the two shapes a branch takes").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Line {
    /// Session line: bound for life to one logical agent session; takes turn/merge/view commits.
    #[default]
    Session,
    /// File line: never claims a session; takes only file commits and file-reconciling merges.
    /// It cannot be resumed, only used as the starting point of a fork / new.
    File,
}

/// Compute a session identity: the root meta's `session` field.
///
/// `hash(cwd + transcript)` — no parent (that is the git commit's business), no `session` itself
/// (which would be circular). Stable for the same (cwd, transcript bytes), and computable ahead
/// of the commit.
pub fn session_hash(cwd: &str, transcript: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(cwd.as_bytes());
    h.update([0u8]);
    h.update(transcript);
    format!("{ID_PREFIX}{}", &hex::encode(h.finalize())[..ID_HEX_LEN])
}

/// Mint a **new** session id: `ID_PREFIX` + [`ID_HEX_LEN`] random hex characters.
///
/// One place — [`ID_HEX_LEN`] — decides the length, instead of every caller writing its own
/// number: an id assembled in `rc` from `uuid::Uuid::new_v4().simple()` carries only 32 hex
/// characters, and it then fails [`is_bare_id`] (a `[..40.min(32)]` slice is the mark of wanting
/// 40 and getting 32).
///
/// The random source concatenates two UUIDs and truncates: what is wanted here is a
/// non-colliding identifier, not cryptographic randomness.
pub fn mint_session_id() -> String {
    let mut hex = uuid::Uuid::new_v4().simple().to_string();
    hex.push_str(&uuid::Uuid::new_v4().simple().to_string());
    format!("{ID_PREFIX}{}", &hex[..ID_HEX_LEN])
}

/// Turn a git commit SHA into a version ID (add the prefix).
pub fn id_from_sha(sha: &str) -> String {
    format!("{ID_PREFIX}{sha}")
}

/// Take the git commit SHA out of a version ID (strip the prefix).
pub fn sha_from_id(id: &str) -> Option<&str> {
    id.strip_prefix(ID_PREFIX)
}

/// The short form of a version ID or of `parent` (for display).
pub fn short(id: &str) -> String {
    short_hash(id)
}

fn short_hash(h: &str) -> String {
    h.chars().take(ID_PREFIX.len() + 8).collect()
}

/// `agit-` + exactly 40 lowercase hex characters.
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

/// The commit kinds (PRD, section "the four commit kinds").
///
/// The default is `turn`: every commit on a session line is a settlement, semantically the same
/// thing.
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
    /// `agit commit -m`: changes shared files only (memory/skills/AGENTS.md).
    File,
}

/// How far the code anchor can be trusted (PRD, section "code anchor").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Completeness {
    /// A sha exists and the worktree was clean at settlement: a checkout restores it.
    Exact,
    /// Dirty at settlement: only the base can be restored.
    Partial,
    /// No way to tell (turns backfilled by import always land here).
    Unknown,
}

/// A summary of the worktree at the time of capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeStatus {
    Clean,
    Dirty,
    Conflicted,
    #[default]
    Unknown,
}

/// The cwd worktree state a session turn commit observed.
///
/// Only a bounded summary is kept; file paths and contents never enter session history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CwdState {
    /// The configured `origin` remote, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// The full object id at the time of capture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// The short symbolic branch name; detached HEAD has no value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// A bounded classification of the worktree.
    #[serde(default)]
    pub worktree: WorktreeStatus,
    /// Paths with staged changes in the porcelain X column.
    #[serde(default)]
    pub staged: u32,
    /// Paths with unstaged changes in the porcelain Y column.
    #[serde(default)]
    pub unstaged: u32,
    /// Untracked paths reported by Git.
    #[serde(default)]
    pub untracked: u32,
    /// Paths reported with an unmerged/conflicting status.
    #[serde(default)]
    pub conflicted: u32,
    /// A digest of status records, without retaining their paths or contents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_digest: Option<String>,
}

impl CwdState {
    fn unknown(origin: Option<String>, head: Option<String>, branch: Option<String>) -> Self {
        Self {
            origin,
            head,
            branch,
            worktree: WorktreeStatus::Unknown,
            staged: 0,
            unstaged: 0,
            untracked: 0,
            conflicted: 0,
            status_digest: None,
        }
    }
}

/// `session/meta.json`: **every** commit carries one, a file line's commits included.
///
/// Deliberately **absent**:
///
/// * **`parent`** — a git commit already records its parent; no second copy of it here.
/// * **`signer` / `key` / `sig`** — meta carries no signature. Content integrity comes from the
///   git commit hash, and git's `user.name` / `user.email` record who authored the commit.
/// * **the runtime's local session id** — it identifies "this install on this machine", not a
///   durable identity. Recording it only tempts others to use it as one.
/// * `version` — that is the version ID (the commit hash); it would be self-referential.
/// * `captured_at` / `signed_at` — unverifiable self-report. Ask the git commit for times.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    /// Repository storage layout. Old meta carry no such field and are read as v0.
    #[serde(default)]
    pub layout: LayoutVersion,
    /// Whether this branch is a session line or a file line. Fixed at birth, never converted.
    #[serde(default)]
    pub line: Line,
    /// This branch's session identity: **the value its root meta claims** (`agit-` + 40 hex).
    ///
    /// Empty in two cases: a file line (which never claims a session), and the stretch after a
    /// session line is born but before its first turn commit lands — the identity needs
    /// `hash(cwd + transcript bytes)`, and there is no transcript yet.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub session: String,
    /// The runtime that produced this session: `codex` / `claude-code`.
    ///
    /// Required once a session line has claimed an identity: the envelope's `_source` and VIEW
    /// slicing both parse against it, and nothing else can supply it. A file line has no runtime.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub runtime: String,
    /// The working directory the session ran in. The source of truth for "which project this
    /// memory belongs to". A file line has none.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cwd: String,
    /// The matching code state, `<origin>@<short-sha>`. Absent when cwd is not a git repo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// The cwd worktree summary observed at commit time.
    /// The field is `cwd_state`; `code_state` stays only as a read alias for published metadata.
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "code_state")]
    pub cwd_state: Option<CwdState>,
    /// The commit kind. Defaults to turn.
    #[serde(default)]
    pub kind: Kind,
    /// Which turn of this session line this is (counting from 1). A file line's commits carry
    /// no turn ordinal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u32>,
    /// How far the code anchor could be trusted at that turn's settlement. Meaningful only when
    /// `code` exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completeness: Option<Completeness>,
    /// The phase summary added by `--milestone`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub milestone: Option<String>,
    /// The live transcript's baseline byte count once the settlement completes (what doctor
    /// tests "still append-only" against).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_bytes: Option<u64>,
    /// The runtime instances registered on this branch (the logical session id is fixed, the
    /// instance is not). Element form: `<runtime>/<local session id>`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runtime_instances: Vec<String>,
}

impl Meta {
    /// The meta of one settlement: a session line whose identity is claimed.
    pub fn new(session: String, runtime: String, cwd: String) -> Self {
        Meta {
            layout: LayoutVersion::CURRENT,
            line: Line::Session,
            session,
            runtime,
            cwd,
            code: None,
            cwd_state: None,
            kind: Kind::Turn,
            turn: None,
            completeness: None,
            milestone: None,
            baseline_bytes: None,
            runtime_instances: Vec::new(),
        }
    }

    /// The **birth** meta of a session line: the shape is fixed on the spot, the identity is left
    /// to the first turn commit.
    ///
    /// import / fork / new / run write it when they create the branch. The identity cannot be
    /// computed yet (`session_hash` needs transcript bytes, and a branch is born before its first
    /// settlement), but the shape has to be fixed now — otherwise the decision falls back to
    /// inferring it from which files exist.
    ///
    /// `kind` is `File` rather than the default `Turn`: a birth commit holds not one word of
    /// conversation, and marking it a turn makes `agit log` display a settlement that never
    /// happened.
    pub fn new_session_line(runtime: String, cwd: String) -> Self {
        Meta {
            runtime,
            cwd,
            kind: Kind::File,
            ..Meta::new(String::new(), String::new(), String::new())
        }
    }

    /// The birth meta of a file line: it never claims a session, so it has no identity and no
    /// runtime.
    ///
    /// `agit init` / `agit repo create` write it when they create main.
    pub fn new_file_line() -> Self {
        Meta {
            line: Line::File,
            kind: Kind::File,
            ..Meta::new(String::new(), String::new(), String::new())
        }
    }

    pub fn is_file_line(&self) -> bool {
        self.line == Line::File
    }

    pub fn is_session_line(&self) -> bool {
        self.line == Line::Session
    }
}

pub fn path_in(repo_root: &Path) -> PathBuf {
    repo_root.join(FILE)
}

/// Fail-closed topology preflight for a future [`write`] without changing the filesystem.
///
/// The repository root and every existing component below it must be real directories; the final
/// meta destination may be absent or a regular file, never a symlink/directory/special file.
pub fn ensure_write_safe(repo_root: &Path) -> Result<()> {
    ensure_real_directory(repo_root, false)?;
    let meta_path = path_in(repo_root);
    let session = meta_path.parent().expect("FILE always has a directory");
    match std::fs::symlink_metadata(session) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            ensure_regular_file_or_missing(&meta_path)?;
        }
        Ok(_) => anyhow::bail!(
            "refusing metadata directory {}: it is a symlink or non-directory",
            session.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("cannot inspect {}", session.display()));
        }
    }
    Ok(())
}

/// Ensure `session/` exists. In v1 it holds meta only; in v0 the log / VIEW live there too.
pub fn ensure_session_dir(repo_root: &Path) -> Result<()> {
    ensure_write_safe(repo_root)?;
    let d = path_in(repo_root)
        .parent()
        .expect("FILE always has a directory")
        .to_path_buf();
    ensure_real_directory(&d, true)?;
    Ok(())
}

/// Write `session/meta.json` (overwriting in place; git keeps the history).
///
/// This is the **only** place that produces `session/meta.json`.
pub fn write(repo_root: &Path, meta: &Meta) -> Result<PathBuf> {
    validate(meta)?;
    ensure_session_dir(repo_root)?;
    let p = path_in(repo_root);
    ensure_regular_file_or_missing(&p)?;
    let bytes = format!("{}\n", serde_json::to_string_pretty(meta)?);
    let parent = p.parent().expect("FILE always has a directory");
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("cannot create temporary metadata in {}", parent.display()))?;
    temporary
        .write_all(bytes.as_bytes())
        .with_context(|| format!("cannot write temporary metadata for {}", p.display()))?;
    temporary
        .as_file()
        .sync_all()
        .with_context(|| format!("cannot sync temporary metadata for {}", p.display()))?;
    temporary
        .persist(&p)
        .map_err(|error| error.error)
        .with_context(|| format!("cannot publish {}", p.display()))?;
    Ok(p)
}

fn ensure_regular_file_or_missing(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            Ok(true)
        }
        Ok(_) => anyhow::bail!(
            "refusing metadata path {}: expected a regular file, not a symlink/directory/special file",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("cannot inspect {}", path.display())),
    }
}

fn ensure_real_directory(path: &Path, create_if_missing: bool) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            Ok(())
        }
        Ok(_) => anyhow::bail!(
            "refusing metadata directory {}: it is a symlink or non-directory",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && create_if_missing => {
            match std::fs::create_dir(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("cannot create directory {}", path.display()));
                }
            }
            ensure_real_directory(path, false)
        }
        Err(error) => Err(error).with_context(|| format!("cannot inspect {}", path.display())),
    }
}

/// The meta invariants. Both the write-to-disk path and the tree-building (plumbing) path go
/// through here.
pub fn validate(meta: &Meta) -> Result<()> {
    if meta.is_file_line() {
        // A file line carrying a session declares itself a session — exactly the ambiguity
        // Line exists to remove.
        if !meta.session.is_empty() {
            anyhow::bail!("a file line never claims a session; got `{}`", meta.session);
        }
        return Ok(());
    }
    if meta.session.is_empty() {
        // Newborn: no identity claimed yet, and the runtime is optional.
        return Ok(());
    }
    if !is_bare_id(&meta.session) {
        anyhow::bail!(
            "session must be the root id claimed by this branch (`agit-` + 40 lowercase hex); got `{}`",
            meta.session
        );
    }
    // An empty runtime can never be supplied from elsewhere: the envelope's `_source` and VIEW
    // slicing both parse against it.
    if meta.runtime.is_empty() {
        anyhow::bail!("a claimed session line must record its runtime");
    }
    Ok(())
}

/// Serialize to the bytes that land in the tree (one trailing newline, byte-for-byte identical
/// to [`write`]).
pub fn to_text(meta: &Meta) -> Result<String> {
    validate(meta)?;
    Ok(format!("{}\n", serde_json::to_string_pretty(meta)?))
}

/// Canonical result of upgrading a v0 metadata blob without discarding extension fields.
#[derive(Debug, Clone)]
pub struct StorageMigrationMeta {
    pub snapshot: Meta,
    pub text: String,
}

/// Turn raw v0 metadata into the exact v1 bytes shared by local and backend migration.
///
/// Parsing only into [`Meta`] would silently discard fields written by a newer producer.  Keep
/// the complete JSON value, validate the known schema through [`Meta`], mutate the three migration
/// declarations, then recursively sort object keys before pretty serialization.  The output is
/// therefore independent of input key order and serde map implementation while preserving every
/// unknown value.
pub fn storage_migration_meta(raw: &str) -> Result<StorageMigrationMeta> {
    let mut value: serde_json::Value =
        serde_json::from_str(raw).context("invalid v0 session metadata JSON")?;
    anyhow::ensure!(value.is_object(), "session metadata must be a JSON object");
    let mut snapshot: Meta = serde_json::from_value(value.clone())
        .context("invalid known fields in v0 session metadata")?;
    validate(&snapshot).context("invalid v0 session metadata")?;
    anyhow::ensure!(snapshot.layout == LayoutVersion::V0, "metadata is not v0");
    if snapshot.is_session_line() && !snapshot.session.is_empty() {
        anyhow::ensure!(
            !snapshot.cwd.trim().is_empty(),
            "a claimed v0 session line must record its cwd"
        );
    }

    let object = value
        .as_object_mut()
        .expect("object shape was checked above");
    object.insert("layout".into(), serde_json::Value::String("v1".into()));
    object.insert("kind".into(), serde_json::Value::String("file".into()));
    object.insert(
        "milestone".into(),
        serde_json::Value::String(STORAGE_MIGRATION_MILESTONE.into()),
    );
    sort_json_objects(&mut value);
    let mut text = serde_json::to_string_pretty(&value)?;
    text.push('\n');

    snapshot.layout = LayoutVersion::V1;
    snapshot.kind = Kind::File;
    snapshot.milestone = Some(STORAGE_MIGRATION_MILESTONE.into());
    validate(&snapshot).context("migrated session metadata is invalid")?;
    Ok(StorageMigrationMeta { snapshot, text })
}

fn sort_json_objects(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            let mut entries: Vec<_> = std::mem::take(object).into_iter().collect();
            entries.sort_by(|(left, _), (right, _)| left.cmp(right));
            for (key, mut child) in entries {
                sort_json_objects(&mut child);
                object.insert(key, child);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                sort_json_objects(item);
            }
        }
        _ => {}
    }
}

/// Read `session/meta.json` from the worktree.
pub fn read(repo_root: &Path) -> Option<Meta> {
    serde_json::from_str(&std::fs::read_to_string(path_in(repo_root)).ok()?).ok()
}

/// Read the `session/meta.json` at a git ref (without switching the worktree).
pub fn read_at_ref(repo: &crate::domain::repo::Repo, git_ref: &str) -> Option<Meta> {
    read_at_ref_result(repo, git_ref).ok().flatten()
}

/// Strictly read a ref's `session/meta.json` without collapsing corruption into absence.
///
/// `Ok(None)` means only that the exact path is absent from an otherwise-readable commit. Git
/// failures, non-UTF-8 bytes, malformed JSON and violated [`Meta`] invariants are errors. Mutation
/// paths must use this API so a damaged declaration can never silently fall back to v0 semantics.
pub fn read_at_ref_result(repo: &crate::domain::repo::Repo, git_ref: &str) -> Result<Option<Meta>> {
    let Some(text) = repo.show_raw_result(git_ref, FILE)? else {
        return Ok(None);
    };
    parse_strict(&text, git_ref).map(Some)
}

/// Strictly parse one `session/meta.json` body read from `at` (a ref or sha, for the error message).
///
/// Malformed JSON and violated [`Meta`] invariants are errors, never absence: this is the same
/// judgement [`read_at_ref_result`] makes, exposed for callers that fetch the bodies in a batch.
pub fn parse_strict(text: &str, at: &str) -> Result<Meta> {
    let snapshot: Meta =
        serde_json::from_str(text).with_context(|| format!("invalid {FILE} JSON at {at}"))?;
    validate(&snapshot).with_context(|| format!("invalid {FILE} metadata at {at}"))?;
    Ok(snapshot)
}

/// The branch shape at a ref. No readable meta = no shape declaration at that point (an
/// incomplete checkout, or a commit agit did not make) — every caller decides what to say about
/// that; this does not guess for it.
pub fn line_at_ref(repo: &crate::domain::repo::Repo, git_ref: &str) -> Option<Line> {
    read_at_ref(repo, git_ref).map(|m| m.line)
}

/// The `session/meta.json` of a batch of refs, read in one `git cat-file`.
///
/// The result has **the same order and length** as `refs`; `None` means what it means in
/// [`read_at_ref`]: no declaration at that point, or a declaration that cannot be read.
///
/// # Why not call [`read_at_ref`] in a loop
///
/// "which line is this branch" and "which turn is this commit" both ask once per **object**. The
/// cost of one `git show` per object sits almost entirely in starting the process, so it grows
/// linearly with the number of objects; the two batch calls below spend a constant number of
/// processes, independent of that number. The list screen walks this on every rebuild, and the
/// discipline in `docs/07_tui.md` §4.1 watches exactly this kind of cost that grows with the
/// count.
///
/// Any failure falls back to a whole row of `None`: these answers display counts and block
/// picking a wrong starting point, and guessing one is worse than admitting ignorance.
pub fn at_refs(repo: &crate::domain::repo::Repo, refs: &[String]) -> Vec<Option<Meta>> {
    let mut out: Vec<Option<Meta>> = refs.iter().map(|_| None).collect();
    if refs.is_empty() {
        return out;
    }
    let names: Vec<String> = refs.iter().map(|r| format!("{r}:{FILE}")).collect();

    // Ask "is it there" first. `cat-file --batch` aborts **the whole batch** on an object it
    // cannot read, and "no meta at this point" is a normal case that must not carry away the
    // answers for the other objects in the batch.
    let mut present: Vec<bool> = Vec::with_capacity(names.len());
    let checked = repo.git_cat_file_batch_check(names.clone(), |_, kind, _| {
        present.push(kind != "missing");
        Ok(())
    });
    if checked.is_err() || present.len() != names.len() {
        return out;
    }

    let wanted: Vec<String> = names
        .iter()
        .zip(&present)
        .filter(|(_, ok)| **ok)
        .map(|(n, _)| n.clone())
        .collect();
    let mut read: Vec<Option<Meta>> = Vec::with_capacity(wanted.len());
    // The cap is `usize::MAX`: this layer has no "too big to read" requirement, and setting a
    // cap costs one more `--batch-check` pass (see `Repo::git_cat_file_batch`) — exactly what is
    // being saved here.
    let got = repo.git_cat_file_batch(wanted, usize::MAX, |_, _, body| {
        let crate::domain::repo::ObjectBody::Read(bytes) = body else {
            read.push(None);
            return Ok(());
        };
        read.push(
            std::str::from_utf8(bytes)
                .ok()
                .and_then(|t| serde_json::from_str::<Meta>(t).ok())
                .filter(|m| validate(m).is_ok()),
        );
        Ok(())
    });
    if got.is_err() {
        return out;
    }

    // The two sides share one order: each true slot in `present` corresponds, in order, to one
    // entry in `read`.
    let mut answers = read.into_iter();
    for (slot, ok) in out.iter_mut().zip(present) {
        if ok {
            *slot = answers.next().flatten();
        }
    }
    out
}

/// The branch shape of each ref in a batch. A thin wrapper over [`at_refs`].
pub fn lines_at_refs(repo: &crate::domain::repo::Repo, refs: &[String]) -> Vec<Option<Line>> {
    at_refs(repo, refs)
        .into_iter()
        .map(|m| m.map(|m| m.line))
        .collect()
}

/// Whether this ref is a file line. Unreadable meta is **not** one — taking "no declaration"
/// for a file line is what deadlocks W1.
pub fn is_file_line_at(repo: &crate::domain::repo::Repo, git_ref: &str) -> bool {
    line_at_ref(repo, git_ref) == Some(Line::File)
}

/// Read the meta from the worktree.
pub fn resolve(repo_root: &Path) -> Result<Meta> {
    ensure_write_safe(repo_root)?;
    let path = path_in(repo_root);
    let bytes = std::fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?;
    let text = String::from_utf8(bytes)
        .with_context(|| format!("{} is not valid UTF-8", path.display()))?;
    let snapshot: Meta = serde_json::from_str(&text)
        .with_context(|| format!("invalid {FILE} JSON in {}", repo_root.display()))?;
    validate(&snapshot)
        .with_context(|| format!("invalid {FILE} metadata in {}", repo_root.display()))?;
    Ok(snapshot)
}

/// The code state matching a working directory, `<origin>@<short-sha>`.
///
/// Missing either half returns None: a sha alone cannot be resolved by anyone else (nothing says
/// which repo to look in), and an origin alone matches no particular version. Half an answer is
/// easier to misuse than no answer.
pub fn code_of(cwd: &Path) -> Option<String> {
    let origin = git_field(cwd, &["remote", "get-url", "origin"])?;
    let sha = git_field(cwd, &["rev-parse", "--short", "HEAD"])?;
    Some(format!("{origin}@{sha}"))
}

/// Read the worktree status without retaining the paths and contents from Git's output.
pub fn cwd_state_of(cwd: &Path) -> Option<CwdState> {
    git_field(cwd, &["rev-parse", "--show-toplevel"])?;
    let origin = git_field(cwd, &["remote", "get-url", "origin"]);
    let head = git_field(cwd, &["rev-parse", "--verify", "HEAD"]);
    let branch = git_field(cwd, &["symbolic-ref", "--quiet", "--short", "HEAD"]);
    if configured_clean_filters(cwd).is_some() {
        return Some(CwdState::unknown(origin, head, branch));
    }

    let (worktree, staged, unstaged, untracked, conflicted, status_digest) =
        read_worktree_status(cwd).unwrap_or((WorktreeStatus::Unknown, 0, 0, 0, 0, None));
    Some(CwdState {
        origin,
        head,
        branch,
        worktree,
        staged,
        unstaged,
        untracked,
        conflicted,
        status_digest,
    })
}

/// Query the repository-level filter configuration; where a filter exists, status capture must
/// keep away from `git status`.
pub(crate) fn configured_clean_filters(cwd: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(GIT_SAFE)
        .args([
            "config",
            "--local",
            "--get-regexp",
            r"^filter\..*\.(clean|smudge|process)$",
        ])
        .current_dir(cwd)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let names: std::collections::BTreeSet<&str> = text
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter_map(|key| key.strip_prefix("filter."))
        .filter_map(|key| key.rsplit_once('.').map(|(name, _)| name))
        .collect();
    (!names.is_empty()).then(|| names.into_iter().collect::<Vec<_>>().join(", "))
}

fn read_worktree_status(
    cwd: &Path,
) -> Option<(WorktreeStatus, u32, u32, u32, u32, Option<String>)> {
    let mut child = std::process::Command::new("git")
        .args(GIT_SAFE)
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .current_dir(cwd)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let mut reader = BufReader::new(stdout);
    let mut record = Vec::new();
    let mut digest = Sha256::new();
    let mut staged: u32 = 0;
    let mut unstaged: u32 = 0;
    let mut untracked: u32 = 0;
    let mut conflicted: u32 = 0;
    let mut skip_rename_source = false;

    loop {
        record.clear();
        let bytes = reader.read_until(0, &mut record).ok()?;
        if bytes == 0 {
            break;
        }
        if record.last() == Some(&0) {
            record.pop();
        }
        if skip_rename_source {
            digest.update(&record);
            digest.update([0]);
            skip_rename_source = false;
            continue;
        }
        if record.len() < 3
            || record[2] != b' '
            || !is_porcelain_status_byte(record[0])
            || !is_porcelain_status_byte(record[1])
        {
            continue;
        }
        let x = record[0];
        let y = record[1];
        digest.update(&record);
        digest.update([0]);
        if x == b'R' || x == b'C' || y == b'R' || y == b'C' {
            skip_rename_source = true;
        }
        if x == b'?' && y == b'?' {
            untracked = untracked.saturating_add(1);
        } else {
            if x != b' ' && x != b'!' {
                staged = staged.saturating_add(1);
            }
            if y != b' ' && y != b'!' {
                unstaged = unstaged.saturating_add(1);
            }
        }
        if is_conflict_status(x, y) {
            conflicted = conflicted.saturating_add(1);
        }
    }

    if !child.wait().ok()?.success() {
        return None;
    }
    let worktree = if conflicted > 0 {
        WorktreeStatus::Conflicted
    } else if staged > 0 || unstaged > 0 || untracked > 0 {
        WorktreeStatus::Dirty
    } else {
        WorktreeStatus::Clean
    };
    Some((
        worktree,
        staged,
        unstaged,
        untracked,
        conflicted,
        Some(hex::encode(digest.finalize())),
    ))
}

fn is_porcelain_status_byte(byte: u8) -> bool {
    matches!(
        byte,
        b' ' | b'M' | b'A' | b'D' | b'R' | b'C' | b'U' | b'T' | b'?' | b'!'
    )
}

fn is_conflict_status(x: u8, y: u8) -> bool {
    x == b'U' || y == b'U' || (x == b'A' && y == b'A') || (x == b'D' && y == b'D')
}

/// The safe git arguments for running git inside a **directory the agent can write to**.
///
/// git reads that directory's `.git/config` and **executes** the programs it points at:
/// `core.fsmonitor` runs on every `status`, `core.pager` on every paging, `core.hooksPath`
/// decides where hooks come from, and `protocol.ext` turns a remote URL into a command. agitd
/// runs `git remote get-url origin` / `rev-parse` / `status --porcelain` inside the project
/// directory at every settlement — so between "write one config line into the repo" and "have
/// the daemon run any command for me" there is **no second approval**.
///
/// This gate and the approval classifier are two independent lines of defence, and neither can
/// be dropped: under `auto` / `accept_edits` claude-code never sends `can_use_tool`, so the
/// classifier is not called once, while this still runs.
///
/// A `-c` value given on the command line beats the one in the repo; `GIT_CONFIG_NOSYSTEM` cuts
/// off `/etc/gitconfig`.
pub(crate) const GIT_SAFE: &[&str] = &[
    "-c",
    "core.fsmonitor=",
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.pager=cat",
    "-c",
    "core.editor=true",
    "-c",
    "protocol.ext.allow=never",
    "--no-optional-locks",
];

fn git_field(dir: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(GIT_SAFE)
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
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

    fn meta(session: &str) -> Meta {
        Meta::new(session.into(), "codex".into(), "/r".into())
    }

    fn fake_id(c: char) -> String {
        format!("{ID_PREFIX}{}", c.to_string().repeat(40))
    }

    fn git_for_test(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(GIT_SAFE)
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn git_try_for_test(dir: &Path, args: &[&str]) -> bool {
        std::process::Command::new("git")
            .args(GIT_SAFE)
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    }

    #[test]
    fn cwd_state_captures_identity_and_bounded_status_summary() {
        let dir = tempfile::tempdir().unwrap();
        git_for_test(dir.path(), &["init", "-q"]);
        git_for_test(dir.path(), &["config", "user.name", "Test"]);
        git_for_test(dir.path(), &["config", "user.email", "test@example.com"]);
        git_for_test(
            dir.path(),
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/team/app.git",
            ],
        );
        std::fs::write(dir.path().join("tracked.txt"), "clean\n").unwrap();
        git_for_test(dir.path(), &["add", "tracked.txt"]);
        git_for_test(dir.path(), &["commit", "-q", "-m", "seed"]);
        git_for_test(dir.path(), &["branch", "-M", "main"]);

        let clean = cwd_state_of(dir.path()).unwrap();
        let head = git_for_test(dir.path(), &["rev-parse", "HEAD"]);
        assert_eq!(
            clean.origin.as_deref(),
            Some("https://example.invalid/team/app.git")
        );
        assert_eq!(clean.head.as_deref(), Some(head.as_str()));
        assert_eq!(clean.branch.as_deref(), Some("main"));
        assert_eq!(clean.worktree, WorktreeStatus::Clean);
        assert_eq!(
            (
                clean.staged,
                clean.unstaged,
                clean.untracked,
                clean.conflicted
            ),
            (0, 0, 0, 0)
        );
        assert!(clean.status_digest.is_some());

        std::fs::write(dir.path().join("tracked.txt"), "staged\n").unwrap();
        git_for_test(dir.path(), &["add", "tracked.txt"]);
        std::fs::write(dir.path().join("tracked.txt"), "unstaged\n").unwrap();
        std::fs::write(dir.path().join("untracked.txt"), "new\n").unwrap();
        let dirty = cwd_state_of(dir.path()).unwrap();
        assert_eq!(dirty.worktree, WorktreeStatus::Dirty);
        assert_eq!(
            (
                dirty.staged,
                dirty.unstaged,
                dirty.untracked,
                dirty.conflicted
            ),
            (1, 1, 1, 0)
        );
        assert_ne!(dirty.status_digest, clean.status_digest);
    }

    #[test]
    fn cwd_state_marks_unmerged_paths_as_conflicted() {
        let dir = tempfile::tempdir().unwrap();
        git_for_test(dir.path(), &["init", "-q"]);
        git_for_test(dir.path(), &["config", "user.name", "Test"]);
        git_for_test(dir.path(), &["config", "user.email", "test@example.com"]);
        std::fs::write(dir.path().join("conflict.txt"), "base\n").unwrap();
        git_for_test(dir.path(), &["add", "conflict.txt"]);
        git_for_test(dir.path(), &["commit", "-q", "-m", "base"]);
        git_for_test(dir.path(), &["branch", "-M", "main"]);
        git_for_test(dir.path(), &["switch", "-q", "-c", "topic"]);
        std::fs::write(dir.path().join("conflict.txt"), "topic\n").unwrap();
        git_for_test(dir.path(), &["commit", "-q", "-am", "topic"]);
        git_for_test(dir.path(), &["switch", "-q", "main"]);
        std::fs::write(dir.path().join("conflict.txt"), "main\n").unwrap();
        git_for_test(dir.path(), &["commit", "-q", "-am", "main"]);
        assert!(!git_try_for_test(
            dir.path(),
            &["merge", "--no-edit", "topic"]
        ));

        let state = cwd_state_of(dir.path()).unwrap();
        assert_eq!(state.worktree, WorktreeStatus::Conflicted);
        assert_eq!(state.conflicted, 1);
        assert_eq!((state.staged, state.unstaged, state.untracked), (1, 1, 0));
    }

    #[test]
    fn cwd_state_does_not_claim_filter_worktrees_are_equal() {
        let dir = tempfile::tempdir().unwrap();
        git_for_test(dir.path(), &["init", "-q"]);
        git_for_test(dir.path(), &["config", "user.name", "Test"]);
        git_for_test(dir.path(), &["config", "user.email", "test@example.com"]);
        std::fs::write(dir.path().join("tracked.txt"), "base\n").unwrap();
        git_for_test(dir.path(), &["add", "tracked.txt"]);
        git_for_test(dir.path(), &["commit", "-q", "-m", "base"]);
        git_for_test(
            dir.path(),
            &["config", "filter.agent-controlled.clean", "false"],
        );

        let state = cwd_state_of(dir.path()).unwrap();
        assert_eq!(state.worktree, WorktreeStatus::Unknown);
        assert_eq!(
            (
                state.staged,
                state.unstaged,
                state.untracked,
                state.conflicted
            ),
            (0, 0, 0, 0)
        );
        assert!(state.status_digest.is_none());
    }

    #[test]
    fn cwd_state_is_an_optional_metadata_extension() {
        let mut snapshot = meta(&fake_id('a'));
        snapshot.cwd_state = Some(CwdState {
            origin: Some("https://example.invalid/team/app.git".into()),
            head: Some("a".repeat(40)),
            branch: Some("main".into()),
            worktree: WorktreeStatus::Dirty,
            staged: 1,
            unstaged: 2,
            untracked: 3,
            conflicted: 0,
            status_digest: Some("b".repeat(64)),
        });

        let text = to_text(&snapshot).unwrap();
        let parsed = parse_strict(&text, "test").unwrap();
        assert_eq!(parsed.cwd_state, snapshot.cwd_state);
        assert!(
            serde_json::from_str::<serde_json::Value>(&text)
                .unwrap()
                .get("cwd_state")
                .is_some()
        );

        let old = r#"{"line":"session","session":"agit-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","runtime":"codex","cwd":"/r"}"#;
        assert!(parse_strict(old, "old").is_ok());

        let renamed = format!(
            r#"{{"line":"session","session":"agit-{}","runtime":"codex","cwd":"/r","code_state":{{"worktree":"clean"}}}}"#,
            "a".repeat(40)
        );
        let parsed_old_name = parse_strict(&renamed, "old-code-state").unwrap();
        assert_eq!(
            parsed_old_name
                .cwd_state
                .as_ref()
                .map(|state| state.worktree),
            Some(WorktreeStatus::Clean)
        );
    }

    #[test]
    fn cwd_state_is_absent_outside_a_git_repository() {
        let dir = tempfile::tempdir().unwrap();
        assert!(cwd_state_of(dir.path()).is_none());
    }

    #[test]
    fn session_hash_is_stable_and_deterministic() {
        let h = session_hash("/w", b"hello");
        assert!(h.starts_with(ID_PREFIX));
        assert_eq!(h.len(), ID_PREFIX.len() + ID_HEX_LEN);
        assert_eq!(h, session_hash("/w", b"hello"), "the hash is deterministic");
        assert_ne!(
            h,
            session_hash("/w", b"world"),
            "different content gives a different hash"
        );
        assert_ne!(
            h,
            session_hash("/other", b"hello"),
            "a different cwd gives a different hash"
        );
    }

    #[test]
    fn id_from_sha_and_back() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let id = id_from_sha(sha);
        assert_eq!(id, "agit-0123456789abcdef0123456789abcdef01234567");
        assert_eq!(sha_from_id(&id), Some(sha));
        assert_eq!(sha_from_id("no-prefix"), None);
    }

    /// A minted session id passes the check it is minted for.
    ///
    /// This pins a real bug shape: an id assembled in `rc` from `uuid::Uuid::new_v4().simple()`
    /// carries only 32 hex characters — short of [`ID_HEX_LEN`] — so `is_bare_id` returns false
    /// for every session id the daemon generates. A `[..40.min(32)]` slice is the mark of wanting
    /// 40 and getting 32, and clippy's `40 is never smaller than 32` points straight at it — a
    /// warning that `allow_failure` hides.
    #[test]
    fn a_minted_session_id_passes_its_own_check() {
        for _ in 0..64 {
            let id = mint_session_id();
            assert!(
                is_bare_id(&id),
                "a minted id passes is_bare_id: {id} (hex length {})",
                id.len() - ID_PREFIX.len()
            );
        }
        // Two mints must not collide — it is an identifier, not a digest.
        assert_ne!(mint_session_id(), mint_session_id());
    }

    #[test]
    fn is_bare_id_checks_shape() {
        assert!(is_bare_id(&format!("{ID_PREFIX}{}", "a".repeat(40))));
        assert!(!is_bare_id(&format!("{ID_PREFIX}{}", "A".repeat(40)))); // uppercase
        assert!(!is_bare_id(&format!("{ID_PREFIX}{}", "a".repeat(39)))); // too short
        assert!(!is_bare_id("no-prefix"));
    }

    #[test]
    fn event_path_validates_and_shards_the_full_id() {
        let id = "3f7a1c9e0123456789abcdef0123456789abcdef";
        assert_eq!(event_path(id).unwrap(), format!("events/3/f/7/a/{id}"));
        assert!(event_path(&"a".repeat(39)).is_err());
        assert!(event_path(&"A".repeat(40)).is_err());
        assert!(event_path("../../../../etc/passwd").is_err());
    }

    #[test]
    fn storage_migration_meta_is_order_independent_and_preserves_extensions() {
        let session = fake_id('a');
        let first = format!(
            r#"{{"unknown":{{"z":1,"a":2}},"turn":1,"kind":"turn","cwd":"/r","runtime":"codex","session":"{session}","line":"session"}}"#
        );
        let second = format!(
            r#"{{"line":"session","session":"{session}","runtime":"codex","cwd":"/r","kind":"turn","turn":1,"unknown":{{"a":2,"z":1}}}}"#
        );

        let one = storage_migration_meta(&first).unwrap();
        let two = storage_migration_meta(&second).unwrap();
        assert_eq!(one.text, two.text);
        assert_eq!(one.snapshot.layout, LayoutVersion::V1);
        assert_eq!(one.snapshot.kind, Kind::File);
        let value: serde_json::Value = serde_json::from_str(&one.text).unwrap();
        assert_eq!(value["unknown"]["a"], 2);
        assert_eq!(value["unknown"]["z"], 1);
        assert_eq!(value["milestone"], STORAGE_MIGRATION_MILESTONE);
    }

    #[test]
    fn storage_paths_cover_both_layouts_without_prefix_false_positives() {
        for path in [
            FILE,
            LOG_FILE,
            VIEW_FILE,
            LEGACY_LOG_FILE,
            LEGACY_VIEW_FILE,
            EVENTS_DIR,
            "events/a/b/c/d/deadbeef",
        ] {
            assert!(is_storage_path(path), "{path}");
        }
        assert!(!is_storage_path(ATTRS_FILE));
        assert!(!is_storage_path("events-not-managed/file"));
        assert!(!is_storage_path("memory/MEMORY.md"));

        for user_path in [LOG_FILE, VIEW_FILE, EVENTS_DIR, "events/user.txt"] {
            assert!(
                !is_storage_path_for(LayoutVersion::V0, user_path),
                "v0 did not reserve {user_path}"
            );
            assert!(is_storage_path_for(LayoutVersion::V1, user_path));
        }
        assert!(!is_storage_path_for(LayoutVersion::V0, ATTRS_FILE));
        assert!(!is_storage_path_for(LayoutVersion::V1, ATTRS_FILE));
        for legacy in [FILE, LEGACY_LOG_FILE, LEGACY_VIEW_FILE] {
            assert!(is_storage_path_for(LayoutVersion::V0, legacy));
        }
    }

    #[test]
    fn missing_layout_means_v0_but_new_meta_is_v1() {
        let old: Meta =
            serde_json::from_str(r#"{"line":"session","session":"","runtime":"codex","cwd":"/r"}"#)
                .unwrap();
        assert_eq!(old.layout, LayoutVersion::V0);

        let new = Meta::new_session_line("codex".into(), "/r".into());
        assert_eq!(new.layout, LayoutVersion::V1);
        assert!(to_text(&new).unwrap().contains("\"layout\": \"v1\""));
    }

    #[test]
    fn write_and_read_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let m = meta(&fake_id('a'));
        let p = write(d.path(), &m).unwrap();
        assert!(
            p.ends_with("session/meta.json"),
            "the write lands at {FILE}: {}",
            p.display()
        );
        let back = read(d.path()).unwrap();
        assert_eq!(back.session, m.session);
        assert_eq!(back.cwd, m.cwd);
        assert_eq!(back.runtime, m.runtime);
        assert_eq!(
            back.line,
            Line::Session,
            "the default shape is a session line"
        );
        assert_eq!(back.cwd_state, None);
    }

    /// Reading shapes in a batch: same order and length, `None` for the ref that declares
    /// nothing, and **no other answer in the batch goes down with it**.
    ///
    /// The middle ref carrying no meta is deliberate: `cat-file --batch` aborts the whole batch on
    /// an object it cannot read, so this pins that one incomplete branch must not zero the counts
    /// for the whole repo.
    #[test]
    fn lines_are_read_in_one_batch_and_a_bare_branch_does_not_take_the_others_down() {
        let d = tempfile::tempdir().unwrap();
        let repo = crate::domain::repo::Repo::init(d.path()).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();

        write(d.path(), &Meta::new_file_line()).unwrap();
        repo.add_all().unwrap();
        repo.commit("file line").unwrap();

        repo.git(&["checkout", "--quiet", "-b", "bare"]).unwrap();
        std::fs::remove_file(path_in(d.path())).unwrap();
        repo.git(&["add", "-A"]).unwrap();
        repo.commit("no declaration").unwrap();

        repo.git(&["checkout", "--quiet", "-b", "work", "main"])
            .unwrap();
        write(
            d.path(),
            &Meta::new_session_line("codex".into(), "/r".into()),
        )
        .unwrap();
        repo.add_all().unwrap();
        repo.commit("session line").unwrap();

        let refs: Vec<String> = ["main", "bare", "work", "no-such-branch"]
            .iter()
            .map(|b| format!("refs/heads/{b}"))
            .collect();
        assert_eq!(
            lines_at_refs(&repo, &refs),
            vec![Some(Line::File), None, Some(Line::Session), None],
            "same order and length; an unreadable ref gives None"
        );
        // The batched answers match the one-by-one reads — batching saves processes, it does
        // not swap in another test.
        for (r, batched) in refs.iter().zip(lines_at_refs(&repo, &refs)) {
            assert_eq!(line_at_ref(&repo, r), batched, "{r}");
        }
        assert!(lines_at_refs(&repo, &[]).is_empty());
    }

    #[test]
    fn strict_ref_read_distinguishes_missing_json_and_validation_errors() {
        let d = tempfile::tempdir().unwrap();
        let repo = crate::domain::repo::Repo::init(d.path()).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        std::fs::write(d.path().join("README.md"), "plain tree\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("no meta").unwrap();
        assert!(read_at_ref_result(&repo, "HEAD").unwrap().is_none());

        ensure_session_dir(d.path()).unwrap();
        std::fs::write(path_in(d.path()), "{not-json\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("bad json").unwrap();
        let json_error = read_at_ref_result(&repo, "HEAD").unwrap_err();
        assert!(json_error.to_string().contains("JSON"), "{json_error:#}");

        std::fs::write(
            path_in(d.path()),
            r#"{"layout":"v1","line":"session","session":"bad","runtime":"codex","cwd":"/r","kind":"turn"}"#,
        )
        .unwrap();
        repo.add_all().unwrap();
        repo.commit("bad invariant").unwrap();
        let validation_error = read_at_ref_result(&repo, "HEAD").unwrap_err();
        assert!(
            validation_error.to_string().contains("metadata"),
            "{validation_error:#}"
        );
        let worktree_error = resolve(d.path()).unwrap_err();
        assert!(
            worktree_error.to_string().contains("metadata"),
            "{worktree_error:#}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn metadata_writer_rejects_symlinked_directory_or_file_without_external_write() {
        use std::os::unix::fs::symlink;

        let directory_case = tempfile::tempdir().unwrap();
        let outside_directory = tempfile::tempdir().unwrap();
        symlink(
            outside_directory.path(),
            directory_case.path().join("session"),
        )
        .unwrap();
        let error = write(directory_case.path(), &Meta::new_file_line()).unwrap_err();
        assert!(error.to_string().contains("symlink"), "{error:#}");
        assert_eq!(
            std::fs::read_dir(outside_directory.path()).unwrap().count(),
            0
        );

        let file_case = tempfile::tempdir().unwrap();
        std::fs::create_dir(file_case.path().join("session")).unwrap();
        let outside_file = file_case.path().join("outside-meta-target");
        std::fs::write(&outside_file, "outside bytes\n").unwrap();
        symlink(&outside_file, path_in(file_case.path())).unwrap();
        let error = write(file_case.path(), &Meta::new_file_line()).unwrap_err();
        assert!(error.to_string().contains("regular file"), "{error:#}");
        assert_eq!(
            std::fs::read_to_string(outside_file).unwrap(),
            "outside bytes\n"
        );
    }

    /// A file line has a meta.json too, and its shape is written in, not inferred.
    #[test]
    fn a_file_line_meta_declares_itself_and_claims_nothing() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), &Meta::new_file_line()).unwrap();
        let back = read(d.path()).unwrap();
        assert!(back.is_file_line());
        assert!(back.session.is_empty() && back.runtime.is_empty());
        // The serialization carries no empty-string key — it would make "no identity" look like
        // "the identity is empty".
        let text = std::fs::read_to_string(path_in(d.path())).unwrap();
        assert!(text.contains("\"line\": \"file\""), "{text}");
        assert!(!text.contains("\"session\""), "{text}");
    }

    /// A session line is born with its shape fixed and its identity open — the state between
    /// import creating the branch and the first settlement; misjudging it as a file line is the
    /// W1 deadlock.
    #[test]
    fn a_newborn_session_line_is_valid_without_a_claim() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            &Meta::new_session_line("codex".into(), "/r".into()),
        )
        .unwrap();
        let back = read(d.path()).unwrap();
        assert!(back.is_session_line());
        assert!(back.session.is_empty());
    }

    #[test]
    fn write_refuses_an_empty_runtime_on_a_claimed_line() {
        let d = tempfile::tempdir().unwrap();
        let mut m = meta(&fake_id('7'));
        m.runtime = String::new();
        let e = write(d.path(), &m).unwrap_err().to_string();
        assert!(e.contains("runtime"), "{e}");
    }

    #[test]
    fn write_refuses_a_session_that_is_not_the_branch_claim() {
        let d = tempfile::tempdir().unwrap();
        let m = meta("sessions/codex/AB.jsonl");
        let e = write(d.path(), &m).unwrap_err().to_string();
        assert!(e.contains("agit-"), "{e}");
    }

    /// A file line claiming a session contradicts its own shape and cannot be written.
    #[test]
    fn write_refuses_a_file_line_that_claims_a_session() {
        let d = tempfile::tempdir().unwrap();
        let mut m = Meta::new_file_line();
        m.session = fake_id('c');
        let e = write(d.path(), &m).unwrap_err().to_string();
        assert!(e.contains("file line"), "{e}");
    }

    #[test]
    fn short_trims_to_prefix_plus_8() {
        let id = format!("{ID_PREFIX}{}", "3f9c8a12".repeat(5));
        assert_eq!(short(&id), "agit-3f9c8a12");
    }
}
