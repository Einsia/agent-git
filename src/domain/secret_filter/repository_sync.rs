//! Incremental synchronization retains the revision that authorized each local decision.

use super::repository::{bump_and_write, reseal_record};
use super::*;
use crate::domain::secrets::repository_policy::*;
use std::collections::HashSet;

#[derive(Debug, thiserror::Error)]
#[error(
    "repository declaration policy changed; review `agit secrets review --json` and make a new allow/unallow decision"
)]
pub struct PolicySyncConflict;

pub trait RepositoryPolicyTransport {
    fn snapshot(&self) -> crate::Result<PolicySnapshot>;
    fn change(&self, change: &PolicyChange) -> crate::Result<PolicyChangeResponse>;
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct PolicyCache {
    pub target: DeclarationTarget,
    pub snapshot: PolicySnapshot,
}

pub(super) fn read_cache(unlocked: &Unlocked) -> crate::Result<Option<PolicyCache>> {
    let Some(sealed) = &unlocked.file.repository_policy else {
        return Ok(None);
    };
    let bytes = open(
        &unlocked.dek,
        sealed,
        format!("repository-policy:{}", unlocked.file.vault_id).as_bytes(),
    )?;
    let cache: PolicyCache =
        serde_json::from_slice(&bytes).context("invalid cached repository policy")?;
    cache.snapshot.validate(&cache.target.repository_id)?;
    Ok(Some(cache))
}

fn save_cache(unlocked: &mut Unlocked, cache: &PolicyCache) -> crate::Result<()> {
    let bytes = Zeroizing::new(serde_json::to_vec(cache)?);
    unlocked.file.repository_policy = Some(seal(
        &unlocked.dek,
        &bytes,
        format!("repository-policy:{}", unlocked.file.vault_id).as_bytes(),
    )?);
    Ok(())
}

pub(super) fn active_identities(
    unlocked: Option<&Unlocked>,
    records: &[DecryptedRecord],
) -> crate::Result<HashSet<String>> {
    let mut identities: HashSet<String> = match unlocked.map(read_cache).transpose()?.flatten() {
        Some(cache) => cache
            .snapshot
            .decisions
            .into_iter()
            .filter(|d| d.state == DecisionState::Active)
            .map(|d| d.value_identity)
            .collect(),
        None => HashSet::new(),
    };
    for record in records {
        if record
            .declaration
            .as_ref()
            .is_some_and(|d| d.pending_operation.is_some())
        {
            let identity = value_identity(&record.secret);
            if record.heuristic_disposition == HeuristicDisposition::Allow {
                identities.insert(identity);
            } else {
                identities.remove(&identity);
            }
        }
    }
    Ok(identities)
}

impl<K: KeyStore> RepositoryDictionary<K> {
    /// A confirmed copy retains reversible mappings and explicit blocks, but source allowances
    /// and pending declarations do not authorize publication to the new repository.
    pub fn reset_policy_for_copy(&self, source: &DeclarationTarget) -> crate::Result<()> {
        self.store.with_lock(|| {
            if !self.exists() {
                return Ok(());
            }
            let mut unlocked = self.store.unlock_existing()?;
            if let Some(cache) = read_cache(&unlocked)? {
                anyhow::ensure!(
                    &cache.target == source,
                    "cached repository policy does not belong to the copy source"
                );
            }
            let mut records = decrypt_records(&unlocked.file, &unlocked.dek)?;
            for record in &records {
                if let Some(target) = record.declaration.as_ref().and_then(|d| d.target.as_ref()) {
                    anyhow::ensure!(
                        target == source,
                        "repository declaration does not belong to the copy source"
                    );
                }
            }
            let mut changed = unlocked.file.repository_policy.take().is_some();
            for record in &mut records {
                if record.declaration.is_some()
                    || record.heuristic_disposition == HeuristicDisposition::Allow
                {
                    record.declaration = None;
                    record.heuristic_disposition = HeuristicDisposition::Protect;
                    // Detached allowances need a protection origin even without a discovery rule.
                    if !record.origins.contains(&RecordOrigin::Heuristic) {
                        record.origins.push(RecordOrigin::Heuristic);
                    }
                    reseal_record(&mut unlocked, record)?;
                    changed = true;
                }
            }
            if changed {
                bump_and_write(&self.store, &mut unlocked, false)?;
            }
            Ok(())
        })
    }

