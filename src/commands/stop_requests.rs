//! Ordinary linked Stops hand immutable input evidence to a recoverable worker.

use super::commit::{QueuedStop, StopReceipt, StopWithheld};
use crate::domain::link::{self, Link};
use crate::domain::repo::Repo;
use crate::domain::session_privacy::AccountIdentity;
use crate::domain::stop_requests::{self as journal, Authority, BlockedBy, MemoryOutcome, Phase};
use crate::domain::store::Store;
use crate::hub::identity::{self, RemoteIdentity};
use crate::infra::{config, credentials};
use anyhow::{Context as _, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Input {
    pub(super) bytes: usize,
    pub(super) sha256: String,
    pub(super) receipt: Option<StopReceipt>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Pending {
    pub(super) authority: Authority,
    pub(super) epoch: Option<String>,
    pub(super) policy_epochs: BTreeMap<String, Option<String>>,
    pub(super) project_revisions: BTreeMap<PathBuf, (u64, u64)>,
    pub(super) cwd: PathBuf,
    pub(super) account: Option<AccountIdentity>,
    pub(super) hub: String,
    pub(super) author: String,
    pub(super) repository: String,
    pub(super) branch: String,
    pub(super) common_directory: PathBuf,
    pub(super) remote: Option<RemoteIdentity>,
    pub(super) source_path: PathBuf,
    pub(super) input: Input,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) project: Option<super::project::stops::Intent>,
}

impl Pending {
    fn verify_queue(&self, store: &Store, queue: &journal::Journal) -> crate::Result<()> {
        let expected = match &self.project {
            Some(project) => super::project::stops::queue(&project.root)?,
            None => journal::for_session(store, &self.authority.runtime, &self.authority.session),
        };
        ensure!(
            expected.directory() == queue.directory(),
            "Stop request belongs to another queue"
        );
        Ok(())
    }

    fn route(&self, store: &Store) -> crate::Result<(Authority, Option<String>)> {
        match &self.project {
            Some(project) => project.route(store, self),
            None => Ok((self.authority.clone(), self.epoch.clone())),
        }
    }

    pub(super) fn destination_branch(&self, store: &Store) -> crate::Result<String> {
        self.route(store)?
            .0
            .branch
            .context("queued Stop has no proven branch claim")
    }

    pub(super) fn policies_current(&self) -> crate::Result<bool> {
        for (scope, expected) in &self.policy_epochs {
            if journal::policy_epoch(scope)? != *expected {
                return Ok(false);
            }
        }
        Ok(super::project::stop_revisions(
            &self.cwd,
            &self.authority.runtime,
            &self.authority.session,
        )? == self.project_revisions)
    }

    pub(super) fn account_current(&self) -> crate::Result<bool> {
        Ok(credentials::current_account()? == self.account
            && identity::normalize_hub(&config::hub_url())? == self.hub
            && credentials::current_user().as_deref() == Some(&self.author))
    }

    fn route_current(&self, store: &Store, current: &Link) -> crate::Result<bool> {
        let (authority, epoch) = self.route(store)?;
        let native_queue =
            journal::for_session(store, &self.authority.runtime, &self.authority.session);
        Ok(authority.admits_settlement(&Authority::of(current))
            && journal::epoch(&native_queue)? == epoch
            && self
                .project
                .as_ref()
                .map_or(Ok(true), |project| project.current(self))?)
    }
}

const PUBLICATION_ENV: &str = "AGIT_STOP_PUBLICATION";

/// The child repeats the request's restrictions before its ordinary publication gates run.
pub(crate) fn verify_publication(target: Option<&str>, tip: Option<&str>) -> crate::Result<()> {
    let Some(selected_request) = std::env::var_os(PUBLICATION_ENV) else {
        return Ok(());
    };
    let selected_request = selected_request
        .to_str()
        .context("invalid Stop publication identity")?;
    let (queue_key, id) = selected_request
        .split_once(':')
        .context("invalid Stop publication identity")?;
    let store = Store::open()?.context("queued publication has no session store")?;
    let queue = selected(&store, queue_key)?;
    let request = queue.read::<Pending>(&journal::RequestId::parse(id)?)?;
    let pending = &request.payload;
    pending.verify_queue(&store, &queue)?;
    let Phase::Publishing { tip: saved, .. } = &request.phase else {
        anyhow::bail!("queued publication is not active");
    };
    ensure!(
        tip == Some(saved.as_str())
            && target
                == Some(
                    format!(
                        "{}@{}",
                        pending.repository,
                        pending.destination_branch(&store)?
                    )
                    .as_str()
                ),
        "queued publication target changed"
    );
    let current = link::get_checked(
        &store,
        &pending.authority.runtime,
        &pending.authority.session,
    )?
    .context("queued publication claim is unavailable")?;
    ensure!(
        pending.route_current(&store, &current)? && pending.account_current()?,
        "queued publication authority changed"
    );
    ensure!(
        super::project::stop_revisions(
            &pending.cwd,
            &pending.authority.runtime,
            &pending.authority.session
        )? == pending.project_revisions,
        "queued publication was withdrawn by folder policy"
    );
    for (scope, epoch) in &pending.policy_epochs {
        ensure!(
            journal::policy_epoch(scope)? == *epoch,
            "queued publication was withdrawn by automatic saving policy"
        );
    }
    let (owner, name) = super::parse_slug(&pending.repository)?;
    let repo = Repo::open(config::repo_dir(&owner, &name)?)
        .context("queued publication repository is unavailable")?;
    ensure!(
        repo.common_dir()?.canonicalize()? == pending.common_directory
            && identity::read(&repo)? == pending.remote,
        "queued publication repository changed"
    );
    Ok(())
}

#[derive(Debug, thiserror::Error)]
#[error(
    "automatic saving is queued but its worker could not start; `agit status` shows its recovery command"
)]
pub(crate) struct WorkerUnavailable;

