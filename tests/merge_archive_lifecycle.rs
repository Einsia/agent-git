//! Public lifecycle commands consume retained Archive identity without selecting new authority.

use agit::commands::{merge, plumbing};
use agit::domain::{
    archive_history,
    link::{self, Link},
    merge_archive::{
        self, ArchiveJournal, ArchiveJournalGuard, ArchivePhase, ArchivePublicationKind,
        ExplorationBinding, FrozenMergeSource, MergeArchiveRole, PreparedActivation,
        PreparedArchivePublication, PreparedDetach, PreviousClaim, RetainedAbort,
        RetainedMergeLanding, RuntimeLinkKey,
    },
    mergetx, meta,
    native_archive::Frontier,
    repo::Repo,
    storage,
    store::Store,
    transcript,
};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

#[cfg(windows)]
#[path = "../src/infra/windows_security.rs"]
#[allow(dead_code)]
mod security;

const SESSION: &str = "agit-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const NATIVE: &str = "aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa";
const OLD: &str = "bbbbbbbb-2222-4222-8222-bbbbbbbbbbbb";
const TARGET: &str = "alice/target@work";
const EXPLORATION: &str = "{\"type\":\"assistant\",\"text\":\"SYNTHETIC-PRIVATE-EXPLORATION\"}\n";

fn private_write(path: &Path, bytes: &[u8]) {
    #[cfg(windows)]
    {
        if path.exists() {
            fs::remove_file(path).unwrap();
        }
        security::write_private_file(path, bytes).unwrap();
    }
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(path)
            .unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
    }
}

struct Lab {
    _temporary: tempfile::TempDir,
    home: PathBuf,
    agit: PathBuf,
    repo: Repo,
    source: Repo,
    store: Store,
    binding: ExplorationBinding,
    preparing: ArchiveJournal,
    native: PathBuf,
    original_log: String,
    source_log: String,
}

