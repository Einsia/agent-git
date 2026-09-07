use agit::commands::merge::marker_envelope;
use agit::domain::{mergetx, meta, repo::Repo, storage, transcript};
use std::process::{Command, Output};

const CLAIM: &str = "agit-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

struct MergeFixture {
    _temporary: tempfile::TempDir,
    home: std::path::PathBuf,
    repo: Repo,
    target_head: String,
}

impl MergeFixture {
    fn new(source_log: &str, picked: &str) -> Self {
        Self::with_source(source_log, picked, false)
    }

    fn with_source(source_log: &str, picked: &str, cross_repo: bool) -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home");
        let repo = Repo::init(&home.join("repos/local/merge-test")).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        repo.git(&["checkout", "-b", "target"]).unwrap();
        let mut metadata = meta::Meta::new(CLAIM.into(), "codex".into(), "/work".into());
        metadata.turn = Some(1);
        meta::write(repo.root(), &metadata).unwrap();
        let target_log =
            transcript::wrap_lines("{\"type\":\"user\",\"text\":\"target\"}\n", "codex", CLAIM);
        storage::write_snapshot(repo.root(), &target_log, &target_log).unwrap();
        std::fs::write(repo.root().join("AGENTS.md"), "Shared instructions\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("target fixture").unwrap();
        let target_head = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
        let (source_repo, source_slug) = if cross_repo {
            let source = Repo::init(&home.join("repos/local/merge-source")).unwrap();
            source.git(&["config", "commit.gpgsign", "false"]).unwrap();
            source.git(&["checkout", "-b", "source"]).unwrap();
            (source, "local/merge-source")
        } else {
            repo.git(&["checkout", "--orphan", "source"]).unwrap();
            (Repo::at(repo.root()), "local/merge-test")
        };
        storage::write_snapshot(source_repo.root(), source_log, source_log).unwrap();
        meta::write(source_repo.root(), &metadata).unwrap();
        source_repo.add_all().unwrap();
        source_repo.commit("source fixture").unwrap();
        let source_head = source_repo
            .git(&["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();
        if !cross_repo {
            repo.git(&["checkout", "target"]).unwrap();
        }
        std::fs::write(repo.root().join("AGENTS.md"), "Reconciled instructions\n").unwrap();
        mergetx::lock(
            repo.root(),
            &mergetx::Tx {
                target: "target".into(),
                source: format!("{source_slug}@source"),
                source_repo: Some(source_slug.into()),
                source_branch: Some("source".into()),
                base: target_head.clone(),
                target_head: target_head.clone(),
                source_head,
                picked: vec![picked.into()],
                summary: Some("Keep the selected source context".into()),
            },
        )
        .unwrap();
        Self {
            _temporary: temporary,
            home,
            repo,
            target_head,
        }
    }

    fn command(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_agit"))
            .args(["merge", "--into", "local/merge-test@target"])
            .args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("AGIT_HOME", &self.home)
            .env("CI", "1")
            .current_dir(self.repo.root())
            .output()
            .unwrap()
    }

    fn replace_target_snapshot(&mut self, log: &str, view: &str) {
        storage::write_snapshot(self.repo.root(), log, view).unwrap();
        self.update_target_head();
    }

    fn update_target_head(&mut self) {
        std::fs::write(self.repo.root().join("AGENTS.md"), "Shared instructions\n").unwrap();
        self.repo.add_all().unwrap();
        self.repo.commit("target integrity fixture").unwrap();
        self.target_head = self.repo.git(&["rev-parse", "HEAD"]).unwrap().trim().into();
        let mut tx = mergetx::read(self.repo.root()).unwrap().unwrap();
        tx.target_head = self.target_head.clone();
        tx.base = self.target_head.clone();
        mergetx::unlock(self.repo.root()).unwrap();
        mergetx::lock(self.repo.root(), &tx).unwrap();
        std::fs::write(
            self.repo.root().join("AGENTS.md"),
            "Reconciled instructions\n",
        )
        .unwrap();
    }

    fn assert_refused_without_mutation(&self, expected: &str) {
        let refs = self
            .repo
            .git(&["for-each-ref", "--format=%(refname) %(objectname)"])
            .unwrap();
        let objects = self.repo.git(&["count-objects", "-v"]).unwrap();
        let status = self.repo.git(&["status", "--porcelain=v1"]).unwrap();
        let tx = serde_json::to_vec(&mergetx::read(self.repo.root()).unwrap()).unwrap();
        let output = self.command(&["--continue"]);
        assert!(!output.status.success(), "invalid VIEW must not land");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "{stderr}");
        assert_eq!(
            self.repo
                .git(&["for-each-ref", "--format=%(refname) %(objectname)"])
                .unwrap(),
            refs
        );
        assert_eq!(self.repo.git(&["count-objects", "-v"]).unwrap(), objects);
        assert_eq!(
            self.repo.git(&["status", "--porcelain=v1"]).unwrap(),
            status
        );
        assert_eq!(
            serde_json::to_vec(&mergetx::read(self.repo.root()).unwrap()).unwrap(),
            tx
        );
        assert_eq!(
            std::fs::read_to_string(self.repo.root().join("AGENTS.md")).unwrap(),
            "Reconciled instructions\n"
        );
    }
}

fn marker(kind: &str, source: &str) -> String {
    marker_envelope(kind, "codex", CLAIM, source)
}

#[test]
fn a_partial_marker_selection_is_repairable_without_mutation() {
    let start = marker("__merge_start__", "nested");
    let end = marker("__merge_end__", "nested");
    let nested_start = marker("__cherry_pick_start__", "inner");
    let nested_end = marker("__cherry_pick_end__", "inner");
    let fixture = MergeFixture::new(
        &format!("{start}{nested_start}{nested_end}{end}"),
        "source#1.1",
    );
    fixture.assert_refused_without_mutation("misplaced or mismatched");
    assert!(fixture.command(&["drop", "source#1.1"]).status.success());
    assert!(fixture.command(&["pick", "source#1"]).status.success());
    let output = fixture.command(&["--continue"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_ne!(
        fixture
            .repo
            .git(&["rev-parse", "refs/heads/target"])
            .unwrap()
            .trim(),
        fixture.target_head
    );
    assert!(mergetx::read(fixture.repo.root()).unwrap().is_none());
    let view = storage::materialize_at(fixture.repo.root(), "target", meta::VIEW_FILE).unwrap();
    assert!(view.contains("nested"));
}

#[test]
fn misplaced_or_mismatched_marker_pairs_cannot_land() {
    let start = marker("__merge_start__", "nested");
    let end = marker("__merge_end__", "nested");
    let wrong_kind = marker("__cherry_pick_end__", "nested");
    let wrong_source = marker("__merge_end__", "another");
    let wrong_runtime = marker_envelope("__merge_end__", "claude", CLAIM, "nested");
    let wrong_claim = marker_envelope(
        "__merge_end__",
        "codex",
        "agit-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "nested",
    );
    let inner_start = marker("__cherry_pick_start__", "inner");
    let inner_end = marker("__cherry_pick_end__", "inner");
    for log in [
        end.clone(),
        format!("{end}{start}"),
        format!("{start}{wrong_kind}"),
        format!("{start}{wrong_source}"),
        format!("{start}{wrong_runtime}"),
        format!("{start}{wrong_claim}"),
        format!("{start}{inner_start}{end}{inner_end}"),
    ] {
        let fixture = MergeFixture::new(&log, "source#1");
        fixture.assert_refused_without_mutation("misplaced or mismatched");
    }
}

#[test]
fn an_unbalanced_target_view_cannot_be_carried_into_a_merge() {
    let source =
        transcript::wrap_lines("{\"type\":\"user\",\"text\":\"source\"}\n", "codex", CLAIM);
    let mut fixture = MergeFixture::new(&source, "source#1");
    let start = marker("__merge_start__", "inherited");
    let end = marker("__merge_end__", "inherited");
    fixture.replace_target_snapshot(&format!("{start}{end}"), &start);
    fixture.assert_refused_without_mutation("misplaced or mismatched");
}

#[test]
fn a_view_event_outside_log_is_rejected_without_mutation() {
    let source =
        transcript::wrap_lines("{\"type\":\"user\",\"text\":\"source\"}\n", "codex", CLAIM);
    let mut fixture = MergeFixture::new(&source, "source#1");
    std::fs::write(fixture.repo.root().join(meta::LOG_FILE), "").unwrap();
    fixture.update_target_head();
    fixture.assert_refused_without_mutation("not reachable from LOG");
}

#[test]
fn cross_repository_validation_precedes_object_import() {
    let start = marker("__merge_start__", "cross-repo");
    let end = marker("__merge_end__", "cross-repo");
    let fixture = MergeFixture::with_source(&format!("{start}{end}"), "source#1.1", true);
    let source_head = mergetx::read(fixture.repo.root())
        .unwrap()
        .unwrap()
        .source_head;
    assert!(
        fixture
            .repo
            .git_opt(&["cat-file", "-e", &source_head])
            .is_none()
    );
    fixture.assert_refused_without_mutation("misplaced or mismatched");
    assert!(
        fixture
            .repo
            .git_opt(&["cat-file", "-e", &source_head])
            .is_none()
    );
    assert!(fixture.command(&["drop", "source#1.1"]).status.success());
    assert!(fixture.command(&["pick", "source#1"]).status.success());
    let output = fixture.command(&["--continue"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let parents = fixture
        .repo
        .git(&["rev-list", "--parents", "-n", "1", "target"])
        .unwrap();
    assert!(
        parents
            .split_whitespace()
            .any(|parent| parent == source_head)
    );
    let view = storage::materialize_at(fixture.repo.root(), "target", meta::VIEW_FILE).unwrap();
    assert!(view.contains("cross-repo"));
    assert!(mergetx::read(fixture.repo.root()).unwrap().is_none());
}
