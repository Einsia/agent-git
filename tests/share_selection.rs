//! A share uploads only its selected saved sequence or explicitly selected live transcript.

#![cfg(unix)]

use agit::domain::privacy_envelope::PrivacyEnvelope;
use agit::domain::{link, meta, repo::Repo, storage, store::Store, transcript};
use crypto_box::SecretKey;
use serde_json::{Value, json};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

#[path = "support/publication_identity.rs"]
mod publication_identity;
#[path = "support/publication_text.rs"]
mod publication_text;

struct Hub {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
    rotate_key: Arc<AtomicBool>,
    legacy: Arc<AtomicBool>,
}

impl Hub {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let rotate_key = Arc::new(AtomicBool::new(false));
        let rotation = rotate_key.clone();
        let legacy = Arc::new(AtomicBool::new(false));
        let old_protocol = legacy.clone();
        let policy_hub = url.clone();
        let worker = std::thread::spawn(move || {
            let key_reads = AtomicUsize::new(0);
            while !stopping.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(error) => panic!("fake Hub accept: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_millis(100)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let deadline = Instant::now() + Duration::from_secs(3);
                let mut bytes = Vec::new();
                let request = loop {
                    if Instant::now() >= deadline || bytes.len() > 1024 * 1024 {
                        break None;
                    }
                    if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        let header = String::from_utf8_lossy(&bytes[..end]);
                        let length: usize = header
                            .lines()
                            .find_map(|line| {
                                let (key, value) = line.split_once(':')?;
                                key.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse().ok())
                                    .flatten()
                            })
                            .unwrap_or(0);
                        if bytes.len() >= end + 4 + length {
                            let mut request_line =
                                header.lines().next().unwrap().split_whitespace();
                            let method = request_line.next().unwrap();
                            let target = request_line.next().unwrap();
                            if let Some((status, body)) = publication_identity::route(
                                &policy_hub,
                                "me",
                                method,
                                target,
                                &bytes[end + 4..end + 4 + length],
                            ) {
                                let body = body.to_string();
                                write!(stream, "HTTP/1.1 {status} Synthetic\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                                break None;
                            }
                            if target.starts_with("/api/agents/me/paper/privacy/keys/") {
                                let (recipient, commit) = target
                                    .strip_prefix("/api/agents/me/paper/privacy/keys/")
                                    .unwrap()
                                    .split_once("?ref=")
                                    .unwrap();
                                let fixture: Value = serde_json::from_str(include_str!(
                                    "fixtures/privacy-web-key.json"
                                ))
                                .unwrap();
                                let mut record = fixture["record"].clone();
                                use base64::Engine;
                                let public = base64::engine::general_purpose::STANDARD
                                    .encode(SecretKey::from([31; 32]).public_key().as_bytes());
                                record["public_key"] = json!(public);
                                record["recipient"] =
                                    json!(agit::domain::privacy_key::recipient_id(&public));
                                assert_eq!(recipient, record["recipient"].as_str().unwrap());
                                if key_reads.fetch_add(1, Ordering::SeqCst) > 0
                                    && rotation.load(Ordering::SeqCst)
                                {
                                    record["recipient"] = json!("changed-recipient");
                                }
                                let body = json!({"agent_id":"00000000-0000-0000-0000-000000000001","commit":commit,"session_id":format!("agit-{}","b".repeat(40)),"key":record}).to_string();
                                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                                break None;
                            }
                            assert!(header.starts_with("POST /api/shares/privacy "));
                            if old_protocol.load(Ordering::SeqCst) {
                                write!(stream, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                                break None;
                            }
                            break Some(
                                serde_json::from_slice(&bytes[end + 4..end + 4 + length]).unwrap(),
                            );
                        }
                    }
                    let mut block = [0; 4096];
                    match stream.read(&mut block) {
                        Ok(0) => break None,
                        Ok(count) => bytes.extend_from_slice(&block[..count]),
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) => {}
                        Err(error) => panic!("fake Hub read: {error}"),
                    }
                };
                if let Some(request) = request {
                    captured.lock().unwrap().push(request);
                    let body = r#"{"format_version":2,"slug":"fixture","url":"http://127.0.0.1/share/fixture"}"#;
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                }
            }
        });
        Self {
            url,
            requests,
            stop,
            worker: Some(worker),
            rotate_key,
            legacy,
        }
    }

    fn payloads(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.worker.take().unwrap().join().unwrap();
    }
}

struct Fixture {
    temporary: tempfile::TempDir,
    hub: Hub,
    repo: Repo,
    sha: String,
}

fn envelope(text: &str) -> String {
    let raw = format!(
        "{}\n",
        json!({"type":"user","sessionId":"native-fixture","message":{"role":"user","content":text}})
    );
    transcript::wrap_lines(&raw, "claude-code", &format!("agit-{}", "b".repeat(40)))
}

#[path = "support/privacy_input_budget.rs"]
mod privacy_input_budget;
#[path = "support/startup_cache.rs"]
mod startup_cache;