impl Lab {
    fn new(summary: bool) -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let home = root.join("home");
        let agit = root.join("agit");
        fs::create_dir_all(&home).unwrap();
        let repo = Repo::init(&agit.join("repos/alice/target")).unwrap();
        let source = Repo::init(&agit.join("repos/alice/source")).unwrap();
        for repo in [&repo, &source] {
            repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        }
        let mut metadata = meta::Meta::new(
            SESSION.into(),
            "codex".into(),
            home.to_string_lossy().into_owned(),
        );
        metadata.turn = Some(1);
        meta::write(repo.root(), &metadata).unwrap();
        let original_log = transcript::wrap_lines(
            "{\"type\":\"user\",\"text\":\"SYNTHETIC-TARGET\"}\n",
            "codex",
            SESSION,
        );
        storage::write_snapshot(repo.root(), &original_log, &original_log).unwrap();
        fs::write(
            repo.root().join("AGENTS.md"),
            "Synthetic target instructions\n",
        )
        .unwrap();
        repo.add_all().unwrap();
        repo.commit("synthetic archive target").unwrap();
        let origin = repo.git(&["rev-parse", "HEAD"]).unwrap();
        repo.git(&["update-ref", "refs/heads/work", &origin])
            .unwrap();
        repo.git(&["symbolic-ref", "HEAD", "refs/heads/work"])
            .unwrap();
        plumbing::import_commit_graph(&source, &repo, &origin).unwrap();
        let event = transcript::wrap_lines(
            "{\"type\":\"user\",\"text\":\"SYNTHETIC-SOURCE\"}\n",
            "codex",
            SESSION,
        );
        let source_log = format!("{original_log}{event}");
        metadata.turn = Some(2);
        let mut files = storage::snapshot_files(&source_log, &source_log).unwrap();
        files.insert(
            meta::FILE.into(),
            meta::to_text(&metadata).unwrap().into_bytes(),
        );
        let tree = plumbing::tree_apply_owned(
            &source,
            &origin,
            files
                .into_iter()
                .map(|(path, bytes)| (path, Some(bytes)))
                .collect(),
        )
        .unwrap();
        let source_head =
            plumbing::commit_tree(&source, &tree, &[&origin], "synthetic frozen source").unwrap();
        source
            .git(&["update-ref", "refs/heads/source", &source_head])
            .unwrap();
        let native = home
            .join(".codex/sessions/2026/09/10")
            .join(format!("rollout-2026-09-10T00-00-00-{NATIVE}.jsonl"));
        fs::create_dir_all(native.parent().unwrap()).unwrap();
        let baseline = format!(
            "{}\n",
            serde_json::json!({"type":"session_meta","payload":{"id":NATIVE,"cwd":home}})
        );
        fs::write(&native, &baseline).unwrap();
        let binding = ExplorationBinding {
            role: MergeArchiveRole {
                generation: uuid::Uuid::now_v7().to_string(),
                slug: "alice/target".into(),
                branch: "work".into(),
                origin_head: origin.clone(),
                logical_session: SESSION.into(),
            },
            native: RuntimeLinkKey {
                runtime: "codex".into(),
                session_id: NATIVE.into(),
            },
            installed: Frontier {
                bytes: baseline.len() as u64,
                sha256: hex::encode(Sha256::digest(baseline.as_bytes())),
            },
            source: FrozenMergeSource {
                reference: "alice/source@source".into(),
                slug: "alice/source".into(),
                branch: Some("source".into()),
                head: source_head,
                base: Some(origin.clone()),
            },
        };
        let mut installed = Link::new("codex", NATIVE, Some(&home));
        installed.owner = Some("alice".into());
        installed.agent = Some("target".into());
        installed.branch = Some("work".into());
        installed.materialized_from = Some(origin.clone());
        installed.baseline_bytes = Some(binding.installed.bytes);
        installed.baseline_hash = Some(binding.installed.sha256.clone());
        let mut old = installed.clone();
        old.session_id = OLD.into();
        let original_json = format!(
            "{},\"future\":9007199254740993.0000000000000001}}\n",
            old.to_json().unwrap().strip_suffix('}').unwrap()
        );
        old.superseded_by = Some(format!("codex/{NATIVE}"));
        let retired_json = format!(
            "{},\"future\":9007199254740993.0000000000000001}}\n",
            old.to_json().unwrap().strip_suffix('}').unwrap()
        );
        installed.merge_archive = Some(binding.role.clone());
        let successor_json = format!("{}\n", installed.to_json().unwrap());
        let tx = mergetx::Tx {
            mode: Some(mergetx::Mode::SessionAgent),
            exploration: None,
            generation: Some(binding.role.generation.clone()),
            target: "work".into(),
            source: binding.source.reference.clone(),
            source_repo: Some(binding.source.slug.clone()),
            source_branch: binding.source.branch.clone(),
            base: origin.clone(),
            target_head: origin,
            source_head: binding.source.head.clone(),
            picked: vec![format!("{}#2.1", binding.source.reference)],
            summary: summary.then(|| "Retain the selected source decision".into()),
        };
        mergetx::create(repo.root(), &tx).unwrap();
        let transaction_original_json =
            fs::read_to_string(repo.git_path(mergetx::LOCK_FILE).unwrap()).unwrap();
        let mut bound = tx;
        bound.exploration = Some(binding.clone());
        let transaction_bound_json = format!("{}\n", serde_json::to_string(&bound).unwrap());
        let preparing = ArchiveJournal {
            version: merge_archive::VERSION,
            binding: binding.clone(),
            phase: ArchivePhase::Preparing,
            consumed: binding.installed.clone(),
            opencode: None,
            accepted_commit: None,
            publication: None,
            landing: None,
            abort: None,
            previous_claims: vec![PreviousClaim {
                native: RuntimeLinkKey {
                    runtime: "codex".into(),
                    session_id: OLD.into(),
                },
                original_json,
                retired_json,
            }],
            activation: Some(PreparedActivation {
                successor_json,
                transaction_original_json,
                transaction_bound_json,
            }),
            detach: None,
        };
        let store = Store::at(agit.join("store"));
        {
            let _branch = link::lock_branch(&store, "alice/target", "work").unwrap();
            let _new = link::lock(&store, "codex", NATIVE).unwrap();
            let _old = link::lock(&store, "codex", OLD).unwrap();
            link::publish_archive_transition_locked(
                &store,
                "codex",
                OLD,
                None,
                &preparing.previous_claims[0].original_json,
            )
            .unwrap();
            let guard =
                ArchiveJournalGuard::acquire(repo.root(), &binding.role.generation).unwrap();
            let _control = mergetx::ControlGuard::acquire(repo.root()).unwrap();
            guard.create(&preparing).unwrap();
        }
        fs::write(agit.join("layout-v1.complete"), b"1\n").unwrap();
        Self {
            _temporary: temporary,
            home,
            agit,
            repo,
            source,
            store,
            binding,
            preparing,
            native,
            original_log,
            source_log,
        }
    }

    fn command(&self, args: &[&str], exact_generation: bool) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command
            .args(args)
            .current_dir(&self.home)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("AGIT_HOME", &self.agit)
            .env("CODEX_HOME", self.home.join(".codex"))
            .env("CLAUDE_CONFIG_DIR", self.home.join(".claude"))
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_DATA_HOME", self.home.join(".local/share"))
            .env("AGIT_HUB_URL", "http://127.0.0.1:1")
            .env("GIT_CONFIG_GLOBAL", self.home.join("empty-gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(windows) { "os" } else { "file" },
            )
            .env("CI", "1")
            .env("NO_COLOR", "1")
            .stdin(Stdio::null());
        if exact_generation {
            command
                .env(mergetx::ENV, TARGET)
                .env(mergetx::GENERATION_ENV, &self.binding.role.generation);
        }
        #[cfg(windows)]
        {
            for name in ["SystemRoot", "WINDIR", "TEMP", "TMP", "ComSpec"] {
                if let Some(value) = std::env::var_os(name) {
                    command.env(name, value);
                }
            }
            command.env("USERPROFILE", &self.home);
        }
        command
    }
    fn run(&self, action: &str, exact: bool) -> Output {
        self.command(&["merge", "--into", TARGET, action], exact)
            .output()
            .unwrap()
    }
    fn success(&self, action: &str, exact: bool) -> Output {
        let output = self.run(action, exact);
        assert!(output.status.success(), "{output:?}");
        output
    }
    fn tx_path(&self) -> PathBuf {
        self.repo.git_path(mergetx::LOCK_FILE).unwrap()
    }
    fn head(&self) -> String {
        self.repo.git(&["rev-parse", "refs/heads/work"]).unwrap()
    }
    fn journal(&self) -> ArchiveJournal {
        merge_archive::read(self.repo.root(), &self.binding.role.generation)
            .unwrap()
            .unwrap()
    }
    fn advance(&self, open: bool) {
        let _branch = link::lock_branch(&self.store, "alice/target", "work").unwrap();
        let _new = link::lock(&self.store, "codex", NATIVE).unwrap();
        let _old = link::lock(&self.store, "codex", OLD).unwrap();
        let guard =
            ArchiveJournalGuard::acquire(self.repo.root(), &self.binding.role.generation).unwrap();
        let control = mergetx::ControlGuard::acquire(self.repo.root()).unwrap();
        let activation = self.preparing.activation.as_ref().unwrap();
        link::publish_archive_transition_locked(
            &self.store,
            "codex",
            NATIVE,
            None,
            &activation.successor_json,
        )
        .unwrap();
        let previous = &self.preparing.previous_claims[0];
        link::publish_archive_transition_locked(
            &self.store,
            "codex",
            OLD,
            Some(&previous.original_json),
            &previous.retired_json,
        )
        .unwrap();
        if open {
            control
                .publish_activation_binding(
                    &activation.transaction_original_json,
                    &activation.transaction_bound_json,
                )
                .unwrap();
            let mut next = self.preparing.clone();
            next.phase = ArchivePhase::Open;
            next.activation = None;
            guard.replace(&self.preparing, &next).unwrap();
        }
    }
    fn aborting(&self) {
        let journal = self.journal();
        let mut next = journal.clone();
        next.phase = ArchivePhase::Aborting;
        next.abort = Some(RetainedAbort {
            expected_head: self.binding.role.origin_head.clone(),
            transaction_json: fs::read_to_string(self.tx_path()).unwrap(),
            successor_json: Some(
                fs::read_to_string(link::link_path(&self.store, "codex", NATIVE)).unwrap(),
            ),
            activation: None,
            cancelled_publication: None,
        });
        ArchiveJournalGuard::acquire(self.repo.root(), &self.binding.role.generation)
            .unwrap()
            .replace(&journal, &next)
            .unwrap();
    }
    fn pending(&self, visible: bool) -> String {
        plumbing::import_commit_graph(&self.repo, &self.source, &self.binding.source.head).unwrap();
        let tx: mergetx::Tx = serde_json::from_slice(&fs::read(self.tx_path()).unwrap()).unwrap();
        let mark_start = merge::marker_envelope("__merge_start__", "codex", SESSION, &tx.source);
        let mark_end = merge::marker_envelope("__merge_end__", "codex", SESSION, &tx.source);
        let summary = merge::summary_envelope(&tx.summary_text(), "codex", SESSION);
        let ordinary_log = format!(
            "{}{mark_start}{}{summary}{mark_end}",
            self.original_log, self.source_log
        );
        let mut files = storage::snapshot_files(&ordinary_log, &ordinary_log).unwrap();
        let mut metadata = meta::read_at_ref(&self.repo, &self.binding.role.origin_head).unwrap();
        metadata.kind = meta::Kind::Merge;
        files.insert(
            meta::FILE.into(),
            meta::to_text(&metadata).unwrap().into_bytes(),
        );
        let ordinary = plumbing::tree_apply_owned(
            &self.repo,
            &self.binding.role.origin_head,
            files
                .into_iter()
                .map(|(path, bytes)| (path, Some(bytes)))
                .collect(),
        )
        .unwrap();
        let candidate = plumbing::commit_tree(
            &self.repo,
            &ordinary,
            &[&self.binding.role.origin_head, &self.binding.source.head],
            "synthetic retained landing",
        )
        .unwrap();
        archive_history::verify_merge_landing(
            &self.repo,
            &self.binding.role.origin_head,
            &self.binding.source.head,
            &ordinary,
            &candidate,
            &ordinary,
        )
        .unwrap();
        let journal = self.journal();
        let mut pending = journal.clone();
        pending.landing = Some(RetainedMergeLanding {
            transaction_json: fs::read_to_string(self.tx_path()).unwrap(),
            ordinary_tree: ordinary.clone(),
            worktree_tree: None,
        });
        pending.publication = Some(PreparedArchivePublication {
            kind: ArchivePublicationKind::MergeLanding {
                source_head: self.binding.source.head.clone(),
            },
            link_json: fs::read_to_string(link::link_path(&self.store, "codex", NATIVE)).unwrap(),
            expected_old: self.binding.role.origin_head.clone(),
            candidate: candidate.clone(),
            candidate_tree: ordinary,
            prior_frontier: self.binding.installed.clone(),
            next_frontier: self.binding.installed.clone(),
            next_opencode: None,
            appended_records: 0,
            protected_suffix_sha256: hex::encode(Sha256::digest([])),
        });
        ArchiveJournalGuard::acquire(self.repo.root(), &self.binding.role.generation)
            .unwrap()
            .replace(&journal, &pending)
            .unwrap();
        self.repo
            .git(&["checkout", "--detach", &self.binding.role.origin_head])
            .unwrap();
        if visible {
            self.repo
                .git(&["update-ref", "refs/heads/work", &candidate])
                .unwrap();
        }
        candidate
    }
    fn facts(&self) -> BTreeMap<String, Vec<u8>> {
        let mut facts = BTreeMap::new();
        facts.insert("refs".into(), self.repo.git_bytes(&["show-ref"]).unwrap());
        for (name, path) in [
            ("tx", self.tx_path()),
            ("native", self.native.clone()),
            ("old", link::link_path(&self.store, "codex", OLD)),
            ("successor", link::link_path(&self.store, "codex", NATIVE)),
            ("index", self.repo.git_path("index").unwrap()),
            ("HEAD", self.repo.git_path("HEAD").unwrap()),
            ("shared", self.repo.root().join("AGENTS.md")),
            (
                "landed-receipt",
                self.tx_path()
                    .with_extension(format!("landed-{}.json", self.binding.role.generation)),
            ),
            (
                "aborted-receipt",
                self.tx_path()
                    .with_extension(format!("aborted-{}.json", self.binding.role.generation)),
            ),
        ] {
            if path.exists() {
                facts.insert(name.into(), fs::read(path).unwrap());
            }
        }
        let directory = self.repo.git_path(merge_archive::DIRECTORY).unwrap();
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path
                .extension()
                .is_some_and(|ext| ext == "json" || ext == "recovery")
            {
                facts.insert(
                    format!("journal/{}", path.file_name().unwrap().to_string_lossy()),
                    fs::read(path).unwrap(),
                );
            }
        }
        facts
    }
}

#[cfg(windows)]
impl Drop for Lab {
    fn drop(&mut self) {
        use agit::domain::secret_filter::KeyStore;
        for path in [
            self.agit.join("secret-filter/vault.json"),
            self.repo
                .root()
                .join(".git/agit/secret-dictionary/vault.json"),
        ] {
            if let Ok(bytes) = fs::read(path)
                && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes)
                && let Some(id) = value.get("vault_id").and_then(serde_json::Value::as_str)
            {
                let _ = agit::domain::secret_filter::OsKeyStore.delete(id);
            }
        }
    }
}

#[test]
fn preparing_and_open_abort_restore_claims_without_summary_source_or_native() {
    for stage in 0..3 {
        let lab = Lab::new(false);
        if stage > 0 {
            lab.advance(stage == 2);
        }
        fs::rename(lab.source.root(), lab.home.join("unavailable-source")).unwrap();
        fs::write(&lab.native, b"invalid native evidence remains retained").unwrap();
        let before = lab.facts();
        lab.success("--abort", false);
        assert_eq!(lab.journal().phase, ArchivePhase::Aborted);
        assert_eq!(lab.head(), lab.binding.role.origin_head);
        assert_eq!(
            fs::read(link::link_path(&lab.store, "codex", OLD)).unwrap(),
            lab.preparing.previous_claims[0].original_json.as_bytes()
        );
        assert!(!lab.tx_path().exists());
        let after = lab.facts();
        for name in ["refs", "native", "index", "HEAD", "shared", "successor"] {
            assert_eq!(before.get(name), after.get(name), "{name}");
        }
        assert!(!lab.run("--abort", false).status.success());
        assert_eq!(lab.facts(), after);
        lab.success("--abort", true);
        assert_eq!(lab.facts(), after);
    }
}

