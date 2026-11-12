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

#[path = "session_projection.rs"]
mod projection;
#[path = "session_sources.rs"]
mod sources;

const CANDIDATE_CACHE_VERSION: u32 = 3;

pub struct SessionPublicationPlan {
    pub identity: RemoteIdentity,
    pub remote: RemoteRefs,
    pub configs: BTreeMap<String, SessionPrivacyResponse>,
    pub(crate) delivery: Option<super::delivery::Delivery>,
    pub(crate) sources: Option<SourceHistory>,
    /// Branch to commit: where each branch is cut when the publication is one part of several
    /// (see [`SessionPublicationPlan::plan_parts`]); empty for a whole publication.
    pub(crate) pins: BTreeMap<String, String>,
    /// Withheld refs this publication plans again ([`accepted::Ledger::ask_again`]).
    pub(crate) asking_again: BTreeSet<String>,
    /// Repositories, `owner/repo`, that introduced snapshots may keep naming as their fork source,
    /// each with the repository identity that vouches for it: the destination itself, and any that
    /// an anonymous lookup on this Hub found public while this publication was resolved. Every
    /// other `source_ref` is withheld with its commit.
    pub(crate) fork_sources: BTreeMap<String, String>,
    /// The repository identity this checkout is pinned to on the destination's Hub, read when the
    /// publication was resolved: a `fork_origin` without a snapshot points into it.
    pub(crate) pinned_source: Option<String>,
    /// Sessions registered encrypted that this publication continues as plain, in order.
    pub switched: Vec<SwitchedSession>,
    /// The destination accepts this client's declaration that it ran the carried-record proof;
    /// without it the Hub refuses any session that continues an encrypted one.
    pub carried_records_proof: bool,
    /// Switched sessions whose plain registration waits until what they publish has been proven
    /// and inspected: each target session and the privacy it registers.
    pub(crate) to_register: Vec<(String, SessionPrivacy)>,
}

/// A session that was published encrypted and continues as plain from this publication on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwitchedSession {
    pub session: String,
    /// Where it is published, `owner/repo@branch`, when a branch of this publication carries it.
    pub target: Option<String>,
    /// Whether the signed-in account owns it; otherwise its owner is told on the Hub.
    pub owned: bool,
}

impl SwitchedSession {
    /// What the person pushing is told, once, at the push that switches the session.
    pub fn notice(&self, dry_run: bool) -> String {
        let session = &self.session;
        let (subject, verb) = match (self.owned, dry_run) {
            (true, false) => (session.clone(), "now publishes"),
            (true, true) => (session.clone(), "will publish, once pushed,"),
            (false, false) => (
                format!("{session}, another account's session,"),
                "now publishes",
            ),
            (false, true) => (
                format!("{session}, another account's session,"),
                "will publish, once pushed,",
            ),
        };
        let mut notice = format!(
            "{subject} {verb} like your other sessions: new turns upload only the redacted view people with access already see, and the originals stay on this computer. Files it changed before this, such as memory or skills, are not uploaded."
        );
        if self.owned {
            let show = match &self.target {
                Some(target) => format!("agit privacy access show {target}"),
                None => "agit privacy access show".to_owned(),
            };
            notice.push_str(&format!(
                " People you gave access to the originals keep it for earlier turns only (`{show}` lists them)."
            ));
        } else {
            notice.push_str(" Its owner is told on the Hub.");
        }
        notice
    }
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
/// its birth, and below a boundary the plan would name a later commit as the birth. With `pins`,
/// each branch is cut at its pin, as one part of a publication in several; the whole-history
/// cases cannot be cut, so they refuse instead.
fn freeze_publication(
    source: &Repo,
    branches: &[String],
    held: &BTreeSet<String>,
    ledger: &accepted::Ledger,
    delivering: bool,
    pins: &BTreeMap<String, String>,
) -> Result<PublicationPlan> {
    let whole = |reason: &str| {
        ensure!(
            pins.is_empty(),
            "this publication cannot be sent in parts: {reason}"
        );
        PublicationPlan::freeze_selected(source, branches)
    };
    if delivering {
        return whole("a cross-Hub delivery carries the whole history");
    }
    let mut stops = ledger.accepted_sources(branches);
    stops.extend(held.iter().cloned());
    let plan = if pins.is_empty() {
        PublicationPlan::freeze_selected_after(source, branches, &stops)?
    } else {
        PublicationPlan::freeze_selected_after_at(source, pins, &stops, &BTreeSet::new())?
    };
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
                return whole("it continues history another branch accepted");
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
        pins: BTreeMap<String, String>,
    ) -> Result<Self> {
        ensure!(
            crate::domain::session_privacy::canonical_hub(client.base())? == identity.hub,
            "session publication belongs to another Hub"
        );
        let held = PublicationPlan::held_history(source, &remote.heads)?;
        let mut ledger = accepted::Ledger::open(source, &identity)?;
        ledger.reconcile(&remote)?;
        let delivering = explicit_hub || super::delivery::Delivery::active(source, &identity)?;
        let plan = freeze_publication(source, branches, &held, &ledger, delivering, &pins)?;
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
        // Asked once: the Hub takes the declaration that the carried-record proof ran.
        let carried_records_proof = delivery.is_none() && client.accepts_carried_records_proof();
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
        let mut switched = Vec::new();
        let mut to_register = Vec::new();
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
                // Encrypted publication is retired: a session registered encrypted continues as
                // plain at its next revision, whoever publishes it. Its earlier snapshots stay as
                // they are, and its turns already out keep their published projection.
                Some(response)
                    if delivery.is_none()
                        && response.config.privacy.scheme == SessionScheme::SessionOwnerV2 =>
                {
                    ensure!(
                        carried_records_proof,
                        "this Hub doesn't yet accept a conversation that continues one published encrypted, so nothing was uploaded; it will once the Hub is updated"
                    );
                    let mut privacy = response.config.privacy.clone();
                    privacy.scheme = SessionScheme::Plain;
                    let account = client.session_account().ok();
                    // Planned only: the Hub learns of the switch, and declares it, only once the
                    // proof and the outgoing inspection passed (see `register_switched`).
                    if register {
                        to_register.push((target_session.clone(), privacy.clone()));
                    }
                    let continued = SessionPrivacyResponse {
                        config: crate::domain::session_privacy::SessionPrivacyConfig {
                            privacy,
                            revision: response.config.revision + 1,
                            ..response.config.clone()
                        },
                        owner_key: None,
                    };
                    // A Hub that keeps the session encrypted has not retired encrypted
                    // publication yet; the session continues as before.
                    if continued.config.privacy.scheme == SessionScheme::Plain {
                        let carries = |head: &crate::domain::repo::publication::FrozenRef| {
                            ancestors(head.oid(), &history.parents).is_ok_and(|reachable| {
                                reachable.iter().any(|oid| {
                                    history
                                        .stored(oid)
                                        .is_some_and(|metadata| metadata.session == session)
                                })
                            })
                        };
                        let target =
                            plan.heads()
                                .iter()
                                .find(|head| carries(head))
                                .and_then(|head| {
                                    head.name()
                                        .strip_prefix("refs/heads/")
                                        .map(|branch| format!("{repository}@{branch}"))
                                });
                        switched.push(SwitchedSession {
                            session: session.clone(),
                            target,
                            owned: account.is_some_and(|account| {
                                continued.config.privacy.owner
                                    == Some(crate::domain::session_privacy::SessionOwner::User(
                                        account.account_id,
                                    ))
                            }),
                        });
                    }
                    continued
                }
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
                    // Encrypted publication is retired: whatever the birth, an inherited link or
                    // an earlier choice recorded, a new session registers as plain. A cross-Hub
                    // delivery keeps the registration it carries.
                    if delivery.is_none() {
                        privacy.scheme = SessionScheme::Plain;
                    }
                    client.register_session_privacy(
                        repository,
                        &identity,
                        &target_session,
                        &privacy,
                        carried_records_proof,
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
            pins,
            asking_again: BTreeSet::new(),
            fork_sources,
            pinned_source,
            switched,
            to_register,
            carried_records_proof,
        })
    }

    /// Where to cut `branch` so that it is published up to `head` in parts that each fit one
    /// publication to a destination admitting `max_new_commits` per request, oldest first, with
    /// the number of commits it adds: one cut, `head`, when the whole of it fits. The stops are the ones its freeze uses (this
    /// branch's accepted sources and the history the destination holds), so planner and freeze
    /// agree on what each part adds. A cut names its own session, since an unclaimed commit takes
    /// its session from a descendant in the same publication. A destination this checkout
    /// delivers to across Hubs is published whole.
    pub fn plan_parts(
        source: &Repo,
        branch: &str,
        head: &str,
        identity: &RemoteIdentity,
        remote: &RemoteRefs,
        max_new_commits: usize,
    ) -> Result<(Vec<String>, usize)> {
        let source = source.clone().local_objects_only();
        if super::delivery::Delivery::active(&source, identity)? {
            return Ok((vec![head.to_owned()], 0));
        }
        let mut ledger = accepted::Ledger::open(&source, identity)?;
        ledger.reconcile(remote)?;
        let mut stops = ledger.accepted_sources(&[branch.to_owned()]);
        stops.extend(PublicationPlan::held_history(&source, &remote.heads)?);
        let history = crate::domain::repo::publication::NewHistory::read(&source, head, &stops)?;
        let cuts = history.plan_parts(
            crate::domain::repo::publication::PartBudget::new(max_new_commits),
            |oid| {
                crate::domain::storage::metadata_local(source.root(), oid).is_ok_and(|metadata| {
                    !metadata.is_session_line() || !metadata.session.is_empty()
                })
            },
        )?;
        Ok((cuts, history.len()))
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

    /// Registers the switched sessions as planned, declaring the carried-record proof that has
    /// run by now, once; whether any was. Each answer must be the plain configuration planned for
    /// it, at its planned revision; anything else refuses, with nothing sent.
    pub fn register_switched(
        &mut self,
        client: &crate::hub::Client,
        repository: &str,
    ) -> Result<bool> {
        let pending = std::mem::take(&mut self.to_register);
        let registered_any = !pending.is_empty();
        for (session, privacy) in &pending {
            let planned = self
                .configs
                .get(session)
                .context("a switched session has no planned configuration")?;
            let registered = client.register_session_privacy(
                repository,
                &self.identity,
                session,
                privacy,
                self.carried_records_proof,
            )?;
            ensure!(
                registered.config.privacy.scheme == SessionScheme::Plain
                    && registered.config.revision == planned.config.revision,
                "the Hub didn't continue {session} as plain as planned, so nothing was sent"
            );
        }
        Ok(registered_any)
    }

    /// Drops the planned switches of a publication that sends nothing: each such session keeps
    /// the configuration the Hub holds for it, and nobody is told of a switch.
    pub fn forgo_switches(&mut self, client: &crate::hub::Client, repository: &str) -> Result<()> {
        for (session, _) in std::mem::take(&mut self.to_register) {
            let held = client
                .registered_session_privacy(repository, &self.identity, &session)?
                .with_context(|| format!("{session} is no longer registered on the Hub"))?;
            self.configs.insert(session, held);
        }
        self.switched.clear();
        Ok(())
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
            accepted::Inconsistent
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
        let position = if let Some(entry) = accepted.filter(|entry| entry.public != entry.source) {
            Position::BehindProjected {
                encrypted: entry.recipient.is_some(),
            }
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
        } else if meta::is_file_line_at(&source, head.oid()) {
            // Only objects already here decide "same files": the Hub's tip is compared when a
            // fetch brought it, and nothing is claimed about it otherwise.
            let tree = |commit: &str| {
                source.git_opt(&["rev-parse", "--verify", &format!("{commit}^{{tree}}")])
            };
            Position::DivergedFiles {
                same: tree(remote_head).is_some_and(|theirs| tree(head.oid()) == Some(theirs)),
            }
        } else {
            let keep_both = keep_both_branch(&source, remote, branch);
            return Err(Diverged {
                message: remote_movement(
                    repository,
                    branch,
                    remote_head,
                    Position::Diverged,
                    difference,
                    &keep_both,
                ),
                repository: repository.to_owned(),
                branch: branch.to_owned(),
                keep_both,
            }
            .into());
        };
        anyhow::bail!(
            "{}",
            remote_movement(repository, branch, remote_head, position, difference, "")
        );
    }
    Ok(())
}

