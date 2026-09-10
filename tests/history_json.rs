use agit::domain::meta::{self, Kind, Meta};
use agit::domain::repo::Repo;
use serde_json::{Value, json};
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

const SLUG: &str = "organization/history";
const BRANCH: &str = "topic/claude";
const SESSION: &str = "agit-1234567890abcdef1234567890abcdef12345678";
const CODE_ANCHOR: &str = "https://example.test/team/source.git@1234567890abcdef";
const OLD: u64 = 946_684_800;

struct Lab {
    _temp: tempfile::TempDir,
    home: PathBuf,
    store: PathBuf,
    cwd: PathBuf,
    repo: Repo,
}

impl Lab {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let store = temp.path().join("agit");
        let cwd = temp.path().join("work");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&cwd).unwrap();
        let repo = Repo::init(&store.join("repos").join(SLUG)).unwrap();
        repo.git(&["config", "user.name", "History fixture"])
            .unwrap();
        repo.git(&["config", "user.email", "history@example.test"])
            .unwrap();
        repo.git(&["config", "core.hooksPath", "/dev/null"])
            .unwrap();
        Self {
            _temp: temp,
            home,
            store,
            cwd,
            repo,
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command
            .args(args)
            .current_dir(&self.cwd)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("AGIT_HOME", &self.store)
            .env("AGIT_SESSION", format!("{SLUG}@{BRANCH}"))
            .env("AGIT_HUB_URL", "http://127.0.0.1:1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1");
        command
    }

    fn document(output: Output) -> Value {
        assert!(
            output.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let document: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(document["ok"], true);
        assert_eq!(document["result"]["format"], "json");
        let value = document["result"]["value"].clone();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["repo"], SLUG);
        value
    }

    fn run(&self, args: &[&str]) -> Value {
        Self::document(self.command(args).output().unwrap())
    }

    fn commit(&self, metadata: &Meta, subject: &str, at: u64) -> String {
        meta::write(self.repo.root(), metadata).unwrap();
        if !metadata.session.is_empty() {
            fs::write(self.repo.root().join(meta::LOG_FILE), "").unwrap();
        }
        fs::write(self.repo.root().join("history-marker"), subject).unwrap();
        self.repo.add_all().unwrap();
        let output = Command::new("git")
            .arg("-C")
            .arg(self.repo.root())
            .args(["-c", "commit.gpgsign=false", "commit", "-q", "-m", subject])
            .env("GIT_AUTHOR_DATE", format!("{at} +0000"))
            .env("GIT_COMMITTER_DATE", format!("{at} +0000"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        self.oid("HEAD")
    }

    fn oid(&self, reference: &str) -> String {
        self.repo
            .git(&["rev-parse", reference])
            .unwrap()
            .trim()
            .to_owned()
    }

    fn trace(&self, args: &[&str], name: &str) -> Vec<Value> {
        let path = self._temp.path().join(name);
        Self::document(
            self.command(args)
                .env("GIT_TRACE2_EVENT", &path)
                .output()
                .unwrap(),
        );
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|event| event["event"] == "start")
            .collect()
    }
}

struct History {
    lab: Lab,
    root: String,
    first: String,
    second: String,
    third: String,
    head: String,
    codex: String,
    opening: String,
    recent: u64,
}

