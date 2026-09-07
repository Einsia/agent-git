use agit::domain::{meta, repo::Repo, storage, transcript};
use std::{collections::BTreeMap, fs, path::Path, process::Command};

struct Lab {
    temporary: tempfile::TempDir,
}

impl Lab {
    fn new() -> Self {
        Self {
            temporary: tempfile::Builder::new()
                .prefix("diff endpoints ")
                .tempdir()
                .unwrap(),
        }
    }

    fn repo(&self, name: &str) -> Repo {
        let repo = Repo::init(&self.temporary.path().join("repos/alice").join(name)).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
        fs::write(repo.root().join("AGENTS.md"), "shared base\n").unwrap();
        repo.add_all().unwrap();
        repo.commit(&format!("initialize {name}")).unwrap();
        repo
    }

    fn clone_repo(&self, source: &Repo, name: &str) -> Repo {
        let path = self.temporary.path().join("repos/alice").join(name);
        source
            .git(&[
                "clone",
                "--no-hardlinks",
                "--no-checkout",
                source.root().to_str().unwrap(),
                path.to_str().unwrap(),
            ])
            .unwrap();
        let repo = Repo::at(path);
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        repo.git(&["reset", "--hard", "HEAD"]).unwrap();
        repo
    }

    fn diff(&self, range: &str, mode: &str) -> std::process::Output {
        self.command(range, mode).output().unwrap()
    }

    fn command(&self, range: &str, mode: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command
            .args(["diff", range, mode])
            .current_dir(self.temporary.path())
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.temporary.path())
            .env("AGIT_HOME", self.temporary.path())
            .env("AGIT_HUB_URL", "http://127.0.0.1:1")
            .env("CI", "1")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null");
        command
    }
}

fn success(output: std::process::Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn turn(repo: &Repo, branch: &str, index: u32, content: &str) -> String {
    repo.git(&["checkout", "-B", branch]).unwrap();
    let claim = format!("agit-{}", "a".repeat(40));
    let raw = format!(
        "{}\n",
        serde_json::json!({"type":"response_item","payload":{
            "type":"message","role":"user","content":[{"type":"input_text","text":content}]
        }})
    );
    let envelope = transcript::wrap_lines(&raw, "codex", &claim);
    let prior = storage::materialize_worktree(repo.root(), meta::LOG_FILE).unwrap_or_default();
    let log = format!("{prior}{envelope}");
    storage::write_snapshot(repo.root(), &log, &log).unwrap();
    let mut snapshot = meta::Meta::new(claim, "codex".into(), "/fixture".into());
    snapshot.turn = Some(index);
    meta::write(repo.root(), &snapshot).unwrap();
    fs::write(repo.root().join("AGENTS.md"), format!("{content}\n")).unwrap();
    repo.add_all().unwrap();
    repo.commit(&format!("settle {content}")).unwrap();
    repo.git(&["rev-parse", "HEAD"]).unwrap()
}

fn bytes_under(root: &Path) -> BTreeMap<std::path::PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    for entry in walkdir::WalkDir::new(root) {
        let entry = entry.unwrap();
        if entry.file_type().is_file() {
            files.insert(
                entry.path().strip_prefix(root).unwrap().to_owned(),
                fs::read(entry.path()).unwrap(),
            );
        }
    }
    files
}

