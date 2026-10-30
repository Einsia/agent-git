//! Registered session modes govern introduced objects; accepted ancestry keeps its identity.

use super::*;
use crate::domain::privacy_envelope::digest_bytes;
use crate::domain::session_privacy::{
    AcceptedSnapshot, PublishedSnapshot, SESSION_ENVELOPE_VERSION, SessionPrivacy,
    SessionPublicationManifest, SessionScheme, SessionSnapshotBinding, SourceCommitter,
};
use crate::hub::{
    git::RemoteRefs, identity::RemoteIdentity, privacy::sessions::SessionPrivacyResponse,
};

#[path = "session_sources.rs"]
mod sources;

const CANDIDATE_CACHE_VERSION: u32 = 3;

pub struct SessionPublicationPlan {
    pub identity: RemoteIdentity,
    pub remote: RemoteRefs,
    pub configs: BTreeMap<String, SessionPrivacyResponse>,
    pub(crate) delivery: Option<super::delivery::Delivery>,
    pub(crate) sources: Option<SourceHistory>,
    /// Repositories, `owner/repo`, that introduced snapshots may keep naming as their fork source,
    /// each with the repository identity that vouches for it: the destination itself, and any that
    /// an anonymous lookup on this Hub found public while this publication was resolved. Every
    /// other `source_ref` is withheld with its commit.
    pub(crate) fork_sources: BTreeMap<String, String>,
    /// The repository identity this checkout is pinned to on the destination's Hub, read when the
    /// publication was resolved: a `fork_origin` without a snapshot points into it.
    pub(crate) pinned_source: Option<String>,
}

/// Committed facts belong to the same object store and frozen selection. Remote authority and
/// accepted mappings are read separately because they can change while preparation runs.
pub(crate) struct SourceHistory {
    root: std::path::PathBuf,
    plan: PublicationPlan,
    history: sources::History,
    enveloped: BTreeSet<String>,
}

impl SourceHistory {
    pub(super) fn read(source: &Repo, plan: &PublicationPlan) -> Result<Self> {
        Ok(Self {
            root: source.common_dir()?.canonicalize()?,
            plan: plan.clone(),
            history: sources::History::read(source, plan)?,
            enveloped: enveloped_commits(source, plan.commit_objects())?,
        })
    }

    fn matches(&self, source: &Repo, plan: &PublicationPlan) -> Result<bool> {
        Ok(self.plan == *plan && self.root == source.common_dir()?.canonicalize()?)
    }
}

/// Raw parents with the boundary's left out: a plan does not carry the boundary's ancestry.
fn walk_parents(
    mut parents: BTreeMap<String, Vec<String>>,
    boundary: &[String],
) -> BTreeMap<String, Vec<String>> {
    for oid in boundary {
        if let Some(parents) = parents.get_mut(oid) {
            parents.clear();
        }
    }
    parents
}

/// The selected history a publication carries: from the heads down to the history the
/// destination already holds, so the publication's bounds apply to what it adds. Each boundary
/// commit must be one the branch itself accepted, or history the Hub holds under its own id. One
/// only another branch accepted has its ancestry outside this branch's record, so it makes the
/// publication carry the whole history. So does a cross-Hub delivery: it admits each session from
/// its birth, and below a boundary the plan would name a later commit as the birth.
fn freeze_publication(
    source: &Repo,
    branches: &[String],
    held: &BTreeSet<String>,
    ledger: &accepted::Ledger,
    delivering: bool,
) -> Result<PublicationPlan> {
    if delivering {
        return PublicationPlan::freeze_selected(source, branches);
    }
    let mut stops = ledger.accepted_sources(branches);
    stops.extend(held.iter().cloned());
    let plan = PublicationPlan::freeze_selected_after(source, branches, &stops)?;
    if plan.boundary().is_empty() {
        return Ok(plan);
    }
    let parents = walk_parents(
        commit_parent_map(source, plan.commit_objects())?,
        plan.boundary(),
    );
    for head in plan.heads() {
        let branch = head
            .name()
            .strip_prefix("refs/heads/")
            .context("invalid publication branch")?;
        for oid in ancestors(head.oid(), &parents)? {
            if plan.boundary().binary_search(&oid).is_err() {
                continue;
            }
            if ledger.get(branch, &oid).is_none() && !held.contains(&oid) {
                return PublicationPlan::freeze_selected(source, branches);
            }
        }
    }
    Ok(plan)
}

impl SessionPublicationPlan {
    #[allow(clippy::too_many_arguments)]
    pub fn resolve(
        source: &Repo,
        branches: &[String],
        client: &crate::hub::Client,
        repository: &str,
        identity: RemoteIdentity,
        remote: RemoteRefs,
        register: bool,
        explicit_hub: bool,
    ) -> Result<Self> {
        ensure!(
            crate::domain::session_privacy::canonical_hub(client.base())? == identity.hub,
            "session publication belongs to another Hub"
        );
        let held = PublicationPlan::held_history(source, &remote.heads)?;
        let mut ledger = accepted::Ledger::open(source, &identity)?;
        ledger.reconcile(&remote)?;
        let delivering = explicit_hub || super::delivery::Delivery::active(source, &identity)?;
        let plan = freeze_publication(source, branches, &held, &ledger, delivering)?;
        let sources = SourceHistory::read(source, &plan)?;
        let history = &sources.history;
        require_remote_ancestry(source, &plan, &ledger, &remote, repository)?;
        let accepted = plan
            .commit_objects()
            .iter()
            .filter(|oid| held.contains(*oid))
            .cloned()
            .collect::<BTreeSet<_>>();
        let enveloped = &sources.enveloped;
        let mut sessions = BTreeSet::new();
        let mut source_hubs = BTreeSet::new();
        let mut named_sources = BTreeSet::new();
        for oid in plan.commit_objects() {
            let mut inherited = false;
            for branch in branches {
                inherited |= ledger.inherited_immutable(branch, oid)?.is_some();
            }
            if accepted.contains(oid) || inherited {
                continue;
            }
            let metadata = history.metadata(source, oid)?;
            if let Some(account) = &metadata.committer_account {
                source_hubs.insert(account.hub.clone());
            }
            if let Some(hub) = metadata
                .session_privacy
                .as_ref()
                .and_then(|privacy| privacy.hub.as_ref())
            {
                source_hubs.insert(hub.clone());
            }
            ensure!(
                !enveloped.contains(oid),
                "unaccepted ciphertext requires its original publication authority"
            );
            if let Some(repository) = fork_source_on(&metadata, &identity.hub) {
                named_sources.insert(repository.to_owned());
            }
            if metadata.is_session_line() {
                ensure!(
                    meta::is_bare_id(&metadata.session),
                    "save a native snapshot before publishing an unclaimed session"
                );
                sessions.insert(metadata.session);
            }
        }
        for session in &sessions {
            let birth = &history.births[session];
            if let Some(account) = &birth.committer {
                source_hubs.insert(account.hub.clone());
            }
            if let Some(hub) = birth
                .privacy
                .as_ref()
                .and_then(|privacy| privacy.hub.as_ref())
            {
                source_hubs.insert(hub.clone());
            }
        }
        let mut delivery = super::delivery::Delivery::prepare(
            source,
            &identity,
            client,
            explicit_hub,
            &sessions,
            register,
        )?;
        // Every selected source is admitted before registering any destination session.
        if let Some(delivery) = &mut delivery {
            delivery.load_source_accounts(&source_hubs)?;
            for session in &sessions {
                let birth = &history.births[session];
                let privacy = match &birth.privacy {
                    Some(privacy) => privacy.clone(),
                    None => SessionPrivacy::new(birth.committer.clone())?,
                };
                delivery
                    .admit_session(session, &privacy)
                    .with_context(|| format!("cannot deliver session {session}"))?;
            }
        }
        let mut configs = BTreeMap::new();
        for session in sessions {
            let target_session = match &delivery {
                Some(delivery) => delivery.session(&session)?.to_owned(),
                None => session.clone(),
            };
            let _mode_lock = super::modes::lock(source, &identity.hub, &session)?;
            let mut choice = if delivery.is_some() {
                None
            } else {
                super::modes::load(source, &identity.hub, &session)?
            };
            if let Some(choice) = &choice {
                choice.verify(&identity, repository)?;
            }
            let response = match client.registered_session_privacy(
                repository,
                &identity,
                &target_session,
            )? {
                // A session already registered in this repository continues under its stored
                // configuration, whatever this client chose or recorded.
                Some(response) => response,
                None => {
                    ensure!(
                        register,
                        "session {session} must be registered before a dry-run publication"
                    );
                    let birth = history
                        .births
                        .get(&session)
                        .context("session has no source birth evidence")?;
                    let account = client.session_account()?;
                    let mut privacy = match &birth.privacy {
                        Some(privacy) => privacy.clone(),
                        None => super::super::session_privacy::SessionPrivacy::new(
                            birth.committer.clone(),
                        )?,
                    };
                    if let Some(delivery) = &delivery {
                        privacy = delivery.registration(&session)?.clone();
                    }
                    if privacy.owner_source
                        == super::super::session_privacy::OwnerResolution::Unresolved
                    {
                        let scheme = privacy.scheme;
                        privacy = super::super::session_privacy::SessionPrivacy::new(Some(
                            account.clone(),
                        ))?;
                        privacy.scheme = scheme;
                    }
                    if let Some(choice) = &mut choice {
                        ensure!(
                            !explicit_hub,
                            "a cross-Hub delivery cannot reuse the source session mode"
                        );
                        ensure!(
                            choice.actor == account
                                && privacy.owner
                                    == Some(crate::domain::session_privacy::SessionOwner::User(
                                        account.account_id.clone()
                                    )),
                            "initial publication mode must be registered by the session creator who selected it"
                        );
                        privacy.scheme = choice.scheme;
                        choice.repository_id = Some(identity.agent_id.clone());
                        choice.save(source)?;
                    }
                    // A session first published on another Hub publishes here as a new plain
                    // session of this account only when this device is signed in to that Hub as
                    // the account that created it. Any other session there is someone else's:
                    // registered here as this account's, its copy would be out of its author's
                    // reach to withdraw or erase, and would name the uploader as its creator.
                    if let Some(hub) = privacy.hub.clone().filter(|hub| *hub != identity.hub) {
                        let signed_in = crate::infra::credentials::load_checked(&hub)?
                            .and_then(|credential| credential.account_id);
                        ensure!(
                            privacy.creator.is_some() && privacy.creator == signed_in,
                            "session {session} was first published on {hub} by an account this device is not signed in to there, so it cannot be published here as yours; sign in to {hub} with the account that created it, or push without the branches that hold it"
                        );
                        privacy = super::super::session_privacy::SessionPrivacy::new(Some(
                            account.clone(),
                        ))?;
                        privacy.scheme = SessionScheme::Plain;
                    }
                    if privacy.scheme == SessionScheme::SessionOwnerV2
                        && privacy.creator.as_deref() == Some(account.account_id.as_str())
                    {
                        client.ensure_session_key(&account)?;
                    }
                    client.register_session_privacy(
                        repository,
                        &identity,
                        &target_session,
                        &privacy,
                    )?
                }
            };
            if let Some(delivery) = &delivery {
                response.config.verify_registration(
                    &identity.agent_id,
                    &target_session,
                    delivery.registration(&session)?,
                )?;
            }
            configs.insert(target_session, response);
        }
        // Cross-Hub delivery withholds every fork source, so it never asks this Hub about one.
        let fork_sources = if delivery.is_some() {
            BTreeMap::new()
        } else {
            readable_fork_sources(
                client.base(),
                (repository, &identity.agent_id),
                named_sources,
            )
        };
        let pinned_source = pinned_source(source, &identity.hub);
        plan.verify(source, branches)?;
        Ok(Self {
            identity,
            remote,
            configs,
            delivery,
            sources: Some(sources),
            fork_sources,
            pinned_source,
        })
    }

