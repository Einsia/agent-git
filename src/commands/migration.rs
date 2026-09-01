//! Storage layout migration for the agent repositories on this machine.
//!
//! v0 history commits and tags are never rewritten: a snapshot id is the commit SHA, and silently
//! changing the DAG invalidates published references. Startup migration only appends one
//! mechanical v1 commit after each v0 branch tip; the read layer keeps reading v0 history as
//! well. Every ref advances by CAS inside one `update-ref --stdin` transaction, so when any one
//! branch is modified concurrently while the migration is being prepared, not one ref in the
//! repository moves.

use crate::domain::meta::{self, LayoutVersion};
use crate::domain::repo::Repo;
use crate::domain::storage;
use anyhow::{Context, Result};
use fs2::FileExt as _;
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub repos: usize,
    pub branches: usize,
    pub skipped: usize,
}

#[derive(Debug, Clone, Copy)]
struct MigrationLimits {
    event_bytes: usize,
    legacy_blob_bytes: usize,
    materialized_sequence_bytes: usize,
    sequence_events: usize,
    unique_event_bytes: usize,
}

const MIGRATION_LIMITS: MigrationLimits = MigrationLimits {
    event_bytes: storage::MAX_EVENT_BYTES,
    legacy_blob_bytes: storage::MAX_MATERIALIZED_BYTES,
    materialized_sequence_bytes: storage::MAX_MATERIALIZED_BYTES,
    sequence_events: storage::MAX_SEQUENCE_EVENTS,
    unique_event_bytes: storage::MAX_MATERIALIZED_BYTES,
};

#[derive(Debug, Default, Clone, Copy)]
struct MigrationMetrics {
    peak_event_buffers: usize,
    legacy_batch_processes: usize,
    object_hash_processes: usize,
    event_spool_writes: usize,
    duplicate_event_reuses: usize,
}

const MIGRATION_SPOOL_PREFIX: &str = "agit-layout-v1-spool-";
const MIGRATION_SPOOL_LOCK: &str = "agit-layout-v1-spool.lock";
const MIGRATION_STDERR_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy)]
struct DiscoveryLimits {
    owners: usize,
    repos_per_owner: usize,
    repos: usize,
}

const DISCOVERY_LIMITS: DiscoveryLimits = DiscoveryLimits {
    owners: 4_096,
    repos_per_owner: 4_096,
    repos: 65_536,
};

fn migration_git_dir(repo: &Repo) -> Result<PathBuf> {
    let path = repo.root().join(".git");
    let metadata = std::fs::symlink_metadata(&path)
        .with_context(|| format!("cannot inspect migration Git directory {}", path.display()))?;
    anyhow::ensure!(
        metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
        "refusing non-directory or symlinked migration Git directory {}",
        path.display()
    );
    std::fs::canonicalize(&path)
        .with_context(|| format!("cannot resolve migration Git directory {}", path.display()))
}

fn lock_repo_migration(repo: &Repo) -> Result<(File, PathBuf)> {
    let git_dir = migration_git_dir(repo)?;
    let path = git_dir.join(MIGRATION_SPOOL_LOCK);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {}
        Ok(_) => anyhow::bail!(
            "refusing non-regular migration spool lock {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!("cannot inspect migration spool lock {}", path.display())
            });
        }
    }
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("cannot open migration spool lock {}", path.display()))?;
    let metadata = std::fs::symlink_metadata(&path)
        .with_context(|| format!("cannot re-inspect migration spool lock {}", path.display()))?;
    anyhow::ensure!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "migration spool lock became non-regular while opening {}",
        path.display()
    );
    lock.lock_exclusive()
        .with_context(|| format!("cannot lock migration spool state in {}", path.display()))?;
    Ok((lock, git_dir))
}

fn is_owned_spool_name(name: &OsStr) -> Result<bool> {
    let Some(name) = name.to_str() else {
        return Ok(false);
    };
    let Some(suffix) = name.strip_prefix(MIGRATION_SPOOL_PREFIX) else {
        return Ok(false);
    };
    anyhow::ensure!(
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric()),
        "refusing malformed migration spool name {name:?}"
    );
    Ok(true)
}

/// Remove only stale directories created by [`LegacySnapshotSpool`]. The caller holds the
/// repository migration lock for both this cleanup and every subsequent spool creation, so an
/// entry matching the reserved prefix cannot belong to a live AgentGit migration.
fn cleanup_stale_spools(git_dir: &Path, _lock: &File) -> Result<usize> {
    let mut removed = 0usize;
    for entry in std::fs::read_dir(git_dir)
        .with_context(|| format!("cannot scan migration Git directory {}", git_dir.display()))?
    {
        let entry = entry?;
        if !is_owned_spool_name(&entry.file_name())? {
            continue;
        }
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)
            .with_context(|| format!("cannot inspect stale migration spool {}", path.display()))?;
        anyhow::ensure!(
            metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
            "refusing non-directory or symlinked migration spool {}",
            path.display()
        );
        std::fs::remove_dir_all(&path)
            .with_context(|| format!("cannot remove stale migration spool {}", path.display()))?;
        removed = removed
            .checked_add(1)
            .context("stale spool count overflow")?;
    }
    Ok(removed)
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct MigrationProbe {
    reserved_spool: bool,
    checkout_recovery: bool,
    legacy_storage_recovery: bool,
    v0_branch: bool,
}

impl MigrationProbe {
    fn needs_lock(self) -> bool {
        self.reserved_spool
            || self.checkout_recovery
            || self.legacy_storage_recovery
            || self.v0_branch
    }

    fn recovery_required(self) -> bool {
        self.checkout_recovery || self.legacy_storage_recovery
    }
}

#[derive(Debug)]
enum RepoMigrationFailure {
    Skippable(anyhow::Error),
    Recovery(anyhow::Error),
}

impl RepoMigrationFailure {
    fn into_error(self) -> anyhow::Error {
        match self {
            Self::Skippable(error) | Self::Recovery(error) => error,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MigrationFailureKind {
    Skippable,
    Recovery,
}

impl MigrationFailureKind {
    fn classify(self, error: anyhow::Error) -> RepoMigrationFailure {
        match self {
            Self::Skippable => RepoMigrationFailure::Skippable(error),
            Self::Recovery => RepoMigrationFailure::Recovery(error),
        }
    }

    fn result<T>(self, result: Result<T>) -> RepoMigrationResult<T> {
        result.map_err(|error| self.classify(error))
    }
}

type RepoMigrationResult<T> = std::result::Result<T, RepoMigrationFailure>;

fn failure_kind_after_probe(probe: MigrationProbe) -> MigrationFailureKind {
    if probe.recovery_required() {
        MigrationFailureKind::Recovery
    } else {
        MigrationFailureKind::Skippable
    }
}

fn has_reserved_spool(git_dir: &Path) -> Result<bool> {
    for entry in std::fs::read_dir(git_dir)
        .with_context(|| format!("cannot scan migration Git directory {}", git_dir.display()))?
    {
        let entry = entry?;
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(MIGRATION_SPOOL_PREFIX)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Pure read-only decision boundary. In particular, it never opens either AgentGit lock file.
#[cfg(test)]
fn probe_repo_migration(repo: &Repo) -> RepoMigrationResult<MigrationProbe> {
    probe_repo_migration_with_failure_kind(repo, MigrationFailureKind::Skippable)
}

fn probe_repo_migration_with_failure_kind(
    repo: &Repo,
    inherited: MigrationFailureKind,
) -> RepoMigrationResult<MigrationProbe> {
    let git_dir = inherited.result(migration_git_dir(repo))?;
    let reserved_spool = inherited.result(has_reserved_spool(&git_dir))?;
    let checkout_recovery =
        inherited.result(super::plumbing::interrupted_checkout_metadata_present(repo))?;
    if checkout_recovery {
        // Do not perform unrelated reads after finding a half-published checkout. The caller must
        // acquire the recovery locks and every later failure is command-fatal.
        return Ok(MigrationProbe {
            reserved_spool,
            checkout_recovery: true,
            ..MigrationProbe::default()
        });
    }
    let legacy_storage_recovery = probe_legacy_storage_checkout_recovery(repo, inherited)?;
    if legacy_storage_recovery {
        return Ok(MigrationProbe {
            reserved_spool,
            legacy_storage_recovery: true,
            ..MigrationProbe::default()
        });
    }
    // A successful absence/clean-status check is the durable recovery-complete checkpoint. Even
    // when this was a locked re-probe after an earlier recovery observation, unrelated branch
    // reads below return to ordinary per-repository isolation.
    let failure_kind = MigrationFailureKind::Skippable;
    let mut v0_branch = false;
    for (_, sha) in failure_kind.result(branch_heads(repo))? {
        if failure_kind
            .result(read_meta_at(repo, &sha))?
            .is_some_and(|snapshot| snapshot.layout == LayoutVersion::V0)
        {
            v0_branch = true;
            break;
        }
    }
    Ok(MigrationProbe {
        reserved_spool,
        checkout_recovery,
        legacy_storage_recovery,
        v0_branch,
    })
}

/// Every CLI startup first scans `$AGIT_HOME/repos/<owner>/<name>`.
///
/// The scan itself is cheap; the global file lock serializes only the real ref transactions. A v0
/// branch arriving from a fresh clone/fetch is therefore migrated before the next command, and no
/// "done once" marker can let it slip through.
pub fn migrate_startup() -> Result<Report> {
    let home = crate::infra::config::agit_home()?;
    let repos = crate::infra::config::repos_dir()?;
    migrate_startup_at(&home, &repos)
}

fn migrate_startup_at(home: &Path, repos: &Path) -> Result<Report> {
    std::fs::create_dir_all(home)?;
    let lock_path = home.join("layout-v1.lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("cannot open migration lock {}", lock_path.display()))?;
    lock.lock_exclusive()
        .with_context(|| format!("cannot lock {}", lock_path.display()))?;

    let discovery = local_repo_paths(repos)?;
    let mut report = Report {
        skipped: discovery.skipped,
        ..Report::default()
    };
    for repo_path in discovery.paths {
        let Some(repo) = Repo::open(&repo_path) else {
            continue;
        };
        let branches = match migrate_repo_classified(&repo) {
            Ok(branches) => branches,
            Err(RepoMigrationFailure::Skippable(error)) => {
                report.skipped += 1;
                crate::ui::warning(&format!(
                    "skipped storage migration for {}: {error:#}",
                    repo_path.display()
                ));
                continue;
            }
            Err(RepoMigrationFailure::Recovery(error)) => {
                return Err(error).with_context(|| {
                    format!(
                        "cannot safely recover interrupted checkout in {}",
                        repo_path.display()
                    )
                });
            }
        };
        if branches > 0 {
            report.repos += 1;
            report.branches += branches;
        }
    }
    fs2::FileExt::unlock(&lock)?;
    Ok(report)
}

#[derive(Debug, Default)]
struct LocalRepoDiscovery {
    paths: Vec<PathBuf>,
    skipped: usize,
}

fn warn_discovery_skip(path: &Path, error: &dyn std::fmt::Display) {
    crate::ui::warning(&format!(
        "skipped local repository discovery at {}: {error}",
        path.display()
    ));
}

fn local_repo_paths(root: &Path) -> Result<LocalRepoDiscovery> {
    local_repo_paths_with_limits(root, DISCOVERY_LIMITS)
}

fn local_repo_paths_with_limits(
    root: &Path,
    limits: DiscoveryLimits,
) -> Result<LocalRepoDiscovery> {
    anyhow::ensure!(
        limits.owners > 0 && limits.repos_per_owner > 0 && limits.repos > 0,
        "local repository discovery limits must be positive"
    );
    let metadata = match std::fs::metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LocalRepoDiscovery::default());
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("cannot inspect local repository root {}", root.display())
            });
        }
    };
    anyhow::ensure!(
        metadata.is_dir(),
        "local repository root {} is not a directory",
        root.display()
    );

    let mut discovery = LocalRepoDiscovery::default();
    let owners = std::fs::read_dir(root)
        .with_context(|| format!("cannot enumerate local repository root {}", root.display()))?;
    let mut owner_count = 0usize;
    for owner in owners {
        owner_count = owner_count
            .checked_add(1)
            .context("local repository owner count overflow")?;
        anyhow::ensure!(
            owner_count <= limits.owners,
            "local repository root exceeds its {}-owner discovery cap",
            limits.owners
        );
        let owner = match owner {
            Ok(owner) => owner,
            Err(error) => {
                discovery.skipped += 1;
                warn_discovery_skip(root, &error);
                continue;
            }
        };
        let owner_path = owner.path();
        let owner_type = match owner.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                discovery.skipped += 1;
                warn_discovery_skip(&owner_path, &error);
                continue;
            }
        };
        if !owner_type.is_dir() {
            continue;
        }
        let repos = match std::fs::read_dir(&owner_path) {
            Ok(repos) => repos,
            Err(error) => {
                discovery.skipped += 1;
                warn_discovery_skip(&owner_path, &error);
                continue;
            }
        };

