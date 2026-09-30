//! Content capture precedes remote mutation and can use a bound live advertisement for inspection.

use super::{FrozenPublication, PreparedPublication, Source, absolute_path, path_text};
use crate::domain::lfs::Pointer;
use crate::domain::repo::{
    Repo,
    publication::{InspectionScope, PublicationPlan},
};
use crate::domain::secrets::publication::{CapturedPolicy, InspectionFailure};
use crate::hub::git::StagedLfsPayloads;
use crate::hub::identity::RemoteIdentity;
use anyhow::{Context, Result, ensure};
use std::io::Read;
use std::path::{Path, PathBuf};

pub(super) struct GitContent {
    pub(super) directory: tempfile::TempDir,
    pub(super) format: String,
    pub(super) lfs_objects: std::result::Result<PathBuf, super::lfs_cache::Failure>,
    pub(super) inspection_policy: std::result::Result<CapturedPolicy, InspectionFailure>,
    pub(super) plan: PublicationPlan,
    pub(super) scope: InspectionScope,
    pub(super) baseline: Option<(String, RemoteIdentity)>,
    pub(super) lfs_inventory: Vec<Pointer>,
}

struct AdvertisedBaseline {
    url: String,
    identity: RemoteIdentity,
    refs: crate::hub::git::RemoteRefs,
}

impl GitContent {
    pub(super) fn capture(source: &Source, plan: &PublicationPlan) -> Result<Self> {
        Self::capture_for(source, plan, None)
    }

    fn capture_for(
        source: &Source,
        plan: &PublicationPlan,
        target: Option<(&str, &RemoteIdentity)>,
    ) -> Result<Self> {
        let advertised = target.and_then(|(url, identity)| {
            let refs =
                FrozenPublication::advertised_refs_for(&Repo::at(&source.root), url, identity)?;
            Some(AdvertisedBaseline {
                url: url.into(),
                identity: identity.clone(),
                refs,
            })
        });
        Self::capture_advertised(source, plan, advertised)
    }

    fn capture_advertised(
        source: &Source,
        plan: &PublicationPlan,
        advertised: Option<AdvertisedBaseline>,
    ) -> Result<Self> {
        let format = source.text(&["rev-parse", "--show-object-format"])?;
        ensure!(
            matches!(format.as_str(), "sha1" | "sha256"),
            "unsupported Git object format"
        );
        let objects = source.text(&["rev-parse", "--git-path", "objects"])?;
        let objects = absolute_path(&source.root, &objects)?;
        let objects = objects
            .canonicalize()
            .context("publication object store is unavailable")?;
        ensure!(
            objects.is_dir(),
            "publication object store is not a directory"
        );
        let lfs_objects = super::lfs_cache::capture(source);
        let inspection_policy = Self::capture_policy(source);
        let directory = private_git_directory(&format)?;
        write_alternate(directory.path(), &objects)?;
        let isolated = Repo::at(directory.path()).exact_bare_root_inspection();
        let mut baseline = None;
        let scope = advertised
            .and_then(|advertised| {
                let scope = InspectionScope::incremental(
                    &isolated,
                    plan,
                    advertised.refs.refs.into_values(),
                )
                .ok()?;
                baseline = Some((advertised.url, advertised.identity));
                Some(scope)
            })
            .unwrap_or_else(|| InspectionScope::full(plan));
        let lfs_inventory = crate::domain::lfs::history::for_inspection(&isolated, &scope)?;
        Ok(Self {
            directory,
            format,
            lfs_objects,
            inspection_policy,
            plan: plan.clone(),
            scope,
            baseline,
            lfs_inventory,
        })
    }

    fn capture_policy(source: &Source) -> std::result::Result<CapturedPolicy, InspectionFailure> {
        let common = source
            .text(&["rev-parse", "--git-common-dir"])
            .map_err(|_| InspectionFailure::LocalState)?;
        let common =
            absolute_path(&source.root, &common).map_err(|_| InspectionFailure::LocalState)?;
        let common = common
            .canonicalize()
            .map_err(|_| InspectionFailure::LocalState)?;
        CapturedPolicy::capture(&common)
    }

