//! Login and RC pairing distinguish remote refusal, local persistence, and interactive admission.

use agit::infra::credentials::{HubCredential, save_at};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const PAT: &str = "SYNTHETIC-login-pat";
const OLD_ACCESS: &str = "SYNTHETIC-old-access";
const OLD_REFRESH: &str = "SYNTHETIC-old-refresh";
const ACCESS: &str = "SYNTHETIC-new-access";
const REFRESH: &str = "SYNTHETIC-new-refresh";
const DEVICE: &str = "SYNTHETIC-private-device";

#[derive(Clone, Debug)]
enum Reply {
    Status(u16),
    TruncatedHeaders,
    Json(Value),
}

#[derive(Debug)]
struct Request {
    method: String,
    target: String,
    authorization: String,
    body: Vec<u8>,
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
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(120);
            let mut requests = Vec::new();
            while !stopping.load(Ordering::Acquire) {
                assert!(Instant::now() < deadline, "loopback Hub deadline elapsed");
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("loopback accept failed: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let reply = replies
                    .get(requests.len())
                    .expect("unexpected request replay");
                requests.push(read_request(&mut stream));
                let (status, body) = match reply {
                    Reply::Status(status) => (
                        *status,
                        json!({
                            "error":"HTTP 401 (synthetic status spoof)",
                            "kind":"unauthorized", "fix":[{"kind":"authenticate"}]
                        }),
                    ),
                    Reply::Json(value) => (200, value.clone()),
                    Reply::TruncatedHeaders => {
                        stream
                            .write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Len",
                            )
                            .unwrap();
                        stream.flush().unwrap();
                        continue;
                    }
                };
                let body = body.to_string();
                write!(stream, "HTTP/1.1 {status} Synthetic\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                stream.flush().unwrap();
            }
            requests
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
            let _ = worker.join();
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Request {
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    let end = loop {
        let read = stream.read(&mut buffer).unwrap();
        assert_ne!(read, 0, "request headers were truncated");
        bytes.extend_from_slice(&buffer[..read]);
        assert!(bytes.len() <= 65536, "request headers exceeded their bound");
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let header = String::from_utf8(bytes[..end].to_vec()).unwrap();
    let mut lines = header.lines();
    let mut start = lines.next().unwrap().split_whitespace();
    let method = start.next().unwrap().to_owned();
    let target = start.next().unwrap().to_owned();
    let mut authorization = String::new();
    let mut length = 0;
    for line in lines {
        if let Some((key, value)) = line.split_once(':') {
            if key.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse::<usize>().unwrap();
            }
            if key.eq_ignore_ascii_case("authorization") {
                authorization = value.trim().to_owned();
            }
        }
    }
    assert!(length <= 65536, "request body exceeded its bound");
    while bytes.len() < end + length {
        let read = stream.read(&mut buffer).unwrap();
        assert_ne!(read, 0, "request body was truncated");
        bytes.extend_from_slice(&buffer[..read]);
    }
    Request {
        method,
        target,
        authorization,
        body: bytes[end..end + length].to_vec(),
    }
}

struct Lab {
    root: tempfile::TempDir,
    home: PathBuf,
    store: PathBuf,
    work: PathBuf,
}

impl Lab {
    fn new(base: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let home = root_path.join("home");
        let store = root_path.join("agit");
        let work = root_path.join("work");
        for path in [&home, &store, &work] {
            fs::create_dir_all(path).unwrap();
        }
        let lab = Self {
            root,
            home,
            store,
            work,
        };
        let output = lab.run(base, &[], &["config", "--list"], None);
        assert!(output.status.success(), "{output:?}");
        lab
    }

    fn credential_path(&self, base: &str) -> PathBuf {
        self.store.join("credentials").join(format!(
            "{}.json",
            agit::infra::config::hub_host_key(base).unwrap()
        ))
    }

    fn seed(&self, base: &str) {
        save_at(
            &self.credential_path(base),
            &HubCredential {
                username: "old-owner".into(),
                email: None,
                hub: Some(base.into()),
                access_token: OLD_ACCESS.into(),
                refresh_token: OLD_REFRESH.into(),
                access_expires_at: "2099-01-01T00:00:00Z".into(),
                refresh_expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .unwrap();
    }

    fn run(&self, base: &str, flags: &[&str], args: &[&str], input: Option<&[u8]>) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("AGIT_HOME", &self.store)
            .env("AGIT_HUB_URL", base)
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(windows) { "os" } else { "file" },
            )
            .env("GIT_CONFIG_GLOBAL", self.home.join("absent-gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("CI", "1")
            .env("NO_COLOR", "1")
            .env("AGIT_TUI", "0")
            .current_dir(&self.work)
            .args(flags)
            .args(args)
            .stdin(Stdio::null());
        #[cfg(windows)]
        for name in ["SystemRoot", "WINDIR", "TEMP", "TMP", "ComSpec"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        let file = input.map(|bytes| {
            let mut file = tempfile::tempfile().unwrap();
            file.write_all(bytes).unwrap();
            std::io::Seek::rewind(&mut file).unwrap();
            file
        });
        if let Some(file) = file {
            command.stdin(file);
        }
        run_bounded(command)
    }

    fn state(&self) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        walkdir::WalkDir::new(self.root.path())
            .into_iter()
            .map(|entry| {
                let entry = entry.unwrap();
                assert!(!entry.file_type().is_symlink());
                (
                    entry
                        .path()
                        .strip_prefix(self.root.path())
                        .unwrap()
                        .to_owned(),
                    entry
                        .file_type()
                        .is_file()
                        .then(|| fs::read(entry.path()).unwrap()),
                )
            })
            .collect()
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

fn modes() -> [Vec<&'static str>; 4] {
    [
        vec![],
        vec!["--quiet"],
        vec!["--json", "--json-version", "1"],
        vec!["--json", "--json-version", "2"],
    ]
}

fn assert_output(output: &Output, flags: &[&str], code: i32) -> Option<Value> {
    assert_command_output(output, flags, "login", code)
}

fn assert_command_output(
    output: &Output,
    flags: &[&str],
    command: &str,
    code: i32,
) -> Option<Value> {
    assert_eq!(output.status.code(), Some(code), "{flags:?}: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    for secret in [PAT, OLD_ACCESS, OLD_REFRESH, ACCESS, REFRESH, DEVICE] {
        assert!(
            !stdout.contains(secret) && !stderr.contains(secret),
            "credential in output"
        );
    }
    if flags.contains(&"--json") {
        assert!(stderr.is_empty(), "{output:?}");
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["schema"], "cli-output");
        assert_eq!(value["command"], command);
        assert_eq!(
            value["schema_version"],
            flags.last().unwrap().parse::<u32>().unwrap()
        );
        assert_eq!(value["exit_code"], code);
        assert_eq!(value["ok"], code == 0);
        if flags.last() == Some(&"1") {
            assert!(value.get("fix").is_none());
        }
        if code != 0 {
            assert!(
                !value["diagnostics"]["stderr"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
        }
        Some(value)
    } else {
        if code != 0 {
            assert!(!stderr.is_empty());
        }
        None
    }
}

fn assert_request(request: &Request, target: &str, body: Value) {
    assert_eq!(request.method, "POST");
    assert_eq!(request.target, target);
    assert!(request.authorization.is_empty());
    assert_eq!(
        serde_json::from_slice::<Value>(&request.body).unwrap(),
        body
    );
}

fn session() -> Value {
    json!({"username":"new-owner", "email":"synthetic@example.test",
        "access_token":ACCESS, "refresh_token":REFRESH,
        "access_expires_at":"2099-01-01T00:00:00Z", "refresh_expires_at":"2099-01-02T00:00:00Z"})
}

fn device(expires: u64) -> Reply {
    Reply::Json(json!({"device_code":DEVICE, "user_code":"VISIBLE-CODE",
        "verification_uri":"https://example.invalid/verify", "interval":2, "expires_in":expires}))
}

#[test]
fn pat_status_and_transport_failures_preserve_identity_and_do_not_retry() {
    for flags in modes() {
        for (reply, code) in [
            (Reply::Status(401), 5),
            (Reply::Status(403), 6),
            (Reply::Status(500), 6),
            (Reply::Status(503), 6),
            (Reply::TruncatedHeaders, 6),
            (
                Reply::Json(json!({"access_token":ACCESS,"refresh_token":REFRESH})),
                6,
            ),
        ] {
            let hub = Hub::new(vec![reply]);
            let lab = Lab::new(&hub.base);
            lab.seed(&hub.base);
            let before = lab.state();
            let output = lab.run(
                &hub.base,
                &flags,
                &["login", "--with-token"],
                Some(PAT.as_bytes()),
            );
            let value = assert_output(&output, &flags, code);
            if let Some(value) = value
                && flags.last() == Some(&"2")
            {
                if code == 5 {
                    assert_eq!(value["fix"].as_array().unwrap().len(), 1);
                    let action = &value["fix"][0];
                    assert_eq!(action["kind"], "agit_command");
                    assert_eq!(action["argv"], json!(["agit", "login", "--hub", hub.base]));
                    assert_eq!(action["env"]["AGIT_HOME"], lab.store.to_str().unwrap());
                    assert_eq!(action["env"]["AGIT_HUB_URL"], hub.base);
                    assert_eq!(
                        action["env"],
                        json!({"AGIT_HOME":lab.store,"AGIT_HUB_URL":hub.base})
                    );
                    let cwd = lab.work.to_str().unwrap();
                    #[cfg(windows)]
                    let cwd = cwd.strip_prefix(r"\\?\").unwrap_or(cwd);
                    assert_eq!(action["cwd"], cwd);
                    assert_eq!(action["requires_interaction"], true);
                } else {
                    assert_eq!(value["fix"], json!([]));
                }
            }
            assert!(
                !String::from_utf8_lossy(&output.stderr).contains("needs an interactive terminal")
            );
            assert_eq!(lab.state(), before);
            let requests = hub.finish();
            assert_eq!(requests.len(), 1);
            assert_request(&requests[0], "/api/auth/login", json!({"token":PAT}));
        }
    }
}

#[test]
fn pat_success_persists_only_the_selected_hub_and_local_save_failure_is_not_network() {
    for flags in modes() {
        for blocked in [false, true] {
            let hub = Hub::new(vec![Reply::Json(session())]);
            let lab = Lab::new(&hub.base);
            if blocked {
                fs::write(lab.store.join("credentials"), b"OWNED-BLOCKER").unwrap();
            } else {
                lab.seed(&hub.base);
                lab.seed("https://other.example.test");
                // Credential locking is a persistent local carrier, independent of login success.
                fs::write(lab.store.join("credentials.lock"), b"").unwrap();
            }
            let before = lab.state();
            let output = lab.run(
                &hub.base,
                &flags,
                &["login", "--with-token"],
                Some(PAT.as_bytes()),
            );
            let code = if blocked { 4 } else { 0 };
            let value = assert_output(&output, &flags, code);
            if let Some(value) = value
                && flags.last() == Some(&"2")
            {
                assert_eq!(value["fix"], json!([]));
            }
            if blocked {
                assert_eq!(lab.state(), before);
            } else {
                let saved: Value =
                    serde_json::from_slice(&fs::read(lab.credential_path(&hub.base)).unwrap())
                        .unwrap();
                assert_eq!(saved["username"], "new-owner");
                assert_eq!(saved["hub"], hub.base);
                assert_eq!(saved["access_token"], ACCESS);
                assert_eq!(saved["refresh_token"], REFRESH);
                let mut after = lab.state();
                let changed = lab
                    .credential_path(&hub.base)
                    .strip_prefix(lab.root.path().canonicalize().unwrap())
                    .unwrap()
                    .to_owned();
                after.insert(changed.clone(), before[&changed].clone());
                assert_eq!(after, before);
            }
            let requests = hub.finish();
            assert_eq!(requests.len(), 1);
            assert_request(&requests[0], "/api/auth/login", json!({"token":PAT}));
        }
    }
}

#[test]
fn input_and_interactive_admission_fail_without_a_request_or_credential_change() {
    for flags in modes() {
        let hub = Hub::new(vec![]);
        let lab = Lab::new(&hub.base);
        lab.seed(&hub.base);
        let before = lab.state();
        for (args, input, code) in [
            (vec!["login"], None, 8),
            (vec!["login", "--with-token"], Some(b"   ".as_slice()), 2),
            (vec!["login", "--with-token"], Some(b"\xff".as_slice()), 4),
            (
                vec!["login", "--with-token", "--hub", "invalid"],
                Some(PAT.as_bytes()),
                2,
            ),
        ] {
            assert_output(&lab.run(&hub.base, &flags, &args, input), &flags, code);
            assert_eq!(lab.state(), before);
        }
        if flags.contains(&"--json") {
            assert_output(
                &lab.run(&hub.base, &flags, &["login", "--device"], None),
                &flags,
                8,
            );
            assert_eq!(lab.state(), before);
        }
        assert!(hub.finish().is_empty());
    }
}

#[test]
fn device_and_poll_failures_keep_their_remote_category_and_expiry_is_interactive() {
    for flags in [vec![], vec!["--quiet"]] {
        for (replies, code, requests_expected) in [
            (vec![Reply::Status(503)], 6, 1),
            (vec![Reply::Status(401)], 5, 1),
            (vec![Reply::TruncatedHeaders], 6, 1),
            (vec![device(120), Reply::Status(503)], 6, 2),
            (vec![device(120), Reply::Status(401)], 5, 2),
            (vec![device(120), Reply::TruncatedHeaders], 6, 2),
            (
                vec![
                    device(120),
                    Reply::Json(json!({"access_token":ACCESS, "refresh_token":REFRESH})),
                ],
                6,
                2,
            ),
            (vec![device(0)], 8, 1),
        ] {
            let hub = Hub::new(replies);
            let lab = Lab::new(&hub.base);
            lab.seed(&hub.base);
            let before = lab.state();
            let output = lab.run(&hub.base, &flags, &["login", "--device"], None);
            assert_output(&output, &flags, code);
            if code == 8 {
                let stderr = String::from_utf8_lossy(&output.stderr);
                assert!(stderr.contains("expired before it was approved"));
                assert!(!stderr.contains("needs an interactive terminal"));
            }
            assert_eq!(lab.state(), before);
            let requests = hub.finish();
            assert_eq!(requests.len(), requests_expected);
            assert_request(&requests[0], "/api/auth/device/code", json!({}));
            if requests_expected == 2 {
                assert_request(
                    &requests[1],
                    "/api/auth/device/token",
                    json!({"device_code":DEVICE}),
                );
            }
        }
    }
}

#[test]
fn pending_device_authorization_can_complete_without_exposing_the_token_pair() {
    for flags in [vec![], vec!["--quiet"]] {
        let hub = Hub::new(vec![
            device(120),
            Reply::Json(json!({"status":"pending"})),
            Reply::Json(session()),
        ]);
        let lab = Lab::new(&hub.base);
        lab.seed(&hub.base);
        lab.seed("https://other.example.test");
        fs::write(lab.store.join("credentials.lock"), b"").unwrap();
        let before = lab.state();
        let output = lab.run(&hub.base, &flags, &["login", "--device"], None);
        assert_output(&output, &flags, 0);
        let saved: Value =
            serde_json::from_slice(&fs::read(lab.credential_path(&hub.base)).unwrap()).unwrap();
        assert_eq!(saved["username"], "new-owner");
        assert_eq!(saved["hub"], hub.base);
        assert_eq!(saved["access_token"], ACCESS);
        assert_eq!(saved["refresh_token"], REFRESH);
        let mut after = lab.state();
        let changed = lab
            .credential_path(&hub.base)
            .strip_prefix(lab.root.path().canonicalize().unwrap())
            .unwrap()
            .to_owned();
        after.insert(changed.clone(), before[&changed].clone());
        assert_eq!(after, before);
        let requests = hub.finish();
        assert_eq!(requests.len(), 3);
        assert_request(&requests[0], "/api/auth/device/code", json!({}));
        for request in &requests[1..] {
            assert_request(
                request,
                "/api/auth/device/token",
                json!({"device_code":DEVICE}),
            );
        }
    }
}

#[test]
fn pairing_storage_refusals_preserve_local_evidence_and_the_request_boundary() {
    for flags in modes() {
        for stage in [
            "identity",
            "paired-identity",
            "connection",
            "malformed-connection",
            "credentials-directory",
            "credentials-malformed",
            "missing-credentials",
        ] {
            let mut replies = vec![Reply::Json(json!({
                "connection_id":"existing-connection", "token":ACCESS
            }))];
            if stage == "connection" {
                replies.push(Reply::Json(json!({
                    "connection_id":"replacement-connection", "token":REFRESH
                })));
            }
            let hub = Hub::new(replies);
            let lab = Lab::new(&hub.base);
            lab.seed(&hub.base);
            let seeded = lab.run(&hub.base, &[], &["rc", "pair"], None);
            assert_command_output(&seeded, &[], "rc", 0);
            let identity = lab.store.join("rc/identity.json");
            let machine: Value = serde_json::from_slice(&fs::read(&identity).unwrap()).unwrap();
            let connection = lab.store.join("rc/connections").join(format!(
                "{}.json",
                agit::infra::config::hub_host_key(&hub.base).unwrap()
            ));
            let credentials = lab.credential_path(&hub.base);
            let carrier = match stage {
                "identity" | "paired-identity" => &identity,
                "connection" | "malformed-connection" => &connection,
                _ => &credentials,
            };
            fs::rename(carrier, carrier.with_extension("saved")).unwrap();
            match stage {
                "malformed-connection" | "credentials-malformed" => {
                    fs::write(carrier, format!("{{\"token\":\"{ACCESS}\", invalid")).unwrap();
                }
                "missing-credentials" => {}
                _ => {
                    fs::create_dir(carrier).unwrap();
                    fs::write(carrier.join("retained-entry"), b"owned local evidence").unwrap();
                }
            }
            if matches!(
                stage,
                "identity"
                    | "credentials-directory"
                    | "credentials-malformed"
                    | "missing-credentials"
            ) {
                fs::rename(&connection, connection.with_extension("saved")).unwrap();
            }
            let before = lab.state();
            let prepare_identity = "cannot prepare the local RC machine identity";
            let read_connection = "cannot read the local RC connection";
            let operations = match stage {
                "identity" => vec![
                    (vec!["rc", "pair"], prepare_identity),
                    (vec!["rc", "start", "--detach"], prepare_identity),
                    (
                        vec!["rc", "start", "--detach", "--name", "changed-name"],
                        "cannot save the local RC machine name",
                    ),
                ],
                "paired-identity" => vec![(vec!["rc", "start", "--detach"], prepare_identity)],
                "connection" => vec![
                    (vec!["rc", "start", "--detach"], read_connection),
                    (vec!["rc", "pair"], "cannot save the local RC connection"),
                ],
                "malformed-connection" => vec![(vec!["rc", "start", "--detach"], read_connection)],
                _ => {
                    let diagnostic = if stage == "missing-credentials" {
                        "pairing a machine needs an account"
                    } else {
                        "cannot read the saved Hub credentials"
                    };
                    vec![
                        (vec!["rc", "pair"], diagnostic),
                        (vec!["rc", "start", "--detach"], diagnostic),
                    ]
                }
            };
            for (operation, diagnostic) in operations {
                let output = lab.run(&hub.base, &flags, &operation, None);
                let code = if stage == "missing-credentials" { 5 } else { 4 };
                let value = assert_command_output(&output, &flags, "rc", code);
                let text = if let Some(value) = value {
                    if flags.last() == Some(&"2") {
                        assert_eq!(value["fix"], json!([]));
                    }
                    value["diagnostics"]["stderr"].to_string()
                } else {
                    assert!(output.stdout.is_empty(), "{output:?}");
                    String::from_utf8(output.stderr).unwrap()
                };
                assert!(text.contains(diagnostic), "{stage}: {text}");
                assert_eq!(
                    lab.state(),
                    before,
                    "{stage}: refusal changed local evidence"
                );
                assert!(!lab.store.join("rc/agitd.pid").exists());
            }
            if stage == "identity" {
                for operation in [
                    vec!["rc", "pair"],
                    vec!["rc", "start", "--detach", "--name", "changed-name"],
                ] {
                    let output = lab.run("invalid", &flags, &operation, None);
                    assert_command_output(&output, &flags, "rc", 2);
                    assert_eq!(lab.state(), before);
                    assert!(!lab.store.join("rc/agitd.pid").exists());
                }
            }
            let requests = hub.finish();
            assert_eq!(requests.len(), if stage == "connection" { 2 } else { 1 });
            for request in requests {
                assert_eq!(request.method, "POST");
                assert_eq!(request.target, "/api/rc/connections");
                assert_eq!(request.authorization, format!("Bearer {OLD_ACCESS}"));
                assert_eq!(
                    serde_json::from_slice::<Value>(&request.body).unwrap(),
                    json!({
                        "machine_fingerprint":machine["machine_fingerprint"],
                        "display_name":machine["display_name"],
                        "platform":agit::rc::platform(),
                        "agit_version":env!("CARGO_PKG_VERSION")
                    })
                );
            }
        }
    }
}

fn run_bounded(mut command: Command) -> Output {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!(
                "command deadline elapsed: {:?}",
                child.wait_with_output().unwrap()
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
}
