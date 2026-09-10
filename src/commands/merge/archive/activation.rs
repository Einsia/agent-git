//! Durably binds an explicitly installed native instance before any merge-agent launch.
//!
//! Activation owns only local authority transitions. It neither installs nor launches a runtime,
//! publishes Git history, lands a merge, or dispatches ordinary resume and hook callers. The
//! installed Link is unpublished; a retained Preparing intent is the only replay authority.

use anyhow::{Context, ensure};

#[cfg(test)]
use super::current_head;
use super::{encode_facts, read_native, require_destination_routing};
use crate::Result;
use crate::domain::link::{self, ArchiveLinkSnapshot, Link};
use crate::domain::merge_archive::{
    ArchiveJournal, ArchiveJournalGuard, ArchivePhase, ExplorationBinding, PreparedActivation,
    PreviousClaim, RuntimeLinkKey,
};
use crate::domain::metadata_facts::JsonFacts;
use crate::domain::{
    archive_history, merge_archive, mergetx, native_archive, repo::Repo, store::Store,
};

/// The caller selects every participant before entering the branch and sorted Link lock order.
pub struct ActivationRequest<'a> {
    pub repo: &'a Repo,
    pub source_repo: &'a Repo,
    pub store: &'a Store,
    pub binding: &'a ExplorationBinding,
    pub installed: &'a Link,
    pub transaction_json: &'a str,
    pub previous_claims: &'a [ArchiveLinkSnapshot],
}