    /// Withholds `source_ref` and `source_commit` together unless [`Self::fork_sources`] admits
    /// the repository the snapshot names on this destination's Hub. Withholds `fork_origin` whole
    /// unless the repository it points into, its snapshot's or else this checkout's pinned one,
    /// is the destination or is the one that vouched for the named source: its binding carries
    /// that repository's identity, session ids, owner account and committer.
    pub(crate) fn withhold_fork_source(&self, metadata: &mut Meta) {
        let vouched = fork_source_on(metadata, &self.identity.hub)
            .and_then(|repository| self.fork_sources.get(repository))
            .cloned();
        if vouched.is_none() {
            metadata.source_ref = None;
            metadata.source_commit = None;
        }
        if let Some(origin) = &metadata.fork_origin {
            let points_into = match &origin.snapshot {
                Some(snapshot) => Some(&snapshot.binding.repository_id),
                None => self.pinned_source.as_ref(),
            };
            let kept = recorded_on(metadata, &self.identity.hub)
                && points_into.is_some_and(|repository| {
                    *repository == self.identity.agent_id || vouched.as_ref() == Some(repository)
                });
            if !kept {
                metadata.fork_origin = None;
            }
        }
    }

    pub fn verify_configs(&self, client: &crate::hub::Client, repository: &str) -> Result<()> {
        /// Lookups in flight at once. Each session is one independent Hub read, so a publication
        /// of many sessions otherwise waits for them one round trip after another.
        const CONCURRENT: usize = 8;
        let configs = self.configs.iter().collect::<Vec<_>>();
        let chunk = configs.len().div_ceil(CONCURRENT).max(1);
        std::thread::scope(|scope| {
            let workers = configs
                .chunks(chunk)
                .map(|part| {
                    let client = client.clone();
                    scope.spawn(move || -> Result<()> {
                        for (session, expected) in part {
                            let current = client.session_privacy_config(
                                repository,
                                &self.identity,
                                session,
                            )?;
                            ensure!(
                                current.config == expected.config,
                                "registered session authority changed during publication"
                            );
                        }
                        Ok(())
                    })
                })
                .collect::<Vec<_>>();
            workers.into_iter().try_for_each(|worker| {
                worker
                    .join()
                    .map_err(|_| anyhow::anyhow!("session authority check panicked"))?
            })
        })
    }

    fn config(&self, metadata: &Meta) -> Result<&SessionPrivacyResponse> {
        let response = self.configs.get(&metadata.session).context(
            "session has no registered privacy configuration; raw publication is forbidden",
        )?;
        let config = &response.config;
        config.validate()?;
        ensure!(
            config.repository_id == self.identity.agent_id
                && config.session_id == metadata.session
                && config
                    .privacy
                    .hub
                    .as_deref()
                    .is_none_or(|hub| hub == self.identity.hub),
            "registered session belongs to another publication authority"
        );
        if let Some(local) = &metadata.session_privacy {
            let mut effective = local.clone();
            // Registered authority governs unpublished snapshots independently of their birth default.
            effective.scheme = config.privacy.scheme;
            config.verify_registration(&self.identity.agent_id, &metadata.session, &effective)?;
        }
        Ok(response)
    }
}

pub(super) fn require_remote_ancestry(
    source: &Repo,
    plan: &PublicationPlan,
    ledger: &accepted::Ledger,
    remote: &RemoteRefs,
    repository: &str,
) -> Result<()> {
    let source = source.clone().local_objects_only();
    let parents = walk_parents(
        commit_parent_map(&source, plan.commit_objects())?,
        plan.boundary(),
    );
    // A plan with a boundary stops at history the destination holds, so the remote head and the
    // branch's accepted ancestors may lie below the walk; there Git's ancestry decides the
    // fast-forward, and a missing mapping is not evidence of an inconsistent ledger.
    let truncated = !plan.boundary().is_empty();
    for head in plan.heads() {
        let branch = head
            .name()
            .strip_prefix("refs/heads/")
            .context("invalid publication branch")?;
        let Some(remote_head) = remote.refs.get(head.name()) else {
            continue;
        };
        let reachable = ancestors(head.oid(), &parents)?;
        if reachable.contains(remote_head) {
            continue;
        }
        let mut mapped = false;
        let mut contained = false;
        for oid in &reachable {
            if let Some(entry) = ledger.inherited_immutable(branch, oid)? {
                mapped = true;
                contained |= &entry.public == remote_head;
            }
        }
        if contained
            || (truncated
                && source
                    .git_opt(&[
                        "merge-base",
                        "--is-ancestor",
                        remote_head.as_str(),
                        head.oid(),
                    ])
                    .is_some())
        {
            continue;
        }
        // A valid accepted ancestor establishes the publication baseline. Its remote successor
        // need not exist locally; absence of that object does not invalidate the local ledger.
        ensure!(
            mapped || truncated || !ledger.has_accepted_branch(branch),
            "{}",
            accepted::RECOVERY
        );
        let difference = source
            .git_opt(&[
                "rev-list",
                "--left-right",
                "--count",
                &format!("{}...{remote_head}", head.oid()),
            ])
            .and_then(|counts| {
                let mut counts = counts.split_whitespace();
                Some((
                    counts.next()?.parse::<usize>().ok()?,
                    counts.next()?.parse::<usize>().ok()?,
                ))
            });
        // The ledger decides first: a head it accepted has nothing to publish whatever the commit
        // counts say, and one whose public copy is a different commit is never a Git ancestor of
        // the Hub's line, so `pull` cannot fast-forward it. Otherwise the counts decide, or,
        // without the Hub's tip here, whether the last fetched remote branch holds the head.
        let accepted = ledger.inherited_immutable(branch, head.oid())?;
        let position = if accepted.is_some_and(|entry| entry.public != entry.source) {
            Position::BehindProjected
        } else if accepted.is_some()
            || difference.is_some_and(|(ahead, _)| ahead == 0)
            || source
                .git_opt(&[
                    "merge-base",
                    "--is-ancestor",
                    head.oid(),
                    &format!("refs/remotes/origin/{branch}"),
                ])
                .is_some()
        {
            Position::Behind
        } else {
            Position::Diverged
        };
        anyhow::bail!(
            "{}",
            remote_movement(repository, branch, remote_head, position, difference)
        );
    }
    Ok(())
}

/// Where this checkout stands against a Hub branch that its head does not contain.
#[derive(Clone, Copy)]
enum Position {
    /// Nothing here is unpublished, and the Hub's commits can be fast-forwarded onto it.
    Behind,
    /// Nothing here is unpublished, but its published copy is a projection of it.
    BehindProjected,
    /// Both sides have commits the other does not.
    Diverged,
}

/// Why the Hub refused this branch, followed by the commands that resolve it, one `→` line each.
/// A behind checkout only needs the Hub's turns; a diverged one must either publish its own line
/// under a new branch or merge the Hub's line in, and a merge needs the Hub's objects first.
fn remote_movement(
    repository: &str,
    branch: &str,
    remote_head: &str,
    position: Position,
    counts: Option<(usize, usize)>,
) -> String {
    #[cfg(windows)]
    use crate::ui::quote_powershell_argument as quote_arg;
    #[cfg(not(windows))]
    use crate::ui::session::shell_arg as quote_arg;
    let target = quote_arg(&format!("{repository}@{branch}"));
    let tip = remote_head.chars().take(12).collect::<String>();
    match position {
        Position::Behind => {
            let state = match counts {
                Some((0, behind)) => format!("is {behind} commits ahead of this checkout"),
                _ => "has commits this checkout does not have".into(),
            };
            format!(
                "remote branch {branch} {state} (Hub tip {tip}), and this checkout has nothing new on it\n  → bring the Hub's turns here: `agit pull {target}`"
            )
        }
        Position::BehindProjected => format!(
            "remote branch {branch} moved on from this checkout's encrypted publication (Hub tip {tip}), and this checkout has nothing new on it; `agit pull` cannot move a branch whose published copy is encrypted\n  → continue from the Hub's copy: keep this checkout, which holds the originals, and clone {repository} into a separate checkout"
        ),
        Position::Diverged => {
            let state = match counts {
                Some((ahead, behind)) => format!("local +{ahead} / Hub +{behind}"),
                None => "each side has commits the other does not".into(),
            };
            let fork_branch = quote_arg(&format!("{branch}-local"));
            let fork = quote_arg(&format!("{repository}@{branch}-local"));
            let merge_source = quote_arg(&format!("{repository}@{tip}"));
            let peer = quote_arg(repository);
            format!(
                "remote branch {branch} has diverged from this checkout ({state}; Hub tip {tip}); local unpublished commits are kept\n  → keep both: `agit fork {target} -b {fork_branch}`, then `agit push {fork}`\n  → or merge the Hub's line in: `agit fetch {peer}`, then `agit merge {merge_source} --into {target}`, then `agit push {target}`"
            )
        }
    }
}

impl ProjectedHistory {
    pub fn prepare_sessions(
        source: &Repo,
        branches: &[String],
        selected: &SessionPublicationPlan,
        additional: &[super::super::privacy::mandatory::MandatoryPolicy],
    ) -> Result<Self> {
        let redactor = Redactor::try_this_machine()?.with_repository(source.root())?;
        Self::prepare_sessions_with_redactor(source, branches, selected, additional, &redactor)
    }

