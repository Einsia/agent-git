#![cfg(feature = "cli")]

//! A real Git receiver must see projected ancestry while encrypted snapshots retain the originals.

use agit::domain::{
    meta, privacy::PrivacyPolicy, privacy_envelope::PrivacyEnvelope, repo::Repo, storage,
    transcript,
};
use agit::infra::{
    config,
    credentials::{HubCredential, save_at},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use crypto_box::SecretKey;
use serde_json::json;
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    path::Path,
    process::{Command, Output, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[path = "support/privacy_password_terminal.rs"]
mod privacy_password_terminal;
#[path = "support/publication_http.rs"]
mod publication_http;
#[path = "support/publication_text.rs"]
mod publication_text;
#[path = "support/startup_cache.rs"]
mod startup_cache;

const AGENT_ID: &str = "9f2c3b53-7fe0-412f-b62a-bf68a6845ce7";
const SOURCE_ID: &str = "3a48ec29-769e-449d-9593-ec777ea87fd7";
const SESSION_SECRET: &str = "blue horse battery";

struct Hub {
    base: String,
    posts: Arc<AtomicUsize>,
    deny: Arc<AtomicBool>,
    corrupt_receipt: Arc<AtomicBool>,
    confirmations: Arc<AtomicUsize>,
    source_revision: Arc<AtomicUsize>,
    change_sources_after_read: Arc<AtomicBool>,
    resolutions: Arc<Mutex<Vec<serde_json::Value>>>,
    creations: Arc<AtomicUsize>,
    strategies: Arc<AtomicUsize>,
    viewing_public: Arc<Mutex<String>>,
    key_version: Arc<AtomicUsize>,
    revoked_reader_key: Arc<AtomicBool>,
    reader_requests: Arc<AtomicUsize>,
    source_writable: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Hub {
    fn start(root: &Path, viewing_key: [u8; 32]) -> Self {
        let key = SecretKey::from(viewing_key);
        let public = STANDARD.encode(key.public_key().as_bytes());
        let wrapped = agit::domain::privacy_key::KeyInput::wrap(
            &zeroize::Zeroizing::new(viewing_key),
            &zeroize::Zeroizing::new("synthetic-repository-password".into()),
        )
        .unwrap();
        let viewing_public = Arc::new(Mutex::new(public.clone()));
        let served_public = Arc::clone(&viewing_public);
        let key_version = Arc::new(AtomicUsize::new(1));
        let served_version = Arc::clone(&key_version);
        let revoked_reader_key = Arc::new(AtomicBool::new(false));
        let revoked = Arc::clone(&revoked_reader_key);
        let reader_requests = Arc::new(AtomicUsize::new(0));
        let reader_count = Arc::clone(&reader_requests);
        let source_writable = Arc::new(AtomicBool::new(false));
        let source_write_access = source_writable.clone();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let url = format!("{base}/alice/app.git");
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let posts = Arc::new(AtomicUsize::new(0));
        let received = Arc::clone(&posts);
        let deny = Arc::new(AtomicBool::new(false));
        let denied = Arc::clone(&deny);
        let corrupt_receipt = Arc::new(AtomicBool::new(false));
        let corrupting = Arc::clone(&corrupt_receipt);
        let confirmations = Arc::new(AtomicUsize::new(0));
        let confirmed = Arc::clone(&confirmations);
        let source_revision = Arc::new(AtomicUsize::new(1));
        let served_revision = Arc::clone(&source_revision);
        let change_sources_after_read = Arc::new(AtomicBool::new(false));
        let change_sources = Arc::clone(&change_sources_after_read);
        let resolutions = Arc::new(Mutex::new(Vec::new()));
        let resolved = Arc::clone(&resolutions);
        let creations = Arc::new(AtomicUsize::new(0));
        let created = Arc::clone(&creations);
        let strategies = Arc::new(AtomicUsize::new(0));
        let registered = Arc::clone(&strategies);
        let resolver_hub = base.clone();
        let root = root.to_owned();
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(300);
            let mut strategy = serde_json::Value::Null;
            let mut preview = serde_json::Value::Null;
            let mut pending_receipt = None;
            let hub_sources = json!([{"version": 1, "id": "hub-organization", "revision": "r1", "exclude": ["src/hub/**"], "memory_exclude": []}]);
            let content_digest = agit::domain::privacy_envelope::digest_json(
                &json!({"version":1, "owner_id":"owner-1", "revision":"r1", "sources":hub_sources}),
            )
            .unwrap();
            let mandatory = format!(
                r#"{{"version":1,"source":"repository","require_envelope":true,"publication_format_version":1,"protected_paths":["session/log.jsonl","session/VIEW"],"content_policy_digest":"{content_digest}"}}"#
            );
            let mandatory_digest =
                agit::domain::privacy_envelope::digest_bytes(mandatory.as_bytes());
            let mut last_request = String::new();
            while !stopping.load(Ordering::Acquire) {
                assert!(
                    Instant::now() < deadline,
                    "mock Hub deadline elapsed after {last_request}"
                );
                let (mut stream, _) = match publication_http::accept(&listener) {
                    Ok(pair) => pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("mock Hub accept failed: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(&mut stream);
                let mut first = String::new();
                if reader.read_line(&mut first).unwrap() == 0 {
                    continue;
                }
                last_request = first.trim().to_owned();
                let mut words = first.split_whitespace();
                let method = words.next().unwrap();
                let target = words.next().unwrap();
                let mut length = 0;
                let mut content_type = String::new();
                let mut authenticated = false;
                let mut receipt_id = None;
                let mut receiver_scope = None;
                loop {
                    let mut line = String::new();
                    assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                    if line == "\r\n" {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(value) = lower.strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                    if let Some(value) = lower.strip_prefix("content-type:") {
                        content_type = value.trim().to_owned();
                    }
                    assert!(
                        !lower.starts_with("transfer-encoding:"),
                        "fixture body must be bounded"
                    );
                    authenticated |= lower.trim() == "authorization: bearer synthetic-access";
                    if let Some(value) = lower.strip_prefix("x-agentgit-privacy-preview-id:") {
                        receipt_id = Some(value.trim().to_owned());
                    }
                    if let Some(value) = lower.strip_prefix("x-agentgit-privacy-receiver-scope:") {
                        receiver_scope = Some(value.trim().to_owned());
                    }
                }
                assert!(authenticated || target == "/api/auth/refresh", "{first}");
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                drop(reader);
                if target == "/api/agents/alice/app" && !root.join("alice/app.git").is_dir() {
                    let body = r#"{"error":"repository is unavailable"}"#;
                    write!(stream, "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                    continue;
                }
                if target.starts_with("/team/app.git/") {
                    assert_eq!(method, "GET");
                    assert!(target.ends_with("info/refs?service=git-receive-pack"));
                    write!(
                        stream,
                        "HTTP/1.1 {} Reply\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        if source_write_access.load(Ordering::Acquire) {
                            200
                        } else {
                            403
                        }
                    )
                    .unwrap();
                    continue;
                }
                if target.starts_with("/alice/app.git/") && denied.load(Ordering::Acquire) {
                    write!(
                        stream,
                        "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .unwrap();
                    continue;
                }
                if method == "GET" && target.contains("/privacy/keys/") {
                    reader_count.fetch_add(1, Ordering::AcqRel);
                    if revoked.load(Ordering::Acquire) {
                        let body =
                            r#"{"kind":"not_found","error":"publication viewing key not found"}"#;
                        write!(stream, "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                        continue;
                    }
                }
                let json = match (method, target) {
                    ("POST", "/api/auth/refresh") => {
                        assert_eq!(
                            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["refresh_token"],
                            "synthetic-refresh"
                        );
                        Some(
                            json!({"account_id":"account-1","username":"alice","access_token":"synthetic-access","refresh_token":"synthetic-refresh","access_expires_at":"2099-01-01T00:00:00Z","refresh_expires_at":"2099-01-01T00:00:00Z"}),
                        )
                    }
                    ("GET", "/api/auth/me") => {
                        Some(json!({"account_id":"account-1","username":"alice"}))
                    }
                    ("GET", path)
                        if path.starts_with("/api/agents/alice/app/secret-allowances?") =>
                    {
                        assert_eq!(
                            path,
                            format!(
                                "/api/agents/alice/app/secret-allowances?expected_agent_id={AGENT_ID}"
                            )
                        );
                        assert!(body.is_empty());
                        Some(json!({
                            "version": 1, "agent_id": AGENT_ID, "revision": 0,
                            "value_identity_scheme": "sha256-v1", "decisions": []
                        }))
                    }
                    ("GET", "/api/agents/alice/app/privacy/publishing-key") => {
                        let current = served_public.lock().unwrap().clone();
                        Some(
                            json!({"agent_id":AGENT_ID,"config_version":served_version.load(Ordering::Acquire),"current":if current.is_empty() { serde_json::Value::Null } else { json!({"recipient":agit::domain::privacy_key::recipient_id(&current),"public_key_algorithm":"x25519","public_key":current}) }}),
                        )
                    }
                    ("POST", "/api/privacy/policy-sources/resolve") => {
                        let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
                        assert_eq!(request["version"], 1);
                        let source = request["repository"] == "team/app";
                        if source {
                            assert_eq!(request["agent_id"], SOURCE_ID);
                        } else {
                            assert_eq!(request["repository"], "alice/app");
                            assert_eq!(
                                request["agent_id"],
                                if root.join("alice/app.git").is_dir() {
                                    json!(AGENT_ID)
                                } else {
                                    serde_json::Value::Null
                                }
                            );
                        }
                        resolved.lock().unwrap().push(request.clone());
                        assert!(!request["request_id"].as_str().unwrap().is_empty());
                        let now = chrono::Utc::now();
                        let revision = served_revision.load(Ordering::Acquire);
                        if change_sources.swap(false, Ordering::AcqRel) {
                            served_revision.store(2, Ordering::Release);
                        }
                        Some(json!({
                            "version": 1, "hub": resolver_hub, "repository": request["repository"], "agent_id": request["agent_id"],
                            "account_id": "account-1", "request_id": request["request_id"], "owner_id": if source { "source-owner" } else { "owner-1" },
                            "revision": format!("r{revision}"),
                            "issued_at": now, "expires_at": now + chrono::Duration::minutes(5),
                            "sources": if source { json!([{"version":1,"id":"source-organization","revision":"r1","exclude":["src/source/**"],"memory_exclude":[]}]) } else { hub_sources.clone() }
                        }))
                    }
                    ("POST", "/api/agents") => {
                        let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
                        assert_eq!(request["name"], "app");
                        assert_eq!(request["public"], false);
                        assert_eq!(request["repo_origins"], json!([]));
                        assert!(request["owner"].is_null());
                        let destination = root.join("alice/app.git");
                        assert!(!destination.exists());
                        fs::create_dir_all(&destination).unwrap();
                        git(&destination, &["init", "--bare", "--initial-branch=main"]);
                        git(&destination, &["config", "http.receivepack", "true"]);
                        created.fetch_add(1, Ordering::Release);
                        Some(
                            json!({"agent_id":AGENT_ID,"encryption_enabled":true,"owner":"alice","name":"app","push_url":url,"web_url":format!("{resolver_hub}/alice/app")}),
                        )
                    }
                    ("GET", "/api/agents/team/app") => Some(
                        json!({"agent_id":SOURCE_ID,"encryption_enabled":false,"owner":"team","name":"app","clone_url":format!("{resolver_hub}/team/app.git"),"visibility":"private"}),
                    ),
                    ("PUT", "/api/agents/alice/app/privacy/strategy") => {
                        registered.fetch_add(1, Ordering::Release);
                        strategy = serde_json::from_slice(&body).unwrap();
                        assert_eq!(strategy["summary"], json!({"session_only":true}));
                        strategy["agent_id"] = json!(AGENT_ID);
                        strategy["mandatory_policy"] = serde_json::from_str(&mandatory).unwrap();
                        strategy["mandatory_policy_digest"] = json!(mandatory_digest);
                        Some(strategy.clone())
                    }
                    ("POST", "/api/agents/alice/app/privacy/publication/preview") => {
                        preview = serde_json::from_slice(&body).unwrap();
                        assert_eq!(preview["policy_digest"], strategy["policy_digest"]);
                        assert_eq!(
                            preview["expected_refs_digest"],
                            remote_refs_digest(&root.join("alice/app.git"))
                        );
                        assert_eq!(preview["snapshot_digest"], preview["target_refs_digest"]);
                        let public = served_public.lock().unwrap().clone();
                        let recipient =
                            agit::domain::privacy_envelope::ViewingRecipient::from_base64(
                                agit::domain::privacy_envelope::digest_bytes(public.as_bytes())
                                    .replace(':', "-"),
                                &public,
                            )
                            .unwrap();
                        assert_eq!(
                            preview["receiver_scope"],
                            format!("repository:private:{}", recipient.fingerprint().unwrap())
                        );
                        preview["preview_id"] =
                            json!(format!("preview-{}", confirmed.load(Ordering::Acquire)));
                        preview["mandatory_policy_digest"] = json!(mandatory_digest);
                        let mut response = preview.clone();
                        response["expires_at"] = json!(
                            (chrono::Utc::now() + chrono::Duration::minutes(10)).to_rfc3339()
                        );
                        Some(response)
                    }
                    ("POST", "/api/agents/alice/app/privacy/publication/confirm") => {
                        let mut request: serde_json::Value = serde_json::from_slice(&body).unwrap();
                        request["mandatory_policy_digest"] = json!(mandatory_digest);
                        assert_eq!(request, preview);
                        confirmed.fetch_add(1, Ordering::Release);
                        pending_receipt = Some(preview.clone());
                        if corrupting.load(Ordering::Acquire) {
                            request["target_refs_digest"] = json!(
                                agit::domain::privacy_envelope::digest_bytes(b"another snapshot")
                            );
                        }
                        Some(request)
                    }
                    ("GET", "/api/agents/alice/app") => Some(
                        json!({"agent_id":AGENT_ID,"encryption_enabled":true,"owner":"alice","name":"app","clone_url":url,"visibility":"private"}),
                    ),
                    ("GET", path) if path.starts_with("/api/agents/alice/app/privacy/keys/") => {
                        let (recipient, commit) = path
                            .strip_prefix("/api/agents/alice/app/privacy/keys/")
                            .unwrap()
                            .split_once("?ref=")
                            .unwrap();
                        assert_eq!(recipient, agit::domain::privacy_key::recipient_id(&public));
                        let metadata: meta::Meta = serde_json::from_str(&git(
                            &root.join("alice/app.git"),
                            &["show", &format!("{commit}:{}", meta::FILE)],
                        ))
                        .unwrap();
                        let mut record = serde_json::to_value(&wrapped).unwrap();
                        record["recipient"] = json!(recipient);
                        record["current"] = json!(true);
                        record["updated_at"] = json!("2026-09-26T00:00:00Z");
                        Some(
                            json!({"agent_id":AGENT_ID,"commit":commit,"session_id":metadata.session,"key":record}),
                        )
                    }
                    _ => None,
                };
                if let Some(json) = json {
                    let body = json.to_string();
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                } else {
                    assert!(
                        target.starts_with("/alice/app.git/"),
                        "unexpected request: {first}"
                    );
                    if method == "POST" && target.ends_with("/git-receive-pack") {
                        let receipt: serde_json::Value = pending_receipt
                            .take()
                            .expect("receive needs a single-use confirmation");
                        assert_eq!(receipt_id.as_deref(), receipt["preview_id"].as_str());
                        assert_eq!(
                            receiver_scope.as_deref(),
                            receipt["receiver_scope"].as_str()
                        );
                        assert_eq!(
                            receipt["expected_refs_digest"],
                            remote_refs_digest(&root.join("alice/app.git"))
                        );
                        assert!(
                            body.windows(b" atomic".len())
                                .any(|value| value == b" atomic"),
                            "receipt-bound receive must request atomic refs"
                        );
                        received.fetch_add(1, Ordering::Release);
                    }
                    let (path, query) = target.split_once('?').unwrap_or((target, ""));
                    let mut command = git_command(&root);
                    let mut child = command
                        .arg("http-backend")
                        .env("GIT_PROJECT_ROOT", &root)
                        .env("GIT_HTTP_EXPORT_ALL", "1")
                        .env("REQUEST_METHOD", method)
                        .env("PATH_INFO", path)
                        .env("QUERY_STRING", query)
                        .env("CONTENT_TYPE", content_type)
                        .env("CONTENT_LENGTH", body.len().to_string())
                        .env("REMOTE_USER", "alice")
                        .stdin(Stdio::piped())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .spawn()
                        .unwrap();
                    child.stdin.take().unwrap().write_all(&body).unwrap();
                    let response = child.wait_with_output().unwrap();
                    assert!(response.status.success(), "{response:?}");
                    if method == "POST" && target.ends_with("/git-receive-pack") {
                        assert_eq!(
                            preview["target_refs_digest"],
                            remote_refs_digest(&root.join("alice/app.git"))
                        );
                    }
                    let split = response
                        .stdout
                        .windows(4)
                        .position(|bytes| bytes == b"\r\n\r\n")
                        .unwrap();
                    let header = std::str::from_utf8(&response.stdout[..split]).unwrap();
                    assert!(!header.contains("Status:"), "{header}");
                    let body = &response.stdout[split + 4..];
                    // Access probes may close after the response headers.
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\n{header}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(body);
                }
            }
        });
        Self {
            base,
            posts,
            deny,
            corrupt_receipt,
            confirmations,
            source_revision,
            change_sources_after_read,
            resolutions,
            creations,
            strategies,
            viewing_public,
            key_version,
            revoked_reader_key,
            reader_requests,
            source_writable,
            stop,
            worker: Some(worker),
        }
    }
}

fn remote_refs_digest(repository: &Path) -> String {
    use sha2::{Digest, Sha256};
    let output = git(
        repository,
        &["for-each-ref", "--format=%(refname)%00%(objectname)"],
    );
    let mut refs: Vec<_> = output.lines().collect();
    refs.sort();
    let mut digest = Sha256::new();
    for reference in refs {
        digest.update(reference.as_bytes());
        digest.update(b"\n");
    }
    format!("sha256:{}", hex::encode(digest.finalize()))
}

impl Drop for Hub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let result = publication_http::join(self.worker.take().unwrap());
        if !thread::panicking() {
            result.unwrap();
        }
    }
}

fn git_command(root: &Path) -> Command {
    let mut command = Command::new("git");
    command.env_clear();
    for name in ["PATH", "SystemRoot", "WINDIR", "TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
        .env("HOME", root)
        .env("USERPROFILE", root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", root.join("empty-config"));
    command
}

fn git(root: &Path, args: &[&str]) -> String {
    let out = git_command(root)
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "{args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

#[track_caller]
fn assert_success(out: &Output) {
    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn first_publication_requires_configuration_and_read_only_copy_refuses() {
    for copying in [false, true] {
        check_first_publication(copying, false, false);
    }
}

#[test]
fn local_rc_publication_retains_local_identity_and_confirmed_destination() {
    check_first_publication(false, true, false);
}

#[test]
fn separate_encrypted_destination_keeps_source_state_and_requires_original_ancestry() {
    check_first_publication(true, false, true);
}

fn check_first_publication(copying: bool, local_rc: bool, separate: bool) {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    startup_cache::seed(&home);
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(workspace.join("src/hub")).unwrap();
    fs::create_dir_all(workspace.join("src/source")).unwrap();
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(&remote_root).unwrap();
    let key = SecretKey::from([25; 32]);
    let hub = Hub::start(&remote_root, key.to_bytes());
    save_at(
        &home
            .join("credentials")
            .join(format!("{}.json", config::hub_host_key(&hub.base).unwrap())),
        &HubCredential {
            account_id: Some("account-1".into()),
            username: "alice".into(),
            email: None,
            hub: Some(hub.base.clone()),
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    let owner = if local_rc {
        "desktop-machine"
    } else if copying {
        "team"
    } else {
        "alice"
    };
    let repo = Repo::init(&home.join(format!("repos/{owner}/app"))).unwrap();
    repo.git(&["config", "user.name", "Synthetic source"])
        .unwrap();
    repo.git(&["config", "user.email", "source@example.invalid"])
        .unwrap();
    repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
    if local_rc {
        fs::create_dir_all(home.join("desktop-rc")).unwrap();
        fs::write(
            home.join("desktop-rc/identity.json"),
            json!({
                "machine_fingerprint":"machine", "display_name":"Synthetic executor",
                "created_at":"2026-09-23T00:00:00Z"
            })
            .to_string(),
        )
        .unwrap();
        repo.git(&["config", "agit.desktopIdentity", SOURCE_ID])
            .unwrap();
        repo.git(&["config", "agit.desktopAuthority", "local:machine"])
            .unwrap();
    }
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    fs::write(repo.root().join("private.txt"), "INDEPENDENT_SOURCE_FILE").unwrap();
    repo.add_all().unwrap();
    repo.commit("Source files").unwrap();
    repo.git(&["checkout", "-b", "work"]).unwrap();
    let mut raw = [
            json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"destination","name":"Read","input":{"file_path":workspace.join("src/hub/private.rs")}}]}}),
            json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"destination","content":"DESTINATION_RESTRICTED_TEXT"}]}}),
            json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"source","name":"Read","input":{"file_path":workspace.join("src/source/private.rs")}}]}}),
            json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"source","content":"SOURCE_RESTRICTED_TEXT"}]}}),
            json!({"type":"assistant","message":{"role":"assistant","content":"Visible session reply"}}),
        ].into_iter().map(|record| format!("{record}\n")).collect::<String>();
    if local_rc {
        let native = "3f6b1c2a-8d40-4e7b-9a15-2c0de4f8b731";
        for record in [
            json!({"type":"assistant","sessionId":native,"uuid":"88bfaf5e-4107-4cf2-9038-a2a891d171f1","message":{"role":"assistant","content":[
                {"type":"text","text":format!("Readable RC reply {native} {SESSION_SECRET}")},
                {"type":"tool_use","id":"rc-shell","name":"Bash","input":{"command":format!("printf 'Readable RC command {SESSION_SECRET}'")}}
            ]}}),
            json!({"type":"user","sessionId":native,"uuid":"81567b2e-195a-4ed3-a180-06eca3efc210","parentUuid":"88bfaf5e-4107-4cf2-9038-a2a891d171f1","message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"rc-shell","content":format!("Readable RC output {SESSION_SECRET}"),"is_error":false}
            ]}}),
        ] {
            raw.push_str(&format!("{record}\n"));
        }
    }
    let session = format!("agit-{}", "c".repeat(40));
    let log = transcript::wrap_lines(&raw, "claude-code", &session);
    let saved = if local_rc {
        let dictionary =
            agit::domain::secret_filter::RepositoryDictionary::open(repo.root()).unwrap();
        dictionary
            .block_add("RC fixture", SESSION_SECRET.to_string().into(), false)
            .unwrap();
        dictionary
            .protect_envelopes(&log, &agit::domain::secret_filter::Matcher::empty())
            .unwrap()
            .text
    } else {
        log.clone()
    };
    storage::write_snapshot(repo.root(), &saved, &saved).unwrap();
    meta::write(
        repo.root(),
        &meta::Meta::new(
            session.clone(),
            "claude-code".into(),
            workspace.to_string_lossy().into(),
        ),
    )
    .unwrap();
    repo.add_all().unwrap();
    repo.commit("Captured session").unwrap();
    let original = repo.git(&["rev-parse", "HEAD"]).unwrap();
    PrivacyPolicy {
        workspace: Some(workspace.clone()),
        ..Default::default()
    }
    .save(&repo)
    .unwrap();
    let source_remote = remote_root.join("team/app.git");
    let source_refs = if copying {
        fs::create_dir_all(&source_remote).unwrap();
        git(&source_remote, &["init", "--bare", "--initial-branch=main"]);
        git(
            repo.root(),
            &[
                "push",
                source_remote.to_str().unwrap(),
                "HEAD:refs/heads/work",
            ],
        );
        repo.set_remote(&format!("{}/team/app.git", hub.base))
            .unwrap();
        agit::hub::identity::pin(
            &repo,
            &agit::hub::identity::RemoteIdentity::new(&hub.base, SOURCE_ID).unwrap(),
        )
        .unwrap();
        Some(git(&source_remote, &["for-each-ref"]))
    } else {
        None
    };
    let target = format!("{owner}/app@work");
    if local_rc || separate {
        repo.set_auto_push(Some(true)).unwrap();
    }
    let primary_receipt = if separate {
        use agit::domain::privacy_receipt::{PublicationMode, PublicationReceipt};
        let receipt = PublicationReceipt {
            version: 2,
            mode: PublicationMode::Ordinary,
            repository: "team/app".into(),
            branch: "work".into(),
            source: original.clone(),
            published: original.clone(),
            projected_session_id: Some(session.clone()),
            destination: agit::hub::identity::read(&repo).unwrap().unwrap(),
            url: repo.remote_url().unwrap(),
            policy_digest: None,
            recipient: None,
        };
        receipt.save(&repo).unwrap();
        fs::write(repo.common_dir().unwrap().join("agit/privacy-auto-consent.json"),
            json!({"version":2,"mode":"ordinary","hub":hub.base,"account":"alice","account_id":"account-1", "agent_id":SOURCE_ID,"url":receipt.url,"visibility":"private"}).to_string()).unwrap();
        Some(receipt)
    } else {
        None
    };
    let source_config = fs::read(repo.root().join(".git/config")).unwrap();
    let source_local_refs = repo.git(&["for-each-ref"]).unwrap();
    let command_with_destination =
        |confirmed: bool, target: &str, destination: Option<&str>, automatic: bool| {
            let mut cli = Command::new(env!("CARGO_BIN_EXE_agit"));
            cli.env_clear();
            for (name, value) in git_command(temp.path()).get_envs() {
                if let Some(value) = value {
                    cli.env(name, value);
                }
            }
            if confirmed {
                cli.arg("--yes");
            }
            if automatic {
                cli.env("AGIT_AUTO_PUSH", "1");
            }
            cli.args(["push", target, "--private"]);
            if let Some(destination) = destination {
                cli.args(["--to", destination]);
            }
            cli.current_dir(&workspace)
                .env("AGIT_HOME", &home)
                .env("AGIT_HUB_URL", &hub.base)
                .env("CI", "1")
                .env("AGIT_TUI", "0")
                .env("AGIT_USE_SYSTEM_GIT", "1")
                .env(
                    "AGIT_SECRETS_KEYSTORE",
                    if cfg!(unix) { "file" } else { "os" },
                )
                .stdin(Stdio::null());
            cli
        };
    let run_with_destination = |confirmed, target: &str, destination: Option<&str>, automatic| {
        command_with_destination(confirmed, target, destination, automatic)
            .output()
            .unwrap()
    };
    let run = |confirmed, target: &str| {
        run_with_destination(
            confirmed,
            target,
            (local_rc || separate).then_some("alice/app"),
            false,
        )
    };
    // Encryption is opted into at creation; without the flag a first push creates an ordinary
    // repository instead of reaching the viewing-key gate.
    let missing = command_with_destination(
        true,
        &target,
        (local_rc || separate).then_some("alice/app"),
        false,
    )
    .arg("--encryption=true")
    .output()
    .unwrap();
    assert!(!missing.status.success());
    if copying && !separate {
        assert!(
            String::from_utf8_lossy(&missing.stderr).contains("historical-key delivery contract"),
            "{missing:?}"
        );
        assert_eq!(hub.creations.load(Ordering::Acquire), 0);
        assert_eq!(hub.posts.load(Ordering::Acquire), 0);
        assert_eq!(git(&source_remote, &["for-each-ref"]), source_refs.unwrap());
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), original);
        return;
    }
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("agit privacy init alice/app"),
        "{missing:?}"
    );
    assert_eq!(hub.creations.load(Ordering::Acquire), 0);
    assert_eq!(hub.posts.load(Ordering::Acquire), 0);
    let destination = remote_root.join("alice/app.git");
    fs::create_dir_all(&destination).unwrap();
    git(&destination, &["init", "--bare", "--initial-branch=main"]);
    git(&destination, &["config", "http.receivepack", "true"]);
    if separate {
        fs::write(
            home.join("config.json"),
            json!({"privacy.encryption":"false"}).to_string(),
        )
        .unwrap();
    }
    let unconfirmed = run(false, &target);
    assert!(!unconfirmed.status.success());
    assert!(
        String::from_utf8_lossy(&unconfirmed.stderr).contains("requires confirmation"),
        "{unconfirmed:?}"
    );
    assert_eq!(hub.creations.load(Ordering::Acquire), 0);
    assert_eq!(hub.posts.load(Ordering::Acquire), 0);
    assert_success(&run(true, &target));
    assert_eq!(hub.creations.load(Ordering::Acquire), 0);
    let destination = remote_root.join("alice/app.git");
    let envelope = PrivacyEnvelope::parse(
        git(
            &destination,
            &["show", "refs/heads/work:privacy/envelope.json"],
        )
        .as_bytes(),
    )
    .unwrap();
    let public = envelope.public_projection.to_string();
    assert!(public.contains("Visible session reply"));
    if local_rc {
        assert!(public.contains("Readable RC reply"));
        for marker in ["Readable RC command", "Readable RC output"] {
            assert!(!public.contains(marker));
        }
        assert!(!public.contains(SESSION_SECRET));
        assert!(!public.contains("content unavailable"));
    }
    assert!(!public.contains("DESTINATION_RESTRICTED_TEXT"));
    assert_eq!(public.contains("SOURCE_RESTRICTED_TEXT"), !copying);
    let private = envelope.open_layer(&key).unwrap();
    let (original_log, original_view) = private.session_bytes().unwrap();
    assert_eq!(original_log.as_str(), log);
    assert_eq!(original_view.as_str(), log);
    assert!(!original_log.contains("INDEPENDENT_SOURCE_FILE"));
    assert!(!git(&destination, &["rev-list", "--all"]).contains(original.trim()));
    if separate {
        use agit::domain::privacy_receipt::PublicationReceipt;
        assert!(envelope.open_layer(&SecretKey::from([26; 32])).is_err());
        assert_eq!(
            fs::read(repo.root().join(".git/config")).unwrap(),
            source_config
        );
        assert_eq!(repo.git(&["for-each-ref"]).unwrap(), source_local_refs);
        assert_eq!(git(&source_remote, &["for-each-ref"]), source_refs.unwrap());
        assert_eq!(
            PublicationReceipt::load(&repo, "work").unwrap(),
            primary_receipt
        );
        let auto_consent: serde_json::Value = serde_json::from_slice(
            &fs::read(
                repo.common_dir()
                    .unwrap()
                    .join("agit/privacy-auto-consent.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(auto_consent["agent_id"], SOURCE_ID);
        let scoped: Vec<PublicationReceipt> = walkdir::WalkDir::new(
            repo.common_dir()
                .unwrap()
                .join("agit/publication-destinations"),
        )
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| serde_json::from_slice(&fs::read(entry.path()).unwrap()).unwrap())
        .collect();
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].destination.agent_id, AGENT_ID);
        assert_eq!(scoped[0].source, original);
        assert_eq!(
            scoped[0].published,
            git(&destination, &["rev-parse", "work"])
        );
        assert_ne!(scoped[0].published, original);
        assert!(scoped[0].mode.is_encrypted());
        let public_key = STANDARD.encode(key.public_key().as_bytes());
        let target_recipient = agit::domain::privacy_envelope::ViewingRecipient::from_base64(
            agit::domain::privacy_key::recipient_id(&public_key),
            &public_key,
        )
        .unwrap();
        assert_eq!(
            scoped[0].recipient.as_deref(),
            Some(target_recipient.fingerprint().unwrap().as_str())
        );
        let published_refs = git(&destination, &["for-each-ref"]);
        let confirmations = hub.confirmations.load(Ordering::Acquire);
        let repeated = run(true, &target);
        assert_success(&repeated);
        assert!(String::from_utf8_lossy(&repeated.stdout).contains("already up to date"));
        assert_eq!(hub.confirmations.load(Ordering::Acquire), confirmations);
        assert_eq!(git(&destination, &["for-each-ref"]), published_refs);
        let resolutions = hub.resolutions.lock().unwrap();
        for (repository, id) in [("team/app", SOURCE_ID), ("alice/app", AGENT_ID)] {
            assert!(resolutions.iter().any(|request| request["repository"] == repository && request["agent_id"] == id));
        }
        drop(resolutions);

        let ciphertext_clone = home.join("repos/team/cipher");
        git(
            temp.path(),
            &[
                "clone",
                "-b",
                "work",
                destination.to_str().unwrap(),
                ciphertext_clone.to_str().unwrap(),
            ],
        );
        let ciphertext_clone = Repo::at(ciphertext_clone);
        agit::hub::identity::pin(
            &ciphertext_clone,
            &agit::hub::identity::RemoteIdentity::new(&hub.base, AGENT_ID).unwrap(),
        )
        .unwrap();
        ciphertext_clone
            .set_remote(&format!("{}/alice/app.git", hub.base))
            .unwrap();
        hub.source_writable.store(true, Ordering::Release);
        let before = git(&source_remote, &["for-each-ref"]);
        let denied = run_with_destination(true, "team/cipher@work", Some("team/app"), false);
        assert!(!denied.status.success());
        assert!(
            String::from_utf8_lossy(&denied.stderr)
                .contains("selected history contains encrypted snapshots"),
            "{denied:?}"
        );
        assert_eq!(git(&source_remote, &["for-each-ref"]), before);
        assert_eq!(hub.reader_requests.load(Ordering::Acquire), 0);
        assert_eq!(
            fs::read(repo.root().join(".git/config")).unwrap(),
            source_config
        );
        assert_eq!(repo.git(&["for-each-ref"]).unwrap(), source_local_refs);
        return;
    }
    let local_path = if local_rc {
        repo.root().to_owned()
    } else {
        home.join("repos/alice/app")
    };
    let local = Repo::open(&local_path).unwrap();
    assert_eq!(
        agit::hub::identity::read(&local).unwrap().unwrap().agent_id,
        AGENT_ID
    );
    if local_rc {
        assert!(!home.join("repos/alice/app").exists());
        assert_eq!(
            local
                .git(&["config", "agit.desktopIdentity"])
                .unwrap()
                .trim(),
            SOURCE_ID
        );
        let binding: serde_json::Value =
            serde_json::from_str(&local.git(&["config", "agit.desktopPublication"]).unwrap())
                .unwrap();
        assert_eq!(binding["repository"], "alice/app");
        assert_eq!(binding["identity"]["agent_id"], AGENT_ID);
        assert_eq!(binding["local_agent_id"], SOURCE_ID);
        let rejected = run_with_destination(true, &target, Some("bob/replacement"), false);
        assert!(!rejected.status.success());
        assert!(
            String::from_utf8_lossy(&rejected.stderr).contains("cannot be replaced"),
            "{rejected:?}"
        );
    }
    assert_eq!(local.git(&["rev-parse", "HEAD"]).unwrap(), original);
    assert_eq!(
        fs::read_to_string(local.root().join("private.txt")).unwrap(),
        "INDEPENDENT_SOURCE_FILE"
    );
    let resolutions = hub.resolutions.lock().unwrap();
    assert!(
        resolutions
            .iter()
            .any(|request| request["repository"] == "alice/app" && request["agent_id"] == AGENT_ID)
    );
    if let Some(before) = source_refs {
        assert_eq!(git(&source_remote, &["for-each-ref"]), before);
        assert!(
            resolutions
                .iter()
                .any(|request| request["repository"] == "team/app"
                    && request["agent_id"] == SOURCE_ID)
        );
        assert_eq!(
            local.upstream_url().unwrap(),
            format!("{}/team/app.git", hub.base)
        );
    }
    drop(resolutions);
    let mut before = git(&destination, &["for-each-ref"]);
    let repeat_target = if local_rc {
        target.as_str()
    } else {
        "alice/app@work"
    };
    let confirmations_before_repeat = hub.confirmations.load(Ordering::Acquire);
    let posts_before_repeat = hub.posts.load(Ordering::Acquire);
    let repeat = run_with_destination(true, repeat_target, None, false);
    assert_success(&repeat);
    let repeat_output = format!(
        "{}{}",
        String::from_utf8_lossy(&repeat.stdout),
        String::from_utf8_lossy(&repeat.stderr)
    );
    assert!(
        repeat_output.contains("already up to date."),
        "repeat push did not report its no-op result: {repeat_output}"
    );
    assert!(
        repeat_output.contains("alice/app@work (private): already up to date."),
        "repeat push did not identify its branch: {repeat_output}"
    );
    for omitted in [
        "target:",
        "Deterministic inspection:",
        "Privacy processing:",
        "Privacy preview:",
        "Publication destination:",
        "Published the reviewed snapshot",
        "requires confirmation",
    ] {
        assert!(
            !repeat_output.contains(omitted),
            "repeat push rendered {omitted}: {repeat_output}"
        );
    }
    assert_eq!(
        hub.confirmations.load(Ordering::Acquire),
        confirmations_before_repeat
    );
    assert_eq!(hub.posts.load(Ordering::Acquire), posts_before_repeat);
    let missing_tag = git(
        &destination,
        &["for-each-ref", "--format=%(refname)", "refs/tags"],
    );
    let missing_tag = missing_tag.lines().next().unwrap_or_default().to_owned();
    assert!(
        !missing_tag.is_empty(),
        "the privacy publication has no tag"
    );
    git(&destination, &["update-ref", "-d", &missing_tag]);
    let posts_before_tag_repair = hub.posts.load(Ordering::Acquire);
    let tag_repair = run_with_destination(true, repeat_target, None, false);
    assert_success(&tag_repair);
    let tag_repair_output = format!(
        "{}{}",
        String::from_utf8_lossy(&tag_repair.stdout),
        String::from_utf8_lossy(&tag_repair.stderr)
    );
    assert!(!tag_repair_output.contains("already up to date."));
    assert!(hub.posts.load(Ordering::Acquire) > posts_before_tag_repair);
    if !copying && !local_rc {
        let mirror = temp.path().join("mirror.git");
        git(
            temp.path(),
            &[
                "clone",
                "--mirror",
                destination.to_str().unwrap(),
                mirror.to_str().unwrap(),
            ],
        );
        assert_eq!(git(&mirror, &["for-each-ref"]), before);
        git(&destination, &["update-ref", "-d", "refs/heads/work"]);
        assert!(git(&destination, &["for-each-ref", "refs/heads/work"]).is_empty());
        git(local.root(), &["remote", "remove", "origin"]);
        let url = format!("{}/alice/app.git", hub.base);
        let rewrite = format!("url.{}.insteadOf", mirror.display());
        git(local.root(), &["config", "--local", &rewrite, &url]);
        assert!(git(local.root(), &["ls-remote", "--refs", &url]).contains("refs/heads/work"));

        let confirmations = hub.confirmations.load(Ordering::Acquire);
        let receives = hub.posts.load(Ordering::Acquire);
        let unconfirmed = run_with_destination(false, repeat_target, None, false);
        assert!(
            !unconfirmed.status.success(),
            "a local mirror cannot satisfy publication confirmation: {unconfirmed:?}"
        );
        let unconfirmed_output = format!(
            "{}{}",
            String::from_utf8_lossy(&unconfirmed.stdout),
            String::from_utf8_lossy(&unconfirmed.stderr)
        );
        assert!(unconfirmed_output.contains("requires confirmation"));
        assert!(!unconfirmed_output.contains("already up to date."));
        assert_eq!(hub.confirmations.load(Ordering::Acquire), confirmations);
        assert_eq!(hub.posts.load(Ordering::Acquire), receives);

        let repaired = run_with_destination(true, repeat_target, None, false);
        assert_success(&repaired);
        let repaired_output = format!(
            "{}{}",
            String::from_utf8_lossy(&repaired.stdout),
            String::from_utf8_lossy(&repaired.stderr)
        );
        assert!(!repaired_output.contains("already up to date."));
        assert!(repaired_output.contains("Published the reviewed snapshot"));
        assert!(hub.confirmations.load(Ordering::Acquire) > confirmations);
        assert!(hub.posts.load(Ordering::Acquire) > receives);
        assert_eq!(git(&destination, &["for-each-ref"]), before);
        assert_eq!(git(&mirror, &["for-each-ref"]), before);
        let confirmations = hub.confirmations.load(Ordering::Acquire);
        let receives = hub.posts.load(Ordering::Acquire);
        git(local.root(), &["remote", "remove", "origin"]);
        let synchronized = run_with_destination(false, repeat_target, None, false);
        assert_success(&synchronized);
        assert!(
            String::from_utf8_lossy(&synchronized.stdout)
                .contains("alice/app@work (private): already up to date.")
        );
        assert_eq!(hub.confirmations.load(Ordering::Acquire), confirmations);
        assert_eq!(hub.posts.load(Ordering::Acquire), receives);
        git(local.root(), &["config", "--local", "--unset", &rewrite]);
        local.set_remote(&url).unwrap();
    }
    if local_rc {
        use agit::domain::privacy_receipt::{
            SupervisorPushRequest,
            outbox::{Capture, Entry},
        };
        let publication = agit::domain::privacy_receipt::PublicationReceipt::load(&local, "work")
            .unwrap()
            .unwrap();
        let mut request = SupervisorPushRequest {
            version: 1,
            request_id: uuid::Uuid::new_v4().to_string(),
            repository: "alice/app".into(),
            branch: "work".into(),
            source: publication.source.clone(),
            destination: publication.destination.clone(),
            notification_id: None,
        };
        Entry::begin(
            &local,
            &mut request,
            Capture {
                session_id: "local-stream".into(),
                native_session_id: "native-session".into(),
                runtime: "claude-code".into(),
                generation: 1,
                incarnation: None,
                through_seq: None,
            },
        )
        .unwrap();
        // A retained uncertain projection cannot lend its notification ID to the generated result.
        let uncertain = agit::domain::privacy_receipt::PublicationReceipt {
            published: "f".repeat(40),
            ..publication.clone()
        };
        assert_ne!(uncertain.published, publication.published);
        Entry::prepare(&local, &request, &uncertain).unwrap();
        let pending = Entry::load(&local, &request).unwrap().unwrap();
        let result_file = tempfile::NamedTempFile::new_in(local.common_dir().unwrap()).unwrap();
        let run_supervised = |request: &SupervisorPushRequest| {
            fs::write(result_file.path(), serde_json::to_vec(request).unwrap()).unwrap();
            let supervised = command_with_destination(false, repeat_target, None, true)
                .env("AGIT_SESSION", repeat_target)
                .env("AGIT_LOCAL_AGENT_ID", SOURCE_ID)
                .env("AGIT_EXPECTED_AGENT_ID", AGENT_ID)
                .env(
                    agit::domain::privacy_receipt::SUPERVISOR_RESULT_ENV,
                    result_file.path(),
                )
                .output()
                .unwrap();
            assert_success(&supervised);
            request
                .read_managed_result(&local, result_file.path())
                .unwrap()
        };
        let (selected, result) = run_supervised(&request);
        assert_eq!(result, publication);
        assert_ne!(selected.notification_id, request.notification_id);
        assert!(request.read_result(result_file.path()).is_err());
        let saved = Entry::load(&local, &selected).unwrap().unwrap();
        assert_eq!(saved.capture, pending.capture);
        assert_eq!(saved.prepared.as_ref(), Some(&publication));
        assert_eq!(saved.publication.as_ref(), Some(&publication));
        assert_eq!(
            Entry::load(&local, &request).unwrap(),
            Some(pending.clone())
        );

        let retry = SupervisorPushRequest {
            request_id: uuid::Uuid::new_v4().to_string(),
            ..request.clone()
        };
        let (retried, result) = run_supervised(&retry);
        assert_eq!(result, publication);
        assert_eq!(retried.request_id, retry.request_id);
        assert_eq!(retried.notification_id, selected.notification_id);
        assert!(
            request
                .read_managed_result(&local, result_file.path())
                .is_err()
        );
        assert_eq!(Entry::load(&local, &request).unwrap(), Some(pending));
        assert_eq!(
            Entry::pending(&local, "work", "native-session", "claude-code")
                .unwrap()
                .len(),
            2
        );
        let reply: serde_json::Value =
            serde_json::from_slice(&fs::read(result_file.path()).unwrap()).unwrap();
        for field in ["request_id", "notification_id"] {
            let mut forged = reply.clone();
            forged["candidate"][field] = json!(uuid::Uuid::new_v4().to_string());
            fs::write(result_file.path(), serde_json::to_vec(&forged).unwrap()).unwrap();
            assert!(
                retry
                    .read_managed_result(&local, result_file.path())
                    .is_err()
            );
        }
        let prepared_only = agit::domain::privacy_receipt::PublicationReceipt {
            published: "e".repeat(40),
            ..publication.clone()
        };
        let other = Entry::prepare_candidate(&local, &retry, &prepared_only).unwrap();
        let mut forged = reply;
        forged["candidate"] = serde_json::to_value(other).unwrap();
        forged["publication"] = serde_json::to_value(prepared_only).unwrap();
        fs::write(result_file.path(), serde_json::to_vec(&forged).unwrap()).unwrap();
        assert!(
            retry
                .read_managed_result(&local, result_file.path())
                .is_err()
        );
        let public_records = agit::domain::storage::parse_envelopes(
            envelope.public_projection["session"]["log"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert!(!public_records.is_empty());
        assert!(public_records.iter().all(|record| Some(&record.session_id) == publication.projected_session_id.as_ref()));
    }
    assert_eq!(git(&destination, &["for-each-ref"]), before);
    if !local_rc {
        let old_receipt = agit::domain::privacy_receipt::PublicationReceipt::load(&local, "work")
            .unwrap()
            .unwrap();
        let receives = hub.posts.load(Ordering::Acquire);
        hub.key_version.store(2, Ordering::Release);
        assert_success(&run_with_destination(true, repeat_target, None, false));
        assert_eq!(hub.posts.load(Ordering::Acquire), receives);
        assert_eq!(git(&destination, &["for-each-ref"]), before);
        let rotated_key = SecretKey::from([26; 32]);
        *hub.viewing_public.lock().unwrap() = STANDARD.encode(rotated_key.public_key().as_bytes());
        hub.key_version.store(3, Ordering::Release);
        assert_success(&run_with_destination(true, repeat_target, None, false));
        assert_eq!(git(&destination, &["for-each-ref"]), before);
        assert_eq!(
            agit::domain::privacy_receipt::PublicationReceipt::load(&local, "work")
                .unwrap()
                .unwrap(),
            old_receipt
        );
        hub.revoked_reader_key.store(true, Ordering::Release);
        let fresh_home = temp.path().join("fresh-writer");
        startup_cache::seed(&fresh_home);
        let fresh_path = fresh_home.join("repos/alice/app");
        fs::create_dir_all(fresh_path.parent().unwrap()).unwrap();
        git(
            temp.path(),
            &[
                "clone",
                "--branch",
                "work",
                destination.to_str().unwrap(),
                fresh_path.to_str().unwrap(),
            ],
        );
        let fresh = Repo::at(&fresh_path);
        fresh
            .set_remote(&format!("{}/alice/app.git", hub.base))
            .unwrap();
        let identity = agit::hub::identity::RemoteIdentity::new(&hub.base, AGENT_ID).unwrap();
        agit::hub::identity::pin(&fresh, &identity).unwrap();
        fs::create_dir_all(fresh_home.join("credentials")).unwrap();
        for entry in fs::read_dir(home.join("credentials")).unwrap() {
            let entry = entry.unwrap();
            fs::copy(
                entry.path(),
                fresh_home.join("credentials").join(entry.file_name()),
            )
            .unwrap();
        }
        PrivacyPolicy {
            workspace: Some(workspace.clone()),
            ..Default::default()
        }
        .save(&fresh)
        .unwrap();
        let reads = hub.reader_requests.load(Ordering::Acquire);
        let receives = hub.posts.load(Ordering::Acquire);
        let run_fresh = |args: &[&str]| {
            let mut command = command_with_destination(true, "alice/app@work", None, false);
            command.env("AGIT_HOME", &fresh_home);
            if !args.is_empty() {
                let mut selected = Command::new(env!("CARGO_BIN_EXE_agit"));
                selected.env_clear();
                for (name, value) in command.get_envs() {
                    if let Some(value) = value {
                        selected.env(name, value);
                    }
                }
                selected
                    .current_dir(&workspace)
                    .stdin(Stdio::null())
                    .args(args);
                return selected.output().unwrap();
            }
            command.output().unwrap()
        };
        let repeat = run_fresh(&[]);
        assert_success(&repeat);
        assert!(String::from_utf8_lossy(&repeat.stdout).contains("already up to date."));
        assert!(
            agit::domain::privacy_receipt::PublicationReceipt::load(&fresh, "work")
                .unwrap()
                .is_none()
        );
        assert_eq!(hub.posts.load(Ordering::Acquire), receives);
        assert_eq!(hub.reader_requests.load(Ordering::Acquire), reads);
        let refused_unlock = run_fresh(&[
            "privacy",
            "unlock",
            "alice/app@work",
            "--workspace",
            workspace.to_str().unwrap(),
        ]);
        assert!(!refused_unlock.status.success());
        assert!(String::from_utf8_lossy(&refused_unlock.stderr).contains("viewing key not found"));
        assert_eq!(hub.reader_requests.load(Ordering::Acquire), reads + 1);
        let forked = run_fresh(&["fork", "alice/app@work", "-b", "collaborator"]);
        assert_success(&forked);
        let pushed = run_fresh(&["--yes", "push", "alice/app@collaborator"]);
        assert_success(&pushed);
        assert_eq!(hub.reader_requests.load(Ordering::Acquire), reads + 1);
        assert_eq!(
            git(&destination, &["rev-parse", "collaborator^"]),
            old_receipt.published
        );
        let ciphertext = PrivacyEnvelope::parse(
            git(
                &destination,
                &["show", "collaborator:privacy/envelope.json"],
            )
            .as_bytes(),
        )
        .unwrap();
        assert!(ciphertext.open_layer(&rotated_key).is_ok());
        assert!(ciphertext.open_layer(&key).is_err());
        let fresh_receipt =
            agit::domain::privacy_receipt::PublicationReceipt::load(&fresh, "collaborator")
                .unwrap()
                .unwrap();
        assert_ne!(fresh_receipt.recipient, old_receipt.recipient);
        let next = format!(
            "{log}{}",
            transcript::wrap_lines(
                &format!(
                    "{}\n",
                    json!({"type":"user","message":{"role":"user","content":"A turn after repository key rotation"}})
                ),
                "claude-code",
                &format!("agit-{}", "c".repeat(40))
            )
        );
        storage::write_snapshot(local.root(), &next, &next).unwrap();
        local.add_all().unwrap();
        local.commit("Continue after rotation").unwrap();
        assert_success(&run_with_destination(true, repeat_target, None, false));
        let next_public = git(&destination, &["rev-parse", "work"]);
        assert_eq!(
            git(&destination, &["rev-parse", "work^"]),
            old_receipt.published
        );
        let encrypted = PrivacyEnvelope::parse(
            git(&destination, &["show", "work:privacy/envelope.json"]).as_bytes(),
        )
        .unwrap();
        assert!(encrypted.open_layer(&rotated_key).is_ok());
        assert!(encrypted.open_layer(&key).is_err());
        assert_eq!(
            serde_json::from_str::<meta::Meta>(&git(
                &destination,
                &["show", &format!("{next_public}:{}", meta::FILE)]
            ))
            .unwrap()
            .session,
            old_receipt.projected_session_id.unwrap()
        );
        for reference in before.lines() {
            assert!(
                git(&destination, &["for-each-ref", "refs/tags"]).contains(reference)
                    || reference.ends_with("refs/heads/work")
            );
        }
        before = git(&destination, &["for-each-ref"]);
        let receives = hub.posts.load(Ordering::Acquire);
        let repeat = run_with_destination(true, repeat_target, None, false);
        assert_success(&repeat);
        assert!(String::from_utf8_lossy(&repeat.stdout).contains("already up to date."));
        assert_eq!(hub.posts.load(Ordering::Acquire), receives);
        *hub.viewing_public.lock().unwrap() = String::new();
        let missing = run_with_destination(true, repeat_target, None, false);
        assert!(!missing.status.success());
        assert!(String::from_utf8_lossy(&missing.stderr).contains("agit privacy init alice/app"));
        assert_eq!(hub.posts.load(Ordering::Acquire), receives);
        *hub.viewing_public.lock().unwrap() = STANDARD.encode(rotated_key.public_key().as_bytes());
    }
    let mut policy = PrivacyPolicy::load(&local).unwrap();
    policy.exclude.push("newly-private/**".into());
    policy.save(&local).unwrap();
    let strategies = hub.strategies.load(Ordering::Acquire);
    let confirmations = hub.confirmations.load(Ordering::Acquire);
    let receives = hub.posts.load(Ordering::Acquire);
    let incompatible = run_with_destination(true, repeat_target, None, false);
    assert!(!incompatible.status.success());
    assert!(
        String::from_utf8_lossy(&incompatible.stderr).contains("cannot fast-forward"),
        "{incompatible:?}"
    );
    assert_eq!(hub.strategies.load(Ordering::Acquire), strategies);
    assert_eq!(hub.confirmations.load(Ordering::Acquire), confirmations);
    assert_eq!(hub.posts.load(Ordering::Acquire), receives);
    assert_eq!(git(&destination, &["for-each-ref"]), before);
}

#[test]
fn ordinary_push_confirms_projects_and_encrypts_complete_incremental_history() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    startup_cache::seed(&home);
    let mandatory_file = temp.path().join("organization-privacy.json");
    let mandatory = json!({
        "version":1, "id":"organization", "revision":"r1", "exclude":["src/managed/**"]
    });
    fs::write(&mandatory_file, mandatory.to_string()).unwrap();
    fs::write(
        home.join("privacy-policy-sources.json"),
        json!({"version":1,"sources":[mandatory_file]}).to_string(),
    )
    .unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(workspace.join("src")).unwrap();
    fs::create_dir_all(workspace.join("src/managed")).unwrap();
    let remote_root = temp.path().join("remote");
    let remote = remote_root.join("alice/app.git");
    fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "--bare", "--initial-branch=main"]);
    git(&remote, &["config", "http.receivepack", "true"]);
    let key = SecretKey::from([25; 32]);
    let hub = Hub::start(&remote_root, key.to_bytes());
    save_at(
        &home
            .join("credentials")
            .join(format!("{}.json", config::hub_host_key(&hub.base).unwrap())),
        &HubCredential {
            account_id: Some("account-1".into()),
            username: "alice".into(),
            email: None,
            hub: Some(hub.base.clone()),
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    repo.git(&["config", "user.name", "PRIVATE_AUTHOR_MARKER"])
        .unwrap();
    repo.git(&["config", "user.email", "private@example.invalid"])
        .unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    fs::write(
        repo.root().join("private.txt"),
        "PRIVATE_SHARED_FILE_MARKER",
    )
    .unwrap();
    repo.git(&["add", "."]).unwrap();
    repo.git(&["commit", "-m", "PRIVATE_COMMIT_MARKER"])
        .unwrap();
    repo.git(&["checkout", "-b", "work"]).unwrap();
    let session = format!("agit-{}", "a".repeat(40));
    let public_tool_output = format!(
        "PUBLIC_TOOL_OUTPUT_START\n{}PUBLIC_TOOL_OUTPUT_END\n\u{1b}[2Jliteral terminal control",
        "visible evidence ".repeat(160)
    );
    fs::write(workspace.join("src/main.rs"), "PRIVATE_SHARED_FILE_MARKER").unwrap();
    let raw = [
        json!({"type":"user", "cwd":workspace, "message":{"role":"user", "content":format!("Read {} with {SESSION_SECRET}", workspace.join("src/main.rs").display())}}),
        json!({"type":"assistant", "message":{"role":"assistant", "content":[{"type":"tool_use", "id":"call", "name":"Read", "input":{"file_path":workspace.join("private.txt")}}]}}),
        json!({"type":"user", "message":{"role":"user", "content":[{"type":"tool_result", "tool_use_id":"call", "content":"PRIVATE_TOOL_OUTPUT_MARKER"}]}}),
        json!({"type":"assistant", "message":{"role":"assistant", "content":[{"type":"tool_use", "id":"allowed-call", "name":"Read", "input":{"file_path":workspace.join("src/main.rs")}}]}}),
        json!({"type":"user", "message":{"role":"user", "content":[{"type":"tool_result", "tool_use_id":"allowed-call", "content":public_tool_output}]}}),
        json!({"type":"assistant", "message":{"role":"assistant", "content":[{"type":"tool_use", "id":"mandatory-call", "name":"Read", "input":{"file_path":workspace.join("src/managed/private.rs")}}]}}),
        json!({"type":"user", "message":{"role":"user", "content":[{"type":"tool_result", "tool_use_id":"mandatory-call", "content":"MANDATORY_TOOL_OUTPUT_MARKER"}]}}),
        json!({"type":"assistant", "message":{"role":"assistant", "content":[{"type":"tool_use", "id":"hub-call", "name":"Read", "input":{"file_path":workspace.join("src/hub/private.rs")}}]}}),
        json!({"type":"user", "message":{"role":"user", "content":[{"type":"tool_result", "tool_use_id":"hub-call", "content":"HUB_PRIVATE_TOOL_OUTPUT_MARKER"}]}}),
    ].into_iter().chain(publication_text::records(&workspace)).map(|record| format!("{record}\n")).collect::<String>();
    let log = transcript::wrap_lines(&raw, "claude-code", &session);
    let dictionary = agit::domain::secret_filter::RepositoryDictionary::open(repo.root()).unwrap();
    dictionary
        .block_add(
            "Synthetic session value",
            zeroize::Zeroizing::new(SESSION_SECRET.into()),
            false,
        )
        .unwrap();
    let saved = dictionary
        .protect_envelopes(&log, &agit::domain::secret_filter::Matcher::empty())
        .unwrap()
        .text;
    assert!(!saved.contains(SESSION_SECRET));
    let selected = saved.split_inclusive('\n').take(1).collect::<String>();
    storage::write_snapshot(repo.root(), &saved, &selected).unwrap();
    meta::write(
        repo.root(),
        &meta::Meta::new(
            session.clone(),
            "claude-code".into(),
            workspace.display().to_string(),
        ),
    )
    .unwrap();
    repo.git(&["add", "."]).unwrap();
    repo.git(&["commit", "-m", "PRIVATE_SESSION_COMMIT"])
        .unwrap();
    let source_head = git(repo.root(), &["rev-parse", "HEAD"]);
    PrivacyPolicy {
        workspace: Some(workspace.clone()),
        replacements: vec![publication_text::replacement()],
        ..Default::default()
    }
    .save(&repo)
    .unwrap();
    let command = |args: &[&str], automatic: bool| {
        let mut command = git_command(temp.path());
        command = {
            let mut cli = Command::new(env!("CARGO_BIN_EXE_agit"));
            cli.env_clear();
            for (name, value) in command.get_envs() {
                if let Some(value) = value {
                    cli.env(name, value);
                }
            }
            cli
        };
        if automatic {
            command.env("AGIT_AUTO_PUSH", "1");
        }
        command
            .current_dir(&workspace)
            .env("AGIT_HOME", &home)
            .env("AGIT_HUB_URL", &hub.base)
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(unix) { "file" } else { "os" },
            )
            .stdin(Stdio::null())
            .args(args);
        command
    };
    let invoke = |args: &[&str], automatic: bool| command(args, automatic).output().unwrap();
    let run = |args: &[&str]| invoke(args, false);
    let auto = || invoke(&["--yes", "push", "alice/app@work"], true);
    assert_success(&run(&[
        "privacy",
        "policy",
        "include",
        "src/managed/**",
        "--repo",
        "alice/app",
    ]));
    assert!(
        !fs::read_to_string(PrivacyPolicy::path(&repo).unwrap())
            .unwrap()
            .contains("mandatory")
    );
    repo.set_auto_push(Some(true)).unwrap();
    let unconfirmed = auto();
    assert!(!unconfirmed.status.success());
    assert!(String::from_utf8_lossy(&unconfirmed.stderr).contains("renewed confirmation"));
    assert_eq!(hub.posts.load(Ordering::Acquire), 0);
    let declined = run(&["push", "alice/app@work"]);
    assert!(!declined.status.success());
    assert!(
        String::from_utf8_lossy(&declined.stderr).contains("requires confirmation"),
        "{declined:?}"
    );
    let preview = String::from_utf8_lossy(&declined.stdout);
    assert!(
        preview.len() < 4096,
        "default preview exceeds its terminal budget"
    );
    let preview_path = preview
        .lines()
        .find_map(|line| line.strip_prefix("Preview file: "))
        .unwrap();
    let reviewed: serde_json::Value =
        serde_json::from_slice(&fs::read(preview_path).unwrap()).unwrap();
    let public_snapshots = reviewed["snapshots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|snapshot| snapshot["public"].clone())
        .collect::<Vec<_>>();
    let digest = agit::domain::privacy_envelope::digest_json(&json!(public_snapshots)).unwrap();
    assert_eq!(reviewed["public_digest"], digest);
    assert!(preview.contains(&format!("Public digest: {digest}")));
    for visible in [
        "Outgoing public session preview",
        "Snapshots:",
        "Records:",
        "Privacy processing:",
        "Public digest:",
        "Preview file:",
        "Private originals: recoverable",
    ] {
        assert!(preview.contains(visible), "preview is missing {visible}");
    }
    for hidden in [
        "Complete public LOG:",
        "Complete public VIEW:",
        "PUBLIC_TOOL_OUTPUT_START",
        "PUBLIC_TOOL_OUTPUT_END",
        "\\u{1b}[2Jliteral terminal control",
    ] {
        assert!(
            !preview.contains(hidden),
            "summary preview expands {hidden}"
        );
    }
    for private in [
        SESSION_SECRET,
        "PRIVATE_TOOL_OUTPUT_MARKER",
        "PRIVATE_SHARED_FILE_MARKER",
        "MANDATORY_TOOL_OUTPUT_MARKER",
        "HUB_PRIVATE_TOOL_OUTPUT_MARKER",
        "\u{1b}[2J",
    ] {
        assert!(!preview.contains(private), "preview exposes {private}");
    }
    assert_eq!(hub.posts.load(Ordering::Acquire), 0);
    assert!(git(&remote, &["for-each-ref"]).is_empty());
    let expanded = run(&["push", "alice/app@work", "--show-preview"]);
    assert!(!expanded.status.success());
    let expanded = String::from_utf8_lossy(&expanded.stdout);
    fn assert_complete_text(value: &serde_json::Value, preview: &str) {
        match value {
            serde_json::Value::String(text) => {
                for line in text.split('\n') {
                    let escaped = line
                        .chars()
                        .flat_map(|character| {
                            if character.is_control() {
                                character.escape_default().collect::<Vec<_>>()
                            } else {
                                vec![character]
                            }
                        })
                        .collect::<String>();
                    assert!(
                        preview.contains(&escaped),
                        "expanded preview omits public text: {escaped}"
                    );
                }
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    assert_complete_text(value, preview);
                }
            }
            serde_json::Value::Object(fields) => {
                for value in fields.values() {
                    assert_complete_text(value, preview);
                }
            }
            _ => {}
        }
    }
    for snapshot in reviewed["snapshots"].as_array().unwrap() {
        for name in ["log", "view"] {
            for record in
                storage::parse_envelopes(snapshot["public"]["session"][name].as_str().unwrap())
                    .unwrap()
            {
                assert_complete_text(&record.content, &expanded);
            }
        }
    }
    for visible in [
        "Complete public LOG:",
        "PUBLIC_TOOL_OUTPUT_START",
        "PUBLIC_TOOL_OUTPUT_END",
        "STRUCTURED_STDOUT",
        "MIXED_TEXT_AFTER",
    ] {
        assert!(
            expanded.contains(visible),
            "expanded preview omits {visible}"
        );
    }
    for hidden in [
        "PRIVATE_VALUE",
        "ATTACHMENT_SKILL_BODY",
        "NESTED_IMAGE_BODY",
        "MIXED_IMAGE_BODY",
        SESSION_SECRET,
    ] {
        assert!(
            !expanded.contains(hidden),
            "expanded preview exposes {hidden}"
        );
    }
    assert_eq!(hub.posts.load(Ordering::Acquire), 0);
    assert_success(&run(&["push", "alice/app@work", "--dry-run"]));
    assert_eq!(hub.posts.load(Ordering::Acquire), 0);
    assert!(
        agit::domain::privacy_receipt::PublicationReceipt::load(&repo, "work")
            .unwrap()
            .is_none()
    );
    assert_eq!(hub.confirmations.load(Ordering::Acquire), 0);
    hub.corrupt_receipt.store(true, Ordering::Release);
    let refused = run(&["--yes", "push", "alice/app@work"]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stdout).contains("receipt differs"),
        "{refused:?}"
    );
    assert_eq!(hub.posts.load(Ordering::Acquire), 0);
    assert!(git(&remote, &["for-each-ref"]).is_empty());
    hub.corrupt_receipt.store(false, Ordering::Release);
    hub.change_sources_after_read.store(true, Ordering::Release);
    let drifted = run(&["--yes", "push", "alice/app@work"]);
    assert!(!drifted.status.success());
    assert!(
        String::from_utf8_lossy(&drifted.stderr).contains("mandatory Hub privacy rules changed"),
        "{drifted:?}"
    );
    assert_eq!(hub.posts.load(Ordering::Acquire), 0);
    assert!(git(&remote, &["for-each-ref"]).is_empty());
    hub.source_revision.store(1, Ordering::Release);
    assert_success(&run(&["--yes", "push", "alice/app@work"]));
    let receipt = repo
        .common_dir()
        .unwrap()
        .join("agit/privacy-auto-consent.json");
    assert!(receipt.is_file());
    let published = git(&remote, &["rev-parse", "work"]);
    assert_ne!(published, source_head);
    let acknowledgment = agit::domain::privacy_receipt::PublicationReceipt::load(&repo, "work")
        .unwrap()
        .unwrap();
    assert_eq!(acknowledgment.source, source_head);
    assert_eq!(acknowledgment.published, published);
    assert_eq!(acknowledgment.destination.agent_id, AGENT_ID);
    let result_file = tempfile::NamedTempFile::new_in(repo.common_dir().unwrap()).unwrap();
    let mut request = agit::domain::privacy_receipt::SupervisorPushRequest {
        version: 1,
        request_id: uuid::Uuid::new_v4().to_string(),
        repository: "alice/app".into(),
        branch: "work".into(),
        source: "0".repeat(40),
        destination: acknowledgment.destination.clone(),
        notification_id: None,
    };
    fs::write(result_file.path(), serde_json::to_vec(&request).unwrap()).unwrap();
    let supervisor_push = || {
        command(&["push", "alice/app@work"], true)
            .env(
                agit::domain::privacy_receipt::SUPERVISOR_RESULT_ENV,
                result_file.path(),
            )
            .output()
            .unwrap()
    };
    let before_posts = hub.posts.load(Ordering::Acquire);
    let changed = supervisor_push();
    assert!(!changed.status.success());
    assert_eq!(hub.posts.load(Ordering::Acquire), before_posts);
    assert!(request.read_result(result_file.path()).is_err());
    request.source = source_head.clone();
    use agit::domain::privacy_receipt::outbox::{Capture, Entry};
    let capture = Capture {
        session_id: "rc-logical-session".into(),
        native_session_id: "native-session".into(),
        runtime: "claude-code".into(),
        generation: 1,
        incarnation: None,
        through_seq: None,
    };
    let intent = Entry::begin(&repo, &mut request, capture.clone()).unwrap();
    assert!(intent.publication.is_none());
    fs::write(result_file.path(), serde_json::to_vec(&request).unwrap()).unwrap();
    assert_success(&supervisor_push());
    assert_eq!(
        request.read_result(result_file.path()).unwrap(),
        acknowledgment
    );
    let restored = Entry::load(&repo, &request).unwrap().unwrap();
    assert_eq!(restored.notification_id, intent.notification_id);
    assert_eq!(restored.publication.as_ref(), Some(&acknowledgment));
    let mut retry = request.clone();
    retry.request_id = uuid::Uuid::new_v4().to_string();
    retry.notification_id = None;
    let restored = Entry::begin(
        &repo,
        &mut retry,
        Capture {
            generation: 2,
            ..capture
        },
    )
    .unwrap();
    assert_eq!(restored.notification_id, intent.notification_id);
    assert_eq!(restored.capture.generation, 1);
    fs::write(result_file.path(), serde_json::to_vec(&retry).unwrap()).unwrap();
    assert_success(&supervisor_push());
    assert_eq!(
        retry.read_result(result_file.path()).unwrap(),
        acknowledgment
    );
    retry.notification_id = Some(uuid::Uuid::new_v4().to_string());
    fs::write(result_file.path(), serde_json::to_vec(&retry).unwrap()).unwrap();
    let posts = hub.posts.load(Ordering::Acquire);
    assert!(!supervisor_push().status.success());
    assert_eq!(hub.posts.load(Ordering::Acquire), posts);
    fs::write(result_file.path(), serde_json::to_vec(&request).unwrap()).unwrap();
    assert_success(&supervisor_push());
    let mut stale = request.clone();
    stale.request_id = uuid::Uuid::new_v4().to_string();
    assert!(stale.read_result(result_file.path()).is_err());
    assert!(git(&remote, &["for-each-ref", "refs/heads/main"]).is_empty());
    let file_line = run(&["--yes", "push", "alice/app@main"]);
    assert!(!file_line.status.success());
    assert!(String::from_utf8_lossy(&file_line.stderr).contains("excludes repository file lines"));
    assert_success(&run(&["--yes", "push", "alice/app", "--all"]));
    assert!(git(&remote, &["for-each-ref", "refs/heads/main"]).is_empty());
    assert_eq!(git(&remote, &["rev-parse", "work"]), published);
    let envelope = git(&remote, &["show", "work:privacy/envelope.json"]);
    let envelope = PrivacyEnvelope::parse(envelope.as_bytes()).unwrap();
    publication_text::assert_public(&envelope.public_projection, &workspace);
    for snapshot in reviewed["snapshots"].as_array().unwrap() {
        let remote_envelope = git(
            &remote,
            &[
                "show",
                &format!(
                    "{}:privacy/envelope.json",
                    snapshot["commit"].as_str().unwrap()
                ),
            ],
        );
        let remote_envelope = PrivacyEnvelope::parse(remote_envelope.as_bytes()).unwrap();
        assert_eq!(snapshot["public"], remote_envelope.public_projection);
        assert_eq!(snapshot["snapshot_digest"], remote_envelope.snapshot_digest);
        assert_eq!(snapshot["policy_digest"], remote_envelope.policy_digest);
    }
    assert!(
        envelope.public_projection["session"]["log"]
            .as_str()
            .unwrap()
            .contains("<workspace>/src/main.rs")
    );
    assert_eq!(
        envelope
            .open_layer(&key)
            .unwrap()
            .session_bytes()
            .unwrap()
            .0
            .as_str(),
        log
    );
    assert_eq!(
        envelope
            .open_layer(&key)
            .unwrap()
            .session_bytes()
            .unwrap()
            .1
            .as_str(),
        log.split_inclusive('\n').next().unwrap()
    );
    let objects = git(&remote, &["rev-list", "--objects", "--all"]);
    for line in objects.lines() {
        let oid = line.split_whitespace().next().unwrap();
        assert_ne!(oid, source_head);
        let object = git(&remote, &["cat-file", "-p", oid]);
        for forbidden in [
            "PRIVATE_AUTHOR_MARKER",
            "PRIVATE_SHARED_FILE_MARKER",
            "PRIVATE_COMMIT_MARKER",
            "PRIVATE_SESSION_COMMIT",
            "PRIVATE_TOOL_OUTPUT_MARKER",
            "MANDATORY_TOOL_OUTPUT_MARKER",
            SESSION_SECRET,
            workspace.to_str().unwrap(),
        ] {
            assert!(
                !object.contains(forbidden),
                "plaintext reached remote object {oid}: {forbidden}"
            );
        }
    }
    let automatic = auto();
    assert_success(&automatic);
    assert!(
        !String::from_utf8_lossy(&automatic.stdout).contains("Outgoing public session preview")
    );
    assert_eq!(git(&remote, &["rev-parse", "work"]), published);
    assert_eq!(git(repo.root(), &["rev-parse", "HEAD"]), source_head);
    let extra = transcript::wrap_lines(
        &[
            json!({"type":"assistant","message":{"role":"assistant","content":"Incremental reply"}}),
            json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"continued-shell","name":"Bash","input":{"command":"CONTINUED_COMMAND"}}]}}),
            json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"continued-shell","content":{"stdout":"CONTINUED_STDOUT","exit_code":0}}]}}),
        ].into_iter().map(|record| format!("{record}\n")).collect::<String>(),
        "claude-code",
        &session,
    );
    let continued = format!("{log}{extra}");
    let saved = dictionary
        .protect_envelopes(&continued, &agit::domain::secret_filter::Matcher::empty())
        .unwrap()
        .text;
    storage::write_snapshot(repo.root(), &saved, &saved).unwrap();
    repo.git(&["add", "."]).unwrap();
    repo.git(&["commit", "-m", "PRIVATE_INCREMENTAL_COMMIT"])
        .unwrap();
    assert_success(&auto());
    assert_eq!(git(&remote, &["rev-parse", "work^"]), published);
    let envelope = git(&remote, &["show", "work:privacy/envelope.json"]);
    let checked = PrivacyEnvelope::parse(envelope.as_bytes()).unwrap();
    publication_text::assert_public(&checked.public_projection, &workspace);
    let public_log = checked.public_projection["session"]["log"]
        .as_str()
        .unwrap();
    assert!(public_log.contains("Incremental reply"));
    assert!(!public_log.contains("CONTINUED_COMMAND"));
    assert!(!public_log.contains("CONTINUED_STDOUT"));
    let incremental_head = git(&remote, &["rev-parse", "work"]);
    assert_success(&auto());
    assert_eq!(git(&remote, &["rev-parse", "work"]), incremental_head);
    assert_eq!(
        checked
            .open_layer(&key)
            .unwrap()
            .session_bytes()
            .unwrap()
            .1
            .as_str(),
        continued
    );
    assert_eq!(
        PrivacyEnvelope::parse(envelope.as_bytes())
            .unwrap()
            .open_layer(&key)
            .unwrap()
            .session_bytes()
            .unwrap()
            .0
            .as_str(),
        continued
    );

    let native_id = "cccccccc-0000-4000-8000-000000000003";
    let native = temp
        .path()
        .join(".claude/projects")
        .join(agit::adapter::claude_code::slug_for(&workspace))
        .join(format!("{native_id}.jsonl"));
    fs::create_dir_all(native.parent().unwrap()).unwrap();
    let append = |turn: usize| {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&native)
            .unwrap();
        for (role, suffix) in [("user", "u"), ("assistant", "a")] {
            writeln!(
                file,
                "{}",
                json!({
                    "type":role,"sessionId":native_id,"cwd":workspace,
                    "uuid":format!("{native_id}-{suffix}{turn}"),
                    "timestamp":format!("2026-09-22T00:00:{turn:02}.000Z"),
                    "message":{"role":role,"content":format!("Hook {role} turn {turn}")},
                })
            )
            .unwrap();
        }
    };
    append(1);
    repo.set_auto_push(Some(false)).unwrap();
    assert_success(&run(&[
        "import",
        native_id,
        "--from",
        "claude-code",
        "--independent",
        "--into",
        "alice/app@hook",
    ]));
    repo.set_auto_push(Some(true)).unwrap();
    let settle = || {
        let mut child = command(&["hooks", "settle"], false)
            .env("AGIT_SESSION", "alice/unrelated@other")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(json!({
            "session_id":native_id,"cwd":workspace,"transcript_path":native,"hook_event_name":"Stop",
        }).to_string().as_bytes()).unwrap();
        child.wait_with_output().unwrap()
    };
    append(2);
    let settled = settle();
    assert_success(&settled);
    assert!(
        !String::from_utf8_lossy(&settled.stderr).contains("automatic push failed"),
        "{settled:?}"
    );
    let hooked = git(&remote, &["rev-parse", "hook"]);
    assert_ne!(hooked, git(repo.root(), &["rev-parse", "hook"]));
    let envelope = git(&remote, &["show", "hook:privacy/envelope.json"]);
    let layer = PrivacyEnvelope::parse(envelope.as_bytes())
        .unwrap()
        .open_layer(&key)
        .unwrap();
    assert!(
        layer
            .session_bytes()
            .unwrap()
            .0
            .contains("Hook assistant turn 2")
    );
    let before_posts = hub.posts.load(Ordering::Acquire);
    assert_success(&settle());
    assert_eq!(hub.posts.load(Ordering::Acquire), before_posts);
    hub.deny.store(true, Ordering::Release);
    let local_before = git(repo.root(), &["rev-parse", "hook"]);
    append(3);
    let failed_upload = settle();
    assert_success(&failed_upload);
    assert!(String::from_utf8_lossy(&failed_upload.stderr).contains("automatic push failed"));
    assert_ne!(git(repo.root(), &["rev-parse", "hook"]), local_before);
    assert_eq!(git(&remote, &["rev-parse", "hook"]), hooked);
    assert_eq!(
        agit::domain::privacy_receipt::PublicationReceipt::load(&repo, "hook")
            .unwrap()
            .unwrap()
            .source,
        local_before
    );
    hub.deny.store(false, Ordering::Release);
    repo.set_auto_push(Some(false)).unwrap();
    assert_success(&run(&["config", "push.auto", "true"]));
    append(4);
    assert_success(&settle());
    assert_eq!(git(&remote, &["rev-parse", "hook"]), hooked);
    repo.set_auto_push(None).unwrap();
    append(5);
    assert_success(&settle());
    let inherited = git(&remote, &["rev-parse", "hook"]);
    assert_ne!(inherited, hooked);
    git(
        &remote,
        &["merge-base", "--is-ancestor", &hooked, &inherited],
    );

    let mut changed_mandatory = mandatory.clone();
    changed_mandatory["revision"] = json!("r2");
    fs::write(&mandatory_file, changed_mandatory.to_string()).unwrap();
    let before_posts = hub.posts.load(Ordering::Acquire);
    let before_refs = git(&remote, &["for-each-ref"]);
    let stale = auto();
    assert!(!stale.status.success());
    assert!(String::from_utf8_lossy(&stale.stderr).contains("renewed confirmation"));
    assert_eq!(hub.posts.load(Ordering::Acquire), before_posts);
    assert_eq!(git(&remote, &["for-each-ref"]), before_refs);
    fs::write(&mandatory_file, mandatory.to_string()).unwrap();

    hub.source_revision.store(2, Ordering::Release);
    let stale = auto();
    assert!(!stale.status.success());
    assert!(String::from_utf8_lossy(&stale.stderr).contains("renewed confirmation"));
    assert_eq!(hub.posts.load(Ordering::Acquire), before_posts);
    assert_eq!(git(&remote, &["for-each-ref"]), before_refs);
    hub.source_revision.store(1, Ordering::Release);

    let mut policy = PrivacyPolicy::load(&repo).unwrap();
    policy.exclude.push("new-private/**".into());
    policy.save(&repo).unwrap();
    let before_posts = hub.posts.load(Ordering::Acquire);
    let before_refs = git(&remote, &["for-each-ref"]);
    let stale = auto();
    assert!(!stale.status.success());
    assert!(
        String::from_utf8_lossy(&stale.stderr).contains("renewed confirmation"),
        "{stale:?}"
    );
    assert_eq!(hub.posts.load(Ordering::Acquire), before_posts);
    assert_eq!(git(&remote, &["for-each-ref"]), before_refs);

    let new_home = temp.path().join("new-device");
    let new_workspace = temp.path().join("new-workspace");
    fs::create_dir_all(new_workspace.join("src")).unwrap();
    startup_cache::seed(&new_home);
    fs::create_dir_all(new_home.join("credentials")).unwrap();
    let credential = format!("{}.json", config::hub_host_key(&hub.base).unwrap());
    fs::copy(
        home.join("credentials").join(&credential),
        new_home.join("credentials").join(credential),
    )
    .unwrap();
    let from_new_device = |args: &[&str]| {
        command(args, false)
            .env("AGIT_HOME", &new_home)
            .env("HOME", &new_home)
            .env("USERPROFILE", &new_home)
            .current_dir(&new_workspace)
            .output()
            .unwrap()
    };
    assert_success(&from_new_device(&[
        "--yes",
        "clone",
        "alice/app@work",
        "--no-bind",
    ]));
    assert_success(&from_new_device(&["--yes", "push", "alice/app@work"]));
    assert_eq!(git(&remote, &["for-each-ref"]), before_refs);

    let clone = Repo::at(new_home.join("repos/alice/app"));
    let new_dictionary =
        agit::domain::secret_filter::RepositoryDictionary::open(clone.root()).unwrap();
    assert!(
        new_dictionary.review().unwrap().is_empty(),
        "policy synchronization must not recover private dictionary records"
    );
    git(clone.root(), &["config", "user.name", "New Device"]);
    git(
        clone.root(),
        &["config", "user.email", "device@example.invalid"],
    );
    assert_success(&from_new_device(&[
        "privacy",
        "policy",
        "set-workspace",
        new_workspace.to_str().unwrap(),
        "--repo",
        "alice/app",
    ]));
    let prior = git(clone.root(), &["rev-parse", "work"]);
    let (status, output) = privacy_password_terminal::terminal_password(
        &new_home,
        &new_home,
        &new_workspace,
        &hub.base,
        &[
            "privacy",
            "unlock",
            "alice/app@work",
            "--workspace",
            new_workspace.to_str().unwrap(),
        ],
        "synthetic-repository-password",
    );
    assert!(status.success(), "{output}");
    assert!(
        !new_dictionary.review().unwrap().is_empty(),
        "unlock must restore private dictionary records"
    );
    let cache = clone
        .common_dir()
        .unwrap()
        .join("agit/privacy-recovery")
        .join(&prior);
    assert_eq!(
        storage::materialize_worktree(&cache, meta::LOG_FILE).unwrap(),
        continued
    );
    assert!(!cache.join("private.txt").exists());
    assert_success(&from_new_device(&[
        "resume",
        "alice/app@work",
        "--as",
        "codex",
        "--no-launch",
        "--cwd",
        new_workspace.to_str().unwrap(),
    ]));
    let runtime_file = walkdir::WalkDir::new(new_home.join(".codex/sessions"))
        .into_iter()
        .filter_map(Result::ok)
        .find(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
        .expect("recovery must prepare a runtime transcript")
        .into_path();
    let active = fs::read_to_string(&runtime_file).unwrap();
    assert!(active.contains(SESSION_SECRET));
    assert!(active.contains("PRIVATE_TOOL_OUTPUT_MARKER"));
    let records: Vec<serde_json::Value> = active
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let output = records
        .iter()
        .find(|record| {
            record["payload"]["type"] == "function_call_output"
                && record["payload"]["output"]
                    .as_str()
                    .is_some_and(|output| output.contains("PRIVATE_TOOL_OUTPUT_MARKER"))
        })
        .expect("recovery must retain the native tool output");
    assert!(
        records
            .iter()
            .any(|record| record["payload"]["type"] == "function_call"
                && record["payload"]["call_id"] == output["payload"]["call_id"]
                && record["payload"]["arguments"]
                    .as_str()
                    .is_some_and(|arguments| arguments.contains("private.txt"))),
        "recovery must preserve the tool call paired with its original output"
    );
    let mut native = fs::OpenOptions::new()
        .append(true)
        .open(&runtime_file)
        .unwrap();
    for record in [
        json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":format!("Continue with {SESSION_SECRET}")}]}}),
        json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"New device reply"}]}}),
    ] {
        writeln!(native, "{record}").unwrap();
    }
    drop(native);
    assert_success(&from_new_device(&["commit", "alice/app@work"]));
    let local = git(clone.root(), &["rev-parse", "work"]);
    assert_ne!(local, prior);
    assert_success(&from_new_device(&["--yes", "push", "alice/app@work"]));
    let next = git(&remote, &["rev-parse", "work"]);
    git(&remote, &["merge-base", "--is-ancestor", &prior, &next]);
    let previous_metadata: meta::Meta =
        serde_json::from_str(&git(&remote, &["show", &format!("{prior}:{}", meta::FILE)])).unwrap();
    let next_metadata: meta::Meta =
        serde_json::from_str(&git(&remote, &["show", &format!("{next}:{}", meta::FILE)])).unwrap();
    assert_eq!(next_metadata.session, previous_metadata.session);
    let envelope =
        PrivacyEnvelope::parse(git(&remote, &["show", "work:privacy/envelope.json"]).as_bytes())
            .unwrap();
    let layer = envelope.open_layer(&key).unwrap();
    let (private_log, _) = layer.session_bytes().unwrap();
    assert!(private_log.starts_with(&continued));
    assert!(private_log.contains("New device reply"));
    assert!(layer.protected_values.contains(SESSION_SECRET));
    assert!(
        envelope.public_projection["session"]["log"]
            .as_str()
            .unwrap()
            .contains("New device reply")
    );
    for line in git(&remote, &["rev-list", "--objects", "--all"]).lines() {
        let oid = line.split_whitespace().next().unwrap();
        let object = git(&remote, &["cat-file", "-p", oid]);
        for forbidden in [
            SESSION_SECRET,
            "PRIVATE_TOOL_OUTPUT_MARKER",
            "PRIVATE_SHARED_FILE_MARKER",
            new_workspace.to_str().unwrap(),
        ] {
            assert!(
                !object.contains(forbidden),
                "plaintext reached remote object {oid}: {forbidden}"
            );
        }
    }
    assert_success(&from_new_device(&["--yes", "push", "alice/app@work"]));
    assert_eq!(git(&remote, &["rev-parse", "work"]), next);
}
