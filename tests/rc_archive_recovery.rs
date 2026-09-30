#![cfg(all(unix, feature = "cli"))]

use sha2::{Digest, Sha256};
use std::{
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

#[path = "support/startup_cache.rs"]
mod startup_cache;

fn command(root: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
    command
        .args(args)
        .current_dir(root.join("workspace"))
        .env("AGIT_HOME", root.join("agit-home"))
        .env("HOME", root.join("runtime-home"))
        .env("AGIT_HUB_URL", "http://127.0.0.1:9")
        .env("AGIT_SECRETS_KEYSTORE", "file")
        .env("AGIT_TELEMETRY", "off")
        .env("CI", "1")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("AGIT_SESSION")
        .env_remove("AGIT_RC");
    command
}

struct Stop<'a>(&'a Path);
impl Drop for Stop<'_> {
    fn drop(&mut self) {
        let _ = command(self.0, &["rc", "local", "stop"]).output();
    }
}

/// A fresh daemon recovers the standard archive without a viewer, prompt, or runtime launch.
/// Disabled publication retains the durable job across another daemon restart.
#[test]
fn daemon_restart_recovers_a_completed_turn_without_resuming_the_model() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let cwd = root.join("workspace");
    let home = root.join("agit-home");
    let rc = home.join("desktop-rc");
    let runtime_home = root.join("runtime-home");
    for path in [&cwd, &rc, &runtime_home] {
        std::fs::create_dir_all(path).unwrap();
    }
    startup_cache::seed(&home);
    let machine = "00000000-0000-0000-0000-000000000001";
    let repository_id = "00000000-0000-0000-0000-000000000002";
    let native = "00000000-0000-0000-0000-000000000003";
    let branch = "conversation";
    let slug = "desktop-fixture/project";
    let lineage = format!("{slug}@{branch}");
    let repo = home.join("repos").join(slug);
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(rc.join("identity.json"), serde_json::json!({
        "machine_fingerprint":machine,"display_name":"fixture","created_at":"2026-01-01T00:00:00Z"
    }).to_string()).unwrap();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    git(&["init", "-q"]);
    git(&["config", "commit.gpgsign", "false"]);
    git(&["config", "agit.desktopIdentity", repository_id]);
    git(&[
        "config",
        "agit.desktopAuthority",
        &format!("local:{machine}"),
    ]);
    git(&["config", "agit.autoPush", "false"]);
    let transcript_dir = runtime_home
        .join(".claude/projects")
        .join(agit::adapter::claude_code::slug_for(&cwd));
    std::fs::create_dir_all(&transcript_dir).unwrap();
    let transcript = transcript_dir.join(format!("{native}.jsonl"));
    let records = [
        serde_json::json!({"type":"user","sessionId":native,"uuid":"u","cwd":cwd,"message":{"role":"user","content":"inspect the project"}}),
        serde_json::json!({"type":"assistant","sessionId":native,"uuid":"a","parentUuid":"u","cwd":cwd,"message":{"role":"assistant","content":[{"type":"tool_use","id":"c","name":"Bash","input":{"command":"printf ok"}}]}}),
        serde_json::json!({"type":"user","sessionId":native,"uuid":"r","parentUuid":"a","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"c","content":"ok"}]}}),
        serde_json::json!({"type":"assistant","sessionId":native,"uuid":"f","parentUuid":"r","message":{"role":"assistant","content":[{"type":"text","text":"inspection completed"}]}}),
    ];
    let text = records
        .iter()
        .map(|record| format!("{record}\n"))
        .collect::<String>();
    std::fs::write(&transcript, &text).unwrap();
    let landed = command(
        &root,
        &[
            "rc",
            "land",
            "--local-owner",
            "--slug",
            slug,
            "--agent-id",
            repository_id,
            "--branch",
            branch,
            "--runtime",
            "claude-code",
            "--session",
            native,
            "--cwd",
            cwd.to_str().unwrap(),
        ],
    )
    .output()
    .unwrap();
    assert!(
        landed.status.success(),
        "landing: {}",
        String::from_utf8_lossy(&landed.stderr)
    );
    let capture = serde_json::json!({"kind":"device_local","agent_id":repository_id});
    let job = serde_json::json!({
        "version":1,"logical":"logical-fixture","native":native,"runtime":"claude-code","native_source":null,
        "cwd":cwd,"lineage":lineage,"repository_id":repository_id,"capture":capture,"turn_id":"turn-one",
        "transcript":transcript,"prefix_bytes":text.len(),"prefix_hash":format!("{:x}",Sha256::digest(text.as_bytes())),
        "required_bytes":text.len(),"archive_handoff":null
    });
    let coordinates = serde_json::json!([
        "logical-fixture",
        "claude-code",
        null,
        native,
        lineage,
        repository_id,
        "turn-one"
    ]);
    let filename = format!(
        "{:x}.json",
        Sha256::digest(serde_json::to_vec(&coordinates).unwrap())
    );
    let jobs = rc.join("pending-archives");
    std::fs::create_dir_all(&jobs).unwrap();
    let job_path = jobs.join(filename);
    std::fs::write(&job_path, job.to_string()).unwrap();
    std::fs::write(rc.join("sessions.json"), serde_json::json!({
        "captures":{"logical-fixture":capture},"sessions":{"logical-fixture":{
            "runtime":"claude-code","thread_id":native,"cwd":cwd,"workspace_id":"local-owner",
            "agit_session":lineage,"expected_agent_id":repository_id,"prior_threads":[],"ever_dangerous":false
        }}
    }).to_string()).unwrap();
    let before = git(&["rev-parse", &format!("refs/heads/{branch}")]);
    let _stop = Stop(&root);
    for pass in 0..2 {
        let started = command(&root, &["rc", "local", "start", "--detach"])
            .output()
            .unwrap();
        assert!(
            started.status.success(),
            "daemon start: {}",
            String::from_utf8_lossy(&started.stderr)
        );
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let head = git(&["rev-parse", &format!("refs/heads/{branch}")]);
            let attempts = std::fs::read_dir(&rc)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry.file_name().to_string_lossy().starts_with("agitd-")
                        && entry.path().extension().is_some_and(|ext| ext == "log")
                })
                .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
                .map(|text| {
                    text.matches("completed turn archive remains pending (logical-fixture)")
                        .count()
                })
                .sum::<usize>();
            if head != before && attempts > pass {
                let (log, view) = agit::domain::storage::materialize_pair_at(&repo, &head).unwrap();
                assert!(
                    log.contains("inspection completed")
                        && view.contains("printf ok")
                        && view.contains("tool_result")
                );
                assert!(job_path.exists(), "unpublished work must remain durable");
                assert_eq!(
                    std::fs::read_to_string(&transcript).unwrap(),
                    text,
                    "recovery cannot append model output"
                );
                let status = command(&root, &["rc", "status", "--json"])
                    .output()
                    .unwrap();
                assert!(status.status.success());
                let status = String::from_utf8(status.stdout).unwrap();
                assert!(
                    !status.contains("logical-fixture"),
                    "recovery must not start a live model: {status}"
                );
                if pass == 1 {
                    assert_eq!(
                        git(&["rev-list", "--count", &format!("refs/heads/{branch}")]),
                        "3"
                    );
                }
                break;
            }
            assert!(
                Instant::now() < deadline,
                "daemon failed to recover the completed prefix"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(
            command(&root, &["rc", "local", "stop"])
                .output()
                .unwrap()
                .status
                .success()
        );
    }
}
