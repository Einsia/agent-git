//! `agit month draft` puts in the bundle only what the Hub published, within the days it keeps.

#![cfg(unix)]

use agit::domain::activity::SCHEMA_VERSION;
use agit::infra::credentials::{HubCredential, save_at};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const ID: &str = "00000000-0000-0000-0000-000000000002";
const MONTH: &str = "2026-10";

fn git(path: &Path, args: &[&str], date: Option<&str>) {
    let mut command = Command::new("git");
    command.current_dir(path).args(args);
    if let Some(date) = date {
        command
            .env("GIT_COMMITTER_DATE", date)
            .env("GIT_AUTHOR_DATE", date);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn identify(path: &Path) {
    git(path, &["config", "user.name", "alice"], None);
    git(path, &["config", "user.email", "alice@example.com"], None);
    git(path, &["config", "commit.gpgsign", "false"], None);
}

/// Decode one query value as the client encodes it.
fn decoded(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap();
            out.push(u8::from_str_radix(hex, 16).unwrap());
            i += 3;
        } else {
            out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

fn field(query: &str, key: &str) -> Option<String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(name, _)| *name == key)
        .map(|(_, value)| decoded(value))
}

/// The Hub's page for the session: one turn saved on the kept day, one on a day the member hid,
/// and one whose time is a publication time, which no day can be told from. Any other session
/// asked for has no turns, so a request for it shows only in what was asked.
fn page(query: &str, session: &str) -> Value {
    let contribution = |commit: &str, saved: &str, basis: &str, text: &str| {
        json!({
            "session": session, "commit": commit, "saved_at": saved, "time_basis": basis,
            "author_name": "Alice Example", "author_email": "hub-author@example.invalid",
            "messages": [{
                "role": "user", "line": 1, "text": text, "sent_at": saved, "sent_basis": "recorded",
            }],
        })
    };
    json!({
        "schema_version": SCHEMA_VERSION,
        "repo": "acme/work",
        "applied_filters": {
            "author": field(query, "author"),
            "since": field(query, "since"),
            "before": field(query, "before"),
            "session": field(query, "session"),
            "assistant": field(query, "assistant").unwrap_or_else(|| "all".into()),
        },
        "contributions": if field(query, "session").as_deref() == Some(session) {
            json!([
                contribution("c0ffee0001", "2026-10-03T09:00:00Z", "committed_at", "HUB-kept-day"),
                contribution("c0ffee0002", "2026-10-02T09:00:00Z", "committed_at", "HUB-hidden-day"),
                contribution("c0ffee0003", "2026-10-03T10:00:00Z", "published_at", "HUB-unplaced"),
            ])
        } else {
            json!([])
        },
        "has_more": false, "next_cursor": null, "incomplete": false, "reasons": {}, "warnings": [],
    })
}

/// The member's own month: the session's kept day is the third; the second was hidden. The
/// Hub names no kept day of the other session, and a Hub older than kept days names none of
/// either.
fn month_view(session: &str, other: &str, old_hub: bool) -> Value {
    let days = json!([{ "start": "2026-10-03T00:00:00.000Z", "end": "2026-10-04T00:00:00.000Z" }]);
    // A detected credential in what was uploaded as the session's title and milestone.
    let mut kept = json!({
        "session": session, "title": "Lease pool AKIA4X7QZ2M5RT6VW3JH", "url": "https://hub.invalid/s",
        "turns": 1, "dates": ["2026-10-03"], "milestones": ["Rotated AKIA4X7QZ2M5RT6VW3JH"],
    });
    if !old_hub {
        kept["days"] = days;
    }
    json!({
        "month": MONTH, "source": "turns_recorded_under_account", "state": "ready",
        "ready_at": "2026-10-31T00:00:00.000Z", "shared": false,
        "totals": { "repositories": 1, "sessions": 1, "active_days": 1, "turns": 1 },
        "repositories": [{
            "repository": "acme/work", "repository_id": "rid-work", "private": true,
            "state": "ready", "version": "version-the-draft-reads", "turns": 1, "active_days": 1,
            "truncated": false,
            "sessions": [kept, {
                "session": other, "title": "Retry budget", "url": "https://hub.invalid/o",
                "turns": 1, "dates": ["2026-10-05"], "milestones": [],
            }],
        }],
    })
}

/// Everything in the bundle comes from the Hub, and only from turns saved within the days the
/// Hub keeps of the session. A local checkout of the repository whose branch holds a commit that
/// was never published, saved on the kept day, must not reach the bundle: a draft that read the
/// checkout would hand the member's agent work they never uploaded. A turn the Hub returns from
/// a day the member hid must not reach it either, though the Hub returns it for the window it
/// was asked, and a turn whose day cannot be told is left out and counted as unread. Who wrote
/// a turn is not in the bundle. A session the Hub names no kept day of is never asked for, and
/// a Hub that names none at all is refused before anything is read: a draft that fell back to
/// the whole month would read the days the member hid. A session's title and milestones pass
/// the device's protection as its messages do. A draft replaces the earlier one whole, so a file
/// or summary from it cannot carry what was hidden since; a summary the member edited there goes
/// only when they say so, and a run that would delete it without --replace changes nothing.
#[test]
fn a_draft_reads_only_published_turns_of_the_kept_days() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().canonicalize().unwrap();
    let remote_root = home.join("remotes");
    let remote = remote_root.join("acme/work.git");
    std::fs::create_dir_all(&remote).unwrap();
    git(
        &remote,
        &["init", "--bare", "-q", "--initial-branch=main"],
        None,
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    save_at(
        &home.join("credentials").join(format!(
            "{}.json",
            agit::infra::config::hub_host_key(&base).unwrap()
        )),
        &HubCredential {
            account_id: None,
            username: "alice".into(),
            email: None,
            hub: Some(base.clone()),
            access_token: "SYNTHETIC-month-draft-access".into(),
            refresh_token: String::new(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2000-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();

    // A published turn, then a clone of the repository with a turn saved on top of it that was
    // never pushed, both on the kept day.
    let sid = format!("agit-{}", "a".repeat(40));
    let other = format!("agit-{}", "b".repeat(40));
    let old_hub = Arc::new(AtomicBool::new(false));
    let asked = Arc::new(Mutex::new(Vec::<String>::new()));
    let seed = home.join("seed");
    std::fs::create_dir_all(seed.join("session")).unwrap();
    git(&seed, &["init", "-q", "--initial-branch=main"], None);
    identify(&seed);
    let mut meta =
        agit::domain::meta::Meta::new(sid.clone(), "claude-code".into(), "/fixture".into());
    let mut log = agit::domain::transcript::wrap_lines(
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"SEED-published\"}}\n",
        "claude-code",
        &sid,
    );
    meta.turn = Some(1);
    std::fs::write(
        seed.join("session/meta.json"),
        serde_json::to_vec(&meta).unwrap(),
    )
    .unwrap();
    agit::domain::storage::write_snapshot(&seed, &log, &log).unwrap();
    git(&seed, &["add", "-A"], None);
    git(
        &seed,
        &["commit", "-qm", "Save fixture"],
        Some("2026-10-03T10:00:00Z"),
    );
    git(
        &seed,
        &["remote", "add", "origin", remote.to_str().unwrap()],
        None,
    );
    git(&seed, &["push", "-q", "origin", "main"], None);
    let local = home.join("repos/acme/work");
    git(
        &home,
        &[
            "clone",
            "-q",
            remote.to_str().unwrap(),
            local.to_str().unwrap(),
        ],
        None,
    );
    identify(&local);
    git(
        &local,
        &[
            "remote",
            "set-url",
            "origin",
            &format!("{base}/acme/work.git"),
        ],
        None,
    );
    let local_repo = agit::domain::repo::Repo::open(&local).unwrap();
    agit::hub::identity::pin(
        &local_repo,
        &agit::hub::identity::RemoteIdentity::new(&base, ID).unwrap(),
    )
    .unwrap();
    log += &agit::domain::transcript::wrap_lines(
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"LOCAL-unpublished\"}}\n",
        "claude-code",
        &sid,
    );
    meta.turn = Some(2);
    std::fs::write(
        local.join("session/meta.json"),
        serde_json::to_vec(&meta).unwrap(),
    )
    .unwrap();
    agit::domain::storage::write_snapshot(&local, &log, &log).unwrap();
    git(&local, &["add", "-A"], None);
    git(
        &local,
        &["commit", "-qm", "Save fixture"],
        Some("2026-10-03T12:00:00Z"),
    );

    let stop = Arc::new(AtomicBool::new(false));
    let server = {
        let (stop, remote_root, sid, other, base, old_hub, asked) = (
            stop.clone(),
            remote_root.clone(),
            sid.clone(),
            other.clone(),
            base.clone(),
            old_hub.clone(),
            asked.clone(),
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
                let json_reply = |body: Value| {
                    (
                        "200 OK".to_owned(),
                        "application/json".to_owned(),
                        body.to_string().into_bytes(),
                    )
                };
                let (status, content_type, body) = if path
                    == format!("/api/orgs/acme/activity/months/{MONTH}")
                {
                    json_reply(month_view(&sid, &other, old_hub.load(Ordering::Relaxed)))
                } else if path == "/api/agents/acme/work/activity" {
                    asked
                        .lock()
                        .unwrap()
                        .push(field(query, "session").unwrap_or_default());
                    json_reply(page(query, &sid))
                } else if path == "/api/agents/acme/work/activity/attribution" {
                    let request: Value =
                        serde_json::from_slice(&input[header_end..header_end + length]).unwrap();
                    let body = if request.get("facts").is_some() {
                        json!({"facts":{"account":null,"publications":[],"horizon":null,"unmapped":{"count":0}}})
                    } else if request.get("missing").is_some() {
                        json!({"missing":{"count":0}})
                    } else {
                        json!({"aliases":[]})
                    };
                    json_reply(
                        json!({"schema_version":SCHEMA_VERSION,"repository_id":ID,"body":body}),
                    )
                } else if path == "/api/agents/acme/work" {
                    json_reply(json!({
                        "agent_id": ID, "owner": "acme", "name": "work",
                        "clone_url": format!("{base}/acme/work.git"),
                        "encryption_enabled": false,
                    }))
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
                    let body = output.stdout[boundary + 4..].to_vec();
                    (status, content_type, body)
                };
                write!(stream, "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                stream.write_all(&body).unwrap();
            }
        })
    };

    std::fs::create_dir_all(home.join("h")).unwrap();
    let draft = |extra: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_agit"))
            .current_dir(&home)
            .env("HOME", home.join("h"))
            .env("AGIT_HOME", &home)
            .env("AGIT_HUB_URL", &base)
            .env("AGIT_TELEMETRY_DISABLED", "1")
            .env("AGIT_SECRETS_KEYSTORE", "file")
            .env("NO_COLOR", "1")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("AGIT_SESSION")
            .env_remove("AGIT_SESSION_ID")
            .env_remove("AGIT_MERGE_TX")
            .env_remove("AGIT_RC")
            .env_remove("AGIT_EXPECTED_AGENT_ID")
            .env_remove("AGIT_EXPECTED_HUB")
            .args(["month", "draft", "acme", MONTH])
            .args(extra)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    // An earlier draft left a repository's file, listed in its README, beside a summary and
    // notes of the member's own.
    let bundle_dir = home.join("month/acme").join(MONTH);
    std::fs::create_dir_all(&bundle_dir).unwrap();
    std::fs::write(
        bundle_dir.join("README.md"),
        "Repositories:\n\n- acme/gone: 1 session(s)\n",
    )
    .unwrap();
    std::fs::write(bundle_dir.join("acme__gone.json"), "STALE-hidden").unwrap();
    std::fs::write(bundle_dir.join("summary.md"), "STALE-summary").unwrap();
    std::fs::write(bundle_dir.join("notes.json"), "MEMBER-notes").unwrap();
    // Each run's result is taken before the next starts, so every assertion below judges only
    // the run it is about.
    let kept = draft(&[]);
    let summary_kept = std::fs::read_to_string(bundle_dir.join("summary.md")).ok();
    let notes_kept = std::fs::read_to_string(bundle_dir.join("notes.json")).ok();
    let asked_kept = asked.lock().unwrap().len();
    let output = draft(&["--replace"]);
    let stale_left = ["acme__gone.json", "summary.md", "notes.json"]
        .iter()
        .filter(|name| bundle_dir.join(name).exists())
        .count();
    let bundle_path = home.join("month/acme").join(MONTH).join("acme__work.json");
    let raw = std::fs::read_to_string(&bundle_path).unwrap_or_default();
    let asked_first = asked.lock().unwrap().clone();
    old_hub.store(true, Ordering::Relaxed);
    let refused = draft(&[]);
    stop.store(true, Ordering::Relaxed);
    server.join().unwrap();
    assert_eq!(kept.status.code(), Some(4));
    let named = String::from_utf8_lossy(&kept.stderr);
    assert!(
        named.contains("notes.json") && named.contains("summary.md"),
        "{named}"
    );
    assert!(!named.contains("acme__gone.json"), "{named}");
    assert_eq!(
        summary_kept.as_deref(),
        Some("STALE-summary"),
        "a redraft without --replace deleted the summary the member edited"
    );
    assert_eq!(
        notes_kept.as_deref(),
        Some("MEMBER-notes"),
        "a redraft without --replace deleted a JSON file the member saved"
    );
    assert_eq!(asked_kept, 0, "a refused redraft read the month");
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        stale_left, 0,
        "a file of an earlier draft was left beside the new one"
    );
    assert!(
        !raw.contains("LOCAL-unpublished"),
        "a turn that was never published reached the bundle: {raw}"
    );
    assert!(
        !raw.contains("HUB-hidden-day"),
        "a turn of a day the member hid reached the bundle: {raw}"
    );
    assert!(raw.contains("HUB-kept-day"), "{raw}");
    assert!(!raw.contains("HUB-unplaced"), "{raw}");
    assert!(
        !raw.contains("hub-author@example.invalid") && !raw.contains("Alice Example"),
        "who wrote a turn reached the bundle: {raw}"
    );
    let bundle: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(bundle["incomplete"][&sid]["unplaced"]["count"], 1, "{raw}");
    assert_eq!(bundle["version"], "version-the-draft-reads");
    assert_eq!(bundle["repository_id"], "rid-work");
    assert_eq!(
        asked_first,
        vec![sid.clone()],
        "the draft asked for a session over days the Hub does not keep"
    );
    assert_eq!(
        bundle["incomplete"][&other]["no_kept_days"]["count"], 1,
        "{raw}"
    );
    assert_eq!(
        refused.status.code(),
        Some(4),
        "a Hub that names no kept day was drafted from: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(String::from_utf8_lossy(&refused.stderr).contains("newer Hub"));
    assert!(
        !raw.contains("AKIA4X7QZ2M5RT6VW3JH"),
        "a credential in a session's title or milestone reached the bundle: {raw}"
    );
}