/// A push refused because this checkout's session branch and the Hub's copy each moved on without
/// the other. It carries the branch a keep-both push publishes this checkout's line as.
#[derive(Debug)]
pub(crate) struct Diverged {
    pub(crate) repository: String,
    pub(crate) branch: String,
    /// The first of `<branch>-local`, `<branch>-local-2`, … that names no branch here or on the
    /// Hub.
    pub(crate) keep_both: String,
    message: String,
}

impl std::fmt::Display for Diverged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Diverged {}

/// The first of `<branch>-local`, `<branch>-local-2`, … that names no branch in `source` or among
/// the Hub's refs, so keeping both never lands on a line either side already has.
fn keep_both_branch(source: &Repo, remote: &RemoteRefs, branch: &str) -> String {
    (1..)
        .map(|n| match n {
            1 => format!("{branch}-local"),
            n => format!("{branch}-local-{n}"),
        })
        .find(|name| {
            let reference = format!("refs/heads/{name}");
            !remote.refs.contains_key(&reference) && !source.has_ref(&reference)
        })
        .expect("some suffix names no branch")
}

/// How a push says that the Hub's copy of a session branch and this checkout's have each moved on
/// without the other, read by automatic upload as `push::OVERSIZED` is.
pub(crate) const DIVERGED: &str = "has diverged from this checkout";

/// How a push says that the Hub's copy of a session branch has moved on and nothing here is new,
/// read by automatic upload as [`DIVERGED`] is. Unread, the upload is told as one the next turn
/// gets past, and that turn is refused as diverged.
pub(crate) const NOTHING_NEW: &str = "this checkout has nothing new on it";

/// Where this checkout stands against a Hub branch that its head does not contain.
#[derive(Clone, Copy)]
enum Position {
    /// Nothing here is unpublished, and the Hub's commits can be fast-forwarded onto it.
    Behind,
    /// Nothing here is unpublished, but its published copy is a projection of it: sealed to a
    /// recipient when `encrypted`, and plain otherwise.
    BehindProjected { encrypted: bool },
    /// Both sides have commits the other does not.
    Diverged,
    /// Both sides of a file line have commits the other does not, and `pull` merges them; `same`
    /// when the Hub's tip is here and holds exactly this head's files.
    DivergedFiles { same: bool },
}

