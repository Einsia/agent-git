//! Installs a selected merge VIEW and durably activates its native archive authority.
//!
//! This API installs authority without spawning a runtime. The branch guard stays
//! held from strict claim discovery through activation. A failure before Preparing can leave an
//! unclaimed native artifact; only a retained journal selects an installed instance on recovery.

use std::path::Path;

use anyhow::{Context, ensure};
use sha2::{Digest, Sha256};

use super::{activation, file_agent, read_native, require_destination_routing};
use crate::Result;
use crate::adapter::{self, Installed};
use crate::domain::link::{self, ArchiveLinkSnapshot, Link};
use crate::domain::merge_archive::{
    self, ArchivePhase, ExplorationBinding, FrozenFileTarget, FrozenMergeSource, MergeArchiveRole,
    RuntimeLinkKey,
};
use crate::domain::metadata_facts::JsonFacts;
use crate::domain::native_archive::Frontier;
use crate::domain::{
    archive_history, install, mergetx, meta, repo::Repo, storage, store::Store, transcript,
};

pub struct PreparationRequest<'a> {
    pub repo: &'a Repo,
    pub source_repo: &'a Repo,
    pub store: &'a Store,
    pub slug: &'a str,
    pub transaction_json: &'a str,
    pub runtime: &'a str,
    pub cwd: &'a Path,
}

pub struct PreparedExploration {
    pub binding: ExplorationBinding,
    pub reused: bool,
    pub lossy: bool,
    /// Recovery reuses the retained installation instead of hydrating VIEW again.
    pub unresolved_placeholders: Option<usize>,
}

/// The returned native identity is ready for later launch admission, not permission to spawn.
pub fn prepare(request: PreparationRequest<'_>) -> Result<PreparedExploration> {
    let file = mergetx::checked_activation_image(request.transaction_json)?.mode
        == Some(mergetx::Mode::FileAgent);
    prepare_with(
        request,
        |text, from, to, cwd| {
            if file && to == "claude-code" {
                ensure!(
                    text.is_empty(),
                    "fresh file exploration contains prior context"
                );
                file_agent::install_fresh_claude(cwd)
            } else if file && to == "opencode" {
                ensure!(
                    text.is_empty(),
                    "fresh file exploration contains prior context"
                );
                let (id, payload) = empty_opencode_bootstrap(cwd)?;
                adapter::get(to)?.install(&payload, &id, cwd)
            } else {
                install::install(text, from, to, cwd).map(|(installed, _)| installed)
            }
        },
        read_native,
        |_| Ok(()),
    )
}

