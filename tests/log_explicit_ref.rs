use agit::domain::repo::Repo;
use sha2::{Digest, Sha256};
use std::{fs, process::Command};

#[test]
fn invalid_since_is_a_usage_error_before_repository_resolution() {
    let tmp = tempfile::tempdir().unwrap();
    for duration in [
        "",
        "h",
        "1.5d",
        "24hours",
        "4294967296d",
        "\u{5929}",
        "1\u{5929}",
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_agit"))
            .args(["log", "missing/repo@main", "--since", duration])
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", tmp.path())
            .env("AGIT_HOME", tmp.path().join("agit"))
            .env("CI", "1")
            .current_dir(tmp.path())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(agit::ExitCode::Usage.as_i32()));
        assert!(output.stdout.is_empty());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("invalid value"), "{error}");
        assert!(error.contains("--since"), "{error}");
        assert!(!tmp.path().join("agit").exists());
    }
}

#[test]
fn slash_ref_uses_the_explicit_session_repository() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let work = tmp.path().join("work");
    fs::create_dir_all(&work).unwrap();

    let repo = Repo::init(&home.join("repos/drh/qa")).unwrap();
    fs::write(repo.root().join("README.md"), "fixture").unwrap();
    repo.add_all().unwrap();
    repo.commit("first turn").unwrap();
    repo.git(&["branch", "-m", "topic/first-session"]).unwrap();

    let canonical = work.canonicalize().unwrap();
    let id = &hex::encode(Sha256::digest(canonical.to_string_lossy().as_bytes()))[..16];
    let workspace_dir = home.join("workspaces");
    fs::create_dir_all(&workspace_dir).unwrap();
    fs::write(
        workspace_dir.join(format!("{id}.json")),
        serde_json::to_vec(&serde_json::json!({
            "dir": canonical,
            "repo": "drh/qa"
        }))
        .unwrap(),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_agit"))
        .args(["log", "topic/first-session", "--oneline"])
        .current_dir(&work)
        .env("AGIT_HOME", &home)
        .env("AGIT_SESSION", "drh/qa@topic/first-session")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("first turn"));
}

#[test]
fn branch_list_views_keep_repository_semantics_over_a_same_name_local_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let work = tmp.path().join("work");
    fs::create_dir_all(&work).unwrap();

    let repo = Repo::init(&home.join("repos/drh/qa")).unwrap();
    fs::write(repo.root().join("README.md"), "fixture").unwrap();
    repo.add_all().unwrap();
    repo.commit("first turn").unwrap();
    repo.git(&["branch", "-m", "alice/ci-notes"]).unwrap();

    let canonical = work.canonicalize().unwrap();
    let id = &hex::encode(Sha256::digest(canonical.to_string_lossy().as_bytes()))[..16];
    let workspace_dir = home.join("workspaces");
    fs::create_dir_all(&workspace_dir).unwrap();
    fs::write(
        workspace_dir.join(format!("{id}.json")),
        serde_json::to_vec(&serde_json::json!({
            "dir": canonical,
            "repo": "drh/qa"
        }))
        .unwrap(),
    )
    .unwrap();

    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_agit"))
            .args(args)
            .current_dir(&work)
            .env("AGIT_HOME", &home)
            .env("AGIT_SESSION", "drh/qa@alice/ci-notes")
            .output()
            .unwrap()
    };

    // The plain positional reads the existing local branch.
    let plain = run(&["log", "alice/ci-notes", "--oneline"]);
    assert!(
        plain.status.success(),
        "{}",
        String::from_utf8_lossy(&plain.stderr)
    );
    assert!(String::from_utf8_lossy(&plain.stdout).contains("first turn"));

    // The explicit branch-list views name a repository outright, so the same
    // string must route to the (absent) repository alice/ci-notes — never to
    // the local branch of the explicitly selected repository.
    for flag in ["--branches", "--graph"] {
        let view = run(&["log", "alice/ci-notes", flag]);
        let stdout = String::from_utf8_lossy(&view.stdout);
        let stderr = String::from_utf8_lossy(&view.stderr);
        assert!(
            !stdout.contains("first turn"),
            "{flag} must not fall back to the local branch: {stdout}"
        );
        assert!(
            stderr.contains("doesn’t exist locally") || stderr.contains("doesn't exist locally"),
            "{flag} must report the repository, got: {stderr}"
        );
    }
}
