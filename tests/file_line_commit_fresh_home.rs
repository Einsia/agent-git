//! File-line commits work without a session store, and initialization preserves existing user files.
//! Selecting a file line must not require importing a session or replace hand-written scaffolding.

use std::path::{Path, PathBuf};
use std::process::Command;

const HUB: &str = "http://127.0.0.1:9";

struct Lab {
    _tmp: tempfile::TempDir,
    agit_home: PathBuf,
    work: PathBuf,
}

impl Lab {
    fn new() -> Lab {
        let tmp = tempfile::tempdir().unwrap();
        let agit_home = tmp.path().join("agit-home");
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        // Signed in: the commit's author fields come from the credentials. The rest of the home
        // directory is empty — no store.
        let cred = agit::infra::credentials::HubCredential {
            account_id: None,
            username: "me".into(),
            email: None,
            hub: Some(HUB.into()),
            access_token: "fake".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_token: "fake".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        };
        let key = agit::infra::config::hub_host_key(HUB).unwrap();
        agit::infra::credentials::save_at(
            &agit_home.join("credentials").join(format!("{key}.json")),
            &cred,
        )
        .unwrap();
        Lab {
            _tmp: tmp,
            agit_home,
            work,
        }
    }

    fn agit(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_agit"))
            .args(args)
            .current_dir(&self.work)
            .env("AGIT_HOME", &self.agit_home)
            .env("AGIT_HUB_URL", HUB)
            .env("AGIT_YES", "1")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.agit(args);
        assert!(
            out.status.success(),
            "`agit {}` failed:\n{}{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn repo_dir(&self, name: &str) -> PathBuf {
        self.agit_home.join("repos").join("me").join(name)
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn a_file_line_commit_needs_no_session_store() {
    let lab = Lab::new();
    lab.ok(&["init", "notes", "--no-bind"]);
    assert!(
        !lab.agit_home.join("store").exists(),
        "the fixture must start without a store, or the test proves nothing"
    );

    let repo = lab.repo_dir("notes");
    std::fs::write(repo.join("README.md"), "# notes\n").unwrap();
    let out = lab.agit(&[
        "commit",
        "me/notes@main",
        "-m",
        "docs: add README",
        "--",
        "README.md",
    ]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "a file-line commit must not depend on a session store:\n{stderr}"
    );
    assert!(
        !stderr.contains("no local store"),
        "the store gate must not fire for a file-line target: {stderr}"
    );
    assert_eq!(
        git(&repo, &["log", "-1", "--format=%s", "refs/heads/main"]),
        "docs: add README"
    );
    assert_eq!(
        git(&repo, &["show", "refs/heads/main:README.md"]),
        "# notes"
    );
}

#[test]
fn init_refuses_to_scaffold_over_untracked_user_content() {
    let lab = Lab::new();
    // The shell `agit repo create` + `agit clone` leave behind: a checkout with no commit at all.
    let repo = lab.repo_dir("shell");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "--initial-branch=main"]);
    let mine = "# my notes, not yet committed\n";
    std::fs::write(repo.join("AGENTS.md"), mine).unwrap();

    let out = lab.agit(&["init", "shell", "--no-bind"]);
    assert_eq!(
        out.status.code(),
        Some(agit::ExitCode::Precondition.as_i32()),
        "init must refuse a checkout that already holds user content:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("AGENTS.md")).unwrap(),
        mine,
        "the refused init must leave the user's file byte-for-byte intact"
    );
    assert!(
        !repo.join("session").exists() && !repo.join("memory").exists(),
        "no scaffold may land on a refused checkout"
    );

    // Once it is cleared, the same checkout can be taken over.
    std::fs::remove_file(repo.join("AGENTS.md")).unwrap();
    lab.ok(&["init", "shell", "--no-bind"]);
    assert_eq!(
        git(&repo, &["log", "-1", "--format=%s", "refs/heads/main"]),
        "agit: init (main file line)"
    );
}

/// A repository created on the Hub clones into a checkout with no commit at all, and `agit new`
/// on it starts from a main file line it lays down itself. An implementation that resolves
/// `main` first fails with "no ref `main`"; one that branches the session off nothing leaves the
/// repo without the file line every later `agit new` inherits from.
#[test]
fn new_on_an_empty_checkout_lays_down_the_main_file_line() {
    let lab = Lab::new();
    let repo = lab.agit_home.join("repos").join("simco").join("lumen");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "--initial-branch=main"]);

    lab.ok(&[
        "new",
        "simco/lumen",
        "-b",
        "first",
        "--as",
        "codex",
        "--no-launch",
        "--fresh",
    ]);
    assert_eq!(
        git(&repo, &["log", "-1", "--format=%s", "refs/heads/main"]),
        "agit: init (main file line)"
    );
    assert!(
        git(&repo, &["show", "refs/heads/main:session/meta.json"]).contains(r#""line": "file""#),
        "the laid-down main must declare itself a file line"
    );
    assert_eq!(
        git(&repo, &["rev-parse", "refs/heads/first^"]),
        git(&repo, &["rev-parse", "refs/heads/main"]),
        "the first session must inherit from the laid-down main"
    );
}

