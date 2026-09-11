//! A visible session branch owns file-merge exploration without changing the file-line target.
//!
//! The seed is retained before its ref can exist. Installation and merge disposition consume
//! this exact seed; branch creation alone neither installs a runtime nor authorizes settlement.

use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::commands::{new, plumbing};
use crate::domain::merge_archive::MergeArchiveRole;
use crate::domain::metadata_facts::JsonFacts;
use crate::domain::{
    archive_history, link, merge_archive, mergetx, meta, repo::Repo, store::Store,
};

const MAX_SEED_BYTES: u64 = 512 * 1024;

/// An attempt is never replayed: process creation has no atomic acknowledgement with this file.
#[derive(Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FreshLaunchAttempt {
    version: u32,
    binding: merge_archive::ExplorationBinding,
    native_path: PathBuf,
}

fn fresh_path(cwd: &Path, id: &str) -> Result<PathBuf> {
    ensure!(
        uuid::Uuid::parse_str(id)?.to_string() == id,
        "fresh Claude exploration requires its canonical native UUID"
    );
    Ok(crate::adapter::claude_code::projects_dir()?
        .join(crate::adapter::claude_code::slug_for(cwd))
        .join(format!("{id}.jsonl")))
}

pub(super) fn install_fresh_claude(cwd: &Path) -> Result<crate::adapter::Installed> {
    let id = crate::adapter::get("claude-code")?.mint_id();
    let path = fresh_path(cwd, &id)?;
    std::fs::create_dir_all(path.parent().context("native path has no directory")?)?;
    merge_archive::durable_publish_bytes(&path, b"", false)?;
    Ok(crate::adapter::Installed {
        path,
        next: crate::adapter::Next::Resume(format!("claude --session-id {id}")),
    })
}

fn attempt_path(repo: &Repo, binding: &merge_archive::ExplorationBinding) -> PathBuf {
    crate::domain::repo::common_git_dir(repo.root())
        .join(format!("AGIT_FILE_LAUNCH-{}.json", binding.role.generation))
}

fn retired_path(native: &Path, generation: &str) -> PathBuf {
    native.with_extension(format!("agit-{generation}-empty"))
}

fn selected_fresh_path(
    binding: &merge_archive::ExplorationBinding,
    selected: &link::Link,
) -> Result<PathBuf> {
    let cwd = selected
        .cwd
        .as_deref()
        .context("file exploration cwd is missing")?;
    fresh_path(Path::new(cwd), &binding.native.session_id)
}

