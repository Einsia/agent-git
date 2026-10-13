#![cfg(feature = "cli")]

//! Privacy export projects the selected snapshot before rendering or writing output.

use agit::domain::{meta, privacy::PrivacyPolicy, repo::Repo, storage, transcript};
use serde_json::json;
use std::process::Command;

#[path = "support/privacy_input_budget.rs"]
mod privacy_input_budget;
#[path = "support/publication_text.rs"]
mod publication_text;
#[path = "support/startup_cache.rs"]
mod startup_cache;

#[test]
fn privacy_export_sanitizes_the_selected_snapshot_and_preserves_output_on_budget_failure() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    startup_cache::seed(&home);
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    repo.git(&["config", "user.name", "Synthetic User"])
        .unwrap();
    repo.git(&["config", "user.email", "synthetic@example.invalid"])
        .unwrap();
    repo.git(&["checkout", "-b", "work"]).unwrap();
    let session = format!("agit-{}", "a".repeat(40));
    let raw = [
        json!({"type":"user", "cwd":workspace, "message":{"role":"user", "content":format!("Read {}", workspace.join("src/main.rs").display())}}),
        json!({"type":"assistant", "message":{"role":"assistant", "content":[{"type":"tool_use", "id":"call", "name":"Read", "input":{"file_path":workspace.join("private.txt")}}]}}),
        json!({"type":"user", "message":{"role":"user", "content":[{"type":"tool_result", "tool_use_id":"call", "content":"EXCLUDED_FILE_BODY"}]}}),
    ].into_iter().chain(publication_text::records(&workspace)).chain([
        json!({"type":"assistant", "message":{"role":"assistant", "content":"Selected reply"}}),
    ]).map(|v| format!("{v}\n")).collect::<String>();
    let log = transcript::wrap_lines(&raw, "claude-code", &session);
    let dictionary = agit::domain::secret_filter::RepositoryDictionary::open(repo.root()).unwrap();
    dictionary
        .block_add(
            "Synthetic private reply",
            zeroize::Zeroizing::new("Selected reply".into()),
            false,
        )
        .unwrap();
    let saved = dictionary
        .protect_envelopes(&log, &agit::domain::secret_filter::Matcher::empty())
        .unwrap()
        .text;
    let saved_view = saved.split_inclusive('\n').next_back().unwrap();
    storage::write_snapshot(repo.root(), &saved, saved_view).unwrap();
    let metadata = meta::Meta::new(
        session,
        "claude-code".into(),
        workspace.display().to_string(),
    );
    meta::write(repo.root(), &metadata).unwrap();
    repo.git(&["add", "."]).unwrap();
    repo.git(&["commit", "-m", "Synthetic session"]).unwrap();
    let before = repo.git(&["rev-parse", "HEAD"]).unwrap();
    repo.git(&["checkout", "-b", "distractor"]).unwrap();
    let decoy = transcript::wrap_lines(
        &format!(
            "{}\n",
            json!({"type":"user","message":{"role":"user","content":"WRONG_SNAPSHOT"}})
        ),
        "claude-code",
        &metadata.session,
    );
    storage::write_snapshot(repo.root(), &decoy, &decoy).unwrap();
    repo.git(&["add", "."]).unwrap();
    repo.git(&["commit", "-m", "Unrelated snapshot"]).unwrap();
    repo.git(&["branch", &before]).unwrap();
    repo.git(&["checkout", "work"]).unwrap();
    PrivacyPolicy {
        workspace: Some(workspace.clone()),
        replacements: vec![publication_text::replacement()],
        ..Default::default()
    }
    .save(&repo)
    .unwrap();

    let run = |args: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear();
        for name in ["PATH", "SystemRoot", "TEMP", "TMP"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command
            .current_dir(&workspace)
            .env("HOME", temp.path())
            .env("USERPROFILE", temp.path())
            .env("AGIT_HOME", &home)
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(args)
            .output()
            .unwrap()
    };
    let candidate_path = workspace.join("src/main.rs");
    let preview = run(&[
        "--json",
        "privacy",
        "preview",
        "--repo",
        "alice/app",
        candidate_path.to_str().unwrap(),
    ]);
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    let preview: serde_json::Value = serde_json::from_slice(&preview.stdout).unwrap();
    assert_eq!(
        preview["result"]["value"]["report"]["candidates"][0]["rule"],
        "repository.include[0]"
    );
    let exported = run(&["export", "alice/app@work", "--privacy"]);
    assert!(
        exported.status.success(),
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );
    let public_log = transcript::wrap_lines(
        &String::from_utf8(exported.stdout.clone()).unwrap(),
        "claude-code",
        &metadata.session,
    );
    publication_text::assert_public(&json!({"session":{"log":public_log}}), &workspace);
    let report = String::from_utf8_lossy(&exported.stderr);
    assert!(report.contains("original records are not included"));
    assert!(report.contains("Source LOG record 2: omitted from public content"));
    assert!(report.contains("tool source is excluded, ambiguous, or unknown"));
    assert!(report.contains("Policy version 1: sha256:"));
    assert!(report.contains("repository.include"));
    assert!(report.contains("exclude_source"));
    assert!(!report.contains("EXCLUDED_FILE_BODY"));
    assert!(!report.contains("private.txt"));
    assert!(!report.contains(workspace.to_str().unwrap()));
    assert!(public_log.contains("<workspace>/src/main.rs"));
    assert!(!public_log.contains("EXCLUDED_FILE_BODY"));
    assert!(!public_log.contains("Selected reply"));
    assert!(!String::from_utf8_lossy(&exported.stdout).contains(workspace.to_str().unwrap()));
    let selected = run(&["export", "alice/app@work", "--privacy", "--view-only"]);
    assert!(
        selected.status.success(),
        "{}",
        String::from_utf8_lossy(&selected.stderr)
    );
    let selected = String::from_utf8(selected.stdout).unwrap();
    assert!(selected.contains("AGIT_SECRET_V1"));
    assert!(!selected.contains("Read"));
    assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), before);
    privacy_input_budget::commit_oversized_event(&repo);
    let output = temp.path().join("export.json");
    std::fs::write(&output, "KEEP_EXISTING_OUTPUT").unwrap();
    let refused = run(&[
        "export",
        "alice/app@work",
        "--privacy",
        "--out",
        output.to_str().unwrap(),
    ]);
    assert!(!refused.status.success());
    let error = String::from_utf8_lossy(&refused.stderr);
    assert!(
        error.contains("privacy preparation input budget"),
        "{error}"
    );
    assert!(!error.contains("invalid envelope"), "{error}");
    assert_eq!(
        std::fs::read_to_string(output).unwrap(),
        "KEEP_EXISTING_OUTPUT"
    );
}
