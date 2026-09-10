//! The merge transaction lock: the target branch stays locked between `agit merge` starting and
//! the merge landing.
//!
//! A lock is one JSON file in the repo's `.git/AGIT_MERGE_TX` (source branch, fork point, target
//! branch head). It lives in a file and not in memory: the merge agent is another process, maybe
//! in another terminal; and `.git/` is not part of the tree, so the lock never enters history
//! with a commit.
//!
//! Behavior while the lock is held (PRD, "merge" section):
//! * an ordinary `agit commit` on the **target branch** is always rejected, pointing at
//!   `merge --status`;
//! * automatic settlement from that merge agent session's hooks (`commit --from-hook`) is
//!   silently skipped;
//! * `merge --continue` creates the merge commit once its checks pass and unlocks; `--abort`
//!   discards and unlocks.
//!
//! # The granularity is one branch, not one repo
//!
//! The lock file is repo-level (one repo runs one merge at a time), but the only branch it blocks
//! is [`Tx::target`]. Testing whether the lock file exists rejects ordinary commits on every
//! other branch of the same repo for as long as the transaction runs, and the error text names
//! that unrelated branch as "locked". The gate goes through [`locking`]; [`is_locked`] answers
//! only "is a transaction running".

use crate::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const LOCK_FILE: &str = "AGIT_MERGE_TX";

/// The marker a merge agent session carries, of the form `<owner>/<name>@<target>`.
///
/// `agit merge` injects it into the **child process's** environment when it launches the merge
/// agent (never into its own), and it is the authoritative declaration that "this session is the
/// merge agent" — an order of magnitude more precise than the lock file, which can say only
/// "this repo has a merge running", never "it runs in my session".
pub const ENV: &str = "AGIT_MERGE_TX";

/// A launched merge agent may mutate only the transaction instance that prepared it.
pub const GENERATION_ENV: &str = "AGIT_MERGE_GENERATION";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Manual,
    SessionAgent,
    FileAgent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tx {
    /// The preparation mode is immutable and controls whether a native archive binding is required.
    #[serde(default)]
    pub mode: Option<Mode>,
    /// Native evidence belongs to an explicitly installed instance of this transaction.
    #[serde(default)]
    pub exploration: Option<crate::domain::merge_archive::ExplorationBinding>,
    /// A preparation belongs to this transaction instance even when another uses identical refs.
    #[serde(default)]
    pub generation: Option<String>,
    /// The target branch (the merge lands on it).
    pub target: String,
    /// The source ref (what the merge draws from).
    pub source: String,
    /// The source repo's full identity; a new transaction always writes it, and a lock file
    /// without it reads back as empty.
    #[serde(default)]
    pub source_repo: Option<String>,
    /// The branch the source ref resolved against. `@#n` and a repo-only source need it to
    /// re-resolve free of the cwd.
    #[serde(default)]
    pub source_branch: Option<String>,
    /// The unique common Git ancestor, or an empty string when the histories are unrelated.
    /// An absent ancestor does not establish a turn-count baseline.
    pub base: String,
    /// The target branch head when the transaction started (what the CAS compares against).
    pub target_head: String,
    /// The commit the source resolved to when the transaction started.
    pub source_head: String,
    /// The source events the merge agent has picked (the ref text in `#n` / `#n.k` form, one
    /// per entry).
    #[serde(default)]
    pub picked: Vec<String>,
    /// The merge agent's merge_summary text.
    #[serde(default)]
    pub summary: Option<String>,
}

impl Tx {
    /// Mutable progress belongs to the same creation and frozen source/target selection.
    pub fn same_instance(&self, other: &Self) -> bool {
        self.generation == other.generation
            && self.exploration == other.exploration
            && self.mode == other.mode
            && self.target == other.target
            && self.target_head == other.target_head
            && self.source == other.source
            && self.source_head == other.source_head
            && self.source_repo == other.source_repo
            && self.source_branch == other.source_branch
            && self.base == other.base
    }