#[test]
fn fresh_continue_uses_frozen_source_after_its_branch_moves_or_disappears() {
    for deleted in [false, true] {
        let lab = Lab::new(true);
        lab.advance(true);
        if deleted {
            lab.source
                .git(&["update-ref", "-d", "refs/heads/source"])
                .unwrap();
        } else {
            lab.source
                .git(&[
                    "update-ref",
                    "refs/heads/source",
                    &lab.binding.role.origin_head,
                ])
                .unwrap();
        }
        let baseline = fs::read_to_string(&lab.native).unwrap();
        fs::write(&lab.native, format!("{baseline}{EXPLORATION}")).unwrap();
        let native = fs::read(&lab.native).unwrap();
        let source_refs = lab.source.git(&["for-each-ref"]).unwrap();
        lab.success("--continue", false);
        let head = lab.head();
        assert_eq!(
            lab.repo.git(&["show", "-s", "--format=%P", &head]).unwrap(),
            format!(
                "{} {}",
                lab.binding.role.origin_head, lab.binding.source.head
            )
        );
        let view = storage::materialize_at(lab.repo.root(), &head, meta::VIEW_FILE).unwrap();
        let log = storage::materialize_at(lab.repo.root(), &head, meta::LOG_FILE).unwrap();
        assert!(view.contains("SYNTHETIC-SOURCE"));
        assert!(!view.contains("SYNTHETIC-PRIVATE-EXPLORATION"));
        assert_eq!(log.matches("SYNTHETIC-PRIVATE-EXPLORATION").count(), 1);
        assert_eq!(fs::read(&lab.native).unwrap(), native);
        assert_eq!(lab.source.git(&["for-each-ref"]).unwrap(), source_refs);
        let before = lab.facts();
        lab.success("--continue", true);
        assert_eq!(lab.facts(), before);
    }
}

#[test]
fn pending_and_visible_replay_never_require_source_native_or_vault() {
    for action in ["--continue", "--abort"] {
        for visible in [false, true] {
            let lab = Lab::new(true);
            lab.advance(true);
            let candidate = lab.pending(visible);
            fs::rename(lab.source.root(), lab.home.join("unavailable-source")).unwrap();
            fs::remove_file(&lab.native).unwrap();
            fs::create_dir_all(lab.agit.join("secret-filter")).unwrap();
            fs::write(
                lab.agit.join("secret-filter/vault.json"),
                b"unreadable vault",
            )
            .unwrap();
            let output = lab.success(action, false);
            if action == "--abort" && !visible {
                assert_eq!(lab.head(), lab.binding.role.origin_head);
                assert_eq!(lab.journal().phase, ArchivePhase::Aborted);
            } else {
                assert_eq!(lab.head(), candidate);
                assert_eq!(
                    lab.journal().phase,
                    ArchivePhase::Landed {
                        merge_commit: candidate
                    }
                );
                if action == "--abort" {
                    assert!(
                        String::from_utf8(output.stdout)
                            .unwrap()
                            .contains("already landed")
                    );
                }
            }
            assert!(!lab.native.exists());
            assert!(!lab.tx_path().exists());
            let before = lab.facts();
            lab.success(action, true);
            assert_eq!(lab.facts(), before);
        }
    }
}

#[test]
fn progress_refuses_preparing_pending_aborting_and_terminal_receipts() {
    for phase in [
        "preparing",
        "pending",
        "aborting",
        "landed",
        "aborted",
        "changed-mode",
    ] {
        let lab = Lab::new(true);
        if phase != "preparing" {
            lab.advance(true);
        }
        match phase {
            "pending" => {
                lab.pending(false);
            }
            "aborting" => lab.aborting(),
            "changed-mode" => {
                let mut tx: mergetx::Tx =
                    serde_json::from_slice(&fs::read(lab.tx_path()).unwrap()).unwrap();
                tx.mode = Some(mergetx::Mode::Manual);
                tx.exploration = None;
                private_write(&lab.tx_path(), &serde_json::to_vec(&tx).unwrap());
            }
            "landed" => {
                lab.pending(true);
                lab.success("--continue", false);
            }
            "aborted" => {
                lab.success("--abort", false);
            }
            _ => {}
        }
        for command in [
            vec!["pick", "#1.1"],
            vec!["drop", "#2.1"],
            vec!["summary", "-m", "replacement"],
        ] {
            let before = lab.facts();
            let mut args = vec!["merge", "--into", TARGET];
            args.extend(command);
            let output = lab.command(&args, true).output().unwrap();
            assert!(!output.status.success(), "{phase}: {output:?}");
            assert_eq!(lab.facts(), before);
        }
        if phase == "aborting" {
            lab.success("--abort", true);
        }
    }
}

#[test]
fn open_progress_remains_editable_and_a_stale_agent_cannot_select_its_replacement() {
    let lab = Lab::new(true);
    lab.advance(true);
    for command in [
        vec!["drop", "#2.1"],
        vec!["pick", "#2.1"],
        vec!["summary", "-m", "Explicit reconciliation"],
    ] {
        let mut args = vec!["merge", "--into", TARGET];
        args.extend(command);
        let output = lab.command(&args, true).output().unwrap();
        assert!(output.status.success(), "{output:?}");
    }
    lab.success("--abort", true);
    let mut replacement: mergetx::Tx = serde_json::from_str(
        &lab.preparing
            .activation
            .as_ref()
            .unwrap()
            .transaction_original_json,
    )
    .unwrap();
    replacement.generation = Some(uuid::Uuid::now_v7().to_string());
    replacement.mode = Some(mergetx::Mode::Manual);
    mergetx::create(lab.repo.root(), &replacement).unwrap();
    let before = lab.facts();
    for action in ["--continue", "--abort"] {
        assert!(!lab.run(action, true).status.success());
        assert_eq!(lab.facts(), before);
    }
    lab.success("--abort", false);
    replacement.generation = Some(uuid::Uuid::now_v7().to_string());
    replacement.mode = Some(mergetx::Mode::FileAgent);
    mergetx::create(lab.repo.root(), &replacement).unwrap();
    lab.success("--abort", false);
}

#[test]
fn missing_transaction_and_missing_completion_carrier_are_not_success_evidence() {
    for completed in [false, true] {
        let lab = Lab::new(false);
        lab.advance(true);
        if completed {
            lab.success("--abort", false);
            fs::remove_file(
                lab.tx_path()
                    .with_extension(format!("aborted-{}.json", lab.binding.role.generation)),
            )
            .unwrap();
        } else {
            fs::remove_file(lab.tx_path()).unwrap();
        }
        for exact in [false, true] {
            let before = lab.facts();
            assert!(!lab.run("--abort", exact).status.success());
            assert_eq!(lab.facts(), before);
        }
    }
}

#[test]
fn changed_journal_route_generation_claim_or_successor_refuses_without_overwrite() {
    for changed in ["route", "generation", "claim", "successor", "mode"] {
        let lab = Lab::new(false);
        lab.advance(true);
        let mut command = lab.command(&["merge", "--into", TARGET, "--abort"], true);
        match changed {
            "route" => {
                command.env(mergetx::ENV, "other/target@work");
            }
            "generation" => {
                command.env(mergetx::GENERATION_ENV, uuid::Uuid::now_v7().to_string());
            }
            "mode" => {
                let mut tx: mergetx::Tx =
                    serde_json::from_slice(&fs::read(lab.tx_path()).unwrap()).unwrap();
                tx.mode = Some(mergetx::Mode::Manual);
                tx.exploration = None;
                private_write(&lab.tx_path(), &serde_json::to_vec(&tx).unwrap());
            }
            _ => {
                let id = if changed == "claim" { OLD } else { NATIVE };
                let path = link::link_path(&lab.store, "codex", id);
                let text = fs::read_to_string(&path).unwrap();
                if changed == "claim" {
                    private_write(&path, format!("{text} \n").as_bytes());
                } else {
                    let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
                    value["baseline_bytes"] = (lab.binding.installed.bytes + 1).into();
                    private_write(&path, &serde_json::to_vec(&value).unwrap());
                }
            }
        }
        let before = lab.facts();
        let output = command.output().unwrap();
        assert!(!output.status.success(), "{changed}: {output:?}");
        assert_eq!(lab.facts(), before);
    }
}

