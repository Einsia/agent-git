#![cfg(feature = "cli")]

//! A real Git receiver must see projected ancestry while encrypted snapshots retain the originals.

use agit::domain::{meta, privacy_envelope::PrivacyEnvelope, repo::Repo, storage, transcript};
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

#[path = "support/publication_http.rs"]
mod publication_http;
#[path = "support/startup_cache.rs"]
mod startup_cache;

const AGENT_ID: &str = "9f2c3b53-7fe0-412f-b62a-bf68a6845ce7";
const TRANSPORT_USER_AGENT: &str = "synthetic-publisher-transport";

struct Hub {
    base: String,
    posts: Arc<AtomicUsize>,
    manifests: Arc<Mutex<Vec<serde_json::Value>>>,
    registrations: Arc<AtomicUsize>,
    deny: Arc<AtomicBool>,
    /// Refuses a receive that updates only tags, as a Hub that closed them would.
    refuse_tags: Arc<AtomicBool>,
    unsupported_policy: Arc<AtomicBool>,
    corrupt_receipt: Arc<AtomicBool>,
    creations: Arc<AtomicUsize>,
    viewing_public: Arc<Mutex<String>>,
    visibility: Arc<Mutex<&'static str>>,
    key_version: Arc<AtomicUsize>,
    key_requests: Arc<AtomicUsize>,
    /// Every publication preview request body, in arrival order.
    previews: Arc<Mutex<Vec<serde_json::Value>>>,
    /// A ref the next strategy registration moves before answering: `(name, oid)`.
    move_on_strategy: Arc<Mutex<Option<(String, String)>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Hub {
    fn start(root: &Path, viewing_key: [u8; 32]) -> Self {
        let key = SecretKey::from(viewing_key);
        let public = STANDARD.encode(key.public_key().as_bytes());
        let viewing_public = Arc::new(Mutex::new(public.clone()));
        let served_public = Arc::clone(&viewing_public);
        let key_version = Arc::new(AtomicUsize::new(1));
        let served_version = Arc::clone(&key_version);
        let key_requests = Arc::new(AtomicUsize::new(0));
        let requested_keys = Arc::clone(&key_requests);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let url = format!("{base}/alice/app.git");
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let posts = Arc::new(AtomicUsize::new(0));
        let received = Arc::clone(&posts);
        let manifests = Arc::new(Mutex::new(Vec::new()));
        let recorded_manifests = Arc::clone(&manifests);
        let registrations = Arc::new(AtomicUsize::new(0));
        let registered = Arc::clone(&registrations);
        let deny = Arc::new(AtomicBool::new(false));
        let denied = Arc::clone(&deny);
        let refuse_tags = Arc::new(AtomicBool::new(false));
        let refusing_tags = Arc::clone(&refuse_tags);
        let unsupported_policy = Arc::new(AtomicBool::new(false));
        let unsupported = Arc::clone(&unsupported_policy);
        let corrupt_receipt = Arc::new(AtomicBool::new(false));
        let corrupting = Arc::clone(&corrupt_receipt);
        let confirmations = Arc::new(AtomicUsize::new(0));
        let confirmed = Arc::clone(&confirmations);
        let creations = Arc::new(AtomicUsize::new(0));
        let created = Arc::clone(&creations);
        let previews = Arc::new(Mutex::new(Vec::new()));
        let recorded_previews = Arc::clone(&previews);
        let move_on_strategy = Arc::new(Mutex::new(None::<(String, String)>));
        let strategy_move = Arc::clone(&move_on_strategy);
        let visibility = Arc::new(Mutex::new("private"));
        let served_visibility = Arc::clone(&visibility);
        let resolver_hub = base.clone();
        let root = root.to_owned();
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(300);
            let mut strategy = serde_json::Value::Null;
            let mut preview = serde_json::Value::Null;
            let mut pending_receipt = None;
            let mandatory = r#"{"version":1,"source":"repository","require_envelope":false,"require_session_config":true,"publication_format_version":2,"protected_paths":[]}"#;
            let mandatory_digest =
                agit::domain::privacy_envelope::digest_bytes(mandatory.as_bytes());
            let mut sessions = std::collections::BTreeMap::<String, serde_json::Value>::new();
            let mut policies = std::collections::BTreeMap::<
                String,
                agit::domain::session_grants::SessionGrantPolicy,
            >::new();
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
                let mut compressed = false;
                let mut user_agent = String::new();
                let mut authenticated = false;
                let mut collaborator_account = false;
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
                    compressed |= lower.trim() == "content-encoding: gzip";
                    if let Some(value) = lower.strip_prefix("user-agent:") {
                        user_agent = value.trim().to_owned();
                    }
                    assert!(
                        !lower.starts_with("transfer-encoding:"),
                        "fixture body must be bounded"
                    );
                    authenticated |= lower.trim() == "authorization: bearer synthetic-access";
                    collaborator_account |=
                        lower.trim() == "authorization: bearer synthetic-collaborator";
                    if let Some(value) = lower.strip_prefix("x-agentgit-privacy-preview-id:") {
                        receipt_id = Some(value.trim().to_owned());
                    }
                    if let Some(value) = lower.strip_prefix("x-agentgit-privacy-receiver-scope:") {
                        receiver_scope = Some(value.trim().to_owned());
                    }
                }
                assert!(
                    authenticated || collaborator_account || target == "/api/auth/refresh",
                    "{first}"
                );
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                drop(reader);
                if compressed {
                    let mut decoded = Vec::new();
                    flate2::read::GzDecoder::new(body.as_slice())
                        .read_to_end(&mut decoded)
                        .unwrap();
                    body = decoded;
                }
                if target == "/api/agents/alice/app" && !root.join("alice/app.git").is_dir() {
                    let body = r#"{"error":"repository is unavailable"}"#;
                    write!(stream, "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                    continue;
                }
                let contains =
                    |needle: &[u8]| body.windows(needle.len()).any(|value| value == needle);
                if target.ends_with("/git-receive-pack")
                    && refusing_tags.load(Ordering::Acquire)
                    && contains(b"refs/tags/")
                    && !contains(b"refs/heads/")
                {
                    write!(
                        stream,
                        "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
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
                if target.starts_with("/api/me/privacy/") {
                    write!(stream, "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                    continue;
                }
                let json = match (method, target) {
                    ("POST", path)
                        if path.starts_with("/api/agents/alice/app/sessions/")
                            && path.ends_with("/reuses") =>
                    {
                        Some(json!({"ok":true}))
                    }
                    (method, path)
                        if path.starts_with("/api/agents/alice/app/sessions/")
                            && path.ends_with("/privacy-config") =>
                    {
                        let session = path
                            .strip_prefix("/api/agents/alice/app/sessions/")
                            .unwrap()
                            .strip_suffix("/privacy-config")
                            .unwrap();
                        if method == "POST" {
                            registered.fetch_add(1, Ordering::AcqRel);
                            let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
                            assert_eq!(request["expected_repository_id"], AGENT_ID);
                            let config = json!({"repository_id":AGENT_ID,"session_id":session,"privacy":request["privacy"],"revision":1,"created_at":"2026-01-01T00:00:00Z"});
                            policies.entry(session.into()).or_insert_with(|| {
                                agit::domain::session_grants::SessionGrantPolicy {
                                    agent_id: AGENT_ID.into(),
                                    session_id: session.into(),
                                    grant_scope: agit::domain::session_grants::GrantScope::Session,
                                    policy_revision: 1,
                                    grant_revision: 1,
                                }
                            });
                            if let Some(previous) = sessions.insert(session.into(), config.clone())
                            {
                                assert_eq!(previous, config);
                            }
                        }
                        if session == format!("agit-{}", "b".repeat(40)) {
                            sessions.entry(session.into()).or_insert_with(|| json!({"repository_id":AGENT_ID,"session_id":session,"privacy":{"scheme":"plain","hub":null,"creator":null,"owner":null,"owner_source":"unresolved"},"revision":1,"created_at":"2026-01-01T00:00:00Z"}));
                        }
                        let Some(config) = sessions.get(session) else {
                            let body = r#"{"kind":"session_privacy_unregistered","error":"register this session"}"#;
                            write!(stream, "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                            continue;
                        };
                        let key = if config["privacy"]["scheme"] == "session_owner_v2" {
                            json!({"account_id":config["privacy"]["owner"]["id"],"key_version":served_version.load(Ordering::Acquire),"public_key_algorithm":"x25519","public_key":served_public.lock().unwrap().clone()})
                        } else {
                            serde_json::Value::Null
                        };
                        Some(json!({"config":config,"owner_key":key}))
                    }
                    (method, path)
                        if path.starts_with("/api/agents/alice/app/sessions/")
                            && path.ends_with("/original-grant-policy") =>
                    {
                        use agit::domain::session_grants::{GrantPolicyMutation, GrantScope};
                        if method == "POST" && unsupported.load(Ordering::Acquire) {
                            let body = r#"{"kind":"unsupported","error":"Enabling session-level authorization after disabling it is unsupported."}"#;
                            write!(stream, "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                            continue;
                        }
                        let session = path
                            .strip_prefix("/api/agents/alice/app/sessions/")
                            .unwrap()
                            .strip_suffix("/original-grant-policy")
                            .unwrap();
                        let policy = policies
                            .get_mut(session)
                            .expect("policy requires registration");
                        if method == "GET" {
                            Some(json!(policy))
                        } else {
                            let request: GrantPolicyMutation =
                                serde_json::from_slice(&body).unwrap();
                            request.validate().unwrap();
                            assert_eq!(request.target, policy.target().unwrap());
                            assert_eq!(request.expected_policy_revision, policy.policy_revision);
                            assert_eq!(request.expected_grant_revision, policy.grant_revision);
                            assert_eq!(request.grant_scope, GrantScope::Commit);
                            if policy.grant_scope == GrantScope::Session {
                                policy.grant_scope = GrantScope::Commit;
                                policy.policy_revision += 1;
                                policy.grant_revision += 1;
                            }
                            Some(
                                json!({"policy":policy,"converted_recipients":0,"converted_snapshots":0}),
                            )
                        }
                    }
                    ("POST", "/api/me/session-key/ensure") => {
                        requested_keys.fetch_add(1, Ordering::Acquire);
                        Some(
                            json!({"ready":true,"key":{"account_id":"account-1","key_version":served_version.load(Ordering::Acquire),"public_key_algorithm":"x25519","public_key":served_public.lock().unwrap().clone()}}),
                        )
                    }
                    ("POST", "/api/auth/refresh") => {
                        assert_eq!(
                            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["refresh_token"],
                            "synthetic-refresh"
                        );
                        Some(
                            json!({"account_id":"account-1","username":"alice","access_token":"synthetic-access","refresh_token":"synthetic-refresh","access_expires_at":"2099-01-01T00:00:00Z","refresh_expires_at":"2099-01-01T00:00:00Z"}),
                        )
                    }
                    ("GET", "/api/auth/me") => Some(if collaborator_account {
                        json!({"account_id":"account-2","username":"bob"})
                    } else {
                        json!({"account_id":"account-1","username":"alice"})
                    }),
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
                            json!({"agent_id":AGENT_ID,"owner":"alice","name":"app","push_url":url,"web_url":format!("{resolver_hub}/alice/app")}),
                        )
                    }
                    ("PUT", "/api/agents/alice/app/privacy/strategy") => {
                        if let Some((name, oid)) = strategy_move.lock().unwrap().take() {
                            git(&root.join("alice/app.git"), &["update-ref", &name, &oid]);
                        }
                        strategy = serde_json::from_slice(&body).unwrap();
                        assert_eq!(strategy["publication_format_version"], 2);
                        assert_eq!(
                            strategy["summary"],
                            json!({"session_modes":["plain","session_owner_v2"]})
                        );
                        strategy["agent_id"] = json!(AGENT_ID);
                        strategy["mandatory_policy"] = serde_json::from_str(mandatory).unwrap();
                        strategy["mandatory_policy_digest"] = json!(mandatory_digest);
                        Some(strategy.clone())
                    }
                    ("POST", "/api/agents/alice/app/privacy/publication/preview") => {
                        preview = serde_json::from_slice(&body).unwrap();
                        recorded_previews.lock().unwrap().push(preview.clone());
                        assert_eq!(preview["policy_digest"], strategy["policy_digest"]);
                        assert_eq!(
                            preview["expected_refs_digest"],
                            remote_refs_digest(&root.join("alice/app.git"))
                        );
                        assert_eq!(preview["envelope_format_version"], 2);
                        let manifest: agit::domain::session_privacy::SessionPublicationManifest =
                            serde_json::from_value(preview["manifest"].clone()).unwrap();
                        manifest.validate().unwrap();
                        recorded_manifests
                            .lock()
                            .unwrap()
                            .push(preview["manifest"].clone());
                        assert_eq!(
                            preview["snapshot_digest"],
                            agit::domain::privacy_envelope::digest_bytes(
                                &serde_json::to_vec(&manifest).unwrap()
                            )
                        );
                        assert_eq!(
                            preview["receiver_scope"],
                            format!("sessions:{}:{AGENT_ID}", served_visibility.lock().unwrap())
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
                        json!({"agent_id":AGENT_ID,"owner":"alice","name":"app","clone_url":url,"visibility":*served_visibility.lock().unwrap()}),
                    ),
                    // A publication plans against the limits the Hub publishes.
                    ("GET", "/api/hub/config") => Some(json!({
                        "mode":"live",
                        "limits":{"push":{"max_new_commits":4096,"max_request_bytes":2147483648u64}}
                    })),
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
                        assert_eq!(user_agent, TRANSPORT_USER_AGENT);
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
            manifests,
            registrations,
            deny,
            refuse_tags,
            unsupported_policy,
            corrupt_receipt,
            creations,
            viewing_public,
            visibility,
            key_version,
            key_requests,
            previews,
            move_on_strategy,
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

/// Creation and Git receive require consent and exact receipts; retries preserve session authority.
/// Projected Git uploads use the publisher's configured transport preferences.
/// Explicit plain selection survives renames and retries without changing source history or other sessions.
#[test]
fn session_push_registers_owner_and_preserves_mixed_history_and_immutable_retries() {
    use agit::domain::session_privacy::{AccountIdentity, CommitterAttribution, SessionPrivacy};
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    startup_cache::seed(&home);
    let remote_root = temp.path().join("remote");
    let remote = remote_root.join("alice/app.git");
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
    let run_home = |args: &[&str], automatic: bool, selected_home: &Path| {
        let environment = git_command(temp.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear();
        for (name, value) in environment.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command
            .current_dir(temp.path())
            .env("AGIT_HOME", selected_home)
            .env("CODEX_HOME", selected_home.join("codex"))
            .env("CLAUDE_CONFIG_DIR", selected_home.join("claude"))
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
        if automatic {
            command.env("AGIT_AUTO_PUSH", "1");
        }
        command.output().unwrap()
    };
    let run = |args: &[&str], automatic: bool| run_home(args, automatic, &home);
    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    repo.git(&["config", "http.userAgent", TRANSPORT_USER_AGENT])
        .unwrap();
    repo.git(&["config", "user.name", "Source Writer"]).unwrap();
    repo.git(&["config", "user.email", "source@example.invalid"])
        .unwrap();
    repo.git(&["config", "tag.gpgsign", "false"]).unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    fs::write(repo.root().join("README.md"), "Accepted plain file").unwrap();
    repo.add_all().unwrap();
    repo.commit("Accepted file line").unwrap();
    let base = git(repo.root(), &["rev-parse", "HEAD"]);
    git(repo.root(), &["tag", "file-release"]);
    git(
        repo.root(),
        &["tag", "-a", "file-notes", "-m", "Reviewed file release"],
    );
    let file_tag = git(repo.root(), &["rev-parse", "refs/tags/file-notes"]);
    assert_success(&run(
        &["new", "alice/app", "-b", "unencrypted", "--no-launch"],
        false,
    ));
    let plain_birth = git(repo.root(), &["rev-parse", "refs/heads/unencrypted"]);
    let chosen_meta = meta::read_at_ref_result(&repo, &plain_birth)
        .unwrap()
        .unwrap();
    let mode_args = ["privacy", "mode", "alice/app@unencrypted", "encrypted"];
    assert!(!run(&mode_args, false).status.success());
    let unchanged = run(
        &["--json", "privacy", "mode", "alice/app@unencrypted"],
        false,
    );
    assert_success(&unchanged);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&unchanged.stdout).unwrap()["result"]["value"]
            ["local_mode"],
        "plain"
    );
    assert_eq!(
        chosen_meta.session_privacy.as_ref().unwrap().scheme,
        agit::domain::session_privacy::SessionScheme::Plain
    );
    let mode = run(
        &["--json", "privacy", "status", "alice/app@unencrypted"],
        false,
    );
    assert_success(&mode);
    let mode: serde_json::Value = serde_json::from_slice(&mode.stdout).unwrap();
    assert_eq!(mode["result"]["value"]["local_mode"], "plain");
    assert_eq!(
        mode["result"]["value"]["registered_mode"],
        serde_json::Value::Null
    );
    assert!(!remote.exists());
    let dry = run(&["push", "alice/app@main", "--dry-run"], false);
    assert!(!dry.status.success(), "{dry:?}");
    assert!(String::from_utf8_lossy(&dry.stderr).contains("Publication processing time:"));
    for flags in [vec!["--quiet"], vec!["--json"]] {
        let mut args = flags;
        args.extend(["push", "alice/app@main", "--dry-run"]);
        let output = run(&args, false);
        assert!(!output.status.success());
        for bytes in [&output.stdout, &output.stderr] {
            assert!(!String::from_utf8_lossy(bytes).contains("Publication processing time:"));
        }
        if args.contains(&"--json") {
            let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(value["ok"], false);
        }
    }
    assert_eq!(hub.creations.load(Ordering::Acquire), 0);
    assert!(!remote.exists());
    let unconfirmed = run(&["push", "alice/app@main"], false);
    assert!(!unconfirmed.status.success(), "{unconfirmed:?}");
    assert_eq!(hub.creations.load(Ordering::Acquire), 0);
    hub.corrupt_receipt.store(true, Ordering::Release);
    let create = ["--yes", "push", "alice/app@main"];
    let refused = run(&create, false);
    assert!(!refused.status.success(), "{refused:?}");
    let progress = String::from_utf8_lossy(&refused.stderr);
    assert!(
        progress
            .lines()
            .any(|line| line.starts_with("Getting receipt for batch") && line.contains("stopped")),
        "{progress}"
    );
    assert!(
        !progress.contains("Uploading and awaiting acknowledgement"),
        "{progress}"
    );
    assert_eq!(hub.creations.load(Ordering::Acquire), 1);
    assert_eq!(hub.posts.load(Ordering::Acquire), 0);
    assert!(git(&remote, &["for-each-ref"]).is_empty());
    hub.corrupt_receipt.store(false, Ordering::Release);
    let published = run(&create, false);
    assert_success(&published);
    let progress = String::from_utf8_lossy(&published.stderr);
    for stage in [
        "Preparing publication snapshots",
        "Writing publication preview",
        "Rechecking source and destination before upload",
        "Uploading and awaiting acknowledgement",
        "Recording publication acknowledgements",
    ] {
        assert!(
            progress
                .lines()
                .any(|line| line.starts_with(stage) && line.contains("done (")),
            "{stage}: {progress}"
        );
        assert!(!String::from_utf8_lossy(&published.stdout).contains(stage));
    }
    assert_eq!(hub.creations.load(Ordering::Acquire), 1);
    assert!(hub.posts.load(Ordering::Acquire) > 0);
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/main"]), base);
    assert_eq!(git(&remote, &["rev-parse", "refs/tags/file-release"]), base);
    assert_eq!(
        git(&remote, &["rev-parse", "refs/tags/file-notes"]),
        file_tag
    );
    git(repo.root(), &["tag", "-d", "file-release"]);
    hub.posts.store(0, Ordering::Release);
    let save = |metadata: &meta::Meta, content: &str| {
        let log = transcript::wrap_lines(
            &format!(
                "{}\n",
                json!({"type":"user","message":{"role":"user","content":content}})
            ),
            "claude-code",
            &metadata.session,
        );
        storage::write_snapshot(repo.root(), &log, &log).unwrap();
        meta::write(repo.root(), metadata).unwrap();
        repo.add_all().unwrap();
        repo.git(&[
            "-c",
            "commit.gpgsign=false",
            "-c",
            "i18n.commitEncoding=ISO-8859-1",
            "commit",
            "--author=Original Author <author@example.invalid>",
            "--date=1700000000 +0530",
            "-m",
            "Saved session snapshot\n\nReviewed by the original writer.",
        ])
        .unwrap();
        git(repo.root(), &["rev-parse", "HEAD"])
    };
    repo.git(&["checkout", "-b", "legacy"]).unwrap();
    let plain_meta = meta::Meta::new(
        format!("agit-{}", "b".repeat(40)),
        "claude-code".into(),
        String::new(),
    );
    let plain = save(&plain_meta, "Registered plain conversation");
    git(repo.root(), &["tag", "plain-release"]);
    git(
        repo.root(),
        &["tag", "-a", "inner", "-m", "Reviewed inner release"],
    );
    let inner = git(repo.root(), &["rev-parse", "refs/tags/inner"]);
    git(
        repo.root(),
        &[
            "tag",
            "-a",
            "release-1",
            "inner",
            "-m",
            "Reviewed outer release",
        ],
    );
    let outer = git(repo.root(), &["rev-parse", "refs/tags/release-1"]);
    git(repo.root(), &["tag", "-d", "inner"]);
    repo.git(&["checkout", "-b", "work", &base]).unwrap();
    let mut metadata = meta::Meta::new(
        format!("agit-{}", "a".repeat(40)),
        "claude-code".into(),
        String::new(),
    );
    metadata.session_privacy = Some(SessionPrivacy {
        scheme: agit::domain::session_privacy::SessionScheme::SessionOwnerV2,
        ..SessionPrivacy::new(Some(
            AccountIdentity::new(&hub.base, "session-owner").unwrap(),
        ))
        .unwrap()
    });
    metadata.committer_account = Some(AccountIdentity::new(&hub.base, "account-1").unwrap());
    fs::write(
        repo.root().join("native-private.txt"),
        "PRIVATE_NATIVE_FILE",
    )
    .unwrap();
    let original = save(&metadata, "Private transcript ORIGINAL_CONTEXT");
    let args = ["--yes", "push", "alice/app", "-b", "work", "-b", "legacy"];
    git(repo.root(), &["tag", &format!("agit-{original}")]);
    hub.deny.store(true, Ordering::Release);
    assert!(!run(&args, false).status.success());
    assert_eq!(hub.posts.load(Ordering::Acquire), 0);
    hub.deny.store(false, Ordering::Release);
    let dry = run(&["push", "alice/app@work", "--dry-run"], false);
    assert!(!dry.status.success(), "{dry:?}");
    assert!(
        String::from_utf8_lossy(&dry.stderr).contains("registered before a dry-run"),
        "{dry:?}"
    );
    assert_eq!(hub.posts.load(Ordering::Acquire), 0);
    git(repo.root(), &["tag", "private-release"]);
    let refused_tag = run(&args, false);
    assert!(!refused_tag.status.success(), "{refused_tag:?}");
    assert!(
        String::from_utf8_lossy(&refused_tag.stderr)
            .contains("points to rewritten publication history"),
        "{refused_tag:?}"
    );
    assert_eq!(hub.posts.load(Ordering::Acquire), 0);
    git(repo.root(), &["tag", "-d", "private-release"]);
    hub.corrupt_receipt.store(true, Ordering::Release);
    let mut reviewed_args = args.to_vec();
    reviewed_args.push("--show-preview");
    let refused = run(&reviewed_args, false);
    assert!(!refused.status.success(), "{refused:?}");
    let displayed = String::from_utf8_lossy(&refused.stdout);
    assert!(displayed.contains("Registered plain conversation"));
    // The destination is confirmed by a person, so it reads as a sentence, not a JSON object.
    assert!(
        displayed.lines().any(|line| line
            == format!(
                "Publication destination: alice/app on {} (private, existing repository) as alice",
                hub.base
            )),
        "{displayed}"
    );
    assert!(!displayed.contains("PRIVATE_NATIVE_FILE"));
    assert!(
        String::from_utf8_lossy(&refused.stdout).contains("receipt differs"),
        "{refused:?}"
    );
    assert_eq!(hub.posts.load(Ordering::Acquire), 0);
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/main"]), base);
    let path = String::from_utf8_lossy(&refused.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("Privacy preview: "))
        .unwrap()
        .to_owned();
    let reviewed: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    let tags = reviewed["plan"]["tags"].as_array().unwrap();
    assert!(
        tags.iter()
            .all(|tag| tag["name"] != "refs/tags/file-release")
    );
    assert!(
        tags.iter()
            .any(|tag| tag["name"] == "refs/tags/plain-release" && tag["oid"] == plain)
    );
    assert!(
        tags.iter()
            .any(|tag| tag["name"] == "refs/tags/release-1" && tag["oid"] == outer)
    );
    let objects = reviewed["plan"]["tag_objects"].as_array().unwrap();
    assert!(objects.contains(&json!(inner)));
    assert!(objects.contains(&json!(outer)));
    let planned = reviewed["plan"]["heads"]
        .as_array()
        .unwrap()
        .iter()
        .find(|head| head["name"] == "refs/heads/work")
        .unwrap()["oid"]
        .as_str()
        .unwrap()
        .to_owned();
    let source_commit = git(repo.root(), &["cat-file", "commit", &original]);
    let committer = source_commit
        .lines()
        .find_map(|line| line.strip_prefix("committer "))
        .unwrap();
    let snapshot = reviewed["snapshots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|snapshot| snapshot["commit"] == planned)
        .unwrap();
    assert_eq!(
        snapshot["git"],
        json!({
            "author": "Original Author <author@example.invalid> 1700000000 +0530",
            "committer": committer,
            "encoding": "ISO-8859-1",
            "message": "Saved session snapshot\n\nReviewed by the original writer.\n",
        })
    );
    for field in snapshot["git"].as_object().unwrap().values() {
        for line in field
            .as_str()
            .unwrap()
            .lines()
            .filter(|line| !line.is_empty())
        {
            assert!(displayed.contains(line), "missing preview content: {line}");
        }
    }
    let rotated = SecretKey::from([26; 32]);
    *hub.viewing_public.lock().unwrap() = STANDARD.encode(rotated.public_key().as_bytes());
    hub.key_version.store(2, Ordering::Release);
    hub.corrupt_receipt.store(false, Ordering::Release);
    assert_success(&run(&args, false));
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/work"]), planned);
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/legacy"]), plain);
    assert_eq!(
        git(&remote, &["rev-parse", "refs/tags/plain-release"]),
        plain
    );
    assert_eq!(git(&remote, &["rev-parse", "refs/tags/release-1"]), outer);
    for oid in [&inner, &outer] {
        assert_eq!(
            git(&remote, &["cat-file", "tag", oid]),
            git(repo.root(), &["cat-file", "tag", oid])
        );
    }
    assert!(
        git(
            &remote,
            &["for-each-ref", "--format=%(refname)", "refs/tags/"]
        )
        .lines()
        .all(|name| name != format!("refs/tags/agit-{original}"))
    );
    assert_eq!(git(&remote, &["rev-parse", &format!("{planned}^")]), base);
    let attribution = "--format=%an%n%ae%n%aI%n%cn%n%ce%n%cI%n%B";
    assert_eq!(
        git(&remote, &["show", "--no-patch", attribution, &planned]),
        git(repo.root(), &["show", "--no-patch", attribution, &original])
    );
    let envelope = |commit: &str| {
        PrivacyEnvelope::parse(
            git(
                &remote,
                &["show", &format!("{commit}:privacy/envelope.json")],
            )
            .as_bytes(),
        )
        .unwrap()
    };
    let first = envelope(&planned);
    assert!(first.open_layer(&key).is_ok());
    assert!(first.open_layer(&rotated).is_err());
    let binding = first.session_binding.as_ref().unwrap();
    assert_eq!(binding.owner_key.account_id, "session-owner");
    assert_eq!(
        binding.committer,
        CommitterAttribution::Account {
            account_id: "account-1".into()
        }
    );
    let tree = git(&remote, &["ls-tree", "-r", "--name-only", &planned]);
    assert!(!tree.contains("native-private.txt"));
    assert!(
        !git_command(&remote)
            .args(["cat-file", "-e", &original])
            .output()
            .unwrap()
            .status
            .success()
    );
    let pushed = hub.posts.load(Ordering::Acquire);
    assert_success(&run(&args, false));
    assert_eq!(hub.posts.load(Ordering::Acquire), pushed);
    repo.set_auto_push(Some(true)).unwrap();
    let continued = save(&metadata, "Private continuation COMPLETE_CONTEXT");
    assert_success(&run(&["push", "alice/app@work"], true));
    let next = git(&remote, &["rev-parse", "refs/heads/work"]);
    assert_eq!(git(&remote, &["rev-parse", &format!("{next}^")]), planned);
    let next_envelope = envelope(&next);
    let next_binding = next_envelope.session_binding.as_ref().unwrap();
    assert_eq!(next_binding.owner_key.account_id, "session-owner");
    assert_eq!(next_binding.owner_key.key_version, 2);
    assert_eq!(
        next_binding.published_session_id,
        binding.published_session_id
    );
    assert_eq!(next_binding.source_commit, continued);
    assert!(next_envelope.open_layer(&rotated).is_ok());
    let receipt = agit::domain::privacy_receipt::PublicationReceipt::load(&repo, "work")
        .unwrap()
        .unwrap();
    assert_eq!(receipt.session_snapshot.unwrap().binding, *next_binding);

    // Policy configuration precedes publication without becoming immutable encryption metadata.
    assert_success(&run(
        &["new", "alice/app", "-b", "prepublish", "--no-launch"],
        false,
    ));
    assert_success(&run(
        &[
            "--yes",
            "privacy",
            "mode",
            "alice/app@prepublish",
            "encrypted",
        ],
        false,
    ));
    let before_policy = git(repo.root(), &["rev-parse", "refs/heads/prepublish"]);
    let created = meta::read_at_ref_result(&repo, &before_policy)
        .unwrap()
        .unwrap();
    let policy = run(
        &[
            "--json",
            "privacy",
            "access",
            "policy",
            "alice/app@prepublish",
        ],
        false,
    );
    assert_success(&policy);
    let policy: serde_json::Value = serde_json::from_slice(&policy.stdout).unwrap();
    assert_eq!(
        policy["result"]["value"]["policy"]["grant_scope"],
        "session"
    );
    assert_eq!(
        policy["result"]["value"]["policy"]["session_id"],
        created.session
    );
    let disable = run(
        &[
            "--yes",
            "--json",
            "privacy",
            "access",
            "disable-session",
            "alice/app@prepublish",
        ],
        false,
    );
    assert_success(&disable);
    let disable: serde_json::Value = serde_json::from_slice(&disable.stdout).unwrap();
    assert_eq!(disable["result"]["value"]["converted_snapshots"], 0);
    let disabled_policy = disable["result"]["value"]["policy"].clone();
    assert_eq!(disabled_policy["grant_scope"], "commit");
    assert_eq!(
        git(repo.root(), &["rev-parse", "refs/heads/prepublish"]),
        before_policy
    );
    let unpublished = run(
        &[
            "--yes",
            "privacy",
            "access",
            "grant",
            "alice/app@prepublish",
        ],
        false,
    );
    assert!(!unpublished.status.success(), "{unpublished:?}");
    assert!(String::from_utf8_lossy(&unpublished.stderr).contains("requires an accepted snapshot"));
    let partial = run(
        &["privacy", "access", "policy", "alice/app@prepublish#1"],
        false,
    );
    assert!(!partial.status.success(), "{partial:?}");
    assert_success(&run(&["--yes", "push", "alice/app@prepublish"], false));
    let published = git(&remote, &["rev-parse", "refs/heads/prepublish"]);
    let publication = envelope(&published);
    let authority = publication.session_binding.as_ref().unwrap();
    assert_eq!(authority.session_id, created.session);
    assert_eq!(authority.owner_key.account_id, "account-1");
    assert_eq!(authority.source_commit, before_policy);
    let policy_after = run(
        &[
            "--json",
            "privacy",
            "access",
            "policy",
            "alice/app@prepublish",
        ],
        false,
    );
    assert_success(&policy_after);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&policy_after.stdout).unwrap()["result"]["value"]
            ["policy"],
        disabled_policy
    );
    let repeated = run(
        &[
            "--yes",
            "privacy",
            "access",
            "disable-session",
            "alice/app@prepublish",
        ],
        false,
    );
    assert_success(&repeated);
    assert_eq!(
        git(&remote, &["rev-parse", "refs/heads/prepublish"]),
        published
    );
    hub.unsupported_policy.store(true, Ordering::Release);
    let unsupported = run(
        &[
            "--yes",
            "--json",
            "privacy",
            "access",
            "disable-session",
            "alice/app@prepublish",
        ],
        false,
    );
    assert!(!unsupported.status.success(), "{unsupported:?}");
    let response = String::from_utf8_lossy(&unsupported.stdout);
    assert!(
        response
            .contains("Enabling session-level authorization after disabling it is unsupported."),
        "{unsupported:?}"
    );
    assert!(!response.contains("is retained"));
    assert!(!response.contains("authorization is disabled"));
    let policy_after = run(
        &[
            "--json",
            "privacy",
            "access",
            "policy",
            "alice/app@prepublish",
        ],
        false,
    );
    assert_success(&policy_after);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&policy_after.stdout).unwrap()["result"]["value"]
            ["policy"],
        disabled_policy
    );
    let key_requests = hub.key_requests.load(Ordering::Acquire);
    repo.git(&["checkout", "--ignore-other-worktrees", "unencrypted"])
        .unwrap();
    let first_plain_source = save(&chosen_meta, "Explicit plain context");
    let second_plain_source = save(&chosen_meta, "Next plain context");
    repo.git(&["branch", "-m", "unencrypted", "publish-plain"])
        .unwrap();
    assert_success(&run(&["privacy", "mode", "alice/app@publish-plain"], false));
    let access = run(
        &["privacy", "access", "policy", "alice/app@publish-plain"],
        false,
    );
    assert!(!access.status.success());
    assert!(
        String::from_utf8_lossy(&access.stderr)
            .contains("plain sessions use repository read access")
    );
    let plain_push = ["--yes", "push", "alice/app@publish-plain"];
    hub.corrupt_receipt.store(true, Ordering::Release);
    assert!(!run(&plain_push, false).status.success());
    let locked = run(
        &[
            "--yes",
            "privacy",
            "mode",
            "alice/app@publish-plain",
            "encrypted",
        ],
        false,
    );
    assert!(!locked.status.success());
    assert!(String::from_utf8_lossy(&locked.stderr).contains("already registered"));
    hub.corrupt_receipt.store(false, Ordering::Release);
    assert_success(&run(&plain_push, false));
    let plain_head = git(&remote, &["rev-parse", "refs/heads/publish-plain"]);
    assert_eq!(plain_head, second_plain_source);
    assert_eq!(
        git(repo.root(), &["rev-parse", "refs/heads/publish-plain"]),
        second_plain_source
    );
    assert!(
        git(
            &remote,
            &["ls-tree", "-r", &plain_head, "--", "privacy/envelope.json"]
        )
        .is_empty()
    );
    let plain_metadata: meta::Meta = serde_json::from_str(&git(
        &remote,
        &["show", &format!("{plain_head}:session/meta.json")],
    ))
    .unwrap();
    assert_eq!(
        plain_metadata.session_privacy.unwrap().scheme,
        agit::domain::session_privacy::SessionScheme::Plain
    );
    assert_eq!(
        storage::materialize_at(&remote, &plain_head, meta::LOG_FILE).unwrap(),
        storage::materialize_at(repo.root(), &second_plain_source, meta::LOG_FILE).unwrap()
    );
    assert_eq!(
        git(&remote, &["rev-parse", &format!("{plain_head}^")]),
        first_plain_source
    );
    assert_success(&run(&plain_push, false));
    assert_eq!(
        git(&remote, &["rev-parse", "refs/heads/publish-plain"]),
        plain_head
    );
    let third = save(&chosen_meta, "Continued plain context");
    assert_success(&run(&plain_push, false));
    let next_plain = git(&remote, &["rev-parse", "refs/heads/publish-plain"]);
    assert_eq!(
        git(&remote, &["rev-parse", &format!("{next_plain}^")]),
        plain_head
    );
    let receipt = agit::domain::privacy_receipt::PublicationReceipt::load(&repo, "publish-plain")
        .unwrap()
        .unwrap();
    assert_eq!(receipt.source, third);
    assert_eq!(receipt.published, next_plain);
    assert_eq!(
        receipt.mode,
        agit::domain::privacy_receipt::PublicationMode::Ordinary
    );
    assert_eq!(hub.key_requests.load(Ordering::Acquire), key_requests);
    for (target, mode) in [
        ("alice/app@publish-plain", "encrypted"),
        ("alice/app@prepublish", "plain"),
    ] {
        let refused = run(&["--yes", "privacy", "mode", target, mode], false);
        assert!(!refused.status.success(), "{refused:?}");
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains("already registered"),
            "{refused:?}"
        );
    }

    // A publication restores repeated events across worker batches from one retained dictionary.
    use agit::domain::privacy::{
        dictionary::{Label, Origin, Record},
        keys,
        storage::Store,
    };
    let owner = keys::owner(&hub.base, "account-1").unwrap();
    let mut dictionary = Store::open(
        &keys::owner_directory(&home.join("privacy"), &owner).join("dictionary"),
        Some(owner),
    )
    .unwrap();
    let secret = "AKIA2E7YQXK4NMZ5VJ3T";
    let record = Record::new(
        &dictionary.dictionary_id,
        secret,
        Origin::RepositoryUser,
        Label::secret("explicit-block"),
    )
    .unwrap();
    dictionary
        .append(std::slice::from_ref(&record), None)
        .unwrap();
    repo.git(&["checkout", "-b", "hydrated", &base]).unwrap();
    let mut hydrated_meta = meta::Meta::new(
        format!("agit-{}", "f".repeat(40)),
        "claude-code".into(),
        String::new(),
    );
    hydrated_meta.session_privacy = Some(SessionPrivacy {
        scheme: agit::domain::session_privacy::SessionScheme::SessionOwnerV2,
        ..SessionPrivacy::new(Some(AccountIdentity::new(&hub.base, "account-1").unwrap())).unwrap()
    });
    let repeated = transcript::wrap_lines(
        &json!({"type":"user","message":{"role":"user","content":record.token}}).to_string(),
        "claude-code",
        &hydrated_meta.session,
    );
    let padding = transcript::wrap_lines(
        &json!({"type":"user","message":{"role":"user","content":"padding ".repeat(20_000)}})
            .to_string(),
        "claude-code",
        &hydrated_meta.session,
    );
    let log = format!("{repeated}{padding}{repeated}");
    let view = repeated.repeat(2);
    storage::write_snapshot(repo.root(), &log, &view).unwrap();
    meta::write(repo.root(), &hydrated_meta).unwrap();
    repo.add_all().unwrap();
    repo.commit("Protected repeated events").unwrap();
    assert_success(&run(&["--yes", "push", "alice/app@hydrated"], false));
    let restored = envelope(&git(&remote, &["rev-parse", "refs/heads/hydrated"]))
        .open_layer(&rotated)
        .unwrap();
    let (restored_log, restored_view) = restored.session_bytes().unwrap();
    let records = storage::parse_envelopes(&restored_log).unwrap();
    assert_eq!(records[0].content["message"]["content"], secret);
    assert_eq!(records[0].content, records[2].content);
    assert_eq!(
        *restored_view,
        storage::envelope_line(&records[0]).repeat(2)
    );
    assert!(restored.protected_values.contains(secret));
    assert_eq!(
        storage::materialize_worktree(repo.root(), meta::LOG_FILE).unwrap(),
        log
    );

    let missing = Record::new(
        &dictionary.dictionary_id,
        "missing original",
        Origin::RepositoryUser,
        Label::secret("explicit-block"),
    )
    .unwrap();
    let unknown = transcript::wrap_lines(
        &json!({"type":"user","message":{"role":"user","content":missing.token}}).to_string(),
        "claude-code",
        &hydrated_meta.session,
    );
    storage::write_snapshot(repo.root(), &unknown, &unknown).unwrap();
    repo.add_all().unwrap();
    repo.commit("Unavailable original").unwrap();
    let received = hub.posts.load(Ordering::Acquire);
    let refused = run(&["--yes", "push", "alice/app@hydrated"], false);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("publication hydration is incomplete"),
        "{refused:?}"
    );
    assert_eq!(hub.posts.load(Ordering::Acquire), received);

    // A value the user registered is refused in a publication branch name. Only the privacy
    // worker's dictionary holds it, so a check that ran the detector alone would publish it.
    let registered = "zephyr-orchid-7741";
    let environment = git_command(temp.path());
    let mut add = Command::new(env!("CARGO_BIN_EXE_agit"));
    add.env_clear();
    for (name, value) in environment.get_envs() {
        if let Some(value) = value {
            add.env(name, value);
        }
    }
    let mut add = add
        .current_dir(temp.path())
        .env("AGIT_HOME", &home)
        .env("AGIT_HUB_URL", &hub.base)
        .env("CI", "1")
        .env("AGIT_TUI", "0")
        .env("AGIT_USE_SYSTEM_GIT", "1")
        .env(
            "AGIT_SECRETS_KEYSTORE",
            if cfg!(unix) { "file" } else { "os" },
        )
        .args(["secrets", "add", "branch-word", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    add.stdin
        .take()
        .unwrap()
        .write_all(registered.as_bytes())
        .unwrap();
    assert_success(&add.wait_with_output().unwrap());
    let named = format!("ship-{registered}");
    repo.git(&["branch", &named, "work"]).unwrap();
    let refused = run(&["--yes", "push", &format!("alice/app@{named}")], false);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("requires privacy rewriting"),
        "{refused:?}"
    );
    assert_eq!(hub.posts.load(Ordering::Acquire), received);

    // A fresh checkout publishes only inherited public history and its own cross-runtime turns.
    let collaborator = temp.path().join("collaborator");
    startup_cache::seed(&collaborator);
    let credential_name = format!("{}.json", config::hub_host_key(&hub.base).unwrap());
    fs::create_dir_all(collaborator.join("credentials")).unwrap();
    save_at(
        &collaborator.join("credentials").join(&credential_name),
        &HubCredential {
            account_id: Some("account-2".into()),
            username: "bob".into(),
            email: None,
            hub: Some(hub.base.clone()),
            access_token: "synthetic-collaborator".into(),
            refresh_token: "synthetic-collaborator-refresh".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    assert_success(&run_home(
        &["clone", "alice/app", "--no-bind"],
        false,
        &collaborator,
    ));
    let collaborator_repo = Repo::at(collaborator.join("repos/alice/app"));
    assert!(
        !collaborator_repo
            .root()
            .join(".git/agit/privacy-accepted")
            .exists()
    );
    collaborator_repo
        .git(&["config", "http.userAgent", TRANSPORT_USER_AGENT])
        .unwrap();
    let local_main = git(collaborator_repo.root(), &["rev-parse", "refs/heads/main"]);
    let teammate = temp.path().join("teammate");
    let cloned = git_command(temp.path())
        .args(["clone", "--quiet", "--single-branch", "--branch", "main"])
        .arg(&remote)
        .arg(&teammate)
        .output()
        .unwrap();
    assert_success(&cloned);
    git(&teammate, &["config", "user.name", "Teammate"]);
    git(
        &teammate,
        &["config", "user.email", "teammate@example.invalid"],
    );
    fs::write(teammate.join("shared-update.md"), "New shared knowledge\n").unwrap();
    git(&teammate, &["add", "shared-update.md"]);
    git(
        &teammate,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--no-gpg-sign",
            "-m",
            "Update shared knowledge",
        ],
    );
    git(&teammate, &["push", "origin", "main"]);
    let remote_main = git(&remote, &["rev-parse", "refs/heads/main"]);
    assert_ne!(remote_main, local_main);
    let fork_target = "alice/app@collaborator-fork";
    let fork_source = format!(
        "alice/app@{}",
        git(&remote, &["rev-parse", "refs/heads/legacy"])
    );
    assert_success(&run_home(
        &[
            "run",
            &fork_source,
            "--no-launch",
            "--as",
            "codex",
            "-b",
            "collaborator-fork",
        ],
        false,
        &collaborator,
    ));
    assert_success(&run_home(
        &["resume", fork_target, "--as", "claude-code", "--no-launch"],
        false,
        &collaborator,
    ));
    let live = walkdir::WalkDir::new(collaborator.join("claude/projects"))
        .into_iter()
        .filter_map(Result::ok)
        .find(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .expect("cross-runtime resume installs a Claude transcript")
        .into_path();
    let native_id = live.file_stem().unwrap().to_str().unwrap();
    let mut parent = fs::read_to_string(&live)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|record| record.get("uuid").cloned())
        .next_back()
        .unwrap_or(serde_json::Value::Null);
    let mut appended = fs::OpenOptions::new().append(true).open(&live).unwrap();
    for mut record in [
        json!({"type":"user","sessionId":native_id,"uuid":uuid::Uuid::now_v7(),"message":{"role":"user","content":"Collaborator continuation"}}),
        json!({"type":"assistant","sessionId":native_id,"uuid":uuid::Uuid::now_v7(),"message":{"role":"assistant","content":[{"type":"text","text":"Collaborator reply"}]}}),
    ] {
        record["parentUuid"] = parent;
        parent = record["uuid"].clone();
        record["cwd"] = json!(temp.path());
        writeln!(appended, "{record}").unwrap();
    }
    drop(appended);
    assert_success(&run_home(&["commit", fork_target], false, &collaborator));
    let pushed = run_home(&["--yes", "push", fork_target], false, &collaborator);
    assert_success(&pushed);
    let notice = String::from_utf8_lossy(&pushed.stdout);
    assert_eq!(
        notice.matches("main left as on the Hub").count(),
        1,
        "{notice}"
    );
    assert!(notice.contains("agit pull alice/app@main"), "{notice}");
    assert_success(&run_home(
        &["push", fork_target, "--dry-run"],
        false,
        &collaborator,
    ));
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/main"]), remote_main);
    assert_eq!(
        git(collaborator_repo.root(), &["rev-parse", "refs/heads/main"]),
        local_main
    );
    assert_eq!(
        git(&remote, &["show", "main:shared-update.md"]),
        "New shared knowledge"
    );
    for args in [
        vec!["push", "alice/app@main", "--dry-run"],
        vec!["push", "alice/app", "--all", "--dry-run"],
    ] {
        let explicit = run_home(&args, false, &collaborator);
        assert!(!explicit.status.success(), "{explicit:?}");
        assert!(
            String::from_utf8_lossy(&explicit.stderr).contains("agit pull alice/app@main"),
            "{explicit:?}"
        );
        assert!(
            !String::from_utf8_lossy(&explicit.stdout).contains("main left as on the Hub"),
            "{explicit:?}"
        );
    }
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/main"]), remote_main);
    assert_eq!(
        git(&remote, &["rev-parse", "refs/heads/legacy"]),
        fork_source.strip_prefix("alice/app@").unwrap()
    );
    let published_meta: meta::Meta = serde_json::from_str(&git(
        &remote,
        &["show", &format!("collaborator-fork:{}", meta::FILE)],
    ))
    .unwrap();
    assert_eq!(published_meta.runtime, "claude-code");
    assert_eq!(
        published_meta.committer_account.unwrap().account_id,
        "account-2"
    );

    // A shallow boundary cannot hide a locally available parent from implicit-main selection.
    assert_success(&run_home(&["pull", "alice/app@main"], false, &collaborator));
    fs::write(
        collaborator_repo.root().join("local-main-update.md"),
        "Local shared knowledge\n",
    )
    .unwrap();
    git(collaborator_repo.root(), &["add", "local-main-update.md"]);
    git(
        collaborator_repo.root(),
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--no-gpg-sign",
            "-m",
            "Local shared knowledge",
        ],
    );
    let local_ahead = git(collaborator_repo.root(), &["rev-parse", "refs/heads/main"]);
    let shallow = collaborator_repo.root().join(".git/shallow");
    fs::write(&shallow, format!("{local_ahead}\n")).unwrap();
    let published = run_home(&["--yes", "push", fork_target], false, &collaborator);
    fs::remove_file(shallow).unwrap();
    assert_success(&published);
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/main"]), local_ahead);
    assert!(
        !String::from_utf8_lossy(&published.stdout).contains("main left as on the Hub"),
        "{published:?}"
    );
}

/// A migration must send the registered target identity in real Git objects and retain it
/// when a later ordinary push extends native source history. Same-identity pushes repair origin.
/// Collaborator commits stay unattributed; unprovable creators fail before any registration.
/// Legacy birth attribution survives continuation recorded under the destination account.
/// Without a terminal, a plain session publishes to a private repository unasked, but a public
/// destination takes the person's own `--yes`. A gate that only checked encryption would publish
/// publicly here; one that refused after preparing the upload would leave refs on the receiver.
#[test]
fn unattended_plain_publication_to_a_public_repository_needs_explicit_confirmation() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    startup_cache::seed(&home);
    let remote_root = temp.path().join("remote");
    let remote = remote_root.join("alice/app.git");
    let hub = Hub::start(&remote_root, [27; 32]);
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
    let run = |args: &[&str]| {
        let environment = git_command(temp.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear();
        for (name, value) in environment.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command
            .current_dir(temp.path())
            .env("AGIT_HOME", &home)
            .env("CODEX_HOME", home.join("codex"))
            .env("CLAUDE_CONFIG_DIR", home.join("claude"))
            .env("AGIT_HUB_URL", &hub.base)
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(unix) { "file" } else { "os" },
            )
            .stdin(Stdio::null())
            .args(args)
            .output()
            .unwrap()
    };
    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    repo.git(&["config", "http.userAgent", TRANSPORT_USER_AGENT])
        .unwrap();
    repo.git(&["config", "user.name", "Source Writer"]).unwrap();
    repo.git(&["config", "user.email", "source@example.invalid"])
        .unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    fs::write(repo.root().join("README.md"), "Shared file").unwrap();
    repo.add_all().unwrap();
    repo.commit("File line").unwrap();
    assert_success(&run(&["new", "alice/app", "-b", "work", "--no-launch"]));
    assert_success(&run(&["--yes", "push", "alice/app@main"]));
    hub.posts.store(0, Ordering::Release);

    *hub.visibility.lock().unwrap() = "public";
    let refused = run(&["push", "alice/app@work"]);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains(
            "publishing to a public repository needs the person's confirmation: run it in a terminal, or pass --yes once they have confirmed it in this conversation"
        ),
        "{refused:?}"
    );
    assert_eq!(hub.posts.load(Ordering::Acquire), 0);
    assert!(git(&remote, &["for-each-ref", "refs/heads/work"]).is_empty());

    assert_success(&run(&["--yes", "push", "alice/app@work"]));
    assert_eq!(
        git(&remote, &["rev-parse", "refs/heads/work"]),
        git(repo.root(), &["rev-parse", "refs/heads/work"])
    );

    *hub.visibility.lock().unwrap() = "private";
    let work = home.join("worktrees/alice/app/work");
    fs::write(work.join("README.md"), "Shared file, revised").unwrap();
    git(&work, &["commit", "-am", "Revise file"]);
    assert_success(&run(&["push", "alice/app@work"]));
    assert_eq!(
        git(&remote, &["rev-parse", "refs/heads/work"]),
        git(repo.root(), &["rev-parse", "refs/heads/work"])
    );
}

/// The first upload counts as soon as the Hub acknowledged a branch it updated, before anything
/// after that can fail. Here the Hub refuses the tag of a push whose branch went up, so the push
/// fails, and its retry finds the branch up to date. Counting only at the end of a push that
/// fully succeeded would miss this installation's first upload for good.
#[test]
fn a_first_upload_counts_once_the_hub_acknowledged_a_branch() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    startup_cache::seed(&home);
    let remote_root = temp.path().join("remote");
    let remote = remote_root.join("alice/app.git");
    let hub = Hub::start(&remote_root, [29; 32]);
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
    let run = |args: &[&str]| {
        let environment = git_command(temp.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear();
        for (name, value) in environment.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command
            .current_dir(temp.path())
            .env("AGIT_HOME", &home)
            .env("CODEX_HOME", home.join("codex"))
            .env("CLAUDE_CONFIG_DIR", home.join("claude"))
            .env("AGIT_HUB_URL", &hub.base)
            .env("AGIT_TELEMETRY_HOST", "http://127.0.0.1:9")
            .env("AGIT_TELEMETRY_KEY", "synthetic_project")
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(unix) { "file" } else { "os" },
            )
            .stdin(Stdio::null())
            .args(args)
            .output()
            .unwrap()
    };
    let telemetry = home.join("telemetry");
    let uploads = || {
        let queue: serde_json::Value = fs::read(telemetry.join("queue.json"))
            .ok()
            .and_then(|body| serde_json::from_slice(&body).ok())
            .unwrap_or_default();
        queue["entries"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|entry| entry["event"]["event"] == "cli_first_upload")
            .count()
    };
    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    repo.git(&["config", "http.userAgent", TRANSPORT_USER_AGENT])
        .unwrap();
    repo.git(&["config", "user.name", "Source Writer"]).unwrap();
    repo.git(&["config", "user.email", "source@example.invalid"])
        .unwrap();
    repo.git(&["config", "tag.gpgsign", "false"]).unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    fs::write(repo.root().join("README.md"), "Shared file").unwrap();
    repo.add_all().unwrap();
    repo.commit("File line").unwrap();
    // The repository is created and first published while statistics are still undecided, so
    // nothing is counted yet.
    assert_success(&run(&["--yes", "push", "alice/app@main"]));
    assert_eq!(uploads(), 0);