    pub fn has_policy_state(&self) -> crate::Result<bool> {
        self.store.with_lock(|| {
            if !self.exists() {
                return Ok(false);
            }
            let unlocked = self.store.unlock_existing()?;
            Ok(unlocked.file.repository_policy.is_some()
                || decrypt_records(&unlocked.file, &unlocked.dek)?
                    .iter()
                    .any(|r| {
                        r.declaration.is_some()
                            || r.heuristic_disposition == HeuristicDisposition::Allow
                    }))
        })
    }

    pub fn synchronize_policy(
        &self,
        target: &DeclarationTarget,
        transport: &impl RepositoryPolicyTransport,
        dry_run: bool,
    ) -> crate::Result<()> {
        if !dry_run {
            self.bind_declarations(target)?;
        }
        self.store.with_lock(|| {
            let created = !self.exists();
            let mut unlocked = if created { None } else { Some(self.store.unlock_existing()?) };
            let cached = unlocked.as_ref().map(read_cache).transpose()?.flatten();
            if let Some(cache) = &cached {
                anyhow::ensure!(&cache.target == target, "cached repository policy belongs to a different Hub or immutable repository");
            }
            let mut records = match &unlocked {
                Some(unlocked) => decrypt_records(&unlocked.file, &unlocked.dek)?,
                None => vec![],
            };
            for record in &records {
                if let Some(bound) = record.declaration.as_ref().and_then(|d| d.target.as_ref()) {
                    anyhow::ensure!(bound == target, "repository declarations belong to a different Hub or immutable repository");
                }
            }
            let mut snapshot = transport.snapshot()?;
            snapshot.validate(&target.repository_id)?;
            anyhow::ensure!(cached.as_ref().is_none_or(|c| c.snapshot.revision <= snapshot.revision), "repository policy revision moved backwards");
            anyhow::ensure!(!dry_run || cached.as_ref().is_none_or(|c| c.snapshot.revision == snapshot.revision),
                "remote repository policy changed; a normal push must refresh local scan policy before publication");
            for record in &mut records {
                if record.declaration.is_none() && record.heuristic_disposition == HeuristicDisposition::Allow {
                    record.declaration = Some(RepositoryDeclaration {
                        reason: None, target: Some(target.clone()), pending_operation: Some(DeclarationOperation::Allow),
                        base_revision: None, server_policy_id: None, server_version: None, conflict: false, unconfirmed_write: None,
                    });
                }
                let identity = value_identity(&record.secret);
                if let Some(declaration) = &mut record.declaration
                    && declaration.pending_operation.is_some() && declaration.base_revision.is_none()
                    && !snapshot.decisions.iter().any(|d| d.value_identity == identity) {
                    declaration.base_revision = Some(snapshot.revision);
                }
            }
            let mut error = None;
            for index in 0..records.len() {
                let record = &mut records[index];
                let Some(declaration) = &mut record.declaration else { continue };
                declaration.target = Some(target.clone());
                let identity = value_identity(&record.secret);
                let remote = snapshot.decisions.iter().find(|d| d.value_identity == identity).cloned();
                let active = remote.as_ref().is_some_and(|d| d.state == DecisionState::Active);
                let Some(operation) = declaration.pending_operation else {
                    continue;
                };
                let wanted_active = operation == DeclarationOperation::Allow;
                if let Some((previous, revision)) = declaration.unconfirmed_write {
                    if snapshot.revision > revision {
                        declaration.unconfirmed_write = None;
                    } else if previous != operation {
                        error = Some(anyhow::anyhow!("a prior declaration write has not been confirmed; the new local decision remains pending until synchronization can verify its outcome"));
                        continue;
                    }
                }
                if !declaration.conflict && active == wanted_active {
                    let recovered_revision = cached.as_ref().filter(|previous|
                        declaration.base_revision == Some(previous.snapshot.revision)
                        && only_value_changed(&previous.snapshot, &snapshot, &identity)
                    ).map(|previous| previous.snapshot.revision);
                    declaration.pending_operation = None;
                    declaration.server_policy_id = remote.as_ref().map(|d| d.id.clone());
                    declaration.server_version = Some(snapshot.revision);
                    declaration.unconfirmed_write = None;
                    if let Some(previous) = recovered_revision {
                        advance_pending(&mut records, previous, snapshot.revision);
                    }
                    continue;
                }
                if declaration.conflict
                    || declaration.base_revision.is_some_and(|revision| revision != snapshot.revision)
                    || (declaration.base_revision.is_none() && remote.is_some())
                {
                    declaration.conflict = true;
                    error = Some(anyhow::Error::from(PolicySyncConflict));
                    continue;
                }
                declaration.base_revision = Some(snapshot.revision);
                if dry_run { continue }
                declaration.unconfirmed_write = Some((operation, snapshot.revision));
                let action = match operation {
                    DeclarationOperation::Allow => PolicyAction::Allow { value_identity: identity.clone(), reason: declaration.reason.clone() },
                    DeclarationOperation::Revoke => PolicyAction::Revoke { policy_id: remote.as_ref().expect("active remote decision exists").id.clone() },
                };
                // Persist the request's precondition before sending it so a lost reply is retryable.
                if unlocked.is_none() { unlocked = Some(self.store.create_unlocked()?); }
                persist(self, unlocked.as_mut().unwrap(), &records, target, &snapshot, created)?;
                let changed = transport.change(&PolicyChange {
                    version: 1, expected_agent_id: target.repository_id.clone(), expected_revision: snapshot.revision, action,
                });
                let declaration = records[index].declaration.as_mut().unwrap();
                let response = match changed {
                    Ok(response) => response,
                    Err(failure) => {
                        declaration.conflict = failure.is::<PolicySyncConflict>();
                        if declaration.conflict
                            && let Ok(current) = transport.snapshot()
                            && current.validate(&target.repository_id).is_ok()
                            && current.revision >= snapshot.revision {
                            snapshot = current;
                        }
                        error = Some(failure);
                        break;
                    }
                };
                if response.version != 1 || response.agent_id != target.repository_id
                    || response.revision <= snapshot.revision || response.decision.value_identity != identity
                    || (response.decision.state == DecisionState::Active) != wanted_active
                    || response.decision.validate(response.revision).is_err()
                    || remote.as_ref().is_some_and(|previous| previous.id != response.decision.id)
                {
                    error = Some(anyhow::anyhow!("invalid repository declaration acknowledgement; local operation remains pending"));
                    break;
                }
                let previous_revision = snapshot.revision;
                if response.revision == previous_revision.saturating_add(1)
                    && response.decision.revision == response.revision {
                    snapshot.revision = response.revision;
                    snapshot.decisions.retain(|d| d.value_identity != identity);
                    snapshot.decisions.push(response.decision);
                    advance_pending(&mut records, previous_revision, snapshot.revision);
                } else {
                    match transport.snapshot().and_then(|current| {
                        current.validate(&target.repository_id)?;
                        anyhow::ensure!(current.revision >= response.revision, "repository policy acknowledgement is ahead of its snapshot");
                        Ok(current)
                    }) {
                        Ok(current) => snapshot = current,
                        Err(failure) => { error = Some(failure); break; }
                    }
                }
                let declaration = records[index].declaration.as_mut().unwrap();
                let current = snapshot.decisions.iter().find(|d| d.value_identity == identity);
                if current.is_some_and(|d| (d.state == DecisionState::Active) == wanted_active) {
                    declaration.pending_operation = None;
                    declaration.server_policy_id = current.map(|d| d.id.clone());
                    declaration.server_version = Some(snapshot.revision);
                    declaration.unconfirmed_write = None;
                } else {
                    declaration.conflict = true;
                    error = Some(anyhow::Error::from(PolicySyncConflict));
                }
            }
            for record in &mut records {
                if let Some(declaration) = &mut record.declaration
                    && declaration.pending_operation.is_none() {
                    let identity = value_identity(&record.secret);
                    let remote = snapshot.decisions.iter().find(|decision| decision.value_identity == identity);
                    record.heuristic_disposition = if remote.is_some_and(|d| d.state == DecisionState::Active) {
                        HeuristicDisposition::Allow
                    } else { HeuristicDisposition::Protect };
                    declaration.server_policy_id = remote.map(|d| d.id.clone());
                    declaration.server_version = Some(snapshot.revision);
                }
            }
            if !dry_run {
                if unlocked.is_none() { unlocked = Some(self.store.create_unlocked()?); }
                persist(self, unlocked.as_mut().unwrap(), &records, target, &snapshot, created)?;
            }
            match error { Some(error) => Err(error), None => Ok(()) }
        })
    }
}

fn advance_pending(records: &mut [DecryptedRecord], previous: u64, current: u64) {
    for record in records {
        if let Some(pending) = &mut record.declaration
            && pending.pending_operation.is_some()
            && !pending.conflict
            && pending.base_revision == Some(previous)
        {
            pending.base_revision = Some(current);
        }
    }
}

fn only_value_changed(previous: &PolicySnapshot, current: &PolicySnapshot, identity: &str) -> bool {
    if current.revision != previous.revision.saturating_add(1)
        || !current
            .decisions
            .iter()
            .any(|d| d.value_identity == identity && d.revision == current.revision)
    {
        return false;
    }
    let other = |snapshot: &PolicySnapshot| {
        snapshot
            .decisions
            .iter()
            .filter(|d| d.value_identity != identity)
            .map(|d| (d.id.clone(), d.clone()))
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    other(previous) == other(current)
}

fn persist<K: KeyStore>(
    dictionary: &RepositoryDictionary<K>,
    unlocked: &mut Unlocked,
    records: &[DecryptedRecord],
    target: &DeclarationTarget,
    snapshot: &PolicySnapshot,
    created: bool,
) -> crate::Result<()> {
    for record in records {
        reseal_record(unlocked, record)?;
    }
    save_cache(
        unlocked,
        &PolicyCache {
            target: target.clone(),
            snapshot: snapshot.clone(),
        },
    )?;
    bump_and_write(&dictionary.store, unlocked, created)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    struct Hub {
        snapshot: RefCell<PolicySnapshot>,
        writes: Cell<usize>,
        lose_reply: Cell<bool>,
        race_on_write: Cell<bool>,
        external_revocation_on_write: Cell<bool>,
    }

    impl RepositoryPolicyTransport for Hub {
        fn snapshot(&self) -> crate::Result<PolicySnapshot> {
            Ok(self.snapshot.borrow().clone())
        }
        fn change(&self, change: &PolicyChange) -> crate::Result<PolicyChangeResponse> {
            let mut snapshot = self.snapshot.borrow_mut();
            if self.race_on_write.replace(false) {
                snapshot.revision += 1;
            }
            if snapshot.revision != change.expected_revision {
                return Err(PolicySyncConflict.into());
            }
            assert_eq!(change.expected_agent_id, snapshot.agent_id);
            snapshot.revision += 1;
            let revision = snapshot.revision;
            let decision = match &change.action {
                PolicyAction::Allow {
                    value_identity,
                    reason,
                } => {
                    let id = snapshot
                        .decisions
                        .iter()
                        .find(|d| &d.value_identity == value_identity)
                        .map(|d| d.id.clone())
                        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                    snapshot
                        .decisions
                        .retain(|d| &d.value_identity != value_identity);
                    PolicyDecision {
                        id,
                        value_identity: value_identity.clone(),
                        state: DecisionState::Active,
                        revision,
                        reason: reason.clone(),
                    }
                }
                PolicyAction::Revoke { policy_id } => {
                    let mut decision = snapshot
                        .decisions
                        .iter()
                        .find(|d| &d.id == policy_id)
                        .unwrap()
                        .clone();
                    snapshot.decisions.retain(|d| &d.id != policy_id);
                    decision.state = DecisionState::Revoked;
                    decision.revision = revision;
                    decision
                }
            };
            snapshot.decisions.push(decision.clone());
            if self.external_revocation_on_write.replace(false) {
                snapshot.revision += 1;
                let revision = snapshot.revision;
                snapshot.decisions.push(PolicyDecision {
                    id: "external-b".into(),
                    value_identity: value_identity("second fixture"),
                    state: DecisionState::Revoked,
                    revision,
                    reason: None,
                });
            }
            self.writes.set(self.writes.get() + 1);
            if self.lose_reply.replace(false) {
                anyhow::bail!("connection lost after policy write")
            }
            Ok(PolicyChangeResponse {
                version: 1,
                agent_id: snapshot.agent_id.clone(),
                revision: snapshot.revision,
                decision,
            })
        }
    }

    fn fixture() -> (
        tempfile::TempDir,
        RepositoryDictionary,
        DeclarationTarget,
        Hub,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let repo = crate::domain::repo::Repo::init(dir.path()).unwrap();
        let dictionary = RepositoryDictionary::open(repo.root()).unwrap();
        let target = DeclarationTarget {
            hub: "https://hub.example".into(),
            repository_id: uuid::Uuid::new_v4().to_string(),
        };
        let hub = Hub {
            snapshot: RefCell::new(PolicySnapshot {
                version: 1,
                agent_id: target.repository_id.clone(),
                revision: 0,
                value_identity_scheme: VALUE_IDENTITY_SCHEME.into(),
                decisions: vec![],
            }),
            writes: Cell::new(0),
            lose_reply: Cell::new(false),
            race_on_write: Cell::new(false),
            external_revocation_on_write: Cell::new(false),
        };
        (dir, dictionary, target, hub)
    }

    #[test]
    fn sync_is_incremental_and_lost_replies_are_idempotent() {
        let (_dir, dictionary, target, hub) = fixture();
        let a = dictionary
            .allow_value(
                Zeroizing::new("first fixture".into()),
                Some("Public sample"),
            )
            .unwrap();
        let b = dictionary
            .allow_value(Zeroizing::new("second fixture".into()), None)
            .unwrap();
        let before = std::fs::read(&dictionary.store.path).unwrap();
        dictionary.synchronize_policy(&target, &hub, true).unwrap();
        assert_eq!(std::fs::read(&dictionary.store.path).unwrap(), before);
        assert_eq!(hub.writes.get(), 0);
        hub.lose_reply.set(true);
        assert!(dictionary.synchronize_policy(&target, &hub, false).is_err());
        assert_eq!(hub.writes.get(), 1);
        dictionary.synchronize_policy(&target, &hub, false).unwrap();
        assert_eq!(hub.writes.get(), 2);
        assert!(
            dictionary
                .review()
                .unwrap()
                .iter()
                .all(|d| d.sync_status == "synced")
        );
        assert!(
            dictionary
                .review()
                .unwrap()
                .iter()
                .all(|d| d.server_policy_id.is_some())
        );
        dictionary.unallow(&a.id).unwrap();
        dictionary.synchronize_policy(&target, &hub, false).unwrap();
        assert_eq!(hub.writes.get(), 3);
        assert_eq!(
            dictionary
                .review()
                .unwrap()
                .iter()
                .find(|d| d.id == b.id)
                .unwrap()
                .local_state,
            "allowed"
        );
        dictionary.synchronize_policy(&target, &hub, false).unwrap();
        assert_eq!(hub.writes.get(), 3);
    }

    #[test]
    fn remote_revocation_and_stale_local_intent_require_a_new_decision() {
        let (_dir, dictionary, target, hub) = fixture();
        let value = "ghp_R7kQ2mXv9LpZ4tNc8WjF3bHy6sVd1aGe5uKr";
        let record = dictionary
            .allow_value(Zeroizing::new(value.into()), None)
            .unwrap();
        dictionary.synchronize_policy(&target, &hub, false).unwrap();
        dictionary.allow(&record.id).unwrap();
        {
            let mut snapshot = hub.snapshot.borrow_mut();
            snapshot.revision += 1;
            snapshot.decisions[0].revision = snapshot.revision;
            snapshot.decisions[0].state = DecisionState::Revoked;
        }
        assert!(
            dictionary
                .synchronize_policy(&target, &hub, false)
                .unwrap_err()
                .is::<PolicySyncConflict>()
        );
        assert_eq!(dictionary.review().unwrap()[0].sync_status, "conflict");
        assert!(dictionary.synchronize_policy(&target, &hub, false).is_err());
        assert_eq!(hub.writes.get(), 1);
        dictionary
            .allow_with_reason(&record.id, Some("Reviewed revocation"))
            .unwrap();
        dictionary.synchronize_policy(&target, &hub, false).unwrap();
        assert_eq!(hub.writes.get(), 2);
        {
            let mut snapshot = hub.snapshot.borrow_mut();
            snapshot.revision += 1;
            snapshot.decisions[0].revision = snapshot.revision;
            snapshot.decisions[0].state = DecisionState::Revoked;
        }
        dictionary.synchronize_policy(&target, &hub, false).unwrap();
        assert_eq!(dictionary.review().unwrap()[0].local_state, "default");
        assert!(
            !crate::domain::secrets::scan_text_registered_with(
                value,
                &HashSet::new(),
                &dictionary.active_matcher().unwrap()
            )
            .is_empty()
        );
    }

    #[test]
    fn cached_remote_only_allowances_apply_to_scans_and_projection() {
        let (_dir, dictionary, target, hub) = fixture();
        let a = "ghp_R7kQ2mXv9LpZ4tNc8WjF3bHy6sVd1aGe5uKr";
        let b = "ghp_T8nR4pYw6MkZ2sDc9XjH5bFu7vQe1aGt3iLs";
        hub.change(&PolicyChange {
            version: 1,
            expected_agent_id: target.repository_id.clone(),
            expected_revision: 0,
            action: PolicyAction::Allow {
                value_identity: value_identity(a),
                reason: None,
            },
        })
        .unwrap();
        dictionary.synchronize_policy(&target, &hub, false).unwrap();
        assert!(dictionary.review().unwrap().is_empty());
        let matcher = dictionary.active_matcher().unwrap();
        assert!(
            crate::domain::secrets::scan_text_registered_with(a, &HashSet::new(), &matcher)
                .is_empty()
        );
        assert!(
            !crate::domain::secrets::scan_text_registered_with(b, &HashSet::new(), &matcher)
                .is_empty()
        );
        assert_eq!(
            dictionary
                .protect_jsonl(&serde_json::to_string(a).unwrap(), &Matcher::empty())
                .unwrap()
                .replacements,
            0
        );
        assert!(
            dictionary
                .protect_jsonl(&serde_json::to_string(b).unwrap(), &Matcher::empty())
                .unwrap()
                .replacements
                > 0
        );
    }

    /// Copies drop source decisions and pending writes without discarding restoration or blocks.
    #[test]
    fn copying_resets_policy_authority_but_preserves_protection_records() {
        let (_dir, dictionary, source, hub) = fixture();
        let (_copy_dir, _, destination, destination_hub) = fixture();
        let mapped = "ghp_R7kQ2mXv9LpZ4tNc8WjF3bHy6sVd1aGe5uKr";
        let remote_only = "ghp_T8nR4pYw6MkZ2sDc9XjH5bFu7vQe1aGt3iLs";
        let protected = dictionary.protect_text(mapped, &Matcher::empty()).unwrap();
        let id = dictionary.review().unwrap()[0].id.clone();
        dictionary.allow(&id).unwrap();
        dictionary.synchronize_policy(&source, &hub, false).unwrap();
        hub.change(&PolicyChange {
            version: 1,
            expected_agent_id: source.repository_id.clone(),
            expected_revision: 1,
            action: PolicyAction::Allow {
                value_identity: value_identity(remote_only),
                reason: None,
            },
        })
        .unwrap();
        dictionary.synchronize_policy(&source, &hub, false).unwrap();
        let declaration_only = "pending public example";
        assert!(crate::domain::secrets::scan_text(declaration_only, &HashSet::new()).is_empty());
        let declared = dictionary
            .allow_value(declaration_only.to_owned().into(), None)
            .unwrap();
        assert_eq!(declared.origins, vec!["declaration"]);
        dictionary
            .block_add("local-block", "blue horse battery".to_owned().into(), false)
            .unwrap();
        let before = std::fs::read(&dictionary.store.path).unwrap();
        assert!(dictionary.reset_policy_for_copy(&destination).is_err());
        assert_eq!(std::fs::read(&dictionary.store.path).unwrap(), before);

        dictionary.reset_policy_for_copy(&source).unwrap();
        assert!(!dictionary.has_policy_state().unwrap());
        assert_eq!(
            dictionary.hydrate_text(&protected.text).unwrap().text,
            mapped
        );
        let records = dictionary.review().unwrap();
        assert!(
            records
                .iter()
                .all(|record| record.target.is_none() && record.pending_operation.is_none())
        );
        assert!(records.iter().any(|record| record.explicit_block));
        let copied_declaration = records
            .iter()
            .find(|record| record.id == declared.id)
            .unwrap();
        assert!(copied_declaration.effective_protect);
        assert!(!copied_declaration.explicit_block);
        let matcher = dictionary.active_matcher().unwrap();
        for value in [mapped, remote_only, "blue horse battery", declaration_only] {
            assert!(
                !crate::domain::secrets::scan_text_registered_with(
                    value,
                    &HashSet::new(),
                    &matcher
                )
                .is_empty()
            );
        }
        let protected = dictionary
            .protect_text(declaration_only, &Matcher::empty())
            .unwrap();
        assert_ne!(protected.text, declaration_only);
        assert_eq!(protected.new_records, 0);
        assert_eq!(
            dictionary.hydrate_text(&protected.text).unwrap().text,
            declaration_only
        );
        dictionary
            .synchronize_policy(&destination, &destination_hub, false)
            .unwrap();
        assert_eq!(destination_hub.writes.get(), 0);
    }

    #[test]
    fn a_server_revision_race_is_not_retried_as_a_new_decision() {
        let (_dir, dictionary, target, hub) = fixture();
        let record = dictionary
            .allow_value(Zeroizing::new("public fixture".into()), None)
            .unwrap();
        hub.race_on_write.set(true);
        assert!(
            dictionary
                .synchronize_policy(&target, &hub, false)
                .unwrap_err()
                .is::<PolicySyncConflict>()
        );
        assert_eq!(hub.writes.get(), 0);
        assert!(dictionary.synchronize_policy(&target, &hub, false).is_err());
        assert_eq!(hub.writes.get(), 0);
        dictionary.allow(&record.id).unwrap();
        dictionary.synchronize_policy(&target, &hub, false).unwrap();
        assert_eq!(hub.writes.get(), 1);
    }

    #[test]
    fn legacy_local_allows_cannot_reactivate_remote_tombstones() {
        let (_dir, dictionary, target, hub) = fixture();
        let value = "public fixture";
        dictionary
            .allow_value(Zeroizing::new(value.into()), None)
            .unwrap();
        let mut unlocked = dictionary.store.unlock_existing().unwrap();
        let mut records = decrypt_records(&unlocked.file, &unlocked.dek).unwrap();
        records[0].declaration = None;
        reseal_record(&mut unlocked, &records[0]).unwrap();
        write_vault(&dictionary.store.path, &unlocked.file).unwrap();
        hub.change(&PolicyChange {
            version: 1,
            expected_agent_id: target.repository_id.clone(),
            expected_revision: 0,
            action: PolicyAction::Allow {
                value_identity: value_identity(value),
                reason: None,
            },
        })
        .unwrap();
        let id = hub.snapshot.borrow().decisions[0].id.clone();
        hub.change(&PolicyChange {
            version: 1,
            expected_agent_id: target.repository_id.clone(),
            expected_revision: 1,
            action: PolicyAction::Revoke { policy_id: id },
        })
        .unwrap();
        assert!(
            dictionary
                .synchronize_policy(&target, &hub, false)
                .unwrap_err()
                .is::<PolicySyncConflict>()
        );
        assert_eq!(hub.writes.get(), 2);
        assert_eq!(dictionary.review().unwrap()[0].sync_status, "conflict");
    }

    #[test]
    fn an_idempotent_reply_cannot_authorize_other_pending_changes() {
        let (_dir, dictionary, target, hub) = fixture();
        dictionary
            .allow_value(Zeroizing::new("first fixture".into()), None)
            .unwrap();
        let b = dictionary
            .allow_value(Zeroizing::new("second fixture".into()), None)
            .unwrap();
        hub.external_revocation_on_write.set(true);
        assert!(
            dictionary
                .synchronize_policy(&target, &hub, false)
                .unwrap_err()
                .is::<PolicySyncConflict>()
        );
        assert_eq!(hub.writes.get(), 1);
        assert!(
            hub.snapshot
                .borrow()
                .decisions
                .iter()
                .any(|d| d.id == "external-b" && d.state == DecisionState::Revoked)
        );
        dictionary.allow(&b.id).unwrap();
        dictionary.synchronize_policy(&target, &hub, false).unwrap();
        assert_eq!(hub.writes.get(), 2);
    }

    #[test]
    fn an_unconfirmed_allow_cannot_be_reported_as_revoked_from_an_unchanged_snapshot() {
        struct UnavailableWrite<'a>(&'a Hub);
        impl RepositoryPolicyTransport for UnavailableWrite<'_> {
            fn snapshot(&self) -> crate::Result<PolicySnapshot> {
                self.0.snapshot()
            }
            fn change(&self, _: &PolicyChange) -> crate::Result<PolicyChangeResponse> {
                anyhow::bail!("connection closed without acknowledgement")
            }
        }
        let (_dir, dictionary, target, hub) = fixture();
        let record = dictionary
            .allow_value(Zeroizing::new("public fixture".into()), None)
            .unwrap();
        assert!(
            dictionary
                .synchronize_policy(&target, &UnavailableWrite(&hub), false)
                .is_err()
        );
        dictionary.unallow(&record.id).unwrap();
        assert!(dictionary.synchronize_policy(&target, &hub, false).is_err());
        let record = dictionary.review().unwrap().remove(0);
        assert_eq!(record.local_state, "default");
        assert_eq!(record.sync_status, "pending");
        assert!(record.write_outcome_uncertain);
        assert_eq!(hub.writes.get(), 0);
        hub.snapshot.borrow_mut().revision += 1;
        dictionary.synchronize_policy(&target, &hub, false).unwrap();
        assert_eq!(dictionary.review().unwrap()[0].sync_status, "synced");
        assert_eq!(hub.writes.get(), 0);
    }
}