/// Why the Hub refused this branch, followed by the commands that resolve it, one `→` line each.
/// A behind checkout only needs the Hub's turns; a diverged one must either publish its own line
/// under a new branch or merge the Hub's line in, and a merge needs the Hub's objects first. A
/// diverged file line is not a conversation to keep beside another: `pull` fetches and merges its
/// files, and stops only where both sides changed the same lines.
fn remote_movement(
    repository: &str,
    branch: &str,
    remote_head: &str,
    position: Position,
    counts: Option<(usize, usize)>,
    keep_both: &str,
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
                "remote branch {branch} {state} (Hub tip {tip}), and {NOTHING_NEW}\n  → bring the Hub's turns here: `agit pull {target}`"
            )
        }
        // The Hub's newer turn sits on a published copy, which is a different commit from this
        // checkout's own, so neither copy can be fast-forwarded onto the other. A pull brings the
        // turn over as the Hub holds it; where it can't, the way on is a separate checkout of the
        // Hub's copy, with this one kept for its originals.
        Position::BehindProjected { encrypted: false } => format!(
            "remote branch {branch}: the Hub has a newer turn from another computer on top of what this checkout uploaded (Hub tip {tip}), and {NOTHING_NEW}\n  → bring it over: `agit pull {target}`, then `agit push {target}`; if it can't be brought over, keep this checkout, which holds your own turns as they ran, and clone {repository} into a separate checkout"
        ),
        Position::BehindProjected { encrypted: true } => format!(
            "remote branch {branch} moved on from this checkout's encrypted publication (Hub tip {tip}), and {NOTHING_NEW}; `agit pull` cannot move a branch whose published copy is encrypted\n  → continue from the Hub's copy: keep this checkout, which holds the originals, and clone {repository} into a separate checkout"
        ),
        Position::DivergedFiles { same } => {
            let state = match counts {
                Some((ahead, behind)) => format!("local +{ahead} / Hub +{behind}"),
                None => "each side has commits the other does not".into(),
            };
            if same {
                format!(
                    "remote branch {branch} has diverged from this checkout ({state}; Hub tip {tip}), but both sides hold the same files; local unpublished commits are kept\n  → bring the Hub's line in: `agit pull {target}`, then `agit push {target}`"
                )
            } else {
                format!(
                    "remote branch {branch} has diverged from this checkout ({state}; Hub tip {tip}); local unpublished commits are kept\n  → bring the Hub's changes in: `agit pull {target}` (it merges the files and stops only where both sides changed the same lines), then `agit push {target}`"
                )
            }
        }
        Position::Diverged => {
            let state = match counts {
                Some((ahead, behind)) => format!("local +{ahead} / Hub +{behind}"),
                None => "each side has commits the other does not".into(),
            };
            let merge_source = quote_arg(&format!("{repository}@{tip}"));
            let peer = quote_arg(repository);
            // Keeping both publishes a branch, so an agent reads it as the person's to choose.
            let keep = if crate::ui::prompt::can_ask() {
                "keep both"
            } else {
                "if the user wants to keep both"
            };
            format!(
                "remote branch {branch} {DIVERGED} ({state}; Hub tip {tip}); local unpublished commits are kept\n  → {keep}: `agit push {target} --keep-both` publishes this checkout's line as {keep_both}, beside the Hub's\n  → or merge the Hub's line in: `agit fetch {peer}`, then `agit merge {merge_source} --into {target}`, then `agit push {target}`"
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
        let lock_path = parent.join("lock");
        let lock = crate::infra::config::state_file_options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;
        let lock = crate::infra::local_state::HeldLock::exclusive(
            lock,
            &lock_path,
            super::PUBLICATION_LOCK,
        )?;
        let scope = digest_json(
            &json!({"version": SESSION_ENVELOPE_VERSION, "kind":"session-owner", "identity":identity}),
        )?;
        let store = scope.trim_start_matches("sha256:").to_owned();
        let root = parent.join(&store);
        let mut ledger = accepted::Ledger::open(&source, identity)?;
        ledger.reconcile(&selected.remote)?;
        ledger.ask_again(&selected.asking_again);
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
            &selected.pins,
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
        let mut left_out = LeftOut::default();
        let mut rechecked = Rechecked::default();
        // The source commit holding each plain public commit's LOG, for public commits this run
        // writes.
        let mut written = std::collections::HashMap::new();
        let mut projector = projection::Projector::default();
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
                    ensure!(found, accepted::Inconsistent);
                }
                let mut mapped = BTreeMap::<String, accepted::Mapping>::new();
                let mut plain_rewrites = BTreeSet::new();
                let mut encrypted_below =
                    super::frozen::Finder::new(&source, &parent, &ledger, &held, branch);
                let mut file_line_bases = std::collections::HashMap::new();
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
                            accepted::Inconsistent
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
                        anyhow::bail!(accepted::Inconsistent);
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
                    let neutral = rechecked.message(&source, oid, neutral)?;
                    let config = if metadata.is_file_line() {
                        None
                    } else {
                        Some(selected.config(&metadata)?)
                    };
                    if config.is_none_or(|response| {
                        response.config.privacy.scheme == SessionScheme::Plain
                    }) {
                        // A session snapshot continuing one this device published encrypted, and
                        // the Hub accepted, is rewritten onto its public parents with the turns
                        // that one published kept as projected (see `frozen`), so its source
                        // ancestry never leaves.
                        let continues_projection = config.is_some() && selected.delivery.is_none();
                        for (source, public) in parents[oid].iter().zip(&public_parents) {
                            ensure!(
                                source == public
                                    || plain_rewrites.contains(source)
                                    || (continues_projection
                                        && ledger
                                            .inherited_immutable(branch, source)?
                                            .is_some_and(super::frozen::encrypted)),
                                "plain publication cannot expose unaccepted original ancestry from an encrypted session"
                            );
                        }
                        ensure!(
                            !enveloped.contains(oid),
                            "encrypted or obsolete ciphertext cannot be admitted as plaintext"
                        );
                        imports.add(oid, &parents[oid]);
                        // A session snapshot publishes its conversation records only, and those an
                        // encrypted snapshot below it published as they were projected then.
                        let conversation = match config {
                            Some(_) if selected.delivery.is_none() => {
                                let nearest = encrypted_below
                                    .below_new(oid, parents[oid].first().map(String::as_str))?;
                                let published = nearest
                                    .as_deref()
                                    .map(super::frozen::Nearest::published)
                                    .transpose()?;
                                let encrypted_others =
                                    encrypted_below.others(oid, nearest.as_deref())?;
                                let others = encrypted_others
                                    .iter()
                                    .map(|other| other.published())
                                    .collect::<Result<Vec<_>>>()?;
                                ensure!(
                                    (published.is_none() && others.is_empty())
                                        || selected.carried_records_proof,
                                    "this Hub doesn't yet accept a conversation that continues one published encrypted, so nothing was uploaded; it will once the Hub is updated"
                                );
                                // Files the session changed while it was published encrypted were
                                // never uploaded; they stay as its file line had them.
                                let encrypted: Vec<String> = nearest
                                    .as_deref()
                                    .into_iter()
                                    .chain(encrypted_others.iter().map(|other| &**other))
                                    .filter_map(super::frozen::Nearest::local_source)
                                    .map(str::to_owned)
                                    .collect();
                                let held_back = super::frozen::held_back_files(
                                    &source,
                                    oid,
                                    &encrypted,
                                    &mut file_line_bases,
                                )?;
                                // Lines a public parent already holds go out as they are, so only
                                // a parent shown to be plain counts: an encrypted parent's
                                // projection is what `splice` carries, and it passes today's
                                // protection again, as does every line of a parent that can't
                                // be shown plain.
                                let mut plain_sources = Vec::new();
                                let mut plain_publics = Vec::new();
                                for (source_parent, public) in
                                    parents[oid].iter().zip(&public_parents)
                                {
                                    let encrypted_here = ledger
                                        .inherited_immutable(branch, source_parent)?
                                        .is_some_and(super::frozen::encrypted);
                                    if !encrypted_here
                                        && shown_plain(
                                            &source,
                                            &repo,
                                            written
                                                .get(public)
                                                .map_or(public.as_str(), String::as_str),
                                        )
                                    {
                                        plain_sources.push(source_parent.clone());
                                        plain_publics.push(public.clone());
                                    }
                                }
                                let public_records = published_lines(
                                    &source,
                                    &repo,
                                    &written,
                                    oid,
                                    &plain_sources,
                                    &plain_publics,
                                )?;
                                let carried = Carried {
                                    projected: published,
                                    others: &others,
                                    held_back,
                                };
                                // A snapshot that carries what an encrypted publication below it
                                // published is projected whole, and the projector starts again
                                // after it: its records' public form comes from that projection,
                                // not from the snapshot before it.
                                let conversation = if carried.projected.is_none()
                                    && carried.others.is_empty()
                                    && carried.held_back.is_empty()
                                {
                                    let conversation = projector.project(
                                        &source,
                                        oid,
                                        &parents[oid],
                                        &public_records,
                                        &mut rechecked,
                                    )?;
                                    // Debug builds hold the projector to the whole-LOG projection
                                    // on every snapshot.
                                    #[cfg(debug_assertions)]
                                    ensure!(
                                        conversation
                                            == conversation_commit(
                                                &source,
                                                oid,
                                                &parents[oid],
                                                &public_records,
                                                Carried::default(),
                                                &mut rechecked,
                                            )?,
                                        "the snapshot projector differs from the whole-LOG projection"
                                    );
                                    conversation
                                } else {
                                    projector = projection::Projector::default();
                                    conversation_commit(
                                        &source,
                                        oid,
                                        &parents[oid],
                                        &public_records,
                                        carried,
                                        &mut rechecked,
                                    )?
                                };
                                count_left_out(
                                    &repo,
                                    &public_parents,
                                    conversation.left_out,
                                    &mut left_out,
                                )?;
                                conversation.commit
                            }
                            _ => None,
                        };
                        if let Some(conversation) = &conversation {
                            imports.add(conversation, &parents[oid]);
                        }
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
                                && conversation.is_none()
                            {
                                oid.clone()
                            } else {
                                plain_commit(
                                    &source,
                                    &repo,
                                    (conversation.as_deref().unwrap_or(oid), neutral.as_deref()),
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
                        if selected.delivery.is_none() {
                            written.insert(
                                public.clone(),
                                conversation.clone().unwrap_or_else(|| oid.clone()),
                            );
                        }
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
                                accepted::Inconsistent
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
        let mut left_out_tags = Vec::new();
        // Session tags the destination already holds on their turns' public commits.
        let mut on_hub = BTreeMap::new();
        let moved = retain_tags(
            &source,
            &source_plan,
            &repo,
            &candidates,
            ledger.withheld(),
            &mut references,
            &mut left_out_tags,
        )?;
        let mut carried = carry_tags(
            &source,
            &source_plan,
            &repo,
            &parent,
            &ledger,
            branches,
            &selected.remote,
            &held,
            &mut references,
            &mut on_hub,
            &mut left_out_tags,
        )?;
        carried.extend(moved);
        let handled = source_plan
            .tags()
            .iter()
            .map(|tag| tag.name().to_owned())
            .chain(
                left_out_tags
                    .iter()
                    .map(|tag| format!("refs/tags/{}", tag.name)),
            )
            .collect::<BTreeSet<_>>();
        place_unreached_tags(
            &source,
            &repo,
            &parent,
            &ledger,
            &candidates,
            &selected.remote,
            &handled,
            &mut references,
            &mut carried,
            &mut on_hub,
            &mut left_out_tags,
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
        // The plan keeps a carried tag only when a branch it sends reaches it: a tag on a
        // published turn of a branch outside this push is named here, so nothing claims it went.
        let planned = plan
            .tags()
            .iter()
            .map(|tag| (tag.name().to_owned(), tag.oid().to_owned()))
            .collect::<BTreeMap<_, _>>();
        for name in &carried {
            if !planned.contains_key(name) && !name.starts_with("refs/tags/agit-") {
                left_out_tags.push(LeftOutTag::new(name, TagLeftOut::OtherBranch));
            }
        }
        let mut published = planned;
        published.extend(on_hub);
        let published_tags = published_elsewhere(&source, &published)?;
        source_plan.verify(&source, branches)?;
        preparing.finish();
        super::schedule_compaction(&root);
        Ok(Self {
            source_root: source.root().to_path_buf(),
            repo,
            source: source_plan,
            branches: branches.to_vec(),
            plan,
            policy_digest,
            additional_policies: additional.to_vec(),
            reports,
            left_out: left_out.added.len(),
            accepted_ledger: ledger,
            candidates,
            held_boundary,
            // A publication that leaves a local tag out never vouches for the local tags, so the
            // next push asks again and names the tag again instead of reporting nothing to do.
            source_tags: if left_out_tags.is_empty() {
                source_tags
            } else {
                String::new()
            },
            left_out_tags,
            published_tags,
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

/// The records with parts left out that one publication has met, each counted once.
#[derive(Default)]
struct LeftOut {
    /// The event paths of records this publication adds.
    added: BTreeSet<String>,
    /// The event paths of records a public parent already holds: published, and counted, before.
    published: BTreeSet<String>,
}

/// Sort the records with parts left out that a snapshot publishes, `paths`, into those this
/// publication adds and those a public parent of it already holds. A record met before in this
/// publication is not looked up again, and the rest are looked up in one batch, so a long
/// history of snapshots that each carry the same records costs no Git process per record.
fn count_left_out(
    repo: &Repo,
    public_parents: &[String],
    paths: BTreeSet<String>,
    seen: &mut LeftOut,
) -> Result<()> {
    let unknown: Vec<String> = paths
        .into_iter()
        .filter(|path| !seen.added.contains(path) && !seen.published.contains(path))
        .collect();
    let mut held = BTreeSet::new();
    let probes: Vec<String> = unknown
        .iter()
        .flat_map(|path| {
            public_parents
                .iter()
                .map(move |parent| format!("{parent}:{path}"))
        })
        .collect();
    let mut probe = 0;
    repo.git_cat_file_batch_check_with_policy(
        probes,
        crate::domain::repo::ReadPolicy::LocalOnly,
        |_, kind, _| {
            if kind != "missing" {
                held.insert(probe / public_parents.len());
            }
            probe += 1;
            Ok(())
        },
    )?;
    for (index, path) in unknown.into_iter().enumerate() {
        if held.contains(&index) {
            seen.published.insert(path);
        } else {
            seen.added.insert(path);
        }
    }
    Ok(())
}

/// `oid` as plain publication carries it, and the records in it that keep parts the privacy check
/// left out.
#[derive(Debug, PartialEq, Eq)]
struct Conversation {
    /// `None` when every record `oid` holds is published as it is, otherwise a commit written to
    /// the source with the same message and parents whose LOG, VIEW and events hold only what
    /// [`publication_records`] keeps.
    ///
    /// [`publication_records`]: crate::domain::publication_records
    commit: Option<String>,
    /// The event path of each published record holding a placeholder the privacy check put where
    /// it left something out.
    left_out: BTreeSet<String>,
}

/// The longest path under `events/` a session tree may list. The reader refuses a longer one
/// before holding all of it, so with the most events a session may hold it bounds what
/// [`held_events`] keeps in memory; a bound checked only on the finished path leaves a tree with
/// one huge name free to fill memory first.
const MAX_EVENT_PATH_BYTES: usize = 4096;

/// Every event object `oid`'s tree holds under `events/`, named by its LOG or not.
///
/// The listing grows with the session, so it is streamed and bounded by what a session may hold.
/// Read whole through one local inspection, whose output bound is sized for a single object, it
/// refuses the snapshot of every session that recorded enough events.
fn held_events(source: &Repo, oid: &str) -> Result<BTreeSet<String>> {
    let mut held = BTreeSet::new();
    source.git_stream_split_local_bounded(
        &[
            "ls-tree",
            "-r",
            "-z",
            "--name-only",
            oid,
            "--",
            meta::EVENTS_DIR,
        ],
        0,
        MAX_EVENT_PATH_BYTES,
        |path| {
            ensure!(
                held.len() < crate::domain::storage::MAX_SEQUENCE_EVENTS,
                "a session tree holds more event objects than a session may"
            );
            held.insert(String::from_utf8(path.to_vec()).context("event path is not UTF-8")?);
            Ok(())
        },
    )?;
    Ok(held)
}

/// What a snapshot's public parents already published: lines a publication carries as they are.
#[derive(Default)]
struct Published<'a> {
    /// The event ids the public parents' LOGs name.
    ids: std::collections::HashSet<String>,
    /// When the snapshot's LOG extends a source parent's, as a merge's extends its target's, the
    /// public LOG of the longest such parent, in order.
    prefix: Option<Prefix<'a>>,
}

/// The public lines of the records a snapshot shares with a source parent whose LOG its own
/// extends. The publication rule decides a record by it and the records before it alone, so the
/// records the two share are kept the same way in both, and the public parent's LOG names them in
/// the same order.
struct Prefix<'a> {
    /// How many of the snapshot's LOG lines its source parent holds.
    source_lines: usize,
    /// The public parent's LOG, in order.
    public: Vec<String>,
    holder: &'a Repo,
    commit: String,
}

impl Prefix<'_> {
    /// The line the public parent publishes as `id`.
    fn line(&self, id: &str) -> Result<String> {
        use crate::domain::storage;
        let bytes = read(
            self.holder,
            &[
                "cat-file",
                "blob",
                &format!("{}:{}", self.commit, meta::event_path(id)?),
            ],
            storage::MAX_MATERIALIZED_BYTES,
        )?;
        let line = String::from_utf8(bytes).context("a published event is not UTF-8")?;
        ensure!(
            storage::event_id(&line)? == id,
            "a published event does not match its id"
        );
        Ok(line)
    }
}

/// Whether the public commit `commit` shows by its own tree that it is plain: a tree this run can
/// read, holding nothing under `privacy/`. An envelope of any format, a tree that can't be read or
/// any answer Git can't give cleanly says no, so its lines pass protection again.
fn shown_plain(source: &Repo, repo: &Repo, commit: &str) -> bool {
    [source, repo].into_iter().any(|holder| {
        matches!(
            holder.git_status_local(&["ls-tree", "--name-only", commit, "--", "privacy"]),
            Ok((Some(0), stdout, stderr)) if stdout.trim().is_empty() && stderr.is_empty()
        )
    })
}

/// [`Published`] for a snapshot `oid` with source `parents` and their `public_parents`. A parent
/// this run wrote is read through the source commit `written` names for it, which holds the same
/// LOG: what the run writes reaches the publication repository only when the run ends.
fn published_lines<'a>(
    source: &'a Repo,
    repo: &'a Repo,
    written: &std::collections::HashMap<String, String>,
    oid: &str,
    parents: &[String],
    public_parents: &[String],
) -> Result<Published<'a>> {
    use crate::domain::storage;
    let limit = storage::MAX_MATERIALIZED_BYTES;
    let blob = |holder: &Repo, commit: &str, path: &str| -> Result<Option<String>> {
        let spec = format!("{commit}:{path}");
        if holder.git_opt(&["cat-file", "-e", &spec]).is_none() {
            return Ok(None);
        }
        let bytes = read(holder, &["cat-file", "blob", &spec], limit)?;
        Ok(Some(
            String::from_utf8(bytes).context("a published LOG is not UTF-8")?,
        ))
    };
    let mut published = Published::default();
    let mut orders = Vec::new();
    for parent in public_parents {
        let commit = written.get(parent).unwrap_or(parent);
        let holder = [source, repo]
            .into_iter()
            .find(|holder| {
                holder
                    .git_opt(&["cat-file", "-e", &format!("{commit}^{{tree}}")])
                    .is_some()
            })
            .context("a published parent is unavailable")?;
        if let Some(log) = blob(holder, commit, meta::LOG_FILE)? {
            let ids = log.lines().map(str::to_owned).collect::<Vec<_>>();
            published.ids.extend(ids.iter().cloned());
            orders.push(Some((holder, commit.clone(), ids)));
        } else {
            if let Some(log) = blob(holder, commit, meta::LEGACY_LOG_FILE)? {
                published.ids.extend(
                    log.lines()
                        .filter(|line| !line.trim().is_empty())
                        .map(|line| storage::canonical_event_id(&format!("{line}\n"))),
                );
            }
            orders.push(None);
        }
    }
    if let Some(own) = blob(source, oid, meta::LOG_FILE)? {
        let own = own.lines().collect::<Vec<_>>();
        for (parent, order) in parents.iter().zip(&orders) {
            let Some((holder, commit, public)) = order else {
                continue;
            };
            let Some(shared) = blob(source, parent, meta::LOG_FILE)? else {
                continue;
            };
            let shared = shared.lines().collect::<Vec<_>>();
            if own.starts_with(&shared)
                && published
                    .prefix
                    .as_ref()
                    .is_none_or(|prefix| prefix.source_lines < shared.len())
            {
                published.prefix = Some(Prefix {
                    source_lines: shared.len(),
                    public: public.clone(),
                    holder,
                    commit: commit.clone(),
                });
            }
        }
    }
    Ok(published)
}

/// How much of a snapshot's added records one protection call takes. The records, their text and
/// what protection returns are all in memory during the call, so a batch the size of a long
/// session would add several times its size to a push's peak.
const PROTECTION_BATCH_BYTES: usize = 16 * 1024 * 1024;

/// Records a publication run has already passed through today's protection, by the digest of
/// their text as captured. Protection depends on a record's text alone, so a record passed once
/// is published the same way in every later snapshot of the run that carries it.
#[derive(Default)]
struct Rechecked {
    /// Records protection leaves as they are.
    unchanged: std::collections::HashSet<String>,
    /// Records protection changes, as they are published.
    protected: std::collections::HashMap<String, serde_json::Value>,
    /// Commit messages protection leaves as they are.
    messages: std::collections::HashSet<String>,
}

impl Rechecked {
    /// `contents`, records a snapshot's publication adds, as they are published today. A record is
    /// protected under the rules in force when it was captured; one captured before a rule existed,
    /// or before a value was registered, would otherwise go out holding what today's rules replace.
    /// So every record a publication adds passes capture's own protection again before anything
    /// is sent, and is judged as capture judges it: field by field on decoded values, each with its
    /// field's name as context. A scan of the serialized line is a different judgement, and finds
    /// less. A record protection leaves as it is stays the value it was, so its event id does not
    /// move. A record whose protection does not complete refuses the publication.
    ///
    /// Returns the indices of the records protection changed.
    fn contents(&mut self, repo: &Repo, contents: &mut [serde_json::Value]) -> Result<Vec<usize>> {
        let mut changed = Vec::new();
        let mut pending = Vec::new();
        let mut input = String::new();
        for (index, content) in contents.iter_mut().enumerate() {
            let text = serde_json::to_string(content)?;
            let digest = digest_bytes(text.as_bytes());
            if self.unchanged.contains(&digest) {
                continue;
            }
            if let Some(protected) = self.protected.get(&digest) {
                *content = protected.clone();
                changed.push(index);
                continue;
            }
            input.push_str(&text);
            input.push('\n');
            pending.push((index, digest, text));
        }
        if pending.is_empty() {
            return Ok(changed);
        }
        let output = crate::domain::privacy::service::protect_for_commit(
            Some(repo.root()),
            &input,
            crate::domain::privacy::projector::Mode::ProtectJsonl,
        )
        .map_err(|pending| {
            anyhow::Error::new(pending).context(
                "a record this publication carries did not pass today's privacy check, so nothing was published",
            )
        })?;
        let lines = output.split_terminator('\n').collect::<Vec<_>>();
        ensure!(
            lines.len() == pending.len(),
            "protection returned a different number of records than it was given"
        );
        for ((index, digest, text), line) in pending.into_iter().zip(lines) {
            if line == text {
                self.unchanged.insert(digest);
                continue;
            }
            let protected: serde_json::Value =
                serde_json::from_str(line).context("a protected record is not one JSON record")?;
            self.protected.insert(digest, protected.clone());
            contents[index] = protected;
            changed.push(index);
        }
        Ok(changed)
    }

    /// The commit a snapshot is published with, `commit` when an earlier step rewrote it,
    /// otherwise `oid`'s own, with its message as capture's text protection writes it today: a
    /// message is written text like any record. `None` when `commit` is `None` and protection
    /// leaves `oid`'s message as it is.
    fn message(
        &mut self,
        repo: &Repo,
        oid: &str,
        commit: Option<Vec<u8>>,
    ) -> Result<Option<Vec<u8>>> {
        let original = match &commit {
            Some(commit) => commit.clone(),
            None => read(repo, &["cat-file", "commit", oid], MAX_COMMIT)?,
        };
        let boundary = original
            .windows(2)
            .position(|bytes| bytes == b"\n\n")
            .context("publication source commit has no header boundary")?;
        let message = std::str::from_utf8(&original[boundary + 2..])
            .context("publication source commit message is not UTF-8")?;
        let digest = digest_bytes(message.as_bytes());
        if self.messages.contains(&digest) {
            return Ok(commit);
        }
        let protected = crate::domain::privacy::service::protect_for_commit(
            Some(repo.root()),
            message,
            crate::domain::privacy::projector::Mode::ProtectText,
        )
        .map_err(|pending| {
            anyhow::Error::new(pending).context(
                "a commit message this publication carries did not pass today's privacy check, so nothing was published",
            )
        })?;
        if protected == message {
            self.messages.insert(digest);
            return Ok(commit);
        }
        let mut rewritten = original[..boundary + 2].to_vec();
        rewritten.extend_from_slice(protected.as_bytes());
        Ok(Some(rewritten))
    }
}

/// What encrypted snapshots below a plain one published, which it carries (see `frozen`).
#[derive(Default)]
struct Carried<'a> {
    /// The nearest encrypted snapshot below, or the plain one the destination holds above it.
    projected: Option<&'a super::frozen::Published>,
    /// Those a merge below it brought in.
    others: &'a [&'a super::frozen::Published],
    /// Files the session changed while it was published encrypted, put back as its file line had
    /// them.
    held_back: Vec<(String, Option<super::frozen::Restored>)>,
}