#[test]
fn continuation_collects_only_the_registered_target_checkout() {
    let lab = Lab::new(true);
    lab.advance(true);
    lab.repo
        .git(&["checkout", "--detach", &lab.binding.role.origin_head])
        .unwrap();
    let holder = lab.repo.root().parent().unwrap().join("linked-target");
    lab.repo
        .git(&["worktree", "add", "--", "../linked-target", "work"])
        .unwrap();
    fs::write(
        holder.join("AGENTS.md"),
        "Synthetic chosen holder instructions\n",
    )
    .unwrap();
    fs::write(
        lab.repo.root().join("AGENTS.md"),
        "Synthetic unrelated primary instructions\n",
    )
    .unwrap();
    lab.success("--continue", false);
    let head = lab.head();
    assert_eq!(
        lab.repo
            .git_bytes(&["show", &format!("{head}:AGENTS.md")])
            .unwrap(),
        b"Synthetic chosen holder instructions\n"
    );
    assert_eq!(
        fs::read(lab.repo.root().join("AGENTS.md")).unwrap(),
        b"Synthetic unrelated primary instructions\n"
    );
    assert_eq!(
        fs::read(holder.join("AGENTS.md")).unwrap(),
        b"Synthetic chosen holder instructions\n"
    );
}

#[test]
fn json_lifecycle_keeps_visible_history_and_structures_refusals() {
    for version in [1, 2] {
        let lab = Lab::new(true);
        lab.advance(true);
        let candidate = lab.pending(true);
        let version_arg = version.to_string();
        let output = lab
            .command(
                &[
                    "--json",
                    "--json-version",
                    &version_arg,
                    "merge",
                    "--into",
                    TARGET,
                    "--abort",
                ],
                false,
            )
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["schema"], "cli-output");
        assert_eq!(value["schema_version"], version);
        assert_eq!(value["exit_code"], 0);
        assert_eq!(lab.head(), candidate);
        let before = lab.facts();
        let output = lab
            .command(
                &[
                    "--json",
                    "--json-version",
                    &version_arg,
                    "merge",
                    "--into",
                    TARGET,
                    "--continue",
                ],
                true,
            )
            .env(mergetx::GENERATION_ENV, uuid::Uuid::now_v7().to_string())
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stderr.is_empty(), "{output:?}");
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["exit_code"], output.status.code().unwrap());
        assert_eq!(lab.facts(), before);
    }
}

const SETTLEMENT_NATIVE: &str = "AGIT_SETTLEMENT_NATIVE";
const SETTLEMENT_ROLE: &str = "AGIT_SETTLEMENT_ARCHIVE_ROLE";
const SETTLEMENT_RESULT: &str = "AGIT_RC_SUPERVISOR_COMMIT_RESULT";
const AGENT_ID: &str = "cccccccc-3333-4333-8333-cccccccccccc";

