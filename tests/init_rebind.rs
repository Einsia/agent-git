//! When the directory is already bound to another repo, `agit init` refuses before it touches
//! the disk: no half-built repository may be left behind for the next attempt to hit
//! "already exists". `--rebind` is the only way to rebind explicitly.

use std::path::Path;
use std::{fs, process::Command};

fn bind(home: &Path, work: &Path, repo: &str) {
    use sha2::{Digest, Sha256};
    let canonical = work.canonicalize().unwrap();
    let id = &hex::encode(Sha256::digest(canonical.to_string_lossy().as_bytes()))[..16];
    let dir = home.join("workspaces");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(format!("{id}.json")),
        serde_json::to_vec(&serde_json::json!({ "dir": canonical, "repo": repo })).unwrap(),
    )
    .unwrap();
}

fn agit(home: &Path, work: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_agit"))
        .args(args)
        .current_dir(work)
        .env("AGIT_HOME", home)
        .env_remove("AGIT_SESSION")
        .output()
        .unwrap()
}

#[test]
fn init_refuses_before_creating_anything_when_the_directory_is_bound_elsewhere() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let work = tmp.path().join("work");
    fs::create_dir_all(&work).unwrap();
    bind(&home, &work, "me/other");

    let out = agit(&home, &work, &["init", "qa"]);
    assert!(!out.status.success(), "init must refuse");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("me/other") && stderr.contains("--rebind"),
        "{stderr}"
    );
    assert!(
        !home.join("repos/local/qa").exists(),
        "a refused init leaves no repository behind"
    );

    let out = agit(&home, &work, &["init", "qa", "--rebind"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(home.join("repos/local/qa/.git").exists());
    let ws = fs::read_dir(home.join("workspaces"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    let text = fs::read_to_string(ws.path()).unwrap();
    assert!(text.contains("local/qa"), "{text}");
}