/// The commit whose LOG and VIEW plain publication sends for `oid`, a turn on `parent`, when
/// `public_parent` is the published form of `parent`: what the next push publishes for it, under
/// this computer's rules today. `None` when `public_parent` doesn't show by its own tree that it
/// is plain, since the push then passes every line through protection again.
pub(super) fn plain_published(
    source: &Repo,
    oid: &str,
    parent: &str,
    public_parent: &str,
) -> Result<Option<String>> {
    if !shown_plain(source, source, public_parent) {
        return Ok(None);
    }
    let parents = [parent.to_owned()];
    let published = published_lines(
        source,
        source,
        &std::collections::HashMap::new(),
        oid,
        &parents,
        &[public_parent.to_owned()],
    )?;
    let conversation = conversation_commit(
        source,
        oid,
        &parents,
        &published,
        Carried::default(),
        &mut Rechecked::default(),
    )?;
    Ok(Some(conversation.commit.unwrap_or_else(|| oid.to_owned())))
}

/// Only publication metadata and mapped parents change; native evidence and file modes stay exact.
/// The local commit is left as it is, so local history and resume keep the complete native
/// transcript. `published` is what plain public parents already hold, and `carried` what encrypted
/// snapshots below `oid` published.
fn conversation_commit(
    source: &Repo,
    oid: &str,
    parents: &[String],
    published: &Published<'_>,
    carried: Carried<'_>,
    rechecked: &mut Rechecked,
) -> Result<Conversation> {
    let Carried {
        projected,
        others,
        held_back,
    } = carried;
    use crate::domain::{publication_records, storage, transcript::Envelope};
    let limit = storage::MAX_MATERIALIZED_BYTES;
    let (log, view) = storage::materialize_pair_local(source.root(), oid, limit, limit)?;
    let lines = super::frozen::record_lines;
    let log = lines(&log);
    let envelopes = log
        .iter()
        .map(|line| storage::parse_legacy_envelope_line(line))
        .collect::<Result<Vec<_>>>()?;
    // A tool step's context agit's plugin handed over goes out as the typed message it was,
    // where this device's records vouch for it; any other stays context and is left out.
    let typed = crate::rc::step_delivery::typed_records(
        envelopes
            .iter()
            .map(|envelope| (envelope.source.as_str(), &envelope.content)),
    );
    let kept = publication_records::published_sequence(envelopes.iter().zip(&typed).map(
        |(envelope, typed)| {
            (
                envelope.source.as_str(),
                typed.as_ref().unwrap_or(&envelope.content),
            )
        },
    ));
    // Records an encrypted snapshot already published keep the projection their readers saw,
    // narrowed by today's rules.
    let spliced = super::frozen::splice(&envelopes, kept, projected, others)?;
    let spliced = spliced
        .into_iter()
        .map(|line| line.map(|line| storage::parse_legacy_envelope_line(&line)))
        .map(Option::transpose)
        .collect::<Result<Vec<_>>>()?;
    let public = |envelope: &Envelope, content: serde_json::Value| Envelope {
        source: envelope.source.clone(),
        session_id: envelope.session_id.clone(),
        object_hash: crate::domain::transcript::object_hash(&content),
        content,
    };
    // A line a plain public parent already holds goes out as it is: it is published, and this LOG
    // extends the parent's only while those lines keep their bytes. A record this snapshot shares
    // with the parent whose LOG it extends goes out as the line the public parent published for
    // it, though protection changed it then and today's rules would change it again. Every other
    // line passes today's protection first, a record an encrypted snapshot published as well:
    // its readers weren't plain readers, and protection only replaces more.
    let prefix = published.prefix.as_ref().filter(|prefix| {
        prefix.source_lines <= spliced.len()
            && spliced[..prefix.source_lines]
                .iter()
                .filter(|line| line.is_some())
                .count()
                == prefix.public.len()
    });
    let mut shared = 0;
    let mut public_lines = Vec::with_capacity(log.len());
    let mut fresh = Vec::new();
    let mut contents = Vec::new();
    let mut batched = 0;
    let mut protect = |fresh: &mut Vec<usize>,
                       contents: &mut Vec<serde_json::Value>,
                       public_lines: &mut Vec<Option<String>>|
     -> Result<()> {
        for changed in rechecked.contents(source, contents)? {
            let index = fresh[changed];
            let content = std::mem::take(&mut contents[changed]);
            let base = spliced[index]
                .as_ref()
                .context("a protected record has no line")?;
            public_lines[index] = Some(storage::envelope_line(&public(base, content)));
        }
        fresh.clear();
        contents.clear();
        Ok(())
    };
    for (index, candidate) in spliced.iter().enumerate() {
        let Some(candidate) = candidate else {
            public_lines.push(None);
            continue;
        };
        let mut line = storage::envelope_line(candidate);
        let id = storage::canonical_event_id(&line);
        if let Some(prefix) = prefix.filter(|prefix| index < prefix.source_lines) {
            if prefix.public[shared] != id {
                line = prefix.line(&prefix.public[shared])?;
            }
            shared += 1;
        } else if !published.ids.contains(&id) {
            batched += line.len();
            fresh.push(index);
            contents.push(candidate.content.clone());
        }
        public_lines.push(Some(line));
        if batched >= PROTECTION_BATCH_BYTES {
            protect(&mut fresh, &mut contents, &mut public_lines)?;
            batched = 0;
        }
    }
    protect(&mut fresh, &mut contents, &mut public_lines)?;
    let mut published = BTreeMap::<&str, Option<String>>::new();
    let mut public_log = String::new();
    let mut changed = false;
    for (line, public) in log.iter().zip(public_lines) {
        changed |= public.as_deref() != Some(line.as_str());
        if let Some(public) = &public {
            public_log.push_str(public);
        }
        published.entry(line).or_insert(public);
    }
    let text = read(
        source,
        &["cat-file", "blob", &format!("{oid}:{}", meta::FILE)],
        meta::MAX_BYTES,
    )?;
    let mut metadata: Meta = serde_json::from_slice(&text)?;
    // Every event object a v1 tree holds, named by its LOG or not: one that is not published
    // must leave the tree, so a tree holding one is never published as it is. A v0 tree has no
    // such objects; an `events/` there is the repository's own content.
    let held = if metadata.layout == meta::LayoutVersion::V1 {
        held_events(source, oid)?
    } else {
        BTreeSet::new()
    };
    let named = published
        .values()
        .flatten()
        .map(|line| meta::event_path(&storage::event_id(line)?))
        .collect::<Result<BTreeSet<_>>>()?;
    let left_out = published
        .values()
        .flatten()
        .filter(|line| {
            use crate::domain::privacy::{
                projector::UNREADABLE_TEXT,
                service::{CROWDED_TEXT, OMITTED_TEXT},
            };
            line.contains(OMITTED_TEXT)
                || line.contains(CROWDED_TEXT)
                || line.contains(UNREADABLE_TEXT)
        })
        .map(|line| meta::event_path(&storage::event_id(line)?))
        .collect::<Result<BTreeSet<_>>>()?;
    changed |= held.iter().any(|path| !named.contains(path));
    changed |= !held_back.is_empty();
    if !changed {
        return Ok(Conversation {
            commit: None,
            left_out,
        });
    }
    let mut public_view = String::new();
    for line in lines(&view) {
        let public = published
            .get(line.as_str())
            .context("a session VIEW holds an event its LOG does not")?;
        if let Some(public) = public {
            public_view.push_str(public);
        }
    }

    let tree = if metadata.layout == meta::LayoutVersion::V1 {
        // Only what changes is written: every event object the tree holds that is not published
        // is removed, events that changed are added, and LOG and VIEW are replaced. Every other
        // event keeps its object.
        let files = storage::snapshot_files(&public_log, &public_view)?;
        let original = held;
        let mut edits: Vec<(String, Option<Vec<u8>>)> = original
            .iter()
            .filter(|path| !files.contains_key(*path))
            .map(|path| (path.clone(), None))
            .collect();
        edits.extend(
            files
                .into_iter()
                .filter(|(path, _)| {
                    path == meta::LOG_FILE || path == meta::VIEW_FILE || !original.contains(path)
                })
                .map(|(path, bytes)| (path, Some(bytes))),
        );
        crate::commands::plumbing::tree_apply_owned(source, oid, edits)?
    } else {
        // A v0 snapshot is published in the v1 layout its rebuilt tree has.
        metadata.layout = meta::LayoutVersion::V1;
        crate::commands::plumbing::session_snapshot_tree(
            source,
            oid,
            &public_log,
            &public_view,
            &meta::to_text(&metadata)?,
        )?
    };
    let tree = if held_back.is_empty() {
        tree
    } else {
        crate::commands::plumbing::tree_restore_owned(source, &tree, held_back)?
    };
    let original = read(source, &["cat-file", "commit", oid], MAX_COMMIT)?;
    Ok(Conversation {
        commit: Some(write_object(
            source,
            "commit",
            &commit_body(&tree, parents, &original)?,
        )?),
        left_out,
    })
}

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
            meta::MAX_BYTES,
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
        let text = serde_json::to_vec(&value)?;
        meta::ensure_within_limit(&text)?;
        let blob = write_object(destination, "blob", &text)?;
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

