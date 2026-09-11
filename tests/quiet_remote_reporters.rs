//! Quiet remote commands retain data and authority while suppressing their own routine notices.

use serde_json::{Value, json};
use std::collections::VecDeque;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const ACCESS: &str = "SYNTHETIC-quiet-access";
const REFRESH: &str = "SYNTHETIC-quiet-refresh";
const PAT: &str = "SYNTHETIC-quiet-pat";

#[derive(Debug)]
struct Request {
    method: String,
    path: String,
    authorization: Option<String>,
    body: Vec<u8>,
}

struct Reply {
    method: &'static str,
    path: &'static str,
    status: u16,
    body: Value,
}

impl Reply {
    fn ok(method: &'static str, path: &'static str, body: Value) -> Self {
        Self {
            method,
            path,
            status: 200,
            body,
        }
    }
}

struct Hub {
    base: String,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<Vec<Request>>>,
}

impl Hub {
    fn new(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let worker = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(120);
            let mut replies = VecDeque::from(replies);
            let mut seen = Vec::new();
            while !stopping.load(Ordering::Acquire) {
                assert!(Instant::now() < deadline, "synthetic Hub deadline elapsed");
                let (mut socket, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(e) => panic!("synthetic Hub accept failed: {e}"),
                };
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let request = read_request(&mut socket);
                let reply = replies.pop_front().expect("unexpected request or replay");
                assert_eq!(request.method, reply.method);
                assert_eq!(request.path, reply.path);
                seen.push(request);
                let body = reply.body.to_string();
                write!(socket, "HTTP/1.1 {} Synthetic\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", reply.status, body.len()).unwrap();
                socket.flush().unwrap();
            }
            assert!(replies.is_empty(), "expected requests were not sent");
            seen
        });
        Self {
            base,
            stop,
            worker: Some(worker),
        }
    }

    fn finish(mut self) -> Vec<Request> {
        self.stop.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap()
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let failed = worker.join().is_err();
            assert!(!failed || std::thread::panicking(), "synthetic Hub failed");
        }
    }
}

fn read_request(socket: &mut TcpStream) -> Request {
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    let end = loop {
        let n = socket.read(&mut buffer).unwrap();
        assert_ne!(n, 0, "truncated request headers");
        bytes.extend_from_slice(&buffer[..n]);
        assert!(bytes.len() <= 65536, "request exceeds header budget");
        if let Some(end) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let header = std::str::from_utf8(&bytes[..end]).unwrap();
    let mut lines = header.lines();
    let mut first = lines.next().unwrap().split_whitespace();
    let method = first.next().unwrap().to_owned();
    let path = first.next().unwrap().to_owned();
    let mut length = 0;
    let mut authorization = None;
    for line in lines {
        if let Some((key, value)) = line.split_once(':') {
            if key.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse::<usize>().unwrap();
            }
            if key.eq_ignore_ascii_case("authorization") {
                authorization = Some(value.trim().to_owned());
            }
        }
    }
    assert!(length <= 65536, "request exceeds body budget");
    while bytes.len() < end + length {
        let n = socket.read(&mut buffer).unwrap();
        assert_ne!(n, 0, "truncated request body");
        bytes.extend_from_slice(&buffer[..n]);
    }
    Request {
        method,
        path,
        authorization,
        body: bytes[end..end + length].to_vec(),
    }
}

struct Lab {
    root: tempfile::TempDir,
    home: PathBuf,
    store: PathBuf,
}

