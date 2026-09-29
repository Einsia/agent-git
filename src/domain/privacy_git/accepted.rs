//! Accepted source mappings outlive recipient-specific preparation caches and build IDs.

use super::*;
use crate::hub::{
    git::{PublicationReport, PublicationStatus, RemoteRefs},
    identity::RemoteIdentity,
};
use zeroize::Zeroizing;

pub(super) const RECOVERY: &str = "accepted publication mapping is unavailable or inconsistent; restore this checkout's privacy state or clone the accepted remote into a fresh checkout before continuing";

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
            if let Some(bytes) = super::state::load(&directory, &scope).context(RECOVERY)? {
                let (version, branches): (u32, BTreeMap<String, Branch>) =
                    serde_json::from_slice(&bytes).context(RECOVERY)?;
                ensure!(version == 1, "{RECOVERY}");
                branches
            } else {
                BTreeMap::new()
            };
        let ledger = Self {
            directory,
            scope,
            branches,
        };
        ledger.validate()?;
        Ok(ledger)
    }

    fn validate(&self) -> Result<()> {
        for (branch, state) in &self.branches {
            crate::domain::repo::valid_branch_name(branch)?;
            validate_mappings(&state.accepted)?;
            for candidate in &state.pending {
                validate_mappings(&candidate.mappings)?;
                ensure!(
                    candidate
                        .mappings
                        .get(&candidate.source_head)
                        .is_some_and(|entry| entry.public == candidate.public_head),
                    "{RECOVERY}"
                );
            }
        }
        Ok(())
    }

    pub fn get(&self, branch: &str, source: &str) -> Option<&Mapping> {
        self.branches.get(branch)?.accepted.get(source)
    }

    pub fn inherited(
        &self,
        branch: &str,
        source: &str,
        policy: &PrivacyPolicy,
    ) -> Result<Option<&Mapping>> {
        if let Some(entry) = self.get(branch, source) {
            return Ok(Some(entry));
        }
        let mut matches = self
            .branches
            .iter()
            .filter_map(|(name, state)| state.accepted.get(source).map(|entry| (name, entry)));
        let Some((origin, selected)) = matches.next() else {
            return Ok(None);
        };
        let origins = std::iter::once((origin, selected)).chain(matches);
        let target_restrictions = policy.branches.get(branch).cloned().unwrap_or_default();
        let policy_digest = policy.digest()?;
        for (origin, entry) in origins {
            ensure!(
                entry.public == selected.public
                    && entry.source_parents == selected.source_parents
                    && entry.public_parents == selected.public_parents
                    && compatible_recipient(entry, selected),
                "this ancestor has ambiguous accepted publications; continue from an explicit accepted public commit"
            );
            ensure!(
                (entry.source == entry.public || entry.policy == policy_digest)
                    && policy.branches.get(origin).cloned().unwrap_or_default()
                        == target_restrictions,
                "accepted ancestor has incompatible branch privacy restrictions; reconcile the policy before publishing the fork"
            );
        }
        Ok(Some(selected))
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
            receipt.mode.is_encrypted()
                && Some(entry.policy.as_str()) == receipt.policy_digest.as_deref()
                && entry.recipient.as_deref() == receipt.recipient.as_deref(),
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

    pub fn verify_remote_prefix(
        &self,
        branch: &str,
        remote_head: Option<&String>,
        source_parents: &BTreeMap<String, Vec<String>>,
    ) -> Result<()> {
        let Some(head) = remote_head else {
            return Ok(());
        };
        if source_parents.contains_key(head) {
            return Ok(());
        }
        ensure!(
            self.branches
                .get(branch)
                .is_some_and(|state| state.accepted.values().any(
                    |entry| &entry.public == head && source_parents.contains_key(&entry.source)
                )),
            "{RECOVERY}"
        );
        Ok(())
    }

    pub fn prepare(&mut self, candidates: &BTreeMap<String, Candidate>) -> Result<()> {
        for (branch, candidate) in candidates {
            let state = self.branches.entry(branch.clone()).or_default();
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

fn promote(state: &mut Branch, candidate: &Candidate, source: &str) -> Result<()> {
    let mut pending = vec![source.to_owned()];
    let mut visited = BTreeSet::new();
    while let Some(source) = pending.pop() {
        if !visited.insert(source.clone()) {
            continue;
        }
        let entry = candidate.mappings.get(&source).context(RECOVERY)?;
        if let Some(old) = state.accepted.get(&source) {
            ensure!(
                old.public == entry.public
                    && old.public_parents == entry.public_parents
                    && old.source_parents == entry.source_parents
                    && old.public_session == entry.public_session
                    && compatible_recipient(old, entry),
                "{RECOVERY}"
            );
        } else {
            state.accepted.insert(source.clone(), entry.clone());
        }
        pending.extend(entry.source_parents.clone());
    }
    state.pending.retain(|pending| {
        pending.source_head != source || pending.public_head != candidate.public_head
    });
    Ok(())
}

fn validate_mappings(mappings: &BTreeMap<String, Mapping>) -> Result<()> {
    for (source, entry) in mappings {
        ensure!(
            source == &entry.source
                && valid_oid(source)
                && valid_oid(&entry.public)
                && entry.source_parents.len() == entry.public_parents.len()
                && entry.store.len() == 64
                && entry.store.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "{RECOVERY}"
        );
        for (source_parent, public_parent) in entry.source_parents.iter().zip(&entry.public_parents)
        {
            ensure!(
                mappings
                    .get(source_parent)
                    .is_some_and(|parent| &parent.public == public_parent),
                "{RECOVERY}"
            );
        }
        super::super::privacy_envelope::validate_digest(&entry.policy)?;
        if let Some(recipient) = &entry.recipient {
            super::super::privacy_envelope::validate_digest(recipient)?;
        } else {
            ensure!(
                entry.source == entry.public,
                "a newly encrypted mapping requires its publishing recipient"
            );
        }
    }
    Ok(())
}

fn compatible_recipient(left: &Mapping, right: &Mapping) -> bool {
    match (&left.recipient, &right.recipient) {
        (Some(left), Some(right)) => left == right,
        _ => true,
    }
}