#[allow(clippy::too_many_arguments)]
fn retain_tags(
    source: &Repo,
    plan: &PublicationPlan,
    destination: &Repo,
    candidates: &BTreeMap<String, accepted::Candidate>,
    withheld: &BTreeSet<String>,
    references: &mut BTreeMap<String, String>,
    left_out: &mut Vec<LeftOutTag>,
) -> Result<BTreeSet<String>> {
    // Tags moved onto a public commit, which the public plan may hold below its boundary.
    let mut moved = BTreeSet::new();
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
        if !mappings.is_empty() && mappings.iter().all(|entry| entry.public == commit) {
            insert_tag(references, tag.name(), tag.oid())?;
            continue;
        }
        ensure!(
            !mappings.is_empty() && mappings.iter().all(|entry| entry.plain),
            "custom tag {} points to rewritten publication history; remove it from the source or recreate it on the accepted public commit",
            tag.name()
        );
        let publics = mappings
            .iter()
            .map(|entry| entry.public.as_str())
            .collect::<BTreeSet<_>>();
        match session_tag_target(tag.oid(), &commit, &publics) {
            Ok(public) => {
                insert_tag(references, tag.name(), &public)?;
                moved.insert(tag.name().to_owned());
            }
            Err(reason) => left_out.push(LeftOutTag::new(tag.name(), reason)),
        }
    }
    for oid in plan.tag_objects() {
        let bytes = read(source, &["cat-file", "tag", oid], MAX_COMMIT)?;
        ensure!(
            write_object(destination, "tag", &bytes)? == *oid,
            "publication tag changed object identity"
        );
    }
    Ok(moved)
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
    on_hub: &mut BTreeMap<String, String>,
    left_out: &mut Vec<LeftOutTag>,
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
        if !held.contains(&commit) {
            // A turn plain publication rewrote: its tag goes on the public commit the
            // destination accepted for it, which the accepted ledger names.
            let entries = branches
                .iter()
                .filter_map(|branch| ledger.get(branch, &commit).map(|entry| (branch, entry)))
                .collect::<Vec<_>>();
            ensure!(
                !entries.is_empty() && entries.iter().all(|(_, entry)| entry.plain),
                "custom tag {} points to rewritten publication history; remove it from the source or recreate it on the accepted public commit",
                tag.name()
            );
            let publics = entries
                .iter()
                .map(|(_, entry)| entry.public.as_str())
                .collect::<BTreeSet<_>>();
            match session_tag_target(tag.oid(), &commit, &publics) {
                Ok(public) if remote.refs.get(tag.name()) == Some(&public) => {
                    on_hub.insert(tag.name().to_owned(), public);
                }
                Ok(public) => {
                    let (branch, entry) = entries
                        .iter()
                        .find(|(_, entry)| entry.public == public)
                        .copied()
                        .context("an accepted public commit has no mapping")?;
                    let retained = entry.object_source(source, parent, branch)?;
                    crate::commands::plumbing::import_commit_graph(
                        destination,
                        &retained,
                        &public,
                    )?;
                    insert_tag(references, tag.name(), &public)?;
                    carried.insert(tag.name().to_owned());
                }
                Err(reason) => left_out.push(LeftOutTag::new(tag.name(), reason)),
            }
            continue;
        }
        crate::commands::plumbing::import_commit_graph(destination, source, &commit)?;
        for oid in &objects {
            let bytes = read(source, &["cat-file", "tag", oid], MAX_COMMIT)?;
            ensure!(
                write_object(destination, "tag", &bytes)? == *oid,
                "publication tag changed object identity"
            );
        }
        insert_tag(references, tag.name(), tag.oid())?;
        carried.insert(tag.name().to_owned());
    }
    Ok(carried)
}

