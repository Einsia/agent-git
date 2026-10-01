#![cfg(feature = "cli")]

use agit::{
    domain::repo::Repo,
    hub::identity::{self, RemoteIdentity},
    infra::{
        config,
        credentials::{HubCredential, save_at},
    },
};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

#[path = "support/ordinary_git_receiver.rs"]
mod ordinary_git_receiver;
#[path = "support/publication_http.rs"]
mod publication_http;

const ID: &str = "ef306cd5-e4d3-4a3c-a8ce-e3c2f65b48d4";

struct Hub {
    base: String,
    response: Arc<Mutex<(u16, Value)>>,
    publishing_key: Arc<Mutex<(u16, Value)>>,
    requests: Arc<Mutex<Vec<(String, Value)>>>,
    missing_until_create: Arc<AtomicBool>,
    drop_receive_response: Arc<AtomicBool>,
    reject_advertisement: Arc<AtomicBool>,
    payloads: Arc<Mutex<std::collections::BTreeMap<String, Vec<u8>>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Hub {
    fn start() -> Self {
        Self::start_git(None)
    }

    fn start_git(git_root: Option<PathBuf>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let response = Arc::new(Mutex::new((
            200,
            json!({
                "agent_id": ID, "owner":"alice", "name":"demo", "encryption_enabled":false,
                "visibility":"private", "clone_url":format!("{base}/alice/demo.git"),
                "push_url":format!("{base}/alice/demo.git"), "web_url":format!("{base}/@alice/demo")
            }),
        )));
        let publishing_key = Arc::new(Mutex::new((
            200,
            json!({
                "agent_id": ID, "config_version": 0, "current": null
            }),
        )));
        let served_key = publishing_key.clone();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let missing_until_create = Arc::new(AtomicBool::new(false));
        let missing = missing_until_create.clone();
        let drop_receive_response = Arc::new(AtomicBool::new(false));
        let drop_response = drop_receive_response.clone();
        let reject_advertisement = Arc::new(AtomicBool::new(false));
        let reject_refs = reject_advertisement.clone();
        let payloads = Arc::new(Mutex::new(std::collections::BTreeMap::new()));
        let received_payloads = payloads.clone();
        let served_base = base.clone();
        let (served, recorded, stopping) = (response.clone(), requests.clone(), stop.clone());
        let worker = thread::spawn(move || {
            while !stopping.load(Ordering::Acquire) {
                let (mut stream, _) = match publication_http::accept(&listener) {
                    Ok(pair) => pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("fixture accept failed: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                let mut length = 0;
                let mut content_type = String::new();
                let mut expected_ids = Vec::new();
                let mut authorizations = Vec::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-type:") {
                        content_type = value.trim().into();
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("x-agentgit-expected-agent-id")
                    {
                        expected_ids.push(value.trim().to_owned());
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("authorization")
                    {
                        authorizations.push(value.trim().to_owned());
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let words: Vec<_> = request.split_whitespace().collect();
                let (method, target) = (words[0], words[1]);
                // Publication request counts exclude independent account dictionary synchronization.
                if target != "/api/cli/version" && !target.starts_with("/api/me/privacy/") {
                    recorded.lock().unwrap().push((
                        request.trim().into(),
                        serde_json::from_slice(&body).unwrap_or(Value::Null),
                    ));
                }
                if let Some(root) = &git_root
                    && target.starts_with("/alice/demo.git/")
                {
                    assert_eq!(expected_ids, [ID], "{request}");
                    assert_eq!(authorizations, ["Bearer synthetic-access"], "{request}");
                    if target.ends_with("/info/refs?service=git-receive-pack")
                        && reject_refs.load(Ordering::Acquire)
                    {
                        let body = "publication is awaiting receive reconciliation";
                        write!(stream, "HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                        continue;
                    }
                    let (headers, bytes) = ordinary_git_receiver::respond(
                        root,
                        &served_base,
                        method,
                        target,
                        &content_type,
                        &body,
                        &mut received_payloads.lock().unwrap(),
                    );
                    if method == "POST"
                        && target.ends_with("/git-receive-pack")
                        && drop_response.swap(false, Ordering::AcqRel)
                    {
                        continue;
                    }
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\n{headers}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        bytes.len()
                    );
                    let _ = stream.write_all(&bytes);
                    continue;
                }
                let (status, response) = if target == "/api/cli/version" {
                    (200, json!({"version": "0.0.0", "tag": "v0.0.0"}))
                } else if target.starts_with("/api/me/privacy/") {
                    (503, json!({"error":"privacy storage unavailable"}))
                } else if target == "/api/agents/alice/demo/privacy/publishing-key" {
                    assert_eq!(method, "GET");
                    assert_eq!(expected_ids, [ID]);
                    assert_eq!(authorizations, ["Bearer synthetic-access"]);
                    served_key.lock().unwrap().clone()
                } else if request.starts_with("GET ") && missing.load(Ordering::Acquire) {
                    (404, json!({"error":"not found"}))
                } else {
                    if request.starts_with("POST /api/agents ") {
                        if let Some(root) = &git_root {
                            ordinary_git_receiver::initialize(&root.join("alice/demo.git"));
                        }
                        missing.store(false, Ordering::Release);
                    }
                    served.lock().unwrap().clone()
                };
                let text = response.to_string();
                write!(stream, "HTTP/1.1 {status} Reply\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len()).unwrap();
            }
        });
        Self {
            base,
            response,
            publishing_key,
            requests,
            missing_until_create,
            drop_receive_response,
            reject_advertisement,
            payloads,
            stop,
            worker: Some(worker),
        }
    }

    fn authenticate(&self, root: &Path) {
        save_at(
            &root.join("agit/credentials").join(format!(
                "{}.json",
                config::hub_host_key(&self.base).unwrap()
            )),
            &HubCredential {
                account_id: Some("account-1".into()),
                username: "alice".into(),
                email: None,
                hub: Some(self.base.clone()),
                access_token: "synthetic-access".into(),
                refresh_token: "synthetic-refresh".into(),
                access_expires_at: "2099-01-01T00:00:00Z".into(),
                refresh_expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .unwrap();
    }

    fn run(&self, root: &Path, args: &[&str]) -> Output {
        self.command(root, args).output().unwrap()
    }

    fn command(&self, root: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", root)
            .env("USERPROFILE", root)
            .env("AGIT_HOME", root.join("agit"))
            .env("AGIT_HUB_URL", &self.base)
            .env("AGIT_TUI", "0")
            .env("CI", "1")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", root.join("absent-gitconfig"))
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .current_dir(root)
            .args(args);
        for name in ["SYSTEMROOT", "WINDIR", "COMSPEC"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command
    }

    fn setting(&self, root: &Path) -> Value {
        let output = self.run(
            root,
            &[
                "--json",
                "config",
                "--repo",
                "alice/demo",
                "privacy.encryption",
            ],
        );
        assert!(output.status.success(), "{output:?}");
        let doc: Value = serde_json::from_slice(&output.stdout).unwrap();
        doc["result"]["value"]["setting"].clone()
    }
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

fn refused(output: Output, message: &str) {
    assert!(!output.status.success(), "{output:?}");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(text.contains(message), "{text}");
}

fn ordinary_source(root: &Path) -> (Repo, String, String) {
    use agit::domain::{meta, storage, transcript};
    let repo = Repo::init(&root.join("agit/repos/alice/demo")).unwrap();
    repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    std::fs::write(
        repo.root().join("AGENTS.md"),
        "Shared ordinary instructions.\n",
    )
    .unwrap();
    repo.add_all().unwrap();
    repo.commit("Shared file line").unwrap();
    repo.git(&["branch", "assets"]).unwrap();
    repo.git(&["checkout", "-b", "work"]).unwrap();
    let session = format!("agit-{}", "a".repeat(40));
    let log = transcript::wrap_lines(
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"Ordinary source conversation\"}}\n",
        "claude-code",
        &session,
    );
    storage::write_snapshot(repo.root(), &log, &log).unwrap();
    meta::write(
        repo.root(),
        &meta::Meta::new(
            session.clone(),
            "claude-code".into(),
            root.to_string_lossy().into(),
        ),
    )
    .unwrap();
    repo.add_all().unwrap();
    repo.commit("Original ordinary session").unwrap();
    repo.tag(&format!("{session}/1.1")).unwrap();
    (repo, session, log)
}

#[test]
fn ordinary_history_continues_with_original_ids_file_lines_tags_lfs_and_automatic_publication() {
    use agit::domain::{
        meta,
        privacy_receipt::{PublicationMode, PublicationReceipt},
        storage,
    };
    use sha2::Digest;
    let root = tempfile::tempdir().unwrap();
    let remote_root = root.path().join("remote");
    let remote = remote_root.join("alice/demo.git");
    ordinary_git_receiver::initialize(&remote);
    let hub = Hub::start_git(Some(remote_root));
    hub.authenticate(root.path());
    let (repo, session, log) = ordinary_source(root.path());
    let original = repo.git(&["rev-parse", "work"]).unwrap();
    repo.git(&[
        "push",
        remote.to_str().unwrap(),
        "refs/heads/main",
        "refs/heads/work",
        "--tags",
    ])
    .unwrap();
    let original_tags = ordinary_git_receiver::git(&remote, &["for-each-ref", "refs/tags"]);
    repo.set_remote(&format!("{}/alice/demo.git", hub.base))
        .unwrap();
    let pointer_bytes = b"Ordinary LFS attachment with readable content.\n";
    let oid = hex::encode(sha2::Sha256::digest(pointer_bytes));
    let cache = repo
        .common_dir()
        .unwrap()
        .join("lfs/objects")
        .join(&oid[..2])
        .join(&oid[2..4])
        .join(&oid);
    std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
    std::fs::write(&cache, pointer_bytes).unwrap();
    std::fs::write(
        repo.root().join("attachment.txt"),
        format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {}\n",
            pointer_bytes.len()
        ),
    )
    .unwrap();
    let continued = format!(
        "{log}{}",
        agit::domain::transcript::wrap_lines(
            "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":\"Appended ordinary turn\"}}\n",
            "claude-code",
            &session
        )
    );
    storage::write_snapshot(repo.root(), &continued, &continued).unwrap();
    repo.add_all().unwrap();
    repo.commit("Append ordinary turn and attachment").unwrap();
    repo.tag(&format!("{session}/1.2")).unwrap();
    let head = repo.git(&["rev-parse", "work"]).unwrap();
    let automatic = || {
        hub.command(root.path(), &["push", "alice/demo@work"])
            .env("AGIT_AUTO_PUSH", "1")
            .output()
            .unwrap()
    };
    refused(automatic(), "automatic publication is disabled");
    repo.set_auto_push(Some(true)).unwrap();
    let before = ordinary_git_receiver::git(&remote, &["for-each-ref"]);
    let preview = hub.run(
        root.path(),
        &["push", "alice/demo", "--all", "--dry-run", "--show-preview"],
    );
    assert!(preview.status.success(), "{preview:?}");
    assert!(String::from_utf8_lossy(&preview.stdout).contains("encryption: disabled"));
    assert_eq!(
        ordinary_git_receiver::git(&remote, &["for-each-ref"]),
        before
    );
    assert!(hub.payloads.lock().unwrap().is_empty());
    refused(
        hub.run(
            root.path(),
            &["--yes", "push", "alice/demo@work", "--encryption=true"],
        ),
        "fixed at creation",
    );
    assert_eq!(
        ordinary_git_receiver::git(&remote, &["for-each-ref"]),
        before
    );
    let output = hub.run(root.path(), &["--yes", "push", "alice/demo", "--all"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        ordinary_git_receiver::git(&remote, &["rev-parse", "work"]),
        head
    );
    assert_eq!(
        ordinary_git_receiver::git(&remote, &["rev-parse", "work^"]),
        original
    );
    assert_eq!(
        ordinary_git_receiver::git(&remote, &["rev-parse", "assets"]),
        repo.git(&["rev-parse", "assets"]).unwrap()
    );
    assert_eq!(repo.git(&["rev-parse", "origin/work"]).unwrap(), head);
    assert!(
        ordinary_git_receiver::git(&remote, &["for-each-ref", "refs/tags"])
            .contains(&original_tags)
    );
    assert_eq!(
        hub.payloads.lock().unwrap().get(&oid).unwrap(),
        pointer_bytes
    );
    let receipt = PublicationReceipt::load(&repo, "work").unwrap().unwrap();
    assert_eq!(receipt.mode, PublicationMode::Ordinary);
    assert_eq!(receipt.source, head);
    assert_eq!(receipt.published, head);
    assert_eq!(
        receipt.projected_session_id.as_deref(),
        Some(session.as_str())
    );
    assert!(receipt.recipient.is_none() && receipt.policy_digest.is_none());
    assert_eq!(
        meta::read_at_ref_result(&repo, &head)
            .unwrap()
            .unwrap()
            .session,
        session
    );
    assert!(
        repo.show_result(&head, "privacy/envelope.json")
            .unwrap()
            .is_none()
    );

    std::fs::write(
        repo.root().join("continuation.txt"),
        "Another ordinary change.\n",
    )
    .unwrap();
    std::fs::remove_file(repo.root().join("attachment.txt")).unwrap();
    std::fs::remove_dir_all(repo.common_dir().unwrap().join("lfs")).unwrap();
    repo.add_all().unwrap();
    repo.commit("Automatic ordinary continuation").unwrap();
    let next = repo.git(&["rev-parse", "work"]).unwrap();
    let requests_before = hub.requests.lock().unwrap().len();
    hub.drop_receive_response.store(true, Ordering::Release);
    assert!(!automatic().status.success());
    assert_eq!(
        ordinary_git_receiver::git(&remote, &["rev-parse", "work"]),
        next
    );
    assert_eq!(
        PublicationReceipt::load(&repo, "work").unwrap().unwrap(),
        receipt
    );
    let retry = automatic();
    assert!(retry.status.success(), "{retry:?}");
    assert!(
        !cache.exists(),
        "recovery must use private inspection storage"
    );
    assert_eq!(
        PublicationReceipt::load(&repo, "work")
            .unwrap()
            .unwrap()
            .published,
        next
    );
    let commands = hub.requests.lock().unwrap();
    assert!(commands[requests_before..].iter().all(|(request, body)| {
        !request.contains("/info/lfs/objects/batch") || body["operation"] != "download"
    }));
    assert!(
        commands[requests_before..]
            .iter()
            .all(|(request, _)| !request.starts_with("PUT "))
    );
    assert!(
        commands
            .iter()
            .all(|(request, _)| !request.contains("/privacy/")
                && !request.starts_with("POST /api/agents "))
    );
    assert!(meta::is_bare_id(&session));
    drop(commands);

    let cold = root.path().join("cold");
    std::fs::create_dir(&cold).unwrap();
    hub.authenticate(&cold);
    let cloned = hub.run(
        &cold,
        &["clone", "alice/demo@work", "--no-bind", "--auto-push=false"],
    );
    assert!(cloned.status.success(), "{cloned:?}");
    let cloned_repo = Repo::at(cold.join("agit/repos/alice/demo"));
    assert!(
        !cloned_repo
            .common_dir()
            .unwrap()
            .join("lfs/objects")
            .exists()
    );
    let repeated = hub.run(&cold, &["--yes", "push", "alice/demo@work"]);
    assert!(repeated.status.success(), "{repeated:?}");
    assert_eq!(
        ordinary_git_receiver::git(&remote, &["rev-parse", "work"]),
        next
    );

    let accepted = PublicationReceipt::load(&repo, "work").unwrap().unwrap();
    let refs = ordinary_git_receiver::git(&remote, &["for-each-ref"]);
    // Missing or corrupt new payloads cannot be covered by the accepted history baseline.
    let new_payload = b"Unpublished LFS attachment.\n";
    let new_oid = hex::encode(sha2::Sha256::digest(new_payload));
    std::fs::write(
        repo.root().join("new-attachment.txt"),
        format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{new_oid}\nsize {}\n",
            new_payload.len()
        ),
    )
    .unwrap();
    repo.add_all().unwrap();
    repo.commit("Reference an unavailable attachment").unwrap();
    refused(automatic(), "cannot recover historical LFS object");
    hub.payloads
        .lock()
        .unwrap()
        .insert(new_oid, vec![b'x'; new_payload.len()]);
    refused(automatic(), "cannot recover historical LFS object");
    assert_eq!(ordinary_git_receiver::git(&remote, &["for-each-ref"]), refs);
    assert_eq!(
        PublicationReceipt::load(&repo, "work").unwrap().unwrap(),
        accepted
    );
}

#[test]
fn unchanged_ordinary_push_requires_live_reconciliation_before_saving_a_receipt() {
    use agit::domain::privacy_receipt::{
        PublicationReceipt, SUPERVISOR_RESULT_ENV, SupervisorPushRequest,
    };
    let root = tempfile::tempdir().unwrap();
    let remote_root = root.path().join("remote");
    let remote = remote_root.join("alice/demo.git");
    ordinary_git_receiver::initialize(&remote);
    let hub = Hub::start_git(Some(remote_root));
    hub.authenticate(root.path());
    let (repo, _, _) = ordinary_source(root.path());
    repo.git(&["push", remote.to_str().unwrap(), "--all"])
        .unwrap();
    repo.git(&["push", remote.to_str().unwrap(), "--tags"])
        .unwrap();
    repo.set_remote(&format!("{}/alice/demo.git", hub.base))
        .unwrap();
    let destination = RemoteIdentity::new(&hub.base, ID).unwrap();
    identity::pin(&repo, &destination).unwrap();
    repo.set_auto_push(Some(true)).unwrap();
    let head = repo.git(&["rev-parse", "work"]).unwrap();
    let refs = ordinary_git_receiver::git(&remote, &["for-each-ref"]);
    let request = SupervisorPushRequest {
        version: 1,
        request_id: uuid::Uuid::new_v4().to_string(),
        repository: "alice/demo".into(),
        branch: "work".into(),
        source: head.clone(),
        destination,
        notification_id: None,
    };
    let pending = serde_json::to_vec(&request).unwrap();
    let result = root.path().join("supervisor-result.json");
    let push = || {
        std::fs::write(&result, &pending).unwrap();
        let mut command = hub.command(root.path(), &["--yes", "push", "alice/demo@work"]);
        command
            .env(SUPERVISOR_RESULT_ENV, &result)
            .output()
            .unwrap()
    };

    hub.reject_advertisement.store(true, Ordering::Release);
    refused(push(), "answered 503 to the push-access probe");
    assert!(PublicationReceipt::load(&repo, "work").unwrap().is_none());
    assert_eq!(std::fs::read(&result).unwrap(), pending);

    hub.reject_advertisement.store(false, Ordering::Release);
    let accepted = push();
    assert!(accepted.status.success(), "{accepted:?}");
    assert!(String::from_utf8_lossy(&accepted.stdout).contains("up to date"));
    let receipt = PublicationReceipt::load(&repo, "work").unwrap().unwrap();
    assert_eq!(receipt.published, head);
    assert_eq!(request.read_result(&result).unwrap(), receipt);
    let request_count = hub.requests.lock().unwrap().len();

    hub.reject_advertisement.store(true, Ordering::Release);
    refused(push(), "answered 503 to the push-access probe");
    assert_eq!(
        PublicationReceipt::load(&repo, "work").unwrap().unwrap(),
        receipt
    );
    assert_eq!(std::fs::read(&result).unwrap(), pending);
    assert_eq!(ordinary_git_receiver::git(&remote, &["for-each-ref"]), refs);
    let requests = hub.requests.lock().unwrap();
    assert!(
        requests[request_count..]
            .iter()
            .any(|(request, _)| { request.contains("/info/refs?service=git-receive-pack") })
    );
    assert!(
        requests
            .iter()
            .all(|(request, _)| !request.starts_with("POST /alice/demo.git/git-receive-pack"))
    );
}

#[test]
fn initial_ordinary_push_creates_without_a_viewing_password_and_never_uses_a_failed_mode_lookup() {
    let root = tempfile::tempdir().unwrap();
    let remote_root = root.path().join("remote");
    std::fs::create_dir_all(&remote_root).unwrap();
    let hub = Hub::start_git(Some(remote_root.clone()));
    hub.authenticate(root.path());
    let (repo, _, _) = ordinary_source(root.path());
    let expected_refs = repo
        .git(&["for-each-ref", "refs/heads", "refs/tags"])
        .unwrap();
    let original = hub.response.lock().unwrap().1.clone();
    for (status, mode) in [
        (200, None),
        (200, Some(json!("false"))),
        (503, Some(json!(false))),
    ] {
        let mut response = original.clone();
        response
            .as_object_mut()
            .unwrap()
            .remove("encryption_enabled");
        if let Some(mode) = mode {
            response["encryption_enabled"] = mode;
        }
        *hub.response.lock().unwrap() = (status, response);
        assert!(
            !hub.run(
                root.path(),
                &["--yes", "push", "alice/demo@work", "--encryption=false"]
            )
            .status
            .success()
        );
    }
    assert!(
        hub.requests
            .lock()
            .unwrap()
            .iter()
            .all(|(request, _)| !request.starts_with("POST "))
    );
    assert!(!remote_root.join("alice/demo.git").exists());
    *hub.response.lock().unwrap() = (200, original);
    hub.missing_until_create.store(true, Ordering::Release);
    let output = hub.run(
        root.path(),
        &["--yes", "push", "alice/demo", "--all", "--encryption=false"],
    );
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        ordinary_git_receiver::git(
            &remote_root.join("alice/demo.git"),
            &["for-each-ref", "refs/heads", "refs/tags"]
        ),
        expected_refs
    );
    let requests = hub.requests.lock().unwrap();
    let (_, creation) = requests
        .iter()
        .find(|(request, _)| request.starts_with("POST /api/agents "))
        .unwrap();
    assert_eq!(creation["encryption_enabled"], false);
    assert!(
        requests
            .iter()
            .all(|(request, _)| !request.contains("/privacy/"))
    );
}

/// Without a creation preference, `--yes` or a terminal, a first push creates an ordinary
/// repository, and an automatic push later continues it with only push.auto enabled, while
/// push.auto off refuses it without publishing. An encrypted default stops at the viewing-key
/// gate, a confirmation requirement refuses before creation, a saved-consent requirement refuses
/// the automatic push because nothing here saves one, and ignoring push.auto publishes the
/// refused turn.
#[test]
fn default_publication_is_ordinary_unattended_and_continues_automatically() {
    let root = tempfile::tempdir().unwrap();
    let remote_root = root.path().join("remote");
    std::fs::create_dir_all(&remote_root).unwrap();
    let hub = Hub::start_git(Some(remote_root.clone()));
    hub.authenticate(root.path());
    let (repo, _, _) = ordinary_source(root.path());
    let remote = remote_root.join("alice/demo.git");
    hub.missing_until_create.store(true, Ordering::Release);
    let first = hub.run(root.path(), &["push", "alice/demo@work"]);
    assert!(first.status.success(), "{first:?}");
    assert_eq!(
        ordinary_git_receiver::git(&remote, &["rev-parse", "work"]),
        repo.git(&["rev-parse", "work"]).unwrap()
    );
    let requests = hub.requests.lock().unwrap();
    let (_, creation) = requests
        .iter()
        .find(|(request, _)| request.starts_with("POST /api/agents "))
        .unwrap();
    assert_eq!(creation["encryption_enabled"], false);
    assert!(
        requests
            .iter()
            .all(|(request, _)| !request.contains("/privacy/"))
    );
    drop(requests);

    std::fs::write(repo.root().join("continuation.txt"), "Automatic turn.\n").unwrap();
    repo.add_all().unwrap();
    repo.commit("Automatic ordinary continuation").unwrap();
    let published = ordinary_git_receiver::git(&remote, &["rev-parse", "work"]);
    let automatic = || {
        hub.command(root.path(), &["--json", "push", "alice/demo@work"])
            .env("AGIT_AUTO_PUSH", "1")
            .output()
            .unwrap()
    };
    repo.set_auto_push(Some(false)).unwrap();
    refused(automatic(), "automatic publication is disabled");
    assert_eq!(
        ordinary_git_receiver::git(&remote, &["rev-parse", "work"]),
        published
    );
    repo.set_auto_push(Some(true)).unwrap();
    let automatic = automatic();
    assert!(automatic.status.success(), "{automatic:?}");
    assert_eq!(
        ordinary_git_receiver::git(&remote, &["rev-parse", "work"]),
        repo.git(&["rev-parse", "work"]).unwrap()
    );
    assert!(
        !repo
            .common_dir()
            .unwrap()
            .join("agit/privacy-auto-consent.json")
            .exists()
    );
}

/// A checkout initialized for encryption refuses its first publication to an existing ordinary
/// repository, before any Git transport, until the command line selects ordinary publication.
/// Following the Hub's mode silently would send original history in plaintext to a repository
/// whose mode came from a default rather than the owner's choice.
#[test]
fn encrypted_creation_intent_refuses_first_ordinary_publication_without_explicit_selection() {
    let root = tempfile::tempdir().unwrap();
    let remote_root = root.path().join("remote");
    std::fs::create_dir_all(&remote_root).unwrap();
    let hub = Hub::start_git(Some(remote_root.clone()));
    hub.authenticate(root.path());
    let (repo, _, _) = ordinary_source(root.path());
    repo.set_creation_encryption(true).unwrap();
    let remote = remote_root.join("alice/demo.git");
    ordinary_git_receiver::initialize(&remote);

    refused(
        hub.run(root.path(), &["push", "alice/demo@work"]),
        "initialized for an encrypted repository",
    );
    assert!(
        hub.requests
            .lock()
            .unwrap()
            .iter()
            .all(|(request, _)| !request.contains("git-receive-pack")
                && !request.starts_with("POST "))
    );

    let explicit = hub.run(
        root.path(),
        &["push", "alice/demo@work", "--encryption=false"],
    );
    assert!(explicit.status.success(), "{explicit:?}");
    assert_eq!(
        ordinary_git_receiver::git(&remote, &["rev-parse", "work"]),
        repo.git(&["rev-parse", "work"]).unwrap()
    );
}

#[test]
fn separate_ordinary_destination_preserves_source_bindings_and_refuses_replacement_identity() {
    check_separate_ordinary_destination(false);
}

#[test]
fn explicit_separate_copy_retains_a_local_rc_repositorys_primary_destination() {
    check_separate_ordinary_destination(true);
}

#[test]
fn unpublished_local_history_retains_its_creation_intent_for_the_selected_hub_identity() {
    let root = tempfile::tempdir().unwrap();
    let remote_root = root.path().join("remote");
    std::fs::create_dir_all(&remote_root).unwrap();
    let hub = Hub::start_git(Some(remote_root.clone()));
    hub.authenticate(root.path());
    let (initial, _, _) = ordinary_source(root.path());
    let source = root.path().join("agit/repos/local/offline");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::rename(initial.root(), &source).unwrap();
    let repo = Repo::at(source);
    repo.set_creation_encryption(false).unwrap();
    let refs = repo.git(&["for-each-ref"]).unwrap();
    hub.missing_until_create.store(true, Ordering::Release);
    let output = hub.run(
        root.path(),
        &["--yes", "push", "local/offline@work", "--to", "alice/demo"],
    );
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        ordinary_git_receiver::git(&remote_root.join("alice/demo.git"), &["rev-parse", "work"]),
        repo.git(&["rev-parse", "work"]).unwrap()
    );
    assert_eq!(repo.git(&["for-each-ref"]).unwrap(), refs);
    assert!(identity::read(&repo).unwrap().is_none());
    assert!(repo.remote_url().is_none());
    assert_eq!(repo.creation_encryption().unwrap(), Some(false));
    let requests = hub.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .find(|(request, _)| request.starts_with("POST /api/agents "))
            .unwrap()
            .1["encryption_enabled"],
        false
    );
}

fn check_separate_ordinary_destination(local_rc: bool) {
    use agit::domain::privacy_receipt::{PublicationMode, PublicationReceipt};
    let root = tempfile::tempdir().unwrap();
    let remote_root = root.path().join("remote");
    let original_remote = remote_root.join("alice/source.git");
    ordinary_git_receiver::initialize(&original_remote);
    let hub = Hub::start_git(Some(remote_root.clone()));
    hub.authenticate(root.path());
    let (initial, session, _) = ordinary_source(root.path());
    let source_slug = if local_rc {
        "desktop-machine/source"
    } else {
        "alice/source"
    };
    let source_path = root.path().join(format!("agit/repos/{source_slug}"));
    std::fs::create_dir_all(source_path.parent().unwrap()).unwrap();
    std::fs::rename(initial.root(), &source_path).unwrap();
    let repo = Repo::at(&source_path);
    repo.git(&["push", original_remote.to_str().unwrap(), "--all"])
        .unwrap();
    repo.git(&["push", original_remote.to_str().unwrap(), "--tags"])
        .unwrap();
    let original_identity =
        RemoteIdentity::new(&hub.base, "00000000-0000-0000-0000-000000000001").unwrap();
    identity::pin(&repo, &original_identity).unwrap();
    if local_rc {
        std::fs::create_dir_all(root.path().join("agit/desktop-rc")).unwrap();
        std::fs::write(root.path().join("agit/desktop-rc/identity.json"), json!({
            "machine_fingerprint":"machine", "display_name":"Synthetic executor", "created_at":"2026-09-23T00:00:00Z"
        }).to_string()).unwrap();
        let local_id = "00000000-0000-0000-0000-000000000003";
        repo.git(&["config", "agit.desktopIdentity", local_id])
            .unwrap();
        repo.git(&["config", "agit.desktopAuthority", "local:machine"])
            .unwrap();
        repo.git(&[
            "config",
            "agit.desktopPublication",
            &json!({"version":1, "local_agent_id":local_id,
            "repository":"alice/source", "identity":original_identity})
            .to_string(),
        ])
        .unwrap();
    }
    let original_url = format!("{}/alice/source.git", hub.base);
    repo.set_remote(&original_url).unwrap();
    repo.set_auto_push(Some(true)).unwrap();
    repo.set_creation_encryption(true).unwrap();
    let source = repo.git(&["rev-parse", "work"]).unwrap();
    let primary = PublicationReceipt {
        version: 2,
        mode: PublicationMode::Ordinary,
        repository: "alice/source".into(),
        branch: "work".into(),
        source: source.clone(),
        published: source.clone(),
        projected_session_id: Some(session),
        destination: original_identity.clone(),
        url: original_url,
        policy_digest: None,
        recipient: None,
    };
    primary.save(&repo).unwrap();
    let consent_path = repo
        .common_dir()
        .unwrap()
        .join("agit/privacy-auto-consent.json");
    let consent = json!({"version":2,"mode":"ordinary","hub":hub.base,"account":"alice","account_id":"account-1",
        "agent_id":original_identity.agent_id,"url":primary.url,"visibility":"private"}).to_string();
    std::fs::write(&consent_path, &consent).unwrap();
    let original_config = std::fs::read(repo.root().join(".git/config")).unwrap();
    let original_refs = repo.git(&["for-each-ref"]).unwrap();
    let remote_refs = ordinary_git_receiver::git(&original_remote, &["for-each-ref"]);
    assert!(
        hub.run(root.path(), &["config", "privacy.encryption", "false"])
            .status
            .success()
    );
    let mut args = vec!["--yes", "push", source_slug, "--all", "--to", "alice/demo"];
    if local_rc {
        refused(hub.run(root.path(), &args), "cannot be replaced");
        args.push("--separate");
    }
    refused(
        hub.command(root.path(), &args)
            .env("AGIT_AUTO_PUSH", "1")
            .output()
            .unwrap(),
        "explicit unsupervised push",
    );
    refused(
        hub.command(root.path(), &args)
            .env(identity::EXPECTED_AGENT_ID_ENV, &original_identity.agent_id)
            .output()
            .unwrap(),
        "explicit unsupervised push",
    );
    hub.missing_until_create.store(true, Ordering::Release);
    let mut preview = args.to_vec();
    preview.push("--dry-run");
    let output = hub.run(root.path(), &preview);
    assert!(output.status.success(), "{output:?}");
    assert!(
        !repo
            .common_dir()
            .unwrap()
            .join("agit/publication-targets")
            .exists()
    );
    assert!(!remote_root.join("alice/demo.git").exists());
    let output = hub.run(root.path(), &args);
    assert!(output.status.success(), "{output:?}");
    let target = remote_root.join("alice/demo.git");
    assert_eq!(
        ordinary_git_receiver::git(&target, &["rev-parse", "work"]),
        source
    );
    assert_eq!(repo.git(&["for-each-ref"]).unwrap(), original_refs);
    assert_eq!(
        ordinary_git_receiver::git(&original_remote, &["for-each-ref"]),
        remote_refs
    );
    assert_eq!(
        std::fs::read(repo.root().join(".git/config")).unwrap(),
        original_config
    );
    assert_eq!(std::fs::read_to_string(&consent_path).unwrap(), consent);
    assert_eq!(
        PublicationReceipt::load(&repo, "work").unwrap(),
        Some(primary.clone())
    );
    assert_eq!(identity::read(&repo).unwrap(), Some(original_identity));
    let scoped: Vec<_> = walkdir::WalkDir::new(
        repo.common_dir()
            .unwrap()
            .join("agit/publication-destinations"),
    )
    .into_iter()
    .filter_map(Result::ok)
    .filter(|entry| entry.file_type().is_file())
    .map(|entry| {
        serde_json::from_slice::<PublicationReceipt>(&std::fs::read(entry.path()).unwrap()).unwrap()
    })
    .collect();
    assert_eq!(scoped.len(), 1);
    assert_eq!(scoped[0].destination.agent_id, ID);
    assert_eq!(scoped[0].mode, PublicationMode::Ordinary);
    assert_eq!(scoped[0].published, source);
    let repeated = hub.run(root.path(), &args);
    assert!(repeated.status.success(), "{repeated:?}");
    assert_eq!(
        PublicationReceipt::load(&repo, "work").unwrap(),
        Some(primary)
    );
    // A session branch copied into the now existing destination leaves that destination's file
    // line alone, even when the source's own `main` has advanced.
    let destination_main = ordinary_git_receiver::git(&target, &["rev-parse", "main"]);
    let source_main = repo.git(&["rev-parse", "main"]).unwrap();
    let tree = repo.git(&["rev-parse", "main^{tree}"]).unwrap();
    let advanced = repo
        .git(&[
            "commit-tree",
            &tree,
            "-p",
            "main",
            "-m",
            "Advance the source file line",
        ])
        .unwrap();
    repo.git(&["update-ref", "refs/heads/main", &advanced])
        .unwrap();
    let mut explicit = vec!["--yes", "push"];
    let selected = format!("{source_slug}@work");
    explicit.extend([selected.as_str(), "--to", "alice/demo"]);
    if local_rc {
        explicit.push("--separate");
    }
    let copied = hub.run(root.path(), &explicit);
    assert!(copied.status.success(), "{copied:?}");
    assert_eq!(
        ordinary_git_receiver::git(&target, &["rev-parse", "main"]),
        destination_main
    );
    repo.git(&["update-ref", "refs/heads/main", &source_main])
        .unwrap();
    let requests = hub.requests.lock().unwrap();
    let mutation_count = requests
        .iter()
        .filter(|(request, _)| request.starts_with("POST ") || request.starts_with("PUT "))
        .count();
    let creation = requests
        .iter()
        .find(|(request, _)| request.starts_with("POST /api/agents "))
        .unwrap();
    assert_eq!(creation.1["encryption_enabled"], false);
    drop(requests);
    hub.response.lock().unwrap().1["agent_id"] = "00000000-0000-0000-0000-000000000002".into();
    refused(
        hub.run(root.path(), &args),
        "changed identity or fixed mode",
    );
    hub.missing_until_create.store(true, Ordering::Release);
    refused(
        hub.run(root.path(), &args),
        "refusing to create a replacement",
    );
    assert_eq!(
        hub.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(request, _)| request.starts_with("POST ") || request.starts_with("PUT "))
            .count(),
        mutation_count
    );
    assert_eq!(repo.git(&["for-each-ref"]).unwrap(), original_refs);
    assert_eq!(
        std::fs::read(repo.root().join(".git/config")).unwrap(),
        original_config
    );
}

#[test]
fn empty_hub_repository_mode_is_fixed_and_never_inherits_local_defaults() {
    let root = tempfile::tempdir().unwrap();
    let hub = Hub::start();
    hub.authenticate(root.path());
    let repo = Repo::init(&root.path().join("agit/repos/alice/demo")).unwrap();
    let local = hub.setting(root.path());
    assert_eq!(local["effective"], false);
    assert_eq!(local["scope"], "creation_intent");
    repo.set_creation_encryption(true).unwrap();
    identity::pin(&repo, &RemoteIdentity::new(&hub.base, ID).unwrap()).unwrap();
    repo.set_remote(&format!("{}/alice/demo.git", hub.base))
        .unwrap();
    let before = std::fs::read(repo.root().join(".git/config")).unwrap();
    for preference in ["false", "true"] {
        assert!(
            hub.run(root.path(), &["config", "privacy.encryption", preference])
                .status
                .success()
        );
        let actual = hub.setting(root.path());
        assert_eq!(actual["effective"], false);
        assert_eq!(actual["stored"], false);
        assert_eq!(actual["source"], "hub");
        assert_eq!(actual["fixed"], true);
        assert_eq!(actual["agent_id"], ID);
    }
    for args in [
        vec![
            "config",
            "--repo",
            "alice/demo",
            "privacy.encryption",
            "true",
        ],
        vec![
            "config",
            "--repo",
            "alice/demo",
            "--unset",
            "privacy.encryption",
        ],
        vec!["init", "demo", "--no-bind", "--encryption=true"],
        vec!["privacy", "init", "alice/demo"],
        vec!["repo", "create", "demo", "--encryption=true"],
    ] {
        refused(hub.run(root.path(), &args), "fixed at creation");
    }
    assert!(repo.local_branches().is_empty());
    assert_eq!(
        std::fs::read(repo.root().join(".git/config")).unwrap(),
        before
    );
    assert!(
        hub.requests
            .lock()
            .unwrap()
            .iter()
            .all(
                |(request, _)| request.starts_with("GET /api/agents/alice/demo ")
                    || request.starts_with("GET /api/cli/version ")
            ),
        "{:?}",
        hub.requests.lock().unwrap()
    );

    hub.response.lock().unwrap().1["encryption_enabled"] = true.into();
    assert_eq!(hub.setting(root.path())["effective"], true);
    refused(
        hub.run(root.path(), &["privacy", "init", "alice/demo"]),
        "requires an interactive terminal",
    );
}

#[test]
fn local_rc_configuration_reads_the_confirmed_hub_destination_mode() {
    let root = tempfile::tempdir().unwrap();
    let hub = Hub::start();
    hub.authenticate(root.path());
    let repo = Repo::init(&root.path().join("agit/repos/desktop-machine/local")).unwrap();
    let local_id = "00000000-0000-0000-0000-000000000003";
    std::fs::create_dir_all(root.path().join("agit/desktop-rc")).unwrap();
    std::fs::write(root.path().join("agit/desktop-rc/identity.json"), json!({
        "machine_fingerprint":"machine", "display_name":"Synthetic executor", "created_at":"2026-09-23T00:00:00Z"
    }).to_string()).unwrap();
    repo.git(&["config", "agit.desktopIdentity", local_id])
        .unwrap();
    repo.git(&["config", "agit.desktopAuthority", "local:machine"])
        .unwrap();
    let destination = RemoteIdentity::new(&hub.base, ID).unwrap();
    identity::pin(&repo, &destination).unwrap();
    repo.git(&[
        "config",
        "agit.desktopPublication",
        &json!({"version":1,"local_agent_id":local_id,
        "repository":"alice/demo","identity":destination})
        .to_string(),
    ])
    .unwrap();
    repo.set_creation_encryption(true).unwrap();
    let before = std::fs::read(repo.root().join(".git/config")).unwrap();
    let output = hub.run(
        root.path(),
        &[
            "--json",
            "config",
            "--repo",
            "desktop-machine/local",
            "privacy.encryption",
        ],
    );
    assert!(output.status.success(), "{output:?}");
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    let entry = &response["result"]["value"]["setting"];
    assert_eq!(entry["effective"], false);
    assert_eq!(entry["source"], "hub");
    assert_eq!(entry["agent_id"], ID);
    assert_eq!(entry["publication_repository"], "alice/demo");
    assert_eq!(
        std::fs::read(repo.root().join(".git/config")).unwrap(),
        before
    );
    assert!(
        hub.requests
            .lock()
            .unwrap()
            .iter()
            .all(|(request, _)| !request.contains("/api/agents/desktop-machine/"))
    );
}

#[test]
fn unavailable_or_unsupported_mode_never_becomes_disabled_or_creates_a_replacement() {
    let root = tempfile::tempdir().unwrap();
    let hub = Hub::start();
    hub.authenticate(root.path());
    let repo = Repo::init(&root.path().join("agit/repos/alice/demo")).unwrap();
    identity::pin(&repo, &RemoteIdentity::new(&hub.base, ID).unwrap()).unwrap();
    let original = hub.response.lock().unwrap().1.clone();
    for status in [403, 404, 503] {
        *hub.response.lock().unwrap() = (status, json!({"error":"synthetic refusal"}));
        assert!(
            !hub.run(
                root.path(),
                &["config", "--repo", "alice/demo", "privacy.encryption"]
            )
            .status
            .success()
        );
        assert!(
            !hub.run(root.path(), &["privacy", "init", "alice/demo"])
                .status
                .success()
        );
    }
    for mode in [None, Some(Value::Null), Some(json!("false"))] {
        let mut response = original.clone();
        response
            .as_object_mut()
            .unwrap()
            .remove("encryption_enabled");
        if let Some(value) = mode {
            response["encryption_enabled"] = value;
        }
        *hub.response.lock().unwrap() = (200, response);
        assert!(
            !hub.run(
                root.path(),
                &["config", "--repo", "alice/demo", "privacy.encryption"]
            )
            .status
            .success()
        );
    }
    let mut changed = original;
    changed["agent_id"] = "00000000-0000-0000-0000-000000000001".into();
    *hub.response.lock().unwrap() = (200, changed);
    refused(
        hub.run(
            root.path(),
            &["config", "--repo", "alice/demo", "privacy.encryption"],
        ),
        "refusing a reused remote name",
    );
    assert!(
        hub.requests
            .lock()
            .unwrap()
            .iter()
            .all(|(request, _)| request.starts_with("GET "))
    );
    assert!(repo.local_branches().is_empty());
}

#[test]
fn creation_sends_selected_mode_and_rechecks_it_before_installing_local_identity() {
    let root = tempfile::tempdir().unwrap();
    let hub = Hub::start();
    hub.authenticate(root.path());
    let response = |name: &str, enabled: bool| {
        json!({
            "agent_id":ID, "owner":"alice", "name":name, "encryption_enabled":enabled,
            "visibility":"private", "clone_url":format!("{}/alice/{name}.git", hub.base),
            "push_url":format!("{}/alice/{name}.git", hub.base), "web_url":format!("{}/@alice/{name}", hub.base)
        })
    };
    for (name, enabled, flags) in [
        ("ordinary", false, vec![]),
        ("encrypted", true, vec!["--encryption=true"]),
        ("preferred", true, vec![]),
        ("override", false, vec!["--encryption=false"]),
    ] {
        *hub.response.lock().unwrap() = (200, response(name, enabled));
        hub.missing_until_create.store(true, Ordering::Release);
        let mut args = vec!["repo", "create", name, "--private"];
        args.extend(flags);
        let output = hub.run(root.path(), &args);
        assert!(output.status.success(), "{output:?}");
        let requests = hub.requests.lock().unwrap();
        let (_, body) = requests
            .iter()
            .rev()
            .find(|(request, _)| request.starts_with("POST "))
            .unwrap();
        assert_eq!(body["encryption_enabled"], enabled);
        assert_eq!(body["public"], false);
        drop(requests);
        let repo = Repo::open(root.path().join(format!("agit/repos/alice/{name}"))).unwrap();
        assert_eq!(identity::read(&repo).unwrap().unwrap().agent_id, ID);
        assert_eq!(repo.auto_push_override().unwrap(), None);
        if name == "encrypted" {
            assert!(
                hub.run(root.path(), &["config", "privacy.encryption", "true"])
                    .status
                    .success()
            );
        }
    }
    *hub.response.lock().unwrap() = (200, response("conflict", false));
    let requests_before_retry = hub.requests.lock().unwrap().len();
    refused(
        hub.run(root.path(), &["repo", "create", "conflict"]),
        "already exists with encryption disabled",
    );
    assert_eq!(
        hub.requests.lock().unwrap().len(),
        requests_before_retry + 1
    );
    refused(
        hub.run(
            root.path(),
            &["repo", "create", "conflict", "--encryption=true"],
        ),
        "fixed at creation",
    );
    assert!(!root.path().join("agit/repos/alice/conflict").exists());
    hub.missing_until_create.store(true, Ordering::Release);
    refused(
        hub.run(
            root.path(),
            &["repo", "create", "conflict", "--encryption=true"],
        ),
        "fixed at creation",
    );
    let mut missing = response("unsupported", false);
    missing
        .as_object_mut()
        .unwrap()
        .remove("encryption_enabled");
    *hub.response.lock().unwrap() = (200, missing);
    hub.missing_until_create.store(true, Ordering::Release);
    refused(
        hub.run(
            root.path(),
            &["repo", "create", "unsupported", "--encryption=false"],
        ),
        "did not return encryption_enabled",
    );
    assert!(!root.path().join("agit/repos/alice/unsupported").exists());
}

/// Password setup for a missing repository creates it encrypted although no flag or preference
/// selects encryption; inheriting the ordinary creation default would refuse before creation.
#[test]
fn browser_password_setup_creates_privately_and_checks_current_key_without_mutation() {
    let hub = Hub::start();
    let root = tempfile::tempdir().unwrap();
    hub.authenticate(root.path());
    hub.response.lock().unwrap().1["encryption_enabled"] = json!(true);
    hub.missing_until_create.store(true, Ordering::Release);
    let args = ["privacy", "init", "alice/demo", "--browser", "--json"];
    let refused = hub.run(root.path(), &args);
    assert_eq!(refused.status.code(), Some(8));
    assert!(
        hub.requests
            .lock()
            .unwrap()
            .iter()
            .all(|(r, _)| r.starts_with("GET "))
    );
    let confirmed = [
        "privacy",
        "init",
        "alice/demo",
        "--browser",
        "--json",
        "--yes",
    ];
    for _ in 0..2 {
        let output = hub.run(root.path(), &confirmed);
        assert_eq!(
            output.status.code(),
            Some(8),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(envelope["schema"], "cli-output");
        assert_eq!(envelope["exit_code"], 8);
        let result = &envelope["result"]["value"];
        assert_eq!(result["status"], "setup_required");
        assert_eq!(result["agent_id"], ID);
        assert_eq!(result["repository"], "alice/demo");
        assert_eq!(result["hub"], hub.base);
        assert_eq!(
            result["setup_url"],
            format!(
                "{}/@alice/demo/settings?setup=initialize&expected_agent_id={ID}#repository-password",
                hub.base
            )
        );
    }
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/privacy-web-key.json")).unwrap();
    let key = json!({
        "recipient": fixture["record"]["recipient"],
        "public_key_algorithm": fixture["record"]["public_key_algorithm"],
        "public_key": fixture["record"]["public_key"],
    });
    let configured = json!({"agent_id": ID, "config_version": 1, "current": key});
    *hub.publishing_key.lock().unwrap() = (200, configured.clone());
    for _ in 0..2 {
        let output = hub.run(root.path(), &args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(envelope["result"]["value"]["status"], "ready");
        assert_eq!(envelope["result"]["value"]["agent_id"], ID);
        assert!(envelope["result"]["value"].get("setup_url").is_none());
    }
    assert_eq!(hub.publishing_key.lock().unwrap().1, configured);
    let requests = hub.requests.lock().unwrap();
    let writes: Vec<_> = requests
        .iter()
        .filter(|(r, _)| !r.starts_with("GET "))
        .collect();
    assert_eq!(writes.len(), 1);
    assert!(writes[0].0.starts_with("POST /api/agents "));
    assert_eq!(writes[0].1["public"], false);
    assert_eq!(writes[0].1["encryption_enabled"], true);
    drop(requests);
    for (status, body) in [
        (403, json!({"error": "forbidden"})),
        (404, json!({"error": "missing endpoint"})),
        (503, json!({"error": "unavailable"})),
        (
            200,
            json!({"agent_id": "a31b3a75-8245-4ae5-83a7-52a612012c90", "config_version": 1, "current": null}),
        ),
        (
            200,
            json!({"agent_id": ID, "config_version": 1, "current": {"recipient":"bad", "public_key_algorithm":"x25519", "public_key":"invalid"}}),
        ),
    ] {
        *hub.publishing_key.lock().unwrap() = (status, body);
        let output = hub.run(root.path(), &args);
        assert!(!output.status.success());
        assert_ne!(output.status.code(), Some(8));
        let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_ne!(envelope["result"]["value"]["status"], "setup_required");
    }
    hub.response.lock().unwrap().1["agent_id"] = json!("a31b3a75-8245-4ae5-83a7-52a612012c90");
    hub.requests.lock().unwrap().clear();
    assert!(!hub.run(root.path(), &confirmed).status.success());
    assert_eq!(hub.requests.lock().unwrap().len(), 1);
}

#[test]
fn browser_password_setup_preserves_mode_and_lookup_refusals() {
    let hub = Hub::start();
    let root = tempfile::tempdir().unwrap();
    hub.authenticate(root.path());
    let args = [
        "privacy",
        "init",
        "alice/demo",
        "--browser",
        "--json",
        "--yes",
    ];
    let original = hub.response.lock().unwrap().1.clone();
    for (status, body) in [
        (200, original.clone()),
        (403, json!({"error":"forbidden"})),
        (503, json!({"error":"unavailable"})),
    ] {
        *hub.response.lock().unwrap() = (status, body);
        hub.requests.lock().unwrap().clear();
        let output = hub.run(root.path(), &args);
        assert!(!output.status.success());
        let requests = hub.requests.lock().unwrap();
        assert!(!requests.is_empty());
        assert!(
            requests
                .iter()
                .all(|(request, _)| request == "GET /api/agents/alice/demo HTTP/1.1"),
            "{requests:?}"
        );
    }
    *hub.response.lock().unwrap() = (200, original);
    hub.missing_until_create.store(true, Ordering::Release);
    hub.requests.lock().unwrap().clear();
    let mut disabled = args.to_vec();
    disabled.push("--encryption=false");
    assert!(!hub.run(root.path(), &disabled).status.success());
    assert!(
        hub.requests
            .lock()
            .unwrap()
            .iter()
            .all(|(r, _)| r.starts_with("GET "))
    );
}

#[test]
fn browser_password_setup_pins_an_existing_remote_before_returning_a_link() {
    let hub = Hub::start();
    let root = tempfile::tempdir().unwrap();
    hub.authenticate(root.path());
    hub.response.lock().unwrap().1["encryption_enabled"] = json!(true);
    let args = [
        "privacy",
        "init",
        "alice/demo",
        "--browser",
        "--json",
        "--yes",
    ];
    refused(hub.run(root.path(), &args), "requires a local repository");
    let repo = Repo::init(&root.path().join("agit/repos/alice/demo")).unwrap();
    *hub.publishing_key.lock().unwrap() = (403, json!({"error":"forbidden"}));
    assert!(!hub.run(root.path(), &args).status.success());
    assert!(identity::read(&repo).unwrap().is_none());
    *hub.publishing_key.lock().unwrap() = (
        200,
        json!({"agent_id": ID, "config_version": 0, "current": null}),
    );
    assert!(identity::read(&repo).unwrap().is_none());
    let first = hub.run(root.path(), &args);
    assert_eq!(first.status.code(), Some(8));
    assert_eq!(
        identity::read(&repo).unwrap(),
        Some(RemoteIdentity::new(&hub.base, ID).unwrap())
    );
    let config = std::fs::read(repo.root().join(".git/config")).unwrap();
    assert_eq!(hub.run(root.path(), &args).status.code(), Some(8));
    assert_eq!(
        std::fs::read(repo.root().join(".git/config")).unwrap(),
        config
    );
    hub.response.lock().unwrap().1["agent_id"] = json!("a31b3a75-8245-4ae5-83a7-52a612012c90");
    hub.requests.lock().unwrap().clear();
    let retry = hub.run(root.path(), &args);
    assert!(!retry.status.success());
    assert_ne!(retry.status.code(), Some(8));
    assert_eq!(hub.requests.lock().unwrap().len(), 1);
    assert_eq!(
        identity::read(&repo).unwrap(),
        Some(RemoteIdentity::new(&hub.base, ID).unwrap())
    );
    hub.missing_until_create.store(true, Ordering::Release);
    hub.requests.lock().unwrap().clear();
    assert!(!hub.run(root.path(), &args).status.success());
    assert!(
        hub.requests
            .lock()
            .unwrap()
            .iter()
            .all(|(request, _)| request.starts_with("GET "))
    );
}