#[derive(Debug, thiserror::Error)]
#[error(
    "automatic saving could not queue this Stop; its work is not confirmed saved; run `agit save` to save it explicitly"
)]
pub(crate) struct AdmissionFailed;

pub(crate) fn enqueue(store: &Store, selected: &Link) -> crate::Result<bool> {
    let (Some(repository), Some(branch)) = (
        super::context::slug_of_link(selected),
        selected.branch.clone(),
    ) else {
        return Ok(false);
    };
    let author = super::commit::owner_for_recording(true)?.context(super::commit::SIGNED_OUT)?;
    let (owner, name) = super::parse_slug(&repository)?;
    let repo = Repo::open(config::repo_dir(&owner, &name)?).ok_or_else(|| {
        super::settle_failures::NotSaved(format!("{repository} isn't on this computer"))
    })?;
    if let Some(refusal) = super::commit::outside_folder(&repo, &repository, &branch, selected) {
        anyhow::bail!("{refusal}");
    }
    ensure!(
        selected.is_active(),
        super::settle_failures::NotSaved("this conversation continues as another session".into())
    );
    ensure!(
        !super::branch::is_sealed(&repo, &branch),
        super::settle_failures::NotSaved(format!(
            "`{branch}` is sealed — read-only. Fork it to keep working: `agit fork {branch} -b <new> --resume`"
        ))
    );
    super::hooks::require_stop_admission_ready()?;
    enqueue_inner(store, selected, repository, branch, repo, author)
        .map(|()| true)
        .map_err(|error| {
            if error.is::<WorkerUnavailable>() {
                error
            } else {
                error.context(AdmissionFailed)
            }
        })
}

