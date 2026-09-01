use agit::domain::repo::Repo;
use sha2::{Digest, Sha256};
use std::{fs, process::Command};

#[test]
fn slash_ref_works_with_a_bound_but_unpinned_workspace() {
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
        // This test is about the directory binding; the terminal running the tests may itself be
        // inside an agit session, and an inherited AGIT_SESSION resolves the repo somewhere else.
        .env_remove("AGIT_SESSION")
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
            .env_remove("AGIT_SESSION")
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
    // the local branch of the bound repository.
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
