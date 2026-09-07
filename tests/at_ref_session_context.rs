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
    for (name, _) in agit::infra::runtime_session::ENV_SESSIONS {
        cmd.env_remove(name);
    }
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

/// An absent selected branch cannot expose the history of a different checked-out branch.
#[test]
fn missing_context_branch_does_not_fall_back_to_head() {
    let tmp = tempfile::tempdir().unwrap();
    let home = fixture(tmp.path());
    let work = tmp.path().join("work");
    fs::create_dir_all(&work).unwrap();
    let repo = Repo::open(home.join("repos").join(REPO)).unwrap();
    let head = repo.git(&["rev-parse", "HEAD"]).unwrap();
    let missing = format!("{REPO}@absent");

    for args in [&["log", "--oneline"][..], &["log", "@", "--oneline"][..]] {
        let output = agit(&home, &work, Some(&missing), args);
        assert_eq!(
            output.status.code(),
            Some(agit::ExitCode::Ref.as_i32()),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("absent"));
    }
    assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), head);
    assert_eq!(repo.current_branch().as_deref(), Some(BRANCH));
    assert!(!repo.has_ref("refs/heads/absent"));

    let target = format!("{REPO}@{BRANCH}");
    let explicit = agit(&home, &work, Some(&missing), &["log", &target, "--oneline"]);
    assert!(explicit.status.success());
    assert!(String::from_utf8_lossy(&explicit.stdout).contains("first turn"));
}