/// File commits preserve code symbols and artifact references in the canonical Git tree.
#[test]
fn file_commit_preserves_code_and_artifact_references() {
    let lab = Lab::new();
    lab.ok(&["init", "project", "--no-bind"]);
    let repo = lab.repo_dir("project");
    let files = [
        (
            "README.md",
            "# Project\n[Guide](artifacts/03-guide-A4-fold.pdf)\n[Report](output/pdf/central-park-2024-reading.pdf)",
        ),
        (
            "server.py",
            "from http.server import SimpleHTTPRequestHandler",
        ),
        ("client.js", "export class SimpleHTTPRequestHandler {}"),
        ("lib.rs", "pub struct SimpleHTTPRequestHandler;"),
        (
            "guide.md",
            "[Data](datasets/regional-2026-observations.csv)\n[Chart](figures/regional-2026-observations.svg)",
        ),
        (
            "datasets/regional-2026-observations.csv",
            "region,value\nNorth,3",
        ),
        (
            "figures/regional-2026-observations.svg",
            "<svg xmlns=\"http://www.w3.org/2000/svg\"/>",
        ),
    ];
    for (path, content) in files {
        let source = lab.work.join(path);
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, content).unwrap();
        let source = source.canonicalize().unwrap();
        lab.ok(&[
            "file",
            "add",
            "--into",
            "me/project@main",
            "--to",
            path,
            source.to_str().unwrap(),
        ]);
    }
    lab.ok(&[
        "file",
        "commit",
        "--into",
        "me/project@main",
        "-m",
        "Add project files",
    ]);
    for (path, content) in files {
        assert_eq!(
            git(&repo, &["show", &format!("refs/heads/main:{path}")]),
            content,
            "{path}"
        );
    }
}

/// A repository whose `main` the Hub or plain Git wrote, with no AgentGit metadata in its tree
/// (an organization wiki started on the Hub), still takes file commits on `main`: the pages named
/// are committed, not left out by a session settlement that has nothing to settle.
#[test]
fn a_file_commit_on_a_main_without_agentgit_metadata_takes_the_named_pages() {
    let lab = Lab::new();
    let repo = lab.repo_dir("wiki");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "--initial-branch=main"]);
    std::fs::write(repo.join("README.md"), "# wiki\n").unwrap();
    git(&repo, &["add", "README.md"]);
    git(
        &repo,
        &[
            "-c",
            "user.name=hub",
            "-c",
            "user.email=hub@example.test",
            "commit",
            "-qm",
            "start the wiki",
        ],
    );

    std::fs::write(
        repo.join("run.md"),
        "# Running\n\nStart the database first.\n",
    )
    .unwrap();
    std::fs::write(repo.join("README.md"), "# wiki\n\n- [Running](run.md)\n").unwrap();
    lab.ok(&[
        "commit",
        "me/wiki@main",
        "-m",
        "wiki: how to run",
        "--",
        "run.md",
        "README.md",
    ]);
    assert_eq!(
        git(&repo, &["log", "-1", "--format=%s", "refs/heads/main"]),
        "wiki: how to run"
    );
    assert_eq!(
        git(&repo, &["show", "refs/heads/main:run.md"]),
        "# Running\n\nStart the database first."
    );
    assert!(git(&repo, &["show", "refs/heads/main:README.md"]).contains("[Running](run.md)"));
}
