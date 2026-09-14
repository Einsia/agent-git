//! Content capture is independent of remote creation, transport binding and publication consent.

use super::{FrozenPublication, PreparedPublication, Source, absolute_path, path_text};
use crate::domain::lfs::Pointer;
use crate::domain::repo::{Repo, publication::PublicationPlan};
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
    pub(super) lfs_inventory: Vec<Pointer>,
}

impl GitContent {
    pub(super) fn capture(source: &Source, plan: &PublicationPlan) -> Result<Self> {
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
        let repo = Repo::at(&source.root);
        let inspection_policy = (|| {
            use crate::domain::secrets::publication::{CapturedPolicy, InspectionFailure};
            let common = source
                .text(&["rev-parse", "--git-common-dir"])
                .map_err(|_| InspectionFailure::LocalState)?;
            let common =
                absolute_path(&source.root, &common).map_err(|_| InspectionFailure::LocalState)?;
            let common = common
                .canonicalize()
                .map_err(|_| InspectionFailure::LocalState)?;
            CapturedPolicy::capture(&common)
        })();
        let lfs_inventory = if source.gitdir == "." {
            crate::domain::lfs::history::for_bare_publication(&repo, plan)?
        } else {
            crate::domain::lfs::history::for_publication(&repo, plan)?
        };
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
        write_alternate(directory.path(), &objects)?;
        Ok(Self {
            directory,
            format,
            lfs_objects,
            inspection_policy,
            plan: plan.clone(),
            lfs_inventory,
        })
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
    /// Capture full selected history and stage every LFS pointer before contacting a destination.
    /// A remote-present payload needs the same verified local bytes as an absent payload.
    pub fn capture(repo: &Repo, plan: &PublicationPlan, byte_budget: u64) -> Result<Self> {
        let source = Source::new(repo)?;
        let git = GitContent::capture(&source, plan)?;
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
            Some(
                StagedLfsPayloads::stage(objects, &git.lfs_inventory, byte_budget, directory)
                    .map_err(|error| anyhow::anyhow!("{error}"))?,
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
        let heads = self.plan().heads();
        // The plan adds main independently of the explicitly selected session branches.
        let branches = heads
            .iter()
            .filter(|reference| heads.len() == 1 || reference.name() != "refs/heads/main")
            .map(|reference| {
                reference
                    .name()
                    .strip_prefix("refs/heads/")
                    .map(str::to_owned)
                    .context("captured branch name is invalid")
            })
            .collect::<Result<Vec<_>>>()?;
        self.plan().verify(repo, &branches)
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
