//! Merge reconnaissance borrows graph evidence without changing either selected repository.

use agit::domain::{mergetx, meta, repo::Repo, storage, transcript};
use std::{collections::BTreeMap, fs, path::Path, process::Command};

struct Lab {
    temporary: tempfile::TempDir,
}

impl Lab {
    fn new() -> Self {
        Self {
            temporary: tempfile::tempdir().unwrap(),
        }
    }

    fn repo(&self, name: &str) -> Repo {
        let repo = Repo::init(&self.temporary.path().join("repos/alice").join(name)).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
        fs::write(
            repo.root().join("AGENTS.md"),
            "Synthetic shared instructions\n",
        )
        .unwrap();
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
                source.root().to_str().unwrap(),
                path.to_str().unwrap(),
            ])
            .unwrap();
        let repo = Repo::at(path);
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        repo.ensure_committer().unwrap();
        repo
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command
            .args(args)
            .current_dir(self.temporary.path())
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.temporary.path())
            .env("AGIT_HOME", self.temporary.path())
            .env("AGIT_HUB_URL", "http://127.0.0.1:1")
            .env("CI", "1")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                self.temporary.path().join("empty-gitconfig"),
            )
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("NO_COLOR", "1");
        #[cfg(windows)]
        {
            for name in ["SystemRoot", "WINDIR", "TEMP", "TMP", "ComSpec"] {
                if let Some(value) = std::env::var_os(name) {
                    command.env(name, value);
                }
            }
            command.env("USERPROFILE", self.temporary.path());
        }
        command.output().unwrap()
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
    repo.add_all().unwrap();
    repo.commit(content).unwrap();
    repo.git(&["rev-parse", "HEAD"]).unwrap()
}

fn bytes_under(root: &Path) -> BTreeMap<std::path::PathBuf, Vec<u8>> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .map(Result::unwrap)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| {
            (
                entry.path().strip_prefix(root).unwrap().to_owned(),
                fs::read(entry.path()).unwrap(),
            )
        })
        .collect()
}

#[test]
fn related_repositories_report_each_side_since_the_real_common_commit() {
    let lab = Lab::new();
    let left = lab.repo("left");
    let shared = turn(&left, "shared", 1, "shared turn");
    let right = lab.clone_repo(&left, "right");
    turn(&left, "target", 2, "target turn");
    turn(&right, "source", 2, "source first turn");
    turn(&right, "source", 3, "source next turn");
    success(lab.run(&["log", "alice/left@target", "--oneline"]));
    let before_left = bytes_under(left.root());
    let before_right = bytes_under(right.root());
    let text = success(lab.run(&[
        "merge",
        "alice/right@source",
        "--into",
        "alice/left@target",
        "--dry-run",
    ]));
    assert!(
        text.contains(&format!("fork point  {}", &shared[..9])),
        "{text}"
    );
    assert!(
        text.contains("this side  +1 turns    source side  +2 turns"),
        "{text}"
    );
    assert_eq!(bytes_under(left.root()), before_left);
    assert_eq!(bytes_under(right.root()), before_right);
    assert!(mergetx::read(left.root()).unwrap().is_none());
}

#[test]
fn a_same_repository_fork_uses_turns_instead_of_identity_or_file_commits() {
    let lab = Lab::new();
    let repo = lab.repo("left");
    let shared = turn(&repo, "shared", 1, "shared turn");
    turn(&repo, "target", 2, "target turn");
    fs::write(repo.root().join("AGENTS.md"), "Target file edit\n").unwrap();
    let mut file = meta::read(repo.root()).unwrap();
    file.kind = meta::Kind::File;
    file.turn = None;
    meta::write(repo.root(), &file).unwrap();
    repo.add_all().unwrap();
    repo.commit("shared file edit").unwrap();
    repo.git(&["checkout", "shared"]).unwrap();
    turn(&repo, "source", 2, "source turn");
    let text = success(lab.run(&[
        "merge",
        "alice/left@source",
        "--into",
        "alice/left@target",
        "--dry-run",
    ]));
    assert!(
        text.contains(&format!("fork point  {}", &shared[..9])),
        "{text}"
    );
    assert!(
        text.contains("this side  +1 turns    source side  +1 turns"),
        "{text}"
    );
}

#[test]
fn unrelated_equal_content_never_claims_a_fork_or_zero_added_turns() {
    let lab = Lab::new();
    let left = lab.repo("left");
    let right = lab.repo("right");
    turn(&left, "target", 1, "same content");
    turn(&right, "source", 1, "same content");
    let args = ["merge", "alice/right@source", "--into", "alice/left@target"];
    let text = success(lab.run(&[args.as_slice(), &["--dry-run"]].concat()));
    assert!(text.contains("fork point  unavailable"), "{text}");
    assert!(
        text.contains("this side  unknown    source side  unknown"),
        "{text}"
    );
    success(lab.run(&[args.as_slice(), &["--manual"]].concat()));
    assert!(mergetx::read(left.root()).unwrap().unwrap().base.is_empty());
    let status = success(lab.run(&["merge", "--status", "--into", "alice/left@target"]));
    assert!(status.contains("fork point    unavailable"), "{status}");
    success(lab.run(&["merge", "--abort", "--into", "alice/left@target"]));
}

