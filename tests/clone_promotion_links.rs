//! Promoting a checkout moves only claims for its fully qualified source repository.

use agit::domain::{link, repo::Repo, store::Store};
use agit::hub::identity::{self, RemoteIdentity};
use serde_json::json;
use std::io::{self, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

const SOURCE: &str = "aaaaaaaa-0000-4000-8000-000000000001";
const COPY: &str = "bbbbbbbb-0000-4000-8000-000000000002";

#[test]
fn same_name_promotion_moves_the_recorded_owner() {
    promotion("qa");
}

#[test]
fn renamed_promotion_keeps_other_namespaces_and_legacy_claims_unchanged() {
    promotion("mine");
}

const MAX_REQUEST_HEADERS: usize = 8 * 1024;
const MAX_REQUEST_BODY: usize = 16 * 1024;

// Responding before the declared body is consumed can close the connection during its upload.
fn read_request(stream: impl Read) -> io::Result<(String, Vec<u8>)> {
    let invalid = |message| io::Error::new(io::ErrorKind::InvalidData, message);
    let mut reader = BufReader::new(stream);
    let mut headers = Vec::new();
    while !headers.ends_with(b"\r\n\r\n") {
        if headers.len() == MAX_REQUEST_HEADERS {
            return Err(invalid(
                "local hub request headers exceed the fixture limit",
            ));
        }
        let mut byte = [0];
        reader.read_exact(&mut byte)?;
        headers.push(byte[0]);
    }
    let headers = String::from_utf8(headers)
        .map_err(|_| invalid("local hub request headers are not UTF-8"))?;
    let mut length = None;
    for line in headers
        .split("\r\n")
        .skip(1)
        .filter(|line| !line.is_empty())
    {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| invalid("local hub request has a malformed header"))?;
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(invalid("local hub fixture requires Content-Length framing"));
        }
        if name.eq_ignore_ascii_case("content-length") {
            if length.is_some() {
                return Err(invalid("local hub request repeats Content-Length"));
            }
            let value = value.trim();
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(invalid("local hub request has an invalid Content-Length"));
            }
            let value = value
                .parse::<usize>()
                .map_err(|_| invalid("local hub Content-Length cannot be represented"))?;
            if value > MAX_REQUEST_BODY {
                return Err(invalid("local hub request body exceeds the fixture limit"));
            }
            length = Some(value);
        }
    }
    if headers.starts_with("POST ") && length.is_none() {
        return Err(invalid("local hub POST omits Content-Length"));
    }
    let mut body = vec![0; length.unwrap_or(0)];
    reader.read_exact(&mut body)?;
    Ok((headers, body))
}