#[test]
fn explicit_cross_repository_points_compare_without_mutating_either_repository() {
    let lab = Lab::new();
    let left = lab.repo("left");
    let shared = turn(&left, "topic/shared", 1, "shared turn");
    let right = lab.clone_repo(&left, "right");
    turn(&left, "topic/left", 2, "left turn");
    turn(&right, "topic/right", 2, "right turn");
    success(lab.diff("alice/left@topic/left..topic/left", "--files"));
    let before_left = bytes_under(left.root());
    let before_right = bytes_under(right.root());

    let same = success(lab.diff("alice/left@topic/shared..alice/left@topic/left", "--files"));
    assert!(same.contains("+left turn") && same.contains("-shared turn"));
    let two = success(lab.diff("alice/left@topic/left..alice/right@topic/right", "--files"));
    assert!(two.contains("+right turn") && two.contains("-left turn"));
    let three = success(lab.diff("alice/left@topic/left...alice/right@topic/right", "--files"));
    assert!(three.contains("+right turn") && three.contains("-shared turn"));
    assert!(!three.contains("-left turn"));
    let view = success(lab.diff("alice/left@topic/left..alice/right@topic/right", "--view"));
    assert!(view.contains("common 1") && view.contains("removed 1") && view.contains("added 1"));
    let view = success(lab.diff("alice/left@topic/left...alice/right@topic/right", "--view"));
    assert!(view.contains("common 1") && view.contains("removed 0") && view.contains("added 1"));
    let turns = success(lab.diff("alice/left@topic/left...alice/right@topic/right", "--turns"));
    assert!(turns.contains(&format!("fork point  {}", &shared[..9])));
    assert!(turns.contains("A side    +1 turns") && turns.contains("B side    +1 turns"));
    assert_eq!(bytes_under(left.root()), before_left);
    assert_eq!(bytes_under(right.root()), before_right);
}

#[test]
fn unrelated_equal_content_never_becomes_a_claimed_fork_point() {
    let lab = Lab::new();
    let left = lab.repo("left");
    let right = lab.repo("right");
    turn(&left, "topic", 1, "same content");
    turn(&right, "topic", 1, "same content");
    let output = lab.diff("alice/left@topic...alice/right@topic", "--turns");
    assert!(String::from_utf8_lossy(&output.stderr).contains("no common Git ancestor"));
    let text = success(output);
    assert!(text.starts_with("base") && !text.contains("fork point"));
    assert!(text.contains("B side    +1 turns"));
    let view = success(lab.diff("alice/left@topic..alice/right@topic", "--view"));
    assert!(view.contains("removed 0") && view.contains("added 0"));
}

#[test]
fn historic_points_and_slash_branches_keep_their_selected_repository() {
    let lab = Lab::new();
    let repo = lab.repo("history");
    let first = turn(&repo, "topic/session", 1, "first turn");
    turn(&repo, "topic/session", 2, "second turn");
    repo.git(&["tag", "-a", "saved", &first, "-m", "historic point"])
        .unwrap();
    for left in ["topic/session#1", "topic/session~1", "saved"] {
        let range = format!("alice/history@{left}..alice/history@topic/session#-1");
        let text = success(lab.diff(&range, "--files"));
        assert!(text.contains("-first turn") && text.contains("+second turn"));
    }
    success(lab.diff("alice/history@main..topic/session", "--view"));
    for selector in ["topic/session#1.1", "topic/session:AGENTS.md"] {
        let output = lab.diff(
            &format!("alice/history@{selector}..topic/session"),
            "--view",
        );
        assert_eq!(output.status.code(), Some(agit::ExitCode::Usage.as_i32()));
        assert!(output.stdout.is_empty());
    }
    let output = lab.diff("alice/history@topic/session#1..#2", "--view");
    assert!(!output.status.success() && output.stdout.is_empty());
}

#[test]
fn non_turn_commits_do_not_inflate_the_turn_report() {
    let lab = Lab::new();
    let repo = lab.repo("history");
    let first = turn(&repo, "topic", 1, "first turn");
    turn(&repo, "topic", 2, "second turn");
    let mut snapshot = meta::read(repo.root()).unwrap();
    snapshot.kind = meta::Kind::View;
    meta::write(repo.root(), &snapshot).unwrap();
    repo.add_all().unwrap();
    repo.commit("view-only edit").unwrap();
    let report = success(lab.diff(&format!("alice/history@{first}..topic"), "--turns"));
    assert!(report.contains("B side    +1 turns"));
    assert!(!report.contains("view-only edit"));
}