    pub(super) fn prepare_sessions_with_redactor(
        source: &Repo,
        branches: &[String],
        selected: &SessionPublicationPlan,
        additional: &[super::super::privacy::mandatory::MandatoryPolicy],
        redactor: &Redactor,
    ) -> Result<Self> {
        let preparing = crate::ui::publication::stage("Preparing publication snapshots");
        let identity = &selected.identity;
        ensure!(
            RemoteIdentity::new(&identity.hub, &identity.agent_id)? == *identity,
            "invalid session publication destination"
        );
        let source = source.clone().local_objects_only();
        let policy = effective_policy(&source, additional)?;
        let policy_digest = policy.digest()?;
        let parent = source.common_dir()?.join("agit/privacy-publication");
        crate::infra::config::create_state_dir(&parent)?;
        let lock = crate::infra::config::state_file_options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(parent.join("lock"))?;
        fs2::FileExt::try_lock_exclusive(&lock)?;
        let scope = digest_json(
            &json!({"version": SESSION_ENVELOPE_VERSION, "kind":"session-owner", "identity":identity}),
        )?;
        let store = scope.trim_start_matches("sha256:").to_owned();
        let root = parent.join(&store);
        let mut ledger = accepted::Ledger::open(&source, identity)?;
        ledger.reconcile(&selected.remote)?;
        // Read before the freeze: a tag added in between is then in the plan but not in the
        // listing, which costs the next push its local shortcut, never the tag.
        let source_tags = super::super::privacy_receipt::current_tag_listing(&source)?;
        // What the destination already holds, read from the advertisement the receipts are bound to.
        let held = PublicationPlan::held_history(&source, &selected.remote.heads)?;
        let source_plan = freeze_publication(
            &source,
            branches,
            &held,
            &ledger,
            selected.delivery.is_some(),
        )?;
        preparing.detail(format!(
            "0/{} snapshots",
            source_plan.commit_objects().len()
        ));
        initialize_with_format(&root, source_plan.heads()[0].oid().len())?;
        let repo = Repo::at(&root).local_objects_only();
        let mut cache = match state::load(&root.join(".git/session-candidates"), &scope)? {
            Some(bytes) => candidate_cache(&bytes),
            None => Cache {
                version: CANDIDATE_CACHE_VERSION,
                ..Default::default()
            },
        };
        let fresh;
        let sources = match &selected.sources {
            Some(cached) if cached.matches(&source, &source_plan)? => cached,
            _ => {
                fresh = SourceHistory::read(&source, &source_plan)?;
                &fresh
            }
        };
        // Raw parents: a boundary commit's own parents are checked against its accepted mapping,
        // while the walk leaves them out.
        let parents = commit_parent_map(&source, source_plan.commit_objects())?;
        let walk = &sources.history.parents;
        let enveloped = &sources.enveloped;
        let order = parent_first(walk)?;
        let source_sessions = &sources.history;
        let accepted = source_plan
            .commit_objects()
            .iter()
            .filter(|oid| held.contains(*oid))
            .cloned()
            .collect::<BTreeSet<_>>();
        // The ledger outranks the candidate cache: an accepted encrypted session keeps the public
        // identity it was accepted under, so a cache that lost or replaced it cannot mint a second
        // one. A session the boundary continues gets no new identity at all; the boundary's own
        // binding, re-read below, must agree with the ledger.
        let mut continued = BTreeSet::new();
        for oid in source_plan.boundary() {
            for branch in branches {
                if let Some(entry) = ledger.get(branch, oid) {
                    continued.insert(entry.source_session.clone());
                    if entry.plain || accepted::is_identity(entry) {
                        continue;
                    }
                    let key = digest_json(
                        &json!({"repository":identity.agent_id,"session":entry.source_session}),
                    )?;
                    if let Some(previous) = cache.sessions.insert(key, entry.public_session.clone())
                    {
                        ensure!(
                            previous == entry.public_session,
                            "accepted session has ambiguous publication identities"
                        );
                    }
                }
            }
            if let Some(metadata) = source_sessions.stored(oid) {
                continued.insert(metadata.session.clone());
            }
        }
        let mut public_boundary = BTreeSet::new();
        let mut held_boundary = BTreeSet::new();
        let mut references = BTreeMap::new();
        let mut candidates = BTreeMap::new();
        let mut snapshots = BTreeMap::new();
        let mut preserved = BTreeSet::new();
        let mut reports = Vec::new();
        let mut imports = crate::commands::plumbing::CommitImports::default();
        let mut completed = BTreeSet::new();
        let mut reused = 0;
        let mut prepared = 0;
        let mut record_progress = |oid: &str, retained: bool| {
            if completed.insert(oid.to_owned()) {
                if retained {
                    reused += 1;
                } else {
                    prepared += 1;
                }
                preparing.detail(format!(
                    "{}/{} snapshots, {prepared} prepared, {reused} reused",
                    completed.len(),
                    source_plan.commit_objects().len()
                ));
            }
        };
        #[cfg(test)]
        let mut preparations = 0;
        let alias_repo = if selected.delivery.is_some() {
            &repo
        } else {
            &source
        };
        PathAliasStore::transact(alias_repo, |aliases| {
            for head in source_plan.heads() {
                let branch = head
                    .name()
                    .strip_prefix("refs/heads/")
                    .context("invalid publication branch")?;
                ensure!(
                    redactor.publishes_unchanged(branch)?,
                    "publication branch name requires privacy rewriting; rename it before publishing"
                );
                let reachable = ancestors(head.oid(), walk)?;
                // Below a boundary the remote head may be held history the walk did not reach;
                // whether the publication fast-forwards it is checked against the Hub before
                // anything is sent.
                if let Some(remote_head) = selected.remote.refs.get(head.name())
                    && !reachable.contains(remote_head)
                    && (source_plan.boundary().is_empty() || !held.contains(remote_head))
                {
                    let mut found = false;
                    for oid in &reachable {
                        found |= ledger
                            .inherited_immutable(branch, oid)?
                            .is_some_and(|entry| &entry.public == remote_head);
                    }
                    ensure!(found, "{}", accepted::RECOVERY);
                }
                let mut mapped = BTreeMap::<String, accepted::Mapping>::new();
                let mut plain_rewrites = BTreeSet::new();
                for oid in order.iter().filter(|oid| reachable.contains(*oid)) {
                    // The plan leaves a boundary commit's ancestry out, so its public parents come
                    // from its accepted mapping instead of from this walk.
                    let boundary = source_plan.boundary().binary_search(oid).is_ok();
                    let public_parents = if boundary {
                        None
                    } else {
                        Some(
                            parents[oid]
                                .iter()
                                .map(|oid| {
                                    mapped
                                        .get(oid)
                                        .map(|entry| entry.public.clone())
                                        .context("published parent is unavailable")
                                })
                                .collect::<Result<Vec<_>>>()?,
                        )
                    };
                    if let Some(entry) = ledger.inherited_immutable(branch, oid)? {
                        // A boundary is admitted only as this branch's own accepted mapping or as
                        // an identity mapping; anything else would map an old commit anew with
                        // no ancestry check.
                        ensure!(
                            entry.source_parents == parents[oid]
                                && public_parents
                                    .as_ref()
                                    .is_none_or(|public| &entry.public_parents == public)
                                && (!boundary
                                    || ledger.get(branch, oid) == Some(entry)
                                    || accepted::is_identity(entry)),
                            "{}",
                            accepted::RECOVERY
                        );
                        if boundary {
                            public_boundary.insert(entry.public.clone());
                            if held.contains(oid) && accepted::is_identity(entry) {
                                held_boundary.insert(oid.clone());
                            }
                        }
                        let retained = entry.object_source(&source, &parent, branch)?;
                        let plain = entry.plain;
                        if plain {
                            let metadata = crate::domain::storage::metadata_local(
                                retained.root(),
                                &entry.public,
                            )?;
                            ensure!(
                                metadata
                                    .session_privacy
                                    .as_ref()
                                    .is_none_or(|privacy| privacy.scheme == SessionScheme::Plain),
                                "accepted plain history declares encryption"
                            );
                            plain_rewrites.insert(oid.clone());
                        }
                        if entry.source == entry.public || plain {
                            crate::commands::plumbing::import_commit_graph(
                                &repo,
                                &retained,
                                &entry.public,
                            )?;
                        } else if let Some(reused) = published::retain_accepted(
                            &retained,
                            &repo,
                            &entry.public,
                            &entry.public_parents,
                        )? {
                            aliases.reserve_aliases(&reused.inspection)?;
                            reports.push(reused.report);
                        }
                        if !plain {
                            retain_session_mapping(&repo, &entry.public, identity, &mut cache)?;
                        }
                        preserved.insert(entry.public.clone());
                        mapped.insert(oid.clone(), entry.clone());
                        record_progress(oid, true);
                        continue;
                    }
                    if accepted.contains(oid) {
                        let public_parents = public_parents.unwrap_or_else(|| parents[oid].clone());
                        ensure!(
                            public_parents == parents[oid],
                            "accepted history cannot change parents"
                        );
                        if boundary {
                            public_boundary.insert(oid.clone());
                            held_boundary.insert(oid.clone());
                        }
                        imports.add(oid, &parents[oid]);
                        if enveloped.contains(oid) {
                            // Accepted objects keep their IDs, so the source holds the same bytes.
                            retain_session_mapping(&source, oid, identity, &mut cache)?;
                        }
                        let session = source_sessions
                            .stored(oid)
                            .map(|metadata| metadata.session.clone())
                            .unwrap_or_default();
                        preserved.insert(oid.clone());
                        mapped.insert(
                            oid.clone(),
                            accepted::Mapping {
                                source: oid.clone(),
                                public: oid.clone(),
                                source_parents: parents[oid].clone(),
                                public_parents,
                                store: store.clone(),
                                source_session: session.clone(),
                                public_session: session,
                                policy: policy_digest.clone(),
                                recipient: None,
                                plain: false,
                                delivered: false,
                            },
                        );
                        record_progress(oid, true);
                        continue;
                    }
                    let Some(public_parents) = public_parents else {
                        anyhow::bail!("{}", accepted::RECOVERY);
                    };
                    let original_metadata = source_sessions.metadata(&source, oid).context(
                        "introduced snapshots require declared file or session metadata",
                    )?;
                    let mut metadata = match &selected.delivery {
                        Some(delivery) => delivery.metadata(&original_metadata)?,
                        None => original_metadata.clone(),
                    };
                    selected.withhold_fork_source(&mut metadata);
                    let neutral = match original_metadata
                        .source_ref
                        .as_deref()
                        .filter(|_| metadata.source_ref.is_none())
                    {
                        Some(source_ref) => {
                            let mut commit =
                                read(&source, &["cat-file", "commit", oid], MAX_COMMIT)?;
                            neutral_fork_text(source_ref, &mut metadata, &mut commit)
                                .then_some(commit)
                        }
                        None => None,
                    };
                    let config = if metadata.is_file_line() {
                        None
                    } else {
                        Some(selected.config(&metadata)?)
                    };
                    if config.is_none_or(|response| {
                        response.config.privacy.scheme == SessionScheme::Plain
                    }) {
                        ensure!(
                            parents[oid]
                                .iter()
                                .zip(&public_parents)
                                .all(|(source, public)| source == public
                                    || plain_rewrites.contains(source)),
                            "plain publication cannot expose unaccepted original ancestry from an encrypted session"
                        );
                        ensure!(
                            !enveloped.contains(oid),
                            "encrypted or obsolete ciphertext cannot be admitted as plaintext"
                        );
                        imports.add(oid, &parents[oid]);
                        let public = if let Some(delivery) = &selected.delivery {
                            let delivered = Meta {
                                milestone: metadata.milestone.clone(),
                                ..original_metadata.clone()
                            };
                            let (commit, report) =
                                delivery.project_snapshot(super::delivery::Snapshot {
                                    source: &source,
                                    destination: &repo,
                                    oid,
                                    commit: neutral.as_deref(),
                                    parents: &public_parents,
                                    metadata: &delivered,
                                    policy: &policy,
                                    aliases,
                                    branch,
                                    redactor,
                                })?;
                            reports.push(report);
                            commit
                        } else if let Some(response) = config {
                            let mut published = metadata.clone();
                            let born_elsewhere = source_sessions
                                .births
                                .get(&metadata.session)
                                .and_then(sources::Birth::hub)
                                .is_some_and(|hub| hub != identity.hub);
                            if born_elsewhere
                                || published
                                    .session_privacy
                                    .as_ref()
                                    .is_some_and(|privacy| privacy.scheme != SessionScheme::Plain)
                            {
                                published.session_privacy = Some(response.config.privacy.clone());
                            }
                            // A session registered here from another Hub's birth carries no
                            // account of that Hub: it names nobody here.
                            if born_elsewhere
                                && published
                                    .committer_account
                                    .as_ref()
                                    .is_some_and(|account| account.hub != identity.hub)
                            {
                                published.committer_account = None;
                            }
                            if selected.delivery.is_none()
                                && metadata.session_privacy == published.session_privacy
                                && metadata.committer_account == published.committer_account
                                && metadata.source_ref == original_metadata.source_ref
                                && metadata.source_commit == original_metadata.source_commit
                                && metadata.fork_origin == original_metadata.fork_origin
                                && neutral.is_none()
                                && public_parents == parents[oid]
                            {
                                oid.clone()
                            } else {
                                plain_commit(
                                    &source,
                                    &repo,
                                    (oid, neutral.as_deref()),
                                    &public_parents,
                                    Some(&published),
                                )?
                            }
                        } else if neutral.is_some() || public_parents != parents[oid] {
                            let commit = (oid.as_str(), neutral.as_deref());
                            plain_commit(&source, &repo, commit, &public_parents, None)?
                        } else {
                            oid.clone()
                        };
                        plain_rewrites.insert(oid.clone());
                        let snapshot = match config {
                            None => PublishedSnapshot::FileLine {
                                commit: public.clone(),
                            },
                            Some(response) => PublishedSnapshot::Plain {
                                commit: public.clone(),
                                session_id: metadata.session.clone(),
                                config_revision: response.config.revision,
                            },
                        };
                        snapshots.insert(public.clone(), snapshot);
                        let rewritten = public != *oid;
                        mapped.insert(
                            oid.clone(),
                            accepted::Mapping {
                                source: oid.clone(),
                                public,
                                source_parents: parents[oid].clone(),
                                public_parents,
                                store: store.clone(),
                                source_session: original_metadata.session,
                                public_session: metadata.session,
                                policy: policy_digest.clone(),
                                recipient: None,
                                plain: rewritten,
                                delivered: selected.delivery.is_some(),
                            },
                        );
                        record_progress(oid, false);
                        continue;
                    }
                    let response = config.expect("encrypted session configuration");
                    let config = &response.config;
                    ensure!(
                        !enveloped.contains(oid),
                        "unaccepted ciphertext requires its original publication authority"
                    );
                    let limit = super::super::privacy_publication::MAX_INPUT_BYTES;
                    let (log, view) = super::super::storage::materialize_pair_local(
                        source.root(),
                        oid,
                        limit,
                        limit,
                    )?;
                    let original = match &neutral {
                        Some(commit) => commit.clone(),
                        None => read(&source, &["cat-file", "commit", oid], MAX_COMMIT)?,
                    };
                    let committer = SourceCommitter::from_commit(
                        &original,
                        metadata.committer_account.clone(),
                    )?;
                    let attribution = committer.attribution(&identity.hub)?;
                    let mut projection = PublicationSnapshot::capture(
                        &source, &log, &view, &metadata,
                    )?
                    .project(&policy, aliases, Some(branch), redactor)?;
                    projection.retain_committer(committer)?;
                    let session_key = digest_json(
                        &json!({"repository":identity.agent_id,"session":metadata.session}),
                    )?;
                    let public_session = match cache.sessions.get(&session_key) {
                        Some(public_session) => public_session.clone(),
                        None => {
                            ensure!(
                                !continued.contains(&metadata.session),
                                "{}",
                                accepted::RECOVERY
                            );
                            let public_session = meta::mint_session_id();
                            cache.sessions.insert(session_key, public_session.clone());
                            public_session
                        }
                    };
                    let mut files = projection.prepare_git(&public_session)?;
                    let mut key = json!({"source":oid, "parents":public_parents, "policy":policy_digest,
                        "config":config, "public":projection.public_value(), "private":projection.private_fingerprint()?});
                    if neutral.is_some() {
                        key["commit"] = json!(digest_bytes(&original));
                    }
                    let key = digest_json(&key)?;
                    let envelope_bytes = if let Some(public) = cache.commits.get(&key) {
                        let bytes = read(
                            &repo,
                            &[
                                "cat-file",
                                "blob",
                                &format!("{public}:privacy/envelope.json"),
                            ],
                            super::super::privacy_envelope::MAX_ENVELOPE_BYTES,
                        )?;
                        let envelope = PrivacyEnvelope::parse(&bytes)?;
                        let binding = envelope
                            .session_binding
                            .as_ref()
                            .context("cached session has no owner binding")?;
                        binding.verify_config(config)?;
                        ensure!(
                            binding.source_commit == *oid
                                && binding.published_session_id == public_session
                                && binding.committer == attribution
                                && envelope.public_projection == projection.public_value()
                                && envelope.policy_digest == policy_digest,
                            "cached session candidate differs from its source or registered authority"
                        );
                        bytes
                    } else {
                        let key = response.require_owner_key()?;
                        let binding = SessionSnapshotBinding {
                            repository_id: identity.agent_id.clone(),
                            session_id: metadata.session.clone(),
                            published_session_id: public_session.clone(),
                            owner_key: key.binding(),
                            committer: attribution,
                            source_commit: oid.clone(),
                        };
                        #[cfg(test)]
                        {
                            preparations += 1;
                        }
                        serde_json::to_vec(&projection.seal_session(
                            &key.viewing_recipient()?,
                            config,
                            binding,
                        )?)?
                    };
                    let envelope = PrivacyEnvelope::parse(&envelope_bytes)?;
                    let binding = envelope
                        .session_binding
                        .clone()
                        .context("session candidate has no owner binding")?;
                    let envelope_digest = digest_bytes(&envelope_bytes);
                    files.insert("privacy/envelope.json".into(), envelope_bytes);
                    let tree = write_tree(&repo, files)?;
                    let public = write_object(
                        &repo,
                        "commit",
                        &commit_body(&tree, &public_parents, &original)?,
                    )?;
                    ensure!(
                        cache
                            .commits
                            .get(&key)
                            .is_none_or(|cached| *cached == public),
                        "cached session candidate changed object identity"
                    );
                    cache.commits.insert(key, public.clone());
                    snapshots.insert(
                        public.clone(),
                        PublishedSnapshot::SessionOwnerV2 {
                            snapshot: AcceptedSnapshot {
                                version: SESSION_ENVELOPE_VERSION,
                                binding: binding.clone(),
                                published_commit: public.clone(),
                                envelope_digest,
                            },
                            config_revision: config.revision,
                        },
                    );
                    mapped.insert(
                        oid.clone(),
                        accepted::Mapping {
                            source: oid.clone(),
                            public: public.clone(),
                            source_parents: parents[oid].clone(),
                            public_parents,
                            store: store.clone(),
                            source_session: metadata.session.clone(),
                            public_session,
                            policy: policy_digest.clone(),
                            recipient: Some(binding.owner_key.fingerprint()?),
                            plain: false,
                            delivered: false,
                        },
                    );
                    reports.push(projection.report().clone());
                    record_progress(oid, false);
                }
                let public_head = mapped[head.oid()].public.clone();
                references.insert(head.name().to_owned(), public_head.clone());
                for entry in mapped.values() {
                    let name = format!("refs/tags/agit-{}", entry.public);
                    if !ledger.withheld().contains(&name) {
                        references.insert(name, entry.public.clone());
                    }
                }
                candidates.insert(
                    branch.to_owned(),
                    accepted::Candidate {
                        source_head: head.oid().into(),
                        public_head,
                        mappings: mapped,
                    },
                );
            }
            Ok(())
        })?;
        imports.finish(&repo, &source)?;
        retain_tags(
            &source,
            &source_plan,
            &repo,
            &candidates,
            ledger.withheld(),
            &mut references,
        )?;
        let carried = carry_tags(
            &source,
            &source_plan,
            &repo,
            &parent,
            &ledger,
            branches,
            &selected.remote,
            &held,
            &mut references,
        )?;
        // A previously accepted alias wins when the same public object is reached on another branch.
        snapshots.retain(|commit, _| !preserved.contains(commit));
        let manifest = SessionPublicationManifest {
            version: SESSION_ENVELOPE_VERSION,
            repository_id: identity.agent_id.clone(),
            snapshots: snapshots.into_values().collect(),
            preserved_commits: preserved.into_iter().collect(),
        };
        manifest.validate()?;
        state::save(
            &root.join(".git/session-candidates"),
            &scope,
            &zeroize::Zeroizing::new(serde_json::to_vec(&cache)?),
        )?;
        let mut refs = references
            .iter()
            .map(|(name, oid)| format!("update {name} {oid}\n"))
            .collect::<String>();
        // Cached tags must not reintroduce names removed from the frozen source selection.
        for name in repo
            .git(&["for-each-ref", "--format=%(refname)", "refs/tags/"])?
            .lines()
        {
            if !references.contains_key(name) {
                refs.push_str(&format!("delete {name}\n"));
            }
        }
        input(
            &repo,
            &["-c", "core.hooksPath=", "update-ref", "--stdin"],
            refs.as_bytes(),
        )?;
        let plan = PublicationPlan::freeze_selected_after_carrying(
            &repo,
            branches,
            &public_boundary,
            &carried,
        )?;
        source_plan.verify(&source, branches)?;
        preparing.finish();
        super::schedule_compaction(&root);
        Ok(Self {
            repo,
            source: source_plan,
            branches: branches.to_vec(),
            plan,
            policy_digest,
            additional_policies: additional.to_vec(),
            reports,
            accepted_ledger: ledger,
            candidates,
            held_boundary,
            source_tags,
            session_manifest: manifest,
            #[cfg(test)]
            preparations,
            _lock: lock,
        })
    }
}

