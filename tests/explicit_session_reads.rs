//! Reading and sharing an omitted target follows the explicitly selected branch, never recency.

use agit::domain::{meta, repo::Repo, storage, transcript};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;

const REPO: &str = "me/context";

fn fixture(root: &Path) -> PathBuf {
    let home = root.join("agit");
    let repo = Repo::init(&home.join("repos").join(REPO)).unwrap();
    repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    repo.add_all().unwrap();
    repo.commit("file line").unwrap();
    for (branch, marker, id, date) in [
        ("selected", "SELECTED-CONTENT", 'a', "2024-01-01T00:00:00Z"),
        ("newest", "UNSELECTED-CONTENT", 'b', "2025-01-01T00:00:00Z"),
    ] {
        repo.git(&["switch", "-q", "-c", branch, "main"]).unwrap();
        let claim = format!("agit-{}", id.to_string().repeat(40));
        let raw = serde_json::json!({"type":"user","message":{"role":"user","content":marker}})
            .to_string();
        let envelope = transcript::wrap_lines(&raw, "claude-code", &claim);
        storage::write_snapshot(repo.root(), &envelope, &envelope).unwrap();
        meta::write(
            repo.root(),
            &meta::Meta::new(claim, "claude-code".into(), "/work".into()),
        )
        .unwrap();
        repo.add_all().unwrap();
        let out = Command::new("git")
            .current_dir(repo.root())
            .args(["commit", "-qm", branch])
            .env("GIT_COMMITTER_DATE", date)
            .env("GIT_AUTHOR_DATE", date)
            .output()
            .unwrap();
        assert!(out.status.success());
    }
    home
}

fn agit(home: &Path, work: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
    command
        .current_dir(work)
        .args(args)
        .env("AGIT_HOME", home)
        .env("AGIT_SESSION", format!("{REPO}@selected"));
    for (key, _) in agit::infra::runtime_session::ENV_SESSIONS {
        command.env_remove(key);
    }
    command
}

#[test]
fn show_uses_the_selected_branch_even_when_another_session_is_newer() {
    let tmp = tempfile::tempdir().unwrap();
    let home = fixture(tmp.path());
    let out = agit(&home, tmp.path(), &["show"]).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("SELECTED-CONTENT"), "{text}");
    assert!(!text.contains("UNSELECTED-CONTENT"), "{text}");
}

/// Session reads and forks retain their selected branch when a tag names another history.
#[test]
fn captured_session_refs_read_and_fork_the_local_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let home = fixture(tmp.path());
    let repo = Repo::open(home.join("repos").join(REPO)).unwrap();
    repo.git(&["tag", "selected", "refs/heads/newest"]).unwrap();
    let tip = repo.git(&["rev-parse", "refs/heads/selected"]).unwrap();
    for args in [
        vec!["view", "--json"],
        vec!["view", "@", "--json"],
        vec!["export", "@"],
    ] {
        let output = agit(&home, tmp.path(), &args).output().unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("SELECTED-CONTENT"), "{args:?}: {text}");
        assert!(!text.contains("UNSELECTED-CONTENT"), "{args:?}: {text}");
    }
    let forked = agit(&home, tmp.path(), &["fork", "@", "-b", "forked"])
        .output()
        .unwrap();
    assert!(
        forked.status.success(),
        "{}",
        String::from_utf8_lossy(&forked.stderr)
    );
    assert_eq!(repo.git(&["rev-parse", "refs/heads/forked^"]).unwrap(), tip);
    let text = storage::materialize_at(repo.root(), "refs/heads/forked", meta::VIEW_FILE).unwrap();
    assert!(text.contains("SELECTED-CONTENT"));
    assert!(!text.contains("UNSELECTED-CONTENT"));
}

#[test]
fn share_sends_only_the_selected_branch_to_the_requested_hub() {
    let tmp = tempfile::tempdir().unwrap();
    let home = fixture(tmp.path());
    let payload = share_payload(&home, tmp.path());
    assert!(payload.contains("SELECTED-CONTENT"), "{payload}");
    assert!(!payload.contains("UNSELECTED-CONTENT"), "{payload}");
}