impl Lab {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let store = root.path().join("agit");
        fs::create_dir_all(&home).unwrap();
        Self { root, home, store }
    }

    fn command(&self, base: &str, args: &[&str], mode: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command
            .args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("AGIT_HOME", &self.store)
            .env("AGIT_HUB_URL", base)
            .env("CI", "1")
            .env("NO_COLOR", "1")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(windows) { "os" } else { "file" },
            )
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ALLOW_PROTOCOL", "")
            .current_dir(self.root.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if mode == "flag" {
            command.arg("--quiet");
        } else if mode != "ordinary" {
            command.env("AGIT_QUIET", mode);
        }
        #[cfg(windows)]
        for key in ["SystemRoot", "WINDIR", "TEMP", "TMP", "ComSpec"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        command
    }

    fn git(&self, repo: &agit::domain::repo::Repo, args: &[&str]) -> String {
        let environment = self.command("http://127.0.0.1:1", &[], "ordinary");
        let mut command = Command::new("git");
        command
            .env_clear()
            .envs(
                environment
                    .get_envs()
                    .filter_map(|(key, value)| value.map(|value| (key, value))),
            )
            .arg("-C")
            .arg(repo.root())
            .args(args)
            .current_dir(self.root.path())
            .stdin(Stdio::null());
        String::from_utf8(success(command.output().unwrap()).stdout)
            .unwrap()
            .trim_end()
            .to_owned()
    }

    fn credential_path(&self, base: &str) -> PathBuf {
        self.store.join("credentials").join(format!(
            "{}.json",
            agit::infra::config::hub_host_key(base).unwrap()
        ))
    }

    fn signed_in(&self, base: &str) {
        agit::infra::credentials::save_at(
            &self.credential_path(base),
            &agit::infra::credentials::HubCredential {
                username: "me".into(),
                email: None,
                hub: Some(base.into()),
                access_token: ACCESS.into(),
                refresh_token: REFRESH.into(),
                access_expires_at: "2099-01-01T00:00:00Z".into(),
                refresh_expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .unwrap();
    }
}

fn login_response() -> Value {
    json!({"username":"me", "access_token":ACCESS, "refresh_token":REFRESH,
        "access_expires_at":"2099-01-01T00:00:00Z", "refresh_expires_at":"2099-01-01T00:00:00Z"})
}

fn login(mut command: Command) -> Output {
    command.stdin(Stdio::piped());
    let mut child = command.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{PAT}\n").as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn success(output: Output) -> Output {
    assert!(output.status.success(), "{output:?}");
    for secret in [ACCESS, REFRESH, PAT] {
        assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(secret));
    }
    output
}

#[test]
fn quiet_auth_keeps_token_exchange_revoke_and_local_assets() {
    for mode in ["ordinary", "flag", "", "1"] {
        let hub = Hub::new(vec![
            Reply::ok("POST", "/api/auth/login", login_response()),
            Reply::ok("POST", "/api/auth/logout", json!({})),
        ]);
        let lab = Lab::new();
        let output = success(login(lab.command(
            &hub.base,
            &["login", "--with-token"],
            mode,
        )));
        if mode == "ordinary" {
            assert!(String::from_utf8_lossy(&output.stdout).contains("signed in as me"));
        } else {
            assert!(output.stdout.is_empty(), "{output:?}");
        }
        assert!(output.stderr.is_empty(), "{output:?}");
        let saved: Value =
            serde_json::from_slice(&fs::read(lab.credential_path(&hub.base)).unwrap()).unwrap();
        assert_eq!(saved["username"], "me");
        assert_eq!(saved["access_token"], ACCESS);
        let sentinel = lab.store.join("store/owned.txt");
        fs::create_dir_all(sentinel.parent().unwrap()).unwrap();
        fs::write(&sentinel, b"SYNTHETIC-LOCAL-ASSET").unwrap();
        let args: &[&str] = if mode == "1" {
            &["logout", "--all"]
        } else {
            &["logout"]
        };
        let output = success(lab.command(&hub.base, args, mode).output().unwrap());
        if mode != "ordinary" {
            assert!(output.stdout.is_empty(), "{output:?}");
        }
        assert!(output.stderr.is_empty(), "{output:?}");
        assert!(!lab.credential_path(&hub.base).exists());
        assert_eq!(fs::read(sentinel).unwrap(), b"SYNTHETIC-LOCAL-ASSET");
        for args in [&["logout"][..], &["logout", "--all"][..]] {
            let output = success(lab.command(&hub.base, args, mode).output().unwrap());
            if mode != "ordinary" {
                assert!(output.stdout.is_empty(), "{output:?}");
            }
        }
        let requests = hub.finish();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            serde_json::from_slice::<Value>(&requests[0].body).unwrap(),
            json!({"token":PAT})
        );
        assert!(requests[0].authorization.is_none());
        assert_eq!(
            requests[1].authorization.as_deref(),
            Some(format!("Bearer {ACCESS}").as_str())
        );
    }
}

