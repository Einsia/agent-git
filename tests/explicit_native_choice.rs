//! Runtime transcript discovery can list candidates but cannot choose one for adoption.

use std::{fs, process::Command};

const SESSION: &str = "abababab-0000-4000-8000-000000000001";

#[test]
fn import_at_cannot_infer_a_native_runtime_session() {
    refuses_implicit_import(Some("@"));
}

#[test]
fn import_cannot_adopt_the_only_cwd_candidate_without_a_choice() {
    refuses_implicit_import(None);
}

fn refuses_implicit_import(target: Option<&str>) {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    let home = tmp.path().join("home");
    let store = tmp.path().join("agit");
    fs::create_dir_all(&work).unwrap();
    let work = work.canonicalize().unwrap();
    let project = home
        .join(".claude/projects")
        .join(agit::adapter::claude_code::slug_for(&work));
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join(format!("{SESSION}.jsonl")),
        serde_json::json!({
            "type":"user", "sessionId":SESSION, "cwd":work,
            "message":{"role":"user","content":"Synthetic session awaiting explicit selection"}
        })
        .to_string(),
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
    command
        .current_dir(&work)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("AGIT_HOME", &store)
        .env("CLAUDE_CODE_SESSION_ID", SESSION)
        .args(["--no-tui", "import", "--link-only"]);
    if let Some(target) = target {
        command.arg(target);
    }
    let output = command.output().unwrap();
    assert!(
        !output.status.success(),
        "adoption inferred {target:?}: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !store
            .join("store/claude-code")
            .join(format!("{SESSION}.json"))
            .exists()
    );
}
