//! A merge that would launch an agent must refuse before it settles, materializes, or locks.
//!
//! `Command::output` deliberately gives the child no TTY, which is the same shape as CI and an
//! agent harness.  The setup uses `--no-launch` only for creating the two empty session lines; the
//! merge itself follows the normal launching path and must stop at its preflight gate.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Lab {
    _tmp: tempfile::TempDir,
    agit_home: PathBuf,
    home: PathBuf,
    work: PathBuf,
}

impl Lab {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let agit_home = tmp.path().join("agit");
        let home = tmp.path().join("home");
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        Self {
            _tmp: tmp,
            agit_home,
            home,
            work,
        }
    }

    fn agit(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_agit"))
            .args(args)
            .current_dir(&self.work)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("AGIT_HOME", &self.agit_home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap()
    }

    fn repo(&self) -> PathBuf {
        self.agit_home.join("repos").join("local").join("repo")
    }
}

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

#[test]
fn noninteractive_merge_refuses_before_opening_transaction() {
    let lab = Lab::new();
    for args in [
        ["init", "repo", "--no-bind"].as_slice(),
        ["new", "local/repo", "-b", "target", "--no-launch"].as_slice(),
        ["new", "local/repo", "-b", "source", "--no-launch"].as_slice(),
    ] {
        let out = lab.agit(args);
        assert!(
            out.status.success(),
            "`agit {}` failed:\n{}{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let repo = lab.repo();
    let target_before = git(&repo, &["rev-parse", "refs/heads/target"]);
    let out = lab.agit(&["merge", "local/repo@source", "--into", "local/repo@target"]);
    assert_eq!(
        out.status.code(),
        Some(agit::ExitCode::Interactive.as_i32()),
        "merge should refuse before launching in a non-TTY environment: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("requires an interactive terminal"),
        "stderr should explain the TTY requirement: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !repo.join(".git").join("AGIT_MERGE_TX").exists(),
        "the merge transaction must not be opened when launch preflight fails"
    );
    assert_eq!(
        git(&repo, &["rev-parse", "refs/heads/target"]),
        target_before,
        "the target ref must not move"
    );
}
