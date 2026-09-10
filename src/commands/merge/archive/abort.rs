//! Cancels retained archive authority without erasing native evidence or visible Git history.
//!
//! Claim restoration follows durable Aborting intent and exact endpoint checks. A visible merge
//! candidate belongs to Landing replay; cancellation cannot roll it back or rebuild its contents.

use anyhow::{Context, ensure};

use super::{current_head, landing, require_destination_routing};
use crate::Result;
use crate::domain::link;
use crate::domain::merge_archive::{
    self, ArchiveJournal, ArchiveJournalGuard, ArchivePhase, ExplorationBinding, RetainedAbort,
    checked_abort_transaction,
};
use crate::domain::{mergetx, repo::Repo, store::Store};

pub struct AbortRequest<'a> {
    pub repo: &'a Repo,
    pub store: &'a Store,
    pub binding: &'a ExplorationBinding,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AbortOutcome {
    Aborted,
    AlreadyLanded(landing::LandingOutcome),
}

/// This core requires an explicit retained binding and does not dispatch a public merge command.
pub fn abort(request: AbortRequest<'_>) -> Result<AbortOutcome> {
    abort_with(request, |_| Ok(()))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Checkpoint {
    Intent,
    BeforeRestore(usize),
    Restored(usize),
    Aborted,
    BeforeRetire,
    Retired,
}

fn successor(request: &AbortRequest<'_>) -> Result<Option<String>> {
    let binding = request.binding;
    let selected = link::read_archive_link_snapshot(
        request.store,
        &binding.native.runtime,
        &binding.native.session_id,
    )?;
    if let Some(selected) = &selected {
        ensure!(
            selected.link.is_archive_for(
                &binding.role,
                &binding.native.runtime,
                &binding.native.session_id
            ) && selected.link.baseline_bytes == Some(binding.installed.bytes)
                && selected.link.baseline_hash.as_ref() == Some(&binding.installed.sha256),
            "archive cancellation successor has different authority"
        );
    }
    Ok(selected.map(|selected| selected.json))
}

fn require_claims(
    request: &AbortRequest<'_>,
    journal: &ArchiveJournal,
    restored: bool,
) -> Result<()> {
    for previous in &journal.previous_claims {
        let current = link::read_archive_link_snapshot(
            request.store,
            &previous.native.runtime,
            &previous.native.session_id,
        )?
        .context("archive cancellation previous claim is missing")?;
        ensure!(
            current.json == previous.original_json
                || (!restored && current.json == previous.retired_json),
            "archive cancellation claim differs from its retained endpoints"
        );
    }
    let (owner, agent) = request
        .binding
        .role
        .slug
        .split_once('/')
        .context("invalid archive slug")?;
    let active =
        link::archive_claims_for_branch(request.store, owner, agent, &request.binding.role.branch)?;
    ensure!(
        active
            .iter()
            .all(|image| journal.previous_claims.iter().any(|previous| {
                image.link.source == previous.native.runtime
                    && image.link.session_id == previous.native.session_id
                    && image.json == previous.original_json
            })),
        "archive cancellation would restore claims alongside foreign authority"
    );
    Ok(())
}

fn require_endpoints(
    request: &AbortRequest<'_>,
    journal: &ArchiveJournal,
    control: &mergetx::ControlGuard,
) -> Result<()> {
    let retained = journal
        .abort
        .as_ref()
        .context("archive cancellation intent is missing")?;
    require_destination_routing(request.repo, &request.binding.role.branch)?;
    ensure!(
        current_head(request.repo, &request.binding.role.branch)? == retained.expected_head,
        "archive cancellation target changed from its retained expected head"
    );
    ensure!(
        successor(request)? == retained.successor_json,
        "archive cancellation successor changed after its retained intent"
    );
    require_claims(request, journal, journal.phase == ArchivePhase::Aborted)?;
    let current = control.read_activation_snapshot()?;
    ensure!(
        current.as_ref().map(|image| image.json.as_str())
            == Some(retained.transaction_json.as_str())
            || (journal.phase == ArchivePhase::Aborted && current.is_none()),
        "archive cancellation transaction changed after its retained intent"
    );
    Ok(())
}

fn abort_with(
    request: AbortRequest<'_>,
    mut checkpoint: impl FnMut(Checkpoint) -> Result<()>,
) -> Result<AbortOutcome> {
    let binding = request.binding;
    let role = &binding.role;
    role.validate(role.origin_head.len())?;
    binding.native.validate()?;
    require_destination_routing(request.repo, &role.branch)?;
    let _branch = link::lock_branch(request.store, &role.slug, &role.branch)?;
    let observed = merge_archive::read_preparation_intent(request.repo.root(), &role.generation)?
        .context("archive cancellation has no retained journal")?;
    ensure!(
        observed.binding == *binding,
        "archive cancellation selects a different binding"
    );
    let mut keys: Vec<_> = observed
        .previous_claims
        .iter()
        .map(|claim| &claim.native)
        .collect();
    keys.push(&binding.native);
    keys.sort();
    let _links: Vec<_> = keys
        .into_iter()
        .map(|key| link::lock(request.store, &key.runtime, &key.session_id))
        .collect::<Result<_>>()?;
    let guard = ArchiveJournalGuard::acquire(request.repo.root(), &role.generation)?;
    let control = mergetx::ControlGuard::acquire(request.repo.root())?;
    guard.recover_pending()?;
    let mut journal = guard
        .read()?
        .context("archive cancellation journal disappeared")?;
    ensure!(
        journal == observed && journal.detach.is_none(),
        "archive cancellation participants or disposition changed before admission"
    );
    require_destination_routing(request.repo, &role.branch)?;
    let head = current_head(request.repo, &role.branch)?;
    if matches!(journal.phase, ArchivePhase::Landed { .. })
        || (journal.phase == ArchivePhase::Open
            && journal
                .publication
                .as_ref()
                .is_some_and(|publication| head == publication.candidate))
    {
        return landing::complete_visible(
            request.repo,
            request.store,
            binding,
            &guard,
            &control,
            &journal,
        )
        .map(AbortOutcome::AlreadyLanded);
    }
    ensure!(
        head == role.origin_head,
        "archive cancellation target moved outside its expected endpoint"
    );
    if matches!(journal.phase, ArchivePhase::Preparing | ArchivePhase::Open) {
        let transaction = control
            .read_activation_snapshot()?
            .context("archive cancellation transaction is missing")?;
        let selected = successor(&request)?;
        checked_abort_transaction(
            &transaction.json,
            binding,
            journal.phase == ArchivePhase::Preparing,
        )?;
        require_claims(&request, &journal, false)?;
        if let Some(activation) = &journal.activation {
            ensure!(
                (transaction.json == activation.transaction_original_json
                    || transaction.json == activation.transaction_bound_json)
                    && selected
                        .as_ref()
                        .is_none_or(|json| json == &activation.successor_json),
                "archive cancellation differs from retained preparation endpoints"
            );
        } else {
            ensure!(
                selected.is_some(),
                "archive cancellation successor is missing"
            );
            for previous in &journal.previous_claims {
                let current = link::read_archive_link_snapshot(
                    request.store,
                    &previous.native.runtime,
                    &previous.native.session_id,
                )?
                .context("archive cancellation previous claim is missing")?;
                ensure!(
                    current.json == previous.retired_json,
                    "open archive claim was restored without cancellation intent"
                );
            }
        }
        let mut aborting = journal.clone();
        aborting.abort = Some(RetainedAbort {
            expected_head: head,
            transaction_json: transaction.json,
            successor_json: selected,
            activation: journal.activation.clone(),
            cancelled_publication: journal.publication.clone(),
        });
        aborting.publication = None;
        aborting.phase = ArchivePhase::Aborting;
        guard.replace(&journal, &aborting)?;
        journal = aborting;
        checkpoint(Checkpoint::Intent)?;
    }
    ensure!(
        matches!(
            journal.phase,
            ArchivePhase::Aborting | ArchivePhase::Aborted
        ),
        "archive cancellation requires its retained abort disposition"
    );
    require_endpoints(&request, &journal, &control)?;
    if journal.phase == ArchivePhase::Aborting {
        for (index, previous) in journal.previous_claims.iter().enumerate() {
            checkpoint(Checkpoint::BeforeRestore(index))?;
            require_endpoints(&request, &journal, &control)?;
            let current = link::read_archive_link_snapshot(
                request.store,
                &previous.native.runtime,
                &previous.native.session_id,
            )?
            .context("archive cancellation claim disappeared before restoration")?;
            link::publish_archive_transition_locked(
                request.store,
                &previous.native.runtime,
                &previous.native.session_id,
                Some(&current.json),
                &previous.original_json,
            )?;
            checkpoint(Checkpoint::Restored(index))?;
        }
        require_endpoints(&request, &journal, &control)?;
        require_claims(&request, &journal, true)?;
        let mut aborted = journal.clone();
        aborted.phase = ArchivePhase::Aborted;
        aborted.activation = None;
        guard.replace(&journal, &aborted)?;
        journal = aborted;
        checkpoint(Checkpoint::Aborted)?;
    }
    checkpoint(Checkpoint::BeforeRetire)?;
    require_endpoints(&request, &journal, &control)?;
    control.complete_archive_abort(binding, &journal.abort.as_ref().unwrap().transaction_json)?;
    checkpoint(Checkpoint::Retired)?;
    Ok(AbortOutcome::Aborted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{merge::archive::activation, plumbing};
    use crate::domain::link::{ArchiveLinkSnapshot, Link};
    use crate::domain::merge_archive::{FrozenMergeSource, MergeArchiveRole, RuntimeLinkKey};
    use crate::domain::native_archive::Frontier;
    use crate::domain::{meta, storage, transcript};
    use sha2::{Digest, Sha256};
    use std::path::PathBuf;

    struct Fixture {
        _directory: tempfile::TempDir,
        repo: Repo,
        store: Store,
        binding: ExplorationBinding,
        installed: Link,
        transaction: String,
        previous: Vec<ArchiveLinkSnapshot>,
        native: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let repo = Repo::init(&directory.path().join("repo")).unwrap();
            let session = "agit-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
            let mut metadata = meta::Meta::new(session.into(), "codex".into(), "/work".into());
            metadata.turn = Some(1);
            meta::write(repo.root(), &metadata).unwrap();
            let log = transcript::wrap_lines(
                "{\"type\":\"user\",\"text\":\"inherited\"}\n",
                "codex",
                session,
            );
            storage::write_snapshot(repo.root(), &log, &log).unwrap();
            std::fs::write(repo.root().join("AGENTS.md"), "Shared instructions\n").unwrap();
            repo.add_all().unwrap();
            repo.commit("cancellation fixture").unwrap();
            let origin = repo.git(&["rev-parse", "HEAD"]).unwrap();
            repo.git(&["update-ref", "refs/heads/work", &origin])
                .unwrap();
            repo.git(&["symbolic-ref", "HEAD", "refs/heads/work"])
                .unwrap();
            let source = plumbing::commit_tree(
                &repo,
                &repo.git(&["rev-parse", "HEAD^{tree}"]).unwrap(),
                &[&origin],
                "frozen source",
            )
            .unwrap();
            repo.git(&["update-ref", "refs/heads/source", &source])
                .unwrap();
            let native = directory.path().join("native.jsonl");
            let bytes = b"{\"type\":\"session_meta\",\"id\":\"INSTALLED\"}\n";
            std::fs::write(&native, bytes).unwrap();
            let binding = ExplorationBinding {
                role: MergeArchiveRole {
                    generation: uuid::Uuid::now_v7().to_string(),
                    slug: "alice/photo".into(),
                    branch: "work".into(),
                    origin_head: origin.clone(),
                    logical_session: session.into(),
                },
                native: RuntimeLinkKey {
                    runtime: "codex".into(),
                    session_id: "INSTALLED".into(),
                },
                installed: Frontier {
                    bytes: bytes.len() as u64,
                    sha256: hex::encode(Sha256::digest(bytes)),
                },
                source: FrozenMergeSource {
                    reference: "alice/photo@source".into(),
                    slug: "alice/photo".into(),
                    branch: Some("source".into()),
                    head: source,
                    base: Some(origin.clone()),
                },
            };
            let mut installed = Link::new("codex", "INSTALLED", Some(directory.path()));
            installed.owner = Some("alice".into());
            installed.agent = Some("photo".into());
            installed.branch = Some("work".into());
            installed.materialized_from = Some(origin);
            installed.baseline_bytes = Some(binding.installed.bytes);
            installed.baseline_hash = Some(binding.installed.sha256.clone());
            let store = Store::at(directory.path().join("store"));
            let mut previous = Vec::new();
            for id in ["OLD_Z", "OLD_A"] {
                let mut old = installed.clone();
                old.session_id = id.into();
                let json = format!(
                    "{},\"future\":9007199254740993.0000000000000001}}\n",
                    old.to_json().unwrap().strip_suffix('}').unwrap()
                );
                let _lock = link::lock(&store, "codex", id).unwrap();
                link::publish_archive_transition_locked(&store, "codex", id, None, &json).unwrap();
                previous.push(
                    link::read_archive_link_snapshot(&store, "codex", id)
                        .unwrap()
                        .unwrap(),
                );
            }
            let transaction =
                merge_archive::test_activation(&binding, "{}".into()).transaction_original_json;
            let tx = mergetx::checked_activation_image(&transaction).unwrap();
            assert!(!tx.has_summary());
            mergetx::create(repo.root(), &tx).unwrap();
            let tx_path = crate::domain::repo::common_git_dir(repo.root()).join(mergetx::LOCK_FILE);
            merge_archive::durable_publish_transition_bytes(&tx_path, transaction.as_bytes(), true)
                .unwrap();
            Self {
                _directory: directory,
                repo,
                store,
                binding,
                installed,
                transaction,
                previous,
                native,
            }
        }

        fn activate(&self, stop: Option<activation::Checkpoint>) {
            let result = activation::activate_with(
                activation::ActivationRequest {
                    repo: &self.repo,
                    source_repo: &self.repo,
                    store: &self.store,
                    binding: &self.binding,
                    installed: &self.installed,
                    transaction_json: &self.transaction,
                    previous_claims: &self.previous,
                },
                |_| Ok(std::fs::read(&self.native)?),
                |point| {
                    if Some(point) == stop {
                        anyhow::bail!("injected activation stop");
                    }
                    Ok(())
                },
            );
            assert_eq!(result.is_err(), stop.is_some());
        }

        fn request(&self) -> AbortRequest<'_> {
            AbortRequest {
                repo: &self.repo,
                store: &self.store,
                binding: &self.binding,
            }
        }
        fn tx_path(&self) -> PathBuf {
            crate::domain::repo::common_git_dir(self.repo.root()).join(mergetx::LOCK_FILE)
        }
        fn journal(&self) -> ArchiveJournal {
            merge_archive::read(self.repo.root(), &self.binding.role.generation)
                .unwrap()
                .unwrap()
        }
        fn stop(&self, stop: Checkpoint) {
            assert!(
                abort_with(self.request(), |point| {
                    if point == stop {
                        anyhow::bail!("injected cancellation stop");
                    }
                    Ok(())
                })
                .is_err()
            );
        }
        fn git_bytes(&self) -> Vec<Vec<u8>> {
            [meta::LOG_FILE, meta::VIEW_FILE, meta::FILE, "AGENTS.md"]
                .into_iter()
                .map(|path| std::fs::read(self.repo.root().join(path)).unwrap())
                .chain([
                    std::fs::read(self.repo.git_path("index").unwrap()).unwrap(),
                    self.repo.git(&["show-ref"]).unwrap().into_bytes(),
                ])
                .collect()
        }
        fn endpoints(&self) -> Vec<Option<Vec<u8>>> {
            ["OLD_A", "OLD_Z", "INSTALLED"]
                .into_iter()
                .map(|id| std::fs::read(link::link_path(&self.store, "codex", id)).ok())
                .chain([
                    std::fs::read(self.tx_path()).ok(),
                    Some(serde_json::to_vec(&self.journal()).unwrap()),
                ])
                .collect()
        }
        fn assert_aborted(&self, transaction: &[u8], successor: Option<String>) {
            let journal = self.journal();
            assert_eq!(journal.phase, ArchivePhase::Aborted);
            assert_eq!(
                journal.abort.as_ref().unwrap().transaction_json.as_bytes(),
                transaction
            );
            assert_eq!(super::successor(&self.request()).unwrap(), successor);
            assert!(!self.tx_path().exists());
            assert_eq!(
                std::fs::read(
                    self.tx_path()
                        .with_extension(format!("aborted-{}.json", self.binding.role.generation))
                )
                .unwrap(),
                transaction
            );
            for image in &self.previous {
                assert_eq!(
                    link::read_archive_link_snapshot(
                        &self.store,
                        &image.link.source,
                        &image.link.session_id
                    )
                    .unwrap()
                    .unwrap()
                    .json,
                    image.json
                );
            }
            assert_eq!(
                current_head(&self.repo, "work").unwrap(),
                self.binding.role.origin_head
            );
        }
    }

    #[test]
    fn cancellation_recovers_every_activation_endpoint_without_summary_or_native_read() {
        for stop in [
            Some(activation::Checkpoint::Prepared),
            Some(activation::Checkpoint::Successor),
            Some(activation::Checkpoint::Retired(0)),
            Some(activation::Checkpoint::Retired(1)),
            Some(activation::Checkpoint::Transaction),
            None,
        ] {
            let fixture = Fixture::new();
            fixture.activate(stop);
            fixture
                .repo
                .git(&["update-ref", "-d", "refs/heads/source"])
                .unwrap();
            std::fs::write(&fixture.native, b"incomplete native evidence preserved").unwrap();
            let git = fixture.git_bytes();
            let transaction = std::fs::read(fixture.tx_path()).unwrap();
            let selected = successor(&fixture.request()).unwrap();
            assert_eq!(abort(fixture.request()).unwrap(), AbortOutcome::Aborted);
            fixture.assert_aborted(&transaction, selected.clone());
            assert_eq!(abort(fixture.request()).unwrap(), AbortOutcome::Aborted);
            fixture.assert_aborted(&transaction, selected);
            assert_eq!(fixture.git_bytes(), git);
            assert_eq!(
                std::fs::read(&fixture.native).unwrap(),
                b"incomplete native evidence preserved"
            );
        }
    }

    #[test]
    fn cancellation_replays_every_retained_restoration_and_retirement_endpoint() {
        for stop in [
            Checkpoint::Intent,
            Checkpoint::BeforeRestore(0),
            Checkpoint::Restored(0),
            Checkpoint::Restored(1),
            Checkpoint::Aborted,
            Checkpoint::BeforeRetire,
            Checkpoint::Retired,
        ] {
            let fixture = Fixture::new();
            fixture.activate(None);
            let transaction = std::fs::read(fixture.tx_path()).unwrap();
            let selected = successor(&fixture.request()).unwrap();
            let git = fixture.git_bytes();
            fixture.stop(stop);
            assert!(fixture.journal().abort.is_some());
            assert_eq!(abort(fixture.request()).unwrap(), AbortOutcome::Aborted);
            fixture.assert_aborted(&transaction, selected);
            assert_eq!(fixture.git_bytes(), git);
        }
    }

    #[test]
    fn cancellation_refuses_changed_claim_successor_generation_and_target_before_effects() {
        for pending in [false, true] {
            for changed in ["claim", "successor", "transaction", "head", "foreign"] {
                let fixture = Fixture::new();
                fixture.activate(None);
                if pending {
                    fixture.stop(Checkpoint::Intent);
                }
                match changed {
                    "claim" | "successor" => {
                        let id = if changed == "claim" {
                            "OLD_Z"
                        } else {
                            "INSTALLED"
                        };
                        let path = link::link_path(&fixture.store, "codex", id);
                        let text = std::fs::read_to_string(&path).unwrap();
                        if changed == "successor" && !pending {
                            let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
                            value["baseline_bytes"] = (fixture.binding.installed.bytes + 1).into();
                            std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
                        } else {
                            std::fs::write(&path, format!("{} \n", text.trim_end())).unwrap();
                        }
                    }
                    "transaction" => {
                        let mut tx = mergetx::read(fixture.repo.root()).unwrap().unwrap();
                        tx.generation = Some(uuid::Uuid::now_v7().to_string());
                        std::fs::write(fixture.tx_path(), serde_json::to_vec(&tx).unwrap())
                            .unwrap();
                    }
                    "head" => {
                        fixture
                            .repo
                            .git(&[
                                "update-ref",
                                "refs/heads/work",
                                &fixture.binding.source.head,
                            ])
                            .unwrap();
                    }
                    "foreign" => {
                        let _lock = link::lock(&fixture.store, "codex", "FOREIGN").unwrap();
                        link::publish_archive_transition_locked(
                            &fixture.store,
                            "codex",
                            "FOREIGN",
                            None,
                            &fixture.installed.to_json().unwrap(),
                        )
                        .unwrap();
                    }
                    _ => unreachable!(),
                }
                let before = fixture.endpoints();
                let git = fixture.git_bytes();
                assert!(
                    abort(fixture.request()).is_err(),
                    "accepted changed {changed}"
                );
                assert_eq!(fixture.endpoints(), before);
                assert_eq!(fixture.git_bytes(), git);
            }
        }
    }

    #[test]
    fn cancellation_rechecks_authority_before_each_restoration_and_retirement() {
        for stop in [Checkpoint::BeforeRestore(1), Checkpoint::BeforeRetire] {
            let fixture = Fixture::new();
            fixture.activate(None);
            let mut replacement = None;
            let result = abort_with(fixture.request(), |point| {
                if point == stop {
                    let bytes = b"{\"generation\":\"replacement\"}\n".to_vec();
                    std::fs::write(fixture.tx_path(), &bytes)?;
                    replacement = Some(bytes);
                }
                Ok(())
            });
            assert!(result.is_err());
            assert_eq!(
                std::fs::read(fixture.tx_path()).unwrap(),
                replacement.unwrap()
            );
            assert!(abort(fixture.request()).is_err());
        }
    }

    #[test]
    fn missing_transaction_requires_the_exact_durable_cancellation_carrier() {
        let fixture = Fixture::new();
        fixture.activate(None);
        fixture.stop(Checkpoint::Aborted);
        std::fs::remove_file(fixture.tx_path()).unwrap();
        let before = fixture.endpoints();
        assert!(abort(fixture.request()).is_err());
        assert_eq!(fixture.endpoints(), before);
    }

    #[test]
    fn cancellation_intent_cannot_drop_or_rewrite_its_restoration_authority() {
        let fixture = Fixture::new();
        fixture.activate(None);
        fixture.stop(Checkpoint::Intent);
        let pending = fixture.journal();
        let guard =
            ArchiveJournalGuard::acquire(fixture.repo.root(), &fixture.binding.role.generation)
                .unwrap();
        for change in ["transaction", "successor", "claims", "intent"] {
            let mut next = pending.clone();
            next.phase = ArchivePhase::Aborted;
            match change {
                "transaction" => next.abort.as_mut().unwrap().transaction_json.push(' '),
                "successor" => next
                    .abort
                    .as_mut()
                    .unwrap()
                    .successor_json
                    .as_mut()
                    .unwrap()
                    .push(' '),
                "claims" => next.previous_claims.clear(),
                "intent" => next.abort = None,
                _ => unreachable!(),
            }
            assert!(
                guard.replace(&pending, &next).is_err(),
                "accepted changed {change}"
            );
            assert_eq!(guard.read().unwrap(), Some(pending.clone()));
        }
        let control = mergetx::ControlGuard::acquire(fixture.repo.root()).unwrap();
        assert!(control.remove().is_err());
        assert!(
            control
                .complete_archive_abort(
                    &fixture.binding,
                    &pending.abort.as_ref().unwrap().transaction_json
                )
                .is_err()
        );
    }

    #[test]
    fn completed_cancellation_preserves_replacement_transaction_and_changed_restored_claim() {
        for replacement in [true, false] {
            let fixture = Fixture::new();
            fixture.activate(None);
            abort(fixture.request()).unwrap();
            let path = if replacement {
                fixture.tx_path()
            } else {
                link::link_path(&fixture.store, "codex", "OLD_A")
            };
            std::fs::write(&path, b"{\"foreign\":true}\n").unwrap();
            let before = fixture.endpoints();
            assert!(abort(fixture.request()).is_err());
            assert_eq!(fixture.endpoints(), before);
            assert_eq!(std::fs::read(path).unwrap(), b"{\"foreign\":true}\n");
        }
    }
}