#[test]
fn quiet_auth_preserves_revoke_failure_and_interactive_recovery() {
    for mode in ["flag", "", "1"] {
        let hub = Hub::new(vec![Reply {
            method: "POST",
            path: "/api/auth/logout",
            status: 500,
            body: json!({"error":"synthetic revoke failure"}),
        }]);
        let lab = Lab::new();
        lab.signed_in(&hub.base);
        let output = success(lab.command(&hub.base, &["logout"], mode).output().unwrap());
        assert!(output.stdout.is_empty(), "{output:?}");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains("server-side revoke failed")
                && error.contains("local credentials are still deleted"),
            "{error}"
        );
        assert!(!lab.credential_path(&hub.base).exists());
        let refused = lab.command(&hub.base, &["login"], mode).output().unwrap();
        assert_eq!(refused.status.code(), Some(8), "{refused:?}");
        assert!(refused.stdout.is_empty(), "{refused:?}");
        assert!(String::from_utf8_lossy(&refused.stderr).contains("--with-token"));
        assert_eq!(hub.finish().len(), 1);
    }
}

#[test]
fn quiet_auth_json_keeps_complete_login_and_logout_results() {
    for version in ["1", "2"] {
        let hub = Hub::new(
            (0..2)
                .flat_map(|_| {
                    [
                        Reply::ok("POST", "/api/auth/login", login_response()),
                        Reply::ok("POST", "/api/auth/logout", json!({})),
                    ]
                })
                .collect(),
        );
        let mut outputs = Vec::new();
        for mode in ["ordinary", "flag"] {
            let lab = Lab::new();
            let args = ["--json", "--json-version", version, "login", "--with-token"];
            let signed = success(login(lab.command(&hub.base, &args, mode)));
            let out = success(
                lab.command(
                    &hub.base,
                    &["--json", "--json-version", version, "logout"],
                    mode,
                )
                .output()
                .unwrap(),
            );
            assert!(signed.stderr.is_empty() && out.stderr.is_empty());
            let signed: Value = serde_json::from_slice(&signed.stdout).unwrap();
            assert!(
                signed["result"]["lines"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|v| v.as_str().unwrap().contains("signed in as me"))
            );
            outputs.push((
                signed,
                serde_json::from_slice::<Value>(&out.stdout).unwrap(),
            ));
        }
        assert_eq!(outputs[0], outputs[1]);
        assert_eq!(hub.finish().len(), 4);
    }
}

#[test]
fn quiet_device_login_keeps_the_authorization_url_and_code() {
    for mode in ["ordinary", "flag"] {
        let hub = Hub::new(vec![
            Reply::ok(
                "POST",
                "/api/auth/device/code",
                json!({
                    "device_code":"SYNTHETIC-device", "user_code":"SYNTHETIC-CODE",
                    "verification_uri":"https://example.invalid/authorize", "interval":2, "expires_in":60
                }),
            ),
            Reply::ok("POST", "/api/auth/device/token", login_response()),
        ]);
        let lab = Lab::new();
        let output = success(
            lab.command(&hub.base, &["login", "--device"], mode)
                .output()
                .unwrap(),
        );
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains("https://example.invalid/authorize"));
        assert!(text.contains("SYNTHETIC-CODE"));
        assert_eq!(text.contains("waiting"), mode == "ordinary");
        assert!(output.stderr.is_empty());
        assert!(lab.credential_path(&hub.base).exists());
        let requests = hub.finish();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            serde_json::from_slice::<Value>(&requests[1].body).unwrap(),
            json!({"device_code":"SYNTHETIC-device"})
        );
    }
}

