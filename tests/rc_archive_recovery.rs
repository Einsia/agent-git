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
        serde_json::json!({"type":"assistant","sessionId":native,"uuid":"f","parentUuid":"r","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"inspection completed"}]}}),
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

/// Watching retains per-turn collection after viewers disconnect and the daemon restarts.
/// Native input stays outside the daemon, and failed publication retains archive jobs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn observed_native_turns_archive_without_a_viewer_or_model_takeover() {
    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let cwd = root.join("workspace");
    let home = root.join("agit-home");
    let runtime_home = root.join("runtime-home");
    for path in [&cwd, &home, &runtime_home] {
        std::fs::create_dir_all(path).unwrap();
    }
    startup_cache::seed(&home);
    let native = "00000000-0000-0000-0000-000000000013";
    let transcript_dir = runtime_home
        .join(".claude/projects")
        .join(agit::adapter::claude_code::slug_for(&cwd));
    std::fs::create_dir_all(&transcript_dir).unwrap();
    let transcript = transcript_dir.join(format!("{native}.jsonl"));
    let turn = |number: usize| {
        [
            json!({"type":"user","sessionId":native,"uuid":format!("u{number}"),"cwd":cwd,"message":{"role":"user","content":format!("Inspect turn {number}")}}),
            json!({"type":"assistant","sessionId":native,"uuid":format!("a{number}"),"parentUuid":format!("u{number}"),"cwd":cwd,"message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":format!("Observed answer {number}")}]}}),
        ].iter().map(|row| format!("{row}\n")).collect::<String>()
    };
    let first = turn(1);
    std::fs::write(&transcript, &first).unwrap();
    let _stop = Stop(&root);
    let started = command(&root, &["rc", "local", "start", "--detach"])
        .output()
        .unwrap();
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let mut bridge = tokio::process::Command::from(command(&root, &["rc", "local", "bridge"]))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = bridge.stdin.take().unwrap();
    let mut lines = BufReader::new(bridge.stdout.take().unwrap()).lines();
    let mut replies = vec![];
    for (id, method, params) in [
        (
            "bind",
            "project.bind",
            json!({"workspace_id":"local-owner","project_id":"observed-project","local_path":cwd}),
        ),
        (
            "watch",
            "session.watch",
            json!({"workspace_id":"local-owner","session_id":native}),
        ),
    ] {
        input
            .write_all(
                format!(
                    "{}\n",
                    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let row: Value =
                    serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                if row["id"] == id {
                    break row;
                }
            }
        })
        .await
        .unwrap();
        assert!(reply.get("error").is_none(), "{reply}");
        replies.push(reply);
    }
    assert_eq!(replies[1]["result"]["read_only"], true);
    let logical = replies[1]["result"]["archive_session"]["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(replies[1]["result"]["session"]["session_id"], logical);
    drop(input);
    tokio::time::timeout(Duration::from_secs(10), bridge.wait())
        .await
        .unwrap()
        .unwrap();
    let mut initial_head = None;
    for number in 1..=2 {
        if number == 2 {
            assert!(
                command(&root, &["rc", "local", "stop"])
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
            std::fs::write(&transcript, format!("{first}{}", turn(2))).unwrap();
            assert!(
                command(&root, &["rc", "local", "start", "--detach"])
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let saved: Value = serde_json::from_slice(
                &std::fs::read(home.join("desktop-rc/sessions.json")).unwrap(),
            )
            .unwrap();
            if let Some(lineage) = saved["sessions"][&logical]["agit_session"].as_str() {
                let (slug, branch) = lineage.split_once('@').unwrap();
                let repo = home.join("repos").join(slug);
                let head = Command::new("git")
                    .arg("-C")
                    .arg(&repo)
                    .args(["rev-parse", &format!("refs/heads/{branch}")])
                    .output()
                    .unwrap();
                if head.status.success() {
                    let head = String::from_utf8(head.stdout).unwrap().trim().to_owned();
                    if let Ok((_, view)) = agit::domain::storage::materialize_pair_at(&repo, &head)
                        && view.contains(&format!("Observed answer {number}"))
                    {
                        assert!(view.contains("Observed answer 1"));
                        if let Some(before) = &initial_head {
                            let commits = Command::new("git")
                                .arg("-C")
                                .arg(&repo)
                                .args(["rev-list", "--count", &format!("{before}..{head}")])
                                .output()
                                .unwrap();
                            assert_eq!(String::from_utf8(commits.stdout).unwrap().trim(), "1");
                        }
                        initial_head = Some(head);
                        break;
                    }
                }
            }
            assert!(
                Instant::now() < deadline,
                "observed completed turn did not enter its canonical archive"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    let status = command(&root, &["rc", "status", "--json"])
        .output()
        .unwrap();
    assert!(
        !String::from_utf8(status.stdout).unwrap().contains(&logical),
        "observation cannot launch a model"
    );
    assert_eq!(
        std::fs::read_to_string(&transcript).unwrap(),
        format!("{first}{}", turn(2))
    );
    assert!(
        std::fs::read_dir(home.join("desktop-rc/pending-archives"))
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .path()
                .extension()
                .is_some_and(|ext| ext == "json"))
    );
}
