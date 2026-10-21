//! Automatic source selection must preserve sync failures and the machine-readable contract.
//! A continuation reads the snapshot its first page fetched; fetching again per page would let
//! concurrent saves move the history under an open cursor.

use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

fn git(path: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(path)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn command(home: &Path, base: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
    command
        .current_dir(home)
        .env("AGIT_HOME", home)
        .env("AGIT_HUB_URL", base)
        .env("NO_COLOR", "1")
        .env_remove("AGIT_SESSION")
        .env_remove("AGIT_SESSION_ID")
        .env_remove("AGIT_MERGE_TX")
        .env_remove("AGIT_RC")
        .env_remove("AGIT_EXPECTED_AGENT_ID")
        .env_remove("AGIT_EXPECTED_HUB");
    command
}

fn activity(home: &Path, base: &str, extra: &[&str]) -> std::process::Output {
    command(home, base)
        .args([
            "activity",
            "alice/work",
            "--author",
            "alice",
            "--since",
            "2026-10-01",
            "--before",
            "2100-01-01",
            "--json",
        ])
        .args(extra)
        .output()
        .unwrap()
}

fn value(output: &std::process::Output) -> Value {
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let doc: Value = serde_json::from_slice(&output.stdout).unwrap();
    doc["result"]["value"].clone()
}

fn texts(value: &Value) -> Vec<String> {
    value["contributions"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|c| c["messages"].as_array().unwrap())
        .map(|m| m["text"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn absent_repository_uses_hub_and_existing_repository_fetches_without_moving_local_branches() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    let remote_root = home.join("remotes");
    let remote = remote_root.join("alice/work.git");
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "--bare", "-q", "--initial-branch=main"]);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let id = "00000000-0000-0000-0000-000000000001";
    std::fs::create_dir_all(home.join("credentials")).unwrap();
    std::fs::write(home.join("credentials").join(format!("127.0.0.1_{}.json", listener.local_addr().unwrap().port())), json!({"username":"alice","hub":base,"access_token":"test-only","refresh_token":"test-only","access_expires_at":"2100-01-01T00:00:00Z","refresh_expires_at":"2100-01-01T00:00:00Z"}).to_string()).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let fail_fetch = Arc::new(AtomicBool::new(false));
    let activity_calls = Arc::new(AtomicUsize::new(0));
    let git_calls = Arc::new(AtomicUsize::new(0));
    let server = {
        let (stop, fail_fetch, activity_calls, git_calls, base) = (
            stop.clone(),
            fail_fetch.clone(),
            activity_calls.clone(),
            git_calls.clone(),
            base.clone(),
        );
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                // Accepted sockets can inherit the listener's non-blocking mode.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                    .unwrap();
                let mut input = Vec::new();
                let header_end = loop {
                    if let Some(end) = input.windows(4).position(|v| v == b"\r\n\r\n") {
                        break end + 4;
                    }
                    let mut buffer = [0; 8192];
                    let count = stream.read(&mut buffer).unwrap();
                    assert!(count > 0);
                    input.extend_from_slice(&buffer[..count]);
                };
                let headers = String::from_utf8(input[..header_end].to_vec()).unwrap();
                let header = |key: &str| {
                    headers
                        .lines()
                        .find_map(|line| {
                            line.split_once(':')
                                .filter(|(name, _)| name.eq_ignore_ascii_case(key))
                                .map(|(_, value)| value.trim().to_owned())
                        })
                        .unwrap_or_default()
                };
                let length: usize = header("content-length").parse().unwrap_or(0);
                while input.len() < header_end + length {
                    let mut buffer = [0; 8192];
                    let count = stream.read(&mut buffer).unwrap();
                    assert!(count > 0);
                    input.extend_from_slice(&buffer[..count]);
                }
                let mut request = headers.lines().next().unwrap().split_whitespace();
                let method = request.next().unwrap();
                let target = request.next().unwrap();
                let (path, query) = target.split_once('?').unwrap_or((target, ""));
                let (status, content_type, body) = if path == "/api/agents/alice/work/activity" {
                    activity_calls.fetch_add(1, Ordering::Relaxed);
                    assert!(query.contains("author=alice"));
                    ("200 OK".into(), "application/json".into(), json!({"schema_version":3,"repo":"alice/work","applied_filters":{"author":"alice","since":"2026-10-01T00:00:00Z","before":"2100-01-01T00:00:00Z","session":null},"contributions":[],"has_more":false,"next_cursor":null,"incomplete":false,"reasons":{},"warnings":[]}).to_string().into_bytes())
                } else if path == "/api/agents/alice/work/activity/attribution" {
                    assert_eq!(method, "POST");
                    assert_eq!(header("x-agentgit-expected-agent-id"), id);
                    let request: Value =
                        serde_json::from_slice(&input[header_end..header_end + length]).unwrap();
                    let body = if request.get("facts").is_some() {
                        json!({"facts":{"account":null,"publications":[],"horizon":null,"unmapped":{"count":0}}})
                    } else if request.get("missing").is_some() {
                        json!({"missing":{"count":0}})
                    } else {
                        json!({"aliases":[]})
                    };
                    (
                        "200 OK".into(),
                        "application/json".into(),
                        json!({"schema_version":3,"repository_id":id,"body":body})
                            .to_string()
                            .into_bytes(),
                    )
                } else if path == "/api/agents/alice/work" {
                    ("200 OK".into(), "application/json".into(), json!({"agent_id":id,"owner":"alice","name":"work","clone_url":format!("{base}/alice/work.git"),"encryption_enabled":false}).to_string().into_bytes())
                } else {
                    git_calls.fetch_add(1, Ordering::Relaxed);
                    if fail_fetch.load(Ordering::Relaxed) {
                        (
                            "503 Service Unavailable".into(),
                            "text/plain".into(),
                            b"sync unavailable".to_vec(),
                        )
                    } else {
                        let mut child = Command::new("git")
                            .arg("http-backend")
                            .env("GIT_PROJECT_ROOT", &remote_root)
                            .env("GIT_HTTP_EXPORT_ALL", "1")
                            .env("PATH_INFO", path)
                            .env("QUERY_STRING", query)
                            .env("REQUEST_METHOD", method)
                            .env("CONTENT_TYPE", header("content-type"))
                            .env("CONTENT_LENGTH", length.to_string())
                            .env("GIT_PROTOCOL", header("git-protocol"))
                            .stdin(Stdio::piped())
                            .stdout(Stdio::piped())
                            .stderr(Stdio::piped())
                            .spawn()
                            .unwrap();
                        child
                            .stdin
                            .take()
                            .unwrap()
                            .write_all(&input[header_end..header_end + length])
                            .unwrap();
                        let output = child.wait_with_output().unwrap();
                        assert!(
                            output.status.success(),
                            "{}",
                            String::from_utf8_lossy(&output.stderr)
                        );
                        let boundary = output
                            .stdout
                            .windows(4)
                            .position(|v| v == b"\r\n\r\n")
                            .unwrap();
                        let cgi = String::from_utf8_lossy(&output.stdout[..boundary]);
                        let status = cgi
                            .lines()
                            .find_map(|line| line.strip_prefix("Status: "))
                            .unwrap_or("200 OK")
                            .to_owned();
                        let content_type = cgi
                            .lines()
                            .find_map(|line| line.strip_prefix("Content-Type: "))
                            .unwrap_or("application/octet-stream")
                            .to_owned();
                        (status, content_type, output.stdout[boundary + 4..].to_vec())
                    }
                };
                write!(stream, "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                stream.write_all(&body).unwrap();
            }
        })
    };
    let hub = value(&activity(home, &base, &[]));
    assert_eq!(hub["source"], "hub");
    assert!(!home.join("repos/alice/work").exists());
    assert_eq!(activity_calls.load(Ordering::Relaxed), 1);
    let work = home.join("seed");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "--initial-branch=main"]);
    git(&work, &["config", "user.name", "alice"]);
    git(&work, &["config", "user.email", "alice@example.com"]);
    git(&work, &["config", "commit.gpgsign", "false"]);
    let sid = format!("agit-{}", "a".repeat(40));
    std::fs::create_dir_all(work.join("session")).unwrap();
    let mut meta =
        agit::domain::meta::Meta::new(sid.clone(), "claude-code".into(), "/fixture".into());
    let mut log = String::new();
    let mut save = |log: &str, turn: u32| {
        meta.turn = Some(turn);
        std::fs::write(
            work.join("session/meta.json"),
            serde_json::to_vec(&meta).unwrap(),
        )
        .unwrap();
        agit::domain::storage::write_snapshot(&work, log, log).unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-qm", "Save fixture"]);
    };
    log += &agit::domain::transcript::wrap_lines(
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"original\"}}\n",
        "claude-code",
        &sid,
    );
    save(&log, 1);
    git(
        &work,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&work, &["push", "-q", "origin", "main"]);
    let local = home.join("repos/alice/work");
    git(
        home,
        &[
            "clone",
            "-q",
            remote.to_str().unwrap(),
            local.to_str().unwrap(),
        ],
    );
    git(
        &local,
        &[
            "remote",
            "set-url",
            "origin",
            &format!("{base}/alice/work.git"),
        ],
    );
    let local_repo = agit::domain::repo::Repo::open(&local).unwrap();
    agit::hub::identity::pin(
        &local_repo,
        &agit::hub::identity::RemoteIdentity::new(&base, id).unwrap(),
    )
    .unwrap();
    let old_head = local_repo.git(&["rev-parse", "HEAD"]).unwrap();
    log += &agit::domain::transcript::wrap_lines(
        "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":\"fresh from remote\"}}\n",
        "claude-code",
        &sid,
    );
    save(&log, 2);
    git(&work, &["push", "-q", "origin", "main"]);
    let first = value(&activity(home, &base, &["--limit", "1"]));
    assert_eq!(first["source"], "local");
    assert_eq!(texts(&first), ["original"]);
    assert_eq!(local_repo.git(&["rev-parse", "HEAD"]).unwrap(), old_head);
    assert_eq!(activity_calls.load(Ordering::Relaxed), 1);
    let fetched = git_calls.load(Ordering::Relaxed);
    assert!(fetched > 0);
    let cursor = first["next_cursor"].as_str().unwrap();
    let second = value(&activity(
        home,
        &base,
        &["--limit", "1", "--cursor", cursor],
    ));
    assert_eq!(texts(&second), ["fresh from remote"]);
    assert_eq!(second["has_more"], false);
    assert_eq!(git_calls.load(Ordering::Relaxed), fetched);
    fail_fetch.store(true, Ordering::Relaxed);
    assert!(!activity(home, &base, &[]).status.success());
    assert_eq!(activity_calls.load(Ordering::Relaxed), 1);
    stop.store(true, Ordering::Relaxed);
    server.join().unwrap();
}
