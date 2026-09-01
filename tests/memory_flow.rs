//! Memory's landing places, walked from the CLI entry point: `memory sync` collects new files in
//! the runtime directory into the session branch and places the branch's files into a mirror
//! subdirectory of the runtime directory; `memory status` names what is not in main yet;
//! `distill -y` carries them into main, after which status reports everything in sync. An
//! implementation that treats the runtime directory as the branch itself (rather than a live
//! copy) fails on "no local file moves"; one that writes main on its own fails on "main does not
//! have it before distill".

use agit::domain::meta::{self, Meta};
use agit::domain::repo::Repo;
use std::path::{Path, PathBuf};
use std::{fs, process::Command};

const REPO: &str = "drh/qa";

/// An Agent repo: `memory/team.md` on main, session line `s1` recording the claude-code runtime
/// and the working directory `work`; plus a fake Claude memory directory (located through
/// `CLAUDE_CONFIG_DIR`).
fn fixture(tmp: &Path) -> (PathBuf, Repo, PathBuf, PathBuf) {
    let home = tmp.join("home");
    let work = tmp.join("work");
    fs::create_dir_all(&work).unwrap();
    let repo = Repo::init(&home.join("repos").join(REPO)).unwrap();
    repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
    meta::write(repo.root(), &Meta::new_file_line()).unwrap();
    fs::create_dir_all(repo.root().join("memory")).unwrap();
    fs::write(
        repo.root().join("memory/team.md"),
        "---\nname: team\ndescription: shared\n---\nuid, not user_id\n",
    )
    .unwrap();
    repo.add_all().unwrap();
    repo.commit("agit: init").unwrap();

    let mut snap = Meta::new_session_line(
        "claude-code".into(),
        work.canonicalize().unwrap().to_string_lossy().into_owned(),
    );
    snap.session = format!("{}{}", meta::ID_PREFIX, "c".repeat(meta::ID_HEX_LEN));
    meta::write(repo.root(), &snap).unwrap();
    repo.git(&["switch", "-q", "-c", "s1"]).unwrap();
    repo.add_all().unwrap();
    repo.commit("agit: new session s1").unwrap();
    repo.switch("main").unwrap();
    meta::write(repo.root(), &Meta::new_file_line()).unwrap();

    let claude = tmp.join("claude-config");
    let project = agit::infra::runtime_memory::encode_project(&work.canonicalize().unwrap());
    let mem = claude.join("projects").join(project).join("memory");
    fs::create_dir_all(&mem).unwrap();
    (home, repo, claude, mem)
}

fn agit(home: &Path, claude: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_agit"))
        .args(args)
        .current_dir(home)
        .env("AGIT_HOME", home)
        .env("CLAUDE_CONFIG_DIR", claude)
        .env_remove("AGIT_SESSION")
        .output()
        .unwrap()
}

fn ok(out: &std::process::Output) -> String {
    assert!(
        out.status.success(),
        "stderr: {}\nstdout: {}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[test]
fn memory_flows_runtime_to_branch_and_branch_to_main_only_on_distill() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, repo, claude, mem) = fixture(tmp.path());
    let into = format!("{REPO}@s1");
    fs::write(mem.join("mine.md"), "---\nname: mine\n---\nlocal fact\n").unwrap();
    fs::write(
        mem.join("MEMORY.md"),
        "# Memory Index\n\n- [mine](mine.md)\n",
    )
    .unwrap();

    // sync: the new local file enters the branch; the branch's team.md (inherited from main)
    // enters the mirror subdirectory and is indexed.
    ok(&agit(&home, &claude, &["memory", "sync", "--into", &into]));
    assert!(repo.show("refs/heads/s1", "memory/mine.md").is_some());
    assert!(
        repo.show("refs/heads/main", "memory/mine.md").is_none(),
        "main never moves by itself"
    );
    assert!(mem.join("agit/drh/qa/s1/team.md").is_file());
    let index = fs::read_to_string(mem.join("MEMORY.md")).unwrap();
    assert!(
        index.contains("- [mine](mine.md)"),
        "local lines untouched: {index}"
    );
    assert!(index.contains("(agit/drh/qa/s1/team.md)"), "{index}");
    assert_eq!(
        fs::read_to_string(mem.join("mine.md")).unwrap(),
        "---\nname: mine\n---\nlocal fact\n"
    );

    // status: mine.md is not in main yet.
    let status = ok(&agit(
        &home,
        &claude,
        &["memory", "status", "--into", &into],
    ));
    assert!(status.contains("mine.md"), "{status}");
    assert!(status.contains("not in main"), "{status}");
    assert!(
        status.contains("1 change on `s1` not yet in main"),
        "{status}"
    );

    // distill -y: into main, after which everything is in sync.
    ok(&agit(&home, &claude, &["distill", "-y", "--into", &into]));
    assert_eq!(
        repo.git_bytes(&["show", "refs/heads/main:memory/mine.md"])
            .unwrap(),
        b"---\nname: mine\n---\nlocal fact\n"
    );
    assert!(
        meta::read_at_ref(&repo, "refs/heads/main")
            .unwrap()
            .is_file_line()
    );
    let status = ok(&agit(
        &home,
        &claude,
        &["memory", "status", "--into", &into],
    ));
    assert!(status.contains("everything in sync"), "{status}");
}

#[test]
fn a_branch_without_a_runtime_memory_dir_still_reports_and_distills() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, repo, claude, _mem) = fixture(tmp.path());
    // Turn s1 into a codex session: no per-project memory directory on disk.
    let refname = "refs/heads/s1";
    let mut snap = meta::read_at_ref(&repo, refname).unwrap();
    snap.runtime = "codex".into();
    let tree = agit::commands::plumbing::tree_apply_owned(
        &repo,
        refname,
        vec![(
            meta::FILE.into(),
            Some(meta::to_text(&snap).unwrap().into_bytes()),
        )],
    )
    .unwrap();
    let head = repo.git(&["rev-parse", refname]).unwrap();
    let c = agit::commands::plumbing::commit_tree(&repo, &tree, &[head.trim()], "codex").unwrap();
    repo.git(&["update-ref", refname, &c]).unwrap();

    let into = format!("{REPO}@s1");
    let out = ok(&agit(&home, &claude, &["memory", "sync", "--into", &into]));
    assert!(out.contains("no per-project memory directory"), "{out}");
    let status = ok(&agit(
        &home,
        &claude,
        &["memory", "status", "--into", &into],
    ));
    assert!(status.contains("everything in sync"), "{status}");
}