/// A local tag a publication does not send, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LeftOutTag {
    /// The tag's name without `refs/tags/`.
    pub name: String,
    pub reason: TagLeftOut,
}

impl LeftOutTag {
    fn new(reference: &str, reason: TagLeftOut) -> Self {
        Self {
            name: reference
                .strip_prefix("refs/tags/")
                .unwrap_or(reference)
                .to_owned(),
            reason,
        }
    }
}

/// Why a tag on a session turn is not sent. A tag is only ever published on a commit the same
/// publication sends or the destination already holds; it never makes a commit go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TagLeftOut {
    /// An annotated tag object names the local commit and carries a message nothing protects.
    Annotated,
    /// Its turn was published as more than one public commit.
    Ambiguous,
    /// Its turn is not part of this upload and not on the Hub.
    NotSent,
    /// Its turn is on the Hub, on a branch this upload does not send.
    OtherBranch,
}

impl TagLeftOut {
    pub(crate) fn describe(self) -> &'static str {
        match self {
            Self::Annotated => {
                "it has a message; a tag on a session turn is published only without one (`agit tag NAME TARGET`)"
            }
            Self::Ambiguous => {
                "its turn was published more than once; tag the published commit you mean"
            }
            Self::NotSent => {
                "its turn is not part of this upload and not on the Hub; push the branch it is on"
            }
            Self::OtherBranch => {
                "its turn is on a branch this push doesn't include; push that branch to send the tag"
            }
        }
    }
}

