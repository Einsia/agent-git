#![cfg(feature = "cli")]

//! A share link's key is replaced on each path that protects a conversation: saving, the default
//! export and the publication projection.

use agit::domain::privacy::{detector::placeholder, projector::Mode, service::protect_for_commit};
use agit::domain::{meta, repo::Repo, storage, transcript};
use serde_json::json;
use std::process::Command;

const KEY: &str = "JjGmd6ZAB0ytnDA9d2YRAK3CvCDyN9dPDVLlYolzU8s";
const ADDRESS: &str = "http://127.0.0.1/share/fixture";
const KEYLESS: &str = "http://127.0.0.1/share/fixturb";

/// The key `agit share` printed, in its plain output, in its `--json` output and in the agent's
/// reply, is gone from what is saved, from the default export and from the publication
/// projection, each holding a placeholder where it stood, with the address in front of it and a
/// keyless address still there. A protection that missed one of these paths, or that recognised
/// the key only in one of the three places, leaves it readable to whoever reads that copy; one
/// that removed whole links would take the keyless address too.
#[test]
fn a_share_links_key_is_replaced_when_saved_exported_and_published() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    // SAFETY: this binary holds one test, so no other thread reads the environment; the privacy
    // worker it starts inherits these variables.
    unsafe {
        std::env::set_var("AGIT_HOME", &home);
        std::env::set_var("HOME", temp.path());
        std::env::set_var("AGIT_HUB_URL", "http://127.0.0.1:1");
        // A test binary can't host the worker; the agit executable does.
        std::env::set_var("AGIT_TEST_PRIVACY_WORKER", env!("CARGO_BIN_EXE_agit"));
    }
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    repo.git(&["config", "user.name", "Synthetic User"])
        .unwrap();
    repo.git(&["config", "user.email", "synthetic@example.invalid"])
        .unwrap();
    repo.git(&["checkout", "-b", "work"]).unwrap();

    let link = format!("{ADDRESS}#k={KEY}");
    let plain = format!(
        "✓ read-only copy created\n\n  {link}\n\n  source      alice/app@talk\n\n→ revoke: agit share rm fixture\n"
    );
    let as_json = json!({"link": link, "slug": "fixture", "encrypted": true}).to_string();
    let listed = format!("slug     link\nfixturb  {KEYLESS}\n");
    let raw = [
        json!({"type":"user","cwd":workspace,"message":{"role":"user","content":"share this conversation"}}),
        json!({"type":"assistant","message":{"role":"assistant","content":[
            {"type":"tool_use","id":"plain","name":"Bash","input":{"command":"agit share alice/app@talk -y"}},
            {"type":"tool_use","id":"json","name":"Bash","input":{"command":"agit --json share alice/app@talk -y"}},
            {"type":"tool_use","id":"listed","name":"Bash","input":{"command":"agit share list"}}
        ]}}),
        json!({"type":"user","message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"plain","content":plain},
            {"type":"tool_result","tool_use_id":"json","content":as_json},
            {"type":"tool_result","tool_use_id":"listed","content":listed}
        ]}}),
        json!({"type":"assistant","message":{"role":"assistant","content":format!("Here is the link: {link}")}}),
    ]
    .map(|v| format!("{v}\n"))
    .concat();
    let protected = |stage: &str, text: &str| {
        assert!(text.contains("share this conversation"), "{stage}: {text}");
        assert!(!text.contains(KEY), "{stage}: {text}");
        let fragment = format!("{ADDRESS}#k=");
        let kept: Vec<_> = text.match_indices(&fragment).collect();
        assert_eq!(kept.len(), 3, "{stage}: {text}");
        for (at, _) in kept {
            let after: String = text[at + fragment.len()..].chars().take(160).collect();
            assert!(
                !after.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                    && placeholder::contains_token(&after),
                "{stage}: {after}"
            );
        }
        assert!(text.contains(KEYLESS), "{stage}: {text}");
    };

    let saved = protect_for_commit(Some(repo.root()), &raw, Mode::ProtectJsonl).unwrap();
    protected("save", &saved);

    let session = format!("agit-{}", "a".repeat(40));
    // The exports read a snapshot that still holds the key, so each shows its own replacement
    // rather than the save's.
    let log = transcript::wrap_lines(&raw, "claude-code", &session);
    storage::write_snapshot(repo.root(), &log, &log).unwrap();
    meta::write(
        repo.root(),
        &meta::Meta::new(
            session,
            "claude-code".into(),
            workspace.display().to_string(),
        ),
    )
    .unwrap();
    repo.git(&["add", "."]).unwrap();
    repo.git(&["commit", "-m", "Synthetic session"]).unwrap();

    let run = |args: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear();
        for name in ["PATH", "SystemRoot", "TEMP", "TMP"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        let output = command
            .current_dir(&workspace)
            .env("HOME", temp.path())
            .env("USERPROFILE", temp.path())
            .env("AGIT_HOME", &home)
            .env("AGIT_HUB_URL", "http://127.0.0.1:1")
            .env("AGIT_SKIP_SETUP", "1")
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    protected("default export", &run(&["export", "alice/app@work"]));
    protected(
        "publication projection",
        &run(&["export", "alice/app@work", "--privacy"]),
    );
}