#[test]
fn shared_text_fixture_preserves_public_text_and_recovers_selected_originals() {
    use aes_gcm::{Aes256Gcm, KeyInit, Nonce, aead::Aead};
    use agit::domain::privacy::PrivacyPolicy;
    use base64::Engine;

    let f = Fixture::new("unselected");
    std::fs::create_dir_all(f.home().join("src")).unwrap();
    let raw = publication_text::records(f.home())
        .into_iter()
        .map(|record| format!("{record}\n"))
        .collect::<String>();
    let log = transcript::wrap_lines(&raw, "claude-code", &format!("agit-{}", "b".repeat(40)));
    f.repo.git(&["checkout", "chosen"]).unwrap();
    storage::write_snapshot(f.repo.root(), &log, &log).unwrap();
    f.repo.add_all().unwrap();
    f.repo
        .commit("Session with textual and attachment records")
        .unwrap();
    PrivacyPolicy {
        workspace: Some(f.home().into()),
        replacements: vec![publication_text::replacement()],
        ..Default::default()
    }
    .save(&f.repo)
    .unwrap();

    f.success(&["me/paper@chosen", "--public"], None);
    let request = f.hub.payloads().pop().unwrap();
    let public: Value = serde_json::from_str(request["payload"].as_str().unwrap()).unwrap();
    publication_text::assert_public(&public, f.home());

    f.accept_current();
    let output = f.success(&["me/paper@chosen"], None);
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .matches("attachment body is excluded from public content")
            .count()
            >= 4
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let key = stdout
        .split("#k=")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap();
    let request = f.hub.payloads().pop().unwrap();
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let blob = engine.decode(request["payload"].as_str().unwrap()).unwrap();
    let cipher = Aes256Gcm::new_from_slice(&engine.decode(key).unwrap()).unwrap();
    let plaintext = cipher
        .decrypt(Nonce::from_slice(&blob[..12]), &blob[12..])
        .unwrap();
    let envelope = PrivacyEnvelope::parse(&plaintext).unwrap();
    assert_eq!(envelope.public_projection, public);
    let recovered = envelope.open_layer(&SecretKey::from([31; 32])).unwrap();
    let (original_log, original_view) = recovered.session_bytes().unwrap();
    assert_eq!(original_log.as_str(), log);
    assert_eq!(original_view.as_str(), log);
}

#[test]
fn oversized_saved_and_live_sources_fail_before_upload() {
    let f = Fixture::new("unselected");
    f.repo.git(&["checkout", "-b", "oversized"]).unwrap();
    privacy_input_budget::commit_oversized_event(&f.repo);
    let result = f.run(&["me/paper@oversized", "--public"], None);
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("limit"),
        "{result:?}"
    );
    assert!(f.hub.payloads().is_empty());
    let native = f.native("oversized-native");
    std::fs::OpenOptions::new()
        .write(true)
        .open(native)
        .unwrap()
        .set_len(agit::domain::privacy_publication::MAX_INPUT_BYTES as u64 + 1)
        .unwrap();
    let result = f.run(&["oversized-native", "--public"], None);
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("budget"),
        "{result:?}"
    );
    assert!(f.hub.payloads().is_empty());
}