/// The public commit a tag on a session turn is published on: the one commit `publics` names,
/// when the tag is lightweight. Its name is published as it is.
fn session_tag_target(
    oid: &str,
    commit: &str,
    publics: &BTreeSet<&str>,
) -> std::result::Result<String, TagLeftOut> {
    let mut each = publics.iter();
    let (Some(public), None) = (each.next(), each.next()) else {
        return Err(TagLeftOut::Ambiguous);
    };
    if oid != commit {
        return Err(TagLeftOut::Annotated);
    }
    Ok((*public).to_owned())
}

/// The tags this publication sends on another commit than the one the same local tag names: a
/// tag on a turn plain publication rewrote. Version tags are named by their public commit and
/// have no local counterpart.
fn published_elsewhere(
    source: &Repo,
    references: &BTreeMap<String, String>,
) -> Result<Vec<crate::domain::privacy_receipt::PublishedTag>> {
    let listing = source.git(&[
        "for-each-ref",
        "--format=%(objectname) %(refname)",
        "refs/tags/",
    ])?;
    let local = listing
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(oid, name)| (name, oid))
        .collect::<BTreeMap<_, _>>();
    Ok(references
        .iter()
        .filter(|(name, _)| name.starts_with("refs/tags/") && !name.starts_with("refs/tags/agit-"))
        .filter_map(|(name, public)| {
            let here = *local.get(name.as_str())?;
            (here != public).then(|| crate::domain::privacy_receipt::PublishedTag {
                name: name.trim_start_matches("refs/tags/").to_owned(),
                local: here.to_owned(),
                public: public.clone(),
            })
        })
        .collect())
}

fn insert_tag(references: &mut BTreeMap<String, String>, name: &str, oid: &str) -> Result<()> {
    if let Some(previous) = references.insert(name.to_owned(), oid.to_owned()) {
        ensure!(
            previous == oid,
            "custom tag conflicts with a generated version tag"
        );
    }
    Ok(())
}