impl History {
    fn new() -> Self {
        let lab = Lab::new();
        let root = lab.commit(&Meta::new_file_line(), "Shared instructions", OLD);
        lab.repo.git(&["checkout", "-q", "-b", BRANCH]).unwrap();
        let opening = format!("Opening needle: {}", "full prompt detail ".repeat(12));
        let opening = opening.trim_end().to_owned();
        let mut metadata = Meta::new(SESSION.into(), "claude-code".into(), "/source".into());
        metadata.turn = Some(1);
        metadata.code = Some(CODE_ANCHOR.into());
        metadata.milestone = Some("Stable history contract".into());
        let first = lab.commit(&metadata, &opening, OLD + 1);
        lab.repo.git(&["tag", "checkpoint-light", &first]).unwrap();
        lab.repo
            .git(&[
                "tag",
                "-a",
                "checkpoint-annotated",
                "-m",
                "Checkpoint",
                &first,
            ])
            .unwrap();

        metadata.kind = Kind::File;
        metadata.code = None;
        metadata.milestone = None;
        fs::create_dir_all(lab.repo.root().join("notes")).unwrap();
        fs::write(lab.repo.root().join("notes/agent.md"), "Shared invariant\n").unwrap();
        lab.commit(&metadata, "Document shared invariant", OLD + 2);

        metadata.kind = Kind::Turn;
        metadata.turn = Some(2);
        let second = lab.commit(&metadata, "Second needle turn", OLD + 3);
        metadata.kind = Kind::View;
        lab.commit(&metadata, "Remove an event from the view", OLD + 4);
        metadata.kind = Kind::Turn;
        metadata.turn = Some(3);
        let recent = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 60;
        let third = lab.commit(&metadata, "Third needle turn", recent);

        lab.repo
            .git(&["checkout", "-q", "-b", "topic/codex", &root])
            .unwrap();
        let mut codex_meta = Meta::new(SESSION.into(), "codex".into(), "/source".into());
        codex_meta.turn = Some(1);
        let codex = lab.commit(&codex_meta, "Codex opening prompt", OLD + 5);
        lab.repo.git(&["checkout", "-q", BRANCH]).unwrap();
        lab.repo
            .git(&["merge", "-s", "ours", "--no-commit", "topic/codex"])
            .unwrap();
        metadata.kind = Kind::Merge;
        metadata.code = Some(CODE_ANCHOR.into());
        fs::write(lab.repo.root().join(".agit-sealed"), "Review complete\n").unwrap();
        let head = lab.commit(&metadata, "Merge the codex session", recent + 1);
        Self {
            lab,
            root,
            first,
            second,
            third,
            head,
            codex,
            opening,
            recent,
        }
    }
}

fn row_by_ref<'a>(rows: &'a Value, reference: &str) -> &'a Value {
    rows.as_array()
        .unwrap()
        .iter()
        .find(|row| row["ref"] == reference)
        .unwrap_or_else(|| panic!("missing {reference}: {rows}"))
}

fn assert_full_oid(value: &Value) {
    let oid = value.as_str().unwrap();
    assert_eq!(oid.len(), 40, "{oid}");
    assert!(oid.bytes().all(|byte| byte.is_ascii_hexdigit()), "{oid}");
}