impl Lab {
    fn saved_share(&self, base: &str) -> agit::domain::repo::Repo {
        use agit::domain::{meta, storage, transcript};
        self.signed_in(base);
        success(
            self.command(base, &["init", "qa", "--no-bind"], "ordinary")
                .output()
                .unwrap(),
        );
        let repo = agit::domain::repo::Repo::open(self.store.join("repos/me/qa")).unwrap();
        self.git(&repo, &["checkout", "-b", "chosen"]);
        let session = format!("agit-{}", "b".repeat(40));
        let raw = format!(
            "{}\n",
            json!({"type":"user", "sessionId":"synthetic-share", "message":{"role":"user", "content":"SYNTHETIC-SHARE-CONTENT"}})
        );
        let envelope = transcript::wrap_lines(&raw, "claude-code", &session);
        storage::write_snapshot(repo.root(), &envelope, &envelope).unwrap();
        let mut metadata = meta::Meta::new(
            session,
            "claude-code".into(),
            self.root.path().to_string_lossy().into(),
        );
        metadata.turn = Some(1);
        meta::write(repo.root(), &metadata).unwrap();
        self.git(&repo, &["add", "."]);
        self.git(&repo, &["commit", "-m", "Seed synthetic share source"]);
        repo
    }
}

#[cfg(windows)]
impl Drop for Lab {
    fn drop(&mut self) {
        use agit::domain::secret_filter::KeyStore;
        if let Ok(bytes) = fs::read(self.store.join("secret-filter/vault.json"))
            && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
            && let Some(id) = value["vault_id"].as_str()
        {
            let _ = agit::domain::secret_filter::OsKeyStore.delete(id);
        }
    }
}

#[test]
fn quiet_share_preserves_the_created_link_policy_data_and_revoke_hint() {
    for mode in ["ordinary", "flag", "", "1"] {
        let hub = Hub::new(vec![Reply::ok(
            "POST",
            "/api/shares",
            json!({"slug":"owned-share", "url":"https://example.invalid/s/owned-share"}),
        )]);
        let lab = Lab::new();
        let repo = lab.saved_share(&hub.base);
        let head = lab.git(&repo, &["rev-parse", "HEAD"]);
        let args = [
            "--yes",
            "share",
            "me/qa@chosen",
            "--public",
            "--expire",
            "1h",
            "--views",
            "3",
        ];
        let output = success(lab.command(&hub.base, &args, mode).output().unwrap());
        let text = String::from_utf8(output.stdout).unwrap();
        assert_eq!(text.contains("share created"), mode == "ordinary");
        for data in [
            "https://example.invalid/s/owned-share",
            "source",
            "encrypted",
            "public",
            "expires",
            "1h",
            "view cap",
            "3",
            "passphrase",
        ] {
            assert!(text.contains(data), "{data}: {text}");
        }
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("revoke: agit share rm owned-share")
        );
        assert_eq!(lab.git(&repo, &["rev-parse", "HEAD"]), head);
        let requests = hub.finish();
        assert_eq!(requests.len(), 1);
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["encrypted"], false);
        assert_eq!(body["expire_seconds"], 3600);
        assert_eq!(body["max_views"], 3);
        assert!(
            body["payload"]
                .as_str()
                .unwrap()
                .contains("SYNTHETIC-SHARE-CONTENT")
        );
        assert_eq!(
            requests[0].authorization.as_deref(),
            Some(format!("Bearer {ACCESS}").as_str())
        );
    }
}

#[test]
fn quiet_share_keeps_confirmation_refusal_without_a_request() {
    let hub = Hub::new(vec![]);
    let lab = Lab::new();
    let repo = lab.saved_share(&hub.base);
    let head = lab.git(&repo, &["rev-parse", "HEAD"]);
    for mode in ["flag", "", "1"] {
        let output = lab
            .command(&hub.base, &["share", "me/qa@chosen", "--public"], mode)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(8), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains("requires confirmation") && error.contains("--yes"),
            "{error}"
        );
        assert_eq!(lab.git(&repo, &["rev-parse", "HEAD"]), head);
    }
    assert!(hub.finish().is_empty());
}

