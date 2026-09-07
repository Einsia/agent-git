//! A file-line reference describes its own tree and history without requiring a conversation.

use agit::domain::{meta, repo::Repo};
use std::{path::Path, process::Command};

fn show(home: &Path, target: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_agit"))
        .args(["show", target])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("AGIT_HOME", home)
        .env("CI", "1")
        .env("NO_COLOR", "1")
        .output()
        .unwrap()
}

#[test]
fn file_line_show_uses_the_selected_tree_and_history() {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path();
    let repo = Repo::init(&home.join("repos/me/files")).unwrap();
    repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    std::fs::write(repo.root().join("README.md"), "shared instructions\n").unwrap();
    std::fs::create_dir(repo.root().join("memory")).unwrap();
    std::fs::write(repo.root().join("memory/rules.md"), "shared rules\n").unwrap();
    repo.add_all().unwrap();
    repo.commit("record shared instructions").unwrap();
    std::fs::write(repo.root().join("README.md"), "updated instructions\n").unwrap();
    repo.add_all().unwrap();
    repo.commit("update shared instructions").unwrap();

    repo.git(&["checkout", "-b", "session"]).unwrap();
    meta::write(
        repo.root(),
        &meta::Meta::new_session_line("codex".into(), "/work".into()),
    )
    .unwrap();
    std::fs::write(repo.root().join("session-only.txt"), "other branch\n").unwrap();
    repo.add_all().unwrap();
    repo.commit("unrelated session history").unwrap();

    let output = show(home, "me/files@main");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("file line"));
    assert!(text.contains("README.md") && text.contains("memory"));
    assert!(text.contains("update shared instructions"));
    assert!(text.contains("record shared instructions"));
    assert!(
        text.ends_with('\n'),
        "the final history record must terminate"
    );
    assert!(!text.contains("session-only.txt"));
    assert!(!text.contains("unrelated session history"));
    assert!(!text.contains("cannot read this point's VIEW"));

    let raw = show(home, "me/files@main:README.md");
    assert!(raw.status.success());
    assert_eq!(raw.stdout, b"updated instructions\n");
    assert_eq!(repo.current_branch().as_deref(), Some("session"));
    assert!(repo.git(&["status", "--porcelain"]).unwrap().is_empty());
}