    /// Human commands select the current transaction; launched agents retain their generation.
    pub fn require_agent_context(&self, slug: &str) -> Result<()> {
        let read = |name| match std::env::var(name) {
            Ok(value) => Ok(Some(value)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => {
                anyhow::bail!("{name} is not valid Unicode; refusing merge-agent authority")
            }
        };
        self.require_agent_context_values(
            slug,
            read(ENV)?.as_deref(),
            read(GENERATION_ENV)?.as_deref(),
        )
    }

    fn require_agent_context_values(
        &self,
        slug: &str,
        marker: Option<&str>,
        generation: Option<&str>,
    ) -> Result<()> {
        if marker.is_none() && generation.is_none() {
            return Ok(());
        }
        anyhow::ensure!(
            marker == Some(format!("{slug}@{}", self.target).as_str())
                && generation.is_some_and(|generation| {
                    !generation.is_empty() && self.generation.as_deref() == Some(generation)
                }),
            "this merge agent does not own the selected transaction generation; inspect the current transaction from a separate terminal before retrying"
        );
        Ok(())
    }

    /// The pick list (the raw ref text, for example `B#3..#5`).
    pub fn picked_refs(&self) -> &[String] {
        &self.picked
    }
    pub fn picked_count(&self) -> usize {
        self.picked.len()
    }
    pub fn has_summary(&self) -> bool {
        self.summary
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
    }
    pub fn summary_text(&self) -> String {
        self.summary.clone().unwrap_or_default()
    }
    /// Append to the picks (deduplicated).
    pub fn pick_more(&mut self, refs: &[String]) {
        for r in refs {
            if !self.picked.contains(r) {
                self.picked.push(r.clone());
            }
        }
    }
    /// Remove picks; returns how many were removed.
    pub fn drop(&mut self, refs: &[String]) -> usize {
        let before = self.picked.len();
        self.picked.retain(|p| !refs.contains(p));
        before - self.picked.len()
    }
    pub fn set_summary(&mut self, text: String) {
        self.summary = Some(text);
    }
}

fn lock_path(repo_root: &Path) -> PathBuf {
    // One lock per repo: a path built from a session branch's worktree must still land in the
    // shared git directory.
    crate::domain::repo::common_git_dir(repo_root).join(LOCK_FILE)
}

/// Whether this repo has an open transaction.
///
/// **Not a write gate**: it does not know which branch is locked. Use [`locking`] to decide
/// whether a branch is writable right now. It exists for one purpose — the exclusion test for
/// "one repo runs one merge at a time".
pub fn is_locked(repo_root: &Path) -> bool {
    lock_path(repo_root).exists()
}

/// Whether `branch` is the branch some transaction has locked; if it is, hands back that
/// transaction.
///
/// Returns the transaction itself and not a bool: the error text has to say which branch is
/// locked and what the source is — "{branch} is locked" is false when the lock is on another
/// branch.
pub fn locking(repo_root: &Path, branch: &str) -> Option<Tx> {
    read(repo_root)
        .ok()
        .flatten()
        .filter(|tx| tx.target == branch)
}

/// Whether automatic settlement from hooks suspends for this call.
///
/// The design suspends **a merge agent session carrying the `AGIT_MERGE_TX` marker**, so the test
/// reads that marker from the environment first — suspension is scoped to that one session and
/// not to the whole repo (other sessions on the same machine settle automatically as usual). With
/// the marker unreadable it falls back to the lock file, because a hook process does not
/// necessarily inherit the merge agent's environment; the fallback too blocks only the target
/// branch.
pub fn hook_suspended(repo_root: &Path, branch: &str) -> bool {
    match suspended_by(std::env::var(ENV).ok().as_deref(), branch) {
        Some(v) => v,
        None => locking(repo_root, branch).is_some(),
    }
}

/// The pure-function core of the marker test: `None` = the marker is unusable, hand over to the
/// fallback.
///
/// Splitting on `@` uses `split_once` and not `rsplit_once`: a slug is `owner/name` and holds no
/// `@`, while a branch name may (`feat@2`). Same approach as `AGIT_SESSION`.
fn suspended_by(marker: Option<&str>, branch: &str) -> Option<bool> {
    let m = marker?;
    let (slug, target) = m.split_once('@')?;
    if slug.is_empty() || target.is_empty() {
        return None;
    }
    Some(target == branch)
}

pub fn read(repo_root: &Path) -> Result<Option<Tx>> {
    match std::fs::read_to_string(lock_path(repo_root)) {
        Ok(t) => Ok(Some(serde_json::from_str(&t)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

const MAX_ACTIVATION_TRANSACTION_BYTES: u64 = 256 * 1024;

/// Exact transaction bytes bind archive activation without discarding unknown fields or progress.
#[derive(Debug, Clone)]
pub struct ActivationSnapshot {
    pub tx: Tx,
    pub json: String,
}

pub(crate) fn checked_activation_image(json: &str) -> Result<Tx> {
    anyhow::ensure!(
        json.len() as u64 <= MAX_ACTIVATION_TRANSACTION_BYTES,
        "archive activation transaction exceeds its byte limit"
    );
    anyhow::ensure!(
        matches!(
            crate::domain::metadata_facts::JsonFacts::parse(json)?,
            crate::domain::metadata_facts::JsonFacts::Object(_)
        ),
        "archive activation transaction must be a JSON object"
    );
    Ok(serde_json::from_str(json)?)
}

/// Serialize transaction publication, progress, cancellation, and launch admission.
/// The handle is not inherited by spawned runtimes, so cancellation never waits for their lifetime.
pub struct ControlGuard {
    path: PathBuf,
    _file: std::fs::File,
}

impl ControlGuard {
    pub fn acquire(repo_root: &Path) -> Result<Self> {
        use anyhow::Context as _;

        let path = lock_path(repo_root);
        let mut options = std::fs::OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let file = options
            .open(path.with_extension("control"))
            .context("cannot open merge transaction control")?;
        fs2::FileExt::lock_exclusive(&file).context("cannot lock merge transaction control")?;
        Ok(Self { path, _file: file })
    }

    pub fn read(&self) -> Result<Option<Tx>> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => Ok(Some(serde_json::from_str(&text)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Activation reads retain exact bytes and refuse missing, redirected or corrupt authority.
    pub fn read_activation_snapshot(&self) -> Result<Option<ActivationSnapshot>> {
        let Some(bytes) = crate::domain::merge_archive::read_transition_bytes(
            &self.path,
            MAX_ACTIVATION_TRANSACTION_BYTES,
        )?
        else {
            return Ok(None);
        };
        let json = String::from_utf8(bytes)?;
        let tx = checked_activation_image(&json)?;
        Ok(Some(ActivationSnapshot { tx, json }))
    }

    /// The caller journals both images before binding activation; replay accepts exact endpoints.
    /// Replacement persists the private successor and its parent directory before returning.
    pub fn publish_activation_binding(
        &self,
        expected_json: &str,
        planned_json: &str,
    ) -> Result<()> {
        use crate::domain::metadata_facts::JsonFacts;
        let expected = checked_activation_image(expected_json)?;
        let planned = checked_activation_image(planned_json)?;
        anyhow::ensure!(
            planned.exploration.is_some()
                && (expected.exploration.is_none() || expected.exploration == planned.exploration),
            "archive activation cannot replace another transaction binding"
        );
        let JsonFacts::Object(mut before) = JsonFacts::parse(expected_json)? else {
            unreachable!()
        };
        let JsonFacts::Object(mut after) = JsonFacts::parse(planned_json)? else {
            unreachable!()
        };
        before.remove("exploration");
        after.remove("exploration");
        anyhow::ensure!(
            before == after,
            "archive activation cannot change transaction progress or selection"
        );
        let current = self.read_activation_snapshot()?;
        anyhow::ensure!(
            current.as_ref().map(|snapshot| snapshot.json.as_str()) == Some(expected_json),
            "merge transaction changed before archive activation publication"
        );
        crate::domain::merge_archive::durable_publish_transition_bytes(
            &self.path,
            planned_json.as_bytes(),
            true,
        )
    }

    fn retained_archive_intent(&self, tx: &Tx) -> Result<bool> {
        let Some(generation) = tx.generation.as_deref() else {
            return Ok(false);
        };
        crate::domain::merge_archive::has_retained_intent(
            self.path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("transaction has no common directory"))?,
            generation,
        )
    }

    fn publish(&self, tx: &Tx, replace: bool) -> Result<()> {
        if replace && let Some(current) = self.read()? {
            if current.exploration.is_some() {
                anyhow::ensure!(
                    current.same_instance(tx),
                    "ordinary transaction publication cannot replace archive exploration authority"
                );
                let snapshot = self
                    .read_activation_snapshot()?
                    .ok_or_else(|| anyhow::anyhow!("archive transaction disappeared"))?;
                anyhow::ensure!(
                    snapshot.tx.same_instance(tx),
                    "archive transaction changed before progress publication"
                );
                crate::domain::merge_archive::require_open_progress(
                    self.path
                        .parent()
                        .ok_or_else(|| anyhow::anyhow!("transaction has no common directory"))?,
                    snapshot.tx.exploration.as_ref().ok_or_else(|| {
                        anyhow::anyhow!("archive transaction binding disappeared")
                    })?,
                )?;
                use crate::domain::metadata_facts::JsonFacts;
                let JsonFacts::Object(mut facts) = JsonFacts::parse(&snapshot.json)? else {
                    unreachable!()
                };
                facts.insert(
                    "picked".into(),
                    JsonFacts::parse(&serde_json::to_string(&tx.picked)?)?,
                );
                facts.insert(
                    "summary".into(),
                    JsonFacts::parse(&serde_json::to_string(&tx.summary)?)?,
                );
                let mut json = String::new();
                JsonFacts::Object(facts).write_json(&mut json)?;
                json.push('\n');
                checked_activation_image(&json)?;
                return crate::domain::merge_archive::durable_publish_transition_bytes(
                    &self.path,
                    json.as_bytes(),
                    true,
                );
            }
            anyhow::ensure!(
                !self.retained_archive_intent(&current)?,
                "preparing archive authority prevents ordinary transaction replacement"
            );
        }
        anyhow::ensure!(
            !self.retained_archive_intent(tx)?,
            "retained archive authority prevents ordinary transaction admission"
        );
        anyhow::ensure!(
            tx.exploration.is_none(),
            "archive exploration must be bound through durable activation"
        );

        #[cfg(windows)]
        {
            // Preparing can cancel before activation replaces this ordinary transaction.
            // Its initial carrier must already satisfy private completion ownership.
            let json = format!("{}\n", serde_json::to_string_pretty(tx)?);
            crate::domain::merge_archive::durable_publish_transition_bytes(
                &self.path,
                json.as_bytes(),
                replace,
            )
        }
        #[cfg(not(windows))]
        {
            use anyhow::Context as _;
            use std::io::Write as _;

            let mut pending = tempfile::NamedTempFile::new_in(
                self.path
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!("the transaction has no parent directory"))?,
            )?;
            writeln!(pending, "{}", serde_json::to_string_pretty(tx)?)?;
            pending.as_file().sync_all()?;
            if replace {
                pending
                    .persist(&self.path)
                    .map_err(|error| error.error)
                    .context("cannot update the merge transaction")?;
            } else {
                pending.persist_noclobber(&self.path).map_err(|error| error.error)
                    .context("cannot open the merge transaction without replacing existing state; inspect `agit merge --status`")?;
            }
            Ok(())
        }
    }

    /// Ordinary landing must recheck archive admission after acquiring transaction control.
    /// This observation takes no journal lock, preserving the archive lock order.
    pub fn require_ordinary(&self, tx: &Tx) -> Result<()> {
        anyhow::ensure!(
            tx.exploration.is_none() && !self.retained_archive_intent(tx)?,
            "retained archive authority requires archive lifecycle dispatch; retry the selected command"
        );
        Ok(())
    }

    pub fn write(&self, tx: &Tx) -> Result<()> {
        self.publish(tx, true)
    }

    pub fn remove(&self) -> Result<()> {
        if let Some(tx) = self.read()? {
            anyhow::ensure!(
                tx.exploration.is_none() && !self.retained_archive_intent(&tx)?,
                "archive exploration requires durable disposition before its transaction can be removed"
            );
        }
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// A completed archive disposition retires only its exact transaction image.
    /// Missing authority succeeds only with the matching durable completion carrier.
    pub fn complete_archive_landing(
        &self,
        binding: &crate::domain::merge_archive::ExplorationBinding,
        transaction_json: &str,
        merge_commit: &str,
    ) -> Result<()> {
        use crate::domain::merge_archive;
        merge_archive::checked_landing_transaction(transaction_json, binding)?;
        let parent = self
            .path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("transaction has no common directory"))?;
        merge_archive::require_landed_transaction(parent, binding, transaction_json, merge_commit)?;
        let retired = self
            .path
            .with_extension(format!("landed-{}.json", binding.role.generation));
        merge_archive::durable_retire_transition_bytes(
            &self.path,
            &retired,
            transaction_json.as_bytes(),
        )
    }

    /// Cancellation retires only the image whose restoration has a settled Aborted journal.
    pub fn complete_archive_abort(
        &self,
        binding: &crate::domain::merge_archive::ExplorationBinding,
        transaction_json: &str,
    ) -> Result<()> {
        use crate::domain::merge_archive;
        let parent = self
            .path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("transaction has no common directory"))?;
        merge_archive::require_aborted_transaction(parent, binding, transaction_json)?;
        let retired = self
            .path
            .with_extension(format!("aborted-{}.json", binding.role.generation));
        merge_archive::durable_retire_transition_bytes(
            &self.path,
            &retired,
            transaction_json.as_bytes(),
        )
    }
}

/// Publish a complete transaction only when no transaction already owns the repository.
/// Admission must not use the update path, which intentionally replaces existing progress.
pub fn create(repo_root: &Path, tx: &Tx) -> Result<()> {
    ControlGuard::acquire(repo_root)?.publish(tx, false)
}

pub fn lock(repo_root: &Path, tx: &Tx) -> Result<()> {
    ControlGuard::acquire(repo_root)?.write(tx)
}

pub fn unlock(repo_root: &Path) -> Result<()> {
    ControlGuard::acquire(repo_root)?.remove()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_read_unlock_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let git = d.path().join(".git");
        std::fs::create_dir_all(&git).unwrap();
        assert!(!is_locked(d.path()));
        let tx = Tx {
            mode: Some(crate::domain::mergetx::Mode::Manual),
            exploration: None,
            generation: None,
            target: "a".into(),
            source: "b".into(),
            source_repo: None,
            source_branch: None,
            base: "x".into(),
            target_head: "y".into(),
            source_head: "z".into(),
            picked: vec![],
            summary: None,
        };
        lock(d.path(), &tx).unwrap();
        assert!(is_locked(d.path()));
        assert_eq!(read(d.path()).unwrap().unwrap().source, "b");
        unlock(d.path()).unwrap();
        assert!(!is_locked(d.path()));
    }

    fn tx_on(target: &str) -> Tx {
        Tx {
            mode: Some(crate::domain::mergetx::Mode::Manual),
            exploration: None,
            generation: None,
            target: target.into(),
            source: "b".into(),
            source_repo: None,
            source_branch: None,
            base: "x".into(),
            target_head: "y".into(),
            source_head: "z".into(),
            picked: vec![],
            summary: None,
        }
    }

    #[cfg(windows)]
    fn windows_file_identity(path: &Path) -> [u32; 3] {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
        };

        let file = std::fs::File::open(path).unwrap();
        let mut facts = BY_HANDLE_FILE_INFORMATION::default();
        assert_ne!(
            unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut facts) },
            0
        );
        [
            facts.dwVolumeSerialNumber,
            facts.nFileIndexHigh,
            facts.nFileIndexLow,
        ]
    }