#[test]
fn quiet_share_json_keeps_the_complete_created_result() {
    for version in ["1", "2"] {
        let hub = Hub::new((0..2).map(|_| Reply::ok("POST", "/api/shares", json!({"slug":"owned-share", "url":"https://example.invalid/s/owned-share"}))).collect());
        let lab = Lab::new();
        lab.saved_share(&hub.base);
        let mut outputs = Vec::new();
        for mode in ["ordinary", "flag"] {
            let out = success(
                lab.command(
                    &hub.base,
                    &[
                        "--json",
                        "--json-version",
                        version,
                        "--yes",
                        "share",
                        "me/qa@chosen",
                        "--public",
                    ],
                    mode,
                )
                .output()
                .unwrap(),
            );
            assert!(out.stderr.is_empty());
            let value: Value = serde_json::from_slice(&out.stdout).unwrap();
            assert!(
                value["result"]["lines"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|v| v.as_str().unwrap().contains("share created"))
            );
            outputs.push(value);
        }
        assert_eq!(outputs[0], outputs[1]);
        assert_eq!(hub.finish().len(), 2);
    }
}

#[test]
fn quiet_upgrade_keeps_explicit_version_queries_and_channel_refusals() {
    let production = agit::infra::config::is_production_release();
    let cases = [(env!("CARGO_PKG_VERSION"), false), ("999.0.0", true)];
    for (version, stale) in cases {
        let hub = Hub::new(if production {
            (0..6).map(|_| Reply::ok("GET", "/api/cli/version", json!({
                "version":version, "tag":format!("v{version}"), "url":"https://example.invalid/release", "stale":stale
            }))).collect()
        } else {
            vec![]
        });
        let lab = Lab::new();
        for args in [
            vec!["upgrade", "--check"],
            vec!["--json", "--json-version", "1", "upgrade", "--check"],
            vec!["--json", "--json-version", "2", "upgrade", "--check"],
        ] {
            let mut results = Vec::new();
            for mode in ["ordinary", "flag"] {
                let out = lab.command(&hub.base, &args, mode).output().unwrap();
                assert_eq!(
                    out.status.code(),
                    Some(if production { 0 } else { 2 }),
                    "{out:?}"
                );
                let text = String::from_utf8_lossy(&out.stdout).to_string()
                    + &String::from_utf8_lossy(&out.stderr);
                if production {
                    assert!(text.contains(version), "{text}");
                    if stale {
                        assert!(text.contains("served from cache"), "{text}");
                    }
                } else {
                    assert!(text.contains("self-upgrade is disabled"), "{text}");
                }
                results.push((out.stdout, out.stderr));
            }
            assert_eq!(results[0], results[1]);
        }
        assert_eq!(hub.finish().len(), if production { 6 } else { 0 });
    }
}

#[test]
fn quiet_upgrade_noop_has_no_routine_output_and_never_downloads() {
    let production = agit::infra::config::is_production_release();
    let hub = Hub::new(if production {
        (0..4)
            .map(|_| {
                Reply::ok(
                    "GET",
                    "/api/cli/version",
                    json!({"version":env!("CARGO_PKG_VERSION"), "tag":"synthetic"}),
                )
            })
            .collect()
    } else {
        vec![]
    });
    let lab = Lab::new();
    for mode in ["ordinary", "flag", "", "1"] {
        let out = lab.command(&hub.base, &["upgrade"], mode).output().unwrap();
        assert_eq!(
            out.status.code(),
            Some(if production { 0 } else { 2 }),
            "{out:?}"
        );
        if production && mode == "ordinary" {
            assert!(String::from_utf8_lossy(&out.stdout).contains("up to date"));
        } else {
            assert!(out.stdout.is_empty(), "{out:?}");
        }
        if production {
            assert!(out.stderr.is_empty(), "{out:?}");
        } else {
            assert!(String::from_utf8_lossy(&out.stderr).contains("fresh artifact"));
        }
    }
    assert_eq!(hub.finish().len(), if production { 4 } else { 0 });
}