/// The repository a snapshot's fork source names, when the snapshot was recorded on `hub`.
///
/// The locator itself names no Hub, so the snapshot's own account or session Hub stands for it;
/// a snapshot with neither, or a locator that is not `<owner>/<repo>@...`, names nothing.
fn fork_source_on<'a>(metadata: &'a Meta, hub: &str) -> Option<&'a str> {
    if !recorded_on(metadata, hub) {
        return None;
    }
    let (repository, _) = metadata.source_ref.as_deref()?.split_once('@')?;
    let (owner, name) = repository.split_once('/')?;
    (!owner.is_empty() && !name.is_empty() && !name.contains('/')).then_some(repository)
}

/// Whether the snapshot's own account or session Hub is `hub`.
fn recorded_on(metadata: &Meta, hub: &str) -> bool {
    metadata
        .committer_account
        .as_ref()
        .map(|account| account.hub.as_str())
        .or_else(|| {
            metadata
                .session_privacy
                .as_ref()
                .and_then(|privacy| privacy.hub.as_deref())
        })
        .and_then(|recorded| crate::domain::session_privacy::canonical_hub(recorded).ok())
        .is_some_and(|recorded| recorded == hub)
}

/// Puts the fork wording agit generates into its neutral form where it names `source_ref`, the
/// withheld source of the snapshot `metadata` and `commit` describe: the milestone
/// `fork from <reference>` becomes `fork`, the message `agit: fork <name> from <reference>` becomes
/// `agit: fork <name>`. Text matches only in that exact form with a `<reference>` that resolves
/// into the repository and ref `source_ref` names, so a milestone or message the person wrote is
/// never changed. Returns whether either changed; `commit` holds the published commit bytes.
fn neutral_fork_text(source_ref: &str, metadata: &mut Meta, commit: &mut Vec<u8>) -> bool {
    let mut changed = false;
    if metadata
        .milestone
        .as_deref()
        .and_then(|milestone| milestone.strip_prefix("fork from "))
        .is_some_and(|reference| names_fork_source(reference, source_ref))
    {
        metadata.milestone = Some("fork".into());
        changed = true;
    }
    let Some(boundary) = commit.windows(2).position(|bytes| bytes == b"\n\n") else {
        return changed;
    };
    let neutral = std::str::from_utf8(&commit[boundary + 2..])
        .ok()
        .and_then(|message| message.strip_suffix('\n'))
        .and_then(|message| message.strip_prefix("agit: fork "))
        .and_then(|rest| rest.split_once(" from "))
        .filter(|(name, reference)| {
            !name.is_empty()
                && !name.contains(char::is_whitespace)
                && names_fork_source(reference, source_ref)
        })
        .map(|(name, _)| format!("agit: fork {name}\n"));
    if let Some(neutral) = neutral {
        commit.truncate(boundary + 2);
        commit.extend_from_slice(neutral.as_bytes());
        changed = true;
    }
    changed
}

