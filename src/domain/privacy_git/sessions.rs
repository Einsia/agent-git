//! Registered session modes govern introduced objects; accepted ancestry keeps its identity.

use super::*;
use crate::domain::privacy_envelope::digest_bytes;
use crate::domain::session_privacy::{
    AcceptedSnapshot, PublishedSnapshot, SESSION_ENVELOPE_VERSION, SessionPublicationManifest,
    SessionScheme, SessionSnapshotBinding, SourceCommitter,
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
}

impl SessionPublicationPlan {
    pub fn resolve(
        source: &Repo,
        branches: &[String],
        client: &crate::hub::Client,
        repository: &str,
        identity: RemoteIdentity,
        remote: RemoteRefs,
        register: bool,
    ) -> Result<Self> {
        ensure!(
            crate::domain::session_privacy::canonical_hub(client.base())? == identity.hub,
            "session publication belongs to another Hub"
        );
        let plan = PublicationPlan::freeze_selected(source, branches)?;
        let history = sources::History::read(source, &plan)?;
        let mut ledger = accepted::Ledger::open(source, &identity)?;
        ledger.reconcile(&remote)?;
        require_remote_ancestry(source, &plan, &ledger, &remote, repository)?;
        let accepted = plan.accepted_commits(source, &remote.heads)?;
        let enveloped = enveloped_commits(source, plan.commit_objects())?;
        let mut sessions = BTreeSet::new();
        for oid in plan.commit_objects() {
            let mut inherited = false;
            for branch in branches {
                inherited |= ledger.inherited_immutable(branch, oid)?.is_some();
            }
            if accepted.contains(oid) || inherited {
                continue;
            }
            let metadata = history.metadata(source, oid)?;
            ensure!(
                !enveloped.contains(oid),
                "unaccepted ciphertext requires its original publication authority"
            );
            if metadata.is_session_line() {
                ensure!(
                    meta::is_bare_id(&metadata.session),
                    "save a native snapshot before publishing an unclaimed session"
                );
                sessions.insert(metadata.session);
            }
        }
        let mut configs = BTreeMap::new();
        for session in sessions {
            let _mode_lock = super::modes::lock(source, &identity.hub, &session)?;
            let mut choice = super::modes::load(source, &identity.hub, &session)?;
            if let Some(choice) = &choice {
                choice.verify(&identity, repository)?;
            }
            let response = match client
                .registered_session_privacy(repository, &identity, &session)?
            {
                Some(response) => {
                    ensure!(
                        choice
                            .as_ref()
                            .is_none_or(|choice| choice.scheme == response.config.privacy.scheme),
                        "session publication mode is already registered and differs from the local choice"
                    );
                    response
                }
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
                    ensure!(
                        privacy.hub.as_deref() == Some(identity.hub.as_str()),
                        "session creator belongs to another Hub"
                    );
                    if privacy.scheme == SessionScheme::SessionOwnerV2
                        && privacy.creator.as_deref() == Some(account.account_id.as_str())
                    {
                        client.ensure_session_key(&account)?;
                    }
                    client.register_session_privacy(repository, &identity, &session, &privacy)?
                }
            };
            configs.insert(session, response);
        }
        plan.verify(source, branches)?;
        Ok(Self {
            identity,
            remote,
            configs,
        })
    }

    pub fn verify_configs(&self, client: &crate::hub::Client, repository: &str) -> Result<()> {
        for (session, expected) in &self.configs {
            let current = client.session_privacy_config(repository, &self.identity, session)?;
            ensure!(
                current.config == expected.config,
                "registered session authority changed during publication"
            );
        }
        Ok(())
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
    let parents = commit_parent_map(&source, plan.commit_objects())?;
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
        if contained {
            continue;
        }
        // A valid accepted ancestor establishes the publication baseline. Its remote successor
        // need not exist locally; absence of that object does not invalidate the local ledger.
        ensure!(
            mapped || !ledger.has_accepted_branch(branch),
            "{}",
            accepted::RECOVERY
        );
        #[cfg(windows)]
        use crate::ui::quote_powershell_argument as quote_arg;
        #[cfg(not(windows))]
        use crate::ui::session::shell_arg as quote_arg;
        let target = quote_arg(&format!("{repository}@{branch}"));
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
        let state = match difference {
            Some((0, behind)) => format!("is ahead of this checkout by {behind} commits"),
            Some((ahead, behind)) if behind > 0 => {
                format!("has diverged (local +{ahead} / remote +{behind})")
            }
            _ => "has changed beyond this checkout's accepted history".into(),
        };
        let tip = remote_head.chars().take(12).collect::<String>();
        anyhow::bail!(
            "remote branch {branch} {state} (Hub tip {tip}); run `agit pull {target}` before pushing; local unpublished commits are kept"
        );
    }
    Ok(())
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
        let identity = &selected.identity;
        ensure!(
            RemoteIdentity::new(&identity.hub, &identity.agent_id)? == *identity,
            "invalid session publication destination"
        );
        let source = source.clone().local_objects_only();
        let source_plan = PublicationPlan::freeze_selected(&source, branches)?;
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
        initialize_with_format(&root, source_plan.heads()[0].oid().len())?;
        let repo = Repo::at(&root).local_objects_only();
        let mut cache: Cache = match state::load(&root.join(".git/session-candidates"), &scope)? {
            Some(bytes) => {
                serde_json::from_slice(&bytes).context("invalid session candidate cache")?
            }
            None => Cache {
                version: CANDIDATE_CACHE_VERSION,
                ..Default::default()
            },
        };
        ensure!(
            matches!(cache.version, 2 | CANDIDATE_CACHE_VERSION),
            "unsupported session candidate cache"
        );
        if cache.version != CANDIDATE_CACHE_VERSION {
            // Session aliases remain stable, but anonymous candidates cannot identify attributed commits.
            cache.commits.clear();
            cache.version = CANDIDATE_CACHE_VERSION;
        }
        let mut ledger = accepted::Ledger::open(&source, identity)?;
        ledger.reconcile(&selected.remote)?;
        let parents = commit_parent_map(&source, source_plan.commit_objects())?;
        let enveloped = enveloped_commits(&source, source_plan.commit_objects())?;
        let order = parent_first(&parents)?;
        let source_sessions = sources::History::read(&source, &source_plan)?;
        let accepted = source_plan.accepted_commits(&source, &selected.remote.heads)?;
        let mut references = BTreeMap::new();
        let mut candidates = BTreeMap::new();
        let mut snapshots = BTreeMap::new();
        let mut preserved = BTreeSet::new();
        let mut reports = Vec::new();
        let mut imports = crate::commands::plumbing::CommitImports::default();
        #[cfg(test)]
        let mut preparations = 0;
        PathAliasStore::transact(&source, |aliases| {
            for head in source_plan.heads() {
                let branch = head
                    .name()
                    .strip_prefix("refs/heads/")
                    .context("invalid publication branch")?;
                ensure!(
                    redactor.publishes_unchanged(branch)?,
                    "publication branch name requires privacy rewriting; rename it before publishing"
                );
                let reachable = ancestors(head.oid(), &parents)?;
                if let Some(remote_head) = selected.remote.refs.get(head.name())
                    && !reachable.contains(remote_head)
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
                    let public_parents = parents[oid]
                        .iter()
                        .map(|oid| {
                            mapped
                                .get(oid)
                                .map(|entry| entry.public.clone())
                                .context("published parent is unavailable")
                        })
                        .collect::<Result<Vec<_>>>()?;
                    if let Some(entry) = ledger.inherited_immutable(branch, oid)? {
                        ensure!(
                            entry.source_parents == parents[oid]
                                && entry.public_parents == public_parents,
                            "{}",
                            accepted::RECOVERY
                        );
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
                            &public_parents,
                        )? {
                            aliases.reserve_aliases(&reused.inspection)?;
                            reports.push(reused.report);
                        }
                        if !plain {
                            retain_session_mapping(&repo, &entry.public, identity, &mut cache)?;
                        }
                        preserved.insert(entry.public.clone());
                        mapped.insert(oid.clone(), entry.clone());
                        continue;
                    }
                    if accepted.contains(oid) {
                        ensure!(
                            public_parents == parents[oid],
                            "accepted history cannot change parents"
                        );
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
                            },
                        );
                        continue;
                    }
                    let metadata = source_sessions.metadata(&source, oid).context(
                        "introduced snapshots require declared file or session metadata",
                    )?;
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
                        let public = if let Some(response) = config {
                            let mut published = metadata.clone();
                            if published
                                .session_privacy
                                .as_ref()
                                .is_some_and(|privacy| privacy.scheme != SessionScheme::Plain)
                            {
                                published.session_privacy = Some(response.config.privacy.clone());
                            }
                            if metadata.session_privacy == published.session_privacy
                                && public_parents == parents[oid]
                            {
                                oid.clone()
                            } else {
                                plain_commit(
                                    &source,
                                    &repo,
                                    oid,
                                    &public_parents,
                                    Some(&published),
                                )?
                            }
                        } else if public_parents != parents[oid] {
                            plain_commit(&source, &repo, oid, &public_parents, None)?
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
                                source_session: metadata.session.clone(),
                                public_session: metadata.session,
                                policy: policy_digest.clone(),
                                recipient: None,
                                plain: rewritten,
                            },
                        );
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
                    let original = read(&source, &["cat-file", "commit", oid], MAX_COMMIT)?;
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
                    let public_session = cache
                        .sessions
                        .entry(session_key)
                        .or_insert_with(meta::mint_session_id)
                        .clone();
                    let mut files = projection.prepare_git(&public_session)?;
                    let key = digest_json(
                        &json!({"source":oid, "parents":public_parents, "policy":policy_digest,
                        "config":config, "public":projection.public_value(), "private":projection.private_fingerprint()?}),
                    )?;
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
                        },
                    );
                    reports.push(projection.report().clone());
                }
                let public_head = mapped[head.oid()].public.clone();
                references.insert(head.name().to_owned(), public_head.clone());
                for entry in mapped.values() {
                    references.insert(
                        format!("refs/tags/agit-{}", entry.public),
                        entry.public.clone(),
                    );
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
        retain_tags(&source, &source_plan, &repo, &candidates, &mut references)?;
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
        let plan = PublicationPlan::freeze_selected(&repo, branches)?;
        source_plan.verify(&source, branches)?;
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
            session_manifest: manifest,
            #[cfg(test)]
            preparations,
            _lock: lock,
        })
    }
}

