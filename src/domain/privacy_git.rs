//! Public Git history is built in an object store with no source-object alternates.
//!
//! Private cache keys bind original snapshots, projected bytes, policy, recipients and parents.
//! Only generated trees and commits become publication roots; native Git metadata is not copied.

use super::{
    meta::{self, Meta},
    privacy::PrivacyPolicy,
    privacy_envelope::{PrivacyEnvelope, ViewingRecipient, digest_json},
    privacy_paths::PathAliasStore,
    privacy_publication::{ProjectionReport, PublicationSnapshot},
    redact::Redactor,
    repo::{
        Repo,
        publication::{PublicationPlan, commit_parents},
    },
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Seek, Write},
    path::Path,
};

const VERSION: u32 = 2;
const MAX_REF_LIST: usize = 4 * 1024 * 1024;
const MAX_COMMIT: usize = 1024 * 1024;
const COMMIT_SUFFIX: &str = "author AgentGit Privacy <privacy@agentgit.local> 0 +0000\ncommitter AgentGit Privacy <privacy@agentgit.local> 0 +0000\n\nPrivacy-projected snapshot\n";

mod accepted;
#[cfg(test)]
mod accepted_tests;
mod incremental;
#[cfg(test)]
mod incremental_tests;
mod published;
#[cfg(test)]
mod receipt_tests;
mod state;

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cache {
    version: u32,
    sessions: BTreeMap<String, String>,
    commits: BTreeMap<String, String>,
}

pub struct ProjectedHistory {
    repo: Repo,
    source: PublicationPlan,
    branches: Vec<String>,
    plan: PublicationPlan,
    policy_digest: String,
    additional_policies: Vec<super::privacy::mandatory::MandatoryPolicy>,
    reports: Vec<ProjectionReport>,
    inspection_views: BTreeMap<String, String>,
    accepted_ledger: Option<accepted::Ledger>,
    candidates: BTreeMap<String, accepted::Candidate>,
    #[cfg(test)]
    preparations: usize,
    _lock: fs::File,
}

impl ProjectedHistory {
    /// Prepare complete selected ancestry. The lock keeps cached publication refs stable through
    /// inspection and transport while source verification detects concurrent local settlement.
    pub fn prepare(
        source: &Repo,
        branches: &[String],
        recipient: &ViewingRecipient,
        destination: &str,
    ) -> Result<Self> {
        let redactor = Redactor::try_this_machine()?.with_repository(source.root())?;
        Self::prepare_with_redactor(source, branches, recipient, destination, &redactor)
    }

    fn prepare_with_redactor(
        source: &Repo,
        branches: &[String],
        recipient: &ViewingRecipient,
        destination: &str,
        redactor: &Redactor,
    ) -> Result<Self> {
        Self::prepare_with_rules(
            source,
            branches,
            recipient,
            destination,
            redactor,
            &[],
            None,
        )
    }

    #[cfg(feature = "cli")]
    pub(crate) fn prepare_with_sources(
        source: &Repo,
        branches: &[String],
        recipient: &ViewingRecipient,
        destination: &str,
        additional: &[super::privacy::mandatory::MandatoryPolicy],
        identity: &crate::hub::identity::RemoteIdentity,
    ) -> Result<Self> {
        let redactor = Redactor::try_this_machine()?.with_repository(source.root())?;
        let remote = crate::hub::git::FrozenPublication::advertised_refs_for(
            source,
            destination,
            identity,
        )
        .context(
            "cannot verify accepted publication refs; retry when the destination is reachable",
        )?;
        Self::prepare_with_rules(
            source,
            branches,
            recipient,
            destination,
            &redactor,
            additional,
            Some((identity, &remote)),
        )
    }