fn enqueue_inner(
    store: &Store,
    selected: &Link,
    repository: String,
    branch: String,
    repo: Repo,
    author: String,
) -> crate::Result<()> {
    let (pending, _) = observe(store, selected, repository, branch, repo, author)?;
    let queue = journal::for_session(store, &selected.source, &selected.session_id);
    {
        // Link writers cannot replace the checked claim before its immutable request is installed.
        let _claim = link::try_lock(store, &selected.source, &selected.session_id)?;
        let admission = queue.admit()?;
        let current = link::get_checked(store, &selected.source, &selected.session_id)?
            .context("the Stop's native claim disappeared")?;
        ensure!(
            Authority::of(&current) == pending.authority
                && journal::epoch(&queue)? == pending.epoch,
            "the Stop's native claim changed before admission"
        );
        ensure!(
            credentials::current_account()? == pending.account
                && credentials::current_user().as_deref() == Some(&pending.author)
                && identity::normalize_hub(&config::hub_url())? == pending.hub
                && super::project::stop_revisions(
                    &pending.cwd,
                    &selected.source,
                    &selected.session_id,
                )? == pending.project_revisions,
            "the Stop's authority changed before admission"
        );
        for (scope, expected) in &pending.policy_epochs {
            ensure!(
                journal::policy_epoch(scope)? == *expected,
                "the Stop was withdrawn before admission"
            );
        }
        admission.enqueue(pending)?;
    }
    let idle = queue.try_own()?.is_some();
    if idle {
        spawn(&queue).map_err(|_| WorkerUnavailable)?;
    }
    Ok(())
}

pub(super) fn observe(
    store: &Store,
    selected: &Link,
    repository: String,
    branch: String,
    repo: Repo,
    author: String,
) -> crate::Result<(Pending, Vec<u8>)> {
    ensure!(
        selected.merge_archive.is_none(),
        "Archive settlement must remain synchronous"
    );
    let authority = Authority::of(selected);
    let (owner, name) = super::parse_slug(&repository)?;
    let account = credentials::current_account()?;
    let root = journal::root(store);
    crate::domain::merge_archive::create_namespace(&root)?;
    let queue = journal::for_session(store, &selected.source, &selected.session_id);
    queue.create()?;
    let epoch = journal::epoch(&queue)?;
    let common_directory = repo.common_dir()?.canonicalize()?;
    let cwd = selected
        .cwd
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?)
        .canonicalize()?;
    let policy_epochs = [
        "commit.auto".to_owned(),
        "push.auto".to_owned(),
        format!("repository:{}", common_directory.display()),
        journal::destination_policy(&owner, &name)?,
    ]
    .into_iter()
    .map(|scope| Ok((scope.clone(), journal::policy_epoch(&scope)?)))
    .collect::<crate::Result<BTreeMap<_, _>>>()?;
    let project_revisions =
        super::project::stop_revisions(&cwd, &selected.source, &selected.session_id)?;
    let hub = identity::normalize_hub(&config::hub_url())?;
    let remote = identity::read(&repo)?;
    let (source_path, bytes, receipt) = super::commit::observe_queued_stop(selected)?;
    let end = receipt
        .as_ref()
        .map_or(bytes.len(), |receipt| receipt.bytes);
    let pending = Pending {
        authority,
        epoch,
        policy_epochs,
        project_revisions,
        cwd,
        account,
        hub,
        author,
        repository,
        branch,
        common_directory,
        remote,
        source_path,
        project: None,
        input: Input {
            bytes: end,
            sha256: hex::encode(Sha256::digest(&bytes[..end])),
            receipt,
        },
    };
    Ok((pending, bytes))
}

fn key(queue: &journal::Journal) -> crate::Result<&str> {
    queue
        .directory()
        .file_name()
        .and_then(|name| name.to_str())
        .context("Stop queue has no identity")
}

fn spawn(queue: &journal::Journal) -> crate::Result<()> {
    #[cfg(debug_assertions)]
    if std::env::var("AGIT_TEST_STOP_SPAWN_FAIL").as_deref() == Ok("1") {
        anyhow::bail!("synthetic Stop worker launch failure");
    }
    let mut command = crate::infra::background::command(std::env::current_exe()?);
    command
        .args(["--quiet", "hooks", "drain-stops", key(queue)?])
        .env_remove("AGIT_SESSION")
        .env_remove("AGIT_YES")
        .env_remove("AGIT_LOCAL_AGENT_ID")
        .env_remove(identity::EXPECTED_AGENT_ID_ENV)
        .env_remove(super::commit::SUPERVISOR_RESULT_ENV)
        .env_remove(super::commit::SUPERVISOR_PREPARED_ENV)
        .env_remove(PUBLICATION_ENV)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    command.spawn()?;
    Ok(())
}