    fn attach(&self, source: &Source) -> Result<()> {
        ensure!(
            source.text(&["rev-parse", "--show-object-format"])? == self.format,
            "publication object format changed during audit"
        );
        let objects = source.text(&["rev-parse", "--git-path", "objects"])?;
        let objects = absolute_path(&source.root, &objects)?
            .canonicalize()
            .context("publication object store is unavailable after audit")?;
        ensure!(
            objects.is_dir(),
            "publication object store is not a directory"
        );
        write_alternate(self.directory.path(), &objects)
    }

    fn write_refs(&self) -> Result<()> {
        for reference in self.plan.heads().iter().chain(self.plan.tags()) {
            let path = self.directory.path().join(reference.name());
            let parent = path.parent().context("captured reference has no parent")?;
            std::fs::create_dir_all(parent)?;
            std::fs::write(path, format!("{}\n", reference.oid()))?;
        }
        let head = self
            .plan
            .heads()
            .first()
            .context("captured publication has no heads")?;
        std::fs::write(
            self.directory.path().join("HEAD"),
            format!("ref: {}\n", head.name()),
        )?;
        Ok(())
    }
}

pub(super) fn private_git_directory(format: &str) -> Result<tempfile::TempDir> {
    let directory = tempfile::tempdir().context("cannot create frozen Git context")?;
    for path in [
        "objects/info",
        "objects/pack",
        "refs/heads",
        "hooks",
        "home",
    ] {
        std::fs::create_dir_all(directory.path().join(path))?;
    }
    std::fs::write(directory.path().join("empty-config"), b"")?;
    std::fs::write(
        directory.path().join("HEAD"),
        b"ref: refs/heads/agit-frozen\n",
    )?;
    let config = if format == "sha256" {
        "[core]\nrepositoryformatversion = 1\nbare = true\n[extensions]\nobjectformat = sha256\n"
    } else {
        "[core]\nrepositoryformatversion = 0\nbare = true\n"
    };
    std::fs::write(directory.path().join("config"), config)?;
    Ok(directory)
}

fn write_alternate(directory: &Path, objects: &Path) -> Result<()> {
    let objects = path_text(objects)?;
    let alternate = format!(
        "\"{}\"\n",
        objects.replace('\\', "\\\\").replace('"', "\\\"")
    );
    std::fs::write(directory.join("objects/info/alternates"), alternate)?;
    Ok(())
}

/// Owns all captured payload bytes without asserting that a remote repository exists.
/// Git objects remain borrowed by immutable object ID; missing objects make inspection fail.
/// No copied report or exposed read path grants transport or publication authority.
pub struct CapturedPublication {
    pub(super) staged: Option<StagedLfsPayloads>,
    pub(super) git: GitContent,
    source: Source,
}

impl CapturedPublication {
    /// Source allowances cannot authorize disclosure to a separately selected repository.
    pub fn with_copy_policy(
        mut self,
        policy_repo: &Repo,
        identities: std::collections::HashSet<String>,
    ) -> Result<Self> {
        self.git.inspection_policy = self.git.inspection_policy.and_then(|policy| {
            policy
                .for_copy(
                    &policy_repo
                        .common_dir()
                        .map_err(|_| InspectionFailure::LocalState)?,
                    identities,
                )
                .map_err(|_| InspectionFailure::LocalState)
        });
        Ok(self)
    }

    /// Projected objects retain the source repository's registered-secret inspection policy.
    pub fn capture_projected(
        projected: &crate::domain::privacy_git::ProjectedHistory,
        byte_budget: u64,
        policy_repo: &Repo,
    ) -> Result<Self> {
        Self::capture_projected_for(projected, byte_budget, policy_repo, None)
    }