    fn prepare_with_rules(
        source: &Repo,
        branches: &[String],
        recipient: &ViewingRecipient,
        destination: &str,
        redactor: &Redactor,
        additional: &[super::privacy::mandatory::MandatoryPolicy],
        bound: Option<(
            &crate::hub::identity::RemoteIdentity,
            &crate::hub::git::RemoteRefs,
        )>,
    ) -> Result<Self> {
        let policy = effective_policy(source, additional)?;
        let source_plan = PublicationPlan::freeze_selected(source, branches)?;
        for head in source_plan.heads() {
            let metadata = super::storage::metadata_local(source.root(), head.oid())?;
            ensure!(
                metadata.is_session_line(),
                "session publication excludes repository file lines"
            );
        }
        let source = source.clone().local_objects_only();
        let parent = source.common_dir()?.join("agit/privacy-publication");
        crate::infra::config::create_state_dir(&parent)?;
        let lock = crate::infra::config::state_file_options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(parent.join("lock"))?;
        fs2::FileExt::lock_exclusive(&lock)?;
        let mut scope_value = json!({"version":VERSION,"destination":destination,"recipient":recipient.fingerprint()?});
        if let Some((identity, _)) = bound {
            scope_value["identity"] = json!(identity);
        }
        let scope = digest_json(&scope_value)?;
        let root = parent.join(scope.trim_start_matches("sha256:"));
        initialize(&root)?;
        let repo = Repo::at(&root).local_objects_only();
        let cache_path = root.join(".git/privacy-cache.json");
        let mut cache: Cache = match fs::read(&cache_path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).context("invalid privacy publication cache")?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Cache {
                version: VERSION,
                ..Default::default()
            },
            Err(error) => return Err(error.into()),
        };
        ensure!(
            cache.version == VERSION,
            "unsupported privacy publication cache"
        );
        let mut incremental = incremental::Index::open(&repo, &scope)?;
        #[cfg(test)]
        let mut preparations = 0;
        let mut parents = BTreeMap::new();
        for oid in source_plan.commit_objects() {
            let bytes = read(&source, &["cat-file", "commit", oid], MAX_COMMIT)?;
            parents.insert(oid.clone(), commit_parents(&bytes)?);
        }
        let order = parent_first(&parents)?;
        let mut accepted_ledger = bound
            .map(|(identity, _)| accepted::Ledger::open(&source, identity))
            .transpose()?;
        if let (Some(ledger), Some((_, remote))) = (&mut accepted_ledger, bound) {
            ledger.reconcile(remote)?;
        }
        let accepted = if let Some((_, remote)) = bound {
            let mut accepted = BTreeSet::new();
            for head in &remote.heads {
                if parents.contains_key(head) {
                    accepted.extend(ancestors(head, &parents)?);
                }
            }
            accepted
        } else {
            accepted_ancestors(&source, destination, &parents)?
        };
        let mut reports = Vec::new();
        let mut inspection_views = BTreeMap::new();
        let mut references = BTreeMap::new();
        let mut candidates = BTreeMap::new();
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
                if let (Some(ledger), Some((_, remote))) = (&accepted_ledger, bound) {
                    ledger.verify_remote_prefix(branch, remote.refs.get(head.name()), &parents)?;
                }
                let mut mapped = BTreeMap::<String, String>::new();
                let mut inherited = BTreeMap::new();
                for oid in order.iter().filter(|oid| reachable.contains(*oid)) {
                    let projected_parents = parents[oid]
                        .iter()
                        .map(|parent| {
                            mapped
                                .get(parent)
                                .cloned()
                                .context("projected parent is unavailable")
                        })
                        .collect::<Result<Vec<_>>>()?;
                    if let Some(entry) = accepted_ledger
                        .as_ref()
                        .map(|ledger| ledger.inherited(branch, oid, &policy))
                        .transpose()?
                        .flatten()
                    {
                        ensure!(
                            entry.source_parents == parents[oid]
                                && entry.public_parents == projected_parents,
                            "{}",
                            accepted::RECOVERY
                        );
                        let retained = parent.join(&entry.store);
                        ensure!(
                            retained.join(".git/config").is_file()
                                && !retained.join(".git/objects/info/alternates").exists(),
                            "{}",
                            accepted::RECOVERY
                        );
                        let published = published::reuse(
                            &Repo::at(retained).local_objects_only(),
                            &repo,
                            &entry.public,
                            &projected_parents,
                        )?
                        .context(accepted::RECOVERY)?;
                        ensure!(
                            published.session_id == entry.public_session
                                && published.policy_digest == entry.policy,
                            "{}",
                            accepted::RECOVERY
                        );
                        aliases.reserve_aliases(&published.inspection)?;
                        inspection_views.insert(published.envelope_oid, published.inspection);
                        reports.push(published.report);
                        if !entry.public_session.is_empty() {
                            let session_key = digest_json(
                                &json!({"branch":branch,"session":entry.source_session}),
                            )?;
                            cache
                                .sessions
                                .insert(session_key, entry.public_session.clone());
                        }
                        if entry.source == entry.public {
                            bind_recovered_session(
                                &source,
                                oid,
                                branch,
                                &entry.public_session,
                                &mut cache,
                            )?;
                        }
                        references.insert(
                            format!("refs/tags/agit-{}", entry.public),
                            entry.public.clone(),
                        );
                        mapped.insert(oid.clone(), entry.public.clone());
                        inherited.insert(oid.clone(), entry.clone());
                        continue;
                    }
                    if accepted.contains(oid)
                        && let Some(published) =
                            published::reuse(&source, &repo, oid, &projected_parents)?
                    {
                        aliases.reserve_aliases(&published.inspection)?;
                        inspection_views.insert(published.envelope_oid, published.inspection);
                        reports.push(published.report);
                        references.insert(format!("refs/tags/agit-{oid}"), oid.clone());
                        mapped.insert(oid.clone(), oid.clone());
                        let metadata = super::storage::metadata_local(source.root(), oid)?;
                        let session_key =
                            digest_json(&json!({"branch":branch,"session":metadata.session}))?;
                        bind_recovered_session(
                            &source,
                            oid,
                            branch,
                            &published.session_id,
                            &mut cache,
                        )?;
                        if !published.session_id.is_empty() {
                            cache.sessions.insert(session_key, published.session_id);
                        }
                        continue;
                    }
                    let dependencies = incremental::dependencies(&source, redactor)?;
                    let preparation_key = digest_json(&json!({
                        "source":oid, "branch":branch, "policy":policy.digest()?,
                        "scope":scope, "parents":projected_parents, "dependencies":dependencies,
                    }))?;
                    if let Some(entry) = incremental.get(&preparation_key)
                        && entry.dependencies.matches(&policy, aliases, branch)?
                    {
                        ensure!(
                            cache.commits.get(&entry.ciphertext_key) == Some(&entry.projected),
                            "cached publication tree or commit differs from the authenticated preparation"
                        );
                        let published =
                            published::reuse(&repo, &repo, &entry.projected, &projected_parents)?
                                .context("cached publication is not a generated snapshot")?;
                        aliases.reserve_aliases(&published.inspection)?;
                        inspection_views.insert(published.envelope_oid, published.inspection);
                        reports.push(published.report);
                        references.insert(
                            format!("refs/tags/agit-{}", entry.projected),
                            entry.projected.clone(),
                        );
                        mapped.insert(oid.clone(), entry.projected.clone());
                        continue;
                    }
                    let has_metadata = !read(
                        &source,
                        &["ls-tree", "-z", "--full-name", oid, "--", meta::FILE],
                        4096,
                    )?
                    .is_empty();
                    let metadata = if has_metadata {
                        super::storage::metadata_local(source.root(), oid)?
                    } else {
                        Meta::new_file_line()
                    };
                    let (log, view) = if metadata.is_file_line() {
                        (String::new(), String::new())
                    } else {
                        let limit = super::privacy_publication::MAX_INPUT_BYTES;
                        super::storage::materialize_pair_local(source.root(), oid, limit, limit)
                            .context(
                                "cannot read session within the privacy preparation input budget",
                            )?
                    };
                    ensure!(
                        metadata.session.is_empty() || !log.is_empty(),
                        "claimed source snapshot is missing session content"
                    );
                    #[cfg(test)]
                    {
                        preparations += 1;
                    }
                    let mut projection = PublicationSnapshot::capture(
                        &source, &log, &view, &metadata,
                    )?
                    .project(&policy, aliases, Some(branch), redactor)?;
                    let session_key =
                        digest_json(&json!({"branch":branch,"session":metadata.session}))?;
                    let public_session = cache
                        .sessions
                        .entry(session_key)
                        .or_insert_with(meta::mint_session_id)
                        .clone();
                    let mut files = projection.prepare_git(&public_session)?;
                    let key = digest_json(&json!({
                        "version":VERSION, "source":oid, "branch":branch, "policy":policy.digest()?,
                        "recipient":recipient.fingerprint()?, "public":projection.public_value(),
                        "private":projection.private_fingerprint()?, "parents":projected_parents,
                    }))?;
                    let cached = cache.commits.get(&key).cloned();
                    let envelope_bytes = if let Some(projected) = &cached {
                        ensure!(valid_oid(projected), "invalid cached publication commit");
                        let bytes = repo
                            .show_result(projected, "privacy/envelope.json")?
                            .context("cached publication has no envelope")?
                            .into_bytes();
                        let envelope = PrivacyEnvelope::parse(&bytes)?;
                        ensure!(
                            envelope.public_projection == projection.public_value()
                                && envelope.policy_digest == policy.digest()?
                                && envelope.snapshot_digest
                                    == digest_json(&projection.public_value())?
                                && envelope.attachments.is_empty(),
                            "cached publication differs from the generated projection"
                        );
                        bytes
                    } else {
                        serde_json::to_vec(&projection.seal(recipient)?)?
                    };
                    let envelope_oid = write_object(&repo, "blob", &envelope_bytes)?;
                    inspection_views.insert(envelope_oid, projection.inspection_text()?);
                    files.insert("privacy/envelope.json".into(), envelope_bytes);
                    let tree = write_tree(&repo, files)?;
                    let body = commit_body(&tree, &projected_parents);
                    let projected = write_object(&repo, "commit", body.as_bytes())?;
                    ensure!(
                        cached.as_ref().is_none_or(|cached| cached == &projected),
                        "cached publication tree or commit differs from the generated snapshot"
                    );
                    cache.commits.insert(key.clone(), projected.clone());
                    if dependencies == incremental::dependencies(&source, redactor)?
                        && projection
                            .dependencies()
                            .matches(&policy, aliases, branch)?
                    {
                        incremental.insert(
                            preparation_key,
                            incremental::Entry {
                                ciphertext_key: key,
                                projected: projected.clone(),
                                dependencies: projection.dependencies().clone(),
                            },
                        );
                    }
                    references.insert(format!("refs/tags/agit-{projected}"), projected.clone());
                    mapped.insert(oid.clone(), projected);
                    reports.push(projection.report().clone());
                }
                references.insert(head.name().into(), mapped[head.oid()].clone());
                if bound.is_some() {
                    let mut mappings = BTreeMap::new();
                    for (oid, public) in &mapped {
                        let old = inherited.get(oid);
                        let entry = if let Some(old) = old {
                            old.clone()
                        } else {
                            let bytes = repo
                                .show_result(public, "privacy/envelope.json")?
                                .context("publication has no envelope")?;
                            let envelope = PrivacyEnvelope::parse(bytes.as_bytes())?;
                            ensure!(
                                envelope.private_payload.wrapped_keys.len() == 1,
                                "repository publication requires one viewing recipient"
                            );
                            let selected = &envelope.private_payload.wrapped_keys[0].recipient;
                            let fingerprint = if selected == recipient.id() {
                                Some(recipient.fingerprint()?)
                            } else {
                                ensure!(
                                    oid == public && accepted.contains(oid),
                                    "only accepted inherited ciphertext can omit a recipient fingerprint"
                                );
                                None
                            };
                            accepted::Mapping {
                                source: oid.clone(),
                                public: public.clone(),
                                source_parents: parents[oid].clone(),
                                public_parents: parents[oid]
                                    .iter()
                                    .map(|oid| mapped[oid].clone())
                                    .collect(),
                                store: scope.trim_start_matches("sha256:").into(),
                                source_session: super::storage::metadata_local(source.root(), oid)
                                    .map(|metadata| metadata.session)
                                    .unwrap_or_default(),
                                public_session: super::storage::metadata_local(
                                    repo.root(),
                                    public,
                                )?
                                .session,
                                policy: envelope.policy_digest,
                                recipient: fingerprint,
                            }
                        };
                        mappings.insert(oid.clone(), entry);
                    }
                    candidates.insert(
                        branch.to_owned(),
                        accepted::Candidate {
                            source_head: head.oid().into(),
                            public_head: mapped[head.oid()].clone(),
                            mappings,
                        },
                    );
                }
            }
            Ok(())
        })?;
        // Install mappings before refs: retries must retain the same randomized ciphertext even
        // if a process stops while publishing the generated references.
        let mut file = tempfile::NamedTempFile::new_in(root.join(".git"))?;
        file.write_all(&serde_json::to_vec(&cache)?)?;
        file.as_file().sync_all()?;
        file.persist(cache_path).map_err(|error| error.error)?;
        incremental.save()?;
        let refs = references
            .iter()
            .map(|(name, oid)| format!("update {name} {oid}\n"))
            .collect::<String>();
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
            policy_digest: policy.digest()?,
            additional_policies: additional.to_vec(),
            reports,
            inspection_views,
            accepted_ledger,
            candidates,
            #[cfg(test)]
            preparations,
            _lock: lock,
        })
    }

    pub fn repo(&self) -> &Repo {
        &self.repo
    }
    pub fn plan(&self) -> &PublicationPlan {
        &self.plan
    }
    pub(crate) fn source_heads(&self) -> &[super::repo::publication::FrozenRef] {
        self.source.heads()
    }
    pub fn reports(&self) -> &[ProjectionReport] {
        &self.reports
    }

    pub fn policy_digest(&self) -> &str {
        &self.policy_digest
    }

    pub(crate) fn prepare_acceptance(&mut self) -> Result<()> {
        if let Some(ledger) = &mut self.accepted_ledger {
            ledger.prepare(&self.candidates)?;
        }
        Ok(())
    }

    pub(crate) fn confirm_acceptance(
        &mut self,
        report: &crate::hub::git::PublicationReport,
    ) -> Result<()> {
        if let Some(ledger) = &mut self.accepted_ledger {
            ledger.confirm(&self.candidates, report)?;
        }
        Ok(())
    }

    pub(crate) fn receipt_binding(&self, branch: &str) -> Result<Option<(&str, &str)>> {
        let candidate = self
            .candidates
            .get(branch)
            .context("publication has no candidate binding")?;
        let entry = candidate
            .mappings
            .get(&candidate.source_head)
            .context("publication has no head binding")?;
        if let Some(recipient) = entry.recipient.as_deref() {
            Ok(Some((&entry.policy, recipient)))
        } else {
            ensure!(
                entry.source == entry.public,
                "new publication has no recipient fingerprint"
            );
            Ok(None)
        }
    }

    pub(crate) fn verify_accepted_policy(&self) -> Result<()> {
        if let Some(ledger) = &self.accepted_ledger {
            for (branch, candidate) in &self.candidates {
                for source in candidate.mappings.keys() {
                    if let Some(entry) = ledger.get(branch, source) {
                        ensure!(
                            entry.source == entry.public || entry.policy == self.policy_digest,
                            "accepted publication policy changed and regenerated history cannot fast-forward; restore the reviewed policy or continue from a fresh clone of the accepted history"
                        );
                    }
                }
            }
        }
        Ok(())
    }

    /// Digest the public projections in publication order without incorporating private layers.
    pub fn public_digest(&self) -> Result<String> {
        let mut snapshots = Vec::with_capacity(self.plan.commit_objects().len());
        for commit in self.plan.commit_objects() {
            let bytes = self
                .repo
                .show_result(commit, "privacy/envelope.json")?
                .context("projected snapshot has no envelope")?;
            let envelope = super::privacy_envelope::PrivacyEnvelope::parse(bytes.as_bytes())?;
            snapshots.push(envelope.public_projection);
        }
        super::privacy_envelope::digest_json(&json!(snapshots))
    }

    pub(crate) fn inspection_views(&self) -> &BTreeMap<String, String> {
        &self.inspection_views
    }

    /// Promotion may move the source checkout and its private publication storage together.
    pub fn relocate(&mut self, source: &Repo) -> Result<()> {
        let name = self
            .repo
            .root()
            .file_name()
            .context("publication store has no name")?;
        let root = source
            .common_dir()?
            .join("agit/privacy-publication")
            .join(name);
        ensure!(
            root.join(".git/config").is_file(),
            "relocated publication storage is unavailable"
        );
        self.repo = Repo::at(root).local_objects_only();
        self.verify_source(source)
    }

    /// Save the actual public contents reviewed by the CLI; the private envelope stays ciphertext.
    pub fn write_preview(
        &self,
        destination: &serde_json::Value,
        source: &Repo,
    ) -> Result<std::path::PathBuf> {
        self.verify_source(source)?;
        let policy = effective_policy(source, &self.additional_policies)?;
        let mut snapshots = Vec::new();
        for commit in self.plan.commit_objects() {
            let envelope = self
                .repo
                .show_result(commit, "privacy/envelope.json")?
                .context("projected snapshot has no envelope")?;
            let envelope = super::privacy_envelope::PrivacyEnvelope::parse(envelope.as_bytes())?;
            snapshots.push(json!({"commit":commit, "snapshot_digest":envelope.snapshot_digest,"policy_digest":envelope.policy_digest,"public":envelope.public_projection}));
        }
        let path = self.repo.root().join(".git/publication-preview.json");
        let public_digest = self.public_digest()?;
        fs::write(
            &path,
            serde_json::to_vec_pretty(
                &json!({"destination":destination,"policy":policy,"plan":self.plan,"public_digest":public_digest,"snapshots":snapshots}),
            )?,
        )?;
        Ok(path)
    }

    pub fn verify_source(&self, source: &Repo) -> Result<()> {
        self.source.verify(source, &self.branches)?;
        ensure!(
            effective_policy(source, &self.additional_policies)?.digest()? == self.policy_digest,
            "privacy policy changed after preview"
        );
        self.plan.verify(&self.repo, &self.branches)
    }
}