/// Only publication metadata and mapped parents change; native evidence and file modes stay exact.
fn plain_commit(
    source: &Repo,
    destination: &Repo,
    oid: &str,
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
    let original = read(source, &["cat-file", "commit", oid], MAX_COMMIT)?;
    let tree = source.git(&["rev-parse", &format!("{oid}^{{tree}}")])?;
    let tree = if let Some(metadata) = metadata {
        let mut value: serde_json::Value = serde_json::from_slice(&read(
            source,
            &["cat-file", "blob", &format!("{oid}:{}", meta::FILE)],
            1024 * 1024,
        )?)?;
        value["session_privacy"] = serde_json::to_value(&metadata.session_privacy)?;
        value["session"] = json!(metadata.session);
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
    references: &mut BTreeMap<String, String>,
) -> Result<()> {
    for tag in plan.tags() {
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

fn retain_session_mapping(
    repo: &Repo,
    commit: &str,
    identity: &RemoteIdentity,
    cache: &mut Cache,
) -> Result<()> {
    let Some(bytes) = repo
        .show_result(commit, "privacy/envelope.json")
        .ok()
        .flatten()
    else {
        return Ok(());
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&bytes) else {
        return Ok(());
    };
    if value
        .get("format_version")
        .and_then(serde_json::Value::as_u64)
        != Some(SESSION_ENVELOPE_VERSION.into())
    {
        return Ok(());
    }
    let envelope = PrivacyEnvelope::parse(bytes.as_bytes())?;
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