/// Whether `reference`, as `agit fork` or `agit run` was given it, can name the fork point
/// `source_ref` records: trimmed as the CLI trims it, it parses as one reference, any repository
/// it writes is the one `source_ref` names, and any ref name it writes is that ref. A fork point
/// recorded as a bare commit keeps no ref name to compare, so any ref name matches it.
fn names_fork_source(reference: &str, source_ref: &str) -> bool {
    use crate::domain::refs::{Base, RepoSel};
    let Some((repository, point)) = source_ref.split_once('@') else {
        return false;
    };
    let Some((owner, name)) = repository.split_once('/') else {
        return false;
    };
    let point = point.split_once('#').map_or(point, |(point, _)| point);
    let reference = reference.trim();
    if reference.is_empty() || reference.contains(char::is_whitespace) {
        return false;
    }
    let Ok(spec) = crate::domain::refs::parse(reference) else {
        return false;
    };
    let repository = match &spec.repo {
        RepoSel::Context => true,
        RepoSel::Slug(o, n) => o == owner && n == name,
        RepoSel::Local(n) => n == name,
    };
    let base = match &spec.base {
        Base::At | Base::Default => true,
        Base::Name(base) | Base::SessionBranch(base) => {
            base == point
                || (point.len() >= 40 && point.bytes().all(|byte| byte.is_ascii_hexdigit()))
        }
    };
    repository && base
}

/// The repository identity `source` is pinned to, when that pin is on `hub`. An unreadable or
/// absent pin names nothing.
fn pinned_source(source: &Repo, hub: &str) -> Option<String> {
    crate::hub::identity::read(source)
        .ok()
        .flatten()
        .filter(|pin| pin.hub == hub)
        .map(|pin| pin.agent_id)
}

/// The named repositories a fork source may keep naming in `destination`, `(owner/repo, id)`,
/// each with the identity of the repository that vouches for it.
///
/// Each lookup is anonymous, made now and answered by this Hub alone: a reader set seen through
/// the person's account, remembered from an earlier publication, or served by another Hub after a
/// redirect does not say who can read the source here today. Any answer other than this exact
/// repository being public with its clone URL on this Hub, an error, redirect or timeout
/// included, leaves the repository out.
fn readable_fork_sources(
    hub: &str,
    (destination, destination_id): (&str, &str),
    named: BTreeSet<String>,
) -> BTreeMap<String, String> {
    let anonymous =
        crate::hub::Client::for_hub_with_timeout(hub, std::time::Duration::from_secs(5));
    let authority = crate::infra::hub_authority::HubAuthority::parse(hub).ok();
    named
        .into_iter()
        .filter_map(|repository| {
            if repository == destination {
                return Some((repository, destination_id.to_owned()));
            }
            let (owner, name) = repository.split_once('/')?;
            let remote = anonymous.get_agent_unredirected(owner, name).ok()?;
            let public = remote.is_public()
                && remote.owner == owner
                && remote.name == name
                && remote.clone_url.starts_with(&format!("{hub}/"))
                && authority
                    .as_ref()
                    .is_some_and(|hub| hub.matches(&remote.clone_url));
            public.then_some((repository, remote.agent_id))
        })
        .collect()
}

/// Only publication metadata and mapped parents change; native evidence and file modes stay exact.
fn plain_commit(
    source: &Repo,
    destination: &Repo,
    (oid, commit): (&str, Option<&[u8]>),
    parents: &[String],
    metadata: Option<&Meta>,
) -> Result<String> {
    fn replace(
        source: &Repo,
        destination: &Repo,
        tree: &str,
        parts: &[&str],
        blob: &str,
    ) -> Result<String> {
        let entries = source.git_bytes_result(&["ls-tree", "-z", tree])?;
        let mut output = Vec::new();
        let mut found = false;
        for entry in entries
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
        {
            let separator = entry
                .iter()
                .position(|byte| *byte == b'\t')
                .context("invalid plain tree entry")?;
            if &entry[separator + 1..] == parts[0].as_bytes() {
                found = true;
                let header = std::str::from_utf8(&entry[..separator])?;
                let fields: Vec<_> = header.split(' ').collect();
                ensure!(fields.len() == 3, "invalid plain tree entry");
                let replacement = if parts.len() == 1 {
                    ensure!(
                        fields[0] == "100644" && fields[1] == "blob",
                        "session metadata must be a regular file"
                    );
                    blob.to_owned()
                } else {
                    ensure!(
                        fields[0] == "040000" && fields[1] == "tree",
                        "session metadata parent must be a directory"
                    );
                    replace(source, destination, fields[2], &parts[1..], blob)?
                };
                output.extend_from_slice(
                    format!("{} {} {}\t", fields[0], fields[1], replacement).as_bytes(),
                );
                output.extend_from_slice(&entry[separator + 1..]);
            } else {
                output.extend_from_slice(entry);
            }
            output.push(0);
        }
        ensure!(found, "plain publication metadata path is missing");
        // Source objects are imported in a batch before this publication becomes inspectable.
        input(destination, &["mktree", "-z", "--missing"], &output)
    }
    let original = match commit {
        Some(commit) => commit.to_vec(),
        None => read(source, &["cat-file", "commit", oid], MAX_COMMIT)?,
    };
    let tree = source.git(&["rev-parse", &format!("{oid}^{{tree}}")])?;
    let tree = if let Some(metadata) = metadata {
        let mut value: serde_json::Value = serde_json::from_slice(&read(
            source,
            &["cat-file", "blob", &format!("{oid}:{}", meta::FILE)],
            1024 * 1024,
        )?)?;
        value["session_privacy"] = serde_json::to_value(&metadata.session_privacy)?;
        value["session"] = json!(metadata.session);
        if metadata.milestone.is_some() {
            value["milestone"] = json!(metadata.milestone);
        }
        let fields = value
            .as_object_mut()
            .context("session metadata must be an object")?;
        for (key, kept) in [
            ("source_ref", json!(metadata.source_ref)),
            ("source_commit", json!(metadata.source_commit)),
            ("fork_origin", serde_json::to_value(&metadata.fork_origin)?),
            (
                "committer_account",
                serde_json::to_value(&metadata.committer_account)?,
            ),
        ] {
            match kept {
                serde_json::Value::Null => fields.remove(key),
                kept => fields.insert(key.into(), kept),
            };
        }
        let blob = write_object(destination, "blob", &serde_json::to_vec(&value)?)?;
        replace(
            source,
            destination,
            &tree,
            &meta::FILE.split('/').collect::<Vec<_>>(),
            &blob,
        )?
    } else {
        tree
    };
    write_object(
        destination,
        "commit",
        &commit_body(&tree, parents, &original)?,
    )
}

const EMPTY_TREE_SHA1: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const EMPTY_TREE_SHA256: &str = "6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321";

/// A blob a published commit introduces at a path.
struct Introduced {
    commit: String,
    path: String,
    blob: String,
    /// The blob the first parent holds at the same path, when that is a file.
    previous: Option<String>,
}

impl Introduced {
    fn preview(&self) -> serde_json::Value {
        json!({"commit":self.commit,"path":self.path,"blob":self.blob})
    }
}

/// Every introduced entry is emitted in order; one whose blob was read earlier is a repeat.
struct PreviewOrder<'a> {
    introduced: &'a [Introduced],
    next: usize,
}

impl PreviewOrder<'_> {
    fn repeat_until(
        &mut self,
        end: usize,
        emit: &mut impl FnMut(serde_json::Value) -> Result<()>,
    ) -> Result<()> {
        while self.next < end {
            let mut value = self.introduced[self.next].preview();
            value["repeated"] = json!(true);
            emit(value)?;
            self.next += 1;
        }
        Ok(())
    }
}

/// Text is shown as text; anything else by its length.
fn describe(value: &mut serde_json::Value, bytes: &[u8]) {
    match std::str::from_utf8(bytes) {
        Ok(text) if !text.contains('\0') => value["text"] = json!(text),
        _ => value["binary_bytes"] = json!(bytes.len()),
    }
}

/// The entries each commit introduces. A path counts only when its blob differs from the same
/// path in every parent, so content a parent already carries is not reviewed again; a root
/// commit introduces its whole tree. Commits keep the caller's order.
fn introduced_blobs(repo: &Repo, commits: &[&str]) -> Result<Vec<Introduced>> {
    let Some(first) = commits.first() else {
        return Ok(Vec::new());
    };
    let empty = if first.len() == 64 {
        EMPTY_TREE_SHA256
    } else {
        EMPTY_TREE_SHA1
    };
    let mut trees = BTreeMap::new();
    let mut parents = BTreeMap::new();
    read_commit_trees(
        repo,
        commits.iter().map(|oid| (*oid).to_owned()).collect(),
        &mut trees,
        &mut parents,
    )?;
    let outside = parents
        .values()
        .flatten()
        .filter(|oid| !trees.contains_key(*oid))
        .cloned()
        .collect::<BTreeSet<_>>();
    read_commit_trees(
        repo,
        outside.into_iter().collect(),
        &mut trees,
        &mut BTreeMap::new(),
    )?;
    type Changes = BTreeMap<String, (String, Option<String>)>;
    let mut pairs = BTreeMap::<(String, String), Changes>::new();
    for commit in commits {
        let tree = &trees[*commit];
        if parents[*commit].is_empty() {
            pairs.insert((empty.to_owned(), tree.clone()), BTreeMap::new());
        }
        for parent in &parents[*commit] {
            pairs.insert((trees[parent].clone(), tree.clone()), BTreeMap::new());
        }
    }
    let mut input = tempfile::tempfile()?;
    for (old, new) in pairs.keys() {
        writeln!(input, "{old} {new}")?;
    }
    input.rewind()?;
    // Tree-pair headers end with LF even under -z and precede every pair, including an empty
    // diff; each raw record is followed by its NUL-terminated path.
    let mut current = None;
    let mut pending = None;
    repo.git_stream_split_stdin_file(
        &["diff-tree", "--stdin", "-r", "-z", "--no-renames", "--raw"],
        input,
        0,
        |token| {
            if let Some(blob) = pending.take() {
                let path = std::str::from_utf8(token)
                    .context("plain preview requires UTF-8 paths")?
                    .to_owned();
                if let Some(blob) = blob {
                    pairs
                        .get_mut(current.as_ref().context("diff record has no tree pair")?)
                        .context("diff record names an unrequested tree pair")?
                        .insert(path, blob);
                }
                return Ok(());
            }
            let text = std::str::from_utf8(token).context("invalid plain preview diff")?;
            let mut lines = text.split('\n').peekable();
            while let Some(line) = lines.next() {
                if lines.peek().is_some() {
                    let (old, new) = line.split_once(' ').context("invalid diff tree pair")?;
                    current = Some((old.to_owned(), new.to_owned()));
                } else if let Some(record) = line.strip_prefix(':') {
                    let fields = record.split(' ').collect::<Vec<_>>();
                    ensure!(fields.len() == 5, "invalid plain preview diff record");
                    let file = |mode: &str| matches!(mode, "100644" | "100755" | "120000");
                    let blob = file(fields[1]).then(|| {
                        (
                            fields[3].to_owned(),
                            file(fields[0]).then(|| fields[2].to_owned()),
                        )
                    });
                    pending = Some(blob);
                } else {
                    ensure!(line.is_empty(), "invalid plain preview diff output");
                }
            }
            Ok(())
        },
    )?;
    ensure!(pending.is_none(), "plain preview diff is truncated");
    let mut introduced = Vec::new();
    for commit in commits {
        let tree = &trees[*commit];
        let compared = if parents[*commit].is_empty() {
            vec![&pairs[&(empty.to_owned(), tree.clone())]]
        } else {
            parents[*commit]
                .iter()
                .map(|parent| &pairs[&(trees[parent].clone(), tree.clone())])
                .collect()
        };
        for (path, (blob, previous)) in compared[0] {
            if compared[1..]
                .iter()
                .all(|other| other.get(path).is_some_and(|(other, _)| other == blob))
            {
                introduced.push(Introduced {
                    commit: (*commit).to_owned(),
                    path: path.clone(),
                    blob: blob.clone(),
                    previous: previous.clone(),
                });
            }
        }
    }
    Ok(introduced)
}

