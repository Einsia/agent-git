//! Durable explicit intent is independent of automatic Stop markers and routing backups.

use super::*;

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Authorization {
    generation: u64,
    sessions: BTreeMap<String, u64>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    candidate: Candidate,
    root: PathBuf,
    repository: String,
    identity: RemoteIdentity,
    account: String,
    account_id: Option<String>,
    revisions: BTreeMap<PathBuf, (u64, u64)>,
    earlier: bool,
    named: bool,
}

#[derive(PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Destination {
    repository: String,
    identity: RemoteIdentity,
    account: String,
    account_id: Option<String>,
}

impl Destination {
    fn of(intent: &Intent) -> Self {
        Self {
            repository: intent.repository.clone(),
            identity: intent.identity.clone(),
            account: intent.account.clone(),
            account_id: intent.account_id.clone(),
        }
    }

    fn selected(repository: &str) -> Result<Option<Self>> {
        let (owner, name) = super::super::parse_slug(repository)?;
        let Some(repo) = Repo::open(config::repo_dir(&owner, &name)?) else {
            return Ok(None);
        };
        let hub = identity::normalize_hub(&config::hub_url())?;
        let Some(identity) = identity::read(&repo)?.filter(|identity| identity.hub == hub) else {
            return Ok(None);
        };
        let Some(account) = credentials::load_checked(&hub)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            repository: repository.to_owned(),
            identity,
            account: account.username,
            account_id: account.account_id,
        }))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    intent: Intent,
    running: bool,
    #[serde(default)]
    publication: Option<Publication>,
    #[serde(default)]
    wake_at: Option<i64>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Publication {
    branch: String,
    tip: String,
    automatic: bool,
    previous_tip: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Origin {
    root: PathBuf,
    request_id: Option<String>,
    intent: Option<Intent>,
    tip: String,
}

pub(super) struct Owned {
    root: PathBuf,
    id: String,
}

fn queue_lock() -> Result<fs::File> {
    lock_at(
        home()?.join("save-requests.lock"),
        "the save request journal lock",
    )
}

fn authorization_path(root: &Path) -> Result<PathBuf> {
    Ok(state_path(root)?.with_extension("save-authorization"))
}

fn authorization(root: &Path) -> Result<Authorization> {
    match fs::read(authorization_path(root)?) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Authorization::default()),
        Err(error) => Err(error.into()),
    }
}

fn sync_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn write(path: &Path, value: &impl Serialize) -> Result<()> {
    let directory = path.parent().context("save request has no directory")?;
    config::create_state_dir(directory)?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    file.write_all(&serde_json::to_vec(value)?)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    sync_dir(directory)?;
    if let Some(parent) = directory.parent() {
        sync_dir(parent)?;
    }
    Ok(())
}

fn increment(value: &mut u64) -> Result<()> {
    *value = value
        .checked_add(1)
        .context("save authorization revision exhausted")?;
    Ok(())
}

/// Policy publication and its cancellation revision share only the short journal lock. No
/// journal holder waits for a policy lock, transcript, capture, or network operation.
pub(super) fn save_policy(project: &Project) -> Result<()> {
    let _journal = queue_lock()?;
    let previous = read(&project.root)?;
    let mut auth = authorization(&project.root)?;
    let changed = previous.as_ref().is_none_or(|before| {
        before.identity != project.identity
            || before.repository != project.repository
            || before.account != project.account
            || before.account_id != project.account_id
            || before.enabled != project.enabled
            || before.auto_upload != project.auto_upload
    });
    if changed {
        increment(&mut auth.generation)?;
    }
    let added: Vec<_> = project
        .excluded
        .iter()
        .filter(|session| {
            previous
                .as_ref()
                .is_none_or(|before| !before.excluded.contains(*session))
        })
        .collect();
    for session in &added {
        increment(auth.sessions.entry((*session).clone()).or_default())?;
    }
    if changed || !added.is_empty() {
        // Cancellation lands first: interruption can cancel intent, never revive it.
        write(&authorization_path(&project.root)?, &auth)?;
    }
    write(&state_path(&project.root)?, project)
}

/// Kept outside Routing so restoring an earlier binding cannot restore its queued consent.
pub(super) fn invalidate_session(root: &Path, session: &str) -> Result<()> {
    let _journal = queue_lock()?;
    let mut auth = authorization(root)?;
    increment(auth.sessions.entry(session.to_owned()).or_default())?;
    write(&authorization_path(root)?, &auth)
}

pub(super) fn invalidate(root: &Path) -> Result<()> {
    let _journal = queue_lock()?;
    let mut auth = authorization(root)?;
    increment(&mut auth.generation)?;
    write(&authorization_path(root)?, &auth)
}