impl Fixture {
    fn new(excluded: &str) -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path();
        startup_cache::seed(home);
        let hub = Hub::start();
        let credential = agit::infra::credentials::HubCredential {
            account_id: None,
            username: "me".into(),
            email: None,
            hub: Some(hub.url.clone()),
            access_token: "fixture-access".into(),
            refresh_token: "fixture-refresh".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        };
        let key = agit::infra::config::hub_host_key(&hub.url).unwrap();
        agit::infra::credentials::save_at(
            &home.join("credentials").join(format!("{key}.json")),
            &credential,
        )
        .unwrap();
        let repo = Repo::init(&home.join("repos/me/paper")).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
        repo.add_all().unwrap();
        repo.commit("file line").unwrap();
        repo.git(&["checkout", "-b", "chosen"]).unwrap();
        let visible = envelope("VISIBLE-SAVED");
        storage::write_snapshot(
            repo.root(),
            &(visible.clone() + &envelope(excluded)),
            &visible,
        )
        .unwrap();
        let mut snapshot = meta::Meta::new(
            format!("agit-{}", "b".repeat(40)),
            "claude-code".into(),
            home.to_string_lossy().into(),
        );
        snapshot.turn = Some(1);
        meta::write(repo.root(), &snapshot).unwrap();
        repo.add_all().unwrap();
        repo.commit("selected session").unwrap();
        let sha = repo.git(&["rev-parse", "HEAD"]).unwrap();
        repo.git(&["tag", "saved"]).unwrap();
        repo.git(&["checkout", "main"]).unwrap();
        Self {
            temporary,
            hub,
            repo,
            sha,
        }
    }

    fn accept_current(&self) {
        use base64::Engine;
        let branch = self.repo.current_branch().unwrap();
        let log = storage::materialize_at(self.repo.root(), "HEAD", meta::LOG_FILE).unwrap();
        let view = storage::materialize_at(self.repo.root(), "HEAD", meta::VIEW_FILE).unwrap();
        let metadata = meta::read(self.repo.root()).unwrap();
        let key = SecretKey::from([31; 32]);
        let public = base64::engine::general_purpose::STANDARD.encode(key.public_key().as_bytes());
        let layer = agit::domain::privacy_layer::PrivateLayer::new(
            &log,
            &view,
            serde_json::to_value(&metadata).unwrap(),
            Default::default(),
        )
        .unwrap();
        let envelope = PrivacyEnvelope::seal_layer(
            agit::domain::privacy_envelope::digest_bytes(b"policy"),
            agit::domain::privacy_envelope::digest_bytes(b"snapshot"),
            json!({"metadata":{"session":metadata.session}}),
            &layer,
            &agit::domain::privacy_envelope::ViewingRecipient::from_base64(
                agit::domain::privacy_key::recipient_id(&public),
                &public,
            )
            .unwrap(),
            vec![],
        )
        .unwrap();
        std::fs::create_dir_all(self.repo.root().join("privacy")).unwrap();
        std::fs::write(
            self.repo.root().join("privacy/envelope.json"),
            serde_json::to_vec(&envelope).unwrap(),
        )
        .unwrap();
        self.repo.add_all().unwrap();
        self.repo.commit("Accepted publication fixture").unwrap();
        self.repo
            .set_remote(&format!("{}/me/paper.git", self.hub.url))
            .unwrap();
        agit::hub::identity::pin(
            &self.repo,
            &agit::hub::identity::RemoteIdentity::new(
                &self.hub.url,
                "00000000-0000-0000-0000-000000000001",
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(self.repo.current_branch().unwrap(), branch);
    }

    fn home(&self) -> &Path {
        self.temporary.path()
    }

    fn run(&self, args: &[&str], session: Option<&str>) -> Output {
        self.run_command("share", args, session)
    }

    fn run_command(&self, subcommand: &str, args: &[&str], session: Option<&str>) -> Output {
        let mut stdout = tempfile::tempfile().unwrap();
        let mut stderr = tempfile::tempfile().unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command
            .args(["-y", subcommand])
            .args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.home())
            .env("AGIT_HOME", self.home())
            .env("AGIT_HUB_URL", &self.hub.url)
            .env("AGIT_SECRETS_KEYSTORE", "file")
            .env("CI", "1")
            .env("NO_COLOR", "1")
            .current_dir(self.home())
            .stdin(Stdio::null())
            .stdout(stdout.try_clone().unwrap())
            .stderr(stderr.try_clone().unwrap());
        if let Some(session) = session {
            command.env("AGIT_SESSION", session);
        }
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("{subcommand} subprocess exceeded its deadline");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let read = |file: &mut std::fs::File| {
            file.rewind().unwrap();
            let mut bytes = Vec::new();
            file.take(1024 * 1024).read_to_end(&mut bytes).unwrap();
            bytes
        };
        Output {
            status,
            stdout: read(&mut stdout),
            stderr: read(&mut stderr),
        }
    }

    fn success(&self, args: &[&str], session: Option<&str>) -> Output {
        let result = self.run(args, session);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        result
    }

    fn refuse_without_upload(&self, args: &[&str], session: Option<&str>) {
        let before = self.hub.payloads().len();
        let result = self.run(args, session);
        assert!(
            !result.status.success(),
            "unexpected share for {args:?}: {}",
            String::from_utf8_lossy(&result.stdout)
        );
        assert_eq!(self.hub.payloads().len(), before);
    }

    fn native(&self, id: &str) -> PathBuf {
        let project = self.home().join(".claude/projects/fixture");
        std::fs::create_dir_all(&project).unwrap();
        let path = project.join(format!("{id}.jsonl"));
        std::fs::write(&path, format!("{}\n", json!({"type":"user","sessionId":id,"message":{"role":"user","content":"LIVE-UNSETTLED"}}))).unwrap();
        let store = Store::at(self.home().join("store"));
        link::write(
            &store,
            &link::Link::new("claude-code", id, Some(self.home())),
        )
        .unwrap();
        path
    }
}