fn require_fresh_selection(
    repo: &Repo,
    binding: &merge_archive::ExplorationBinding,
    selected: &link::Link,
) -> Result<()> {
    use sha2::{Digest, Sha256};
    let seed =
        require_seed_binding(repo, binding)?.context("fresh launch requires a file merge")?;
    ensure!(
        binding.native.runtime == "claude-code"
            && binding.installed.bytes == 0
            && binding.installed.sha256 == hex::encode(Sha256::digest([]))
            && selected.is_archive_for(
                &binding.role,
                &binding.native.runtime,
                &binding.native.session_id
            )
            && selected.baseline_bytes == Some(0)
            && selected.baseline_hash.as_ref() == Some(&binding.installed.sha256)
            && selected.materialized_from.as_ref() == Some(&binding.role.origin_head)
            && selected.cwd.as_deref() == Some(seed.cwd.as_str()),
        "fresh launch differs from its private empty installation"
    );
    Ok(())
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum FreshLaunchPoint {
    AttemptRetained,
    PlaceholderRetired,
}

/// Call under the selected branches, native Link, journal, and transaction guards before spawn.
/// A failed attempt remains abortable; an absent native file never authorizes another launch.
pub fn reserve_fresh_launch(
    repo: &Repo,
    binding: &merge_archive::ExplorationBinding,
    selected: &link::Link,
) -> Result<()> {
    let path = selected_fresh_path(binding, selected)?;
    ensure!(
        selected.resolve().as_ref() == Some(&path),
        "fresh native path differs from its installed Link"
    );
    reserve_fresh_launch_at(repo, binding, selected, &path, |_| Ok(()))
}

fn reserve_fresh_launch_at(
    repo: &Repo,
    binding: &merge_archive::ExplorationBinding,
    selected: &link::Link,
    path: &Path,
    mut checkpoint: impl FnMut(FreshLaunchPoint) -> Result<()>,
) -> Result<()> {
    require_fresh_selection(repo, binding, selected)?;
    let attempt = attempt_path(repo, binding);
    ensure!(
        merge_archive::read_private_bytes(&attempt, MAX_SEED_BYTES)?.is_none(),
        "file exploration already reserved a native launch; cancel the retained merge instead of launching it again"
    );
    let retired = retired_path(path, &binding.role.generation);
    ensure!(
        merge_archive::read_private_bytes(path, 0)?.as_deref() == Some(b"")
            && merge_archive::read_private_bytes(&retired, 0)?.is_none(),
        "fresh native launch requires its private empty placeholder without prior retirement"
    );
    let bytes = serde_json::to_vec(&FreshLaunchAttempt {
        version: 1,
        binding: binding.clone(),
        native_path: path.to_owned(),
    })?;
    ensure!(
        bytes.len() as u64 <= MAX_SEED_BYTES,
        "fresh launch receipt exceeds its limit"
    );
    merge_archive::durable_publish_bytes(&attempt, &bytes, false)?;
    checkpoint(FreshLaunchPoint::AttemptRetained)?;
    // Retaining the original carrier preserves bytes written by an already-open descriptor.
    merge_archive::durable_retire_transition_bytes(path, &retired, b"")?;
    checkpoint(FreshLaunchPoint::PlaceholderRetired)?;
    verify_fresh_launch_at(repo, binding, selected, path)
}

#[cfg(test)]
pub(super) fn reserve_fresh_launch_fixture(
    repo: &Repo,
    binding: &merge_archive::ExplorationBinding,
    selected: &link::Link,
    path: &Path,
) -> Result<()> {
    reserve_fresh_launch_at(repo, binding, selected, path, |_| Ok(()))
}

pub(super) fn verify_fresh_launch_evidence(
    repo: &Repo,
    binding: &merge_archive::ExplorationBinding,
    selected: &link::Link,
) -> Result<()> {
    if binding.file_target.is_none() || binding.native.runtime != "claude-code" {
        return Ok(());
    }
    let path = selected_fresh_path(binding, selected)?;
    if !has_fresh_attempt(repo, binding, &path)? {
        return Ok(());
    }
    ensure!(
        selected.resolve().as_ref() == Some(&path),
        "fresh native capture no longer selects its exact launched path"
    );
    verify_fresh_launch_at(repo, binding, selected, &path)
}

fn has_fresh_attempt(
    repo: &Repo,
    binding: &merge_archive::ExplorationBinding,
    path: &Path,
) -> Result<bool> {
    if merge_archive::read_private_bytes(&attempt_path(repo, binding), MAX_SEED_BYTES)?.is_none() {
        ensure!(
            merge_archive::read_private_bytes(&retired_path(path, &binding.role.generation), 0)?
                .is_none(),
            "retained native placeholder has no launch receipt; its evidence cannot be omitted"
        );
        return Ok(false);
    }
    Ok(true)
}

fn verify_fresh_launch_at(
    repo: &Repo,
    binding: &merge_archive::ExplorationBinding,
    selected: &link::Link,
    path: &Path,
) -> Result<()> {
    require_fresh_selection(repo, binding, selected)?;
    let bytes = merge_archive::read_private_bytes(&attempt_path(repo, binding), MAX_SEED_BYTES)?
        .context("fresh native launch receipt is missing")?;
    let text = std::str::from_utf8(&bytes)?;
    ensure!(
        matches!(JsonFacts::parse(text)?, JsonFacts::Object(_)),
        "fresh native launch receipt must be an object"
    );
    let attempt: FreshLaunchAttempt = serde_json::from_str(text)?;
    ensure!(
        attempt.version == 1 && attempt.binding == *binding && attempt.native_path == path,
        "fresh native launch receipt differs from its selected binding"
    );
    ensure!(
        merge_archive::read_private_bytes(&retired_path(path, &binding.role.generation), 0)?
            .as_deref()
            == Some(b""),
        "retained native placeholder is missing or contains uncaptured evidence"
    );
    Ok(())
}

/// The target ref and the session evidence ref are separate immutable selections.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceSeed {
    pub version: u32,
    pub role: MergeArchiveRole,
    pub target: String,
    pub target_head: String,
    pub runtime: String,
    pub cwd: String,
    pub tree: String,
    pub transaction_json: String,
}

impl EvidenceSeed {
    fn transaction(&self) -> Result<mergetx::Tx> {
        self.role.validate(self.target_head.len())?;
        ensure!(
            self.version == 1,
            "unsupported file exploration seed version"
        );
        crate::domain::repo::valid_branch_name(&self.target)?;
        let tx = mergetx::checked_activation_image(&self.transaction_json)?;
        ensure!(
            tx.mode == Some(mergetx::Mode::FileAgent)
                && tx.exploration.is_none()
                && tx.generation.as_ref() == Some(&self.role.generation)
                && tx.target == self.target
                && tx.target_head == self.target_head
                && self.role.branch == evidence_branch(&self.role.generation)
                && self.role.branch != self.target,
            "file exploration seed differs from its frozen target or generation"
        );
        ensure!(
            Path::new(&self.cwd).is_absolute()
                && self.cwd.len() <= 4096
                && !self.cwd.contains('\0'),
            "file exploration cwd must be an absolute bounded path"
        );
        let runtime = crate::adapter::normalize(&self.runtime)?;
        ensure!(
            runtime == self.runtime
                && crate::adapter::get(runtime)?.capability()
                    == crate::adapter::Capability::Resumable,
            "file exploration requires a canonical resumable runtime"
        );
        Ok(tx)
    }

    fn metadata(&self) -> Result<String> {
        let mut metadata = meta::Meta::new(
            self.role.logical_session.clone(),
            self.runtime.clone(),
            self.cwd.clone(),
        );
        metadata.kind = meta::Kind::File;
        metadata.milestone = Some(format!("file merge exploration for {}", self.target));
        meta::to_text(&metadata)
    }

    /// Read the immutable seed's complete storage and shared-file proof before trusting its role.
    pub fn verify(&self, repo: &Repo) -> Result<()> {
        self.transaction()?;
        archive_history::verify_file_evidence_seed(
            repo,
            &self.target_head,
            &self.role.origin_head,
            &self.tree,
            &self.metadata()?,
        )
    }
}