    {
        fs::create_dir_all(&telemetry).unwrap();
        let gate = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(telemetry.join("gate.lock"))
            .unwrap();
        fs2::FileExt::lock_exclusive(&gate).unwrap();
        let preferences = json!({
            "preference": "enabled",
            "generation": 1,
            "device_id": uuid::Uuid::new_v4(),
            "decision_source": "explicit_enable",
            "notice_version": agit::telemetry::state::NOTICE_VERSION,
        });
        fs::write(telemetry.join("preferences.json"), preferences.to_string()).unwrap();
    }
    hub.refuse_tags.store(true, Ordering::Release);
    fs::write(repo.root().join("README.md"), "Shared file, revised").unwrap();
    repo.add_all().unwrap();
    repo.commit("Revise file").unwrap();
    git(repo.root(), &["tag", "release-2"]);
    let refused = run(&["--yes", "push", "alice/app@main"]);
    assert!(!refused.status.success(), "{refused:?}");
    assert_eq!(
        git(&remote, &["rev-parse", "refs/heads/main"]),
        git(repo.root(), &["rev-parse", "refs/heads/main"])
    );
    assert!(git(&remote, &["for-each-ref", "refs/tags/release-2"]).is_empty());
    assert_eq!(uploads(), 1, "{refused:?}");