pub(super) fn selected(store: &Store, key: &str) -> crate::Result<journal::Journal> {
    ensure!(
        key.strip_prefix("project-").unwrap_or(key).len() == 64
            && key
                .strip_prefix("project-")
                .unwrap_or(key)
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit()),
        "invalid Stop queue identity"
    );
    Ok(journal::Journal::at(journal::root(store).join(key)))
}

struct Attempt<'a> {
    store: &'a Store,
    queue: &'a journal::Journal,
    owner: &'a journal::Owned<'a>,
    request: journal::Request<Pending>,
    retry: Option<u32>,
}

impl Attempt<'_> {
    fn update(&mut self, phase: Phase) -> crate::Result<()> {
        self.request = self.owner.transition(&self.request, phase)?;
        Ok(())
    }

    fn block(&mut self, reason: BlockedBy, tip: Option<String>) -> crate::Result<()> {
        let memory = match &self.request.phase {
            Phase::Saved { memory, .. }
            | Phase::Publishing { memory, .. }
            | Phase::Published { memory, .. } => Some(memory.clone()),
            Phase::Blocked { memory, .. } => memory.clone(),
            _ => None,
        };
        self.update(Phase::Blocked {
            reason,
            tip,
            memory,
        })
    }

    fn repository(&self) -> crate::Result<Repo> {
        let pending = &self.request.payload;
        let (owner, name) = super::parse_slug(&pending.repository)?;
        let repo = Repo::open(config::repo_dir(&owner, &name)?)
            .context("queued Stop repository is unavailable")?;
        ensure!(
            repo.common_dir()?.canonicalize()? == pending.common_directory,
            "queued Stop repository changed"
        );
        ensure!(
            identity::read(&repo)? == pending.remote,
            "queued Stop repository identity changed"
        );
        Ok(repo)
    }

    fn pin(&self, repo: &Repo, tip: &str) -> crate::Result<()> {
        let reference = format!("refs/agit/stop-requests/{}", self.request.id.as_str());
        repo.git(&["update-ref", &reference, tip])?;
        Ok(())
    }

    fn retire_completed(&self) -> crate::Result<()> {
        let Some(tip) = self.request.phase.completed_tip() else {
            return Ok(());
        };
        let repo = self.repository()?;
        let pending = &self.request.payload;
        if pending.project.is_some() {
            super::upload_outcome::cancel_stop_retry(
                &pending.repository,
                &pending.destination_branch(self.store)?,
                tip,
                &super::upload_outcome::StopPublication {
                    queue: key(self.queue)?.to_owned(),
                    request: self.request.id.as_str().to_owned(),
                },
            )?;
        }
        repo.git(&[
            "update-ref",
            "-d",
            &format!("refs/agit/stop-requests/{}", self.request.id.as_str()),
            tip,
        ])?;
        self.owner.retire(&self.request)
    }

    fn policies_current(&self) -> crate::Result<bool> {
        self.request.payload.policies_current()
    }

    fn save_result(&mut self, repo: &Repo, tip: &str, memory: MemoryOutcome) -> crate::Result<()> {
        repo.git(&[
            "merge-base",
            "--is-ancestor",
            tip,
            &format!(
                "refs/heads/{}",
                self.request.payload.destination_branch(self.store)?
            ),
        ])?;
        self.pin(repo, tip)?;
        self.update(Phase::Saved {
            tip: tip.to_owned(),
            memory,
        })?;
        let pending = &self.request.payload;
        if super::settle_failures::output_discarded(&pending.authority.runtime) {
            super::settle_failures::settled(
                &pending.authority.runtime,
                &pending.authority.session,
                Some(&pending.cwd),
                Ok(()),
                false,
            );
        }
        Ok(())
    }

    fn publication(&mut self) -> crate::Result<()> {
        let Phase::Saved { tip, memory } = &self.request.phase else {
            return Ok(());
        };
        let tip = tip.clone();
        let memory = memory.clone();
        if matches!(memory, MemoryOutcome::Failed | MemoryOutcome::Superseded) {
            return self.block(BlockedBy::MemoryIncomplete, Some(tip));
        }
        // A policy withdrawal ends publication of an already saved request without new admission.
        if !self.policies_current()? {
            return self.block(BlockedBy::PublicationOff, Some(tip));
        }
        let pending = &self.request.payload;
        let current = link::get_checked(
            self.store,
            &pending.authority.runtime,
            &pending.authority.session,
        )?
        .context("queued Stop claim is missing before publication")?;
        if !self.admits(&current)? {
            return self.block(BlockedBy::AuthorityChanged, Some(tip));
        }
        let repo = self.repository()?;
        let branch = pending.destination_branch(self.store)?;
        let retry = self.retry.or_else(|| {
            pending.project.as_ref().map(|_| {
                super::upload_outcome::retries(&pending.repository, std::slice::from_ref(&branch))
                    .into_iter()
                    .find(|due| due.tip == tip)
                    .map_or(0, |due| due.retry)
            })
        });
        let Some(remote) = &pending.remote else {
            return self.block(BlockedBy::RepositoryIdentityUnavailable, Some(tip));
        };
        if !super::auto_push::publication_allowed(&repo, &pending.repository)?
            || super::project::blocks_auto_push(&repo, &pending.repository, &branch)?
        {
            return self.block(BlockedBy::PublicationOff, Some(tip));
        }
        let target = format!("{}@{}", pending.repository, branch);
        let repository = pending.repository.clone();
        let agent_id = remote.agent_id.clone();
        let queue_key = key(self.queue)?.to_owned();
        let publication = format!("{}:{}", queue_key, self.request.id.as_str());
        let stop_publication = pending
            .project
            .as_ref()
            .map(|_| (queue_key, self.request.id.as_str().to_owned()));
        self.update(Phase::Publishing {
            tip: tip.clone(),
            memory: memory.clone(),
        })?;
        let output = super::upload_outcome::recorded(
            super::upload_outcome::Place::of(&repository, &branch).map(|place| {
                let place = match retry {
                    Some(retry) => place.retried(retry),
                    None => place,
                };
                match &stop_publication {
                    Some((queue, request)) => place.for_stop(queue, request),
                    None => place,
                }
            }),
            &tip,
            || {
                let mut command = crate::infra::background::command(std::env::current_exe()?);
                command.arg("--json");
                super::auto_push::automatic_push_up_to(&mut command, &target, &tip);
                command
                    .current_dir(repo.root())
                    .env("AGIT_SESSION", &target)
                    .env(identity::EXPECTED_AGENT_ID_ENV, &agent_id)
                    .env(PUBLICATION_ENV, &publication)
                    .env("AGIT_TUI", "0")
                    .stdin(Stdio::null());
                command.output()
            },
            |output| match output {
                Ok(output) if output.status.success() => None,
                Ok(output) => Some(super::upload_outcome::Refusal::of(output)),
                Err(_) => Some(super::upload_outcome::Refusal::unexplained()),
            },
        )
        .map_err(|_| anyhow::anyhow!("queued Stop publication outcome could not be recorded"))?;
        if let Some(project) = &self.request.payload.project {
            let failure = match &output {
                Ok(output) if output.status.success() => None,
                Ok(output) => Some(super::upload_outcome::Refusal::of(output).class(&target)),
                Err(_) => Some(super::project::UploadFailure::Other),
            };
            project.record_upload(&self.request.payload, &tip, failure)?;
        }
        match output {
            Ok(output) if output.status.success() => {
                self.update(Phase::Published {
                    tip: tip.clone(),
                    memory,
                })?;
                if self.request.payload.project.is_some() {
                    super::project::refresh_published_branch(
                        &repository,
                        &branch,
                        &self.request.payload.cwd,
                    );
                }
                Ok(())
            }
            _ => self.block(BlockedBy::PublicationFailed, Some(tip)),
        }
    }
}