impl Lab {
    fn settlement_ready(&self) {
        let hub = "http://127.0.0.1:1";
        agit::infra::credentials::save_at(
            &self.agit.join("credentials").join(format!(
                "{}.json",
                agit::infra::config::hub_host_key(hub).unwrap()
            )),
            &agit::infra::credentials::HubCredential {
                username: "alice".into(),
                email: None,
                hub: Some(hub.into()),
                access_token: "synthetic".into(),
                refresh_token: "synthetic".into(),
                access_expires_at: "2099-01-01T00:00:00Z".into(),
                refresh_expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .unwrap();
        self.repo
            .git(&[
                "config",
                "agit.remoteIdentity",
                &serde_json::json!({"hub":hub,"agent_id":AGENT_ID}).to_string(),
            ])
            .unwrap();
        // Run the actual startup before measuring settlement effects; marker bytes are not a
        // substitute for the CLI's migration and configuration admission.
        fs::remove_file(self.agit.join("layout-v1.complete")).unwrap();
        let output = self
            .command(&["config", "commit.auto", "true"], false)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }

    fn append_native(&self, suffix: &str) {
        use std::io::Write;
        fs::OpenOptions::new()
            .append(true)
            .open(&self.native)
            .unwrap()
            .write_all(suffix.as_bytes())
            .unwrap();
    }

    fn settlement_command(&self, entry: &str) -> Command {
        let mut command = match entry {
            "installed" => self.command(&["hooks", "settle", "--runtime", "codex"], false),
            "legacy" => self.command(&["commit", "--from-hook"], false),
            "explicit" => self.command(&["commit", NATIVE], false),
            "process" => self.command(&["commit"], true),
            "strict" => self.command(&["commit", "--from-supervisor"], false),
            _ => panic!("unknown settlement entry"),
        };
        // Deliberately stale process context must not override the hook's native payload.
        command
            .env("AGIT_SESSION", "alice/source@source")
            .env("CODEX_SESSION_ID", OLD);
        if matches!(entry, "process" | "strict") {
            command.env(
                SETTLEMENT_NATIVE,
                serde_json::to_string(&self.binding.native).unwrap(),
            );
        }
        if entry == "strict" {
            command
                .env("AGIT_SESSION", TARGET)
                .env(
                    SETTLEMENT_ROLE,
                    serde_json::to_string(&self.binding.role).unwrap(),
                )
                .env("AGIT_EXPECTED_AGENT_ID", AGENT_ID)
                .env(SETTLEMENT_RESULT, self.home.join("strict-result"));
        }
        command
    }

    fn settle_with(&self, mut command: Command, entry: &str) -> Output {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        if matches!(entry, "installed" | "legacy") {
            let payload = serde_json::json!({"session_id":NATIVE,"transcript_path":self.native,"cwd":self.home});
            self.hook_payload(command, payload.to_string().as_bytes())
        } else {
            command.output().unwrap()
        }
    }

    fn hook_payload(&self, mut command: Command, payload: &[u8]) -> Output {
        use std::io::Write;
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        if let Err(error) = child.stdin.take().unwrap().write_all(payload) {
            assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
        }
        child.wait_with_output().unwrap()
    }

    fn settle(&self, entry: &str) -> Output {
        self.settle_with(self.settlement_command(entry), entry)
    }
}

/// Session entry reports retained Archive identity without making it an ordinary writable claim.
#[test]
fn archive_ingest_preserves_exact_role_context_without_rewriting_authority() {
    let lab = Lab::new(true);
    lab.settlement_ready();
    lab.advance(true);
    let selected = link::read_archive_link_snapshot(&lab.store, "codex", NATIVE)
        .unwrap()
        .unwrap();
    assert!(!selected.link.is_active());
    let ingest = |source: &str| {
        let payload = serde_json::json!({
            "session_id": NATIVE,
            "transcript_path": lab.native,
            "cwd": lab.home,
            "source": source,
        });
        let mut command = lab.command(&["hooks", "ingest", "--runtime", "codex"], false);
        command.env("AGIT_SESSION", "alice/source@source");
        let output = lab.hook_payload(command, payload.to_string().as_bytes());
        assert!(output.status.success(), "{output:?}");
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    for source in ["startup", "resume"] {
        let before = lab.facts();
        let response = ingest(source);
        let context = response["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(context.contains("'alice/target@work'"), "{response}");
        assert!(
            context.contains("Do not import this session again"),
            "{response}"
        );
        assert!(!context.contains("alice/source@source"), "{response}");
        assert_eq!(lab.facts(), before);
    }
    for change in ["superseded", "owner", "branch", "generation"] {
        let mut invalid = selected.link.clone();
        match change {
            "superseded" => invalid.superseded_by = Some(format!("codex/{OLD}")),
            "owner" => invalid.owner = Some("other".into()),
            "branch" => invalid.branch = Some("other".into()),
            "generation" => invalid.merge_archive.as_mut().unwrap().generation.clear(),
            _ => unreachable!(),
        }
        private_write(
            &link::link_path(&lab.store, "codex", NATIVE),
            invalid.to_json().unwrap().as_bytes(),
        );
        let before = lab.facts();
        let response = ingest("resume");
        let context = response["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(
            context.contains("no active agit branch"),
            "{change}: {response}"
        );
        assert!(
            !context.contains("'alice/target@work'"),
            "{change}: {response}"
        );
        assert_eq!(lab.facts(), before, "{change}");
    }
}

/// Required Archive capture cannot report success for an event whose native key is missing.
/// Delegation and disabled auto settlement remain earlier gates and never inspect native state.
#[test]
fn archive_hooks_refuse_invalid_payloads_without_falling_back_to_process_identity() {
    let lab = Lab::new(true);
    lab.settlement_ready();
    lab.advance(true);
    lab.append_native(EXPLORATION);
    let invalid: &[&[u8]] = &[
        b"{invalid",
        b"{}",
        b"{\"session_id\":\"\"}",
        b"{\"session_id\":42}",
        b"",
        b" \n",
        b"\xff",
    ];
    for authority in ["launch", "role"] {
        for entry in ["installed", "legacy"] {
            for payload in invalid {
                let mut command = lab.settlement_command(entry);
                command.env(
                    SETTLEMENT_NATIVE,
                    serde_json::to_string(&lab.binding.native).unwrap(),
                );
                if authority == "launch" {
                    command
                        .env(mergetx::ENV, TARGET)
                        .env(mergetx::GENERATION_ENV, &lab.binding.role.generation);
                } else {
                    command.env(
                        SETTLEMENT_ROLE,
                        serde_json::to_string(&lab.binding.role).unwrap(),
                    );
                }
                let before = lab.facts();
                let output = lab.hook_payload(command, payload);
                assert!(
                    !output.status.success(),
                    "{authority}/{entry}/{payload:?}: {output:?}"
                );
                assert!(
                    String::from_utf8_lossy(&output.stderr)
                        .contains("archive hook payload has no native session identity"),
                    "{output:?}"
                );
                assert_eq!(lab.facts(), before);
                assert!(!lab.home.join("strict-result").exists());
            }
        }
    }
    for entry in ["installed", "legacy"] {
        let before = lab.facts();
        let output = lab.hook_payload(lab.settlement_command(entry), b"{}");
        assert!(output.status.success(), "ordinary invalid hook: {output:?}");
        assert_eq!(lab.facts(), before);
    }
    for entry in ["installed", "legacy"] {
        let payload = serde_json::json!({
            "session_id": "dddddddd-5555-4555-8555-dddddddddddd",
            "transcript_path": lab.native,
        });
        let mut command = lab.settlement_command(entry);
        command
            .env(
                SETTLEMENT_NATIVE,
                serde_json::to_string(&lab.binding.native).unwrap(),
            )
            .env(mergetx::ENV, TARGET)
            .env(mergetx::GENERATION_ENV, &lab.binding.role.generation);
        let before = lab.facts();
        let output = lab.hook_payload(command, payload.to_string().as_bytes());
        assert!(
            !output.status.success(),
            "unadopted Archive payload: {entry}: {output:?}"
        );
        assert_eq!(lab.facts(), before);
    }
    private_write(
        &link::link_path(&lab.store, "codex", NATIVE),
        b"{do not read native metadata",
    );
    for gate in ["delegated", "disabled"] {
        if gate == "disabled" {
            let output = lab
                .command(&["config", "commit.auto", "false"], false)
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
        }
        for entry in ["installed", "legacy"] {
            let mut command = lab.settlement_command(entry);
            command
                .env(
                    SETTLEMENT_NATIVE,
                    serde_json::to_string(&lab.binding.native).unwrap(),
                )
                .env(mergetx::ENV, TARGET)
                .env(mergetx::GENERATION_ENV, &lab.binding.role.generation);
            if gate == "delegated" {
                command.env("AGIT_RC_SUPERVISED_HOOK", "1");
            }
            let before = lab.facts();
            let output = lab.hook_payload(command, b"{invalid");
            assert!(output.status.success(), "{gate}/{entry}: {output:?}");
            assert_eq!(lab.facts(), before);
        }
    }
}

/// Missing or replaced store roots cannot become fresh stores under retained Archive authority.
/// Ordinary hook initialization stays available when no Archive ownership is required.
#[test]
fn archive_hooks_refuse_missing_or_replaced_stores_without_recreating_native_ownership() {
    let inventory = |root: &Path| {
        let mut images = BTreeMap::new();
        let mut pending = vec![root.to_owned()];
        while let Some(path) = pending.pop() {
            let metadata = fs::symlink_metadata(&path).unwrap();
            let image = if metadata.is_dir() {
                pending.extend(
                    fs::read_dir(&path)
                        .unwrap()
                        .map(|entry| entry.unwrap().path()),
                );
                None
            } else {
                assert!(metadata.is_file());
                Some(fs::read(&path).unwrap())
            };
            images.insert(path.strip_prefix(root).unwrap().to_owned(), image);
        }
        images
    };
    for replacement in ["missing", "file"] {
        let lab = Lab::new(true);
        lab.settlement_ready();
        lab.advance(true);
        lab.append_native(EXPLORATION);
        let original = inventory(lab.store.root());
        let retained = lab.home.join("retained-store");
        fs::rename(lab.store.root(), &retained).unwrap();
        if replacement == "file" {
            private_write(lab.store.root(), b"SYNTHETIC-STORE-ROOT");
        }
        let before = lab.facts();
        for authority in ["launch", "role"] {
            for entry in ["installed", "legacy"] {
                let mut command = lab.settlement_command(entry);
                command.env(
                    SETTLEMENT_NATIVE,
                    serde_json::to_string(&lab.binding.native).unwrap(),
                );
                if authority == "launch" {
                    command
                        .env(mergetx::ENV, TARGET)
                        .env(mergetx::GENERATION_ENV, &lab.binding.role.generation);
                } else {
                    command.env(
                        SETTLEMENT_ROLE,
                        serde_json::to_string(&lab.binding.role).unwrap(),
                    );
                }
                let output = lab.settle_with(command, entry);
                assert!(
                    !output.status.success(),
                    "{replacement}/{authority}/{entry}: {output:?}"
                );
                assert!(
                    String::from_utf8_lossy(&output.stderr)
                        .contains("the launched Archive store is missing or unreadable"),
                    "{output:?}"
                );
                assert_eq!(lab.facts(), before);
                assert_eq!(inventory(&retained), original);
                if replacement == "missing" {
                    assert!(!lab.store.root().exists());
                } else {
                    assert_eq!(fs::read(lab.store.root()).unwrap(), b"SYNTHETIC-STORE-ROOT");
                }
                assert!(!lab.home.join("strict-result").exists());
            }
        }
        for entry in ["installed", "legacy"] {
            let output = lab.settle(entry);
            assert!(
                output.status.success(),
                "ordinary {replacement}/{entry}: {output:?}"
            );
            assert_eq!(lab.facts(), before);
            assert_eq!(inventory(&retained), original);
        }
        if replacement == "missing" {
            assert!(lab.store.root().is_dir());
        } else {
            assert_eq!(fs::read(lab.store.root()).unwrap(), b"SYNTHETIC-STORE-ROOT");
        }
    }
}

/// Missing credentials cannot erase the obligation carried by a retained Archive identity.
#[test]
fn archive_role_loss_cannot_be_hidden_by_quiet_signed_out_hooks() {
    let lab = Lab::new(true);
    lab.settlement_ready();
    lab.advance(true);
    lab.append_native(EXPLORATION);
    let path = link::link_path(&lab.store, "codex", NATIVE);
    let original = fs::read(&path).unwrap();
    let mut ordinary: serde_json::Value = serde_json::from_slice(&original).unwrap();
    ordinary.as_object_mut().unwrap().remove("merge_archive");
    let ordinary = serde_json::to_vec(&ordinary).unwrap();
    let credentials = lab.agit.join("credentials");
    assert!(credentials.is_dir());
    fs::remove_dir_all(&credentials).unwrap();
    for role in ["present", "removed"] {
        private_write(
            &path,
            if role == "present" {
                &original
            } else {
                &ordinary
            },
        );
        for authority in ["launch", "role"] {
            for entry in ["installed", "legacy"] {
                let mut command = lab.settlement_command(entry);
                command.env(
                    SETTLEMENT_NATIVE,
                    serde_json::to_string(&lab.binding.native).unwrap(),
                );
                command.env(SETTLEMENT_RESULT, lab.home.join("strict-result"));
                if authority == "launch" {
                    command
                        .env(mergetx::ENV, TARGET)
                        .env(mergetx::GENERATION_ENV, &lab.binding.role.generation);
                } else {
                    command.env(
                        SETTLEMENT_ROLE,
                        serde_json::to_string(&lab.binding.role).unwrap(),
                    );
                }
                let before = lab.facts();
                let output = lab.settle_with(command, entry);
                assert!(
                    !output.status.success(),
                    "{role}/{authority}/{entry}: {output:?}"
                );
                assert!(output.stdout.is_empty(), "{output:?}");
                let diagnostic = String::from_utf8_lossy(&output.stderr);
                assert!(
                    diagnostic.contains("archive settlement was not completed"),
                    "{output:?}"
                );
                assert!(
                    diagnostic.contains(if role == "present" {
                        "archive settlement requires sign-in"
                    } else {
                        "the selected native Link lost its archive role"
                    }),
                    "{output:?}"
                );
                assert_eq!(lab.facts(), before);
                assert!(!credentials.exists());
                assert!(!lab.home.join("strict-result").exists());
            }
        }
    }
    for entry in ["installed", "legacy"] {
        let before = lab.facts();
        let output = lab.settle(entry);
        assert!(
            output.status.success(),
            "ordinary signed-out {entry}: {output:?}"
        );
        assert!(
            output.stdout.is_empty() && output.stderr.is_empty(),
            "{output:?}"
        );
        assert_eq!(lab.facts(), before);
    }
    private_write(&path, b"{unreadable native metadata");
    for gate in ["delegated", "disabled"] {
        if gate == "disabled" {
            let output = lab
                .command(&["config", "commit.auto", "false"], false)
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
        }
        for entry in ["installed", "legacy"] {
            let mut command = lab.settlement_command(entry);
            command
                .env(mergetx::ENV, TARGET)
                .env(mergetx::GENERATION_ENV, &lab.binding.role.generation);
            if gate == "delegated" {
                command.env("AGIT_RC_SUPERVISED_HOOK", "1");
            }
            let before = lab.facts();
            let output = lab.settle_with(command, entry);
            assert!(output.status.success(), "{gate}/{entry}: {output:?}");
            assert!(
                output.stdout.is_empty() && output.stderr.is_empty(),
                "{output:?}"
            );
            assert_eq!(lab.facts(), before);
            assert!(!credentials.exists());
        }
    }
}

/// Open exploration belongs only to native storage, even when no complete record can be read.
#[test]
fn settlement_dispatcher_holds_preparing_and_open_without_target_or_frontier_writes() {
    for open in [false, true] {
        let lab = Lab::new(true);
        lab.settlement_ready();
        lab.advance(open);
        lab.append_native("incomplete private runtime bytes");
        let before = lab.facts();
        for entry in ["installed", "legacy", "explicit", "process", "strict"] {
            let output = lab.settle(entry);
            assert!(output.status.success(), "{open}/{entry}: {output:?}");
            assert_eq!(lab.facts(), before, "{entry}");
            if entry == "strict" {
                assert_eq!(
                    fs::read_to_string(lab.home.join("strict-result")).unwrap(),
                    "archive-exploration-withheld\n"
                );
            } else {
                assert!(!lab.home.join("strict-result").exists());
            }
        }
        assert_eq!(lab.journal().consumed, lab.binding.installed);
        assert_eq!(lab.head(), lab.binding.role.origin_head);
    }
}

/// All entry points append the same complete Archive suffix and leave VIEW and logical identity.
#[test]
fn landed_hook_tail_is_idempotent_and_incomplete_records_wait_for_their_newline() {
    let lab = Lab::new(true);
    lab.settlement_ready();
    lab.advance(true);
    lab.append_native(EXPLORATION);
    lab.success("--continue", false);
    let landed = lab.head();
    let view = storage::materialize_at(lab.repo.root(), &landed, meta::VIEW_FILE).unwrap();
    let selected = fs::read(link::link_path(&lab.store, "codex", NATIVE)).unwrap();
    let retired = fs::read(link::link_path(&lab.store, "codex", OLD)).unwrap();
    let source = lab.source.git(&["for-each-ref"]).unwrap();
    let partial = "{\"type\":\"assistant\",\"text\":\"SYNTHETIC-PARTIAL";
    lab.append_native(&format!("{EXPLORATION}{partial}"));
    let output = lab.settle("installed");
    assert!(output.status.success(), "{output:?}");
    let first = lab.head();
    assert_ne!(first, landed);
    let metadata = meta::read_at_ref(&lab.repo, &first).unwrap();
    assert_eq!(metadata.kind, meta::Kind::Archive);
    assert_eq!(metadata.session, SESSION);
    assert_eq!(
        storage::materialize_at(lab.repo.root(), &first, meta::VIEW_FILE).unwrap(),
        view
    );
    assert_eq!(
        lab.journal().consumed.bytes,
        fs::metadata(&lab.native).unwrap().len() - partial.len() as u64
    );
    let before = lab.facts();
    for entry in ["installed", "legacy", "explicit", "strict"] {
        let output = lab.settle(entry);
        assert!(output.status.success(), "{entry}: {output:?}");
        assert_eq!(lab.facts(), before, "{entry}");
    }
    assert!(!lab.home.join("strict-result").exists());
    lab.append_native("\"}\n");
    let output = lab.settle("legacy");
    assert!(output.status.success(), "{output:?}");
    assert_ne!(lab.head(), first);
    for entry in ["explicit", "process", "strict"] {
        let before = lab.head();
        lab.append_native(EXPLORATION);
        let output = lab.settle(entry);
        assert!(output.status.success(), "{entry}: {output:?}");
        assert_ne!(lab.head(), before);
        assert_eq!(
            storage::materialize_at(lab.repo.root(), &lab.head(), meta::VIEW_FILE).unwrap(),
            view
        );
    }
    assert_eq!(
        fs::read_to_string(lab.home.join("strict-result"))
            .unwrap()
            .trim(),
        lab.head()
    );
    assert_eq!(
        fs::read(link::link_path(&lab.store, "codex", NATIVE)).unwrap(),
        selected
    );
    assert_eq!(
        fs::read(link::link_path(&lab.store, "codex", OLD)).unwrap(),
        retired
    );
    assert_eq!(lab.source.git(&["for-each-ref"]).unwrap(), source);
    assert_eq!(
        lab.journal().consumed.bytes,
        fs::metadata(&lab.native).unwrap().len()
    );
}

/// Neither hook wrapper may turn broken generation or native-role authority into saved progress.
#[test]
fn archive_hook_identity_and_generation_failures_survive_both_quiet_wrappers() {
    for change in [
        "owner",
        "role",
        "generation",
        "frontier",
        "json",
        "transaction",
        "missing-journal",
        "missing-link",
    ] {
        for entry in ["installed", "legacy"] {
            let lab = Lab::new(true);
            lab.settlement_ready();
            lab.advance(true);
            let path = link::link_path(&lab.store, "codex", NATIVE);
            let mut value: serde_json::Value =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            match change {
                "owner" => value["owner"] = "foreign".into(),
                "role" => {
                    value["merge_archive"]["logical_session"] =
                        "agit-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into()
                }
                "generation" => {
                    value["merge_archive"]["generation"] = uuid::Uuid::now_v7().to_string().into()
                }
                "frontier" => value["baseline_bytes"] = 0.into(),
                "json" => {
                    private_write(&path, b"{broken");
                }
                "transaction" => {
                    let mut tx: serde_json::Value =
                        serde_json::from_slice(&fs::read(lab.tx_path()).unwrap()).unwrap();
                    tx["generation"] = uuid::Uuid::now_v7().to_string().into();
                    private_write(&lab.tx_path(), &serde_json::to_vec(&tx).unwrap());
                }
                "missing-link" => {
                    fs::remove_file(&path).unwrap();
                }
                "missing-journal" => {
                    fs::remove_file(
                        lab.repo
                            .git_path(merge_archive::DIRECTORY)
                            .unwrap()
                            .join(format!("{}.json", lab.binding.role.generation)),
                    )
                    .unwrap();
                }
                _ => unreachable!(),
            }
            if matches!(change, "owner" | "role" | "generation" | "frontier") {
                private_write(&path, &serde_json::to_vec(&value).unwrap());
            }
            let before = lab.facts();
            let mut command = lab.settlement_command(entry);
            if change == "missing-link" {
                command
                    .env(
                        SETTLEMENT_NATIVE,
                        serde_json::to_string(&lab.binding.native).unwrap(),
                    )
                    .env(mergetx::ENV, TARGET)
                    .env(mergetx::GENERATION_ENV, &lab.binding.role.generation);
            }
            let output = lab.settle_with(command, entry);
            assert!(!output.status.success(), "{entry}/{change}: {output:?}");
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("archive settlement"),
                "{output:?}"
            );
            assert_eq!(lab.facts(), before, "{entry}/{change}");
        }
    }
}

/// Stopped Archive identities retain their disposition and never acquire an ordinary claim.
#[test]
fn stopped_archive_hooks_refuse_without_ordinary_settlement() {
    for phase in ["aborting", "aborted", "detached"] {
        let lab = Lab::new(false);
        lab.settlement_ready();
        lab.advance(true);
        if phase != "aborting" {
            lab.success("--abort", false);
        } else {
            lab.aborting();
        }
        if phase == "detached" {
            let journal = lab.journal();
            assert_eq!(journal.phase, ArchivePhase::Aborted);
            let selected = link::read_archive_link_snapshot(&lab.store, "codex", NATIVE)
                .unwrap()
                .unwrap();
            let mut destination = selected.link.clone();
            destination.branch = Some("detached".into());
            destination.merge_archive = None;
            let mut pending = journal.clone();
            pending.detach = Some(PreparedDetach {
                admission_id: uuid::Uuid::now_v7().to_string(),
                destination_slug: lab.binding.role.slug.clone(),
                destination_branch: "detached".into(),
                expected_destination: None,
                candidate_destination: lab.binding.role.origin_head.clone(),
                candidate_tree: lab
                    .repo
                    .git(&[
                        "rev-parse",
                        &format!("{}^{{tree}}", lab.binding.role.origin_head),
                    ])
                    .unwrap(),
                old_link_json: selected.json,
                new_link_json: destination.to_json().unwrap(),
            });
            let guard = ArchiveJournalGuard::acquire(lab.repo.root(), &lab.binding.role.generation)
                .unwrap();
            guard.replace(&journal, &pending).unwrap();
            let mut detached = pending.clone();
            detached.phase = ArchivePhase::Detached;
            detached.previous_claims.clear();
            detached.detach = None;
            guard.replace(&pending, &detached).unwrap();
            drop(guard);
            // Stale Archive authority must be refused by a valid terminal journal, not by a
            // mismatched recovery carrier or an invalid phase image.
            assert_eq!(lab.journal(), detached);
        }
        lab.append_native(EXPLORATION);
        let before = lab.facts();
        for entry in ["installed", "legacy", "explicit", "process", "strict"] {
            assert!(!lab.settle(entry).status.success(), "{entry}");
            assert_eq!(lab.facts(), before, "{entry}");
        }
    }
}

/// Delegation and auto policy run before native metadata access, while strict settlement refuses
/// an absent or stale immutable-identity/generation handoff without acknowledging a commit.
#[test]
fn archive_settlement_preserves_delegation_auto_and_strict_handoffs() {
    let lab = Lab::new(true);
    lab.settlement_ready();
    lab.advance(true);
    lab.success("--continue", false);
    lab.append_native(EXPLORATION);
    for omit in [
        SETTLEMENT_NATIVE,
        SETTLEMENT_ROLE,
        "AGIT_EXPECTED_AGENT_ID",
        SETTLEMENT_RESULT,
    ] {
        let before = lab.facts();
        let mut command = lab.settlement_command("strict");
        command.env_remove(omit).env("CODEX_SESSION_ID", NATIVE);
        let output = lab.settle_with(command, "strict");
        assert!(!output.status.success(), "{omit}: {output:?}");
        assert_eq!(lab.facts(), before);
        assert!(!lab.home.join("strict-result").exists());
    }
    for drift in ["identity", "generation", "role-removed"] {
        let mut command = lab.settlement_command("strict");
        let path = link::link_path(&lab.store, "codex", NATIVE);
        let original = fs::read(&path).unwrap();
        if drift == "identity" {
            command.env(
                "AGIT_EXPECTED_AGENT_ID",
                "dddddddd-4444-4444-8444-dddddddddddd",
            );
        } else if drift == "generation" {
            let mut role = lab.binding.role.clone();
            role.generation = uuid::Uuid::now_v7().to_string();
            command.env(SETTLEMENT_ROLE, serde_json::to_string(&role).unwrap());
        } else {
            let mut value: serde_json::Value = serde_json::from_slice(&original).unwrap();
            value.as_object_mut().unwrap().remove("merge_archive");
            private_write(&path, &serde_json::to_vec(&value).unwrap());
        }
        let before = lab.facts();
        let output = lab.settle_with(command, "strict");
        assert!(!output.status.success(), "{drift}: {output:?}");
        assert_eq!(lab.facts(), before);
        private_write(&path, &original);
    }
    let disabled = lab
        .command(&["config", "commit.auto", "false"], false)
        .output()
        .unwrap();
    assert!(disabled.status.success());
    let path = link::link_path(&lab.store, "codex", NATIVE);
    private_write(&path, b"{unreadable metadata must not be opened");
    let before = lab.facts();
    for entry in ["installed", "legacy", "strict"] {
        let output = lab.settle(entry);
        assert_eq!(
            output.status.success(),
            entry != "strict",
            "{entry}: {output:?}"
        );
        assert_eq!(lab.facts(), before);
    }
    assert!(
        lab.command(&["config", "commit.auto", "true"], false)
            .output()
            .unwrap()
            .status
            .success()
    );
    let before = lab.facts();
    for entry in ["installed", "legacy", "explicit", "strict"] {
        let mut command = lab.settlement_command(entry);
        command.env("AGIT_RC_SUPERVISED_HOOK", "1");
        let output = lab.settle_with(command, entry);
        assert_eq!(
            output.status.success(),
            matches!(entry, "installed" | "legacy"),
            "{entry}: {output:?}"
        );
        assert_eq!(lab.facts(), before);
    }
}

/// The real RC landing command proves the remote immutable identity and returns an exact role
/// handoff without rewriting the Archive Link. Its strict child consumes that same generation.
#[test]
fn rc_land_to_strict_archive_settlement_retains_native_authority() {
    use std::io::{Read, Write};
    use std::time::{Duration, Instant};
    for phase in [
        "open",
        "landed",
        "wrong-cwd",
        "wrong-branch",
        "repeat-role",
        "repeat-link",
        "repeat-generation",
        "repeat-stopped",
        "repeat-same",
        "repeat-landed",
    ] {
        let lab = Lab::new(true);
        lab.settlement_ready();
        lab.advance(true);
        if phase == "landed" || phase == "repeat-landed" {
            lab.success("--continue", false);
        }
        lab.append_native(EXPLORATION);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let hub = format!("http://{}", listener.local_addr().unwrap());
        let response = serde_json::json!({"agent_id":AGENT_ID,"owner":"alice","name":"target","clone_url":format!("{hub}/alice/target.git")}).to_string();
        let requests = if phase.starts_with("repeat-") { 2 } else { 1 };
        let worker = std::thread::spawn(move || {
            for _ in 0..requests {
                let deadline = Instant::now() + Duration::from_secs(20);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error)
                            if error.kind() == std::io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            std::thread::sleep(Duration::from_millis(10))
                        }
                        error => panic!("RC fixture request unavailable: {error:?}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") && request.len() < 65536 {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                assert!(request.starts_with(b"GET /api/agents/alice/target "));
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).unwrap();
            }
        });
        lab.repo
            .git(&[
                "config",
                "agit.remoteIdentity",
                &serde_json::json!({"hub":hub,"agent_id":AGENT_ID}).to_string(),
            ])
            .unwrap();
        agit::infra::credentials::save_at(
            &lab.agit.join("credentials").join(format!(
                "{}.json",
                agit::infra::config::hub_host_key(&hub).unwrap()
            )),
            &agit::infra::credentials::HubCredential {
                username: "alice".into(),
                email: None,
                hub: Some(hub.clone()),
                access_token: "synthetic".into(),
                refresh_token: "synthetic".into(),
                access_expires_at: "2099-01-01T00:00:00Z".into(),
                refresh_expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .unwrap();
        let other = lab.home.join("other");
        fs::create_dir(&other).unwrap();
        let cwd = if phase == "wrong-cwd" {
            &other
        } else {
            &lab.home
        };
        let branch = if phase == "wrong-branch" {
            "other"
        } else {
            "work"
        };
        let argv = agit::commands::rc::land_argv(
            "alice/target",
            AGENT_ID,
            branch,
            "codex",
            NATIVE,
            cwd.to_str().unwrap(),
        );
        let args = argv.iter().map(String::as_str).collect::<Vec<_>>();
        let before = lab.facts();
        let before_head = lab.head();
        let output = lab
            .command(&args, false)
            .env("AGIT_HUB_URL", &hub)
            .env("AGIT_EXPECTED_AGENT_ID", AGENT_ID)
            .output()
            .unwrap();
        assert_eq!(
            lab.facts(),
            before,
            "RC landing rewrote archive state: {phase}"
        );
        if phase.starts_with("wrong-") {
            worker.join().unwrap();
            assert!(!output.status.success(), "{phase}: {output:?}");
            continue;
        }
        assert!(output.status.success(), "{phase}: {output:?}");
        let text = String::from_utf8(output.stdout).unwrap();
        let handoff: serde_json::Value = serde_json::from_str(
            text.trim()
                .strip_prefix("AGIT_ARCHIVE_SETTLEMENT ")
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            handoff["native"],
            serde_json::to_value(&lab.binding.native).unwrap()
        );
        assert_eq!(
            handoff["role"],
            serde_json::to_value(&lab.binding.role).unwrap()
        );
        if phase.starts_with("repeat-") {
            let path = link::link_path(&lab.store, "codex", NATIVE);
            match phase {
                "repeat-role" | "repeat-generation" => {
                    let mut value: serde_json::Value =
                        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                    if phase == "repeat-role" {
                        value.as_object_mut().unwrap().remove("merge_archive");
                    } else {
                        value["merge_archive"]["generation"] =
                            uuid::Uuid::now_v7().to_string().into();
                    }
                    private_write(&path, &serde_json::to_vec(&value).unwrap());
                }
                "repeat-link" => fs::remove_file(&path).unwrap(),
                "repeat-stopped" => {
                    lab.success("--abort", false);
                }
                "repeat-same" | "repeat-landed" => {}
                _ => unreachable!(),
            }
            let before = lab.facts();
            let output = lab
                .command(&args, false)
                .env("AGIT_HUB_URL", &hub)
                .env("AGIT_EXPECTED_AGENT_ID", AGENT_ID)
                .env(SETTLEMENT_NATIVE, handoff["native"].to_string())
                .env(SETTLEMENT_ROLE, handoff["role"].to_string())
                .output()
                .unwrap();
            worker.join().unwrap();
            assert_eq!(
                output.status.success(),
                matches!(phase, "repeat-same" | "repeat-landed"),
                "{phase}: {output:?}"
            );
            assert_eq!(
                lab.facts(),
                before,
                "re-landing changed a retained Archive: {phase}"
            );
            assert!(!lab.home.join("strict-result").exists());
            if output.status.success() {
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(
                        String::from_utf8(output.stdout)
                            .unwrap()
                            .trim()
                            .strip_prefix("AGIT_ARCHIVE_SETTLEMENT ")
                            .unwrap()
                    )
                    .unwrap(),
                    handoff
                );
            } else {
                assert!(output.stdout.is_empty(), "{phase}: {output:?}");
            }
            continue;
        }
        worker.join().unwrap();
        let mut command = lab.settlement_command("strict");
        command
            .env("AGIT_HUB_URL", &hub)
            .env(SETTLEMENT_NATIVE, handoff["native"].to_string())
            .env(SETTLEMENT_ROLE, handoff["role"].to_string());
        let output = lab.settle_with(command, "strict");
        assert!(output.status.success(), "{phase}: {output:?}");
        if phase == "open" {
            assert_eq!(lab.facts(), before);
            assert_eq!(
                fs::read_to_string(lab.home.join("strict-result")).unwrap(),
                "archive-exploration-withheld\n"
            );
        } else {
            assert_ne!(lab.head(), before_head);
            assert_eq!(
                fs::read_to_string(lab.home.join("strict-result"))
                    .unwrap()
                    .trim(),
                lab.head()
            );
            assert_eq!(
                meta::read_at_ref(&lab.repo, &lab.head()).unwrap().kind,
                meta::Kind::Archive
            );
        }
    }
}

const COMPLETION_CASE: &str = "AGIT_TEST_ARCHIVE_COMPLETION";
const COMPLETION_NATIVE: &str = "AGIT_TEST_ARCHIVE_NATIVE_PATH";
const FINAL_RECORD: &str = "{\"type\":\"assistant\",\"text\":\"SYNTHETIC-FINAL-CHILD-RECORD\"}\n";
const HOOK_RECORD: &str = "{\"type\":\"assistant\",\"text\":\"SYNTHETIC-BEFORE-FINAL-HOOK\"}\n";

impl Lab {
    fn completion_command(&self, case: &str) -> Command {
        let environment = self.command(&[], true);
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "archive_completion_driver", "--nocapture"])
            .env_clear()
            .current_dir(&self.home)
            .stdin(Stdio::null());
        for (key, value) in environment.get_envs() {
            if let Some(value) = value {
                command.env(key, value);
            }
        }
        command
            .env(COMPLETION_CASE, case)
            .env(COMPLETION_NATIVE, &self.native)
            .env(
                "AGIT_TEST_ARCHIVE_BINDING",
                serde_json::to_string(&self.binding).unwrap(),
            )
            .env("AGIT_TEST_ARCHIVE_REPO", self.repo.root())
            .env("AGIT_TEST_ARCHIVE_SOURCE", self.source.root())
            .env("AGIT_TEST_ARCHIVE_STORE", self.store.root())
            .env(
                "AGIT_TEST_ARCHIVE_RESULT",
                self.home.join("completion-result.json"),
            );
        command
    }
}