#[test]
fn refs_and_injected_branch_share_view_while_full_log_is_explicit() {
    let f = Fixture::new("EXCLUDED-LOG");
    for target in [
        "me/paper@chosen",
        "paper@chosen",
        "me/paper@saved",
        "me/paper@chosen#1",
        "me/paper@chosen#-1",
        "me/paper@chosen~0",
        &format!("me/paper@{}", f.sha),
    ] {
        f.success(&[target, "--public"], None);
        let request = f.hub.payloads().pop().unwrap();
        let text = request["payload"].as_str().unwrap();
        assert!(text.contains("VISIBLE-SAVED"));
        assert!(!text.contains("EXCLUDED-LOG"));
    }
    f.success(&["--public"], Some("me/paper@chosen"));
    f.success(&["@", "--public"], Some("me/paper@chosen"));
    f.success(&["@#1", "--public"], Some("me/paper@chosen"));
    f.success(&["me/paper@chosen", "--public", "--full-log"], None);
    assert!(
        f.hub.payloads().last().unwrap()["payload"]
            .as_str()
            .unwrap()
            .contains("EXCLUDED-LOG")
    );
    f.refuse_without_upload(&["--public"], None);
    f.refuse_without_upload(&["--public"], Some("me/paper@missing"));
    assert_eq!(f.repo.current_branch().as_deref(), Some("main"));
    assert!(f.repo.git(&["status", "--porcelain"]).unwrap().is_empty());
}

#[test]
fn selected_secret_scope_and_invalid_selectors_never_widen_the_upload() {
    let f = Fixture::new("ghp_7Kd2mQ9xR4vB1nT8sW3zY6cL5jH0gF2aE4pU");
    f.success(&["me/paper@chosen", "--public"], None);
    f.success(&["me/paper@chosen", "--public", "--full-log"], None);
    assert!(
        !f.hub.payloads().last().unwrap()["payload"]
            .as_str()
            .unwrap()
            .contains("ghp_7Kd2mQ9xR4vB1nT8sW3zY6cL5jH0gF2aE4pU")
    );
    for target in [
        "me/paper@chosen#1.1",
        "me/paper@chosen#1..#1",
        "me/paper@chosen:VIEW",
        "me/paper@main",
        "me/paper@missing",
    ] {
        f.refuse_without_upload(&[target, "--public"], None);
    }
    f.repo.git(&["checkout", "chosen"]).unwrap();
    std::fs::write(f.repo.root().join(meta::VIEW_FILE), "invalid-view\n").unwrap();
    f.repo.add_all().unwrap();
    f.repo.commit("invalid selected view").unwrap();
    f.repo.git(&["checkout", "main"]).unwrap();
    f.refuse_without_upload(&["me/paper@chosen", "--public"], None);
    f.repo.git(&["checkout", "chosen"]).unwrap();
    std::fs::remove_file(f.repo.root().join(meta::VIEW_FILE)).unwrap();
    f.repo.add_all().unwrap();
    f.repo.commit("missing selected view").unwrap();
    f.repo.git(&["checkout", "main"]).unwrap();
    f.refuse_without_upload(&["me/paper@chosen", "--public"], None);
}

#[test]
fn native_ids_remain_live_and_local_ref_ambiguities_are_rejected() {
    let f = Fixture::new("EXCLUDED-LOG");
    let path = f.native("native-fixture");
    let before = std::fs::read(&path).unwrap();
    let output = f.success(&["native-fixture", "--public"], None);
    assert!(String::from_utf8_lossy(&output.stdout).contains("live runtime transcript"));
    assert!(
        f.hub.payloads().last().unwrap()["payload"]
            .as_str()
            .unwrap()
            .contains("LIVE-UNSETTLED")
    );
    f.success(&["native-fixture", "--public"], Some("me/missing@branch"));
    f.refuse_without_upload(&["native-fixture", "--public", "--full-log"], None);
    f.refuse_without_upload(&["", "--public"], None);
    f.refuse_without_upload(&["  ", "--public"], None);
    f.repo.git(&["branch", "native-fixture", "chosen"]).unwrap();
    f.refuse_without_upload(&["native-fixture", "--public"], Some("me/paper@chosen"));
    let other = Repo::init(&f.home().join("repos/other/paper")).unwrap();
    assert!(other.root().is_dir());
    f.refuse_without_upload(&["paper@chosen", "--public"], None);
    assert_eq!(std::fs::read(path).unwrap(), before);

    let root = f.home().join(".cursor/projects/fixture");
    let cursor = root.join("agent-transcripts/cursor-fixture/cursor-fixture.jsonl");
    std::fs::create_dir_all(cursor.parent().unwrap()).unwrap();
    std::fs::write(
        &cursor,
        format!(
            "{}\n",
            json!({"role":"user","message":{"content":[{"type":"text","text":"LIVE-CURSOR"}]}})
        ),
    )
    .unwrap();
    std::fs::write(root.join("cursor-fixture.jsonl"), "UNRELATED-FILE").unwrap();
    link::write(
        &Store::at(f.home().join("store")),
        &link::Link::new("cursor", "cursor-fixture", Some(f.home())),
    )
    .unwrap();
    let output = f.success(&["cursor-fixture", "--public"], None);
    assert!(String::from_utf8_lossy(&output.stderr).contains("1 records checked"));
    assert!(
        f.hub.payloads().last().unwrap()["payload"]
            .as_str()
            .unwrap()
            .contains("LIVE-CURSOR")
    );
    let duplicate = root.join("agent-transcripts/duplicate/cursor-fixture.jsonl");
    std::fs::create_dir_all(duplicate.parent().unwrap()).unwrap();
    std::fs::copy(cursor, duplicate).unwrap();
    f.refuse_without_upload(&["cursor-fixture", "--public"], None);
}