/// A retained receipt selects an immutable generated snapshot, independently of current refs.
pub(crate) fn receipt_session_id(
    source: &Repo,
    receipt: &super::privacy_receipt::PublicationReceipt,
) -> Result<String> {
    let (repo, _lock) = receipt_cache(source, receipt)?;
    let commit = read(
        &repo,
        &["cat-file", "commit", &receipt.published],
        MAX_COMMIT,
    )?;
    let parents = commit_parents(&commit)?;
    let snapshot = published::reuse(&repo, &repo, &receipt.published, &parents)?
        .context("retained publication is not a generated snapshot")?;
    ensure!(
        Some(snapshot.policy_digest.as_str()) == receipt.policy_digest.as_deref()
            && meta::is_bare_id(&snapshot.session_id),
        "retained publication policy or session identity is invalid"
    );
    Ok(snapshot.session_id)
}

pub(crate) fn receipt_ancestors(
    source: &Repo,
    receipt: &super::privacy_receipt::PublicationReceipt,
) -> Result<BTreeSet<String>> {
    let (repo, _lock) = receipt_cache(source, receipt)?;
    super::repo::publication::raw_ancestors(&repo, &receipt.published)
}

/// Only authenticated, accepted mappings may translate private points for Hub bookkeeping.
/// Prepared candidates and ambiguous cross-branch projections cannot authorize disclosure.
pub(crate) fn accepted_session_identity(
    source: &Repo,
    branch: Option<&str>,
    commit: &str,
    identity: &crate::hub::identity::RemoteIdentity,
) -> Result<Option<(String, String)>> {
    let ledger = accepted::Ledger::open(source, identity)?;
    Ok(ledger
        .selected(branch, commit)?
        .map(|mapping| (mapping.public_session.clone(), mapping.public.clone())))
}

