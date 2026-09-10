//! Sharing and search preserve terminal categories without changing identity or replaying requests.

use agit::domain::{meta, storage, transcript};
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

const ACCESS: &str = "SYNTHETIC-category-access";
const REFRESH: &str = "SYNTHETIC-category-refresh";
const CONTENT: &str = "SYNTHETIC-SAVED-SHARE-CONTENT";
const BUILTIN_SECRET: &str = "AKIA4X7QZ2M5RT6VW3JH";
const REGISTERED_SECRET: &str = "SYNTHETIC-registered-share-secret-alpha";
const REGISTERED_LABEL: &str = "SYNTHETIC-private-share-label";

#[derive(Clone, Copy, Debug)]
enum Reply {
    Status(u16),
    TruncatedHeaders,
    EmptyList,
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
    fn new(reply: Reply) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(180);
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
                requests.push(read_request(&mut stream));
                assert!(requests.len() <= 16, "unexpected request replay");
                match reply {
                    Reply::Status(status) => {
                        let body = json!({
                            "error":"HTTP 401: agit login (synthetic status body)",
                            "kind":"unauthorized",
                            "fix":[{"kind":"authenticate"}]
                        })
                        .to_string();
                        write!(stream, "HTTP/1.1 {status} Synthetic\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                    }
                    Reply::TruncatedHeaders => {
                        // An incomplete response cannot confirm either a read or a write.
                        stream
                            .write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Len",
                            )
                            .unwrap();
                    }
                    Reply::EmptyList => {
                        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n[]").unwrap();
                    }
                }
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
        for directory in [&home, &work, &root_path.join("git-template")] {
            fs::create_dir_all(directory).unwrap();
        }
        let lab = Self {
            root,
            home,
            store,
            work,
        };
        save_at(
            &lab.credential_path(base),
            &HubCredential {
                username: "me".into(),
                email: None,
                hub: Some(base.into()),
                access_token: ACCESS.into(),
                refresh_token: REFRESH.into(),
                access_expires_at: "2099-01-01T00:00:00Z".into(),
                refresh_expires_at: "2000-01-01T00:00:00Z".into(),
            },
        )
        .unwrap();
        success(lab.run(base, &["init", "qa", "--no-bind"]));
        lab.git(&["checkout", "-b", "chosen"]);
        lab.write_saved(CONTENT);
        // Exercise startup and lazy scan storage before taking refusal snapshots.
        let warm = lab.run(
            base,
            &["--yes", "share", "me/qa@chosen", "--expire", "invalid"],
        );
        assert_eq!(warm.status.code(), Some(2), "{warm:?}");
        assert!(
            String::from_utf8_lossy(&warm.stderr).contains("duration"),
            "{warm:?}"
        );
        lab
    }

    fn process(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("AGIT_HOME", &self.store)
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(windows) { "os" } else { "file" },
            )
            .env("GIT_CONFIG_GLOBAL", self.home.join("empty-gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_TEMPLATE_DIR",
                self.home.parent().unwrap().join("git-template"),
            )
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_AUTHOR_NAME", "Category fixture")
            .env("GIT_AUTHOR_EMAIL", "category@example.test")
            .env("GIT_COMMITTER_NAME", "Category fixture")
            .env("GIT_COMMITTER_EMAIL", "category@example.test")
            .env("CI", "1")
            .env("NO_COLOR", "1")
            .current_dir(&self.work)
            .stdin(Stdio::null());
        #[cfg(windows)]
        for name in ["SystemRoot", "WINDIR", "TEMP", "TMP", "ComSpec"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command
    }

    fn command(&self, base: &str, args: &[&str]) -> Command {
        let mut command = self.process(env!("CARGO_BIN_EXE_agit"));
        command.env("AGIT_HUB_URL", base).args(args);
        command
    }

    fn run(&self, base: &str, args: &[&str]) -> Output {
        run_bounded(self.command(base, args))
    }

    fn register_secret(&self, base: &str) {
        let input = self.work.join("synthetic-secret.txt");
        fs::write(&input, REGISTERED_SECRET).unwrap();
        let mut command = self.command(base, &["secrets", "add", REGISTERED_LABEL, "--stdin"]);
        command.stdin(fs::File::open(input).unwrap());
        let output = success(run_bounded(command));
        assert!(!String::from_utf8_lossy(&output.stdout).contains(REGISTERED_SECRET));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(REGISTERED_SECRET));
    }