#[test]
fn history_already_reachable_through_the_fork_point_is_not_added_again() {
    let lab = Lab::new();
    let left = lab.repo("left");
    turn(&left, "shared", 1, "shared turn");
    turn(&left, "a", 2, "left ancestor turn");
    left.git(&["checkout", "shared"]).unwrap();
    let b = turn(&left, "b", 2, "right ancestor turn");
    left.git(&["checkout", "a"]).unwrap();
    left.git(&["merge", "--no-commit", "-s", "ours", "b"])
        .unwrap();
    let mut metadata = meta::read(left.root()).unwrap();
    metadata.kind = meta::Kind::Merge;
    meta::write(left.root(), &metadata).unwrap();
    left.add_all().unwrap();
    left.commit("common reconciliation").unwrap();
    let base = left.git(&["rev-parse", "HEAD"]).unwrap();
    let right = lab.clone_repo(&left, "right");
    turn(&left, "target", 3, "new target turn");
    right.git(&["checkout", "-b", "source", &b]).unwrap();
    right
        .git(&["merge", "--no-ff", "--no-commit", "-s", "ours", &base])
        .unwrap();
    meta::write(right.root(), &metadata).unwrap();
    right.add_all().unwrap();
    right.commit("retain another primary line").unwrap();
    turn(&right, "source", 3, "new source turn");
    let text = success(lab.run(&[
        "merge",
        "alice/right@source",
        "--into",
        "alice/left@target",
        "--dry-run",
    ]));
    assert!(
        text.contains(&format!("fork point  {}", &base[..9])),
        "{text}"
    );
    assert!(
        text.contains("this side  +1 turns    source side  +1 turns"),
        "{text}"
    );
}

#[test]
fn incomplete_or_ambiguous_graphs_refuse_before_a_transaction() {
    for shape in ["shallow", "missing", "ambiguous"] {
        let lab = Lab::new();
        let repo = lab.repo("left");
        let shared = turn(&repo, "shared", 1, "shared turn");
        let tree = repo.git(&["rev-parse", "HEAD^{tree}"]).unwrap();
        let a = repo
            .git(&["commit-tree", &tree, "-p", &shared, "-m", "left child"])
            .unwrap();
        let b = repo
            .git(&["commit-tree", &tree, "-p", &shared, "-m", "right child"])
            .unwrap();
        let (left, right) = if shape == "ambiguous" {
            (
                repo.git(&["commit-tree", &tree, "-p", &a, "-p", &b, "-m", "left merge"])
                    .unwrap(),
                repo.git(&[
                    "commit-tree",
                    &tree,
                    "-p",
                    &b,
                    "-p",
                    &a,
                    "-m",
                    "right merge",
                ])
                .unwrap(),
            )
        } else {
            (a.clone(), b)
        };
        repo.git(&["update-ref", "refs/heads/target", &left])
            .unwrap();
        repo.git(&["update-ref", "refs/heads/source", &right])
            .unwrap();
        success(lab.run(&["log", "alice/left@target", "--oneline"]));
        match shape {
            "shallow" => {
                fs::write(repo.common_dir().unwrap().join("shallow"), format!("{a}\n")).unwrap()
            }
            "missing" => {
                let object = repo
                    .common_dir()
                    .unwrap()
                    .join("objects")
                    .join(&shared[..2])
                    .join(&shared[2..]);
                #[cfg(windows)]
                {
                    // The corruption fixture must remove its object even if Git marks it read-only.
                    let mut permissions = fs::metadata(&object).unwrap().permissions();
                    permissions.set_readonly(false);
                    fs::set_permissions(&object, permissions).unwrap();
                }
                fs::remove_file(object).unwrap();
            }
            _ => {}
        }
        let before = bytes_under(repo.root());
        let output = lab.run(&[
            "merge",
            "alice/left@source",
            "--into",
            "alice/left@target",
            "--manual",
        ]);
        assert!(!output.status.success(), "{shape}: {output:?}");
        assert_eq!(bytes_under(repo.root()), before, "{shape}");
        assert!(mergetx::read(repo.root()).unwrap().is_none());
    }
}