    pub fn capture_projected_for(
        projected: &crate::domain::privacy_git::ProjectedHistory,
        byte_budget: u64,
        policy_repo: &Repo,
        target: Option<(&str, &RemoteIdentity)>,
    ) -> Result<Self> {
        let mut captured = Self::capture_inner(
            projected.repo(),
            projected.plan(),
            byte_budget,
            false,
            target,
        )?;
        captured.git.inspection_policy =
            CapturedPolicy::capture(&policy_repo.common_dir()?).map(|mut policy| {
                policy.privacy_views = projected.inspection_views().clone();
                policy
            });
        Ok(captured)
    }

    /// Capture full selected history and stage every LFS pointer before contacting a destination.
    /// A remote-present payload needs the same verified local bytes as an absent payload.
    pub fn capture(repo: &Repo, plan: &PublicationPlan, byte_budget: u64) -> Result<Self> {
        Self::capture_inner(repo, plan, byte_budget, false, None)
    }

    /// Missing historical payloads are read from the pinned source into private inspection storage.
    /// Remote presence never substitutes for verified bytes or authorizes a new destination.
    pub fn capture_with_lfs_recovery(
        repo: &Repo,
        plan: &PublicationPlan,
        byte_budget: u64,
    ) -> Result<Self> {
        Self::capture_inner(repo, plan, byte_budget, true, None)
    }

    /// A verified existing destination permits incremental inspection; no target means full review.
    pub fn capture_for_destination(
        repo: &Repo,
        plan: &PublicationPlan,
        byte_budget: u64,
        target: Option<(&str, &RemoteIdentity)>,
    ) -> Result<Self> {
        Self::capture_inner(repo, plan, byte_budget, true, target)
    }

    fn capture_inner(
        repo: &Repo,
        plan: &PublicationPlan,
        byte_budget: u64,
        recover_missing: bool,
        target: Option<(&str, &RemoteIdentity)>,
    ) -> Result<Self> {
        let source = Source::new(repo)?;
        let git = GitContent::capture_for(&source, plan, target)?;
        Self::stage(repo, plan, byte_budget, recover_missing, source, git)
    }

    fn stage(
        repo: &Repo,
        plan: &PublicationPlan,
        byte_budget: u64,
        recover_missing: bool,
        source: Source,
        git: GitContent,
    ) -> Result<Self> {
        git.write_refs()?;
        let staged = if git.lfs_inventory.is_empty() {
            None
        } else {
            let objects = git
                .lfs_objects
                .as_deref()
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            crate::hub::git::frozen_lfs_stage::source_outside_directory(
                objects,
                git.directory.path(),
            )?;
            let directory = tempfile::Builder::new()
                .prefix("audit-lfs-")
                .tempdir_in(git.directory.path())
                .context("cannot create private audit payload storage")?;
            let mut recovery = None;
            let mut recovery_error = None;
            let staged = StagedLfsPayloads::stage_with_recovery(
                objects,
                &git.lfs_inventory,
                byte_budget,
                directory,
                |pointer| {
                    use crate::hub::git::LfsStagingFailure;
                    if !recover_missing {
                        return Err(LfsStagingFailure::Source);
                    }
                    let result = (|| {
                        if recovery.is_none() {
                            let identity = crate::hub::identity::read(repo)?
                                .context("missing LFS content has no pinned source repository; restore the original payload before pushing")?;
                            let url = repo.remote_url()
                                .context("missing LFS content has no source remote; restore the original payload before pushing")?;
                            recovery =
                                Some(FrozenPublication::prepare(repo, plan, &url, &identity)?);
                        }
                        recovery.as_ref().expect("recovery source is prepared")
                            .download_lfs_payload(pointer)
                            .with_context(|| format!("cannot recover historical LFS object {} from the pinned source; restore the payload or retry when the source is available", pointer.oid))
                    })();
                    result.map_err(|error| {
                        recovery_error = Some(error);
                        LfsStagingFailure::Source
                    })
                },
            );
            Some(
                staged.map_err(|error| {
                    recovery_error.unwrap_or_else(|| anyhow::anyhow!("{error}"))
                })?,
            )
        };
        Ok(Self {
            staged,
            git,
            source,
        })
    }