/// A selected session cannot be replaced by a tag or a remote-tracking branch.
#[test]
fn at_requires_a_local_branch_even_when_other_names_resolve() {
    let tmp = tempfile::tempdir().unwrap();
    let home = fixture(tmp.path());
    let work = tmp.path().join("work");
    fs::create_dir_all(&work).unwrap();
    let repo = Repo::open(home.join("repos").join(REPO)).unwrap();
    repo.git(&["tag", "tag-only", "HEAD"]).unwrap();
    repo.git(&["update-ref", "refs/remotes/origin/remote-only", "HEAD"])
        .unwrap();
    for branch in ["tag-only", "remote-only"] {
        let session = format!("{REPO}@{branch}");
        for args in [
            vec!["log", "@", "--oneline"],
            vec!["show", "@:README.md"],
            vec!["view", "@", "--json"],
            vec!["view", "--json"],
            vec!["export", "@"],
            vec!["scan", "@", "--secrets"],
            vec!["fork", "@", "-b", "must-not-exist"],
        ] {
            let output = agit(&home, &work, Some(&session), &args);
            assert_eq!(
                output.status.code(),
                Some(agit::ExitCode::Ref.as_i32()),
                "{args:?}: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let text = String::from_utf8_lossy(&output.stdout);
            assert!(
                !text.contains("first turn") && !text.contains("fixture"),
                "{args:?}: {text}"
            );
        }
        assert!(!repo.has_ref("refs/heads/must-not-exist"));
    }
}

/// An explicit session branch stays selected when other reference namespaces collide.
#[test]
fn at_keeps_its_branch_when_tags_and_object_names_collide() {
    let tmp = tempfile::tempdir().unwrap();
    let home = fixture(tmp.path());
    let work = tmp.path().join("work");
    fs::create_dir_all(&work).unwrap();
    let repo = Repo::open(home.join("repos").join(REPO)).unwrap();
    let first = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    fs::write(repo.root().join("README.md"), "selected branch").unwrap();
    repo.add_all().unwrap();
    repo.commit("selected history").unwrap();
    let selected = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    repo.git(&["tag", BRANCH, &first]).unwrap();
    repo.git(&["branch", &first, &selected]).unwrap();
    repo.git(&["tag", &first, &first]).unwrap();
    repo.git(&["branch", &selected, &format!("refs/tags/{BRANCH}")])
        .unwrap();

    for branch in [BRANCH, first.as_str()] {
        let session = format!("{REPO}@{branch}");
        let shown = agit(&home, &work, Some(&session), &["show", "@:README.md"]);
        assert!(
            shown.status.success(),
            "{}",
            String::from_utf8_lossy(&shown.stderr)
        );
        assert_eq!(shown.stdout, b"selected branch");
        let log = agit(&home, &work, Some(&session), &["log", "@", "--oneline"]);
        assert!(
            log.status.success(),
            "{}",
            String::from_utf8_lossy(&log.stderr)
        );
        assert!(String::from_utf8_lossy(&log.stdout).contains("selected history"));
        let parent = agit(&home, &work, Some(&session), &["log", "@~1", "--oneline"]);
        assert!(
            parent.status.success(),
            "{}",
            String::from_utf8_lossy(&parent.stderr)
        );
        let text = String::from_utf8_lossy(&parent.stdout);
        assert!(text.contains("first turn"));
        assert!(!text.contains("selected history"));
    }
}

#[test]
fn at_never_falls_back_to_the_workspace_pin() {
    let tmp = tempfile::tempdir().unwrap();
    let home = fixture(tmp.path());
    let work = tmp.path().join("work");
    fs::create_dir_all(&work).unwrap();
    pin_workspace(&home, &work);

    // Persisted directory state cannot supply any part of an omitted session target.
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

    // A fully qualified target supplies the repository without consulting directory state.
    let target = format!("{REPO}@{BRANCH}");
    let out = agit(&home, &work, None, &["log", &target, "--oneline"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("first turn"));
}

#[test]
fn ordinary_commands_ignore_legacy_pins_and_adopted_cwd() {
    let tmp = tempfile::tempdir().unwrap();
    let home = fixture(tmp.path());
    let work = tmp.path().join("work");
    fs::create_dir_all(&work).unwrap();
    pin_workspace(&home, &work);
    let links = home.join("store/codex");
    fs::create_dir_all(&links).unwrap();
    fs::write(
        links.join("explicit-context-session.json"),
        serde_json::to_vec(&serde_json::json!({
            "cwd": work,
            "agent": "qa",
            "owner": "drh",
            "branch": BRANCH,
        }))
        .unwrap(),
    )
    .unwrap();

    for args in [
        vec!["log", "--oneline"],
        vec!["branch"],
        vec!["log", BRANCH],
    ] {
        let out = agit(&home, &work, None, &args);
        assert!(!out.status.success(), "directory state selected {args:?}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("AGIT_SESSION"));
    }
}

#[test]
fn a_native_runtime_identity_can_only_veto_an_explicit_environment_target() {
    let tmp = tempfile::tempdir().unwrap();
    let home = fixture(tmp.path());
    let repo = Repo::open(home.join("repos").join(REPO)).unwrap();
    repo.git(&["branch", "another-session", "HEAD"]).unwrap();
    let work = tmp.path().join("work");
    fs::create_dir_all(&work).unwrap();
    let links = home.join("store/codex");
    fs::create_dir_all(&links).unwrap();
    for superseded_by in [None, Some("codex/replacement-session")] {
        fs::write(
            links.join("explicit-context-session.json"),
            serde_json::to_vec(&serde_json::json!({
                "cwd": work,
                "agent": "qa",
                "owner": "drh",
                "branch": BRANCH,
                "superseded_by": superseded_by,
            }))
            .unwrap(),
        )
        .unwrap();
        for injected in [None, Some("drh/qa@another-session")] {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_agit"));
            cmd.args(["log", "--oneline"])
                .current_dir(&work)
                .env("AGIT_HOME", &home)
                .env_remove("AGIT_SESSION");
            for (name, _) in agit::infra::runtime_session::ENV_SESSIONS {
                cmd.env_remove(name);
            }
            cmd.env("CODEX_SESSION_ID", "explicit-context-session");
            if let Some(value) = injected {
                cmd.env("AGIT_SESSION", value);
            }
            let out = cmd.output().unwrap();
            assert!(!out.status.success(), "runtime identity selected a target");
            assert!(!String::from_utf8_lossy(&out.stdout).contains("first turn"));
        }
    }
}

#[test]
fn branch_governance_accepts_an_explicit_repo_without_session_inference() {
    let tmp = tempfile::tempdir().unwrap();
    let home = fixture(tmp.path());
    let work = tmp.path().join("work");
    fs::create_dir_all(&work).unwrap();
    let out = agit(&home, &work, None, &["branch", "--repo", REPO]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains(BRANCH));

    let out = agit(
        &home,
        &work,
        Some("unknown/repo@missing"),
        &["branch", "rename", BRANCH, "renamed", "--repo", REPO],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let repo = Repo::open(home.join("repos").join(REPO)).unwrap();
    assert!(repo.has_ref("refs/heads/renamed"));
    assert!(!repo.has_ref(&format!("refs/heads/{BRANCH}")));
}
