//! Automatic Stops retain their own input proof independently of discovery and explicit saves.

use super::*;
use crate::commands::stop_requests::Pending;
use crate::domain::merge_archive as persistence;
use crate::domain::stop_requests::{self as journal, Authority, RequestId};

#[derive(Debug, thiserror::Error)]
#[error("project Stop adoption was interrupted; retained evidence needs inspection")]
pub(in crate::commands) struct InterruptedAdoption;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::commands) struct Intent {
    pub(in crate::commands) root: PathBuf,
    initial: Option<Authority>,
    generation: Option<RequestId>,
    revision: (u64, u64),
    parents: Vec<Parent>,
    pub(in crate::commands) sequence: u64,
    pub(in crate::commands) capture: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Parent {
    session: String,
    claim: Option<Authority>,
    epoch: Option<String>,
    cwd: PathBuf,
    root: PathBuf,
    revisions: BTreeMap<PathBuf, (u64, u64)>,
    parent: Option<String>,
}

fn parent_id(header: &serde_json::Value) -> Result<Option<String>> {
    if !crate::adapter::codex::delegated_header(header) {
        return Ok(None);
    }
    let payload = &header["payload"];
    let parents: Vec<_> = [
        payload["source"]["subagent"]["thread_spawn"]["parent_thread_id"].as_str(),
        payload["parent_thread_id"].as_str(),
    ]
    .into_iter()
    .flatten()
    .collect();
    let parent = parents
        .first()
        .copied()
        .or_else(|| payload["forked_from_id"].as_str())
        .context("project Stop delegated parent is missing")?;
    ensure!(
        uuid::Uuid::parse_str(parent).is_ok() && parents.iter().all(|id| *id == parent),
        "project Stop delegated parent is ambiguous"
    );
    Ok(Some(parent.to_owned()))
}

fn parent_cwd(header: &serde_json::Value, session: &str) -> Result<PathBuf> {
    ensure!(
        header["type"] == "session_meta" && header["payload"]["id"] == session,
        "project Stop delegated parent identity changed"
    );
    let cwd = header["payload"]["cwd"]
        .as_str()
        .map(Path::new)
        .context("project Stop delegated parent has no directory")?;
    ensure!(
        cwd.is_absolute(),
        "project Stop delegated parent directory is relative"
    );
    cwd.canonicalize()
        .context("project Stop delegated parent directory is unavailable")
}

impl Parent {
    fn revisions(&self) -> Result<BTreeMap<PathBuf, (u64, u64)>> {
        let mut revisions = stop_revisions(&self.cwd, "codex", &self.session)?;
        revisions.insert(
            self.root.clone(),
            revision(&self.root, "codex", &self.session)?,
        );
        Ok(revisions)
    }

