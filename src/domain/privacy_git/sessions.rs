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
        url: &str,
        register: bool,
    ) -> Result<Self> {
        ensure!(
            crate::domain::session_privacy::canonical_hub(client.base())? == identity.hub,
            "session publication belongs to another Hub"
        );
        let plan = PublicationPlan::freeze_selected(source, branches)?;
        let history = sources::History::read(source, &plan)?;
        let remote =
            crate::hub::git::FrozenPublication::advertised_refs_for(source, url, &identity)
                .context("cannot establish accepted publication history")?;
        let ledger = accepted::Ledger::open(source, &identity)?;
        let accepted = plan.accepted_commits(source, &remote.heads)?;
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
                source.show_result(oid, "privacy/envelope.json")?.is_none(),
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
            let response = match client
                .registered_session_privacy(repository, &identity, &session)?
            {
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
                    if privacy.owner_source
                        == super::super::session_privacy::OwnerResolution::Unresolved
                    {
                        privacy = super::super::session_privacy::SessionPrivacy::new(Some(
                            account.clone(),
                        ))?;
                    }
                    ensure!(
                        privacy.scheme == SessionScheme::SessionOwnerV2,
                        "an unregistered session cannot opt into legacy plaintext publication"
                    );
                    ensure!(
                        privacy.hub.as_deref() == Some(identity.hub.as_str()),
                        "session creator belongs to another Hub"
                    );
                    if privacy.creator.as_deref() == Some(account.account_id.as_str()) {
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
            config.verify_registration(&self.identity.agent_id, &metadata.session, local)?;
        }
        Ok(response)
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
        let mut parents = BTreeMap::new();
        for oid in source_plan.commit_objects() {
            let bytes = read(&source, &["cat-file", "commit", oid], MAX_COMMIT)?;
            parents.insert(oid.clone(), commit_parents(&bytes)?);
        }
        let order = parent_first(&parents)?;
        let source_sessions = sources::History::read(&source, &source_plan)?;
        let accepted = source_plan.accepted_commits(&source, &selected.remote.heads)?;
        let mut references = BTreeMap::new();
        let mut candidates = BTreeMap::new();
        let mut snapshots = BTreeMap::new();
        let mut preserved = BTreeSet::new();
        let mut reports = Vec::new();
        #[cfg(test)]
        let mut preparations = 0;
        PathAliasStore::transact(&source, |aliases| {
            for head in source_plan.heads() {
                let branch = head
                    .name()
                    .strip_prefix("refs/heads/")
                    .context("invalid publication branch")?;
                ensure!(
                    redactor.try_scrub(branch)?.text == branch,
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
                        let retained = Repo::at(parent.join(&entry.store)).local_objects_only();
                        ensure!(
                            retained.root().join(".git/config").is_file()
                                && !retained
                                    .root()
                                    .join(".git/objects/info/alternates")
                                    .exists(),
                            "{}",
                            accepted::RECOVERY
                        );
                        if entry.source == entry.public {
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
                        retain_session_mapping(&repo, &entry.public, identity, &mut cache)?;
                        preserved.insert(entry.public.clone());
                        mapped.insert(oid.clone(), entry.clone());
                        continue;
                    }
                    if accepted.contains(oid) {
                        ensure!(
                            public_parents == parents[oid],
                            "accepted history cannot change parents"
                        );
                        crate::commands::plumbing::import_commit_graph(&repo, &source, oid)?;
                        retain_session_mapping(&repo, oid, identity, &mut cache)?;
                        let session = super::super::storage::metadata_local(source.root(), oid)
                            .map(|metadata| metadata.session)
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
                            public_parents == parents[oid],
                            "plain publication cannot expose unaccepted original ancestry from an encrypted session"
                        );
                        ensure!(
                            source.show_result(oid, "privacy/envelope.json")?.is_none(),
                            "encrypted or obsolete ciphertext cannot be admitted as plaintext"
                        );
                        crate::commands::plumbing::import_commit_graph(&repo, &source, oid)?;
                        let snapshot = match config {
                            None => PublishedSnapshot::FileLine {
                                commit: oid.clone(),
                            },
                            Some(response) => PublishedSnapshot::Plain {
                                commit: oid.clone(),
                                session_id: metadata.session.clone(),
                                config_revision: response.config.revision,
                            },
                        };
                        snapshots.insert(oid.clone(), snapshot);
                        mapped.insert(
                            oid.clone(),
                            accepted::Mapping {
                                source: oid.clone(),
                                public: oid.clone(),
                                source_parents: parents[oid].clone(),
                                public_parents,
                                store: store.clone(),
                                source_session: metadata.session.clone(),
                                public_session: metadata.session,
                                policy: policy_digest.clone(),
                                recipient: None,
                            },
                        );
                        continue;
                    }
                    let response = config.expect("encrypted session configuration");
                    let config = &response.config;
                    ensure!(
                        source.show_result(oid, "privacy/envelope.json")?.is_none(),
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
            "custom tag {} points to rewritten encrypted history; remove it from the source or recreate it on the accepted public commit",
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

    /// Plain file review reads captured payloads; a live LFS cache cannot change the reviewed bytes.
    pub(crate) fn plain_file_preview(
        &self,
        captured: &crate::hub::git::CapturedPublication,
    ) -> Result<Vec<serde_json::Value>> {
        use std::io::Read;
        ensure!(
            captured.plan() == &self.plan,
            "preview capture differs from its publication plan"
        );
        let manifest = &self.session_manifest;
        let mut budget = super::super::privacy_publication::MAX_INPUT_BYTES;
        let mut files = Vec::new();
        for entry in &manifest.snapshots {
            if matches!(entry, PublishedSnapshot::SessionOwnerV2 { .. }) {
                continue;
            }
            let commit = entry.commit();
            let tree = self
                .repo
                .git_bytes_result(&["ls-tree", "-r", "-l", "-z", commit])?;
            for entry in tree
                .split(|byte| *byte == 0)
                .filter(|entry| !entry.is_empty())
            {
                let entry =
                    std::str::from_utf8(entry).context("plain preview requires UTF-8 paths")?;
                let (header, path) = entry
                    .split_once('\t')
                    .context("invalid plain preview tree")?;
                let parts = header.split_whitespace().collect::<Vec<_>>();
                ensure!(parts.len() == 4, "invalid plain preview tree entry");
                if parts[1] != "blob" {
                    continue;
                }
                let size: usize = parts[3].parse()?;
                budget = budget
                    .checked_sub(size)
                    .context("plain preview exceeds its content budget")?;
                let mut bytes = read(&self.repo, &["cat-file", "blob", parts[2]], size)?;
                ensure!(bytes.len() == size, "plain preview object changed size");
                let mut value = json!({"commit":commit,"path":path});
                if let Some(pointer) = super::super::lfs::Pointer::parse(&bytes)? {
                    value["lfs"] = serde_json::to_value(&pointer)?;
                    if captured.preserves_object(parts[2])
                        && !captured.pointers().contains(&pointer)
                    {
                        value["preserved"] = json!(true);
                        files.push(value);
                        continue;
                    }
                    let size =
                        usize::try_from(pointer.size).context("plain LFS preview size overflow")?;
                    budget = budget
                        .checked_sub(size)
                        .context("plain preview exceeds its content budget")?;
                    bytes.clear();
                    captured.open_payload(&pointer)?.read_to_end(&mut bytes)?;
                    pointer.verify(bytes.as_slice())?;
                }
                match std::str::from_utf8(&bytes) {
                    Ok(text) if !text.contains('\0') => value["text"] = json!(text),
                    _ => value["binary_bytes"] = json!(bytes.len()),
                }
                files.push(value);
            }
        }
        Ok(files)
    }

    pub(crate) fn session_preview(&self) -> Result<Vec<serde_json::Value>> {
        let manifest = &self.session_manifest;
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
                    let (metadata, log, view) =
                        super::super::privacy_publication::read_snapshot(&self.repo, commit)?;
                    json!({"metadata":metadata,"session":{"log":log,"view":view}})
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