#[test]
fn turn_history_preserves_ids_subjects_tags_and_non_turn_absence() {
    let history = History::new();
    let value = history.lab.run(&["--json", "log"]);
    assert_eq!(value["view"], "turns");
    assert_eq!(value["head_oid"], history.head);
    let turns = value["turns"].as_array().unwrap();
    assert_eq!(turns.len(), 7);
    for row in turns {
        assert_full_oid(&row["oid"]);
        assert!(row["committed_at"].is_u64());
        assert!(row["tags"].is_array());
        if row["kind"] != "turn" {
            assert!(row["turn"].is_null(), "{row}");
        }
    }
    let first = turns
        .iter()
        .find(|row| row["oid"] == history.first)
        .unwrap();
    assert_eq!(first["subject"], history.opening);
    assert_eq!(first["turn"], 1);
    assert_eq!(first["kind"], "turn");
    assert_eq!(first["code_anchor"], CODE_ANCHOR);
    assert_eq!(first["milestone"], "Stable history contract");
    assert_eq!(first["committed_at"], OLD + 1);
    let mut tags: Vec<_> = first["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tag| tag.as_str().unwrap())
        .collect();
    tags.sort_unstable();
    assert_eq!(tags, ["checkpoint-annotated", "checkpoint-light"]);
    let second = turns
        .iter()
        .find(|row| row["oid"] == history.second)
        .unwrap();
    assert!(second["code_anchor"].is_null());
    assert!(second["milestone"].is_null());
    assert_eq!(second["tags"], json!([]));
    assert_eq!(turns.last().unwrap()["kind"], "merge");
    assert_eq!(turns.last().unwrap()["oid"], history.head);
}

#[test]
fn history_filters_preserve_original_turn_coordinates_and_return_empty_arrays() {
    let history = History::new();
    let value = history.lab.run(&[
        "log", "--json", "--kind", "turn", "--grep", "needle", "-n", "1",
    ]);
    assert_eq!(value["turns"].as_array().unwrap().len(), 1);
    assert_eq!(value["turns"][0]["oid"], history.third);
    assert_eq!(value["turns"][0]["turn"], 3);
    let value = history
        .lab
        .run(&["log", "--json", "--since", "24h", "--kind", "turn"]);
    assert_eq!(value["turns"].as_array().unwrap().len(), 1);
    assert_eq!(value["turns"][0]["turn"], 3);
    assert_eq!(value["turns"][0]["committed_at"], history.recent);
    let value = history.lab.run(&["log", "--json", "--", "notes/agent.md"]);
    assert_eq!(value["turns"].as_array().unwrap().len(), 1);
    assert_eq!(value["turns"][0]["kind"], "file");
    assert!(value["turns"][0]["turn"].is_null());
    for arguments in [
        vec!["log", "--json", "--grep", "absent subject"],
        vec!["log", "--json", "-n", "0"],
        vec!["log", "--json", "--", "missing-file.md"],
    ] {
        let value = history.lab.run(&arguments);
        assert_eq!(value["view"], "turns");
        assert_eq!(value["head_oid"], history.head);
        assert_eq!(value["turns"], json!([]));
    }
}

#[test]
fn branch_views_preserve_runtime_namespaces_and_distinguish_tracking_states() {
    let history = History::new();
    let repo = &history.lab.repo;
    repo.git(&["remote", "add", "origin", "/nonexistent/history-remote"])
        .unwrap();
    let tree = history.lab.oid("HEAD^{tree}");
    let remote_tip = repo
        .git(&[
            "commit-tree",
            &tree,
            "-p",
            &history.second,
            "-m",
            "Remote-only work",
        ])
        .unwrap();
    let remote_tip = remote_tip.trim();
    repo.git(&["update-ref", "refs/remotes/origin/topic/claude", remote_tip])
        .unwrap();
    repo.git(&["config", "branch.topic/claude.remote", "origin"])
        .unwrap();
    repo.git(&[
        "config",
        "branch.topic/claude.merge",
        "refs/heads/topic/claude",
    ])
    .unwrap();
    repo.git(&[
        "update-ref",
        "refs/remotes/origin/topic/codex",
        &history.codex,
    ])
    .unwrap();
    repo.git(&["config", "branch.topic/codex.remote", "origin"])
        .unwrap();
    repo.git(&[
        "config",
        "branch.topic/codex.merge",
        "refs/heads/topic/codex",
    ])
    .unwrap();
    repo.git(&["branch", "missing-upstream", &history.root])
        .unwrap();
    repo.git(&["config", "branch.missing-upstream.remote", "origin"])
        .unwrap();
    repo.git(&[
        "config",
        "branch.missing-upstream.merge",
        "refs/heads/not-fetched",
    ])
    .unwrap();

    let value = history
        .lab
        .run(&["branch", "--json", "--repo", SLUG, "--all"]);
    let branches = &value["branches"];
    assert_eq!(branches.as_array().unwrap().len(), 6);
    for row in branches.as_array().unwrap() {
        assert_full_oid(&row["oid"]);
        assert!(row["committed_at"].is_u64());
        assert!(row["local"].is_boolean());
        assert!(row["current"].is_boolean());
    }
    let claude = row_by_ref(branches, "refs/heads/topic/claude");
    assert_eq!(claude["name"], BRANCH);
    assert_eq!(claude["local"], true);
    assert_eq!(claude["current"], true);
    assert_eq!(claude["oid"], history.head);
    assert_eq!(claude["line"], "session");
    assert_eq!(claude["session_id"], SESSION);
    assert_eq!(claude["runtime"], "claude-code");
    assert_eq!(claude["turns"], 3);
    assert_eq!(claude["opening_subject"], history.opening);
    assert_eq!(claude["sealed"], true);
    assert_eq!(claude["code_anchor"], CODE_ANCHOR);
    assert_eq!(
        claude["sync"]["upstream_ref"],
        "refs/remotes/origin/topic/claude"
    );
    assert_eq!(claude["sync"]["ahead"], 4);
    assert_eq!(claude["sync"]["behind"], 1);
    let remote = row_by_ref(branches, "refs/remotes/origin/topic/claude");
    assert_eq!(remote["oid"], remote_tip);
    assert_eq!(remote["local"], false);
    assert_eq!(remote["current"], false);
    assert_eq!(remote["sealed"], true);
    let codex = row_by_ref(branches, "refs/heads/topic/codex");
    assert_eq!(codex["session_id"], SESSION);
    assert_eq!(codex["runtime"], "codex");
    assert_eq!(codex["sealed"], false);
    assert_eq!(codex["sync"]["ahead"], 0);
    assert_eq!(codex["sync"]["behind"], 0);
    let main = row_by_ref(branches, "refs/heads/main");
    assert_eq!(main["line"], "file");
    assert!(main["session_id"].is_null());
    assert!(main["runtime"].is_null());
    assert_eq!(main["turns"], 0);
    assert!(main["sync"]["upstream_ref"].is_null());
    assert!(main["sync"]["ahead"].is_null());
    assert!(main["sync"]["behind"].is_null());
    let missing = row_by_ref(branches, "refs/heads/missing-upstream");
    assert_eq!(
        missing["sync"]["upstream_ref"],
        "refs/remotes/origin/not-fetched"
    );
    assert!(missing["sync"]["ahead"].is_null());
    assert!(missing["sync"]["behind"].is_null());

    let local = history.lab.run(&["branch", "--json", "--repo", SLUG]);
    assert_eq!(local["branches"].as_array().unwrap().len(), 4);
    assert!(
        local["branches"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["local"] == true)
    );
    for arguments in [
        vec!["log", "--json", SLUG],
        vec!["log", "--json", "--branches"],
    ] {
        let value = history.lab.run(&arguments);
        assert_eq!(value["view"], "branches");
        assert_eq!(
            row_by_ref(&value["branches"], "refs/heads/topic/claude")["opening_subject"],
            history.opening
        );
    }
}

#[test]
fn graph_is_structured_and_preserves_merge_parents_and_full_refs() {
    let history = History::new();
    let value = history.lab.run(&["--json", "log", "--graph"]);
    assert_eq!(value["view"], "graph");
    let commits = value["commits"].as_array().unwrap();
    assert_eq!(commits.len(), 8);
    for commit in commits {
        assert_full_oid(&commit["oid"]);
        for parent in commit["parents"].as_array().unwrap() {
            assert_full_oid(parent);
        }
    }
    let merge = commits
        .iter()
        .find(|commit| commit["oid"] == history.head)
        .unwrap();
    assert_eq!(merge["parents"], json!([history.third, history.codex]));
    let root = commits
        .iter()
        .find(|commit| commit["oid"] == history.root)
        .unwrap();
    assert_eq!(root["parents"], json!([]));
    let opening = commits
        .iter()
        .find(|commit| commit["oid"] == history.first)
        .unwrap();
    assert_eq!(opening["subject"], history.opening);
    assert_eq!(
        row_by_ref(&value["refs"], "refs/heads/topic/claude")["oid"],
        history.head
    );
    assert_eq!(
        row_by_ref(&value["refs"], "refs/heads/topic/codex")["oid"],
        history.codex
    );
}

#[test]
fn graph_uses_an_explicit_repository_without_session_context() {
    let history = History::new();
    let value = Lab::document(
        history
            .lab
            .command(&["log", SLUG, "--graph", "--json"])
            .env_remove("AGIT_SESSION")
            .output()
            .unwrap(),
    );
    assert_eq!(value["view"], "graph");
    assert_eq!(value["commits"].as_array().unwrap().len(), 8);
    let human = history
        .lab
        .command(&["log", SLUG, "--graph"])
        .env_remove("AGIT_SESSION")
        .output()
        .unwrap();
    assert!(human.status.success(), "{human:?}");
    let text = String::from_utf8(human.stdout).unwrap();
    assert!(text.contains("*"), "{text}");
    assert!(text.contains("Merge the codex session"), "{text}");
    assert!(!text.contains("turns ·"), "{text}");
    let shorthand = Lab::document(
        history
            .lab
            .command(&["log", SLUG, "--json"])
            .env_remove("AGIT_SESSION")
            .output()
            .unwrap(),
    );
    assert_eq!(shorthand["view"], "branches");
}

#[test]
fn graph_and_branch_overview_are_incompatible_views() {
    let lab = Lab::new();
    for flags in [["--graph", "--branches"], ["--branches", "--graph"]] {
        let output = lab
            .command(&["log", SLUG, flags[0], flags[1], "--json"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(agit::ExitCode::Usage.as_i32()));
        let document: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(document["ok"], false);
        let diagnostics = document["diagnostics"].to_string();
        assert!(diagnostics.contains("--graph"), "{diagnostics}");
        assert!(diagnostics.contains("--branches"), "{diagnostics}");
    }
}

#[test]
fn unborn_repository_reports_successful_empty_branch_and_graph_arrays() {
    let lab = Lab::new();
    let branches = lab.run(&["branch", "--json", "--repo", SLUG, "--all"]);
    assert_eq!(branches["branches"], json!([]));
    let log = lab.run(&["log", "--json", SLUG]);
    assert_eq!(log["view"], "branches");
    assert_eq!(log["branches"], json!([]));
    let graph = lab.run(&["log", "--json", "--graph"]);
    assert_eq!(graph["view"], "graph");
    assert_eq!(graph["commits"], json!([]));
    assert_eq!(graph["refs"], json!([]));
}

#[test]
fn corrupt_committed_metadata_fails_without_fabricating_an_empty_history() {
    let history = History::new();
    fs::write(history.lab.repo.root().join(meta::FILE), "{broken metadata").unwrap();
    history.lab.repo.add_all().unwrap();
    history.lab.repo.commit("Invalid metadata fixture").unwrap();
    for arguments in [
        vec!["log", "--json"],
        vec!["branch", "--json", "--repo", SLUG],
        vec!["log", "--json", SLUG],
    ] {
        let output = history.lab.command(&arguments).output().unwrap();
        assert!(!output.status.success(), "{arguments:?}");
        let document: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(document["ok"], false);
        assert_ne!(document["result"]["value"]["branches"], json!([]));
        assert_ne!(document["result"]["value"]["turns"], json!([]));
    }
}

#[test]
fn only_metadata_of_branches_in_the_requested_view_can_block_that_view() {
    let history = History::new();
    let repo = &history.lab.repo;
    repo.git(&["checkout", "-q", "--detach", &history.head])
        .unwrap();
    fs::write(repo.root().join(meta::FILE), "{broken remote metadata").unwrap();
    repo.add_all().unwrap();
    repo.commit("Remote metadata fixture").unwrap();
    let corrupt = history.lab.oid("HEAD");
    repo.git(&["checkout", "-q", BRANCH]).unwrap();
    repo.git(&["update-ref", "refs/remotes/origin/topic/claude", &corrupt])
        .unwrap();

    for arguments in [
        vec!["branch", "--json", "--repo", SLUG],
        vec!["log", "--json", SLUG],
        vec!["log", "--json", "--branches"],
    ] {
        let value = history.lab.run(&arguments);
        assert_eq!(value["branches"].as_array().unwrap().len(), 3);
        assert_eq!(
            row_by_ref(&value["branches"], "refs/heads/topic/claude")["oid"],
            history.head
        );
    }
    let output = history
        .lab
        .command(&["branch", "--json", "--repo", SLUG, "--all"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["ok"], false);

    repo.git(&["update-ref", "refs/remotes/origin/remote-only", &corrupt])
        .unwrap();
    let local = history.lab.run(&["branch", "--json", "--repo", SLUG]);
    assert_eq!(local["branches"].as_array().unwrap().len(), 3);
    for arguments in [
        vec!["log", "--json", SLUG],
        vec!["log", "--json", "--branches"],
    ] {
        let output = history.lab.command(&arguments).output().unwrap();
        assert!(!output.status.success(), "{arguments:?}");
        let document: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(document["ok"], false);
        assert_ne!(document["result"]["value"]["branches"], json!([]));
    }
}

#[test]
fn opening_subject_belongs_to_the_first_numbered_turn_of_the_current_session() {
    const FORK_SESSION: &str = "agit-abcdef1234567890abcdef1234567890abcdef12";
    let lab = Lab::new();
    let file_meta = Meta::new_file_line();
    lab.commit(&file_meta, "Shared instructions", OLD);
    lab.commit(&file_meta, "Shared file maintenance", OLD + 1);
    lab.commit(&file_meta, "More shared file maintenance", OLD + 2);
    lab.repo.git(&["checkout", "-q", "-b", BRANCH]).unwrap();
    let mut source = Meta::new(SESSION.into(), "claude-code".into(), "/source".into());
    source.kind = Kind::File;
    lab.commit(&source, "agit: new session topic/claude", OLD + 3);
    lab.repo.git(&["branch", "newborn"]).unwrap();
    source.kind = Kind::Turn;
    lab.commit(&source, "Unnumbered handoff marker", OLD + 4);
    lab.repo.git(&["branch", "unnumbered"]).unwrap();
    source.turn = Some(1);
    lab.commit(&source, "The source session's opening prompt", OLD + 5);
    source.kind = Kind::File;
    lab.commit(&source, "Document source-session invariant", OLD + 6);
    source.kind = Kind::Turn;
    source.turn = Some(2);
    let source_head = lab.commit(&source, "A later source-session prompt", OLD + 7);

    lab.repo
        .git(&["checkout", "-q", "-b", "topic/fork", &source_head])
        .unwrap();
    let mut fork = Meta::new(FORK_SESSION.into(), "claude-code".into(), "/source".into());
    fork.kind = Kind::File;
    fork.turn = Some(2);
    lab.commit(&fork, "agit: fork topic/fork from topic/claude", OLD + 8);
    lab.repo.git(&["branch", "newborn-fork"]).unwrap();

    let value = lab.run(&["branch", "--json", "--repo", SLUG]);
    for reference in [
        "refs/heads/main",
        "refs/heads/newborn",
        "refs/heads/unnumbered",
        "refs/heads/newborn-fork",
        "refs/heads/topic/fork",
    ] {
        assert_eq!(
            row_by_ref(&value["branches"], reference)["opening_subject"],
            "",
            "{reference}"
        );
    }
    assert_eq!(
        row_by_ref(&value["branches"], "refs/heads/topic/claude")["opening_subject"],
        "The source session's opening prompt"
    );

    fork.kind = Kind::Turn;
    fork.turn = Some(3);
    lab.commit(&fork, "The fork session's own opening prompt", OLD + 9);
    fork.turn = Some(4);
    lab.commit(&fork, "A later fork-session prompt", OLD + 10);

    lab.repo
        .git(&["checkout", "-q", "-b", "topic/codex", &source_head])
        .unwrap();
    let mut codex = Meta::new(SESSION.into(), "codex".into(), "/source".into());
    codex.kind = Kind::File;
    codex.turn = Some(2);
    lab.commit(&codex, "Resume the source session with codex", OLD + 11);
    let value = lab.run(&["branch", "--json", "--repo", SLUG]);
    assert_eq!(
        row_by_ref(&value["branches"], "refs/heads/topic/codex")["opening_subject"],
        "The source session's opening prompt"
    );
    assert_eq!(
        row_by_ref(&value["branches"], "refs/heads/topic/codex")["runtime"],
        "codex"
    );
    codex.kind = Kind::Turn;
    codex.turn = Some(3);
    lab.commit(&codex, "Continue the source session with codex", OLD + 12);

    for arguments in [
        vec!["branch", "--json", "--repo", SLUG],
        vec!["log", "--json", SLUG],
        vec!["log", "--json", "--branches"],
    ] {
        let value = lab.run(&arguments);
        for (reference, expected) in [
            ("refs/heads/main", ""),
            ("refs/heads/newborn", ""),
            ("refs/heads/unnumbered", ""),
            ("refs/heads/newborn-fork", ""),
            (
                "refs/heads/topic/claude",
                "The source session's opening prompt",
            ),
            (
                "refs/heads/topic/fork",
                "The fork session's own opening prompt",
            ),
            (
                "refs/heads/topic/codex",
                "The source session's opening prompt",
            ),
        ] {
            assert_eq!(
                row_by_ref(&value["branches"], reference)["opening_subject"],
                expected,
                "{reference}"
            );
        }
    }
}

#[test]
fn human_branch_summaries_read_current_metadata_without_traversing_opening_history() {
    let lab = Lab::new();
    lab.commit(&Meta::new_file_line(), "Shared instructions", OLD);
    lab.repo.git(&["checkout", "-q", "-b", BRANCH]).unwrap();
    let mut metadata = Meta::new(SESSION.into(), "claude-code".into(), "/source".into());
    metadata.turn = Some(1);
    lab.commit(&metadata, "An earlier session turn", OLD + 1);
    fs::write(
        lab.repo.root().join(meta::FILE),
        "{broken historical metadata",
    )
    .unwrap();
    lab.repo.add_all().unwrap();
    lab.repo
        .commit("Unreadable historical metadata fixture")
        .unwrap();
    metadata.turn = Some(7);
    lab.commit(&metadata, "The current session turn", OLD + 3);

    let trace_path = lab._temp.path().join("human-branch-trace.jsonl");
    let plain = lab
        .command(&["branch", "--repo", SLUG])
        .env("GIT_TRACE2_EVENT", &trace_path)
        .output()
        .unwrap();
    assert!(
        plain.status.success(),
        "{}",
        String::from_utf8_lossy(&plain.stderr)
    );
    let plain = String::from_utf8(plain.stdout).unwrap();
    let row = plain.lines().find(|line| line.contains(BRANCH)).unwrap();
    assert!(
        row.split_whitespace()
            .collect::<Vec<_>>()
            .windows(2)
            .any(|pair| pair == ["7", "turns"]),
        "{row}"
    );
    let starts: Vec<Value> = fs::read_to_string(trace_path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|event| event["event"] == "start")
        .collect();
    assert!(!starts.is_empty());
    assert!(
        starts.iter().all(|event| {
            !event["argv"]
                .as_array()
                .unwrap()
                .iter()
                .any(|arg| arg == "--format=%H%x00%P%x00%s%x00%ct")
        }),
        "plain branch listing traversed the full history: {starts:?}"
    );

    let verbose = lab
        .command(&["branch", "--repo", SLUG, "-v"])
        .output()
        .unwrap();
    assert!(
        verbose.status.success(),
        "{}",
        String::from_utf8_lossy(&verbose.stderr)
    );
    let verbose = String::from_utf8(verbose.stdout).unwrap();
    assert!(verbose.contains(BRANCH), "{verbose}");
    assert!(verbose.contains(SESSION), "{verbose}");
    assert!(verbose.contains("runtime claude-code"), "{verbose}");
    assert!(verbose.contains("7 turns"), "{verbose}");
}

#[test]
fn branch_listing_batches_git_reads_as_the_ref_count_grows() {
    let history = History::new();
    let arguments = ["branch", "--json", "--repo", SLUG, "--all"];
    let before = history.lab.trace(&arguments, "before-trace.jsonl");
    assert!(!before.is_empty());
    for index in 0..24 {
        history
            .lab
            .repo
            .git(&[
                "branch",
                &format!("parallel/session-{index}"),
                &history.head,
            ])
            .unwrap();
    }
    let after = history.lab.trace(&arguments, "after-trace.jsonl");
    assert!(
        after.len() <= before.len() + 1,
        "git invocations grew with branch count: {} -> {}",
        before.len(),
        after.len()
    );
    let value = history.lab.run(&arguments);
    assert_eq!(value["branches"].as_array().unwrap().len(), 27);
}

#[cfg(unix)]
#[test]
fn history_inspection_does_not_open_live_transcript_pipes() {
    use std::{
        process::Stdio,
        thread,
        time::{Duration, Instant},
    };

    let history = History::new();
    let runtime_id = "aaaaaaaa-0000-4000-8000-000000000001";
    let transcript = history
        .lab
        .home
        .join(".claude/projects")
        .join(agit::domain::store::slug_for(&history.lab.cwd))
        .join(format!("{runtime_id}.jsonl"));
    fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    assert!(
        Command::new("mkfifo")
            .arg(&transcript)
            .status()
            .unwrap()
            .success()
    );
    let links = history.lab.store.join("store/claude-code");
    fs::create_dir_all(&links).unwrap();
    fs::write(
        links.join(format!("{runtime_id}.json")),
        json!({
            "owner": "organization", "agent": "history", "branch": BRANCH,
            "cwd": history.lab.cwd,
        })
        .to_string(),
    )
    .unwrap();
    for (index, arguments) in [
        vec!["log", "--json"],
        vec!["branch", "--json", "--repo", SLUG, "--all"],
        vec!["log", "--json", SLUG],
    ]
    .iter()
    .enumerate()
    {
        let stdout = history
            .lab
            ._temp
            .path()
            .join(format!("fifo-output-{index}.json"));
        let stderr = history
            .lab
            ._temp
            .path()
            .join(format!("fifo-error-{index}.txt"));
        let mut child = history
            .lab
            .command(arguments)
            .stdout(Stdio::from(fs::File::create(&stdout).unwrap()))
            .stderr(Stdio::from(fs::File::create(&stderr).unwrap()))
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("{arguments:?} blocked while inspecting a live transcript pipe");
            }
            thread::sleep(Duration::from_millis(20));
        };
        assert!(status.success(), "{}", fs::read_to_string(stderr).unwrap());
        let document: Value = serde_json::from_slice(&fs::read(stdout).unwrap()).unwrap();
        assert_eq!(document["result"]["format"], "json");
        assert_eq!(document["ok"], true);
    }
}

#[test]
fn metadata_history_does_not_read_saved_log_or_event_bodies() {
    for missing_log in [false, true] {
        let history = History::new();
        let lab = &history.lab;
        let mut metadata = Meta::new(SESSION.into(), "claude-code".into(), "/source".into());
        metadata.kind = Kind::Turn;
        metadata.turn = Some(4);
        meta::write(lab.repo.root(), &metadata).unwrap();
        let native = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"Saved event body\"}}\n";
        let envelopes = agit::domain::transcript::wrap_lines(native, "claude-code", SESSION);
        agit::domain::storage::write_snapshot(lab.repo.root(), &envelopes, &envelopes).unwrap();
        lab.repo.add_all().unwrap();
        lab.repo
            .git(&[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "Metadata with separate event evidence",
            ])
            .unwrap();
        let head = lab.oid("HEAD");
        let path = if missing_log {
            meta::LOG_FILE.to_owned()
        } else {
            let log = fs::read_to_string(lab.repo.root().join(meta::LOG_FILE)).unwrap();
            meta::event_path(log.trim()).unwrap()
        };
        let object = lab.oid(&format!("HEAD:{path}"));
        let loose = lab
            .repo
            .git(&[
                "rev-parse",
                "--git-path",
                &format!("objects/{}/{}", &object[..2], &object[2..]),
            ])
            .unwrap();
        fs::remove_file(lab.repo.root().join(loose.trim())).unwrap();
        for version in ["1", "2"] {
            let value = lab.run(&["log", "--json", "--json-version", version, "-n", "1"]);
            assert_eq!(value["turns"][0]["oid"], head);
            assert_eq!(value["turns"][0]["turn"], 4);
            assert_eq!(
                value["turns"][0]["subject"],
                "Metadata with separate event evidence"
            );
        }
        let text = lab.command(&["log", "-n", "1"]).output().unwrap();
        assert_eq!(
            text.status.code(),
            Some(agit::ExitCode::Precondition.as_i32())
        );
        assert!(
            String::from_utf8_lossy(&text.stderr).contains("cannot read this branch's history")
        );
    }
}