#[test]
fn encrypted_shares_keep_the_key_out_of_the_request_and_preserve_limits() {
    use aes_gcm::{
        Aes256Gcm, Key, Nonce,
        aead::{Aead, KeyInit},
    };
    use base64::Engine;
    let f = Fixture::new("EXCLUDED-LOG");
    f.repo.git(&["checkout", "chosen"]).unwrap();
    f.accept_current();
    let output = f.success(
        &["me/paper@chosen", "--expire", "24h", "--views", "3"],
        None,
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("recover the selected original records")
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let key = stdout
        .split("#k=")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap();
    let request = f.hub.payloads().pop().unwrap();
    assert_eq!(request["encrypted"], true);
    assert_eq!(request["expire_seconds"], 86400);
    assert_eq!(request["max_views"], 3);
    assert!(!request.to_string().contains(key));
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let blob = engine.decode(request["payload"].as_str().unwrap()).unwrap();
    let bytes = engine.decode(key).unwrap();
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&bytes));
    let plaintext = String::from_utf8(
        cipher
            .decrypt(Nonce::from_slice(&blob[..12]), &blob[12..])
            .unwrap(),
    )
    .unwrap();
    assert_eq!(request["format_version"], 2);
    let envelope = PrivacyEnvelope::parse(plaintext.as_bytes()).unwrap();
    assert_eq!(envelope.public_projection["kind"], "share");
    let visible = f
        .repo
        .show_result(&f.sha, meta::VIEW_FILE)
        .unwrap()
        .unwrap();
    let recovered = envelope.open_layer(&SecretKey::from([31; 32])).unwrap();
    let (private_log, private_view) = recovered.session_bytes().unwrap();
    assert_eq!(private_log.as_str(), visible);
    assert_eq!(private_view.as_str(), visible);
    assert!(
        !serde_json::to_string(&recovered)
            .unwrap()
            .contains("EXCLUDED-LOG")
    );
    assert!(envelope.open_layer(&SecretKey::from([32; 32])).is_err());
    assert!(plaintext.contains("VISIBLE-SAVED"));
    assert!(!plaintext.contains("EXCLUDED-LOG"));
    let mut tampered = envelope;
    tampered.public_projection["presentation"] = json!("changed presentation");
    assert!(tampered.open_layer(&SecretKey::from([31; 32])).is_err());
    assert!(stdout.contains("VIEW of me/paper@"));
}

#[test]
fn privacy_shares_refuse_recipient_drift_and_do_not_fall_back_to_legacy_writes() {
    let f = Fixture::new("EXCLUDED-LOG");
    f.repo.git(&["checkout", "chosen"]).unwrap();
    f.accept_current();
    let exported = f.run_command(
        "export",
        &["me/paper@chosen", "--format", "privacy-envelope"],
        None,
    );
    assert!(exported.status.success(), "{exported:?}");
    let exported = PrivacyEnvelope::parse(&exported.stdout).unwrap();
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/privacy-repository-key.json")).unwrap();
    let record: agit::domain::privacy_key::KeyRecord =
        serde_json::from_value(fixture["record"].clone()).unwrap();
    let private = record
        .unlock(&zeroize::Zeroizing::new(
            fixture["password"].as_str().unwrap().into(),
        ))
        .unwrap();
    assert!(
        exported
            .open_layer(&SecretKey::from_slice(private.as_ref()).unwrap())
            .is_ok()
    );
    let f = Fixture::new("EXCLUDED-LOG");
    f.repo.git(&["checkout", "chosen"]).unwrap();
    f.accept_current();
    f.hub.rotate_key.store(true, Ordering::SeqCst);
    let output = f.run(&["me/paper@chosen"], None);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("does not match the selected publication")
    );
    assert!(f.hub.payloads().is_empty());
    let f = Fixture::new("EXCLUDED-LOG");
    f.repo.git(&["checkout", "chosen"]).unwrap();
    f.accept_current();
    f.hub.legacy.store(true, Ordering::SeqCst);
    f.refuse_without_upload(&["me/paper@chosen"], None);
}

#[test]
fn unbound_live_encrypted_shares_refuse_before_upload() {
    let f = Fixture::new("EXCLUDED-LOG");
    f.native("unbound-native");
    let output = f.run(&["unbound-native"], None);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("standalone encrypted sharing"),
        "{output:?}"
    );
    assert!(f.hub.payloads().is_empty());
}