fn share_payload(home: &Path, work: &Path) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let hub = format!("http://{}", listener.local_addr().unwrap());
    let credential = agit::infra::credentials::HubCredential {
        username: "me".into(),
        email: None,
        hub: Some(hub.clone()),
        access_token: "synthetic-test-token".into(),
        access_expires_at: "2099-01-01T00:00:00Z".into(),
        refresh_token: "synthetic-test-refresh".into(),
        refresh_expires_at: "2099-01-01T00:00:00Z".into(),
    };
    let key = agit::infra::config::hub_host_key(&hub).unwrap();
    agit::infra::credentials::save_at(
        &home.join("credentials").join(format!("{key}.json")),
        &credential,
    )
    .unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for incoming in listener.incoming() {
            let mut stream = incoming.unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                .unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            let mut length = None;
            let mut chunked = false;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                let (key, value) = line.split_once(':').unwrap();
                if key.eq_ignore_ascii_case("content-length") {
                    length = Some(value.trim().parse::<usize>().unwrap());
                }
                if key.eq_ignore_ascii_case("transfer-encoding") {
                    chunked = value.trim().eq_ignore_ascii_case("chunked");
                }
            }
            let mut payload = Vec::new();
            if let Some(length) = length {
                payload.resize(length, 0);
                reader.read_exact(&mut payload).unwrap();
            } else if chunked {
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    let count = usize::from_str_radix(line.trim(), 16).unwrap();
                    if count == 0 {
                        break;
                    }
                    let offset = payload.len();
                    payload.resize(offset + count, 0);
                    reader.read_exact(&mut payload[offset..]).unwrap();
                    let mut ending = [0; 2];
                    reader.read_exact(&mut ending).unwrap();
                    assert_eq!(&ending, b"\r\n");
                }
            }
            let shared = request_line.starts_with("POST /api/shares ");
            let body = if shared {
                sender
                    .send(serde_json::from_slice::<serde_json::Value>(&payload).unwrap())
                    .unwrap();
                r#"{"slug":"synthetic-share","url":"http://localhost/s/synthetic-share"}"#
                    .to_owned()
            } else {
                serde_json::json!({"version":env!("CARGO_PKG_VERSION"),"tag":"test"}).to_string()
            };
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            if shared {
                break;
            }
        }
    });
    let out = agit(home, work, &["share", "--public", "-y"])
        .env("AGIT_HUB_URL", &hub)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body = receiver
        .recv_timeout(std::time::Duration::from_secs(10))
        .unwrap();
    body["payload"].as_str().unwrap().to_owned()
}

/// Source adapters may need earlier selected records, but merging their output must preserve
/// the envelope order rather than grouping all dialogue from the same runtime together.
#[test]
fn share_renders_both_runtime_orders_and_each_runtime_on_its_own() {
    let claude = |text: &str| {
        transcript::wrap_lines(
            &serde_json::json!({"type":"user","message":{"role":"user","content":text}})
                .to_string(),
            "claude-code",
            &format!("agit-{}", "c".repeat(40)),
        )
    };
    let codex = |text: &str| {
        transcript::wrap_lines(
            &serde_json::json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}}).to_string(),
            "codex", &format!("agit-{}", "d".repeat(40)),
        )
    };
    let c1 = claude("CLAUDE-FIRST");
    let c2 = claude("CLAUDE-LAST");
    let x1 = codex("CODEX-FIRST");
    let x2 = codex("CODEX-LAST");
    let summary = agit::commands::merge::summary_envelope(
        "MERGED-CONCLUSION",
        "codex",
        &format!("agit-{}", "d".repeat(40)),
    );
    for (selected, runtime, expected) in [
        (
            format!("{c1}{x1}{summary}{c2}{x2}"),
            "claude-code",
            vec![
                "CLAUDE-FIRST",
                "CODEX-FIRST",
                "MERGED-CONCLUSION",
                "CLAUDE-LAST",
                "CODEX-LAST",
            ],
        ),
        (
            format!("{x1}{c1}{summary}{x2}{c2}"),
            "codex",
            vec![
                "CODEX-FIRST",
                "CLAUDE-FIRST",
                "MERGED-CONCLUSION",
                "CODEX-LAST",
                "CLAUDE-LAST",
            ],
        ),
        (
            format!("{c1}{c2}"),
            "claude-code",
            vec!["CLAUDE-FIRST", "CLAUDE-LAST"],
        ),
        (
            format!("{x1}{x2}"),
            "codex",
            vec!["CODEX-FIRST", "CODEX-LAST"],
        ),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let home = fixture(tmp.path());
        let repo = Repo::open(home.join("repos").join(REPO)).unwrap();
        repo.git(&["switch", "-q", "selected"]).unwrap();
        storage::write_snapshot(repo.root(), &selected, &selected).unwrap();
        let mut snapshot = meta::Meta::new(
            format!("agit-{}", "a".repeat(40)),
            runtime.into(),
            "/work".into(),
        );
        snapshot.kind = meta::Kind::Merge;
        meta::write(repo.root(), &snapshot).unwrap();
        repo.add_all().unwrap();
        repo.commit("selected mixed history").unwrap();
        let payload = share_payload(&home, tmp.path());
        let mut offset = 0;
        for text in expected {
            let position = payload[offset..]
                .find(text)
                .unwrap_or_else(|| panic!("selected event {text} missing or reordered: {payload}"));
            offset += position + text.len();
        }
        assert!(!payload.contains("UNSELECTED-CONTENT"), "{payload}");
    }
}
