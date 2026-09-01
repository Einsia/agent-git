//! git plumbing pieces: build commits without touching the worktree or the checkout.
//!
//! fork / merge / seal share one need: the target branch may be exactly the one the worktree has
//! checked out, but more often it is not — and "`git checkout` that branch → edit files → commit
//! → switch back" leaves a mess when any step fails. plumbing (hash-object / commit-tree /
//! update-ref) does not have that problem, and carries its own expected-head CAS.

use crate::Result;
use crate::domain::meta;
use crate::domain::repo::Repo;
use crate::domain::storage;
use anyhow::Context as _;
use fs2::FileExt as _;
use sha2::{Digest as _, Sha256};
use std::io::{BufRead as _, Read, Write as _};

const CHECKOUT_JOURNAL_VERSION: u32 = 4;
const CHECKOUT_JOURNAL_INLINE_VERSION: u32 = 3;
const CHECKOUT_JOURNAL_OID_VERSION: u32 = 2;
const CHECKOUT_JOURNAL_LEGACY_VERSION: u32 = 1;
const CHECKOUT_JOURNAL_NAME: &str = "agit-checkout-transaction.json";
const CHECKOUT_ATTRIBUTES_SIDECAR_NAME: &str = "agit-checkout-attributes.snapshot";
const CHECKOUT_LOCK_NAME: &str = "agit-checkout-transaction.lock";
const MAX_CHECKOUT_JOURNAL_BYTES: u64 = 16 * 1024;
// `.gitattributes` was unrestricted user state in v0. Keep the checkout subsystem's existing
// materialization boundary instead of imposing the journal's tiny metadata bound on user data.
// Normalization can append AgentGit's fixed managed block, hence the small headroom.
const MAX_CHECKOUT_ATTRIBUTES_LAYER_BYTES: usize = storage::MAX_MATERIALIZED_BYTES + 16 * 1024;
const MAX_CHECKOUT_ATTRIBUTES_SIDECAR_BYTES: u64 = (MAX_CHECKOUT_ATTRIBUTES_LAYER_BYTES as u64) * 2;

/// Read a tree path only when it is an ordinary non-executable blob.
///
/// Storage upgrades rewrite `.gitattributes`. In v0 that path was user-owned, so silently
/// replacing a symlink, gitlink, tree or executable file would lose tracked metadata even when
/// the bytes happened to be valid UTF-8. Callers use this boundary before generating the managed
/// attributes blocks.
pub fn regular_blob_text_at(repo: &Repo, treeish: &str, path: &str) -> Result<Option<String>> {
    let entry = repo.git_bytes_result(&["ls-tree", "-z", "--full-name", treeish, "--", path])?;
    if entry.is_empty() {
        return Ok(None);
    }
    anyhow::ensure!(
        entry.last() == Some(&0) && entry[..entry.len() - 1].iter().all(|byte| *byte != 0),
        "git returned multiple or unterminated tree entries for {treeish}:{path}"
    );
    let record = &entry[..entry.len() - 1];
    let tab = record
        .iter()
        .position(|byte| *byte == b'\t')
        .context("git returned a malformed ls-tree record")?;
    let header = std::str::from_utf8(&record[..tab]).context("ls-tree header is not UTF-8")?;
    anyhow::ensure!(
        &record[tab + 1..] == path.as_bytes(),
        "git returned the wrong tree path while reading {treeish}:{path}"
    );
    let mut fields = header.split_ascii_whitespace();
    let mode = fields.next().context("ls-tree record has no mode")?;
    let kind = fields.next().context("ls-tree record has no object type")?;
    let oid = fields.next().context("ls-tree record has no object id")?;
    anyhow::ensure!(fields.next().is_none(), "ls-tree record has extra fields");
    anyhow::ensure!(
        mode == "100644" && kind == "blob",
        "refusing to replace non-regular {treeish}:{path} (mode {mode}, type {kind})"
    );
    let bytes = repo.git_bytes_result(&["cat-file", "blob", oid])?;
    String::from_utf8(bytes)
        .with_context(|| format!("{treeish}:{path} is not UTF-8"))
        .map(Some)
}