/// Export/share key lookup follows an authenticated accepted source mapping, never a candidate.
pub(crate) fn accepted_envelope(
    source: &Repo,
    branch: Option<&str>,
    commit: &str,
    identity: &crate::hub::identity::RemoteIdentity,
) -> Result<(String, PrivacyEnvelope)> {
    let parent = source.common_dir()?.join("agit/privacy-publication");
    let lock = crate::infra::config::state_file_options()
        .read(true)
        .open(parent.join("lock"));
    let _lock = match lock {
        Ok(lock) => {
            fs2::FileExt::try_lock_shared(&lock).context("publication state is busy; retry")?;
            Some(lock)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let ledger = accepted::Ledger::open(source, identity)?;
    let mapping = ledger.selected(branch, commit)?;
    let (repo, public) = if let Some(mapping) = mapping {
        (
            Repo::at(parent.join(&mapping.store)).local_objects_only(),
            mapping.public.as_str(),
        )
    } else {
        (source.clone().local_objects_only(), commit)
    };
    let raw = read(&repo, &["cat-file", "blob", &format!("{public}:privacy/envelope.json")], super::privacy_envelope::MAX_ENVELOPE_BYTES)
        .context("default repository encryption requires a verified accepted publication; push this snapshot first or use an explicit export public key")?;
    Ok((public.to_owned(), PrivacyEnvelope::parse(&raw)?))
}

fn receipt_cache(
    source: &Repo,
    receipt: &super::privacy_receipt::PublicationReceipt,
) -> Result<(Repo, fs::File)> {
    receipt.validate()?;
    ensure!(
        receipt.mode.is_encrypted(),
        "ordinary publication does not use generated history"
    );
    ensure!(
        receipt.source != receipt.published,
        "private source commits cannot be notified"
    );
    let parent = source.common_dir()?.join("agit/privacy-publication");
    let lock = crate::infra::config::state_file_options()
        .read(true)
        .open(parent.join("lock"))?;
    fs2::FileExt::try_lock_shared(&lock)
        .context("privacy publication cache is being updated; retry")?;
    let ledger = accepted::Ledger::open(source, &receipt.destination)?;
    let root = if let Some(store) = ledger.receipt_store(receipt)? {
        parent.join(store)
    } else {
        let scope = digest_json(
            &json!({"version":VERSION, "destination":receipt.url, "recipient":receipt.recipient,"identity":receipt.destination}),
        )?;
        let bound = parent.join(scope.trim_start_matches("sha256:"));
        if bound.join(".git/config").is_file() {
            bound
        } else {
            let scope = digest_json(
                &json!({"version":VERSION, "destination":receipt.url, "recipient":receipt.recipient}),
            )?;
            parent.join(scope.trim_start_matches("sha256:"))
        }
    };
    ensure!(
        root.join(".git/config").is_file() && !root.join(".git/objects/info/alternates").exists(),
        "retained privacy publication storage is unavailable or not isolated"
    );
    let repo = Repo::at(root).local_objects_only();
    Ok((repo, lock))
}

fn effective_policy(
    source: &Repo,
    additional: &[super::privacy::mandatory::MandatoryPolicy],
) -> Result<PrivacyPolicy> {
    let mut policy = PrivacyPolicy::load(source)?;
    policy.mandatory.extend_from_slice(additional);
    policy.validate()?;
    Ok(policy)
}

fn bind_recovered_session(
    source: &Repo,
    commit: &str,
    branch: &str,
    public_session: &str,
    cache: &mut Cache,
) -> Result<()> {
    if let Some(recovered) = super::privacy_recovery::RecoveredSnapshot::load(source, commit)? {
        let key = digest_json(&json!({"branch":branch,"session":recovered.metadata.session}))?;
        cache.sessions.insert(key, public_session.into());
    }
    Ok(())
}

fn commit_body(tree: &str, parents: &[String]) -> String {
    let mut body = format!("tree {tree}\n");
    for parent in parents {
        body.push_str(&format!("parent {parent}\n"));
    }
    body.push_str(COMMIT_SUFFIX);
    body
}

fn accepted_ancestors(
    source: &Repo,
    destination: &str,
    parents: &BTreeMap<String, Vec<String>>,
) -> Result<BTreeSet<String>> {
    let mut accepted = BTreeSet::new();
    if source.remote_url().as_deref() == Some(destination) {
        let refs = read(
            source,
            &[
                "for-each-ref",
                "--format=%(objectname)",
                "refs/remotes/origin/",
            ],
            MAX_REF_LIST,
        )?;
        for oid in std::str::from_utf8(&refs)?.lines() {
            if parents.contains_key(oid) {
                accepted.extend(ancestors(oid, parents)?);
            }
        }
    }
    Ok(accepted)
}

fn initialize(root: &Path) -> Result<()> {
    if root.exists() {
        ensure!(
            root.join(".git/config").is_file()
                && !root.join(".git/objects/info/alternates").exists(),
            "privacy publication storage is not isolated"
        );
        return Ok(());
    }
    let parent = root
        .parent()
        .context("privacy publication storage has no parent")?;
    let staging = tempfile::tempdir_in(parent)?;
    for directory in [
        "objects/info",
        "objects/pack",
        "refs/heads",
        "refs/tags",
        "hooks",
    ] {
        fs::create_dir_all(staging.path().join(".git").join(directory))?;
    }
    fs::write(
        staging.path().join(".git/config"),
        "[core]\nrepositoryformatversion = 0\nbare = false\nlogallrefupdates = false\nhooksPath =\n",
    )?;
    fs::write(staging.path().join(".git/HEAD"), "ref: refs/heads/main\n")?;
    fs::rename(staging.path(), root)?;
    Ok(())
}

fn valid_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64) && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn read(repo: &Repo, args: &[&str], limit: usize) -> Result<Vec<u8>> {
    let output = repo.inspection_output(args, limit)?;
    ensure!(
        output.status.success() && output.stderr.is_empty(),
        "privacy source object is unavailable"
    );
    Ok(output.stdout)
}

fn input(repo: &Repo, args: &[&str], bytes: &[u8]) -> Result<String> {
    let mut file = tempfile::tempfile()?;
    file.write_all(bytes)?;
    file.rewind()?;
    repo.git_with_stdin_file(args, file)
}

fn write_object(repo: &Repo, kind: &str, bytes: &[u8]) -> Result<String> {
    let oid = input(repo, &["hash-object", "-t", kind, "-w", "--stdin"], bytes)?;
    ensure!(
        valid_oid(&oid),
        "Git returned an invalid projected object ID"
    );
    Ok(oid)
}

#[derive(Default)]
struct Tree {
    files: BTreeMap<String, Vec<u8>>,
    directories: BTreeMap<String, Tree>,
}

fn write_tree(repo: &Repo, files: BTreeMap<String, Vec<u8>>) -> Result<String> {
    let mut tree = Tree::default();
    for (path, bytes) in files {
        let mut parts = path.split('/').peekable();
        let mut node = &mut tree;
        while let Some(part) = parts.next() {
            ensure!(
                !matches!(part, "" | "." | ".." | ".git") && !part.chars().any(char::is_control),
                "invalid projected tree path"
            );
            if parts.peek().is_some() {
                node = node.directories.entry(part.into()).or_default();
            } else {
                node.files.insert(part.into(), bytes);
                break;
            }
        }
    }
    fn emit(repo: &Repo, tree: Tree) -> Result<String> {
        let mut records = Vec::new();
        for (name, bytes) in tree.files {
            records.extend(
                format!(
                    "100644 blob {}\t{name}\0",
                    write_object(repo, "blob", &bytes)?
                )
                .as_bytes(),
            );
        }
        for (name, subtree) in tree.directories {
            records.extend(format!("040000 tree {}\t{name}\0", emit(repo, subtree)?).as_bytes());
        }
        input(repo, &["mktree", "-z"], &records)
    }
    emit(repo, tree)
}

fn ancestors(head: &str, parents: &BTreeMap<String, Vec<String>>) -> Result<BTreeSet<String>> {
    let mut found = BTreeSet::new();
    let mut pending = vec![head.to_owned()];
    while let Some(oid) = pending.pop() {
        if found.insert(oid.clone()) {
            pending.extend(
                parents
                    .get(&oid)
                    .context("source parent is outside the frozen plan")?
                    .iter()
                    .cloned(),
            );
        }
    }
    Ok(found)
}

fn parent_first(parents: &BTreeMap<String, Vec<String>>) -> Result<Vec<String>> {
    let mut pending = parents.keys().cloned().collect::<BTreeSet<_>>();
    let mut done = BTreeSet::new();
    let mut order = Vec::new();
    while !pending.is_empty() {
        let ready = pending
            .iter()
            .filter(|oid| parents[*oid].iter().all(|parent| done.contains(parent)))
            .cloned()
            .collect::<Vec<_>>();
        ensure!(
            !ready.is_empty(),
            "source history has an unavailable or cyclic parent"
        );
        for oid in ready {
            pending.remove(&oid);
            done.insert(oid.clone());
            order.push(oid);
        }
    }
    Ok(order)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{privacy_envelope::PrivacyEnvelope, storage, transcript};
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use crypto_box::SecretKey;
    use std::process::Command;

    #[test]
    fn projected_history_has_no_plaintext_ancestry_and_reuses_incremental_objects() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let source = Repo::init(&root.join("source")).unwrap();
        let workspace = source.root().canonicalize().unwrap();
        source
            .git(&["config", "user.name", "Private Author"])
            .unwrap();
        source
            .git(&["config", "user.email", "private-author@example.invalid"])
            .unwrap();
        meta::write(source.root(), &Meta::new_file_line()).unwrap();
        let readme = format!(
            "Allowed documentation for {}",
            workspace.join("README.md").display()
        );
        fs::write(source.root().join("README.md"), &readme).unwrap();
        fs::write(source.root().join("private-note.txt"), "EXCLUDED_OLD_FILE").unwrap();
        source.git(&["add", "."]).unwrap();
        source
            .git(&["commit", "-m", "PRIVATE_COMMIT_MESSAGE"])
            .unwrap();
        source.git(&["checkout", "-b", "work"]).unwrap();
        let session = format!("agit-{}", "a".repeat(40));
        let raw = [
            json!({"type":"user","message":{"role":"user","content":"Public request"}}),
            json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"private-read","name":"Read","input":{"file_path":workspace.join("private-note.txt")}}]}}),
            json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"private-read","content":"EXCLUDED_TOOL_BODY"}]}}),
        ].into_iter().map(|record| format!("{record}\n")).collect::<String>();
        let log = transcript::wrap_lines(&raw, "claude-code", &session);
        storage::write_snapshot(source.root(), &log, &log).unwrap();
        let mut metadata = Meta::new(
            session.clone(),
            "claude-code".into(),
            workspace.display().to_string(),
        );
        metadata.cwd_state = Some(meta::CwdState {
            origin: Some("https://private-user:PRIVATE_ORIGIN_TOKEN@example.invalid/team/PROJECT_MARKER.git?token=PRIVATE_QUERY".into()),
            head: Some("PRIVATE_CODE_HEAD".into()), branch: Some("PRIVATE_CODE_BRANCH".into()),
            worktree: meta::WorktreeStatus::Dirty, staged: 1, unstaged: 2, untracked: 3, conflicted: 0,
            status_digest: Some("PRIVATE_STATUS_DIGEST".into()),
        });
        meta::write(source.root(), &metadata).unwrap();
        source.git(&["add", "."]).unwrap();
        source
            .git(&["commit", "-m", "PRIVATE_SESSION_MESSAGE"])
            .unwrap();
        let source_head = source.git(&["rev-parse", "HEAD"]).unwrap();
        PrivacyPolicy {
            workspace: Some(workspace.clone()),
            metadata: super::super::privacy::MetadataMode::Project,
            replacements: vec![super::super::privacy::ReplacementRule {
                pattern: "PROJECT_MARKER".into(),
                replacement: "public-project".into(),
                regex: false,
            }],
            ..Default::default()
        }
        .save(&source)
        .unwrap();
        let key = SecretKey::from([21; 32]);
        let recipient = ViewingRecipient::from_base64(
            "viewer".into(),
            &STANDARD.encode(key.public_key().as_bytes()),
        )
        .unwrap();
        let redactor = Redactor::new(Default::default());
        let branches = vec!["work".into()];
        let prepare = || {
            ProjectedHistory::prepare_with_redactor(
                &source,
                &branches,
                &recipient,
                "https://hub.invalid/alice/app",
                &redactor,
            )
            .unwrap()
        };
        let projected = prepare();
        assert_eq!(projected.preparations, 2);
        projected.verify_source(&source).unwrap();
        assert_eq!(
            projected
                .plan
                .heads()
                .iter()
                .map(|head| head.name())
                .collect::<Vec<_>>(),
            ["refs/heads/work"]
        );
        let head = projected
            .repo
            .git(&["rev-parse", "refs/heads/work"])
            .unwrap();
        let initial = projected.plan.clone();
        assert!(
            projected
                .repo
                .git(&["cat-file", "-e", &source_head])
                .is_err()
        );
        let objects = projected
            .repo
            .git(&["rev-list", "--objects", "--all"])
            .unwrap();
        for line in objects.lines() {
            let oid = line.split_whitespace().next().unwrap();
            let kind = projected.repo.git(&["cat-file", "-t", oid]).unwrap();
            let body = read(&projected.repo, &["cat-file", &kind, oid], 8 * 1024 * 1024).unwrap();
            let body = String::from_utf8_lossy(&body);
            for forbidden in [
                "private-workspace",
                "private-author",
                "PRIVATE_COMMIT_MESSAGE",
                "PRIVATE_SESSION_MESSAGE",
                "EXCLUDED_OLD_FILE",
                "EXCLUDED_TOOL_BODY",
                "private-note.txt",
                "PRIVATE_ORIGIN_TOKEN",
                "PRIVATE_QUERY",
                "PRIVATE_CODE_HEAD",
                "PRIVATE_CODE_BRANCH",
                "PRIVATE_STATUS_DIGEST",
                "PROJECT_MARKER",
            ] {
                assert!(
                    !body.contains(forbidden),
                    "public object contains {forbidden}"
                );
            }
        }
        assert!(
            projected
                .repo
                .show_result(&head, "README.md")
                .unwrap()
                .is_none()
        );
        let envelope = projected
            .repo
            .show_result(&head, "privacy/envelope.json")
            .unwrap()
            .unwrap();
        let envelope = PrivacyEnvelope::parse(envelope.as_bytes()).unwrap();
        let public_metadata = &envelope.public_projection["metadata"];
        assert_eq!(
            public_metadata["privacy"]["origin"],
            "https://example.invalid/team/public-project.git"
        );
        assert_eq!(public_metadata["privacy"]["workspace"], "<workspace>");
        assert_eq!(public_metadata["privacy"]["worktree"], "dirty");
        assert_eq!(public_metadata["privacy"]["staged"], 1);
        assert!(public_metadata.get("cwd_state").is_none());
        let git_metadata: serde_json::Value = serde_json::from_str(
            &projected
                .repo
                .show_result(&head, meta::FILE)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(&git_metadata, public_metadata);
        let layer = envelope.open_layer(&key).unwrap();
        assert_eq!(layer.metadata, serde_json::to_value(&metadata).unwrap());
        assert_eq!(layer.session_bytes().unwrap().0.as_str(), log);
        let private = serde_json::to_value(&layer).unwrap();
        assert!(private.get("attachments").is_none());
        assert!(
            !serde_json::to_string(&private)
                .unwrap()
                .contains("Allowed documentation")
        );
        assert!(
            !layer
                .path_aliases
                .values()
                .any(|path| path.ends_with("README.md"))
        );
        assert_eq!(source.git(&["rev-parse", "HEAD"]).unwrap(), source_head);
        drop(projected);
        let repeated = prepare();
        assert_eq!(repeated.plan, initial);
        assert_eq!(
            repeated.preparations, 0,
            "unchanged source snapshots reuse checked preparations"
        );
        let next = format!("{log}{}", transcript::wrap_lines(&json!({"type":"user","message":{"role":"user","content":"Next request /outside/new-file"}}).to_string(), "claude-code", &session));
        storage::write_snapshot(source.root(), &next, &next).unwrap();
        source.git(&["add", "."]).unwrap();
        source
            .git(&["commit", "-m", "Next private snapshot"])
            .unwrap();
        assert!(repeated.verify_source(&source).is_err());
        drop(repeated);
        let increment = prepare();
        assert_eq!(
            increment.preparations, 1,
            "only the new source snapshot is projected"
        );
        let next_head = increment
            .repo
            .git(&["rev-parse", "refs/heads/work"])
            .unwrap();
        assert_ne!(next_head, head);
        increment
            .repo
            .git(&["merge-base", "--is-ancestor", &head, &next_head])
            .unwrap();
        let remote = root.join("published.git");
        let cloned = Command::new("git")
            .args([
                "clone",
                "--bare",
                "--no-local",
                increment.repo.root().to_str().unwrap(),
                remote.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            cloned.status.success(),
            "{}",
            String::from_utf8_lossy(&cloned.stderr)
        );
        let cloned_path = root.join("new-device");
        let cloned = Command::new("git")
            .args([
                "clone",
                "--no-local",
                "--branch",
                "work",
                remote.to_str().unwrap(),
                cloned_path.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            cloned.status.success(),
            "{}",
            String::from_utf8_lossy(&cloned.stderr)
        );
        let cloned = Repo::at(&cloned_path);
        cloned.git(&["config", "user.name", "New Device"]).unwrap();
        cloned
            .git(&["config", "user.email", "new-device@example.invalid"])
            .unwrap();
        let prepare_cloned = || {
            ProjectedHistory::prepare_with_redactor(
                &cloned,
                &branches,
                &recipient,
                remote.to_str().unwrap(),
                &redactor,
            )
            .unwrap()
        };
        let unchanged = prepare_cloned();
        assert_eq!(
            unchanged.repo.git(&["rev-parse", "work"]).unwrap(),
            next_head
        );
        assert_eq!(
            unchanged.inspection_views.len(),
            unchanged.plan.commit_objects().len()
        );
        drop(unchanged);

        let continued = format!(
            "{next}{}",
            transcript::wrap_lines(
                &json!({"type":"user","message":{"role":"user","content":"New device request"}})
                    .to_string(),
                "claude-code",
                &session
            )
        );
        storage::write_snapshot(cloned.root(), &continued, &continued).unwrap();
        meta::write(
            cloned.root(),
            &Meta::new(
                session.clone(),
                "claude-code".into(),
                cloned_path.display().to_string(),
            ),
        )
        .unwrap();
        fs::remove_file(cloned.root().join("privacy/envelope.json")).unwrap();
        cloned.git(&["add", "."]).unwrap();
        cloned
            .git(&["commit", "-m", "Private continuation"])
            .unwrap();
        let continuation = prepare_cloned();
        let continued_head = continuation.repo.git(&["rev-parse", "work"]).unwrap();
        assert_eq!(
            continuation.repo.git(&["rev-parse", "work^"]).unwrap(),
            next_head
        );
        let pushed = Command::new("git")
            .args([
                "-C",
                continuation.repo.root().to_str().unwrap(),
                "push",
                remote.to_str().unwrap(),
                "work:work",
            ])
            .output()
            .unwrap();
        assert!(
            pushed.status.success(),
            "{}",
            String::from_utf8_lossy(&pushed.stderr)
        );
        let envelope = continuation
            .repo
            .show_result(&continued_head, "privacy/envelope.json")
            .unwrap()
            .unwrap();
        assert_eq!(
            PrivacyEnvelope::parse(envelope.as_bytes())
                .unwrap()
                .open_layer(&key)
                .unwrap()
                .session_bytes()
                .unwrap()
                .0
                .as_str(),
            continued
        );
        drop(continuation);

        cloned.git(&["read-tree", &next_head]).unwrap();
        fs::write(
            cloned.root().join("private-copy.txt"),
            "UNDECLARED_PRIVATE_COPY",
        )
        .unwrap();
        cloned.git(&["add", "private-copy.txt"]).unwrap();
        let tree = cloned.git(&["write-tree"]).unwrap();
        let original_parents = commit_parents(
            &read(&cloned, &["cat-file", "commit", &next_head], MAX_COMMIT).unwrap(),
        )
        .unwrap();
        let forged = write_object(
            &cloned,
            "commit",
            commit_body(&tree, &original_parents).as_bytes(),
        )
        .unwrap();
        cloned
            .git(&["update-ref", "refs/heads/work", &forged])
            .unwrap();
        cloned
            .git(&["update-ref", "refs/remotes/origin/work", &forged])
            .unwrap();
        let error = ProjectedHistory::prepare_with_redactor(
            &cloned,
            &branches,
            &recipient,
            remote.to_str().unwrap(),
            &redactor,
        )
        .err()
        .expect("an envelope cannot authorize extra plaintext blobs");
        assert!(
            error.to_string().contains("declared public tree"),
            "{error:#}"
        );
        let cached_repo = increment.repo.clone();
        let cache_path = cached_repo.root().join(".git/privacy-cache.json");
        let original = read(
            &cached_repo,
            &["cat-file", "commit", &next_head],
            MAX_COMMIT,
        )
        .unwrap();
        let mut forged = original;
        forged.extend_from_slice(b"PRIVATE_CACHE_INJECTION\n");
        let forged = write_object(&cached_repo, "commit", &forged).unwrap();
        let mut cache: Cache = serde_json::from_slice(&fs::read(&cache_path).unwrap()).unwrap();
        for commit in cache.commits.values_mut() {
            if commit == &next_head {
                *commit = forged.clone();
            }
        }
        fs::write(cache_path, serde_json::to_vec(&cache).unwrap()).unwrap();
        drop(increment);
        let error = ProjectedHistory::prepare_with_redactor(
            &source,
            &branches,
            &recipient,
            "https://hub.invalid/alice/app",
            &redactor,
        )
        .err()
        .expect("a cached commit must match the generated tree and metadata");
        assert!(
            error
                .to_string()
                .contains("cached publication tree or commit")
        );
    }
}