impl QueuedStop for Attempt<'_> {
    fn project_capture(&self) -> bool {
        self.request
            .payload
            .project
            .as_ref()
            .is_some_and(|project| project.capture)
    }

    fn receipt(&self) -> Option<StopReceipt> {
        self.request.payload.input.receipt.clone()
    }

    fn input_boundary(&self) -> (usize, &str) {
        let input = &self.request.payload.input;
        (input.bytes, &input.sha256)
    }

    fn admits(&self, current: &Link) -> crate::Result<bool> {
        let pending = &self.request.payload;
        Ok(pending.route_current(self.store, current)?
            && journal::policy_epoch("commit.auto")?
                == pending.policy_epochs.get("commit.auto").cloned().flatten()
            && super::project::stop_revisions(
                &pending.cwd,
                &pending.authority.runtime,
                &pending.authority.session,
            )? == pending.project_revisions
            && current.cwd.as_ref().is_none_or(|cwd| {
                Path::new(cwd).canonicalize().ok().as_ref() == Some(&pending.cwd)
            })
            && credentials::current_account()? == pending.account
            && identity::normalize_hub(&config::hub_url())? == pending.hub
            && credentials::current_user().as_deref() == Some(&pending.author)
            && current
                .resolve()
                .and_then(|path| path.canonicalize().ok())
                .as_ref()
                == Some(&pending.source_path)
            && self.repository().is_ok())
    }

    fn withheld(&mut self, reason: StopWithheld) -> crate::Result<()> {
        let reason = match reason {
            StopWithheld::AutomaticSavingOff => BlockedBy::AutomaticSavingOff,
            StopWithheld::Delegated => BlockedBy::Delegated,
            StopWithheld::MissingLink => BlockedBy::MissingLink,
            StopWithheld::AuthorityChanged => BlockedBy::AuthorityChanged,
            StopWithheld::MergePaused => BlockedBy::MergePaused,
            StopWithheld::Sealed => BlockedBy::Sealed,
            StopWithheld::SaveFailed => BlockedBy::SaveFailed,
        };
        self.block(reason, None)
    }

    fn saving(&mut self, parent: Option<&str>, covered: bool) -> crate::Result<()> {
        ensure!(
            covered
                || !matches!(
                    self.request.phase,
                    Phase::Saving { .. }
                        | Phase::Blocked {
                            reason: BlockedBy::InterruptedSave,
                            ..
                        }
                ),
            "interrupted Stop settlement has no proven completion; retained evidence needs inspection"
        );
        self.update(Phase::Saving {
            parent: parent.map(str::to_owned),
        })
    }

    fn settled(
        &mut self,
        repo: &Repo,
        current: &Link,
        tip: Option<&str>,
        covered: bool,
        memory: MemoryOutcome,
    ) -> crate::Result<()> {
        if covered && let Some(tip) = tip {
            return self.save_result(repo, tip, memory);
        }
        let reason = if current.privacy_recovery.is_some() {
            BlockedBy::PrivacyPending
        } else {
            BlockedBy::NativeCompletionPending
        };
        self.update(Phase::Blocked {
            reason,
            tip: None,
            memory: Some(memory),
        })
    }
}