/// A git call with optional stdin.
pub fn raw_git(repo: &Repo, args: &[&str], stdin: Option<&str>) -> Result<String> {
    let mut child = std::process::Command::new("git")
        .arg("--no-replace-objects")
        .args(args)
        .current_dir(repo.root())
        .stdin(if stdin.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    if let Some(input) = stdin {
        child
            .stdin
            .take()
            .expect("stdin already taken over")
            .write_all(input.as_bytes())?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// One tree surgery = modify or add one file. Returns the new tree id.
///
/// `content=None` deletes the path.
pub fn tree_with(
    repo: &Repo,
    base_commit: &str,
    path: &str,
    content: Option<&str>,
) -> Result<String> {
    // Via `git rev-parse --git-path`: a linked worktree's `.git` is a file, so `root/.git/...`
    // builds no path there.
    let idx = repo.git_path(&format!("agit-plumbing-{}.index", std::process::id()))?;
    let _ = std::fs::remove_file(&idx);
    let result = (|| -> Result<String> {
        let run = |args: &[&str]| -> Result<String> {
            let out = std::process::Command::new("git")
                .arg("--no-replace-objects")
                .args(args)
                .current_dir(repo.root())
                .env("GIT_INDEX_FILE", &idx)
                .output()?;
            if !out.status.success() {
                anyhow::bail!(
                    "git {} failed: {}",
                    args.join(" "),
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            Ok(String::from_utf8_lossy(&out.stdout).to_string())
        };
        run(&["read-tree", base_commit])?;
        match content {
            Some(text) => {
                let blob = raw_git(repo, &["hash-object", "-w", "--stdin"], Some(text))?;
                run(&[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &format!("100644,{},{}", blob.trim(), path),
                ])?;
            }
            None => {
                run(&["update-index", "--force-remove", path])?;
            }
        }
        Ok(run(&["write-tree"])?.trim().to_string())
    })();
    let _ = std::fs::remove_file(&idx);
    result
}

/// One tree surgery = several edits. In `edits`, a `None` content deletes that path.
pub fn tree_apply(
    repo: &Repo,
    base_commitish: &str,
    edits: &[(&str, Option<&str>)],
) -> Result<String> {
    // Via `git rev-parse --git-path`: a linked worktree's `.git` is a file, so `root/.git/...`
    // builds no path there.
    let idx = repo.git_path(&format!("agit-plumbing-{}.index", std::process::id()))?;
    let _ = std::fs::remove_file(&idx);
    let result = (|| -> Result<String> {
        let run = |args: &[&str]| -> Result<String> {
            let out = std::process::Command::new("git")
                .arg("--no-replace-objects")
                .args(args)
                .current_dir(repo.root())
                .env("GIT_INDEX_FILE", &idx)
                .output()?;
            if !out.status.success() {
                anyhow::bail!(
                    "git {} failed: {}",
                    args.join(" "),
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            Ok(String::from_utf8_lossy(&out.stdout).to_string())
        };
        run(&["read-tree", base_commitish])?;
        for (path, content) in edits {
            match content {
                Some(text) => {
                    let blob = raw_git(repo, &["hash-object", "-w", "--stdin"], Some(text))?;
                    run(&[
                        "update-index",
                        "--add",
                        "--cacheinfo",
                        &format!("100644,{},{}", blob.trim(), path),
                    ])?;
                }
                None => {
                    run(&["update-index", "--force-remove", path])?;
                }
            }
        }
        Ok(run(&["write-tree"])?.trim().to_string())
    })();
    let _ = std::fs::remove_file(&idx);
    result
}

/// Add and delete files in bulk on an existing tree, storing content as raw bytes.
///
/// `Some(bytes)` adds or replaces a file, `None` deletes it. Next to [`tree_apply`], this entry
/// point is for large, dynamically generated path sets (for example `events/a/b/c/d/<hash>`):
///
/// * all added blobs spawn `git hash-object --stdin-paths` only once;
/// * all index changes spawn `git update-index --index-info -z` only once;
/// * `--no-filters` keeps the incoming binary bytes from being rewritten by `.gitattributes`;
/// * temporary files and the temporary index live in one RAII directory, cleaned up on success
///   and on error alike;
/// * the real index, the worktree and the checkout are not touched by a single byte.
///
/// Paths use the git tree `/`-separated form. NUL cannot be expressed by `--index-info -z`, and
/// an empty path, an absolute path and a `.` / `..` component are not canonical in-repository
/// paths either, so they are rejected before git starts.
pub fn tree_apply_owned(
    repo: &Repo,
    base_commitish: &str,
    edits: Vec<(String, Option<Vec<u8>>)>,
) -> Result<String> {
    let mut paths = std::collections::BTreeSet::new();
    for (path, _) in &edits {
        validate_tree_path(path)?;
        if !paths.insert(path.as_str()) {
            anyhow::bail!("tree edit path is duplicated: {path:?}");
        }
    }
    for path in &paths {
        let mut parent = path.rsplit_once('/').map(|(parent, _)| parent);
        while let Some(candidate) = parent {
            if paths.contains(candidate) {
                anyhow::bail!(
                    "tree edit path collision: {candidate:?} is also the parent of {path:?}"
                );
            }
            parent = candidate.rsplit_once('/').map(|(next, _)| next);
        }
    }

    // Resolve to a tree oid first. Besides making read-tree's input unambiguous, its length
    // gives deletion records the null oid of this repository's object format (40 for SHA-1,
    // 64 for SHA-256).
    let tree_expr = format!("{base_commitish}^{{tree}}");
    let base_tree = repo.git(&["rev-parse", "--verify", &tree_expr])?;
    let base_tree = base_tree.trim();
    if edits.is_empty() {
        return Ok(base_tree.to_string());
    }
    if base_tree.is_empty() || !base_tree.bytes().all(|b| b.is_ascii_hexdigit()) {
        anyhow::bail!("git rev-parse returned an invalid tree object id");
    }

    let git_dir = repo.git(&["rev-parse", "--git-dir"])?;
    let git_dir = std::path::PathBuf::from(git_dir.trim());
    let git_dir = if git_dir.is_absolute() {
        git_dir
    } else {
        repo.root().join(git_dir)
    };
    // hash-object runs inside the scratch directory; GIT_DIR must be absolute, or a relative
    // `.git` points at scratch itself once current_dir has changed.
    let git_dir = std::fs::canonicalize(&git_dir)?;
    let scratch = tempfile::Builder::new()
        .prefix("agit-tree-")
        .tempdir_in(&git_dir)?;
    let index = scratch.path().join("index");

    run_with_index(repo, &index, &["read-tree", base_tree], None)?;

    // Feed --stdin-paths controlled ASCII temporary file names. A repository that itself sits
    // under a non-UTF-8 path therefore never has that path pushed lossily into stdin;
    // hash-object's cwd is the scratch directory.
    let mut hash_input = Vec::new();
    let mut additions = 0usize;
    for (i, (_, content)) in edits.iter().enumerate() {
        let Some(bytes) = content else { continue };
        let name = format!("blob-{i}");
        std::fs::write(scratch.path().join(&name), bytes)?;
        hash_input.extend_from_slice(name.as_bytes());
        hash_input.push(b'\n');
        additions += 1;
    }

    let hashes = if additions == 0 {
        Vec::new()
    } else {
        hash_owned_blobs(&git_dir, scratch.path(), &hash_input, additions)?
    };
    let mut hashes = hashes.into_iter();

    // An --index-info -z record is still `mode SP oid TAB path NUL`; -z only swaps the path
    // terminator from LF to NUL and turns off path quoting. A deletion uses mode 0 plus an
    // all-zero oid of the current object format's length, so tabs and newlines inside a path
    // survive byte for byte.
    let null_oid = "0".repeat(base_tree.len());
    let mut index_info = Vec::new();
    for (path, content) in &edits {
        match content {
            Some(_) => {
                let oid = hashes.next().ok_or_else(|| {
                    anyhow::anyhow!("git hash-object returned too few object ids")
                })?;
                index_info.extend_from_slice(b"100644 ");
                index_info.extend_from_slice(oid.as_bytes());
            }
            None => {
                index_info.extend_from_slice(b"0 ");
                index_info.extend_from_slice(null_oid.as_bytes());
            }
        }
        index_info.push(b'\t');
        index_info.extend_from_slice(path.as_bytes());
        index_info.push(0);
    }
    if hashes.next().is_some() {
        anyhow::bail!("git hash-object returned too many object ids");
    }

    run_with_index(
        repo,
        &index,
        &["update-index", "-z", "--index-info"],
        Some(&index_info),
    )?;
    let tree = run_with_index(repo, &index, &["write-tree"], None)?;
    Ok(String::from_utf8(tree)?.trim().to_string())
}

/// Rebuild a complete v1 session snapshot on top of `base_commitish`.
///
/// Every old/current managed storage path is first removed, then LOG/VIEW/events, meta and the
/// merged attributes rules are installed together. This is the shared upgrade path for view
/// surgery that may target a dirty-but-readable v0 branch after startup migration skipped it.
pub fn session_snapshot_tree(
    repo: &Repo,
    base_commitish: &str,
    log: &str,
    view: &str,
    meta_text: &str,
) -> Result<String> {
    let layout = storage_layout_at(repo, base_commitish)?;
    ensure_v1_upgrade_preflight(repo, base_commitish)?;
    let existing_attributes = regular_blob_text_at(repo, base_commitish, meta::ATTRS_FILE)?;
    let mut edits: std::collections::BTreeMap<String, Option<Vec<u8>>> = repo
        .ls_tree_result(base_commitish)?
        .into_iter()
        .filter(|path| meta::is_storage_path_for(layout, path))
        .map(|path| (path, None))
        .collect();
    for (path, bytes) in storage::snapshot_files(log, view)? {
        edits.insert(path, Some(bytes));
    }
    edits.insert(meta::FILE.to_owned(), Some(meta_text.as_bytes().to_vec()));
    edits.insert(
        meta::ATTRS_FILE.to_owned(),
        Some(storage::attributes_text_strict(existing_attributes.as_deref())?.into_bytes()),
    );
    tree_apply_owned(repo, base_commitish, edits.into_iter().collect())
}

/// Reject a v0 -> v1 write when the new root namespace is already user-owned.
///
/// These names were not reserved by v0. Replacing them would be irreversible data loss even when
/// the old commit remains reachable, because ignored/untracked worktree content is not in Git at
/// all. V1 snapshots have already crossed this boundary and legitimately contain these paths.
pub fn ensure_v1_namespace_available_at(repo: &Repo, commitish: &str) -> Result<()> {
    if storage_layout_at(repo, commitish)? == meta::LayoutVersion::V1 {
        return Ok(());
    }
    let tree = repo.git(&["ls-tree", "-r", "--name-only", commitish])?;
    let conflicts: Vec<String> = tree
        .lines()
        .filter(|path| is_v1_root_namespace(path))
        .map(str::to_owned)
        .collect();
    anyhow::ensure!(
        conflicts.is_empty(),
        "cannot upgrade v0 snapshot {commitish} to storage v1: the new storage namespace is already user-owned: {}",
        conflicts.join(", ")
    );
    Ok(())
}

/// Worktree half of [`ensure_v1_namespace_available_at`]. Uses symlink metadata so ignored files,
/// directories and symlinks are all visible; `git status` alone cannot provide this guarantee.
pub fn ensure_v1_namespace_available_in_worktree(repo: &Repo) -> Result<()> {
    let mut conflicts = Vec::new();
    for path in [meta::LOG_FILE, meta::VIEW_FILE, meta::EVENTS_DIR] {
        match std::fs::symlink_metadata(repo.root().join(path)) {
            Ok(_) => conflicts.push(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("cannot inspect {path}")),
        }
    }
    anyhow::ensure!(
        conflicts.is_empty(),
        "cannot upgrade the checked-out v0 branch to storage v1: these root paths already exist and may be user data: {}",
        conflicts.join(", ")
    );
    Ok(())
}

/// Shared fail-closed boundary for every command that creates or checks out a v1 snapshot.
///
/// The source tree must not already use names that v0 treated as user-owned. Independently, when
/// this repository's real checkout is still v0, its tracked and untracked/ignored filesystem
/// namespace must also be collision-free: a newly-created v1 branch can be checked out later in
/// the same command (resume/settlement), and Git is allowed to overwrite ignored files.
pub fn ensure_v1_upgrade_preflight(repo: &Repo, source: &str) -> Result<()> {
    ensure_v1_namespace_available_at(repo, source)?;
    let head = repo.git(&["rev-parse", "--verify", "HEAD^{commit}"])?;
    if storage_layout_at(repo, &head)? == meta::LayoutVersion::V0 {
        ensure_v1_namespace_available_at(repo, &head)?;
        ensure_v1_namespace_available_in_worktree(repo)?;
    }
    Ok(())
}

/// Fail-closed boundary before Git changes the active checkout to `target`.
///
/// A v0 checkout may contain tracked, untracked or ignored user data at names that v1 reserves.
/// Git's ordinary checkout can overwrite ignored paths, so every command that moves HEAD must run
/// this check before invoking checkout. When the current checkout is already v1, also prove its
/// content-addressed namespace has not been replaced with symlinks or unexpected files.
pub fn ensure_safe_checkout(repo: &Repo, target: &str) -> Result<()> {
    let target_layout = match meta::read_at_ref_result(repo, target)? {
        Some(snapshot) => snapshot.layout,
        None => {
            // Meta-less external history is still browseable when it does not claim the v1
            // namespace. A tree containing LOG/VIEW/events without its version declaration is
            // ambiguous and must never be allowed to overwrite ignored v0 user files.
            ensure_v1_namespace_available_at(repo, target)?;
            meta::LayoutVersion::V0
        }
    };
    if target_layout == meta::LayoutVersion::V1 {
        ensure_v1_upgrade_preflight(repo, target)?;
    }
    let head = repo.git(&["rev-parse", "--verify", "HEAD^{commit}"])?;
    if storage_layout_at(repo, &head)? == meta::LayoutVersion::V1 {
        ensure_v1_namespace_absent_or_matches(repo, &head)?;
    }
    Ok(())
}

pub(super) fn storage_layout_at(repo: &Repo, commitish: &str) -> Result<meta::LayoutVersion> {
    let Some(text) = repo.show_raw_result(commitish, meta::FILE)? else {
        return Ok(meta::LayoutVersion::V0);
    };
    let snapshot: meta::Meta = serde_json::from_str(&text)
        .with_context(|| format!("invalid {} at {commitish}", meta::FILE))?;
    meta::validate(&snapshot)
        .with_context(|| format!("invalid {} invariants at {commitish}", meta::FILE))?;
    Ok(snapshot.layout)
}

fn is_v1_root_namespace(path: &str) -> bool {
    matches!(path, meta::LOG_FILE | meta::VIEW_FILE)
        || path == meta::EVENTS_DIR
        || path
            .strip_prefix(meta::EVENTS_DIR)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

/// Align only AgentGit-owned storage paths in the real index/worktree to `commit`.
///
/// Shared files and their staging state are untouched. Missing paths are removed, present paths
/// are checked out from the immutable commit, and a final status check proves the owned surface is
/// clean before the caller reports success.
pub fn refresh_storage_checkout(repo: &Repo, old: &str, new: &str) -> Result<()> {
    refresh_checkout_transactionally(repo, old, new, CheckoutScope::Storage, false, None)
        .map_err(|failure| failure.error)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum CheckoutScope {
    Storage,
    Full,
    /// Publish the first commit of the checked-out branch. The pre-CAS checkout has no commit
    /// endpoint: recovery is therefore forward-only once the expected-absent CAS succeeds.
    Root,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckoutJournal {
    version: u32,
    branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    old: Option<String>,
    new: String,
    scope: CheckoutScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    attributes_upgrade: Option<CheckoutAttributesUpgrade>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CheckoutJournalPublication {
    /// The journal name never entered the directory, or its removal was followed by a successful
    /// directory fsync. The sidecar can therefore be removed without stranding a journal.
    DurablyAbsent,
    /// The journal was published and its name may survive a crash. Even when an attempted unlink
    /// is visible now, retain the sidecar until startup recovery observes the durable namespace.
    PublishedOrCleanupUncertain,
}

#[derive(Debug)]
struct CheckoutJournalWriteFailure {
    error: anyhow::Error,
    publication: CheckoutJournalPublication,
}

/// Durable metadata for the staged/worktree `.gitattributes` snapshot. A v4 journal keeps only a
/// digest and exact layer lengths here; the raw user bytes live in an fsynced, untracked sidecar.
/// The `index`/`worktree` fields decode legacy v2 object ids and v3 inline bytes.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckoutAttributesUpgrade {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    index: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    worktree: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sidecar: Option<CheckoutAttributesSidecar>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckoutAttributesSidecar {
    sha256: String,
    bytes: u64,
    index_bytes: Option<u64>,
    worktree_bytes: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AttributesLayers {
    index: Option<String>,
    worktree: Option<String>,
}

/// An exclusive per-checkout transaction. The file lock prevents a second AgentGit process from
/// mistaking a live transaction for a crash, while the journal itself survives process death.
pub(super) struct CheckoutTransaction {
    journal: CheckoutJournal,
    attributes_layers: Option<AttributesLayers>,
    path: std::path::PathBuf,
    sidecar_path: Option<std::path::PathBuf>,
    _lock: std::fs::File,
}

pub(super) struct CheckoutRefreshFailure {
    pub(super) error: anyhow::Error,
    restored: bool,
}

impl CheckoutRefreshFailure {
    pub(super) fn restored(&self) -> bool {
        self.restored
    }
}

/// Recover a transaction whose process stopped between the branch CAS and checkout refresh.
///
/// The ref is the durable decision: `new` converges every journalled path to the new tree and
/// `old` converges to the old tree. Each index/worktree path may already be at either endpoint,
/// which makes recovery safe after a stop in the middle of `checkout-index`. Any other ref,
/// checkout branch, or local path state is ambiguous and therefore fails closed.
pub fn recover_interrupted_checkout(repo: &Repo) -> Result<bool> {
    let lock = lock_checkout_transactions(repo)?;
    recover_interrupted_checkout_locked(repo, &lock)
}

/// Read-only preflight for startup recovery.
///
/// The ordinary recovery entry point necessarily creates/opens its lock and removes an orphaned
/// attributes sidecar. Startup callers use this probe first so a clean repository never needs a
/// writable Git directory merely to prove that there is no transaction to recover. File shape and
/// journal contents remain the locked recovery path's responsibility.
pub(super) fn interrupted_checkout_metadata_present(repo: &Repo) -> Result<bool> {
    for name in [CHECKOUT_JOURNAL_NAME, CHECKOUT_ATTRIBUTES_SIDECAR_NAME] {
        let path = checkout_git_path(repo, name)?;
        match std::fs::symlink_metadata(&path) {
            Ok(_) => return Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "cannot inspect checkout recovery metadata {}",
                        path.display()
                    )
                });
            }
        }
    }
    Ok(false)
}

#[derive(Debug)]
pub(super) enum ExistingCheckoutBarrierFailure {
    Skippable(anyhow::Error),
    Recovery(anyhow::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ExistingCheckoutBarrierOutcome {
    Clear,
    RecoveryPending,
    RecoveryCompleted,
}

fn classify_existing_checkout_barrier_failure(
    repo: &Repo,
    recovery_metadata_seen: bool,
    error: anyhow::Error,
) -> ExistingCheckoutBarrierFailure {
    // A transaction can publish its journal while startup is opening or waiting on the existing
    // mutex. Preserve the original error, but make one last read-only observation so callers do
    // not downgrade a now-visible recovery obligation to an ordinary per-repository skip.
    let recovery_metadata_seen =
        recovery_metadata_seen || interrupted_checkout_metadata_present(repo).unwrap_or(false);
    if recovery_metadata_seen {
        ExistingCheckoutBarrierFailure::Recovery(error)
    } else {
        ExistingCheckoutBarrierFailure::Skippable(error)
    }
}

#[cfg(unix)]
fn same_file_identity(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;

    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file_identity(_left: &std::fs::Metadata, _right: &std::fs::Metadata) -> bool {
    // AgentGit does not currently ship Windows artifacts. The surrounding pre/post shape checks
    // retain the previous behavior on other targets; Unix release targets additionally bind the
    // opened descriptor to the exact inode observed at the path.
    true
}

/// Wait for an already-established checkout mutex without creating it on a clean repository.
///
/// `prepare_checkout_transaction` creates and locks this file before it can publish a sidecar or
/// journal. Opening only an existing file restores startup's barrier against a transaction that
/// was already in that pre-publication window, while keeping a read-only v1 repository with no
/// recovery state completely write-free. A transaction that starts after the absence check can
/// still race later command dispatch, just as it could after the old unconditional recovery call
/// released this lock; closing that wider window requires command-lifetime checkout locking.
///
/// The outcome distinguishes metadata that still needs a newly-created lock from recovery that
/// completed behind the existing lock. Failures carry the same classification so startup can
/// fail closed only after a recovery obligation is known.
pub(super) fn recover_behind_existing_checkout_lock(
    repo: &Repo,
) -> std::result::Result<ExistingCheckoutBarrierOutcome, ExistingCheckoutBarrierFailure> {
    let mut recovery_metadata_seen = interrupted_checkout_metadata_present(repo)
        .map_err(ExistingCheckoutBarrierFailure::Skippable)?;
    let path = checkout_git_path(repo, CHECKOUT_LOCK_NAME).map_err(|error| {
        classify_existing_checkout_barrier_failure(repo, recovery_metadata_seen, error)
    })?;
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(if recovery_metadata_seen {
                ExistingCheckoutBarrierOutcome::RecoveryPending
            } else {
                ExistingCheckoutBarrierOutcome::Clear
            });
        }
        Err(error) => {
            let error = anyhow::Error::new(error).context(format!(
                "cannot inspect checkout transaction lock {}",
                path.display()
            ));
            return Err(classify_existing_checkout_barrier_failure(
                repo,
                recovery_metadata_seen,
                error,
            ));
        }
    };
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(classify_existing_checkout_barrier_failure(
            repo,
            recovery_metadata_seen,
            anyhow::anyhow!(
                "refusing non-regular checkout transaction lock {}",
                path.display()
            ),
        ));
    }
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .open(&path)
        .with_context(|| {
            format!(
                "cannot open existing checkout transaction lock {}",
                path.display()
            )
        })
        .map_err(|error| {
            classify_existing_checkout_barrier_failure(repo, recovery_metadata_seen, error)
        })?;
    let opened_metadata = lock
        .metadata()
        .with_context(|| {
            format!(
                "cannot inspect opened checkout transaction lock {}",
                path.display()
            )
        })
        .map_err(|error| {
            classify_existing_checkout_barrier_failure(repo, recovery_metadata_seen, error)
        })?;
    let current_metadata = std::fs::symlink_metadata(&path)
        .with_context(|| {
            format!(
                "cannot re-inspect checkout transaction lock {}",
                path.display()
            )
        })
        .map_err(|error| {
            classify_existing_checkout_barrier_failure(repo, recovery_metadata_seen, error)
        })?;
    if !(current_metadata.file_type().is_file()
        && !current_metadata.file_type().is_symlink()
        && same_file_identity(&metadata, &opened_metadata)
        && same_file_identity(&opened_metadata, &current_metadata))
    {
        return Err(classify_existing_checkout_barrier_failure(
            repo,
            recovery_metadata_seen,
            anyhow::anyhow!(
                "checkout transaction lock changed identity while opening {}",
                path.display()
            ),
        ));
    }
    lock.lock_exclusive()
        .with_context(|| {
            format!(
                "cannot synchronize with checkout transactions in {}",
                path.display()
            )
        })
        .map_err(|error| {
            classify_existing_checkout_barrier_failure(repo, recovery_metadata_seen, error)
        })?;

    recovery_metadata_seen |= interrupted_checkout_metadata_present(repo).map_err(|error| {
        classify_existing_checkout_barrier_failure(repo, recovery_metadata_seen, error)
    })?;
    let recovered = recover_interrupted_checkout_locked(repo, &lock).map_err(|error| {
        classify_existing_checkout_barrier_failure(repo, recovery_metadata_seen, error)
    })?;
    Ok(if recovery_metadata_seen || recovered {
        ExistingCheckoutBarrierOutcome::RecoveryCompleted
    } else {
        ExistingCheckoutBarrierOutcome::Clear
    })
}

/// Write and fsync the recovery decision before the caller moves any ref. The returned guard owns
/// the per-checkout lock until the caller has either completed or rolled back the transaction.
pub(super) fn prepare_checkout_transaction(
    repo: &Repo,
    branch: &str,
    old: &str,
    new: &str,
    refresh_full_index: bool,
) -> Result<CheckoutTransaction> {
    let lock = lock_checkout_transactions(repo)?;
    recover_interrupted_checkout_locked(repo, &lock)?;

    let checked_out = checked_out_branch(repo)?;
    anyhow::ensure!(
        checked_out.as_deref() == Some(branch),
        "refusing checkout transaction for branch {branch:?}: checked out branch is {:?}",
        checked_out.as_deref().unwrap_or("detached HEAD")
    );
    let old = canonical_commit(repo, old)?;
    let new = canonical_commit(repo, new)?;
    let refname = format!("refs/heads/{branch}");
    let current = repo.git(&["rev-parse", "--verify", &format!("{refname}^{{commit}}")])?;
    anyhow::ensure!(
        current == old,
        "refusing checkout transaction for {refname}: expected {old}, found {current}"
    );

    let scope = if refresh_full_index {
        CheckoutScope::Full
    } else {
        CheckoutScope::Storage
    };
    let attributes_layers = prepare_attributes_upgrade(repo, &old, &new, scope)?;
    persist_checkout_transaction(
        repo,
        lock,
        CheckoutJournal {
            version: CHECKOUT_JOURNAL_VERSION,
            branch: branch.to_owned(),
            old: Some(old),
            new,
            scope,
            attributes_upgrade: None,
        },
        attributes_layers,
    )
}

/// Persist the recovery decision for the first commit of the checked-out branch.
///
/// Unlike an ordinary checkout transaction there is no old commit to restore. The real index and
/// worktree stay untouched until the expected-absent ref CAS succeeds; after that durable decision
/// recovery always converges them forward to `new`.
fn prepare_absent_checkout_transaction(
    repo: &Repo,
    branch: &str,
    new: &str,
) -> Result<CheckoutTransaction> {
    let lock = lock_checkout_transactions(repo)?;
    recover_interrupted_checkout_locked(repo, &lock)?;

    let checked_out = checked_out_branch(repo)?;
    anyhow::ensure!(
        checked_out.as_deref() == Some(branch),
        "refusing root checkout transaction for branch {branch:?}: checked out branch is {:?}",
        checked_out.as_deref().unwrap_or("detached HEAD")
    );
    let refname = format!("refs/heads/{branch}");
    anyhow::ensure!(
        optional_ref_commit(repo, &refname)?.is_none(),
        "refusing root checkout transaction for {refname}: the branch already exists"
    );
    let new = canonical_commit(repo, new)?;
    anyhow::ensure!(
        storage_layout_at(repo, &new)? == meta::LayoutVersion::V1,
        "root checkout endpoint is not a storage v1 commit"
    );
    ensure_v1_namespace_available_in_worktree(repo)?;
    meta::ensure_write_safe(repo.root())?;
    let attributes_layers = capture_attributes_layers(repo)?;
    validate_root_attributes(repo, &new, &attributes_layers)?;

    persist_checkout_transaction(
        repo,
        lock,
        CheckoutJournal {
            version: CHECKOUT_JOURNAL_VERSION,
            branch: branch.to_owned(),
            old: None,
            new,
            scope: CheckoutScope::Root,
            attributes_upgrade: None,
        },
        Some(attributes_layers),
    )
}

fn persist_checkout_transaction(
    repo: &Repo,
    lock: std::fs::File,
    mut journal: CheckoutJournal,
    attributes_layers: Option<AttributesLayers>,
) -> Result<CheckoutTransaction> {
    let sidecar_path = attributes_layers
        .as_ref()
        .map(|layers| write_attributes_sidecar(repo, layers))
        .transpose()?;
    if sidecar_path.is_some() {
        maybe_crash_checkout_at("after_attributes_sidecar");
    }
    let mut journal_publication = CheckoutJournalPublication::DurablyAbsent;
    let durable_metadata = (|| -> Result<(CheckoutJournal, std::path::PathBuf)> {
        let attributes_upgrade = attributes_layers
            .as_ref()
            .map(attributes_sidecar_descriptor)
            .transpose()?;
        if let (Some(expected), Some(descriptor)) = (
            attributes_layers.as_ref(),
            attributes_upgrade
                .as_ref()
                .and_then(|upgrade| upgrade.sidecar.as_ref()),
        ) {
            let durable = read_attributes_sidecar(repo, descriptor)?;
            anyhow::ensure!(
                durable == *expected,
                "attributes sidecar did not preserve the captured layers"
            );
        }
        journal.attributes_upgrade = attributes_upgrade;
        let path = checkout_git_path(repo, CHECKOUT_JOURNAL_NAME)?;
        if let Err(failure) = write_checkout_journal(&path, &journal) {
            journal_publication = failure.publication;
            return Err(failure.error);
        }
        Ok((journal, path))
    })();
    let (journal, path) = match durable_metadata {
        Ok(durable) => durable,
        Err(journal_error) => {
            if journal_publication == CheckoutJournalPublication::DurablyAbsent
                && let Some(sidecar_path) = sidecar_path.as_deref()
                && let Err(cleanup_error) = remove_checkout_sidecar(sidecar_path)
            {
                anyhow::bail!(
                    "preparing durable checkout metadata failed ({journal_error:#}) and removing its attributes sidecar also failed ({cleanup_error:#})"
                );
            }
            return Err(journal_error);
        }
    };
    Ok(CheckoutTransaction {
        journal,
        attributes_layers,
        path,
        sidecar_path,
        _lock: lock,
    })
}

pub(super) fn finish_checkout_transaction(transaction: CheckoutTransaction) -> Result<()> {
    remove_checkout_journal(&transaction.path)?;
    if let Some(path) = transaction.sidecar_path.as_deref() {
        remove_checkout_sidecar(path)?;
    }
    Ok(())
}

pub(super) fn refresh_prepared_checkout(
    repo: &Repo,
    transaction: &CheckoutTransaction,
) -> std::result::Result<(), CheckoutRefreshFailure> {
    let checked_out = checked_out_branch(repo).map_err(|error| CheckoutRefreshFailure {
        error,
        restored: true,
    })?;
    if checked_out.as_deref() != Some(transaction.journal.branch.as_str()) {
        return Err(CheckoutRefreshFailure {
            error: anyhow::anyhow!(
                "checked out branch changed during checkout transaction: expected {:?}, found {:?}",
                transaction.journal.branch,
                checked_out.as_deref().unwrap_or("detached HEAD")
            ),
            restored: true,
        });
    }
    let refname = format!("refs/heads/{}", transaction.journal.branch);
    let current = optional_ref_commit(repo, &refname).map_err(|error| CheckoutRefreshFailure {
        error,
        restored: true,
    })?;
    if current.as_deref() != Some(transaction.journal.new.as_str()) {
        return Err(CheckoutRefreshFailure {
            error: anyhow::anyhow!(
                "branch changed during checkout transaction: expected {}, found {}",
                transaction.journal.new,
                current.as_deref().unwrap_or("an absent ref")
            ),
            restored: true,
        });
    }
    if transaction.journal.scope == CheckoutScope::Root {
        let original_attributes =
            transaction
                .attributes_layers
                .as_ref()
                .ok_or_else(|| CheckoutRefreshFailure {
                    error: anyhow::anyhow!(
                        "root checkout transaction lost its attributes snapshot"
                    ),
                    restored: false,
                })?;
        return refresh_root_checkout(repo, &transaction.journal.new, original_attributes).map_err(
            |error| CheckoutRefreshFailure {
                error,
                // Root publication is forward-only. Once its ref exists, any partial refresh is
                // deliberately retained behind the journal for startup recovery.
                restored: false,
            },
        );
    }
    let old = transaction
        .journal
        .old
        .as_deref()
        .expect("non-root checkout transactions have an old endpoint");
    refresh_checkout_transactionally(
        repo,
        old,
        &transaction.journal.new,
        transaction.journal.scope,
        false,
        transaction
            .attributes_layers
            .as_ref()
            .map(|upgrade| (upgrade, true)),
    )
}

fn lock_checkout_transactions(repo: &Repo) -> Result<std::fs::File> {
    let path = checkout_git_path(repo, CHECKOUT_LOCK_NAME)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("cannot open checkout transaction lock {}", path.display()))?;
    lock.lock_exclusive()
        .with_context(|| format!("cannot lock checkout transactions in {}", path.display()))?;
    Ok(lock)
}

fn recover_interrupted_checkout_locked(repo: &Repo, _lock: &std::fs::File) -> Result<bool> {
    let path = checkout_git_path(repo, CHECKOUT_JOURNAL_NAME)?;
    let Some(journal) = read_checkout_journal(&path)? else {
        remove_orphaned_checkout_sidecar(repo)?;
        return Ok(false);
    };
    let attributes_upgrade = validate_checkout_journal(repo, &journal)?;

    let checked_out = checked_out_branch(repo)?;
    anyhow::ensure!(
        checked_out.as_deref() == Some(journal.branch.as_str()),
        "cannot recover checkout transaction for branch {:?}: checked out branch is {:?}",
        journal.branch,
        checked_out.as_deref().unwrap_or("detached HEAD")
    );
    let refname = format!("refs/heads/{}", journal.branch);
    let current = optional_ref_commit(repo, &refname)?;
    if journal.scope == CheckoutScope::Root {
        match current.as_deref() {
            None => {
                // The process stopped before its expected-absent CAS. No checkout byte could have
                // changed, so discarding the durable intent is the complete recovery action.
            }
            Some(commit) if commit == journal.new => {
                let original_attributes = attributes_upgrade
                    .as_ref()
                    .context("root checkout journal has no durable attributes snapshot")?;
                refresh_root_checkout(repo, &journal.new, original_attributes).with_context(
                    || format!("cannot recover root checkout transaction for {refname}"),
                )?;
            }
            Some(commit) => anyhow::bail!(
                "cannot recover root checkout transaction for {refname}: ref is {commit}, expected {} or an absent ref",
                journal.new
            ),
        }
        remove_recovered_checkout_metadata(repo, &path, &journal)?;
        return Ok(true);
    }

    let old = journal
        .old
        .as_deref()
        .expect("validated non-root checkout journal has an old endpoint");
    let current = current.context(format!(
        "cannot recover checkout transaction for {refname}: ref is absent"
    ))?;
    let (source, target, target_is_v1) = if current == journal.new {
        (old, journal.new.as_str(), true)
    } else if current == old {
        (journal.new.as_str(), old, false)
    } else {
        anyhow::bail!(
            "cannot recover checkout transaction for {refname}: ref is {current}, expected {} or {}",
            old,
            journal.new
        );
    };

    refresh_checkout_transactionally(
        repo,
        source,
        target,
        journal.scope,
        true,
        attributes_upgrade
            .as_ref()
            .map(|upgrade| (upgrade, target_is_v1)),
    )
    .map_err(|failure| failure.error)
    .with_context(|| format!("cannot recover checkout transaction for {refname} at {current}"))?;
    remove_recovered_checkout_metadata(repo, &path, &journal)?;
    Ok(true)
}

fn remove_recovered_checkout_metadata(
    repo: &Repo,
    journal_path: &std::path::Path,
    journal: &CheckoutJournal,
) -> Result<()> {
    let sidecar_path = journal_uses_attributes_sidecar(journal)
        .then(|| checkout_git_path(repo, CHECKOUT_ATTRIBUTES_SIDECAR_NAME))
        .transpose()?;
    remove_checkout_journal(journal_path)?;
    if let Some(path) = sidecar_path.as_deref() {
        remove_checkout_sidecar(path)?;
    }
    Ok(())
}

fn validate_checkout_journal(
    repo: &Repo,
    journal: &CheckoutJournal,
) -> Result<Option<AttributesLayers>> {
    anyhow::ensure!(
        matches!(
            journal.version,
            CHECKOUT_JOURNAL_LEGACY_VERSION
                | CHECKOUT_JOURNAL_OID_VERSION
                | CHECKOUT_JOURNAL_INLINE_VERSION
                | CHECKOUT_JOURNAL_VERSION
        ),
        "unsupported checkout transaction journal version {}",
        journal.version
    );
    anyhow::ensure!(
        !journal.branch.is_empty()
            && !journal.branch.contains(['\0', '\n', '\r'])
            && !journal.branch.starts_with("refs/"),
        "invalid checkout transaction branch {:?}",
        journal.branch
    );
    let checked = std::process::Command::new("git")
        .arg("--no-replace-objects")
        .arg("-C")
        .arg(repo.root())
        .args(["check-ref-format", "--branch", &journal.branch])
        .output()?;
    anyhow::ensure!(
        checked.status.success(),
        "invalid checkout transaction branch {:?}",
        journal.branch
    );
    if journal.scope == CheckoutScope::Root {
        anyhow::ensure!(
            journal.version == CHECKOUT_JOURNAL_VERSION && journal.old.is_none(),
            "root checkout transaction requires a current journal with an absent old endpoint"
        );
    } else {
        let old = journal
            .old
            .as_deref()
            .context("checkout transaction has no old endpoint")?;
        anyhow::ensure!(
            canonical_commit(repo, old)? == old,
            "checkout transaction old endpoint is not a canonical commit id"
        );
    }
    anyhow::ensure!(
        canonical_commit(repo, &journal.new)? == journal.new,
        "checkout transaction new endpoint is not a canonical commit id"
    );
    let attributes_upgrade = journal
        .attributes_upgrade
        .as_ref()
        .map(|upgrade| -> Result<AttributesLayers> {
            anyhow::ensure!(
                journal.version != CHECKOUT_JOURNAL_LEGACY_VERSION
                    && matches!(journal.scope, CheckoutScope::Storage | CheckoutScope::Root),
                "checkout attributes snapshot requires a storage/root journal newer than v{CHECKOUT_JOURNAL_LEGACY_VERSION}"
            );
            let upgrade = materialize_attributes_upgrade(repo, journal.version, upgrade)?;
            if journal.scope == CheckoutScope::Root {
                validate_root_attributes(repo, &journal.new, &upgrade)?;
            } else {
                validate_attributes_upgrade(
                    repo,
                    journal
                        .old
                        .as_deref()
                        .expect("validated storage journal has an old endpoint"),
                    &journal.new,
                    &upgrade,
                )?;
            }
            Ok(upgrade)
        })
        .transpose()?;
    anyhow::ensure!(
        journal.scope != CheckoutScope::Root || attributes_upgrade.is_some(),
        "root checkout journal has no durable attributes snapshot"
    );
    Ok(attributes_upgrade)
}

fn canonical_commit(repo: &Repo, commit: &str) -> Result<String> {
    anyhow::ensure!(
        !commit.is_empty()
            && commit.bytes().all(|byte| byte.is_ascii_hexdigit())
            && !commit.contains(['\n', '\r']),
        "invalid checkout transaction commit id"
    );
    repo.git(&["rev-parse", "--verify", &format!("{commit}^{{commit}}")])
}

fn optional_ref_commit(repo: &Repo, refname: &str) -> Result<Option<String>> {
    let output = std::process::Command::new("git")
        .arg("--no-replace-objects")
        .arg("-C")
        .arg(repo.root())
        .args(["for-each-ref", "--format=%(refname)", refname])
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "git for-each-ref {refname} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    anyhow::ensure!(
        output.stderr.is_empty(),
        "git for-each-ref {refname} reported corruption: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let refs = String::from_utf8(output.stdout)
        .context("git for-each-ref returned a non-UTF-8 ref name")?;
    let mut exact = refs.lines().filter(|name| *name == refname);
    if exact.next().is_none() {
        return Ok(None);
    }
    anyhow::ensure!(
        exact.next().is_none(),
        "git for-each-ref returned {refname} more than once"
    );
    let commit = repo.git(&["rev-parse", "--verify", &format!("{refname}^{{commit}}")])?;
    canonical_commit(repo, &commit).map(Some)
}

fn prepare_attributes_upgrade(
    repo: &Repo,
    old: &str,
    new: &str,
    scope: CheckoutScope,
) -> Result<Option<AttributesLayers>> {
    if scope != CheckoutScope::Storage
        || storage_layout_at(repo, old)? != meta::LayoutVersion::V0
        || storage_layout_at(repo, new)? != meta::LayoutVersion::V1
    {
        return Ok(None);
    }
    validate_attributes_upgrade_commits(repo, old, new)?;
    let upgrade = capture_attributes_layers(repo)?;
    // Precompute both normalized layers before publishing recovery metadata. This rejects an
    // oversized or malformed layer before the ref can move; the fsynced sidecar then carries the
    // exact original bytes needed by either recovery decision.
    let _ = normalize_attributes_layers(&upgrade)?;
    Ok(Some(upgrade))
}

fn validate_attributes_upgrade(
    repo: &Repo,
    old: &str,
    new: &str,
    upgrade: &AttributesLayers,
) -> Result<()> {
    anyhow::ensure!(
        storage_layout_at(repo, old)? == meta::LayoutVersion::V0
            && storage_layout_at(repo, new)? == meta::LayoutVersion::V1,
        "checkout attributes journal does not describe a v0 to v1 upgrade"
    );
    validate_attributes_upgrade_commits(repo, old, new)?;
    validate_attributes_layers(upgrade)?;
    let _ = normalize_attributes_layers(upgrade)?;
    Ok(())
}

fn validate_attributes_upgrade_commits(repo: &Repo, old: &str, new: &str) -> Result<()> {
    let old_attributes = regular_blob_text_at(repo, old, meta::ATTRS_FILE)?;
    let expected = storage::attributes_text_strict(old_attributes.as_deref())?;
    let new_attributes = regular_blob_text_at(repo, new, meta::ATTRS_FILE)?
        .context("v1 checkout endpoint has no .gitattributes")?;
    anyhow::ensure!(
        new_attributes == expected,
        "v1 checkout endpoint does not normalize the committed v0 .gitattributes layer"
    );
    Ok(())
}

fn capture_attributes_layers(repo: &Repo) -> Result<AttributesLayers> {
    let path = vec![meta::ATTRS_FILE.to_owned()];
    let index = match checkout_index_files(repo, &path)?.remove(meta::ATTRS_FILE) {
        Some(entry) => {
            anyhow::ensure!(
                entry.mode == "100644",
                "refusing non-regular staged {} mode {} during storage upgrade",
                meta::ATTRS_FILE,
                entry.mode
            );
            let text =
                attributes_blob_text_capped(repo, &entry.oid, MAX_CHECKOUT_ATTRIBUTES_LAYER_BYTES)?;
            storage::attributes_text_strict(Some(&text))?;
            Some(text)
        }
        None => None,
    };
    let worktree = match checkout_file_with_limit(
        repo.root(),
        meta::ATTRS_FILE,
        MAX_CHECKOUT_ATTRIBUTES_LAYER_BYTES,
    )? {
        CheckoutFile::Absent => None,
        CheckoutFile::Regular { bytes, executable } => {
            anyhow::ensure!(
                !executable,
                "refusing executable worktree {} during storage upgrade",
                meta::ATTRS_FILE
            );
            let text = std::str::from_utf8(&bytes)
                .with_context(|| format!("worktree {} is not UTF-8", meta::ATTRS_FILE))?;
            storage::attributes_text_strict(Some(text))?;
            Some(text.to_owned())
        }
        CheckoutFile::Symlink(_) | CheckoutFile::Directory => anyhow::bail!(
            "refusing non-regular worktree {} during storage upgrade",
            meta::ATTRS_FILE
        ),
    };
    let upgrade = AttributesLayers { index, worktree };
    validate_attributes_layers(&upgrade)?;
    Ok(upgrade)
}

fn materialize_attributes_upgrade(
    repo: &Repo,
    journal_version: u32,
    upgrade: &CheckoutAttributesUpgrade,
) -> Result<AttributesLayers> {
    if journal_version == CHECKOUT_JOURNAL_VERSION {
        anyhow::ensure!(
            upgrade.index.is_none() && upgrade.worktree.is_none(),
            "v{CHECKOUT_JOURNAL_VERSION} attributes journal must not inline user bytes"
        );
        let sidecar = upgrade
            .sidecar
            .as_ref()
            .context("v4 attributes journal has no sidecar descriptor")?;
        return read_attributes_sidecar(repo, sidecar);
    }
    anyhow::ensure!(
        upgrade.sidecar.is_none(),
        "legacy attributes journal cannot reference a v4 sidecar"
    );
    if journal_version == CHECKOUT_JOURNAL_INLINE_VERSION {
        let materialized = AttributesLayers {
            index: upgrade.index.clone(),
            worktree: upgrade.worktree.clone(),
        };
        validate_attributes_layers(&materialized)?;
        return Ok(materialized);
    }
    anyhow::ensure!(
        journal_version == CHECKOUT_JOURNAL_OID_VERSION,
        "attributes upgrade is unsupported in checkout journal v{journal_version}"
    );
    let materialize = |oid: Option<&str>| -> Result<Option<String>> {
        let Some(oid) = oid else {
            return Ok(None);
        };
        let text = attributes_blob_text_capped(repo, oid, MAX_CHECKOUT_ATTRIBUTES_LAYER_BYTES)?;
        Ok(Some(text))
    };
    let materialized = AttributesLayers {
        index: materialize(upgrade.index.as_deref())?,
        worktree: materialize(upgrade.worktree.as_deref())?,
    };
    validate_attributes_layers(&materialized)?;
    Ok(materialized)
}

fn attributes_blob_text_capped(repo: &Repo, oid: &str, byte_limit: usize) -> Result<String> {
    anyhow::ensure!(
        !oid.is_empty() && oid.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid attributes layer object id"
    );
    let canonical = repo.git(&["rev-parse", "--verify", &format!("{oid}^{{blob}}")])?;
    anyhow::ensure!(
        canonical == oid,
        "attributes layer object id is not canonical"
    );
    let size: usize = repo
        .git(&["cat-file", "-s", oid])?
        .parse()
        .context("attributes layer object has an invalid size")?;
    anyhow::ensure!(
        size <= byte_limit,
        "attributes layer exceeds its {byte_limit}-byte checkout snapshot safety limit"
    );
    let bytes = git_bytes_checked(repo, &["cat-file", "blob", oid])?;
    anyhow::ensure!(
        bytes.len() == size,
        "attributes layer object size changed while reading"
    );
    String::from_utf8(bytes).with_context(|| format!("attributes layer object {oid} is not UTF-8"))
}

fn validate_attributes_layers(layers: &AttributesLayers) -> Result<()> {
    for text in [layers.index.as_deref(), layers.worktree.as_deref()]
        .into_iter()
        .flatten()
    {
        anyhow::ensure!(
            text.len() <= MAX_CHECKOUT_ATTRIBUTES_LAYER_BYTES,
            "attributes layer exceeds its {MAX_CHECKOUT_ATTRIBUTES_LAYER_BYTES}-byte checkout snapshot safety limit"
        );
        storage::attributes_text_strict(Some(text))?;
    }
    Ok(())
}

fn normalize_attributes_layers(original: &AttributesLayers) -> Result<AttributesLayers> {
    let normalized = AttributesLayers {
        index: Some(storage::attributes_text_strict(original.index.as_deref())?),
        worktree: Some(storage::attributes_text_strict(
            original.worktree.as_deref(),
        )?),
    };
    validate_attributes_layers(&normalized)?;
    Ok(normalized)
}

fn root_attributes_target(repo: &Repo, new: &str) -> Result<String> {
    let text = regular_blob_text_at(repo, new, meta::ATTRS_FILE)?
        .context("storage v1 root checkout endpoint has no .gitattributes")?;
    anyhow::ensure!(
        storage::attributes_text_strict(Some(&text))? == text,
        "storage v1 root checkout endpoint has non-canonical .gitattributes"
    );
    Ok(text)
}

fn validate_root_attributes(repo: &Repo, new: &str, original: &AttributesLayers) -> Result<()> {
    anyhow::ensure!(
        storage_layout_at(repo, new)? == meta::LayoutVersion::V1,
        "root checkout endpoint is not a storage v1 commit"
    );
    validate_attributes_layers(original)?;
    let target = root_attributes_target(repo, new)?;
    anyhow::ensure!(
        target == storage::attributes_text_strict(original.worktree.as_deref())?,
        "root checkout endpoint does not preserve the journalled worktree .gitattributes layer"
    );
    Ok(())
}

fn refresh_root_checkout(
    repo: &Repo,
    new: &str,
    original_attributes: &AttributesLayers,
) -> Result<()> {
    validate_root_attributes(repo, new, original_attributes)?;
    ensure_v1_namespace_absent_or_matches(repo, new)?;
    meta::ensure_write_safe(repo.root())?;

    let attributes_text = root_attributes_target(repo, new)?;
    let target_attributes = AttributesLayers {
        index: Some(attributes_text.clone()),
        worktree: Some(attributes_text),
    };
    let current_attributes = capture_attributes_layers(repo)?;
    anyhow::ensure!(
        current_attributes.index == original_attributes.index
            || current_attributes.index == target_attributes.index,
        "refusing to overwrite staged {} during root recovery: it matches neither the journalled layer nor the published root",
        meta::ATTRS_FILE
    );
    anyhow::ensure!(
        current_attributes.worktree == original_attributes.worktree
            || current_attributes.worktree == target_attributes.worktree,
        "refusing to overwrite worktree {} during root recovery: it matches neither the journalled layer nor the published root",
        meta::ATTRS_FILE
    );

    let log = storage::materialize_at(repo.root(), new, meta::LOG_FILE)?;
    let view = storage::materialize_at(repo.root(), new, meta::VIEW_FILE)?;
    let snapshot = meta::read_at_ref_result(repo, new)?
        .context("storage v1 root checkout endpoint has no session metadata")?;

    // These writers publish each file atomically and are idempotent. A hard stop can therefore
    // leave any prefix of the owned snapshot installed; startup accepts only absent/exact v1 root
    // paths and reruns the same forward materialization.
    storage::write_snapshot(repo.root(), &log, &view)?;
    maybe_crash_checkout_at("during_apply");
    meta::write(repo.root(), &snapshot)?;

    // Align the complete real index without moving HEAD. `git reset <new>` would silently move a
    // concurrently advanced branch; read-tree changes only the lock-protected index file.
    checkout_git(repo, &["read-tree", "--reset", new])?;

    let current_head = repo.git(&["rev-parse", "--verify", "HEAD^{commit}"])?;
    anyhow::ensure!(
        current_head == new,
        "checked-out branch changed during root checkout refresh: expected {new}, found {current_head}"
    );
    let expected_tree = repo.git(&["rev-parse", "--verify", &format!("{new}^{{tree}}")])?;
    let index_tree = checkout_git(repo, &["write-tree"])?;
    anyhow::ensure!(
        index_tree == expected_tree,
        "root checkout index did not converge to the published tree"
    );
    anyhow::ensure!(
        storage::materialize_worktree(repo.root(), meta::LOG_FILE)? == log,
        "root checkout LOG did not converge to the published snapshot"
    );
    anyhow::ensure!(
        storage::materialize_worktree(repo.root(), meta::VIEW_FILE)? == view,
        "root checkout VIEW did not converge to the published snapshot"
    );
    let expected_meta = meta::to_text(&snapshot)?;
    let worktree_meta = std::fs::read_to_string(repo.root().join(meta::FILE))
        .with_context(|| format!("cannot read worktree {}", meta::FILE))?;
    anyhow::ensure!(
        worktree_meta == expected_meta,
        "root checkout metadata did not converge to the published snapshot"
    );
    anyhow::ensure!(
        capture_attributes_layers(repo)? == target_attributes,
        "root checkout {} layers did not converge to the published snapshot",
        meta::ATTRS_FILE
    );
    ensure_v1_namespace_absent_or_matches(repo, new)?;
    Ok(())
}

fn attributes_sidecar_descriptor(layers: &AttributesLayers) -> Result<CheckoutAttributesUpgrade> {
    validate_attributes_layers(layers)?;
    let index_bytes = layers
        .index
        .as_ref()
        .map(|text| u64::try_from(text.len()).context("staged attributes layer is too large"))
        .transpose()?;
    let worktree_bytes = layers
        .worktree
        .as_ref()
        .map(|text| u64::try_from(text.len()).context("worktree attributes layer is too large"))
        .transpose()?;
    let bytes = index_bytes
        .unwrap_or(0)
        .checked_add(worktree_bytes.unwrap_or(0))
        .context("attributes sidecar byte count overflowed")?;
    anyhow::ensure!(
        bytes <= MAX_CHECKOUT_ATTRIBUTES_SIDECAR_BYTES,
        "attributes sidecar exceeds its {MAX_CHECKOUT_ATTRIBUTES_SIDECAR_BYTES}-byte safety limit"
    );
    let mut digest = Sha256::new();
    if let Some(text) = &layers.index {
        digest.update(text.as_bytes());
    }
    if let Some(text) = &layers.worktree {
        digest.update(text.as_bytes());
    }
    Ok(CheckoutAttributesUpgrade {
        index: None,
        worktree: None,
        sidecar: Some(CheckoutAttributesSidecar {
            sha256: hex::encode(digest.finalize()),
            bytes,
            index_bytes,
            worktree_bytes,
        }),
    })
}

fn write_attributes_sidecar(repo: &Repo, layers: &AttributesLayers) -> Result<std::path::PathBuf> {
    validate_attributes_layers(layers)?;
    let path = checkout_git_path(repo, CHECKOUT_ATTRIBUTES_SIDECAR_NAME)?;
    let parent = path
        .parent()
        .context("checkout attributes sidecar has no parent directory")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).with_context(|| {
        format!(
            "cannot create temporary checkout attributes sidecar in {}",
            parent.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        temporary
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    if let Some(text) = &layers.index {
        temporary.write_all(text.as_bytes())?;
    }
    if let Some(text) = &layers.worktree {
        temporary.write_all(text.as_bytes())?;
    }
    temporary
        .as_file()
        .sync_all()
        .with_context(|| format!("cannot sync checkout attributes sidecar {}", path.display()))?;
    temporary.persist_noclobber(&path).map_err(|error| {
        anyhow::anyhow!(error.error).context(format!(
            "cannot publish checkout attributes sidecar {}; a prior transaction may need recovery",
            path.display()
        ))
    })?;
    if let Err(sync_error) = sync_directory(parent) {
        return match remove_checkout_sidecar(&path) {
            Ok(_) => Err(sync_error.context(format!(
                "checkout attributes sidecar {} was removed because syncing its directory failed",
                path.display()
            ))),
            Err(cleanup_error) => anyhow::bail!(
                "syncing checkout attributes sidecar directory failed ({sync_error:#}) and removing the published sidecar also failed ({cleanup_error:#})"
            ),
        };
    }
    Ok(path)
}

fn read_attributes_sidecar(
    repo: &Repo,
    descriptor: &CheckoutAttributesSidecar,
) -> Result<AttributesLayers> {
    anyhow::ensure!(
        descriptor.sha256.len() == 64
            && descriptor
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "checkout attributes sidecar has an invalid SHA-256 digest"
    );
    for length in [descriptor.index_bytes, descriptor.worktree_bytes]
        .into_iter()
        .flatten()
    {
        anyhow::ensure!(
            length <= MAX_CHECKOUT_ATTRIBUTES_LAYER_BYTES as u64,
            "checkout attributes sidecar layer exceeds its {MAX_CHECKOUT_ATTRIBUTES_LAYER_BYTES}-byte safety limit"
        );
    }
    let expected_bytes = descriptor
        .index_bytes
        .unwrap_or(0)
        .checked_add(descriptor.worktree_bytes.unwrap_or(0))
        .context("checkout attributes sidecar byte count overflowed")?;
    anyhow::ensure!(
        expected_bytes == descriptor.bytes
            && descriptor.bytes <= MAX_CHECKOUT_ATTRIBUTES_SIDECAR_BYTES,
        "checkout attributes sidecar has invalid bounded lengths"
    );

    let path = checkout_git_path(repo, CHECKOUT_ATTRIBUTES_SIDECAR_NAME)?;
    let metadata = std::fs::symlink_metadata(&path).with_context(|| {
        format!(
            "cannot inspect checkout attributes sidecar {}",
            path.display()
        )
    })?;
    anyhow::ensure!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "refusing non-regular checkout attributes sidecar {}",
        path.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        anyhow::ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "checkout attributes sidecar {} is accessible outside its owner",
            path.display()
        );
    }
    anyhow::ensure!(
        metadata.len() == descriptor.bytes,
        "checkout attributes sidecar {} has {} bytes, expected {}",
        path.display(),
        metadata.len(),
        descriptor.bytes
    );
    let capacity = usize::try_from(descriptor.bytes)
        .context("checkout attributes sidecar is too large for this platform")?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(capacity)
        .context("cannot reserve memory for checkout attributes sidecar")?;
    std::fs::File::open(&path)
        .with_context(|| format!("cannot open checkout attributes sidecar {}", path.display()))?
        .take(descriptor.bytes + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("cannot read checkout attributes sidecar {}", path.display()))?;
    anyhow::ensure!(
        bytes.len() as u64 == descriptor.bytes,
        "checkout attributes sidecar {} changed while reading",
        path.display()
    );
    anyhow::ensure!(
        hex::encode(Sha256::digest(&bytes)) == descriptor.sha256,
        "checkout attributes sidecar {} failed its SHA-256 integrity check",
        path.display()
    );

    let mut text = String::from_utf8(bytes).with_context(|| {
        format!(
            "checkout attributes sidecar {} is not UTF-8",
            path.display()
        )
    })?;
    let index_len = descriptor.index_bytes.unwrap_or(0) as usize;
    anyhow::ensure!(
        text.is_char_boundary(index_len),
        "checkout attributes sidecar splits a UTF-8 code point between layers"
    );
    let worktree = if descriptor.worktree_bytes.is_some() {
        Some(text.split_off(index_len))
    } else {
        anyhow::ensure!(
            text.len() == index_len,
            "checkout attributes sidecar has unassigned bytes"
        );
        None
    };
    let index = if descriptor.index_bytes.is_some() {
        Some(text)
    } else {
        anyhow::ensure!(
            text.is_empty(),
            "checkout attributes sidecar has unassigned bytes"
        );
        None
    };
    let layers = AttributesLayers { index, worktree };
    validate_attributes_layers(&layers)?;
    Ok(layers)
}

fn journal_uses_attributes_sidecar(journal: &CheckoutJournal) -> bool {
    journal
        .attributes_upgrade
        .as_ref()
        .and_then(|upgrade| upgrade.sidecar.as_ref())
        .is_some()
}

fn remove_orphaned_checkout_sidecar(repo: &Repo) -> Result<()> {
    let path = checkout_git_path(repo, CHECKOUT_ATTRIBUTES_SIDECAR_NAME)?;
    remove_checkout_sidecar(&path)?;
    Ok(())
}

fn remove_checkout_sidecar(path: &std::path::Path) -> Result<bool> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "cannot inspect checkout attributes sidecar {}",
                    path.display()
                )
            });
        }
    };
    anyhow::ensure!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "refusing non-regular checkout attributes sidecar {}",
        path.display()
    );
    let parent = path
        .parent()
        .context("checkout attributes sidecar has no parent directory")?;
    std::fs::remove_file(path).with_context(|| {
        format!(
            "cannot remove checkout attributes sidecar {}",
            path.display()
        )
    })?;
    sync_directory(parent)?;
    Ok(true)
}