    fn repo(&self) -> PathBuf {
        self.store.join("repos/me/qa")
    }

    fn git(&self, args: &[&str]) -> Vec<u8> {
        let mut command = self.process("git");
        command.arg("-C").arg(self.repo()).args(args);
        success(run_bounded(command)).stdout
    }

    fn write_saved(&self, content: &str) {
        let raw = format!(
            "{}\n",
            json!({"type":"user","sessionId":"category-native","message":{"role":"user","content":content}})
        );
        let native = self
            .home
            .join(".claude/projects/fixture/category-native.jsonl");
        fs::create_dir_all(native.parent().unwrap()).unwrap();
        fs::write(native, &raw).unwrap();
        let session = format!("agit-{}", "b".repeat(40));
        let envelope = transcript::wrap_lines(&raw, "claude-code", &session);
        storage::write_snapshot(&self.repo(), &envelope, &envelope).unwrap();
        let mut snapshot = meta::Meta::new(
            session,
            "claude-code".into(),
            self.work.to_string_lossy().into(),
        );
        snapshot.turn = Some(1);
        meta::write(&self.repo(), &snapshot).unwrap();
        self.git(&["add", "."]);
        self.git(&["commit", "-m", "Seed selected saved session"]);
    }

    fn credential_path(&self, base: &str) -> PathBuf {
        self.store.join("credentials").join(format!(
            "{}.json",
            agit::infra::config::hub_host_key(base).unwrap()
        ))
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

fn success(output: Output) -> Output {
    assert!(output.status.success(), "{output:?}");
    output
}

fn modes() -> [(&'static str, Vec<&'static str>); 4] {
    [
        ("human", vec![]),
        ("quiet", vec!["--quiet"]),
        ("json1", vec!["--json", "--json-version", "1"]),
        ("json2", vec!["--json", "--json-version", "2"]),
    ]
}

fn assert_failure(output: &Output, mode: &str, code: i32, lab: &Lab, base: &str, login: bool) {
    assert_eq!(output.status.code(), Some(code), "{mode}: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    for secret in [ACCESS, REFRESH, CONTENT] {
        assert!(
            !stdout.contains(secret) && !stderr.contains(secret),
            "private content in output"
        );
    }
    assert!(!stdout.contains("share created") && !stdout.contains("revoked "));
    if let Some(version) = mode.strip_prefix("json") {
        assert!(stderr.is_empty(), "{output:?}");
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["schema"], "cli-output");
        assert_eq!(value["schema_version"], version.parse::<u32>().unwrap());
        assert_eq!(value["exit_code"], code);
        assert_eq!(value["ok"], false);
        assert_eq!(value["result"]["format"], "empty");
        assert!(
            !value["diagnostics"]["stderr"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        if version == "1" {
            assert!(value.get("fix").is_none());
        } else if login {
            let cwd = lab.work.clone();
            // Child working directories drop verbatim prefixes; injected environment paths do not.
            #[cfg(windows)]
            let cwd = {
                let path = cwd.to_str().unwrap();
                if let Some(share) = path.strip_prefix(r"\\?\UNC\") {
                    PathBuf::from(format!(r"\\{share}"))
                } else {
                    PathBuf::from(path.strip_prefix(r"\\?\").unwrap_or(path))
                }
            };
            assert_eq!(
                value["fix"],
                json!([{
                    "kind":"agit_command",
                    "argv":["agit", "login", "--hub", base],
                    "cwd":cwd,
                    "env":{"AGIT_HOME":lab.store,"AGIT_HUB_URL":base},
                    "requires_interaction":true
                }])
            );
        } else {
            assert_eq!(value["fix"], json!([]));
        }
    } else {
        assert!(!stderr.is_empty(), "{output:?}");
        if mode == "quiet" {
            assert!(stdout.is_empty(), "{output:?}");
        }
    }
    let login_hint = if cfg!(windows) {
        "log in from PowerShell with"
    } else {
        "log in with `agit login --hub"
    };
    assert_eq!(format!("{stdout}{stderr}").contains(login_hint), login);
}

fn network_matrix(cases: &[(&[&str], &str, &str)]) {
    for &(args, method, target) in cases {
        for reply in [
            Reply::Status(401),
            Reply::Status(503),
            Reply::TruncatedHeaders,
        ] {
            let hub = Hub::new(reply);
            let lab = Lab::new(&hub.base);
            let credential = fs::read(lab.credential_path(&hub.base)).unwrap();
            let before = lab.state();
            let login = matches!(reply, Reply::Status(401));
            for (mode, mut flags) in modes() {
                flags.extend_from_slice(args);
                let output = lab.run(&hub.base, &flags);
                assert_failure(
                    &output,
                    mode,
                    if login { 5 } else { 6 },
                    &lab,
                    &hub.base,
                    login,
                );
                if matches!(reply, Reply::Status(_)) {
                    assert!(
                        format!(
                            "{}{}",
                            String::from_utf8_lossy(&output.stdout),
                            String::from_utf8_lossy(&output.stderr)
                        )
                        .contains("HTTP 401: agit login")
                    );
                }
                assert_eq!(
                    fs::read(lab.credential_path(&hub.base)).unwrap(),
                    credential
                );
                assert_eq!(lab.state(), before, "{args:?} {reply:?} {mode}");
            }
            let requests = hub.finish();
            assert_eq!(requests.len(), 4, "{requests:?}");
            for request in requests {
                assert_eq!(request.method, method);
                assert_eq!(request.target, target);
                assert_eq!(request.authorization, format!("Bearer {ACCESS}"));
                if method == "POST" {
                    let body: Value = serde_json::from_slice(&request.body).unwrap();
                    assert!(body["payload"].as_str().unwrap().contains(CONTENT));
                    assert_eq!(body["encrypted"], false);
                    assert!(body["password_hash"].is_null());
                    assert!(!String::from_utf8_lossy(&request.body).contains(ACCESS));
                } else {
                    assert!(request.body.is_empty());
                }
            }
        }
    }
}

#[test]
fn share_http_and_transport_failures_preserve_auth_without_replaying_writes() {
    network_matrix(&[
        (&["share", "list"], "GET", "/api/shares"),
        (
            &["share", "rm", "selected-link"],
            "DELETE",
            "/api/shares/selected-link",
        ),
        (
            &["--yes", "share", "me/qa@chosen", "--public"],
            "POST",
            "/api/shares",
        ),
    ]);
}

#[test]
fn every_search_route_uses_network_failure_in_all_output_modes() {
    network_matrix(&[
        (
            &["search", "needle"],
            "GET",
            "/api/search/sessions?q=needle&per=10",
        ),
        (
            &["search", "needle", "--type", "agents"],
            "GET",
            "/api/search/agents?q=needle&per=10",
        ),
        (
            &["search", "needle", "--type", "prs"],
            "GET",
            "/api/search/prs?q=needle&per=10",
        ),
        (
            &["search", "needle", "--type", "people"],
            "GET",
            "/api/search/people?q=needle&per=10",
        ),
        (
            &["search", "needle", "--counts"],
            "GET",
            "/api/search/counts?q=needle",
        ),
    ]);
}

#[test]
fn local_selection_and_syntax_failures_do_not_become_network_errors() {
    let hub = Hub::new(Reply::EmptyList);
    let lab = Lab::new(&hub.base);
    let before = lab.state();
    for (mode, flags) in modes() {
        for (args, code) in [
            (vec!["search", " "], 2),
            (vec!["search", "needle", "--type", "invalid"], 2),
            (vec!["share", "me/qa@absent"], 4),
            (vec!["share", "me/qa@chosen", "--expire", "invalid"], 2),
        ] {
            let mut argv = flags.clone();
            argv.extend(args);
            assert_failure(
                &lab.run(&hub.base, &argv),
                mode,
                code,
                &lab,
                &hub.base,
                false,
            );
            assert_eq!(lab.state(), before);
        }
    }
    assert!(hub.finish().is_empty());
}

#[test]
fn an_empty_share_list_is_still_successful_and_keeps_credentials() {
    let hub = Hub::new(Reply::EmptyList);
    let lab = Lab::new(&hub.base);
    let before = lab.state();
    for (mode, mut flags) in modes() {
        flags.extend(["share", "list"]);
        let output = success(lab.run(&hub.base, &flags));
        if mode.starts_with("json") {
            let value: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(value["exit_code"], 0);
            assert_eq!(value["ok"], true);
        } else {
            assert!(String::from_utf8_lossy(&output.stdout).contains("no active shares"));
        }
        assert_eq!(lab.state(), before);
    }
    assert_eq!(hub.finish().len(), 4);
}

#[test]
fn secret_shares_refuse_with_policy_in_all_modes_without_contacting_the_hub() {
    for secret in [BUILTIN_SECRET, REGISTERED_SECRET] {
        let hub = Hub::new(Reply::EmptyList);
        let lab = Lab::new(&hub.base);
        lab.write_saved(secret);
        if secret == REGISTERED_SECRET {
            lab.register_secret(&hub.base);
        }
        let before = lab.state();
        for (mode, mut flags) in modes() {
            flags.extend(["--yes", "share", "me/qa@chosen", "--public"]);
            let mut command = lab.command(&hub.base, &flags);
            command.env("AGIT_ALLOW_SECRETS", "1");
            let output = run_bounded(command);
            assert_eq!(output.status.code(), Some(7), "{mode}: {output:?}");
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let text = format!("{stdout}{stderr}");
            for private in [secret, REGISTERED_LABEL, ACCESS, REFRESH] {
                assert!(
                    !text.contains(private),
                    "share refusal leaked private content"
                );
            }
            assert!(text.contains("refusing to share"), "{output:?}");
            assert!(!text.contains("share created"), "{output:?}");
            assert!(!text.contains("log in with `agit login --hub"));
            assert!(!text.contains("log in from PowerShell with"));
            if secret == REGISTERED_SECRET {
                assert!(text.contains("[redacted:registered-secret]"), "{output:?}");
            } else {
                assert!(text.contains("aws-access-token"), "{output:?}");
            }
            if let Some(version) = mode.strip_prefix("json") {
                assert!(stderr.is_empty(), "{output:?}");
                let value: Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(value["schema"], "cli-output");
                assert_eq!(value["schema_version"], version.parse::<u32>().unwrap());
                assert_eq!(value["exit_code"], 7);
                assert_eq!(value["ok"], false);
                assert_eq!(value["result"]["format"], "text");
                if version == "1" {
                    assert!(value.get("fix").is_none());
                } else {
                    assert_eq!(value["fix"], json!([]));
                }
            } else {
                assert!(stderr.contains("refusing to share"), "{output:?}");
            }
            assert_eq!(lab.state(), before, "{mode}: refusal changed local data");
        }
        assert!(hub.finish().is_empty());
    }
}

#[test]
fn a_share_passphrase_needs_a_terminal_even_with_explicit_confirmation() {
    let hub = Hub::new(Reply::EmptyList);
    let lab = Lab::new(&hub.base);
    let before = lab.state();
    for (mode, mut flags) in modes() {
        flags.extend(["--yes", "share", "me/qa@chosen", "--public", "--password"]);
        let output = lab.run(&hub.base, &flags);
        assert_failure(&output, mode, 8, &lab, &hub.base, false);
        assert!(
            format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
            .contains("--password needs an interactive terminal")
        );
        assert_eq!(lab.state(), before);
    }
    assert!(hub.finish().is_empty());
}

#[test]
fn missing_authentication_precedes_share_policy_and_interaction_refusal() {
    let hub = Hub::new(Reply::EmptyList);
    let lab = Lab::new(&hub.base);
    lab.write_saved(BUILTIN_SECRET);
    fs::remove_file(lab.credential_path(&hub.base)).unwrap();
    let before = lab.state();
    for (mode, flags) in modes() {
        for password in [false, true] {
            let mut args = flags.clone();
            args.extend(["--yes", "share", "me/qa@chosen", "--public"]);
            if password {
                args.push("--password");
            }
            let output = lab.run(&hub.base, &args);
            assert_failure(&output, mode, 5, &lab, &hub.base, true);
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(!text.contains(BUILTIN_SECRET));
            assert!(!text.contains("refusing to share"));
            assert!(!text.contains("--password needs an interactive terminal"));
            assert_eq!(lab.state(), before);
        }
    }
    assert!(hub.finish().is_empty());
}