pub(crate) fn drain(queue_key: &str) -> crate::Result<bool> {
    let store = Store::open()?.context("there is no local session store")?;
    let queue = selected(&store, queue_key)?;
    if queue_key.starts_with("project-") {
        return super::project::stops::recover_selected(&queue);
    }
    let mut seen = BTreeSet::new();
    loop {
        let Some(owner) = queue.try_own()? else {
            return Ok(false);
        };
        drain_owned(&store, &queue, &owner, &mut seen, false, None)?;
        drop(owner);
        let remaining = queue.pending()?;
        if remaining.iter().all(|id| seen.contains(id.as_str())) {
            return Ok(remaining.is_empty());
        }
    }
}

pub(super) fn drain_owned(
    store: &Store,
    queue: &journal::Journal,
    owner: &journal::Owned<'_>,
    seen: &mut BTreeSet<String>,
    automatic: bool,
    retry: Option<u32>,
) -> crate::Result<()> {
    let mut requests = Vec::new();
    for id in queue.pending()? {
        if seen.insert(id.as_str().to_owned()) {
            match queue.read::<Pending>(&id) {
                Ok(request) if request.payload.verify_queue(store, queue).is_ok() => {
                    requests.push(request)
                }
                Ok(_) => crate::ui::warning(
                    "a Stop request has an inconsistent queue identity; its evidence is retained",
                ),
                Err(_) => {
                    crate::ui::warning("a Stop request is unreadable; its evidence is retained")
                }
            }
        }
    }
    requests.sort_by_key(|request| {
        (
            request.payload.input.bytes,
            request
                .payload
                .project
                .as_ref()
                .map_or(0, |project| project.sequence),
        )
    });
    for request in requests {
        if automatic
            && request.payload.project.is_some()
            && matches!(
                request.phase,
                Phase::Blocked {
                    reason: BlockedBy::PublicationFailed,
                    ..
                }
            )
        {
            continue;
        }
        let mut attempt = Attempt {
            store,
            queue,
            owner,
            request,
            retry,
        };
        let result = (|| -> crate::Result<()> {
            let project = attempt.request.payload.project.clone();
            let _policy = project.as_ref().map(|project| project.lock()).transpose()?;
            match attempt.request.phase.clone() {
                Phase::Publishing { tip, memory }
                | Phase::Blocked {
                    reason: BlockedBy::PublicationFailed,
                    tip: Some(tip),
                    memory: Some(memory),
                } => {
                    attempt.update(Phase::Saved { tip, memory })?;
                }
                Phase::Published { .. } => return attempt.retire_completed(),
                Phase::Accepted
                | Phase::Saving { .. }
                | Phase::Blocked {
                    reason: BlockedBy::MemoryIncomplete,
                    memory: Some(MemoryOutcome::Failed),
                    ..
                }
                | Phase::Blocked {
                    reason: BlockedBy::NativeCompletionPending | BlockedBy::InterruptedSave,
                    ..
                } => {
                    let pending = &attempt.request.payload;
                    let link = match &pending.project {
                        Some(project) => {
                            Some(project.prepare(store, queue, &attempt.request.id, pending)?)
                        }
                        None => link::get_checked(
                            store,
                            &pending.authority.runtime,
                            &pending.authority.session,
                        )?,
                    };
                    let Some(link) = link else {
                        return attempt.block(BlockedBy::MissingLink, None);
                    };
                    let code = super::commit::settle_queued_link(store, link, &mut attempt)?;
                    if matches!(
                        attempt.request.phase,
                        Phase::Accepted | Phase::Saving { .. }
                    ) {
                        attempt.block(BlockedBy::SaveFailed, None)?;
                    }
                    let _ = code;
                }
                Phase::Saved { .. } => {}
                Phase::Blocked { .. } => return attempt.retire_completed(),
            }
            attempt.publication()?;
            attempt.retire_completed()
        })();
        if let Err(error) = result {
            let reason = if error.is::<super::project::stops::InterruptedAdoption>() {
                Some(BlockedBy::InterruptedAdoption)
            } else if error.is::<super::commit::QueuedInputUnavailable>() {
                Some(BlockedBy::InputChanged)
            } else {
                match attempt.request.phase {
                    Phase::Accepted => Some(BlockedBy::SaveFailed),
                    Phase::Saving { .. } => Some(BlockedBy::InterruptedSave),
                    _ => None,
                }
            };
            if let Some(reason) = reason {
                let _ = attempt.block(reason, None);
            }
            crate::ui::warning(
                "a queued Stop did not finish; its request and outcome remain available for recovery",
            );
        }
    }
    Ok(())
}

