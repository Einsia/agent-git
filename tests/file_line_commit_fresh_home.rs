//! Two regressions reached through the command entry point, both on a machine that has **never
//! run import / new**:
//!
//! 1. `agit commit <owner/repo>@main -m` needs no session store. The file line's contract is to
//!    depend on no session; an entry point that requires `$AGIT_HOME/store` to exist before it
//!    resolves the target makes the manual path `repo create → clone → init → write README →
//!    commit` exit at its last step.
//! 2. `agit init` taking over an empty checkout does not overwrite user content. An AGENTS.md
//!    written by hand after the clone and not yet committed is the user's bytes; the scaffold
//!    must not paint over it.

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
            username: "me".into(),
            email: None,
            hub: Some(HUB.into()),
            access_token: "fake".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_token: "fake".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        };
        let key = agit::infra::config::hub_host_key(HUB);
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