fn read_commit_trees(
    repo: &Repo,
    commits: Vec<String>,
    trees: &mut BTreeMap<String, String>,
    parents: &mut BTreeMap<String, Vec<String>>,
) -> Result<()> {
    repo.git_cat_file_batch_with_policy(
        commits,
        MAX_COMMIT,
        crate::domain::repo::ReadPolicy::LocalOnly,
        |oid, kind, body| {
            let crate::domain::repo::ObjectBody::Read(bytes) = body else {
                anyhow::bail!("plain preview commit exceeds its size limit");
            };
            ensure!(kind == "commit", "plain preview commit is unavailable");
            let tree = std::str::from_utf8(bytes)?
                .lines()
                .next()
                .and_then(|line| line.strip_prefix("tree "))
                .context("plain preview commit has no tree")?;
            trees.insert(oid.to_owned(), tree.to_owned());
            parents.insert(oid.to_owned(), commit_parents(bytes)?);
            Ok(())
        },
    )
}

fn retain_tags(
    source: &Repo,
    plan: &PublicationPlan,
    destination: &Repo,
    candidates: &BTreeMap<String, accepted::Candidate>,
    withheld: &BTreeSet<String>,
    references: &mut BTreeMap<String, String>,
) -> Result<()> {
    for tag in plan
        .tags()
        .iter()
        .filter(|tag| !withheld.contains(tag.name()))
    {
        // Canonical version tags follow generated public IDs; their names cannot expose source IDs.
        if tag.name() == format!("refs/tags/agit-{}", tag.oid())
            && plan.commit_objects().iter().any(|oid| oid == tag.oid())
        {
            continue;
        }
        let commit = source.git(&[
            "rev-parse",
            "--verify",
            &format!("{}^{{commit}}", tag.oid()),
        ])?;
        let mappings = candidates
            .values()
            .filter_map(|candidate| candidate.mappings.get(&commit))
            .collect::<Vec<_>>();
        ensure!(
            !mappings.is_empty() && mappings.iter().all(|entry| entry.public == commit),
            "custom tag {} points to rewritten publication history; remove it from the source or recreate it on the accepted public commit",
            tag.name()
        );
        if let Some(previous) = references.insert(tag.name().to_owned(), tag.oid().to_owned()) {
            ensure!(
                previous == tag.oid(),
                "custom tag conflicts with a generated version tag"
            );
        }
    }
    for oid in plan.tag_objects() {
        let bytes = read(source, &["cat-file", "tag", oid], MAX_COMMIT)?;
        ensure!(
            write_object(destination, "tag", &bytes)? == *oid,
            "publication tag changed object identity"
        );
    }
    Ok(())
}

/// The session candidate cache in `bytes`, at this build's version. A cache from another version,
/// or with a shape this build cannot read, was written by another agit on this machine: its
/// session aliases are kept wherever they can still be read, since history accepted before session
/// envelopes records its published session nowhere else, and its candidates are dropped and
/// prepared again. Refusing such a cache would fail every session on a machine where two agit
/// versions publish.
fn candidate_cache(bytes: &[u8]) -> Cache {
    #[derive(Deserialize)]
    struct Aliases {
        #[serde(default)]
        sessions: BTreeMap<String, String>,
    }
    let (version, sessions) = match serde_json::from_slice::<Cache>(bytes) {
        Ok(cache) if cache.version == CANDIDATE_CACHE_VERSION => return cache,
        Ok(cache) => (Some(cache.version), cache.sessions),
        Err(_) => (
            None,
            serde_json::from_slice::<Aliases>(bytes)
                .map(|aliases| aliases.sessions)
                .unwrap_or_default(),
        ),
    };
    // Version 2 is this build's own predecessor, whose candidates cannot identify attributed
    // commits; it is upgraded in place without a note.
    if version != Some(2) {
        crate::ui::warning(
            "the session publication cache was written by another agit version; rebuilding it",
        );
    }
    Cache {
        version: CANDIDATE_CACHE_VERSION,
        sessions,
        commits: BTreeMap::new(),
    }
}

/// Tags the branches reach below the boundary that the destination does not have as they are
/// here, added to `references`, with the names returned for the public plan to carry. A plan takes
/// only its own commits' tags, so without this a tag added to an earlier, published commit, and a
/// version tag of a published turn whose tags phase failed after its heads landed, never reach the
/// Hub; a plan that captured their history instead would meet the commit bound once the published
/// history outgrows it. Each points to a commit in `held`, which the destination holds under that
/// id, so no history is sent for it. A turn whose commit the destination no longer holds (erased,
/// withdrawn, its branch gone) owes no tag, and neither does a rewritten turn, whose public commit
/// `held` cannot contain, nor a tag the Hub refused as withdrawn (see `Ledger::withheld`).
#[allow(clippy::too_many_arguments)]
fn carry_tags(
    source: &Repo,
    plan: &PublicationPlan,
    destination: &Repo,
    parent: &std::path::Path,
    ledger: &accepted::Ledger,
    branches: &[String],
    remote: &RemoteRefs,
    held: &BTreeSet<String>,
    references: &mut BTreeMap<String, String>,
) -> Result<BTreeSet<String>> {
    let lacks = |name: &str, oid: &str| {
        !ledger.withheld().contains(name)
            && remote.refs.get(name).is_none_or(|current| current != oid)
    };
    let mut carried = BTreeSet::new();
    for branch in branches {
        for entry in ledger.accepted(branch) {
            let name = format!("refs/tags/agit-{}", entry.public);
            if plan.commit_objects().binary_search(&entry.source).is_ok()
                || !held.contains(&entry.public)
                || references.contains_key(&name)
                || !lacks(&name, &entry.public)
            {
                continue;
            }
            let retained = entry.object_source(source, parent, branch)?;
            crate::commands::plumbing::import_commit_graph(destination, &retained, &entry.public)?;
            references.insert(name.clone(), entry.public.clone());
            carried.insert(name);
        }
    }
    for (tag, commit, objects) in plan.tags_below_boundary(source, |name, oid| !lacks(name, oid))? {
        ensure!(
            held.contains(&commit),
            "custom tag {} points to rewritten publication history; remove it from the source or recreate it on the accepted public commit",
            tag.name()
        );
        crate::commands::plumbing::import_commit_graph(destination, source, &commit)?;
        for oid in &objects {
            let bytes = read(source, &["cat-file", "tag", oid], MAX_COMMIT)?;
            ensure!(
                write_object(destination, "tag", &bytes)? == *oid,
                "publication tag changed object identity"
            );
        }
        if let Some(previous) = references.insert(tag.name().to_owned(), tag.oid().to_owned()) {
            ensure!(
                previous == tag.oid(),
                "custom tag conflicts with a generated version tag"
            );
        }
        carried.insert(tag.name().to_owned());
    }
    Ok(carried)
}

fn retain_session_mapping(
    repo: &Repo,
    commit: &str,
    identity: &RemoteIdentity,
    cache: &mut Cache,
) -> Result<()> {
    // Callers reach here only once the commit's objects are local, so a failed read is damage, not
    // an absent envelope: skipping it would drop the session's published identity.
    let Some(bytes) = crate::domain::privacy_recovery::committed_envelope(repo, commit)? else {
        return Ok(());
    };
    #[derive(serde::Deserialize)]
    struct Version {
        format_version: Option<u64>,
    }
    let version: Version =
        serde_json::from_slice(&bytes).context("committed privacy envelope is malformed")?;
    if version.format_version != Some(SESSION_ENVELOPE_VERSION.into()) {
        return Ok(());
    }
    let envelope = PrivacyEnvelope::parse(&bytes)?;
    let binding = envelope
        .session_binding
        .context("accepted session has no owner binding")?;
    if binding.repository_id == identity.agent_id {
        let key =
            digest_json(&json!({"repository":identity.agent_id,"session":binding.session_id}))?;
        if let Some(previous) = cache
            .sessions
            .insert(key, binding.published_session_id.clone())
        {
            ensure!(
                previous == binding.published_session_id,
                "accepted session has ambiguous publication identities"
            );
        }
    }
    Ok(())
}