#[test]
fn privacy_exports_preserve_output_when_the_recipient_changes() {
    let f = Fixture::new("EXCLUDED-LOG");
    f.repo.git(&["checkout", "chosen"]).unwrap();
    f.accept_current();
    let output_path = f.home().join("export.json");
    std::fs::write(&output_path, "UNCHANGED_OUTPUT").unwrap();
    f.hub.rotate_key.store(true, Ordering::SeqCst);
    let result = f.run_command(
        "export",
        &[
            "me/paper@chosen",
            "--format",
            "privacy-envelope",
            "--out",
            output_path.to_str().unwrap(),
        ],
        None,
    );
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("does not match the selected publication"),
        "{result:?}"
    );
    assert_eq!(
        std::fs::read_to_string(output_path).unwrap(),
        "UNCHANGED_OUTPUT"
    );
    assert!(f.hub.payloads().is_empty());
}

#[test]
fn saved_and_live_shares_apply_the_same_scope_rewrites_and_aliases() {
    use agit::domain::privacy::{BranchRestriction, PrivacyPolicy, ReplacementRule};

    let f = Fixture::new("EXCLUDED-LOG");
    let source_hub = Hub::start();
    let credential_path = |url: &str| {
        f.home().join("credentials").join(format!(
            "{}.json",
            agit::infra::config::hub_host_key(url).unwrap()
        ))
    };
    let mut credential = agit::infra::credentials::load_at(&credential_path(&f.hub.url)).unwrap();
    credential.hub = Some(source_hub.url.clone());
    agit::infra::credentials::save_at(&credential_path(&source_hub.url), &credential).unwrap();
    f.repo
        .set_remote_named("origin", &format!("{}/me/paper.git", source_hub.url))
        .unwrap();
    agit::hub::identity::pin(
        &f.repo,
        &agit::hub::identity::RemoteIdentity::new(
            &source_hub.url,
            "00000000-0000-0000-0000-000000000001",
        )
        .unwrap(),
    )
    .unwrap();
    let private_file = f.home().join("src/private.rs");
    let managed_file = f.home().join("src/managed.rs");
    let public_file = f.home().join("src/main.rs");
    std::fs::create_dir_all(private_file.parent().unwrap()).unwrap();
    std::fs::write(&public_file, "public source\n").unwrap();
    std::fs::write(&private_file, "private source\n").unwrap();
    std::fs::write(&managed_file, "managed source\n").unwrap();
    let allowed_output = format!(
        "{}\nALLOWED_TOOL_TAIL\u{1b}[2J",
        "Allowed output ".repeat(30)
    );
    let raw = [
        json!({"type":"user","sessionId":"native-fixture","cwd":f.home(),"message":{"role":"user","content":format!("PRIVATE_LABEL Read {} and {}", public_file.display(), private_file.display())}}),
        json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"read-private","name":"Read","input":{"file_path":private_file}}]}}),
        json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"read-private","content":"PRIVATE_SOURCE_OUTPUT"}]}}),
        json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"read-managed","name":"Read","input":{"file_path":managed_file}}]}}),
        json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"read-managed","content":"MANAGED_SOURCE_OUTPUT"}]}}),
        json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"read-public","name":"Read","input":{"file_path":public_file}}]}}),
        json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"read-public","content":allowed_output}]}}),
        json!({"type":"assistant","message":{"role":"assistant","content":"VISIBLE-SAVED"}}),
    ].into_iter().map(|record| format!("{record}\n")).collect::<String>();
    let claim = format!("agit-{}", "b".repeat(40));
    let view = transcript::wrap_lines(&raw, "claude-code", &claim);
    f.repo.git(&["checkout", "chosen"]).unwrap();
    storage::write_snapshot(f.repo.root(), &(envelope("HIDDEN_HISTORY") + &view), &view).unwrap();
    f.repo.add_all().unwrap();
    f.repo
        .commit("Session with source-bound tool output")
        .unwrap();
    f.repo.git(&["tag", "privacy-saved"]).unwrap();
    f.repo.git(&["checkout", "main"]).unwrap();
    let policy = PrivacyPolicy {
        exclude: vec!["src/managed.rs".into()],
        workspace: Some(f.home().to_path_buf()),
        replacements: vec![ReplacementRule {
            pattern: "PRIVATE_LABEL".into(),
            replacement: "Public label".into(),
            regex: false,
        }],
        branches: [(
            "chosen".into(),
            BranchRestriction {
                exclude: vec!["src/private.rs".into()],
                ..Default::default()
            },
        )]
        .into(),
        ..Default::default()
    };
    policy.save(&f.repo).unwrap();
    let path = f.native("native-fixture");
    std::fs::write(&path, &raw).unwrap();
    let store = Store::at(f.home().join("store"));
    let mut native = link::get(&store, "claude-code", "native-fixture").unwrap();
    native.owner = Some("me".into());
    native.agent = Some("paper".into());
    native.branch = Some("chosen".into());
    link::write(&store, &native).unwrap();

    for target in [
        "me/paper@chosen",
        "me/paper@privacy-saved",
        "native-fixture",
    ] {
        let output = f.success(&[target, "--public"], None);
        let request = f.hub.payloads().pop().unwrap();
        let text = request["payload"].as_str().unwrap();
        let public: Value = serde_json::from_str(text).unwrap();
        assert_eq!(public["kind"], "share");
        assert!(public["session"]["log"].is_string());
        assert!(
            public["presentation"]
                .as_str()
                .unwrap()
                .contains("VISIBLE-SAVED")
        );
        assert!(text.contains("Public label"));
        assert!(text.contains("<workspace>/src/main.rs"), "{text}");
        assert!(text.contains("<private-file-1>"));
        for excluded in [
            "PRIVATE_LABEL",
            "PRIVATE_SOURCE_OUTPUT",
            "MANAGED_SOURCE_OUTPUT",
            "src/managed.rs",
            "src/private.rs",
            "HIDDEN_HISTORY",
            f.home().to_str().unwrap(),
        ] {
            assert!(!text.contains(excluded), "{text}");
        }
        let preview = String::from_utf8_lossy(&output.stderr);
        assert!(preview.contains("Privacy preview"));
        assert!(preview.contains("Source LOG record"));
        assert!(preview.contains("tool source is excluded, ambiguous, or unknown"));
        assert!(preview.contains("original records are not included"));
        assert!(preview.contains("ALLOWED_TOOL_TAIL\\u{1b}[2J"));
        assert!(!preview.contains('\u{1b}'));
        assert!(!preview.contains("PRIVATE_SOURCE_OUTPUT"));
        assert!(!preview.contains("HIDDEN_HISTORY"));
    }
    assert_eq!(std::fs::read_to_string(path).unwrap(), raw);
}

