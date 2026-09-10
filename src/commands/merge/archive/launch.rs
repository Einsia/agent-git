//! Starts only the native instance whose Open archive authority remains locked and verified.

use anyhow::{Context, ensure};
use std::process::{Child, Command};

use crate::Result;
use crate::commands::resume::{self, ArchiveLaunch};
use crate::domain::link;
use crate::domain::merge_archive::{self, ArchiveJournalGuard, ArchivePhase, ExplorationBinding};
use crate::domain::{archive_history, mergetx, native_archive, repo::Repo, store::Store};

use super::{preparation, read_native, require_destination_routing};

pub(in crate::commands::merge) struct Launched {
    pub child: Child,
    pub binding: ExplorationBinding,
}

pub(in crate::commands::merge) fn start(
    repo: &Repo,
    source: &Repo,
    store: &Store,
    tx: &mergetx::Tx,
    slug: &str,
    launch: &ArchiveLaunch,
    prompt: &str,
) -> Result<Launched> {
    resume::require_archive_launch_state(repo, &tx.target, &tx.target_head, launch)?;
    let original = {
        let control = mergetx::ControlGuard::acquire(repo.root())?;
        let image = control
            .read_activation_snapshot()?
            .context("merge transaction is missing")?;
        ensure!(
            image.tx.same_instance(tx)
                && image.tx.picked == tx.picked
                && image.tx.summary == tx.summary,
            "merge transaction changed before archive installation"
        );
        image.json
    };
    let prepared = preparation::prepare(preparation::PreparationRequest {
        repo,
        source_repo: source,
        store,
        slug,
        transaction_json: &original,
        runtime: &launch.runtime,
        cwd: &launch.cwd,
    })?;
    let binding = &prepared.binding;
    let mut resumed = resume::prepared_archive_resume(
        launch,
        &binding.native.session_id,
        slug,
        &tx.target,
        prompt,
    )?;
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(
            resumed
                .cmd
                .as_deref()
                .context("archive command is missing")?,
        )
        .env(mergetx::ENV, format!("{slug}@{}", tx.target))
        .env(mergetx::GENERATION_ENV, &binding.role.generation)
        .env(
            crate::commands::commit::archive::NATIVE_ENV,
            serde_json::to_string(&binding.native)?,
        );
    let child = spawn_verified(
        repo,
        source,
        store,
        binding,
        launch,
        &mut command,
        || Ok(()),
    )?;
    resumed.emit_launch_messages();
    if let Some(unresolved) = prepared.unresolved_placeholders.filter(|count| *count > 0) {
        crate::ui::warning(&format!(
            "archive VIEW retains {unresolved} unresolved secret placeholders"
        ));
    }
    Ok(Launched {
        child,
        binding: prepared.binding,
    })
}