fn completion_hook() -> std::process::Child {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_agit"))
        .args(["hooks", "settle", "--runtime", "codex"])
        .env("AGIT_SESSION", "alice/source@source")
        .env("CODEX_SESSION_ID", OLD)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let payload = serde_json::json!({
        "session_id": NATIVE,
        "transcript_path": std::env::var(COMPLETION_NATIVE).unwrap(),
        "cwd": std::env::current_dir().unwrap(),
    });
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    child
}

/// The native child runs real lifecycle and hook commands before writing its last native record.
#[test]
fn archive_completion_child() {
    use std::io::Write;
    if std::env::var_os("AGIT_TEST_ARCHIVE_CHILD").is_none() {
        return;
    }
    let case = std::env::var(COMPLETION_CASE).unwrap();
    let path = PathBuf::from(std::env::var_os(COMPLETION_NATIVE).unwrap());
    let append = |bytes: &[u8]| {
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(bytes)
            .unwrap();
    };
    if case != "open" {
        let action = if case == "aborted" {
            "--abort"
        } else {
            "--continue"
        };
        let output = Command::new(env!("CARGO_BIN_EXE_agit"))
            .args(["merge", "--into", TARGET, action])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(output.status.success(), "{case}: {output:?}");
    }
    if case == "aborted" {
        // A stopped finalizer must not inspect this retired carrier or invent a new merge.
        fs::remove_file(&path).unwrap();
    } else if case == "open" {
        append(EXPLORATION.as_bytes());
    } else {
        append(HOOK_RECORD.as_bytes());
        assert!(completion_hook().wait().unwrap().success());
        append(if case == "partial" {
            FINAL_RECORD.trim_end().as_bytes()
        } else {
            FINAL_RECORD.as_bytes()
        });
    }
    if case == "source-gone" {
        fs::rename(
            PathBuf::from(std::env::var_os("AGIT_TEST_ARCHIVE_SOURCE").unwrap()),
            std::env::current_dir().unwrap().join("unavailable-source"),
        )
        .unwrap();
    }
    if case == "role-loss" {
        let store = Store::at(PathBuf::from(
            std::env::var_os("AGIT_TEST_ARCHIVE_STORE").unwrap(),
        ));
        let link_path = link::link_path(&store, "codex", NATIVE);
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&link_path).unwrap()).unwrap();
        value.as_object_mut().unwrap().remove("merge_archive");
        private_write(&link_path, &serde_json::to_vec(&value).unwrap());
    }
    if case == "nonzero" {
        std::process::exit(17);
    }
    #[cfg(unix)]
    if case == "signal" {
        unsafe {
            libc::raise(libc::SIGTERM);
        }
        panic!("termination signal did not end the fixture child");
    }
}