    pub fn plan(&self) -> &PublicationPlan {
        &self.git.plan
    }

    pub fn pointers(&self) -> &[Pointer] {
        &self.git.lfs_inventory
    }

    /// Read-only consumers may map this private bare directory into an isolated checkout.
    /// Captured refs are conveniences; readers must retain the plan's literal object IDs.
    /// Callers retain this owner and must not edit the directory or its borrowed objects.
    pub fn snapshot_git_dir(&self) -> &Path {
        self.git.directory.path()
    }

    /// This storage contains the captured payload inventory for read-only snapshot consumers.
    pub fn snapshot_lfs_storage(&self) -> Option<PathBuf> {
        self.staged.as_ref().map(StagedLfsPayloads::storage)
    }

    /// Readers see verified owned payloads, never the live cache or a remote-presence claim.
    pub fn open_payload(&self, pointer: &Pointer) -> Result<impl Read + '_> {
        let reader = self
            .staged
            .as_ref()
            .context("captured payload bytes are unavailable")?
            .open_payload(pointer)
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let bound = pointer
            .size
            .checked_add(1)
            .context("captured payload size overflow")?;
        Ok(reader.take(bound))
    }

    /// Call before any destination creation or promotion and again after a source relocation.
    /// This observation does not lock refs; publication still uses captured literal object IDs.
    pub fn verify_source(&self, repo: &Repo) -> Result<()> {
        self.plan().verify_captured(repo)
    }

    pub(super) fn refresh_policy(mut self, repo: &Repo) -> Result<Self> {
        self.verify_source(repo)?;
        let source = Source::at(repo, self.source.environment.clone())?;
        self.git.attach(&source)?;
        self.git.inspection_policy = GitContent::capture_policy(&source);
        Ok(self)
    }

    pub(super) fn bind_destination(
        self,
        repo: &Repo,
        canonical_url: &str,
        identity: &RemoteIdentity,
    ) -> Result<PreparedPublication> {
        self.bind(repo, canonical_url, identity, None)
    }

    pub(super) fn bind_destination_with_client(
        self,
        repo: &Repo,
        canonical_url: &str,
        identity: &RemoteIdentity,
        client: crate::hub::Client,
    ) -> Result<PreparedPublication> {
        self.bind(repo, canonical_url, identity, Some(client))
    }

    fn bind(
        self,
        repo: &Repo,
        canonical_url: &str,
        identity: &RemoteIdentity,
        client: Option<crate::hub::Client>,
    ) -> Result<PreparedPublication> {
        self.verify_source(repo)?;
        if let Some((url, expected)) = &self.git.baseline {
            ensure!(
                url == canonical_url && expected == identity,
                "inspection destination changed; inspect again before publishing"
            );
        }
        let Self {
            staged,
            git,
            source,
        } = self;
        let source = Source::at(repo, source.environment)?;
        let (url, client) = match client {
            Some(client) => FrozenPublication::destination_with_client(
                &source,
                canonical_url,
                identity,
                client,
            )?,
            None => FrozenPublication::destination(&source, canonical_url, identity)?,
        };
        git.attach(&source)?;
        let mut publication = FrozenPublication::from_captured(
            source,
            git,
            url,
            identity.clone(),
            Some(client),
            "http:https",
        )?;
        let upload_missing = publication.missing_lfs_payloads()?;
        Ok(PreparedPublication {
            staged,
            publication,
            upload_missing,
        })
    }
}

#[cfg(test)]
#[path = "incremental_capture_tests.rs"]
mod incremental_tests;