fn checkout_git_path(repo: &Repo, name: &str) -> Result<std::path::PathBuf> {
    let value = repo.git(&["rev-parse", "--git-path", name])?;
    anyhow::ensure!(!value.is_empty(), "git returned an empty path for {name}");
    let path = std::path::PathBuf::from(value);
    Ok(if path.is_absolute() {
        path
    } else {
        repo.root().join(path)
    })
}

fn read_checkout_journal(path: &std::path::Path) -> Result<Option<CheckoutJournal>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("cannot inspect checkout journal {}", path.display()));
        }
    };
    anyhow::ensure!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "refusing non-regular checkout journal {}",
        path.display()
    );
    anyhow::ensure!(
        metadata.len() <= MAX_CHECKOUT_JOURNAL_BYTES,
        "checkout journal {} exceeds {} bytes",
        path.display(),
        MAX_CHECKOUT_JOURNAL_BYTES
    );
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    std::fs::File::open(path)
        .with_context(|| format!("cannot open checkout journal {}", path.display()))?
        .take(MAX_CHECKOUT_JOURNAL_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("cannot read checkout journal {}", path.display()))?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_CHECKOUT_JOURNAL_BYTES,
        "checkout journal {} grew beyond {} bytes while reading",
        path.display(),
        MAX_CHECKOUT_JOURNAL_BYTES
    );
    serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid checkout journal {}", path.display()))
        .map(Some)
}

fn write_checkout_journal(
    path: &std::path::Path,
    journal: &CheckoutJournal,
) -> std::result::Result<(), CheckoutJournalWriteFailure> {
    let before_publication = |error| CheckoutJournalWriteFailure {
        error,
        publication: CheckoutJournalPublication::DurablyAbsent,
    };
    let after_publication = |error| CheckoutJournalWriteFailure {
        error,
        publication: CheckoutJournalPublication::PublishedOrCleanupUncertain,
    };
    let parent = path
        .parent()
        .context("checkout transaction journal has no parent directory")
        .map_err(before_publication)?;
    let mut bytes =
        serde_json::to_vec(journal).map_err(|error| before_publication(error.into()))?;
    bytes.push(b'\n');
    if bytes.len() as u64 > MAX_CHECKOUT_JOURNAL_BYTES {
        return Err(before_publication(anyhow::anyhow!(
            "checkout journal needs {} bytes, exceeding the {}-byte safety limit",
            bytes.len(),
            MAX_CHECKOUT_JOURNAL_BYTES
        )));
    }
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| {
            format!(
                "cannot create temporary checkout journal in {}",
                parent.display()
            )
        })
        .map_err(before_publication)?;
    temporary
        .write_all(&bytes)
        .with_context(|| format!("cannot write checkout journal {}", path.display()))
        .map_err(before_publication)?;
    temporary
        .as_file()
        .sync_all()
        .with_context(|| format!("cannot sync checkout journal {}", path.display()))
        .map_err(before_publication)?;
    temporary.persist_noclobber(path).map_err(|error| {
        after_publication(anyhow::anyhow!(error.error).context(format!(
            "cannot publish checkout journal {}; a prior transaction may need recovery",
            path.display()
        )))
    })?;
    if let Err(sync_error) = sync_checkout_journal_directory(parent) {
        return match remove_failed_checkout_journal(path, &bytes) {
            Ok(()) => Err(before_publication(sync_error.context(format!(
                "checkout journal {} was removed because syncing its directory failed",
                path.display()
            )))),
            Err(cleanup_error) => Err(after_publication(anyhow::anyhow!(
                "syncing checkout journal directory failed ({sync_error:#}) and safely removing the published journal also failed ({cleanup_error:#})"
            ))),
        };
    }
    Ok(())
}

fn sync_checkout_journal_directory(path: &std::path::Path) -> Result<()> {
    #[cfg(test)]
    if FAIL_CHECKOUT_JOURNAL_DIRECTORY_SYNC.with(|fail| fail.replace(false)) {
        anyhow::bail!("injected checkout journal directory sync failure");
    }
    sync_directory(path)
}

#[cfg(test)]
thread_local! {
    static FAIL_CHECKOUT_JOURNAL_DIRECTORY_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn fail_next_checkout_journal_directory_sync() {
    FAIL_CHECKOUT_JOURNAL_DIRECTORY_SYNC.with(|fail| fail.set(true));
}

fn remove_failed_checkout_journal(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .context("checkout transaction journal has no parent directory")?;
    unlink_checkout_journal(path)?;
    if let Err(sync_error) = sync_removed_checkout_journal_directory(parent) {
        // Re-establish the complete recovery pair in the live namespace. If republishing also
        // fails, the caller still retains the sidecar because the failed unlink fsync means the
        // original journal may reappear after a crash.
        let error = match restore_checkout_journal(path, bytes) {
            Ok(()) => anyhow::anyhow!(
                "syncing the journal removal failed ({sync_error:#}); the journal was restored for startup recovery"
            ),
            Err(restore_error) => anyhow::anyhow!(
                "syncing the journal removal failed ({sync_error:#}) and restoring the journal also failed ({restore_error:#})"
            ),
        };
        return Err(error);
    }
    Ok(())
}

fn restore_checkout_journal(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .context("checkout transaction journal has no parent directory")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).with_context(|| {
        format!(
            "cannot create temporary checkout journal in {}",
            parent.display()
        )
    })?;
    temporary
        .write_all(bytes)
        .with_context(|| format!("cannot restore checkout journal {}", path.display()))?;
    temporary
        .as_file()
        .sync_all()
        .with_context(|| format!("cannot sync restored checkout journal {}", path.display()))?;
    temporary.persist_noclobber(path).map_err(|error| {
        anyhow::anyhow!(error.error).context(format!(
            "cannot republish checkout journal {} for recovery",
            path.display()
        ))
    })?;
    sync_directory(parent).with_context(|| {
        format!(
            "cannot sync restored checkout journal directory {}",
            parent.display()
        )
    })
}

fn unlink_checkout_journal(path: &std::path::Path) -> Result<()> {
    #[cfg(test)]
    if FAIL_CHECKOUT_JOURNAL_UNLINK.with(|fail| fail.replace(false)) {
        anyhow::bail!("injected checkout journal unlink failure");
    }
    std::fs::remove_file(path)
        .with_context(|| format!("cannot remove checkout journal {}", path.display()))
}