    hub.refuse_tags.store(false, Ordering::Release);
    assert_success(&run(&["--yes", "push", "alice/app@main"]));
    assert!(!git(&remote, &["for-each-ref", "refs/tags/release-2"]).is_empty());
    assert_eq!(uploads(), 1);
}

#[test]
fn cross_hub_delivery_publishes_target_objects_and_continues_with_ordinary_push() {
    use agit::domain::session_privacy::{AccountIdentity, SessionPrivacy};
    use agit::hub::identity::{self, RemoteIdentity};
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    startup_cache::seed(&home);
    let source_root = temp.path().join("source");
    let target_root = temp.path().join("target");
    let source = Hub::start(&source_root, [25; 32]);
    let target = Hub::start(&target_root, [26; 32]);
    for hub in [&source, &target] {
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
    }
    let run = |hub: &Hub, args: &[&str]| {
        let environment = git_command(temp.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear();
        for (name, value) in environment.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command
            .current_dir(temp.path())
            .env("AGIT_HOME", &home)
            .env("CODEX_HOME", home.join("codex"))
            .env("CLAUDE_CONFIG_DIR", home.join("claude"))
            .env("AGIT_HUB_URL", &hub.base)
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(unix) { "file" } else { "os" },
            )
            .stdin(Stdio::null())
            .args(args)
            .output()
            .unwrap()
    };
    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    for (key, value) in [
        ("http.userAgent", TRANSPORT_USER_AGENT),
        ("user.name", "Source Writer"),
        ("user.email", "source@example.invalid"),
        ("commit.gpgsign", "false"),
    ] {
        repo.git(&["config", key, value]).unwrap();
    }
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    repo.add_all().unwrap();
    repo.commit("File line").unwrap();
    repo.git(&["checkout", "-b", "work"]).unwrap();
    let source_account = AccountIdentity::new(&source.base, "account-1").unwrap();
    let mut metadata = meta::Meta::new(
        format!("agit-{}", "a".repeat(40)),
        "claude-code".into(),
        "/work".into(),
    );
    metadata.session_privacy = None;
    metadata.committer_account = Some(source_account);
    fs::write(repo.root().join("artifact.txt"), "Delivered artifact\n").unwrap();
    let mut claim = metadata.clone();
    claim.session.clear();
    storage::write_snapshot(repo.root(), "", "").unwrap();
    meta::write(repo.root(), &claim).unwrap();
    repo.add_all().unwrap();
    repo.commit("Claim session").unwrap();
    let save = |metadata: &meta::Meta, contents: &[&str], message: &str| {
        let log = transcript::wrap_lines(
            &contents
                .iter()
                .map(|content| {
                    format!(
                        "{}\n",
                        json!({"type":"user","message":{"role":"user","content":content}})
                    )
                })
                .collect::<String>(),
            "claude-code",
            &metadata.session,
        );
        storage::write_snapshot(repo.root(), &log, &log).unwrap();
        meta::write(repo.root(), metadata).unwrap();
        repo.add_all().unwrap();
        assert!(
            repo.commit(message).unwrap(),
            "fixture must create {message}"
        );
        git(repo.root(), &["rev-parse", "HEAD"])
    };
    let first = save(&metadata, &["First turn"], "Subject\n\nMilestone details.");
    repo.git(&["config", "author.name", "Other Author"])
        .unwrap();
    repo.git(&["config", "author.email", "other@example.invalid"])
        .unwrap();
    save(
        &metadata,
        &["First turn", "Other author's turn"],
        "Other author, own committer",
    );
    repo.git(&["config", "--unset", "author.name"]).unwrap();
    repo.git(&["config", "--unset", "author.email"]).unwrap();
    let mut collaborator = metadata.clone();
    collaborator.committer_account = Some(AccountIdentity::new(&source.base, "account-2").unwrap());
    save(&collaborator, &["First turn"], "Collaborator contribution");
    // An ordinary push of a session first published on another Hub registers it as the pusher's
    // only when this device is signed in there as its creator, or when its birth names no Hub.
    // Someone else's session from the target Hub is refused, named, before anything reaches the
    // source Hub; one that re-owned every such session would register it as account-1's here.
    // An admitted one names neither the other Hub's registration nor its accounts, including one
    // whose metadata predates recorded privacy and names its birth only by its committer.
    for (branch, id, birth, recorded, admitted) in [
        ("theirs", "d", Some("account-2"), true, false),
        ("mine", "e", Some("account-1"), true, true),
        ("legacy", "7", Some("account-1"), false, true),
        ("offline", "f", None, false, true),
    ] {
        repo.git(&["checkout", "-b", branch, &format!("{first}~2")])
            .unwrap();
        let mut elsewhere = metadata.clone();
        elsewhere.session = format!("agit-{}", id.repeat(40));
        let creator = birth.map(|creator| AccountIdentity::new(&target.base, creator).unwrap());
        elsewhere.session_privacy = recorded.then(|| SessionPrivacy::new(creator.clone()).unwrap());
        elsewhere.committer_account = creator;
        let mut claim = elsewhere.clone();
        claim.session.clear();
        storage::write_snapshot(repo.root(), "", "").unwrap();
        meta::write(repo.root(), &claim).unwrap();
        repo.add_all().unwrap();
        repo.commit("Claim session").unwrap();
        save(&elsewhere, &["Session from elsewhere"], "Elsewhere");
        let before = (
            source.registrations.load(Ordering::Acquire),
            source.posts.load(Ordering::Acquire),
        );
        let pushed = run(&source, &["--yes", "push", &format!("alice/app@{branch}")]);
        repo.git(&["checkout", "work"]).unwrap();
        if !admitted {
            assert!(!pushed.status.success(), "{pushed:?}");
            let stderr = String::from_utf8_lossy(&pushed.stderr);
            assert!(
                stderr.contains(&format!(
                    "session {} was first published on",
                    elsewhere.session
                )) && stderr.contains("cannot be published here as yours"),
                "{pushed:?}"
            );
            assert_eq!(
                (
                    source.registrations.load(Ordering::Acquire),
                    source.posts.load(Ordering::Acquire),
                ),
                before
            );
            continue;
        }
        assert_success(&pushed);
        let tip = git(
            &source_root.join("alice/app.git"),
            &["rev-parse", &format!("refs/heads/{branch}")],
        );
        let published: serde_json::Value = serde_json::from_str(&git(
            &source_root.join("alice/app.git"),
            &["show", &format!("{tip}:session/meta.json")],
        ))
        .unwrap();
        assert_eq!(published["session"], elsewhere.session);
        assert!(published["committer_account"].is_null(), "{published}");
        if birth.is_some() {
            assert_eq!(
                published["session_privacy"]["hub"], source.base,
                "{branch}: {published}"
            );
            assert_eq!(published["session_privacy"]["creator"], "account-1");
        }
    }
    assert_success(&run(&source, &["--yes", "push", "alice/app@work"]));
    let source_refs = git(&source_root.join("alice/app.git"), &["show-ref"]);
    for (branch, hub, creator) in [
        ("foreign", source.base.as_str(), "account-2"),
        ("unknown", "https://unknown.example", "account-1"),
    ] {
        repo.git(&["checkout", "-b", branch]).unwrap();
        let mut foreign = metadata.clone();
        foreign.session = format!("agit-{}", "c".repeat(40));
        foreign.session_privacy =
            Some(SessionPrivacy::new(Some(AccountIdentity::new(hub, creator).unwrap())).unwrap());
        save(&foreign, &["Foreign session"], "Foreign creator");
        let refused = run(
            &source,
            &[
                "--yes",
                "push",
                &format!("alice/app@{branch}"),
                "--to-hub",
                &target.base,
            ],
        );
        assert!(!refused.status.success(), "{refused:?}");
        assert!(
            String::from_utf8_lossy(&refused.stderr)
                .contains("cannot verify this session's creator"),
            "{refused:?}"
        );
        assert_eq!(target.registrations.load(Ordering::Acquire), 0);
        assert_eq!(target.posts.load(Ordering::Acquire), 0);
        repo.git(&["checkout", "work"]).unwrap();
    }
    let refused = run(&target, &["--yes", "push", "alice/app@work"]);
    assert!(!refused.status.success());
    assert_eq!(target.posts.load(Ordering::Acquire), 0);
    target.corrupt_receipt.store(true, Ordering::Release);
    let failed = run(
        &source,
        &["--yes", "push", "alice/app@work", "--to-hub", &target.base],
    );
    assert!(!failed.status.success());
    assert_eq!(
        identity::read(&repo).unwrap(),
        Some(RemoteIdentity::new(&source.base, AGENT_ID).unwrap())
    );
    assert_eq!(
        repo.remote_url().unwrap(),
        format!("{}/alice/app.git", source.base)
    );
    assert_eq!(target.posts.load(Ordering::Acquire), 0);
    target.corrupt_receipt.store(false, Ordering::Release);
    let unconfirmed = run(
        &source,
        &["push", "alice/app@work", "--to-hub", &target.base],
    );
    assert!(!unconfirmed.status.success());
    assert_eq!(target.posts.load(Ordering::Acquire), 0);
    assert_success(&run(
        &source,
        &["--yes", "push", "alice/app@work", "--to-hub", &target.base],
    ));
    let remote = target_root.join("alice/app.git");
    let published = git(&remote, &["rev-parse", "refs/heads/work"]);
    assert_ne!(published, first);
    assert_eq!(
        git(&remote, &["show", &format!("{published}:artifact.txt")]),
        "Delivered artifact"
    );
    let published_meta: serde_json::Value = serde_json::from_str(&git(
        &remote,
        &["show", &format!("{published}:session/meta.json")],
    ))
    .unwrap();
    let target_session = published_meta["session"].as_str().unwrap().to_owned();
    assert_ne!(target_session, metadata.session);
    let public_log = storage::materialize_at(&remote, &published, meta::LOG_FILE).unwrap();
    assert!(!public_log.contains(&metadata.session));
    assert!(public_log.contains(&target_session));
    assert!(
        !git_command(&remote)
            .arg("-C")
            .arg(&remote)
            .args(["cat-file", "-e", &first])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(published_meta["session_privacy"]["hub"], target.base);
    assert!(published_meta["committer_account"].is_null());
    assert_eq!(published_meta["session_privacy"]["creator"], "account-1");
    assert_eq!(
        git(&remote, &["log", "-1", "--format=%B", &published]),
        "Collaborator contribution"
    );
    assert_eq!(
        git(&remote, &["log", "-1", "--format=%ae", &published]),
        "unmapped@agentgit.local"
    );
    assert_eq!(
        git(&remote, &["log", "-1", "--format=%ce", &published]),
        "unmapped@agentgit.local"
    );
    let authored = format!("{published}^");
    assert_eq!(
        git(&remote, &["log", "-1", "--format=%ae", &authored]),
        "unmapped@agentgit.local"
    );
    assert_eq!(
        git(&remote, &["log", "-1", "--format=%ce", &authored]),
        "account-1@agentgit.local"
    );
    let owned = format!("{published}~2");
    assert_eq!(
        git(&remote, &["log", "-1", "--format=%ae", &owned]),
        "account-1@agentgit.local"
    );
    assert_eq!(
        git(&remote, &["log", "-1", "--format=%B", &owned]),
        "Subject\n\nMilestone details."
    );
    assert_eq!(
        identity::read(&repo).unwrap(),
        Some(RemoteIdentity::new(&target.base, AGENT_ID).unwrap())
    );
    let source_after = meta::read_at_ref_result(&repo, &first).unwrap().unwrap();
    assert_eq!(source_after.session, metadata.session);
    metadata.committer_account = Some(AccountIdentity::new(&target.base, "account-1").unwrap());
    metadata.turn = Some(2);
    let second = save(
        &metadata,
        &["First turn", "Second turn"],
        "Second subject\n\nSecond body.",
    );
    repo.git(&["remote", "remove", "origin"]).unwrap();
    assert_success(&run(&target, &["--yes", "push", "alice/app@work"]));
    assert_eq!(
        repo.remote_url().as_deref(),
        Some(format!("{}/alice/app.git", target.base).as_str())
    );
    let continued = git(&remote, &["rev-parse", "refs/heads/work"]);
    assert_ne!(continued, second);
    assert_eq!(
        git(&remote, &["rev-parse", &format!("{continued}^")]),
        published
    );
    assert_eq!(
        git(&remote, &["log", "-1", "--format=%B", &continued]),
        "Second subject\n\nSecond body."
    );
    let continued_meta: serde_json::Value = serde_json::from_str(&git(
        &remote,
        &["show", &format!("{continued}:session/meta.json")],
    ))
    .unwrap();
    assert_eq!(continued_meta["session"], target_session);
    assert_eq!(continued_meta["committer_account"]["hub"], target.base);
    let received = target.posts.load(Ordering::Acquire);
    repo.set_remote("https://obsolete.example/alice/app.git")
        .unwrap();
    assert_success(&run(&target, &["--yes", "push", "alice/app@work"]));
    assert_eq!(target.posts.load(Ordering::Acquire), received);
    assert_eq!(
        repo.remote_url().unwrap(),
        format!("{}/alice/app.git", target.base)
    );
    assert_success(&run(&target, &["push", "alice/app@work", "--dry-run"]));
    assert_eq!(target.posts.load(Ordering::Acquire), received);
    assert_eq!(
        git(&source_root.join("alice/app.git"), &["show-ref"]),
        source_refs
    );
    for manifest in target.manifests.lock().unwrap().iter() {
        for snapshot in manifest["snapshots"].as_array().unwrap() {
            if let Some(session) = snapshot.get("session_id") {
                let commit = snapshot["commit"].as_str().unwrap();
                let actual: serde_json::Value = serde_json::from_str(&git(
                    &remote,
                    &["show", &format!("{commit}:session/meta.json")],
                ))
                .unwrap();
                assert_eq!(&actual["session"], session);
                assert_eq!(session, &target_session);
            }
        }
    }
}

/// A plain session that continues its published history declares only what it adds: the next
/// push preserves just the heads the Hub already holds and snapshots just the new turns, so its
/// size follows the push, not the session. A walk of the whole history declares every earlier turn
/// again. If a branch head of the advertisement that history was read from moves before a receipt
/// binds it, the declaration may leave out commits the Hub no longer holds, so the publication is
/// refused before anything is sent.
///
/// The bound applies to commits, not to tags: a tag the branch reaches below the boundary that the
/// Hub lacks still arrives, both one added to an earlier turn after it was published and a version
/// tag of a published turn the Hub lost, as when a tags phase fails after the heads land, and the
/// push that carries them still declares only the new turn. A plan that takes only its own
/// commits' tags leaves both off the Hub on every later push; one that publishes them by capturing
/// the whole history again declares every turn, and past the commit bound cannot push at all. The
/// added tag arrives without a new turn too: a push that skips a branch because its tip is still
/// the one its receipt acknowledged, without checking the tags, reports it up to date and never
/// sends the tag. Once the tag is acknowledged the branch is skipped again; a tag listing that is
/// recorded differently from how the skip reads it would never skip anything.
#[test]
fn continued_plain_session_declares_only_what_it_adds() {
    use agit::domain::session_privacy::SessionPublicationManifest;
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    startup_cache::seed(&home);
    let remote_root = temp.path().join("remote");
    let remote = remote_root.join("alice/app.git");
    let hub = Hub::start(&remote_root, [26; 32]);
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
    let run = |args: &[&str]| {
        let environment = git_command(temp.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear();
        for (name, value) in environment.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command
            .current_dir(temp.path())
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
            .args(args)
            .output()
            .unwrap()
    };
    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    repo.git(&["config", "http.userAgent", TRANSPORT_USER_AGENT])
        .unwrap();
    repo.git(&["config", "user.name", "Source Writer"]).unwrap();
    repo.git(&["config", "user.email", "source@example.invalid"])
        .unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    fs::write(repo.root().join("README.md"), "Shared file").unwrap();
    repo.add_all().unwrap();
    repo.commit("File line").unwrap();
    let base = git(repo.root(), &["rev-parse", "HEAD"]);
    assert_success(&run(&["--yes", "push", "alice/app@main"]));
    repo.git(&["checkout", "-b", "talk"]).unwrap();
    let session = meta::Meta::new(
        format!("agit-{}", "b".repeat(40)),
        "claude-code".into(),
        String::new(),
    );
    let turn = |content: &str| {
        let log = transcript::wrap_lines(
            &format!(
                "{}\n",
                json!({"type":"user","message":{"role":"user","content":content}})
            ),
            "claude-code",
            &session.session,
        );
        storage::write_snapshot(repo.root(), &log, &log).unwrap();
        meta::write(repo.root(), &session).unwrap();
        repo.add_all().unwrap();
        repo.commit(content).unwrap();
        git(repo.root(), &["rev-parse", "HEAD"])
    };
    let first: Vec<String> = (0..3).map(|index| turn(&format!("turn {index}"))).collect();
    assert_success(&run(&["--yes", "push", "alice/app@talk"]));
    let published = git(repo.root(), &["rev-parse", "HEAD"]);
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/talk"]), published);

    let earlier = hub.previews.lock().unwrap().len();
    let added = std::collections::BTreeSet::from([turn("turn 3"), turn("turn 4")]);
    assert_success(&run(&["--yes", "push", "alice/app@talk"]));
    let head = git(repo.root(), &["rev-parse", "HEAD"]);
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/talk"]), head);
    let previews = hub.previews.lock().unwrap()[earlier..].to_vec();
    assert!(!previews.is_empty());
    for preview in previews {
        let declared: SessionPublicationManifest =
            serde_json::from_value(preview["manifest"].clone()).unwrap();
        assert_eq!(
            declared
                .preserved_commits
                .iter()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>(),
            std::collections::BTreeSet::from([base.clone(), published.clone()])
        );
        assert_eq!(
            declared
                .snapshots
                .iter()
                .map(|snapshot| snapshot.commit().to_owned())
                .collect::<std::collections::BTreeSet<_>>(),
            added
        );
    }

    // A new branch off the published tip continues history another branch accepted and the
    // destination holds: it declares only its own turn, though it has accepted nothing itself.
    repo.git(&["checkout", "-b", "other"]).unwrap();
    let other = meta::Meta::new(
        format!("agit-{}", "d".repeat(40)),
        "claude-code".into(),
        String::new(),
    );
    let log = transcript::wrap_lines(
        &format!(
            "{}\n",
            json!({"type":"user","message":{"role":"user","content":"another session"}})
        ),
        "claude-code",
        &other.session,
    );
    storage::write_snapshot(repo.root(), &log, &log).unwrap();
    meta::write(repo.root(), &other).unwrap();
    repo.add_all().unwrap();
    repo.commit("another session").unwrap();
    let branched = git(repo.root(), &["rev-parse", "HEAD"]);
    let earlier = hub.previews.lock().unwrap().len();
    assert_success(&run(&["--yes", "push", "alice/app@other"]));
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/other"]), branched);
    for preview in &hub.previews.lock().unwrap()[earlier..] {
        let declared: SessionPublicationManifest =
            serde_json::from_value(preview["manifest"].clone()).unwrap();
        assert_eq!(
            declared
                .snapshots
                .iter()
                .map(|snapshot| snapshot.commit().to_owned())
                .collect::<Vec<_>>(),
            std::slice::from_ref(&branched)
        );
    }
    repo.git(&["checkout", "talk"]).unwrap();

    let version = format!("refs/tags/agit-{}", first[1]);
    assert_eq!(git(&remote, &["rev-parse", &version]), first[1]);
    // As after `agit fetch`: a push decides from local records alone only while the checkout's
    // main is the one it last fetched.
    git(
        repo.root(),
        &["update-ref", "refs/remotes/origin/main", "refs/heads/main"],
    );
    git(repo.root(), &["tag", "reviewed", &first[0]]);
    assert_success(&run(&["--yes", "push", "alice/app@talk"]));
    assert_eq!(git(&remote, &["rev-parse", "refs/tags/reviewed"]), first[0]);
    let unchanged = run(&["--yes", "push", "alice/app@talk"]);
    assert_success(&unchanged);
    assert!(
        format!(
            "{}{}",
            String::from_utf8_lossy(&unchanged.stdout),
            String::from_utf8_lossy(&unchanged.stderr)
        )
        .contains("unchanged since their acknowledged publication: talk"),
        "{unchanged:?}"
    );
    git(&remote, &["update-ref", "-d", &version]);
    let earlier = hub.previews.lock().unwrap().len();
    let fifth = turn("turn 5");
    assert_success(&run(&["--yes", "push", "alice/app@talk"]));
    assert_eq!(git(&remote, &["rev-parse", &version]), first[1]);
    assert_eq!(git(&remote, &["rev-parse", "refs/tags/reviewed"]), first[0]);
    let previews = hub.previews.lock().unwrap()[earlier..].to_vec();
    assert!(!previews.is_empty());
    for preview in previews {
        let declared: SessionPublicationManifest =
            serde_json::from_value(preview["manifest"].clone()).unwrap();
        assert_eq!(
            declared
                .preserved_commits
                .iter()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>(),
            std::collections::BTreeSet::from([base.clone(), head.clone()])
        );
        assert_eq!(
            declared
                .snapshots
                .iter()
                .map(|snapshot| snapshot.commit().to_owned())
                .collect::<Vec<_>>(),
            std::slice::from_ref(&fifth)
        );
    }

    turn("turn 6");
    let earlier = hub.previews.lock().unwrap().len();
    let received = hub.posts.load(Ordering::Acquire);
    *hub.move_on_strategy.lock().unwrap() = Some(("refs/heads/talk".into(), published.clone()));
    let refused = run(&["--yes", "push", "alice/app@talk"]);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(
        format!(
            "{}{}",
            String::from_utf8_lossy(&refused.stdout),
            String::from_utf8_lossy(&refused.stderr)
        )
        .contains("changed during this publication"),
        "{refused:?}"
    );
    assert_eq!(hub.posts.load(Ordering::Acquire), received);
    assert_eq!(hub.previews.lock().unwrap().len(), earlier);
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/talk"]), published);
}

/// An encrypted session that continues its published history declares only what it adds, and its
/// new snapshots keep the public session identity it was accepted under even when the local
/// candidate cache is gone: the accepted record, not the cache, decides the identity. Minting one
/// here would split the session into two public sessions on the Hub.
#[test]
fn continued_encrypted_session_keeps_its_public_identity() {
    use agit::domain::session_privacy::{
        AccountIdentity, PublishedSnapshot, SessionPrivacy, SessionPublicationManifest,
    };
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    startup_cache::seed(&home);
    let remote_root = temp.path().join("remote");
    let remote = remote_root.join("alice/app.git");
    let hub = Hub::start(&remote_root, [27; 32]);
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
    let run = |args: &[&str]| {
        let environment = git_command(temp.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear();
        for (name, value) in environment.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command
            .current_dir(temp.path())
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
            .args(args)
            .output()
            .unwrap()
    };
    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    repo.git(&["config", "http.userAgent", TRANSPORT_USER_AGENT])
        .unwrap();
    repo.git(&["config", "user.name", "Source Writer"]).unwrap();
    repo.git(&["config", "user.email", "source@example.invalid"])
        .unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    fs::write(repo.root().join("README.md"), "Shared file").unwrap();
    repo.add_all().unwrap();
    repo.commit("File line").unwrap();
    let base = git(repo.root(), &["rev-parse", "HEAD"]);
    assert_success(&run(&["--yes", "push", "alice/app@main"]));
    repo.git(&["checkout", "-b", "work"]).unwrap();
    let mut session = meta::Meta::new(
        format!("agit-{}", "a".repeat(40)),
        "claude-code".into(),
        String::new(),
    );
    session.session_privacy = Some(SessionPrivacy {
        scheme: agit::domain::session_privacy::SessionScheme::SessionOwnerV2,
        ..SessionPrivacy::new(Some(
            AccountIdentity::new(&hub.base, "session-owner").unwrap(),
        ))
        .unwrap()
    });
    session.committer_account = Some(AccountIdentity::new(&hub.base, "account-1").unwrap());
    let turn = |content: &str| {
        let log = transcript::wrap_lines(
            &format!(
                "{}\n",
                json!({"type":"user","message":{"role":"user","content":content}})
            ),
            "claude-code",
            &session.session,
        );
        storage::write_snapshot(repo.root(), &log, &log).unwrap();
        meta::write(repo.root(), &session).unwrap();
        repo.add_all().unwrap();
        repo.commit(content).unwrap();
        git(repo.root(), &["rev-parse", "HEAD"])
    };
    for index in 0..3 {
        turn(&format!("turn {index}"));
    }
    assert_success(&run(&["--yes", "push", "alice/app@work"]));
    let published = git(&remote, &["rev-parse", "refs/heads/work"]);
    let public_session = |commit: &str| {
        PrivacyEnvelope::parse(
            git(
                &remote,
                &["show", &format!("{commit}:privacy/envelope.json")],
            )
            .as_bytes(),
        )
        .unwrap()
        .session_binding
        .unwrap()
        .published_session_id
    };
    let accepted_identity = public_session(&published);

    // The candidate cache holds the session's public identity; losing it must not change it.
    let stores = home
        .join("repos/alice/app/.git/agit/privacy-publication")
        .read_dir()
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.join(".git/session-candidates").exists())
        .collect::<Vec<_>>();
    assert_eq!(stores.len(), 1);
    fs::remove_dir_all(stores[0].join(".git/session-candidates")).unwrap();

    let earlier = hub.previews.lock().unwrap().len();
    turn("turn 3");
    turn("turn 4");
    assert_success(&run(&["--yes", "push", "alice/app@work"]));
    let head = git(&remote, &["rev-parse", "refs/heads/work"]);
    assert_eq!(
        git(&remote, &["rev-parse", &format!("{head}^^")]),
        published
    );
    assert_eq!(public_session(&head), accepted_identity);
    let previews = hub.previews.lock().unwrap()[earlier..].to_vec();
    assert!(!previews.is_empty());
    for preview in previews {
        let declared: SessionPublicationManifest =
            serde_json::from_value(preview["manifest"].clone()).unwrap();
        assert_eq!(
            declared
                .preserved_commits
                .iter()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>(),
            std::collections::BTreeSet::from([base.clone(), published.clone()])
        );
        assert_eq!(declared.snapshots.len(), 2);
        for snapshot in &declared.snapshots {
            let PublishedSnapshot::SessionOwnerV2 { snapshot, .. } = snapshot else {
                panic!("an encrypted turn is declared as {snapshot:?}");
            };
            assert_eq!(snapshot.binding.published_session_id, accepted_identity);
        }
    }
}

/// Automatic upload into a public repository is switched on only by the person, at a terminal.
/// Each row names an entry point, the destination's audience, whether `--yes` was given and
/// whether a terminal is attached. Without a terminal a public switch-on is refused with or without
/// `--yes` and leaves automatic upload off; private, and user-wide or destination-less switches,
/// stay allowed, but an automatic push into a public repository that nobody enrolled is refused
/// before upload. A gate that accepted `--yes` would let an agent enroll a public repository on
/// a relayed sentence; a gate only at the entry points would let an inherited user preference
/// publish publicly. Choosing "On", or inheriting an enabled preference, in a first checkout's
/// menu at a terminal records the same enrollment as `--auto-push=true` there; a menu that only
/// stored the switch would leave every later automatic upload refused.
#[cfg(unix)]
#[test]
fn automatic_upload_into_a_public_repository_is_switched_on_only_by_the_person() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agit");
    let remote_root = temp.path().join("remote");
    let remote = remote_root.join("alice/app.git");
    let hub = Hub::start(&remote_root, [28; 32]);
    let sign_in = |home: &Path| {
        startup_cache::seed(home);
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
    };
    sign_in(&home);
    let project = temp.path().join("project");
    fs::create_dir_all(&project).unwrap();
    let run_in = |home: &Path, args: &[&str], automatic: bool| {
        let environment = git_command(temp.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear();
        for (name, value) in environment.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command
            .current_dir(temp.path())
            .env("AGIT_HOME", home)
            .env("CODEX_HOME", home.join("codex"))
            .env("CLAUDE_CONFIG_DIR", home.join("claude"))
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
        if automatic {
            command.env("AGIT_AUTO_PUSH", "1");
        }
        command.output().unwrap()
    };
    let run = |args: &[&str], automatic: bool| run_in(&home, args, automatic);
    let repo = Repo::init(&home.join("repos/alice/app")).unwrap();
    repo.git(&["config", "http.userAgent", TRANSPORT_USER_AGENT])
        .unwrap();
    repo.git(&["config", "user.name", "Source Writer"]).unwrap();
    repo.git(&["config", "user.email", "source@example.invalid"])
        .unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    fs::write(repo.root().join("README.md"), "Shared file").unwrap();
    repo.add_all().unwrap();
    repo.commit("File line").unwrap();
    assert_success(&run(
        &["new", "alice/app", "-b", "work", "--no-launch"],
        false,
    ));
    assert_success(&run(&["--yes", "push", "alice/app@main"], false));
    let work = home.join("worktrees/alice/app/work");
    let enrollment = repo.root().join(".git/agit/privacy-auto-consent.json");
    // A terminal with no agent session: the person typing the command. When `answer` is given,
    // it is typed once the automatic-upload menu is drawn.
    let run_terminal = |home: &Path, args: &[&str], answer: Option<&str>| {
        use portable_pty::{CommandBuilder, PtySize, native_pty_system};
        let pair = native_pty_system().openpty(PtySize::default()).unwrap();
        let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_agit"));
        builder.env_clear();
        for (name, value) in git_command(temp.path()).get_envs() {
            if let Some(value) = value {
                builder.env(name, value);
            }
        }
        builder.env("AGIT_HOME", home);
        builder.env("CODEX_HOME", home.join("codex"));
        builder.env("CLAUDE_CONFIG_DIR", home.join("claude"));
        builder.env("AGIT_HUB_URL", &hub.base);
        builder.env("AGIT_TUI", "0");
        builder.env("AGIT_USE_SYSTEM_GIT", "1");
        builder.env("AGIT_SECRETS_KEYSTORE", "file");
        builder.env("TERM", "xterm");
        builder.cwd(temp.path());
        builder.args(args);
        let mut child = pair.slave.spawn_command(builder).unwrap();
        drop(pair.slave);
        let mut keyboard = pair.master.take_writer().unwrap();
        let mut reader = pair.master.try_clone_reader().unwrap();
        let (sender, chunks) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let mut buffer = [0; 4096];
            while let Ok(count) = reader.read(&mut buffer) {
                if count == 0 || sender.send(buffer[..count].to_vec()).is_err() {
                    break;
                }
            }
        });
        let mut answer = answer;
        let mut screen = String::new();
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            while let Ok(chunk) = chunks.try_recv() {
                screen.push_str(&String::from_utf8_lossy(&chunk));
            }
            if let Some(keys) = answer
                && screen.contains("Automatically push settled turns")
            {
                keyboard.write_all(keys.as_bytes()).unwrap();
                keyboard.flush().unwrap();
                answer = None;
            }
            if let Some(status) = child.try_wait().unwrap() {
                assert!(answer.is_none(), "agit {args:?} never asked: {screen}");
                break status.success();
            }
            assert!(
                Instant::now() < deadline,
                "agit {args:?} waited at a terminal: {screen}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    };
    let effective = |home: &Path| {
        let out = run_in(
            home,
            &["--json", "config", "--repo", "alice/app", "push.auto"],
            false,
        );
        assert_success(&out);
        let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        value["result"]["value"]["setting"]["effective"] == "true"
    };
    let project_dir = project.to_str().unwrap();
    let repo_switch: &[&str] = &["config", "--repo", "alice/app", "push.auto", "true"];
    let clone_switch: &[&str] = &["clone", "alice/app", "--auto-push=true"];
    let bind_switch: &[&str] = &[
        "project",
        "bind",
        project_dir,
        "--repo",
        "alice/app",
        "--history",
        "none",
        "--auto-upload",
    ];
    let user_switch: &[&str] = &["config", "push.auto", "true"];
    let init_switch: &[&str] = &["init", "fresh", "--no-bind", "--auto-push=true"];
    let menu_clone: &[&str] = &["clone", "alice/app"];
    const ON: &str = "\x1b[B\r";
    const INHERIT: &str = "\r";
    const ESCAPE: &str = "\x1b";
    const QUIT: &str = "q";
    struct Row<'a> {
        entry: &'a [&'a str],
        audience: &'static str,
        yes: bool,
        terminal: bool,
        /// Keys typed at the automatic-upload menu of a first checkout in a fresh home.
        menu: Option<&'static str>,
        /// The user preference the menu offers to inherit is on.
        inherits_on: bool,
        /// The menu records the person's enrollment of the public destination.
        enrolls: bool,
        switched_on: bool,
        /// Whether an automatic push from the main checkout then publishes.
        publishes: Option<bool>,
    }
    let row = |entry, audience, yes, terminal, switched_on, publishes| Row {
        entry,
        audience,
        yes,
        terminal,
        menu: None,
        inherits_on: false,
        enrolls: false,
        switched_on,
        publishes,
    };
    let menu = |keys, audience, inherits_on, enrolls, publishes| Row {
        entry: menu_clone,
        audience,
        yes: false,
        terminal: true,
        menu: Some(keys),
        inherits_on,
        enrolls,
        switched_on: true,
        publishes,
    };
    let rows = [
        row(repo_switch, "public", false, false, false, None),
        row(repo_switch, "public", true, false, false, None),
        row(repo_switch, "public", false, true, true, Some(true)),
        row(repo_switch, "private", false, false, true, Some(true)),
        row(clone_switch, "public", false, false, false, None),
        row(clone_switch, "public", true, false, false, None),
        row(clone_switch, "private", false, false, true, None),
        row(bind_switch, "public", true, false, false, None),
        row(user_switch, "public", true, false, true, Some(false)),
        row(init_switch, "public", false, false, true, None),
        menu(ON, "public", false, true, Some(true)),
        menu(INHERIT, "public", true, true, Some(true)),
        menu(ON, "private", false, false, None),
        // A cancelled menu is not an answer: the inherited preference stays the user's, and
        // nothing enrolls the public destination, so the next automatic upload is refused.
        menu(ESCAPE, "public", true, false, Some(false)),
        menu(QUIT, "public", true, false, Some(false)),
    ];
    for (index, row) in rows.into_iter().enumerate() {
        let Row {
            entry,
            audience,
            yes,
            terminal,
            menu,
            inherits_on,
            enrolls,
            switched_on,
            publishes,
        } = row;
        let case = format!(
            "{entry:?} {audience} yes={yes} terminal={terminal} menu={menu:?} inherits_on={inherits_on}"
        );
        repo.git(&["config", "--local", "--unset-all", "agit.autoPush"])
            .ok();
        assert_success(&run(&["config", "--unset", "push.auto"], false));
        let _ = fs::remove_file(&enrollment);
        *hub.visibility.lock().unwrap() = audience;

        if let Some(keys) = menu {
            // A first checkout is where the menu is asked; the main checkout already exists.
            let fresh = temp.path().join(format!("fresh-{index}"));
            sign_in(&fresh);
            if inherits_on {
                assert_success(&run_in(&fresh, &["config", "push.auto", "true"], false));
            }
            assert!(run_terminal(&fresh, entry, Some(keys)), "{case}");
            assert_eq!(effective(&fresh), switched_on, "{case}");
            assert_eq!(
                fresh
                    .join("repos/alice/app/.git/agit/privacy-auto-consent.json")
                    .exists(),
                enrolls,
                "{case}"
            );
            if let Some(publishes) = publishes {
                let branch = format!("menu-{index}");
                git(
                    &fresh.join("repos/alice/app"),
                    &["config", "http.userAgent", TRANSPORT_USER_AGENT],
                );
                assert_success(&run_in(
                    &fresh,
                    &["new", "alice/app", "-b", &branch, "--no-launch"],
                    false,
                ));
                let work = fresh.join("worktrees/alice/app").join(&branch);
                fs::write(work.join("README.md"), format!("Revision {index}")).unwrap();
                git(
                    &work,
                    &[
                        "-c",
                        "user.name=Source Writer",
                        "-c",
                        "user.email=source@example.invalid",
                        "commit",
                        "-am",
                        "Revise file",
                    ],
                );
                let local = git(&work, &["rev-parse", "HEAD"]);
                hub.posts.store(0, Ordering::Release);
                let pushed = run_in(
                    &fresh,
                    &["--json", "push", &format!("alice/app@{branch}")],
                    true,
                );
                assert_eq!(pushed.status.success(), publishes, "{case}: {pushed:?}");
                let published = git(&remote, &["for-each-ref", &format!("refs/heads/{branch}")]);
                if publishes {
                    assert_eq!(
                        git(&remote, &["rev-parse", &format!("refs/heads/{branch}")]),
                        local
                    );
                } else {
                    assert!(
                        format!(
                            "{}{}",
                            String::from_utf8_lossy(&pushed.stdout),
                            String::from_utf8_lossy(&pushed.stderr)
                        )
                        .contains("was not switched on by the person"),
                        "{case}: {pushed:?}"
                    );
                    assert_eq!(hub.posts.load(Ordering::Acquire), 0, "{case}");
                    assert_eq!(published, "", "{case}");
                }
            }
            continue;
        }

        let mut args = Vec::new();
        if yes {
            args.push("--yes");
        }
        args.extend_from_slice(entry);
        if terminal {
            assert_eq!(run_terminal(&home, &args, None), switched_on, "{case}");
        } else {
            let out = run(&args, false);
            assert_eq!(out.status.success(), switched_on, "{case}: {out:?}");
            if !switched_on {
                assert!(
                    String::from_utf8_lossy(&out.stderr).contains("themselves in a terminal"),
                    "{case}: {out:?}"
                );
            }
        }
        if entry[0] != "init" {
            assert_eq!(effective(&home), switched_on, "{case}");
        }

        if let Some(publishes) = publishes {
            fs::write(work.join("README.md"), format!("Revision {index}")).unwrap();
            git(&work, &["commit", "-am", "Revise file"]);
            let local = git(&work, &["rev-parse", "HEAD"]);
            let before = git(&remote, &["for-each-ref", "refs/heads/work"]);
            hub.posts.store(0, Ordering::Release);
            let pushed = run(&["--json", "push", "alice/app@work"], true);
            assert_eq!(pushed.status.success(), publishes, "{case}: {pushed:?}");
            if publishes {
                assert_eq!(git(&remote, &["rev-parse", "refs/heads/work"]), local);
            } else {
                assert!(
                    format!(
                        "{}{}",
                        String::from_utf8_lossy(&pushed.stdout),
                        String::from_utf8_lossy(&pushed.stderr)
                    )
                    .contains("was not switched on by the person"),
                    "{case}: {pushed:?}"
                );
                assert_eq!(hub.posts.load(Ordering::Acquire), 0, "{case}");
                assert_eq!(git(&remote, &["for-each-ref", "refs/heads/work"]), before);
            }
        }
    }
}