/// Re-execution isolates parent finalization from the test runner's environment and credentials.
#[test]
fn archive_completion_driver() {
    let Ok(case) = std::env::var(COMPLETION_CASE) else {
        return;
    };
    if std::env::var_os("AGIT_TEST_ARCHIVE_CHILD").is_some() {
        return;
    }
    let repo = Repo::open(PathBuf::from(
        std::env::var_os("AGIT_TEST_ARCHIVE_REPO").unwrap(),
    ))
    .unwrap();
    let store = Store::at(PathBuf::from(
        std::env::var_os("AGIT_TEST_ARCHIVE_STORE").unwrap(),
    ));
    let binding: ExplorationBinding =
        serde_json::from_str(&std::env::var("AGIT_TEST_ARCHIVE_BINDING").unwrap()).unwrap();
    let native = PathBuf::from(std::env::var_os(COMPLETION_NATIVE).unwrap());
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "archive_completion_child", "--nocapture"])
        .env("AGIT_TEST_ARCHIVE_CHILD", "1")
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let status = child.wait().unwrap();
    assert_eq!(
        status.success(),
        !matches!(case.as_str(), "nonzero" | "signal")
    );
    if case == "nonzero" {
        assert_eq!(status.code(), Some(17));
    }
    #[cfg(unix)]
    if case == "signal" {
        assert_eq!(status.code(), None);
    }
    let before_head = repo.git(&["rev-parse", "refs/heads/work"]).unwrap();
    let before_native = fs::read(&native).ok();
    let before_link = fs::read(link::link_path(&store, "codex", NATIVE)).unwrap();
    let mut racing_hook = (case == "race").then(completion_hook);
    let code = merge::archive::completion::finish(&repo, &store, &binding, &mut child).unwrap();
    if let Some(hook) = racing_hook.as_mut() {
        assert!(hook.wait().unwrap().success());
    }
    assert_eq!(fs::read(&native).ok(), before_native);
    assert_eq!(
        fs::read(link::link_path(&store, "codex", NATIVE)).unwrap(),
        before_link
    );
    let after_head = repo.git(&["rev-parse", "refs/heads/work"]).unwrap();
    private_write(
        &PathBuf::from(std::env::var_os("AGIT_TEST_ARCHIVE_RESULT").unwrap()),
        &serde_json::to_vec(&serde_json::json!({"before_head":before_head,"after_head":after_head,"code":code.as_i32()})).unwrap(),
    );
    std::process::exit(code.as_i32());
}