    fn current(&self, store: &Store) -> Result<bool> {
        Ok(link::get_checked(store, "codex", &self.session)?
            .as_ref()
            .map(Authority::of)
            == self.claim
            && journal::epoch(&journal::for_session(store, "codex", &self.session))? == self.epoch
            && self.revisions()? == self.revisions)
    }
}

/// Snapshot cancellation metadata before observing input; later policy restoration cannot
/// authorize an accepted descendant again. The frozen leaf header must select this same chain.
fn parents(store: &Store, project: &Project, candidate: &Candidate) -> Result<Vec<Parent>> {
    if candidate.runtime != "codex" {
        return Ok(Vec::new());
    }
    persistence::create_namespace(&journal::root(store))?;
    let header = delegation_header(&candidate.session_id)?
        .context("project Stop native ancestry is unavailable")?;
    let mut next = parent_id(&header)?;
    let mut seen = BTreeSet::from([candidate.session_id.clone()]);
    let mut parents = Vec::new();
    while let Some(session) = next {
        ensure!(
            seen.len() < 64 && seen.insert(session.clone()),
            "invalid project Stop ancestry"
        );
        let queue = journal::for_session(store, "codex", &session);
        queue.create()?;
        let epoch = journal::epoch(&queue)?;
        let claim = link::get_checked(store, "codex", &session)?
            .as_ref()
            .map(Authority::of);
        let header =
            delegation_header(&session)?.context("project Stop delegated parent is unavailable")?;
        let cwd = parent_cwd(&header, &session)?;
        let root =
            governing(&cwd)?.map_or_else(|| project.root.clone(), |(project, _)| project.root);
        next = parent_id(&header)?;
        let mut parent = Parent {
            session,
            claim,
            epoch,
            cwd,
            root,
            revisions: BTreeMap::new(),
            parent: next.clone(),
        };
        parent.revisions = parent.revisions()?;
        parents.push(parent);
    }
    Ok(parents)
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Generation {
    version: u32,
    id: RequestId,
    binding: String,
    next_sequence: u64,
    last_bytes: usize,
    last_sha256: String,
    state: Adoption,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum Adoption {
    Accepting,
    Planned {
        request: RequestId,
        claim: Authority,
    },
    Adopted {
        request: RequestId,
        claim: Authority,
        epoch: Option<String>,
    },
}

pub(in crate::commands) fn queue(root: &Path) -> Result<journal::Journal> {
    let store = Store::at(config::store_root()?);
    Ok(journal::Journal::at(journal::root(&store).join(format!(
        "project-{}",
        hex::encode(Sha256::digest(root.as_os_str().as_encoded_bytes()))
    ))))
}

fn generation_path(queue: &journal::Journal, id: &RequestId) -> Result<PathBuf> {
    RequestId::parse(id.as_str())?;
    Ok(queue.directory().join(format!("{}.adoption", id.as_str())))
}

fn read_generation(queue: &journal::Journal, id: &RequestId) -> Result<Generation> {
    let bytes = persistence::read_private_bytes(&generation_path(queue, id)?, 64 * 1024)?
        .context("project Stop adoption evidence is missing")?;
    let value: Generation = serde_json::from_slice(&bytes)?;
    ensure!(
        value.version == 1 && value.id == *id,
        "unsupported project Stop adoption evidence"
    );
    Ok(value)
}

fn write_generation(queue: &journal::Journal, value: &Generation, replace: bool) -> Result<()> {
    persistence::durable_publish_bytes(
        &generation_path(queue, &value.id)?,
        &serde_json::to_vec(value)?,
        replace,
    )
}

fn revision(root: &Path, runtime: &str, session: &str) -> Result<(u64, u64)> {
    stop_revisions(root, runtime, session)?
        .get(root)
        .copied()
        .context("project Stop policy revision is unavailable")
}

impl Intent {
    fn binding(&self, pending: &Pending) -> Result<String> {
        let bytes = serde_json::to_vec(&serde_json::json!({
            "root": self.root, "initial": self.initial, "revision": self.revision,
            "parents": self.parents,
            "authority": pending.authority, "epoch": pending.epoch,
            "policies": pending.policy_epochs, "revisions": pending.project_revisions,
            "cwd": pending.cwd, "account": pending.account, "hub": pending.hub,
            "author": pending.author, "repository": pending.repository,
            "common_directory": pending.common_directory, "remote": pending.remote,
            "source": pending.source_path,
        }))?;
        Ok(hex::encode(Sha256::digest(bytes)))
    }

    fn policy_current(&self, pending: &Pending) -> Result<bool> {
        let store = Store::at(config::store_root()?);
        for parent in &self.parents {
            if !parent.current(&store)? {
                return Ok(false);
            }
        }
        let Some(project) = read(&self.root)? else {
            return Ok(false);
        };
        Ok(project.root == self.root
            && project.repository == pending.repository
            && Some(&project.identity) == pending.remote.as_ref()
            && project.account == pending.author
            && project.account_id.as_deref()
                == pending
                    .account
                    .as_ref()
                    .map(|account| account.account_id.as_str())
            && project.enabled
            && project.auto_upload
            && !project.leaves_out(&key(&pending.authority.runtime, &pending.authority.session))
            && revision(
                &self.root,
                &pending.authority.runtime,
                &pending.authority.session,
            )? == self.revision
            && pending.account_current()?
            && pending.policies_current()?)
    }

    pub(in crate::commands) fn current(&self, pending: &Pending) -> Result<bool> {
        if !self.policy_current(pending)? {
            return Ok(false);
        }
        for parent in &self.parents {
            let Some(header) = delegation_header(&parent.session)? else {
                return Ok(false);
            };
            if parent_cwd(&header, &parent.session)? != parent.cwd
                || parent_id(&header)? != parent.parent
            {
                return Ok(false);
            }
        }
        let Some((project, _)) = governing(&pending.cwd)? else {
            return Ok(false);
        };
        if project.root != self.root {
            return Ok(false);
        }
        let candidate = Candidate {
            runtime: pending.authority.runtime.clone(),
            session_id: pending.authority.session.clone(),
            cwd: pending.cwd.clone(),
        };
        Ok(!withholds(&project, &candidate, false))
    }

    pub(in crate::commands) fn lock(&self) -> Result<fs::File> {
        super::lock(&self.root)
    }

    /// The caller retains project ownership through publication and its folder summary. Only
    /// the admitted tip may become success evidence; a later native tail is still unsaved.
    pub(in crate::commands) fn record_upload(
        &self,
        pending: &Pending,
        tip: &str,
        failure: Option<UploadFailure>,
    ) -> Result<()> {
        let mut project = read(&self.root)?.context("project Stop policy disappeared")?;
        ensure!(
            project.repository == pending.repository
                && Some(&project.identity) == pending.remote.as_ref(),
            "project Stop publication summary belongs to another repository"
        );
        if failure == Some(UploadFailure::PublicNeedsConsent) {
            observed_public(&self.root);
        }
        project.last_result = Some(serde_json::json!({
            "at": chrono::Utc::now(),
            "session_id": pending.authority.session,
            "ok": failure.is_none(),
            "error": failure.map(|reason| reason.describe()),
            "reason": failure,
            "head": failure.is_none().then_some(tip),
        }));
        save(&project)
    }

    pub(in crate::commands) fn route(
        &self,
        _store: &Store,
        pending: &Pending,
    ) -> Result<(Authority, Option<String>)> {
        let Some(id) = &self.generation else {
            return Ok((pending.authority.clone(), pending.epoch.clone()));
        };
        let value = read_generation(&queue(&self.root)?, id)?;
        ensure!(
            value.binding == self.binding(pending)? && self.sequence < value.next_sequence,
            "project Stop adoption membership changed"
        );
        match value.state {
            Adoption::Adopted { claim, epoch, .. } => Ok((claim, epoch)),
            _ => anyhow::bail!(
                "project Stop adoption has no proven completion; its evidence needs inspection"
            ),
        }
    }

    pub(in crate::commands) fn prepare(
        &self,
        store: &Store,
        queue: &journal::Journal,
        request: &RequestId,
        pending: &Pending,
    ) -> Result<link::Link> {
        ensure!(
            self.current(pending)?,
            "project Stop authority was withdrawn"
        );
        let current = link::get_checked(
            store,
            &pending.authority.runtime,
            &pending.authority.session,
        )?;
        let Some(id) = &self.generation else {
            return current.context("project Stop claim disappeared");
        };
        let generation = read_generation(queue, id)?;
        ensure!(
            generation.binding == self.binding(pending)?,
            "project Stop adoption membership changed"
        );
        if matches!(generation.state, Adoption::Adopted { .. }) {
            let (claim, epoch) = self.route(store, pending)?;
            let current = current.context("project Stop adopted claim disappeared")?;
            ensure!(
                claim.admits_settlement(&Authority::of(&current))
                    && journal::epoch(&journal::for_session(
                        store,
                        &claim.runtime,
                        &claim.session
                    ))? == epoch,
                "project Stop adopted claim was replaced"
            );
            return Ok(current);
        }
        ensure!(
            matches!(generation.state, Adoption::Accepting),
            InterruptedAdoption
        );
        ensure!(
            current.as_ref().map(Authority::of) == self.initial,
            "project Stop initial claim changed"
        );
        let mut claim = current.unwrap_or_else(|| {
            link::Link::new(
                &pending.authority.runtime,
                &pending.authority.session,
                Some(&pending.cwd),
            )
        });
        ensure!(
            claim.branch.is_none()
                && claim.native_binding.is_none()
                && claim.merge_archive.is_none()
                && claim.is_active()
                && claim.materialized_from.is_none(),
            "project Stop cannot adopt this native claim"
        );
        claim.cwd = Some(pending.cwd.to_string_lossy().into_owned());
        let path = claim
            .resolve()
            .and_then(|path| path.canonicalize().ok())
            .context("project Stop native source is unavailable")?;
        ensure!(
            path == pending.source_path,
            "project Stop native source changed"
        );
        let bytes = claim
            .read_bytes()
            .map_err(|_| crate::commands::commit::QueuedInputUnavailable)?;
        let frozen = bytes
            .get(..pending.input.bytes)
            .context(crate::commands::commit::QueuedInputUnavailable)?;
        ensure!(
            hex::encode(Sha256::digest(frozen)) == pending.input.sha256,
            crate::commands::commit::QueuedInputUnavailable
        );
        ensure!(
            verify_identity(&claim, &pending.cwd, frozen)?
                == self.parents.first().map(|parent| parent.session.clone()),
            "project Stop retained parent differs from its authority"
        );
        let project = read(&self.root)?.context("project Stop policy disappeared")?;
        let repo = verify(&project)?;
        ensure!(
            repo.common_dir()?.canonicalize()? == pending.common_directory,
            "project Stop repository changed"
        );
        let candidate = Candidate {
            runtime: claim.source.clone(),
            session_id: claim.session_id.clone(),
            cwd: pending.cwd.clone(),
        };
        let gist = adapter::get(&claim.source)?
            .parse(std::str::from_utf8(frozen)?)?
            .gist(200);
        let branch = new_branch_from_gist(&project, &candidate, gist.as_deref());
        if claim.session_privacy.is_none() {
            claim.session_privacy = Some(crate::domain::session_privacy::SessionPrivacy::new(
                pending.account.clone(),
            )?);
        }
        let _ = crate::commands::memory::name_start(
            &repo,
            &key(&claim.source, &claim.session_id),
            &branch,
        );
        let mut adoption = AdoptionAttempt {
            intent: self,
            pending,
            queue,
            request,
            generation,
        };
        let adopted = crate::commands::import::adopt_project_stop(
            store,
            claim,
            &pending.repository,
            &branch,
            &pending.author,
            &mut adoption,
        )?;
        record_capture(Captured {
            repository: pending.repository.clone(),
            branch,
            runtime: adopted.source.clone(),
            session: adopted.session_id.clone(),
        })?;
        Ok(adopted)
    }
}

struct AdoptionAttempt<'a> {
    intent: &'a Intent,
    pending: &'a Pending,
    queue: &'a journal::Journal,
    request: &'a RequestId,
    generation: Generation,
}

impl crate::commands::import::ProjectStopAdoption for AdoptionAttempt<'_> {
    fn begin(
        &mut self,
        store: &Store,
        before: Option<&link::Link>,
        after: &link::Link,
    ) -> Result<()> {
        let _authorization = save_requests::try_queue_lock()?;
        let _admission = self.queue.admit()?;
        ensure!(
            self.intent.policy_current(self.pending)?,
            "project Stop authority was withdrawn before adoption"
        );
        ensure!(
            before.map(Authority::of) == self.intent.initial
                && journal::epoch(&journal::for_session(
                    store,
                    &after.source,
                    &after.session_id
                ))? == self.pending.epoch,
            "project Stop initial authority changed before adoption"
        );
        let current = read_generation(self.queue, &self.generation.id)?;
        ensure!(
            current.binding == self.generation.binding
                && matches!(current.state, Adoption::Accepting),
            "project Stop adoption already started"
        );
        self.generation = current;
        self.generation.state = Adoption::Planned {
            request: self.request.clone(),
            claim: Authority::of(after),
        };
        write_generation(self.queue, &self.generation, true)?;
        save_request_crash_point("project_stop_after_plan");
        Ok(())
    }

    fn completed(&mut self, store: &Store, claim: &link::Link) -> Result<()> {
        save_request_crash_point("project_stop_after_claim");
        let _authorization = save_requests::try_queue_lock()?;
        let _admission = self.queue.admit()?;
        ensure!(
            self.intent.policy_current(self.pending)?,
            "project Stop authority was withdrawn during adoption"
        );
        let current = read_generation(self.queue, &self.generation.id)?;
        let Adoption::Planned {
            request,
            claim: planned,
        } = &current.state
        else {
            anyhow::bail!("project Stop adoption has no matching plan");
        };
        ensure!(
            request == self.request && *planned == Authority::of(claim),
            "project Stop adoption differs from its plan"
        );
        let actual = link::get_checked(store, &claim.source, &claim.session_id)?
            .context("project Stop adopted claim is unavailable")?;
        ensure!(
            Authority::of(&actual) == *planned,
            "project Stop adopted claim changed"
        );
        let epoch = journal::epoch(&journal::for_session(
            store,
            &claim.source,
            &claim.session_id,
        ))?;
        ensure!(
            epoch != self.pending.epoch,
            "project Stop adoption did not revoke its initial route"
        );
        self.generation.state = Adoption::Adopted {
            request: request.clone(),
            claim: planned.clone(),
            epoch,
        };
        write_generation(self.queue, &self.generation, true)
    }
}

fn verify_identity(claim: &link::Link, cwd: &Path, bytes: &[u8]) -> Result<Option<String>> {
    ensure!(
        claim.native_binding.is_none(),
        "protected native claims cannot use project first adoption"
    );
    if claim.source == "codex" {
        let end = bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .context("project Stop has no complete native header")?;
        ensure!(
            end as u64 <= crate::adapter::codex_rollout::HEADER_LIMIT,
            "project Stop native header exceeds its identity allowance"
        );
        let header: serde_json::Value = serde_json::from_slice(&bytes[..end])?;
        ensure!(
            header["type"] == "session_meta"
                && header["payload"]["id"].as_str() == Some(&claim.session_id),
            "project Stop native header does not identify the selected session"
        );
        let source_cwd = header["payload"]["cwd"]
            .as_str()
            .map(Path::new)
            .context("project Stop native header has no directory")?;
        ensure!(
            source_cwd.is_absolute() && source_cwd.canonicalize()? == cwd,
            "project Stop native directory differs from its folder"
        );
        return parent_id(&header);
    } else {
        let parsed = adapter::get(&claim.source)?.parse(std::str::from_utf8(bytes)?)?;
        ensure!(
            parsed.id == claim.session_id
                && parsed
                    .cwd
                    .as_deref()
                    .map(Path::new)
                    .is_some_and(|path| path.is_absolute()
                        && path.canonicalize().is_ok_and(|path| path == cwd)),
            "project Stop native identity differs from its selected session and folder"
        );
    }
    Ok(None)
}

pub(super) fn enqueue(project: &Project, candidate: &Candidate) -> Result<()> {
    let store = Store::at(config::store_root()?);
    let initial = link::get_checked(&store, &candidate.runtime, &candidate.session_id)?;
    let mut descriptor = initial.clone().unwrap_or_else(|| {
        link::Link::new(
            &candidate.runtime,
            &candidate.session_id,
            Some(&candidate.cwd),
        )
    });
    ensure!(
        descriptor.native_binding.is_none(),
        "protected native claims retain their direct Stop route"
    );
    if descriptor.cwd.is_none() {
        descriptor.cwd = Some(candidate.cwd.to_string_lossy().into_owned());
    }
    let repo = verify(project)?;
    let author = crate::commands::commit::owner_for_recording(true)?
        .context(crate::commands::commit::SIGNED_OUT)?;
    let mut intent = Intent {
        root: project.root.clone(),
        initial: initial.as_ref().map(Authority::of),
        generation: None,
        revision: revision(&project.root, &candidate.runtime, &candidate.session_id)?,
        parents: parents(&store, project, candidate)?,
        sequence: 0,
        capture: descriptor.branch.is_none()
            || captured_records()?.iter().any(|record| {
                record.repository == project.repository
                    && Some(&record.branch) == descriptor.branch.as_ref()
                    && record.runtime == candidate.runtime
                    && record.session == candidate.session_id
            }),
    };
    let (mut pending, bytes) = crate::commands::stop_requests::observe(
        &store,
        &descriptor,
        project.repository.clone(),
        descriptor.branch.clone().unwrap_or_default(),
        repo,
        author,
    )?;
    ensure!(
        pending.cwd == candidate.cwd,
        "project Stop selected directory differs from its claim"
    );
    ensure!(
        verify_identity(&descriptor, &candidate.cwd, &bytes)?
            == intent.parents.first().map(|parent| parent.session.clone()),
        "project Stop native parent changed during observation"
    );
    ensure!(
        intent.current(&pending)?,
        "project Stop routing changed during observation"
    );
    if let Some(original) = &initial {
        pending.authority = Authority::of(original);
    }
    let queue = queue(&project.root)?;
    queue.create()?;
    let observed_generation = if descriptor.branch.is_none() {
        let active = queue.directory().join(format!(
            "{}.generation",
            hash(&key(&candidate.runtime, &candidate.session_id))
        ));
        let previous = persistence::read_private_bytes(&active, 128)?;
        let generation = previous
            .as_ref()
            .map(|bytes| read_generation(&queue, &RequestId::parse(std::str::from_utf8(bytes)?)?))
            .transpose()?;
        let binding = intent.binding(&pending)?;
        if let Some(value) = &generation
            && value.binding == binding
        {
            ensure!(
                matches!(value.state, Adoption::Accepting),
                "project Stop first adoption is interrupted or still running"
            );
            let prior = bytes
                .get(..value.last_bytes)
                .context("project Stop source shrank before first adoption")?;
            ensure!(
                hex::encode(Sha256::digest(prior)) == value.last_sha256,
                "project Stop source changed before first adoption"
            );
        }
        Some((active, previous, generation, binding))
    } else {
        None
    };
    {
        let _parents = intent
            .parents
            .iter()
            .map(|parent| link::try_lock(&store, "codex", &parent.session))
            .collect::<Result<Vec<_>>>()?;
        let _claim = link::try_lock(&store, &candidate.runtime, &candidate.session_id)?;
        let _authorization = save_requests::try_queue_lock()?;
        let admission = queue.admit()?;
        ensure!(
            intent.policy_current(&pending)?,
            "project Stop authority changed before admission"
        );
        let current = link::get_checked(&store, &candidate.runtime, &candidate.session_id)?;
        ensure!(
            current.as_ref().map(Authority::of) == intent.initial
                && journal::epoch(&journal::for_session(
                    &store,
                    &candidate.runtime,
                    &candidate.session_id
                ))? == pending.epoch,
            "project Stop native claim changed before admission"
        );
        if let Some((active, previous, observed, binding)) = observed_generation {
            ensure!(
                persistence::read_private_bytes(&active, 128)? == previous,
                "project Stop generation changed before admission"
            );
            if let Some(value) = &observed {
                ensure!(
                    read_generation(&queue, &value.id)? == *value,
                    "project Stop generation advanced before admission"
                );
            }
            let mut generation = match observed {
                Some(value) if value.binding == binding => value,
                _ => {
                    let value = Generation {
                        version: 1,
                        id: RequestId::parse(&uuid::Uuid::new_v4().to_string())?,
                        binding,
                        next_sequence: 0,
                        last_bytes: 0,
                        last_sha256: hex::encode(Sha256::digest([])),
                        state: Adoption::Accepting,
                    };
                    write_generation(&queue, &value, false)?;
                    persistence::durable_publish_bytes(
                        &active,
                        value.id.as_str().as_bytes(),
                        true,
                    )?;
                    value
                }
            };
            generation.last_bytes = pending.input.bytes;
            generation.last_sha256 = pending.input.sha256.clone();
            intent.generation = Some(generation.id.clone());
            intent.sequence = generation.next_sequence;
            generation.next_sequence = generation
                .next_sequence
                .checked_add(1)
                .context("project Stop admission sequence exhausted")?;
            write_generation(&queue, &generation, true)?;
        }
        pending.project = Some(intent);
        admission.enqueue(pending)?;
    }
    Ok(())
}

pub(super) fn pending(root: &Path) -> Result<bool> {
    Ok(!queue(root)?.pending()?.is_empty())
}

pub(super) fn unseen(root: &Path, seen: &BTreeSet<String>) -> Result<bool> {
    let queue = queue(root)?;
    for id in queue.pending()? {
        if !seen.contains(id.as_str())
            && !matches!(
                queue.read::<Pending>(&id)?.phase,
                journal::Phase::Blocked { .. }
            )
        {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn drain(root: &Path, seen: &mut BTreeSet<String>, automatic: bool) -> Result<()> {
    let queue = queue(root)?;
    if queue.pending()?.is_empty() {
        return Ok(());
    }
    let Some(owner) = queue.try_own()? else {
        return Ok(());
    };
    crate::commands::stop_requests::drain_owned(
        &Store::at(config::store_root()?),
        &queue,
        &owner,
        seen,
        automatic,
        None,
    )
}

pub(in crate::commands) fn recover_selected(selected: &journal::Journal) -> Result<bool> {
    let Some(id) = selected.pending()?.into_iter().next() else {
        return Ok(true);
    };
    let request = selected.read::<Pending>(&id)?;
    let intent = request
        .payload
        .project
        .context("project Stop request has no folder identity")?;
    ensure!(
        queue(&intent.root)?.directory() == selected.directory(),
        "project Stop queue identity differs from its folder"
    );
    super::drain(&intent.root)
}

/// A saved result is not pending capture, even when publication still waits.
pub(super) fn contains(root: &Path, runtime: &str, session: &str) -> Result<bool> {
    let queue = queue(root)?;
    for id in queue.pending()? {
        let request = queue.read::<Pending>(&id)?;
        if request.payload.authority.runtime == runtime
            && request.payload.authority.session == session
            && matches!(
                request.phase,
                journal::Phase::Accepted
                    | journal::Phase::Saving { .. }
                    | journal::Phase::Blocked { tip: None, .. }
            )
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The failed attempt carries its request identity, so another folder cannot borrow its retry.
pub(super) fn retry_owner(due: &crate::commands::upload_outcome::Due) -> Result<Option<PathBuf>> {
    let Some(stop) = &due.stop else {
        return Ok(None);
    };
    let store = Store::at(config::store_root()?);
    ensure!(
        stop.queue.starts_with("project-"),
        "project retry has a non-project Stop identity"
    );
    let selected = crate::commands::stop_requests::selected(&store, &stop.queue)?;
    let request = selected.read::<Pending>(&RequestId::parse(&stop.request)?)?;
    let intent = request
        .payload
        .project
        .context("project retry has no original folder")?;
    ensure!(
        queue(&intent.root)?.directory() == selected.directory(),
        "project retry folder differs from its original request"
    );
    Ok(Some(intent.root))
}

/// Scheduled retries retain the original request's authorization and exact saved tip.
pub(super) fn retry(
    root: &Path,
    repository: &str,
    due: &crate::commands::upload_outcome::Due,
) -> Result<bool> {
    let Some(stop) = &due.stop else {
        return Ok(false);
    };
    ensure!(
        retry_owner(due)?.as_deref() == Some(root),
        "project retry belongs to another folder"
    );
    let queue = queue(root)?;
    let store = Store::at(config::store_root()?);
    let id = RequestId::parse(&stop.request)?;
    let request = queue.read::<Pending>(&id)?;
    let tip = match &request.phase {
        journal::Phase::Saved { tip, .. }
        | journal::Phase::Publishing { tip, .. }
        | journal::Phase::Blocked { tip: Some(tip), .. } => Some(tip.as_str()),
        _ => None,
    };
    if request.payload.repository != repository
        || tip != Some(due.tip.as_str())
        || request.payload.destination_branch(&store).ok().as_deref() != Some(due.branch.as_str())
    {
        due.cancel(repository)?;
        return Ok(true);
    }
    let Some(owner) = queue.try_own()? else {
        return Ok(true);
    };
    if !due.claim(repository)? {
        return Ok(true);
    }
    let mut seen = queue
        .pending()?
        .into_iter()
        .filter(|candidate| *candidate != id)
        .map(|id| id.as_str().to_owned())
        .collect();
    crate::commands::stop_requests::drain_owned(
        &store,
        &queue,
        &owner,
        &mut seen,
        false,
        Some(due.retry),
    )?;
    Ok(true)
}