#[test]
fn current_session_endpoints_keep_repo_and_branch_together() {
    let lab = Lab::new();
    let left = lab.repo("left");
    let right = lab.repo("right");
    turn(&left, "topic", 1, "left content");
    turn(&right, "topic", 1, "right content");
    for (range, identity) in [
        ("alice/left@topic..@", "alice/right@topic"),
        ("@..alice/right@topic", "alice/left@topic"),
    ] {
        let output = lab
            .command(range, "--files")
            .env("AGIT_SESSION", identity)
            .output()
            .unwrap();
        let text = success(output);
        assert!(text.contains("-left content") && text.contains("+right content"));
    }
    for range in [
        "alice/left@@..alice/right@topic",
        "alice/right@topic..alice/left@@",
    ] {
        let output = lab
            .command(range, "--files")
            .env("AGIT_SESSION", "alice/right@topic")
            .output()
            .unwrap();
        assert!(!output.status.success() && output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("belongs to alice/right"));
    }
    let output = lab.diff("@..alice/right@topic", "--files");
    assert_eq!(output.status.code(), Some(agit::ExitCode::Ref.as_i32()));
    assert!(output.stdout.is_empty());
}

#[test]
fn multiple_merge_bases_require_an_explicit_comparison_base() {
    let lab = Lab::new();
    let repo = lab.repo("left");
    let root = repo.git(&["rev-parse", "HEAD"]).unwrap();
    let tree = repo.git(&["rev-parse", "HEAD^{tree}"]).unwrap();
    let a = repo
        .git(&["commit-tree", &tree, "-p", &root, "-m", "left ancestry"])
        .unwrap();
    let b = repo
        .git(&["commit-tree", &tree, "-p", &root, "-m", "right ancestry"])
        .unwrap();
    let left = repo
        .git(&["commit-tree", &tree, "-p", &a, "-p", &b, "-m", "left merge"])
        .unwrap();
    let right = repo
        .git(&[
            "commit-tree",
            &tree,
            "-p",
            &b,
            "-p",
            &a,
            "-m",
            "right merge",
        ])
        .unwrap();
    repo.git(&["update-ref", "refs/heads/left", &left]).unwrap();
    repo.git(&["update-ref", "refs/heads/right", &right])
        .unwrap();
    lab.clone_repo(&repo, "right");
    for mode in ["--files", "--view", "--turns"] {
        let output = lab.diff("alice/left@left...alice/right@right", mode);
        assert_eq!(
            output.status.code(),
            Some(agit::ExitCode::Precondition.as_i32())
        );
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("multiple common Git ancestors"));
    }
    success(lab.diff("alice/left@left..alice/right@right", "--files"));
}

#[test]
fn missing_ancestry_is_an_error_instead_of_an_unrelated_history_claim() {
    let lab = Lab::new();
    let left = lab.repo("left");
    let right = lab.repo("right");
    let parent = turn(&left, "topic", 1, "lost ancestor");
    turn(&left, "topic", 2, "left head");
    turn(&right, "topic", 1, "right head");
    success(lab.diff("alice/left@topic..topic", "--files"));
    fs::remove_file(
        left.git_path("objects")
            .unwrap()
            .join(&parent[..2])
            .join(&parent[2..]),
    )
    .unwrap();
    let output = lab.diff("alice/left@topic...alice/right@topic", "--files");
    assert!(!output.status.success() && output.stdout.is_empty());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("cannot determine Git ancestry"), "{error}");
    assert!(!error.contains("no common Git ancestor"));
}

#[test]
fn linked_worktree_endpoints_read_their_common_object_store() {
    let lab = Lab::new();
    let left = lab.repo("left");
    turn(&left, "topic", 1, "common content");
    let right = lab.clone_repo(&left, "right");
    turn(&left, "topic", 2, "left content");
    turn(&right, "topic", 2, "right content");
    right.git(&["branch", "linked", "HEAD"]).unwrap();
    right
        .add_worktree(&lab.temporary.path().join("repos/alice/linked"), "linked")
        .unwrap();
    let text = success(lab.diff("alice/left@topic...alice/linked@linked", "--files"));
    assert!(text.contains("-common content") && text.contains("+right content"));
    assert!(!text.contains("-left content"));
}

#[cfg(unix)]
#[test]
fn quoted_alternate_paths_cannot_become_extra_object_store_entries() {
    let lab = Lab {
        temporary: tempfile::Builder::new()
            .prefix("diff \"quoted\"\npath ")
            .tempdir()
            .unwrap(),
    };
    let left = lab.repo("left");
    let right = lab.repo("right");
    turn(&left, "topic", 1, "left content");
    turn(&right, "topic", 1, "right content");
    let text = success(lab.diff("alice/left@topic..alice/right@topic", "--files"));
    assert!(text.contains("-left content") && text.contains("+right content"));
}