fn evidence_branch(generation: &str) -> String {
    format!("merge-exploration/{generation}")
}

fn seed_path(repo: &Repo, generation: &str) -> PathBuf {
    crate::domain::repo::common_git_dir(repo.root())
        .join(format!("AGIT_FILE_EXPLORATION-{generation}.json"))
}

fn require_direct_ref(repo: &Repo, branch: &str) -> Result<()> {
    let (status, _, _) =
        repo.git_status_local(&["symbolic-ref", "--quiet", &format!("refs/heads/{branch}")])?;
    ensure!(
        status == Some(1),
        "file exploration requires direct branch refs"
    );
    Ok(())
}

pub(super) fn read_seed(repo: &Repo, generation: &str) -> Result<Option<EvidenceSeed>> {
    let Some(bytes) =
        merge_archive::read_private_bytes(&seed_path(repo, generation), MAX_SEED_BYTES)?
    else {
        return Ok(None);
    };
    let text = std::str::from_utf8(&bytes)?;
    ensure!(
        matches!(JsonFacts::parse(text)?, JsonFacts::Object(_)),
        "file exploration seed must be an object"
    );
    let seed: EvidenceSeed = serde_json::from_str(text)?;
    seed.verify(repo)?;
    ensure!(
        seed.role.generation == generation,
        "file exploration carrier names another generation"
    );
    Ok(Some(seed))
}

pub(in crate::commands) fn require_seed_binding(
    repo: &Repo,
    binding: &merge_archive::ExplorationBinding,
) -> Result<Option<EvidenceSeed>> {
    let Some(target) = &binding.file_target else {
        return Ok(None);
    };
    let seed =
        read_seed(repo, &binding.role.generation)?.context("file exploration seed is missing")?;
    ensure!(
        seed.role == binding.role
            && seed.target == target.branch
            && seed.target_head == target.head
            && seed.runtime == binding.native.runtime,
        "file exploration binding differs from its retained seed"
    );
    merge_archive::checked_abort_transaction(&seed.transaction_json, binding, true)?;
    require_direct_ref(repo, &seed.target)?;
    require_direct_ref(repo, &seed.role.branch)?;
    Ok(Some(seed))
}

pub struct SeedRequest<'a> {
    pub repo: &'a Repo,
    pub store: &'a Store,
    pub slug: &'a str,
    pub transaction_json: &'a str,
    pub runtime: &'a str,
    pub cwd: &'a Path,
}