impl ProjectedHistory {
    pub fn session_receipts(
        &self,
        repository: &str,
        identity: &RemoteIdentity,
        url: &str,
    ) -> Result<Vec<super::super::privacy_receipt::PublicationReceipt>> {
        use super::super::privacy_receipt::{PublicationMode, PublicationReceipt};
        let manifest = &self.session_manifest;
        let scope = digest_json(
            &json!({"version": SESSION_ENVELOPE_VERSION, "kind":"session-owner", "identity":identity}),
        )?;
        ensure!(
            manifest.repository_id == identity.agent_id
                && self.repo.root().file_name().and_then(|name| name.to_str())
                    == scope.strip_prefix("sha256:"),
            "session receipt destination differs from its prepared authority"
        );
        let mut receipts = Vec::new();
        for head in self.source.heads() {
            let branch = head
                .name()
                .strip_prefix("refs/heads/")
                .context("invalid session publication branch")?;
            let candidate = self
                .candidates
                .get(branch)
                .context("session publication has no branch candidate")?;
            let metadata =
                super::super::storage::metadata_local(self.repo.root(), &candidate.public_head)?;
            if metadata.is_file_line() {
                continue;
            }
            let mut receipt = PublicationReceipt {
                version: 2,
                mode: PublicationMode::Ordinary,
                session_snapshot: None,
                repository: repository.into(),
                branch: branch.into(),
                source: head.oid().into(),
                published: candidate.public_head.clone(),
                projected_session_id: Some(metadata.session),
                destination: identity.clone(),
                url: url.into(),
                policy_digest: None,
                recipient: None,
            };
            if receipt.source != receipt.published {
                receipt.version = 3;
                receipt.mode = PublicationMode::SessionPlain;
            }
            let object = format!("{}:privacy/envelope.json", candidate.public_head);
            if self.repo.git(&["cat-file", "-e", &object]).is_ok() {
                let bytes = read(
                    &self.repo,
                    &["cat-file", "blob", &object],
                    super::super::privacy_envelope::MAX_ENVELOPE_BYTES,
                )?;
                if manifest.preserved_commits.contains(&candidate.public_head)
                    && published::is_legacy_envelope(&bytes)?
                {
                    continue;
                }
                let envelope = PrivacyEnvelope::parse(&bytes)?;
                let Some(binding) = envelope.session_binding else {
                    continue;
                };
                receipt.version = 3;
                receipt.mode = PublicationMode::SessionOwnerV2;
                receipt.policy_digest = Some(envelope.policy_digest);
                receipt.recipient = Some(binding.owner_key.fingerprint()?);
                receipt.session_snapshot = Some(AcceptedSnapshot {
                    version: SESSION_ENVELOPE_VERSION,
                    binding,
                    published_commit: candidate.public_head.clone(),
                    envelope_digest: digest_bytes(&bytes),
                });
            }
            receipt.validate()?;
            receipts.push(receipt);
        }
        Ok(receipts)
    }

    /// Plain file review covers the blobs each snapshot introduces relative to its parents and
    /// reads captured payloads, so a live LFS cache cannot change the reviewed bytes.
    ///
    /// Entries are emitted in publication order while at most two object bodies are held, so the
    /// review of a long history costs disk, not memory. A blob is shown once and later entries
    /// that carry it are marked `repeated`. A file that grows by appending to the blob its first
    /// parent holds at the same path shows only the appended bytes, under `appended_to`. An
    /// object over the per-object limit is still verified but shown by its size alone.
    pub(crate) fn plain_file_preview(
        &self,
        captured: &crate::hub::git::CapturedPublication,
        mut emit: impl FnMut(serde_json::Value) -> Result<()>,
    ) -> Result<()> {
        use crate::domain::repo::{ObjectBody, ReadPolicy};
        use std::io::Read;
        const OBJECT_LIMIT: usize = super::super::privacy_publication::MAX_INPUT_BYTES;
        enum Step {
            Previous,
            Entry(usize),
        }
        ensure!(
            captured.plan() == &self.plan,
            "preview capture differs from its publication plan"
        );
        let commits = self
            .session_manifest
            .snapshots
            .iter()
            .filter(|entry| !matches!(entry, PublishedSnapshot::SessionOwnerV2 { .. }))
            .map(PublishedSnapshot::commit)
            .collect::<Vec<_>>();
        let introduced = introduced_blobs(&self.repo, &commits)?;
        let mut seen = BTreeSet::new();
        let mut reads = Vec::new();
        let mut steps = Vec::new();
        for (index, entry) in introduced.iter().enumerate() {
            if !seen.insert(entry.blob.as_str()) {
                continue;
            }
            if let Some(previous) = &entry.previous {
                reads.push(previous.clone());
                steps.push(Step::Previous);
            }
            reads.push(entry.blob.clone());
            steps.push(Step::Entry(index));
        }
        let mut order = PreviewOrder {
            introduced: &introduced,
            next: 0,
        };
        let mut steps = steps.into_iter();
        let mut previous: Option<Vec<u8>> = None;
        self.repo.git_cat_file_batch_with_policy(
            reads,
            OBJECT_LIMIT,
            ReadPolicy::LocalOnly,
            |_, kind, body| {
                ensure!(kind == "blob", "plain preview object is unavailable");
                let index = match steps.next().context("plain preview read is unrequested")? {
                    Step::Previous => {
                        previous = match body {
                            ObjectBody::Read(bytes) => Some(bytes.to_vec()),
                            ObjectBody::TooLarge(_) => None,
                        };
                        return Ok(());
                    }
                    Step::Entry(index) => index,
                };
                order.repeat_until(index, &mut emit)?;
                let entry = &introduced[index];
                let base = previous.take();
                let mut value = entry.preview();
                let bytes = match body {
                    ObjectBody::TooLarge(size) => {
                        value["omitted_bytes"] = json!(size);
                        order.next = index + 1;
                        return emit(value);
                    }
                    ObjectBody::Read(bytes) => bytes,
                };
                if let Some(pointer) = super::super::lfs::Pointer::parse(bytes)? {
                    value["lfs"] = serde_json::to_value(&pointer)?;
                    if captured.preserves_object(&entry.blob)
                        && !captured.pointers().contains(&pointer)
                    {
                        value["preserved"] = json!(true);
                    } else if pointer.size > OBJECT_LIMIT as u64 {
                        pointer.verify(captured.open_payload(&pointer)?)?;
                        value["omitted_bytes"] = json!(pointer.size);
                    } else {
                        let mut payload = Vec::new();
                        captured.open_payload(&pointer)?.read_to_end(&mut payload)?;
                        pointer.verify(payload.as_slice())?;
                        describe(&mut value, &payload);
                    }
                } else if let Some(base) =
                    base.filter(|base| !base.is_empty() && bytes.starts_with(base))
                {
                    value["appended_to"] = json!(entry.previous);
                    describe(&mut value, &bytes[base.len()..]);
                } else {
                    describe(&mut value, bytes);
                }
                order.next = index + 1;
                emit(value)
            },
        )?;
        ensure!(steps.next().is_none(), "plain preview read is truncated");
        order.repeat_until(introduced.len(), &mut emit)
    }