#[test]
fn a_file_line_is_an_empty_view_but_a_damaged_claimed_view_is_an_error() {
    let lab = Lab::new();
    let left = lab.repo("left");
    let right = lab.repo("right");
    turn(&right, "topic", 1, "right content");
    let output = success(lab.diff("alice/left@main..alice/right@topic", "--view"));
    assert!(output.contains("common 0") && output.contains("added 1"));
    fs::remove_file(right.root().join(meta::VIEW_FILE)).unwrap();
    right.add_all().unwrap();
    right.commit("claimed view unavailable").unwrap();
    let output = lab.diff("alice/left@main..alice/right@topic", "--view");
    assert!(!output.status.success() && output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("VIEW"));
    assert!(left.root().join("AGENTS.md").exists());
}

#[test]
fn a_shallow_endpoint_uses_the_comparison_graph_for_its_shared_view() {
    let lab = Lab::new();
    let right = lab.repo("right");
    let shared = turn(&right, "shared", 1, "shared content");
    turn(&right, "left", 2, "left content");
    right.git(&["checkout", "shared"]).unwrap();
    turn(&right, "right", 2, "right content");
    let left_path = lab.temporary.path().join("repos/alice/left");
    let source = format!(
        "file:///{}",
        right
            .root()
            .to_str()
            .unwrap()
            .replace('\\', "/")
            .trim_start_matches('/')
    );
    right
        .git(&[
            "clone",
            "--depth",
            "1",
            "--branch",
            "left",
            source.as_str(),
            left_path.to_str().unwrap(),
        ])
        .unwrap();
    let left = Repo::at(left_path);
    assert!(left.git_path("shallow").unwrap().is_file());
    assert_ne!(
        left.git_status(&["cat-file", "-e", &shared]).unwrap().0,
        Some(0)
    );
    success(lab.diff("alice/left@left..left", "--files"));
    let left_before = bytes_under(left.root());
    let right_before = bytes_under(right.root());
    let range = "alice/left@left...alice/right@right";
    let files = success(lab.diff(range, "--files"));
    assert!(files.contains("-shared content") && files.contains("+right content"));
    let view = success(lab.diff(range, "--view"));
    assert!(view.contains("common 1") && view.contains("removed 0") && view.contains("added 1"));
    let turns = success(lab.diff(range, "--turns"));
    assert!(turns.contains("A side    +1 turns") && turns.contains("B side    +1 turns"));
    assert_eq!(bytes_under(left.root()), left_before);
    assert_eq!(bytes_under(right.root()), right_before);
}

#[test]
fn current_session_uses_only_its_local_branch_despite_other_ref_names() {
    let lab = Lab::new();
    let repo = lab.repo("history");
    let head = turn(&repo, "topic", 1, "session content");
    repo.git(&["tag", "topic", "main"]).unwrap();
    repo.git(&["tag", &head, "main"]).unwrap();
    let output = lab
        .command("alice/history@main..@#1", "--files")
        .env("AGIT_SESSION", "alice/history@topic")
        .output()
        .unwrap();
    let text = success(output);
    assert!(text.contains("+session content"));
}

#[test]
fn a_missing_current_session_branch_never_falls_back_to_tags_or_remotes() {
    let mut substituted = Vec::new();
    for replacement in ["refs/tags/topic", "refs/remotes/origin/topic"] {
        let lab = Lab::new();
        let repo = lab.repo("history");
        let head = turn(&repo, "topic", 1, "session content");
        repo.git(&["checkout", "main"]).unwrap();
        repo.git(&["branch", "-D", "topic"]).unwrap();
        repo.git(&["update-ref", replacement, &head]).unwrap();
        let output = lab
            .command("alice/history@main..@", "--files")
            .env("AGIT_SESSION", "alice/history@topic")
            .output()
            .unwrap();
        if output.status.success() {
            substituted.push(replacement);
        } else {
            assert!(output.stdout.is_empty());
        }
    }
    assert!(substituted.is_empty(), "substituted {substituted:?}");
}