/// A durable request can remain pending even when its worker has not acquired the lock yet.
pub(crate) fn save_pending(store: &Store, claim: &Link) -> bool {
    let queue = journal::for_session(store, &claim.source, &claim.session_id);
    let Ok(ids) = queue.pending() else {
        return false;
    };
    ids.iter().any(|id| {
        let Ok(request) = queue.read::<Pending>(id) else {
            return false;
        };
        request
            .payload
            .authority
            .admits_settlement(&Authority::of(claim))
            && journal::epoch(&queue).is_ok_and(|epoch| epoch == request.payload.epoch)
            && matches!(
                request.phase,
                Phase::Accepted | Phase::Saving { .. } | Phase::Blocked { tip: None, .. }
            )
    })
}

fn explanation(phase: &Phase, active: bool) -> (&'static str, bool) {
    match phase {
        Phase::Blocked {
            reason: BlockedBy::MemoryIncomplete,
            memory: Some(MemoryOutcome::Failed),
            ..
        } => (
            "conversation saved locally; memory collection failed and can be retried",
            true,
        ),
        Phase::Blocked {
            reason: BlockedBy::MemoryIncomplete,
            ..
        } => (
            "conversation saved locally; later conversation output prevents attributing current memory to this Stop",
            false,
        ),
        Phase::Accepted => (
            if active {
                "queued; a worker is saving this Stop"
            } else {
                "queued; no worker is running"
            },
            true,
        ),
        Phase::Saving { .. } => (
            if active {
                "saving locally"
            } else {
                "saving was interrupted; completion has not been verified"
            },
            true,
        ),
        Phase::Saved { .. } => ("saved locally; publication is pending", true),
        Phase::Publishing { .. } => ("saved locally; publication has not been confirmed", true),
        Phase::Published { .. } => ("published; local queue cleanup is pending", true),
        Phase::Blocked {
            reason: BlockedBy::PublicationFailed,
            ..
        } => ("saved locally; publication failed", true),
        Phase::Blocked {
            reason: BlockedBy::InterruptedAdoption,
            ..
        } => (
            "first adoption was interrupted; inspect its retained evidence before saving explicitly",
            false,
        ),
        Phase::Blocked {
            reason: BlockedBy::InterruptedSave,
            ..
        } => (
            "saving was interrupted; recovery can verify existing completion evidence",
            true,
        ),
        Phase::Blocked {
            reason: BlockedBy::NativeCompletionPending,
            ..
        } => (
            "the admitted Stop has no verified complete execution; save the conversation explicitly when it finishes",
            false,
        ),
        Phase::Blocked {
            reason: BlockedBy::InputChanged,
            ..
        } => (
            "the native source is unavailable or changed; inspect it before saving explicitly",
            false,
        ),
        Phase::Blocked { tip: Some(_), .. } => (
            "saved locally; publication is withheld by current identity or policy; use an explicit push after resolving it",
            false,
        ),
        Phase::Blocked { .. } => (
            "automatic saving was withheld; resolve its policy or identity before saving explicitly",
            false,
        ),
    }
}

