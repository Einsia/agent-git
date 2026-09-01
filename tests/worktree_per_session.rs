//! One worktree per session branch: the main checkout stays on the main file line, and each
//! branch checks out somewhere of its own.
//!
//! This pins three things from the CLI entry point — `repo path <repo>@<branch>` gives (and
//! creates on demand) that branch's worktree; `branch rm` deletes a branch that has a worktree
//! (the branch is no longer held by the main checkout); and the main checkout still sits on main
//! through all of it. An implementation that still serves a branch by switching the main
//! checkout hits git's "used by worktree" on the second one.

use agit::domain::meta::{self, Meta};
use agit::domain::repo::Repo;
use std::path::{Path, PathBuf};
use std::{fs, process::Command};

const REPO: &str = "drh/qa";

fn fixture(tmp: &Path) -> (PathBuf, Repo) {
    let home = tmp.join("home");
    let repo = Repo::init(&home.join("repos").join(REPO)).unwrap();
    repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
    meta::write(repo.root(), &Meta::new_file_line()).unwrap();
    fs::write(repo.root().join("AGENTS.md"), "# team\n").unwrap();
    repo.add_all().unwrap();
    repo.commit("agit: init").unwrap();
    repo.git(&["branch", "s1", "main"]).unwrap();
    (home, repo)
}

fn agit(home: &Path, work: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_agit"))
        .args(args)
        .current_dir(work)
        .env("AGIT_HOME", home)
        .env("AGIT_SESSION", format!("{REPO}@s1"))
        .output()
        .unwrap()
}

fn stdout(out: &std::process::Output) -> String {
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn a_session_branch_lives_in_its_own_worktree_and_can_be_deleted() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, repo) = fixture(tmp.path());
    let work = tmp.path().join("work");
    fs::create_dir_all(&work).unwrap();

    // The main checkout itself.
    let primary = stdout(&agit(&home, &work, &["repo", "path", REPO]));
    assert_eq!(Path::new(&primary), repo.root());

    // The branch worktree: created on demand, under worktrees/<owner>/<name>/<branch>.
    let wt = stdout(&agit(
        &home,
        &work,
        &["repo", "path", &format!("{REPO}@s1")],
    ));
    let wt = PathBuf::from(wt);
    assert_eq!(
        wt,
        home.canonicalize()
            .unwrap()
            .join("worktrees")
            .join(REPO)
            .join("s1")
    );
    assert!(
        wt.join(".git").is_file(),
        "a linked worktree carries a .git file"
    );
    assert_eq!(Repo::at(&wt).current_branch().as_deref(), Some("s1"));
    assert_eq!(repo.current_branch().as_deref(), Some("main"));

    // `@` is the current session's branch: the same directory.
    let same = stdout(&agit(&home, &work, &["repo", "path", "@"]));
    assert_eq!(PathBuf::from(same), wt);

    // A branch with a worktree deletes just the same; the directory goes with it and the main
    // checkout does not move.
    stdout(&agit(&home, &work, &["branch", "rm", "s1", "--force"]));
    assert!(!repo.has_ref("refs/heads/s1"));
    assert!(!wt.exists());
    assert_eq!(repo.current_branch().as_deref(), Some("main"));
}

#[test]
fn a_primary_parked_on_a_session_branch_is_moved_back_to_main_on_first_use() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, repo) = fixture(tmp.path());
    let work = tmp.path().join("work");
    fs::create_dir_all(&work).unwrap();
    repo.switch("s1").unwrap();

    let wt = PathBuf::from(stdout(&agit(
        &home,
        &work,
        &["repo", "path", &format!("{REPO}@s1")],
    )));
    assert!(wt.join(".git").is_file());
    assert_eq!(repo.current_branch().as_deref(), Some("main"));
}