#[test]
fn legacy_saved_view_must_be_reachable_from_its_log() {
    let f = Fixture::new("EXCLUDED-LOG");
    f.repo.git(&["checkout", "--detach", &f.sha]).unwrap();
    let mut snapshot = meta::read(f.repo.root()).unwrap();
    snapshot.layout = meta::LayoutVersion::V0;
    std::fs::write(
        f.repo.root().join(meta::FILE),
        meta::to_text(&snapshot).unwrap(),
    )
    .unwrap();
    std::fs::write(
        f.repo.root().join(meta::LEGACY_LOG_FILE),
        envelope("VISIBLE-LOG"),
    )
    .unwrap();
    std::fs::write(
        f.repo.root().join(meta::LEGACY_VIEW_FILE),
        envelope("OUTSIDE-LOG"),
    )
    .unwrap();
    f.repo.add_all().unwrap();
    f.repo.commit("unreachable legacy view probe").unwrap();
    f.repo.git(&["tag", "legacy-unreachable"]).unwrap();
    f.repo.git(&["checkout", "main"]).unwrap();
    f.refuse_without_upload(&["me/paper@legacy-unreachable", "--public"], None);
    f.success(
        &["me/paper@legacy-unreachable", "--public", "--full-log"],
        None,
    );
    let request = f.hub.payloads().pop().unwrap();
    let text = request["payload"].as_str().unwrap();
    assert!(text.contains("VISIBLE-LOG"));
    assert!(!text.contains("OUTSIDE-LOG"));
}

#[test]
fn injected_session_never_falls_back_to_tags_or_remote_branches() {
    let f = Fixture::new("EXCLUDED-LOG");
    f.repo.git(&["tag", "chosen", &f.sha]).unwrap();
    f.repo.git(&["branch", "-D", "chosen"]).unwrap();
    for args in [
        vec!["--public"],
        vec!["@", "--public"],
        vec!["@#1", "--public"],
        vec!["@~0", "--public"],
        vec!["--public", "--full-log"],
    ] {
        f.refuse_without_upload(&args, Some("me/paper@chosen"));
    }
    f.repo.git(&["tag", "-d", "chosen"]).unwrap();
    f.repo
        .git(&["update-ref", "refs/remotes/origin/chosen", &f.sha])
        .unwrap();
    f.refuse_without_upload(&["--public"], Some("me/paper@chosen"));
    f.refuse_without_upload(&["@", "--public"], Some("me/paper@chosen"));
}

#[test]
fn injected_session_selects_its_branch_even_when_other_names_collide() {
    let f = Fixture::new("EXCLUDED-LOG");
    f.repo.git(&["tag", "chosen", "main"]).unwrap();
    f.repo.git(&["branch", &f.sha, "main"]).unwrap();
    for args in [
        vec!["--public"],
        vec!["@", "--public"],
        vec!["@#1", "--public"],
        vec!["@~0", "--public"],
    ] {
        f.success(&args, Some("me/paper@chosen"));
        let request = f.hub.payloads().pop().unwrap();
        let text = request["payload"].as_str().unwrap();
        assert!(text.contains("VISIBLE-SAVED"));
        assert!(!text.contains("EXCLUDED-LOG"));
    }
    f.refuse_without_upload(&["me/paper@chosen", "--public"], None);
}