#[test]
fn quiet_upgrade_download_failure_retains_diagnostics_without_progress() {
    let production = agit::infra::config::is_production_release();
    let path = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "/synthetic-linux-x64/999.0.0",
        ("linux", "aarch64") => "/synthetic-linux-arm64/999.0.0",
        ("macos", "x86_64") => "/synthetic-darwin-x64/999.0.0",
        ("macos", "aarch64") => "/synthetic-darwin-arm64/999.0.0",
        ("windows", "x86_64") => "/synthetic-win32-x64/999.0.0",
        platform => panic!("unregistered fixture platform: {platform:?}"),
    };
    for args in [
        vec!["upgrade"],
        vec!["--json", "--json-version", "1", "upgrade"],
        vec!["--json", "--json-version", "2", "upgrade"],
    ] {
        let hub = Hub::new(if production {
            (0..2).flat_map(|_| [
                Reply::ok("GET", "/api/cli/version", json!({"version":"999.0.0", "tag":"synthetic", "npm_package":"synthetic"})),
                // Missing dist metadata stops before any executable staging or replacement.
                Reply::ok("GET", path, json!({})),
            ]).collect()
        } else {
            vec![]
        });
        let lab = Lab::new();
        let mut outputs = Vec::new();
        for mode in ["ordinary", "flag"] {
            let out = lab
                .command(&hub.base, &args, mode)
                .env("AGIT_NPM_REGISTRY", &hub.base)
                .output()
                .unwrap();
            assert_eq!(
                out.status.code(),
                Some(if production { 6 } else { 2 }),
                "{out:?}"
            );
            if args[0] == "upgrade" {
                if production {
                    assert_eq!(
                        String::from_utf8_lossy(&out.stdout).contains("fetching synthetic-"),
                        mode == "ordinary"
                    );
                    assert!(String::from_utf8_lossy(&out.stderr).contains("has no dist.tarball"));
                    assert!(String::from_utf8_lossy(&out.stderr).contains("fallback:"));
                }
                if mode != "ordinary" {
                    assert!(out.stdout.is_empty(), "{out:?}");
                }
            } else {
                assert!(out.stderr.is_empty());
                let value: Value = serde_json::from_slice(&out.stdout).unwrap();
                assert_eq!(value["exit_code"], if production { 6 } else { 2 });
            }
            outputs.push((out.stdout, out.stderr));
        }
        if args[0] == "--json" {
            assert_eq!(outputs[0], outputs[1]);
        }
        assert_eq!(hub.finish().len(), if production { 4 } else { 0 });
    }
}