/// Creates only the named evidence ref; the current file target is verified in the same Git CAS.
pub fn prepare_seed(request: SeedRequest<'_>) -> Result<EvidenceSeed> {
    prepare_seed_with(request, |_| Ok(()))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Checkpoint {
    Prepared,
    Published,
}

fn prepare_seed_with(
    request: SeedRequest<'_>,
    mut checkpoint: impl FnMut(Checkpoint) -> Result<()>,
) -> Result<EvidenceSeed> {
    let tx = mergetx::checked_activation_image(request.transaction_json)?;
    let generation = tx
        .generation
        .as_deref()
        .context("file merge has no generation")?;
    let branch = evidence_branch(generation);
    let runtime = crate::adapter::normalize(request.runtime)?.to_owned();
    let cwd = request
        .cwd
        .to_str()
        .context("file exploration cwd is not Unicode")?
        .to_owned();
    let mut selection = EvidenceSeed {
        version: 1,
        role: MergeArchiveRole {
            generation: generation.to_owned(),
            slug: request.slug.to_owned(),
            branch: branch.clone(),
            origin_head: tx.target_head.clone(),
            logical_session: meta::mint_session_id(),
        },
        target: tx.target.clone(),
        target_head: tx.target_head.clone(),
        runtime,
        cwd,
        tree: String::new(),
        transaction_json: request.transaction_json.to_owned(),
    };
    selection.transaction()?;
    super::require_destination_routing(request.repo, &tx.target)?;
    let mut branches = [&tx.target, &branch];
    branches.sort();
    let _branches = branches
        .into_iter()
        .map(|branch| link::lock_branch(request.store, request.slug, branch))
        .collect::<Result<Vec<_>>>()?;
    let control = mergetx::ControlGuard::acquire(request.repo.root())?;
    ensure!(
        control
            .read_activation_snapshot()?
            .as_ref()
            .map(|snapshot| snapshot.json.as_str())
            == Some(request.transaction_json),
        "file merge transaction changed before evidence preparation"
    );
    super::require_destination_routing(request.repo, &tx.target)?;
    super::require_destination_routing(request.repo, &branch)?;
    require_direct_ref(request.repo, &tx.target)?;
    require_direct_ref(request.repo, &branch)?;
    ensure!(
        super::current_head(request.repo, &tx.target)? == tx.target_head,
        "file merge target moved before evidence preparation"
    );
    archive_history::verify_frozen_source(request.repo, &tx.target_head)?;
    let metadata = meta::read_at_ref_result(request.repo, &tx.target_head)?
        .context("file target metadata is missing")?;
    ensure!(
        metadata.is_file_line(),
        "file exploration target is not a file line"
    );
    let (owner, agent) = crate::commands::parse_slug(request.slug)?;
    ensure!(
        link::archive_claims_for_branch(request.store, &owner, &agent, &branch)?.is_empty(),
        "file exploration branch has an existing native claim"
    );
    let retained = read_seed(request.repo, generation)?;
    let seed = if let Some(seed) = retained {
        ensure!(
            seed.target == selection.target
                && seed.target_head == selection.target_head
                && seed.role.slug == selection.role.slug
                && seed.role.branch == selection.role.branch
                && seed.runtime == selection.runtime
                && seed.cwd == selection.cwd
                && seed.transaction_json == selection.transaction_json,
            "retained file exploration seed belongs to another selection"
        );
        seed
    } else {
        let (status, _, _) = request.repo.git_status_local(&[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])?;
        ensure!(
            status == Some(1),
            "file exploration branch already exists or cannot be inspected"
        );
        selection.tree =
            new::fresh_session_tree(request.repo, &tx.target_head, &selection.metadata()?)?;
        selection.role.origin_head = plumbing::commit_tree(
            request.repo,
            &selection.tree,
            &[&tx.target_head],
            &format!("agit: file merge exploration {branch}"),
        )?;
        selection.verify(request.repo)?;
        let bytes = serde_json::to_vec(&selection)?;
        ensure!(
            bytes.len() as u64 <= MAX_SEED_BYTES,
            "file exploration seed exceeds its byte limit"
        );
        merge_archive::durable_publish_bytes(&seed_path(request.repo, generation), &bytes, false)?;
        selection
    };
    checkpoint(Checkpoint::Prepared)?;
    require_direct_ref(request.repo, &tx.target)?;
    require_direct_ref(request.repo, &branch)?;
    let refname = format!("refs/heads/{branch}");
    let (status, _, _) = request
        .repo
        .git_status_local(&["show-ref", "--verify", "--quiet", &refname])?;
    match status {
        Some(1) => {
            plumbing::raw_git(
                request.repo,
                &["update-ref", "--stdin"],
                Some(&format!(
                    "start\noption no-deref\nverify refs/heads/{} {}\noption no-deref\ncreate {} {}\nprepare\ncommit\n",
                    tx.target, tx.target_head, refname, seed.role.origin_head
                )),
            )?;
        }
        Some(0) => {
            plumbing::raw_git(
                request.repo,
                &["update-ref", "--stdin"],
                Some(&format!(
                    "start\noption no-deref\nverify refs/heads/{} {}\noption no-deref\nverify {} {}\nprepare\ncommit\n",
                    tx.target, tx.target_head, refname, seed.role.origin_head
                )),
            )?;
        }
        _ => anyhow::bail!("file exploration ref cannot be inspected"),
    }
    checkpoint(Checkpoint::Published)?;
    require_direct_ref(request.repo, &branch)?;
    ensure!(
        super::current_head(request.repo, &branch)? == seed.role.origin_head,
        "file exploration branch moved outside its retained seed"
    );
    seed.verify(request.repo)?;
    Ok(seed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::storage;

    struct Fixture {
        _dir: tempfile::TempDir,
        repo: Repo,
        store: Store,
        tx: mergetx::Tx,
        json: String,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let repo = Repo::init(&dir.path().join("repo")).unwrap();
            repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
            meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
            std::fs::write(repo.root().join("AGENTS.md"), "Shared instructions\n").unwrap();
            std::fs::create_dir(repo.root().join("memory")).unwrap();
            std::fs::write(repo.root().join("memory/rules.md"), "Shared rule\n").unwrap();
            repo.add_all().unwrap();
            repo.commit("file target").unwrap();
            let head = repo.git(&["rev-parse", "HEAD"]).unwrap();
            repo.git(&["update-ref", "refs/heads/main", &head]).unwrap();
            repo.git(&["symbolic-ref", "HEAD", "refs/heads/main"])
                .unwrap();
            let tx = mergetx::Tx {
                mode: Some(mergetx::Mode::FileAgent),
                exploration: None,
                generation: Some(uuid::Uuid::now_v7().to_string()),
                target: "main".into(),
                source: "alice/project@source".into(),
                source_repo: Some("alice/project".into()),
                source_branch: Some("source".into()),
                base: head.clone(),
                target_head: head.clone(),
                source_head: head,
                picked: vec![],
                summary: None,
            };
            mergetx::create(repo.root(), &tx).unwrap();
            let json = mergetx::ControlGuard::acquire(repo.root())
                .unwrap()
                .read_activation_snapshot()
                .unwrap()
                .unwrap()
                .json;
            let store = Store::at(dir.path().join("store"));
            Self {
                _dir: dir,
                repo,
                store,
                tx,
                json,
            }
        }

        fn request(&self) -> SeedRequest<'_> {
            SeedRequest {
                repo: &self.repo,
                store: &self.store,
                slug: "alice/project",
                transaction_json: &self.json,
                runtime: "codex",
                cwd: self.repo.root(),
            }
        }

        fn retained(&self) -> EvidenceSeed {
            read_seed(&self.repo, self.tx.generation.as_deref().unwrap())
                .unwrap()
                .unwrap()
        }

        fn refs(&self) -> String {
            self.repo
                .git(&["for-each-ref", "--format=%(refname) %(objectname)"])
                .unwrap()
        }

        fn fresh_claude(&self) -> (merge_archive::ExplorationBinding, link::Link, PathBuf) {
            use sha2::{Digest, Sha256};
            let seed = prepare_seed(SeedRequest {
                runtime: "claude-code",
                ..self.request()
            })
            .unwrap();
            let native = merge_archive::RuntimeLinkKey {
                runtime: "claude-code".into(),
                session_id: uuid::Uuid::now_v7().to_string(),
            };
            let binding = merge_archive::ExplorationBinding {
                role: seed.role,
                native,
                installed: crate::domain::native_archive::Frontier {
                    bytes: 0,
                    sha256: hex::encode(Sha256::digest([])),
                },
                source: merge_archive::FrozenMergeSource {
                    reference: self.tx.source.clone(),
                    slug: self.tx.source_repo.clone().unwrap(),
                    branch: self.tx.source_branch.clone(),
                    head: self.tx.source_head.clone(),
                    base: Some(self.tx.base.clone()),
                },
                file_target: Some(merge_archive::FrozenFileTarget {
                    branch: seed.target,
                    head: seed.target_head,
                }),
            };
            let mut selected = link::Link::new(
                "claude-code",
                &binding.native.session_id,
                Some(self.repo.root()),
            );
            selected.owner = Some("alice".into());
            selected.agent = Some("project".into());
            selected.branch = Some(binding.role.branch.clone());
            selected.merge_archive = Some(binding.role.clone());
            selected.materialized_from = Some(binding.role.origin_head.clone());
            selected.baseline_bytes = Some(0);
            selected.baseline_hash = Some(binding.installed.sha256.clone());
            let path = self
                ._dir
                .path()
                .join(format!("{}.jsonl", binding.native.session_id));
            merge_archive::durable_publish_bytes(&path, b"", false).unwrap();
            (binding, selected, path)
        }
    }

    #[test]
    fn fresh_launch_attempt_is_not_replayed_across_either_crash_boundary() {
        for stop in [
            None,
            Some(FreshLaunchPoint::AttemptRetained),
            Some(FreshLaunchPoint::PlaceholderRetired),
        ] {
            let f = Fixture::new();
            let (binding, selected, path) = f.fresh_claude();
            let refs = f.refs();
            let result = reserve_fresh_launch_at(&f.repo, &binding, &selected, &path, |at| {
                if stop == Some(at) {
                    anyhow::bail!("injected launch interruption");
                }
                Ok(())
            });
            assert_eq!(result.is_ok(), stop.is_none());
            let attempt = std::fs::read(attempt_path(&f.repo, &binding)).unwrap();
            assert!(
                reserve_fresh_launch_at(&f.repo, &binding, &selected, &path, |_| Ok(())).is_err()
            );
            assert_eq!(
                std::fs::read(attempt_path(&f.repo, &binding)).unwrap(),
                attempt
            );
            assert_eq!(f.refs(), refs);
            if stop == Some(FreshLaunchPoint::AttemptRetained) {
                assert_eq!(std::fs::read(&path).unwrap(), b"");
                assert!(verify_fresh_launch_at(&f.repo, &binding, &selected, &path).is_err());
            } else {
                assert!(!path.exists());
                verify_fresh_launch_at(&f.repo, &binding, &selected, &path).unwrap();
            }
        }
    }

    #[test]
    fn a_retired_placeholder_preserves_open_descriptor_bytes_and_blocks_capture() {
        use std::io::Write;
        let f = Fixture::new();
        let (binding, selected, path) = f.fresh_claude();
        let mut original = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        reserve_fresh_launch_at(&f.repo, &binding, &selected, &path, |_| Ok(())).unwrap();
        merge_archive::durable_publish_bytes(
            &path,
            b"{\"type\":\"user\",\"text\":\"new session\"}\n",
            false,
        )
        .unwrap();
        verify_fresh_launch_at(&f.repo, &binding, &selected, &path).unwrap();
        original
            .write_all(b"{\"type\":\"user\",\"text\":\"retained evidence\"}\n")
            .unwrap();
        original.sync_all().unwrap();
        assert!(verify_fresh_launch_at(&f.repo, &binding, &selected, &path).is_err());
        assert_eq!(
            std::fs::read(retired_path(&path, &binding.role.generation)).unwrap(),
            b"{\"type\":\"user\",\"text\":\"retained evidence\"}\n"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"{\"type\":\"user\",\"text\":\"new session\"}\n"
        );
    }

    #[test]
    fn a_missing_attempt_cannot_hide_an_empty_or_written_retired_native_carrier() {
        for extra in [b"".as_slice(), b"retained native evidence\n".as_slice()] {
            let f = Fixture::new();
            let (binding, selected, path) = f.fresh_claude();
            assert!(!has_fresh_attempt(&f.repo, &binding, &path).unwrap());
            reserve_fresh_launch_at(&f.repo, &binding, &selected, &path, |_| Ok(())).unwrap();
            let retired = retired_path(&path, &binding.role.generation);
            std::fs::write(&retired, extra).unwrap();
            merge_archive::durable_publish_bytes(&path, b"new native evidence\n", false).unwrap();
            std::fs::remove_file(attempt_path(&f.repo, &binding)).unwrap();
            let refs = f.refs();
            let tx = std::fs::read(f.repo.git_path(mergetx::LOCK_FILE).unwrap()).unwrap();
            assert!(has_fresh_attempt(&f.repo, &binding, &path).is_err());
            assert_eq!(std::fs::read(&retired).unwrap(), extra);
            assert_eq!(std::fs::read(&path).unwrap(), b"new native evidence\n");
            assert_eq!(f.refs(), refs);
            assert_eq!(
                std::fs::read(f.repo.git_path(mergetx::LOCK_FILE).unwrap()).unwrap(),
                tx
            );
        }
    }

    #[test]
    fn fresh_launch_refuses_missing_nonempty_and_changed_identity_before_reserving() {
        for change in ["missing", "content", "role", "baseline", "cwd"] {
            let f = Fixture::new();
            let (binding, mut selected, path) = f.fresh_claude();
            match change {
                "missing" => std::fs::remove_file(&path).unwrap(),
                "content" => std::fs::write(&path, b"native evidence").unwrap(),
                "role" => selected.merge_archive = None,
                "baseline" => selected.baseline_bytes = Some(1),
                "cwd" => selected.cwd = Some(f._dir.path().to_str().unwrap().into()),
                _ => unreachable!(),
            }
            let before = std::fs::read(&path).ok();
            let refs = f.refs();
            assert!(
                reserve_fresh_launch_at(&f.repo, &binding, &selected, &path, |_| Ok(())).is_err()
            );
            assert!(!attempt_path(&f.repo, &binding).exists());
            assert!(!retired_path(&path, &binding.role.generation).exists());
            assert_eq!(std::fs::read(&path).ok(), before);
            assert_eq!(f.refs(), refs);
        }
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn public_readable_placeholder_is_not_consumed_and_private_restore_allows_first_attempt() {
        let f = Fixture::new();
        let (binding, selected, path) = f.fresh_claude();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        #[cfg(windows)]
        assert!(
            std::process::Command::new("icacls")
                .arg(&path)
                .args(["/grant", "*S-1-1-0:R"])
                .output()
                .unwrap()
                .status
                .success()
        );
        assert!(reserve_fresh_launch_at(&f.repo, &binding, &selected, &path, |_| Ok(())).is_err());
        assert!(!attempt_path(&f.repo, &binding).exists());
        assert_eq!(std::fs::read(&path).unwrap(), b"");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        #[cfg(windows)]
        assert!(
            std::process::Command::new("icacls")
                .arg(&path)
                .args(["/remove:g", "*S-1-1-0"])
                .output()
                .unwrap()
                .status
                .success()
        );
        reserve_fresh_launch_at(&f.repo, &binding, &selected, &path, |_| Ok(())).unwrap();
    }

    #[test]
    fn file_evidence_is_a_visible_empty_session_without_changing_main() {
        let f = Fixture::new();
        let files = f.repo.git(&["status", "--porcelain"]).unwrap();
        let seed = prepare_seed(f.request()).unwrap();
        seed.verify(&f.repo).unwrap();
        assert!(f.refs().contains(&format!(
            "refs/heads/{} {}",
            seed.role.branch, seed.role.origin_head
        )));
        assert_eq!(
            super::super::current_head(&f.repo, "main").unwrap(),
            f.tx.target_head
        );
        assert_eq!(
            f.repo.git(&["symbolic-ref", "HEAD"]).unwrap(),
            "refs/heads/main"
        );
        assert_eq!(f.repo.git(&["status", "--porcelain"]).unwrap(), files);
        assert!(
            meta::read_at_ref_result(&f.repo, "refs/heads/main")
                .unwrap()
                .unwrap()
                .is_file_line()
        );
        let metadata = meta::read_at_ref_result(&f.repo, &seed.role.origin_head)
            .unwrap()
            .unwrap();
        assert!(metadata.is_session_line());
        assert_eq!(metadata.session, seed.role.logical_session);
        assert_eq!(metadata.turn, None);
        for path in [meta::LOG_FILE, meta::VIEW_FILE] {
            assert_eq!(
                storage::materialize_at(f.repo.root(), &seed.role.origin_head, path).unwrap(),
                ""
            );
            assert!(!f.repo.root().join(path).exists());
        }
        assert_eq!(
            std::fs::read(f.repo.root().join("memory/rules.md")).unwrap(),
            b"Shared rule\n"
        );
        let (status, _, _) = f
            .repo
            .git_status_local(&[
                "merge-base",
                "--is-ancestor",
                &seed.role.origin_head,
                &f.tx.target_head,
            ])
            .unwrap();
        assert_eq!(
            status,
            Some(1),
            "main publication must not reach the evidence seed"
        );
    }

    #[test]
    fn retained_seed_replays_exactly_before_and_after_branch_creation() {
        for stop in [Checkpoint::Prepared, Checkpoint::Published] {
            let f = Fixture::new();
            assert!(
                prepare_seed_with(f.request(), |at| {
                    if at == stop {
                        anyhow::bail!("injected seed interruption");
                    }
                    Ok(())
                })
                .is_err()
            );
            let retained = f.retained();
            let before = std::fs::read(seed_path(&f.repo, &retained.role.generation)).unwrap();
            let retried = prepare_seed(f.request()).unwrap();
            assert_eq!(retried, retained);
            assert_eq!(
                std::fs::read(seed_path(&f.repo, &retained.role.generation)).unwrap(),
                before
            );
            assert_eq!(prepare_seed(f.request()).unwrap(), retained);
            assert_eq!(
                super::super::current_head(&f.repo, "main").unwrap(),
                f.tx.target_head
            );
        }
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn readable_private_seed_is_refused_until_its_permissions_are_restored() {
        let f = Fixture::new();
        assert!(prepare_seed_with(f.request(), |_| anyhow::bail!("stop before ref")).is_err());
        let seed = f.retained();
        let path = seed_path(&f.repo, &seed.role.generation);
        let bytes = std::fs::read(&path).unwrap();
        let refs = f.refs();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        #[cfg(windows)]
        assert!(
            std::process::Command::new("icacls")
                .arg(&path)
                .args(["/grant", "*S-1-1-0:R"])
                .output()
                .unwrap()
                .status
                .success()
        );
        assert_eq!(
            merge_archive::read_transition_bytes(&path, MAX_SEED_BYTES).unwrap(),
            Some(bytes.clone())
        );
        assert!(prepare_seed(f.request()).is_err());
        assert_eq!(f.refs(), refs);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(merge_archive::read_private_bytes(&path, MAX_SEED_BYTES).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        #[cfg(windows)]
        assert!(
            std::process::Command::new("icacls")
                .arg(&path)
                .args(["/remove:g", "*S-1-1-0"])
                .output()
                .unwrap()
                .status
                .success()
        );
        assert_eq!(prepare_seed(f.request()).unwrap(), seed);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(
            super::super::current_head(&f.repo, "main").unwrap(),
            f.tx.target_head
        );
    }

    #[test]
    fn a_target_move_in_the_prepared_window_cannot_create_the_evidence_ref() {
        let f = Fixture::new();
        let tree = f.repo.git(&["rev-parse", "HEAD^{tree}"]).unwrap();
        let moved = plumbing::commit_tree(
            &f.repo,
            &tree,
            &[&f.tx.target_head],
            "independent file change",
        )
        .unwrap();
        assert!(
            prepare_seed_with(f.request(), |at| {
                if at == Checkpoint::Prepared {
                    plumbing::update_ref_cas(
                        &f.repo,
                        "refs/heads/main",
                        &moved,
                        Some(&f.tx.target_head),
                    )?;
                }
                Ok(())
            })
            .is_err()
        );
        let seed = f.retained();
        assert!(
            !f.refs()
                .contains(&format!("refs/heads/{} ", seed.role.branch))
        );
        assert_eq!(super::super::current_head(&f.repo, "main").unwrap(), moved);
        assert!(prepare_seed(f.request()).is_err());
        plumbing::update_ref_cas(&f.repo, "refs/heads/main", &f.tx.target_head, Some(&moved))
            .unwrap();
        assert_eq!(prepare_seed(f.request()).unwrap(), seed);
    }

    #[test]
    fn existing_or_symbolic_evidence_refs_are_never_claimed_or_followed() {
        for symbolic in [false, true] {
            let f = Fixture::new();
            let branch = evidence_branch(f.tx.generation.as_deref().unwrap());
            let name = format!("refs/heads/{branch}");
            if symbolic {
                f.repo
                    .git(&["symbolic-ref", &name, "refs/heads/foreign"])
                    .unwrap();
            } else {
                f.repo
                    .git(&["update-ref", &name, &f.tx.target_head])
                    .unwrap();
            }
            let before = f.refs();
            assert!(prepare_seed(f.request()).is_err());
            assert_eq!(f.refs(), before);
            assert!(!seed_path(&f.repo, f.tx.generation.as_deref().unwrap()).exists());
            assert!(!f.repo.has_ref("refs/heads/foreign"));
            if symbolic {
                assert_eq!(
                    f.repo.git(&["symbolic-ref", &name]).unwrap(),
                    "refs/heads/foreign"
                );
            }
        }
    }

    #[test]
    fn a_symbolic_ref_inserted_after_preflight_cannot_redirect_seed_creation() {
        let f = Fixture::new();
        let branch = evidence_branch(f.tx.generation.as_deref().unwrap());
        let name = format!("refs/heads/{branch}");
        assert!(
            prepare_seed_with(f.request(), |at| {
                if at == Checkpoint::Prepared {
                    f.repo.git(&["symbolic-ref", &name, "refs/heads/foreign"])?;
                }
                Ok(())
            })
            .is_err()
        );
        assert!(!f.repo.has_ref("refs/heads/foreign"));
        assert_eq!(
            f.repo.git(&["symbolic-ref", &name]).unwrap(),
            "refs/heads/foreign"
        );
        assert_eq!(
            super::super::current_head(&f.repo, "main").unwrap(),
            f.tx.target_head
        );
        f.retained().verify(&f.repo).unwrap();
    }

    #[test]
    fn retained_selection_and_seed_contents_are_rechecked_before_replay() {
        let f = Fixture::new();
        assert!(prepare_seed_with(f.request(), |_| anyhow::bail!("stop before ref")).is_err());
        let seed = f.retained();
        let mut different = f.request();
        different.runtime = "claude-code";
        assert!(prepare_seed(different).is_err());
        let tree = plumbing::tree_apply_owned(
            &f.repo,
            &seed.tree,
            vec![("memory/rules.md".into(), Some(b"Replaced rule\n".to_vec()))],
        )
        .unwrap();
        let candidate =
            plumbing::commit_tree(&f.repo, &tree, &[&seed.target_head], "invalid seed").unwrap();
        let mut forged = seed.clone();
        forged.tree = tree;
        forged.role.origin_head = candidate;
        assert!(forged.verify(&f.repo).is_err());
        std::fs::write(
            seed_path(&f.repo, &seed.role.generation),
            serde_json::to_vec(&forged).unwrap(),
        )
        .unwrap();
        assert!(prepare_seed(f.request()).is_err());
        assert!(
            !f.refs()
                .contains(&format!("refs/heads/{} ", seed.role.branch))
        );
    }

    #[test]
    fn seed_proof_rejects_conversation_and_unexpected_parent_edges() {
        let f = Fixture::new();
        let seed = prepare_seed(f.request()).unwrap();
        for parents in [
            vec![],
            vec![seed.target_head.as_str(), seed.role.origin_head.as_str()],
        ] {
            let mut forged = seed.clone();
            forged.role.origin_head =
                plumbing::commit_tree(&f.repo, &seed.tree, &parents, "wrong parents").unwrap();
            assert!(forged.verify(&f.repo).is_err());
        }
        let wrapped = crate::domain::transcript::wrap_lines(
            "{\"type\":\"user\",\"text\":\"unexpected context\"}\n",
            "codex",
            &seed.role.logical_session,
        );
        let edits = storage::snapshot_files(&wrapped, "")
            .unwrap()
            .into_iter()
            .map(|(path, bytes)| (path, Some(bytes)))
            .collect();
        let mut forged = seed.clone();
        forged.tree = plumbing::tree_apply_owned(&f.repo, &seed.tree, edits).unwrap();
        forged.role.origin_head =
            plumbing::commit_tree(&f.repo, &forged.tree, &[&seed.target_head], "nonempty seed")
                .unwrap();
        assert!(forged.verify(&f.repo).is_err());
    }

    #[test]
    fn file_landing_proof_excludes_evidence_parents_and_session_storage() {
        let f = Fixture::new();
        let seed = prepare_seed(f.request()).unwrap();
        let source_tree = plumbing::tree_apply_owned(
            &f.repo,
            &f.tx.target_head,
            vec![(
                "memory/source.md".into(),
                Some(b"Source shared rule\n".to_vec()),
            )],
        )
        .unwrap();
        let source = plumbing::commit_tree(
            &f.repo,
            &source_tree,
            &[&f.tx.target_head],
            "source shared files",
        )
        .unwrap();
        let mut metadata = meta::Meta::new_file_line();
        metadata.kind = meta::Kind::Merge;
        let tree = plumbing::tree_apply_owned(
            &f.repo,
            &source_tree,
            vec![(
                meta::FILE.into(),
                Some(meta::to_text(&metadata).unwrap().into_bytes()),
            )],
        )
        .unwrap();
        let main =
            plumbing::commit_tree(&f.repo, &tree, &[&f.tx.target_head, &source], "file merge")
                .unwrap();
        archive_history::verify_file_merge_landing(
            &f.repo,
            &f.tx.target_head,
            &source,
            &main,
            &tree,
        )
        .unwrap();
        assert_eq!(
            f.repo
                .git_status_local(&["merge-base", "--is-ancestor", &seed.role.origin_head, &main])
                .unwrap()
                .0,
            Some(1)
        );
        let leaked = plumbing::commit_tree(
            &f.repo,
            &tree,
            &[&seed.role.origin_head, &source],
            "leaked evidence parent",
        )
        .unwrap();
        assert!(
            archive_history::verify_file_merge_landing(
                &f.repo,
                &f.tx.target_head,
                &source,
                &leaked,
                &tree
            )
            .is_err()
        );
        for path in [
            meta::LOG_FILE,
            meta::VIEW_FILE,
            meta::LEGACY_LOG_FILE,
            "events/evidence",
        ] {
            let leaked_tree = plumbing::tree_apply_owned(
                &f.repo,
                &tree,
                vec![(path.into(), Some(b"private evidence\n".to_vec()))],
            )
            .unwrap();
            let leaked = plumbing::commit_tree(
                &f.repo,
                &leaked_tree,
                &[&f.tx.target_head, &source],
                "invalid file result",
            )
            .unwrap();
            assert!(
                archive_history::verify_file_merge_landing(
                    &f.repo,
                    &f.tx.target_head,
                    &source,
                    &leaked,
                    &leaked_tree
                )
                .is_err()
            );
        }
        let session_tree = plumbing::tree_apply_owned(
            &f.repo,
            &tree,
            vec![(
                meta::FILE.into(),
                Some(seed.metadata().unwrap().into_bytes()),
            )],
        )
        .unwrap();
        let session = plumbing::commit_tree(
            &f.repo,
            &session_tree,
            &[&f.tx.target_head, &source],
            "wrong line identity",
        )
        .unwrap();
        assert!(
            archive_history::verify_file_merge_landing(
                &f.repo,
                &f.tx.target_head,
                &source,
                &session,
                &session_tree
            )
            .is_err()
        );
    }
}