/// Keeping the target is an explicit conflict decision, not an inferred empty worktree diff.
#[test]
fn manual_file_merge_requires_exact_current_conflict_acknowledgement() {
    let lab = Lab::new();
    let repo = lab.repo("files");
    fs::create_dir_all(repo.root().join("memory")).unwrap();
    fs::write(repo.root().join("memory/common.md"), "Base memory\n").unwrap();
    repo.add_all().unwrap();
    repo.commit("synthetic common memory").unwrap();
    repo.git(&["checkout", "-b", "source"]).unwrap();
    fs::write(repo.root().join("AGENTS.md"), "Source instructions\n").unwrap();
    fs::create_dir_all(repo.root().join("memory")).unwrap();
    fs::write(repo.root().join("memory/source.md"), "Source addition\n").unwrap();
    repo.add_all().unwrap();
    repo.commit("synthetic source change").unwrap();
    fs::write(repo.root().join("memory/common.md"), "Source memory\n").unwrap();
    repo.add_all().unwrap();
    repo.commit("synthetic source memory").unwrap();
    let source = repo.git(&["rev-parse", "HEAD"]).unwrap();
    repo.git(&["checkout", "main"]).unwrap();
    fs::write(repo.root().join("AGENTS.md"), "Target instructions\n").unwrap();
    repo.add_all().unwrap();
    repo.commit("synthetic target change").unwrap();
    fs::write(repo.root().join("memory/common.md"), "Target memory\n").unwrap();
    repo.add_all().unwrap();
    repo.commit("synthetic target memory").unwrap();
    let target = repo.git(&["rev-parse", "HEAD"]).unwrap();
    success(lab.run(&[
        "merge",
        "alice/files@source",
        "--into",
        "alice/files@main",
        "--manual",
    ]));
    success(lab.run(&[
        "merge",
        "--into",
        "alice/files@main",
        "summary",
        "-m",
        "Keep the target instructions",
    ]));
    let refs = repo.git(&["show-ref"]).unwrap();
    let tx = serde_json::to_value(mergetx::read(repo.root()).unwrap().unwrap()).unwrap();
    for extra in [
        vec![],
        vec!["--resolved", "AGENTS.md"],
        vec!["--resolved", "missing.md"],
        vec!["--resolved", "memory/source.md"],
        vec!["--resolved", "../AGENTS.md"],
        vec!["--resolved", meta::FILE],
        vec!["--resolved", "AGENTS.md", "--resolved", "AGENTS.md"],
    ] {
        let args = [
            &["merge", "--continue", "--into", "alice/files@main"][..],
            extra.as_slice(),
        ]
        .concat();
        let output = lab.run(&args);
        assert_eq!(output.status.code(), Some(4), "{args:?}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("shared-file reconciliation is incomplete")
        );
        assert_eq!(repo.git(&["show-ref"]).unwrap(), refs);
        assert_eq!(
            serde_json::to_value(mergetx::read(repo.root()).unwrap().unwrap()).unwrap(),
            tx
        );
        assert_eq!(
            fs::read_to_string(repo.root().join("AGENTS.md")).unwrap(),
            "Target instructions\n"
        );
    }
    for flags in [
        vec![],
        vec!["--status"],
        vec!["--abort"],
        vec!["--continue", "--manual"],
    ] {
        let args = [
            &[
                "merge",
                "--into",
                "alice/files@main",
                "--resolved",
                "AGENTS.md",
            ][..],
            flags.as_slice(),
        ]
        .concat();
        let output = lab.run(&args);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        assert_eq!(repo.git(&["show-ref"]).unwrap(), refs);
        assert_eq!(
            serde_json::to_value(mergetx::read(repo.root()).unwrap().unwrap()).unwrap(),
            tx
        );
    }
    let markers = "<<<<<<< ours\nUnresolved\n=======\nOther\n>>>>>>> theirs\n";
    fs::write(repo.root().join("AGENTS.md"), markers).unwrap();
    let rejected = lab.run(&[
        "merge",
        "--continue",
        "--into",
        "alice/files@main",
        "--resolved",
        "AGENTS.md",
        "--resolved",
        "memory/common.md",
    ]);
    assert_eq!(rejected.status.code(), Some(4), "{rejected:?}");
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("conflict-marker check"));
    assert_eq!(repo.git(&["show-ref"]).unwrap(), refs);
    assert_eq!(
        fs::read_to_string(repo.root().join("AGENTS.md")).unwrap(),
        markers
    );
    fs::write(repo.root().join("AGENTS.md"), "Target instructions\n").unwrap();
    assert_eq!(
        repo.git(&["diff", "--name-only", &target, "--"]).unwrap(),
        ""
    );
    success(lab.run(&[
        "merge",
        "--continue",
        "--into",
        "alice/files@main",
        "--resolved",
        "AGENTS.md",
        "--resolved",
        "memory/common.md",
    ]));
    let merged = repo.git(&["rev-parse", "HEAD"]).unwrap();
    assert_eq!(
        repo.git(&["show", "-s", "--format=%P", &merged]).unwrap(),
        format!("{target} {source}")
    );
    assert_eq!(
        fs::read_to_string(repo.root().join("AGENTS.md")).unwrap(),
        "Target instructions\n"
    );
    assert_eq!(
        fs::read_to_string(repo.root().join("memory/source.md")).unwrap(),
        "Source addition\n"
    );
    assert_eq!(
        fs::read_to_string(repo.root().join("memory/common.md")).unwrap(),
        "Target memory\n"
    );
    assert!(mergetx::read(repo.root()).unwrap().is_none());
    assert!(
        meta::read_at_ref_result(&repo, &merged)
            .unwrap()
            .unwrap()
            .is_file_line()
    );
    assert_eq!(repo.git(&["status", "--porcelain"]).unwrap(), "");
}