#[test]
fn quiet_repository_and_pr_queries_preserve_requested_data() {
    let cases = [
        (
            vec!["repo", "list", "--remote"],
            "/api/agents",
            json!([]),
            "no repos visible",
        ),
        (
            vec!["repo", "list", "--remote"],
            "/api/agents",
            json!([{
                "agent_id":"synthetic-agent", "owner":"me", "name":"qa", "clone_url":"https://example.invalid/me/qa.git", "visibility":"private", "session_count":7
            }]),
            "7 sessions",
        ),
        (
            vec!["pr", "list", "me/qa"],
            "/api/agents/me/qa/prs",
            json!([]),
            "no open PRs",
        ),
        (
            vec!["pr", "show", "42"],
            "/api/prs/42",
            json!({"id":42, "source":"me/qa:work", "target_branch":"main", "state":"open", "title":"SYNTHETIC-PR", "summary":"SYNTHETIC-SUMMARY"}),
            "SYNTHETIC-SUMMARY",
        ),
        (
            vec!["share", "list"],
            "/api/shares",
            json!([{"slug":"owned-share", "url":"https://example.invalid/s/owned-share"}]),
            "https://example.invalid/s/owned-share",
        ),
    ];
    for (args, path, body, expected) in cases {
        let hub = Hub::new(
            (0..6)
                .map(|_| Reply::ok("GET", path, body.clone()))
                .collect(),
        );
        let lab = Lab::new();
        lab.signed_in(&hub.base);
        for version in [None, Some("1"), Some("2")] {
            let mut selected = Vec::new();
            if let Some(version) = version {
                selected.extend(["--json", "--json-version", version]);
            }
            selected.extend_from_slice(&args);
            let mut outputs = Vec::new();
            for mode in ["ordinary", "flag"] {
                let out = success(lab.command(&hub.base, &selected, mode).output().unwrap());
                assert!(
                    String::from_utf8_lossy(&out.stdout).contains(expected),
                    "{out:?}"
                );
                assert!(out.stderr.is_empty(), "{out:?}");
                outputs.push(out.stdout);
            }
            assert_eq!(outputs[0], outputs[1]);
        }
        let requests = hub.finish();
        assert_eq!(requests.len(), 6);
        for request in requests {
            assert_eq!(
                request.authorization.as_deref(),
                Some(format!("Bearer {ACCESS}").as_str())
            );
            assert!(request.body.is_empty());
        }
    }
}

#[test]
fn quiet_pr_landing_preserves_the_request_and_json_result() {
    let hub = Hub::new(
        (0..6)
            .map(|_| Reply::ok("POST", "/api/prs/42/merge", json!({"mode":"adopt"})))
            .collect(),
    );
    let lab = Lab::new();
    lab.signed_in(&hub.base);
    for version in [None, Some("1"), Some("2")] {
        let mut args = Vec::new();
        if let Some(version) = version {
            args.extend(["--json", "--json-version", version]);
        }
        args.extend(["pr", "merge", "42", "--adopt", "owned-adoption"]);
        let mut outputs = Vec::new();
        for mode in ["ordinary", "flag"] {
            let out = success(lab.command(&hub.base, &args, mode).output().unwrap());
            if version.is_none() && mode == "flag" {
                assert!(out.stdout.is_empty(), "{out:?}");
            } else {
                assert!(String::from_utf8_lossy(&out.stdout).contains("PR #42 landed (adopt)"));
            }
            assert!(out.stderr.is_empty());
            outputs.push(out.stdout);
        }
        if version.is_some() {
            assert_eq!(outputs[0], outputs[1]);
        }
    }
    let requests = hub.finish();
    assert_eq!(requests.len(), 6);
    for request in requests {
        assert_eq!(
            serde_json::from_slice::<Value>(&request.body).unwrap(),
            json!({"adopt":"owned-adoption"})
        );
        assert_eq!(
            request.authorization.as_deref(),
            Some(format!("Bearer {ACCESS}").as_str())
        );
    }
}

#[test]
fn quiet_repository_keeps_local_paths_and_refuses_unconfirmed_deletion() {
    let hub = Hub::new(vec![]);
    let lab = Lab::new();
    let repo = lab.saved_share(&hub.base);
    let head = lab.git(&repo, &["rev-parse", "HEAD"]);
    let ordinary = success(
        lab.command(&hub.base, &["repo", "path", "me/qa"], "ordinary")
            .output()
            .unwrap(),
    );
    for mode in ["flag", "", "1"] {
        let out = success(
            lab.command(&hub.base, &["repo", "path", "me/qa"], mode)
                .output()
                .unwrap(),
        );
        assert_eq!(out.stdout, ordinary.stdout);
        assert!(!out.stdout.is_empty());
        let refused = lab
            .command(&hub.base, &["repo", "delete", "me/qa", "--local"], mode)
            .output()
            .unwrap();
        assert_eq!(refused.status.code(), Some(8), "{refused:?}");
        assert!(!refused.stderr.is_empty());
        assert!(repo.root().is_dir());
        assert_eq!(lab.git(&repo, &["rev-parse", "HEAD"]), head);
    }
    assert!(hub.finish().is_empty());
}