/// A landed child captures post-hook bytes even on failure; incomplete or stopped work stays failed.
#[test]
fn child_completion_retains_exact_archive_tail_and_exit_status() {
    let mut cases = vec![
        "success",
        "source-gone",
        "nonzero",
        "race",
        "partial",
        "open",
        "aborted",
        "role-loss",
    ];
    if cfg!(unix) {
        cases.push("signal");
    }
    for case in cases {
        let lab = Lab::new(true);
        lab.settlement_ready();
        lab.advance(true);
        let output = lab.completion_command(case).output().unwrap();
        let expected = if matches!(case, "success" | "source-gone" | "race") {
            0
        } else {
            4
        };
        assert_eq!(output.status.code(), Some(expected), "{case}: {output:?}");
        let proof: serde_json::Value =
            serde_json::from_slice(&fs::read(lab.home.join("completion-result.json")).unwrap())
                .unwrap();
        assert_eq!(proof["code"], expected);
        if matches!(case, "open" | "aborted" | "role-loss" | "partial") {
            assert_eq!(proof["before_head"], proof["after_head"], "{case}");
        }
        let journal = lab.journal();
        if case == "open" {
            assert_eq!(journal.phase, ArchivePhase::Open);
            assert_eq!(lab.head().trim(), lab.binding.role.origin_head);
            assert!(lab.tx_path().exists());
        } else if case == "aborted" {
            assert_eq!(journal.phase, ArchivePhase::Aborted);
            assert_eq!(lab.head().trim(), lab.binding.role.origin_head);
            assert!(!lab.tx_path().exists());
        } else {
            assert!(matches!(journal.phase, ArchivePhase::Landed { .. }));
            let log = storage::materialize_at(lab.repo.root(), lab.head().trim(), meta::LOG_FILE)
                .unwrap();
            assert_eq!(
                log.matches("SYNTHETIC-BEFORE-FINAL-HOOK").count(),
                1,
                "{case}"
            );
            let complete = !matches!(case, "partial" | "role-loss");
            assert_eq!(
                log.matches("SYNTHETIC-FINAL-CHILD-RECORD").count(),
                usize::from(complete),
                "{case}"
            );
            if complete {
                assert_eq!(
                    journal.consumed.bytes,
                    fs::metadata(&lab.native).unwrap().len()
                );
            }
            assert!(!log.contains("SYNTHETIC-PRIVATE-EXPLORATION"));
            assert!(!lab.tx_path().exists());
        }
        if matches!(case, "nonzero" | "signal") {
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .contains("archive merge child did not exit successfully"),
                "{output:?}"
            );
        }
    }
}