    #[cfg(windows)]
    #[test]
    fn ordinary_transaction_publication_is_private_before_archive_activation() {
        use crate::infra::windows_security as security;

        let temporary = tempfile::tempdir().unwrap();
        let repo = crate::domain::repo::Repo::init(&temporary.path().join("repo")).unwrap();
        let path = lock_path(repo.root());
        let mut tx = tx_on("work");
        create(repo.root(), &tx).unwrap();
        security::validate_path(&path, false, true).unwrap();
        let original = std::fs::read(&path).unwrap();
        let identity = windows_file_identity(&path);
        assert_eq!(
            original,
            format!("{}\n", serde_json::to_string_pretty(&tx).unwrap()).as_bytes()
        );
        assert!(create(repo.root(), &tx_on("other")).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(windows_file_identity(&path), identity);
        security::validate_path(&path, false, true).unwrap();

        tx.summary = Some("Synthetic ordinary progress".into());
        lock(repo.root(), &tx).unwrap();
        security::validate_path(&path, false, true).unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            format!("{}\n", serde_json::to_string_pretty(&tx).unwrap()).as_bytes()
        );
        assert_eq!(read(repo.root()).unwrap().unwrap().summary, tx.summary);
        unlock(repo.root()).unwrap();
        assert!(!path.exists());
    }