fn sync_removed_checkout_journal_directory(path: &std::path::Path) -> Result<()> {
    #[cfg(test)]
    if FAIL_CHECKOUT_JOURNAL_REMOVAL_DIRECTORY_SYNC.with(|fail| fail.replace(false)) {
        anyhow::bail!("injected checkout journal removal directory sync failure");
    }
    sync_directory(path)
}

#[cfg(test)]
thread_local! {
    static FAIL_CHECKOUT_JOURNAL_UNLINK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FAIL_CHECKOUT_JOURNAL_REMOVAL_DIRECTORY_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn fail_next_checkout_journal_unlink() {
    FAIL_CHECKOUT_JOURNAL_UNLINK.with(|fail| fail.set(true));
}

#[cfg(test)]
fn fail_next_checkout_journal_removal_directory_sync() {
    FAIL_CHECKOUT_JOURNAL_REMOVAL_DIRECTORY_SYNC.with(|fail| fail.set(true));
}

fn remove_checkout_journal(path: &std::path::Path) -> Result<()> {
    let parent = path
        .parent()
        .context("checkout transaction journal has no parent directory")?;
    unlink_checkout_journal(path)?;
    sync_directory(parent)
}

fn sync_directory(path: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(path)
            .with_context(|| format!("cannot open directory {} for sync", path.display()))?
            .sync_all()
            .with_context(|| format!("cannot sync directory {}", path.display()))?;
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum CheckoutFile {
    Absent,
    Regular { bytes: Vec<u8>, executable: bool },
    Symlink(Vec<u8>),
    Directory,
}

#[derive(Clone)]
struct TreeFile {
    mode: String,
    bytes: Vec<u8>,
}

struct CheckoutPreparation {
    paths: Vec<String>,
    new_files: std::collections::BTreeMap<String, TreeEntry>,
    snapshot: Vec<(String, CheckoutFile)>,
    absent_parents: Vec<String>,
    attributes: Option<PreparedAttributesUpgrade>,
}

struct PreparedAttributesUpgrade {
    before: AttributesLayers,
    target: AttributesLayers,
}

fn refresh_checkout_transactionally(
    repo: &Repo,
    old: &str,
    new: &str,
    scope: CheckoutScope,
    recovering: bool,
    attributes: Option<(&AttributesLayers, bool)>,
) -> std::result::Result<(), CheckoutRefreshFailure> {
    let prepared = prepare_checkout_refresh(repo, old, new, scope, recovering, attributes)
        .map_err(|error| {
            CheckoutRefreshFailure {
                error,
                // Preparation is read-only apart from hashing already-present bytes into the
                // object database. No index/worktree layer needs restoration when it fails.
                restored: true,
            }
        })?;
    let CheckoutPreparation {
        paths,
        new_files,
        snapshot,
        absent_parents,
        attributes,
    } = prepared;

    let apply = (|| -> Result<()> {
        apply_checkout(repo, new, &paths, &new_files, scope)?;
        if let Some(attributes) = &attributes {
            apply_attributes_layers(repo, &attributes.target)?;
        }
        #[cfg(test)]
        if FAIL_CHECKOUT_AFTER_APPLY.with(|fail| fail.replace(false)) {
            anyhow::bail!("injected checkout postflight failure");
        }
        Ok(())
    })();
    if let Err(error) = apply {
        let restore = (|| -> Result<()> {
            restore_checkout(repo, old, &paths, &snapshot, &absent_parents, scope)?;
            if let Some(attributes) = &attributes {
                apply_attributes_layers(repo, &attributes.before)?;
            }
            Ok(())
        })();
        return match restore {
            Ok(()) => Err(CheckoutRefreshFailure {
                error: error.context("checkout was restored after refresh failed"),
                restored: true,
            }),
            Err(restore_error) => Err(CheckoutRefreshFailure {
                error: anyhow::anyhow!(
                    "checkout refresh failed ({error:#}) and restoring its exact previous bytes also failed ({restore_error:#})"
                ),
                restored: false,
            }),
        };
    }
    Ok(())
}

fn prepare_checkout_refresh(
    repo: &Repo,
    old: &str,
    new: &str,
    scope: CheckoutScope,
    recovering: bool,
    attributes_upgrade: Option<(&AttributesLayers, bool)>,
) -> Result<CheckoutPreparation> {
    let attributes = attributes_upgrade
        .map(|(upgrade, target_is_v1)| prepare_attributes_refresh(repo, upgrade, target_is_v1))
        .transpose()?;
    let mut paths = checkout_paths(repo, old, new, scope)?;
    if attributes.is_some() {
        paths.retain(|path| path != meta::ATTRS_FILE);
    }
    reject_path_topology_changes(&paths)?;
    if matches!(scope, CheckoutScope::Full) && !recovering {
        let staged = checkout_git(repo, &["diff", "--cached", "--name-only", old, "--"])?;
        anyhow::ensure!(
            staged.is_empty(),
            "cannot refresh the checked-out branch while it has staged changes:\n{staged}"
        );
    }

    let wanted: std::collections::BTreeSet<&str> = paths.iter().map(String::as_str).collect();
    let old_files = checkout_tree_entries(repo, old, &wanted, scope)?;
    let new_files = checkout_tree_entries(repo, new, &wanted, scope)?;
    let index_files = if recovering || matches!(scope, CheckoutScope::Storage) {
        Some(checkout_index_files(repo, &paths)?)
    } else {
        None
    };
    let mut snapshot = Vec::with_capacity(paths.len());
    let mut snapshot_bytes = 0usize;
    for path in &paths {
        let remaining = storage::MAX_MATERIALIZED_BYTES
            .checked_sub(snapshot_bytes)
            .context("checkout snapshot byte count overflowed")?;
        let before = checkout_file_with_limit(repo.root(), path, remaining)?;
        snapshot_bytes = snapshot_bytes
            .checked_add(checkout_file_bytes(&before))
            .context("checkout snapshot byte count overflowed")?;
        anyhow::ensure!(
            snapshot_bytes <= storage::MAX_MATERIALIZED_BYTES,
            "checkout snapshot exceeds the {}-byte safety limit",
            storage::MAX_MATERIALIZED_BYTES
        );
        let old_file = old_files.get(path);
        let new_file = new_files.get(path);
        if let Some(index_files) = &index_files {
            let index_file = index_files.get(path);
            anyhow::ensure!(
                index_matches_tree(index_file, old_file)
                    || index_matches_tree(index_file, new_file),
                "refusing to overwrite index path {path:?}: it matches neither {old} nor {new}"
            );
        }
        if new_file.is_some() {
            ensure_checkout_parents_are_directories(repo.root(), path)?;
        }
        snapshot.push((path.clone(), before));
    }
    ensure_checkout_snapshot_matches_endpoints(
        repo, &paths, &snapshot, &old_files, &new_files, old, new,
    )?;
    let absent_parents = absent_checkout_parents(repo.root(), &paths)?;
    Ok(CheckoutPreparation {
        paths,
        new_files,
        snapshot,
        absent_parents,
        attributes,
    })
}

fn prepare_attributes_refresh(
    repo: &Repo,
    original: &AttributesLayers,
    target_is_v1: bool,
) -> Result<PreparedAttributesUpgrade> {
    let upgraded = normalize_attributes_layers(original)?;
    let before = capture_attributes_layers(repo)?;
    anyhow::ensure!(
        before.index == original.index || before.index == upgraded.index,
        "refusing to overwrite staged {}: it matches neither the journalled v0 layer nor its normalized v1 layer",
        meta::ATTRS_FILE
    );
    anyhow::ensure!(
        before.worktree == original.worktree || before.worktree == upgraded.worktree,
        "refusing to overwrite worktree {}: it matches neither the journalled v0 layer nor its normalized v1 layer",
        meta::ATTRS_FILE
    );
    Ok(PreparedAttributesUpgrade {
        before,
        target: if target_is_v1 {
            upgraded
        } else {
            original.clone()
        },
    })
}

fn checkout_git(repo: &Repo, args: &[&str]) -> Result<String> {
    record_checkout_git_process();
    repo.git(args)
}

fn checkout_git_output(repo: &Repo, args: &[&str]) -> Result<std::process::Output> {
    checkout_git_command(repo)
        .args(args)
        .output()
        .map_err(Into::into)
}

fn checkout_git_command(repo: &Repo) -> std::process::Command {
    record_checkout_git_process();
    let mut command = std::process::Command::new("git");
    command
        .arg("--no-replace-objects")
        .arg("-C")
        .arg(repo.root());
    command
}

#[cfg(test)]
thread_local! {
    static CHECKOUT_GIT_PROCESS_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn record_checkout_git_process() {
    CHECKOUT_GIT_PROCESS_COUNT.with(|count| count.set(count.get() + 1));
}

#[cfg(not(test))]
fn record_checkout_git_process() {}

#[cfg(test)]
fn reset_checkout_git_process_count() {
    CHECKOUT_GIT_PROCESS_COUNT.with(|count| count.set(0));
}

#[cfg(test)]
fn checkout_git_process_count() -> usize {
    CHECKOUT_GIT_PROCESS_COUNT.with(std::cell::Cell::get)
}

fn checkout_paths(repo: &Repo, old: &str, new: &str, scope: CheckoutScope) -> Result<Vec<String>> {
    let mut paths = std::collections::BTreeSet::new();
    let changed = checkout_changed_paths(repo, old, new)?;
    match scope {
        CheckoutScope::Full => {
            paths.extend(changed);
        }
        CheckoutScope::Storage => {
            let old_layout = storage_layout_at(repo, old)?;
            let new_layout = storage_layout_at(repo, new)?;
            paths.extend(changed.into_iter().filter(|path| {
                meta::is_storage_path_for(old_layout, path.as_str())
                    || meta::is_storage_path_for(new_layout, path.as_str())
                    || path == meta::ATTRS_FILE
            }));
        }
        CheckoutScope::Root => {
            anyhow::bail!("root checkout paths use the dedicated forward materializer")
        }
    }
    Ok(paths.into_iter().collect())
}

fn checkout_changed_paths(repo: &Repo, old: &str, new: &str) -> Result<Vec<String>> {
    let output = checkout_git_output(
        repo,
        &["diff", "--name-only", "-z", "--no-renames", old, new, "--"],
    )?;
    if !output.status.success() {
        anyhow::bail!(
            "git diff --name-only {old} {new} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    if output.stdout.is_empty() {
        return Ok(Vec::new());
    }
    anyhow::ensure!(
        output.stdout.last() == Some(&0),
        "git diff returned an unterminated checkout path"
    );
    output.stdout[..output.stdout.len() - 1]
        .split(|byte| *byte == 0)
        .map(|path| {
            anyhow::ensure!(!path.is_empty(), "git diff returned an empty checkout path");
            std::str::from_utf8(path)
                .context("checkout path is not valid UTF-8")
                .map(str::to_owned)
        })
        .collect()
}

/// Prove that a possibly half-refreshed v0 checkout contains only absent or already-correct v1
/// root namespace entries. This is stricter than looking at the worktree meta: a process can stop
/// after writing `session/meta.json` but before replacing an ignored `events/` path.
pub fn ensure_v1_namespace_absent_or_matches(repo: &Repo, new: &str) -> Result<()> {
    for path in [meta::LOG_FILE, meta::VIEW_FILE] {
        let local = checkout_file(repo.root(), path)?;
        let expected = tree_file(repo, new, path)?;
        anyhow::ensure!(
            matches!(local, CheckoutFile::Absent)
                || checkout_matches_tree(&local, expected.as_ref()),
            "refusing interrupted v1 checkout: root path {path:?} is neither absent nor the expected v1 object"
        );
    }

    let expected: std::collections::BTreeSet<String> = repo
        .git(&["ls-tree", "-r", "--name-only", new, "--", meta::EVENTS_DIR])?
        .lines()
        .filter(|path| path.starts_with("events/"))
        .map(str::to_owned)
        .collect();
    let events_root = repo.root().join(meta::EVENTS_DIR);
    match std::fs::symlink_metadata(&events_root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => anyhow::bail!(
            "refusing interrupted v1 checkout: {} is not the expected events directory",
            events_root.display()
        ),
    }
    verify_existing_event_namespace(repo, new, &events_root, &expected)
}

fn verify_existing_event_namespace(
    repo: &Repo,
    new: &str,
    directory: &std::path::Path,
    expected: &std::collections::BTreeSet<String>,
) -> Result<()> {
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let absolute = entry.path();
        let relative = absolute
            .strip_prefix(repo.root())?
            .to_str()
            .context("events path is not valid UTF-8")?
            .replace(std::path::MAIN_SEPARATOR, "/");
        let metadata = std::fs::symlink_metadata(&absolute)?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            let prefix = format!("{relative}/");
            anyhow::ensure!(
                expected.iter().any(|path| path.starts_with(&prefix)),
                "refusing interrupted v1 checkout: unexpected directory {relative:?} exists under events/"
            );
            verify_existing_event_namespace(repo, new, &absolute, expected)?;
            continue;
        }
        anyhow::ensure!(
            expected.contains(&relative),
            "refusing interrupted v1 checkout: unexpected path {relative:?} exists under events/"
        );
        let local = checkout_file(repo.root(), &relative)?;
        let target = tree_file(repo, new, &relative)?;
        anyhow::ensure!(
            checkout_matches_tree(&local, target.as_ref()),
            "refusing interrupted v1 checkout: event path {relative:?} differs from the v1 snapshot"
        );
    }
    Ok(())
}

fn reject_path_topology_changes(paths: &[String]) -> Result<()> {
    let set: std::collections::BTreeSet<&str> = paths.iter().map(String::as_str).collect();
    for path in paths {
        let mut parent = path.rsplit_once('/').map(|(parent, _)| parent);
        while let Some(parent_path) = parent {
            anyhow::ensure!(
                !set.contains(parent_path),
                "cannot transactionally refresh a file/directory topology change at {parent_path:?}"
            );
            parent = parent_path.rsplit_once('/').map(|(next, _)| next);
        }
    }
    Ok(())
}

fn tree_file(repo: &Repo, commit: &str, path: &str) -> Result<Option<TreeFile>> {
    let output = std::process::Command::new("git")
        .arg("--no-replace-objects")
        .arg("-C")
        .arg(repo.root())
        .args(["ls-tree", commit, "--", path])
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "git ls-tree {commit} -- {path} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    if output.stdout.is_empty() {
        return Ok(None);
    }
    let record = String::from_utf8(output.stdout)?;
    let (header, returned_path) = record
        .trim_end_matches('\n')
        .split_once('\t')
        .context("git ls-tree returned a malformed record")?;
    anyhow::ensure!(
        returned_path == path,
        "git ls-tree returned an unexpected path"
    );
    let mut fields = header.split_whitespace();
    let mode = fields.next().context("git ls-tree omitted mode")?;
    let kind = fields.next().context("git ls-tree omitted type")?;
    let oid = fields.next().context("git ls-tree omitted object id")?;
    anyhow::ensure!(kind == "blob", "{commit}:{path} is not a blob");
    let bytes = git_bytes_checked(repo, &["cat-file", "blob", oid])?;
    Ok(Some(TreeFile {
        mode: mode.to_owned(),
        bytes,
    }))
}

#[derive(Clone)]
struct TreeEntry {
    mode: String,
    oid: String,
}

#[derive(Clone)]
struct IndexFile {
    mode: String,
    oid: String,
}

fn checkout_tree_entries(
    repo: &Repo,
    commit: &str,
    wanted: &std::collections::BTreeSet<&str>,
    scope: CheckoutScope,
) -> Result<std::collections::BTreeMap<String, TreeEntry>> {
    let wanted_bytes: std::collections::BTreeMap<&[u8], &str> =
        wanted.iter().map(|path| (path.as_bytes(), *path)).collect();
    let mut args = vec![
        "ls-tree".to_owned(),
        "-r".to_owned(),
        "-z".to_owned(),
        "--full-tree".to_owned(),
        commit.to_owned(),
    ];
    if matches!(scope, CheckoutScope::Storage) {
        args.push("--".to_owned());
        args.extend(storage_roots().into_iter().map(str::to_owned));
        args.push(meta::ATTRS_FILE.to_owned());
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = checkout_git_output(repo, &refs)?;
    if !output.status.success() {
        anyhow::bail!(
            "git ls-tree {commit} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let mut entries = std::collections::BTreeMap::new();
    for record in output.stdout.split(|byte| *byte == 0) {
        if record.is_empty() {
            continue;
        }
        let tab = record
            .iter()
            .position(|byte| *byte == b'\t')
            .context("git ls-tree returned a malformed record")?;
        let Some(path) = wanted_bytes.get(&record[tab + 1..]).copied() else {
            continue;
        };
        let header = std::str::from_utf8(&record[..tab])
            .context("git ls-tree returned a non-UTF-8 header")?;
        let mut fields = header.split_ascii_whitespace();
        let mode = fields.next().context("git ls-tree omitted mode")?;
        let kind = fields.next().context("git ls-tree omitted object type")?;
        let oid = fields.next().context("git ls-tree omitted object id")?;
        anyhow::ensure!(fields.next().is_none(), "git ls-tree added header fields");
        anyhow::ensure!(kind == "blob", "{commit}:{path} is not a blob");
        anyhow::ensure!(
            matches!(mode, "100644" | "100755" | "120000"),
            "unsupported checkout mode {mode} at {commit}:{path}"
        );
        anyhow::ensure!(
            entries
                .insert(
                    path.to_owned(),
                    TreeEntry {
                        mode: mode.to_owned(),
                        oid: oid.to_owned(),
                    },
                )
                .is_none(),
            "git ls-tree returned duplicate path {path:?}"
        );
    }
    Ok(entries)
}

#[derive(Clone, Copy)]
enum CheckoutEndpoint {
    Old,
    New,
}

#[derive(Clone, Copy)]
struct CheckoutBlobCandidate {
    path_index: usize,
    endpoint: CheckoutEndpoint,
}

const CHECKOUT_BLOB_BUFFER_BYTES: usize = 64 * 1024;

fn ensure_checkout_snapshot_matches_endpoints(
    repo: &Repo,
    paths: &[String],
    snapshot: &[(String, CheckoutFile)],
    old_files: &std::collections::BTreeMap<String, TreeEntry>,
    new_files: &std::collections::BTreeMap<String, TreeEntry>,
    old: &str,
    new: &str,
) -> Result<()> {
    anyhow::ensure!(
        paths.len() == snapshot.len(),
        "checkout snapshot path count changed"
    );
    let mut old_matches = vec![false; paths.len()];
    let mut new_matches = vec![false; paths.len()];
    let mut candidates = std::collections::BTreeMap::<String, Vec<CheckoutBlobCandidate>>::new();

    for (path_index, (path, (snapshot_path, local))) in
        paths.iter().zip(snapshot.iter()).enumerate()
    {
        anyhow::ensure!(
            path == snapshot_path,
            "checkout snapshot path order changed"
        );
        add_checkout_blob_candidate(
            local,
            old_files.get(path),
            path_index,
            CheckoutEndpoint::Old,
            &mut old_matches,
            &mut candidates,
        );
        add_checkout_blob_candidate(
            local,
            new_files.get(path),
            path_index,
            CheckoutEndpoint::New,
            &mut new_matches,
            &mut candidates,
        );
    }

    let requests: Vec<String> = candidates.keys().cloned().collect();
    stream_checkout_blobs(repo, &requests, |oid, size, blob| {
        let oid_candidates = candidates
            .get(oid)
            .context("cat-file returned an unrequested checkout object")?;
        for candidate in oid_candidates {
            let bytes = checkout_file_payload(&snapshot[candidate.path_index].1)
                .context("checkout blob candidate lost its byte payload")?;
            *checkout_endpoint_match(
                candidate.endpoint,
                candidate.path_index,
                &mut old_matches,
                &mut new_matches,
            ) = bytes.len() == size;
        }

        let mut offset = 0usize;
        let mut buffer = [0u8; CHECKOUT_BLOB_BUFFER_BYTES];
        while offset < size {
            let count = (size - offset).min(buffer.len());
            blob.read_exact(&mut buffer[..count])?;
            for candidate in oid_candidates {
                let matches = checkout_endpoint_match(
                    candidate.endpoint,
                    candidate.path_index,
                    &mut old_matches,
                    &mut new_matches,
                );
                if *matches {
                    let bytes = checkout_file_payload(&snapshot[candidate.path_index].1)
                        .expect("candidate payload was checked above");
                    if bytes[offset..offset + count] != buffer[..count] {
                        *matches = false;
                    }
                }
            }
            offset += count;
        }
        Ok(())
    })?;

    for (path_index, path) in paths.iter().enumerate() {
        anyhow::ensure!(
            old_matches[path_index] || new_matches[path_index],
            "refusing to overwrite checked-out path {path:?}: its bytes/type match neither {old} nor {new}"
        );
    }
    Ok(())
}

fn add_checkout_blob_candidate(
    local: &CheckoutFile,
    tree: Option<&TreeEntry>,
    path_index: usize,
    endpoint: CheckoutEndpoint,
    endpoint_matches: &mut [bool],
    candidates: &mut std::collections::BTreeMap<String, Vec<CheckoutBlobCandidate>>,
) {
    let Some(tree) = tree else {
        endpoint_matches[path_index] = matches!(local, CheckoutFile::Absent);
        return;
    };
    if checkout_mode_matches_tree(local, tree) {
        candidates
            .entry(tree.oid.clone())
            .or_default()
            .push(CheckoutBlobCandidate {
                path_index,
                endpoint,
            });
    }
}

fn checkout_mode_matches_tree(local: &CheckoutFile, tree: &TreeEntry) -> bool {
    match local {
        CheckoutFile::Regular { executable, .. } => {
            matches!(tree.mode.as_str(), "100644" | "100755")
                && *executable == (tree.mode == "100755")
        }
        CheckoutFile::Symlink(_) => tree.mode == "120000",
        CheckoutFile::Absent | CheckoutFile::Directory => false,
    }
}

fn checkout_file_payload(file: &CheckoutFile) -> Option<&[u8]> {
    match file {
        CheckoutFile::Regular { bytes, .. } | CheckoutFile::Symlink(bytes) => Some(bytes),
        CheckoutFile::Absent | CheckoutFile::Directory => None,
    }
}

fn checkout_endpoint_match<'a>(
    endpoint: CheckoutEndpoint,
    path_index: usize,
    old_matches: &'a mut [bool],
    new_matches: &'a mut [bool],
) -> &'a mut bool {
    match endpoint {
        CheckoutEndpoint::Old => &mut old_matches[path_index],
        CheckoutEndpoint::New => &mut new_matches[path_index],
    }
}

fn stream_checkout_blobs(
    repo: &Repo,
    requests: &[String],
    mut visit: impl FnMut(&str, usize, &mut dyn Read) -> Result<()>,
) -> Result<()> {
    if requests.is_empty() {
        return Ok(());
    }
    let mut child = checkout_git_command(repo)
        .args(["cat-file", "--batch"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .context("git cat-file stdin is missing")?;
    let writer_requests = requests.to_vec();
    let writer = std::thread::spawn(move || -> std::io::Result<()> {
        for oid in writer_requests {
            stdin.write_all(oid.as_bytes())?;
            stdin.write_all(b"\n")?;
        }
        Ok(())
    });
    let stdout = child
        .stdout
        .take()
        .context("git cat-file stdout is missing")?;
    let mut stderr = child
        .stderr
        .take()
        .context("git cat-file stderr is missing")?;
    let stderr_reader = std::thread::spawn(move || -> std::io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes)?;
        Ok(bytes)
    });
    let mut reader = std::io::BufReader::new(stdout);

    let stream_result = (|| -> Result<()> {
        for requested in requests {
            let mut header = Vec::new();
            anyhow::ensure!(
                reader.read_until(b'\n', &mut header)? > 0,
                "git cat-file omitted blob {requested}"
            );
            anyhow::ensure!(header.pop() == Some(b'\n'), "unterminated cat-file header");
            let header = std::str::from_utf8(&header).context("cat-file header is not UTF-8")?;
            let mut fields = header.split_ascii_whitespace();
            let returned = fields.next().context("cat-file header omitted object id")?;
            let kind = fields
                .next()
                .context("cat-file header omitted object type")?;
            let size: usize = fields
                .next()
                .context("cat-file header omitted object size")?
                .parse()
                .context("cat-file returned an invalid object size")?;
            anyhow::ensure!(fields.next().is_none(), "cat-file added header fields");
            anyhow::ensure!(returned == requested, "cat-file returned the wrong object");
            anyhow::ensure!(kind == "blob", "checkout object {requested} is not a blob");
            let mut blob = (&mut reader).take(size as u64);
            visit(requested, size, &mut blob)?;
            anyhow::ensure!(
                blob.limit() == 0,
                "checkout blob visitor did not consume {requested}"
            );
            let mut terminator = [0];
            reader.read_exact(&mut terminator)?;
            anyhow::ensure!(terminator == *b"\n", "cat-file blob lacks its terminator");
        }
        anyhow::ensure!(
            reader.fill_buf()?.is_empty(),
            "cat-file returned extra output"
        );
        Ok(())
    })();
    if stream_result.is_err() {
        let _ = child.kill();
    }
    drop(reader);
    let wait_result = child.wait();
    let writer_result = writer
        .join()
        .map_err(|_| anyhow::anyhow!("git cat-file request writer panicked"));
    let stderr_result = stderr_reader
        .join()
        .map_err(|_| anyhow::anyhow!("git cat-file stderr reader panicked"));

    stream_result?;
    writer_result??;
    let stderr = stderr_result??;
    let status = wait_result?;
    if !status.success() {
        anyhow::bail!(
            "git cat-file --batch failed: {}",
            String::from_utf8_lossy(&stderr).trim()
        );
    }
    Ok(())
}

fn checkout_index_files(
    repo: &Repo,
    paths: &[String],
) -> Result<std::collections::BTreeMap<String, IndexFile>> {
    let wanted: std::collections::BTreeMap<&[u8], &str> = paths
        .iter()
        .map(|path| (path.as_bytes(), path.as_str()))
        .collect();
    let output = checkout_git_output(repo, &["ls-files", "--stage", "-z"])?;
    if !output.status.success() {
        anyhow::bail!(
            "git ls-files --stage failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let mut entries = std::collections::BTreeMap::new();
    for record in output.stdout.split(|byte| *byte == 0) {
        if record.is_empty() {
            continue;
        }
        let tab = record
            .iter()
            .position(|byte| *byte == b'\t')
            .context("git ls-files returned a malformed record")?;
        let Some(path) = wanted.get(&record[tab + 1..]).copied() else {
            continue;
        };
        let header = std::str::from_utf8(&record[..tab])
            .context("git ls-files returned a non-UTF-8 header")?;
        let mut fields = header.split_ascii_whitespace();
        let mode = fields.next().context("git ls-files omitted mode")?;
        let oid = fields.next().context("git ls-files omitted object id")?;
        let stage = fields.next().context("git ls-files omitted stage")?;
        anyhow::ensure!(fields.next().is_none(), "git ls-files added header fields");
        anyhow::ensure!(stage == "0", "index path {path:?} is unmerged");
        anyhow::ensure!(
            entries
                .insert(
                    path.to_owned(),
                    IndexFile {
                        mode: mode.to_owned(),
                        oid: oid.to_owned(),
                    },
                )
                .is_none(),
            "git ls-files returned duplicate path {path:?}"
        );
    }
    Ok(entries)
}

fn index_matches_tree(index: Option<&IndexFile>, tree: Option<&TreeEntry>) -> bool {
    match (index, tree) {
        (None, None) => true,
        (Some(index), Some(tree)) => index.mode == tree.mode && index.oid == tree.oid,
        _ => false,
    }
}

fn git_bytes_checked(repo: &Repo, args: &[&str]) -> Result<Vec<u8>> {
    let output = std::process::Command::new("git")
        .arg("--no-replace-objects")
        .arg("-C")
        .arg(repo.root())
        .args(args)
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

fn checkout_file(root: &std::path::Path, path: &str) -> Result<CheckoutFile> {
    checkout_file_with_limit(root, path, storage::MAX_MATERIALIZED_BYTES)
}

fn checkout_file_with_limit(
    root: &std::path::Path,
    path: &str,
    byte_limit: usize,
) -> Result<CheckoutFile> {
    let absolute = root.join(path);
    let metadata = match std::fs::symlink_metadata(&absolute) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CheckoutFile::Absent);
        }
        Err(error) => return Err(error).with_context(|| format!("cannot inspect {path}")),
    };
    if metadata.file_type().is_symlink() {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt as _;
            let bytes = std::fs::read_link(&absolute)?
                .as_os_str()
                .as_bytes()
                .to_vec();
            anyhow::ensure!(
                bytes.len() <= byte_limit,
                "worktree path {path:?} exceeds its {byte_limit}-byte snapshot safety limit"
            );
            return Ok(CheckoutFile::Symlink(bytes));
        }
        #[cfg(not(unix))]
        anyhow::bail!("symlink checkout refresh is unsupported on this platform");
    }
    if metadata.is_dir() {
        return Ok(CheckoutFile::Directory);
    }
    anyhow::ensure!(metadata.is_file(), "unsupported worktree entry at {path:?}");
    #[cfg(unix)]
    let executable = {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111 != 0
    };
    #[cfg(not(unix))]
    let executable = false;
    let mut bytes = Vec::with_capacity(
        usize::try_from(metadata.len())
            .unwrap_or(byte_limit.saturating_add(1))
            .min(byte_limit.saturating_add(1)),
    );
    std::fs::File::open(&absolute)?
        .take(byte_limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= byte_limit,
        "worktree path {path:?} exceeds its {byte_limit}-byte snapshot safety limit"
    );
    Ok(CheckoutFile::Regular { bytes, executable })
}

fn checkout_file_bytes(file: &CheckoutFile) -> usize {
    checkout_file_payload(file).map_or(0, <[u8]>::len)
}

fn checkout_matches_tree(local: &CheckoutFile, tree: Option<&TreeFile>) -> bool {
    match (local, tree) {
        (CheckoutFile::Absent, None) => true,
        (CheckoutFile::Symlink(local), Some(tree)) => tree.mode == "120000" && *local == tree.bytes,
        (
            CheckoutFile::Regular { bytes, executable },
            Some(TreeFile {
                mode,
                bytes: expected,
            }),
        ) => {
            matches!(mode.as_str(), "100644" | "100755")
                && bytes == expected
                && (*executable == (mode == "100755"))
        }
        _ => false,
    }
}

fn ensure_checkout_parents_are_directories(root: &std::path::Path, path: &str) -> Result<()> {
    let relative = std::path::Path::new(path);
    let mut current = root.to_path_buf();
    if let Some(parent) = relative.parent() {
        for component in parent.components() {
            current.push(component);
            match std::fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
                Ok(_) => anyhow::bail!(
                    "refusing to traverse symlink or non-directory worktree ancestor {}",
                    current.display()
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(())
}

/// Record directory ancestors that do not exist before a transactional checkout. Git may create
/// them while installing new event objects; rollback must remove those empty directories as well
/// as the leaf files or a failed v0 -> v1 upgrade would not restore the exact prior worktree.
fn absent_checkout_parents(root: &std::path::Path, paths: &[String]) -> Result<Vec<String>> {
    let mut absent = std::collections::BTreeSet::new();
    for path in paths {
        let mut relative = std::path::PathBuf::new();
        let Some(parent) = std::path::Path::new(path).parent() else {
            continue;
        };
        for component in parent.components() {
            relative.push(component);
            match std::fs::symlink_metadata(root.join(&relative)) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
                Ok(_) => anyhow::bail!(
                    "refusing to traverse symlink or non-directory worktree ancestor {}",
                    root.join(&relative).display()
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    absent.insert(relative.clone());
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
    let mut absent: Vec<String> = absent
        .into_iter()
        .map(|path| {
            path.to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/")
        })
        .collect();
    absent.sort_by_key(|path| std::cmp::Reverse(path.matches('/').count()));
    Ok(absent)
}

fn apply_checkout(
    repo: &Repo,
    new: &str,
    paths: &[String],
    new_files: &std::collections::BTreeMap<String, TreeEntry>,
    scope: CheckoutScope,
) -> Result<()> {
    reset_checkout_index(repo, new, paths)?;
    for path in paths {
        if !new_files.contains_key(path) {
            remove_checkout_leaf(repo.root(), path)?;
            maybe_crash_checkout_at("during_apply");
        }
    }
    let present: Vec<&str> = paths
        .iter()
        .filter(|path| new_files.contains_key(path.as_str()))
        .map(String::as_str)
        .collect();
    let midpoint = present.len().div_ceil(2);
    checkout_index_batch(repo, &present[..midpoint])?;
    if !present.is_empty() {
        maybe_crash_checkout_at("during_apply");
    }
    checkout_index_batch(repo, &present[midpoint..])?;
    verify_checkout(repo, new, paths, scope)
}

fn apply_attributes_layers(repo: &Repo, target: &AttributesLayers) -> Result<()> {
    validate_attributes_layers(target)?;
    match &target.index {
        Some(text) => {
            let oid = raw_git(
                repo,
                &["hash-object", "-w", "--no-filters", "--stdin"],
                Some(text),
            )?;
            repo.git(&[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{},{}", oid.trim(), meta::ATTRS_FILE),
            ])?;
        }
        None => {
            repo.git(&["update-index", "--force-remove", "--", meta::ATTRS_FILE])?;
        }
    }
    maybe_crash_checkout_at("during_attributes");
    write_attributes_worktree(repo, target.worktree.as_deref())?;
    let current = capture_attributes_layers(repo)?;
    anyhow::ensure!(
        current == *target,
        "checkout {} layers did not converge to the journalled state",
        meta::ATTRS_FILE
    );
    Ok(())
}

fn write_attributes_worktree(repo: &Repo, text: Option<&str>) -> Result<()> {
    let path = repo.root().join(meta::ATTRS_FILE);
    let Some(text) = text else {
        remove_checkout_leaf(repo.root(), meta::ATTRS_FILE)?;
        sync_directory(repo.root())?;
        return Ok(());
    };
    storage::attributes_text_strict(Some(text))?;
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) => anyhow::ensure!(
            metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
            "refusing to replace non-regular worktree {}",
            meta::ATTRS_FILE
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let parent = path.parent().context("attributes path has no parent")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("cannot create temporary {}", meta::ATTRS_FILE))?;
    temporary.write_all(text.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        temporary
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o644))?;
    }
    temporary.as_file().sync_all()?;
    temporary
        .persist(&path)
        .map_err(|error| error.error)
        .with_context(|| format!("cannot publish worktree {}", meta::ATTRS_FILE))?;
    sync_directory(parent)?;
    Ok(())
}

fn checkout_index_batch(repo: &Repo, paths: &[&str]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut child = checkout_git_command(repo)
        .args(["checkout-index", "-q", "-f", "-z", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    {
        let mut stdin = child
            .stdin
            .take()
            .context("git checkout-index stdin is missing")?;
        for path in paths {
            anyhow::ensure!(!path.as_bytes().contains(&0), "checkout path contains NUL");
            stdin.write_all(path.as_bytes())?;
            stdin.write_all(&[0])?;
        }
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        anyhow::bail!(
            "git checkout-index --stdin failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    static FAIL_CHECKOUT_AFTER_APPLY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(super) fn fail_next_checkout_postflight() {
    FAIL_CHECKOUT_AFTER_APPLY.with(|fail| fail.set(true));
}

#[cfg(test)]
fn maybe_crash_checkout_at(point: &str) {
    if std::env::var("AGIT_TEST_CHECKOUT_CRASH_AT").as_deref() == Ok(point) {
        std::process::exit(86);
    }
}

#[cfg(not(test))]
fn maybe_crash_checkout_at(_point: &str) {}

fn reset_checkout_index(repo: &Repo, commit: &str, paths: &[String]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut child = checkout_git_command(repo)
        .arg("--literal-pathspecs")
        .args([
            "reset",
            "-q",
            commit,
            "--pathspec-from-file=-",
            "--pathspec-file-nul",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    {
        let mut stdin = child.stdin.take().context("git reset stdin is missing")?;
        for path in paths {
            anyhow::ensure!(!path.as_bytes().contains(&0), "checkout path contains NUL");
            stdin.write_all(path.as_bytes())?;
            stdin.write_all(&[0])?;
        }
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        anyhow::bail!(
            "git reset {commit} for checkout paths failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn remove_checkout_leaf(root: &std::path::Path, path: &str) -> Result<()> {
    let absolute = root.join(path);
    match std::fs::symlink_metadata(&absolute) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            anyhow::bail!(
                "refusing to recursively delete directory {}",
                absolute.display()
            )
        }
        Ok(_) => std::fs::remove_file(&absolute)
            .with_context(|| format!("cannot remove checked-out path {path}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn verify_checkout(
    repo: &Repo,
    commit: &str,
    paths: &[String],
    _scope: CheckoutScope,
) -> Result<()> {
    let dirty = checkout_dirty_transaction_paths(repo, paths)?;
    anyhow::ensure!(
        dirty.is_empty(),
        "checked-out transaction paths did not converge to {commit}: {dirty:?}"
    );
    Ok(())
}

fn checkout_dirty_transaction_paths(repo: &Repo, paths: &[String]) -> Result<Vec<String>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let output = checkout_git_output(
        repo,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )?;
    if !output.status.success() {
        anyhow::bail!(
            "git status for checkout verification failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let wanted: std::collections::BTreeMap<&[u8], &str> = paths
        .iter()
        .map(|path| (path.as_bytes(), path.as_str()))
        .collect();
    let records: Vec<&[u8]> = output.stdout.split(|byte| *byte == 0).collect();
    anyhow::ensure!(
        records.last().is_some_and(|record| record.is_empty()),
        "git status returned an unterminated checkout path"
    );
    let mut dirty = std::collections::BTreeSet::new();
    let mut position = 0usize;
    while position + 1 < records.len() {
        let record = records[position];
        anyhow::ensure!(
            record.len() >= 4 && record[2] == b' ',
            "git status returned a malformed porcelain record"
        );
        if let Some(path) = wanted.get(&record[3..]) {
            dirty.insert((*path).to_owned());
        }
        let renamed_or_copied =
            matches!(record[0], b'R' | b'C') || matches!(record[1], b'R' | b'C');
        if renamed_or_copied {
            position += 1;
            anyhow::ensure!(
                position + 1 < records.len(),
                "git status omitted a rename source path"
            );
            if let Some(path) = wanted.get(records[position]) {
                dirty.insert((*path).to_owned());
            }
        }
        position += 1;
    }
    Ok(dirty.into_iter().collect())
}

fn storage_roots() -> [&'static str; 6] {
    [
        meta::FILE,
        meta::LOG_FILE,
        meta::VIEW_FILE,
        meta::LEGACY_LOG_FILE,
        meta::LEGACY_VIEW_FILE,
        meta::EVENTS_DIR,
    ]
}

fn restore_checkout(
    repo: &Repo,
    old: &str,
    paths: &[String],
    snapshot: &[(String, CheckoutFile)],
    absent_parents: &[String],
    _scope: CheckoutScope,
) -> Result<()> {
    reset_checkout_index(repo, old, paths)?;
    for (path, _) in snapshot.iter().rev() {
        remove_checkout_leaf(repo.root(), path)?;
    }
    for (path, state) in snapshot {
        let absolute = repo.root().join(path);
        match state {
            CheckoutFile::Absent => {}
            CheckoutFile::Regular { bytes, executable } => {
                if let Some(parent) = absolute.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&absolute, bytes)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    let mode = if *executable { 0o755 } else { 0o644 };
                    std::fs::set_permissions(&absolute, std::fs::Permissions::from_mode(mode))?;
                }
            }
            CheckoutFile::Symlink(target) => {
                #[cfg(unix)]
                {
                    use std::os::unix::ffi::OsStrExt as _;
                    use std::os::unix::fs::symlink;
                    if let Some(parent) = absolute.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    symlink(std::ffi::OsStr::from_bytes(target), &absolute)?;
                }
                #[cfg(not(unix))]
                anyhow::bail!("cannot restore symlink on this platform");
            }
            CheckoutFile::Directory => {
                std::fs::create_dir_all(&absolute)?;
            }
        }
    }
    for relative in absent_parents {
        let directory = repo.root().join(relative);
        match std::fs::remove_dir(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "cannot remove checkout directory created during failed refresh: {}",
                        directory.display()
                    )
                });
            }
        }
    }
    Ok(())
}

fn validate_tree_path(path: &str) -> Result<()> {
    if path.is_empty() {
        anyhow::bail!("tree edit path must not be empty");
    }
    if path.as_bytes().contains(&0) {
        anyhow::bail!("tree edit path contains NUL");
    }
    if path.starts_with('/') || path.ends_with('/') {
        anyhow::bail!("tree edit path must be a relative git path: {path:?}");
    }
    if path
        .split('/')
        .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        anyhow::bail!("tree edit path contains a non-canonical component: {path:?}");
    }
    Ok(())
}

fn hash_owned_blobs(
    git_dir: &std::path::Path,
    scratch: &std::path::Path,
    input: &[u8],
    expected: usize,
) -> Result<Vec<String>> {
    let mut child = std::process::Command::new("git")
        .arg("--no-replace-objects")
        .args(["hash-object", "-w", "--no-filters", "--stdin-paths"])
        .env("GIT_DIR", git_dir)
        .current_dir(scratch)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .expect("hash-object stdin already taken")
        .write_all(input)?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        anyhow::bail!(
            "git hash-object --stdin-paths failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let stdout = String::from_utf8(out.stdout)?;
    let hashes: Vec<String> = stdout.lines().map(str::to_string).collect();
    if hashes.len() != expected
        || hashes
            .iter()
            .any(|oid| oid.is_empty() || !oid.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        anyhow::bail!(
            "git hash-object returned {} valid-looking object ids for {expected} inputs",
            hashes.len()
        );
    }
    Ok(hashes)
}

fn run_with_index(
    repo: &Repo,
    index: &std::path::Path,
    args: &[&str],
    stdin: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let mut child = std::process::Command::new("git")
        .arg("--no-replace-objects")
        .args(args)
        .current_dir(repo.root())
        .env("GIT_INDEX_FILE", index)
        .stdin(if stdin.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    if let Some(input) = stdin {
        child
            .stdin
            .take()
            .expect("git index stdin already taken")
            .write_all(input)?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

/// Worktree paths changed relative to `since_commit` (including untracked new files), minus
/// `exclude`.
///
/// A merge agent reconciles shared files by **editing the worktree directly** (the design says so
/// explicitly: that is its job); those bytes pass through neither the index nor a commit, so
/// settlement has to collect them from the worktree itself.
pub fn worktree_changes(repo: &Repo, since_commit: &str, exclude: &[&str]) -> Result<Vec<String>> {
    let mut out: Vec<String> = vec![];
    // Tracked: commit tree ↔ worktree (the index is not consulted; the agent does not `git add`
    // for us).
    let list = repo.git(&["diff", "--name-only", "--no-renames", since_commit, "--"])?;
    out.extend(list.lines().filter(|l| !l.is_empty()).map(str::to_string));
    // Untracked: a newly written memory/skills file is not in the tree the first time it appears.
    let list = repo.git(&["ls-files", "--others", "--exclude-standard"])?;
    out.extend(list.lines().filter(|l| !l.is_empty()).map(str::to_string));
    out.retain(|p| {
        !exclude.iter().any(|excluded| {
            p == excluded
                || p.strip_prefix(excluded)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        })
    });
    out.sort();
    out.dedup();
    Ok(out)
}

/// Overlay the worktree's `paths` onto `base_treeish`; returns the new tree id.
///
/// Goes through a temporary index: the real index and the worktree are not touched by a single
/// byte (the real index may be parked in another state right now, and switching the checkout
/// leaves a mess when any step fails). Content goes through `update-index --add` so git reads the
/// worktree file itself — it handles the mode bits and binary content, one conversion fewer than
/// reading into a String and then hash-object, which destroys non-UTF-8 content.
pub fn tree_overlay_worktree(repo: &Repo, base_treeish: &str, paths: &[String]) -> Result<String> {
    if paths.is_empty() {
        return raw_git(
            repo,
            &["rev-parse", &format!("{base_treeish}^{{tree}}")],
            None,
        )
        .map(|s| s.trim().to_string());
    }
    // Via `git rev-parse --git-path`: a linked worktree's `.git` is a file, so `root/.git/...`
    // builds no path there.
    let idx = repo.git_path(&format!("agit-overlay-{}.index", std::process::id()))?;
    let _ = std::fs::remove_file(&idx);
    let result = (|| -> Result<String> {
        let run = |args: &[&str]| -> Result<String> {
            let out = std::process::Command::new("git")
                .arg("--no-replace-objects")
                .args(args)
                .current_dir(repo.root())
                .env("GIT_INDEX_FILE", &idx)
                .output()?;
            if !out.status.success() {
                anyhow::bail!(
                    "git {} failed: {}",
                    args.join(" "),
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            Ok(String::from_utf8_lossy(&out.stdout).to_string())
        };
        run(&["read-tree", base_treeish])?;
        for p in paths {
            if repo.root().join(p).exists() {
                run(&["update-index", "--add", "--", p])?;
            } else {
                // A shared file deleted in the worktree — reconciliation can also conclude that
                // "this memory is void".
                run(&["update-index", "--force-remove", "--", p])?;
            }
        }
        Ok(run(&["write-tree"])?.trim().to_string())
    })();
    let _ = std::fs::remove_file(&idx);
    result
}

/// Build a commit from a tree (one parent, or two for a merge) without touching any ref.
pub fn commit_tree(repo: &Repo, tree: &str, parents: &[&str], message: &str) -> Result<String> {
    // plumbing does not go through Repo::commit and must supply the fallback committer
    // identity itself: a freshly cloned repository that has never committed (no
    // user.name/email) otherwise fails to record a fork commit here.
    repo.ensure_committer()?;
    let mut args: Vec<String> = vec!["commit-tree".into(), tree.into()];
    for p in parents {
        args.push("-p".into());
        args.push((*p).to_string());
    }
    args.push("-m".into());
    args.push(message.into());
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    Ok(raw_git(repo, &arg_refs, None)?.trim().to_string())
}

/// Create the one mechanical storage-migration commit shared by every runtime.
///
/// Unlike ordinary authored commits, this child must be byte-identical when a local CLI and the
/// Hub encounter the same v0 parent independently. Fixed identity and the immutable parent's date
/// are part of the storage protocol, not ambient repository configuration. Reusing the parent date
/// preserves relative branch recency instead of making every migrated session appear equally old.
pub fn storage_migration_commit(repo: &Repo, tree: &str, parent: &str) -> Result<String> {
    let date_output = std::process::Command::new("git")
        .arg("--no-replace-objects")
        .args([
            "-c",
            "i18n.logOutputEncoding=UTF-8",
            "show",
            "-s",
            "--no-show-signature",
            "--format=%cI",
            parent,
        ])
        .current_dir(repo.root())
        .env("GIT_PAGER", "cat")
        .output()?;
    if !date_output.status.success() {
        anyhow::bail!(
            "git show failed while reading migration parent date: {}",
            String::from_utf8_lossy(&date_output.stderr).trim()
        );
    }
    let parent_date = String::from_utf8(date_output.stdout)?.trim().to_owned();
    anyhow::ensure!(
        !parent_date.is_empty() && !parent_date.contains(['\n', '\r']),
        "git returned an invalid migration parent date"
    );
    let output = std::process::Command::new("git")
        .arg("--no-replace-objects")
        .args([
            "-c",
            "i18n.commitEncoding=UTF-8",
            "commit-tree",
            tree,
            "-p",
            parent,
            "-m",
            meta::STORAGE_MIGRATION_MESSAGE,
        ])
        .current_dir(repo.root())
        .env("GIT_AUTHOR_NAME", meta::STORAGE_MIGRATION_NAME)
        .env("GIT_AUTHOR_EMAIL", meta::STORAGE_MIGRATION_EMAIL)
        .env("GIT_AUTHOR_DATE", &parent_date)
        .env("GIT_COMMITTER_NAME", meta::STORAGE_MIGRATION_NAME)
        .env("GIT_COMMITTER_EMAIL", meta::STORAGE_MIGRATION_EMAIL)
        .env("GIT_COMMITTER_DATE", &parent_date)
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "git commit-tree failed for storage migration: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let oid = String::from_utf8(output.stdout)?.trim().to_owned();
    anyhow::ensure!(
        !oid.is_empty() && oid.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "git commit-tree returned an invalid migration commit id"
    );
    Ok(oid)
}

/// Copy one commit and everything reachable from it into another repository's object database.
///
/// A cross-repository merge cannot merely pass the source OID to `commit-tree -p`: Git requires
/// every parent object to exist in the target object database, and the resulting history must stay
/// connected after the source checkout disappears.  Transfer a complete pack directly between the
/// two object databases.  Unlike `git fetch`, this does not write `FETCH_HEAD`, create a temporary
/// ref, touch either index, or change either worktree.
pub fn import_commit_graph(target: &Repo, source: &Repo, commit: &str) -> Result<()> {
    if target.root() == source.root() {
        return Ok(());
    }

    let commit_expr = format!("{commit}^{{commit}}");
    let source_commit = source
        .git(&["rev-parse", "--verify", &commit_expr])?
        .trim()
        .to_string();
    if target
        .git_opt(&[
            "rev-parse",
            "--verify",
            &format!("{source_commit}^{{commit}}"),
        ])
        .is_some()
        && verify_commit_connectivity(target, &source_commit).is_ok()
    {
        return Ok(());
    }

    // `pack-objects --revs` writes a self-contained (non-thin) pack for the complete reachable
    // graph.  Feed it straight to `index-pack --stdin`, which installs the objects but updates no
    // refs.  Disable replace-object resolution on both sides: the imported parent must be the real
    // graph that the source ref names, not a local replacement that would disappear later.
    let mut pack = std::process::Command::new("git")
        .arg("--no-replace-objects")
        .arg("-C")
        .arg(source.root())
        .args(["pack-objects", "--quiet", "--stdout", "--revs"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let pack_stdout = pack
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("git pack-objects has no stdout"))?;
    let mut index = match std::process::Command::new("git")
        .arg("--no-replace-objects")
        .arg("-C")
        .arg(target.root())
        .args(["index-pack", "--stdin"])
        .stdin(std::process::Stdio::from(pack_stdout))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            let _ = pack.kill();
            let _ = pack.wait();
            return Err(error.into());
        }
    };
    let write_result = pack
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("git pack-objects has no stdin"))?
        .write_all(format!("{source_commit}\n").as_bytes());
    if let Err(error) = write_result {
        let _ = pack.kill();
        let _ = index.kill();
        let _ = pack.wait();
        let _ = index.wait();
        return Err(error.into());
    }

    let index_output = index.wait_with_output()?;
    let pack_output = pack.wait_with_output()?;
    if !pack_output.status.success() {
        anyhow::bail!(
            "git pack-objects failed while importing merge parent {source_commit}: {}",
            String::from_utf8_lossy(&pack_output.stderr).trim()
        );
    }
    if !index_output.status.success() {
        anyhow::bail!(
            "git index-pack failed while importing merge parent {source_commit}: {}",
            String::from_utf8_lossy(&index_output.stderr).trim()
        );
    }

    let imported = target
        .git(&[
            "rev-parse",
            "--verify",
            &format!("{source_commit}^{{commit}}"),
        ])?
        .trim()
        .to_string();
    if imported != source_commit {
        anyhow::bail!("imported merge parent resolved to {imported}, expected {source_commit}");
    }
    verify_commit_connectivity(target, &source_commit).with_context(|| {
        format!("imported merge parent {source_commit} does not have a complete reachable graph")
    })?;
    Ok(())
}

fn verify_commit_connectivity(repo: &Repo, commit: &str) -> Result<()> {
    let output = std::process::Command::new("git")
        .arg("--no-replace-objects")
        .arg("-C")
        .arg(repo.root())
        .args(["fsck", "--connectivity-only", "--no-dangling", commit])
        .env("GIT_NO_LAZY_FETCH", "1")
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "git fsck rejected {commit}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Move a ref by CAS: an `old` of `None` means "must not exist yet" (creating a branch).
pub fn update_ref_cas(repo: &Repo, refname: &str, new: &str, old: Option<&str>) -> Result<()> {
    let mut args = vec!["update-ref", refname, new];
    match old {
        Some(o) => args.push(o),
        None => args.push(""), // empty old value = assert the ref does not exist
    }
    let arg_refs: Vec<&str> = args.to_vec();
    raw_git(repo, &arg_refs, None)?;
    Ok(())
}

/// Publish the first commit of the active branch with an expected-absent CAS and a durable,
/// forward-recoverable checkout refresh.
pub fn update_absent_branch_cas_and_refresh(repo: &Repo, branch: &str, new: &str) -> Result<()> {
    let refname = format!("refs/heads/{branch}");
    let transaction = prepare_absent_checkout_transaction(repo, branch, new)?;
    maybe_crash_checkout_at("after_journal");
    if let Err(update_error) = update_ref_cas(repo, &refname, new, None) {
        return match finish_checkout_transaction(transaction) {
            Ok(()) => Err(update_error),
            Err(cleanup_error) => anyhow::bail!(
                "root branch CAS failed ({update_error:#}) and removing its checkout journal also failed ({cleanup_error:#})"
            ),
        };
    }
    maybe_crash_checkout_at("after_ref_cas");

    match refresh_prepared_checkout(repo, &transaction) {
        Ok(()) => finish_checkout_transaction(transaction),
        Err(failure) => anyhow::bail!(
            "root branch {branch} was published at {new}, but checkout refresh failed ({:#}); the checkout journal was retained for startup recovery",
            failure.error
        ),
    }
}

/// CAS a branch and, when it is the active checkout, prove its index/worktree reached the new
/// storage snapshot before reporting success. A failed postflight CAS-rolls the ref back and
/// restores the old owned paths.
pub fn update_branch_cas_and_refresh(
    repo: &Repo,
    branch: &str,
    new: &str,
    old: &str,
    refresh_full_index: bool,
) -> Result<()> {
    let refname = format!("refs/heads/{branch}");
    let active = checked_out_branch(repo)?.as_deref() == Some(branch);
    if !active {
        update_ref_cas(repo, &refname, new, Some(old))?;
        return Ok(());
    }

    // A previous process may have stopped after its CAS. Recover it before any preflight reads
    // the half-refreshed worktree, then create this operation's durable decision before our CAS.
    recover_interrupted_checkout(repo)?;
    if active
        && storage_layout_at(repo, old)? == meta::LayoutVersion::V0
        && storage_layout_at(repo, new)? == meta::LayoutVersion::V1
    {
        ensure_v1_upgrade_preflight(repo, old)?;
    }
    let transaction = prepare_checkout_transaction(repo, branch, old, new, refresh_full_index)?;
    maybe_crash_checkout_at("after_journal");
    if let Err(update_error) = update_ref_cas(repo, &refname, new, Some(old)) {
        return match finish_checkout_transaction(transaction) {
            Ok(()) => Err(update_error),
            Err(cleanup_error) => anyhow::bail!(
                "branch CAS failed ({update_error:#}) and removing its checkout journal also failed ({cleanup_error:#})"
            ),
        };
    }
    maybe_crash_checkout_at("after_ref_cas");

    let postflight = refresh_prepared_checkout(repo, &transaction);
    let Err(postflight_error) = postflight else {
        finish_checkout_transaction(transaction)?;
        return Ok(());
    };

    if let Err(rollback_error) = update_ref_cas(repo, &refname, old, Some(new)) {
        anyhow::bail!(
            "branch {branch} moved to {new}, checkout refresh failed ({:#}), and CAS rollback failed ({rollback_error:#}); the checkout journal was retained for startup recovery",
            postflight_error.error
        );
    }
    if postflight_error.restored() {
        finish_checkout_transaction(transaction)?;
    }
    anyhow::bail!(
        "branch ref was rolled back because checkout refresh failed: {:#}{}",
        postflight_error.error,
        if postflight_error.restored() {
            ""
        } else {
            "; the checkout journal was retained for startup recovery"
        }
    )
}

fn checked_out_branch(repo: &Repo) -> Result<Option<String>> {
    let output = std::process::Command::new("git")
        .arg("--no-replace-objects")
        .arg("-C")
        .arg(repo.root())
        .args(["symbolic-ref", "-q", "--short", "HEAD"])
        .output()?;
    if output.status.success() {
        let branch = String::from_utf8(output.stdout)?.trim().to_owned();
        anyhow::ensure!(
            !branch.is_empty(),
            "git symbolic-ref returned an empty branch"
        );
        return Ok(Some(branch));
    }
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    anyhow::bail!(
        "git symbolic-ref HEAD failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

#[cfg(test)]
mod tests {
    /// This pins that the shared-file overlay's temporary index works in a linked worktree, where
    /// `.git` is a file and `root/.git/<index>` builds no path. Both targets — a session line and
    /// the file line — must pass.
    #[test]
    fn worktree_overlays_work_in_a_linked_worktree() {
        use crate::domain::meta::{self, Meta};
        use crate::domain::repo::Repo;
        let d = tempfile::tempdir().unwrap();
        let base = d.path().canonicalize().unwrap();
        let repo = Repo::init(&base.join("repo")).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        meta::write(repo.root(), &Meta::new_file_line()).unwrap();
        std::fs::create_dir_all(repo.root().join("memory")).unwrap();
        std::fs::write(repo.root().join("memory/team.md"), "v1\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("init").unwrap();
        repo.git(&["branch", "s1", "main"]).unwrap();
        for branch in ["s1", "main"] {
            let dir = base.join(format!("wt-{branch}"));
            let path = dir.to_string_lossy().into_owned();
            let wt = if branch == "main" {
                repo.git(&["worktree", "add", "--quiet", "--detach", &path, "main"])
                    .unwrap();
                Repo::at(&dir)
            } else {
                repo.add_worktree(&dir, branch).unwrap()
            };
            assert!(wt.is_linked_worktree());
            std::fs::write(wt.root().join("memory/team.md"), "reconciled\n").unwrap();
            let head = wt.git(&["rev-parse", "HEAD"]).unwrap();
            let tree =
                super::tree_overlay_worktree(&wt, head.trim(), &["memory/team.md".into()]).unwrap();
            let blob = wt
                .git(&["rev-parse", &format!("{}:memory/team.md", tree.trim())])
                .unwrap();
            let text = wt.git(&["cat-file", "-p", blob.trim()]).unwrap();
            assert_eq!(text.trim_end(), "reconciled");
            let with = super::tree_with(&wt, head.trim(), "AGENTS.md", Some("# x\n")).unwrap();
            assert!(!with.trim().is_empty());
        }
    }

    use super::*;

    fn cat_blob(repo: &Repo, tree: &str, path: &str) -> Vec<u8> {
        let object = format!("{tree}:{path}");
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.root())
            .args(["cat-file", "blob"])
            .arg(&object)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git cat-file {object:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    }

    fn scratch_dirs(repo: &Repo) -> Vec<std::ffi::OsString> {
        let mut names: Vec<_> = std::fs::read_dir(repo.root().join(".git"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().starts_with("agit-tree-"))
            .collect();
        names.sort();
        names
    }

    #[test]
    fn plumbing_commit_without_touching_workdir() {
        let d = tempfile::tempdir().unwrap();
        let repo = Repo::init(&d.path().join("r")).unwrap();
        std::fs::write(repo.root().join("a.txt"), "v1\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("init").unwrap();
        let head = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

        // Tree surgery: modify a.txt, add b.txt; the worktree stays as it is.
        let t = tree_with(&repo, &head, "a.txt", Some("v2\n")).unwrap();
        let t = tree_with_commit_free(&repo, &t, "b.txt", Some("new\n")).unwrap();
        let c = commit_tree(&repo, &t, &[&head], "plumbing").unwrap();
        update_ref_cas(&repo, "refs/heads/other", &c, None).unwrap();

        assert_eq!(repo.show("other", "a.txt").unwrap().trim_end(), "v2");
        assert_eq!(repo.show("other", "b.txt").unwrap().trim_end(), "new");
        // The worktree (main) is unchanged.
        assert_eq!(
            std::fs::read_to_string(repo.root().join("a.txt")).unwrap(),
            "v1\n"
        );
        assert!(!repo.root().join("b.txt").exists());

        // CAS: a branch of that name already exists, so creating it must fail.
        let c2 = commit_tree(&repo, &t, &[&head], "again").unwrap();
        assert!(update_ref_cas(&repo, "refs/heads/other", &c2, None).is_err());
    }

    #[test]
    fn owned_tree_apply_batches_binary_additions_and_deletions_without_checkout() {
        let d = tempfile::tempdir().unwrap();
        let repo = Repo::init(&d.path().join("r")).unwrap();
        let removed = "old\tname\n.bin";
        let original = b"still in the worktree\0\xff";
        std::fs::write(repo.root().join(removed), original).unwrap();
        std::fs::write(repo.root().join("keep.txt"), b"untouched\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("init").unwrap();
        let head = repo.git(&["rev-parse", "HEAD"]).unwrap();

        let event_path = "events/01/23/45/67/0123456789abcdef0123456789abcdef01234567";
        let event_bytes = vec![0, 1, 2, b'\n', b'\r', 0xff, 0xfe, 0];
        // Tabs and newlines prove that update-index is really consuming the `-z`
        // form; a line-delimited or quoted implementation cannot round-trip this path.
        let unusual_path = "events/tab\tand\nnewline.bin";
        let unusual_bytes = vec![0x80, b'\t', 0, b'\n', 0x81];
        let tree = tree_apply_owned(
            &repo,
            &head,
            vec![
                (event_path.to_string(), Some(event_bytes.clone())),
                (unusual_path.to_string(), Some(unusual_bytes.clone())),
                (removed.to_string(), None),
            ],
        )
        .unwrap();

        assert_eq!(cat_blob(&repo, &tree, event_path), event_bytes);
        assert_eq!(cat_blob(&repo, &tree, unusual_path), unusual_bytes);
        let removed_object = format!("{tree}:{removed}");
        let removed_status = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.root())
            .args(["cat-file", "-e"])
            .arg(removed_object)
            .output()
            .unwrap();
        assert!(
            !removed_status.status.success(),
            "the deletion must be present in the tree edit"
        );

        // The helper only edits a temporary index: neither the checkout nor the real
        // index moves, even though the returned tree has both additions and a deletion.
        assert_eq!(std::fs::read(repo.root().join(removed)).unwrap(), original);
        assert!(!repo.root().join(event_path).exists());
        assert!(!repo.root().join(unusual_path).exists());
        assert!(
            repo.git(&["diff", "--cached", "--name-only"])
                .unwrap()
                .is_empty()
        );
        assert!(scratch_dirs(&repo).is_empty());
    }

    #[test]
    fn commit_graph_import_repairs_a_target_that_only_has_the_parent_commit_object() {
        let source_dir = tempfile::tempdir().unwrap();
        let target_dir = tempfile::tempdir().unwrap();
        let source = Repo::init(source_dir.path()).unwrap();
        let target = Repo::init(target_dir.path()).unwrap();
        source.git(&["config", "commit.gpgsign", "false"]).unwrap();
        target.git(&["config", "commit.gpgsign", "false"]).unwrap();
        std::fs::write(source.root().join("one"), "one\n").unwrap();
        source.add_all().unwrap();
        source.commit("one").unwrap();
        std::fs::write(source.root().join("two"), "two\n").unwrap();
        source.add_all().unwrap();
        source.commit("two").unwrap();
        let head = source.git(&["rev-parse", "HEAD"]).unwrap();

        let commit_bytes = source.git_bytes(&["cat-file", "commit", &head]).unwrap();
        let commit_text = String::from_utf8(commit_bytes).unwrap();
        let inserted = raw_git(
            &target,
            &["hash-object", "-t", "commit", "-w", "--stdin"],
            Some(&commit_text),
        )
        .unwrap();
        assert_eq!(inserted.trim(), head);
        assert!(verify_commit_connectivity(&target, &head).is_err());

        import_commit_graph(&target, &source, &head).unwrap();
        verify_commit_connectivity(&target, &head).unwrap();
    }

    #[test]
    fn owned_tree_apply_validates_paths_and_cleans_scratch_after_git_errors() {
        let d = tempfile::tempdir().unwrap();
        let repo = Repo::init(&d.path().join("r")).unwrap();
        std::fs::write(repo.root().join("base.txt"), b"base\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("init").unwrap();
        let head = repo.git(&["rev-parse", "HEAD"]).unwrap();

        for invalid in ["", "/absolute", "trailing/", "a//b", "a/./b", "a/../b"] {
            let err = tree_apply_owned(&repo, &head, vec![(invalid.to_string(), Some(vec![1]))])
                .unwrap_err();
            assert!(
                err.to_string().contains("tree edit path"),
                "unexpected error for {invalid:?}: {err:#}"
            );
        }
        let err = tree_apply_owned(
            &repo,
            &head,
            vec![("events/bad\0path".to_string(), Some(vec![1]))],
        )
        .unwrap_err();
        assert!(err.to_string().contains("NUL"));
        assert!(scratch_dirs(&repo).is_empty());

        // A file and its child cannot coexist in one tree. Reject it before Git silently
        // lets the later index record replace the earlier one.
        let err = tree_apply_owned(
            &repo,
            &head,
            vec![
                ("collision".to_string(), Some(vec![1])),
                ("collision/child".to_string(), Some(vec![2])),
            ],
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("tree edit path collision"),
            "unexpected collision failure: {err:#}"
        );
        assert!(scratch_dirs(&repo).is_empty());
    }

    /// This pins that worktree edits that never went through `git add` still land in the tree —
    /// a merge agent reconciles shared files by editing the worktree directly, and without
    /// collecting them at settlement not one byte of that work reaches a commit.
    #[test]
    fn worktree_edits_can_be_folded_into_a_tree() {
        let d = tempfile::tempdir().unwrap();
        let repo = Repo::init(&d.path().join("r")).unwrap();
        std::fs::create_dir_all(repo.root().join("memory")).unwrap();
        std::fs::write(repo.root().join("memory/team.md"), "v1\n").unwrap();
        std::fs::write(repo.root().join("keep.txt"), "same\n").unwrap();
        std::fs::write(repo.root().join("session"), "").unwrap();
        std::fs::remove_file(repo.root().join("session")).unwrap();
        repo.add_all().unwrap();
        repo.commit("init").unwrap();
        let head = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

        // One modified, one added, one deleted in the worktree, with no `git add` at all.
        std::fs::write(repo.root().join("memory/team.md"), "v2\n").unwrap();
        std::fs::write(repo.root().join("memory/new.md"), "fresh\n").unwrap();
        std::fs::remove_file(repo.root().join("keep.txt")).unwrap();

        let changed = worktree_changes(&repo, &head, &[]).unwrap();
        assert_eq!(changed, vec!["keep.txt", "memory/new.md", "memory/team.md"]);

        let tree = tree_overlay_worktree(&repo, &head, &changed).unwrap();
        let c = commit_tree(&repo, &tree, &[&head], "overlay").unwrap();
        assert_eq!(repo.show(&c, "memory/team.md").unwrap().trim_end(), "v2");
        assert_eq!(repo.show(&c, "memory/new.md").unwrap().trim_end(), "fresh");
        assert!(
            repo.show(&c, "keep.txt").is_none(),
            "a file deleted in the worktree disappears from the tree"
        );
        // The real index is untouched: the content of the initial commit is still staged.
        assert!(
            repo.git(&["diff", "--cached", "--name-only"])
                .unwrap()
                .trim()
                .is_empty()
        );
    }

    /// This pins that paths on the exclusion list stay out of the tree — merge's own surgery owns
    /// the session storage files, and the worktree copy holds stale bytes from the last checkout
    /// that would overwrite the merge result.
    #[test]
    fn the_overlay_honours_the_exclusion_list() {
        let d = tempfile::tempdir().unwrap();
        let repo = Repo::init(&d.path().join("r")).unwrap();
        std::fs::create_dir_all(repo.root().join("session")).unwrap();
        std::fs::write(repo.root().join("session/VIEW"), "old\n").unwrap();
        std::fs::write(repo.root().join("notes.md"), "n1\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("init").unwrap();
        let head = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

        std::fs::write(repo.root().join("session/VIEW"), "stale\n").unwrap();
        std::fs::write(repo.root().join("notes.md"), "n2\n").unwrap();
        let changed = worktree_changes(&repo, &head, &["session/VIEW"]).unwrap();
        assert_eq!(changed, vec!["notes.md"]);
    }

    #[test]
    fn worktree_change_exclusions_cover_directories_but_not_similar_prefixes() {
        let d = tempfile::tempdir().unwrap();
        let repo = Repo::init(&d.path().join("r")).unwrap();
        std::fs::create_dir_all(repo.root().join("events/nested")).unwrap();
        std::fs::write(repo.root().join("events/existing"), "old\n").unwrap();
        std::fs::write(repo.root().join("events-adjacent"), "old\n").unwrap();
        std::fs::write(repo.root().join("exact.txt"), "old\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("init").unwrap();
        let head = repo.git(&["rev-parse", "HEAD"]).unwrap();

        std::fs::write(repo.root().join("events/existing"), "new\n").unwrap();
        std::fs::write(repo.root().join("events/nested/new"), "new\n").unwrap();
        std::fs::write(repo.root().join("events-adjacent"), "new\n").unwrap();
        std::fs::write(repo.root().join("exact.txt"), "new\n").unwrap();

        let changed = worktree_changes(&repo, &head, &["events", "exact.txt"]).unwrap();
        assert_eq!(changed, vec!["events-adjacent"]);
    }

    #[test]
    fn active_branch_full_refresh_converges_modify_add_and_delete() {
        let d = tempfile::tempdir().unwrap();
        let repo = Repo::init(d.path()).unwrap();
        std::fs::write(repo.root().join("modified.md"), "old\n").unwrap();
        std::fs::write(repo.root().join("deleted.md"), "delete me\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("old").unwrap();
        let old = repo.git(&["rev-parse", "HEAD"]).unwrap();

        let tree = tree_apply_owned(
            &repo,
            &old,
            vec![
                ("modified.md".into(), Some(b"new\n".to_vec())),
                ("added.md".into(), Some(b"added\n".to_vec())),
                ("deleted.md".into(), None),
            ],
        )
        .unwrap();
        let new = commit_tree(&repo, &tree, &[&old], "new").unwrap();

        // The merge agent has already reconciled these exact bytes in its worktree. The ref
        // settlement must both preserve them and align the real index, including deletion.
        std::fs::write(repo.root().join("modified.md"), "new\n").unwrap();
        std::fs::write(repo.root().join("added.md"), "added\n").unwrap();
        std::fs::remove_file(repo.root().join("deleted.md")).unwrap();
        update_branch_cas_and_refresh(&repo, "main", &new, &old, true).unwrap();

        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), new);
        assert_eq!(
            std::fs::read_to_string(repo.root().join("modified.md")).unwrap(),
            "new\n"
        );
        assert_eq!(
            std::fs::read_to_string(repo.root().join("added.md")).unwrap(),
            "added\n"
        );
        assert!(!repo.root().join("deleted.md").exists());
        assert!(repo.git(&["status", "--porcelain"]).unwrap().is_empty());
    }

    #[test]
    fn active_branch_full_refresh_handles_utf8_path_with_quote_path_enabled() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repo::init(directory.path()).unwrap();
        repo.git(&["config", "core.quotePath", "true"]).unwrap();
        std::fs::create_dir_all(repo.root().join("memory")).unwrap();
        // CJK path fixture: with `core.quotePath` enabled git quotes this path, which an ASCII
        // fixture never exercises.
        std::fs::write(repo.root().join("memory/中文.md"), "old\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("old").unwrap();
        let old = repo.git(&["rev-parse", "HEAD"]).unwrap();

        let tree = tree_apply_owned(
            &repo,
            &old,
            vec![("memory/中文.md".into(), Some("new\n".as_bytes().to_vec()))],
        )
        .unwrap();
        let new = commit_tree(&repo, &tree, &[&old], "new").unwrap();

        update_branch_cas_and_refresh(&repo, "main", &new, &old, true).unwrap();

        assert_eq!(
            std::fs::read_to_string(repo.root().join("memory/中文.md")).unwrap(),
            "new\n"
        );
        assert!(repo.git(&["status", "--porcelain"]).unwrap().is_empty());
    }

    fn checkout_processes_for_event_additions(event_count: usize) -> usize {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repo::init(directory.path()).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        let snapshot = meta::Meta::new_file_line();
        meta::write(repo.root(), &snapshot).unwrap();
        repo.add_all().unwrap();
        repo.commit("old").unwrap();
        let old = repo.git(&["rev-parse", "HEAD"]).unwrap();

        let edits = (0..event_count)
            .map(|index| {
                let id = format!("{index:040x}");
                (
                    meta::event_path(&id).unwrap(),
                    Some(format!("event {index}\n").into_bytes()),
                )
            })
            .collect();
        let tree = tree_apply_owned(&repo, &old, edits).unwrap();
        let new = commit_tree(&repo, &tree, &[&old], "events").unwrap();

        reset_checkout_git_process_count();
        update_branch_cas_and_refresh(&repo, "main", &new, &old, false).unwrap();
        let processes = checkout_git_process_count();
        assert!(repo.git(&["status", "--porcelain"]).unwrap().is_empty());
        let last_id = format!("{:040x}", event_count - 1);
        assert_eq!(
            std::fs::read_to_string(repo.root().join(meta::event_path(&last_id).unwrap())).unwrap(),
            format!("event {}\n", event_count - 1)
        );
        processes
    }

    #[test]
    fn checkout_git_processes_do_not_scale_with_event_count() {
        let small = checkout_processes_for_event_additions(2);
        let large = checkout_processes_for_event_additions(512);
        assert!(
            large <= small + 1,
            "checkout spawned {small} Git processes for 2 events but {large} for 512"
        );
    }

    #[test]
    fn failed_active_branch_refresh_restores_ref_index_and_exact_worktree() {
        let d = tempfile::tempdir().unwrap();
        let repo = Repo::init(d.path()).unwrap();
        std::fs::write(repo.root().join("modified.md"), "old\n").unwrap();
        std::fs::write(repo.root().join("deleted.md"), "delete me\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("old").unwrap();
        let old = repo.git(&["rev-parse", "HEAD"]).unwrap();
        let tree = tree_apply_owned(
            &repo,
            &old,
            vec![
                ("modified.md".into(), Some(b"new\n".to_vec())),
                ("added.md".into(), Some(b"added\n".to_vec())),
                ("deleted.md".into(), None),
            ],
        )
        .unwrap();
        let new = commit_tree(&repo, &tree, &[&old], "new").unwrap();
        std::fs::write(repo.root().join("modified.md"), "new\n").unwrap();
        std::fs::write(repo.root().join("added.md"), "added\n").unwrap();
        std::fs::remove_file(repo.root().join("deleted.md")).unwrap();
        let before_status = repo
            .git(&["status", "--porcelain", "--untracked-files=all"])
            .unwrap();

        FAIL_CHECKOUT_AFTER_APPLY.with(|fail| fail.set(true));
        let error = update_branch_cas_and_refresh(&repo, "main", &new, &old, true).unwrap_err();
        assert!(error.to_string().contains("rolled back"), "{error:#}");
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), old);
        assert_eq!(
            std::fs::read_to_string(repo.root().join("modified.md")).unwrap(),
            "new\n"
        );
        assert_eq!(
            std::fs::read_to_string(repo.root().join("added.md")).unwrap(),
            "added\n"
        );
        assert!(!repo.root().join("deleted.md").exists());
        assert_eq!(
            repo.git(&["status", "--porcelain", "--untracked-files=all"])
                .unwrap(),
            before_status
        );
        assert!(
            repo.git(&["diff", "--cached", "--name-only"])
                .unwrap()
                .is_empty()
        );
        assert!(
            !checkout_git_path(&repo, CHECKOUT_JOURNAL_NAME)
                .unwrap()
                .exists(),
            "a caught failure with exact rollback must remove its durable journal"
        );
    }

    fn checkout_crash_fixture(root: &std::path::Path) -> (Repo, String, String) {
        let repo = Repo::init(root).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        std::fs::write(repo.root().join("a.txt"), "old a\n").unwrap();
        std::fs::write(repo.root().join("b.txt"), "old b\n").unwrap();
        std::fs::write(repo.root().join("outside.txt"), "old outside\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("old").unwrap();
        let old = repo.git(&["rev-parse", "HEAD"]).unwrap();
        let tree = tree_apply_owned(
            &repo,
            &old,
            vec![
                ("a.txt".into(), Some(b"new a\n".to_vec())),
                ("b.txt".into(), Some(b"new b\n".to_vec())),
            ],
        )
        .unwrap();
        let new = commit_tree(&repo, &tree, &[&old], "new").unwrap();
        (repo, old, new)
    }

    fn root_checkout_crash_fixture(root: &std::path::Path) -> (Repo, String, String) {
        let repo = Repo::init(root).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();

        std::fs::write(repo.root().join("memory.md"), "staged memory\n").unwrap();
        std::fs::write(repo.root().join(meta::ATTRS_FILE), "*.staged binary\n").unwrap();
        repo.add_all().unwrap();
        let index_path = checkout_git_path(&repo, "index").unwrap();
        let staged_index = std::fs::read(&index_path).unwrap();

        std::fs::write(repo.root().join("memory.md"), "worktree memory\n").unwrap();
        let worktree_attributes = "*.worktree binary\n";
        std::fs::write(repo.root().join(meta::ATTRS_FILE), worktree_attributes).unwrap();

        // Reproduce the first-settlement tree's historical `git add -A` worktree-wins behavior,
        // then put the real staged layer back before the expected-absent publication begins.
        repo.add_all().unwrap();
        let base_tree = repo.git(&["write-tree"]).unwrap();
        std::fs::write(&index_path, staged_index).unwrap();

        let mut snapshot = meta::Meta::new(
            "agit-0123456789abcdef0123456789abcdef01234567".into(),
            "codex".into(),
            ".".into(),
        );
        snapshot.turn = Some(1);
        let mut edits: Vec<(String, Option<Vec<u8>>)> = storage::snapshot_files("", "")
            .unwrap()
            .into_iter()
            .map(|(path, bytes)| (path, Some(bytes)))
            .collect();
        edits.push((
            meta::FILE.into(),
            Some(meta::to_text(&snapshot).unwrap().into_bytes()),
        ));
        let canonical_attributes =
            storage::attributes_text_strict(Some(worktree_attributes)).unwrap();
        edits.push((
            meta::ATTRS_FILE.into(),
            Some(canonical_attributes.as_bytes().to_vec()),
        ));
        let tree = tree_apply_owned(&repo, &base_tree, edits).unwrap();
        let new = commit_tree(&repo, &tree, &[], "root").unwrap();
        assert!(
            optional_ref_commit(&repo, "refs/heads/main")
                .unwrap()
                .is_none()
        );
        (repo, new, canonical_attributes)
    }

    fn attributes_upgrade_crash_fixture(
        root: &std::path::Path,
    ) -> (Repo, String, String, String, String) {
        let repo = Repo::init(root).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        let mut old_meta = meta::Meta::new_file_line();
        old_meta.layout = meta::LayoutVersion::V0;
        meta::write(repo.root(), &old_meta).unwrap();
        let committed = "*.committed binary\n";
        std::fs::write(repo.root().join(meta::ATTRS_FILE), committed).unwrap();
        repo.add_all().unwrap();
        repo.commit("v0").unwrap();
        let old = repo.git(&["rev-parse", "HEAD"]).unwrap();

        let new_meta = meta::to_text(&meta::Meta::new_file_line()).unwrap();
        let tree = session_snapshot_tree(&repo, &old, "", "", &new_meta).unwrap();
        let new = commit_tree(&repo, &tree, &[&old], "v1").unwrap();

        let staged = "*.staged binary\n".to_owned();
        std::fs::write(repo.root().join(meta::ATTRS_FILE), &staged).unwrap();
        repo.git(&["add", "--", meta::ATTRS_FILE]).unwrap();
        let worktree = "*.worktree binary\n".to_owned();
        std::fs::write(repo.root().join(meta::ATTRS_FILE), &worktree).unwrap();
        (repo, old, new, staged, worktree)
    }

    fn large_attributes(prefix: &str, minimum_bytes: usize) -> String {
        use std::fmt::Write as _;

        let mut attributes = String::new();
        let mut index = 0usize;
        while attributes.len() <= minimum_bytes {
            writeln!(&mut attributes, "{prefix}/path-{index:06} -text").unwrap();
            index += 1;
        }
        attributes
    }

    fn install_large_dirty_attributes(repo: &Repo) -> (String, String) {
        let staged = large_attributes("staged", 64 * 1024);
        std::fs::write(repo.root().join(meta::ATTRS_FILE), &staged).unwrap();
        repo.git(&["add", "--", meta::ATTRS_FILE]).unwrap();
        let worktree = large_attributes("worktree", 96 * 1024);
        std::fs::write(repo.root().join(meta::ATTRS_FILE), &worktree).unwrap();
        (staged, worktree)
    }

    fn crash_checkout_subprocess(
        repo: &Repo,
        old: &str,
        new: &str,
        point: &str,
        refresh_full_index: bool,
    ) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "commands::plumbing::tests::checkout_transaction_crash_child",
                "--nocapture",
            ])
            .env("AGIT_TEST_CHECKOUT_REPO", repo.root())
            .env("AGIT_TEST_CHECKOUT_OLD", old)
            .env("AGIT_TEST_CHECKOUT_NEW", new)
            .env("AGIT_TEST_CHECKOUT_CRASH_AT", point)
            .env(
                "AGIT_TEST_CHECKOUT_FULL_INDEX",
                if refresh_full_index { "1" } else { "0" },
            )
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(86),
            "child did not stop at {point}: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn crash_root_checkout_subprocess(repo: &Repo, new: &str, point: &str) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "commands::plumbing::tests::checkout_transaction_crash_child",
                "--nocapture",
            ])
            .env("AGIT_TEST_CHECKOUT_REPO", repo.root())
            .env("AGIT_TEST_CHECKOUT_NEW", new)
            .env("AGIT_TEST_CHECKOUT_ROOT", "1")
            .env("AGIT_TEST_CHECKOUT_CRASH_AT", point)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(86),
            "root child did not stop at {point}: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn restart_checkout_recovery_subprocess(repo: &Repo) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "commands::plumbing::tests::checkout_transaction_restart_recovery_child",
                "--nocapture",
            ])
            .env("AGIT_TEST_CHECKOUT_RECOVERY_REPO", repo.root())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "restart recovery failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// This test is normally a no-op. The parent test re-execs the real libtest process with a
    /// failpoint so `process::exit` models SIGKILL semantics: no Rust destructor or rollback runs.
    #[test]
    fn checkout_transaction_crash_child() {
        let Some(root) = std::env::var_os("AGIT_TEST_CHECKOUT_REPO") else {
            return;
        };
        let new = std::env::var("AGIT_TEST_CHECKOUT_NEW").unwrap();
        let root_checkout = std::env::var("AGIT_TEST_CHECKOUT_ROOT").as_deref() == Ok("1");
        let refresh_full_index =
            std::env::var("AGIT_TEST_CHECKOUT_FULL_INDEX").as_deref() == Ok("1");
        let repo = Repo::open(std::path::PathBuf::from(root)).unwrap();
        let result = if root_checkout {
            update_absent_branch_cas_and_refresh(&repo, "main", &new)
        } else {
            let old = std::env::var("AGIT_TEST_CHECKOUT_OLD").unwrap();
            update_branch_cas_and_refresh(&repo, "main", &new, &old, refresh_full_index)
        };
        panic!("checkout failpoint did not terminate the process: {result:?}");
    }

    /// Re-executed by publication-failure tests so recovery opens the repository in a fresh
    /// process, after the failed writer has dropped its transaction lock and all in-memory state.
    #[test]
    fn checkout_transaction_restart_recovery_child() {
        let Some(root) = std::env::var_os("AGIT_TEST_CHECKOUT_RECOVERY_REPO") else {
            return;
        };
        let repo = Repo::open(std::path::PathBuf::from(root)).unwrap();
        assert!(recover_interrupted_checkout(&repo).unwrap());
    }

    #[test]
    fn startup_discards_root_intent_when_process_exits_before_absent_cas() {
        let directory = tempfile::tempdir().unwrap();
        let (repo, new, _) = root_checkout_crash_fixture(directory.path());
        let original_index_tree = repo.git(&["write-tree"]).unwrap();

        crash_root_checkout_subprocess(&repo, &new, "after_journal");

        assert!(
            optional_ref_commit(&repo, "refs/heads/main")
                .unwrap()
                .is_none()
        );
        assert!(
            checkout_git_path(&repo, CHECKOUT_JOURNAL_NAME)
                .unwrap()
                .is_file()
        );
        assert!(
            checkout_git_path(&repo, CHECKOUT_ATTRIBUTES_SIDECAR_NAME)
                .unwrap()
                .is_file()
        );
        assert!(recover_interrupted_checkout(&repo).unwrap());
        assert!(
            optional_ref_commit(&repo, "refs/heads/main")
                .unwrap()
                .is_none()
        );
        assert_eq!(repo.git(&["write-tree"]).unwrap(), original_index_tree);
        assert!(!repo.root().join(meta::LOG_FILE).exists());
        assert!(!repo.root().join(meta::VIEW_FILE).exists());
        assert!(
            !checkout_git_path(&repo, CHECKOUT_JOURNAL_NAME)
                .unwrap()
                .exists()
        );
        assert!(
            !checkout_git_path(&repo, CHECKOUT_ATTRIBUTES_SIDECAR_NAME)
                .unwrap()
                .exists()
        );
        assert!(!recover_interrupted_checkout(&repo).unwrap());
    }

    #[test]
    fn startup_recovers_root_process_exit_after_cas_and_mid_refresh() {
        for point in ["after_ref_cas", "during_apply"] {
            let directory = tempfile::tempdir().unwrap();
            let (repo, new, canonical_attributes) = root_checkout_crash_fixture(directory.path());
            let original_index_tree = repo.git(&["write-tree"]).unwrap();

            crash_root_checkout_subprocess(&repo, &new, point);

            assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), new);
            assert_eq!(repo.git(&["rev-list", "--count", "HEAD"]).unwrap(), "1");
            assert_ne!(
                repo.git(&["write-tree"]).unwrap(),
                repo.git(&["rev-parse", "HEAD^{tree}"]).unwrap(),
                "the hard stop must happen before the real index is refreshed"
            );
            assert_eq!(repo.git(&["write-tree"]).unwrap(), original_index_tree);
            if point == "after_ref_cas" {
                assert!(!repo.root().join(meta::LOG_FILE).exists());
            } else {
                assert!(repo.root().join(meta::LOG_FILE).is_file());
                assert!(repo.root().join(meta::VIEW_FILE).is_file());
                assert!(!repo.root().join(meta::FILE).exists());
            }

            let journal_path = checkout_git_path(&repo, CHECKOUT_JOURNAL_NAME).unwrap();
            let journal = read_checkout_journal(&journal_path).unwrap().unwrap();
            assert_eq!(journal.old, None);
            assert_eq!(journal.new, new);
            assert_eq!(journal.scope, CheckoutScope::Root);
            assert!(
                checkout_git_path(&repo, CHECKOUT_ATTRIBUTES_SIDECAR_NAME)
                    .unwrap()
                    .is_file()
            );

            assert_eq!(crate::commands::migration::migrate_repo(&repo).unwrap(), 0);
            assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), new);
            assert_eq!(
                repo.git(&["write-tree"]).unwrap(),
                repo.git(&["rev-parse", "HEAD^{tree}"]).unwrap()
            );
            assert_eq!(
                std::fs::read_to_string(repo.root().join("memory.md")).unwrap(),
                "worktree memory\n"
            );
            assert_eq!(
                std::fs::read_to_string(repo.root().join(meta::ATTRS_FILE)).unwrap(),
                canonical_attributes
            );
            assert_eq!(
                storage::materialize_worktree(repo.root(), meta::LOG_FILE).unwrap(),
                storage::materialize_at(repo.root(), "HEAD", meta::LOG_FILE).unwrap()
            );
            assert_eq!(
                storage::materialize_worktree(repo.root(), meta::VIEW_FILE).unwrap(),
                storage::materialize_at(repo.root(), "HEAD", meta::VIEW_FILE).unwrap()
            );
            assert_eq!(
                std::fs::read_to_string(repo.root().join(meta::FILE)).unwrap(),
                repo.show_raw_result("HEAD", meta::FILE).unwrap().unwrap()
            );
            assert!(repo.git(&["status", "--porcelain"]).unwrap().is_empty());
            assert!(!journal_path.exists());
            assert!(
                !checkout_git_path(&repo, CHECKOUT_ATTRIBUTES_SIDECAR_NAME)
                    .unwrap()
                    .exists()
            );

            // Recovery is a one-shot durable decision; a second startup is a clean no-op.
            assert_eq!(crate::commands::migration::migrate_repo(&repo).unwrap(), 0);
            assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), new);
            assert!(repo.git(&["status", "--porcelain"]).unwrap().is_empty());
        }
    }

    #[test]
    fn startup_recovers_process_exit_after_cas_and_during_checkout_apply() {
        for point in ["after_journal", "after_ref_cas", "during_apply"] {
            let directory = tempfile::tempdir().unwrap();
            let (repo, old, new) = checkout_crash_fixture(directory.path());
            crash_checkout_subprocess(&repo, &old, &new, point, true);

            let current = repo.git(&["rev-parse", "HEAD"]).unwrap();
            let expected = if point == "after_journal" { &old } else { &new };
            assert_eq!(current, *expected);
            assert!(
                checkout_git_path(&repo, CHECKOUT_JOURNAL_NAME)
                    .unwrap()
                    .is_file()
            );
            let journal =
                read_checkout_journal(&checkout_git_path(&repo, CHECKOUT_JOURNAL_NAME).unwrap())
                    .unwrap()
                    .unwrap();
            assert_eq!(journal.version, CHECKOUT_JOURNAL_VERSION);
            assert_eq!(journal.branch, "main");
            assert_eq!(journal.old.as_deref(), Some(old.as_str()));
            assert_eq!(journal.new, new);
            assert_eq!(journal.scope, CheckoutScope::Full);

            let a = std::fs::read_to_string(repo.root().join("a.txt")).unwrap();
            let b = std::fs::read_to_string(repo.root().join("b.txt")).unwrap();
            if point == "during_apply" {
                assert!(
                    (a == "new a\n" && b == "old b\n") || (a == "old a\n" && b == "new b\n"),
                    "mid-apply fixture must contain one old and one new worktree path: a={a:?}, b={b:?}"
                );
            }

            assert_eq!(
                crate::commands::migration::migrate_repo(&repo).unwrap(),
                0,
                "startup migration must recover the journal before scanning layouts"
            );
            assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), *expected);
            let target_a = if point == "after_journal" {
                "old a\n"
            } else {
                "new a\n"
            };
            let target_b = if point == "after_journal" {
                "old b\n"
            } else {
                "new b\n"
            };
            assert_eq!(
                std::fs::read_to_string(repo.root().join("a.txt")).unwrap(),
                target_a
            );
            assert_eq!(
                std::fs::read_to_string(repo.root().join("b.txt")).unwrap(),
                target_b
            );
            assert!(repo.git(&["status", "--porcelain"]).unwrap().is_empty());
            assert!(
                !checkout_git_path(&repo, CHECKOUT_JOURNAL_NAME)
                    .unwrap()
                    .exists()
            );
            assert!(!recover_interrupted_checkout(&repo).unwrap());
        }
    }

    #[test]
    fn large_legacy_attribute_layers_upgrade_without_using_the_bounded_journal_body() {
        let directory = tempfile::tempdir().unwrap();
        let (repo, old, new, _, _) = attributes_upgrade_crash_fixture(directory.path());
        let (staged, worktree) = install_large_dirty_attributes(&repo);
        assert!(staged.len() + worktree.len() > MAX_CHECKOUT_JOURNAL_BYTES as usize);

        update_branch_cas_and_refresh(&repo, "main", &new, &old, false).unwrap();

        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), new);
        assert_eq!(
            repo.git_bytes_result(&["show", &format!(":{}", meta::ATTRS_FILE)])
                .unwrap(),
            storage::attributes_text_strict(Some(&staged))
                .unwrap()
                .into_bytes()
        );
        assert_eq!(
            std::fs::read(repo.root().join(meta::ATTRS_FILE)).unwrap(),
            storage::attributes_text_strict(Some(&worktree))
                .unwrap()
                .into_bytes()
        );
        assert!(
            !checkout_git_path(&repo, CHECKOUT_JOURNAL_NAME)
                .unwrap()
                .exists()
        );
        assert!(
            !checkout_git_path(&repo, CHECKOUT_ATTRIBUTES_SIDECAR_NAME)
                .unwrap()
                .exists()
        );
    }

    #[test]
    fn large_attribute_sidecar_survives_gc_after_cas_and_mid_apply_then_is_cleaned() {
        for point in ["after_ref_cas", "during_attributes"] {
            let directory = tempfile::tempdir().unwrap();
            let (repo, old, new, _, _) = attributes_upgrade_crash_fixture(directory.path());
            let (staged, worktree) = install_large_dirty_attributes(&repo);
            let staged_oid = repo
                .git(&["rev-parse", &format!(":{}", meta::ATTRS_FILE)])
                .unwrap();

            crash_checkout_subprocess(&repo, &old, &new, point, false);

            assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), new);
            let journal_path = checkout_git_path(&repo, CHECKOUT_JOURNAL_NAME).unwrap();
            let sidecar_path = checkout_git_path(&repo, CHECKOUT_ATTRIBUTES_SIDECAR_NAME).unwrap();
            let journal_bytes = std::fs::read(&journal_path).unwrap();
            assert!(journal_bytes.len() <= MAX_CHECKOUT_JOURNAL_BYTES as usize);
            assert!(
                !journal_bytes
                    .windows(b"staged/path-000000".len())
                    .any(|window| window == b"staged/path-000000"),
                "raw user attributes must not leak into the bounded JSON journal"
            );
            let sidecar_size = std::fs::metadata(&sidecar_path).unwrap().len();
            assert_eq!(sidecar_size, (staged.len() + worktree.len()) as u64);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                assert_eq!(
                    std::fs::metadata(&sidecar_path)
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o077,
                    0,
                    "the recovery snapshot contains user bytes and must remain owner-only"
                );
            }

            repo.git(&["gc", "--prune=now"]).unwrap();
            assert_eq!(
                std::fs::metadata(&sidecar_path).unwrap().len(),
                sidecar_size
            );
            if point == "during_attributes" {
                assert!(
                    repo.git_opt(&["cat-file", "-e", &staged_oid]).is_none(),
                    "mid-attributes recovery must not accidentally rely on the pruned staged blob"
                );
            }

            assert!(recover_interrupted_checkout(&repo).unwrap());
            assert_eq!(
                repo.git_bytes_result(&["show", &format!(":{}", meta::ATTRS_FILE)])
                    .unwrap(),
                storage::attributes_text_strict(Some(&staged))
                    .unwrap()
                    .into_bytes()
            );
            assert_eq!(
                std::fs::read(repo.root().join(meta::ATTRS_FILE)).unwrap(),
                storage::attributes_text_strict(Some(&worktree))
                    .unwrap()
                    .into_bytes()
            );
            assert!(!journal_path.exists());
            assert!(!sidecar_path.exists());
            assert!(!recover_interrupted_checkout(&repo).unwrap());
        }
    }

    #[test]
    fn startup_removes_a_sidecar_left_before_the_journal_was_published() {
        let directory = tempfile::tempdir().unwrap();
        let (repo, old, new, staged, worktree) = attributes_upgrade_crash_fixture(directory.path());

        crash_checkout_subprocess(&repo, &old, &new, "after_attributes_sidecar", false);

        let journal_path = checkout_git_path(&repo, CHECKOUT_JOURNAL_NAME).unwrap();
        let sidecar_path = checkout_git_path(&repo, CHECKOUT_ATTRIBUTES_SIDECAR_NAME).unwrap();
        assert!(!journal_path.exists());
        assert!(sidecar_path.is_file());
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), old);
        assert!(!recover_interrupted_checkout(&repo).unwrap());
        assert!(!sidecar_path.exists());
        assert_eq!(
            repo.git_bytes_result(&["show", &format!(":{}", meta::ATTRS_FILE)])
                .unwrap(),
            staged.as_bytes()
        );
        assert_eq!(
            std::fs::read(repo.root().join(meta::ATTRS_FILE)).unwrap(),
            worktree.as_bytes()
        );
    }

    #[test]
    fn journal_directory_sync_failure_removes_journal_and_sidecar_before_restart() {
        let directory = tempfile::tempdir().unwrap();
        let (repo, old, new, staged, worktree) = attributes_upgrade_crash_fixture(directory.path());

        fail_next_checkout_journal_directory_sync();
        let error = update_branch_cas_and_refresh(&repo, "main", &new, &old, false).unwrap_err();
        assert!(
            format!("{error:#}").contains("injected checkout journal directory sync failure"),
            "{error:#}"
        );

        let journal_path = checkout_git_path(&repo, CHECKOUT_JOURNAL_NAME).unwrap();
        let sidecar_path = checkout_git_path(&repo, CHECKOUT_ATTRIBUTES_SIDECAR_NAME).unwrap();
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), old);
        assert!(!journal_path.exists());
        assert!(!sidecar_path.exists());
        assert!(
            !recover_interrupted_checkout(&repo).unwrap(),
            "a clean restart must not discover a half-published durable decision"
        );
        assert_eq!(
            repo.git_bytes_result(&["show", &format!(":{}", meta::ATTRS_FILE)])
                .unwrap(),
            staged.as_bytes()
        );
        assert_eq!(
            std::fs::read(repo.root().join(meta::ATTRS_FILE)).unwrap(),
            worktree.as_bytes()
        );
    }

    #[test]
    fn journal_unlink_failure_retains_complete_old_side_recovery_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let (repo, old, new, staged, worktree) = attributes_upgrade_crash_fixture(directory.path());

        fail_next_checkout_journal_directory_sync();
        fail_next_checkout_journal_unlink();
        let error = update_branch_cas_and_refresh(&repo, "main", &new, &old, false).unwrap_err();
        let error = format!("{error:#}");
        assert!(
            error.contains("injected checkout journal directory sync failure"),
            "{error}"
        );
        assert!(
            error.contains("injected checkout journal unlink failure"),
            "{error}"
        );

        let journal_path = checkout_git_path(&repo, CHECKOUT_JOURNAL_NAME).unwrap();
        let sidecar_path = checkout_git_path(&repo, CHECKOUT_ATTRIBUTES_SIDECAR_NAME).unwrap();
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), old);
        assert!(journal_path.is_file());
        assert!(sidecar_path.is_file());
        let journal = read_checkout_journal(&journal_path).unwrap().unwrap();
        assert!(
            validate_checkout_journal(&repo, &journal)
                .unwrap()
                .is_some()
        );

        restart_checkout_recovery_subprocess(&repo);

        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), old);
        assert!(!journal_path.exists());
        assert!(!sidecar_path.exists());
        assert_eq!(
            repo.git_bytes_result(&["show", &format!(":{}", meta::ATTRS_FILE)])
                .unwrap(),
            staged.as_bytes()
        );
        assert_eq!(
            std::fs::read(repo.root().join(meta::ATTRS_FILE)).unwrap(),
            worktree.as_bytes()
        );
    }

    #[test]
    fn journal_removal_sync_failure_restores_complete_old_side_recovery_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let (repo, old, new, staged, worktree) = attributes_upgrade_crash_fixture(directory.path());

        fail_next_checkout_journal_directory_sync();
        fail_next_checkout_journal_removal_directory_sync();
        let error = update_branch_cas_and_refresh(&repo, "main", &new, &old, false).unwrap_err();
        let error = format!("{error:#}");
        assert!(
            error.contains("injected checkout journal directory sync failure"),
            "{error}"
        );
        assert!(
            error.contains("injected checkout journal removal directory sync failure"),
            "{error}"
        );
        assert!(error.contains("restored for startup recovery"), "{error}");

        let journal_path = checkout_git_path(&repo, CHECKOUT_JOURNAL_NAME).unwrap();
        let sidecar_path = checkout_git_path(&repo, CHECKOUT_ATTRIBUTES_SIDECAR_NAME).unwrap();
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), old);
        assert!(journal_path.is_file());
        assert!(sidecar_path.is_file());
        let journal = read_checkout_journal(&journal_path).unwrap().unwrap();
        assert!(
            validate_checkout_journal(&repo, &journal)
                .unwrap()
                .is_some()
        );

        restart_checkout_recovery_subprocess(&repo);

        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), old);
        assert!(!journal_path.exists());
        assert!(!sidecar_path.exists());
        assert_eq!(
            repo.git_bytes_result(&["show", &format!(":{}", meta::ATTRS_FILE)])
                .unwrap(),
            staged.as_bytes()
        );
        assert_eq!(
            std::fs::read(repo.root().join(meta::ATTRS_FILE)).unwrap(),
            worktree.as_bytes()
        );
    }

    #[test]
    fn startup_recovers_each_dirty_attribute_layer_after_upgrade_crash() {
        let directory = tempfile::tempdir().unwrap();
        let (repo, old, new, staged, worktree) = attributes_upgrade_crash_fixture(directory.path());
        let staged_oid = repo
            .git(&["rev-parse", &format!(":{}", meta::ATTRS_FILE)])
            .unwrap();
        crash_checkout_subprocess(&repo, &old, &new, "during_attributes", false);

        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), new);
        let journal =
            read_checkout_journal(&checkout_git_path(&repo, CHECKOUT_JOURNAL_NAME).unwrap())
                .unwrap()
                .unwrap();
        let upgrade = journal.attributes_upgrade.as_ref().unwrap();
        assert!(upgrade.index.is_none());
        assert!(upgrade.worktree.is_none());
        assert!(upgrade.sidecar.is_some());
        let durable = materialize_attributes_upgrade(&repo, journal.version, upgrade).unwrap();
        assert_eq!(durable.index.as_deref(), Some(staged.as_str()));
        assert_eq!(durable.worktree.as_deref(), Some(worktree.as_str()));
        assert_eq!(
            repo.git_bytes_result(&["show", &format!(":{}", meta::ATTRS_FILE)])
                .unwrap(),
            storage::attributes_text_strict(Some(&staged))
                .unwrap()
                .into_bytes(),
            "the crash point is after the index layer was normalized"
        );
        assert_eq!(
            std::fs::read(repo.root().join(meta::ATTRS_FILE)).unwrap(),
            worktree.as_bytes(),
            "the worktree layer must still be at its journalled v0 bytes"
        );

        repo.git(&["gc", "--prune=now"]).unwrap();
        assert!(
            repo.git_opt(&["cat-file", "-e", &staged_oid]).is_none(),
            "the original staged blob must actually be pruned before journal recovery"
        );
        assert!(recover_interrupted_checkout(&repo).unwrap());
        assert_eq!(
            repo.git_bytes_result(&["show", &format!(":{}", meta::ATTRS_FILE)])
                .unwrap(),
            storage::attributes_text_strict(Some(&staged))
                .unwrap()
                .into_bytes()
        );
        assert_eq!(
            std::fs::read(repo.root().join(meta::ATTRS_FILE)).unwrap(),
            storage::attributes_text_strict(Some(&worktree))
                .unwrap()
                .into_bytes()
        );
        assert_eq!(
            repo.git(&["status", "--short", "--", meta::ATTRS_FILE])
                .unwrap(),
            "MM .gitattributes"
        );
        assert!(
            !checkout_git_path(&repo, CHECKOUT_JOURNAL_NAME)
                .unwrap()
                .exists()
        );
        assert!(
            !checkout_git_path(&repo, CHECKOUT_ATTRIBUTES_SIDECAR_NAME)
                .unwrap()
                .exists()
        );
    }

    #[test]
    fn startup_recovery_before_upgrade_cas_preserves_dirty_attribute_layers() {
        let directory = tempfile::tempdir().unwrap();
        let (repo, old, new, staged, worktree) = attributes_upgrade_crash_fixture(directory.path());
        crash_checkout_subprocess(&repo, &old, &new, "after_journal", false);

        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), old);
        assert!(recover_interrupted_checkout(&repo).unwrap());
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), old);
        assert_eq!(
            repo.git_bytes_result(&["show", &format!(":{}", meta::ATTRS_FILE)])
                .unwrap(),
            staged.as_bytes()
        );
        assert_eq!(
            std::fs::read(repo.root().join(meta::ATTRS_FILE)).unwrap(),
            worktree.as_bytes()
        );
        assert_eq!(
            repo.git(&["status", "--short", "--", meta::ATTRS_FILE])
                .unwrap(),
            "MM .gitattributes"
        );
        assert!(
            !checkout_git_path(&repo, CHECKOUT_JOURNAL_NAME)
                .unwrap()
                .exists()
        );
        assert!(
            !checkout_git_path(&repo, CHECKOUT_ATTRIBUTES_SIDECAR_NAME)
                .unwrap()
                .exists()
        );
    }

    #[test]
    fn legacy_v2_attribute_oid_journal_recovers_while_its_objects_exist() {
        let directory = tempfile::tempdir().unwrap();
        let (repo, old, new, staged, worktree) = attributes_upgrade_crash_fixture(directory.path());
        crash_checkout_subprocess(&repo, &old, &new, "after_journal", false);

        let path = checkout_git_path(&repo, CHECKOUT_JOURNAL_NAME).unwrap();
        let mut journal = read_checkout_journal(&path).unwrap().unwrap();
        let materialized = materialize_attributes_upgrade(
            &repo,
            journal.version,
            journal.attributes_upgrade.as_ref().unwrap(),
        )
        .unwrap();
        let upgrade = journal.attributes_upgrade.as_mut().unwrap();
        upgrade.index = materialized.index.as_deref().map(|text| {
            raw_git(
                &repo,
                &["hash-object", "-w", "--no-filters", "--stdin"],
                Some(text),
            )
            .unwrap()
            .trim()
            .to_owned()
        });
        upgrade.worktree = materialized.worktree.as_deref().map(|text| {
            raw_git(
                &repo,
                &["hash-object", "-w", "--no-filters", "--stdin"],
                Some(text),
            )
            .unwrap()
            .trim()
            .to_owned()
        });
        upgrade.sidecar = None;
        journal.version = CHECKOUT_JOURNAL_OID_VERSION;
        remove_checkout_journal(&path).unwrap();
        remove_checkout_sidecar(
            &checkout_git_path(&repo, CHECKOUT_ATTRIBUTES_SIDECAR_NAME).unwrap(),
        )
        .unwrap();
        write_checkout_journal(&path, &journal).unwrap();

        assert!(recover_interrupted_checkout(&repo).unwrap());
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), old);
        assert_eq!(
            repo.git_bytes_result(&["show", &format!(":{}", meta::ATTRS_FILE)])
                .unwrap(),
            staged.as_bytes()
        );
        assert_eq!(
            std::fs::read(repo.root().join(meta::ATTRS_FILE)).unwrap(),
            worktree.as_bytes()
        );
        assert!(!path.exists());
    }

    #[test]
    fn legacy_v3_inline_attribute_journal_still_recovers() {
        let directory = tempfile::tempdir().unwrap();
        let (repo, old, new, staged, worktree) = attributes_upgrade_crash_fixture(directory.path());
        crash_checkout_subprocess(&repo, &old, &new, "after_journal", false);

        let path = checkout_git_path(&repo, CHECKOUT_JOURNAL_NAME).unwrap();
        let sidecar_path = checkout_git_path(&repo, CHECKOUT_ATTRIBUTES_SIDECAR_NAME).unwrap();
        let mut journal = read_checkout_journal(&path).unwrap().unwrap();
        let materialized = materialize_attributes_upgrade(
            &repo,
            journal.version,
            journal.attributes_upgrade.as_ref().unwrap(),
        )
        .unwrap();
        let upgrade = journal.attributes_upgrade.as_mut().unwrap();
        upgrade.index = materialized.index;
        upgrade.worktree = materialized.worktree;
        upgrade.sidecar = None;
        journal.version = CHECKOUT_JOURNAL_INLINE_VERSION;
        remove_checkout_journal(&path).unwrap();
        remove_checkout_sidecar(&sidecar_path).unwrap();
        write_checkout_journal(&path, &journal).unwrap();

        assert!(recover_interrupted_checkout(&repo).unwrap());
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), old);
        assert_eq!(
            repo.git_bytes_result(&["show", &format!(":{}", meta::ATTRS_FILE)])
                .unwrap(),
            staged.as_bytes()
        );
        assert_eq!(
            std::fs::read(repo.root().join(meta::ATTRS_FILE)).unwrap(),
            worktree.as_bytes()
        );
        assert!(!path.exists());
        assert!(!sidecar_path.exists());
    }

    #[test]
    fn recovery_rejects_sidecar_lengths_above_the_checkout_bound_before_reading() {
        let directory = tempfile::tempdir().unwrap();
        let (repo, old, new, _, _) = attributes_upgrade_crash_fixture(directory.path());
        crash_checkout_subprocess(&repo, &old, &new, "after_journal", false);

        let path = checkout_git_path(&repo, CHECKOUT_JOURNAL_NAME).unwrap();
        let sidecar_path = checkout_git_path(&repo, CHECKOUT_ATTRIBUTES_SIDECAR_NAME).unwrap();
        let mut journal = read_checkout_journal(&path).unwrap().unwrap();
        let descriptor = journal
            .attributes_upgrade
            .as_mut()
            .unwrap()
            .sidecar
            .as_mut()
            .unwrap();
        descriptor.index_bytes = Some(MAX_CHECKOUT_ATTRIBUTES_LAYER_BYTES as u64 + 1);
        descriptor.bytes =
            descriptor.index_bytes.unwrap() + descriptor.worktree_bytes.unwrap_or_default();
        remove_checkout_journal(&path).unwrap();
        write_checkout_journal(&path, &journal).unwrap();

        let error = recover_interrupted_checkout(&repo).unwrap_err();
        assert!(error.to_string().contains("safety limit"), "{error:#}");
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), old);
        assert!(path.exists(), "failed recovery must retain its decision");
        assert!(
            sidecar_path.exists(),
            "failed recovery must retain its snapshot for diagnosis/retry"
        );
    }

    #[test]
    fn attributes_upgrade_rejects_a_layer_above_the_checkout_snapshot_bound() {
        let directory = tempfile::tempdir().unwrap();
        let (repo, old, new, staged, _) = attributes_upgrade_crash_fixture(directory.path());
        let attributes_path = repo.root().join(meta::ATTRS_FILE);
        let oversized = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&attributes_path)
            .unwrap();
        oversized
            .set_len(MAX_CHECKOUT_ATTRIBUTES_LAYER_BYTES as u64 + 1)
            .unwrap();

        let error = update_branch_cas_and_refresh(&repo, "main", &new, &old, false).unwrap_err();
        assert!(error.to_string().contains("safety limit"), "{error:#}");
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), old);
        assert_eq!(
            repo.git_bytes_result(&["show", &format!(":{}", meta::ATTRS_FILE)])
                .unwrap(),
            staged.as_bytes()
        );
        assert_eq!(
            std::fs::metadata(attributes_path).unwrap().len(),
            MAX_CHECKOUT_ATTRIBUTES_LAYER_BYTES as u64 + 1
        );
        assert!(
            !checkout_git_path(&repo, CHECKOUT_JOURNAL_NAME)
                .unwrap()
                .exists()
        );
        assert!(
            !checkout_git_path(&repo, CHECKOUT_ATTRIBUTES_SIDECAR_NAME)
                .unwrap()
                .exists()
        );
    }

    #[test]
    fn checkout_recovery_preserves_staged_changes_outside_transaction_paths() {
        let directory = tempfile::tempdir().unwrap();
        let (repo, old, new) = checkout_crash_fixture(directory.path());
        crash_checkout_subprocess(&repo, &old, &new, "after_ref_cas", true);

        std::fs::write(repo.root().join("outside.txt"), "staged after crash\n").unwrap();
        repo.git(&["add", "--", "outside.txt"]).unwrap();

        assert_eq!(crate::commands::migration::migrate_repo(&repo).unwrap(), 0);
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), new);
        assert_eq!(
            repo.git(&["diff", "--cached", "--name-only"]).unwrap(),
            "outside.txt"
        );
        assert_eq!(
            repo.git(&["show", ":outside.txt"]).unwrap(),
            "staged after crash"
        );
        assert_eq!(
            std::fs::read_to_string(repo.root().join("a.txt")).unwrap(),
            "new a\n"
        );
        assert_eq!(
            std::fs::read_to_string(repo.root().join("b.txt")).unwrap(),
            "new b\n"
        );
        assert!(
            !checkout_git_path(&repo, CHECKOUT_JOURNAL_NAME)
                .unwrap()
                .exists()
        );
    }

    #[test]
    fn checkout_recovery_fails_closed_for_branch_or_ref_mismatch() {
        let branch_directory = tempfile::tempdir().unwrap();
        let (branch_repo, old, new) = checkout_crash_fixture(branch_directory.path());
        crash_checkout_subprocess(&branch_repo, &old, &new, "after_ref_cas", true);
        update_ref_cas(&branch_repo, "refs/heads/other", &old, None).unwrap();
        branch_repo
            .git(&["symbolic-ref", "HEAD", "refs/heads/other"])
            .unwrap();
        let branch_error = recover_interrupted_checkout(&branch_repo).unwrap_err();
        assert!(
            branch_error.to_string().contains("checked out branch"),
            "{branch_error:#}"
        );
        assert!(
            checkout_git_path(&branch_repo, CHECKOUT_JOURNAL_NAME)
                .unwrap()
                .exists()
        );

        let ref_directory = tempfile::tempdir().unwrap();
        let (ref_repo, old, new) = checkout_crash_fixture(ref_directory.path());
        crash_checkout_subprocess(&ref_repo, &old, &new, "after_ref_cas", true);
        let tree = tree_apply_owned(
            &ref_repo,
            &new,
            vec![("a.txt".into(), Some(b"third\n".to_vec()))],
        )
        .unwrap();
        let third = commit_tree(&ref_repo, &tree, &[&new], "third").unwrap();
        update_ref_cas(&ref_repo, "refs/heads/main", &third, Some(&new)).unwrap();
        let ref_error = recover_interrupted_checkout(&ref_repo).unwrap_err();
        assert!(ref_error.to_string().contains("expected"), "{ref_error:#}");
        assert!(
            checkout_git_path(&ref_repo, CHECKOUT_JOURNAL_NAME)
                .unwrap()
                .exists()
        );
    }

    /// Continue surgery on a tree rather than a commit: `tree_with` needs a commit, so this is
    /// the tree variant.
    fn tree_with_commit_free(
        repo: &Repo,
        base_tree: &str,
        path: &str,
        content: Option<&str>,
    ) -> Result<String> {
        // read-tree accepts a tree id.
        let idx = repo
            .root()
            .join(format!(".git/agit-plumbing2-{}.index", std::process::id()));
        let _ = std::fs::remove_file(&idx);
        let out = (|| -> Result<String> {
            let run = |args: &[&str]| -> Result<String> {
                let o = std::process::Command::new("git")
                    .args(args)
                    .current_dir(repo.root())
                    .env("GIT_INDEX_FILE", &idx)
                    .output()?;
                if !o.status.success() {
                    anyhow::bail!("{}", String::from_utf8_lossy(&o.stderr));
                }
                Ok(String::from_utf8_lossy(&o.stdout).to_string())
            };
            run(&["read-tree", base_tree])?;
            if let Some(text) = content {
                let blob = raw_git(repo, &["hash-object", "-w", "--stdin"], Some(text))?;
                run(&[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &format!("100644,{},{}", blob.trim(), path),
                ])?;
            }
            Ok(run(&["write-tree"])?.trim().to_string())
        })();
        let _ = std::fs::remove_file(&idx);
        out
    }
}