pub(crate) fn status(cwd: &Path, repository: Option<&str>) -> crate::Result<serde_json::Value> {
    let store = Store::at(config::store_root()?);
    let root = journal::root(&store);
    let mut items = Vec::new();
    let mut unavailable = 0usize;
    let mut omitted = 0usize;
    if crate::domain::merge_archive::authority_directory_exists(&root)? {
        let account = credentials::current_account()?;
        let cwd = cwd.canonicalize()?;
        for (index, entry) in std::fs::read_dir(&root)?.enumerate() {
            ensure!(
                index < 1024,
                "Stop queue listing exceeds its inspection budget"
            );
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                unavailable += 1;
                continue;
            };
            let queue = match selected(&store, name) {
                Ok(queue) => queue,
                Err(_) => {
                    unavailable += 1;
                    continue;
                }
            };
            for id in queue.pending()? {
                if items.len() >= 128 {
                    omitted += 1;
                    continue;
                }
                let request = match queue.read::<Pending>(&id) {
                    Ok(request) => request,
                    Err(_) => {
                        unavailable += 1;
                        continue;
                    }
                };
                let pending = &request.payload;
                let source_cwd = pending.cwd.canonicalize().ok();
                if account != pending.account
                    || credentials::current_user().as_deref() != Some(&pending.author)
                    || identity::normalize_hub(&config::hub_url())? != pending.hub
                    || repository.is_some_and(|selected| selected != pending.repository)
                    || source_cwd.as_ref().is_none_or(|source| source != &cwd)
                {
                    omitted += 1;
                    continue;
                }
                let active = queue.owned()?;
                let (description, recoverable) = explanation(&request.phase, active);
                items.push(serde_json::json!({
                    "request": id.as_str(), "repository": pending.repository,
                    "branch": pending.destination_branch(&store).ok(), "outcome": request.phase,
                    "worker_owned": active, "description": description,
                    "recovery": recoverable.then(|| match &pending.project {
                        Some(project) => serde_json::json!({"program": "agit", "args": ["project", "recover", project.root]}),
                        None => serde_json::json!({"program": "agit", "args": ["hooks", "recover", name]}),
                    }),
                }));
            }
        }
    }
    Ok(serde_json::json!({"items": items, "unavailable": unavailable, "omitted": omitted}))
}