/// Empty OpenCode exploration still needs an importable session object with one native identity.
fn empty_opencode_bootstrap(cwd: &Path) -> Result<(String, String)> {
    let runtime = adapter::get("opencode")?;
    let id = runtime.mint_id();
    let empty = runtime.parse("")?;
    let payload = runtime.render(&empty, &id, cwd)?;
    Ok((id, payload))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Checkpoint {
    BranchLocked,
    Installed,
    Activation(activation::Checkpoint),
}

fn require_transaction(request: &PreparationRequest<'_>, expected: &str) -> Result<()> {
    let control = mergetx::ControlGuard::acquire(request.repo.root())?;
    ensure!(
        control
            .read_activation_snapshot()?
            .as_ref()
            .map(|image| image.json.as_str())
            == Some(expected),
        "merge transaction changed before archive preparation"
    );
    Ok(())
}

fn claims(request: &PreparationRequest<'_>, branch: &str) -> Result<Vec<ArchiveLinkSnapshot>> {
    let (owner, agent) = request
        .slug
        .split_once('/')
        .context("invalid archive repository")?;
    link::archive_claims_for_branch(request.store, owner, agent, branch)
}

fn require_claims(
    request: &PreparationRequest<'_>,
    branch: &str,
    selected: &[ArchiveLinkSnapshot],
    committed: &str,
    read: &impl Fn(&Link) -> Result<Vec<u8>>,
    recovery: bool,
    check_native: bool,
) -> Result<()> {
    let current = claims(request, branch)?;
    ensure!(
        current.iter().all(|image| selected
            .iter()
            .any(|old| old.link.source == image.link.source
                && old.link.session_id == image.link.session_id
                && old.json == image.json))
            && (recovery || current.len() == selected.len()),
        "archive target claims changed during preparation"
    );
    for image in selected.iter().filter(|_| check_native) {
        crate::commands::resume::require_merge_claim_with_bytes(
            request.repo,
            committed,
            &image.link,
            request.slug,
            branch,
            &read(&image.link)?,
            false,
        )?;
    }
    Ok(())
}

fn prepare_with(
    request: PreparationRequest<'_>,
    install: impl FnOnce(&str, &str, &str, &Path) -> Result<Installed>,
    read: impl Fn(&Link) -> Result<Vec<u8>>,
    mut checkpoint: impl FnMut(Checkpoint) -> Result<()>,
) -> Result<PreparedExploration> {
    let transaction = mergetx::checked_activation_image(request.transaction_json)?;
    ensure!(
        matches!(
            transaction.mode,
            Some(mergetx::Mode::SessionAgent | mergetx::Mode::FileAgent)
        ),
        "archive preparation requires an agent transaction"
    );
    ensure!(
        request.cwd.is_absolute(),
        "archive installation cwd must be absolute"
    );
    ensure!(
        request.cwd.to_str().is_some(),
        "archive installation cwd must be valid Unicode"
    );
    let runtime = adapter::normalize(request.runtime)?;
    ensure!(
        adapter::get(runtime)?.capability() == adapter::Capability::Resumable,
        "archive exploration requires a resumable native runtime"
    );
    let seed = if transaction.mode == Some(mergetx::Mode::FileAgent) {
        let generation = transaction
            .generation
            .as_deref()
            .context("file merge has no generation")?;
        let seed = if transaction.exploration.is_some() {
            file_agent::read_seed(request.repo, generation)?
                .context("bound file archive seed is missing")?
        } else {
            file_agent::prepare_seed(file_agent::SeedRequest {
                repo: request.repo,
                store: request.store,
                slug: request.slug,
                transaction_json: request.transaction_json,
                runtime,
                cwd: request.cwd,
            })?
        };
        let original = mergetx::checked_activation_image(&seed.transaction_json)?;
        let mut unbound = transaction.clone();
        unbound.exploration = None;
        ensure!(
            original.same_instance(&unbound)
                && seed.role.slug == request.slug
                && seed.runtime == runtime
                && Some(seed.cwd.as_str()) == request.cwd.to_str(),
            "file archive retry differs from the retained seed selection"
        );
        Some(seed)
    } else {
        None
    };
    let file_target = seed.as_ref().map(|seed| FrozenFileTarget {
        branch: seed.target.clone(),
        head: seed.target_head.clone(),
    });
    let role = if let Some(seed) = &seed {
        seed.role.clone()
    } else {
        MergeArchiveRole {
            generation: transaction
                .generation
                .clone()
                .context("archive transaction has no generation")?,
            slug: request.slug.to_owned(),
            branch: transaction.target.clone(),
            origin_head: transaction.target_head.clone(),
            logical_session: String::new(),
        }
    };
    let source = FrozenMergeSource {
        reference: transaction.source.clone(),
        slug: transaction
            .source_repo
            .clone()
            .context("archive transaction has no frozen source repository")?,
        branch: transaction.source_branch.clone(),
        head: transaction.source_head.clone(),
        base: (!transaction.base.is_empty()).then(|| transaction.base.clone()),
    };
    source.validate(role.origin_head.len())?;
    require_destination_routing(request.repo, &role.branch)?;
    let proof = archive_history::verify_chain(request.repo, &role.origin_head, &role.origin_head)?;
    let role = MergeArchiveRole {
        logical_session: proof.session().to_owned(),
        ..role
    };
    role.validate(role.origin_head.len())?;
    archive_history::verify_frozen_source(request.source_repo, &source.head)?;
    let mut names = vec![role.branch.clone(), transaction.target.clone()];
    names.sort();
    names.dedup();
    let branches = names
        .into_iter()
        .map(|name| {
            let guard = link::lock_branch(request.store, request.slug, &name)?;
            Ok((name, guard))
        })
        .collect::<Result<Vec<_>>>()?;
    let branch_guard = &branches
        .iter()
        .find(|(name, _)| name == &role.branch)
        .unwrap()
        .1;
    let target_guard = file_target.as_ref().map(|target| {
        &branches
            .iter()
            .find(|(name, _)| name == &target.branch)
            .unwrap()
            .1
    });
    require_destination_routing(request.repo, &role.branch)?;
    checkpoint(Checkpoint::BranchLocked)?;
    ensure!(
        super::current_head(request.repo, &role.branch)? == role.origin_head,
        "archive preparation target moved from its frozen head"
    );
    if let Some(target) = &file_target {
        require_destination_routing(request.repo, &target.branch)?;
        ensure!(
            super::current_head(request.repo, &target.branch)? == target.head,
            "file merge target moved before native installation"
        );
    }
    require_transaction(&request, request.transaction_json)?;
    let previous = merge_archive::read_preparation_intent(request.repo.root(), &role.generation)?;
    let open_replay = previous
        .as_ref()
        .is_some_and(|journal| journal.phase == ArchivePhase::Open);
    let metadata = meta::read_at_ref_result(request.repo, &role.origin_head)?
        .context("archive target metadata is missing")?;
    let committed =
        storage::materialize_at(request.repo.root(), &role.origin_head, meta::LOG_FILE)?;
    let lossy = adapter::is_lossy_conversion(&metadata.runtime, runtime);
    let (binding, installed, previous_claims, original_json, unresolved) = if let Some(journal) =
        previous
    {
        ensure!(
            journal.binding.role == role
                && journal.binding.file_target == file_target
                && journal.binding.source == source
                && journal.binding.native.runtime == runtime
                && matches!(journal.phase, ArchivePhase::Preparing | ArchivePhase::Open),
            "retained archive preparation belongs to another selection or disposition"
        );
        let previous_claims = journal
            .previous_claims
            .iter()
            .map(|claim| {
                link::parse_archive_link_image(
                    &claim.native.runtime,
                    &claim.native.session_id,
                    claim.original_json.clone(),
                )
            })
            .collect::<Result<Vec<_>>>()?;
        let (installed, original_json) = match journal.activation {
            Some(activation) => {
                ensure!(
                    request.transaction_json == activation.transaction_original_json
                        || request.transaction_json == activation.transaction_bound_json,
                    "preparing transaction differs from its retained endpoints"
                );
                let mut installed = link::parse_archive_link_image(
                    &journal.binding.native.runtime,
                    &journal.binding.native.session_id,
                    activation.successor_json,
                )?
                .link;
                installed.merge_archive = None;
                (installed, activation.transaction_original_json)
            }
            None => {
                ensure!(
                    transaction.exploration.as_ref() == Some(&journal.binding),
                    "open archive has no matching transaction binding"
                );
                let mut installed = link::read_archive_link_snapshot(
                    request.store,
                    &journal.binding.native.runtime,
                    &journal.binding.native.session_id,
                )?
                .context("open archive Link is missing")?
                .link;
                installed.merge_archive = None;
                let JsonFacts::Object(mut fields) = JsonFacts::parse(request.transaction_json)?
                else {
                    unreachable!()
                };
                fields.insert("exploration".into(), JsonFacts::Atom("null".into()));
                let mut original_json = String::new();
                JsonFacts::Object(fields).write_json(&mut original_json)?;
                original_json.push('\n');
                (installed, original_json)
            }
        };
        ensure!(
            installed.cwd.as_deref() == request.cwd.to_str(),
            "archive retry requested a different installation cwd"
        );
        (
            journal.binding,
            installed,
            previous_claims,
            original_json,
            None,
        )
    } else {
        ensure!(
            transaction.exploration.is_none(),
            "archive transaction binding has no retained preparation"
        );
        let retained =
            merge_archive::probe_for_branch(request.repo.root(), request.slug, &role.branch)?;
        ensure!(
            retained.iter().all(|journal| matches!(
                journal.phase,
                ArchivePhase::Landed { .. } | ArchivePhase::Aborted | ArchivePhase::Detached
            ) && !journal.publication_pending
                && !journal.detach_pending),
            "another archive generation still owns unresolved preparation on this branch"
        );
        let previous_claims = claims(&request, &role.branch)?;
        ensure!(
            file_target.is_none() || previous_claims.is_empty(),
            "file exploration cannot replace an existing native claim"
        );
        let claim_guards = previous_claims
            .iter()
            .map(|image| link::lock(request.store, &image.link.source, &image.link.session_id))
            .collect::<Result<Vec<_>>>()?;
        require_claims(
            &request,
            &role.branch,
            &previous_claims,
            &committed,
            &read,
            false,
            true,
        )?;
        let view =
            storage::materialize_at(request.repo.root(), &role.origin_head, meta::VIEW_FILE)?;
        let text = transcript::unwrap_strict(&view)?;
        let text = transcript::restore_bootstrap(&text, &committed, &metadata.runtime);
        ensure!(
            file_target.is_some() || !text.trim().is_empty(),
            "archive merge VIEW is empty"
        );
        let (install_text, unresolved) = if file_target.is_some() {
            ensure!(
                text.is_empty() && committed.is_empty(),
                "file exploration seed contains transcript context"
            );
            (String::new(), 0)
        } else {
            let hydrated =
                crate::domain::secret_filter::RepositoryDictionary::open(request.repo.root())?
                    .hydrate_jsonl(&text)?;
            (hydrated.text, hydrated.unresolved)
        };
        ensure!(
            install_text.len() <= storage::MAX_MATERIALIZED_BYTES,
            "hydrated archive VIEW exceeds its byte limit"
        );
        require_destination_routing(request.repo, &role.branch)?;
        require_transaction(&request, request.transaction_json)?;
        let receipt = install(&install_text, &metadata.runtime, runtime, request.cwd)?;
        let id = receipt
            .path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(adapter::session_id_from_stem)
            .context("installed archive runtime has no native ID")?;
        let native = RuntimeLinkKey {
            runtime: runtime.into(),
            session_id: id,
        };
        native.validate()?;
        let mut installed = Link::new(runtime, &native.session_id, Some(request.cwd));
        let (owner, agent) = request
            .slug
            .split_once('/')
            .context("invalid archive repository")?;
        installed.owner = Some(owner.into());
        installed.agent = Some(agent.into());
        installed.branch = Some(role.branch.clone());
        installed.materialized_from = Some(role.origin_head.clone());
        let bytes = read(&installed)?;
        ensure!(
            bytes.len() <= storage::MAX_MATERIALIZED_BYTES,
            "installed archive runtime exceeds its byte limit"
        );
        let empty = hex::encode(Sha256::digest([]));
        let captured = crate::domain::native_archive::capture(&bytes, 0, &empty)?;
        ensure!(
            (file_target.is_some() || captured.record_count > 0) && captured.unconsumed.is_empty(),
            "installed archive runtime has no complete baseline"
        );
        let frontier = Frontier {
            bytes: bytes.len() as u64,
            sha256: hex::encode(Sha256::digest(&bytes)),
        };
        installed.baseline_bytes = Some(frontier.bytes);
        installed.baseline_hash = Some(frontier.sha256.clone());
        checkpoint(Checkpoint::Installed)?;
        require_claims(
            &request,
            &role.branch,
            &previous_claims,
            &committed,
            &read,
            false,
            true,
        )?;
        drop(claim_guards);
        (
            ExplorationBinding {
                file_target: file_target.clone(),
                role,
                source,
                native,
                installed: frontier,
            },
            installed,
            previous_claims,
            request.transaction_json.to_owned(),
            Some(unresolved),
        )
    };
    let reused = unresolved.is_none();
    let activated = activation::activate_under_branch_with(
        activation::ActivationRequest {
            repo: request.repo,
            source_repo: request.source_repo,
            store: request.store,
            binding: &binding,
            installed: &installed,
            transaction_json: &original_json,
            previous_claims: &previous_claims,
        },
        branch_guard,
        target_guard,
        &read,
        || {
            require_claims(
                &request,
                &binding.role.branch,
                &previous_claims,
                &committed,
                &read,
                true,
                !open_replay,
            )
        },
        |step| checkpoint(Checkpoint::Activation(step)),
    )?;
    Ok(PreparedExploration {
        binding: activated,
        reused,
        lossy,
        unresolved_placeholders: unresolved,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const SESSION: &str = "agit-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const VISIBLE: &str = "{\"type\":\"user\",\"text\":\"visible installation context\"}\n";

    struct FileFixture {
        directory: tempfile::TempDir,
        repo: Repo,
        store: Store,
        json: String,
        head: String,
        installs: AtomicUsize,
    }

    impl FileFixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let repo = Repo::init(&directory.path().join("repo")).unwrap();
            repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
            meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
            std::fs::write(repo.root().join("AGENTS.md"), "Shared instructions\n").unwrap();
            repo.add_all().unwrap();
            repo.commit("file line").unwrap();
            let head = repo.git(&["rev-parse", "HEAD"]).unwrap();
            repo.git(&["update-ref", "refs/heads/main", &head]).unwrap();
            repo.git(&["symbolic-ref", "HEAD", "refs/heads/main"])
                .unwrap();
            let tx = mergetx::Tx {
                mode: Some(mergetx::Mode::FileAgent),
                exploration: None,
                generation: Some(uuid::Uuid::now_v7().to_string()),
                target: "main".into(),
                source: "alice/photo@source".into(),
                source_repo: Some("alice/photo".into()),
                source_branch: Some("source".into()),
                base: head.clone(),
                target_head: head.clone(),
                source_head: head.clone(),
                picked: vec![],
                summary: None,
            };
            mergetx::create(repo.root(), &tx).unwrap();
            let json = mergetx::ControlGuard::acquire(repo.root())
                .unwrap()
                .read_activation_snapshot()
                .unwrap()
                .unwrap()
                .json;
            let store = Store::at(directory.path().join("store"));
            Self {
                directory,
                repo,
                store,
                json,
                head,
                installs: AtomicUsize::new(0),
            }
        }

        fn run(
            &self,
            json: &str,
            runtime: &str,
            checkpoint: impl FnMut(Checkpoint) -> Result<()>,
        ) -> Result<PreparedExploration> {
            prepare_with(
                PreparationRequest {
                    repo: &self.repo,
                    source_repo: &self.repo,
                    store: &self.store,
                    slug: "alice/photo",
                    transaction_json: json,
                    runtime,
                    cwd: self.directory.path(),
                },
                |text, from, to, cwd| {
                    assert_eq!(
                        text, "",
                        "file exploration cannot install source conversation bytes"
                    );
                    assert_eq!(from, runtime);
                    assert_eq!(to, runtime);
                    assert_eq!(cwd, self.directory.path());
                    let tx = mergetx::checked_activation_image(json).unwrap();
                    for branch in [
                        "main".to_owned(),
                        format!("merge-exploration/{}", tx.generation.as_deref().unwrap()),
                    ] {
                        let mut digest = Sha256::new();
                        digest.update(b"alice/photo\0");
                        digest.update(branch.as_bytes());
                        let path = self
                            .store
                            .root()
                            .join(".locks/branches")
                            .join(format!("{}.lock", hex::encode(digest.finalize())));
                        let lock = std::fs::OpenOptions::new()
                            .read(true)
                            .write(true)
                            .open(path)?;
                        assert!(
                            fs2::FileExt::try_lock_exclusive(&lock).is_err(),
                            "installation must retain both branch guards"
                        );
                    }
                    let index = self.installs.fetch_add(1, Ordering::Relaxed) + 1;
                    let path = self.directory.path().join(format!("FILE_{index}.jsonl"));
                    let baseline = if runtime == "claude-code" {
                        ""
                    } else {
                        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"FILE_1\"}}\n"
                    };
                    std::fs::write(&path, baseline)?;
                    Ok(Installed {
                        path,
                        next: adapter::Next::Resume("not executed".into()),
                    })
                },
                |link| {
                    Ok(std::fs::read(
                        self.directory
                            .path()
                            .join(format!("{}.jsonl", link.session_id)),
                    )?)
                },
                checkpoint,
            )
        }

        fn current_json(&self) -> String {
            mergetx::ControlGuard::acquire(self.repo.root())
                .unwrap()
                .read_activation_snapshot()
                .unwrap()
                .unwrap()
                .json
        }
    }

    #[test]
    fn empty_opencode_bootstrap_is_an_importable_independent_session() {
        for cwd in [Path::new("/owned/work"), Path::new(r"C:\owned\work")] {
            let (id, payload) = empty_opencode_bootstrap(cwd).unwrap();
            let body: serde_json::Value = serde_json::from_str(&payload).unwrap();
            assert!(id.starts_with("ses_"));
            assert_eq!(body["info"]["id"], id);
            assert_eq!(body["info"]["directory"], cwd.to_string_lossy().as_ref());
            assert_eq!(body["info"]["projectID"], "global");
            assert!(body["info"].get("parentID").is_none());
            assert_eq!(body["messages"], serde_json::json!([]));
        }
    }

    /// The owned import process validates the actual adapter payload and creates only its native row.
    #[test]
    #[cfg(unix)]
    fn file_opencode_import_probe() {
        let Ok(receipt) = std::env::var("AGIT_FILE_OPENCODE_IMPORT_PAYLOAD") else {
            return;
        };
        let body: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&receipt).unwrap()).unwrap();
        let id = body["info"]["id"].as_str().unwrap();
        assert_eq!(body["messages"], serde_json::json!([]));
        assert_eq!(Path::new(&receipt).file_stem().unwrap(), id);
        let marker = std::env::var_os("AGIT_FILE_OPENCODE_IMPORT_MARKER").unwrap();
        use std::io::Write;
        writeln!(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(marker)
                .unwrap(),
            "{receipt}"
        )
        .unwrap();
        let mode = std::env::var("AGIT_FILE_OPENCODE_IMPORT_MODE").unwrap();
        assert_ne!(mode, "import-failure", "controlled import failure");
        if mode == "missing-native" {
            return;
        }
        let database =
            PathBuf::from(std::env::var_os("XDG_DATA_HOME").unwrap()).join("opencode/opencode.db");
        std::fs::create_dir_all(database.parent().unwrap()).unwrap();
        let connection = rusqlite::Connection::open(database).unwrap();
        connection.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT, parent_id TEXT, directory TEXT, time_created INTEGER, time_updated INTEGER, version TEXT);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
             CREATE TABLE part (id TEXT PRIMARY KEY, session_id TEXT, message_id TEXT, time_created INTEGER, data TEXT);",
        ).unwrap();
        connection
            .execute(
                "INSERT INTO session VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    id,
                    body["info"]["projectID"].as_str().unwrap(),
                    body["info"]["directory"].as_str().unwrap(),
                    body["info"]["time"]["created"].as_i64().unwrap(),
                    body["info"]["time"]["updated"].as_i64().unwrap(),
                    body["info"]["version"].as_str().unwrap(),
                ],
            )
            .unwrap();
    }

    /// Preparing requires a successful official import and canonical readback before native authority.
    #[test]
    #[cfg(unix)]
    fn file_opencode_preparation_uses_real_import_and_readback_before_admission() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;
        const CHILD: &str = "AGIT_FILE_OPENCODE_PREPARATION_CHILD";
        if std::env::var_os(CHILD).is_none() {
            for mode in ["ok", "import-failure", "missing-native"] {
                let home = tempfile::tempdir().unwrap();
                let bin = home.path().join("bin");
                std::fs::create_dir_all(&bin).unwrap();
                let shim = bin.join("opencode");
                std::fs::write(&shim, concat!(
                    "#!/bin/sh\nset -eu\n[ \"$#\" -eq 2 ]\n[ \"$1\" = import ]\n",
                    "export AGIT_FILE_OPENCODE_IMPORT_PAYLOAD=\"$2\"\n",
                    "exec \"$AGIT_FILE_OPENCODE_TEST_EXECUTABLE\" --exact commands::merge::archive::preparation::tests::file_opencode_import_probe --nocapture\n",
                )).unwrap();
                std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o700)).unwrap();
                let mut paths = vec![bin];
                paths.extend(std::env::split_paths(
                    &std::env::var_os("PATH").unwrap_or_default(),
                ));
                let mut command = Command::new(std::env::current_exe().unwrap());
                command.args(["--exact", "commands::merge::archive::preparation::tests::file_opencode_preparation_uses_real_import_and_readback_before_admission", "--nocapture"])
                    .env(CHILD, "1")
                    .env("HOME", home.path())
                    .env("USERPROFILE", home.path())
                    .env("AGIT_HOME", home.path().join("agit"))
                    .env("XDG_DATA_HOME", home.path().join("data"))
                    .env("PATH", std::env::join_paths(paths).unwrap())
                    .env("GIT_CONFIG_GLOBAL", home.path().join("gitconfig"))
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .env("AGIT_FILE_OPENCODE_IMPORT_MODE", mode)
                    .env("AGIT_FILE_OPENCODE_IMPORT_MARKER", home.path().join("imports.log"))
                    .env("AGIT_FILE_OPENCODE_TEST_EXECUTABLE", std::env::current_exe().unwrap());
                for name in archive_history::GIT_ROUTING_ENV {
                    command.env_remove(name);
                }
                command
                    .env_remove(mergetx::ENV)
                    .env_remove(mergetx::GENERATION_ENV);
                let output = command.output().unwrap();
                assert!(output.status.success(), "{mode}: {output:?}");
            }
            return;
        }
        let fixture = FileFixture::new();
        let mode = std::env::var("AGIT_FILE_OPENCODE_IMPORT_MODE").unwrap();
        let tx = mergetx::checked_activation_image(&fixture.json).unwrap();
        let worktree = fixture.repo.git(&["status", "--porcelain"]).unwrap();
        let run = |transaction_json: &str| {
            prepare(PreparationRequest {
                repo: &fixture.repo,
                source_repo: &fixture.repo,
                store: &fixture.store,
                slug: "alice/photo",
                transaction_json,
                runtime: "opencode",
                cwd: fixture.directory.path(),
            })
        };
        let result = run(&fixture.json);
        let marker = std::env::var_os("AGIT_FILE_OPENCODE_IMPORT_MARKER").unwrap();
        let imported = std::fs::read_to_string(&marker).unwrap();
        assert_eq!(
            imported.lines().count(),
            1,
            "the real import subprocess must run once"
        );
        let receipt = PathBuf::from(imported.trim());
        let receipt_bytes = std::fs::read(&receipt).unwrap();
        let id = receipt.file_stem().unwrap().to_str().unwrap();
        assert_eq!(
            super::super::current_head(&fixture.repo, "main").unwrap(),
            fixture.head
        );
        assert_eq!(
            fixture.repo.git(&["status", "--porcelain"]).unwrap(),
            worktree
        );
        if mode != "ok" {
            assert!(
                result.is_err(),
                "failed import/readback cannot acquire authority"
            );
            assert_eq!(fixture.current_json(), fixture.json);
            assert!(
                link::read_archive_link_snapshot(&fixture.store, "opencode", id)
                    .unwrap()
                    .is_none()
            );
            assert!(
                merge_archive::read_preparation_intent(
                    fixture.repo.root(),
                    tx.generation.as_deref().unwrap()
                )
                .unwrap()
                .is_none()
            );
            assert_eq!(std::fs::read(&receipt).unwrap(), receipt_bytes);
            let seed = file_agent::read_seed(&fixture.repo, tx.generation.as_deref().unwrap())
                .unwrap()
                .unwrap();
            seed.verify(&fixture.repo).unwrap();
            return;
        }
        let prepared = result.unwrap();
        assert_eq!(prepared.binding.native.session_id, id);
        let selected = link::read_archive_link_snapshot(&fixture.store, "opencode", id)
            .unwrap()
            .unwrap();
        let native = read_native(&selected.link).unwrap();
        let canonical: serde_json::Value = serde_json::from_slice(&native).unwrap();
        assert_eq!(canonical["kind"], "opencode.meta");
        assert_eq!(canonical["id"], id);
        assert_eq!(native.iter().filter(|byte| **byte == b'\n').count(), 1);
        assert_ne!(
            native, receipt_bytes,
            "the baseline comes from canonical library rows"
        );
        assert_eq!(prepared.binding.installed.bytes, native.len() as u64);
        assert_eq!(
            prepared.binding.installed.sha256,
            hex::encode(Sha256::digest(&native))
        );
        assert_eq!(
            storage::materialize_at(
                fixture.repo.root(),
                &prepared.binding.role.origin_head,
                meta::LOG_FILE
            )
            .unwrap(),
            ""
        );
        let refs = fixture.repo.git(&["show-ref"]).unwrap();
        let current = fixture.current_json();
        let repeated = run(&current).unwrap();
        assert!(repeated.reused);
        assert_eq!(repeated.binding, prepared.binding);
        assert_eq!(fixture.repo.git(&["show-ref"]).unwrap(), refs);
        assert_eq!(fixture.current_json(), current);
        assert_eq!(std::fs::read_to_string(marker).unwrap(), imported);
        assert_eq!(read_native(&selected.link).unwrap(), native);
        assert_eq!(std::fs::read(receipt).unwrap(), receipt_bytes);
    }

    #[test]
    fn file_preparation_binds_only_the_empty_evidence_session_and_replays_once() {
        for runtime in ["codex", "claude-code"] {
            let f = FileFixture::new();
            let worktree = f.repo.git(&["status", "--porcelain"]).unwrap();
            let prepared = f.run(&f.json, runtime, |_| Ok(())).unwrap();
            let binding = &prepared.binding;
            assert_eq!(binding.target_branch(), "main");
            assert_eq!(binding.target_head(), f.head);
            assert!(binding.role.branch.starts_with("merge-exploration/"));
            assert_ne!(binding.role.origin_head, f.head);
            let selected = link::read_archive_link_snapshot(&f.store, runtime, "FILE_1")
                .unwrap()
                .unwrap();
            assert!(
                selected
                    .link
                    .is_archive_for(&binding.role, runtime, "FILE_1")
            );
            assert_eq!(
                selected.link.branch.as_deref(),
                Some(binding.role.branch.as_str())
            );
            assert_eq!(super::super::current_head(&f.repo, "main").unwrap(), f.head);
            assert_eq!(f.repo.git(&["status", "--porcelain"]).unwrap(), worktree);
            assert!(!f.repo.root().join(meta::LOG_FILE).exists());
            assert!(!f.repo.root().join(meta::VIEW_FILE).exists());
            assert_eq!(
                storage::materialize_at(f.repo.root(), &binding.role.origin_head, meta::LOG_FILE)
                    .unwrap(),
                ""
            );
            let current = f.current_json();
            let repeated = f.run(&current, runtime, |_| Ok(())).unwrap();
            assert!(repeated.reused);
            assert_eq!(repeated.binding, *binding);
            assert_eq!(f.installs.load(Ordering::Relaxed), 1);
            assert_eq!(f.current_json(), current);
        }
    }

    #[test]
    fn interrupted_file_activation_reuses_its_exact_installation() {
        for stop in [
            activation::Checkpoint::Prepared,
            activation::Checkpoint::Successor,
            activation::Checkpoint::Transaction,
            activation::Checkpoint::Open,
        ] {
            let f = FileFixture::new();
            assert!(
                f.run(&f.json, "codex", |step| {
                    ensure!(
                        step != Checkpoint::Activation(stop),
                        "injected file activation interruption"
                    );
                    Ok(())
                })
                .is_err()
            );
            let tx = mergetx::checked_activation_image(&f.json).unwrap();
            let retained = merge_archive::read_preparation_intent(
                f.repo.root(),
                tx.generation.as_deref().unwrap(),
            )
            .unwrap()
            .unwrap();
            let bytes = std::fs::read(f.directory.path().join("FILE_1.jsonl")).unwrap();
            let recovered = f.run(&f.current_json(), "codex", |_| Ok(())).unwrap();
            assert!(recovered.reused);
            assert_eq!(recovered.binding, retained.binding);
            assert_eq!(f.installs.load(Ordering::Relaxed), 1);
            assert_eq!(
                std::fs::read(f.directory.path().join("FILE_1.jsonl")).unwrap(),
                bytes
            );
            assert_eq!(super::super::current_head(&f.repo, "main").unwrap(), f.head);
        }
    }

    #[test]
    fn a_file_target_move_after_installation_cannot_bind_native_authority() {
        let f = FileFixture::new();
        let tree = f.repo.git(&["rev-parse", "HEAD^{tree}"]).unwrap();
        let moved = crate::commands::plumbing::commit_tree(
            &f.repo,
            &tree,
            &[&f.head],
            "independent main update",
        )
        .unwrap();
        assert!(
            f.run(&f.json, "codex", |step| {
                if step == Checkpoint::Installed {
                    crate::commands::plumbing::update_ref_cas(
                        &f.repo,
                        "refs/heads/main",
                        &moved,
                        Some(&f.head),
                    )?;
                }
                Ok(())
            })
            .is_err()
        );
        assert_eq!(f.current_json(), f.json);
        assert_eq!(super::super::current_head(&f.repo, "main").unwrap(), moved);
        assert!(
            link::read_archive_link_snapshot(&f.store, "codex", "FILE_1")
                .unwrap()
                .is_none()
        );
        let tx = mergetx::checked_activation_image(&f.json).unwrap();
        assert!(
            merge_archive::read_preparation_intent(
                f.repo.root(),
                tx.generation.as_deref().unwrap()
            )
            .unwrap()
            .is_none()
        );
        assert!(f.directory.path().join("FILE_1.jsonl").is_file());
    }

    struct Fixture {
        directory: tempfile::TempDir,
        repo: Repo,
        store: Store,
        transaction_json: String,
        installs: AtomicUsize,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let repo = Repo::init(&directory.path().join("repo")).unwrap();
            repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
            let mut metadata = meta::Meta::new(SESSION.into(), "codex".into(), "/work".into());
            metadata.turn = Some(1);
            meta::write(repo.root(), &metadata).unwrap();
            let view = transcript::wrap_lines(VISIBLE, "codex", SESSION);
            let log = format!(
                "{view}{}",
                transcript::wrap_lines(
                    "{\"type\":\"user\",\"text\":\"excluded log evidence\"}\n",
                    "codex",
                    SESSION
                )
            );
            storage::write_snapshot(repo.root(), &log, &view).unwrap();
            std::fs::write(repo.root().join("AGENTS.md"), "Shared instructions\n").unwrap();
            repo.add_all().unwrap();
            repo.commit("preparation fixture").unwrap();
            let head = repo.git(&["rev-parse", "HEAD"]).unwrap();
            repo.git(&["update-ref", "refs/heads/work", &head]).unwrap();
            repo.git(&["update-ref", "refs/heads/source", &head])
                .unwrap();
            repo.git(&["symbolic-ref", "HEAD", "refs/heads/work"])
                .unwrap();
            let tx = mergetx::Tx {
                mode: Some(mergetx::Mode::SessionAgent),
                exploration: None,
                generation: Some(uuid::Uuid::now_v7().to_string()),
                target: "work".into(),
                source: "alice/photo@source".into(),
                source_repo: Some("alice/photo".into()),
                source_branch: Some("source".into()),
                base: head.clone(),
                target_head: head.clone(),
                source_head: head.clone(),
                picked: vec![],
                summary: None,
            };
            mergetx::create(repo.root(), &tx).unwrap();
            let mut facts = match JsonFacts::parse(&serde_json::to_string(&tx).unwrap()).unwrap() {
                JsonFacts::Object(facts) => facts,
                _ => unreachable!(),
            };
            facts.insert(
                "future".into(),
                JsonFacts::Atom("9007199254740993.0000000000000001".into()),
            );
            let mut transaction_json = String::new();
            JsonFacts::Object(facts)
                .write_json(&mut transaction_json)
                .unwrap();
            transaction_json.push('\n');
            std::fs::write(
                crate::domain::repo::common_git_dir(repo.root()).join(mergetx::LOCK_FILE),
                &transaction_json,
            )
            .unwrap();
            let store = Store::at(directory.path().join("store"));
            let mut old = Link::new("codex", "OLD", Some(directory.path()));
            old.owner = Some("alice".into());
            old.agent = Some("photo".into());
            old.branch = Some("work".into());
            old.materialized_from = Some(head);
            old.baseline_bytes = Some(VISIBLE.len() as u64);
            old.baseline_hash = Some(hex::encode(Sha256::digest(VISIBLE)));
            link::write(&store, &old).unwrap();
            std::fs::write(directory.path().join("OLD.jsonl"), VISIBLE).unwrap();
            Self {
                directory,
                repo,
                store,
                transaction_json,
                installs: AtomicUsize::new(0),
            }
        }

        fn request<'a>(&'a self, transaction_json: &'a str) -> PreparationRequest<'a> {
            PreparationRequest {
                repo: &self.repo,
                source_repo: &self.repo,
                store: &self.store,
                slug: "alice/photo",
                transaction_json,
                runtime: "codex",
                cwd: self.directory.path(),
            }
        }

        fn native(&self, link: &Link) -> Result<Vec<u8>> {
            Ok(std::fs::read(
                self.directory
                    .path()
                    .join(format!("{}.jsonl", link.session_id)),
            )?)
        }

        fn install(&self, text: &str, from: &str, to: &str, cwd: &Path) -> Result<Installed> {
            assert_eq!(from, "codex");
            assert_eq!(to, "codex");
            assert_eq!(cwd, self.directory.path());
            assert!(text.contains("visible installation context"));
            assert!(!text.contains("excluded log evidence"));
            let mut digest = Sha256::new();
            digest.update(b"alice/photo\0work");
            let lock_path = self
                .store
                .root()
                .join(".locks/branches")
                .join(format!("{}.lock", hex::encode(digest.finalize())));
            let lock = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(lock_path)?;
            assert!(
                fs2::FileExt::try_lock_exclusive(&lock).is_err(),
                "installation must retain the branch guard"
            );
            self.installs.fetch_add(1, Ordering::Relaxed);
            let path = self.directory.path().join(format!(
                "NEW_{}.jsonl",
                self.installs.load(Ordering::Relaxed)
            ));
            std::fs::write(&path, text)?;
            Ok(Installed {
                path,
                next: adapter::Next::Resume("unused launcher".into()),
            })
        }

        fn run(
            &self,
            json: &str,
            checkpoint: impl FnMut(Checkpoint) -> Result<()>,
        ) -> Result<PreparedExploration> {
            prepare_with(
                self.request(json),
                |text, from, to, cwd| self.install(text, from, to, cwd),
                |link| self.native(link),
                checkpoint,
            )
        }

        fn tx_path(&self) -> PathBuf {
            crate::domain::repo::common_git_dir(self.repo.root()).join(mergetx::LOCK_FILE)
        }

        fn authority(&self) -> (String, Vec<u8>, Vec<u8>) {
            (
                self.repo.git(&["rev-parse", "refs/heads/work"]).unwrap(),
                std::fs::read(self.tx_path()).unwrap(),
                std::fs::read(link::link_path(&self.store, "codex", "OLD")).unwrap(),
            )
        }
    }

    #[test]
    fn preparation_installs_view_once_and_reuses_open_authority_after_progress() {
        let fixture = Fixture::new();
        let before = fixture.authority();
        let result = fixture.run(&fixture.transaction_json, |_| Ok(())).unwrap();
        assert!(!result.reused);
        assert_eq!(result.unresolved_placeholders, Some(0));
        assert!(!result.lossy);
        assert_eq!(fixture.installs.load(Ordering::Relaxed), 1);
        assert_eq!(fixture.authority().0, before.0);
        assert_eq!(
            std::fs::read(fixture.directory.path().join("OLD.jsonl")).unwrap(),
            VISIBLE.as_bytes()
        );
        assert!(link::active_for_branch(&fixture.store, "alice", "photo", "work").is_empty());
        let control = mergetx::ControlGuard::acquire(fixture.repo.root()).unwrap();
        let mut tx = control.read().unwrap().unwrap();
        tx.pick_more(&["source#1".into()]);
        tx.set_summary("keep selected context".into());
        control.write(&tx).unwrap();
        drop(control);
        let progressed = std::fs::read_to_string(fixture.tx_path()).unwrap();
        assert!(progressed.contains("9007199254740993.0000000000000001"));
        std::fs::remove_file(fixture.directory.path().join("OLD.jsonl")).unwrap();
        let next = fixture.run(&progressed, |_| Ok(())).unwrap();
        assert!(next.reused);
        assert_eq!(next.binding, result.binding);
        assert_eq!(fixture.installs.load(Ordering::Relaxed), 1);
        assert_eq!(
            std::fs::read_to_string(fixture.tx_path()).unwrap(),
            progressed
        );
    }

    #[test]
    fn preparation_failure_before_journal_leaves_only_unclaimed_native_artifact() {
        let fixture = Fixture::new();
        let before = fixture.authority();
        assert!(
            fixture
                .run(&fixture.transaction_json, |step| {
                    ensure!(
                        step != Checkpoint::Installed,
                        "injected pre-activation failure"
                    );
                    Ok(())
                })
                .is_err()
        );
        assert_eq!(fixture.authority(), before);
        assert!(fixture.directory.path().join("NEW_1.jsonl").is_file());
        assert!(
            link::read_archive_link_snapshot(&fixture.store, "codex", "NEW_1")
                .unwrap()
                .is_none()
        );
        let tx = mergetx::checked_activation_image(&fixture.transaction_json).unwrap();
        assert!(
            merge_archive::read_preparation_intent(
                fixture.repo.root(),
                tx.generation.as_deref().unwrap()
            )
            .unwrap()
            .is_none()
        );
        let next = fixture.run(&fixture.transaction_json, |_| Ok(())).unwrap();
        assert_eq!(next.binding.native.session_id, "NEW_2");
        assert!(fixture.directory.path().join("NEW_1.jsonl").is_file());
    }

    #[test]
    fn preparation_recovers_every_activation_endpoint_without_reinstalling() {
        for boundary in [
            activation::Checkpoint::Prepared,
            activation::Checkpoint::Successor,
            activation::Checkpoint::Retired(0),
            activation::Checkpoint::BeforeTransaction,
            activation::Checkpoint::Transaction,
            activation::Checkpoint::Open,
        ] {
            let fixture = Fixture::new();
            assert!(
                fixture
                    .run(&fixture.transaction_json, |step| {
                        ensure!(
                            step != Checkpoint::Activation(boundary),
                            "injected activation failure"
                        );
                        Ok(())
                    })
                    .is_err()
            );
            let current = std::fs::read_to_string(fixture.tx_path()).unwrap();
            let result = fixture.run(&current, |_| Ok(())).unwrap();
            assert!(result.reused);
            assert_eq!(result.binding.native.session_id, "NEW_1");
            assert_eq!(fixture.installs.load(Ordering::Relaxed), 1);
            assert_eq!(
                merge_archive::read(fixture.repo.root(), &result.binding.role.generation)
                    .unwrap()
                    .unwrap()
                    .phase,
                ArchivePhase::Open
            );
        }
    }

    #[test]
    fn preparation_uses_pending_journal_intent_instead_of_installing_again() {
        let fixture = Fixture::new();
        assert!(
            fixture
                .run(&fixture.transaction_json, |step| {
                    ensure!(
                        step != Checkpoint::Activation(activation::Checkpoint::Prepared),
                        "retain Preparing"
                    );
                    Ok(())
                })
                .is_err()
        );
        let tx = mergetx::checked_activation_image(&fixture.transaction_json).unwrap();
        let path = crate::domain::repo::common_git_dir(fixture.repo.root())
            .join(merge_archive::DIRECTORY)
            .join(format!("{}.json", tx.generation.unwrap()));
        std::fs::remove_file(path).unwrap();
        let result = fixture.run(&fixture.transaction_json, |_| Ok(())).unwrap();
        assert!(result.reused);
        assert_eq!(fixture.installs.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn preparation_rejects_claim_changes_and_native_growth_without_claim_publication() {
        for change in ["new", "route", "native"] {
            let fixture = Fixture::new();
            assert!(
                fixture
                    .run(&fixture.transaction_json, |step| {
                        if step == Checkpoint::Installed {
                            match change {
                                "new" => {
                                    let mut claim =
                                        link::get(&fixture.store, "codex", "OLD").unwrap();
                                    claim.session_id = "FOREIGN".into();
                                    link::write(&fixture.store, &claim)?;
                                }
                                "route" => {
                                    let mut claim =
                                        link::get(&fixture.store, "codex", "OLD").unwrap();
                                    claim.branch = Some("elsewhere".into());
                                    link::write(&fixture.store, &claim)?;
                                }
                                _ => {
                                    std::fs::write(
                                        fixture.directory.path().join("OLD.jsonl"),
                                        format!("{VISIBLE}{VISIBLE}"),
                                    )?;
                                }
                            }
                        }
                        Ok(())
                    })
                    .is_err()
            );
            assert!(
                link::read_archive_link_snapshot(&fixture.store, "codex", "NEW_1")
                    .unwrap()
                    .is_none()
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
    fn preparation_rejects_corrupt_unrelated_links_and_stale_transaction_before_install() {
        for defect in ["corrupt", "stale", "owner"] {
            let fixture = Fixture::new();
            let before = fixture.authority();
            match defect {
                "corrupt" => {
                    std::fs::write(fixture.store.root().join("codex/BROKEN.json"), "{").unwrap()
                }
                "stale" => {
                    let mut tx = mergetx::read(fixture.repo.root()).unwrap().unwrap();
                    tx.set_summary("new progress".into());
                    mergetx::lock(fixture.repo.root(), &tx).unwrap();
                }
                _ => {
                    let mut claim = link::get(&fixture.store, "codex", "OLD").unwrap();
                    claim.owner = None;
                    link::write(&fixture.store, &claim).unwrap();
                }
            }
            let current = fixture.authority();
            assert!(fixture.run(&fixture.transaction_json, |_| Ok(())).is_err());
            assert_eq!(fixture.installs.load(Ordering::Relaxed), 0);
            assert_eq!(fixture.authority(), current);
            assert_eq!(fixture.authority().0, before.0);
        }
    }

    #[test]
    fn ordinary_transaction_paths_cannot_create_replace_or_remove_archive_authority() {
        let fixture = Fixture::new();
        fixture.run(&fixture.transaction_json, |_| Ok(())).unwrap();
        let before = std::fs::read(fixture.tx_path()).unwrap();
        let stale = mergetx::checked_activation_image(&fixture.transaction_json).unwrap();
        assert!(mergetx::lock(fixture.repo.root(), &stale).is_err());
        assert!(mergetx::unlock(fixture.repo.root()).is_err());
        let control = mergetx::ControlGuard::acquire(fixture.repo.root()).unwrap();
        let bound = control.read().unwrap().unwrap();
        assert!(!bound.same_instance(&stale));
        let mut foreign = bound.clone();
        foreign.exploration.as_mut().unwrap().native.session_id = "FOREIGN".into();
        assert!(control.write(&foreign).is_err());
        assert!(control.remove().is_err());
        drop(control);
        assert_eq!(std::fs::read(fixture.tx_path()).unwrap(), before);
        let empty = tempfile::tempdir().unwrap();
        std::fs::create_dir(empty.path().join(".git")).unwrap();
        assert!(mergetx::create(empty.path(), &bound).is_err());
        assert!(mergetx::lock(empty.path(), &bound).is_err());
        assert!(!mergetx::is_locked(empty.path()));
    }

    #[test]
    fn preparation_refuses_foreign_retry_cwd_runtime_and_native_baseline() {
        for defect in ["cwd", "runtime", "native"] {
            let fixture = Fixture::new();
            assert!(
                fixture
                    .run(&fixture.transaction_json, |step| {
                        ensure!(
                            step != Checkpoint::Activation(activation::Checkpoint::Prepared),
                            "retain Preparing"
                        );
                        Ok(())
                    })
                    .is_err()
            );
            let mut request = fixture.request(&fixture.transaction_json);
            let elsewhere = fixture.directory.path().join("elsewhere");
            match defect {
                "cwd" => request.cwd = &elsewhere,
                "runtime" => request.runtime = "claude-code",
                _ => std::fs::write(
                    fixture.directory.path().join("NEW_1.jsonl"),
                    "{\"changed\":true}\n",
                )
                .unwrap(),
            }
            assert!(
                prepare_with(
                    request,
                    |_, _, _, _| panic!("recovery must not install"),
                    |link| fixture.native(link),
                    |_| Ok(())
                )
                .is_err()
            );
            assert_eq!(fixture.installs.load(Ordering::Relaxed), 1);
        }
    }

    #[test]
    fn preparation_actual_codex_install_and_live_readback_keep_log_only_evidence_out() {
        const CHILD: &str = "AGIT_ARCHIVE_PREPARATION_CODEX_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let home = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "commands::merge::archive::preparation::tests::preparation_actual_codex_install_and_live_readback_keep_log_only_evidence_out", "--nocapture"])
                .env(CHILD, "1").env("HOME", home.path()).env("CODEX_HOME", home.path().join(".codex"))
                .env("AGIT_HOME", home.path().join(".agit")).output().unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let fixture = Fixture::new();
        std::fs::remove_file(link::link_path(&fixture.store, "codex", "OLD")).unwrap();
        let result = prepare(fixture.request(&fixture.transaction_json)).unwrap();
        let installed = link::read_archive_link_snapshot(
            &fixture.store,
            "codex",
            &result.binding.native.session_id,
        )
        .unwrap()
        .unwrap();
        let native = installed.link.read_bytes().unwrap();
        let text = std::str::from_utf8(&native).unwrap();
        assert!(text.contains("visible installation context"));
        assert!(!text.contains("excluded log evidence"));
        assert_eq!(native.len() as u64, result.binding.installed.bytes);
        assert_eq!(
            hex::encode(Sha256::digest(&native)),
            result.binding.installed.sha256
        );
        let current = std::fs::read_to_string(fixture.tx_path()).unwrap();
        let reused = prepare(fixture.request(&current)).unwrap();
        assert!(reused.reused);
        assert_eq!(reused.binding, result.binding);
    }
    #[test]
    fn preparing_intent_blocks_unbound_ordinary_changes_but_not_another_generation() {
        let fixture = Fixture::new();
        assert!(
            fixture
                .run(&fixture.transaction_json, |step| {
                    ensure!(
                        step != Checkpoint::Activation(activation::Checkpoint::Prepared),
                        "retain Preparing"
                    );
                    Ok(())
                })
                .is_err()
        );
        let before = std::fs::read(fixture.tx_path()).unwrap();
        let mut tx = mergetx::read(fixture.repo.root()).unwrap().unwrap();
        assert!(tx.exploration.is_none());
        tx.set_summary("ordinary progress".into());
        assert!(mergetx::lock(fixture.repo.root(), &tx).is_err());
        assert!(mergetx::unlock(fixture.repo.root()).is_err());
        assert_eq!(std::fs::read(fixture.tx_path()).unwrap(), before);
        std::fs::remove_file(fixture.tx_path()).unwrap();
        assert!(mergetx::create(fixture.repo.root(), &tx).is_err());
        tx.generation = Some(uuid::Uuid::now_v7().to_string());
        mergetx::create(fixture.repo.root(), &tx).unwrap();
        tx.set_summary("new generation progress".into());
        mergetx::lock(fixture.repo.root(), &tx).unwrap();
        let new_json = std::fs::read_to_string(fixture.tx_path()).unwrap();
        assert!(fixture.run(&new_json, |_| Ok(())).is_err());
        assert_eq!(fixture.installs.load(Ordering::Relaxed), 1);
        mergetx::unlock(fixture.repo.root()).unwrap();
    }

    #[test]
    fn bound_progress_waits_for_open_and_preserves_preparing_replay() {
        let fixture = Fixture::new();
        assert!(
            fixture
                .run(&fixture.transaction_json, |step| {
                    ensure!(
                        step != Checkpoint::Activation(activation::Checkpoint::Transaction),
                        "retain bound Preparing endpoint"
                    );
                    Ok(())
                })
                .is_err()
        );
        let bound = std::fs::read_to_string(fixture.tx_path()).unwrap();
        let mut progress = mergetx::read(fixture.repo.root()).unwrap().unwrap();
        assert!(progress.exploration.is_some());
        progress.set_summary("progress after durable activation".into());
        assert!(mergetx::lock(fixture.repo.root(), &progress).is_err());
        assert_eq!(std::fs::read_to_string(fixture.tx_path()).unwrap(), bound);
        fixture.run(&bound, |_| Ok(())).unwrap();
        mergetx::lock(fixture.repo.root(), &progress).unwrap();
        assert_eq!(
            mergetx::read(fixture.repo.root()).unwrap().unwrap().summary,
            progress.summary
        );
        assert_eq!(fixture.installs.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn bound_progress_refuses_missing_or_corrupt_journal_evidence() {
        for defect in ["missing", "corrupt", "recovery"] {
            let fixture = Fixture::new();
            let result = fixture.run(&fixture.transaction_json, |_| Ok(())).unwrap();
            let before = std::fs::read(fixture.tx_path()).unwrap();
            let directory = crate::domain::repo::common_git_dir(fixture.repo.root())
                .join(merge_archive::DIRECTORY);
            let primary = directory.join(format!("{}.json", result.binding.role.generation));
            match defect {
                "missing" => std::fs::remove_file(primary).unwrap(),
                "corrupt" => std::fs::write(primary, "{").unwrap(),
                _ => std::fs::remove_file(
                    directory.join(format!("{}.recovery", result.binding.role.generation)),
                )
                .unwrap(),
            }
            let mut progress = mergetx::read(fixture.repo.root()).unwrap().unwrap();
            progress.set_summary("rejected progress".into());
            assert!(mergetx::lock(fixture.repo.root(), &progress).is_err());
            assert_eq!(std::fs::read(fixture.tx_path()).unwrap(), before);
        }
    }

    #[test]
    fn ordinary_progress_holding_transaction_control_wins_before_preparation_admission() {
        let fixture = Fixture::new();
        let control = mergetx::ControlGuard::acquire(fixture.repo.root()).unwrap();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        std::thread::scope(|scope| {
            let pending = scope.spawn(|| {
                fixture.run(&fixture.transaction_json, |step| {
                    if step == Checkpoint::BranchLocked {
                        sender.send(())?;
                    }
                    Ok(())
                })
            });
            let ready = receiver.recv_timeout(std::time::Duration::from_secs(5));
            assert_eq!(fixture.installs.load(Ordering::Relaxed), 0);
            let mut current = control.read().unwrap().unwrap();
            current.set_summary("ordinary progress wins".into());
            control.write(&current).unwrap();
            drop(control);
            let outcome = pending.join().unwrap();
            assert!(ready.is_ok(), "preparation did not reach branch admission");
            assert!(outcome.is_err());
        });
        assert_eq!(fixture.installs.load(Ordering::Relaxed), 0);
        assert_eq!(
            mergetx::read(fixture.repo.root())
                .unwrap()
                .unwrap()
                .summary
                .as_deref(),
            Some("ordinary progress wins")
        );
    }

    #[test]
    fn preparation_inventory_limits_refuse_before_native_installation() {
        for limit in ["claims", "entries", "bytes"] {
            let fixture = Fixture::new();
            let mut template = link::get(&fixture.store, "codex", "OLD").unwrap();
            if limit == "bytes" {
                template.owner = Some("unrelated".into());
            }
            let count = match limit {
                "claims" => 128,
                "entries" => 8193,
                _ => 134,
            };
            for index in 0..count {
                if limit == "entries" {
                    std::fs::write(
                        fixture
                            .store
                            .root()
                            .join(format!("codex/ignored-{index}.other")),
                        "",
                    )
                    .unwrap();
                } else {
                    template.session_id = format!("CLAIM_{index}");
                    let mut json = template.to_json().unwrap();
                    if limit == "bytes" {
                        json.pop();
                        json.push_str(&format!(",\"padding\":\"{}\"}}", "x".repeat(63 * 1024)));
                    }
                    std::fs::write(
                        link::link_path(&fixture.store, "codex", &template.session_id),
                        json,
                    )
                    .unwrap();
                }
            }
            let before = fixture.authority();
            let error = fixture
                .run(&fixture.transaction_json, |_| Ok(()))
                .err()
                .expect("inventory must refuse");
            let expected = match limit {
                "claims" => "too many archive previous claims",
                "entries" => "entry limit",
                _ => "byte limit",
            };
            assert!(error.to_string().contains(expected), "{error:#}");
            assert_eq!(fixture.installs.load(Ordering::Relaxed), 0);
            assert_eq!(fixture.authority(), before);
        }
    }
}
