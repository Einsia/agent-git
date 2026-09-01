//! `@` is shorthand for "the current session branch" and reads only the session environment
//! (`AGIT_SESSION`).
//!
//! The resolver does not read the environment: turning `@` into a branch name is the command
//! layer's job. This pins two things from the CLI entry point — with `AGIT_SESSION` carried, `@`
//! is that branch; without it, `@` does not guess the pinned branch, even when the directory
//! happens to pin one. An implementation that hands `@` straight to the resolver errors on the
//! former; one that hands `@` to the full context chain silently points at the pinned branch on
//! the latter.

use agit::domain::repo::Repo;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::{fs, process::Command};

const REPO: &str = "drh/qa";
const BRANCH: &str = "first-session";

/// A local Agent repo with one turn commit, on branch [`BRANCH`]; returns `AGIT_HOME`.
fn fixture(tmp: &Path) -> PathBuf {
    let home = tmp.join("home");
    let repo = Repo::init(&home.join("repos").join(REPO)).unwrap();
    repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
    fs::write(repo.root().join("README.md"), "fixture").unwrap();
    repo.add_all().unwrap();
    repo.commit("first turn").unwrap();
    repo.git(&["branch", "-m", BRANCH]).unwrap();
    home
}

/// Binds `work` to [`REPO`] and pins [`BRANCH`] — the fallback `@` must not look at.
fn pin_workspace(home: &Path, work: &Path) {
    let canonical = work.canonicalize().unwrap();
    let id = &hex::encode(Sha256::digest(canonical.to_string_lossy().as_bytes()))[..16];
    let dir = home.join("workspaces");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(format!("{id}.json")),
        serde_json::to_vec(&serde_json::json!({
            "dir": canonical,
            "repo": REPO,
            "pinned": BRANCH,
        }))
        .unwrap(),
    )
    .unwrap();
}

fn agit(home: &Path, work: &Path, session: Option<&str>, args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agit"));
    cmd.args(args)
        .current_dir(work)
        .env("AGIT_HOME", home)
        .env_remove("AGIT_SESSION");
    if let Some(session) = session {
        cmd.env("AGIT_SESSION", session);
    }
    cmd.output().unwrap()
}

#[test]
fn at_names_the_branch_carried_by_agit_session() {
    let tmp = tempfile::tempdir().unwrap();
    let home = fixture(tmp.path());
    let work = tmp.path().join("work");
    fs::create_dir_all(&work).unwrap();
    let session = format!("{REPO}@{BRANCH}");

    let out = agit(&home, &work, Some(&session), &["log", "@", "--oneline"]);
    assert!(
        out.status.success(),
        "log @: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("first turn"));

    let out = agit(&home, &work, Some(&session), &["show", "@:README.md"]);
    assert!(
        out.status.success(),
        "show @:path: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "fixture");

    // `scan` validates the ref before it scans the whole repo: `@` has to become a branch
    // name before that validation.
    let out = agit(&home, &work, Some(&session), &["scan", "@", "--secrets"]);
    assert!(
        out.status.success(),
        "scan @ --secrets: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn at_never_falls_back_to_the_workspace_pin() {
    let tmp = tempfile::tempdir().unwrap();
    let home = fixture(tmp.path());
    let work = tmp.path().join("work");
    fs::create_dir_all(&work).unwrap();
    pin_workspace(&home, &work);

    // The pin answers "which repo", so the command gets as far as `@`; the only thing left
    // that can fail is `@` itself.
    let out = agit(&home, &work, None, &["log", "@", "--oneline"]);
    assert!(!out.status.success(), "`@` resolved without a session");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("AGIT_SESSION"),
        "the error must say what `@` needs: {stderr}"
    );
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("first turn"),
        "`@` silently took the pinned branch"
    );

    let out = agit(&home, &work, None, &["scan", "@", "--secrets"]);
    assert!(!out.status.success(), "scan `@` resolved without a session");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("AGIT_SESSION"),
        "scan must say what `@` needs"
    );

    // Same directory, same pin: spelling the branch name out reads as usual.
    let out = agit(&home, &work, None, &["log", BRANCH, "--oneline"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("first turn"));
}