fn spawn_verified(
    repo: &Repo,
    source: &Repo,
    store: &Store,
    binding: &ExplorationBinding,
    launch: &ArchiveLaunch,
    command: &mut Command,
    before_lock: impl FnOnce() -> Result<()>,
) -> Result<Child> {
    require_destination_routing(repo, &binding.role.branch)?;
    let observed = merge_archive::read_preparation_intent(repo.root(), &binding.role.generation)?
        .context("archive launch journal is missing")?;
    let selected = link::read_archive_link_snapshot(
        store,
        &binding.native.runtime,
        &binding.native.session_id,
    )?
    .context("archive launch Link is missing")?;
    let transaction = {
        let control = mergetx::ControlGuard::acquire(repo.root())?;
        control
            .read_activation_snapshot()?
            .context("archive launch transaction is missing")?
    };
    before_lock()?;
    let _branch = link::lock_branch(store, &binding.role.slug, &binding.role.branch)?;
    let mut keys = observed
        .previous_claims
        .iter()
        .map(|claim| claim.native.clone())
        .collect::<Vec<_>>();
    keys.push(binding.native.clone());
    keys.sort();
    keys.dedup();
    let _links = keys
        .iter()
        .map(|key| link::lock(store, &key.runtime, &key.session_id))
        .collect::<Result<Vec<_>>>()?;
    let journal = ArchiveJournalGuard::acquire(repo.root(), &binding.role.generation)?;
    let control = mergetx::ControlGuard::acquire(repo.root())?;
    require_destination_routing(repo, &binding.role.branch)?;
    journal.recover_pending()?;
    let current = journal
        .read()?
        .context("archive launch journal disappeared")?;
    ensure!(
        current == observed
            && current.binding == *binding
            && current.phase == ArchivePhase::Open
            && current.activation.is_none()
            && current.publication.is_none()
            && current.landing.is_none()
            && current.abort.is_none()
            && current.detach.is_none()
            && current.accepted_commit.is_none()
            && current.consumed == binding.installed,
        "archive launch requires its unchanged Open installation"
    );
    let image = control
        .read_activation_snapshot()?
        .context("archive launch transaction disappeared")?;
    ensure!(
        image.json == transaction.json,
        "archive launch transaction changed"
    );
    merge_archive::checked_abort_transaction(&image.json, binding, false)?;
    resume::require_archive_launch_state(
        repo,
        &binding.role.branch,
        &binding.role.origin_head,
        launch,
    )?;
    archive_history::verify_frozen_source(source, &binding.source.head)?;
    let actual = link::read_archive_link_snapshot(
        store,
        &binding.native.runtime,
        &binding.native.session_id,
    )?
    .context("archive launch Link disappeared")?;
    ensure!(
        actual.json == selected.json
            && actual.link.is_archive_for(
                &binding.role,
                &binding.native.runtime,
                &binding.native.session_id
            )
            && actual.link.baseline_bytes == Some(binding.installed.bytes)
            && actual.link.baseline_hash.as_ref() == Some(&binding.installed.sha256)
            && actual.link.materialized_from.as_ref() == Some(&binding.role.origin_head)
            && actual.link.cwd.as_deref() == launch.cwd.to_str()
            && binding.native.runtime == launch.runtime,
        "archive launch Link differs from the installed identity"
    );
    for previous in &current.previous_claims {
        let actual = link::read_archive_link_snapshot(
            store,
            &previous.native.runtime,
            &previous.native.session_id,
        )?
        .context("retired archive claim disappeared")?;
        ensure!(
            actual.json == previous.retired_json,
            "retired archive claim changed before launch"
        );
    }
    let (owner, agent) = binding
        .role
        .slug
        .split_once('/')
        .context("invalid archive repository")?;
    ensure!(
        link::archive_claims_for_branch(store, owner, agent, &binding.role.branch)?.is_empty(),
        "another ordinary claim appeared before archive launch"
    );
    let bytes = read_native(&actual.link)?;
    ensure!(
        bytes.len() as u64 == binding.installed.bytes,
        "archive installation changed before its first launch"
    );
    native_archive::capture(&bytes, binding.installed.bytes, &binding.installed.sha256)?;
    // Only spawn is protected. Waiting under these guards would prevent the child from landing.
    command
        .spawn()
        .context("archive runtime could not start; the retained merge can be cancelled")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{install, meta, storage, transcript};
    use sha2::{Digest, Sha256};

    const SESSION: &str = "agit-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const VISIBLE: &str = "{\"type\":\"user\",\"text\":\"SYNTHETIC-ARCHIVE-VIEW\"}\n";

    struct Fixture {
        _directory: tempfile::TempDir,
        repo: Repo,
        store: Store,
        launch: ArchiveLaunch,
        binding: ExplorationBinding,
        old: link::ArchiveLinkSnapshot,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let cwd = directory.path().canonicalize().unwrap();
            let repo = Repo::init(&cwd.join("repo")).unwrap();
            repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
            let mut metadata =
                meta::Meta::new(SESSION.into(), "codex".into(), cwd.display().to_string());
            metadata.turn = Some(1);
            meta::write(repo.root(), &metadata).unwrap();
            let view = transcript::wrap_lines(VISIBLE, "codex", SESSION);
            let log = format!(
                "{view}{}",
                transcript::wrap_lines(
                    "{\"type\":\"user\",\"text\":\"SYNTHETIC-LOG-ONLY\"}\n",
                    "codex",
                    SESSION
                )
            );
            storage::write_snapshot(repo.root(), &log, &view).unwrap();
            repo.add_all().unwrap();
            repo.commit("synthetic launch target").unwrap();
            let head = repo.git(&["rev-parse", "HEAD"]).unwrap();
            for branch in ["work", "source"] {
                repo.git(&["update-ref", &format!("refs/heads/{branch}"), &head])
                    .unwrap();
            }
            repo.git(&["symbolic-ref", "HEAD", "refs/heads/work"])
                .unwrap();
            let store = Store::at(cwd.join("store"));
            let (installed, _) = install::install(VISIBLE, "codex", "codex", &cwd).unwrap();
            let sid = crate::adapter::session_id_from_stem(
                installed.path.file_stem().unwrap().to_str().unwrap(),
            );
            let bytes = std::fs::read(&installed.path).unwrap();
            let mut previous = link::Link::new("codex", &sid, Some(&cwd));
            previous.owner = Some("alice".into());
            previous.agent = Some("target".into());
            previous.branch = Some("work".into());
            previous.materialized_from = Some(head.clone());
            previous.baseline_bytes = Some(bytes.len() as u64);
            previous.baseline_hash = Some(hex::encode(Sha256::digest(&bytes)));
            link::write(&store, &previous).unwrap();
            let old = link::read_archive_link_snapshot(&store, "codex", &sid)
                .unwrap()
                .unwrap();
            let tx = mergetx::Tx {
                mode: Some(mergetx::Mode::SessionAgent),
                exploration: None,
                generation: Some(uuid::Uuid::now_v7().to_string()),
                target: "work".into(),
                source: "alice/target@source".into(),
                source_repo: Some("alice/target".into()),
                source_branch: Some("source".into()),
                base: head.clone(),
                target_head: head.clone(),
                source_head: head,
                picked: vec![],
                summary: None,
            };
            mergetx::create(repo.root(), &tx).unwrap();
            let launch = resume::archive_launch_context(
                &repo,
                "alice/target",
                "work",
                &tx.target_head,
                "codex".into(),
                &cwd,
            )
            .unwrap()
            .unwrap();
            let json = std::fs::read_to_string(repo.git_path(mergetx::LOCK_FILE).unwrap()).unwrap();
            let prepared = preparation::prepare(preparation::PreparationRequest {
                repo: &repo,
                source_repo: &repo,
                store: &store,
                slug: "alice/target",
                transaction_json: &json,
                runtime: "codex",
                cwd: &cwd,
            })
            .unwrap();
            Self {
                _directory: directory,
                repo,
                store,
                launch,
                binding: prepared.binding,
                old,
            }
        }

        fn journal(&self) -> merge_archive::ArchiveJournal {
            merge_archive::read(self.repo.root(), &self.binding.role.generation)
                .unwrap()
                .unwrap()
        }

        fn native(&self) -> link::ArchiveLinkSnapshot {
            link::read_archive_link_snapshot(&self.store, "codex", &self.binding.native.session_id)
                .unwrap()
                .unwrap()
        }

        fn cancel(&self) {
            assert_eq!(
                super::super::abort::abort(super::super::abort::AbortRequest {
                    repo: &self.repo,
                    store: &self.store,
                    binding: &self.binding,
                })
                .unwrap(),
                super::super::abort::AbortOutcome::Aborted
            );
            let restored =
                link::read_archive_link_snapshot(&self.store, "codex", &self.old.link.session_id)
                    .unwrap()
                    .unwrap();
            assert_eq!(restored.json, self.old.json);
            assert!(mergetx::read(self.repo.root()).unwrap().is_none());
        }
    }

    #[test]
    fn spawn_probe_child() {
        if let Some(path) = std::env::var_os("AGIT_ARCHIVE_SPAWN_PROBE") {
            std::fs::write(path, b"started").unwrap();
        }
    }

    #[test]
    fn launch_rechecks_retained_authority_and_leaves_failures_cancellable() {
        const CHILD: &str = "AGIT_ARCHIVE_LAUNCH_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let home = tempfile::tempdir().unwrap();
            let mut command = Command::new(std::env::current_exe().unwrap());
            command.args(["--exact", "commands::merge::archive::launch::tests::launch_rechecks_retained_authority_and_leaves_failures_cancellable", "--nocapture"])
                .env(CHILD, "1").env("HOME", home.path()).env("USERPROFILE", home.path())
                .env("CODEX_HOME", home.path().join(".codex")).env("AGIT_HOME", home.path().join("agit"))
                .env("AGIT_SECRETS_KEYSTORE", if cfg!(windows) { "os" } else { "file" });
            for name in archive_history::GIT_ROUTING_ENV {
                command.env_remove(name);
            }
            command
                .env_remove(mergetx::ENV)
                .env_remove(mergetx::GENERATION_ENV);
            let output = command.output().unwrap();
            assert!(output.status.success(), "{output:?}");
            return;
        }
        for change in [
            "none",
            "spawn-failure",
            "interrupted",
            "transaction",
            "progress",
            "link",
            "retired",
            "native",
            "head",
            "tracking",
            "aborted",
        ] {
            let fixture = Fixture::new();
            let marker = fixture.launch.cwd.join("spawn-marker");
            let mut command = Command::new(if change == "spawn-failure" {
                fixture.launch.cwd.join("missing-executable")
            } else {
                std::env::current_exe().unwrap()
            });
            command
                .args([
                    "--exact",
                    "commands::merge::archive::launch::tests::spawn_probe_child",
                ])
                .env("AGIT_ARCHIVE_SPAWN_PROBE", &marker);
            let installed = read_native(&fixture.native().link).unwrap();
            assert!(
                std::str::from_utf8(&installed)
                    .unwrap()
                    .contains("SYNTHETIC-ARCHIVE-VIEW")
            );
            assert!(
                !std::str::from_utf8(&installed)
                    .unwrap()
                    .contains("SYNTHETIC-LOG-ONLY")
            );
            let result = spawn_verified(
                &fixture.repo,
                &fixture.repo,
                &fixture.store,
                &fixture.binding,
                &fixture.launch,
                &mut command,
                || {
                    match change {
                        "interrupted" => {
                            anyhow::bail!("synthetic interruption before launch locks")
                        }
                        "transaction" | "progress" => {
                            let path = fixture.repo.git_path(mergetx::LOCK_FILE)?;
                            let mut tx: mergetx::Tx =
                                serde_json::from_slice(&std::fs::read(&path)?)?;
                            if change == "transaction" {
                                tx.generation = Some(uuid::Uuid::now_v7().to_string());
                            } else {
                                tx.summary = Some("changed progress".into());
                            }
                            std::fs::write(path, serde_json::to_vec_pretty(&tx)?)?;
                        }
                        "link" | "retired" => {
                            let sid = if change == "link" {
                                &fixture.binding.native.session_id
                            } else {
                                &fixture.old.link.session_id
                            };
                            let path = link::link_path(&fixture.store, "codex", sid);
                            let mut json: serde_json::Value =
                                serde_json::from_slice(&std::fs::read(&path)?)?;
                            json["cwd"] = serde_json::json!(fixture.launch.cwd.join("other"));
                            std::fs::write(path, serde_json::to_vec_pretty(&json)?)?;
                        }
                        "native" => {
                            use std::io::Write;
                            std::fs::OpenOptions::new()
                                .append(true)
                                .open(fixture.native().link.resolve().unwrap())?
                                .write_all(
                                    b"{\"type\":\"user\",\"text\":\"unexpected writer\"}\n",
                                )?;
                        }
                        "head" => {
                            fixture.repo.git(&[
                                "commit",
                                "--allow-empty",
                                "-m",
                                "independent target movement",
                            ])?;
                        }
                        "tracking" => {
                            fixture.repo.git(&["config", "branch.work.remote", "."])?;
                            fixture.repo.git(&[
                                "config",
                                "branch.work.merge",
                                "refs/heads/source",
                            ])?;
                        }
                        "aborted" => fixture.cancel(),
                        _ => {}
                    }
                    Ok(())
                },
            );
            if change == "none" {
                assert!(result.unwrap().wait().unwrap().success());
                assert_eq!(std::fs::read(&marker).unwrap(), b"started");
            } else {
                assert!(result.is_err(), "{change} must refuse launch");
                assert!(!marker.exists(), "{change} must not spawn a runtime");
            }
            if matches!(change, "none" | "spawn-failure" | "interrupted") {
                assert_eq!(fixture.journal().phase, ArchivePhase::Open);
                assert_eq!(
                    super::super::current_head(&fixture.repo, "work").unwrap(),
                    fixture.binding.role.origin_head
                );
                assert_eq!(read_native(&fixture.native().link).unwrap(), installed);
                fixture.cancel();
            }
        }
    }
}
