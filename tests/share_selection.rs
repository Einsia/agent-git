//! A share uploads only its selected saved sequence or explicitly selected live transcript.

#![cfg(unix)]

use agit::domain::{link, meta, repo::Repo, storage, store::Store, transcript};
use serde_json::{Value, json};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

struct Hub {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
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
        let worker = std::thread::spawn(move || {
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
                            assert!(header.starts_with("POST /api/shares "));
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
                    let body = r#"{"slug":"fixture","url":"http://127.0.0.1/share/fixture"}"#;
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                }
            }
        });
        Self {
            url,
            requests,
            stop,
            worker: Some(worker),
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

impl Fixture {
    fn new(excluded: &str) -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path();
        let hub = Hub::start();
        let credential = agit::infra::credentials::HubCredential {
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

    fn home(&self) -> &Path {
        self.temporary.path()
    }

    fn run(&self, args: &[&str], session: Option<&str>) -> Output {
        let mut stdout = tempfile::tempfile().unwrap();
        let mut stderr = tempfile::tempfile().unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command
            .args(["-y", "share"])
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
                panic!("share subprocess exceeded its deadline");
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
            "unexpected share: {}",
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
    f.refuse_without_upload(&["me/paper@chosen", "--public", "--full-log"], None);
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
}

#[test]
fn encrypted_shares_keep_the_key_out_of_the_request_and_preserve_limits() {
    use aes_gcm::{
        Aes256Gcm, Key, Nonce,
        aead::{Aead, KeyInit},
    };
    use base64::Engine;
    let f = Fixture::new("EXCLUDED-LOG");
    let output = f.success(
        &["me/paper@chosen", "--expire", "24h", "--views", "3"],
        None,
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
    assert!(plaintext.contains("VISIBLE-SAVED"));
    assert!(!plaintext.contains("EXCLUDED-LOG"));
    assert!(stdout.contains("VIEW of me/paper@"));
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