#[test]
fn local_hub_consumes_the_complete_body_across_transport_chunks() {
    let body = br#"{"name":"mine","expected_source_agent_id":"synthetic"}"#;
    let headers = format!(
        "POST /api/agents/alice/qa/clone HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    let request = [headers.as_bytes(), body].concat();
    for split in [headers.len() - 1, headers.len(), headers.len() + 1] {
        let input =
            std::io::Cursor::new(&request[..split]).chain(std::io::Cursor::new(&request[split..]));
        let (actual_headers, actual_body) = read_request(input).unwrap();
        assert_eq!(actual_headers, headers);
        assert_eq!(actual_body, body);
    }
    let (headers, body) =
        read_request(b"GET /api/agents/alice/qa HTTP/1.1\r\n\r\n".as_slice()).unwrap();
    assert!(headers.starts_with("GET "));
    assert!(body.is_empty());
}

#[test]
fn local_hub_refuses_truncated_malformed_and_oversized_requests() {
    for request in [
        "POST / HTTP/1.1\r\n".to_owned(),
        "POST / HTTP/1.1\r\nContent-Length: 3\r\n\r\nab".into(),
    ] {
        assert_eq!(
            read_request(request.as_bytes()).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }
    for headers in [
        "Content-Length: -1".to_owned(),
        "Content-Length: nope".into(),
        "Content-Length:".into(),
        "Content-Length: 99999999999999999999999999999999".into(),
        "Content-Length: 0\r\ncontent-length: 0".into(),
        "Content-Length: 0\r\nContent-Length: 1".into(),
        "Transfer-Encoding: chunked".into(),
        "Host: localhost".into(),
        "malformed-header".into(),
        format!("Content-Length: {}", MAX_REQUEST_BODY + 1),
        format!("X-Fill: {}", "x".repeat(MAX_REQUEST_HEADERS)),
    ] {
        let request = format!("POST / HTTP/1.1\r\n{headers}\r\n\r\n");
        assert_eq!(
            read_request(request.as_bytes()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}

fn promotion(name: &str) {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("agit");
    let work = temporary.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let hub = format!("http://{}", listener.local_addr().unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    let copy_name = name.to_string();
    let server_hub = hub.clone();
    let server = std::thread::spawn(move || {
        while !stopped.load(Ordering::Relaxed) {
            let (mut stream, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("local hub accept: {error}"),
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let (request, body) =
                read_request(&mut stream).expect("local hub request is incomplete or invalid");
            let response = if request.starts_with("POST /api/agents/alice/qa/clone ") {
                let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(
                    body,
                    json!({"name": (copy_name != "qa").then_some(copy_name.as_str()),
                        "expected_source_agent_id": SOURCE})
                );
                json!({"agent_id":COPY,"forked_from":SOURCE,"owner":"me","name":copy_name,
                    "push_url":format!("{server_hub}/me/{copy_name}.git"),"web_url":format!("{server_hub}/me/{copy_name}")})
            } else {
                assert!(request.starts_with("GET /api/agents/alice/qa "), "{request}");
                assert!(body.is_empty());
                json!({"agent_id":SOURCE,"owner":"alice","name":"qa","visibility":"public",
                    "clone_url":format!("{server_hub}/alice/qa.git")})
            }.to_string();
            write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",response.len(),response).unwrap();
        }
    });
    let credential = agit::infra::credentials::HubCredential {
        username: "me".into(),
        email: None,
        hub: Some(hub.clone()),
        access_token: "synthetic-token".into(),
        access_expires_at: "2099-01-01T00:00:00Z".into(),
        refresh_token: "synthetic-refresh".into(),
        refresh_expires_at: "2099-01-01T00:00:00Z".into(),
    };
    agit::infra::credentials::save_at(
        &home.join("credentials").join(format!(
            "{}.json",
            agit::infra::config::hub_host_key(&hub).unwrap()
        )),
        &credential,
    )
    .unwrap();
    let source = Repo::init(&home.join("repos/alice/qa")).unwrap();
    source.git(&["config", "commit.gpgsign", "false"]).unwrap();
    std::fs::write(
        source.root().join("README.md"),
        "local history retained by promotion",
    )
    .unwrap();
    source.add_all().unwrap();
    source.commit("local history").unwrap();
    source.git(&["branch", "-m", "work"]).unwrap();
    let original_head = source.git(&["rev-parse", "refs/heads/work"]).unwrap();
    identity::pin(&source, &RemoteIdentity::new(&hub, SOURCE).unwrap()).unwrap();
    let store = Store::at(home.join("store"));
    for (id, owner) in [
        ("alice-session", Some("alice")),
        ("bob-session", Some("bob")),
        ("legacy-session", None),
    ] {
        let mut claim = link::Link::new("claude-code", id, Some(&work));
        claim.agent = Some("qa".into());
        claim.owner = owner.map(str::to_string);
        claim.branch = Some("work".into());
        link::write(&store, &claim).unwrap();
    }
    let untouched: Vec<_> = ["bob-session", "legacy-session"]
        .iter()
        .map(|id| {
            let path = store.root().join("claude-code").join(format!("{id}.json"));
            (path.clone(), std::fs::read(path).unwrap())
        })
        .collect();
    let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
    command
        .args(["clone", "alice/qa", "--mine", "--no-bind"])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", temporary.path())
        .env("AGIT_HOME", &home)
        .env("AGIT_HUB_URL", &hub)
        .env("CI", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .current_dir(&work);
    if name != "qa" {
        command.args(["--name", name]);
    }
    let output = command.output().unwrap();
    stop.store(true, Ordering::Relaxed);
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!home.join("repos/alice/qa").exists());
    let promoted = Repo::open(home.join("repos/me").join(name)).unwrap();
    assert_eq!(
        promoted.git(&["rev-parse", "refs/heads/work"]).unwrap(),
        original_head
    );
    let moved = link::get(&store, "claude-code", "alice-session").unwrap();
    assert_eq!(moved.owner.as_deref(), Some("me"));
    assert_eq!(moved.agent.as_deref(), Some(name));
    assert_eq!(moved.branch.as_deref(), Some("work"));
    for (path, bytes) in untouched {
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
}