        let owner_start = discovery.paths.len();
        let mut repo_count = 0usize;
        let mut owner_overflow = false;
        for repo in repos {
            repo_count = repo_count
                .checked_add(1)
                .context("per-owner local repository count overflow")?;
            if repo_count > limits.repos_per_owner {
                owner_overflow = true;
                break;
            }
            let repo = match repo {
                Ok(repo) => repo,
                Err(error) => {
                    discovery.skipped += 1;
                    warn_discovery_skip(&owner_path, &error);
                    continue;
                }
            };
            let repo_path = repo.path();
            let repo_type = match repo.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    discovery.skipped += 1;
                    warn_discovery_skip(&repo_path, &error);
                    continue;
                }
            };
            if !repo_type.is_dir() {
                continue;
            }
            match std::fs::symlink_metadata(repo_path.join(".git")) {
                Ok(git) if git.file_type().is_dir() && !git.file_type().is_symlink() => {
                    anyhow::ensure!(
                        discovery.paths.len() < limits.repos,
                        "local repository root exceeds its {}-repository discovery cap",
                        limits.repos
                    );
                    discovery
                        .paths
                        .try_reserve(1)
                        .context("cannot allocate local repository discovery result")?;
                    discovery.paths.push(repo_path);
                }
                Ok(_) => {
                    discovery.skipped += 1;
                    warn_discovery_skip(&repo_path, &".git is not a real directory");
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    discovery.skipped += 1;
                    warn_discovery_skip(&repo_path, &error);
                }
            }
        }
        if owner_overflow {
            discovery.paths.truncate(owner_start);
            discovery.skipped += 1;
            warn_discovery_skip(
                &owner_path,
                &format_args!(
                    "owner exceeds its {}-repository discovery cap",
                    limits.repos_per_owner
                ),
            );
        }
    }
    discovery.paths.sort();
    Ok(discovery)
}

/// Migrate one local non-bare agent repo. Returns how many branches advanced.
pub fn migrate_repo(repo: &Repo) -> Result<usize> {
    migrate_repo_classified(repo).map_err(RepoMigrationFailure::into_error)
}

fn migrate_repo_classified(repo: &Repo) -> RepoMigrationResult<usize> {
    // Synchronize with a checkout transaction that was already inside its pre-journal window.
    // This opens only an existing mutex: a clean read-only v1 repository stays write-free. It is
    // a startup barrier, not command-lifetime exclusion; a later transaction can still begin in
    // the same window that existed after the old unconditional recovery call released its lock.
    let checkout_barrier = match super::plumbing::recover_behind_existing_checkout_lock(repo) {
        Ok(outcome) => outcome,
        Err(super::plumbing::ExistingCheckoutBarrierFailure::Skippable(error)) => {
            return Err(RepoMigrationFailure::Skippable(error));
        }
        Err(super::plumbing::ExistingCheckoutBarrierFailure::Recovery(error)) => {
            return Err(RepoMigrationFailure::Recovery(error));
        }
    };
    if checkout_barrier == super::plumbing::ExistingCheckoutBarrierOutcome::RecoveryPending {
        // Recovery metadata authorizes creating the checkout mutex. Once locked recovery returns,
        // the checkout has converged and later ordinary migration failures become skippable again.
        MigrationFailureKind::Recovery
            .result(super::plumbing::recover_interrupted_checkout(repo))?;
    }
    let probe = probe_repo_migration_with_failure_kind(repo, MigrationFailureKind::Skippable)?;
    migrate_repo_after_probe(repo, probe)
}

fn migrate_repo_after_probe(repo: &Repo, probe: MigrationProbe) -> RepoMigrationResult<usize> {
    if !probe.needs_lock() {
        return Ok(0);
    }
    let mut failure_kind = failure_kind_after_probe(probe);
    let (repo_migration_lock, git_dir) = failure_kind.result(lock_repo_migration(repo))?;
    // The probe-to-lock interval permits another process to finish the work. Re-run the exact
    // read-only decision while holding the lock before cleaning or recovering anything.
    let probe = probe_repo_migration_with_failure_kind(repo, failure_kind)?;
    if !probe.needs_lock() {
        return Ok(0);
    }
    failure_kind = failure_kind_after_probe(probe);
    failure_kind.result(cleanup_stale_spools(&git_dir, &repo_migration_lock))?;
    if probe.checkout_recovery {
        failure_kind.result(super::plumbing::recover_interrupted_checkout(repo))?;
        // The journal/sidecar was durably removed only after checkout convergence. Do not let an
        // unrelated malformed v0 branch defeat per-repository isolation beyond this checkpoint.
        failure_kind = MigrationFailureKind::Skippable;
    }
    if probe_legacy_storage_checkout_recovery(repo, failure_kind)? {
        failure_kind = MigrationFailureKind::Recovery;
        failure_kind.result(finish_interrupted_checkout(repo))?;
        // `refresh_storage_checkout` returned only after the exact old/new endpoints converged.
        failure_kind = MigrationFailureKind::Skippable;
    } else if probe.legacy_storage_recovery {
        // Another process completed the candidate between the locked probes; the successful
        // no-optional-locks status check is the corresponding safe checkpoint.
        failure_kind = MigrationFailureKind::Skippable;
    }
    failure_kind.result(migrate_repo_after_checkout_recovery(repo))
}

fn migrate_repo_after_checkout_recovery(repo: &Repo) -> Result<usize> {
    let heads = branch_heads(repo)?;
    let mut pending = Vec::new();
    for (name, sha) in heads {
        if read_meta_at(repo, &sha)?.is_some_and(|snapshot| snapshot.layout == LayoutVersion::V0) {
            pending.push((name, sha));
        }
    }
    if pending.is_empty() {
        return Ok(0);
    }

    let current = current_branch_ref(repo)?;
    let checkout_head = repo.git(&["rev-parse", "--verify", "HEAD^{commit}"])?;
    if read_meta_at(repo, &checkout_head)?
        .is_some_and(|snapshot| snapshot.layout == LayoutVersion::V0)
    {
        // This also covers a detached v0 checkout. Even though no symbolic branch will be
        // refreshed below, migrating other heads must not leave a future checkout poised to
        // overwrite ignored/untracked root data.
        super::plumbing::ensure_v1_upgrade_preflight(repo, &checkout_head)?;
        let dirty = repo.git(&["status", "--porcelain"])?;
        if !dirty.trim().is_empty() {
            anyhow::bail!(
                "the checked-out v0 branch has uncommitted or staged work; finish or discard it before the automatic v1 migration:\n{dirty}"
            );
        }
    }

    // Branches pointing at the same old tip reuse one migration commit and keep sharing objects
    // and history.
    let mut migrated_heads: HashMap<String, String> = HashMap::new();
    let mut updates = Vec::with_capacity(pending.len());
    for (refname, old) in &pending {
        let new = match migrated_heads.get(old) {
            Some(new) => new.clone(),
            None => {
                let new = migrate_tip(repo, old)?;
                migrated_heads.insert(old.clone(), new.clone());
                new
            }
        };
        updates.push((refname.clone(), old.clone(), new));
    }

    let mut checkout_transaction = if let Some(current_ref) = current.as_deref()
        && let Some((_, old, new)) = updates.iter().find(|(name, _, _)| name == current_ref)
    {
        let branch = current_ref
            .strip_prefix("refs/heads/")
            .context("current branch ref has an unexpected shape")?;
        Some(super::plumbing::prepare_checkout_transaction(
            repo, branch, old, new, false,
        )?)
    } else {
        None
    };

    let created_recoveries = match update_refs_atomically(repo, &updates) {
        Ok(created) => created,
        Err(update_error) => {
            if let Some(transaction) = checkout_transaction.take()
                && let Err(cleanup_error) =
                    super::plumbing::finish_checkout_transaction(transaction)
            {
                anyhow::bail!(
                    "storage ref transaction failed ({update_error:#}) and removing its checkout journal also failed ({cleanup_error:#})"
                );
            }
            return Err(update_error);
        }
    };

    // update-ref does not touch the checkout. The real index is pointed at the new HEAD and the
    // format-managed paths refreshed only when the worktree was confirmed completely clean before
    // the migration; shared files stay byte-for-byte unchanged.
    if let Some(transaction) = checkout_transaction.as_ref()
        && let Err(refresh_error) = super::plumbing::refresh_prepared_checkout(repo, transaction)
    {
        // The refs moved atomically, but Git has no transaction spanning refs + worktree. If the
        // clean checkout cannot be refreshed, put every branch head back with another CAS before
        // restoring the old storage paths. Recovery refs intentionally remain as an audit trail.
        if let Err(rollback_error) = rollback_heads_atomically(repo, &updates, &created_recoveries)
        {
            anyhow::bail!(
                "storage refs moved to v1, checkout refresh failed ({:#}), and the CAS rollback also failed ({rollback_error:#}); the checkout journal and refs/agit/layout-v0 recovery refs were retained",
                refresh_error.error
            );
        }
        if refresh_error.restored()
            && let Some(transaction) = checkout_transaction.take()
        {
            super::plumbing::finish_checkout_transaction(transaction)?;
        }
        return Err(refresh_error.error)
            .context("v1 refs were rolled back because checkout refresh failed");
    }

    if let Some(transaction) = checkout_transaction.take() {
        super::plumbing::finish_checkout_transaction(transaction)?;
    }

    Ok(updates.len())
}

/// Complete the only non-atomic part of a previous migration. The ref transaction may have
/// committed immediately before the process stopped, leaving HEAD on the mechanical v1 commit
/// while the real index/worktree still contains its v0 parent. The recovery ref gives us an
/// unambiguous old endpoint; the exact migration subject and parent relationship keep this from
/// treating an ordinary later v1 commit as an interrupted migration.
#[derive(Debug)]
struct InterruptedStorageCheckout {
    current_ref: String,
    old: String,
    head: String,
}

fn interrupted_storage_checkout(repo: &Repo) -> Result<Option<InterruptedStorageCheckout>> {
    let Some(current_ref) = current_branch_ref(repo)? else {
        return Ok(None);
    };
    let branch = current_ref
        .strip_prefix("refs/heads/")
        .context("current branch ref has an unexpected shape")?;
    let recovery = format!("refs/agit/layout-v0/{branch}");
    let Some(old) = optional_ref(repo, &recovery)? else {
        return Ok(None);
    };
    let head = repo.git(&["rev-parse", "--verify", "HEAD^{commit}"])?;
    let Some(snapshot) = read_meta_at(repo, &head)? else {
        return Ok(None);
    };
    if snapshot.layout != LayoutVersion::V1 {
        return Ok(None);
    }
    let subject = repo.git(&["show", "-s", "--format=%s", &head])?;
    if subject != meta::STORAGE_MIGRATION_MESSAGE {
        return Ok(None);
    }
    let parent = repo.git(&["rev-parse", &format!("{head}^1")])?;
    if parent != old {
        return Ok(None);
    }
    Ok(Some(InterruptedStorageCheckout {
        current_ref,
        old,
        head,
    }))
}

