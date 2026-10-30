//! Accepted source mappings outlive recipient-specific preparation caches and build IDs.

use super::*;
use crate::hub::{
    git::{PublicationReport, PublicationStatus, RemoteRefs},
    identity::RemoteIdentity,
};
use zeroize::Zeroizing;

pub(super) use super::InconsistentAcceptedMapping as Inconsistent;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Mapping {
    pub source: String,
    pub public: String,
    pub source_parents: Vec<String>,
    pub public_parents: Vec<String>,
    pub store: String,
    pub source_session: String,
    pub public_session: String,
    pub policy: String,
    /// Cloned accepted ciphertext can outlive access to its public-key fingerprint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipient: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub plain: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub delivered: bool,
}

impl Mapping {
    pub fn object_source(&self, source: &Repo, parent: &Path, branch: &str) -> Result<Repo> {
        let source = source.clone().local_objects_only();
        // An identity mapping refers to the public source graph. Its bytes do not depend
        // on a separate projection store being retained.
        if self.source == self.public {
            return Ok(source);
        }
        let cached = Repo::at(parent.join(&self.store)).local_objects_only();
        if cached.root().join(".git/config").is_file()
            && !cached.root().join(".git/objects/info/alternates").exists()
            && cached
                .git_opt(&["cat-file", "-e", &format!("{}^{{commit}}", self.public)])
                .is_some()
        {
            return Ok(cached);
        }
        if source
            .git_opt(&["cat-file", "-e", &format!("{}^{{commit}}", self.public)])
            .is_some()
        {
            return Ok(source);
        }
        Err(MissingAcceptedObjects {
            branch: branch.into(),
            published: self.public.clone(),
        }
        .into())
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Candidate {
    pub source_head: String,
    pub public_head: String,
    pub mappings: BTreeMap<String, Mapping>,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Branch {
    accepted: BTreeMap<String, Mapping>,
    pending: Vec<Candidate>,
}

pub(super) struct Ledger {
    directory: std::path::PathBuf,
    scope: String,
    branches: BTreeMap<String, Branch>,
    withheld: BTreeSet<String>,
}

impl Ledger {
    pub fn open(source: &Repo, identity: &RemoteIdentity) -> Result<Self> {
        ensure!(
            RemoteIdentity::new(&identity.hub, &identity.agent_id)? == *identity,
            "invalid accepted publication identity"
        );
        let scope = digest_json(&json!({"version":1, "destination":identity}))?;
        let directory = source
            .common_dir()?
            .join("agit/privacy-accepted")
            .join(scope.trim_start_matches("sha256:"));
        let branches =
            if let Some(bytes) = super::state::load(&directory, &scope).context(Inconsistent)? {
                let (version, branches): (u32, BTreeMap<String, Branch>) =
                    serde_json::from_slice(&bytes).context(Inconsistent)?;
                ensure!(version == 1, Inconsistent);
                branches
            } else {
                BTreeMap::new()
            };
        // Kept beside the ledger rather than in it, so a release that predates it still reads the
        // ledger.
        let withheld =
            match super::state::load(&directory.join("withheld"), &scope).context(Inconsistent)? {
                Some(bytes) => {
                    let (version, refs): (u32, BTreeSet<String>) =
                        serde_json::from_slice(&bytes).context(Inconsistent)?;
                    ensure!(version == 1, Inconsistent);
                    refs
                }
                None => BTreeSet::new(),
            };
        let ledger = Self {
            directory,
            scope,
            branches,
            withheld,
        };
        ledger.validate()?;
        Ok(ledger)
    }

    /// Destination refs this checkout no longer publishes because the Hub refused them as
    /// withdrawn. A plan leaves them out and a missing one is not owed, until [`Ledger::release`]
    /// takes it back; otherwise every later push would plan them again and meet the same refusal.
    /// The record is local and never sent.
    pub fn withheld(&self) -> &BTreeSet<String> {
        &self.withheld
    }

    pub fn withhold(&mut self, refs: &[String]) -> Result<()> {
        for name in refs {
            ensure!(valid_ref_name(name), "invalid withheld publication ref");
        }
        self.withheld.extend(refs.iter().cloned());
        self.save_withheld()
    }

    /// Plans these refs in this publication only, without changing the record: a ref the person
    /// names may have been restored, but only the Hub accepting it again says so, and then the
    /// caller releases it.
    pub fn ask_again(&mut self, refs: &BTreeSet<String>) {
        self.withheld.retain(|name| !refs.contains(name));
    }

    pub fn release(&mut self, refs: &[String]) -> Result<()> {
        for name in refs {
            self.withheld.remove(name);
        }
        self.save_withheld()
    }

    fn save_withheld(&self) -> Result<()> {
        let bytes = Zeroizing::new(serde_json::to_vec(&(1, &self.withheld))?);
        super::state::save(&self.directory.join("withheld"), &self.scope, &bytes)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.withheld.iter().all(|name| valid_ref_name(name)),
            Inconsistent
        );
        for (branch, state) in &self.branches {
            crate::domain::repo::valid_branch_name(branch)?;
            validate_mappings(&state.accepted, None)?;
            for candidate in &state.pending {
                validate_mappings(&candidate.mappings, Some(&state.accepted))?;
                ensure!(
                    candidate
                        .mappings
                        .get(&candidate.source_head)
                        .is_some_and(|entry| entry.public == candidate.public_head),
                    Inconsistent
                );
            }
        }
        Ok(())
    }

    pub fn get(&self, branch: &str, source: &str) -> Option<&Mapping> {
        self.branches.get(branch)?.accepted.get(source)
    }

    /// Sources these branches have accepted publications for.
    pub fn accepted_sources(&self, branches: &[String]) -> BTreeSet<String> {
        branches
            .iter()
            .filter_map(|branch| self.branches.get(branch))
            .flat_map(|state| state.accepted.keys().cloned())
            .collect()
    }

    /// The mappings this branch has accepted.
    pub fn accepted(&self, branch: &str) -> impl Iterator<Item = &Mapping> {
        self.branches
            .get(branch)
            .into_iter()
            .flat_map(|state| state.accepted.values())
    }

    pub fn has_accepted_branch(&self, branch: &str) -> bool {
        self.branches
            .get(branch)
            .is_some_and(|state| !state.accepted.is_empty())
    }

    pub fn selected(&self, branch: Option<&str>, source: &str) -> Result<Option<&Mapping>> {
        if let Some(branch) = branch {
            return Ok(self.get(branch, source));
        }
        let mut matches = self
            .branches
            .values()
            .filter_map(|branch| branch.accepted.get(source));
        let selected = matches.next();
        if let Some(selected) = selected {
            ensure!(
                matches.all(|entry| entry.public == selected.public
                    && compatible_recipient(entry, selected)),
                "this source has different accepted publications on multiple branches; select an explicit branch"
            );
        }
        Ok(selected)
    }

    pub fn inherited_immutable(&self, branch: &str, source: &str) -> Result<Option<&Mapping>> {
        if let Some(entry) = self.get(branch, source) {
            return Ok(Some(entry));
        }
        let mut entries = self
            .branches
            .values()
            .filter_map(|state| state.accepted.get(source));
        let selected = entries.next();
        if let Some(selected) = selected {
            ensure!(
                entries.all(|entry| entry.public == selected.public
                    && entry.public_parents == selected.public_parents
                    && entry.source_parents == selected.source_parents
                    && entry.source_session == selected.source_session
                    && entry.public_session == selected.public_session
                    && compatible_recipient(entry, selected)),
                "this source has ambiguous accepted publications; continue from an explicit public commit"
            );
        }
        Ok(selected)
    }

    pub fn receipt_store(
        &self,
        receipt: &crate::domain::privacy_receipt::PublicationReceipt,
    ) -> Result<Option<&str>> {
        let Some(branch) = self.branches.get(&receipt.branch) else {
            return Ok(None);
        };
        let selected = branch
            .accepted
            .get(&receipt.source)
            .into_iter()
            .chain(
                branch
                    .pending
                    .iter()
                    .filter_map(|candidate| candidate.mappings.get(&receipt.source)),
            )
            .find(|entry| entry.public == receipt.published);
        let Some(entry) = selected else {
            return Ok(None);
        };
        ensure!(
            (receipt.mode == crate::domain::privacy_receipt::PublicationMode::SessionPlain
                && entry.plain
                && entry.recipient.is_none()
                && (entry.delivered || entry.source_session == entry.public_session)
                && Some(&entry.public_session) == receipt.projected_session_id.as_ref())
                || receipt.mode.is_encrypted()
                    && ((Some(entry.policy.as_str()) == receipt.policy_digest.as_deref()
                        && entry.recipient.as_deref() == receipt.recipient.as_deref())
                        || (receipt.session_snapshot.is_some() && entry.source == entry.public)),
            "retained publication scope differs from its authenticated mapping"
        );
        Ok(Some(&entry.store))
    }

    pub fn reconcile(&mut self, remote: &RemoteRefs) -> Result<()> {
        for (name, state) in &mut self.branches {
            let Some(head) = remote.refs.get(&format!("refs/heads/{name}")) else {
                continue;
            };
            let candidates = state.pending.clone();
            for candidate in candidates {
                if let Some(mapping) = candidate
                    .mappings
                    .values()
                    .find(|entry| &entry.public == head)
                {
                    promote(state, &candidate, &mapping.source)?;
                }
            }
        }
        self.save()
    }

    /// A candidate may carry a commit the branch already accepted only as that accepted mapping,
    /// and may leave a mapping's ancestry out only when the mapping is an identity mapping of a
    /// commit in `held`, which the destination's advertisement reaches. Validation admits that
    /// shape alone; without the held check an identity mapping of a local-only commit would carry
    /// its unpublished ancestry past the ledger.
    pub fn prepare(
        &mut self,
        candidates: &BTreeMap<String, Candidate>,
        held: &BTreeSet<String>,
    ) -> Result<()> {
        for (branch, candidate) in candidates {
            let state = self.branches.entry(branch.clone()).or_default();
            for (source, entry) in &candidate.mappings {
                if let Some(old) = state.accepted.get(source) {
                    ensure!(same_mapping(old, entry), Inconsistent);
                    continue;
                }
                let continues = entry.source_parents.iter().all(|parent| {
                    candidate.mappings.contains_key(parent) || state.accepted.contains_key(parent)
                });
                ensure!(continues || held.contains(source), Inconsistent);
            }
            if !state.pending.iter().any(|old| {
                old.source_head == candidate.source_head && old.public_head == candidate.public_head
            }) {
                state.pending.push(candidate.clone());
            }
        }
        self.validate()?;
        self.save()
    }

    pub fn confirm(
        &mut self,
        candidates: &BTreeMap<String, Candidate>,
        report: &PublicationReport,
    ) -> Result<()> {
        for (branch, candidate) in candidates {
            let observed = report
                .heads
                .as_ref()
                .into_iter()
                .flat_map(|phase| &phase.attempts)
                .flat_map(|attempt| attempt.refs.iter().map(move |item| (attempt.ok(), item)))
                .rfind(|(_, item)| item.reference.name() == format!("refs/heads/{branch}"));
            if observed.is_some_and(|(complete, item)| {
                complete
                    && item.reference.oid() == candidate.public_head
                    && matches!(
                        item.status,
                        PublicationStatus::Updated | PublicationStatus::UpToDate
                    )
            }) {
                let state = self.branches.entry(branch.clone()).or_default();
                promote(state, candidate, &candidate.source_head)?;
            }
        }
        self.save()
    }

    fn save(&self) -> Result<()> {
        let bytes = Zeroizing::new(serde_json::to_vec(&(1, &self.branches))?);
        super::state::save(&self.directory, &self.scope, &bytes)
    }
}

/// A candidate that continues accepted history carries only what it adds and its boundary, so the
/// walk stops where the candidate ends: there validation admitted only history this branch already
/// accepted or the ancestry of an identity mapping, and neither changes state.
fn promote(state: &mut Branch, candidate: &Candidate, source: &str) -> Result<()> {
    let mut pending = vec![(source.to_owned(), false)];
    let mut visited = BTreeSet::new();
    while let Some((source, below_identity)) = pending.pop() {
        if !visited.insert(source.clone()) {
            continue;
        }
        let Some(entry) = candidate.mappings.get(&source) else {
            ensure!(
                below_identity || state.accepted.contains_key(&source),
                Inconsistent
            );
            continue;
        };
        if let Some(old) = state.accepted.get(&source) {
            ensure!(same_mapping(old, entry), Inconsistent);
        } else {
            state.accepted.insert(source.clone(), entry.clone());
        }
        let identity = is_identity(entry);
        pending.extend(
            entry
                .source_parents
                .iter()
                .map(|parent| (parent.clone(), identity)),
        );
    }
    state.pending.retain(|pending| {
        pending.source_head != source || pending.public_head != candidate.public_head
    });
    Ok(())
}

/// An accepted commit published under its own id with its own parents. Its public commit is its
/// source commit, so the destination holds its ancestry under the same ids and no mapping of an
/// ancestor can differ from it.
fn valid_ref_name(name: &str) -> bool {
    (name.starts_with("refs/heads/") || name.starts_with("refs/tags/"))
        && name.len() <= 1024
        && !name.contains(['\0', '\n'])
}

pub(super) fn is_identity(entry: &Mapping) -> bool {
    entry.source == entry.public
        && entry.source_parents == entry.public_parents
        && entry.recipient.is_none()
        && !entry.plain
}

/// Each mapping's public parents are the mappings of its source parents. A parent may come from
/// `accepted` when the mappings continue it; one found nowhere is admitted only below an identity
/// mapping, whose ancestry is identity. Any other mapping without its parents is refused, so a
/// candidate cannot attach a new mapping to an old commit without the ancestry check.
fn validate_mappings(
    mappings: &BTreeMap<String, Mapping>,
    accepted: Option<&BTreeMap<String, Mapping>>,
) -> Result<()> {
    for (source, entry) in mappings {
        ensure!(
            source == &entry.source
                && valid_oid(source)
                && valid_oid(&entry.public)
                && entry.source_parents.len() == entry.public_parents.len()
                && entry.store.len() == 64
                && entry.store.bytes().all(|byte| byte.is_ascii_hexdigit()),
            Inconsistent
        );
        for (source_parent, public_parent) in entry.source_parents.iter().zip(&entry.public_parents)
        {
            let parent = mappings
                .get(source_parent)
                .or_else(|| accepted.and_then(|accepted| accepted.get(source_parent)));
            match parent {
                Some(parent) => ensure!(&parent.public == public_parent, Inconsistent),
                None => ensure!(is_identity(entry), Inconsistent),
            }
        }
        super::super::privacy_envelope::validate_digest(&entry.policy)?;
        if entry.plain {
            ensure!(
                entry.recipient.is_none()
                    && (entry.delivered || entry.source_session == entry.public_session),
                "plain mapping cannot change session authority or carry an encryption recipient"
            );
        }
        if entry.delivered {
            ensure!(
                entry.plain
                    && (entry.source_session.is_empty() && entry.public_session.is_empty()
                        || meta::is_bare_id(&entry.source_session)
                            && meta::is_bare_id(&entry.public_session)
                            && entry.source_session != entry.public_session),
                "delivered mapping must retain independent plain session identities"
            );
        }
        if let Some(recipient) = &entry.recipient {
            super::super::privacy_envelope::validate_digest(recipient)?;
        } else {
            ensure!(
                entry.source == entry.public || entry.plain,
                "a newly encrypted mapping requires its publishing recipient"
            );
        }
    }
    Ok(())
}

/// An accepted source keeps its public commit, parents and session for good.
fn same_mapping(old: &Mapping, entry: &Mapping) -> bool {
    old.public == entry.public
        && old.public_parents == entry.public_parents
        && old.source_parents == entry.source_parents
        && old.public_session == entry.public_session
        && compatible_recipient(old, entry)
}

fn compatible_recipient(left: &Mapping, right: &Mapping) -> bool {
    if left.plain != right.plain || left.delivered != right.delivered {
        return false;
    }
    match (&left.recipient, &right.recipient) {
        (Some(left), Some(right)) => left == right,
        _ => true,
    }
}