    #[cfg(windows)]
    #[test]
    fn ordinary_publication_refuses_an_unsafe_existing_transaction_without_repair() {
        use crate::infra::windows_security as security;
        use std::io::Write;
        use std::os::windows::io::FromRawHandle;
        use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
        use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
        use windows_sys::Win32::Storage::FileSystem::{
            CREATE_NEW, CreateFileW, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
            FILE_SHARE_READ, FILE_SHARE_WRITE,
        };

        let temporary = tempfile::tempdir().unwrap();
        let repo = crate::domain::repo::Repo::init(&temporary.path().join("repo")).unwrap();
        let path = lock_path(repo.root());
        let tx = tx_on("work");
        let bytes = format!("{}\n", serde_json::to_string_pretty(&tx).unwrap());
        let sid = security::current_sid().unwrap();
        let sddl = security::wide(format!("O:{sid}D:P(A;;GA;;;{sid})(A;;GW;;;WD)")).unwrap();
        let mut descriptor = std::ptr::null_mut();
        assert_ne!(
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    1,
                    &mut descriptor,
                    std::ptr::null_mut(),
                )
            },
            0
        );
        let descriptor = security::LocalAllocation(descriptor);
        let attributes = security::attributes(&descriptor);
        let name = security::wide(&path).unwrap();
        let handle = security::Handle::new(unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                &attributes,
                CREATE_NEW,
                FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null_mut(),
            )
        })
        .unwrap();
        let raw = handle.0;
        std::mem::forget(handle);
        let mut file = unsafe { std::fs::File::from_raw_handle(raw) };
        file.write_all(bytes.as_bytes()).unwrap();
        file.sync_all().unwrap();
        drop(file);
        let identity = windows_file_identity(&path);
        for replace in [false, true] {
            let control = ControlGuard::acquire(repo.root()).unwrap();
            assert!(control.publish(&tx, replace).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), bytes.as_bytes());
            assert_eq!(windows_file_identity(&path), identity);
            assert!(security::validate_path(&path, false, false).is_err());
            assert!(security::validate_path(&path, false, true).is_err());
        }
    }

    #[test]
    fn launched_agent_authority_requires_the_exact_repository_and_generation() {
        let mut transaction = tx_on("work@nested");
        transaction.generation = Some("current-generation".into());
        assert!(
            transaction
                .require_agent_context_values("me/qa", None, None)
                .is_ok()
        );
        assert!(
            transaction
                .require_agent_context_values(
                    "me/qa",
                    Some("me/qa@work@nested"),
                    Some("current-generation")
                )
                .is_ok()
        );
        for (marker, generation) in [
            (Some("me/qa@work@nested"), Some("old-generation")),
            (Some("other/qa@work@nested"), Some("current-generation")),
            (Some("me/qa@another"), Some("current-generation")),
            (Some("me/qa@work@nested"), None),
            (None, Some("current-generation")),
            (Some("me/qa@work@nested"), Some("")),
            (Some(""), Some("current-generation")),
        ] {
            assert!(
                transaction
                    .require_agent_context_values("me/qa", marker, generation)
                    .is_err()
            );
        }
        transaction.generation = None;
        assert!(
            transaction
                .require_agent_context_values("me/qa", None, None)
                .is_ok()
        );
        assert!(
            transaction
                .require_agent_context_values(
                    "me/qa",
                    Some("me/qa@work@nested"),
                    Some("current-generation")
                )
                .is_err()
        );
    }

    #[test]
    fn creation_preserves_existing_progress_and_unreadable_state() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join(".git")).unwrap();
        let mut original = tx_on("target");
        original.picked.push("source#1".into());
        original.summary = Some("preserve the reconciliation".into());
        create(directory.path(), &original).unwrap();
        let path = lock_path(directory.path());
        let before = std::fs::read(&path).unwrap();
        assert!(create(directory.path(), &tx_on("other")).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        original.picked.push("source#2".into());
        lock(directory.path(), &original).unwrap();
        assert_eq!(
            read(directory.path()).unwrap().unwrap().picked,
            original.picked
        );
        std::fs::write(&path, "unreadable transaction").unwrap();
        assert!(create(directory.path(), &tx_on("other")).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"unreadable transaction");
    }

    #[test]
    fn concurrent_creators_publish_only_the_winning_transaction() {
        use std::sync::{Arc, Barrier};

        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join(".git")).unwrap();
        let barrier = Arc::new(Barrier::new(8));
        let winners = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|index| {
                    let barrier = barrier.clone();
                    let root = directory.path();
                    scope.spawn(move || {
                        let target = format!("target-{index}");
                        let mut transaction = tx_on(&target);
                        transaction.picked.push(format!("source-{index}#1"));
                        transaction.summary = Some(format!("intent from {index}"));
                        barrier.wait();
                        create(root, &transaction).is_ok().then_some(transaction)
                    })
                })
                .collect();
            handles
                .into_iter()
                .filter_map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(winners.len(), 1);
        let actual = read(directory.path()).unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(&winners[0]).unwrap()
        );
    }

    #[test]
    fn legacy_transactions_remain_readable_without_generation() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join(".git")).unwrap();
        let mut legacy = serde_json::to_value(tx_on("target")).unwrap();
        legacy.as_object_mut().unwrap().remove("generation");
        std::fs::write(
            lock_path(directory.path()),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();
        let control = ControlGuard::acquire(directory.path()).unwrap();
        let mut transaction = control.read().unwrap().unwrap();
        assert!(transaction.generation.is_none());
        transaction.pick_more(&["source#1".into()]);
        control.write(&transaction).unwrap();
        assert_eq!(control.read().unwrap().unwrap().picked, transaction.picked);
        control.remove().unwrap();
        assert!(control.read().unwrap().is_none());
    }

    /// The lock blocks the target branch only: other branches of the same repo stay writable.
    #[test]
    fn the_lock_only_covers_the_target_branch() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".git")).unwrap();
        lock(d.path(), &tx_on("main")).unwrap();

        assert!(
            locking(d.path(), "main").is_some(),
            "the target branch must be blocked"
        );
        assert!(
            locking(d.path(), "other").is_none(),
            "the transaction locks main; blocking other along with it is the bug itself"
        );
        // The exclusion test still sees this transaction.
        assert!(is_locked(d.path()));
    }

    /// Hook suspension reads the session marker first: the scope is that merge agent, not the
    /// whole repo.
    #[test]
    fn hook_suspension_reads_the_session_marker_first() {
        assert_eq!(suspended_by(Some("me/repo@main"), "main"), Some(true));
        assert_eq!(suspended_by(Some("me/repo@main"), "other"), Some(false));
        // A branch name holding `@` still splits correctly (a slug has no `@`, so split from
        // the left).
        assert_eq!(suspended_by(Some("me/repo@feat@2"), "feat@2"), Some(true));
        // An unusable marker → None, and the caller falls back to the lock file.
        assert_eq!(suspended_by(None, "main"), None);
        assert_eq!(suspended_by(Some("garbage"), "main"), None);
        assert_eq!(suspended_by(Some("me/repo@"), "main"), None);
    }

    /// With no marker the fallback is the lock file, and the fallback too blocks only the
    /// target branch.
    #[test]
    fn hook_suspension_falls_back_to_the_lock_file_per_branch() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".git")).unwrap();
        lock(d.path(), &tx_on("main")).unwrap();
        assert!(locking(d.path(), "main").is_some());
        assert!(locking(d.path(), "side").is_none());
    }
}