/// Old AgentGit versions could move the migration ref without a checkout journal. Recovery refs
/// are intentionally permanent audit records, so their mere presence is not pending work. Use a
/// no-optional-locks status restricted to migration-owned paths to distinguish a clean completed
/// checkout from the old or partially refreshed endpoint without touching the Git directory.
/// Classify the status read as recovery-fatal once the exact legacy ref/commit/parent candidate
/// has been found. Before that point, ordinary repository corruption remains an isolated skip.
fn probe_legacy_storage_checkout_recovery(
    repo: &Repo,
    inherited: MigrationFailureKind,
) -> RepoMigrationResult<bool> {
    let Some(_) = inherited.result(interrupted_storage_checkout(repo))? else {
        return Ok(false);
    };
    MigrationFailureKind::Recovery.result(legacy_storage_checkout_paths_dirty(repo))
}

fn legacy_storage_checkout_paths_dirty(repo: &Repo) -> Result<bool> {
    let output = Command::new("git")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .arg("--no-replace-objects")
        .args(["-c", "core.fsmonitor=false"])
        .arg("-C")
        .arg(repo.root())
        .args([
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignored=matching",
            "--",
            meta::ATTRS_FILE,
            meta::FILE,
            meta::LOG_FILE,
            meta::VIEW_FILE,
            meta::LEGACY_LOG_FILE,
            meta::LEGACY_VIEW_FILE,
            meta::EVENTS_DIR,
        ])
        .output()
        .context("cannot inspect legacy storage checkout recovery state")?;
    if !output.status.success() {
        anyhow::bail!(
            "git status for legacy storage checkout recovery failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(!output.stdout.is_empty())
}

fn finish_interrupted_checkout(repo: &Repo) -> Result<()> {
    let Some(interrupted) = interrupted_storage_checkout(repo)? else {
        return Ok(());
    };

    super::plumbing::ensure_v1_namespace_available_at(repo, &interrupted.old)?;
    super::plumbing::ensure_v1_namespace_absent_or_matches(repo, &interrupted.head)?;
    super::plumbing::refresh_storage_checkout(repo, &interrupted.old, &interrupted.head)
        .with_context(|| {
            format!(
                "cannot finish interrupted storage migration for {}",
                interrupted.current_ref
            )
        })
}

fn current_branch_ref(repo: &Repo) -> Result<Option<String>> {
    let output = std::process::Command::new("git")
        .arg("--no-replace-objects")
        .arg("-C")
        .arg(repo.root())
        .args(["symbolic-ref", "-q", "HEAD"])
        .output()?;
    if output.status.success() {
        let name = String::from_utf8(output.stdout)?.trim().to_owned();
        anyhow::ensure!(
            name.starts_with("refs/heads/"),
            "HEAD names a non-branch ref"
        );
        return Ok(Some(name));
    }
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    anyhow::bail!(
        "git symbolic-ref HEAD failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

fn optional_ref(repo: &Repo, name: &str) -> Result<Option<String>> {
    let output = std::process::Command::new("git")
        .arg("--no-replace-objects")
        .arg("-C")
        .arg(repo.root())
        .args(["rev-parse", "--verify", "--quiet", name])
        .output()?;
    if output.status.success() {
        return Ok(Some(String::from_utf8(output.stdout)?.trim().to_owned()));
    }
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    anyhow::bail!(
        "git rev-parse --verify {name} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

fn read_meta_at(repo: &Repo, commit: &str) -> Result<Option<meta::Meta>> {
    let Some(text) = repo.show_raw_result(commit, meta::FILE)? else {
        return Ok(None);
    };
    serde_json::from_str(&text)
        .with_context(|| format!("invalid {} at {commit}", meta::FILE))
        .map(Some)
}

fn branch_heads(repo: &Repo) -> Result<Vec<(String, String)>> {
    let out = repo.git(&[
        "for-each-ref",
        "--format=%(refname)%00%(objectname)",
        "refs/heads/",
    ])?;
    let mut heads = vec![];
    for line in out.lines() {
        let Some((name, sha)) = line.split_once('\0') else {
            anyhow::bail!("git returned a malformed branch record");
        };
        if !name.starts_with("refs/heads/") || sha.is_empty() {
            anyhow::bail!("git returned a malformed branch head: {line:?}");
        }
        heads.push((name.to_string(), sha.to_string()));
    }
    Ok(heads)
}

fn migrate_tip(repo: &Repo, old: &str) -> Result<String> {
    let mut metrics = MigrationMetrics::default();
    migrate_tip_with_limits(repo, old, MIGRATION_LIMITS, &mut metrics)
}

fn migrate_tip_with_limits(
    repo: &Repo,
    old: &str,
    limits: MigrationLimits,
    metrics: &mut MigrationMetrics,
) -> Result<String> {
    let raw_meta = repo
        .show_raw_result(old, meta::FILE)?
        .with_context(|| format!("{old} has no readable {}", meta::FILE))?;
    let migrated_meta = meta::storage_migration_meta(&raw_meta)?;
    let snapshot = &migrated_meta.snapshot;
    super::plumbing::ensure_v1_namespace_available_at(repo, old)?;

    let existing_attrs = super::plumbing::regular_blob_text_at(repo, old, meta::ATTRS_FILE)?;
    let attrs = storage::attributes_text_strict(existing_attrs.as_deref())?;
    let tree = if snapshot.is_session_line() && !snapshot.session.is_empty() {
        let mut spool = stream_legacy_snapshot(repo, old, limits, metrics)
            .with_context(|| format!("v0 session tip {old} has no readable transcript"))?;
        spool.finish_payloads(attrs.as_bytes(), migrated_meta.text.as_bytes())?;
        apply_streamed_tree_edits(repo, old, &spool, metrics)?
    } else {
        anyhow::ensure!(
            !tree_path_exists(repo, old, meta::LEGACY_LOG_FILE)?
                && !tree_path_exists(repo, old, meta::LEGACY_VIEW_FILE)?,
            "unclaimed v0 session/file line carries legacy session storage; refusing to delete it"
        );
        let mut edits: BTreeMap<String, Option<Vec<u8>>> = BTreeMap::new();
        edits.insert(meta::ATTRS_FILE.into(), Some(attrs.into_bytes()));
        edits.insert(meta::FILE.into(), Some(migrated_meta.text.into_bytes()));
        super::plumbing::tree_apply_owned(repo, old, edits.into_iter().collect())?
    };
    super::plumbing::storage_migration_commit(repo, &tree, old)
}

/// Disk-backed builder for one legacy tip. Only one bounded raw line and its canonical form are
/// resident at a time; event bodies and both v1 sequences stay in a private Git-directory spool
/// until every resource check has passed.
struct LegacySnapshotSpool {
    log: BufWriter<File>,
    view: BufWriter<File>,
    hash_sources: BufWriter<File>,
    hash_targets: BufWriter<File>,
    events: HashMap<String, EventFingerprint>,
    log_bytes: usize,
    view_bytes: usize,
    log_events: usize,
    view_events: usize,
    unique_event_bytes: usize,
    root: tempfile::TempDir,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EventFingerprint {
    bytes: usize,
    digest: [u8; 32],
}

impl EventFingerprint {
    fn from_canonical(canonical: &str) -> Self {
        Self {
            bytes: canonical.len(),
            digest: Sha256::digest(canonical.as_bytes()).into(),
        }
    }
}

#[derive(Clone, Copy)]
enum SequenceTarget {
    Log,
    View,
}

impl LegacySnapshotSpool {
    fn create(repo: &Repo) -> Result<Self> {
        let git_dir = migration_git_dir(repo)?;
        let root = tempfile::Builder::new()
            .prefix(MIGRATION_SPOOL_PREFIX)
            .tempdir_in(&git_dir)
            .with_context(|| format!("cannot create migration spool in {}", git_dir.display()))?;
        let spool_name = root
            .path()
            .file_name()
            .context("temporary migration spool has no final path component")?;
        anyhow::ensure!(
            root.path().parent() == Some(git_dir.as_path()) && is_owned_spool_name(spool_name)?,
            "temporary migration spool escaped its reserved Git-directory prefix"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700))?;
        }
        std::fs::create_dir(root.path().join("events"))?;
        std::fs::create_dir(root.path().join("control"))?;
        let log = BufWriter::new(File::create(root.path().join("control/log-sequence"))?);
        let view = BufWriter::new(File::create(root.path().join("control/view-sequence"))?);
        let hash_sources = BufWriter::new(File::create(root.path().join("hash-sources"))?);
        let hash_targets = BufWriter::new(File::create(root.path().join("hash-targets"))?);
        Ok(Self {
            log,
            view,
            hash_sources,
            hash_targets,
            events: HashMap::new(),
            log_bytes: 0,
            view_bytes: 0,
            log_events: 0,
            view_events: 0,
            unique_event_bytes: 0,
            root,
        })
    }

    fn root(&self) -> &Path {
        self.root.path()
    }

    fn record_log(
        &mut self,
        canonical: &str,
        id: &str,
        limits: MigrationLimits,
        metrics: &mut MigrationMetrics,
    ) -> Result<()> {
        self.append_sequence(SequenceTarget::Log, id, canonical.len(), limits)?;
        self.ensure_event(canonical, id, limits, metrics)
    }

    fn record_view(
        &mut self,
        canonical: &str,
        id: &str,
        limits: MigrationLimits,
        metrics: &mut MigrationMetrics,
    ) -> Result<()> {
        self.append_sequence(SequenceTarget::View, id, canonical.len(), limits)?;
        if !self.events.contains_key(id) {
            // Historical v0 merge/revert markers could exist only in VIEW. The first such
            // occurrence is appended to LOG exactly once so the v1 reachability contract holds.
            self.append_sequence(SequenceTarget::Log, id, canonical.len(), limits)?;
        }
        self.ensure_event(canonical, id, limits, metrics)
    }

    fn append_sequence(
        &mut self,
        target: SequenceTarget,
        id: &str,
        canonical_bytes: usize,
        limits: MigrationLimits,
    ) -> Result<()> {
        let (bytes, events, writer, label) = match target {
            SequenceTarget::Log => (
                &mut self.log_bytes,
                &mut self.log_events,
                &mut self.log,
                meta::LOG_FILE,
            ),
            SequenceTarget::View => (
                &mut self.view_bytes,
                &mut self.view_events,
                &mut self.view,
                meta::VIEW_FILE,
            ),
        };
        let projected_sequence = bytes
            .checked_add(canonical_bytes)
            .context("migrated sequence size overflow")?;
        anyhow::ensure!(
            projected_sequence <= limits.materialized_sequence_bytes,
            "migrated {label} exceeds its {}-byte materialized cap",
            limits.materialized_sequence_bytes
        );
        anyhow::ensure!(
            *events < limits.sequence_events,
            "migrated {label} exceeds its {}-event cap",
            limits.sequence_events
        );
        writer.write_all(id.as_bytes())?;
        writer.write_all(b"\n")?;
        *bytes = projected_sequence;
        *events += 1;
        Ok(())
    }

    fn ensure_event(
        &mut self,
        canonical: &str,
        id: &str,
        limits: MigrationLimits,
        metrics: &mut MigrationMetrics,
    ) -> Result<()> {
        anyhow::ensure!(
            canonical.len() <= limits.event_bytes,
            "event {id} exceeds its {}-byte cap",
            limits.event_bytes
        );
        let fingerprint = EventFingerprint::from_canonical(canonical);
        if let Some(existing) = self.events.get(id) {
            anyhow::ensure!(existing == &fingerprint, "event id collision for {id}");
            metrics.duplicate_event_reuses = metrics
                .duplicate_event_reuses
                .checked_add(1)
                .context("duplicate event reuse count overflow")?;
            return Ok(());
        }

        let projected = self
            .unique_event_bytes
            .checked_add(canonical.len())
            .context("unique event byte count overflow")?;
        anyhow::ensure!(
            projected <= limits.unique_event_bytes,
            "unique event bytes exceed the {}-byte snapshot cap",
            limits.unique_event_bytes
        );
        let target = meta::event_path(id)?;
        let path = self.root().join(&target);
        let parent = path.parent().context("event spool path has no parent")?;
        std::fs::create_dir_all(parent)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("cannot create migration event spool {}", path.display()))?;
        file.write_all(canonical.as_bytes())?;
        drop(file);
        self.add_blob_target(&target, &path)?;
        self.events
            .try_reserve(1)
            .context("cannot allocate migration event fingerprint cache")?;
        self.events.insert(id.to_owned(), fingerprint);
        metrics.event_spool_writes = metrics
            .event_spool_writes
            .checked_add(1)
            .context("event spool write count overflow")?;
        self.unique_event_bytes = projected;
        Ok(())
    }

    fn add_blob_target(&mut self, target: &str, source: &Path) -> Result<()> {
        anyhow::ensure!(
            !target.is_empty() && !target.contains(['\n', '\r', '\t', '\0']),
            "unsafe migration target path"
        );
        let source = source
            .strip_prefix(self.root())
            .context("migration blob source escaped its spool")?
            .to_str()
            .context("migration blob source path is not UTF-8")?;
        anyhow::ensure!(
            !source.is_empty() && !source.contains(['\n', '\r', '\0']),
            "unsafe migration blob source path"
        );
        writeln!(self.hash_sources, "{source}")?;
        writeln!(self.hash_targets, "{target}")?;
        Ok(())
    }

    fn finish_payloads(&mut self, attributes: &[u8], metadata: &[u8]) -> Result<()> {
        self.log.flush()?;
        self.view.flush()?;
        let log = self.root().join("control/log-sequence");
        let view = self.root().join("control/view-sequence");
        self.add_blob_target(meta::LOG_FILE, &log)?;
        self.add_blob_target(meta::VIEW_FILE, &view)?;
        self.write_control_blob(meta::ATTRS_FILE, "attributes", attributes)?;
        self.write_control_blob(meta::FILE, "meta", metadata)?;
        self.hash_sources.flush()?;
        self.hash_targets.flush()?;
        Ok(())
    }

    fn write_control_blob(&mut self, target: &str, name: &str, bytes: &[u8]) -> Result<()> {
        let path = self.root().join("control").join(name);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        file.write_all(bytes)?;
        drop(file);
        self.add_blob_target(target, &path)
    }
}

fn stream_legacy_snapshot(
    repo: &Repo,
    old: &str,
    limits: MigrationLimits,
    metrics: &mut MigrationMetrics,
) -> Result<LegacySnapshotSpool> {
    let mut spool = LegacySnapshotSpool::create(repo)?;
    metrics.legacy_batch_processes += 1;
    storage::visit_v0_pair_at_with_limits(
        repo.root(),
        old,
        limits.event_bytes,
        limits.legacy_blob_bytes,
        limits.sequence_events,
        |sequence, raw_bytes, canonical| {
            metrics.peak_event_buffers = metrics
                .peak_event_buffers
                .max(raw_bytes.saturating_add(canonical.len()));
            let id = storage::event_id(canonical)?;
            match sequence {
                storage::SequenceKind::Log => spool.record_log(canonical, &id, limits, metrics),
                storage::SequenceKind::View => spool.record_view(canonical, &id, limits, metrics),
            }?;
            maybe_crash_migration_after_spool();
            Ok(())
        },
    )?;
    Ok(spool)
}

#[cfg(test)]
fn maybe_crash_migration_after_spool() {
    if std::env::var("AGIT_TEST_MIGRATION_CRASH_AFTER_SPOOL").as_deref() == Ok("1") {
        // Real process termination is intentional: no TempDir/File/fs2 guard destructor runs.
        std::process::exit(87);
    }
}

#[cfg(not(test))]
fn maybe_crash_migration_after_spool() {}

fn apply_streamed_tree_edits(
    repo: &Repo,
    old: &str,
    spool: &LegacySnapshotSpool,
    metrics: &mut MigrationMetrics,
) -> Result<String> {
    let index = spool.root().join("migration.index");
    run_index_git(repo, &index, &["read-tree", old], None)?;

    let index_info_path = spool.root().join("index-info");
    let mut index_info = BufWriter::new(File::create(&index_info_path)?);
    anyhow::ensure!(
        matches!(old.len(), 40 | 64) && old.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "migration requires an immutable commit object id"
    );
    let null_oid = "0".repeat(old.len());
    for path in [meta::LEGACY_LOG_FILE, meta::LEGACY_VIEW_FILE] {
        write!(index_info, "0 {null_oid}\t{path}\0")?;
    }
    hash_spooled_blobs(repo, spool, &mut index_info, metrics)?;
    index_info.flush()?;
    drop(index_info);

    let input = File::open(index_info_path)?;
    run_index_git(
        repo,
        &index,
        &["update-index", "-z", "--index-info"],
        Some(input),
    )?;
    let tree = run_index_git(repo, &index, &["write-tree"], None)?;
    let tree = String::from_utf8(tree)?.trim().to_owned();
    anyhow::ensure!(
        matches!(tree.len(), 40 | 64) && tree.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "git write-tree returned an invalid object id"
    );
    Ok(tree)
}

fn hash_spooled_blobs(
    repo: &Repo,
    spool: &LegacySnapshotSpool,
    index_info: &mut impl Write,
    metrics: &mut MigrationMetrics,
) -> Result<()> {
    let mut command = Command::new("git");
    command.arg("--no-replace-objects").args([
        "hash-object",
        "-w",
        "--no-filters",
        "--stdin-paths",
    ]);
    hash_spooled_blobs_with_command(repo, spool, index_info, metrics, &mut command)
}

#[derive(Debug)]
struct BoundedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

fn read_bounded_output(mut reader: impl Read, limit: usize) -> std::io::Result<BoundedOutput> {
    let mut bytes = Vec::with_capacity(limit.min(8192));
    let mut truncated = false;
    let mut chunk = [0u8; 8192];
    loop {
        let count = reader.read(&mut chunk)?;
        if count == 0 {
            break;
        }
        let retained = count.min(limit.saturating_sub(bytes.len()));
        bytes.extend_from_slice(&chunk[..retained]);
        truncated |= retained != count;
    }
    Ok(BoundedOutput { bytes, truncated })
}

fn bounded_output_for_error(output: BoundedOutput, limit: usize) -> String {
    let mut message = String::from_utf8_lossy(&output.bytes).trim().to_owned();
    if output.truncated {
        if !message.is_empty() {
            message.push('\n');
        }
        message.push_str(&format!("[stderr truncated after {limit} bytes]"));
    }
    message
}

fn hash_spooled_blobs_with_command(
    repo: &Repo,
    spool: &LegacySnapshotSpool,
    index_info: &mut impl Write,
    metrics: &mut MigrationMetrics,
    command: &mut Command,
) -> Result<()> {
    let sources = File::open(spool.root().join("hash-sources"))?;
    let mut targets = BufReader::new(File::open(spool.root().join("hash-targets"))?);
    let git_dir = std::fs::canonicalize(repo.root().join(".git"))?;
    metrics.object_hash_processes += 1;
    let mut child = command
        .env("GIT_DIR", git_dir)
        .current_dir(spool.root())
        .stdin(Stdio::from(sources))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("cannot start migration object hash batch")?;
    let stdout = child
        .stdout
        .take()
        .context("migration object hash batch has no stdout")?;
    let stderr = child
        .stderr
        .take()
        .context("migration object hash batch has no stderr")?;
    // stdout is parsed synchronously below. Drain stderr in parallel: git may emit more than a
    // pipe buffer before producing stdout, while the retained diagnostic remains memory-bounded.
    let stderr_reader =
        std::thread::spawn(move || read_bounded_output(stderr, MIGRATION_STDERR_BYTES));
    let mut hashes = BufReader::new(stdout);
    let parsed = (|| -> Result<()> {
        let mut target = String::new();
        let mut hash = String::new();
        while targets.read_line(&mut target)? > 0 {
            anyhow::ensure!(target.ends_with('\n'), "unterminated migration hash target");
            let target_path = target.trim_end_matches('\n');
            anyhow::ensure!(
                !target_path.is_empty() && !target_path.contains(['\r', '\t', '\0']),
                "unsafe migration hash target"
            );
            hash.clear();
            anyhow::ensure!(
                hashes.read_line(&mut hash)? > 0,
                "migration object hash batch returned too few object ids"
            );
            anyhow::ensure!(hash.ends_with('\n'), "unterminated migration object id");
            let oid = hash.trim_end_matches('\n');
            anyhow::ensure!(
                matches!(oid.len(), 40 | 64) && oid.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "migration object hash batch returned an invalid object id"
            );
            write!(index_info, "100644 {oid}\t{target_path}\0")?;
            target.clear();
        }
        hash.clear();
        anyhow::ensure!(
            hashes.read_line(&mut hash)? == 0,
            "migration object hash batch returned too many object ids"
        );
        Ok(())
    })();
    if parsed.is_err() {
        let _ = child.kill();
    }
    let status = child
        .wait()
        .context("cannot wait for migration object hash batch")?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| anyhow::anyhow!("migration object hash stderr reader panicked"))?
        .context("cannot read migration object hash stderr")?;
    let stderr = bounded_output_for_error(stderr, MIGRATION_STDERR_BYTES);
    if let Err(error) = parsed {
        let mut context = format!("migration object hash batch output rejected ({status})");
        if !stderr.is_empty() {
            context.push_str(": ");
            context.push_str(&stderr);
        }
        return Err(error).context(context);
    }
    anyhow::ensure!(
        status.success(),
        "migration object hash batch failed ({status}){}{}",
        if stderr.is_empty() { "" } else { ": " },
        stderr
    );
    Ok(())
}

fn run_index_git(repo: &Repo, index: &Path, args: &[&str], input: Option<File>) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .arg("--no-replace-objects")
        .args(args)
        .current_dir(repo.root())
        .env("GIT_INDEX_FILE", index)
        .stdin(input.map_or_else(Stdio::null, Stdio::from))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("cannot start git {}", args.join(" ")))?;
    if !output.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

fn tree_path_exists(repo: &Repo, treeish: &str, path: &str) -> Result<bool> {
    Ok(!repo
        .git_bytes_result(&["ls-tree", "-z", "--full-name", treeish, "--", path])?
        .is_empty())
}

fn update_refs_atomically(
    repo: &Repo,
    updates: &[(String, String, String)],
) -> Result<Vec<String>> {
    let mut script = String::from("start\n");
    let mut created_recoveries = Vec::new();
    for (name, old, new) in updates {
        // The refname and the oids all come from git itself; a newline would make the
        // transaction syntax ambiguous, so this rejects it as defense in depth.
        anyhow::ensure!(
            !name.contains(['\n', '\r', ' '])
                && old.bytes().all(|b| b.is_ascii_hexdigit())
                && new.bytes().all(|b| b.is_ascii_hexdigit()),
            "unsafe ref update record"
        );
        let branch = name
            .strip_prefix("refs/heads/")
            .context("migration only accepts branch refs")?;
        let recovery = format!("refs/agit/layout-v0/{branch}");
        match optional_ref(repo, &recovery)? {
            None => {
                script.push_str(&format!("create {recovery} {old}\n"));
                created_recoveries.push(recovery);
            }
            Some(existing) if existing == *old => {
                script.push_str(&format!("verify {recovery} {old}\n"));
            }
            Some(existing) => anyhow::bail!(
                "recovery ref {recovery} already points at {existing}, not the v0 tip {old}"
            ),
        }
        script.push_str(&format!("update {name} {new} {old}\n"));
    }
    script.push_str("prepare\ncommit\n");
    super::plumbing::raw_git(repo, &["update-ref", "--stdin"], Some(&script))?;
    Ok(created_recoveries)
}

fn rollback_heads_atomically(
    repo: &Repo,
    updates: &[(String, String, String)],
    created_recoveries: &[String],
) -> Result<()> {
    let mut script = String::from("start\n");
    for (name, old, new) in updates {
        script.push_str(&format!("update {name} {old} {new}\n"));
    }
    for recovery in created_recoveries {
        let branch = recovery
            .strip_prefix("refs/agit/layout-v0/")
            .context("invalid recovery ref recorded by migration")?;
        let old = updates
            .iter()
            .find_map(|(name, old, _)| {
                (name.strip_prefix("refs/heads/") == Some(branch)).then_some(old)
            })
            .context("recovery ref has no matching branch update")?;
        script.push_str(&format!("delete {recovery} {old}\n"));
    }
    script.push_str("prepare\ncommit\n");
    super::plumbing::raw_git(repo, &["update-ref", "--stdin"], Some(&script))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::meta::{Line, Meta};

    // Byte-for-byte fixture shared with AgentGit-Backend's migration tests. Keep the parent object
    // raw and configuration-independent so both runtimes must derive the same child commit.
    const CROSS_RUNTIME_V0_META: &[u8] = b"{\"line\":\"file\",\"extension\":{\"z\":1,\"a\":{\"y\":\"\\u8de8\\u8fd0\\u884c\\u65f6\",\"b\":[3,2,1]}},\"kind\":\"file\"}\n";
    const CROSS_RUNTIME_V0_COMMIT: &str = "8f5754435d83945342213688ed2629daf550779c";
    const CROSS_RUNTIME_MIGRATED_COMMIT: &str = "1150efebbaf278d0778f7e8a21b14721d572e8cd";

    fn cross_runtime_v0_fixture(repo: &Repo) -> String {
        let raw_meta = std::str::from_utf8(CROSS_RUNTIME_V0_META).unwrap();
        let meta_oid = crate::commands::plumbing::raw_git(
            repo,
            &["hash-object", "-w", "--stdin"],
            Some(raw_meta),
        )
        .unwrap()
        .trim()
        .to_owned();
        let session_tree = crate::commands::plumbing::raw_git(
            repo,
            &["mktree"],
            Some(&format!("100644 blob {meta_oid}\tmeta.json\n")),
        )
        .unwrap()
        .trim()
        .to_owned();
        let tree = crate::commands::plumbing::raw_git(
            repo,
            &["mktree"],
            Some(&format!("040000 tree {session_tree}\tsession\n")),
        )
        .unwrap()
        .trim()
        .to_owned();
        let commit = format!(
            "tree {tree}\nauthor AgentGit fixture <fixture@agentgit.local> 1700000000 +0000\ncommitter AgentGit fixture <fixture@agentgit.local> 1700000000 +0000\n\nagit: cross-runtime storage v0 fixture\n"
        );
        crate::commands::plumbing::raw_git(
            repo,
            &["hash-object", "-t", "commit", "-w", "--stdin"],
            Some(&commit),
        )
        .unwrap()
        .trim()
        .to_owned()
    }

    fn legacy_meta(line: Line, session: &str) -> String {
        // No layout field means v0.
        serde_json::json!({
            "line": match line { Line::Session => "session", Line::File => "file" },
            "session": session,
            "runtime": if line == Line::Session { "codex" } else { "" },
            "cwd": "/work",
            "kind": "turn",
            "turn": 1,
        })
        .to_string()
            + "\n"
    }

    fn envelope(content: serde_json::Value, session: &str) -> String {
        let raw = content.to_string();
        crate::domain::transcript::wrap_lines(&format!("{raw}\n"), "codex", session)
    }

    fn legacy_session_tip(repo: &Repo, session: &str, log: &str, view: &str) -> String {
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        meta::ensure_session_dir(repo.root()).unwrap();
        std::fs::write(repo.root().join(meta::LEGACY_LOG_FILE), log).unwrap();
        std::fs::write(repo.root().join(meta::LEGACY_VIEW_FILE), view).unwrap();
        std::fs::write(
            repo.root().join(meta::FILE),
            legacy_meta(Line::Session, session),
        )
        .unwrap();
        repo.add_all().unwrap();
        repo.commit("v0 streamed fixture").unwrap();
        repo.git(&["rev-parse", "HEAD"]).unwrap()
    }

    fn migration_spools(repo: &Repo) -> Vec<PathBuf> {
        std::fs::read_dir(repo.root().join(".git"))
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(MIGRATION_SPOOL_PREFIX)
            })
            .map(|entry| entry.path())
            .collect()
    }

    fn assert_no_migration_spool(repo: &Repo) {
        assert!(
            migration_spools(repo).is_empty(),
            "migration must remove its private disk spool"
        );
    }

    fn local_repo_with_layout(
        repos: &Path,
        owner: &str,
        name: &str,
        layout: LayoutVersion,
    ) -> Repo {
        let repo = Repo::init(&repos.join(owner).join(name)).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        let mut snapshot = Meta::new_file_line();
        snapshot.layout = layout;
        meta::write(repo.root(), &snapshot).unwrap();
        repo.add_all().unwrap();
        repo.commit(match layout {
            LayoutVersion::V0 => "v0",
            LayoutVersion::V1 => "v1",
        })
        .unwrap();
        repo
    }

    #[test]
    fn discovery_caps_fail_closed_without_returning_a_partial_owner() {
        let root = tempfile::tempdir().unwrap();
        for path in [
            "a-overflow/one/.git",
            "a-overflow/two/.git",
            "b-healthy/one/.git",
        ] {
            std::fs::create_dir_all(root.path().join(path)).unwrap();
        }

        let discovery = local_repo_paths_with_limits(
            root.path(),
            DiscoveryLimits {
                owners: 8,
                repos_per_owner: 1,
                repos: 8,
            },
        )
        .unwrap();
        assert_eq!(discovery.skipped, 1);
        assert_eq!(
            discovery.paths,
            vec![root.path().join("b-healthy/one")],
            "an overflowing owner must contribute no partially enumerated repositories"
        );

        let owner_error = local_repo_paths_with_limits(
            root.path(),
            DiscoveryLimits {
                owners: 1,
                repos_per_owner: 8,
                repos: 8,
            },
        )
        .unwrap_err();
        assert!(owner_error.to_string().contains("owner discovery cap"));

        let total_error = local_repo_paths_with_limits(
            root.path(),
            DiscoveryLimits {
                owners: 8,
                repos_per_owner: 8,
                repos: 1,
            },
        )
        .unwrap_err();
        assert!(total_error.to_string().contains("repository discovery cap"));
    }

    #[cfg(unix)]
    #[test]
    fn clean_v1_startup_probe_needs_no_writable_repo_locks() {
        use std::os::unix::fs::PermissionsExt as _;

        let home = tempfile::tempdir().unwrap();
        let repos = home.path().join("repos");
        let readonly = local_repo_with_layout(&repos, "owner", "readonly", LayoutVersion::V1);
        let other = local_repo_with_layout(&repos, "owner", "status", LayoutVersion::V1);
        let git_dir = readonly.root().join(".git");
        std::fs::set_permissions(&git_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let startup = migrate_startup_at(home.path(), &repos);

        std::fs::set_permissions(&git_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(startup.unwrap(), Report::default());
        assert!(!git_dir.join(MIGRATION_SPOOL_LOCK).exists());
        assert!(!git_dir.join("agit-checkout-transaction.lock").exists());
        assert!(other.git(&["status", "--porcelain"]).unwrap().is_empty());
        assert!(
            !other
                .root()
                .join(".git")
                .join(MIGRATION_SPOOL_LOCK)
                .exists()
        );
    }

    #[test]
    fn stale_spool_on_v1_repo_triggers_locked_cleanup() {
        let home = tempfile::tempdir().unwrap();
        let repos = home.path().join("repos");
        let repo = local_repo_with_layout(&repos, "owner", "stale", LayoutVersion::V1);
        let stale = repo
            .root()
            .join(".git")
            .join(format!("{MIGRATION_SPOOL_PREFIX}stale123"));
        std::fs::create_dir(&stale).unwrap();

        assert_eq!(migrate_repo(&repo).unwrap(), 0);

        assert!(!stale.exists());
        assert!(
            repo.root()
                .join(".git")
                .join(MIGRATION_SPOOL_LOCK)
                .is_file()
        );
    }

    #[test]
    fn locked_reprobe_observes_work_completed_after_initial_probe() {
        let home = tempfile::tempdir().unwrap();
        let repos = home.path().join("repos");
        let repo = local_repo_with_layout(&repos, "owner", "raced", LayoutVersion::V1);
        let stale = repo
            .root()
            .join(".git")
            .join(format!("{MIGRATION_SPOOL_PREFIX}raced123"));
        std::fs::create_dir(&stale).unwrap();
        let initial = probe_repo_migration(&repo).unwrap();
        assert!(initial.reserved_spool && initial.needs_lock());

        // Model the previous lock owner finishing between our optimistic probe and lock acquire.
        std::fs::remove_dir(&stale).unwrap();
        assert_eq!(migrate_repo_after_probe(&repo, initial).unwrap(), 0);

        assert!(!stale.exists());
        assert!(
            repo.root()
                .join(".git")
                .join(MIGRATION_SPOOL_LOCK)
                .is_file()
        );
        assert!(
            !repo
                .root()
                .join(".git/agit-checkout-transaction.lock")
                .exists(),
            "the locked re-probe must return before unrelated recovery work"
        );
    }

    #[test]
    fn startup_waits_for_an_existing_prejournal_checkout_lock() {
        use fs2::FileExt as _;
        use std::sync::mpsc::{RecvTimeoutError, channel};
        use std::time::Duration;

        let home = tempfile::tempdir().unwrap();
        let repos = home.path().join("repos");
        let repo = local_repo_with_layout(&repos, "owner", "active", LayoutVersion::V1);
        let checkout_lock = repo.root().join(".git/agit-checkout-transaction.lock");
        let holder = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&checkout_lock)
            .unwrap();
        holder.lock_exclusive().unwrap();

        let home_path = home.path().to_owned();
        let repos_path = repos.clone();
        let (started_sender, started_receiver) = channel();
        let (sender, receiver) = channel();
        let startup = std::thread::spawn(move || {
            started_sender.send(()).unwrap();
            sender
                .send(migrate_startup_at(&home_path, &repos_path))
                .unwrap();
        });

        started_receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert!(matches!(
            receiver.recv_timeout(Duration::from_millis(150)),
            Err(RecvTimeoutError::Timeout)
        ));
        fs2::FileExt::unlock(&holder).unwrap();
        drop(holder);
        assert_eq!(
            receiver
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap(),
            Report::default()
        );
        startup.join().unwrap();
        assert!(
            !repo.root().join(".git").join(MIGRATION_SPOOL_LOCK).exists(),
            "synchronizing with an existing checkout lock must not create the migration lock"
        );
    }

    #[cfg(unix)]
    #[test]
    fn startup_isolates_permission_denied_repo_and_migrates_the_next_repo() {
        use std::os::unix::fs::PermissionsExt as _;

        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let repos = home.path().join("repos");
        let denied = local_repo_with_layout(&repos, "owner", "a-denied", LayoutVersion::V0);
        let healthy = local_repo_with_layout(&repos, "owner", "b-healthy", LayoutVersion::V0);
        let denied_git = denied.root().join(".git");
        std::fs::set_permissions(&denied_git, std::fs::Permissions::from_mode(0o555)).unwrap();

        let startup = migrate_startup_at(home.path(), &repos);

        std::fs::set_permissions(&denied_git, std::fs::Permissions::from_mode(0o755)).unwrap();
        let report = startup.unwrap();
        assert_eq!(report.skipped, 1);
        assert_eq!(report.repos, 1);
        assert_eq!(report.branches, 1);
        assert_eq!(meta::read(denied.root()).unwrap().layout, LayoutVersion::V0);
        assert_eq!(
            meta::read(healthy.root()).unwrap().layout,
            LayoutVersion::V1
        );
        assert!(healthy.git(&["status", "--porcelain"]).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn startup_isolates_unreadable_owner_and_processes_a_healthy_owner() {
        use std::os::unix::fs::PermissionsExt as _;

        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let repos = home.path().join("repos");
        let hidden = local_repo_with_layout(&repos, "a-hidden", "legacy", LayoutVersion::V0);
        let healthy = local_repo_with_layout(&repos, "b-healthy", "legacy", LayoutVersion::V0);
        let hidden_owner = repos.join("a-hidden");
        std::fs::set_permissions(&hidden_owner, std::fs::Permissions::from_mode(0o000)).unwrap();

        let startup = migrate_startup_at(home.path(), &repos);

        std::fs::set_permissions(&hidden_owner, std::fs::Permissions::from_mode(0o755)).unwrap();
        let report = startup.unwrap();
        assert_eq!(report.skipped, 1);
        assert_eq!(report.repos, 1);
        assert_eq!(report.branches, 1);
        assert_eq!(meta::read(hidden.root()).unwrap().layout, LayoutVersion::V0);
        assert_eq!(
            meta::read(healthy.root()).unwrap().layout,
            LayoutVersion::V1
        );
    }

    fn assert_startup_fails_closed_on_broken_checkout_recovery(existing_lock: bool) {
        let home = tempfile::tempdir().unwrap();
        let repos = home.path().join("repos");
        let broken = local_repo_with_layout(&repos, "owner", "a-broken", LayoutVersion::V1);
        let healthy = local_repo_with_layout(&repos, "owner", "b-healthy", LayoutVersion::V0);
        if existing_lock {
            OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .open(broken.root().join(".git/agit-checkout-transaction.lock"))
                .unwrap();
        }
        let journal = broken
            .root()
            .join(".git")
            .join("agit-checkout-transaction.json");
        std::fs::write(&journal, b"not a checkout journal\n").unwrap();

        let error = migrate_startup_at(home.path(), &repos).unwrap_err();

        assert!(
            format!("{error:#}").contains("cannot safely recover interrupted checkout"),
            "unexpected startup error: {error:#}"
        );
        assert!(
            journal.is_file(),
            "ambiguous recovery metadata must be retained"
        );
        assert_eq!(
            meta::read(healthy.root()).unwrap().layout,
            LayoutVersion::V0,
            "startup must not continue to later repositories after a recovery invariant fails"
        );
        assert!(healthy.git(&["status", "--porcelain"]).unwrap().is_empty());
    }

    #[test]
    fn startup_fails_closed_on_broken_checkout_recovery() {
        assert_startup_fails_closed_on_broken_checkout_recovery(false);
    }

    #[test]
    fn existing_checkout_barrier_keeps_broken_recovery_fatal() {
        assert_startup_fails_closed_on_broken_checkout_recovery(true);
    }

    #[test]
    fn legacy_recovery_candidate_status_failure_is_fatal() {
        let home = tempfile::tempdir().unwrap();
        let repos = home.path().join("repos");
        let broken = local_repo_with_layout(&repos, "owner", "a-broken", LayoutVersion::V0);
        let healthy = local_repo_with_layout(&repos, "owner", "b-healthy", LayoutVersion::V0);
        assert_eq!(migrate_repo(&broken).unwrap(), 1);

        std::fs::write(broken.root().join(".git/index"), b"invalid index\n").unwrap();
        let error = migrate_startup_at(home.path(), &repos).unwrap_err();

        assert!(
            format!("{error:#}").contains("cannot safely recover interrupted checkout"),
            "unexpected startup error: {error:#}"
        );
        assert_eq!(
            meta::read(healthy.root()).unwrap().layout,
            LayoutVersion::V0,
            "startup must stop before a later repository when legacy recovery cannot be proven"
        );
    }

    #[test]
    fn completed_legacy_recovery_restores_ordinary_migration_isolation() {
        let home = tempfile::tempdir().unwrap();
        let repos = home.path().join("repos");
        let recovered = local_repo_with_layout(&repos, "owner", "a-recovered", LayoutVersion::V0);
        recovered.git(&["checkout", "-b", "bad"]).unwrap();
        std::fs::write(meta::path_in(recovered.root()), b"not valid metadata\n").unwrap();
        recovered.add_all().unwrap();
        recovered.commit("bad v0 metadata").unwrap();
        recovered.git(&["checkout", "main"]).unwrap();

        let old = recovered.git(&["rev-parse", "HEAD"]).unwrap();
        let new = migrate_tip(&recovered, &old).unwrap();
        update_refs_atomically(&recovered, &[("refs/heads/main".to_owned(), old, new)]).unwrap();
        assert_eq!(
            meta::read(recovered.root()).unwrap().layout,
            LayoutVersion::V0
        );

        let healthy = local_repo_with_layout(&repos, "owner", "b-healthy", LayoutVersion::V0);
        let report = migrate_startup_at(home.path(), &repos).unwrap();

        assert_eq!(report.skipped, 1);
        assert_eq!(report.repos, 1);
        assert_eq!(report.branches, 1);
        assert_eq!(
            meta::read(recovered.root()).unwrap().layout,
            LayoutVersion::V1,
            "legacy checkout recovery must complete before the bad branch is isolated"
        );
        assert_eq!(
            meta::read(healthy.root()).unwrap().layout,
            LayoutVersion::V1
        );
    }

    #[test]
    fn migration_adds_one_commit_and_preserves_old_tag_sha() {
        let d = tempfile::tempdir().unwrap();
        let repo = Repo::init(d.path()).unwrap();
        let session = format!("agit-{}", "a".repeat(40));
        meta::ensure_session_dir(repo.root()).unwrap();
        let log = envelope(serde_json::json!({"type":"event","n":1}), &session);
        let marker_content = serde_json::json!({
            "type":"system",
            "subtype":"agit:__merge_start__"
        });
        let legacy_marker = format!(
            "{}\n",
            serde_json::json!({
                "_source":"codex",
                "_session_id":session,
                "_object_hash":crate::domain::transcript::object_hash(&marker_content),
                "content":marker_content,
            })
        );
        let canonical_marker = envelope(
            serde_json::json!({"type":"system","subtype":"agit:__merge_start__"}),
            &session,
        );
        std::fs::write(repo.root().join(meta::LEGACY_LOG_FILE), &log).unwrap();
        std::fs::write(
            repo.root().join(meta::LEGACY_VIEW_FILE),
            format!("{log}{legacy_marker}"),
        )
        .unwrap();
        std::fs::write(
            repo.root().join(meta::FILE),
            legacy_meta(Line::Session, &session),
        )
        .unwrap();
        repo.add_all().unwrap();
        repo.commit("v0").unwrap();
        let old = repo.git(&["rev-parse", "HEAD"]).unwrap();
        repo.git(&["tag", &format!("agit-{old}")]).unwrap();

        assert_eq!(migrate_repo(&repo).unwrap(), 1);
        assert!(
            repo.root()
                .join(".git")
                .join(MIGRATION_SPOOL_LOCK)
                .is_file(),
            "a v0 migration must run behind the repository migration lock"
        );
        let new = repo.git(&["rev-parse", "HEAD"]).unwrap();
        assert_ne!(new, old);
        assert_eq!(
            repo.git(&["rev-parse", &format!("agit-{old}")]).unwrap(),
            old,
            "immutable snapshot tag must keep its old commit"
        );
        assert_eq!(
            repo.git(&["rev-parse", "refs/agit/layout-v0/main"])
                .unwrap(),
            old,
            "automatic migration must leave an explicit rollback ref"
        );
        assert_eq!(meta::read(repo.root()).unwrap().layout, LayoutVersion::V1);
        assert_eq!(
            storage::materialize_worktree(repo.root(), meta::LOG_FILE).unwrap(),
            format!("{log}{canonical_marker}")
        );
        assert!(!repo.root().join(meta::LEGACY_LOG_FILE).exists());
        assert_eq!(migrate_repo(&repo).unwrap(), 0, "migration is idempotent");
    }

    /// Normally a no-op. The parent test re-execs libtest with a failpoint so process::exit leaves
    /// the private spool behind exactly as SIGKILL or power loss would.
    #[test]
    fn migration_spool_crash_child() {
        let Some(root) = std::env::var_os("AGIT_TEST_MIGRATION_REPO") else {
            return;
        };
        let repo = Repo::open(PathBuf::from(root)).unwrap();
        let result = migrate_repo(&repo);
        panic!("migration spool failpoint did not terminate the process: {result:?}");
    }

    #[test]
    fn restart_removes_hard_exit_spool_before_retrying_migration() {
        let d = tempfile::tempdir().unwrap();
        let repo = Repo::init(d.path()).unwrap();
        let session = format!("agit-{}", "e".repeat(40));
        let event = envelope(serde_json::json!({"type":"event","n":1}), &session);
        let old = legacy_session_tip(&repo, &session, &event, &event);

        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "commands::migration::tests::migration_spool_crash_child",
                "--nocapture",
            ])
            .env("AGIT_TEST_MIGRATION_REPO", repo.root())
            .env("AGIT_TEST_MIGRATION_CRASH_AFTER_SPOOL", "1")
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(87),
            "child did not hard-exit: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), old);
        let spools = migration_spools(&repo);
        assert_eq!(spools.len(), 1, "hard exit must leave one stale spool");
        let id = storage::event_id(&event).unwrap();
        assert!(spools[0].join(meta::event_path(&id).unwrap()).is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::symlink_metadata(&spools[0])
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o077,
                0,
                "canonical transcript spool must be owner-only"
            );
        }

        assert_eq!(migrate_repo(&repo).unwrap(), 1);
        assert_no_migration_spool(&repo);
        assert_eq!(meta::read(repo.root()).unwrap().layout, LayoutVersion::V1);
    }

    #[cfg(unix)]
    #[test]
    fn stale_spool_cleanup_refuses_symlinks_and_non_directories() {
        use std::os::unix::fs::symlink;

        let d = tempfile::tempdir().unwrap();
        let repo = Repo::init(d.path()).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        meta::write(repo.root(), &Meta::new_file_line()).unwrap();
        repo.add_all().unwrap();
        repo.commit("v1").unwrap();
        let git_dir = repo.root().join(".git");
        let lookalike = git_dir.join("agit-layout-v1-spoolish-user");
        std::fs::create_dir(&lookalike).unwrap();
        let outside = repo.root().join("outside-spool-target");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("sentinel"), "preserve\n").unwrap();
        let hostile = git_dir.join(format!("{MIGRATION_SPOOL_PREFIX}hostile"));
        symlink(&outside, &hostile).unwrap();

        let error = migrate_repo(&repo).unwrap_err();
        assert!(format!("{error:#}").contains("symlinked"), "{error:#}");
        assert_eq!(
            std::fs::read_to_string(outside.join("sentinel")).unwrap(),
            "preserve\n"
        );
        assert!(hostile.is_symlink());
        assert!(lookalike.is_dir(), "non-reserved lookalike must be ignored");

        std::fs::remove_file(&hostile).unwrap();
        std::fs::write(&hostile, "user file\n").unwrap();
        let error = migrate_repo(&repo).unwrap_err();
        assert!(format!("{error:#}").contains("non-directory"), "{error:#}");
        assert_eq!(std::fs::read_to_string(&hostile).unwrap(), "user file\n");
        assert!(lookalike.is_dir());
    }

    #[test]
    fn streamed_migration_uses_fixed_git_batches_for_many_events() {
        let d = tempfile::tempdir().unwrap();
        let repo = Repo::init(d.path()).unwrap();
        let session = format!("agit-{}", "c".repeat(40));
        let events: Vec<String> = (0..64)
            .map(|n| envelope(serde_json::json!({"type":"event","n":n}), &session))
            .collect();
        let log = events.concat();
        let view = events.iter().rev().cloned().collect::<String>();
        let old = legacy_session_tip(&repo, &session, &log, &view);
        let max_event = events.iter().map(String::len).max().unwrap();
        let limits = MigrationLimits {
            event_bytes: max_event,
            legacy_blob_bytes: log.len().max(view.len()),
            materialized_sequence_bytes: log.len().max(view.len()),
            sequence_events: events.len(),
            unique_event_bytes: log.len(),
        };
        let mut metrics = MigrationMetrics::default();

        let new = migrate_tip_with_limits(&repo, &old, limits, &mut metrics).unwrap();

        assert_eq!(metrics.legacy_batch_processes, 1);
        assert_eq!(metrics.object_hash_processes, 1);
        assert!(metrics.peak_event_buffers <= max_event * 2);
        assert_eq!(
            storage::materialize_pair_at(repo.root(), &new).unwrap(),
            (log, view)
        );
        assert_no_migration_spool(&repo);
    }

    #[test]
    fn streamed_migration_checks_per_sequence_and_union_caps() {
        let d = tempfile::tempdir().unwrap();
        let repo = Repo::init(d.path()).unwrap();
        let session = format!("agit-{}", "d".repeat(40));
        let in_log = envelope(serde_json::json!({"type":"event","n":1}), &session);
        let view_only = envelope(
            serde_json::json!({"type":"__merge_summary__","n":2}),
            &session,
        );
        let view = format!("{in_log}{view_only}{view_only}");
        let expected_log = format!("{in_log}{view_only}");
        let old = legacy_session_tip(&repo, &session, &in_log, &view);
        let sequence_limit = expected_log.len().max(view.len());
        let unique_limit = in_log.len() + view_only.len();
        assert!(expected_log.len() + view.len() > sequence_limit);
        let base_limits = MigrationLimits {
            event_bytes: in_log.len().max(view_only.len()),
            legacy_blob_bytes: in_log.len().max(view.len()),
            materialized_sequence_bytes: sequence_limit,
            sequence_events: 3,
            unique_event_bytes: unique_limit,
        };

        let mut rejected_metrics = MigrationMetrics::default();
        let error = migrate_tip_with_limits(
            &repo,
            &old,
            MigrationLimits {
                materialized_sequence_bytes: sequence_limit - 1,
                ..base_limits
            },
            &mut rejected_metrics,
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("materialized cap"),
            "{error:#}"
        );
        assert_eq!(rejected_metrics.legacy_batch_processes, 1);
        assert_eq!(rejected_metrics.object_hash_processes, 0);
        assert_no_migration_spool(&repo);

        let mut rejected_metrics = MigrationMetrics::default();
        let error = migrate_tip_with_limits(
            &repo,
            &old,
            MigrationLimits {
                unique_event_bytes: unique_limit - 1,
                ..base_limits
            },
            &mut rejected_metrics,
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("unique event bytes"),
            "{error:#}"
        );
        assert_eq!(rejected_metrics.legacy_batch_processes, 1);
        assert_eq!(rejected_metrics.object_hash_processes, 0);
        assert_no_migration_spool(&repo);

        let mut metrics = MigrationMetrics::default();
        let new = migrate_tip_with_limits(&repo, &old, base_limits, &mut metrics).unwrap();
        assert_eq!(metrics.legacy_batch_processes, 1);
        assert_eq!(metrics.object_hash_processes, 1);
        assert_eq!(
            storage::materialize_pair_at(repo.root(), &new).unwrap(),
            (expected_log.clone(), view.clone())
        );
        assert_eq!(expected_log.matches(&view_only).count(), 1);
        assert_no_migration_spool(&repo);
    }

    #[test]
    fn duplicate_dense_migration_spools_each_unique_event_once() {
        const REPEATS: usize = 256;

        let d = tempfile::tempdir().unwrap();
        let repo = Repo::init(d.path()).unwrap();
        let session = format!("agit-{}", "9".repeat(40));
        let event = envelope(
            serde_json::json!({"type":"event","payload":"repeated"}),
            &session,
        );
        let log = event.repeat(REPEATS);
        let view = event.repeat(REPEATS);
        let old = legacy_session_tip(&repo, &session, &log, &view);
        let limits = MigrationLimits {
            event_bytes: event.len(),
            legacy_blob_bytes: log.len(),
            materialized_sequence_bytes: log.len(),
            sequence_events: REPEATS,
            unique_event_bytes: event.len(),
        };

        let mut metrics = MigrationMetrics::default();
        let new = migrate_tip_with_limits(&repo, &old, limits, &mut metrics).unwrap();
        assert_eq!(metrics.event_spool_writes, 1);
        assert_eq!(metrics.duplicate_event_reuses, REPEATS * 2 - 1);
        assert_eq!(
            storage::materialize_pair_at(repo.root(), &new).unwrap(),
            (log, view)
        );
        assert_no_migration_spool(&repo);
    }

    #[test]
    fn duplicate_event_cache_compares_the_full_digest() {
        let d = tempfile::tempdir().unwrap();
        let repo = Repo::init(d.path()).unwrap();
        let session = format!("agit-{}", "8".repeat(40));
        let first = envelope(serde_json::json!({"type":"event","n":1}), &session);
        let same_length_other = envelope(serde_json::json!({"type":"event","n":2}), &session);
        assert_eq!(first.len(), same_length_other.len());
        let id = storage::event_id(&first).unwrap();
        let limits = MigrationLimits {
            event_bytes: first.len(),
            legacy_blob_bytes: first.len(),
            materialized_sequence_bytes: first.len(),
            sequence_events: 1,
            unique_event_bytes: first.len(),
        };
        let mut spool = LegacySnapshotSpool::create(&repo).unwrap();
        let mut metrics = MigrationMetrics::default();
        spool
            .ensure_event(&first, &id, limits, &mut metrics)
            .unwrap();

        let error = spool
            .ensure_event(&same_length_other, &id, limits, &mut metrics)
            .unwrap_err();
        assert!(format!("{error:#}").contains("event id collision"));
        assert_eq!(metrics.event_spool_writes, 1);
        assert_eq!(metrics.duplicate_event_reuses, 0);
    }

    #[cfg(unix)]
    #[test]
    fn object_hash_failure_drains_and_bounds_stderr() {
        let d = tempfile::tempdir().unwrap();
        let repo = Repo::init(d.path()).unwrap();
        let mut spool = LegacySnapshotSpool::create(&repo).unwrap();
        spool.finish_payloads(b"", b"").unwrap();
        let helper = d.path().join("failing-hash-object.sh");
        std::fs::write(
            &helper,
            "#!/bin/sh\n\
             printf 'hash-object diagnostic sentinel\\n' >&2\n\
             i=0\n\
             while [ \"$i\" -lt 9000 ]; do\n\
               printf '0123456789abcdef\\n' >&2\n\
               i=$((i + 1))\n\
             done\n\
             oid=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n\
             printf '%s\\n%s\\n%s\\n%s\\n' \"$oid\" \"$oid\" \"$oid\" \"$oid\"\n\
             exit 42\n",
        )
        .unwrap();
        // A file this binary just wrote is read by an interpreter, never exec'd: a concurrent
        // fork in another test thread inherits the write handle and the kernel refuses the exec.
        let mut command = Command::new("/bin/sh");
        command.arg(&helper);
        let mut index_info = Vec::new();
        let mut metrics = MigrationMetrics::default();

        let error = hash_spooled_blobs_with_command(
            &repo,
            &spool,
            &mut index_info,
            &mut metrics,
            &mut command,
        )
        .unwrap_err();
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("hash-object diagnostic sentinel"),
            "{rendered}"
        );
        assert!(
            rendered.contains(&format!(
                "[stderr truncated after {MIGRATION_STDERR_BYTES} bytes]"
            )),
            "missing bounded-capture marker"
        );
        assert!(rendered.len() <= MIGRATION_STDERR_BYTES + 1024);
        assert_eq!(metrics.object_hash_processes, 1);
    }

    #[test]
    fn migration_commit_is_identical_across_clones_and_local_identities() {
        let source_dir = tempfile::tempdir().unwrap();
        let source = Repo::init(source_dir.path()).unwrap();
        let raw = serde_json::json!({
            "line": "file",
            "kind": "file",
            "unknown_preserved": {"z": 1, "a": 2},
        })
        .to_string()
            + "\n";
        meta::ensure_session_dir(source.root()).unwrap();
        std::fs::write(source.root().join(meta::FILE), raw).unwrap();
        source.add_all().unwrap();
        source.commit("shared v0 parent").unwrap();
        let old = source.git(&["rev-parse", "HEAD"]).unwrap();
        let old_time = source.git(&["show", "-s", "--format=%ct", &old]).unwrap();

        let clones = tempfile::tempdir().unwrap();
        let left_path = clones.path().join("left");
        let right_path = clones.path().join("right");
        for path in [&left_path, &right_path] {
            let output = std::process::Command::new("git")
                .args(["clone", "--quiet"])
                .arg(source.root())
                .arg(path)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git clone failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let left = Repo::open(&left_path).unwrap();
        let right = Repo::open(&right_path).unwrap();
        left.git(&["config", "user.name", "Left User"]).unwrap();
        left.git(&["config", "user.email", "left@example.invalid"])
            .unwrap();
        right.git(&["config", "user.name", "Right User"]).unwrap();
        right
            .git(&["config", "user.email", "right@example.invalid"])
            .unwrap();
        left.git(&["config", "i18n.commitEncoding", "ISO-8859-1"])
            .unwrap();
        right
            .git(&["config", "i18n.commitEncoding", "UTF-16"])
            .unwrap();

        let left_new = migrate_tip(&left, &old).unwrap();
        let right_new = migrate_tip(&right, &old).unwrap();
        assert_eq!(left_new, right_new, "migration child must be deterministic");
        let raw_commit = left.git(&["cat-file", "commit", &left_new]).unwrap();
        assert!(
            !raw_commit.lines().any(|line| line.starts_with("encoding ")),
            "the canonical UTF-8 migration commit must not inherit an encoding header:\n{raw_commit}"
        );
        let migrated = left.show_raw(&left_new, meta::FILE).unwrap();
        let value: serde_json::Value = serde_json::from_str(&migrated).unwrap();
        assert_eq!(value["unknown_preserved"]["a"], 2);
        assert_eq!(
            left.git(&[
                "show",
                "-s",
                "--format=%an <%ae>|%at|%cn <%ce>|%ct",
                &left_new
            ])
            .unwrap(),
            format!(
                "AgentGit migration <migration@agentgit.local>|{old_time}|AgentGit migration <migration@agentgit.local>|{old_time}"
            )
        );
    }

    #[test]
    fn cross_runtime_file_migration_matches_golden_commit() {
        let d = tempfile::tempdir().unwrap();
        let output = std::process::Command::new("git")
            .args(["init", "--quiet", "--object-format=sha1"])
            .arg(d.path())
            // Prove that the explicit fixture format wins over a caller's process-wide default.
            .env("GIT_DEFAULT_HASH", "sha256")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "could not create the SHA-1 golden fixture: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let repo = Repo::open(d.path()).unwrap();
        let old = cross_runtime_v0_fixture(&repo);
        assert_eq!(old, CROSS_RUNTIME_V0_COMMIT);

        let migrated = migrate_tip(&repo, &old).unwrap();
        assert_eq!(migrated, CROSS_RUNTIME_MIGRATED_COMMIT);
    }

    #[test]
    fn migration_refuses_legacy_carriers_on_an_unclaimed_session_line() {
        let d = tempfile::tempdir().unwrap();
        let repo = Repo::init(d.path()).unwrap();
        let mut snapshot = Meta::new_session_line("codex".into(), "/work".into());
        snapshot.layout = LayoutVersion::V0;
        meta::write(repo.root(), &snapshot).unwrap();
        std::fs::write(repo.root().join(meta::LEGACY_LOG_FILE), "orphan log\n").unwrap();
        std::fs::write(repo.root().join(meta::LEGACY_VIEW_FILE), "orphan view\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("invalid unclaimed v0 carrier").unwrap();
        let old = repo.git(&["rev-parse", "HEAD"]).unwrap();

        let error = migrate_repo(&repo).unwrap_err();
        assert!(
            error.to_string().contains("refusing to delete"),
            "{error:#}"
        );
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), old);
        assert_eq!(
            std::fs::read_to_string(repo.root().join(meta::LEGACY_LOG_FILE)).unwrap(),
            "orphan log\n"
        );
        assert!(
            repo.git_opt(&["rev-parse", "--verify", "refs/agit/layout-v0/main"])
                .is_none()
        );
    }

    #[test]
    fn migration_refuses_legacy_carriers_on_a_file_line() {
        let d = tempfile::tempdir().unwrap();
        let repo = Repo::init(d.path()).unwrap();
        let mut snapshot = Meta::new_file_line();
        snapshot.layout = LayoutVersion::V0;
        meta::write(repo.root(), &snapshot).unwrap();
        std::fs::write(repo.root().join(meta::LEGACY_VIEW_FILE), "orphan view\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("invalid file-line carrier").unwrap();
        let old = repo.git(&["rev-parse", "HEAD"]).unwrap();

        let error = migrate_repo(&repo).unwrap_err();
        assert!(
            error.to_string().contains("refusing to delete"),
            "{error:#}"
        );
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), old);
        assert_eq!(
            std::fs::read_to_string(repo.root().join(meta::LEGACY_VIEW_FILE)).unwrap(),
            "orphan view\n"
        );
        assert!(
            repo.git_opt(&["rev-parse", "--verify", "refs/agit/layout-v0/main"])
                .is_none()
        );
    }

    #[test]
    fn migration_refuses_to_touch_a_dirty_checked_out_v0_branch() {
        let d = tempfile::tempdir().unwrap();
        let repo = Repo::init(d.path()).unwrap();
        meta::write(repo.root(), &Meta::new_file_line()).unwrap();
        // Force the fixture back to v0 on disk before its first commit.
        let text = std::fs::read_to_string(meta::path_in(repo.root()))
            .unwrap()
            .replace("\"layout\": \"v1\",\n", "");
        std::fs::write(meta::path_in(repo.root()), text).unwrap();
        std::fs::write(repo.root().join("memory.md"), "clean\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("v0 file line").unwrap();
        std::fs::write(repo.root().join("memory.md"), "user edit\n").unwrap();

        let old = repo.git(&["rev-parse", "HEAD"]).unwrap();
        let error = migrate_repo(&repo).unwrap_err().to_string();
        assert!(error.contains("uncommitted or staged"), "{error}");
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), old);
        assert_eq!(
            std::fs::read_to_string(repo.root().join("memory.md")).unwrap(),
            "user edit\n"
        );
    }

    #[test]
    fn migration_rejects_ignored_v1_namespace_collision_without_touching_it() {
        let d = tempfile::tempdir().unwrap();
        let repo = Repo::init(d.path()).unwrap();
        let mut snapshot = Meta::new_file_line();
        snapshot.layout = LayoutVersion::V0;
        meta::write(repo.root(), &snapshot).unwrap();
        std::fs::write(repo.root().join(".gitignore"), "/LOG\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("v0").unwrap();
        let old = repo.git(&["rev-parse", "HEAD"]).unwrap();
        std::fs::write(repo.root().join(meta::LOG_FILE), "ignored user data\n").unwrap();

        let error = migrate_repo(&repo).unwrap_err();
        assert!(error.to_string().contains("may be user data"), "{error:#}");
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), old);
        assert_eq!(
            std::fs::read_to_string(repo.root().join(meta::LOG_FILE)).unwrap(),
            "ignored user data\n"
        );
    }

    #[test]
    fn migration_rejects_tracked_v1_namespace_collision() {
        let d = tempfile::tempdir().unwrap();
        let repo = Repo::init(d.path()).unwrap();
        let mut snapshot = Meta::new_file_line();
        snapshot.layout = LayoutVersion::V0;
        meta::write(repo.root(), &snapshot).unwrap();
        std::fs::write(repo.root().join(meta::VIEW_FILE), "tracked user data\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("v0 with root VIEW").unwrap();
        let old = repo.git(&["rev-parse", "HEAD"]).unwrap();

        let error = migrate_repo(&repo).unwrap_err();
        assert!(error.to_string().contains("user-owned"), "{error:#}");
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), old);
        assert_eq!(
            std::fs::read_to_string(repo.root().join(meta::VIEW_FILE)).unwrap(),
            "tracked user data\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn migration_rejects_symlink_v1_namespace_collision() {
        use std::os::unix::fs::symlink;

        let d = tempfile::tempdir().unwrap();
        let repo = Repo::init(d.path()).unwrap();
        let mut snapshot = Meta::new_file_line();
        snapshot.layout = LayoutVersion::V0;
        meta::write(repo.root(), &snapshot).unwrap();
        repo.add_all().unwrap();
        repo.commit("v0").unwrap();
        let old = repo.git(&["rev-parse", "HEAD"]).unwrap();
        symlink("somewhere-private", repo.root().join(meta::EVENTS_DIR)).unwrap();

        let error = migrate_repo(&repo).unwrap_err();
        assert!(error.to_string().contains("may be user data"), "{error:#}");
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), old);
        assert_eq!(
            std::fs::read_link(repo.root().join(meta::EVENTS_DIR)).unwrap(),
            std::path::PathBuf::from("somewhere-private")
        );
    }

    #[cfg(unix)]
    #[test]
    fn migration_refuses_to_replace_tracked_symlink_attributes() {
        use std::os::unix::fs::symlink;

        let d = tempfile::tempdir().unwrap();
        let repo = Repo::init(d.path()).unwrap();
        let mut snapshot = Meta::new_file_line();
        snapshot.layout = LayoutVersion::V0;
        meta::write(repo.root(), &snapshot).unwrap();
        std::fs::write(repo.root().join("user-attributes"), "*.bin binary\n").unwrap();
        symlink("user-attributes", repo.root().join(meta::ATTRS_FILE)).unwrap();
        repo.add_all().unwrap();
        repo.commit("v0 with symlink attributes").unwrap();
        let old = repo.git(&["rev-parse", "HEAD"]).unwrap();

        let error = migrate_repo(&repo).unwrap_err();
        assert!(error.to_string().contains("non-regular"), "{error:#}");
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), old);
        assert_eq!(
            std::fs::read_link(repo.root().join(meta::ATTRS_FILE)).unwrap(),
            std::path::PathBuf::from("user-attributes")
        );
    }

    #[test]
    fn detached_v0_checkout_still_gets_collision_preflight() {
        let d = tempfile::tempdir().unwrap();
        let repo = Repo::init(d.path()).unwrap();
        let mut snapshot = Meta::new_file_line();
        snapshot.layout = LayoutVersion::V0;
        meta::write(repo.root(), &snapshot).unwrap();
        std::fs::write(repo.root().join(".gitignore"), "/LOG\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("v0").unwrap();
        let old = repo.git(&["rev-parse", "HEAD"]).unwrap();
        repo.git(&["update-ref", "--no-deref", "HEAD", &old])
            .unwrap();
        assert!(current_branch_ref(&repo).unwrap().is_none());
        std::fs::write(repo.root().join(meta::LOG_FILE), "detached ignored data\n").unwrap();

        let error = migrate_repo(&repo).unwrap_err();
        assert!(error.to_string().contains("may be user data"), "{error:#}");
        assert_eq!(repo.git(&["rev-parse", "refs/heads/main"]).unwrap(), old);
        assert_eq!(
            std::fs::read_to_string(repo.root().join(meta::LOG_FILE)).unwrap(),
            "detached ignored data\n"
        );
    }

    #[test]
    fn migration_appends_each_view_only_event_to_log_exactly_once() {
        let session = format!("agit-{}", "b".repeat(40));
        let in_log = envelope(serde_json::json!({"type":"event","n":1}), &session);
        let view_only = envelope(
            serde_json::json!({"type":"__merge_summary__","n":2}),
            &session,
        );

        let migrated =
            storage::make_view_reachable(&in_log, &format!("{in_log}{view_only}")).unwrap();
        assert_eq!(migrated, format!("{in_log}{view_only}"));
        assert_eq!(migrated.matches(&view_only).count(), 1);
    }

    #[test]
    fn failed_postflight_rollback_removes_new_recovery_ref_and_allows_retry() {
        let d = tempfile::tempdir().unwrap();
        let repo = Repo::init(d.path()).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        let mut snapshot = Meta::new_file_line();
        snapshot.layout = LayoutVersion::V0;
        meta::write(repo.root(), &snapshot).unwrap();
        repo.add_all().unwrap();
        repo.commit("v0").unwrap();
        let old = repo.git(&["rev-parse", "HEAD"]).unwrap();
        let new = migrate_tip(&repo, &old).unwrap();
        let updates = vec![("refs/heads/main".to_owned(), old.clone(), new.clone())];

        let created = update_refs_atomically(&repo, &updates).unwrap();
        assert_eq!(created, vec!["refs/agit/layout-v0/main"]);
        rollback_heads_atomically(&repo, &updates, &created).unwrap();
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), old);
        assert!(
            repo.git_opt(&["rev-parse", "--verify", "refs/agit/layout-v0/main"])
                .is_none()
        );

        std::fs::write(repo.root().join("memory.md"), "advance v0\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("advance v0").unwrap();
        assert_eq!(migrate_repo(&repo).unwrap(), 1);
        assert_eq!(meta::read(repo.root()).unwrap().layout, LayoutVersion::V1);
    }

    #[test]
    fn caught_migration_refresh_failure_removes_checkout_journal_and_retries() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repo::init(directory.path()).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        let mut snapshot = Meta::new_file_line();
        snapshot.layout = LayoutVersion::V0;
        meta::write(repo.root(), &snapshot).unwrap();
        repo.add_all().unwrap();
        repo.commit("v0").unwrap();
        let old = repo.git(&["rev-parse", "HEAD"]).unwrap();

        super::super::plumbing::fail_next_checkout_postflight();
        let error = migrate_repo(&repo).unwrap_err();
        assert!(error.to_string().contains("rolled back"), "{error:#}");
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), old);
        assert!(repo.git(&["status", "--porcelain"]).unwrap().is_empty());
        assert!(
            !repo
                .root()
                .join(".git/agit-checkout-transaction.json")
                .exists()
        );
        assert!(
            repo.git_opt(&["rev-parse", "--verify", "refs/agit/layout-v0/main"])
                .is_none()
        );

        assert_eq!(migrate_repo(&repo).unwrap(), 1);
        assert_eq!(meta::read(repo.root()).unwrap().layout, LayoutVersion::V1);
    }

    #[test]
    fn restart_finishes_checkout_after_refs_already_moved() {
        let d = tempfile::tempdir().unwrap();
        let repo = Repo::init(d.path()).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        let mut snapshot = Meta::new_file_line();
        snapshot.layout = LayoutVersion::V0;
        meta::write(repo.root(), &snapshot).unwrap();
        std::fs::write(repo.root().join("shared.md"), "preserved\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("v0").unwrap();
        let old = repo.git(&["rev-parse", "HEAD"]).unwrap();
        let new = migrate_tip(&repo, &old).unwrap();
        let updates = vec![("refs/heads/main".to_owned(), old.clone(), new.clone())];

        // Simulate the process stopping immediately after the atomic ref transaction. Git has
        // moved HEAD because it is symbolic, but neither the index nor worktree was refreshed.
        update_refs_atomically(&repo, &updates).unwrap();
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), new);
        assert_eq!(meta::read(repo.root()).unwrap().layout, LayoutVersion::V0);

        assert_eq!(migrate_repo(&repo).unwrap(), 0);
        assert_eq!(meta::read(repo.root()).unwrap().layout, LayoutVersion::V1);
        assert_eq!(
            std::fs::read_to_string(repo.root().join("shared.md")).unwrap(),
            "preserved\n"
        );
        assert!(repo.git(&["status", "--porcelain"]).unwrap().is_empty());
        assert_eq!(migrate_repo(&repo).unwrap(), 0);
    }

    #[test]
    fn interrupted_checkout_does_not_trust_meta_when_ignored_events_collide() {
        let d = tempfile::tempdir().unwrap();
        let repo = Repo::init(d.path()).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        let mut snapshot = Meta::new_file_line();
        snapshot.layout = LayoutVersion::V0;
        meta::write(repo.root(), &snapshot).unwrap();
        std::fs::write(repo.root().join(".gitignore"), "/events/\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("v0").unwrap();
        let old = repo.git(&["rev-parse", "HEAD"]).unwrap();
        let new = migrate_tip(&repo, &old).unwrap();
        update_refs_atomically(
            &repo,
            &[("refs/heads/main".to_owned(), old.clone(), new.clone())],
        )
        .unwrap();

        // Model a process that managed to write the new meta but stopped before the rest of the
        // owned namespace. An ignored path appearing under events must still be treated as user
        // data, not as proof that the whole checkout is already v1.
        let new_meta = repo.show_raw_result(&new, meta::FILE).unwrap().unwrap();
        std::fs::write(meta::path_in(repo.root()), new_meta).unwrap();
        std::fs::create_dir_all(repo.root().join("events/private")).unwrap();
        std::fs::write(
            repo.root().join("events/private/notes"),
            "ignored user data\n",
        )
        .unwrap();

        let error = migrate_repo(&repo).unwrap_err();
        assert!(
            error.to_string().contains("unexpected directory"),
            "{error:#}"
        );
        assert_eq!(
            std::fs::read_to_string(repo.root().join("events/private/notes")).unwrap(),
            "ignored user data\n"
        );
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), new);
    }
}