    /// Plain snapshots report their metadata and sequence lengths; their content is reviewed as the
    /// files each one introduces, and every distinct event is validated against its ID once.
    pub(crate) fn session_preview(&self) -> Result<Vec<serde_json::Value>> {
        let manifest = &self.session_manifest;
        let mut validator = super::super::storage::SnapshotValidator::default();
        let mut snapshots = Vec::new();
        for entry in &manifest.snapshots {
            let commit = entry.commit();
            let public = match entry {
                PublishedSnapshot::SessionOwnerV2 { snapshot, .. } => {
                    let bytes = read(
                        &self.repo,
                        &[
                            "cat-file",
                            "blob",
                            &format!("{commit}:privacy/envelope.json"),
                        ],
                        super::super::privacy_envelope::MAX_ENVELOPE_BYTES,
                    )?;
                    ensure!(
                        digest_bytes(&bytes) == snapshot.envelope_digest,
                        "session preview envelope differs from its manifest"
                    );
                    let envelope = PrivacyEnvelope::parse(&bytes)?;
                    ensure!(
                        envelope.session_binding.as_ref() == Some(&snapshot.binding),
                        "session preview authority differs from its manifest"
                    );
                    envelope.public_projection
                }
                PublishedSnapshot::Plain { .. } => {
                    let metadata = super::super::storage::metadata_local(self.repo.root(), commit)?;
                    ensure!(
                        metadata.is_session_line(),
                        "privacy publication requires a session snapshot"
                    );
                    let (log, view) = match metadata.layout {
                        meta::LayoutVersion::V0 => {
                            let (_, log, view) = super::super::privacy_publication::read_snapshot(
                                &self.repo, commit,
                            )?;
                            (log.lines().count(), view.lines().count())
                        }
                        meta::LayoutVersion::V1 => {
                            validator.validate_local(self.repo.root(), commit)?
                        }
                    };
                    json!({"metadata":metadata,"session":{"log_events":log,"view_events":view}})
                }
                PublishedSnapshot::FileLine { .. } => {
                    json!({"metadata": super::super::storage::metadata_local(self.repo.root(), commit)?})
                }
            };
            let git = commit_preview(&read(
                &self.repo,
                &["cat-file", "commit", commit],
                MAX_COMMIT,
            )?)?;
            snapshots.push(json!({"commit":commit,"publication":entry,"public":public,"git":git}));
        }
        for commit in &manifest.preserved_commits {
            snapshots.push(json!({"commit":commit,"preserved":true}));
        }
        Ok(snapshots)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(root: &std::path::Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    /// A fork names its source only in that source's own repository, or while an anonymous lookup
    /// on the same Hub finds the source public; `source_ref` and `source_commit` go together, and
    /// `fork_origin` goes whole, on every snapshot the publication introduces. Where they are
    /// withheld, the fork message and milestone agit wrote take the neutral form, so neither the
    /// source repository nor its branch is left in any published commit or meta, while the text the
    /// person wrote is published as written. A projection that strips only the branch tip still
    /// publishes the fork point beneath it, one that takes an unanswered lookup for a public
    /// answer names the private source in another repository, and one that leaves display text
    /// alone names it in the fork's own message.
    #[test]
    fn a_fork_source_is_published_only_to_its_own_repository_or_while_it_is_public() {
        use crate::domain::session_privacy::{
            AccountIdentity, ForkContext, ForkOrigin, SessionPrivacyConfig,
        };
        use crate::hub::privacy::sessions::SessionPrivacyResponse;
        // Nothing listens on this Hub, so every anonymous lookup on it fails.
        let hub = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", listener.local_addr().unwrap())
        };
        let account = AccountIdentity::new(&hub, "alice").unwrap();
        let temp = tempfile::tempdir().unwrap();
        let source = Repo::init(&temp.path().join("source")).unwrap();
        for (key, value) in [
            ("user.name", "Alice"),
            ("user.email", "alice@example.invalid"),
            ("commit.gpgsign", "false"),
        ] {
            source.git(&["config", key, value]).unwrap();
        }
        source.git(&["checkout", "-b", "fork"]).unwrap();
        let home = "00000000-0000-0000-0000-000000000001";
        crate::hub::identity::pin(&source, &RemoteIdentity::new(&hub, home).unwrap()).unwrap();
        let snapshot = |metadata: &Meta, message: &str| {
            meta::write(source.root(), metadata).unwrap();
            crate::domain::storage::write_snapshot(source.root(), "", "").unwrap();
            source.add_all().unwrap();
            source.commit(message).unwrap();
            source.git(&["rev-parse", "HEAD"]).unwrap()
        };
        let mut base = Meta::new(
            format!("agit-{}", "a".repeat(40)),
            "codex".into(),
            "".into(),
        );
        base.committer_account = Some(account.clone());
        base.session_privacy = Some(SessionPrivacy::new(Some(account)).unwrap());
        let point = snapshot(&base, "snapshot");
        let mut forked = base.clone();
        forked.session = format!("agit-{}", "b".repeat(40));
        forked.source_ref = Some("alice/private@hidden#1".into());
        forked.source_commit = Some(point.clone());
        let origin = ForkOrigin {
            commit: point.clone(),
            context: ForkContext::Original,
            snapshot: None,
        };
        forked.fork_origin = Some(origin.clone());
        forked.milestone = Some("fork from alice/private@hidden".into());
        snapshot(&forked, "agit: fork fork from alice/private@hidden");
        forked.turn = Some(2);
        forked.milestone = Some("fork from the review plan".into());
        snapshot(&forked, "Turn 2: fork from here");
        type Published = (
            Option<String>,
            Option<String>,
            Option<ForkOrigin>,
            Option<String>,
            String,
        );
        let publish = |destination: &str, agent: &str| -> Vec<Published> {
            let selected = SessionPublicationPlan {
                identity: RemoteIdentity::new(&hub, agent).unwrap(),
                remote: Default::default(),
                configs: [&base, &forked]
                    .map(|metadata| {
                        let config = SessionPrivacyConfig {
                            repository_id: agent.into(),
                            session_id: metadata.session.clone(),
                            privacy: metadata.session_privacy.clone().unwrap(),
                            revision: 1,
                            created_at: "2026-01-01T00:00:00Z".into(),
                        };
                        let response = SessionPrivacyResponse {
                            config,
                            owner_key: None,
                        };
                        (metadata.session.clone(), response)
                    })
                    .into(),
                delivery: None,
                sources: None,
                fork_sources: readable_fork_sources(
                    &hub,
                    (destination, agent),
                    BTreeSet::from(["alice/private".to_owned()]),
                ),
                pinned_source: pinned_source(&source, &hub),
            };
            let published = ProjectedHistory::prepare_sessions_with_redactor(
                &source,
                &["fork".into()],
                &selected,
                &[],
                &Redactor::new(Default::default()),
            )
            .unwrap();
            let commits = published
                .repo
                .git(&["rev-list", "refs/heads/fork"])
                .unwrap();
            commits
                .lines()
                .map(|commit| {
                    let object = published.repo.git(&["cat-file", "commit", commit]).unwrap();
                    let meta = published
                        .repo
                        .git(&["cat-file", "blob", &format!("{commit}:{}", meta::FILE)])
                        .unwrap();
                    let metadata: Meta = serde_json::from_str(&meta).unwrap();
                    let message = object.split_once("\n\n").unwrap().1.to_owned();
                    let names_source = [object, meta]
                        .iter()
                        .any(|text| text.contains("alice/private") || text.contains("hidden"));
                    assert_eq!(names_source, metadata.source_ref.is_some(), "{commit}");
                    (
                        metadata.source_ref,
                        metadata.source_commit,
                        metadata.fork_origin,
                        metadata.milestone,
                        message,
                    )
                })
                .collect()
        };
        let written = (
            Some("fork from the review plan".to_owned()),
            "Turn 2: fork from here".to_owned(),
        );
        let base = (None, None, None, None, "snapshot".to_owned());
        assert_eq!(
            publish("bob/public", "00000000-0000-0000-0000-000000000002"),
            [
                (None, None, None, written.0.clone(), written.1.clone()),
                (
                    None,
                    None,
                    None,
                    Some("fork".into()),
                    "agit: fork fork".into()
                ),
                base.clone(),
            ]
        );
        let kept = |(milestone, message): (Option<String>, String)| {
            let source = Some("alice/private@hidden#1".to_owned());
            let (commit, origin) = (Some(point.clone()), Some(origin.clone()));
            (source, commit, origin, milestone, message)
        };
        assert_eq!(
            publish("alice/private", home),
            [
                kept(written),
                kept((
                    Some("fork from alice/private@hidden".into()),
                    "agit: fork fork from alice/private@hidden".into(),
                )),
                base,
            ]
        );
    }

    /// Only the source's own Hub can vouch that a fork source is public. A probe that followed a
    /// redirect would take another Hub's public repository of the same name for the source and
    /// keep a private source in a public publication.
    #[test]
    fn a_redirected_lookup_never_vouches_for_a_fork_source() {
        use std::io::{Read, Write};
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let respond = |listener: std::net::TcpListener, reply: String| {
            let done = done.clone();
            std::thread::spawn(move || {
                listener.set_nonblocking(true).unwrap();
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                while !done.load(std::sync::atomic::Ordering::SeqCst)
                    && std::time::Instant::now() < deadline
                {
                    match listener.accept() {
                        Ok((mut socket, _)) => {
                            socket.set_nonblocking(false).unwrap();
                            let mut request = [0u8; 4096];
                            let _ = socket.read(&mut request).unwrap();
                            socket.write_all(reply.as_bytes()).unwrap();
                            return true;
                        }
                        Err(_) => std::thread::sleep(std::time::Duration::from_millis(5)),
                    }
                }
                false
            })
        };
        let elsewhere = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let other_hub = format!("http://{}", elsewhere.local_addr().unwrap());
        let public = json!({
            "agent_id": "00000000-0000-0000-0000-000000000009",
            "owner": "alice",
            "name": "private",
            "clone_url": format!("{other_hub}/alice/private.git"),
            "visibility": "public",
        })
        .to_string();
        let served = respond(
            elsewhere,
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{public}",
                public.len()
            ),
        );
        let source = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let hub = format!("http://{}", source.local_addr().unwrap());
        let redirected = respond(
            source,
            format!(
                "HTTP/1.1 302 Found\r\nLocation: {other_hub}/api/agents/alice/private\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            ),
        );
        let kept = readable_fork_sources(
            &hub,
            ("bob/public", "00000000-0000-0000-0000-000000000002"),
            BTreeSet::from(["alice/private".to_owned()]),
        );
        done.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(redirected.join().unwrap(), "the source Hub was not asked");
        assert!(kept.is_empty(), "{kept:?}");
        assert!(!served.join().unwrap(), "the redirect was followed");
    }

    /// A version tag is owed only for a turn the destination still holds. An accepted turn below
    /// the boundary whose tag the advertisement lacks is carried while its commit is in the held
    /// history, and skipped once the destination no longer holds it (an erased or withdrawn turn,
    /// a branch gone), since the tag would then name history the Hub no longer has. A turn the plan
    /// captures takes its tag from the plan itself. A tag the Hub refused as withdrawn stays out,
    /// across ledger reopens, until it is released. An implementation that carried every accepted
    /// turn's missing tag pushes the erased turn's tag here, and one that forgot the refusal would
    /// plan the tag on every push and meet the same refusal.
    #[test]
    fn only_turns_the_destination_still_holds_owe_their_version_tag() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("source");
        std::fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q"]);
        let commit = |message: &str| {
            git(&root, &["commit", "-q", "--allow-empty", "-m", message]);
            git(&root, &["rev-parse", "HEAD"])
        };
        let first = commit("first");
        let second = commit("second");
        commit("third");
        let source = Repo::at(&root).local_objects_only();
        let identity = RemoteIdentity::new(
            "https://hub.invalid",
            "00000000-0000-0000-0000-000000000001",
        )
        .unwrap();
        let session = format!("agit-{}", "a".repeat(40));
        let mapping = |oid: &str, parent: Option<&str>| accepted::Mapping {
            source: oid.into(),
            public: oid.into(),
            source_parents: parent.map(str::to_owned).into_iter().collect(),
            public_parents: parent.map(str::to_owned).into_iter().collect(),
            store: "c".repeat(64),
            source_session: session.clone(),
            public_session: session.clone(),
            policy: crate::domain::privacy_envelope::digest_bytes(b"policy"),
            recipient: None,
            plain: false,
            delivered: false,
        };
        let mut ledger = accepted::Ledger::open(&source, &identity).unwrap();
        ledger
            .prepare(
                &BTreeMap::from([(
                    "main".to_owned(),
                    accepted::Candidate {
                        source_head: second.clone(),
                        public_head: second.clone(),
                        mappings: BTreeMap::from([
                            (first.clone(), mapping(&first, None)),
                            (second.clone(), mapping(&second, Some(&first))),
                        ]),
                    },
                )]),
                &BTreeSet::new(),
            )
            .unwrap();
        let remote = RemoteRefs {
            refs: BTreeMap::from([("refs/heads/main".to_owned(), second.clone())]),
            ..Default::default()
        };
        ledger.reconcile(&remote).unwrap();
        let plan = PublicationPlan::freeze_selected_after(
            &source,
            &["main".into()],
            &BTreeSet::from([second.clone()]),
        )
        .unwrap();
        let stores = std::cell::Cell::new(0);
        let carry = |ledger: &accepted::Ledger, held: BTreeSet<String>| {
            stores.set(stores.get() + 1);
            let store = dir.path().join(format!("store-{}", stores.get()));
            std::fs::create_dir(&store).unwrap();
            git(&store, &["init", "-q"]);
            let destination = Repo::at(&store);
            let mut references = BTreeMap::new();
            let carried = carry_tags(
                &source,
                &plan,
                &destination,
                &dir.path().join("publication"),
                ledger,
                &["main".into()],
                &remote,
                &held,
                &mut references,
            )
            .unwrap();
            (carried, references, destination)
        };

        let version = format!("refs/tags/agit-{first}");
        let both = BTreeSet::from([first.clone(), second.clone()]);
        let (carried, references, destination) = carry(&ledger, both.clone());
        assert_eq!(carried, BTreeSet::from([version.clone()]));
        assert_eq!(
            references,
            BTreeMap::from([(version.clone(), first.clone())])
        );
        destination
            .git(&["cat-file", "-e", &format!("{first}^{{commit}}")])
            .unwrap();
        let (carried, references, _) = carry(&ledger, BTreeSet::from([second.clone()]));
        assert!(carried.is_empty());
        assert!(references.is_empty());

        super::super::withhold_refs(&source, &identity, std::slice::from_ref(&version)).unwrap();
        assert_eq!(
            super::super::withheld_refs(&source, &identity).unwrap(),
            BTreeSet::from([version.clone()])
        );
        let ledger = accepted::Ledger::open(&source, &identity).unwrap();
        let (carried, references, _) = carry(&ledger, both.clone());
        assert!(carried.is_empty());
        assert!(references.is_empty());
        super::super::release_refs(&source, &identity, std::slice::from_ref(&version)).unwrap();
        let ledger = accepted::Ledger::open(&source, &identity).unwrap();
        assert_eq!(carry(&ledger, both).0, BTreeSet::from([version]));
    }

    /// A root introduces its tree and a merge introduces only paths whose blob differs from every
    /// parent; a union over parents would review again what one parent already carries.
    #[test]
    fn merge_introduces_only_paths_that_differ_from_every_parent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let commit = |path: &str, text: &str, message: &str| {
            std::fs::write(root.join(path), text).unwrap();
            git(root, &["add", path]);
            git(root, &["commit", "-q", "-m", message]);
            git(root, &["rev-parse", "HEAD"])
        };
        git(root, &["init", "-q"]);
        std::fs::write(root.join("a.txt"), "base").unwrap();
        git(root, &["add", "a.txt"]);
        let base = commit("b.txt", "base", "base");
        let left = commit("a.txt", "left", "left");
        git(root, &["checkout", "-q", "-b", "side", &base]);
        let side = commit("b.txt", "side", "side");
        git(root, &["checkout", "-q", &left]);
        git(root, &["merge", "-q", "--no-ff", "--no-commit", "side"]);
        let merge = commit("c.txt", "merge", "merge");

        let repo = Repo::at(root).local_objects_only();
        let introduced = introduced_blobs(&repo, &[&base, &left, &side, &merge]).unwrap();
        let named = introduced
            .iter()
            .map(|entry| (entry.commit.as_str(), entry.path.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            named,
            [
                (base.as_str(), "a.txt"),
                (base.as_str(), "b.txt"),
                (left.as_str(), "a.txt"),
                (side.as_str(), "b.txt"),
                (merge.as_str(), "c.txt"),
            ]
        );
    }
}