#[test]
fn saved_views_require_balanced_markers_in_each_layout() {
    fn marker(subtype: &str) -> String {
        transcript::wrap_lines(
            &format!(
                "{}\n",
                json!({"type":"system","subtype":subtype,"source":"me/source","content":"saved boundary"})
            ),
            "claude-code",
            &format!("agit-{}", "b".repeat(40)),
        )
    }
    for legacy in [false, true] {
        let f = Fixture::new("EXCLUDED-LOG");
        let save = |tag: &str, view: &str| {
            f.repo.git(&["checkout", "--detach", &f.sha]).unwrap();
            storage::write_snapshot(f.repo.root(), view, view).unwrap();
            if legacy {
                let mut snapshot = meta::read(f.repo.root()).unwrap();
                snapshot.layout = meta::LayoutVersion::V0;
                std::fs::write(
                    f.repo.root().join(meta::FILE),
                    meta::to_text(&snapshot).unwrap(),
                )
                .unwrap();
                std::fs::write(f.repo.root().join(meta::LEGACY_LOG_FILE), view).unwrap();
                std::fs::write(f.repo.root().join(meta::LEGACY_VIEW_FILE), view).unwrap();
            }
            f.repo.add_all().unwrap();
            f.repo.commit("saved marker boundary").unwrap();
            f.repo.git(&["tag", tag]).unwrap();
            f.repo.git(&["checkout", "main"]).unwrap();
        };
        let visible = envelope("VISIBLE-SAVED");
        let end = marker("agit:__merge_end__");
        save("invalid-marker", &(visible.clone() + &end));
        f.refuse_without_upload(&["me/paper@invalid-marker", "--public"], None);
        f.success(&["me/paper@invalid-marker", "--public", "--full-log"], None);
        save(
            "valid-marker",
            &(marker("agit:__merge_start__") + &visible + &end),
        );
        f.success(&["me/paper@valid-marker", "--public"], None);
        let request = f.hub.payloads().pop().unwrap();
        assert!(
            request["payload"]
                .as_str()
                .unwrap()
                .contains("VISIBLE-SAVED")
        );
    }
}

#[test]
fn legacy_view_only_synthetic_lines_require_the_writer_shape() {
    let f = Fixture::new("EXCLUDED-LOG");
    let visible = envelope("VISIBLE-SAVED");
    let claim = format!("agit-{}", "b".repeat(40));
    let marker =
        |kind| agit::commands::merge::marker_envelope(kind, "claude-code", &claim, "me/source#1");
    let save = |tag: &str, view: &str| {
        f.repo.git(&["checkout", "--detach", &f.sha]).unwrap();
        let mut snapshot = meta::read(f.repo.root()).unwrap();
        snapshot.layout = meta::LayoutVersion::V0;
        std::fs::write(
            f.repo.root().join(meta::FILE),
            meta::to_text(&snapshot).unwrap(),
        )
        .unwrap();
        std::fs::write(f.repo.root().join(meta::LEGACY_LOG_FILE), &visible).unwrap();
        std::fs::write(f.repo.root().join(meta::LEGACY_VIEW_FILE), view).unwrap();
        f.repo.add_all().unwrap();
        f.repo.commit("legacy view-only synthetic lines").unwrap();
        f.repo.git(&["tag", tag]).unwrap();
        f.repo.git(&["checkout", "main"]).unwrap();
    };
    let valid = marker("__merge_start__")
        + &marker("__cherry_pick_start__")
        + &visible
        + &marker("__cherry_pick_end__")
        + &agit::commands::merge::summary_envelope("Saved summary", "claude-code", &claim)
        + &marker("__merge_end__")
        + &marker("__revert__");
    save("legacy-synthetic", &valid);
    f.success(&["me/paper@legacy-synthetic", "--public"], None);
    assert!(
        f.hub.payloads().pop().unwrap()["payload"]
            .as_str()
            .unwrap()
            .contains("VISIBLE-SAVED")
    );

    for (index, content) in [
        json!({"type":"system","subtype":"agit:__unknown__","source":"me/source#1"}),
        json!({"type":"system","subtype":"agit:__revert__","source":"me/source#1","content":"extra payload"}),
        json!({"type":"user","agit":"merge_summary","message":{"role":"user","content":[{"type":"tool_use","name":"hidden"}]}}),
        json!({"type":"user","agit":"merge_summary","message":{"role":"assistant","content":"wrong role"}}),
    ]
    .into_iter()
    .enumerate()
    {
        let forged = transcript::wrap_lines(
            &format!("{content}\n"),
            "claude-code",
            &claim,
        );
        let tag = format!("legacy-forged-{index}");
        save(&tag, &(visible.clone() + &forged));
        f.refuse_without_upload(&[&format!("me/paper@{tag}"), "--public"], None);
    }
    save(
        "legacy-unbalanced",
        &(visible.clone() + &marker("__merge_end__")),
    );
    f.refuse_without_upload(&["me/paper@legacy-unbalanced", "--public"], None);
    save("legacy-foreign", &(valid + &envelope("FOREIGN-REAL-EVENT")));
    f.refuse_without_upload(&["me/paper@legacy-foreign", "--public"], None);
}