/// Tags on session turns no branch of this publication reaches: one put on a turn's public commit
/// itself, or on a turn of a branch this upload leaves out. A tag whose public commit this
/// publication sends, or the destination already holds, is published there; any other is named
/// with why it stays here. Tags on other history are not this publication's to send.
#[allow(clippy::too_many_arguments)]
fn place_unreached_tags(
    source: &Repo,
    destination: &Repo,
    parent: &std::path::Path,
    ledger: &accepted::Ledger,
    candidates: &BTreeMap<String, accepted::Candidate>,
    remote: &RemoteRefs,
    handled: &BTreeSet<String>,
    references: &mut BTreeMap<String, String>,
    carried: &mut BTreeSet<String>,
    on_hub: &mut BTreeMap<String, String>,
    left_out: &mut Vec<LeftOutTag>,
) -> Result<()> {
    let sending = candidates
        .values()
        .flat_map(|candidate| candidate.mappings.values())
        .map(|entry| entry.public.as_str())
        .collect::<BTreeSet<_>>();
    let listing = source.git(&[
        "for-each-ref",
        "--format=%(objectname) %(refname)",
        "refs/tags/",
    ])?;
    for (oid, name) in listing.lines().filter_map(|line| line.split_once(' ')) {
        if handled.contains(name)
            || references.contains_key(name)
            || name.starts_with("refs/tags/agit-")
            || ledger.withheld().contains(name)
            || remote.refs.get(name).is_some_and(|current| current == oid)
        {
            continue;
        }
        let Some(commit) =
            source.git_opt(&["rev-parse", "--verify", "-q", &format!("{oid}^{{commit}}")])
        else {
            continue;
        };
        let commit = commit.trim().to_owned();
        let on_session_line = matches!(
            crate::domain::meta::read_at_ref_result(source, &commit),
            Ok(Some(metadata)) if metadata.is_session_line()
        );
        if !on_session_line {
            continue;
        }
        let accepted = ledger
            .every_accepted_by_branch()
            .filter(|(_, entry)| entry.plain && (entry.public == commit || entry.source == commit))
            .collect::<Vec<_>>();
        let publics = if sending.contains(commit.as_str()) {
            BTreeSet::from([commit.as_str()])
        } else {
            accepted
                .iter()
                .map(|(_, entry)| entry.public.as_str())
                .collect()
        };
        if publics.is_empty() {
            left_out.push(LeftOutTag::new(name, TagLeftOut::NotSent));
            continue;
        }
        match session_tag_target(oid, &commit, &publics) {
            Ok(public) if remote.refs.get(name) == Some(&public) => {
                on_hub.insert(name.to_owned(), public);
            }
            Ok(public) if sending.contains(public.as_str()) => {
                insert_tag(references, name, &public)?;
                carried.insert(name.to_owned());
            }
            // An accepted turn's public commit is on the destination; whether this push's
            // branches reach it, and so take the tag, the final plan decides.
            Ok(public) => match accepted.iter().find(|(_, entry)| entry.public == public) {
                Some(&(branch, entry)) => {
                    let retained = entry.object_source(source, parent, branch)?;
                    crate::commands::plumbing::import_commit_graph(
                        destination,
                        &retained,
                        &public,
                    )?;
                    insert_tag(references, name, &public)?;
                    carried.insert(name.to_owned());
                }
                None => left_out.push(LeftOutTag::new(name, TagLeftOut::NotSent)),
            },
            Err(reason) => left_out.push(LeftOutTag::new(name, reason)),
        }
    }
    Ok(())
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
        use crate::domain::privacy_envelope::MAX_ENVELOPE_BYTES;
        use crate::domain::repo::{ObjectBody, ReadPolicy};
        enum Step {
            Envelope(usize),
            Commit(usize),
        }
        let manifest = &self.session_manifest;
        // Every snapshot's commit, and the envelope of an encrypted one, stream through one batch
        // in snapshot order; each is reduced to its preview as it arrives, so no more than one
        // envelope is held at a time.
        let mut reads = Vec::new();
        let mut steps = Vec::new();
        for (index, entry) in manifest.snapshots.iter().enumerate() {
            let commit = entry.commit();
            if matches!(entry, PublishedSnapshot::SessionOwnerV2 { .. }) {
                reads.push(format!("{commit}:privacy/envelope.json"));
                steps.push(Step::Envelope(index));
            }
            reads.push(commit.to_owned());
            steps.push(Step::Commit(index));
        }
        let mut publics = vec![None; manifest.snapshots.len()];
        let mut gits = vec![None; manifest.snapshots.len()];
        let mut steps = steps.into_iter();
        self.repo.git_cat_file_batch_with_policy(
            reads,
            MAX_ENVELOPE_BYTES.max(MAX_COMMIT),
            ReadPolicy::LocalOnly,
            |_, kind, body| {
                let step = steps
                    .next()
                    .context("session preview read is unrequested")?;
                let ObjectBody::Read(bytes) = body else {
                    anyhow::bail!("privacy source object is unavailable");
                };
                match step {
                    Step::Envelope(index) => {
                        let PublishedSnapshot::SessionOwnerV2 { snapshot, .. } =
                            &manifest.snapshots[index]
                        else {
                            anyhow::bail!("session preview read is unrequested");
                        };
                        ensure!(
                            kind == "blob" && bytes.len() <= MAX_ENVELOPE_BYTES,
                            "privacy source object is unavailable"
                        );
                        ensure!(
                            digest_bytes(bytes) == snapshot.envelope_digest,
                            "session preview envelope differs from its manifest"
                        );
                        let envelope = PrivacyEnvelope::parse(bytes)?;
                        ensure!(
                            envelope.session_binding.as_ref() == Some(&snapshot.binding),
                            "session preview authority differs from its manifest"
                        );
                        publics[index] = Some(envelope.public_projection);
                    }
                    Step::Commit(index) => {
                        ensure!(
                            kind == "commit" && bytes.len() <= MAX_COMMIT,
                            "privacy source object is unavailable"
                        );
                        gits[index] = Some(commit_preview(bytes)?);
                    }
                }
                Ok(())
            },
        )?;
        ensure!(steps.next().is_none(), "session preview read is truncated");
        let mut validator = super::super::storage::SnapshotValidator::default();
        let mut snapshots = Vec::new();
        for (index, entry) in manifest.snapshots.iter().enumerate() {
            let commit = entry.commit();
            let public = match entry {
                PublishedSnapshot::SessionOwnerV2 { .. } => publics[index]
                    .take()
                    .context("session preview read is truncated")?,
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
            let git = gits[index]
                .take()
                .context("session preview read is truncated")?;
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

    /// A v0 snapshot keeps its session in the three root files, and an `events/` directory beside
    /// them is the repository's own content: a snapshot whose records are all published needs no
    /// projection, and the push publishes it as it is.
    #[test]
    fn a_v0_snapshot_with_a_repository_events_directory_is_published_as_it_is() {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init", "-q"]);
        let mut session = Meta::new(
            format!("agit-{}", "a".repeat(40)),
            "claude-code".into(),
            String::new(),
        );
        session.layout = meta::LayoutVersion::V0;
        let log = crate::domain::transcript::wrap_lines(
            &format!(
                "{}\n",
                json!({"type":"user","message":{"role":"user","content":"hello"}})
            ),
            "claude-code",
            &session.session,
        );
        std::fs::create_dir_all(temp.path().join("session")).unwrap();
        std::fs::write(temp.path().join(meta::LEGACY_LOG_FILE), &log).unwrap();
        std::fs::write(temp.path().join(meta::LEGACY_VIEW_FILE), &log).unwrap();
        std::fs::write(
            temp.path().join(meta::FILE),
            serde_json::to_vec(&session).unwrap(),
        )
        .unwrap();
        std::fs::create_dir_all(temp.path().join(meta::EVENTS_DIR)).unwrap();
        std::fs::write(
            temp.path().join("events/notes.md"),
            "the team's event notes\n",
        )
        .unwrap();
        git(temp.path(), &["add", "-A"]);
        git(temp.path(), &["commit", "-q", "-m", "v0 snapshot"]);
        let head = git(temp.path(), &["rev-parse", "HEAD"]);
        let repo = Repo::at(temp.path());
        assert_eq!(
            conversation_commit(
                &repo,
                head.trim(),
                &[],
                &Published::default(),
                Carried::default(),
                &mut Rechecked::default()
            )
            .unwrap(),
            Conversation {
                commit: None,
                left_out: BTreeSet::new(),
            }
        );
    }

    /// A WorkBuddy request an earlier publication rule withheld is published on the session's next
    /// push. The public copy of the parent keeps fewer of the records the two snapshots share than
    /// today's rule keeps, so none of that copy's lines are reused for the shared part and each
    /// record there is decided again; the reply it did publish goes out as it was. A publication
    /// that reused the public parent's lines whenever the LOG extends it would keep the person's
    /// question hidden for good.
    #[test]
    fn a_request_an_earlier_rule_withheld_is_published_on_the_next_push() {
        use crate::domain::{storage, transcript::Envelope};
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("source");
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q"]);
        let session = format!("agit-{}", "b".repeat(40));
        let line = |content: serde_json::Value| {
            storage::envelope_line(&Envelope {
                source: "workbuddy".into(),
                session_id: session.clone(),
                object_hash: crate::domain::transcript::object_hash(&content),
                content,
            })
        };
        let asked = |request: &str| {
            line(
                json!({"type":"message","role":"user","content":[{"type":"input_text",
                "text":format!("<system-reminder>rules</system-reminder><user_query>{request}</user_query>")}]}),
            )
        };
        let replied = line(
            json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"Done."}]}),
        );
        let snapshot = |name: &str, parents: &[&str], log: &[String]| {
            let work = temp.path().join(name);
            std::fs::create_dir_all(&work).unwrap();
            storage::write_snapshot(&work, &log.concat(), &log.concat()).unwrap();
            meta::write(
                &work,
                &Meta::new(session.clone(), "workbuddy".into(), String::new()),
            )
            .unwrap();
            let index = temp.path().join(format!("{name}.index"));
            let staged = |args: &[&str]| {
                let output = std::process::Command::new("git")
                    .current_dir(&work)
                    .args(args)
                    .env("GIT_DIR", root.join(".git"))
                    .env("GIT_WORK_TREE", &work)
                    .env("GIT_INDEX_FILE", &index)
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
            };
            staged(&["add", "-A", "."]);
            let tree = staged(&["write-tree"]);
            let mut args = vec!["commit-tree", tree.as_str(), "-m", "turn"];
            for parent in parents {
                args.extend(["-p", parent]);
            }
            git(&root, &args)
        };
        let first = asked("Build a desk.");
        let parent = snapshot("parent", &[], &[first.clone(), replied.clone()]);
        let child = snapshot(
            "child",
            &[&parent],
            &[first, replied.clone(), asked("Add a chart.")],
        );
        // What the earlier rule published for the parent: the reply alone.
        let public = snapshot("public", &[], &[replied.clone()]);
        let reply = storage::canonical_event_id(&replied);
        let source = Repo::at(&root);
        let published = Published {
            ids: [reply.clone()].into_iter().collect(),
            prefix: Some(Prefix {
                source_lines: 2,
                public: vec![reply],
                holder: &source,
                commit: public,
            }),
        };
        let projected = conversation_commit(
            &source,
            &child,
            &[parent],
            &published,
            Carried::default(),
            &mut Rechecked::default(),
        )
        .unwrap();
        let limit = storage::MAX_MATERIALIZED_BYTES;
        let (log, _) = storage::materialize_pair_local(
            &root,
            &projected.commit.expect("the snapshot is rewritten"),
            limit,
            limit,
        )
        .unwrap();
        assert!(
            log.contains("Build a desk.") && log.contains("Add a chart.") && log.contains("Done."),
            "{log}"
        );
        assert!(!log.contains("rules"), "{log}");
    }

    /// A long session's snapshot lists every event object it holds, where its listing is longer
    /// than one local inspection may print. Each object is one line of the listing, so a listing
    /// read whole through that inspection refuses the first publication of every session that
    /// recorded enough events.
    #[test]
    fn a_snapshot_lists_every_event_object_past_one_inspections_output() {
        use std::io::Write;
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init", "-q"]);
        let line = meta::event_path(&"0".repeat(meta::EVENT_ID_HEX_LEN))
            .unwrap()
            .len()
            + 1;
        let count = crate::domain::repo::MAX_INSPECTION_OUTPUT_BYTES / line + 1;
        let blob = std::process::Command::new("git")
            .arg("-C")
            .arg(temp.path())
            .args(["hash-object", "-w", "--stdin"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                child.stdin.take().unwrap().write_all(b"{}\n")?;
                child.wait_with_output()
            })
            .unwrap();
        let blob = String::from_utf8(blob.stdout).unwrap();
        let mut index = std::process::Command::new("git")
            .arg("-C")
            .arg(temp.path())
            .args(["update-index", "--add", "--index-info"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = index.stdin.take().unwrap();
        for id in 0..count {
            let path = meta::event_path(&format!("{id:040x}")).unwrap();
            writeln!(input, "100644 {}\t{path}", blob.trim()).unwrap();
        }
        drop(input);
        assert!(index.wait().unwrap().success());
        let tree = git(temp.path(), &["write-tree"]);
        let commit = git(temp.path(), &["commit-tree", &tree, "-m", "long session"]);
        let source = Repo::at(temp.path()).local_objects_only();
        assert_eq!(held_events(&source, &commit).unwrap().len(), count);
    }

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
                pins: Default::default(),
                fork_sources: readable_fork_sources(
                    &hub,
                    (destination, agent),
                    BTreeSet::from(["alice/private".to_owned()]),
                ),
                pinned_source: pinned_source(&source, &hub),
                switched: Vec::new(),
                carried_records_proof: true,
                to_register: Vec::new(),
                asking_again: Default::default(),
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
            let mut left_out = Vec::new();
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
                &mut BTreeMap::new(),
                &mut left_out,
            )
            .unwrap();
            assert!(left_out.is_empty(), "{left_out:?}");
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

    /// A session that continues as plain is told in plain words, naming only the session; the
    /// person's own grants are mentioned only when the session is theirs, and a dry run says what
    /// the push will do.
    #[test]
    fn a_switched_session_is_told_in_plain_words() {
        let own = SwitchedSession {
            session: "agit-x".into(),
            target: Some("alice/app@work".into()),
            owned: true,
        };
        assert_eq!(
            own.notice(false),
            "agit-x now publishes like your other sessions: new turns upload only the redacted view people with access already see, and the originals stay on this computer. Files it changed before this, such as memory or skills, are not uploaded. People you gave access to the originals keep it for earlier turns only (`agit privacy access show alice/app@work` lists them)."
        );
        assert!(
            own.notice(true)
                .starts_with("agit-x will publish, once pushed, like")
        );
        let theirs = SwitchedSession {
            owned: false,
            ..own
        };
        let told = theirs.notice(false);
        assert!(told.starts_with("agit-x, another account's session, now publishes like"));
        assert!(told.ends_with("are not uploaded. Its owner is told on the Hub."));
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