/// A session branch that moved on the Hub while this checkout settled its own turn is refused
/// with the divergence and the commands that resolve it, piped and at a terminal. At a terminal
/// the stopped stage ends its own row: a progress display that leaves its last row open puts the
/// error on the stage's row, where a reader of that row sees only that the stage stopped.
#[test]
fn diverged_session_branch_names_the_divergence_and_next_commands() {
    let temp = tempfile::tempdir().unwrap();
    let desktop = temp.path().join("desktop");
    let laptop = temp.path().join("laptop");
    let remote_root = temp.path().join("remote");
    let remote = remote_root.join("alice/app.git");
    let hub = Hub::start(&remote_root, [29; 32]);
    let sign_in = |home: &Path| {
        startup_cache::seed(home);
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
    };
    sign_in(&desktop);
    sign_in(&laptop);
    let run_in = |home: &Path, args: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear();
        for (name, value) in git_command(temp.path()).get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command
            .current_dir(temp.path())
            .env("AGIT_HOME", home)
            .env("CODEX_HOME", home.join("codex"))
            .env("CLAUDE_CONFIG_DIR", home.join("claude"))
            .env("AGIT_HUB_URL", &hub.base)
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("AGIT_USE_SYSTEM_GIT", "1")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(unix) { "file" } else { "os" },
            )
            .stdin(Stdio::null())
            .args(args)
            .output()
            .unwrap()
    };
    let repo = Repo::init(&desktop.join("repos/alice/app")).unwrap();
    repo.git(&["config", "http.userAgent", TRANSPORT_USER_AGENT])
        .unwrap();
    repo.git(&["config", "user.name", "Source Writer"]).unwrap();
    repo.git(&["config", "user.email", "source@example.invalid"])
        .unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    fs::write(repo.root().join("README.md"), "Shared file").unwrap();
    repo.add_all().unwrap();
    repo.commit("File line").unwrap();
    assert_success(&run_in(
        &desktop,
        &["new", "alice/app", "-b", "work", "--no-launch"],
    ));
    assert_success(&run_in(&desktop, &["--yes", "push", "alice/app@main"]));
    assert_success(&run_in(&desktop, &["--yes", "push", "alice/app@work"]));
    assert_success(&run_in(&laptop, &["clone", "alice/app", "--no-bind"]));
    let laptop_repo = laptop.join("repos/alice/app");
    git(
        &laptop_repo,
        &["config", "http.userAgent", TRANSPORT_USER_AGENT],
    );
    let laptop_work = temp.path().join("laptop-work");
    git(
        &laptop_repo,
        &["worktree", "add", laptop_work.to_str().unwrap(), "work"],
    );

    let desktop_work = desktop.join("worktrees/alice/app/work");
    fs::write(desktop_work.join("README.md"), "Desktop turn").unwrap();
    git(&desktop_work, &["commit", "-am", "Desktop turn"]);
    assert_success(&run_in(&desktop, &["push", "alice/app@work"]));
    let hub_tip = git(&remote, &["rev-parse", "refs/heads/work"]);
    git(&laptop_work, &["config", "user.name", "Source Writer"]);
    git(
        &laptop_work,
        &["config", "user.email", "source@example.invalid"],
    );
    fs::write(laptop_work.join("NOTES.md"), "Laptop turn").unwrap();
    git(&laptop_work, &["add", "NOTES.md"]);
    git(
        &laptop_work,
        &["-c", "commit.gpgsign=false", "commit", "-m", "Laptop turn"],
    );
    let explains = |output: &str| {
        for expected in [
            "error remote branch work has diverged from this checkout",
            "local unpublished commits are kept",
            "`agit fork alice/app@work -b work-local`, then `agit push alice/app@work-local`",
            &format!(
                "`agit fetch alice/app`, then `agit merge alice/app@{} --into alice/app@work`",
                &hub_tip[..12]
            ),
        ] {
            assert!(output.contains(expected), "{expected}: {output}");
        }
    };
    let refused = run_in(&laptop, &["push", "alice/app@work"]);
    assert!(!refused.status.success(), "{refused:?}");
    explains(&String::from_utf8_lossy(&refused.stderr));

    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    let pair = native_pty_system().openpty(PtySize::default()).unwrap();
    let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_agit"));
    builder.env_clear();
    for (name, value) in git_command(temp.path()).get_envs() {
        if let Some(value) = value {
            builder.env(name, value);
        }
    }
    builder.env("AGIT_HOME", &laptop);
    builder.env("CODEX_HOME", laptop.join("codex"));
    builder.env("CLAUDE_CONFIG_DIR", laptop.join("claude"));
    builder.env("AGIT_HUB_URL", &hub.base);
    builder.env("AGIT_TUI", "0");
    builder.env("AGIT_USE_SYSTEM_GIT", "1");
    builder.env("AGIT_SECRETS_KEYSTORE", "file");
    builder.env("TERM", "xterm");
    builder.cwd(temp.path());
    builder.args(["push", "alice/app@work"]);
    let mut child = pair.slave.spawn_command(builder).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let (sender, chunks) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let mut buffer = [0; 4096];
        while let Ok(count) = reader.read(&mut buffer) {
            if count == 0 || sender.send(buffer[..count].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut screen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        while let Ok(chunk) = chunks.try_recv() {
            screen.extend(chunk);
        }
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "push waited at a terminal");
        thread::sleep(Duration::from_millis(20));
    };
    thread::sleep(Duration::from_millis(200));
    while let Ok(chunk) = chunks.try_recv() {
        screen.extend(chunk);
    }
    assert!(!status.success());
    let screen = String::from_utf8_lossy(&screen)
        .replace("\u{1b}[2K", "")
        .replace("\u{1b}[31m", "")
        .replace("\u{1b}[39m", "");
    explains(&screen);
    let stopped = screen
        .lines()
        .find(|row| row.contains("Reading remote history and session modes — stopped"))
        .unwrap_or_else(|| panic!("{screen}"));
    assert!(!stopped.contains("error"), "{screen}");
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/work"]), hub_tip);
}