/// Return only after the role, retired claims, exact transaction binding and Open journal agree.
pub fn activate(request: ActivationRequest<'_>) -> Result<ExplorationBinding> {
    activate_with(request, read_native, |_| Ok(()))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Checkpoint {
    Prepared,
    Successor,
    Retired(usize),
    BeforeTransaction,
    Transaction,
    Open,
}

fn json_with_field(text: &str, field: &str, value: JsonFacts) -> Result<String> {
    let JsonFacts::Object(mut fields) = JsonFacts::parse(text)? else {
        anyhow::bail!("archive activation image must be an object");
    };
    fields.insert(field.into(), value);
    let mut json = String::new();
    encode_facts(&JsonFacts::Object(fields), &mut json)?;
    json.push('\n');
    Ok(json)
}

fn prepare(request: &ActivationRequest<'_>) -> Result<ArchiveJournal> {
    let binding = request.binding;
    let installed = request.installed;
    let (owner, agent) = binding
        .role
        .slug
        .split_once('/')
        .context("invalid archive repository")?;
    binding.role.validate(binding.role.origin_head.len())?;
    binding.native.validate()?;
    ensure!(
        installed.source == binding.native.runtime
            && installed.session_id == binding.native.session_id
            && installed.owner.as_deref() == Some(owner)
            && installed.agent.as_deref() == Some(agent)
            && installed.branch.as_deref() == Some(binding.role.branch.as_str())
            && installed.baseline_bytes == Some(binding.installed.bytes)
            && installed.baseline_hash.as_ref() == Some(&binding.installed.sha256)
            && installed.materialized_from.as_ref() == Some(&binding.role.origin_head)
            && installed.is_active()
            && !installed.naming_ignored,
        "installed Link differs from the requested archive baseline or route"
    );
    let mut successor = installed.clone();
    successor.merge_archive = Some(binding.role.clone());
    let successor_json = format!("{}\n", successor.to_json()?);
    mergetx::checked_activation_image(request.transaction_json)?;
    let transaction_bound_json = json_with_field(
        request.transaction_json,
        "exploration",
        JsonFacts::parse(&serde_json::to_string(binding)?)?,
    )?;
    let activation = PreparedActivation {
        successor_json,
        transaction_original_json: request.transaction_json.to_owned(),
        transaction_bound_json,
    };
    activation.validate(binding)?;
    ensure!(
        request.previous_claims.len() <= 128,
        "too many archive previous claims"
    );
    let mut previous_claims = Vec::new();
    for previous in request.previous_claims {
        let selected = link::parse_archive_link_image(
            &previous.link.source,
            &previous.link.session_id,
            previous.json.clone(),
        )?;
        let claim = &selected.link;
        ensure!(
            claim.is_active()
                && claim.owner.as_deref() == Some(owner)
                && claim.agent.as_deref() == Some(agent)
                && claim.branch.as_deref() == Some(binding.role.branch.as_str()),
            "previous Link is not an ordinary claim on the archive target"
        );
        let retired_json = json_with_field(
            &selected.json,
            "superseded_by",
            JsonFacts::String(format!(
                "{}/{}",
                binding.native.runtime, binding.native.session_id
            )),
        )?;
        previous_claims.push(PreviousClaim {
            native: RuntimeLinkKey {
                runtime: claim.source.clone(),
                session_id: claim.session_id.clone(),
            },
            original_json: selected.json,
            retired_json,
        });
    }
    previous_claims.sort_by(|a, b| a.native.cmp(&b.native));
    let journal = ArchiveJournal {
        version: merge_archive::VERSION,
        binding: binding.clone(),
        phase: ArchivePhase::Preparing,
        consumed: binding.installed.clone(),
        opencode: None,
        accepted_commit: None,
        publication: None,
        landing: None,
        abort: None,
        previous_claims,
        activation: Some(activation),
        detach: None,
    };
    journal.validate(binding.role.origin_head.len())?;
    Ok(journal)
}

fn require_heads(request: &ActivationRequest<'_>) -> Result<()> {
    let role = &request.binding.role;
    require_destination_routing(request.repo, &role.branch)?;
    let (status, head, _) = request.repo.git_status_local(&[
        "rev-parse",
        "--verify",
        &format!("refs/heads/{}^{{commit}}", role.branch),
    ])?;
    ensure!(
        status == Some(0) && head.trim() == role.origin_head,
        "archive activation target is unavailable or moved from its frozen head"
    );
    Ok(())
}

fn require_endpoints(
    request: &ActivationRequest<'_>,
    preparing: &ArchiveJournal,
    control: &mergetx::ControlGuard,
    completed: bool,
) -> Result<()> {
    let activation = preparing
        .activation
        .as_ref()
        .context("archive activation intent is missing")?;
    let native = &request.binding.native;
    let successor =
        link::read_archive_link_snapshot(request.store, &native.runtime, &native.session_id)?;
    ensure!(
        successor.as_ref().map(|image| image.json.as_str())
            == Some(activation.successor_json.as_str())
            || (!completed && successor.is_none()),
        "archive successor Link is outside retained activation endpoints"
    );
    for previous in &preparing.previous_claims {
        let current = link::read_archive_link_snapshot(
            request.store,
            &previous.native.runtime,
            &previous.native.session_id,
        )?
        .context("previous archive claim is missing")?;
        ensure!(
            current.json == previous.retired_json
                || (!completed && current.json == previous.original_json),
            "previous archive claim is outside retained activation endpoints"
        );
    }
    let tx = control
        .read_activation_snapshot()?
        .context("archive activation transaction is missing")?;
    ensure!(
        tx.json == activation.transaction_bound_json
            || (!completed && tx.json == activation.transaction_original_json),
        "merge transaction is outside retained archive activation endpoints"
    );
    Ok(())
}

pub(super) fn activate_with(
    request: ActivationRequest<'_>,
    read_native: impl FnOnce(&Link) -> Result<Vec<u8>>,
    checkpoint: impl FnMut(Checkpoint) -> Result<()>,
) -> Result<ExplorationBinding> {
    require_destination_routing(request.repo, &request.binding.role.branch)?;
    let preparing = prepare(&request)?;
    let role = &request.binding.role;
    let branch = link::lock_branch(request.store, &role.slug, &role.branch)?;
    run_locked(
        request,
        preparing,
        &branch,
        read_native,
        || Ok(()),
        checkpoint,
    )
}

pub(super) fn activate_under_branch_with(
    request: ActivationRequest<'_>,
    branch: &link::BranchLock,
    read_native: impl FnOnce(&Link) -> Result<Vec<u8>>,
    before_effects: impl FnMut() -> Result<()>,
    checkpoint: impl FnMut(Checkpoint) -> Result<()>,
) -> Result<ExplorationBinding> {
    let preparing = prepare(&request)?;
    run_locked(
        request,
        preparing,
        branch,
        read_native,
        before_effects,
        checkpoint,
    )
}

fn run_locked(
    request: ActivationRequest<'_>,
    preparing: ArchiveJournal,
    branch: &link::BranchLock,
    read_native: impl FnOnce(&Link) -> Result<Vec<u8>>,
    mut before_effects: impl FnMut() -> Result<()>,
    mut checkpoint: impl FnMut(Checkpoint) -> Result<()>,
) -> Result<ExplorationBinding> {
    let role = &request.binding.role;
    let native = &request.binding.native;
    branch.require_route(request.store, &role.slug, &role.branch)?;
    let mut keys: Vec<_> = preparing
        .previous_claims
        .iter()
        .map(|claim| &claim.native)
        .collect();
    keys.push(native);
    keys.sort();
    let _links: Vec<_> = keys
        .into_iter()
        .map(|key| link::lock(request.store, &key.runtime, &key.session_id))
        .collect::<Result<_>>()?;
    let guard = ArchiveJournalGuard::acquire(request.repo.root(), &role.generation)?;
    let control = mergetx::ControlGuard::acquire(request.repo.root())?;
    require_heads(&request)?;
    let proof = archive_history::verify_chain(request.repo, &role.origin_head, &role.origin_head)?;
    ensure!(
        proof.session() == role.logical_session,
        "archive activation target has a different logical session"
    );
    archive_history::verify_frozen_source(request.source_repo, &request.binding.source.head)?;
    let snapshot = read_native(request.installed)?;
    ensure!(
        snapshot.len() <= crate::domain::storage::MAX_MATERIALIZED_BYTES,
        "archive native snapshot exceeds its byte limit"
    );
    let prefix = snapshot
        .get(..usize::try_from(request.binding.installed.bytes)?)
        .context("installed archive baseline is missing")?;
    native_archive::capture(
        prefix,
        request.binding.installed.bytes,
        &request.binding.installed.sha256,
    )?;
    let empty = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    let checked = native_archive::capture(prefix, 0, empty)?;
    ensure!(
        checked.unconsumed.is_empty(),
        "installed archive baseline ends inside a record"
    );
    let mut preparing = preparing;
    if request.binding.native.runtime == "opencode" {
        preparing.opencode = Some(native_archive::opencode::State::installed(
            prefix,
            &request.binding.native.session_id,
        )?);
    }
    before_effects()?;
    guard.recover_pending()?;
    let current = guard.read()?;
    let mut open = preparing.clone();
    open.phase = ArchivePhase::Open;
    open.activation = None;
    if current.as_ref() == Some(&open) {
        require_endpoints(&request, &preparing, &control, true)?;
        return Ok(request.binding.clone());
    }
    ensure!(
        current.as_ref().is_none_or(|current| current == &preparing),
        "archive journal belongs to a different activation or disposition"
    );
    require_endpoints(&request, &preparing, &control, false)?;
    if current.is_none() {
        let selected =
            link::read_archive_link_snapshot(request.store, &native.runtime, &native.session_id)?;
        ensure!(
            selected.is_none(),
            "an unjournaled archive successor already exists"
        );
        for previous in &preparing.previous_claims {
            let current = link::read_archive_link_snapshot(
                request.store,
                &previous.native.runtime,
                &previous.native.session_id,
            )?
            .context("previous archive claim is missing")?;
            ensure!(
                current.json == previous.original_json,
                "an unjournaled archive claim was already retired"
            );
        }
        ensure!(
            control
                .read_activation_snapshot()?
                .context("archive activation transaction is missing")?
                .json
                == request.transaction_json,
            "an unjournaled archive transaction is already bound"
        );
        guard.create(&preparing)?;
    }
    checkpoint(Checkpoint::Prepared)?;
    let activation = preparing
        .activation
        .as_ref()
        .context("archive activation intent is missing")?;
    require_heads(&request)?;
    require_endpoints(&request, &preparing, &control, false)?;
    before_effects()?;
    let successor =
        link::read_archive_link_snapshot(request.store, &native.runtime, &native.session_id)?;
    link::publish_archive_transition_locked(
        request.store,
        &native.runtime,
        &native.session_id,
        successor.as_ref().map(|image| image.json.as_str()),
        &activation.successor_json,
    )?;
    checkpoint(Checkpoint::Successor)?;
    for (index, previous) in preparing.previous_claims.iter().enumerate() {
        require_heads(&request)?;
        require_endpoints(&request, &preparing, &control, false)?;
        before_effects()?;
        let current = link::read_archive_link_snapshot(
            request.store,
            &previous.native.runtime,
            &previous.native.session_id,
        )?
        .context("previous archive claim is missing")?;
        link::publish_archive_transition_locked(
            request.store,
            &previous.native.runtime,
            &previous.native.session_id,
            Some(&current.json),
            &previous.retired_json,
        )?;
        checkpoint(Checkpoint::Retired(index))?;
    }
    checkpoint(Checkpoint::BeforeTransaction)?;
    require_heads(&request)?;
    require_endpoints(&request, &preparing, &control, false)?;
    let current = control
        .read_activation_snapshot()?
        .context("archive activation transaction is missing")?;
    before_effects()?;
    control.publish_activation_binding(&current.json, &activation.transaction_bound_json)?;
    checkpoint(Checkpoint::Transaction)?;
    require_heads(&request)?;
    require_endpoints(&request, &preparing, &control, true)?;
    before_effects()?;
    guard.replace(&preparing, &open)?;
    checkpoint(Checkpoint::Open)?;
    Ok(request.binding.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::plumbing;
    use crate::domain::merge_archive::{FrozenMergeSource, MergeArchiveRole};
    use crate::domain::native_archive::Frontier;
    use crate::domain::{meta, storage, transcript};
    use sha2::{Digest, Sha256};
    use std::path::PathBuf;

    const SESSION: &str = "agit-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    struct Fixture {
        _directory: tempfile::TempDir,
        repo: Repo,
        store: Store,
        binding: ExplorationBinding,
        installed: Link,
        transaction_json: String,
        previous: Vec<ArchiveLinkSnapshot>,
        native: PathBuf,
        original_tree: String,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let repo = Repo::init(&directory.path().join("repo")).unwrap();
            repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
            let mut metadata = meta::Meta::new(SESSION.into(), "codex".into(), "/work".into());
            metadata.turn = Some(1);
            meta::write(repo.root(), &metadata).unwrap();
            let log = transcript::wrap_lines(
                "{\"type\":\"user\",\"text\":\"inherited\"}\n",
                "codex",
                SESSION,
            );
            storage::write_snapshot(repo.root(), &log, &log).unwrap();
            std::fs::write(repo.root().join("AGENTS.md"), "Shared instructions\n").unwrap();
            repo.add_all().unwrap();
            repo.commit("activation fixture").unwrap();
            let origin = repo.git(&["rev-parse", "HEAD"]).unwrap();
            let original_tree = repo.git(&["rev-parse", "HEAD^{tree}"]).unwrap();
            repo.git(&["update-ref", "refs/heads/work", &origin])
                .unwrap();
            repo.git(&["symbolic-ref", "HEAD", "refs/heads/work"])
                .unwrap();
            let source =
                plumbing::commit_tree(&repo, &original_tree, &[&origin], "frozen source").unwrap();
            repo.git(&["update-ref", "refs/heads/source", &source])
                .unwrap();
            let native = directory.path().join("native.jsonl");
            let bytes = b"{\"type\":\"session_meta\",\"id\":\"INSTALLED\"}\n";
            std::fs::write(&native, bytes).unwrap();
            let frontier = Frontier {
                bytes: bytes.len() as u64,
                sha256: hex::encode(Sha256::digest(bytes)),
            };
            let role = MergeArchiveRole {
                generation: uuid::Uuid::now_v7().to_string(),
                slug: "alice/photo".into(),
                branch: "work".into(),
                origin_head: origin.clone(),
                logical_session: SESSION.into(),
            };
            let binding = ExplorationBinding {
                role,
                native: RuntimeLinkKey {
                    runtime: "codex".into(),
                    session_id: "INSTALLED".into(),
                },
                installed: frontier.clone(),
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
            installed.baseline_bytes = Some(frontier.bytes);
            installed.baseline_hash = Some(frontier.sha256);
            let store = Store::at(directory.path().join("store"));
            let mut previous = Vec::new();
            for id in ["OLD_Z", "OLD_A"] {
                let mut old = installed.clone();
                old.session_id = id.into();
                let json = json_with_field(
                    &old.to_json().unwrap(),
                    "future",
                    JsonFacts::Atom("9007199254740993.0".into()),
                )
                .unwrap();
                let _lock = link::lock(&store, "codex", id).unwrap();
                link::publish_archive_transition_locked(&store, "codex", id, None, &json).unwrap();
                previous.push(
                    link::read_archive_link_snapshot(&store, "codex", id)
                        .unwrap()
                        .unwrap(),
                );
            }
            let tx = merge_archive::test_activation(&binding, "{}".into());
            let transaction_json = json_with_field(
                &tx.transaction_original_json,
                "future",
                JsonFacts::Atom("9007199254740993.0000000000000001".into()),
            )
            .unwrap();
            let typed = mergetx::checked_activation_image(&transaction_json).unwrap();
            mergetx::create(repo.root(), &typed).unwrap();
            std::fs::write(
                crate::domain::repo::common_git_dir(repo.root()).join(mergetx::LOCK_FILE),
                &transaction_json,
            )
            .unwrap();
            Self {
                _directory: directory,
                repo,
                store,
                binding,
                installed,
                transaction_json,
                previous,
                native,
                original_tree,
            }
        }

        fn request(&self) -> ActivationRequest<'_> {
            ActivationRequest {
                repo: &self.repo,
                source_repo: &self.repo,
                store: &self.store,
                binding: &self.binding,
                installed: &self.installed,
                transaction_json: &self.transaction_json,
                previous_claims: &self.previous,
            }
        }

        fn run(
            &self,
            checkpoint: impl FnMut(Checkpoint) -> Result<()>,
        ) -> Result<ExplorationBinding> {
            activate_with(
                self.request(),
                |_| Ok(std::fs::read(&self.native)?),
                checkpoint,
            )
        }

        fn journal(&self) -> Option<ArchiveJournal> {
            merge_archive::read(self.repo.root(), &self.binding.role.generation).unwrap()
        }

        fn tx_path(&self) -> PathBuf {
            crate::domain::repo::common_git_dir(self.repo.root()).join(mergetx::LOCK_FILE)
        }

        fn assert_open(&self) {
            let journal = self.journal().unwrap();
            assert_eq!(journal.phase, ArchivePhase::Open);
            assert_eq!(journal.binding, self.binding);
            assert_eq!(journal.consumed, self.binding.installed);
            assert!(journal.activation.is_none());
            assert!(journal.accepted_commit.is_none());
            let selected = link::read_archive_link_snapshot(&self.store, "codex", "INSTALLED")
                .unwrap()
                .unwrap();
            assert!(
                selected
                    .link
                    .is_archive_for(&self.binding.role, "codex", "INSTALLED")
            );
            assert!(!selected.link.is_active());
            for claim in &journal.previous_claims {
                let actual = link::read_archive_link_snapshot(
                    &self.store,
                    &claim.native.runtime,
                    &claim.native.session_id,
                )
                .unwrap()
                .unwrap();
                assert_eq!(actual.json, claim.retired_json);
                assert!(!actual.link.is_active());
                assert!(actual.json.contains("9007199254740993.0"));
            }
            let tx = mergetx::ControlGuard::acquire(self.repo.root())
                .unwrap()
                .read_activation_snapshot()
                .unwrap()
                .unwrap();
            assert_eq!(tx.tx.exploration, Some(self.binding.clone()));
            assert!(tx.json.contains("9007199254740993.0000000000000001"));
            assert_eq!(
                current_head(&self.repo, "work").unwrap(),
                self.binding.role.origin_head
            );
            assert_eq!(self.repo.git(&["write-tree"]).unwrap(), self.original_tree);
            assert_eq!(self.repo.git(&["status", "--porcelain"]).unwrap(), "");
        }

        fn authority_bytes(&self) -> Vec<Vec<u8>> {
            let mut images = vec![
                std::fs::read(self.tx_path()).unwrap(),
                std::fs::read(&self.native).unwrap(),
            ];
            for id in ["OLD_A", "OLD_Z", "INSTALLED"] {
                images.push(
                    std::fs::read(link::link_path(&self.store, "codex", id)).unwrap_or_default(),
                );
            }
            images
        }
    }

    #[test]
    fn activation_rejects_a_branch_guard_for_another_route() {
        let fixture = Fixture::new();
        let before = fixture.authority_bytes();
        let guard = link::lock_branch(&fixture.store, "alice/photo", "other").unwrap();
        assert!(
            activate_under_branch_with(
                fixture.request(),
                &guard,
                |_| Ok(std::fs::read(&fixture.native)?),
                || Ok(()),
                |_| Ok(())
            )
            .is_err()
        );
        assert_eq!(fixture.authority_bytes(), before);
        assert!(fixture.journal().is_none());
    }

    #[test]
    fn activation_publishes_explicit_role_and_preserves_unknown_facts() {
        let fixture = Fixture::new();
        let before_native = std::fs::read(&fixture.native).unwrap();
        let mut steps = Vec::new();
        assert_eq!(
            fixture
                .run(|step| {
                    steps.push(step);
                    Ok(())
                })
                .unwrap(),
            fixture.binding
        );
        assert_eq!(
            steps,
            [
                Checkpoint::Prepared,
                Checkpoint::Successor,
                Checkpoint::Retired(0),
                Checkpoint::Retired(1),
                Checkpoint::BeforeTransaction,
                Checkpoint::Transaction,
                Checkpoint::Open
            ]
        );
        fixture.assert_open();
        assert_eq!(std::fs::read(&fixture.native).unwrap(), before_native);
        let before = fixture.authority_bytes();
        fixture
            .run(|_| anyhow::bail!("completed activation must not publish again"))
            .unwrap();
        assert_eq!(fixture.authority_bytes(), before);
    }

    #[test]
    fn activation_replays_every_retained_publication_boundary() {
        for failed in [
            Checkpoint::Prepared,
            Checkpoint::Successor,
            Checkpoint::Retired(0),
            Checkpoint::Retired(1),
            Checkpoint::BeforeTransaction,
            Checkpoint::Transaction,
            Checkpoint::Open,
        ] {
            let fixture = Fixture::new();
            assert!(
                fixture
                    .run(|step| {
                        ensure!(step != failed, "injected activation interruption");
                        Ok(())
                    })
                    .is_err()
            );
            let interrupted = fixture.journal().unwrap();
            assert_eq!(interrupted.consumed, fixture.binding.installed);
            assert_eq!(
                interrupted.phase,
                if failed == Checkpoint::Open {
                    ArchivePhase::Open
                } else {
                    ArchivePhase::Preparing
                }
            );
            let mut native = std::fs::read(&fixture.native).unwrap();
            native.extend_from_slice(
                b"{\"type\":\"tool_result\",\"text\":\"pending exploration\"}\n",
            );
            std::fs::write(&fixture.native, &native).unwrap();
            fixture.run(|_| Ok(())).unwrap();
            fixture.assert_open();
            assert_eq!(std::fs::read(&fixture.native).unwrap(), native);
        }
    }

    #[test]
    fn activation_refuses_foreign_links_and_transaction_without_advancing_intent() {
        for kind in ["successor", "old", "transaction", "transaction_precision"] {
            let fixture = Fixture::new();
            assert!(
                fixture
                    .run(|step| {
                        ensure!(
                            step != Checkpoint::Successor,
                            "injected activation interruption"
                        );
                        Ok(())
                    })
                    .is_err()
            );
            let path = match kind {
                "successor" => link::link_path(&fixture.store, "codex", "INSTALLED"),
                "old" => link::link_path(&fixture.store, "codex", "OLD_A"),
                _ => fixture.tx_path(),
            };
            let original = std::fs::read_to_string(&path).unwrap();
            let foreign = if kind == "transaction_precision" {
                original.replace(
                    "9007199254740993.0000000000000001",
                    "9007199254740993.0000000000000002",
                )
            } else {
                json_with_field(&original, "future_change", JsonFacts::Atom("true".into())).unwrap()
            };
            std::fs::write(&path, foreign).unwrap();
            let before = fixture.authority_bytes();
            let journal = fixture.journal();
            assert!(fixture.run(|_| Ok(())).is_err());
            assert_eq!(fixture.authority_bytes(), before);
            assert_eq!(fixture.journal(), journal);
            std::fs::write(&path, original).unwrap();
            fixture.run(|_| Ok(())).unwrap();
            fixture.assert_open();
        }
    }

    #[test]
    fn activation_refuses_unjournaled_successor_and_retirement() {
        for successor in [true, false] {
            let fixture = Fixture::new();
            let planned = prepare(&fixture.request()).unwrap();
            if successor {
                let _lock = link::lock(&fixture.store, "codex", "INSTALLED").unwrap();
                link::publish_archive_transition_locked(
                    &fixture.store,
                    "codex",
                    "INSTALLED",
                    None,
                    &planned.activation.as_ref().unwrap().successor_json,
                )
                .unwrap();
            } else {
                let previous = &planned.previous_claims[0];
                std::fs::write(
                    link::link_path(
                        &fixture.store,
                        &previous.native.runtime,
                        &previous.native.session_id,
                    ),
                    &previous.retired_json,
                )
                .unwrap();
            }
            let before = fixture.authority_bytes();
            assert!(fixture.run(|_| Ok(())).is_err());
            assert!(fixture.journal().is_none());
            assert_eq!(fixture.authority_bytes(), before);
        }
    }

    #[test]
    fn activation_requires_exact_transaction_generation_selection_and_progress() {
        for field in [
            "generation",
            "target_head",
            "source_head",
            "source",
            "summary",
            "exploration",
        ] {
            let fixture = Fixture::new();
            let replacement = if field == "exploration" {
                JsonFacts::parse(&serde_json::to_string(&fixture.binding).unwrap()).unwrap()
            } else {
                JsonFacts::String("changed".into())
            };
            std::fs::write(
                fixture.tx_path(),
                json_with_field(&fixture.transaction_json, field, replacement).unwrap(),
            )
            .unwrap();
            let before = fixture.authority_bytes();
            assert!(fixture.run(|_| Ok(())).is_err());
            assert!(fixture.journal().is_none());
            assert_eq!(fixture.authority_bytes(), before);
        }
    }

    #[test]
    fn activation_checks_baseline_and_exact_target_after_locking() {
        for change in ["prefix", "short", "target", "identity"] {
            let fixture = Fixture::new();
            match change {
                "prefix" => {
                    let mut bytes = std::fs::read(&fixture.native).unwrap();
                    bytes[3] = b'X';
                    std::fs::write(&fixture.native, bytes).unwrap();
                }
                "short" => std::fs::write(&fixture.native, b"{}").unwrap(),
                "target" => {
                    fixture
                        .repo
                        .git(&[
                            "update-ref",
                            "refs/heads/work",
                            &fixture.binding.source.head,
                        ])
                        .unwrap();
                }
                _ => {
                    let mut request_binding = fixture.binding.clone();
                    request_binding.role.logical_session = format!("agit-{}", "b".repeat(40));
                    let mut request = fixture.request();
                    request.binding = &request_binding;
                    assert!(
                        activate_with(request, |_| Ok(std::fs::read(&fixture.native)?), |_| Ok(()))
                            .is_err()
                    );
                    assert!(fixture.journal().is_none());
                    continue;
                }
            }
            let before = fixture.authority_bytes();
            assert!(fixture.run(|_| Ok(())).is_err());
            assert_eq!(fixture.authority_bytes(), before);
            assert!(fixture.journal().is_none());
        }
    }

    #[test]
    fn activation_retains_frozen_source_when_its_branch_advances() {
        let fixture = Fixture::new();
        let later = plumbing::commit_tree(
            &fixture.repo,
            &fixture.original_tree,
            &[&fixture.binding.source.head],
            "later source",
        )
        .unwrap();
        fixture
            .repo
            .git(&["update-ref", "refs/heads/source", &later])
            .unwrap();
        fixture.run(|_| Ok(())).unwrap();
        fixture.assert_open();
        assert_eq!(
            fixture.journal().unwrap().binding.source.head,
            fixture.binding.source.head
        );
        assert_eq!(current_head(&fixture.repo, "source").unwrap(), later);
    }

    #[test]
    fn activation_rechecks_target_and_foreign_authority_before_transaction_binding() {
        for change in ["target", "transaction", "successor"] {
            let fixture = Fixture::new();
            assert!(
                fixture
                    .run(|step| {
                        if step == Checkpoint::BeforeTransaction {
                            match change {
                                "target" => {
                                    fixture.repo.git(&[
                                        "update-ref",
                                        "refs/heads/work",
                                        &fixture.binding.source.head,
                                    ])?;
                                }
                                "transaction" => std::fs::write(
                                    fixture.tx_path(),
                                    json_with_field(
                                        &fixture.transaction_json,
                                        "summary",
                                        JsonFacts::String("foreign progress".into()),
                                    )?,
                                )?,
                                _ => std::fs::write(
                                    link::link_path(&fixture.store, "codex", "INSTALLED"),
                                    "{}\n",
                                )?,
                            }
                        }
                        Ok(())
                    })
                    .is_err()
            );
            assert_eq!(fixture.journal().unwrap().phase, ArchivePhase::Preparing);
            assert_eq!(
                fixture.journal().unwrap().consumed,
                fixture.binding.installed
            );
            assert!(
                mergetx::read(fixture.repo.root())
                    .unwrap()
                    .unwrap()
                    .exploration
                    .is_none()
            );
        }
    }

    #[test]
    fn activation_refuses_duplicate_missing_and_corrupt_participants() {
        for change in ["duplicate", "missing", "corrupt", "wrong_route"] {
            let mut fixture = Fixture::new();
            match change {
                "duplicate" => fixture.previous.push(fixture.previous[0].clone()),
                "missing" => {
                    std::fs::remove_file(link::link_path(&fixture.store, "codex", "OLD_Z")).unwrap()
                }
                "corrupt" => std::fs::write(
                    link::link_path(&fixture.store, "codex", "OLD_Z"),
                    "not JSON",
                )
                .unwrap(),
                _ => {
                    fixture.previous[0].json = json_with_field(
                        &fixture.previous[0].json,
                        "owner",
                        JsonFacts::String("elsewhere".into()),
                    )
                    .unwrap();
                }
            }
            let before = fixture.authority_bytes();
            assert!(fixture.run(|_| Ok(())).is_err());
            assert!(fixture.journal().is_none());
            assert_eq!(fixture.authority_bytes(), before);
        }
    }

    #[test]
    fn activation_refuses_missing_frozen_source_evidence() {
        let fixture = Fixture::new();
        let head = &fixture.binding.source.head;
        let object = crate::domain::repo::common_git_dir(fixture.repo.root())
            .join("objects")
            .join(&head[..2])
            .join(&head[2..]);
        let bytes = std::fs::read(&object).unwrap();
        std::fs::remove_file(&object).unwrap();
        let before = fixture.authority_bytes();
        assert!(fixture.run(|_| Ok(())).is_err());
        assert_eq!(fixture.authority_bytes(), before);
        assert!(fixture.journal().is_none());
        std::fs::write(object, bytes).unwrap();
        fixture.run(|_| Ok(())).unwrap();
        fixture.assert_open();
    }

    #[test]
    fn activation_accepts_file_and_historical_frozen_source_selectors() {
        for selection in ["main", "tag", "historical", "version"] {
            let mut fixture = Fixture::new();
            let selector = match selection {
                "main" => {
                    let metadata = meta::to_text(&meta::Meta::new_file_line()).unwrap();
                    let empty_tree = fixture
                        .repo
                        .git(&["hash-object", "-t", "tree", "-w", "--stdin"])
                        .unwrap();
                    let tree = plumbing::tree_apply_owned(
                        &fixture.repo,
                        &empty_tree,
                        vec![
                            (meta::FILE.into(), Some(metadata.into_bytes())),
                            ("AGENTS.md".into(), Some(b"Shared instructions\n".to_vec())),
                        ],
                    )
                    .unwrap();
                    fixture.binding.source.head =
                        plumbing::commit_tree(&fixture.repo, &tree, &[], "file source").unwrap();
                    fixture
                        .repo
                        .git(&[
                            "update-ref",
                            "refs/heads/main",
                            &fixture.binding.source.head,
                        ])
                        .unwrap();
                    "main".to_owned()
                }
                "tag" => {
                    fixture
                        .repo
                        .git(&["tag", "release-tag", &fixture.binding.source.head])
                        .unwrap();
                    "release-tag".into()
                }
                "historical" => {
                    fixture.binding.source.head = fixture.binding.role.origin_head.clone();
                    "source#1".into()
                }
                _ => format!("agit-{}", fixture.binding.source.head),
            };
            fixture.binding.source.reference = format!("alice/photo@{selector}");
            fixture.binding.source.branch = Some(if selection == "historical" {
                "source".into()
            } else {
                selector
            });
            let spec = crate::domain::refs::parse(&fixture.binding.source.reference).unwrap();
            let resolved = crate::domain::refs::resolve(&fixture.repo, &spec).unwrap();
            assert_eq!(resolved.sha, fixture.binding.source.head);
            fixture.transaction_json = json_with_field(
                &merge_archive::test_activation(&fixture.binding, "{}".into())
                    .transaction_original_json,
                "future",
                JsonFacts::Atom("9007199254740993.0000000000000001".into()),
            )
            .unwrap();
            std::fs::write(fixture.tx_path(), &fixture.transaction_json).unwrap();
            fixture.run(|_| Ok(())).unwrap();
            fixture.assert_open();
            assert_eq!(
                fixture.journal().unwrap().binding.source,
                fixture.binding.source
            );
            let mut role = fixture.binding.role.clone();
            role.branch = "main".into();
            assert!(role.validate(role.origin_head.len()).is_err());
        }
    }

    #[test]
    fn activation_refuses_corrupt_journal_and_transaction_carriers() {
        for carrier in ["journal", "transaction", "oversized", "duplicate"] {
            let fixture = Fixture::new();
            assert!(
                fixture
                    .run(|step| {
                        ensure!(step != Checkpoint::Prepared, "injected interruption");
                        Ok(())
                    })
                    .is_err()
            );
            let journal_path = crate::domain::repo::common_git_dir(fixture.repo.root())
                .join(merge_archive::DIRECTORY)
                .join(format!("{}.json", fixture.binding.role.generation));
            let path = if carrier == "journal" {
                journal_path.clone()
            } else {
                fixture.tx_path()
            };
            let before_corruption = std::fs::read(&path).unwrap();
            let bad = match carrier {
                "oversized" => " ".repeat(256 * 1024 + 1),
                "duplicate" => format!(
                    "{{\"target\":\"foreign\",{}",
                    fixture.transaction_json.strip_prefix('{').unwrap()
                ),
                _ => "not JSON".into(),
            };
            std::fs::write(&path, bad).unwrap();
            let before = fixture.authority_bytes();
            let journal_bytes = std::fs::read(&journal_path).unwrap();
            assert!(fixture.run(|_| Ok(())).is_err());
            assert_eq!(fixture.authority_bytes(), before);
            assert_eq!(std::fs::read(&journal_path).unwrap(), journal_bytes);
            std::fs::write(path, before_corruption).unwrap();
            fixture.run(|_| Ok(())).unwrap();
            fixture.assert_open();
        }
    }

    #[test]
    fn activation_rejects_valid_digest_for_malformed_installed_records() {
        for bytes in [
            b"{\"same\":1,\"same\":2}\n".as_slice(),
            b"[]\n",
            b"{\"open\":",
        ] {
            let mut fixture = Fixture::new();
            std::fs::write(&fixture.native, bytes).unwrap();
            fixture.binding.installed = Frontier {
                bytes: bytes.len() as u64,
                sha256: hex::encode(Sha256::digest(bytes)),
            };
            fixture.installed.baseline_bytes = Some(fixture.binding.installed.bytes);
            fixture.installed.baseline_hash = Some(fixture.binding.installed.sha256.clone());
            let before = fixture.authority_bytes();
            assert!(fixture.run(|_| Ok(())).is_err());
            assert!(fixture.journal().is_none());
            assert_eq!(fixture.authority_bytes(), before);
        }
    }

    #[cfg(unix)]
    #[test]
    fn activation_transaction_redirect_refuses_without_touching_its_target() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let decoy = fixture._directory.path().join("decoy.json");
        std::fs::rename(fixture.tx_path(), &decoy).unwrap();
        symlink(&decoy, fixture.tx_path()).unwrap();
        let before = std::fs::read(&decoy).unwrap();
        assert!(fixture.run(|_| Ok(())).is_err());
        assert_eq!(std::fs::read(&decoy).unwrap(), before);
        assert!(fixture.journal().is_none());
        assert!(
            link::read_archive_link_snapshot(&fixture.store, "codex", "INSTALLED")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn activation_rechecks_configured_worktree_before_binding() {
        let fixture = Fixture::new();
        let decoy = Fixture::new();
        let before = decoy.authority_bytes();
        assert!(
            fixture
                .run(|step| {
                    if step == Checkpoint::BeforeTransaction {
                        fixture.repo.git(&[
                            "config",
                            "core.worktree",
                            decoy.repo.root().to_str().unwrap(),
                        ])?;
                    }
                    Ok(())
                })
                .is_err()
        );
        assert_eq!(decoy.authority_bytes(), before);
        fixture
            .repo
            .git(&["config", "--unset", "core.worktree"])
            .unwrap();
        assert_eq!(fixture.journal().unwrap().phase, ArchivePhase::Preparing);
        assert!(
            mergetx::read(fixture.repo.root())
                .unwrap()
                .unwrap()
                .exploration
                .is_none()
        );
        fixture.run(|_| Ok(())).unwrap();
        fixture.assert_open();
    }

    #[test]
    fn activation_frozen_source_requires_its_own_event_objects() {
        let mut fixture = Fixture::new();
        let log = transcript::wrap_lines(
            "{\"type\":\"user\",\"text\":\"source-only evidence\"}\n",
            "codex",
            SESSION,
        );
        let files = storage::snapshot_files(&log, &log).unwrap();
        let event = files
            .iter()
            .find(|(path, _)| path.starts_with("events/"))
            .unwrap()
            .0
            .clone();
        let tree = plumbing::tree_apply_owned(
            &fixture.repo,
            &fixture.binding.source.head,
            files
                .into_iter()
                .map(|(path, bytes)| (path, Some(bytes)))
                .collect(),
        )
        .unwrap();
        fixture.binding.source.head = plumbing::commit_tree(
            &fixture.repo,
            &tree,
            &[&fixture.binding.source.head],
            "source-only event",
        )
        .unwrap();
        fixture.transaction_json =
            merge_archive::test_activation(&fixture.binding, "{}".into()).transaction_original_json;
        std::fs::write(fixture.tx_path(), &fixture.transaction_json).unwrap();
        let blob = fixture
            .repo
            .git(&[
                "rev-parse",
                &format!("{}:{event}", fixture.binding.source.head),
            ])
            .unwrap();
        let path = crate::domain::repo::common_git_dir(fixture.repo.root())
            .join("objects")
            .join(&blob[..2])
            .join(&blob[2..]);
        let bytes = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        archive_history::verify_chain(
            &fixture.repo,
            &fixture.binding.role.origin_head,
            &fixture.binding.role.origin_head,
        )
        .unwrap();
        let before = fixture.authority_bytes();
        assert!(fixture.run(|_| Ok(())).is_err());
        assert_eq!(fixture.authority_bytes(), before);
        assert!(fixture.journal().is_none());
        std::fs::write(path, bytes).unwrap();
        fixture.run(|_| Ok(())).unwrap();
        assert_eq!(fixture.journal().unwrap().phase, ArchivePhase::Open);
    }
}
