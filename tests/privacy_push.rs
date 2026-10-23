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
    deny: Arc<AtomicBool>,
    unsupported_policy: Arc<AtomicBool>,
    corrupt_receipt: Arc<AtomicBool>,
    creations: Arc<AtomicUsize>,
    viewing_public: Arc<Mutex<String>>,
    key_version: Arc<AtomicUsize>,
    key_requests: Arc<AtomicUsize>,
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
        let deny = Arc::new(AtomicBool::new(false));
        let denied = Arc::clone(&deny);
        let unsupported_policy = Arc::new(AtomicBool::new(false));
        let unsupported = Arc::clone(&unsupported_policy);
        let corrupt_receipt = Arc::new(AtomicBool::new(false));
        let corrupting = Arc::clone(&corrupt_receipt);
        let confirmations = Arc::new(AtomicUsize::new(0));
        let confirmed = Arc::clone(&confirmations);
        let creations = Arc::new(AtomicUsize::new(0));
        let created = Arc::clone(&creations);
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
                let mut user_agent = String::new();
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
                    if let Some(value) = lower.strip_prefix("user-agent:") {
                        user_agent = value.trim().to_owned();
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
                    ("GET", "/api/auth/me") => {
                        Some(json!({"account_id":"account-1","username":"alice"}))
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
                            json!({"agent_id":AGENT_ID,"owner":"alice","name":"app","push_url":url,"web_url":format!("{resolver_hub}/alice/app")}),
                        )
                    }
                    ("PUT", "/api/agents/alice/app/privacy/strategy") => {
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
                        assert_eq!(preview["policy_digest"], strategy["policy_digest"]);
                        assert_eq!(
                            preview["expected_refs_digest"],
                            remote_refs_digest(&root.join("alice/app.git"))
                        );
                        assert_eq!(preview["envelope_format_version"], 2);
                        let manifest: agit::domain::session_privacy::SessionPublicationManifest =
                            serde_json::from_value(preview["manifest"].clone()).unwrap();
                        manifest.validate().unwrap();
                        assert_eq!(
                            preview["snapshot_digest"],
                            agit::domain::privacy_envelope::digest_bytes(
                                &serde_json::to_vec(&manifest).unwrap()
                            )
                        );
                        assert_eq!(
                            preview["receiver_scope"],
                            format!("sessions:private:{AGENT_ID}")
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
                        json!({"agent_id":AGENT_ID,"owner":"alice","name":"app","clone_url":url,"visibility":"private"}),
                    ),
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
            deny,
            unsupported_policy,
            corrupt_receipt,
            creations,
            viewing_public,
            key_version,
            key_requests,
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
    let run = |args: &[&str], automatic: bool| {
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
            .args(args);
        if automatic {
            command.env("AGIT_AUTO_PUSH", "1");
        }
        command.output().unwrap()
    };
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
    let mode_args = ["privacy", "mode", "alice/app@unencrypted", "plain"];
    assert!(!run(&mode_args, false).status.success());
    let unchanged = run(
        &["--json", "privacy", "mode", "alice/app@unencrypted"],
        false,
    );
    assert_success(&unchanged);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&unchanged.stdout).unwrap()["result"]["value"]
            ["local_mode"],
        "session_owner_v2"
    );
    assert_success(&run(
        &["--yes", "privacy", "mode", "alice/app@unencrypted", "plain"],
        false,
    ));
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
    assert_eq!(hub.creations.load(Ordering::Acquire), 0);
    assert!(!remote.exists());
    let unconfirmed = run(&["push", "alice/app@main"], false);
    assert!(!unconfirmed.status.success(), "{unconfirmed:?}");
    assert_eq!(hub.creations.load(Ordering::Acquire), 0);
    hub.corrupt_receipt.store(true, Ordering::Release);
    let create = ["--yes", "push", "alice/app@main"];
    let refused = run(&create, false);
    assert!(!refused.status.success(), "{refused:?}");
    assert_eq!(hub.creations.load(Ordering::Acquire), 1);
    assert_eq!(hub.posts.load(Ordering::Acquire), 0);
    assert!(git(&remote, &["for-each-ref"]).is_empty());
    hub.corrupt_receipt.store(false, Ordering::Release);
    assert_success(&run(&create, false));
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
    metadata.session_privacy = Some(
        SessionPrivacy::new(Some(
            AccountIdentity::new(&hub.base, "session-owner").unwrap(),
        ))
        .unwrap(),
    );
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
    assert_ne!(plain_head, second_plain_source);
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
    assert_ne!(
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
        agit::domain::privacy_receipt::PublicationMode::SessionPlain
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
}