fn revisions(root: &Path, candidate: &Candidate) -> Result<BTreeMap<PathBuf, (u64, u64)>> {
    let session = key(&candidate.runtime, &candidate.session_id);
    candidate
        .cwd
        .ancestors()
        .chain(std::iter::once(root))
        .map(|path| {
            let auth = authorization(path)?;
            Ok((
                path.to_owned(),
                (
                    auth.generation,
                    auth.sessions.get(&session).copied().unwrap_or(0),
                ),
            ))
        })
        .collect()
}

fn directory(root: &Path) -> Result<PathBuf> {
    Ok(state_path(root)?.with_extension("save-requests"))
}

fn files(root: &Path, extension: &str) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(directory(root)?) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut files = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_some_and(|value| value == extension) {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

pub(super) fn pending(root: &Path) -> Result<bool> {
    for path in files(root, "request")? {
        let request: Request = serde_json::from_slice(&fs::read(path)?)?;
        if request
            .wake_at
            .is_none_or(|at| at <= super::super::upload_outcome::now())
        {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn contains(root: &Path, runtime: &str, session: &str) -> Result<bool> {
    for path in files(root, "request")? {
        let request: Request = serde_json::from_slice(&fs::read(path)?)?;
        if request.publication.is_none()
            && request.intent.candidate.runtime == runtime
            && request.intent.candidate.session_id == session
        {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn enqueue(
    root: &Path,
    candidate: Candidate,
    earlier: bool,
    named: bool,
) -> Result<String> {
    let candidate = Candidate {
        cwd: candidate.cwd.canonicalize()?,
        ..candidate
    };
    let _journal = queue_lock()?;
    let project = read(root)?.context("folder is not set up")?;
    ensure!(
        project.enabled && project.auto_upload,
        "folder is off; turn it on before saving"
    );
    let intent = Intent {
        revisions: revisions(root, &candidate)?,
        candidate,
        root: root.to_owned(),
        repository: project.repository,
        identity: project.identity,
        account: project.account,
        account_id: project.account_id,
        earlier,
        named,
    };
    // Only pending equivalent intent coalesces; a running capture may already have read its
    // transcript, and a sweep cannot replace a named admission.
    for path in files(root, "request")? {
        let pending: Request = serde_json::from_slice(&fs::read(&path)?)?;
        if !pending.running
            && pending.intent == intent
            && !path.with_extension("result").try_exists()?
        {
            return Ok(path
                .file_stem()
                .context("save request has no id")?
                .to_string_lossy()
                .into_owned());
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    let path = directory(root)?.join(format!("{id}.request"));
    write(
        &path,
        &Request {
            intent,
            running: false,
            publication: None,
            wake_at: None,
        },
    )?;
    Ok(id)
}

fn current(intent: &Intent, project: &Project) -> Result<bool> {
    let _journal = queue_lock()?;
    current_locked(intent, project)
}

fn current_locked(intent: &Intent, project: &Project) -> Result<bool> {
    if intent.revisions != revisions(&intent.root, &intent.candidate)?
        || project.root != intent.root
        || project.repository != intent.repository
        || project.identity != intent.identity
        || project.account != intent.account
        || project.account_id != intent.account_id
        || !project.enabled
        || !project.auto_upload
        || intent.candidate.cwd.canonicalize().ok().as_ref() != Some(&intent.candidate.cwd)
    {
        return Ok(false);
    }
    Ok(true)
}

fn result_value(id: &str, intent: &Intent, state: &str, branch: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "request_id": id, "runtime": intent.candidate.runtime,
        "session_id": intent.candidate.session_id, "status": state, "branch": branch,
        "destination": Destination::of(intent),
    })
}

fn request_path(root: &Path, id: &str) -> Result<PathBuf> {
    Ok(directory(root)?.join(format!("{id}.request")))
}

fn origin_path(repository: &str, branch: &str, tip: &str) -> Result<PathBuf> {
    Ok(home()?.join("save-origins").join(format!(
        "{}-{}-{}.origin",
        hash(repository),
        hash(branch),
        hash(tip)
    )))
}

fn read_origin(repository: &str, branch: &str, tip: &str) -> Result<Option<Origin>> {
    match fs::read(origin_path(repository, branch, tip)?) {
        Ok(bytes) => {
            let origin: Origin = serde_json::from_slice(&bytes)?;
            ensure!(
                origin.tip == tip,
                "upload origin does not match its captured tip"
            );
            Ok(Some(origin))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Recovery restores missing provenance before repairing a retry. A later admitted request
/// may own the same tip; the older request cannot recover or cancel that replacement's attempt.
fn owns_origin(root: &Path, id: &str, intent: &Intent, publication: &Publication) -> Result<bool> {
    match read_origin(&intent.repository, &publication.branch, &publication.tip)? {
        Some(origin) => Ok(origin.root == root && origin.request_id.as_deref() == Some(id)),
        None => {
            write(
                &origin_path(&intent.repository, &publication.branch, &publication.tip)?,
                &Origin {
                    root: root.to_owned(),
                    request_id: Some(id.to_owned()),
                    intent: None,
                    tip: publication.tip.clone(),
                },
            )?;
            Ok(true)
        }
    }
}

/// Origin is durable before an upload can create an executable retry. It stays after terminal
/// cancellation, so an interrupted retry cannot become an ordinary automatic upload.
pub(super) fn before_upload(
    project: &Project,
    candidate: &Candidate,
    branch: &str,
    tip: &str,
    automatic: bool,
    owned: Option<&Owned>,
) -> Result<()> {
    let root = &project.root;
    let repository = &project.repository;
    let _journal = queue_lock()?;
    let origin = origin_path(repository, branch, tip)?;
    match owned {
        Some(owned) => {
            ensure!(
                owned.root.as_path() == root.as_path(),
                "save request root changed"
            );
            let path = request_path(root, &owned.id)?;
            let mut request: Request = serde_json::from_slice(&fs::read(&path)?)?;
            request.publication = Some(Publication {
                branch: branch.into(),
                tip: tip.into(),
                automatic,
                previous_tip: super::super::upload_outcome::publication_tip(repository, branch)?,
            });
            write(
                &origin,
                &Origin {
                    root: root.to_owned(),
                    request_id: Some(owned.id.clone()),
                    intent: None,
                    tip: tip.into(),
                },
            )?;
            save_request_crash_point("between_origin_and_publication");
            write(&path, &request)?;
        }
        None => {
            // Every admitted capture keeps its own authorization at its tip. A newer tip
            // must not erase the provenance needed to fence an older executable retry.
            let intent = Intent {
                candidate: candidate.clone(),
                root: root.clone(),
                repository: repository.clone(),
                identity: project.identity.clone(),
                account: project.account.clone(),
                account_id: project.account_id.clone(),
                revisions: revisions(root, candidate)?,
                earlier: false,
                named: false,
            };
            write(
                &origin,
                &Origin {
                    root: root.clone(),
                    request_id: None,
                    intent: Some(intent),
                    tip: tip.into(),
                },
            )?;
        }
    }
    Ok(())
}

fn terminal(value: &serde_json::Value) -> bool {
    !matches!(value["status"].as_str(), Some("pending" | "retrying"))
}

fn finish(root: &Path, id: &str, intent: &Intent, state: &str, branch: Option<&str>) -> Result<()> {
    let _journal = queue_lock()?;
    let path = request_path(root, id)?;
    write(
        &path.with_extension("result"),
        &result_value(id, intent, state, branch),
    )?;
    fs::remove_file(path)?;
    sync_dir(&directory(root)?)
}

fn wait_for_retry(root: &Path, id: &str, at: i64) -> Result<()> {
    let _journal = queue_lock()?;
    let path = request_path(root, id)?;
    let mut request: Request = serde_json::from_slice(&fs::read(&path)?)?;
    request.running = false;
    request.wake_at = Some(at);
    write(&path, &request)?;
    write(
        &path.with_extension("result"),
        &result_value(
            id,
            &request.intent,
            "retrying",
            request
                .publication
                .as_ref()
                .map(|publication| publication.branch.as_str()),
        ),
    )
}

fn reconcile(
    root: &Path,
    id: &str,
    intent: &Intent,
    publication: &Publication,
) -> Result<Option<i64>> {
    use super::super::upload_outcome::RequestPublication;
    let state = {
        let _journal = queue_lock()?;
        if owns_origin(root, id, intent, publication)? {
            super::super::upload_outcome::request_publication(
                &intent.repository,
                &publication.branch,
                &publication.tip,
                publication.automatic,
                publication.previous_tip.as_deref(),
            )?
        } else {
            RequestPublication::Superseded
        }
    };
    match state {
        RequestPublication::RetryAt(at) => {
            wait_for_retry(root, id, at)?;
            Ok(Some(at))
        }
        state => {
            let status = match state {
                RequestPublication::Uploaded => "uploaded",
                RequestPublication::Failed => "failed",
                RequestPublication::Cancelled => "cancelled",
                RequestPublication::Superseded => "superseded",
                RequestPublication::RetryAt(_) => unreachable!(),
            };
            finish(root, id, intent, status, Some(&publication.branch))?;
            Ok(None)
        }
    }
}

/// Capture ownership recovers running requests. Each intent stays until a terminal receipt;
/// retry records are reconciled independently for every accepted request on the same branch.
pub(super) fn drain(root: &Path) -> Result<Option<i64>> {
    let mut next = None;
    for path in files(root, "request")? {
        let id = path
            .file_stem()
            .and_then(|name| name.to_str())
            .context("invalid save request id")?;
        let request = {
            let _journal = queue_lock()?;
            let receipt = path.with_extension("result");
            if receipt.try_exists()? && terminal(&serde_json::from_slice(&fs::read(receipt)?)?) {
                fs::remove_file(&path)?;
                sync_dir(&directory(root)?)?;
                continue;
            }
            let mut request: Request = serde_json::from_slice(&fs::read(&path)?)?;
            request.running = true;
            write(&path, &request)?;
            request
        };
        let intent = &request.intent;
        let _policy = lock(root)?;
        let project = read(root)?;
        let valid = match project.as_ref() {
            Some(project) => {
                current(intent, project)?
                    && governing(&intent.candidate.cwd)?
                        .is_some_and(|(live, _)| live.root == intent.root)
                    && verify(project).is_ok()
            }
            None => false,
        };
        if !valid {
            if let Some(publication) = &request.publication {
                let _journal = queue_lock()?;
                if owns_origin(root, id, intent, publication)? {
                    for due in super::super::upload_outcome::retries(
                        &intent.repository,
                        std::slice::from_ref(&publication.branch),
                    ) {
                        if due.tip == publication.tip {
                            due.cancel(&intent.repository)?;
                        }
                    }
                }
            }
            finish(
                root,
                id,
                intent,
                "cancelled",
                request
                    .publication
                    .as_ref()
                    .map(|value| value.branch.as_str()),
            )?;
            continue;
        }
        if let Some(publication) = &request.publication {
            if let Some(at) = reconcile(root, id, intent, publication)? {
                next = Some(next.map_or(at, |before: i64| before.min(at)));
            }
            continue;
        }
        let mut project = project.context("save request lost its policy")?;
        let _ = look_for_earlier_opencode_locked(&mut project);
        let _ = look_for_earlier_pi_locked(&mut project);
        let owned = Owned {
            root: root.to_owned(),
            id: id.to_owned(),
        };
        let outcome = save_selected(
            project,
            intent.candidate.clone(),
            intent.earlier,
            intent.named,
            None,
            Some(&owned),
        );
        save_request_crash_point("after_upload");
        let (state, branch) = match &outcome {
            Ok(Saved::Uploaded(branch, _)) => ("uploaded", Some(branch.as_str())),
            Ok(Saved::Pending(branch)) => ("waiting_for_turn", Some(branch.as_str())),
            Ok(Saved::Refused(_)) => ("refused", None),
            Ok(Saved::Elsewhere { .. }) => ("elsewhere", None),
            Ok(Saved::Rewritten { .. }) => ("refused", None),
            Ok(Saved::Failed(_)) | Err(_) => ("failed", None),
            Ok(Saved::Queued { .. }) => {
                unreachable!("a capture owner does not queue its own request")
            }
        };
        if state == "failed" {
            let latest: Request = serde_json::from_slice(&fs::read(&path)?)?;
            if let Some(publication) = &latest.publication {
                if let Some(at) = reconcile(root, id, intent, publication)? {
                    next = Some(next.map_or(at, |before: i64| before.min(at)));
                }
                continue;
            }
        }
        finish(root, id, intent, state, branch)?;
    }
    Ok(next)
}

pub(super) fn retry_owner(
    repository: &str,
    due: &super::super::upload_outcome::Due,
) -> Result<Option<PathBuf>> {
    Ok(read_origin(repository, &due.branch, &due.tip)?.map(|origin| origin.root))
}

pub(super) fn retry_allowed(
    root: &Path,
    repository: &str,
    due: &super::super::upload_outcome::Due,
) -> Result<bool> {
    let Some(origin) = read_origin(repository, &due.branch, &due.tip)? else {
        return Ok(true);
    };
    if origin.root != root {
        return Ok(false);
    }
    let intent = match origin.request_id {
        Some(id) => {
            let path = request_path(root, &id)?;
            if !path.try_exists()? {
                return Ok(false);
            }
            let request: Request = serde_json::from_slice(&fs::read(path)?)?;
            request.intent
        }
        None => origin
            .intent
            .context("automatic upload origin lacks its authorization")?,
    };
    if intent.root != root || intent.repository != repository {
        return Ok(false);
    }
    let Some(project) = read(root)? else {
        return Ok(false);
    };
    Ok(current(&intent, &project)?
        && governing(&intent.candidate.cwd)?.is_some_and(|(live, _)| live.root == intent.root)
        && verify(&project).is_ok()
        && matches!(placement(&project, &intent.candidate), Ok(Placement::Claim(branch)) if branch == due.branch)
        && !project.leaves_out(&key(
            &intent.candidate.runtime,
            &intent.candidate.session_id,
        ))
        && !delegation_withheld(
            &project.repository,
            &intent.candidate,
            Some(&project),
            false,
        )?)
}

pub(super) fn status(root: &Path) -> Result<Vec<serde_json::Value>> {
    if !directory(root)?.try_exists()? {
        return Ok(Vec::new());
    }
    let _journal = queue_lock()?;
    let mut rows: Vec<serde_json::Value> = Vec::new();
    for path in files(root, "result")? {
        rows.push(serde_json::from_slice(&fs::read(path)?)?);
    }
    for path in files(root, "request")? {
        let request: Request = serde_json::from_slice(&fs::read(&path)?)?;
        let id = path
            .file_stem()
            .and_then(|name| name.to_str())
            .context("invalid save request id")?;
        let valid = match read(root)? {
            Some(project) => current_locked(&request.intent, &project)?,
            None => false,
        };
        if let Some(row) = rows.iter_mut().find(|row| row["request_id"] == id) {
            if !valid && !terminal(row) {
                row["status"] = serde_json::json!("cancelled");
            }
        } else {
            rows.push(result_value(
                id,
                &request.intent,
                if valid { "pending" } else { "cancelled" },
                None,
            ));
        }
    }
    Ok(rows)
}

/// Receipts retain their original destination even after the folder is rebound or unbound.
/// A receipt without that identity cannot be attributed from the folder's current policy.
pub(super) fn report(
    rows: Vec<serde_json::Value>,
    repository: Option<&str>,
) -> Result<SaveRequestStatus> {
    if rows.is_empty() {
        return Ok(SaveRequestStatus::default());
    }
    let selected = repository.map(Destination::selected).transpose()?.flatten();
    let hub = identity::normalize_hub(&config::hub_url())?;
    let account = credentials::load_checked(&hub)?;
    let mut report = SaveRequestStatus::default();
    for row in rows {
        let destination = serde_json::from_value::<Destination>(row["destination"].clone()).ok();
        if destination.as_ref().is_some_and(|destination| {
            destination.identity.hub == hub
                && account.as_ref().is_some_and(|account| {
                    destination.account == account.username
                        && destination.account_id == account.account_id
                })
                && (repository.is_none() || selected.as_ref() == Some(destination))
        }) {
            report.items.push(serde_json::json!({
                "request_id": row["request_id"], "runtime": row["runtime"],
                "session_id": row["session_id"], "status": row["status"],
                "branch": row["branch"],
            }));
        } else {
            report.omitted += 1;
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Folder ancestry and rebinding cannot relabel old requests as the selected repository's.
    /// Terminal receipts must retain the same identity as pending intent after its file is gone.
    #[test]
    fn request_reports_keep_original_destinations_across_folder_bindings() {
        if crate::rc::in_isolated_test(
            "commands::project::save_requests::tests::request_reports_keep_original_destinations_across_folder_bindings",
        ) {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("parent");
        let child = root.join("child");
        fs::create_dir_all(&child).unwrap();
        unsafe {
            std::env::set_var("AGIT_HOME", temp.path().join("agit"));
            std::env::set_var("AGIT_HUB_URL", "https://example.test");
        }
        let parent = super::super::tests::project(&root);
        let mut nested = super::super::tests::project(&child);
        nested.repository = "alice/child".into();
        nested.identity.agent_id = "bbbbbbbb-0000-4000-8000-000000000003".into();
        let mut credential = credentials::HubCredential {
            account_id: None,
            username: "alice".into(),
            email: None,
            hub: Some(parent.identity.hub.clone()),
            access_token: "synthetic-access".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_token: "synthetic-refresh".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        };
        credentials::save(&parent.identity.hub, &credential).unwrap();
        let bind = |project: &Project| {
            let (owner, name) = super::super::super::parse_slug(&project.repository).unwrap();
            let repo = Repo::init(&config::repo_dir(&owner, &name).unwrap()).unwrap();
            identity::pin(&repo, &project.identity).unwrap();
            save_policy(project).unwrap();
            repo
        };
        bind(&parent);
        bind(&nested);
        let queue = |project: &Project, number: u64, terminal: bool| {
            let id = enqueue(
                &project.root,
                Candidate {
                    runtime: "claude-code".into(),
                    session_id: format!("aaaaaaaa-0000-4000-8000-{number:012}"),
                    cwd: project.root.clone(),
                },
                false,
                true,
            )
            .unwrap();
            if terminal {
                let path = request_path(&project.root, &id).unwrap();
                let request: Request = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                finish(
                    &project.root,
                    &id,
                    &request.intent,
                    "uploaded",
                    Some("saved-branch"),
                )
                .unwrap();
                assert!(!path.exists());
            }
            id
        };
        queue(&parent, 1, false);
        queue(&parent, 2, true);
        let pending = queue(&nested, 3, false);
        let uploaded = queue(&nested, 4, true);
        let selected = save_request_status(&child, Some(&nested.repository)).unwrap();
        assert_eq!(selected.omitted, 2);
        assert_eq!(selected.items.len(), 2);
        assert!(
            selected
                .items
                .iter()
                .any(|row| row["request_id"] == pending)
        );
        assert!(
            selected
                .items
                .iter()
                .any(|row| row["request_id"] == uploaded)
        );
        assert!(
            selected
                .items
                .iter()
                .all(|row| row.get("destination").is_none())
        );

        nested.repository = "alice/rebound".into();
        nested.identity.agent_id = "bbbbbbbb-0000-4000-8000-000000000004".into();
        let repo = bind(&nested);
        let rebound = queue(&nested, 5, false);
        let selected = save_request_status(&child, Some(&nested.repository)).unwrap();
        assert_eq!(selected.omitted, 4);
        assert_eq!(selected.items.len(), 1);
        assert_eq!(selected.items[0]["request_id"], rebound);

        let old_identity = nested.identity.clone();
        nested.identity.agent_id = "bbbbbbbb-0000-4000-8000-000000000005".into();
        identity::rebind(&repo, &old_identity, &nested.identity).unwrap();
        save_policy(&nested).unwrap();
        let replacement = queue(&nested, 6, true);
        let selected = save_request_status(&child, Some(&nested.repository)).unwrap();
        assert_eq!(selected.omitted, 5);
        assert_eq!(selected.items.len(), 1);
        assert_eq!(selected.items[0]["request_id"], replacement);

        write(
            &directory(&nested.root).unwrap().join("legacy.result"),
            &serde_json::json!({"request_id": "legacy", "session_id": "unattributed"}),
        )
        .unwrap();
        let all = save_request_status(&child, None).unwrap();
        assert_eq!(all.items.len(), 6);
        assert_eq!(all.omitted, 1);
        credential.account_id = Some("cccccccc-0000-4000-8000-000000000001".into());
        credentials::save(&parent.identity.hub, &credential).unwrap();
        for repository in [None, Some(nested.repository.as_str())] {
            let selected = save_request_status(&child, repository).unwrap();
            assert!(selected.items.is_empty());
            assert_eq!(selected.omitted, 7);
        }
        unsafe {
            std::env::set_var("AGIT_HUB_URL", "https://other.example.test");
        }
        let all = save_request_status(&child, None).unwrap();
        assert!(all.items.is_empty());
        assert_eq!(all.omitted, 7);
    }

    /// Equivalent pending intent coalesces, but cancellation survives policy restoration and
    /// same-name replacement. Otherwise an old named request could lift a later refusal.
    #[test]
    fn queued_consent_does_not_survive_its_authorization() {
        if crate::rc::in_isolated_test(
            "commands::project::save_requests::tests::queued_consent_does_not_survive_its_authorization",
        ) {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("AGIT_HOME", temp.path().join("agit"));
        }
        let root = temp.path().canonicalize().unwrap();
        let mut project = super::super::tests::project(&root);
        save_policy(&project).unwrap();
        let candidate = Candidate {
            runtime: "claude-code".into(),
            session_id: "aaaaaaaa-0000-4000-8000-000000000001".into(),
            cwd: root.clone(),
        };
        let request = |earlier, named| {
            let id = enqueue(&root, candidate.clone(), earlier, named).unwrap();
            let record: Request = serde_json::from_slice(
                &fs::read(directory(&root).unwrap().join(format!("{id}.request"))).unwrap(),
            )
            .unwrap();
            (id, record.intent)
        };
        let (id, intent) = request(false, true);
        assert_eq!(request(false, true).0, id);
        assert_ne!(request(true, false).0, id);
        project.last_result = Some(serde_json::json!({"ok": true}));
        save_policy(&project).unwrap();
        assert!(
            current(&intent, &project).unwrap(),
            "an upload result does not revoke intent"
        );
        project.enabled = false;
        save_policy(&project).unwrap();
        project.enabled = true;
        save_policy(&project).unwrap();
        assert!(
            !current(&intent, &project).unwrap(),
            "off/on cannot revive consent"
        );
        let (_, dropped) = request(false, true);
        exclude_locked(&root, &key(&candidate.runtime, &candidate.session_id)).unwrap();
        project = read(&root).unwrap().unwrap();
        assert!(!current(&dropped, &project).unwrap());
        let (_, named_again) = request(false, true);
        exclude_locked(&root, &key(&candidate.runtime, &candidate.session_id)).unwrap();
        assert!(
            !current(&named_again, &project).unwrap(),
            "repeating drop also cancels renewed intent"
        );
        let (_, moving) = request(false, true);
        let routing = unbind_for_move(&root).unwrap();
        routing.restore(&root, "alice/other").unwrap();
        project = read(&root).unwrap().unwrap();
        assert!(
            !current(&moving, &project).unwrap(),
            "routing rollback cannot roll consent back"
        );
        let (_, recreated) = request(false, true);
        project.identity.agent_id = "bbbbbbbb-0000-4000-8000-000000000003".into();
        save_policy(&project).unwrap();
        assert!(!current(&recreated, &project).unwrap());
        let (_, nested) = request(false, true);
        invalidate(&root).unwrap();
        assert!(
            !current(&nested, &project).unwrap(),
            "folder refusal is independent of a policy rewrite"
        );
    }
    /// A successor arrives owning capture and retains it until queued work has a durable
    /// outcome. A contender admitted at handoff could make the successor leave its work behind.
    #[test]
    fn a_successor_transfers_capture_ownership_into_the_drain() {
        if crate::rc::in_isolated_test(
            "commands::project::save_requests::tests::a_successor_transfers_capture_ownership_into_the_drain",
        ) {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("AGIT_HOME", temp.path().join("agit"));
        }
        let root = temp.path().canonicalize().unwrap();
        let project = super::super::tests::project(&root);
        save_policy(&project).unwrap();
        let candidate = Candidate {
            runtime: "claude-code".into(),
            session_id: "aaaaaaaa-0000-4000-8000-000000000002".into(),
            cwd: root.clone(),
        };
        let id = enqueue(&root, candidate, false, true).unwrap();
        // Marking it running models a worker that ended before acknowledging the request.
        let path = directory(&root).unwrap().join(format!("{id}.request"));
        let mut request: Request = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        request.running = true;
        write(&path, &request).unwrap();
        invalidate(&root).unwrap();
        let acquired = wait_for_worker(&root).unwrap();
        *CAPTURE_HANDOFF.lock().unwrap() = Some(|root| {
            assert!(
                capture_worker(root).unwrap().is_none(),
                "capture ownership must not be released at handoff"
            );
        });
        drain_owned(&root, false, acquired).unwrap();
        assert!(
            !pending(&root).unwrap(),
            "recovered intent must reach a terminal receipt"
        );
        assert_eq!(status(&root).unwrap()[0]["status"], "cancelled");
        assert!(capture_worker(&root).unwrap().is_some());
    }
    /// Results belong to the captured tip and request, not merely to the branch. A newer
    /// publication cannot turn an older request into an uploaded result or erase its receipt.
    #[test]
    fn same_branch_requests_keep_independent_results_for_their_captured_tips() {
        if crate::rc::in_isolated_test(
            "commands::project::save_requests::tests::same_branch_requests_keep_independent_results_for_their_captured_tips",
        ) {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("AGIT_HOME", temp.path().join("agit"));
        }
        let root = temp.path().canonicalize().unwrap();
        let project = super::super::tests::project(&root);
        save_policy(&project).unwrap();
        let candidate = Candidate {
            runtime: "claude-code".into(),
            session_id: "aaaaaaaa-0000-4000-8000-000000000003".into(),
            cwd: root.clone(),
        };
        let first = enqueue(&root, candidate.clone(), false, true).unwrap();
        let path = request_path(&root, &first).unwrap();
        let mut request: Request = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        request.running = true;
        write(&path, &request).unwrap();
        let second = enqueue(&root, candidate.clone(), false, true).unwrap();
        assert_ne!(first, second, "running intent cannot absorb a later save");
        let old_tip = "1".repeat(40);
        let new_tip = "2".repeat(40);
        for (id, tip) in [(&first, &old_tip), (&second, &new_tip)] {
            before_upload(
                &project,
                &candidate,
                "work",
                tip,
                true,
                Some(&Owned {
                    root: root.clone(),
                    id: id.clone(),
                }),
            )
            .unwrap();
        }
        crate::commands::upload_outcome::recorded(
            crate::commands::upload_outcome::Place::of(&project.repository, "work"),
            &new_tip,
            || (),
            |_| None,
        )
        .unwrap();
        for id in [&first, &second] {
            let request: Request =
                serde_json::from_slice(&fs::read(request_path(&root, id).unwrap()).unwrap())
                    .unwrap();
            assert!(
                reconcile(
                    &root,
                    id,
                    &request.intent,
                    request.publication.as_ref().unwrap()
                )
                .unwrap()
                .is_none()
            );
        }
        let rows = status(&root).unwrap();
        assert_eq!(
            rows.iter().find(|row| row["request_id"] == first).unwrap()["status"],
            "superseded"
        );
        assert_eq!(
            rows.iter().find(|row| row["request_id"] == second).unwrap()["status"],
            "uploaded"
        );
        assert!(!pending(&root).unwrap());

        let third = enqueue(&root, candidate.clone(), false, true).unwrap();
        let next_tip = "3".repeat(40);
        before_upload(
            &project,
            &candidate,
            "work",
            &next_tip,
            true,
            Some(&Owned {
                root: root.clone(),
                id: third.clone(),
            }),
        )
        .unwrap();
        let request: Request =
            serde_json::from_slice(&fs::read(request_path(&root, &third).unwrap()).unwrap())
                .unwrap();
        fs::remove_file(origin_path(&project.repository, "work", &next_tip).unwrap()).unwrap();
        assert!(
            reconcile(
                &root,
                &third,
                &request.intent,
                request.publication.as_ref().unwrap()
            )
            .unwrap()
            .is_some(),
            "the prior tip's result cannot supersede a request that has not started its attempt"
        );
        let due =
            super::super::super::upload_outcome::retries(&project.repository, &["work".into()])
                .pop()
                .unwrap();
        assert_eq!(
            retry_owner(&project.repository, &due).unwrap(),
            Some(root.clone()),
            "recovery must restore provenance before making an attempt executable"
        );
        crate::commands::upload_outcome::recorded(
            crate::commands::upload_outcome::Place::of(&project.repository, "work"),
            &next_tip,
            || (),
            |_| None,
        )
        .unwrap();
        assert!(
            reconcile(
                &root,
                &third,
                &request.intent,
                request.publication.as_ref().unwrap()
            )
            .unwrap()
            .is_none()
        );
        let rows = status(&root).unwrap();
        assert_eq!(
            rows.iter().find(|row| row["request_id"] == third).unwrap()["status"],
            "uploaded"
        );
    }
    /// A newer capture from another binding must not erase an older tip's source. An obsolete
    /// request also cannot cancel an attempt that another admitted source now owns at its tip.
    #[test]
    fn retry_provenance_follows_each_tip_and_its_current_owner() {
        if crate::rc::in_isolated_test(
            "commands::project::save_requests::tests::retry_provenance_follows_each_tip_and_its_current_owner",
        ) {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("AGIT_HOME", temp.path().join("agit"));
        }
        let base = temp.path().canonicalize().unwrap();
        let root = base.join("source");
        let other = base.join("other");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&other).unwrap();
        let project = super::super::tests::project(&root);
        let replacement = super::super::tests::project(&other);
        save_policy(&project).unwrap();
        save_policy(&replacement).unwrap();
        let candidate = Candidate {
            runtime: "claude-code".into(),
            session_id: "aaaaaaaa-0000-4000-8000-000000000004".into(),
            cwd: root.clone(),
        };
        let replacement_candidate = Candidate {
            cwd: other.clone(),
            ..candidate.clone()
        };
        let tip = "1".repeat(40);
        let later = "2".repeat(40);
        let enqueue_attempt = || {
            let id = enqueue(&root, candidate.clone(), false, true).unwrap();
            before_upload(
                &project,
                &candidate,
                "work",
                &tip,
                true,
                Some(&Owned {
                    root: root.clone(),
                    id: id.clone(),
                }),
            )
            .unwrap();
            super::super::super::upload_outcome::recorded(
                super::super::super::upload_outcome::Place::of(&project.repository, "work")
                    .map(|place| place.retried(0)),
                &tip,
                || (),
                |_| {
                    Some(super::super::super::upload_outcome::Refusal::child(
                        Some(crate::ExitCode::Network.as_i32()),
                        "synthetic network failure".into(),
                    ))
                },
            )
            .unwrap();
            id
        };
        let first = enqueue_attempt();
        before_upload(
            &replacement,
            &replacement_candidate,
            "work",
            &later,
            true,
            None,
        )
        .unwrap();
        let due =
            super::super::super::upload_outcome::retries(&project.repository, &["work".into()])
                .pop()
                .unwrap();
        assert_eq!(
            retry_owner(&project.repository, &due).unwrap(),
            Some(root.clone())
        );
        invalidate(&root).unwrap();
        drain(&root).unwrap();
        assert!(
            super::super::super::upload_outcome::retries(&project.repository, &["work".into()],)
                .is_empty()
        );
        assert_eq!(
            status(&root)
                .unwrap()
                .iter()
                .find(|row| row["request_id"] == first)
                .unwrap()["status"],
            "cancelled"
        );

        let second = enqueue_attempt();
        before_upload(
            &replacement,
            &replacement_candidate,
            "work",
            &tip,
            true,
            None,
        )
        .unwrap();
        invalidate(&root).unwrap();
        drain(&root).unwrap();
        let due =
            super::super::super::upload_outcome::retries(&project.repository, &["work".into()])
                .pop()
                .expect("an obsolete request cannot cancel the replacement source's retry");
        assert_eq!(retry_owner(&project.repository, &due).unwrap(), Some(other));
        assert_eq!(
            status(&root)
                .unwrap()
                .iter()
                .find(|row| row["request_id"] == second)
                .unwrap()["status"],
            "cancelled"
        );
    }
}
